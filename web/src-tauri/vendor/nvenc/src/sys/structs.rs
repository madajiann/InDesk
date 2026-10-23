use std::{ffi::c_void, mem::ManuallyDrop};

use crate::sys::{
    enums::{
        NVencAV1PartSize, NVencBFrameRefMode, NVencBitDepth, NVencBufferFormat, NVencBufferUsage,
        NVencCaps, NVencDeviceType, NVencDisplayPicStruct, NVencH264AdaptiveTransformMode,
        NVencH264BDirectMode, NVencH264EntropyCodingMode, NVencH264FMOMode, NVencHEVCCuSize,
        NVencInputResourceType, NVencLookAheadLevel, NVencMVPrecision, NVencMemoryHeap,
        NVencMultiPass, NVencNumRefFrames, NVencOutputStatsLevel, NVencParamsFrameFieldMode,
        NVencParamsRcMode, NVencPicStruct, NVencPicType, NVencQPMapMode, NVencStateRestoreType,
        NVencStereoPackingMode, NVencTemporalFilterLevel, NVencTuningInfo, NVencVUIColorPrimaries,
        NVencVUIMatrixCoeffs, NVencVUITransCharacteristics, NVencVUIVideoFormat,
    },
    version::struct_version,
};

#[derive(Default, Clone, PartialEq, Eq)]
#[repr(C)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl Guid {
    pub const fn from_values(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Self {
        Self {
            data1,
            data2,
            data3,
            data4,
        }
    }

    pub const fn from_u128(uuid: u128) -> Self {
        Self {
            data1: (uuid >> 96) as u32,
            data2: ((uuid >> 80) & 0xffff) as u16,
            data3: ((uuid >> 64) & 0xffff) as u16,
            data4: (uuid as u64).to_be_bytes(),
        }
    }
}

#[repr(C)]
pub struct NVencCapsParam {
    pub version: u32,
    pub caps_to_query: NVencCaps,
    pub rsvd: [u32; 62],
}

pub const NV_ENC_CAPS_PARAM_VERSION: u32 = struct_version(1);

#[repr(C)]
pub struct NVencRestoreEncoderStateParams {
    pub version: u32,
    pub buffer: u32,
    pub state: NVencStateRestoreType,
    pub rsvd: u32,
    pub output_bit_stream: *mut c_void,
    pub completion_event: *mut c_void,
    pub rsvd1: [u32; 64],
    pub rsvd2: [*mut c_void; 64],
}

pub const NV_ENC_RESTORE_STATE_PARAMS_VER: u32 = struct_version(1);

#[repr(C)]
pub struct NVencOutputStatsBlock {
    pub version: u32,
    pub qp: u8,
    pub rsvd: [u8; 3],
    pub bit_count: u32,
    pub satd_cost: u32,
    pub rsvd1: [u32; 12],
}

pub const NV_ENC_OUTPUT_STATS_BLOCK_VER: u32 = struct_version(1);

#[repr(C)]
pub struct NVencOutputStatsRow {
    pub version: u32,
    pub qp: u8,
    pub rsvd: [u8; 3],
    pub bit_count: u32,
    pub satd_cost: u32,
    pub rsvd1: [u32; 12],
}

pub const NV_ENC_OUTPUT_STATS_ROW_VER: u32 = struct_version(1);

#[repr(C)]
pub struct NVencEncodeOutParams {
    pub version: u32,
    pub bitstream_size_in_bytes: u32,
    pub rsvd: [u32; 62],
}

pub const NV_ENC_ENCODE_OUT_PARAMS_VER: u32 = struct_version(1);

#[repr(C)]
pub struct NVencLookAheadPicParams {
    pub version: u32,
    pub rsvd: u32,
    pub input_buffer: *mut c_void,
    pub picture_type: NVencPicType,
    pub rsvd1: [u32; 63],
    pub rsvd2: [*mut c_void; 64],
}

pub const NV_ENC_LOOKAHEAD_PIC_PARAMS_VER: u32 = struct_version(1);

#[repr(C)]
pub struct NVencCreateInputBuffer {
    pub version: u32,
    pub width: u32,
    pub height: u32,
    pub memory_heap: NVencMemoryHeap,
    pub buffer_fmt: NVencBufferFormat,
    pub rsvd: u32,
    pub input_buffer: *mut c_void,
    pub p_sys_mem_buffer: *mut c_void,
    pub rsvd1: [u32; 58],
    pub rsvd2: [*mut c_void; 63],
}

