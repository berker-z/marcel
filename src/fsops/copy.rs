//! Copying one entry into a private staging directory and publishing it with a
//! single rename, and folding one directory into another.
//!
//! The source is read through descriptors, never re-resolved by path. The
//! walker holds one directory descriptor per level and opens every entry
//! relative to its parent with `O_NOFOLLOW`, so a component that another
//! writer swaps for a symlink after the walk listed it is not followed, and a
//! FIFO or link put in a file's place between inspection and open is refused
//! rather than blocked on or read through. The destination is Marcel's own
//! `0700` staging directory until the publishing rename, so it is addressed by
//! path.
//!
//! Durability: every file's content is fsync'd before its metadata is applied,
//! every directory of the staged tree is fsync'd once the whole tree is in
//! place, and the directory a copy is published into is fsync'd after the
//! rename. Publication is therefore crash-durable and not just ordered — after
//! a power loss the destination holds either the whole copy or nothing, and
//! the staging directory that held the rest is reclaimed as abandoned. The
//! directory syncs are batched so they cost one journal commit per copy rather
//! than one per directory; a merge likewise syncs each directory it added to
//! once at the end, not once per file.
//!
//! Conceptually follows Yazi's copier and attribute preservation; no Yazi code
//! is copied:
//! https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-fs/src/engine/local/copier.rs
//! https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-fs/src/engine/attrs.rs

use std::{
    collections::{BTreeSet, HashMap},
    ffi::OsStr,
    fs,
    io::{self, Read as _, Seek as _, Write as _},
    os::fd::{AsFd as _, BorrowedFd},
    path::{Path, PathBuf},
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
};

use super::local::PathContext as _;
use anyhow::{Context as _, Result, bail};

use super::{
    TransferProgress,
    conflict::describe_occupant,
    identity::{FileIdentity, ObjectKey},
    journal::{
        PathSnapshot, SnapshotCollector, SnapshotKind, rebase_snapshots,
        refresh_snapshot_identities,
    },
    local::{
        CWD, ensure_unoccupied, hold_directory, hold_entry, inspect, open_directory_at,
        open_regular_file_at, read_link_target, rename_no_replace, sorted_child_names,
        sorted_children,
    },
    quarantine::{WorkingKind, staging_prefix},
};

pub(super) struct CopiedItem {
    pub(super) sources: Vec<PathSnapshot>,
    pub(super) created: Vec<PathSnapshot>,
    pub(super) overflowed: bool,
    pub(super) undoable: bool,
}

pub(super) fn copy_one(
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
    progress: Option<&TransferProgress>,
    snapshot_limit: usize,
) -> Result<CopiedItem> {
    let copied = copy_one_unsynced(source, destination, cancelled, progress, snapshot_limit)?;
    sync_directory(destination.parent().context("Copy destination has no parent directory")?);
    Ok(copied)
}

/// [`copy_one`] without the final fsync of the directory published into.
///
/// A merge publishes many files into the same few directories and syncs each
/// of them once at the end instead; doing it here would turn one journal
/// commit per directory into one per file.
fn copy_one_unsynced(
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
    progress: Option<&TransferProgress>,
    snapshot_limit: usize,
) -> Result<CopiedItem> {
    // Prepare.
    ensure_unoccupied(destination)?;
    ensure_not_self_containing(source, destination, "copy")?;
    let name = destination.file_name().context("Copy destination has no file name")?;
    let staging = reserve_staging_directory(destination)?;
    let staged = staging.path().join(name);
    let mut copier = Copier {
        cancelled,
        progress,
        hardlinks: HashMap::new(),
        sources: SnapshotCollector::new(snapshot_limit),
        created: SnapshotCollector::new(snapshot_limit),
        staged_directories: Vec::new(),
    };
    copier.copy_tree(source, &staged)?;
    // Commit. The staging directory is removed when `staging` drops, taking
    // any partially copied tree with it; only the published entry survives.
    rename_no_replace(&staged, destination).with_context(|| {
        format!("Could not publish copy at “{}”; nothing was overwritten", destination.display())
    })?;
    // Finalize: the copy is published. Re-reading identities can only cost
    // undo, because publication renames the staged root and bumps its ctime.
    let Copier { sources, created, .. } = copier;
    let mut created_snapshots = created.snapshots;
    rebase_snapshots(&mut created_snapshots, &staged, destination);
    let undoable = refresh_snapshot_identities(&mut created_snapshots);
    Ok(CopiedItem {
        sources: sources.snapshots,
        created: created_snapshots,
        overflowed: sources.overflowed || created.overflowed,
        undoable,
    })
}

