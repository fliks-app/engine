//! The ffmpeg commands of the boot detection, with the timeouts the TS gives each.
use super::Encoder;
use super::decoders::{self, Decoder};
use super::device::Device;
use crate::api::{HwAccel, Platform, VideoCodec};
use crate::strs;

/// One ffmpeg invocation: argv after the binary, and the kill deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub args: Vec<String>,
    pub timeout_ms: u64,
}

pub fn cmd(args: Vec<String>, timeout_ms: u64) -> Cmd {
    Cmd { args, timeout_ms }
}

/// Argv pieces to one argv.
fn cat(parts: &[&[String]]) -> Vec<String> {
    parts.concat()
}

const QUIET: &[&str] = &["-hide_banner", "-loglevel", "error"];
// 320x240: AMD VCN rejects tinier frames. [ts:backend/src/modules/streaming/transcoding/hw-detect.ts:11-14]
const BLACK_INPUT: &[&str] = &["-f", "lavfi", "-i", "color=black:s=320x240:d=0.1"];
const ONE_FRAME_NULL: &[&str] = &["-frames:v", "1", "-f", "null", "-"];

/// `ffmpeg -encoders`, read for `libfdk_aac`. [ts:backend/src/modules/streaming/transcoding/audio-encoder-probe.ts:25-35]
pub fn encoders_listing() -> Cmd {
    cmd(strs(&["-hide_banner", "-encoders"]), 10_000)
}

/// One-frame encode per backend, in the platform's probe order: the first to pass is the backend.
/// [ts:backend/src/modules/streaming/transcoding/hw-detect.ts:20-138]
pub fn hw_tests(d: &Device) -> Vec<(HwAccel, Cmd)> {
    let qsv_filter = if d.platform == Platform::Win32 {
        "format=nv12,hwupload,vpp_qsv=w=160:h=120:format=nv12"
    } else {
        "hwupload,format=qsv"
    };
    let qsv = cat(&[
        &strs(QUIET),
        &d.qsv_init(),
        &strs(&["-filter_hw_device", "qs"]),
        &strs(BLACK_INPUT),
        &strs(&["-vf", qsv_filter, "-c:v", "h264_qsv"]),
        &strs(ONE_FRAME_NULL),
    ]);
    let vaapi = cat(&[
        &strs(QUIET),
        &d.vaapi_init(),
        &strs(BLACK_INPUT),
        &strs(&["-filter_hw_device", "va", "-vf", "format=nv12,hwupload"]),
        &strs(&["-c:v", "h264_vaapi"]),
        &strs(ONE_FRAME_NULL),
    ]);
    // No -hwaccel on AMF: it probes the encoder only.
    let simple = |pre: &[&str], enc: &str| {
        cat(&[
            &strs(QUIET),
            &strs(pre),
            &strs(BLACK_INPUT),
            &strs(&["-c:v", enc]),
            &strs(ONE_FRAME_NULL),
        ])
    };
    let amf = simple(&[], "h264_amf");
    let nvenc = simple(&["-hwaccel", "cuda"], "h264_nvenc");
    let vt = simple(&[], "h264_videotoolbox");
    let t = |h, a| (h, cmd(a, 10_000));
    match d.platform {
        Platform::Darwin => vec![t(HwAccel::Videotoolbox, vt)],
        Platform::Win32 => vec![
            t(HwAccel::Qsv, qsv),
            t(HwAccel::Amf, amf),
            t(HwAccel::Nvenc, nvenc),
        ],
        Platform::Linux => vec![
            t(HwAccel::Qsv, qsv),
            t(HwAccel::Vaapi, vaapi),
            t(HwAccel::Nvenc, nvenc),
        ],
    }
}

