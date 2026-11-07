//! The `Optimize` job: one library file rewritten into `staged` (metadata edit, remux or
//! re-encode), checked against the source, hashed. Spec: `docs/spec/jobs.md` section 11.
use super::scan::{ffprobe, lines, scan_file};
use super::{Cmd, Ctx, free_bytes, tail};
use crate::probe::run::ProcessTools;
use crate::proc::{Ffmpeg, Pipes};
use crate::safe_write;
use crate::slots::{CPU_DEVICE, Lane};
use fliks_engine_core::api::{
    HwAccel, JobErrorClass as Class, JobResult, MEDIA_INFO_SCHEMA_VERSION, MediaInfo,
    OptimizeContainer, OptimizeLevel, OptimizeMeta, OptimizePayload, OptimizePlan, OptimizeQuality,
    OptimizeTooSlow,
};
use fliks_engine_core::args::{self, FileFormat};
use fliks_engine_core::jobs::merge::progress_seconds;
use fliks_engine_core::jobs::optimize::{self as opt, Facts, OutputCheck, PacketStats};
use fliks_engine_core::probe::{self, ProbeOpts};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tokio::io::{AsyncBufReadExt, BufReader};

pub fn validate(p: &OptimizePayload) -> Result<(), String> {
    opt::main_video(&p.info)?;
    let plan = &p.plan;
    match plan.level {
        OptimizeLevel::Metadata => {
            if !opt::is_matroska(&p.info) {
                return Err("the metadata level edits Matroska files only".into());
            }
            let rewrites = plan
                .streams
                .iter()
                .any(|s| !s.keep || s.encode.is_some() || s.keep_original);
            if rewrites || !plan.extra_subtitles.is_empty() || plan.crop.is_some() {
                return Err("the metadata level cannot remove, convert or add tracks".into());
            }
        }
        OptimizeLevel::Reencode => {
            if !matches!(plan.codec.as_deref(), Some("hevc" | "av1")) {
                return Err("a reencode needs codec hevc or av1".into());
            }
        }
        OptimizeLevel::Remux => {}
    }
    if let Some(x) = plan
        .extra_subtitles
        .iter()
        .find(|x| !Path::new(&x.path).is_absolute())
    {
        return Err(format!("extra subtitle path is not absolute: {}", x.path));
    }
    Ok(())
}

/// Class of a failed file operation.
pub(super) fn io_class(e: &std::io::Error) -> Class {
    #[cfg(unix)]
    match e.raw_os_error() {
        Some(libc::EROFS) => return Class::Access,
        Some(libc::EIO) => return Class::Io,
        Some(libc::ENOSPC) => return Class::Nospace,
        _ => {}
    }
    match e.kind() {
        ErrorKind::StorageFull => Class::Nospace,
        ErrorKind::NotFound | ErrorKind::PermissionDenied => Class::Access,
        ErrorKind::AlreadyExists => Class::Engine,
        _ => Class::Io,
    }
}

pub(super) fn io_err(what: &Path, e: std::io::Error) -> String {
    io_class(&e).message(format!("{}: {e}", what.display()))
}

/// `Err` text of a run that did not end well: a cancel stays itself, the rest is an engine fault.
fn pipeline_err(e: String) -> String {
    if e == super::CANCELLED || e == "promoted" || is_busy(&e) {
        e
    } else {
        Class::Engine.message(e)
    }
}

/// Whether a failed hardware attempt is worth redoing in software.
fn software_may_help(e: &str) -> bool {
    !is_busy(e) && matches!(Class::from_message(e).0, Class::Engine | Class::Verify)
}

/// The error of the output's decode run: a real ffmpeg exit is `verify`, any other end
/// (a playback yield, a cancel, the watchdog) passes through.
fn decode_verdict(e: String) -> String {
    if e.contains("ffmpeg exited with") {
        Class::Verify.message(format!("the output does not decode: {e}"))
    } else {
        e
    }
}

/// A run stopped for playback: the job goes back to the queue, no failure.
fn is_busy(e: &str) -> bool {
    Class::from_message(e).0 == Class::Busy && e.starts_with("busy: ")
}

