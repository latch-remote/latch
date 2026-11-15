//! AppKit's system cursor, rasterized in encoded-picture pixels. All AppKit
//! calls run on the main queue. A busy/unavailable main queue keeps SCK drawing.

use super::ffi::{Owned, Ref};
use crate::{
    cursor::{unpremultiply, Tracker},
    input::DisplayRect,
    video::CursorUpdate,
};
use latch_proto::control::CursorShape;
use objc2::{class, msg_send, rc::Retained, runtime::AnyObject, MainThreadMarker};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use std::{
    cell::RefCell,
    collections::VecDeque,
    ptr,
    sync::mpsc,
    time::{Duration, Instant},
};

/// Read a system cursor on the main thread without starting screen capture.
/// Returns encoded-picture geometry, suitable for the existing cursor control.
pub fn system_cursor(display: DisplayRect, width: u32, height: u32) -> Option<CursorUpdate> {
    objc2::rc::autoreleasepool(|_| read(display, width, height)).map(|snapshot| CursorUpdate {
        visible: snapshot.visible,
        shape: Some(snapshot.shape),
    })
}

struct Snapshot {
    shape: CursorShape,
    visible: bool,
    sampled: Instant,
}

const SAMPLE_MAX_AGE: Duration = Duration::from_millis(250);
const SHAPE_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 30 + 1);
const VISIBLE_STABLE: Duration = Duration::from_secs(1);
const COMPOSITION_DWELL: Duration = Duration::from_secs(3);

/// Hidden/off-display readings recover after stable visibility. Unreadable or
/// stale observations latch until capture restarts. Immediate fallback plus a
/// three-second composition dwell permits at most two changes in any 3 s window.
#[derive(Default)]
struct Safety {
    separate: bool,
    fallback: bool,
    visible_since: Option<Instant>,
    composed_at: Option<Instant>,
    failed_at: Option<Instant>,
}
impl Safety {
    fn observe(&mut self, visible: Option<bool>, now: Instant) {
        self.fallback |= visible.is_none();
        if visible != Some(true) {
            self.visible_since = None;
            self.separate = false;
            return;
        }
        let since = *self.visible_since.get_or_insert(now);
        self.separate = !self.fallback
            && now.duration_since(since) >= VISIBLE_STABLE
            && self
                .composed_at
                .is_none_or(|at| now.duration_since(at) >= COMPOSITION_DWELL);
    }
    fn configured(&mut self, composited: bool, now: Instant) {
        self.failed_at = None;
        if composited {
            self.composed_at = Some(now);
            self.separate = false;
        }
    }
    fn configuration_failed(&mut self, now: Instant) {
        self.observe(None, now);
        self.failed_at = Some(now);
    }
}