/// Backends whose encoders are worth probing: the detected one, the CPU, and VAAPI on QSV hosts.
/// [ts:backend/src/modules/streaming/transcoding/codec/encoder-probe.ts:88-105]
pub fn probeable(detected: HwAccel, hw: HwAccel) -> bool {
    hw == HwAccel::None || hw == detected || (detected == HwAccel::Qsv && hw == HwAccel::Vaapi)
}

/// VAAPI driver vendor string, logged at verbose level during device init.
/// [ts:backend/src/modules/streaming/transcoding/codec/encoder-probe.ts:51-80]
pub fn vaapi_driver_cmd(d: &Device) -> Cmd {
    cmd(
        cat(&[
            &strs(&["-hide_banner", "-loglevel", "verbose"]),
            &d.vaapi_init(),
            &strs(&["-f", "lavfi", "-i", "nullsrc=size=64x64:rate=1"]),
            &strs(ONE_FRAME_NULL),
        ]),
        10_000,
    )
}

/// One-frame encode of a descriptor through the device chain a session uses.
/// [ts:backend/src/modules/streaming/transcoding/codec/encoder-probe.ts:247-310]
pub fn encoder_cmd(e: &Encoder, d: &Device) -> Cmd {
    let pix = if e.ten_bit { "yuv420p10le" } else { "yuv420p" };
    let surface = if e.ten_bit { "p010le" } else { "nv12" };
    let lavfi = format!("nullsrc=size=320x180:rate=30,format={pix}");
    let lavfi_in = |dev: &str| strs(&["-filter_hw_device", dev, "-f", "lavfi", "-i", &lavfi]);
    let (input, filter) = match e.hw {
        HwAccel::Vaapi => (
            cat(&[&d.vaapi_init(), &lavfi_in("va")]),
            strs(&["-vf", &format!("format={surface},hwupload")]),
        ),
        HwAccel::Qsv => (
            cat(&[&d.qsv_init(), &lavfi_in("qs")]),
            strs(&["-vf", &format!("format={surface},hwupload,format=qsv")]),
        ),
        _ => (strs(&["-f", "lavfi", "-i", &lavfi]), vec![]),
    };
    cmd(
        cat(&[
            &strs(QUIET),
            &input,
            &strs(&["-frames:v", "1"]),
            &filter,
            &strs(e.args),
            &strs(&["-f", "null", "-"]),
        ]),
        10_000,
    )
}

/// Software encoder for the one-frame bitstream each decoder probe decodes.
fn sample_encoder(c: VideoCodec) -> (&'static str, &'static str) {
    match c {
        VideoCodec::H264 => ("libx264", "h264"),
        VideoCodec::Hevc => ("libx265", "hevc"),
        VideoCodec::Av1 => ("libsvtav1", "ivf"),
    }
}

pub fn decoder_sample_name(c: VideoCodec, pid: u32) -> String {
    format!(
        "fliks-decoder-probe-{}-{pid}.{}",
        super::codec_str(c),
        sample_encoder(c).1
    )
}

/// [ts:backend/src/modules/streaming/transcoding/codec/decoder-probe.ts:105-150]
pub fn decoder_sample_cmd(c: VideoCodec, path: &str) -> Cmd {
    cmd(
        strs(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "nullsrc=size=320x180:rate=30",
            "-frames:v",
            "1",
            "-c:v",
            sample_encoder(c).0,
            "-pix_fmt",
            "yuv420p",
            path,
        ]),
        10_000,
    )
}

/// [ts:backend/src/modules/streaming/transcoding/codec/decoder-probe.ts:156-190]
pub fn decoder_cmd(dec: &Decoder, d: &Device, sample: &str) -> Cmd {
    let mut a = strs(&["-hide_banner", "-loglevel", "error"]);
    a.extend(dec.input_args(d));
    a.extend(strs(&["-i", sample, "-frames:v", "1"]));
    if dec.gpu_output() {
        a.extend(strs(&["-vf", "hwdownload,format=nv12"]));
    }
    a.extend(strs(&["-f", "null", "-"]));
    cmd(a, 10_000)
}

