//! Seek-bar sprite: layout, frame extractor choice and argv, tiling, missing-frame backfill and the
//! JPEG header read for `thumbHeight`. Spec: `docs/spec/jobs.md` section 5.
use crate::api::{CropInfo, HwAccel, MediaInfo, Platform, SpriteMeta};
use crate::jsnum::{js_string, round};
use crate::mp4::{input_seek_seconds, source_timeline};
use crate::strs;

pub const COLUMNS: u32 = 10;
pub const THUMB_WIDTH: u32 = 240;
/// Unpaused wall time of one frame or tile run. [ts:modules/streaming/thumbnail.service.ts:682,744]
pub const RUN_TIMEOUT_MS: u32 = 30_000;
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

/// The TS backend order over the detected host: VAAPI (no crop) then QSV (crop) on the render
/// node, CUDA, software; VideoToolbox on darwin. [ts:modules/streaming/thumbnail-extractors/index.ts:20-75]
pub fn pick_extractor(
    platform: Platform,
    hw: HwAccel,
    render_node: &str,
    crop: bool,
    force_software: bool,
) -> Extractor {
    if force_software {
        return Extractor::Software;
    }
    let device = render_node.to_string();
    match (platform, hw) {
        (Platform::Darwin, _) => Extractor::VideoToolbox,
        (Platform::Linux, HwAccel::Vaapi | HwAccel::Qsv) if !crop => Extractor::Vaapi { device },
        (Platform::Linux, HwAccel::Qsv) => Extractor::Qsv { device },
        (Platform::Linux, HwAccel::Nvenc) => Extractor::Cuda,
        _ => Extractor::Software,
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
    let mut v: Vec<String> = [
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-threads",
        "1",
    ]
    .map(String::from)
    .to_vec();
    let push = |v: &mut Vec<String>, xs: &[&str]| v.extend(strs(xs));
    let ss = js_string(a.seek_seconds);
    let vf = match e {
        Extractor::Software => {
            push(&mut v, &["-noaccurate_seek", "-ss", &ss]);
            push(&mut v, &["-analyzeduration", "0", "-probesize", "5000000"]);
            push(&mut v, &["-i", a.input_path]);
            cpu_vf
        }
        Extractor::Vaapi { device } => {
            push(&mut v, &["-hwaccel", "vaapi", "-hwaccel_device", device]);
            push(
                &mut v,
                &["-hwaccel_output_format", "vaapi", "-noaccurate_seek"],
            );
            push(&mut v, &["-ss", &ss, "-an", "-sn", "-i", a.input_path]);
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
            push(
                &mut v,
                &[
                    "-noaccurate_seek",
                    "-ss",
                    &ss,
                    "-an",
                    "-sn",
                    "-i",
                    a.input_path,
                ],
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
            push(
                &mut v,
                &[
                    "-noaccurate_seek",
                    "-ss",
                    &ss,
                    "-an",
                    "-sn",
                    "-i",
                    a.input_path,
                ],
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
            push(&mut v, &["-hwaccel", "videotoolbox", "-noaccurate_seek"]);
            push(&mut v, &["-ss", &ss, "-an", "-sn", "-i", a.input_path]);
            cpu_vf
        }
    };
    push(
        &mut v,
        &[
            "-frames:v",
            "1",
            "-vf",
            &vf,
            "-q:v",
            "5",
            "-y",
            a.output_path,
        ],
    );
    Ok(v)
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

    #[test]
    fn extractor_follows_the_detected_backend() {
        use Platform::*;
        let pick = |p, hw, crop, sw| pick_extractor(p, hw, "/dev/dri/renderD129", crop, sw);
        let node = "/dev/dri/renderD129".to_string();
        assert_eq!(
            pick(Linux, HwAccel::Qsv, false, false),
            Extractor::Vaapi {
                device: node.clone()
            }
        );
        assert_eq!(
            pick(Linux, HwAccel::Qsv, true, false),
            Extractor::Qsv {
                device: node.clone()
            }
        );
        assert_eq!(
            pick(Linux, HwAccel::Vaapi, false, false),
            Extractor::Vaapi { device: node }
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
