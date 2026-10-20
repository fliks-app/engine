//! Direct play, byte ranges and the sprite sheet.

use super::{Q, S, access, encode, file_id, stream_body, text};
use axum::body::Body;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use fliks_engine_core::api::ResolvePurpose;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

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

pub(super) fn content_type(path: &str, format: Option<&str>) -> String {
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

pub(super) fn attachment(name: &str) -> String {
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

pub(super) fn base_name(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

/// Streams `[start, end]` of a file.
async fn file_body(path: &str, start: u64, len: u64) -> std::io::Result<Body> {
    let mut f = tokio::fs::File::open(path).await?;
    f.seek(std::io::SeekFrom::Start(start)).await?;
    Ok(stream_body(f.take(len), 64 * 1024))
}

/// `path` whole, or the part of it a `Range` header asks for; `res` carries the route's headers.
pub(super) async fn ranged_file(
    mut res: axum::http::response::Builder,
    headers: &HeaderMap,
    path: &str,
    size: u64,
) -> std::io::Result<Response> {
    let (status, start, len) = match headers.get(header::RANGE).and_then(|r| r.to_str().ok()) {
        None => (StatusCode::OK, 0, size),
        Some(r) => match parse_range(r, size) {
            Some((s, e)) => {
                res = res.header(header::CONTENT_RANGE, format!("bytes {s}-{e}/{size}"));
                (StatusCode::PARTIAL_CONTENT, s, e - s + 1)
            }
            None => {
                return Ok((
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    [(header::CONTENT_RANGE, format!("bytes */{size}"))],
                )
                    .into_response());
            }
        },
    };
    let body = file_body(path, start, len).await?;
    Ok(res
        .status(status)
        .header(header::CONTENT_LENGTH, len)
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()))
}

/// Direct play and `?download=1`. [ts:modules/streaming/streaming.controller.ts:2555-2641]
pub(super) async fn direct(
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
    let path = rt.map_media_path(&a.resolved.path);
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
    ranged_file(res, &headers, &path, size)
        .await
        .unwrap_or_else(|e| {
            tracing::error!(path, "direct play read: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })
}

/// The engine's `sprite.json` / `sprite.jpg`, else the ones next to the resolver's `sprite.jpg`. [ts:modules/streaming/streaming.controller.ts:1363-1395]
pub(super) async fn sprite(
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
