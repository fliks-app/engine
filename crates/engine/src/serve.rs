//! Data plane: the host's `/api/stream/...` routes, byte for byte, authorized by the host
//! resolver. Spec: `docs/spec/runtime.md` section 6.
// The error side of these helpers is the HTTP answer itself.
#![allow(clippy::result_large_err)]
use crate::resolve::{Ask, Host, ResolveError, Resolved};
use crate::runtime::{Fetch, Runtime, Session};
use axum::{
    Router,
    body::Body,
    extract::{Path as UrlPath, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};
use fliks_engine_core::api::{EngineEvent, ResolvePurpose, SessionExpired, SessionKind, Track};
use fliks_engine_core::args::{IframeArgs, iframe};
use fliks_engine_core::decide::timeline::{
    frame_seconds_of, real_segment_seconds, uniform_segment_count,
};
use fliks_engine_core::hls::{
    Rendition, audio_playlist, iframe_index, iframe_resolution, master_playlist, subtitle_playlist,
    variant_playlist,
};
use fliks_engine_core::mp4::{
    IdMap, TrackInfo, cue_offset_seconds, input_seek_seconds, parse_init_tracks, rewrite_cmaf,
    rewrite_segment_tfdt, served_origin,
};
use fliks_engine_core::session::{
    MasterQuery, SegmentName, is_valid_quality, master_options, parse_segment_name,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

type Q = Query<HashMap<String, String>>;
type S = State<Arc<Runtime>>;

const PLAYLIST: &str = "application/vnd.apple.mpegurl";

/// The data-plane router, mountable behind the gateway or on its own listener.
pub fn router(rt: Arc<Runtime>) -> Router {
    Router::new()
        .route("/api/stream/{id}", get(direct))
        .route("/api/stream/{id}/master.m3u8", get(master))
        .route("/api/stream/{id}/iframe/index.m3u8", get(iframe_list))
        .route("/api/stream/{id}/iframe/{seg}", get(iframe_seg))
        .route("/api/stream/{id}/audio/{idx}/index.m3u8", get(audio_list))
        .route("/api/stream/{id}/audio/{idx}/{seg}", get(audio_seg))
        .route("/api/stream/{id}/thumbnails/{file}", get(sprite))
        .route(
            "/api/stream/{id}/subtitles/embedded/{idx}",
            get(embedded_vtt),
        )
        .route(
            "/api/stream/{id}/subtitles/embedded/{idx}/download",
            get(embedded_download),
        )
        .route(
            "/api/stream/{id}/subtitles/embedded/{idx}/index.m3u8",
            get(embedded_list),
        )
        .route("/api/stream/{id}/subtitles/{sub}", get(external_vtt))
        .route(
            "/api/stream/{id}/subtitles/{sub}/download",
            get(external_download),
        )
        .route(
            "/api/stream/{id}/subtitles/{sub}/index.m3u8",
            get(external_list),
        )
        .route("/api/stream/{id}/{quality}/index.m3u8", get(variant))
        .route("/api/stream/{id}/{quality}/{seg}", get(segment))
        .layer(middleware::map_response(cors))
        .with_state(rt)
}

async fn cors(mut res: Response) -> Response {
    res.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    res
}

fn text(status: StatusCode, body: &str) -> Response {
    (status, body.to_string()).into_response()
}

fn retry() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (header::RETRY_AFTER, "2"),
            (header::CACHE_CONTROL, "no-store, no-cache, must-revalidate"),
        ],
    )
        .into_response()
}

fn expired(sid: Option<&str>) -> Response {
    (
        StatusCode::GONE,
        axum::Json(SessionExpired {
            code: "session_expired".into(),
            sid: sid.map(String::from),
            message: "live session no longer exists".into(),
        }),
    )
        .into_response()
}