/// HEVC Main10 PQ/BT.2020 sample shared by the tone-map probes.
/// [ts:backend/src/modules/streaming/transcoding/codec/hdr-probe-sample.ts:9-45]
pub fn hdr_sample_cmd(path: &str) -> Cmd {
    cmd(
        strs(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "nullsrc=size=320x180:rate=30,format=yuv420p10le",
            "-frames:v",
            "8",
            "-c:v",
            "libx265",
            "-color_primaries",
            "bt2020",
            "-color_trc",
            "smpte2084",
            "-colorspace",
            "bt2020nc",
            "-x265-params",
            "hdr-opt=1:repeat-headers=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1):max-cll=1000,400",
            path,
        ]),
        15_000,
    )
}

pub fn hdr_sample_name(probe: &str, pid: u32) -> String {
    format!("fliks-{probe}-probe-{pid}.hevc")
}

pub fn qsv_native_decode_args(d: &Device) -> Vec<String> {
    decoders::qsv_native_hevc(d.platform).input_args(d)
}

pub fn vaapi_decode_args(d: &Device) -> Vec<String> {
    decoders::find("hevc_vaapi_decode").input_args(d)
}

fn tail_enc(enc: &[&str], frames: &str) -> Vec<String> {
    cat(&[
        &strs(&["-c:v"]),
        &strs(enc),
        &strs(&[
            "-preset",
            "veryfast",
            "-frames:v",
            frames,
            "-f",
            "null",
            "-",
        ]),
    ])
}

/// Is the iGPU's fixed-function tone-map LUT usable (`vpp_qsv=tonemap=1`).
/// [ts:backend/src/modules/streaming/transcoding/codec/vpp-qsv-probe.ts:30-54]
pub fn vpp_qsv_cmd(d: &Device, sample: &str) -> Cmd {
    cmd(
        cat(&[
            &strs(QUIET),
            &qsv_native_decode_args(d),
            &strs(&["-i", sample, "-vf"]),
            &strs(&["hwmap=derive_device=qsv,vpp_qsv=tonemap=1:w=320:h=176:format=nv12"]),
            &tail_enc(&["h264_qsv"], "1"),
        ]),
        15_000,
    )
}

/// `tonemap_opencl` on the VAAPI-derived bridge: plain, with a CPU crop prefix, HDR10 target.
/// [ts:backend/src/modules/streaming/transcoding/codec/tonemap-opencl-probe.ts:48-160]
pub fn tonemap_opencl_cmds(hw: HwAccel, d: &Device, sample: &str) -> [Cmd; 3] {
    let qsv = hw == HwAccel::Qsv;
    let decode = if qsv {
        qsv_native_decode_args(d)
    } else {
        vaapi_decode_args(d)
    };
    let base = cat(&[
        &strs(QUIET),
        &decode,
        &strs(&["-init_hw_device", "opencl=ocl:0.0", "-i", sample, "-vf"]),
    ]);
    let reverse = if qsv {
        "hwmap=derive_device=qsv:mode=write:reverse=1:extra_hw_frames=16,format=qsv"
    } else {
        "hwmap=derive_device=vaapi:mode=write:reverse=1,format=vaapi"
    };
    let (h264, hevc) = if qsv {
        ("h264_qsv", "hevc_qsv")
    } else {
        ("h264_vaapi", "hevc_vaapi")
    };
    let sdr = "tonemap_opencl=format=nv12:p=bt709:t=bt709:m=bt709:tonemap=reinhard:desat=0";
    let mk = |vf: String, enc: &[&str]| {
        let mut a = base.clone();
        a.push(vf);
        a.extend(tail_enc(enc, "1"));
        cmd(a, 20_000)
    };
    [
        mk(
            format!(
                "scale_vaapi=w=288:h=160:extra_hw_frames=24,hwmap=derive_device=opencl:mode=read,{sdr},{reverse}"
            ),
            &[h264],
        ),
        mk(
            format!(
                "hwdownload,format=p010le,crop=288:160:16:8,hwupload=derive_device=vaapi,scale_vaapi=w=288:h=160:extra_hw_frames=24,hwmap=derive_device=opencl:mode=read,{sdr},{reverse}"
            ),
            &[h264],
        ),
        mk(
            format!(
                "scale_vaapi=w=288:h=160:extra_hw_frames=24,hwmap=derive_device=opencl:mode=read,tonemap_opencl=format=p010:t=smpte2084:p=bt2020:m=bt2020:r=tv:apply_dovi=1,{reverse}"
            ),
            &[hevc, "-profile:v", "main10"],
        ),
    ]
}

