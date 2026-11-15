//! HEVC for VA-API: parameter sets and slice segment headers parsed, the
//! reference picture set applied (ITU-T H.265 8.3), and each access unit
//! turned into VA picture, matrix and slice parameters.
//!
//! Main and Main 10 in 4:2:0: I, P and B slices, short- and long-term
//! references, list modification, weighted prediction, scaling lists,
//! tiles, wavefronts and dependent slice segments. Range and screen
//! content extensions are refused. Pictures are given out in decoding
//! order; RASL pictures after a CRA the stream starts at are skipped.

use super::bits::{ceil_log2, emulation_prevention_before, nal_units, rbsp, Bits};
use super::va::{self, pack, Id, PictureHevc};
use super::{hardware, Backend, Decoded, Format, Profile, Rect};
use anyhow::{bail, ensure, Context, Result};

const IDR_W_RADL: u32 = 19;
const IDR_N_LP: u32 = 20;
const EOS: u32 = 36;

// Table 7-6, in up-right diagonal order.
const DEFAULT_INTRA: [u8; 64] = [
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 16, 17, 16, 17, 18, 17, 18, 18, 17, 18, 21, 19, 20,
    21, 20, 19, 21, 24, 22, 22, 24, 24, 22, 22, 24, 25, 25, 27, 30, 27, 25, 25, 29, 31, 35, 35, 31,
    29, 36, 41, 44, 41, 36, 47, 54, 54, 47, 65, 70, 65, 88, 88, 115,
];
const DEFAULT_INTER: [u8; 64] = [
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 17, 17, 17, 17, 18, 18, 18, 18, 18, 18, 20, 20, 20,
    20, 20, 20, 20, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 28, 28, 28, 28, 28,
    28, 33, 33, 33, 33, 33, 41, 41, 41, 41, 54, 54, 54, 71, 71, 91,
];

/// Raster position of each coefficient in up-right diagonal order (6.5.3).
fn diagonal(size: usize) -> Vec<usize> {
    let mut out = Vec::with_capacity(size * size);
    let (mut x, mut y) = (0i32, 0i32);
    while out.len() < size * size {
        while y >= 0 {
            if (x as usize) < size && (y as usize) < size {
                out.push(y as usize * size + x as usize);
            }
            y -= 1;
            x += 1;
        }
        y = x;
        x = 0;
    }
    out
}

/// Scaling lists as coded (diagonal order): `[size][matrix]`, with the
/// DC values of 16×16 and 32×32.
#[derive(Clone, Debug, PartialEq)]
struct ScalingLists {
    lists: [[[u8; 64]; 6]; 4],
    dc: [[u8; 6]; 2],
}

impl ScalingLists {
    fn defaults() -> Self {
        let mut s = Self {
            lists: [[[16; 64]; 6]; 4],
            dc: [[16; 6]; 2],
        };
        for lists in s.lists.iter_mut().skip(1) {
            for (m, list) in lists.iter_mut().enumerate() {
                *list = if m < 3 { DEFAULT_INTRA } else { DEFAULT_INTER };
            }
        }
        s
    }

    fn parse(b: &mut Bits) -> Result<Self> {
        let mut s = Self::defaults();
        for size in 0..4 {
            let step = if size == 3 { 3 } else { 1 };
            let coefficients = (1usize << (4 + (size << 1))).min(64);
            for m in (0..6).step_by(step) {
                if !b.flag()? {
                    let delta = b
                        .ue_max(m as u32 / step as u32, "scaling_list_pred_matrix_id_delta")?
                        as usize;
                    if delta != 0 {
                        let from = m - delta * step;
                        s.lists[size][m] = s.lists[size][from];
                        if size > 1 {
                            s.dc[size - 2][m] = s.dc[size - 2][from];
                        }
                    } else {
                        let default = Self::defaults();
                        s.lists[size][m] = default.lists[size][m];
                        if size > 1 {
                            s.dc[size - 2][m] = 16;
                        }
                    }
                } else {
                    let mut next = 8i32;
                    if size > 1 {
                        let dc = b.se()?;
                        ensure!((-7..=247).contains(&dc), "scaling list DC out of range");
                        next = dc + 8;
                        s.dc[size - 2][m] = next as u8;
                    }
                    for coefficient in s.lists[size][m].iter_mut().take(coefficients) {
                        let delta = b.se()?;
                        ensure!(
                            (-128..=127).contains(&delta),
                            "scaling list delta out of range"
                        );
                        next = (next + delta + 256) % 256;
                        *coefficient = next as u8;
                    }
                }
            }
            if size == 3 {
                // 4:2:0 has no chroma 32×32 lists; their slots copy luma's.
                for m in [1, 2, 4, 5] {
                    s.lists[3][m] = s.lists[3][m - m % 3];
                    s.dc[1][m] = s.dc[1][m - m % 3];
                }
            }
        }
        Ok(s)
    }

    fn va(&self) -> va::IqMatrixHevc {
        let (d4, d8) = (diagonal(4), diagonal(8));
        let mut iq: va::IqMatrixHevc = va::zeroed();
        for m in 0..6 {
            for (i, &position) in d4.iter().enumerate() {
                iq.ScalingList4x4[m][position] = self.lists[0][m][i];
            }
            for (i, &position) in d8.iter().enumerate() {
                iq.ScalingList8x8[m][position] = self.lists[1][m][i];
                iq.ScalingList16x16[m][position] = self.lists[2][m][i];
            }
            iq.ScalingListDC16x16[m] = self.dc[0][m];
        }
        for (k, m) in [0, 3].into_iter().enumerate() {
            for (i, &position) in d8.iter().enumerate() {
                iq.ScalingList32x32[k][position] = self.lists[3][m][i];
            }
            iq.ScalingListDC32x32[k] = self.dc[1][m];
        }
        iq
    }
}

/// A short-term reference picture set: POC deltas and whether the
/// current picture uses each, nearest first.
#[derive(Clone, Debug, Default, PartialEq)]
struct Rps {
    negative: Vec<(i32, bool)>,
    positive: Vec<(i32, bool)>,
}

impl Rps {
    /// `st_ref_pic_set(idx)` (7.3.7, 7.4.8). `sets` are the SPS's; `idx`
    /// equal to their number is a slice header's own set.
    fn parse(b: &mut Bits, idx: usize, sets: &[Rps]) -> Result<Self> {
        if idx != 0 && b.flag()? {
            let delta_idx = if idx == sets.len() {
                b.ue_max(idx as u32 - 1, "delta_idx_minus1")? as usize + 1
            } else {
                1
            };
            let reference = &sets[idx - delta_idx];
            let sign = b.bit()?;
            let abs = b.ue_max(0x7fff, "abs_delta_rps_minus1")? as i32 + 1;
            let delta_rps = if sign == 1 { -abs } else { abs };
            let n = reference.negative.len() + reference.positive.len();
            let mut used = Vec::with_capacity(n + 1);
            let mut use_delta = Vec::with_capacity(n + 1);
            for _ in 0..=n {
                let u = b.flag()?;
                used.push(u);
                use_delta.push(u || b.flag()?);
            }
            let neg_len = reference.negative.len();
            let mut rps = Rps::default();
            for j in (0..reference.positive.len()).rev() {
                let d = reference.positive[j].0 + delta_rps;
                if d < 0 && use_delta[neg_len + j] {
                    rps.negative.push((d, used[neg_len + j]));
                }
            }
            if delta_rps < 0 && use_delta[n] {
                rps.negative.push((delta_rps, used[n]));
            }
            for j in 0..neg_len {
                let d = reference.negative[j].0 + delta_rps;
                if d < 0 && use_delta[j] {
                    rps.negative.push((d, used[j]));
                }
            }
            for j in (0..neg_len).rev() {
                let d = reference.negative[j].0 + delta_rps;
                if d > 0 && use_delta[j] {
                    rps.positive.push((d, used[j]));
                }
            }
            if delta_rps > 0 && use_delta[n] {
                rps.positive.push((delta_rps, used[n]));
            }
            for j in 0..reference.positive.len() {
                let d = reference.positive[j].0 + delta_rps;
                if d > 0 && use_delta[neg_len + j] {
                    rps.positive.push((d, used[neg_len + j]));
                }
            }
            ensure!(
                rps.negative.len() + rps.positive.len() <= 16,
                "reference picture set too large"
            );
            Ok(rps)
        } else {
            let negatives = b.ue_max(16, "num_negative_pics")?;
            let positives = b.ue_max(16 - negatives, "num_positive_pics")?;
            let mut rps = Rps::default();
            let mut poc = 0;
            for _ in 0..negatives {
                poc -= b.ue_max(0x7fff, "delta_poc_s0_minus1")? as i32 + 1;
                rps.negative.push((poc, b.flag()?));
            }
            poc = 0;
            for _ in 0..positives {
                poc += b.ue_max(0x7fff, "delta_poc_s1_minus1")? as i32 + 1;
                rps.positive.push((poc, b.flag()?));
            }
            Ok(rps)
        }
    }
}

