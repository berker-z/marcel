//! Copying or moving a set of sources into a destination directory, accounting
//! for every one of them exactly once.
//!
//! Conceptually follows Yazi's per-item scheduled transfer outcomes,
//! cooperative cancellation, partial-success accounting, and rename-first move
//! path. No Yazi code is copied:
//! https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-scheduler/src/worker.rs
//! https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-scheduler/src/file/file.rs

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::Result;

use super::{
    DirectoryChanges, PathFailure, TransferProgress,
    conflict::{
        ConflictPolicy, ConflictRequest, ConflictResponse, describe_occupant, unique_name_in,
    },
    copy::{MergeStop, copy_one, ensure_not_self_containing, merge_directories},
    journal::{
        MoveRecord, OperationRecord, PathSnapshot, UNDO_SNAPSHOT_LIMIT, rebase_snapshots,
        refresh_snapshot_identities, snapshot_tree_within,
    },
    local::{ensure_unoccupied, rename_no_replace},
    mutations::validate_entry_os_name,
    quarantine::{
        REPLACEMENT_UNDO_BYTE_LIMIT, ReplacedItem, erase_replacement_quarantine,
        preserve_unrestored, quarantine_for_replacement, restore_replaced_items,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferMode {
    Copy,
    Move,
}

impl TransferMode {
    pub fn verb(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Move => "move",
        }
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

/// The result of a transfer, accounting for every requested source exactly
/// once across `completed`, `skipped`, `failed`, `already_in_place`, and
/// `cancelled`.
///
/// Cancellation previously recorded one failure and abandoned the loop, so a
/// hundred-item transfer stopped at item ten reported ten results and left
/// eighty-nine sources with no state at all. Nothing downstream could tell
/// "skipped by the user" from "silently forgotten".
#[derive(Debug)]
pub struct TransferOutcome {
    pub operation: Option<OperationRecord>,
    pub completed: Vec<CompletedTransfer>,
    pub failures: Vec<PathFailure>,
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

    pub fn completed_destinations(&self) -> Vec<PathBuf> {
        self.completed
            .iter()
            .map(|t| t.destination.clone())
            .collect()
    }

    /// The visible effect of the transfer, taken from the exact recorded
    /// transfers rather than from an undo record that may cover only a subset.
    pub fn changes(&self, mode: TransferMode) -> DirectoryChanges {
        DirectoryChanges {
            removed: match mode {
                TransferMode::Move => self.completed.iter().map(|t| t.source.clone()).collect(),
                TransferMode::Copy => Vec::new(),
            },
            upserted: self.completed_destinations(),
        }
    }

    pub fn summarize_failures(&self) -> String {
        match self.failures.as_slice() {
            [] => String::new(),
            [failure] => failure.message.clone(),
            failures => format!(
                "{} items failed; first error: {}",
                failures.len(),
                failures[0].message
            ),
        }
    }
}

pub fn transfer_paths(
    sources: &[PathBuf],
    destination: &Path,
    mode: TransferMode,
    cancelled: Arc<AtomicBool>,
) -> TransferOutcome {
    Transfer::new(sources, destination, mode, cancelled).run(&mut ConflictPolicy::refusing())
}

pub fn transfer_paths_with_progress(
    sources: &[PathBuf],
    destination: &Path,
    mode: TransferMode,
    cancelled: Arc<AtomicBool>,
    progress: Arc<TransferProgress>,
) -> TransferOutcome {
    Transfer::new(sources, destination, mode, cancelled)
        .with_progress(progress)
        .run(&mut ConflictPolicy::refusing())
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
    Transfer::new(sources, destination, mode, cancelled)
        .with_progress(progress)
        .run(policy)
}

/// What a transfer may spend on undo bookkeeping.
///
/// Both limits follow the same rule: past the budget the transfer still
/// happens, it just stops being undoable and says so.
#[derive(Clone, Copy, Debug)]
pub(super) struct TransferBudget {
    pub(super) undo_snapshot_limit: usize,
    pub(super) replacement_undo_byte_limit: u64,
}

impl Default for TransferBudget {
    fn default() -> Self {
        Self {
            undo_snapshot_limit: UNDO_SNAPSHOT_LIMIT,
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
    /// Fold it into the directory already at this destination.
    Merge(PathBuf),
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
    let free_name = |target: &Path| {
        target
            .file_name()
            .and_then(|name| unique_name_in(destination_dir, name, source_is_directory))
            .map(|name| destination_dir.join(name))
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
            return match mode {
                // Copying something onto itself is a request to duplicate it,
                // and it has exactly one sensible answer, so give it rather
                // than interrupting to ask.
                TransferMode::Copy => match free_name(&target) {
                    Some(target) => SourcePlan::Transfer(target),
                    None => SourcePlan::Failed(format!(
                        "Could not find a free name to duplicate “{}”",
                        source.display()
                    )),
                },
                // Moving something to where it already is changes nothing, so
                // there is nothing to do and nothing worth reporting.
                TransferMode::Move if target == source => SourcePlan::AlreadyInPlace,
                // A different name for the same object. Replacing would
                // quarantine the very thing about to be renamed.
                TransferMode::Move => {
                    SourcePlan::Failed(format!("Cannot move “{}” over itself", source.display()))
                }
            };
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
                return match free_name(&target) {
                    Some(target) => SourcePlan::Transfer(target),
                    None => SourcePlan::Failed(format!(
                        "Could not find a free name for “{}” in “{}”",
                        source.display(),
                        destination_dir.display()
                    )),
                };
            }
            // Two directories meeting is a merge, not a replacement: the
            // destination keeps everything it has and gains what it lacks.
            // Moving cannot express that yet, so it still refuses rather than
            // discarding a tree the user expected to be joined.
            ConflictResponse::Replace if request.is_merge() => {
                return match mode {
                    TransferMode::Copy => SourcePlan::Merge(target),
                    TransferMode::Move => SourcePlan::Failed(format!(
                        "“{}” is a folder; merging folders is only supported when copying",
                        target.display()
                    )),
                };
            }
            ConflictResponse::Replace => return SourcePlan::Replace(target),
        }
    }
    SourcePlan::Failed(format!(
        "Could not find a free name for “{}” after {MAX_CONFLICT_RETRIES} attempts",
        source.display()
    ))
}

/// Undo bookkeeping for one kind of work in a transfer, up to a shared budget.
///
/// Past the budget the work still happens; the ledger simply empties itself
/// and stops accepting entries, and the outcome reports undo as unavailable.
struct Ledger<T> {
    entries: Vec<T>,
    unavailable: bool,
}

impl<T> Ledger<T> {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            unavailable: false,
        }
    }

    fn give_up(&mut self) {
        self.unavailable = true;
        self.entries.clear();
    }

    fn extend(&mut self, entries: impl IntoIterator<Item = T>) {
        if !self.unavailable {
            self.entries.extend(entries);
        }
    }
}

/// One transfer, in progress.
pub(super) struct Transfer<'a> {
    sources: &'a [PathBuf],
    destination: &'a Path,
    mode: TransferMode,
    cancelled: Arc<AtomicBool>,
    progress: Option<Arc<TransferProgress>>,
    budget: TransferBudget,
}

