//! Live TV argv and bitrate parsing.

use super::super::{LiveArgs, LiveMode};
use crate::api::{HostCaps, HwAccel};
use crate::args::filters::DEINTERLACE;
use crate::hw::{Device, GpuDeint};
use crate::jsnum::{js_string as js, round};
use crate::ladder::height_for_bitrate;
use crate::strs;

/// Live input decode flags: the shared device init, `-hwaccel` of the backend.
/// [ts:backend/src/modules/livetv/services/live-ffmpeg-args.ts:60-89]
pub fn live_input(hw: HwAccel, dev: &Device) -> Vec<String> {
    match hw {
        HwAccel::Qsv => {
            let mut a = dev.qsv_init();
            a.extend(strs(&[
                "-hwaccel",
                "qsv",
                "-hwaccel_output_format",
                "qsv",
                "-hwaccel_device",
                "qs",
            ]));
            a
        }
        HwAccel::Vaapi => {
            let mut a = dev.vaapi_init();
            a.extend(strs(&[
                "-hwaccel",
                "vaapi",
                "-hwaccel_output_format",
                "vaapi",
                "-hwaccel_device",
                "va",
            ]));
            a
        }
        // Software frames, uploaded by the filter graph: NVDEC may not take the codec, and
        // `scale_cuda` takes only CUDA frames.
        HwAccel::Nvenc => dev.cuda_hwaccel(false),
        HwAccel::Videotoolbox => strs(&["-hwaccel", "videotoolbox"]),
        _ => vec![],
    }
}

/// The live transcode's encoder. [ts:backend/src/modules/livetv/services/live-ffmpeg-args.ts:47-54]
pub fn live_h264_encoder(hw: HwAccel) -> String {
    match hw {
        HwAccel::None => "libx264".into(),
        hw => format!("h264_{}", hw.as_str()),
    }
}

/// `-vf` of a transcode: an interlaced source is deinterlaced like VOD's (per frame when its field
/// order is unknown, except QSV: `vpp_qsv` cannot tell frames apart) and the picture is cut to the
/// rung the bitrate pays for, never enlarged.
pub fn live_filters(o: &LiveArgs, caps: &HostCaps) -> Option<String> {
    if o.mode == LiveMode::Remux {
        return None;
    }
    let height = o
        .video_bitrate_bps
        .filter(|b| *b > 0.0)
        .map(height_for_bitrate);
    let known = o.interlaced == Some(true);
    let deint = o.interlaced != Some(false);
    let h = |w: &str| height.map(|h| format!("{w}w=-2:h=min(ih\\,{h})"));
    // A hardware chain without a GPU deinterlacer round-trips the frames through the CPU.
    let bounce = |fmt: &str, up: &str| format!("hwdownload,format={fmt},{DEINTERLACE},{up}");
    let gpu = |g: GpuDeint| caps.probe(g.probe_name());
    let chain: Vec<Option<String>> = match o.hw_accel {
        HwAccel::Vaapi => [
            (deint && gpu(GpuDeint::Vaapi))
                .then(|| GpuDeint::Vaapi.filter().unwrap_or_default().to_string())
                .or_else(|| known.then(|| bounce("nv12", "hwupload=derive_device=vaapi"))),
            h("scale_vaapi="),
        ]
        .into(),
        HwAccel::Nvenc => {
            let size = h("scale_cuda=");
            let up = size.is_some().then_some("hwupload_cuda");
            let pre = if deint && gpu(GpuDeint::Cuda) {
                let f = GpuDeint::Cuda.filter().unwrap_or_default();
                Some(format!("hwupload_cuda,{f}"))
            } else if known {
                Some(strs_join([Some(DEINTERLACE), up]))
            } else {
                up.map(String::from)
            };
            [pre, size].into()
        }
        HwAccel::Qsv => {
            let vpp = |deint: &str, size: Option<String>| {
                let opts = format!("{deint}{}", size.unwrap_or_default());
                (!opts.is_empty()).then(|| format!("vpp_qsv={}", opts.trim_end_matches(':')))
            };
            if known && !gpu(GpuDeint::Qsv) {
                [
                    Some(bounce("nv12", "hwupload=extra_hw_frames=16")),
                    vpp("", h("")),
                ]
                .into()
            } else {
                [vpp(
                    if known { GpuDeint::Qsv.vpp_opts() } else { "" },
                    h(""),
                )]
                .into()
            }
        }
        // CPU frames: the software encoder's, and VideoToolbox's downloaded ones.
        _ => [
            deint.then(|| DEINTERLACE.to_string()),
            height.map(|h| format!("scale=-2:min(ih\\,{h})")),
        ]
        .into(),
    };
    let chain: Vec<String> = chain.into_iter().flatten().collect();
    (!chain.is_empty()).then(|| chain.join(","))
}

fn strs_join<const N: usize>(parts: [Option<&str>; N]) -> String {
    parts.into_iter().flatten().collect::<Vec<_>>().join(",")
}

/// Live video: copy, or H.264 on the backend's encoder cut on a time grid.
/// [ts:backend/src/modules/livetv/services/live-ffmpeg-args.ts:135-156]
pub fn live_video(o: &LiveArgs) -> Vec<String> {
    if o.mode == LiveMode::Remux {
        return strs(&["-c:v", "copy"]);
    }
    let mut a = vec!["-c:v".to_string(), live_h264_encoder(o.hw_accel)];
    if o.hw_accel == HwAccel::None {
        let threads = o.encode_threads.max(1).to_string();
        a.extend(["-threads:v".into(), threads]);
        a.extend(strs(&["-preset", "veryfast", "-tune", "zerolatency"]));
    }
    let b = o.video_bitrate_bps.unwrap_or(4_000_000.0);
    a.extend([
        "-b:v".into(),
        js(b),
        "-maxrate".into(),
        js(b),
        "-bufsize".into(),
        js(b * 2.0),
        "-force_key_frames".into(),
        format!("expr:gte(t,n_forced*{})", js(o.segment_seconds)),
    ]);
    a
}

/// `parseBitrateToBps`: `8M`, `500k`, `192k` or bare bps; anything else is 0.
pub fn parse_bitrate(s: &str) -> f64 {
    let t = s.trim();
    let (num, unit) = match t.char_indices().last() {
        Some((i, c)) if "kKmM".contains(c) => (t[..i].trim_end(), Some(c.to_ascii_lowercase())),
        _ => (t, None),
    };
    let (int, frac) = num.split_once('.').unwrap_or((num, "0"));
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    if !digits(int) || !digits(frac) {
        return 0.0;
    }
    let n: f64 = num.parse().unwrap_or(0.0);
    match unit {
        Some('m') => round(n * 1e6),
        Some('k') => round(n * 1e3),
        _ => round(n),
    }
}

#[cfg(test)]
mod bitrate_tests {
    use super::super::*;

    #[test]
    fn bitrates() {
        assert_eq!(parse_bitrate("8M"), 8e6);
        assert_eq!(parse_bitrate(" 192k "), 192e3);
        assert_eq!(parse_bitrate("1.5 M"), 1.5e6);
        assert_eq!(parse_bitrate("8000000"), 8e6);
        assert_eq!(parse_bitrate("8MB"), 0.0);
        assert_eq!(parse_bitrate(".5M"), 0.0);
    }
}
