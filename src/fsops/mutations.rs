//! The single-commit mutations: create, rename, compress, extract.
//!
//! Each follows the prepare / commit / finalize rule spelled out on
//! [`CommittedOperation`]: everything that can fail happens before the one
//! syscall that changes the disk, and nothing after it can fail the operation.

use std::{
    ffi::OsStr,
    fs,
    os::unix::fs::MetadataExt as _,
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
    conflict::ConflictPolicy,
    identity::FileIdentity,
    journal::{
        CommittedOperation, OperationRecord, PathSnapshot, SnapshotKind, reject_special_entries,
        snapshot_removable_tree,
    },
    local::{ensure_unoccupied, inspect, rename_no_replace, sorted_children},
    quarantine::{REPLACEMENT_UNDO_BYTE_LIMIT, ReplacedItem, erase_replacement_quarantine},
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
    // Marcel's working files are told apart from the user's by name alone,
    // and the browser hides them and a later Marcel may sweep them. A user
    // must not be able to give their own file that fate through Rename.
    if bytes.starts_with(WORKING_NAME_PREFIX) {
        bail!(
            "Names beginning with “{}” are reserved for Marcel's own working files",
            String::from_utf8_lossy(WORKING_NAME_PREFIX)
        );
    }
    Ok(())
}

/// What every name Marcel gives its own working state begins with:
/// replacement and deletion quarantines, copy and archive staging, recovery
/// remnants.
const WORKING_NAME_PREFIX: &[u8] = b".marcel-";

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

pub fn create_file(parent: &Path, name: &str) -> Result<CommittedOperation> {
    validate_entry_name(name)?;
    create_file_at(parent.join(name))
}

