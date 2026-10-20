//! Sessions: the playback state a client holds, and the registry calls that open, find and end them.

use super::{Job, JobKind, OpenError, Runtime, kill_all, now_ms, take_orphans};
use crate::cache::{remove_dir, valid_owner};
use crate::lock::Locked;
use fliks_engine_core::api::{
    Decision, EngineEvent, Hints, HwAccel, MEDIA_INFO_SCHEMA_VERSION, OpenSessionRequest,
    OpenSessionResponse, PlayMethod, SessionKind, SessionSnapshot, StreamingConfig,
};
use fliks_engine_core::mp4::grid_segment_index;
use fliks_engine_core::session::{
    self as core_session, SessionPlan, SessionVariant, Viewer, live_session_expired,
    seconds_to_segment, session_key, session_to_evict, tombstone_revivable, variant_hash,
};
use std::path::Path;
use std::sync::{Arc, Mutex};

pub(super) struct SessionMut {
    pub(super) last_beat_ms: i64,
    position_s: f64,
    pub(super) state: String,
    pub(super) quality: Option<String>,
    pub(super) last_requested_segment: Option<u32>,
    last_segment_at_ms: Option<i64>,
    pub(super) served: Option<(String, HwAccel)>,
    /// What the client plays now: an ABR switch moves a DirectStream session to a rung.
    pub(super) kind: SessionKind,
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
    pub(super) m: Mutex<SessionMut>,
}

impl Session {
    pub(super) fn variant(&self, kind: JobKind) -> SessionVariant {
        match kind {
            JobKind::Main => SessionVariant::Main,
            JobKind::Early => SessionVariant::Early,
            JobKind::Remux => core_session::remux_variant_of(&self.req, &self.plan),
        }
    }

    /// Cache key of a run kind: the profile hash and its variant suffix.
    pub fn cache_hash(&self, kind: JobKind) -> String {
        variant_hash(&self.hash, &self.variant(kind))
    }

    pub fn key(&self, kind: JobKind) -> String {
        session_key(
            self.media_file_id,
            self.owner.as_deref(),
            &self.cache_hash(kind),
        )
    }

    pub fn job_key(&self) -> String {
        self.key(JobKind::Main)
    }

    pub fn served_kind(&self) -> SessionKind {
        self.m.locked().kind
    }

    /// The run snapshots and job commands follow: the remux one while the client plays it.
    pub fn primary_key(&self) -> String {
        self.key(if self.served_kind() == SessionKind::Remux {
            JobKind::Remux
        } else {
            JobKind::Main
        })
    }

    pub fn pinned(&self) -> bool {
        self.req.pinned
    }

    pub fn position_s(&self) -> f64 {
        self.m.locked().position_s
    }

    pub(super) fn viewer(&self) -> Viewer {
        let m = self.m.locked();
        Viewer {
            position: m.position_s,
            last_requested_segment: m.last_requested_segment,
            pinned: self.req.pinned,
        }
    }

    /// The resume floor of an init request: the job's start or the playhead's segment.
    /// [ts:modules/streaming/streaming.controller.ts:428-450]
    pub(super) fn resume_floor(&self, job: Option<&Job>) -> u32 {
        let pos = seconds_to_segment(
            self.position_s(),
            self.plan.segment_duration,
            self.plan.source_fps,
        );
        pos.max(job.map_or(0, |j| j.start_segment))
    }

    /// The resume floor of a remux run, on its keyframe grid when it has one.
    /// [ts:modules/streaming/streaming.controller.ts:428-450]
    pub(super) fn remux_floor(&self, job: Option<&Job>) -> u32 {
        let pos = match &self.plan.remux_grid {
            Some(g) => {
                grid_segment_index(&g.boundaries, self.position_s(), self.plan.timeline.origin)
                    as u32
            }
            None => seconds_to_segment(
                self.position_s(),
                self.plan.segment_duration,
                self.plan.source_fps,
            ),
        };
        pos.max(job.map_or(0, |j| j.start_segment))
    }
}

