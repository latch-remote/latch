//! Standalone H.264, HEVC and AV1 decoding. COM and MF stay on a dedicated MTA so callers
//! can create and move the decoder from a UI thread with any apartment.
use super::worker_deadline;
use anyhow::{bail, ensure, Context, Result};
use latch_proto::control::Codec;
use parking_lot::Mutex;
use std::{
    cell::RefCell,
    mem::ManuallyDrop,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc,
    },
    thread,
};
use windows::{
    core::{Interface, VARIANT},
    Win32::{
        Foundation::{CloseHandle, E_NOTIMPL, HANDLE, HMODULE, LUID, WAIT_OBJECT_0},
        Graphics::{
            Direct3D::*,
            Direct3D11::*,
            Direct3D12::{ID3D12Device, ID3D12Resource},
            Dxgi::{Common::*, *},
        },
        Media::MediaFoundation::*,
        System::{
            Com::*,
            Threading::{CreateEventW, WaitForSingleObject},
        },
    },
};

pub struct DecodedNv12 {
    /// 10 when `y` and `uv` hold P010 samples (bits 15:6), else 8.
    pub bit_depth: u8,
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub y_stride: usize,
    pub uv: Vec<u8>,
    pub uv_stride: usize,
    /// The picture left on the GPU instead of in `y` and `uv`, and whether
    /// the decoder's latest sample of the stream was black.
    pub shared: Option<(Arc<SharedSurface>, bool)>,
}

/// Set by [`super::render_on_d3d12`]; read when a decoder starts.
pub(super) static RENDER_ADAPTER: Mutex<Option<LUID>> = Mutex::new(None);

/// Shared textures per decoder: the one being drawn, one waiting in the
/// frame slot, one the GPU may still be reading, and the one being filled.
/// When all are taken the picture goes through the CPU instead.
const SURFACES: usize = 6;
/// Every this many pictures, one also goes to the CPU (asynchronously) so
/// the session can still tell a black picture from a missing one.
const SAMPLE_EVERY: u32 = 15;

/// A shareable D3D11 NV12 texture holding one decoded picture, cropped to
/// the visible size (rounded down to even).
pub struct SharedSurface {
    texture: ID3D11Texture2D,
    handle: HANDLE,
    id: u64,
    width: u32,
    height: u32,
}
// SAFETY: The texture is only kept alive here (D3D11 resources are
// free-threaded) and the handle is a kernel handle, closed once on drop.
unsafe impl Send for SharedSurface {}
unsafe impl Sync for SharedSurface {}
impl std::fmt::Debug for SharedSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedSurface")
            .field("id", &self.id)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}
impl SharedSurface {
    /// Unique for the life of the process, so a renderer can cache what it
    /// opened by it.
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    /// Open this texture on a D3D12 device on the same adapter.
    pub fn open(&self, device: &ID3D12Device) -> Result<ID3D12Resource> {
        let mut resource: Option<ID3D12Resource> = None;
        // SAFETY: The handle stays open for as long as `self`; D3D12 takes
        // its own reference to the resource.
        unsafe { device.OpenSharedHandle(self.handle, &mut resource) }
            .context("open shared decoder texture on D3D12")?;
        resource.context("D3D12 returned no shared texture")
    }
}
impl Drop for SharedSurface {
    fn drop(&mut self) {
        // SAFETY: Created by CreateSharedHandle and closed only here.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}
type Pictures = Vec<(i64, DecodedNv12)>;
/// Consume every ready output before asking the MFT for another input.
/// Keep partial output on error so a retry can still return its identities.
fn drain_output<T>(
    pictures: &mut Vec<T>,
    mut next: impl FnMut() -> Result<Option<T>>,
) -> Result<()> {
    for _ in 0..64 {
        match next()? {
            Some(picture) => pictures.push(picture),
            None => return Ok(()),
        }
    }
    bail!("decoder produced too many outputs without needing input")
}

struct Request {
    bytes: Vec<u8>,
    time: i64,
    reply: mpsc::SyncSender<Result<(Pictures, &'static str)>>,
}
pub struct MfDecoder {
    requests: Option<mpsc::Sender<Request>>,
    stream: (Codec, u32, u32, u8),
    name: &'static str,
    next_time: i64,
}
impl MfDecoder {
    pub fn new() -> Result<Self> {
        Self::with_codec(Codec::H264Baseline)
    }

    pub fn with_codec(codec: Codec) -> Result<Self> {
        Self::with_size(codec, 0, 0)
    }

    pub fn with_size(codec: Codec, width: u32, height: u32) -> Result<Self> {
        Self::with_stream(codec, width, height, 8)
    }

    /// `depth` 10 keeps a Main10 stream's P010 samples (see
    /// [`DecodedNv12::bit_depth`]); 8 reduces any P010 output to NV12.
    pub fn with_stream(codec: Codec, width: u32, height: u32, depth: u8) -> Result<Self> {
        worker_deadline::check()?;
        let (requests, receive) = mpsc::channel::<Request>();
        let (ready, startup) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("latch-mf-decode".into())
            .spawn(move || {
                let adapter = *RENDER_ADAPTER.lock();
                let initialized = (|| {
                    let runtime = Runtime::new()?;
                    let decoder =
                        State::new(codec, true, adapter, width, height, depth).or_else(|e| {
                            tracing::warn!("Media Foundation DXVA: {e:#}; trying without it");
                            State::new(codec, false, None, width, height, depth)
                        })?;
                    Ok::<_, anyhow::Error>((runtime, decoder))
                })();
                let (_runtime, mut decoder) = match initialized {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                };
                let name = if decoder.gpu.is_some() {
                    "Media Foundation (DXVA)"
                } else {
                    "Media Foundation (software)"
                };
                if ready.send(Ok(name)).is_err() {
                    return;
                }
                for request in receive {
                    let result = decoder
                        .decode(&request.bytes, request.time)
                        .map(|frame| (frame, decoder.name()))
                        .context("decode Media Foundation access unit");
                    let _ = request.reply.send(result);
                }
            })
            .context("spawn Media Foundation decoder worker")?;
        Ok(Self {
            requests: Some(requests),
            stream: (codec, width, height, depth),
            name: worker_deadline::receive(&startup, worker_deadline::STARTUP)??,
            next_time: 0,
        })
    }
    /// Convenience for synchronous tools. A buffered or multi-output decoder
    /// fails explicitly here. Sessions use decode_frames and retain every output.
    pub fn decode(&mut self, annexb: &[u8]) -> Result<Option<DecodedNv12>> {
        let time = self.next_time;
        self.next_time = self
            .next_time
            .checked_add(166667)
            .context("sample time overflow")?;
        let mut pictures = self.decode_frames(annexb, time)?;
        ensure!(
            pictures.len() <= 1 && pictures.first().is_none_or(|p| p.0 == time),
            "decoder buffered pictures; use timestamped decode_frames"
        );
        Ok(pictures.pop().map(|(_, picture)| picture))
    }
    /// The timestamp is an opaque, unique input identity in 100 ns units.
    /// Every returned picture carries its MFT output sample's timestamp.
    pub fn decode_frames(&mut self, annexb: &[u8], time: i64) -> Result<Pictures> {
        ensure!(
            annexb.len() <= 64 * 1024 * 1024,
            "video access unit exceeds 64 MiB"
        );
        let (reply, receive) = mpsc::sync_channel(1);
        if self.requests.is_none() {
            let (codec, width, height, depth) = self.stream;
            let next_time = self.next_time;
            *self = Self::with_stream(codec, width, height, depth)
                .context(worker_deadline::ReopenFailed)?;
            self.next_time = next_time;
        }
        self.requests
            .as_ref()
            .expect("reopened worker")
            .send(Request {
                bytes: annexb.to_vec(),
                time,
                reply,
            })
            .context("decoder worker stopped")?;
        let response = match worker_deadline::receive(&receive, worker_deadline::DECODE) {
            Ok(response) => response,
            Err(error) => {
                // Dropping the channel prevents further submissions. The stuck
                // worker owns all COM/GPU resources until the driver returns.
                self.requests = None;
                tracing::warn!("{error:#}; reopening decoder on the next keyframe");
                return Err(error);
            }
        };
        let (frame, name) = response?;
        self.name = name;
        Ok(frame)
    }
    pub fn name(&self) -> &'static str {
        self.name
    }
}
/// Probe actual pictures, not just an installed MFT or a D3D profile GUID.
/// This also catches missing Store codec extensions and unusable driver paths.
pub fn supported() -> Vec<Codec> {
    if worker_deadline::check().is_err() {
        return Vec::new();
    }
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = (|| {
            let Ok(_runtime) = Runtime::new() else {
                return Vec::new();
            };
            [
                (
                    Codec::Av1,
                    include_bytes!("../../tests/fixtures/decode-probe.av1").as_slice(),
                ),
                (
                    Codec::Hevc,
                    include_bytes!("../../tests/fixtures/decode-probe.hevc").as_slice(),
                ),
                (
                    Codec::H264High,
                    include_bytes!("../../tests/fixtures/decode-probe.h264").as_slice(),
                ),
            ]
            .into_iter()
            .filter_map(|(codec, bytes)| {
                let result: Result<()> = (|| {
                    let mut state = State::new(codec, true, None, 256, 256, 8)?;
                    for index in 0..10 {
                        if !state.decode(bytes, index * 166667)?.is_empty() {
                            ensure!(state.dxva_used, "decoder returned CPU samples");
                            return Ok(());
                        }
                    }
                    bail!("no hardware picture within ten access units");
                })();
                match result {
                    Ok(()) => Some(codec),
                    Err(error) => {
                        tracing::info!(?codec, "hardware decode unavailable: {error:#}");
                        None
                    }
                }
            })
            .collect()
        })();
        let _ = tx.send(result);
    });
    worker_deadline::receive(&rx, worker_deadline::PROBE).unwrap_or_default()
}