pub async fn run(ctx: &Ctx, p: &OptimizePayload) -> Result<JobResult, String> {
    let remote = p.input_url.is_some();
    let src = match &p.input_url {
        Some(url) => PathBuf::from(url),
        None => PathBuf::from(ctx.media_path()),
    };
    let staged = match (&p.staged_path, remote) {
        (Some(s), _) => PathBuf::from(ctx.rt.map_media_path(s)),
        (None, true) => ctx
            .rt
            .cache
            .optimize_dir()
            .join(format!(".fliks-opt-{}.tmp", ctx.job.id)),
        (None, false) => src
            .parent()
            .unwrap_or(Path::new("."))
            .join(format!(".fliks-opt-{}-1.tmp", ctx.job.id)),
    };
    if !staged.is_absolute() || (!remote && !src.is_absolute()) {
        return Err(Class::Access.message("paths must be absolute on this host"));
    }
    if remote && p.plan.level == OptimizeLevel::Metadata {
        return Err(Class::Access.message("the metadata level edits a local file"));
    }
    // A remote source has no size and time to hold still: the host's lease fences it instead.
    let before = if remote {
        (0, None)
    } else {
        fingerprint(&src).map_err(|e| io_err(&src, e))?
    };
    let mut w = Work {
        ctx,
        p,
        src: &src,
        staged: &staged,
        src_packets: None,
        created: false,
        remote,
        home: ctx.lane(),
    };
    let out = w.start(before.0).await;
    let out = match out {
        Ok(m) if !remote && fingerprint(&src).ok() != Some(before) => {
            drop(m);
            Err(Class::Stale.message("the source changed during the job"))
        }
        other => other,
    };
    // Only a file this job created is ever removed: a taken name belongs to someone else.
    if out.is_err() && w.created {
        w.cleanup().await;
    }
    out.map(|optimize| JobResult::Optimize {
        optimize: Box::new(optimize),
    })
}

/// Size and modification time: what must not move under a running job.
fn fingerprint(p: &Path) -> std::io::Result<(u64, Option<SystemTime>)> {
    let m = std::fs::metadata(p)?;
    // Reading one byte proves the file is readable, not only listed.
    std::fs::File::open(p)?;
    Ok((m.len(), m.modified().ok()))
}

struct Work<'a> {
    ctx: &'a Ctx,
    p: &'a OptimizePayload,
    src: &'a Path,
    staged: &'a Path,
    src_packets: Option<PacketStats>,
    created: bool,
    /// `src` holds the `inputUrl`, read over HTTP.
    remote: bool,
    /// The lane the job was leased on.
    home: Option<Lane>,
}

/// What the output turned out to be, for the result.
struct Produced {
    level: OptimizeLevel,
    copy_reason: Option<&'static str>,
    cpu_fallback: bool,
}

