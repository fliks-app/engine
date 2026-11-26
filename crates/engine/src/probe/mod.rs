//! `POST /v1/probe`: runs ffprobe, mkvmerge and ffmpeg for the pure parser in `core::probe`.
//! Spec: `docs/spec/probe.md`.
mod audio_tags;
pub(crate) mod run;

use crate::AppState;
use axum::{Json, extract::State, http::StatusCode};
use fliks_engine_core::api::{MovieHash, ProbeRequest, ProbeResult};
use fliks_engine_core::probe::{self, ProbeOpts};
use run::probe_stamped;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// Host path rewritten by `pathMappings`. Both sides stay absolute: a relative one is an argv option.
fn resolve_probe_path(rt: &crate::runtime::Runtime, host: &str) -> Result<String, StatusCode> {
    rt.map_absolute(host).ok_or(StatusCode::BAD_REQUEST)
}

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
    responses((status = 200, body = ProbeResult, description = "the probe result"), (status = 400, description = "path is not absolute")))]
pub async fn probe(
    State(s): State<Arc<AppState>>,
    Json(req): Json<ProbeRequest>,
) -> Result<Json<ProbeResult>, StatusCode> {
    let path = resolve_probe_path(&s.rt, &req.media.path)?;
    let (host, render_node) = s.hw.host();
    let opts = ProbeOpts {
        crop: req.crop,
        mkv_tags: req.mkv_tags,
        host,
        render_node,
    };
    let out = probe_stamped(&path, &s.rt, &opts).await;
    if out.crop_hw_refused {
        tracing::info!(
            path,
            "cropdetect: hw decode rejected this file, software fallback"
        );
    }
    if let Some(used) = &out.crop_decode {
        tracing::info!(path, decode = %used, "cropdetect");
    }
    if !out.ok {
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

#[cfg(test)]
mod tests {
    use super::resolve_probe_path;
    use crate::runtime::{FOREIGN, Runtime};

    #[test]
    fn probe_opens_the_mapped_path() {
        let (from, to, host, mapped) = if cfg!(windows) {
            (r"C:\nas", r"D:\data", r"C:\nas\a.mkv", r"D:\data\a.mkv")
        } else {
            (
                "/mnt/nas",
                "/data/server",
                "/mnt/nas/a.mkv",
                "/data/server/a.mkv",
            )
        };
        let opened = Runtime::with_mapping(from, to);
        assert_eq!(resolve_probe_path(&opened, host).unwrap(), mapped);
        assert!(resolve_probe_path(&opened, "rel.mkv").is_err());
        let relative = Runtime::with_mapping(from, "relative");
        assert!(resolve_probe_path(&relative, host).is_err());
    }

    #[test]
    fn a_foreign_platform_path_is_mapped_before_the_absolute_check() {
        let (from, to, path) = FOREIGN;
        let r = Runtime::with_mapping(from, to);
        assert_eq!(resolve_probe_path(&r, path).unwrap(), format!("{to}/x.mkv"));
        assert!(resolve_probe_path(&r, "media/x.mkv").is_err());
        assert!(resolve_probe_path(&Runtime::with_mapping(from, "relative"), path).is_err());
    }
}
