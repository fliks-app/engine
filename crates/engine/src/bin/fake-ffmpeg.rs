//! Test stand-in for ffmpeg (never shipped: the release builds `--bin fliks-engine` only). The
//! mode is the first dash-separated word of the exe name: `hang`, `busy`, `talk`, `early`.
use std::io::Write;
use std::time::{Duration, Instant};

fn hang() -> ! {
    // A hung ffmpeg ignores the polite stop; stdin is never read either.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn talk() -> ! {
    loop {
        let _ = writeln!(std::io::stderr(), "tick");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn main() {
    let exe = std::env::current_exe().unwrap();
    let stem = exe.file_stem().unwrap().to_string_lossy().into_owned();
    match stem.split('-').next().unwrap() {
        // CPU for 4 s with no file and no stderr, then hangs like the real bug.
        "busy" => {
            let t = Instant::now();
            while t.elapsed() < Duration::from_secs(4) {
                std::hint::black_box(0);
            }
            hang()
        }
        "talk" => talk(),
        // Only the early run (`-to`) keeps talking, so only its natural end can stop it.
        "early" if std::env::args().any(|a| a == "-to") => talk(),
        _ => hang(),
    }
}