fn playlist(body: String) -> Response {
    (
        [
            (header::CONTENT_TYPE, PLAYLIST),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

fn file_id(s: &str) -> Result<u32, Response> {
    s.parse().map_err(|_| {
        text(
            StatusCode::BAD_REQUEST,
            "Validation failed (numeric string is expected)",
        )
    })
}

/// `encodeURIComponent`.
fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `?token=..&sid=..` of the request, for every URI a playlist names. [ts:modules/streaming/streaming.controller.ts:175-199]
fn token_param(q: &HashMap<String, String>) -> String {
    let parts: Vec<String> = ["token", "sid"]
        .iter()
        .filter_map(|k| {
            q.get(*k)
                .filter(|v| !v.is_empty())
                .map(|v| format!("{k}={}", encode(v)))
        })
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

/// The credential as received, in the host's extractor order: the `Cookie` header, else a
/// `Bearer` `Authorization` value, else `?token=`. [ts:modules/auth/strategies/jwt.strategy.ts:43-47]
fn credential(headers: &HeaderMap, q: &HashMap<String, String>) -> String {
    let h = |n| {
        headers
            .get(n)
            .and_then(|v: &HeaderValue| v.to_str().ok())
            .filter(|v| !v.is_empty())
    };
    let bearer = h(header::AUTHORIZATION).filter(|v| {
        v.get(..7)
            .is_some_and(|s| s.eq_ignore_ascii_case("bearer "))
    });
    h(header::COOKIE)
        .or(bearer)
        .map(String::from)
        .or_else(|| q.get("token").filter(|t| !t.is_empty()).cloned())
        .unwrap_or_default()
}

struct Access {
    resolved: Resolved,
    /// The request's `sid`, when it names a session of this file.
    session: Option<Arc<Session>>,
}

/// Unknown sid 410 before any resolver call, then another owner's or file's 403.
async fn access(
    rt: &Arc<Runtime>,
    id: u32,
    purpose: ResolvePurpose,
    headers: &HeaderMap,
    q: &HashMap<String, String>,
    (subtitle_id, stream_index): (Option<u32>, Option<u32>),
) -> Result<Access, Response> {
    let sid = q.get("sid").filter(|s| !s.is_empty());
    let config = rt.current_config();
    let host = config.host_url.map(|url| Host {
        url,
        token: config.host_token,
    });
    let cred = credential(headers, q);
    let ask = Ask {
        media: id,
        credential: &cred,
        purpose,
        subtitle_id,
        stream_index,
    };
    let session = sid.map(|s| rt.session(s));
    if let Some(None) = session {
        return Err(expired(sid.map(String::as_str)));
    }
    let resolved = match rt.resolver.resolve(host.as_ref(), ask).await {
        Ok(r) => r,
        Err(ResolveError::Denied) => return Err(text(StatusCode::FORBIDDEN, "Forbidden")),
        // Host down: a live session of this file keeps playing.
        Err(ResolveError::Unavailable) => match &session {
            Some(Some(s)) if s.media_file_id == id => Resolved {
                path: s.path.clone(),
                owner: s.owner.clone(),
                format: None,
            },
            _ => return Err(retry()),
        },
    };
    let session = match session {
        None | Some(None) => None,
        Some(Some(s)) if s.owner != resolved.owner || s.media_file_id != id => {
            return Err(text(StatusCode::FORBIDDEN, "Forbidden"));
        }
        Some(Some(s)) => Some(s),
    };
    Ok(Access { resolved, session })
}

/// The HLS routes' session: the sid's, else the owner's latest of the file, else 410.
async fn hls(
    rt: &Arc<Runtime>,
    id: u32,
    headers: &HeaderMap,
    q: &HashMap<String, String>,
    segment: Option<u32>,
) -> Result<(Arc<Session>, Resolved), Response> {
    let a = access(rt, id, ResolvePurpose::Stream, headers, q, (None, None)).await?;
    let s = match a.session {
        Some(s) => s,
        None => rt
            .current_session(a.resolved.owner.as_deref(), id)
            .ok_or_else(|| expired(None))?,
    };
    rt.touch(&s, segment);
    Ok((s, a.resolved))
}

fn prefix(id: u32) -> String {
    format!("/api/stream/{id}")
}

fn duration_of(s: &Session, q: &HashMap<String, String>) -> f64 {
    q.get("duration")
        .and_then(|d| d.parse::<f64>().ok())
        .filter(|d| *d > 0.0)
        .unwrap_or(s.plan.duration)
}

/// [ts:modules/streaming/streaming.controller.ts:1402-1585]
async fn master(
    State(rt): S,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
    Query(q): Q,
) -> Response {
    let id = match file_id(&id) {
        Ok(i) => i,
        Err(r) => return r,
    };
    let (s, _) = match hls(&rt, id, &headers, &q, None).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let caps = rt.hw.snapshot().map(|h| h.host_caps()).unwrap_or_default();
    let token = token_param(&q);
    let only = q
        .get("startQuality")
        .filter(|v| !v.is_empty())
        .map(String::as_str);
    let opts = master_options(
        &s.req,
        &s.decision,
        &s.plan,
        &caps,
        &MasterQuery {
            prefix: &prefix(id),
            token: &token,
            include_remux: q.get("remux").is_some_and(|v| v == "1"),
            only_quality: only,
        },
    );
    let body = master_playlist(&opts);
    let start_at = q.get("startAt").and_then(|v| v.parse().ok());
    rt.prewarm(&s, only, start_at);
    playlist(body)
}

/// `remux`: the remux variant or a rendition of it, on the frozen keyframe grid.
fn rendition<'a>(
    s: &'a Session,
    q: &HashMap<String, String>,
    token: &'a str,
    remux: bool,
) -> Rendition<'a> {
    Rendition {
        token,
        use_ts: s.plan.use_ts,
        remux_durations: remux
            .then(|| s.plan.remux_grid.as_ref().map(|g| g.durations.as_slice()))
            .flatten(),
        duration: duration_of(s, q),
        source_fps: s.plan.source_fps,
        segment_duration: s.plan.segment_duration,
    }
}

/// [ts:modules/streaming/streaming.controller.ts:2126-2242]
async fn variant(
    State(rt): S,
    UrlPath((id, quality)): UrlPath<(String, String)>,
    headers: HeaderMap,
    Query(q): Q,
) -> Response {
    let id = match file_id(&id) {
        Ok(i) => i,
        Err(r) => return r,
    };
    if !is_valid_quality(&quality) {
        return text(
            StatusCode::BAD_REQUEST,
            &format!("Invalid quality: {quality}"),
        );
    }
    let (s, _) = match hls(&rt, id, &headers, &q, None).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let token = token_param(&q);
    let r = rendition(&s, &q, &token, quality == "remux");
    if r.duration <= 0.0 {
        return text(
            StatusCode::NOT_FOUND,
            "Duration unknown — rescan the file first",
        );
    }
    if let Some(at) = q.get("startAt").and_then(|v| v.parse().ok()) {
        if quality == "remux" {
            rt.prewarm_remux(&s, at);
        } else {
            rt.prewarm(&s, Some(&quality), Some(at));
        }
    }
    playlist(variant_playlist(
        &prefix(id),
        &quality,
        s.plan.multi_audio,
        &r,
    ))
}

/// [ts:modules/streaming/streaming.controller.ts:1855-1906]
async fn audio_list(
    State(rt): S,
    UrlPath((id, idx)): UrlPath<(String, u32)>,
    headers: HeaderMap,
    Query(q): Q,
) -> Response {
    let id = match file_id(&id) {
        Ok(i) => i,
        Err(r) => return r,
    };
    let (s, _) = match hls(&rt, id, &headers, &q, None).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let token = token_param(&q);
    let remux = s.served_kind() == SessionKind::Remux;
    let r = rendition(&s, &q, &token, remux);
    if r.duration <= 0.0 {
        return text(StatusCode::NOT_FOUND, "Duration unknown");
    }
    playlist(audio_playlist(&prefix(id), idx, remux, &r))
}

/// [ts:modules/streaming/streaming.controller.ts:1586-1622]
async fn iframe_list(
    State(rt): S,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
    Query(q): Q,
) -> Response {
    let id = match file_id(&id) {
        Ok(i) => i,
        Err(r) => return r,
    };
    let (s, _) = match hls(&rt, id, &headers, &q, None).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let d = duration_of(&s, &q);
    if d <= 0.0 {
        return text(
            StatusCode::NOT_FOUND,
            "Duration unknown — rescan the file first",
        );
    }
    playlist(iframe_index(
        &prefix(id),
        &token_param(&q),
        d,
        s.plan.source_fps,
        s.plan.segment_duration,
    ))
}

/// One trick-play keyframe, decoded and muxed to MPEG-TS on demand. [ts:modules/streaming/streaming.controller.ts:1625-1680]
async fn iframe_seg(
    State(rt): S,
    UrlPath((id, seg)): UrlPath<(String, String)>,
    headers: HeaderMap,
    Query(q): Q,
) -> Response {
    let id = match file_id(&id) {
        Ok(i) => i,
        Err(r) => return r,
    };
    let Some(n) = seg
        .strip_prefix("seg-")
        .and_then(|r| r.strip_suffix(".ts"))
        .and_then(|d| d.parse::<u32>().ok())
    else {
        return text(
            StatusCode::BAD_REQUEST,
            &format!("Invalid trick-play segment: {seg}"),
        );
    };
    let (s, res) = match hls(&rt, id, &headers, &q, None).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let v = s.req.decide.info.video.first();
    let crop = v.and_then(|v| v.crop.clone());
    let (w, h) = iframe_resolution(
        crop.as_ref()
            .map(|c| c.width)
            .or(v.and_then(|v| v.width))
            .unwrap_or(1920) as f64,
        crop.as_ref()
            .map(|c| c.height)
            .or(v.and_then(|v| v.height))
            .unwrap_or(1080) as f64,
    );
    let t = &s.plan.timeline;
    let args = iframe(&IframeArgs {
        input_path: crate::runtime::map_media_path(&res.path),
        seek_seconds: input_seek_seconds(
            f64::from(n) * real_segment_seconds(s.plan.segment_duration, s.plan.source_fps),
            t,
        ),
        width: w as u32,
        height: h as u32,
        crop,
        timeline_offset_seconds: Some((1.0 - t.origin).max(0.0)),
    });
    match run_capture(&rt, &args, Duration::from_secs(20)).await {
        Some(out) if !out.is_empty() => (
            [
                (header::CONTENT_TYPE, "video/mp2t"),
                (header::CACHE_CONTROL, "public, max-age=3600"),
            ],
            out,
        )
            .into_response(),
        // A refused frame must stay retryable: some players drop trick play on a 404.
        _ => retry(),
    }
}

/// stdout of a short playback ffmpeg run, `None` on failure or timeout.
async fn run_capture(rt: &Runtime, args: &[String], timeout: Duration) -> Option<Vec<u8>> {
    let _slot = rt.slots.playback();
    let mut cmd = tokio::process::Command::new(&rt.ffmpeg);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let child = crate::proc::spawn(&mut cmd).await.ok()?;
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    out.status.success().then_some(out.stdout)
}

fn init_tracks_cache() -> &'static Mutex<HashMap<PathBuf, IdMap<TrackInfo>>> {
    static C: OnceLock<Mutex<HashMap<PathBuf, IdMap<TrackInfo>>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// Track info of a rendition directory, parsed once from its init.
async fn init_tracks(dir: &Path) -> IdMap<TrackInfo> {
    if let Some(t) = init_tracks_cache().lock().unwrap().get(dir) {
        return t.clone();
    }
    let mut init = None;
    if let Ok(mut rd) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            if matches!(
                parse_segment_name(&e.file_name().to_string_lossy()),
                Some(SegmentName::Init(_))
            ) {
                init = tokio::fs::read(e.path()).await.ok();
                break;
            }
        }
    }
    let tracks = init
        .and_then(|b| parse_init_tracks(&b).ok())
        .unwrap_or_default();
    if !tracks.is_empty() {
        init_tracks_cache()
            .lock()
            .unwrap()
            .insert(dir.to_path_buf(), tracks.clone());
    }
    tracks
}

