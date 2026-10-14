//! Sessions and their ffmpeg jobs: open, heartbeat, seek and rung switch, HW fallback, throttle,
//! idle reaping, revocation and cache eviction. Spec: `docs/spec/runtime.md` sections 2-5.
use crate::cache::{Cache, remove_dir, valid_owner};
use crate::events::Events;
use crate::hw::Hw;
use crate::proc::{Exit, Ffmpeg};
use crate::resolve::Resolver;
use fliks_engine_core::api::{
    Decision, EngineConfig, EngineEvent, Hints, HostCaps, HwAccel, JobState, JobStats,
    MEDIA_INFO_SCHEMA_VERSION, OpenSessionRequest, OpenSessionResponse, PlayMethod, SessionKind,
    SessionSnapshot, StreamingConfig,
};
use fliks_engine_core::session::{
    self as core_session, CacheEntryInfo, CachePolicy, ExistingRun, JobIdleAction, JobIdleInput,
    JobIdleState, PauseCapability, ResolveInput, ResumeAction, RunProgress, SegmentName,
    SegmentSet, SessionPlan, SessionVariant, ThrottleDecision, ThrottleInput, TranscodeRun, Viewer,
    job_idle, live_session_expired, parse_segment_name, plan_gc, seconds_to_segment,
    segment_index_to_seconds, session_key, session_to_evict, tombstone_revivable,
};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use tokio::time::Instant;

const WATCH_EVERY: Duration = Duration::from_millis(200);
const THROTTLE_EVERY: Duration = Duration::from_secs(5);
const INIT_NON_EMPTY: Duration = Duration::from_millis(core_session::INIT_NON_EMPTY_WAIT_MS as u64);

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// M9 hook: the host's media path as this engine sees it.
// ponytail: identity until `EngineConfig.pathMappings` (remote engine, M9)
pub fn map_media_path(p: &str) -> String {
    p.to_string()
}

struct SessionMut {
    last_beat_ms: i64,
    position_s: f64,
    state: String,
    quality: Option<String>,
    last_requested_segment: Option<u32>,
    last_segment_at_ms: Option<i64>,
    served: Option<(String, HwAccel)>,
}

pub struct Session {
    pub sid: String,
    pub owner: Option<String>,
    pub media_file_id: u32,
    pub path: String,
    pub req: OpenSessionRequest,
    pub decision: Decision,
    pub streaming: StreamingConfig,
    pub config_revision: u32,
    pub plan: SessionPlan,
    /// Profile hash with the instance suffix of a concurrent playback.
    pub hash: String,
    pub opened_at_ms: i64,
    m: Mutex<SessionMut>,
}

impl Session {
    pub fn job_key(&self) -> String {
        session_key(self.media_file_id, self.owner.as_deref(), &self.hash)
    }

    pub fn pinned(&self) -> bool {
        self.req.pinned
    }

    pub fn position_s(&self) -> f64 {
        self.m.lock().unwrap().position_s
    }

    fn viewer(&self) -> Viewer {
        let m = self.m.lock().unwrap();
        Viewer {
            position: m.position_s,
            last_requested_segment: m.last_requested_segment,
            pinned: self.req.pinned,
        }
    }

    /// The resume floor of an init request: the job's start or the playhead's segment.
    /// [ts:modules/streaming/streaming.controller.ts:428-450]
    fn resume_floor(&self, job: Option<&Job>) -> u32 {
        let pos = seconds_to_segment(
            self.position_s(),
            self.plan.segment_duration,
            self.plan.source_fps,
        );
        pos.max(job.map_or(0, |j| j.start_segment))
    }
}

struct JobMut {
    sets: Vec<SegmentSet>,
    inits: Vec<bool>,
    /// The watcher listed the directories after the process exited: the sets are final.
    settled: bool,
    last_access_ms: i64,
    idle: JobIdleState,
    paused: bool,
    frontier_hint: Option<u32>,
    killed: bool,
}

/// One ffmpeg run of a session key (`docs/spec/runtime.md` section 2).
pub struct Job {
    pub key: String,
    pub hash: String,
    pub media_file_id: u32,
    pub owner: Option<String>,
    pub quality: String,
    pub dir: PathBuf,
    pub var_stream_map: bool,
    pub tracks: usize,
    pub ext: &'static str,
    pub start_segment: u32,
    pub requested_hw: HwAccel,
    pub hw: HwAccel,
    pub segment_duration: f64,
    pub fps: Option<f64>,
    pub duration: f64,
    pub restarts: u32,
    proc: Ffmpeg,
    m: Mutex<JobMut>,
    changed: watch::Sender<u64>,
}

impl Job {
    pub fn track_dir(&self, t: usize) -> PathBuf {
        if self.var_stream_map {
            self.dir.join(t.to_string())
        } else {
            self.dir.clone()
        }
    }

    pub fn init_path(&self, t: usize) -> PathBuf {
        let name = if self.var_stream_map {
            format!("init_{t}.mp4")
        } else {
            "init.mp4".into()
        };
        self.track_dir(t).join(name)
    }

    pub fn segment_path(&self, t: usize, n: u32) -> PathBuf {
        self.track_dir(t).join(format!("seg-{n:04}.{}", self.ext))
    }

    pub fn producing(&self) -> bool {
        self.proc.exited().is_none()
    }

    pub fn killed(&self) -> bool {
        self.m.lock().unwrap().killed
    }

    pub fn has(&self, t: usize, n: u32) -> bool {
        self.m
            .lock()
            .unwrap()
            .sets
            .get(t)
            .is_some_and(|s| s.contains(n))
    }

    fn touch(&self) {
        self.m.lock().unwrap().last_access_ms = now_ms();
    }

    /// Stops ffmpeg on purpose (seek, switch, idle, revoke) and waits for it.
    pub async fn kill(&self) -> Exit {
        self.m.lock().unwrap().killed = true;
        self.proc.stop().await
    }

