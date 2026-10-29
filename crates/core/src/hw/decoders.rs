//! Decoder descriptors and the argv slice each puts before `-i`.
//! [ts:backend/src/modules/streaming/transcoding/codec/decoders/index.ts:30-125]
use super::device::Device;
use crate::api::{HwAccel, Platform, VideoCodec};
use crate::strs;

/// Frame memory between decode and encode. [ts:backend/src/modules/streaming/transcoding/codec/decoders/types.ts:20]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Surface {
    Cpu,
    Vaapi,
    Cuda,
    D3d11,
    Videotoolbox,
}

/// A source codec some decoder reads: the output codecs plus the legacy inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecodeCodec {
    H264,
    Hevc,
    Av1,
    Vp9,
    Mpeg2,
    Vc1,
}

impl From<VideoCodec> for DecodeCodec {
    fn from(c: VideoCodec) -> Self {
        match c {
            VideoCodec::H264 => Self::H264,
            VideoCodec::Hevc => Self::Hevc,
            VideoCodec::Av1 => Self::Av1,
        }
    }
}

impl DecodeCodec {
    /// ffprobe codec name, aliases collapsed.
    pub fn from_source(name: &str) -> Option<Self> {
        match name.to_lowercase().as_str() {
            "h264" | "avc" | "avc1" => Some(Self::H264),
            "hevc" | "h265" | "hvc1" | "hev1" => Some(Self::Hevc),
            "av1" | "av01" => Some(Self::Av1),
            "vp9" | "vp09" => Some(Self::Vp9),
            "mpeg2video" | "mpeg2" => Some(Self::Mpeg2),
            "vc1" => Some(Self::Vc1),
            _ => None,
        }
    }