/// Wire bytes of a cache file: TS verbatim; fMP4 CMAF-rewritten and anchored on the served
/// timeline, unless `keyframe_cut` (a remux file, assembled on it already).
/// [ts:modules/streaming/services/segment-packaging.service.ts:30-130]
async fn package(
    path: &Path,
    name: SegmentName,
    s: &Session,
    keyframe_cut: bool,
) -> Option<Vec<u8>> {
    let buf = tokio::fs::read(path).await.ok().filter(|b| !b.is_empty())?;
    if path.extension().is_some_and(|e| e == "ts") {
        return Some(buf);
    }
    let buf = rewrite_cmaf(&buf)?;
    let SegmentName::Segment(n) = name else {
        return Some(buf);
    };
    if keyframe_cut {
        return Some(buf);
    }
    let tracks = init_tracks(path.parent()?).await;
    let seg = real_segment_seconds(s.plan.segment_duration, s.plan.source_fps);
    match rewrite_segment_tfdt(&buf, &tracks, n, seg, served_origin(s.plan.timeline.origin)) {
        Ok(b) => Some(b),
        Err(e) => {
            tracing::warn!(path = %path.display(), "tfdt rewrite: {e:?}");
            Some(buf)
        }
    }
}

/// CMCD of a request: `?CMCD=`, else the `CMCD-*` headers.
fn cmcd(headers: &HeaderMap, q: &HashMap<String, String>) -> Option<String> {
    if let Some(c) = q.get("CMCD") {
        return Some(c.clone());
    }
    let parts: Vec<&str> = ["cmcd-request", "cmcd-object", "cmcd-status", "cmcd-session"]
        .iter()
        .filter_map(|h| headers.get(*h).and_then(|v| v.to_str().ok()))
        .collect();
    (!parts.is_empty()).then(|| parts.join(","))
}

