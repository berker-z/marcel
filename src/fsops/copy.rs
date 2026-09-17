//! Copying one entry into a private staging directory and publishing it with a
//! single rename, and folding one directory into another.
//!
//! Conceptually follows Yazi's copier and attribute preservation; no Yazi code
//! is copied:
//! https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-fs/src/engine/local/copier.rs
//! https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-fs/src/engine/attrs.rs

use std::{
    collections::HashMap,
    ffi::OsStr,
    fs,
    io::{self, Read as _, Seek as _, Write as _},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use super::local::PathContext as _;
use anyhow::{Context as _, Result, bail};

use super::{
    TransferProgress,
    conflict::describe_occupant,
    identity::FileIdentity,
    journal::{
        PathSnapshot, SnapshotCollector, SnapshotKind, rebase_snapshots,
        refresh_snapshot_identities,
    },
    local::{ensure_unoccupied, inspect, rename_no_replace, sorted_children},
};

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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

/// Reject a copy or move whose destination resolves back inside its own
/// source. A lexical prefix test misses a symlinked destination, which would
/// place Marcel's staging directory inside the tree being walked and make the
/// copy enumerate and re-copy its own output until `PATH_MAX` stops it.
pub(super) fn ensure_not_self_containing(
    source: &Path,
    destination: &Path,
    action: &str,
) -> Result<()> {
    if !inspect(source)?.file_type().is_dir() {
        // Symbolic links are recreated as links rather than traversed, so a
        // link resolving into the destination cannot recurse.
        return Ok(());
    }
    let source_real = source.canonicalize().at("Could not resolve", source)?;
    let parent = destination.parent().context("Destination has no parent directory")?;
    let parent_real = parent.canonicalize().at("Could not resolve", parent)?;
    let name = destination.file_name().context("Destination has no file name")?;
    if parent_real.join(name).starts_with(&source_real) {
        bail!(
            "Cannot {action} “{}” into itself",
            source.file_name().unwrap_or_default().to_string_lossy()
        );
    }
    Ok(())
}

/// Reserve a private staging directory beside the destination.
///
/// This matches the archive staging model: the directory is created atomically
/// with a unique name instead of being probed for and created later, so Marcel
/// can never adopt — and then recursively delete — a path another process
/// created in the gap.
fn reserve_staging_directory(destination: &Path) -> Result<tempfile::TempDir> {
    let parent = destination.parent().context("Copy destination has no parent directory")?;
    let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    tempfile::Builder::new()
        .prefix(&format!(".marcel-copy-{}-{sequence}-", std::process::id()))
        .tempdir_in(parent)
        .at("Could not reserve a temporary copy directory in", parent)
}

/// One unit of copy work. Directories are visited twice so their metadata is
/// applied after their children exist, which an explicit stack expresses
/// directly.
enum CopyStep {
    Visit {
        source: PathBuf,
        destination: PathBuf,
    },
    FinishDirectory {
        source: PathBuf,
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
}

impl Copier<'_> {
    /// Copy one entry using an explicit work stack.
    ///
    /// Recursion here was bounded only by the thread stack: Marcel runs
    /// transfers on `blocking` pool threads with Rust's 2 MiB default, so a
    /// deep enough tree aborted the whole process with a stack overflow
    /// mid-mutation. The archive and delete walkers already used explicit
    /// stacks; this matches them.
    fn copy_tree(&mut self, source: &Path, destination: &Path) -> Result<()> {
        let mut steps = vec![CopyStep::Visit {
            source: source.to_path_buf(),
            destination: destination.to_path_buf(),
        }];
        while let Some(step) = steps.pop() {
            match step {
                CopyStep::Visit { source, destination } => {
                    self.visit(source, destination, &mut steps)?
                }
                CopyStep::FinishDirectory { source, destination, metadata, created_index } => {
                    preserve_metadata(&source, &destination, &metadata)?;
                    self.created.refresh(created_index, &destination, &inspect(&destination)?);
                    self.complete_item();
                }
            }
        }
        Ok(())
    }

    fn visit(
        &mut self,
        source: PathBuf,
        destination: PathBuf,
        steps: &mut Vec<CopyStep>,
    ) -> Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            bail!("Operation cancelled");
        }
        let metadata = inspect(&source)?;
        self.sources.push(&source, &metadata);
        let kind = metadata.file_type();
        if let Some(progress) = self.progress {
            progress.set_current_path(Some(source.clone()));
        }

        if kind.is_dir() {
            fs::create_dir(&destination).at("Could not create", &destination)?;
            let created_index = self.created.push(&destination, &inspect(&destination)?);
            let children = sorted_children(&source)?;
            steps.push(CopyStep::FinishDirectory {
                source,
                destination: destination.clone(),
                metadata,
                created_index,
            });
            // Reversed so children pop in enumeration order.
            for child in children.into_iter().rev() {
                steps.push(CopyStep::Visit {
                    destination: destination.join(child.file_name()),
                    source: child.path(),
                });
            }
            return Ok(());
        }
        if kind.is_file() {
            self.copy_regular_file(&source, &destination, &metadata)?;
            preserve_metadata(&source, &destination, &metadata)?;
        } else if kind.is_symlink() {
            let target = fs::read_link(&source).at("Could not read link", &source)?;
            std::os::unix::fs::symlink(target, &destination).at("Could not copy link", &source)?;
            preserve_supported_xattrs(&source, &destination)?;
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

        copy_file_cancellable(source, destination, self.cancelled, self.progress)?;
        if metadata.nlink() > 1 {
            self.hardlinks.insert(identity, destination.to_path_buf());
        }
        Ok(())
    }
}

pub(super) fn copy_file_cancellable(
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
    progress: Option<&TransferProgress>,
) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut input = fs::File::open(source).at("Could not open", source)?;
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
    if !try_copy_sparse(&mut input, &mut output, source, cancelled, progress)? {
        input.seek(io::SeekFrom::Start(0)).at("Could not rewind", source)?;
        output
            .set_len(0)
            .and_then(|()| output.seek(io::SeekFrom::Start(0)).map(|_| ()))
            .at("Could not restart", destination)?;
        copy_buffered(&mut input, &mut output, source, destination, cancelled, progress)?;
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
pub(super) fn preserve_metadata(
    source: &Path,
    destination: &Path,
    metadata: &fs::Metadata,
) -> Result<()> {
    preserve_supported_xattrs(source, destination)?;

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

fn preserve_supported_xattrs(source: &Path, destination: &Path) -> Result<()> {
    let attributes = match xattr::list(source) {
        Ok(attributes) => attributes,
        Err(error) if xattrs_unsupported(&error) => return Ok(()),
        Err(error) => {
            return Err(error).at("Could not list attributes on", source);
        }
    };

    for name in attributes.filter(|name| supported_xattr_name(name)) {
        let Some(value) = xattr::get(source, &name).with_context(|| {
            format!(
                "Could not read attribute “{}” from “{}”",
                name.to_string_lossy(),
                source.display()
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
            match copy_one(from, to, cancelled, progress, remaining / 2) {
                Ok(copied) if copied.overflowed || !copied.undoable => undoable = false,
                Ok(copied) if undoable => {
                    files.extend(copied.created);
                    undoable &= directories.len() + files.len() < snapshot_limit;
                }
                Ok(_) => {}
                Err(error) => {
                    stopped = Some(MergeStop::Failed(error));
                    break;
                }
            }
        }
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
