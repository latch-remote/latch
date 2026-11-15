//! DXGI pointer buffers and plane decisions, kept platform independent so CI
//! exercises malformed buffers and XOR cursors on every OS.
use latch_proto::control::{Cursor, CursorShape};
use parking_lot::Mutex;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

// Viewers cache textures by serial across capture/encoder reopen.
static NEXT_SERIAL: AtomicU32 = AtomicU32::new(1);

#[derive(Clone, Copy, Debug)]
pub(crate) struct ShapeInfo {
    pub kind: u32,
    pub width: u32,
    /// Monochrome buffers contain the AND rows followed by the XOR rows.
    pub height: u32,
    pub pitch: u32,
    pub hot_x: i32,
    pub hot_y: i32,
}
impl ShapeInfo {
    pub fn size(self) -> Option<(usize, usize)> {
        let height = if self.kind == 1 {
            self.height / 2
        } else {
            self.height
        };
        let row = if self.kind == 1 {
            self.width.div_ceil(8)
        } else {
            self.width.checked_mul(4)?
        };
        (matches!(self.kind, 1 | 2 | 4)
            && self.width > 0
            && height > 0
            && self.width <= 4096
            && height <= 4096
            && (self.kind != 1 || self.height.is_multiple_of(2))
            && self.pitch >= row)
            .then_some((self.width as usize, height as usize))
    }
    /// The GPU compositor consumes the original AND/XOR semantics.
    pub fn pixels(self, bytes: &[u8]) -> Option<Vec<u32>> {
        let (width, height) = self.size()?;
        let pitch = self.pitch as usize;
        if pitch.checked_mul(self.height as usize)? > bytes.len() {
            return None;
        }
        let mut pixels = Vec::with_capacity(width * height);
        for y in 0..height {
            for x in 0..width {
                let row = y * pitch;
                pixels.push(if self.kind == 1 {
                    let and = (bytes[row + x / 8] >> (7 - x % 8)) & 1;
                    let xor = (bytes[row + height * pitch + x / 8] >> (7 - x % 8)) & 1;
                    (u32::from(and) * 0xff000000) | (u32::from(xor) * 0x00ffffff)
                } else {
                    let at = row + x * 4;
                    u32::from_le_bytes(bytes[at..at + 4].try_into().ok()?)
                });
            }
        }
        Some(pixels)
    }
    pub fn rgba(self, bytes: &[u8], serial: u32) -> Option<CursorShape> {
        let (width, height) = self.size()?;
        if self.hot_x < 0
            || self.hot_y < 0
            || self.hot_x as usize >= width
            || self.hot_y as usize >= height
        {
            return None;
        }
        let pixels = self.pixels(bytes)?;
        let mut rgba = Vec::with_capacity(pixels.len() * 4);
        let mut inverted = vec![false; pixels.len()];
        for (i, pixel) in pixels.into_iter().enumerate() {
            let [b, g, r, a] = pixel.to_le_bytes();
            let color = match self.kind {
                // Microsoft's Desktop Duplication sample uploads the colour
                // buffer directly and blends with SRC_ALPHA / INV_SRC_ALPHA.
                // DXGI therefore supplies straight-alpha BGRA, unlike GDI's
                // premultiplied colour bitmap used by the fallback.
                2 => [r, g, b, a],
                1 | 4 => {
                    if a == 0 {
                        [r, g, b, 255]
                    } else if a != 255 {
                        return None;
                    } else if [r, g, b] == [0; 3] {
                        [0; 4]
                    } else if [r, g, b] == [255; 3] {
                        inverted[i] = true;
                        [255; 4]
                    } else {
                        // Arbitrary colour XOR depends on the desktop. Keep its
                        // exact appearance in the GPU compositor for this shape.
                        return None;
                    }
                }
                _ => return None,
            };
            rgba.extend_from_slice(&color);
        }
        if !inverted.iter().any(|&v| v) {
            return Some(CursorShape::new(
                serial,
                width as u32,
                height as u32,
                self.hot_x as u32,
                self.hot_y as u32,
                &rgba,
            ));
        }
        // White inverted regions with a one-pixel dark outline stay legible on
        // light and dark desktops. Padding preserves edge pixels and the hotspot.
        let stride = width + 2;
        let mut outlined = vec![0; stride * (height + 2) * 4];
        for y in 0..height {
            for x in 0..width {
                if inverted[y * width + x] {
                    for dy in 0..3 {
                        for dx in 0..3 {
                            let at = ((y + dy) * stride + x + dx) * 4;
                            outlined[at..at + 4].copy_from_slice(&[0, 0, 0, 255]);
                        }
                    }
                }
            }
        }
        for y in 0..height {
            for x in 0..width {
                let at = (y * width + x) * 4;
                if rgba[at + 3] != 0 {
                    let dst = ((y + 1) * stride + x + 1) * 4;
                    outlined[dst..dst + 4].copy_from_slice(&rgba[at..at + 4]);
                }
            }
        }
        Some(CursorShape::new(
            serial,
            stride as u32,
            (height + 2) as u32,
            self.hot_x as u32 + 1,
            self.hot_y as u32 + 1,
            &outlined,
        ))
    }
}

