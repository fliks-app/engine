//! The worker loop: this engine pulls `Optimize` and `Verify` jobs from the host (`hello`, `lease`
//! per device, `heartbeat`, `result`) and runs them as leased jobs on their device's slot lane.
//! Protocol and fencing: `PROTOCOL.md`; sharing with playback: `docs/spec/jobs.md` section 3.
use crate::jobs::{self, BgJob};
use crate::lock::Locked;
use crate::resolve::Host;
use crate::runtime::Runtime;
use crate::slots::{CPU_DEVICE, GPU_DEVICE, Lane};
use axum::body::Bytes;
use axum::http::{Request, StatusCode, header};
use fliks_engine_core::api::{
    BgJobState, HwAccel, JobError, JobErrorClass, JobHeartbeatRequest, JobHeartbeatResponse,
    JobKind, JobRequest, JobResult, JobResultRequest, JobSnapshot, LeaseRef, LeaseRequest,
    LeasedJob, MediaRef, WorkerDevice, WorkerHello, WorkerHelloResponse, WorkerSettings,
};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::task::JoinSet;

const HEARTBEAT_S: u64 = 15;
const LEASE_S: u64 = 60;
const LONG_POLL_S: u64 = 30;
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
const RETRY: Duration = Duration::from_secs(5);
/// A host without the worker protocol is asked again this rarely.
const UNSUPPORTED_RETRY: Duration = Duration::from_secs(300);

/// Starts the loop; it idles until the host's config says `worker: true`.
pub fn start(rt: &Arc<Runtime>) {
    tokio::spawn(supervise(rt.clone()));
}

fn enabled(rt: &Runtime) -> Option<Host> {
    let c = rt.current_config();
    match (&c.host_url, c.worker) {
        (Some(url), true) => Some(Host {
            url: url.clone(),
            token: c.host_token.clone(),
        }),
        _ => None,
    }
}

fn same(a: &Host, b: &Option<Host>) -> bool {
    b.as_ref()
        .is_some_and(|b| a.url == b.url && a.token == b.token)
}

