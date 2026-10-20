//! Subtitle sync argv, the ffsubsync retry, and alass reference resolution.
//! Spec: `docs/spec/subtitle-jobs.md` section 3.
use crate::api::{MediaInfo, SubtitleFormat, SubtitleText, SyncReference};
use crate::jsnum::js_string;
use crate::strs;

pub const PCM_TIMEOUT_MS: u64 = 900_000;
pub const MAX_STREAM_INDEX: u32 = 999;

const SUB_EXTS: &[&str] = &["srt", "sub", "ass", "ssa", "vtt", "idx"];

pub fn format_ext(f: SubtitleFormat) -> &'static str {
    match f {
        SubtitleFormat::Srt => "srt",
        SubtitleFormat::Sub => "sub",
        SubtitleFormat::Ass => "ass",
        SubtitleFormat::Ssa => "ssa",
        SubtitleFormat::Vtt => "vtt",
    }
}

fn path_ext(path: &str) -> Option<String> {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// The text format a subtitle path's extension names (`idx` is binary: none).
pub fn format_of_path(path: &str) -> Option<SubtitleFormat> {
    Some(match path_ext(path)?.as_str() {
        "srt" => SubtitleFormat::Srt,
        "sub" => SubtitleFormat::Sub,
        "ass" => SubtitleFormat::Ass,
        "ssa" => SubtitleFormat::Ssa,
        "vtt" => SubtitleFormat::Vtt,
        _ => return None,
    })
}

pub fn is_subtitle_path(path: &str) -> bool {
    path_ext(path).is_some_and(|e| SUB_EXTS.contains(&e.as_str()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioMap {
    StreamIndex(u32),
    FirstAudio,
}

impl AudioMap {
    pub fn map_arg(&self) -> String {
        match self {
            AudioMap::StreamIndex(i) => format!("0:{i}"),
            AudioMap::FirstAudio => "0:a:0".into(),
        }
    }
}

/// Resolved reference for the alass leg (no I/O).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedReference {
    Pcm {
        path: String,
        map: AudioMap,
    },
    SubtitleFile {
        path: String,
    },
    ExtractSubtitleStream {
        media_path: String,
        stream_index: u32,
    },
}

pub fn validate_sync(
    info: &MediaInfo,
    subtitle: &SubtitleText,
    reference: &SyncReference,
    max_offset_seconds: Option<f64>,
) -> Result<(), String> {
    if subtitle.text.trim().is_empty() {
        return Err("SubtitleSync subtitle text must be non-empty".into());
    }
    if let Some(n) = max_offset_seconds
        && (!n.is_finite() || n < 0.0)
    {
        return Err("maxOffsetSeconds must be finite and >= 0".into());
    }
    resolve_reference(info, reference, "")?;
    Ok(())
}

pub fn resolve_reference(
    info: &MediaInfo,
    reference: &SyncReference,
    media_path: &str,
) -> Result<ResolvedReference, String> {
    match reference {
        SyncReference::Auto => {
            let audio = info
                .audio
                .first()
                .ok_or_else(|| "no audio stream to sync against".to_string())?;
            Ok(ResolvedReference::Pcm {
                path: media_path.into(),
                map: AudioMap::StreamIndex(audio.stream_index),
            })
        }
        SyncReference::AudioStream { index } => {
            if *index > MAX_STREAM_INDEX {
                return Err(format!("Invalid stream index: above {MAX_STREAM_INDEX}"));
            }
            let ok = info.audio.iter().any(|a| a.stream_index == *index);
            if !ok {
                return Err("SubtitleSync audioStream index must be in info.audio".into());
            }
            Ok(ResolvedReference::Pcm {
                path: media_path.into(),
                map: AudioMap::StreamIndex(*index),
            })
        }
        SyncReference::SubtitleStream { index } => {
            if *index > MAX_STREAM_INDEX {
                return Err(format!("Invalid stream index: above {MAX_STREAM_INDEX}"));
            }
            let track = info
                .subtitles
                .iter()
                .find(|s| s.stream_index == *index)
                .ok_or_else(|| {
                    "SubtitleSync subtitleStream index must be in info.subtitles".to_string()
                })?;
            if track.is_image_based {
                return Err("subtitle stream reference must be a text track".into());
            }
            Ok(ResolvedReference::ExtractSubtitleStream {
                media_path: media_path.into(),
                stream_index: *index,
            })
        }
        SyncReference::File { path } => {
            if !std::path::Path::new(path).is_absolute() {
                return Err("SubtitleSync reference file path must be absolute".into());
            }
            if is_subtitle_path(path) {
                Ok(ResolvedReference::SubtitleFile { path: path.clone() })
            } else {
                Ok(ResolvedReference::Pcm {
                    path: path.clone(),
                    map: AudioMap::FirstAudio,
                })
            }
        }
    }
}

pub fn pcm_extract_argv(path: &str, map: &str, sample_rate: u32) -> Vec<String> {
    let mut a = strs(&["-nostdin", "-i"]);
    a.push(path.into());
    a.push("-map".into());
    a.push(map.into());
    a.extend(strs(&["-ac", "1"]));
    a.push("-ar".into());
    a.push(sample_rate.to_string());
    a.extend(strs(&["-f", "s16le", "-c:a", "pcm_s16le", "pipe:1"]));
    a
}

pub fn ffsubsync_argv(
    reference: &str,
    input: &str,
    output: &str,
    stream: Option<u32>,
    max_offset_seconds: Option<f64>,
    no_fix_framerate: bool,
    gss: bool,
) -> Vec<String> {
    let mut a = vec![
        reference.into(),
        "-i".into(),
        input.into(),
        "-o".into(),
        output.into(),
    ];
    if let Some(index) = stream {
        a.push("--reference-stream".into());
        a.push(format!("stream:{index}"));
    }
    if let Some(n) = max_offset_seconds {
        a.push("--max-offset-seconds".into());
        a.push(js_string(n));
    }
    if no_fix_framerate {
        a.push("--no-fix-framerate".into());
    }
    if gss {
        a.push("--gss".into());
    }
    a
}

/// First audio stream to retry with after `unable to detect speech` on an `auto` reference.
pub fn speech_retry_stream(
    reference: &SyncReference,
    stderr: &str,
    first_audio: Option<u32>,
) -> Option<u32> {
    if !matches!(reference, SyncReference::Auto) {
        return None;
    }
    if !stderr
        .to_ascii_lowercase()
        .contains("unable to detect speech")
    {
        return None;
    }
    first_audio
}

pub fn subtitle_stream_extract_argv(path: &str, stream_index: u32, out_srt: &str) -> Vec<String> {
    let mut a = strs(&["-y", "-i"]);
    a.push(path.into());
    a.push("-map".into());
    a.push(format!("0:{stream_index}"));
    a.extend(strs(&["-c:s", "srt"]));
    a.push(out_srt.into());
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{AudioStreamInfo, SubtitleStreamInfo, SyncReference};

    fn info_audio(idx: u32) -> MediaInfo {
        MediaInfo {
            audio: vec![AudioStreamInfo {
                stream_index: idx,
                codec: "aac".into(),
                language: "eng".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn info_text_sub(idx: u32, image: bool) -> MediaInfo {
        MediaInfo {
            subtitles: vec![SubtitleStreamInfo {
                stream_index: idx,
                codec: if image {
                    "hdmv_pgs_subtitle".into()
                } else {
                    "subrip".into()
                },
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
    fn pcm_argv_matches_stream_map_and_rate() {
        let a = pcm_extract_argv("/m.mkv", "0:1", 8000);
        assert_eq!(
            a,
            vec![
                "-nostdin",
                "-i",
                "/m.mkv",
                "-map",
                "0:1",
                "-ac",
                "1",
                "-ar",
                "8000",
                "-f",
                "s16le",
                "-c:a",
                "pcm_s16le",
                "pipe:1"
            ]
        );
    }

    #[test]
    fn auto_without_audio_fails() {
        let e =
            resolve_reference(&MediaInfo::default(), &SyncReference::Auto, "/m.mkv").unwrap_err();
        assert_eq!(e, "no audio stream to sync against");
    }

    #[test]
    fn relative_reference_file_fails() {
        let e = resolve_reference(
            &info_audio(1),
            &SyncReference::File {
                path: "rel.srt".into(),
            },
            "/m.mkv",
        )
        .unwrap_err();
        assert!(e.contains("absolute"));
    }

    #[test]
    fn image_subtitle_stream_fails() {
        let e = resolve_reference(
            &info_text_sub(2, true),
            &SyncReference::SubtitleStream { index: 2 },
            "/m.mkv",
        )
        .unwrap_err();
        assert_eq!(e, "subtitle stream reference must be a text track");
    }

    #[test]
    fn subtitle_file_reference_uses_cues() {
        // A leading slash is not absolute on Windows.
        let path = if cfg!(windows) {
            r"C:\refs\a.SRT".to_string()
        } else {
            "/refs/a.SRT".to_string()
        };
        let r = resolve_reference(
            &MediaInfo::default(),
            &SyncReference::File { path: path.clone() },
            "/m.mkv",
        )
        .unwrap();
        assert_eq!(r, ResolvedReference::SubtitleFile { path });
    }

    #[test]
    fn speech_retry_only_for_auto() {
        let auto = speech_retry_stream(
            &SyncReference::Auto,
            "Unable to detect speech in the reference",
            Some(2),
        );
        assert_eq!(auto, Some(2));
        let named = speech_retry_stream(
            &SyncReference::AudioStream { index: 1 },
            "unable to detect speech",
            Some(2),
        );
        assert_eq!(named, None);
        let quiet = speech_retry_stream(&SyncReference::Auto, "parse error", Some(2));
        assert_eq!(quiet, None);
    }

    #[test]
    fn stream_index_above_999_fails() {
        let e = resolve_reference(
            &info_audio(1),
            &SyncReference::AudioStream { index: 1000 },
            "/m.mkv",
        )
        .unwrap_err();
        assert!(e.contains("999"));
    }
}
