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

/// What the MP4 muxer leaves for a `Merge`: `ftyp`, `mdat`, then `moov` with an `ilst`.
fn m4b() -> Vec<u8> {
    use fliks_engine_core::mp4::make_box;
    let part = |kind: &[u8; 4], kids: &[&[u8]]| make_box(kind, kids).unwrap();
    let ilst = part(b"ilst", &[&part(b"\xA9too", &[b"fake"])]);
    let meta = part(b"meta", &[&[0; 4], &ilst]);
    let moov = part(b"moov", &[&part(b"udta", &[&meta])]);
    [part(b"ftyp", &[b"M4B "]), part(b"mdat", &[b"audio"]), moov].concat()
}

/// A merge run: progress lines on stdout, then the output (last argument) or, for `failmerge`, a
/// partial one and an exit code.
fn merge(mode: &str, args: &[String]) -> ! {
    let out = Path::new(args.last().unwrap());
    if mode != "merge" {
        std::fs::write(out, b"partial").unwrap();
        if mode == "hangmerge" {
            hang()
        }
        std::process::exit(1)
    }
    for us in [1_000_000, 2_000_000] {
        let _ = writeln!(std::io::stdout(), "out_time_us={us}\nprogress=continue");
        let _ = std::io::stdout().flush();
        std::thread::sleep(Duration::from_millis(300));
    }
    std::fs::write(out, m4b()).unwrap();
    std::process::exit(0)
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

/// A JPEG that is only a start-of-frame header of `w` x `h`.
fn jpeg_header(w: u16, h: u16) -> Vec<u8> {
    let mut jpg = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08];
    jpg.extend(h.to_be_bytes());
    jpg.extend(w.to_be_bytes());
    jpg.extend([0x03; 10]);
    jpg.extend([0xFF, 0xD9]);
    jpg
}