async fn supervise(rt: Arc<Runtime>) {
    let instance = crate::random_hex(8);
    loop {
        let Some(host) = enabled(&rt) else {
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        };
        let w = Arc::new(Worker {
            rt: rt.clone(),
            host: host.clone(),
            instance: instance.clone(),
            client: Client::builder(TokioExecutor::new()).build_http(),
            held: Mutex::new(HashMap::new()),
            settings: Mutex::new(default_settings()),
        });
        tokio::select! {
            _ = w.clone().run() => {}
            _ = async {
                while same(&host, &enabled(&rt)) {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            } => {}
        }
        w.drop_all().await;
    }
}

fn default_settings() -> WorkerSettings {
    WorkerSettings {
        lease_s: LEASE_S as u32,
        heartbeat_s: HEARTBEAT_S as u32,
        long_poll_s: LONG_POLL_S as u32,
        upload_chunk_bytes: fliks_engine_core::api::UPLOAD_CHUNK_BYTES,
        cpu_weight: None,
        gpu_playback_before_pause: None,
    }
}

struct Held {
    attempt: u32,
    device: String,
    job: Arc<BgJob>,
    /// Set when the host told the worker to let go: no result is sent for it.
    dropped: Arc<AtomicBool>,
}

struct Worker {
    rt: Arc<Runtime>,
    host: Host,
    instance: String,
    client: Client<HttpConnector, Full<Bytes>>,
    held: Mutex<HashMap<u32, Held>>,
    settings: Mutex<WorkerSettings>,
}

struct Answer {
    status: StatusCode,
    body: Bytes,
}

/// What one heartbeat answer means for the job.
#[derive(Debug, PartialEq, Eq)]
enum Beat {
    /// Alive: the lease is renewed.
    Renewed,
    /// No answer worth acting on (network error, 5xx): retry until the local expiry.
    Unanswered,
    /// The host fenced or cancelled the job: kill the encoder and delete the files.
    Fenced,
}

/// `409` is the only status that fences; a transport error or a 5xx kills nothing.
fn beat(answer: &Result<Answer, String>) -> Beat {
    match answer {
        Ok(a) if a.status == StatusCode::CONFLICT => Beat::Fenced,
        Ok(a) if a.status.is_success() => {
            match serde_json::from_slice::<JobHeartbeatResponse>(&a.body) {
                Ok(r) if r.cancel => Beat::Fenced,
                _ => Beat::Renewed,
            }
        }
        _ => Beat::Unanswered,
    }
}

impl Worker {
    async fn post(
        &self,
        path: &str,
        body: &impl Serialize,
        timeout: Duration,
    ) -> Result<Answer, String> {
        let body = serde_json::to_vec(body).map_err(|e| e.to_string())?;
        let mut req = Request::post(format!("{}{path}", self.host.url.trim_end_matches('/')))
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(t) = &self.host.token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let req = req
            .body(Full::new(Bytes::from(body)))
            .map_err(|e| e.to_string())?;
        let call = async {
            let res = self.client.request(req).await.map_err(|e| e.to_string())?;
            let status = res.status();
            let body = res.into_body().collect().await.map_err(|e| e.to_string())?;
            Ok(Answer {
                status,
                body: body.to_bytes(),
            })
        };
        tokio::time::timeout(timeout, call)
            .await
            .map_err(|_| "timed out".to_string())?
    }

    fn setting<T>(&self, f: impl Fn(&WorkerSettings) -> T) -> T {
        f(&self.settings.locked())
    }

    fn held_on(&self, device: &str) -> usize {
        self.held
            .locked()
            .values()
            .filter(|h| h.device == device)
            .count()
    }

    /// Hello, then one lease loop per device; a lease that answers "unknown worker" (the host
    /// restarted) starts over with a hello that lists the jobs still held.
    async fn run(self: Arc<Self>) {
        loop {
            let devices = self.devices().await;
            for d in &devices {
                self.rt.slots.set_device_limit(&d.id, d.max_jobs);
            }
            if !self.hello(&devices).await {
                continue;
            }
            let mut loops = JoinSet::new();
            for d in devices {
                loops.spawn(self.clone().lease_loop(d));
            }
            let _ = loops.join_next().await;
            loops.abort_all();
        }
    }

    /// The devices of this engine: the CPU and, when detected, its one GPU.
    async fn devices(&self) -> Vec<WorkerDevice> {
        let caps = loop {
            match self.rt.caps() {
                Some(c) => break c,
                None => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        };
        let pause = self.rt.optimize_pause();
        let usable = |names: &[&str]| -> Vec<String> {
            names
                .iter()
                .filter(|n| !caps.disabled_encoders.iter().any(|d| d == *n))
                .map(|n| n.to_string())
                .collect()
        };
        let decoders: Vec<String> = ["h264", "hevc", "av1", "vp9", "mpeg2video"]
            .map(String::from)
            .to_vec();
        let mut out = vec![WorkerDevice {
            id: CPU_DEVICE.into(),
            encoders: usable(&["libx265", "libsvtav1"]),
            decoders: decoders.clone(),
            max_jobs: (self.rt.slots.size() / 2).max(1) as u32,
            vram_mib: None,
            pause,
        }];
        let gpu: &[&str] = match caps.hw_accel {
            HwAccel::Nvenc => &["hevc_nvenc", "av1_nvenc"],
            HwAccel::Vaapi => &["hevc_vaapi", "av1_vaapi"],
            HwAccel::Qsv => &["hevc_qsv", "av1_qsv"],
            HwAccel::Amf => &["hevc_amf", "av1_amf"],
            HwAccel::Videotoolbox => &["hevc_videotoolbox"],
            HwAccel::None => &[],
        };
        let encoders = usable(gpu);
        if !encoders.is_empty() {
            out.push(WorkerDevice {
                id: GPU_DEVICE.into(),
                encoders,
                decoders,
                max_jobs: 1,
                vram_mib: None,
                pause,
            });
        }
        out
    }

    /// False when the host did not take it: the caller asks again.
    async fn hello(&self, devices: &[WorkerDevice]) -> bool {
        let jobs = self
            .held
            .locked()
            .iter()
            .map(|(id, h)| LeaseRef {
                id: *id,
                attempt: h.attempt,
            })
            .collect();
        let body = WorkerHello {
            instance_id: self.instance.clone(),
            engine_version: env!("CARGO_PKG_VERSION").into(),
            devices: devices.to_vec(),
            jobs,
        };
        let answer = self
            .post("/engine/workers/hello", &body, CALL_TIMEOUT)
            .await;
        let wait = match &answer {
            Ok(a) if a.status.is_success() => {
                match serde_json::from_slice::<WorkerHelloResponse>(&a.body) {
                    Ok(r) => {
                        self.rt.slots.set_optimize(
                            r.settings.cpu_weight,
                            r.settings.gpu_playback_before_pause,
                        );
                        *self.settings.locked() = r.settings;
                        for id in r.abandon {
                            self.drop_job(id).await;
                        }
                        return true;
                    }
                    Err(e) => {
                        tracing::warn!("worker hello: unreadable answer: {e}");
                        RETRY
                    }
                }
            }
            Ok(a) if matches!(a.status.as_u16(), 404 | 405 | 501) => {
                tracing::warn!("the host does not serve the worker protocol ({})", a.status);
                UNSUPPORTED_RETRY
            }
            Ok(a) => {
                tracing::warn!(status = %a.status, "worker hello refused");
                RETRY
            }
            Err(e) => {
                tracing::warn!("worker hello failed: {e}");
                RETRY
            }
        };
        tokio::time::sleep(wait).await;
        false
    }

    /// Leases for one device while it has room; returns when the host forgot this worker.
    async fn lease_loop(self: Arc<Self>, device: WorkerDevice) {
        loop {
            if self.held_on(&device.id) >= device.max_jobs as usize {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            let req = LeaseRequest {
                worker_id: self.instance.clone(),
                device_id: device.id.clone(),
            };
            let poll = Duration::from_secs(self.setting(|s| u64::from(s.long_poll_s)) + 10);
            match self.post("/engine/jobs/lease", &req, poll).await {
                Ok(a) if a.status == StatusCode::OK => match serde_json::from_slice(&a.body) {
                    Ok(job) => self.accept(job, &device).await,
                    Err(e) => {
                        tracing::warn!("lease: unreadable job: {e}");
                        tokio::time::sleep(RETRY).await;
                    }
                },
                Ok(a) if a.status == StatusCode::NO_CONTENT => {}
                Ok(a)
                    if a.status == StatusCode::NOT_FOUND
                        && String::from_utf8_lossy(&a.body).contains("unknown worker") =>
                {
                    return;
                }
                Ok(a) => {
                    tracing::warn!(status = %a.status, "lease refused");
                    tokio::time::sleep(RETRY).await;
                }
                Err(e) => {
                    tracing::warn!("lease failed: {e}");
                    tokio::time::sleep(RETRY).await;
                }
            }
        }
    }

    /// Runs the leased job on its device's lane and watches it until its result is delivered.
    async fn accept(self: &Arc<Self>, leased: LeasedJob, device: &WorkerDevice) {
        let (id, attempt) = (leased.id, leased.attempt);
        let lane = Lane {
            device: device.id.clone(),
            gpu: device.id == GPU_DEVICE,
        };
        let started = request(leased).and_then(|req| {
            jobs::submit_leased(&self.rt, req, lane, &format!("lease:{id}:{attempt}"))
        });
        match started {
            Ok(job) => {
                let dropped = Arc::new(AtomicBool::new(false));
                self.held.locked().insert(
                    id,
                    Held {
                        attempt,
                        device: device.id.clone(),
                        job: job.clone(),
                        dropped: dropped.clone(),
                    },
                );
                tokio::spawn(self.clone().watch(id, attempt, job, dropped));
            }
            Err(e) => {
                tracing::warn!(job = id, "leased job refused: {e}");
                let end = Instant::now() + Duration::from_secs(LEASE_S);
                self.deliver(id, error_result(&self.instance, attempt, &e), end)
                    .await;
            }
        }
    }

    /// Heartbeats the job every `heartbeatS`, then reports how it ended.
    async fn watch(
        self: Arc<Self>,
        id: u32,
        attempt: u32,
        job: Arc<BgJob>,
        dropped: Arc<AtomicBool>,
    ) {
        let lease = Duration::from_secs(self.setting(|s| u64::from(s.lease_s)));
        let every = Duration::from_secs(self.setting(|s| u64::from(s.heartbeat_s)));
        let mut deadline = Instant::now() + lease;
        let mut next = Instant::now() + every;
        let mut fenced = false;
        while !job.snapshot().state.ended() {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if Instant::now() < next {
                continue;
            }
            next = Instant::now() + every;
            let snap = job.snapshot();
            let req = JobHeartbeatRequest {
                worker_id: self.instance.clone(),
                attempt,
                progress: (snap.pct / 100.0).clamp(0.0, 1.0),
                paused: snap.state == BgJobState::Paused,
            };
            let answer = self
                .post(&format!("/engine/jobs/{id}/heartbeat"), &req, CALL_TIMEOUT)
                .await;
            match beat(&answer) {
                Beat::Renewed => deadline = Instant::now() + lease,
                Beat::Fenced => {
                    fenced = true;
                    break;
                }
                Beat::Unanswered => {
                    if let Err(e) = &answer {
                        tracing::warn!(job = id, "heartbeat failed: {e}");
                    }
                    if Instant::now() >= deadline {
                        tracing::warn!(job = id, "lease expired locally: stopping");
                        fenced = true;
                        break;
                    }
                }
            }
        }
        if fenced || dropped.load(Relaxed) {
            // The executor removes its staged file when it is cancelled.
            jobs::cancel(&self.rt, &job.id).await;
        } else {
            let snap = job.snapshot();
            if snap.state != BgJobState::Cancelled {
                let body = result_request(&self.instance, attempt, &snap);
                if !self.deliver(id, body, deadline).await {
                    remove_staged(&snap);
                }
            }
        }
        self.held.locked().remove(&id);
    }

    /// Posts the result until the host answers; false when it fenced it or the lease ran out.
    async fn deliver(&self, id: u32, body: JobResultRequest, deadline: Instant) -> bool {
        loop {
            match self
                .post(&format!("/engine/jobs/{id}/result"), &body, CALL_TIMEOUT)
                .await
            {
                Ok(a) if a.status.is_success() => return true,
                Ok(a) if a.status == StatusCode::CONFLICT => return false,
                Ok(a) if a.status.is_client_error() => {
                    tracing::warn!(job = id, status = %a.status, "result refused");
                    return false;
                }
                other => {
                    if let Err(e) = other {
                        tracing::warn!(job = id, "result failed: {e}");
                    }
                    if Instant::now() >= deadline {
                        return false;
                    }
                    tokio::time::sleep(RETRY).await;
                }
            }
        }
    }

    /// The host abandoned this lease: kill the run, report nothing.
    async fn drop_job(&self, id: u32) {
        let held = self.held.locked().remove(&id);
        if let Some(h) = held {
            h.dropped.store(true, Relaxed);
            jobs::cancel(&self.rt, &h.job.id).await;
        }
    }

    async fn drop_all(&self) {
        let ids: Vec<u32> = self.held.locked().keys().copied().collect();
        for id in ids {
            self.drop_job(id).await;
        }
    }
}

/// The engine job a lease asks for; the lease's paths and staging file go into the payload.
fn request(l: LeasedJob) -> Result<JobRequest, String> {
    let LeasedJob {
        kind,
        input_path,
        staged_path,
        input_url,
        input_token,
        ..
    } = l;
    let path = input_path
        .clone()
        .or_else(|| input_url.clone())
        .ok_or_else(|| JobErrorClass::Access.message("the lease has no input"))?;
    let kind = match kind {
        JobKind::Optimize { mut payload } => {
            payload.staged_path = staged_path.or(payload.staged_path);
            if input_path.is_none() {
                payload.input_url = input_url;
                payload.input_token = input_token;
            }
            JobKind::Optimize { payload }
        }
        JobKind::Verify { .. } if input_path.is_none() => {
            return Err(JobErrorClass::Access.message("a verification reads a local file"));
        }
        other => other,
    };
    Ok(JobRequest {
        kind,
        media: MediaRef {
            media_file_id: 0,
            path,
        },
        priority: None,
        force: false,
    })
}

fn error_result(worker: &str, attempt: u32, e: &str) -> JobResultRequest {
    let (class, message) = JobErrorClass::from_message(e);
    JobResultRequest {
        worker_id: worker.into(),
        attempt,
        ok: false,
        out_size: 0.0,
        out_sha256: None,
        out_info: None,
        error: Some(JobError {
            class,
            message: message.into(),
            stderr_tail: None,
        }),
    }
}

/// The result of an ended job: the optimization's output facts, or its `<class>: <message>` error.
fn result_request(worker: &str, attempt: u32, snap: &JobSnapshot) -> JobResultRequest {
    let ok = |out_size, out_sha256, out_info| JobResultRequest {
        worker_id: worker.into(),
        attempt,
        ok: true,
        out_size,
        out_sha256,
        out_info,
        error: None,
    };
    match &snap.result {
        Some(JobResult::Optimize { optimize }) => ok(
            optimize.out_size,
            Some(optimize.out_sha256.clone()),
            Some(optimize.out_info.clone()),
        ),
        Some(JobResult::Verify { .. }) => ok(0.0, None, None),
        _ => error_result(
            worker,
            attempt,
            snap.error
                .as_deref()
                .unwrap_or("engine: the job ended without a result"),
        ),
    }
}

/// A finished output the host will never read: the lease was lost.
fn remove_staged(snap: &JobSnapshot) {
    if let Some(JobResult::Optimize { optimize }) = &snap.result {
        let _ = std::fs::remove_file(PathBuf::from(&optimize.staged_path));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fliks_engine_core::api::{JobPriority, MediaInfo, OptimizeLevel, OptimizeMeta};

    fn answer(status: u16, body: &str) -> Result<Answer, String> {
        Ok(Answer {
            status: StatusCode::from_u16(status).unwrap(),
            body: Bytes::from(body.to_string()),
        })
    }

    #[test]
    fn only_a_409_or_a_cancel_stops_the_job() {
        let ok = r#"{"leaseUntil":1,"cancel":false}"#;
        assert_eq!(beat(&answer(200, ok)), Beat::Renewed);
        assert_eq!(beat(&answer(409, "")), Beat::Fenced);
        assert_eq!(
            beat(&answer(200, r#"{"leaseUntil":1,"cancel":true}"#)),
            Beat::Fenced
        );
        assert_eq!(beat(&answer(503, "")), Beat::Unanswered);
        assert_eq!(beat(&answer(404, "")), Beat::Unanswered);
        assert_eq!(beat(&Err("connection refused".into())), Beat::Unanswered);
    }

    fn snapshot(state: BgJobState, result: Option<JobResult>, error: Option<&str>) -> JobSnapshot {
        JobSnapshot {
            job_id: "j".into(),
            kind: "Optimize".into(),
            media_file_id: 0,
            priority: JobPriority::Normal,
            state,
            pct: 100.0,
            created_ms: 0.0,
            started_ms: None,
            ended_ms: None,
            result,
            error: error.map(String::from),
        }
    }

    #[test]
    fn a_failed_job_reports_its_error_class() {
        let snap = snapshot(BgJobState::Failed, None, Some("nospace: 10 bytes free"));
        let r = result_request("w", 3, &snap);
        let e = r.error.expect("error");
        assert!(!r.ok && r.attempt == 3);
        assert_eq!(
            (e.class, e.message.as_str()),
            (JobErrorClass::Nospace, "10 bytes free")
        );
        let plain = result_request("w", 1, &snapshot(BgJobState::Failed, None, Some("boom")));
        assert_eq!(plain.error.unwrap().class, JobErrorClass::Engine);
    }

    #[test]
    fn an_optimized_job_reports_its_output() {
        let meta = OptimizeMeta {
            staged_path: "/m/.s".into(),
            out_size: 12.0,
            out_sha256: "ab".into(),
            out_info: MediaInfo::default(),
            level: OptimizeLevel::Remux,
            copy_reason: None,
            cpu_fallback: false,
        };
        let snap = snapshot(
            BgJobState::Done,
            Some(JobResult::Optimize {
                optimize: Box::new(meta),
            }),
            None,
        );
        let r = result_request("w", 1, &snap);
        assert!(r.ok && r.out_size == 12.0 && r.out_sha256.as_deref() == Some("ab"));
    }
}
