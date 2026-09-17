//! What a mutation leaves behind so it can be undone: records, snapshots, and
//! the bounded stacks that hold them.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result, bail};

use super::{
    DirectoryChanges, TransferProgress,
    identity::FileIdentity,
    local::{inspect, sorted_children},
    quarantine::{ReplacedItem, erase_replacement_quarantine},
    trash::TrashRecord,
};

/// How many operations each of the undo and redo stacks retains.
///
/// A record can carry one `PathSnapshot` per descendant, so depth multiplies
/// the worst-case cost of `UNDO_SNAPSHOT_LIMIT` rather than adding to it.
/// Nautilus retains exactly one undoable operation
/// (`nautilus-file-undo-manager.c`), which is the far end of the same trade:
/// depth costs memory and buys reach the user rarely exercises. Marcel keeps a
/// usable stack but not an unreasoned one.
pub const OPERATION_HISTORY_LIMIT: usize = 20;

/// How many `PathSnapshot`s one transfer's undo record may hold.
///
/// One budget for the whole operation, shared by every path that contributes to
/// its record: copied sources and output, what a merge folds into an existing
/// tree, and the trees a move renames. A per-source or per-leaf allowance
/// bounds nothing, because the operation is what the record belongs to.
pub const UNDO_SNAPSHOT_LIMIT: usize = 100_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationRecord {
    CreateDirectory {
        path: PathBuf,
        identity: FileIdentity,
    },
    CreateFile {
        path: PathBuf,
        identity: FileIdentity,
    },
    Copy {
        sources: Vec<PathSnapshot>,
        destination: PathBuf,
        created: Vec<PathSnapshot>,
        replaced: Vec<ReplacedItem>,
        /// Items added by folding a directory into an existing one.
        ///
        /// Held apart from `created` because they sit scattered inside a tree
        /// that was already there, so undo has to validate them one at a time
        /// rather than by re-walking a tree it owns outright.
        merged: Vec<PathSnapshot>,
    },
    Move {
        transfers: Vec<MoveRecord>,
        replaced: Vec<ReplacedItem>,
    },
    Trash {
        records: Vec<TrashRecord>,
    },
    Restore {
        records: Vec<TrashRecord>,
    },
    Rename {
        source: PathBuf,
        destination: PathBuf,
        identity: FileIdentity,
    },
    /// A permission change: the bits before and after, on one object.
    SetMode {
        path: PathBuf,
        identity: FileIdentity,
        previous: u32,
        mode: u32,
    },
    ArchiveCreate {
        sources: Vec<PathSnapshot>,
        destination: PathBuf,
        created: Vec<PathSnapshot>,
    },
    ArchiveExtract {
        source: Vec<PathSnapshot>,
        output: PathBuf,
        /// The published tree; empty when the output was merged instead.
        created: Vec<PathSnapshot>,
        replaced: Vec<ReplacedItem>,
        /// What a merge added to a folder that was already at `output`.
        merged: Vec<PathSnapshot>,
    },
}

impl OperationRecord {
    pub fn path(&self) -> &Path {
        match self {
            Self::CreateDirectory { path, .. } | Self::CreateFile { path, .. } => path,
            Self::Copy { destination, created, .. } => {
                created.first().map(|snapshot| snapshot.path.as_path()).unwrap_or(destination)
            }
            Self::Move { transfers, .. } => transfers
                .first()
                .map(|transfer| transfer.destination.as_path())
                .unwrap_or_else(|| Path::new("")),
            Self::Trash { records } | Self::Restore { records } => {
                records.first().map(TrashRecord::original_path).unwrap_or_else(|| Path::new(""))
            }
            Self::Rename { destination, .. } => destination,
            Self::SetMode { path, .. } => path,
            Self::ArchiveCreate { destination, .. } => destination,
            Self::ArchiveExtract { output, .. } => output,
        }
    }

