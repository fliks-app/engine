//! The `Cover` job: the embedded picture of an audio file, copied out by one ffmpeg run into the
//! engine cache and read with `GET /v1/covers/{mediaFileId}`. Spec: `docs/spec/jobs.md` section 9.
use super::Ctx;
use crate::AppState;
use axum::extract::{Path as UrlPath, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use fliks_engine_core::api::{CoverMeta, JobResult, MediaInfo};
use fliks_engine_core::jobs::cover::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The stored cover of a file: its path and size.
pub async fn stored(dir: &Path) -> Option<(PathBuf, u64)> {
    for name in FILES {
        let path = dir.join(name);
        if let Ok(m) = tokio::fs::metadata(&path).await {
            return Some((path, m.len()));
        }
    }
    None
}

fn meta(path: &Path, bytes: u64) -> JobResult {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    JobResult::Cover {
        cover: CoverMeta {
            mime: mime(&name).into(),
            bytes: bytes.min(u64::from(u32::MAX)) as u32,
        },
    }
}

pub async fn run(ctx: &Ctx, info: &MediaInfo) -> Result<JobResult, String> {
    let cover = validate(info)?;
    let dir = ctx.rt.cache.cover_dir(ctx.job.req.media.media_file_id);
    if !ctx.job.req.force
        && let Some((path, bytes)) = stored(&dir).await
    {
        return Ok(meta(&path, bytes));
    }
    ctx.check_disk(&dir).await?;
    let ext = extension(cover);
    let tmp = dir.join(format!("cover-tmp.{ext}"));
    let argv = extract_args(&ctx.media_path(), cover, &tmp.to_string_lossy());
    let ran = ctx.ffmpeg(&argv).await;
    let bytes = match ran {
        Ok(()) => tokio::fs::metadata(&tmp)
            .await
            .map(|m| m.len())
            .unwrap_or(0),
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
    };
    if bytes == 0 {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err("ffmpeg wrote no picture".into());
    }
    for name in FILES {
        let _ = tokio::fs::remove_file(dir.join(name)).await;
    }
    let path = dir.join(format!("cover.{ext}"));
    tokio::fs::rename(&tmp, &path)
        .await
        .map_err(|e| format!("write cover: {e}"))?;
    Ok(meta(&path, bytes))
}

#[utoipa::path(get, path = "/v1/covers/{mediaFileId}", operation_id = "get_cover", responses(
    (status = 200, content_type = "image/jpeg", description = "the picture a `Cover` job stored (`image/png` for a PNG)"),
    (status = 404, description = "no stored cover")))]
pub async fn get(State(s): State<Arc<AppState>>, UrlPath(id): UrlPath<u32>) -> Response {
    let Some((path, _)) = stored(&s.rt.cache.cover_dir(id)).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    match tokio::fs::read(&path).await {
        Ok(b) => ([(header::CONTENT_TYPE, mime(&name))], b).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
