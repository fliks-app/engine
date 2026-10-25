//! The `-vf` pieces of the filter graph and the bitmap burn-in composite.
//! [ts:backend/src/modules/streaming/transcoding/ffmpeg-filter-graph.ts:1-176]
use super::Filters;
use crate::api::{CropInfo, HwAccel, TonemapCurve};
use crate::hw::GpuDeint;

/// The bare zscale chain leaks HDR10 static side data into SDR output.
const HDR_STATIC_SIDEDATA_DELETE: &str = "sidedata=mode=delete:type=MASTERING_DISPLAY_METADATA,sidedata=mode=delete:type=CONTENT_LIGHT_LEVEL,sidedata=mode=delete:type=DYNAMIC_HDR_PLUS";

pub fn curve_name(c: Option<TonemapCurve>) -> &'static str {
    match c.unwrap_or(TonemapCurve::Mobius) {
        TonemapCurve::Hable => "hable",
        TonemapCurve::Mobius => "mobius",
        TonemapCurve::Reinhard => "reinhard",
    }
}

/// The bundled ffmpeg defaults `apply_dovi` to 1, so every other source passes 0.
pub fn apply_dovi(apply: bool) -> String {
    format!("apply_dovi={}", u8::from(apply))
}

/// Shared `tonemap_opencl`/`tonemap_cuda` options; an HDR10 target reshapes to PQ without a curve.
pub fn tonemap_opencl_opts(hdr10_target: bool, curve: &str, apply_rpu: bool) -> String {
    if hdr10_target {
        format!(
            "format=p010:t=smpte2084:p=bt2020:m=bt2020:r=tv:{}",
            apply_dovi(apply_rpu)
        )
    } else {
        format!(
            "format=nv12:p=bt709:t=bt709:m=bt709:tonemap={curve}:desat=0:{}",
            apply_dovi(apply_rpu)
        )
    }
}

pub fn crop_filter(c: &CropInfo) -> String {
    format!("crop={}:{}:{}:{}", c.width, c.height, c.x, c.y)
}

/// One output frame per frame (the frame rate is kept); progressive frames pass untouched.
const DEINTERLACE: &str = "bwdif=mode=send_frame:parity=auto:deint=interlaced";

pub struct FilterContext<'a> {
    pub deinterlace: bool,
    /// The GPU deinterlacer of the chain's surface, when the host has one.
    pub gpu_deint: Option<GpuDeint>,
    pub crop: Option<&'a CropInfo>,
    /// `burnIn.filter`: a text track's libass step.
    pub burn_in_filter: Option<&'a str>,
    pub tonemap: bool,
    pub use_vaapi_tonemap: bool,
    pub use_vulkan_tonemap: bool,
    pub source_bit_depth: u32,
    pub dv_no_base: bool,
    pub dv_apply_rpu: bool,
    pub curve: Option<TonemapCurve>,
    pub opencl_tonemap: bool,
    pub cuda_tonemap: bool,
    pub scale_width: String,
    pub scale_height: String,
    pub hdr10_target: bool,
}

