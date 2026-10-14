//! Video encoder argv per descriptor and their `-vf` helpers.
//! [ts:backend/src/modules/streaming/transcoding/codec/encoders/*.ts]
use super::filters::{apply_dovi, curve_name, tonemap_opencl_opts};
use super::{EncoderInput, Filters};
use crate::api::{HdrFormat, HdrMetadata, HwAccel, VideoCodec};
use crate::decide::codec::{Encoder, encoder_by_id};
use crate::decide::tonemap::TonemapPath;
use crate::hw::decoders::Surface;
use crate::jsnum::{js_string as js, round, to_fixed};

const GENERIC_MASTER_DISPLAY: &str =
    "G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1)";
const GENERIC_MAX_CLL: &str = "1000,400";

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// Encoder argv for the descriptor `id`; errors as the host's descriptor throws them.
pub fn encoder_args(id: &str, input: &EncoderInput) -> Result<Vec<String>, String> {
    let e = encoder_by_id(id).ok_or_else(|| format!("unknown encoder {id}"))?;
    build(e, input)
}

/// `-color_*` of an HDR bitstream. [ts:backend/src/modules/streaming/transcoding/codec/encoders/helpers/hdr-variants.ts:8-18]
fn hdr_color_args(f: HdrFormat) -> Vec<String> {
    let trc = if f == HdrFormat::Hlg {
        "arib-std-b67"
    } else {
        "smpte2084"
    };
    s(&[
        "-color_primaries",
        "bt2020",
        "-color_trc",
        trc,
        "-colorspace",
        "bt2020nc",
    ])
}

/// `ceil(ih*W/iw/2)*2`: mod-2 height from the source aspect, as the HW paths' `h=-2`.
fn even_height(w: &str) -> String {
    format!("ceil(ih*{w}/iw/2)*2")
}

fn cpu_chain(f: &Filters, w: &str, tonemap: bool, fmt: &str) -> String {
    let tm = if tonemap { f.tonemap_cpu.as_str() } else { "" };
    format!(
        "{}{tm}scale={w}:{}:flags=lanczos,format={fmt}{}",
        f.cpu_crop_prefix,
        even_height(w),
        f.burn_in_filter
    )
}

