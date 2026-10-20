//! Live TV argv and bitrate parsing.

use super::super::{LiveArgs, LiveMode};
use super::hw_name;
use crate::api::HwAccel;
use crate::hw::Device;
use crate::jsnum::{js_string as js, round};
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
        HwAccel::Nvenc => strs(&["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"]),
        HwAccel::Videotoolbox => strs(&["-hwaccel", "videotoolbox"]),
        _ => vec![],
    }
}

/// The live transcode's encoder. [ts:backend/src/modules/livetv/services/live-ffmpeg-args.ts:47-54]
pub fn live_h264_encoder(hw: HwAccel) -> String {
    match hw {
        HwAccel::None => "libx264".into(),
        hw => format!("h264_{}", hw_name(hw)),
    }
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
