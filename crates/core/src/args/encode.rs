//! Layer 1: input, seek, HW decode, filters, video encode and audio args, shared by all outputs.
//! [ts:backend/src/modules/streaming/transcoding/ffmpeg-args.ts:660-1409]
use super::encoders::{amf_ends_on_gpu, build, nvenc_ends_on_gpu};
use super::filters::{FilterContext, Overlay, image_burn_in, video_filters};
use super::{BurnInKind, EncoderInput, LiveArgs, LiveMode, QsvTuning, Target, VodArgs};
use crate::api::{AudioCodec, AudioPlan, CodecVariant, HdrFormat, HostCaps, HwAccel, Platform};
use crate::api::{TonemapAlgo, VideoCodec};
use crate::decide::audio::output_bitrate_bps;
use crate::decide::codec::{codec_name, normalise_source_codec};
use crate::decide::dolby_vision::{applies_rpu, has_no_base};
use crate::decide::timeline::real_segment_seconds;
use crate::decide::tonemap::{EncodeContext, Pipeline, TonemapPath, resolve_pipeline};
use crate::hw::Device;
use crate::hw::decoders::{DECODERS, Decoder, Surface, find};
use crate::jsnum::{js_string as js, round, seconds};
use crate::ladder::{codec_factor, hevc_main_tier_cap_bps};

const DEFAULT_SEGMENT_DURATION: f64 = 3.0;
const DEFAULT_FPS: f64 = 24.0;
/// Read ceiling of the trusted-streamInfo fast path: AV1-in-Matroska needs ~3 MB to seek.
const TRUSTED_PROBE_SIZE: &str = "5000000";
const ENCODE_SAMPLE_RATE: &str = "48000";

pub(super) fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

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

pub fn probe_args(trusted: bool) -> Vec<String> {
    if trusted {
        s(&["-analyzeduration", "0", "-probesize", TRUSTED_PROBE_SIZE])
    } else {
        s(&["-analyzeduration", "1000000", "-probesize", "1000000"])
    }
}

/// `-map` of an audio track: its absolute index, else the optional relative one.
pub fn audio_map(streams: Option<&[super::AudioStream]>, i: usize) -> String {
    match streams.and_then(|s| s.get(i)).and_then(|s| s.stream_index) {
        Some(abs) => format!("0:{abs}"),
        None => format!("0:a:{i}?"),
    }
}

pub fn video_map(index: Option<u32>) -> String {
    index.map_or("0:v:0".into(), |i| format!("0:{i}"))
}

/// Pads or trims the audio to start on the video's first frame and pads it to `end`.
fn align_filter(start: f64, end: Option<f64>) -> String {
    let st = seconds(start.abs());
    let (to, back) = if start < 0.0 { ("+", "-") } else { ("-", "+") };
    let pad = match end {
        Some(e) if e > start => format!(",apad=whole_dur={}", seconds(e - start)),
        _ => String::new(),
    };
    format!("asetpts=PTS{to}{st}/TB,aresample=async=1:first_pts=0{pad},asetpts=PTS{back}{st}/TB")
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
            v.push(align_filter(a.align_start, a.end));
            v
        }
    }
}

struct Sdr {
    space: String,
    primaries: String,
    transfer: String,
    range: &'static str,
}

/// Source tags for SDR->SDR, BT.709 for a tone-map; an untagged axis falls back to BT.709 limited.
/// [ts:backend/src/modules/streaming/transcoding/ffmpeg-args.ts:268-290]
fn sdr_tags(o: &VodArgs) -> Sdr {
    let tag = |v: &Option<String>| {
        v.as_deref()
            .filter(|v| !v.is_empty() && *v != "unknown" && *v != "reserved")
            .unwrap_or("bt709")
            .to_string()
    };
    let full = matches!(
        o.source_color_range
            .as_deref()
            .map(str::to_lowercase)
            .as_deref(),
        Some("pc" | "full" | "jpeg")
    );
    if o.tonemap {
        return Sdr {
            space: "bt709".into(),
            primaries: "bt709".into(),
            transfer: "bt709".into(),
            range: "tv",
        };
    }
    Sdr {
        space: tag(&o.source_color_space),
        primaries: tag(&o.source_color_primaries),
        transfer: tag(&o.source_color_transfer),
        range: if full { "pc" } else { "tv" },
    }
}

fn color_args(c: &Sdr) -> Vec<String> {
    s(&[
        "-color_primaries",
        &c.primaries,
        "-color_trc",
        &c.transfer,
        "-colorspace",
        &c.space,
        "-color_range",
        c.range,
    ])
}

