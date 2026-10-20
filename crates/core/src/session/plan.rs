//! What a session freezes at open and how its routes and runs read it back: the profile hash
//! inputs, the ffmpeg run options and the master options. Spec: `docs/spec/runtime.md`.
use super::{LayoutAudio, SessionLayout, profile_from_layout, profile_hash};
use super::{SessionVariant, remux_variant};
use crate::api::{
    AudioCodec, AudioPlan, BurnInKind, Decision, HdrFormat, HostCaps, HwAccel, MediaInfo,
    MuxFlavour, OpenSessionRequest, PlayMethod, QsvPreset, SessionKind, StreamingConfig,
    SubtitleTrackKind, VideoStreamInfo,
};
use crate::args::{self, AudioStream, BurnIn, Profile, VodArgs};
use crate::args::{RemuxArgs, RemuxRun};
use crate::decide::codec::resolve_encoder;
use crate::decide::dolby_vision::{dv_standalone_codecs, dv_supplemental_codecs, has_no_base};
use crate::decide::source::is_mpeg_ts;
use crate::decide::timeline::{parse_source_fps, real_segment_seconds};
use crate::hls::{
    AudioStreamMeta, HdrPassThrough, MasterOptions, SubtitleKind, SubtitleRendition,
    copy_source_codec_string,
};
use crate::ladder::{
    HDR_LADDER, SDR_LADDER, profile_fits_source, resolve_source_video_bitrate_bps,
};
use crate::mp4::{
    KeyframeGrid, RemuxRunStart, SourceTimeline, remux_segment_grid, source_timeline,
};

/// What every request of a session reads back, decided once at open.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionPlan {
    pub kind: SessionKind,
    /// Cache profile hash (the instance suffix is the caller's); a Direct Play session keeps one
    /// for a client that falls back to HLS, but does not report it.
    pub profile_hash: String,
    pub use_ts: bool,
    /// More than one audio track: var_stream_map, one EXT-X-MEDIA rendition per track.
    pub multi_audio: bool,
    pub source_fps: Option<f64>,
    pub segment_duration: f64,
    pub timeline: SourceTimeline,
    pub duration: f64,
    /// Keyframe grid a DirectStream session's remux runs are cut on, frozen at open.
    pub remux_grid: Option<KeyframeGrid>,
    /// The client fetches seg-0 on a load-then-seek: a resume gets the early companion.
    pub probes_seg_zero: bool,
}

pub fn session_kind(m: PlayMethod) -> SessionKind {
    match m {
        PlayMethod::DirectPlay => SessionKind::Directplay,
        PlayMethod::DirectStream => SessionKind::Remux,
        PlayMethod::Transcode => SessionKind::Transcode,
    }
}

/// The encoder binary an audio plan runs, as the argv builder picks it.
fn audio_encoder_id(codec: AudioCodec, channels: u32, caps: &HostCaps) -> String {
    match codec {
        AudioCodec::Aac if caps.libfdk_aac && channels <= 6 => "libfdk_aac".into(),
        AudioCodec::Opus => "libopus".into(),
        c => c.name().into(),
    }
}

fn layout_audio(p: &AudioPlan, caps: &HostCaps) -> LayoutAudio {
    match p {
        AudioPlan::Copy {
            codec, channels, ..
        } => LayoutAudio {
            codec: codec.clone(),
            channels: channels.unwrap_or(2),
            copy: true,
            encoder_id: "copy".into(),
        },
        AudioPlan::Transcode { codec, channels } => LayoutAudio {
            codec: codec.name().into(),
            channels: *channels,
            copy: false,
            encoder_id: audio_encoder_id(*codec, *channels, caps),
        },
    }
}