pub(crate) fn build(e: &Encoder, i: &EncoderInput) -> Result<Vec<String>, String> {
    use HwAccel::*;
    use VideoCodec::*;
    let t = &i.target;
    let (b, gop) = (js(t.video_bitrate_bps), js(t.gop_size));
    let fkf = i.force_keyframes_expr.clone();
    let ten = e.hdr.is_some();
    let hdr10_args = || hdr_color_args(HdrFormat::Hdr10);
    // Descriptors built with `hlgFromHdr10`: the HDR10 argv with its transfer flipped.
    let hlg_from_hdr10 = matches!(
        (e.hw, e.codec),
        (Qsv | Vaapi | Nvenc | Amf | Videotoolbox, Hevc) | (Amf, Av1)
    ) && e.hdr == Some(HdrFormat::Hlg);
    let gop_tail = |key: bool| {
        let mut v = s(&["-g", &gop]);
        if key {
            v.extend(s(&["-keyint_min", &gop]));
        }
        v.extend(s(&["-force_key_frames", &fkf]));
        v
    };
    let tag = s(&["-tag:v", "hvc1"]);
    let mut a: Vec<String> = match e.hw {
        Qsv => {
            let mut a = s(&["-c:v", &format!("{}_qsv", codec_str(e.codec))]);
            if e.codec == Hevc && ten {
                a.extend(s(&["-profile:v", "main10"]));
            }
            a.extend(s(&["-preset", &i.preset]));
            a.extend(i.qsv.extra.iter().cloned());
            if e.codec != Av1 {
                a.extend(s(&["-mbbrc", "1"]));
            }
            let max = if e.codec == Hevc {
                b.clone()
            } else {
                js(t.video_bitrate_bps + 1.0)
            };
            a.extend(s(&[
                "-b:v",
                &b,
                "-maxrate",
                &max,
                "-rc_init_occupancy",
                &js(i.qsv.rc_init_occupancy),
                "-bufsize",
                &js(i.qsv.bufsize),
                "-vf",
                &(if ten { qsv_10bit(i)? } else { qsv_8bit(i) }),
            ]));
            a.extend(gop_tail(true));
            a
        }
        Vaapi => {
            let mut a = s(&["-c:v", &format!("{}_vaapi", codec_str(e.codec))]);
            if e.codec == Hevc {
                a.extend(s(&["-profile:v", if ten { "2" } else { "1" }]));
            }
            a.extend(s(&[
                "-b:v",
                &b,
                "-maxrate",
                &b,
                "-vf",
                &vaapi_filter(i, ten)?,
            ]));
            a.extend(gop_tail(true));
            a
        }
        Nvenc => {
            let mut a = s(&["-c:v", &format!("{}_nvenc", codec_str(e.codec))]);
            if e.codec == Av1 && ten {
                a.extend(s(&["-pix_fmt", "p010le"]));
            }
            a.extend(s(&["-preset", &i.nvenc_preset]));
            if e.codec == Hevc {
                a.extend(s(&["-profile:v", if ten { "main10" } else { "main" }]));
                if ten {
                    a.extend(s(&["-pix_fmt", "p010le"]));
                }
            }
            a.extend(s(&[
                "-b:v",
                &b,
                "-maxrate",
                &b,
                "-vf",
                &nvenc_filter(i, ten),
            ]));
            a.extend(s(&["-forced-idr", "1"]));
            if e.codec != Av1 {
                a.extend(s(&["-no-scenecut", "1"]));
            }
            a.extend(s(&["-bf", "0"]));
            a.extend(gop_tail(true));
            a
        }
        Amf => {
            let mut a = s(&[
                "-c:v",
                &format!("{}_amf", codec_str(e.codec)),
                "-usage",
                "transcoding",
            ]);
            match (e.codec, ten) {
                (H264, _) => a.extend(s(&["-profile:v", "high"])),
                (Hevc, false) => a.extend(s(&["-profile:v", "main"])),
                (Hevc, true) => a.extend(s(&["-profile:v", "main10"])),
                _ => {}
            }
            if ten && i.input_surface != Surface::D3d11 {
                a.extend(s(&["-pix_fmt", "p010le"]));
            }
            a.extend(s(&[
                "-quality",
                "balanced",
                "-rc",
                "cbr",
                "-b:v",
                &b,
                "-maxrate",
                &b,
                "-vf",
                &amf_filter(i, ten),
                "-forced_idr",
                "1",
                "-bf",
                "0",
            ]));
            a.extend(gop_tail(false));
            a
        }
        Videotoolbox => {
            let w = js(t.width);
            let metal = i.input_surface == Surface::Videotoolbox;
            if ten && i.tonemap {
                return Err("hevc_videotoolbox_main10: tonemap requested but not supported".into());
            }
            let profile = match (e.codec, ten) {
                (H264, _) => "high",
                (_, false) => "main",
                _ => "main10",
            };
            let mut a = s(&[
                "-c:v",
                &format!("{}_videotoolbox", codec_str(e.codec)),
                "-profile:v",
                profile,
            ]);
            if i.early {
                a.extend(s(&["-realtime", "1"]));
            }
            if ten && !metal {
                a.extend(s(&["-pix_fmt", "p010le"]));
            }
            let vf = match (ten, metal) {
                (true, true) => format!("scale_vt=w={w}:h=-2"),
                (true, false) => cpu_chain(&i.filters, &w, false, "p010le"),
                (false, true) => vt_tonemap_filter(i),
                (false, false) => cpu_chain(&i.filters, &w, true, "yuv420p"),
            };
            a.extend(s(&["-b:v", &b, "-maxrate", &b, "-vf", &vf]));
            a.extend(gop_tail(true));
            a
        }
        None => cpu_args(e, i, &b, &fkf, &gop),
    };
    if ten && e.hw != None {
        a.extend(hdr10_args());
    }
    if e.codec == Hevc && e.hw != None {
        a.extend(tag);
    }
    if hlg_from_hdr10 {
        if let Some(k) = a.iter().position(|x| x == "-color_trc") {
            a[k + 1] = "arib-std-b67".into();
        }
    } else if e.hdr == Some(HdrFormat::Hlg) && e.hw != None {
        // av1_nvenc_hlg builds its own HLG tags.
        let k = a.iter().position(|x| x == "-color_trc").unwrap();
        a[k + 1] = "arib-std-b67".into();
    }
    Ok(a)
}

