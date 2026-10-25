//! Control-plane token: `FLIKS_ENGINE_TOKEN`, else `<conf>/.engine-token`, else a fresh one.
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

fn conf_dirs() -> Vec<PathBuf> {
    let main = std::env::var_os("FLIKS_CONF_DIR")
        .filter(|v| !v.is_empty())
        .map_or_else(|| "/app/conf".into(), PathBuf::from);
    vec![main, std::env::temp_dir().join("fliks-conf")]
}

fn read(file: &Path) -> Option<String> {
    let s = fs::read_to_string(file).ok()?.trim().to_owned();
    (!s.is_empty()).then_some(s)
}

/// Written 0600 with `create_new`, so the first writer wins; `None` when the dir is not writable.
fn persist(dir: &Path, fresh: &str) -> Option<String> {
    let file = dir.join(".engine-token");
    if let Some(s) = read(&file) {
        return Some(s);
    }
    let mut b = DirBuilder::new();
    b.recursive(true);
    let mut open = OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
        std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
    }
    let written = b.create(dir).and_then(|()| {
        open.open(&file)
            .and_then(|mut f| f.write_all(fresh.as_bytes()))
    });
    match written {
        Ok(()) => Some(fresh.to_owned()),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => read(&file),
        Err(_) => None,
    }
}

/// An empty env value counts as unset. Without a writable conf dir the token lasts for the process.
pub fn resolve() -> String {
    let env = std::env::var("FLIKS_ENGINE_TOKEN").ok();
    resolve_in(env.as_deref(), &conf_dirs())
}

fn resolve_in(env: Option<&str>, dirs: &[PathBuf]) -> String {
    if let Some(t) = env.filter(|t| !t.is_empty()) {
        return t.to_owned();
    }
    let fresh = crate::random_hex(32);
    dirs.iter()
        .find_map(|d| persist(d, &fresh))
        .unwrap_or_else(|| {
            tracing::warn!("no writable conf dir: the engine token lasts for this process only");
            fresh
        })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn env_wins_and_writes_nothing() {
        let d = std::env::temp_dir().join(format!("eng-tok-{}", crate::random_hex(4)));
        assert_eq!(resolve_in(Some("x"), std::slice::from_ref(&d)), "x");
        assert!(!d.exists());
    }

    #[test]
    fn empty_env_falls_back_to_the_file_then_reuses_it() {
        let d = std::env::temp_dir().join(format!("eng-tok-{}", crate::random_hex(4)));
        let first = resolve_in(Some(""), std::slice::from_ref(&d));
        let file = d.join(".engine-token");
        assert_eq!(first.len(), 64);
        assert_eq!(fs::read_to_string(&file).unwrap(), first);
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(resolve_in(None, std::slice::from_ref(&d)), first);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn unwritable_dir_yields_a_process_local_token() {
        let t = resolve_in(None, &[PathBuf::from("/proc/nope/conf")]);
        assert_eq!(t.len(), 64);
    }
}
