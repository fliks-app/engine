//! The `cache/v1` directory: layout, index walk, eviction and the one-shot purge of the old
//! layout. Spec: `docs/spec/runtime.md` section 5.
use fliks_engine_core::api::CacheEntry;
use fliks_engine_core::session::{CACHE_NAMESPACE, cache_path};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub struct Cache {
    pub root: PathBuf,
}

/// One `<owner>/<file>/<hash>` directory found on disk.
#[derive(Debug, Clone, PartialEq)]
pub struct Scanned {
    pub owner: Option<String>,
    pub media_file_id: u32,
    pub hash: String,
    pub dir: PathBuf,
    pub bytes: u64,
    pub last_access_ms: i64,
}

impl Scanned {
    pub fn entry(&self, live: bool) -> CacheEntry {
        CacheEntry {
            owner_id: self.owner.clone(),
            media_file_id: self.media_file_id,
            profile_hash: self.hash.clone(),
            bytes: self.bytes as f64,
            last_access_ms: self.last_access_ms as f64,
            live,
        }
    }
}

/// Opaque owner ids become directory names: keep them to a safe alphabet.
pub fn valid_owner(o: &str) -> bool {
    !o.is_empty()
        && o.len() <= 64
        && o.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

/// Total bytes and newest mtime (ms) under `dir`.
fn walk(dir: &Path) -> (u64, i64) {
    let (mut bytes, mut newest) = (0, 0);
    let Ok(rd) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    for e in rd.flatten() {
        let Ok(md) = e.metadata() else { continue };
        if md.is_dir() {
            let (b, n) = walk(&e.path());
            bytes += b;
            newest = newest.max(n);
        } else {
            bytes += md.len();
            let m = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis() as i64);
            newest = newest.max(m);
        }
    }
    (bytes, newest)
}

fn subdirs(dir: &Path) -> Vec<(String, PathBuf)> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
                .collect()
        })
        .unwrap_or_default()
}

impl Cache {
    /// `FLIKS_TRANSCODE_DIR`, else `FLIKS_CACHE_DIR`, else the OS temp dir.
    pub fn from_env() -> Self {
        let var = |k| {
            std::env::var(k)
                .ok()
                .filter(|v: &String| !v.trim().is_empty())
        };
        let root = var("FLIKS_TRANSCODE_DIR")
            .or_else(|| var("FLIKS_CACHE_DIR"))
            .map_or_else(
                || std::env::temp_dir().join("fliks-transcode"),
                PathBuf::from,
            );
        Self { root }
    }

    pub fn entry_dir(&self, owner: Option<&str>, file: u32, hash: &str) -> PathBuf {
        self.root.join(cache_path(owner, file, hash, None))
    }

    pub fn rung_dir(&self, owner: Option<&str>, file: u32, hash: &str, quality: &str) -> PathBuf {
        self.root.join(cache_path(owner, file, hash, Some(quality)))
    }

    /// The engine-generated `sprite.jpg` and `sprite.json` of a file.
    pub fn sprite_dir(&self, file: u32) -> PathBuf {
        self.root.join("sprites/v1").join(file.to_string())
    }

    pub fn subtitles_dir(&self, file: u32) -> PathBuf {
        self.root.join("subtitles/v2").join(file.to_string())
    }

    /// Deletes every entry of `<root>/cache/` but the current namespace (the old layout), and the
    /// `-f webvtt` extracts of `subtitles/v1`.
    pub fn purge_old_layout(&self) {
        remove_dir(&self.root.join("subtitles/v1"));
        let parent = self.root.join(CACHE_NAMESPACE);
        let (Some(base), Some(keep)) = (parent.parent(), parent.file_name()) else {
            return;
        };
        let Ok(rd) = std::fs::read_dir(base) else {
            return;
        };
        for e in rd.flatten() {
            if e.file_name() == keep {
                continue;
            }
            let p = e.path();
            let r = if e.file_type().is_ok_and(|t| t.is_dir()) {
                std::fs::remove_dir_all(&p)
            } else {
                std::fs::remove_file(&p)
            };
            match r {
                Ok(()) => tracing::info!(path = %p.display(), "purged the old cache layout"),
                Err(e) => tracing::warn!(path = %p.display(), "old cache purge: {e}"),
            }
        }
    }

    /// The index, rebuilt from disk (blocking).
    pub fn scan(&self) -> Vec<Scanned> {
        let mut out = vec![];
        for (owner_dir, owner_path) in subdirs(&self.root.join(CACHE_NAMESPACE)) {
            let owner = match owner_dir.as_str() {
                "anon" => None,
                o => match o.strip_prefix('u') {
                    Some(id) => Some(id.to_string()),
                    None => continue,
                },
            };
            for (file_dir, file_path) in subdirs(&owner_path) {
                let Ok(file) = file_dir.parse() else { continue };
                for (hash, dir) in subdirs(&file_path) {
                    let (bytes, last) = walk(&dir);
                    out.push(Scanned {
                        owner: owner.clone(),
                        media_file_id: file,
                        hash,
                        dir,
                        bytes,
                        last_access_ms: last,
                    });
                }
            }
        }
        out
    }
}

/// `rm -rf`, quiet on a missing path.
pub fn remove_dir(dir: &Path) {
    if let Err(e) = std::fs::remove_dir_all(dir)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(dir = %dir.display(), "remove: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_scan_and_old_layout_purge() {
        let root = std::env::temp_dir().join(format!("fliks-cache-test-{}", std::process::id()));
        let c = Cache { root: root.clone() };
        let rung = c.rung_dir(Some("7"), 42, "abc", "720p");
        std::fs::create_dir_all(&rung).unwrap();
        std::fs::write(rung.join("seg-0000.m4s"), [0u8; 10]).unwrap();
        std::fs::create_dir_all(c.entry_dir(None, 9, "def")).unwrap();
        std::fs::create_dir_all(root.join("cache/u7/42/old")).unwrap();
        std::fs::create_dir_all(root.join("subtitles/v1/42")).unwrap();
        c.purge_old_layout();
        assert!(!root.join("subtitles/v1").exists());
        assert!(!root.join("cache/u7").exists());
        let mut s = c.scan();
        s.sort_by_key(|e| e.media_file_id);
        assert_eq!(s.len(), 2);
        assert_eq!((s[0].owner.as_deref(), s[0].bytes), (None, 0));
        assert_eq!(
            (s[1].owner.as_deref(), s[1].hash.as_str(), s[1].bytes),
            (Some("7"), "abc", 10)
        );
        assert!(s[1].last_access_ms > 0);
        assert!(valid_owner("a-b_c.1") && !valid_owner("a/b") && !valid_owner(""));
        remove_dir(&root);
    }
}
