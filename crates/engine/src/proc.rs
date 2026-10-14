//! Child process helpers shared by every ffmpeg/ffprobe/mkvmerge caller, and the lifecycle of
//! long ffmpeg runs (`docs/spec/runtime.md` section 1).
use fliks_engine_core::session::{PauseCapability, PauseCommand, pause_command};
use std::collections::VecDeque;
use std::io;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, watch};

/// `Command::spawn`, retried briefly on ETXTBSY: a concurrent fork can still hold a just-written binary open.
pub async fn spawn(cmd: &mut Command) -> io::Result<Child> {
    // A SIGKILLed or crashed engine runs no shutdown path: the kernel kills the child instead.
    #[cfg(target_os = "linux")]
    // SAFETY: prctl is async-signal-safe and touches no memory.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
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

#[cfg(target_os = "linux")]
fn on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
}

/// `ionice -c3 nice -n19` in front of a background run, where the host has them.
pub fn background(cmd: &str, args: &[String]) -> (String, Vec<String>) {
    #[cfg(target_os = "linux")]
    if on_path("ionice") && on_path("nice") {
        let rest = ["-c3", "nice", "-n19", cmd].map(String::from);
        return (
            "ionice".into(),
            rest.into_iter().chain(args.iter().cloned()).collect(),
        );
    }
    (cmd.into(), args.to_vec())
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

/// What a run exposes besides stderr: its stdout, and (unix) output pipes `pipe:3`, `pipe:4`, ...
#[derive(Clone, Copy, Default)]
pub struct Pipes {
    pub stdout: bool,
    pub fds: usize,
}

#[cfg(unix)]
pub type FdReader = tokio::net::unix::pipe::Receiver;
#[cfg(not(unix))]
pub type FdReader = tokio::io::Empty;

/// One running ffmpeg: its own process group (unix) or the kill-on-close job (windows).
/// Dropping the handle kills it.
pub struct Ffmpeg {
    // Signalled on unix; windows only reads its CPU time and stops through the job and stdin.
    pid: Option<u32>,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    stdout: Mutex<Option<ChildStdout>>,
    fds: Mutex<Vec<Option<FdReader>>>,
    stderr: Arc<Mutex<VecDeque<String>>>,
    stderr_lines: Arc<AtomicU64>,
    exit: watch::Receiver<Option<Exit>>,
    kill: mpsc::Sender<()>,
}

/// Gives the child `n` pipes as fds 3.. and returns their read ends.
#[cfg(unix)]
fn fd_pipes(cmd: &mut Command, n: usize) -> io::Result<Vec<FdReader>> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let mut readers = vec![];
    let mut writers: Vec<OwnedFd> = vec![];
    for _ in 0..n {
        let (tx, rx) = tokio::net::unix::pipe::pipe()?;
        let w = tx.into_blocking_fd()?;
        // Above the target range, so no dup2 below overwrites a source fd.
        // SAFETY: a valid fd and a minimum; the new fd is owned at once.
        let hi = unsafe { libc::fcntl(w.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3 + n as i32) };
        if hi < 0 {
            return Err(io::Error::last_os_error());
        }
        writers.push(unsafe { OwnedFd::from_raw_fd(hi) });
        readers.push(rx);
    }
    if n > 0 {
        // SAFETY: dup2 only (async-signal-safe).
        unsafe {
            cmd.pre_exec(move || {
                for (i, w) in writers.iter().enumerate() {
                    if libc::dup2(w.as_raw_fd(), 3 + i as i32) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }
    Ok(readers)
}

#[cfg(not(unix))]
fn fd_pipes(_: &mut Command, _: usize) -> io::Result<Vec<FdReader>> {
    Ok(vec![])
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
        Self::spawn_with(bin, args, Pipes::default()).await
    }

    pub async fn spawn_with(bin: &str, args: &[String], pipes: Pipes) -> io::Result<Self> {
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(if pipes.stdout {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        let fds = fd_pipes(&mut cmd, pipes.fds)?;
        #[cfg(not(unix))]
        if pipes.fds > 0 {
            return Err(io::ErrorKind::Unsupported.into());
        }
        let mut child = spawn(&mut cmd).await?;
        // The children hold the write ends now: EOF reaches the readers when they exit.
        drop(cmd);
        #[cfg(windows)]
        assign_to_job(&child);
        let stderr = Arc::new(Mutex::new(VecDeque::new()));
        let stderr_lines = Arc::new(AtomicU64::new(0));
        if let Some(err) = child.stderr.take() {
            let (ring, count) = (stderr.clone(), stderr_lines.clone());
            tokio::spawn(async move {
                let mut lines = BufReader::new(err).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    count.fetch_add(1, Ordering::Relaxed);
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
        let stdout = child.stdout.take();
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
            stdout: Mutex::new(stdout),
            fds: Mutex::new(fds.into_iter().map(Some).collect()),
            stderr,
            stderr_lines,
            exit,
            kill,
        })
    }

    /// The stdin the run reads its input from (no key writes after this: no stdin pause).
    pub async fn take_stdin(&self) -> Option<ChildStdin> {
        self.stdin.lock().await.take()
    }

    pub fn take_stdout(&self) -> Option<ChildStdout> {
        self.stdout.lock().unwrap().take()
    }

    pub fn take_fd(&self, i: usize) -> Option<FdReader> {
        self.fds.lock().unwrap().get_mut(i)?.take()
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

    /// Lines written to stderr so far: a progress signal for the watchdog.
    pub fn stderr_lines(&self) -> u64 {
        self.stderr_lines.load(Ordering::Relaxed)
    }

    /// CPU time (user + system, threads and reaped children) burnt so far.
    #[cfg(target_os = "linux")]
    pub fn cpu_time_ms(&self) -> Option<u64> {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", self.pid?)).ok()?;
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
        Some(stat_cpu_ticks(&stat)? * 1000 / ticks)
    }

    /// `proc_pid_rusage` counts in mach ticks, hence the timebase.
    #[cfg(target_os = "macos")]
    #[allow(deprecated)]
    pub fn cpu_time_ms(&self) -> Option<u64> {
        let (mut ri, mut tb): (libc::rusage_info_v2, libc::mach_timebase_info_data_t) =
            unsafe { std::mem::zeroed() };
        let buf = &mut ri as *mut _ as *mut libc::rusage_info_t;
        let ok = unsafe {
            libc::mach_timebase_info(&mut tb) == 0
                && tb.denom != 0
                && libc::proc_pid_rusage(self.pid? as i32, libc::RUSAGE_INFO_V2, buf) == 0
        };
        let ticks = ri.ri_user_time + ri.ri_system_time;
        let ticks = ticks + ri.ri_child_user_time + ri.ri_child_system_time;
        ok.then(|| (ticks as u128 * tb.numer as u128 / tb.denom as u128 / 1_000_000) as u64)
    }

    /// CPU time (user + kernel) burnt so far, from `GetProcessTimes`.
    #[cfg(windows)]
    pub fn cpu_time_ms(&self) -> Option<u64> {
        use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
        use windows_sys::Win32::System::Threading::*;
        let ticks = |t: FILETIME| (t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64;
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, self.pid?);
            if h.is_null() {
                return None;
            }
            let mut t: [FILETIME; 4] = std::mem::zeroed();
            let ok = GetProcessTimes(h, &mut t[0], &mut t[1], &mut t[2], &mut t[3]);
            CloseHandle(h);
            // FILETIME counts 100 ns.
            (ok != 0).then(|| (ticks(t[2]) + ticks(t[3])) / 10_000)
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    pub fn cpu_time_ms(&self) -> Option<u64> {
        None
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

/// utime + stime + cutime + cstime of a `/proc/<pid>/stat` line (the name may hold spaces).
#[cfg(target_os = "linux")]
fn stat_cpu_ticks(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace()
        .skip(11)
        .take(4)
        .map(|f| f.parse::<u64>().ok())
        .sum()
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

    #[cfg(target_os = "linux")]
    #[test]
    fn cpu_ticks_come_from_fields_14_to_17() {
        let line = "42 (ff mpeg) ) S 1 42 42 0 -1 4194560 100 0 0 0 7 5 3 1 20 0 4 0 99 0";
        assert_eq!(stat_cpu_ticks(line), Some(16));
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