#[derive(Default)]
pub(super) struct Plane {
    pending: Option<(Instant, mpsc::Receiver<Option<Snapshot>>)>,
    checked_shape: Option<Instant>,
    snapshot: Option<Snapshot>,
    last_shape: Option<CursorShape>,
    safety: Safety,
    tracker: Tracker,
    composition: VecDeque<(Instant, bool)>,
    frame_separate: bool,
    preparing: bool,
    entry_announced: bool,
    overlap: bool,
    requested_drawn: bool,
}
impl Plane {
    /// Drop replies in the old display/encoded geometry before restarting SCK.
    pub fn reset(&mut self) {
        self.reset_at(Instant::now());
    }
    fn reset_at(&mut self, now: Instant) {
        self.pending = None;
        self.checked_shape = None;
        self.snapshot = None;
        self.safety = Safety::default();
        self.preparing = false;
        self.entry_announced = false;
        self.configured(true, now);
    }
    fn accept(&mut self, snapshot: Option<Snapshot>, now: Instant) {
        let observation = snapshot.as_ref().and_then(|s| {
            (now.saturating_duration_since(s.sampled) <= SAMPLE_MAX_AGE).then_some(s.visible)
        });
        self.safety.observe(observation, now);
        // Keep the last readable shape until SCK confirms composition. If
        // updateConfiguration fails, the viewer must still draw that shape.
        if let Some(snapshot) = snapshot {
            self.last_shape = Some(snapshot.shape.clone());
            self.snapshot = Some(snapshot);
        }
    }
    pub fn request_drawn(&mut self, drawn: bool) {
        if drawn != self.requested_drawn {
            // Captured mode does not sample. Discard its pending reply so a
            // later free-mode request starts a fresh visibility interval.
            self.pending = None;
            self.checked_shape = None;
            self.safety.visible_since = None;
            self.safety.separate = false;
        }
        self.requested_drawn = drawn;
    }
    pub fn sample(&mut self, display: DisplayRect, width: u32, height: u32) {
        self.sample_at(display, width, height, Instant::now());
    }
    fn sample_at(&mut self, display: DisplayRect, width: u32, height: u32, now: Instant) {
        // CoreGraphics reads are thread-safe. Hidden/off-display is temporary,
        // while failure to obtain a position is an unreliable observation.
        self.safety.observe(visible(display), now);
        if let Some((started, receiver)) = &self.pending {
            match receiver.try_recv() {
                Ok(snapshot) => {
                    self.pending = None;
                    self.accept(snapshot, now);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending = None;
                    self.accept(None, now);
                }
                Err(mpsc::TryRecvError::Empty) => {
                    if now.duration_since(*started) > SAMPLE_MAX_AGE {
                        self.safety.observe(None, now);
                    }
                    return;
                }
            }
        }
        if !self.shape_due(now) {
            return;
        }
        if MainThreadMarker::new().is_some() {
            self.accept(
                objc2::rc::autoreleasepool(|_| read(display, width, height)),
                Instant::now(),
            );
        } else {
            let (tx, rx) = mpsc::sync_channel(1);
            dispatch2::DispatchQueue::main().exec_async(move || {
                let _ = tx.send(objc2::rc::autoreleasepool(|_| read(display, width, height)));
            });
            // Only the first negotiation waits briefly. Normal frames use the
            // last completed sample and never block the encoder on AppKit.
            if self.snapshot.is_none() {
                if let Ok(snapshot) = rx.recv_timeout(Duration::from_millis(40)) {
                    self.accept(snapshot, Instant::now());
                    return;
                }
            }
            self.pending = Some((now, rx));
        }
    }
    fn shape_due(&mut self, now: Instant) -> bool {
        if self
            .checked_shape
            .is_some_and(|at| now.duration_since(at) < SHAPE_INTERVAL)
        {
            return false;
        }
        self.checked_shape = Some(now);
        true
    }
    pub fn can_configure(&self, now: Instant) -> bool {
        self.safety
            .failed_at
            .is_none_or(|at| now.duration_since(at) >= COMPOSITION_DWELL)
    }
    pub fn configuration_drawn(&mut self) -> bool {
        let drawn = self.drawn();
        if drawn {
            self.entry_announced = false;
        }
        // Send a local-plane update with one final composited picture before
        // omitting it from capture. This deliberately chooses a brief double.
        self.preparing = !drawn && !self.entry_announced;
        drawn || self.preparing
    }
    pub fn configured(&mut self, composited: bool, now: Instant) {
        self.preparing = false;
        self.safety.configured(composited, now);
        self.composition.push_back((now, composited));
    }
    pub fn encoded(&mut self, captured: Instant) {
        // VideoToolbox may still return up to two pre-transition pictures.
        // Their cursor control must reflect their own composition mode.
        let was_separate = self.frame_separate;
        while self
            .composition
            .front()
            .is_some_and(|(at, _)| *at <= captured)
        {
            self.frame_separate = !self.composition.pop_front().unwrap().1;
        }
        // Keep the plane through the first composited picture as well. Control
        // and video travel separately, so favour overlap on both handoff edges.
        self.overlap = was_separate && !self.frame_separate;
    }
    pub fn update_for_frame(&mut self) -> Option<CursorUpdate> {
        if self.preparing {
            self.entry_announced = true;
        }
        self.update(
            !(self.frame_separate || self.preparing || self.entry_announced || self.overlap),
        )
    }
    pub fn configuration_failed(&mut self, now: Instant) {
        self.preparing = false;
        self.safety.configuration_failed(now);
    }
    pub fn drawn(&self) -> bool {
        self.requested_drawn || !self.safety.separate
    }
    pub fn update(&mut self, composited: bool) -> Option<CursorUpdate> {
        // Use the applied SCK mode, including a failed configuration change.
        // A hidden/unreadable sample requests composition, never a blank plane.
        self.tracker.update(!composited, self.last_shape.clone())
    }
}

