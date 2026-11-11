//! HLS playlists, init and media segments, trick-play and audio renditions.

use super::{
    Q, S, duration_of, file_id, hls, playlist, prefix, retry, stream_body, text, token_param,
};
use crate::events::Events;
use crate::lock::Locked;
use crate::runtime::{Fetch, Runtime, Session};
use axum::body::Body;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use fliks_engine_core::api::{EngineEvent, SessionKind, Track};
use fliks_engine_core::args::{IframeArgs, iframe};
use fliks_engine_core::decide::timeline::{
    frame_seconds_of, real_segment_seconds, uniform_segment_count,
};
use fliks_engine_core::hls::{
    Rendition, audio_playlist, iframe_index, iframe_resolution, master_playlist, variant_playlist,
};
use fliks_engine_core::mp4::{
    IdMap, TrackInfo, has_samples, input_seek_seconds, parse_init_tracks, rewrite_cmaf,
    rewrite_segment_tfdt, served_origin,
};
use fliks_engine_core::session::{
    MasterQuery, SegmentName, is_valid_quality, master_options, parse_segment_name,
    rung_source_size,
};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// Rendition directories whose init is cached; the map restarts empty past it.
const MAX_INIT_TRACKS: usize = 1024;

/// [ts:modules/streaming/streaming.controller.ts:1402-1585]
pub(super) async fn master(
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
    let caps = rt.session_caps(s.device().as_deref());
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
pub(super) async fn variant(
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
pub(super) async fn audio_list(
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
pub(super) async fn iframe_list(
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
pub(super) async fn iframe_seg(
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
    let (w, h) = rung_source_size(v);
    let (w, h) = iframe_resolution(f64::from(w), f64::from(h));
    let t = &s.plan.timeline;
    let args = iframe(&IframeArgs {
        input_path: rt.map_media_path(&res.path),
        seek_seconds: input_seek_seconds(
            f64::from(n) * real_segment_seconds(s.plan.segment_duration, s.plan.source_fps),
            t,
        ),
        width: w as u32,
        height: h as u32,
        crop,
        timeline_offset_seconds: Some((1.0 - t.origin).max(0.0)),
    });
    match run_capture(&rt, &args, Duration::from_secs(20), &rt.ffmpeg).await {
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
pub(super) async fn run_capture(
    rt: &Runtime,
    args: &[String],
    timeout: Duration,
    tool: &str,
) -> Option<Vec<u8>> {
    let _permit = rt.capture.acquire().await.ok()?;
    let _slot = rt.slots.playback_on(None);
    let mut cmd = tokio::process::Command::new(tool);
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

/// Track info of a rendition directory, parsed from its init and cached while the file is unchanged.
async fn init_tracks(rt: &Runtime, dir: &Path) -> IdMap<TrackInfo> {
    let known = rt.init_tracks.locked().get(dir).cloned();
    if let Some((init, stamp, tracks)) = known
        && stamp_of(&init).await == Some(stamp)
    {
        return tracks;
    }
    let mut init = None;
    if let Ok(mut rd) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            if matches!(
                parse_segment_name(&e.file_name().to_string_lossy()),
                Some(SegmentName::Init(_))
            ) {
                init = Some(e.path());
                break;
            }
        }
    }
    let Some(init) = init else {
        return IdMap::default();
    };
    let stamp = stamp_of(&init).await;
    let tracks = tokio::fs::read(&init)
        .await
        .ok()
        .and_then(|b| parse_init_tracks(&b).ok())
        .unwrap_or_default();
    if let (false, Some(stamp)) = (tracks.is_empty(), stamp) {
        let mut cache = rt.init_tracks.locked();
        if cache.len() >= MAX_INIT_TRACKS {
            cache.clear();
        }
        cache.insert(dir.to_path_buf(), (init, stamp, tracks.clone()));
    }
    tracks
}

/// Modification time and size of a file: what tells a regenerated init from the cached one.
async fn stamp_of(path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let m = tokio::fs::metadata(path).await.ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// Wire body of a cache file and its length: TS streamed verbatim; fMP4 CMAF-rewritten and
/// anchored on the served timeline, unless `keyframe_cut` (a remux file, assembled on it already).
/// [ts:modules/streaming/services/segment-packaging.service.ts:30-130]
async fn package(
    rt: &Runtime,
    path: &Path,
    name: SegmentName,
    s: &Session,
    (keyframe_cut, start_pts): (bool, f64),
) -> Result<(Body, usize), Unpackable> {
    if path.extension().is_some_and(|e| e == "ts") {
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|_| Unpackable::Gone)?;
        let len = file.metadata().await.map_err(|_| Unpackable::Gone)?.len();
        let len = usize::try_from(len).map_err(|_| Unpackable::Gone)?;
        return if len > 0 {
            Ok((stream_body(file, 256 * 1024), len))
        } else {
            Err(Unpackable::Gone)
        };
    }
    let read = || async { rewrite_cmaf(tokio::fs::read(path).await.ok()?) };
    let mut buf = read().await.ok_or(Unpackable::Gone)?;
    if let (SegmentName::Segment(n), false) = (name, keyframe_cut) {
        if !has_samples(&buf) {
            return Err(Unpackable::NoMedia);
        }
        let tracks = init_tracks(rt, path.parent().ok_or(Unpackable::Gone)?).await;
        let seg = real_segment_seconds(s.plan.segment_duration, s.plan.source_fps);
        buf = match rewrite_segment_tfdt(buf, &tracks, n, seg, start_pts) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(path = %path.display(), "tfdt rewrite: {e:?}");
                read().await.ok_or(Unpackable::Gone)?
            }
        };
    }
    let len = buf.len();
    Ok((Body::from(buf), len))
}

/// Why a cache file cannot be served.
enum Unpackable {
    /// Removed or unreadable (a restart): the player retries.
    Gone,
    /// A sample-less segment, what ffmpeg writes when it seeks past the end of the source.
    NoMedia,
}

/// CMCD of a request: `?CMCD=`, else the `CMCD-*` headers.
pub(super) fn cmcd(headers: &HeaderMap, q: &HashMap<String, String>) -> Option<String> {
    if let Some(c) = q.get("CMCD") {
        return Some(c.clone());
    }
    let parts: Vec<&str> = ["cmcd-request", "cmcd-object", "cmcd-status", "cmcd-session"]
        .iter()
        .filter_map(|h| headers.get(*h).and_then(|v| v.to_str().ok()))
        .collect();
    (!parts.is_empty()).then(|| parts.join(","))
}

/// Where a part's segments sit on an item's timeline.
#[derive(Clone, Copy)]
pub(super) struct PartBase {
    pub offset_s: f64,
    pub first_segment: u32,
}

/// Waits below this are ordinary seek cancellations, not worth an event.
const ABANDONED_MIN_WAIT: Duration = Duration::from_secs(1);
/// A session whose last request is further than this from a dropped one has seeked away from it.
const SAME_POSITION_SEGMENTS: u32 = 2;

/// Emits `SegmentAbandoned` when the client drops a request that was still waiting for its file.
pub(super) struct WaitGuard<'a> {
    events: &'a Events,
    sid: String,
    track: Track,
    rung: String,
    index: Option<u32>,
    session: Option<Arc<Session>>,
    t0: Instant,
    done: bool,
}

impl<'a> WaitGuard<'a> {
    pub(super) fn new(
        events: &'a Events,
        session: Option<&Arc<Session>>,
        sid: &str,
        track: Track,
        rung: &str,
        index: Option<u32>,
    ) -> Self {
        Self {
            events,
            sid: sid.into(),
            track,
            rung: rung.into(),
            index,
            session: session.cloned(),
            t0: Instant::now(),
            done: false,
        }
    }

    pub(super) fn done(mut self) {
        self.done = true;
    }
}

impl Drop for WaitGuard<'_> {
    fn drop(&mut self) {
        let waited = self.t0.elapsed();
        if self.done || waited < ABANDONED_MIN_WAIT {
            return;
        }
        let Some(index) = self.index else { return };
        let last = self
            .session
            .as_ref()
            .and_then(|s| s.last_requested_segment());
        if last.is_some_and(|l| l.abs_diff(index) > SAME_POSITION_SEGMENTS) {
            return;
        }
        self.events.emit(EngineEvent::SegmentAbandoned {
            session_id: std::mem::take(&mut self.sid),
            track: self.track,
            rung: std::mem::take(&mut self.rung),
            index,
            waited_ms: waited.as_secs_f64() * 1000.0,
        });
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn respond(
    rt: &Runtime,
    s: &Session,
    fetch: Fetch,
    name: SegmentName,
    (track, rung, remux): (Track, &str, bool),
    part: Option<PartBase>,
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
        Fetch::Failed => {
            let dir = rt
                .cache
                .entry_dir(s.owner.as_deref(), s.media_file_id, &s.hash);
            return match rt.check_disk(&dir).await {
                Err(e) => text(StatusCode::SERVICE_UNAVAILABLE, &e),
                Ok(()) => text(StatusCode::INTERNAL_SERVER_ERROR, "ffmpeg could not start"),
            };
        }
    };
    // A restart can remove the file between the wait and the read.
    let start_pts = part.map_or(served_origin(s.plan.timeline.origin), |p| p.offset_s);
    let (body, len) = match package(rt, &path, name, s, (remux, start_pts)).await {
        Ok(p) => p,
        // The watcher discards a no-media run before this read: its file is gone, not restarting.
        Err(Unpackable::Gone) if job.as_ref().is_some_and(|j| j.discarded()) => {
            return text(StatusCode::NOT_FOUND, "Segment not found");
        }
        Err(Unpackable::Gone) => return retry(),
        Err(Unpackable::NoMedia) => {
            let at = if let SegmentName::Segment(n) = name {
                n
            } else {
                0
            };
            let start = job.as_ref().map_or(at, |j| j.start_segment);
            rt.discard_no_media(job.as_ref(), &path, start, s).await;
            return text(StatusCode::NOT_FOUND, "Segment not found");
        }
    };
    rt.cache.touch(&path).await;
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
            index: index + part.map_or(0, |p| p.first_segment),
            waited_ms,
            cache_hit,
            cmcd_raw: cmcd(headers, q),
        });
    }
    let ts = path.extension().is_some_and(|e| e == "ts");
    let ct = match (ts, part.is_some()) {
        (true, false) => "video/MP2T",
        (true, true) => "audio/MP2T",
        (false, false) => "video/mp4",
        (false, true) => "audio/mp4",
    };
    (
        [
            (header::CONTENT_TYPE, ct),
            (header::CACHE_CONTROL, "no-store"),
            (header::CONTENT_LENGTH, &len.to_string()),
        ],
        body,
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

pub(super) fn seg_name(seg: &str) -> Result<SegmentName, Response> {
    parse_segment_name(seg).ok_or_else(|| {
        text(
            StatusCode::BAD_REQUEST,
            &format!("Invalid segment name: {seg}"),
        )
    })
}

/// [ts:modules/streaming/streaming.controller.ts:2245-2545]
pub(super) async fn segment(
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
    let wait = WaitGuard::new(&rt.events, Some(&s), &s.sid, Track::Video, &quality, index);
    let fetch = if remux {
        rt.remux_file(&s, 0, name).await
    } else {
        rt.video_file(&s, &quality, name).await
    };
    wait.done();
    respond(
        &rt,
        &s,
        fetch,
        name,
        (Track::Video, &quality, remux),
        None,
        &headers,
        &q,
    )
    .await
}

/// [ts:modules/streaming/streaming.controller.ts:1909-2120]
pub(super) async fn audio_seg(
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
    let rung = format!("audio-{idx}");
    let wait = WaitGuard::new(&rt.events, Some(&s), &s.sid, Track::Audio, &rung, index);
    let fetch = if remux {
        rt.remux_file(&s, track, name).await
    } else {
        rt.audio_file(&s, track, name).await
    };
    wait.done();
    respond(
        &rt,
        &s,
        fetch,
        name,
        (Track::Audio, &rung, remux),
        None,
        &headers,
        &q,
    )
    .await
}

#[cfg(test)]
mod wait_guard_tests {
    use super::*;

    fn guard(events: &Events) -> WaitGuard<'_> {
        WaitGuard::new(events, None, "sid", Track::Video, "720p", Some(7))
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_dropped_after_a_long_wait_is_reported() {
        let events = Events::default();
        let mut rx = events.subscribe();
        let g = guard(&events);
        tokio::time::advance(Duration::from_secs(3)).await;
        drop(g);
        match rx.try_recv().unwrap() {
            EngineEvent::SegmentAbandoned {
                session_id,
                index,
                waited_ms,
                ..
            } => {
                assert_eq!((session_id.as_str(), index), ("sid", 7));
                assert!(waited_ms >= 3000.0);
            }
            e => panic!("unexpected {e:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_short_wait_or_a_finished_request_is_silent() {
        let events = Events::default();
        let mut rx = events.subscribe();
        drop(guard(&events));
        let g = guard(&events);
        tokio::time::advance(Duration::from_secs(3)).await;
        g.done();
        assert!(rx.try_recv().is_err());
    }
}
