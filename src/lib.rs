//! Marcel, a preview-first graphical file explorer.
//!
//! The crate reads top-down:
//!
//! - [`app`] is the window: what it holds, what it does, and how it draws.
//! - [`browse`] is the model a window shows — entries, projection, selection,
//!   history — kept current by a watcher.
//! - [`operations`] is the application's one owner of mutations; [`fsops`]
//!   is how each mutation is carried out safely.
//! - [`preview`] decodes what the preview pane shows.
//! - [`desktop`] is everything outside the process: the bus, the portal,
//!   launches, other applications, icon themes.
//! - [`window`], [`surface`], [`theme`], [`fonts`], [`config`], [`names`]
//!   and [`bookmarks`] are the small application-wide services the rest lean
//!   on.
//!
//! Only what `main.rs` starts up or drives from the bus is `pub`; the rest is
//! crate-private so that an item nothing uses any more is a warning, not a
//! permanent export.

// The session bus, the portal backend, the Trash, and the launch paths are
// all freedesktop; the dependencies that speak them are unconditional, so a
// build for anything else fails here with a reason rather than in zbus.
#[cfg(not(target_os = "linux"))]
compile_error!("Marcel is a Linux desktop application; it has no other target");

mod app;
pub(crate) mod bookmarks;
pub(crate) mod browse;
pub mod config;
pub mod desktop;
pub mod fonts;
pub(crate) mod fsops;
pub(crate) mod names;
pub mod operations;
pub(crate) mod preview;
pub mod surface;
pub mod theme;
pub mod window;

#[cfg(test)]
pub(crate) mod testing;

pub use app::{Marcel, init_key_bindings};
