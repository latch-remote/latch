//! Fused, damage-aware BGRA resampling and BT.709 conversion.
use super::{pool, rgb_to_cb_fast, rgb_to_cr_fast, rgb_to_y_fast, Planes, Rect};
use crate::Yuv420;

type Taps = Vec<(usize, u64)>;

/// Cached filter footprints. Reductions integrate pixel areas; enlargements
/// interpolate pixel centres. Empty footprints are the black letterbox bars.
pub struct Scaler {
    x: Vec<Taps>,
    y: Vec<Taps>,
    denominator: u64,
}

fn axis(source: u32, output: u32, start: u32, extent: u32) -> (Vec<Taps>, u64) {
    let mut taps = vec![Vec::new(); output as usize];
    let denominator = if source >= extent { source } else { 2 * extent };
    for i in 0..extent {
        let row = &mut taps[(start + i) as usize];
        if source >= extent {
            // Coordinates in units of 1/extent source pixels: exact rational box.
            let lo = u64::from(i) * u64::from(source);
            let hi = lo + u64::from(source);
            let step = u64::from(extent);
            for s in lo / step..hi.div_ceil(step) {
                row.push((s as usize, hi.min((s + 1) * step) - lo.max(s * step)));
            }
        } else {
            let d = i64::from(2 * extent);
            let centre = (i64::from(2 * i + 1) * i64::from(source) - i64::from(extent))
                .clamp(0, i64::from(source - 1) * d);
            let s = centre / d;
            let fraction = centre % d;
            row.push((s as usize, (d - fraction) as u64));
            if fraction > 0 {
                row.push(((s + 1) as usize, fraction as u64));
            }
        }
    }
    (taps, u64::from(denominator))
}

impl Scaler {
    pub fn new(source: (u32, u32), output: (u32, u32), content: Rect) -> Self {
        assert!(source.0 > 0 && source.1 > 0 && content.width > 0 && content.height > 0);
        assert!(content.x + content.width <= output.0 && content.y + content.height <= output.1);
        let (x, dx) = axis(source.0, output.0, content.x, content.width);
        let (y, dy) = axis(source.1, output.1, content.y, content.height);
        Self {
            x,
            y,
            denominator: dx * dy,
        }
    }

    /// Map source damage through the actual filter footprints, then expand to
    /// chroma pairs. This includes both neighbours of a bilinear sample and
    /// every partially covered box at the boundary of a reduction.
    pub fn damage(&self, rects: &[Rect]) -> Vec<Rect> {
        let affected = |axis: &[Taps], start: u32, length: u32| {
            let end = start.saturating_add(length) as usize;
            let mut hits = axis.iter().enumerate().filter_map(|(i, taps)| {
                taps.iter()
                    .any(|&(s, weight)| weight > 0 && s >= start as usize && s < end)
                    .then_some(i)
            });
            let first = hits.next()?;
            let last = hits.next_back().unwrap_or(first);
            Some((
                (first & !1) as u32,
                ((last + 2) & !1).min(axis.len()) as u32,
            ))
        };
        rects
            .iter()
            .filter_map(|r| {
                let (x, right) = affected(&self.x, r.x, r.width)?;
                let (y, bottom) = affected(&self.y, r.y, r.height)?;
                Some(Rect {
                    x,
                    y,
                    width: right - x,
                    height: bottom - y,
                })
            })
            .collect()
    }

    fn sample(&self, src: &[u8], stride: usize, x: usize, y: usize) -> [i32; 3] {
        if self.x[x].is_empty() || self.y[y].is_empty() {
            return [0; 3];
        }
        let mut rgb = [0u64; 3];
        for &(sy, wy) in &self.y[y] {
            for &(sx, wx) in &self.x[x] {
                let p = sy * stride + sx * 4;
                for c in 0..3 {
                    rgb[c] += u64::from(src[p + 2 - c]) * wx * wy;
                }
            }
        }
        rgb.map(|v| ((v + self.denominator / 2) / self.denominator) as i32)
    }

