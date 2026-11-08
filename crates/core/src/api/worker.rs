//! Worker protocol: a worker (local or remote engine) calls the host under `/engine/workers/*`
//! and `/engine/jobs/*`, like `/engine/resolve`. Routes and fencing rules: `PROTOCOL.md`.
use super::{CropInfo, JobKind, MediaInfo};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Chunked upload: byte offset of the chunk, which must equal the length received so far.
pub const HEADER_UPLOAD_OFFSET: &str = "Upload-Offset";
/// `?1` on the last chunk of an output upload.
pub const HEADER_UPLOAD_COMPLETE: &str = "Upload-Complete";
/// Lease `attempt` the upload belongs to (fencing).
pub const HEADER_ATTEMPT: &str = "X-Fliks-Attempt";
/// Instance identifier of the uploading worker (fencing).
pub const HEADER_WORKER_ID: &str = "X-Fliks-Worker-Id";
/// Size of an output chunk.
pub const UPLOAD_CHUNK_BYTES: u32 = 8 * 1024 * 1024;

/// How a device holds a job that must yield to playback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PauseMode {
    /// The encoder process is stopped and resumed.
    Suspend,
    /// The encoder is killed and the job starts over.
    Restart,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkerDevice {
    /// Stable within an instance (`cpu`, a render node...).
    pub id: String,
    /// What a person calls it (the GPU's label); absent for the CPU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub encoders: Vec<String>,
    pub decoders: Vec<String>,
    pub max_jobs: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vram_mib: Option<u32>,
    pub pause: PauseMode,
}

/// A lease the worker holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LeaseRef {
    pub id: u32,
    pub attempt: u32,
}

/// `POST /engine/workers/hello`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkerHello {
    /// Generated at worker startup; the `workerId` of every later request.
    pub instance_id: String,
    pub engine_version: String,
    pub devices: Vec<WorkerDevice>,
    /// Leases still held; the host puts every other lease of this instance back in the queue.
    #[serde(default)]
    pub jobs: Vec<LeaseRef>,
}

/// Settings the host imposes; path access and mappings are the admin's, never the worker's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkerSettings {
    pub lease_s: u32,
    pub heartbeat_s: u32,
    pub long_poll_s: u32,
    pub upload_chunk_bytes: u32,
    /// CPU weight of a software optimization against the playback budget; absent: 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_weight: Option<u32>,
    /// Hardware playback reads on a device that pause its background GPU jobs; absent: 1, 0 never.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_playback_before_pause: Option<u32>,
    /// Per-device override of `gpuPlaybackBeforePause`, by device id.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub device_gpu_playback_before_pause: std::collections::BTreeMap<String, u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkerHelloResponse {
    pub settings: WorkerSettings,
    /// Leases from `jobs` to drop now: the worker kills their process and deletes their files.
    #[serde(default)]
    pub abandon: Vec<u32>,
}

/// `POST /engine/jobs/lease`: long-polls for a job that fits the device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LeaseRequest {
    pub worker_id: String,
    pub device_id: String,
}

/// The answer to a lease; 204 with no body when nothing was queued within the poll.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LeasedJob {
    pub id: u32,
    /// Fencing counter, +1 on each lease; every later call of the job carries it.
    pub attempt: u32,
    pub lease_s: u32,
    /// `Optimize` or `Verify`, with its payload.
    pub kind: JobKind,
    /// Local read: the source and the staging file to write, both on the worker's filesystem.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged_path: Option<String>,
    /// Remote read: the main engine's data plane (`purpose: download`), with a token bound to the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_token: Option<String>,
}

/// How much of the file a job rewrites.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum OptimizeLevel {
    /// Tags and dispositions rewritten in place, no stream copied.
    Metadata,
    /// Streams copied into a new container, some dropped or retagged.
    Remux,
    /// The video re-encoded.
    Reencode,
}

/// Playable audio a track is converted to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum StreamEncode {
    /// E-AC-3, 640 kb/s up to 5.1 (7.1 folds to 5.1), 256 kb/s stereo.
    Eac3,
    /// Stereo AAC 256 kb/s; multichannel sources are downmixed with a boosted centre.
    Aac,
}

/// What happens to one stream of the source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct StreamPlan {
    pub stream_index: u32,
    /// A stream absent from `OptimizePlan.streams` is kept as it is.
    pub keep: bool,
    /// Language tag to write (ISO 639-2), when it differs from the source's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Title to write; `Some("")` clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Sets or clears the default flag; absent keeps the source's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<bool>,
    /// Audio only: the kept track is replaced by this encode (or, with `keepOriginal`, joined by it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encode: Option<StreamEncode>,
    /// With `encode`: the original stays beside the new track (`title` and `default` go to the new one).
    #[serde(default)]
    pub keep_original: bool,
}

