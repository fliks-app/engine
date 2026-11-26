//! Seek-bar sprite: layout, frame extractor choice and argv, tiling, missing-frame backfill and the
//! JPEG header read for `thumbHeight`. Spec: `docs/spec/jobs.md` section 5.
use crate::api::{CropInfo, HostCaps, HwAccel, MediaInfo, Platform, SpriteMeta, VideoStreamInfo};
use crate::hw::decoders::source_decoder;
use crate::jsnum::{js_string, round};
use crate::mp4::{input_seek_seconds, source_timeline};
use crate::strs;

pub const COLUMNS: u32 = 10;
pub const THUMB_WIDTH: u32 = 240;
/// Unpaused wall time of one frame or tile run. [ts:modules/streaming/thumbnail.service.ts:682,744]
pub const RUN_TIMEOUT_MS: u32 = 30_000;
/// Frame runs that may fail in a row from the start: past it nothing decodes this file, and the
/// job stops instead of trying every frame.
pub const GIVE_UP_AFTER: u32 = 16;
pub const FRAME_PATTERN: &str = "frame-%04d.jpg";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub interval: u32,
    pub count: u32,
    pub rows: u32,
}

/// [ts:modules/streaming/thumbnail.service.ts:285-287,780-785]
pub fn layout(duration: f64) -> Layout {
    let interval = if duration < 300.0 {
        2
    } else if duration <= 3600.0 {
        5
    } else if duration <= 7200.0 {
        15
    } else {
        30
    };
    let count = (duration / f64::from(interval)).ceil() as u32;
    Layout {
        interval,
        count,
        rows: count.div_ceil(COLUMNS),
    }
}

/// Input seek of tile 0: tile times count from the first frame, `-ss` from the container start.
pub fn seek_offset(info: &MediaInfo) -> f64 {
    input_seek_seconds(0.0, &source_timeline(Some(info)))
}

/// PQ or HLG: the tiles need the tone-map chain. [ts:modules/streaming/thumbnail.service.ts:43-47]
pub fn is_hdr(info: &MediaInfo) -> bool {
    let t = info.video.first().and_then(|v| v.color_transfer.as_deref());
    matches!(t, Some("smpte2084" | "arib-std-b67"))
}

/// `frame-<i+1>.jpg` of 0-based tile `i`.
pub fn frame_name(i: u32) -> String {
    format!("frame-{:04}.jpg", i + 1)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Extractor {
    Software,
    Vaapi { device: String },
    Qsv { device: String },
    Cuda,
    VideoToolbox,
}

impl Extractor {
    /// The backend whose decoder descriptors its `-hwaccel` is; `None` decodes on the CPU.
    fn decode_hw(&self) -> HwAccel {
        match self {
            Extractor::Software => HwAccel::None,
            Extractor::Vaapi { .. } => HwAccel::Vaapi,
            Extractor::Qsv { .. } => HwAccel::Qsv,
            Extractor::Cuda => HwAccel::Nvenc,
            Extractor::VideoToolbox => HwAccel::Videotoolbox,
        }
    }
}

/// The TS backend order over the detected host: VAAPI (no crop) then QSV (crop) on the render
/// node, CUDA, software; VideoToolbox on darwin. [ts:modules/streaming/thumbnail-extractors/index.ts:20-75]
/// Software too when the boot probe left that backend no decoder for `video` (its codec at its
/// bit depth): the run forces GPU frames, so it would fail instead of decoding on the CPU.
pub fn pick_extractor(
    caps: &HostCaps,
    render_node: &str,
    video: Option<&VideoStreamInfo>,
    force_software: bool,
) -> Extractor {
    if force_software {
        return Extractor::Software;
    }
    let crop = video.is_some_and(|v| v.crop.is_some());
    let device = render_node.to_string();
    let e = match (caps.platform, caps.hw_accel) {
        (Platform::Darwin, _) => Extractor::VideoToolbox,
        (Platform::Linux, HwAccel::Vaapi | HwAccel::Qsv) if !crop => Extractor::Vaapi { device },
        (Platform::Linux, HwAccel::Qsv) => Extractor::Qsv { device },
        (Platform::Linux, HwAccel::Nvenc) => Extractor::Cuda,
        _ => Extractor::Software,
    };
    let hw = e.decode_hw();
    let codec = video.map(|v| v.codec.as_str());
    let depth = video.and_then(|v| v.bit_depth).unwrap_or(8);
    if source_decoder(codec, depth, hw, caps).hw() == hw {
        e
    } else {
        Extractor::Software
    }
}

pub struct FrameArgs<'a> {
    pub input_path: &'a str,
    pub seek_seconds: f64,
    pub output_path: &'a str,
    pub crop: Option<&'a CropInfo>,
    pub thumb_width: u32,
    pub hdr: bool,
}

