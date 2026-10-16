//! The probe tools as processes: `core::probe::Tools` over tokio.
use crate::proc::background;
use crate::slots::Slots;
use fliks_engine_core::api::JobPriority;
use fliks_engine_core::probe::{ToolError, Tools, args};
use std::future::Future;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::task::JoinSet;
use tokio::time::{Instant, timeout_at};

/// `mkvmerge` is missing from the native bundles: remember it for the process, like the host.
static MKVMERGE_MISSING: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
struct Binaries {
    ffprobe: String,
    ffmpeg: String,
    mkvmerge: String,
}

impl Binaries {
    fn from_env() -> Self {
        let var = |k: &str, d: &str| {
            std::env::var(k)
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or(d.into())
        };
        Self {
            ffprobe: var("FFPROBE_PATH", "ffprobe"),
            ffmpeg: var("FFMPEG_PATH", "ffmpeg"),
            mkvmerge: var("MKVMERGE_PATH", "mkvmerge"),
        }
    }
}

pub struct ProcessTools {
    bin: Binaries,
    path: String,
    slots: std::sync::Arc<Slots>,
}

impl ProcessTools {
    pub fn new(path: &str, slots: std::sync::Arc<Slots>) -> Self {
        Self {
            bin: Binaries::from_env(),
            path: path.into(),
            slots,
        }
    }
}

fn failed(cmd: &str, args: &[String], stderr: String, stdout: String) -> ToolError {
    ToolError {
        message: format!("Command failed: {cmd} {}\n{stderr}", args.join(" ")),
        stderr,
        stdout,
        missing: false,
    }
}

fn spawn_error(cmd: &str, e: &std::io::Error) -> ToolError {
    ToolError {
        message: format!("spawn {cmd} {e}"),
        missing: e.kind() == std::io::ErrorKind::NotFound,
        ..ToolError::default()
    }
}

/// Reads up to `cap` bytes; `Err(text so far)` when the stream is longer.
async fn read_capped(mut r: impl AsyncReadExt + Unpin, cap: usize) -> Result<String, String> {
    let mut buf = Vec::new();
    let _ = (&mut r).take(cap as u64 + 1).read_to_end(&mut buf).await;
    let over = buf.len() > cap;
    buf.truncate(cap);
    let text = String::from_utf8_lossy(&buf).into_owned();
    if over { Err(text) } else { Ok(text) }
}

/// `execFile`: the whole output, killed on timeout or when an output outgrows `max_out`.
async fn capture(
    cmd: &str,
    args: &[String],
    timeout: Duration,
    max_out: usize,
) -> Result<(String, String), ToolError> {
    let mut child = crate::proc::spawn(
        Command::new(cmd)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true),
    )
    .await
    .map_err(|e| spawn_error(cmd, &e))?;
    let (out, err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let run = async {
        let ((out, err), status) = tokio::join!(
            async { tokio::join!(read_capped(out, max_out), read_capped(err, max_out)) },
            child.wait()
        );
        (out, err, status)
    };
    let Ok((out, err, status)) = tokio::time::timeout(timeout, run).await else {
        let _ = child.kill().await;
        return Err(failed(cmd, args, String::new(), String::new()));
    };
    match (out, err, status) {
        (Ok(o), Ok(e), Ok(s)) if s.success() => Ok((o, e)),
        (Ok(o), Ok(e), _) => Err(failed(cmd, args, e, o)),
        (Err(o), _, _) => {
            let _ = child.kill().await;
            Err(ToolError {
                message: "stdout maxBuffer length exceeded".into(),
                stdout: o,
                ..ToolError::default()
            })
        }
        (Ok(o), Err(_), _) => {
            let _ = child.kill().await;
            Err(ToolError {
                message: "stderr maxBuffer length exceeded".into(),
                stdout: o,
                ..ToolError::default()
            })
        }
    }
}