    pub fn forward_directory_changes(&self) -> DirectoryChanges {
        match self {
            Self::CreateDirectory { path, .. } | Self::CreateFile { path, .. } => {
                DirectoryChanges::upserted(vec![path.clone()])
            }
            Self::Copy { destination, created, .. } => DirectoryChanges::upserted(
                created
                    .iter()
                    .filter(|snapshot| snapshot.path.parent() == Some(destination.as_path()))
                    .map(|snapshot| snapshot.path.clone())
                    .collect(),
            ),
            Self::Move { transfers, .. } => DirectoryChanges {
                removed: transfers.iter().map(|t| t.source.clone()).collect(),
                upserted: transfers.iter().map(|t| t.destination.clone()).collect(),
            },
            Self::Trash { records } => DirectoryChanges::removed(original_paths(records)),
            Self::Restore { records } => DirectoryChanges::upserted(original_paths(records)),
            Self::Rename { source, destination, .. } => DirectoryChanges {
                removed: vec![source.clone()],
                upserted: vec![destination.clone()],
            },
            Self::SetMode { path, .. } => DirectoryChanges::upserted(vec![path.clone()]),
            Self::ArchiveCreate { destination, .. } => {
                DirectoryChanges::upserted(vec![destination.clone()])
            }
            Self::ArchiveExtract { output, .. } => DirectoryChanges::upserted(vec![output.clone()]),
        }
    }

    pub fn reverse_directory_changes(&self) -> DirectoryChanges {
        self.forward_directory_changes().reversed()
    }

    /// The objects this record is holding aside so undo can restore them.
    pub fn replaced_items(&self) -> &[ReplacedItem] {
        match self {
            Self::Copy { replaced, .. }
            | Self::Move { replaced, .. }
            | Self::ArchiveExtract { replaced, .. } => replaced,
            _ => &[],
        }
    }

    /// Give up everything this record was holding aside.
    ///
    /// Called when the record can no longer be reached, which makes its
    /// quarantines unreachable rather than merely unused.
    pub fn release_quarantines(&self) {
        for item in self.replaced_items() {
            erase_replacement_quarantine(item);
        }
    }

    pub fn trash_records(&self) -> Option<&[TrashRecord]> {
        match self {
            Self::Trash { records } | Self::Restore { records } => Some(records),
            _ => None,
        }
    }
}

fn original_paths(records: &[TrashRecord]) -> Vec<PathBuf> {
    records.iter().map(|record| record.original_path().to_path_buf()).collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathSnapshot {
    pub(super) path: PathBuf,
    pub(super) identity: FileIdentity,
    pub(super) kind: SnapshotKind,
}

/// The filesystem object kinds Marcel records in an undo snapshot.
///
/// The variant set mirrors Yazi's `ChaType`, which distinguishes every Unix
/// object kind rather than collapsing the ones it cannot reproduce:
/// https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-fs/src/cha/type.rs
///
/// Marcel keeps the distinction for two reasons. It names the exact obstacle in
/// a refusal instead of saying "special file", and because snapshots compare by
/// equality, a socket replaced by a FIFO at the same path is rejected on kind
/// alone rather than relying on inode and ctime to differ.
///
/// Marcel cannot copy, archive, or recreate the special kinds — but a rename
/// does not care what a directory holds, so recording them lets a moved tree
/// stay undoable while staying out of every path that would have to reproduce
/// or delete it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SnapshotKind {
    Directory,
    File,
    Symlink,
    BlockDevice,
    CharDevice,
    Socket,
    Fifo,
    Unknown,
}

impl SnapshotKind {
    fn of(file_type: fs::FileType) -> Self {
        use std::os::unix::fs::FileTypeExt as _;

        if file_type.is_dir() {
            Self::Directory
        } else if file_type.is_file() {
            Self::File
        } else if file_type.is_symlink() {
            Self::Symlink
        } else if file_type.is_block_device() {
            Self::BlockDevice
        } else if file_type.is_char_device() {
            Self::CharDevice
        } else if file_type.is_socket() {
            Self::Socket
        } else if file_type.is_fifo() {
            Self::Fifo
        } else {
            Self::Unknown
        }
    }

