//! Host capability detection: the ffmpeg commands to run, how to read their results, and the
//! `HostCaps`/`Capabilities` they add up to. IO is injected through [`Runner`]; see `docs/spec/hw.md`.
pub(crate) mod decoders;
mod detect;
mod device;
mod encoders;
mod gpu;
mod parse;
mod pause;
mod probe;

pub use detect::{Detected, Outcome, Runner, detect};
pub use device::{DEFAULT_RENDER_NODE, Device, selected_render_node};
pub use gpu::{GpuInfo, GpuKind, GpuRaw, describe_gpu};
pub use parse::{has_libfdk_aac, vaapi_driver_is_intel_ihd};
pub use pause::{PROBE_ARGS, PauseVerdict, pause_capability, pause_step};
pub use probe::{Cmd, encoders_listing};

use crate::api::{HwAccel, Platform};

/// One compiled-in encoder, in registry order. `args` is the probe argv from `encoders.rs`.
pub(crate) struct Encoder {
    pub id: &'static str,
    pub hw: HwAccel,
    pub ten_bit: bool,
    pub win32_only: bool,
    pub args: &'static [&'static str],
}

impl Encoder {
    /// Descriptor `supports()`: AMF is Windows-only. [ts:backend/src/modules/streaming/transcoding/codec/encoders/amf.spec.ts:1]
    pub fn supported(&self, p: Platform) -> bool {
        !self.win32_only || p == Platform::Win32
    }
}

#[cfg(test)]
pub(crate) fn encoder_ids() -> Vec<&'static str> {
    encoders::ENCODERS.iter().map(|e| e.id).collect()
}

/// `std::env::consts::OS` as the wire platform; anything unknown probes like Linux, as the TS does.
pub fn platform_from_os(os: &str) -> Platform {
    match os {
        "macos" | "darwin" => Platform::Darwin,
        "windows" | "win32" => Platform::Win32,
        _ => Platform::Linux,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn probe_table_lists_the_decision_registry() {
        assert_eq!(super::encoder_ids(), crate::decide::codec::registry_ids());
    }
}
