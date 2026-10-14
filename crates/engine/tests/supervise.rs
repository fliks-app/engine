#![cfg(unix)]
use std::{
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
