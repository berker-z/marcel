use std::{
    collections::{HashMap, VecDeque},
    ffi::OsStr,
    fs,
    io::{self, Read as _, Seek as _, Write as _},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use anyhow::{Context as _, Result, bail};

use crate::{
    archive_ops::{MAX_ARCHIVE_ENTRIES, create_zip_archive, extract_archive},
    conflict::{
        ConflictPolicy, ConflictRequest, ConflictResponse, describe_occupant, unique_name_in,
    },
    local_fs::rename_no_replace,
    trash_ops::{TrashRecord, restore_trash_records, retrash_records},
};

/// How many operations each of the undo and redo stacks retains.
///
/// A record can carry one `PathSnapshot` per descendant, so depth multiplies
/// the worst-case cost of `COPY_UNDO_SNAPSHOT_LIMIT` rather than adding to it.
/// Nautilus retains exactly one undoable operation
/// (`nautilus-file-undo-manager.c`), which is the far end of the same trade:
/// depth costs memory and buys reach the user rarely exercises. Marcel keeps a
/// usable stack but not an unreasoned one.
pub const OPERATION_HISTORY_LIMIT: usize = 20;
pub const COPY_UNDO_SNAPSHOT_LIMIT: usize = 100_000;

/// How many bytes of replaced data one operation may hold aside for undo.
///
/// Quarantining what a replace displaced is what makes replacement reversible,
/// but it is real disk held for as long as the record lives. This follows the
/// rule `COPY_UNDO_SNAPSHOT_LIMIT` already sets: past the budget the operation
/// still happens, it simply stops being undoable and says so. The alternative —
/// refusing to replace large files — would be a worse answer to a question the
/// user already asked.
pub const REPLACEMENT_UNDO_BYTE_LIMIT: u64 = 1024 * 1024 * 1024;

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static REPLACEMENT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct CopyContext {
    hardlinks: HashMap<(u64, u64), PathBuf>,
}

struct SnapshotCollector {
    snapshots: Vec<PathSnapshot>,
    limit: usize,
    overflowed: bool,
}

impl SnapshotCollector {
    fn new(limit: usize) -> Self {
        Self {
            snapshots: Vec::new(),
            limit,
            overflowed: false,
        }
    }

    fn push(&mut self, path: &Path, metadata: &fs::Metadata) -> Result<Option<usize>> {
        if self.snapshots.len() >= self.limit {
            self.overflowed = true;
            return Ok(None);
        }
        let index = self.snapshots.len();
        self.snapshots.push(snapshot_from_metadata(path, metadata)?);
        Ok(Some(index))
    }

    fn refresh(
        &mut self,
        index: Option<usize>,
        path: &Path,
        metadata: &fs::Metadata,
    ) -> Result<()> {
        if let Some(index) = index {
            self.snapshots[index] = snapshot_from_metadata(path, metadata)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    device: u64,
    inode: u64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationRecord {
    CreateDirectory {
        path: PathBuf,
        identity: FileIdentity,
    },
    Copy {
        sources: Vec<PathSnapshot>,
        destination: PathBuf,
        created: Vec<PathSnapshot>,
        replaced: Vec<ReplacedItem>,
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
    ArchiveCreate {
        sources: Vec<PathSnapshot>,
        destination: PathBuf,
        created: Vec<PathSnapshot>,
    },
    ArchiveExtract {
        source: Vec<PathSnapshot>,
        output: PathBuf,
        created: Vec<PathSnapshot>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathSnapshot {
    path: PathBuf,
    identity: FileIdentity,
    kind: SnapshotKind,
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
enum SnapshotKind {
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
    /// Whether Marcel can move this object but never reproduce or remove it.
    fn is_special(self) -> bool {
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
    source: PathBuf,
    destination: PathBuf,
    expected_state: Vec<PathSnapshot>,
}

/// An object a transfer displaced, held aside so undo can put it back.
///
/// Nautilus keeps nothing here: it overwrites in place, and undo of a copy
/// deletes the destinations, so whatever was replaced is gone for good. Marcel
/// treats a replacement as reversible or reports that it is not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplacedItem {
    /// Where it lived, and where undo must put it back.
    path: PathBuf,
    /// Where it is being held meanwhile.
    quarantine: PathBuf,
    identity: FileIdentity,
}

impl ReplacedItem {
    /// The hidden path holding this item, so an evicted record can release it.
    pub fn quarantine(&self) -> &Path {
        &self.quarantine
    }
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
    /// A commit whose undo record was captured successfully.
    pub fn recorded(record: OperationRecord) -> Self {
        Self {
            path: record.path().to_path_buf(),
            changes: record.forward_directory_changes(),
            record: Some(record),
        }
    }

    /// A commit whose observable effect is known even when `record` is `None`.
    pub fn new(path: PathBuf, changes: DirectoryChanges, record: Option<OperationRecord>) -> Self {
        Self {
            path,
            changes,
            record,
        }
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
    Discarded {
        changes: DirectoryChanges,
        error: anyhow::Error,
    },
}

impl MutationOutcome {
    /// Treat a failure that has not reached the filesystem as retryable.
    fn unchanged(error: impl Into<anyhow::Error>) -> Self {
        Self::Unchanged(error.into())
    }

    /// Treat a failure past the first commit as history-invalidating.
    fn discarded(changes: DirectoryChanges, error: impl Into<anyhow::Error>) -> Self {
        Self::Discarded {
            changes,
            error: error.into(),
        }
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

#[cfg(test)]
impl MutationOutcome {
    #[track_caller]
    fn unwrap(self) -> CommittedOperation {
        match self {
            Self::Committed(committed) => committed,
            Self::Unchanged(error) => panic!("expected a commit, got Unchanged: {error}"),
            Self::Discarded { error, .. } => panic!("expected a commit, got Discarded: {error}"),
        }
    }

    fn is_err(&self) -> bool {
        !matches!(self, Self::Committed(_))
    }
}

/// One source that reached one destination. Recorded exactly rather than
/// reconstructed from file names: basename reconciliation silently conflates
/// same-named sources the moment a transfer can span directories.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedTransfer {
    pub source: PathBuf,
    pub destination: PathBuf,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirectoryChanges {
    pub removed: Vec<PathBuf>,
    pub upserted: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferMode {
    Copy,
    Move,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferFailure {
    pub path: PathBuf,
    pub message: String,
}

/// The result of a transfer, accounting for every requested source exactly
/// once across `completed`, `skipped`, `failed`, and `cancelled`.
///
/// Cancellation previously recorded one failure and abandoned the loop, so a
/// hundred-item transfer stopped at item ten reported ten results and left
/// eighty-nine sources with no state at all. Nothing downstream could tell
/// "skipped by the user" from "silently forgotten".
#[derive(Debug)]
pub struct TransferOutcome {
    pub operation: Option<OperationRecord>,
    pub completed: Vec<CompletedTransfer>,
    pub failures: Vec<TransferFailure>,
    /// Sources the user declined to transfer because their destination was
    /// occupied.
    pub skipped: Vec<PathBuf>,
    /// Sources that were already where they were asked to go.
    ///
    /// Dragging a selection onto the folder it already lives in asks for
    /// nothing, so this is neither work done nor work refused.
    pub already_in_place: Vec<PathBuf>,
    /// Sources never attempted, because the operation was cancelled first.
    pub cancelled: Vec<PathBuf>,
    pub undo_unavailable: bool,
}

impl TransferOutcome {
    /// Every requested source, in the state it ended in.
    pub fn accounted(&self) -> usize {
        self.completed.len()
            + self.failures.len()
            + self.skipped.len()
            + self.already_in_place.len()
            + self.cancelled.len()
    }
}

impl TransferOutcome {
    pub fn completed_destinations(&self) -> Vec<PathBuf> {
        self.completed
            .iter()
            .map(|transfer| transfer.destination.clone())
            .collect()
    }

    pub fn completed_sources(&self) -> Vec<PathBuf> {
        self.completed
            .iter()
            .map(|transfer| transfer.source.clone())
            .collect()
    }
}

#[derive(Debug, Default)]
pub struct TransferProgress {
    preparing: AtomicBool,
    total_items: AtomicU64,
    completed_items: AtomicU64,
    total_bytes: AtomicU64,
    completed_bytes: AtomicU64,
    current_path: Mutex<Option<PathBuf>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferProgressSnapshot {
    pub preparing: bool,
    pub total_items: u64,
    pub completed_items: u64,
    pub total_bytes: u64,
    pub completed_bytes: u64,
    pub current_path: Option<PathBuf>,
}

impl TransferProgress {
    pub fn snapshot(&self) -> TransferProgressSnapshot {
        TransferProgressSnapshot {
            preparing: self.preparing.load(Ordering::Relaxed),
            total_items: self.total_items.load(Ordering::Relaxed),
            completed_items: self.completed_items.load(Ordering::Relaxed),
            total_bytes: self.total_bytes.load(Ordering::Relaxed),
            completed_bytes: self.completed_bytes.load(Ordering::Relaxed),
            current_path: self
                .current_path
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
        }
    }

    pub(crate) fn set_preparing(&self, preparing: bool) {
        self.preparing.store(preparing, Ordering::Relaxed);
    }

    pub(crate) fn add_total(&self, items: u64, bytes: u64) {
        self.total_items.fetch_add(items, Ordering::Relaxed);
        self.total_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn set_current_path(&self, path: Option<PathBuf>) {
        *self
            .current_path
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = path;
    }

    pub(crate) fn complete_item(&self) {
        self.completed_items.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn complete_bytes(&self, bytes: u64) {
        self.completed_bytes.fetch_add(bytes, Ordering::Relaxed);
    }
}

impl OperationRecord {
    pub fn path(&self) -> &Path {
        match self {
            Self::CreateDirectory { path, .. } => path,
            Self::Copy {
                destination,
                created,
                ..
            } => created
                .first()
                .map(|snapshot| snapshot.path.as_path())
                .unwrap_or(destination),
            Self::Move { transfers, .. } => transfers
                .first()
                .map(|transfer| transfer.destination.as_path())
                .unwrap_or_else(|| Path::new("")),
            Self::Trash { records } | Self::Restore { records } => records
                .first()
                .map(TrashRecord::original_path)
                .unwrap_or_else(|| Path::new("")),
            Self::Rename { destination, .. } => destination,
            Self::ArchiveCreate { destination, .. } => destination,
            Self::ArchiveExtract { output, .. } => output,
        }
    }

    pub fn forward_directory_changes(&self) -> DirectoryChanges {
        match self {
            Self::CreateDirectory { path, .. } => DirectoryChanges {
                upserted: vec![path.clone()],
                ..DirectoryChanges::default()
            },
            Self::Copy {
                destination,
                created,
                ..
            } => DirectoryChanges {
                upserted: created
                    .iter()
                    .filter(|snapshot| snapshot.path.parent() == Some(destination.as_path()))
                    .map(|snapshot| snapshot.path.clone())
                    .collect(),
                ..DirectoryChanges::default()
            },
            Self::Move { transfers, .. } => DirectoryChanges {
                removed: transfers
                    .iter()
                    .map(|transfer| transfer.source.clone())
                    .collect(),
                upserted: transfers
                    .iter()
                    .map(|transfer| transfer.destination.clone())
                    .collect(),
            },
            Self::Trash { records } => DirectoryChanges {
                removed: records
                    .iter()
                    .map(|record| record.original_path().to_path_buf())
                    .collect(),
                ..DirectoryChanges::default()
            },
            Self::Restore { records } => DirectoryChanges {
                upserted: records
                    .iter()
                    .map(|record| record.original_path().to_path_buf())
                    .collect(),
                ..DirectoryChanges::default()
            },
            Self::Rename {
                source,
                destination,
                ..
            } => DirectoryChanges {
                removed: vec![source.clone()],
                upserted: vec![destination.clone()],
            },
            Self::ArchiveCreate { destination, .. } => DirectoryChanges {
                upserted: vec![destination.clone()],
                ..DirectoryChanges::default()
            },
            Self::ArchiveExtract { output, .. } => DirectoryChanges {
                upserted: vec![output.clone()],
                ..DirectoryChanges::default()
            },
        }
    }

    pub fn reverse_directory_changes(&self) -> DirectoryChanges {
        let forward = self.forward_directory_changes();
        DirectoryChanges {
            removed: forward.upserted,
            upserted: forward.removed,
        }
    }

    /// The objects this record is holding aside so undo can restore them.
    pub fn replaced_items(&self) -> &[ReplacedItem] {
        match self {
            Self::Copy { replaced, .. } | Self::Move { replaced, .. } => replaced,
            _ => &[],
        }
    }

    /// Give up everything this record was holding aside.
    ///
    /// Called when the record can no longer be reached, which makes its
    /// quarantines unreachable rather than merely unused.
    pub fn release_quarantines(&self) {
        for item in self.replaced_items() {
            erase_replacement_quarantine(item.quarantine());
        }
    }

    pub fn trash_records(&self) -> Option<&[TrashRecord]> {
        match self {
            Self::Trash { records } | Self::Restore { records } => Some(records),
            _ => None,
        }
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
        Self {
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            limit,
        }
    }

    /// Take every record out, leaving the journal empty.
    ///
    /// Used when the journal is going away, so its records can release what
    /// they were holding aside before they are dropped.
    pub fn drain(&mut self) -> impl Iterator<Item = OperationRecord> + use<> {
        std::mem::take(&mut self.undo)
            .into_iter()
            .chain(std::mem::take(&mut self.redo))
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
        let mut evicted = std::mem::take(&mut self.redo)
            .into_iter()
            .collect::<Vec<_>>();
        evicted.extend(push_bounded(&mut self.undo, operation, self.limit));
        evicted
    }

    pub fn begin_undo(&mut self) -> Option<OperationRecord> {
        self.undo.pop_back()
    }

    pub fn finish_undo(&mut self, operation: OperationRecord) {
        push_bounded(&mut self.redo, operation, self.limit);
    }

    pub fn cancel_undo(&mut self, operation: OperationRecord) {
        push_bounded(&mut self.undo, operation, self.limit);
    }

    pub fn begin_redo(&mut self) -> Option<OperationRecord> {
        self.redo.pop_back()
    }

    pub fn finish_redo(&mut self, operation: OperationRecord) {
        push_bounded(&mut self.undo, operation, self.limit);
    }

    pub fn cancel_redo(&mut self, operation: OperationRecord) {
        push_bounded(&mut self.redo, operation, self.limit);
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
        bail!(
            "“{}” is reserved and cannot be used as a name",
            name.to_string_lossy()
        );
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
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("Could not inspect “{}”", source.display()))?;
    let expected = file_identity(&metadata);
    validate_file_identity(source, &expected, "rename")?;
    // Commit.
    rename_no_replace(source, &destination).with_context(|| {
        format!(
            "Could not rename “{}” to “{}”",
            source.display(),
            destination.display()
        )
    })?;
    // Finalize: the entry is renamed on disk. A failed inspection costs undo,
    // never the rename itself.
    let record = fs::symlink_metadata(&destination)
        .ok()
        .map(|metadata| OperationRecord::Rename {
            source: source.to_path_buf(),
            destination: destination.clone(),
            identity: file_identity(&metadata),
        });
    Ok(CommittedOperation::new(
        destination.clone(),
        DirectoryChanges {
            removed: vec![source.to_path_buf()],
            upserted: vec![destination],
        },
        record,
    ))
}

pub fn undo_operation(operation: &OperationRecord) -> MutationOutcome {
    let reversed = operation.reverse_directory_changes();
    match operation {
        OperationRecord::CreateDirectory { path, identity } => {
            // Prepare: every check below runs before the single commit, so a
            // failure here provably left the directory in place.
            let metadata = match fs::symlink_metadata(path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    return MutationOutcome::unchanged(anyhow::Error::new(error).context(format!(
                        "Cannot undo: “{}” no longer exists",
                        path.display()
                    )));
                }
            };
            if !metadata.file_type().is_dir() {
                return MutationOutcome::unchanged(anyhow::anyhow!(
                    "Cannot undo: “{}” is no longer a directory",
                    path.display()
                ));
            }
            match fs::read_dir(path) {
                Ok(mut entries) => {
                    if entries.next().is_some() {
                        return MutationOutcome::unchanged(anyhow::anyhow!(
                            "Cannot undo: “{}” is no longer empty",
                            path.display()
                        ));
                    }
                }
                Err(error) => {
                    return MutationOutcome::unchanged(
                        anyhow::Error::new(error)
                            .context(format!("Cannot inspect “{}”", path.display())),
                    );
                }
            }
            if file_identity(&metadata) != *identity {
                return MutationOutcome::unchanged(anyhow::anyhow!(
                    "Cannot undo: “{}” changed or was replaced",
                    path.display()
                ));
            }
            // Commit: `remove_dir` either removes the directory or leaves it.
            if let Err(error) = fs::remove_dir(path) {
                return MutationOutcome::unchanged(
                    anyhow::Error::new(error)
                        .context(format!("Could not remove “{}”", path.display())),
                );
            }
            MutationOutcome::Committed(CommittedOperation::new(
                path.clone(),
                reversed,
                Some(operation.clone()),
            ))
        }
        OperationRecord::Copy { created, .. }
        | OperationRecord::ArchiveCreate { created, .. }
        | OperationRecord::ArchiveExtract { created, .. } => {
            let replaced = match operation {
                OperationRecord::Copy { replaced, .. } => replaced.as_slice(),
                _ => &[],
            };
            match remove_snapshotted_tree(created) {
                Ok(()) => {
                    // The output is gone, so whatever it displaced can come
                    // back. A failure here leaves the copy removed, so it
                    // cannot be reported as though nothing happened.
                    if let Err(error) = restore_replaced_items(replaced) {
                        return MutationOutcome::discarded(reversed, error);
                    }
                    MutationOutcome::Committed(CommittedOperation::new(
                        operation.path().to_path_buf(),
                        reversed,
                        // Redo would have to displace the restored items all
                        // over again, which is a fresh decision the user has
                        // not made, so a replacement is undoable but not
                        // redoable.
                        replaced.is_empty().then(|| operation.clone()),
                    ))
                }
                // Nothing was removed, so the output still matches the record.
                Err(failure) if failure.removed.is_empty() => {
                    MutationOutcome::unchanged(failure.error)
                }
                // Part of the output is gone. The record claims a whole tree
                // that no longer exists, so it cannot be retried.
                Err(failure) => MutationOutcome::discarded(
                    DirectoryChanges {
                        removed: failure.removed,
                        upserted: Vec::new(),
                    },
                    failure.error,
                ),
            }
        }
        OperationRecord::Move {
            transfers,
            replaced,
        } => {
            // Prepare: validating every transfer also produces the snapshots
            // this undo needs, so no traversal is required after a rename
            // commits.
            for transfer in transfers {
                if let Err(error) = validate_snapshot_tree(&transfer.expected_state) {
                    return MutationOutcome::unchanged(error);
                }
                if let Err(error) = ensure_unoccupied(&transfer.source) {
                    return MutationOutcome::unchanged(error);
                }
            }
            let mut undone = Vec::with_capacity(transfers.len());
            let mut undoable = true;
            for (attempted, transfer) in transfers.iter().rev().enumerate() {
                // Commit.
                if let Err(error) = rename_no_replace(&transfer.destination, &transfer.source) {
                    let message = format!(
                        "Could not move “{}” back to “{}”: {error}",
                        transfer.destination.display(),
                        transfer.source.display()
                    );
                    if attempted == 0 {
                        // The first rename failed, so nothing moved and the
                        // record still describes the disk exactly.
                        return MutationOutcome::unchanged(anyhow::anyhow!("{message}"));
                    }
                    // Earlier renames committed. Whether or not compensation
                    // restores the paths, those roots have been renamed twice
                    // and their recorded ctimes are stale, so the record can
                    // never validate again.
                    return match rollback_undone_moves(&undone) {
                        Ok(()) => MutationOutcome::discarded(
                            DirectoryChanges::default(),
                            anyhow::anyhow!("{message}; earlier moves were rolled back"),
                        ),
                        Err(rollback_error) => MutationOutcome::discarded(
                            partial_undo_changes(&undone),
                            anyhow::anyhow!("{message}; rollback also failed: {rollback_error}"),
                        ),
                    };
                }
                // Finalize: rebasing the already-validated snapshots cannot
                // fail, and only the renamed root's identity needs re-reading.
                let mut expected_state = transfer.expected_state.clone();
                rebase_snapshots(&mut expected_state, &transfer.destination, &transfer.source);
                undoable &= refresh_snapshot_identities(&mut expected_state);
                undone.push(MoveRecord {
                    source: transfer.source.clone(),
                    destination: transfer.destination.clone(),
                    expected_state,
                });
            }
            undone.reverse();
            // Every source is back, so the destinations are free again and
            // what they displaced can return.
            if let Err(error) = restore_replaced_items(replaced) {
                return MutationOutcome::discarded(reversed, error);
            }
            MutationOutcome::Committed(CommittedOperation::new(
                undone
                    .first()
                    .map(|transfer| transfer.source.clone())
                    .unwrap_or_default(),
                reversed,
                // A replacement is undoable but not redoable: redoing would
                // displace the restored items again, which is a decision the
                // user has not made a second time.
                (undoable && replaced.is_empty()).then_some(OperationRecord::Move {
                    transfers: undone,
                    replaced: Vec::new(),
                }),
            ))
        }
        OperationRecord::Trash { records } => match restore_trash_records(records) {
            Ok(restored) => MutationOutcome::Committed(CommittedOperation::new(
                operation.path().to_path_buf(),
                reversed,
                restored.undoable.then_some(OperationRecord::Trash {
                    records: restored.records,
                }),
            )),
            Err(failure) => trash_failure_outcome(failure),
        },
        OperationRecord::Restore { records } => match retrash_records(records) {
            Ok(records) => MutationOutcome::Committed(CommittedOperation::new(
                operation.path().to_path_buf(),
                reversed,
                Some(OperationRecord::Restore { records }),
            )),
            Err(failure) => trash_failure_outcome(failure),
        },
        OperationRecord::Rename { .. } => match reverse_rename(operation) {
            Ok(committed) => MutationOutcome::Committed(committed),
            // A rename is one atomic commit: it either happened or it did not.
            Err(error) => MutationOutcome::unchanged(error),
        },
    }
}

pub fn redo_operation(operation: &OperationRecord) -> MutationOutcome {
    let forward = operation.forward_directory_changes();
    match operation {
        OperationRecord::CreateDirectory { path, .. } => match create_directory_at(path.clone()) {
            Ok(committed) => MutationOutcome::Committed(committed),
            Err(error) => MutationOutcome::unchanged(error),
        },
        OperationRecord::Copy {
            sources,
            destination,
            ..
        } => {
            if let Err(error) = validate_snapshot_tree(sources) {
                return MutationOutcome::unchanged(error);
            }
            let source_paths = top_level_paths(sources);
            let outcome = transfer_paths(
                &source_paths,
                destination,
                TransferMode::Copy,
                Arc::new(AtomicBool::new(false)),
            );
            if !outcome.failures.is_empty() {
                return rollback_failed_redo(outcome, TransferMode::Copy);
            }
            MutationOutcome::Committed(redone_transfer(
                outcome,
                destination.clone(),
                TransferMode::Copy,
            ))
        }
        OperationRecord::Move { transfers, .. } => {
            for transfer in transfers {
                if let Err(error) = validate_snapshot_tree(&transfer.expected_state) {
                    return MutationOutcome::unchanged(error);
                }
            }
            let source_paths = transfers
                .iter()
                .map(|transfer| transfer.source.clone())
                .collect::<Vec<_>>();
            let Some(destination) = transfers
                .first()
                .and_then(|transfer| transfer.destination.parent())
            else {
                return MutationOutcome::unchanged(anyhow::anyhow!(
                    "Move record has no destination directory"
                ));
            };
            let outcome = transfer_paths(
                &source_paths,
                destination,
                TransferMode::Move,
                Arc::new(AtomicBool::new(false)),
            );
            if !outcome.failures.is_empty() {
                return rollback_failed_redo(outcome, TransferMode::Move);
            }
            MutationOutcome::Committed(redone_transfer(
                outcome,
                destination.to_path_buf(),
                TransferMode::Move,
            ))
        }
        OperationRecord::Trash { records } => match retrash_records(records) {
            Ok(records) => MutationOutcome::Committed(CommittedOperation::new(
                operation.path().to_path_buf(),
                forward,
                Some(OperationRecord::Trash { records }),
            )),
            Err(failure) => trash_failure_outcome(failure),
        },
        OperationRecord::Restore { records } => match restore_trash_records(records) {
            Ok(restored) => MutationOutcome::Committed(CommittedOperation::new(
                operation.path().to_path_buf(),
                forward,
                restored.undoable.then_some(OperationRecord::Restore {
                    records: restored.records,
                }),
            )),
            Err(failure) => trash_failure_outcome(failure),
        },
        OperationRecord::Rename { .. } => match reverse_rename(operation) {
            Ok(committed) => MutationOutcome::Committed(committed),
            Err(error) => MutationOutcome::unchanged(error),
        },
        OperationRecord::ArchiveCreate {
            sources,
            destination,
            ..
        } => {
            if let Err(error) = validate_snapshot_tree(sources) {
                return MutationOutcome::unchanged(error);
            }
            match create_zip_operation(
                &top_level_paths(sources),
                destination,
                Arc::new(AtomicBool::new(false)),
            ) {
                Ok(committed) => MutationOutcome::Committed(committed),
                // The archive is published atomically, so a failure leaves the
                // destination untouched.
                Err(error) => MutationOutcome::unchanged(error),
            }
        }
        OperationRecord::ArchiveExtract { source, .. } => {
            if let Err(error) = validate_snapshot_tree(source) {
                return MutationOutcome::unchanged(error);
            }
            let Some(archive) = top_level_paths(source).into_iter().next() else {
                return MutationOutcome::unchanged(anyhow::anyhow!(
                    "Archive operation has no source"
                ));
            };
            match extract_archive_operation(&archive, Arc::new(AtomicBool::new(false))) {
                Ok(committed) => MutationOutcome::Committed(committed),
                Err(error) => MutationOutcome::unchanged(error),
            }
        }
    }
}

/// Turn a redone transfer into a committed outcome. The transfer itself has
/// already applied its own prepare/commit/finalize discipline, so a missing
/// operation here means undo bookkeeping was lost, not that the redo failed.
///
/// Visible effects come from the exact `CompletedTransfer` records and the
/// transfer mode. Deriving them from the undo record instead dropped every
/// item that committed without one, and the mode-blind fallback marked a
/// copy's *source* removed because only a move retires its sources.
fn redone_transfer(
    outcome: TransferOutcome,
    destination: PathBuf,
    mode: TransferMode,
) -> CommittedOperation {
    let path = outcome
        .completed
        .first()
        .map(|transfer| transfer.destination.clone())
        .unwrap_or(destination);
    let changes = transfer_changes(&outcome, mode);
    CommittedOperation::new(path, changes, outcome.operation)
}

pub fn create_zip_operation(
    sources: &[PathBuf],
    destination: &Path,
    cancelled: Arc<AtomicBool>,
) -> Result<CommittedOperation> {
    // Prepare.
    let source_snapshots = snapshot_paths_cancellable(sources, &cancelled)?;
    // Commit: the archive is published by `create_zip_archive`.
    let outcome = create_zip_archive(sources, destination, cancelled)?;
    // Finalize: a failed snapshot loses undo, it does not unpublish the ZIP.
    let record = snapshot_removable_tree(&outcome.published)
        .ok()
        .map(|created| OperationRecord::ArchiveCreate {
            sources: source_snapshots,
            destination: outcome.published.clone(),
            created,
        });
    Ok(CommittedOperation::new(
        outcome.published.clone(),
        DirectoryChanges {
            upserted: vec![outcome.published],
            ..DirectoryChanges::default()
        },
        record,
    ))
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
    let outcome = extract_archive(archive, cancelled)?;
    // Finalize.
    let record = snapshot_removable_tree(&outcome.published)
        .ok()
        .map(|created| OperationRecord::ArchiveExtract {
            source,
            output: outcome.published.clone(),
            created,
        });
    Ok(CommittedOperation::new(
        outcome.published.clone(),
        DirectoryChanges {
            upserted: vec![outcome.published],
            ..DirectoryChanges::default()
        },
        record,
    ))
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
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("Could not inspect “{}”", path.display()))?;
        let snapshot = snapshot_from_metadata(&path, &metadata)?;
        let kind = snapshot.kind;
        // An archive cannot carry a socket, FIFO, or device node, so refuse the
        // selection before any staging work rather than failing mid-compression.
        reject_special_entries(std::slice::from_ref(&snapshot))?;
        snapshots.push(snapshot);
        if snapshots.len() > MAX_ARCHIVE_ENTRIES {
            bail!("Selection contains more than {MAX_ARCHIVE_ENTRIES} entries");
        }
        if kind == SnapshotKind::Directory {
            let mut children = fs::read_dir(&path)
                .with_context(|| format!("Could not read “{}”", path.display()))?
                .collect::<io::Result<Vec<_>>>()
                .with_context(|| format!("Could not read an entry in “{}”", path.display()))?;
            children.sort_by_key(|entry| entry.file_name());
            pending.extend(children.into_iter().rev().map(|entry| entry.path()));
        }
    }
    Ok(snapshots)
}

fn reverse_rename(operation: &OperationRecord) -> Result<CommittedOperation> {
    let OperationRecord::Rename {
        source,
        destination,
        identity,
    } = operation
    else {
        bail!("Operation is not a rename");
    };
    // Prepare.
    validate_file_identity(destination, identity, "reverse rename")?;
    ensure_unoccupied(source)?;
    // Commit.
    rename_no_replace(destination, source).with_context(|| {
        format!(
            "Could not rename “{}” back to “{}”",
            destination.display(),
            source.display()
        )
    })?;
    // Finalize: the rename happened, so a failed inspection only costs undo.
    let record = fs::symlink_metadata(source)
        .ok()
        .map(|metadata| OperationRecord::Rename {
            source: destination.clone(),
            destination: source.clone(),
            identity: file_identity(&metadata),
        });
    Ok(CommittedOperation::new(
        source.clone(),
        DirectoryChanges {
            removed: vec![destination.clone()],
            upserted: vec![source.clone()],
        },
        record,
    ))
}

fn rollback_failed_redo(mut outcome: TransferOutcome, mode: TransferMode) -> MutationOutcome {
    let failure = summarize_failures(&outcome.failures);
    if outcome.completed.is_empty() {
        // Nothing reached the destination, so the record still describes the
        // disk and the user can retry.
        return MutationOutcome::unchanged(anyhow::anyhow!("{failure}"));
    }

    let Some(partial) = outcome.operation.take() else {
        // Items transferred but produced no undo bookkeeping, so Marcel cannot
        // take them back. Report exactly what landed.
        return MutationOutcome::discarded(
            transfer_changes(&outcome, mode),
            anyhow::anyhow!("{failure}; completed items could not be rolled back"),
        );
    };
    match undo_operation(&partial) {
        MutationOutcome::Committed(_) => MutationOutcome::discarded(
            DirectoryChanges::default(),
            anyhow::anyhow!("{failure}; completed items were rolled back"),
        ),
        MutationOutcome::Unchanged(rollback_error)
        | MutationOutcome::Discarded {
            error: rollback_error,
            ..
        } => MutationOutcome::discarded(
            transfer_changes(&outcome, mode),
            anyhow::anyhow!("{failure}; rollback also failed: {rollback_error}"),
        ),
    }
}

/// The visible effect of a transfer, taken from the exact recorded transfers
/// rather than from an undo record that may cover only a subset.
pub fn transfer_changes(outcome: &TransferOutcome, mode: TransferMode) -> DirectoryChanges {
    DirectoryChanges {
        removed: match mode {
            TransferMode::Move => outcome.completed_sources(),
            TransferMode::Copy => Vec::new(),
        },
        upserted: outcome.completed_destinations(),
    }
}

fn rollback_undone_moves(undone: &[MoveRecord]) -> Result<()> {
    for transfer in undone.iter().rev() {
        rename_no_replace(&transfer.source, &transfer.destination).with_context(|| {
            format!(
                "Could not restore “{}” to “{}”",
                transfer.source.display(),
                transfer.destination.display()
            )
        })?;
    }
    Ok(())
}

pub fn transfer_paths(
    sources: &[PathBuf],
    destination: &Path,
    mode: TransferMode,
    cancelled: Arc<AtomicBool>,
) -> TransferOutcome {
    transfer_paths_impl(
        sources,
        destination,
        mode,
        cancelled,
        None,
        TransferBudget::default(),
        &mut ConflictPolicy::refusing(),
    )
}

pub fn transfer_paths_with_progress(
    sources: &[PathBuf],
    destination: &Path,
    mode: TransferMode,
    cancelled: Arc<AtomicBool>,
    progress: Arc<TransferProgress>,
) -> TransferOutcome {
    transfer_paths_impl(
        sources,
        destination,
        mode,
        cancelled,
        Some(progress),
        TransferBudget::default(),
        &mut ConflictPolicy::refusing(),
    )
}

/// Transfer with a policy that can answer destination conflicts.
///
/// The policy is borrowed for the whole transfer because its apply-to-all state
/// belongs to this operation and must not outlive it.
pub fn transfer_paths_with_conflicts(
    sources: &[PathBuf],
    destination: &Path,
    mode: TransferMode,
    cancelled: Arc<AtomicBool>,
    progress: Arc<TransferProgress>,
    policy: &mut ConflictPolicy,
) -> TransferOutcome {
    transfer_paths_impl(
        sources,
        destination,
        mode,
        cancelled,
        Some(progress),
        TransferBudget::default(),
        policy,
    )
}

/// What a transfer may spend on undo bookkeeping.
///
/// Both limits follow the same rule: past the budget the transfer still
/// happens, it just stops being undoable and says so.
#[derive(Clone, Copy, Debug)]
struct TransferBudget {
    copy_undo_snapshot_limit: usize,
    replacement_undo_byte_limit: u64,
}

impl Default for TransferBudget {
    fn default() -> Self {
        Self {
            copy_undo_snapshot_limit: COPY_UNDO_SNAPSHOT_LIMIT,
            replacement_undo_byte_limit: REPLACEMENT_UNDO_BYTE_LIMIT,
        }
    }
}

/// How many times one source may be renamed before Marcel stops asking.
///
/// A resolver that keeps returning an occupied name would otherwise spin
/// forever. Reaching this is a bug in the resolver, not a user action.
const MAX_CONFLICT_RETRIES: usize = 64;

/// What conflict resolution decided to do with one source.
enum SourcePlan {
    /// Transfer it to this destination, which is free.
    Transfer(PathBuf),
    /// Transfer it to this destination after displacing what is there.
    Replace(PathBuf),
    /// It is already where it was asked to go, so the request is satisfied.
    AlreadyInPlace,
    /// The user declined this source.
    Skip,
    /// The user abandoned the operation.
    Cancel,
    Failed(String),
}

/// Resolve a destination for one source, asking the policy while the chosen
/// name stays occupied.
fn plan_source(
    source: &Path,
    destination_dir: &Path,
    initial_target: PathBuf,
    mode: TransferMode,
    policy: &mut ConflictPolicy,
) -> SourcePlan {
    let source_metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) => {
            return SourcePlan::Failed(format!(
                "Could not inspect “{}”: {error}",
                source.display()
            ));
        }
    };
    let source_is_directory = source_metadata.file_type().is_dir();
    let action = match mode {
        TransferMode::Copy => "copy",
        TransferMode::Move => "move",
    };

    let mut target = initial_target;
    for _ in 0..MAX_CONFLICT_RETRIES {
        let occupant = match describe_occupant(&target) {
            Ok(occupant) => occupant,
            Err(error) => {
                return SourcePlan::Failed(format!(
                    "Could not inspect destination “{}”: {error}",
                    target.display()
                ));
            }
        };
        let Some(occupant) = occupant else {
            return SourcePlan::Transfer(target);
        };
        // The destination *is* the source, which a path comparison alone would
        // miss when a hard link names the same object elsewhere. There is no
        // question to ask here, only an answer to apply.
        if occupant.is_same_object_as(&source_metadata) {
            match mode {
                // Copying something onto itself is a request to duplicate it,
                // and it has exactly one sensible answer, so give it rather
                // than interrupting to ask.
                TransferMode::Copy => {
                    let Some(name) = target.file_name().and_then(|name| {
                        unique_name_in(destination_dir, name, source_is_directory)
                    }) else {
                        return SourcePlan::Failed(format!(
                            "Could not find a free name to duplicate “{}”",
                            source.display()
                        ));
                    };
                    return SourcePlan::Transfer(destination_dir.join(name));
                }
                // Moving something to where it already is changes nothing, so
                // there is nothing to do and nothing worth reporting.
                TransferMode::Move if target == source => return SourcePlan::AlreadyInPlace,
                // A different name for the same object. Replacing would
                // quarantine the very thing about to be renamed.
                TransferMode::Move => {
                    return SourcePlan::Failed(format!(
                        "Cannot {action} “{}” over itself",
                        source.display()
                    ));
                }
            }
        }
        // A skip the user chose and a refusal nobody could answer are different
        // outcomes. Without an interface to ask, this stays the visible failure
        // it has always been, rather than becoming a silent no-op that reports
        // zero items transferred and no error.
        if !policy.is_interactive() {
            return SourcePlan::Failed(format!(
                "“{}” already exists; nothing was overwritten",
                target.display()
            ));
        }

        let request = ConflictRequest {
            source: source.to_path_buf(),
            destination: target.clone(),
            source_is_directory,
            destination_is_directory: occupant.is_directory,
        };
        match policy.decide(&request) {
            ConflictResponse::Skip => return SourcePlan::Skip,
            ConflictResponse::Cancel => return SourcePlan::Cancel,
            ConflictResponse::Rename(name) => {
                if let Err(error) = validate_entry_os_name(&name) {
                    return SourcePlan::Failed(error.to_string());
                }
                target = destination_dir.join(name);
            }
            // Marcel picks the name, so it searches for a free one directly
            // rather than proposing candidates back through the resolver.
            ConflictResponse::AutoRename => {
                let Some(name) = target
                    .file_name()
                    .and_then(|name| unique_name_in(destination_dir, name, source_is_directory))
                else {
                    return SourcePlan::Failed(format!(
                        "Could not find a free name for “{}” in “{}”",
                        source.display(),
                        destination_dir.display()
                    ));
                };
                return SourcePlan::Transfer(destination_dir.join(name));
            }
            // Merging two directory trees is a separate operation with its own
            // per-child decisions, so replace still refuses it rather than
            // discarding a tree the user expected to be merged into.
            ConflictResponse::Replace if request.is_merge() => {
                return SourcePlan::Failed(format!(
                    "“{}” is a folder; merging folders is not supported yet",
                    target.display()
                ));
            }
            ConflictResponse::Replace => return SourcePlan::Replace(target),
        }
    }
    SourcePlan::Failed(format!(
        "Could not find a free name for “{}” after {MAX_CONFLICT_RETRIES} attempts",
        source.display()
    ))
}

fn transfer_paths_impl(
    sources: &[PathBuf],
    destination: &Path,
    mode: TransferMode,
    cancelled: Arc<AtomicBool>,
    progress: Option<Arc<TransferProgress>>,
    budget: TransferBudget,
    policy: &mut ConflictPolicy,
) -> TransferOutcome {
    let TransferBudget {
        copy_undo_snapshot_limit,
        replacement_undo_byte_limit,
    } = budget;
    // Conceptually follows Yazi's per-item scheduled transfer outcomes,
    // cooperative cancellation, partial-success accounting, and rename-first
    // move path. No Yazi code is copied:
    // https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-scheduler/src/worker.rs
    // https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-scheduler/src/file/file.rs
    // https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-scheduler/src/file/traverse.rs
    // https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-fs/src/engine/local/copier.rs
    // https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-fs/src/engine/attrs.rs
    let mut completed = Vec::new();
    let mut failures = Vec::new();
    let mut skipped = Vec::new();
    let mut already_in_place = Vec::new();
    let mut cancelled_sources = Vec::new();
    let mut replaced_items: Vec<ReplacedItem> = Vec::new();
    let mut replaced_bytes: u64 = 0;
    let mut replacement_undo_unavailable = false;
    let mut copied_sources = Vec::new();
    let mut copied_created = Vec::new();
    let mut copy_undo_unavailable = false;
    let mut move_undo_unavailable = false;
    let mut moved = Vec::new();

    if let Some(progress) = &progress {
        progress.set_preparing(true);
        match mode {
            TransferMode::Copy => {
                for source in sources {
                    measure_entry(source, &cancelled, progress);
                    if cancelled.load(Ordering::Acquire) {
                        break;
                    }
                }
            }
            TransferMode::Move => progress.add_total(sources.len() as u64, 0),
        }
        progress.set_preparing(false);
    }

    for (index, source) in sources.iter().enumerate() {
        // Account for every source that will not be attempted, rather than
        // recording one failure and abandoning the rest silently.
        if cancelled.load(Ordering::Acquire) || policy.is_cancelled() {
            cancelled_sources.extend(sources[index..].iter().cloned());
            break;
        }

        let Some(name) = source.file_name() else {
            failures.push(TransferFailure {
                path: source.clone(),
                message: "Source has no file name".to_string(),
            });
            continue;
        };
        let plan = plan_source(source, destination, destination.join(name), mode, policy);
        let (target, displaced) = match plan {
            SourcePlan::Transfer(target) => (target, None),
            // Move what is there aside before publishing over it, so the
            // replacement is never destroyed by a transfer that then fails.
            SourcePlan::Replace(target) => match quarantine_for_replacement(&target) {
                Ok(item) => (target, Some(item)),
                Err(error) => {
                    failures.push(TransferFailure {
                        path: source.clone(),
                        message: error.to_string(),
                    });
                    continue;
                }
            },
            // Not a refusal and not work: the item is where it was asked to
            // be, so the request is already satisfied. Reporting it as skipped
            // or failed would invent a problem the user does not have.
            SourcePlan::AlreadyInPlace => {
                already_in_place.push(source.clone());
                continue;
            }
            SourcePlan::Skip => {
                skipped.push(source.clone());
                continue;
            }
            SourcePlan::Cancel => {
                cancelled_sources.extend(sources[index..].iter().cloned());
                break;
            }
            SourcePlan::Failed(message) => {
                failures.push(TransferFailure {
                    path: source.clone(),
                    message,
                });
                continue;
            }
        };
        if let Some(progress) = &progress {
            progress.set_current_path(Some(source.clone()));
        }
        let result = match mode {
            TransferMode::Copy => {
                let remaining = if copy_undo_unavailable {
                    0
                } else {
                    copy_undo_snapshot_limit
                        .saturating_sub(copied_sources.len() + copied_created.len())
                };
                copy_one(
                    source,
                    &target,
                    &cancelled,
                    progress.as_deref(),
                    remaining / 2,
                )
                .map(|copied| {
                    if copied.overflowed || !copied.undoable {
                        copy_undo_unavailable = true;
                        copied_sources.clear();
                        copied_created.clear();
                    } else if !copy_undo_unavailable {
                        copied_sources.extend(copied.sources);
                        copied_created.extend(copied.created);
                    }
                })
            }
            TransferMode::Move => move_one(source, &target).map(|record| {
                match record {
                    Some(record) => moved.push(record),
                    // The rename committed; only its undo record was lost.
                    None => move_undo_unavailable = true,
                }
                if let Some(progress) = &progress {
                    progress.complete_item();
                }
            }),
        };

        match result {
            Ok(()) => {
                if let Some(item) = displaced {
                    // Holding the displaced object is what makes this undoable.
                    // Past the budget the replacement still stands; it simply
                    // stops being reversible, which the caller reports.
                    let bytes = quarantined_bytes(item.quarantine());
                    if replaced_bytes.saturating_add(bytes) > replacement_undo_byte_limit {
                        erase_replacement_quarantine(item.quarantine());
                        replacement_undo_unavailable = true;
                    } else {
                        replaced_bytes = replaced_bytes.saturating_add(bytes);
                        replaced_items.push(item);
                    }
                }
                completed.push(CompletedTransfer {
                    source: source.clone(),
                    destination: target,
                });
            }
            Err(error) => {
                // The transfer failed, so put back what it displaced rather
                // than leaving the destination empty.
                let mut message = error.to_string();
                if let Some(item) = displaced
                    && let Err(restore_error) = restore_replaced_items(std::slice::from_ref(&item))
                {
                    message.push_str(&format!(
                        "; the replaced item remains at “{}”: {restore_error}",
                        item.quarantine().display()
                    ));
                }
                failures.push(TransferFailure {
                    path: source.clone(),
                    message,
                });
            }
        }
    }

    let operation = match mode {
        TransferMode::Copy if !copied_created.is_empty() => Some(OperationRecord::Copy {
            sources: copied_sources,
            destination: destination.to_path_buf(),
            created: copied_created,
            replaced: std::mem::take(&mut replaced_items),
        }),
        TransferMode::Move if !moved.is_empty() => Some(OperationRecord::Move {
            transfers: moved,
            replaced: std::mem::take(&mut replaced_items),
        }),
        _ => None,
    };
    // Nothing will carry these into the journal, so they can never be restored.
    for item in &replaced_items {
        erase_replacement_quarantine(item.quarantine());
    }

    if let Some(progress) = &progress {
        progress.set_current_path(None);
    }
    TransferOutcome {
        operation,
        completed,
        failures,
        skipped,
        already_in_place,
        cancelled: cancelled_sources,
        undo_unavailable: copy_undo_unavailable
            || move_undo_unavailable
            || replacement_undo_unavailable,
    }
}

fn measure_entry(path: &Path, cancelled: &AtomicBool, progress: &TransferProgress) {
    let mut pending = vec![path.to_path_buf()];
    while let Some(path) = pending.pop() {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        let kind = metadata.file_type();
        progress.add_total(1, if kind.is_file() { metadata.len() } else { 0 });
        if !kind.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&path) else {
            continue;
        };
        pending.extend(entries.flatten().map(|entry| entry.path()));
    }
}

pub fn summarize_failures(failures: &[TransferFailure]) -> String {
    match failures {
        [] => String::new(),
        [failure] => failure.message.clone(),
        failures => format!(
            "{} items failed; first error: {}",
            failures.len(),
            failures[0].message
        ),
    }
}

struct CopiedItem {
    sources: Vec<PathSnapshot>,
    created: Vec<PathSnapshot>,
    overflowed: bool,
    undoable: bool,
}

fn copy_one(
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
    progress: Option<&TransferProgress>,
    snapshot_limit: usize,
) -> Result<CopiedItem> {
    // Prepare.
    ensure_unoccupied(destination)?;
    ensure_not_self_containing(source, destination, "copy")?;
    let name = destination
        .file_name()
        .context("Copy destination has no file name")?;
    let staging = reserve_staging_directory(destination)?;
    let staged = staging.path().join(name);
    let mut context = CopyContext::default();
    let mut source_state = SnapshotCollector::new(snapshot_limit);
    let mut created_state = SnapshotCollector::new(snapshot_limit);
    copy_entry(
        source,
        &staged,
        cancelled,
        progress,
        &mut context,
        &mut source_state,
        &mut created_state,
    )?;
    // Commit. The staging directory is removed when `staging` drops, taking
    // any partially copied tree with it; only the published entry survives.
    rename_no_replace(&staged, destination).with_context(|| {
        format!(
            "Could not publish copy at “{}”; nothing was overwritten",
            destination.display()
        )
    })?;
    // Finalize: the copy is published. Re-reading identities can only cost
    // undo, because publication renames the staged root and bumps its ctime.
    rebase_snapshots(&mut created_state.snapshots, &staged, destination);
    let undoable = refresh_snapshot_identities(&mut created_state.snapshots);
    Ok(CopiedItem {
        sources: source_state.snapshots,
        created: created_state.snapshots,
        overflowed: source_state.overflowed || created_state.overflowed,
        undoable,
    })
}

/// Reject a copy or move whose destination resolves back inside its own
/// source. A lexical prefix test misses a symlinked destination, which would
/// place Marcel's staging directory inside the tree being walked and make the
/// copy enumerate and re-copy its own output until `PATH_MAX` stops it.
fn ensure_not_self_containing(source: &Path, destination: &Path, action: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("Could not inspect “{}”", source.display()))?;
    if !metadata.file_type().is_dir() {
        // Symbolic links are recreated as links rather than traversed, so a
        // link resolving into the destination cannot recurse.
        return Ok(());
    }
    let source_real = source
        .canonicalize()
        .with_context(|| format!("Could not resolve “{}”", source.display()))?;
    let parent = destination
        .parent()
        .context("Destination has no parent directory")?;
    let parent_real = parent
        .canonicalize()
        .with_context(|| format!("Could not resolve “{}”", parent.display()))?;
    let name = destination
        .file_name()
        .context("Destination has no file name")?;
    if parent_real.join(name).starts_with(&source_real) {
        bail!(
            "Cannot {action} “{}” into itself",
            source.file_name().unwrap_or_default().to_string_lossy()
        );
    }
    Ok(())
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

/// Copy one entry using an explicit work stack.
///
/// Recursion here was bounded only by the thread stack: Marcel runs transfers
/// on `blocking` pool threads with Rust's 2 MiB default, so a deep enough tree
/// aborted the whole process with a stack overflow mid-mutation. The archive
/// and delete walkers already used explicit stacks; this matches them.
fn copy_entry(
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
    progress: Option<&TransferProgress>,
    context: &mut CopyContext,
    source_state: &mut SnapshotCollector,
    created_state: &mut SnapshotCollector,
) -> Result<()> {
    let mut steps = vec![CopyStep::Visit {
        source: source.to_path_buf(),
        destination: destination.to_path_buf(),
    }];

    while let Some(step) = steps.pop() {
        match step {
            CopyStep::Visit {
                source,
                destination,
            } => {
                if cancelled.load(Ordering::Acquire) {
                    bail!("Operation cancelled");
                }
                let metadata = fs::symlink_metadata(&source)
                    .with_context(|| format!("Could not inspect “{}”", source.display()))?;
                source_state.push(&source, &metadata)?;
                let kind = metadata.file_type();
                if let Some(progress) = progress {
                    progress.set_current_path(Some(source.clone()));
                }

                if kind.is_dir() {
                    fs::create_dir(&destination)
                        .with_context(|| format!("Could not create “{}”", destination.display()))?;
                    let created_index = created_state.push(
                        &destination,
                        &fs::symlink_metadata(&destination).with_context(|| {
                            format!("Could not inspect “{}”", destination.display())
                        })?,
                    )?;
                    let children = fs::read_dir(&source)
                        .with_context(|| format!("Could not read “{}”", source.display()))?
                        .collect::<io::Result<Vec<_>>>()
                        .with_context(|| {
                            format!("Could not read an entry in “{}”", source.display())
                        })?;
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
                } else if kind.is_file() {
                    copy_regular_file(
                        &source,
                        &destination,
                        &metadata,
                        cancelled,
                        progress,
                        context,
                    )?;
                    preserve_metadata(&source, &destination, &metadata)?;
                    created_state.push(
                        &destination,
                        &fs::symlink_metadata(&destination).with_context(|| {
                            format!("Could not inspect “{}”", destination.display())
                        })?,
                    )?;
                    if let Some(progress) = progress {
                        progress.complete_item();
                    }
                } else if kind.is_symlink() {
                    let target = fs::read_link(&source)
                        .with_context(|| format!("Could not read link “{}”", source.display()))?;
                    std::os::unix::fs::symlink(target, &destination)
                        .with_context(|| format!("Could not copy link “{}”", source.display()))?;
                    preserve_supported_xattrs(&source, &destination)?;
                    created_state.push(
                        &destination,
                        &fs::symlink_metadata(&destination).with_context(|| {
                            format!("Could not inspect “{}”", destination.display())
                        })?,
                    )?;
                    if let Some(progress) = progress {
                        progress.complete_item();
                    }
                } else {
                    bail!(
                        "Special files are not supported yet: “{}”",
                        source.display()
                    );
                }
            }
            CopyStep::FinishDirectory {
                source,
                destination,
                metadata,
                created_index,
            } => {
                preserve_metadata(&source, &destination, &metadata)?;
                created_state.refresh(
                    created_index,
                    &destination,
                    &fs::symlink_metadata(&destination).with_context(|| {
                        format!("Could not inspect “{}”", destination.display())
                    })?,
                )?;
                if let Some(progress) = progress {
                    progress.complete_item();
                }
            }
        }
    }
    Ok(())
}

fn copy_regular_file(
    source: &Path,
    destination: &Path,
    metadata: &fs::Metadata,
    cancelled: &AtomicBool,
    progress: Option<&TransferProgress>,
    context: &mut CopyContext,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let identity = (metadata.dev(), metadata.ino());
    if metadata.nlink() > 1
        && let Some(existing) = context.hardlinks.get(&identity)
    {
        fs::hard_link(existing, destination).with_context(|| {
            format!(
                "Could not preserve hardlink “{}” at “{}”",
                source.display(),
                destination.display()
            )
        })?;
        if let Some(progress) = progress {
            progress.complete_bytes(metadata.len());
        }
        return Ok(());
    }

    copy_file_cancellable(source, destination, cancelled, progress)?;
    if metadata.nlink() > 1 {
        context
            .hardlinks
            .insert(identity, destination.to_path_buf());
    }
    Ok(())
}

fn copy_file_cancellable(
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
    progress: Option<&TransferProgress>,
) -> Result<()> {
    let mut input =
        fs::File::open(source).with_context(|| format!("Could not open “{}”", source.display()))?;
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .with_context(|| format!("Could not create “{}”", destination.display()))?;
    if try_copy_sparse(&mut input, &mut output, source, cancelled, progress)? {
        return output
            .sync_all()
            .with_context(|| format!("Could not finish “{}”", destination.display()));
    }

    input
        .seek(io::SeekFrom::Start(0))
        .with_context(|| format!("Could not rewind “{}”", source.display()))?;
    output
        .set_len(0)
        .and_then(|()| output.seek(io::SeekFrom::Start(0)).map(|_| ()))
        .with_context(|| format!("Could not restart “{}”", destination.display()))?;
    copy_buffered(
        &mut input,
        &mut output,
        source,
        destination,
        cancelled,
        progress,
    )?;
    output
        .sync_all()
        .with_context(|| format!("Could not finish “{}”", destination.display()))
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
        let read = input
            .read(&mut buffer)
            .with_context(|| format!("Could not read “{}”", source.display()))?;
        if read == 0 {
            break;
        }
        output
            .write_all(&buffer[..read])
            .with_context(|| format!("Could not write “{}”", destination.display()))?;
        if let Some(progress) = progress {
            progress.complete_bytes(read as u64);
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
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

    let length = input
        .metadata()
        .with_context(|| format!("Could not inspect “{}”", source.display()))?
        .len();
    if length == 0 {
        return Ok(false);
    }

    let first_data = match seek(&*input, SeekFrom::Data(0)) {
        Ok(offset) => offset,
        Err(Errno::NXIO) => {
            output
                .set_len(length)
                .with_context(|| format!("Could not size sparse file “{}”", source.display()))?;
            if let Some(progress) = progress {
                progress.complete_bytes(length);
            }
            return Ok(true);
        }
        Err(_) => return Ok(false),
    };
    let first_hole = match seek(&*input, SeekFrom::Hole(first_data)) {
        Ok(offset) => offset.min(length),
        Err(_) => return Ok(false),
    };
    if first_data == 0 && first_hole >= length {
        return Ok(false);
    }

    let mut cursor = 0;
    let mut buffer = vec![0_u8; 1024 * 1024];
    while cursor < length {
        if cancelled.load(Ordering::Acquire) {
            bail!("Operation cancelled");
        }
        let data = match seek(&*input, SeekFrom::Data(cursor)) {
            Ok(offset) => offset,
            Err(Errno::NXIO) => break,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Could not inspect sparse extents in “{}”", source.display())
                });
            }
        };
        if data >= length {
            break;
        }
        let hole = seek(&*input, SeekFrom::Hole(data))
            .with_context(|| format!("Could not inspect sparse extents in “{}”", source.display()))?
            .min(length);
        input
            .seek(io::SeekFrom::Start(data))
            .and_then(|_| output.seek(io::SeekFrom::Start(data)))
            .with_context(|| format!("Could not seek sparse file “{}”", source.display()))?;

        let mut remaining = hole.saturating_sub(data);
        while remaining > 0 {
            if cancelled.load(Ordering::Acquire) {
                bail!("Operation cancelled");
            }
            let chunk = usize::try_from(remaining.min(buffer.len() as u64))
                .expect("chunk is bounded by the buffer length");
            input
                .read_exact(&mut buffer[..chunk])
                .with_context(|| format!("Could not read “{}”", source.display()))?;
            output.write_all(&buffer[..chunk]).with_context(|| {
                format!("Could not write sparse extent for “{}”", source.display())
            })?;
            remaining -= chunk as u64;
        }
        cursor = hole;
    }

    output
        .set_len(length)
        .with_context(|| format!("Could not size sparse file “{}”", source.display()))?;
    if let Some(progress) = progress {
        progress.complete_bytes(length);
    }
    Ok(true)
}

#[cfg(not(target_os = "linux"))]
fn try_copy_sparse(
    _input: &mut fs::File,
    _output: &mut fs::File,
    _source: &Path,
    _cancelled: &AtomicBool,
    _progress: Option<&TransferProgress>,
) -> Result<bool> {
    Ok(false)
}

fn preserve_metadata(source: &Path, destination: &Path, metadata: &fs::Metadata) -> Result<()> {
    fs::set_permissions(destination, metadata.permissions()).with_context(|| {
        format!(
            "Could not preserve permissions on “{}”",
            destination.display()
        )
    })?;
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
            .with_context(|| {
                format!(
                    "Could not preserve timestamps on “{}”",
                    destination.display()
                )
            })?;
    }
    Ok(())
}

fn preserve_supported_xattrs(source: &Path, destination: &Path) -> Result<()> {
    let attributes = match xattr::list(source) {
        Ok(attributes) => attributes,
        Err(error) if xattrs_unsupported(&error) => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Could not list attributes on “{}”", source.display()));
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

fn supported_xattr_name(name: &OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt as _;

    let name = name.as_bytes();
    name.starts_with(b"user.")
        || matches!(
            name,
            b"system.posix_acl_access" | b"system.posix_acl_default"
        )
}

fn xattrs_unsupported(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::Unsupported || matches!(error.raw_os_error(), Some(45 | 95))
}

/// Move one entry, returning `Ok(None)` when the rename committed but its undo
/// record could not be captured. A committed move must never be reported as a
/// failure: the caller would leave a vanished source in the browser, keep a
/// dangling cut clipboard, and tell the user nothing happened.
fn move_one(source: &Path, destination: &Path) -> Result<Option<MoveRecord>> {
    ensure_unoccupied(destination)?;
    ensure_not_self_containing(source, destination, "move")?;
    // Prepare: walk the tree before the rename, not after, and treat the walk
    // as bookkeeping rather than a precondition. `snapshot_tree` rejects
    // sockets and FIFOs, but a rename does not care what a directory holds —
    // such a tree is still movable, it just cannot be described for undo.
    // Snapshotting it after the commit instead turned every such move into a
    // deterministic phantom failure.
    let prepared = snapshot_tree(source).ok();
    // Commit.
    rename_no_replace(source, destination)
        .map_err(|error| move_error(&error, source, destination))?;
    // Finalize: a same-filesystem rename preserves every descendant's identity
    // but bumps the renamed root's ctime, so refresh before recording.
    let Some(mut expected_state) = prepared else {
        return Ok(None);
    };
    rebase_snapshots(&mut expected_state, source, destination);
    if !refresh_snapshot_identities(&mut expected_state) {
        return Ok(None);
    }
    Ok(Some(MoveRecord {
        source: source.to_path_buf(),
        destination: destination.to_path_buf(),
        expected_state,
    }))
}

fn move_error(error: &io::Error, source: &Path, destination: &Path) -> anyhow::Error {
    // Only report the parked cross-filesystem limitation when that is actually
    // what happened; attaching it to every rename error hid the real cause.
    let detail = if error.kind() == io::ErrorKind::CrossesDevices {
        "; cross-filesystem moves are not supported yet"
    } else {
        ""
    };
    anyhow::anyhow!(
        "Could not move “{}” to “{}”: {error}{detail}",
        source.display(),
        destination.display()
    )
}

/// Reserve a private staging directory beside the destination.
///
/// This matches `archive_ops`' staging model: the directory is created
/// atomically with a unique name instead of being probed for and created
/// later, so Marcel can never adopt — and then recursively delete — a path
/// another process created in the gap.
fn reserve_staging_directory(destination: &Path) -> Result<tempfile::TempDir> {
    let parent = destination
        .parent()
        .context("Copy destination has no parent directory")?;
    let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    tempfile::Builder::new()
        .prefix(&format!(".marcel-copy-{}-{sequence}-", std::process::id()))
        .tempdir_in(parent)
        .with_context(|| {
            format!(
                "Could not reserve a temporary copy directory in “{}”",
                parent.display()
            )
        })
}

/// Snapshot a tree for a mutation that only ever renames it.
///
/// Sockets, FIFOs, and device nodes are recorded rather than rejected: a
/// rename moves them without inspecting them, so refusing to describe such a
/// tree only costs undo on an operation that succeeded anyway.
/// The name prefix Marcel gives an object it has displaced.
///
/// The process id is part of the name so a later Marcel can tell its own live
/// quarantines from those a dead process abandoned, which is the same rule
/// permanent deletion already uses for its own remnants.
pub fn replacement_quarantine_prefix(process: u32) -> String {
    format!(".marcel-replaced-{process}-")
}

pub fn is_replacement_quarantine_name(name: &OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt as _;

    name.as_bytes().starts_with(b".marcel-replaced-")
}

/// Whether a name belongs to Marcel's own working state rather than the user's
/// data.
///
/// Hidden entries are shown by default, so without this a user who replaces a
/// file watches a cryptic sibling appear beside it and vanish later. Copy and
/// archive staging have the same problem while an operation runs.
///
/// Permanent-delete quarantines are deliberately excluded: their recovery
/// guidance points the user straight at the path, so hiding them would make
/// that advice impossible to follow.
pub fn is_internal_working_name(name: &OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt as _;

    let bytes = name.as_bytes();
    bytes.starts_with(b".marcel-replaced-")
        || bytes.starts_with(b".marcel-copy-")
        || bytes.starts_with(b".marcel-archive-")
}

/// The process that created a replacement quarantine, if the name carries one.
fn quarantine_owner(name: &OsStr) -> Option<u32> {
    use std::os::unix::ffi::OsStrExt as _;

    let rest = name.as_bytes().strip_prefix(b".marcel-replaced-")?;
    let end = rest.iter().position(|byte| *byte == b'-')?;
    std::str::from_utf8(&rest[..end]).ok()?.parse().ok()
}

/// Whether a process is still running, and so might still be able to undo.
///
/// A live owner's quarantine is its own business: two Marcel processes can
/// exist when desktop integration is unavailable, and reclaiming another's
/// would destroy data it can still restore. An unknown answer keeps the file.
#[cfg(target_os = "linux")]
fn process_is_running(process: u32) -> bool {
    Path::new(&format!("/proc/{process}")).exists()
}

#[cfg(not(target_os = "linux"))]
fn process_is_running(_process: u32) -> bool {
    true
}

/// Release replacement quarantines abandoned by processes that are gone.
///
/// Unlike an interrupted permanent deletion, this needs no user involvement.
/// A replaced file is one the user chose to overwrite, and once the process
/// holding its record is gone nothing can ever restore it, so it is provably
/// unreachable rather than possibly-wanted. Returns how many were released.
pub fn reclaim_abandoned_quarantines(directory: &Path) -> usize {
    let current = std::process::id();
    let Ok(entries) = fs::read_dir(directory) else {
        return 0;
    };
    let mut released = 0;
    for entry in entries.flatten() {
        let Some(owner) = quarantine_owner(&entry.file_name()) else {
            continue;
        };
        if owner == current || process_is_running(owner) {
            continue;
        }
        erase_replacement_quarantine(&entry.path());
        released += 1;
    }
    released
}

/// Move the object at `path` aside, returning where it went.
///
/// The rename is atomic, so the destination is never briefly absent in a way
/// that another writer could occupy, and the displaced object is never
/// destroyed before its replacement is safely published.
fn quarantine_for_replacement(path: &Path) -> Result<ReplacedItem> {
    let parent = path
        .parent()
        .context("Replacement target has no parent directory")?;
    let name = path
        .file_name()
        .context("Replacement target has no file name")?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Could not inspect “{}”", path.display()))?;

    for _ in 0..1024 {
        let sequence = REPLACEMENT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut quarantine_name = replacement_quarantine_prefix(std::process::id());
        quarantine_name.push_str(&sequence.to_string());
        quarantine_name.push('-');
        quarantine_name.push_str(&name.to_string_lossy());
        let candidate = parent.join(quarantine_name);
        match fs::symlink_metadata(&candidate) {
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let expected = file_identity(&metadata);
                rename_no_replace(path, &candidate).with_context(|| {
                    format!("Could not move “{}” aside to replace it", path.display())
                })?;
                // Finalize: the rename bumped the moved root's ctime, so the
                // identity has to be re-read rather than carried over — the
                // same discipline the move path already follows. Device and
                // inode survive a rename, so they prove this is still the
                // object that was moved and not something that took its place.
                let moved = fs::symlink_metadata(&candidate).with_context(|| {
                    format!(
                        "Could not inspect “{}” after moving it aside",
                        candidate.display()
                    )
                })?;
                let identity = file_identity(&moved);
                if (identity.device, identity.inode) != (expected.device, expected.inode) {
                    bail!(
                        "“{}” changed while being moved aside to replace it",
                        path.display()
                    );
                }
                return Ok(ReplacedItem {
                    path: path.to_path_buf(),
                    quarantine: candidate,
                    identity,
                });
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Could not inspect replacement quarantine “{}”",
                        candidate.display()
                    )
                });
            }
        }
    }
    bail!("Could not reserve a unique replacement quarantine path")
}

/// Total bytes held by a quarantined object.
fn quarantined_bytes(path: &Path) -> u64 {
    let mut total: u64 = 0;
    let mut pending = vec![path.to_path_buf()];
    while let Some(path) = pending.pop() {
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_file() {
            total = total.saturating_add(metadata.len());
        }
        if metadata.file_type().is_dir()
            && let Ok(entries) = fs::read_dir(&path)
        {
            pending.extend(entries.flatten().map(|entry| entry.path()));
        }
    }
    total
}

/// Release a quarantined object, because nothing can restore it any more.
pub fn erase_replacement_quarantine(path: &Path) {
    // The path was produced by Marcel's own atomic rename under a
    // process-namespaced name, so nothing else can have taken it.
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    let _ = if metadata.file_type().is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
}

/// Put a displaced object back where it came from.
fn restore_replaced_items(replaced: &[ReplacedItem]) -> Result<()> {
    for item in replaced.iter().rev() {
        validate_file_identity(
            &item.quarantine,
            &item.identity,
            "restore the replaced item",
        )?;
        rename_no_replace(&item.quarantine, &item.path).with_context(|| {
            format!(
                "Could not put “{}” back after undoing a replacement",
                item.path.display()
            )
        })?;
    }
    Ok(())
}

fn snapshot_tree(root: &Path) -> Result<Vec<PathSnapshot>> {
    let mut snapshots = Vec::new();
    snapshot_entry(root, &mut snapshots)?;
    Ok(snapshots)
}

/// Snapshot a tree for a mutation whose undo has to *delete* it.
///
/// Marcel cannot recreate a special file, so a tree holding one must not enter
/// a record that Undo would later erase. Callers downgrade the failure to
/// success-without-undo.
fn snapshot_removable_tree(root: &Path) -> Result<Vec<PathSnapshot>> {
    let snapshots = snapshot_tree(root)?;
    reject_special_entries(&snapshots)?;
    Ok(snapshots)
}

fn reject_special_entries(snapshots: &[PathSnapshot]) -> Result<()> {
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
fn snapshot_entry(path: &Path, snapshots: &mut Vec<PathSnapshot>) -> Result<()> {
    let mut pending = vec![path.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("Could not inspect “{}”", path.display()))?;
        let snapshot = snapshot_from_metadata(&path, &metadata)?;
        let kind = snapshot.kind;
        snapshots.push(snapshot);
        if kind == SnapshotKind::Directory {
            let mut children = fs::read_dir(&path)
                .with_context(|| format!("Could not read “{}”", path.display()))?
                .collect::<io::Result<Vec<_>>>()
                .with_context(|| format!("Could not read an entry in “{}”", path.display()))?;
            children.sort_by_key(|entry| entry.file_name());
            pending.extend(children.into_iter().rev().map(|entry| entry.path()));
        }
    }
    Ok(())
}

fn snapshot_from_metadata(path: &Path, metadata: &fs::Metadata) -> Result<PathSnapshot> {
    use std::os::unix::fs::FileTypeExt as _;

    let file_type = metadata.file_type();
    let kind = if file_type.is_dir() {
        SnapshotKind::Directory
    } else if file_type.is_file() {
        SnapshotKind::File
    } else if file_type.is_symlink() {
        SnapshotKind::Symlink
    } else if file_type.is_block_device() {
        SnapshotKind::BlockDevice
    } else if file_type.is_char_device() {
        SnapshotKind::CharDevice
    } else if file_type.is_socket() {
        SnapshotKind::Socket
    } else if file_type.is_fifo() {
        SnapshotKind::Fifo
    } else {
        SnapshotKind::Unknown
    };
    Ok(PathSnapshot {
        path: path.to_path_buf(),
        identity: file_identity(metadata),
        kind,
    })
}

/// Rebase recorded paths from one root to another.
///
/// Infallible by construction: every snapshot in a tree is rooted at `from`,
/// so this runs after a commit without being able to fail it.
fn rebase_snapshots(snapshots: &mut [PathSnapshot], from: &Path, to: &Path) {
    for snapshot in snapshots {
        let Ok(relative) = snapshot.path.strip_prefix(from) else {
            continue;
        };
        snapshot.path = if relative.as_os_str().is_empty() {
            to.to_path_buf()
        } else {
            to.join(relative)
        };
    }
}

/// Re-read recorded identities after a commit.
///
/// Returns `false` when any entry could not be inspected. Callers downgrade
/// that to success-without-undo rather than failing a mutation that already
/// happened.
fn refresh_snapshot_identities(snapshots: &mut [PathSnapshot]) -> bool {
    let mut complete = true;
    for snapshot in snapshots {
        match fs::symlink_metadata(&snapshot.path) {
            Ok(metadata) => snapshot.identity = file_identity(&metadata),
            Err(_) => complete = false,
        }
    }
    complete
}

fn validate_snapshot_tree(snapshots: &[PathSnapshot]) -> Result<()> {
    let expected = snapshots
        .iter()
        .map(|snapshot| (snapshot.path.as_path(), snapshot))
        .collect::<HashMap<_, _>>();
    let mut actual = Vec::with_capacity(snapshots.len());
    for root in top_level_paths(snapshots) {
        snapshot_entry(&root, &mut actual).with_context(|| {
            format!(
                "Cannot continue: “{}” changed or no longer exists",
                root.display()
            )
        })?;
    }
    if actual.len() != snapshots.len() {
        bail!("Cannot continue: the recorded directory contents changed");
    }
    for actual in actual {
        if expected
            .get(actual.path.as_path())
            .is_none_or(|expected| *expected != &actual)
        {
            bail!(
                "Cannot continue: “{}” changed or was replaced",
                actual.path.display()
            );
        }
    }
    Ok(())
}

/// A tree removal that stopped partway.
///
/// `removed` is empty when the failure happened during validation, which lets
/// the caller keep an undo record that still describes the disk.
struct PartialRemoval {
    removed: Vec<PathBuf>,
    error: anyhow::Error,
}

fn remove_snapshotted_tree(snapshots: &[PathSnapshot]) -> Result<(), PartialRemoval> {
    let prepared = validate_snapshot_tree(snapshots)
        // Only copy and archive output reaches here, and neither can contain a
        // special file. Refuse before removing anything rather than discovering
        // it partway through an irreversible walk.
        .and_then(|()| reject_special_entries(snapshots));
    if let Err(error) = prepared {
        return Err(PartialRemoval {
            removed: Vec::new(),
            error,
        });
    }

    let mut removed = Vec::new();
    for snapshot in snapshots.iter().rev() {
        let result = match snapshot.kind {
            SnapshotKind::Directory => fs::remove_dir(&snapshot.path),
            SnapshotKind::File | SnapshotKind::Symlink => fs::remove_file(&snapshot.path),
            kind if kind.is_special() => {
                unreachable!("special entries are rejected above")
            }
            _ => unreachable!("every kind is covered"),
        };
        match result {
            Ok(()) => removed.push(snapshot.path.clone()),
            Err(error) => {
                return Err(PartialRemoval {
                    removed,
                    error: anyhow::Error::new(error)
                        .context(format!("Could not remove “{}”", snapshot.path.display())),
                });
            }
        }
    }
    Ok(())
}

/// Map a Trash failure onto the shared outcome.
///
/// Trash placement and restoration move payloads between the Trash and the
/// user's directories, and Marcel reconciles the Trash view from the operation
/// record rather than from `DirectoryChanges`, so there is no browser effect to
/// report here — only whether the record survives.
fn trash_failure_outcome(failure: crate::trash_ops::TrashMutationFailure) -> MutationOutcome {
    if failure.committed {
        MutationOutcome::discarded(DirectoryChanges::default(), failure.error)
    } else {
        MutationOutcome::unchanged(failure.error)
    }
}

/// The visible effect of an undo whose compensation could not put everything
/// back: the transfers that were reversed are now at their sources.
fn partial_undo_changes(undone: &[MoveRecord]) -> DirectoryChanges {
    DirectoryChanges {
        removed: undone
            .iter()
            .map(|transfer| transfer.destination.clone())
            .collect(),
        upserted: undone
            .iter()
            .map(|transfer| transfer.source.clone())
            .collect(),
    }
}

fn validate_file_identity(path: &Path, expected: &FileIdentity, action: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Cannot {action}: “{}” no longer exists", path.display()))?;
    if file_identity(&metadata) != *expected {
        bail!(
            "Cannot {action}: “{}” changed or was replaced",
            path.display()
        );
    }
    Ok(())
}

fn ensure_unoccupied(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => bail!(
            "“{}” already exists; nothing was overwritten",
            path.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("Could not inspect destination “{}”", path.display())),
    }
}

fn top_level_paths(snapshots: &[PathSnapshot]) -> Vec<PathBuf> {
    let directories = snapshots
        .iter()
        .filter(|snapshot| snapshot.kind == SnapshotKind::Directory)
        .map(|snapshot| snapshot.path.as_path())
        .collect::<std::collections::HashSet<_>>();
    snapshots
        .iter()
        .filter(|candidate| {
            !candidate
                .path
                .ancestors()
                .skip(1)
                .any(|ancestor| directories.contains(ancestor))
        })
        .map(|snapshot| snapshot.path.clone())
        .collect()
}

fn create_directory_at(path: PathBuf) -> Result<CommittedOperation> {
    // Commit.
    fs::create_dir(&path).with_context(|| format!("Could not create “{}”", path.display()))?;
    // Finalize: the directory exists. Refusing to record an unexpected result
    // costs undo, but must not report the creation as failed.
    let record = fs::symlink_metadata(&path)
        .ok()
        .filter(|metadata| metadata.file_type().is_dir())
        .map(|metadata| OperationRecord::CreateDirectory {
            path: path.clone(),
            identity: file_identity(&metadata),
        });
    Ok(CommittedOperation::new(
        path.clone(),
        DirectoryChanges {
            upserted: vec![path],
            ..DirectoryChanges::default()
        },
        record,
    ))
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt as _;

    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    }
}

#[cfg(not(unix))]
compile_error!("Marcel's safe file-operation identity checks currently require Unix metadata");

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    /// Unwrap a committed operation that the test expects to have retained
    /// undo. Failing here means bookkeeping was lost, not that the mutation
    /// failed.
    fn recorded(committed: CommittedOperation) -> OperationRecord {
        committed
            .into_record()
            .expect("operation should have retained an undo record")
    }

    fn destinations(outcome: &TransferOutcome) -> Vec<PathBuf> {
        outcome.completed_destinations()
    }

    /// A lexical prefix test misses a symlinked destination. Marcel then placed
    /// its staging directory inside the tree it was walking and re-copied its
    /// own output once per path component until `PATH_MAX` stopped it, writing
    /// roughly 156x the source size into the user's own directory.
    #[test]
    fn copy_refuses_a_destination_that_resolves_inside_the_source() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("src");
        let sub = source.join("sub");
        let alias = root.path().join("alias");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("payload.bin"), vec![7_u8; 4096]).unwrap();
        std::os::unix::fs::symlink(&sub, &alias).unwrap();
        let before = tree_size(&source);

        let outcome = transfer_paths(
            std::slice::from_ref(&source),
            &alias,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
        );

        assert_eq!(outcome.failures.len(), 1);
        assert!(
            outcome.failures[0].message.contains("into itself"),
            "{:?}",
            outcome.failures
        );
        assert!(outcome.operation.is_none());
        assert_eq!(tree_size(&source), before, "the copy amplified the source");
    }

    #[test]
    fn move_refuses_a_destination_that_resolves_inside_the_source() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("src");
        let sub = source.join("sub");
        let alias = root.path().join("alias");
        fs::create_dir_all(&sub).unwrap();
        std::os::unix::fs::symlink(&sub, &alias).unwrap();

        let outcome = transfer_paths(
            std::slice::from_ref(&source),
            &alias,
            TransferMode::Move,
            Arc::new(AtomicBool::new(false)),
        );

        assert_eq!(outcome.failures.len(), 1);
        assert!(source.is_dir());
    }

    /// Snapshotting after the rename turned any directory containing a socket
    /// into a phantom failure: the move had happened, but the caller was told
    /// it had not.
    #[test]
    fn a_committed_move_is_never_reported_as_a_failure() {
        use std::os::unix::net::UnixListener;

        let root = tempfile::tempdir().unwrap();
        let source_parent = root.path().join("source");
        let destination = root.path().join("destination");
        let project = source_parent.join("project");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(project.join("notes.txt"), b"important").unwrap();
        let _listener = UnixListener::bind(project.join("daemon.sock")).unwrap();

        let outcome = transfer_paths(
            std::slice::from_ref(&project),
            &destination,
            TransferMode::Move,
            Arc::new(AtomicBool::new(false)),
        );

        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert_eq!(destinations(&outcome), [destination.join("project")]);
        assert!(!project.exists());
        assert_eq!(
            fs::read(destination.join("project/notes.txt")).unwrap(),
            b"important"
        );
        // A rename never inspects what the tree holds, so the socket costs
        // nothing: the move succeeds *and* stays undoable.
        assert!(!outcome.undo_unavailable);
        assert!(outcome.operation.is_some());
    }

    /// A compensating rollback renames each root a second time, which bumps its
    /// ctime and invalidates the identities in the record that produced the
    /// attempt. Reinserting that record made every later Undo fail with
    /// "changed or was replaced" — blaming the user for Marcel's own recovery.
    ///
    /// Nautilus discards its undo record whenever an undo fails
    /// (`nautilus-file-undo-manager.c`, `undo_info_apply_ready`). Marcel does
    /// the same past the commit point, and only there.
    #[test]
    fn a_rolled_back_undo_discards_the_record_it_invalidated() {
        use std::os::unix::fs::PermissionsExt as _;

        if rustix::process::geteuid().is_root() {
            // Permission bits do not constrain root, so the mid-loop failure
            // this test depends on cannot be provoked.
            return;
        }

        let root = tempfile::tempdir().unwrap();
        // Two source parents, so one can be sealed without blocking the other.
        // `undo_operation` validates every transfer before renaming any, so an
        // obstacle it can see up front yields `Unchanged`; reaching the
        // rolled-back path needs a failure only the rename itself discovers.
        let blocked = root.path().join("blocked");
        let open = root.path().join("open");
        let destination = root.path().join("destination");
        for directory in [&blocked, &open, &destination] {
            fs::create_dir(directory).unwrap();
        }
        fs::create_dir(blocked.join("first")).unwrap();
        fs::write(blocked.join("first/data.txt"), b"first").unwrap();
        fs::create_dir(open.join("second")).unwrap();
        fs::write(open.join("second/data.txt"), b"second").unwrap();

        let outcome = transfer_paths(
            &[blocked.join("first"), open.join("second")],
            &destination,
            TransferMode::Move,
            Arc::new(AtomicBool::new(false)),
        );
        let operation = outcome.operation.expect("the move retains undo");

        // Undo walks transfers in reverse: "second" returns to `open` first and
        // commits, then "first" cannot be created back inside a read-only
        // parent. Stat still succeeds, so the preflight cannot catch it.
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o555)).unwrap();
        let result = undo_operation(&operation);
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            !result.keeps_history(),
            "a rolled-back undo must discard its record, got {result:?}"
        );
        let MutationOutcome::Discarded { .. } = result else {
            panic!("expected Discarded, got {result:?}");
        };
        // Compensation returned "second" to the destination, so the disk is
        // whole even though the record is gone.
        assert!(destination.join("first/data.txt").exists());
        assert!(destination.join("second/data.txt").exists());
        assert!(!open.join("second").exists());
    }

    /// The mirror case: a failure that never reached the disk keeps the record,
    /// so the user can clear the obstacle and retry. This is where Marcel is
    /// deliberately less blunt than Nautilus, which discards either way.
    #[test]
    fn an_undo_that_never_commits_keeps_its_record() {
        let root = tempfile::tempdir().unwrap();
        let source_dir = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir(&source_dir).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::create_dir(source_dir.join("only")).unwrap();
        fs::write(source_dir.join("only").join("data.txt"), b"payload").unwrap();

        let outcome = transfer_paths(
            std::slice::from_ref(&source_dir.join("only")),
            &destination,
            TransferMode::Move,
            Arc::new(AtomicBool::new(false)),
        );
        let operation = outcome.operation.expect("the move retains undo");
        fs::write(source_dir.join("only"), b"in the way").unwrap();

        let result = undo_operation(&operation);

        assert!(
            result.keeps_history(),
            "a pre-commit refusal must stay retryable, got {result:?}"
        );
        assert!(destination.join("only/data.txt").exists());

        // Clearing the obstacle makes the retained record work.
        fs::remove_file(source_dir.join("only")).unwrap();
        undo_operation(&operation).unwrap();
        assert!(source_dir.join("only/data.txt").exists());
    }

    /// A move is a rename in both directions, so a tree Marcel could never
    /// copy or archive is still fully reversible. Rejecting such trees during
    /// snapshotting was a copy concern leaking into move bookkeeping, and it
    /// silently cost undo on the whole batch.
    #[test]
    fn a_moved_tree_holding_a_socket_is_undoable_and_redoable() {
        use std::os::unix::net::UnixListener;

        let root = tempfile::tempdir().unwrap();
        let source_parent = root.path().join("source");
        let destination = root.path().join("destination");
        let project = source_parent.join("project");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(project.join("notes.txt"), b"important").unwrap();
        let _listener = UnixListener::bind(project.join("daemon.sock")).unwrap();

        let outcome = transfer_paths(
            std::slice::from_ref(&project),
            &destination,
            TransferMode::Move,
            Arc::new(AtomicBool::new(false)),
        );
        let operation = outcome.operation.expect("a moved socket tree retains undo");

        let redo_record = recorded(undo_operation(&operation).unwrap());
        assert!(
            project.join("daemon.sock").exists(),
            "undo restored the tree"
        );
        assert_eq!(fs::read(project.join("notes.txt")).unwrap(), b"important");
        assert!(!destination.join("project").exists());

        recorded(redo_operation(&redo_record).unwrap());
        assert!(destination.join("project/daemon.sock").exists());
        assert!(!project.exists());
    }

    /// Recording special files must not leak into paths whose undo deletes the
    /// tree: Marcel cannot recreate a socket, so an archive holding one stays
    /// success-without-undo rather than gaining an undo that would erase it.
    #[test]
    fn archive_sources_still_refuse_special_files() {
        use std::os::unix::net::UnixListener;

        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("payload");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("notes.txt"), b"keep").unwrap();
        let _listener = UnixListener::bind(source.join("daemon.sock")).unwrap();

        let error = create_zip_operation(
            std::slice::from_ref(&source),
            &root.path().join("payload.zip"),
            Arc::new(AtomicBool::new(false)),
        )
        .expect_err("an archive cannot carry a socket");

        assert!(error.to_string().contains("Special files"), "{error}");
        assert!(!root.path().join("payload.zip").exists());
    }

    #[test]
    fn undo_refuses_to_delete_a_tree_holding_a_special_file() {
        use std::os::unix::net::UnixListener;

        let root = tempfile::tempdir().unwrap();
        let tree = root.path().join("output");
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("kept.txt"), b"keep").unwrap();
        let _listener = UnixListener::bind(tree.join("daemon.sock")).unwrap();

        let snapshots = snapshot_tree(&tree).expect("the rename walker records specials");

        assert!(remove_snapshotted_tree(&snapshots).is_err());
        assert_eq!(fs::read(tree.join("kept.txt")).unwrap(), b"keep");
        assert!(tree.join("daemon.sock").exists());
    }

    /// Marcel runs transfers on `blocking` pool threads with Rust's 2 MiB
    /// default stack. Recursive walkers aborted the whole process with a stack
    /// overflow rather than reporting a failure.
    #[test]
    fn deep_directory_trees_do_not_exhaust_the_worker_stack() {
        let worker = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(|| {
                let root = tempfile::tempdir().unwrap();
                let source = root.path().join("deep");
                let destination = root.path().join("destination");
                let mut current = source.clone();
                fs::create_dir(&current).unwrap();
                for _ in 0..1_500 {
                    current = current.join("d");
                    fs::create_dir(&current).unwrap();
                }
                fs::write(current.join("leaf.txt"), b"leaf").unwrap();
                fs::create_dir(&destination).unwrap();

                let outcome = transfer_paths_with_progress(
                    std::slice::from_ref(&source),
                    &destination,
                    TransferMode::Copy,
                    Arc::new(AtomicBool::new(false)),
                    Arc::new(TransferProgress::default()),
                );
                assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);

                // Permanent deletion walks the same shape.
                crate::delete_ops::delete_paths(
                    std::slice::from_ref(&source),
                    Arc::new(TransferProgress::default()),
                );
            })
            .unwrap();
        worker.join().expect("deep tree work must not abort");
    }

    fn tree_size(root: &Path) -> u64 {
        let mut bytes = 0;
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            let Ok(entries) = fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
                    continue;
                };
                if metadata.is_dir() {
                    pending.push(entry.path());
                } else if metadata.is_file() {
                    bytes += metadata.len();
                }
            }
        }
        bytes
    }

    #[test]
    fn rejects_names_that_escape_the_parent_or_have_no_name() {
        for name in ["", " ", ".", "..", "nested/folder", "bad\0name"] {
            assert!(validate_entry_name(name).is_err(), "{name:?} was accepted");
        }
        assert!(validate_entry_name(".config").is_ok());
        assert!(validate_entry_name("New Folder").is_ok());
    }

    #[test]
    fn create_never_overwrites_an_occupied_destination() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("occupied"), b"keep me").unwrap();

        assert!(create_directory(root.path(), "occupied").is_err());
        assert_eq!(fs::read(root.path().join("occupied")).unwrap(), b"keep me");
    }

    #[test]
    fn rename_is_no_replace_and_supports_undo_redo() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("draft.txt");
        let destination = root.path().join("final.txt");
        fs::write(&source, b"contents").unwrap();

        let operation = recorded(rename_entry(&source, "final.txt").unwrap());
        assert!(!source.exists());
        assert_eq!(fs::read(&destination).unwrap(), b"contents");
        assert_eq!(
            operation.forward_directory_changes(),
            DirectoryChanges {
                removed: vec![source.clone()],
                upserted: vec![destination.clone()],
            }
        );
        assert_eq!(
            operation.reverse_directory_changes(),
            DirectoryChanges {
                removed: vec![destination.clone()],
                upserted: vec![source.clone()],
            }
        );

        let redo_record = recorded(undo_operation(&operation).unwrap());
        assert_eq!(fs::read(&source).unwrap(), b"contents");
        assert!(!destination.exists());

        let redone = recorded(redo_operation(&redo_record).unwrap());
        assert_eq!(redone.path(), destination);
        assert_eq!(fs::read(&destination).unwrap(), b"contents");
    }

    #[cfg(unix)]
    #[test]
    fn rename_accepts_an_invalid_utf8_source_identity() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};

        let root = tempfile::tempdir().unwrap();
        let source = root.path().join(OsString::from_vec(vec![b'n', 0xff]));
        let destination = root.path().join("readable.txt");
        fs::write(&source, b"contents").unwrap();

        let operation = recorded(rename_entry(&source, "readable.txt").unwrap());

        assert!(!source.exists());
        assert_eq!(operation.path(), destination);
        assert_eq!(fs::read(destination).unwrap(), b"contents");
    }

    #[test]
    fn rename_refuses_an_occupied_destination() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.txt");
        let destination = root.path().join("occupied.txt");
        fs::write(&source, b"source").unwrap();
        fs::write(&destination, b"keep").unwrap();

        assert!(rename_entry(&source, "occupied.txt").is_err());
        assert_eq!(fs::read(&source).unwrap(), b"source");
        assert_eq!(fs::read(&destination).unwrap(), b"keep");
    }

    #[test]
    fn rename_undo_refuses_a_modified_result() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("draft.txt");
        let destination = root.path().join("final.txt");
        fs::write(&source, b"original").unwrap();
        let operation = recorded(rename_entry(&source, "final.txt").unwrap());
        fs::write(&destination, b"modified").unwrap();

        assert!(undo_operation(&operation).is_err());
        assert!(!source.exists());
        assert_eq!(fs::read(&destination).unwrap(), b"modified");
    }

    #[test]
    fn create_undo_and_redo_validate_the_path() {
        let root = tempfile::tempdir().unwrap();
        let created = recorded(create_directory(root.path(), "photos").unwrap());

        undo_operation(&created).unwrap();
        assert!(!created.path().exists());

        let recreated = recorded(redo_operation(&created).unwrap());
        assert!(recreated.path().is_dir());
    }

    #[test]
    fn undo_refuses_a_non_empty_created_directory() {
        let root = tempfile::tempdir().unwrap();
        let created = recorded(create_directory(root.path(), "work").unwrap());
        fs::write(created.path().join("important.txt"), b"data").unwrap();

        assert!(undo_operation(&created).is_err());
        assert_eq!(
            fs::read(created.path().join("important.txt")).unwrap(),
            b"data"
        );
    }

    #[test]
    fn undo_refuses_a_replacement_at_the_same_path() {
        let root = tempfile::tempdir().unwrap();
        let created = recorded(create_directory(root.path(), "replace-me").unwrap());
        fs::remove_dir(created.path()).unwrap();
        fs::create_dir(created.path()).unwrap();

        assert!(undo_operation(&created).is_err());
        assert!(created.path().is_dir());
    }

    #[test]
    fn history_is_bounded_and_new_work_clears_redo() {
        let root = tempfile::tempdir().unwrap();
        let mut journal = OperationJournal::new(2);
        let first = recorded(create_directory(root.path(), "first").unwrap());
        let second = recorded(create_directory(root.path(), "second").unwrap());
        let third = recorded(create_directory(root.path(), "third").unwrap());
        let _ = journal.record(first);
        let _ = journal.record(second.clone());
        let _ = journal.record(third.clone());

        assert_eq!(journal.begin_undo(), Some(third.clone()));
        assert_eq!(journal.begin_undo(), Some(second.clone()));
        assert_eq!(journal.begin_undo(), None);

        journal.finish_undo(second.clone());
        assert!(journal.can_redo());
        journal.cancel_undo(third);
        let _ = journal.record(second);
        assert!(!journal.can_redo());
    }

    #[test]
    fn recursive_copy_preserves_sources_and_supports_undo_redo() {
        let root = tempfile::tempdir().unwrap();
        let source_parent = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir(&source_parent).unwrap();
        fs::create_dir(&destination).unwrap();
        let album = source_parent.join("album");
        fs::create_dir(&album).unwrap();
        fs::write(album.join("notes.txt"), b"hello").unwrap();
        std::os::unix::fs::symlink("notes.txt", album.join("notes-link")).unwrap();

        let outcome = transfer_paths(
            std::slice::from_ref(&album),
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert_eq!(
            fs::read(destination.join("album/notes.txt")).unwrap(),
            b"hello"
        );
        assert_eq!(
            fs::read_link(destination.join("album/notes-link")).unwrap(),
            PathBuf::from("notes.txt")
        );
        assert_eq!(fs::read(album.join("notes.txt")).unwrap(), b"hello");

        let operation = outcome.operation.unwrap();
        assert_eq!(
            operation.forward_directory_changes(),
            DirectoryChanges {
                removed: Vec::new(),
                upserted: vec![destination.join("album")],
            }
        );
        let redo_record = recorded(undo_operation(&operation).unwrap());
        assert!(!destination.join("album").exists());
        let redone = recorded(redo_operation(&redo_record).unwrap());
        assert_eq!(fs::read(redone.path().join("notes.txt")).unwrap(), b"hello");
    }

    #[test]
    fn copy_never_overwrites_an_occupied_destination() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.txt");
        let destination = root.path().join("destination");
        fs::write(&source, b"new").unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("source.txt"), b"keep").unwrap();

        let outcome = transfer_paths(
            &[source],
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(outcome.failures.len(), 1);
        assert!(outcome.operation.is_none());
        assert_eq!(fs::read(destination.join("source.txt")).unwrap(), b"keep");
    }

    /// A resolver that answers every conflict the same way.
    struct AlwaysAnswers(crate::conflict::ConflictDecision);

    impl crate::conflict::ConflictResolver for AlwaysAnswers {
        fn resolve(&self, _request: &ConflictRequest) -> crate::conflict::ConflictDecision {
            self.0.clone()
        }
    }

    fn answering(decision: crate::conflict::ConflictDecision) -> ConflictPolicy {
        ConflictPolicy::interactive(Arc::new(AlwaysAnswers(decision)))
    }

    fn occupied_transfer_fixture() -> (tempfile::TempDir, Vec<PathBuf>, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let source_dir = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir(&source_dir).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(source_dir.join("taken.txt"), b"new").unwrap();
        fs::write(source_dir.join("free.txt"), b"free").unwrap();
        fs::write(destination.join("taken.txt"), b"keep").unwrap();
        let sources = vec![source_dir.join("taken.txt"), source_dir.join("free.txt")];
        (root, sources, destination)
    }

    /// Skipping is a deliberate outcome, not a failure, and it must not stop
    /// the sources that follow it.
    #[test]
    fn a_skipped_conflict_leaves_both_items_and_continues() {
        let (_root, sources, destination) = occupied_transfer_fixture();
        let mut policy = answering(crate::conflict::ConflictDecision::once(
            ConflictResponse::Skip,
        ));

        let outcome = transfer_paths_with_conflicts(
            &sources,
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
            Arc::new(TransferProgress::default()),
            &mut policy,
        );

        assert_eq!(outcome.skipped, [sources[0].clone()]);
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert_eq!(outcome.completed.len(), 1);
        assert_eq!(fs::read(destination.join("taken.txt")).unwrap(), b"keep");
        assert_eq!(fs::read(destination.join("free.txt")).unwrap(), b"free");
        assert_eq!(outcome.accounted(), sources.len());
    }

    /// Cancelling from a conflict abandons the operation, and every source it
    /// never reached is accounted for rather than silently forgotten.
    #[test]
    fn cancelling_a_conflict_accounts_for_every_unattempted_source() {
        let (_root, sources, destination) = occupied_transfer_fixture();
        let mut policy = answering(crate::conflict::ConflictDecision::once(
            ConflictResponse::Cancel,
        ));

        let outcome = transfer_paths_with_conflicts(
            &sources,
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
            Arc::new(TransferProgress::default()),
            &mut policy,
        );

        assert_eq!(outcome.cancelled, sources);
        assert!(outcome.completed.is_empty());
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert!(!destination.join("free.txt").exists());
        assert_eq!(outcome.accounted(), sources.len());
    }

    #[test]
    fn renaming_resolves_a_conflict_without_touching_the_occupant() {
        let (_root, sources, destination) = occupied_transfer_fixture();
        let mut policy = answering(crate::conflict::ConflictDecision::once(
            ConflictResponse::Rename(OsStr::new("renamed.txt").to_os_string()),
        ));

        let outcome = transfer_paths_with_conflicts(
            &sources,
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
            Arc::new(TransferProgress::default()),
            &mut policy,
        );

        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert_eq!(fs::read(destination.join("taken.txt")).unwrap(), b"keep");
        assert_eq!(fs::read(destination.join("renamed.txt")).unwrap(), b"new");
        assert_eq!(outcome.accounted(), sources.len());
    }

    /// A chosen name gets the same scrutiny as one typed into Rename, so a
    /// resolver cannot smuggle a path separator past the destination directory.
    #[test]
    fn a_rename_response_cannot_escape_the_destination_directory() {
        let (_root, sources, destination) = occupied_transfer_fixture();
        let mut policy = answering(crate::conflict::ConflictDecision::once(
            ConflictResponse::Rename(OsStr::new("../escaped.txt").to_os_string()),
        ));

        let outcome = transfer_paths_with_conflicts(
            &sources,
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
            Arc::new(TransferProgress::default()),
            &mut policy,
        );

        assert_eq!(outcome.failures.len(), 1);
        assert!(
            outcome.failures[0].message.contains("cannot contain"),
            "{:?}",
            outcome.failures
        );
        assert!(!destination.parent().unwrap().join("escaped.txt").exists());
        assert_eq!(outcome.accounted(), sources.len());
    }

    /// A crash cannot run the exit path, so the remnants it leaves have to be
    /// reclaimed later. A dead owner's quarantine can never be restored, which
    /// makes it unreachable garbage rather than data anyone might want.
    #[test]
    fn quarantines_from_dead_processes_are_reclaimed_and_live_ones_are_left() {
        let root = tempfile::tempdir().unwrap();
        // Process id 0 is never a real process, so it stands in for a Marcel
        // that is gone.
        let abandoned = root.path().join(".marcel-replaced-0-0-report.txt");
        let live = root.path().join(format!(
            ".marcel-replaced-{}-0-report.txt",
            std::process::id()
        ));
        let ordinary = root.path().join("report.txt");
        for path in [&abandoned, &live, &ordinary] {
            fs::write(path, b"payload").unwrap();
        }

        let released = reclaim_abandoned_quarantines(root.path());

        assert_eq!(released, 1);
        assert!(!abandoned.exists(), "a dead owner's quarantine is garbage");
        assert!(
            live.exists(),
            "this process can still undo, so its quarantine stays"
        );
        assert!(ordinary.exists(), "user data is never touched");
    }

    /// A live second Marcel can still restore its own replacements, so its
    /// quarantines must survive another instance listing the same directory.
    #[test]
    fn a_running_owners_quarantine_is_never_reclaimed() {
        let root = tempfile::tempdir().unwrap();
        // The test process itself is a live owner that is not this process id
        // only in the sense that the check must consult liveness, not equality.
        let parent = std::os::unix::process::parent_id();
        let live = root
            .path()
            .join(format!(".marcel-replaced-{parent}-0-report.txt"));
        fs::write(&live, b"payload").unwrap();

        assert_eq!(reclaim_abandoned_quarantines(root.path()), 0);
        assert!(live.exists());
    }

    #[test]
    fn marcel_working_names_are_recognized_without_catching_user_data() {
        for name in [
            ".marcel-replaced-1-0-report.txt",
            ".marcel-copy-1-0-abc",
            ".marcel-archive-abc",
        ] {
            assert!(is_internal_working_name(OsStr::new(name)), "{name}");
        }
        for name in [
            // Recovery guidance points the user straight at these.
            ".marcel-delete-1-0-report.txt",
            "report.txt",
            ".hidden",
            "marcel-replaced-1-0",
        ] {
            assert!(!is_internal_working_name(OsStr::new(name)), "{name}");
        }
    }

    /// A record pushed out of the journal can never be undone, so what it was
    /// holding aside stops being recoverable data and becomes a hidden file
    /// nobody would ever collect.
    #[test]
    fn evicting_a_record_releases_the_data_it_was_holding() {
        let root = tempfile::tempdir().unwrap();
        let source_dir = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir(&source_dir).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(source_dir.join("report.txt"), b"replacement").unwrap();
        fs::write(destination.join("report.txt"), b"the original").unwrap();
        let mut policy = answering(crate::conflict::ConflictDecision::once(
            ConflictResponse::Replace,
        ));
        let outcome = transfer_paths_with_conflicts(
            std::slice::from_ref(&source_dir.join("report.txt")),
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
            Arc::new(TransferProgress::default()),
            &mut policy,
        );
        let replacing = outcome.operation.expect("a replacement retains undo");
        assert!(!no_replacement_quarantines(&destination));

        let mut journal = OperationJournal::new(1);
        assert!(journal.record(replacing).is_empty());
        // A second record displaces the first, which can now never be undone.
        let evicted = journal.record(recorded(create_directory(root.path(), "later").unwrap()));

        assert_eq!(evicted.len(), 1);
        for record in &evicted {
            record.release_quarantines();
        }
        assert!(
            no_replacement_quarantines(&destination),
            "an unreachable record must not keep holding disk"
        );
        // The replacement itself is untouched; only its way back is gone.
        assert_eq!(
            fs::read(destination.join("report.txt")).unwrap(),
            b"replacement"
        );
    }

    /// The whole promise of replacement: what it displaced comes back. Nautilus
    /// overwrites in place, so its undo cannot do this at all.
    #[test]
    fn undoing_a_replacement_puts_the_displaced_file_back() {
        let root = tempfile::tempdir().unwrap();
        let source_dir = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir(&source_dir).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(source_dir.join("report.txt"), b"replacement").unwrap();
        fs::write(destination.join("report.txt"), b"the original").unwrap();
        let mut policy = answering(crate::conflict::ConflictDecision::once(
            ConflictResponse::Replace,
        ));

        let outcome = transfer_paths_with_conflicts(
            std::slice::from_ref(&source_dir.join("report.txt")),
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
            Arc::new(TransferProgress::default()),
            &mut policy,
        );

        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert!(!outcome.undo_unavailable);
        assert_eq!(
            fs::read(destination.join("report.txt")).unwrap(),
            b"replacement"
        );
        let operation = outcome.operation.expect("a replacement retains undo");

        undo_operation(&operation).unwrap();

        assert_eq!(
            fs::read(destination.join("report.txt")).unwrap(),
            b"the original",
            "undo must restore what the replacement displaced"
        );
        assert!(no_replacement_quarantines(&destination));
    }

    /// A transfer that fails after displacing must put the displaced item back
    /// rather than leaving the destination empty.
    #[test]
    fn a_failed_replacement_restores_what_it_displaced() {
        let root = tempfile::tempdir().unwrap();
        let source_dir = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir(&source_dir).unwrap();
        fs::create_dir(&destination).unwrap();
        // A socket cannot be copied, so the transfer fails after the
        // destination has already been moved aside.
        let source = source_dir.join("payload");
        fs::create_dir(&source).unwrap();
        let _listener = std::os::unix::net::UnixListener::bind(source.join("daemon.sock")).unwrap();
        fs::write(destination.join("payload"), b"the original").unwrap();
        let mut policy = answering(crate::conflict::ConflictDecision::once(
            ConflictResponse::Replace,
        ));

        let outcome = transfer_paths_with_conflicts(
            std::slice::from_ref(&source),
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
            Arc::new(TransferProgress::default()),
            &mut policy,
        );

        assert_eq!(outcome.failures.len(), 1, "{outcome:?}");
        assert_eq!(
            fs::read(destination.join("payload")).unwrap(),
            b"the original",
            "a failed replacement must not consume the original"
        );
        assert!(no_replacement_quarantines(&destination));
    }

    /// Past the byte budget the replacement still happens; it simply stops
    /// being reversible, and the quarantine is released rather than held.
    #[test]
    fn an_oversized_replacement_succeeds_without_undo() {
        let root = tempfile::tempdir().unwrap();
        let source_dir = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir(&source_dir).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(source_dir.join("blob.bin"), b"replacement").unwrap();
        fs::write(destination.join("blob.bin"), vec![0_u8; 4096]).unwrap();
        let mut policy = answering(crate::conflict::ConflictDecision::once(
            ConflictResponse::Replace,
        ));

        let outcome = transfer_paths_impl(
            std::slice::from_ref(&source_dir.join("blob.bin")),
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
            None,
            TransferBudget {
                replacement_undo_byte_limit: 1024,
                ..TransferBudget::default()
            },
            &mut policy,
        );

        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert!(
            outcome.undo_unavailable,
            "an oversized replacement cannot be undone"
        );
        assert_eq!(
            fs::read(destination.join("blob.bin")).unwrap(),
            b"replacement"
        );
        assert!(
            no_replacement_quarantines(&destination),
            "an unreachable quarantine must be released, not left on disk"
        );
    }

    /// Merging two directory trees needs per-child decisions, so replace must
    /// not quietly discard a tree the user expected to be merged into.
    #[test]
    fn replacing_a_directory_with_a_directory_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let source_dir = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir_all(source_dir.join("shared")).unwrap();
        fs::create_dir_all(destination.join("shared")).unwrap();
        fs::write(destination.join("shared/keep.txt"), b"keep").unwrap();
        let mut policy = answering(crate::conflict::ConflictDecision::for_all(
            ConflictResponse::Replace,
        ));

        let outcome = transfer_paths_with_conflicts(
            std::slice::from_ref(&source_dir.join("shared")),
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
            Arc::new(TransferProgress::default()),
            &mut policy,
        );

        assert_eq!(outcome.failures.len(), 1, "{outcome:?}");
        assert_eq!(
            fs::read(destination.join("shared/keep.txt")).unwrap(),
            b"keep"
        );
    }

    fn no_replacement_quarantines(directory: &Path) -> bool {
        fs::read_dir(directory)
            .unwrap()
            .flatten()
            .all(|entry| !is_replacement_quarantine_name(&entry.file_name()))
    }

    /// Renaming everything keeps every source, each beside the item it
    /// collided with, without a single item being lost or overwritten.
    #[test]
    fn renaming_all_keeps_every_colliding_source() {
        let root = tempfile::tempdir().unwrap();
        let source_dir = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir(&source_dir).unwrap();
        fs::create_dir(&destination).unwrap();
        for name in ["a.txt", "b.txt"] {
            fs::write(source_dir.join(name), b"new").unwrap();
            fs::write(destination.join(name), b"existing").unwrap();
        }
        // Already occupied, so "a.txt" has to land past it.
        fs::write(destination.join("a (2).txt"), b"existing too").unwrap();
        let sources = vec![source_dir.join("a.txt"), source_dir.join("b.txt")];
        let mut policy = answering(crate::conflict::ConflictDecision::for_all(
            ConflictResponse::AutoRename,
        ));

        let outcome = transfer_paths_with_conflicts(
            &sources,
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
            Arc::new(TransferProgress::default()),
            &mut policy,
        );

        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert_eq!(outcome.completed.len(), 2);
        assert_eq!(outcome.accounted(), sources.len());
        // Nothing that was already there changed.
        assert_eq!(fs::read(destination.join("a.txt")).unwrap(), b"existing");
        assert_eq!(fs::read(destination.join("b.txt")).unwrap(), b"existing");
        assert_eq!(
            fs::read(destination.join("a (2).txt")).unwrap(),
            b"existing too"
        );
        // Both sources arrived beside them.
        assert_eq!(fs::read(destination.join("a (3).txt")).unwrap(), b"new");
        assert_eq!(fs::read(destination.join("b (2).txt")).unwrap(), b"new");
    }

    /// Dropping a selection onto the folder it already lives in asks for
    /// nothing. Refusing it would invent a problem the user does not have, and
    /// reporting it as skipped would claim they declined something.
    #[test]
    fn moving_an_item_where_it_already_is_does_nothing() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("folder");
        fs::create_dir(&folder).unwrap();
        let source = folder.join("report.txt");
        fs::write(&source, b"payload").unwrap();
        let mut policy = answering(crate::conflict::ConflictDecision::for_all(
            ConflictResponse::Replace,
        ));

        let outcome = transfer_paths_with_conflicts(
            std::slice::from_ref(&source),
            &folder,
            TransferMode::Move,
            Arc::new(AtomicBool::new(false)),
            Arc::new(TransferProgress::default()),
            &mut policy,
        );

        assert_eq!(outcome.already_in_place, std::slice::from_ref(&source));
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert!(outcome.skipped.is_empty());
        assert!(outcome.completed.is_empty());
        assert!(outcome.operation.is_none());
        assert_eq!(fs::read(&source).unwrap(), b"payload");
        assert_eq!(outcome.accounted(), 1);
    }

    /// Copying a file into the folder it lives in is a request to duplicate it,
    /// and it has one sensible answer, so it is answered rather than asked.
    #[test]
    fn copying_an_item_into_its_own_folder_duplicates_it_without_asking() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("folder");
        fs::create_dir(&folder).unwrap();
        let source = folder.join("report.txt");
        fs::write(&source, b"payload").unwrap();
        // A resolver that would panic if consulted: this must not ask.
        let mut policy = ConflictPolicy::interactive(Arc::new(AlwaysAnswers(
            crate::conflict::ConflictDecision::once(ConflictResponse::Cancel),
        )));

        let outcome = transfer_paths_with_conflicts(
            std::slice::from_ref(&source),
            &folder,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
            Arc::new(TransferProgress::default()),
            &mut policy,
        );

        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert_eq!(outcome.completed.len(), 1);
        assert_eq!(fs::read(&source).unwrap(), b"payload");
        assert_eq!(
            fs::read(folder.join("report (2).txt")).unwrap(),
            b"payload",
            "the duplicate lands beside the original"
        );
        // Duplicating again steps past the name it just created.
        let outcome = transfer_paths_with_conflicts(
            std::slice::from_ref(&source),
            &folder,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
            Arc::new(TransferProgress::default()),
            &mut policy,
        );
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert!(folder.join("report (3).txt").exists());
    }

    /// A hard link names the same object under a different path, so comparing
    /// paths would miss it. Replacing there would quarantine the very object
    /// about to be renamed, so a move refuses; a copy is safe because it never
    /// destroys the source, and duplicating is what was asked for.
    #[test]
    fn a_hardlink_to_the_source_is_recognized_as_the_same_object() {
        let root = tempfile::tempdir().unwrap();
        let source_dir = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir(&source_dir).unwrap();
        fs::create_dir(&destination).unwrap();
        let source = source_dir.join("report.pdf");
        fs::write(&source, b"original").unwrap();
        // Same inode, different directory, same basename: the transfer would
        // land exactly on its own source.
        fs::hard_link(&source, destination.join("report.pdf")).unwrap();
        let mut policy = answering(crate::conflict::ConflictDecision::for_all(
            ConflictResponse::Replace,
        ));

        let moved = transfer_paths_with_conflicts(
            std::slice::from_ref(&source),
            &destination,
            TransferMode::Move,
            Arc::new(AtomicBool::new(false)),
            Arc::new(TransferProgress::default()),
            &mut policy,
        );

        assert_eq!(moved.failures.len(), 1, "{moved:?}");
        assert!(
            moved.failures[0].message.contains("over itself"),
            "{:?}",
            moved.failures
        );
        assert_eq!(fs::read(&source).unwrap(), b"original");

        let copied = transfer_paths_with_conflicts(
            std::slice::from_ref(&source),
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
            Arc::new(TransferProgress::default()),
            &mut policy,
        );

        assert!(copied.failures.is_empty(), "{:?}", copied.failures);
        assert_eq!(fs::read(&source).unwrap(), b"original");
        // The existing link is untouched and the duplicate lands beside it.
        assert_eq!(
            fs::read(destination.join("report.pdf")).unwrap(),
            b"original"
        );
        assert_eq!(
            fs::read(destination.join("report (2).pdf")).unwrap(),
            b"original"
        );
    }

    /// Cancelling the operation through the cancel flag must also account for
    /// the sources it never reached.
    #[test]
    fn a_cancelled_transfer_accounts_for_every_requested_source() {
        let (_root, sources, destination) = occupied_transfer_fixture();

        let outcome = transfer_paths(
            &sources,
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(true)),
        );

        assert_eq!(outcome.cancelled, sources);
        assert_eq!(outcome.accounted(), sources.len());
    }

    #[test]
    fn copy_undo_refuses_a_modified_output() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.txt");
        let destination = root.path().join("destination");
        fs::write(&source, b"original").unwrap();
        fs::create_dir(&destination).unwrap();
        let outcome = transfer_paths(
            &[source],
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
        );
        let operation = outcome.operation.unwrap();
        fs::write(destination.join("source.txt"), b"changed").unwrap();

        assert!(undo_operation(&operation).is_err());
        assert_eq!(
            fs::read(destination.join("source.txt")).unwrap(),
            b"changed"
        );
    }

    #[test]
    fn copy_undo_refuses_added_children_without_partially_removing_output() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("original.txt"), b"original").unwrap();
        fs::create_dir(&destination).unwrap();
        let outcome = transfer_paths(
            &[source],
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
        );
        let operation = outcome.operation.unwrap();
        let copied = destination.join("source");
        fs::write(copied.join("added-later.txt"), b"keep").unwrap();

        assert!(undo_operation(&operation).is_err());
        assert_eq!(fs::read(copied.join("original.txt")).unwrap(), b"original");
        assert_eq!(fs::read(copied.join("added-later.txt")).unwrap(), b"keep");
    }

    #[test]
    fn copy_redo_refuses_new_source_children_without_publishing_output() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("original.txt"), b"original").unwrap();
        fs::create_dir(&destination).unwrap();
        let outcome = transfer_paths(
            std::slice::from_ref(&source),
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
        );
        let redo_record = recorded(undo_operation(&outcome.operation.unwrap()).unwrap());
        fs::write(source.join("added-later.txt"), b"new").unwrap();

        assert!(redo_operation(&redo_record).is_err());
        assert!(!destination.join("source").exists());
    }

    #[test]
    fn move_supports_identity_checked_undo_and_redo() {
        let root = tempfile::tempdir().unwrap();
        let source_parent = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir(&source_parent).unwrap();
        fs::create_dir(&destination).unwrap();
        let source = source_parent.join("move-me.txt");
        fs::write(&source, b"contents").unwrap();

        let outcome = transfer_paths(
            std::slice::from_ref(&source),
            &destination,
            TransferMode::Move,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        let operation = outcome.operation.unwrap();
        assert!(!source.exists());
        assert_eq!(
            operation.forward_directory_changes(),
            DirectoryChanges {
                removed: vec![source.clone()],
                upserted: vec![destination.join("move-me.txt")],
            }
        );

        let redo_record = recorded(undo_operation(&operation).unwrap());
        assert_eq!(fs::read(&source).unwrap(), b"contents");
        let redone = recorded(redo_operation(&redo_record).unwrap());
        assert_eq!(fs::read(redone.path()).unwrap(), b"contents");
        assert!(!source.exists());
    }

    #[test]
    fn move_refuses_to_put_a_directory_inside_itself() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let descendant = source.join("descendant");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&descendant).unwrap();

        let outcome = transfer_paths(
            std::slice::from_ref(&source),
            &descendant,
            TransferMode::Move,
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(outcome.failures.len(), 1);
        assert!(source.is_dir());
    }

    #[test]
    fn cancelled_transfer_does_not_publish_an_output() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.txt");
        let destination = root.path().join("destination");
        fs::write(&source, b"contents").unwrap();
        fs::create_dir(&destination).unwrap();
        let cancelled = Arc::new(AtomicBool::new(true));

        let outcome = transfer_paths(&[source], &destination, TransferMode::Copy, cancelled);
        assert!(outcome.operation.is_none());
        assert!(!destination.join("source.txt").exists());
    }

    #[test]
    fn copy_reports_item_and_byte_progress() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.txt");
        let destination = root.path().join("destination");
        fs::write(&source, b"marcel").unwrap();
        fs::create_dir(&destination).unwrap();
        let progress = Arc::new(TransferProgress::default());

        let outcome = transfer_paths_with_progress(
            &[source],
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
            progress.clone(),
        );

        assert!(outcome.failures.is_empty());
        assert_eq!(
            progress.snapshot(),
            TransferProgressSnapshot {
                preparing: false,
                total_items: 1,
                completed_items: 1,
                total_bytes: 6,
                completed_bytes: 6,
                current_path: None,
            }
        );
    }

    #[test]
    fn copy_preserves_file_and_directory_modes_and_times() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let source_parent = root.path().join("source");
        let destination = root.path().join("destination");
        let tree = source_parent.join("tree");
        let file = tree.join("script.sh");
        fs::create_dir_all(&tree).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(&file, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&tree, fs::Permissions::from_mode(0o750)).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();

        let accessed = UNIX_EPOCH + Duration::from_secs(1_650_000_000);
        let modified = UNIX_EPOCH + Duration::from_secs(1_650_000_123);
        let times = fs::FileTimes::new()
            .set_accessed(accessed)
            .set_modified(modified);
        fs::File::open(&file).unwrap().set_times(times).unwrap();
        fs::File::open(&tree).unwrap().set_times(times).unwrap();

        let outcome = transfer_paths(
            std::slice::from_ref(&tree),
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);

        let copied_tree = destination.join("tree");
        let copied_file = copied_tree.join("script.sh");
        let tree_metadata = fs::metadata(copied_tree).unwrap();
        let file_metadata = fs::metadata(copied_file).unwrap();
        assert_eq!(tree_metadata.permissions().mode() & 0o7777, 0o750);
        assert_eq!(file_metadata.permissions().mode() & 0o7777, 0o640);
        assert_eq!(tree_metadata.modified().unwrap(), modified);
        assert_eq!(file_metadata.modified().unwrap(), modified);
        assert_eq!(file_metadata.accessed().unwrap(), accessed);
    }

    #[test]
    fn copy_preserves_supported_user_xattrs() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.txt");
        let destination = root.path().join("destination");
        fs::write(&source, b"contents").unwrap();
        fs::create_dir(&destination).unwrap();
        if let Err(error) = xattr::set(&source, "user.marcel-copy-test", b"kept") {
            assert!(xattrs_unsupported(&error));
            return;
        }

        let outcome = transfer_paths(
            &[source],
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert_eq!(
            xattr::get(destination.join("source.txt"), "user.marcel-copy-test")
                .unwrap()
                .as_deref(),
            Some(b"kept".as_slice())
        );
    }

    #[test]
    fn copy_preserves_posix_access_acl_xattr_when_supported() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.txt");
        let destination = root.path().join("destination");
        fs::write(&source, b"contents").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();
        fs::create_dir(&destination).unwrap();

        let mut acl = 2_u32.to_le_bytes().to_vec();
        for (tag, permissions, id) in [
            (0x01_u16, 0x06_u16, u32::MAX),
            (0x02, 0x04, fs::metadata(&source).unwrap().uid()),
            (0x04, 0x04, u32::MAX),
            (0x10, 0x04, u32::MAX),
            (0x20, 0x00, u32::MAX),
        ] {
            acl.extend(tag.to_le_bytes());
            acl.extend(permissions.to_le_bytes());
            acl.extend(id.to_le_bytes());
        }
        if let Err(error) = xattr::set(&source, "system.posix_acl_access", &acl) {
            if xattrs_unsupported(&error)
                || matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::InvalidInput
                )
            {
                return;
            }
            panic!("could not create ACL fixture: {error}");
        }
        let expected = xattr::get(&source, "system.posix_acl_access")
            .unwrap()
            .expect("ACL fixture disappeared");

        let outcome = transfer_paths(
            &[source],
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert_eq!(
            xattr::get(destination.join("source.txt"), "system.posix_acl_access").unwrap(),
            Some(expected)
        );
    }

    #[test]
    fn copy_preserves_hardlinks_within_a_directory_tree() {
        use std::os::unix::fs::MetadataExt as _;

        let root = tempfile::tempdir().unwrap();
        let source_parent = root.path().join("source");
        let destination = root.path().join("destination");
        let tree = source_parent.join("tree");
        fs::create_dir_all(&tree).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(tree.join("first"), b"shared").unwrap();
        fs::hard_link(tree.join("first"), tree.join("second")).unwrap();

        let outcome = transfer_paths(
            std::slice::from_ref(&tree),
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);

        let first = fs::metadata(destination.join("tree/first")).unwrap();
        let second = fs::metadata(destination.join("tree/second")).unwrap();
        assert_eq!(first.dev(), second.dev());
        assert_eq!(first.ino(), second.ino());
        assert_eq!(first.nlink(), 2);
        recorded(undo_operation(&outcome.operation.unwrap()).unwrap());
        assert!(!destination.join("tree").exists());
    }

    #[test]
    fn copy_preserves_sparse_layout_when_extents_are_available() {
        use std::os::unix::fs::MetadataExt as _;

        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("sparse.bin");
        let destination = root.path().join("destination");
        fs::create_dir(&destination).unwrap();
        let mut file = fs::File::create(&source).unwrap();
        file.set_len(16 * 1024 * 1024).unwrap();
        file.write_all(b"start").unwrap();
        file.seek(io::SeekFrom::End(-4)).unwrap();
        file.write_all(b"end!").unwrap();
        file.sync_all().unwrap();
        let source_metadata = file.metadata().unwrap();
        if source_metadata.blocks() * 512 >= source_metadata.len() {
            return;
        }

        let outcome = transfer_paths(
            &[source],
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);

        let copied = destination.join("sparse.bin");
        let copied_metadata = fs::metadata(&copied).unwrap();
        assert_eq!(copied_metadata.len(), source_metadata.len());
        assert!(copied_metadata.blocks() * 512 < copied_metadata.len());
        let mut copied_file = fs::File::open(copied).unwrap();
        let mut start = [0; 5];
        copied_file.read_exact(&mut start).unwrap();
        copied_file.seek(io::SeekFrom::End(-4)).unwrap();
        let mut end = [0; 4];
        copied_file.read_exact(&mut end).unwrap();
        assert_eq!(&start, b"start");
        assert_eq!(&end, b"end!");
    }

    #[test]
    fn copy_rejects_special_files_without_publishing_them() {
        use std::os::unix::net::UnixListener;

        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("socket");
        let destination = root.path().join("destination");
        fs::create_dir(&destination).unwrap();
        let _listener = UnixListener::bind(&source).unwrap();

        let outcome = transfer_paths(
            &[source],
            &destination,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(outcome.failures.len(), 1);
        assert!(outcome.operation.is_none());
        assert!(!destination.join("socket").exists());
    }

    #[test]
    fn xattr_policy_includes_user_and_posix_acl_namespaces_only() {
        assert!(supported_xattr_name(OsStr::new("user.comment")));
        assert!(supported_xattr_name(OsStr::new("system.posix_acl_access")));
        assert!(supported_xattr_name(OsStr::new("system.posix_acl_default")));
        assert!(!supported_xattr_name(OsStr::new("security.selinux")));
        assert!(!supported_xattr_name(OsStr::new("trusted.overlay")));
    }

    #[test]
    fn overflowing_snapshot_budget_keeps_the_copy_but_omits_excess_records() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("one"), b"1").unwrap();
        fs::write(source.join("two"), b"2").unwrap();

        let destination_parent = root.path().join("destination");
        fs::create_dir(&destination_parent).unwrap();
        let outcome = transfer_paths_impl(
            &[source],
            &destination_parent,
            TransferMode::Copy,
            Arc::new(AtomicBool::new(false)),
            None,
            TransferBudget {
                copy_undo_snapshot_limit: 2,
                ..TransferBudget::default()
            },
            &mut ConflictPolicy::refusing(),
        );

        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert!(outcome.undo_unavailable);
        assert!(outcome.operation.is_none());
        let destination = destination_parent.join("source");
        assert_eq!(fs::read(destination.join("one")).unwrap(), b"1");
        assert_eq!(fs::read(destination.join("two")).unwrap(), b"2");
    }

    #[test]
    fn archive_create_and_extract_support_identity_validated_undo_redo() {
        if crate::archive_ops::SevenZipBackend::discover().is_err() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("report.txt");
        let archive = root.path().join("report.zip");
        fs::write(&source, b"archive history").unwrap();

        let created = recorded(
            create_zip_operation(
                std::slice::from_ref(&source),
                &archive,
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap(),
        );
        assert!(archive.is_file());
        let undone = recorded(undo_operation(&created).unwrap());
        assert!(!archive.exists());
        let recreated = recorded(redo_operation(&undone).unwrap());
        assert!(archive.is_file());

        fs::remove_file(&source).unwrap();
        let extracted = recorded(
            extract_archive_operation(&archive, Arc::new(AtomicBool::new(false))).unwrap(),
        );
        assert_eq!(fs::read(&source).unwrap(), b"archive history");
        let undone = recorded(undo_operation(&extracted).unwrap());
        assert!(!source.exists());
        let redone = recorded(redo_operation(&undone).unwrap());
        assert_eq!(fs::read(redone.path()).unwrap(), b"archive history");

        fs::write(redone.path(), b"changed").unwrap();
        assert!(undo_operation(&redone).is_err());
        assert_eq!(fs::read(redone.path()).unwrap(), b"changed");

        // Keep the compiler and test honest that the recreated record remains
        // a normal archive operation rather than a special test-only path.
        assert!(matches!(recreated, OperationRecord::ArchiveCreate { .. }));
    }
}