/// Make a directory's entries durable after something was published into it.
///
/// This runs after the commit, so it can only make the published entry
/// survive a crash; it cannot make the publication fail. A filesystem that
/// refuses to sync a directory has left the copy correct and reachable, and
/// there is nothing the caller could undo or redo about it, so the error is
/// not reported.
fn sync_directory(directory: &Path) {
    if let Ok(directory) = fs::File::open(directory) {
        let _ = directory.sync_all();
    }
}

/// Reject a copy or move whose destination resolves back inside its own
/// source. Otherwise Marcel's staging directory lands inside the tree being
/// walked and the copy enumerates and re-copies its own output until
/// `PATH_MAX` stops it.
///
/// The destination's ancestors are compared with the source by identity, not
/// by path: a lexical prefix test misses a symlinked destination, and a
/// canonical-path test misses a bind mount, which gives the same directory a
/// second name that resolves to nothing in common with the first. Device and
/// inode are shared by every name of a directory, so walking `..` from the
/// destination's parent to the root and comparing each step catches both.
/// (The bind-mount case is covered by construction; creating one needs root,
/// so only the symlink case is exercised by the tests.)
pub(super) fn ensure_not_self_containing(
    source: &Path,
    destination: &Path,
    action: &str,
) -> Result<()> {
    let source_metadata = inspect(source)?;
    if !source_metadata.file_type().is_dir() {
        // Symbolic links are recreated as links rather than traversed, so a
        // link resolving into the destination cannot recurse.
        return Ok(());
    }
    let source_key = ObjectKey::of(&source_metadata);
    let parent = destination.parent().context("Destination has no parent directory")?;
    let refuse = || {
        bail!(
            "Cannot {action} “{}” into itself",
            source.file_name().unwrap_or_default().to_string_lossy()
        )
    };
    let mut ancestor = hold_directory(CWD, parent).at("Could not resolve", parent)?;
    loop {
        let key = ObjectKey::of(&ancestor.metadata().at("Could not inspect", parent)?);
        if key == source_key {
            return refuse();
        }
        let above =
            hold_directory(ancestor.as_fd(), Path::new("..")).at("Could not resolve", parent)?;
        // The root is its own parent; nothing above it can be the source.
        if ObjectKey::of(&above.metadata().at("Could not inspect", parent)?) == key {
            return Ok(());
        }
        ancestor = above;
    }
}

/// Reserve a private staging directory beside the destination.
///
/// This matches the archive staging model: the directory is created atomically
/// with a unique name instead of being probed for and created later, so Marcel
/// can never adopt — and then recursively delete — a path another process
/// created in the gap.
fn reserve_staging_directory(destination: &Path) -> Result<tempfile::TempDir> {
    let parent = destination.parent().context("Copy destination has no parent directory")?;
    tempfile::Builder::new()
        .prefix(&staging_prefix(WorkingKind::Copy))
        .tempdir_in(parent)
        .at("Could not reserve a temporary copy directory in", parent)
}

/// One unit of copy work. Directories are visited twice so their metadata is
/// applied after their children exist, which an explicit stack expresses
/// directly.
enum CopyStep {
    Visit {
        /// The held directory `source` is named in, or `None` for the root,
        /// whose whole path the user chose. Every pending child of a directory
        /// shares its descriptor, which is what keeps the level open until
        /// the last child has been read through it.
        parent: Option<Rc<fs::File>>,
        source: PathBuf,
        destination: PathBuf,
    },
    FinishDirectory {
        source: Rc<fs::File>,
        source_path: PathBuf,
        destination: PathBuf,
        metadata: fs::Metadata,
        created_index: Option<usize>,
    },
}

