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

/// One unsafe reading latches composition for this capture's lifetime. This
/// deliberately conservative hysteresis permits at most one fallback, even if
/// deprecated visibility readings flap or the main queue repeatedly stalls.
#[derive(Default)]
struct Safety {
    separate: bool,
    fallback: bool,
}
impl Safety {
    fn observe(&mut self, trustworthy_visible: bool) {
        self.fallback |= !trustworthy_visible;
        self.separate = trustworthy_visible && !self.fallback;
    }
}

#[derive(Default)]
pub(super) struct Plane {
    pending: Option<(Instant, mpsc::Receiver<Option<Snapshot>>)>,
    checked_shape: Option<Instant>,
    snapshot: Option<Snapshot>,
    safety: Safety,
    tracker: Tracker,
    pub requested_drawn: bool,
}
impl Plane {
    /// Drop replies in the old display/encoded geometry before restarting SCK.
    pub fn reset(&mut self) {
        self.pending = None;
        self.checked_shape = None;
        self.snapshot = None;
        self.safety = Safety::default();
    }
    fn accept(&mut self, snapshot: Option<Snapshot>) {
        self.safety.observe(
            snapshot
                .as_ref()
                .is_some_and(|s| s.visible && s.sampled.elapsed() <= Duration::from_millis(250)),
        );
        // Keep the last readable shape until SCK confirms composition. If
        // updateConfiguration fails, the viewer must still draw that shape.
        if snapshot.is_some() {
            self.snapshot = snapshot;
        }
    }
    pub fn sample(&mut self, display: DisplayRect, width: u32, height: u32) {
        // CoreGraphics position/visibility reads are thread-safe. AppKit shape
        // checks run at most 30 Hz, including when its object identities churn.
        if !visible(display) {
            self.safety.observe(false);
        }
        if let Some((started, receiver)) = &self.pending {
            match receiver.try_recv() {
                Ok(snapshot) => {
                    self.pending = None;
                    self.accept(snapshot);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending = None;
                    self.accept(None);
                }
                Err(mpsc::TryRecvError::Empty) => {
                    if started.elapsed() > Duration::from_millis(250) {
                        self.safety.observe(false);
                    }
                    return;
                }
            }
        }
        if self
            .checked_shape
            .is_some_and(|at| at.elapsed() < Duration::from_millis(33))
        {
            return;
        }
        self.checked_shape = Some(Instant::now());
        if MainThreadMarker::new().is_some() {
            self.accept(objc2::rc::autoreleasepool(|_| read(display, width, height)));
        } else {
            let (tx, rx) = mpsc::sync_channel(1);
            dispatch2::DispatchQueue::main().exec_async(move || {
                let _ = tx.send(objc2::rc::autoreleasepool(|_| read(display, width, height)));
            });
            // Only the first negotiation waits briefly. Normal frames use the
            // last completed sample and never block the encoder on AppKit.
            if self.snapshot.is_none() {
                if let Ok(snapshot) = rx.recv_timeout(Duration::from_millis(40)) {
                    self.accept(snapshot);
                    return;
                }
            }
            self.pending = Some((Instant::now(), rx));
        }
    }
    pub fn drawn(&self) -> bool {
        self.requested_drawn || !self.safety.separate
    }
    pub fn update(&mut self, composited: bool) -> Option<CursorUpdate> {
        // Use the applied SCK mode, including a failed configuration change.
        // A hidden/unreadable sample requests composition, never a blank plane.
        self.tracker
            .update(!composited, self.snapshot.as_ref().map(|s| s.shape.clone()))
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
    source: Vec<u8>,
}
#[derive(Default)]
struct ShapeCache {
    key: Option<ShapeKey>,
    shape: Option<CursorShape>,
}
impl ShapeCache {
    fn get(
        &mut self,
        key: ShapeKey,
        draw: impl FnOnce() -> Option<CursorShape>,
    ) -> Option<CursorShape> {
        if self.key.as_ref() != Some(&key) {
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
            source: std::slice::from_raw_parts(bytes, len as usize).to_vec(),
        };
        let shape =
            SHAPE.with_borrow_mut(|cache| cache.get(key, || rasterize(cg, w, h, hx, hy)))?;
        Some(Snapshot {
            visible: visible(display),
            shape,
            sampled: Instant::now(),
        })
    }
}

fn visible(display: DisplayRect) -> bool {
    // SAFETY: CoreGraphics queries neither inject events nor change visibility.
    unsafe {
        let Ok(event) = Owned::new(CGEventCreate(ptr::null())) else {
            return false;
        };
        let point = CGEventGetLocation(event.0);
        point.x >= f64::from(display.x)
            && point.y >= f64::from(display.y)
            && point.x < f64::from(display.x) + f64::from(display.width)
            && point.y < f64::from(display.y) + f64::from(display.height)
            && CGCursorIsVisible() != 0
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
    #[test]
    fn every_mode_transition_selects_exactly_one_cursor_and_fallback_latches() {
        // Exhaust every sequence of visible, hidden and unreadable observations,
        // requested free/captured mode, and configuration success/failure.
        for sequence in 0..81 {
            let mut n = sequence;
            let mut plane = Plane::default();
            let mut composited = true;
            let mut unsafe_seen = false;
            let mut flips = 0;
            let mut local = false;
            for step in 0..4 {
                let reading = n % 3;
                n /= 3;
                unsafe_seen |= reading != 0;
                plane.accept(match reading {
                    0 => Some(snapshot(true)),
                    1 => Some(snapshot(false)),
                    _ => None,
                });
                if unsafe_seen {
                    assert!(plane.drawn());
                }
                for captured in [true, false] {
                    plane.requested_drawn = captured;
                    let desired = plane.drawn();
                    for succeeds in [false, true] {
                        // Failing an SCK update leaves the applied mode intact.
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
                    flips += 1;
                    composited = plane.drawn();
                }
            }
            assert!(flips <= 2); // One separate-plane entry, at most one fallback.
        }
    }
    #[test]
    fn stalled_samples_and_restart_discard_stale_geometry() {
        let mut plane = Plane::default();
        plane.accept(Some(snapshot(true)));
        assert!(!plane.drawn());
        let (_tx, rx) = mpsc::sync_channel(1);
        plane.pending = Some((Instant::now() - Duration::from_secs(1), rx));
        plane.sample(DisplayRect::default(), 1920, 1080);
        assert!(plane.drawn());
        plane.accept(Some(snapshot(true)));
        assert!(plane.drawn());
        let mut stale = snapshot(true);
        stale.sampled = Instant::now() - Duration::from_secs(1);
        plane.accept(Some(stale));
        assert!(plane.drawn());
        plane.reset();
        assert!(plane.pending.is_none());
        assert!(plane.snapshot.is_none());
        assert!(plane.drawn());
        plane.accept(Some(snapshot(true)));
        assert!(!plane.drawn());
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
                    source: vec![identity; 16 * 16 * 4],
                },
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
        plane.snapshot = Some(Snapshot {
            shape: CursorShape::new(0, 1, 1, 0, 0, &[255; 4]),
            visible: true,
            sampled: Instant::now(),
        });
        plane.safety.observe(true);
        assert!(!plane.drawn());
        plane.requested_drawn = true;
        assert!(plane.drawn());
        assert!(!plane.update(true).unwrap().visible);
    }
}