/// Create an empty regular file, refusing an occupied path.
///
/// `O_EXCL` makes the creation the one commit: the file either did not exist
/// and now does, or the call failed and nothing changed. Undo removes it only
/// while it is still the empty file Marcel made.
pub(super) fn create_file_at(path: PathBuf) -> Result<CommittedOperation> {
    // Commit.
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .at("Could not create", &path)?;
    // Finalize: the file exists. Its identity comes from the descriptor just
    // opened, so it is this file's and not that of something that replaced it.
    let record =
        file.metadata().ok().filter(|metadata| metadata.file_type().is_file()).map(|metadata| {
            OperationRecord::CreateFile {
                path: path.clone(),
                identity: FileIdentity::of(&metadata),
            }
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

/// Change an object's permission bits, recording the ones it had.
///
/// Symbolic links are refused: `chmod` follows them, so the bits would land
/// on whatever the link points at, which is not what the dialog showed.
pub fn set_mode(path: &Path, mode: u32) -> Result<CommittedOperation> {
    // Prepare.
    let metadata = inspect(path)?;
    if metadata.file_type().is_symlink() {
        bail!("The permissions of a symbolic link cannot be changed");
    }
    let previous = metadata.mode() & 0o7777;
    if previous == mode & 0o7777 {
        bail!("The permissions are unchanged");
    }
    change_mode(path, &FileIdentity::of(&metadata), previous, mode)
}

/// Undo or redo a mode change by putting the other mode on, which is one
/// more single commit.
pub(super) fn reverse_set_mode(operation: &OperationRecord) -> Result<CommittedOperation> {
    let OperationRecord::SetMode { path, identity, previous, mode } = operation else {
        bail!("Operation is not a permission change");
    };
    change_mode(path, identity, *mode, *previous)
}

/// Commit a mode change and describe it, whichever direction it runs in.
///
/// `chmod(2)` resolves its path again and follows a symbolic link at the end
/// of it, so a check by `lstat` followed by `chmod` leaves a window in which
/// the object can be swapped for a link and the bits land on its target. The
/// object is pinned with a descriptor instead: the identity check and the
/// commit then concern one inode, whatever the path names in between.
fn change_mode(
    path: &Path,
    expected: &FileIdentity,
    from: u32,
    to: u32,
) -> Result<CommittedOperation> {
    // Prepare: the object must still be the one whose bits were read.
    let pinned = pin_object(path).with_context(|| {
        format!("Cannot change permissions: “{}” no longer exists", path.display())
    })?;
    let metadata = pinned.metadata().at("Could not inspect", path)?;
    if metadata.file_type().is_symlink() {
        bail!("The permissions of a symbolic link cannot be changed");
    }
    if FileIdentity::of(&metadata) != *expected {
        bail!("Cannot change permissions: “{}” changed or was replaced", path.display());
    }
    // Commit.
    chmod_pinned(&pinned, to).at("Could not change permissions on", path)?;
    // Finalize: the bits are on disk. The change moved the ctime, so the
    // record carries a fresh identity; a failed read costs undo, not the
    // change.
    let record = pinned.metadata().ok().map(|metadata| OperationRecord::SetMode {
        path: path.to_path_buf(),
        identity: FileIdentity::of(&metadata),
        previous: from,
        mode: to,
    });
    Ok(CommittedOperation::new(
        path.to_path_buf(),
        DirectoryChanges::upserted(vec![path.to_path_buf()]),
        record,
    ))
}

/// A descriptor for whatever `path` names, without following a final link
/// and without opening the object.
///
/// `O_PATH` is what makes this safe for anything: a FIFO is not opened, so
/// nothing blocks; a device is not opened, so nothing is triggered; and no
/// read permission is needed, which matters because a file with mode `000` is
/// exactly the one a user reaches for the permissions dialog to fix.
///
/// This belongs beside `open_regular_file` in `local.rs`; it lives here until
/// that file is next touched.
fn pin_object(path: &Path) -> std::io::Result<fs::File> {
    use rustix::fs::{Mode, OFlags};

    rustix::fs::open(path, OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())
        .map(fs::File::from)
        .map_err(Into::into)
}

/// `chmod` the object a pinned descriptor refers to.
///
/// `fchmod` refuses an `O_PATH` descriptor, and `fchmodat(AT_SYMLINK_NOFOLLOW)`
/// is not implemented by the kernel (`fchmodat2` is, from 6.6, but rustix 1.1
/// does not expose it). Going through the `/proc/self/fd` link is how glibc
/// itself implements the flag. The kernel follows that link to the pinned
/// object and would keep following if that object were a symbolic link, which
/// is why the caller has already ruled one out.
fn chmod_pinned(pinned: &fs::File, mode: u32) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;

    rustix::fs::chmod(
        format!("/proc/self/fd/{}", pinned.as_raw_fd()),
        rustix::fs::Mode::from_raw_mode(mode),
    )
    .map_err(Into::into)
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
    policy: &mut ConflictPolicy,
) -> Result<CommittedOperation> {
    if cancelled.load(Ordering::Acquire) {
        bail!("Archive operation cancelled");
    }
    // Prepare.
    let source = snapshot_removable_tree(archive)?;
    // Commit.
    let outcome = extract_archive(archive, cancelled, policy)?;
    let published = outcome.published;
    // Finalize: the output is on disk, or folded into the folder that was
    // there. Whatever cannot be described costs undo, never the extraction.
    let record = match outcome.merged {
        // A merge added to a tree that was already there, so the record
        // names those additions rather than the tree.
        Some(merge) => merge.undoable.then(|| OperationRecord::ArchiveExtract {
            source,
            output: published.clone(),
            created: Vec::new(),
            replaced: Vec::new(),
            merged: merge.added,
        }),
        None => {
            // Holding a displaced item is what makes replacing it undoable.
            // Past the budget the replacement stands and simply cannot be
            // taken back, the same rule a transfer applies.
            let replaced = outcome.replaced;
            let within_budget = replaced.iter().map(ReplacedItem::bytes).sum::<u64>()
                <= REPLACEMENT_UNDO_BYTE_LIMIT;
            if !within_budget {
                for item in &replaced {
                    erase_replacement_quarantine(item);
                }
            }
            snapshot_removable_tree(&published).ok().filter(|_| within_budget).map(|created| {
                OperationRecord::ArchiveExtract {
                    source,
                    output: published.clone(),
                    created,
                    replaced,
                    merged: Vec::new(),
                }
            })
        }
    };
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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;
    use crate::testing::{Sandbox, skip_as_root};

    fn mode_of(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().mode() & 0o7777
    }

    /// The dialog read the bits of one object; by the time the change commits
    /// a link stands at that path. `chmod` would follow it, so the bits must
    /// not land on the target — and the record must not claim they did.
    #[test]
    fn a_link_that_replaced_the_object_after_it_was_read_is_not_followed() {
        let sandbox = Sandbox::new();
        let target = sandbox.file("target", b"");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let victim = sandbox.file("victim", b"");
        let expected = FileIdentity::read(&victim).unwrap();

        fs::remove_file(&victim).unwrap();
        std::os::unix::fs::symlink(&target, &victim).unwrap();

        let error = change_mode(&victim, &expected, 0o644, 0o777).unwrap_err().to_string();
        assert!(error.contains("symbolic link"), "{error}");
        assert_eq!(mode_of(&target), 0o600);
        assert!(fs::symlink_metadata(&victim).unwrap().file_type().is_symlink());
    }

    /// The same window, filled by another regular file rather than a link:
    /// the identity read from the descriptor is what decides, not the path.
    #[test]
    fn an_object_replaced_after_it_was_read_keeps_its_bits() {
        let sandbox = Sandbox::new();
        let original = sandbox.file("note", b"");
        let expected = FileIdentity::read(&original).unwrap();
        let replacement = sandbox.file("elsewhere", b"");
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o600)).unwrap();
        fs::rename(&replacement, &original).unwrap();

        let error = change_mode(&original, &expected, 0o644, 0o777).unwrap_err().to_string();
        assert!(error.contains("changed or was replaced"), "{error}");
        assert_eq!(mode_of(&original), 0o600);
    }

    /// A file nobody may read is the one a user most wants to fix, and pinning
    /// it must not need the permission it lacks.
    #[test]
    fn an_unreadable_file_can_still_have_its_bits_changed() {
        if skip_as_root() {
            return;
        }
        let sandbox = Sandbox::new();
        let locked = sandbox.file("locked", b"");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        assert!(fs::read(&locked).is_err(), "the fixture is unreadable");

        let committed = set_mode(&locked, 0o644).unwrap();

        assert_eq!(mode_of(&locked), 0o644);
        assert!(committed.into_record().is_some(), "the change is recorded for undo");
    }
}