    /// The prefix of every decoder id of this codec.
    pub fn name(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Av1 => "av1",
            Self::Vp9 => "vp9",
            Self::Mpeg2 => "mpeg2",
            Self::Vc1 => "vc1",
        }
    }

    /// The output codec this source codec is also, when it is one.
    pub fn video_codec(self) -> Option<VideoCodec> {
        match self {
            Self::H264 => Some(VideoCodec::H264),
            Self::Hevc => Some(VideoCodec::Hevc),
            Self::Av1 => Some(VideoCodec::Av1),
            _ => None,
        }
    }

    pub fn max_bit_depth(self) -> u32 {
        match self {
            Self::H264 | Self::Mpeg2 | Self::Vc1 => 8,
            _ => 10,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Kind {
    Cpu,
    Vaapi,
    QsvVaapi,
    QsvD3d11,
    Cuda,
    D3d11Cpu,
    D3d11Native,
    Vt,
}

pub(crate) struct Decoder {
    pub id: &'static str,
    /// `None` is the software path, which decodes any codec.
    pub codec: Option<DecodeCodec>,
    pub kind: Kind,
}

impl Decoder {
    /// Descriptor `supports()`. [ts:backend/src/modules/streaming/transcoding/codec/decoders/qsv.ts:21]
    pub fn supported(&self, p: Platform) -> bool {
        match self.kind {
            Kind::Cpu | Kind::Vaapi | Kind::Cuda => true,
            Kind::QsvVaapi => p != Platform::Win32,
            Kind::QsvD3d11 | Kind::D3d11Cpu | Kind::D3d11Native => p == Platform::Win32,
            Kind::Vt => p == Platform::Darwin,
        }
    }

    /// Frames stay on the GPU, so the probe downloads one. [ts:backend/src/modules/streaming/transcoding/codec/decoder-probe.ts:156-158]
    pub fn gpu_output(&self) -> bool {
        matches!(
            self.kind,
            Kind::Vaapi | Kind::QsvVaapi | Kind::QsvD3d11 | Kind::Cuda | Kind::D3d11Native
        )
    }

    /// The backend the registry pairs it with. [ts:backend/src/modules/streaming/transcoding/codec/decoders/qsv.ts:8-70]
    pub fn hw(&self) -> HwAccel {
        match self.kind {
            Kind::Cpu => HwAccel::None,
            Kind::Vaapi => HwAccel::Vaapi,
            Kind::QsvVaapi | Kind::QsvD3d11 => HwAccel::Qsv,
            Kind::Cuda => HwAccel::Nvenc,
            Kind::D3d11Cpu | Kind::D3d11Native => HwAccel::Amf,
            Kind::Vt => HwAccel::Videotoolbox,
        }
    }

    pub fn max_bit_depth(&self) -> u32 {
        self.codec.map_or(10, DecodeCodec::max_bit_depth)
    }

    /// Where decoded frames land; VideoToolbox reports CPU, the argv opts into its surface.
    pub fn surface(&self) -> Surface {
        match self.kind {
            Kind::Cpu | Kind::D3d11Cpu | Kind::Vt => Surface::Cpu,
            Kind::Vaapi | Kind::QsvVaapi => Surface::Vaapi,
            Kind::QsvD3d11 | Kind::D3d11Native => Surface::D3d11,
            Kind::Cuda => Surface::Cuda,
        }
    }

    pub fn input_args(&self, d: &Device) -> Vec<String> {
        let tail = ["-extra_hw_frames", "32", "-noautorotate"];
        match self.kind {
            Kind::Cpu => vec![],
            Kind::Vaapi => d
                .vaapi_init()
                .into_iter()
                .chain(strs(&[
                    "-filter_hw_device",
                    "va",
                    "-hwaccel",
                    "vaapi",
                    "-hwaccel_output_format",
                    "vaapi",
                    "-hwaccel_device",
                    "va",
                ]))
                .chain(strs(&tail))
                .collect(),
            Kind::QsvVaapi => d
                .qsv_init()
                .into_iter()
                .chain(strs(&[
                    "-filter_hw_device",
                    "qs",
                    "-hwaccel",
                    "vaapi",
                    "-hwaccel_output_format",
                    "vaapi",
                    "-hwaccel_device",
                    "va",
                ]))
                .chain(strs(&tail))
                .collect(),
            Kind::QsvD3d11 => Device::qsv_via_d3d11_init()
                .into_iter()
                .chain(strs(&[
                    "-filter_hw_device",
                    "qs",
                    "-hwaccel",
                    "d3d11va",
                    "-hwaccel_output_format",
                    "d3d11",
                    "-hwaccel_device",
                    "dx",
                ]))
                .chain(strs(&tail))
                .collect(),
            Kind::Cuda => strs(&[
                "-hwaccel",
                "cuda",
                "-hwaccel_output_format",
                "cuda",
                "-threads",
                "1",
                "-noautorotate",
            ]),
            Kind::D3d11Cpu => strs(&["-hwaccel", "d3d11va", "-noautorotate"]),
            Kind::D3d11Native => Device::amf_d3d11_opencl_init()
                .into_iter()
                .chain(strs(&[
                    "-hwaccel",
                    "d3d11va",
                    "-hwaccel_output_format",
                    "d3d11",
                    "-hwaccel_device",
                    "dx",
                ]))
                .chain(strs(&tail))
                .collect(),
            Kind::Vt => {
                let mut a = strs(&["-hwaccel", "videotoolbox"]);
                // ffmpeg defaults AV1 to libdav1d, which ignores the hwaccel.
                if self.codec == Some(DecodeCodec::Av1) {
                    a.extend(strs(&["-c:v", "av1"]));
                }
                a.push("-noautorotate".into());
                a
            }
        }
    }
}

use DecodeCodec::{Av1, H264, Hevc, Mpeg2, Vc1, Vp9};
use Kind as K;

const fn dec(id: &'static str, codec: Option<DecodeCodec>, kind: Kind) -> Decoder {
    Decoder { id, codec, kind }
}

/// Registry order, then the Windows QSV/AMF D3D11 variants the registry only looks up.
pub(crate) const DECODERS: &[Decoder] = &[
    dec("h264_qsv_decode", Some(H264), K::QsvVaapi),
    dec("hevc_qsv_decode", Some(Hevc), K::QsvVaapi),
    dec("av1_qsv_decode", Some(Av1), K::QsvVaapi),
    dec("vp9_qsv_decode", Some(Vp9), K::QsvVaapi),
    dec("mpeg2_qsv_decode", Some(Mpeg2), K::QsvVaapi),
    dec("vc1_qsv_decode", Some(Vc1), K::QsvVaapi),
    dec("h264_vaapi_decode", Some(H264), K::Vaapi),
    dec("hevc_vaapi_decode", Some(Hevc), K::Vaapi),
    dec("av1_vaapi_decode", Some(Av1), K::Vaapi),
    dec("vp9_vaapi_decode", Some(Vp9), K::Vaapi),
    dec("mpeg2_vaapi_decode", Some(Mpeg2), K::Vaapi),
    dec("vc1_vaapi_decode", Some(Vc1), K::Vaapi),
    dec("h264_cuda_decode", Some(H264), K::Cuda),
    dec("hevc_cuda_decode", Some(Hevc), K::Cuda),
    dec("av1_cuda_decode", Some(Av1), K::Cuda),
    dec("vp9_cuda_decode", Some(Vp9), K::Cuda),
    dec("mpeg2_cuda_decode", Some(Mpeg2), K::Cuda),
    dec("vc1_cuda_decode", Some(Vc1), K::Cuda),
    dec("h264_d3d11va_decode", Some(H264), K::D3d11Cpu),
    dec("hevc_d3d11va_decode", Some(Hevc), K::D3d11Cpu),
    dec("av1_d3d11va_decode", Some(Av1), K::D3d11Cpu),
    dec("vp9_d3d11va_decode", Some(Vp9), K::D3d11Cpu),
    dec("mpeg2_d3d11va_decode", Some(Mpeg2), K::D3d11Cpu),
    dec("vc1_d3d11va_decode", Some(Vc1), K::D3d11Cpu),
    dec("h264_videotoolbox_decode", Some(H264), K::Vt),
    dec("hevc_videotoolbox_decode", Some(Hevc), K::Vt),
    dec("av1_videotoolbox_decode", Some(Av1), K::Vt),
    dec("vp9_videotoolbox_decode", Some(Vp9), K::Vt),
    dec("mpeg2_videotoolbox_decode", Some(Mpeg2), K::Vt),
    dec("cpu", None, K::Cpu),
    dec("h264_qsv_d3d11_decode", Some(H264), K::QsvD3d11),
    dec("hevc_qsv_d3d11_decode", Some(Hevc), K::QsvD3d11),
    dec("av1_qsv_d3d11_decode", Some(Av1), K::QsvD3d11),
    dec("vp9_qsv_d3d11_decode", Some(Vp9), K::QsvD3d11),
    dec("mpeg2_qsv_d3d11_decode", Some(Mpeg2), K::QsvD3d11),
    dec("vc1_qsv_d3d11_decode", Some(Vc1), K::QsvD3d11),
    dec("h264_d3d11va_native_decode", Some(H264), K::D3d11Native),
    dec("hevc_d3d11va_native_decode", Some(Hevc), K::D3d11Native),
    dec("av1_d3d11va_native_decode", Some(Av1), K::D3d11Native),
    dec("vp9_d3d11va_native_decode", Some(Vp9), K::D3d11Native),
    dec("mpeg2_d3d11va_native_decode", Some(Mpeg2), K::D3d11Native),
    dec("vc1_d3d11va_native_decode", Some(Vc1), K::D3d11Native),
];

/// The decoder a QSV encode path uses for HEVC: D3D11 on Windows, VAAPI-backed elsewhere.
/// [ts:backend/src/modules/streaming/transcoding/codec/decoders/index.ts:108-121]
pub(super) fn qsv_native_hevc(p: Platform) -> &'static Decoder {
    let id = if p == Platform::Win32 {
        "hevc_qsv_d3d11_decode"
    } else {
        "hevc_qsv_decode"
    };
    DECODERS.iter().find(|d| d.id == id).unwrap()
}

pub(crate) fn find(id: &str) -> &'static Decoder {
    DECODERS.iter().find(|d| d.id == id).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_codec_has_each_variant_the_pipeline_looks_up() {
        for c in [H264, Hevc, Av1, Vp9, Mpeg2, Vc1] {
            for suffix in ["_qsv_decode", "_qsv_d3d11_decode", "_d3d11va_native_decode"] {
                find(&format!("{}{suffix}", c.name()));
            }
        }
    }

    #[test]
    fn source_names_collapse_to_one_codec() {
        assert_eq!(DecodeCodec::from_source("MPEG2VIDEO"), Some(Mpeg2));
        assert_eq!(DecodeCodec::from_source("vp09"), Some(Vp9));
        assert_eq!(DecodeCodec::from_source("vc1"), Some(Vc1));
        assert_eq!(DecodeCodec::from_source("wmv3"), None);
    }
}