impl<'a> Transfer<'a> {
    pub(super) fn new(
        sources: &'a [PathBuf],
        destination: &'a Path,
        mode: TransferMode,
        cancelled: Arc<AtomicBool>,
    ) -> Self {
        Self {
            sources,
            destination,
            mode,
            cancelled,
            progress: None,
            budget: TransferBudget::default(),
        }
    }

    pub(super) fn with_progress(mut self, progress: Arc<TransferProgress>) -> Self {
        self.progress = Some(progress);
        self
    }

    #[cfg(test)]
    pub(super) fn with_budget(mut self, budget: TransferBudget) -> Self {
        self.budget = budget;
        self
    }

    pub(super) fn run(self, policy: &mut ConflictPolicy) -> TransferOutcome {
        let Self {
            sources,
            destination,
            mode,
            cancelled,
            progress,
            budget,
        } = self;
        let progress = progress.as_deref();
        let mut completed = Vec::new();
        let mut failures = Vec::new();
        let mut skipped = Vec::new();
        let mut already_in_place = Vec::new();
        let mut cancelled_sources = Vec::new();
        let mut copied_sources: Ledger<PathSnapshot> = Ledger::new();
        let mut copied_created: Ledger<PathSnapshot> = Ledger::new();
        let mut merged_created: Ledger<PathSnapshot> = Ledger::new();
        let mut moved: Vec<MoveRecord> = Vec::new();
        let mut moved_snapshots = 0;
        let mut move_undo_unavailable = false;
        // Unlike copy bookkeeping, a lost move record or an over-budget
        // replacement does not spoil what was already recorded: every other
        // rename is still exactly reversible, so those records stay and only
        // the operation as a whole is reported as not undoable.
        let mut replaced: Vec<ReplacedItem> = Vec::new();
        let mut replaced_bytes: u64 = 0;
        let mut replacement_undo_unavailable = false;
        let fail = |failures: &mut Vec<PathFailure>, source: &Path, message: String| {
            failures.push(PathFailure::new(source, message));
        };

        if let Some(progress) = progress {
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
                fail(&mut failures, source, "Source has no file name".to_string());
                continue;
            };
            let copy_remaining = |copied_sources: &Ledger<_>,
                                  copied_created: &Ledger<_>,
                                  merged_created: &Ledger<_>| {
                budget.undo_snapshot_limit.saturating_sub(
                    copied_sources.entries.len()
                        + copied_created.entries.len()
                        + merged_created.entries.len(),
                )
            };
            let plan = plan_source(source, destination, destination.join(name), mode, policy);
            let (target, displaced) = match plan {
                SourcePlan::Transfer(target) => (target, None),
                // Move what is there aside before publishing over it, so the
                // replacement is never destroyed by a transfer that then fails.
                SourcePlan::Replace(target) => match quarantine_for_replacement(&target) {
                    Ok(item) => (target, Some(item)),
                    Err(error) => {
                        fail(&mut failures, source, error.to_string());
                        continue;
                    }
                },
                // A merge adds to a tree that is already there rather than
                // publishing a new one, so it runs on its own path and records
                // what it added separately.
                SourcePlan::Merge(target) => {
                    if let Some(progress) = progress {
                        progress.set_current_path(Some(source.clone()));
                    }
                    let remaining = if merged_created.unavailable {
                        0
                    } else {
                        copy_remaining(&copied_sources, &copied_created, &merged_created)
                    };
                    let outcome =
                        merge_directories(source, &target, &cancelled, progress, remaining);
                    // A merge that stopped short still added what it added.
                    // Those additions are on disk whatever comes next, so they
                    // belong in the record rather than in a return value the
                    // caller reads as "nothing happened".
                    if outcome.undoable {
                        merged_created.extend(outcome.created);
                    } else {
                        merged_created.give_up();
                    }
                    match outcome.stopped {
                        None => completed.push(CompletedTransfer {
                            source: source.clone(),
                            destination: target,
                        }),
                        // Cancelling is an answer, not a fault. Reporting it as
                        // a failure would tell the user their merge broke, and
                        // carrying on would attempt work they just stopped.
                        Some(MergeStop::Cancelled) => {
                            cancelled_sources.extend(sources[index..].iter().cloned());
                            break;
                        }
                        Some(MergeStop::Failed(error)) => {
                            fail(&mut failures, source, error.to_string());
                        }
                    }
                    continue;
                }
                // Not a refusal and not work: the item is where it was asked
                // to be, so the request is already satisfied. Reporting it as
                // skipped or failed would invent a problem the user does not
                // have.
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
                    fail(&mut failures, source, message);
                    continue;
                }
            };
            if let Some(progress) = progress {
                progress.set_current_path(Some(source.clone()));
            }
            let result = match mode {
                TransferMode::Copy => {
                    let remaining = if copied_created.unavailable {
                        0
                    } else {
                        copy_remaining(&copied_sources, &copied_created, &merged_created)
                    };
                    copy_one(source, &target, &cancelled, progress, remaining / 2).map(|copied| {
                        if copied.overflowed || !copied.undoable {
                            copied_sources.give_up();
                            copied_created.give_up();
                        } else {
                            copied_sources.extend(copied.sources);
                            copied_created.extend(copied.created);
                        }
                    })
                }
                TransferMode::Move => {
                    let remaining = if move_undo_unavailable {
                        0
                    } else {
                        budget.undo_snapshot_limit.saturating_sub(moved_snapshots)
                    };
                    move_one(source, &target, remaining).map(|record| {
                        match record {
                            Some(record) => {
                                moved_snapshots += record.expected_state.len();
                                moved.push(record);
                            }
                            // The rename committed; only its undo record was lost.
                            None => move_undo_unavailable = true,
                        }
                        if let Some(progress) = progress {
                            progress.complete_item();
                        }
                    })
                }
            };