/// [ts:modules/streaming/streaming.controller.ts:1083-1124]
pub fn plan(
    req: &OpenSessionRequest,
    d: &Decision,
    streaming: &StreamingConfig,
    caps: &HostCaps,
) -> SessionPlan {
    let info = &req.decide.info;
    let v = info.video.first();
    let timeline = source_timeline(Some(info));
    let use_ts = d.mux_flavour == MuxFlavour::Ts;
    let kind = session_kind(d.response.play_method);
    let layout = SessionLayout {
        use_ts,
        audio_stream_count: info.audio.len() as u32,
        audio_plan: layout_audio(&d.response.audio_plan, caps),
        audio_track_plans: Some(
            d.audio_plans
                .iter()
                .map(|p| layout_audio(p, caps))
                .collect(),
        ),
        video_variant: Some(d.video_variant),
        timeline_origin: timeline.origin,
        timeline_format_start: Some(timeline.format_start),
        source_version: req.source_version.clone(),
        dolby_vision: d.response.dolby_vision,
        tonemapping: d.transcode_tonemapping,
        tonemap_curve: streaming.tonemap_curve,
        source_dv_profile: v.and_then(|v| v.dv_profile),
        source_dv_bl_signal_compat_id: v.and_then(|v| v.dv_bl_signal_compat_id),
        source_hdr10_plus: v.and_then(|v| v.hdr10_plus).unwrap_or(false),
    };
    let ms = (streaming.segment_duration * 1000.0).round() as u32;
    SessionPlan {
        kind,
        profile_hash: profile_hash(&profile_from_layout(&layout, ms)),
        use_ts,
        multi_audio: info.audio.len() > 1,
        source_fps: parse_source_fps(v.and_then(|v| v.frame_rate.as_deref())),
        segment_duration: streaming.segment_duration,
        remux_grid: (kind == SessionKind::Remux)
            .then(|| {
                remux_segment_grid(
                    req.decide.scan.as_ref(),
                    timeline.origin,
                    streaming.segment_duration,
                    v.and_then(|v| v.frame_rate.as_deref()),
                )
            })
            .flatten(),
        probes_seg_zero: req.decide.device.probes_seg_zero.unwrap_or(true),
        timeline,
        duration: info.duration_seconds.unwrap_or(0.0),
    }
}

/// Rung of a quality name: the HDR ladder for a `-hdr` name, the ladder's top when unknown.
/// [ts:modules/streaming/transcoding/transcoding.service.ts:816-819]
pub fn rung_profile(quality: &str) -> Profile {
    let ladder = if quality.ends_with("-hdr") {
        HDR_LADDER
    } else {
        SDR_LADDER
    };
    let r = ladder
        .iter()
        .find(|r| r.name == quality)
        .unwrap_or(&ladder[0]);
    Profile {
        name: r.name.into(),
        max_width: r.max_width,
        max_height: r.max_height,
        video_bitrate: format!("{}", r.video_bps),
        audio_bitrate: format!("{}", r.audio_bps),
    }
}

/// The size rungs are picked on: the cropped picture, else the stream's, else 1920x1080.
pub fn rung_source_size(v: Option<&VideoStreamInfo>) -> (u32, u32) {
    let crop = v.and_then(|v| v.crop.as_ref());
    (
        crop.map(|c| c.width)
            .or(v.and_then(|v| v.width))
            .unwrap_or(1920),
        crop.map(|c| c.height)
            .or(v.and_then(|v| v.height))
            .unwrap_or(1080),
    )
}

/// The top rung of the ladder that fits the source (its last rung when none does).
pub fn top_rung(w: u32, h: u32, hdr: bool) -> &'static str {
    let ladder = if hdr { HDR_LADDER } else { SDR_LADDER };
    ladder
        .iter()
        .find(|r| profile_fits_source(r, Some(w), Some(h)))
        .unwrap_or(&ladder[ladder.len() - 1])
        .name
}

pub fn is_valid_quality(q: &str) -> bool {
    q == "remux" || SDR_LADDER.iter().chain(HDR_LADDER).any(|r| r.name == q)
}

/// `secondsToSegmentIndex`. [ts:modules/streaming/transcoding/constants.ts:114-121]
pub fn seconds_to_segment(seconds: f64, segment_duration: f64, fps: Option<f64>) -> u32 {
    if seconds <= 0.0 || seconds.is_nan() {
        return 0;
    }
    (seconds / real_segment_seconds(segment_duration, fps)).floor() as u32
}