/// HDR to SDR BT.709 at tile size. [ts:modules/streaming/thumbnail-extractors/tonemap.ts]
fn tonemap_chain(w: u32) -> String {
    format!(
        "zscale=w={w}:h=-2:t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,\
         tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p"
    )
}

fn crop_step(c: &CropInfo) -> String {
    format!("crop={}:{}:{}:{}", c.width, c.height, c.x, c.y)
}

/// One frame's argv. [ts:modules/streaming/thumbnail-extractors/*.extractor.ts]
pub fn frame_args(e: &Extractor, a: &FrameArgs) -> Result<Vec<String>, String> {
    let ss = js_string(a.seek_seconds);
    let (mut v, vf) = input_and_filter(e, a, Input::Seek(&ss))?;
    v.extend(strs(&[
        "-frames:v",
        "1",
        "-vf",
        &vf,
        "-q:v",
        "5",
        "-y",
        a.output_path,
    ]));
    Ok(v)
}

/// Seconds of packets the keyframe probe reads when the file is too short for three windows.
pub const KEYFRAME_PROBE_SECONDS: u32 = 90;

/// Where the keyframe probe reads, as (start, seconds): the first `KEYFRAME_PROBE_SECONDS`
/// of a short file; else three windows at 0, 1/3 and 2/3 of `duration`, each of 30 s or three
/// intervals, so a busy opening does not speak for static scenes later on.
pub fn keyframe_probe_windows(duration: f64, interval: u32) -> Vec<(f64, f64)> {
    let len = f64::from(interval * 3).max(30.0);
    if duration.is_nan() || duration <= 3.0 * f64::from(KEYFRAME_PROBE_SECONDS) {
        return vec![(0.0, f64::from(KEYFRAME_PROBE_SECONDS))];
    }
    (0..3)
        .map(|k| ((duration * f64::from(k) / 3.0).floor(), len))
        .collect()
}

/// Packet times and flags of video stream `stream_index` (absolute, as `video[0].streamIndex`:
/// never an attached picture) over one probe window (demux only, nothing decoded). Lines:
/// `<pts_time>,<dts_time>,<flags>`.
pub fn keyframe_probe_args(input_path: &str, stream_index: u32, window: (f64, f64)) -> Vec<String> {
    let interval = format!("{}%+{}", js_string(window.0), js_string(window.1));
    strs(&[
        "-v",
        "error",
        "-select_streams",
        &stream_index.to_string(),
        "-read_intervals",
        &interval,
        "-show_entries",
        "packet=pts_time,dts_time,flags",
        "-of",
        "csv=p=0",
        input_path,
    ])
}

/// The time of a keyframe line of `keyframe_probe_args` (`pts_time`, else `dts_time` when the
/// container has no pts: AVI, some TS), `None` for any other packet.
pub fn keyframe_time(line: &str) -> Option<f64> {
    let mut f = line.trim().split(',');
    let (pts, dts, flags) = (f.next()?, f.next()?, f.next()?);
    if !flags.starts_with('K') {
        return None;
    }
    pts.parse().ok().or_else(|| dts.parse().ok())
}

