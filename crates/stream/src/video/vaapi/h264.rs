//! H.264 for VA-API: the parameter sets and slice headers parsed, the
//! reference pictures kept and marked (ITU-T H.264 8.2), and each access
//! unit turned into VA picture, matrix and slice parameters. The driver
//! decodes the slice data itself.
//!
//! Progressive frames in 4:2:0 at 8 bits: Constrained Baseline, Main and
//! High, CAVLC or CABAC, I, P and B slices, long-term references and
//! every memory management operation, reordering commands, weighted
//! prediction, scaling matrices, several slices a picture and gaps in
//! `frame_num`. Interlaced coding, FMO, SP/SI slices, 4:2:2/4:4:4 and
//! High 10 are refused (an error, never a wrong picture). Pictures are
//! given out in decoding order, as every Latch decoder does: Latch's
//! encoders do not reorder frames.

use super::bits::{nal_units, rbsp, Bits};
use super::va::{self, pack, Id, PictureH264};
use super::{hardware, Backend, Decoded, Format, Profile, Rect};
use anyhow::{bail, ensure, Context, Result};

const ZIGZAG_4X4: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];
const ZIGZAG_8X8: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];
// Tables 7-3 and 7-4, in zig-zag order.
const DEFAULT_4X4_INTRA: [u8; 16] = [
    6, 13, 13, 20, 20, 20, 28, 28, 28, 28, 32, 32, 32, 37, 37, 42,
];
const DEFAULT_4X4_INTER: [u8; 16] = [
    10, 14, 14, 20, 20, 20, 24, 24, 24, 24, 27, 27, 27, 30, 30, 34,
];
const DEFAULT_8X8_INTRA: [u8; 64] = [
    6, 10, 10, 13, 11, 13, 16, 16, 16, 16, 18, 18, 18, 18, 18, 23, 23, 23, 23, 23, 23, 25, 25, 25,
    25, 25, 25, 25, 27, 27, 27, 27, 27, 27, 27, 27, 29, 29, 29, 29, 29, 29, 29, 31, 31, 31, 31, 31,
    31, 33, 33, 33, 33, 33, 36, 36, 36, 36, 38, 38, 38, 40, 40, 42,
];
const DEFAULT_8X8_INTER: [u8; 64] = [
    9, 13, 13, 15, 13, 15, 17, 17, 17, 17, 19, 19, 19, 19, 19, 21, 21, 21, 21, 21, 21, 22, 22, 22,
    22, 22, 22, 22, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 27, 27, 27, 27, 27,
    27, 28, 28, 28, 28, 28, 30, 30, 30, 30, 32, 32, 32, 33, 33, 35,
];

/// One `scaling_list()` as coded.
#[derive(Clone, Debug, PartialEq)]
enum List {
    Absent,
    Default,
    /// Zig-zag order.
    Coded(Vec<u8>),
}

/// The six 4×4 and two 8×8 matrices of a 4:2:0 picture, in raster order.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Matrices {
    m4: [[u8; 16]; 6],
    m8: [[u8; 64]; 2],
}

impl Matrices {
    const FLAT: Self = Self {
        m4: [[16; 16]; 6],
        m8: [[16; 64]; 2],
    };

    fn defaults() -> Self {
        let mut m = Self::FLAT;
        for (i, list) in m.m4.iter_mut().enumerate() {
            *list = raster4(if i < 3 {
                &DEFAULT_4X4_INTRA
            } else {
                &DEFAULT_4X4_INTER
            });
        }
        m.m8 = [raster8(&DEFAULT_8X8_INTRA), raster8(&DEFAULT_8X8_INTER)];
        m
    }

    /// Lists as coded, falling back to `fallback` for lists 0 and 3 and
    /// the 8×8s and to the previous list otherwise (rules A and B, 7.4.2).
    fn resolve(lists: &[List], fallback: &Self) -> Self {
        let mut m = Self::FLAT;
        for (i, list) in lists.iter().take(6).enumerate() {
            m.m4[i] = match list {
                List::Coded(v) => raster4(v),
                List::Default if i < 3 => raster4(&DEFAULT_4X4_INTRA),
                List::Default => raster4(&DEFAULT_4X4_INTER),
                List::Absent if i == 0 || i == 3 => fallback.m4[i],
                List::Absent => m.m4[i - 1],
            };
        }
        for i in 0..2 {
            m.m8[i] = match lists.get(6 + i).unwrap_or(&List::Absent) {
                List::Coded(v) => raster8(v),
                List::Default if i == 0 => raster8(&DEFAULT_8X8_INTRA),
                List::Default => raster8(&DEFAULT_8X8_INTER),
                List::Absent => fallback.m8[i],
            };
        }
        m
    }
}

fn raster4(zigzag: &[u8]) -> [u8; 16] {
    let mut out = [16; 16];
    for (k, &v) in zigzag.iter().take(16).enumerate() {
        out[ZIGZAG_4X4[k]] = v;
    }
    out
}

fn raster8(zigzag: &[u8]) -> [u8; 64] {
    let mut out = [16; 64];
    for (k, &v) in zigzag.iter().take(64).enumerate() {
        out[ZIGZAG_8X8[k]] = v;
    }
    out
}

fn scaling_list(b: &mut Bits, size: usize) -> Result<List> {
    let mut list = vec![0u8; size];
    let (mut last, mut next) = (8i32, 8i32);
    for (j, slot) in list.iter_mut().enumerate() {
        if next != 0 {
            let delta = b.se()?;
            ensure!(
                (-128..=127).contains(&delta),
                "scaling list delta out of range"
            );
            next = (last + delta + 256) % 256;
            if j == 0 && next == 0 {
                return Ok(List::Default);
            }
        }
        let value = if next == 0 { last } else { next };
        *slot = value as u8;
        last = value;
    }
    Ok(List::Coded(list))
}