fn source_video_bps(info: &MediaInfo) -> Option<f64> {
    let audio: f64 = info
        .audio
        .iter()
        .map(|a| a.bit_rate.unwrap_or(0) as f64)
        .sum();
    resolve_source_video_bitrate_bps(
        info.video.first().and_then(|v| v.bit_rate).map(f64::from),
        info.format_bit_rate.map(f64::from),
        audio,
    )
}

fn preset(p: QsvPreset) -> String {
    serde_json::to_value(p)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| "faster".into())
}

/// One run's ffmpeg options. [ts:modules/streaming/transcoding/transcoding.service.ts:1740-1804]
/// [ts:modules/streaming/services/session-context-builder.service.ts:31-80]
pub struct RunInput<'a> {
    pub req: &'a OpenSessionRequest,
    pub decision: &'a Decision,
    pub streaming: &'a StreamingConfig,
    pub plan: &'a SessionPlan,
    pub input_path: &'a str,
    pub output_dir: &'a str,
    pub quality: &'a str,
    pub start_segment: u32,
    pub hw: HwAccel,
    /// The early companion: seg-0 and seg-1 only. [ts:transcoding/transcoding.service.ts:965-1060]
    pub early: bool,
    /// ffmpeg `-threads` / `-threads:v`. The engine passes the CPU quota.
    pub encode_threads: u32,
}

/// The picked audio track, when it names one of the source's.
pub fn picked_audio(req: &OpenSessionRequest) -> Option<u32> {
    req.decide
        .selection
        .audio_stream_index
        .filter(|i| *i >= 0 && (*i as usize) < req.decide.info.audio.len())
        .map(|i| i as u32)
}

/// The cache variant of a session's remux runs. [ts:transcoding/transcoding.service.ts:1409-1430]
pub fn remux_variant_of(req: &OpenSessionRequest, plan: &SessionPlan) -> SessionVariant {
    remux_variant(
        picked_audio(req),
        plan.remux_grid.is_some(),
        plan.multi_audio,
    )
}

/// The plan an encoded remux audio start snaps onto: the group's transcoded track, else the
/// picked one's. [ts:transcoding/transcoding.service.ts:1482-1486]
pub fn remux_audio_basis(d: &Decision) -> &AudioPlan {
    d.audio_plans
        .iter()
        .find(|p| matches!(p, AudioPlan::Transcode { .. }))
        .unwrap_or(&d.response.audio_plan)
}

/// One remux run's ffmpeg options; `output_dir` is the run's GOP dir.
/// [ts:transcoding/transcoding.service.ts:1497-1520]
pub fn remux_args(r: &RunInput, run: &RemuxRunStart) -> RemuxArgs {
    let (req, d, plan) = (r.req, r.decision, r.plan);
    let info = &req.decide.info;
    let v = info.video.first();
    RemuxArgs {
        input_path: r.input_path.into(),
        output_dir: r.output_dir.into(),
        run: RemuxRun {
            audio_start_seconds: run.audio_start_seconds,
            seek_seconds: run.seek_seconds,
            start_number: run.start_number as u32,
        },
        trusted_stream_info: v.is_some_and(|v| !v.codec.is_empty()),
        audio_stream_index: picked_audio(req),
        source_video_codec: v.map(|v| v.codec.to_lowercase()).filter(|c| !c.is_empty()),
        audio_streams: Some(audio_streams(info)),
        grid: plan.remux_grid.as_ref().map(|_| serde::de::IgnoredAny),
        segment_duration: Some(plan.segment_duration),
        video_stream_index: v.map(|v| v.stream_index),
        source_end_seconds: plan.timeline.end,
        source_clock_break_seconds: plan.timeline.clock_break,
        audio_plan: Some(d.response.audio_plan.clone()),
        audio_track_plans: (plan.multi_audio && !d.audio_plans.is_empty())
            .then(|| d.audio_plans.clone()),
        video_only: plan.multi_audio,
        dolby_vision: d.response.dolby_vision,
        source_dv_profile: v.and_then(|v| v.dv_profile),
        source_dv_bl_signal_compat_id: v.and_then(|v| v.dv_bl_signal_compat_id),
        hdr10_plus: v.and_then(|v| v.hdr10_plus).unwrap_or(false),
    }
}