/// Per-frame seeks cost less than a full pass: a seek decodes about half a GOP and costs about
/// twice a pass's frame (a process and a device start each; 519 seeks of an AV1 episode took 231 s
/// CPU against 43 s for its full pass), so seeks win when `count * mean GOP` < `duration`. Long-GOP
/// HEVC, where keyframes alone come out shuffled, then goes back to the frame runs.
pub fn seeks_cheaper(windows: &[Vec<f64>], count: u32, duration: f64) -> bool {
    let gaps: Vec<f64> = windows
        .iter()
        .flat_map(|w| {
            let mut t = w.clone();
            t.sort_by(f64::total_cmp);
            t.dedup();
            t.windows(2).map(|p| p[1] - p[0]).collect::<Vec<_>>()
        })
        .collect();
    if gaps.is_empty() || duration <= 0.0 {
        return false;
    }
    let mean = gaps.iter().sum::<f64>() / gaps.len() as f64;
    f64::from(count) * mean < duration
}

/// The widest gap between two keyframes of the sample; `None` under two keyframes.
fn max_keyframe_gap(times: &[f64]) -> Option<f64> {
    let mut t = times.to_vec();
    t.sort_by(f64::total_cmp);
    t.dedup();
    t.windows(2).map(|w| w[1] - w[0]).reduce(f64::max)
}

/// Keyframes alone put a tile on every mark: no two keyframes of the sample sit more than one
/// interval apart. Fewer than two keyframes (one GOP over the whole sample) says no.
pub fn keyframes_suffice(times: &[f64], interval: u32) -> bool {
    max_keyframe_gap(times).is_some_and(|g| g <= f64::from(interval) + 0.05)
}

/// HEVC frames between two keyframes from which `-skip_frame nokey` stops decoding them in
/// display order: the skipped frames break the POC derivation of open GOPs (x265's default), the
/// keyframes come out shuffled (`10 0 30 20 40`) and `fps` anchors on a late one.
pub const HEVC_MAX_KEYFRAME_GAP_FRAMES: f64 = 128.0;

/// Whether `-skip_frame nokey` gives the keyframes in display order: always, except HEVC, whose
/// sampled keyframes must sit under `HEVC_MAX_KEYFRAME_GAP_FRAMES` apart at a known frame rate.
pub fn keyframes_in_order(codec: &str, times: &[f64], fps: Option<f64>) -> bool {
    if !matches!(codec, "hevc" | "h265") {
        return true;
    }
    match (max_keyframe_gap(times), fps.filter(|f| *f > 0.0)) {
        (Some(gap), Some(fps)) => gap * fps < HEVC_MAX_KEYFRAME_GAP_FRAMES,
        _ => false,
    }
}

/// Frames per second of a stream: `avgFrameRate`, else `frameRate` (`n/d` or a number).
pub fn frames_per_second(v: &VideoStreamInfo) -> Option<f64> {
    let parse = |s: &str| -> Option<f64> {
        let f = match s.split_once('/') {
            Some((n, d)) => n.trim().parse::<f64>().ok()? / d.trim().parse::<f64>().ok()?,
            None => s.trim().parse().ok()?,
        };
        (f.is_finite() && f > 0.0).then_some(f)
    };
    [&v.avg_frame_rate, &v.frame_rate]
        .into_iter()
        .find_map(|r| r.as_deref().and_then(parse))
}

/// Unpaused wall time of the single pass: a slot decoding at a quarter of real time still ends.
pub fn pass_timeout_ms(duration: f64) -> u64 {
    u64::from(RUN_TIMEOUT_MS).saturating_add((duration.max(0.0) * 4000.0) as u64)
}

pub struct PassArgs<'a> {
    pub input_path: &'a str,
    /// `<frames>/frame-%04d.jpg`: the pass writes the frames the per-frame runs would.
    pub frames_pattern: &'a str,
    pub crop: Option<&'a CropInfo>,
    pub thumb_width: u32,
    pub hdr: bool,
    pub layout: Layout,
    pub seek_offset: f64,
    pub keyframes_only: bool,
    /// `video[0].codec`: VP9 drops packets for its keyframes only.
    pub codec: &'a str,
    /// `video[0].streamIndex`, mapped so the pass reads the stream the probe and `codec` describe.
    pub stream_index: u32,
}

/// VP9's keyframes only: the non-key packets dropped before the decoder, since the VP9 decoders
/// (native, libvpx, VAAPI) ignore `-skip_frame nokey`. Same frames as `-skip_frame` on the other
/// codecs, but an input bitstream filter fails on older ffmpeg (6.1), where `-skip_frame` works.
pub const VP9_KEYFRAMES_ONLY: &str = "noise=drop=not(key)";