/// The state one `copy_one` carries across its whole tree.
struct Copier<'a> {
    cancelled: &'a AtomicBool,
    progress: Option<&'a TransferProgress>,
    /// Hardlinked sources already copied, so their siblings can link to the
    /// copy instead of duplicating its bytes.
    hardlinks: HashMap<(u64, u64), PathBuf>,
    sources: SnapshotCollector,
    created: SnapshotCollector,
    /// Every directory created so far, synced together before publication.
    staged_directories: Vec<PathBuf>,
}

impl Copier<'_> {
    /// Copy one entry using an explicit work stack.
    ///
    /// Recursion here was bounded only by the thread stack: Marcel runs
    /// transfers on `blocking` pool threads with Rust's 2 MiB default, so a
    /// deep enough tree aborted the whole process with a stack overflow
    /// mid-mutation. The archive and delete walkers already used explicit
    /// stacks; this matches them.
    ///
    /// The walk holds one descriptor per level of the path it is currently
    /// inside, so a tree deeper than the process's descriptor limit fails
    /// with `EMFILE` rather than escaping the guard. `PATH_MAX` keeps that
    /// depth in the low thousands, and the usual soft limit is well above.
    fn copy_tree(&mut self, source: &Path, destination: &Path) -> Result<()> {
        let mut steps = vec![CopyStep::Visit {
            parent: None,
            source: source.to_path_buf(),
            destination: destination.to_path_buf(),
        }];
        while let Some(step) = steps.pop() {
            match step {
                CopyStep::Visit { parent, source, destination } => {
                    self.visit(parent, source, destination, &mut steps)?
                }
                CopyStep::FinishDirectory {
                    source,
                    source_path,
                    destination,
                    metadata,
                    created_index,
                } => {
                    preserve_metadata(&source, &source_path, &destination, &metadata)?;
                    self.created.refresh(created_index, &destination, &inspect(&destination)?);
                    self.complete_item();
                }
            }
        }
        self.sync_staged_directories()
    }

    /// Make the staged directories durable, all at once, before publication.
    ///
    /// Every file was fsync'd as it was written, so what is still in flight
    /// here is directory metadata: entries, times, modes. Syncing each
    /// directory as it finished cost a journal commit apiece and doubled the
    /// time to copy a tree of one-file directories (2.1 s to 3.8 s for a
    /// thousand of them); done together after the last file, the first sync
    /// commits everything and the rest find nothing dirty (about 0.1 s for
    /// the same thousand). A directory whose copied mode denies its owner the
    /// read bit cannot be opened for the sync; the publishing directory's own
    /// fsync still covers it on a journaling filesystem, and there is nothing
    /// else to do, so it is skipped.
    fn sync_staged_directories(&mut self) -> Result<()> {
        for directory in self.staged_directories.drain(..) {
            match fs::File::open(&directory) {
                Ok(opened) => opened.sync_all().at("Could not finish", &directory)?,
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
                Err(error) => return Err(error).at("Could not finish", &directory),
            }
        }
        Ok(())
    }

    fn visit(
        &mut self,
        parent: Option<Rc<fs::File>>,
        source: PathBuf,
        destination: PathBuf,
        steps: &mut Vec<CopyStep>,
    ) -> Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            bail!("Operation cancelled");
        }
        let (dir, name): (BorrowedFd<'_>, &Path) = match &parent {
            Some(parent) => (
                parent.as_fd(),
                Path::new(source.file_name().context("Source entry has no file name")?),
            ),
            None => (CWD, &source),
        };
        // Decide on the object first, then open it and check that the open
        // reached the same object. Between the two, a writer sharing the
        // directory can swap the name for a link or a FIFO; the identity
        // check is what turns that into a refusal instead of a read of
        // whatever the new name leads to.
        let held = hold_entry(dir, name).at("Could not inspect", &source)?;
        let metadata = held.metadata().at("Could not inspect", &source)?;
        #[cfg(test)]
        fault::between_inspection_and_open(&source);
        self.sources.push(&source, &metadata);
        let kind = metadata.file_type();
        if let Some(progress) = self.progress {
            progress.set_current_path(Some(source.clone()));
        }

        if kind.is_dir() {
            let opened = open_directory_at(dir, name).at("Could not open", &source)?;
            ensure_same_object(&metadata, &opened, &source)?;
            fs::create_dir(&destination).at("Could not create", &destination)?;
            let created_index = self.created.push(&destination, &inspect(&destination)?);
            self.staged_directories.push(destination.clone());
            let children = sorted_child_names(&opened).at("Could not read", &source)?;
            let opened = Rc::new(opened);
            steps.push(CopyStep::FinishDirectory {
                source: Rc::clone(&opened),
                source_path: source.clone(),
                destination: destination.clone(),
                metadata,
                created_index,
            });
            // Reversed so children pop in enumeration order.
            for child in children.into_iter().rev() {
                steps.push(CopyStep::Visit {
                    parent: Some(Rc::clone(&opened)),
                    destination: destination.join(&child),
                    source: source.join(&child),
                });
            }
            return Ok(());
        }
        if kind.is_file() {
            let mut opened = open_regular_file_at(dir, name).at("Could not open", &source)?;
            ensure_same_object(&metadata, &opened, &source)?;
            self.copy_regular_file(&mut opened, &source, &destination, &metadata)?;
            preserve_metadata(&opened, &source, &destination, &metadata)?;
        } else if kind.is_symlink() {
            // Linux keeps user and ACL attributes off symbolic links, so the
            // target is all there is to preserve.
            let target = read_link_target(&held).at("Could not read link", &source)?;
            std::os::unix::fs::symlink(target, &destination).at("Could not copy link", &source)?;
        } else {
            bail!("Special files are not supported yet: “{}”", source.display());
        }
        self.created.push(&destination, &inspect(&destination)?);
        self.complete_item();
        Ok(())
    }

    fn complete_item(&self) {
        if let Some(progress) = self.progress {
            progress.complete_item();
        }
    }

    fn copy_regular_file(
        &mut self,
        input: &mut fs::File,
        source: &Path,
        destination: &Path,
        metadata: &fs::Metadata,
    ) -> Result<()> {
        use std::os::unix::fs::MetadataExt as _;

        let identity = (metadata.dev(), metadata.ino());
        if metadata.nlink() > 1
            && let Some(existing) = self.hardlinks.get(&identity)
        {
            fs::hard_link(existing, destination).with_context(|| {
                format!(
                    "Could not preserve hardlink “{}” at “{}”",
                    source.display(),
                    destination.display()
                )
            })?;
            if let Some(progress) = self.progress {
                progress.complete_bytes(metadata.len());
            }
            return Ok(());
        }

        copy_file_cancellable(input, source, destination, self.cancelled, self.progress)?;
        if metadata.nlink() > 1 {
            self.hardlinks.insert(identity, destination.to_path_buf());
        }
        Ok(())
    }
}