/// The sprite's single pass: logs `keyframes` (`-skip_frame nokey`), `keyframes-bsf` (the VP9
/// packet drop) or `full` to `<input>.pass`, then writes the `-frames:v` frames of the pattern
/// with `-progress` lines. `hangsprite` hangs, `promopasssprite` hangs in its first pass only,
/// `failsprite` fails, `passfailsprite` writes 8
/// half-written frames then fails (the job falls back to frame runs), `flakysprite` skips every
/// 4th frame.
fn sprite_pass(mode: &str, args: &[String]) {
    let after = |flag: &str| &args[args.iter().position(|a| a == flag).unwrap() + 1];
    let decode = if args.iter().any(|a| a == "nokey") {
        "keyframes"
    } else if args.iter().any(|a| a == "noise=drop=not(key)") {
        "keyframes-bsf"
    } else {
        "full"
    };
    assert!(
        args.windows(2).any(|w| w == ["-fps_mode", "passthrough"]),
        "the pass keeps fps's timestamps"
    );
    let passes = append_line(&format!("{}.pass", after("-i")), decode);
    if mode == "promopasssprite" && passes == 1 {
        println!("out_time_us=0");
        let _ = std::io::stdout().flush();
        hang()
    }
    match mode {
        "hangsprite" => {
            println!("out_time_us=0");
            let _ = std::io::stdout().flush();
            hang()
        }
        "failsprite" => std::process::exit(1),
        "passfailsprite" => {
            let pattern = args.last().unwrap();
            for i in 1..=8 {
                put(
                    Path::new(&pattern.replace("%04d", &format!("{i:04}"))),
                    b"\xFF",
                );
            }
            std::process::exit(1)
        }
        _ => {}
    }
    let count: u32 = after("-frames:v").parse().unwrap();
    let interval: u64 = args
        .iter()
        .find_map(|a| a.split("fps=1/").nth(1))
        .and_then(|s| s.split(':').next()?.parse().ok())
        .unwrap();
    let pattern = args.last().unwrap();
    for i in 1..=count {
        if mode == "flakysprite" && i.is_multiple_of(4) {
            continue;
        }
        put(
            Path::new(&pattern.replace("%04d", &format!("{i:04}"))),
            jpeg_header(240, 135),
        );
        if i.is_multiple_of(10) {
            println!("out_time_us={}", u64::from(i - 1) * interval * 1_000_000);
            let _ = std::io::stdout().flush();
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    println!("progress=end");
}

/// Appends `line` to the log at `path`; returns its line count after.
fn append_line(path: &str, line: &str) -> usize {
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    // One write: `writeln!` splits the text and the newline, and parallel runs interleave them.
    let _ = log.write_all(format!("{line}\n").as_bytes());
    drop(log);
    std::fs::read_to_string(path).unwrap().lines().count()
}

/// A frame (240x135) or tile (10 x rows of them) run: a JPEG header at the output path, each
/// frame run's number appended to `<input>.seeks`. The keyframe probe sees a keyframe every 2 s
/// (`sparsesprite`: 10 s) over its `-read_intervals` window, from the one before its start;
/// `probefailsprite` fails it, `promoprobesprite` hangs in the first one (logged to
/// `<input>.probes`). The single pass is `sprite_pass`.
fn sprite(mode: &str, args: &[String]) {
    if args.iter().any(|a| a == "packet=pts_time,dts_time,flags") {
        if mode == "probefailsprite" {
            std::process::exit(1)
        }
        let probes = format!("{}.probes", args.last().unwrap());
        if mode == "promoprobesprite" && append_line(&probes, "probe") == 1 {
            hang()
        }
        let window = &args[args.iter().position(|a| a == "-read_intervals").unwrap() + 1];
        let (start, len) = window.split_once("%+").unwrap();
        let (start, len): (f64, f64) = (start.parse().unwrap(), len.parse().unwrap());
        let gap = if mode == "sparsesprite" { 10.0 } else { 2.0 };
        let mut t = (start / gap).floor() * gap;
        while t <= start + len {
            println!("{t:.6},{t:.6},K__");
            println!("{:.6},N/A,___", t + 1.0);
            t += gap;
        }
        return;
    }
    if args.iter().any(|a| a == "-progress") {
        return sprite_pass(mode, args);
    }
    let rows = args
        .iter()
        .find_map(|a| a.strip_prefix("tile=10x"))
        .map(|r| r.parse::<u16>().unwrap());
    if rows.is_none() && !args.iter().any(|a| a == "-frames:v") {
        hang()
    }
    if rows.is_none() {
        let input = &args[args.iter().position(|a| a == "-i").unwrap() + 1];
        let n: u32 = args.last().unwrap()[..args.last().unwrap().len() - 4]
            .rsplit('-')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        append_line(&format!("{input}.seeks"), &n.to_string());
        if matches!(mode, "flakysprite" | "passfailsprite") && n.is_multiple_of(4) {
            std::process::exit(1)
        }
    }
    if rows.is_some() {
        // The image2 tiler stops at the first gap and fails on a broken JPEG: refuse both.
        let pattern = &args[args.iter().position(|a| a == "-i").unwrap() + 1];
        let frame = |i: usize| pattern.replace("%04d", &format!("{i:04}"));
        let present: Vec<bool> = (1..=999)
            .map(|i| std::path::Path::new(&frame(i)).exists())
            .collect();
        let first_gap = present.iter().position(|p| !p).unwrap();
        if present[first_gap..].iter().any(|p| *p) {
            eprintln!("gap in the frame sequence at {}", first_gap + 1);
            std::process::exit(1)
        }
        for i in 1..=first_gap {
            if !std::fs::read(frame(i)).unwrap().starts_with(&[0xFF, 0xD8]) {
                eprintln!("broken frame {i}");
                std::process::exit(1)
            }
        }
    }
    match (mode, rows) {
        ("hangsprite", None) => hang(),
        ("failsprite", None) => std::process::exit(1),
        _ => std::thread::sleep(Duration::from_millis(50)),
    }
    let (w, h) = rows.map_or((240u16, 135u16), |r| (2400, 135 * r));
    put(Path::new(args.last().unwrap()), jpeg_header(w, h));
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
        // The packet run hangs while niced (idle class); it logs its nice to `<input>.runs`.
        "idlescan" => {
            let nice = std::fs::read_to_string("/proc/self/stat")
                .ok()
                .and_then(|t| {
                    t.rsplit(')')
                        .next()?
                        .split(' ')
                        .nth(17)?
                        .parse::<i32>()
                        .ok()
                });
            let input = args.last().unwrap();
            let mut runs = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(format!("{input}.runs"))
                .unwrap();
            let _ = writeln!(runs, "{}", nice.unwrap_or(0));
            if nice == Some(19) {
                println!("pts_time=0.000000|dts_time=0.000000|duration_time=0.041667|flags=K_");
                let _ = std::io::stdout().flush();
                hang()
            }
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

/// Crop probe: ffprobe's stream run reports 100 s of `av1` (if the file says so) else `h264`, its
/// other runs fail; a crop sample logs its `-hwaccel` to `<input>.hw` and finds a letterbox.
fn crop(args: &[String]) {
    if args.iter().any(|a| a == "-show_streams") {
        let body = std::fs::read_to_string(args.last().unwrap()).unwrap_or_default();
        let codec = if body.contains("av1") { "av1" } else { "h264" };
        let stream = format!(
            r#"{{"index":0,"codec_name":"{codec}","codec_type":"video","width":480,"height":270,"pix_fmt":"yuv420p"}}"#
        );
        print!(
            r#"{{"streams":[{stream}],"format":{{"format_name":"mov,mp4","duration":"100.0"}}}}"#
        );
        return;
    }
    let (Some(i), true) = (
        args.iter().position(|a| a == "-i"),
        args.iter().any(|a| a.contains("cropdetect")),
    ) else {
        std::process::exit(1)
    };
    if let Some(h) = args.iter().position(|a| a == "-hwaccel") {
        append_line(&format!("{}.hw", args[i + 1]), &args[h + 1]);
    }
    eprintln!("[Parsed_cropdetect_1 @ 0x0] w:480 h:190 x:0 y:40 pts:0 t:0.0 crop=480:190:0:40");
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

/// A VOD run: on VAAPI an init and two segments from `-start_number`, then it dies; on the CPU
/// an init of its own and one segment, then it hangs.
fn hwlate(args: &[String]) -> ! {
    let after = |flag: &str| &args[args.iter().position(|a| a == flag).unwrap() + 1];
    let dir = Path::new(after("-hls_segment_filename")).parent().unwrap();
    let start: u32 = after("-start_number").parse().unwrap();
    let vaapi = args.iter().any(|a| a.contains("vaapi"));
    put(&dir.join("init.mp4"), if vaapi { "vaapi" } else { "cpu" });
    for n in start..start + if vaapi { 2 } else { 1 } {
        put(&dir.join(format!("seg-{n:04}.m4s")), "segment");
    }
    if vaapi {
        std::process::exit(1)
    }
    hang()
}

/// Logs a run's argv to `argv.log` in its directory and returns the directory.
fn log_argv(args: &[String]) -> &Path {
    let at = |flag: &str| &args[args.iter().position(|a| a == flag).unwrap() + 1];
    let dir = Path::new(at("-hls_segment_filename")).parent().unwrap();
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("argv.log"))
        .unwrap();
    writeln!(log, "{}", serde_json::to_string(args).unwrap()).unwrap();
    dir
}

/// One part's run of an item: every segment its `-t` covers, written at once as MPEG-TS from
/// `-start_number`, with the argv logged.
fn item(args: &[String]) {
    let at = |flag: &str| &args[args.iter().position(|a| a == flag).unwrap() + 1];
    let dir = log_argv(args);
    let start: u32 = at("-start_number").parse().unwrap();
    let count = (at("-t").parse::<f64>().unwrap() / 6.0).ceil() as u32;
    for n in start..start + count {
        put(&dir.join(format!("seg-{n:04}.ts")), format!("segment {n}"));
    }
}

/// An item run alive on its second segment: a SIGTERM closes it under its final name, like the
/// muxer's trailer does, a SIGKILL leaves the `.tmp` behind.
#[cfg(unix)]
fn itemstop(args: &[String]) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static TERM: AtomicBool = AtomicBool::new(false);
    extern "C" fn on_term(_: libc::c_int) {
        TERM.store(true, Ordering::Relaxed);
    }
    unsafe { libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t) };
    let at = |flag: &str| &args[args.iter().position(|a| a == flag).unwrap() + 1];
    let dir = log_argv(args);
    let start: u32 = at("-start_number").parse().unwrap();
    put(
        &dir.join(format!("seg-{start:04}.ts")),
        format!("segment {start}"),
    );
    let open = dir.join(format!("seg-{:04}.ts.tmp", start + 1));
    std::fs::write(&open, "partial").unwrap();
    while !TERM.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::rename(&open, dir.join(format!("seg-{:04}.ts", start + 1))).unwrap();
}

/// Alive until a SIGTERM, which flushes `seg` (and a playlist if none) as the muxer's trailer does;
/// a SIGKILL leaves nothing. `said` goes to stderr once the handler is in place.
fn flush_on_term(dir: &std::path::Path, seg: &str, said: &str) -> ! {
    #[cfg(unix)]
    static TERM: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    #[cfg(unix)]
    {
        extern "C" fn on_term(_: libc::c_int) {
            TERM.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        unsafe { libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t) };
    }
    eprintln!("{said}");
    #[cfg(unix)]
    {
        while !TERM.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::write(dir.join(seg), "flushed").unwrap();
        if !dir.join("index.m3u8").exists() {
            std::fs::write(dir.join("index.m3u8"), "#EXTM3U\n").unwrap();
        }
        std::process::exit(255)
    }
    #[cfg(not(unix))]
    {
        let _ = (dir, seg);
        hang()
    }
}

/// Live TV by URL: `fail`, `badchain`, `slowstart`, `slowts`, `stall`, `dies` (at `die`), `10bit`,
/// `gpudies`/`gpuhangs`/`gpurefuses` (GPU runs); with `flush`, a polite stop writes a segment.
fn live(args: &[String]) {
    let url = args.last().unwrap();
    if args.iter().any(|a| a == "csv=p=0") {
        match () {
            _ if url.contains("noprobe") => std::process::exit(1),
            _ if url.contains("10bit") => print!("h264,video,yuv420p10le,progressive\nmpegts\n"),
            _ if url.contains("hevc") => print!("hevc,video\naac,audio\nmpegts\n"),
            _ if url.contains("av1") => print!("av1,video\naac,audio\nmpegts\n"),
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
        eprintln!("[in#0 @ 0x0] Error opening input: Connection refused");
        eprintln!("Error opening input file {input}.");
        eprintln!("Error opening input files: Connection refused");
        std::process::exit(1)
    }
    if input.contains("badchain") {
        eprintln!("Input #0, mpegts, from '{input}':");
        eprintln!("Conversion failed!");
        std::process::exit(1)
    }
    if input.contains("slowstart") {
        hang()
    }
    // A GPU that cannot decode this upstream: a `-hwaccel` run dies, hangs, or runs on refusing it.
    if args.iter().any(|a| a == "-hwaccel") {
        if input.contains("gpurefuses") {
            let refusal = "[vist#0:0/h264 @ 0x0] [dec:h264_qsv @ 0x0] Error submitting packet to \
                           decoder: Function not implemented";
            flush_on_term(&dir, "seg-00000.m4s", refusal)
        }
        if input.contains("gpudies") {
            eprintln!(
                "[h264 @ 0x0] Failed setup for format vaapi: hwaccel initialisation returned error."
            );
            std::process::exit(1)
        }
        if input.contains("gpuhangs") {
            if input.contains("flush") {
                flush_on_term(&dir, "seg-00000.m4s", "ready")
            }
            hang()
        }
    }
    let ext = if pattern.ends_with(".ts") {
        "ts"
    } else {
        "m4s"
    };
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
    let init = args
        .iter()
        .position(|a| a == "-hls_fmp4_init_filename")
        .map(|i| args[i + 1].clone());
    if let Some(init) = &init {
        put(&dir.join(init), b"init");
    }
    // An MPEG-TS run slow to open: its first segment stays a `.tmp` for 6 s.
    if input.contains("slowts") {
        std::thread::sleep(Duration::from_secs(11));
        let open = dir.join(format!("seg-00000.{ext}.tmp"));
        std::fs::write(&open, "partial").unwrap();
        std::thread::sleep(Duration::from_secs(6));
        std::fs::remove_file(open).unwrap();
    }
    let mut m3u8 = format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:{first}\n"
    );
    if let Some(init) = &init {
        m3u8.push_str(&format!("#EXT-X-MAP:URI=\"{init}\"\n"));
    }
    for n in first.. {
        let written = n - first;
        if input.contains("stall") && written == 1 {
            if input.contains("flush") {
                flush_on_term(&dir, &format!("seg-{n:05}.{ext}"), "ready")
            }
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
    // The filter listing is a query: no mode may take its last argument for an output file.
    if args.last().is_some_and(|a| a == "-filters") {
        return;
    }
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
        "crop" => crop(&args),
        "item" => item(&args),
        #[cfg(unix)]
        "itemstop" => itemstop(&args),
        "nomedia" => nomedia(&args),
        "hwlate" => hwlate(&args),
        // Only the early run (`-to`) keeps talking, so only its natural end can stop it.
        "early" if args.iter().any(|a| a == "-to") => talk(),
        // 100 bytes at the output (last argument), or a partial file and exit 1.
        "pcm" => put(Path::new(args.last().unwrap()), [0u8; 100]),
        "failpcm" => {
            std::fs::write(args.last().unwrap(), b"partial").unwrap();
            std::process::exit(1)
        }
        m @ ("merge" | "failmerge" | "hangmerge") => merge(m, &args),
        m @ ("sprite" | "hangsprite" | "failsprite" | "flakysprite" | "passfailsprite"
        | "sparsesprite" | "probefailsprite" | "promoprobesprite" | "promopasssprite") => {
            sprite(m, &args)
        }
        // A sprite mode on a GPU that rejects the file: a hardware run is logged to
        // `<input>.hw` and fails, the others are the mode's.
        m @ ("hwsprite" | "hwfailsprite" | "hwsparsesprite") => {
            if let Some(i) = args.iter().position(|a| a == "-hwaccel") {
                let input = &args[args.iter().position(|a| a == "-i").unwrap() + 1];
                append_line(&format!("{input}.hw"), &args[i + 1]);
                std::process::exit(1)
            }
            sprite(&m[2..], &args)
        }
        m @ ("scan" | "failscan" | "hangscan" | "idlescan" | "fp" | "failfp" | "hangfp" | "sub"
        | "emptysub" | "failsub" | "hangsub" | "slowsub") => job(m, &args),
        "sync" => {
            if args.iter().any(|a| a == "pcm_s16le") {
                eprintln!("no audio");
                std::process::exit(1);
            }
            hang()
        }
        // The picture copy: a JPEG-looking file at the output path, nothing for `emptycover`.
        "cover" => put(Path::new(args.last().unwrap()), b"\xFF\xD8cover\xFF\xD9"),
        "emptycover" => put(Path::new(args.last().unwrap()), b""),
        "bigcover" => put(Path::new(args.last().unwrap()), vec![0u8; (16 << 20) + 1]),
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
