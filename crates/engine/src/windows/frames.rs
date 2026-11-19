//! Screen frames from the SYSTEM helper, for while the host cannot
//! duplicate its own desktop: the lock screen, the sign-in screen and
//! administrator prompts are on the secure desktop, which only SYSTEM may
//! capture.
//!
//! The host asks over its own pipe connection (see `elevated`) for one
//! output, named by its adapter's LUID and its GDI device name, and gets
//! handles to an unnamed file mapping and an auto-reset event, duplicated
//! into its process by the helper. Nothing is reachable by name, so no
//! other process can open or plant either object; their security grants
//! SYSTEM and the one user, nobody else.
//!
//! The mapping is a [`HEADER`]-byte header and two BGRA slots. The helper
//! writes a changed frame (cursor composited, at most [`MAX_FPS`] a
//! second) into the slot the host is not reading, publishes its number and
//! sets the event. Each slot carries a sequence (odd while it is written),
//! so a reader that was overtaken sees it and drops that copy.
use super::capture::{Capture, Target};
use super::pipe::Pipe;
use crate::software::{Bgra, CpuCapture};
use anyhow::{bail, ensure, Context, Result};
use std::{
    io::{Read, Write},
    sync::atomic::{fence, AtomicU32, AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};
use windows::{
    core::{HSTRING, PCWSTR},
    Win32::{
        Foundation::{
            CloseHandle, DuplicateHandle, LocalFree, DUPLICATE_CLOSE_SOURCE,
            DUPLICATE_HANDLE_OPTIONS, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
        },
        Security::{
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
            },
            PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
        },
        System::{
            Memory::{
                CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_READ, FILE_MAP_WRITE,
                MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
            },
            Pipes::PeekNamedPipe,
            Threading::{CreateEventW, GetCurrentProcess, SetEvent, WaitForSingleObject},
        },
    },
};

/// Bytes before the first slot.
pub(super) const HEADER: usize = 64;
const MAGIC: u32 = u32::from_le_bytes(*b"LFRM");
const VERSION: u32 = 1;
/// The helper sends at most this many frames a second.
pub(super) const MAX_FPS: u32 = 30;
/// The largest output the helper shares (8K, with room for a rotated one).
const MAX_SIDE: u32 = 8192;
/// SYNCHRONIZE: all the host may do with the event.
const SYNCHRONIZE: u32 = 0x0010_0000;

/// What the header says the helper is doing.
pub(super) mod state {
    pub const RUNNING: u32 = 1;
    /// The output's size changed: ask again for a mapping of the new size.
    pub const RESIZED: u32 = 2;
    pub const FAILED: u32 = 3;
}

/// The helper's output changed size and ended this mapping: ask again for
/// one of the new size, rather than giving up on the helper.
#[derive(Debug)]
pub(super) struct Resized;
impl std::fmt::Display for Resized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the output changed size")
    }
}
impl std::error::Error for Resized {}

#[repr(C)]
struct Header {
    magic: AtomicU32,
    version: AtomicU32,
    state: AtomicU32,
    slots: AtomicU32,
    width: AtomicU32,
    height: AtomicU32,
    stride: AtomicU32,
    _pad: AtomicU32,
    /// The number of the last whole frame; it is in slot `published % 2`.
    published: AtomicU64,
    /// Per slot: `2n + 1` while frame `n` is written into it, `2n` after.
    seq: [AtomicU64; 2],
    _reserved: AtomicU64,
}
const _: () = assert!(std::mem::size_of::<Header>() == HEADER);

/// The mapping's size for a `width` by `height` output.
pub(super) fn mapping_size(width: u32, height: u32) -> Option<usize> {
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return None;
    }
    let slot = (width as usize)
        .checked_mul(4)?
        .checked_mul(height as usize)?;
    HEADER.checked_add(slot.checked_mul(2)?)
}

