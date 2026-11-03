//! The `Merge` job: the parts of an audiobook into one `.m4b`, one ffmpeg run, written beside its
//! final name and published without overwriting. Spec: `docs/spec/jobs.md` section 10.
use super::{Cmd, Ctx, tail};
use crate::cache::{MERGE_JOURNAL, file_id};
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
    // The core accepts any platform's absolute form; the mapped paths must be this host's.
    let paths: Vec<String> = parts
        .iter()
        .map(|p| ctx.rt.map_media_path(&p.path))
        .collect();
    if let Some(bad) = std::iter::once(&*out)
        .chain(paths.iter().map(Path::new))
        .find(|p| !p.is_absolute())
    {
        return Err(format!(
            "not an absolute path on this host: {}",
            bad.display()
        ));
    }
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
    // The temporary file is created here, exclusively, and journaled with its identity: only
    // that file is ever removed, by this job or by a boot after a crash.
    let claimed = async {
        tokio::fs::create_dir_all(&scratch).await?;
        tokio::fs::File::options()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .await?;
        let id = file_id(&tmp).unwrap_or_default();
        let text = format!("{}\n{}\n{id}\n", path_text(&out), path_text(&tmp));
        tokio::fs::write(scratch.join(MERGE_JOURNAL), text).await?;
        Ok::<_, std::io::Error>(id)
    };
    let (claimed, built) = match claimed.await {
        Ok(id) => (
            Some(id),
            build(ctx, parts, &paths, over, &scratch, &tmp).await,
        ),
        Err(e) => (None, Err(format!("write merge files: {e}"))),
    };
    let published = match built {
        Ok(r) => publish(&tmp, &out).await.map(|()| r),
        Err(e) => Err(e),
    };
    if claimed.is_some_and(|id| file_id(&tmp).as_deref() == Some(id.as_str())) {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    let _ = tokio::fs::remove_dir_all(&scratch).await;
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
    paths: &[String],
    over: Option<&MediaTags>,
    scratch: &Path,
    tmp: &Path,
) -> Result<(Vec<Chapter>, bool), String> {
    let (tags, chapters, encoding) = (merged_tags(parts, over), chapters(parts), encoding(parts));
    let files = MergeFiles {
        list: path_text(&scratch.join("list.txt")),
        meta: path_text(&scratch.join("meta.txt")),
        graph: path_text(&scratch.join("graph.txt")),
    };
    let graph = match encoding {
        Encoding::Copy => String::new(),
        Encoding::Aac {
            sample_rate,
            channels,
            ..
        } => filter_graph(parts.len(), sample_rate, channels),
    };
    let write = async {
        tokio::fs::create_dir_all(scratch).await?;
        tokio::fs::write(&files.list, concat_list(paths)).await?;
        tokio::fs::write(&files.meta, ffmetadata(&tags, &chapters)).await?;
        tokio::fs::write(&files.graph, graph).await
    };
    write.await.map_err(|e| format!("write merge files: {e}"))?;
    let argv = merge_args(parts, paths, &encoding, &files, &path_text(tmp));
    let chars = argv_chars(&argv);
    if cfg!(windows) && chars > WINDOWS_ARGV_LIMIT {
        return Err(format!(
            "{} parts need a command line of {chars} characters, past the {WINDOWS_ARGV_LIMIT} Windows allows",
            parts.len()
        ));
    }
    ffmpeg(ctx, &argv, total_seconds(parts)).await?;
    let items = freeform_tags(&tags);
    let file = tmp.to_owned();
    tokio::task::spawn_blocking(move || finish_file(&file, &items))
        .await
        .map_err(|e| e.to_string())??;
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
/// would replace it; a filesystem without links gets `rename_new`.
async fn publish(tmp: &Path, out: &Path) -> Result<(), String> {
    match tokio::fs::hard_link(tmp, out).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Err(taken(out)),
        Err(_) if exists(out).await => Err(taken(out)),
        Err(_) => {
            let (from, to) = (tmp.to_owned(), out.to_owned());
            match tokio::task::spawn_blocking(move || rename_new(&from, &to)).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) if e.kind() == ErrorKind::AlreadyExists => Err(taken(out)),
                Ok(Err(e)) => Err(format!("write {}: {e}", out.display())),
                Err(e) => Err(e.to_string()),
            }
        }
    }
}

/// Renames without replacing: atomic through `renameat2(RENAME_NOREPLACE)` on Linux, a check and
/// a rename where the kernel or the filesystem lacks it (and elsewhere).
fn rename_new(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes());
        if let (Ok(f), Ok(t)) = (c(from), c(to)) {
            // SAFETY: both pointers are NUL-terminated strings that outlive the call.
            let r = unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    libc::AT_FDCWD,
                    f.as_ptr(),
                    libc::AT_FDCWD,
                    t.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            if r == 0 {
                return Ok(());
            }
            let e = std::io::Error::last_os_error();
            if !matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENOSYS)) {
                return Err(e);
            }
        }
    }
    if to.try_exists()? {
        return Err(ErrorKind::AlreadyExists.into());
    }
    std::fs::rename(from, to)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn a_rename_never_replaces() {
        let d = std::env::temp_dir().join(format!("fliks-noreplace-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let (a, b) = (d.join("a"), d.join("b"));
        std::fs::write(&a, b"new").unwrap();
        std::fs::write(&b, b"old").unwrap();
        assert_eq!(
            rename_new(&a, &b).unwrap_err().kind(),
            ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(&b).unwrap(), b"old");
        std::fs::remove_file(&b).unwrap();
        rename_new(&a, &b).unwrap();
        assert_eq!(std::fs::read(&b).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&d);
    }
}

fn path_text(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Adds free-form atoms to the `moov` of `path`, which the muxer wrote last (no faststart), in
/// place (nothing before it moves), then flushes the file to disk before it is published.
fn finish_file(path: &Path, items: &[(&str, String)]) -> Result<(), String> {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(io)?;
    if !items.is_empty() {
        add_freeform(&mut f, items)?;
    }
    f.sync_all().map_err(io)
}

fn io(e: std::io::Error) -> String {
    format!("finish the m4b: {e}")
}

fn add_freeform(f: &mut std::fs::File, items: &[(&str, String)]) -> Result<(), String> {
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