fn codec_str(c: VideoCodec) -> &'static str {
    crate::decide::codec::codec_name(c)
}

/// libx264, libx265, libsvtav1. [ts:backend/src/modules/streaming/transcoding/codec/encoders/h264-cpu.ts:20-57] [ts:backend/src/modules/streaming/transcoding/codec/encoders/hevc-cpu.ts:12-165] [ts:backend/src/modules/streaming/transcoding/codec/encoders/av1-cpu.ts:12-161]
fn cpu_args(e: &Encoder, i: &EncoderInput, b: &str, fkf: &str, gop: &str) -> Vec<String> {
    let w = js(i.target.width);
    let f = &i.filters;
    let gop_tail = s(&["-g", gop, "-keyint_min", gop, "-force_key_frames", fkf]);
    let hlg = e.hdr == Some(HdrFormat::Hlg);
    let fmt = if e.hdr.is_some() {
        "yuv420p10le"
    } else {
        "yuv420p"
    };
    let vf = cpu_chain(f, &w, !hlg, fmt);
    match e.codec {
        VideoCodec::H264 => {
            let mut a = s(&[
                "-c:v",
                "libx264",
                "-threads:v",
                "4",
                "-preset",
                "veryfast",
                "-profile:v",
                "high",
                "-b:v",
                b,
                "-maxrate",
                b,
                "-bufsize",
                &i.libx264_bufsize_mb,
                "-vf",
                &vf,
            ]);
            a.extend(gop_tail);
            a.extend(s(&["-sc_threshold:v:0", "0"]));
            a
        }
        VideoCodec::Hevc => {
            let mut a = s(&["-c:v", "libx265", "-threads:v", "4", "-preset", &i.preset]);
            if e.hdr.is_some() {
                a.extend(s(&["-pix_fmt", "yuv420p10le"]));
            }
            a.extend(s(&[
                "-b:v",
                b,
                "-maxrate",
                b,
                "-bufsize",
                &i.libx264_bufsize_mb,
                "-vf",
                &vf,
            ]));
            a.extend(gop_tail);
            let params = match e.hdr {
                None => "open-gop=0".to_string(),
                Some(HdrFormat::Hdr10) => format!(
                    "hdr-opt=1:repeat-headers=1:open-gop=0:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:master-display={}:max-cll={}",
                    master_display(i.hdr_metadata.as_ref()),
                    max_cll(i.hdr_metadata.as_ref())
                ),
                Some(HdrFormat::Hlg) => "repeat-headers=1:open-gop=0:colorprim=bt2020:transfer=arib-std-b67:colormatrix=bt2020nc".to_string(),
            };
            a.extend(s(&["-x265-params", &params]));
            if let Some(h) = e.hdr {
                a.extend(hdr_color_args(h));
            }
            a.extend(s(&["-tag:v", "hvc1"]));
            a
        }
        VideoCodec::Av1 => {
            let bps = i.target.video_bitrate_bps.max(1.0);
            let mut a = s(&["-c:v", "libsvtav1"]);
            if e.hdr.is_some() {
                a.extend(s(&["-pix_fmt", "yuv420p10le"]));
            }
            a.extend(s(&[
                "-preset",
                svt_preset(&i.preset),
                "-b:v",
                &js(bps),
                "-bufsize",
                &js(round(bps * 2.0)),
            ]));
            match e.hdr {
                Some(HdrFormat::Hdr10) => a.extend(s(&[
                    "-svtav1-params",
                    &format!(
                        "mastering-display={}:content-light={}",
                        svt_master_display(i.hdr_metadata.as_ref()),
                        max_cll(i.hdr_metadata.as_ref())
                    ),
                ])),
                Some(HdrFormat::Hlg) => a.extend(s(&[
                    "-svtav1-params",
                    "color-primaries=9:transfer-characteristics=18:matrix-coefficients=9",
                ])),
                None => {}
            }
            a.extend(s(&["-vf", &vf]));
            a.extend(gop_tail);
            if let Some(h) = e.hdr {
                a.extend(hdr_color_args(h));
            }
            a
        }
    }
}

