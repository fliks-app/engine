//! Test stand-in for ffmpeg (never shipped: the release builds `--bin fliks-engine` only). The
//! mode is the first dash-separated word of the exe name: `hang`, `busy`, `talk`, `early`, and the
//! sprite modes `sprite`, `slowsprite`, `hangsprite`, `failsprite` (other runs hang).
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

/// A frame (240x135) or tile (10 x rows of them) run: a JPEG header at the output path.
fn sprite(mode: &str, args: &[String]) {
    let rows = args
        .iter()
        .find_map(|a| a.strip_prefix("tile=10x"))
        .map(|r| r.parse::<u16>().unwrap());
    if rows.is_none() && !args.iter().any(|a| a == "-frames:v") {
        hang()
    }
    match (mode, rows) {
        ("hangsprite", None) => hang(),
        ("failsprite", None) => std::process::exit(1),
        ("slowsprite", None) => std::thread::sleep(Duration::from_millis(1000)),
        _ => std::thread::sleep(Duration::from_millis(50)),
    }
    let (w, h) = rows.map_or((240u16, 135u16), |r| (2400, 135 * r));
    let mut jpg = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08];
    jpg.extend(h.to_be_bytes());
    jpg.extend(w.to_be_bytes());
    jpg.extend([0x03; 10]);
    jpg.extend([0xFF, 0xD9]);
    std::fs::write(args.last().unwrap(), jpg).unwrap();
}

fn main() {
    let exe = std::env::current_exe().unwrap();
    let stem = exe.file_stem().unwrap().to_string_lossy().into_owned();
    let args: Vec<String> = std::env::args().skip(1).collect();
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
        "early" if args.iter().any(|a| a == "-to") => talk(),
        m @ ("sprite" | "slowsprite" | "hangsprite" | "failsprite") => sprite(m, &args),
        _ => hang(),
    }
}