    /// `rects` are output-space damage from `damage`, or the full output
    /// on initialization. Bands share no mutable pixels; the existing kept
    /// conversion worker pool handles them just as it does unscaled pictures.
    pub fn convert(
        &self,
        src: &[u8],
        stride: usize,
        rects: &[Rect],
        threads: usize,
        dst: &mut Yuv420,
    ) {
        let (w, h) = (self.x.len(), self.y.len());
        assert_eq!((dst.width as usize, dst.height as usize), (w, h));
        let area: u64 = rects.iter().map(Rect::area).sum();
        let threads = threads.clamp(1, 16).min(area as usize / w / 64).max(1);
        let pairs = h.div_ceil(2);
        let (ys, cs) = (dst.y_stride, dst.c_stride);
        let (mut yr, mut ur, mut vr) = (&mut dst.y[..], &mut dst.u[..], &mut dst.v[..]);
        let mut bands = Vec::new();
        for i in 0..threads {
            let (first, end) = (i * pairs / threads, (i + 1) * pairs / threads);
            let n = end - first;
            let (yn, cn) = ((n * 2 * ys).min(yr.len()), (n * cs).min(ur.len()));
            let (y, next_y) = std::mem::take(&mut yr).split_at_mut(yn);
            let (u, next_u) = std::mem::take(&mut ur).split_at_mut(cn);
            let (v, next_v) = std::mem::take(&mut vr).split_at_mut(cn);
            (yr, ur, vr) = (next_y, next_u, next_v);
            bands.push(std::sync::Mutex::new(Planes {
                y,
                u,
                v,
                y_stride: ys,
                c_stride: cs,
                row0: first * 2,
                rows: first * 2..(end * 2).min(h),
            }));
        }
        pool::run(bands.len(), &|i| {
            let mut p = bands[i].lock().unwrap_or_else(|e| e.into_inner());
            for r in rects {
                let y0 = (r.y as usize & !1).max(p.rows.start);
                let y1 = (r.y.saturating_add(r.height) as usize).min(p.rows.end);
                let x0 = r.x as usize & !1;
                let x1 = (r.x.saturating_add(r.width) as usize).min(w);
                for y in (y0..y1).step_by(2) {
                    for x in (x0..x1).step_by(2) {
                        let mut sum = [0; 3];
                        for dy in 0..2 {
                            for dx in 0..2 {
                                let (sx, sy) = ((x + dx).min(w - 1), (y + dy).min(h - 1));
                                let rgb = self.sample(src, stride, sx, sy);
                                if x + dx < w && y + dy < h {
                                    let at = (sy - p.row0) * ys + sx;
                                    p.y[at] = rgb_to_y_fast(rgb[0], rgb[1], rgb[2]);
                                }
                                for c in 0..3 {
                                    sum[c] += rgb[c];
                                }
                            }
                        }
                        let [r, g, b] = sum.map(|v| (v + 2) >> 2);
                        let at = (y - p.row0) / 2 * cs + x / 2;
                        p.u[at] = rgb_to_cb_fast(r, g, b);
                        p.v[at] = rgb_to_cr_fast(r, g, b);
                    }
                }
            }
        });
        drop(bands);
        dst.pad_edges();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn full(w: u32, h: u32) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }
    }
    #[test]
    fn integer_boxes_and_bilinear_edges_are_exact() {
        let s = Scaler::new((4, 4), (2, 2), full(2, 2));
        let src: Vec<u8> = (0..16).flat_map(|n| [n * 8, n * 8, n * 8, 255]).collect();
        assert_eq!(s.sample(&src, 16, 0, 0), [20; 3]);
        assert_eq!(s.sample(&src, 16, 1, 1), [100; 3]);
        let odd = Scaler::new((3, 1), (2, 1), full(2, 1));
        let pixels = [0, 0, 0, 255, 60, 60, 60, 255, 120, 120, 120, 255];
        assert_eq!(odd.sample(&pixels, 12, 0, 0), [20; 3]);
        assert_eq!(odd.sample(&pixels, 12, 1, 0), [100; 3]);
        let mut picture = Yuv420::new(2, 1);
        odd.convert(&pixels, 12, &[full(2, 1)], 1, &mut picture);
        assert_eq!(&picture.y[..2], &[33, 102]);
        assert_eq!((picture.u[0], picture.v[0]), (128, 128));
        let s = Scaler::new((2, 1), (4, 1), full(4, 1));
        let src = [0, 0, 0, 255, 100, 100, 100, 255];
        for (x, expected) in [0, 25, 75, 100].into_iter().enumerate() {
            assert_eq!(s.sample(&src, 8, x, 0), [expected; 3]);
        }
    }
    #[test]
    fn every_damaged_pixel_matches_a_full_rescale_including_odd_sizes_and_bars() {
        for (sw, sh, w, h, content) in [
            (7, 5, 3, 3, full(3, 3)),
            (3, 3, 7, 5, full(7, 5)),
            (
                8,
                6,
                4,
                4,
                Rect {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 3,
                },
            ),
            (
                3,
                5,
                9,
                9,
                Rect {
                    x: 2,
                    y: 0,
                    width: 5,
                    height: 9,
                },
            ),
            (17, 269, 11, 193, full(11, 193)),
        ] {
            let scale = Scaler::new((sw, sh), (w, h), content);
            let stride = sw as usize * 4 + 7;
            let src: Vec<u8> = (0..stride * sh as usize).map(|i| (i * 79) as u8).collect();
            let mut old = Yuv420::new(w, h);
            scale.convert(&src, stride, &[full(w, h)], 4, &mut old);
            for y in 0..sh {
                for x in 0..sw {
                    let mut changed = src.clone();
                    let at = y as usize * stride + x as usize * 4;
                    changed[at..at + 3].fill(255);
                    let mut expected = Yuv420::new(w, h);
                    scale.convert(&changed, stride, &[full(w, h)], 1, &mut expected);
                    let mut actual = old.clone();
                    let damage = scale.damage(&[Rect {
                        x,
                        y,
                        width: 1,
                        height: 1,
                    }]);
                    scale.convert(&changed, stride, &damage, 8, &mut actual);
                    assert_eq!(actual, expected, "{sw}x{sh} -> {w}x{h}, damage {x},{y}");
                }
            }
        }
    }
}
