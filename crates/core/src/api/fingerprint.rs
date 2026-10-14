//! Result of the intro fingerprint job (`jobs::fingerprint`).
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Chromaprint hashes of one audio window; mirrors the host's in-memory `Fingerprint`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Fingerprint {
    /// Raw int32 hashes, about 8 per second.
    pub hashes: Vec<i32>,
    /// Seconds fpcalc reports (the file's length for a file read, 0 on stdin), else the length asked for.
    pub duration_seconds: f64,
    /// File offset the window starts at (0 for the head of the file).
    #[serde(default)]
    pub base_offset_sec: f64,
}