    /// Whether Marcel can move this object but never reproduce or remove it.
    pub(super) fn is_special(self) -> bool {
        !matches!(self, Self::Directory | Self::File | Self::Symlink)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Directory => "a directory",
            Self::File => "a regular file",
            Self::Symlink => "a symbolic link",
            Self::BlockDevice => "a block device",
            Self::CharDevice => "a character device",
            Self::Socket => "a socket",
            Self::Fifo => "a FIFO",
            Self::Unknown => "an unrecognized filesystem object",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveRecord {
    pub(super) source: PathBuf,
    pub(super) destination: PathBuf,
    pub(super) expected_state: Vec<PathSnapshot>,
}

/// The outcome of a mutation that has already committed to the filesystem.
///
/// Marcel's mutation APIs follow one rule: every fallible traversal,
/// validation, and journal construction happens *before* the filesystem is
/// touched (prepare), the mutation itself is one minimal call (commit), and
/// everything afterwards is infallible in-memory work (finalize).
///
/// `record` is `None` when the mutation succeeded but its undo bookkeeping
/// could not be captured. Callers must present that as success without undo.
/// Returning `Err` after a commit would tell the caller "nothing happened"
/// while the disk says otherwise, leaving the browser projection, the
/// clipboard, the operation journal, and the user's notification in
/// disagreement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommittedOperation {
    path: PathBuf,
    changes: DirectoryChanges,
    record: Option<OperationRecord>,
}

impl CommittedOperation {
    /// A commit whose observable effect is known even when `record` is `None`.
    pub fn new(path: PathBuf, changes: DirectoryChanges, record: Option<OperationRecord>) -> Self {
        Self { path, changes, record }
    }

    /// A commit that published one new path, undoable when `record` is present.
    pub fn published(path: PathBuf, record: Option<OperationRecord>) -> Self {
        Self::new(path.clone(), DirectoryChanges::upserted(vec![path]), record)
    }

    /// The published path, which is known whether or not undo was retained.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The directory-reducer effect of the commit. Always populated, so the
    /// browser stays consistent with the disk even when undo was lost.
    pub fn changes(&self) -> &DirectoryChanges {
        &self.changes
    }

    pub fn is_undoable(&self) -> bool {
        self.record.is_some()
    }

    pub fn into_record(self) -> Option<OperationRecord> {
        self.record
    }
}

/// The outcome of a mutation that may cross more than one commit point.
///
/// `CommittedOperation` models a single commit correctly, but Undo, Redo, and
/// the Trash paths can rename item A, fail on item B, and then compensate. Two
/// states cannot describe that, and collapsing it into an ordinary `Err` told
/// the caller "nothing happened" while the journal had already been
/// invalidated.
///
/// Nautilus solves the same problem by discarding its undo record whenever an
/// undo fails for any reason other than user cancellation
/// (`nautilus-file-undo-manager.c`, `undo_info_apply_ready`). It can afford a
/// blunt rule because it stores no identities to go stale. Marcel keeps its
/// identity checks, so it splits that rule in two: a failure that provably
/// never reached the disk keeps the record retryable, and anything past the
/// first commit discards it.
#[derive(Debug)]
pub enum MutationOutcome {
    /// Nothing reached the filesystem. The history record still describes the
    /// disk, so the caller keeps it and the user can retry.
    Unchanged(anyhow::Error),
    /// The mutation committed. Undo bookkeeping may still be absent.
    Committed(CommittedOperation),
    /// The mutation crossed its commit point and then failed.
    ///
    /// `changes` describes whatever reached the disk, which may be empty when
    /// compensation put everything back. Empty does *not* mean retryable: a
    /// compensating rename bumps the root's ctime, so the record that produced
    /// this attempt can no longer validate and must be discarded.
    Discarded { changes: DirectoryChanges, error: anyhow::Error },
}

impl MutationOutcome {
    /// Treat a failure that has not reached the filesystem as retryable.
    pub(super) fn unchanged(error: impl Into<anyhow::Error>) -> Self {
        Self::Unchanged(error.into())
    }

    /// Treat a failure past the first commit as history-invalidating.
    pub(super) fn discarded(changes: DirectoryChanges, error: impl Into<anyhow::Error>) -> Self {
        Self::Discarded { changes, error: error.into() }
    }

