//! Opening a live session: validation, joining a shared upstream and starting its first run.

use super::{
    Live, OpenError, Outcome, PROBE_TIMEOUT, Pipe, PipeMut, SCHEMES, Viewer, recover, root,
    supervise, teardown, unregister,
};
use crate::cache::valid_owner;
use crate::lock::Locked;
use crate::runtime::{Runtime, now_ms};
use fliks_engine_core::api::{LiveMode, LiveProbe, OpenLiveRequest, OpenLiveResponse};
use fliks_engine_core::live::{self as core_live, Failover};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio::time::Instant;

fn validate(req: &OpenLiveRequest) -> Result<(), OpenError> {
    let bad = |m: &str| Err(OpenError::BadRequest(m.into()));
    if req.upstreams.is_empty() {
        return bad("upstreams: at least one");
    }
    if req.capacity_token.is_empty() {
        return bad("capacityToken: required");
    }
    if req.owner_id.as_deref().is_some_and(|o| !valid_owner(o)) {
        return bad("ownerId: [A-Za-z0-9_.-]{1,64}");
    }
    for u in &req.upstreams {
        let lower = u.url.to_ascii_lowercase();
        if !SCHEMES.iter().any(|s| lower.starts_with(s)) {
            return bad("upstreams[].url: http(s), rtmp, rtsp, rtp, udp or srt");
        }
        let crlf = |s: &str| s.contains(['\r', '\n']);
        if u.headers
            .iter()
            .any(|(k, v)| k.is_empty() || crlf(k) || crlf(v))
        {
            return bad("upstreams[].headers: no empty name, no CR/LF");
        }
    }
    Ok(())
}

fn known_probe(u: &fliks_engine_core::api::LiveUpstream) -> Option<LiveProbe> {
    (u.probed_video_codec.is_some() || u.probed_container.is_some()).then(|| LiveProbe {
        video_codec: u.probed_video_codec.clone(),
        audio_codec: u.probed_audio_codec.clone(),
        container: u.probed_container.clone(),
    })
}

pub(super) fn has_video(p: &Option<LiveProbe>) -> bool {
    p.as_ref().is_some_and(|p| p.video_codec.is_some())
}

/// `POST /v1/live`. [ts:svc:328-387]
pub async fn open(
    rt: &Arc<Runtime>,
    mut req: OpenLiveRequest,
) -> Result<OpenLiveResponse, OpenError> {
    let config = rt.config.locked().clone().ok_or(OpenError::NoConfig)?;
    let caps = rt.caps().ok_or(OpenError::Detecting)?;
    validate(&req)?;
    let mut order: Vec<usize> = (0..req.upstreams.len()).collect();
    order.sort_by_key(|i| req.upstreams[*i].priority);
    // Vetoed upstreams are never used; when all are, `start` answers 409 (a joined session needs none).
    if order.iter().any(|i| !req.upstreams[*i].at_capacity) {
        order.retain(|i| !req.upstreams[*i].at_capacity);
    }
    let mut probes: Vec<Option<LiveProbe>> = req.upstreams.iter().map(known_probe).collect();
    let fast_zap = config.livetv.fast_zap;
    loop {
        let snapped = core_live::snapped(&req.caps);
        let first = order[0];
        if !req.upstreams[first].at_capacity
            && core_live::decide_mode(&snapped, fast_zap, caps.hw_accel) == LiveMode::Direct
            && !has_video(&probes[first])
            && let Some(p) = probe(rt, &req.upstreams[first]).await
        {
            probes[first] = Some(p);
        }
        let (mode, planned) =
            core_live::plan(&req.caps, fast_zap, caps.hw_accel, probes[first].as_ref());
        let key = core_live::session_key(&req.channel_id, mode, &planned);
        match acquire(rt, &key, &req, mode, &planned, &order, &probes).await {
            Ok(pipe) => return Ok(join(rt, &pipe, &req, caps.hw_accel)),
            Err(OpenError::NotStreamable) => {
                tracing::info!(
                    channel = req.channel_id,
                    "upstream is a manifest, packaging it"
                );
                req.caps.direct_play = false;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Clears the in-flight start of `key` when the open is dropped before it ends; its waiters retry.
struct OpeningGuard<'a> {
    pub(super) live: &'a Live,
    pub(super) key: &'a str,
    pub(super) tx: Option<watch::Sender<Outcome>>,
}

impl Drop for OpeningGuard<'_> {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            self.live.opening.locked().remove(self.key);
            let _ = tx.send(Some(Ok(())));
        }
    }
}

