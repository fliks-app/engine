//! Runs the host as a child: restart with backoff, signal forwarding, zombie reaping (PID 1).
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};

const STABLE_AFTER: Duration = Duration::from_secs(60);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).unwrap();
        let mut int = signal(SignalKind::interrupt()).unwrap();
        tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await.unwrap();
}

#[cfg(unix)]
fn forward_term(child: &Child) {
    if let Some(pid) = child.id() {
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    }
}

#[cfg(not(unix))]
fn forward_term(_: &Child) {}

/// Reaps orphans re-parented to us, leaving our direct children (the host, ffmpeg...) to tokio.
#[cfg(unix)]
async fn reap_orphans(child_pid: impl Fn() -> u32) {
    use tokio::signal::unix::{SignalKind, signal};
    let mut chld = signal(SignalKind::child()).unwrap();
    while chld.recv().await.is_some() {
        loop {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let r = unsafe {
                libc::waitid(
                    libc::P_ALL,
                    0,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            let pid = unsafe { info.si_pid() };
            // ponytail: a zombie child at the head of the queue hides orphans behind it until the next SIGCHLD
            if r != 0 || pid == 0 || pid as u32 == child_pid() || crate::proc::is_own(pid as u32) {
                break;
            }
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
        }
    }
}

fn backoff_base() -> Duration {
    std::env::var("FLIKS_ENGINE_BACKOFF_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(Duration::from_secs(1), Duration::from_millis)
}

/// Returns the exit code of the engine process.
pub async fn run(cmd: &[String], url: &str, token: &str, extra_env: &[(&str, String)]) -> u8 {
    #[cfg(unix)]
    let current = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    #[cfg(unix)]
    if std::process::id() == 1 {
        let c = current.clone();
        tokio::spawn(reap_orphans(move || {
            c.load(std::sync::atomic::Ordering::Relaxed)
        }));
    }
    let base = backoff_base();
    let mut delay = base;
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        let started = Instant::now();
        let mut child = match Command::new(&cmd[0])
            .args(&cmd[1..])
            .env("FLIKS_ENGINE_URL", url)
            .env("FLIKS_ENGINE_TOKEN", token)
            .envs(extra_env.iter().map(|(k, v)| (k, v)))
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("spawn {}: {e}", cmd[0]);
                tokio::select! { _ = tokio::time::sleep(delay) => {}, _ = &mut shutdown => return 1 }
                delay = (delay * 2).min(base * 30);
                continue;
            }
        };
        #[cfg(windows)]
        crate::proc::assign_to_job(&child);
        #[cfg(unix)]
        current.store(
            child.id().unwrap_or(0),
            std::sync::atomic::Ordering::Relaxed,
        );
        tokio::select! {
            status = child.wait() => {
                tracing::warn!("host exited: {status:?}");
                if started.elapsed() >= STABLE_AFTER { delay = base; }
            }
            _ = &mut shutdown => return stop(&mut child).await,
        }
        tokio::select! { _ = tokio::time::sleep(delay) => {}, _ = &mut shutdown => return 0 }
        delay = (delay * 2).min(base * 30);
    }
}

async fn stop(child: &mut Child) -> u8 {
    forward_term(child);
    let status = match tokio::time::timeout(STOP_TIMEOUT, child.wait()).await {
        Ok(s) => s,
        Err(_) => {
            let _ = child.kill().await;
            child.wait().await
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Ok(s) = &status {
            return s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0)) as u8;
        }
    }
    status.ok().and_then(|s| s.code()).unwrap_or(1) as u8
}
