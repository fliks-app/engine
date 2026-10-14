//! Test stand-in for ffmpeg, ffprobe and fpcalc (never shipped: the release builds `--bin fliks-engine` only).
//! The mode is the first dash-separated word of the exe name (see `main`); job modes tell their tool by argv.
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
    if let (None, "flakysprite") = (rows, mode) {
        let n: u32 = args.last().unwrap()[..args.last().unwrap().len() - 4]
            .rsplit('-')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        if n.is_multiple_of(4) {
            std::process::exit(1)
        }
    }
    if rows.is_some() {
        // The image2 tiler stops at the first gap: refuse one.
        let pattern = &args[args.iter().position(|a| a == "-i").unwrap() + 1];
        let present: Vec<bool> = (1..=999)
            .map(|i| std::path::Path::new(&pattern.replace("%04d", &format!("{i:04}"))).exists())
            .collect();
        let first_gap = present.iter().position(|p| !p).unwrap();
        if present[first_gap..].iter().any(|p| *p) {
            eprintln!("gap in the frame sequence at {}", first_gap + 1);
            std::process::exit(1)
        }
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

const ASS: &str = include_str!("../../../../fixtures/jobs/subtitles/served.ass");

fn drain_stdin() {
    let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
}

/// ffprobe: the reorder lead, or the packets of a 120 s file (keyframes at 0 and 0.125 s).
fn probe(mode: &str, args: &[String]) {
    if args.iter().any(|a| a.starts_with("stream=")) {
        println!("has_b_frames=2|avg_frame_rate=24/1");
        return;
    }
    if args.last().is_some_and(|a| a == "pipe:0") {
        drain_stdin();
    }
    match mode {
        "failscan" => {
            eprintln!("fake ffprobe: boom");
            std::process::exit(1)
        }
        "hangscan" => {
            println!("pts_time=0.000000|dts_time=0.000000|duration_time=0.041667|flags=K_");
            let _ = std::io::stdout().flush();
            hang()
        }
        _ => {}
    }
    let lines = [
        "pts_time=0.000000|dts_time=-0.083333|duration_time=0.041667|flags=K_",
        "pts_time=0.083333|dts_time=-0.041667|duration_time=0.041667|flags=__",
        "pts_time=0.041667|dts_time=0.000000|duration_time=0.041667|flags=__",
        "pts_time=0.125000|dts_time=0.041667|duration_time=0.041667|flags=K_",
    ];
    for l in lines {
        println!("{l}");
    }
}

/// The ADTS ffmpeg of a scan: reads the file on stdin, writes two frames to each `pipe:N`.
fn adts(mode: &str, args: &[String]) {
    #[cfg(unix)]
    {
        use std::os::fd::FromRawFd;
        let outs = args
            .iter()
            .filter_map(|a| a.strip_prefix("pipe:")?.parse::<i32>().ok());
        let frame = |sf: u8| [0xFF, 0xF1, 0x40 | sf << 2, 0x80, 0x01, 0x1F, 0xFC, 0x00];
        let second = if mode == "scanchange" { 3 } else { 4 };
        for fd in outs.filter(|fd| *fd >= 3) {
            // SAFETY: the engine handed this process the fd.
            let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
            let _ = f.write_all(&[frame(4), frame(second)].concat());
        }
    }
    drain_stdin();
}

fn fpcalc(mode: &str, args: &[String]) {
    if args.last().is_some_and(|a| a == "-") {
        drain_stdin();
    }
    match mode {
        "failfp" => {
            eprintln!("fake fpcalc: boom");
            std::process::exit(2)
        }
        "hangfp" => hang(),
        _ => print!("DURATION=12.5\nFINGERPRINT=1,-2,3000000000\n"),
    }
}

/// The `-ss .. -f wav -` window run of a fingerprint: some bytes on stdout.
fn wav(mode: &str) {
    if mode == "hangfp" {
        hang()
    }
    let _ = std::io::stdout().write_all(&[0u8; 8820]);
}

/// Each `-y <out>` of an extract gets the ASS fixture (`pipe:1`: stdout).
fn subs(mode: &str, args: &[String]) {
    match mode {
        "failsub" => std::process::exit(1),
        "hangsub" => hang(),
        _ => {}
    }
    let body = if mode == "emptysub" { "" } else { ASS };
    for w in args.windows(2).filter(|w| w[0] == "-y") {
        if w[1] == "pipe:1" {
            let _ = std::io::stdout().write_all(body.as_bytes());
        } else {
            std::fs::write(&w[1], body).unwrap();
        }
    }
}

fn job(mode: &str, args: &[String]) {
    let has = |a: &str| args.iter().any(|x| x == a);
    if has("-show_entries") {
        probe(mode, args)
    } else if has("-raw") {
        fpcalc(mode, args)
    } else if has("wav") {
        wav(mode)
    } else if has("data") {
        adts(mode, args)
    } else if has("ass") {
        subs(mode, args)
    } else {
        hang()
    }
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
        m @ ("sprite" | "slowsprite" | "hangsprite" | "failsprite" | "flakysprite") => {
            sprite(m, &args)
        }
        m @ ("scan" | "scanchange" | "failscan" | "hangscan" | "fp" | "failfp" | "hangfp"
        | "sub" | "emptysub" | "failsub" | "hangsub") => job(m, &args),
        _ => hang(),
    }
}
