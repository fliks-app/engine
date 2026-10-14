//! Host capabilities as the decision reads them.
use crate::api::{HostCaps, Platform};

impl HostCaps {
    pub fn probe(&self, name: &str) -> bool {
        self.probes.iter().any(|p| p == name)
    }

    /// QSV derives from a VAAPI device, which only Linux has.
    pub fn has_vaapi(&self) -> bool {
        self.platform == Platform::Linux
    }
}