    pub async fn pause(&self, cap: PauseCapability, pause: bool) -> bool {
        let sent = self.proc.pause(cap, pause).await;
        if sent {
            self.m.lock().unwrap().paused = pause;
        }
        sent
    }

    fn state(&self) -> JobState {
        let m = self.m.lock().unwrap();
        match self.proc.exited() {
            None if m.paused => JobState::Paused,
            None => JobState::Running,
            Some(_) if m.killed => JobState::Killed,
            Some(e) if e.code == Some(0) => JobState::Done,
            Some(_) => JobState::Failed,
        }
    }

    pub fn stats(&self) -> JobStats {
        let state = self.state();
        let m = self.m.lock().unwrap();
        let latest = m.sets.first().and_then(SegmentSet::latest);
        let percent = match (latest, self.duration > 0.0) {
            (Some(l), true) => {
                let done = segment_index_to_seconds(l + 1, self.segment_duration, self.fps);
                (done / self.duration * 100.0).min(100.0)
            }
            _ => 0.0,
        };
        JobStats {
            state,
            quality: self.quality.clone(),
            requested_hw: self.requested_hw,
            actual_hw: self.hw,
            start_segment: self.start_segment,
            frontier_segment: latest,
            transcode_percent: percent,
            throttled: m.paused,
            restarts: self.restarts,
            exit_code: self.proc.exited().and_then(|e| e.code),
            stderr_tail: self.proc.stderr_tail(),
        }
    }

    /// Waits until `pred` holds, the run is over (false) or the timeout (false).
    async fn wait_for(&self, timeout: Duration, pred: impl Fn(&JobMut) -> bool) -> bool {
        let mut rx = self.changed.subscribe();
        let deadline = Instant::now() + timeout;
        loop {
            {
                let m = self.m.lock().unwrap();
                if pred(&m) {
                    return true;
                }
                if m.settled {
                    return false;
                }
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => {}
                _ => return false,
            }
        }
    }

    pub async fn wait_segment(&self, t: usize, n: u32, timeout: Duration) -> bool {
        self.wait_for(timeout, |m| m.sets.get(t).is_some_and(|s| s.contains(n)))
            .await
    }

    /// The init landed and has bytes (ffmpeg writes it without a temp file).
    pub async fn wait_init(&self, t: usize, timeout: Duration) -> bool {
        if !self
            .wait_for(timeout, |m| m.inits.get(t).copied().unwrap_or(false))
            .await
        {
            return false;
        }
        non_empty_within(&self.init_path(t), INIT_NON_EMPTY).await
    }

    /// One listing of the run's directories; true when something changed.
    async fn scan(&self) -> bool {
        let mut sets = vec![];
        let mut inits = vec![];
        for t in 0..self.tracks {
            let mut set = SegmentSet::default();
            let mut init = false;
            if let Ok(mut rd) = tokio::fs::read_dir(self.track_dir(t)).await {
                while let Ok(Some(e)) = rd.next_entry().await {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    let ext_ok = name.ends_with(self.ext) || name.ends_with(".mp4");
                    match parse_segment_name(&name) {
                        Some(SegmentName::Segment(n)) if ext_ok => set.insert(n),
                        Some(SegmentName::Init(_)) => init = true,
                        _ => {}
                    }
                }
            }
            sets.push(set);
            inits.push(init);
        }
        let mut m = self.m.lock().unwrap();
        let changed = m.sets != sets || m.inits != inits;
        m.sets = sets;
        m.inits = inits;
        changed
    }

    fn existing_run(&self) -> ExistingRun {
        let exit = self.proc.exited();
        ExistingRun {
            exit_code: exit.and_then(|e| e.code),
            signal: exit.is_some_and(|e| e.signal),
            output_pending: false,
            start_segment: self.start_segment,
            remux: false,
            assembler: None,
        }
    }
}