// currentSystemCursor can return fresh NSCursor/NSImage identities for an
// unchanged shape. Compare its small source bitmap before drawing/encoding.
thread_local! {
    static SHAPE: RefCell<ShapeCache> = RefCell::new(ShapeCache::default());
}
#[derive(PartialEq)]
struct ShapeKey {
    points: [f64; 5],
    display: DisplayRect,
    pixels: (u32, u32, u32, u32),
    format: [usize; 6],
}
#[derive(Default)]
struct ShapeCache {
    key: Option<ShapeKey>,
    source: Vec<u8>,
    image: Option<Owned>,
    shape: Option<CursorShape>,
}
impl ShapeCache {
    fn get(
        &mut self,
        key: ShapeKey,
        source: &[u8],
        draw: impl FnOnce() -> Option<CursorShape>,
    ) -> Option<CursorShape> {
        if self.key.as_ref() != Some(&key) || self.source != source {
            self.source.clear();
            self.source.extend_from_slice(source);
            self.shape = draw();
            self.key = Some(key);
        }
        self.shape.clone()
    }
}

/// Encoded pixels per global desktop point. Both the capture's aspect fit and
/// each display's Retina point geometry are reflected here.
fn geometry(
    display: DisplayRect,
    width: u32,
    height: u32,
    size: CGSize,
    hotspot: CGPoint,
) -> Option<(u32, u32, u32, u32)> {
    if display.width == 0 || display.height == 0 || !hotspot.x.is_finite() || !hotspot.y.is_finite()
    {
        return None;
    }
    let scale = (f64::from(width) / f64::from(display.width))
        .min(f64::from(height) / f64::from(display.height));
    let w = (size.width * scale).round();
    let h = (size.height * scale).round();
    if !scale.is_finite()
        || scale <= 0.0
        || !w.is_finite()
        || !h.is_finite()
        || !(1.0..=4096.0).contains(&w)
        || !(1.0..=4096.0).contains(&h)
    {
        return None;
    }
    Some((
        w as u32,
        h as u32,
        (hotspot.x * scale).round().clamp(0.0, w - 1.0) as u32,
        (hotspot.y * scale).round().clamp(0.0, h - 1.0) as u32,
    ))
}
fn read(display: DisplayRect, width: u32, height: u32) -> Option<Snapshot> {
    MainThreadMarker::new()?;
    // SAFETY: AppKit is accessed only on the main thread. The retained cursor
    // and image keep their borrowed CGImage alive through rasterization.
    unsafe {
        let cursor: Option<Retained<AnyObject>> = msg_send![class!(NSCursor), currentSystemCursor];
        let cursor = cursor?;
        let image: Retained<AnyObject> = msg_send![&*cursor, image];
        let hotspot: CGPoint = msg_send![&*cursor, hotSpot];
        let size: CGSize = msg_send![&*image, size];
        let (w, h, hx, hy) = geometry(display, width, height, size, hotspot)?;
        let backing = backing_scale(display);
        let proposed = representation_size(size, backing, w, h);
        let mut rect = CGRect::new(CGPoint::new(0.0, 0.0), proposed);
        let cg: *const CGImage = msg_send![&*image, CGImageForProposedRect: &mut rect, context: ptr::null::<AnyObject>(), hints: ptr::null::<AnyObject>()];
        if cg.is_null() {
            return None;
        }
        let key = ShapeKey {
            points: [size.width, size.height, hotspot.x, hotspot.y, backing],
            display,
            pixels: (w, h, hx, hy),
            format: [
                CGImageGetWidth(cg),
                CGImageGetHeight(cg),
                CGImageGetBytesPerRow(cg),
                CGImageGetBitsPerPixel(cg),
                CGImageGetBitsPerComponent(cg),
                CGImageGetBitmapInfo(cg) as usize,
            ],
        };
        let shape = SHAPE.with_borrow_mut(|cache| {
            if cache.key.as_ref() == Some(&key)
                && cache.image.as_ref().is_some_and(|old| old.0 == cg.cast())
            {
                return cache.shape.clone();
            }
            let provider = CGImageGetDataProvider(cg);
            if provider.is_null() {
                return None;
            }
            let data = Owned::new(CGDataProviderCopyData(provider)).ok()?;
            let len = CFDataGetLength(data.0);
            if !(1..=64 * 1024 * 1024).contains(&len) {
                return None;
            }
            let bytes = CFDataGetBytePtr(data.0);
            if bytes.is_null() {
                return None;
            }
            let shape = cache.get(key, std::slice::from_raw_parts(bytes, len as usize), || {
                rasterize(cg, w, h, hx, hy)
            });
            cache.image = Some(Owned::retain(cg.cast()));
            shape
        })?;
        Some(Snapshot {
            visible: visible(display)?,
            shape,
            sampled: Instant::now(),
        })
    }
}

