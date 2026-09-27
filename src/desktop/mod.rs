//! Marcel as a citizen of the desktop: the session bus, the file-chooser
//! portal, launches, opening files with other applications, terminals, the
//! user's standard folders, and icon themes; the clipboard shared with other
//! programs; and the two services that know about places Marcel did not make,
//! UDisks2 for drives and GVfs for network shares. Nothing here knows about
//! GPUI; the stores that put drives and shares on screen are
//! [`crate::volumes`] and [`crate::network`].

pub mod bus;
pub(crate) mod clipboard;
pub mod file_chooser;
pub(crate) mod gvfs;
pub mod icons;
pub mod launch;
pub mod open;
pub mod picker;
pub mod places;
pub mod terminal;
pub(crate) mod volumes;
