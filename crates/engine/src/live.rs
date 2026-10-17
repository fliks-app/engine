//! Live TV sessions: one upstream per key shared by its viewers, ffmpeg's sliding-window HLS or a
//! raw tee, failover, stall watchdog, idle teardown. Spec: `docs/spec/live.md`; tags relative to
//! `backend/src/modules/livetv/` (`svc` = `services/livetv-session.service.ts`).
use crate::AppState;
use crate::cache::{Cache, remove_dir, valid_owner};
use crate::proc::Ffmpeg;
use crate::runtime::{Runtime, now_ms};
use crate::slots::PlaybackSlot;
use axum::{
    Json,
    body::Bytes,
    extract::{Path as UrlPath, Query, State},
    http::{Request, StatusCode, header},
    response::{IntoResponse, Response},
};
use fliks_engine_core::api::{
    EngineEvent, LiveError, LiveMode, LiveProbe, LiveSnapshot, OpenLiveRequest, OpenLiveResponse,
};
use fliks_engine_core::args::{self, LiveArgs};
use fliks_engine_core::live::{self as core_live, Failover};
use http_body_util::{BodyExt, Empty};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

const TICK: Duration = Duration::from_millis(200);
/// [ts:svc:138-141,148,169]
const FIRST_SEGMENT_TIMEOUT: Duration = Duration::from_secs(20);
const FIRST_SEGMENT_POLL: Duration = Duration::from_millis(300);
const STALL_TIMEOUT: Duration = Duration::from_secs(15);
const STALL_CHECK_EVERY: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const DIRECT_SNIFF: Duration = Duration::from_secs(4);
/// axios' `timeout` and `maxRedirects`. [ts:livetv-http.ts:109-111]
const DIRECT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 5;
/// ponytail: a chunk count stands in for the TS's 4 MiB high-water mark (~16 KiB chunks).
const TEE_CHUNKS: usize = 256;
const STARTUP_ERROR: &str = "no segment produced within the startup window";
/// ffmpeg also reads `file:`, `concat:`, `subfile:`...: never from a channel URL.
const SCHEMES: [&str; 11] = [
    "http://", "https://", "rtmp://", "rtmps://", "rtmpt://", "rtmpe://", "rtsp://", "rtsps://",
    "rtp://", "udp://", "srt://",
];

/// Why an open failed.
#[derive(Debug, Clone, PartialEq)]
pub enum OpenError {
    NoConfig,
    Detecting,
    BadRequest(String),
    AtCapacity,
    Unavailable,
    /// Every direct attempt met a manifest: the open retries packaged.
    NotStreamable,
}

type Outcome = Option<Result<(), OpenError>>;

pub struct Live {
    reg: Mutex<Reg>,
    /// Key -> the in-flight start, so concurrent opens share one attempt and its outcome.
    opening: Mutex<HashMap<String, watch::Receiver<Outcome>>>,
    seq: AtomicU64,
    client: Client<HttpsConnector<HttpConnector>, Empty<Bytes>>,
}

/// http and https with the platform's roots (`SSL_CERT_FILE`/`SSL_CERT_DIR` override them).
fn https() -> HttpsConnector<HttpConnector> {
    let tls = HttpsConnectorBuilder::new()
        .with_native_roots()
        .unwrap_or_else(|e| {
            tracing::warn!("no root certificates ({e}): https upstreams will fail");
            let roots = rustls::RootCertStore::empty();
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            HttpsConnectorBuilder::new().with_tls_config(config)
        });
    tls.https_or_http().enable_http1().build()
}

/// An error with its causes: hyper's own text hides a refused certificate.
fn causes(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        out = format!("{out}: {s}");
        src = s.source();
    }
    out
}

impl Default for Live {
    fn default() -> Self {
        Self {
            reg: Default::default(),
            opening: Default::default(),
            seq: AtomicU64::new(0),
            client: Client::builder(TokioExecutor::new()).build(https()),
        }
    }
}

