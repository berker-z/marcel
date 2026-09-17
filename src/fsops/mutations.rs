//! The single-commit mutations: create, rename, compress, extract.
//!
//! Each follows the prepare / commit / finalize rule spelled out on
//! [`CommittedOperation`]: everything that can fail happens before the one
//! syscall that changes the disk, and nothing after it can fail the operation.

use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use super::local::PathContext as _;
use anyhow::{Context as _, Result, bail};

use super::{
    DirectoryChanges,
    archive::{MAX_ARCHIVE_ENTRIES, create_zip_archive, extract_archive},
    identity::FileIdentity,
    journal::{
        CommittedOperation, OperationRecord, PathSnapshot, SnapshotKind, reject_special_entries,
        snapshot_removable_tree,
    },
    local::{ensure_unoccupied, inspect, rename_no_replace, sorted_children},
};

pub fn validate_entry_name(name: &str) -> Result<()> {
    validate_entry_os_name(OsStr::new(name))
}

/// The one name rule, applied to the raw bytes.
///
/// A conflict can be resolved by choosing a new name, and that name must clear
/// exactly the same bar as Rename and New Folder. Marcel keeps authoritative
/// `OsString` filenames, so the check works on bytes rather than requiring
/// valid UTF-8.
pub fn validate_entry_os_name(name: &OsStr) -> Result<()> {
    use std::os::unix::ffi::OsStrExt as _;

    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.iter().all(u8::is_ascii_whitespace) {
        bail!("Enter a name");
    }
    if bytes == b"." || bytes == b".." {
        bail!("“{}” is reserved and cannot be used as a name", name.to_string_lossy());
    }
    if bytes.contains(&b'/') || bytes.contains(&0) {
        bail!("Names cannot contain “/” or a null character");
    }
    Ok(())
}

pub fn create_directory(parent: &Path, name: &str) -> Result<CommittedOperation> {
    validate_entry_name(name)?;
    create_directory_at(parent.join(name))
}

pub(super) fn create_directory_at(path: PathBuf) -> Result<CommittedOperation> {
    // Commit.
    fs::create_dir(&path).at("Could not create", &path)?;
    // Finalize: the directory exists. Refusing to record an unexpected result
    // costs undo, but must not report the creation as failed.
    let record = fs::symlink_metadata(&path)
        .ok()
        .filter(|metadata| metadata.file_type().is_dir())
        .map(|metadata| OperationRecord::CreateDirectory {
            path: path.clone(),
            identity: FileIdentity::of(&metadata),
        });
    Ok(CommittedOperation::published(path, record))
}

// Yazi's rename actor coordinates focused input, watcher updates, and reveal:
// https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-actor/src/mgr/rename.rs
// Marcel keeps those interaction principles but owns this stricter
// RENAME_NOREPLACE and identity-validating Undo/Redo implementation. No Yazi
// code is copied.
pub fn rename_entry(source: &Path, name: &str) -> Result<CommittedOperation> {
    // Prepare.
    validate_entry_name(name)?;
    let parent = source.parent().context("Rename source has no parent")?;
    let current_name = source.file_name().context("Rename source has no name")?;
    if current_name == OsStr::new(name) {
        bail!("The new name is unchanged");
    }
    let destination = parent.join(name);
    ensure_unoccupied(&destination)?;
    let expected = FileIdentity::read(source)?;
    expected.validate(source, "rename")?;
    rename_committing(source, &destination)
}

/// Undo or redo a rename by renaming back, which is one more atomic commit.
pub(super) fn reverse_rename(operation: &OperationRecord) -> Result<CommittedOperation> {
    let OperationRecord::Rename { source, destination, identity } = operation else {
        bail!("Operation is not a rename");
    };
    // Prepare.
    identity.validate(destination, "reverse rename")?;
    ensure_unoccupied(source)?;
    rename_committing(destination, source)
}

/// Commit a rename and describe it, whichever direction it runs in.
fn rename_committing(from: &Path, to: &Path) -> Result<CommittedOperation> {
    // Commit.
    rename_no_replace(from, to)
        .with_context(|| format!("Could not rename “{}” to “{}”", from.display(), to.display()))?;
    // Finalize: the entry is renamed on disk. A failed inspection costs undo,
    // never the rename itself.
    let record = fs::symlink_metadata(to).ok().map(|metadata| OperationRecord::Rename {
        source: from.to_path_buf(),
        destination: to.to_path_buf(),
        identity: FileIdentity::of(&metadata),
    });
    Ok(CommittedOperation::new(
        to.to_path_buf(),
        DirectoryChanges { removed: vec![from.to_path_buf()], upserted: vec![to.to_path_buf()] },
        record,
    ))
}

pub fn create_zip_operation(
    sources: &[PathBuf],
    destination: &Path,
    cancelled: Arc<AtomicBool>,
) -> Result<CommittedOperation> {
    // Prepare.
    let source_snapshots = snapshot_paths_cancellable(sources, &cancelled)?;
    // Commit: the archive is published by `create_zip_archive`.
    let published = create_zip_archive(sources, destination, cancelled)?.published;
    // Finalize: a failed snapshot loses undo, it does not unpublish the ZIP.
    let record =
        snapshot_removable_tree(&published).ok().map(|created| OperationRecord::ArchiveCreate {
            sources: source_snapshots,
            destination: published.clone(),
            created,
        });
    Ok(CommittedOperation::published(published, record))
}

pub fn extract_archive_operation(
    archive: &Path,
    cancelled: Arc<AtomicBool>,
) -> Result<CommittedOperation> {
    if cancelled.load(Ordering::Acquire) {
        bail!("Archive operation cancelled");
    }
    // Prepare.
    let source = snapshot_removable_tree(archive)?;
    // Commit.
    let published = extract_archive(archive, cancelled)?.published;
    // Finalize.
    let record = snapshot_removable_tree(&published).ok().map(|created| {
        OperationRecord::ArchiveExtract { source, output: published.clone(), created }
    });
    Ok(CommittedOperation::published(published, record))
}

fn snapshot_paths_cancellable(
    paths: &[PathBuf],
    cancelled: &AtomicBool,
) -> Result<Vec<PathSnapshot>> {
    let mut snapshots = Vec::new();
    let mut pending = paths.iter().rev().cloned().collect::<Vec<_>>();
    while let Some(path) = pending.pop() {
        if cancelled.load(Ordering::Acquire) {
            bail!("Archive operation cancelled");
        }
        let snapshot = PathSnapshot::of(&path, &inspect(&path)?);
        let kind = snapshot.kind;
        // An archive cannot carry a socket, FIFO, or device node, so refuse the
        // selection before any staging work rather than failing mid-compression.
        reject_special_entries(std::slice::from_ref(&snapshot))?;
        snapshots.push(snapshot);
        if snapshots.len() > MAX_ARCHIVE_ENTRIES {
            bail!("Selection contains more than {MAX_ARCHIVE_ENTRIES} entries");
        }
        if kind == SnapshotKind::Directory {
            pending.extend(sorted_children(&path)?.into_iter().rev().map(|e| e.path()));
        }
    }
    Ok(snapshots)
}
