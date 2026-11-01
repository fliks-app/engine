//! The `Cover` job: the embedded picture of an audio file, copied out by one ffmpeg run into the
//! engine cache and read with `GET /v1/covers/{mediaFileId}`. Spec: `docs/spec/jobs.md` section 9.
use super::Ctx;
use crate::AppState;
use crate::serve::stream_body;
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

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn meta(path: &Path, bytes: u64) -> JobResult {
    JobResult::Cover {
        cover: CoverMeta {
            mime: mime(&file_name(path)).into(),
            bytes: bytes as u32,
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
    let tmp = dir.join(format!("cover-tmp-{}.{ext}", crate::random_hex(4)));
    let argv = extract_args(&ctx.media_path(), cover, &tmp.to_string_lossy());
    let ran = ctx.ffmpeg(&argv).await;
    let bytes = tokio::fs::metadata(&tmp).await.map_or(0, |m| m.len());
    let verdict = match ran {
        Err(e) => Err(e),
        Ok(()) if bytes == 0 => Err("ffmpeg wrote no picture".into()),
        Ok(()) if bytes > MAX_BYTES => Err("cover too large".into()),
        Ok(()) => Ok(()),
    };
    if let Err(e) = verdict {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    let path = dir.join(format!("cover.{ext}"));
    tokio::fs::rename(&tmp, &path)
        .await
        .map_err(|e| format!("write cover: {e}"))?;
    // The other format only goes once the new file is in place.
    for name in FILES.iter().filter(|n| **n != file_name(&path)) {
        let _ = tokio::fs::remove_file(dir.join(name)).await;
    }
    Ok(meta(&path, bytes))
}

#[utoipa::path(get, path = "/v1/covers/{mediaFileId}", operation_id = "get_cover", responses(
    (status = 200, content_type = "image/jpeg", description = "the picture a `Cover` job stored (`image/png` for a PNG)"),
    (status = 404, description = "no stored cover")))]
pub async fn get(State(s): State<Arc<AppState>>, UrlPath(id): UrlPath<u32>) -> Response {
    let Some((path, _)) = stored(&s.rt.cache.cover_dir(id)).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(file) = tokio::fs::File::open(&path).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [(header::CONTENT_TYPE, mime(&file_name(&path)))],
        stream_body(file, 64 * 1024),
    )
        .into_response()
}