/// Deterministic interference in the window between the walker inspecting a
/// source entry and opening it — the window a co-writer in the source
/// directory would use. Thread-local like `local::fault`, so parallel tests
/// cannot interfere with one another.
#[cfg(test)]
pub(super) mod fault {
    use std::{cell::RefCell, path::Path};

    type Hook = Box<dyn FnMut(&Path)>;

    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    /// Run `hook` with every source path after it is inspected and before it
    /// is opened, until the guard drops.
    #[must_use = "the hook is removed when the guard drops"]
    pub fn between_inspection_and_open_do(hook: impl FnMut(&Path) + 'static) -> Guard {
        HOOK.with_borrow_mut(|slot| *slot = Some(Box::new(hook)));
        Guard
    }

    pub struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            HOOK.with_borrow_mut(|slot| *slot = None);
        }
    }

    pub(super) fn between_inspection_and_open(source: &Path) {
        HOOK.with_borrow_mut(|slot| {
            if let Some(hook) = slot {
                hook(source);
            }
        });
    }
}

/// Refuse an opened descriptor unless it is the object the walker inspected.
///
/// `O_NOFOLLOW` already rejects a link in the final position; this catches the
/// rest — a name unlinked and recreated as another file, or a directory
/// swapped for a different one — by comparing device, inode, and type.
fn ensure_same_object(expected: &fs::Metadata, opened: &fs::File, source: &Path) -> Result<()> {
    let found = opened.metadata().at("Could not inspect", source)?;
    if ObjectKey::of(&found) != ObjectKey::of(expected) || found.file_type() != expected.file_type()
    {
        bail!("“{}” was replaced while it was being copied", source.display());
    }
    Ok(())
}