pub const NV_ENC_CREATE_INPUT_BUFFER_VER: u32 = struct_version(1);

#[repr(C)]
pub struct NVencCreateBitstreamBuffer {
    pub version: u32,
    #[deprecated]
    pub size: u32,
    #[deprecated]
    pub memory_heap: NVencMemoryHeap,
    pub rsvd: u32,
    pub bitstream_buffer: *mut c_void,
    pub bitstream_buffer_ptr: *mut c_void,
    pub rsvd1: [u32; 58],
    pub rsvd2: [*mut c_void; 64],
}

pub const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = struct_version(1);

#[repr(C)]
pub struct NVencMVector {
    pub mvx: i16,
    pub mvy: i16,
}

#[repr(C)]
pub struct NVencH264MVData {
    pub mv: [NVencMVector; 4],
    pub mb_type: u8,
    pub partition_type: u8,
    pub rsvd: u16,
    pub mb_cost: u32,
}

#[repr(C)]
pub struct NVencHEVCMVData {
    pub mv: [NVencMVector; 4],
    pub cu_type: u8,
    pub cu_size: u8,
    pub partition_mode: u8,
    pub last_cu_int_ctb: u8,
}

#[repr(C)]
pub struct NVencCreateMVBuffer {
    pub version: u32,
    pub rsvd: u32,
    pub mv_buffer: *mut c_void,
    pub rsvd1: [u32; 254],
    pub rsvd2: [*mut c_void; 63],
}

pub const NV_ENC_CREATE_MV_BUFFER_VER: u32 = struct_version(1);

#[repr(C)]
pub struct NVencQP {
    pub qp_inter_p: u32,
    pub qp_inter_b: u32,
    pub qp_intra: u32,
}

pub const MAX_NUM_VIEWS_MINUS_1: usize = 7;

#[repr(C)]
pub struct NVencRcParams {
    pub version: u32,
    pub rate_control_mode: NVencParamsRcMode,
    pub const_qp: NVencQP,
    pub average_bit_rate: u32,
    pub max_bit_rate: u32,
    pub vbv_buffer_size: u32,
    pub vbv_initial_delay: u32,
    // TODO: bit fields
    pub bit_fields: NVencRcParamsBitFlags,
    pub min_qp: NVencQP,
    pub max_qp: NVencQP,
    pub initial_rcqp: NVencQP,
    pub temporal_layer_idx_mask: u32,
    pub temporal_layer_qp: [u8; 8],
    pub target_quality: u8,
    pub target_quality_lsb: u8,
    pub look_ahead_depth: u16,
    pub low_delay_key_frame_scale: u8,
    pub y_dc_qp_index_offset: i8,
    pub u_dc_qp_index_offset: i8,
    pub v_dc_qp_index_offset: i8,
    pub qp_map_mode: NVencQPMapMode,
    pub multi_pass: NVencMultiPass,
    pub alpha_layer_bitrate_ratio: u32,
    pub cb_qp_index_offset: i8,
    pub cr_qp_index_offset: i8,
    pub rsvd2: u16,
    pub lookahead_level: NVencLookAheadLevel,
    pub view_bitrate_ratios: [u8; MAX_NUM_VIEWS_MINUS_1],
    pub rsvd3: u8,
    pub rsvd1: u32,
}

#[repr(transparent)]
pub struct NVencRcParamsBitFlags(u32);

impl NVencRcParamsBitFlags {}

pub const NV_ENC_RC_PARAMS_VER: u32 = struct_version(1);

pub const MAX_NUM_CLOCK_TS: usize = 3;

#[repr(C)]
pub struct NVencClockTimeStampSet {
    pub bitflags: u32,
    pub time_offset: u32,
}

impl NVencClockTimeStampSet {}

#[repr(C)]
pub struct NVencTimeCode {
    pub display_pic_struct: NVencDisplayPicStruct,
    pub clock_timestamp: [NVencClockTimeStampSet; MAX_NUM_CLOCK_TS],
    pub skip_clock_timestamp_insertion: u32,
}

pub const MULTIVIEW_MAX_NUM_REF_DISPLAY: usize = 32;

