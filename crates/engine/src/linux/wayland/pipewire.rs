//! A PipeWire video consumer for the portal's screencast node.
//!
//! `libpipewire-0.3.so.0` is opened with `libloading` when a Wayland
//! session first needs it, as the GPU encoders are: nothing is linked and
//! no header is compiled, so a build machine needs no PipeWire, and a host
//! without it reports so instead of failing to start. The structures below
//! mirror PipeWire's and SPA's headers (pipewire/stream.h, spa/buffer/*.h,
//! spa/param/video/*.h); their sizes and offsets are checked at compile
//! time for the 64-bit targets Latch ships.
//!
//! The stream runs on PipeWire's own thread loop. Each `process` callback
//! takes the newest buffer, copies what its damage says changed into a
//! [`Desktop`], and gives every buffer straight back, so the producer is
//! never short of buffers while the encoder works.

use super::frame::{self, Desktop, Source};
use super::pod::{self, Negotiated, PixelFormat, Settle};
use crate::software::Rect;
use anyhow::{bail, Context};
use libloading::Library;
use parking_lot::{Condvar, Mutex};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::os::fd::{IntoRawFd, OwnedFd};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

#[repr(C)]
struct SpaHook {
    link: [*mut c_void; 2],
    funcs: *const c_void,
    data: *mut c_void,
    removed: *mut c_void,
    private: *mut c_void,
}

type Data = *mut c_void;

/// `struct pw_stream_events`, version 0 (PipeWire skips the later members
/// for a version-0 listener).
#[repr(C)]
struct StreamEvents {
    version: u32,
    destroy: Option<unsafe extern "C" fn(Data)>,
    state_changed: Option<unsafe extern "C" fn(Data, c_int, c_int, *const c_char)>,
    control_info: Option<unsafe extern "C" fn(Data, u32, *const c_void)>,
    io_changed: Option<unsafe extern "C" fn(Data, u32, *mut c_void, u32)>,
    param_changed: Option<unsafe extern "C" fn(Data, u32, *const u8)>,
    add_buffer: Option<unsafe extern "C" fn(Data, *mut PwBuffer)>,
    remove_buffer: Option<unsafe extern "C" fn(Data, *mut PwBuffer)>,
    process: Option<unsafe extern "C" fn(Data)>,
    drained: Option<unsafe extern "C" fn(Data)>,
}

/// The leading members of `struct pw_buffer`: only `buffer` is read.
#[repr(C)]
struct PwBuffer {
    buffer: *mut SpaBuffer,
    user_data: *mut c_void,
    size: u64,
}

#[repr(C)]
struct SpaBuffer {
    n_metas: u32,
    n_datas: u32,
    metas: *mut SpaMeta,
    datas: *mut SpaData,
}

#[repr(C)]
struct SpaMeta {
    kind: u32,
    size: u32,
    data: *mut c_void,
}

#[repr(C)]
struct SpaData {
    kind: u32,
    flags: u32,
    fd: i64,
    mapoffset: u32,
    maxsize: u32,
    data: *mut c_void,
    chunk: *mut SpaChunk,
}

#[repr(C)]
struct SpaChunk {
    offset: u32,
    size: u32,
    stride: i32,
    flags: i32,
}

#[repr(C)]
struct SpaMetaHeader {
    flags: u32,
    offset: u32,
    pts: i64,
    dts_offset: i64,
    seq: u64,
}

#[repr(C)]
struct SpaRegion {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<SpaHook>() == 48);
    assert!(size_of::<StreamEvents>() == 80);
    assert!(offset_of!(StreamEvents, param_changed) == 40);
    assert!(offset_of!(StreamEvents, process) == 64);
    assert!(size_of::<SpaBuffer>() == 24);
    assert!(size_of::<SpaMeta>() == 16);
    assert!(size_of::<SpaData>() == 40);
    assert!(offset_of!(SpaData, data) == 24);
    assert!(size_of::<SpaChunk>() == 16);
    assert!(size_of::<SpaMetaHeader>() == pod::META_HEADER_SIZE as usize);
    assert!(size_of::<SpaRegion>() == pod::META_REGION_SIZE as usize);
};