/// The shared region: header, then two slots of `stride * height` bytes.
struct Region {
    ptr: *mut u8,
    len: usize,
}
impl Region {
    /// # Safety
    /// `ptr` must be valid for reads (and, for a writer, writes) of `len`
    /// bytes, 8-byte aligned, for as long as the region is used.
    unsafe fn new(ptr: *mut u8, len: usize) -> Result<Self> {
        ensure!(!ptr.is_null() && len >= HEADER, "frame mapping too small");
        ensure!(ptr as usize % 8 == 0, "frame mapping misaligned");
        Ok(Self { ptr, len })
    }
    fn header(&self) -> &Header {
        // SAFETY: new() checked the size and alignment; every field is atomic.
        unsafe { &*self.ptr.cast::<Header>() }
    }
    /// Slot `slot`'s first byte, after checking it lies inside the region.
    fn slot(&self, slot: usize, stride: usize, height: usize) -> Result<*mut u8> {
        let size = stride.checked_mul(height).context("frame size")?;
        let end = HEADER
            .checked_add(size.checked_mul(slot + 1).context("frame size")?)
            .context("frame size")?;
        ensure!(
            slot < 2 && end <= self.len,
            "frame slot outside the mapping"
        );
        // SAFETY: The offset is inside the region, checked above.
        Ok(unsafe { self.ptr.add(HEADER + size * slot) })
    }
}

/// The helper's end of the region.
pub(super) struct Writer {
    region: Region,
    width: u32,
    height: u32,
    next: u64,
}
// SAFETY: The region is owned memory of this process's mapping view.
unsafe impl Send for Writer {}
impl Writer {
    /// # Safety
    /// As [`Region::new`], for reads and writes.
    pub(super) unsafe fn init(ptr: *mut u8, len: usize, width: u32, height: u32) -> Result<Self> {
        let region = Region::new(ptr, len)?;
        let size = mapping_size(width, height).context("output size out of range")?;
        ensure!(size <= len, "frame mapping too small for the output");
        let h = region.header();
        h.version.store(VERSION, Ordering::Relaxed);
        h.state.store(state::RUNNING, Ordering::Relaxed);
        h.slots.store(2, Ordering::Relaxed);
        h.width.store(width, Ordering::Relaxed);
        h.height.store(height, Ordering::Relaxed);
        h.stride.store(width * 4, Ordering::Relaxed);
        h.published.store(0, Ordering::Relaxed);
        h.seq[0].store(0, Ordering::Relaxed);
        h.seq[1].store(0, Ordering::Relaxed);
        h.magic.store(MAGIC, Ordering::Release);
        Ok(Self {
            region,
            width,
            height,
            next: 1,
        })
    }
    /// Copies a frame of rows `src_stride` bytes apart into the free slot
    /// and publishes it.
    pub(super) fn publish(&mut self, src: &[u8], src_stride: usize) -> Result<()> {
        let row = self.width as usize * 4;
        let rows = self.height as usize;
        ensure!(
            src_stride >= row && src.len() >= src_stride * (rows - 1) + row,
            "frame smaller than the output"
        );
        let n = self.next;
        let slot = (n % 2) as usize;
        let dst = self.region.slot(slot, row, rows)?;
        let h = self.region.header();
        h.seq[slot].store(2 * n + 1, Ordering::Relaxed);
        fence(Ordering::Release);
        for y in 0..rows {
            // SAFETY: Both rows are in bounds, checked above and by slot().
            unsafe {
                std::ptr::copy_nonoverlapping(
                    src.as_ptr().add(y * src_stride),
                    dst.add(y * row),
                    row,
                );
            }
        }
        h.seq[slot].store(2 * n, Ordering::Release);
        h.published.store(n, Ordering::Release);
        self.next += 1;
        Ok(())
    }
    pub(super) fn stop(&self, why: u32) {
        self.region.header().state.store(why, Ordering::Release);
    }
}

