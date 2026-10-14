//! Background jobs: queue, priority, cancellation, progress, disk check, and their ffmpeg runs on
//! the shared slot budget. Spec: `docs/spec/jobs.md`.
mod fingerprint;
mod scan;
mod sprite;
pub(crate) mod subtitles;

use crate::AppState;
use crate::proc::{Ffmpeg, Pipes, background};
use crate::runtime::{Runtime, map_media_path, now_ms};
use crate::slots::BackgroundSlot;
use axum::{
    Json,
    extract::{Path as UrlPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use fliks_engine_core::api::{
    BgJobState, EngineEvent, FingerprintWindow, JobAccepted, JobKind, JobPriority, JobRequest,
    JobResult, JobSnapshot,
};
use fliks_engine_core::jobs::sprite::RUN_TIMEOUT_MS;
use fliks_engine_core::session::{JobWatch, JobWatchInput, job_watch};
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Jobs running at once. [ts:modules/streaming/thumbnail.service.ts:56]
const MAX_RUNNING: usize = 2;
const KEEP_ENDED: usize = 200;
const TICK: Duration = Duration::from_millis(200);
const PROGRESS_EVERY: Duration = Duration::from_secs(1);
const DEFAULT_MIN_FREE_BYTES: f64 = 1024.0 * 1024.0 * 1024.0;
pub const CANCELLED: &str = "cancelled";

struct JobMut {
    /// `Queued`, `Running` or an end state; `Paused` is derived from the runs.
    state: BgJobState,
    pct: f64,
    started_ms: Option<i64>,
    ended_ms: Option<i64>,
    result: Option<JobResult>,
    error: Option<String>,
    runs: u32,
    paused_runs: u32,
    last_progress: Option<Instant>,
}

pub struct BgJob {
    pub id: String,
    pub req: JobRequest,
    dedupe: String,
    created_ms: i64,
    cancel: watch::Sender<bool>,
    /// Bumped by `Ctx::pulse`: work the tools' stderr and CPU do not show (reading a file).
    activity: AtomicU64,
    m: Mutex<JobMut>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl BgJob {
    fn priority(&self) -> JobPriority {
        self.req.priority.unwrap_or_default()
    }

    fn base_state(&self) -> BgJobState {
        self.m.lock().unwrap().state
    }

    pub fn snapshot(&self) -> JobSnapshot {
        let m = self.m.lock().unwrap();
        let paused = m.state == BgJobState::Running && m.runs > 0 && m.paused_runs == m.runs;
        JobSnapshot {
            job_id: self.id.clone(),
            kind: self.req.kind.name().into(),
            media_file_id: self.req.media.media_file_id,
            priority: self.priority(),
            state: if paused { BgJobState::Paused } else { m.state },
            pct: m.pct,
            created_ms: self.created_ms as f64,
            started_ms: m.started_ms.map(|t| t as f64),
            ended_ms: m.ended_ms.map(|t| t as f64),
            result: m.result.clone(),
            error: m.error.clone(),
        }
    }
}

/// Every job of the engine, in creation order (ended ones trimmed to the last `KEEP_ENDED`).
#[derive(Default)]
pub struct Jobs {
    list: Mutex<Vec<Arc<BgJob>>>,
}

impl Jobs {
    pub fn get(&self, id: &str) -> Option<Arc<BgJob>> {
        self.list
            .lock()
            .unwrap()
            .iter()
            .find(|j| j.id == id)
            .cloned()
    }

    pub fn snapshots(&self) -> Vec<JobSnapshot> {
        let list = self.list.lock().unwrap().clone();
        list.iter().map(|j| j.snapshot()).collect()
    }

    fn live(&self, keep: impl Fn(&BgJob) -> bool) -> Vec<Arc<BgJob>> {
        let list = self.list.lock().unwrap().clone();
        list.into_iter()
            .filter(|j| !j.base_state().ended() && keep(j))
            .collect()
    }
}

fn validate(kind: &JobKind) -> Result<(), String> {
    match kind {
        JobKind::Sprite { info } => {
            if !info
                .duration_seconds
                .is_some_and(|d| d.is_finite() && d > 0.0)
            {
                return Err("Sprite needs a positive info.durationSeconds".into());
            }
        }
        JobKind::Scan { info } => {
            if info.video.is_empty() {
                return Err("Scan needs a video stream in info".into());
            }
        }
        JobKind::Fingerprint { window, .. } => {
            if let FingerprintWindow::Outro { duration_seconds } = window
                && !(duration_seconds.is_finite() && *duration_seconds > 0.0)
            {
                return Err("Fingerprint outro needs a positive durationSeconds".into());
            }
        }
        JobKind::SubtitleExtract { info } => subtitles::validate(info)?,
    }
    Ok(())
}

/// Queues a job, or answers the in-flight one of an identical request.
pub fn submit(rt: &Arc<Runtime>, req: JobRequest) -> Result<String, String> {
    if !Path::new(&req.media.path).is_absolute() {
        return Err("media.path must be absolute".into());
    }
    validate(&req.kind)?;
    let dedupe = serde_json::to_string(&(&req.kind, &req.media)).unwrap_or_default();
    let id = {
        let mut list = rt.jobs.list.lock().unwrap();
        if let Some(j) = list
            .iter()
            .find(|j| j.dedupe == dedupe && !j.base_state().ended())
        {
            return Ok(j.id.clone());
        }
        let j = Arc::new(BgJob {
            id: crate::random_hex(8),
            req,
            dedupe,
            created_ms: now_ms(),
            cancel: watch::channel(false).0,
            activity: AtomicU64::new(0),
            m: Mutex::new(JobMut {
                state: BgJobState::Queued,
                pct: 0.0,
                started_ms: None,
                ended_ms: None,
                result: None,
                error: None,
                runs: 0,
                paused_runs: 0,
                last_progress: None,
            }),
            task: Mutex::new(None),
        });
        list.push(j.clone());
        j.id.clone()
    };
    pump(rt);
    Ok(id)
}

/// Starts queued jobs by priority, then age, while fewer than `MAX_RUNNING` run.
fn pump(rt: &Arc<Runtime>) {
    let start: Vec<Arc<BgJob>> = {
        let list = rt.jobs.list.lock().unwrap();
        let running = list
            .iter()
            .filter(|j| j.base_state() == BgJobState::Running)
            .count();
        let mut queued: Vec<&Arc<BgJob>> = list
            .iter()
            .filter(|j| j.base_state() == BgJobState::Queued)
            .collect();
        queued.sort_by_key(|j| j.priority());
        let picked: Vec<Arc<BgJob>> = queued
            .into_iter()
            .take(MAX_RUNNING.saturating_sub(running))
            .cloned()
            .collect();
        for j in &picked {
            let mut m = j.m.lock().unwrap();
            m.state = BgJobState::Running;
            m.started_ms = Some(now_ms());
        }
        picked
    };
    for j in start {
        let task = tokio::spawn(run(rt.clone(), j.clone()));
        *j.task.lock().unwrap() = Some(task);
    }
}

async fn run(rt: Arc<Runtime>, job: Arc<BgJob>) {
    let ctx = Ctx {
        rt: rt.clone(),
        job: job.clone(),
    };
    let out = match &job.req.kind {
        JobKind::Sprite { info } => sprite::run(&ctx, info).await,
        JobKind::Scan { info } => scan::run(&ctx, info).await,
        JobKind::Fingerprint {
            window,
            max_lookup_minutes,
        } => fingerprint::run(&ctx, window, *max_lookup_minutes).await,
        JobKind::SubtitleExtract { info } => subtitles::run(&ctx, info).await,
    };
    let (state, result, error) = match out {
        Ok(r) => (BgJobState::Done, Some(r), None),
        Err(_) if ctx.cancelled() => (BgJobState::Cancelled, None, Some(CANCELLED.into())),
        Err(e) => {
            tracing::warn!(job = job.id, kind = job.req.kind.name(), "job failed: {e}");
            (BgJobState::Failed, None, Some(e))
        }
    };
    finish(&rt, &job, state, result, error);
    pump(&rt);
}

fn finish(
    rt: &Runtime,
    job: &BgJob,
    state: BgJobState,
    result: Option<JobResult>,
    error: Option<String>,
) {
    {
        let mut m = job.m.lock().unwrap();
        if m.state.ended() {
            return;
        }
        m.state = state;
        m.ended_ms = Some(now_ms());
        m.result = result.clone();
        m.error = error.clone();
        if state == BgJobState::Done {
            m.pct = 100.0;
        }
    }
    rt.events.emit(EngineEvent::BgJobDone {
        job_id: job.id.clone(),
        state,
        result,
        error,
    });
    let mut list = rt.jobs.list.lock().unwrap();
    let ended = list.iter().filter(|j| j.base_state().ended()).count();
    for _ in KEEP_ENDED..ended {
        if let Some(i) = list.iter().position(|j| j.base_state().ended()) {
            list.remove(i);
        }
    }
}

/// Cancels `jobs` and waits until their runs are gone.
async fn stop(rt: &Arc<Runtime>, jobs: Vec<Arc<BgJob>>) {
    for j in &jobs {
        j.cancel.send_replace(true);
        if j.base_state() == BgJobState::Queued {
            finish(rt, j, BgJobState::Cancelled, None, Some(CANCELLED.into()));
        }
    }
    for j in jobs {
        let task = j.task.lock().unwrap().take();
        if let Some(t) = task {
            let _ = t.await;
        }
    }
}

/// `DELETE /v1/jobs/{id}`: false when unknown.
pub async fn cancel(rt: &Arc<Runtime>, id: &str) -> bool {
    let Some(j) = rt.jobs.get(id) else {
        return false;
    };
    if !j.base_state().ended() {
        stop(rt, vec![j]).await;
    }
    true
}

/// Invalidate: every unfinished job of the file. Returns how many were cancelled.
pub async fn cancel_file(rt: &Arc<Runtime>, file: u32) -> u32 {
    let jobs = rt.jobs.live(|j| j.req.media.media_file_id == file);
    let n = jobs.len() as u32;
    stop(rt, jobs).await;
    n
}

pub async fn shutdown(rt: &Arc<Runtime>) {
    stop(rt, rt.jobs.live(|_| true)).await;
}

/// What a running job uses: its ffmpeg runs, progress, cancellation and the disk check.
#[derive(Clone)]
pub struct Ctx {
    pub rt: Arc<Runtime>,
    pub job: Arc<BgJob>,
}

impl Ctx {
    pub fn cancelled(&self) -> bool {
        *self.job.cancel.borrow()
    }

    /// `BgJobProgress`, at most once per `PROGRESS_EVERY`.
    pub fn progress(&self, pct: f64) {
        let due = {
            let mut m = self.job.m.lock().unwrap();
            m.pct = pct;
            let due = m
                .last_progress
                .is_none_or(|t| t.elapsed() >= PROGRESS_EVERY);
            if due {
                m.last_progress = Some(Instant::now());
            }
            due
        };
        if due {
            self.rt.events.emit(EngineEvent::BgJobProgress {
                job_id: self.job.id.clone(),
                pct,
            });
        }
    }

    /// Fails below `streaming.jobMinFreeBytes` free under `dir` (created first).
    pub async fn check_disk(&self, dir: &Path) -> Result<(), String> {
        let min = self
            .rt
            .current_config()
            .streaming
            .job_min_free_bytes
            .unwrap_or(DEFAULT_MIN_FREE_BYTES);
        let d = dir.to_path_buf();
        let free = tokio::task::spawn_blocking(move || {
            let _ = std::fs::create_dir_all(&d);
            free_bytes(&d)
        })
        .await
        .ok()
        .flatten();
        disk_verdict(free, min, dir)
    }

    pub fn media_path(&self) -> String {
        map_media_path(&self.job.req.media.path)
    }

    /// Marks progress the watchdog cannot see (bytes read, lines parsed).
    pub fn pulse(&self) {
        self.job.activity.fetch_add(1, Relaxed);
    }

    /// One ffmpeg run on a background slot: paused while preempted, killed when hung, past
    /// `RUN_TIMEOUT_MS` of unpaused time, or on cancel.
    pub async fn ffmpeg(&self, argv: &[String]) -> Result<(), String> {
        let cmd = Cmd::new("ffmpeg", &self.rt.ffmpeg, argv, Pipes::default());
        self.pipeline(u64::from(RUN_TIMEOUT_MS), vec![cmd], |p| async move {
            let p = &p[0];
            match p.wait().await.code {
                Some(0) => Ok(()),
                code => Err(format!("ffmpeg exited with {code:?}: {}", tail(p, 2))),
            }
        })
        .await?
    }

    /// Runs `cmds` as one unit on one background slot; `work` drives them, a watchdog verdict,
    /// the unpaused-time timeout or a cancel stops them all.
    pub async fn pipeline<T, F, Fut>(
        &self,
        timeout_ms: u64,
        cmds: Vec<Cmd>,
        work: F,
    ) -> Result<T, String>
    where
        F: FnOnce(Vec<Arc<Ffmpeg>>) -> Fut,
        Fut: Future<Output = T>,
    {
        let mut cancel = self.job.cancel.subscribe();
        let mut slot = tokio::select! {
            s = self.rt.slots.background(self.job.priority()) => s,
            _ = cancel.wait_for(|c| *c) => return Err(CANCELLED.into()),
        };
        let mut procs = vec![];
        for c in &cmds {
            let (bin, args) = background(&c.bin, &c.argv);
            let p = Ffmpeg::spawn_with(&bin, &args, c.pipes).await;
            procs.push(Arc::new(p.map_err(|e| format!("spawn {}: {e}", c.bin))?));
        }
        self.job.m.lock().unwrap().runs += 1;
        let paused = AtomicBool::new(false);
        let work = work(procs.clone());
        let verdict = {
            let watch = self.supervise(&procs, &mut slot, &mut cancel, timeout_ms, &paused);
            tokio::pin!(watch, work);
            tokio::select! {
                out = &mut work => Ok(out),
                reason = &mut watch => Err(reason),
            }
        };
        {
            let mut m = self.job.m.lock().unwrap();
            m.runs -= 1;
            m.paused_runs -= u32::from(paused.load(Relaxed));
        }
        let reason = match verdict {
            Ok(out) => return Ok(out),
            Err(r) => r,
        };
        for p in &procs {
            p.stop().await;
        }
        Err(if reason == CANCELLED {
            reason.into()
        } else {
            format!("{} {reason}", cmds[0].name)
        })
    }

    /// Applies the slot's pause verdict and returns why the runs must be stopped.
    async fn supervise(
        &self,
        procs: &[Arc<Ffmpeg>],
        slot: &mut BackgroundSlot,
        cancel: &mut watch::Receiver<bool>,
        timeout_ms: u64,
        paused_out: &AtomicBool,
    ) -> &'static str {
        let cap = self.rt.pause_capability();
        let lifetime = self.rt.current_config().lifetime;
        let (mut paused, mut active_ms, mut tick) = (false, 0u64, Instant::now());
        let (mut lines, mut cpu, mut last_progress) = (0, 0, now_ms());
        loop {
            tokio::select! {
                _ = slot.preempted.changed() => {}
                _ = cancel.changed() => {}
                _ = tokio::time::sleep(TICK) => {}
            }
            if !paused {
                active_ms += tick.elapsed().as_millis() as u64;
            }
            tick = Instant::now();
            if *cancel.borrow() {
                return CANCELLED;
            }
            let want = *slot.preempted.borrow();
            if want != paused {
                let mut sent = false;
                for p in procs {
                    sent |= p.pause(cap, want).await;
                }
                if sent {
                    paused = want;
                    paused_out.store(paused, Relaxed);
                    let mut m = self.job.m.lock().unwrap();
                    if paused {
                        m.paused_runs += 1;
                    } else {
                        m.paused_runs -= 1;
                        last_progress = now_ms();
                    }
                }
            }
            let l = procs.iter().map(|p| p.stderr_lines()).sum::<u64>()
                + self.job.activity.load(Relaxed);
            let c = procs
                .iter()
                .filter_map(|p| p.cpu_time_ms())
                .reduce(|a, b| a + b);
            let cpu_delta_ms = c.map_or(0, |c| c.saturating_sub(cpu));
            cpu = c.unwrap_or(cpu);
            if l != lines || cpu_delta_ms > 0 {
                lines = l;
                last_progress = now_ms();
            }
            let watch = JobWatchInput {
                now_ms: now_ms(),
                started_ms: 0,
                last_progress_ms: last_progress,
                paused,
                cpu_delta_ms,
                natural_end_ms: None,
            };
            if job_watch(&watch, &lifetime) != JobWatch::Ok {
                return "stalled";
            }
            if active_ms > timeout_ms {
                return "timed out";
            }
        }
    }
}

/// One process of a `Ctx::pipeline`; `name` labels its failures.
pub struct Cmd {
    pub name: &'static str,
    pub bin: String,
    pub argv: Vec<String>,
    pub pipes: Pipes,
}

impl Cmd {
    pub fn new(name: &'static str, bin: &str, argv: &[String], pipes: Pipes) -> Self {
        Self {
            name,
            bin: bin.into(),
            argv: argv.to_vec(),
            pipes,
        }
    }
}

/// The last `n` stderr lines of a run, on one line.
pub fn tail(p: &Ffmpeg, n: usize) -> String {
    let t = p.stderr_tail();
    t[t.len().saturating_sub(n)..].join(" ")
}

fn disk_verdict(free: Option<u64>, min: f64, dir: &Path) -> Result<(), String> {
    match free {
        Some(f) if min > 0.0 && (f as f64) < min => Err(format!(
            "not enough disk space: {f} bytes free under {}, jobMinFreeBytes is {min}",
            dir.display()
        )),
        _ => Ok(()),
    }
}

#[cfg(unix)]
#[allow(clippy::useless_conversion)]
fn free_bytes(p: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a valid C string and a zeroed out-struct.
    let ok = unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0;
    ok.then(|| u64::from(s.f_bavail) * u64::from(s.f_frsize))
}

#[cfg(windows)]
fn free_bytes(p: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let w: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
    let mut free = 0u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            w.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(free)
}

#[cfg(not(any(unix, windows)))]
fn free_bytes(_: &Path) -> Option<u64> {
    None
}

type S = State<Arc<AppState>>;

#[utoipa::path(post, path = "/v1/jobs", request_body = JobRequest, responses(
    (status = 200, body = JobAccepted, description = "queued, or the in-flight identical request"),
    (status = 400, description = "relative path or unusable request")))]
