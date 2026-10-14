//! Source facts: the bitrates, codecs and container the decision reads, echoed in the answer.
use crate::api::{DecideRequest, HdrFormat, MediaInfo, PlaybackSource, StreamingConfig};
use crate::ladder::resolve_source_video_bitrate_bps;
use std::path::Path;

impl HdrFormat {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Hdr10 => "HDR10",
            Self::Hlg => "HLG",
        }
    }
}

const MPEG_TS_EXTENSIONS: [&str; 6] = ["ts", "m2ts", "mts", "m2t", "tp", "trp"];

/// Whether the probed file is MPEG-TS: its format name, or the extension for a row without one.
pub fn is_mpeg_ts(info: &MediaInfo, path: &str) -> bool {
    match info.format_name.as_deref().filter(|n| !n.is_empty()) {
        Some(name) => name.split(',').any(|f| f == "mpegts"),
        None => Path::new(path).extension().is_some_and(|e| {
            MPEG_TS_EXTENSIONS.contains(&e.to_string_lossy().to_lowercase().as_str())
        }),
    }
}

pub fn build(req: &DecideRequest, config: &StreamingConfig, picked_audio: usize) -> PlaybackSource {
    let info = &req.info;
    let v = info.video.first();
    let a = info.audio.get(picked_audio);

    let format_bit_rate = info.format_bit_rate.filter(|b| *b > 0);
    let audio_sum: f64 = info
        .audio
        .iter()
        .map(|a| f64::from(a.bit_rate.unwrap_or(0)))
        .sum();
    let video_bit_rate = resolve_source_video_bitrate_bps(
        v.and_then(|v| v.bit_rate).map(f64::from),
        format_bit_rate.map(f64::from),
        audio_sum,
    );
    let mut audio_bit_rate = a.and_then(|a| a.bit_rate).map(f64::from);
    if let (None, Some(format), Some(video)) = (audio_bit_rate, format_bit_rate, video_bit_rate) {
        let leftover = f64::from(format) - video;
        if leftover > 1_000.0 {
            audio_bit_rate = Some(leftover);
        }
    }

    PlaybackSource {
        container: req.media.ext.replacen('.', "", 1).to_lowercase(),
        video_codec: v.map_or(String::new(), |v| v.codec.to_lowercase()),
        video_profile: v.and_then(|v| v.profile.as_ref()).map(|p| p.to_lowercase()),
        video_level: v.and_then(|v| v.level),
        video_bit_rate: video_bit_rate.map(|b| b as u32),
        format_bit_rate,
        video_bit_depth: v.and_then(|v| v.bit_depth),
        width: v.and_then(|v| v.width),
        height: v.and_then(|v| v.height),
        frame_rate: v.and_then(|v| v.frame_rate.clone()),
        audio_codec: a.map_or(String::new(), |a| a.codec.to_lowercase()),
        audio_channels: a.and_then(|a| a.channels),
        audio_channel_layout: a.and_then(|a| a.channel_layout.clone()),
        audio_bit_rate: audio_bit_rate.map(|b| b as u32),
        audio_sample_rate: a.and_then(|a| a.sample_rate),
        audio_language: a.map(|a| a.language.clone()),
        duration_seconds: info.duration_seconds,
        hdr_format: v.and_then(|v| v.hdr_format),
        color_space: v.and_then(|v| v.color_space.clone()),
        color_transfer: v.and_then(|v| v.color_transfer.clone()),
        color_primaries: v.and_then(|v| v.color_primaries.clone()),
        // The admin toggle gates both the decision and the crop shown in the answer.
        crop: v
            .and_then(|v| v.crop.clone())
            .filter(|_| config.auto_crop_enabled),
    }
}
