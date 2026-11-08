//! Free-space checks that gate jobs and playback runs.

use crate::runtime::Runtime;
use std::path::Path;

const DEFAULT_MIN_FREE_BYTES: f64 = 1024.0 * 1024.0 * 1024.0;

fn disk_verdict(free: Option<u64>, min: f64, dir: &Path) -> Result<(), String> {
    match free {
        Some(f) if min > 0.0 && (f as f64) < min => Err(format!(
            "not enough disk space: {f} bytes free under {}, jobMinFreeBytes is {min}",
            dir.display()
        )),
        _ => Ok(()),
    }
}

#[cfg(unix)]
#[allow(clippy::useless_conversion)]
pub(super) fn free_bytes(p: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a valid C string and a zeroed out-struct.
    let ok = unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0;
    ok.then(|| u64::from(s.f_bavail) * u64::from(s.f_frsize))
}

#[cfg(windows)]
pub(super) fn free_bytes(p: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let w: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
    let mut free = 0u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            w.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(free)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn free_bytes(_: &Path) -> Option<u64> {
    None
}

impl Runtime {
    /// Fails below `streaming.jobMinFreeBytes` free under `dir` (created first): jobs and playback runs.
    pub async fn check_disk(&self, dir: &Path) -> Result<(), String> {
        self.check_disk_in(dir, true).await
    }

    /// `check_disk` on a directory that must already exist: nothing is created (a missing one passes).
    pub async fn check_disk_existing(&self, dir: &Path) -> Result<(), String> {
        self.check_disk_in(dir, false).await
    }

    async fn check_disk_in(&self, dir: &Path, create: bool) -> Result<(), String> {
        let min = self
            .current_config()
            .streaming
            .job_min_free_bytes
            .unwrap_or(DEFAULT_MIN_FREE_BYTES);
        let d = dir.to_path_buf();
        let free = tokio::task::spawn_blocking(move || {
            if create {
                let _ = std::fs::create_dir_all(&d);
            }
            free_bytes(&d)
        })
        .await
        .ok()
        .flatten();
        disk_verdict(free, min, dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_check_fails_below_the_threshold_only() {
        let d = std::env::temp_dir();
        let free = free_bytes(&d).expect("free space of the temp dir");
        assert!(free > 0);
        assert!(disk_verdict(Some(free), 1.0, &d).is_ok());
        let e = disk_verdict(Some(10), 1e9, &d).unwrap_err();
        assert!(e.starts_with("not enough disk space: 10 bytes free"), "{e}");
        assert!(disk_verdict(Some(10), 0.0, &d).is_ok(), "0 disables");
        assert!(
            disk_verdict(None, 1e9, &d).is_ok(),
            "unknown is not a failure"
        );
    }
}