/// Every frame of the sheet in one run: the file decoded once (keyframes only when asked), then
/// `fps=1/interval:round=up` keeps, for each mark, the frame on screen at it (`round=down` keeps
/// the last frame before the next mark, one interval late). `fps` comes before the scale chain:
/// only the kept frames are scaled (and downloaded from the GPU; VideoToolbox downloads every
/// decoded frame anyway). `-fps_mode passthrough`: with a non-zero `start_time`, `fps` numbers its
/// first frame 1, and the image2 muxer's CFR sync would duplicate it into slot 0, every tile one
/// interval late. `-progress` lines on stdout.
pub fn pass_args(e: &Extractor, a: &PassArgs) -> Result<Vec<String>, String> {
    let f = FrameArgs {
        input_path: a.input_path,
        seek_seconds: 0.0,
        output_path: a.frames_pattern,
        crop: a.crop,
        thumb_width: a.thumb_width,
        hdr: a.hdr,
    };
    let input = Input::Pass {
        keyframes_only: a.keyframes_only,
        vp9: a.codec == "vp9",
    };
    let (mut v, vf) = input_and_filter(e, &f, input)?;
    // Always anchored: without `start_time`, `fps` starts at the first frame it gets, and a first
    // frame past 0 (video starting after audio) shifts every tile one interval late.
    let fps = format!(
        "fps=1/{}:round=up:start_time={}",
        a.layout.interval,
        js_string(a.seek_offset)
    );
    let count = a.layout.count.to_string();
    v.extend(strs(&[
        "-map",
        &format!("0:{}", a.stream_index),
        "-fps_mode",
        "passthrough",
        "-frames:v",
        &count,
        "-vf",
        &format!("{fps},{vf}"),
        "-q:v",
        "5",
        "-start_number",
        "1",
        "-nostats",
        "-progress",
        "pipe:1",
        "-y",
        a.frames_pattern,
    ]));
    Ok(v)
}

/// How a run reads its input: one frame at a seek, or the whole file in one pass.
#[derive(Clone, Copy)]
enum Input<'a> {
    Seek(&'a str),
    Pass { keyframes_only: bool, vp9: bool },
}

/// The argv up to the input, and the scale (crop, tone-map) chain, of one extractor.
fn input_and_filter(
    e: &Extractor,
    a: &FrameArgs,
    input: Input,
) -> Result<(Vec<String>, String), String> {
    let w = a.thumb_width;
    let pix = if a.hdr { "p010le" } else { "nv12" };
    let hdr_tail = if a.hdr {
        format!(",{}", tonemap_chain(w))
    } else {
        String::new()
    };
    let cpu_scale = if a.hdr {
        tonemap_chain(w)
    } else {
        format!("scale={w}:-1")
    };
    let cpu_vf = match a.crop {
        Some(c) => format!("{},{cpu_scale}", crop_step(c)),
        None => cpu_scale.clone(),
    };
    let mut v = strs(&[
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-threads",
        "1",
    ]);
    let push = |v: &mut Vec<String>, xs: &[&str]| v.extend(strs(xs));
    let vf = match e {
        Extractor::Software => cpu_vf,
        Extractor::Vaapi { device } => {
            push(&mut v, &["-hwaccel", "vaapi", "-hwaccel_device", device]);
            push(&mut v, &["-hwaccel_output_format", "vaapi"]);
            format!("scale_vaapi={w}:-2:format={pix},hwdownload,format={pix}{hdr_tail}")
        }
        Extractor::Qsv { device } => {
            let c = a.crop.ok_or("QsvExtractor requires a crop config")?;
            let out_h = round(f64::from(c.height) * f64::from(w) / f64::from(c.width) / 2.0) * 2.0;
            push(&mut v, &["-init_hw_device", &format!("vaapi=va:{device}")]);
            push(
                &mut v,
                &["-init_hw_device", "qsv=hw@va", "-filter_hw_device", "hw"],
            );
            push(
                &mut v,
                &["-hwaccel", "vaapi", "-hwaccel_output_format", "vaapi"],
            );
            format!(
                "hwmap=derive_device=qsv,vpp_qsv=w={w}:h={}:cw={}:ch={}:cx={}:cy={}:format={pix},\
                 hwdownload,format={pix}{hdr_tail}",
                js_string(out_h),
                c.width,
                c.height,
                c.x,
                c.y
            )
        }
        Extractor::Cuda => {
            push(
                &mut v,
                &["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"],
            );
            match a.crop {
                Some(c) => {
                    let fmt = if a.hdr {
                        format!(",format={pix}")
                    } else {
                        String::new()
                    };
                    format!("hwdownload{fmt},{},{cpu_scale}", crop_step(c))
                }
                None => {
                    format!("scale_cuda=w={w}:h=-2:format={pix},hwdownload,format={pix}{hdr_tail}")
                }
            }
        }
        Extractor::VideoToolbox => {
            push(&mut v, &["-hwaccel", "videotoolbox"]);
            cpu_vf
        }
    };
    match input {
        Input::Seek(ss) => push(&mut v, &["-noaccurate_seek", "-ss", ss]),
        Input::Pass {
            keyframes_only: true,
            vp9: true,
        } => push(&mut v, &["-bsf:v", VP9_KEYFRAMES_ONLY]),
        Input::Pass {
            keyframes_only: true,
            ..
        } => push(&mut v, &["-skip_frame", "nokey"]),
        Input::Pass { .. } => {}
    }
    if *e == Extractor::Software {
        push(&mut v, &["-analyzeduration", "0", "-probesize", "5000000"]);
    }
    // The per-frame software run never dropped the other streams; the pass drops them everywhere.
    match (input, e) {
        (Input::Seek(_), Extractor::Software) => {}
        (Input::Seek(_), _) => push(&mut v, &["-an", "-sn"]),
        (Input::Pass { .. }, _) => push(&mut v, &["-an", "-sn", "-dn"]),
    }
    push(&mut v, &["-i", a.input_path]);
    Ok((v, vf))
}