impl Tools for ProcessTools {
    fn ffprobe(
        &self,
        args: Vec<String>,
        timeout_ms: u64,
    ) -> impl Future<Output = Result<String, ToolError>> + Send {
        let cmd = self.bin.ffprobe.clone();
        async move {
            capture(
                &cmd,
                &args,
                Duration::from_millis(timeout_ms),
                args::MAX_OUTPUT_BYTES,
            )
            .await
            .map(|(out, _)| out)
        }
    }

    fn ffprobe_lines(
        &self,
        args: Vec<String>,
        timeout_ms: u64,
        on_line: &mut (dyn FnMut(&str) -> bool + Send),
    ) -> impl Future<Output = Result<(), ToolError>> + Send {
        let cmd = self.bin.ffprobe.clone();
        async move {
            let mut child = crate::proc::spawn(
                Command::new(&cmd)
                    .args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true),
            )
            .await
            .map_err(|e| spawn_error(&cmd, &e))?;
            let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
            let stderr = child.stderr.take().unwrap();
            // The last 2000 bytes of stderr, collected while stdout is read.
            let tail = tokio::spawn(async move {
                let mut buf = Vec::new();
                let _ = BufReader::new(stderr).read_to_end(&mut buf).await;
                String::from_utf8_lossy(&buf[buf.len().saturating_sub(2000)..]).into_owned()
            });
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            loop {
                match timeout_at(deadline, lines.next_line()).await {
                    Err(_) => {
                        let _ = child.kill().await;
                        return Err(ToolError {
                            message: format!("ffprobe timed out after {timeout_ms} ms"),
                            ..ToolError::default()
                        });
                    }
                    Ok(Ok(Some(line))) => {
                        if !line.is_empty() && !on_line(&line) {
                            let _ = child.kill().await;
                            return Ok(());
                        }
                    }
                    Ok(_) => break,
                }
            }
            let status = child.wait().await.map_err(|e| spawn_error(&cmd, &e))?;
            if status.success() {
                return Ok(());
            }
            let stderr = tail.await.unwrap_or_default();
            Err(ToolError {
                message: format!(
                    "ffprobe exited {}: {}",
                    status.code().map_or("null".into(), |c| c.to_string()),
                    stderr.trim()
                ),
                stderr,
                ..ToolError::default()
            })
        }
    }

    fn mkvmerge(
        &self,
        args: Vec<String>,
    ) -> impl Future<Output = Result<String, ToolError>> + Send {
        let cmd = self.bin.mkvmerge.clone();
        async move {
            if MKVMERGE_MISSING.load(Ordering::Relaxed) {
                return Err(ToolError {
                    message: format!("spawn {cmd} ENOENT"),
                    missing: true,
                    ..ToolError::default()
                });
            }
            let out = capture(
                &cmd,
                &args,
                Duration::from_millis(args::MKVMERGE_TIMEOUT_MS),
                args::MKVMERGE_MAX_OUTPUT_BYTES,
            )
            .await;
            if out.as_ref().is_err_and(|e| e.missing) {
                MKVMERGE_MISSING.store(true, Ordering::Relaxed);
                tracing::info!("mkvmerge not found; BCP-47 language tags disabled");
            }
            out.map(|(stdout, _)| stdout)
        }
    }

    fn ffmpeg_samples(
        &self,
        runs: Vec<Vec<String>>,
    ) -> impl Future<Output = Vec<Result<String, ToolError>>> + Send {
        let (ffmpeg, slots) = (self.bin.ffmpeg.clone(), self.slots.clone());
        async move {
            let mut set = JoinSet::new();
            for (i, a) in runs.into_iter().enumerate() {
                let (cmd, args) = background(&ffmpeg, &a);
                let slots = slots.clone();
                set.spawn(async move {
                    let _slot = slots.background(JobPriority::Normal).await;
                    let r = capture(
                        &cmd,
                        &args,
                        Duration::from_millis(args::CROP_TIMEOUT_MS),
                        args::MAX_OUTPUT_BYTES,
                    )
                    .await;
                    (i, r.map(|(_, stderr)| stderr))
                });
            }
            let mut out: Vec<Option<Result<String, ToolError>>> =
                (0..set.len()).map(|_| None).collect();
            while let Some(Ok((i, r))) = set.join_next().await {
                out[i] = Some(r);
            }
            out.into_iter()
                .map(|r| r.unwrap_or_else(|| Err(ToolError::default())))
                .collect()
        }
    }

    fn file_size(&self) -> impl Future<Output = Option<u64>> + Send {
        let path = self.path.clone();
        async move { tokio::fs::metadata(path).await.ok().map(|m| m.len()) }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant as StdInstant;

    /// Scripts standing in for the tools, in a directory removed on drop.
    struct Scripts(std::path::PathBuf);

    impl Scripts {
        fn new(test: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("fliks-probe-run-{test}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn script(&self, name: &str, body: &str) -> String {
            let path = self.0.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path.to_string_lossy().into_owned()
        }
    }

    impl Drop for Scripts {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tools(ffprobe: &str) -> ProcessTools {
        ProcessTools {
            bin: Binaries {
                ffprobe: ffprobe.into(),
                ffmpeg: "ffmpeg".into(),
                mkvmerge: "mkvmerge".into(),
            },
            path: "/x".into(),
            slots: Slots::new(1),
        }
    }

    #[tokio::test]
    async fn output_exit_status_and_missing_binary() {
        let dir = Scripts::new("output");
        let ok = tools(&dir.script("ok", "echo '{\"a\":1}'; echo warn >&2"));
        assert_eq!(ok.ffprobe(vec![], 5000).await.unwrap().trim(), "{\"a\":1}");
        let bad = tools(&dir.script("bad", "echo nope >&2; exit 1"));
        let e = bad.ffprobe(vec!["-v".into()], 5000).await.unwrap_err();
        assert_eq!(e.stderr.trim(), "nope");
        assert!(e.message.starts_with("Command failed:") && !e.missing);
        let e = tools("/nonexistent/ffprobe")
            .ffprobe(vec![], 5000)
            .await
            .unwrap_err();
        assert!(e.missing, "{e:?}");
    }

    #[tokio::test]
    async fn timeout_and_output_cap_kill_the_process() {
        let dir = Scripts::new("timeout");
        let slow = tools(&dir.script("slow", "sleep 30"));
        let t = StdInstant::now();
        assert!(slow.ffprobe(vec![], 200).await.is_err());
        assert!(t.elapsed() < Duration::from_secs(5));
        let loud = tools(&dir.script("loud", "yes | head -c 3000000"));
        let e = loud.ffprobe(vec![], 5000).await.unwrap_err();
        assert_eq!(e.message, "stdout maxBuffer length exceeded");
    }

    #[tokio::test]
    async fn lines_stop_the_process_early() {
        let dir = Scripts::new("lines");
        let endless =
            tools(&dir.script("endless", "i=0; while true; do echo $i; i=$((i+1)); done"));
        let mut seen = vec![];
        let t = StdInstant::now();
        endless
            .ffprobe_lines(vec![], 5000, &mut |l| {
                seen.push(l.to_string());
                seen.len() < 3
            })
            .await
            .unwrap();
        assert_eq!(seen, ["0", "1", "2"]);
        assert!(t.elapsed() < Duration::from_secs(3));
        let failing = tools(&dir.script("failing", "echo 1; echo bad >&2; exit 3"));
        let e = failing
            .ffprobe_lines(vec![], 5000, &mut |_| true)
            .await
            .unwrap_err();
        assert!(
            e.message.contains("exited 3") && e.stderr.contains("bad"),
            "{e:?}"
        );
        let hang = tools(&dir.script("hang", "echo 1; sleep 30"));
        assert!(
            hang.ffprobe_lines(vec![], 300, &mut |_| true)
                .await
                .is_err()
        );
    }
}