/// Copy the content of an already opened, already verified regular file.
pub(super) fn copy_file_cancellable(
    input: &mut fs::File,
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
    progress: Option<&TransferProgress>,
) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;

    // Owner-only until the content is in place. The staging directory is
    // already private, but the file should not depend on that: a copy of a
    // key or a cookie store is readable by nobody else at any point, and
    // `preserve_metadata` widens it to the source's mode once it is whole.
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(destination)
        .at("Could not create", destination)?;
    if !try_copy_sparse(input, &mut output, source, cancelled, progress)? {
        input.seek(io::SeekFrom::Start(0)).at("Could not rewind", source)?;
        output
            .set_len(0)
            .and_then(|()| output.seek(io::SeekFrom::Start(0)).map(|_| ()))
            .at("Could not restart", destination)?;
        copy_buffered(input, &mut output, source, destination, cancelled, progress)?;
    }
    output.sync_all().at("Could not finish", destination)
}

fn copy_buffered(
    input: &mut fs::File,
    output: &mut fs::File,
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
    progress: Option<&TransferProgress>,
) -> Result<()> {
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        if cancelled.load(Ordering::Acquire) {
            bail!("Operation cancelled");
        }
        let read = input.read(&mut buffer).at("Could not read", source)?;
        if read == 0 {
            return Ok(());
        }
        output.write_all(&buffer[..read]).at("Could not write", destination)?;
        if let Some(progress) = progress {
            progress.complete_bytes(read as u64);
        }
    }
}

/// Copy only the extents that hold data, so a sparse source stays sparse.
/// Returns `false` when the file has no holes worth preserving, in which case
/// the caller copies it plainly.
fn try_copy_sparse(
    input: &mut fs::File,
    output: &mut fs::File,
    source: &Path,
    cancelled: &AtomicBool,
    progress: Option<&TransferProgress>,
) -> Result<bool> {
    use rustix::{
        fs::{SeekFrom, seek},
        io::Errno,
    };

    let length = input.metadata().at("Could not inspect", source)?.len();
    if length == 0 {
        return Ok(false);
    }
    let finish = |output: &mut fs::File| {
        output.set_len(length).at("Could not size sparse file", source)?;
        if let Some(progress) = progress {
            progress.complete_bytes(length);
        }
        Ok(true)
    };

    let first_data = match seek(&*input, SeekFrom::Data(0)) {
        Ok(offset) => offset,
        // Entirely a hole.
        Err(Errno::NXIO) => return finish(output),
        Err(_) => return Ok(false),
    };
    let first_hole = match seek(&*input, SeekFrom::Hole(first_data)) {
        Ok(offset) => offset.min(length),
        Err(_) => return Ok(false),
    };
    if first_data == 0 && first_hole >= length {
        return Ok(false);
    }

    let extents = |what: &str| format!("Could not inspect sparse extents in “{}”", what);
    let mut cursor = 0;
    let mut buffer = vec![0_u8; 1024 * 1024];
    while cursor < length {
        if cancelled.load(Ordering::Acquire) {
            bail!("Operation cancelled");
        }
        let data = match seek(&*input, SeekFrom::Data(cursor)) {
            Ok(offset) if offset < length => offset,
            Ok(_) | Err(Errno::NXIO) => break,
            Err(error) => {
                return Err(error).with_context(|| extents(&source.display().to_string()));
            }
        };
        let hole = seek(&*input, SeekFrom::Hole(data))
            .with_context(|| extents(&source.display().to_string()))?
            .min(length);
        input
            .seek(io::SeekFrom::Start(data))
            .and_then(|_| output.seek(io::SeekFrom::Start(data)))
            .at("Could not seek sparse file", source)?;

        let mut remaining = hole.saturating_sub(data);
        while remaining > 0 {
            if cancelled.load(Ordering::Acquire) {
                bail!("Operation cancelled");
            }
            let chunk = usize::try_from(remaining.min(buffer.len() as u64))
                .expect("chunk is bounded by the buffer length");
            input.read_exact(&mut buffer[..chunk]).at("Could not read", source)?;
            output.write_all(&buffer[..chunk]).at("Could not write sparse extent for", source)?;
            remaining -= chunk as u64;
        }
        cursor = hole;
    }
    finish(output)
}

