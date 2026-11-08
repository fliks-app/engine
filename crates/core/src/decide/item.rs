//! The decision of an audio item: its parts play as they are (Direct Play, each part by byte
//! range), are copied into HLS, or are all encoded to one AAC. Spec: `docs/spec/item.md`.
use super::audio::{
    AudioContext, REMUX_STEREO_BPS, TrackDecision, audio_channel_cap, audio_reason_message,
    decide_audio,
};
use super::{reason, source, track_dto};
use crate::api::{
    AudioPlan, AudioStreamInfo, DecideRequest, Decision, HdrFormat, HwAccel, ItemPart, MuxFlavour,
    PlayMethod, PlaybackInfo, PlaybackPart, StreamingConfig, TranscodeReason,
};
use crate::item::{audio_mime, parts_of};
use std::path::Path;

fn ext_of(path: &str) -> String {
    Path::new(path)
        .extension()
        .map_or_else(String::new, |e| e.to_string_lossy().to_lowercase())
}

/// A part's MIME type: its probed container when known, else its extension.
fn mime_of(p: &ItemPart) -> &'static str {
    let ext = ext_of(&p.path);
    let named = p
        .info
        .format_name
        .iter()
        .flat_map(|f| f.split(','))
        .find_map(audio_mime);
    named
        .or_else(|| audio_mime(&ext))
        .unwrap_or(if ext == "mp4" {
            "audio/mp4"
        } else {
            "application/octet-stream"
        })
}

/// The part's picked audio track: the request's index when the part has it, else the first.
fn picked(p: &ItemPart, index: Option<i32>) -> AudioStreamInfo {
    let at = index
        .filter(|i| *i >= 0)
        .and_then(|i| p.info.audio.get(i as usize))
        .or_else(|| p.info.audio.first());
    at.cloned().unwrap_or_default()
}

/// Names a container goes by: the extension and each of ffprobe's format names.
fn containers(p: &ItemPart) -> Vec<String> {
    let mut names = vec![ext_of(&p.path)];
    names.extend(
        p.info
            .format_name
            .iter()
            .flat_map(|f| f.split(',').map(str::to_lowercase)),
    );
    names
}

fn whole_file(p: &ItemPart) -> bool {
    let end = p.end_s.zip(p.info.duration_seconds);
    p.start_s.is_none_or(|s| s <= 0.0) && end.is_none_or(|(e, d)| e >= d - 0.05)
}

fn same_format(a: &AudioStreamInfo, b: &AudioStreamInfo) -> bool {
    a.codec.eq_ignore_ascii_case(&b.codec)
        && a.channels == b.channels
        && a.sample_rate == b.sample_rate
        && a.profile == b.profile
}

fn flag_reason(flag: &str, message: &str, out: &mut Vec<TranscodeReason>) {
    if !out.iter().any(|r| r.flag == flag) {
        out.push(reason(flag, message));
    }
}

