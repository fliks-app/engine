//! `POST /v1/probe`: runs ffprobe, mkvmerge and ffmpeg for the pure parser in `core::probe`.
//! Spec: `docs/spec/probe.md`.
mod run;

use axum::{Json, http::StatusCode};
use fliks_engine_core::api::{MEDIA_INFO_SCHEMA_VERSION, MovieHash, ProbeRequest, ProbeResult};
use fliks_engine_core::probe::{self, ProbeOpts};
use run::ProcessTools;
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// First and last 64 KiB of the file for the moviehash; `None` when unreadable or under 128 KiB.
async fn moviehash(path: &str) -> Option<MovieHash> {
    use probe::moviehash::CHUNK;
    let mut file = tokio::fs::File::open(path).await.ok()?;
    let size = file.metadata().await.ok()?.len();
    if size < (CHUNK * 2) as u64 {
        return None;
    }
    let (mut head, mut tail) = (vec![0u8; CHUNK], vec![0u8; CHUNK]);
    file.read_exact(&mut head).await.ok()?;
    file.seek(std::io::SeekFrom::Start(size - CHUNK as u64))
        .await
        .ok()?;
    file.read_exact(&mut tail).await.ok()?;
    Some(MovieHash {
        hash: probe::moviehash(size, &head, &tail)?,
        bytesize: size as f64,
    })
}

#[utoipa::path(post, path = "/v1/probe", request_body = ProbeRequest,
    responses((status = 200, body = ProbeResult), (status = 400, description = "path is not absolute")))]
pub async fn probe(Json(req): Json<ProbeRequest>) -> Result<Json<ProbeResult>, StatusCode> {
    let path = req.media.path;
    // A relative path could read as an option of the tools.
    if !Path::new(&path).is_absolute() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let opts = ProbeOpts {
        crop: req.crop,
        mkv_tags: req.mkv_tags,
    };
    let mut out = probe::probe(&ProcessTools::new(&path), &path, &opts).await;
    if out.ok {
        out.info.schema_version = Some(MEDIA_INFO_SCHEMA_VERSION);
    } else {
        tracing::warn!(path, error = ?out.error, "ffprobe could not read the file");
    }
    Ok(Json(ProbeResult {
        ok: out.ok,
        info: Some(out.info),
        moviehash: if req.moviehash {
            moviehash(&path).await
        } else {
            None
        },
        error: out.error,
    }))
}