fn visible(display: DisplayRect) -> Option<bool> {
    // SAFETY: CoreGraphics queries neither inject events nor change visibility.
    unsafe {
        let Ok(event) = Owned::new(CGEventCreate(ptr::null())) else {
            return None;
        };
        let point = CGEventGetLocation(event.0);
        Some(
            point.x >= f64::from(display.x)
                && point.y >= f64::from(display.y)
                && point.x < f64::from(display.x) + f64::from(display.width)
                && point.y < f64::from(display.y) + f64::from(display.height)
                && CGCursorIsVisible() != 0,
        )
    }
}
/// Request at least the display's backing pixels so Retina artwork remains
/// sharp even when the encoded picture is downscaled.
fn representation_size(size: CGSize, backing: f64, w: u32, h: u32) -> CGSize {
    CGSize::new(
        (size.width * backing).max(f64::from(w)),
        (size.height * backing).max(f64::from(h)),
    )
}
fn backing_scale(display: DisplayRect) -> f64 {
    let center = CGPoint::new(
        f64::from(display.x) + f64::from(display.width) / 2.0,
        f64::from(display.y) + f64::from(display.height) / 2.0,
    );
    let (mut id, mut count) = (0, 0);
    // SAFETY: CoreGraphics only writes one display ID and a display count.
    if unsafe { CGGetDisplaysWithPoint(center, 1, &mut id, &mut count) } == 0
        && count == 1
        && display.width != 0
    {
        (unsafe { CGDisplayPixelsWide(id) }) as f64 / f64::from(display.width)
    } else {
        1.0
    }
}
unsafe fn rasterize(cg: *const CGImage, w: u32, h: u32, hx: u32, hy: u32) -> Option<CursorShape> {
    let color = Owned::new(CGColorSpaceCreateWithName(kCGColorSpaceSRGB)).ok()?;
    let mut rgba = vec![0; w as usize * h as usize * 4];
    let context = Owned::new(CGBitmapContextCreate(
        rgba.as_mut_ptr().cast(),
        w as usize,
        h as usize,
        8,
        w as usize * 4,
        color.0,
        0x4001,
    ))
    .ok()?;
    CGContextDrawImage(
        context.0,
        CGRect::new(
            CGPoint::new(0.0, 0.0),
            CGSize::new(f64::from(w), f64::from(h)),
        ),
        cg,
    );
    drop(context);
    if !rgba.as_chunks::<4>().0.iter().any(|p| p[3] != 0) {
        return None;
    }
    unpremultiply(&mut rgba);
    Some(CursorShape::new(0, w, h, hx, hy, &rgba))
}