/// The host's end of the region.
pub(super) struct Reader {
    region: Region,
    pub width: u32,
    pub height: u32,
    stride: usize,
    seen: u64,
}
// SAFETY: The region is a mapping view this process owns for its lifetime.
unsafe impl Send for Reader {}
impl Reader {
    /// # Safety
    /// As [`Region::new`], for reads.
    pub(super) unsafe fn open(ptr: *mut u8, len: usize) -> Result<Self> {
        let region = Region::new(ptr, len)?;
        let h = region.header();
        ensure!(
            h.magic.load(Ordering::Acquire) == MAGIC,
            "not a frame mapping"
        );
        ensure!(
            h.version.load(Ordering::Relaxed) == VERSION,
            "frame mapping version"
        );
        let (width, height) = (
            h.width.load(Ordering::Relaxed),
            h.height.load(Ordering::Relaxed),
        );
        let stride = h.stride.load(Ordering::Relaxed) as usize;
        let size = mapping_size(width, height).context("frame size out of range")?;
        ensure!(
            stride == width as usize * 4 && size <= len,
            "frame mapping smaller than its frames"
        );
        Ok(Self {
            region,
            width,
            height,
            stride,
            seen: 0,
        })
    }
    pub(super) fn state(&self) -> u32 {
        self.region.header().state.load(Ordering::Acquire)
    }
    /// Hands the newest frame not yet seen to `read` (its first byte and
    /// row pitch). Returns `None` when there is none, and `Some((r, false))`
    /// when the helper overwrote the slot during `read`: drop that copy.
    pub(super) fn latest<R>(
        &mut self,
        read: impl FnOnce(*const u8, usize) -> R,
    ) -> Option<(R, bool)> {
        let h = self.region.header();
        let n = h.published.load(Ordering::Acquire);
        if n == 0 || n == self.seen {
            return None;
        }
        let slot = (n % 2) as usize;
        let before = h.seq[slot].load(Ordering::Acquire);
        if before != 2 * n {
            // Already being rewritten with a newer frame; take that one.
            return None;
        }
        let ptr = self
            .region
            .slot(slot, self.stride, self.height as usize)
            .ok()?;
        let result = read(ptr, self.stride);
        fence(Ordering::Acquire);
        let after = h.seq[slot].load(Ordering::Relaxed);
        self.seen = n;
        Some((result, after == before))
    }
}

/// The objects' security: SYSTEM fully; the user may only read the
/// mapping and wait on the event.
pub(super) fn mapping_sddl(user_sid: &str) -> String {
    format!("D:P(A;;GA;;;SY)(A;;0x00020005;;;{user_sid})")
}
pub(super) fn event_sddl(user_sid: &str) -> String {
    format!("D:P(A;;GA;;;SY)(A;;0x{SYNCHRONIZE:08x};;;{user_sid})")
}

/// What the host asks for: an output, by adapter LUID and device name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Want {
    pub luid: (i32, u32),
    pub name: [u16; 32],
}
const WANT: usize = 8 + 64;
pub(super) fn encode_want(want: &Want) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + WANT);
    out.extend((WANT as u16).to_le_bytes());
    out.extend(want.luid.0.to_le_bytes());
    out.extend(want.luid.1.to_le_bytes());
    for c in want.name {
        out.extend(c.to_le_bytes());
    }
    out
}
fn read_want(pipe: &mut impl Read) -> Result<Want> {
    let mut len = [0; 2];
    pipe.read_exact(&mut len)?;
    ensure!(
        usize::from(u16::from_le_bytes(len)) == WANT,
        "malformed frame request"
    );
    let mut b = [0u8; WANT];
    pipe.read_exact(&mut b)?;
    let mut name = [0u16; 32];
    for (i, c) in name.iter_mut().enumerate() {
        *c = u16::from_le_bytes([b[8 + 2 * i], b[9 + 2 * i]]);
    }
    ensure!(name.contains(&0), "unterminated output name");
    Ok(Want {
        luid: (
            i32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
        ),
        name,
    })
}

