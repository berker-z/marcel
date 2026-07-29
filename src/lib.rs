mod app;
mod bookmarks;
pub mod commands;
pub mod directory_session;
mod directory_watcher;
pub mod file_ops;
pub mod fs;
mod history;
mod icons;
mod operations;
mod pdf_preview;
mod places;
pub mod preview;
pub mod selection;
#[cfg(target_os = "linux")]
mod system_open;
pub mod theme;
mod thumbnails;
mod trash_ops;

pub use app::Marcel;