pub async fn non_empty_within(p: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if tokio::fs::metadata(p).await.is_ok_and(|m| m.len() > 0) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[derive(Default)]
struct Reg {
    sessions: HashMap<String, Arc<Session>>,
    tombstones: HashMap<String, Arc<Session>>,
    jobs: HashMap<String, Arc<Job>>,
}

/// Why a request could not be served.
#[derive(Debug, Clone, PartialEq)]
pub enum OpenError {
    BadRequest(String),
    NoConfig,
    Detecting,
}

/// What a segment request found.
pub enum Fetch {
    Ready {
        path: PathBuf,
        job: Option<Arc<Job>>,
        waited_ms: f64,
        cache_hit: bool,
    },
    Retry,
    NotFound,
    Failed,
}

pub struct Runtime {
    pub config: Mutex<Option<EngineConfig>>,
    pub hw: Arc<Hw>,
    pub cache: Cache,
    pub events: Events,
    pub resolver: Resolver,
    pub ffmpeg: String,
    reg: Mutex<Reg>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Runtime {
    pub fn new(hw: Arc<Hw>, cache: Cache, ffmpeg: String) -> Arc<Self> {
        Arc::new(Self {
            config: Mutex::new(None),
            hw,
            cache,
            events: Events::default(),
            resolver: Resolver::default(),
            ffmpeg,
            reg: Mutex::new(Reg::default()),
            locks: Mutex::new(HashMap::new()),
        })
    }

    pub fn current_config(&self) -> EngineConfig {
        self.config.lock().unwrap().clone().unwrap_or_default()
    }

    fn lock_for(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(key.into())
            .or_default()
            .clone()
    }

    fn pause_capability(&self) -> PauseCapability {
        self.hw.snapshot().map_or(PauseCapability::None, |s| {
            PauseCapability::parse(&s.pause_capability)
        })
    }

    fn caps(&self) -> Option<HostCaps> {
        self.hw.snapshot().map(|s| s.host_caps())
    }

    // ---- sessions ----

    pub fn open(
        self: &Arc<Self>,
        req: OpenSessionRequest,
    ) -> Result<OpenSessionResponse, OpenError> {
        let config = self
            .config
            .lock()
            .unwrap()
            .clone()
            .ok_or(OpenError::NoConfig)?;
        let caps = self.caps().ok_or(OpenError::Detecting)?;
        if req.owner_id.as_deref().is_some_and(|o| !valid_owner(o)) {
            return Err(OpenError::BadRequest(
                "ownerId: [A-Za-z0-9_.-]{1,64}".into(),
            ));
        }
        if !Path::new(&req.decide.media.path).is_absolute() {
            return Err(OpenError::BadRequest("media.path must be absolute".into()));
        }
        let decision = fliks_engine_core::decide::decide(&req.decide, &caps, &config.streaming);
        let plan = core_session::plan(&req, &decision, &config.streaming, &caps);
        let sid = crate::random_hex(16);
        let now = now_ms();
        let lifetime = config.lifetime.clone();
        let owner = req.owner_id.clone();
        let file = req.decide.media.media_file_id;
        let mut evicted = None;
        let session = {
            let mut reg = self.reg.lock().unwrap();
            // [ts:modules/streaming/live-session.service.ts:409-432]
            let sibling = reg.sessions.values().any(|s| {
                s.owner == owner
                    && s.media_file_id == file
                    && s.plan.profile_hash == plan.profile_hash
                    && !live_session_expired(
                        s.m.lock().unwrap().last_beat_ms,
                        s.pinned(),
                        now,
                        &lifetime,
                    )
            });
            let hash = if sibling {
                let alnum: String = sid
                    .chars()
                    .filter(char::is_ascii_alphanumeric)
                    .take(8)
                    .collect();
                format!("{}~i{alnum}", plan.profile_hash)
            } else {
                plan.profile_hash.clone()
            };
            if owner.is_some() {
                let own: Vec<(String, i64)> = reg
                    .sessions
                    .values()
                    .filter(|s| s.owner == owner)
                    .map(|s| (s.sid.clone(), s.m.lock().unwrap().last_beat_ms))
                    .collect();
                let beats: Vec<i64> = own.iter().map(|o| o.1).collect();
                if let Some(i) = session_to_evict(&beats, &lifetime) {
                    reg.sessions.remove(&own[i].0);
                    evicted = Some(own[i].0.clone());
                }
            }
            let s = Arc::new(Session {
                sid: sid.clone(),
                owner: owner.clone(),
                media_file_id: file,
                path: map_media_path(&req.decide.media.path),
                decision: decision.clone(),
                streaming: config.streaming.clone(),
                config_revision: config.revision,
                hash,
                opened_at_ms: now,
                m: Mutex::new(SessionMut {
                    last_beat_ms: now,
                    position_s: req.start_at_s.unwrap_or(0.0),
                    state: "opened".into(),
                    quality: Some(decision.response.quality.clone()),
                    last_requested_segment: None,
                    last_segment_at_ms: None,
                    served: None,
                }),
                plan,
                req,
            });
            reg.sessions.insert(sid.clone(), s.clone());
            s
        };
        if let Some(e) = evicted {
            self.events.emit(EngineEvent::SessionEnded {
                session_id: e,
                reason: "evicted".into(),
            });
        }
        self.events.emit(EngineEvent::SessionOpened {
            session: Box::new(self.snapshot(&session)),
        });
        if decision.response.play_method == PlayMethod::Transcode {
            let q = session.req.decide.selection.requested_quality.clone();
            self.prewarm(&session, q.as_deref(), session.req.start_at_s);
        }
        let info = &session.req.decide.info;
        Ok(OpenSessionResponse {
            session_id: sid.clone(),
            profile_hash: (session.plan.kind != SessionKind::Directplay)
                .then(|| session.hash.clone()),
            play_path: core_session::play_path(&decision.response.play_url, &sid),
            hints: Hints {
                probe_stale: info
                    .schema_version
                    .is_none_or(|v| v < MEDIA_INFO_SCHEMA_VERSION),
                scan_missing: session.req.decide.scan.is_none(),
            },
            decision,
        })
    }

    /// Warms the pinned rung at the resume segment. [ts:modules/streaming/streaming.controller.ts:612-690]
    pub fn prewarm(
        self: &Arc<Self>,
        s: &Arc<Session>,
        start_quality: Option<&str>,
        start_at: Option<f64>,
    ) {
        let Some(q) = start_quality.filter(|q| !matches!(*q, "" | "auto" | "remux" | "original"))
        else {
            return;
        };
        if s.plan.kind != SessionKind::Transcode {
            return;
        }
        let v = s.req.decide.info.video.first();
        let w = v
            .and_then(|v| v.crop.as_ref().map(|c| c.width).or(v.width))
            .unwrap_or(1920);
        let h = v
            .and_then(|v| v.crop.as_ref().map(|c| c.height).or(v.height))
            .unwrap_or(1080);
        let quality = fliks_engine_core::hls::pinned_rung_name(q, w, h, s.decision.use_hdr_ladder);
        if !core_session::is_valid_quality(&quality) {
            return;
        }
        let start = seconds_to_segment(
            start_at.unwrap_or(0.0),
            s.plan.segment_duration,
            s.plan.source_fps,
        );
        let (rt, s) = (self.clone(), s.clone());
        tokio::spawn(async move {
            let key = s.job_key();
            if rt.job(&key).is_some_and(|j| j.producing()) {
                return;
            }
            let lock = rt.lock_for(&key);
            let _g = lock.lock().await;
            if rt.job(&key).is_none_or(|j| !j.producing()) {
                let hw = rt.detected_hw();
                rt.spawn_job(&s, &quality, start, (hw, hw), 0).await;
            }
        });
    }

    pub fn job(&self, key: &str) -> Option<Arc<Job>> {
        self.reg.lock().unwrap().jobs.get(key).cloned()
    }

    /// A live session, never a tombstone.
    pub fn peek(&self, sid: &str) -> Option<Arc<Session>> {
        self.reg.lock().unwrap().sessions.get(sid).cloned()
    }

    /// A live session, or a tombstone revived within the revive window.
    pub fn session(&self, sid: &str) -> Option<Arc<Session>> {
        let mut reg = self.reg.lock().unwrap();
        if let Some(s) = reg.sessions.get(sid) {
            return Some(s.clone());
        }
        let buried = reg.tombstones.remove(sid)?;
        let lifetime = self.current_config().lifetime;
        if !tombstone_revivable(buried.m.lock().unwrap().last_beat_ms, now_ms(), &lifetime) {
            return None;
        }
        tracing::info!(sid, "revived a session");
        reg.sessions.insert(sid.into(), buried.clone());
        Some(buried)
    }

    /// The owner's most recently active session of a file (requests without a sid).
    pub fn current_session(&self, owner: Option<&str>, file: u32) -> Option<Arc<Session>> {
        let reg = self.reg.lock().unwrap();
        reg.sessions
            .values()
            .filter(|s| s.owner.as_deref() == owner && s.media_file_id == file)
            .max_by_key(|s| s.m.lock().unwrap().last_beat_ms)
            .cloned()
    }

    pub fn touch(&self, s: &Session, segment: Option<u32>) {
        let mut m = s.m.lock().unwrap();
        m.last_beat_ms = now_ms();
        if let Some(n) = segment {
            m.last_requested_segment = Some(n);
            m.last_segment_at_ms = Some(m.last_beat_ms);
        }
    }

    pub fn heartbeat(&self, s: &Session, hb: &fliks_engine_core::api::HeartbeatRequest) {
        let mut m = s.m.lock().unwrap();
        m.last_beat_ms = now_ms();
        if let Some(p) = hb.position_s.filter(|p| p.is_finite()) {
            m.position_s = p;
        }
        if let Some(st) = &hb.state {
            m.state = st.clone();
        }
        if hb.quality.is_some() {
            m.quality = hb.quality.clone();
        }
    }

    /// Notes the rung and hardware a segment came from; a change is a `PipelineChanged`.
    pub fn note_served(&self, s: &Session, quality: &str, hw: HwAccel) {
        let changed = {
            let mut m = s.m.lock().unwrap();
            let now = (quality.to_string(), hw);
            let changed = m.served.as_ref() != Some(&now);
            m.served = Some(now);
            m.quality = Some(quality.into());
            changed
        };
        if changed {
            self.events.emit(EngineEvent::PipelineChanged {
                session_id: s.sid.clone(),
                quality: quality.into(),
                hw_accel: hw,
            });
        }
    }

    pub fn snapshot(&self, s: &Session) -> SessionSnapshot {
        let job = self.job(&s.job_key()).map(|j| j.stats());
        let m = s.m.lock().unwrap();
        SessionSnapshot {
            session_id: s.sid.clone(),
            owner_id: s.owner.clone(),
            media_file_id: s.media_file_id,
            profile_hash: (s.plan.kind != SessionKind::Directplay).then(|| s.hash.clone()),
            kind: s.plan.kind,
            quality: m.quality.clone(),
            client: s.req.client.clone(),
            tags: s.req.tags.clone(),
            opened_at_ms: s.opened_at_ms as f64,
            last_beat_ms: m.last_beat_ms as f64,
            last_segment_at_ms: m.last_segment_at_ms.map(|t| t as f64),
            position_s: m.position_s,
            state: m.state.clone(),
            pinned: s.req.pinned,
            config_revision: s.config_revision,
            decision: s.decision.clone(),
            job,
        }
    }

    pub fn list(&self, owner: Option<&str>, file: Option<u32>) -> Vec<SessionSnapshot> {
        let sessions: Vec<Arc<Session>> = {
            let reg = self.reg.lock().unwrap();
            reg.sessions.values().cloned().collect()
        };
        let mut out: Vec<SessionSnapshot> = sessions
            .iter()
            .filter(|s| owner.is_none_or(|o| s.owner.as_deref() == Some(o)))
            .filter(|s| file.is_none_or(|f| s.media_file_id == f))
            .map(|s| self.snapshot(s))
            .collect();
        out.sort_by(|a, b| a.opened_at_ms.total_cmp(&b.opened_at_ms));
        out
    }

    /// Idempotent; the job is left to the idle rule. [ts:modules/streaming/live-session.service.ts:553-558]
    pub fn stop(&self, sid: &str, reason: &str) -> bool {
        let known = {
            let mut reg = self.reg.lock().unwrap();
            let a = reg.sessions.remove(sid).is_some();
            reg.tombstones.remove(sid).is_some() || a
        };
        if known {
            self.events.emit(EngineEvent::SessionEnded {
                session_id: sid.into(),
                reason: reason.into(),
            });
        }
        known
    }

    /// Sessions matching every given filter go, with their jobs and resolver answers.
    pub async fn revoke(&self, owner: Option<&str>, files: &[u32]) -> u32 {
        let matches = |s: &Session| {
            owner.is_none_or(|o| s.owner.as_deref() == Some(o))
                && (files.is_empty() || files.contains(&s.media_file_id))
        };
        let (gone, jobs) = {
            let mut reg = self.reg.lock().unwrap();
            let gone: Vec<Arc<Session>> = reg
                .sessions
                .values()
                .filter(|s| matches(s))
                .cloned()
                .collect();
            reg.sessions.retain(|_, s| !matches(s));
            reg.tombstones.retain(|_, s| !matches(s));
            let keys: Vec<String> = gone.iter().map(|s| s.job_key()).collect();
            let jobs: Vec<Arc<Job>> = keys.iter().filter_map(|k| reg.jobs.remove(k)).collect();
            (gone, jobs)
        };
        self.resolver.purge(owner, files);
        for j in jobs {
            j.kill().await;
        }
        for s in &gone {
            self.events.emit(EngineEvent::SessionEnded {
                session_id: s.sid.clone(),
                reason: "revoked".into(),
            });
        }
        gone.len() as u32
    }

    /// Rescan or deletion: every job of the file dies, its cache goes; sessions stay.
    pub async fn invalidate(&self, file: u32) -> (u32, u32) {
        let jobs: Vec<Arc<Job>> = {
            let mut reg = self.reg.lock().unwrap();
            let keys: Vec<String> = reg
                .jobs
                .iter()
                .filter(|(_, j)| j.media_file_id == file)
                .map(|(k, _)| k.clone())
                .collect();
            keys.iter().filter_map(|k| reg.jobs.remove(k)).collect()
        };
        for j in &jobs {
            j.kill().await;
        }
        self.resolver.purge(None, &[file]);
        let entries = self.scan_cache().await;
        let mut removed = 0;
        for e in entries.iter().filter(|e| e.media_file_id == file) {
            let dir = e.dir.clone();
            let _ = tokio::task::spawn_blocking(move || remove_dir(&dir)).await;
            removed += 1;
        }
        let subs = self.cache.subtitles_dir(file);
        let _ = tokio::task::spawn_blocking(move || remove_dir(&subs)).await;
        (jobs.len() as u32, removed)
    }

    pub async fn pause_job(&self, s: &Session, pause: bool) -> bool {
        match self.job(&s.job_key()) {
            Some(j) => j.pause(self.pause_capability(), pause).await,
            None => false,
        }
    }

    // ---- jobs ----

    fn detected_hw(&self) -> HwAccel {
        self.caps().map_or(HwAccel::None, |c| c.hw_accel)
    }

    /// Kills the key's previous run, purges the rung from `start` and spawns.
    /// [ts:modules/streaming/transcoding/transcoding.service.ts:790-860]
    async fn spawn_job(
        self: &Arc<Self>,
        s: &Arc<Session>,
        quality: &str,
        start: u32,
        (hw, requested_hw): (HwAccel, HwAccel),
        restarts: u32,
    ) -> Option<Arc<Job>> {
        let key = s.job_key();
        if let Some(old) = self.job(&key)
            && old.producing()
        {
            old.kill().await;
        }
        let dir = self
            .cache
            .rung_dir(s.owner.as_deref(), s.media_file_id, &s.hash, quality);
        let var_stream_map = s.plan.multi_audio;
        let tracks = if var_stream_map {
            s.req.decide.info.audio.len() + 1
        } else {
            1
        };
        let ext = if s.plan.use_ts { "ts" } else { "m4s" };
        {
            let (dir, ext) = (dir.clone(), ext);
            let _ = tokio::task::spawn_blocking(move || {
                purge_from(&dir, var_stream_map, tracks, start, ext)
            })
            .await;
        }
        let caps = self.caps().unwrap_or_default();
        let platform = caps.platform;
        let dev = fliks_engine_core::hw::Device {
            platform,
            render_node: self.hw.snapshot().map(|h| h.render_node),
            opencl_device: std::env::var("FLIKS_OPENCL_DEVICE").ok(),
        };
        let dir_s = dir.to_string_lossy().into_owned();
        let build = |hw| {
            let args = core_session::vod_args(&core_session::RunInput {
                req: &s.req,
                decision: &s.decision,
                streaming: &s.streaming,
                plan: &s.plan,
                input_path: &s.path,
                output_dir: &dir_s,
                quality,
                start_segment: start,
                hw,
            });
            fliks_engine_core::args::vod(&args, &caps, &dev)
        };
        let (argv, hw) = match build(hw) {
            Ok(a) => (a, hw),
            Err(e) if hw != HwAccel::None => {
                tracing::warn!(key, "argv on {hw:?}: {e}; CPU instead");
                (build(HwAccel::None).ok()?, HwAccel::None)
            }
            Err(e) => {
                tracing::error!(key, "argv: {e}");
                return None;
            }
        };
        tracing::info!(key, quality, start, ?hw, "ffmpeg start");
        tracing::debug!(key, "ffmpeg {}", argv.join(" "));
        let proc = match Ffmpeg::spawn(&self.ffmpeg, &argv).await {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(key, "spawn {}: {e}", self.ffmpeg);
                return None;
            }
        };
        let job = Arc::new(Job {
            key: key.clone(),
            hash: s.hash.clone(),
            media_file_id: s.media_file_id,
            owner: s.owner.clone(),
            quality: quality.into(),
            dir,
            var_stream_map,
            tracks,
            ext,
            start_segment: start,
            requested_hw,
            hw,
            segment_duration: s.plan.segment_duration,
            fps: s.plan.source_fps,
            duration: s.plan.duration,
            restarts,
            proc,
            m: Mutex::new(JobMut {
                sets: vec![SegmentSet::default(); tracks],
                inits: vec![false; tracks],
                settled: false,
                last_access_ms: now_ms(),
                idle: JobIdleState::default(),
                paused: false,
                frontier_hint: None,
                killed: false,
            }),
            changed: watch::channel(0).0,
        });
        job.scan().await;
        self.reg
            .lock()
            .unwrap()
            .jobs
            .insert(key.clone(), job.clone());
        self.events.emit(EngineEvent::JobStarted {
            job_key: key,
            media_file_id: s.media_file_id,
            quality: quality.into(),
            start_segment: start,
            hw_accel: hw,
        });
        tokio::spawn(self.clone().watch(job.clone(), s.clone()));
        Some(job)
    }

    /// Follows the run's output until it exits, then falls back to the CPU when the hardware
    /// failed before the first segment. [ts:modules/streaming/transcoding/transcoding.service.ts:870-940]
    // A boxed `dyn Future`: the watcher respawns through `spawn_job`, which spawns a watcher.
    fn watch(
        self: Arc<Self>,
        job: Arc<Job>,
        s: Arc<Session>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            loop {
                if job.scan().await {
                    job.changed.send_modify(|v| *v += 1);
                }
                if job.proc.exited().is_some() {
                    break;
                }
                tokio::select! {
                    _ = tokio::time::sleep(WATCH_EVERY) => {}
                    _ = job.proc.wait() => {}
                }
            }
            job.scan().await;
            job.m.lock().unwrap().settled = true;
            job.changed.send_modify(|v| *v += 1);
            let exit = job.proc.exited().unwrap();
            let state = job.state();
            if state == JobState::Failed {
                tracing::warn!(
                    key = job.key,
                    ?exit,
                    "ffmpeg failed:\n{}",
                    job.proc.stderr_tail().join("\n")
                );
            }
            self.events.emit(EngineEvent::JobEnded {
                job_key: job.key.clone(),
                state,
                exit_code: exit.code,
            });
            let crashed_early = state == JobState::Failed && !job.has(0, job.start_segment);
            if !(crashed_early && job.hw != HwAccel::None) {
                return;
            }
            let lock = self.lock_for(&job.key);
            let _g = lock.lock().await;
            if !self.job(&job.key).is_some_and(|j| Arc::ptr_eq(&j, &job)) {
                return;
            }
            tracing::warn!(key = job.key, from = ?job.hw, "hardware run failed, CPU fallback");
            self.events.emit(EngineEvent::HwFallback {
                job_key: job.key.clone(),
                from: job.hw,
                to: HwAccel::None,
                stderr_tail: job.proc.stderr_tail(),
            });
            let dir = job.dir.clone();
            let _ = tokio::task::spawn_blocking(move || remove_dir(&dir)).await;
            let hw = (HwAccel::None, job.requested_hw);
            self.spawn_job(&s, &job.quality, job.start_segment, hw, job.restarts + 1)
                .await;
        })
    }

    /// The job that serves `quality` for a request at `requested`, spawning or restarting one as
    /// the seek/switch matrix says. [ts:modules/streaming/transcoding/transcoding.service.ts:743-812]
    async fn ensure_job(
        self: &Arc<Self>,
        s: &Arc<Session>,
        quality: &str,
        mut requested: u32,
    ) -> Option<Arc<Job>> {
        let key = s.job_key();
        let lock = self.lock_for(&key);
        let _g = lock.lock().await;
        let mut restarts = 0;
        let detected = self.detected_hw();
        let mut hw = (detected, detected);
        if let Some(e) = self.job(&key) {
            restarts = e.restarts + 1;
            // A run that fell back stays on the CPU.
            hw = (e.hw, e.requested_hw);
            let quality_match = e.quality == quality;
            if !quality_match && e.producing() {
                if requested == 0 && e.start_segment > 0 {
                    requested = e.start_segment;
                }
                e.kill().await;
            } else if !quality_match {
                // A finished run of another rung: start where this request is.
            } else {
                let input = ResolveInput {
                    run: e.existing_run(),
                    requested_segment: requested,
                    quality_match,
                    present: e.m.lock().unwrap().sets[0].clone(),
                };
                match core_session::resolve_existing(&input) {
                    ResumeAction::Serve | ResumeAction::Wait => {
                        e.touch();
                        return Some(e);
                    }
                    ResumeAction::Respawn(r) => {
                        if r.kill {
                            e.kill().await;
                        }
                        if r.purge_cache {
                            let dir = e.dir.clone();
                            let _ = tokio::task::spawn_blocking(move || remove_dir(&dir)).await;
                        }
                        requested = r.start_segment.unwrap_or(e.start_segment);
                    }
                }
            }
        }
        self.spawn_job(s, quality, requested, hw, restarts).await
    }

    /// A video file of `quality` (`docs/spec/runtime.md` section 4).
    pub async fn video_file(
        self: &Arc<Self>,
        s: &Arc<Session>,
        quality: &str,
        name: SegmentName,
    ) -> Fetch {
        let key = s.job_key();
        let existing = self.job(&key);
        let same = existing.as_ref().filter(|j| j.quality == quality).cloned();
        if let (Some(j), SegmentName::Segment(n)) = (&same, name)
            && j.has(0, n)
        {
            j.touch();
            return ready(j.segment_path(0, n), Some(j.clone()), 0.0, true);
        }
        if same.is_none() {
            // An earlier run of this rung left the file: serve it without spawning. [new]
            let dir = self
                .cache
                .rung_dir(s.owner.as_deref(), s.media_file_id, &s.hash, quality);
            let path = match name {
                SegmentName::Init(_) if s.plan.multi_audio => dir.join("0/init_0.mp4"),
                SegmentName::Init(_) => dir.join("init.mp4"),
                SegmentName::Segment(n) => {
                    let sub = if s.plan.multi_audio {
                        dir.join("0")
                    } else {
                        dir
                    };
                    sub.join(format!(
                        "seg-{n:04}.{}",
                        if s.plan.use_ts { "ts" } else { "m4s" }
                    ))
                }
            };
            if tokio::fs::metadata(&path).await.is_ok_and(|m| m.len() > 0) {
                return ready(path, existing, 0.0, true);
            }
        }
        let t0 = Instant::now();
        let (job, landed) = match name {
            SegmentName::Init(_) => {
                let job = match same {
                    Some(j) => j,
                    None => {
                        let floor = s.resume_floor(existing.as_deref());
                        match self.ensure_job(s, quality, floor).await {
                            Some(j) => j,
                            None => return Fetch::Failed,
                        }
                    }
                };
                let wait = Duration::from_millis(core_session::SEGMENT_WAIT_MS as u64);
                let ok = job.wait_init(0, wait).await;
                (job, ok)
            }
            SegmentName::Segment(n) => {
                let Some(job) = self.ensure_job(s, quality, n).await else {
                    return Fetch::Failed;
                };
                let wait = Duration::from_millis(core_session::SEGMENT_WAIT_MS as u64);
                let ok = job.wait_segment(0, n, wait).await;
                (job, ok)
            }
        };
        let path = match name {
            SegmentName::Init(_) => job.init_path(0),
            SegmentName::Segment(n) => job.segment_path(0, n),
        };
        finish(path, job, landed, t0).await
    }

    /// Audio rendition `track` (1-based var_stream_map index) of the session's job, whatever rung
    /// it runs; spawns the top rung when none runs. [ts:modules/streaming/streaming.controller.ts:1909-2120]
    pub async fn audio_file(
        self: &Arc<Self>,
        s: &Arc<Session>,
        track: usize,
        name: SegmentName,
    ) -> Fetch {
        let key = s.job_key();
        let existing = self.job(&key);
        let job = match existing
            .filter(|j| j.producing() || matches!(name, SegmentName::Segment(n) if j.has(track, n)))
        {
            Some(j) => j,
            None => {
                let v = s.req.decide.info.video.first();
                let (w, h) = (
                    v.and_then(|v| v.width).unwrap_or(1920),
                    v.and_then(|v| v.height).unwrap_or(1080),
                );
                let quality = core_session::top_rung(w, h, s.decision.use_hdr_ladder);
                let start = match name {
                    SegmentName::Init(_) => s.resume_floor(None),
                    SegmentName::Segment(n) => n,
                };
                match self.ensure_job(s, quality, start).await {
                    Some(j) => j,
                    None => return Fetch::Failed,
                }
            }
        };
        if track >= job.tracks {
            return Fetch::NotFound;
        }
        let t0 = Instant::now();
        let wait = Duration::from_millis(core_session::SEGMENT_WAIT_MS as u64);
        let (path, landed) = match name {
            SegmentName::Init(_) => (job.init_path(track), job.wait_init(track, wait).await),
            SegmentName::Segment(n) => (
                job.segment_path(track, n),
                job.wait_segment(track, n, wait).await,
            ),
        };
        finish(path, job, landed, t0).await
    }

    // ---- periodic work ----

    /// Session expiry and job reaping, every `liveSessionGcIntervalMs`.
    pub async fn gc_tick(&self) {
        let now = now_ms();
        let lifetime = self.current_config().lifetime;
        let (expired, reaped) = {
            let mut reg = self.reg.lock().unwrap();
            let expired: Vec<String> = reg
                .sessions
                .values()
                .filter(|s| {
                    live_session_expired(
                        s.m.lock().unwrap().last_beat_ms,
                        s.pinned(),
                        now,
                        &lifetime,
                    )
                })
                .map(|s| s.sid.clone())
                .collect();
            for sid in &expired {
                if let Some(s) = reg.sessions.remove(sid) {
                    reg.tombstones.insert(sid.clone(), s);
                }
            }
            reg.tombstones.retain(|_, s| {
                tombstone_revivable(s.m.lock().unwrap().last_beat_ms, now, &lifetime)
            });
            let mut reaped = vec![];
            let keys: Vec<String> = reg.jobs.keys().cloned().collect();
            for k in keys {
                let job = reg.jobs[&k].clone();
                let viewers = reg
                    .sessions
                    .values()
                    .filter(|s| {
                        s.media_file_id == job.media_file_id
                            && s.owner == job.owner
                            && s.hash == job.hash
                    })
                    .count() as u32;
                let input = {
                    let m = job.m.lock().unwrap();
                    JobIdleInput {
                        now_ms: now,
                        last_access_ms: m.last_access_ms,
                        has_base_hash: true,
                        viewers,
                        process_running: job.producing(),
                        state: m.idle,
                    }
                };
                let (state, action) = job_idle(&input, &lifetime);
                job.m.lock().unwrap().idle = state;
                if let JobIdleAction::Reap { kill } = action {
                    reg.jobs.remove(&k);
                    if kill {
                        reaped.push(job);
                    }
                }
            }
            (expired, reaped)
        };
        for sid in expired {
            self.events.emit(EngineEvent::SessionEnded {
                session_id: sid,
                reason: "expired".into(),
            });
        }
        for j in reaped {
            tracing::info!(key = j.key, "no viewer left, killing ffmpeg (cache kept)");
            j.kill().await;
        }
    }

    /// One throttle pass over the running jobs. [ts:modules/streaming/transcoding/ffmpeg-throttle.service.ts:94-150]
    pub async fn throttle_tick(&self) {
        let cap = self.pause_capability();
        let streaming = self.current_config().streaming;
        let work: Vec<(Arc<Job>, Vec<Viewer>)> = {
            let reg = self.reg.lock().unwrap();
            reg.jobs
                .values()
                .filter(|j| j.producing())
                .map(|j| {
                    let viewers = reg
                        .sessions
                        .values()
                        .filter(|s| {
                            s.media_file_id == j.media_file_id
                                && s.owner == j.owner
                                && s.hash == j.hash
                        })
                        .map(|s| s.viewer())
                        .collect();
                    (j.clone(), viewers)
                })
                .collect()
        };
        for (job, viewers) in work {
            let input = {
                let m = job.m.lock().unwrap();
                ThrottleInput {
                    capability: cap,
                    enabled: streaming.throttle_enabled,
                    threshold_seconds: streaming.throttle_threshold_seconds,
                    paused: m.paused,
                    process_exited: !job.producing(),
                    variant: Some(SessionVariant::Main),
                    viewers,
                    run: RunProgress::Transcode(TranscodeRun {
                        start_segment: job.start_segment,
                        frontier_hint: m.frontier_hint,
                        present: m.sets[0].clone(),
                        segment_duration: job.segment_duration,
                        fps: job.fps,
                    }),
                }
            };
            let out = core_session::throttle_step(&input);
            if out.frontier_segment.is_some() {
                job.m.lock().unwrap().frontier_hint = out.frontier_segment;
            }
            match out.decision {
                ThrottleDecision::Pause => {
                    job.pause(cap, true).await;
                }
                ThrottleDecision::Resume => {
                    job.pause(cap, false).await;
                }
                ThrottleDecision::Noop => {}
            }
        }
    }

    async fn scan_cache(&self) -> Vec<crate::cache::Scanned> {
        let root = self.cache.root.clone();
        tokio::task::spawn_blocking(move || Cache { root }.scan())
            .await
            .unwrap_or_default()
    }

    /// Entry dirs a job or session uses: never evicted.
    fn live_dirs(&self) -> Vec<PathBuf> {
        let reg = self.reg.lock().unwrap();
        let sessions = reg
            .sessions
            .values()
            .map(|s| (s.owner.clone(), s.media_file_id, s.hash.clone()));
        let jobs = reg
            .jobs
            .values()
            .map(|j| (j.owner.clone(), j.media_file_id, j.hash.clone()));
        sessions
            .chain(jobs)
            .map(|(o, f, h)| self.cache.entry_dir(o.as_deref(), f, &h))
            .collect()
    }

    pub async fn cache_summary(&self) -> fliks_engine_core::api::CacheSummary {
        let live = self.live_dirs();
        let entries: Vec<_> = self
            .scan_cache()
            .await
            .iter()
            .map(|e| e.entry(live.contains(&e.dir)))
            .collect();
        fliks_engine_core::api::CacheSummary {
            used_bytes: entries.iter().map(|e| e.bytes).sum(),
            entries,
        }
    }

    /// Removes the non-live entries matching the filters.
    pub async fn cache_delete(&self, file: Option<u32>, owner: Option<&str>) -> u32 {
        let live = self.live_dirs();
        let mut n = 0;
        for e in self.scan_cache().await {
            if live.contains(&e.dir)
                || file.is_some_and(|f| f != e.media_file_id)
                || owner.is_some_and(|o| e.owner.as_deref() != Some(o))
            {
                continue;
            }
            let _ = tokio::task::spawn_blocking(move || remove_dir(&e.dir)).await;
            n += 1;
        }
        n
    }

    /// TTL then LRU eviction over the rebuilt index. [ts:modules/streaming/transcoding/transcode-cache.service.ts:268-306]
    pub async fn cache_gc(&self) -> u32 {
        let live = self.live_dirs();
        let policy = CachePolicy::from_config(&self.current_config().streaming);
        let scanned = self.scan_cache().await;
        let index: Vec<CacheEntryInfo> = scanned
            .iter()
            .map(|e| CacheEntryInfo {
                key: e.dir.to_string_lossy().into_owned(),
                total_bytes: e.bytes,
                last_access_ms: e.last_access_ms,
                live: live.contains(&e.dir),
            })
            .collect();
        let gone = plan_gc(&index, now_ms(), &policy);
        for k in &gone {
            let dir = PathBuf::from(k);
            tracing::info!(dir = %dir.display(), "cache eviction");
            let _ = tokio::task::spawn_blocking(move || remove_dir(&dir)).await;
        }
        gone.len() as u32
    }

    /// Boot purge, then the GC, throttle and cache loops.
    pub fn start(self: &Arc<Self>) {
        let rt = self.clone();
        tokio::spawn(async move {
            let root = rt.cache.root.clone();
            let _ = tokio::task::spawn_blocking(move || Cache { root }.purge_old_layout()).await;
            loop {
                let ms = rt.current_config().lifetime.resolved().cache_gc_interval_ms;
                tokio::time::sleep(Duration::from_millis(ms.into())).await;
                rt.cache_gc().await;
            }
        });
        let rt = self.clone();
        tokio::spawn(async move {
            loop {
                let ms = rt
                    .current_config()
                    .lifetime
                    .resolved()
                    .live_session_gc_interval_ms;
                tokio::time::sleep(Duration::from_millis(ms.into())).await;
                rt.gc_tick().await;
            }
        });
        let rt = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(THROTTLE_EVERY).await;
                rt.throttle_tick().await;
            }
        });
    }

    /// Kills every job (engine shutdown).
    pub async fn shutdown(&self) {
        let jobs: Vec<Arc<Job>> = self
            .reg
            .lock()
            .unwrap()
            .jobs
            .drain()
            .map(|(_, j)| j)
            .collect();
        for j in jobs {
            j.kill().await;
        }
    }
}