/// The helper's answer: handles valid in the host's process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Granted {
    pub mapping: u64,
    pub event: u64,
    pub size: u64,
}
pub(super) fn encode_granted(g: &Granted) -> Vec<u8> {
    let mut out = vec![0];
    out.extend(g.mapping.to_le_bytes());
    out.extend(g.event.to_le_bytes());
    out.extend(g.size.to_le_bytes());
    out
}
pub(super) fn encode_refusal(message: &str) -> Vec<u8> {
    let mut message = message.as_bytes().to_vec();
    message.truncate(1024);
    let mut out = vec![1];
    out.extend((message.len() as u16).to_le_bytes());
    out.extend(message);
    out
}
pub(super) fn read_granted(pipe: &mut impl Read) -> Result<Granted> {
    let mut status = [0];
    pipe.read_exact(&mut status)?;
    match status[0] {
        0 => {
            let mut b = [0u8; 24];
            pipe.read_exact(&mut b)?;
            let n = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().expect("8 bytes"));
            Ok(Granted {
                mapping: n(0),
                event: n(8),
                size: n(16),
            })
        }
        1 => {
            let mut len = [0; 2];
            pipe.read_exact(&mut len)?;
            let mut message = vec![0; usize::from(u16::from_le_bytes(len))];
            pipe.read_exact(&mut message)?;
            bail!("{}", String::from_utf8_lossy(&message))
        }
        _ => bail!("unexpected reply from the screen helper"),
    }
}

struct Owned(HANDLE);
impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: The handle is owned and closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
// SAFETY: Kernel handles may be used from any thread.
unsafe impl Send for Owned {}

struct View(MEMORY_MAPPED_VIEW_ADDRESS);
impl Drop for View {
    fn drop(&mut self) {
        // SAFETY: The view was mapped by this process and is unmapped once.
        unsafe {
            let _ = UnmapViewOfFile(self.0);
        }
    }
}
// SAFETY: A mapped view is process memory, usable from any thread.
unsafe impl Send for View {}

fn security(sddl: &str) -> Result<(SECURITY_ATTRIBUTES, PSECURITY_DESCRIPTOR)> {
    let sddl = HSTRING::from(sddl);
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: The descriptor is allocated by the call; the caller frees it.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )?;
    }
    Ok((
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        },
        descriptor,
    ))
}

fn free(descriptor: PSECURITY_DESCRIPTOR) {
    // SAFETY: Allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
    unsafe {
        LocalFree(HLOCAL(descriptor.0));
    }
}

/// Whether the other end of `pipe` is still there.
fn connected(pipe: &Pipe) -> bool {
    let mut available = 0;
    // SAFETY: Reads nothing; only asks how many bytes wait.
    unsafe { PeekNamedPipe(pipe.raw(), None, 0, None, Some(&mut available), None) }.is_ok()
}

/// A handle duplicated into the host's process. Until the host has been
/// told about it, it is closed there again (DUPLICATE_CLOSE_SOURCE from
/// that process) when the grant fails, so a failed grant leaks nothing.
struct Given {
    process: HANDLE,
    handle: HANDLE,
}
impl Given {
    fn value(&self) -> u64 {
        self.handle.0 as u64
    }
    /// The host has it now: it is the host's to close.
    fn handed_over(self) {
        std::mem::forget(self);
    }
}
impl Drop for Given {
    fn drop(&mut self) {
        // SAFETY: Closes the handle this helper put into the host's process,
        // which the host never learnt of; nothing is duplicated here.
        unsafe {
            let _ = DuplicateHandle(
                self.process,
                self.handle,
                HANDLE::default(),
                std::ptr::null_mut(),
                0,
                false,
                DUPLICATE_CLOSE_SOURCE,
            );
        }
    }
}

