use fliks_engine::hw::{Hw, Snapshot};
use fliks_engine_core::api::{HwAccel, Platform};
use fliks_engine_core::hw::Detected;
use std::sync::Arc;

/// A detected CPU-only host, the same caps as the golden `cpu` decision vectors.
pub fn cpu_host() -> Arc<Hw> {
    Hw::fixed(Snapshot {
        detected: Detected {
            hw_accel: HwAccel::None,
            platform: Platform::Linux,
            libfdk_aac: false,
            vaapi_writes_hdr_metadata: false,
            probes: vec![],
            encoders: vec![],
            decoders: vec![],
            disabled_encoders: vec![],
            disabled_decoders: vec![],
        },
        gpus: vec![],
        pause_capability: "none".into(),
        render_node: "/dev/dri/renderD128".into(),
    })
}
