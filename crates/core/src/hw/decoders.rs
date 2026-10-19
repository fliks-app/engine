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
    pub codec: Option<VideoCodec>,
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
        if self.codec == Some(VideoCodec::H264) {
            8
        } else {
            10
        }
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
                if self.codec == Some(VideoCodec::Av1) {
                    a.extend(strs(&["-c:v", "av1"]));
                }
                a.push("-noautorotate".into());
                a
            }
        }
    }
}

use Kind as K;
use VideoCodec::{Av1, H264, Hevc};

const fn dec(id: &'static str, codec: Option<VideoCodec>, kind: Kind) -> Decoder {
    Decoder { id, codec, kind }
}

/// Registry order, then the Windows QSV/AMF D3D11 variants the registry only looks up.
pub(crate) const DECODERS: &[Decoder] = &[
    dec("h264_qsv_decode", Some(H264), K::QsvVaapi),
    dec("hevc_qsv_decode", Some(Hevc), K::QsvVaapi),
    dec("av1_qsv_decode", Some(Av1), K::QsvVaapi),
    dec("h264_vaapi_decode", Some(H264), K::Vaapi),
    dec("hevc_vaapi_decode", Some(Hevc), K::Vaapi),
    dec("av1_vaapi_decode", Some(Av1), K::Vaapi),
    dec("h264_cuda_decode", Some(H264), K::Cuda),
    dec("hevc_cuda_decode", Some(Hevc), K::Cuda),
    dec("av1_cuda_decode", Some(Av1), K::Cuda),
    dec("h264_d3d11va_decode", Some(H264), K::D3d11Cpu),
    dec("hevc_d3d11va_decode", Some(Hevc), K::D3d11Cpu),
    dec("av1_d3d11va_decode", Some(Av1), K::D3d11Cpu),
    dec("h264_videotoolbox_decode", Some(H264), K::Vt),
    dec("hevc_videotoolbox_decode", Some(Hevc), K::Vt),
    dec("av1_videotoolbox_decode", Some(Av1), K::Vt),
    dec("cpu", None, K::Cpu),
    dec("h264_qsv_d3d11_decode", Some(H264), K::QsvD3d11),
    dec("hevc_qsv_d3d11_decode", Some(Hevc), K::QsvD3d11),
    dec("av1_qsv_d3d11_decode", Some(Av1), K::QsvD3d11),
    dec("h264_d3d11va_native_decode", Some(H264), K::D3d11Native),
    dec("hevc_d3d11va_native_decode", Some(Hevc), K::D3d11Native),
    dec("av1_d3d11va_native_decode", Some(Av1), K::D3d11Native),
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