pub async fn create(State(s): S, Json(req): Json<JobRequest>) -> Response {
    match submit(&s.rt, req) {
        Ok(job_id) => Json(JobAccepted { job_id }).into_response(),
        Err(m) => (StatusCode::BAD_REQUEST, m).into_response(),
    }
}

#[utoipa::path(get, path = "/v1/jobs", responses((status = 200, body = Vec<JobSnapshot>)))]
pub async fn list(State(s): S) -> Json<Vec<JobSnapshot>> {
    Json(s.rt.jobs.snapshots())
}

#[utoipa::path(get, path = "/v1/jobs/{id}", responses(
    (status = 200, body = JobSnapshot), (status = 404, description = "unknown job")))]
pub async fn get_one(State(s): S, UrlPath(id): UrlPath<String>) -> Response {
    match s.rt.jobs.get(&id) {
        Some(j) => Json(j.snapshot()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[utoipa::path(delete, path = "/v1/jobs/{id}", responses(
    (status = 204, description = "cancelled (an ended job is left as is)"),
    (status = 404, description = "unknown job")))]
pub async fn delete(State(s): S, UrlPath(id): UrlPath<String>) -> StatusCode {
    if cancel(&s.rt, &id).await {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_check_fails_below_the_threshold_only() {
        let d = std::env::temp_dir();
        let free = free_bytes(&d).expect("free space of the temp dir");
        assert!(free > 0);
        assert!(disk_verdict(Some(free), 1.0, &d).is_ok());
        let e = disk_verdict(Some(10), 1e9, &d).unwrap_err();
        assert!(e.starts_with("not enough disk space: 10 bytes free"), "{e}");
        assert!(disk_verdict(Some(10), 0.0, &d).is_ok(), "0 disables");
        assert!(
            disk_verdict(None, 1e9, &d).is_ok(),
            "unknown is not a failure"
        );
    }
}
