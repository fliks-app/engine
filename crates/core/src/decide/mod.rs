//! The playback decision: DirectPlay, DirectStream (remux) or Transcode, a pure port of the
//! host's `StreamBuilderService.evaluate()`. Spec: `docs/spec/decide.md`.
pub mod audio;
mod caps;
pub mod codec;
mod direct_play;
pub mod dolby_vision;
mod estimate;
mod qualities;
pub mod source;
pub mod timeline;
pub(crate) mod tonemap;

use crate::api::{
    AudioPlan, AudioTrackPlan, AutoQualityMode, DecideRequest, Decision, HdrFormat, HostCaps,
    HwAccel, MuxFlavour, PlayMethod, PlaybackInfo, StreamingConfig, TranscodeReason,
};
use crate::ladder::{
    Rung, bucket_resolution_height, is_eco, ladder, profile_fits_source, resolve_rung, top_non_eco,
};
use audio::{
    AudioContext, ENCODE_SAMPLE_RATE, REMUX_STEREO_BPS, TrackDecision, audio_reason_message,
    decide_audio, lower_codec, output_bitrate_bps,
};
use codec::{SourceForSelector, normalise_source_codec, pick_primary_variant};
use direct_play::try_direct_play;
pub use estimate::estimate_sizes;
use qualities::{build_quality_list, transcode_bitrate_by_quality};
use timeline::{
    VideoCut, frame_seconds_of, last_video_segment_start, parse_source_fps, remux_grid_boundaries,
};
use tonemap::{EncodeContext, dv_no_base_hdr10_path_supported, resolve_pipeline};

fn reason(flag: &str, message: impl Into<String>) -> TranscodeReason {
    TranscodeReason {
        flag: flag.into(),
        message: message.into(),
    }
}

/// `{mode: copy}` plan and track report of a stream played as-is.
fn track_dto(
    t: &crate::api::AudioStreamInfo,
    index: usize,
    d: &TrackDecision,
    stereo_bps: f64,
) -> AudioTrackPlan {
    let (copy, output_codec, output_channels) = match &d.plan {
        AudioPlan::Copy {
            codec, channels, ..
        } => (true, codec.clone(), *channels),
        AudioPlan::Transcode { codec, channels } => {
            (false, codec.name().to_string(), Some(*channels))
        }
    };
    AudioTrackPlan {
        index: index as u32,
        language: t.language.clone(),
        codec: lower_codec(t),
        channels: t.channels,
        copy,
        output_codec,
        output_channels,
        bitrate_bps: output_bitrate_bps(&d.plan, stereo_bps) as u32,
        sample_rate: if copy {
            t.sample_rate
        } else {
            Some(ENCODE_SAMPLE_RATE)
        },
        reason_flags: d.reason_flags.iter().map(|f| f.to_string()).collect(),
    }
}