#[repr(C)]
pub struct HEVC3DReferenceDisplayInfo {
    // TODO
    pub bitflags: u32,
    pub prec_ref_display_width: i32,
    pub prec_ref_viewing_dist: i32,
    pub num_ref_displays_minus1: i32,
    pub left_view_id: [i32; MULTIVIEW_MAX_NUM_REF_DISPLAY],
    pub right_view_id: [i32; MULTIVIEW_MAX_NUM_REF_DISPLAY],
    pub exponent_ref_display_width: [i32; MULTIVIEW_MAX_NUM_REF_DISPLAY],
    pub mantissa_ref_display_width: [i32; MULTIVIEW_MAX_NUM_REF_DISPLAY],
    pub exponent_ref_viewing_distance: [i32; MULTIVIEW_MAX_NUM_REF_DISPLAY],
    pub mantissa_ref_viewing_distance: [i32; MULTIVIEW_MAX_NUM_REF_DISPLAY],
    pub num_sample_shift_plus512: [i32; MULTIVIEW_MAX_NUM_REF_DISPLAY],
    pub additional_shift_present_flag: [u8; MULTIVIEW_MAX_NUM_REF_DISPLAY],
    pub rsvd2: [u32; 4],
}

#[repr(C)]
pub struct ChromaPoints {
    pub x: u16,
    pub y: u16,
}

#[repr(C)]
pub struct MasteringDisplayInfo {
    pub g: ChromaPoints,
    pub b: ChromaPoints,
    pub r: ChromaPoints,
    pub white_point: ChromaPoints,
    pub max_luma: u32,
    pub min_luma: u32,
}

#[repr(C)]
pub struct ContentLightLevel {
    pub max_content_light_level: u16,
    pub max_pic_average_light_level: u16,
}

#[repr(C)]
pub struct NVencConfigH264VUIParameters {
    pub overscan_info_present_flag: u32,
    pub overscan_info: u32,
    pub video_signal_type_present_flag: u32,
    pub video_format: NVencVUIVideoFormat,
    pub video_full_range_flag: u32,
    pub color_description_present_flag: u32,
    pub color_primaries: NVencVUIColorPrimaries,
    pub transfer_characteristics: NVencVUITransCharacteristics,
    pub color_matrix: NVencVUIMatrixCoeffs,
    pub chroma_sample_location_flag: u32,
    pub chroma_sample_location_top: u32,
    pub chroma_sample_location_bot: u32,
    pub bitstream_restriction_flag: u32,
    pub timing_info_present_flag: u32,
    pub num_unit_in_ticks: u32,
    pub time_scale: u32,
    pub rsvd: [u32; 12],
}

pub type NVencConfigHEVCVUIParamaters = NVencConfigH264VUIParameters;

#[repr(C)]
#[derive(Default)]
pub struct NVencExternalMeHintCountsPerBlockType {
    pub bitflags: u32,
    pub rsvd1: [u32; 3],
}

#[repr(transparent)]
pub struct NVencExternalMeHint(i32);

#[repr(C)]
pub struct NVencExternalMeSbHint {
    pub flags1: i16,
    pub flags2: i16,
    pub flags3: i16,
}

#[repr(C)]
pub struct NVencConfigH264 {
    pub bitflags: u32,
    pub level: u32,
    pub idr_period: u32,
    pub seperate_color_plane_flag: u32,
    pub disable_deblocking_filter_idc: u32,
    pub num_termpoal_layers: u32,
    pub sps_id: u32,
    pub pps_id: u32,
    pub adaptive_transform_mode: NVencH264AdaptiveTransformMode,
    pub fmo_mode: NVencH264FMOMode,
    pub bdirect_mode: NVencH264BDirectMode,
    pub entropy_coding_mode: NVencH264EntropyCodingMode,
    pub stereo_mode: NVencStereoPackingMode,
    pub intra_refresh_period: u32,
    pub intra_refresh_count: u32,
    pub max_num_ref_frames: u32,
    pub slice_mode: u32,
    pub slice_mode_data: u32,
    pub h264_vui_parameters: NVencConfigH264VUIParameters,
    pub ltr_num_frames: u32,
    pub ltr_trust_mode: u32,
    pub chroma_format_idc: u32,
    pub max_termpoal_layers: u32,
    pub use_b_frames_as_ref: NVencBFrameRefMode,
    pub num_ref_l0: NVencNumRefFrames,
    pub num_ref_l1: NVencNumRefFrames,
    pub output_bit_depth: NVencBitDepth,
    pub input_bit_depth: NVencBitDepth,
    pub tf_level: NVencTemporalFilterLevel,
    pub rsvd1: [u32; 264],
    pub rsvd: [*mut c_void; 64],
}

