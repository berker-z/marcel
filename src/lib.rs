mod app;
pub mod archive_ops;
mod bookmarks;
pub mod commands;
pub mod conflict;
mod delete_ops;
#[cfg(target_os = "linux")]
pub mod desktop_integration;
pub mod directory_session;
mod directory_watcher;
mod drag_controller;
pub mod file_ops;
pub mod fs;
mod history;
mod icons;
pub mod identity;
mod image_preview;
pub mod launch;
mod local_fs;
pub mod operations;
mod pdf_preview;
mod places;
pub mod preview;
mod preview_controller;
pub mod selection;
mod sidebar_controller;
mod state;
pub mod surface;
#[cfg(target_os = "linux")]
mod system_open;
#[cfg(target_os = "linux")]
mod system_terminal;
pub mod theme;
mod thumbnails;
mod trash_ops;
pub mod window;
mod window_ui_state;

pub use app::Marcel;
