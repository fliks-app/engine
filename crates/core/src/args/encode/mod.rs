//! Layer 1: input, seek, HW decode, filters, video encode and audio args, shared by all outputs.
//! [ts:backend/src/modules/streaming/transcoding/ffmpeg-args.ts:660-1409]

mod audio;
mod decode;
mod live;

pub use self::audio::{
    AudioEnc, DEFAULT_AUDIO_PLAN, audio_copy_args, audio_encode_args, audio_map, audio_stream_args,
    video_map,
};
pub(crate) use self::decode::{ReportNeeds, stage_report};
pub use self::live::{live_h264_encoder, live_input, live_video, parse_bitrate};

use self::decode::{
    DecodeNeeds, color_args, decode_stage, gpu_deint, hw_name, sdr_tags, tonemap_bounces,
    variant_json,
};
use super::encoders::{amf_ends_on_gpu, build, nvenc_ends_on_gpu};
use super::filters::{FilterContext, Overlay, image_burn_in, video_filters};
use super::{BurnInKind, EncoderInput, QsvTuning, Target, VodArgs};
use crate::api::{HdrFormat, HostCaps, HwAccel, TonemapAlgo, VideoCodec};
use crate::decide::codec::codec_name;
use crate::decide::dolby_vision::{applies_rpu, has_no_base};
use crate::decide::timeline::real_segment_seconds;
use crate::decide::tonemap::{EncodeContext, TonemapPath, resolve_pipeline};
use crate::hw::Device;
use crate::hw::decoders::Surface;
use crate::jsnum::{js_string as js, round, seconds};
use crate::ladder::{codec_factor, hevc_main_tier_cap_bps};
use crate::strs;

const DEFAULT_SEGMENT_DURATION: f64 = 3.0;
const DEFAULT_FPS: f64 = 24.0;
/// Read ceiling of the trusted-streamInfo fast path: AV1-in-Matroska needs ~3 MB to seek.
const TRUSTED_PROBE_SIZE: &str = "5000000";
const ENCODE_SAMPLE_RATE: &str = "48000";

/// What layer 2 needs of a layer-1 run.
pub struct Encoded {
    /// Probe, seek, decode, `-i`, the video encode and its colour tags.
    pub args: Vec<String>,
    /// `-map` value of the encoded video (`0:v:0`, `0:N` or `[vout]`).
    pub video_map: String,
    pub audio: AudioEnc,
    pub segment_duration: f64,
    /// Real segment length on the wire (GOP / fps).
    pub real_segment: f64,
    /// Output `-ss` of the run, 0 for none.
    pub output_seek: f64,
    /// Source time of the first presented frame.
    pub origin: f64,
}

pub fn probe_args(trusted: bool) -> Vec<String> {
    if trusted {
        strs(&["-analyzeduration", "0", "-probesize", TRUSTED_PROBE_SIZE])
    } else {
        strs(&["-analyzeduration", "1000000", "-probesize", "1000000"])
    }
}