    /// Whether the history record survives this outcome.
    ///
    /// `Unchanged` is the only failure a caller may retry: every other failure
    /// crossed a commit point and left the record describing a disk state that
    /// no longer exists.
    pub fn keeps_history(&self) -> bool {
        matches!(self, Self::Unchanged(_))
    }
}

impl From<Result<CommittedOperation>> for MutationOutcome {
    /// A single-commit mutation: it either happened or it did not.
    fn from(result: Result<CommittedOperation>) -> Self {
        match result {
            Ok(committed) => Self::Committed(committed),
            Err(error) => Self::Unchanged(error),
        }
    }
}

#[cfg(test)]
impl MutationOutcome {
    #[track_caller]
    pub(super) fn unwrap(self) -> CommittedOperation {
        match self {
            Self::Committed(committed) => committed,
            Self::Unchanged(error) => panic!("expected a commit, got Unchanged: {error}"),
            Self::Discarded { error, .. } => panic!("expected a commit, got Discarded: {error}"),
        }
    }

    pub(super) fn is_err(&self) -> bool {
        !matches!(self, Self::Committed(_))
    }
}

#[derive(Debug)]
pub struct OperationJournal {
    undo: VecDeque<OperationRecord>,
    redo: VecDeque<OperationRecord>,
    limit: usize,
}

impl Default for OperationJournal {
    fn default() -> Self {
        Self::new(OPERATION_HISTORY_LIMIT)
    }
}

impl OperationJournal {
    pub fn new(limit: usize) -> Self {
        Self { undo: VecDeque::new(), redo: VecDeque::new(), limit }
    }

    /// Take every record out, leaving the journal empty.
    ///
    /// Used when the journal is going away, so its records can release what
    /// they were holding aside before they are dropped.
    pub fn drain(&mut self) -> impl Iterator<Item = OperationRecord> + use<> {
        std::mem::take(&mut self.undo).into_iter().chain(std::mem::take(&mut self.redo))
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Record an operation, returning every record this displaced.
    ///
    /// A displaced record can never be reached again, so anything it was
    /// holding aside — a replaced file waiting to be restored — is now
    /// unreachable garbage. Returning them rather than dropping them silently
    /// is what keeps quarantines from accumulating for the life of the process.
    #[must_use = "evicted records may hold quarantined data that must be released"]
    pub fn record(&mut self, operation: OperationRecord) -> Vec<OperationRecord> {
        let mut evicted = std::mem::take(&mut self.redo).into_iter().collect::<Vec<_>>();
        evicted.extend(push_bounded(&mut self.undo, operation, self.limit));
        evicted
    }

    /// Take the record the next step of `direction` would apply.
    pub fn begin(&mut self, direction: HistoryDirection) -> Option<OperationRecord> {
        self.stack_mut(direction).pop_back()
    }

    /// A finished step lands its replacement on the opposite stack.
    pub fn finish(&mut self, direction: HistoryDirection, operation: OperationRecord) {
        let limit = self.limit;
        push_bounded(self.stack_mut(direction.opposite()), operation, limit);
    }

    /// A step that never reached the disk goes back where it came from.
    pub fn cancel(&mut self, direction: HistoryDirection, operation: OperationRecord) {
        let limit = self.limit;
        push_bounded(self.stack_mut(direction), operation, limit);
    }

    fn stack_mut(&mut self, direction: HistoryDirection) -> &mut VecDeque<OperationRecord> {
        match direction {
            HistoryDirection::Undo => &mut self.undo,
            HistoryDirection::Redo => &mut self.redo,
        }
    }
}

/// Which way through the journal a step goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryDirection {
    Undo,
    Redo,
}

impl HistoryDirection {
    pub fn opposite(self) -> Self {
        match self {
            Self::Undo => Self::Redo,
            Self::Redo => Self::Undo,
        }
    }
}

fn push_bounded(
    stack: &mut VecDeque<OperationRecord>,
    operation: OperationRecord,
    limit: usize,
) -> Vec<OperationRecord> {
    if limit == 0 {
        return vec![operation];
    }
    let mut evicted = Vec::new();
    while stack.len() >= limit {
        evicted.extend(stack.pop_front());
    }
    stack.push_back(operation);
    evicted
}

// ---------------------------------------------------------------------------
// Snapshots: describing a tree so a later step can prove it is unchanged.

/// Collects snapshots up to a budget, remembering whether it ran out.
pub(super) struct SnapshotCollector {
    pub(super) snapshots: Vec<PathSnapshot>,
    limit: usize,
    pub(super) overflowed: bool,
}

impl SnapshotCollector {
    pub(super) fn new(limit: usize) -> Self {
        Self { snapshots: Vec::new(), limit, overflowed: false }
    }

