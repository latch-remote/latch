//! libva itself: loaded with `libloading` when first asked for, a display
//! on each DRM render node, and the [`Backend`] the bitstream side drives.

use super::va::{self, ConfigAttrib, Id, Image, ImageFormat, INVALID_ID};
use super::{
    copy_planes, decoder_for, limits, probe_picture_ok, Backend, Bitstream, Capability, Decoded,
    Format, Limit, Profile, Report,
};
use crate::video::{Decoder, Frame};
use anyhow::{ensure, Context, Result};
use latch_proto::control::Codec;
use libloading::Library;
use std::ffi::{c_char, c_int, c_uint, c_void, CStr};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

type Display = *mut c_void;
type Status = c_int;
const SUCCESS: Status = 0;
const PROGRESSIVE: c_int = 1;

macro_rules! entry_points {
    ($($name:ident: fn($($arg:ty),*) $(-> $ret:ty)?;)*) => {
        #[allow(non_snake_case)]
        struct Api {
            _va: Library,
            _drm: Library,
            get_display_drm: unsafe extern "C" fn(c_int) -> Display,
            export: Option<unsafe extern "C" fn(Display, Id, u32, u32, *mut super::dmabuf::Descriptor) -> Status>,
            $($name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*
        }
        fn load() -> Result<Api> {
            // SAFETY: libva's initializers have no preconditions; symbol
            // types are those of va.h and va_drm.h.
            unsafe {
                let va = Library::new("libva.so.2").context("no libva.so.2")?;
                let drm = Library::new("libva-drm.so.2").context("no libva-drm.so.2")?;
                Ok(Api {
                    get_display_drm: *drm.get(b"vaGetDisplayDRM\0")?,
                    export: va.get(b"vaExportSurfaceHandle\0").ok().map(|s| *s),
                    $($name: *va.get(concat!(stringify!($name), "\0").as_bytes())?,)*
                    _va: va,
                    _drm: drm,
                })
            }
        }
    };
}

entry_points! {
    vaInitialize: fn(Display, *mut c_int, *mut c_int) -> Status;
    vaTerminate: fn(Display) -> Status;
    vaErrorStr: fn(Status) -> *const c_char;
    vaQueryVendorString: fn(Display) -> *const c_char;
    vaMaxNumProfiles: fn(Display) -> c_int;
    vaMaxNumEntrypoints: fn(Display) -> c_int;
    vaQueryConfigProfiles: fn(Display, *mut c_int, *mut c_int) -> Status;
    vaQueryConfigEntrypoints: fn(Display, c_int, *mut c_int, *mut c_int) -> Status;
    vaGetConfigAttributes: fn(Display, c_int, c_int, *mut ConfigAttrib, c_int) -> Status;
    vaCreateConfig: fn(Display, c_int, c_int, *mut ConfigAttrib, c_int, *mut Id) -> Status;
    vaDestroyConfig: fn(Display, Id) -> Status;
    vaQuerySurfaceAttributes: fn(Display, Id, *mut va::SurfaceAttrib, *mut c_uint) -> Status;
    vaCreateSurfaces: fn(Display, c_uint, c_uint, c_uint, *mut Id, c_uint, *mut c_void, c_uint) -> Status;
    vaDestroySurfaces: fn(Display, *mut Id, c_int) -> Status;
    vaCreateContext: fn(Display, Id, c_int, c_int, c_int, *mut Id, c_int, *mut Id) -> Status;
    vaDestroyContext: fn(Display, Id) -> Status;
    vaCreateBuffer: fn(Display, Id, c_int, c_uint, c_uint, *mut c_void, *mut Id) -> Status;
    vaDestroyBuffer: fn(Display, Id) -> Status;
    vaMapBuffer: fn(Display, Id, *mut *mut c_void) -> Status;
    vaUnmapBuffer: fn(Display, Id) -> Status;
    vaBeginPicture: fn(Display, Id, Id) -> Status;
    vaRenderPicture: fn(Display, Id, *mut Id, c_int) -> Status;
    vaEndPicture: fn(Display, Id) -> Status;
    vaSyncSurface: fn(Display, Id) -> Status;
    vaCreateImage: fn(Display, *mut ImageFormat, c_int, c_int, *mut Image) -> Status;
    vaGetImage: fn(Display, Id, c_int, c_int, c_uint, c_uint, Id) -> Status;
    vaDeriveImage: fn(Display, Id, *mut Image) -> Status;
    vaDestroyImage: fn(Display, Id) -> Status;
    vaSetErrorCallback: fn(Display, Option<MessageCallback>, *mut c_void) -> Option<MessageCallback>;
    vaSetInfoCallback: fn(Display, Option<MessageCallback>, *mut c_void) -> Option<MessageCallback>;
}

type MessageCallback = unsafe extern "C" fn(*mut c_void, *const c_char);

/// libva's messages go to the log at debug level, not to stderr.
unsafe extern "C" fn message(_: *mut c_void, text: *const c_char) {
    if !text.is_null() {
        // SAFETY: libva passes a NUL-terminated message.
        let text = unsafe { CStr::from_ptr(text) };
        tracing::debug!("libva: {}", text.to_string_lossy().trim_end());
    }
}

fn api() -> Result<&'static Api> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| load().map_err(|e| format!("{e:#}")))
        .as_ref()
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// An initialized VA display on a DRM render node.
struct Device {
    api: &'static Api,
    display: Display,
    _fd: OwnedFd,
    vendor: String,
}

// SAFETY: a VA display may be used from any thread, one call at a time;
// each decoder owns its own and is used by one thread at a time.
unsafe impl Send for Device {}

impl Device {
    fn check(&self, status: Status, what: &str) -> Result<()> {
        if status == SUCCESS {
            return Ok(());
        }
        // SAFETY: vaErrorStr returns a static string for any status.
        let text = unsafe { CStr::from_ptr((self.api.vaErrorStr)(status)) };
        anyhow::bail!("VA-API {what}: {} ({status})", text.to_string_lossy())
    }

    /// The render nodes in order, `$LATCH_VAAPI_DEVICE` first if set.
    fn nodes() -> Vec<PathBuf> {
        let mut nodes: Vec<PathBuf> = std::env::var_os("LATCH_VAAPI_DEVICE")
            .map(Into::into)
            .into_iter()
            .collect();
        let mut discovered: Vec<_> = std::fs::read_dir("/dev/dri")
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry.file_name().to_str().is_some_and(|s| {
                    s.strip_prefix("renderD")
                        .is_some_and(|n| n.parse::<u32>().is_ok())
                })
            })
            .map(|entry| entry.path())
            .collect();
        discovered.sort();
        for node in discovered {
            if !nodes.contains(&node) {
                nodes.push(node);
            }
        }
        nodes
    }

    /// The driver on `node`, initialized; `None` when there is no node.
    fn open(node: &Path) -> Result<Option<Self>> {
        let api = api()?;
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(node)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => anyhow::bail!("{}: {e}", node.display()),
        };
        let fd = OwnedFd::from(file);
        // SAFETY: `fd` stays open for as long as the display lives.
        let display = unsafe { (api.get_display_drm)(fd.as_raw_fd()) };
        if display.is_null() {
            return Ok(None);
        }
        let (mut major, mut minor) = (0, 0);
        // SAFETY: a display just returned by vaGetDisplayDRM.
        let status = unsafe {
            (api.vaSetErrorCallback)(display, Some(message), null_mut());
            (api.vaSetInfoCallback)(display, Some(message), null_mut());
            (api.vaInitialize)(display, &mut major, &mut minor)
        };
        if status != SUCCESS {
            // SAFETY: as above; terminating also frees the display.
            unsafe { (api.vaTerminate)(display) };
            anyhow::bail!("{}: no VA-API driver (status {status})", node.display());
        }
        // SAFETY: initialized; the vendor string lives with the display.
        let vendor = unsafe {
            let s = (api.vaQueryVendorString)(display);
            if s.is_null() {
                String::new()
            } else {
                CStr::from_ptr(s).to_string_lossy().into_owned()
            }
        };
        Ok(Some(Self {
            api,
            display,
            _fd: fd,
            vendor,
        }))
    }

    /// What the driver reports of each profile Latch decodes.
    fn reports(&self) -> Vec<Report> {
        let api = self.api;
        // SAFETY: lists sized to the driver's maxima; out-parameters only.
        unsafe {
            let max = (api.vaMaxNumProfiles)(self.display).max(1) as usize;
            let mut profiles = vec![0; max];
            let mut count = 0;
            if (api.vaQueryConfigProfiles)(self.display, profiles.as_mut_ptr(), &mut count)
                != SUCCESS
            {
                return Vec::new();
            }
            profiles.truncate(count.clamp(0, max as c_int) as usize);
            let max_entrypoints = (api.vaMaxNumEntrypoints)(self.display).max(1) as usize;
            Profile::ALL
                .iter()
                .map(|p| p.va())
                .filter(|p| profiles.contains(p))
                .map(|profile| {
                    let mut entrypoints = vec![0; max_entrypoints];
                    let mut n = 0;
                    let vld = (api.vaQueryConfigEntrypoints)(
                        self.display,
                        profile,
                        entrypoints.as_mut_ptr(),
                        &mut n,
                    ) == SUCCESS
                        && entrypoints[..n.clamp(0, max_entrypoints as c_int) as usize]
                            .contains(&va::ENTRYPOINT_VLD);
                    let mut attribs = [
                        ConfigAttrib {
                            kind: va::ATTRIB_RT_FORMAT,
                            value: 0,
                        },
                        ConfigAttrib {
                            kind: va::ATTRIB_MAX_PICTURE_WIDTH,
                            value: 0,
                        },
                        ConfigAttrib {
                            kind: va::ATTRIB_MAX_PICTURE_HEIGHT,
                            value: 0,
                        },
                    ];
                    let got = vld
                        && (api.vaGetConfigAttributes)(
                            self.display,
                            profile,
                            va::ENTRYPOINT_VLD,
                            attribs.as_mut_ptr(),
                            attribs.len() as c_int,
                        ) == SUCCESS;
                    let said = |a: &ConfigAttrib| {
                        (got && a.value != va::ATTRIB_NOT_SUPPORTED && a.value != 0)
                            .then_some(a.value)
                    };
                    let dimensions = self.surface_limits(profile, said(&attribs[0]));
                    Report {
                        profile,
                        vld,
                        rt_formats: (got && attribs[0].value != va::ATTRIB_NOT_SUPPORTED)
                            .then_some(attribs[0].value),
                        max_width: dimensions.0.or_else(|| said(&attribs[1])),
                        max_height: dimensions.1.or_else(|| said(&attribs[2])),
                    }
                })
                .collect()
        }
    }
    fn surface_limits(&self, profile: i32, formats: Option<u32>) -> (Option<u32>, Option<u32>) {
        let (api, dpy) = (self.api, self.display);
        let ten_bit = profile == va::PROFILE_HEVC_MAIN10;
        let rt = if ten_bit {
            va::RT_FORMAT_YUV420_10
        } else {
            va::RT_FORMAT_YUV420
        };
        if formats.is_some_and(|f| f & rt == 0) {
            return (None, None);
        }
        let mut attrib = ConfigAttrib {
            kind: va::ATTRIB_RT_FORMAT,
            value: rt,
        };
        let mut config = INVALID_ID;
        // SAFETY: temporary config, and driver-sized out-array. The integer
        // VAGenericValue member is read only when its type tag is integer.
        unsafe {
            if (api.vaCreateConfig)(
                dpy,
                profile,
                va::ENTRYPOINT_VLD,
                &mut attrib,
                1,
                &mut config,
            ) != SUCCESS
            {
                return (None, None);
            }
            let mut count = 0;
            let mut dimensions = (None, None);
            if (api.vaQuerySurfaceAttributes)(dpy, config, null_mut(), &mut count) == SUCCESS
                && count <= 1024
            {
                let mut attrs = vec![va::SurfaceAttrib::default(); count as usize];
                if (api.vaQuerySurfaceAttributes)(dpy, config, attrs.as_mut_ptr(), &mut count)
                    == SUCCESS
                {
                    for a in attrs
                        .iter()
                        .take(count as usize)
                        .filter(|a| a.value_type == 1)
                    {
                        let value = a.value.integer as u32;
                        if value > 0 && value <= i32::MAX as u32 {
                            match a.kind {
                                3 => dimensions.0 = Some(value), // VASurfaceAttribMaxWidth
                                5 => dimensions.1 = Some(value), // VASurfaceAttribMaxHeight
                                _ => {}
                            }
                        }
                    }
                }
            }
            (api.vaDestroyConfig)(dpy, config);
            dimensions
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: initialized in `open`; nothing uses it afterwards.
        unsafe { (self.api.vaTerminate)(self.display) };
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Output {
    Readback,
    Presentation,
}

impl Output {
    fn exports(self, renderer_enabled: bool) -> bool {
        self == Self::Presentation && renderer_enabled
    }
}

/// A configured decoder: config, surfaces, context and readback image.
struct Driver {
    device: Device,
    output: Output,
    limits: Vec<Limit>,
    config: Id,
    context: Id,
    surfaces: Vec<Id>,
    leases: Vec<std::sync::Arc<()>>,
    format: Option<Format>,
    image: Option<Image>,
    /// `vaGetImage` failed once; map derived images instead.
    derive: bool,
}

impl Driver {
    fn release(&mut self) {
        let (api, dpy) = (self.device.api, self.device.display);
        // SAFETY: each id is this driver's and destroyed once.
        unsafe {
            if let Some(image) = self.image.take() {
                (api.vaDestroyImage)(dpy, image.image_id);
            }
            if self.context != INVALID_ID {
                (api.vaDestroyContext)(dpy, self.context);
                self.context = INVALID_ID;
            }
            if !self.surfaces.is_empty() {
                (api.vaDestroySurfaces)(
                    dpy,
                    self.surfaces.as_mut_ptr(),
                    self.surfaces.len() as c_int,
                );
                self.surfaces.clear();
                self.leases.clear();
            }
            if self.config != INVALID_ID {
                (api.vaDestroyConfig)(dpy, self.config);
                self.config = INVALID_ID;
            }
        }
        self.format = None;
    }

    fn buffer(&self, kind: c_int, bytes: va::BufferData<'_>) -> Result<Id> {
        let mut id = INVALID_ID;
        // SAFETY: libva copies `bytes.len()` bytes; the pointer is only read.
        let status = unsafe {
            (self.device.api.vaCreateBuffer)(
                self.device.display,
                self.context,
                kind,
                bytes.len as c_uint,
                1,
                bytes.ptr as *mut c_void,
                &mut id,
            )
        };
        self.device.check(status, "buffer")?;
        Ok(id)
    }

    /// One picture: its buffers, between Begin and End.
    fn render(&mut self, target: Id, parts: &[(c_int, va::BufferData<'_>)]) -> Result<()> {
        ensure!(
            self.context != INVALID_ID,
            "VA-API picture before a context"
        );
        let (api, dpy) = (self.device.api, self.device.display);
        let mut buffers = Vec::with_capacity(parts.len());
        let result = (|| {
            for &(kind, bytes) in parts {
                buffers.push(self.buffer(kind, bytes)?);
            }
            // SAFETY: the context, target and buffers are this driver's.
            unsafe {
                self.device.check(
                    (api.vaBeginPicture)(dpy, self.context, target),
                    "begin picture",
                )?;
                let rendered = (api.vaRenderPicture)(
                    dpy,
                    self.context,
                    buffers.as_mut_ptr(),
                    buffers.len() as c_int,
                );
                // A begun picture is always ended.
                let ended = (api.vaEndPicture)(dpy, self.context);
                self.device.check(rendered, "render picture")?;
                self.device.check(ended, "end picture")
            }
        })();
        for id in buffers {
            // SAFETY: created above; libva 2 leaves them to the caller.
            unsafe { (api.vaDestroyBuffer)(dpy, id) };
        }
        result
    }

    /// The decoded picture, copied out into `spare`'s buffers.
    fn read(&mut self, d: Decoded, spare: Option<Frame>) -> Result<Frame> {
        let (api, dpy) = (self.device.api, self.device.display);
        let format = self
            .format
            .clone()
            .context("VA-API readback without a format")?;
        if self
            .output
            .exports(super::dmabuf::ENABLED.load(std::sync::atomic::Ordering::Acquire))
            && d.visible.x == 0
            && d.visible.y == 0
            && d.visible.width == format.width
            && d.visible.height == format.height
        {
            match self.export(d) {
                Ok(native) => {
                    return Ok(Frame {
                        width: d.visible.width,
                        height: d.visible.height,
                        format: if d.ten_bit {
                            super::PixelFormat::P010
                        } else {
                            super::PixelFormat::Nv12
                        },
                        color: latch_proto::color::ColorDescription {
                            bit_depth: if d.ten_bit { 10 } else { 8 },
                            ..Default::default()
                        },
                        native: Some(native),
                        ..Default::default()
                    })
                }
                Err(e) => {
                    tracing::warn!("VA-API DMA-BUF export failed ({e:#}); using readback");
                    super::dmabuf::render_on_dmabuf(false);
                }
            }
        }
        let fourcc = if d.ten_bit {
            va::FOURCC_P010
        } else {
            va::FOURCC_NV12
        };
        // SAFETY: the surface is this driver's; the image's mapping covers
        // `data_size` bytes, which `copy_planes` checks the planes fit.
        unsafe {
            self.device
                .check((api.vaSyncSurface)(dpy, d.surface), "sync surface")?;
            if !self.derive {
                if self.image.is_none() {
                    let mut f = ImageFormat {
                        fourcc,
                        byte_order: 1,
                        bits_per_pixel: if d.ten_bit { 24 } else { 12 },
                        ..Default::default()
                    };
                    let mut image = Image::default();
                    let status = (api.vaCreateImage)(
                        dpy,
                        &mut f,
                        format.width as c_int,
                        format.height as c_int,
                        &mut image,
                    );
                    if status == SUCCESS {
                        self.image = Some(image);
                    }
                }
                let got = self.image.is_some_and(|image| {
                    (api.vaGetImage)(
                        dpy,
                        d.surface,
                        0,
                        0,
                        format.width,
                        format.height,
                        image.image_id,
                    ) == SUCCESS
                });
                if let (true, Some(image)) = (got, self.image) {
                    return self.copy(image, d, spare);
                }
                tracing::debug!("vaGetImage failed; reading derived images instead");
                self.derive = true;
            }
            let mut image = Image::default();
            self.device.check(
                (api.vaDeriveImage)(dpy, d.surface, &mut image),
                "derive image",
            )?;
            let result = if image.format.fourcc == fourcc {
                self.copy(image, d, spare)
            } else {
                Err(anyhow::anyhow!(
                    "VA-API derived image is {:#x}, not {fourcc:#x}",
                    image.format.fourcc
                ))
            };
            (api.vaDestroyImage)(dpy, image.image_id);
            result
        }
    }

    fn export(&self, d: Decoded) -> Result<super::dmabuf::NativeFrame> {
        let export = self
            .device
            .api
            .export
            .context("libva has no surface export")?;
        let mut desc = super::dmabuf::Descriptor::default();
        // SAFETY: decode has submitted this surface. Sync completes its writes
        // before EGL samples it, and the lease excludes further decoder writes.
        unsafe {
            self.device.check(
                (self.device.api.vaSyncSurface)(self.device.display, d.surface),
                "sync export",
            )?;
            self.device.check(
                export(
                    self.device.display,
                    d.surface,
                    0x4000_0000,
                    0x1 | 0x4,
                    &mut desc,
                ),
                "export surface",
            )?;
        }
        let index = self
            .surfaces
            .iter()
            .position(|s| *s == d.surface)
            .context("unknown VA surface")?;
        super::dmabuf::NativeFrame::new(desc, self.leases[index].clone())
    }

    fn copy(&self, image: Image, d: Decoded, spare: Option<Frame>) -> Result<Frame> {
        ensure!(
            image.num_planes == 2,
            "VA-API image is not two-plane NV12/P010"
        );
        let (api, dpy) = (self.device.api, self.device.display);
        let mut data = null_mut();
        // SAFETY: the image's buffer, mapped for reading and unmapped
        // before returning; the slice does not outlive the mapping.
        unsafe {
            self.device
                .check((api.vaMapBuffer)(dpy, image.buf, &mut data), "map image")?;
            if data.is_null() || image.data_size == 0 {
                (api.vaUnmapBuffer)(dpy, image.buf);
                anyhow::bail!("VA-API mapped an empty image");
            }
            let bytes = std::slice::from_raw_parts(data.cast::<u8>(), image.data_size as usize);
            let result = copy_planes(
                bytes,
                [image.offsets[0] as usize, image.offsets[1] as usize],
                [image.pitches[0] as usize, image.pitches[1] as usize],
                d.visible,
                d.ten_bit,
                spare,
            );
            (api.vaUnmapBuffer)(dpy, image.buf);
            result
        }
    }
}

impl Backend for Driver {
    fn available(&self, surface: Id) -> bool {
        self.surfaces
            .iter()
            .position(|s| *s == surface)
            .is_some_and(|i| std::sync::Arc::strong_count(&self.leases[i]) == 1)
    }

    fn configure(&mut self, format: &Format) -> Result<Vec<Id>> {
        self.release();
        let fitting: Vec<_> = self
            .limits
            .iter()
            .copied()
            .filter(|l| l.fits(format.width, format.height))
            .collect();
        let limit = decoder_for(&fitting, format.profile)
            .with_context(|| format!("this GPU does not decode {:?}", format.profile))?;
        ensure!(
            limit.fits(format.width, format.height),
            "this GPU decodes {:?} up to {}×{}, not {}×{}",
            limit.profile,
            limit.max_width,
            limit.max_height,
            format.width,
            format.height
        );
        let (api, dpy) = (self.device.api, self.device.display);
        let rt = format.profile.rt_format();
        // SAFETY: out-parameters sized as libva says; ids are released
        // by `release` (here on failure, or when replaced or dropped).
        let result = unsafe {
            (|| {
                let mut attrib = ConfigAttrib {
                    kind: va::ATTRIB_RT_FORMAT,
                    value: rt,
                };
                self.device.check(
                    (api.vaCreateConfig)(
                        dpy,
                        limit.profile.va(),
                        va::ENTRYPOINT_VLD,
                        &mut attrib,
                        1,
                        &mut self.config,
                    ),
                    "config",
                )?;
                let mut surfaces = vec![INVALID_ID; format.surfaces + 4];
                self.device.check(
                    (api.vaCreateSurfaces)(
                        dpy,
                        rt,
                        format.width,
                        format.height,
                        surfaces.as_mut_ptr(),
                        surfaces.len() as c_uint,
                        null_mut(),
                        0,
                    ),
                    "surfaces",
                )?;
                self.leases = surfaces.iter().map(|_| std::sync::Arc::new(())).collect();
                self.surfaces = surfaces;
                self.device.check(
                    (api.vaCreateContext)(
                        dpy,
                        self.config,
                        format.width as c_int,
                        format.height as c_int,
                        PROGRESSIVE,
                        self.surfaces.as_mut_ptr(),
                        self.surfaces.len() as c_int,
                        &mut self.context,
                    ),
                    "context",
                )
            })()
        };
        if let Err(e) = result {
            self.release();
            return Err(e);
        }
        self.format = Some(format.clone());
        Ok(self.surfaces.clone())
    }

    fn h264(
        &mut self,
        target: Id,
        picture: &va::PictureParameterH264,
        iq: &va::IqMatrixH264,
        slices: &[(va::SliceH264, &[u8])],
    ) -> Result<()> {
        let mut parts = vec![
            (va::PICTURE_PARAMETER_BUFFER, va::bytes(picture)),
            (va::IQ_MATRIX_BUFFER, va::bytes(iq)),
        ];
        for (params, data) in slices {
            parts.push((va::SLICE_PARAMETER_BUFFER, va::bytes(params)));
            parts.push((va::SLICE_DATA_BUFFER, va::slice(data)));
        }
        self.render(target, &parts)
    }

    fn hevc(
        &mut self,
        target: Id,
        picture: &va::PictureParameterHevc,
        iq: Option<&va::IqMatrixHevc>,
        slices: &[(va::SliceHevc, &[u8])],
        subsets: &[u32],
    ) -> Result<()> {
        let mut parts = vec![(va::PICTURE_PARAMETER_BUFFER, va::bytes(picture))];
        if !subsets.is_empty() {
            parts.push((va::SUBSETS_PARAMETER_BUFFER, va::slice(subsets)));
        }
        if let Some(iq) = iq {
            parts.push((va::IQ_MATRIX_BUFFER, va::bytes(iq)));
        }
        for (params, data) in slices {
            parts.push((va::SLICE_PARAMETER_BUFFER, va::bytes(params)));
            parts.push((va::SLICE_DATA_BUFFER, va::slice(data)));
        }
        self.render(target, &parts)
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        self.release();
    }
}

/// A VA-API decoder for one stream.
pub struct Vaapi {
    bitstream: Bitstream,
    driver: Driver,
    name: &'static str,
}

impl Vaapi {
    fn on(device: Device, codec: Codec, output: Output) -> Result<Self> {
        let limits = limits(&device.reports());
        Ok(Self {
            bitstream: Bitstream::new(codec)?,
            name: match codec {
                Codec::Hevc => "VA-API HEVC",
                _ => "VA-API H.264",
            },
            driver: Driver {
                device,
                output,
                limits,
                config: INVALID_ID,
                context: INVALID_ID,
                surfaces: Vec::new(),
                leases: Vec::new(),
                format: None,
                image: None,
                derive: false,
            },
        })
    }

    fn vendor(&self) -> &str {
        &self.driver.device.vendor
    }
}

impl Decoder for Vaapi {
    fn decode(&mut self, annexb: &[u8], _idr: bool, spare: Option<Frame>) -> Result<Option<Frame>> {
        let Some(decoded) = self.bitstream.decode(annexb, &mut self.driver)? else {
            return Ok(None);
        };
        match self.driver.read(decoded, spare) {
            Ok(frame) => Ok(Some(frame)),
            Err(e) => {
                // The references may be fine, but a picture that cannot be
                // read means the next IDR should set the hardware up anew.
                self.bitstream = Bitstream::new(match self.bitstream {
                    Bitstream::Hevc(_) => Codec::Hevc,
                    Bitstream::H264(_) => Codec::H264High,
                })?;
                self.driver.release();
                Err(super::hardware(e))
            }
        }
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

/// A decoder for a negotiated stream, on the node its probe passed on.
fn open_direct(codec: Codec, width: u32, height: u32, ten_bit: bool) -> Result<Vaapi> {
    let caps = capabilities();
    let cap = super::capability(&caps, codec, ten_bit, width, height)?;
    let device =
        Device::open(&cap.node)?.with_context(|| format!("{} is gone", cap.node.display()))?;
    Vaapi::on(device, codec, Output::Presentation)
}

/// Keep proven capabilities while retrying unproven entries after a cooldown.
/// A timed-out probe stays pending so a hung driver owns at most one worker
/// and a late result can still be collected by a later lookup.
#[derive(Default)]
struct CapabilityCache {
    caps: Vec<Capability>,
    pending: Option<std::sync::mpsc::Receiver<Vec<Capability>>>,
    retry_at: Option<Instant>,
}

impl CapabilityCache {
    fn get(
        &mut self,
        now: Instant,
        timeout: Duration,
        start_probe: impl FnOnce(Vec<Capability>) -> Option<std::sync::mpsc::Receiver<Vec<Capability>>>,
    ) -> Vec<Capability> {
        let mut wait = Duration::ZERO;
        if self.pending.is_none() && self.retry_at.is_none_or(|deadline| now >= deadline) {
            self.retry_at = Some(now + Duration::from_secs(30));
            self.pending = start_probe(self.caps.clone());
            // Retry in the background when we already have usable codecs.
            if self.caps.is_empty() {
                wait = timeout;
            }
        }
        if let Some(rx) = &self.pending {
            match rx.recv_timeout(wait) {
                Ok(caps) => {
                    self.caps.extend(caps);
                    self.pending = None;
                    self.retry_at = Some(now + Duration::from_secs(30));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    self.pending = None;
                    self.retry_at = Some(now + Duration::from_secs(30));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if !wait.is_zero() {
                        tracing::warn!("VA-API probe is still pending; decoding in software");
                    }
                }
            }
        }
        self.caps.clone()
    }
}

/// What every render node was seen to decode. Pixel probes use readback,
/// with a deadline for driver calls and a cooldown before retrying failures.
pub fn capabilities() -> Vec<Capability> {
    static CACHE: Mutex<CapabilityCache> = Mutex::new(CapabilityCache {
        caps: Vec::new(),
        pending: None,
        retry_at: None,
    });
    if !super::wanted(std::env::var("LATCH_LINUX_DECODER").ok().as_deref()) {
        return Vec::new();
    }
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(
        Instant::now(),
        Duration::from_secs(10),
        |known| {
            if STALLED.load(std::sync::atomic::Ordering::Acquire) {
                return None;
            }
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("latch-vaapi-probe".into())
                .spawn(move || {
                    let _ = tx.send(probe(&known));
                })
                .ok()?;
            Some(rx)
        },
    )
}

const PROBE_H264: &[u8] = include_bytes!("../../../tests/fixtures/decode-probe.h264");
const PROBE_HEVC: &[u8] = include_bytes!("../../../tests/fixtures/decode-probe.hevc");
const PROBE_HEVC_MAIN10: &[u8] = include_bytes!("../../../tests/fixtures/decode-probe-main10.hevc");

fn probe(known: &[Capability]) -> Vec<Capability> {
    if let Err(e) = api() {
        tracing::info!("VA-API unavailable: {e:#}");
        return Vec::new();
    }
    let (baseline, bw, bh) = super::probe_baseline();
    let cases: [(Codec, bool, &[u8], u32, u32); 4] = [
        (Codec::Hevc, false, PROBE_HEVC, 256, 256),
        (Codec::Hevc, true, PROBE_HEVC_MAIN10, 256, 256),
        (Codec::H264High, false, PROBE_H264, 256, 256),
        (Codec::H264Baseline, false, &baseline, bw, bh),
    ];
    let mut caps: Vec<Capability> = Vec::new();
    for node in Device::nodes() {
        let device = match Device::open(&node) {
            Ok(Some(device)) => device,
            Ok(None) => continue,
            Err(e) => {
                tracing::info!("VA-API: {e:#}");
                continue;
            }
        };
        let limits = limits(&device.reports());
        let vendor = device.vendor.clone();
        drop(device);
        for &(codec, ten_bit, bytes, width, height) in &cases {
            if known
                .iter()
                .any(|cap| cap.node == node && cap.codec == codec && cap.ten_bit == ten_bit)
            {
                continue;
            }
            let Some(limit) =
                Profile::for_codec(codec, ten_bit).and_then(|p| decoder_for(&limits, p))
            else {
                continue;
            };
            let result = (|| {
                let device = Device::open(&node)?.context("the node went away")?;
                // Pixel validation always needs CPU planes, independent of the
                // renderer's process-wide DMA-BUF presentation preference.
                let mut decoder = Vaapi::on(device, codec, Output::Readback)?;
                let frame = decoder.decode(bytes, true, None)?.context("no picture")?;
                ensure!(
                    probe_picture_ok(&frame, width, height),
                    "the picture is not the one encoded"
                );
                Ok::<_, anyhow::Error>(decoder.vendor().to_string())
            })();
            match result {
                Ok(_) => {
                    tracing::info!(
                        ?codec,
                        ten_bit,
                        node = %node.display(),
                        vendor,
                        max_width = limit.max_width,
                        max_height = limit.max_height,
                        "VA-API decodes"
                    );
                    caps.push(Capability {
                        codec,
                        ten_bit,
                        node: node.clone(),
                        limit,
                    });
                }
                Err(e) => tracing::info!(
                    ?codec,
                    ten_bit,
                    node = %node.display(),
                    "VA-API decode unavailable: {e:#}"
                ),
            }
        }
    }
    caps
}

/// A driver call stays on its worker. A hung GPU disables further VA-API
/// opens for this process, bounding abandoned workers and allowing software
/// recovery without waiting for driver-owned resources to be destroyed.
static STALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

type PictureResult = Result<Option<Frame>>;
struct Request {
    data: Vec<u8>,
    idr: bool,
    spare: Option<Frame>,
}

pub struct Worker {
    tx: std::sync::mpsc::SyncSender<Request>,
    rx: std::sync::mpsc::Receiver<PictureResult>,
    name: &'static str,
    active: bool,
}

fn receive<T>(
    rx: &std::sync::mpsc::Receiver<T>,
    timeout: std::time::Duration,
    stalled: &std::sync::atomic::AtomicBool,
) -> Result<T> {
    match rx.recv_timeout(timeout) {
        Ok(value) => Ok(value),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            stalled.store(true, std::sync::atomic::Ordering::Release);
            Err(super::hardware(anyhow::anyhow!("VA-API driver timed out")))
        }
        Err(_) => Err(super::hardware(anyhow::anyhow!("VA-API worker stopped"))),
    }
}

pub fn open(codec: Codec, width: u32, height: u32, ten_bit: bool) -> Result<Worker> {
    ensure!(
        !STALLED.load(std::sync::atomic::Ordering::Acquire),
        "VA-API driver stalled earlier"
    );
    // Probe has its own deadline, before the worker's startup deadline.
    super::capability(&capabilities(), codec, ten_bit, width, height)?;
    let (tx, requests) = std::sync::mpsc::sync_channel::<Request>(1);
    let (results, rx) = std::sync::mpsc::channel();
    let (ready, startup) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("latch-vaapi-decode".into())
        .spawn(move || {
            let mut decoder = match open_direct(codec, width, height, ten_bit) {
                Ok(decoder) => decoder,
                Err(e) => {
                    let _ = ready.send(Err(e));
                    return;
                }
            };
            if ready.send(Ok(decoder.name())).is_err() {
                return;
            }
            while let Ok(request) = requests.recv() {
                let result = decoder.decode(&request.data, request.idr, request.spare);
                if results.send(result).is_err() {
                    break;
                }
            }
        })?;
    let name = receive(&startup, std::time::Duration::from_secs(5), &STALLED)??;
    Ok(Worker {
        tx,
        rx,
        name,
        active: true,
    })
}

impl Decoder for Worker {
    fn decode(&mut self, data: &[u8], idr: bool, spare: Option<Frame>) -> PictureResult {
        ensure!(
            self.active,
            super::HardwareFailed("VA-API worker was abandoned".into())
        );
        self.tx
            .try_send(Request {
                data: data.to_vec(),
                idr,
                spare,
            })
            .map_err(|_| super::hardware(anyhow::anyhow!("VA-API worker unavailable")))?;
        match receive(&self.rx, std::time::Duration::from_secs(1), &STALLED) {
            Ok(result) => result,
            Err(e) => {
                self.active = false;
                Err(e)
            }
        }
    }
    fn name(&self) -> &'static str {
        self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_deadlines_and_disconnection_report_fatal_hardware_failures() {
        let stalled = std::sync::atomic::AtomicBool::new(false);
        let (tx, rx) = std::sync::mpsc::channel::<u32>();
        let error = receive(&rx, std::time::Duration::ZERO, &stalled).unwrap_err();
        assert!(error.is::<super::super::HardwareFailed>());
        assert!(stalled.load(std::sync::atomic::Ordering::Acquire));
        tx.send(42).unwrap();
        assert_eq!(
            receive(&rx, std::time::Duration::ZERO, &stalled).unwrap(),
            42
        );
        drop(tx);
        assert!(receive(&rx, std::time::Duration::ZERO, &stalled)
            .unwrap_err()
            .is::<super::super::HardwareFailed>());
    }

    #[test]
    fn an_abandoned_worker_cannot_return_late_pictures() {
        let (tx, requests) = std::sync::mpsc::sync_channel(1);
        let (results, rx) = std::sync::mpsc::channel();
        assert!(results.send(Ok(Some(Frame::default()))).is_ok());
        let mut worker = Worker {
            tx,
            rx,
            name: "test VA-API",
            active: false,
        };
        assert!(worker
            .decode(&[], true, None)
            .err()
            .unwrap()
            .is::<super::super::HardwareFailed>());
        assert!(requests.try_recv().is_err());
        assert!(worker.rx.try_recv().unwrap().unwrap().is_some());
    }

    #[test]
    fn probe_readback_ignores_renderer_exports() {
        for enabled in [false, true] {
            assert!(!Output::Readback.exports(enabled));
            assert_eq!(Output::Presentation.exports(enabled), enabled);
        }
    }

    fn test_capability(codec: Codec, ten_bit: bool) -> Capability {
        Capability {
            codec,
            ten_bit,
            node: "/dev/dri/renderD128".into(),
            limit: Limit {
                profile: Profile::for_codec(codec, ten_bit).unwrap(),
                max_width: 4096,
                max_height: 4096,
            },
        }
    }

    fn completed_probe(
        caps: Vec<Capability>,
    ) -> Option<std::sync::mpsc::Receiver<Vec<Capability>>> {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(caps).unwrap();
        Some(rx)
    }

    #[test]
    fn empty_capabilities_retry_after_the_cooldown() {
        let mut cache = CapabilityCache::default();
        let now = Instant::now();
        assert!(cache
            .get(now, Duration::ZERO, |_| completed_probe(Vec::new()))
            .is_empty());
        assert!(cache
            .get(now + Duration::from_secs(29), Duration::ZERO, |_| {
                panic!("cooldown")
            })
            .is_empty());
        let caps = cache.get(now + Duration::from_secs(30), Duration::ZERO, |known| {
            assert!(known.is_empty());
            completed_probe(vec![test_capability(Codec::Hevc, true)])
        });
        assert_eq!(caps.len(), 1);
        assert!(caps[0].ten_bit);
        assert_eq!(
            cache.get(now + Duration::from_secs(59), Duration::ZERO, |_| {
                panic!("cached success")
            }),
            caps
        );
    }

    #[test]
    fn probe_completing_after_its_deadline_is_available_after_the_cooldown() {
        let mut cache = CapabilityCache::default();
        let now = Instant::now();
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(cache.get(now, Duration::ZERO, |_| Some(rx)).is_empty());
        assert!(cache
            .get(now + Duration::from_secs(10), Duration::ZERO, |_| {
                panic!("pending probe")
            })
            .is_empty());
        // The worker finishes just after its deadline, before the next lookup.
        let expected = vec![test_capability(Codec::H264High, false)];
        tx.send(expected.clone()).unwrap();
        assert_eq!(
            cache.get(now + Duration::from_secs(31), Duration::ZERO, |_| {
                panic!("late result was discarded")
            }),
            expected
        );
    }

    #[test]
    fn timed_out_probe_accepts_late_success_without_duplicate_workers() {
        let mut cache = CapabilityCache::default();
        let now = Instant::now();
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(cache.get(now, Duration::ZERO, |_| Some(rx)).is_empty());
        // Even well past the cooldown, a hung probe keeps its sole worker.
        for seconds in [10, 31, 61, 91] {
            assert!(cache
                .get(now + Duration::from_secs(seconds), Duration::ZERO, |_| {
                    panic!("duplicated a pending probe")
                })
                .is_empty());
            assert!(cache.pending.is_some());
        }
        let expected = vec![test_capability(Codec::H264High, false)];
        tx.send(expected.clone()).unwrap();
        let later = now + Duration::from_secs(92);
        let caps = cache.get(later, Duration::ZERO, |_| panic!("pending probe"));
        assert_eq!(caps, expected);
        assert!(cache.pending.is_none());
        assert!(super::super::capability(&caps, Codec::H264High, false, 256, 256).is_ok());
        assert_eq!(
            cache.get(later + Duration::from_secs(29), Duration::ZERO, |_| {
                panic!("late success was not cached")
            }),
            caps
        );
    }

    #[test]
    fn partial_capabilities_retry_unproven_profiles_and_keep_proven_ones() {
        let mut cache = CapabilityCache::default();
        let now = Instant::now();
        let h264 = test_capability(Codec::H264High, false);
        let requested = latch_proto::color::ColorCapabilities {
            hdr: true,
            ten_bit: true,
            peak_nits: 1000,
        };
        let caps = cache.get(now, Duration::ZERO, |_| completed_probe(vec![h264.clone()]));
        assert_eq!(caps, vec![h264.clone()]);
        assert_eq!(
            super::super::codecs(&caps),
            vec![Codec::H264High, Codec::H264Baseline]
        );
        assert!(super::super::capability(&caps, Codec::Hevc, true, 256, 256).is_err());
        let color = super::super::color(&caps, requested, 256, 256);
        assert!(!color.ten_bit && !color.hdr);
        assert_eq!(
            cache.get(now + Duration::from_secs(29), Duration::ZERO, |_| {
                panic!("partial result ignored the cooldown")
            }),
            caps
        );

        let later = now + Duration::from_secs(30);
        let (tx, rx) = std::sync::mpsc::channel();
        assert_eq!(
            cache.get(later, Duration::from_secs(5), |known| {
                assert_eq!(known, vec![h264.clone()]);
                Some(rx)
            }),
            caps
        );
        // Main10 is still unproven after this retry.
        tx.send(vec![test_capability(Codec::Hevc, false)]).unwrap();
        let caps = cache.get(later, Duration::ZERO, |_| panic!("pending probe"));
        assert_eq!(
            caps,
            vec![h264.clone(), test_capability(Codec::Hevc, false)]
        );
        assert!(super::super::capability(&caps, Codec::Hevc, true, 256, 256).is_err());
        let color = super::super::color(&caps, requested, 256, 256);
        assert!(!color.ten_bit && !color.hdr);
        assert_eq!(
            super::super::codecs(&caps),
            vec![Codec::Hevc, Codec::H264High, Codec::H264Baseline]
        );

        let later = later + Duration::from_secs(30);
        let caps = cache.get(later, Duration::ZERO, |known| {
            assert_eq!(known, vec![h264, test_capability(Codec::Hevc, false)]);
            completed_probe(vec![test_capability(Codec::Hevc, true)])
        });
        assert_eq!(caps.len(), 3);
        assert!(super::super::capability(&caps, Codec::Hevc, true, 256, 256).is_ok());
        assert_eq!(super::super::color(&caps, requested, 256, 256), requested);

        // A subsequent failed retry preserves all the proven entries.
        let later = later + Duration::from_secs(30);
        assert_eq!(
            cache.get(later, Duration::ZERO, |_| completed_probe(Vec::new())),
            caps
        );
    }

    #[test]
    fn failed_probe_retries_after_the_cooldown() {
        for disconnected in [false, true] {
            let mut cache = CapabilityCache::default();
            let now = Instant::now();
            assert!(cache
                .get(now, Duration::ZERO, |_| {
                    // Either spawning failed or the worker stopped without a result.
                    if disconnected {
                        let (_, rx) = std::sync::mpsc::channel();
                        Some(rx)
                    } else {
                        None
                    }
                })
                .is_empty());
            assert!(cache.pending.is_none());
            assert!(cache
                .get(now + Duration::from_secs(29), Duration::ZERO, |_| {
                    panic!("cooldown")
                })
                .is_empty());
            assert_eq!(
                cache
                    .get(now + Duration::from_secs(30), Duration::ZERO, |_| {
                        completed_probe(vec![test_capability(Codec::H264High, false)])
                    })
                    .len(),
                1
            );
        }
    }

    /// Runs everywhere: without libva or a render node (CI, containers)
    /// the probe finds nothing and quickly; with a GPU it lists what
    /// decoded its probe picture. Either way the viewer keeps Baseline.
    #[test]
    fn probing_never_fails_and_offers_baseline_regardless() {
        let caps = capabilities();
        let codecs = super::super::codecs(&caps);
        assert_eq!(codecs.last(), Some(&Codec::H264Baseline));
        for cap in caps {
            assert!(cap.node.starts_with("/") || cap.node.is_relative());
        }
    }

    #[test]
    fn baseline_starts_through_the_viewer_entry_point_with_or_without_libva() {
        let (bytes, width, height) = super::super::probe_baseline();
        let mut decoder =
            crate::video::new_decoder_for_stream(Codec::H264Baseline, width, height).unwrap();
        let frame = decoder
            .decode(&bytes, true, None)
            .unwrap()
            .expect("Baseline picture");
        assert!(super::super::probe_picture_ok(&frame, width, height));
    }

    /// With a VA-API GPU, each probed codec decodes a stream of several
    /// pictures too (`LATCH_TEST_VAAPI=1` makes its absence a failure).
    #[test]
    fn probed_decoders_decode_their_streams() {
        let caps = capabilities();
        if caps.is_empty() {
            assert!(
                std::env::var_os("LATCH_TEST_VAAPI").is_none(),
                "LATCH_TEST_VAAPI is set but no VA-API decoder was found"
            );
            return;
        }
        for cap in caps.iter().filter(|c| c.codec == Codec::H264Baseline) {
            let mut decoder = open(cap.codec, 256, 256, false).unwrap();
            let mut encoder = latch_codec::h264::Encoder::new(latch_codec::h264::EncoderConfig {
                width: 256,
                height: 256,
                fps: 30,
                bitrate_kbps: 1_000,
                threads: 1,
            })
            .unwrap();
            let mut picture = latch_codec::Yuv420::new(256, 256);
            for n in 0..10u8 {
                picture.y.fill(30 + n * 20);
                let mut au = Vec::new();
                encoder.encode(&picture, n == 0, &mut au).unwrap();
                let frame = decoder.decode(&au, n == 0, None).unwrap().unwrap();
                assert!(frame.y[0].abs_diff(30 + n * 20) <= 3, "picture {n}");
            }
        }
    }
}