/// Give the copy the source's attributes: extended attributes, timestamps,
/// then the mode, in that order. The mode can take away the owner's own read
/// bit — a `0000` lock file or a write-only drop folder copies fine, it just
/// cannot be *opened* afterwards, and applying timestamps needs an open
/// descriptor. Going last also keeps setuid and setgid off the copy until its
/// content is final.
///
/// The whole mode is preserved, setuid, setgid, and sticky bits included. That
/// is not an escalation: the copy is owned by whoever ran Marcel, so a setuid
/// bit on it grants that user's own privileges and nothing more, exactly as
/// `cp -p` would. A setgid bit that would hand the copy to a group the copier
/// is not in is dropped by the kernel itself. Stripping the bits instead would
/// silently break the copied program, which is the surprise `cp` chose not to
/// spring either.
///
/// `source` is the open descriptor the content was read through, so the
/// attributes come from the object that was copied rather than from whatever
/// `source_path` names by the time they are read.
pub(super) fn preserve_metadata(
    source: &fs::File,
    source_path: &Path,
    destination: &Path,
    metadata: &fs::Metadata,
) -> Result<()> {
    preserve_supported_xattrs(source, source_path, destination)?;

    let mut times = fs::FileTimes::new();
    let mut has_times = false;
    if let Ok(accessed) = metadata.accessed() {
        times = times.set_accessed(accessed);
        has_times = true;
    }
    if let Ok(modified) = metadata.modified() {
        times = times.set_modified(modified);
        has_times = true;
    }
    if has_times {
        fs::File::open(destination)
            .and_then(|file| file.set_times(times))
            .at("Could not preserve timestamps on", destination)?;
    }
    fs::set_permissions(destination, metadata.permissions())
        .at("Could not preserve permissions on", destination)
}

fn preserve_supported_xattrs(
    source: &fs::File,
    source_path: &Path,
    destination: &Path,
) -> Result<()> {
    use xattr::FileExt as _;

    let attributes = match source.list_xattr() {
        Ok(attributes) => attributes,
        Err(error) if xattrs_unsupported(&error) => return Ok(()),
        Err(error) => {
            return Err(error).at("Could not list attributes on", source_path);
        }
    };

    for name in attributes.filter(|name| supported_xattr_name(name)) {
        let Some(value) = source.get_xattr(&name).with_context(|| {
            format!(
                "Could not read attribute “{}” from “{}”",
                name.to_string_lossy(),
                source_path.display()
            )
        })?
        else {
            continue;
        };
        xattr::set(destination, &name, &value).with_context(|| {
            format!(
                "Could not preserve attribute “{}” on “{}”",
                name.to_string_lossy(),
                destination.display()
            )
        })?;
    }
    Ok(())
}

pub(super) fn supported_xattr_name(name: &OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt as _;

    let name = name.as_bytes();
    name.starts_with(b"user.")
        || matches!(name, b"system.posix_acl_access" | b"system.posix_acl_default")
}

pub(super) fn xattrs_unsupported(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::Unsupported || matches!(error.raw_os_error(), Some(45 | 95))
}

// ---------------------------------------------------------------------------
// Merging one directory into another.
//
// Merging is the union of two trees: whatever the destination already has, it
// keeps. That makes it a pure addition, which is what lets it stay inside the
// guardrails. Nothing is displaced, so nothing needs quarantining; undo is
// exactly "remove what was added", which restores the previous state rather
// than approximating it; and a merge that fails partway has added a subset of
// what it planned, which is describable.
//
// The operation as a whole cannot be published atomically the way a copy is,
// because it writes into a tree that is already visible. Each file is still
// published atomically on its own, so a half-written file is never reachable
// under its final name.

