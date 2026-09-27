//! Where a window is.
//!
//! A window used to answer that with four values that had to agree: the
//! session's folder, a `browsing_trash` flag, a Place whose path was the
//! string `trash:///`, and a history of paths that could not hold the Trash
//! at all. Each of the twenty-eight places that needed the answer worked it
//! out again. `Location` is the answer, held once, and a new kind of place is
//! a new variant the compiler walks every caller through.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Location {
    /// A directory on disk, however it is mounted: local, a drive, a share.
    Folder(PathBuf),
    /// A Trash, shown as a flat list of what it holds.
    Trash(TrashScope),
}

/// Which Trash.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TrashScope {
    /// The user's own, `~/.local/share/Trash`.
    Home,
}

impl Location {
    /// The directory, for everything that needs one: Paste, New Folder, Open
    /// in Terminal, a watcher, a drop. `None` in the Trash, which has no
    /// folder a file could be put into.
    pub fn as_folder(&self) -> Option<&Path> {
        match self {
            Self::Folder(path) => Some(path),
            Self::Trash(_) => None,
        }
    }

    pub fn is_trash(&self) -> bool {
        matches!(self, Self::Trash(_))
    }

    /// What the location bar and a window title call it.
    pub fn label(&self) -> String {
        match self {
            Self::Folder(path) => path.display().to_string(),
            Self::Trash(TrashScope::Home) => "Trash".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_folder_is_a_folder() {
        let folder = Location::Folder("/home/me".into());
        assert_eq!(folder.as_folder(), Some(Path::new("/home/me")));
        assert!(!folder.is_trash());

        let trash = Location::Trash(TrashScope::Home);
        assert_eq!(trash.as_folder(), None);
        assert!(trash.is_trash());
        assert_eq!(trash.label(), "Trash");
    }
}