/// `{"bitDepth":..,"codec":..,"hdr":..}`: the host's `JSON.stringify` of the stored variant.
fn variant_json(v: &CodecVariant) -> String {
    let hdr = match v.hdr {
        None => "null",
        Some(HdrFormat::Hdr10) => "\"HDR10\"",
        Some(HdrFormat::Hlg) => "\"HLG\"",
    };
    format!(
        "{{\"bitDepth\":{},\"codec\":\"{}\",\"hdr\":{hdr}}}",
        v.bit_depth,
        codec_name(v.codec)
    )
}

fn hw_name(h: HwAccel) -> &'static str {
    match h {
        HwAccel::Vaapi => "vaapi",
        HwAccel::Nvenc => "nvenc",
        HwAccel::Qsv => "qsv",
        HwAccel::Amf => "amf",
        HwAccel::Videotoolbox => "videotoolbox",
        HwAccel::None => "none",
    }
}

/// Decoder registry: same backend, then any other HW one, then CPU.
/// [ts:backend/src/modules/streaming/transcoding/codec/decoders/index.ts:66-103]
fn resolve_decoder(
    codec: VideoCodec,
    depth: u32,
    preferred: HwAccel,
    caps: &HostCaps,
) -> &'static Decoder {
    let registry = DECODERS.iter().take_while(|d| d.id != "cpu");
    let usable = |d: &&Decoder| {
        d.supported(caps.platform)
            && d.codec.is_none_or(|c| c == codec)
            && depth <= d.max_bit_depth()
            && !caps.disabled_decoders.iter().any(|x| x == d.id)
    };
    registry
        .clone()
        .filter(|d| d.hw() == preferred)
        .find(usable)
        .or_else(|| {
            (preferred != HwAccel::None)
                .then(|| {
                    registry
                        .filter(|d| d.hw() != preferred && d.hw() != HwAccel::None)
                        .find(usable)
                })
                .flatten()
        })
        .unwrap_or_else(|| find("cpu"))
}

fn vt_surface_decodable(codec: Option<VideoCodec>, caps: &HostCaps) -> bool {
    codec.is_some_and(|c| {
        resolve_decoder(c, 10, HwAccel::Videotoolbox, caps).hw() == HwAccel::Videotoolbox
    })
}

struct DecodeStage {
    args: Vec<String>,
    decoder: &'static Decoder,
    vt_metal: bool,
    vt_surface: bool,
    qsv_opencl_tonemap: bool,
}

struct DecodeNeeds<'a> {
    source_video_codec: Option<&'a str>,
    source_bit_depth: u32,
    tonemap: bool,
    hdr_output: bool,
    burn_in: bool,
    crop: bool,
    opencl_tonemap: bool,
}

/// The decoder a resolved pipeline reads the source with. [ts:backend/src/modules/streaming/transcoding/ffmpeg-args.ts:660-705]
fn pick_decoder(
    p: &Pipeline,
    source_video_codec: Option<&str>,
    depth: u32,
    caps: &HostCaps,
) -> &'static Decoder {
    let hw = p.effective;
    let codec = source_video_codec.and_then(normalise_source_codec);
    let reported = source_video_codec.is_some_and(|c| !c.is_empty());
    match codec {
        Some(c) if p.qsv_native => find(&format!(
            "{}{}",
            codec_name(c),
            if caps.platform == Platform::Win32 {
                "_qsv_d3d11_decode"
            } else {
                "_qsv_decode"
            }
        )),
        Some(c) if p.amf_opencl_available && hw == HwAccel::Amf => {
            find(&format!("{}_d3d11va_native_decode", codec_name(c)))
        }
        // A reported codec without a HW decoder here decodes on the CPU.
        _ => resolve_decoder(
            codec.unwrap_or(VideoCodec::H264),
            depth,
            if reported && codec.is_none() {
                HwAccel::None
            } else {
                hw
            },
            caps,
        ),
    }
}

/// `tonemap_cuda` on NVENC, else the OpenCL bounce of NVENC/AMF and of no-base or RPU sources.
/// [ts:backend/src/modules/streaming/transcoding/encode-pipeline.ts:31-53]
fn tonemap_bounces(
    tonemap: bool,
    hw: HwAccel,
    dv_no_base: bool,
    dv_apply_rpu: bool,
    caps: &HostCaps,
) -> (bool, bool) {
    let cuda = tonemap && hw == HwAccel::Nvenc && caps.probe("cuda");
    let opencl = tonemap
        && !cuda
        && caps.probe("opencl")
        && (matches!(hw, HwAccel::Nvenc | HwAccel::Amf)
            || dv_no_base
            || (dv_apply_rpu && hw == HwAccel::None));
    (cuda, opencl)
}