/// The helper's side of one frame connection: the host's process handle
/// (opened when the pipe was accepted, with PROCESS_DUP_HANDLE) receives
/// the objects. Reading the request, and writing the answer, each end
/// within `wait`; the connection then ends when the host closes the pipe.
pub(super) fn serve(mut pipe: Pipe, client: HANDLE, user_sid: &str, wait: Duration) -> Result<()> {
    pipe.set_deadline(Some(Instant::now() + wait));
    let want = read_want(&mut pipe)?;
    let opened = (|| {
        super::desktop::follow()?;
        let capture = Capture::open(
            Default::default(),
            Target::Exact {
                luid: want.luid,
                name: want.name,
            },
        )?;
        let (width, height) = capture.size();
        let size = mapping_size(width, height).context("output size out of range")?;
        let (attributes, descriptor) = security(&mapping_sddl(user_sid))?;
        // SAFETY: The attributes outlive the call; the mapping is unnamed.
        let mapping = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(&attributes),
                PAGE_READWRITE,
                (size as u64 >> 32) as u32,
                size as u32,
                PCWSTR::null(),
            )
        };
        free(descriptor);
        let mapping = Owned(mapping.context("create the frame mapping")?);
        let (attributes, descriptor) = security(&event_sddl(user_sid))?;
        // SAFETY: As above; the event is unnamed and auto-reset.
        let event = unsafe { CreateEventW(Some(&attributes), false, false, PCWSTR::null()) };
        free(descriptor);
        let event = Owned(event.context("create the frame event")?);
        // SAFETY: The view is unmapped by View's drop, after the writer.
        let view = View(unsafe { MapViewOfFile(mapping.0, FILE_MAP_WRITE, 0, 0, size) });
        ensure!(!view.0.Value.is_null(), "map the frame mapping");
        // SAFETY: The view is `size` bytes, page aligned, and outlives the writer.
        let writer = unsafe { Writer::init(view.0.Value.cast(), size, width, height)? };
        let give = |handle: HANDLE, access: u32| -> Result<Given> {
            let mut theirs = HANDLE::default();
            // SAFETY: Duplicates into the verified host process, with no more
            // access than it needs and nothing inheritable.
            unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    handle,
                    client,
                    &mut theirs,
                    access,
                    false,
                    DUPLICATE_HANDLE_OPTIONS(0),
                )
            }?;
            Ok(Given {
                process: client,
                handle: theirs,
            })
        };
        // If the event cannot be given, dropping the mapping's closes it
        // in the host again.
        let given = (
            give(mapping.0, FILE_MAP_READ.0)?,
            give(event.0, SYNCHRONIZE)?,
        );
        Ok::<_, anyhow::Error>((capture, mapping, event, view, writer, given, size))
    })();
    let (mut capture, _mapping, event, _view, mut writer, (mapping_given, event_given), size) =
        match opened {
            Ok(v) => v,
            Err(e) => {
                pipe.set_deadline(Some(Instant::now() + wait));
                pipe.write_all(&encode_refusal(&format!("{e:#}")))?;
                return Err(e);
            }
        };
    let granted = Granted {
        mapping: mapping_given.value(),
        event: event_given.value(),
        size: size as u64,
    };
    // If the answer does not reach the host, the handles drop here, which
    // closes them in the host again.
    pipe.set_deadline(Some(Instant::now() + wait));
    pipe.write_all(&encode_granted(&granted))?;
    mapping_given.handed_over();
    event_given.handed_over();
    pipe.set_deadline(None);
    let interval = Duration::from_secs(1) / MAX_FPS;
    let mut frame = Bgra::default();
    let mut last = Instant::now() - interval;
    let size = capture.size();
    let result = loop {
        if !connected(&pipe) {
            break Ok(());
        }
        let wait = interval.saturating_sub(last.elapsed());
        if !wait.is_zero() {
            thread::sleep(wait);
        }
        match CpuCapture::grab(&mut capture, &mut frame, Duration::from_millis(100)) {
            Ok(true) => {
                if (frame.width, frame.height) != size {
                    writer.stop(state::RESIZED);
                    break Ok(());
                }
                writer.publish(&frame.data, frame.stride)?;
                last = Instant::now();
                // SAFETY: The event handle is owned.
                unsafe { SetEvent(event.0)? };
            }
            Ok(false) => {}
            Err(e) => {
                writer.stop(state::FAILED);
                break Err(e);
            }
        }
    };
    // SAFETY: As above; wakes a host waiting on a frame that will not come.
    unsafe {
        let _ = SetEvent(event.0);
    }
    result
}

