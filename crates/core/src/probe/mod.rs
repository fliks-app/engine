//! Media probing, ffprobe/mkvmerge/cropdetect output to `MediaInfo` (`docs/spec/probe.md`).
//! The tools run behind [`Tools`]: processes in the engine, recorded output in the tests.
pub mod args;
mod build;
pub mod crop;
mod ffjson;
pub(crate) mod js;
mod lang;
mod mkv;
pub mod moviehash;

pub use build::{apply_late_audio, build, first_frame_stream, measure_fps, parse_main};
pub use ffjson::Frame;
pub use moviehash::moviehash;

#[cfg(test)]
mod tests;

use crate::api::{CropInfo, MediaInfo};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;

/// A failed tool run.
#[derive(Debug, Default, Clone)]
pub struct ToolError {
    /// Spawn error, timeout or `Command failed: ...`.
    pub message: String,
    pub stderr: String,
    pub stdout: String,
    /// The binary does not exist.
    pub missing: bool,
}

/// Runs the tools on one file; `args` are the full argument lists of [`args`].
pub trait Tools: Sync {
    /// Whole stdout of ffprobe.
    fn ffprobe(
        &self,
        args: Vec<String>,
        timeout_ms: u64,
    ) -> impl Future<Output = Result<String, ToolError>> + Send;
    /// ffprobe line by line; the run ends as soon as `on_line` returns false.
    fn ffprobe_lines(
        &self,
        args: Vec<String>,
        timeout_ms: u64,
        on_line: &mut (dyn FnMut(&str) -> bool + Send),
    ) -> impl Future<Output = Result<(), ToolError>> + Send;
    /// Stdout of `mkvmerge -J`; an error keeps the stdout it printed.
    fn mkvmerge(&self, args: Vec<String>)
    -> impl Future<Output = Result<String, ToolError>> + Send;
    /// ffmpeg runs, up to the host's slot budget at a time; Ok is the stderr log.
    fn ffmpeg_samples(
        &self,
        args: Vec<Vec<String>>,
    ) -> impl Future<Output = Vec<Result<String, ToolError>>> + Send;
    fn file_size(&self) -> impl Future<Output = Option<u64>> + Send;
}

#[derive(Debug, Clone, Default)]
pub struct ProbeOpts {
    pub crop: bool,
    pub mkv_tags: bool,
    /// Hardware decode args for the crop samples (`args::crop_hw_decode`); `None` = software.
    pub crop_decode: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    /// False when ffprobe could not tell; `info` is then the error blob.
    pub ok: bool,
    pub info: MediaInfo,
    pub error: Option<String>,
    /// Decoder of the crop samples: the `-hwaccel` name, or `sw`. `None` when no crop ran.
    pub crop_decode: Option<String>,
}

fn failed(message: String) -> Outcome {
    Outcome {
        ok: false,
        info: build::failure(&message),
        error: Some(message),
        crop_decode: None,
    }
}

/// First frame of an `-of json` frame dump read so far: the first complete element of `frames`.
pub fn first_frame_of(json: &str) -> Option<Frame> {
    let key = json.find("\"frames\"")?;
    let start = key + 1 + json[key + 1..].find('{')?;
    let (mut depth, mut quoted, mut skip) = (0, false, false);
    for (i, c) in json[start..].char_indices() {
        if skip {
            skip = false;
        } else if quoted {
            match c {
                '\\' => skip = true,
                '"' => quoted = false,
                _ => {}
            }
        } else {
            match c {
                '"' => quoted = true,
                '{' | '[' => depth += 1,
                '}' | ']' => {
                    depth -= 1;
                    if depth == 0 {
                        return serde_json::from_str(&json[start..start + i + 1]).ok();
                    }
                }
                _ => {}
            }
        }
    }
    None
}

#[derive(Default)]
struct FirstFrame {
    buf: String,
    frame: Option<Frame>,
}

impl FirstFrame {
    /// False once the first frame is complete.
    fn push(&mut self, line: &str) -> bool {
        if line.is_empty() {
            return true;
        }
        self.buf.push_str(line);
        self.buf.push('\n');
        self.frame = first_frame_of(&self.buf);
        self.frame.is_none()
    }
}

struct FirstPackets {
    wanted: BTreeSet<u32>,
    firsts: BTreeMap<u32, f64>,
    origin: Option<f64>,
}

impl FirstPackets {
    /// False once every wanted stream showed, or the read is `LATE_AUDIO_SEARCH_SECONDS` in.
    fn push(&mut self, line: &str) -> bool {
        if line.is_empty() {
            return true;
        }
        let mut cols = line.split(',');
        let (index, time) = (cols.next().unwrap_or(""), cols.next());
        let Some(pts) = time.and_then(js::parse_float) else {
            return true;
        };
        let origin = *self.origin.get_or_insert(pts);
        if let Some(stream) = js::number(index).map(|n| n as u32)
            && self.wanted.contains(&stream)
        {
            self.firsts.entry(stream).or_insert(pts);
        }
        self.firsts.len() < self.wanted.len() && pts - origin <= build::LATE_AUDIO_SEARCH_SECONDS
    }
}