fn audio_streams(info: &MediaInfo) -> Vec<AudioStream> {
    info.audio
        .iter()
        .map(|a| AudioStream {
            language: Some(a.language.clone()),
            stream_index: Some(a.stream_index),
        })
        .collect()
}

pub fn vod_args(r: &RunInput) -> VodArgs {
    let (req, d, plan) = (r.req, r.decision, r.plan);
    let info = &req.decide.info;
    let v = info.video.first();
    let dv_no_base = has_no_base(
        v.and_then(|v| v.dv_profile),
        v.and_then(|v| v.dv_bl_signal_compat_id),
    );
    let hdr = v.and_then(|v| v.hdr_format).is_some();
    let picked = picked_audio(req);
    VodArgs {
        input_path: r.input_path.into(),
        profile: rung_profile(r.quality),
        output_dir: r.output_dir.into(),
        hw_accel: r.hw,
        start_segment: r.start_segment,
        tonemap: d.transcode_tonemapping,
        burn_in: req.burn_in.as_ref().map(|b| BurnIn {
            filter: b.filter.clone(),
            stream_index: b.stream_index,
            kind: match b.kind {
                BurnInKind::Text => args::BurnInKind::Text,
                BurnInKind::Image => args::BurnInKind::Image,
            },
        }),
        audio_stream_index: picked,
        crop: r
            .streaming
            .auto_crop_enabled
            .then(|| v.and_then(|v| v.crop.clone()))
            .flatten(),
        video_only: plan.multi_audio,
        audio_streams: Some(audio_streams(info)),
        video_variant: Some(d.video_variant),
        source_video_codec: v.map(|v| v.codec.to_lowercase()).filter(|c| !c.is_empty()),
        source_bit_depth: Some(if hdr || dv_no_base { 10 } else { 8 }),
        source_width: v.and_then(|v| v.width),
        source_height: v.and_then(|v| v.height),
        source_video_bitrate_bps: source_video_bps(info),
        source_hdr_metadata: v.and_then(|v| v.hdr_metadata.clone()),
        source_dv_profile: v.and_then(|v| v.dv_profile),
        source_dv_bl_signal_compat_id: v.and_then(|v| v.dv_bl_signal_compat_id),
        audio_plan: Some(d.response.audio_plan.clone()),
        audio_track_plans: (!d.audio_plans.is_empty()).then(|| d.audio_plans.clone()),
        source_start_pts: Some(plan.timeline.origin),
        source_format_start: Some(plan.timeline.format_start),
        seek_keyframe_dts: seek_keyframe_dts(r),
        source_clock_break_seconds: plan.timeline.clock_break,
        source_end_seconds: plan.timeline.end,
        video_stream_index: v.map(|v| v.stream_index),
        encoder_preset: Some(preset(r.streaming.qsv_preset)),
        tonemap_algo: Some(r.streaming.tonemap_algo),
        tonemap_curve: Some(r.streaming.tonemap_curve),
        source_fps: plan.source_fps,
        source_color_space: v.and_then(|v| v.color_space.clone()),
        source_color_primaries: v.and_then(|v| v.color_primaries.clone()),
        source_color_transfer: v.and_then(|v| v.color_transfer.clone()),
        source_color_range: v.and_then(|v| v.color_range.clone()),
        segment_duration: Some(plan.segment_duration),
        trusted_stream_info: v.is_some_and(|v| !v.codec.is_empty()),
        early: r.early,
        use_ts: plan.use_ts,
        encode_threads: r.encode_threads.max(1),
    }
}