async fn respond(
    rt: &Runtime,
    s: &Session,
    fetch: Fetch,
    name: SegmentName,
    (track, rung, remux): (Track, &str, bool),
    headers: &HeaderMap,
    q: &HashMap<String, String>,
) -> Response {
    let (path, job, waited_ms, cache_hit) = match fetch {
        Fetch::Ready {
            path,
            job,
            waited_ms,
            cache_hit,
        } => (path, job, waited_ms, cache_hit),
        Fetch::Retry => return retry(),
        Fetch::NotFound => return text(StatusCode::NOT_FOUND, "Segment not found"),
        Fetch::Failed => return text(StatusCode::INTERNAL_SERVER_ERROR, "ffmpeg could not start"),
    };
    // A restart can remove the file between the wait and the read.
    let Some(bytes) = package(&path, name, s, remux).await else {
        return retry();
    };
    if let SegmentName::Segment(index) = name {
        if track == Track::Video {
            let hw = job
                .as_ref()
                .map_or(fliks_engine_core::api::HwAccel::None, |j| j.hw);
            rt.note_served(s, rung, hw);
        }
        rt.events.emit(EngineEvent::SegmentServed {
            session_id: s.sid.clone(),
            track,
            rung: rung.into(),
            index,
            waited_ms,
            cache_hit,
            cmcd_raw: cmcd(headers, q),
        });
    }
    let ct = if path.extension().is_some_and(|e| e == "ts") {
        "video/MP2T"
    } else {
        "video/mp4"
    };
    (
        [
            (header::CONTENT_TYPE, ct),
            (header::CACHE_CONTROL, "no-store"),
        ],
        bytes,
    )
        .into_response()
}

