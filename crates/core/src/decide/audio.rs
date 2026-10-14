//! Audio plan: one output codec per group of renditions, copy or encode per track, and bitrates.
use super::timeline::video_presentation_start;
use crate::api::{
    AudioCodec, AudioPlan, AudioStreamInfo, DeviceProfile, MuxFlavour, SourceScan, VideoStreamInfo,
};
use crate::ladder::{SDR_LADDER, js_round};

/// Above the start offset encoder priming alone reports.
const START_OFFSET_THRESHOLD_SECONDS: f64 = 0.05;
/// Every encode runs at 48 kHz.
pub const ENCODE_SAMPLE_RATE: u32 = 48_000;
const DOLBY_MAX_BITRATE_BPS: f64 = 640_000.0;
const AC3_BITRATES_KBPS: [u32; 19] = [
    32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640,
];

impl AudioCodec {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "aac" => Some(Self::Aac),
            "ac3" => Some(Self::Ac3),
            "eac3" => Some(Self::Eac3),
            "opus" => Some(Self::Opus),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Aac => "aac",
            Self::Ac3 => "ac3",
            Self::Eac3 => "eac3",
            Self::Opus => "opus",
        }
    }

    fn encoder_max_channels(self) -> f64 {
        match self {
            Self::Aac | Self::Opus => 8.0,
            Self::Ac3 | Self::Eac3 => 6.0,
        }
    }
}

/// Codecs that can be copied verbatim into fMP4 segments via MSE.
fn fmp4_compatible(codec: &str) -> bool {
    matches!(codec, "aac" | "ac3" | "eac3" | "opus" | "flac")
}

pub fn lower_codec(a: &AudioStreamInfo) -> String {
    a.codec.to_lowercase()
}

/// Max channels the device decodes for `codec`: per-codec cap, else global, else stereo.
pub fn audio_channel_cap(d: &DeviceProfile, codec: &str) -> f64 {
    d.audio_channels_by_codec
        .as_ref()
        .and_then(|m| m.get(&codec.to_lowercase()).copied())
        .or(d.max_audio_channels)
        .unwrap_or(2.0)
}

/// Channels an encode to `codec` can deliver: what both the device and the encoder take.
fn encode_channel_cap(d: &DeviceProfile, codec: AudioCodec) -> f64 {
    audio_channel_cap(d, codec.name()).min(codec.encoder_max_channels())
}

/// A rung's audio bitrate is a stereo budget, so each further channel pair gets as much again; a
/// Dolby encode stays in Dolby's range, on a rate AC-3 can carry.
fn encode_bitrate_bps(codec: AudioCodec, channels: f64, stereo_bps: f64) -> f64 {
    let ch = channels.max(2.0);
    let budget = js_round(stereo_bps * ch / 2.0);
    if !matches!(codec, AudioCodec::Ac3 | AudioCodec::Eac3) {
        return budget;
    }
    let ac3_floor = 192_000.0 + 48_000.0 * (ch - 2.0);
    let floor = if codec == AudioCodec::Ac3 {
        ac3_floor
    } else {
        js_round(ac3_floor * 2.0 / 3.0)
    };
    let bps = DOLBY_MAX_BITRATE_BPS.min(floor.max(budget));
    if codec == AudioCodec::Eac3 {
        return bps;
    }
    AC3_BITRATES_KBPS
        .iter()
        .map(|k| f64::from(*k) * 1000.0)
        .find(|r| *r >= bps)
        .unwrap_or(DOLBY_MAX_BITRATE_BPS)
}

/// Bitrate an output streams at on a rung of `stereo_bps`: a copy's source bitrate, else what an
/// encode of that layout would spend.
pub fn output_bitrate_bps(plan: &AudioPlan, stereo_bps: f64) -> f64 {
    match plan {
        AudioPlan::Copy {
            bitrate_bps: Some(b),
            ..
        } if *b != 0 => f64::from(*b),
        AudioPlan::Copy {
            codec, channels, ..
        } => encode_bitrate_bps(
            AudioCodec::parse(codec).unwrap_or(AudioCodec::Aac),
            f64::from(channels.unwrap_or(2)),
            stereo_bps,
        ),
        AudioPlan::Transcode { codec, channels } => {
            encode_bitrate_bps(*codec, f64::from(*channels), stereo_bps)
        }
    }
}

/// One track's decision: its output, and why it re-encodes (`Audio*` flags, empty when copied).
pub struct TrackDecision {
    pub plan: AudioPlan,
    pub reason_flags: Vec<&'static str>,
}

/// What of the source and layout the copy blockers read.
pub struct AudioContext<'a> {
    pub video: Option<&'a VideoStreamInfo>,
    pub mux: MuxFlavour,
    pub mpeg_ts: bool,
    pub scan: Option<&'a SourceScan>,
    /// Where the last video segment starts, for separate renditions only.
    pub last_segment_start: Option<f64>,
}

/// Why a track the device plays as-is must still be re-encoded into fMP4.
fn copy_blocker(a: &AudioStreamInfo, c: &AudioContext) -> Option<&'static str> {
    if c.mux != MuxFlavour::Fmp4 {
        return None;
    }
    // ADTS restates the AAC configuration per frame and broadcasts switch it mid-stream; fMP4
    // keeps the first. Only a scan that saw no change lets it copy.
    let may_change = c.scan.is_none_or(|s| {
        s.audio_config_changes
            .get(&a.stream_index)
            .copied()
            .unwrap_or(false)
    });
    if a.codec.to_lowercase() == "aac" && c.mpeg_ts && may_change {
        return Some("AudioFormatMayChange");
    }
    if let (Some(at), Some(vt)) = (a.start_time_seconds, video_presentation_start(c.video))
        && (at - vt).abs() > START_OFFSET_THRESHOLD_SECONDS
    {
        return Some("AudioStartOffset");
    }
    // A separate rendition would lack the segments its playlist lists past its end.
    match (a.end_seconds, c.last_segment_start) {
        (Some(end), Some(last)) if end <= last => Some("AudioEndsEarly"),
        _ => None,
    }
}