/// MPEG-TS demuxers land after their seek target: seek to the scanned keyframe at or before the
/// run's first frame. [ts:modules/streaming/transcoding/transcoding.service.ts:1704-1738]
fn seek_keyframe_dts(r: &RunInput) -> Option<f64> {
    let info = &r.req.decide.info;
    if r.start_segment == 0 || !is_mpeg_ts(info, r.input_path) {
        return None;
    }
    let start = super::segment_index_to_seconds(
        r.start_segment,
        r.plan.segment_duration,
        r.plan.source_fps,
    ) + r.plan.timeline.origin;
    r.req
        .decide
        .scan
        .as_ref()?
        .keyframes
        .iter()
        .filter(|k| k.pts <= start)
        .map(|k| k.dts)
        .next_back()
}

/// The master's query-driven inputs.
pub struct MasterQuery<'a> {
    /// `/api/stream/<id>`.
    pub prefix: &'a str,
    pub token: &'a str,
    pub include_remux: bool,
    pub only_quality: Option<&'a str>,
}

/// [ts:modules/streaming/streaming.controller.ts:1402-1560]
pub fn master_options(
    req: &OpenSessionRequest,
    d: &Decision,
    plan: &SessionPlan,
    caps: &HostCaps,
    q: &MasterQuery,
) -> MasterOptions {
    let info = &req.decide.info;
    let device = &req.decide.device;
    let v = info.video.first();
    let audio_meta = |a: &crate::api::AudioStreamInfo| AudioStreamMeta {
        language: Some(a.language.clone()),
        language_tag: a.language_tag.clone(),
        title: a.title.clone(),
        channels: a.channels,
    };
    let variant = d.video_variant;
    let hdr_format = v.and_then(|v| v.hdr_format).or(variant.hdr);
    let hdr_pass_through = match (d.use_hdr_ladder, hdr_format, variant.hdr) {
        (true, Some(f), Some(_)) => Some(HdrPassThrough {
            hdr_format: f,
            hdr_variant: variant,
        }),
        _ => None,
    };
    let subtitles =
        (device.supports_hls_subtitles == Some(true)).then(|| match &req.subtitle_renditions {
            Some(list) => list
                .iter()
                .map(|s| SubtitleRendition {
                    kind: match s.kind {
                        SubtitleTrackKind::Embedded => SubtitleKind::Embedded,
                        SubtitleTrackKind::External => SubtitleKind::External,
                    },
                    key: s.key,
                    language: s.language.clone(),
                    language_tag: s.language_tag.clone(),
                    name: s.name.clone(),
                    forced: s.forced,
                })
                .collect(),
            None => info
                .subtitles
                .iter()
                .filter(|s| !s.is_image_based)
                .map(|s| SubtitleRendition {
                    kind: SubtitleKind::Embedded,
                    key: s.stream_index,
                    language: Some(s.language.clone()),
                    language_tag: s.language_tag.clone(),
                    name: s
                        .title
                        .clone()
                        .filter(|t| !t.is_empty())
                        .unwrap_or_else(|| s.language.clone()),
                    forced: s.forced,
                })
                .collect(),
        });
    let positive = |x: Option<u32>| x.filter(|b| *b > 0).map(f64::from);
    // [ts:modules/streaming/streaming.controller.ts:1506-1551]
    let dv = q.include_remux && d.response.dolby_vision;
    let dv_standalone = dv.then(|| dv_standalone_codecs(v)).flatten();
    MasterOptions {
        prefix: q.prefix.into(),
        token_param: q.token.into(),
        source_width: rung_source_size(v).0,
        source_height: rung_source_size(v).1,
        remux_width: v.and_then(|v| v.width),
        remux_height: v.and_then(|v| v.height),
        include_remux: q.include_remux,
        audio_streams: (plan.multi_audio || info.audio.is_empty())
            .then(|| info.audio.iter().map(audio_meta).collect()),
        muxed_audio: (!plan.multi_audio
            && info.audio.len() == 1
            && device.supports_muxed_audio_rendition == Some(true))
        .then(|| audio_meta(&info.audio[0])),
        audio_plans: Some(if plan.multi_audio {
            d.audio_plans.clone()
        } else {
            vec![d.response.audio_plan.clone()]
        }),
        only_quality: q.only_quality.map(String::from),
        default_audio_index: i64::from(req.decide.selection.audio_stream_index.unwrap_or(0)),
        can_emit_hdr_ladder: hdr_pass_through.is_some()
            && resolve_encoder(&variant, caps.hw_accel, caps).is_some(),
        hdr_pass_through,
        sdr_variant: variant.hdr.is_none().then_some(variant),
        source_frame_rate: plan.source_fps,
        subtitle_renditions: subtitles,
        source_video_bitrate_bps: source_video_bps(info),
        source_video_codec: v.map(|v| v.codec.to_lowercase()).filter(|c| !c.is_empty()),
        dedupes_audio_by_language: device.dedupes_audio_by_language == Some(true),
        supports_abr: device.supports_abr,
        remux_codecs: if q.include_remux {
            dv_standalone
                .clone()
                .or_else(|| v.and_then(copy_source_codec_string))
        } else {
            None
        },
        remux_supplemental_codecs: dv.then(|| dv_supplemental_codecs(v)).flatten(),
        format_bit_rate: positive(info.format_bit_rate),
        // A no-base DV copy's standalone CODECS is always PQ.
        source_hdr_format: if dv_standalone.is_some() {
            Some(HdrFormat::Hdr10)
        } else {
            v.and_then(|v| v.hdr_format)
        },
        i_frame_trick_play_segment_seconds: (device.supports_i_frame_trick_play == Some(true))
            .then(|| real_segment_seconds(plan.segment_duration, plan.source_fps)),
    }
}

