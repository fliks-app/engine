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
