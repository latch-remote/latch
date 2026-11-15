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
}
#[derive(Default)]
pub(super) struct Plane {
    pending: Option<(Instant, mpsc::Receiver<Option<Snapshot>>)>,
    snapshot: Option<Snapshot>,
    tracker: Tracker,
    pub requested_drawn: bool,
}
impl Plane {
    pub fn sample(&mut self, display: DisplayRect, width: u32, height: u32) {
        if let Some((started, receiver)) = &self.pending {
            match receiver.try_recv() {
                Ok(snapshot) => {
                    self.snapshot = snapshot;
                    self.pending = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.snapshot = None;
                    self.pending = None;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    if started.elapsed() > Duration::from_millis(250) {
                        self.snapshot = None;
                    }
                    return;
                }
            }
        }
        if MainThreadMarker::new().is_some() {
            self.snapshot = read(display, width, height);
        } else {
            let (tx, rx) = mpsc::sync_channel(1);
            dispatch2::DispatchQueue::main().exec_async(move || {
                let _ = tx.send(objc2::rc::autoreleasepool(|_| read(display, width, height)));
            });
            // Only the first negotiation waits briefly. Normal frames use the
            // last completed sample and never block the encoder on AppKit.
            if self.snapshot.is_none() {
                if let Ok(snapshot) = rx.recv_timeout(Duration::from_millis(40)) {
                    self.snapshot = snapshot;
                    return;
                }
            }
            self.pending = Some((Instant::now(), rx));
        }
    }
    pub fn drawn(&self) -> bool {
        self.requested_drawn || self.snapshot.is_none()
    }
    pub fn update(&mut self, composited: bool) -> Option<CursorUpdate> {
        self.tracker.update(
            !composited && self.snapshot.as_ref().is_some_and(|s| s.visible),
            self.snapshot.as_ref().map(|s| s.shape.clone()),
        )
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
        let mut proposed = CGRect::new(CGPoint::new(0.0, 0.0), size);
        // Opaque CGImage pointer has the same ABI as the returned CF reference.
        let cg: *const CGImage = msg_send![&*image, CGImageForProposedRect: &mut proposed, context: ptr::null::<AnyObject>(), hints: ptr::null::<AnyObject>()];
        if cg.is_null() {
            return None;
        }
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
        unpremultiply(&mut rgba);
        let event = Owned::new(CGEventCreate(ptr::null())).ok()?;
        let point = CGEventGetLocation(event.0);
        let on_display = point.x >= f64::from(display.x)
            && point.y >= f64::from(display.y)
            && point.x < f64::from(display.x) + f64::from(display.width)
            && point.y < f64::from(display.y) + f64::from(display.height);
        Some(Snapshot {
            visible: on_display
                && CGCursorIsVisible() != 0
                && rgba.as_chunks::<4>().0.iter().any(|p| p[3] != 0),
            shape: CursorShape::new(0, w, h, hx, hy, &rgba),
        })
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retina_hotspots_follow_the_selected_display_and_aspect_fit() {
        let display = DisplayRect {
            x: 1440,
            y: 0,
            width: 1440,
            height: 900,
        };
        let size = CGSize::new(32.0, 32.0);
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
        });
        assert!(!plane.drawn());
        plane.requested_drawn = true;
        assert!(plane.drawn());
        assert!(!plane.update(true).unwrap().visible);
    }
}
