//! Wire types of `POST /v1/decide`: the request, the host capabilities and the `Decision`.
//! `Decision` mirrors the host's `EvaluateResult` field for field (`fixtures/decide`).
use super::{CropInfo, DeviceProfile, HdrFormat, MediaInfo, SourceScan};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum HwAccel {
    Vaapi,
    Nvenc,
    Qsv,
    Amf,
    Videotoolbox,
    #[default]
    None,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    #[default]
    Linux,
    Darwin,
    Win32,
}

/// What the decision reads of the host: detected backend and boot probe results (`core::hw`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct HostCaps {
    pub hw_accel: HwAccel,
    pub platform: Platform,
    /// Not read by `decide` (it picks the AAC encoder binary in the argv builder).
    pub libfdk_aac: bool,
    pub vaapi_writes_hdr_metadata: bool,
    /// Names of the probes that passed; unlisted ones read false (`tonemapOpencl`, `vppQsv`,
    /// `vulkan`, `cuda`, `opencl`, `amfOpencl` and their `Crop`/`Hdr10` variants, plus the
    /// deinterlacers `deinterlaceVaapi`, `deinterlaceQsv`, `bwdifCuda`, `bwdifVideotoolbox`).
    pub probes: Vec<String>,
    /// Encoder descriptor ids the boot probe blacklisted (`hevc_qsv_main10`, `libx265`, ...).
    pub disabled_encoders: Vec<String>,
    /// Decoder descriptor ids the boot probe blacklisted (`hevc_qsv_decode`, ...).
    pub disabled_decoders: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DecideMedia {
    pub media_file_id: u32,
    pub path: String,
    /// With the dot, e.g. `.mkv`.
    pub ext: String,
    /// Host MIME type of the file, echoed for Direct Play.
    pub content_type: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct DecideSelection {
    /// Host URL suffix (`?token=...`), copied verbatim into `playUrl`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_param: Option<String>,
    /// Only tested for truthiness: id 0 means no burn-in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub burn_in_subtitle_id: Option<u32>,
    /// Text (not image) burn-in: only libass forces the pipeline off HW.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub burn_in_is_text: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_quality: Option<String>,
    /// Position in `info.audio`, not a stream index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_stream_index: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DecideRequest {
    pub media: DecideMedia,
    pub info: MediaInfo,
    #[serde(default)]
    pub scan: Option<SourceScan>,
    pub device: DeviceProfile,
    #[serde(default)]
    pub selection: DecideSelection,
}

/// Body of `POST /v1/decide`. `hostCaps` overrides the engine's detected capabilities (debug, shadow runs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DecideBody {
    pub request: DecideRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_caps: Option<HostCaps>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum PlayMethod {
    DirectPlay,
    DirectStream,
    Transcode,
}

/// Segment container of the HLS output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum MuxFlavour {
    Ts,
    Fmp4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum AudioCodec {
    Aac,
    Ac3,
    Eac3,
    Opus,
}

/// Output video format: `hdr` is `null` for SDR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CodecVariant {
    pub codec: VideoCodec,
    pub bit_depth: u32,
    #[schema(required = true)]
    pub hdr: Option<HdrFormat>,
}

/// One audio output's decision: a verbatim copy of the source or the encode target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum AudioPlan {
    // Per variant: utoipa ignores `rename_all_fields`.
    #[serde(rename_all = "camelCase")]
    Copy {
        /// Source codec, lowercased, whatever it is.
        codec: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channels: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bitrate_bps: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Transcode { codec: AudioCodec, channels: u32 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct TranscodeReason {
    pub flag: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AudioTrackPlan {
    /// Position in `info.audio`.
    pub index: u32,
    pub language: String,
    /// Source codec, lowercased.
    pub codec: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<u32>,
    pub copy: bool,
    pub output_codec: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_channels: Option<u32>,
    pub bitrate_bps: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    /// Why the track is re-encoded (`Audio*` flags); empty when copied.
    pub reason_flags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct QualityOption {
    pub id: String,
    pub label: String,
    pub height: u32,
    pub width: u32,
    pub total_bitrate_bps: u32,
    pub is_remux: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low_bandwidth: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BitrateHint {
    pub video_bitrate_bps: u32,
    pub audio_bitrate_bps: u32,
    pub total_bitrate_bps: u32,
}

/// The facts of the source the decision was made on.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackSource {
    pub container: String,
    pub video_codec: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_codec_tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_level: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_bit_rate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format_bit_rate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_bit_depth: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<String>,
    pub audio_codec: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_channels: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_channel_layout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_bit_rate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_sample_rate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hdr_format: Option<HdrFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_space: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_transfer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_primaries: Option<String>,
    /// Only when auto-crop is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<CropInfo>,
}

/// The public playback-info answer (`PlaybackInfoResponse`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackInfo {
    pub media_file_id: u32,
    pub play_method: PlayMethod,
    pub play_url: String,
    pub content_type: String,
    /// Empty on Direct Play.
    pub transcode_reasons: Vec<TranscodeReason>,
    pub video_copy_stream: bool,
    pub audio_copy_stream: bool,
    pub output_video_codec: String,
    pub output_audio_codec: String,
    pub audio_plan: AudioPlan,
    /// `hls` or the source container on Direct Play.
    pub output_container: String,
    /// `original`, or the pinned rung id, or `auto`.
    pub quality: String,
    pub hw_accel: HwAccel,
    pub tonemapping: bool,
    /// HDR copied to an SDR client that tone-maps it itself.
    pub client_tonemap: bool,
    pub dolby_vision: bool,
    /// BANDWIDTH of the remux variant (DirectStream only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remux_master_bandwidth_bps: Option<u32>,
    /// By rung id; absent on Direct Play.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode_bitrate_by_quality: Option<BTreeMap<String, BitrateHint>>,
    pub qualities: Vec<QualityOption>,
    pub audio_tracks: Vec<AudioTrackPlan>,
    pub source: PlaybackSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    pub response: PlaybackInfo,
    pub use_hdr_ladder: bool,
    pub video_variant: CodecVariant,
    pub mux_flavour: MuxFlavour,
    /// Per-rendition audio output, in `info.audio` order.
    pub audio_plans: Vec<AudioPlan>,
    /// Whether a transcoded rung runs a tone-map, whatever the play method.
    pub transcode_tonemapping: bool,
}

/// Body of `POST /v1/estimate-sizes`. `fileSize` is the source size in bytes, 0 when unknown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct EstimateSizesRequest {
    pub info: MediaInfo,
    #[serde(default)]
    pub file_size: f64,
}

/// One download quality; `estimatedSize` is in bytes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct QualityEstimate {
    pub key: String,
    pub label: String,
    pub estimated_size: f64,
}