/// One directory folded into another, decided before anything is written.
#[derive(Debug, Default)]
struct MergePlan {
    /// Directories to create, parents before children.
    directories: Vec<PathBuf>,
    /// Files, symlinks, and other leaves to copy, as (source, destination).
    files: Vec<(PathBuf, PathBuf)>,
}

/// Decide a whole merge before performing any of it.
///
/// Directories are not conflicts here — they are the points at which the two
/// trees join, so an existing directory is descended into rather than skipped.
/// Anything else already present is left exactly as it is.
fn plan_merge(source: &Path, destination: &Path) -> Result<MergePlan> {
    let mut plan = MergePlan::default();
    let mut pending = vec![(source.to_path_buf(), destination.to_path_buf())];
    while let Some((source, destination)) = pending.pop() {
        let is_dir = inspect(&source)?.file_type().is_dir();
        let occupant =
            describe_occupant(&destination).at("Could not inspect destination", &destination)?;

        match (is_dir, occupant) {
            // Two directories meet: join them and keep walking.
            (true, Some(occupant)) if occupant.is_directory => {}
            // A directory arriving where nothing is: create it, then walk it.
            (true, None) => plan.directories.push(destination.clone()),
            // A leaf arriving where nothing is: copy it.
            (false, None) => {
                plan.files.push((source, destination));
                continue;
            }
            // Anything else is occupied, and a merge keeps what is there.
            _ => continue,
        }

        // Reversed so children are visited in enumeration order, which keeps
        // created parents ahead of their children.
        pending.extend(
            sorted_children(&source)?
                .into_iter()
                .rev()
                .map(|child| (child.path(), destination.join(child.file_name()))),
        );
    }
    Ok(plan)
}

/// Why a merge stopped before it had added everything it planned.
pub(super) enum MergeStop {
    Failed(anyhow::Error),
    Cancelled,
}

/// What a merge added, and why it stopped if it did not finish.
///
/// A merge crosses one commit boundary per entry it creates, so `Result` cannot
/// describe it: after the first `create_dir` the disk has changed no matter what
/// happens next, and a bare `Err` would tell the caller the opposite. Every
/// field below is true of the disk at the moment the merge returned, including
/// when it returned because something failed.
pub(super) struct MergeOutcome {
    /// Exactly what reached the destination, parents before children, so
    /// removing in reverse takes leaves first.
    pub(super) created: Vec<PathSnapshot>,
    /// Whether `created` describes every addition. False when a snapshot could
    /// not be taken or the operation's budget ran out; the additions stand
    /// either way, they simply cannot be taken back.
    pub(super) undoable: bool,
    pub(super) stopped: Option<MergeStop>,
}

