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
    /// Drop the per-track statistics tags, which the new file would carry stale.
    #[serde(default)]
    pub strip_statistics_tags: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OptimizePayload {
    /// Probe of the source at queue time.
    pub info: MediaInfo,
    pub plan: OptimizePlan,
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
    /// The output failed its own check.
    Damaged,
    /// No room for the staging file.
    Nospace,
    /// The encoder crashed or misbehaved.
    Engine,
    /// A transfer with the host failed.
    Network,
    /// A `Verify` job found the file faulty.
    Verify,
    /// The device or file is in use; the job goes back without counting as a failure.
    Busy,
    /// The source no longer matches the queue-time snapshot.
    Stale,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["error"]["class"], "nospace");
        assert_eq!(v["workerId"], "w1");
    }
}
