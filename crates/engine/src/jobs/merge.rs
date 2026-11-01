//! The `Merge` job: the parts of an audiobook into one `.m4b`, one ffmpeg run, written beside its
//! final name and published without overwriting. Spec: `docs/spec/jobs.md` section 10.
use super::{Cmd, Ctx, tail};
use crate::proc::{Ffmpeg, Pipes};
use fliks_engine_core::api::{Chapter, JobResult, MediaTags, MergeMeta, MergePart};
use fliks_engine_core::jobs::merge::*;
use fliks_engine_core::mp4::{add_freeform_items, read_box_header};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, BufReader};

pub async fn run(
    ctx: &Ctx,
    parts: &[MergePart],
    output: &str,
    over: Option<&MediaTags>,
) -> Result<JobResult, String> {
    let out = PathBuf::from(ctx.rt.map_media_path(output));
    let (dir, name) = out
        .parent()
        .zip(out.file_name())
        .ok_or("output has no directory")?;
    if exists(&out).await {
        return Err(taken(&out));
    }
    ctx.check_disk(dir).await?;
    let tmp = dir.join(format!(
        ".{}.merge-{}.tmp",
        name.to_string_lossy(),
        crate::random_hex(4)
    ));
    let scratch = ctx.rt.cache.merge_dir(&ctx.job.id);
    let built = build(ctx, parts, over, &scratch, &tmp).await;
    let _ = tokio::fs::remove_dir_all(&scratch).await;
    let published = match built {
        Ok(r) => publish(&tmp, &out).await.map(|()| r),
        Err(e) => Err(e),
    };
    let _ = tokio::fs::remove_file(&tmp).await;
    let (chapters, reencoded) = published?;
    let bytes = tokio::fs::metadata(&out).await.map_or(0, |m| m.len());
    Ok(JobResult::Merge {
        merge: MergeMeta {
            path: output.into(),
            bytes: bytes as f64,
            duration_seconds: total_seconds(parts),
            chapters,
            reencoded,
        },
    })
}

async fn exists(p: &Path) -> bool {
    tokio::fs::try_exists(p).await.unwrap_or(false)
}

fn taken(out: &Path) -> String {
    format!("output exists: {}", out.display())
}

/// The finished temporary file: chapters of the merged timeline and whether the audio was encoded.
async fn build(
    ctx: &Ctx,
    parts: &[MergePart],
    over: Option<&MediaTags>,
    scratch: &Path,
    tmp: &Path,
) -> Result<(Vec<Chapter>, bool), String> {
    let (tags, chapters, encoding) = (merged_tags(parts, over), chapters(parts), encoding(parts));
    let paths: Vec<String> = parts
        .iter()
        .map(|p| ctx.rt.map_media_path(&p.path))
        .collect();
    let (list, meta) = (scratch.join("list.txt"), scratch.join("meta.txt"));
    let write = async {
        tokio::fs::create_dir_all(scratch).await?;
        tokio::fs::write(&list, concat_list(&paths)).await?;
        tokio::fs::write(&meta, ffmetadata(&tags, &chapters)).await
    };
    write.await.map_err(|e| format!("write merge files: {e}"))?;
    let argv = merge_args(
        parts,
        &paths,
        &encoding,
        &list.to_string_lossy(),
        &meta.to_string_lossy(),
        &tmp.to_string_lossy(),
    );
    ffmpeg(ctx, &argv, total_seconds(parts)).await?;
    let items = freeform_tags(&tags);
    if !items.is_empty() {
        let tmp = tmp.to_owned();
        tokio::task::spawn_blocking(move || add_freeform(&tmp, &items))
            .await
            .map_err(|e| e.to_string())??;
    }
    Ok((chapters, encoding != Encoding::Copy))
}

/// The run, its `-progress` lines on stdout as progress.
async fn ffmpeg(ctx: &Ctx, argv: &[String], total: f64) -> Result<(), String> {
    let cmd = Cmd::new("ffmpeg", &ctx.rt.ffmpeg, argv, Pipes { stdout: true });
    let c = ctx.clone();
    ctx.pipeline(timeout_ms(total), vec![cmd], |p| async move {
        let p = &p[0];
        let (_, exit) = tokio::join!(report(&c, p, total), p.wait());
        match exit.code {
            Some(0) => Ok(()),
            code => Err(format!("ffmpeg exited with {code:?}: {}", tail(p, 2))),
        }
    })
    .await?
}

async fn report(ctx: &Ctx, p: &Ffmpeg, total: f64) {
    let Some(out) = p.take_stdout() else {
        return;
    };
    let mut lines = BufReader::new(out).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        ctx.pulse();
        if let Some(s) = progress_seconds(&line) {
            ctx.progress((s / total * 100.0).clamp(0.0, 99.0));
        }
    }
}

/// Moves `tmp` to `out` unless `out` exists: a hard link fails on a taken name where a rename
/// would replace it; a filesystem without links gets a check and a rename.
async fn publish(tmp: &Path, out: &Path) -> Result<(), String> {
    match tokio::fs::hard_link(tmp, out).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Err(taken(out)),
        Err(_) if exists(out).await => Err(taken(out)),
        Err(_) => tokio::fs::rename(tmp, out)
            .await
            .map_err(|e| format!("write {}: {e}", out.display())),
    }
}

/// Adds free-form atoms to the `moov` of `path`, which the muxer wrote last (no faststart), in
/// place: nothing before it moves.
fn add_freeform(path: &Path, items: &[(&str, String)]) -> Result<(), String> {
    let io = |e: std::io::Error| format!("tag {}: {e}", path.display());
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(io)?;
    let len = f.metadata().map_err(io)?.len() as usize;
    let mut at = 0;
    let moov = loop {
        let mut head = [0u8; 16];
        f.seek(SeekFrom::Start(at as u64)).map_err(io)?;
        let n = f.read(&mut head).map_err(io)?;
        let b = read_box_header(&head[..n], 0, at, len).ok_or("malformed mp4")?;
        if &b.kind == b"moov" {
            break b;
        }
        at = b.end();
    };
    if moov.end() != len {
        return Err("moov is not the last box".into());
    }
    let mut buf = vec![0u8; moov.size];
    f.seek(SeekFrom::Start(at as u64)).map_err(io)?;
    f.read_exact(&mut buf).map_err(io)?;
    let grown = add_freeform_items(&buf, items).map_err(|e| e.to_string())?;
    f.seek(SeekFrom::Start(at as u64)).map_err(io)?;
    f.write_all(&grown).map_err(io)
}
