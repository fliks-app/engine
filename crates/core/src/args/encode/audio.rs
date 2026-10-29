//! Audio stream argv: copy, encode and the stream map.

use super::ENCODE_SAMPLE_RATE;
use crate::api::{AudioCodec, AudioPlan};
use crate::decide::audio::output_bitrate_bps;
use crate::jsnum::{js_string as js, round, seconds};

/// What every audio output of one run shares. [ts:backend/src/modules/streaming/transcoding/ffmpeg-args.ts:136-144]
#[derive(Debug, Clone)]
pub struct AudioEnc {
    pub stereo_bps: f64,
    pub align_start: f64,
    pub end: Option<f64>,
    pub use_ts: bool,
    pub libfdk_aac: bool,
}

pub const DEFAULT_AUDIO_PLAN: AudioPlan = AudioPlan::Transcode {
    codec: AudioCodec::Aac,
    channels: 2,
};

/// `-map` of an audio track: its absolute index, else the optional relative one.
pub fn audio_map(streams: Option<&[super::super::AudioStream]>, i: usize) -> String {
    match streams.and_then(|s| s.get(i)).and_then(|s| s.stream_index) {
        Some(abs) => format!("0:{abs}"),
        None => format!("0:a:{i}?"),
    }
}

pub fn video_map(index: Option<u32>) -> String {
    index.map_or("0:v:0".into(), |i| format!("0:{i}"))
}

/// Aligns the audio to the video's first frame and pads it to `end`; `stereo` downmixes.
fn align_filter(start: f64, end: Option<f64>, stereo: bool) -> String {
    let st = seconds(start.abs());
    let (to, back) = if start < 0.0 { ("+", "-") } else { ("-", "+") };
    let pad = match end {
        Some(e) if e > start => format!(",apad=whole_dur={}", seconds(e - start)),
        _ => String::new(),
    };
    let down = if stereo {
        ":rematrix_maxval=3,aformat=channel_layouts=stereo,\
         alimiter=limit=0.977:attack=5:release=50:level=disabled:latency=1"
    } else {
        ""
    };
    format!(
        "asetpts=PTS{to}{st}/TB,aresample=async=1:first_pts=0{down}{pad},asetpts=PTS{back}{st}/TB"
    )
}

/// MP4 carries AAC raw, MPEG-TS as ADTS. [ts:backend/src/modules/streaming/transcoding/audio-encode.ts:217-225]
pub fn audio_copy_args(spec: &str, codec: &str, use_ts: bool) -> Vec<String> {
    let mut a = vec![format!("-c:a{spec}"), "copy".into()];
    if codec == "aac" && !use_ts {
        a.extend([format!("-bsf:a{spec}"), "aac_adtstoasc".into()]);
    }
    a
}

/// [ts:backend/src/modules/streaming/transcoding/audio-encode.ts:62-66,196-213]
pub fn audio_encode_args(
    spec: &str,
    codec: AudioCodec,
    channels: u32,
    bps: f64,
    libfdk: bool,
) -> Vec<String> {
    let encoder = match codec {
        AudioCodec::Aac if libfdk && channels <= 6 => "libfdk_aac",
        AudioCodec::Opus => "libopus",
        c => c.name(),
    };
    // `-ac` / `-ar` carry no stream type, so an indexed one must name the audio.
    let (ac, ar) = if spec.is_empty() {
        ("-ac".to_string(), "-ar".to_string())
    } else {
        (format!("-ac:a{spec}"), format!("-ar:a{spec}"))
    };
    vec![
        format!("-c:a{spec}"),
        encoder.into(),
        format!("-b:a{spec}"),
        format!("{}k", js(round(bps / 1000.0))),
        ac,
        channels.to_string(),
        ar,
        ENCODE_SAMPLE_RATE.into(),
    ]
}

/// One audio output (`spec` `''` or `:<i>`). [ts:backend/src/modules/streaming/transcoding/ffmpeg-args.ts:148-164]
pub fn audio_stream_args(spec: &str, plan: &AudioPlan, a: &AudioEnc) -> Vec<String> {
    match plan {
        AudioPlan::Copy { codec, .. } => audio_copy_args(spec, codec, a.use_ts),
        AudioPlan::Transcode { codec, channels } => {
            let bps = output_bitrate_bps(plan, a.stereo_bps);
            let mut v = audio_encode_args(spec, *codec, *channels, bps, a.libfdk_aac);
            v.push(format!("-filter:a{spec}"));
            v.push(align_filter(a.align_start, a.end, *channels == 2));
            v
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(channels: u32) -> String {
        let a = AudioEnc {
            stereo_bps: 128_000.0,
            align_start: 0.0,
            end: None,
            use_ts: false,
            libfdk_aac: false,
        };
        let plan = AudioPlan::Transcode {
            codec: AudioCodec::Aac,
            channels,
        };
        audio_stream_args("", &plan, &a).pop().unwrap()
    }

    #[test]
    fn only_a_stereo_target_downmixes_unnormalised() {
        let f = filter(2);
        assert!(f.contains(
            "aresample=async=1:first_pts=0:rematrix_maxval=3,aformat=channel_layouts=stereo,\
             alimiter=limit=0.977:attack=5:release=50:level=disabled:latency=1"
        ));
        assert!(!filter(6).contains("rematrix_maxval"));
    }
}
