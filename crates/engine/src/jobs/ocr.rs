//! The `SubtitleOcr` job: extract a bitmap track, OCR to SRT. Spec: `docs/spec/subtitle-jobs.md` section 2.
use super::{Cmd, Ctx, tail};
use crate::proc::Pipes;
use fliks_engine_core::api::{JobResult, MediaInfo};
use fliks_engine_core::jobs::ocr as pure;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn validate(
    info: &MediaInfo,
    stream_index: u32,
    language: Option<&str>,
) -> Result<(), String> {
    pure::validate_ocr(info, stream_index, language)?;
    Ok(())
}

fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            #[cfg(windows)]
            {
                dir.join(format!("{name}.exe")).is_file() || dir.join(name).is_file()
            }
            #[cfg(not(windows))]
            {
                dir.join(name).is_file()
            }
        })
    })
}

pub fn announced() -> bool {
    on_path("pgsrip") || (on_path("subtile-ocr") && on_path("mkvextract"))
}

async fn remove_tree(dir: &Path) {
    let _ = tokio::fs::remove_dir_all(dir).await;
}

pub async fn run(
    ctx: &Ctx,
    info: &MediaInfo,
    stream_index: u32,
    language: Option<&str>,
) -> Result<JobResult, String> {
    let track = pure::validate_ocr(info, stream_index, language)?;
    let media = ctx.media_path();
    let ietf = pure::resolved_language(language, &track.language);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let stage = ctx
        .rt
        .cache
        .root
        .join("tmp")
        .join("ocr")
        .join(&ctx.job.id)
        .join(format!("ocr-{stream_index}-{nanos}"));
    let base = stage.join(format!("ocr-{stream_index}-{nanos}"));
    let base_str = base.to_string_lossy().into_owned();
    let prefix = base
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    ctx.check_disk(&stage).await?;
    tokio::fs::create_dir_all(&stage)
        .await
        .map_err(|e| e.to_string())?;
    let out = match track.codec.as_str() {
        "hdmv_pgs_subtitle" => {
            run_pgs(ctx, &media, stream_index, &ietf, &stage, &prefix, &base_str).await
        }
        "dvd_subtitle" => run_vobsub(ctx, &media, stream_index, &ietf, &base_str).await,
        c => Err(format!("OCR is not available for \"{c}\" subtitles")),
    };
    remove_tree(&stage).await;
    out.map(|srt| JobResult::SubtitleOcr { srt })
}

async fn run_pgs(
    ctx: &Ctx,
    media: &str,
    stream_index: u32,
    ietf: &str,
    stage: &Path,
    prefix: &str,
    base: &str,
) -> Result<String, String> {
    let sup = pure::pgs_sup_path(base, ietf);
    let extract = pure::pgs_extract_argv(media, stream_index, &sup);
    let cmd = Cmd::new("ffmpeg", &ctx.rt.ffmpeg, &extract, Pipes::default());
    ctx.pipeline(pure::EXTRACT_TIMEOUT_MS, vec![cmd], |p| async move {
        match p[0].wait().await.code {
            Some(0) => Ok(()),
            code => Err(format!("ffmpeg exited with {code:?}: {}", tail(&p[0], 2))),
        }
    })
    .await??;
    ctx.progress(20.0);
    let rip = pure::pgsrip_argv(ietf, &sup);
    let cmd = Cmd::new("pgsrip", "pgsrip", &rip, Pipes::default());
    let tool_out = ctx
        .pipeline(pure::OCR_TOOL_TIMEOUT_MS, vec![cmd], |p| async move {
            let proc = &p[0];
            let code = proc.wait().await.code;
            let stderr = proc.stderr_tail().join(" ");
            match code {
                Some(0) => Ok(stderr),
                c => Err(format!("pgsrip exited with {c:?}: {}", tail(proc, 2))),
            }
        })
        .await??;
    match find_srt(stage, prefix).await {
        Some(path) => tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| format!("read {}: {e}", path.display())),
        None => Err(pure::pgs_missing_srt_error(ietf, &tool_out, "")),
    }
}

async fn run_vobsub(
    ctx: &Ctx,
    media: &str,
    stream_index: u32,
    ietf: &str,
    base: &str,
) -> Result<String, String> {
    pure::require_matroska_vobsub(media)?;
    let idx = format!("{base}.idx");
    let sub = format!("{base}.sub");
    let extract = pure::mkvextract_argv(media, stream_index, &idx);
    let cmd = Cmd::new("mkvextract", "mkvextract", &extract, Pipes::default());
    ctx.pipeline(pure::EXTRACT_TIMEOUT_MS, vec![cmd], |p| async move {
        let code = p[0].wait().await.code;
        if code == Some(0) || Path::new(&sub).is_file() {
            Ok(())
        } else {
            Err(format!("mkvextract exited with {code:?}: {}", tail(&p[0], 2)))
        }
    })
    .await??;
    ctx.progress(20.0);
    let pack = pure::tesseract_pack(ietf);
    let ocr = pure::subtile_ocr_argv(pack, base);
    let cmd = Cmd::new("subtile-ocr", "subtile-ocr", &ocr, Pipes::default());
    ctx.pipeline(pure::OCR_TOOL_TIMEOUT_MS, vec![cmd], |p| async move {
        match p[0].wait().await.code {
            Some(0) => Ok(()),
            code => Err(format!("subtile-ocr exited with {code:?}: {}", tail(&p[0], 2))),
        }
    })
    .await??;
    tokio::fs::read_to_string(format!("{base}.srt"))
        .await
        .map_err(|e| format!("read {base}.srt: {e}"))
}

async fn find_srt(stage: &Path, prefix: &str) -> Option<PathBuf> {
    let mut rd = tokio::fs::read_dir(stage).await.ok()?;
    while let Ok(Some(e)) = rd.next_entry().await {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with(prefix) && name.ends_with(".srt") {
            return Some(e.path());
        }
    }
    None
}
