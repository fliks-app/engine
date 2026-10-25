//! The IO of host detection: ffmpeg runs, DRI/sysfs reads and the pause probe.
use fliks_engine_core::hw::{
    Cmd, GpuInfo, GpuRaw, Outcome, PROBE_ARGS, PauseVerdict, Runner, describe_gpu, pause_step,
};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::{sleep, timeout};

/// Spawns `ffmpeg` per command, killed on the deadline or when the future is dropped.
pub struct ProcRunner {
    pub ffmpeg: String,
}

impl Runner for ProcRunner {
    async fn run(&self, cmd: &Cmd) -> Outcome {
        let child = crate::proc::spawn(
            Command::new(&self.ffmpeg)
                .args(&cmd.args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true),
        )
        .await;
        let Ok(child) = child else {
            return Outcome::default();
        };
        match timeout(
            Duration::from_millis(cmd.timeout_ms),
            child.wait_with_output(),
        )
        .await
        {
            Ok(Ok(o)) => Outcome {
                ok: o.status.success(),
                stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
            },
            _ => Outcome::default(),
        }
    }

    async fn write(&self, path: &str, bytes: &[u8]) -> bool {
        std::fs::write(path, bytes).is_ok()
    }

    async fn remove(&self, path: &str) {
        let _ = std::fs::remove_file(path);
    }
}

fn read_trimmed(p: &Path) -> Option<String> {
    std::fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().to_string())
}

/// DRI render nodes with their sysfs identity; empty without `/dev/dri` (Windows, macOS, no GPU).
/// [ts:backend/src/modules/streaming/transcoding/gpu-registry.ts:71-125]
pub fn enumerate_gpus(dri: &Path, sys_class_drm: &Path) -> Vec<GpuInfo> {
    let Ok(rd) = std::fs::read_dir(dri) else {
        return vec![];
    };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| {
            n.strip_prefix("renderD")
                .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let dev = sys_class_drm.join(&name).join("device");
            let raw = GpuRaw {
                vendor_id: read_trimmed(&dev.join("vendor")),
                device_id: read_trimmed(&dev.join("device")),
                boot_vga: read_trimmed(&dev.join("boot_vga")),
            };
            describe_gpu(&name, &dri.join(&name).to_string_lossy(), &raw)
        })
        .collect()
}

/// Does this ffmpeg pause on stdin `p` and resume on `u`? Gives up (false) after 12 s.
/// [ts:backend/src/modules/streaming/transcoding/ffmpeg-pause.ts:61-117]
pub async fn probe_stdin_pause(ffmpeg: &str) -> bool {
    timeout(Duration::from_secs(12), pause_run(ffmpeg))
        .await
        .unwrap_or(false)
}

async fn pause_run(ffmpeg: &str) -> bool {
    let Ok(mut child) = crate::proc::spawn(
        Command::new(ffmpeg)
            .args(PROBE_ARGS)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true),
    )
    .await
    else {
        return false;
    };
    let (Some(mut stdin), Some(stderr)) = (child.stdin.take(), child.stderr.take()) else {
        return false;
    };
    let frames = Arc::new(AtomicU32::new(0));
    let seen = frames.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            if let Some(n) = l.strip_prefix("frame=").and_then(|n| n.trim().parse().ok()) {
                seen.store(n, Ordering::Relaxed);
            }
        }
    });
    let now = || frames.load(Ordering::Relaxed);
    sleep(Duration::from_millis(900)).await;
    if stdin.write_all(b"p").await.is_err() {
        return false;
    }
    sleep(Duration::from_millis(1500)).await;
    let settle = now();
    sleep(Duration::from_millis(1500)).await;
    if pause_step(settle, now(), false) != PauseVerdict::Frozen {
        return false;
    }
    if stdin.write_all(b"u").await.is_err() {
        return false;
    }
    sleep(Duration::from_millis(1000)).await;
    pause_step(settle, now(), true) == PauseVerdict::Works
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    #[test]
    fn reads_render_nodes_and_ignores_the_rest() {
        let root = std::env::temp_dir().join(format!("fliks-gpu-{}", std::process::id()));
        let (dri, sys) = (root.join("dri"), root.join("sys"));
        for n in ["card1", "renderD129", "renderD128", "renderDx", "by-path"] {
            write(&dri.join(n), "");
        }
        write(&sys.join("renderD128/device/vendor"), "0x8086\n");
        write(&sys.join("renderD128/device/device"), "0xa7a8\n");
        write(&sys.join("renderD128/device/boot_vga"), "1\n");
        write(&sys.join("renderD129/device/vendor"), "0x8086\n");
        write(&sys.join("renderD129/device/device"), "0x5693\n");
        let g = enumerate_gpus(&dri, &sys);
        std::fs::remove_dir_all(&root).unwrap();
        let labels: Vec<_> = g.iter().map(|x| x.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Intel Raptor Lake UHD Graphics · iGPU · renderD128",
                "Intel Arc A370M · dGPU · renderD129"
            ]
        );
        assert!(g[0].render_node.ends_with("renderD128"));
    }

    #[test]
    fn no_dri_directory_means_no_gpus() {
        assert!(
            enumerate_gpus(Path::new("/nonexistent-dri"), Path::new("/nonexistent-sys")).is_empty()
        );
    }

    #[tokio::test]
    async fn a_missing_ffmpeg_fails_closed() {
        let r = ProcRunner {
            ffmpeg: "/nonexistent/ffmpeg".into(),
        };
        let c = Cmd {
            args: vec![],
            timeout_ms: 1000,
        };
        assert!(!r.run(&c).await.ok);
        assert!(!probe_stdin_pause("/nonexistent/ffmpeg").await);
    }
}
