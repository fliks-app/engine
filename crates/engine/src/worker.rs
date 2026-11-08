//! The worker loop: this engine pulls `Optimize` and `Verify` jobs from the host (`hello`, `lease`
//! per device, `heartbeat`, `result`) and runs them as leased jobs on their device's slot lane.
//! Protocol and fencing: `PROTOCOL.md`; sharing with playback: `docs/spec/jobs.md` section 3.
use crate::jobs::{self, BgJob};
use crate::lock::Locked;
use crate::resolve::Host;
use crate::runtime::Runtime;
use crate::slots::{CPU_DEVICE, Lane};
use axum::body::Bytes;
use axum::http::{Request, StatusCode, header};
use fliks_engine_core::api::{
    BgJobState, HEADER_ATTEMPT, HEADER_UPLOAD_COMPLETE, HEADER_UPLOAD_OFFSET, HEADER_WORKER_ID,
    HwAccel, JobError, JobErrorClass, JobHeartbeatRequest, JobHeartbeatResponse, JobKind,
    JobRequest, JobResult, JobResultRequest, JobSnapshot, LeaseRef, LeaseRequest, LeasedJob,
    MediaRef, UploadAck, WorkerDevice, WorkerHello, WorkerHelloResponse, WorkerSettings,
};
use fliks_engine_core::hw::Detected;
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::task::JoinSet;

const HEARTBEAT_S: u64 = 15;
const LEASE_S: u64 = 60;
const LONG_POLL_S: u64 = 30;
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const RETRY: Duration = Duration::from_secs(5);
/// A host without the worker protocol is asked again this rarely.
const UNSUPPORTED_RETRY: Duration = Duration::from_secs(300);

/// The host of a remote worker (no disk access to the library), fixed at startup.
static REMOTE: OnceLock<Host> = OnceLock::new();

/// Makes this engine a remote worker of `url`: it leases over `inputUrl` and uploads its outputs.
pub fn set_remote(url: String, token: String) {
    let _ = REMOTE.set(Host {
        url,
        token: Some(token),
    });
}

pub fn is_remote() -> bool {
    REMOTE.get().is_some()
}

/// Starts the loop; it idles until the host's config says `worker: true` (unless remote).
pub fn start(rt: &Arc<Runtime>) {
    tokio::spawn(supervise(rt.clone()));
}

fn enabled(rt: &Runtime) -> Option<Host> {
    if let Some(h) = REMOTE.get() {
        return Some(h.clone());
    }
    let c = rt.current_config();
    match (&c.host_url, c.worker) {
        (Some(url), true) => Some(Host {
            url: url.clone(),
            token: c.host_token.clone(),
        }),
        _ => None,
    }
}