/// A segment past the playlist's last one would spawn a run seeking past the end: the remux
/// grid's [ts:modules/streaming/streaming.controller.ts:555-568], else the uniform one's [new].
fn past_end(s: &Session, name: SegmentName, remux: bool) -> bool {
    let SegmentName::Segment(n) = name else {
        return false;
    };
    let p = &s.plan;
    if let (true, Some(g)) = (remux, &p.remux_grid) {
        return n as usize >= g.durations.len();
    }
    let seg = if remux {
        p.segment_duration
    } else {
        real_segment_seconds(p.segment_duration, p.source_fps)
    };
    p.duration > 0.0
        && f64::from(n) >= uniform_segment_count(p.duration, seg, frame_seconds_of(p.source_fps))
}

fn seg_name(seg: &str) -> Result<SegmentName, Response> {
    parse_segment_name(seg).ok_or_else(|| {
        text(
            StatusCode::BAD_REQUEST,
            &format!("Invalid segment name: {seg}"),
        )
    })
}

/// [ts:modules/streaming/streaming.controller.ts:2245-2545]
async fn segment(
    State(rt): S,
    UrlPath((id, quality, seg)): UrlPath<(String, String, String)>,
    headers: HeaderMap,
    Query(q): Q,
) -> Response {
    let id = match file_id(&id) {
        Ok(i) => i,
        Err(r) => return r,
    };
    if !is_valid_quality(&quality) {
        return text(
            StatusCode::BAD_REQUEST,
            &format!("Invalid quality: {quality}"),
        );
    }
    let name = match seg_name(&seg) {
        Ok(n) => n,
        Err(r) => return r,
    };
    let index = match name {
        SegmentName::Segment(n) => Some(n),
        SegmentName::Init(_) => None,
    };
    let (s, _) = match hls(&rt, id, &headers, &q, index).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let remux = quality == "remux";
    rt.note_kind(&s, remux);
    if past_end(&s, name, remux) {
        return text(StatusCode::NOT_FOUND, "Segment not found");
    }
    let fetch = if remux {
        rt.remux_file(&s, 0, name).await
    } else {
        rt.video_file(&s, &quality, name).await
    };
    respond(
        &rt,
        &s,
        fetch,
        name,
        (Track::Video, &quality, remux),
        &headers,
        &q,
    )
    .await
}

/// [ts:modules/streaming/streaming.controller.ts:1909-2120]
async fn audio_seg(
    State(rt): S,
    UrlPath((id, idx, seg)): UrlPath<(String, u32, String)>,
    headers: HeaderMap,
    Query(q): Q,
) -> Response {
    let id = match file_id(&id) {
        Ok(i) => i,
        Err(r) => return r,
    };
    let name = match seg_name(&seg) {
        Ok(n) => n,
        Err(r) => return r,
    };
    let index = match name {
        SegmentName::Segment(n) => Some(n),
        SegmentName::Init(_) => None,
    };
    let (s, _) = match hls(&rt, id, &headers, &q, index).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let remux = s.served_kind() == SessionKind::Remux;
    if !s.plan.multi_audio || past_end(&s, name, remux) {
        return text(StatusCode::NOT_FOUND, "Segment not found");
    }
    let track = idx as usize + 1;
    let fetch = if remux {
        rt.remux_file(&s, track, name).await
    } else {
        rt.audio_file(&s, track, name).await
    };
    let rung = format!("audio-{idx}");
    respond(
        &rt,
        &s,
        fetch,
        name,
        (Track::Audio, &rung, remux),
        &headers,
        &q,
    )
    .await
}

/// `bytes=S-E`, `S-`, `-N` against `size`, inclusive. [ts:modules/streaming/byte-range.util.ts:16-50]
pub fn parse_range(h: &str, size: u64) -> Option<(u64, u64)> {
    let spec = h.trim_start_matches("bytes=").split(',').next()?.trim();
    let (a, b) = spec.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    let num = |s: &str| {
        (!s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
            .then(|| s.parse::<u64>().ok())
            .flatten()
    };
    if a.is_empty() {
        let n = num(b).filter(|n| *n > 0)?;
        return Some((size.saturating_sub(n), size.saturating_sub(1)));
    }
    let start = num(a).filter(|s| *s < size)?;
    let end = if b.is_empty() {
        size - 1
    } else {
        num(b)?.min(size - 1)
    };
    (end >= start).then_some((start, end))
}

fn content_type(path: &str, format: Option<&str>) -> String {
    if let Some(f) = format.filter(|f| f.contains('/')) {
        return f.into();
    }
    let ext = Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "ts" | "m2ts" | "mts" => "video/mp2t",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "flac" => "audio/flac",
        _ => "application/octet-stream",
    }
    .into()
}