/// What a session's pipeline reports: its tone-map step, whether that step takes a curve, and
/// its decoder. [ts:backend/src/modules/streaming/transcoding/encode-pipeline.ts:306-358]
pub(crate) struct StageReport {
    pub tonemap: Option<&'static str>,
    pub curve_applies: bool,
    pub decoder_hw: HwAccel,
    pub decoder_id: &'static str,
}

pub(crate) struct ReportNeeds<'a> {
    pub tonemap: bool,
    pub dv_no_base: bool,
    pub dv_apply_rpu: bool,
    /// Any burn-in.
    pub burn_in: bool,
    pub source_video_codec: Option<&'a str>,
    pub source_bit_depth: u32,
    /// No-base DV reshaped to HDR10: `apply_dovi` runs no curve.
    pub hdr10_target: bool,
}

pub(crate) fn stage_report(p: &Pipeline, n: &ReportNeeds, caps: &HostCaps) -> StageReport {
    let hw = p.effective;
    let (cuda, opencl) = tonemap_bounces(n.tonemap, hw, n.dv_no_base, n.dv_apply_rpu, caps);
    let codec = n.source_video_codec.and_then(normalise_source_codec);
    let tonemap = n.tonemap.then(|| match hw {
        HwAccel::Qsv | HwAccel::Vaapi if p.use_vaapi_tonemap => "vaapi",
        HwAccel::Qsv | HwAccel::Vaapi => match p.path {
            TonemapPath::Vaapi => "vaapi",
            TonemapPath::Opencl => "opencl",
            TonemapPath::Qsv => "qsv",
            TonemapPath::Vulkan => "vulkan",
        },
        _ if cuda => "cuda",
        _ if opencl || (hw == HwAccel::Amf && p.amf_opencl_available) => "opencl",
        HwAccel::Videotoolbox if !n.burn_in && vt_surface_decodable(codec, caps) => "videotoolbox",
        _ => "cpu",
    });
    let decoder = pick_decoder(p, n.source_video_codec, n.source_bit_depth, caps);
    StageReport {
        tonemap,
        curve_applies: !n.hdr10_target
            && matches!(tonemap, Some("opencl" | "vulkan" | "cpu" | "cuda")),
        decoder_hw: decoder.hw(),
        decoder_id: decoder.id,
    }
}

