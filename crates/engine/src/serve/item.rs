//! An item's HLS playlist and segments: the global numbering of the playlist mapped onto the
//! per-part runs. Spec: `docs/spec/item.md`.

use super::hls::{PartBase, WaitGuard, respond, seg_name};
use super::{Q, S, file_id, hls, playlist, prefix, text, token_param};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use fliks_engine_core::api::Track;
use fliks_engine_core::hls::{SegUrl, variable_vod_playlist};
use fliks_engine_core::session::SegmentName;

/// `{id}/item/index.m3u8`: every part's segments on one timeline.
pub(super) async fn list(
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
    let Some(item) = &s.plan.item else {
        return text(StatusCode::NOT_FOUND, "Not an HLS item");
    };
    let token = token_param(&q);
    let base = format!("{}/item", prefix(id));
    let seg_prefix = format!("{base}/seg-");
    let ext = if s.plan.use_ts { "ts" } else { "m4s" };
    let suffix = format!(".{ext}{token}");
    let init = (!s.plan.use_ts).then(|| format!("{base}/init.mp4{token}"));
    let at = q.get("startAt").and_then(|v| v.parse().ok());
    rt.prewarm_item(&s, at.unwrap_or_else(|| s.position_s()));
    playlist(variable_vod_playlist(
        &item.timeline.durations(),
        SegUrl {
            prefix: &seg_prefix,
            suffix: &suffix,
        },
        init.as_deref(),
    ))
}

/// `{id}/item/init.mp4` and `{id}/item/seg-N`, `N` counted over the whole item.
pub(super) async fn file(
    State(rt): S,
    UrlPath((id, seg)): UrlPath<(String, String)>,
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
    let global = match name {
        SegmentName::Segment(n) => Some(n),
        SegmentName::Init(_) => None,
    };
    let (s, _) = match hls(&rt, id, &headers, &q, global).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(item) = &s.plan.item else {
        return text(StatusCode::NOT_FOUND, "Not an HLS item");
    };
    let (part, local_name) = match global {
        Some(n) => match item.timeline.segment(n) {
            Some((p, local)) => (p, SegmentName::Segment(local)),
            None => return text(StatusCode::NOT_FOUND, "Segment not found"),
        },
        None => match item.timeline.locate(s.position_s()) {
            Some(l) => (l.part, name),
            None => return text(StatusCode::NOT_FOUND, "Item has no duration"),
        },
    };
    let span = &item.timeline.parts[part];
    let base = PartBase {
        offset_s: span.offset_s,
        first_segment: span.first_segment,
    };
    let wait = WaitGuard::new(&rt.events, Some(&s), &s.sid, Track::Audio, "item", global);
    let fetch = rt.item_file(&s, part, local_name).await;
    wait.done();
    respond(
        &rt,
        &s,
        fetch,
        local_name,
        (Track::Audio, "item", false),
        Some(base),
        &headers,
        &q,
    )
    .await
}
