//! Control-plane token: `FLIKS_ENGINE_TOKEN`, else `<conf>/.engine-token`, else a fresh one.
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{ErrorKind, Read as _, Write};
use std::path::{Path, PathBuf};

/// The configured dir, then a private temp dir.
fn conf_dirs() -> [PathBuf; 2] {
    let main = std::env::var_os("FLIKS_CONF_DIR")
        .filter(|v| !v.is_empty())
        .map_or_else(|| "/app/conf".into(), PathBuf::from);
    [main, std::env::temp_dir().join("fliks-conf")]
}

#[cfg(unix)]
fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

enum Read {
    Absent,
    Refused,
    Found(String),
}

/// The configured dir: owned by us, or not world-writable (a root:0 `g=u` volume, a `fsGroup` mount).
/// The temp dir: owned by us only, it is created 0700.
#[cfg(unix)]
fn dir_usable(dir: &Path, temp: bool) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(m) = fs::metadata(dir) else {
        return false;
    };
    let (owner, world_writable) = (m.uid(), m.mode() & 0o002 != 0);
    if owner == euid() {
        if world_writable && !temp {
            tracing::warn!("{} is world-writable: run chmod o-w on it", dir.display());
        }
        return true;
    }
    if !temp && !world_writable {
        return true;
    }
    tracing::error!(
        "{}: owned by uid {owner}, the engine runs as uid {} and the directory is {}: run chown -R {} on it",
        dir.display(),
        euid(),
        if temp {
            "not private"
        } else {
            "world-writable"
        },
        euid()
    );
    false
}

#[cfg(not(unix))]
fn dir_usable(_: &Path, _: bool) -> bool {
    true
}

/// A permission error names the path and the `chown` that fixes it; anything else is logged as is.
#[cfg(unix)]
fn denied(path: &Path, e: &std::io::Error) {
    if e.kind() == ErrorKind::PermissionDenied {
        let dir = path.parent().unwrap_or(path);
        tracing::error!(
            "cannot access {}: {e}: run chown -R {}:0 {} on the host",
            path.display(),
            euid(),
            dir.display()
        );
    } else {
        tracing::error!("cannot open {}: {e}", path.display());
    }
}

/// Opened without following a symlink and checked on the descriptor: a regular file with no other
/// link, owned by us. A group or other bit is dropped and the file adopted.
#[cfg(unix)]
fn read_secret(file: &Path) -> Read {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let f = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(file)
    {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => return Read::Absent,
        Err(e) => {
            denied(file, &e);
            return Read::Refused;
        }
    };
    let Ok(m) = f.metadata() else {
        return Read::Refused;
    };
    if !m.is_file() || m.nlink() != 1 || m.uid() != euid() {
        tracing::error!(
            "{}: not a regular file with one link owned by uid {}, found uid {}: run chown {} on it, or delete it to rotate",
            file.display(),
            euid(),
            m.uid(),
            euid()
        );
        return Read::Refused;
    }
    if m.mode() & 0o077 != 0 {
        let exposed = file
            .parent()
            .and_then(|d| fs::metadata(d).ok())
            .is_some_and(|d| d.mode() & 0o001 != 0);
        match f.set_permissions(fs::Permissions::from_mode(0o600)) {
            Ok(()) if exposed => tracing::warn!(
                "{} was readable by other users, now 0600: delete it to rotate the token",
                file.display()
            ),
            Ok(()) => tracing::warn!("{} was open to group or others, now 0600", file.display()),
            Err(e) => {
                tracing::error!("cannot tighten {}: {e}", file.display());
                return Read::Refused;
            }
        }
    }
    read_from(f, file)
}

#[cfg(not(unix))]
fn read_secret(file: &Path) -> Read {
    match fs::File::open(file) {
        Ok(f) => read_from(f, file),
        Err(e) if e.kind() == ErrorKind::NotFound => Read::Absent,
        Err(_) => Read::Refused,
    }
}

fn read_from(mut f: fs::File, file: &Path) -> Read {
    let mut s = String::new();
    if let Err(e) = f.read_to_string(&mut s) {
        tracing::error!("cannot read {}: {e}", file.display());
        return Read::Refused;
    }
    let s = s.trim();
    if s.is_empty() {
        tracing::error!(
            "{} is empty: delete it to generate a new one",
            file.display()
        );
        return Read::Refused;
    }
    Read::Found(s.to_owned())
}

/// Written 0600 through a synced temp file hard-linked into place, so the first writer wins and no
/// reader sees a partial file; `None` when the dir is not usable, not writable, or the file is
/// refused (a shared `/tmp` entry is never adopted).
fn persist(dir: &Path, fresh: &str, temp: bool) -> Option<String> {
    let file = dir.join(".engine-token");
    if dir.exists() && !dir_usable(dir, temp) {
        return None;
    }
    match read_secret(&file) {
        Read::Found(s) => return Some(s),
        Read::Refused => return None,
        Read::Absent => {}
    }
    let mut b = DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    match b.create(dir).and_then(|()| publish(dir, &file, fresh)) {
        Ok(()) => Some(fresh.to_owned()),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => match read_secret(&file) {
            Read::Found(s) => Some(s),
            _ => None,
        },
        Err(e) => {
            tracing::warn!("cannot persist the engine token in {}: {e}", dir.display());
            None
        }
    }
}

