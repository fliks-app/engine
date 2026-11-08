//! Which hardware devices a host has: pure assembly of the per-node detection results.
use super::{Detected, GpuInfo};
use crate::api::HwAccel;

/// One detection result: the node it ran on and the CUDA ordinal it used, if any.
#[derive(Debug, Clone)]
pub struct Found {
    pub render_node: String,
    pub detected: Detected,
    pub cuda_index: Option<u32>,
}

/// A device the engine encodes on. `id` is `gpu<N>`, N the node's position among all the host's
/// render nodes, so a card whose probe fails once does not renumber the others.
#[derive(Debug, Clone)]
pub struct DeviceEntry {
    pub id: String,
    pub render_node: String,
    pub name: Option<String>,
    pub cuda_index: Option<u32>,
    pub detected: Detected,
}

/// Keeps the results with a hardware encoder, in the order given.
pub fn assemble(gpus: &[GpuInfo], found: Vec<Found>) -> Vec<DeviceEntry> {
    found
        .into_iter()
        .filter(|f| f.detected.hw_accel != HwAccel::None)
        .map(|f| {
            let at = gpus.iter().position(|g| g.render_node == f.render_node);
            DeviceEntry {
                id: format!("gpu{}", at.unwrap_or(0)),
                name: at.map(|i| gpus[i].label.clone()),
                render_node: f.render_node,
                cuda_index: f.cuda_index,
                detected: f.detected,
            }
        })
        .collect()
}

/// CUDA ordinal of the NVIDIA card on `node`: its rank by PCI bus id, which is what ffmpeg sees
/// under `CUDA_DEVICE_ORDER=PCI_BUS_ID`. `None` with a single NVIDIA card (the default is right).
pub fn cuda_index(gpus: &[GpuInfo], node: &str) -> Option<u32> {
    let mut nvidia: Vec<&GpuInfo> = gpus.iter().filter(|g| g.vendor == "nvidia").collect();
    if nvidia.len() < 2 {
        return None;
    }
    nvidia.sort_by(|a, b| (&a.pci_bus, &a.render_node).cmp(&(&b.pci_bus, &b.render_node)));
    nvidia
        .iter()
        .position(|g| g.render_node == node)
        .map(|i| i as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Platform;
    use crate::hw::{GpuRaw, describe_gpu};

    fn gpu(name: &str, vendor: &str, pci: &str) -> GpuInfo {
        let raw = GpuRaw {
            vendor_id: Some(vendor.into()),
            pci_bus: Some(pci.into()),
            ..GpuRaw::default()
        };
        describe_gpu(name, &format!("/dev/dri/{name}"), &raw)
    }

    fn found(node: &str, hw: HwAccel) -> Found {
        Found {
            render_node: format!("/dev/dri/{node}"),
            cuda_index: None,
            detected: Detected {
                hw_accel: hw,
                platform: Platform::Linux,
                libfdk_aac: false,
                vaapi_writes_hdr_metadata: false,
                probes: vec![],
                encoders: vec![],
                decoders: vec![],
                disabled_encoders: vec![],
                disabled_decoders: vec![],
            },
        }
    }

    #[test]
    fn ids_follow_the_node_not_the_survivors() {
        let gpus = [
            gpu("renderD128", "0x8086", "0000:00:02.0"),
            gpu("renderD129", "0x1002", "0000:03:00.0"),
            gpu("renderD130", "0x1002", "0000:04:00.0"),
        ];
        let d = assemble(
            &gpus,
            vec![
                found("renderD128", HwAccel::Qsv),
                found("renderD129", HwAccel::None),
                found("renderD130", HwAccel::Vaapi),
            ],
        );
        let ids: Vec<_> = d.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["gpu0", "gpu2"]);
        assert!(d[1].name.as_deref().unwrap().contains("renderD130"));
    }

    #[test]
    fn intel_plus_nvidia_gives_two_devices() {
        let gpus = [
            gpu("renderD128", "0x8086", "0000:00:02.0"),
            gpu("renderD129", "0x10de", "0000:01:00.0"),
        ];
        let d = assemble(
            &gpus,
            vec![
                found("renderD128", HwAccel::Qsv),
                found("renderD129", HwAccel::Nvenc),
            ],
        );
        assert_eq!(d.len(), 2);
        assert_eq!(d[1].detected.hw_accel, HwAccel::Nvenc);
        assert_eq!(cuda_index(&gpus, "/dev/dri/renderD129"), None);
    }

    #[test]
    fn cuda_ordinal_is_the_rank_by_pci_bus() {
        let gpus = [
            gpu("renderD128", "0x10de", "0000:41:00.0"),
            gpu("renderD129", "0x8086", "0000:00:02.0"),
            gpu("renderD130", "0x10de", "0000:03:00.0"),
        ];
        assert_eq!(cuda_index(&gpus, "/dev/dri/renderD130"), Some(0));
        assert_eq!(cuda_index(&gpus, "/dev/dri/renderD128"), Some(1));
        assert_eq!(cuda_index(&gpus, "/dev/dri/renderD129"), None);
    }

    #[test]
    fn no_node_listing_is_gpu0() {
        let d = assemble(&[], vec![found("renderD128", HwAccel::Nvenc)]);
        assert_eq!(d[0].id, "gpu0");
        assert!(d[0].name.is_none());
    }
}
