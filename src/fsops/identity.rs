//! Who a filesystem object is.
//!
//! Every mutation Marcel can undo, and every deletion it stages, re-reads the
//! object it is about to touch and refuses unless it is still the one it
//! recorded. This is the one description of "the same object" they all share.

use std::{fs, os::unix::fs::MetadataExt as _, path::Path};

use anyhow::{Context as _, Result, bail};

/// Device and inode: the part of an identity a rename preserves.
///
/// It is the key carried across a commit boundary, because the commit itself
/// — a rename — bumps the ctime and so invalidates the fuller identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ObjectKey {
    pub device: u64,
    pub inode: u64,
}

impl ObjectKey {
    pub fn of(metadata: &fs::Metadata) -> Self {
        Self { device: metadata.dev(), inode: metadata.ino() }
    }

    /// Whether `path` still names this object.
    pub fn validate(self, path: &Path) -> Result<()> {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("Cannot continue: “{}” is missing", path.display()))?;
        if Self::of(&metadata) != self {
            bail!("Cannot continue: “{}” changed or was replaced", path.display());
        }
        Ok(())
    }
}

/// An object plus its ctime, which moves on any change to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    pub key: ObjectKey,
    changed: (i64, i64),
}

impl FileIdentity {
    pub fn of(metadata: &fs::Metadata) -> Self {
        Self { key: ObjectKey::of(metadata), changed: (metadata.ctime(), metadata.ctime_nsec()) }
    }

    pub fn read(path: &Path) -> Result<Self> {
        super::local::inspect(path).map(|metadata| Self::of(&metadata))
    }

    /// Read what is at `path` now and refuse unless it is still this object,
    /// unchanged since it was recorded.
    pub fn validate(&self, path: &Path, action: &str) -> Result<()> {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("Cannot {action}: “{}” no longer exists", path.display()))?;
        if Self::of(&metadata) != *self {
            bail!("Cannot {action}: “{}” changed or was replaced", path.display());
        }
        Ok(())
    }
}