pub fn decide(req: &DecideRequest, caps: &HostCaps, config: &StreamingConfig) -> Decision {
    let (info, device, sel) = (&req.info, &req.device, &req.selection);
    let v = info.video.first();
    let audio_streams = &info.audio;
    let has_audio = !audio_streams.is_empty();
    let picked_audio = sel
        .audio_stream_index
        .filter(|i| *i >= 0 && (*i as usize) < audio_streams.len())
        .map_or(0, |i| i as usize);

    let source = source::build(req, config, picked_audio);
    let is_source_hdr = source.hdr_format.is_some();
    let dv = dolby_vision::derive(v);
    let no_base = dolby_vision::has_no_base(dv.profile, dv.compat_id);
    let dv_label = if no_base {
        format!("Dolby Vision P{}", dv.profile.unwrap_or(0))
    } else {
        source
            .hdr_format
            .map_or("undefined", HdrFormat::as_str)
            .to_string()
    };
    let client_supports_hdr = device.supports_hdr == Some(true);
    let dv_profiles = dolby_vision::client_profiles(device);
    let burn_in_is_text = sel.burn_in_is_text == Some(true);
    let dv_no_base_hdr10_eligible = no_base
        && client_supports_hdr
        && dv_no_base_hdr10_path_supported(
            caps,
            config.tonemap_algo,
            source.crop.is_some(),
            burn_in_is_text,
        );

    let selected_variant = pick_primary_variant(
        &SourceForSelector {
            width: source.width.unwrap_or(0),
            height: source.height.unwrap_or(0),
            hdr: if no_base {
                dv_no_base_hdr10_eligible.then_some(HdrFormat::Hdr10)
            } else {
                source.hdr_format
            },
            codec: normalise_source_codec(&source.video_codec),
        },
        device,
        caps,
    );
    let use_hdr_ladder = selected_variant.hdr.is_some();
    let dv_no_base_hdr10 = no_base && selected_variant.hdr == Some(HdrFormat::Hdr10);
    // Copy paths never tone-map; a re-encode that cannot keep the HDR does.
    let transcode_tonemaps = (is_source_hdr || no_base) && !use_hdr_ladder;
    let runs_tonemap_filter = transcode_tonemaps || dv_no_base_hdr10;
    let mux = if device.use_ts == Some(true)
        || (device.use_ts_on_single_audio == Some(true) && audio_streams.len() <= 1)
    {
        MuxFlavour::Ts
    } else {
        MuxFlavour::Fmp4
    };
    let needs_burn_in = sel.burn_in_subtitle_id.is_some_and(|id| id != 0);
    let needs_crop = source.crop.is_some();
    let client_crops_black_bars = device.crops_black_bars_locally == Some(true);

    let wrap = |response: PlaybackInfo, audio_plans: Vec<AudioPlan>| Decision {
        response,
        use_hdr_ladder,
        video_variant: selected_variant,
        mux_flavour: mux,
        audio_plans,
        transcode_tonemapping: runs_tonemap_filter,
    };

    let mut reasons: Vec<TranscodeReason> = vec![];
    // Once per session: where HDR blocks the copy, or before a transcode that tone-maps anyway.
    let push_tonemap_reason = |reasons: &mut Vec<TranscodeReason>| {
        if !runs_tonemap_filter || reasons.iter().any(|r| r.flag == "VideoHdrNotSupported") {
            return;
        }
        reasons.push(reason(
            "VideoHdrNotSupported",
            if dv_no_base_hdr10 {
                format!("{dv_label} → HDR10 (tone mapping)")
            } else {
                format!("HDR → SDR (tone mapping {dv_label})")
            },
        ));
    };

    let mut dp = try_direct_play(&source, device, &mut reasons, has_audio);

    // DirectPlay and remux copy the bitstream, so HDR survives for any codec the client accepts at
    // this bit depth (`video_conditions_met` covers the per-codec cap).
    let video_ok = dp.video_supported && dp.video_conditions_met;
    let client_can_present_hdr = is_source_hdr
        && !no_base
        && (client_supports_hdr || device.tonemaps_hdr_locally == Some(true))
        && video_ok;
    let client_can_present_dv = dv.single_layer
        && dv
            .profile
            .is_some_and(|p| dv_profiles.contains(&(p as i32)))
        && video_ok;
    let client_can_present_dynamic_range = client_can_present_hdr || client_can_present_dv;
    let dv_with_base = client_can_present_dv && dolby_vision::dv_supplemental_codecs(v).is_some();
    let dv_standalone_copy =
        client_can_present_dv && dolby_vision::dv_standalone_codecs(v).is_some();
    let dv_copy_allowed = !no_base || client_can_present_dv;
    let dv_remux_copy_allowed = !no_base || dv_standalone_copy;
    let dv_remux_eligible =
        (dv_with_base && dolby_vision::has_copy_codec_string(v)) || dv_standalone_copy;
    // Raw DirectPlay skips the remux-only muxer validation; P7 is dual-layer (never single-layer)
    // yet still reports DV once the client lists profile 7.
    let dv_direct_play_label = (client_can_present_dv
        && (no_base || matches!(dv.compat_id, Some(1 | 4))))
        || (dv.profile == Some(7) && dv_profiles.contains(&7));
    let client_tonemap =
        (client_can_present_hdr || client_can_present_dv || use_hdr_ladder) && !client_supports_hdr;

    if (is_source_hdr && !client_can_present_dynamic_range) || !dv_copy_allowed {
        dp.can_direct_play = false;
        push_tonemap_reason(&mut reasons);
    }
    if needs_burn_in {
        dp.can_direct_play = false;
        reasons.push(reason("SubtitleBurnIn", "Subtitles burned into the video"));
    }
    if needs_crop && !client_crops_black_bars {
        dp.can_direct_play = false;
        reasons.push(reason("VideoCrop", "Removing black bars (crop)"));
    }
    // No platform documents deinterlacing an interlaced bitstream it is handed as-is.
    let needs_deinterlace = v.is_some_and(|v| v.field_order.is_some());
    if needs_deinterlace {
        dp.can_direct_play = false;
        reasons.push(reason("VideoInterlaced", "Deinterlacing"));
    }

    // Quality routing: the source rung is Direct Play / remux; a lower explicit rung or ABR-on-auto
    // routes to the transcode ladder.
    let quality_ladder = ladder(use_hdr_ladder);
    let requested = sel.requested_quality.as_deref();
    let wants_source_rung = matches!(requested, None | Some("auto" | "original"));
    let auto_on_ladder = matches!(requested, None | Some("auto"))
        && config.auto_quality_mode == AutoQualityMode::Abr;
    // Resolved like the master playlist pin, so a rung above the source names the one served.
    let fitting: Vec<Rung> = quality_ladder
        .iter()
        .filter(|r| profile_fits_source(r, source.width, source.height))
        .copied()
        .collect();
    let explicit_rung: Option<&Rung> = match requested {
        Some(name) if !wants_source_rung => resolve_rung(name, &fitting),
        _ => None,
    };
    let negotiated_quality = explicit_rung.map_or("auto", |r| r.name);
    let negotiated_stereo_bps = explicit_rung
        .unwrap_or_else(|| top_non_eco(quality_ladder))
        .audio_bps;
    // An explicit rung forces the ladder only when it steps below the source.
    let explicit_downscale = explicit_rung.is_some_and(|r| {
        bucket_resolution_height(Some(r.max_width), Some(r.max_height))
            < bucket_resolution_height(source.width, source.height)
            || is_eco(r.name)
    });
    let force_ladder = auto_on_ladder || explicit_downscale;

    // Whether the source can be served at its own resolution without re-encoding.
    let source_copyable = video_ok
        && dv_copy_allowed
        && (!is_source_hdr || client_can_present_dynamic_range)
        && !needs_burn_in
        && (!needs_crop || client_crops_black_bars)
        && !needs_deinterlace;
    let copyable_ignoring_gates = source_copyable && !force_ladder;
    let copy_gates = [
        (
            // MPEG-TS carries a copied video for a single audio track only.
            mux == MuxFlavour::Ts && audio_streams.len() > 1,
            "MuxNotSupported",
            "MPEG-TS packaging can't carry a copied video stream",
        ),
        (
            device.reject_copy == Some(true),
            "ClientRejectedCopy",
            "Client asked for a transcode instead of a copied stream",
        ),
    ];
    let can_copy_video =
        copyable_ignoring_gates && dv_remux_copy_allowed && copy_gates.iter().all(|g| !g.0);

    // A client without a real P7 decoder shows black video on the raw dual-layer file.
    if dv.profile == Some(7) && !dv_profiles.contains(&7) {
        dp.can_direct_play = false;
        if copyable_ignoring_gates {
            reasons.push(reason(
                "VideoDolbyVisionP7NotSupported",
                "Dolby Vision profile 7 (dual layer) is not supported by this client",
            ));
        }
    }
    if device.supports_direct_play == Some(false) && dp.can_direct_play {
        dp.can_direct_play = false;
        reasons.push(reason(
            "ClientRequiresHls",
            "Client requires an HLS container (no raw direct play)",
        ));
    }
    if device.needs_hls_subtitles == Some(true) && dp.can_direct_play {
        dp.can_direct_play = false;
        reasons.push(reason(
            "ClientNeedsHlsSubtitles",
            "Client needs HLS subtitle renditions (no sideload into a raw file)",
        ));
    }
    // A raw file plays its first audio track; an engine folding tracks by language reaches only
    // the first of each language.
    let picked_language = audio_streams.get(picked_audio).map(|t| &t.language);
    let unreachable = device.switches_direct_play_audio == Some(false)
        || (device.dedupes_audio_by_language == Some(true)
            && audio_streams
                .iter()
                .position(|t| Some(&t.language) == picked_language)
                != Some(picked_audio));
    // No in-file switch and several tracks: the remux carries them all.
    let remux_for_switching = device.switches_direct_play_audio == Some(false)
        && audio_streams.len() > 1
        && can_copy_video;
    if ((picked_audio != 0 && unreachable) || remux_for_switching) && dp.can_direct_play {
        dp.can_direct_play = false;
        reasons.push(reason(
            "ClientCannotSwitchAudio",
            "Client cannot switch audio tracks inside a raw file",
        ));
    }
    // The raw file carries the same video the client just failed to decode copied.
    if device.reject_copy == Some(true) {
        dp.can_direct_play = false;
    }
    if force_ladder {
        dp.can_direct_play = false;
        if auto_on_ladder {
            reasons.push(reason("VideoAbr", "Adaptive bitrate (ABR) mode"));
        }
    }
    if force_ladder
        && !auto_on_ladder
        && requested.is_some_and(|n| quality_ladder.iter().any(|r| r.name == n))
        && let Some(r) = explicit_rung
    {
        reasons.push(reason(
            "VideoQualityReduced",
            format!("Reduced-quality rung selected ({})", r.name),
        ));
    }

    let token = sel.token_param.as_deref().unwrap_or("");
    let id = req.media.media_file_id;
    let video_codec_out = selected_variant.codec;
    let target_codec = codec::codec_name(video_codec_out);

    if dp.can_direct_play {
        let copies: Vec<TrackDecision> = audio_streams
            .iter()
            .map(|t| TrackDecision {
                plan: AudioPlan::Copy {
                    codec: lower_codec(t),
                    channels: t.channels,
                    bitrate_bps: t.bit_rate,
                    profile: t.profile.clone(),
                },
                reason_flags: vec![],
            })
            .collect();
        let audio_plan = copies.get(picked_audio).map_or_else(
            || AudioPlan::Copy {
                codec: source.audio_codec.clone(),
                channels: None,
                bitrate_bps: None,
                profile: None,
            },
            |c| c.plan.clone(),
        );
        return wrap(
            PlaybackInfo {
                media_file_id: id,
                play_method: PlayMethod::DirectPlay,
                play_url: format!("/api/stream/{id}{token}"),
                content_type: req.media.content_type.clone(),
                transcode_reasons: vec![],
                video_copy_stream: true,
                audio_copy_stream: true,
                output_video_codec: source.video_codec.clone(),
                output_audio_codec: source.audio_codec.clone(),
                audio_plan,
                output_container: source.container.clone(),
                quality: "original".into(),
                hw_accel: HwAccel::None,
                tonemapping: false,
                client_tonemap,
                dolby_vision: dv_direct_play_label,
                remux_master_bandwidth_bps: None,
                transcode_bitrate_by_quality: None,
                qualities: build_quality_list(
                    &source,
                    PlayMethod::DirectPlay,
                    source_copyable,
                    quality_ladder,
                    target_codec,
                    has_audio,
                ),
                audio_tracks: audio_streams
                    .iter()
                    .zip(&copies)
                    .enumerate()
                    .map(|(i, (t, c))| track_dto(t, i, c, negotiated_stereo_bps))
                    .collect(),
                source,
            },
            copies.into_iter().map(|c| c.plan).collect(),
        );
    }

    // Each gate fires only when the source would otherwise have copied.
    if copyable_ignoring_gates {
        for (active, flag, message) in &copy_gates {
            if *active {
                reasons.push(reason(flag, *message));
            }
        }
    }

    // Separate renditions lack the segments their playlist lists past their end.
    let multi_audio = audio_streams.len() > 1;
    let grid = (can_copy_video && multi_audio)
        .then(|| {
            req.scan.as_ref().and_then(|scan| {
                let origin = timeline::source_timeline(info).0;
                let fps = parse_source_fps(source.frame_rate.as_deref());
                remux_grid_boundaries(scan, origin, config.segment_duration, frame_seconds_of(fps))
            })
        })
        .flatten();
    let cut = match (&grid, can_copy_video) {
        (Some(b), true) => VideoCut::Grid(b),
        (None, true) => VideoCut::CopyUnscanned,
        (_, false) => VideoCut::Reencode,
    };
    let group_decisions = decide_audio(
        audio_streams,
        device,
        &AudioContext {
            video: v,
            mux,
            mpeg_ts: source::is_mpeg_ts(info, &req.media.path),
            scan: req.scan.as_ref(),
            last_segment_start: multi_audio
                .then(|| last_video_segment_start(info, config.segment_duration, &cut))
                .flatten(),
        },
    );
    // A DirectStream encodes a transcoded track at the remux encoder's own fixed budget.
    let track_stereo_bps = if can_copy_video {
        REMUX_STEREO_BPS
    } else {
        negotiated_stereo_bps
    };
    let audio_tracks: Vec<AudioTrackPlan> = audio_streams
        .iter()
        .zip(&group_decisions)
        .enumerate()
        .map(|(i, (t, d))| track_dto(t, i, d, track_stereo_bps))
        .collect();
    let audio_plans: Vec<AudioPlan> = group_decisions.iter().map(|d| d.plan.clone()).collect();
    // No audio stream: nothing to copy or encode, never the AAC fallback.
    let audio_plan = match (has_audio, group_decisions.get(picked_audio)) {
        (true, Some(d)) => d.plan.clone(),
        _ => AudioPlan::Copy {
            codec: String::new(),
            channels: None,
            bitrate_bps: None,
            profile: None,
        },
    };
    let rung_audio_bps = |r: &Rung| {
        if has_audio {
            output_bitrate_bps(&audio_plan, r.audio_bps)
        } else {
            0.0
        }
    };
    let can_copy_audio = matches!(audio_plan, AudioPlan::Copy { .. });
    let output_audio_codec = match &audio_plan {
        AudioPlan::Copy { codec, .. } => codec.clone(),
        AudioPlan::Transcode { codec, .. } => codec.name().to_string(),
    };
    if let Some(t) = audio_tracks.get(picked_audio) {
        reasons.extend(t.reason_flags.iter().map(|f| {
            reason(
                f,
                audio_reason_message(f, t.channels, t.output_channels, &t.codec),
            )
        }));
    }

    if can_copy_video {
        if !dp.container_supported && !reasons.iter().any(|r| r.flag == "ContainerNotSupported") {
            reasons.push(reason(
                "ContainerNotSupported",
                format!("Container \"{}\" not supported", source.container),
            ));
        }
        let sep = if token.is_empty() { '?' } else { '&' };
        let remux_bw = remux_bandwidth_bps(&source, &audio_plans);
        let hints =
            transcode_bitrate_by_quality(&source, target_codec, quality_ladder, rung_audio_bps);
        return wrap(
            PlaybackInfo {
                media_file_id: id,
                play_method: PlayMethod::DirectStream,
                play_url: format!("/api/stream/{id}/master.m3u8{token}{sep}remux=1"),
                content_type: "application/vnd.apple.mpegurl".into(),
                transcode_reasons: reasons,
                video_copy_stream: true,
                audio_copy_stream: can_copy_audio,
                output_video_codec: source.video_codec.clone(),
                output_audio_codec,
                audio_plan,
                output_container: "hls".into(),
                quality: "original".into(),
                // The detected backend, not an effective one: the stats bind it to the quality.
                hw_accel: caps.hw_accel,
                tonemapping: false,
                client_tonemap,
                dolby_vision: dv_remux_eligible,
                remux_master_bandwidth_bps: (remux_bw > 0.0).then_some(remux_bw as u32),
                transcode_bitrate_by_quality: Some(hints),
                qualities: build_quality_list(
                    &source,
                    PlayMethod::DirectStream,
                    source_copyable,
                    quality_ladder,
                    target_codec,
                    has_audio,
                ),
                audio_tracks,
                source,
            },
            audio_plans,
        );
    }

    push_tonemap_reason(&mut reasons);
    let hints = transcode_bitrate_by_quality(&source, target_codec, quality_ladder, rung_audio_bps);
    let effective_hw_accel = resolve_pipeline(
        &selected_variant,
        &EncodeContext {
            crop: needs_crop,
            burn_in: burn_in_is_text,
            tonemap: runs_tonemap_filter,
            algo: config.tonemap_algo,
            source_video_codec: &source.video_codec,
            dv_no_base: no_base,
            dv_apply_rpu: dolby_vision::applies_rpu(dv.profile, dv.compat_id),
            source_bit_depth: 8,
            interlaced: v.is_some_and(|v| v.field_order.is_some()),
        },
        caps,
    )
    .effective;
    wrap(
        PlaybackInfo {
            media_file_id: id,
            play_method: PlayMethod::Transcode,
            play_url: format!("/api/stream/{id}/master.m3u8{token}"),
            content_type: "application/vnd.apple.mpegurl".into(),
            transcode_reasons: reasons,
            video_copy_stream: false,
            audio_copy_stream: can_copy_audio,
            output_video_codec: target_codec.into(),
            output_audio_codec,
            audio_plan,
            output_container: "hls".into(),
            quality: negotiated_quality.into(),
            hw_accel: effective_hw_accel,
            tonemapping: runs_tonemap_filter,
            client_tonemap,
            // A transcode re-encodes to H.264/HEVC/AV1 SDR or HDR10, never DV.
            dolby_vision: false,
            remux_master_bandwidth_bps: None,
            transcode_bitrate_by_quality: Some(hints),
            // `can_copy_video`, not `source_copyable`: a source only gated off by mux/reject must
            // not collapse to an uncapped `original`.
            qualities: build_quality_list(
                &source,
                PlayMethod::Transcode,
                can_copy_video,
                quality_ladder,
                target_codec,
                has_audio,
            ),
            audio_tracks,
            source,
        },
        audio_plans,
    )
}

/// BANDWIDTH of the remux variant: the copied video plus the largest audio rendition, else the
/// container total, else a manifest-only placeholder.
fn remux_bandwidth_bps(source: &crate::api::PlaybackSource, plans: &[AudioPlan]) -> f64 {
    if let Some(video) = source.video_bit_rate.filter(|b| *b != 0) {
        let audio_peak = plans
            .iter()
            .map(|p| output_bitrate_bps(p, REMUX_STEREO_BPS))
            .fold(0.0, f64::max);
        return f64::from(video) + audio_peak;
    }
    match source.format_bit_rate.filter(|b| *b != 0) {
        Some(f) => f64::from(f),
        None => 8_000_000.0,
    }
}
