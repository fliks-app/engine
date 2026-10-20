//! Live TV playlists and segments.

use super::{Q, S, cmcd, credential, playlist, ranged_file, text};
use crate::resolve::{Ask, Host, ResolveError};
use crate::runtime::Runtime;
use axum::body::Body;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use fliks_engine_core::api::{EngineEvent, ResolveMediaId, ResolvePurpose, Track};
use std::collections::HashMap;
use std::sync::Arc;

/// Nest's error body.
fn nest(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({
        "statusCode": status.as_u16(),
        "message": message,
        "error": status.canonical_reason().unwrap_or(""),
    });
    (status, axum::Json(body)).into_response()
}

/// Unknown sid 404 before any resolver call, then `purpose: live`: a denial for the viewer's owner ends it.
/// [ts:modules/livetv/controllers/livetv.controller.ts:201-215]
async fn live_access(
    rt: &Arc<Runtime>,
    sid: &str,
    headers: &HeaderMap,
    q: &HashMap<String, String>,
    unknown: &str,
) -> Result<Arc<crate::live::Pipe>, Response> {
    let Some(pipe) = rt.live.lookup(sid) else {
        return Err(nest(StatusCode::NOT_FOUND, unknown));
    };
    let config = rt.current_config();
    let host = config.host_url.clone().map(|url| Host {
        url,
        token: config.host_token.clone(),
    });
    let cred = credential(headers, q);
    let ask = Ask {
        media: ResolveMediaId::Live(sid.into()),
        credential: &cred,
        purpose: ResolvePurpose::Live,
        subtitle_id: None,
        stream_index: None,
    };
    match rt.resolver.resolve(host.as_ref(), ask).await {
        Ok(r) if pipe.owner_of(sid).is_some_and(|o| o != r.owner) => {
            Err(text(StatusCode::FORBIDDEN, "Forbidden"))
        }
        Err(ResolveError::Denied { owner }) => {
            // Only a denial for the viewer's owner ends it; any other just refuses the request.
            if owner.is_some() && pipe.owner_of(sid) == Some(owner) {
                rt.live.leave(rt, sid, "revoked");
            }
            Err(text(StatusCode::FORBIDDEN, "Forbidden"))
        }
        // Host down: a known viewer keeps watching.
        Ok(_) | Err(ResolveError::Unavailable) => {
            rt.live.touch(&pipe, sid);
            Ok(pipe)
        }
    }
}

/// ffmpeg's live playlist, every URI carrying the request's token. [ts:modules/livetv/controllers/livetv.controller.ts:245-266]
pub(super) async fn live_playlist(
    State(rt): S,
    UrlPath(sid): UrlPath<String>,
    headers: HeaderMap,
    Query(q): Q,
) -> Response {
    let pipe = match live_access(&rt, &sid, &headers, &q, "Live TV session not found").await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let raw = match pipe.dir() {
        Some(d) => tokio::fs::read_to_string(d.join("index.m3u8")).await.ok(),
        None => None,
    };
    match raw {
        Some(raw) => playlist(fliks_engine_core::live::with_token(
            &raw,
            q.get("token").map(String::as_str),
        )),
        None => nest(StatusCode::NOT_FOUND, "Playlist not ready yet"),
    }
}

/// A file of the live window, ranges honoured like Express's `sendFile`. [ts:modules/livetv/controllers/livetv.controller.ts:292-318]
pub(super) async fn live_segment(
    State(rt): S,
    UrlPath((sid, name)): UrlPath<(String, String)>,
    headers: HeaderMap,
    Query(q): Q,
) -> Response {
    const MISSING: &str = "Live TV segment not found";
    let pipe = match live_access(&rt, &sid, &headers, &q, MISSING).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let Some(dir) = pipe.dir() else {
        return nest(StatusCode::NOT_FOUND, MISSING);
    };
    if !fliks_engine_core::live::is_segment_name(&name) {
        return nest(
            StatusCode::BAD_REQUEST,
            &format!("Invalid segment name: {name}"),
        );
    }
    let path = dir.join(&name).to_string_lossy().into_owned();
    let Ok(md) = tokio::fs::metadata(&path).await else {
        return nest(StatusCode::NOT_FOUND, MISSING);
    };
    if let Some(index) = fliks_engine_core::live::segment_number(&name) {
        rt.events.emit(EngineEvent::SegmentServed {
            session_id: sid,
            track: Track::Video,
            rung: "live".into(),
            index,
            waited_ms: 0.0,
            cache_hit: true,
            cmcd_raw: cmcd(&headers, &q),
        });
    }
    let size = md.len();
    let ct = if name.ends_with(".ts") {
        "video/mp2t"
    } else {
        "video/mp4"
    };
    let res = Response::builder()
        .header(header::CONTENT_TYPE, ct)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "public, max-age=0");
    // ffmpeg may slide the window between the stat and the open.
    ranged_file(res, &headers, &path, size)
        .await
        .unwrap_or_else(|_| nest(StatusCode::NOT_FOUND, MISSING))
}

/// The shared upstream of a direct session, from now on. [ts:modules/livetv/controllers/livetv.controller.ts:268-290]
pub(super) async fn live_direct(
    State(rt): S,
    UrlPath(sid): UrlPath<String>,
    headers: HeaderMap,
    Query(q): Q,
) -> Response {
    const MISSING: &str = "Live TV session not found";
    let pipe = match live_access(&rt, &sid, &headers, &q, MISSING).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    if pipe.dir().is_some() {
        return nest(StatusCode::NOT_FOUND, MISSING);
    }
    let (rx, content_type) = pipe.attach(&sid);
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|c| (Ok::<_, std::io::Error>(c), rx))
    });
    (
        [
            (header::CACHE_CONTROL, "no-store".to_string()),
            (header::CONTENT_TYPE, content_type),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}
