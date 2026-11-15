//! Cursor pixels and change detection shared by native capture backends.
//! Windows can feed its decoded straight-alpha DXGI shape to `Tracker::update`.

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
    previous: Option<CursorUpdate>,
}
impl Tracker {
    /// `None` means native shape reading failed. Hide the local plane while
    /// capture falls back to composition. A later readable shape restores it.
    pub fn update(&mut self, visible: bool, shape: Option<CursorShape>) -> Option<CursorUpdate> {
        let visible = visible && shape.is_some();
        let changed = match (&self.previous, &shape) {
            (Some(old), Some(shape)) => old.shape.as_ref().is_none_or(|old| {
                (old.width, old.height, old.hot_x, old.hot_y, &old.rgba)
                    != (
                        shape.width,
                        shape.height,
                        shape.hot_x,
                        shape.hot_y,
                        &shape.rgba,
                    )
            }),
            (None, Some(_)) => true,
            _ => false,
        };
        if !changed
            && self
                .previous
                .as_ref()
                .is_some_and(|old| old.visible == visible)
        {
            return None;
        }
        let shape = if changed {
            // A pipeline can reopen inside the same viewer session. Its first
            // shape must invalidate the viewer's previous texture as well.
            static NEXT_SERIAL: AtomicU32 = AtomicU32::new(1);
            shape.map(|mut shape| {
                shape.serial = NEXT_SERIAL.fetch_add(1, Ordering::Relaxed);
                shape
            })
        } else {
            None
        };
        let update = CursorUpdate { visible, shape };
        self.previous = Some(CursorUpdate {
            visible,
            shape: update
                .shape
                .clone()
                .or_else(|| self.previous.as_ref().and_then(|old| old.shape.clone())),
        });
        Some(update)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
