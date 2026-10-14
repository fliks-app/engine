#![cfg(unix)]
use std::{
    io::BufRead,
    process::Command,
    time::{Duration, Instant},
};

#[test]
fn restarts_failing_host() {
    let dir = std::env::temp_dir().join(format!("fliks-sup-{}", std::process::id()));
    let log = dir.with_extension("log");
    let mut engine = Command::new(env!("CARGO_BIN_EXE_fliks-engine"))
        .env("FLIKS_ENGINE_BACKOFF_MS", "20")
        .args(["--supervise", "--", "sh", "-c"])
        .arg(format!(
            "echo \"$FLIKS_ENGINE_URL\" >> {}; exit 1",
            log.display()
        ))
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let runs = loop {
        let n = std::fs::read_to_string(&log).map_or(0, |s| s.lines().count());
        if n >= 2 || start.elapsed() > Duration::from_secs(10) {
            break n;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    engine.kill().unwrap();
    engine.wait().unwrap();
    let urls = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    assert!(runs >= 2, "host not restarted");
    assert!(urls.starts_with("http://127.0.0.1:"));
}

#[test]
fn upstream_sets_the_host_port_and_serves_the_public_listener() {
    let out = std::env::temp_dir().join(format!("fliks-port-{}", std::process::id()));
    let mut engine = Command::new(env!("CARGO_BIN_EXE_fliks-engine"))
        .args(["--supervise", "--upstream", "http://10.1.2.3:4321"])
        .args(["--public-listen", "127.0.0.1:0", "--", "sh", "-c"])
        .arg(format!("echo \"$PORT\" > {}; sleep 30", out.display()))
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = std::io::BufReader::new(engine.stdout.take().unwrap()).lines();
    let public = lines.find_map(|l| {
        l.ok()?
            .strip_prefix("FLIKS_ENGINE_PUBLIC_LISTENING ")
            .map(String::from)
    });
    let start = Instant::now();
    while !out.exists() && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let port = std::fs::read_to_string(&out).unwrap_or_default();
    engine.kill().unwrap();
    engine.wait().unwrap();
    let _ = std::fs::remove_file(&out);
    assert_eq!(port.trim(), "4321");
    assert!(public.is_some_and(|a| a.starts_with("127.0.0.1:")));
}

/// A SIGKILLed engine runs no shutdown path: its ffmpeg children must die with it.
#[cfg(target_os = "linux")]
#[test]
fn a_killed_engine_leaves_no_ffmpeg() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("fliks-pdeath-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (fake, pids) = (dir.join("ffmpeg"), dir.join("pids"));
    std::fs::write(
        &fake,
        format!("#!/bin/sh\necho $$ >> {}\nexec sleep 600\n", pids.display()),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut engine = Command::new(env!("CARGO_BIN_EXE_fliks-engine"))
        .env("FFMPEG_PATH", &fake)
        .args(["--supervise", "--", "sleep", "5"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let alive = |pid: &str| {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .is_ok_and(|s| s.rsplit(") ").next().is_some_and(|r| !r.starts_with('Z')))
    };
    let child = loop {
        match std::fs::read_to_string(&pids)
            .ok()
            .and_then(|s| s.lines().next().map(String::from))
        {
            Some(p) => break p,
            None if start.elapsed() > Duration::from_secs(10) => panic!("engine never ran ffmpeg"),
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    assert!(alive(&child));
    engine.kill().unwrap();
    engine.wait().unwrap();
    let start = Instant::now();
    while alive(&child) && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let left = alive(&child);
    // Whatever the probes started is gone too.
    for p in std::fs::read_to_string(&pids).unwrap_or_default().lines() {
        if alive(p) {
            let _ = Command::new("kill").args(["-9", p]).status();
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(!left, "ffmpeg {child} outlived the engine");
}