const STREAM_STATE_ERROR: c_int = -1;
const STREAM_STATE_UNCONNECTED: c_int = 0;
const STREAM_STATE_PAUSED: c_int = 2;
const STREAM_STATE_STREAMING: c_int = 3;
const DIRECTION_INPUT: c_int = 0;
const FLAG_AUTOCONNECT: c_int = 1 << 0;
const FLAG_MAP_BUFFERS: c_int = 1 << 2;
const META_HEADER_FLAG_DISCONT: u32 = 1 << 0;
const META_HEADER_FLAG_CORRUPTED: u32 = 1 << 1;
/// `SPA_CHUNK_FLAG_CORRUPTED` and `SPA_CHUNK_FLAG_EMPTY`.
const CHUNK_SKIP: i32 = (1 << 0) | (1 << 1);
/// `DMA_BUF_IOCTL_SYNC`, `_IOW('b', 0, struct dma_buf_sync)`.
const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x4008_6200;
const DMA_BUF_SYNC_READ: u64 = 1;
const DMA_BUF_SYNC_END: u64 = 1 << 2;

macro_rules! entry_points {
    ($($name:ident: fn($($arg:ty),*) $(-> $ret:ty)?;)*) => {
        #[allow(non_snake_case)]
        struct Api {
            _lib: Library,
            $($name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*
        }
        fn load() -> anyhow::Result<Api> {
            // SAFETY: loading libpipewire runs its constructors only; the
            // symbol types are those of the PipeWire 0.3/1.x headers.
            unsafe {
                let lib = Library::new("libpipewire-0.3.so.0")
                    .context("no libpipewire-0.3.so.0 (install PipeWire)")?;
                Ok(Api {
                    $($name: *lib.get(concat!(stringify!($name), "\0").as_bytes())?,)*
                    _lib: lib,
                })
            }
        }
    };
}

type Loop = *mut c_void;
type ThreadLoop = *mut c_void;
type PwContext = *mut c_void;
type Core = *mut c_void;
type Stream = *mut c_void;
type Properties = *mut c_void;

entry_points! {
    pw_init: fn(*mut c_int, *mut *mut *mut c_char);
    pw_get_library_version: fn() -> *const c_char;
    pw_properties_new_string: fn(*const c_char) -> Properties;
    pw_thread_loop_new: fn(*const c_char, *const c_void) -> ThreadLoop;
    pw_thread_loop_get_loop: fn(ThreadLoop) -> Loop;
    pw_thread_loop_start: fn(ThreadLoop) -> c_int;
    pw_thread_loop_stop: fn(ThreadLoop);
    pw_thread_loop_destroy: fn(ThreadLoop);
    pw_thread_loop_lock: fn(ThreadLoop);
    pw_thread_loop_unlock: fn(ThreadLoop);
    pw_context_new: fn(Loop, Properties, usize) -> PwContext;
    pw_context_destroy: fn(PwContext);
    pw_context_connect_fd: fn(PwContext, c_int, Properties, usize) -> Core;
    pw_core_disconnect: fn(Core) -> c_int;
    pw_stream_new: fn(Core, *const c_char, Properties) -> Stream;
    pw_stream_destroy: fn(Stream);
    pw_stream_add_listener: fn(Stream, *mut SpaHook, *const StreamEvents, Data);
    pw_stream_connect: fn(Stream, c_int, u32, c_int, *mut *const u8, u32) -> c_int;
    pw_stream_trigger_process: fn(Stream) -> c_int;
    pw_stream_disconnect: fn(Stream) -> c_int;
    pw_stream_dequeue_buffer: fn(Stream) -> *mut PwBuffer;
    pw_stream_queue_buffer: fn(Stream, *mut PwBuffer) -> c_int;
    pw_stream_update_params: fn(Stream, *mut *const u8, u32) -> c_int;
}

fn api() -> anyhow::Result<&'static Api> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| {
        let api = load().map_err(|e| format!("{e:#}"))?;
        // SAFETY: pw_init accepts null arguments and may be called more
        // than once.
        unsafe { (api.pw_init)(std::ptr::null_mut(), std::ptr::null_mut()) };
        // SAFETY: a static NUL-terminated string.
        let version = unsafe { CStr::from_ptr((api.pw_get_library_version)()) };
        tracing::info!("PipeWire {}", version.to_string_lossy());
        Ok(api)
    })
    .as_ref()
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Whether DMA-BUFs are offered: `$LATCH_WAYLAND_DMABUF=0` asks for
/// shared memory only.
fn dmabuf_wanted() -> bool {
    !std::env::var("LATCH_WAYLAND_DMABUF").is_ok_and(|v| v.trim() == "0")
}

