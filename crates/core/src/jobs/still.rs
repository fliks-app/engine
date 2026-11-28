//! One JPEG frame of a video file. Spec: `docs/spec/jobs.md` section 9b.
use super::cover::MAX_BYTES;
use super::sprite::{is_hdr, seek_offset, tonemap_chain};
use crate::api::{MediaInfo, VideoStreamInfo};
use crate::jsnum::{js_string, round};
use crate::strs;

/// Frames the `thumbnail` filter picks the most representative of.
pub const CANDIDATES: u32 = 24;
pub const MAX_WIDTH: u32 = 1920;
pub const FILE: &str = "still.jpg";
pub const MIME: &str = "image/jpeg";

/// Where the capture starts: `atSeconds`, else a tenth of the duration.
pub fn offset(at: Option<f64>, duration: Option<f64>) -> Option<f64> {
    at.or_else(|| duration.filter(|d| *d > 0.0).map(|d| d / 10.0))
}

/// The content position the capture starts at.
pub fn validate(info: &MediaInfo, at: Option<f64>) -> Result<f64, String> {
    if info.video.is_empty() {
        return Err("Still needs a video stream in info".into());
    }
    if at.is_some_and(|a| !(a.is_finite() && a >= 0.0)) {
        return Err("Still atSeconds must be a finite number of seconds, 0 or more".into());
    }
    let duration = info.duration_seconds.filter(|d| d.is_finite());
    offset(at, duration)
        .ok_or_else(|| "Still needs atSeconds or a positive info.durationSeconds".into())
}

fn ratio(s: &str) -> Option<f64> {
    let (a, b) = s.split_once(':')?;
    let (a, b) = (a.parse::<f64>().ok()?, b.parse::<f64>().ok()?);
    (a > 0.0 && b > 0.0).then_some(a / b)
}

/// Displayed size, even, at most `MAX_WIDTH` wide: the sample aspect applied through the
/// display aspect ratio (the stored size when absent).
pub fn size(v: &VideoStreamInfo) -> Option<(u32, u32)> {
    let (w, h) = (f64::from(v.width?), f64::from(v.height?));
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let dar = v
        .display_aspect_ratio
        .as_deref()
        .and_then(ratio)
        .unwrap_or(w / h);
    let out_w = (round(h * dar).min(f64::from(MAX_WIDTH)) / 2.0).floor() * 2.0;
    let out_h = (round(out_w / dar) / 2.0).floor() * 2.0;
    (out_w >= 2.0 && out_h >= 2.0).then_some((out_w as u32, out_h as u32))
}

/// scale, then the best of `CANDIDATES` frames, then the tone map of an HDR stream.
pub fn filter(info: &MediaInfo) -> String {
    let size = info.video.first().and_then(size);
    let scale = match size {
        Some((w, h)) => format!("scale={w}:{h}"),
        None => format!("scale='min({MAX_WIDTH},iw)':-2"),
    };
    let mut vf = format!("{scale},thumbnail=n={CANDIDATES}");
    if is_hdr(info) {
        vf.push(',');
        vf.push_str(&tonemap_chain(size.map_or(MAX_WIDTH, |s| s.0)));
    }
    vf
}

/// The best of `CANDIDATES` frames from content position `at`. MPEG-TS decodes keyframes only:
/// its seek lands anywhere and a delta frame would start corrupt.
pub fn capture_args(input: &str, info: &MediaInfo, at: f64, output: &str) -> Vec<String> {
    let mut a = strs(&["-nostdin", "-hide_banner", "-loglevel", "error", "-y"]);
    if info
        .format_name
        .as_deref()
        .is_some_and(|f| f.contains("mpegts"))
    {
        a.extend(strs(&["-skip_frame", "nokey"]));
    }
    a.push("-ss".into());
    // ffmpeg adds the container start to the input seek.
    a.push(js_string(at + seek_offset(info)));
    a.extend(strs(&["-i", input, "-map"]));
    a.push(format!("0:{}", info.video[0].stream_index));
    a.extend(strs(&["-an", "-sn", "-dn", "-vf"]));
    a.push(filter(info));
    a.extend(strs(&["-frames:v", "1", "-q:v", "3", "-fs"]));
    a.push((MAX_BYTES + 1).to_string());
    a.extend(strs(&["-f", "image2", "-update", "1"]));
    a.push(output.into());
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn info(v: Value, extra: Value) -> MediaInfo {
        let mut i = json!({ "video": [v], "audio": [], "subtitles": [] });
        i.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(i).unwrap()
    }

    fn sdr() -> MediaInfo {
        info(
            json!({ "streamIndex": 3, "codec": "h264", "width": 1440, "height": 1080,
                "displayAspectRatio": "16:9" }),
            json!({ "durationSeconds": 600.0 }),
        )
    }

    #[test]
    fn the_offset_is_given_or_a_tenth_of_the_duration() {
        assert_eq!(offset(Some(42.0), Some(600.0)), Some(42.0));
        assert_eq!(offset(None, Some(600.0)), Some(60.0));
        assert_eq!(offset(None, None), None);
        assert_eq!(offset(None, Some(0.0)), None);
        assert_eq!(validate(&sdr(), None), Ok(60.0));
        assert!(validate(&sdr(), Some(-1.0)).is_err());
        assert!(validate(&MediaInfo::default(), Some(1.0)).is_err());
        let no_duration = info(json!({ "streamIndex": 0, "codec": "h264" }), json!({}));
        assert!(validate(&no_duration, None).is_err());
        assert_eq!(validate(&no_duration, Some(5.0)), Ok(5.0));
    }

    #[test]
    fn the_size_applies_the_sample_aspect_and_caps_the_width() {
        assert_eq!(size(&sdr().video[0]), Some((1920, 1080)));
        let uhd = info(
            json!({ "streamIndex": 0, "codec": "hevc", "width": 3840, "height": 2160 }),
            json!({}),
        );
        assert_eq!(size(&uhd.video[0]), Some((1920, 1080)));
        let small = info(
            json!({ "streamIndex": 0, "codec": "mpeg2video", "width": 720, "height": 576,
                "displayAspectRatio": "4:3" }),
            json!({}),
        );
        assert_eq!(size(&small.video[0]), Some((768, 576)));
    }

    #[test]
    fn a_still_scales_picks_then_tone_maps_an_hdr_stream() {
        assert_eq!(filter(&sdr()), "scale=1920:1080,thumbnail=n=24");
        let hdr = info(
            json!({ "streamIndex": 0, "codec": "hevc", "width": 1920, "height": 1080,
                "colorTransfer": "smpte2084" }),
            json!({}),
        );
        assert_eq!(
            filter(&hdr),
            format!("scale=1920:1080,thumbnail=n=24,{}", tonemap_chain(1920))
        );
    }

    #[test]
    fn the_argv_seeks_the_input_and_keeps_keyframes_for_mpegts() {
        let argv = capture_args("/m.mkv", &sdr(), 60.0, "/t/still-tmp.jpg").join(" ");
        assert_eq!(
            argv,
            "-nostdin -hide_banner -loglevel error -y -ss 60 -i /m.mkv -map 0:3 -an -sn -dn \
             -vf scale=1920:1080,thumbnail=n=24 -frames:v 1 -q:v 3 -fs 16777217 -f image2 \
             -update 1 /t/still-tmp.jpg"
        );
        let mut ts = sdr();
        ts.format_name = Some("mpegts".into());
        let argv = capture_args("/m.ts", &ts, 60.0, "/o.jpg").join(" ");
        assert!(argv.contains("-y -skip_frame nokey -ss 60 -i"), "{argv}");
    }
}