fn ready(path: PathBuf, job: Option<Arc<Job>>, waited_ms: f64, cache_hit: bool) -> Fetch {
    Fetch::Ready {
        path,
        job,
        waited_ms,
        cache_hit,
    }
}

/// The 503/404 rule after the wait. [ts:modules/streaming/streaming.controller.ts:2506-2545]
async fn finish(path: PathBuf, job: Arc<Job>, landed: bool, t0: Instant) -> Fetch {
    let size = tokio::fs::metadata(&path).await.ok().map(|m| m.len());
    let probe = core_session::SegmentProbe {
        landed,
        size,
        producing: job.producing(),
        intentionally_killed: job.killed(),
    };
    match core_session::segment_response(&probe) {
        core_session::SegmentResponse::Serve => {
            job.touch();
            ready(path, Some(job), t0.elapsed().as_secs_f64() * 1000.0, false)
        }
        core_session::SegmentResponse::Unavailable { .. } => Fetch::Retry,
        core_session::SegmentResponse::NotFound => Fetch::NotFound,
    }
}

/// Drops a previous run's segments at and after `start`: one timeline forward of the restart.
/// [ts:modules/streaming/transcoding/segment-utils.ts:127-170]
fn purge_from(dir: &Path, var_stream_map: bool, tracks: usize, start: u32, ext: &str) {
    let dirs: Vec<PathBuf> = if var_stream_map {
        (0..tracks).map(|t| dir.join(t.to_string())).collect()
    } else {
        vec![dir.to_path_buf()]
    };
    for d in dirs {
        let _ = std::fs::create_dir_all(&d);
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let base = name.strip_suffix(".tmp").unwrap_or(&name);
            if let Some(SegmentName::Segment(n)) = parse_segment_name(base)
                && n >= start
                && base.ends_with(ext)
            {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purge_keeps_segments_before_the_start() {
        let d = std::env::temp_dir().join(format!("fliks-purge-{}", std::process::id()));
        std::fs::create_dir_all(d.join("0")).unwrap();
        for n in [
            "seg-0001.m4s",
            "seg-0005.m4s",
            "seg-0006.m4s.tmp",
            "init_0.mp4",
        ] {
            std::fs::write(d.join("0").join(n), b"x").unwrap();
        }
        purge_from(&d, true, 2, 5, "m4s");
        let mut left: Vec<String> = std::fs::read_dir(d.join("0"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["init_0.mp4", "seg-0001.m4s"]);
        assert!(d.join("1").is_dir());
        remove_dir(&d);
    }
}