#[derive(Default)]
struct State {
    desktop: Desktop,
    format: Option<Negotiated>,
    error: Option<String>,
    streaming: bool,
    paused: Option<Instant>,
    transform: u32,
    /// A DMA-BUF that could not be mapped: offer shared memory only.
    dmabuf_failed: bool,
    frames: u64,
    full_frame: bool,
    sequence: Option<u64>,
}

struct Shared {
    api: &'static Api,
    stream: Mutex<Stream>,
    state: Mutex<State>,
    changed: Condvar,
    fps: u32,
}

// SAFETY: the raw stream pointer is used on the PipeWire thread (in its
// callbacks) and under the thread-loop lock elsewhere.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

/// A connected screencast stream. Dropping it disconnects.
pub struct Consumer {
    api: &'static Api,
    thread: ThreadLoop,
    context: PwContext,
    core: Core,
    stream: Stream,
    _hook: Box<SpaHook>,
    _events: Box<StreamEvents>,
    shared: Arc<Shared>,
}

// SAFETY: as for `Shared`; the handles are touched only under the loop lock.
unsafe impl Send for Consumer {}

fn offers(dmabuf: bool, fps: u32) -> Vec<pod::Pod> {
    let mut pods = Vec::new();
    if dmabuf {
        pods.push(pod::enum_format(Some(&[pod::MODIFIER_LINEAR]), fps).build());
    }
    pods.push(pod::enum_format(None, fps).build());
    pods
}

fn update(api: &Api, stream: Stream, pods: &[pod::Pod]) {
    let mut ptrs: Vec<*const u8> = pods.iter().map(|p| p.as_ptr()).collect();
    // SAFETY: called on the PipeWire thread or under its lock; the PODs
    // outlive the call, which copies them.
    let result =
        unsafe { (api.pw_stream_update_params)(stream, ptrs.as_mut_ptr(), ptrs.len() as u32) };
    if result < 0 {
        tracing::warn!("PipeWire refused the stream parameters ({result})");
    }
}

unsafe extern "C" fn on_state(data: Data, _old: c_int, state: c_int, error: *const c_char) {
    // SAFETY: `data` is the `Shared` the listener was added with.
    let shared = unsafe { &*(data as *const Shared) };
    let mut s = shared.state.lock();
    match state {
        STREAM_STATE_ERROR => {
            let why = if error.is_null() {
                "unknown error".into()
            } else {
                // SAFETY: PipeWire passes a NUL-terminated message.
                unsafe { CStr::from_ptr(error) }
                    .to_string_lossy()
                    .into_owned()
            };
            s.error = Some(format!("the screencast stream failed: {why}"));
        }
        STREAM_STATE_UNCONNECTED if s.streaming => {
            s.error =
                Some("the screencast stream ended (sharing was stopped on this machine)".into());
        }
        STREAM_STATE_PAUSED if s.streaming => s.paused = Some(Instant::now()),
        STREAM_STATE_STREAMING => {
            s.streaming = true;
            s.paused = None;
        }
        _ => {}
    }
    shared.changed.notify_all();
}

unsafe extern "C" fn on_param(data: Data, id: u32, param: *const u8) {
    // SAFETY: as in `on_state`.
    let shared = unsafe { &*(data as *const Shared) };
    if id != pod::PARAM_FORMAT || param.is_null() {
        return;
    }
    // SAFETY: a POD starts with its body size; the body follows.
    let bytes = unsafe {
        let size = (param as *const u32).read_unaligned() as usize;
        std::slice::from_raw_parts(param, size + 8)
    };
    let stream = *shared.stream.lock();
    let settled = pod::Object::parse(bytes).and_then(|object| pod::settle(&object));
    match settled {
        Ok(Settle::Use(format)) => {
            tracing::info!(
                "screencast format {:?} {} × {}{}",
                format.format,
                format.width,
                format.height,
                if format.modifier.is_some() {
                    " in DMA-BUFs"
                } else {
                    " in shared memory"
                }
            );
            {
                let mut state = shared.state.lock();
                state.format = Some(format);
                state.full_frame = true;
            }
            let [header, damage, transform] = pod::metas();
            update(
                shared.api,
                stream,
                &[
                    pod::buffers(&format).build(),
                    header.build(),
                    damage.build(),
                    transform.build(),
                ],
            );
        }
        Ok(Settle::Fixate { linear }) => {
            update(shared.api, stream, &offers(linear, shared.fps));
        }
        Err(e) => {
            let mut s = shared.state.lock();
            s.error = Some(format!("{e:#}"));
            shared.changed.notify_all();
        }
    }
}