impl Work<'_> {
    fn plan(&self) -> &OptimizePlan {
        &self.p.plan
    }

    fn info(&self) -> &MediaInfo {
        &self.p.info
    }

    fn duration(&self) -> f64 {
        self.info()
            .duration_seconds
            .filter(|d| *d > 0.0)
            .unwrap_or(0.0)
    }

    /// The HTTP input options on every input of `argv` when the source is remote.
    fn net(&self, argv: Vec<String>) -> Vec<String> {
        match &self.p.input_url {
            Some(url) => opt::http_input(argv, url, self.p.input_token.as_deref()),
            None => argv,
        }
    }

    /// A failed read of a remote source is the network's: its text says so, or the read itself broke.
    fn source_error(&self, e: String) -> String {
        let (class, text) = Class::from_message(&e);
        match class {
            Class::Engine | Class::Io
                if self.remote && (class == Class::Io || opt::is_network_error(text)) =>
            {
                Class::Network.message(text)
            }
            _ => e,
        }
    }

    /// `-threads` of a run: a software optimization's weight in the cores, else the playback split.
    fn threads(&self) -> u32 {
        let slots = &self.ctx.rt.slots;
        match self.ctx.lane() {
            Some(l) if !l.gpu => slots.optimize_threads(),
            _ => slots.encode_threads(),
        }
    }

    /// A job leased on the GPU lane runs its software encodes on the CPU lane, weighed and
    /// paused like any CPU run; its hardware encodes go back to its own lane.
    fn enter_lane(&self, hardware: bool) {
        let Some(home) = self.home.as_ref().filter(|h| h.gpu) else {
            return;
        };
        self.ctx.set_lane(if hardware {
            home.clone()
        } else {
            Lane {
                device: CPU_DEVICE.into(),
                gpu: false,
            }
        });
    }

    fn staged_text(&self) -> String {
        self.staged.to_string_lossy().into_owned()
    }

    async fn cleanup(&self) {
        let _ = tokio::fs::remove_file(self.staged).await;
        for k in 0..opt::SAMPLE_COUNT {
            for kind in ["s", "e"] {
                let _ = tokio::fs::remove_file(self.sample(k, kind)).await;
            }
        }
    }

    /// Samples live in the engine cache, never beside the source.
    fn sample(&self, k: usize, kind: &str) -> PathBuf {
        let id = &self.ctx.job.id;
        let dir = self.ctx.rt.cache.optimize_dir();
        dir.join(format!(".fliks-opt-{id}.sample{k}{kind}.mp4"))
    }

    async fn start(&mut self, src_len: u64) -> Result<OptimizeMeta, String> {
        let dir = self.staged.parent().ok_or("staged has no directory")?;
        // Only our own cache dir is created; a missing library folder fails `create_staged`.
        if self.remote {
            std::fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
        }
        self.ctx
            .check_disk_existing(dir)
            .await
            .map_err(|e| Class::Nospace.message(e))?;
        let metadata = self.plan().level == OptimizeLevel::Metadata;
        // The metadata level copies the source first: checked only if the reflink fails.
        if !metadata
            && let Some(free) = free_bytes(dir)
            && free < src_len
        {
            return Err(Class::Nospace.message(format!(
                "{free} bytes free beside the staged file, the source is {src_len}"
            )));
        }
        let (src, staged) = (self.src.to_owned(), self.staged.to_owned());
        let (copy, remote) = (metadata, self.remote);
        let dir = dir.to_owned();
        tokio::task::spawn_blocking(move || {
            if remote {
                std::fs::File::options()
                    .write(true)
                    .create_new(true)
                    .open(&staged)?;
                return Ok(());
            }
            let mut f = safe_write::create_staged(&src, &staged)?;
            if copy {
                safe_write::copy_file(&src, &mut f, || match free_bytes(&dir) {
                    Some(free) if free < src_len => Err(std::io::Error::new(
                        ErrorKind::StorageFull,
                        format!("{free} bytes free, the source is {src_len}"),
                    )),
                    _ => Ok(()),
                })?;
                f.sync_all()?;
            }
            Ok::<_, std::io::Error>(())
        })
        .await
        .map_err(|e| Class::Engine.message(e))?
        .map_err(|e| io_err(self.staged, e))?;
        self.created = true;
        let made = match self.plan().level {
            OptimizeLevel::Metadata => self.metadata().await?,
            OptimizeLevel::Remux => self.copy(None).await?,
            OptimizeLevel::Reencode => self.reencode().await?,
        };
        self.ctx.progress(95.0);
        self.finish(made).await
    }

    async fn metadata(&mut self) -> Result<Produced, String> {
        let argv = opt::mkvpropedit_args(self.info(), self.plan(), &self.staged_text());
        self.mkvpropedit(argv).await?;
        Ok(Produced {
            level: OptimizeLevel::Metadata,
            copy_reason: None,
            cpu_fallback: false,
        })
    }

    /// Statistics tags of a Matroska output: the muxer's (or the source's) are wrong after a rewrite.
    async fn statistics_tags(&self) -> Result<(), String> {
        if opt::container_of(self.info(), self.plan()) != OptimizeContainer::Mkv {
            return Ok(());
        }
        self.mkvpropedit(opt::statistics_tags_args(&self.staged_text()))
            .await
    }

    async fn mkvpropedit(&self, argv: Vec<String>) -> Result<(), String> {
        let cmd = Cmd::new("mkvpropedit", &mkvpropedit(), &argv, Pipes::default());
        self.ctx
            .pipeline(self.ctx.run_timeout_ms(), vec![cmd], |p| async move {
                let p = &p[0];
                match p.wait().await.code {
                    Some(0 | 1) => Ok(()),
                    code => Err(Class::Engine
                        .message(format!("mkvpropedit exited with {code:?}: {}", tail(p, 3)))),
                }
            })
            .await
            .map_err(pipeline_err)?
    }

    /// The remux: video copied, the rest by the plan.
    async fn copy(&mut self, reason: Option<&'static str>) -> Result<Produced, String> {
        let argv = self.argv(true, "none", self.staged_text())?;
        let total = self.duration();
        self.run(&argv, total, (5.0, 85.0))
            .await
            .and_then(|r| self.damage(&r))?;
        self.statistics_tags().await?;
        self.check(false, false).await?;
        Ok(Produced {
            level: OptimizeLevel::Remux,
            copy_reason: reason,
            cpu_fallback: false,
        })
    }

    fn argv(&self, copy_video: bool, hw: &str, out: String) -> Result<Vec<String>, String> {
        let rt = &self.ctx.rt;
        let caps = rt.caps().unwrap_or_default();
        let dev = rt.device(&caps);
        let file = opt::build_file_args(
            self.info(),
            self.plan(),
            &opt::Output {
                input_path: &self.src.to_string_lossy(),
                path: &out,
                codec: self.plan().codec.as_deref().unwrap_or("hevc"),
                hw,
                threads: self.threads(),
                copy_video,
            },
        )
        .map_err(|e| Class::Engine.message(e))?;
        args::file(&file, &caps, &dev).map_err(|e| Class::Engine.message(e))
    }

    /// A source that showed decoding errors is refused, the output discarded.
    fn damage(&self, r: &Ran) -> Result<(), String> {
        if r.errors == 0 {
            return Ok(());
        }
        Err(opt::decode_failure(r.errors, self.duration(), &r.excerpt))
    }

    async fn reencode(&mut self) -> Result<Produced, String> {
        let rt = &self.ctx.rt;
        let caps = rt.caps().unwrap_or_default();
        let hw = hw_name(caps.hw_accel);
        // A job leased on the CPU device never takes the GPU, whatever the plan says.
        let on_cpu = self.home.as_ref().is_some_and(|l| !l.gpu);
        let mut hardware = !self.plan().software && hw != "none" && !on_cpu;
        let v = opt::main_video(self.info())?;
        if let Some(r) = opt::copy_video_reason(v, self.plan(), hardware) {
            return self.copy(Some(r)).await;
        }
        let max_hours = self
            .plan()
            .max_software_hours
            .unwrap_or(opt::DEFAULT_MAX_SOFTWARE_HOURS);
        let mut cpu_fallback = false;
        let sampled = loop {
            self.enter_lane(hardware);
            match self.samples(if hardware { hw } else { "none" }).await {
                Err(e) if hardware && self.falls_back(&e) => {
                    (hardware, cpu_fallback) = (false, true);
                }
                other => break other?,
            }
        };
        match sampled {
            Sampled::Copy(r) => return self.copy(Some(r)).await,
            Sampled::Hours(h) if !hardware && h > max_hours => {
                // The CPU lane never takes the GPU: the video is copied instead.
                if hw == "none" || on_cpu || self.plan().too_slow == OptimizeTooSlow::Skip {
                    return self.copy(Some("too_slow")).await;
                }
                hardware = true;
            }
            Sampled::Hours(_) | Sampled::None => {}
        }
        let total = self.duration();
        let stats = loop {
            self.enter_lane(hardware);
            let argv = match self.argv(
                false,
                if hardware { hw } else { "none" },
                self.staged_text(),
            ) {
                Ok(a) => a,
                Err(_) if hardware => {
                    (hardware, cpu_fallback) = (false, true);
                    continue;
                }
                Err(e) => return Err(e),
            };
            let run = self.run(&argv, total, (5.0, 85.0)).await;
            let verdict = match run {
                Ok(r) if r.errors > 0 => Err(self.damage(&r).unwrap_err()),
                Ok(_) => match self.statistics_tags().await {
                    Ok(()) => self.check(true, hardware).await.map(Some),
                    Err(e) => return Err(e),
                },
                Err(e) => Err(e),
            };
            match verdict {
                Ok(f) => break f,
                Err(e) if hardware && self.falls_back(&e) => {
                    (hardware, cpu_fallback) = (false, true);
                }
                Err(e) => return Err(e),
            }
        };
        // The produced video track must still save what the samples promised.
        if let (Some(f), Some(min)) = (stats, self.plan().min_savings_pct)
            && let Some(src) = self.src_packets
            && opt::savings_pct(src.bytes, f.bytes) < min
        {
            return self.copy(Some("low_savings")).await;
        }
        Ok(Produced {
            level: OptimizeLevel::Reencode,
            copy_reason: None,
            cpu_fallback,
        })
    }

    /// A failed hardware attempt is redone in software only when the encoder or the output is at
    /// fault: not a cancel, a playback yield, the network, the disk or a damaged source.
    fn falls_back(&self, e: &str) -> bool {
        !self.ctx.cancelled() && software_may_help(e)
    }

    /// Encodes the sample windows and the same windows copied: savings and software speed.
    async fn samples(&mut self, hw: &str) -> Result<Sampled, String> {
        let starts = opt::sample_starts(self.duration());
        if starts.is_empty() {
            return Ok(Sampled::None);
        }
        let dir = self.ctx.rt.cache.optimize_dir();
        std::fs::create_dir_all(&dir).map_err(|e| io_err(&dir, e))?;
        let (mut copied, mut encoded, mut wall) = (0.0, 0.0, 0.0);
        let input = self.src.to_string_lossy().into_owned();
        let step = 5.0 / starts.len() as f64;
        for (k, start) in starts.iter().enumerate() {
            for (copy, kind) in [(true, "s"), (false, "e")] {
                let out = self.sample(k, kind);
                let mut file = opt::build_file_args(
                    self.info(),
                    self.plan(),
                    &opt::Output {
                        input_path: &input,
                        path: &out.to_string_lossy(),
                        codec: self.plan().codec.as_deref().unwrap_or("hevc"),
                        hw: if copy { "none" } else { hw },
                        threads: self.threads(),
                        copy_video: copy,
                    },
                )
                .map_err(|e| Class::Engine.message(e))?;
                // Video alone in an MP4: the sizes compare the video tracks.
                file.format = FileFormat::Mp4;
                if let Some(a) = file.archive.as_mut() {
                    (a.audio, a.subtitles, a.extra_subtitles, a.hvc1_tag) =
                        (vec![], vec![], vec![], false);
                }
                let rt = &self.ctx.rt;
                let caps = rt.caps().unwrap_or_default();
                let argv = args::file(&file, &caps, &rt.device(&caps))
                    .map_err(|e| Class::Engine.message(e))?;
                let argv = opt::windowed(argv, &input, *start, opt::SAMPLE_SECONDS);
                let r = self
                    .run(
                        &argv,
                        opt::SAMPLE_SECONDS,
                        (k as f64 * step, (k + 1) as f64 * step),
                    )
                    .await?;
                if !copy {
                    wall += self.ctx.last_active_ms() as f64 / 1000.0;
                }
                let len = tokio::fs::metadata(&out)
                    .await
                    .map_err(|e| io_err(&out, e))?
                    .len() as f64;
                let _ = tokio::fs::remove_file(&out).await;
                if copy {
                    copied += len;
                } else {
                    encoded += len;
                    if r.errors > 0 && hw != "none" {
                        // A hardware decode fault is not the source's: the full run decides.
                        continue;
                    }
                }
            }
        }
        if let Some(min) = self.plan().min_savings_pct
            && opt::savings_pct(copied, encoded) < min
        {
            return Ok(Sampled::Copy("low_savings"));
        }
        let sampled = opt::SAMPLE_SECONDS * starts.len() as f64;
        Ok(Sampled::Hours(opt::estimated_hours(
            wall,
            sampled,
            self.duration(),
        )))
    }

    /// One ffmpeg run with its progress mapped onto `pct`; its decoding errors and stderr tail.
    async fn run(&self, argv: &[String], total: f64, pct: (f64, f64)) -> Result<Ran, String> {
        let argv = self.net(argv.to_vec());
        run_ffmpeg(self.ctx, &opt::runnable(argv), total, pct)
            .await
            .map_err(|e| self.source_error(e))
    }

    /// Verifies the staged output; the video packet stats of the output.
    async fn check(&mut self, reencoded: bool, hardware: bool) -> Result<PacketStats, String> {
        let ctx = self.ctx;
        let path = self.staged_text();
        let _ = safe_write::sync_file(self.staged);
        let probed = probe_file(ctx, &path).await?;
        let out_packets = packets(ctx, opt::packet_args(&path)).await?;
        if self.src_packets.is_none() {
            let argv = self.net(opt::packet_args(&self.src.to_string_lossy()));
            self.src_packets = Some(packets(ctx, argv).await.map_err(|e| self.source_error(e))?);
        }
        let gap = if reencoded {
            keyframe_gap(ctx, &probed, &path).await?
        } else {
            None
        };
        let out_facts = Facts {
            packets: Some(out_packets),
            max_keyframe_gap: gap,
        };
        let bad = opt::verify_output(&OutputCheck {
            src: self.info(),
            out: &probed,
            plan: self.plan(),
            src_facts: Facts {
                packets: self.src_packets,
                max_keyframe_gap: None,
            },
            out_facts,
            reencoded,
        });
        if !bad.is_empty() {
            return Err(Class::Verify.message(bad.join("; ")));
        }
        let r = run_ffmpeg(
            ctx,
            &opt::decode_args(&path, true),
            self.duration(),
            (85.0, 95.0),
        )
        .await;
        if let Err(e) = r {
            // Only a real ffmpeg exit is a verdict; a busy, cancel or watchdog end is not.
            return Err(decode_verdict(e));
        }
        let faithful = self.plan().quality == Some(OptimizeQuality::Faithful);
        if reencoded && (hardware || faithful) && self.plan().crop.is_none() {
            self.ssim(&probed, &path).await?;
        }
        Ok(out_packets)
    }

    /// Mean SSIM of three windows against the source: green or black frames score far below.
    async fn ssim(&self, out: &MediaInfo, path: &str) -> Result<(), String> {
        let v = opt::main_video(out)?;
        let (Some(w), Some(h)) = (v.width, v.height) else {
            return Ok(());
        };
        let input = self.src.to_string_lossy().into_owned();
        for start in opt::sample_starts(self.duration()) {
            let argv = self.net(opt::ssim_args(&input, path, start, 5.0, w, h));
            let r = run_ffmpeg(self.ctx, &argv, 5.0, (95.0, 95.0))
                .await
                .map_err(|e| self.source_error(e))?;
            match opt::parse_ssim(&r.lines) {
                Some(s) if s >= opt::MIN_SSIM => {}
                other => {
                    return Err(Class::Verify.message(format!(
                        "SSIM {other:?} at {start:.0}s, at least {} expected",
                        opt::MIN_SSIM
                    )));
                }
            }
        }
        Ok(())
    }

    async fn finish(&self, made: Produced) -> Result<OptimizeMeta, String> {
        let _ = safe_write::sync_file(self.staged);
        let path = self.staged.to_owned();
        let ctx = self.ctx.clone();
        let sha =
            tokio::task::spawn_blocking(move || safe_write::sha256_file(&path, || ctx.pulse()))
                .await
                .map_err(|e| Class::Engine.message(e))?
                .map_err(|e| io_err(self.staged, e))?;
        let out_size = tokio::fs::metadata(self.staged)
            .await
            .map_err(|e| io_err(self.staged, e))?
            .len();
        let out_info = probe_file(self.ctx, &self.staged_text()).await?;
        Ok(OptimizeMeta {
            staged_path: self.staged_text(),
            out_size: out_size as f64,
            out_sha256: sha,
            out_info,
            level: made.level,
            copy_reason: made.copy_reason.map(String::from),
            cpu_fallback: made.cpu_fallback,
        })
    }
}

