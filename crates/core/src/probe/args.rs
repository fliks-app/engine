//! Command lines of the probe tools: the same arguments, order and timeouts as the host.
use crate::api::{HostCaps, HwAccel, Platform, VideoStreamInfo};
use crate::hw::decoders::decode_on;
use crate::strs;

pub const MAIN_TIMEOUT_MS: u64 = 30_000;
pub const FPS_TIMEOUT_MS: u64 = 30_000;
pub const FIRST_FRAME_TIMEOUT_MS: u64 = 30_000;
pub const AUDIO_PACKETS_TIMEOUT_MS: u64 = 60_000;
pub const LATE_AUDIO_TIMEOUT_MS: u64 = 120_000;
pub const MKVMERGE_TIMEOUT_MS: u64 = 15_000;
pub const CROP_TIMEOUT_MS: u64 = 30_000;
/// `execFile` default `maxBuffer` of the host's ffprobe and ffmpeg runs.
pub const MAX_OUTPUT_BYTES: usize = 1 << 20;
/// Output cap of the ffprobe JSON runs: an audiobook with thousands of chapters passes 1 MiB.
pub const FFPROBE_MAX_OUTPUT_BYTES: usize = 1 << 24;
/// `maxBuffer` of the host's `mkvmerge -J`.
pub const MKVMERGE_MAX_OUTPUT_BYTES: usize = 1 << 24;

/// `format=nv12` first: cropdetect compares `limit` to raw samples, 10-bit black sits near 64.
const CROP_FILTER: &str = "format=nv12,cropdetect=limit=24:round=16:reset=0:skip=24";

fn with(mut a: Vec<String>, rest: &[String]) -> Vec<String> {
    a.extend_from_slice(rest);
    a
}

pub fn main(path: &str) -> Vec<String> {
    with(
        strs(&[
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_streams",
            "-show_format",
            "-show_chapters",
        ]),
        &[path.into()],
    )
}

pub fn fps(path: &str, stream: u32, from: u32) -> Vec<String> {
    with(
        strs(&["-v", "error", "-select_streams"]),
        &[
            stream.to_string(),
            "-show_entries".into(),
            "packet=pts_time".into(),
            "-read_intervals".into(),
            format!("{from}%+60"),
            "-of".into(),
            "csv=p=0".into(),
            path.into(),
        ],
    )
}

pub fn first_frame(path: &str, stream: u32) -> Vec<String> {
    with(
        strs(&["-v", "error", "-select_streams"]),
        &[
            stream.to_string(),
            "-show_entries".into(),
            "frame=pts,best_effort_timestamp,side_data_list:frame_side_data=side_data_type,red_x,red_y,green_x,green_y,blue_x,blue_y,white_point_x,white_point_y,min_luminance,max_luminance,max_content,max_average".into(),
            "-of".into(),
            "json".into(),
            path.into(),
        ],
    )
}

pub fn audio_packets(path: &str) -> Vec<String> {
    with(
        strs(&[
            "-v",
            "error",
            "-show_entries",
            "packet=stream_index,pts_time",
            "-of",
            "csv=p=0",
        ]),
        &[path.into()],
    )
}

pub fn late_audio(path: &str, reach_seconds: f64, file_size: u64) -> Vec<String> {
    with(
        strs(&["-v", "error", "-analyzeduration"]),
        &[
            ((reach_seconds * 1_000_000.0).ceil() as u64).to_string(),
            "-probesize".into(),
            file_size.to_string(),
            "-select_streams".into(),
            "a".into(),
            "-show_entries".into(),
            "stream=index,channels,channel_layout,sample_rate".into(),
            "-of".into(),
            "json".into(),
            path.into(),
        ],
    )
}

pub fn mkvmerge(path: &str) -> Vec<String> {
    strs(&["-J", path])
}

/// Hardware decode args of a crop sample of `video`, `None` for software. The backend follows the
/// host: VAAPI on the render node of a `vaapi` or `qsv` Linux host, CUDA on an `nvenc` one, D3D11VA
/// on Windows, VideoToolbox on darwin; software when the boot probe left it no decoder for the
/// video (`decode_on`). Only VAAPI keeps frames on the device; the others let ffmpeg download,
/// since `hwdownload` fails on 10-bit. [ts:backend/src/modules/media-probe/ffprobe.service.ts:37-60]
pub fn crop_hw_decode(
    caps: &HostCaps,
    render_node: &str,
    video: &VideoStreamInfo,
) -> Option<Vec<String>> {
    let (backend, args) = match (caps.platform, caps.hw_accel) {
        (Platform::Linux, HwAccel::Vaapi | HwAccel::Qsv) => (
            HwAccel::Vaapi,
            strs(&[
                "-hwaccel",
                "vaapi",
                "-hwaccel_device",
                render_node,
                "-hwaccel_output_format",
                "vaapi",
            ]),
        ),
        (Platform::Linux, HwAccel::Nvenc) => (HwAccel::Nvenc, strs(&["-hwaccel", "cuda"])),
        (Platform::Linux, _) => return None,
        // The registry pairs the D3D11VA decoders with AMF; any Windows GPU runs them.
        (Platform::Win32, _) => (HwAccel::Amf, strs(&["-hwaccel", "d3d11va"])),
        (Platform::Darwin, _) => (HwAccel::Videotoolbox, strs(&["-hwaccel", "videotoolbox"])),
    };
    let on = decode_on(backend, Some(&video.codec), video.bit_depth, caps);
    (on == backend).then_some(args)
}