/// Tiles `frame-%04d.jpg` into one JPEG. [ts:modules/streaming/thumbnail.service.ts:710-735]
pub fn tile_args(frames_pattern: &str, sprite_path: &str, rows: u32) -> Vec<String> {
    let tile = format!("tile={COLUMNS}x{rows}");
    [
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-start_number",
        "1",
        "-i",
        frames_pattern,
        "-vf",
        &tile,
        "-q:v",
        "5",
        "-y",
        sprite_path,
    ]
    .map(String::from)
    .to_vec()
}

/// `(missing, source)` copies (0-based) that leave no gap, applied in order: the nearest present
/// frame, the lower one first; a filled slot serves later ones. Empty when nothing was extracted.
/// [ts:modules/streaming/thumbnail.service.ts:624-660]
pub fn backfill(present: &[bool]) -> Vec<(usize, usize)> {
    let mut present = present.to_vec();
    let n = present.len();
    let mut out = vec![];
    for i in 0..n {
        if present[i] {
            continue;
        }
        let src = (1..n).find_map(|d| {
            if i >= d && present[i - d] {
                Some(i - d)
            } else {
                (i + d < n && present[i + d]).then_some(i + d)
            }
        });
        let Some(src) = src else { return out };
        out.push((i, src));
        present[i] = true;
    }
    out
}

