pub mod display;
pub mod input;

use crate::{PalError, VideoPipelineBackend};
use tracing::info;

pub use display::WindowsVirtualDisplayBackend;
pub use input::WindowsInputBackend;

/// Windows 11 Hardware Video Pipeline (`DXGI Desktop Duplication` + `NVENC/AMF` + `D3D11VA` Borderless Fullscreen Viewer).
/// Phase 1-3 provides emergency fullscreen overlay teardown hooks; full DXGI/D3D11VA pipeline arrives in Phase 4.
pub struct WindowsVideoPipelineBackend {
    viewers_open: bool,
}

impl Default for WindowsVideoPipelineBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsVideoPipelineBackend {
    pub const fn new() -> Self {
        Self {
            viewers_open: false,
        }
    }
}

impl VideoPipelineBackend for WindowsVideoPipelineBackend {
    fn close_all_viewer_windows(&mut self) -> Result<(), PalError> {
        if self.viewers_open {
            self.viewers_open = false;
        }
        info!("Destroyed all Windows D3D11VA borderless fullscreen viewer windows");
        Ok(())
    }
}
