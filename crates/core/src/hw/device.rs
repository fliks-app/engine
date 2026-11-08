//! `-init_hw_device` argv and the render node, one place for the Linux/Windows difference.
//! [ts:backend/src/modules/streaming/transcoding/hw-device.ts:1-110]
use crate::api::Platform;
use crate::strs;

pub const DEFAULT_RENDER_NODE: &str = "/dev/dri/renderD128";

/// `auto`, empty or whitespace mean no pin. [ts:backend/src/modules/streaming/transcoding/hw-device.ts:18-21]
pub fn selected_render_node(setting: &str) -> Option<String> {
    let v = setting.trim();
    (!v.is_empty() && v != "auto").then(|| v.to_string())
}

/// What the ffmpeg argv builders vary on: platform, admin-pinned node, `FLIKS_OPENCL_DEVICE`.
#[derive(Debug, Clone)]
pub struct Device {
    pub platform: Platform,
    pub render_node: Option<String>,
    pub opencl_device: Option<String>,
    /// CUDA ordinal (PCI bus order) of the NVIDIA card to run on; `None` leaves ffmpeg's default.
    pub cuda_index: Option<u32>,
}

impl Device {
    pub fn node(&self) -> &str {
        self.render_node.as_deref().unwrap_or(DEFAULT_RENDER_NODE)
    }

    pub fn vaapi_init(&self) -> Vec<String> {
        strs(&["-init_hw_device", &format!("vaapi=va:{}", self.node())])
    }

    /// `-hwaccel cuda` input flags, on the card of `cuda_index`.
    pub fn cuda_hwaccel(&self, output_cuda: bool) -> Vec<String> {
        let mut a = vec![];
        if let Some(n) = self.cuda_index {
            a.extend(strs(&["-init_hw_device", &format!("cuda=cu:{n}")]));
        }
        a.extend(strs(&["-hwaccel", "cuda"]));
        if let Some(n) = self.cuda_index {
            a.extend(strs(&["-hwaccel_device", &n.to_string()]));
        }
        if output_cuda {
            a.extend(strs(&["-hwaccel_output_format", "cuda"]));
        }
        a
    }

    pub fn qsv_init(&self) -> Vec<String> {
        if self.platform == Platform::Win32 {
            return strs(&["-init_hw_device", "qsv=qs"]);
        }
        let mut a = self.vaapi_init();
        a.extend(strs(&["-init_hw_device", "qsv=qs@va"]));
        a
    }

    pub fn qsv_via_d3d11_init() -> Vec<String> {
        strs(&[
            "-init_hw_device",
            "d3d11va=dx",
            "-init_hw_device",
            "qsv=qs@dx",
        ])
    }

    pub fn vulkan_init(&self) -> Vec<String> {
        strs(&[
            "-init_hw_device",
            &format!("drm=dr:{}", self.node()),
            "-init_hw_device",
            "vaapi=va@dr",
            "-init_hw_device",
            "vulkan=vk@dr",
            "-filter_hw_device",
            "vk",
        ])
    }

    pub fn amf_d3d11_opencl_init() -> Vec<String> {
        strs(&[
            "-init_hw_device",
            "d3d11va=dx:,vendor_id=0x1002",
            "-init_hw_device",
            "opencl=ocl@dx",
            "-filter_hw_device",
            "ocl",
        ])
    }

    pub fn opencl_tonemap_init(&self) -> Vec<String> {
        let spec = match self.opencl_device.as_deref().map(str::trim) {
            Some(d) if !d.is_empty() => format!("opencl=ocl:{d}"),
            _ => "opencl=ocl".to_string(),
        };
        strs(&["-init_hw_device", &spec, "-filter_hw_device", "ocl"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(cuda_index: Option<u32>) -> Device {
        Device {
            platform: Platform::Linux,
            render_node: None,
            opencl_device: None,
            cuda_index,
        }
    }

    #[test]
    fn cuda_flags_pick_the_card_only_when_asked() {
        assert_eq!(
            dev(None).cuda_hwaccel(true),
            ["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"]
        );
        assert_eq!(
            dev(Some(1)).cuda_hwaccel(false),
            [
                "-init_hw_device",
                "cuda=cu:1",
                "-hwaccel",
                "cuda",
                "-hwaccel_device",
                "1"
            ]
        );
    }
}
