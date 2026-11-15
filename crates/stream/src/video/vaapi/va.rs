//! libva 2.x decode structures (`va.h`, `va_dec_hevc.h`), whose layout
//! libva keeps stable. Sizes and field offsets were checked against libva
//! 2.20's headers on x86-64 and arm64 and are asserted below; C bit-fields
//! are written as the `u32` words they pack into, lowest field first.

#![allow(non_snake_case)]

pub type Id = u32;

pub const INVALID_ID: Id = 0xffff_ffff;

pub const PROFILE_H264_MAIN: i32 = 6;
pub const PROFILE_H264_HIGH: i32 = 7;
pub const PROFILE_H264_CONSTRAINED_BASELINE: i32 = 13;
pub const PROFILE_HEVC_MAIN: i32 = 17;
pub const PROFILE_HEVC_MAIN10: i32 = 18;
pub const ENTRYPOINT_VLD: i32 = 1;

pub const ATTRIB_RT_FORMAT: i32 = 0;
pub const ATTRIB_MAX_PICTURE_WIDTH: i32 = 18;
pub const ATTRIB_MAX_PICTURE_HEIGHT: i32 = 19;
pub const ATTRIB_NOT_SUPPORTED: u32 = 0x8000_0000;
pub const RT_FORMAT_YUV420: u32 = 0x1;
pub const RT_FORMAT_YUV420_10: u32 = 0x100;
pub const FOURCC_NV12: u32 = 0x3231_564e;
pub const FOURCC_P010: u32 = 0x3031_3050;

pub const PICTURE_PARAMETER_BUFFER: i32 = 0;
pub const IQ_MATRIX_BUFFER: i32 = 1;
pub const SLICE_PARAMETER_BUFFER: i32 = 4;
pub const SLICE_DATA_BUFFER: i32 = 5;

pub const H264_INVALID: u32 = 0x01;
pub const H264_SHORT_TERM_REFERENCE: u32 = 0x08;
pub const H264_LONG_TERM_REFERENCE: u32 = 0x10;

pub const HEVC_INVALID: u32 = 0x01;
pub const HEVC_LONG_TERM_REFERENCE: u32 = 0x08;
pub const HEVC_RPS_ST_CURR_BEFORE: u32 = 0x10;
pub const HEVC_RPS_ST_CURR_AFTER: u32 = 0x20;
pub const HEVC_RPS_LT_CURR: u32 = 0x40;

