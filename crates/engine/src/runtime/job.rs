//! A running ffmpeg or remux run and the files it has produced.

use super::{INIT_NON_EMPTY, JobKind, Session, now_ms};
use crate::lock::Locked;
use crate::proc::{Exit, Ffmpeg};
use crate::remux::Remux;
use crate::slots::PlaybackSlot;
use fliks_engine_core::api::{HwAccel, JobState, JobStats};
use fliks_engine_core::session::{
    ExistingRun, PauseCapability, SegmentName, SegmentSet, parse_segment_name,
    segment_index_to_seconds,
};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

pub(super) struct JobMut {
    pub(super) sets: Vec<SegmentSet>,
    pub(super) inits: Vec<bool>,
    /// The watcher listed the directories after the process exited: the sets are final.
    pub(super) settled: bool,
    pub(super) paused: bool,
    pub(super) frontier_hint: Option<u32>,
    pub(super) killed: bool,
    /// A remux run's assembler still builds segments after the process exited.
    pub(super) pending: bool,
    /// Watchdog clocks; a resume restarts both.
    pub(super) started_ms: i64,
    pub(super) last_progress_ms: i64,
    pub(super) seen_lines: u64,
    pub(super) tmp_bytes: u64,
    pub(super) cpu_ms: u64,
    pub(super) reason: Option<&'static str>,
}

/// One ffmpeg run of a session key (`docs/spec/runtime.md` section 2).
pub struct Job {
    pub key: String,
    pub kind: JobKind,
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
    /// Media time a run with a natural end produces (the early companion's `-to`), in ms.
    pub(super) natural_end_ms: Option<i64>,
    pub(super) proc: Ffmpeg,
    pub(super) remux: Option<Remux>,
    /// Held while the process lives; lent back while throttled.
    pub(super) slot: Mutex<Option<PlaybackSlot>>,
    pub(super) m: Mutex<JobMut>,
    pub(super) changed: watch::Sender<u64>,
}

impl Job {
    /// Track `t`'s own directory: var_stream_map's `<t>/`, a remux run's audio `<t>/`.
    fn split(&self, t: usize) -> bool {
        self.var_stream_map || (self.kind == JobKind::Remux && t > 0)
    }

    pub fn track_dir(&self, t: usize) -> PathBuf {
        if self.split(t) {
            self.dir.join(t.to_string())
        } else {
            self.dir.clone()
        }
    }

    /// The init or segment file of track `t`.
    pub fn file_path(&self, t: usize, name: SegmentName) -> PathBuf {
        layout_path(&self.dir, self.split(t), t, self.ext, name)
    }

    /// The process lives or its output is still being assembled.
    pub fn producing(&self) -> bool {
        self.proc.exited().is_none() || self.m.locked().pending
    }

    pub fn killed(&self) -> bool {
        self.m.locked().killed
    }

    pub fn has(&self, t: usize, n: u32) -> bool {
        self.m.locked().sets.get(t).is_some_and(|s| s.contains(n))
    }

    pub(super) fn serves(&self, s: &Session) -> bool {
        s.media_file_id == self.media_file_id && s.owner == self.owner && s.hash == self.hash
    }

    /// Stops ffmpeg on purpose (idle, revoke), waits for it and frees its slot.
    pub async fn kill(&self) -> Exit {
        self.stop_with(false).await
    }

    /// SIGKILLs a run whose output is about to be replaced (seek, switch): `temp_file` keeps a
    /// half-written segment out of the final names.
    pub async fn kill_now(&self) -> Exit {
        self.stop_with(true).await
    }

    async fn stop_with(&self, hard: bool) -> Exit {
        self.m.locked().killed = true;
        let exit = if hard {
            self.proc.kill().await
        } else {
            self.proc.stop().await
        };
        self.slot.locked().take();
        exit
    }

    pub async fn pause(&self, cap: PauseCapability, pause: bool) -> bool {
        let sent = self.proc.pause(cap, pause).await;
        if sent {
            {
                let mut m = self.m.locked();
                m.paused = pause;
                if !pause {
                    m.started_ms = now_ms();
                    m.last_progress_ms = m.started_ms;
                }
            }
            if let Some(r) = &self.remux {
                r.view.pause(pause);
            }
            if let Some(s) = &*self.slot.locked() {
                s.set_idle(pause);
            }
        }
        sent
    }

    pub(super) fn state(&self) -> JobState {
        let m = self.m.locked();
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
        let m = self.m.locked();
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
                let m = self.m.locked();
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
        non_empty_within(&self.file_path(t, SegmentName::Init(None)), INIT_NON_EMPTY).await
    }

    /// One listing of the run's directories; true when something changed.
    pub(super) async fn scan(&self) -> bool {
        let (dirs, ext) = (
            (0..self.tracks).map(|t| self.track_dir(t)).collect(),
            self.ext,
        );
        // One blocking hop for the whole listing, not one per batch of entries.
        let Ok((sets, inits, tmp_bytes)) =
            tokio::task::spawn_blocking(move || list_tracks(dirs, ext)).await
        else {
            return false;
        };
        let mut m = self.m.locked();
        let changed = m.sets != sets || m.inits != inits;
        if changed || m.tmp_bytes != tmp_bytes {
            m.last_progress_ms = now_ms();
        }
        m.sets = sets;
        m.inits = inits;
        m.tmp_bytes = tmp_bytes;
        changed
    }

    /// `audio`: the 1-based rendition a remux request is for.
    pub(super) fn existing_run(&self, audio: Option<usize>) -> ExistingRun {
        let exit = self.proc.exited();
        ExistingRun {
            exit_code: exit.and_then(|e| e.code),
            signal: exit.is_some_and(|e| e.signal),
            output_pending: self.m.locked().pending,
            start_segment: self.start_segment,
            remux: self.remux.is_some(),
            assembler: self.remux.as_ref().map(|r| r.view.progress(audio)),
        }
    }

    /// Waits until the watcher saw the run end and its output was assembled.
    pub(super) async fn settled(&self) {
        self.wait_for(Duration::from_secs(3600), |_| false).await;
    }
}

/// Segments, init presence and in-flight bytes of each track directory of a run.
fn list_tracks(dirs: Vec<PathBuf>, ext: &str) -> (Vec<SegmentSet>, Vec<bool>, u64) {
    let (mut sets, mut inits, mut tmp_bytes) = (vec![], vec![], 0);
    for dir in dirs {
        let mut set = SegmentSet::default();
        let mut init = false;
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.contains(".tmp") {
                tmp_bytes += e.metadata().map_or(0, |m| m.len());
            }
            let ext_ok = name.ends_with(ext) || name.ends_with(".mp4");
            match parse_segment_name(&name) {
                Some(SegmentName::Segment(n)) if ext_ok => set.insert(n),
                Some(SegmentName::Init(_)) => init = true,
                _ => {}
            }
        }
        sets.push(set);
        inits.push(init);
    }
    (sets, inits, tmp_bytes)
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

/// A run's file under the rung directory `dir`. A split layout keeps each track in `<t>/` and
/// names its init `init_<t>.mp4`.
pub(super) fn layout_path(
    dir: &Path,
    split: bool,
    t: usize,
    ext: &str,
    name: SegmentName,
) -> PathBuf {
    let dir = if split {
        dir.join(t.to_string())
    } else {
        dir.to_path_buf()
    };
    match name {
        SegmentName::Init(_) if split => dir.join(format!("init_{t}.mp4")),
        SegmentName::Init(_) => dir.join("init.mp4"),
        SegmentName::Segment(n) => dir.join(format!("seg-{n:04}.{ext}")),
    }
}