/// Standalone `tonemap_opencl` for NVENC and AMF: `[sdr, hdr10]`.
/// [ts:backend/src/modules/streaming/transcoding/codec/opencl-tonemap-probe.ts:25-72]
pub fn opencl_tonemap_cmds(hw: HwAccel, d: &Device, sample: &str) -> [Cmd; 2] {
    let decode = match hw {
        HwAccel::Nvenc => strs(&["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"]),
        HwAccel::Amf => strs(&["-hwaccel", "d3d11va"]),
        _ => vec![],
    };
    let download = if hw == HwAccel::Nvenc {
        "hwdownload,format=p010le,"
    } else {
        ""
    };
    let mk = |vf: String| {
        cmd(
            cat(&[
                &strs(QUIET),
                &decode,
                &d.opencl_tonemap_init(),
                &strs(&["-i", sample, "-vf", &vf]),
                &strs(ONE_FRAME_NULL),
            ]),
            20_000,
        )
    };
    [
        mk(format!(
            "{download}format=p010le,hwupload,tonemap_opencl=t=bt709:m=bt709:p=bt709:tonemap=hable:desat=0:format=nv12,hwdownload,format=nv12"
        )),
        mk(format!(
            "{download}format=p010le,hwupload,tonemap_opencl=format=p010:t=smpte2084:p=bt2020:m=bt2020:r=tv:apply_dovi=1,hwdownload,format=p010le"
        )),
    ]
}

/// Windows QSV D3D11 to OpenCL bridge: `[sdr, hdr10]`.
/// [ts:backend/src/modules/streaming/transcoding/codec/qsv-opencl-probe.ts:25-98]
pub fn qsv_opencl_cmds(sample: &str) -> [Cmd; 2] {
    let mk = |vf: &str, enc: &[&str]| {
        cmd(
            cat(&[
                &strs(QUIET),
                &strs(&[
                    "-init_hw_device",
                    "d3d11va=dx",
                    "-init_hw_device",
                    "qsv=qs@dx",
                    "-init_hw_device",
                    "opencl=ocl@dx",
                    "-filter_hw_device",
                    "ocl",
                    "-hwaccel",
                    "d3d11va",
                    "-hwaccel_output_format",
                    "d3d11",
                    "-hwaccel_device",
                    "dx",
                    "-i",
                    sample,
                    "-vf",
                    vf,
                ]),
                &tail_enc(enc, "1"),
            ]),
            15_000,
        )
    };
    [
        mk(
            "hwmap=derive_device=qsv,vpp_qsv=w=320:h=176:format=p010le,hwmap=derive_device=opencl,tonemap_opencl=tonemap=hable:t=bt709:m=bt709:p=bt709:format=nv12:desat=0,hwmap=derive_device=qsv:mode=write:reverse=1:extra_hw_frames=16,format=qsv",
            &["h264_qsv"],
        ),
        mk(
            "hwmap=derive_device=qsv,vpp_qsv=w=320:h=176:format=p010le,hwmap=derive_device=opencl,tonemap_opencl=format=p010:t=smpte2084:p=bt2020:m=bt2020:r=tv:apply_dovi=1,hwmap=derive_device=qsv:mode=write:reverse=1:extra_hw_frames=16,format=qsv",
            &["hevc_qsv", "-profile:v", "main10"],
        ),
    ]
}

