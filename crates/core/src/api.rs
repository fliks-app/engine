//! Wire types: one serde struct is the JSON format, the persisted blob and the OpenAPI schema.
//! Readers ignore unknown fields; no 64-bit integers in JSON.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

mod decide;
mod fingerprint;
mod intro;
mod jobs;
mod lifetime;
mod live;
mod probe;
mod runtime;
pub use decide::*;
pub use fingerprint::*;
pub use intro::*;
pub use jobs::*;
pub use lifetime::*;
pub use live::*;
pub use probe::*;
pub use runtime::*;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MEDIA_INFO_SCHEMA_VERSION: u32 = 1;
pub const SOURCE_SCAN_SCHEMA_VERSION: u32 = 1;

/// Distinguishes an absent key (None) from an explicit `null` (Some(None)).
mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<f64>>, D::Error> {
        Option::<f64>::deserialize(d).map(Some)
    }

    pub fn serialize<S: Serializer>(v: &Option<Option<f64>>, s: S) -> Result<S::Ok, S::Error> {
        v.serialize(s)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub engine_version: String,
    pub protocol_version: u32,
    pub features: Vec<String>,
    pub instance_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Gpu {
    pub render_node: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// `vaapi` | `nvenc` | `qsv` | `amf` | `videotoolbox` | `none`
    pub hw_accel: String,
    pub encoders: Vec<String>,
    pub decoders: Vec<String>,
    pub tonemap_algos: Vec<String>,
    pub gpus: Vec<Gpu>,
    pub default_gpu: Option<String>,
    pub platform: String,
    /// `signal` | `stdin` | `none`
    pub pause_capability: String,
}

/// `GET /v1/capabilities`: the summary plus the exact host state the decision reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CapabilitiesResponse {
    #[serde(flatten)]
    pub capabilities: Capabilities,
    pub host_caps: HostCaps,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum QsvPreset {
    Veryfast,
    Faster,
    Fast,
    Medium,
    Slow,
    Slower,
    Veryslow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TonemapAlgo {
    Auto,
    Opencl,
    Vaapi,
    Qsv,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TonemapCurve {
    Hable,
    Mobius,
    Reinhard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum AutoQualityMode {
    Directplay,
    Abr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SubtitlePrewarm {
    Off,
    Playback,
    Import,
}

/// The `streaming_*` settings, already resolved by the host. Defaults match the host's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct StreamingConfig {
    pub segment_duration: f64,
    pub qsv_preset: QsvPreset,
    pub tonemap_algo: TonemapAlgo,
    pub tonemap_curve: TonemapCurve,
    /// Absent: the engine default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_max_bytes: Option<f64>,
    /// Absent: the engine default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_ttl_ms: Option<f64>,
    /// Absent: derived from cgroup/CPU budget.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ffmpeg_slots: Option<u32>,
    /// A job that writes files fails below this much free disk under the cache root; absent: 1 GiB, 0
    /// disables.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_min_free_bytes: Option<f64>,
    pub auto_quality_mode: AutoQualityMode,
    pub auto_crop_enabled: bool,
    /// `auto` or a `/dev/dri/renderD*` path.
    pub gpu_render_node: String,
    pub subtitle_prewarm: SubtitlePrewarm,
    pub throttle_enabled: bool,
    pub throttle_threshold_seconds: f64,
}

impl Default for StreamingConfig {
    fn default() -> Self {
        Self {
            segment_duration: 3.0,
            qsv_preset: QsvPreset::Faster,
            tonemap_algo: TonemapAlgo::Auto,
            tonemap_curve: TonemapCurve::Mobius,
            cache_max_bytes: None,
            cache_ttl_ms: None,
            ffmpeg_slots: None,
            job_min_free_bytes: None,
            auto_quality_mode: AutoQualityMode::Directplay,
            auto_crop_enabled: true,
            gpu_render_node: "auto".into(),
            subtitle_prewarm: SubtitlePrewarm::Playback,
            throttle_enabled: true,
            throttle_threshold_seconds: 90.0,
        }
    }
}

/// One prefix of a host media path rewritten to the path this engine opens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PathMapping {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct EngineConfig {
    pub revision: u32,
    /// Resolver callback base URL of the host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_token: Option<String>,
    pub streaming: StreamingConfig,
    #[serde(default)]
    pub lifetime: LifetimeConfig,
    #[serde(default)]
    pub livetv: LiveTvConfig,
    /// Absent: the host path is the path this engine opens.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path_mappings: Vec<PathMapping>,
}

/// Longest `from` that ends on a path boundary. `/mnt/a` does not rewrite `/mnt/ab`.
pub fn apply_path_mappings(mappings: &[PathMapping], path: &str) -> String {
    let best = mappings
        .iter()
        .filter_map(|m| {
            let from = m.from.trim_end_matches(['/', '\\']);
            let to = m.to.trim_end_matches(['/', '\\']);
            if from.is_empty() || to.is_empty() {
                return None;
            }
            let rest = path.strip_prefix(from)?;
            if !(rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\')) {
                return None;
            }
            Some((from.len(), to, rest))
        })
        .max_by_key(|(n, _, _)| *n);
    match best {
        Some((_, to, rest)) => format!("{to}{rest}"),
        None => path.to_string(),
    }
}

// MediaInfo mirrors `MediaFileInfo` (ffprobe.service.ts). JSON integers stay u32/i32 so the
// persisted jsonb round-trips; seconds are f64.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum HdrFormat {
    #[serde(rename = "HDR10")]
    Hdr10,
    #[serde(rename = "HLG")]
    Hlg,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct CropInfo {
    pub width: u32,
    pub height: u32,
    pub x: u32,
    pub y: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HdrMetadata {
    pub mastering_display: String,
    pub max_cll: u32,
    pub max_fall: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoStreamInfo {
    pub stream_index: u32,
    pub codec: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_aspect_ratio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pixel_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reorder_frames: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_frame_rate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_frame_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_rate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_space: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_transfer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_primaries: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_range: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hdr_format: Option<HdrFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dv_profile: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dv_bl_signal_compat_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dv_level: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dv_rpu_present: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dv_bl_present: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dv_el_present: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hdr_metadata: Option<HdrMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hdr10_plus: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<CropInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AudioStreamInfo {
    pub stream_index: u32,
    pub codec: String,
    pub language: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language_tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_layout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_rate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_default: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commentary: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_description: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleStreamInfo {
    pub stream_index: u32,
    pub codec: String,
    pub language: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language_tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub forced: bool,
    pub hearing_impaired: bool,
    /// Bitmap subtitles (PGS, VOBSUB, DVB) need burn-in.
    pub is_image_based: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Chapter {
    pub start_seconds: f64,
    pub end_seconds: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MediaInfo {
    pub video: Vec<VideoStreamInfo>,
    pub audio: Vec<AudioStreamInfo>,
    pub subtitles: Vec<SubtitleStreamInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format_bit_rate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format_start_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format_name: Option<String>,
    /// Absent until the background scan ran; `null` when there is no break.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    #[schema(value_type = Option<f64>, nullable)]
    pub timestamp_break_seconds: Option<Option<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chapters: Option<Vec<Chapter>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Bumped when probing changes what it records; absent on blobs persisted before the engine existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectPlayProfile {
    pub containers: Vec<String>,
    pub video_codecs: Vec<String>,
    pub audio_codecs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CodecCondition {
    pub codec: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_level: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profiles: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bit_depth: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_height: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum DeviceType {
    Mobile,
    Desktop,
}

/// Mirrors `DeviceProfileDto`; bitrates and counts are f64 (JS numbers).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeviceProfile {
    pub direct_play_profiles: Vec<DirectPlayProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec_conditions: Option<Vec<CodecCondition>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_streaming_bitrate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_audio_channels: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_channels_by_codec: Option<BTreeMap<String, f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_hdr: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tonemaps_hdr_locally: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crops_black_bars_locally: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_dolby_vision: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dolby_vision_profiles: Option<Vec<i32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_direct_play: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switches_direct_play_audio: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_hls_subtitles: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupes_audio_by_language: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_muxed_audio_rendition: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_i_frame_trick_play: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_image_subtitles: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probes_seg_zero: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_type: Option<DeviceType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_ts: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_ts_on_single_audio: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_abr: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reject_copy: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_hls_subtitles: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Keyframe {
    pub pts: f64,
    pub dts: f64,
}

/// Mirrors `SourceScan` (source-scan.ts); `audioConfigChanges` is keyed by audio stream index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceScan {
    pub keyframes: Vec<Keyframe>,
    pub end: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub break_seconds: Option<f64>,
    #[serde(deserialize_with = "u32_keys")]
    pub audio_config_changes: BTreeMap<u32, bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u32>,
}

/// Map keys read as text: an internally tagged enum (`JobResult`) buffers them as strings.
fn u32_keys<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BTreeMap<u32, bool>, D::Error> {
    BTreeMap::<String, bool>::deserialize(d)?
        .into_iter()
        .map(|(k, v)| k.parse().map(|k| (k, v)).map_err(serde::de::Error::custom))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn roundtrip<T: Serialize + for<'a> Deserialize<'a>>(v: &Value) -> Value {
        serde_json::to_value(serde_json::from_value::<T>(v.clone()).unwrap()).unwrap()
    }

    #[test]
    fn media_info_roundtrips() {
        let v = json!({
            "video": [{
                "streamIndex": 0, "codec": "hevc", "profile": "Main 10", "level": 153,
                "width": 3840, "height": 2160, "displayAspectRatio": "16:9",
                "pixelFormat": "yuv420p10le", "frameRate": "24000/1001", "reorderFrames": 2,
                "avgFrameRate": "24000/1001", "startTimeSeconds": 0.0416, "firstFrameSeconds": 0.0416,
                "endSeconds": 7265.5, "bitRate": 42000000, "bitDepth": 10,
                "colorSpace": "bt2020nc", "colorTransfer": "smpte2084", "colorPrimaries": "bt2020",
                "colorRange": "tv", "hdrFormat": "HDR10",
                "hdrMetadata": { "masteringDisplay": "G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1)", "maxCll": 1000, "maxFall": 400 },
                "hdr10Plus": false,
                "crop": { "width": 3840, "height": 1608, "x": 0, "y": 276 }
            }],
            "audio": [
                { "streamIndex": 1, "codec": "eac3", "language": "eng", "languageTag": "en", "title": "English 5.1",
                  "channels": 6, "channelLayout": "5.1(side)", "sampleRate": 48000, "bitRate": 640000,
                  "profile": "Dolby Digital Plus + Dolby Atmos", "isDefault": true, "startTimeSeconds": 0.0416 },
                { "streamIndex": 2, "codec": "aac", "language": "fra", "channels": 2, "commentary": true, "audioDescription": false }
            ],
            "subtitles": [
                { "streamIndex": 3, "codec": "subrip", "language": "eng", "forced": false, "hearingImpaired": true, "isImageBased": false },
                { "streamIndex": 4, "codec": "hdmv_pgs_subtitle", "language": "fra", "title": "Forced", "forced": true, "hearingImpaired": false, "isImageBased": true }
            ],
            "formatBitRate": 45000000, "formatStartSeconds": 0.0, "formatName": "matroska,webm",
            "timestampBreakSeconds": null, "durationSeconds": 7265.5,
            "chapters": [
                { "startSeconds": 0.0, "endSeconds": 600.5, "title": "Opening" },
                { "startSeconds": 600.5, "endSeconds": 7265.5 }
            ],
            "schemaVersion": 1
        });
        assert_eq!(roundtrip::<MediaInfo>(&v), v);
    }

    #[test]
    fn media_info_absent_stays_absent_and_unknown_ignored() {
        let v = json!({ "video": [], "audio": [], "subtitles": [], "future": 1 });
        let out = roundtrip::<MediaInfo>(&v);
        assert_eq!(out, json!({ "video": [], "audio": [], "subtitles": [] }));
    }

    #[test]
    fn device_profile_and_scan_roundtrip() {
        let d = json!({
            "directPlayProfiles": [{ "containers": ["mp4"], "videoCodecs": ["h264"], "audioCodecs": ["aac"] }],
            "codecConditions": [{ "codec": "h264", "maxLevel": 41.0, "profiles": ["high"] }],
            "audioChannelsByCodec": { "aac": 8.0 }, "dolbyVisionProfiles": [5, 8],
            "deviceType": "mobile", "supportsAbr": false
        });
        assert_eq!(roundtrip::<DeviceProfile>(&d), d);
        let s = json!({
            "keyframes": [{ "pts": 0.0, "dts": -0.04 }], "end": 12.5,
            "audioConfigChanges": { "1": false, "2": true }, "schemaVersion": 1
        });
        assert_eq!(roundtrip::<SourceScan>(&s), s);
    }

    #[test]
    fn path_mappings_use_the_longest_boundary() {
        let maps = [
            PathMapping {
                from: "/mnt/nas".into(),
                to: "/data".into(),
            },
            PathMapping {
                from: "/mnt/nas/films/".into(),
                to: "/films".into(),
            },
        ];
        assert_eq!(
            apply_path_mappings(&maps, "/mnt/nas/films/a.mkv"),
            "/films/a.mkv"
        );
        assert_eq!(
            apply_path_mappings(&maps, "/mnt/nas/tv/a.mkv"),
            "/data/tv/a.mkv"
        );
        assert_eq!(
            apply_path_mappings(&maps, "/mnt/nas2/a.mkv"),
            "/mnt/nas2/a.mkv"
        );
        assert_eq!(apply_path_mappings(&maps, "/mnt/nas"), "/data");
        assert_eq!(apply_path_mappings(&[], "/mnt/nas/a.mkv"), "/mnt/nas/a.mkv");
    }
}
