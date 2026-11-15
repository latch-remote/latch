//! The screencast's latest picture, kept as BGRA: what PipeWire's buffers
//! bring is copied in where their damage says, and handed to the encoder
//! with the damage since the caller's last picture.

use super::pod::PixelFormat;
use crate::software::{Bgra, Rect};

/// More rectangles than this are kept as their bounding box.
pub const MAX_RECTS: usize = 16;

/// One PipeWire buffer's pixels, as mapped.
pub struct Source<'a> {
    pub data: &'a [u8],
    pub stride: usize,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
}

/// The rectangles of `regions` (x, y, width, height) inside
/// `width` × `height`, or `None` (everything) when there are none.
/// Producers send damage in buffer pixels; anything outside is clipped.
pub fn clip(regions: &[(i32, i32, u32, u32)], width: u32, height: u32) -> Option<Vec<Rect>> {
    let rects: Vec<Rect> = regions
        .iter()
        .filter_map(|&(x, y, w, h)| {
            let x0 = i64::from(x).clamp(0, i64::from(width));
            let y0 = i64::from(y).clamp(0, i64::from(height));
            let x1 = (i64::from(x) + i64::from(w)).clamp(0, i64::from(width));
            let y1 = (i64::from(y) + i64::from(h)).clamp(0, i64::from(height));
            (x1 > x0 && y1 > y0).then(|| Rect {
                x: x0 as u32,
                y: y0 as u32,
                width: (x1 - x0) as u32,
                height: (y1 - y0) as u32,
            })
        })
        .collect();
    (!rects.is_empty()).then_some(rects)
}

fn bounding(rects: &[Rect]) -> Rect {
    let x0 = rects.iter().map(|r| r.x).min().unwrap_or(0);
    let y0 = rects.iter().map(|r| r.y).min().unwrap_or(0);
    let x1 = rects.iter().map(|r| r.x + r.width).max().unwrap_or(0);
    let y1 = rects.iter().map(|r| r.y + r.height).max().unwrap_or(0);
    Rect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    }
}

/// Adds `more` to `pending` damage; `None` on either side is everything.
pub fn merge(pending: &mut Option<Vec<Rect>>, more: Option<&[Rect]>) {
    match (pending.as_mut(), more) {
        (None, _) => {}
        (Some(_), None) => *pending = None,
        (Some(rects), Some(more)) => {
            rects.extend_from_slice(more);
            if rects.len() > MAX_RECTS {
                *rects = vec![bounding(rects)];
            }
        }
    }
}

/// The latest picture and what changed in it since it was last taken.
#[derive(Default)]
pub struct Desktop {
    pub picture: Bgra,
    /// Since the last [`Desktop::take`]: `None` is everything.
    pending: Option<Vec<Rect>>,
    /// A picture came since the last take.
    pub fresh: bool,
    /// The caller's buffer the last take filled, to give it only changes.
    filled: Option<(usize, usize)>,
    oriented: Vec<u8>,
}

