//! `POST /v1/probe` wire types.
use super::MediaInfo;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MediaRef {
    pub media_file_id: u32,
    /// Absolute path, already authorised by the host.
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProbeRequest {
    pub media: MediaRef,
    /// Run `cropdetect` and set `video[0].crop` when bars are significant.
    #[serde(default)]
    pub crop: bool,
    /// Read BCP-47 language tags of a Matroska file with `mkvmerge -J`.
    #[serde(default)]
    pub mkv_tags: bool,
    /// Compute the OpenSubtitles moviehash.
    #[serde(default)]
    pub moviehash: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MovieHash {
    /// 16 lowercase hex digits.
    pub hash: String,
    /// File size in bytes.
    pub bytesize: f64,
}

/// `ok: false` means ffprobe could not tell (unreadable file, timeout): leave stored rows alone.
/// `ok: true` with `info.error` is a readable file without streams.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProbeResult {
    pub ok: bool,
    /// Present when ok; on failure the empty error blob the host persists today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<MediaInfo>,
    /// Absent when the file is under 128 KiB or unreadable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moviehash: Option<MovieHash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
