//! Writing a file beside its final name and publishing it without ever replacing: shared by the
//! `Merge` job and the library optimizer (`staged` outputs).
use std::io::ErrorKind;
use std::path::Path;

/// Whether the name is taken, a dangling symlink included.
pub async fn exists(p: &Path) -> bool {
    tokio::fs::symlink_metadata(p).await.is_ok()
}

pub fn taken(out: &Path) -> String {
    format!("output exists: {}", out.display())
}

/// Moves `tmp` to `out` unless `out` exists: a hard link fails on a taken name where a rename
/// would replace it; a filesystem without links gets `rename_new`.
pub async fn publish(tmp: &Path, out: &Path) -> Result<(), String> {
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

/// Renames without replacing: atomic through `renameat2(RENAME_NOREPLACE)` on Linux and
/// `renamex_np(RENAME_EXCL)` on macOS, a check and a rename where the kernel or filesystem lacks it.
#[cfg(unix)]
pub fn rename_new(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes());
    let (f, t) = (c(from)?, c(to)?);
    // SAFETY: both pointers are NUL-terminated strings that outlive the call.
    #[cfg(target_os = "linux")]
    let (r, unsupported) = unsafe {
        let r = libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD as libc::c_long,
            f.as_ptr() as libc::c_long,
            libc::AT_FDCWD as libc::c_long,
            t.as_ptr() as libc::c_long,
            libc::RENAME_NOREPLACE as libc::c_long,
        );
        // EPERM: a seccomp profile that denies syscalls it does not know.
        (r, [libc::EINVAL, libc::ENOSYS, libc::EPERM])
    };
    // SAFETY: both pointers are NUL-terminated strings that outlive the call.
    #[cfg(target_os = "macos")]
    let (r, unsupported) = unsafe {
        let r = libc::renamex_np(f.as_ptr(), t.as_ptr(), libc::RENAME_EXCL);
        (r, [libc::EINVAL, libc::ENOTSUP])
    };
    if r == 0 {
        return Ok(());
    }
    let e = std::io::Error::last_os_error();
    if !e.raw_os_error().is_some_and(|n| unsupported.contains(&n)) {
        return Err(e);
    }
    if std::fs::symlink_metadata(to).is_ok() {
        return Err(ErrorKind::AlreadyExists.into());
    }
    std::fs::rename(from, to)
}

/// Renames without replacing: `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` fails on a taken
/// name.
#[cfg(windows)]
pub fn rename_new(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let w = |p: &Path| p.as_os_str().encode_wide().chain([0]).collect::<Vec<u16>>();
    // SAFETY: both pointers are NUL-terminated wide strings that outlive the call.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::MoveFileExW(w(from).as_ptr(), w(to).as_ptr(), 0)
    };
    if ok != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Creates `staged` exclusively, never its directory, and fails when it would land on another
/// filesystem than `src` (a wrong mapping must not fill the container layer). Mode and owner are
/// the source's.
pub fn create_staged(src: &Path, staged: &Path) -> std::io::Result<std::fs::File> {
    let meta = std::fs::metadata(src)?;
    #[cfg(unix)]
    {
        let dir = staged.parent().unwrap_or(Path::new("."));
        use std::os::unix::fs::MetadataExt;
        if std::fs::metadata(dir)?.dev() != meta.dev() {
            return Err(std::io::Error::other(format!(
                "{} is not on the source's filesystem",
                dir.display()
            )));
        }
    }
    let f = std::fs::File::options()
        .write(true)
        .create_new(true)
        .open(staged)?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        // SAFETY: a valid descriptor; owner changes need privileges and are best effort.
        unsafe {
            libc::fchmod(f.as_raw_fd(), meta.mode() as libc::mode_t & 0o7777);
            libc::fchown(f.as_raw_fd(), meta.uid(), meta.gid());
        }
    }
    #[cfg(not(unix))]
    let _ = meta;
    Ok(f)
}

/// Copies `src` into `dst`, as a reflink clone when the filesystem has them; `room` runs only
/// before a full copy and can refuse it.
pub fn copy_file(
    src: &Path,
    dst: &mut std::fs::File,
    room: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut from = std::fs::File::open(src)?;
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        const FICLONE: libc::c_ulong = 0x4004_9409;
        // SAFETY: both descriptors are open for the call.
        if unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE as _, from.as_raw_fd()) } == 0 {
            return Ok(());
        }
    }
    room()?;
    std::io::copy(&mut from, dst).map(|_| ())
}

/// Lowercase hex SHA-256 of a file, `tick` called per chunk.
pub fn sha256_file(path: &Path, mut tick: impl FnMut()) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
        tick();
    }
    Ok(ctx
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Flushes a file written by another process to disk.
pub fn sync_file(path: &Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_copy_can_be_refused_before_it_starts() {
        let d = std::env::temp_dir().join(format!("fliks-room-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let (src, dst) = (d.join("src.mkv"), d.join("dst.tmp"));
        std::fs::write(&src, b"abc").unwrap();
        let mut f = std::fs::File::create(&dst).unwrap();
        let no = || Err(std::io::Error::new(ErrorKind::StorageFull, "full"));
        // A reflink (btrfs, XFS) needs no room: the refusal then never runs.
        match copy_file(&src, &mut f, no) {
            Err(e) => {
                assert_eq!(e.kind(), ErrorKind::StorageFull);
                assert_eq!(std::fs::metadata(&dst).unwrap().len(), 0);
            }
            Ok(()) => assert_eq!(std::fs::read(&dst).unwrap(), b"abc"),
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn staged_files_are_created_exclusively_and_hash_hex() {
        let d = std::env::temp_dir().join(format!("fliks-staged-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let (src, staged) = (d.join("src.mkv"), d.join(".fliks-opt-1-1.tmp"));
        std::fs::write(&src, b"abc").unwrap();
        let mut f = create_staged(&src, &staged).unwrap();
        copy_file(&src, &mut f, || Ok(())).unwrap();
        assert_eq!(
            create_staged(&src, &staged).unwrap_err().kind(),
            ErrorKind::AlreadyExists
        );
        let missing = d.join("nodir").join("x.tmp");
        assert!(create_staged(&src, &missing).is_err(), "no create_dir_all");
        assert_eq!(
            sha256_file(&staged, || {}).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

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
        #[cfg(unix)]
        {
            let dangling = d.join("dangling");
            std::os::unix::fs::symlink(d.join("nowhere"), &dangling).unwrap();
            std::fs::write(&a, b"again").unwrap();
            assert_eq!(
                rename_new(&a, &dangling).unwrap_err().kind(),
                ErrorKind::AlreadyExists
            );
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}
