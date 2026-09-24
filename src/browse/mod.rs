//! What a window browses: a directory's entries, the projection the user
//! sees of them, the selection over that projection, and the history of
//! where the window has been.
//!
//! Nothing here touches GPUI or the filesystem's mutation side. It is the
//! model the `app` module renders and the watcher keeps current.

pub mod directory_session;
pub mod entries;
pub mod history;
pub mod remoteness;
pub mod selection;
pub mod watcher;