fn svt_preset(p: &str) -> &'static str {
    match p {
        "veryfast" => "10",
        "faster" => "9",
        "fast" => "7",
        "slow" => "3",
        _ => "5",
    }
}

/// [ts:backend/src/modules/streaming/transcoding/codec/encoders/helpers/hdr-metadata.ts:19-56]
fn master_display(m: Option<&HdrMetadata>) -> String {
    m.map(|m| m.mastering_display.as_str())
        .filter(|d| !d.is_empty())
        .unwrap_or(GENERIC_MASTER_DISPLAY)
        .to_string()
}

fn max_cll(m: Option<&HdrMetadata>) -> String {
    m.map_or(GENERIC_MAX_CLL.into(), |m| {
        format!("{},{}", m.max_cll, m.max_fall)
    })
}

/// `G(gx,gy)B(..)R(..)WP(..)L(max,min)` integers, else `None`.
fn parse_master_display(raw: &str) -> Option<Vec<f64>> {
    let mut out = Vec::new();
    let mut rest = raw;
    for key in ["G(", "B(", "R(", "WP(", "L("] {
        rest = rest.strip_prefix(key)?;
        let (inner, tail) = rest.split_once(')')?;
        let (a, b) = inner.split_once(',')?;
        for n in [a, b] {
            if n.is_empty() || !n.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            out.push(n.parse().ok()?);
        }
        rest = tail;
    }
    rest.is_empty().then_some(out)
}

/// SVT-AV1 wants chromaticities / 50000 and luminance / 10000 as decimals.
fn svt_master_display(m: Option<&HdrMetadata>) -> String {
    let v = parse_master_display(&master_display(m))
        .or_else(|| parse_master_display(GENERIC_MASTER_DISPLAY))
        .unwrap();
    let d = |x: f64, by: f64| js(to_fixed(x / by, 5).parse::<f64>().unwrap());
    let c = |k: usize| format!("{},{}", d(v[k], 50000.0), d(v[k + 1], 50000.0));
    format!(
        "G({})B({})R({})WP({})L({},{})",
        c(0),
        c(2),
        c(4),
        c(6),
        d(v[8], 10000.0),
        d(v[9], 10000.0)
    )
}

const QSV_HWMAP: &str = "hwmap=derive_device=qsv,";

fn qsv_crop_opts(i: &EncoderInput) -> String {
    let parsed = i
        .has_crop
        .then(|| i.filters.crop_str.strip_prefix("crop="))
        .flatten()
        .and_then(|r| {
            let p: Vec<&str> = r.split(':').collect();
            (p.len() == 4
                && p.iter()
                    .all(|x| !x.is_empty() && x.bytes().all(|c| c.is_ascii_digit())))
            .then(|| {
                p.iter()
                    .map(|x| js(x.parse::<f64>().unwrap()))
                    .collect::<Vec<_>>()
            })
        });
    parsed.map_or(String::new(), |p| {
        format!("cw={}:ch={}:cx={}:cy={}:", p[0], p[1], p[2], p[3])
    })
}

/// The qsv-native OpenCL tone-map bounce; Windows maps D3D11 surfaces without `mode=read`.
fn qsv_native_opencl(i: &EncoderInput, w: &str, h: &str, hdr10: bool) -> String {
    let opts = tonemap_opencl_opts(hdr10, curve_name(i.tonemap_curve), i.dv_apply_rpu);
    let read = if i.input_surface == Surface::D3d11 {
        ""
    } else {
        ":mode=read"
    };
    format!(
        "{QSV_HWMAP}vpp_qsv={}w={w}:h={h}:format=p010le,hwmap=derive_device=opencl{read},tonemap_opencl={opts},hwmap=derive_device=qsv:mode=write:reverse=1:extra_hw_frames=16,format=qsv",
        qsv_crop_opts(i)
    )
}