/// libplacebo (Vulkan) tone-map chain of a no-base Dolby Vision session on VAAPI.
/// [ts:backend/src/modules/streaming/transcoding/codec/vulkan-tonemap-probe.ts:19-56]
pub fn vulkan_cmd(d: &Device, sample: &str) -> Cmd {
    cmd(
        cat(&[
            &strs(QUIET),
            &d.vulkan_init(),
            &strs(&[
                "-hwaccel",
                "vaapi",
                "-hwaccel_output_format",
                "vaapi",
                "-hwaccel_device",
                "va",
                "-i",
                sample,
                "-vf",
                "hwmap=derive_device=drm,format=drm_prime,libplacebo=w=320:h=-2:upscaler=none:downscaler=none:format=bgra:tonemapping=mobius:peak_detect=0:color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=pc:apply_dolbyvision=0,format=vulkan,hwmap=derive_device=vaapi,format=vaapi,scale_vaapi=format=nv12:out_range=tv",
            ]),
            &strs(ONE_FRAME_NULL),
        ]),
        20_000,
    )
}

/// `tonemap_cuda` zero-copy chain: `[sdr, hdr10]`.
/// [ts:backend/src/modules/streaming/transcoding/codec/cuda-tonemap-probe.ts:20-48]
pub fn cuda_cmds() -> [Cmd; 2] {
    let mk = |vf: &str| {
        cmd(
            strs(&[
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "nullsrc=size=320x180:rate=30",
                "-vf",
                vf,
                "-frames:v",
                "1",
                "-f",
                "null",
                "-",
            ]),
            15_000,
        )
    };
    [
        mk(
            "setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc,format=p010le,hwupload_cuda,scale_cuda=w=160:h=-2:format=p010le,tonemap_cuda=format=nv12:p=bt709:t=bt709:m=bt709:tonemap=hable:desat=0:apply_dovi=0,hwdownload,format=nv12",
        ),
        mk(
            "setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc,format=p010le,hwupload_cuda,scale_cuda=w=160:h=-2:format=p010le,tonemap_cuda=format=p010:t=smpte2084:p=bt2020:m=bt2020:r=tv:apply_dovi=1,hwdownload,format=p010le",
        ),
    ]
}

/// AMD D3D11 to OpenCL zero-copy chain: SDR tone-map then 10-bit scale; both must pass.
/// [ts:backend/src/modules/streaming/transcoding/codec/amf-opencl-probe.ts:32-91]
pub fn amf_opencl_cmds(sample: &str) -> [Cmd; 2] {
    let decode = decoders::find("hevc_d3d11va_native_decode").input_args(&Device {
        platform: Platform::Win32,
        render_node: None,
        opencl_device: None,
    });
    let mk = |vf: &str, enc: &[&str]| {
        cmd(
            cat(&[
                &strs(QUIET),
                &decode,
                &strs(&["-i", sample, "-vf", vf, "-c:v"]),
                &strs(enc),
                &strs(&["-frames:v", "2", "-f", "null", "-"]),
            ]),
            15_000,
        )
    };
    [
        mk(
            "hwmap=derive_device=opencl:mode=read,scale_opencl=w=320:h=180,tonemap_opencl=format=nv12:p=bt709:t=bt709:m=bt709:tonemap=mobius:desat=0:apply_dovi=0,hwmap=derive_device=d3d11va:mode=write:reverse=1,format=d3d11",
            &["h264_amf"],
        ),
        mk(
            "hwmap=derive_device=opencl:mode=read,scale_opencl=w=320:h=180:format=p010le,hwmap=derive_device=d3d11va:mode=write:reverse=1,format=d3d11",
            &["hevc_amf", "-profile:v", "main10"],
        ),
    ]
}