impl Runtime {
    pub fn open(
        self: &Arc<Self>,
        req: OpenSessionRequest,
    ) -> Result<OpenSessionResponse, OpenError> {
        let config = self.config.locked().clone().ok_or(OpenError::NoConfig)?;
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
        let (session, orphans) = {
            let mut reg = self.reg.locked();
            // [ts:modules/streaming/live-session.service.ts:409-432]
            let sibling = reg.sessions.values().any(|s| {
                s.owner == owner
                    && s.media_file_id == file
                    && s.plan.profile_hash == plan.profile_hash
                    && !live_session_expired(s.m.locked().last_beat_ms, s.pinned(), now, &lifetime)
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
                    .map(|s| (s.sid.clone(), s.m.locked().last_beat_ms))
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
                path: self.map_media_path(&req.decide.media.path),
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
                    kind: plan.kind,
                }),
                plan,
                req,
            });
            reg.sessions.insert(sid.clone(), s.clone());
            (s, take_orphans(&mut reg))
        };
        if !orphans.is_empty() {
            tokio::spawn(kill_all(orphans));
        }
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
        let report = core_session::open_report(
            &session.req.decide,
            &decision,
            &config.streaming,
            &caps,
            session.req.burn_in.as_ref().map(|b| b.kind),
        );
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
            playback_clock_offset_s: Some(report.playback_clock_offset_s),
            tonemap_algo: report.tonemap_algo,
            tonemap_curve: report.tonemap_curve,
            qoe_pipeline: Some(report.qoe_pipeline),
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
        let (w, h) = core_session::rung_source_size(v);
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
            // Players that fetch seg-0 on a load-then-seek get it from the early companion.
            if start > 0 && s.plan.probes_seg_zero {
                rt.early_job(&s, &quality).await;
            }
            let lock = rt.lock_for(&key);
            let _g = lock.lock().await;
            if rt.job(&key).is_none_or(|j| !j.producing()) {
                let hw = rt.detected_hw();
                rt.spawn_job(&s, &quality, start, (hw, hw), 0, JobKind::Main)
                    .await;
            }
        });
    }

    pub fn job(&self, key: &str) -> Option<Arc<Job>> {
        self.reg.locked().jobs.get(key).cloned()
    }

    /// A live session, never a tombstone.
    pub fn peek(&self, sid: &str) -> Option<Arc<Session>> {
        self.reg.locked().sessions.get(sid).cloned()
    }

    /// A live session, or a tombstone revived within the revive window.
    pub fn session(&self, sid: &str) -> Option<Arc<Session>> {
        let mut reg = self.reg.locked();
        if let Some(s) = reg.sessions.get(sid) {
            return Some(s.clone());
        }
        let buried = reg.tombstones.remove(sid)?;
        let lifetime = &self.current_config().lifetime;
        if !tombstone_revivable(buried.m.locked().last_beat_ms, now_ms(), lifetime) {
            return None;
        }
        tracing::info!(sid, "revived a session");
        reg.sessions.insert(sid.into(), buried.clone());
        Some(buried)
    }

    /// The owner's most recently active session of a file (requests without a sid).
    pub fn current_session(&self, owner: Option<&str>, file: u32) -> Option<Arc<Session>> {
        let reg = self.reg.locked();
        reg.sessions
            .values()
            .filter(|s| s.owner.as_deref() == owner && s.media_file_id == file)
            .max_by_key(|s| s.m.locked().last_beat_ms)
            .cloned()
    }

    pub fn touch(&self, s: &Session, segment: Option<u32>) {
        let mut m = s.m.locked();
        m.last_beat_ms = now_ms();
        if let Some(n) = segment {
            m.last_requested_segment = Some(n);
            m.last_segment_at_ms = Some(m.last_beat_ms);
        }
    }

    pub fn heartbeat(&self, s: &Session, hb: &fliks_engine_core::api::HeartbeatRequest) {
        let mut m = s.m.locked();
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

    /// A video request names what the client plays: the remux variant or a rung.
    /// [ts:modules/streaming/streaming.controller.ts:2274-2282]
    pub fn note_kind(&self, s: &Session, remux: bool) {
        s.m.locked().kind = if remux {
            SessionKind::Remux
        } else {
            SessionKind::Transcode
        };
    }

    /// Notes the rung and hardware a segment came from; a change is a `PipelineChanged`.
    pub fn note_served(&self, s: &Session, quality: &str, hw: HwAccel) {
        let changed = {
            let mut m = s.m.locked();
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
        let job = self.job(&s.primary_key()).map(|j| j.stats());
        let m = s.m.locked();
        SessionSnapshot {
            session_id: s.sid.clone(),
            owner_id: s.owner.clone(),
            media_file_id: s.media_file_id,
            profile_hash: (s.plan.kind != SessionKind::Directplay).then(|| s.hash.clone()),
            kind: m.kind,
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
            let reg = self.reg.locked();
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

    /// Idempotent; jobs no other session owns stop before it returns. [ts:modules/streaming/live-session.service.ts:553-558]
    pub async fn stop(&self, sid: &str, reason: &str) -> bool {
        let (known, orphans) = {
            let mut reg = self.reg.locked();
            let a = reg.sessions.remove(sid).is_some();
            let known = reg.tombstones.remove(sid).is_some() || a;
            (known, take_orphans(&mut reg))
        };
        if known {
            self.events.emit(EngineEvent::SessionEnded {
                session_id: sid.into(),
                reason: reason.into(),
            });
        }
        kill_all(orphans).await;
        known
    }

    /// Sessions matching every given filter go, with their jobs and resolver answers.
    pub async fn revoke(&self, owner: Option<&str>, files: &[u32]) -> u32 {
        let matches = |s: &Session| {
            owner.is_none_or(|o| s.owner.as_deref() == Some(o))
                && (files.is_empty() || files.contains(&s.media_file_id))
        };
        let (gone, orphans) = {
            let mut reg = self.reg.locked();
            let gone: Vec<Arc<Session>> = reg
                .sessions
                .values()
                .filter(|s| matches(s))
                .cloned()
                .collect();
            reg.sessions.retain(|_, s| !matches(s));
            reg.tombstones.retain(|_, s| !matches(s));
            (gone, take_orphans(&mut reg))
        };
        self.resolver.purge(owner, files);
        kill_all(orphans).await;
        for s in &gone {
            self.events.emit(EngineEvent::SessionEnded {
                session_id: s.sid.clone(),
                reason: "revoked".into(),
            });
        }
        // Live viewers belong to no media file: only an owner-wide revoke reaches them.
        let live = match owner {
            Some(o) if files.is_empty() => self.live.revoke(self, o),
            _ => 0,
        };
        gone.len() as u32 + live
    }

    /// Rescan or deletion: every job of the file dies, its cache and sprite go; sessions stay.
    pub async fn invalidate(self: &Arc<Self>, file: u32) -> (u32, u32) {
        let background = crate::jobs::cancel_file(self, file).await;
        let jobs: Vec<Arc<Job>> = {
            let mut reg = self.reg.locked();
            let keys: Vec<String> = reg
                .jobs
                .iter()
                .filter(|(_, j)| j.media_file_id == file)
                .map(|(k, _)| k.clone())
                .collect();
            keys.iter().filter_map(|k| reg.jobs.remove(k)).collect()
        };
        kill_all(jobs.clone()).await;
        self.resolver.purge(None, &[file]);
        let entries = self.scan_cache().await;
        let mut removed = 0;
        for e in entries.iter().filter(|e| e.media_file_id == file) {
            let dir = e.dir.clone();
            let _ = tokio::task::spawn_blocking(move || remove_dir(&dir)).await;
            removed += 1;
        }
        let dirs = [self.cache.subtitles_dir(file), self.cache.sprite_dir(file)];
        let _ = tokio::task::spawn_blocking(move || dirs.iter().for_each(|d| remove_dir(d))).await;
        (jobs.len() as u32 + background, removed)
    }

    pub async fn pause_job(&self, s: &Session, pause: bool) -> bool {
        match self.job(&s.primary_key()) {
            Some(j) => j.pause(self.pause_capability(), pause).await,
            None => false,
        }
    }
}