/// [ts:backend/src/modules/streaming/transcoding/codec/encoders/helpers/qsv-filters.ts:30-94]
fn qsv_8bit(i: &EncoderInput) -> String {
    let (w, h, f) = (js(i.target.width), js(i.target.height), &i.filters);
    if i.qsv_native == Some(true) {
        if i.tonemap && i.tonemap_path == TonemapPath::Opencl {
            return qsv_native_opencl(i, &w, &h, false);
        }
        let tm = if i.tonemap { "tonemap=1:" } else { "" };
        return format!(
            "{QSV_HWMAP}vpp_qsv={tm}{}w={w}:h={h}:format=nv12",
            qsv_crop_opts(i)
        );
    }
    let cpu = i.input_surface == Surface::Cpu;
    let crop = if cpu {
        &f.cpu_crop_prefix
    } else {
        &f.hw_crop_prefix
    };
    let up_fmt = if !f.tonemap_vaapi.is_empty() || !f.tonemap_opencl.is_empty() {
        "p010le"
    } else {
        "nv12"
    };
    let upload = if cpu {
        format!("format={up_fmt},hwupload=derive_device=vaapi,")
    } else {
        String::new()
    };
    let burn = if i.has_burn_in {
        format!(
            ",hwdownload,format=nv12{},hwupload=extra_hw_frames=16",
            f.burn_in_filter
        )
    } else {
        String::new()
    };
    if !f.tonemap_vaapi.is_empty() {
        let head = format!(
            "{crop}{upload}scale_vaapi=w={w}:h=-2:extra_hw_frames=24{}",
            f.tonemap_vaapi
        );
        return if i.has_burn_in {
            format!("{head}{burn}")
        } else {
            format!("{head},hwmap=derive_device=qsv,vpp_qsv=format=nv12:passthrough=0")
        };
    }
    if !f.tonemap_opencl.is_empty() {
        return format!(
            "{crop}{upload}scale_vaapi=w={w}:h=-2:extra_hw_frames=24{},hwmap=derive_device=qsv:mode=write:reverse=1:extra_hw_frames=16,format=qsv{burn}",
            f.tonemap_opencl
        );
    }
    format!(
        "{crop}{upload}scale_vaapi=w={w}:h=-2:format=nv12:extra_hw_frames=24,hwmap=derive_device=qsv,format=qsv{burn}"
    )
}

/// [ts:backend/src/modules/streaming/transcoding/codec/encoders/helpers/qsv-filters.ts:96-145]
fn qsv_10bit(i: &EncoderInput) -> Result<String, String> {
    let (w, h, f) = (js(i.target.width), js(i.target.height), &i.filters);
    if i.qsv_native == Some(true) {
        if i.tonemap && i.tonemap_path == TonemapPath::Opencl {
            return Ok(qsv_native_opencl(i, &w, &h, true));
        }
        if i.tonemap {
            return Err("qsvScaleFilter10bit: tonemap requested with no opencl chain on a qsv-native surface".into());
        }
        return Ok(format!(
            "{QSV_HWMAP}vpp_qsv={}w={w}:h={h}:format=p010le",
            qsv_crop_opts(i)
        ));
    }
    let cpu = i.input_surface == Surface::Cpu;
    let crop = if cpu {
        &f.cpu_crop_prefix
    } else {
        &f.hw_crop_prefix
    };
    let upload = if cpu {
        "format=p010le,hwupload=derive_device=vaapi,"
    } else {
        ""
    };
    let burn = if i.has_burn_in {
        format!(
            ",hwdownload,format=p010le{},hwupload=extra_hw_frames=16",
            f.burn_in_filter
        )
    } else {
        String::new()
    };
    if !f.tonemap_opencl.is_empty() {
        return Ok(format!(
            "{crop}{upload}scale_vaapi=w={w}:h=-2:extra_hw_frames=24{},hwmap=derive_device=qsv:mode=write:reverse=1:extra_hw_frames=16,format=qsv{burn}",
            f.tonemap_opencl
        ));
    }
    if i.tonemap {
        return Err("qsvScaleFilter10bit: tonemap requested with no opencl chain on a vaapi-derived surface".into());
    }
    Ok(format!(
        "{crop}{upload}scale_vaapi=w={w}:h=-2:format=p010le:extra_hw_frames=24,hwmap=derive_device=qsv,format=qsv{burn}"
    ))
}