/// Our packed linear RGB formats have exactly one plane. DMA-BUF exporters
/// may leave maxsize zero and use a sentinel chunk size (xdpw 0.7.1).
fn mapping_layout(
    d: &SpaData,
    chunk: &SpaChunk,
    format: &Negotiated,
) -> anyhow::Result<(usize, usize, usize)> {
    anyhow::ensure!(chunk.stride >= 0, "negative screencast stride");
    let stride = if chunk.stride == 0 {
        format.width as usize * 4
    } else {
        chunk.stride as usize
    };
    anyhow::ensure!(
        stride >= format.width as usize * 4,
        "short screencast stride"
    );
    let offset = chunk.offset as usize;
    let len = if d.kind == pod::DATA_DMA_BUF {
        stride
            .checked_mul(format.height as usize)
            .context("DMA-BUF size overflow")?
    } else {
        anyhow::ensure!(
            offset <= d.maxsize as usize,
            "screencast offset outside buffer"
        );
        (chunk.size as usize).min(d.maxsize as usize - offset)
    };
    let size = (d.mapoffset as usize)
        .checked_add(offset)
        .and_then(|n| n.checked_add(len))
        .context("screencast mapping size overflow")?;
    Ok((offset, len, size))
}

/// The pixels of one data plane, mapped for reading; unmapped on drop.
struct Mapped {
    ptr: *const u8,
    len: usize,
    /// An mmap this made (DMA-BUFs), and the descriptor to end CPU access on.
    owned: Option<(*mut c_void, usize, c_int)>,
}

impl Mapped {
    /// # Safety
    /// `d` is a live buffer's data plane, dequeued from the stream.
    unsafe fn new(d: &SpaData, format: &Negotiated) -> anyhow::Result<Self> {
        let chunk = unsafe { d.chunk.as_ref() }.context("buffer without a chunk")?;
        let (offset, len, size) = mapping_layout(d, chunk, format)?;
        if !d.data.is_null() {
            return Ok(Self {
                ptr: unsafe { (d.data as *const u8).add(offset) },
                len,
                owned: None,
            });
        }
        if d.kind != pod::DATA_DMA_BUF {
            bail!("a screencast buffer of type {} is not mapped", d.kind);
        }
        let fd = d.fd as c_int;
        // SAFETY: a read-only shared mapping of the producer's DMA-BUF.
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if map == libc::MAP_FAILED {
            bail!(
                "cannot map the screencast DMA-BUF: {}",
                std::io::Error::last_os_error()
            );
        }
        let sync = DMA_BUF_SYNC_READ;
        // SAFETY: DMA_BUF_IOCTL_SYNC takes a u64; failure only means the
        // exporter does not need it.
        unsafe { libc::ioctl(fd, DMA_BUF_IOCTL_SYNC, &sync) };
        Ok(Self {
            ptr: unsafe { (map as *const u8).add(d.mapoffset as usize + offset) },
            len,
            owned: Some((map, size, fd)),
        })
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: mapped for `len` bytes for as long as `self` lives.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        if let Some((map, size, fd)) = self.owned {
            let sync = DMA_BUF_SYNC_READ | DMA_BUF_SYNC_END;
            // SAFETY: the mapping and descriptor made in `new`.
            unsafe {
                libc::ioctl(fd, DMA_BUF_IOCTL_SYNC, &sync);
                libc::munmap(map, size);
            }
        }
    }
}

/// A buffer's damage in buffer pixels: `None` (everything) without the
/// metadata, or when it lists nothing.
///
/// # Safety
/// `buffer` is live and dequeued.
unsafe fn damage(buffer: &SpaBuffer, width: u32, height: u32) -> Option<Vec<Rect>> {
    if buffer.n_metas == 0 || buffer.metas.is_null() {
        return None;
    }
    let metas = unsafe { std::slice::from_raw_parts(buffer.metas, buffer.n_metas as usize) };
    let meta = metas
        .iter()
        .find(|m| m.kind == pod::META_VIDEO_DAMAGE && !m.data.is_null())?;
    let count = meta.size as usize / std::mem::size_of::<SpaRegion>();
    let regions = unsafe { std::slice::from_raw_parts(meta.data as *const SpaRegion, count) };
    let regions: Vec<_> = regions
        .iter()
        .take_while(|r| r.width > 0 && r.height > 0)
        .map(|r| (r.x, r.y, r.width, r.height))
        .collect();
    frame::clip(&regions, width, height)
}

