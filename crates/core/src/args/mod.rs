//! ffmpeg argv, pure: layer 1 `encode`, layer 2 `output`. Spec: `docs/spec/args.md`.
mod encode;
mod encoders;
mod filters;
mod num;
mod output;

pub use encode::{Encoded, encode};
pub use encoders::encoder_args;
pub use output::{file, iframe, live, remux, vod};

use crate::api::{
    AudioPlan, CodecVariant, CropInfo, HdrMetadata, HwAccel, TonemapAlgo, TonemapCurve,
};
use crate::decide::tonemap::TonemapPath;
use crate::hw::decoders::Surface;
use serde::Deserialize;

/// A ladder rung as the host's `TranscodeProfile`: bitrates are ffmpeg rates (`8M`, `192k`, bps).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub name: String,
    pub max_width: u32,
    pub max_height: u32,
    pub video_bitrate: String,
    pub audio_bitrate: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BurnInKind {
    Text,
    Image,
}

/// `filter` is the libass `-vf` step (`subtitles='...'`) of a text track; images overlay `streamIndex`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BurnIn {
    pub filter: Option<String>,
    #[serde(default)]
    pub stream_index: Option<u32>,
    #[serde(rename = "type")]
    pub kind: BurnInKind,
}

/// One source audio track (`MediaInfo.audio[]`) as the argv maps it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AudioStream {
    pub language: Option<String>,
    pub stream_index: Option<u32>,
}

/// One transcode run: the host's `BuildFfmpegArgsOptions`, field for field.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VodArgs {
    pub input_path: String,
    pub profile: Profile,
    pub output_dir: String,
    pub hw_accel: HwAccel,
    #[serde(default)]
    pub start_segment: u32,
    #[serde(default)]
    pub tonemap: bool,
    #[serde(default)]
    pub burn_in: Option<BurnIn>,
    #[serde(default)]
    pub audio_stream_index: Option<u32>,
    #[serde(default)]
    pub crop: Option<CropInfo>,
    #[serde(default)]
    pub video_only: bool,
    #[serde(default)]
    pub audio_streams: Option<Vec<AudioStream>>,
    #[serde(default)]
    pub video_variant: Option<CodecVariant>,
    #[serde(default)]
    pub source_video_codec: Option<String>,
    #[serde(default)]
    pub source_bit_depth: Option<u32>,
    #[serde(default)]
    pub source_width: Option<u32>,
    #[serde(default)]
    pub source_height: Option<u32>,
    #[serde(default)]
    pub source_video_bitrate_bps: Option<f64>,
    #[serde(default)]
    pub source_hdr_metadata: Option<HdrMetadata>,
    #[serde(default)]
    pub source_dv_profile: Option<u32>,
    #[serde(default)]
    pub source_dv_bl_signal_compat_id: Option<u32>,
    #[serde(default)]
    pub audio_plan: Option<AudioPlan>,
    #[serde(default)]
    pub audio_track_plans: Option<Vec<AudioPlan>>,
    #[serde(default)]
    pub source_start_pts: Option<f64>,
    #[serde(default)]
    pub source_format_start: Option<f64>,
    #[serde(default)]
    pub seek_keyframe_dts: Option<f64>,
    #[serde(default)]
    pub source_clock_break_seconds: Option<f64>,
    #[serde(default)]
    pub source_end_seconds: Option<f64>,
    #[serde(default)]
    pub video_stream_index: Option<u32>,
    #[serde(default)]
    pub encoder_preset: Option<String>,
    #[serde(default)]
    pub tonemap_algo: Option<TonemapAlgo>,
    #[serde(default)]
    pub tonemap_curve: Option<TonemapCurve>,
    #[serde(default)]
    pub source_fps: Option<f64>,
    #[serde(default)]
    pub source_color_space: Option<String>,
    #[serde(default)]
    pub source_color_primaries: Option<String>,
    #[serde(default)]
    pub source_color_transfer: Option<String>,
    #[serde(default)]
    pub source_color_range: Option<String>,
    #[serde(default)]
    pub segment_duration: Option<f64>,
    #[serde(default)]
    pub trusted_stream_info: bool,
    #[serde(default)]
    pub early: bool,
    #[serde(default)]
    pub use_ts: bool,
}

/// Where a remux run starts (the session layer's `remuxRunStart`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemuxRun {
    pub audio_start_seconds: f64,
    pub seek_seconds: Option<f64>,
    pub start_number: u32,
}