#[repr(C)]
pub struct NVencConfigHEVC {
    pub level: u32,
    pub tier: u32,
    pub min_cu_size: NVencHEVCCuSize,
    pub max_cu_size: NVencHEVCCuSize,
    pub bitflags: u32,
    pub idr_period: u32,
    pub intra_refresh_cnt: u32,
    pub max_num_ref_frames_in_dpb: u32,
    pub ltr_num_frames: u32,
    pub vps_id: u32,
    pub sps_id: u32,
    pub pps_id: u32,
    pub slice_mode: u32,
    pub slice_mode_data: u32,
    pub max_temporal_layers_minus_1: u32,
    pub hevc_vui_paramters: NVencConfigHEVCVUIParamaters,
    pub ltr_trust_mode: u32,
    pub use_b_frames_as_ref: NVencBFrameRefMode,
    pub tf_level: NVencTemporalFilterLevel,
    pub disable_deblocking_filter_idc: u32,
    pub output_bit_depth: NVencBitDepth,
    pub input_bit_depth: NVencBitDepth,
    pub num_temporal_layers: u32,
    pub num_views: u32,
    pub rsvd1: [u32; 208],
    pub rsvd2: [*mut c_void; 64],
}

pub const NV_MAX_TILE_COLS_AV1: usize = 64;
pub const NV_MAX_TILE_ROWS_AV1: usize = 64;

#[repr(C)]
pub struct NVencFilmGrainParamsAV1 {
    pub bitfields: u32,
    pub point_y_value: [u8; 14],
    pub point_y_scaling: [u8; 14],
    pub point_cb_value: [u8; 10],
    pub point_cb_scaling: [u8; 10],
    pub point_cr_value: [u8; 10],
    pub point_cr_scaling: [u8; 10],
    pub ar_coeffs_y_plus128: [u8; 24],
    pub ar_coeffs_cb_plus128: [u8; 25],
    pub ar_coeffs_cr_plus128: [u8; 25],
    pub rsvd2: [u8; 2],
    pub cb_mult: u8,
    pub cb_luma_mult: u8,
    pub cb_offset: u16,
    pub cr_mult: u8,
    pub cr_luma_mult: u8,
    pub cr_offset: u16,
}

#[repr(C)]
pub struct NVencConfigAV1 {
    pub level: u32,
    pub tier: u32,
    pub min_part_size: NVencAV1PartSize,
    pub max_part_size: NVencAV1PartSize,
    pub bitflags: u32,
    pub idr_period: u32,
    pub intra_refresh_period: u32,
    pub intra_refresh_count: u32,
    pub max_num_ref_frames_in_dpb: u32,
    pub num_tiles_columns: u32,
    pub num_tile_rows: u32,
    pub rsvd2: u32,
    pub tile_widths: *mut u32,
    pub tile_heights: *mut u32,
    pub max_temporal_layers_minus_1: u32,
    pub color_primaries: NVencVUIColorPrimaries,
    pub transfer_characteristics: NVencVUITransCharacteristics,
    pub matrix_coefficients: NVencVUIMatrixCoeffs,
    pub color_range: u32,
    pub chroma_sample_position: u32,
    pub use_b_frames_as_ref: NVencBFrameRefMode,
    pub film_grain_params: NVencFilmGrainParamsAV1,
    pub num_fwd_refs: NVencNumRefFrames,
    pub num_bwd_refs: NVencNumRefFrames,
    pub input_bit_depth: NVencBitDepth,
    pub ltr_num_frames: u32,
    pub num_temporal_layers: u32,
    pub tf_level: NVencTemporalFilterLevel,
    pub rsvd1: [u32; 230],
    pub rsvd3: [*mut c_void; 62],
}

#[repr(C)]
pub struct NVencConfigH264MeOnly {
    pub bitflags: u32,
    pub rsvd1: [u32; 255],
    pub rsvd2: [*mut c_void; 64],
}

#[repr(C)]
pub struct NVencConfigHEVCMeOnly {
    pub rsvd: [u32; 256],
    pub rsvd1: [*mut c_void; 64],
}

