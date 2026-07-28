mod app;
pub mod fs;
mod history;
mod icons;
mod pdf_preview;
mod places;
pub mod preview;
pub mod selection;
#[cfg(target_os = "linux")]
mod system_open;
pub mod theme;
mod thumbnails;

pub use app::Marcel;