/// How the source reaches memory, plus the tone-map device bridges.
/// [ts:backend/src/modules/streaming/transcoding/ffmpeg-args.ts:660-882]
fn decode_stage(p: &Pipeline, n: &DecodeNeeds, caps: &HostCaps, dev: &Device) -> DecodeStage {
    let hw = p.effective;
    let codec = n.source_video_codec.and_then(normalise_source_codec);
    let amf_native = p.amf_opencl_available && hw == HwAccel::Amf;
    let decoder = pick_decoder(p, n.source_video_codec, n.source_bit_depth, caps);
    let surface = decoder.surface();
    let mut args = decoder.input_args(dev);
    // A CPU decoder inits no device, yet the qsv encoder's filters need one.
    if surface == Surface::Cpu && hw == HwAccel::Qsv {
        args.extend(dev.qsv_init());
        args.extend(s(&["-filter_hw_device", "qs"]));
    }
    let vulkan = p.path == TonemapPath::Vulkan && surface == Surface::Vaapi && hw == HwAccel::Vaapi;
    if let Some(k) = args.iter().position(|a| a == "-filter_hw_device")
        && vulkan
    {
        let tail = args.split_off(k + 2);
        args = dev.vulkan_init();
        args.extend(tail);
    }
    let vt_eligible =
        n.tonemap && hw == HwAccel::Videotoolbox && !n.burn_in && vt_surface_decodable(codec, caps);
    let vt_passthrough = n.hdr_output
        && hw == HwAccel::Videotoolbox
        && !n.burn_in
        && !n.crop
        && vt_surface_decodable(codec, caps);
    let vt_surface = vt_eligible || vt_passthrough;
    if vt_surface {
        args.extend(s(&["-hwaccel_output_format", "videotoolbox_vld"]));
    }
    if n.tonemap
        && !p.use_vaapi_tonemap
        && surface == Surface::Vaapi
        && !p.qsv_native
        && !n.opencl_tonemap
        && p.path != TonemapPath::Vulkan
        && hw != HwAccel::Nvenc
    {
        args.extend(s(&["-init_hw_device", "opencl=ocl:0.0"]));
    }
    if n.opencl_tonemap && !amf_native {
        args.extend(dev.opencl_tonemap_init());
    }
    let qsv_opencl_tonemap = n.tonemap
        && p.path == TonemapPath::Opencl
        && hw == HwAccel::Qsv
        && surface == Surface::D3d11;
    if let Some(k) = args.iter().rposition(|a| a == "-filter_hw_device")
        && qsv_opencl_tonemap
    {
        args[k + 1] = "ocl".into();
        args.splice(k..k, s(&["-init_hw_device", "opencl=ocl@dx"]));
    }
    DecodeStage {
        args,
        decoder,
        vt_metal: vt_eligible && !n.crop,
        vt_surface,
        qsv_opencl_tonemap,
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

    let mut args = s(&["-hide_banner", "-loglevel", "warning"]);
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
        extra: s(&[
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
        },
        caps,
        dev,
    );
    args.extend(d.args);

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
    args.extend(["-i".into(), o.input_path.clone()]);
    args.extend(s(&["-copyts", "-muxdelay", "0", "-muxpreload", "0"]));
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
        dv_apply_rpu,
        has_burn_in: text_burn_in,
        has_crop: o.crop.is_some(),
        input_surface: if d.vt_surface {
            Surface::Videotoolbox
        } else {
            d.decoder.surface()
        },
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

/// Live input decode flags: the shared device init, `-hwaccel` of the backend.
/// [ts:backend/src/modules/livetv/services/live-ffmpeg-args.ts:60-89]
pub fn live_input(hw: HwAccel, dev: &Device) -> Vec<String> {
    match hw {
        HwAccel::Qsv => {
            let mut a = dev.qsv_init();
            a.extend(s(&[
                "-hwaccel",
                "qsv",
                "-hwaccel_output_format",
                "qsv",
                "-hwaccel_device",
                "qs",
            ]));
            a
        }
        HwAccel::Vaapi => {
            let mut a = dev.vaapi_init();
            a.extend(s(&[
                "-hwaccel",
                "vaapi",
                "-hwaccel_output_format",
                "vaapi",
                "-hwaccel_device",
                "va",
            ]));
            a
        }
        HwAccel::Nvenc => s(&["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"]),
        HwAccel::Videotoolbox => s(&["-hwaccel", "videotoolbox"]),
        _ => vec![],
    }
}

/// The live transcode's encoder. [ts:backend/src/modules/livetv/services/live-ffmpeg-args.ts:47-54]
pub fn live_h264_encoder(hw: HwAccel) -> String {
    match hw {
        HwAccel::None => "libx264".into(),
        hw => format!("h264_{}", hw_name(hw)),
    }
}

/// Live video: copy, or H.264 on the backend's encoder cut on a time grid.
/// [ts:backend/src/modules/livetv/services/live-ffmpeg-args.ts:135-156]
pub fn live_video(o: &LiveArgs) -> Vec<String> {
    if o.mode == LiveMode::Remux {
        return s(&["-c:v", "copy"]);
    }
    let mut a = vec!["-c:v".to_string(), live_h264_encoder(o.hw_accel)];
    if o.hw_accel == HwAccel::None {
        a.extend(s(&["-preset", "veryfast", "-tune", "zerolatency"]));
    }
    let b = o.video_bitrate_bps.unwrap_or(4_000_000.0);
    a.extend([
        "-b:v".into(),
        js(b),
        "-maxrate".into(),
        js(b),
        "-bufsize".into(),
        js(b * 2.0),
        "-force_key_frames".into(),
        format!("expr:gte(t,n_forced*{})", js(o.segment_seconds)),
    ]);
    a
}

/// `parseBitrateToBps`: `8M`, `500k`, `192k` or bare bps; anything else is 0.
pub fn parse_bitrate(s: &str) -> f64 {
    let t = s.trim();
    let (num, unit) = match t.char_indices().last() {
        Some((i, c)) if "kKmM".contains(c) => (t[..i].trim_end(), Some(c.to_ascii_lowercase())),
        _ => (t, None),
    };
    let (int, frac) = num.split_once('.').unwrap_or((num, "0"));
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    if !digits(int) || !digits(frac) {
        return 0.0;
    }
    let n: f64 = num.parse().unwrap_or(0.0);
    match unit {
        Some('m') => round(n * 1e6),
        Some('k') => round(n * 1e3),
        _ => round(n),
    }
}

#[cfg(test)]
mod bitrate_tests {
    use super::*;

    #[test]
    fn bitrates() {
        assert_eq!(parse_bitrate("8M"), 8e6);
        assert_eq!(parse_bitrate(" 192k "), 192e3);
        assert_eq!(parse_bitrate("1.5 M"), 1.5e6);
        assert_eq!(parse_bitrate("8000000"), 8e6);
        assert_eq!(parse_bitrate("8MB"), 0.0);
        assert_eq!(parse_bitrate(".5M"), 0.0);
    }
}