#[repr(C)]
pub union NVencCodecConfig {
    pub h264: ManuallyDrop<NVencConfigH264>,
    pub hevc: ManuallyDrop<NVencConfigHEVC>,
    pub av1: ManuallyDrop<NVencConfigAV1>,
    pub h264_meonly: ManuallyDrop<NVencConfigH264MeOnly>,
    pub hevc_meonly: ManuallyDrop<NVencConfigHEVCMeOnly>,
    pub rsvd: [u32; 320],
}

#[repr(C)]
pub struct NVencConfig {
    pub version: u32,
    pub profile_guid: Guid,
    pub gop_len: u32,
    pub frame_interval_p: i32,
    pub mono_chrome_encoding: u32,
    pub frame_field_mode: NVencParamsFrameFieldMode,
    pub mv_precision: NVencMVPrecision,
    pub rc_params: NVencRcParams,
    pub encode_codec_config: NVencCodecConfig,
    pub rsvd: [u32; 278],
    pub rsvd2: [*mut c_void; 64],
}

pub const NV_ENC_CONFIG_VER: u32 = struct_version(8) | (1 << 31);

#[bitfields::bitfield(u32)]
pub struct NVencInitializeParamsBitfields {
    #[bits(1)]
    pub report_slice_offsets: bool,
    #[bits(1)]
    pub enable_sub_frame_write: bool,
    #[bits(1)]
    pub enable_external_me_hints: bool,
    #[bits(1)]
    pub enable_me_only_mode: bool,
    #[bits(1)]
    pub enable_weighted_prediction: bool,
    #[bits(4)]
    pub split_encode_mode: u8,
    #[bits(1)]
    pub enable_output_in_vid_mem: bool,
    #[bits(1)]
    pub enable_recon_frame_output: bool,
    #[bits(1)]
    pub enable_output_stats: bool,
    #[bits(1)]
    pub enable_uni_directional_b: bool,
    #[bits(19)]
    pub rsvd: u32,
}

#[repr(C)]
pub struct NVencInitializeParams {
    pub version: u32,
    pub encode_guid: Guid,
    pub preset_guid: Guid,
    pub encode_width: u32,
    pub encode_height: u32,
    pub dar_width: u32,
    pub dar_height: u32,
    pub frame_rate_num: u32,
    pub frame_rate_den: u32,
    pub enable_encode_async: u32,
    pub enable_ptd: u32,
    pub bit_flags: NVencInitializeParamsBitfields,
    pub(crate) priv_data_size: u32,
    pub(crate) rsvd: u32,
    pub(crate) priv_data: *mut c_void,
    pub encode_config: *mut NVencConfig,
    pub max_encode_width: u32,
    pub max_encode_height: u32,
    pub max_me_hint_counts_per_block: [NVencExternalMeHintCountsPerBlockType; 2],
    pub tuning_info: NVencTuningInfo,
    pub buffer_format: NVencBufferFormat,
    pub num_state_buffers: u32,
    pub output_stats_level: NVencOutputStatsLevel,
    pub(crate) rsvd1: [u32; 285],
    pub(crate) rsvd2: [*mut c_void; 64],
}

pub const NV_ENC_INITIALIZE_PARAMS_VER: u32 = struct_version(6) | (1 << 31);

#[repr(C)]
pub struct NVencReconfigureParams {
    pub version: u32,
    pub rsvd: u32,
    pub re_init_encode_params: NVencInitializeParams,
    pub bitflags: u32,
    pub rsvd2: u32,
}

pub const NV_ENC_RECONFIGURE_PARAMS_VER: u32 = struct_version(1) | (1 << 31);

#[repr(C)]
pub struct NVencPresetConfig {
    pub version: u32,
    pub rsvd: u32,
    pub preset_cfg: NVencConfig,
    pub rsvd1: [u32; 256],
    pub rsvd2: [*mut c_void; 64],
}

pub const NV_ENC_PRESET_CONFIG_VER: u32 = struct_version(4) | (1 << 31);

#[repr(C)]
pub struct NVencPicParamsMVC {
    pub version: u32,
    pub view_id: u32,
    pub temporal_id: u32,
    pub priority_id: u32,
    pub rsvd1: [u32; 12],
    pub rsvd2: [*mut c_void; 8],
}

pub const NV_ENC_PIC_PARAMS_MVC_VER: u32 = struct_version(1);

#[repr(C)]
pub union NVencPicParamsH264EXT {
    pub mvc_pic_params: ManuallyDrop<NVencPicParamsMVC>,
    pub rsvd1: [u32; 32],
}

