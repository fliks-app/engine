//! Command lines of the probe tools: the same arguments, order and timeouts as the host.

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

fn v(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| (*s).to_string()).collect()
}

fn with(mut a: Vec<String>, rest: &[String]) -> Vec<String> {
    a.extend_from_slice(rest);
    a
}

pub fn main(path: &str) -> Vec<String> {
    with(
        v(&[
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
        v(&["-v", "error", "-select_streams"]),
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
        v(&["-v", "error", "-select_streams"]),
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
        v(&[
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
        v(&["-v", "error", "-analyzeduration"]),
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
    v(&["-J", path])
}

/// One software-decoded cropdetect sample.
pub fn crop_sample(path: &str, ss: u32) -> Vec<String> {
    with(
        v(&["-threads", "2", "-ss"]),
        &[
            ss.to_string(),
            "-i".into(),
            path.into(),
            "-t".into(),
            "5".into(),
            "-vf".into(),
            CROP_FILTER.into(),
            "-an".into(),
            "-f".into(),
            "null".into(),
            "-".into(),
        ],
    )
}