/// (width, height) from a JPEG's start-of-frame header.
pub fn jpeg_size(b: &[u8]) -> Option<(u32, u32)> {
    if b.get(..2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut i = 2;
    while i + 3 < b.len() {
        if b[i] != 0xFF {
            return None;
        }
        let m = b[i + 1];
        if m == 0xFF {
            i += 1;
            continue;
        }
        if matches!(m, 0x01 | 0xD0..=0xD9) {
            i += 2;
            continue;
        }
        let len = usize::from(u16::from_be_bytes([b[i + 2], b[i + 3]]));
        if matches!(m, 0xC0..=0xCF) && !matches!(m, 0xC4 | 0xC8 | 0xCC) {
            let h = u16::from_be_bytes([*b.get(i + 5)?, *b.get(i + 6)?]);
            let w = u16::from_be_bytes([*b.get(i + 7)?, *b.get(i + 8)?]);
            return Some((w.into(), h.into()));
        }
        i += 2 + len;
    }
    None
}

/// `round(height / rows)` of the tiled sheet, 90 when unknown. [ts:modules/streaming/thumbnail.service.ts:404-418]
pub fn thumb_height(sheet_height: Option<u32>, rows: u32) -> u32 {
    match sheet_height {
        Some(h) if rows > 0 => round(f64::from(h) / f64::from(rows)) as u32,
        _ => 90,
    }
}

pub fn meta(l: Layout, thumb_height: u32) -> SpriteMeta {
    SpriteMeta {
        interval: l.interval,
        columns: COLUMNS,
        thumb_width: THUMB_WIDTH,
        thumb_height,
        count: l.count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NODE: &str = "/dev/dri/renderD129";

    fn caps(platform: Platform, hw: HwAccel, disabled: &[&str]) -> HostCaps {
        HostCaps {
            platform,
            hw_accel: hw,
            disabled_decoders: disabled.iter().map(|d| d.to_string()).collect(),
            ..HostCaps::default()
        }
    }

    fn video(codec: &str, depth: u32, crop: bool) -> VideoStreamInfo {
        serde_json::from_value(serde_json::json!({
            "streamIndex": 0, "codec": codec, "bitDepth": depth,
            "crop": crop.then(|| serde_json::json!({ "width": 1920, "height": 800, "x": 0, "y": 140 })),
        }))
        .unwrap()
    }

    #[test]
    fn extractor_follows_the_detected_backend() {
        use Platform::*;
        let pick = |p, hw, crop, sw| {
            pick_extractor(&caps(p, hw, &[]), NODE, Some(&video("h264", 8, crop)), sw)
        };
        let device = NODE.to_string();
        assert_eq!(
            pick(Linux, HwAccel::Qsv, false, false),
            Extractor::Vaapi {
                device: device.clone()
            }
        );
        assert_eq!(
            pick(Linux, HwAccel::Qsv, true, false),
            Extractor::Qsv {
                device: device.clone()
            }
        );
        assert_eq!(
            pick(Linux, HwAccel::Vaapi, false, false),
            Extractor::Vaapi { device }
        );
        assert_eq!(
            pick(Linux, HwAccel::Vaapi, true, false),
            Extractor::Software
        );
        assert_eq!(pick(Linux, HwAccel::Nvenc, true, false), Extractor::Cuda);
        assert_eq!(pick(Linux, HwAccel::Qsv, false, true), Extractor::Software);
        assert_eq!(
            pick(Darwin, HwAccel::None, true, false),
            Extractor::VideoToolbox
        );
        assert_eq!(
            pick(Win32, HwAccel::Nvenc, false, false),
            Extractor::Software
        );
        assert_eq!(
            pick_extractor(&caps(Linux, HwAccel::Nvenc, &[]), NODE, None, false),
            Extractor::Cuda,
            "no video stream reported: the backend's"
        );
    }

    #[test]
    fn a_codec_the_backend_cannot_decode_is_extracted_in_software() {
        use Platform::Linux;
        // An iGPU without AV1: the boot probe disabled both of its AV1 decoders.
        let igpu = caps(Linux, HwAccel::Qsv, &["av1_qsv_decode", "av1_vaapi_decode"]);
        let pick = |c: &HostCaps, codec, depth, crop| {
            pick_extractor(c, NODE, Some(&video(codec, depth, crop)), false)
        };
        assert_eq!(pick(&igpu, "av1", 8, false), Extractor::Software);
        assert_eq!(pick(&igpu, "av1", 10, true), Extractor::Software);
        assert_ne!(pick(&igpu, "hevc", 10, false), Extractor::Software);
        assert_ne!(pick(&igpu, "hevc", 10, true), Extractor::Software);
        // Each extractor reads its own backend's descriptor: VAAPI without a crop, QSV with one.
        let no_vaapi = caps(Linux, HwAccel::Qsv, &["av1_vaapi_decode"]);
        assert_eq!(pick(&no_vaapi, "av1", 8, false), Extractor::Software);
        assert_ne!(pick(&no_vaapi, "av1", 8, true), Extractor::Software);
        let nvidia = caps(Linux, HwAccel::Nvenc, &["vp9_cuda_decode"]);
        assert_eq!(pick(&nvidia, "vp9", 8, false), Extractor::Software);
        assert_eq!(pick(&nvidia, "h264", 8, false), Extractor::Cuda);
        // No hardware decoder is compiled in for these at all.
        assert_eq!(pick(&nvidia, "h264", 10, false), Extractor::Software);
        assert_eq!(pick(&nvidia, "mpeg4", 8, false), Extractor::Software);
    }

    #[test]
    fn keyframe_probe_lines() {
        assert_eq!(keyframe_time("12.345000,12.300000,K__"), Some(12.345));
        assert_eq!(keyframe_time("12.345000,12.300000,K_D\n"), Some(12.345));
        assert_eq!(keyframe_time("12.387000,12.300000,___"), None);
        assert_eq!(
            keyframe_time("N/A,7.000000,K__"),
            Some(7.0),
            "no pts (AVI, some TS): the dts"
        );
        assert_eq!(keyframe_time("N/A,N/A,K__"), None);
        assert_eq!(keyframe_time("12.345000,K__"), None);
        assert_eq!(keyframe_time("garbage"), None);
        assert_eq!(
            keyframe_probe_args("/m/e.mkv", 2, (2400.0, 90.0)).join(" "),
            "-v error -select_streams 2 -read_intervals 2400%+90 -show_entries \
             packet=pts_time,dts_time,flags -of csv=p=0 /m/e.mkv"
        );
    }

    #[test]
    fn keyframe_probe_spreads_over_long_files() {
        assert_eq!(keyframe_probe_windows(120.0, 2), [(0.0, 90.0)]);
        assert_eq!(keyframe_probe_windows(f64::NAN, 2), [(0.0, 90.0)]);
        assert_eq!(
            keyframe_probe_windows(2592.5, 5),
            [(0.0, 30.0), (864.0, 30.0), (1728.0, 30.0)]
        );
        assert_eq!(
            keyframe_probe_windows(7201.0, 30),
            [(0.0, 90.0), (2400.0, 90.0), (4800.0, 90.0)]
        );
    }

    #[test]
    fn seeks_win_when_a_gop_is_shorter_than_an_interval() {
        // 2 h film, 30 s interval (241 tiles), 10 s GOPs: 2410 < 7201.
        let ten_s = vec![0.0, 10.0, 20.0, 30.0];
        assert!(seeks_cheaper(
            &[ten_s.clone(), vec![2400.0, 2410.0]],
            241,
            7201.0
        ));
        // 43 min episode, 5 s interval (519 tiles), 10 s GOPs: a pass decodes less.
        assert!(!seeks_cheaper(&[ten_s], 519, 2592.5));
        // The gap between two windows is not a GOP.
        assert!(!seeks_cheaper(&[vec![0.0], vec![2400.0]], 241, 7201.0));
        assert!(!seeks_cheaper(&[], 241, 7201.0));
    }

    #[test]
    fn keyframes_suffice_when_one_falls_in_every_interval() {
        assert!(keyframes_suffice(&[0.0, 2.0, 4.5, 9.0], 5));
        assert!(
            keyframes_suffice(&[10.0, 0.0, 5.02], 5),
            "unsorted, inside the slack"
        );
        assert!(!keyframes_suffice(&[0.0, 4.0, 10.4], 5));
        assert!(
            !keyframes_suffice(&[0.0], 5),
            "a single GOP over the sample"
        );
        assert!(!keyframes_suffice(&[], 30));
        assert!(keyframes_suffice(&[0.0, 10.0, 20.0], 15));
    }

    #[test]
    fn hevc_keyframes_alone_need_short_gops() {
        let five_s = [0.0, 5.0, 10.0];
        let ten_s = [0.0, 10.0, 20.0];
        assert!(keyframes_in_order("av1", &ten_s, None));
        assert!(keyframes_in_order("h264", &[], None));
        assert!(
            keyframes_in_order("hevc", &five_s, Some(24.0)),
            "120 frames"
        );
        assert!(
            !keyframes_in_order("hevc", &ten_s, Some(24.0)),
            "240 frames"
        );
        assert!(
            !keyframes_in_order("hevc", &five_s, Some(30.0)),
            "150 frames"
        );
        assert!(!keyframes_in_order("h265", &five_s, None), "unknown rate");
        assert!(
            !keyframes_in_order("hevc", &[0.0], Some(24.0)),
            "no gap seen"
        );
    }

    #[test]
    fn frame_rates() {
        let mut v: VideoStreamInfo = serde_json::from_value(serde_json::json!({
            "streamIndex": 0, "codec": "hevc", "width": 1920, "height": 1080
        }))
        .unwrap();
        assert_eq!(frames_per_second(&v), None);
        v.frame_rate = Some("25".into());
        assert_eq!(frames_per_second(&v), Some(25.0));
        v.avg_frame_rate = Some("24000/1001".into());
        assert!((frames_per_second(&v).unwrap() - 23.976).abs() < 0.001);
        v.avg_frame_rate = Some("0/0".into());
        assert_eq!(frames_per_second(&v), Some(25.0), "0/0 falls back");
    }

    #[test]
    fn single_pass_argv() {
        let l = layout(2592.5);
        let a = PassArgs {
            input_path: "/m/e.mkv",
            frames_pattern: "/c/f/frame-%04d.jpg",
            crop: None,
            thumb_width: THUMB_WIDTH,
            hdr: false,
            layout: l,
            seek_offset: 0.0,
            keyframes_only: true,
            codec: "av1",
            stream_index: 0,
        };
        let vaapi = Extractor::Vaapi {
            device: "/dev/dri/renderD128".into(),
        };
        let got = pass_args(&vaapi, &a).unwrap().join(" ");
        assert_eq!(
            got,
            "-nostdin -hide_banner -loglevel error -threads 1 -hwaccel vaapi -hwaccel_device \
             /dev/dri/renderD128 -hwaccel_output_format vaapi -skip_frame nokey -an -sn -dn -i \
             /m/e.mkv -map 0:0 -fps_mode passthrough -frames:v 519 -vf fps=1/5:round=up:start_time=0,\
             scale_vaapi=240:-2:format=nv12,hwdownload,format=nv12 -q:v 5 -start_number 1 -nostats -progress pipe:1 -y \
             /c/f/frame-%04d.jpg"
        );
        let full = PassArgs {
            keyframes_only: false,
            seek_offset: 1.5,
            ..a
        };
        let got = pass_args(&Extractor::Software, &full).unwrap().join(" ");
        assert_eq!(
            got,
            "-nostdin -hide_banner -loglevel error -threads 1 -analyzeduration 0 -probesize \
             5000000 -an -sn -dn -i /m/e.mkv -map 0:0 -fps_mode passthrough -frames:v 519 -vf \
             fps=1/5:round=up:start_time=1.5,scale=240:-1 -q:v 5 -start_number 1 -nostats \
             -progress pipe:1 -y /c/f/frame-%04d.jpg"
        );
        // VP9 decoders ignore -skip_frame: its keyframes only drops packets instead.
        let vp9 = PassArgs { codec: "vp9", ..a };
        let got = pass_args(&Extractor::Software, &vp9).unwrap().join(" ");
        assert!(got.contains(" -bsf:v noise=drop=not(key) -analyzeduration 0 "));
        assert!(!got.contains("skip_frame"));
        let vp9_full = PassArgs {
            keyframes_only: false,
            ..vp9
        };
        let got = pass_args(&Extractor::Software, &vp9_full)
            .unwrap()
            .join(" ");
        assert!(!got.contains("-bsf"));
        let qsv = Extractor::Qsv {
            device: "/d".into(),
        };
        assert!(
            pass_args(&qsv, &a).is_err(),
            "QSV needs a crop, as per frame"
        );
        assert_eq!(pass_timeout_ms(100.0), 430_000);
    }

    #[test]
    fn jpeg_header_and_thumb_height() {
        // SOI, an APP0 segment, then SOF0 of 2400x1350.
        let j = [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x05,
            0x46, 0x09, 0x60, 0x03,
        ];
        assert_eq!(jpeg_size(&j), Some((2400, 1350)));
        assert_eq!(jpeg_size(b"\x89PNG"), None);
        assert_eq!(thumb_height(Some(1350), 10), 135);
        assert_eq!(thumb_height(None, 10), 90);
    }
}