/// # Safety
/// `buffer` is live and dequeued.
unsafe fn header(buffer: &SpaBuffer) -> Option<&SpaMetaHeader> {
    if buffer.n_metas == 0 || buffer.metas.is_null() {
        return None;
    }
    let metas = unsafe { std::slice::from_raw_parts(buffer.metas, buffer.n_metas as usize) };
    metas
        .iter()
        .find(|m| {
            m.kind == pod::META_HEADER
                && !m.data.is_null()
                && m.size as usize >= std::mem::size_of::<SpaMetaHeader>()
        })
        .map(|m| unsafe { &*(m.data as *const SpaMetaHeader) })
}

/// # Safety
/// `buffer` is live and dequeued.
unsafe fn video_transform(buffer: &SpaBuffer) -> u32 {
    if buffer.n_metas == 0 || buffer.metas.is_null() {
        return 0;
    }
    let metas = unsafe { std::slice::from_raw_parts(buffer.metas, buffer.n_metas as usize) };
    metas
        .iter()
        .find(|m| m.kind == pod::META_VIDEO_TRANSFORM && m.size >= 4 && !m.data.is_null())
        .map_or(0, |m| unsafe { (m.data as *const u32).read_unaligned() })
}

impl State {
    fn check(&self) -> anyhow::Result<()> {
        if let Some(e) = &self.error {
            return Err(crate::video::CaptureEnded(e.clone()).into());
        }
        if self
            .paused
            .is_some_and(|at| at.elapsed() >= Duration::from_secs(2))
        {
            return Err(crate::video::CaptureEnded(
                "the screencast producer stopped sharing this screen".into(),
            )
            .into());
        }
        Ok(())
    }

    fn note_header(&mut self, header: Option<&SpaMetaHeader>) {
        if let Some(header) = header {
            self.full_frame |= header.flags & META_HEADER_FLAG_DISCONT != 0
                || self
                    .sequence
                    .is_some_and(|previous| header.seq != previous.wrapping_add(1));
        }
        self.sequence = header.map(|h| h.seq);
    }
}

unsafe extern "C" fn on_process(data: Data) {
    // SAFETY: as in `on_state`.
    let shared = unsafe { &*(data as *const Shared) };
    let api = shared.api;
    let stream = *shared.stream.lock();
    // The newest buffer has the picture; every buffer's damage counts, as
    // each is relative to the one before it.
    let mut newest: *mut PwBuffer = std::ptr::null_mut();
    let mut changed: Option<Vec<Rect>> = Some(Vec::new());
    let format = shared.state.lock().format;
    let Some(format) = format else {
        return;
    };
    loop {
        // SAFETY: on the PipeWire thread, inside its process callback.
        let b = unsafe { (api.pw_stream_dequeue_buffer)(stream) };
        if b.is_null() {
            break;
        }
        // SAFETY: dequeued buffers are live until queued again.
        let spa = unsafe { (*b).buffer.as_ref() };
        let usable = spa.is_some_and(|spa| {
            spa.n_datas == 1
                && !spa.datas.is_null()
                && !unsafe { header(spa) }
                    .is_some_and(|h| h.flags & META_HEADER_FLAG_CORRUPTED != 0)
                && unsafe { (*spa.datas).chunk.as_ref() }
                    .is_some_and(|c| c.size > 0 && c.flags & CHUNK_SKIP == 0)
        });
        if !usable {
            shared.state.lock().full_frame = true;
            unsafe { (api.pw_stream_queue_buffer)(stream, b) };
            continue;
        }
        let spa = spa.expect("usable");
        shared.state.lock().note_header(unsafe { header(spa) });
        frame::merge(
            &mut changed,
            unsafe { damage(spa, format.width, format.height) }.as_deref(),
        );
        if !newest.is_null() {
            unsafe { (api.pw_stream_queue_buffer)(stream, newest) };
        }
        newest = b;
    }
    if newest.is_null() {
        return;
    }
    // SAFETY: `newest` is dequeued and usable (checked above).
    let result = unsafe {
        let spa = &*(*newest).buffer;
        let plane = &*spa.datas;
        Mapped::new(plane, &format).and_then(|mapped| {
            let chunk = &*plane.chunk;
            anyhow::ensure!(chunk.stride >= 0, "negative screencast stride");
            let stride = if chunk.stride == 0 {
                format.width as usize * 4
            } else {
                chunk.stride as usize
            };
            let source = Source {
                data: mapped.bytes(),
                stride,
                width: format.width,
                height: format.height,
                format: format.format,
            };
            let mut s = shared.state.lock();
            let transform = video_transform(spa);
            let damage = if s.full_frame || transform != s.transform {
                None
            } else {
                changed.as_deref()
            };
            s.desktop.put_transformed(&source, damage, transform)?;
            s.transform = transform;
            s.full_frame = false;
            s.frames += 1;
            Ok(())
        })
    };
    unsafe { (api.pw_stream_queue_buffer)(stream, newest) };
    match result {
        Ok(()) => {}
        Err(e) if format.modifier.is_some() => {
            let mut s = shared.state.lock();
            if !s.dmabuf_failed {
                tracing::warn!("{e:#}; asking for shared memory instead");
                s.dmabuf_failed = true;
                drop(s);
                update(api, stream, &offers(false, shared.fps));
            }
        }
        Err(e) => {
            shared.state.lock().error = Some(format!("{e:#}"));
        }
    }
    shared.changed.notify_all();
}

