//! Command lines of the probe tools: the same arguments, order and timeouts as the host.
use crate::api::Platform;
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

/// Hardware decode args of a crop sample, `None` when the host has nothing usable. Only VAAPI keeps
/// frames on the device; the others let ffmpeg download, since `hwdownload` fails on 10-bit.
/// [ts:backend/src/modules/media-probe/ffprobe.service.ts:37-60]
pub fn crop_hw_decode(
    platform: Platform,
    render_node: &str,
    node_exists: bool,
    nvidia_exists: bool,
) -> Option<Vec<String>> {
    match platform {
        Platform::Linux if node_exists => Some(strs(&[
            "-hwaccel",
            "vaapi",
            "-hwaccel_device",
            render_node,
            "-hwaccel_output_format",
            "vaapi",
        ])),
        // The nvidia runtime injects /dev/nvidiactl but no /dev/dri node.
        Platform::Linux if nvidia_exists => Some(strs(&["-hwaccel", "cuda"])),
        Platform::Linux => None,
        Platform::Win32 => Some(strs(&["-hwaccel", "d3d11va"])),
        Platform::Darwin => Some(strs(&["-hwaccel", "videotoolbox"])),
    }
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

    #[test]
    fn hw_decode_follows_the_platform() {
        let n = "/dev/dri/renderD129";
        let vaapi = crop_hw_decode(Platform::Linux, n, true, true).unwrap();
        assert_eq!(
            vaapi,
            [
                "-hwaccel",
                "vaapi",
                "-hwaccel_device",
                n,
                "-hwaccel_output_format",
                "vaapi"
            ]
        );
        let cuda = crop_hw_decode(Platform::Linux, n, false, true);
        assert_eq!(cuda.unwrap(), ["-hwaccel", "cuda"]);
        assert_eq!(crop_hw_decode(Platform::Linux, n, false, false), None);
        let win = crop_hw_decode(Platform::Win32, n, false, false);
        assert_eq!(win.unwrap(), ["-hwaccel", "d3d11va"]);
        let mac = crop_hw_decode(Platform::Darwin, n, false, false);
        assert_eq!(mac.unwrap(), ["-hwaccel", "videotoolbox"]);
    }

    #[test]
    fn only_device_frames_are_downloaded() {
        let vaapi = crop_hw_decode(Platform::Linux, "/n", true, false).unwrap();
        let a = crop_sample("/f", 7, Some(&vaapi));
        assert_eq!(a[..2], ["-hwaccel", "vaapi"]);
        assert!(
            a.iter()
                .any(|x| x.starts_with("hwdownload,format=nv12,cropdetect"))
        );
        let cuda = crop_hw_decode(Platform::Linux, "/n", false, true).unwrap();
        let a = crop_sample("/f", 7, Some(&cuda));
        assert!(
            a.contains(&CROP_FILTER.to_string()) && !a.iter().any(|x| x.contains("hwdownload"))
        );
        let sw = crop_sample("/f", 7, None);
        assert_eq!(sw[..3], ["-threads", "2", "-ss"]);
        assert!(sw.contains(&CROP_FILTER.to_string()));
    }
}
