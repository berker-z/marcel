mod app;
pub mod fs;
mod history;
mod places;
pub mod preview;
#[cfg(target_os = "linux")]
mod system_open;
pub mod theme;

pub use app::Marcel;