/// Pack C bit-fields, lowest first: `(value, width)` pairs.
pub fn pack(fields: &[(u32, u32)]) -> u32 {
    let mut word = 0;
    let mut shift = 0;
    for &(value, width) in fields {
        word |= (value & ((1u64 << width) - 1) as u32) << shift;
        shift += width;
    }
    debug_assert!(shift <= 32);
    word
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfigAttrib {
    pub kind: i32,
    pub value: u32,
}

/// VASurfaceAttrib's VAGenericValue union is pointer-sized/aligned. Only
/// the integer member (type 1) is used for dimension limits.
#[repr(C)]
#[derive(Clone, Copy)]
pub union GenericValue {
    pub integer: i32,
    pub pointer: *mut std::ffi::c_void,
}

impl Default for GenericValue {
    fn default() -> Self {
        Self {
            pointer: std::ptr::null_mut(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SurfaceAttrib {
    pub kind: i32,
    pub flags: u32,
    pub value_type: i32,
    pub value: GenericValue,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ImageFormat {
    pub fourcc: u32,
    pub byte_order: u32,
    pub bits_per_pixel: u32,
    pub depth: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub alpha_mask: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Image {
    pub image_id: Id,
    pub format: ImageFormat,
    pub buf: Id,
    pub width: u16,
    pub height: u16,
    pub data_size: u32,
    pub num_planes: u32,
    pub pitches: [u32; 3],
    pub offsets: [u32; 3],
    pub num_palette_entries: i32,
    pub entry_bytes: i32,
    pub component_order: [i8; 4],
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PictureH264 {
    pub picture_id: Id,
    pub frame_idx: u32,
    pub flags: u32,
    pub TopFieldOrderCnt: i32,
    pub BottomFieldOrderCnt: i32,
    pub va_reserved: [u32; 4],
}

impl PictureH264 {
    pub const NONE: Self = Self {
        picture_id: INVALID_ID,
        frame_idx: 0,
        flags: H264_INVALID,
        TopFieldOrderCnt: 0,
        BottomFieldOrderCnt: 0,
        va_reserved: [0; 4],
    };
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PictureParameterH264 {
    pub CurrPic: PictureH264,
    pub ReferenceFrames: [PictureH264; 16],
    pub picture_width_in_mbs_minus1: u16,
    pub picture_height_in_mbs_minus1: u16,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub num_ref_frames: u8,
    /// chroma_format_idc:2, residual_colour_transform_flag:1,
    /// gaps_in_frame_num_value_allowed_flag:1, frame_mbs_only_flag:1,
    /// mb_adaptive_frame_field_flag:1, direct_8x8_inference_flag:1,
    /// MinLumaBiPredSize8x8:1, log2_max_frame_num_minus4:4,
    /// pic_order_cnt_type:2, log2_max_pic_order_cnt_lsb_minus4:4,
    /// delta_pic_order_always_zero_flag:1.
    pub seq_fields: u32,
    pub num_slice_groups_minus1: u8,
    pub slice_group_map_type: u8,
    pub slice_group_change_rate_minus1: u16,
    pub pic_init_qp_minus26: i8,
    pub pic_init_qs_minus26: i8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    /// entropy_coding_mode_flag:1, weighted_pred_flag:1,
    /// weighted_bipred_idc:2, transform_8x8_mode_flag:1, field_pic_flag:1,
    /// constrained_intra_pred_flag:1, pic_order_present_flag:1,
    /// deblocking_filter_control_present_flag:1,
    /// redundant_pic_cnt_present_flag:1, reference_pic_flag:1.
    pub pic_fields: u32,
    pub frame_num: u16,
    pub va_reserved: [u32; 8],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IqMatrixH264 {
    pub ScalingList4x4: [[u8; 16]; 6],
    pub ScalingList8x8: [[u8; 64]; 2],
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SliceH264 {
    pub slice_data_size: u32,
    pub slice_data_offset: u32,
    pub slice_data_flag: u32,
    pub slice_data_bit_offset: u16,
    pub first_mb_in_slice: u16,
    pub slice_type: u8,
    pub direct_spatial_mv_pred_flag: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub cabac_init_idc: u8,
    pub slice_qp_delta: i8,
    pub disable_deblocking_filter_idc: u8,
    pub slice_alpha_c0_offset_div2: i8,
    pub slice_beta_offset_div2: i8,
    pub RefPicList0: [PictureH264; 32],
    pub RefPicList1: [PictureH264; 32],
    pub luma_log2_weight_denom: u8,
    pub chroma_log2_weight_denom: u8,
    pub luma_weight_l0_flag: u8,
    pub luma_weight_l0: [i16; 32],
    pub luma_offset_l0: [i16; 32],
    pub chroma_weight_l0_flag: u8,
    pub chroma_weight_l0: [[i16; 2]; 32],
    pub chroma_offset_l0: [[i16; 2]; 32],
    pub luma_weight_l1_flag: u8,
    pub luma_weight_l1: [i16; 32],
    pub luma_offset_l1: [i16; 32],
    pub chroma_weight_l1_flag: u8,
    pub chroma_weight_l1: [[i16; 2]; 32],
    pub chroma_offset_l1: [[i16; 2]; 32],
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PictureHevc {
    pub picture_id: Id,
    pub pic_order_cnt: i32,
    pub flags: u32,
    pub va_reserved: [u32; 4],
}

impl PictureHevc {
    pub const NONE: Self = Self {
        picture_id: INVALID_ID,
        pic_order_cnt: 0,
        flags: HEVC_INVALID,
        va_reserved: [0; 4],
    };
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PictureParameterHevc {
    pub CurrPic: PictureHevc,
    pub ReferenceFrames: [PictureHevc; 15],
    pub pic_width_in_luma_samples: u16,
    pub pic_height_in_luma_samples: u16,
    /// chroma_format_idc:2, separate_colour_plane_flag:1,
    /// pcm_enabled_flag:1, scaling_list_enabled_flag:1,
    /// transform_skip_enabled_flag:1, amp_enabled_flag:1,
    /// strong_intra_smoothing_enabled_flag:1, sign_data_hiding_enabled_flag:1,
    /// constrained_intra_pred_flag:1, cu_qp_delta_enabled_flag:1,
    /// weighted_pred_flag:1, weighted_bipred_flag:1,
    /// transquant_bypass_enabled_flag:1, tiles_enabled_flag:1,
    /// entropy_coding_sync_enabled_flag:1,
    /// pps_loop_filter_across_slices_enabled_flag:1,
    /// loop_filter_across_tiles_enabled_flag:1,
    /// pcm_loop_filter_disabled_flag:1, NoPicReorderingFlag:1,
    /// NoBiPredFlag:1.
    pub pic_fields: u32,
    pub sps_max_dec_pic_buffering_minus1: u8,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub pcm_sample_bit_depth_luma_minus1: u8,
    pub pcm_sample_bit_depth_chroma_minus1: u8,
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_transform_block_size_minus2: u8,
    pub log2_diff_max_min_transform_block_size: u8,
    pub log2_min_pcm_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_pcm_luma_coding_block_size: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub init_qp_minus26: i8,
    pub diff_cu_qp_delta_depth: u8,
    pub pps_cb_qp_offset: i8,
    pub pps_cr_qp_offset: i8,
    pub log2_parallel_merge_level_minus2: u8,
    pub num_tile_columns_minus1: u8,
    pub num_tile_rows_minus1: u8,
    pub column_width_minus1: [u16; 19],
    pub row_height_minus1: [u16; 21],
    /// lists_modification_present_flag:1, long_term_ref_pics_present_flag:1,
    /// sps_temporal_mvp_enabled_flag:1, cabac_init_present_flag:1,
    /// output_flag_present_flag:1, dependent_slice_segments_enabled_flag:1,
    /// pps_slice_chroma_qp_offsets_present_flag:1,
    /// sample_adaptive_offset_enabled_flag:1,
    /// deblocking_filter_override_enabled_flag:1,
    /// pps_disable_deblocking_filter_flag:1,
    /// slice_segment_header_extension_present_flag:1, RapPicFlag:1,
    /// IdrPicFlag:1, IntraPicFlag:1.
    pub slice_parsing_fields: u32,
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    pub num_short_term_ref_pic_sets: u8,
    pub num_long_term_ref_pics_sps: u8,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub pps_beta_offset_div2: i8,
    pub pps_tc_offset_div2: i8,
    pub num_extra_slice_header_bits: u8,
    pub st_rps_bits: u32,
    pub va_reserved: [u32; 8],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SliceHevc {
    pub slice_data_size: u32,
    pub slice_data_offset: u32,
    pub slice_data_flag: u32,
    pub slice_data_byte_offset: u32,
    pub slice_segment_address: u32,
    pub RefPicList: [[u8; 15]; 2],
    /// LastSliceOfPic:1, dependent_slice_segment_flag:1, slice_type:2,
    /// color_plane_id:2, slice_sao_luma_flag:1, slice_sao_chroma_flag:1,
    /// mvd_l1_zero_flag:1, cabac_init_flag:1,
    /// slice_temporal_mvp_enabled_flag:1,
    /// slice_deblocking_filter_disabled_flag:1, collocated_from_l0_flag:1,
    /// slice_loop_filter_across_slices_enabled_flag:1.
    pub LongSliceFlags: u32,
    pub collocated_ref_idx: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub slice_qp_delta: i8,
    pub slice_cb_qp_offset: i8,
    pub slice_cr_qp_offset: i8,
    pub slice_beta_offset_div2: i8,
    pub slice_tc_offset_div2: i8,
    pub luma_log2_weight_denom: u8,
    pub delta_chroma_log2_weight_denom: i8,
    pub delta_luma_weight_l0: [i8; 15],
    pub luma_offset_l0: [i8; 15],
    pub delta_chroma_weight_l0: [[i8; 2]; 15],
    pub ChromaOffsetL0: [[i8; 2]; 15],
    pub delta_luma_weight_l1: [i8; 15],
    pub luma_offset_l1: [i8; 15],
    pub delta_chroma_weight_l1: [[i8; 2]; 15],
    pub ChromaOffsetL1: [[i8; 2]; 15],
    pub five_minus_max_num_merge_cand: u8,
    pub num_entry_point_offsets: u16,
    pub entry_offset_to_subset_array: u16,
    pub slice_data_num_emu_prevn_bytes: u16,
    pub va_reserved: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IqMatrixHevc {
    pub ScalingList4x4: [[u8; 16]; 6],
    pub ScalingList8x8: [[u8; 64]; 6],
    pub ScalingList16x16: [[u8; 64]; 6],
    pub ScalingList32x32: [[u8; 64]; 2],
    pub ScalingListDC16x16: [u8; 6],
    pub ScalingListDC32x32: [u8; 2],
    pub va_reserved: [u32; 4],
}

/// All-zero bytes: valid for every structure here (integers and arrays).
pub fn zeroed<T: Copy>() -> T {
    // SAFETY: only instantiated with the plain-integer structures above.
    unsafe { std::mem::zeroed() }
}

/// Raw input to libva. Keep parameter structures behind a raw pointer so
/// Rust never forms a byte slice over potentially uninitialized C padding.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
pub struct BufferData<'a> {
    pub ptr: *const std::ffi::c_void,
    pub len: usize,
    _lifetime: std::marker::PhantomData<&'a ()>,
}

#[cfg(target_os = "linux")]
pub fn bytes<T: Copy>(value: &T) -> BufferData<'_> {
    BufferData {
        ptr: (value as *const T).cast(),
        len: std::mem::size_of::<T>(),
        _lifetime: std::marker::PhantomData,
    }
}

#[cfg(target_os = "linux")]
pub fn slice(value: &[u8]) -> BufferData<'_> {
    BufferData {
        ptr: value.as_ptr().cast(),
        len: value.len(),
        _lifetime: std::marker::PhantomData,
    }
}

// Checked against libva 2.20 (`sizeof`/`offsetof` on x86-64 and arm64).
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<ConfigAttrib>() == 8);
    assert!(size_of::<SurfaceAttrib>() == 24);
    assert!(offset_of!(SurfaceAttrib, value) == 16);
    assert!(size_of::<ImageFormat>() == 48);
    assert!(size_of::<Image>() == 120);
    assert!(size_of::<PictureH264>() == 36);
    assert!(size_of::<PictureParameterH264>() == 672);
    assert!(offset_of!(PictureParameterH264, picture_width_in_mbs_minus1) == 612);
    assert!(offset_of!(PictureParameterH264, seq_fields) == 620);
    assert!(offset_of!(PictureParameterH264, num_slice_groups_minus1) == 624);
    assert!(offset_of!(PictureParameterH264, pic_init_qp_minus26) == 628);
    assert!(offset_of!(PictureParameterH264, pic_fields) == 632);
    assert!(offset_of!(PictureParameterH264, frame_num) == 636);
    assert!(size_of::<IqMatrixH264>() == 240);
    assert!(size_of::<SliceH264>() == 3128);
    assert!(offset_of!(SliceH264, RefPicList0) == 28);
    assert!(offset_of!(SliceH264, luma_log2_weight_denom) == 2332);
    assert!(offset_of!(SliceH264, luma_weight_l0) == 2336);
    assert!(offset_of!(SliceH264, chroma_weight_l0_flag) == 2464);
    assert!(offset_of!(SliceH264, chroma_weight_l0) == 2466);
    assert!(offset_of!(SliceH264, luma_weight_l1_flag) == 2722);
    assert!(offset_of!(SliceH264, chroma_offset_l1) == 2982);
    assert!(size_of::<PictureHevc>() == 28);
    assert!(size_of::<PictureParameterHevc>() == 604);
    assert!(offset_of!(PictureParameterHevc, pic_width_in_luma_samples) == 448);
    assert!(offset_of!(PictureParameterHevc, pic_fields) == 452);
    assert!(offset_of!(PictureParameterHevc, sps_max_dec_pic_buffering_minus1) == 456);
    assert!(offset_of!(PictureParameterHevc, column_width_minus1) == 476);
    assert!(offset_of!(PictureParameterHevc, slice_parsing_fields) == 556);
    assert!(offset_of!(PictureParameterHevc, log2_max_pic_order_cnt_lsb_minus4) == 560);
    assert!(offset_of!(PictureParameterHevc, st_rps_bits) == 568);
    assert!(size_of::<SliceHevc>() == 264);
    assert!(offset_of!(SliceHevc, RefPicList) == 20);
    assert!(offset_of!(SliceHevc, LongSliceFlags) == 52);
    assert!(offset_of!(SliceHevc, collocated_ref_idx) == 56);
    assert!(offset_of!(SliceHevc, delta_luma_weight_l0) == 66);
    assert!(offset_of!(SliceHevc, five_minus_max_num_merge_cand) == 246);
    assert!(offset_of!(SliceHevc, num_entry_point_offsets) == 248);
    assert!(offset_of!(SliceHevc, slice_data_num_emu_prevn_bytes) == 252);
    assert!(size_of::<IqMatrixHevc>() == 1016);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_fields_pack_lowest_first() {
        assert_eq!(pack(&[(1, 2), (1, 1), (0, 1), (1, 1)]), 0b1_0101);
        assert_eq!(pack(&[(0xff, 4), (1, 1)]), 0b1_1111, "values are masked");
    }

    /// The words a C compiler packs from libva 2.20's bit-fields, for the
    /// same values (printed by a throwaway program against the headers).
    #[test]
    fn bit_fields_match_what_c_packs() {
        // chroma_format_idc 1, frame_mbs_only, log2_max_frame_num_minus4 5,
        // pic_order_cnt_type 2, delta_pic_order_always_zero.
        let seq = pack(&[
            (1, 2),
            (0, 1),
            (0, 1),
            (1, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (5, 4),
            (2, 2),
            (0, 4),
            (1, 1),
        ]);
        assert_eq!(seq, 0x42511);
        // weighted_bipred_idc 2, transform_8x8_mode, reference_pic_flag.
        let pic = pack(&[
            (0, 1),
            (0, 1),
            (2, 2),
            (1, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (1, 1),
        ]);
        assert_eq!(pic, 0x418);
        // HEVC: chroma_format_idc 1, tiles_enabled (bit 14), NoPicReordering (19).
        assert_eq!(1 | (1 << 14) | (1 << 19), 0x84001);
        // LastSliceOfPic, slice_type 2, collocated_from_l0, loop filter across slices.
        assert_eq!(
            pack(&[(1, 1), (0, 1), (2, 2)]) | (1 << 12) | (1 << 13),
            0x3009
        );
    }
}