/// The one output codec shared by every rendition of the group (HLS requires it): the source codec
/// when all tracks share a playable one, else a surround codec for surround sources, else AAC.
fn pick_group_codec(
    tracks: &[AudioStreamInfo],
    d: &DeviceProfile,
    profile_codecs: &[String],
    blockers: &[Option<&str>],
) -> String {
    let codecs: Vec<String> = tracks.iter().map(lower_codec).collect();
    let fits = |t: &AudioStreamInfo, c: &str| {
        t.channels
            .is_none_or(|n| f64::from(n) <= audio_channel_cap(d, c))
    };
    if let Some(c) = codecs.first().filter(|c| codecs.iter().all(|x| x == *c)) {
        let supported = profile_codecs.contains(c) && fmp4_compatible(c);
        let all_copy = tracks.iter().all(|t| fits(t, c)) && blockers.iter().all(Option::is_none);
        if supported && (all_copy || AudioCodec::parse(c).is_some()) {
            return c.clone();
        }
    }
    let any_surround = tracks.iter().any(|t| t.channels.unwrap_or(0) >= 6);
    let surround: Vec<AudioCodec> = [AudioCodec::Eac3, AudioCodec::Ac3]
        .into_iter()
        .filter(|c| {
            profile_codecs.iter().any(|p| p == c.name()) && encode_channel_cap(d, *c) >= 6.0
        })
        .collect();
    // One a track already is lets that track copy instead of re-encoding.
    let copies = |c: &AudioCodec| {
        tracks
            .iter()
            .enumerate()
            .any(|(i, t)| codecs[i] == c.name() && blockers[i].is_none() && fits(t, c.name()))
    };
    let group = surround.iter().find(|c| copies(c)).or(surround.first());
    match group {
        Some(c) if any_surround => c.name().to_string(),
        _ => "aac".to_string(),
    }
}

/// Copy/transcode decision per track; all tracks share one audio group.
pub fn decide_audio(
    tracks: &[AudioStreamInfo],
    d: &DeviceProfile,
    ctx: &AudioContext,
) -> Vec<TrackDecision> {
    let blockers: Vec<Option<&'static str>> = tracks.iter().map(|t| copy_blocker(t, ctx)).collect();
    let profile_codecs: Vec<String> = d
        .direct_play_profiles
        .iter()
        .flat_map(|p| p.audio_codecs.iter().map(|c| c.to_lowercase()))
        .collect();
    let group = pick_group_codec(tracks, d, &profile_codecs, &blockers);

    tracks
        .iter()
        .zip(&blockers)
        .map(|(t, blocker)| {
            let codec = lower_codec(t);
            let codec_supported = profile_codecs.contains(&codec);
            let fmp4_safe = fmp4_compatible(&codec);
            let exceeds = t
                .channels
                .is_some_and(|n| f64::from(n) > audio_channel_cap(d, &codec));
            if codec == group && codec_supported && fmp4_safe && !exceeds && blocker.is_none() {
                return TrackDecision {
                    plan: AudioPlan::Copy {
                        codec: group.clone(),
                        channels: t.channels,
                        bitrate_bps: t.bit_rate,
                        profile: t.profile.clone(),
                    },
                    reason_flags: vec![],
                };
            }
            // Only an all-copy group settles on a codec that cannot be encoded.
            let enc = AudioCodec::parse(&group).unwrap_or(AudioCodec::Aac);
            let output_channels =
                f64::from(t.channels.unwrap_or(2)).min(encode_channel_cap(d, enc));
            let downmixed = t.channels.is_some_and(|n| output_channels < f64::from(n));
            let surround_preserved = enc != AudioCodec::Aac && output_channels >= 6.0;
            // A track plays as-is only if the device decodes it AND it is fMP4-safe; a codec
            // re-encoded just to match the group is no reason.
            let playable_as_is = codec_supported && fmp4_safe;
            let mut reason_flags = vec![];
            if downmixed {
                reason_flags.push("AudioChannelsNotSupported");
            }
            if !playable_as_is && !surround_preserved {
                reason_flags.push("AudioCodecNotSupported");
            }
            reason_flags.extend(*blocker);
            TrackDecision {
                plan: AudioPlan::Transcode {
                    codec: enc,
                    channels: output_channels as u32,
                },
                reason_flags,
            }
        })
        .collect()
}

/// Stereo budget of the remux encoder: the top desktop rung's audio.
pub const REMUX_STEREO_BPS: f64 = SDR_LADDER[0].audio_bps;

/// Fixed texts of the per-track flags; the channel message quotes the track.
pub fn audio_reason_message(
    flag: &str,
    channels: Option<u32>,
    output_channels: Option<u32>,
    codec: &str,
) -> String {
    let num = |n: Option<u32>| n.map_or("undefined".into(), |n| n.to_string());
    match flag {
        "AudioChannelsNotSupported" => {
            format!("{} channels > max {}", num(channels), num(output_channels))
        }
        "AudioCodecNotSupported" => format!("Audio codec \"{codec}\" is not playable here"),
        "AudioStartOffset" => "Audio starts off the video and is re-encoded to align it".into(),
        "AudioFormatMayChange" => {
            "AAC from MPEG-TS may change format mid-stream and is re-encoded".into()
        }
        "AudioEndsEarly" => "Audio ends before the video and is padded to its end".into(),
        other => other.into(),
    }
}