#[derive(Default)]
struct Reg {
    pipes: HashMap<String, Arc<Pipe>>,
    /// Viewer sid -> its pipe's key.
    viewers: HashMap<String, String>,
}

struct Viewer {
    owner: Option<String>,
    tags: BTreeMap<String, String>,
    opened_at_ms: i64,
    last_seen_ms: i64,
}

struct Direct {
    reader: tokio::task::AbortHandle,
    down: Arc<AtomicBool>,
}

/// One upstream connection and what it feeds.
pub struct Pipe {
    key: String,
    channel_id: String,
    capacity_token: String,
    upstreams: Vec<fliks_engine_core::api::LiveUpstream>,
    /// Request positions in priority order; `Failover` walks positions of this list.
    order: Vec<usize>,
    /// `None` for direct (nothing on disk).
    dir: Option<PathBuf>,
    segment_seconds: f64,
    window_minutes: f64,
    use_ts: bool,
    bitrate: Option<f64>,
    probe_seconds: f64,
    m: Mutex<PipeMut>,
}

struct PipeMut {
    mode: LiveMode,
    /// Per request position.
    probes: Vec<Option<LiveProbe>>,
    failover: Failover,
    /// The position `failover` gave last.
    current: usize,
    proc: Option<Arc<Ffmpeg>>,
    slot: Option<PlaybackSlot>,
    direct: Option<Direct>,
    content_type: Option<String>,
    tees: Vec<(String, mpsc::Sender<Bytes>)>,
    /// The codec of the running fMP4 init (remux).
    active_codec: Option<String>,
    last_segment_at: Instant,
    last_seg: Option<u32>,
    viewers: HashMap<String, Viewer>,
    idle_since: Option<Instant>,
    last_error: Option<String>,
    not_streamable: bool,
    ended: bool,
}

impl Pipe {
    fn upstream_index(&self, m: &PipeMut) -> usize {
        self.order[m.current]
    }

    fn snapshot(&self, sid: &str, m: &PipeMut) -> Option<LiveSnapshot> {
        let v = m.viewers.get(sid)?;
        let probe = m.probes[self.upstream_index(m)].clone().unwrap_or_default();
        Some(LiveSnapshot {
            session_id: sid.into(),
            channel_id: self.channel_id.clone(),
            owner_id: v.owner.clone(),
            tags: v.tags.clone(),
            mode: m.mode,
            capacity_token: self.capacity_token.clone(),
            upstream: self.upstream_index(m) as u32,
            upstream_url: self.upstreams[self.upstream_index(m)].url.clone(),
            upstream_priority: self.upstreams[self.upstream_index(m)].priority,
            opened_at_ms: v.opened_at_ms as f64,
            last_seen_ms: v.last_seen_ms as f64,
            video_codec: m.active_codec.clone().or(probe.video_codec),
            audio_codec: probe.audio_codec,
            container: if self.dir.is_none() || self.use_ts {
                "mpegts"
            } else {
                "fmp4"
            }
            .into(),
            viewers_on_upstream: m.viewers.len() as u32,
            restarts: m.failover.restarts,
        })
    }

    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    pub fn owner_of(&self, sid: &str) -> Option<Option<String>> {
        self.m
            .lock()
            .unwrap()
            .viewers
            .get(sid)
            .map(|v| v.owner.clone())
    }

    /// A viewer's stream of the shared upstream (`direct.ts`) and its content type.
    pub fn attach(&self, sid: &str) -> (mpsc::Receiver<Bytes>, String) {
        let (tx, rx) = mpsc::channel(TEE_CHUNKS);
        let mut m = self.m.lock().unwrap();
        m.tees.push((sid.into(), tx));
        let ct = m
            .content_type
            .clone()
            .unwrap_or_else(|| "video/mp2t".into());
        (rx, ct)
    }
}

/// `<cache root>/live`, wiped at boot.
pub fn root(cache: &Cache) -> PathBuf {
    cache.root.join("live")
}

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