unsafe fn create_transform(subtype: windows::core::GUID) -> Result<IMFTransform> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    };
    let (mut activates, mut count) = (std::ptr::null_mut(), 0);
    mf::MFTEnumEx(
        MFT_CATEGORY_VIDEO_DECODER,
        MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
        &input,
        &mut activates,
        &mut count,
    )?;
    let mut result = None;
    if !activates.is_null() {
        for activate in std::slice::from_raw_parts_mut(activates, count as usize) {
            if let Some(activate) = activate.take() {
                if result.is_none() {
                    result = activate.ActivateObject::<IMFTransform>().ok();
                }
            }
        }
        CoTaskMemFree(Some(activates.cast()));
    }
    result.context("no synchronous Media Foundation decoder installed for codec")
}

struct Runtime {
    mf: bool,
}
impl Runtime {
    fn new() -> Result<Self> {
        // SAFETY: This guard never leaves its worker and balances both APIs.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }.context("decoder COM MTA")?;
        let mut runtime = Self { mf: false };
        // SAFETY: COM is initialized; failure drops the guard and uninitializes it.
        unsafe { mf::MFStartup(MF_VERSION, MFSTARTUP_FULL) }
            .context("start decoder Media Foundation")?;
        runtime.mf = true;
        Ok(runtime)
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        // SAFETY: Decoder interfaces are destroyed before the runtime, on this thread.
        unsafe {
            if self.mf {
                let _ = mf::MFShutdown();
            }
            CoUninitialize();
        }
    }
}
struct Gpu {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    _manager: IMFDXGIDeviceManager,
    staging: RefCell<Option<ID3D11Texture2D>>,
    shared: Option<Shared>,
}
/// Zero-copy output: each picture is copied, on the GPU, into a free
/// [`SharedSurface`], and the worker waits for that copy before replying.
struct Shared {
    context: ID3D11DeviceContext4,
    fence: ID3D11Fence,
    value: u64,
    event: HANDLE,
    surfaces: Vec<Arc<SharedSurface>>,
    pictures: u32,
    /// The visible area of the picture copied to `staging` with the previous
    /// sample, for the next picture to scan.
    sampled: Option<(Layout, ID3D11Texture2D, u32)>,
    black: bool,
    busy: bool,
}
impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: Created by CreateEventW and closed only here.
        unsafe {
            let _ = CloseHandle(self.event);
        }
    }
}
impl Gpu {
    fn new(transform: &IMFTransform, share: Option<LUID>) -> Result<Self> {
        let (mut device, mut context, mut manager, mut token) = (None, None, None, 0);
        // The renderer's adapter, so that it can open what is decoded here.
        let adapter: Option<IDXGIAdapter> = share.and_then(|luid| {
            // SAFETY: Plain factory creation and lookup; both return owned interfaces.
            unsafe { CreateDXGIFactory1::<IDXGIFactory4>().and_then(|f| f.EnumAdapterByLuid(luid)) }
                .inspect_err(|e| tracing::warn!("renderer adapter: {e}; decoding to the CPU"))
                .ok()
        });
        // SAFETY: All interfaces are owned by the decoder worker. The device's
        // immediate context is protected because the MFT may use driver threads.
        unsafe {
            D3D11CreateDevice(
                adapter.as_ref(),
                if adapter.is_some() {
                    D3D_DRIVER_TYPE_UNKNOWN
                } else {
                    D3D_DRIVER_TYPE_HARDWARE
                },
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("create DXVA D3D11 device")?;
            let device = device.context("missing decoder D3D11 device")?;
            let context: ID3D11DeviceContext = context.context("missing decoder D3D11 context")?;
            let _ = context
                .cast::<ID3D11Multithread>()?
                .SetMultithreadProtected(true);
            ensure!(
                transform
                    .GetAttributes()?
                    .GetUINT32(&MF_SA_D3D11_AWARE)
                    .unwrap_or(0)
                    != 0,
                "decoder is not D3D11 aware"
            );
            mf::MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.context("missing decoder DXGI manager")?;
            manager.ResetDevice(&device, token)?;
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .context("configure decoder DXVA manager")?;
            let shared = match adapter {
                Some(_) => Shared::new(&device, &context)
                    .inspect_err(|e| tracing::warn!("{e:#}; decoding to the CPU"))
                    .ok(),
                None => None,
            };
            Ok(Self {
                device,
                context,
                _manager: manager,
                staging: RefCell::new(None),
                shared,
            })
        }
    }
    fn read(&mut self, buffer: &IMFDXGIBuffer, layout: Layout) -> Result<DecodedNv12> {
        let mut resource = std::ptr::null_mut();
        // SAFETY: GetResource writes an owned texture interface reference. It is
        // adopted immediately, and its subresource is copied before releasing it.
        unsafe {
            buffer
                .GetResource(&ID3D11Texture2D::IID, &mut resource)
                .context("get DXVA output texture")?;
            ensure!(!resource.is_null(), "DXVA returned no texture");
            let texture = ID3D11Texture2D::from_raw(resource);
            let subresource = buffer.GetSubresourceIndex()?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);
            ensure!(
                matches!(desc.Format, DXGI_FORMAT_NV12 | DXGI_FORMAT_P010)
                    && layout.coded_width <= desc.Width
                    && layout.coded_height <= desc.Height,
                "invalid DXVA NV12 texture dimensions"
            );
            if self.shared.is_some() && RENDER_ADAPTER.lock().is_none() {
                self.shared = None;
            }
            // The renderer has no ten-bit planar format to open P010 with.
            if desc.Format == DXGI_FORMAT_NV12
                && layout.width.is_multiple_of(2)
                && layout.height.is_multiple_of(2)
            {
                if let Some(mut shared) = self.shared.take() {
                    match self.share(&mut shared, &texture, subresource, &desc, layout) {
                        Ok(Some(picture)) => {
                            self.shared = Some(shared);
                            return Ok(picture);
                        }
                        Ok(None) => {
                            if !std::mem::replace(&mut shared.busy, true) {
                                tracing::debug!("every shared decoder texture is in use");
                            }
                            self.shared = Some(shared);
                        }
                        Err(e) => tracing::warn!("{e:#}; decoding to the CPU"),
                    }
                }
            }
            let staging = self.staging(&desc)?;
            self.context
                .CopySubresourceRegion(&staging, 0, 0, 0, 0, &texture, subresource, None);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("map DXVA output")?;
            let bytes = std::slice::from_raw_parts(
                mapped.pData.cast::<u8>(),
                mapped.RowPitch as usize * desc.Height as usize * 3 / 2,
            );
            let result = layout.copy(bytes, mapped.RowPitch as usize, desc.Height as usize);
            self.context.Unmap(&staging, 0);
            result
        }
    }
    /// A CPU-readable texture like `desc`, kept between pictures.
    fn staging(&self, desc: &D3D11_TEXTURE2D_DESC) -> Result<ID3D11Texture2D> {
        let mut desc = *desc;
        desc.MipLevels = 1;
        desc.ArraySize = 1;
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut cached = self.staging.borrow_mut();
        // SAFETY: Descriptor queries and creation on the worker's device.
        unsafe {
            let matches = cached.as_ref().is_some_and(|texture| {
                let mut old = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&mut old);
                (old.Width, old.Height, old.Format) == (desc.Width, desc.Height, desc.Format)
            });
            if !matches {
                let mut staging = None;
                self.device
                    .CreateTexture2D(&desc, None, Some(&mut staging))
                    .context("allocate DXVA staging texture")?;
                *cached = staging;
            }
        }
        cached.clone().context("missing decoder staging texture")
    }
    /// Copy the visible picture into a free shared texture and wait for the
    /// copy. `None` when every shared texture is still in use.
    ///
    /// # Safety
    /// `texture` and `subresource` are the decoder's current output.
    unsafe fn share(
        &self,
        shared: &mut Shared,
        texture: &ID3D11Texture2D,
        subresource: u32,
        desc: &D3D11_TEXTURE2D_DESC,
        layout: Layout,
    ) -> Result<Option<DecodedNv12>> {
        // The copy sampled with the previous picture finished before that
        // picture's fence, so this map does not wait for the GPU.
        if let Some((sampled, staging, rows)) = shared.sampled.take() {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("map sampled DXVA output")?;
            let (pitch, rows) = (mapped.RowPitch as usize, rows as usize);
            let bytes = std::slice::from_raw_parts(mapped.pData.cast::<u8>(), pitch * rows * 3 / 2);
            let (left, top) = (sampled.left as usize, sampled.top as usize);
            shared.black = super::planes_black(
                (&bytes[(top * pitch + left).min(bytes.len())..], pitch),
                (
                    &bytes[((rows + top / 2) * pitch + left).min(bytes.len())..],
                    pitch,
                ),
                (sampled.width, sampled.height),
                super::PixelFormat::Nv12,
                false,
            );
            self.context.Unmap(&staging, 0);
        }
        // NV12 copies start and end on even pixels.
        let (left, top) = (layout.left & !1, layout.top & !1);
        let width = layout.width.min(desc.Width - left) & !1;
        let height = layout.height.min(desc.Height - top) & !1;
        ensure!(width > 0 && height > 0, "empty decoded picture");
        shared
            .surfaces
            .retain(|s| s.size() == (width, height) || Arc::strong_count(s) > 1);
        let surface = match shared
            .surfaces
            .iter()
            .find(|s| s.size() == (width, height) && Arc::strong_count(s) == 1)
        {
            Some(surface) => surface.clone(),
            None if shared.surfaces.len() < SURFACES => {
                let surface = Arc::new(SharedSurface::new(&self.device, width, height)?);
                shared.surfaces.push(surface.clone());
                surface
            }
            None => return Ok(None),
        };
        shared.busy = false;
        self.context.CopySubresourceRegion(
            &surface.texture,
            0,
            0,
            0,
            0,
            texture,
            subresource,
            Some(&D3D11_BOX {
                left,
                top,
                front: 0,
                right: left + width,
                bottom: top + height,
                back: 1,
            }),
        );
        shared.pictures = shared.pictures.wrapping_add(1);
        if shared.pictures.is_multiple_of(SAMPLE_EVERY) {
            let staging = self.staging(desc)?;
            self.context
                .CopySubresourceRegion(&staging, 0, 0, 0, 0, texture, subresource, None);
            shared.sampled = Some((layout, staging, desc.Height));
        }
        shared.value += 1;
        shared.context.Signal(&shared.fence, shared.value)?;
        self.context.Flush();
        shared
            .fence
            .SetEventOnCompletion(shared.value, shared.event)
            .context("decoder fence event")?;
        ensure!(
            WaitForSingleObject(shared.event, 2000) == WAIT_OBJECT_0,
            "decoder copy did not finish"
        );
        Ok(Some(DecodedNv12 {
            bit_depth: 8,
            width: layout.width,
            height: layout.height,
            y: Vec::new(),
            y_stride: 0,
            uv: Vec::new(),
            uv_stride: 0,
            shared: Some((surface, shared.black)),
        }))
    }
}
impl Shared {
    fn new(device: &ID3D11Device, context: &ID3D11DeviceContext) -> Result<Self> {
        // SAFETY: Interface queries and creation on the worker's device.
        unsafe {
            let context = context
                .cast::<ID3D11DeviceContext4>()
                .context("D3D11.4 context for decoder fences")?;
            let mut fence: Option<ID3D11Fence> = None;
            device
                .cast::<ID3D11Device5>()
                .context("D3D11.5 device for decoder fences")?
                .CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence)
                .context("create decoder fence")?;
            Ok(Self {
                context,
                fence: fence.context("missing decoder fence")?,
                value: 0,
                event: CreateEventW(None, false, false, None).context("decoder fence event")?,
                surfaces: Vec::new(),
                pictures: 0,
                sampled: None,
                black: false,
                busy: false,
            })
        }
    }
}
impl SharedSurface {
    fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
        static IDS: AtomicU64 = AtomicU64::new(1);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0)
                as u32,
        };
        let mut texture = None;
        // SAFETY: Creation on the worker's device; the handle is owned by the
        // returned surface.
        unsafe {
            device
                .CreateTexture2D(&desc, None, Some(&mut texture))
                .context("allocate shared decoder texture")?;
            let texture: ID3D11Texture2D = texture.context("missing shared decoder texture")?;
            let handle = texture
                .cast::<IDXGIResource1>()?
                .CreateSharedHandle(
                    None,
                    (DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE).0,
                    None,
                )
                .context("share decoder texture")?;
            Ok(Self {
                texture,
                handle,
                id: IDS.fetch_add(1, Ordering::Relaxed),
                width,
                height,
            })
        }
    }
}
#[derive(Clone, Copy)]
struct Layout {
    coded_width: u32,
    coded_height: u32,
    left: u32,
    top: u32,
    width: u32,
    height: u32,
    stride: usize,
    p010: bool,
    /// Keep P010's ten bits instead of reducing them to NV12.
    ten_bit: bool,
}
impl Layout {
    fn from_type(ty: &IMFMediaType, depth: u8) -> Result<Self> {
        // SAFETY: Attribute reads are owned values; the aperture blob is read
        // unaligned into its Win32 value type only after checking its byte size.
        unsafe {
            let size = ty
                .GetUINT64(&MF_MT_FRAME_SIZE)
                .context("decoder frame size")?;
            let (coded_width, coded_height) = ((size >> 32) as u32, size as u32);
            ensure!(
                coded_width > 0
                    && coded_height > 0
                    && coded_width <= 16384
                    && coded_height <= 16384,
                "invalid decoded dimensions"
            );
            let p010 = ty.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_P010;
            let mut layout = Self {
                p010,
                ten_bit: p010 && depth == 10,
                coded_width,
                coded_height,
                left: 0,
                top: 0,
                width: coded_width,
                height: coded_height,
                stride: ty
                    .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                    .unwrap_or(coded_width.div_ceil(2) * 2 * if p010 { 2 } else { 1 })
                    as usize,
            };
            for key in [MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_GEOMETRIC_APERTURE] {
                if let Ok(size) = ty.GetBlobSize(&key) {
                    ensure!(
                        size as usize == std::mem::size_of::<MFVideoArea>(),
                        "invalid decoder aperture size"
                    );
                    let mut blob = vec![0; size as usize];
                    ty.GetBlob(&key, &mut blob, None)?;
                    let area = blob.as_ptr().cast::<MFVideoArea>().read_unaligned();
                    ensure!(
                        area.OffsetX.value >= 0
                            && area.OffsetX.fract == 0
                            && area.OffsetY.value >= 0
                            && area.OffsetY.fract == 0
                            && area.Area.cx > 0
                            && area.Area.cy > 0,
                        "invalid decoder aperture"
                    );
                    layout.left = area.OffsetX.value as u32;
                    layout.top = area.OffsetY.value as u32;
                    layout.width = area.Area.cx as u32;
                    layout.height = area.Area.cy as u32;
                    break;
                }
            }
            ensure!(
                layout.left + layout.width <= coded_width
                    && layout.top + layout.height <= coded_height
                    && layout.left.is_multiple_of(2)
                    && layout.top.is_multiple_of(2),
                "decoder crop exceeds NV12 bounds"
            );
            Ok(layout)
        }
    }
    fn copy(self, bytes: &[u8], stride: usize, surface_height: usize) -> Result<DecodedNv12> {
        let (left, top, width, height) = (
            self.left as usize,
            self.top as usize,
            self.width as usize,
            self.height as usize,
        );
        let uv_width = width.div_ceil(2) * 2;
        let bytes_per_sample = if self.p010 { 2 } else { 1 };
        // Ten-bit output keeps both P010 bytes; otherwise P010 is reduced to
        // its high byte, the eight most significant bits.
        let kept = if self.ten_bit { 2 } else { 1 };
        let left = left * bytes_per_sample;
        ensure!(
            stride >= left + uv_width * bytes_per_sample && surface_height >= top + height,
            "invalid NV12 pitch or height"
        );
        let mut y = Vec::with_capacity(width * kept * height);
        let mut uv = Vec::with_capacity(uv_width * kept * height.div_ceil(2));
        for row in top..top + height {
            let start = row
                .checked_mul(stride)
                .and_then(|n| n.checked_add(left))
                .context("NV12 Y offset overflow")?;
            let row = bytes
                .get(start..start + width * bytes_per_sample)
                .context("truncated decoded Y plane")?;
            if self.p010 && !self.ten_bit {
                y.extend(row.iter().skip(1).step_by(2));
            } else {
                y.extend_from_slice(row);
            }
        }
        for row in top / 2..top / 2 + height.div_ceil(2) {
            let start = (surface_height + row)
                .checked_mul(stride)
                .and_then(|n| n.checked_add(left))
                .context("NV12 UV offset overflow")?;
            let row = bytes
                .get(start..start + uv_width * bytes_per_sample)
                .context("truncated decoded UV plane")?;
            if self.p010 && !self.ten_bit {
                uv.extend(row.iter().skip(1).step_by(2));
            } else {
                uv.extend_from_slice(row);
            }
        }
        Ok(DecodedNv12 {
            bit_depth: if self.ten_bit { 10 } else { 8 },
            width: self.width,
            height: self.height,
            y,
            y_stride: width * kept,
            uv,
            uv_stride: uv_width * kept,
            shared: None,
        })
    }
}
struct State {
    depth: u8,
    transform: IMFTransform,
    gpu: Option<Gpu>,
    output_type: Option<IMFMediaType>,
    pictures: Pictures,
    codec: Codec,
    dxva_used: bool,
}
impl State {
    fn name(&self) -> &'static str {
        if self.dxva_used {
            "Media Foundation (DXVA)"
        } else {
            "Media Foundation (software)"
        }
    }
    /// `share`: the renderer's adapter, to decode on it and hand pictures
    /// over as [`SharedSurface`]s.
    fn new(
        codec: Codec,
        dxva: bool,
        share: Option<LUID>,
        width: u32,
        height: u32,
        depth: u8,
    ) -> Result<Self> {
        // SAFETY: This runs only in the initialized decoder MTA.
        unsafe {
            let subtype = match codec {
                Codec::Hevc => MFVideoFormat_HEVC,
                Codec::Av1 => MFVideoFormat_AV1,
                _ => MFVideoFormat_H264,
            };
            let transform = create_transform(subtype)?;
            // MF_LOW_LATENCY and CODECAPI_AVLowLatencyMode share one GUID.
            // Set the UINT32 transform attribute before types. The optional
            // ICodecAPI interface takes VT_BOOL. Read both interfaces back
            // after setup so diagnostics reflect the decoder actually in use.
            // A transform without attributes answers E_NOTIMPL and runs with
            // the bitstream's explicit reordering limits.
            match transform.GetAttributes() {
                Ok(attributes) => {
                    attributes
                        .SetUINT32(&CODECAPI_AVLowLatencyMode, 1)
                        .context("enable decoder low-latency mode")?;
                }
                Err(error) if error.code() == E_NOTIMPL => {
                    tracing::debug!("the decoder has no attributes; no low-latency attribute")
                }
                Err(error) => return Err(error).context("decoder attributes"),
            }
            if let Ok(api) = transform.cast::<ICodecAPI>() {
                if let Err(error) = api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true)) {
                    tracing::debug!("decoder ICodecAPI low-latency mode: {error}");
                }
            }
            let gpu = if dxva {
                Some(Gpu::new(&transform, share)?)
            } else {
                None
            };
            let input = mf::MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
            if width != 0 && height != 0 {
                input.SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    (u64::from(width) << 32) | u64::from(height),
                )?;
                input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            }
            transform
                .SetInputType(0, &input, 0)
                .context("decoder compressed input type")?;
            let mut state = Self {
                depth,
                transform,
                gpu,
                output_type: None,
                pictures: Vec::new(),
                codec,
                dxva_used: dxva,
            };
            match state.output_type() {
                Err(e)
                    if e.downcast_ref::<windows::core::Error>().is_some_and(|e| {
                        matches!(
                            e.code(),
                            MF_E_TRANSFORM_TYPE_NOT_SET
                                | MF_E_ATTRIBUTENOTFOUND
                                | MF_E_INVALIDMEDIATYPE
                        )
                    }) => {}
                other => other?,
            }
            state
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .context("start decoder streaming")?;
            state
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("start decoder input stream")?;
            tracing::info!(settings = ?state.low_latency(), "decoder low-latency readback");
            Ok(state)
        }
    }
    fn low_latency(&self) -> (Option<u32>, Option<bool>) {
        // SAFETY: Read on the same MTA as transform creation and decode.
        unsafe {
            let attributes = self.transform.GetAttributes().ok();
            let mf = attributes
                .as_ref()
                .and_then(|a| a.GetUINT32(&MF_LOW_LATENCY).ok());
            let api = self
                .transform
                .cast::<ICodecAPI>()
                .ok()
                .and_then(|a| a.GetValue(&CODECAPI_AVLowLatencyMode).ok())
                .and_then(|v| bool::try_from(&v).ok());
            (mf, api)
        }
    }
    fn output_type(&mut self) -> Result<()> {
        // SAFETY: Type negotiation is serialized with ProcessInput/Output.
        unsafe {
            // Eight bits take the first NV12 or P010 type. A ten-bit stream
            // needs P010, which a decoder offers only once it has parsed the
            // sequence header; until then NV12 stands in, and the stream
            // change that follows the header picks P010.
            let mut stand_in = None;
            let mut chosen = None;
            for index in 0..256 {
                let ty = match self.transform.GetOutputAvailableType(0, index) {
                    Ok(ty) => ty,
                    Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
                    Err(e) => return Err(e).context("enumerate decoder output types"),
                };
                let subtype = ty.GetGUID(&MF_MT_SUBTYPE)?;
                if subtype == MFVideoFormat_P010
                    || (subtype == MFVideoFormat_NV12 && self.depth != 10)
                {
                    chosen = Some(ty);
                    break;
                }
                if subtype == MFVideoFormat_NV12 && stand_in.is_none() {
                    stand_in = Some(ty);
                }
            }
            let ty = chosen
                .or(stand_in)
                .context("decoder offers no NV12 output")?;
            match self.transform.SetOutputType(0, &ty, 0) {
                Err(e) if e.code() == MF_E_UNSUPPORTED_D3D_TYPE && self.gpu.is_some() => {
                    self.transform
                        .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0)
                        .context("disable unsupported decoder DXVA mode")?;
                    self.gpu = None;
                    self.dxva_used = false;
                    self.transform
                        .SetOutputType(0, &ty, 0)
                        .context("set software decoder NV12 type")?;
                }
                result => result.context("set decoder NV12 output type")?,
            }
            self.output_type = Some(ty);
            Ok(())
        }
    }
    fn decode(&mut self, annexb: &[u8], time: i64) -> Result<Pictures> {
        use latch_codec::bitstream::{low_delay, Format};
        let bytes = match self.codec {
            Codec::H264Baseline | Codec::H264High => low_delay(annexb, Format::H264)?,
            Codec::Hevc => low_delay(annexb, Format::Hevc)?,
            Codec::Av1 => std::borrow::Cow::Borrowed(annexb),
        };
        self.decode_input(&bytes, time)
    }
    fn decode_input(&mut self, annexb: &[u8], time: i64) -> Result<Pictures> {
        if self.output_type.is_some() {
            self.drain()?;
        }
        if !annexb.is_empty() {
            let length = u32::try_from(annexb.len())?;
            // SAFETY: The input is copied into a locked buffer before being given
            // to the transform, which retains its own reference if necessary.
            unsafe {
                let sample = mf::MFCreateSample()?;
                let buffer = mf::MFCreateMemoryBuffer(length)?;
                let mut data = std::ptr::null_mut();
                buffer.Lock(&mut data, None, None)?;
                std::ptr::copy_nonoverlapping(annexb.as_ptr(), data, annexb.len());
                buffer.Unlock()?;
                buffer.SetCurrentLength(length)?;
                sample.AddBuffer(&buffer)?;
                sample.SetSampleTime(time)?;
                sample.SetSampleDuration(166667)?;
                match self.transform.ProcessInput(0, &sample, 0) {
                    Err(e) if e.code() == MF_E_NOTACCEPTING => {
                        self.drain()?;
                        self.transform
                            .ProcessInput(0, &sample, 0)
                            .context("decoder retry ProcessInput")?;
                    }
                    result => result.context("decoder ProcessInput")?,
                }
            }
        }
        self.drain()?;
        Ok(std::mem::take(&mut self.pictures))
    }
    fn drain(&mut self) -> Result<()> {
        if self.output_type.is_none() {
            match self.output_type() {
                Err(e)
                    if e.downcast_ref::<windows::core::Error>()
                        .is_some_and(|e| e.code() == MF_E_TRANSFORM_TYPE_NOT_SET) =>
                {
                    return Ok(())
                }
                other => other?,
            }
        }
        let mut pictures = std::mem::take(&mut self.pictures);
        let result = drain_output(&mut pictures, || self.read_output());
        self.pictures = pictures;
        result
    }
    fn read_output(&mut self) -> Result<Option<(i64, DecodedNv12)>> {
        // SAFETY: ManuallyDrop output fields are released on every HRESULT. All
        // output buffers are copied before the transform can recycle its sample.
        unsafe {
            for _ in 0..64 {
                let info = self.transform.GetOutputStreamInfo(0)?;
                let sample = if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 == 0 {
                    let sample = mf::MFCreateSample()?;
                    sample.AddBuffer(&mf::MFCreateAlignedMemoryBuffer(
                        info.cbSize.max(1),
                        info.cbAlignment.saturating_sub(1),
                    )?)?;
                    Some(sample)
                } else {
                    None
                };
                let mut output = MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(sample),
                    ..Default::default()
                };
                let mut status = 0;
                let result =
                    self.transform
                        .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status);
                let sample = ManuallyDrop::take(&mut output.pSample);
                ManuallyDrop::drop(&mut output.pEvents);
                match result {
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        self.output_type()?;
                        continue;
                    }
                    result => result.context("decoder ProcessOutput")?,
                }
                let sample = sample.context("decoder returned no output sample")?;
                let layout = Layout::from_type(
                    self.output_type
                        .as_ref()
                        .context("decoder has no output type")?,
                    self.depth,
                )?;
                let buffer = sample.GetBufferByIndex(0)?;
                let picture = if let Ok(dxgi) = buffer.cast::<IMFDXGIBuffer>() {
                    self.dxva_used = true;
                    self.gpu
                        .as_mut()
                        .context("DXVA sample without a D3D11 device")?
                        .read(&dxgi, layout)?
                } else if let Ok(buffer2d) = buffer.cast::<IMF2DBuffer2>() {
                    self.dxva_used = false;
                    let (mut scan, mut start, mut pitch, mut length) =
                        (std::ptr::null_mut(), std::ptr::null_mut(), 0, 0);
                    buffer2d.Lock2DSize(
                        MF2DBuffer_LockFlags_Read,
                        &mut scan,
                        &mut pitch,
                        &mut start,
                        &mut length,
                    )?;
                    let result = (|| {
                        ensure!(
                            pitch > 0 && scan as usize >= start as usize,
                            "invalid NV12 2D buffer"
                        );
                        let offset = scan as usize - start as usize;
                        let all = std::slice::from_raw_parts(start, length as usize);
                        layout.copy(
                            all.get(offset..).context("invalid NV12 scanline")?,
                            pitch as usize,
                            layout.coded_height as usize,
                        )
                    })();
                    buffer2d.Unlock2D()?;
                    result?
                } else {
                    self.dxva_used = false;
                    let buffer = sample.ConvertToContiguousBuffer()?;
                    let (mut data, mut length) = (std::ptr::null_mut(), 0);
                    buffer.Lock(&mut data, None, Some(&mut length))?;
                    let result = layout.copy(
                        std::slice::from_raw_parts(data, length as usize),
                        layout.stride,
                        layout.coded_height as usize,
                    );
                    buffer.Unlock()?;
                    result?
                };
                let time = sample
                    .GetSampleTime()
                    .context("decoder output has no sample time")?;
                return Ok(Some((time, picture)));
            }
        }
        bail!("decoder changed output format too many times")
    }
}
impl Drop for State {
    fn drop(&mut self) {
        // SAFETY: The MFT is stopped before its manager/device and MF runtime drop.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if self.gpu.is_some() {
                let _ = self
                    .transform
                    .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn one_drain_returns_every_ready_picture_in_order() -> Result<()> {
        // Reproduce the missing Linux capture identities 2..5: all five
        // pictures become ready before the next ProcessInput. The source
        // is deterministic and uses the collector State::drain calls.
        let mut ready: std::collections::VecDeque<(i64, Vec<u8>)> = (2..=6)
            .map(|identity| (identity * 166667, vec![identity as u8; 6]))
            .collect();
        let mut pictures = Vec::new();
        drain_output(&mut pictures, || Ok(ready.pop_front()))?;
        assert!(ready.is_empty());
        assert_eq!(
            pictures,
            (2..=6)
                .map(|i| (i * 166667, vec![i as u8; 6]))
                .collect::<Vec<_>>()
        );
        // A later drain leaves the returned batch intact.
        drain_output(&mut pictures, || Ok(None))?;
        assert_eq!(pictures.len(), 5);
        Ok(())
    }
    #[test]
    fn a_partial_drain_keeps_output_across_an_error_and_retry() -> Result<()> {
        let mut pictures = Vec::new();
        let mut outputs = [
            Ok(Some(2)),
            Ok(Some(3)),
            Err(anyhow::anyhow!("MFT failure")),
        ]
        .into_iter();
        assert!(drain_output(&mut pictures, || outputs.next().unwrap()).is_err());
        assert_eq!(pictures, [2, 3]);
        let mut ready = [4, 5, 6].into_iter();
        drain_output(&mut pictures, || Ok(ready.next()))?;
        assert_eq!(pictures, [2, 3, 4, 5, 6]);
        Ok(())
    }
    #[test]
    fn installed_hardware_decoders_produce_visible_pixels() {
        let codecs = supported();
        eprintln!("hardware decoders: {codecs:?}");
        for codec in codecs {
            let bytes = match codec {
                Codec::Av1 => include_bytes!("../../tests/fixtures/decode-probe.av1").as_slice(),
                Codec::Hevc => include_bytes!("../../tests/fixtures/decode-probe.hevc").as_slice(),
                _ => include_bytes!("../../tests/fixtures/decode-probe.h264").as_slice(),
            };
            let mut decoder = MfDecoder::with_size(codec, 256, 256).unwrap();
            let frame = decoder.decode(bytes).unwrap().expect("picture");
            assert_eq!((frame.width, frame.height), (256, 256));
            assert!(frame.y[64 * frame.y_stride + 128] >= 230);
            assert!(frame.y[192 * frame.y_stride + 128] <= 20);
            assert_eq!(decoder.name(), "Media Foundation (DXVA)");
        }
    }
    #[test]
    fn linux_capture_outputs_keep_their_sample_times_and_pixels() -> Result<()> {
        let _runtime = Runtime::new()?;
        let bytes = include_bytes!("../../tests/fixtures/linux-aarch64-capture.h264");
        let units = latch_codec::h264::nal::access_units(bytes);
        assert_eq!(units.len(), 24);
        let mut state = State::new(Codec::H264Baseline, false, None, 160, 98, 8)?;
        let mut reference = latch_codec::h264::Decoder::new();
        for (index, unit) in units.into_iter().enumerate() {
            assert!(
                matches!(
                    latch_codec::bitstream::low_delay(unit, latch_codec::bitstream::Format::H264)?,
                    std::borrow::Cow::Borrowed(_)
                ),
                "Linux software SPS already declares zero reordering"
            );
            let time = index as i64 * 166667;
            let pictures = state.decode(unit, time)?;
            assert_eq!(
                pictures.iter().map(|p| p.0).collect::<Vec<_>>(),
                [time],
                "Linux capture input {index}"
            );
            check_picture(
                &pictures[0].1,
                reference.decode(unit)?.context("Linux reference picture")?,
            );
        }
        Ok(())
    }
    fn tape() -> Vec<&'static [u8]> {
        latch_codec::h264::nal::access_units(include_bytes!(
            "../../../../tests/pairs/mac-vt-6.0.0.h264"
        ))
    }
    fn replay(state: &mut State, patch: bool) -> Result<Vec<Vec<i64>>> {
        let mut calls = Vec::new();
        for (index, bytes) in tape().into_iter().enumerate() {
            let time = index as i64 * 166667;
            let pictures = if patch {
                state.decode(bytes, time)?
            } else {
                state.decode_input(bytes, time)?
            };
            calls.push(
                pictures
                    .into_iter()
                    .map(|(time, _)| time / 166667)
                    .collect(),
            );
        }
        Ok(calls)
    }
    #[test]
    fn legacy_mac_outputs_every_picture_with_its_own_sample_time() -> Result<()> {
        let _runtime = Runtime::new()?;
        let mut raw = State::new(Codec::H264Baseline, false, None, 160, 98, 8)?;
        eprintln!(
            "MF low latency (shared MF/CODECAPI attribute, ICodecAPI): {:?}",
            raw.low_latency()
        );
        let before = replay(&mut raw, false)?;
        eprintln!("MF software old host before patch: {before:?}");
        // Read all returned sample times. This test retains delayed outputs
        // even on an MFT version that has learned to release old streams early.
        let observed: Vec<_> = before.iter().flatten().copied().collect();
        assert!(observed.windows(2).all(|pair| pair[0] < pair[1]));
        let mut patched = State::new(Codec::H264Baseline, false, None, 160, 98, 8)?;
        let after = replay(&mut patched, true)?;
        assert_eq!(after, (0..24).map(|i| vec![i]).collect::<Vec<_>>());
        eprintln!("MF software old host after patch: {after:?}");
        // A new host's on-wire normalized stream takes the same path and
        // does not rely on a second rewrite to release its pictures.
        let mut host = State::new(Codec::H264Baseline, false, None, 160, 98, 8)?;
        for (index, bytes) in tape().into_iter().enumerate() {
            let fixed =
                latch_codec::bitstream::low_delay(bytes, latch_codec::bitstream::Format::H264)?;
            let pictures = host.decode_input(&fixed, index as i64 * 166667)?;
            assert_eq!(
                pictures.iter().map(|p| p.0).collect::<Vec<_>>(),
                [index as i64 * 166667]
            );
        }
        Ok(())
    }
    #[test]
    fn explicit_reorder_limit_controls_poc_zero_decoder_delay() -> Result<()> {
        let _runtime = Runtime::new()?;
        // Same recorded VT SPS, same POC type 0 and slices. Only the last
        // two VUI restriction values differ. Larger DPB storage alone must
        // not authorize output reordering.
        for (reorder, buffer, hex) in [
            (0, 1, "2742c015ab4187f08c44d40404041e1108d4"),
            (0, 9, "2742c015ab4187f08c44d40404041e1108c540"),
            (1, 1, "2742c015ab4187f08c44d40404041e1108a5"),
            (1, 9, "2742c015ab4187f08c44d40404041e1108a150"),
            (9, 9, "2742c015ab4187f08c44d40404041e11088a15"),
        ] {
            let sps: Vec<_> = hex
                .as_bytes()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u8::from_str_radix(std::str::from_utf8(b).unwrap(), 16).unwrap())
                .collect();
            let fields = latch_codec::bitstream::h264_sps(&sps)?;
            assert_eq!(
                (fields.poc_type, fields.reorder_frames, fields.buffering),
                (0, Some(reorder), Some(buffer))
            );
            let mut state = State::new(Codec::H264Baseline, false, None, 160, 98, 8)?;
            let mut calls = Vec::new();
            for (index, bytes) in tape().into_iter().take(6).enumerate() {
                let mut unit = Vec::new();
                for nal in latch_codec::h264::nal::nal_units(bytes) {
                    unit.extend_from_slice(&[0, 0, 0, 1]);
                    unit.extend_from_slice(if nal[0] & 31 == 7 { &sps } else { nal });
                }
                calls.push(
                    state
                        .decode_input(&unit, index as i64 * 166667)?
                        .into_iter()
                        .map(|p| p.0 / 166667)
                        .collect::<Vec<_>>(),
                );
            }
            eprintln!("MF POC=0 reorder={reorder} buffering={buffer}: {calls:?}");
            if reorder == 0 {
                assert_eq!(calls, (0..6).map(|i| vec![i]).collect::<Vec<_>>());
            }
        }
        Ok(())
    }
    fn long_gop_delay(hardware: bool) -> Result<()> {
        for (bytes, width, height) in [
            (
                include_bytes!("../../tests/fixtures/mac-long-gop-tiny.h264").as_slice(),
                160,
                98,
            ),
            (
                include_bytes!("../../tests/fixtures/mac-long-gop-1080.h264").as_slice(),
                1920,
                1080,
            ),
        ] {
            let units = latch_codec::h264::nal::access_units(bytes);
            for patch in [false, true] {
                let mut state = State::new(Codec::H264Baseline, hardware, None, width, height, 8)?;
                let mut output = Vec::new();
                for (index, unit) in units.iter().enumerate() {
                    let time = index as i64 * 166667;
                    let pictures = if patch {
                        state.decode(unit, time)?
                    } else {
                        state.decode_input(unit, time)?
                    };
                    if hardware && !pictures.is_empty() {
                        ensure!(state.dxva_used, "decoder returned CPU samples");
                    }
                    if patch {
                        assert_eq!(
                            pictures.iter().map(|p| p.0).collect::<Vec<_>>(),
                            [time],
                            "long-GOP input {index}"
                        );
                    }
                    output.extend(
                        pictures
                            .into_iter()
                            .map(|(time, _)| (index, time / 166667, index as i64 - time / 166667)),
                    );
                }
                eprintln!("long GOP {width}x{height} hardware={hardware} patch={patch}, (call, input, delay)={output:?}");
            }
        }
        Ok(())
    }
    #[test]
    fn long_gop_sps_normalization_eliminates_decoder_delay() -> Result<()> {
        let _runtime = Runtime::new()?;
        long_gop_delay(false)
    }
    fn check_picture(frame: &DecodedNv12, reference: &latch_codec::Yuv420) {
        assert_eq!(
            (frame.width, frame.height),
            (reference.width, reference.height)
        );
        let width = frame.width as usize;
        let height = frame.height as usize;
        for (data, stride, expected, expected_stride, w, h, step, offset) in [
            (
                &frame.y,
                frame.y_stride,
                &reference.y,
                reference.y_stride,
                width,
                height,
                1,
                0,
            ),
            (
                &frame.uv,
                frame.uv_stride,
                &reference.u,
                reference.c_stride,
                width / 2,
                height / 2,
                2,
                0,
            ),
            (
                &frame.uv,
                frame.uv_stride,
                &reference.v,
                reference.c_stride,
                width / 2,
                height / 2,
                2,
                1,
            ),
        ] {
            let mut squared_error = 0f64;
            for y in 0..h {
                for x in 0..w {
                    squared_error += (f64::from(data[y * stride + x * step + offset])
                        - f64::from(expected[y * expected_stride + x]))
                    .powi(2);
                }
            }
            let mse = squared_error / (w * h) as f64;
            assert!(mse <= 6.5025, "decoded picture PSNR below 40 dB, MSE={mse}");
        }
    }
    #[test]
    #[ignore = "decode-only qualification requires a real DXVA adapter"]
    fn hardware_legacy_mac_outputs_immediately() -> Result<()> {
        let _runtime = Runtime::new()?;
        let mut raw = State::new(Codec::H264Baseline, true, None, 160, 98, 8)?;
        eprintln!("DXVA old host before patch: {:?}", replay(&mut raw, false)?);
        for patch in [false, true] {
            let mut reference = latch_codec::h264::Decoder::new();
            let mut state = State::new(Codec::H264Baseline, true, None, 160, 98, 8)?;
            let adapter = unsafe {
                state
                    .gpu
                    .as_ref()
                    .context("no GPU")?
                    .device
                    .cast::<IDXGIDevice>()?
                    .GetAdapter()?
                    .GetDesc()?
            };
            let end = adapter
                .Description
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(adapter.Description.len());
            eprintln!(
                "DXVA adapter {}",
                String::from_utf16_lossy(&adapter.Description[..end])
            );
            let units = tape();
            let mut calls = Vec::new();
            for (index, bytes) in units.into_iter().enumerate() {
                let fixed =
                    latch_codec::bitstream::low_delay(bytes, latch_codec::bitstream::Format::H264)?;
                let pictures = if patch {
                    state.decode(bytes, index as i64 * 166667)?
                } else {
                    state.decode_input(&fixed, index as i64 * 166667)?
                };
                ensure!(state.dxva_used, "decoder returned CPU samples");
                ensure!(
                    pictures.len() == 1 && pictures[0].0 == index as i64 * 166667,
                    "hardware decoder withheld or misidentified frame {index}"
                );
                check_picture(
                    &pictures[0].1,
                    reference.decode(bytes)?.context("reference picture")?,
                );
                calls.push(index);
            }
            eprintln!(
                "DXVA old-host viewer patch={patch}, low latency={:?}, zero-delay frames={calls:?}",
                state.low_latency()
            );
        }
        long_gop_delay(true)?;
        Ok(())
    }
    #[test]
    fn p010_is_reduced_to_sdr_without_reading_padding() {
        let layout = Layout {
            coded_width: 2,
            coded_height: 2,
            left: 0,
            top: 0,
            width: 2,
            height: 2,
            stride: 8,
            p010: true,
            ten_bit: false,
        };
        let mut bytes = vec![0; 24];
        for (offset, sample) in [
            (0, 64u16),
            (2, 940),
            (8, 64),
            (10, 940),
            (16, 512),
            (18, 512),
        ] {
            bytes[offset..offset + 2].copy_from_slice(&(sample << 6).to_le_bytes());
        }
        let frame = layout.copy(&bytes, 8, 2).unwrap();
        assert_eq!(frame.y, [16, 235, 16, 235]);
        assert_eq!(frame.uv, [128, 128]);
    }
    #[test]
    fn crop_preserves_visible_planes_and_ignores_padding() -> Result<()> {
        let layout = Layout {
            coded_width: 8,
            coded_height: 6,
            left: 2,
            top: 2,
            width: 4,
            height: 2,
            stride: 12,
            p010: false,
            ten_bit: false,
        };
        let bytes: Vec<u8> = (0..108).collect();
        let frame = layout.copy(&bytes, 12, 6)?;
        assert_eq!(frame.y, [26, 27, 28, 29, 38, 39, 40, 41]);
        assert_eq!(frame.uv, [86, 87, 88, 89]);
        assert_eq!(
            (frame.width, frame.height, frame.y_stride, frame.uv_stride),
            (4, 2, 4, 4)
        );
        assert!(layout.copy(&bytes[..80], 12, 6).is_err());
        Ok(())
    }
}