/// http and https with the platform's roots: a worker outside the LAN reaches its host over https.
fn client() -> Client<HttpsConnector<HttpConnector>, Full<Bytes>> {
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    let tls = crate::tls_builder().https_or_http().enable_http1();
    Client::builder(TokioExecutor::new()).build(tls.wrap_connector(http))
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
            client: client(),
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

/// The optimize encoders a GPU advertises, and the only ones `Work::hw_failed` counts: the
/// backend's hevc/av1 encoders that passed detection and are not disabled.
pub(crate) fn optimize_encoders(d: &Detected) -> Vec<String> {
    gpu_encoders(d.hw_accel)
        .iter()
        .filter(|n| {
            d.encoders.iter().any(|e| e == *n) && !d.disabled_encoders.iter().any(|x| x == *n)
        })
        .map(|n| n.to_string())
        .collect()
}

fn gpu_encoders(hw: HwAccel) -> &'static [&'static str] {
    match hw {
        HwAccel::Nvenc => &["hevc_nvenc", "av1_nvenc"],
        HwAccel::Vaapi => &["hevc_vaapi", "av1_vaapi"],
        HwAccel::Qsv => &["hevc_qsv", "av1_qsv"],
        HwAccel::Amf => &["hevc_amf", "av1_amf"],
        HwAccel::Videotoolbox => &["hevc_videotoolbox"],
        HwAccel::None => &[],
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
        device_gpu_playback_before_pause: Default::default(),
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
    client: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
    held: Mutex<HashMap<u32, Held>>,
    settings: Mutex<WorkerSettings>,
}

/// Heartbeat timing of one lease: `deadline` is when it ends locally without an answer.
struct Pulse {
    lease: Duration,
    every: Duration,
    deadline: Instant,
    next: Instant,
}

impl Pulse {
    fn new(lease_s: u64, every_s: u64) -> Self {
        let (lease, every) = (Duration::from_secs(lease_s), Duration::from_secs(every_s));
        Self {
            lease,
            every,
            deadline: Instant::now() + lease,
            next: Instant::now() + every,
        }
    }
}

enum Upload {
    Fenced,
    Failed(String),
}

fn io_failed(e: std::io::Error) -> Upload {
    Upload::Failed(JobErrorClass::Engine.message(e.to_string()))
}

struct Answer {
    status: StatusCode,
    body: Bytes,
}

/// The host's answer to a worker it does not know (it restarted).
fn unknown_worker(a: &Answer) -> bool {
    a.status == StatusCode::NOT_FOUND && String::from_utf8_lossy(&a.body).contains("unknown worker")
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
        let req = Request::post(self.url(path)).header(header::CONTENT_TYPE, "application/json");
        self.send(req, Bytes::from(body), timeout).await
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.host.url.trim_end_matches('/'))
    }

    async fn send(
        &self,
        mut req: axum::http::request::Builder,
        body: Bytes,
        timeout: Duration,
    ) -> Result<Answer, String> {
        if let Some(t) = &self.host.token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let req = req.body(Full::new(body)).map_err(|e| e.to_string())?;
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

    /// The devices of this engine: the CPU and each detected GPU (`gpu0..gpuN`).
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
            name: None,
            encoders: usable(&["libx265", "libsvtav1"]),
            decoders: decoders.clone(),
            max_jobs: (self.rt.slots.size() / 2).max(1) as u32,
            pause,
        }];
        let snap = self.rt.hw.snapshot();
        for d in snap.iter().flat_map(|s| &s.devices) {
            let encoders = optimize_encoders(&d.detected);
            if !encoders.is_empty() {
                out.push(WorkerDevice {
                    id: d.id.clone(),
                    name: d.name.clone(),
                    encoders,
                    decoders: decoders.clone(),
                    max_jobs: 1,
                    pause,
                });
            }
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
                        for d in devices {
                            self.rt.slots.set_device_reserve(
                                &d.id,
                                r.settings
                                    .device_gpu_playback_before_pause
                                    .get(&d.id)
                                    .copied(),
                            );
                        }
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
            let lane = Lane::new(device.id.clone());
            if self.held_on(&device.id) >= device.max_jobs as usize
                || !self.rt.slots.has_room(&lane)
                || (lane.gpu() && !self.rt.slots.device_usable(&lane.device))
            {
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
                Ok(a) if unknown_worker(&a) => return,
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
        let (id, attempt, lease_s) = (leased.id, leased.attempt, leased.lease_s);
        let lane = Lane::new(device.id.clone());
        let remote = leased.input_path.is_none();
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
                tokio::spawn(
                    self.clone()
                        .watch(id, attempt, lease_s, job, dropped, remote),
                );
            }
            Err(e) => {
                tracing::warn!(job = id, "leased job refused: {e}");
                let end = Instant::now() + Duration::from_secs(u64::from(lease_s));
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
        lease_s: u32,
        job: Arc<BgJob>,
        dropped: Arc<AtomicBool>,
        remote: bool,
    ) {
        let mut pulse = Pulse::new(
            u64::from(lease_s),
            self.setting(|s| u64::from(s.heartbeat_s)),
        );
        let mut fenced = false;
        while !job.snapshot().state.ended() {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let snap = job.snapshot();
            let progress = (snap.pct / 100.0).clamp(0.0, 1.0);
            if self
                .pulse(
                    id,
                    attempt,
                    &mut pulse,
                    progress,
                    snap.state == BgJobState::Paused,
                )
                .await
            {
                fenced = true;
                break;
            }
        }
        let snap = job.snapshot();
        let mut body = None;
        if !fenced && !dropped.load(Relaxed) && remote {
            match self.upload(id, attempt, &snap, &mut pulse).await {
                Ok(()) => {}
                Err(Upload::Fenced) => fenced = true,
                Err(Upload::Failed(e)) => body = Some(error_result(&self.instance, attempt, &e)),
            }
        }
        if fenced || dropped.load(Relaxed) {
            // A cancel stops a run and its executor removes the file; a job already done leaves it.
            jobs::cancel(&self.rt, &job.id).await;
            remove_staged(&snap);
        } else if snap.state != BgJobState::Cancelled {
            let body = body.unwrap_or_else(|| result_request(&self.instance, attempt, &snap));
            let delivered = self.deliver(id, body, pulse.deadline).await;
            // A remote output was copied to the host; a local one is the host's to take.
            if remote || !delivered {
                remove_staged(&snap);
            }
        }
        self.held.locked().remove(&id);
    }

    /// Heartbeats when one is due; true when the host fenced the job or the lease ran out locally.
    async fn pulse(
        &self,
        id: u32,
        attempt: u32,
        p: &mut Pulse,
        progress: f64,
        paused: bool,
    ) -> bool {
        if Instant::now() < p.next {
            return false;
        }
        p.next = Instant::now() + p.every;
        let req = JobHeartbeatRequest {
            worker_id: self.instance.clone(),
            attempt,
            progress,
            paused,
        };
        let answer = self
            .post(&format!("/engine/jobs/{id}/heartbeat"), &req, CALL_TIMEOUT)
            .await;
        match beat(&answer) {
            Beat::Renewed => {
                p.deadline = Instant::now() + p.lease;
                false
            }
            Beat::Fenced => true,
            Beat::Unanswered => {
                if let Err(e) = &answer {
                    tracing::warn!(job = id, "heartbeat failed: {e}");
                }
                let expired = Instant::now() >= p.deadline;
                if expired {
                    tracing::warn!(job = id, "lease expired locally: stopping");
                }
                expired
            }
        }
    }

    /// Sends a remote job's output in `uploadChunkBytes` chunks, resuming from what the host holds.
    async fn upload(
        &self,
        id: u32,
        attempt: u32,
        snap: &JobSnapshot,
        pulse: &mut Pulse,
    ) -> Result<(), Upload> {
        let Some(JobResult::Optimize { optimize }) = &snap.result else {
            return Ok(());
        };
        if optimize.nothing_to_do {
            return Ok(());
        }
        let chunk = self.setting(|s| u64::from(s.upload_chunk_bytes)).max(1);
        let path = Path::new(&optimize.staged_path);
        let total = tokio::fs::metadata(path)
            .await
            .map_err(|e| {
                Upload::Failed(JobErrorClass::Engine.message(format!("{}: {e}", path.display())))
            })?
            .len();
        let mut file = tokio::fs::File::open(path).await.map_err(|e| {
            Upload::Failed(JobErrorClass::Engine.message(format!("{}: {e}", path.display())))
        })?;
        let mut offset = 0u64;
        loop {
            if self.pulse(id, attempt, pulse, 1.0, false).await {
                return Err(Upload::Fenced);
            }
            if Instant::now() >= pulse.deadline {
                return Err(Upload::Failed(
                    JobErrorClass::Network.message("the upload ran past the lease"),
                ));
            }
            let len = chunk.min(total - offset);
            let mut buf = vec![0u8; len as usize];
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(io_failed)?;
            file.read_exact(&mut buf).await.map_err(io_failed)?;
            let last = offset + len >= total;
            let req = Request::put(self.url(&format!("/engine/jobs/{id}/output")))
                .header(HEADER_UPLOAD_OFFSET, offset.to_string())
                .header(HEADER_ATTEMPT, attempt.to_string())
                .header(HEADER_WORKER_ID, &self.instance)
                .header(HEADER_UPLOAD_COMPLETE, if last { "?1" } else { "?0" })
                .header(header::CONTENT_TYPE, "application/octet-stream");
            match self.send(req, Bytes::from(buf), UPLOAD_TIMEOUT).await {
                Ok(a) if a.status.is_success() => {
                    offset += len;
                    if last {
                        return Ok(());
                    }
                }
                Ok(a) if a.status == StatusCode::CONFLICT => {
                    // The host holds another length: continue from it; no ack means the job is fenced.
                    match serde_json::from_slice::<UploadAck>(&a.body) {
                        Ok(ack) if (ack.received_bytes as u64) <= total => {
                            offset = ack.received_bytes as u64;
                            if offset >= total {
                                return Ok(());
                            }
                        }
                        _ => return Err(Upload::Fenced),
                    }
                }
                Ok(a) if a.status.is_client_error() => {
                    return Err(Upload::Failed(
                        JobErrorClass::Network
                            .message(format!("the host refused the output: {}", a.status)),
                    ));
                }
                other => {
                    if let Err(e) = other {
                        tracing::warn!(job = id, "upload failed: {e}");
                    }
                    tokio::time::sleep(RETRY).await;
                }
            }
        }
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
                Ok(a) if unknown_worker(&a) => {
                    // The host restarted: introduce this worker again, then retry the result.
                    let devices = self.devices().await;
                    self.hello(&devices).await;
                    if Instant::now() >= deadline {
                        return false;
                    }
                }
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
        level: None,
        copy_reason: None,
        cpu_fallback: None,
        nothing_to_do: false,
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
        level: None,
        copy_reason: None,
        cpu_fallback: None,
        nothing_to_do: false,
    };
    match &snap.result {
        Some(JobResult::Optimize { optimize }) if optimize.nothing_to_do => JobResultRequest {
            level: Some(optimize.level),
            copy_reason: optimize.copy_reason.clone(),
            cpu_fallback: Some(optimize.cpu_fallback),
            nothing_to_do: true,
            ..ok(0.0, None, None)
        },
        Some(JobResult::Optimize { optimize }) => JobResultRequest {
            level: Some(optimize.level),
            copy_reason: optimize.copy_reason.clone(),
            cpu_fallback: Some(optimize.cpu_fallback),
            ..ok(
                optimize.out_size,
                Some(optimize.out_sha256.clone()),
                Some(optimize.out_info.clone()),
            )
        },
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

    #[tokio::test]
    async fn the_client_accepts_an_https_url() {
        let req = Request::get("https://127.0.0.1:1/")
            .body(Full::new(Bytes::new()))
            .unwrap();
        let err = client().request(req).await.unwrap_err();
        assert!(
            !format!("{err:?}").contains("scheme is not http"),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn an_upload_resumes_from_the_offset_the_host_holds() {
        use axum::{Router, extract::State, http::HeaderMap, response::IntoResponse, routing::put};
        let held = Arc::new(Mutex::new(b"abc".to_vec()));
        async fn chunk(
            State(held): State<Arc<Mutex<Vec<u8>>>>,
            h: HeaderMap,
            body: Bytes,
        ) -> axum::response::Response {
            let off: usize = h[HEADER_UPLOAD_OFFSET].to_str().unwrap().parse().unwrap();
            let mut g = held.lock().unwrap();
            if off != g.len() {
                let ack = format!(r#"{{"receivedBytes":{}}}"#, g.len());
                return (StatusCode::CONFLICT, ack).into_response();
            }
            g.extend_from_slice(&body);
            StatusCode::OK.into_response()
        }
        let app = Router::new()
            .route("/engine/jobs/{id}/output", put(chunk))
            .with_state(held.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });

        let hw = crate::hw::Hw::new(Arc::new(|_| Box::pin(async { unreachable!() })));
        let rt = Runtime::new(hw, crate::cache::Cache::from_env(), "ffmpeg".into());
        let w = Worker {
            rt,
            host: Host { url, token: None },
            instance: "w".into(),
            client: client(),
            held: Mutex::new(HashMap::new()),
            settings: Mutex::new(WorkerSettings {
                upload_chunk_bytes: 4,
                ..default_settings()
            }),
        };
        let dir = std::env::temp_dir().join(format!("fliks-up-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let staged = dir.join(".out");
        std::fs::write(&staged, b"abcdefghij").unwrap();
        let meta = OptimizeMeta {
            staged_path: staged.to_string_lossy().into_owned(),
            out_size: 10.0,
            out_sha256: "ab".into(),
            out_info: MediaInfo::default(),
            level: OptimizeLevel::Remux,
            copy_reason: None,
            cpu_fallback: false,
            nothing_to_do: false,
        };
        let done = JobResult::Optimize {
            optimize: Box::new(meta),
        };
        let snap = snapshot(BgJobState::Done, Some(done), None);
        let mut pulse = Pulse::new(60, 3600);
        assert!(w.upload(1, 1, &snap, &mut pulse).await.is_ok());
        assert_eq!(held.lock().unwrap().as_slice(), b"abcdefghij");
        let _ = std::fs::remove_dir_all(&dir);
    }

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

    #[test]
    fn a_404_unknown_worker_asks_for_a_new_hello() {
        assert!(unknown_worker(&answer(404, "unknown worker").unwrap()));
        assert!(!unknown_worker(&answer(404, "no such job").unwrap()));
        assert!(!unknown_worker(&answer(409, "unknown worker").unwrap()));
    }

    #[test]
    fn a_finished_output_of_a_fenced_job_is_removed() {
        let dir = std::env::temp_dir().join(format!("fliks-fenced-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let staged = dir.join(".stage");
        std::fs::write(&staged, b"x").unwrap();
        let meta = OptimizeMeta {
            staged_path: staged.to_string_lossy().into_owned(),
            out_size: 1.0,
            out_sha256: "ab".into(),
            out_info: MediaInfo::default(),
            level: OptimizeLevel::Remux,
            copy_reason: None,
            cpu_fallback: false,
            nothing_to_do: false,
        };
        let done = JobResult::Optimize {
            optimize: Box::new(meta),
        };
        remove_staged(&snapshot(BgJobState::Done, Some(done), None));
        assert!(!staged.exists());
        let _ = std::fs::remove_dir_all(&dir);
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
            nothing_to_do: false,
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
        assert_eq!(r.level, Some(OptimizeLevel::Remux));
        assert!(!r.nothing_to_do);
    }

    #[test]
    fn a_video_copy_reports_what_was_done_and_a_noop_has_no_output() {
        let meta = OptimizeMeta {
            staged_path: "/m/.s".into(),
            out_size: 0.0,
            out_sha256: String::new(),
            out_info: MediaInfo::default(),
            level: OptimizeLevel::Remux,
            copy_reason: Some("low_savings".into()),
            cpu_fallback: true,
            nothing_to_do: true,
        };
        let snap = snapshot(
            BgJobState::Done,
            Some(JobResult::Optimize {
                optimize: Box::new(meta),
            }),
            None,
        );
        let r = result_request("w", 1, &snap);
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["level"], "remux");
        assert_eq!(v["copyReason"], "low_savings");
        assert_eq!(v["cpuFallback"], true);
        assert_eq!(v["nothingToDo"], true);
        assert!(r.ok && r.out_sha256.is_none() && r.out_info.is_none());
    }
}
