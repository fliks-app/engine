//! Convert a sidecar or an embedded text track to SRT. Spec: `docs/spec/subtitle-jobs.md` section 6.
use crate::api::{MediaInfo, SrtSource};
use crate::strs;

pub const TIMEOUT_MS: u64 = 120_000;
pub const MAX_STREAM_INDEX: u32 = 999;

pub fn validate_srt(info: &MediaInfo, source: &SrtSource) -> Result<(), String> {
    match source {
        SrtSource::File { path } => {
            if !std::path::Path::new(path).is_absolute() {
                return Err("SubtitleToSrt file path must be absolute".into());
            }
        }
        SrtSource::Stream { index } => {
            if *index > MAX_STREAM_INDEX {
                return Err(format!("Invalid stream index: above {MAX_STREAM_INDEX}"));
            }
            let track = info
                .subtitles
                .iter()
                .find(|s| s.stream_index == *index)
                .ok_or_else(|| {
                    "SubtitleToSrt stream index must be in info.subtitles".to_string()
                })?;
            if track.is_image_based {
                return Err("SubtitleToSrt stream must be a text track".into());
            }
        }
    }
    Ok(())
}

/// ffmpeg writing SRT on stdout. `map` is set for an embedded stream.
pub fn to_srt_argv(input: &str, map: Option<u32>) -> Vec<String> {
    let mut a = strs(&["-y", "-i"]);
    a.push(input.into());
    if let Some(index) = map {
        a.push("-map".into());
        a.push(format!("0:{index}"));
    }
    a.extend(strs(&["-f", "srt", "pipe:1"]));
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::SubtitleStreamInfo;

    fn text_track(index: u32, image: bool) -> MediaInfo {
        MediaInfo {
            subtitles: vec![SubtitleStreamInfo {
                stream_index: index,
                codec: "subrip".into(),
                language: "eng".into(),
                forced: false,
                hearing_impaired: false,
                is_image_based: image,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn file_and_stream_argv() {
        assert_eq!(
            to_srt_argv("/a/movie.ass", None),
            vec!["-y", "-i", "/a/movie.ass", "-f", "srt", "pipe:1"]
        );
        assert_eq!(
            to_srt_argv("/a/movie.mkv", Some(4)),
            vec![
                "-y",
                "-i",
                "/a/movie.mkv",
                "-map",
                "0:4",
                "-f",
                "srt",
                "pipe:1"
            ]
        );
    }

    #[test]
    fn relative_file_and_image_stream_fail() {
        let rel = validate_srt(
            &MediaInfo::default(),
            &SrtSource::File {
                path: "a.ass".into(),
            },
        );
        assert!(rel.unwrap_err().contains("absolute"));
        let image = validate_srt(&text_track(2, true), &SrtSource::Stream { index: 2 });
        assert_eq!(
            image.unwrap_err(),
            "SubtitleToSrt stream must be a text track"
        );
    }
}