fn publish(dir: &Path, file: &Path, content: &str) -> std::io::Result<()> {
    let tmp = dir.join(format!(".tmp-{}", crate::random_hex(8)));
    let mut open = OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
    let written = open.open(&tmp).and_then(|mut f| {
        f.write_all(content.as_bytes())?;
        f.sync_all()
    });
    let linked = written.and_then(|()| fs::hard_link(&tmp, file));
    let _ = fs::remove_file(&tmp);
    linked
}

/// An empty env value counts as unset. Without a writable conf dir the token lasts for the process.
pub fn resolve() -> String {
    let env = std::env::var("FLIKS_ENGINE_TOKEN").ok();
    resolve_in(env.as_deref(), &conf_dirs())
}

fn resolve_in(env: Option<&str>, [conf, tmp]: &[PathBuf; 2]) -> String {
    if let Some(t) = env.filter(|t| !t.is_empty()) {
        return t.to_owned();
    }
    let fresh = crate::random_hex(32);
    if let Some(s) = persist(conf, &fresh, false) {
        return s;
    }
    if let Some(s) = persist(tmp, &fresh, true) {
        tracing::warn!(
            "{} is not usable: the engine token is kept in {}, lost when the container is recreated",
            conf.display(),
            tmp.display()
        );
        return s;
    }
    tracing::warn!("no usable conf dir: the engine token lasts for this process only");
    fresh
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch() -> PathBuf {
        std::env::temp_dir().join(format!("eng-tok-{}", crate::random_hex(4)))
    }

    /// A configured dir and a temp dir that both live under one scratch dir.
    fn dirs(base: &Path) -> [PathBuf; 2] {
        [base.join("conf"), base.join("tmp")]
    }

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn env_wins_and_writes_nothing() {
        let base = scratch();
        assert_eq!(resolve_in(Some("x"), &dirs(&base)), "x");
        assert!(!base.exists());
    }

    #[test]
    fn empty_env_falls_back_to_the_file_then_reuses_it() {
        let base = scratch();
        let d = dirs(&base);
        let first = resolve_in(Some(""), &d);
        let file = d[0].join(".engine-token");
        assert_eq!(first.len(), 64);
        assert_eq!(fs::read_to_string(&file).unwrap(), first);
        assert_eq!((mode(&file), mode(&d[0])), (0o600, 0o700));
        assert_eq!(resolve_in(None, &d), first);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_token_file_open_to_others_is_tightened_and_adopted() {
        let base = scratch();
        let d = dirs(&base);
        fs::create_dir_all(&d[0]).unwrap();
        let file = d[0].join(".engine-token");
        fs::write(&file, "kept\n").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(resolve_in(None, &d), "kept");
        assert_eq!(mode(&file), 0o600);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn an_owned_world_writable_dir_keeps_its_token() {
        let base = scratch();
        let d = dirs(&base);
        fs::create_dir_all(&d[0]).unwrap();
        fs::set_permissions(&d[0], fs::Permissions::from_mode(0o777)).unwrap();
        let first = resolve_in(None, &d);
        assert_eq!(
            fs::read_to_string(d[0].join(".engine-token")).unwrap(),
            first
        );
        assert_eq!(resolve_in(None, &d), first);
        assert!(!d[1].exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_symlinked_or_hard_linked_token_is_refused_and_never_written_through() {
        let base = scratch();
        let d = dirs(&base);
        fs::create_dir_all(&d[0]).unwrap();
        let target = base.join("elsewhere");
        fs::write(&target, "victim").unwrap();
        let file = d[0].join(".engine-token");
        std::os::unix::fs::symlink(&target, &file).unwrap();
        let t = resolve_in(None, &d);
        assert_ne!(t, "victim");
        assert_eq!(fs::read_to_string(&target).unwrap(), "victim");
        assert_eq!(fs::read_to_string(d[1].join(".engine-token")).unwrap(), t);
        fs::remove_dir_all(&base).unwrap();

        let d = dirs(&base);
        fs::create_dir_all(&d[0]).unwrap();
        fs::write(&target, "victim").unwrap();
        fs::hard_link(&target, d[0].join(".engine-token")).unwrap();
        assert_ne!(resolve_in(None, &d), "victim");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn unwritable_dirs_yield_a_process_local_token() {
        let nope = [
            PathBuf::from("/proc/nope/conf"),
            PathBuf::from("/proc/nope/tmp"),
        ];
        assert_eq!(resolve_in(None, &nope).len(), 64);
    }
}
