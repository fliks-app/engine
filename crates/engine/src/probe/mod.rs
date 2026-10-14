//! `POST /v1/probe`: runs ffprobe, mkvmerge and ffmpeg for the pure parser in `core::probe`.
//! Spec: `docs/spec/probe.md`.
mod run;

use crate::AppState;
use axum::{Json, extract::State, http::StatusCode};
use fliks_engine_core::api::{MEDIA_INFO_SCHEMA_VERSION, MovieHash, ProbeRequest, ProbeResult};
use fliks_engine_core::hw::{DEFAULT_RENDER_NODE, platform_from_os};
use fliks_engine_core::probe::{self, ProbeOpts, args};
use run::ProcessTools;
use std::path::Path;
use std::sync::Arc;
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
pub async fn probe(
    State(s): State<Arc<AppState>>,
    Json(req): Json<ProbeRequest>,
) -> Result<Json<ProbeResult>, StatusCode> {
    let path = req.media.path;
    // A relative path could read as an option of the tools.
    if !Path::new(&path).is_absolute() {
        return Err(StatusCode::BAD_REQUEST);
    }
    // Before the first detection ends the host's own platform and default node stand in.
    let (platform, node) = s.hw.snapshot().map_or_else(
        || {
            (
                platform_from_os(std::env::consts::OS),
                DEFAULT_RENDER_NODE.into(),
            )
        },
        |h| (h.detected.platform, h.render_node),
    );
    let opts = ProbeOpts {
        crop: req.crop,
        mkv_tags: req.mkv_tags,
        crop_decode: req
            .crop
            .then(|| {
                let (node_up, nvidia) = (Path::new(&node).exists(), Path::new("/dev/nvidiactl"));
                args::crop_hw_decode(platform, &node, node_up, nvidia.exists())
            })
            .flatten(),
    };
    let mut out = probe::probe(&ProcessTools::new(&path), &path, &opts).await;
    if let Some(used) = &out.crop_decode {
        let asked = opts.crop_decode.as_ref().and_then(|a| a.get(1));
        if asked.is_some_and(|a| a != used) {
            tracing::info!(
                path,
                "cropdetect: hw decode rejected this file, software fallback"
            );
        }
        tracing::info!(path, decode = %used, "cropdetect");
    }
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
