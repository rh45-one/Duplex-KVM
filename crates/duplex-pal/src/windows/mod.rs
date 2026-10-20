mod capture;
pub mod display;
pub mod input;
pub mod video;

pub use display::WindowsVirtualDisplayBackend;
pub use input::WindowsInputBackend;
pub use video::WindowsVideoPipelineBackend;