/// Perform a planned merge, reporting what it added whether or not it finished.
///
/// `snapshot_limit` is what remains of the *operation's* budget, not a fresh
/// allowance per merge or per leaf: a wide union is exactly the shape that would
/// otherwise grow one record without bound.
pub(super) fn merge_directories(
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
    progress: Option<&TransferProgress>,
    snapshot_limit: usize,
) -> MergeOutcome {
    let not_undoable = |stopped| MergeOutcome { created: Vec::new(), undoable: false, stopped };
    // Prepare: decide the whole merge before writing any of it. Nothing has
    // been created yet, so a plan that cannot be made is an ordinary failure.
    let plan = match plan_merge(source, destination) {
        Ok(plan) => plan,
        Err(error) => {
            return MergeOutcome {
                created: Vec::new(),
                undoable: true,
                stopped: Some(MergeStop::Failed(error)),
            };
        }
    };

    let mut directories: Vec<&PathBuf> = Vec::new();
    let mut files = Vec::new();
    let mut undoable = true;
    let mut stopped = None;
    // Every directory something was published into, synced once at the end.
    let mut touched: BTreeSet<&Path> = BTreeSet::new();

    for directory in &plan.directories {
        if cancelled.load(Ordering::Acquire) {
            stopped = Some(MergeStop::Cancelled);
            break;
        }
        if let Err(error) = fs::create_dir(directory).at("Could not create", directory) {
            stopped = Some(MergeStop::Failed(error));
            break;
        }
        directories.push(directory);
        touched.extend(directory.parent());
        // Past the budget the merge still happens; it simply stops being
        // describable, and says so rather than filling a record half way.
        undoable &= directories.len() < snapshot_limit;
    }

    if stopped.is_none() {
        for (from, to) in &plan.files {
            if cancelled.load(Ordering::Acquire) {
                stopped = Some(MergeStop::Cancelled);
                break;
            }
            // Each file goes through the ordinary copy, which stages it
            // privately and publishes it with one atomic rename. The merge as a
            // whole is not atomic, but no individual file is ever reachable
            // half-written.
            let remaining = if undoable {
                snapshot_limit.saturating_sub(directories.len() + files.len())
            } else {
                0
            };
            match copy_one_unsynced(from, to, cancelled, progress, remaining / 2) {
                Ok(copied) => {
                    touched.extend(to.parent());
                    if copied.overflowed || !copied.undoable {
                        undoable = false;
                    } else if undoable {
                        files.extend(copied.created);
                        undoable &= directories.len() + files.len() < snapshot_limit;
                    }
                }
                Err(error) => {
                    stopped = Some(MergeStop::Failed(error));
                    break;
                }
            }
        }
    }

    for directory in touched {
        sync_directory(directory);
    }

    if !undoable {
        return not_undoable(stopped);
    }

    // Snapshot the new directories only now, and only once nothing further will
    // be written into them. Writing a file into a directory moves that
    // directory's ctime, so recording it at creation time would leave undo
    // comparing against an identity its own copying invalidated. A directory
    // that cannot be described only loses undo; the merge itself stands.
    let mut created = Vec::with_capacity(directories.len() + files.len());
    for directory in directories {
        match PathSnapshot::read(directory) {
            Ok(snapshot) => created.push(snapshot),
            Err(_) => return not_undoable(stopped),
        }
    }
    created.extend(files);
    MergeOutcome { created, undoable: true, stopped }
}

/// Remove exactly what a merge added.
///
/// The whole-tree validation a copy uses cannot work here: a merged directory
/// is full of entries that were already there, so re-walking it and comparing
/// against the record would always disagree. Each item is validated on its own
/// instead, and anything that changed since is left alone.
pub(super) fn remove_merged_items(
    created: &[PathSnapshot],
) -> Result<(), super::journal::PartialRemoval> {
    let mut removed = Vec::new();
    for snapshot in created.iter().rev() {
        let failure = |error: anyhow::Error, removed: Vec<PathBuf>| {
            super::journal::PartialRemoval { removed, error }
        };
        let metadata = match fs::symlink_metadata(&snapshot.path) {
            Ok(metadata) => metadata,
            Err(error) => {
                return Err(failure(
                    anyhow::Error::new(error)
                        .context(format!("Cannot undo: “{}” is missing", snapshot.path.display())),
                    removed,
                ));
            }
        };
        let found = FileIdentity::of(&metadata);
        // A directory's ctime moves whenever its contents change — including
        // as a direct result of the removals this loop is performing on its
        // children — so comparing it would reject the tree undo has just been
        // dismantling. Device and inode still prove it is the same directory,
        // and a directory that gained an entry from elsewhere is caught by the
        // removal itself failing rather than by an identity check.
        let matches = if snapshot.kind == SnapshotKind::Directory {
            found.key == snapshot.identity.key
        } else {
            found == snapshot.identity
        };
        if !matches {
            return Err(failure(
                anyhow::anyhow!(
                    "Cannot undo: “{}” changed or was replaced",
                    snapshot.path.display()
                ),
                removed,
            ));
        }
        let result = match snapshot.kind {
            SnapshotKind::Directory => fs::remove_dir(&snapshot.path),
            SnapshotKind::File | SnapshotKind::Symlink => fs::remove_file(&snapshot.path),
            _ => unreachable!("a merge only ever creates directories, files, and links"),
        };
        match result {
            Ok(()) => removed.push(snapshot.path.clone()),
            Err(error) => {
                return Err(failure(
                    anyhow::Error::new(error)
                        .context(format!("Could not remove “{}”", snapshot.path.display())),
                    removed,
                ));
            }
        }
    }
    Ok(())
}