/// [ts:backend/src/modules/streaming/transcoding/codec/encoders/helpers/vaapi-filters.ts:5-26]
fn vaapi_filter(i: &EncoderInput, ten: bool) -> Result<String, String> {
    let (w, f) = (js(i.target.width), &i.filters);
    let fmt = if ten { "p010le" } else { "nv12" };
    let burn = if i.has_burn_in {
        format!(
            ",hwdownload,format={fmt}{},hwupload=derive_device=vaapi:extra_hw_frames=16",
            f.burn_in_filter
        )
    } else {
        String::new()
    };
    if !ten && !f.tonemap_vulkan.is_empty() {
        return Ok(f.tonemap_vulkan.clone());
    }
    if !ten && !f.tonemap_vaapi.is_empty() {
        return Ok(format!(
            "{}scale_vaapi=w={w}:h=-2:extra_hw_frames=24{}{burn}",
            f.hw_crop_prefix, f.tonemap_vaapi
        ));
    }
    if !f.tonemap_opencl.is_empty() {
        return Ok(format!(
            "{}scale_vaapi=w={w}:h=-2:extra_hw_frames=24{},hwmap=derive_device=vaapi:mode=write:reverse=1,format=vaapi{burn}",
            f.hw_crop_prefix, f.tonemap_opencl
        ));
    }
    if ten && i.tonemap {
        return Err("vaapiScaleFilter10bit: tonemap requested with no opencl chain".into());
    }
    Ok(format!(
        "{}scale_vaapi=w={w}:h=-2:format={fmt}{burn}",
        f.hw_crop_prefix
    ))
}

/// Whether the NVENC chain ends on a CUDA surface. [ts:backend/src/modules/streaming/transcoding/codec/encoders/helpers/nvenc-filters.ts:7-8]
pub(crate) fn nvenc_ends_on_gpu(i: &EncoderInput) -> bool {
    if i.tonemap {
        !i.filters.tonemap_cuda.is_empty()
    } else {
        i.input_surface == Surface::Cuda
    }
}

/// [ts:backend/src/modules/streaming/transcoding/codec/encoders/helpers/nvenc-filters.ts:12-49]
fn nvenc_filter(i: &EncoderInput, ten: bool) -> String {
    let (w, f) = (js(i.target.width), &i.filters);
    let fmt = if ten { "p010le" } else { "nv12" };
    let sw = if ten { "p010le" } else { "yuv420p" };
    let crop = &f.cpu_crop_prefix;
    let vf = if i.tonemap && !f.tonemap_cuda.is_empty() {
        let upload = match i.input_surface {
            Surface::Cuda if !i.has_crop => String::new(),
            Surface::Cpu => format!("{crop}format=p010le,hwupload_cuda,"),
            _ => format!("hwdownload,format=p010le,{crop}hwupload_cuda,"),
        };
        format!(
            "{upload}scale_cuda=w={w}:h=-2:format=p010le{}",
            f.tonemap_cuda
        )
    } else if i.tonemap {
        let down = if i.input_surface == Surface::Cpu {
            ""
        } else {
            "hwdownload,format=p010le,"
        };
        format!("{down}{crop}{}scale={w}:{}", f.tonemap_cpu, even_height(&w))
    } else if i.input_surface == Surface::Cuda {
        let crop = if i.has_crop {
            format!("hwdownload,format={fmt},{},hwupload_cuda,", f.crop_str)
        } else {
            String::new()
        };
        format!("{crop}scale_cuda=w={w}:h=-2:format={fmt}")
    } else {
        let down = if i.input_surface == Surface::Cpu {
            String::new()
        } else {
            format!("hwdownload,format={fmt},")
        };
        format!(
            "{down}{crop}scale={w}:{}:flags=lanczos,format={sw}",
            even_height(&w)
        )
    };
    if !i.has_burn_in {
        return vf;
    }
    let bounce = if nvenc_ends_on_gpu(i) {
        format!(",hwdownload,format={fmt}")
    } else {
        String::new()
    };
    format!("{vf}{bounce}{}", f.burn_in_filter)
}