/// A started pipe the open is dropped on (or that never got fed) is torn down like a failed start.
struct StartGuard {
    pub(super) rt: Arc<Runtime>,
    pub(super) pipe: Arc<Pipe>,
    armed: bool,
}

impl Drop for StartGuard {
    fn drop(&mut self) {
        if self.armed {
            unregister(&self.rt, &self.pipe);
            let (rt, pipe) = (self.rt.clone(), self.pipe.clone());
            tokio::spawn(async move { teardown(&rt, &pipe, "failed", true).await });
        }
    }
}

/// Tears a failed start down in a task of its own, so a dropped open cannot leave it half done.
async fn teardown_detached(rt: &Arc<Runtime>, pipe: &Arc<Pipe>, release: bool) {
    unregister(rt, pipe);
    let (rt, pipe) = (rt.clone(), pipe.clone());
    let _ = tokio::spawn(async move { teardown(&rt, &pipe, "failed", release).await }).await;
}

/// The running pipe of `key`, or the outcome of starting one; concurrent callers share a start.
/// [ts:svc:389-436]
async fn acquire(
    rt: &Arc<Runtime>,
    key: &str,
    req: &OpenLiveRequest,
    mode: LiveMode,
    caps: &fliks_engine_core::api::LiveCaps,
    order: &[usize],
    probes: &[Option<LiveProbe>],
) -> Result<Arc<Pipe>, OpenError> {
    loop {
        let found = {
            let mut opening = rt.live.opening.locked();
            if let Some(rx) = opening.get(key) {
                Err(rx.clone())
            } else if let Some(p) = rt.live.reg.locked().pipes.get(key) {
                return Ok(p.clone());
            } else {
                let (tx, rx) = watch::channel(None);
                opening.insert(key.into(), rx);
                Ok(tx)
            }
        };
        match found {
            Ok(tx) => {
                let mut guard = OpeningGuard {
                    live: &rt.live,
                    key,
                    tx: Some(tx),
                };
                let out = start(rt, key, req, mode, caps, order, probes).await;
                rt.live.opening.locked().remove(key);
                let tx = guard.tx.take().expect("armed");
                let _ = tx.send(Some(out.as_ref().map(|_| ()).map_err(Clone::clone)));
                return out;
            }
            Err(mut rx) => {
                let done = rx.wait_for(Option::is_some).await.map(|o| o.clone());
                if let Ok(Some(Err(e))) = done {
                    return Err(e);
                }
            }
        }
    }
}