#[repr(C)]
struct CGImage {
    _private: [u8; 0],
}
// SAFETY: matches the opaque CoreGraphics CGImage name in Objective-C methods.
unsafe impl objc2::encode::RefEncode for CGImage {
    const ENCODING_REF: objc2::encode::Encoding =
        objc2::encode::Encoding::Pointer(&objc2::encode::Encoding::Struct("CGImage", &[]));
}
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    static kCGColorSpaceSRGB: Ref;
    fn CGImageGetDataProvider(image: *const CGImage) -> Ref;
    fn CGDataProviderCopyData(provider: Ref) -> Ref;
    fn CGImageGetWidth(image: *const CGImage) -> usize;
    fn CGImageGetHeight(image: *const CGImage) -> usize;
    fn CGImageGetBytesPerRow(image: *const CGImage) -> usize;
    fn CGImageGetBitsPerPixel(image: *const CGImage) -> usize;
    fn CGImageGetBitsPerComponent(image: *const CGImage) -> usize;
    fn CGImageGetBitmapInfo(image: *const CGImage) -> u32;
    fn CGGetDisplaysWithPoint(point: CGPoint, max: u32, displays: *mut u32, count: *mut u32)
        -> i32;
    fn CGDisplayPixelsWide(display: u32) -> usize;
    fn CGColorSpaceCreateWithName(name: Ref) -> Ref;
    fn CGBitmapContextCreate(
        data: *mut std::ffi::c_void,
        width: usize,
        height: usize,
        bits: usize,
        stride: usize,
        color: Ref,
        info: u32,
    ) -> Ref;
    fn CGContextDrawImage(context: Ref, rect: CGRect, image: *const CGImage);
    fn CGEventCreate(source: Ref) -> Ref;
    fn CGEventGetLocation(event: Ref) -> CGPoint;
    // Exported public ABI, deprecated since 10.9. AppKit has no replacement
    // for system-wide visibility. Keep this isolated for a future replacement.
    fn CGCursorIsVisible() -> u32;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFDataGetLength(data: Ref) -> isize;
    fn CFDataGetBytePtr(data: Ref) -> *const u8;
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(visible: bool) -> Snapshot {
        Snapshot {
            shape: CursorShape::new(0, 1, 1, 0, 0, &[255; 4]),
            visible,
            sampled: Instant::now(),
        }
    }
    fn accept_at(plane: &mut Plane, visible: Option<bool>, now: Instant) {
        let sample = visible.map(|visible| Snapshot {
            sampled: now,
            ..snapshot(visible)
        });
        plane.accept(sample, now);
    }
    #[test]
    fn every_mode_transition_selects_exactly_one_cursor_and_only_unreliable_readings_latch() {
        // Exhaust visible, hidden, off-display and unreadable observations,
        // requested free/captured mode and SCK success/failure. Hidden and
        // off-display both arrive as Some(false).
        for sequence in 0..256 {
            let mut n = sequence;
            let mut plane = Plane::default();
            let mut composited = true;
            let mut unreliable_seen = false;
            let mut local = false;
            let start = Instant::now();
            for step in 0..4 {
                let now = start + Duration::from_secs(step * 5);
                let reading = n % 4;
                n /= 4;
                unreliable_seen |= reading == 3;
                let visible = match reading {
                    0 => Some(true),
                    1 | 2 => Some(false),
                    _ => None,
                };
                accept_at(&mut plane, visible, now);
                accept_at(&mut plane, visible, now + VISIBLE_STABLE);
                assert_eq!(plane.safety.separate, reading == 0 && !unreliable_seen);
                for captured in [true, false] {
                    plane.requested_drawn = captured;
                    let desired = plane.drawn();
                    for succeeds in [false, true] {
                        let applied = if succeeds { desired } else { composited };
                        if let Some(update) = plane.update(applied) {
                            local = update.visible;
                        }
                        assert_eq!(u8::from(applied) + u8::from(local), 1,
                            "sequence {sequence}, step {step}, captured {captured}, success {succeeds}");
                    }
                }
                plane.requested_drawn = false;
                if plane.drawn() != composited {
                    composited = plane.drawn();
                    plane.configured(composited, now + VISIBLE_STABLE);
                }
            }
        }
    }
    #[test]
    fn off_display_and_hidden_for_typing_return_after_stable_visibility() {
        for _reason in ["off display", "hidden for typing"] {
            let mut plane = Plane::default();
            let start = Instant::now();
            accept_at(&mut plane, Some(true), start);
            assert!(plane.drawn());
            accept_at(&mut plane, Some(true), start + VISIBLE_STABLE);
            assert!(!plane.drawn());
            let hidden = start + Duration::from_secs(2);
            accept_at(&mut plane, Some(false), hidden);
            assert!(plane.drawn());
            plane.configured(true, hidden);
            accept_at(&mut plane, Some(true), hidden + Duration::from_secs(2));
            accept_at(
                &mut plane,
                Some(true),
                hidden + COMPOSITION_DWELL - Duration::from_nanos(1),
            );
            assert!(plane.drawn());
            accept_at(&mut plane, Some(true), hidden + COMPOSITION_DWELL);
            assert!(!plane.drawn());
            assert!(!plane.safety.fallback);
        }
    }
    #[test]
    fn rapid_flapping_at_the_hysteresis_bound_limits_configuration_changes() {
        let mut plane = Plane::default();
        let start = Instant::now();
        let mut composited = true;
        let mut changes = Vec::new();
        // One millisecond steps include both edges of the 1 s and 3 s gates.
        for millis in 0..30_000 {
            let now = start + Duration::from_millis(millis);
            // Stay visible long enough to return, then hide immediately.
            let visible = millis % 4001 != 1001;
            accept_at(&mut plane, Some(visible), now);
            if plane.drawn() != composited {
                composited = plane.drawn();
                plane.configured(composited, now);
                changes.push(now);
            }
        }
        assert!(
            changes.len() >= 10,
            "temporary fallback must keep recovering"
        );
        for triple in changes.windows(3) {
            assert!(triple[2].duration_since(triple[0]) >= COMPOSITION_DWELL);
        }
        let now = start + Duration::from_secs(31);
        plane.configuration_failed(now);
        assert!(plane.drawn());
        assert!(!plane.can_configure(now + COMPOSITION_DWELL - Duration::from_nanos(1)));
        assert!(plane.can_configure(now + COMPOSITION_DWELL));
        accept_at(&mut plane, Some(true), now + Duration::from_secs(10));
        assert!(plane.drawn(), "configuration failure latches");
    }
    #[test]
    fn handoffs_keep_a_cursor_on_queued_frames_and_choose_one_frame_of_overlap() {
        let mut plane = Plane::default();
        let start = Instant::now();
        accept_at(&mut plane, Some(true), start);
        accept_at(&mut plane, Some(true), start + VISIBLE_STABLE);
        // First announce the local plane while SCK still draws.
        assert!(plane.configuration_drawn());
        plane.encoded(start + VISIBLE_STABLE);
        assert!(plane.update_for_frame().unwrap().visible);
        assert!(!plane.configuration_drawn());
        plane.configured(false, start + Duration::from_millis(1100));
        // An old composited frame may still leave VideoToolbox first. Keeping
        // the announcement enabled here favours a double until separate video.
        let mut local = true;
        for (millis, composited, expected) in [(1050, true, 2), (1200, false, 1), (1900, false, 1)]
        {
            plane.encoded(start + Duration::from_millis(millis));
            if let Some(update) = plane.update_for_frame() {
                local = update.visible;
            }
            assert_eq!(u8::from(composited) + u8::from(local), expected);
        }
        accept_at(&mut plane, Some(false), start + Duration::from_secs(2));
        assert!(plane.configuration_drawn());
        plane.configured(true, start + Duration::from_secs(2));
        for (millis, composited, expected) in [(1950, false, 1), (2100, true, 2), (2200, true, 1)] {
            plane.encoded(start + Duration::from_millis(millis));
            if let Some(update) = plane.update_for_frame() {
                local = update.visible;
            }
            assert_eq!(u8::from(composited) + u8::from(local), expected);
        }
        assert!(plane.composition.is_empty());
        // Restart keeps the last shape for old separate frames while discarding
        // sampling geometry and restoring composition for the new capture.
        plane.configured(false, start + Duration::from_secs(4));
        plane.encoded(start + Duration::from_millis(4100));
        assert!(plane.update_for_frame().unwrap().visible);
        plane.reset_at(start + Duration::from_secs(5));
        assert!(plane.snapshot.is_none());
        plane.encoded(start + Duration::from_millis(4200));
        assert!(plane.update_for_frame().is_none());
        plane.encoded(start + Duration::from_millis(5100));
        assert!(plane.update_for_frame().is_none());
        plane.encoded(start + Duration::from_millis(5200));
        assert!(!plane.update_for_frame().unwrap().visible);
    }
    #[test]
    fn captured_mode_pause_discards_old_pending_samples_without_latching() {
        let mut plane = Plane::default();
        let start = Instant::now();
        accept_at(&mut plane, Some(true), start);
        let (tx, rx) = mpsc::sync_channel(1);
        tx.send(Some(Snapshot {
            sampled: start,
            ..snapshot(true)
        }))
        .unwrap();
        plane.pending = Some((start, rx));
        plane.request_drawn(true);
        assert!(plane.pending.is_none());
        plane.configured(true, start);
        plane.request_drawn(false);
        accept_at(&mut plane, Some(true), start + Duration::from_secs(10));
        assert!(plane.drawn());
        accept_at(&mut plane, Some(true), start + Duration::from_secs(11));
        assert!(!plane.drawn());
        assert!(!plane.safety.fallback);
    }
    #[test]
    fn stalled_samples_and_restart_discard_stale_geometry() {
        let mut plane = Plane::default();
        let now = Instant::now();
        accept_at(&mut plane, Some(true), now - VISIBLE_STABLE);
        accept_at(&mut plane, Some(true), now);
        assert!(!plane.drawn());
        let (_tx, rx) = mpsc::sync_channel(1);
        plane.pending = Some((now - Duration::from_secs(1), rx));
        plane.sample_at(DisplayRect::default(), 1920, 1080, now);
        assert!(plane.drawn());
        accept_at(&mut plane, Some(true), now + Duration::from_secs(5));
        assert!(plane.drawn());
        let restarted = now + Duration::from_secs(6);
        plane.reset_at(restarted);
        assert!(plane.pending.is_none());
        assert!(plane.snapshot.is_none());
        assert!(plane.drawn());
        accept_at(&mut plane, Some(true), restarted);
        accept_at(&mut plane, Some(true), restarted + COMPOSITION_DWELL);
        assert!(!plane.drawn());
        let mut stale = snapshot(true);
        stale.sampled = restarted;
        plane.accept(Some(stale), restarted + COMPOSITION_DWELL + SAMPLE_MAX_AGE);
        assert!(plane.drawn());
        accept_at(&mut plane, Some(true), now + Duration::from_secs(10));
        assert!(plane.drawn());
    }
    #[test]
    fn shape_checks_are_limited_to_thirty_hz_with_an_injected_clock() {
        let mut plane = Plane::default();
        let start = Instant::now();
        assert!(plane.shape_due(start));
        assert!(!plane.shape_due(start));
        assert!(!plane.shape_due(start + SHAPE_INTERVAL - Duration::from_nanos(1)));
        assert!(plane.shape_due(start + SHAPE_INTERVAL));
        plane.checked_shape = None;
        let checks = (0..1_000_000)
            .filter(|micros| plane.shape_due(start + Duration::from_micros(*micros)))
            .count();
        assert_eq!(checks, 30);
        // Draining a pending reply precedes the gate, even between checks.
        let (tx, rx) = mpsc::sync_channel(1);
        tx.send(Some(Snapshot {
            sampled: start,
            ..snapshot(true)
        }))
        .unwrap();
        plane.pending = Some((start, rx));
        plane.checked_shape = Some(start);
        plane.sample_at(
            DisplayRect::default(),
            1920,
            1080,
            start + Duration::from_millis(1),
        );
        assert!(plane.pending.is_none());
        assert!(plane.snapshot.is_some());
        assert_eq!(plane.checked_shape, Some(start));
    }
    #[test]
    fn unchanged_cursor_is_rasterized_and_encoded_only_once() {
        let mut cache = ShapeCache::default();
        let draws = std::cell::Cell::new(0);
        let mut read = |identity, scale: u32| {
            cache.get(
                ShapeKey {
                    points: [16.0, 16.0, 2.0, 2.0, 2.0],
                    display: DisplayRect::default(),
                    pixels: (16 * scale, 16 * scale, 2 * scale, 2 * scale),
                    format: [16, 16, 64, 32, 8, 0x4001],
                },
                &vec![identity; 16 * 16 * 4],
                || {
                    draws.set(draws.get() + 1);
                    Some(snapshot(true).shape)
                },
            )
        };
        for _ in 0..144 {
            assert!(read(1, 1).is_some());
        }
        assert_eq!(draws.get(), 1);
        read(2, 1);
        assert_eq!(draws.get(), 2);
        read(2, 2);
        assert_eq!(draws.get(), 3);
        read(2, 2);
        assert_eq!(draws.get(), 3);
    }
    #[test]
    fn retina_hotspots_follow_the_selected_display_and_aspect_fit() {
        let display = DisplayRect {
            x: 1440,
            y: 0,
            width: 1440,
            height: 900,
        };
        let size = CGSize::new(32.0, 32.0);
        assert_eq!(
            representation_size(size, 2.0, 38, 38),
            CGSize::new(64.0, 64.0)
        );
        assert_eq!(
            representation_size(size, 1.0, 64, 64),
            CGSize::new(64.0, 64.0)
        );
        let hot = CGPoint::new(5.0, 8.0);
        assert_eq!(
            geometry(display, 2880, 1800, size, hot),
            Some((64, 64, 10, 16))
        );
        assert_eq!(
            geometry(display, 1920, 1080, size, hot),
            Some((38, 38, 6, 10))
        );
        assert_eq!(
            geometry(display, 1440, 900, size, hot),
            Some((32, 32, 5, 8))
        );
        assert!(geometry(display, 0, 0, size, hot).is_none());
        assert!(geometry(display, 1920, 1080, size, CGPoint::new(f64::NAN, 0.0)).is_none());
        assert!(geometry(DisplayRect::default(), 1920, 1080, size, hot).is_none());
    }
    #[test]
    fn old_peers_missing_shapes_and_captured_mode_keep_capture_drawing() {
        let mut plane = Plane::default();
        assert!(plane.drawn());
        let now = Instant::now();
        accept_at(&mut plane, Some(true), now);
        accept_at(&mut plane, Some(true), now + VISIBLE_STABLE);
        assert!(!plane.drawn());
        plane.requested_drawn = true;
        assert!(plane.drawn());
        assert!(!plane.update(true).unwrap().visible);
    }
}
