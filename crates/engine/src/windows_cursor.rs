//! DXGI pointer buffers and plane decisions, kept platform independent so CI
//! exercises malformed buffers and XOR cursors on every OS.
use latch_proto::control::{Cursor, CursorShape};
use parking_lot::Mutex;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};

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

#[derive(Default)]
pub(crate) struct Control {
    pub drawn: AtomicBool,
    pub sink: Mutex<Option<crate::video::CursorSink>>,
}
impl Control {
    pub fn composited() -> Arc<Self> {
        Arc::new(Self {
            drawn: AtomicBool::new(true),
            ..Default::default()
        })
    }
}

pub(crate) struct Plane {
    pub control: Arc<Control>,
    pub shape: Option<CursorShape>,
    shape_visible: bool,
    last: Option<Cursor>,
    serial: u32,
}
impl Plane {
    pub fn new(control: Arc<Control>) -> Self {
        Self {
            control,
            shape: None,
            shape_visible: false,
            last: None,
            serial: 0,
        }
    }
    pub fn shape(&mut self, info: ShapeInfo, bytes: &[u8], source: (u32, u32), stream: (u32, u32)) {
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
        let current = Cursor {
            visible: visible && separate && self.shape_visible,
            position: Some(position),
            shape: self.shape.clone(),
        };
        if self.last.as_ref() == Some(&current) {
            return;
        }
        let Some(sink) = self.control.sink.lock().clone() else {
            return;
        };
        let mut update = current.clone();
        if self.last.as_ref().and_then(|c| c.shape.as_ref()) == current.shape.as_ref() {
            update.shape = None;
        }
        sink(update);
        self.last = Some(current);
    }
    pub fn invalidate(&mut self) {
        self.shape = None;
        self.shape_visible = false;
        self.publish(false, [0, 0], false);
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
        plane.publish(true, [10, 20], true);
        let first = updates.lock()[0].shape.clone().unwrap();
        assert_eq!(
            (first.width, first.height, first.hot_x, first.hot_y),
            (2, 2, 1, 1)
        );
        plane.shape(metadata, &[255; 80], (3840, 2160), (1920, 1080));
        plane.publish(true, [11, 20], true);
        plane.publish(true, [11, 20], true);
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