/// One cropdetect sample, decoded with `hw` or in software (2 threads: a 4K sample can outrun the
/// timeout on a weak CPU).
pub fn crop_sample(path: &str, ss: u32, hw: Option<&[String]>) -> Vec<String> {
    let on_device = hw.is_some_and(|h| h.iter().any(|a| a == "-hwaccel_output_format"));
    let mut a = hw.map_or_else(|| strs(&["-threads", "2"]), <[String]>::to_vec);
    a.extend([
        "-ss".into(),
        ss.to_string(),
        "-i".into(),
        path.into(),
        "-t".into(),
        "5".into(),
        "-vf".into(),
        if on_device {
            format!("hwdownload,{CROP_FILTER}")
        } else {
            CROP_FILTER.into()
        },
        "-an".into(),
        "-f".into(),
        "null".into(),
        "-".into(),
    ]);
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(platform: Platform, hw: HwAccel, disabled: &[&str]) -> HostCaps {
        HostCaps {
            platform,
            hw_accel: hw,
            disabled_decoders: disabled.iter().map(|d| d.to_string()).collect(),
            ..HostCaps::default()
        }
    }

    fn video(codec: &str, depth: u32) -> VideoStreamInfo {
        serde_json::from_value(serde_json::json!({
            "streamIndex": 0, "codec": codec, "bitDepth": depth,
        }))
        .unwrap()
    }

    #[test]
    fn hw_decode_follows_the_detected_backend() {
        use Platform::*;
        let n = "/dev/dri/renderD129";
        let h264 = video("h264", 8);
        let pick = |p, hw| crop_hw_decode(&host(p, hw, &[]), n, &h264);
        let vaapi = [
            "-hwaccel",
            "vaapi",
            "-hwaccel_device",
            n,
            "-hwaccel_output_format",
            "vaapi",
        ];
        assert_eq!(pick(Linux, HwAccel::Vaapi).unwrap(), vaapi);
        assert_eq!(pick(Linux, HwAccel::Qsv).unwrap(), vaapi);
        assert_eq!(pick(Linux, HwAccel::Nvenc).unwrap(), ["-hwaccel", "cuda"]);
        // No backend detected: a render node alone is no reason to try VAAPI.
        assert_eq!(pick(Linux, HwAccel::None), None);
        assert_eq!(
            pick(Win32, HwAccel::Nvenc).unwrap(),
            ["-hwaccel", "d3d11va"]
        );
        let mac = pick(Darwin, HwAccel::Videotoolbox);
        assert_eq!(mac.unwrap(), ["-hwaccel", "videotoolbox"]);
    }

    #[test]
    fn a_codec_the_backend_cannot_decode_is_sampled_in_software() {
        use Platform::*;
        let n = "/dev/dri/renderD129";
        // An iGPU without AV1, as the boot probe found it.
        let igpu = host(Linux, HwAccel::Qsv, &["av1_qsv_decode", "av1_vaapi_decode"]);
        assert_eq!(crop_hw_decode(&igpu, n, &video("av1", 8)), None);
        assert!(crop_hw_decode(&igpu, n, &video("hevc", 10)).is_some());
        // The samples run VAAPI: its descriptor decides, not QSV's.
        let no_vaapi = host(Linux, HwAccel::Qsv, &["av1_vaapi_decode"]);
        assert_eq!(crop_hw_decode(&no_vaapi, n, &video("av1", 8)), None);
        let nvidia = host(Linux, HwAccel::Nvenc, &[]);
        assert_eq!(crop_hw_decode(&nvidia, n, &video("h264", 10)), None);
        let win = host(
            Win32,
            HwAccel::Qsv,
            &["vp9_d3d11va_decode", "vp9_d3d11va_native_decode"],
        );
        assert_eq!(crop_hw_decode(&win, n, &video("vp9", 8)), None);
        let mac = host(Darwin, HwAccel::Videotoolbox, &[]);
        assert_eq!(crop_hw_decode(&mac, n, &video("vc1", 8)), None);
    }

    #[test]
    fn only_device_frames_are_downloaded() {
        let h264 = video("h264", 8);
        let vaapi = crop_hw_decode(&host(Platform::Linux, HwAccel::Vaapi, &[]), "/n", &h264);
        let a = crop_sample("/f", 7, vaapi.as_deref());
        assert_eq!(a[..2], ["-hwaccel", "vaapi"]);
        assert!(
            a.iter()
                .any(|x| x.starts_with("hwdownload,format=nv12,cropdetect"))
        );
        let cuda = crop_hw_decode(&host(Platform::Linux, HwAccel::Nvenc, &[]), "/n", &h264);
        let a = crop_sample("/f", 7, cuda.as_deref());
        assert!(
            a.contains(&CROP_FILTER.to_string()) && !a.iter().any(|x| x.contains("hwdownload"))
        );
        let sw = crop_sample("/f", 7, None);
        assert_eq!(sw[..3], ["-threads", "2", "-ss"]);
        assert!(sw.contains(&CROP_FILTER.to_string()));
    }
}