pub(crate) fn straight(channel: u8, alpha: u8) -> u8 {
    if alpha == 0 {
        0
    } else {
        ((u32::from(channel) * 255 + u32::from(alpha) / 2) / u32::from(alpha)).min(255) as u8
    }
}

/// Position-only messages are coalesced to at most 120 Hz. Shape and visibility
/// changes bypass the interval so mode switches and hiding remain immediate.
const POSITION_INTERVAL: Duration = Duration::from_micros(8_334);

/// Sample position independently of capture, encoding and video pacing. Two
/// ticks bound sampling plus coalescing to 16.668 ms, excluding OS scheduling
/// and control-transport backpressure. Shape and ownership stay capture-owned.
pub(crate) fn position_worker(
    control: &Control,
    stop: &std::sync::mpsc::Receiver<()>,
    mut sample: impl FnMut() -> Option<[i32; 2]>,
) {
    while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = stop.recv_timeout(POSITION_INTERVAL)
    {
        if control.drawn.load(Ordering::Relaxed)
            || !control
                .publication
                .lock()
                .last
                .is_some_and(|last| last.visible)
        {
            continue;
        }
        if let Some(point) = sample() {
            control.publish_position_at(point, Instant::now());
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Published {
    visible: bool,
    position: Option<[i32; 2]>,
    serial: u32,
}

#[derive(Clone, Copy)]
struct PositionSource {
    display: crate::input::DisplayRect,
    stream: (u32, u32),
}
#[derive(Default)]
struct Publication {
    last: Option<Published>,
    last_sent: Option<Instant>,
    source: Option<PositionSource>,
}
impl Publication {
    fn publish(
        &mut self,
        control: &Control,
        current: Published,
        shape: Option<&CursorShape>,
        now: Instant,
    ) {
        if self.last == Some(current) {
            return;
        }
        let position_only = self
            .last
            .is_some_and(|last| last.visible == current.visible && last.serial == current.serial);
        if position_only
            && self
                .last_sent
                .is_some_and(|at| now.saturating_duration_since(at) < POSITION_INTERVAL)
        {
            return;
        }
        let Some(sink) = control.sink.lock().clone() else {
            return;
        };
        sink(Cursor {
            visible: current.visible,
            position: current.position,
            shape: if current.visible && self.last.is_none_or(|last| last.serial != current.serial)
            {
                shape.cloned()
            } else {
                None
            },
        });
        self.last = Some(current);
        self.last_sent = Some(now);
    }
}

#[derive(Default)]
pub(crate) struct Control {
    pub drawn: AtomicBool,
    pub sink: Mutex<Option<crate::video::CursorSink>>,
    publication: Mutex<Publication>,
}
impl Control {
    pub fn composited() -> Arc<Self> {
        Arc::new(Self {
            drawn: AtomicBool::new(true),
            ..Default::default()
        })
    }
    fn publish_position_at(&self, point: [i32; 2], now: Instant) {
        let mut publication = self.publication.lock();
        let (Some(source), Some(last)) = (publication.source, publication.last) else {
            return;
        };
        // Only capture may enable a plane or replace its shape. Position
        // sampling keeps the latest known shape moving until capture resumes.
        if self.drawn.load(Ordering::Relaxed) || !last.visible {
            return;
        }
        let (point, on_output) = position(point, source.display, source.stream);
        if on_output {
            publication.publish(
                self,
                Published {
                    position: Some(point),
                    ..last
                },
                None,
                now,
            );
        }
    }
}

pub(crate) struct Plane {
    pub control: Arc<Control>,
    pub shape: Option<CursorShape>,
    shape_visible: bool,
    source_shape: Option<(ShapeInfo, Vec<u8>)>,
    serial: u32,
}
impl Plane {
    pub fn new(control: Arc<Control>) -> Self {
        *control.publication.lock() = Publication::default();
        Self {
            control,
            shape: None,
            shape_visible: false,
            source_shape: None,
            serial: 0,
        }
    }
    pub fn shape(&mut self, info: ShapeInfo, bytes: &[u8], source: (u32, u32), stream: (u32, u32)) {
        self.convert_shape(info, bytes, source, stream);
        self.source_shape = Some((info, bytes.to_vec()));
    }
    /// A resize changes scaling, not the DXGI shape. DXGI need not resend it.
    pub fn resize(&mut self, source: (u32, u32), stream: (u32, u32)) {
        if let Some((info, bytes)) = self.source_shape.take() {
            self.convert_shape(info, &bytes, source, stream);
            self.source_shape = Some((info, bytes));
        }
    }
    fn convert_shape(
        &mut self,
        info: ShapeInfo,
        bytes: &[u8],
        source: (u32, u32),
        stream: (u32, u32),
    ) {
        let mut shape = info.rgba(bytes, self.serial);
        if let Some(shape) = shape.as_mut() {
            crate::software::scale_cursor(shape, source, stream);
        }
        let mut shape_visible = false;
        if let Some(candidate) = shape.as_ref() {
            let source_pixels = info.pixels(bytes).unwrap_or_default();
            let source_visible = source_pixels.iter().any(|&pixel| {
                if info.kind == 2 {
                    pixel >> 24 != 0
                } else {
                    pixel >> 24 == 0 || pixel & 0x00ff_ffff != 0
                }
            });
            let inverted = info.kind != 2 && source_pixels.contains(&0xffff_ffff);
            let rgba = candidate.decode_rgba().unwrap_or_default();
            let pixels = rgba.as_chunks::<4>().0;
            let visible = pixels.iter().any(|pixel| pixel[3] != 0);
            shape_visible = visible;
            let white = pixels.contains(&[255; 4]);
            let dark = pixels.contains(&[0, 0, 0, 255]);
            // Nearest-neighbour scaling must not erase a thin pointer or its
            // inversion outline. Such shapes retain exact GPU composition.
            if (source_visible && !visible) || (inverted && (!white || !dark)) {
                shape = None;
            }
        }
        self.shape_visible = shape_visible;
        if self.shape != shape {
            self.serial = NEXT_SERIAL.fetch_add(1, Ordering::Relaxed);
            if let Some(shape) = shape.as_mut() {
                shape.serial = self.serial;
            }
            self.shape = shape;
        }
    }
    /// Visibility is authoritative only when DXGI and the OS cursor query agree
    /// that the pointer is separate from the acquired desktop image.
    pub fn separate(&self, known: bool, embedded: bool) -> bool {
        !self.control.drawn.load(Ordering::Relaxed)
            && self.control.sink.lock().is_some()
            && known
            && !embedded
            && self.shape.is_some()
    }
    pub fn publish(&mut self, visible: bool, position: [i32; 2], separate: bool) {
        self.publish_at(visible, position, separate, Instant::now());
    }
    pub(crate) fn publish_at(
        &mut self,
        visible: bool,
        position: [i32; 2],
        separate: bool,
        now: Instant,
    ) {
        let visible = visible && separate && self.shape_visible;
        let mut publication = self.control.publication.lock();
        let serial = if visible {
            self.serial
        } else {
            publication.last.map_or(0, |last| last.serial)
        };
        publication.publish(
            &self.control,
            Published {
                visible,
                position: visible.then_some(position),
                serial,
            },
            self.shape.as_ref(),
            now,
        );
    }
    pub fn track_position(
        &self,
        display: crate::input::DisplayRect,
        stream: (u32, u32),
        separate: bool,
    ) {
        self.control.publication.lock().source =
            separate.then_some(PositionSource { display, stream });
    }
    pub fn invalidate(&mut self) {
        self.shape = None;
        self.shape_visible = false;
        self.source_shape = None;
        self.publish(false, [0, 0], false);
    }
}

/// Cursor ownership and fallback caching, shared by the real DXGI capture
/// loop and fake pointer sources in tests. Hidden cursors require no drawing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Owner {
    Hidden,
    Desktop,
    Host,
    Viewer,
}
#[derive(Clone, Copy)]
pub(crate) struct PointerSample {
    pub os_visible: Option<bool>,
    pub on_output: bool,
    pub dxgi_visible: bool,
    pub position: [i32; 2],
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DrawState {
    owner: Owner,
    position: [i32; 2],
    revision: u64,
}
pub(crate) struct CaptureState {
    pub pointer_known: bool,
    pub fallback_needed: bool,
    pub source_ready: bool,
    pub bitmap_warning: bool,
    fallback_key: Option<usize>,
    fallback_cached: bool,
    retry_at: Option<Instant>,
    revision: u64,
    current: Option<DrawState>,
    rendered: Option<DrawState>,
}
impl Default for CaptureState {
    fn default() -> Self {
        Self {
            pointer_known: false,
            fallback_needed: true,
            source_ready: false,
            bitmap_warning: false,
            fallback_key: None,
            fallback_cached: false,
            retry_at: None,
            revision: 0,
            current: None,
            rendered: None,
        }
    }
}
impl CaptureState {
    pub fn restart(&mut self) {
        *self = Self::default();
    }
    pub fn fallback_due(&mut self, key: usize, now: Instant) -> bool {
        if self.fallback_key != Some(key) {
            self.fallback_key = Some(key);
            self.fallback_cached = false;
            self.retry_at = None;
        }
        !self.fallback_cached && self.retry_at.is_none_or(|at| now >= at)
    }
    pub fn fallback_read(&mut self, success: bool, now: Instant) {
        self.fallback_cached = success;
        self.retry_at = Some(now + Duration::from_millis(250));
    }
    pub fn shape_uploaded(&mut self) {
        self.bitmap_warning = false;
        self.revision = self.revision.wrapping_add(1);
    }
    pub fn dxgi_shape(&mut self) {
        self.fallback_needed = false;
        // The renderer now holds the DXGI texture, so a later fallback must
        // restore its own bitmap even if the OS handle stayed the same.
        self.fallback_cached = false;
        self.retry_at = None;
        self.shape_uploaded();
    }
    pub fn update(&mut self, sample: PointerSample, separate: bool) -> (bool, Owner) {
        let (visible, embedded) = visibility(
            sample.os_visible,
            sample.on_output,
            sample.dxgi_visible,
            self.pointer_known,
        );
        let owner = if !visible {
            Owner::Hidden
        } else if embedded {
            Owner::Desktop
        } else if separate {
            Owner::Viewer
        } else {
            Owner::Host
        };
        self.current = Some(DrawState {
            owner,
            position: if owner == Owner::Host {
                sample.position
            } else {
                [0; 2]
            },
            revision: if owner == Owner::Host {
                self.revision
            } else {
                0
            },
        });
        (visible, owner)
    }
    pub fn can_render(&self) -> bool {
        // Cursor recovery must never hold up a usable desktop picture.
        self.source_ready && self.current.is_some()
    }
    pub fn needs_render(&self) -> bool {
        self.current != self.rendered
    }
    pub fn rendered(&mut self) {
        self.rendered = self.current;
    }
}

/// GetCursorInfo reports the hotspot in DPI-aware virtual-desktop pixels.
/// DXGI positions remain the top-left used by the original GPU compositor.
pub(crate) fn position(
    point: [i32; 2],
    display: crate::input::DisplayRect,
    stream: (u32, u32),
) -> ([i32; 2], bool) {
    let x = i64::from(point[0]) - i64::from(display.x);
    let y = i64::from(point[1]) - i64::from(display.y);
    let fit = display.fitted(stream.0, stream.1);
    let local = [
        fit.x + (x * i64::from(fit.width) / i64::from(display.width.max(1))) as i32,
        fit.y + (y * i64::from(fit.height) / i64::from(display.height.max(1))) as i32,
    ];
    (
        local,
        x >= 0 && y >= 0 && x < i64::from(display.width) && y < i64::from(display.height),
    )
}

/// Reconcile games' hidden/suppressed cursors and cursors on other outputs.
/// A visible OS cursor absent from DXGI's plane is already in the desktop.
pub(crate) fn visibility(
    os_visible: Option<bool>,
    on_output: bool,
    dxgi_visible: bool,
    pointer_known: bool,
) -> (bool, bool) {
    let visible = os_visible.map_or(dxgi_visible, |showing| showing && on_output);
    (visible, visible && pointer_known && !dxgi_visible)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn info(kind: u32, width: u32, height: u32, pitch: u32) -> ShapeInfo {
        ShapeInfo {
            kind,
            width,
            height,
            pitch,
            hot_x: 0,
            hot_y: 0,
        }
    }
    #[test]
    fn support_docs_keep_live_windows_capture_unverified() {
        let platforms = include_str!("../../../docs/PLATFORMS.md");
        let cursor = platforms
            .lines()
            .find(|line| line.starts_with("| Cursor |"))
            .unwrap();
        assert!(cursor.starts_with("| Cursor | **partial**"));
        assert!(cursor.contains("live Windows capture is unverified"));
        for direction in ["| Windows → Mac |", "| Windows → Linux |"] {
            let row = platforms
                .lines()
                .find(|line| line.starts_with(direction))
                .unwrap();
            assert!(row.contains("live Windows capture is unverified"));
        }
        let changelog = include_str!("../../../CHANGELOG.md");
        assert!(changelog.contains("Live Windows capture of the separate plane is unverified"));
    }
    #[test]
    fn hdr_cursor_white_difference_is_documented() {
        let protocol = include_str!("../../../docs/PROTOCOL.md");
        assert!(protocol.contains("SDR white level in nits"));
        assert!(protocol.contains("captured/free switches can change cursor brightness"));
        assert!(protocol.contains("carries no SDR white luminance metadata"));
    }
    #[test]
    fn gdi_premultiplied_fallback_matches_straight_dxgi_colours() {
        assert_eq!(
            [straight(10, 128), straight(20, 128), straight(30, 128)],
            [20, 40, 60]
        );
        assert_eq!(straight(99, 0), 0);
        assert_eq!(straight(128, 128), 255);
        assert_eq!(straight(7, 255), 7);
        // DXGI supplies straight alpha, including semi-transparent edges.
        let shape = info(2, 1, 1, 4).rgba(&[20, 40, 60, 128], 1).unwrap();
        assert_eq!(shape.decode_rgba().unwrap(), [60, 40, 20, 128]);
    }
    #[test]
    fn position_updates_are_bounded_and_never_resend_cached_pixels() {
        let control = Control::composited();
        let updates = Arc::new(Mutex::new(Vec::new()));
        let received = updates.clone();
        *control.sink.lock() = Some(Arc::new(move |cursor| received.lock().push(cursor)));
        let mut plane = Plane::new(control);
        plane.shape(info(2, 1, 1, 4), &[255; 4], (1, 1), (1, 1));
        let pixels = plane.shape.as_ref().unwrap().rgba.as_ptr();
        let now = Instant::now();
        plane.publish_at(true, [0, 0], true, now);
        for x in 1..1000 {
            plane.publish_at(true, [x, 0], true, now + Duration::from_micros(x as u64));
        }
        assert_eq!(updates.lock().len(), 1);
        plane.publish_at(true, [999, 0], true, now + POSITION_INTERVAL);
        assert_eq!(updates.lock().len(), 2);
        assert!(updates.lock()[1].shape.is_none());
        assert_eq!(plane.shape.as_ref().unwrap().rgba.as_ptr(), pixels);
        // Hiding bypasses the rate limit. Invisible pointer moves send nothing.
        plane.publish_at(true, [1000, 0], false, now + POSITION_INTERVAL);
        for x in 1001..2000 {
            plane.publish_at(true, [x, 0], false, now + POSITION_INTERVAL);
        }
        assert_eq!(updates.lock().len(), 3);
        assert!(!updates.lock()[2].visible);
        assert_eq!(updates.lock()[2].position, None);
    }
    #[test]
    fn position_worker_delivers_final_motion_while_video_is_blocked() {
        let control = Control::composited();
        control.drawn.store(false, Ordering::Relaxed);
        let (updates, received) = std::sync::mpsc::channel();
        *control.sink.lock() = Some(Arc::new(move |cursor| {
            updates.send(cursor).unwrap();
        }));
        let mut plane = Plane::new(control.clone());
        plane.shape(info(2, 1, 1, 4), &[255; 4], (100, 100), (100, 100));
        plane.track_position(
            crate::input::DisplayRect {
                width: 100,
                height: 100,
                ..Default::default()
            },
            (100, 100),
            true,
        );
        let now = Instant::now();
        plane.publish_at(true, [0, 0], true, now);
        received.recv().unwrap();
        let final_motion = now + Duration::from_micros(1);
        control.publish_position_at([9, 7], final_motion);
        assert!(received.try_recv().is_err());
        // Deterministic deadline, with no capture tick or encoded frame.
        control.publish_position_at([9, 7], now + POSITION_INTERVAL);
        let moved = received.try_recv().unwrap();
        assert_eq!(moved.position, Some([9, 7]));
        assert!(moved.shape.is_none());
        assert!(
            control
                .publication
                .lock()
                .last_sent
                .unwrap()
                .duration_since(final_motion)
                <= POSITION_INTERVAL * 2
        );

        let (stop, stopped) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let worker_control = &control;
            scope.spawn(move || position_worker(worker_control, &stopped, || Some([19, 17])));
            // Model an encoder or network pacer that never requests capture
            // again. The OS position sampler still sends the final position.
            let moved = received.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(moved.position, Some([19, 17]));
            assert!(moved.shape.is_none());
            stop.send(()).unwrap();
        });
        // Idle sampling never adds a shape, changes ownership, or revives a
        // hidden/unreadable plane. Other outputs cannot move this plane.
        control.publish_position_at([101, 40], now + POSITION_INTERVAL * 10);
        assert!(received.try_recv().is_err());
        control.drawn.store(true, Ordering::Relaxed);
        control.publish_position_at([30, 40], now + POSITION_INTERVAL * 10);
        assert!(received.try_recv().is_err());
        control.drawn.store(false, Ordering::Relaxed);
        plane.invalidate();
        received.recv().unwrap();
        control.publish_position_at([30, 40], now + POSITION_INTERVAL * 10);
        assert!(received.try_recv().is_err());
    }
    #[test]
    fn fallback_reads_once_per_handle_and_retries_failures_after_250_ms() {
        let mut state = CaptureState::default();
        let now = Instant::now();
        let mut reads = 0;
        for micros in 0..1000 {
            let at = now + Duration::from_micros(micros);
            if state.fallback_due(1, at) {
                reads += 1;
                state.fallback_read(true, at);
                state.shape_uploaded();
            }
        }
        assert_eq!(reads, 1); // No first mouse move is needed for caching.
        assert!(state.fallback_due(2, now));
        state.fallback_read(false, now);
        assert!(!state.fallback_due(2, now + Duration::from_millis(249)));
        assert!(state.fallback_due(2, now + Duration::from_millis(250)));
        state.fallback_read(true, now + Duration::from_millis(250));
        assert!(!state.fallback_due(2, now + Duration::from_secs(1)));
        state.restart();
        assert!(state.fallback_due(2, now + Duration::from_secs(1)));
    }
    #[test]
    fn frames_keep_flowing_when_no_cursor_bitmap_ever_arrives() {
        let mut state = CaptureState::default();
        let sample = PointerSample {
            os_visible: Some(true),
            on_output: true,
            dxgi_visible: false,
            position: [1, 2],
        };
        assert_eq!(state.update(sample, false).1, Owner::Host);
        assert!(!state.can_render()); // Still need the first desktop picture.
        state.source_ready = true;
        let now = Instant::now();
        let mut reads = 0;
        for millis in 0..2000 {
            let at = now + Duration::from_millis(millis);
            if state.fallback_due(1, at) {
                reads += 1;
                state.fallback_read(false, at); // Current bitmap and arrow failed.
            }
            state.update(sample, false);
            assert!(state.can_render(), "video stalled at {millis} ms");
            state.rendered();
        }
        assert_eq!(reads, 8); // Recovery keeps its 250 ms backoff.
    }
    #[test]
    fn pointer_recovery_repaints_an_idle_desktop_after_cursor_free_frames() {
        let mut state = CaptureState {
            source_ready: true,
            ..Default::default()
        };
        let sample = PointerSample {
            os_visible: Some(true),
            on_output: true,
            dxgi_visible: false,
            position: [1, 2],
        };
        let now = Instant::now();
        state.update(sample, false);
        assert!(state.fallback_due(1, now));
        state.fallback_read(false, now);
        assert!(state.can_render());
        state.rendered(); // The desktop was delivered without a bitmap.
        assert!(!state.fallback_due(1, now + Duration::from_millis(100)));
        assert!(state.fallback_due(1, now + Duration::from_millis(250)));
        state.fallback_read(true, now + Duration::from_millis(250));
        state.shape_uploaded();
        assert_eq!(state.update(sample, false).1, Owner::Host);
        assert!(state.can_render());
        assert!(state.needs_render()); // No new desktop or mouse event needed.
        state.rendered();
        assert!(!state.needs_render());
    }
    #[test]
    fn capture_state_assigns_exactly_one_drawer_through_idle_modes_and_recovery() {
        struct FakePointer {
            state: CaptureState,
            plane: Plane,
            sample: PointerSample,
        }
        impl FakePointer {
            fn tick(&mut self, expected: Owner, repaint: bool) {
                let (_, embedded) = visibility(
                    self.sample.os_visible,
                    self.sample.on_output,
                    self.sample.dxgi_visible,
                    self.state.pointer_known,
                );
                let separate = self.plane.separate(
                    self.sample.os_visible.is_some()
                        && self.state.pointer_known
                        && !self.state.fallback_needed,
                    embedded,
                );
                let (visible, owner) = self.state.update(self.sample, separate);
                assert_eq!(owner, expected);
                assert!(self.state.can_render());
                let host_draws = matches!(owner, Owner::Desktop | Owner::Host);
                let viewer_draws = owner == Owner::Viewer;
                assert_eq!(
                    u8::from(host_draws) + u8::from(viewer_draws),
                    u8::from(visible)
                );
                assert_eq!(self.state.needs_render(), repaint);
                self.state.rendered();
            }
            fn shape(&mut self) {
                self.plane
                    .shape(info(2, 4, 4, 16), &[255; 64], (100, 100), (100, 100));
                self.state.dxgi_shape();
            }
        }
        let control = Control::composited();
        *control.sink.lock() = Some(Arc::new(|_| {}));
        control.drawn.store(false, Ordering::Relaxed);
        let mut pointer = FakePointer {
            state: CaptureState {
                source_ready: true,
                ..Default::default()
            },
            plane: Plane::new(control.clone()),
            sample: PointerSample {
                os_visible: Some(true),
                on_output: true,
                dxgi_visible: false,
                position: [10, 20],
            },
        };
        pointer.tick(Owner::Host, true); // Startup, cached system bitmap.
        pointer.tick(Owner::Host, false); // Idle AcquireNextFrame timeout.
        pointer.state.shape_uploaded();
        pointer.tick(Owner::Host, true); // A fallback texture changed on timeout.
        pointer.state.pointer_known = true;
        pointer.sample.dxgi_visible = true;
        pointer.shape();
        pointer.tick(Owner::Viewer, true);
        pointer.sample.position = [15, 20];
        pointer.tick(Owner::Viewer, false); // Position-only update skips video.
        control.drawn.store(true, Ordering::Relaxed);
        pointer.tick(Owner::Host, true); // Capture mouse without a new DXGI frame.
        pointer.sample.position = [16, 20];
        pointer.tick(Owner::Host, true); // OS fallback movement on a timeout.
        control.drawn.store(false, Ordering::Relaxed);
        pointer.tick(Owner::Viewer, true);
        pointer.tick(Owner::Viewer, false);
        pointer.plane.resize((200, 200), (100, 100));
        assert_eq!(pointer.plane.shape.as_ref().unwrap().width, 2);
        pointer.tick(Owner::Viewer, false); // Resolution change retains the plane.
        pointer.sample.dxgi_visible = false;
        pointer.tick(Owner::Desktop, true); // Cursor already embedded by DXGI.
        pointer.sample.os_visible = Some(false);
        pointer.tick(Owner::Hidden, true);
        pointer.sample.os_visible = Some(true);
        pointer.sample.on_output = false;
        pointer.tick(Owner::Hidden, false);
        pointer.sample.on_output = true;
        pointer.sample.dxgi_visible = true;
        pointer.sample.os_visible = None;
        pointer.tick(Owner::Host, true); // Failed OS query retains composition.
        pointer.sample.os_visible = Some(true);
        pointer.plane.invalidate();
        pointer.state.fallback_needed = true;
        pointer.tick(Owner::Host, false); // Unreadable DXGI shape.
        pointer
            .plane
            .shape(info(4, 1, 1, 4), &[1, 2, 3, 255], (1, 1), (1, 1));
        pointer.tick(Owner::Host, false); // Arbitrary XOR stays composited.
        for output_changed in [false, true] {
            if output_changed {
                pointer.plane = Plane::new(control.clone());
                pointer.state = CaptureState::default();
            } else {
                pointer.plane.invalidate();
                pointer.state.restart();
            }
            assert!(!pointer.state.source_ready);
            pointer.state.source_ready = true; // First desktop after reopening.
            pointer.sample.dxgi_visible = false;
            pointer.tick(Owner::Host, true);
            pointer.tick(Owner::Host, false);
            pointer.state.pointer_known = true;
            pointer.sample.dxgi_visible = true;
            pointer.shape();
            pointer.tick(Owner::Viewer, true);
        }
        *control.sink.lock() = None;
        pointer.tick(Owner::Host, true); // A viewer without a plane sink uses composition.
    }
    #[test]
    fn colour_alpha_and_padded_odd_rows_are_straight_rgba() {
        let shape = info(2, 3, 1, 16)
            .rgba(
                &[10, 20, 30, 128, 3, 2, 1, 255, 99, 88, 77, 0, 9, 9, 9, 9],
                4,
            )
            .unwrap();
        assert_eq!(
            shape.decode_rgba().unwrap(),
            [30, 20, 10, 128, 1, 2, 3, 255, 77, 88, 99, 0]
        );
        assert_eq!((shape.width, shape.height, shape.serial), (3, 1, 4));
    }
    #[test]
    fn colour_and_masked_colour_read_each_odd_row_using_pitch() {
        for kind in [2, 4] {
            let mut bytes = vec![222; 16 * 3];
            for row in 0..3 {
                for col in 0..3 {
                    let at = row * 16 + col * 4;
                    bytes[at..at + 4].copy_from_slice(&[
                        row as u8,
                        col as u8,
                        42,
                        if kind == 2 { 128 } else { 0 },
                    ]);
                }
            }
            let mut metadata = info(kind, 3, 3, 16);
            metadata.hot_x = 2;
            metadata.hot_y = 1;
            let shape = metadata.rgba(&bytes, 1).unwrap();
            assert_eq!((shape.hot_x, shape.hot_y), (2, 1));
            let rgba = shape.decode_rgba().unwrap();
            for row in 0..3 {
                for col in 0..3 {
                    let at = (row * 3 + col) * 4;
                    assert_eq!(
                        &rgba[at..at + 4],
                        [42, col as u8, row as u8, if kind == 2 { 128 } else { 255 }]
                    );
                }
            }
        }
    }
    #[test]
    fn monochrome_and_xor_truth_table_with_odd_width_and_padded_pitch() {
        // 00 black, 01 white, 10 transparent, 11 inverse. Bits are MSB first.
        let shape = info(1, 5, 2, 2)
            .rgba(&[0b00110000, 99, 0b01010000, 99], 1)
            .unwrap();
        assert_eq!(
            (shape.width, shape.height, shape.hot_x, shape.hot_y),
            (7, 3, 1, 1)
        );
        let p = shape.decode_rgba().unwrap();
        let at = |x: usize, y: usize| &p[(y * 7 + x) * 4..(y * 7 + x + 1) * 4];
        assert_eq!(at(1, 1), [0, 0, 0, 255]);
        assert_eq!(at(2, 1), [255; 4]);
        assert_eq!(at(4, 1), [255; 4]);
        assert_eq!(at(4, 0), [0, 0, 0, 255]);
        assert_eq!(at(0, 0), [0; 4]);
    }
    #[test]
    fn monochrome_32_by_32_inverse_has_dark_outline_at_edges() {
        let mut bytes = vec![0xff; 8 * 64];
        bytes[8 * 32..].fill(0);
        bytes[8 * 32] = 0x80;
        let shape = info(1, 32, 64, 8).rgba(&bytes, 2).unwrap();
        assert_eq!((shape.width, shape.height), (34, 34));
        let p = shape.decode_rgba().unwrap();
        assert_eq!(&p[..4], [0, 0, 0, 255]);
        assert_eq!(&p[(34 + 1) * 4..(34 + 2) * 4], [255; 4]);
        assert_eq!(&p[(34 + 5) * 4..(34 + 6) * 4], [0; 4]);
    }
    #[test]
    fn masked_colour_copy_transparent_inverse_and_unrepresentable_colour() {
        let shape = info(4, 3, 1, 16)
            .rgba(
                &[30, 20, 10, 0, 0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 0],
                1,
            )
            .unwrap();
        let p = shape.decode_rgba().unwrap();
        assert_eq!(&p[(5 + 1) * 4..(5 + 2) * 4], [10, 20, 30, 255]);
        assert_eq!(&p[(5 + 3) * 4..(5 + 4) * 4], [255; 4]);
        assert!(info(4, 1, 1, 4).rgba(&[1, 2, 3, 255], 1).is_none());
        assert!(info(4, 1, 1, 4).rgba(&[0, 0, 0, 128], 1).is_none());
        // The compositor retains the exact arbitrary XOR bytes.
        assert_eq!(
            info(4, 1, 1, 4).pixels(&[1, 2, 3, 255]).unwrap(),
            [0xff030201]
        );
    }
    #[test]
    fn malformed_shapes_fall_back_without_panicking() {
        for shape in [
            info(2, 0, 1, 4),
            info(2, 3, 2, 8),
            info(1, 9, 2, 1),
            info(1, 8, 3, 1),
            info(9, 1, 1, 4),
            info(2, u32::MAX, 1, 4),
        ] {
            assert!(shape.rgba(&[0; 32], 1).is_none(), "{shape:?}");
            assert!(shape.pixels(&[0; 32]).is_none());
        }
        assert!(info(2, 3, 2, 16).rgba(&[0; 31], 1).is_none());
        let mut hot = info(2, 1, 1, 4);
        hot.hot_x = -1;
        assert!(hot.rgba(&[255; 4], 1).is_none());
        hot.hot_x = 1;
        assert!(hot.rgba(&[255; 4], 1).is_none());
    }
    #[test]
    fn positions_use_physical_output_coordinates_scale_and_letterbox() {
        let display = crate::input::DisplayRect {
            x: -3840,
            y: 100,
            width: 3840,
            height: 2160,
        };
        assert_eq!(
            position([-1920, 1180], display, (1920, 1080)),
            ([960, 540], true)
        );
        assert!(!position([0, 1180], display, (1920, 1080)).1);
        assert!(!position([-3841, 100], display, (1920, 1080)).1);
        assert!(!position([-3000, 99], display, (1920, 1080)).1);
        assert!(!position([-3000, 2260], display, (1920, 1080)).1);
        assert_eq!(
            position([-3840, 100], display, (1920, 1200)),
            ([0, 60], true)
        );
    }
    #[test]
    fn visibility_handles_hidden_suppressed_unknown_and_embedded_pointers() {
        assert_eq!(visibility(Some(false), true, true, true), (false, false));
        assert_eq!(visibility(Some(true), false, true, true), (false, false));
        assert_eq!(visibility(Some(true), true, true, true), (true, false));
        assert_eq!(visibility(Some(true), true, false, true), (true, true));
        assert_eq!(visibility(Some(true), true, false, false), (true, false));
        assert_eq!(visibility(None, true, true, true), (true, false));
        assert_eq!(visibility(None, true, false, true), (false, false));
    }
    #[test]
    fn a_transparent_game_cursor_is_reported_hidden() {
        let control = Control::composited();
        let (send, receive) = std::sync::mpsc::channel();
        *control.sink.lock() = Some(Arc::new(move |cursor| {
            send.send(cursor).unwrap();
        }));
        control.drawn.store(false, Ordering::Relaxed);
        let mut plane = Plane::new(control);
        plane.shape(info(2, 3, 1, 12), &[0; 12], (3, 1), (3, 1));
        assert!(plane.separate(true, false));
        plane.publish(true, [1, 0], true);
        assert!(!receive.recv().unwrap().visible);
    }
    #[test]
    fn capture_reopen_does_not_reuse_a_viewer_texture_serial() {
        let control = Control::composited();
        let mut before = Plane::new(control.clone());
        before.shape(info(2, 1, 1, 4), &[255; 4], (1, 1), (1, 1));
        let first = before.shape.as_ref().unwrap().serial;
        let mut after = Plane::new(control);
        after.shape(info(2, 1, 1, 4), &[0, 0, 0, 255], (1, 1), (1, 1));
        assert_ne!(first, after.shape.as_ref().unwrap().serial);
    }
    #[test]
    fn scaling_that_erases_an_inverse_region_falls_back_to_composition() {
        let mut plane = Plane::new(Control::composited());
        let mut bytes = vec![255; 256];
        bytes[128..].fill(0);
        bytes[128] = 128;
        plane.shape(info(1, 32, 64, 4), &bytes, (64, 64), (32, 32));
        assert!(plane.shape.is_none());
    }
    #[test]
    fn plane_caches_shapes_scales_hotspot_and_falls_back_on_doubt() {
        let control = Control::composited();
        let updates = Arc::new(Mutex::new(Vec::new()));
        let received = updates.clone();
        *control.sink.lock() = Some(Arc::new(move |cursor| received.lock().push(cursor)));
        let mut plane = Plane::new(control.clone());
        assert!(!plane.separate(true, false));
        control.drawn.store(false, Ordering::Relaxed);
        assert!(!plane.separate(true, false));
        let mut metadata = info(2, 4, 4, 20);
        metadata.hot_x = 2;
        metadata.hot_y = 2;
        plane.shape(metadata, &[255; 80], (3840, 2160), (1920, 1080));
        assert!(plane.separate(true, false));
        assert!(!plane.separate(false, false));
        assert!(!plane.separate(true, true));
        let now = Instant::now();
        plane.publish_at(true, [10, 20], true, now);
        let first = updates.lock()[0].shape.clone().unwrap();
        assert_eq!(
            (first.width, first.height, first.hot_x, first.hot_y),
            (2, 2, 1, 1)
        );
        plane.shape(metadata, &[255; 80], (3840, 2160), (1920, 1080));
        plane.publish_at(true, [11, 20], true, now + POSITION_INTERVAL);
        plane.publish_at(true, [11, 20], true, now + POSITION_INTERVAL);
        assert_eq!(updates.lock().len(), 2);
        assert!(updates.lock()[1].shape.is_none());
        plane.publish(false, [11, 20], true);
        assert!(!updates.lock()[2].visible);
        control.drawn.store(true, Ordering::Relaxed);
        assert!(!plane.separate(true, false));
        plane.invalidate();
        assert!(!plane.separate(true, false));
        plane.shape(info(4, 1, 1, 4), &[1, 2, 3, 255], (1, 1), (1, 1));
        assert!(plane.shape.is_none());
    }
}