fn scaling_lists(b: &mut Bits, count: usize) -> Result<Vec<List>> {
    (0..count)
        .map(|i| {
            if b.flag()? {
                scaling_list(b, if i < 6 { 16 } else { 64 })
            } else {
                Ok(List::Absent)
            }
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Sps {
    id: u32,
    profile_idc: u8,
    constraint_set1: bool,
    level_idc: u8,
    scaling: Option<Vec<List>>,
    log2_max_frame_num: u32,
    poc_type: u32,
    log2_max_poc_lsb: u32,
    delta_pic_order_always_zero: bool,
    offset_for_non_ref_pic: i32,
    offset_for_top_to_bottom_field: i32,
    offset_for_ref_frame: Vec<i32>,
    max_num_ref_frames: u32,
    gaps_allowed: bool,
    width_mbs: u32,
    height_mbs: u32,
    direct_8x8_inference: bool,
    crop: Rect,
}

impl Sps {
    pub(super) fn parse(rbsp: &[u8]) -> Result<Self> {
        let mut b = Bits::new(rbsp);
        b.skip(8)?; // NAL header
        let profile_idc = b.u(8)? as u8;
        let constraints = b.u(8)?;
        let level_idc = b.u(8)? as u8;
        let id = b.ue_max(31, "seq_parameter_set_id")?;
        let mut scaling = None;
        if matches!(
            profile_idc,
            100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
        ) {
            let chroma_format_idc = b.ue()?;
            ensure!(
                chroma_format_idc == 1,
                "H.264 chroma format {chroma_format_idc} is not 4:2:0"
            );
            let luma = b.ue()?;
            let chroma = b.ue()?;
            ensure!(luma == 0 && chroma == 0, "H.264 above 8 bits");
            ensure!(!b.flag()?, "H.264 lossless coding");
            if b.flag()? {
                scaling = Some(scaling_lists(&mut b, 8)?);
            }
        }
        let log2_max_frame_num = b.ue_max(12, "log2_max_frame_num_minus4")? + 4;
        let poc_type = b.ue_max(2, "pic_order_cnt_type")?;
        let (mut log2_max_poc_lsb, mut delta_pic_order_always_zero) = (0, false);
        let (mut offset_for_non_ref_pic, mut offset_for_top_to_bottom_field) = (0, 0);
        let mut offset_for_ref_frame = Vec::new();
        match poc_type {
            0 => log2_max_poc_lsb = b.ue_max(12, "log2_max_pic_order_cnt_lsb_minus4")? + 4,
            1 => {
                delta_pic_order_always_zero = b.flag()?;
                offset_for_non_ref_pic = b.se()?;
                offset_for_top_to_bottom_field = b.se()?;
                let n = b.ue_max(255, "num_ref_frames_in_pic_order_cnt_cycle")?;
                for _ in 0..n {
                    offset_for_ref_frame.push(b.se()?);
                }
            }
            _ => {}
        }
        let max_num_ref_frames = b.ue_max(16, "max_num_ref_frames")?;
        let gaps_allowed = b.flag()?;
        let width_mbs = b.ue_max(1023, "pic_width_in_mbs_minus1")? + 1;
        let height_mbs = b.ue_max(1023, "pic_height_in_map_units_minus1")? + 1;
        ensure!(b.flag()?, "interlaced H.264");
        let direct_8x8_inference = b.flag()?;
        let (w, h) = (width_mbs * 16, height_mbs * 16);
        let mut crop = Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        };
        if b.flag()? {
            let (l, r, t, bo) = (
                b.ue_max(w / 2, "crop left")? * 2,
                b.ue_max(w / 2, "crop right")? * 2,
                b.ue_max(h / 2, "crop top")? * 2,
                b.ue_max(h / 2, "crop bottom")? * 2,
            );
            ensure!(
                l + r < w && t + bo < h,
                "H.264 cropping exceeds the picture"
            );
            crop = Rect {
                x: l,
                y: t,
                width: w - l - r,
                height: h - t - bo,
            };
        }
        Ok(Self {
            id,
            profile_idc,
            constraint_set1: constraints & 0x40 != 0,
            level_idc,
            scaling,
            log2_max_frame_num,
            poc_type,
            log2_max_poc_lsb,
            delta_pic_order_always_zero,
            offset_for_non_ref_pic,
            offset_for_top_to_bottom_field,
            offset_for_ref_frame,
            max_num_ref_frames,
            gaps_allowed,
            width_mbs,
            height_mbs,
            direct_8x8_inference,
            crop,
        })
    }

    fn matrices(&self) -> Matrices {
        match &self.scaling {
            None => Matrices::FLAT,
            Some(lists) => Matrices::resolve(lists, &Matrices::defaults()),
        }
    }

    /// The least VA profile this stream needs: a Constrained Baseline
    /// stream decodes on Main and High too, Main on High.
    fn profile(&self) -> Result<Profile> {
        Ok(match self.profile_idc {
            // Full Baseline decodes the same without FMO, which a PPS
            // refuses, and ASO, which Latch's encoders do not use.
            66 => Profile::H264ConstrainedBaseline,
            77 => Profile::H264Main,
            100 => Profile::H264High,
            other => bail!("H.264 profile {other}"),
        })
    }

    fn format(&self) -> Result<Format> {
        Ok(Format {
            profile: self.profile()?,
            width: self.width_mbs * 16,
            height: self.height_mbs * 16,
            // References, the picture being decoded, and one spare.
            surfaces: self.max_num_ref_frames.max(1) as usize + 2,
            visible: self.crop,
        })
    }

    fn max_frame_num(&self) -> u32 {
        1 << self.log2_max_frame_num
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Pps {
    id: u32,
    sps_id: u32,
    entropy_coding_mode: bool,
    bottom_field_pic_order_in_frame_present: bool,
    num_ref_idx_default: [u32; 2],
    weighted_pred: bool,
    weighted_bipred_idc: u32,
    pic_init_qp_minus26: i32,
    pic_init_qs_minus26: i32,
    chroma_qp_index_offset: i32,
    deblocking_filter_control_present: bool,
    constrained_intra_pred: bool,
    redundant_pic_cnt_present: bool,
    transform_8x8_mode: bool,
    scaling: Option<Vec<List>>,
    second_chroma_qp_index_offset: i32,
}

impl Pps {
    pub(super) fn parse(rbsp: &[u8]) -> Result<Self> {
        let mut b = Bits::new(rbsp);
        b.skip(8)?;
        let id = b.ue_max(255, "pic_parameter_set_id")?;
        let sps_id = b.ue_max(31, "seq_parameter_set_id")?;
        let entropy_coding_mode = b.flag()?;
        let bottom_field_pic_order_in_frame_present = b.flag()?;
        ensure!(b.ue()? == 0, "H.264 slice groups (FMO)");
        let num_ref_idx_default = [
            b.ue_max(31, "num_ref_idx_l0_default_active_minus1")? + 1,
            b.ue_max(31, "num_ref_idx_l1_default_active_minus1")? + 1,
        ];
        let weighted_pred = b.flag()?;
        let weighted_bipred_idc = b.u(2)?;
        ensure!(weighted_bipred_idc < 3, "weighted_bipred_idc 3");
        let pic_init_qp_minus26 = b.se_range(-26, 25, "pic_init_qp_minus26")?;
        let pic_init_qs_minus26 = b.se_range(-26, 25, "pic_init_qs_minus26")?;
        let chroma_qp_index_offset = b.se_range(-12, 12, "chroma_qp_index_offset")?;
        let deblocking_filter_control_present = b.flag()?;
        let constrained_intra_pred = b.flag()?;
        let redundant_pic_cnt_present = b.flag()?;
        let (mut transform_8x8_mode, mut scaling) = (false, None);
        let mut second_chroma_qp_index_offset = chroma_qp_index_offset;
        if b.more_rbsp_data() {
            transform_8x8_mode = b.flag()?;
            if b.flag()? {
                scaling = Some(scaling_lists(&mut b, 6 + 2 * transform_8x8_mode as usize)?);
            }
            second_chroma_qp_index_offset = b.se_range(-12, 12, "second_chroma_qp_index_offset")?;
        }
        Ok(Self {
            id,
            sps_id,
            entropy_coding_mode,
            bottom_field_pic_order_in_frame_present,
            num_ref_idx_default,
            weighted_pred,
            weighted_bipred_idc,
            pic_init_qp_minus26,
            pic_init_qs_minus26,
            chroma_qp_index_offset,
            deblocking_filter_control_present,
            constrained_intra_pred,
            redundant_pic_cnt_present,
            transform_8x8_mode,
            scaling,
            second_chroma_qp_index_offset,
        })
    }

    fn matrices(&self, sps: &Sps) -> Matrices {
        match &self.scaling {
            None => sps.matrices(),
            Some(lists) if sps.scaling.is_some() => Matrices::resolve(lists, &sps.matrices()),
            Some(lists) => Matrices::resolve(lists, &Matrices::defaults()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mmco {
    ForgetShort(u32),
    ForgetLong(u32),
    ShortToLong(u32, u32),
    MaxLong(u32),
    ForgetAll,
    CurrentToLong(u32),
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Marking {
    long_term_reference: bool,
    adaptive: Option<Vec<Mmco>>,
}

type Weight = (i16, i16);

#[derive(Clone, Debug, Default, PartialEq)]
struct ListWeights {
    luma: Vec<Option<Weight>>,
    chroma: Vec<Option<[Weight; 2]>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Weights {
    luma_denom: u32,
    chroma_denom: u32,
    lists: [ListWeights; 2],
}

const P: u32 = 0;
const B: u32 = 1;
const I: u32 = 2;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Slice {
    nal_ref_idc: u32,
    idr: bool,
    first_mb: u32,
    slice_type: u32,
    pps_id: u32,
    frame_num: u32,
    poc_lsb: u32,
    delta_poc_bottom: i32,
    delta_poc: [i32; 2],
    redundant_pic_cnt: u32,
    direct_spatial_mv_pred: bool,
    num_ref_idx: [u32; 2],
    modifications: [Vec<(u32, u32)>; 2],
    weights: Option<Weights>,
    marking: Marking,
    cabac_init_idc: u32,
    qp_delta: i32,
    deblocking: (u32, i32, i32),
    /// RBSP bits before the slice data, NAL header included.
    header_bits: usize,
}

impl Slice {
    /// The slice header, and the id of the SPS it uses.
    pub(super) fn parse(
        rbsp: &[u8],
        sps: &[Option<Sps>],
        pps: &[Option<Pps>],
    ) -> Result<(Self, u32)> {
        let mut b = Bits::new(rbsp);
        b.skip(1)?;
        let nal_ref_idc = b.u(2)?;
        let idr = b.u(5)? == 5;
        let first_mb = b.ue()?;
        let slice_type = b.ue_max(9, "slice_type")? % 5;
        ensure!(slice_type <= I, "H.264 SP/SI slices");
        let pps_id = b.ue_max(255, "pic_parameter_set_id")?;
        let p = pps[pps_id as usize]
            .as_ref()
            .with_context(|| format!("H.264 slice before its PPS {pps_id}"))?;
        let s = sps[p.sps_id as usize]
            .as_ref()
            .with_context(|| format!("H.264 PPS before its SPS {}", p.sps_id))?;
        ensure!(
            first_mb < s.width_mbs * s.height_mbs && first_mb <= u16::MAX as u32,
            "H.264 first macroblock outside the picture or VA-API limits"
        );
        let frame_num = b.u(s.log2_max_frame_num)?;
        if idr {
            b.ue()?; // idr_pic_id
        }
        let (mut poc_lsb, mut delta_poc_bottom, mut delta_poc) = (0, 0, [0; 2]);
        if s.poc_type == 0 {
            poc_lsb = b.u(s.log2_max_poc_lsb)?;
            if p.bottom_field_pic_order_in_frame_present {
                delta_poc_bottom = b.se()?;
            }
        }
        if s.poc_type == 1 && !s.delta_pic_order_always_zero {
            delta_poc[0] = b.se()?;
            if p.bottom_field_pic_order_in_frame_present {
                delta_poc[1] = b.se()?;
            }
        }
        let redundant_pic_cnt = if p.redundant_pic_cnt_present {
            b.ue()?
        } else {
            0
        };
        let direct_spatial_mv_pred = slice_type == B && b.flag()?;
        let mut num_ref_idx = [0, 0];
        if slice_type != I {
            num_ref_idx = p.num_ref_idx_default;
            if b.flag()? {
                num_ref_idx[0] = b.ue_max(31, "num_ref_idx_l0_active_minus1")? + 1;
                if slice_type == B {
                    num_ref_idx[1] = b.ue_max(31, "num_ref_idx_l1_active_minus1")? + 1;
                }
            }
            if slice_type != B {
                num_ref_idx[1] = 0;
            }
        }
        let mut modifications = [Vec::new(), Vec::new()];
        for (list, mods) in modifications.iter_mut().enumerate() {
            if num_ref_idx[list] == 0 || !b.flag()? {
                continue;
            }
            loop {
                let idc = b.ue_max(3, "modification_of_pic_nums_idc")?;
                if idc == 3 {
                    break;
                }
                ensure!(mods.len() <= 32, "too many reordering commands");
                mods.push((
                    idc,
                    b.ue_max(
                        if idc < 2 { s.max_frame_num() - 1 } else { 15 },
                        "reference list command",
                    )?,
                ));
            }
        }
        let weights = if (p.weighted_pred && slice_type == P)
            || (p.weighted_bipred_idc == 1 && slice_type == B)
        {
            Some(pred_weight_table(&mut b, num_ref_idx)?)
        } else {
            None
        };
        let mut marking = Marking::default();
        if nal_ref_idc != 0 {
            if idr {
                b.flag()?; // no_output_of_prior_pics_flag
                marking.long_term_reference = b.flag()?;
            } else if b.flag()? {
                let mut ops = Vec::new();
                loop {
                    let op = match b.ue_max(6, "memory_management_control_operation")? {
                        0 => break,
                        1 => Mmco::ForgetShort(
                            b.ue_max(s.max_frame_num() - 1, "difference_of_pic_nums_minus1")?,
                        ),
                        2 => Mmco::ForgetLong(b.ue_max(15, "long_term_pic_num")?),
                        3 => Mmco::ShortToLong(
                            b.ue_max(s.max_frame_num() - 1, "difference_of_pic_nums_minus1")?,
                            b.ue_max(15, "long_term_frame_idx")?,
                        ),
                        4 => Mmco::MaxLong(b.ue_max(16, "max_long_term_frame_idx_plus1")?),
                        5 => Mmco::ForgetAll,
                        _ => Mmco::CurrentToLong(b.ue_max(15, "long_term_frame_idx")?),
                    };
                    ensure!(ops.len() < 66, "too many memory management operations");
                    ops.push(op);
                }
                marking.adaptive = Some(ops);
            }
        }
        let cabac_init_idc = if p.entropy_coding_mode && slice_type != I {
            b.ue_max(2, "cabac_init_idc")?
        } else {
            0
        };
        let qp_delta = b.se_range(-51, 51, "slice_qp_delta")?;
        let mut deblocking = (0, 0, 0);
        if p.deblocking_filter_control_present {
            deblocking.0 = b.ue_max(2, "disable_deblocking_filter_idc")?;
            if deblocking.0 != 1 {
                deblocking.1 = b.se_range(-6, 6, "slice_alpha_c0_offset_div2")?;
                deblocking.2 = b.se_range(-6, 6, "slice_beta_offset_div2")?;
            }
        }
        Ok((
            Self {
                nal_ref_idc,
                idr,
                first_mb,
                slice_type,
                pps_id,
                frame_num,
                poc_lsb,
                delta_poc_bottom,
                delta_poc,
                redundant_pic_cnt,
                direct_spatial_mv_pred,
                num_ref_idx,
                modifications,
                weights,
                marking,
                cabac_init_idc,
                qp_delta,
                deblocking,
                header_bits: b.position(),
            },
            p.sps_id,
        ))
    }
}

fn pred_weight_table(b: &mut Bits, num_ref_idx: [u32; 2]) -> Result<Weights> {
    let mut w = Weights {
        luma_denom: b.ue_max(7, "luma_log2_weight_denom")?,
        chroma_denom: b.ue_max(7, "chroma_log2_weight_denom")?,
        ..Default::default()
    };
    for (list, &count) in num_ref_idx.iter().enumerate() {
        for _ in 0..count {
            let luma = if b.flag()? {
                Some((
                    b.se_range(-128, 127, "prediction weight/offset")? as i16,
                    b.se_range(-128, 127, "prediction weight/offset")? as i16,
                ))
            } else {
                None
            };
            let chroma = if b.flag()? {
                Some([
                    (
                        b.se_range(-128, 127, "prediction weight/offset")? as i16,
                        b.se_range(-128, 127, "prediction weight/offset")? as i16,
                    ),
                    (
                        b.se_range(-128, 127, "prediction weight/offset")? as i16,
                        b.se_range(-128, 127, "prediction weight/offset")? as i16,
                    ),
                ])
            } else {
                None
            };
            w.lists[list].luma.push(luma);
            w.lists[list].chroma.push(chroma);
        }
    }
    Ok(w)
}

/// A reference picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Picture {
    /// `INVALID_ID` for a frame inferred for a gap in `frame_num`.
    surface: Id,
    frame_num: u32,
    top: i32,
    bottom: i32,
    long_term: Option<u32>,
}

impl Picture {
    fn poc(&self) -> i32 {
        self.top.min(self.bottom)
    }

    fn va(&self) -> PictureH264 {
        PictureH264 {
            picture_id: self.surface,
            frame_idx: self.long_term.unwrap_or(self.frame_num),
            flags: if self.long_term.is_some() {
                va::H264_LONG_TERM_REFERENCE
            } else {
                va::H264_SHORT_TERM_REFERENCE
            },
            TopFieldOrderCnt: self.top,
            BottomFieldOrderCnt: self.bottom,
            va_reserved: [0; 4],
        }
    }
}

/// What the next picture's order count and `frame_num` follow from.
#[derive(Clone, Copy, Debug, Default)]
struct Previous {
    /// The previous reference picture's POC MSB and LSB (type 0).
    poc_msb: i32,
    poc_lsb: i32,
    /// The previous picture's `frame_num` and FrameNumOffset (types 1, 2).
    frame_num: u32,
    frame_num_offset: i32,
    /// The previous reference picture's `frame_num` (gap detection).
    ref_frame_num: u32,
}

#[derive(Default)]
pub struct H264 {
    sps: Vec<Option<Sps>>,
    pps: Vec<Option<Pps>>,
    active: Option<Sps>,
    format: Option<Format>,
    surfaces: Vec<Id>,
    dpb: Vec<Picture>,
    previous: Previous,
    /// No picture until an IDR: nothing to refer to.
    waiting: bool,
}

impl H264 {
    pub fn new() -> Self {
        Self {
            sps: vec![None; 32],
            pps: vec![None; 256],
            waiting: true,
            ..Default::default()
        }
    }

    /// The references are gone (a lost picture, a failed decode): nothing
    /// decodes again before the next IDR.
    fn invalidate(&mut self) {
        self.dpb.clear();
        self.waiting = true;
    }

    /// The hardware failed: the next IDR sets it up again.
    fn reset_hardware(&mut self) {
        self.invalidate();
        self.format = None;
        self.surfaces.clear();
    }

    /// One access unit: its picture's surface once the hardware has it.
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
        let mut slices: Vec<(Slice, &[u8])> = Vec::new();
        let mut sps_id = None;
        for nal in nal_units(au) {
            match nal[0] & 0x1f {
                7 => {
                    let sps = Sps::parse(&rbsp(nal))?;
                    let id = sps.id as usize;
                    self.sps[id] = Some(sps);
                }
                8 => {
                    let pps = Pps::parse(&rbsp(nal))?;
                    let id = pps.id as usize;
                    self.pps[id] = Some(pps);
                }
                1 | 5 => {
                    let (slice, id) = Slice::parse(&rbsp(nal), &self.sps, &self.pps)?;
                    if slice.redundant_pic_cnt > 0 {
                        continue;
                    }
                    if let Some((first, _)) = slices.first() {
                        ensure!(
                            first.frame_num == slice.frame_num && first.idr == slice.idr,
                            "an access unit with more than one picture"
                        );
                    }
                    sps_id = Some(id);
                    slices.push((slice, nal));
                }
                _ => {}
            }
        }
        let Some(sps_id) = sps_id else {
            return Ok(None);
        };
        let first = slices[0].0.clone();
        if first.idr {
            let sps = self.sps[sps_id as usize].clone().expect("parsed");
            let format = sps.format()?;
            if self.format.as_ref() != Some(&format) {
                self.format = None;
                self.surfaces = backend.configure(&format).map_err(hardware)?;
                ensure!(
                    self.surfaces.len() >= format.surfaces,
                    "VA-API gave too few surfaces"
                );
                self.format = Some(format);
            }
            self.active = Some(sps);
            self.dpb.clear();
            self.previous = Previous::default();
            self.waiting = false;
        } else if self.waiting {
            bail!("H.264 picture before an IDR");
        }
        let sps = self.active.clone().expect("an IDR came first");
        ensure!(
            self.sps[sps_id as usize].as_ref() == Some(&sps),
            "H.264 SPS changed without an IDR"
        );
        let pps = self.pps[first.pps_id as usize].clone().expect("parsed");
        if !first.idr {
            self.fill_frame_num_gap(&sps, &first)?;
        }
        let (top, bottom, frame_num_offset) = self.order(&sps, &first)?;
        ensure!(
            (i64::from(top) - i64::from(bottom)).unsigned_abs() <= i32::MAX as u64,
            "H.264 field order difference exceeds VA-API limits"
        );
        let target = *self
            .surfaces
            .iter()
            .find(|&&s| backend.available(s) && !self.dpb.iter().any(|p| p.surface == s))
            .context("no free VA-API surface")?;
        if sps.poc_type == 0 {
            i32::try_from(i64::from(top) - i64::from(first.poc_lsb))
                .context("H.264 POC MSB overflow")?;
        }
        let current = Picture {
            surface: target,
            frame_num: first.frame_num,
            top,
            bottom,
            long_term: None,
        };

        let mut picture: va::PictureParameterH264 = va::zeroed();
        picture.CurrPic = PictureH264 {
            flags: 0,
            ..current.va()
        };
        picture.ReferenceFrames = [PictureH264::NONE; 16];
        for (slot, p) in picture
            .ReferenceFrames
            .iter_mut()
            .zip(self.dpb.iter().filter(|p| p.surface != va::INVALID_ID))
        {
            *slot = p.va();
        }
        picture.picture_width_in_mbs_minus1 = (sps.width_mbs - 1) as u16;
        picture.picture_height_in_mbs_minus1 = (sps.height_mbs - 1) as u16;
        picture.num_ref_frames = sps.max_num_ref_frames as u8;
        picture.seq_fields = pack(&[
            (1, 2),
            (0, 1),
            (sps.gaps_allowed as u32, 1),
            (1, 1),
            (0, 1),
            (sps.direct_8x8_inference as u32, 1),
            ((sps.level_idc >= 31) as u32, 1),
            (sps.log2_max_frame_num - 4, 4),
            (sps.poc_type, 2),
            (sps.log2_max_poc_lsb.saturating_sub(4), 4),
            (sps.delta_pic_order_always_zero as u32, 1),
        ]);
        picture.pic_init_qp_minus26 = pps.pic_init_qp_minus26 as i8;
        picture.pic_init_qs_minus26 = pps.pic_init_qs_minus26 as i8;
        picture.chroma_qp_index_offset = pps.chroma_qp_index_offset as i8;
        picture.second_chroma_qp_index_offset = pps.second_chroma_qp_index_offset as i8;
        picture.pic_fields = pack(&[
            (pps.entropy_coding_mode as u32, 1),
            (pps.weighted_pred as u32, 1),
            (pps.weighted_bipred_idc, 2),
            (pps.transform_8x8_mode as u32, 1),
            (0, 1),
            (pps.constrained_intra_pred as u32, 1),
            (pps.bottom_field_pic_order_in_frame_present as u32, 1),
            (pps.deblocking_filter_control_present as u32, 1),
            (pps.redundant_pic_cnt_present as u32, 1),
            ((first.nal_ref_idc != 0) as u32, 1),
        ]);
        picture.frame_num = first.frame_num as u16;

        let matrices = pps.matrices(&sps);
        let iq = va::IqMatrixH264 {
            ScalingList4x4: matrices.m4,
            ScalingList8x8: matrices.m8,
            va_reserved: [0; 4],
        };

        let mut params = Vec::with_capacity(slices.len());
        for (slice, nal) in &slices {
            ensure!(slice.pps_id == first.pps_id, "slices with different PPSs");
            params.push((
                self.slice_parameters(&sps, slice, &current, nal.len())?,
                *nal,
            ));
        }
        backend
            .h264(target, &picture, &iq, &params)
            .map_err(hardware)?;

        self.previous.frame_num = first.frame_num;
        self.previous.frame_num_offset = frame_num_offset;
        if first.nal_ref_idc != 0 {
            self.mark(&sps, &first, current);
        }
        Ok(Some(Decoded {
            surface: target,
            visible: sps.crop,
            ten_bit: false,
        }))
    }

    /// Frames inferred for `frame_num`s a stream skipped (8.2.5.2): they
    /// take their place in the sliding window but have no picture.
    fn fill_frame_num_gap(&mut self, sps: &Sps, slice: &Slice) -> Result<()> {
        let frame_num = slice.frame_num;
        let max = sps.max_frame_num();
        let prev = self.previous.ref_frame_num;
        if frame_num == prev || frame_num == (prev + 1) % max {
            return Ok(());
        }
        // Lost long-term-only pictures do not introduce short-term references.
        // The next explicit long-term mark/reordering still has to name a
        // reference we retained. Fabricating short-term entries here would
        // change the default list of the resumed LTR stream.
        if !sps.gaps_allowed && self.dpb.iter().all(|p| p.long_term.is_some()) {
            return Ok(());
        }
        let gap = (frame_num + max - prev - 1) % max;
        let window = sps.max_num_ref_frames.max(1);
        let mut unused = (prev + 1) % max;
        // Non-existing pictures are reference frames with inferred zero
        // deltas. Types 1 and 2 run the normal POC process (8.2.5.2).
        let mut inferred = slice.clone();
        inferred.nal_ref_idc = 1;
        inferred.delta_poc = [0; 2];
        inferred.delta_poc_bottom = 0;
        if gap > window {
            // Only the last ones would survive the sliding window.
            self.dpb.retain(|p| p.long_term.is_some());
            unused = (frame_num + max - window) % max;
            // The discarded prefix still contributes to FrameNumOffset.
            inferred.frame_num = (unused + max - 1) % max;
            self.previous.frame_num_offset = self.order(sps, &inferred)?.2;
            self.previous.frame_num = inferred.frame_num;
        }
        while unused != frame_num {
            inferred.frame_num = unused;
            let (top, bottom, offset) = self.order(sps, &inferred)?;
            self.sliding_window(sps, unused);
            self.dpb.push(Picture {
                surface: va::INVALID_ID,
                frame_num: unused,
                top: if sps.poc_type == 0 { 0 } else { top },
                bottom: if sps.poc_type == 0 { 0 } else { bottom },
                long_term: None,
            });
            self.previous.ref_frame_num = unused;
            self.previous.frame_num = unused;
            self.previous.frame_num_offset = offset;
            unused = (unused + 1) % max;
        }
        Ok(())
    }

    /// TopFieldOrderCnt, BottomFieldOrderCnt and FrameNumOffset (8.2.1).
    fn order(&self, sps: &Sps, slice: &Slice) -> Result<(i32, i32, i32)> {
        let prev = &self.previous;
        let max_frame_num = i64::from(sps.max_frame_num());
        let frame_num_offset = if slice.idr {
            0
        } else if prev.frame_num > slice.frame_num {
            i64::from(prev.frame_num_offset) + max_frame_num
        } else {
            i64::from(prev.frame_num_offset)
        };
        let (top, bottom, offset) = match sps.poc_type {
            0 => {
                let max = 1i64 << sps.log2_max_poc_lsb;
                let lsb = i64::from(slice.poc_lsb);
                let msb = if lsb < i64::from(prev.poc_lsb)
                    && i64::from(prev.poc_lsb) - lsb >= max / 2
                {
                    i64::from(prev.poc_msb) + max
                } else if lsb > i64::from(prev.poc_lsb) && lsb - i64::from(prev.poc_lsb) > max / 2 {
                    i64::from(prev.poc_msb) - max
                } else {
                    i64::from(prev.poc_msb)
                };
                let top = msb + lsb;
                (
                    top,
                    top + i64::from(slice.delta_poc_bottom),
                    frame_num_offset,
                )
            }
            1 => {
                let cycle = &sps.offset_for_ref_frame;
                let mut abs = if cycle.is_empty() {
                    0
                } else {
                    frame_num_offset + i64::from(slice.frame_num)
                };
                if slice.nal_ref_idc == 0 && abs > 0 {
                    abs -= 1;
                }
                let mut expected = 0;
                if abs > 0 {
                    let n = cycle.len() as i64;
                    let per_cycle: i64 = cycle.iter().map(|&v| i64::from(v)).sum();
                    expected = (abs - 1) / n * per_cycle;
                    expected += cycle[..=((abs - 1) % n) as usize]
                        .iter()
                        .map(|&v| i64::from(v))
                        .sum::<i64>();
                }
                if slice.nal_ref_idc == 0 {
                    expected += i64::from(sps.offset_for_non_ref_pic);
                }
                let top = expected + i64::from(slice.delta_poc[0]);
                let bottom = top
                    + i64::from(sps.offset_for_top_to_bottom_field)
                    + i64::from(slice.delta_poc[1]);
                (top, bottom, frame_num_offset)
            }
            _ => {
                let poc = if slice.idr {
                    0
                } else if slice.nal_ref_idc == 0 {
                    2 * (frame_num_offset + i64::from(slice.frame_num)) - 1
                } else {
                    2 * (frame_num_offset + i64::from(slice.frame_num))
                };
                (poc, poc, frame_num_offset)
            }
        };
        Ok((
            i32::try_from(top).context("H.264 POC overflow")?,
            i32::try_from(bottom).context("H.264 POC overflow")?,
            i32::try_from(offset).context("H.264 frame number overflow")?,
        ))
    }

    /// FrameNumWrap of a short-term reference, from `frame_num`'s picture.
    fn pic_num(&self, sps: &Sps, p: &Picture, frame_num: u32) -> i32 {
        if p.frame_num > frame_num {
            p.frame_num as i32 - sps.max_frame_num() as i32
        } else {
            p.frame_num as i32
        }
    }

    fn short(&self, sps: &Sps, num: i32, frame_num: u32) -> Option<usize> {
        (0..self.dpb.len()).find(|&i| {
            self.dpb[i].long_term.is_none() && self.pic_num(sps, &self.dpb[i], frame_num) == num
        })
    }

    /// The reference lists of one slice (8.2.4), as DPB indices.
    fn lists(&self, sps: &Sps, slice: &Slice, current: &Picture) -> Result<[Vec<usize>; 2]> {
        let shorts: Vec<usize> = (0..self.dpb.len())
            .filter(|&i| self.dpb[i].long_term.is_none())
            .collect();
        let mut longs: Vec<usize> = (0..self.dpb.len())
            .filter(|&i| self.dpb[i].long_term.is_some())
            .collect();
        longs.sort_by_key(|&i| self.dpb[i].long_term);
        let mut init: [Vec<usize>; 2] = [Vec::new(), Vec::new()];
        match slice.slice_type {
            P => {
                let mut s = shorts;
                s.sort_by_key(|&i| -self.pic_num(sps, &self.dpb[i], slice.frame_num));
                init[0] = s.into_iter().chain(longs.iter().copied()).collect();
            }
            B => {
                let poc = current.poc();
                let (mut before, mut after): (Vec<usize>, Vec<usize>) =
                    shorts.iter().partition(|&&i| self.dpb[i].poc() < poc);
                before.sort_by_key(|&i| -self.dpb[i].poc());
                after.sort_by_key(|&i| self.dpb[i].poc());
                init[0] = before.iter().chain(&after).chain(&longs).copied().collect();
                init[1] = after.iter().chain(&before).chain(&longs).copied().collect();
                if init[1].len() > 1 && init[0] == init[1] {
                    init[1].swap(0, 1);
                }
            }
            _ => {}
        }
        let mut out: [Vec<usize>; 2] = [Vec::new(), Vec::new()];
        for list in 0..2 {
            let active = slice.num_ref_idx[list] as usize;
            if active == 0 {
                continue;
            }
            let mut l: Vec<Option<usize>> = init[list].iter().map(|&i| Some(i)).collect();
            l.resize(active + 1, None);
            let max = sps.max_frame_num() as i32;
            let curr = slice.frame_num as i32;
            let mut pred = curr;
            for (at, &(idc, value)) in slice.modifications[list].iter().enumerate() {
                let wanted = match idc {
                    0 | 1 => {
                        let diff = value as i32 + 1;
                        ensure!(diff <= max, "abs_diff_pic_num out of range");
                        let mut no_wrap = if idc == 0 { pred - diff } else { pred + diff };
                        if no_wrap < 0 {
                            no_wrap += max;
                        } else if no_wrap >= max {
                            no_wrap -= max;
                        }
                        pred = no_wrap;
                        let num = if no_wrap > curr {
                            no_wrap - max
                        } else {
                            no_wrap
                        };
                        self.short(sps, num, slice.frame_num)
                    }
                    _ => (0..self.dpb.len()).find(|&i| self.dpb[i].long_term == Some(value)),
                };
                let wanted = wanted.context("a reordering command names a missing reference")?;
                ensure!(at < active, "more reordering commands than references");
                l.insert(at, Some(wanted));
                let mut kept = at + 1;
                for c in at + 1..l.len() {
                    if l[c] != Some(wanted) {
                        l[kept] = l[c];
                        kept += 1;
                    }
                }
                l.truncate(kept);
                l.resize(active + 1, None);
            }
            l.truncate(active);
            out[list] = l
                .into_iter()
                .map(|e| {
                    let i = e.context("a reference list entry has no picture")?;
                    ensure!(
                        self.dpb[i].surface != va::INVALID_ID,
                        "H.264 refers to a missing picture"
                    );
                    Ok(i)
                })
                .collect::<Result<_>>()?;
        }
        Ok(out)
    }

    fn slice_parameters(
        &self,
        sps: &Sps,
        slice: &Slice,
        current: &Picture,
        size: usize,
    ) -> Result<va::SliceH264> {
        let lists = self.lists(sps, slice, current)?;
        let mut s: va::SliceH264 = va::zeroed();
        s.slice_data_size = size as u32;
        s.slice_data_bit_offset = slice.header_bits as u16;
        s.first_mb_in_slice = slice.first_mb as u16;
        s.slice_type = slice.slice_type as u8;
        s.direct_spatial_mv_pred_flag = slice.direct_spatial_mv_pred as u8;
        s.num_ref_idx_l0_active_minus1 = slice.num_ref_idx[0].saturating_sub(1) as u8;
        s.num_ref_idx_l1_active_minus1 = slice.num_ref_idx[1].saturating_sub(1) as u8;
        s.cabac_init_idc = slice.cabac_init_idc as u8;
        s.slice_qp_delta = slice.qp_delta as i8;
        s.disable_deblocking_filter_idc = slice.deblocking.0 as u8;
        s.slice_alpha_c0_offset_div2 = slice.deblocking.1 as i8;
        s.slice_beta_offset_div2 = slice.deblocking.2 as i8;
        s.RefPicList0 = [PictureH264::NONE; 32];
        s.RefPicList1 = [PictureH264::NONE; 32];
        for (slot, &i) in s.RefPicList0.iter_mut().zip(&lists[0]) {
            *slot = self.dpb[i].va();
        }
        for (slot, &i) in s.RefPicList1.iter_mut().zip(&lists[1]) {
            *slot = self.dpb[i].va();
        }
        if let Some(w) = &slice.weights {
            s.luma_log2_weight_denom = w.luma_denom as u8;
            s.chroma_log2_weight_denom = w.chroma_denom as u8;
            let luma_default = (1i16 << w.luma_denom, 0);
            let chroma_default = [(1i16 << w.chroma_denom, 0); 2];
            for (list, lw) in w.lists.iter().enumerate() {
                let (mut luma_flag, mut chroma_flag) = (0, 0);
                for (i, (luma, chroma)) in lw.luma.iter().zip(&lw.chroma).enumerate().take(32) {
                    luma_flag |= luma.is_some() as u8;
                    chroma_flag |= chroma.is_some() as u8;
                    let (weight, offset) = luma.unwrap_or(luma_default);
                    let c = chroma.unwrap_or(chroma_default);
                    if list == 0 {
                        s.luma_weight_l0[i] = weight;
                        s.luma_offset_l0[i] = offset;
                        s.chroma_weight_l0[i] = [c[0].0, c[1].0];
                        s.chroma_offset_l0[i] = [c[0].1, c[1].1];
                    } else {
                        s.luma_weight_l1[i] = weight;
                        s.luma_offset_l1[i] = offset;
                        s.chroma_weight_l1[i] = [c[0].0, c[1].0];
                        s.chroma_offset_l1[i] = [c[0].1, c[1].1];
                    }
                }
                if list == 0 {
                    s.luma_weight_l0_flag = luma_flag;
                    s.chroma_weight_l0_flag = chroma_flag;
                } else {
                    s.luma_weight_l1_flag = luma_flag;
                    s.chroma_weight_l1_flag = chroma_flag;
                }
            }
        }
        Ok(s)
    }

    /// Sliding-window marking before `frame_num` joins the references.
    fn sliding_window(&mut self, sps: &Sps, frame_num: u32) {
        let window = sps.max_num_ref_frames.max(1) as usize;
        while self.dpb.len() >= window {
            let oldest = (0..self.dpb.len())
                .filter(|&i| self.dpb[i].long_term.is_none())
                .min_by_key(|&i| self.pic_num(sps, &self.dpb[i], frame_num));
            match oldest {
                Some(i) => {
                    self.dpb.remove(i);
                }
                None => break,
            }
        }
    }

    /// Reference marking once a reference picture is decoded (8.2.5).
    fn mark(&mut self, sps: &Sps, slice: &Slice, mut current: Picture) {
        let mut mmco5 = false;
        let mut poc_lsb = slice.poc_lsb as i32;
        let mut poc_msb = current.top - poc_lsb;
        let curr = slice.frame_num;
        if slice.idr {
            self.dpb.clear();
            if slice.marking.long_term_reference {
                current.long_term = Some(0);
            }
        } else if let Some(ops) = &slice.marking.adaptive {
            for &op in ops {
                match op {
                    Mmco::ForgetShort(diff) => {
                        if let Some(i) = self.short(sps, curr as i32 - (diff as i32 + 1), curr) {
                            self.dpb.remove(i);
                        }
                    }
                    Mmco::ForgetLong(num) => self.dpb.retain(|p| p.long_term != Some(num)),
                    Mmco::ShortToLong(diff, idx) => {
                        self.dpb.retain(|p| p.long_term != Some(idx));
                        if let Some(i) = self.short(sps, curr as i32 - (diff as i32 + 1), curr) {
                            self.dpb[i].long_term = Some(idx);
                        }
                    }
                    Mmco::MaxLong(plus1) => {
                        let max = plus1.checked_sub(1);
                        self.dpb
                            .retain(|p| p.long_term.is_none_or(|i| max.is_some_and(|m| i <= m)));
                    }
                    Mmco::ForgetAll => {
                        self.dpb.clear();
                        mmco5 = true;
                    }
                    Mmco::CurrentToLong(idx) => {
                        self.dpb.retain(|p| p.long_term != Some(idx));
                        current.long_term = Some(idx);
                    }
                }
            }
        } else {
            self.sliding_window(sps, curr);
        }
        if mmco5 {
            // The picture is frame 0 at order 0 from here on (8.2.1).
            let temp = current.poc();
            current.top -= temp;
            current.bottom -= temp;
            current.frame_num = 0;
            poc_msb = 0;
            poc_lsb = current.top;
            self.previous.frame_num = 0;
            self.previous.frame_num_offset = 0;
        }
        // A stream that keeps more than it says still leaves a free surface.
        let window = sps.max_num_ref_frames.max(1) as usize;
        while self.dpb.len() >= window {
            let oldest = (0..self.dpb.len())
                .filter(|&i| self.dpb[i].long_term.is_none())
                .min_by_key(|&i| self.pic_num(sps, &self.dpb[i], current.frame_num))
                .unwrap_or(0);
            self.dpb.remove(oldest);
        }
        self.previous.poc_msb = poc_msb;
        self.previous.poc_lsb = poc_lsb;
        self.previous.ref_frame_num = current.frame_num;
        self.dpb.push(current);
    }

    #[cfg(test)]
    fn references(&self) -> Vec<(u32, Option<u32>)> {
        self.dpb
            .iter()
            .map(|p| (p.frame_num, p.long_term))
            .collect()
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::tests::Recorder;
    use super::*;

    const LTR: &[u8] = include_bytes!("../../../tests/fixtures/vt-ltr-overflow.h264");
    const PROBE: &[u8] = include_bytes!("../../../tests/fixtures/decode-probe.h264");
    const BASELINE: &[u8] = include_bytes!("../../../tests/fixtures/gray-bars.h264");

    /// Access units of an Annex B file. `classify` says of each NAL unit
    /// whether it starts an access unit and whether it is a slice.
    pub fn access_units(data: &[u8], classify: impl Fn(&[u8]) -> (bool, bool)) -> Vec<Vec<u8>> {
        let mut units: Vec<Vec<u8>> = Vec::new();
        let mut current = Vec::new();
        let mut has_slice = false;
        for nal in nal_units(data) {
            let (starts, slice) = classify(nal);
            if starts && has_slice {
                units.push(std::mem::take(&mut current));
                has_slice = false;
            }
            current.extend_from_slice(&[0, 0, 0, 1]);
            current.extend_from_slice(nal);
            has_slice |= slice;
        }
        if has_slice {
            units.push(current);
        }
        units
    }

    fn h264_units(data: &[u8]) -> Vec<Vec<u8>> {
        access_units(data, |nal| match nal[0] & 0x1f {
            1 | 5 => (Bits::new(&rbsp(nal)[1..]).ue().unwrap() == 0, true),
            6..=9 => (true, false),
            _ => (false, false),
        })
    }

    /// After a CABAC slice header, `cabac_alignment_one_bit`s run to the
    /// byte boundary: a header parsed a bit too long or short shows here.
    fn assert_cabac_aligned(nal: &[u8], header_bits: usize) {
        let r = rbsp(nal);
        let mut b = Bits::new(&r);
        b.skip(header_bits).unwrap();
        while !b.byte_aligned() {
            assert_eq!(b.bit().unwrap(), 1, "header ends at bit {header_bits}");
        }
    }

    #[test]
    fn a_videotoolbox_stream_with_long_term_references_decodes_throughout() {
        let mut h264 = H264::new();
        let mut backend = Recorder::default();
        let units = h264_units(LTR);
        assert!(units.len() > 20);
        for (n, au) in units.iter().enumerate() {
            let decoded = h264.decode(au, &mut backend).unwrap().expect("a picture");
            assert_eq!((decoded.visible.width, decoded.visible.height), (960, 540));
            let pictures = backend.h264.last().unwrap();
            assert_eq!(pictures.target, decoded.surface);
            for (slice, nal) in &pictures.slices {
                assert_cabac_aligned(nal, slice.slice_data_bit_offset as usize);
                // Every reference a P slice uses is a real, distinct picture
                // that the picture parameters list too.
                if slice.slice_type == P as u8 {
                    let refs = &slice.RefPicList0[..=slice.num_ref_idx_l0_active_minus1 as usize];
                    for r in refs {
                        assert_ne!(r.picture_id, va::INVALID_ID, "unit {n}");
                        assert_ne!(r.picture_id, decoded.surface, "unit {n}");
                        assert!(
                            pictures.picture.ReferenceFrames.contains(r),
                            "unit {n}: a list entry the picture does not list"
                        );
                    }
                }
            }
            assert!(h264.dpb.len() <= 10, "max_num_ref_frames is 10");
        }
        // The stream keeps long-term pictures, and its reordering commands
        // put one first.
        assert!(h264.references().iter().any(|(_, lt)| lt.is_some()));
        assert!(backend.h264.iter().any(|p| p
            .slices
            .iter()
            .any(|(s, _)| s.RefPicList0[0].flags == va::H264_LONG_TERM_REFERENCE)));
    }

    #[test]
    fn the_probe_picture_and_a_baseline_stream_describe_themselves() {
        let mut h264 = H264::new();
        let mut backend = Recorder::default();
        let decoded = h264.decode(PROBE, &mut backend).unwrap().unwrap();
        let format = backend.formats.last().unwrap().clone();
        assert_eq!(format.profile, Profile::H264High);
        assert!(backend.h264[0]
            .iq
            .ScalingList4x4
            .iter()
            .flatten()
            .all(|&v| v > 0));
        assert_eq!((format.width, format.height), (256, 256));
        assert_eq!(decoded.visible.width, 256);
        let p = &backend.h264[0];
        assert_eq!(p.picture.CurrPic.picture_id, decoded.surface);
        assert_eq!(p.picture.CurrPic.flags, 0);
        assert_eq!(p.picture.picture_width_in_mbs_minus1, 15);
        assert_eq!(p.picture.pic_fields & 1, 1, "x264 High uses CABAC");
        for (slice, nal) in &p.slices {
            assert_eq!(slice.slice_type, I as u8);
            assert_cabac_aligned(nal, slice.slice_data_bit_offset as usize);
        }

        let mut h264 = H264::new();
        let mut backend = Recorder::default();
        let decoded = h264.decode(BASELINE, &mut backend).unwrap().unwrap();
        assert_eq!(backend.formats[0].profile, Profile::H264ConstrainedBaseline);
        assert_eq!((decoded.visible.width, decoded.visible.height), (64, 64));
        assert_eq!(backend.h264[0].picture.pic_fields & 1, 0, "CAVLC");
    }

    #[test]
    fn latch_codec_streams_decode_with_their_slices_and_references() {
        let mut encoder = latch_codec::h264::Encoder::new(latch_codec::h264::EncoderConfig {
            width: 96,
            height: 64,
            fps: 30,
            bitrate_kbps: 1_000,
            threads: 2,
        })
        .unwrap();
        let mut picture = latch_codec::Yuv420::new(96, 64);
        let mut h264 = H264::new();
        let mut backend = Recorder::default();
        for n in 0..8u8 {
            picture.y.fill(40 + n * 20);
            let mut au = Vec::new();
            encoder.encode(&picture, n == 0, &mut au).unwrap();
            let decoded = h264.decode(&au, &mut backend).unwrap().unwrap();
            assert_eq!((decoded.visible.width, decoded.visible.height), (96, 64));
            if n > 0 {
                let s = &backend.h264.last().unwrap().slices[0].0;
                assert_eq!(s.slice_type, P as u8);
                assert_eq!(
                    s.RefPicList0[0].picture_id,
                    backend.h264[n as usize - 1].target,
                    "a P frame refers to the one before"
                );
            }
        }
        assert_eq!(backend.formats.len(), 1, "configured once");
    }

    #[test]
    fn cabac_b_pictures_keep_both_reference_lists() {
        let units = h264_units(include_bytes!(
            "../../../tests/fixtures/vaapi-b-frames.h264"
        ));
        assert_eq!(units.len(), 12);
        let mut h264 = H264::new();
        let mut backend = Recorder::default();
        for unit in units {
            h264.decode(&unit, &mut backend).unwrap().unwrap();
        }
        assert!(backend
            .h264
            .iter()
            .any(|p| p.slices.iter().any(|(s, _)| s.slice_type == B as u8)));
        for picture in &backend.h264 {
            for (slice, _) in &picture.slices {
                for reference in slice
                    .RefPicList0
                    .iter()
                    .chain(&slice.RefPicList1)
                    .filter(|p| p.picture_id != va::INVALID_ID)
                {
                    assert_ne!(reference.picture_id, picture.target);
                    assert!(picture.picture.ReferenceFrames.contains(reference));
                }
            }
        }
    }

    #[test]
    fn pictures_wait_for_an_idr_and_failures_need_another() {
        let units = h264_units(LTR);
        let mut h264 = H264::new();
        let mut backend = Recorder::default();
        assert!(h264.decode(&units[1], &mut backend).is_err(), "no IDR yet");
        h264.decode(&units[0], &mut backend).unwrap().unwrap();
        h264.decode(&units[1], &mut backend).unwrap().unwrap();
        backend.fail = true;
        let error = h264.decode(&units[2], &mut backend).unwrap_err();
        assert!(error.is::<super::super::HardwareFailed>());
        backend.fail = false;
        assert!(
            h264.decode(&units[3], &mut backend).is_err(),
            "needs an IDR"
        );
        let configured = backend.formats.len();
        h264.decode(&units[0], &mut backend).unwrap().unwrap();
        assert_eq!(
            backend.formats.len(),
            configured + 1,
            "hardware set up again after it failed"
        );
    }

    fn sps(max_refs: u32, log2_max_frame_num: u32) -> Sps {
        Sps {
            id: 0,
            profile_idc: 100,
            constraint_set1: false,
            level_idc: 40,
            scaling: None,
            log2_max_frame_num,
            poc_type: 2,
            log2_max_poc_lsb: 0,
            delta_pic_order_always_zero: false,
            offset_for_non_ref_pic: 0,
            offset_for_top_to_bottom_field: 0,
            offset_for_ref_frame: Vec::new(),
            max_num_ref_frames: max_refs,
            gaps_allowed: true,
            width_mbs: 4,
            height_mbs: 4,
            direct_8x8_inference: true,
            crop: Rect {
                x: 0,
                y: 0,
                width: 64,
                height: 64,
            },
        }
    }

    fn slice(frame_num: u32, marking: Marking) -> Slice {
        Slice {
            nal_ref_idc: 1,
            idr: false,
            first_mb: 0,
            slice_type: P,
            pps_id: 0,
            frame_num,
            poc_lsb: 0,
            delta_poc_bottom: 0,
            delta_poc: [0; 2],
            redundant_pic_cnt: 0,
            direct_spatial_mv_pred: false,
            num_ref_idx: [1, 0],
            modifications: [Vec::new(), Vec::new()],
            weights: None,
            marking,
            cabac_init_idc: 0,
            qp_delta: 0,
            deblocking: (0, 0, 0),
            header_bits: 0,
        }
    }

    fn picture(frame_num: u32) -> Picture {
        Picture {
            surface: 100 + frame_num,
            frame_num,
            top: 2 * frame_num as i32,
            bottom: 2 * frame_num as i32,
            long_term: None,
        }
    }

    fn adaptive(ops: &[Mmco]) -> Marking {
        Marking {
            long_term_reference: false,
            adaptive: Some(ops.to_vec()),
        }
    }

    #[test]
    fn the_sliding_window_drops_the_oldest_short_term_reference() {
        let sps = sps(2, 4);
        let mut h264 = H264::new();
        for n in 0..4 {
            h264.mark(&sps, &slice(n, Marking::default()), picture(n));
        }
        assert_eq!(h264.references(), [(2, None), (3, None)]);
        // Across a frame_num wrap the oldest is the largest number.
        for n in [14, 15, 0] {
            h264.mark(&sps, &slice(n, Marking::default()), picture(n));
        }
        assert_eq!(h264.references(), [(15, None), (0, None)]);
    }

    #[test]
    fn memory_management_operations_mark_as_the_standard_says() {
        let sps = sps(4, 4);
        let mut h264 = H264::new();
        for n in 0..3 {
            h264.mark(&sps, &slice(n, Marking::default()), picture(n));
        }
        // Frame 0 becomes long-term 1; the current one long-term 0.
        let ops = adaptive(&[
            Mmco::MaxLong(2),
            Mmco::ShortToLong(2, 1),
            Mmco::CurrentToLong(0),
        ]);
        h264.mark(&sps, &slice(3, ops), picture(3));
        assert_eq!(
            h264.references(),
            [(0, Some(1)), (1, None), (2, None), (3, Some(0))]
        );
        // Forget short-term frame 1 and long-term 1.
        let ops = adaptive(&[Mmco::ForgetShort(2), Mmco::ForgetLong(1)]);
        h264.mark(&sps, &slice(4, ops), picture(4));
        assert_eq!(h264.references(), [(2, None), (3, Some(0)), (4, None)]);
        // MaxLong(0) forgets every long-term reference.
        h264.mark(&sps, &slice(5, adaptive(&[Mmco::MaxLong(0)])), picture(5));
        assert_eq!(h264.references(), [(2, None), (4, None), (5, None)]);
        // ForgetAll: the current picture becomes frame 0 at order 0.
        h264.mark(&sps, &slice(6, adaptive(&[Mmco::ForgetAll])), picture(6));
        assert_eq!(h264.references(), [(0, None)]);
        assert_eq!(h264.dpb[0].top, 0);
        assert_eq!(h264.previous.ref_frame_num, 0);
    }

    #[test]
    fn reordering_commands_move_named_references_to_the_front() {
        let sps = sps(4, 4);
        let mut h264 = H264::new();
        for n in 0..3 {
            h264.mark(&sps, &slice(n, Marking::default()), picture(n));
        }
        h264.mark(
            &sps,
            &slice(3, adaptive(&[Mmco::CurrentToLong(5)])),
            picture(3),
        );
        let current = picture(4);
        let mut s = slice(4, Marking::default());
        s.num_ref_idx = [4, 0];
        let order = |h264: &H264, s: &Slice| -> Vec<(u32, Option<u32>)> {
            h264.lists(&sps, s, &current).unwrap()[0]
                .iter()
                .map(|&i| (h264.dpb[i].frame_num, h264.dpb[i].long_term))
                .collect()
        };
        // Initially: short-term by descending PicNum, then long-term.
        assert_eq!(
            order(&h264, &s),
            [(2, None), (1, None), (0, None), (3, Some(5))]
        );
        // Long-term 5 first, then frame 0 (PicNum 4 - 4), the rest after.
        s.modifications[0] = vec![(2, 5), (0, 3)];
        assert_eq!(
            order(&h264, &s),
            [(3, Some(5)), (0, None), (2, None), (1, None)]
        );
        // A list shorter than the references keeps its first entries.
        s.num_ref_idx = [2, 0];
        assert_eq!(order(&h264, &s), [(3, Some(5)), (0, None)]);
        // A command naming nothing is an error, not a guess.
        s.modifications[0] = vec![(2, 9)];
        assert!(h264.lists(&sps, &s, &current).is_err());
    }

    #[test]
    fn b_lists_order_by_distance_in_both_directions() {
        let sps = sps(4, 4);
        let mut h264 = H264::new();
        for n in [0, 1, 3, 4] {
            h264.mark(&sps, &slice(n, Marking::default()), picture(n));
        }
        let mut s = slice(5, Marking::default());
        s.slice_type = B;
        s.num_ref_idx = [4, 4];
        let current = Picture {
            top: 4,
            bottom: 4,
            ..picture(5)
        };
        let lists = h264.lists(&sps, &s, &current).unwrap();
        let frames = |l: &[usize]| l.iter().map(|&i| h264.dpb[i].frame_num).collect::<Vec<_>>();
        assert_eq!(frames(&lists[0]), [1, 0, 3, 4]);
        assert_eq!(frames(&lists[1]), [3, 4, 1, 0]);
    }

    #[test]
    fn a_frame_num_gap_fills_the_window_with_frames_that_have_no_picture() {
        let sps = sps(3, 4);
        let mut h264 = H264::new();
        h264.mark(&sps, &slice(0, Marking::default()), picture(0));
        h264.mark(&sps, &slice(1, Marking::default()), picture(1));
        h264.fill_frame_num_gap(&sps, &slice(3, Marking::default()))
            .unwrap();
        assert_eq!(h264.references(), [(0, None), (1, None), (2, None)]);
        assert_eq!(h264.dpb[2].surface, va::INVALID_ID);
        h264.fill_frame_num_gap(&sps, &slice(14, Marking::default()))
            .unwrap();
        assert_eq!(h264.references(), [(11, None), (12, None), (13, None)]);
    }

    #[test]
    fn gap_frames_preserve_poc_wrap_and_the_discarded_prefix() {
        for poc_type in [1, 2] {
            // A small gap crosses zero, a large gap crosses in its discarded
            // prefix, and a final case wraps at the current picture itself.
            for (previous, current, window, base_offset) in
                [(14, 1, 3, 0), (14, 1, 3, 16), (7, 6, 2, 16), (13, 0, 2, 16)]
            {
                let mut sps = sps(window, 4);
                sps.poc_type = poc_type;
                sps.offset_for_ref_frame = vec![2];
                let mut h264 = H264::new();
                h264.previous = Previous {
                    frame_num: previous,
                    ref_frame_num: previous,
                    frame_num_offset: base_offset,
                    ..Default::default()
                };
                let mut current_slice = slice(current, Marking::default());
                // The inferred pictures must use reference POC and zero deltas.
                current_slice.nal_ref_idc = 0;
                current_slice.delta_poc = [3, 5];
                h264.fill_frame_num_gap(&sps, &current_slice).unwrap();
                for picture in &h264.dpb {
                    let offset = base_offset + if picture.frame_num > previous { 0 } else { 16 };
                    let expected = 2 * (offset + picture.frame_num as i32);
                    assert_eq!((picture.top, picture.bottom), (expected, expected));
                    assert_eq!(picture.surface, va::INVALID_ID);
                }
                let mut reference = current_slice.clone();
                reference.nal_ref_idc = 1;
                reference.delta_poc = [0; 2];
                let offset = base_offset + 16;
                let expected = 2 * (offset + current as i32);
                assert_eq!(
                    h264.order(&sps, &reference).unwrap(),
                    (expected, expected, offset)
                );
            }
        }
    }

    #[test]
    fn gap_frame_offset_overflow_is_reported() {
        let mut h264 = H264::new();
        h264.previous = Previous {
            frame_num: 14,
            ref_frame_num: 14,
            frame_num_offset: i32::MAX - 8,
            ..Default::default()
        };
        let sps = sps(2, 4);
        assert!(h264
            .fill_frame_num_gap(&sps, &slice(1, Marking::default()))
            .is_err());
    }

    #[test]
    fn order_counts_follow_each_type() {
        let mut s = sps(2, 4);
        let h264 = H264::new();
        let mut sl = slice(3, Marking::default());
        assert_eq!(h264.order(&s, &sl).unwrap(), (6, 6, 0));
        sl.nal_ref_idc = 0;
        assert_eq!(
            h264.order(&s, &sl).unwrap().0,
            5,
            "a non-reference picture is odd"
        );
        s.poc_type = 0;
        s.log2_max_poc_lsb = 4;
        let mut h264 = H264::new();
        h264.previous.poc_lsb = 14;
        sl.poc_lsb = 2;
        assert_eq!(
            h264.order(&s, &sl).unwrap().0,
            18,
            "the LSB wrapped forwards"
        );
        h264.previous.poc_lsb = 2;
        h264.previous.poc_msb = 16;
        sl.poc_lsb = 14;
        assert_eq!(h264.order(&s, &sl).unwrap().0, 14, "and backwards");
        s.poc_type = 1;
        s.offset_for_ref_frame = vec![2];
        s.offset_for_non_ref_pic = -1;
        sl.nal_ref_idc = 1;
        let h264 = H264::new();
        assert_eq!(h264.order(&s, &sl).unwrap().0, 6, "three frames of two");
    }

    #[test]
    fn scaling_matrices_fall_back_as_the_standard_says() {
        // Only list 1 (intra Cb) coded, as a flat 20: list 0 takes the
        // default, list 2 copies list 1, list 3 the inter default.
        let mut lists = vec![List::Absent; 8];
        lists[1] = List::Coded(vec![20; 16]);
        let m = Matrices::resolve(&lists, &Matrices::defaults());
        assert_eq!(m.m4[0], raster4(&DEFAULT_4X4_INTRA));
        assert_eq!(m.m4[1], [20; 16]);
        assert_eq!(m.m4[2], [20; 16]);
        assert_eq!(m.m4[3], raster4(&DEFAULT_4X4_INTER));
        assert_eq!(m.m8[1], raster8(&DEFAULT_8X8_INTER));
        // Raster order: the third zig-zag coefficient is row 1, column 0.
        let mut z = [0u8; 16];
        z[2] = 99;
        assert_eq!(raster4(&z)[4], 99);
        // A list whose first delta brings the next scale to zero is the
        // default: se(-8) is 00001 0001.
        let data = [0b0000_1000, 0b1000_0000];
        assert_eq!(
            scaling_list(&mut Bits::new(&data), 16).unwrap(),
            List::Default
        );
    }
}
