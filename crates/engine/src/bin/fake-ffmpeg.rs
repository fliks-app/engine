//! Test stand-in for ffmpeg, ffprobe and fpcalc (never shipped: the release builds `--bin fliks-engine` only).
//! The mode is the first dash-separated word of the exe name (see `main`); job modes tell their tool by argv.
use std::io::Write;
use std::path::Path;
use std::time::Duration;

/// A file other processes read while it is written: complete or absent, never partial.
fn put(path: &Path, bytes: impl AsRef<[u8]>) {
    let name = path.file_name().unwrap().to_string_lossy();
    let tmp = path.with_file_name(format!(".tmp-{name}"));
    std::fs::write(&tmp, bytes).unwrap();
    std::fs::rename(tmp, path).unwrap();
}

fn hang() -> ! {
    // A hung ffmpeg ignores the polite stop; stdin is never read either.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
    // Tells a test the handler is in place.
    eprintln!("ready");
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
        _ => std::thread::sleep(Duration::from_millis(50)),
    }
    let (w, h) = rows.map_or((240u16, 135u16), |r| (2400, 135 * r));
    let mut jpg = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08];
    jpg.extend(h.to_be_bytes());
    jpg.extend(w.to_be_bytes());
    jpg.extend([0x03; 10]);
    jpg.extend([0xFF, 0xD9]);
    put(Path::new(args.last().unwrap()), jpg);
}

const ASS: &str = include_str!("../../../../fixtures/jobs/subtitles/served.ass");

fn drain_stdin() {
    let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
}

/// ffprobe: the PIDs, the reorder lead, or the packets of a 120 s file (keyframes at 0 and 0.125 s).
fn probe(mode: &str, args: &[String]) {
    if args.iter().any(|a| a == "stream=index,codec_name") {
        println!("2,subrip\n3,ass\n4,hdmv_pgs_subtitle");
        return;
    }
    if args.iter().any(|a| a == "stream=index,id") {
        println!("index=0|id=0x100\nindex=1|id=0x101");
        return;
    }
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
        // One line per run in `<input>.runs`, and a run slow enough for requests to overlap.
        "slowsub" => {
            let i = args.iter().position(|a| a == "-i").unwrap() + 1;
            let mut runs = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(format!("{}.runs", args[i]))
                .unwrap();
            writeln!(runs).unwrap();
            std::thread::sleep(Duration::from_millis(400));
        }
        _ => {}
    }
    let body = if mode == "emptysub" { "" } else { ASS };
    for w in args.windows(2).filter(|w| w[0] == "-y") {
        if w[1] == "pipe:1" {
            let _ = std::io::stdout().write_all(body.as_bytes());
        } else {
            put(Path::new(&w[1]), body);
        }
    }
}

const OCR_SRT: &str = "1\n00:00:00,000 --> 00:00:01,000\nhello\n";

/// `mkvextract` writes `id:path`. A drive letter is also a colon, so only a numeric id is stripped.
fn track_output(spec: &str) -> &str {
    match spec.split_once(':') {
        Some((id, rest)) if !rest.is_empty() && id.chars().all(|c| c.is_ascii_digit()) => rest,
        _ => spec,
    }
}

/// PGS/VobSub OCR tools, distinguished by argv (one fake binary on PATH for all of them).
fn ocr(args: &[String]) {
    let has = |a: &str| args.iter().any(|x| x == a);
    let done = || {
        let _ = writeln!(std::io::stderr(), "ok");
    };
    if has("-map") && has("-c:s") {
        put(Path::new(args.last().unwrap()), b"");
        done();
        return;
    }
    if has("--language") {
        let sup = Path::new(args.last().unwrap());
        put(&sup.with_extension("srt"), OCR_SRT);
        done();
        return;
    }
    if args.first().is_some_and(|a| a == "tracks") {
        let idx = Path::new(track_output(args.last().unwrap()));
        put(idx, b"");
        put(&idx.with_extension("sub"), b"");
        done();
        return;
    }
    if has("-l") && has("-o") {
        let i = args.iter().position(|a| a == "-o").unwrap();
        put(Path::new(&args[i + 1]), OCR_SRT);
        done();
        return;
    }
    hang()
}

/// Canned ffsubsync. `NOSPEECH` in the input fails until `--reference-stream`; `FAIL` always fails.
fn ffsubsync(args: &[String]) {
    let input = args
        .windows(2)
        .find(|w| w[0] == "-i")
        .map(|w| w[1].as_str());
    let output = args
        .windows(2)
        .find(|w| w[0] == "-o")
        .map(|w| w[1].as_str());
    let (Some(input), Some(output)) = (input, output) else {
        hang()
    };
    let body = std::fs::read_to_string(input).unwrap_or_default();
    let streamed = args.windows(2).any(|w| w[0] == "--reference-stream");
    if body.contains("NOSPEECH") && !streamed {
        eprintln!("Unable to detect speech");
        std::process::exit(1);
    }
    if body.contains("FAIL") {
        eprintln!("ffsubsync boom");
        std::process::exit(1);
    }
    put(Path::new(output), body.replace("LATE", "SYNCED"));
}

fn job(mode: &str, args: &[String]) {
    let has = |a: &str| args.iter().any(|x| x == a);
    if has("-show_entries") {
        probe(mode, args)
    } else if has("-raw") {
        fpcalc(mode, args)
    } else if has("wav") {
        wav(mode)
    } else if has("ass") {
        subs(mode, args)
    } else {
        hang()
    }
}

