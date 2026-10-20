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
}

impl Device {
    pub fn node(&self) -> &str {
        self.render_node.as_deref().unwrap_or(DEFAULT_RENDER_NODE)
    }

    pub fn vaapi_init(&self) -> Vec<String> {
        strs(&["-init_hw_device", &format!("vaapi=va:{}", self.node())])
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