enum Sampled {
    /// The video is copied, for this reason.
    Copy(&'static str),
    /// Hours a software encode of the file would take.
    Hours(f64),
    None,
}

fn mkvpropedit() -> String {
    crate::hw::tool_path("MKVPROPEDIT_PATH", "mkvpropedit")
}

fn hw_name(h: HwAccel) -> &'static str {
    match h {
        HwAccel::Vaapi => "vaapi",
        HwAccel::Nvenc => "nvenc",
        HwAccel::Qsv => "qsv",
        HwAccel::Amf => "amf",
        HwAccel::Videotoolbox => "videotoolbox",
        HwAccel::None => "none",
    }
}

/// What an ffmpeg run left behind.
pub(super) struct Ran {
    pub errors: u32,
    pub excerpt: String,
    pub lines: Vec<String>,
}

/// One ffmpeg run on a background slot, `-progress` lines on stdout as job progress between the
/// two percentages. A non-zero exit fails with the class its stderr shows.
pub(super) async fn run_ffmpeg(
    ctx: &Ctx,
    argv: &[String],
    total: f64,
    (from, to): (f64, f64),
) -> Result<Ran, String> {
    let cmd = Cmd::new("ffmpeg", &ctx.rt.ffmpeg, argv, Pipes { stdout: true });
    let c = ctx.clone();
    ctx.pipeline(ctx.run_timeout_ms(), vec![cmd], |p| async move {
        let p = &p[0];
        let (_, exit) = tokio::join!(report(&c, p, total, from, to), p.wait());
        let lines = p.stderr_tail();
        match exit.code {
            Some(0) => Ok(Ran {
                errors: p.decode_errors(),
                excerpt: lines
                    .iter()
                    .filter(|l| opt::is_decode_error(l))
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" | "),
                lines,
            }),
            code => Err(opt::classify_stderr(&lines)
                .message(format!("ffmpeg exited with {code:?}: {}", tail(p, 2)))),
        }
    })
    .await
    .map_err(pipeline_err)?
}