/// A VOD run that seeked past the end: an init and a sample-less first segment, exit 0.
fn nomedia(args: &[String]) {
    let after = |flag: &str| &args[args.iter().position(|a| a == flag).unwrap() + 1];
    let dir = Path::new(after("-hls_segment_filename")).parent().unwrap();
    let start: u32 = after("-start_number").parse().unwrap();
    let boxed = |kind: &[u8; 4], body: &[u8]| {
        let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend(kind);
        b.extend(body);
        b
    };
    let trun = boxed(b"trun", &[0; 8]);
    let moof = boxed(b"moof", &boxed(b"traf", &trun));
    put(&dir.join("init.mp4"), b"init");
    put(
        &dir.join(format!("seg-{start:04}.m4s")),
        [moof, boxed(b"mdat", &[])].concat(),
    );
}

/// Live TV by URL: `fail` exits, `slowstart` never writes, `stall` writes one segment then goes
/// silent, `dies` writes three then exits once `die` exists in the run's directory.
fn live(args: &[String]) {
    let url = args.last().unwrap();
    if args.iter().any(|a| a == "csv=p=0") {
        match () {
            _ if url.contains("noprobe") => std::process::exit(1),
            _ if url.contains("hevc") => print!("hevc,video\naac,audio\nmpegts\n"),
            _ if url.contains("manifest") => print!("h264,video\naac,audio\nhls\n"),
            _ => print!("h264,video\naac,audio\nmpegts\n"),
        }
        return;
    }
    let at = |flag: &str| &args[args.iter().position(|a| a == flag).unwrap() + 1];
    let input = at("-i").clone();
    let pattern = at("-hls_segment_filename").clone();
    let dir = std::path::Path::new(&pattern)
        .parent()
        .unwrap()
        .to_path_buf();
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("argv.log"))
        .unwrap();
    writeln!(log, "{}", serde_json::to_string(args).unwrap()).unwrap();
    if input.contains("fail") {
        eprintln!("fake ffmpeg: {input}: Connection refused");
        std::process::exit(1)
    }
    if input.contains("slowstart") {
        hang()
    }
    let ext = if pattern.ends_with(".ts") {
        "ts"
    } else {
        "m4s"
    };
    let fmp4 = ext == "m4s";
    let existing = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let n = e.unwrap().file_name().to_string_lossy().into_owned();
            n.strip_prefix("seg-")?
                .split('.')
                .next()?
                .parse::<u32>()
                .ok()
        })
        .max();
    let append = at("-hls_flags").contains("append_list");
    let first = if append {
        existing.map_or(0, |m| m + 1)
    } else {
        0
    };
    if fmp4 {
        put(&dir.join("init.mp4"), b"init");
    }
    let mut m3u8 = format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:{first}\n"
    );
    if fmp4 {
        m3u8.push_str("#EXT-X-MAP:URI=\"init.mp4\"\n");
    }
    for n in first.. {
        let written = n - first;
        if input.contains("stall") && written == 1 {
            hang()
        }
        if input.contains("dies") && written == 3 {
            while !dir.join("die").exists() {
                std::thread::sleep(Duration::from_millis(20));
            }
            eprintln!("fake ffmpeg: {input}: end of file");
            std::process::exit(1)
        }
        put(
            &dir.join(format!("seg-{n:05}.{ext}")),
            format!("segment {n}"),
        );
        m3u8.push_str(&format!("#EXTINF:2.000000,\nseg-{n:05}.{ext}\n"));
        put(&dir.join("index.m3u8"), &m3u8);
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn main() {
    let exe = std::env::current_exe().unwrap();
    let stem = exe.file_stem().unwrap().to_string_lossy().into_owned();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if stem == "ffsubsync" {
        ffsubsync(&args);
        return;
    }
    // "subtile-ocr" would otherwise be read as the mode "subtile".
    if stem.starts_with("subtile-ocr")
        || matches!(
            stem.split('-').next().unwrap(),
            "ocr" | "pgsrip" | "mkvextract"
        )
    {
        ocr(&args);
        return;
    }
    match stem.split('-').next().unwrap() {
        // CPU with no file and no stderr until `<exe>.rest` exists, then hangs like the real bug.
        "busy" => {
            let rest = exe.with_extension("rest");
            while !rest.exists() {
                (0..100_000).for_each(|i| {
                    std::hint::black_box(i);
                });
            }
            hang()
        }
        "talk" => talk(),
        // Dies on a VAAPI command line, hangs on the CPU one.
        "hwfail" if args.iter().any(|a| a.contains("vaapi")) => std::process::exit(1),
        "live" => live(&args),
        "nomedia" => nomedia(&args),
        // Only the early run (`-to`) keeps talking, so only its natural end can stop it.
        "early" if args.iter().any(|a| a == "-to") => talk(),
        m @ ("sprite" | "hangsprite" | "failsprite" | "flakysprite") => sprite(m, &args),
        m @ ("scan" | "failscan" | "hangscan" | "fp" | "failfp" | "hangfp" | "sub" | "emptysub"
        | "failsub" | "hangsub" | "slowsub") => job(m, &args),
        "sync" => {
            if args.iter().any(|a| a == "pcm_s16le") {
                eprintln!("no audio");
                std::process::exit(1);
            }
            hang()
        }
        "srt" => {
            let _ = writeln!(std::io::stdout(), "1\n00:00:01,000 --> 00:00:02,000\nhello");
            let _ = std::io::stdout().flush();
        }
        _ => hang(),
    }
}

#[cfg(test)]
mod tests {
    use super::track_output;

    #[test]
    fn track_output_keeps_a_drive_letter() {
        assert_eq!(track_output("6:/cache/a.idx"), "/cache/a.idx");
        assert_eq!(track_output(r"6:C:\refs\a.idx"), r"C:\refs\a.idx");
    }
}