impl Consumer {
    /// Connects to `node` through the portal's PipeWire `remote`, and
    /// waits up to `first` for its first picture.
    pub fn connect(remote: OwnedFd, node: u32, fps: u32, first: Duration) -> anyhow::Result<Self> {
        let api = api()?;
        let name = CString::new("latch-screencast")?;
        let props = CString::new("media.type=Video media.category=Capture media.role=Screen")?;
        // SAFETY: the calls follow PipeWire's thread-loop rules: objects are
        // made, connected and destroyed with the loop locked, and the
        // listener's data outlives the stream.
        unsafe {
            let thread = (api.pw_thread_loop_new)(name.as_ptr(), std::ptr::null());
            if thread.is_null() {
                bail!("could not make a PipeWire thread loop");
            }
            let context = (api.pw_context_new)(
                (api.pw_thread_loop_get_loop)(thread),
                std::ptr::null_mut(),
                0,
            );
            if context.is_null() {
                (api.pw_thread_loop_destroy)(thread);
                bail!("could not make a PipeWire context");
            }
            if (api.pw_thread_loop_start)(thread) < 0 {
                (api.pw_context_destroy)(context);
                (api.pw_thread_loop_destroy)(thread);
                bail!("could not start the PipeWire thread");
            }
            (api.pw_thread_loop_lock)(thread);
            let core =
                (api.pw_context_connect_fd)(context, remote.into_raw_fd(), std::ptr::null_mut(), 0);
            let shared = Arc::new(Shared {
                api,
                stream: Mutex::new(std::ptr::null_mut()),
                state: Mutex::new(State::default()),
                changed: Condvar::new(),
                fps,
            });
            let mut this = Consumer {
                api,
                thread,
                context,
                core,
                stream: std::ptr::null_mut(),
                _hook: Box::new(std::mem::zeroed()),
                _events: Box::new(StreamEvents {
                    version: 0,
                    destroy: None,
                    state_changed: Some(on_state),
                    control_info: None,
                    io_changed: None,
                    param_changed: Some(on_param),
                    add_buffer: None,
                    remove_buffer: None,
                    process: Some(on_process),
                    drained: None,
                }),
                shared: shared.clone(),
            };
            if core.is_null() {
                (api.pw_thread_loop_unlock)(thread);
                bail!(
                    "could not connect to the portal's PipeWire: {}",
                    std::io::Error::last_os_error()
                );
            }
            let stream = (api.pw_stream_new)(
                core,
                name.as_ptr(),
                (api.pw_properties_new_string)(props.as_ptr()),
            );
            if stream.is_null() {
                (api.pw_thread_loop_unlock)(thread);
                bail!("could not make a PipeWire stream");
            }
            this.stream = stream;
            *shared.stream.lock() = stream;
            (api.pw_stream_add_listener)(
                stream,
                &mut *this._hook,
                &*this._events,
                Arc::as_ptr(&shared) as Data,
            );
            let pods = offers(dmabuf_wanted(), fps);
            let mut ptrs: Vec<*const u8> = pods.iter().map(|p| p.as_ptr()).collect();
            let connected = (api.pw_stream_connect)(
                stream,
                DIRECTION_INPUT,
                node,
                FLAG_AUTOCONNECT | FLAG_MAP_BUFFERS,
                ptrs.as_mut_ptr(),
                ptrs.len() as u32,
            );
            (api.pw_thread_loop_unlock)(thread);
            if connected < 0 {
                bail!("could not connect to screencast node {node} ({connected})");
            }
            let deadline = Instant::now() + first;
            let mut s = shared.state.lock();
            while s.frames == 0 && s.error.is_none() {
                // Request a graph cycle after the driver has been assigned.
                // An idle xdpw producer may have queued its sole initial frame
                // before PipeWire's implicit trigger could drive the graph.
                drop(s);
                (api.pw_thread_loop_lock)(thread);
                (api.pw_stream_trigger_process)(stream);
                (api.pw_thread_loop_unlock)(thread);
                s = shared.state.lock();
                if s.frames > 0 || s.error.is_some() {
                    break;
                }
                let wake = deadline.min(Instant::now() + Duration::from_millis(100));
                shared.changed.wait_until(&mut s, wake);
                if Instant::now() >= deadline && s.frames == 0 {
                    bail!(
                        "the screencast sent no picture within {first:?}{}",
                        if s.format.is_none() {
                            " (no format was agreed)"
                        } else {
                            ""
                        }
                    );
                }
            }
            if let Some(e) = s.error.clone() {
                bail!(e);
            }
            drop(s);
            Ok(this)
        }
    }

