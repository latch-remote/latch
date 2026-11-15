//! Cursor pixels and change detection shared by native capture backends.
//! Windows feeds decoded DXGI shapes through `set_shape` and publishes visibility
//! independently. Mac snapshots use `update` with the same serial allocator.

use crate::video::CursorUpdate;
use latch_proto::control::CursorShape;
use std::sync::atomic::{AtomicU32, Ordering};

/// Convert premultiplied RGBA into the protocol's straight-alpha RGBA.
pub(crate) fn unpremultiply(rgba: &mut [u8]) {
    for pixel in rgba.as_chunks_mut::<4>().0 {
        let alpha = u32::from(pixel[3]);
        for channel in &mut pixel[..3] {
            *channel = (u32::from(*channel) * 255 + alpha / 2)
                .checked_div(alpha)
                .unwrap_or(0)
                .min(255) as u8;
        }
    }
}

#[derive(Default)]
pub(crate) struct Tracker {
    shape: Option<CursorShape>,
    readable: bool,
    previous: Option<(bool, u32)>,
}
impl Tracker {
    /// `None` means native shape reading failed. Hide the local plane while
    /// capture falls back to composition. A later readable shape restores it.
    #[cfg(any(target_os = "macos", test))]
    pub fn update(&mut self, visible: bool, shape: Option<CursorShape>) -> Option<CursorUpdate> {
        self.set_shape(shape);
        self.publish(visible)
    }
    /// Native backends call this only when pixels or their scaling change.
    /// Visibility-only ticks can then publish without copying/comparing pixels.
    pub fn set_shape(&mut self, shape: Option<CursorShape>) {
        self.readable = shape.is_some();
        let Some(mut shape) = shape else {
            return;
        };
        let changed = self.shape.as_ref().is_none_or(|old| {
            (old.width, old.height, old.hot_x, old.hot_y, &old.rgba)
                != (
                    shape.width,
                    shape.height,
                    shape.hot_x,
                    shape.hot_y,
                    &shape.rgba,
                )
        });
        if changed {
            // Capture pipelines share one serial namespace across reopen.
            static NEXT_SERIAL: AtomicU32 = AtomicU32::new(1);
            shape.serial = NEXT_SERIAL.fetch_add(1, Ordering::Relaxed);
            self.shape = Some(shape);
        }
    }
    #[cfg(any(windows, test))]
    pub fn shape(&self) -> Option<&CursorShape> {
        self.shape.as_ref().filter(|_| self.readable)
    }
    pub fn publish(&mut self, visible: bool) -> Option<CursorUpdate> {
        let visible = visible && self.readable;
        let serial = self.shape.as_ref().map_or(0, |shape| shape.serial);
        if self.previous == Some((visible, serial)) {
            return None;
        }
        let changed = self.previous.is_none_or(|(_, previous)| previous != serial);
        self.previous = Some((visible, serial));
        Some(CursorUpdate {
            visible,
            shape: if changed { self.shape.clone() } else { None },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn split_native_updates_share_serials_and_cache_pixels_with_combined_updates() {
        let shape = CursorShape::new(0, 1, 1, 0, 0, &[255; 4]);
        let mac = Tracker::default()
            .update(true, Some(shape.clone()))
            .unwrap();
        let mut windows = Tracker::default();
        windows.set_shape(Some(shape.clone()));
        let pixels = windows.shape().unwrap().rgba.as_ptr();
        let first = windows.publish(true).unwrap();
        assert_ne!(
            first.shape.as_ref().unwrap().serial,
            mac.shape.unwrap().serial
        );
        assert!(windows.publish(true).is_none());
        let mut repeated = shape.clone();
        repeated.serial = first.shape.unwrap().serial.wrapping_add(100);
        windows.set_shape(Some(repeated));
        assert!(windows.publish(true).is_none());
        assert_eq!(windows.shape().unwrap().rgba.as_ptr(), pixels);
        let hidden = windows.publish(false).unwrap();
        assert!(!hidden.visible && hidden.shape.is_none());
        windows.set_shape(None);
        assert!(windows.shape().is_none());
        assert!(windows.publish(true).is_none());
        windows.set_shape(Some(shape));
        let recovered = windows.publish(true).unwrap();
        assert!(recovered.visible && recovered.shape.is_none());
        assert_eq!(windows.shape().unwrap().rgba.as_ptr(), pixels);
    }
    #[test]
    fn alpha_conversion_clears_transparency_and_rounds() {
        let mut rgba = [64, 32, 16, 128, 5, 6, 7, 0, 255, 12, 9, 255];
        unpremultiply(&mut rgba);
        assert_eq!(rgba, [128, 64, 32, 128, 0, 0, 0, 0, 255, 12, 9, 255]);
    }
    #[test]
    fn a_reopened_pipeline_always_invalidates_the_previous_texture() {
        let shape = CursorShape::new(0, 1, 1, 0, 0, &[255; 4]);
        let mut first = Tracker::default();
        let mut reopened = Tracker::default();
        let before = first
            .update(true, Some(shape.clone()))
            .unwrap()
            .shape
            .unwrap()
            .serial;
        let after = reopened
            .update(true, Some(shape))
            .unwrap()
            .shape
            .unwrap()
            .serial;
        assert_ne!(before, after);
    }
    #[test]
    fn unreadable_shape_hides_local_plane_and_recovers_without_resending_pixels() {
        let shape = CursorShape::new(0, 1, 1, 0, 0, &[255; 4]);
        let mut tracker = Tracker::default();
        tracker.update(true, Some(shape.clone()));
        let fallback = tracker.update(true, None).unwrap();
        assert!(!fallback.visible && fallback.shape.is_none());
        assert!(tracker.update(true, None).is_none());
        let recovered = tracker.update(true, Some(shape)).unwrap();
        assert!(recovered.visible && recovered.shape.is_none());
    }
    #[test]
    fn visibility_fallback_and_shape_changes_are_independent() {
        let shape = CursorShape::new(0, 1, 1, 0, 0, &[255; 4]);
        let mut tracker = Tracker::default();
        let first_serial = tracker
            .update(true, Some(shape.clone()))
            .unwrap()
            .shape
            .unwrap()
            .serial;
        assert!(tracker.update(true, Some(shape.clone())).is_none());
        let hidden = tracker.update(false, Some(shape.clone())).unwrap();
        assert!(!hidden.visible && hidden.shape.is_none());
        assert!(tracker.update(true, None).is_none());
        assert!(tracker.update(true, Some(shape.clone())).unwrap().visible);
        let mut changed = shape;
        changed.hot_x = 1;
        assert_ne!(
            tracker
                .update(true, Some(changed))
                .unwrap()
                .shape
                .unwrap()
                .serial,
            first_serial
        );
    }
}