/// Layer 1 of a transcode run. [ts:backend/src/modules/streaming/transcoding/ffmpeg-args.ts:945-1386]
pub fn encode(o: &VodArgs, caps: &HostCaps, dev: &Device) -> Result<Encoded, String> {
    // The run's backend, which the session froze, wins over the detected one.
    let caps = &HostCaps {
        hw_accel: o.hw_accel,
        ..caps.clone()
    };
    let seg = o.segment_duration.unwrap_or(DEFAULT_SEGMENT_DURATION);
    let fps = o.source_fps.filter(|f| *f > 0.0).unwrap_or(DEFAULT_FPS);
    let gop = round(seg * fps).max(1.0);
    let real_seg = real_segment_seconds(seg, o.source_fps);
    let start = o.start_segment;
    let seek = if start > 0 {
        f64::from(start) * real_seg
    } else {
        0.0
    };
    let start_pts = o.source_start_pts.unwrap_or(0.0);
    let format_start = o.source_format_start.unwrap_or(start_pts);
    let align = seek + start_pts;
    let output_seek = if start > 0 || align < 0.0 { align } else { 0.0 };
    let audio = AudioEnc {
        stereo_bps: parse_bitrate(&o.profile.audio_bitrate),
        align_start: align,
        end: o.source_end_seconds,
        use_ts: o.use_ts,
        libfdk_aac: caps.libfdk_aac,
    };

    let mut args = strs(&["-hide_banner", "-loglevel", "warning"]);
    args.extend(probe_args(o.trusted_stream_info));
    if start > 0 {
        let ss = match o.seek_keyframe_dts {
            Some(dts) => dts - format_start,
            None => seek + (start_pts - format_start),
        };
        args.extend(["-ss".into(), seconds(ss)]);
    }

    let (fw, fh) = match &o.crop {
        Some(c) => (f64::from(c.width), f64::from(c.height)),
        None => (
            f64::from(o.source_width.unwrap_or(0)),
            f64::from(o.source_height.unwrap_or(0)),
        ),
    };
    let (w, h) = if fw > 0.0 && fh > 0.0 {
        let w = f64::from(o.profile.max_width).min(fw);
        (w, ((w * fh / fw / 2.0).ceil() * 2.0).max(2.0))
    } else {
        (
            f64::from(o.profile.max_width),
            f64::from(o.profile.max_height),
        )
    };

    let src_codec = o.source_video_codec.as_deref();
    let rung_bps = parse_bitrate(&o.profile.video_bitrate);
    let mut bitrate = match o.source_video_bitrate_bps.filter(|b| *b > 0.0) {
        None => rung_bps,
        Some(src) => {
            let target = o
                .video_variant
                .map_or(src_codec.unwrap_or(""), |v| codec_name(v.codec));
            let headroom = (codec_factor(target) / codec_factor(src_codec.unwrap_or(""))).max(1.0);
            rung_bps.min(round(src * headroom))
        }
    };
    if o.video_variant.is_some_and(|v| v.codec == VideoCodec::Hevc) {
        bitrate = bitrate.min(hevc_main_tier_cap_bps(w * h * fps));
    }
    let variant = o.video_variant.ok_or_else(|| {
        format!(
            "buildFfmpegArgs: missing videoVariant for profile \"{}\" \u{2014} caller must thread it from the LiveSession",
            o.profile.name
        )
    })?;
    let hdr_output = variant.hdr.is_some();
    let dv_no_base = has_no_base(o.source_dv_profile, o.source_dv_bl_signal_compat_id);
    let dv_apply_rpu = applies_rpu(o.source_dv_profile, o.source_dv_bl_signal_compat_id);
    let dv_no_base_hdr10 = dv_no_base && variant.hdr == Some(HdrFormat::Hdr10);
    let text_filter = o.burn_in.as_ref().and_then(|b| b.filter.as_deref());
    let text_burn_in = text_filter.is_some_and(|f| !f.is_empty());
    let image_stream = o.burn_in.as_ref().and_then(|b| {
        (b.kind == BurnInKind::Image)
            .then_some(b.stream_index)
            .flatten()
    });
    let any_burn_in = o.burn_in.is_some();
    let source_bit_depth = o.source_bit_depth.unwrap_or(8);
    let deinterlace = o.source_interlaced;

    let p = resolve_pipeline(
        &variant,
        &EncodeContext {
            crop: o.crop.is_some(),
            burn_in: text_burn_in,
            tonemap: o.tonemap,
            algo: o.tonemap_algo.unwrap_or(TonemapAlgo::Auto),
            source_video_codec: src_codec.unwrap_or(""),
            dv_no_base,
            dv_apply_rpu,
            source_bit_depth,
            interlaced: deinterlace,
        },
        caps,
    );
    let Some(encoder) = p.encoder else {
        return Err(format!(
            "No encoder for variant {} on {}",
            variant_json(&variant),
            hw_name(p.requested)
        ));
    };
    let hw = p.effective;
    let use_vulkan = p.path == TonemapPath::Vulkan && hw == HwAccel::Vaapi;
    let (cuda, opencl) = tonemap_bounces(o.tonemap, hw, dv_no_base, dv_apply_rpu, caps);

    let early = o.early;
    let preset = if early {
        "veryfast".to_string()
    } else {
        o.encoder_preset.clone().unwrap_or_else(|| "faster".into())
    };
    let qsv = QsvTuning {
        extra: strs(&[
            "-forced_idr",
            "1",
            "-adaptive_i",
            "0",
            "-bf",
            "0",
            "-b_strategy",
            "0",
        ]),
        rc_init_occupancy: (if early { round(bitrate * 0.5) } else { bitrate }).max(1.0),
        bufsize: bitrate,
    };
    let x264_bufsize = format!(
        "{}M",
        js((bitrate * if early { 1.0 } else { 2.0 } / 1_000_000.0)
            .ceil()
            .max(1.0))
    );

    let d = decode_stage(
        &p,
        &DecodeNeeds {
            source_video_codec: src_codec,
            source_bit_depth,
            tonemap: o.tonemap,
            hdr_output,
            burn_in: any_burn_in,
            crop: o.crop.is_some(),
            opencl_tonemap: opencl,
            interlaced: deinterlace,
        },
        caps,
        dev,
    );
    args.extend(d.args);
    let surface = if d.vt_surface {
        Surface::Videotoolbox
    } else {
        d.decoder.surface()
    };
    let gpu_deint = gpu_deint(&p, surface, deinterlace, caps);

    let sdr = sdr_tags(o);
    if !hdr_output && !d.vt_metal && !o.tonemap {
        args.extend(color_args(&sdr));
    }
    let read_end = o
        .source_clock_break_seconds
        .map_or(f64::INFINITY, |b| b - format_start)
        .min(if early {
            crate::session::early_run_seconds(seg)
        } else {
            f64::INFINITY
        });
    if read_end.is_finite() {
        args.extend(["-to".into(), seconds(read_end)]);
    }
    let threads = o.encode_threads.max(1).to_string();
    args.extend(["-threads".into(), threads]);
    args.extend(["-i".into(), o.input_path.clone()]);
    args.extend(strs(&["-copyts", "-muxdelay", "0", "-muxpreload", "0"]));
    if output_seek != 0.0 {
        args.extend(["-ss".into(), seconds(align)]);
    }

    let (ws, hs) = (js(w), js(h));
    let input = EncoderInput {
        variant,
        target: Target {
            width: w,
            height: h,
            video_bitrate_bps: bitrate,
            gop_size: gop,
            frame_rate: fps,
        },
        preset,
        nvenc_preset: "p4".into(),
        early,
        force_keyframes_expr: format!("expr:gte(n,n_forced*{})", js(gop)),
        qsv,
        libx264_bufsize_mb: x264_bufsize,
        filters: video_filters(&FilterContext {
            deinterlace,
            gpu_deint,
            crop: o.crop.as_ref(),
            burn_in_filter: text_filter,
            tonemap: o.tonemap,
            use_vaapi_tonemap: p.use_vaapi_tonemap,
            use_vulkan_tonemap: use_vulkan,
            source_bit_depth,
            dv_no_base,
            dv_apply_rpu,
            curve: o.tonemap_curve,
            opencl_tonemap: opencl,
            cuda_tonemap: cuda,
            scale_width: ws.clone(),
            scale_height: hs.clone(),
            hdr10_target: dv_no_base_hdr10,
        }),
        tonemap: o.tonemap,
        tonemap_path: p.path,
        tonemap_curve: o.tonemap_curve,
        threads: o.encode_threads.max(1),
        dv_apply_rpu,
        has_burn_in: text_burn_in,
        has_crop: o.crop.is_some(),
        input_surface: surface,
        qsv_native: Some(p.qsv_native),
        hdr_metadata: o.source_hdr_metadata.clone(),
    };
    let video_start = args.len();
    args.extend(build(encoder, &input)?);

    let sdr_step = (!hdr_output && !d.vt_metal && !use_vulkan && hw != HwAccel::Nvenc).then(|| {
        format!(
            "setparams=color_primaries={}:color_trc={}:colorspace={}:range={}",
            sdr.primaries, sdr.transfer, sdr.space, sdr.range
        )
    });
    let vf = args[video_start..]
        .iter()
        .position(|a| a == "-vf")
        .map(|k| k + video_start);
    if let (Some(step), None, Some(k)) = (&sdr_step, image_stream, vf) {
        args[k + 1] = format!("{},{step}", args[k + 1]);
    }
    if let Some(stream_index) = image_stream {
        let chain = vf.map(|k| args.drain(k..k + 2).nth(1).unwrap());
        let frames_on_gpu = match hw {
            HwAccel::Nvenc => nvenc_ends_on_gpu(&input),
            HwAccel::Amf => amf_ends_on_gpu(&input),
            _ => true,
        };
        let graph = image_burn_in(&Overlay {
            hw,
            video_filter: chain.as_deref().unwrap_or(""),
            stream_index,
            video_stream_index: o.video_stream_index,
            width: &ws,
            height: &hs,
            bit_depth: variant.bit_depth,
            crop: o.crop.as_ref(),
            frames_on_gpu,
            opencl_filter_device: hw == HwAccel::Amf || d.qsv_opencl_tonemap,
        });
        let graph = match &sdr_step {
            Some(step) => graph.replacen("[vout]", &format!(",{step}[vout]"), 1),
            None => graph,
        };
        args.extend(["-filter_complex".into(), graph]);
    }
    if !hdr_output && !d.vt_metal && !use_vulkan && !(p.use_vaapi_tonemap && hw == HwAccel::Vaapi) {
        args.extend(color_args(&sdr));
    }
    Ok(Encoded {
        args,
        video_map: if image_stream.is_some() {
            "[vout]".into()
        } else {
            video_map(o.video_stream_index)
        },
        audio,
        segment_duration: seg,
        real_segment: real_seg,
        output_seek,
        origin: start_pts,
    })
}