fn has_video(p: &Option<LiveProbe>) -> bool {
    p.as_ref().is_some_and(|p| p.video_codec.is_some())
}

/// `POST /v1/live`. [ts:svc:328-387]
pub async fn open(
    rt: &Arc<Runtime>,
    mut req: OpenLiveRequest,
) -> Result<OpenLiveResponse, OpenError> {
    let config = rt
        .config
        .lock()
        .unwrap()
        .clone()
        .ok_or(OpenError::NoConfig)?;
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
        if core_live::decide_mode(&snapped, fast_zap, caps.hw_accel) == LiveMode::Direct
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
            let mut opening = rt.live.opening.lock().unwrap();
            if let Some(rx) = opening.get(key) {
                Err(rx.clone())
            } else if let Some(p) = rt.live.reg.lock().unwrap().pipes.get(key) {
                return Ok(p.clone());
            } else {
                let (tx, rx) = watch::channel(None);
                opening.insert(key.into(), rx);
                Ok(tx)
            }
        };
        match found {
            Ok(tx) => {
                let out = start(rt, key, req, mode, caps, order, probes).await;
                rt.live.opening.lock().unwrap().remove(key);
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
async fn start(
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
    let config = rt.current_config().livetv;
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
    if let Some(d) = &dir
        && let Err(e) = tokio::fs::create_dir_all(d).await
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
    rt.live
        .reg
        .lock()
        .unwrap()
        .pipes
        .insert(key.into(), pipe.clone());
    if !recover(rt, &pipe, false).await {
        // A manifest is re-opened packaged under the same hold: not a release.
        if pipe.m.lock().unwrap().not_streamable {
            teardown(rt, &pipe, "failed", false).await;
            return Err(OpenError::NotStreamable);
        }
        teardown(rt, &pipe, "failed", true).await;
        return Err(OpenError::Unavailable);
    }
    tokio::spawn(supervise(rt.clone(), pipe.clone()));
    Ok(pipe)
}

/// Registers the viewer and describes it. [ts:svc:371-386,788-804]
fn join(
    rt: &Runtime,
    pipe: &Arc<Pipe>,
    req: &OpenLiveRequest,
    hw: fliks_engine_core::api::HwAccel,
) -> OpenLiveResponse {
    let sid = crate::random_hex(16);
    let now = now_ms();
    let mut m = pipe.m.lock().unwrap();
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
        .lock()
        .unwrap()
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
async fn probe(rt: &Runtime, u: &fliks_engine_core::api::LiveUpstream) -> Option<LiveProbe> {
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

/// Upstreams in order until one feeds the pipe; false once all are spent (the caller ends it).
/// [ts:svc:820-858]
async fn recover(rt: &Arc<Runtime>, pipe: &Arc<Pipe>, mut attempted_once: bool) -> bool {
    loop {
        let pos = {
            let mut m = pipe.m.lock().unwrap();
            if m.ended {
                return false;
            }
            let pos = m.failover.pick();
            if let Some(p) = pos {
                m.current = p;
            }
            pos
        };
        let Some(pos) = pos else { return false };
        let i = pipe.order[pos];
        let ok = if pipe.dir.is_none() {
            connect_direct(rt, pipe, i).await
        } else {
            spawn_packaged(rt, pipe, i, attempted_once).await
        };
        attempted_once = true;
        if ok {
            attempt(rt, pipe, i, None);
            return true;
        }
        stop_transport(pipe).await;
        let error = pipe.m.lock().unwrap().last_error.take();
        attempt(
            rt,
            pipe,
            i,
            Some(error.unwrap_or_else(|| STARTUP_ERROR.into())),
        );
        pipe.m.lock().unwrap().failover.failed(pos);
    }
}

fn attempt(rt: &Runtime, pipe: &Pipe, upstream: usize, error: Option<String>) {
    let probe = pipe.m.lock().unwrap().probes[upstream].clone();
    rt.events.emit(EngineEvent::LiveAttempt {
        capacity_token: pipe.capacity_token.clone(),
        upstream: upstream as u32,
        upstream_url: pipe.upstreams[upstream].url.clone(),
        upstream_priority: pipe.upstreams[upstream].priority,
        error,
        probe,
    });
}

/// One ffmpeg run into the pipe's directory, successful once a segment exists. [ts:svc:862-927]
async fn spawn_packaged(
    rt: &Arc<Runtime>,
    pipe: &Arc<Pipe>,
    i: usize,
    attempted_once: bool,
) -> bool {
    let u = &pipe.upstreams[i];
    let has = pipe.m.lock().unwrap().probes[i].clone();
    if !has_video(&has)
        && let Some(p) = probe(rt, u).await
    {
        pipe.m.lock().unwrap().probes[i] = Some(p);
    }
    let Some(caps) = rt.caps() else { return false };
    let dir = pipe.dir.clone().expect("packaged pipe");
    let (mode, video, audio, fresh) = {
        let mut m = pipe.m.lock().unwrap();
        let p = m.probes[i].clone().unwrap_or_default();
        let fresh = m.mode == LiveMode::Remux
            && core_live::forces_transcode(p.video_codec.as_deref(), m.active_codec.as_deref());
        if fresh {
            m.mode = LiveMode::Transcode;
        }
        (m.mode, p.video_codec, p.audio_codec, fresh)
    };
    let append = !fresh && attempted_once && dir.join("index.m3u8").exists();
    let (ua, referer, extra) = core_live::split_headers(&u.headers);
    let transcode = mode == LiveMode::Transcode;
    let hw_accel = if transcode {
        caps.hw_accel
    } else {
        fliks_engine_core::api::HwAccel::None
    };
    let argv = args::live(
        &LiveArgs {
            input_url: u.url.clone(),
            output_dir: dir.to_string_lossy().into_owned(),
            segment_seconds: pipe.segment_seconds,
            window_minutes: pipe.window_minutes,
            mode: if transcode {
                args::LiveMode::Transcode
            } else {
                args::LiveMode::Remux
            },
            use_ts: pipe.use_ts,
            user_agent: ua,
            referer,
            extra_headers: extra,
            hw_accel,
            video_bitrate_bps: pipe.bitrate,
            audio_codec: audio,
            append,
            probe_seconds: Some(pipe.probe_seconds),
            encode_threads: crate::slots::cpu_quota() as u32,
        },
        &caps,
        &rt.device(&caps),
    );
    let proc = match Ffmpeg::spawn(&rt.ffmpeg, &argv).await {
        Ok(p) => Arc::new(p),
        Err(e) => {
            pipe.m.lock().unwrap().last_error = Some(format!("spawn: {e}"));
            return false;
        }
    };
    {
        let mut m = pipe.m.lock().unwrap();
        if !m.ended {
            m.proc = Some(proc.clone());
            m.slot = Some(
                rt.slots
                    .playback(hw_accel == fliks_engine_core::api::HwAccel::None),
            );
            m.last_segment_at = Instant::now();
            m.last_seg = None;
        }
    }
    if pipe.m.lock().unwrap().ended {
        proc.stop().await;
        return false;
    }
    let ok = first_segment(&dir, &proc).await;
    if ok && mode == LiveMode::Remux {
        pipe.m.lock().unwrap().active_codec = video;
    }
    ok
}

/// Any `seg-*` entry within 20 s; the run exiting first fails. [ts:svc:1001-1047]
async fn first_segment(dir: &Path, proc: &Ffmpeg) -> bool {
    let until = Instant::now() + FIRST_SEGMENT_TIMEOUT;
    loop {
        if has_segment(dir).await {
            return true;
        }
        if proc.exited().is_some() || Instant::now() >= until {
            return false;
        }
        tokio::time::sleep(FIRST_SEGMENT_POLL).await;
    }
}

async fn has_segment(dir: &Path) -> bool {
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return false;
    };
    while let Ok(Some(e)) = rd.next_entry().await {
        if e.file_name().to_string_lossy().starts_with("seg-") {
            return true;
        }
    }
    false
}

async fn highest_segment(dir: &Path) -> Option<u32> {
    let mut rd = tokio::fs::read_dir(dir).await.ok()?;
    let mut top = None;
    while let Ok(Some(e)) = rd.next_entry().await {
        if let Some(n) = core_live::segment_number(&e.file_name().to_string_lossy()) {
            top = top.max(Some(n));
        }
    }
    top
}

/// A GET (http or https) following up to 5 redirects; `Err` is the reason to record.
async fn http_get(
    rt: &Runtime,
    u: &fliks_engine_core::api::LiveUpstream,
) -> Result<hyper::Response<hyper::body::Incoming>, String> {
    let (ua, _, _) = core_live::split_headers(&u.headers);
    let mut url = u.url.clone();
    for _ in 0..=MAX_REDIRECTS {
        let mut b = Request::get(&url).header(
            header::USER_AGENT,
            ua.as_deref()
                .filter(|v| !v.is_empty())
                .unwrap_or(args::LIVE_USER_AGENT),
        );
        for (k, v) in u
            .headers
            .iter()
            .filter(|(k, _)| !k.eq_ignore_ascii_case("user-agent"))
        {
            b = b.header(k, v);
        }
        let req = b.body(Empty::new()).map_err(|e| e.to_string())?;
        let res = rt.live.client.request(req).await.map_err(|e| causes(&e))?;
        let status = res.status();
        if status.is_redirection()
            && let Some(loc) = res
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
        {
            url = if loc.starts_with('/') {
                let uri: hyper::Uri = url
                    .parse()
                    .map_err(|e: hyper::http::uri::InvalidUri| e.to_string())?;
                let scheme = uri.scheme_str().unwrap_or("http");
                let host = uri.authority().map_or("", |a| a.as_str());
                format!("{scheme}://{host}{loc}")
            } else {
                loc.to_string()
            };
            continue;
        }
        if !status.is_success() {
            return Err(format!(
                "Request failed with status code {}",
                status.as_u16()
            ));
        }
        return Ok(res);
    }
    Err("Maximum number of redirects exceeded".into())
}

/// Opens the shared upstream of a direct pipe and tees it to the viewers. [ts:svc:929-999]
async fn connect_direct(rt: &Arc<Runtime>, pipe: &Arc<Pipe>, i: usize) -> bool {
    let u = &pipe.upstreams[i];
    let res = match tokio::time::timeout(DIRECT_CONNECT_TIMEOUT, http_get(rt, u)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            pipe.m.lock().unwrap().last_error = Some(e);
            return false;
        }
        Err(_) => {
            pipe.m.lock().unwrap().last_error = Some("timeout of 30000ms exceeded".into());
            return false;
        }
    };
    let content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("video/mp2t")
        .to_string();
    if core_live::is_manifest_content_type(&content_type) {
        pipe.m.lock().unwrap().not_streamable = true;
        return false;
    }
    let mut body = res.into_body();
    let first = match tokio::time::timeout(DIRECT_SNIFF, body.frame()).await {
        Err(_) => None,
        Ok(Some(Ok(f))) => f.into_data().ok(),
        Ok(_) => {
            pipe.m.lock().unwrap().last_error = Some("upstream closed before any data".into());
            return false;
        }
    };
    if first.as_deref().is_some_and(core_live::is_manifest_payload) {
        pipe.m.lock().unwrap().not_streamable = true;
        return false;
    }
    let down = Arc::new(AtomicBool::new(false));
    let (p, d) = (pipe.clone(), down.clone());
    let reader = tokio::spawn(async move {
        if let Some(c) = first {
            fan_out(&p, c);
        }
        while let Some(Ok(f)) = body.frame().await {
            if let Ok(c) = f.into_data() {
                fan_out(&p, c);
            }
        }
        d.store(true, Ordering::SeqCst);
    });
    let mut m = pipe.m.lock().unwrap();
    m.content_type = Some(content_type);
    m.last_segment_at = Instant::now();
    m.direct = Some(Direct {
        reader: reader.abort_handle(),
        down,
    });
    if m.ended {
        reader.abort();
        return false;
    }
    true
}

/// A viewer that cannot keep up is dropped, never the upstream. [ts:svc:970-984]
fn fan_out(pipe: &Pipe, chunk: Bytes) {
    let mut m = pipe.m.lock().unwrap();
    m.last_segment_at = Instant::now();
    m.tees.retain(|(_, tx)| tx.try_send(chunk.clone()).is_ok());
}

/// Stops the process or the connection feeding the pipe; viewers and tees stay. [ts:svc:1121-1133]
async fn stop_transport(pipe: &Pipe) {
    let (proc, slot, direct) = {
        let mut m = pipe.m.lock().unwrap();
        (m.proc.take(), m.slot.take(), m.direct.take())
    };
    if let Some(d) = direct {
        d.reader.abort();
    }
    if let Some(p) = proc {
        p.stop().await;
    }
    drop(slot);
}

/// The steady-state judge: new segments, exits, stalls, viewer expiry and idle teardown.
/// [ts:svc:281-302,1049-1076,1140-1145]
async fn supervise(rt: Arc<Runtime>, pipe: Arc<Pipe>) {
    let mut stall_check = Instant::now();
    loop {
        tokio::time::sleep(TICK).await;
        if pipe.m.lock().unwrap().ended {
            return;
        }
        let top = match &pipe.dir {
            Some(d) => highest_segment(d).await,
            None => None,
        };
        let config = rt.current_config();
        let ttl = config.lifetime.resolved().live_session_ttl_ms as i64;
        let idle = Duration::from_secs(config.livetv.channel_idle_seconds.max(0) as u64);
        let now = Instant::now();
        let (expired, idle_over, failure) = {
            let mut m = pipe.m.lock().unwrap();
            if top.is_some() && top > m.last_seg {
                m.last_seg = top;
                m.last_segment_at = now;
            }
            let cutoff = now_ms() - ttl;
            let flowing: Vec<String> = m
                .tees
                .iter()
                .filter(|(_, tx)| !tx.is_closed())
                .map(|(s, _)| s.clone())
                .collect();
            let expired: Vec<String> = m
                .viewers
                .iter()
                .filter(|(sid, v)| v.last_seen_ms <= cutoff && !flowing.contains(sid))
                .map(|(sid, _)| sid.clone())
                .collect();
            for sid in &expired {
                m.viewers.remove(sid);
            }
            let idle_over = if m.viewers.is_empty() {
                now.duration_since(*m.idle_since.get_or_insert(now)) >= idle
            } else {
                m.idle_since = None;
                false
            };
            let failure = if let Some(e) = m.proc.as_ref().and_then(|p| p.exited()) {
                let tail = m.proc.as_ref().unwrap().stderr_tail().join("\n");
                let start = tail.char_indices().rev().nth(299).map_or(0, |(i, _)| i);
                let code = e.code.map_or("null".into(), |c| c.to_string());
                Some(format!("exited (code={code}): {}", &tail[start..]))
            } else if m
                .direct
                .as_ref()
                .is_some_and(|d| d.down.load(Ordering::SeqCst))
            {
                Some("upstream closed".to_string())
            } else if now.duration_since(stall_check) >= STALL_CHECK_EVERY {
                stall_check = now;
                (now.duration_since(m.last_segment_at) > STALL_TIMEOUT)
                    .then(|| format!("no new segment for {} s", STALL_TIMEOUT.as_secs()))
            } else {
                None
            };
            (expired, idle_over, failure)
        };
        if !expired.is_empty() {
            let mut reg = rt.live.reg.lock().unwrap();
            for sid in &expired {
                reg.viewers.remove(sid);
            }
        }
        for sid in expired {
            rt.events.emit(EngineEvent::SessionEnded {
                session_id: sid,
                reason: "expired".into(),
            });
        }
        if idle_over {
            teardown(&rt, &pipe, "idle", true).await;
            return;
        }
        if let Some(error) = failure
            && !handle_failure(&rt, &pipe, error).await
        {
            return;
        }
    }
}

/// The running upstream failed: count it and recover in place. [ts:svc:1087-1102]
async fn handle_failure(rt: &Arc<Runtime>, pipe: &Arc<Pipe>, error: String) -> bool {
    tracing::warn!(key = pipe.key, "live upstream failed: {error}");
    stop_transport(pipe).await;
    let pos = pipe.m.lock().unwrap().current;
    attempt(rt, pipe, pipe.order[pos], Some(error));
    {
        let mut m = pipe.m.lock().unwrap();
        m.failover.failed(pos);
        m.last_seg = None;
    }
    if recover(rt, pipe, true).await {
        return true;
    }
    teardown(rt, pipe, "failed", true).await;
    false
}

/// Ends the pipe: transport, viewers, directory, then `LiveReleased` when `release`.
/// [ts:svc:1147-1164]
async fn teardown(rt: &Runtime, pipe: &Pipe, reason: &str, release: bool) {
    let viewers: Vec<String> = {
        let mut m = pipe.m.lock().unwrap();
        if m.ended {
            return;
        }
        m.ended = true;
        m.tees.clear();
        m.viewers.drain().map(|(sid, _)| sid).collect()
    };
    {
        let mut reg = rt.live.reg.lock().unwrap();
        if reg
            .pipes
            .get(&pipe.key)
            .is_some_and(|p| std::ptr::eq(&**p, pipe))
        {
            reg.pipes.remove(&pipe.key);
        }
        for sid in &viewers {
            reg.viewers.remove(sid);
        }
    }
    stop_transport(pipe).await;
    if let Some(d) = pipe.dir.clone() {
        let _ = tokio::task::spawn_blocking(move || remove_dir(&d)).await;
    }
    for sid in viewers {
        rt.events.emit(EngineEvent::SessionEnded {
            session_id: sid,
            reason: reason.into(),
        });
    }
    tracing::info!(key = pipe.key, reason, "live session ended");
    if release {
        rt.events.emit(EngineEvent::LiveReleased {
            capacity_token: pipe.capacity_token.clone(),
            reason: reason.into(),
        });
    }
}

/// Every pipe, at engine shutdown.
pub async fn shutdown(rt: &Arc<Runtime>) {
    let pipes: Vec<Arc<Pipe>> = rt
        .live
        .reg
        .lock()
        .unwrap()
        .pipes
        .values()
        .cloned()
        .collect();
    for p in pipes {
        teardown(rt, &p, "shutdown", true).await;
    }
}

impl Live {
    fn pipe_of(&self, sid: &str) -> Option<Arc<Pipe>> {
        let reg = self.reg.lock().unwrap();
        reg.pipes.get(reg.viewers.get(sid)?).cloned()
    }

    /// The viewer's pipe for a data-plane request, which keeps the viewer. [ts:svc:590-601]
    pub fn get_for_serve(&self, sid: &str) -> Option<Arc<Pipe>> {
        let pipe = self.pipe_of(sid)?;
        let mut m = pipe.m.lock().unwrap();
        let v = m.viewers.get_mut(sid)?;
        v.last_seen_ms = now_ms();
        drop(m);
        Some(pipe)
    }

    /// The viewer leaves (its direct streams close); false for a sid this registry never had.
    /// [ts:svc:549-559]
    pub fn leave(&self, rt: &Runtime, sid: &str, reason: &str) -> bool {
        let Some(pipe) = self.pipe_of(sid) else {
            return false;
        };
        self.reg.lock().unwrap().viewers.remove(sid);
        {
            let mut m = pipe.m.lock().unwrap();
            m.viewers.remove(sid);
            m.tees.retain(|(s, _)| s != sid);
        }
        rt.events.emit(EngineEvent::SessionEnded {
            session_id: sid.into(),
            reason: reason.into(),
        });
        true
    }

    /// Every viewer of `owner` leaves.
    pub fn revoke(&self, rt: &Runtime, owner: &str) -> u32 {
        let sids: Vec<String> = self
            .list(Some(owner))
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        sids.iter().filter(|s| self.leave(rt, s, "revoked")).count() as u32
    }

    pub fn list(&self, owner: Option<&str>) -> Vec<LiveSnapshot> {
        let pipes: Vec<Arc<Pipe>> = self.reg.lock().unwrap().pipes.values().cloned().collect();
        let mut out: Vec<LiveSnapshot> = pipes
            .iter()
            .flat_map(|p| {
                let m = p.m.lock().unwrap();
                m.viewers
                    .keys()
                    .filter_map(|sid| p.snapshot(sid, &m))
                    .collect::<Vec<_>>()
            })
            .filter(|s| owner.is_none_or(|o| s.owner_id.as_deref() == Some(o)))
            .collect();
        out.sort_by(|a, b| a.opened_at_ms.total_cmp(&b.opened_at_ms));
        out
    }

    pub fn snapshot(&self, sid: &str) -> Option<LiveSnapshot> {
        let p = self.pipe_of(sid)?;
        let m = p.m.lock().unwrap();
        p.snapshot(sid, &m)
    }
}

// ---- control endpoints ----

type S = State<Arc<AppState>>;

fn refused(status: StatusCode, code: &str) -> Response {
    (status, Json(LiveError { code: code.into() })).into_response()
}

#[utoipa::path(post, path = "/v1/live", operation_id = "open_live", request_body = OpenLiveRequest, responses(
    (status = 200, body = OpenLiveResponse),
    (status = 400, description = "no config set yet, or an invalid request"),
    (status = 409, body = LiveError, description = "`live_at_capacity`: a new upstream is needed and `atCapacity` is set"),
    (status = 503, body = LiveError, description = "`live_unavailable`: every upstream failed; or, without a body, hardware detection has not finished (Retry-After: 1)")))]
