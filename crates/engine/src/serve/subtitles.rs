//! Embedded and external subtitles: WebVTT conversion, downloads and lists.

use super::{
    Access, Q, S, access, attachment, base_name, file_id, playlist, prefix, run_capture, text,
    token_param,
};
use crate::runtime::Runtime;
use axum::body::Body;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use fliks_engine_core::api::ResolvePurpose;
use fliks_engine_core::hls::subtitle_playlist;
use fliks_engine_core::mp4::cue_offset_seconds;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

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
    let cached = || tokio::fs::read_to_string(vtt_path(&dir, idx));
    if let Ok(t) = cached().await {
        return Some(t);
    }
    // Concurrent requests for one stream wait for the first extraction, then read its file.
    let lock = rt.lock_for(&format!("subtitle:{id}:{idx}"));
    let _held = lock.lock().await;
    if let Ok(t) = cached().await {
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
    let args = fliks_engine_core::jobs::subtitles::webvtt_args(path);
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
pub(super) async fn embedded_vtt(
    State(rt): S,
    UrlPath((id, idx)): UrlPath<(String, u32)>,
    headers: HeaderMap,
    Query(q): Q,
) -> Response {
    let (id, a) = match subtitle_access(&rt, &id, &headers, &q, (None, Some(idx))).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let media = rt.map_media_path(&a.resolved.path);
    match embedded_text(&rt, id, idx, &media).await {
        Some(t) => vtt_response(with_timestamp_map(&t, cue_offset(&rt, id, &a)), None),
        None => text(StatusCode::NOT_FOUND, "Subtitle stream not found"),
    }
}

/// [ts:modules/streaming/streaming.controller.ts:1715-1746]
pub(super) async fn embedded_download(
    State(rt): S,
    UrlPath((id, idx)): UrlPath<(String, u32)>,
    headers: HeaderMap,
    Query(q): Q,
) -> Response {
    let (id, a) = match subtitle_access(&rt, &id, &headers, &q, (None, Some(idx))).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let media = rt.map_media_path(&a.resolved.path);
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
pub(super) async fn external_vtt(
    State(rt): S,
    UrlPath((id, sub)): UrlPath<(String, u32)>,
    headers: HeaderMap,
    Query(q): Q,
) -> Response {
    let (id, a) = match subtitle_access(&rt, &id, &headers, &q, (Some(sub), None)).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let path = rt.map_media_path(&a.resolved.path);
    match external_text(&rt, &path, a.resolved.format.as_deref()).await {
        Some(t) => vtt_response(with_timestamp_map(&t, cue_offset(&rt, id, &a)), None),
        None => text(StatusCode::NOT_FOUND, "Subtitle not found"),
    }
}

/// The original file, as stored. [ts:modules/streaming/streaming.controller.ts:1748-1763]
pub(super) async fn external_download(
    State(rt): S,
    UrlPath((id, sub)): UrlPath<(String, u32)>,
    headers: HeaderMap,
    Query(q): Q,
) -> Response {
    let (_, a) = match subtitle_access(&rt, &id, &headers, &q, (Some(sub), None)).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let path = rt.map_media_path(&a.resolved.path);
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

pub(super) async fn embedded_list(
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

pub(super) async fn external_list(
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
