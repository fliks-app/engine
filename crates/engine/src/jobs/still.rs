//! The `Still` job: one frame of a video file, written to the engine cache by one ffmpeg run and
//! read with `GET /v1/stills/{mediaFileId}`. Spec: `docs/spec/jobs.md` section 9b.
use super::Ctx;
use super::cover::respond;
use crate::AppState;
use axum::extract::{Path as UrlPath, State};
use axum::response::Response;
use fliks_engine_core::api::{JobResult, MediaInfo, StillMeta};
use fliks_engine_core::jobs::cover::MAX_BYTES;
use fliks_engine_core::jobs::still::*;
use std::sync::Arc;

pub async fn run(ctx: &Ctx, info: &MediaInfo, at: Option<f64>) -> Result<JobResult, String> {
    let at = validate(info, at)?;
    let dir = ctx.rt.cache.still_dir(ctx.job.req.media.media_file_id);
    ctx.check_disk(&dir).await?;
    let tmp = dir.join(format!("still-tmp-{}.jpg", crate::random_hex(4)));
    let argv = capture_args(&ctx.media_path(), info, at, &tmp.to_string_lossy());
    let ran = ctx.ffmpeg(&argv).await;
    let bytes = tokio::fs::metadata(&tmp).await.map_or(0, |m| m.len());
    let verdict = match ran {
        Err(e) => Err(e),
        Ok(()) if bytes == 0 => Err("ffmpeg wrote no picture".into()),
        Ok(()) if bytes > MAX_BYTES => Err("still too large".into()),
        Ok(()) => Ok(()),
    };
    if let Err(e) = verdict {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    tokio::fs::rename(&tmp, dir.join(FILE))
        .await
        .map_err(|e| format!("write still: {e}"))?;
    Ok(JobResult::Still {
        still: StillMeta {
            mime: MIME.into(),
            bytes: bytes as u32,
            at_seconds: at,
        },
    })
}

#[utoipa::path(get, path = "/v1/stills/{mediaFileId}", operation_id = "get_still", responses(
    (status = 200, content_type = "image/jpeg", description = "the frame the last `Still` job stored"),
    (status = 404, description = "no stored still")))]
pub async fn get(State(s): State<Arc<AppState>>, UrlPath(id): UrlPath<u32>) -> Response {
    let path = s.rt.cache.still_dir(id).join(FILE);
    respond(path.is_file().then_some(path)).await
}
