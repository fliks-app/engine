//! Karaoke: the model manager (`/v1/models`) and the results of the `Separate`, `Align` and
//! `Melody` jobs. Spec: `docs/spec/karaoke.md`.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// What a model does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ModelKind {
    /// Vocals and accompaniment stems (`Separate`).
    Separation,
    /// A CTC network or speech recognition that places known lyrics on the audio (`Align`).
    Alignment,
    /// Fundamental frequency of a voice (`Melody`).
    Melody,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ModelState {
    Absent,
    Downloading,
    /// Every file is on disk with its verified size and hash.
    Installed,
    /// The last download or verification failed (`error`); a new download starts from scratch.
    Failed,
}

/// How an alignment model times the lyrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ModelEngine {
    /// A CTC network run on the ONNX runtime.
    Ctc,
    /// The speech recogniser.
    Whisper,
}

/// The licence a model's weights are distributed under, to show before a download.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelLicence {
    /// SPDX identifier or a short name.
    pub name: String,
    /// The licence text, or what the engine knows about the weights' terms.
    pub text: String,
    /// False when the weights' terms are not settled: the host must show `text` and ask first.
    pub clear: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelFile {
    /// File name inside the model's directory.
    pub name: String,
    pub url: String,
    /// Lowercase hex SHA-256 of the file, checked before it is kept.
    pub sha256: String,
    /// f64: no 64-bit integers on the wire.
    pub size_bytes: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    /// Stable identifier, the `model` of a job request.
    pub id: String,
    pub kind: ModelKind,
    pub name: String,
    /// Changes when the weights do; the host stamps it on what a job produced.
    pub version: String,
    pub licence: ModelLicence,
    /// What the job picks when its request names no model of this kind.
    pub default: bool,
    pub files: Vec<ModelFile>,
    /// Two-letter codes of the languages an alignment model is for; empty: every language, or
    /// not an alignment model.
    pub languages: Vec<String>,
    /// Alignment models only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<ModelEngine>,
    /// The model's page (its card or repository).
    pub homepage: String,
    /// Total of `files[].sizeBytes`.
    pub size_bytes: f64,
    /// What the model's directory takes on disk (0 when absent).
    pub disk_bytes: f64,
    /// `version` of the install on disk, once a download recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    /// Installed with a version older than the manifest's: it still runs until an update
    /// (`POST /v1/models/{id}/download`) replaces it.
    pub outdated: bool,
    pub state: ModelState,
    /// The `downloading` state is a verification re-hashing the files, which `DELETE` refuses to
    /// cancel (it would remove them).
    pub verifying: bool,
    /// Bytes fetched (or verified) so far while `downloading`, else the installed size.
    pub downloaded_bytes: f64,
    /// Transfer rate of the last second while `downloading`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_per_second: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A directory of the models directory that no manifest entry names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelOrphan {
    /// Directory name (`DELETE /v1/models/orphans/{name}` removes it).
    pub name: String,
    pub size_bytes: f64,
}

/// Where the engine stands with the Windows ML runtime installed on the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum WindowsMl {
    /// The engine runs the system runtime.
    Active,
    /// Installed since the engine started: a restart makes it run it.
    Restart,
    /// Not installed.
    Missing,
}

/// What this engine build can run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct KaraokeRuntime {
    /// An ONNX runtime is built in and loaded (`Separate` and `Melody` need it).
    pub onnx: bool,
    /// Why `onnx` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onnx_error: Option<String>,
    /// Execution providers that can run a model, `cpu` last: `cuda`, `coreml`, `directml`...
    pub providers: Vec<String>,
    /// The speech recogniser `Align` runs is installed.
    pub whisper: bool,
    /// An ffmpeg with the `rubberband` filter is installed (pitch shift).
    pub rubberband: bool,
    /// Windows 11 22H2 and later only: the Windows ML runtime of the machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows_ml: Option<WindowsMl>,
}

/// `GET /v1/models`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelsResponse {
    pub runtime: KaraokeRuntime,
    /// Where the models live (`<FLIKS_DATA_DIR>/models`, else `<cache root>/models`).
    pub models_dir: String,
    /// Free space of the filesystem holding the models directory; absent when it can't say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_bytes: Option<f64>,
    /// What the models directory takes on disk, caches included.
    pub used_bytes: f64,
    /// Directories no manifest entry names, hidden ones (caches) excluded.
    pub orphans: Vec<ModelOrphan>,
    pub models: Vec<ModelInfo>,
}

/// One written audio file of a `Separate`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct StemFile {
    /// The request's path.
    pub path: String,
    /// f64: no 64-bit integers on the wire.
    pub bytes: f64,
}

/// What a `Separate` wrote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SeparateMeta {
    /// The model that ran and its version.
    pub model: String,
    pub model_version: String,
    /// The execution provider that ran the networks, or `cpu`.
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vocals: Option<StemFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instrumental: Option<StemFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guide: Option<StemFile>,
    /// Length of the stems.
    pub duration_seconds: f64,
}

/// One lyric line to time. A line with no times is placed by the audio alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AlignLine {
    pub text: String,
    /// Where the line is known to start and end, in milliseconds of the stem.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ms: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AlignedWord {
    /// The word as written in the line (whitespace-separated).
    pub text: String,
    /// UTF-8 byte range of the word inside the request line's `text`.
    pub byte_start: u32,
    pub byte_end: u32,
    pub start_ms: u32,
    pub end_ms: u32,
}

/// A line placed on the audio. A line marked `x2` (or `(x2)`, `[x2]`, `2x`) comes back once per
/// repeat, in order, each with its own times and its `repeat` ordinal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AlignedLine {
    /// Index of the request line.
    pub line: u32,
    pub repeat: u32,
    pub start_ms: u32,
    pub end_ms: u32,
    pub words: Vec<AlignedWord>,
}

/// What an `Align` found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AlignMeta {
    pub model: String,
    pub model_version: String,
    /// What ran the model: `ctc-<provider>` (`ctc-cpu`, `ctc-cuda`...), or for the speech
    /// recogniser `vulkan`, `metal` or `cpu`.
    pub backend: String,
    pub lines: Vec<AlignedLine>,
    /// Share of the lyric words found, 0 to 1; a low value means the timing is mostly
    /// interpolated.
    pub matched: f64,
    /// Why the speech recogniser placed the words instead of the CTC model: the CTC failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

/// A span of the vocal stem whose notes a `Melody` looks for (a word cue).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MelodySpan {
    pub start_ms: u32,
    pub end_ms: u32,
}

/// One sustained pitch inside a span.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MelodyNote {
    pub start_ms: u32,
    pub end_ms: u32,
    /// MIDI pitch, fractional: 69 is A4 at 440 Hz.
    pub midi: f64,
}

/// What a `Melody` found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MelodyMeta {
    pub model: String,
    pub model_version: String,
    /// The notes of each request span, in order; an unvoiced span has none.
    pub spans: Vec<Vec<MelodyNote>>,
}

/// `error` of a job failed for want of a model or a runtime, as `<code>: <message>`.
pub mod job_errors {
    /// The model the job needs is not installed (or the request named an unknown one).
    pub const MODEL_MISSING: &str = "model_missing";
    /// This engine build or host cannot run the job (no ONNX runtime, no speech recogniser).
    pub const UNSUPPORTED: &str = "unsupported";

    pub fn message(code: &str, text: impl std::fmt::Display) -> String {
        format!("{code}: {text}")
    }
}