/// Quality target of a `reencode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum OptimizeQuality {
    Small,
    Balanced,
    Faithful,
}

/// What a software encode estimated beyond `maxSoftwareHours` does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum OptimizeTooSlow {
    /// Switch to the hardware encoder when there is one (copy otherwise).
    #[default]
    Hardware,
    /// Copy the video (`too_slow`).
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum OptimizeContainer {
    Mkv,
    Mp4,
}

/// A text subtitle file (the OCR'd SRT of an image track) added to the output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExtraSubtitle {
    /// Absolute path of the SRT on the worker.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forced: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hearing_impaired: Option<bool>,
}

/// The resolved plan of an optimisation: decided by the host, executed as written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OptimizePlan {
    pub level: OptimizeLevel,
    /// Target video codec of a `reencode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// Letterbox bars to cut, `reencode` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<CropInfo>,
    #[serde(default)]
    pub streams: Vec<StreamPlan>,
    /// Write `hvc1` instead of `hev1` on HEVC in MP4.
    #[serde(default)]
    pub hvc1_tag: bool,
    /// Ignored: the statistics tags of every Matroska output are regenerated (`mkvpropedit`).
    #[serde(default)]
    pub strip_statistics_tags: bool,
    /// Quality of a `reencode`; absent: balanced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<OptimizeQuality>,
    /// Software encoder instead of the hardware one.
    #[serde(default)]
    pub software: bool,
    /// Longest estimated software encode, hours; absent: 12. Beyond it the job is `too_slow`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_software_hours: Option<f64>,
    /// What a software encode beyond `maxSoftwareHours` does; absent: `hardware`.
    #[serde(default)]
    pub too_slow: OptimizeTooSlow,
    /// The 3 sample encodes must save at least this share of the video; else the video is copied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_savings_pct: Option<f64>,
    /// Keep Dolby Vision 7/8 and HDR10+ video as it is (copy); absent: true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_hdr10plus_dv: Option<bool>,
    /// Output container; absent: the source's (MP4 and MOV stay MP4, the rest is MKV).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<OptimizeContainer>,
    /// Empty the global title.
    #[serde(default)]
    pub clear_title: bool,
    /// Text subtitles added after the kept tracks.
    #[serde(default)]
    pub extra_subtitles: Vec<ExtraSubtitle>,
    /// Server-side only: the commit renames the file by its words; the engine ignores it.
    #[serde(default)]
    pub rename: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OptimizePayload {
    /// Probe of the source at queue time.
    pub info: MediaInfo,
    pub plan: OptimizePlan,
    /// Where the output is written; absent: `.fliks-opt-<job>-1.tmp` beside the source. The worker
    /// loop sets it from the lease's `stagedPath`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged_path: Option<String>,
    /// Remote source read over HTTP, from the lease's `inputUrl`/`inputToken`; the token goes in
    /// an `Authorization: Bearer` header. Absent: the local file of the job's `media.path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct VerifyPayload {
    pub info: MediaInfo,
}

/// `POST /engine/jobs/{id}/heartbeat`, every `heartbeatS`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobHeartbeatRequest {
    pub worker_id: String,
    pub attempt: u32,
    /// 0 to 1.
    pub progress: f64,
    pub paused: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobHeartbeatResponse {
    /// Epoch ms.
    pub lease_until: f64,
    /// The job was cancelled: stop and delete the files.
    pub cancel: bool,
}

/// `PUT /engine/jobs/{id}/output` answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadAck {
    /// Bytes held by the host: the next `Upload-Offset`.
    pub received_bytes: f64,
}

/// Why a job did not complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum JobErrorClass {
    /// A path is unreadable or unwritable for the worker.
    Access,
    /// A read or write failed mid-job.
    Io,
    /// The source cannot be decoded; retrying will not help.
    Corrupt,
    /// The source decodes with isolated errors below the per-hour threshold: readable, never optimized.
    Damaged,
    /// No room for the staging file.
    Nospace,
    /// The encoder crashed or misbehaved.
    Engine,
    /// A transfer with the host failed.
    Network,
    /// The output does not conform to the source even after the CPU fallback.
    Verify,
    /// The device or file is in use; the job goes back without counting as a failure.
    Busy,
    /// The source no longer matches the queue-time snapshot.
    Stale,
}

