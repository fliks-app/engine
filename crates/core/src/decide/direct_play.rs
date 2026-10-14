//! First pass of the decision: can the file play as-is on this device, and why not.
use super::audio::audio_channel_cap;
use crate::api::{DeviceProfile, PlaybackSource, TranscodeReason};
use crate::ladder::resolution_fits_cap;

pub struct DirectPlay {
    pub can_direct_play: bool,
    pub container_supported: bool,
    pub video_supported: bool,
    pub video_conditions_met: bool,
}

fn reason(flag: &str, message: String) -> TranscodeReason {
    TranscodeReason {
        flag: flag.into(),
        message,
    }
}

/// JS template-literal rendering of a number that may be undefined.
fn js(n: Option<u32>) -> String {
    n.map_or("undefined".into(), |n| n.to_string())
}

/// Matches the source against the device profile and records the reasons it does not play as-is,
/// in the host's order. `has_audio` is false when the file has no audio track.
pub fn try_direct_play(
    source: &PlaybackSource,
    d: &DeviceProfile,
    reasons: &mut Vec<TranscodeReason>,
    has_audio: bool,
) -> DirectPlay {
    let profiles = &d.direct_play_profiles;
    let has = |list: &Vec<String>, x: &String| list.contains(x);
    let container_supported = profiles
        .iter()
        .any(|p| has(&p.containers, &source.container));
    let video_supported = profiles
        .iter()
        .any(|p| has(&p.video_codecs, &source.video_codec));
    let mut audio_supported = !has_audio
        || profiles
            .iter()
            .any(|p| has(&p.audio_codecs, &source.audio_codec));

    // One profile entry must bind container, video and audio: pieces of different entries are a
    // combination the device never claimed.
    let bound_match = profiles.iter().any(|p| {
        has(&p.containers, &source.container)
            && has(&p.video_codecs, &source.video_codec)
            && (!has_audio || has(&p.audio_codecs, &source.audio_codec))
    });

    let mut conditions_met = true;
    let cond = d
        .codec_conditions
        .iter()
        .flatten()
        .find(|c| c.codec == source.video_codec);
    if let (true, Some(cond)) = (video_supported, cond) {
        let truthy = |n: Option<f64>| n.filter(|n| *n != 0.0);
        if let (Some(max), Some(level)) = (
            truthy(cond.max_level),
            source.video_level.filter(|l| *l != 0),
        ) && f64::from(level) > max
        {
            conditions_met = false;
            reasons.push(reason(
                "VideoLevelNotSupported",
                format!("Level {level} > max {max}"),
            ));
        }
        if let (Some(profiles), Some(profile)) = (
            cond.profiles.as_ref().filter(|p| !p.is_empty()),
            source.video_profile.as_ref().filter(|p| !p.is_empty()),
        ) && !profiles.contains(profile)
        {
            conditions_met = false;
            reasons.push(reason(
                "VideoProfileNotSupported",
                format!("Video profile \"{profile}\" not supported"),
            ));
        }
        if let (Some(max), Some(depth)) = (
            truthy(cond.max_bit_depth),
            source.video_bit_depth.filter(|b| *b != 0),
        ) && f64::from(depth) > max
        {
            conditions_met = false;
            reasons.push(reason(
                "VideoBitDepthNotSupported",
                format!("{depth} bit > max {max} bit"),
            ));
        }
        if !resolution_fits_cap(source.width, source.height, cond.max_width, cond.max_height) {
            conditions_met = false;
            let cap = |n: Option<f64>| n.map_or("?".into(), |n| n.to_string());
            reasons.push(reason(
                "VideoResolutionNotSupported",
                format!(
                    "Resolution {}x{} > max {}x{}",
                    js(source.width),
                    js(source.height),
                    cap(cond.max_width),
                    cap(cond.max_height)
                ),
            ));
        }
    }

    if let Some(max) = d.max_streaming_bitrate.filter(|m| *m > 0.0) {
        let total = source.format_bit_rate.map(f64::from).unwrap_or_else(|| {
            f64::from(source.video_bit_rate.unwrap_or(0))
                + f64::from(source.audio_bit_rate.unwrap_or(0))
        });
        if total > max {
            reasons.push(reason(
                "VideoBitrateNotSupported",
                format!("Bitrate too high ({} Mbps)", ((total / 1e6) + 0.5).floor()),
            ));
            conditions_met = false;
        }
    }

    // Against the source codec's own decode cap; the audio reasons come from the per-track plan.
    let cap = audio_channel_cap(d, &source.audio_codec);
    if source
        .audio_channels
        .is_some_and(|c| c != 0 && f64::from(c) > cap)
    {
        audio_supported = false;
    }

    if !video_supported {
        reasons.push(reason(
            "VideoCodecNotSupported",
            format!("Codec \"{}\" not supported", source.video_codec),
        ));
    }
    if !container_supported {
        reasons.push(reason(
            "ContainerNotSupported",
            format!("Container \"{}\" not supported", source.container),
        ));
    }

    DirectPlay {
        can_direct_play: bound_match && audio_supported && conditions_met,
        container_supported,
        video_supported,
        video_conditions_met: conditions_met,
    }
}