fn attachment(name: &str) -> String {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "attachment; filename=\"{ascii}\"; filename*=UTF-8''{}",
        encode(name)
    )
}

fn base_name(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

/// Streams `[start, end]` of a file.
async fn file_body(path: &str, start: u64, len: u64) -> std::io::Result<Body> {
    let mut f = tokio::fs::File::open(path).await?;
    f.seek(std::io::SeekFrom::Start(start)).await?;
    let stream = futures_util::stream::unfold(
        (f.take(len), vec![0u8; 64 * 1024]),
        |(mut r, mut buf)| async move {
            match r.read(&mut buf).await {
                Ok(0) => None,
                Ok(n) => Some((
                    Ok::<_, std::io::Error>(axum::body::Bytes::copy_from_slice(&buf[..n])),
                    (r, buf),
                )),
                Err(e) => Some((Err(e), (r, buf))),
            }
        },
    );
    Ok(Body::from_stream(stream))
}

/// Direct play and `?download=1`. [ts:modules/streaming/streaming.controller.ts:2555-2641]
async fn direct(
    State(rt): S,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
    Query(q): Q,
) -> Response {
    let id = match file_id(&id) {
        Ok(i) => i,
        Err(r) => return r,
    };
    let download = q.get("download").is_some_and(|v| !v.is_empty());
    let purpose = if download {
        ResolvePurpose::Download
    } else {
        ResolvePurpose::Stream
    };
    let a = match access(&rt, id, purpose, &headers, &q, (None, None)).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    if let Some(s) = &a.session {
        rt.touch(s, None);
    }
    let path = crate::runtime::map_media_path(&a.resolved.path);
    let Ok(md) = tokio::fs::metadata(&path).await else {
        return text(StatusCode::NOT_FOUND, "File not found");
    };
    let size = md.len();
    let mut res = Response::builder()
        .header(
            header::CONTENT_TYPE,
            content_type(&path, a.resolved.format.as_deref()),
        )
        .header(header::ACCEPT_RANGES, "bytes");
    let session = a
        .session
        .clone()
        .or_else(|| rt.current_session(a.resolved.owner.as_deref(), id));
    if let Some(d) = session
        .as_ref()
        .and_then(|s| s.req.decide.info.duration_seconds)
    {
        res = res
            .header("X-Content-Duration", fliks_engine_core::jsnum::js_string(d))
            .header(header::ACCESS_CONTROL_EXPOSE_HEADERS, "X-Content-Duration");
    }
    if download {
        res = res.header(header::CONTENT_DISPOSITION, attachment(&base_name(&path)));
    }
    let (status, start, len) = match headers.get(header::RANGE).and_then(|r| r.to_str().ok()) {
        None => (StatusCode::OK, 0, size),
        Some(r) => match parse_range(r, size) {
            Some((s, e)) => {
                res = res.header(header::CONTENT_RANGE, format!("bytes {s}-{e}/{size}"));
                (StatusCode::PARTIAL_CONTENT, s, e - s + 1)
            }
            None => {
                return (
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    [(header::CONTENT_RANGE, format!("bytes */{size}"))],
                )
                    .into_response();
            }
        },
    };
    match file_body(&path, start, len).await {
        Ok(body) => res
            .status(status)
            .header(header::CONTENT_LENGTH, len)
            .body(body)
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        Err(e) => {
            tracing::error!(path, "direct play read: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// The engine's `sprite.json` / `sprite.jpg`, else the ones next to the resolver's `sprite.jpg`. [ts:modules/streaming/streaming.controller.ts:1363-1395]
async fn sprite(
    State(rt): S,
    UrlPath((id, file)): UrlPath<(String, String)>,
    headers: HeaderMap,
    Query(q): Q,
) -> Response {
    let id = match file_id(&id) {
        Ok(i) => i,
        Err(r) => return r,
    };
    let ct = match file.as_str() {
        "sprite.json" => "application/json; charset=utf-8",
        "sprite.jpg" => "image/jpeg",
        _ => return text(StatusCode::NOT_FOUND, "Not Found"),
    };
    let a = match access(&rt, id, ResolvePurpose::Sprite, &headers, &q, (None, None)).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    let own = rt.cache.sprite_dir(id).join(&file);
    let path = if tokio::fs::metadata(&own).await.is_ok() || a.resolved.path.is_empty() {
        own
    } else {
        PathBuf::from(&a.resolved.path).with_file_name(&file)
    };
    match tokio::fs::read(&path).await {
        Ok(b) => (
            [
                (header::CONTENT_TYPE, ct),
                (header::CACHE_CONTROL, "public, max-age=86400"),
            ],
            b,
        )
            .into_response(),
        Err(_) if file == "sprite.json" => (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({ "error": "sprite not generated" })),
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `X-TIMESTAMP-MAP` after the `WEBVTT` line. [ts:modules/streaming/streaming.controller.ts:119-130]
pub fn with_timestamp_map(vtt: &str, offset: f64) -> String {
    let mpegts = (offset.max(0.0) * 90000.0).round() as u64;
    let ms = ((-offset).max(0.0) * 1000.0).round() as u64;
    let local = format!(
        "{:02}:{:02}:{:02}.{:03}",
        (ms / 3_600_000) % 24,
        (ms / 60_000) % 60,
        (ms / 1000) % 60,
        ms % 1000
    );
    let map = format!("X-TIMESTAMP-MAP=MPEGTS:{mpegts},LOCAL:{local}");
    match vtt.find('\n') {
        Some(i) if vtt.starts_with("WEBVTT") => format!("{}\n{map}\n{}", &vtt[..i], &vtt[i + 1..]),
        _ => vtt.to_string(),
    }
}

fn normalize_vtt(s: &str) -> String {
    s.trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

/// Cue offset: the latest session of (file, owner) on the served timeline, else 0.
fn cue_offset(rt: &Runtime, id: u32, a: &Access) -> f64 {
    a.session
        .clone()
        .or_else(|| rt.current_session(a.resolved.owner.as_deref(), id))
        .map_or(0.0, |s| cue_offset_seconds(&s.plan.timeline))
}

fn vtt_response(body: String, download: Option<String>) -> Response {
    let mut res = Response::builder().header(header::CONTENT_TYPE, "text/vtt; charset=utf-8");
    if let Some(name) = download {
        res = res.header(header::CONTENT_DISPOSITION, attachment(&name));
    }
    res.body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Embedded text track as WebVTT, extracted once per (file, stream) the way the job does.
async fn embedded_text(rt: &Runtime, id: u32, idx: u32, media: &str) -> Option<String> {
    use crate::jobs::subtitles::{raw_to_vtt, store, vtt_path};
    let dir = rt.cache.subtitles_dir(id);
    if let Ok(t) = tokio::fs::read_to_string(vtt_path(&dir, idx)).await {
        return Some(t);
    }
    let args = fliks_engine_core::jobs::subtitles::extract_args(media, &[(idx, "pipe:1")]);
    let out = run_capture(rt, &args, Duration::from_secs(300)).await?;
    let vtt = raw_to_vtt(&out, "pipe:1").ok()?;
    let _ = store(&dir, idx, &vtt).await;
    Some(vtt)
}

async fn external_text(rt: &Runtime, path: &str, format: Option<&str>) -> Option<String> {
    let ext = format.map(str::to_lowercase).or_else(|| {
        Path::new(path)
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
    });
    if ext.as_deref() == Some("vtt") {
        return tokio::fs::read_to_string(path)
            .await
            .ok()
            .map(|t| normalize_vtt(&t));
    }
    let args: Vec<String> = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-i",
        path,
        "-f",
        "webvtt",
        "pipe:1",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    run_capture(rt, &args, Duration::from_secs(60))
        .await
        .map(|o| String::from_utf8_lossy(&o).into_owned())
}

async fn subtitle_access(
    rt: &Arc<Runtime>,
    id: &str,
    headers: &HeaderMap,
    q: &HashMap<String, String>,
    key: (Option<u32>, Option<u32>),
) -> Result<(u32, Access), Response> {
    let id = file_id(id)?;
    let a = access(rt, id, ResolvePurpose::Subtitle, headers, q, key).await?;
    Ok((id, a))
}

/// [ts:modules/streaming/streaming.controller.ts:1682-1712]
async fn embedded_vtt(
    State(rt): S,
    UrlPath((id, idx)): UrlPath<(String, u32)>,
    headers: HeaderMap,
    Query(q): Q,
) -> Response {
    let (id, a) = match subtitle_access(&rt, &id, &headers, &q, (None, Some(idx))).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let media = crate::runtime::map_media_path(&a.resolved.path);
    match embedded_text(&rt, id, idx, &media).await {
        Some(t) => vtt_response(with_timestamp_map(&t, cue_offset(&rt, id, &a)), None),
        None => text(StatusCode::NOT_FOUND, "Subtitle stream not found"),
    }
}

/// [ts:modules/streaming/streaming.controller.ts:1715-1746]
async fn embedded_download(
    State(rt): S,
    UrlPath((id, idx)): UrlPath<(String, u32)>,
    headers: HeaderMap,
    Query(q): Q,
) -> Response {
    let (id, a) = match subtitle_access(&rt, &id, &headers, &q, (None, Some(idx))).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let media = crate::runtime::map_media_path(&a.resolved.path);
    let stem = Path::new(&media)
        .file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    match embedded_text(&rt, id, idx, &media).await {
        Some(t) => vtt_response(
            with_timestamp_map(&t, cue_offset(&rt, id, &a)),
            Some(format!("{stem}.track-{idx}.vtt")),
        ),
        None => text(StatusCode::NOT_FOUND, "Subtitle stream not found"),
    }
}

/// [ts:modules/streaming/streaming.controller.ts:1765-1779]
async fn external_vtt(
    State(rt): S,
    UrlPath((id, sub)): UrlPath<(String, u32)>,
    headers: HeaderMap,
    Query(q): Q,
) -> Response {
    let (id, a) = match subtitle_access(&rt, &id, &headers, &q, (Some(sub), None)).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let path = crate::runtime::map_media_path(&a.resolved.path);
    match external_text(&rt, &path, a.resolved.format.as_deref()).await {
        Some(t) => vtt_response(with_timestamp_map(&t, cue_offset(&rt, id, &a)), None),
        None => text(StatusCode::NOT_FOUND, "Subtitle not found"),
    }
}

/// The original file, as stored. [ts:modules/streaming/streaming.controller.ts:1748-1763]
async fn external_download(
    State(rt): S,
    UrlPath((id, sub)): UrlPath<(String, u32)>,
    headers: HeaderMap,
    Query(q): Q,
) -> Response {
    let (_, a) = match subtitle_access(&rt, &id, &headers, &q, (Some(sub), None)).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let path = crate::runtime::map_media_path(&a.resolved.path);
    match tokio::fs::read(&path).await {
        Ok(b) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                (header::CONTENT_DISPOSITION, attachment(&base_name(&path))),
            ],
            b,
        )
            .into_response(),
        Err(_) => text(StatusCode::NOT_FOUND, "Subtitle not found"),
    }
}

/// [ts:modules/streaming/streaming.controller.ts:1801-1849]
async fn subtitle_list(
    rt: &Runtime,
    id: u32,
    a: &Access,
    q: &HashMap<String, String>,
    vtt_path: &str,
) -> Response {
    let duration = a
        .session
        .clone()
        .or_else(|| rt.current_session(a.resolved.owner.as_deref(), id))
        .map_or(0.0, |s| s.plan.duration);
    let url = format!("{}/{vtt_path}{}", prefix(id), token_param(q));
    playlist(subtitle_playlist(duration, &url))
}

async fn embedded_list(
    State(rt): S,
    UrlPath((id, idx)): UrlPath<(String, u32)>,
    headers: HeaderMap,
    Query(q): Q,
) -> Response {
    match subtitle_access(&rt, &id, &headers, &q, (None, Some(idx))).await {
        Ok((id, a)) => subtitle_list(&rt, id, &a, &q, &format!("subtitles/embedded/{idx}")).await,
        Err(r) => r,
    }
}

async fn external_list(
    State(rt): S,
    UrlPath((id, sub)): UrlPath<(String, u32)>,
    headers: HeaderMap,
    Query(q): Q,
) -> Response {
    match subtitle_access(&rt, &id, &headers, &q, (Some(sub), None)).await {
        Ok((id, a)) => subtitle_list(&rt, id, &a, &q, &format!("subtitles/{sub}")).await,
        Err(r) => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=900-", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=-5000", 1000), Some((0, 999)));
        assert_eq!(parse_range("bytes=990-2000", 1000), Some((990, 999)));
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=5-1", 1000), None);
        assert_eq!(parse_range("bytes=a-b", 1000), None);
        assert_eq!(parse_range("bytes=0-1,5-6", 1000), Some((0, 1)));
    }

    #[test]
    fn timestamp_map_and_token() {
        assert_eq!(
            with_timestamp_map("WEBVTT\n\n00:00.000 --> 00:01.000\nhi\n", 1.5),
            "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:135000,LOCAL:00:00:00.000\n\n00:00.000 --> 00:01.000\nhi\n"
        );
        assert!(with_timestamp_map("WEBVTT\n", -2.25).contains("MPEGTS:0,LOCAL:00:00:02.250"));
        let q: HashMap<String, String> =
            [("token".into(), "a b/c".into()), ("sid".into(), "x".into())].into();
        assert_eq!(token_param(&q), "?token=a%20b%2Fc&sid=x");
        assert_eq!(token_param(&HashMap::new()), "");
    }

    #[test]
    fn credential_order_and_content_type() {
        let mut h = HeaderMap::new();
        let q: HashMap<String, String> = [("token".into(), "t".into())].into();
        assert_eq!(credential(&h, &q), "t");
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic x"));
        assert_eq!(credential(&h, &q), "t");
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer j"));
        assert_eq!(credential(&h, &q), "Bearer j");
        h.insert(header::COOKIE, HeaderValue::from_static("a=1"));
        assert_eq!(credential(&h, &q), "a=1");
        assert_eq!(content_type("/a/b.MKV", None), "video/x-matroska");
        assert_eq!(content_type("/a/b.bin", Some("video/mp4")), "video/mp4");
    }
}
