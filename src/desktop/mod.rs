//! Marcel as a citizen of the desktop: the session bus, the clipboard, the
//! file-chooser portal, launches, opening files with other applications, terminals,
//! drives and network shares,
//! the user's standard folders, and icon themes.

pub mod bus;
pub(crate) mod clipboard;
pub mod file_chooser;
pub mod gvfs;
pub mod icons;
pub mod launch;
pub mod open;
pub mod picker;
pub mod places;
pub mod terminal;
pub mod volumes;