/// A new pipe: capacity, probe, directory, then the first upstream that works. [ts:svc:441-547]
pub(super) async fn start(
    rt: &Arc<Runtime>,
    key: &str,
    req: &OpenLiveRequest,
    mode: LiveMode,
    caps: &fliks_engine_core::api::LiveCaps,
    order: &[usize],
    probes: &[Option<LiveProbe>],
) -> Result<Arc<Pipe>, OpenError> {
    if req.at_capacity || req.upstreams[order[0]].at_capacity {
        return Err(OpenError::AtCapacity);
    }
    let config = &rt.current_config().livetv;
    let mut probes = probes.to_vec();
    let first = order[0];
    if mode != LiveMode::Direct
        && !has_video(&probes[first])
        && let Some(p) = probe(rt, &req.upstreams[first]).await
    {
        probes[first] = Some(p);
    }
    let video0 = probes[first]
        .as_ref()
        .and_then(|p| p.video_codec.as_deref());
    let effective = core_live::effective_mode(mode, video0);
    let dir = (effective != LiveMode::Direct).then(|| {
        let safe: String = key
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let seq = rt.live.seq.fetch_add(1, Ordering::Relaxed);
        root(&rt.cache).join(format!("{safe}-{seq}"))
    });
    // Not awaited: a dropped open cannot leave the directory without its `StartGuard`.
    if let Some(d) = &dir
        && let Err(e) = std::fs::create_dir_all(d)
    {
        tracing::error!(dir = %d.display(), "live dir: {e}");
        return Err(OpenError::Unavailable);
    }
    let pipe = Arc::new(Pipe {
        key: key.into(),
        channel_id: req.channel_id.clone(),
        capacity_token: req.capacity_token.clone(),
        upstreams: req.upstreams.clone(),
        order: order.to_vec(),
        dir,
        segment_seconds: config.segment_seconds.max(1) as f64,
        window_minutes: config.timeshift_minutes.max(1) as f64,
        use_ts: caps.use_ts,
        bitrate: caps.max_bitrate_bps,
        probe_seconds: config.probe_seconds as f64,
        m: Mutex::new(PipeMut {
            mode: effective,
            active_codec: (effective == LiveMode::Remux)
                .then(|| video0.map(String::from))
                .flatten(),
            probes,
            failover: Failover::new(order.len()),
            current: 0,
            proc: None,
            slot: None,
            direct: None,
            content_type: None,
            tees: vec![],
            last_segment_at: Instant::now(),
            last_seg: None,
            viewers: HashMap::new(),
            idle_since: None,
            last_error: None,
            not_streamable: false,
            ended: false,
        }),
    });
    rt.live.reg.locked().pipes.insert(key.into(), pipe.clone());
    let mut guard = StartGuard {
        rt: rt.clone(),
        pipe: pipe.clone(),
        armed: true,
    };
    let fed = recover(rt, &pipe, false).await;
    guard.armed = false;
    if !fed {
        // A manifest is re-opened packaged under the same hold: not a release.
        if pipe.m.locked().not_streamable {
            teardown_detached(rt, &pipe, false).await;
            return Err(OpenError::NotStreamable);
        }
        teardown_detached(rt, &pipe, true).await;
        return Err(OpenError::Unavailable);
    }
    tokio::spawn(supervise(rt.clone(), pipe.clone()));
    Ok(pipe)
}

/// Registers the viewer and describes it. [ts:svc:371-386,788-804]
pub(super) fn join(
    rt: &Runtime,
    pipe: &Arc<Pipe>,
    req: &OpenLiveRequest,
    hw: fliks_engine_core::api::HwAccel,
) -> OpenLiveResponse {
    let sid = crate::random_hex(16);
    let now = now_ms();
    let mut m = pipe.m.locked();
    m.viewers.insert(
        sid.clone(),
        Viewer {
            owner: req.owner_id.clone(),
            tags: req.tags.clone(),
            opened_at_ms: now,
            last_seen_ms: now,
        },
    );
    m.idle_since = None;
    rt.live
        .reg
        .locked()
        .viewers
        .insert(sid.clone(), pipe.key.clone());
    let upstream = pipe.upstream_index(&m);
    let probe = m.probes[upstream].clone();
    let codec = m
        .active_codec
        .clone()
        .or_else(|| probe.as_ref().and_then(|p| p.video_codec.clone()));
    let direct = pipe.dir.is_none();
    OpenLiveResponse {
        play_path: format!(
            "/api/livetv/sessions/{sid}/{}",
            if direct { "direct.ts" } else { "index.m3u8" }
        ),
        session_id: sid,
        mode: m.mode,
        upstream: upstream as u32,
        capacity_token: pipe.capacity_token.clone(),
        probe,
        dvr_window_seconds: if direct {
            0.0
        } else {
            pipe.window_minutes * 60.0
        },
        segment_seconds: pipe.segment_seconds,
        qoe_pipeline: core_live::qoe_pipeline(m.mode, hw, codec.as_deref(), pipe.bitrate),
    }
}

/// ffprobe of one upstream under a playback slot; `None` when it could not tell. [ts:svc:746-773]
pub(super) async fn probe(
    rt: &Runtime,
    u: &fliks_engine_core::api::LiveUpstream,
) -> Option<LiveProbe> {
    if u.vetted
        && let Err(e) = super::vet::check(&u.url).await
    {
        tracing::warn!(url = u.url, "live probe refused: {e}");
        return None;
    }
    let _slot = rt.slots.playback(true);
    let mut cmd = tokio::process::Command::new(&rt.ffprobe);
    cmd.args(core_live::probe_args(&u.url, &u.headers))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let child = crate::proc::spawn(&mut cmd).await.ok()?;
    match tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) if out.status.success() => Some(core_live::parse_probe_output(
            &String::from_utf8_lossy(&out.stdout),
        )),
        _ => {
            tracing::warn!(url = u.url, "live probe failed");
            None
        }
    }
}