/// Whether the header's state lets the host keep reading this mapping.
fn running(state: u32) -> Result<()> {
    match state {
        state::RUNNING => Ok(()),
        state::RESIZED => Err(Resized.into()),
        _ => bail!("the screen helper stopped capturing"),
    }
}

/// The host's end: frames of one output from the helper.
pub(super) struct Frames {
    pipe: Pipe,
    _mapping: Owned,
    event: Owned,
    reader: Reader,
    // Last, so the reader is gone before its view is unmapped.
    _view: View,
}
impl Frames {
    /// Takes over the handles the helper sent over `pipe`, already
    /// duplicated into this process.
    pub(super) fn open(pipe: Pipe, granted: Granted) -> Result<Self> {
        let mapping = Owned(HANDLE(granted.mapping as usize as *mut _));
        let event = Owned(HANDLE(granted.event as usize as *mut _));
        let size = usize::try_from(granted.size)?;
        ensure!(size >= HEADER, "frame mapping too small");
        // SAFETY: Mapping more than the section holds fails, so a mapped view
        // is `size` readable bytes.
        let view = View(unsafe { MapViewOfFile(mapping.0, FILE_MAP_READ, 0, 0, size) });
        ensure!(!view.0.Value.is_null(), "map the helper's frames");
        // SAFETY: The view is page aligned and outlives the reader (field order).
        let reader = unsafe { Reader::open(view.0.Value.cast(), size)? };
        Ok(Self {
            pipe,
            _mapping: mapping,
            event,
            reader,
            _view: view,
        })
    }
    pub(super) fn size(&self) -> (u32, u32) {
        (self.reader.width, self.reader.height)
    }
    /// Waits up to `timeout` for a new frame and hands it to `upload`.
    /// `Ok(None)`: none came (or it was overtaken while read). A [`Resized`]
    /// error: ask again for a mapping of the new size. Any other error
    /// means the helper stopped.
    pub(super) fn next<R>(
        &mut self,
        timeout: Duration,
        upload: impl FnOnce(*const u8, usize) -> R,
    ) -> Result<Option<R>> {
        running(self.reader.state())?;
        // SAFETY: The event handle is owned.
        let signalled = unsafe {
            WaitForSingleObject(
                self.event.0,
                timeout.as_millis().min(u32::MAX as u128) as u32,
            )
        } == WAIT_OBJECT_0;
        match self.reader.latest(upload) {
            Some((result, true)) => Ok(Some(result)),
            Some((_, false)) => Ok(None),
            None => {
                ensure!(
                    signalled || connected(&self.pipe),
                    "the screen helper went away"
                );
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(width: u32, height: u32) -> Vec<u64> {
        vec![0u64; mapping_size(width, height).unwrap().div_ceil(8)]
    }

    fn read_all(reader: &mut Reader) -> Option<(Vec<u8>, bool)> {
        let (w, h) = (reader.width as usize, reader.height as usize);
        reader.latest(|ptr, stride| {
            let mut out = vec![0; w * 4 * h];
            for y in 0..h {
                // SAFETY: The reader hands a pointer to `h` rows of `stride`.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        ptr.add(y * stride),
                        out.as_mut_ptr().add(y * w * 4),
                        w * 4,
                    )
                };
            }
            out
        })
    }

    #[test]
    fn frames_alternate_slots_and_each_is_read_once() {
        let (w, h) = (3u32, 2u32);
        let mut memory = region(w, h);
        let len = memory.len() * 8;
        let ptr = memory.as_mut_ptr().cast::<u8>();
        // SAFETY: The vector outlives both ends and is 8-byte aligned.
        let (mut writer, mut reader) = unsafe {
            let writer = Writer::init(ptr, len, w, h).unwrap();
            (writer, Reader::open(ptr, len).unwrap())
        };
        assert_eq!((reader.width, reader.height), (3, 2));
        assert!(read_all(&mut reader).is_none(), "nothing published yet");
        // A source with padded rows: only each row's pixels are copied.
        for value in [7u8, 9, 11] {
            let stride = 16;
            let src: Vec<u8> = (0..stride * 2)
                .map(|i| if i % stride < 12 { value } else { 0xee })
                .collect();
            writer.publish(&src, stride).unwrap();
            let (pixels, intact) = read_all(&mut reader).unwrap();
            assert!(intact);
            assert!(pixels.iter().all(|&p| p == value), "{pixels:?}");
            assert!(read_all(&mut reader).is_none(), "a frame is read once");
        }
        // The reader skips straight to the newest of several frames.
        writer.publish(&[1; 24], 12).unwrap();
        writer.publish(&[2; 24], 12).unwrap();
        assert_eq!(read_all(&mut reader).unwrap().0, vec![2; 24]);
        assert_eq!(reader.state(), state::RUNNING);
        writer.stop(state::RESIZED);
        assert_eq!(reader.state(), state::RESIZED);
    }

    #[test]
    fn a_slot_rewritten_during_a_read_is_dropped() {
        let (w, h) = (2u32, 2u32);
        let mut memory = region(w, h);
        let len = memory.len() * 8;
        let ptr = memory.as_mut_ptr().cast::<u8>();
        // SAFETY: As above. The writer is used from inside the read only
        // through its raw pointer, as another process would.
        unsafe {
            let mut writer = Writer::init(ptr, len, w, h).unwrap();
            let mut reader = Reader::open(ptr, len).unwrap();
            writer.publish(&[1; 16], 8).unwrap();
            let writer = std::ptr::addr_of_mut!(writer);
            let (_, intact) = reader
                .latest(|_, _| {
                    // Two more frames: the second lands in the slot being read.
                    (*writer).publish(&[2; 16], 8).unwrap();
                    (*writer).publish(&[3; 16], 8).unwrap();
                })
                .unwrap();
            assert!(!intact);
            assert_eq!(read_all(&mut reader).unwrap().0, vec![3; 16]);
        }
    }

    #[test]
    fn readers_refuse_mappings_that_do_not_hold_their_frames() {
        let mut memory = region(4, 4);
        let len = memory.len() * 8;
        let ptr = memory.as_mut_ptr().cast::<u8>();
        // SAFETY: The vector outlives the calls and is aligned.
        unsafe {
            assert!(Reader::open(ptr, len).is_err(), "no header yet");
            assert!(Writer::init(ptr, len, 5, 4).is_err(), "frames too large");
            Writer::init(ptr, len, 4, 4).unwrap();
            assert!(Reader::open(ptr, len - 1).is_err(), "truncated mapping");
            assert!(Reader::open(ptr, len).is_ok());
            assert!(Region::new(ptr.add(1), len - 8).is_err(), "misaligned");
        }
        assert_eq!(mapping_size(0, 10), None);
        assert_eq!(mapping_size(MAX_SIDE + 1, 10), None);
        assert_eq!(mapping_size(1920, 1080), Some(64 + 2 * 1920 * 4 * 1080));
    }

    #[test]
    fn only_system_and_the_user_reach_the_shared_objects() {
        let sid = "S-1-5-21-1-2-3-1001";
        for sddl in [mapping_sddl(sid), event_sddl(sid)] {
            assert!(sddl.starts_with("D:P(A;;GA;;;SY)"), "{sddl}");
            assert_eq!(sddl.matches("(A;").count(), 2, "{sddl}");
            assert!(!sddl.contains(";BA)") && !sddl.contains(";WD)") && !sddl.contains(";AU)"));
        }
        // The user may map for reading (SECTION_QUERY | SECTION_MAP_READ and
        // READ_CONTROL), never write, delete or change its security.
        let mapping = mapping_sddl(sid);
        assert!(mapping.contains(&format!("(A;;0x00020005;;;{sid})")));
        for denied in [0x2, 0x0001_0000, 0x0004_0000, 0x0008_0000] {
            assert_eq!(0x0002_0005 & denied, 0, "{denied:#x}");
        }
        assert_eq!(
            event_sddl(sid),
            format!("D:P(A;;GA;;;SY)(A;;0x00100000;;;{sid})"),
            "the user may only wait on the event"
        );
    }

    #[test]
    fn a_resize_asks_for_a_new_mapping_and_a_failure_stops() {
        assert!(running(state::RUNNING).is_ok());
        assert!(running(state::RESIZED).unwrap_err().is::<Resized>());
        let failed = running(state::FAILED).unwrap_err();
        assert!(!failed.is::<Resized>());
        assert!(!running(0).unwrap_err().is::<Resized>());
    }

    #[test]
    fn handles_given_to_a_host_that_never_heard_of_them_are_closed_there() {
        use windows::Win32::Foundation::GetHandleInformation;
        // This process stands in for the host.
        // SAFETY: An unnamed event, owned and closed below.
        let event = Owned(unsafe { CreateEventW(None, false, false, PCWSTR::null()) }.unwrap());
        let give = || {
            let mut theirs = HANDLE::default();
            // SAFETY: Duplicates within this process.
            unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    event.0,
                    GetCurrentProcess(),
                    &mut theirs,
                    SYNCHRONIZE,
                    false,
                    DUPLICATE_HANDLE_OPTIONS(0),
                )
            }
            .unwrap();
            Given {
                // SAFETY: The pseudo-handle stands for this process.
                process: unsafe { GetCurrentProcess() },
                handle: theirs,
            }
        };
        let open = |h: HANDLE| {
            let mut flags = 0;
            // SAFETY: Only reads the handle's flags.
            unsafe { GetHandleInformation(h, &mut flags) }.is_ok()
        };
        let dropped = give();
        let handle = dropped.handle;
        assert!(open(handle));
        drop(dropped);
        assert!(!open(handle), "a grant that failed leaves nothing behind");
        let kept = give();
        let handle = kept.handle;
        kept.handed_over();
        assert!(open(handle), "a grant the host received is the host's");
        drop(Owned(handle));
    }

    #[test]
    fn requests_and_replies_round_trip() {
        let mut name = [0u16; 32];
        for (i, c) in r"\\.\DISPLAY12".encode_utf16().enumerate() {
            name[i] = c;
        }
        let want = Want {
            luid: (-2, 0x1234_5678),
            name,
        };
        let bytes = encode_want(&want);
        assert_eq!(bytes.len(), 2 + WANT);
        assert_eq!(read_want(&mut &bytes[..]).unwrap(), want);
        let mut wrong = bytes.clone();
        wrong[0] = 10;
        assert!(read_want(&mut &wrong[..]).is_err());
        let mut unterminated = bytes.clone();
        for b in &mut unterminated[2 + 8..] {
            *b = b'A';
        }
        assert!(read_want(&mut &unterminated[..]).is_err());

        let granted = Granted {
            mapping: 0x1c4,
            event: 0x1c8,
            size: 64 + 16,
        };
        assert_eq!(
            read_granted(&mut &encode_granted(&granted)[..]).unwrap(),
            granted
        );
        let refused = read_granted(&mut &encode_refusal("no such output")[..]).unwrap_err();
        assert_eq!(refused.to_string(), "no such output");
        assert!(read_granted(&mut &[7u8][..]).is_err());
    }
}
