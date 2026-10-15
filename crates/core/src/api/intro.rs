//! Wire types for `POST /v1/intro-detect`.
use super::{Chapter, Fingerprint};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum IntroDetectKind {
    Intro,
    Outro,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct IntroDetectRequest {
    pub kind: IntroDetectKind,
    pub lookup_seconds: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_segment_seconds: Option<f64>,
    pub episodes: Vec<IntroEpisode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct IntroEpisode {
    pub media_file_id: u32,
    pub duration_seconds: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chapters: Option<Vec<Chapter>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intro_fingerprint: Option<Fingerprint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outro_fingerprint: Option<Fingerprint>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct IntroDetectResponse {
    pub results: Vec<IntroSegment>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct IntroSegment {
    pub media_file_id: u32,
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub confidence: f64,
    pub source: IntroSegmentSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum IntroSegmentSource {
    Chapter,
    Reference,
    Pairwise,
}