            match result {
                Ok(()) => {
                    if let Some(item) = displaced {
                        // Holding the displaced object is what makes this
                        // undoable. Past the budget the replacement still
                        // stands; it simply stops being reversible, which the
                        // caller reports.
                        let bytes = item.bytes();
                        if replaced_bytes.saturating_add(bytes) > budget.replacement_undo_byte_limit
                        {
                            erase_replacement_quarantine(&item);
                            replacement_undo_unavailable = true;
                        } else {
                            replaced_bytes = replaced_bytes.saturating_add(bytes);
                            replaced.push(item);
                        }
                    }
                    completed.push(CompletedTransfer {
                        source: source.clone(),
                        destination: target,
                    });
                }
                Err(error) => {
                    // The transfer failed, so put back what it displaced rather
                    // than leaving the destination empty. When even that fails
                    // the quarantine holds the user's only copy, so it leaves
                    // undo storage for recovery storage before this message is
                    // written.
                    let mut message = error.to_string();
                    if let Some(item) = displaced
                        && let Err(unrestored) = restore_replaced_items(std::slice::from_ref(&item))
                    {
                        message.push_str(&format!("; {}", preserve_unrestored(unrestored)));
                    }
                    fail(&mut failures, source, message);
                }
            }
        }

        let undo_unavailable = copied_created.unavailable
            || merged_created.unavailable
            || move_undo_unavailable
            || replacement_undo_unavailable;
        let operation = match mode {
            TransferMode::Copy
                if !copied_created.entries.is_empty() || !merged_created.entries.is_empty() =>
            {
                Some(OperationRecord::Copy {
                    sources: copied_sources.entries,
                    destination: destination.to_path_buf(),
                    created: copied_created.entries,
                    replaced: std::mem::take(&mut replaced),
                    merged: merged_created.entries,
                })
            }
            TransferMode::Move if !moved.is_empty() => Some(OperationRecord::Move {
                transfers: moved,
                replaced: std::mem::take(&mut replaced),
            }),
            _ => None,
        };
        // Nothing will carry these into the journal, so they can never be
        // restored.
        for item in &replaced {
            erase_replacement_quarantine(item);
        }

        if let Some(progress) = progress {
            progress.set_current_path(None);
        }
        TransferOutcome {
            operation,
            completed,
            failures,
            skipped,
            already_in_place,
            cancelled: cancelled_sources,
            undo_unavailable,
        }
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
        if kind.is_dir()
            && let Ok(entries) = fs::read_dir(&path)
        {
            pending.extend(entries.flatten().map(|entry| entry.path()));
        }
    }
}

/// Move one entry, returning `Ok(None)` when the rename committed but its undo
/// record could not be captured. A committed move must never be reported as a
/// failure: the caller would leave a vanished source in the browser, keep a
/// dangling cut clipboard, and tell the user nothing happened.
fn move_one(
    source: &Path,
    destination: &Path,
    snapshot_limit: usize,
) -> Result<Option<MoveRecord>> {
    ensure_unoccupied(destination)?;
    ensure_not_self_containing(source, destination, "move")?;
    // Prepare: walk the tree before the rename, not after, and treat the walk
    // as bookkeeping rather than a precondition. A rename does not care what a
    // directory holds — a tree with sockets or FIFOs in it is still movable,
    // it just cannot be described for undo. Exceeding the budget reads the
    // same way: the move happens, and it is reported as not undoable.
    let prepared = snapshot_tree_within(source, snapshot_limit).ok();
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
