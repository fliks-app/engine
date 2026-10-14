//! Child process helpers shared by every ffmpeg/ffprobe/mkvmerge caller, and the lifecycle of
//! long ffmpeg runs (`docs/spec/runtime.md` section 1).
use fliks_engine_core::session::{PauseCapability, PauseCommand, pause_command};
use std::collections::VecDeque;
use std::io;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, watch};

/// `Command::spawn`, retried briefly on ETXTBSY: a concurrent fork can still hold a just-written binary open.
pub async fn spawn(cmd: &mut Command) -> io::Result<Child> {
    for _ in 0..5 {
        match cmd.spawn() {
            Err(e) if e.raw_os_error() == Some(26) => {
                tokio::time::sleep(Duration::from_millis(20)).await
            }
            r => return r,
        }
    }
    cmd.spawn()
}

/// Kill-on-close job: the child tree dies with the engine. The handle is intentionally never closed.
#[cfg(windows)]
pub fn assign_to_job(child: &Child) {
    use std::sync::OnceLock;
    use windows_sys::Win32::System::JobObjects::*;
    static JOB: OnceLock<usize> = OnceLock::new();
    let job = *JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of_val(&info) as u32,
        );
        job as usize
    });
    if let Some(h) = child.raw_handle()
        && unsafe { AssignProcessToJobObject(job as _, h as _) } == 0
    {
        tracing::warn!(
            "job object assign failed: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// Between the polite stop (SIGTERM, `q`) and the kill.
pub const STOP_GRACE: Duration = Duration::from_secs(2);
const RING_LINES: usize = 40;
const RING_LINE_BYTES: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exit {
    pub code: Option<i32>,
    /// Ended by a signal (no exit code).
    pub signal: bool,
}

/// One running ffmpeg: its own process group (unix) or the kill-on-close job (windows).
/// Dropping the handle kills it.
pub struct Ffmpeg {
    pid: Option<u32>,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    stderr: Arc<Mutex<VecDeque<String>>>,
    exit: watch::Receiver<Option<Exit>>,
    kill: mpsc::Sender<()>,
}

fn to_exit(s: io::Result<std::process::ExitStatus>) -> Exit {
    let Ok(s) = s else {
        return Exit {
            code: None,
            signal: true,
        };
    };
    Exit {
        code: s.code(),
        signal: s.code().is_none(),
    }
}

fn clip(mut line: String) -> String {
    if line.len() > RING_LINE_BYTES {
        let mut cut = RING_LINE_BYTES;
        while !line.is_char_boundary(cut) {
            cut -= 1;
        }
        line.truncate(cut);
    }
    line
}

impl Ffmpeg {
    pub async fn spawn(bin: &str, args: &[String]) -> io::Result<Self> {
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = spawn(&mut cmd).await?;
        #[cfg(windows)]
        assign_to_job(&child);
        let stderr = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(err) = child.stderr.take() {
            let ring = stderr.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(err).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let mut r = ring.lock().unwrap();
                    if r.len() == RING_LINES {
                        r.pop_front();
                    }
                    r.push_back(clip(l));
                }
            });
        }
        let pid = child.id();
        let stdin = child.stdin.take();
        let (exit_tx, exit) = watch::channel(None);
        let (kill, mut kill_rx) = mpsc::channel::<()>(1);
        tokio::spawn(async move {
            let status = tokio::select! {
                s = child.wait() => s,
                // A request or a dropped handle: kill.
                _ = kill_rx.recv() => {
                    #[cfg(unix)]
                    if let Some(pid) = pid { group_signal(pid, libc::SIGKILL) }
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            let _ = exit_tx.send(Some(to_exit(status)));
        });
        Ok(Self {
            pid,
            stdin: tokio::sync::Mutex::new(stdin),
            stderr,
            exit,
            kill,
        })
    }

    pub fn exited(&self) -> Option<Exit> {
        *self.exit.borrow()
    }

    pub async fn wait(&self) -> Exit {
        let mut rx = self.exit.clone();
        match rx.wait_for(|e| e.is_some()).await {
            Ok(e) => e.unwrap(),
            Err(_) => Exit {
                code: None,
                signal: true,
            },
        }
    }

    pub fn stderr_tail(&self) -> Vec<String> {
        self.stderr.lock().unwrap().iter().cloned().collect()
    }

    async fn write_key(&self, key: u8) -> bool {
        let mut stdin = self.stdin.lock().await;
        match stdin.as_mut() {
            // A dead-but-unreaped ffmpeg fails the write (EPIPE): ignored.
            Some(s) => s.write_all(&[key]).await.is_ok() && s.flush().await.is_ok(),
            None => false,
        }
    }

    /// Polite stop then kill after `STOP_GRACE`; returns once the process is gone.
    pub async fn stop(&self) -> Exit {
        if let Some(e) = self.exited() {
            return e;
        }
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            // A stopped process only handles SIGTERM once continued.
            group_signal(pid, libc::SIGCONT);
            group_signal(pid, libc::SIGTERM);
        }
        #[cfg(not(unix))]
        self.write_key(b'q').await;
        match tokio::time::timeout(STOP_GRACE, self.wait()).await {
            Ok(e) => e,
            Err(_) => {
                let _ = self.kill.try_send(());
                self.wait().await
            }
        }
    }

    /// Pause or resume per the host's capability; false when nothing was sent.
    pub async fn pause(&self, cap: PauseCapability, pause: bool) -> bool {
        if self.exited().is_some() {
            return false;
        }
        match pause_command(cap, pause) {
            PauseCommand::Stdin { key } => self.write_key(key as u8).await,
            #[cfg(unix)]
            PauseCommand::Signal { name } => match self.pid {
                Some(pid) => {
                    let sig = if name == "SIGSTOP" {
                        libc::SIGSTOP
                    } else {
                        libc::SIGCONT
                    };
                    group_signal(pid, sig);
                    true
                }
                None => false,
            },
            _ => false,
        }
    }
}