/// One copy-video run: the host's `BuildRemuxArgsOptions`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemuxArgs {
    pub input_path: String,
    pub output_dir: String,
    pub run: RemuxRun,
    #[serde(default)]
    pub trusted_stream_info: bool,
    #[serde(default)]
    pub audio_stream_index: Option<u32>,
    #[serde(default)]
    pub source_video_codec: Option<String>,
    #[serde(default)]
    pub audio_streams: Option<Vec<AudioStream>>,
    /// Only its presence is read: ffmpeg then cuts on every keyframe.
    #[serde(default)]
    pub grid: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    pub segment_duration: Option<f64>,
    #[serde(default)]
    pub video_stream_index: Option<u32>,
    #[serde(default)]
    pub source_end_seconds: Option<f64>,
    #[serde(default)]
    pub source_clock_break_seconds: Option<f64>,
    #[serde(default)]
    pub audio_plan: Option<AudioPlan>,
    #[serde(default)]
    pub audio_track_plans: Option<Vec<AudioPlan>>,
    #[serde(default)]
    pub video_only: bool,
    #[serde(default)]
    pub dolby_vision: bool,
    #[serde(default)]
    pub source_dv_profile: Option<u32>,
    #[serde(default)]
    pub source_dv_bl_signal_compat_id: Option<u32>,
    #[serde(default)]
    pub hdr10_plus: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LiveMode {
    Remux,
    Transcode,
}

/// One live channel run: the host's `LiveFfmpegArgsOptions`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveArgs {
    pub input_url: String,
    pub output_dir: String,
    pub segment_seconds: f64,
    pub window_minutes: f64,
    pub mode: LiveMode,
    pub use_ts: bool,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub referer: Option<String>,
    pub hw_accel: HwAccel,
    #[serde(default)]
    pub video_bitrate_bps: Option<f64>,
    #[serde(default)]
    pub audio_codec: Option<String>,
    #[serde(default)]
    pub append: bool,
    #[serde(default)]
    pub probe_seconds: Option<f64>,
}

/// One trick-play keyframe.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IframeArgs {
    pub input_path: String,
    pub seek_seconds: f64,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub crop: Option<CropInfo>,
    #[serde(default)]
    pub timeline_offset_seconds: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileFormat {
    Mp4,
    Mkv,
}

/// A whole-file conversion: layer 1 of a VOD request written to one file.
#[derive(Debug, Clone)]
pub struct FileArgs {
    pub encode: VodArgs,
    pub path: String,
    pub format: FileFormat,
    /// Every audio track instead of the picked one.
    pub all_audio: bool,
}

/// Output frame and rate of one encode. [ts:backend/src/modules/streaming/transcoding/codec/types.ts:49-59]
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub width: f64,
    pub height: f64,
    pub video_bitrate_bps: f64,
    pub gop_size: f64,
    pub frame_rate: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QsvTuning {
    pub extra: Vec<String>,
    pub rc_init_occupancy: f64,
    pub bufsize: f64,
}

/// `-vf` pieces each encoder splices into its own chain; each is empty or ready to concatenate.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Filters {
    pub crop_str: String,
    pub cpu_crop_prefix: String,
    pub hw_crop_prefix: String,
    pub burn_in_filter: String,
    pub tonemap_vaapi: String,
    pub tonemap_vulkan: String,
    pub tonemap_opencl: String,
    pub tonemap_cuda: String,
    pub tonemap_cpu: String,
}

/// Per-spawn inputs of an encoder descriptor. [ts:backend/src/modules/streaming/transcoding/codec/types.ts:62-137]
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncoderInput {
    pub variant: CodecVariant,
    pub target: Target,
    pub preset: String,
    pub nvenc_preset: String,
    pub early: bool,
    pub force_keyframes_expr: String,
    pub qsv: QsvTuning,
    pub libx264_bufsize_mb: String,
    pub filters: Filters,
    pub tonemap: bool,
    pub tonemap_path: TonemapPath,
    #[serde(default)]
    pub tonemap_curve: Option<TonemapCurve>,
    pub dv_apply_rpu: bool,
    pub has_burn_in: bool,
    pub has_crop: bool,
    pub input_surface: Surface,
    #[serde(default)]
    pub qsv_native: Option<bool>,
    #[serde(default)]
    pub hdr_metadata: Option<HdrMetadata>,
}