#[repr(C)]
pub struct NVencSeiPayload {
    pub payload_size: u32,
    pub payload_type: u32,
    pub payload: *mut u8,
}

pub type NVencH264SeiPayload = NVencSeiPayload;

#[repr(C)]
pub struct NVencPicParamsH264 {
    pub display_poc_syntax: u32,
    pub rsvd3: u32,
    pub ref_pic_flag: u32,
    pub color_plane_id: u32,
    pub force_intra_refresh_with_frame_count: u32,
    pub bitflags: u32,
    pub slice_type_data: *mut u8,
    pub slice_type_array_count: u32,
    pub sei_payload_array_count: u32,
    pub sei_payload_array: *mut NVencSeiPayload,
    pub slice_mode: u32,
    pub slice_mode_data: u32,
    pub ltr_mark_framce_idx: u32,
    pub ltr_use_frame_bitmap: u32,
    pub ltr_usage_mode: u32,
    pub force_intra_slice_count: u32,
    pub force_intra_slice_idx: *mut u32,
    pub h264_ext_pic_params: NVencPicParamsH264EXT,
    pub time_code: NVencTimeCode,
    pub rsvd: [u32; 202],
    pub rsvd2: [*mut c_void; 61],
}

#[repr(C)]
pub struct NVencPicParamsHEVC {
    pub display_poc_syntax: u32,
    pub ref_pic_flag: u32,
    pub temporal_id: u32,
    pub force_intra_refresh_with_frame_count: u32,
    pub bitflags: u32,
    pub rsvd1: u32,
    pub slice_type_data: *mut u8,
    pub slice_type_array_count: u32,
    pub slice_mode: u32,
    pub slice_mode_data: u32,
    pub ltr_mark_frame_idx: u32,
    pub ltr_usage_frame_bitmap: u32,
    pub ltr_usage_mode: u32,
    pub sei_payload_array_count: u32,
    pub rsvd: u32,
    pub sei_payload_array: *mut NVencSeiPayload,
    pub time_code: NVencTimeCode,
    pub num_temporal_layers: u32,
    pub view_id: u32,
    pub _3d_reference_display_info: *mut HEVC3DReferenceDisplayInfo,
    pub max_cll: *mut ContentLightLevel,
    pub mastering_display: *mut MasteringDisplayInfo,
    pub rsvd2: [u32; 234],
    pub rsvd3: [*mut c_void; 58],
}

pub type NVencAV1OBUPayload = NVencSeiPayload;

#[repr(C)]
pub struct NVencPicParamsAV1 {
    pub display_poc_syntax: u32,
    pub ref_pic_flag: u32,
    pub temporal_id: u32,
    pub force_intra_refresh_with_frame_count: u32,
    pub bitflags: u32,
    pub num_tile_columns: u32,
    pub num_tile_rows: u32,
    pub rsvd: u32,
    pub tile_widths: *mut u32,
    pub tile_heights: *mut u32,
    pub obu_payload_array_count: u32,
    pub rsvd1: u32,
    pub obu_payload_array: *mut NVencAV1OBUPayload,
    pub file_grain_params: *mut NVencFilmGrainParamsAV1,
    pub ltr_mark_frame_idx: u32,
    pub ltr_use_frame_bitmap: u32,
    pub num_temporal_layers: u32,
    pub rsvd4: u32,
    pub max_cll: *mut ContentLightLevel,
    pub mastering_display: *mut MasteringDisplayInfo,
    pub rsvd2: [u32; 242],
    pub rsvd3: [*mut c_void; 59],
}

#[repr(C)]
pub union NVencCodecPicParams {
    pub h264_pic_params: ManuallyDrop<NVencPicParamsH264>,
    pub hevc_pic_params: ManuallyDrop<NVencPicParamsHEVC>,
    pub av1_pic_params: ManuallyDrop<NVencPicParamsAV1>,
    pub rsvd: [u32; 256],
}