/// Whether the AMF chain ends on a D3D11 surface. [ts:backend/src/modules/streaming/transcoding/codec/encoders/helpers/amf-filters.ts:11-12]
pub(crate) fn amf_ends_on_gpu(i: &EncoderInput) -> bool {
    i.input_surface == Surface::D3d11 && !i.has_burn_in
}

/// [ts:backend/src/modules/streaming/transcoding/codec/encoders/helpers/amf-filters.ts:17-85]
fn amf_filter(i: &EncoderInput, ten: bool) -> String {
    let (w, f) = (js(i.target.width), &i.filters);
    let fmt = if ten { "p010le" } else { "nv12" };
    if i.input_surface == Surface::D3d11 {
        let crop = if f.crop_str.is_empty() {
            String::new()
        } else {
            format!("{},", f.crop_str)
        };
        let sar = if f.crop_str.is_empty() {
            ""
        } else {
            ":reset_sar=1"
        };
        let scale = format!("scale_opencl=w={w}:h={}{sar}", js(i.target.height));
        let step = if i.tonemap {
            format!(
                "{scale},tonemap_opencl={}",
                tonemap_opencl_opts(ten, curve_name(i.tonemap_curve), i.dv_apply_rpu)
            )
        } else {
            format!("{scale}:format={fmt}")
        };
        if i.has_burn_in && !f.burn_in_filter.is_empty() {
            return format!(
                "hwmap=derive_device=opencl:mode=read,{crop}{step},hwdownload,format={fmt}{}",
                f.burn_in_filter
            );
        }
        return format!(
            "hwmap=derive_device=opencl:mode=read,{crop}{step},hwmap=derive_device=d3d11va:mode=write:reverse=1,format=d3d11"
        );
    }
    let down = match i.input_surface {
        Surface::Cpu => "",
        _ if ten || i.tonemap => "hwdownload,format=p010le,",
        _ => "hwdownload,format=nv12,",
    };
    let tm = if i.tonemap {
        f.tonemap_cpu.as_str()
    } else {
        ""
    };
    format!(
        "{down}{}{tm}scale={w}:{}:flags=lanczos,format={fmt}{}",
        f.cpu_crop_prefix,
        even_height(&w),
        f.burn_in_filter
    )
}

/// The VideoToolbox Metal path. [ts:backend/src/modules/streaming/transcoding/codec/encoders/helpers/vt-filters.ts:8-22]
fn vt_tonemap_filter(i: &EncoderInput) -> String {
    let w = js(i.target.width);
    let tm = format!(
        "tonemap_videotoolbox=tonemap={}:t=bt709:m=bt709:p=bt709:range=tv:{}",
        curve_name(i.tonemap_curve),
        apply_dovi(i.dv_apply_rpu)
    );
    if i.has_crop {
        return format!(
            "{tm},hwdownload,format=p010le,{}scale={w}:{}:flags=lanczos,format=yuv420p",
            i.filters.cpu_crop_prefix,
            even_height(&w)
        );
    }
    if i.dv_apply_rpu {
        format!("scale_vt=w={w}:h=-2,{tm}:format=nv12")
    } else {
        format!("scale_vt=w={w}:h=-2:color_matrix=bt709:color_primaries=bt709:color_transfer=bt709")
    }
}
