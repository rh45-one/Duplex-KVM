pub mod display;
pub mod input;

use crate::{PalError, VideoPipelineBackend};
use tracing::info;

pub use display::{LinuxDisplayBackendKind, LinuxVirtualDisplayBackend};
pub use input::{LinuxInputBackend, LinuxUinputInjector};

/// Debian Linux Hardware Video Pipeline (`PipeWire DMA-BUF` + `VA-API` HEVC Encode/Decode + Wayland/X11 Borderless Viewer).
/// Phase 1-3 provides emergency fullscreen overlay teardown hooks; full VA-API pipeline arrives in Phase 4.
pub struct LinuxVideoPipelineBackend {
    viewer_open: bool,
}

impl Default for LinuxVideoPipelineBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxVideoPipelineBackend {
    pub const fn new() -> Self {
        Self { viewer_open: false }
    }
}

impl VideoPipelineBackend for LinuxVideoPipelineBackend {
    fn close_all_viewer_windows(&mut self) -> Result<(), PalError> {
        if self.viewer_open {
            self.viewer_open = false;
        }
        info!("Closed all Linux fullscreen hardware-decoded viewer windows");
        Ok(())
    }
}