async fn report(ctx: &Ctx, p: &Ffmpeg, total: f64, from: f64, to: f64) {
    let Some(out) = p.take_stdout() else {
        return;
    };
    let mut lines = BufReader::new(out).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        ctx.pulse();
        if let Some(s) = progress_seconds(&line)
            && total > 0.0
        {
            ctx.progress((from + (to - from) * (s / total).clamp(0.0, 1.0)).min(99.0));
        }
    }
}

pub(super) async fn probe_file(ctx: &Ctx, path: &str) -> Result<MediaInfo, String> {
    let tools = ProcessTools::new(path, ctx.rt.slots.clone());
    let opts = ProbeOpts {
        crop: false,
        mkv_tags: true,
        crop_decode: None,
    };
    let out = probe::probe(&tools, path, &opts).await;
    if !out.ok {
        let why = out.error.unwrap_or_default();
        return Err(opt::classify_stderr(std::slice::from_ref(&why)).message(why));
    }
    let mut info = out.info;
    info.schema_version = Some(MEDIA_INFO_SCHEMA_VERSION);
    Ok(info)
}

/// Video packets of the first video stream of `path`.
async fn packets(ctx: &Ctx, argv: Vec<String>) -> Result<PacketStats, String> {
    let cmd = ffprobe(ctx, &argv);
    let c = ctx.clone();
    ctx.pipeline(ctx.run_timeout_ms(), vec![cmd], |p| async move {
        let mut s = PacketStats::default();
        lines(&c, &p[0], |l| {
            s.add_line(l);
            true
        })
        .await
        .map(|_| s)
    })
    .await
    .map_err(pipeline_err)?
    .map_err(|e| opt::classify_stderr(std::slice::from_ref(&e)).message(e))
}