#[repr(C)]
pub struct NVencPicParams {
    pub version: u32,
    pub input_width: u32,
    pub input_height: u32,
    pub input_pitch: u32,
    pub encode_pic_flags: u32,
    pub frame_idx: u32,
    pub input_time_stamp: u64,
    pub input_duration: u64,
    pub input_buffer: *mut c_void,
    pub output_bitstream: *mut c_void,
    // TODO: Windows only, needed for async
    pub completion_event: *mut c_void,
    pub buffer_format: NVencBufferFormat,
    pub picture_struct: NVencPicStruct,
    pub picture_type: NVencPicType,
    pub codec_pic_params: NVencCodecPicParams,
    pub me_hint_counts_per_block: [NVencExternalMeHintCountsPerBlockType; 2],
    pub me_external_hints: *mut NVencExternalMeHint,
    pub rsvd2: [u32; 7],
    pub rsvd5: [*mut c_void; 2],
    pub qp_delta_map: *mut i8,
    pub qp_deltra_map_size: u32,
    pub rsvd_bit_fields: u32,
    pub me_hint_ref_pic_dist: [u16; 2],
    pub rsvd4: u32,
    pub alpha_buffer: *mut c_void,
    pub me_external_sb_hints: *mut NVencExternalMeSbHint,
    pub me_sb_hints_count: u32,
    pub state_buffer_idx: u32,
    pub output_recon_buffer: *mut c_void,
    pub rsvd3: [u32; 284],
    pub rsvd6: [*mut c_void; 57],
}

pub const NV_ENC_PIC_PARAMS_VER: u32 = struct_version(6) | (1 << 31);

#[repr(C)]
pub struct NVencMeOnlyParams {
    pub version: u32,
    pub input_width: u32,
    pub input_height: u32,
    pub rsvd: u32,
    pub input_buffer: *mut c_void,
    pub reference_frame: *mut c_void,
    pub mv_buffer: *mut c_void,
    pub rsvd2: u32,
    pub buffer_formtat: NVencBufferFormat,
    pub view_id: u32,
    pub me_hint_counts_per_block: [NVencExternalMeHintCountsPerBlockType; 2],
    pub me_external_hints: NVencExternalMeHint,
    pub rsvd1: [u32; 241],
    pub rsvd3: [*mut c_void; 59],
}

pub const NV_ENC_MEONLY_PARAMS_VER: u32 = struct_version(3);

#[bitfields::bitfield(u32)]
pub struct NVencLockBitStreamBitFields {
    #[bits(1)]
    pub do_not_wait: bool,
    #[bits(1)]
    pub ltr_frame: bool,
    #[bits(1)]
    pub get_rc_stats: bool,
    #[bits(29)]
    pub rsvd: u32,
}

#[repr(C)]
pub struct NVencLockBitStream {
    pub version: u32,
    pub bit_fields: NVencLockBitStreamBitFields,
    pub output_bit_stream: *mut c_void,
    pub slice_offsets: *mut u32,
    pub frame_idx: u32,
    pub hw_encode_status: u32,
    pub num_slices: u32,
    pub bitstream_size_in_bytes: u32,
    pub output_time_stamp: u64,
    pub output_duration: u64,
    pub bitstream_buffer: *mut c_void,
    pub picture_type: NVencPicType,
    pub picture_struct: NVencPicStruct,
    pub framge_avg_qp: u32,
    pub frame_satd: u32,
    pub ltr_frame_idx: u32,
    pub ltr_frame_bitmap: u32,
    pub temporal_id: u32,
    pub intra_mb_count: u32,
    pub inter_mb_count: u32,
    pub average_mvx: i32,
    pub average_mvy: i32,
    pub alpha_layer_size_in_bytes: u32,
    pub output_stats_ptr_size: u32,
    pub rsvd: u32,
    pub output_stats_ptr: *mut c_void,
    pub frame_idx_display: u32,
    pub rsvd1: [u32; 220],
    pub rsvd2: [*mut c_void; 63],
    pub rsvd_interanl: [u32; 8],
}

pub const NV_ENC_LOCK_BITSTREAM_VER: u32 = struct_version(1) | (1 << 31);

#[repr(C)]
pub struct NVencLockInputBuffer {
    pub version: u32,
    pub bitflags: u32,
    pub input_buffer: *mut c_void,
    pub buffer_data_ptr: *mut c_void,
    pub pitch: u32,
    pub rsvd1: [u32; 251],
    pub rsvd2: [*mut c_void; 64],
}

pub const NV_ENC_LOCK_INPUT_BUFFER_VER: u32 = struct_version(1);

