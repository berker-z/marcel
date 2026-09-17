//! Applying a journal record backwards or forwards.
//!
//! Every step validates the recorded identities before its first commit, so a
//! refusal here provably left the disk alone and the record can be retried.
//! Anything past the first commit reports what changed and discards the
//! record; see [`MutationOutcome`].

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
};

use super::local::PathContext as _;
use anyhow::{Context as _, Result, bail};

use super::{
    DirectoryChanges,
    copy::remove_merged_items,
    journal::{
        CommittedOperation, MoveRecord, MutationOutcome, OperationRecord, rebase_snapshots,
        refresh_snapshot_identities, reject_special_entries, remove_snapshotted_tree,
        top_level_paths, validate_snapshot_tree,
    },
    local::{ensure_unoccupied, rename_no_replace},
    mutations::{
        create_directory_at, create_zip_operation, extract_archive_operation, reverse_rename,
    },
    quarantine::{preserve_unrestored, restore_replaced_items},
    transfer::{TransferMode, transfer_paths},
    trash::{TrashMutationFailure, restore_trash_records, retrash_records},
};

pub fn undo_operation(operation: &OperationRecord) -> MutationOutcome {
    let reversed = operation.reverse_directory_changes();
    match operation {
        OperationRecord::CreateDirectory { path, identity } => {
            let prepared = (|| -> Result<()> {
                // Every check runs before the single commit, so a failure here
                // provably left the directory in place.
                let metadata = fs::symlink_metadata(path).with_context(|| {
                    format!("Cannot undo: “{}” no longer exists", path.display())
                })?;
                if !metadata.file_type().is_dir() {
                    bail!("Cannot undo: “{}” is no longer a directory", path.display());
                }
                let mut entries = fs::read_dir(path).at("Cannot inspect", path)?;
                if entries.next().is_some() {
                    bail!("Cannot undo: “{}” is no longer empty", path.display());
                }
                identity.validate(path, "undo")?;
                // Commit: `remove_dir` either removes the directory or leaves it.
                fs::remove_dir(path).at("Could not remove", path)
            })();
            match prepared {
                Ok(()) => MutationOutcome::Committed(CommittedOperation::new(
                    path.clone(),
                    reversed,
                    Some(operation.clone()),
                )),
                Err(error) => MutationOutcome::unchanged(error),
            }
        }
        OperationRecord::Copy { created, .. }
        | OperationRecord::ArchiveCreate { created, .. }
        | OperationRecord::ArchiveExtract { created, .. } => {
            let (replaced, merged) = match operation {
                OperationRecord::Copy {
                    replaced, merged, ..
                } => (replaced.as_slice(), merged.as_slice()),
                _ => (&[][..], &[][..]),
            };
            // The merged removals below commit one at a time, so when a merge
            // contributed, validate the copy's own output first: a stale
            // record must refuse while the disk is still intact, not after
            // the merge's additions are already gone.
            if !merged.is_empty()
                && let Err(error) =
                    validate_snapshot_tree(created).and_then(|()| reject_special_entries(created))
            {
                return MutationOutcome::unchanged(error);
            }
            // Take back what was folded into an existing tree first, while the
            // copy's own output is still whole and nothing has been removed.
            if let Err(failure) = remove_merged_items(merged) {
                return if failure.removed.is_empty() {
                    MutationOutcome::unchanged(failure.error)
                } else {
                    MutationOutcome::discarded(
                        DirectoryChanges::removed(failure.removed),
                        failure.error,
                    )
                };
            }
            let merged_removed = merged
                .iter()
                .map(|snapshot| snapshot.path.clone())
                .collect::<Vec<_>>();
            match remove_snapshotted_tree(created) {
                Ok(()) => {
                    // The output is gone, so whatever it displaced can come
                    // back. A failure here leaves the copy removed, so it
                    // cannot be reported as though nothing happened — and this
                    // record is about to be discarded, so anything still in
                    // quarantine has to leave undo storage with it.
                    if let Err(unrestored) = restore_replaced_items(replaced) {
                        return MutationOutcome::discarded(
                            reversed,
                            preserve_unrestored(unrestored),
                        );
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
                // Nothing at all was removed — the merge contributed nothing
                // and the output still matches the record — so the failure
                // provably left the disk unchanged.
                Err(failure) if failure.removed.is_empty() && merged_removed.is_empty() => {
                    MutationOutcome::unchanged(failure.error)
                }
                // Something is gone: the merge's additions, part of the
                // output, or both. The record describes a disk that no longer
                // exists, so it cannot be retried.
                Err(failure) => {
                    let mut removed = merged_removed;
                    removed.extend(failure.removed);
                    MutationOutcome::discarded(DirectoryChanges::removed(removed), failure.error)
                }
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
                let prepared = validate_snapshot_tree(&transfer.expected_state)
                    .and_then(|()| ensure_unoccupied(&transfer.source));
                if let Err(error) = prepared {
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
            // what they displaced can return. A failure here discards the
            // record, so anything still quarantined has to leave undo storage
            // with it rather than waiting for a sweep to decide it is garbage.
            if let Err(unrestored) = restore_replaced_items(replaced) {
                return MutationOutcome::discarded(reversed, preserve_unrestored(unrestored));
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
            Err(failure) => failure.into(),
        },
        OperationRecord::Restore { records } => match retrash_records(records) {
            Ok(records) => MutationOutcome::Committed(CommittedOperation::new(
                operation.path().to_path_buf(),
                reversed,
                Some(OperationRecord::Restore { records }),
            )),
            Err(failure) => failure.into(),
        },
        OperationRecord::Rename { .. } => reverse_rename(operation).into(),
    }
}

pub fn redo_operation(operation: &OperationRecord) -> MutationOutcome {
    let forward = operation.forward_directory_changes();
    let no_cancel = || Arc::new(AtomicBool::new(false));
    match operation {
        OperationRecord::CreateDirectory { path, .. } => create_directory_at(path.clone()).into(),
        OperationRecord::Copy {
            sources,
            destination,
            ..
        } => {
            if let Err(error) = validate_snapshot_tree(sources) {
                return MutationOutcome::unchanged(error);
            }
            redo_transfer(&top_level_paths(sources), destination, TransferMode::Copy)
        }
        OperationRecord::Move { transfers, .. } => {
            for transfer in transfers {
                if let Err(error) = validate_snapshot_tree(&transfer.expected_state) {
                    return MutationOutcome::unchanged(error);
                }
            }
            let sources = transfers
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
            redo_transfer(&sources, destination, TransferMode::Move)
        }
        OperationRecord::Trash { records } => match retrash_records(records) {
            Ok(records) => MutationOutcome::Committed(CommittedOperation::new(
                operation.path().to_path_buf(),
                forward,
                Some(OperationRecord::Trash { records }),
            )),
            Err(failure) => failure.into(),
        },
        OperationRecord::Restore { records } => match restore_trash_records(records) {
            Ok(restored) => MutationOutcome::Committed(CommittedOperation::new(
                operation.path().to_path_buf(),
                forward,
                restored.undoable.then_some(OperationRecord::Restore {
                    records: restored.records,
                }),
            )),
            Err(failure) => failure.into(),
        },
        OperationRecord::Rename { .. } => reverse_rename(operation).into(),
        // The archive is published atomically, so a failure leaves the
        // destination untouched.
        OperationRecord::ArchiveCreate {
            sources,
            destination,
            ..
        } => match validate_snapshot_tree(sources) {
            Ok(()) => {
                create_zip_operation(&top_level_paths(sources), destination, no_cancel()).into()
            }
            Err(error) => MutationOutcome::unchanged(error),
        },
        OperationRecord::ArchiveExtract { source, .. } => {
            if let Err(error) = validate_snapshot_tree(source) {
                return MutationOutcome::unchanged(error);
            }
            let Some(archive) = top_level_paths(source).into_iter().next() else {
                return MutationOutcome::unchanged(anyhow::anyhow!(
                    "Archive operation has no source"
                ));
            };
            extract_archive_operation(&archive, no_cancel()).into()
        }
    }
}

/// Repeat a transfer, rolling back whatever landed if any part of it fails.
fn redo_transfer(sources: &[PathBuf], destination: &Path, mode: TransferMode) -> MutationOutcome {
    let mut outcome = transfer_paths(sources, destination, mode, Arc::new(AtomicBool::new(false)));
    if outcome.failures.is_empty() {
        // The transfer itself has already applied its own prepare/commit/
        // finalize discipline, so a missing operation here means undo
        // bookkeeping was lost, not that the redo failed. Visible effects come
        // from the exact `CompletedTransfer` records and the transfer mode.
        let path = outcome
            .completed
            .first()
            .map(|transfer| transfer.destination.clone())
            .unwrap_or_else(|| destination.to_path_buf());
        let changes = outcome.changes(mode);
        return MutationOutcome::Committed(CommittedOperation::new(
            path,
            changes,
            outcome.operation,
        ));
    }

    let failure = outcome.summarize_failures();
    if outcome.completed.is_empty() {
        // Nothing reached the destination, so the record still describes the
        // disk and the user can retry.
        return MutationOutcome::unchanged(anyhow::anyhow!("{failure}"));
    }
    let Some(partial) = outcome.operation.take() else {
        // Items transferred but produced no undo bookkeeping, so Marcel cannot
        // take them back. Report exactly what landed.
        return MutationOutcome::discarded(
            outcome.changes(mode),
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
            outcome.changes(mode),
            anyhow::anyhow!("{failure}; rollback also failed: {rollback_error}"),
        ),
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

/// The visible effect of an undo whose compensation could not put everything
/// back: the transfers that were reversed are now at their sources.
fn partial_undo_changes(undone: &[MoveRecord]) -> DirectoryChanges {
    DirectoryChanges {
        removed: undone.iter().map(|t| t.destination.clone()).collect(),
        upserted: undone.iter().map(|t| t.source.clone()).collect(),
    }
}

impl From<TrashMutationFailure> for MutationOutcome {
    /// Trash placement and restoration move payloads between the Trash and
    /// the user's directories, and Marcel reconciles the Trash view from the
    /// operation record rather than from `DirectoryChanges`, so there is no
    /// browser effect to report here — only whether the record survives.
    fn from(failure: TrashMutationFailure) -> Self {
        if failure.committed {
            Self::discarded(DirectoryChanges::default(), failure.error)
        } else {
            Self::unchanged(failure.error)
        }
    }
}