pub fn decide(req: &DecideRequest, config: &StreamingConfig) -> Decision {
    let (device, sel) = (&req.device, &req.selection);
    let parts = parts_of(req);
    let streams: Vec<AudioStreamInfo> = parts
        .iter()
        .map(|p| picked(p, sel.audio_stream_index))
        .collect();
    // An item has one audio track.
    let mux = if device.use_ts == Some(true) || device.use_ts_on_single_audio == Some(true) {
        MuxFlavour::Ts
    } else {
        MuxFlavour::Fmp4
    };
    let id = req.media.media_file_id;
    let token = sel.token_param.as_deref().unwrap_or("");
    let rejects_copy = device.reject_copy == Some(true);
    let pitch = crate::item::pitch_shift(sel);
    let mut reasons: Vec<TranscodeReason> = vec![];

    let profiles = &device.direct_play_profiles;
    let part_plays = |(p, a): (&ItemPart, &AudioStreamInfo)| {
        let codec = a.codec.to_lowercase();
        let names = containers(p);
        profiles.iter().any(|pr| {
            pr.audio_codecs.contains(&codec) && pr.containers.iter().any(|c| names.contains(c))
        }) && a
            .channels
            .is_none_or(|c| f64::from(c) <= audio_channel_cap(device, &codec))
    };
    let plays = parts.iter().zip(&streams).all(part_plays);
    if device.supports_direct_play == Some(false) {
        flag_reason(
            "ClientRequiresHls",
            "Client requires an HLS container (no raw direct play)",
            &mut reasons,
        );
    }
    if pitch != 0.0 {
        flag_reason(
            "PitchShifted",
            &format!("Pitch shifted by {pitch} semitones"),
            &mut reasons,
        );
    }
    if rejects_copy {
        flag_reason(
            "ClientRejectedCopy",
            "Client asked for a transcode instead of a copied stream",
            &mut reasons,
        );
    }
    if !parts.iter().all(whole_file) {
        flag_reason(
            "ItemPartTrimmed",
            "A part plays a slice of its file, which a byte range cannot cut",
            &mut reasons,
        );
    }
    if let Some(p) = parts.iter().find(|p| {
        let names = containers(p);
        !profiles
            .iter()
            .any(|pr| pr.containers.iter().any(|c| names.contains(c)))
    }) {
        flag_reason(
            "ContainerNotSupported",
            &format!("Container \"{}\" not supported", ext_of(&p.path)),
            &mut reasons,
        );
    }
    let direct = reasons.is_empty() && plays && !parts.is_empty();

    let ctx = AudioContext {
        video: None,
        mux,
        mpeg_ts: false,
        scan: None,
        last_segment_start: None,
    };
    let decisions = decide_audio(&streams, device, &ctx);
    let uniform = streams.windows(2).all(|w| same_format(&w[0], &w[1]));
    let all_copy = decisions
        .iter()
        .all(|d| matches!(d.plan, AudioPlan::Copy { .. }));
    let copy = all_copy && uniform && !rejects_copy && pitch == 0.0 && !streams.is_empty();
    let as_is = |a: &AudioStreamInfo| AudioPlan::Copy {
        codec: a.codec.to_lowercase(),
        channels: a.channels,
        bitrate_bps: a.bit_rate,
        profile: a.profile.clone(),
    };
    let plan = if direct {
        streams
            .first()
            .map_or_else(|| as_is(&Default::default()), as_is)
    } else if copy {
        decisions[0].plan.clone()
    } else {
        // One output format for the item: every part is encoded to the group codec, at the
        // widest layout any part keeps.
        let codec = decisions
            .iter()
            .find_map(|d| match &d.plan {
                AudioPlan::Transcode { codec, .. } => Some(*codec),
                AudioPlan::Copy { .. } => None,
            })
            .unwrap_or(crate::api::AudioCodec::Aac);
        let channels = decisions
            .iter()
            .zip(&streams)
            .map(|(d, a)| match &d.plan {
                AudioPlan::Transcode { channels, .. } => *channels,
                AudioPlan::Copy { .. } => a.channels.unwrap_or(2),
            })
            .max()
            .unwrap_or(2);
        AudioPlan::Transcode { codec, channels }
    };
    if !direct {
        for d in &decisions {
            for f in &d.reason_flags {
                let a = &streams[0];
                flag_reason(
                    f,
                    &audio_reason_message(f, a.channels, None, &a.codec.to_lowercase()),
                    &mut reasons,
                );
            }
        }
        if all_copy && !uniform && !rejects_copy {
            flag_reason(
                "ItemFormatsDiffer",
                "Parts differ in audio format and are encoded to one",
                &mut reasons,
            );
        }
    }

    let track = decisions.first().map(|d| {
        let own = TrackDecision {
            plan: plan.clone(),
            reason_flags: if direct {
                vec![]
            } else {
                d.reason_flags.clone()
            },
        };
        track_dto(&streams[0], 0, &own, REMUX_STEREO_BPS)
    });
    let mut source = source::build(req, config, 0);
    source.duration_seconds = Some(parts.iter().map(ItemPart::duration_s).sum());
    let (method, copy_out) = match (direct, copy) {
        (true, _) => (PlayMethod::DirectPlay, true),
        (false, true) => (PlayMethod::DirectStream, true),
        (false, false) => (PlayMethod::Transcode, false),
    };
    let mut offset = 0.0;
    let listed: Vec<PlaybackPart> = parts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let part = PlaybackPart {
                index: i as u32,
                play_url: format!("/api/stream/{id}/parts/{i}{token}"),
                offset_s: offset,
                duration_s: p.duration_s(),
                content_type: mime_of(p).into(),
            };
            offset += part.duration_s;
            part
        })
        .collect();
    let (play_url, content_type, container) = if direct {
        (
            listed
                .first()
                .map(|p| p.play_url.clone())
                .unwrap_or_default(),
            req.media.content_type.clone(),
            source.container.clone(),
        )
    } else {
        (
            format!("/api/stream/{id}/item/index.m3u8{token}"),
            "application/vnd.apple.mpegurl".into(),
            "hls".into(),
        )
    };
    let output_audio_codec = match &plan {
        AudioPlan::Copy { codec, .. } => codec.clone(),
        AudioPlan::Transcode { codec, .. } => codec.name().into(),
    };
    Decision {
        response: PlaybackInfo {
            media_file_id: id,
            play_method: method,
            play_url,
            content_type,
            transcode_reasons: if direct { vec![] } else { reasons },
            video_copy_stream: false,
            audio_copy_stream: copy_out,
            output_video_codec: String::new(),
            output_audio_codec,
            audio_plan: plan.clone(),
            output_container: container,
            quality: if method == PlayMethod::Transcode {
                "auto"
            } else {
                "original"
            }
            .into(),
            hw_accel: HwAccel::None,
            tonemapping: false,
            client_tonemap: false,
            dolby_vision: false,
            remux_master_bandwidth_bps: None,
            transcode_bitrate_by_quality: None,
            qualities: vec![],
            audio_tracks: track.into_iter().collect(),
            source,
            parts: listed,
        },
        use_hdr_ladder: false,
        video_variant: crate::api::CodecVariant {
            codec: crate::api::VideoCodec::H264,
            bit_depth: 8,
            hdr: None::<HdrFormat>,
        },
        mux_flavour: mux,
        audio_plans: vec![plan],
        transcode_tonemapping: false,
    }
}
