//! What a session open reports beside the decision: the playback clock offset, the tone-map step
//! and the QoE pipeline, from the resolvers the runs use. [ts:modules/streaming/streaming.controller.ts:1086-1196]
use crate::api::{
    BurnInKind, DecideRequest, Decision, HostCaps, HwAccel, PipelineStage, PlayMethod, QoeOutput,
    QoePipeline, QoeSource, StreamingConfig, TonemapCurve,
};
use crate::args::{ReportNeeds, stage_report};
use crate::decide::codec::codec_name;
use crate::decide::dolby_vision::{applies_rpu, has_no_base};
use crate::decide::tonemap::{EncodeContext, resolve_pipeline};
use crate::mp4::source_timeline;

#[derive(Debug, Clone, PartialEq)]
pub struct OpenReport {
    pub playback_clock_offset_s: f64,
    pub tonemap_algo: Option<String>,
    pub tonemap_curve: Option<TonemapCurve>,
    pub qoe_pipeline: QoePipeline,
}

fn hw_kind(hw: HwAccel) -> &'static str {
    match hw {
        HwAccel::None => "cpu",
        HwAccel::Vaapi => "vaapi",
        HwAccel::Nvenc => "nvenc",
        HwAccel::Qsv => "qsv",
        HwAccel::Amf => "amf",
        HwAccel::Videotoolbox => "videotoolbox",
    }
}

fn stage(hw: HwAccel, name: Option<&str>) -> PipelineStage {
    PipelineStage {
        hw: hw != HwAccel::None,
        kind: hw_kind(hw).into(),
        name: name.map(String::from),
    }
}

/// `caps` = the detected host; `burn_in` = the session's burn-in, if any.
pub fn open_report(
    req: &DecideRequest,
    d: &Decision,
    streaming: &StreamingConfig,
    caps: &HostCaps,
    burn_in: Option<BurnInKind>,
) -> OpenReport {
    let r = &d.response;
    let v = req.info.video.first();
    let codec = v.map(|v| v.codec.to_lowercase()).filter(|c| !c.is_empty());
    let (dv_profile, dv_compat) = (
        v.and_then(|v| v.dv_profile),
        v.and_then(|v| v.dv_bl_signal_compat_id),
    );
    let dv_no_base = has_no_base(dv_profile, dv_compat);
    let dv_apply_rpu = applies_rpu(dv_profile, dv_compat);
    let depth = if v.and_then(|v| v.hdr_format).is_some() || dv_no_base {
        10
    } else {
        8
    };
    let crop = streaming.auto_crop_enabled && v.and_then(|v| v.crop.as_ref()).is_some();
    let interlaced = v.is_some_and(|v| v.field_order.is_some());
    let ctx = |tonemap| EncodeContext {
        crop,
        burn_in: burn_in == Some(BurnInKind::Text),
        tonemap,
        algo: streaming.tonemap_algo,
        source_video_codec: codec.as_deref().unwrap_or(""),
        dv_no_base,
        dv_apply_rpu,
        source_bit_depth: depth,
        interlaced,
    };
    let needs = |tonemap| ReportNeeds {
        tonemap,
        dv_no_base,
        dv_apply_rpu,
        burn_in: burn_in.is_some(),
        source_video_codec: codec.as_deref(),
        source_bit_depth: depth,
        hdr10_target: dv_no_base && d.video_variant.hdr == Some(crate::api::HdrFormat::Hdr10),
        interlaced,
    };
    let variant = &d.video_variant;
    let tonemap = r.tonemapping.then(|| {
        let p = resolve_pipeline(variant, &ctx(true), caps);
        stage_report(&p, &needs(true), caps)
    });
    let tonemap_algo = tonemap.as_ref().and_then(|t| t.tonemap).map(String::from);
    let tonemap_curve = tonemap
        .as_ref()
        .filter(|t| t.curve_applies)
        .map(|_| streaming.tonemap_curve);
    let source = QoeSource {
        codec: codec.clone(),
        width: v.and_then(|v| v.width),
        height: v.and_then(|v| v.height),
        hdr: match (dv_profile, v.and_then(|v| v.hdr_format)) {
            (Some(_), _) => Some("DV".into()),
            (None, Some(h)) => serde_json::to_value(h)
                .ok()
                .and_then(|h| h.as_str().map(String::from)),
            _ => None,
        },
    };
    // [ts:modules/streaming/transcoding/qoe-pipeline.ts:36-76]
    let qoe_pipeline = if r.play_method != PlayMethod::Transcode {
        QoePipeline {
            source: Some(source),
            output: None,
            decode: None,
            encode: None,
            tonemap: None,
            burn_in: None,
            crop: None,
            audio_transcode: (r.play_method != PlayMethod::DirectPlay)
                .then_some(!r.audio_copy_stream),
        }
    } else {
        let p = resolve_pipeline(variant, &ctx(r.tonemapping), caps);
        let rep = stage_report(&p, &needs(r.tonemapping), caps);
        let requested = req
            .selection
            .requested_quality
            .clone()
            .filter(|q| !q.is_empty())
            .unwrap_or_else(|| "auto".into());
        QoePipeline {
            source: Some(source),
            output: Some(QoeOutput {
                codec: Some(codec_name(variant.codec).into()),
                hdr: variant.hdr,
                quality: (!r.quality.is_empty() && r.quality != "auto").then(|| r.quality.clone()),
                bitrate: r
                    .transcode_bitrate_by_quality
                    .as_ref()
                    .and_then(|m| m.get(&r.quality))
                    .map(|b| b.video_bitrate_bps),
                requested: Some(requested),
            }),
            decode: Some(stage(rep.decoder_hw, Some(rep.decoder_id))),
            encode: Some(stage(p.effective, p.encoder.map(|e| e.id()).as_deref())),
            tonemap: tonemap_algo.clone(),
            burn_in,
            crop: Some(crop),
            audio_transcode: Some(!r.audio_copy_stream),
        }
    };
    let t = source_timeline(Some(&req.info));
    OpenReport {
        playback_clock_offset_s: if r.play_method == PlayMethod::DirectPlay {
            0.0
        } else {
            t.format_start - t.origin
        },
        tonemap_algo,
        tonemap_curve,
        qoe_pipeline,
    }
}