    pub(super) fn push(&mut self, path: &Path, metadata: &fs::Metadata) -> Option<usize> {
        if self.snapshots.len() >= self.limit {
            self.overflowed = true;
            return None;
        }
        self.snapshots.push(PathSnapshot::of(path, metadata));
        Some(self.snapshots.len() - 1)
    }

    pub(super) fn refresh(&mut self, index: Option<usize>, path: &Path, metadata: &fs::Metadata) {
        if let Some(index) = index {
            self.snapshots[index] = PathSnapshot::of(path, metadata);
        }
    }
}

impl PathSnapshot {
    pub(super) fn of(path: &Path, metadata: &fs::Metadata) -> Self {
        Self {
            path: path.to_path_buf(),
            identity: FileIdentity::of(metadata),
            kind: SnapshotKind::of(metadata.file_type()),
        }
    }

    pub(super) fn read(path: &Path) -> Result<Self> {
        inspect(path).map(|metadata| Self::of(path, &metadata))
    }
}

pub(super) fn snapshot_tree(root: &Path) -> Result<Vec<PathSnapshot>> {
    snapshot_tree_within(root, usize::MAX)
}

/// Snapshot a tree for bookkeeping, within what the record may hold.
///
/// The walk stops at the budget rather than reading the whole tree and
/// discarding it: bounding the record while still paying to build it would
/// bound the wrong thing. Callers treat the failure as success-without-undo,
/// never as a reason to refuse the mutation.
pub(super) fn snapshot_tree_within(root: &Path, limit: usize) -> Result<Vec<PathSnapshot>> {
    let mut snapshots = Vec::new();
    snapshot_entry(root, &mut snapshots, limit)?;
    Ok(snapshots)
}

/// Snapshot a tree for a mutation whose undo has to *delete* it.
///
/// Marcel cannot recreate a special file, so a tree holding one must not enter
/// a record that Undo would later erase. Callers downgrade the failure to
/// success-without-undo.
pub(super) fn snapshot_removable_tree(root: &Path) -> Result<Vec<PathSnapshot>> {
    let snapshots = snapshot_tree(root)?;
    reject_special_entries(&snapshots)?;
    Ok(snapshots)
}

pub(super) fn reject_special_entries(snapshots: &[PathSnapshot]) -> Result<()> {
    match snapshots.iter().find(|snapshot| snapshot.kind.is_special()) {
        Some(special) => bail!(
            "Special files are not supported yet: “{}” is {}",
            special.path.display(),
            special.kind.label()
        ),
        None => Ok(()),
    }
}

/// Snapshot a tree in pre-order using an explicit stack. Parents must precede
/// their children so `remove_snapshotted_tree` can delete in reverse.
fn snapshot_entry(path: &Path, snapshots: &mut Vec<PathSnapshot>, limit: usize) -> Result<()> {
    let mut pending = vec![path.to_path_buf()];
    while let Some(path) = pending.pop() {
        if snapshots.len() >= limit {
            bail!(
                "“{}” holds more than the {limit} entries one undo record may describe",
                path.display()
            );
        }
        let snapshot = PathSnapshot::read(&path)?;
        let kind = snapshot.kind;
        snapshots.push(snapshot);
        if kind == SnapshotKind::Directory {
            pending.extend(sorted_children(&path)?.into_iter().rev().map(|e| e.path()));
        }
    }
    Ok(())
}

/// Rebase recorded paths from one root to another.
///
/// Infallible by construction: every snapshot in a tree is rooted at `from`,
/// so this runs after a commit without being able to fail it.
pub(super) fn rebase_snapshots(snapshots: &mut [PathSnapshot], from: &Path, to: &Path) {
    for snapshot in snapshots {
        let Ok(relative) = snapshot.path.strip_prefix(from) else {
            continue;
        };
        snapshot.path =
            if relative.as_os_str().is_empty() { to.to_path_buf() } else { to.join(relative) };
    }
}

/// Re-read the identities a commit invalidated, refusing any substitution.
///
/// A rename bumps the renamed root's ctime, so the recorded identity has to be
/// read again — but "whatever is at that path now" is not the same claim as
/// "the object Marcel just committed". Another process can put something else
/// there in between, and adopting it would enter a stranger's object into a
/// record Undo is entitled to delete.
///
/// Device and inode survive a rename, so they are the key carried across the
/// boundary, and the kind is compared with them. Ctime cannot join them: the
/// commit is precisely what moved it, which is why this function exists.
///
/// That leaves one case the key cannot decide — an object deleted and replaced
/// by one the filesystem gives the same inode number. Every substitution that
/// arrives by rename, which is how anything is published atomically, does
/// change the number and is caught. A mismatch is not an error, since the
/// mutation committed; it is success without undo. Returns `false` when any
/// entry could not be adopted.
pub(super) fn refresh_snapshot_identities(snapshots: &mut [PathSnapshot]) -> bool {
    let mut complete = true;
    for snapshot in snapshots {
        let refreshed = PathSnapshot::read(&snapshot.path).ok().filter(|found| {
            found.kind == snapshot.kind && found.identity.key == snapshot.identity.key
        });
        match refreshed {
            Some(found) => snapshot.identity = found.identity,
            None => complete = false,
        }
    }
    complete
}

pub(super) fn validate_snapshot_tree(snapshots: &[PathSnapshot]) -> Result<()> {
    let expected = snapshots
        .iter()
        .map(|snapshot| (snapshot.path.as_path(), snapshot))
        .collect::<HashMap<_, _>>();
    let mut actual = Vec::with_capacity(snapshots.len());
    for root in top_level_paths(snapshots) {
        // Validation compares against a record that already exists, so it is
        // bounded by that record rather than by a budget of its own.
        snapshot_entry(&root, &mut actual, usize::MAX).with_context(|| {
            format!("Cannot continue: “{}” changed or no longer exists", root.display())
        })?;
    }
    if actual.len() != snapshots.len() {
        bail!("Cannot continue: the recorded directory contents changed");
    }
    for actual in actual {
        if expected.get(actual.path.as_path()).is_none_or(|expected| *expected != &actual) {
            bail!("Cannot continue: “{}” changed or was replaced", actual.path.display());
        }
    }
    Ok(())
}

/// A tree removal that stopped partway.
///
/// `removed` is empty when the failure happened during validation, which lets
/// the caller keep an undo record that still describes the disk.
pub(super) struct PartialRemoval {
    pub(super) removed: Vec<PathBuf>,
    pub(super) error: anyhow::Error,
}

pub(super) fn remove_snapshotted_tree(snapshots: &[PathSnapshot]) -> Result<(), PartialRemoval> {
    // Only copy and archive output reaches here, and neither can contain a
    // special file. Refuse before removing anything rather than discovering
    // it partway through an irreversible walk.
    if let Err(error) =
        validate_snapshot_tree(snapshots).and_then(|()| reject_special_entries(snapshots))
    {
        return Err(PartialRemoval { removed: Vec::new(), error });
    }

    // Quarantine first, by reusing permanent deletion rather than walking the
    // tree in place. Each validated root leaves its path with one atomic
    // rename, so undo's visible effect happens at once instead of arriving
    // leaf by leaf; a failure before that point rolls back and nothing moved;
    // and a failure while erasing leaves a recoverable `.marcel-delete-*`
    // remnant, which the browser already knows how to point the user at,
    // rather than a half-removed tree at the path they are looking at.
    let outcome = super::delete::delete_paths(
        &top_level_paths(snapshots),
        Arc::new(TransferProgress::default()),
    );
    match outcome.failures.into_iter().next() {
        None => Ok(()),
        Some(failure) => Err(PartialRemoval {
            removed: outcome.completed,
            error: anyhow::anyhow!("{}", failure.message),
        }),
    }
}

pub(super) fn top_level_paths(snapshots: &[PathSnapshot]) -> Vec<PathBuf> {
    let directories = snapshots
        .iter()
        .filter(|snapshot| snapshot.kind == SnapshotKind::Directory)
        .map(|snapshot| snapshot.path.as_path())
        .collect::<HashSet<_>>();
    snapshots
        .iter()
        .filter(|candidate| {
            !candidate.path.ancestors().skip(1).any(|ancestor| directories.contains(ancestor))
        })
        .map(|snapshot| snapshot.path.clone())
        .collect()
}