impl Desktop {
    /// Copies the `damage` of `source` in (all of it when `None`, or when
    /// the size changed), converting to BGRA.
    pub fn put(&mut self, source: &Source<'_>, damage: Option<&[Rect]>) -> anyhow::Result<()> {
        let (w, h) = (source.width, source.height);
        anyhow::ensure!(w > 0 && h > 0, "empty screencast picture");
        anyhow::ensure!(source.stride >= w as usize * 4, "short screencast stride");
        let need = source.stride * (h as usize - 1) + w as usize * 4;
        anyhow::ensure!(
            source.data.len() >= need,
            "screencast buffer of {} bytes holds no {w} × {h} picture",
            source.data.len()
        );
        let resized = (self.picture.width, self.picture.height) != (w, h);
        if resized {
            self.picture = Bgra {
                width: w,
                height: h,
                stride: w as usize * 4,
                data: vec![0; w as usize * h as usize * 4],
            };
            self.pending = None;
            self.filled = None;
        }
        let whole = [Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }];
        let rects = match damage {
            Some(d) if !resized => d,
            _ => &whole[..],
        };
        let stride = self.picture.stride;
        for r in rects {
            let (x, width) = (r.x as usize * 4, r.width as usize * 4);
            for row in r.y as usize..(r.y + r.height) as usize {
                let from = &source.data[row * source.stride + x..row * source.stride + x + width];
                let to = &mut self.picture.data[row * stride + x..row * stride + x + width];
                if source.format.swaps_red_and_blue() {
                    for (to, from) in to
                        .as_chunks_mut::<4>()
                        .0
                        .iter_mut()
                        .zip(from.as_chunks::<4>().0)
                    {
                        *to = [from[2], from[1], from[0], from[3]];
                    }
                } else {
                    to.copy_from_slice(from);
                }
            }
        }
        if !resized {
            merge(&mut self.pending, damage);
        }
        self.fresh = true;
        Ok(())
    }

    /// SPA's transform describes the buffer orientation. Undo it to present
    /// the compositor's logical desktop, including reflected outputs.
    pub fn put_transformed(
        &mut self,
        source: &Source<'_>,
        damage: Option<&[Rect]>,
        transform: u32,
    ) -> anyhow::Result<()> {
        if transform == 0 {
            return self.put(source, damage);
        }
        anyhow::ensure!(transform < 8, "unknown screencast transform {transform}");
        let (w, h) = (source.width as usize, source.height as usize);
        anyhow::ensure!(
            w > 0 && h > 0 && source.stride >= w * 4,
            "invalid screencast stride"
        );
        anyhow::ensure!(
            source.data.len() >= source.stride * (h - 1) + w * 4,
            "short screencast buffer"
        );
        let (width, height) = if transform & 1 != 0 { (h, w) } else { (w, h) };
        let mut data = std::mem::take(&mut self.oriented);
        data.resize(width * height * 4, 0);
        for y in 0..h {
            for x in 0..w {
                let (mut dx, dy) = match transform & 3 {
                    0 => (x, y),
                    1 => (h - 1 - y, x),
                    2 => (w - 1 - x, h - 1 - y),
                    _ => (y, w - 1 - x),
                };
                if transform & 4 != 0 {
                    dx = width - 1 - dx;
                }
                let from = y * source.stride + x * 4;
                let to = (dy * width + dx) * 4;
                data[to..to + 4].copy_from_slice(&source.data[from..from + 4]);
            }
        }
        // Buffer-space damage changes axes under rotation. A full copy also
        // handles a transform change at the same negotiated buffer size.
        let result = self.put(
            &Source {
                data: &data,
                stride: width * 4,
                width: width as u32,
                height: height as u32,
                format: source.format,
            },
            None,
        );
        self.oriented = data;
        result
    }

    /// Brings `frame` up to the latest picture: only what changed when
    /// `frame` is the buffer the last take filled. Returns the damage
    /// (`None`: all of it) for [`crate::software::CpuCapture::damage`].
    pub fn take(&mut self, frame: &mut Bgra) -> Option<Vec<Rect>> {
        let same = self.filled == Some((frame.data.as_ptr() as usize, frame.data.len()))
            && (frame.width, frame.height, frame.stride)
                == (self.picture.width, self.picture.height, self.picture.stride);
        let damage = match (same, self.pending.take()) {
            (true, Some(rects)) => {
                let stride = frame.stride;
                for r in &rects {
                    let (x, width) = (r.x as usize * 4, r.width as usize * 4);
                    for row in r.y as usize..(r.y + r.height) as usize {
                        let at = row * stride + x;
                        frame.data[at..at + width]
                            .copy_from_slice(&self.picture.data[at..at + width]);
                    }
                }
                Some(rects)
            }
            _ => {
                frame.width = self.picture.width;
                frame.height = self.picture.height;
                frame.stride = self.picture.stride;
                frame.data.clone_from(&self.picture.data);
                None
            }
        };
        self.pending = Some(Vec::new());
        self.fresh = false;
        self.filled = Some((frame.data.as_ptr() as usize, frame.data.len()));
        damage
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(data: &[u8], width: u32, height: u32, format: PixelFormat) -> Source<'_> {
        Source {
            data,
            stride: width as usize * 4 + 8,
            width,
            height,
            format,
        }
    }

    fn padded(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
        let stride = width as usize * 4 + 8;
        let mut data = vec![0xEE; stride * height as usize];
        for y in 0..height {
            for x in 0..width {
                let at = y as usize * stride + x as usize * 4;
                data[at..at + 4].copy_from_slice(&pixel(x, y));
            }
        }
        data
    }

    #[test]
    fn all_output_transforms_restore_the_logical_picture() {
        let pixels: Vec<u8> = (1..=6).flat_map(|v| [v, 0, 0, 255]).collect();
        let source = Source {
            data: &pixels,
            stride: 12,
            width: 3,
            height: 2,
            format: PixelFormat::Bgra,
        };
        let expected = [
            vec![1, 2, 3, 4, 5, 6],
            vec![4, 1, 5, 2, 6, 3],
            vec![6, 5, 4, 3, 2, 1],
            vec![3, 6, 2, 5, 1, 4],
            vec![3, 2, 1, 6, 5, 4],
            vec![1, 4, 2, 5, 3, 6],
            vec![4, 5, 6, 1, 2, 3],
            vec![6, 3, 5, 2, 4, 1],
        ];
        for (transform, expected) in expected.iter().enumerate() {
            let mut desktop = Desktop::default();
            desktop
                .put_transformed(&source, None, transform as u32)
                .unwrap();
            assert_eq!(
                desktop
                    .picture
                    .data
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|p| p[0])
                    .collect::<Vec<_>>(),
                *expected
            );
            assert_eq!(
                (desktop.picture.width, desktop.picture.height),
                if transform & 1 == 0 { (3, 2) } else { (2, 3) }
            );
        }
    }

    #[test]
    fn damage_is_clipped_to_the_picture() {
        assert_eq!(clip(&[], 10, 10), None);
        assert_eq!(clip(&[(20, 20, 5, 5), (0, 0, 0, 3)], 10, 10), None);
        assert_eq!(
            clip(&[(-2, 8, 5, 5)], 10, 10),
            Some(vec![Rect {
                x: 0,
                y: 8,
                width: 3,
                height: 2
            }])
        );
        assert_eq!(
            clip(&[(i32::MAX, 0, u32::MAX, 1)], 10, 10),
            None,
            "no overflow"
        );
    }

    #[test]
    fn many_rectangles_become_their_bounding_box_and_none_is_everything() {
        let r = |x| Rect {
            x,
            y: x,
            width: 1,
            height: 1,
        };
        let mut pending = Some(Vec::new());
        merge(&mut pending, Some(&[r(1), r(3)]));
        assert_eq!(pending.as_ref().unwrap().len(), 2);
        let many: Vec<_> = (0..MAX_RECTS as u32).map(|i| r(i + 5)).collect();
        merge(&mut pending, Some(&many));
        assert_eq!(
            pending,
            Some(vec![Rect {
                x: 1,
                y: 1,
                width: MAX_RECTS as u32 + 4,
                height: MAX_RECTS as u32 + 4
            }])
        );
        merge(&mut pending, None);
        assert_eq!(pending, None);
        merge(&mut pending, Some(&[r(0)]));
        assert_eq!(pending, None, "everything stays everything");
    }

    #[test]
    fn rgbx_is_turned_into_bgra_and_padding_is_skipped() {
        let data = padded(3, 2, |x, y| [x as u8, y as u8, 0x80, 0xFF]);
        let mut desktop = Desktop::default();
        desktop
            .put(&source(&data, 3, 2, PixelFormat::Rgbx), None)
            .unwrap();
        assert_eq!(desktop.picture.stride, 12);
        assert_eq!(&desktop.picture.data[..4], &[0x80, 0, 0, 0xFF]);
        assert_eq!(&desktop.picture.data[12 + 8..12 + 12], &[0x80, 1, 2, 0xFF]);
        let bgrx = padded(3, 2, |x, y| [x as u8, y as u8, 0x80, 0xFF]);
        let mut same = Desktop::default();
        same.put(&source(&bgrx, 3, 2, PixelFormat::Bgrx), None)
            .unwrap();
        assert_eq!(&same.picture.data[12 + 8..12 + 12], &[2, 1, 0x80, 0xFF]);
    }

    #[test]
    fn the_same_buffer_gets_only_the_damage_and_another_gets_everything() {
        let mut desktop = Desktop::default();
        let first = padded(8, 4, |_, _| [1, 1, 1, 1]);
        desktop
            .put(&source(&first, 8, 4, PixelFormat::Bgra), Some(&[]))
            .unwrap();
        let mut frame = Bgra::default();
        assert_eq!(desktop.take(&mut frame), None, "the first take is whole");
        assert!(!desktop.fresh);
        assert_eq!(frame.data, vec![1; 8 * 4 * 4]);

        let second = padded(8, 4, |_, _| [2, 2, 2, 2]);
        let damage = [Rect {
            x: 2,
            y: 1,
            width: 3,
            height: 2,
        }];
        desktop
            .put(&source(&second, 8, 4, PixelFormat::Bgra), Some(&damage))
            .unwrap();
        assert!(desktop.fresh);
        // Only the damage was copied from the producer's buffer.
        assert_eq!(desktop.picture.data[0], 1);
        assert_eq!(desktop.picture.data[32 + 8], 2);
        assert_eq!(desktop.take(&mut frame), Some(damage.to_vec()));
        assert_eq!(frame.data, desktop.picture.data);

        let mut other = Bgra::default();
        assert_eq!(desktop.take(&mut other), None, "another buffer is whole");
        assert_eq!(other.data, desktop.picture.data);
    }

    #[test]
    fn a_new_size_is_copied_whole_whatever_the_damage_says() {
        let mut desktop = Desktop::default();
        let small = padded(4, 2, |_, _| [3; 4]);
        desktop
            .put(&source(&small, 4, 2, PixelFormat::Bgrx), None)
            .unwrap();
        let mut frame = Bgra::default();
        desktop.take(&mut frame);
        let big = padded(6, 3, |_, _| [4; 4]);
        let tiny = [Rect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        }];
        desktop
            .put(&source(&big, 6, 3, PixelFormat::Bgrx), Some(&tiny))
            .unwrap();
        assert_eq!(desktop.take(&mut frame), None);
        assert_eq!((frame.width, frame.height), (6, 3));
        assert!(frame.data.iter().all(|&b| b == 4));
    }

    #[test]
    fn a_short_buffer_is_an_error() {
        let mut desktop = Desktop::default();
        let data = vec![0; 10];
        assert!(desktop
            .put(&source(&data, 4, 4, PixelFormat::Bgrx), None)
            .is_err());
        let mut narrow = source(&data, 4, 1, PixelFormat::Bgrx);
        narrow.stride = 8;
        assert!(desktop.put(&narrow, None).is_err());
    }
}