fn profile_tier_level(b: &mut Bits, max_sub_layers_minus1: u32) -> Result<u32> {
    b.skip(3)?; // profile space, tier
    let profile_idc = b.u(5)?;
    let compatible = b.u(32)?;
    b.skip(4 + 43 + 1 + 8)?; // source flags, constraints, level
    let mut present = Vec::new();
    for _ in 0..max_sub_layers_minus1 {
        present.push((b.flag()?, b.flag()?));
    }
    if max_sub_layers_minus1 > 0 {
        b.skip(2 * (8 - max_sub_layers_minus1 as usize))?;
    }
    for (profile, level) in present {
        if profile {
            b.skip(88)?;
        }
        if level {
            b.skip(8)?;
        }
    }
    // A Main or Main 10 stream says so, or that it conforms to one.
    Ok(match profile_idc {
        1 | 2 => profile_idc,
        _ if compatible & (1 << 30) != 0 => 1,
        _ if compatible & (1 << 29) != 0 => 2,
        other => other,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Sps {
    id: u32,
    width: u32,
    height: u32,
    crop: Rect,
    bit_depth_luma: u32,
    bit_depth_chroma: u32,
    log2_max_poc_lsb: u32,
    max_dec_pic_buffering: u32,
    max_num_reorder: u32,
    log2_min_cb: u32,
    log2_diff_max_min_cb: u32,
    log2_min_tb: u32,
    log2_diff_max_min_tb: u32,
    max_transform_hierarchy_depth_inter: u32,
    max_transform_hierarchy_depth_intra: u32,
    scaling_list_enabled: bool,
    scaling: Option<ScalingLists>,
    amp: bool,
    sao: bool,
    pcm: Option<(u32, u32, u32, u32, bool)>,
    rps: Vec<Rps>,
    long_term: Option<Vec<(u32, bool)>>,
    temporal_mvp: bool,
    strong_intra_smoothing: bool,
}

impl Sps {
    pub(super) fn parse(rbsp: &[u8]) -> Result<Self> {
        let mut b = Bits::new(rbsp);
        b.skip(16 + 4)?; // NAL header, VPS id
        let max_sub_layers_minus1 = b.u(3)?;
        ensure!(max_sub_layers_minus1 < 7, "sps_max_sub_layers_minus1");
        b.skip(1)?;
        let profile = profile_tier_level(&mut b, max_sub_layers_minus1)?;
        ensure!(matches!(profile, 1 | 2), "HEVC profile {profile}");
        let id = b.ue_max(15, "sps_seq_parameter_set_id")?;
        let chroma_format_idc = b.ue()?;
        ensure!(
            chroma_format_idc == 1,
            "HEVC chroma format {chroma_format_idc} is not 4:2:0"
        );
        let width = b.ue_max(16888, "pic_width_in_luma_samples")?;
        let height = b.ue_max(16888, "pic_height_in_luma_samples")?;
        ensure!(width > 0 && height > 0, "HEVC picture without pixels");
        let mut crop = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        if b.flag()? {
            let (l, r, t, bo) = (
                b.ue_max(width / 2, "crop left")? * 2,
                b.ue_max(width / 2, "crop right")? * 2,
                b.ue_max(height / 2, "crop top")? * 2,
                b.ue_max(height / 2, "crop bottom")? * 2,
            );
            ensure!(
                l + r < width && t + bo < height,
                "HEVC conformance window exceeds the picture"
            );
            crop = Rect {
                x: l,
                y: t,
                width: width - l - r,
                height: height - t - bo,
            };
        }
        let bit_depth_luma = b.ue_max(8, "bit_depth_luma_minus8")? + 8;
        let bit_depth_chroma = b.ue_max(8, "bit_depth_chroma_minus8")? + 8;
        ensure!(
            bit_depth_luma == bit_depth_chroma && matches!(bit_depth_luma, 8 | 10),
            "HEVC at {bit_depth_luma}/{bit_depth_chroma} bits"
        );
        let log2_max_poc_lsb = b.ue_max(12, "log2_max_pic_order_cnt_lsb_minus4")? + 4;
        let all_layers = b.flag()?;
        let (mut max_dec_pic_buffering, mut max_num_reorder) = (1, 0);
        for _ in (if all_layers { 0 } else { max_sub_layers_minus1 })..=max_sub_layers_minus1 {
            max_dec_pic_buffering = b.ue_max(15, "sps_max_dec_pic_buffering_minus1")? + 1;
            max_num_reorder = b.ue_max(15, "sps_max_num_reorder_pics")?;
            b.ue()?;
        }
        let log2_min_cb = b.ue_max(3, "log2_min_luma_coding_block_size_minus3")? + 3;
        let log2_diff_max_min_cb = b.ue_max(3, "log2_diff_max_min_luma_coding_block_size")?;
        let log2_min_tb = b.ue_max(3, "log2_min_luma_transform_block_size_minus2")? + 2;
        let log2_diff_max_min_tb = b.ue_max(3, "log2_diff_max_min_luma_transform_block_size")?;
        let max_transform_hierarchy_depth_inter =
            b.ue_max(4, "max_transform_hierarchy_depth_inter")?;
        let max_transform_hierarchy_depth_intra =
            b.ue_max(4, "max_transform_hierarchy_depth_intra")?;
        let scaling_list_enabled = b.flag()?;
        let mut scaling = None;
        if scaling_list_enabled {
            scaling = Some(if b.flag()? {
                ScalingLists::parse(&mut b)?
            } else {
                ScalingLists::defaults()
            });
        }
        let amp = b.flag()?;
        let sao = b.flag()?;
        let pcm = if b.flag()? {
            let luma = b.u(4)? + 1;
            let chroma = b.u(4)? + 1;
            let min = b.ue_max(2, "log2_min_pcm_luma_coding_block_size_minus3")? + 3;
            let diff = b.ue_max(2, "log2_diff_max_min_pcm_luma_coding_block_size")?;
            Some((luma, chroma, min, diff, b.flag()?))
        } else {
            None
        };
        let count = b.ue_max(64, "num_short_term_ref_pic_sets")? as usize;
        let mut rps = Vec::with_capacity(count);
        for i in 0..count {
            let set = Rps::parse(&mut b, i, &rps)?;
            rps.push(set);
        }
        let long_term = if b.flag()? {
            let n = b.ue_max(32, "num_long_term_ref_pics_sps")?;
            let mut list = Vec::new();
            for _ in 0..n {
                list.push((b.u(log2_max_poc_lsb)?, b.flag()?));
            }
            Some(list)
        } else {
            None
        };
        let temporal_mvp = b.flag()?;
        let strong_intra_smoothing = b.flag()?;
        Ok(Self {
            id,
            width,
            height,
            crop,
            bit_depth_luma,
            bit_depth_chroma,
            log2_max_poc_lsb,
            max_dec_pic_buffering,
            max_num_reorder,
            log2_min_cb,
            log2_diff_max_min_cb,
            log2_min_tb,
            log2_diff_max_min_tb,
            max_transform_hierarchy_depth_inter,
            max_transform_hierarchy_depth_intra,
            scaling_list_enabled,
            scaling,
            amp,
            sao,
            pcm,
            rps,
            long_term,
            temporal_mvp,
            strong_intra_smoothing,
        })
    }

    fn ctb_size(&self) -> u32 {
        1 << (self.log2_min_cb + self.log2_diff_max_min_cb)
    }

    fn ctbs(&self) -> (u32, u32) {
        (
            self.width.div_ceil(self.ctb_size()),
            self.height.div_ceil(self.ctb_size()),
        )
    }

    fn format(&self) -> Format {
        Format {
            profile: if self.bit_depth_luma == 10 {
                Profile::HevcMain10
            } else {
                Profile::HevcMain
            },
            width: self.width,
            height: self.height,
            // The decoded picture buffer, which counts the picture being
            // decoded, and one spare.
            surfaces: self.max_dec_pic_buffering as usize + 1,
            visible: self.crop,
        }
    }
}

type Tiles = (u32, u32, Option<(Vec<u32>, Vec<u32>)>, bool);

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Pps {
    id: u32,
    sps_id: u32,
    dependent_slice_segments: bool,
    output_flag_present: bool,
    num_extra_slice_header_bits: u32,
    sign_data_hiding: bool,
    cabac_init_present: bool,
    num_ref_idx_default: [u32; 2],
    init_qp_minus26: i32,
    constrained_intra_pred: bool,
    transform_skip: bool,
    cu_qp_delta_depth: Option<u32>,
    cb_qp_offset: i32,
    cr_qp_offset: i32,
    slice_chroma_qp_offsets_present: bool,
    weighted_pred: bool,
    weighted_bipred: bool,
    transquant_bypass: bool,
    /// Columns and rows, each as widths when not uniformly spaced.
    tiles: Option<Tiles>,
    entropy_coding_sync: bool,
    loop_filter_across_slices: bool,
    deblocking_override_enabled: bool,
    deblocking_disabled: bool,
    beta_offset_div2: i32,
    tc_offset_div2: i32,
    scaling: Option<ScalingLists>,
    lists_modification_present: bool,
    log2_parallel_merge_level: u32,
    slice_header_extension_present: bool,
}

impl Pps {
    pub(super) fn parse(rbsp: &[u8]) -> Result<Self> {
        let mut b = Bits::new(rbsp);
        b.skip(16)?;
        let id = b.ue_max(63, "pps_pic_parameter_set_id")?;
        let sps_id = b.ue_max(15, "pps_seq_parameter_set_id")?;
        let dependent_slice_segments = b.flag()?;
        let output_flag_present = b.flag()?;
        let num_extra_slice_header_bits = b.u(3)?;
        let sign_data_hiding = b.flag()?;
        let cabac_init_present = b.flag()?;
        let num_ref_idx_default = [
            b.ue_max(14, "num_ref_idx_l0_default_active_minus1")? + 1,
            b.ue_max(14, "num_ref_idx_l1_default_active_minus1")? + 1,
        ];
        let init_qp_minus26 = b.se_range(-38, 25, "init_qp_minus26")?;
        let constrained_intra_pred = b.flag()?;
        let transform_skip = b.flag()?;
        let cu_qp_delta_depth = if b.flag()? {
            Some(b.ue_max(3, "diff_cu_qp_delta_depth")?)
        } else {
            None
        };
        let cb_qp_offset = b.se_range(-12, 12, "cb_qp_offset")?;
        let cr_qp_offset = b.se_range(-12, 12, "cr_qp_offset")?;
        let slice_chroma_qp_offsets_present = b.flag()?;
        let weighted_pred = b.flag()?;
        let weighted_bipred = b.flag()?;
        let transquant_bypass = b.flag()?;
        let tiles_enabled = b.flag()?;
        let entropy_coding_sync = b.flag()?;
        let tiles = if tiles_enabled {
            let columns = b.ue_max(18, "num_tile_columns_minus1")? + 1;
            let rows = b.ue_max(20, "num_tile_rows_minus1")? + 1;
            let sizes = if b.flag()? {
                None
            } else {
                let w = (1..columns)
                    .map(|_| Ok(b.ue_max(16888, "tile extent")? + 1))
                    .collect::<Result<Vec<_>>>()?;
                let h = (1..rows)
                    .map(|_| Ok(b.ue_max(16888, "tile extent")? + 1))
                    .collect::<Result<Vec<_>>>()?;
                Some((w, h))
            };
            Some((columns, rows, sizes, b.flag()?))
        } else {
            None
        };
        let loop_filter_across_slices = b.flag()?;
        let (mut deblocking_override_enabled, mut deblocking_disabled) = (false, false);
        let (mut beta_offset_div2, mut tc_offset_div2) = (0, 0);
        if b.flag()? {
            deblocking_override_enabled = b.flag()?;
            deblocking_disabled = b.flag()?;
            if !deblocking_disabled {
                beta_offset_div2 = b.se_range(-6, 6, "beta_offset_div2")?;
                tc_offset_div2 = b.se_range(-6, 6, "tc_offset_div2")?;
            }
        }
        let scaling = if b.flag()? {
            Some(ScalingLists::parse(&mut b)?)
        } else {
            None
        };
        let lists_modification_present = b.flag()?;
        let log2_parallel_merge_level = b.ue_max(4, "log2_parallel_merge_level_minus2")? + 2;
        let slice_header_extension_present = b.flag()?;
        // The range and screen content extensions change slice syntax.
        if b.flag()? {
            let extensions = b.u(8)?;
            ensure!(
                extensions & 0x90 == 0,
                "HEVC range or screen content extensions"
            );
        }
        Ok(Self {
            id,
            sps_id,
            dependent_slice_segments,
            output_flag_present,
            num_extra_slice_header_bits,
            sign_data_hiding,
            cabac_init_present,
            num_ref_idx_default,
            init_qp_minus26,
            constrained_intra_pred,
            transform_skip,
            cu_qp_delta_depth,
            cb_qp_offset,
            cr_qp_offset,
            slice_chroma_qp_offsets_present,
            weighted_pred,
            weighted_bipred,
            transquant_bypass,
            tiles,
            entropy_coding_sync,
            loop_filter_across_slices,
            deblocking_override_enabled,
            deblocking_disabled,
            beta_offset_div2,
            tc_offset_div2,
            scaling,
            lists_modification_present,
            log2_parallel_merge_level,
            slice_header_extension_present,
        })
    }

    /// Tile column widths and row heights in CTBs (6.5.1).
    fn tile_sizes(&self, sps: &Sps) -> (Vec<u32>, Vec<u32>) {
        let (w, h) = sps.ctbs();
        let Some((columns, rows, sizes, _)) = &self.tiles else {
            return (vec![w], vec![h]);
        };
        let split = |n: u32, total: u32, given: Option<&Vec<u32>>| -> Vec<u32> {
            match given {
                Some(given) => {
                    let mut out = given.clone();
                    out.push(total.saturating_sub(given.iter().sum()));
                    out
                }
                None => (0..n)
                    .map(|i| (i + 1) * total / n - i * total / n)
                    .collect(),
            }
        };
        (
            split(*columns, w, sizes.as_ref().map(|s| &s.0)),
            split(*rows, h, sizes.as_ref().map(|s| &s.1)),
        )
    }
}

const SLICE_B: u32 = 0;
const SLICE_P: u32 = 1;
const SLICE_I: u32 = 2;

#[derive(Clone, Debug, Default, PartialEq)]
struct ListWeights {
    luma: Vec<(i8, i8)>,
    chroma: Vec<[(i8, i8); 2]>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Weights {
    luma_denom: u32,
    delta_chroma_denom: i32,
    lists: [ListWeights; 2],
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Slice {
    nal_type: u32,
    temporal_id: u32,
    first: bool,
    pps_id: u32,
    dependent: bool,
    address: u32,
    slice_type: u32,
    colour_plane: u32,
    poc_lsb: u32,
    rps: Rps,
    st_rps_bits: u32,
    /// POC LSB (or full POC when the MSB is present), and used-by-current.
    long_term: Vec<(i64, bool, bool)>,
    temporal_mvp: bool,
    sao_luma: bool,
    sao_chroma: bool,
    num_ref_idx: [u32; 2],
    list_entries: [Option<Vec<u32>>; 2],
    mvd_l1_zero: bool,
    cabac_init: bool,
    collocated_from_l0: bool,
    collocated_ref_idx: u32,
    weights: Option<Weights>,
    five_minus_max_num_merge_cand: u32,
    qp_delta: i32,
    cb_qp_offset: i32,
    cr_qp_offset: i32,
    deblocking_disabled: bool,
    beta_offset_div2: i32,
    tc_offset_div2: i32,
    loop_filter_across_slices: bool,
    entry_points: Vec<u32>,
    /// RBSP bytes before the slice data, NAL header included.
    header_bytes: usize,
}

impl Slice {
    fn is_irap(&self) -> bool {
        (16..=23).contains(&self.nal_type)
    }

    fn is_idr(&self) -> bool {
        matches!(self.nal_type, IDR_W_RADL | IDR_N_LP)
    }

    fn used_by_current(&self) -> usize {
        self.rps
            .negative
            .iter()
            .chain(&self.rps.positive)
            .filter(|(_, used)| *used)
            .count()
            + self.long_term.iter().filter(|(_, used, _)| *used).count()
    }

    /// A slice segment header. A dependent segment takes everything but
    /// its address from `previous`, the independent one before it.
    pub(super) fn parse(
        rbsp: &[u8],
        sps: &[Option<Sps>],
        pps: &[Option<Pps>],
        previous: Option<&Slice>,
    ) -> Result<Self> {
        let mut b = Bits::new(rbsp);
        b.skip(1)?;
        let nal_type = b.u(6)?;
        b.skip(6)?;
        let temporal_id = b.u(3)?.saturating_sub(1);
        let first = b.flag()?;
        if (16..=23).contains(&nal_type) {
            b.flag()?; // no_output_of_prior_pics_flag
        }
        let pps_id = b.ue_max(63, "slice_pic_parameter_set_id")?;
        let p = pps[pps_id as usize]
            .as_ref()
            .with_context(|| format!("HEVC slice before its PPS {pps_id}"))?;
        let s = sps[p.sps_id as usize]
            .as_ref()
            .with_context(|| format!("HEVC PPS before its SPS {}", p.sps_id))?;
        let mut dependent = false;
        let mut address = 0;
        if !first {
            if p.dependent_slice_segments {
                dependent = b.flag()?;
            }
            let (w, h) = s.ctbs();
            address = b.u(ceil_log2(w * h))?;
        }
        let mut slice = if dependent {
            let previous = previous.context("a dependent slice segment comes first")?;
            Slice {
                first,
                dependent,
                address,
                nal_type,
                temporal_id,
                ..previous.clone()
            }
        } else {
            let mut slice = Slice {
                nal_type,
                temporal_id,
                first,
                pps_id,
                address,
                ..Default::default()
            };
            slice.header(&mut b, s, p)?;
            slice
        };
        slice.entry_points.clear();
        if p.tiles.is_some() || p.entropy_coding_sync {
            let count = b.ue_max(
                (s.ctbs().0 * s.ctbs().1).min(u16::MAX as u32),
                "num_entry_point_offsets",
            )?;
            if count > 0 {
                let bits = b.ue_max(31, "offset_len_minus1")? + 1;
                for _ in 0..count {
                    slice.entry_points.push(b.u(bits)?);
                }
            }
        }
        if p.slice_header_extension_present {
            let length = b.ue_max(256, "slice_segment_header_extension_length")?;
            b.skip(8 * length as usize)?;
        }
        ensure!(b.bit()? == 1, "HEVC slice header misread (alignment bit)");
        while !b.byte_aligned() {
            ensure!(b.bit()? == 0, "HEVC slice header misread (alignment)");
        }
        slice.header_bytes = b.position() / 8;
        Ok(slice)
    }

    /// The independent part of a slice segment header.
    fn header(&mut self, b: &mut Bits, s: &Sps, p: &Pps) -> Result<()> {
        b.skip(p.num_extra_slice_header_bits as usize)?;
        self.slice_type = b.ue_max(2, "slice_type")?;
        if p.output_flag_present {
            b.flag()?;
        }
        if !self.is_idr() {
            self.poc_lsb = b.u(s.log2_max_poc_lsb)?;
            if !b.flag()? {
                let start = b.position();
                self.rps = Rps::parse(b, s.rps.len(), &s.rps)?;
                self.st_rps_bits = (b.position() - start) as u32;
            } else {
                ensure!(!s.rps.is_empty(), "a slice names an SPS RPS there is not");
                let idx = b.u(ceil_log2(s.rps.len() as u32))? as usize;
                self.rps = s
                    .rps
                    .get(idx)
                    .context("short_term_ref_pic_set_idx")?
                    .clone();
            }
            if let Some(lt_sps) = &s.long_term {
                let from_sps = if lt_sps.is_empty() {
                    0
                } else {
                    b.ue_max(lt_sps.len() as u32, "num_long_term_sps")?
                };
                let pics = b.ue_max(16, "num_long_term_pics")?;
                let mut msb_cycle = 0i64;
                for i in 0..from_sps + pics {
                    let (lsb, used) = if i < from_sps {
                        let idx = b.u(ceil_log2(lt_sps.len() as u32))? as usize;
                        *lt_sps.get(idx).context("lt_idx_sps")?
                    } else {
                        (b.u(s.log2_max_poc_lsb)?, b.flag()?)
                    };
                    let msb_present = b.flag()?;
                    let mut poc = i64::from(lsb);
                    if msb_present {
                        let cycle = i64::from(b.ue()?);
                        msb_cycle = if i == 0 || i == from_sps {
                            cycle
                        } else {
                            msb_cycle + cycle
                        };
                        // Resolved against the picture's POC later; keep
                        // the cycle in the high bits until then.
                        ensure!(
                            msb_cycle <= i32::MAX as i64,
                            "HEVC long-term cycle exceeds POC limits"
                        );
                        poc |= msb_cycle << 32;
                    } else if i == 0 || i == from_sps {
                        msb_cycle = 0;
                    }
                    self.long_term.push((poc, used, msb_present));
                }
            }
            if s.temporal_mvp {
                self.temporal_mvp = b.flag()?;
            }
        }
        if s.sao {
            self.sao_luma = b.flag()?;
            self.sao_chroma = b.flag()?;
        }
        if self.slice_type != SLICE_I {
            self.num_ref_idx = p.num_ref_idx_default;
            if b.flag()? {
                self.num_ref_idx[0] = b.ue_max(14, "num_ref_idx_l0_active_minus1")? + 1;
                if self.slice_type == SLICE_B {
                    self.num_ref_idx[1] = b.ue_max(14, "num_ref_idx_l1_active_minus1")? + 1;
                }
            }
            if self.slice_type != SLICE_B {
                self.num_ref_idx[1] = 0;
            }
            let total = self.used_by_current() as u32;
            ensure!(total > 0, "an inter slice without references");
            if p.lists_modification_present && total > 1 {
                let bits = ceil_log2(total);
                for list in 0..1 + (self.slice_type == SLICE_B) as usize {
                    if b.flag()? {
                        let entries = (0..self.num_ref_idx[list])
                            .map(|_| b.u(bits))
                            .collect::<Result<Vec<_>>>()?;
                        ensure!(
                            entries.iter().all(|&e| e < total),
                            "list_entry out of range"
                        );
                        self.list_entries[list] = Some(entries);
                    }
                }
            }
            if self.slice_type == SLICE_B {
                self.mvd_l1_zero = b.flag()?;
            }
            if p.cabac_init_present {
                self.cabac_init = b.flag()?;
            }
            self.collocated_from_l0 = true;
            if self.temporal_mvp {
                if self.slice_type == SLICE_B {
                    self.collocated_from_l0 = b.flag()?;
                }
                let list = if self.collocated_from_l0 { 0 } else { 1 };
                if self.num_ref_idx[list] > 1 {
                    self.collocated_ref_idx =
                        b.ue_max(self.num_ref_idx[list] - 1, "collocated_ref_idx")?;
                }
            }
            if (p.weighted_pred && self.slice_type == SLICE_P)
                || (p.weighted_bipred && self.slice_type == SLICE_B)
            {
                self.weights = Some(pred_weight_table(b, self.num_ref_idx)?);
            }
            self.five_minus_max_num_merge_cand = b.ue_max(4, "five_minus_max_num_merge_cand")?;
        }
        self.qp_delta = b.se_range(-63, 63, "slice_qp_delta")?;
        if p.slice_chroma_qp_offsets_present {
            self.cb_qp_offset = b.se_range(-12, 12, "slice_cb_qp_offset")?;
            self.cr_qp_offset = b.se_range(-12, 12, "slice_cr_qp_offset")?;
        }
        let overridden = p.deblocking_override_enabled && b.flag()?;
        if overridden {
            self.deblocking_disabled = b.flag()?;
            if !self.deblocking_disabled {
                self.beta_offset_div2 = b.se_range(-6, 6, "beta_offset_div2")?;
                self.tc_offset_div2 = b.se_range(-6, 6, "tc_offset_div2")?;
            }
        } else {
            self.deblocking_disabled = p.deblocking_disabled;
            self.beta_offset_div2 = p.beta_offset_div2;
            self.tc_offset_div2 = p.tc_offset_div2;
        }
        self.loop_filter_across_slices = if p.loop_filter_across_slices
            && (self.sao_luma || self.sao_chroma || !self.deblocking_disabled)
        {
            b.flag()?
        } else {
            p.loop_filter_across_slices
        };
        Ok(())
    }
}

fn pred_weight_table(b: &mut Bits, num_ref_idx: [u32; 2]) -> Result<Weights> {
    let luma_denom = b.ue_max(7, "luma_log2_weight_denom")?;
    let delta_chroma_denom = b.se()?;
    ensure!(
        (-7..=7).contains(&delta_chroma_denom),
        "chroma weight denominator delta"
    );
    let chroma_denom = luma_denom as i32 + delta_chroma_denom;
    ensure!((0..=7).contains(&chroma_denom), "chroma_log2_weight_denom");
    let mut w = Weights {
        luma_denom,
        delta_chroma_denom,
        ..Default::default()
    };
    for (list, &count) in num_ref_idx.iter().enumerate() {
        let count = count as usize;
        let luma_flags = (0..count).map(|_| b.flag()).collect::<Result<Vec<_>>>()?;
        let chroma_flags = (0..count).map(|_| b.flag()).collect::<Result<Vec<_>>>()?;
        for i in 0..count {
            let luma = if luma_flags[i] {
                let delta = b.se()?;
                let offset = b.se()?;
                ensure!(
                    (-128..=127).contains(&delta) && (-128..=127).contains(&offset),
                    "luma weight"
                );
                (delta as i8, offset as i8)
            } else {
                (0, 0)
            };
            let mut chroma = [(0i8, 0i8); 2];
            if chroma_flags[i] {
                for c in &mut chroma {
                    let delta = b.se()?;
                    let delta_offset = b.se()?;
                    ensure!((-128..=127).contains(&delta), "chroma weight");
                    ensure!((-512..=511).contains(&delta_offset), "chroma offset");
                    let weight = (1 << chroma_denom) + delta;
                    let offset =
                        (128 - ((128 * weight) >> chroma_denom) + delta_offset).clamp(-128, 127);
                    *c = (delta as i8, offset as i8);
                }
            }
            w.lists[list].luma.push(luma);
            w.lists[list].chroma.push(chroma);
        }
    }
    Ok(w)
}

/// A picture in the decoded picture buffer, all of them references.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Picture {
    surface: Id,
    poc: i32,
    long_term: bool,
}

/// The POCs of RefPicSetStCurrBefore, StCurrAfter and LtCurr, in order.
#[derive(Clone, Debug, Default, PartialEq)]
struct Sets {
    before: Vec<i32>,
    after: Vec<i32>,
    long: Vec<i32>,
}

/// Where a reference sits in the current picture's set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Set {
    Before,
    After,
    LongTerm,
    Unused,
}

#[derive(Default)]
pub struct Hevc {
    sps: Vec<Option<Sps>>,
    pps: Vec<Option<Pps>>,
    format: Option<Format>,
    surfaces: Vec<Id>,
    dpb: Vec<(Picture, Set)>,
    /// POC of the previous TemporalId 0 picture (8.3.1).
    prev_tid0_poc: i32,
    /// RASL pictures of the IRAP the stream started at are skipped.
    skip_rasl: bool,
    after_eos: bool,
    waiting: bool,
}

impl Hevc {
    pub fn new() -> Self {
        Self {
            sps: vec![None; 16],
            pps: vec![None; 64],
            waiting: true,
            ..Default::default()
        }
    }

    fn invalidate(&mut self) {
        self.dpb.clear();
        self.waiting = true;
    }

    fn reset_hardware(&mut self) {
        self.invalidate();
        self.format = None;
        self.surfaces.clear();
    }

    pub fn decode(&mut self, au: &[u8], backend: &mut dyn Backend) -> Result<Option<Decoded>> {
        let result = self.decode_unit(au, backend);
        if let Err(e) = &result {
            if e.is::<super::HardwareFailed>() {
                self.reset_hardware();
            } else {
                self.invalidate();
            }
        }
        result
    }

    fn decode_unit(&mut self, au: &[u8], backend: &mut dyn Backend) -> Result<Option<Decoded>> {
        let mut slices: Vec<(Slice, &[u8], usize)> = Vec::new();
        for nal in nal_units(au) {
            ensure!(nal.len() >= 2, "HEVC NAL unit without a header");
            let kind = u32::from(nal[0] >> 1) & 0x3f;
            let layer = (u32::from(nal[0] & 1) << 5) | u32::from(nal[1] >> 3);
            if layer != 0 {
                continue;
            }
            match kind {
                33 => {
                    let sps = Sps::parse(&rbsp(nal))?;
                    let id = sps.id as usize;
                    self.sps[id] = Some(sps);
                }
                34 => {
                    let pps = Pps::parse(&rbsp(nal))?;
                    let id = pps.id as usize;
                    self.pps[id] = Some(pps);
                }
                EOS => self.after_eos = true,
                0..=9 | 16..=21 => {
                    let r = rbsp(nal);
                    let previous = slices.iter().rev().map(|(s, ..)| s).find(|s| !s.dependent);
                    let slice = Slice::parse(&r, &self.sps, &self.pps, previous)?;
                    if slices.is_empty() {
                        ensure!(slice.first, "an access unit starts mid-picture");
                    } else {
                        ensure!(!slice.first, "an access unit with more than one picture");
                    }
                    let epb = emulation_prevention_before(nal, slice.header_bytes);
                    slices.push((slice, nal, epb));
                }
                _ => {}
            }
        }
        let Some((first, ..)) = slices.first() else {
            return Ok(None);
        };
        let first = first.clone();
        let pps = self.pps[first.pps_id as usize].clone().expect("parsed");
        let sps = self.sps[pps.sps_id as usize].clone().expect("parsed");

        let no_rasl_output = first.is_irap()
            && (first.is_idr() || first.nal_type <= 18 || self.waiting || self.after_eos);
        if first.is_irap() {
            let format = sps.format();
            if self.format.as_ref() != Some(&format) {
                ensure!(
                    no_rasl_output || self.format.is_none(),
                    "HEVC size changed at a CRA"
                );
                self.format = None;
                self.surfaces = backend.configure(&format).map_err(hardware)?;
                ensure!(
                    self.surfaces.len() >= format.surfaces,
                    "VA-API gave too few surfaces"
                );
                self.format = Some(format);
                self.dpb.clear();
            }
            self.skip_rasl = no_rasl_output;
            self.waiting = false;
            self.after_eos = false;
        } else if self.waiting {
            bail!("HEVC picture before an IRAP");
        }
        if matches!(first.nal_type, 8 | 9) && self.skip_rasl {
            return Ok(None);
        }
        ensure!(
            self.format.as_ref() == Some(&sps.format()),
            "HEVC SPS changed without an IRAP"
        );

        if let Some((columns, rows, sizes, _)) = &pps.tiles {
            let (w, h) = sps.ctbs();
            ensure!(*columns <= w && *rows <= h, "HEVC tiles exceed the picture");
            if let Some((columns, rows)) = sizes {
                ensure!(
                    columns.iter().sum::<u32>() < w && rows.iter().sum::<u32>() < h,
                    "HEVC explicit tile extents exceed the picture"
                );
            }
        }
        let poc = self.order(&sps, &first, no_rasl_output)?;
        let sets = self.apply_rps(&sps, &first, poc)?;
        ensure!(self.dpb.len() <= 15, "HEVC exceeds VA-API reference slots");
        let target = *self
            .surfaces
            .iter()
            .find(|&&s| backend.available(s) && !self.dpb.iter().any(|(p, _)| p.surface == s))
            .context("no free VA-API surface")?;

        let picture = self.picture_parameters(&sps, &pps, &first, target, poc);
        let iq = pps
            .scaling
            .as_ref()
            .or(sps.scaling.as_ref())
            .filter(|_| sps.scaling_list_enabled)
            .map(ScalingLists::va);
        let count = slices.len();
        let mut params = Vec::with_capacity(count);
        let mut subsets = Vec::new();
        for (n, (slice, nal, epb)) in slices.iter().enumerate() {
            ensure!(slice.pps_id == first.pps_id, "slices with different PPSs");
            let mut params_slice =
                self.slice_parameters(slice, &sets, nal.len(), *epb, n + 1 == count)?;
            params_slice.entry_offset_to_subset_array =
                u16::try_from(subsets.len()).context("HEVC subset array exceeds VA-API limits")?;
            subsets.extend_from_slice(&slice.entry_points);
            params.push((params_slice, *nal));
        }
        backend
            .hevc(target, &picture, iq.as_ref(), &params, &subsets)
            .map_err(hardware)?;

        self.dpb.push((
            Picture {
                surface: target,
                poc,
                long_term: false,
            },
            Set::Unused,
        ));
        Ok(Some(Decoded {
            surface: target,
            visible: sps.crop,
            ten_bit: sps.bit_depth_luma == 10,
        }))
    }

    /// PicOrderCntVal (8.3.1).
    fn order(&mut self, sps: &Sps, slice: &Slice, no_rasl_output: bool) -> Result<i32> {
        let max = 1i64 << sps.log2_max_poc_lsb;
        let lsb = i64::from(slice.poc_lsb);
        let msb = if slice.is_irap() && no_rasl_output {
            0
        } else {
            let prev_lsb = i64::from(self.prev_tid0_poc) & (max - 1);
            let prev_msb = i64::from(self.prev_tid0_poc) - prev_lsb;
            if lsb < prev_lsb && prev_lsb - lsb >= max / 2 {
                prev_msb + max
            } else if lsb > prev_lsb && lsb - prev_lsb > max / 2 {
                prev_msb - max
            } else {
                prev_msb
            }
        };
        let poc = i32::try_from(msb + lsb).context("HEVC POC overflow")?;
        // Not RASL, RADL or a sub-layer non-reference picture.
        let sub_layer_non_reference = slice.nal_type <= 14 && slice.nal_type.is_multiple_of(2);
        if slice.temporal_id == 0 && !matches!(slice.nal_type, 6..=9) && !sub_layer_non_reference {
            self.prev_tid0_poc = poc;
        }
        Ok(poc)
    }

    /// The reference picture set (8.3.2): every picture it does not name
    /// leaves the buffer; the ones it does are marked with their place.
    fn apply_rps(&mut self, sps: &Sps, slice: &Slice, poc: i32) -> Result<Sets> {
        ensure!(
            slice
                .rps
                .negative
                .iter()
                .chain(&slice.rps.positive)
                .all(|&(delta, _)| i32::try_from(i64::from(poc) + i64::from(delta)).is_ok()),
            "HEVC reference POC overflow"
        );
        let mut sets = Sets::default();
        if slice.is_idr() {
            self.dpb.clear();
            return Ok(sets);
        }
        let max = 1i64 << sps.log2_max_poc_lsb;
        for (_, set) in &mut self.dpb {
            *set = Set::Unused;
        }
        let mut kept = vec![false; self.dpb.len()];
        // Long-term first: they may name a picture that was short-term.
        for &(value, used, msb_present) in &slice.long_term {
            let target = if msb_present {
                let cycle = value >> 32;
                let lsb = value & 0xffff_ffff;
                Some(i64::from(poc) - cycle * max - (i64::from(poc) & (max - 1)) + lsb)
            } else {
                None
            };
            let found = (0..self.dpb.len()).find(|&i| match target {
                Some(t) => i64::from(self.dpb[i].0.poc) == t,
                None => i64::from(self.dpb[i].0.poc) & (max - 1) == value,
            });
            match found {
                Some(i) => {
                    kept[i] = true;
                    self.dpb[i].0.long_term = true;
                    if used {
                        self.dpb[i].1 = Set::LongTerm;
                        sets.long.push(self.dpb[i].0.poc);
                    }
                }
                None if used => bail!("HEVC long-term reference missing"),
                None => {}
            }
        }
        for (deltas, set) in [
            (&slice.rps.negative, Set::Before),
            (&slice.rps.positive, Set::After),
        ] {
            for &(delta, used) in deltas {
                let found = (0..self.dpb.len())
                    .find(|&i| !self.dpb[i].0.long_term && self.dpb[i].0.poc == poc + delta);
                match found {
                    Some(i) => {
                        kept[i] = true;
                        if used {
                            self.dpb[i].1 = set;
                            match set {
                                Set::Before => sets.before.push(poc + delta),
                                _ => sets.after.push(poc + delta),
                            }
                        }
                    }
                    None if used && !matches!(slice.nal_type, 8 | 9) => {
                        bail!("HEVC reference POC {} missing", poc + delta)
                    }
                    None => {}
                }
            }
        }
        let mut i = 0;
        self.dpb.retain(|_| {
            i += 1;
            kept[i - 1]
        });
        Ok(sets)
    }

    fn picture_parameters(
        &self,
        sps: &Sps,
        pps: &Pps,
        slice: &Slice,
        target: Id,
        poc: i32,
    ) -> va::PictureParameterHevc {
        let mut p: va::PictureParameterHevc = va::zeroed();
        p.CurrPic = PictureHevc {
            picture_id: target,
            pic_order_cnt: poc,
            flags: 0,
            va_reserved: [0; 4],
        };
        p.ReferenceFrames = [PictureHevc::NONE; 15];
        for (slot, (pic, set)) in p.ReferenceFrames.iter_mut().zip(&self.dpb) {
            *slot = PictureHevc {
                picture_id: pic.surface,
                pic_order_cnt: pic.poc,
                flags: match set {
                    Set::Before => va::HEVC_RPS_ST_CURR_BEFORE,
                    Set::After => va::HEVC_RPS_ST_CURR_AFTER,
                    Set::LongTerm => va::HEVC_RPS_LT_CURR,
                    Set::Unused => 0,
                } | if pic.long_term {
                    va::HEVC_LONG_TERM_REFERENCE
                } else {
                    0
                },
                va_reserved: [0; 4],
            };
        }
        p.pic_width_in_luma_samples = sps.width as u16;
        p.pic_height_in_luma_samples = sps.height as u16;
        let tiles = pps.tiles.as_ref();
        p.pic_fields = pack(&[
            (1, 2),
            (0, 1),
            (sps.pcm.is_some() as u32, 1),
            (sps.scaling_list_enabled as u32, 1),
            (pps.transform_skip as u32, 1),
            (sps.amp as u32, 1),
            (sps.strong_intra_smoothing as u32, 1),
            (pps.sign_data_hiding as u32, 1),
            (pps.constrained_intra_pred as u32, 1),
            (pps.cu_qp_delta_depth.is_some() as u32, 1),
            (pps.weighted_pred as u32, 1),
            (pps.weighted_bipred as u32, 1),
            (pps.transquant_bypass as u32, 1),
            (tiles.is_some() as u32, 1),
            (pps.entropy_coding_sync as u32, 1),
            (pps.loop_filter_across_slices as u32, 1),
            (tiles.is_some_and(|t| t.3) as u32, 1),
            (sps.pcm.is_some_and(|p| p.4) as u32, 1),
            ((sps.max_num_reorder == 0) as u32, 1),
            (0, 1),
        ]);
        p.sps_max_dec_pic_buffering_minus1 = (sps.max_dec_pic_buffering - 1) as u8;
        p.bit_depth_luma_minus8 = (sps.bit_depth_luma - 8) as u8;
        p.bit_depth_chroma_minus8 = (sps.bit_depth_chroma - 8) as u8;
        if let Some((luma, chroma, min, diff, _)) = sps.pcm {
            p.pcm_sample_bit_depth_luma_minus1 = (luma - 1) as u8;
            p.pcm_sample_bit_depth_chroma_minus1 = (chroma - 1) as u8;
            p.log2_min_pcm_luma_coding_block_size_minus3 = (min - 3) as u8;
            p.log2_diff_max_min_pcm_luma_coding_block_size = diff as u8;
        }
        p.log2_min_luma_coding_block_size_minus3 = (sps.log2_min_cb - 3) as u8;
        p.log2_diff_max_min_luma_coding_block_size = sps.log2_diff_max_min_cb as u8;
        p.log2_min_transform_block_size_minus2 = (sps.log2_min_tb - 2) as u8;
        p.log2_diff_max_min_transform_block_size = sps.log2_diff_max_min_tb as u8;
        p.max_transform_hierarchy_depth_intra = sps.max_transform_hierarchy_depth_intra as u8;
        p.max_transform_hierarchy_depth_inter = sps.max_transform_hierarchy_depth_inter as u8;
        p.init_qp_minus26 = pps.init_qp_minus26 as i8;
        p.diff_cu_qp_delta_depth = pps.cu_qp_delta_depth.unwrap_or(0) as u8;
        p.pps_cb_qp_offset = pps.cb_qp_offset as i8;
        p.pps_cr_qp_offset = pps.cr_qp_offset as i8;
        p.log2_parallel_merge_level_minus2 = (pps.log2_parallel_merge_level - 2) as u8;
        if tiles.is_some() {
            let (columns, rows) = pps.tile_sizes(sps);
            p.num_tile_columns_minus1 = (columns.len() - 1) as u8;
            p.num_tile_rows_minus1 = (rows.len() - 1) as u8;
            for (slot, w) in p.column_width_minus1.iter_mut().zip(&columns) {
                *slot = w.saturating_sub(1) as u16;
            }
            for (slot, h) in p.row_height_minus1.iter_mut().zip(&rows) {
                *slot = h.saturating_sub(1) as u16;
            }
        }
        p.slice_parsing_fields = pack(&[
            (pps.lists_modification_present as u32, 1),
            (sps.long_term.is_some() as u32, 1),
            (sps.temporal_mvp as u32, 1),
            (pps.cabac_init_present as u32, 1),
            (pps.output_flag_present as u32, 1),
            (pps.dependent_slice_segments as u32, 1),
            (pps.slice_chroma_qp_offsets_present as u32, 1),
            (sps.sao as u32, 1),
            (pps.deblocking_override_enabled as u32, 1),
            (pps.deblocking_disabled as u32, 1),
            (pps.slice_header_extension_present as u32, 1),
            (slice.is_irap() as u32, 1),
            (slice.is_idr() as u32, 1),
            (slice.is_irap() as u32, 1),
        ]);
        p.log2_max_pic_order_cnt_lsb_minus4 = (sps.log2_max_poc_lsb - 4) as u8;
        p.num_short_term_ref_pic_sets = sps.rps.len() as u8;
        p.num_long_term_ref_pics_sps = sps.long_term.as_ref().map_or(0, |l| l.len()) as u8;
        p.num_ref_idx_l0_default_active_minus1 = (pps.num_ref_idx_default[0] - 1) as u8;
        p.num_ref_idx_l1_default_active_minus1 = (pps.num_ref_idx_default[1] - 1) as u8;
        p.pps_beta_offset_div2 = pps.beta_offset_div2 as i8;
        p.pps_tc_offset_div2 = pps.tc_offset_div2 as i8;
        p.num_extra_slice_header_bits = pps.num_extra_slice_header_bits as u8;
        p.st_rps_bits = slice.st_rps_bits;
        p
    }

    /// The reference lists (8.3.4), as indices into the picture's
    /// `ReferenceFrames` (the buffer's order).
    fn lists(&self, slice: &Slice, sets: &Sets) -> Result<[Vec<u8>; 2]> {
        let index = |pocs: &[i32]| -> Vec<u8> {
            pocs.iter()
                .filter_map(|&poc| self.dpb.iter().position(|(p, _)| p.poc == poc))
                .map(|i| i as u8)
                .collect()
        };
        let (before, after, long) = (index(&sets.before), index(&sets.after), index(&sets.long));
        let mut out: [Vec<u8>; 2] = [Vec::new(), Vec::new()];
        for (list, output) in out.iter_mut().enumerate() {
            let active = slice.num_ref_idx[list] as usize;
            if active == 0 {
                continue;
            }
            let cycle: Vec<u8> = if list == 0 {
                before.iter().chain(&after).chain(&long).copied().collect()
            } else {
                after.iter().chain(&before).chain(&long).copied().collect()
            };
            ensure!(!cycle.is_empty(), "an inter slice without references");
            let temp: Vec<u8> = cycle
                .iter()
                .copied()
                .cycle()
                .take(active.max(cycle.len()))
                .collect();
            *output = match &slice.list_entries[list] {
                Some(entries) => entries
                    .iter()
                    .map(|&e| temp.get(e as usize).copied().context("list_entry"))
                    .collect::<Result<_>>()?,
                None => temp[..active].to_vec(),
            };
        }
        Ok(out)
    }

    fn slice_parameters(
        &self,
        slice: &Slice,
        sets: &Sets,
        size: usize,
        epb: usize,
        last: bool,
    ) -> Result<va::SliceHevc> {
        let lists = self.lists(slice, sets)?;
        let mut s: va::SliceHevc = va::zeroed();
        s.slice_data_size = size as u32;
        s.slice_data_byte_offset = slice.header_bytes as u32;
        s.slice_segment_address = slice.address;
        s.RefPicList = [[0xff; 15]; 2];
        for (list, entries) in lists.iter().enumerate() {
            for (slot, &e) in s.RefPicList[list].iter_mut().zip(entries) {
                *slot = e;
            }
        }
        s.LongSliceFlags = pack(&[
            (last as u32, 1),
            (slice.dependent as u32, 1),
            (slice.slice_type, 2),
            (slice.colour_plane, 2),
            (slice.sao_luma as u32, 1),
            (slice.sao_chroma as u32, 1),
            (slice.mvd_l1_zero as u32, 1),
            (slice.cabac_init as u32, 1),
            (slice.temporal_mvp as u32, 1),
            (slice.deblocking_disabled as u32, 1),
            (slice.collocated_from_l0 as u32, 1),
            (slice.loop_filter_across_slices as u32, 1),
        ]);
        s.collocated_ref_idx = if slice.temporal_mvp {
            slice.collocated_ref_idx as u8
        } else {
            0xff
        };
        s.num_ref_idx_l0_active_minus1 = slice.num_ref_idx[0].saturating_sub(1) as u8;
        s.num_ref_idx_l1_active_minus1 = slice.num_ref_idx[1].saturating_sub(1) as u8;
        s.slice_qp_delta = slice.qp_delta as i8;
        s.slice_cb_qp_offset = slice.cb_qp_offset as i8;
        s.slice_cr_qp_offset = slice.cr_qp_offset as i8;
        s.slice_beta_offset_div2 = slice.beta_offset_div2 as i8;
        s.slice_tc_offset_div2 = slice.tc_offset_div2 as i8;
        if let Some(w) = &slice.weights {
            s.luma_log2_weight_denom = w.luma_denom as u8;
            s.delta_chroma_log2_weight_denom = w.delta_chroma_denom as i8;
            for (i, (&(dw, o), c)) in w.lists[0].luma.iter().zip(&w.lists[0].chroma).enumerate() {
                s.delta_luma_weight_l0[i] = dw;
                s.luma_offset_l0[i] = o;
                s.delta_chroma_weight_l0[i] = [c[0].0, c[1].0];
                s.ChromaOffsetL0[i] = [c[0].1, c[1].1];
            }
            for (i, (&(dw, o), c)) in w.lists[1].luma.iter().zip(&w.lists[1].chroma).enumerate() {
                s.delta_luma_weight_l1[i] = dw;
                s.luma_offset_l1[i] = o;
                s.delta_chroma_weight_l1[i] = [c[0].0, c[1].0];
                s.ChromaOffsetL1[i] = [c[0].1, c[1].1];
            }
        }
        s.five_minus_max_num_merge_cand = slice.five_minus_max_num_merge_cand as u8;
        s.num_entry_point_offsets = slice.entry_points.len() as u16;
        s.slice_data_num_emu_prevn_bytes = epb as u16;
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::super::h264::tests::access_units;
    use super::super::tests::Recorder;
    use super::*;

    fn units(bytes: &[u8]) -> Vec<Vec<u8>> {
        access_units(bytes, |nal| {
            let kind = (nal[0] >> 1) & 0x3f;
            let slice = kind <= 31;
            (
                if slice {
                    nal.get(2).is_some_and(|b| b & 0x80 != 0)
                } else {
                    matches!(kind, 32..=35 | 39)
                },
                slice,
            )
        })
    }

    #[test]
    fn main_and_main10_probes_describe_their_picture_and_slices() {
        for (bytes, ten_bit) in [
            (
                include_bytes!("../../../tests/fixtures/decode-probe.hevc").as_slice(),
                false,
            ),
            (
                include_bytes!("../../../tests/fixtures/decode-probe-main10.hevc").as_slice(),
                true,
            ),
        ] {
            let mut decoder = Hevc::new();
            let mut backend = Recorder::default();
            let decoded = decoder.decode(bytes, &mut backend).unwrap().unwrap();
            assert_eq!(decoded.ten_bit, ten_bit);
            assert_eq!((decoded.visible.width, decoded.visible.height), (256, 256));
            assert_eq!(
                backend.formats[0].profile,
                if ten_bit {
                    Profile::HevcMain10
                } else {
                    Profile::HevcMain
                }
            );
            let picture = &backend.hevc[0];
            assert_eq!(picture.target, decoded.surface);
            assert_eq!(picture.picture.CurrPic.picture_id, decoded.surface);
            assert_eq!(
                picture.picture.bit_depth_luma_minus8,
                if ten_bit { 2 } else { 0 }
            );
            assert_eq!(picture.picture.ReferenceFrames, [PictureHevc::NONE; 15]);
            assert_eq!(picture.slices.last().unwrap().0.LongSliceFlags & 1, 1);
            assert!(
                picture.iq.is_none()
                    || picture.iq.unwrap().ScalingList4x4[0].iter().all(|&v| v > 0)
            );
            for (slice, data) in &picture.slices {
                assert!(slice.slice_data_byte_offset > 2);
                assert!((slice.slice_data_byte_offset as usize) < data.len());
                assert_eq!(slice.slice_data_size as usize, data.len());
            }
        }
    }

    /// Minimal IDR headers with synthetic payload. Only the parameter
    /// buffers are exercised, so the recorder never decodes the payload.
    fn append_slice(au: &mut Vec<u8>, header: &str) {
        let mut bits = header.to_string();
        bits.push('1'); // byte_alignment()
        while !bits.len().is_multiple_of(8) {
            bits.push('0');
        }
        au.extend_from_slice(&[0, 0, 0, 1, 0x28, 1]);
        for byte in bits.as_bytes().chunks_exact(8) {
            au.push(
                byte.iter()
                    .fold(0, |value, bit| (value << 1) | (bit - b'0')),
            );
        }
        au.extend_from_slice(&[0x55; 32]);
    }

    #[test]
    fn tile_and_wavefront_offsets_reach_the_subsets_buffer_in_slice_order() {
        for tiles in [false, true] {
            let mut decoder = Hevc::new();
            let mut backend = Recorder::default();
            decoder
                .decode(
                    include_bytes!("../../../tests/fixtures/decode-probe.hevc"),
                    &mut backend,
                )
                .unwrap();
            let pps = decoder.pps[0].as_mut().unwrap();
            pps.tiles = tiles.then_some((2, 2, None, false));
            pps.entropy_coding_sync = !tiles;
            pps.dependent_slice_segments = true;
            pps.output_flag_present = false;
            pps.num_extra_slice_header_bits = 0;
            pps.slice_chroma_qp_offsets_present = false;
            pps.deblocking_override_enabled = false;
            pps.loop_filter_across_slices = false;
            pps.slice_header_extension_present = false;
            let sps = decoder.sps[pps.sps_id as usize].as_mut().unwrap();
            sps.sao = false;
            let address_bits = ceil_log2(sps.ctbs().0 * sps.ctbs().1) as usize;
            let mut au = Vec::new();
            // First independent I slice, offsets_minus1 = [2, 9].
            append_slice(&mut au, "10101110110010000101001");
            // Dependent segments supply their own entry points, including
            // an empty list that must not inherit the independent slice's.
            append_slice(&mut au, &format!("0011{:0address_bits$b}010001000101", 1));
            append_slice(&mut au, &format!("0011{:0address_bits$b}1", 2));
            decoder.decode(&au, &mut backend).unwrap().unwrap();
            let picture = backend.hevc.last().unwrap();
            assert_eq!(picture.subsets, [2, 9, 5]);
            let counts: Vec<_> = picture
                .slices
                .iter()
                .map(|(s, _)| s.num_entry_point_offsets)
                .collect();
            let starts: Vec<_> = picture
                .slices
                .iter()
                .map(|(s, _)| s.entry_offset_to_subset_array)
                .collect();
            assert_eq!(counts, [2, 1, 0]);
            assert_eq!(starts, [0, 2, 3]);
            for (params, nal) in &picture.slices {
                assert_eq!(&nal[params.slice_data_byte_offset as usize..], &[0x55; 32]);
            }
        }
    }

    #[test]
    fn b_pictures_keep_the_references_and_failures_wait_for_an_irap() {
        let units = units(include_bytes!(
            "../../../tests/fixtures/vaapi-b-frames.hevc"
        ));
        assert!(units.len() >= 6);
        let mut decoder = Hevc::new();
        let mut backend = Recorder::default();
        assert!(decoder.decode(&units[1], &mut backend).is_err());
        for au in &units {
            let decoded = decoder.decode(au, &mut backend).unwrap().unwrap();
            let picture = backend.hevc.last().unwrap();
            assert_ne!(picture.picture.CurrPic.picture_id, va::INVALID_ID);
            for (slice, _) in &picture.slices {
                for &index in slice.RefPicList.iter().flatten().filter(|&&i| i != 0xff) {
                    let reference = picture.picture.ReferenceFrames[index as usize];
                    assert_ne!(reference.picture_id, va::INVALID_ID);
                    assert_ne!(reference.picture_id, decoded.surface);
                }
            }
        }
        assert!(backend.hevc.iter().any(|p| p
            .slices
            .iter()
            .any(|(s, _)| (s.LongSliceFlags >> 2) & 3 == SLICE_B)));
        backend.fail = true;
        let error = decoder.decode(&units[0], &mut backend).unwrap_err();
        assert!(error.is::<super::super::HardwareFailed>());
        backend.fail = false;
        assert!(decoder.decode(&units[1], &mut backend).is_err());
        decoder.decode(&units[0], &mut backend).unwrap().unwrap();
    }
}
