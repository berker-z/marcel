mod app;
pub mod archive_ops;
mod bookmarks;
pub mod commands;
mod delete_ops;
#[cfg(target_os = "linux")]
pub mod desktop_integration;
pub mod directory_session;
mod directory_watcher;
pub mod file_ops;
pub mod fs;
mod history;
mod icons;
pub mod identity;
pub mod launch;
mod operations;
mod pdf_preview;
mod places;
pub mod preview;
pub mod selection;
mod state;
#[cfg(target_os = "linux")]
mod system_open;
#[cfg(target_os = "linux")]
mod system_terminal;
pub mod theme;
mod thumbnails;
mod trash_ops;

pub use app::Marcel;
