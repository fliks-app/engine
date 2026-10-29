//! Live TV sessions: one upstream per key shared by its viewers, ffmpeg's sliding-window HLS or a
//! raw tee, failover, stall watchdog, idle teardown. Spec: `docs/spec/live.md`; tags relative to
//! `backend/src/modules/livetv/` (`svc` = `services/livetv-session.service.ts`).

mod open;
mod proxy;
mod supervise;
mod upstream;
mod vet;

pub use self::open::open;
pub use self::supervise::shutdown;

use self::open::{has_video, probe, vetted_proxy};
use self::proxy::VetProxy;
use self::supervise::{supervise, teardown, unregister};
use self::upstream::{attempt, highest_segment, https, recover, stop_transport};
use self::vet::Resolver;
use crate::AppState;
use crate::cache::Cache;
use crate::lock::Locked;
use crate::proc::Ffmpeg;
use crate::runtime::{Runtime, now_ms};
use crate::slots::PlaybackSlot;
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use fliks_engine_core::api::{
    EngineEvent, LiveError, LiveMode, LiveProbe, LiveSnapshot, OpenLiveRequest, OpenLiveResponse,
};
use fliks_engine_core::live::Failover;
use http_body_util::Empty;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64};
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
    client: Client<HttpsConnector<HttpConnector<Resolver>>, Empty<Bytes>>,
    /// Its resolver refuses internal addresses; used for `vetted` upstreams.
    vetted_client: Client<HttpsConnector<HttpConnector<Resolver>>, Empty<Bytes>>,
    /// Started with the first vetted ffmpeg/ffprobe input.
    proxy: tokio::sync::OnceCell<VetProxy>,
}

impl Live {
    /// The proxy a vetted ffmpeg/ffprobe input fetches through.
    async fn vet_proxy(&self) -> Result<&str, String> {
        self.proxy
            .get_or_try_init(|| VetProxy::start(vet::is_internal))
            .await
            .map(VetProxy::url)
            .map_err(|e| format!("vetting proxy: {e}"))
    }
}

impl Default for Live {
    fn default() -> Self {
        Self {
            reg: Default::default(),
            opening: Default::default(),
            seq: AtomicU64::new(0),
            client: Client::builder(TokioExecutor::new()).build(https(false)),
            vetted_client: Client::builder(TokioExecutor::new()).build(https(true)),
            proxy: Default::default(),
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
        self.m.locked().viewers.get(sid).map(|v| v.owner.clone())
    }

    /// A viewer's stream of the shared upstream (`direct.ts`) and its content type.
    pub fn attach(&self, sid: &str) -> (mpsc::Receiver<Bytes>, String) {
        let (tx, rx) = mpsc::channel(TEE_CHUNKS);
        let mut m = self.m.locked();
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

impl Live {
    fn pipe_of(&self, sid: &str) -> Option<Arc<Pipe>> {
        let reg = self.reg.locked();
        reg.pipes.get(reg.viewers.get(sid)?).cloned()
    }

    /// The viewer's pipe, without keeping the viewer alive (see `touch`).
    pub fn lookup(&self, sid: &str) -> Option<Arc<Pipe>> {
        let pipe = self.pipe_of(sid)?;
        let known = pipe.m.locked().viewers.contains_key(sid);
        known.then_some(pipe)
    }

    /// A served data-plane request keeps the viewer. [ts:svc:590-601]
    pub fn touch(&self, pipe: &Pipe, sid: &str) {
        if let Some(v) = pipe.m.locked().viewers.get_mut(sid) {
            v.last_seen_ms = now_ms();
        }
    }

    /// The viewer leaves (its direct streams close); false for a sid this registry never had.
    /// [ts:svc:549-559]
    pub fn leave(&self, rt: &Runtime, sid: &str, reason: &str) -> bool {
        let Some(pipe) = self.pipe_of(sid) else {
            return false;
        };
        self.reg.locked().viewers.remove(sid);
        {
            let mut m = pipe.m.locked();
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
        let pipes: Vec<Arc<Pipe>> = self.reg.locked().pipes.values().cloned().collect();
        let mut out: Vec<LiveSnapshot> = pipes
            .iter()
            .flat_map(|p| {
                let m = p.m.locked();
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
        let m = p.m.locked();
        p.snapshot(sid, &m)
    }
}

type S = State<Arc<AppState>>;

fn refused(status: StatusCode, code: &str) -> Response {
    (status, Json(LiveError { code: code.into() })).into_response()
}

#[utoipa::path(post, path = "/v1/live", operation_id = "open_live", request_body = OpenLiveRequest, responses(
    (status = 200, body = OpenLiveResponse, description = "the opened live session"),
    (status = 400, description = "an invalid request"),
    (status = 409, body = LiveError, description = "`live_at_capacity`: a new upstream is needed and `atCapacity` is set"),
    (status = 503, body = LiveError, description = "`live_unavailable`: every upstream failed; or, without a body, no config set yet or hardware detection has not finished (Retry-After: 1)")))]
pub async fn open_handler(State(s): S, Json(req): Json<OpenLiveRequest>) -> Response {
    match open(&s.rt, req).await {
        Ok(r) => Json(r).into_response(),
        Err(OpenError::NoConfig | OpenError::Detecting) => crate::not_ready().into_response(),
        Err(OpenError::BadRequest(m)) => (StatusCode::BAD_REQUEST, m).into_response(),
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
    responses((status = 200, body = Vec<LiveSnapshot>, description = "live sessions")))]
pub async fn list_handler(State(s): S, Query(q): Query<ListQuery>) -> Json<Vec<LiveSnapshot>> {
    Json(s.rt.live.list(q.owner_id.as_deref()))
}

#[utoipa::path(get, path = "/v1/live/{sid}", operation_id = "get_live", responses(
    (status = 200, body = LiveSnapshot, description = "the live session"), (status = 404, description = "unknown or ended viewer")))]
pub async fn get_handler(State(s): S, UrlPath(sid): UrlPath<String>) -> Response {
    match s.rt.live.snapshot(&sid) {
        Some(snap) => Json(snap).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