/// `playUrl` with the session id. [ts:modules/streaming/streaming.controller.ts:1236-1245]
pub fn play_path(play_url: &str, sid: &str) -> String {
    let sep = if play_url.contains('?') { '&' } else { '?' };
    format!("{play_url}{sep}sid={sid}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rung_profiles_and_qualities() {
        let p = rung_profile("720p");
        assert_eq!((p.max_width, p.video_bitrate.as_str()), (1280, "4000000"));
        assert_eq!(rung_profile("1080p-hdr").video_bitrate, "5500000");
        assert_eq!(rung_profile("nope").name, "2160p");
        assert!(is_valid_quality("remux") && is_valid_quality("eco-720p"));
        assert!(!is_valid_quality("iframe") && !is_valid_quality("audio"));
        assert_eq!(top_rung(1920, 1080, false), "1080p");
        assert_eq!(top_rung(1920, 1080, true), "1080p-hdr");
        assert_eq!(top_rung(320, 180, false), "240p");
    }

    #[test]
    fn rungs_are_picked_on_the_cropped_size() {
        let mut v = VideoStreamInfo {
            width: Some(3840),
            height: Some(2160),
            ..Default::default()
        };
        assert_eq!(rung_source_size(Some(&v)), (3840, 2160));
        v.crop = Some(crate::api::CropInfo {
            width: 3840,
            height: 1608,
            x: 0,
            y: 276,
        });
        assert_eq!(rung_source_size(Some(&v)), (3840, 1608));
        assert_eq!(rung_source_size(None), (1920, 1080));
    }

    #[test]
    fn segments_and_play_path() {
        assert_eq!(seconds_to_segment(0.0, 3.0, Some(24.0)), 0);
        assert_eq!(seconds_to_segment(7.0, 3.0, Some(24.0)), 2);
        assert_eq!(seconds_to_segment(-1.0, 3.0, None), 0);
        assert_eq!(
            play_path("/api/stream/1/master.m3u8", "s"),
            "/api/stream/1/master.m3u8?sid=s"
        );
        assert_eq!(
            play_path("/api/stream/1?token=t", "s"),
            "/api/stream/1?token=t&sid=s"
        );
    }

    #[test]
    fn encoder_ids_follow_the_argv() {
        let mut caps = HostCaps::default();
        assert_eq!(audio_encoder_id(AudioCodec::Aac, 2, &caps), "aac");
        caps.libfdk_aac = true;
        assert_eq!(audio_encoder_id(AudioCodec::Aac, 6, &caps), "libfdk_aac");
        assert_eq!(audio_encoder_id(AudioCodec::Aac, 8, &caps), "aac");
        assert_eq!(audio_encoder_id(AudioCodec::Opus, 2, &caps), "libopus");
    }
}