    /// The size of the latest picture.
    pub fn size(&self) -> (u32, u32) {
        let s = self.shared.state.lock();
        (s.desktop.picture.width, s.desktop.picture.height)
    }

    /// Waits up to `timeout` for a new picture and copies it into `frame`.
    /// `Some(damage)` when there was one (`damage` as in [`Desktop::take`]).
    pub fn take(
        &self,
        frame: &mut crate::software::Bgra,
        timeout: Duration,
        first: bool,
    ) -> anyhow::Result<Option<Option<Vec<Rect>>>> {
        let deadline = Instant::now() + timeout;
        let mut s = self.shared.state.lock();
        loop {
            s.check()?;
            if first || s.desktop.fresh {
                return Ok(Some(s.desktop.take(frame)));
            }
            if timeout.is_zero() || self.shared.changed.wait_until(&mut s, deadline).timed_out() {
                return Ok(None);
            }
        }
    }

    pub fn format(&self) -> Option<PixelFormat> {
        self.shared.state.lock().format.map(|f| f.format)
    }
}

impl Drop for Consumer {
    fn drop(&mut self) {
        let api = self.api;
        // SAFETY: the reverse of `connect`, with the loop locked while the
        // stream and core go; the listener data (`shared`) is still alive.
        unsafe {
            (api.pw_thread_loop_lock)(self.thread);
            if !self.stream.is_null() {
                (api.pw_stream_disconnect)(self.stream);
                (api.pw_stream_destroy)(self.stream);
                *self.shared.stream.lock() = std::ptr::null_mut();
            }
            if !self.core.is_null() {
                (api.pw_core_disconnect)(self.core);
            }
            (api.pw_thread_loop_unlock)(self.thread);
            (api.pw_thread_loop_stop)(self.thread);
            (api.pw_context_destroy)(self.context);
            (api.pw_thread_loop_destroy)(self.thread);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn producer_removal_ends_capture_after_transient_pause_grace() {
        let mut state = State {
            streaming: true,
            paused: Some(Instant::now()),
            ..Default::default()
        };
        assert!(
            state.check().is_ok(),
            "a format renegotiation may pause briefly"
        );
        state.paused = Some(Instant::now() - Duration::from_secs(3));
        assert!(state
            .check()
            .unwrap_err()
            .is::<crate::video::CaptureEnded>());
        state.paused = None;
        assert!(state.check().is_ok(), "streaming resumed");
        state.error = Some("producer disconnected".into());
        assert!(state
            .check()
            .unwrap_err()
            .is::<crate::video::CaptureEnded>());
    }

    #[test]
    fn wlroots_zero_maxsize_dmabuf_maps_its_packed_plane() {
        use std::io::{Seek, SeekFrom, Write};
        use std::os::fd::AsRawFd;
        let path = std::env::temp_dir().join(format!("latch-dmabuf-{}", rand::random::<u64>()));
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(path).unwrap();
        file.set_len(128).unwrap();
        file.seek(SeekFrom::Start(16)).unwrap();
        file.write_all(&[0x5a; 64]).unwrap();
        let format = Negotiated {
            format: PixelFormat::Bgrx,
            width: 3,
            height: 4,
            modifier: Some(0),
        };
        let mut chunk = SpaChunk {
            offset: 8,
            size: u32::MAX,
            stride: 16,
            flags: 0,
        };
        let data = SpaData {
            kind: pod::DATA_DMA_BUF,
            flags: 0,
            fd: i64::from(file.as_raw_fd()),
            mapoffset: 8,
            maxsize: 0,
            data: std::ptr::null_mut(),
            chunk: &mut chunk,
        };
        assert_eq!(mapping_layout(&data, &chunk, &format).unwrap(), (8, 64, 80));
        // A regular shared descriptor exercises mmap offsets and length. Real
        // exporters additionally implement DMA_BUF_IOCTL_SYNC.
        let mapped = unsafe { Mapped::new(&data, &format) }.unwrap();
        assert_eq!(mapped.bytes(), &[0x5a; 64]);
    }

    #[test]
    fn transformed_metadata_has_the_spa_type_and_layout() {
        let mut transform = 1u32;
        let mut meta = SpaMeta {
            kind: pod::META_VIDEO_TRANSFORM,
            size: 4,
            data: (&mut transform as *mut u32).cast(),
        };
        let buffer = SpaBuffer {
            n_metas: 1,
            n_datas: 0,
            metas: &mut meta,
            datas: std::ptr::null_mut(),
        };
        assert_eq!(unsafe { video_transform(&buffer) }, 1);
        assert_eq!(pod::META_VIDEO_TRANSFORM, 8);
        assert_eq!(pod::metas().len(), 3);
    }

    #[test]
    fn absent_metadata_and_header_flags_follow_the_spa_abi() {
        let mut buffer = SpaBuffer {
            n_metas: 0,
            n_datas: 0,
            metas: std::ptr::null_mut(),
            datas: std::ptr::null_mut(),
        };
        // SAFETY: the empty buffer has no metadata to dereference.
        unsafe {
            assert_eq!(damage(&buffer, 10, 10), None);
            assert!(header(&buffer).is_none());
        }
        let mut meta_header = SpaMetaHeader {
            flags: 1,
            offset: 0,
            pts: 0,
            dts_offset: 0,
            seq: 0,
        };
        let mut meta = SpaMeta {
            kind: pod::META_HEADER,
            size: std::mem::size_of_val(&meta_header) as u32,
            data: (&mut meta_header as *mut SpaMetaHeader).cast(),
        };
        buffer.n_metas = 1;
        buffer.metas = &mut meta;
        // SAFETY: metadata points to the live header above.
        unsafe {
            assert_eq!(header(&buffer).unwrap().flags, META_HEADER_FLAG_DISCONT);
        }
        // Mutate through the live metadata pointer, as a producer does.
        unsafe {
            (*(meta.data as *mut SpaMetaHeader)).flags = 2;
        }
        unsafe {
            assert_eq!(header(&buffer).unwrap().flags, META_HEADER_FLAG_CORRUPTED);
        }
    }
    #[test]
    fn discontinuities_and_skipped_sequences_require_a_full_copy() {
        let mut state = State::default();
        let mut header = SpaMetaHeader {
            flags: 0,
            offset: 0,
            pts: 0,
            dts_offset: 0,
            seq: 10,
        };
        state.note_header(Some(&header));
        header.seq = 11;
        state.note_header(Some(&header));
        assert!(!state.full_frame);
        header.seq = 13;
        state.note_header(Some(&header));
        assert!(state.full_frame);
        state.full_frame = false;
        header.seq = 14;
        header.flags = META_HEADER_FLAG_DISCONT;
        state.note_header(Some(&header));
        assert!(state.full_frame);
    }
}