impl JobErrorClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Access => "access",
            Self::Io => "io",
            Self::Corrupt => "corrupt",
            Self::Damaged => "damaged",
            Self::Nospace => "nospace",
            Self::Engine => "engine",
            Self::Network => "network",
            Self::Verify => "verify",
            Self::Busy => "busy",
            Self::Stale => "stale",
        }
    }

    /// An executor's failure text: `<class>: <message>`, which `from_message` reads back.
    pub fn message(self, text: impl std::fmt::Display) -> String {
        format!("{}: {text}", self.as_str())
    }

    /// The class and text of a failure made by `message`; any other text is an `engine` failure.
    pub fn from_message(s: &str) -> (Self, &str) {
        const ALL: [JobErrorClass; 10] = [
            JobErrorClass::Access,
            JobErrorClass::Io,
            JobErrorClass::Corrupt,
            JobErrorClass::Damaged,
            JobErrorClass::Nospace,
            JobErrorClass::Engine,
            JobErrorClass::Network,
            JobErrorClass::Verify,
            JobErrorClass::Busy,
            JobErrorClass::Stale,
        ];
        ALL.iter()
            .find_map(|c| {
                s.strip_prefix(c.as_str())
                    .and_then(|r| r.strip_prefix(": "))
                    .map(|r| (*c, r))
            })
            .unwrap_or((Self::Engine, s))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobError {
    pub class: JobErrorClass,
    pub message: String,
    /// End of the encoder's stderr; the host keeps 64 KiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
}

/// `POST /engine/jobs/{id}/result`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobResultRequest {
    pub worker_id: String,
    pub attempt: u32,
    pub ok: bool,
    /// Bytes of the staged output; 0 for a `Verify`.
    #[serde(default)]
    pub out_size: f64,
    /// Lowercase hex SHA-256 of the staged output, computed by the worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_sha256: Option<String>,
    /// Probe of the output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_info: Option<MediaInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JobError>,
    /// `Optimize` only: what was really done (a `reencode` whose video was copied reports `remux`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<OptimizeLevel>,
    /// `Optimize` only: why a planned re-encode copied the video.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_reason: Option<String>,
    /// `Optimize` only: the output was produced on the CPU after the hardware encoder failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_fallback: Option<bool>,
    /// `Optimize` only: the video copy left nothing to change; no output was staged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub nothing_to_do: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_rename_flag_defaults_to_false_and_is_read() {
        let p = |v: serde_json::Value| serde_json::from_value::<OptimizePlan>(v).unwrap();
        assert!(!p(json!({ "level": "remux" })).rename);
        assert!(p(json!({ "level": "remux", "rename": true })).rename);
    }

    #[test]
    fn a_leased_optimize_job_round_trips() {
        let wire = json!({
            "id": 7, "attempt": 2, "leaseS": 60,
            "kind": { "type": "Optimize", "payload": {
                "info": { "video": [], "audio": [], "subtitles": [] },
                "plan": { "level": "remux", "streams": [
                    { "streamIndex": 2, "keep": false },
                    { "streamIndex": 1, "keep": true, "language": "fre" }] } } },
            "inputPath": "/m/a.mkv", "stagedPath": "/m/.a.fliks.mkv"
        });
        let job: LeasedJob = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(job.kind.name(), "Optimize");
        assert_eq!(job.input_url, None);
        let JobKind::Optimize { payload } = &job.kind else {
            panic!("not an optimize job");
        };
        assert_eq!(payload.plan.level, OptimizeLevel::Remux);
        assert!(!payload.plan.hvc1_tag);
        let back = serde_json::to_value(&job).unwrap();
        assert_eq!(
            back["kind"]["payload"]["plan"]["streams"][1]["language"],
            "fre"
        );
        assert!(back.get("inputUrl").is_none());
    }

    #[test]
    fn job_error_text_round_trips_its_class() {
        let t = JobErrorClass::Corrupt.message("moov atom not found");
        assert_eq!(
            JobErrorClass::from_message(&t),
            (JobErrorClass::Corrupt, "moov atom not found")
        );
        assert_eq!(
            JobErrorClass::from_message("ffmpeg stalled"),
            (JobErrorClass::Engine, "ffmpeg stalled")
        );
    }

    #[test]
    fn hello_and_result_use_camel_case_and_lowercase_enums() {
        let hello: WorkerHello = serde_json::from_value(json!({
            "instanceId": "w1", "engineVersion": "1.10.0",
            "devices": [{ "id": "cpu", "encoders": ["libx265"], "decoders": ["hevc"],
                "maxJobs": 1, "pause": "suspend" }]
        }))
        .unwrap();
        assert_eq!(hello.devices[0].pause, PauseMode::Suspend);
        assert!(hello.jobs.is_empty());
        let r = JobResultRequest {
            worker_id: "w1".into(),
            attempt: 1,
            ok: false,
            out_size: 0.0,
            out_sha256: None,
            out_info: None,
            error: Some(JobError {
                class: JobErrorClass::Nospace,
                message: "full".into(),
                stderr_tail: None,
            }),
            level: None,
            copy_reason: None,
            cpu_fallback: None,
            nothing_to_do: false,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["error"]["class"], "nospace");
        assert_eq!(v["workerId"], "w1");
    }
}