pub fn video_filters(c: &FilterContext) -> Filters {
    let curve = curve_name(c.curve);
    let dovi = apply_dovi(c.dv_apply_rpu);
    let crop_str = c.crop.map(crop_filter).unwrap_or_default();
    let text = c.burn_in_filter.filter(|f| !f.is_empty());
    let w = &c.scale_width;
    let tonemap_opencl = if c.tonemap && !c.use_vaapi_tonemap && !c.use_vulkan_tonemap {
        format!(
            ",hwmap=derive_device=opencl:mode=read,tonemap_opencl={}",
            tonemap_opencl_opts(c.hdr10_target, curve, c.dv_apply_rpu)
        )
    } else {
        String::new()
    };
    let crop_fmt = if c.source_bit_depth == 10 {
        "p010le"
    } else {
        "nv12"
    };
    let cpu_deint = c.deinterlace && c.gpu_deint.is_none();
    let deint_gpu = c
        .gpu_deint
        .and_then(GpuDeint::filter)
        .map_or(String::new(), |f| format!("{f},"));
    let vulkan_deint = match c.gpu_deint {
        _ if !c.deinterlace => String::new(),
        Some(_) => deint_gpu.clone(),
        None => format!("hwdownload,format={crop_fmt},{DEINTERLACE},hwupload=derive_device=vaapi,"),
    };
    let tonemap_vulkan = match c.crop {
        _ if !c.use_vulkan_tonemap || text.is_some() => String::new(),
        crop => format!(
            "{vulkan_deint}hwmap=derive_device=drm,format=drm_prime,libplacebo={}w={w}:h={}:upscaler=none:downscaler=none:format=bgra:tonemapping={curve}:peak_detect=0:color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=pc:apply_dolbyvision={},format=vulkan,hwmap=derive_device=vaapi,format=vaapi,scale_vaapi=format=nv12:out_range=tv",
            crop.map_or(String::new(), |k| format!(
                "crop_w={}:crop_h={}:crop_x={}:crop_y={}:",
                k.width, k.height, k.x, k.y
            )),
            c.scale_height,
            u8::from(c.dv_apply_rpu)
        ),
    };
    let tonemap_cpu = match () {
        _ if !c.tonemap => String::new(),
        _ if c.opencl_tonemap && c.hdr10_target => format!(
            "format=p010le,hwupload,tonemap_opencl=t=smpte2084:m=bt2020:p=bt2020:r=tv:{dovi}:format=p010,hwdownload,format=p010le,"
        ),
        _ if c.opencl_tonemap => format!(
            "format=p010le,hwupload,tonemap_opencl=t=bt709:m=bt709:p=bt709:tonemap={curve}:desat=0:{dovi}:format=nv12,hwdownload,format=nv12,"
        ),
        _ if c.dv_no_base && c.hdr10_target => format!(
            "scale={w}:-2,tonemapx=t=smpte2084:p=bt2020:m=bt2020:format=yuv420p10le:{},",
            apply_dovi(c.dv_no_base)
        ),
        _ if c.dv_no_base => format!(
            "scale={w}:-2,tonemapx=t=bt709:m=bt709:p=bt709:tonemap={curve}:desat=0:format=yuv420p,"
        ),
        _ => format!(
            "zscale=w={w}:h=-2:t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap={curve}:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p,{HDR_STATIC_SIDEDATA_DELETE},"
        ),
    };
    let cpu_prefix = format!(
        "{}{}",
        if cpu_deint {
            format!("{DEINTERLACE},")
        } else {
            String::new()
        },
        if crop_str.is_empty() {
            String::new()
        } else {
            format!("{crop_str},")
        }
    );
    Filters {
        cpu_crop_prefix: cpu_prefix.clone(),
        hw_crop_prefix: format!(
            "{}{}",
            if c.gpu_deint == Some(GpuDeint::Vaapi) {
                deint_gpu.as_str()
            } else {
                ""
            },
            if cpu_prefix.is_empty() {
                String::new()
            } else {
                format!("hwdownload,format={crop_fmt},{cpu_prefix}hwupload=derive_device=vaapi,")
            }
        ),
        deint_gpu,
        qsv_deint: c.gpu_deint.map_or("", GpuDeint::vpp_opts).into(),
        cpu_deinterlace: cpu_deint,
        burn_in_filter: text.map_or(String::new(), |f| format!(",{f}")),
        tonemap_vaapi: if c.use_vaapi_tonemap {
            ",tonemap_vaapi=format=nv12:t=bt709:p=bt709:m=bt709".into()
        } else {
            String::new()
        },
        tonemap_cuda: if c.tonemap && c.cuda_tonemap {
            format!(
                ",tonemap_cuda={}",
                tonemap_opencl_opts(c.hdr10_target, curve, c.dv_apply_rpu)
            )
        } else {
            String::new()
        },
        crop_str,
        tonemap_vulkan,
        tonemap_opencl,
        tonemap_cpu,
    }
}

pub struct Overlay<'a> {
    pub hw: HwAccel,
    pub video_filter: &'a str,
    pub stream_index: u32,
    pub video_stream_index: Option<u32>,
    pub width: &'a str,
    pub height: &'a str,
    pub bit_depth: u32,
    pub crop: Option<&'a CropInfo>,
    pub frames_on_gpu: bool,
    pub opencl_filter_device: bool,
}

/// Bitmap subtitles composite on CPU frames, the encoder's chain kept as is (output pad `[vout]`).
/// [ts:backend/src/modules/streaming/transcoding/subtitle-overlay-filter.ts:19-106]
pub fn image_burn_in(o: &Overlay) -> String {
    let ten = o.bit_depth >= 10;
    let input = format!(
        "[0:{}]",
        o.video_stream_index
            .map_or("v".to_string(), |i| i.to_string())
    );
    let video = if o.video_filter.is_empty() {
        format!("{input}null")
    } else {
        format!("{input}{}", o.video_filter)
    };
    let whiten = if ten {
        ""
    } else {
        ",colorlevels=rimax=0.6:gimax=0.6:bimax=0.6"
    };
    let sub_crop = o
        .crop
        .map_or(String::new(), |c| format!("{},", crop_filter(c)));
    let sub = format!(
        "[0:{}]{sub_crop}scale={}:{}:flags=lanczos,format=rgba{whiten}[s]",
        o.stream_index, o.width, o.height
    );
    let hw_fmt = if ten { "p010le" } else { "nv12" };
    if matches!(o.hw, HwAccel::None | HwAccel::Videotoolbox) || !o.frames_on_gpu {
        return if ten {
            format!("{video}[v];{sub};[v][s]overlay[vout]")
        } else {
            format!("{video},format=rgb24[v];{sub};[v][s]overlay[ov];[ov]format=yuv420p[vout]")
        };
    }
    let upload = match o.hw {
        HwAccel::Nvenc => "hwupload_cuda".to_string(),
        HwAccel::Vaapi => "hwupload=derive_device=vaapi:extra_hw_frames=16".to_string(),
        hw if o.opencl_filter_device => format!(
            "hwupload=derive_device={}:extra_hw_frames=16",
            if hw == HwAccel::Amf { "d3d11va" } else { "qsv" }
        ),
        _ => "hwupload=extra_hw_frames=16".to_string(),
    };
    let rgb = if ten { "" } else { ",format=rgb24" };
    format!(
        "{video},hwdownload,format={hw_fmt}{rgb}[v];{sub};[v][s]overlay[ov];[ov]format={hw_fmt},{upload}[vout]"
    )
}
