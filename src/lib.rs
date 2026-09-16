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
//! - [`window`], [`surface`], [`theme`], [`fonts`], [`config`] and
//!   [`bookmarks`] are the small application-wide services the rest lean on.

mod app;
pub mod bookmarks;
pub mod browse;
pub mod config;
pub mod desktop;
pub mod fonts;
pub mod fsops;
pub mod operations;
pub mod preview;
pub mod surface;
pub mod theme;
pub mod window;

pub use app::{Marcel, init_key_bindings};