fn message(e: &ToolError) -> String {
    let stderr = e.stderr.trim();
    if stderr.is_empty() {
        e.message.clone()
    } else {
        stderr.to_string()
    }
}

/// `detectMediaFileInfo` (+ `detectCrop`): never throws; a tool that cannot tell is `ok: false`.
pub async fn probe<T: Tools>(t: &T, path: &str, opts: &ProbeOpts) -> Outcome {
    let stdout = match t.ffprobe(args::main(path), args::MAIN_TIMEOUT_MS).await {
        Ok(s) => s,
        Err(e) => return failed(message(&e)),
    };
    let main = match parse_main(&stdout) {
        Ok(m) => m,
        Err(e) => return failed(e),
    };
    let mut x = build::Extras::default();
    if let Some((stream, from)) = build::fps_plan(&main)
        && let Ok(out) = t
            .ffprobe(args::fps(path, stream, from), args::FPS_TIMEOUT_MS)
            .await
    {
        x.measured_fps = measure_fps(&out);
    }
    if let Some(stream) = first_frame_stream(&main) {
        let mut ff = FirstFrame::default();
        let _ = t
            .ffprobe_lines(
                args::first_frame(path, stream),
                args::FIRST_FRAME_TIMEOUT_MS,
                &mut |l| ff.push(l),
            )
            .await;
        x.first_frame = ff.frame;
    }
    let wanted: BTreeSet<u32> = build::audio_indices(&main).into_iter().collect();
    if !wanted.is_empty() {
        let mut fp = FirstPackets {
            wanted,
            firsts: BTreeMap::new(),
            origin: None,
        };
        let _ = t
            .ffprobe_lines(
                args::audio_packets(path),
                args::AUDIO_PACKETS_TIMEOUT_MS,
                &mut |l| fp.push(l),
            )
            .await;
        x.audio_firsts = fp.firsts;
    }

    let built = build::build(&main, &x);
    let mut info = built.info;
    if let Some(late) = built.late_audio
        && let Some(size) = t.file_size().await
        && let Ok(out) = t
            .ffprobe(
                args::late_audio(path, late.reach_seconds, size),
                args::LATE_AUDIO_TIMEOUT_MS,
            )
            .await
    {
        apply_late_audio(&mut info, &out);
    }
    if opts.mkv_tags && mkv::is_matroska(main.format.format_name.as_deref()) {
        match t.mkvmerge(args::mkvmerge(path)).await {
            Ok(out) => {
                mkv::apply(&mut info, &out);
            }
            // mkvmerge exits 1 on warnings but still prints the full JSON.
            Err(e) if !e.missing && !e.stdout.is_empty() => {
                mkv::apply(&mut info, &e.stdout);
            }
            Err(_) => {}
        }
    }
    let mut crop_decode = None;
    if opts.crop
        && let Some(v) = info.video.first()
    {
        let (duration, w, h) = (info.duration_seconds, v.width, v.height);
        let (crop, decode) =
            detect_crop(t, path, duration, w, h, opts.crop_decode.as_deref()).await;
        crop_decode = Some(decode);
        if let (Some(crop), Some(v)) = (crop, info.video.first_mut()) {
            v.crop = Some(crop);
        }
    }
    Outcome {
        ok: true,
        error: None,
        info,
        crop_decode,
    }
}

/// `detectCrop`: samples spread over the file, loosest box wins. Whether the GPU decodes a file
/// depends on its codec, so the first sample decides: on failure every sample runs in software
/// (same nv12 pixels). Also returns the decoder used.
async fn detect_crop<T: Tools>(
    t: &T,
    path: &str,
    duration: Option<f64>,
    width: Option<u32>,
    height: Option<u32>,
    hw: Option<&[String]>,
) -> (Option<CropInfo>, String) {
    let ss = crop::timestamps(duration);
    let run = |hw: Option<&[String]>, ss: &[u32]| {
        t.ffmpeg_samples(ss.iter().map(|s| args::crop_sample(path, *s, hw)).collect())
    };
    let mut hw = hw;
    let mut first = run(hw, &ss[..1]).await.pop();
    if first.as_ref().is_none_or(Result::is_err) {
        // A refused first sample is retried once, in software.
        hw = None;
        first = run(None, &ss[..1]).await.pop();
    }
    let rest = run(hw, &ss[1..]).await;
    let samples: Vec<_> = first
        .into_iter()
        .chain(rest)
        .map(|r| r.ok().and_then(|log| crop::parse_sample(&log)))
        .collect();
    let name = hw.map_or("sw", |h| h.get(1).map_or("sw", String::as_str));
    (crop::aggregate(&samples, width, height), name.into())
}