#[repr(C)]
pub struct NVencMapInputResource {
    pub version: u32,
    pub sub_resource_idx: u32,
    pub input_resource: *mut c_void,
    pub registered_resource: *mut c_void,
    pub mapped_resource: *mut c_void,
    pub mapped_buffer_format: NVencBufferFormat,
    pub rsvd1: [u32; 251],
    pub rsvd: [*mut c_void; 63],
}

pub const NV_ENC_MAP_INPUT_RESOURCE_VER: u32 = struct_version(4);

#[repr(C)]
pub struct NVencInputResourceOpenGLTex {
    pub texture: u32,
    pub target: u32,
}

#[repr(C)]
pub struct NVencFencePointD3D12 {
    pub version: u32,
    pub rsvd: u32,
    pub p_fence: *mut c_void,
    pub wait_value: u64,
    pub signal_value: u64,
    pub bit_flags: u32,
    pub rsvd1: [u32; 7],
}

pub const NV_ENC_FENCE_POINT_D3D12_VER: u32 = struct_version(1);

#[repr(C)]
pub struct NVencInputResourceD3D12 {
    pub version: u32,
    pub rsvd: u32,
    pub input_buffer: *mut c_void,
    pub input_fence_point: NVencFencePointD3D12,
    pub rsvd1: [u32; 16],
    pub rsvd2: [*mut c_void; 16],
}

pub const NV_ENC_INPUT_RESOURCE_D3D12_VER: u32 = struct_version(1);

#[repr(C)]
pub struct NVencOutputResourceD3D12 {
    pub version: u32,
    pub rsvd: u32,
    pub output_buffer: *mut c_void,
    pub output_fence_point: NVencFencePointD3D12,
    pub rsvd1: [u32; 16],
    pub rsvd2: [*mut c_void; 16],
}

pub const NV_ENC_OUTPUT_RESOURCE_D3D12_VER: u32 = struct_version(1);

#[repr(C)]
pub struct NVencRegisterResource {
    pub version: u32,
    pub resource_type: NVencInputResourceType,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub sub_resource_index: u32,
    pub resource_to_register: *mut c_void,
    pub registered_resource: *mut c_void,
    pub buffer_format: NVencBufferFormat,
    pub buffer_usage: NVencBufferUsage,
    pub input_fence_point: *mut NVencFencePointD3D12,
    pub chroma_offset: [u32; 2],
    pub chroma_offset_in: [u32; 2],
    pub(crate) rsvd1: [u32; 244],
    pub(crate) rsvd2: [*mut c_void; 61],
}

pub const NV_ENC_REGISTER_RESOURCE_VER: u32 = struct_version(4);

#[repr(C)]
pub struct NVencStat {
    pub version: u32,
    pub rsvd: u32,
    pub output_bit_stream: *mut c_void,
    pub bit_stream_size: u32,
    pub pic_type: u32,
    pub last_valid_byte_offset: u32,
    pub slice_offsets: [u32; 16],
    pub pic_idx: u32,
    pub frame_avg_qp: u32,
    pub bit_flags: u32,
    pub ltr_frame_idx: u32,
    pub intra_mb_count: u32,
    pub inter_mb_count: u32,
    pub average_mv_x: i32,
    pub average_mv_y: i32,
    pub rsvd1: [u32; 227],
    pub rsvd2: [*mut c_void; 64],
}

pub const NV_ENC_STAT_VER: u32 = struct_version(1);

#[repr(C)]
pub struct NVencSequenceParamPayload {
    pub version: u32,
    pub in_buffer_size: u32,
    pub sps_id: u32,
    pub pps_id: u32,
    pub spspps_buffer: *mut c_void,
    pub output_spspps_payload_size: *mut u32,
    pub rsvd: [u32; 250],
    pub rsvd2: [*mut c_void; 64],
}

pub const NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER: u32 = struct_version(1);

#[repr(C)]
pub struct NVencEventParams {
    pub version: u32,
    pub rsvd: u32,
    pub completion_event: *mut c_void,
    pub rsvd1: [u32; 254],
    pub rsvd2: [*mut c_void; 64],
}

pub const NV_ENC_EVENT_PARAMS_VER: u32 = struct_version(1);

#[repr(C)]
pub struct NVencOpenEncodeSessionExParams {
    pub version: u32,
    pub device_type: NVencDeviceType,
    pub device: *mut c_void,
    pub rsvd: *mut c_void,
    pub api_version: u32,
    pub rsvd1: [u32; 253],
    pub rsvd2: [*mut c_void; 64],
}

pub const NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = struct_version(1);