/// Longest stretch between two keyframes of the output.
async fn keyframe_gap(ctx: &Ctx, info: &MediaInfo, path: &str) -> Result<Option<f64>, String> {
    let scan = scan_file(ctx, info, path).await.map_err(pipeline_err)?;
    let mut at: Vec<f64> = scan.keyframes.iter().map(|k| k.pts).collect();
    at.push(scan.end);
    Ok(at.windows(2).map(|w| w[1] - w[0]).reduce(f64::max))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_engine_or_verify_failure_falls_back_to_software() {
        assert!(software_may_help(
            &Class::Engine.message("ffmpeg exited with Some(1)")
        ));
        assert!(software_may_help(
            &Class::Verify.message("duration differs")
        ));
        for c in [Class::Network, Class::Nospace, Class::Access, Class::Busy] {
            assert!(!software_may_help(&c.message("x")), "{c:?}");
        }
        assert!(!software_may_help(
            &Class::Busy.message("stopped for playback")
        ));
    }

    #[test]
    fn a_decode_run_cut_short_is_not_a_verify_failure() {
        let exit = Class::Engine.message("ffmpeg exited with Some(1): bad");
        assert!(decode_verdict(exit).starts_with("verify: the output does not decode"));
        let busy = Class::Busy.message("stopped for playback, the job starts over");
        assert_eq!(decode_verdict(busy.clone()), busy);
        assert_eq!(
            decode_verdict(super::super::CANCELLED.into()),
            super::super::CANCELLED
        );
        let stalled = Class::Engine.message("ffmpeg stalled");
        assert_eq!(decode_verdict(stalled.clone()), stalled);
    }
}
