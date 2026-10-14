//! GPU identification from DRI render nodes and sysfs readings.
//! [ts:backend/src/modules/streaming/transcoding/gpu-registry.ts:1-127]
use crate::api::Gpu;

/// What sysfs says about `/sys/class/drm/<node>/device`; `None` when a file is unreadable.
#[derive(Debug, Clone, Default)]
pub struct GpuRaw {
    pub vendor_id: Option<String>,
    pub device_id: Option<String>,
    pub boot_vga: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuKind {
    Igpu,
    Dgpu,
    Nvidia,
    Amd,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GpuInfo {
    pub render_node: String,
    pub vendor: String,
    pub kind: GpuKind,
    pub label: String,
}

impl From<&GpuInfo> for Gpu {
    fn from(g: &GpuInfo) -> Self {
        Gpu {
            render_node: g.render_node.clone(),
            name: g.label.clone(),
        }
    }
}

/// Only well-known ids get a model name; the rest label as vendor and PCI id.
const KNOWN_MODELS: &[(&str, &str)] = &[
    ("0xa7a8", "Raptor Lake UHD Graphics"),
    ("0x5693", "Arc A370M"),
];

/// `name` is the node's file name (`renderD128`), `render_node` its path.
pub fn describe_gpu(name: &str, render_node: &str, raw: &GpuRaw) -> GpuInfo {
    let vendor_id = raw.vendor_id.clone().unwrap_or_default();
    let device_id = raw.device_id.clone().unwrap_or_default();
    let vendor = match vendor_id.as_str() {
        "0x8086" => "intel".to_string(),
        "0x10de" => "nvidia".to_string(),
        "0x1002" => "amd".to_string(),
        other => other.to_string(),
    };
    let boot_vga = raw.boot_vga.as_deref() == Some("1");
    let kind = match vendor.as_str() {
        "nvidia" => GpuKind::Nvidia,
        "amd" => GpuKind::Amd,
        "intel" if boot_vga => GpuKind::Igpu,
        "intel" => GpuKind::Dgpu,
        _ => GpuKind::Unknown,
    };
    let kind_label = match kind {
        GpuKind::Igpu => "iGPU",
        GpuKind::Dgpu => "dGPU",
        GpuKind::Nvidia => "NVIDIA",
        GpuKind::Amd => "AMD",
        GpuKind::Unknown => "GPU",
    };
    let vendor_label = match vendor.as_str() {
        "intel" => "Intel",
        "nvidia" => "NVIDIA",
        "amd" => "AMD",
        other => other,
    };
    let q = |s: &str| {
        if s.is_empty() {
            "?".to_string()
        } else {
            s.to_string()
        }
    };
    let label = match KNOWN_MODELS.iter().find(|(id, _)| *id == device_id) {
        Some((_, model)) => format!("{vendor_label} {model} · {kind_label} · {name}"),
        None => format!(
            "{vendor_label} {kind_label} · {name} ({}:{})",
            q(&vendor_id),
            q(&device_id)
        ),
    };
    GpuInfo {
        render_node: render_node.to_string(),
        vendor,
        kind,
        label,
    }
}