#[cfg(unix)]
fn group_signal(pid: u32, sig: i32) {
    unsafe { libc::kill(-(pid as i32), sig) };
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<String> {
        vec!["-c".into(), script.into()]
    }

    #[tokio::test]
    async fn stderr_ring_keeps_the_tail_and_exit_code() {
        let p = Ffmpeg::spawn(
            "sh",
            &sh("for i in $(seq 1 60); do echo line$i >&2; done; exit 3"),
        )
        .await
        .unwrap();
        let e = p.wait().await;
        assert_eq!(e.code, Some(3));
        tokio::time::sleep(Duration::from_millis(50)).await;
        let tail = p.stderr_tail();
        assert_eq!(tail.len(), RING_LINES);
        assert_eq!(tail.last().unwrap(), "line60");
    }

    #[tokio::test]
    async fn stop_terms_the_group_and_kills_a_deaf_process() {
        let p = Ffmpeg::spawn("sh", &sh("sleep 30")).await.unwrap();
        assert!(p.stop().await.signal);
        let deaf = Ffmpeg::spawn("sh", &sh("trap '' TERM; while :; do sleep 0.1; done"))
            .await
            .unwrap();
        // Let the shell install its trap first.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let t = std::time::Instant::now();
        assert!(deaf.stop().await.signal);
        assert!(t.elapsed() >= STOP_GRACE);
    }

    #[tokio::test]
    async fn signal_pause_stops_and_continues() {
        let p = Ffmpeg::spawn("sh", &sh("sleep 30")).await.unwrap();
        assert!(p.pause(PauseCapability::Signal, true).await);
        // A stopped process still stops on request.
        assert!(p.stop().await.signal);
        assert!(!p.pause(PauseCapability::None, true).await);
    }
}