mod mf {
    //! Resolve MF lazily so Windows Server without the Media Foundation feature
    //! can still start Latch and report an unavailable codec instead of failing in
    //! the process loader. The system DLL is retained for the process lifetime.
    #![allow(non_snake_case)]
    use std::{ffi::c_void, sync::OnceLock};
    use windows::{
        core::{s, w, Error, Interface, HRESULT, PCSTR},
        Win32::{
            Foundation::{E_POINTER, HANDLE, HMODULE},
            Media::MediaFoundation::*,
            System::LibraryLoader::*,
        },
    };
    fn resolve(name: PCSTR) -> windows::core::Result<unsafe extern "system" fn() -> isize> {
        static MODULE: OnceLock<Result<usize, HRESULT>> = OnceLock::new();
        let module = MODULE
            .get_or_init(|| {
                // SAFETY: Load only the system copy; retain its reference while cached
                // function pointers and any MF COM objects can still exist.
                unsafe {
                    LoadLibraryExW(
                        w!("mfplat.dll"),
                        HANDLE::default(),
                        LOAD_LIBRARY_SEARCH_SYSTEM32,
                    )
                }
                .map(|m| m.0 as usize)
                .map_err(|e| e.code())
            })
            .as_ref()
            .map_err(|code| Error::from_hresult(*code))?;
        // SAFETY: The cached module retains its loader reference permanently.
        unsafe { GetProcAddress(HMODULE(*module as *mut c_void), name) }
            .ok_or_else(Error::from_win32)
    }
    macro_rules! bind {
        ($name:literal, $signature:ty) => {{
            static FUNCTION: OnceLock<Result<$signature, HRESULT>> = OnceLock::new();
            *FUNCTION
                .get_or_init(|| {
                    let pointer = resolve(s!($name)).map_err(|e| e.code())?;
                    // SAFETY: Each signature below is the exact Windows SDK ABI for
                    // the named export; resolve retains the module that owns it.
                    Ok(unsafe {
                        std::mem::transmute::<unsafe extern "system" fn() -> isize, $signature>(
                            pointer,
                        )
                    })
                })
                .as_ref()
                .map_err(|code| Error::from_hresult(*code))?
        }};
    }
    unsafe fn created<T: Interface>(
        call: impl FnOnce(*mut *mut c_void) -> HRESULT,
    ) -> windows::core::Result<T> {
        let mut pointer = std::ptr::null_mut();
        call(&mut pointer).ok()?;
        if pointer.is_null() {
            return Err(Error::from_hresult(E_POINTER));
        }
        // SAFETY: Successful COM creation transfers one owned reference.
        Ok(unsafe { T::from_raw(pointer) })
    }
    pub unsafe fn MFTEnumEx(
        category: windows::core::GUID,
        flags: MFT_ENUM_FLAG,
        input: *const MFT_REGISTER_TYPE_INFO,
        activates: *mut *mut Option<IMFActivate>,
        count: *mut u32,
    ) -> windows::core::Result<()> {
        let function = bind!(
            "MFTEnumEx",
            unsafe extern "system" fn(
                windows::core::GUID,
                u32,
                *const MFT_REGISTER_TYPE_INFO,
                *const MFT_REGISTER_TYPE_INFO,
                *mut *mut Option<IMFActivate>,
                *mut u32,
            ) -> HRESULT
        );
        function(
            category,
            flags.0 as u32,
            input,
            std::ptr::null(),
            activates,
            count,
        )
        .ok()
    }
    pub unsafe fn MFStartup(version: u32, flags: u32) -> windows::core::Result<()> {
        // SAFETY: Exact SDK signature; caller balances startup with shutdown.
        let function = bind!("MFStartup", unsafe extern "system" fn(u32, u32) -> HRESULT);
        // SAFETY: Caller owns the matching startup/shutdown scope.
        unsafe { function(version, flags).ok() }
    }
    pub unsafe fn MFShutdown() -> windows::core::Result<()> {
        // SAFETY: Caller owns a successful MF startup reference.
        let function = bind!("MFShutdown", unsafe extern "system" fn() -> HRESULT);
        // SAFETY: Caller owns a successful startup reference.
        unsafe { function().ok() }
    }
    pub unsafe fn MFCreateMediaType() -> windows::core::Result<IMFMediaType> {
        let function = bind!(
            "MFCreateMediaType",
            unsafe extern "system" fn(*mut *mut c_void) -> HRESULT
        );
        // SAFETY: Exact SDK creation signature, adopted as an owned interface.
        unsafe { created(|out| function(out)) }
    }
    pub unsafe fn MFCreateSample() -> windows::core::Result<IMFSample> {
        let function = bind!(
            "MFCreateSample",
            unsafe extern "system" fn(*mut *mut c_void) -> HRESULT
        );
        // SAFETY: Exact SDK creation signature, adopted as an owned interface.
        unsafe { created(|out| function(out)) }
    }
    pub unsafe fn MFCreateMemoryBuffer(length: u32) -> windows::core::Result<IMFMediaBuffer> {
        let function = bind!(
            "MFCreateMemoryBuffer",
            unsafe extern "system" fn(u32, *mut *mut c_void) -> HRESULT
        );
        // SAFETY: Exact SDK creation signature, adopted as an owned interface.
        unsafe { created(|out| function(length, out)) }
    }
    pub unsafe fn MFCreateAlignedMemoryBuffer(
        length: u32,
        alignment: u32,
    ) -> windows::core::Result<IMFMediaBuffer> {
        let function = bind!(
            "MFCreateAlignedMemoryBuffer",
            unsafe extern "system" fn(u32, u32, *mut *mut c_void) -> HRESULT
        );
        // SAFETY: Exact SDK creation signature, adopted as an owned interface.
        unsafe { created(|out| function(length, alignment, out)) }
    }
    pub unsafe fn MFCreateDXGIDeviceManager(
        token: *mut u32,
        manager: *mut Option<IMFDXGIDeviceManager>,
    ) -> windows::core::Result<()> {
        // SAFETY: Caller supplies writable SDK output slots.
        let function = bind!(
            "MFCreateDXGIDeviceManager",
            unsafe extern "system" fn(*mut u32, *mut Option<IMFDXGIDeviceManager>) -> HRESULT
        );
        // SAFETY: Caller supplies the SDK output pointers and optional filters.
        unsafe { function(token, manager).ok() }
    }
}

#[cfg(test)]
mod hdr_tests {
    use super::*;
    #[test]
    #[ignore = "requires HEVC decoder and LATCH_HDR_TEST_STREAM (one AMF HDR access unit)"]
    fn main10_decode_preserves_p010_luminance() -> Result<()> {
        let bytes = std::fs::read(std::env::var("LATCH_HDR_TEST_STREAM")?)?;
        let mut decoder = MfDecoder::with_stream(Codec::Hevc, 960, 540, 10)?;
        let frame = decoder.decode(&bytes)?.context("no Main10 picture")?;
        assert_eq!(frame.bit_depth, 10);
        assert_eq!((frame.width, frame.height), (960, 540));
        for (x, expected) in [(160, 326i32), (480, 508), (800, 723)] {
            let offset = 270 * frame.y_stride + x * 2;
            let code = u16::from_le_bytes([frame.y[offset], frame.y[offset + 1]]) >> 6;
            assert!(
                (i32::from(code) - expected).abs() < 5,
                "Main10 decoder luma {code}, expected {expected}"
            );
        }
        eprintln!(
            "Windows Main10 decoder: {}, P010 luma verified",
            decoder.name()
        );
        Ok(())
    }
}
