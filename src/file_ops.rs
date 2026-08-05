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
    local_fs::rename_no_replace,
    trash_ops::{TrashRecord, restore_trash_records, retrash_records},
};

pub const OPERATION_HISTORY_LIMIT: usize = 100;
pub const COPY_UNDO_SNAPSHOT_LIMIT: usize = 100_000;
static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
    },
    Move {
        transfers: Vec<MoveRecord>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SnapshotKind {
    Directory,
    File,
    Symlink,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveRecord {
    source: PathBuf,
    destination: PathBuf,
    expected_state: Vec<PathSnapshot>,
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

#[derive(Debug)]
pub struct TransferOutcome {
    pub operation: Option<OperationRecord>,
    pub completed: Vec<CompletedTransfer>,
    pub failures: Vec<TransferFailure>,
    pub undo_unavailable: bool,
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
            Self::Move { transfers } => transfers
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
            Self::Move { transfers } => DirectoryChanges {
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

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn record(&mut self, operation: OperationRecord) {
        self.redo.clear();
        push_bounded(&mut self.undo, operation, self.limit);
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

fn push_bounded(stack: &mut VecDeque<OperationRecord>, operation: OperationRecord, limit: usize) {
    if limit == 0 {
        return;
    }
    while stack.len() >= limit {
        stack.pop_front();
    }
    stack.push_back(operation);
}

pub fn validate_entry_name(name: &str) -> Result<()> {
    if name.is_empty() || name.trim().is_empty() {
        bail!("Enter a name");
    }
    if name == "." || name == ".." {
        bail!("“{name}” is reserved and cannot be used as a name");
    }
    if name.contains('/') || name.contains('\0') {
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

pub fn undo_operation(operation: &OperationRecord) -> Result<CommittedOperation> {
    let reversed = operation.reverse_directory_changes();
    match operation {
        OperationRecord::CreateDirectory { path, identity } => {
            let metadata = fs::symlink_metadata(path)
                .with_context(|| format!("Cannot undo: “{}” no longer exists", path.display()))?;
            if !metadata.file_type().is_dir() {
                bail!("Cannot undo: “{}” is no longer a directory", path.display());
            }
            if fs::read_dir(path)
                .with_context(|| format!("Cannot inspect “{}”", path.display()))?
                .next()
                .is_some()
            {
                bail!("Cannot undo: “{}” is no longer empty", path.display());
            }
            if file_identity(&metadata) != *identity {
                bail!("Cannot undo: “{}” changed or was replaced", path.display());
            }
            fs::remove_dir(path)
                .with_context(|| format!("Could not remove “{}”", path.display()))?;
            Ok(CommittedOperation::new(
                path.clone(),
                reversed,
                Some(operation.clone()),
            ))
        }
        OperationRecord::Copy { created, .. } => {
            remove_snapshotted_tree(created)?;
            Ok(CommittedOperation::new(
                operation.path().to_path_buf(),
                reversed,
                Some(operation.clone()),
            ))
        }
        OperationRecord::Move { transfers } => {
            // Prepare: validating every transfer also produces the snapshots
            // this undo needs, so no traversal is required after a rename
            // commits.
            for transfer in transfers {
                validate_snapshot_tree(&transfer.expected_state)?;
                ensure_unoccupied(&transfer.source)?;
            }
            let mut undone = Vec::with_capacity(transfers.len());
            let mut undoable = true;
            for transfer in transfers.iter().rev() {
                // Commit.
                if let Err(error) = rename_no_replace(&transfer.destination, &transfer.source) {
                    let rollback_error = rollback_undone_moves(&undone).err();
                    let message = format!(
                        "Could not move “{}” back to “{}”: {error}",
                        transfer.destination.display(),
                        transfer.source.display()
                    );
                    return match rollback_error {
                        Some(rollback_error) => Err(anyhow::anyhow!(
                            "{message}; rollback also failed: {rollback_error}"
                        )),
                        None => Err(anyhow::anyhow!("{message}; earlier moves were rolled back")),
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
            Ok(CommittedOperation::new(
                undone
                    .first()
                    .map(|transfer| transfer.source.clone())
                    .unwrap_or_default(),
                reversed,
                undoable.then_some(OperationRecord::Move { transfers: undone }),
            ))
        }
        OperationRecord::Trash { records } => {
            let restored = restore_trash_records(records)?;
            Ok(CommittedOperation::new(
                operation.path().to_path_buf(),
                reversed,
                restored.undoable.then_some(OperationRecord::Trash {
                    records: restored.records,
                }),
            ))
        }
        OperationRecord::Restore { records } => Ok(CommittedOperation::new(
            operation.path().to_path_buf(),
            reversed,
            Some(OperationRecord::Restore {
                records: retrash_records(records)?,
            }),
        )),
        OperationRecord::Rename { .. } => reverse_rename(operation),
        OperationRecord::ArchiveCreate { created, .. }
        | OperationRecord::ArchiveExtract { created, .. } => {
            remove_snapshotted_tree(created)?;
            Ok(CommittedOperation::new(
                operation.path().to_path_buf(),
                reversed,
                Some(operation.clone()),
            ))
        }
    }
}

pub fn redo_operation(operation: &OperationRecord) -> Result<CommittedOperation> {
    let forward = operation.forward_directory_changes();
    match operation {
        OperationRecord::CreateDirectory { path, .. } => create_directory_at(path.clone()),
        OperationRecord::Copy {
            sources,
            destination,
            ..
        } => {
            validate_snapshot_tree(sources)?;
            let source_paths = top_level_paths(sources);
            let outcome = transfer_paths(
                &source_paths,
                destination,
                TransferMode::Copy,
                Arc::new(AtomicBool::new(false)),
            );
            if !outcome.failures.is_empty() {
                return rollback_failed_redo(outcome);
            }
            Ok(redone_transfer(outcome, destination.clone()))
        }
        OperationRecord::Move { transfers } => {
            for transfer in transfers {
                validate_snapshot_tree(&transfer.expected_state)?;
            }
            let source_paths = transfers
                .iter()
                .map(|transfer| transfer.source.clone())
                .collect::<Vec<_>>();
            let destination = transfers
                .first()
                .and_then(|transfer| transfer.destination.parent())
                .context("Move record has no destination directory")?;
            let outcome = transfer_paths(
                &source_paths,
                destination,
                TransferMode::Move,
                Arc::new(AtomicBool::new(false)),
            );
            if !outcome.failures.is_empty() {
                return rollback_failed_redo(outcome);
            }
            Ok(redone_transfer(outcome, destination.to_path_buf()))
        }
        OperationRecord::Trash { records } => Ok(CommittedOperation::new(
            operation.path().to_path_buf(),
            forward,
            Some(OperationRecord::Trash {
                records: retrash_records(records)?,
            }),
        )),
        OperationRecord::Restore { records } => {
            let restored = restore_trash_records(records)?;
            Ok(CommittedOperation::new(
                operation.path().to_path_buf(),
                forward,
                restored.undoable.then_some(OperationRecord::Restore {
                    records: restored.records,
                }),
            ))
        }
        OperationRecord::Rename { .. } => reverse_rename(operation),
        OperationRecord::ArchiveCreate {
            sources,
            destination,
            ..
        } => {
            validate_snapshot_tree(sources)?;
            create_zip_operation(
                &top_level_paths(sources),
                destination,
                Arc::new(AtomicBool::new(false)),
            )
        }
        OperationRecord::ArchiveExtract { source, .. } => {
            validate_snapshot_tree(source)?;
            let archive = top_level_paths(source)
                .into_iter()
                .next()
                .context("Archive operation has no source")?;
            extract_archive_operation(&archive, Arc::new(AtomicBool::new(false)))
        }
    }
}

/// Turn a redone transfer into a committed outcome. The transfer itself has
/// already applied its own prepare/commit/finalize discipline, so a missing
/// operation here means undo bookkeeping was lost, not that the redo failed.
fn redone_transfer(outcome: TransferOutcome, destination: PathBuf) -> CommittedOperation {
    let path = outcome
        .completed
        .first()
        .map(|transfer| transfer.destination.clone())
        .unwrap_or(destination.clone());
    let changes = outcome.operation.as_ref().map_or_else(
        || DirectoryChanges {
            removed: outcome.completed_sources(),
            upserted: outcome.completed_destinations(),
        },
        OperationRecord::forward_directory_changes,
    );
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
    let record =
        snapshot_tree(&outcome.published)
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
    let source = snapshot_tree(archive)?;
    // Commit.
    let outcome = extract_archive(archive, cancelled)?;
    // Finalize.
    let record =
        snapshot_tree(&outcome.published)
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

fn rollback_failed_redo(outcome: TransferOutcome) -> Result<CommittedOperation> {
    let failure = summarize_failures(&outcome.failures);
    if let Some(partial) = outcome.operation {
        match undo_operation(&partial) {
            Ok(_) => bail!("{failure}; completed items were rolled back"),
            Err(rollback_error) => {
                bail!("{failure}; rollback also failed: {rollback_error}");
            }
        }
    }
    bail!("{failure}");
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
        COPY_UNDO_SNAPSHOT_LIMIT,
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
        COPY_UNDO_SNAPSHOT_LIMIT,
    )
}

fn transfer_paths_impl(
    sources: &[PathBuf],
    destination: &Path,
    mode: TransferMode,
    cancelled: Arc<AtomicBool>,
    progress: Option<Arc<TransferProgress>>,
    copy_undo_snapshot_limit: usize,
) -> TransferOutcome {
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

    for source in sources {
        if cancelled.load(Ordering::Acquire) {
            failures.push(TransferFailure {
                path: source.clone(),
                message: "Operation cancelled".to_string(),
            });
            break;
        }

        let Some(name) = source.file_name() else {
            failures.push(TransferFailure {
                path: source.clone(),
                message: "Source has no file name".to_string(),
            });
            continue;
        };
        let target = destination.join(name);
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
            Ok(()) => completed.push(CompletedTransfer {
                source: source.clone(),
                destination: target,
            }),
            Err(error) => failures.push(TransferFailure {
                path: source.clone(),
                message: error.to_string(),
            }),
        }
    }

    let operation = match mode {
        TransferMode::Copy if !copied_created.is_empty() => Some(OperationRecord::Copy {
            sources: copied_sources,
            destination: destination.to_path_buf(),
            created: copied_created,
        }),
        TransferMode::Move if !moved.is_empty() => Some(OperationRecord::Move { transfers: moved }),
        _ => None,
    };

    if let Some(progress) = &progress {
        progress.set_current_path(None);
    }
    TransferOutcome {
        operation,
        completed,
        failures,
        undo_unavailable: copy_undo_unavailable || move_undo_unavailable,
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

fn snapshot_tree(root: &Path) -> Result<Vec<PathSnapshot>> {
    let mut snapshots = Vec::new();
    snapshot_entry(root, &mut snapshots)?;
    Ok(snapshots)
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
    let kind = if metadata.file_type().is_dir() {
        SnapshotKind::Directory
    } else if metadata.file_type().is_file() {
        SnapshotKind::File
    } else if metadata.file_type().is_symlink() {
        SnapshotKind::Symlink
    } else {
        bail!("Special files are not supported yet: “{}”", path.display());
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

fn remove_snapshotted_tree(snapshots: &[PathSnapshot]) -> Result<()> {
    validate_snapshot_tree(snapshots)?;
    for snapshot in snapshots.iter().rev() {
        match snapshot.kind {
            SnapshotKind::Directory => fs::remove_dir(&snapshot.path),
            SnapshotKind::File | SnapshotKind::Symlink => fs::remove_file(&snapshot.path),
        }
        .with_context(|| format!("Could not remove “{}”", snapshot.path.display()))?;
    }
    Ok(())
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

    /// `snapshot_tree` rejects sockets and FIFOs, so snapshotting after the
    /// rename turned any directory containing one into a phantom failure: the
    /// move had happened, but the caller was told it had not.
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
        // The tree holds a socket, so it cannot be snapshotted for undo. The
        // move still succeeded and must be reported that way.
        assert!(outcome.undo_unavailable);
        assert!(outcome.operation.is_none());
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
        journal.record(first);
        journal.record(second.clone());
        journal.record(third.clone());

        assert_eq!(journal.begin_undo(), Some(third.clone()));
        assert_eq!(journal.begin_undo(), Some(second.clone()));
        assert_eq!(journal.begin_undo(), None);

        journal.finish_undo(second.clone());
        assert!(journal.can_redo());
        journal.cancel_undo(third);
        journal.record(second);
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
            2,
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