pub async fn open_handler(State(s): S, Json(req): Json<OpenLiveRequest>) -> Response {
    match open(&s.rt, req).await {
        Ok(r) => Json(r).into_response(),
        Err(OpenError::NoConfig) => (StatusCode::BAD_REQUEST, "no config set yet").into_response(),
        Err(OpenError::BadRequest(m)) => (StatusCode::BAD_REQUEST, m).into_response(),
        Err(OpenError::Detecting) => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "1")],
        )
            .into_response(),
        Err(OpenError::AtCapacity) => refused(StatusCode::CONFLICT, "live_at_capacity"),
        Err(OpenError::Unavailable | OpenError::NotStreamable) => {
            refused(StatusCode::SERVICE_UNAVAILABLE, "live_unavailable")
        }
    }
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[serde(rename_all = "camelCase")]
pub struct ListQuery {
    owner_id: Option<String>,
}

#[utoipa::path(get, path = "/v1/live", operation_id = "list_live", params(ListQuery),
    responses((status = 200, body = Vec<LiveSnapshot>)))]
pub async fn list_handler(State(s): S, Query(q): Query<ListQuery>) -> Json<Vec<LiveSnapshot>> {
    Json(s.rt.live.list(q.owner_id.as_deref()))
}

#[utoipa::path(get, path = "/v1/live/{sid}", operation_id = "get_live", responses(
    (status = 200, body = LiveSnapshot), (status = 404, description = "unknown or ended viewer")))]
pub async fn get_handler(State(s): S, UrlPath(sid): UrlPath<String>) -> Response {
    match s.rt.live.snapshot(&sid) {
        Some(snap) => Json(snap).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
