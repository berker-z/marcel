//! The application's single owner of filesystem operations.
//!
//! Every mutation Marcel performs runs here, not in the window that asked for
//! it. A window is a surface: it starts operations, shows their questions and
//! their reports, and folds their effects into its own projection. It does not
//! own the work, the undo journal, the clipboard, or the busy lock.
//!
//! That split is the point. When operations were window-owned, closing a window
//! mid-copy removed the only progress surface and the only reconciler while the
//! work continued on the blocking pool, and it silently discarded that window's
//! entire undo history. Two windows also kept two journals and two busy locks,
//! so each could mutate a path the other's records depended on and the later
//! Undo would refuse, indistinguishable from external tampering.
//!
//! Nautilus reaches the same conclusion from the other direction: its file
//! operations belong to `NautilusApplication`, and its undo manager is a
//! singleton. Marcel keeps its own stronger model — identity-validated undo,
//! quarantine-backed replacement — and only moves where that model lives.
//!
//! Reaching a user interface stays a requirement rather than an assumption.
//! Sprint 18 made conflict decisions interactive, so an operation can block a
//! worker thread on an answer. An operation that outlives its window is
//! re-homed onto another live window; with no window at all its questions
//! resolve to refusal immediately, because a worker must never park on a reply
//! that cannot arrive.

use std::{
    collections::HashSet,
    ffi::OsString,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gpui::{
    AnyWindowHandle, App, AppContext as _, Context, Entity, EventEmitter, Global,
    ParentElement as _, Styled as _, Task, Window, div,
};
use gpui_component::{
    WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    checkbox::Checkbox,
    dialog::DialogFooter,
    input::{Input, InputState},
};

use crate::{
    conflict::{
        ConflictDecision, ConflictPolicy, ConflictResponse, PendingConflict, PromptingResolver,
        unique_name_in,
    },
    delete_ops::{DeleteOutcome, delete_paths, summarize_failures as summarize_delete_failures},
    file_ops::{
        CommittedOperation, CompletedTransfer, DirectoryChanges, MutationOutcome, OperationJournal,
        OperationRecord, TransferMode, TransferProgress, create_directory, create_zip_operation,
        extract_archive_operation, redo_operation, rename_entry, summarize_failures,
        transfer_changes, transfer_paths_with_conflicts, undo_operation,
    },
    fs::display_filename,
    surface::{self, Report},
    trash_ops::{
        TrashOutcome, TrashRecord, purge_trash_records, restore_trash_records,
        summarize_failures as summarize_trash_failures, trash_paths,
    },
};

/// How often an active operation's progress is redrawn.
const PROGRESS_REFRESH_INTERVAL: Duration = Duration::from_millis(80);

#[derive(Clone, Debug)]
pub struct FileClipboard {
    pub mode: TransferMode,
    pub paths: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationProgressKind {
    Copy,
    Move,
    Compress,
    Extract,
    Delete,
    EmptyTrash,
}

pub struct ActiveOperationProgress {
    pub kind: OperationProgressKind,
    pub source_count: usize,
    pub detail: String,
    pub cancellable: bool,
    pub progress: Arc<TransferProgress>,
}

/// An effect every window folds into its own projection.
///
/// Effects are broadcast rather than applied to the initiating window, because
/// the operation belongs to the application. A second window showing the
/// destination folder now reconciles from the operation itself instead of
/// waiting for a watcher event, and a window that has since closed simply is
/// not there to hear it.
pub enum OperationEvent {
    /// Paths the operation created, changed, or removed.
    Applied {
        changes: DirectoryChanges,
        /// Candidates the initiating window should reveal, if one of them
        /// landed in the folder that window is showing.
        reveal: Vec<PathBuf>,
        origin: Option<AnyWindowHandle>,
    },
    /// Entries that left the Trash, by backing path.
    TrashRemoved(Vec<PathBuf>),
    /// Entries that entered the Trash or changed inside it.
    TrashUpserted(Vec<TrashRecord>),
    /// The Trash listing can no longer be trusted and must be reloaded.
    TrashInvalidated,
}

struct GlobalOperations(Entity<OperationCoordinator>);

impl Global for GlobalOperations {}

/// Create the application's operation owner and arrange for it to release what
/// it is holding when the application exits.
pub fn init(cx: &mut App) {
    let coordinator = cx.new(|_| OperationCoordinator::default());
    cx.set_global(GlobalOperations(coordinator.clone()));
    // Quitting destroys the records that could restore a replaced file, so the
    // data they were holding aside becomes unreachable at that moment. The
    // journal used to be released when a window closed, which is exactly the
    // discarded history this module exists to stop.
    cx.on_app_quit(move |cx| {
        coordinator.update(cx, |this, _| this.release_unreachable_quarantines());
        async {}
    })
    .detach();
}

/// The application's operation owner.
pub fn global(cx: &App) -> Entity<OperationCoordinator> {
    cx.global::<GlobalOperations>().0.clone()
}

#[derive(Default)]
pub struct OperationCoordinator {
    journal: OperationJournal,
    clipboard: Option<FileClipboard>,
    busy: bool,
    cancel: Option<Arc<AtomicBool>>,
    task: Option<Task<()>>,
    progress: Option<ActiveOperationProgress>,
    progress_task: Option<Task<()>>,
}

impl EventEmitter<OperationEvent> for OperationCoordinator {}

/// Release everything the journal is holding when it goes away.
///
/// A crash cannot run this, which is why startup reclamation exists as well.
impl Drop for OperationCoordinator {
    fn drop(&mut self) {
        self.release_unreachable_quarantines();
    }
}

impl OperationCoordinator {
    pub fn is_busy(&self) -> bool {
        self.busy
    }

    pub fn can_undo(&self) -> bool {
        !self.busy && self.journal.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        !self.busy && self.journal.can_redo()
    }

    pub fn clipboard(&self) -> Option<&FileClipboard> {
        self.clipboard.as_ref()
    }

    pub fn set_clipboard(&mut self, clipboard: Option<FileClipboard>) {
        self.clipboard = clipboard;
    }

    pub fn progress(&self) -> Option<&ActiveOperationProgress> {
        self.progress.as_ref()
    }

    pub fn request_cancel(&self) -> bool {
        let Some(cancel) = &self.cancel else {
            return false;
        };
        cancel.store(true, Ordering::Release);
        true
    }

    pub fn is_cancelling(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|cancel| cancel.load(Ordering::Acquire))
    }

    /// Erase every quarantine no live record can still restore.
    ///
    /// The work is detached rather than done inline because erasing a large
    /// tree must not stall the application that is quitting. A run interrupted
    /// before it finishes leaves the quarantine for the next process to
    /// reclaim, which is the same guarantee a crash already relies on.
    fn release_unreachable_quarantines(&mut self) {
        // The whole item travels, not just its path: erasure validates the
        // recorded identity before removing anything.
        let unreachable = self
            .journal
            .drain()
            .flat_map(|record| record.replaced_items().to_vec())
            .collect::<Vec<_>>();
        if unreachable.is_empty() {
            return;
        }
        std::thread::spawn(move || {
            for item in &unreachable {
                crate::file_ops::erase_replacement_quarantine(item);
            }
        });
    }

    /// Drop the sources that actually moved, matched by exact recorded path.
    ///
    /// Reconstructing this from file names conflates same-named sources as
    /// soon as one transfer can span directories, silently dropping a failed
    /// item from the clipboard and making it unretryable through Paste.
    fn retain_uncompleted_move(
        &mut self,
        clipboard: FileClipboard,
        completed: &[CompletedTransfer],
    ) {
        let completed_sources = completed
            .iter()
            .map(|transfer| transfer.source.as_path())
            .collect::<HashSet<_>>();
        let remaining = clipboard
            .paths
            .into_iter()
            .filter(|path| !completed_sources.contains(path.as_path()))
            .collect::<Vec<_>>();
        self.clipboard = (!remaining.is_empty()).then_some(FileClipboard {
            mode: clipboard.mode,
            paths: remaining,
        });
    }

    fn begin_simple(&mut self) -> bool {
        if self.busy {
            return false;
        }
        self.busy = true;
        self.cancel = None;
        true
    }

    fn begin_transfer(
        &mut self,
        mode: TransferMode,
        source_count: usize,
        destination: PathBuf,
    ) -> Option<(Arc<AtomicBool>, Arc<TransferProgress>)> {
        if self.busy || source_count == 0 {
            return None;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(TransferProgress::default());
        self.busy = true;
        self.cancel = Some(cancel.clone());
        self.progress = Some(ActiveOperationProgress {
            kind: match mode {
                TransferMode::Copy => OperationProgressKind::Copy,
                TransferMode::Move => OperationProgressKind::Move,
            },
            source_count,
            detail: format!("to {}", destination.display()),
            cancellable: true,
            progress: progress.clone(),
        });
        Some((cancel, progress))
    }

    fn begin_permanent_delete(
        &mut self,
        kind: OperationProgressKind,
        source_count: usize,
    ) -> Option<Arc<TransferProgress>> {
        if self.busy || source_count == 0 {
            return None;
        }
        let progress = Arc::new(TransferProgress::default());
        self.busy = true;
        self.cancel = None;
        self.progress = Some(ActiveOperationProgress {
            kind,
            source_count,
            detail: "This cannot be undone".to_string(),
            cancellable: false,
            progress: progress.clone(),
        });
        Some(progress)
    }

    fn begin_archive(
        &mut self,
        kind: OperationProgressKind,
        source_count: usize,
        detail: String,
    ) -> Option<(Arc<AtomicBool>, Arc<TransferProgress>)> {
        if self.busy
            || source_count == 0
            || !matches!(
                kind,
                OperationProgressKind::Compress | OperationProgressKind::Extract
            )
        {
            return None;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(TransferProgress::default());
        progress.set_preparing(true);
        self.busy = true;
        self.cancel = Some(cancel.clone());
        self.progress = Some(ActiveOperationProgress {
            kind,
            source_count,
            detail,
            cancellable: true,
            progress: progress.clone(),
        });
        Some((cancel, progress))
    }

    fn finish_active(&mut self) {
        self.busy = false;
        self.cancel = None;
        self.progress = None;
        // Dropping the progress loop is how it stops. The operation's own task
        // is deliberately left in place: this runs inside it, and dropping a
        // running task cancels whatever it has left to do — here, reporting the
        // result. The next operation replaces the handle.
        self.progress_task.take();
    }

    fn set_task(&mut self, task: Task<()>) {
        self.task = Some(task);
    }

    /// Record an operation and release anything the records it displaced were
    /// holding aside.
    ///
    /// A replaced file is quarantined so undo can put it back. Once its record
    /// leaves the journal nothing can reach it again, so it stops being
    /// recoverable data and becomes a hidden file nobody will ever collect.
    fn record(&mut self, operation: OperationRecord) {
        for evicted in self.journal.record(operation) {
            evicted.release_quarantines();
        }
    }

    fn begin_undo(&mut self) -> Option<OperationRecord> {
        if !self.begin_simple() {
            return None;
        }
        let operation = self.journal.begin_undo();
        if operation.is_none() {
            self.finish_active();
        }
        operation
    }

    fn finish_undo(&mut self, operation: OperationRecord) {
        self.journal.finish_undo(operation);
    }

    fn cancel_undo(&mut self, operation: OperationRecord) {
        self.journal.cancel_undo(operation);
    }

    fn begin_redo(&mut self) -> Option<OperationRecord> {
        if !self.begin_simple() {
            return None;
        }
        let operation = self.journal.begin_redo();
        if operation.is_none() {
            self.finish_active();
        }
        operation
    }

    fn finish_redo(&mut self, operation: OperationRecord) {
        self.journal.finish_redo(operation);
    }

    fn cancel_redo(&mut self, operation: OperationRecord) {
        self.journal.cancel_redo(operation);
    }

    /// Redraw the progress surface of every window while an operation runs.
    fn start_progress_refresh(&mut self, cx: &mut Context<Self>) {
        self.progress_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(PROGRESS_REFRESH_INTERVAL)
                    .await;
                let keep_running = this
                    .update(cx, |this, cx| {
                        let keep_running = this.progress.is_some();
                        if keep_running {
                            cx.notify();
                        }
                        keep_running
                    })
                    .unwrap_or(false);
                if !keep_running {
                    break;
                }
            }
        }));
    }

    /// Publish a mutation that has already committed to the filesystem.
    ///
    /// Every window reduces from the committed effect whether or not undo
    /// bookkeeping survived, so no projection can disagree with the disk.
    /// Returns whether the operation is undoable, for the report.
    fn apply_committed(
        &mut self,
        committed: CommittedOperation,
        reveal: Vec<PathBuf>,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) -> bool {
        let changes = committed.changes().clone();
        let undoable = committed.is_undoable();
        if let Some(record) = committed.into_record() {
            self.record(record);
        }
        cx.emit(OperationEvent::Applied {
            changes,
            reveal,
            origin: Some(origin),
        });
        undoable
    }

    pub fn start_rename(
        &mut self,
        source: PathBuf,
        name: String,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        if !self.begin_simple() {
            return;
        }
        let attempted_destination = source
            .parent()
            .map(|parent| parent.join(&name))
            .unwrap_or_else(|| source.clone());
        let task_source = source.clone();
        let task = cx
            .background_executor()
            .spawn(smol::unblock(move || rename_entry(&task_source, &name)));
        let operation_task = cx.spawn(async move |this, cx| {
            let result = task.await;
            let report = this.update(cx, |this, cx| {
                let report = match result {
                    Ok(committed) => {
                        let destination = committed.path().to_path_buf();
                        let display_name = destination
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let undoable =
                            this.apply_committed(committed, vec![destination], origin, cx);
                        Report::Success(format!(
                            "Renamed to “{display_name}”{}",
                            undo_note(undoable)
                        ))
                    }
                    Err(error) => {
                        // Most failures leave the source untouched. A failure
                        // after the no-replace syscall (for example while
                        // inspecting the result) may still have renamed it, so
                        // revalidate both possible paths instead of assuming
                        // either filesystem state.
                        cx.emit(OperationEvent::Applied {
                            changes: DirectoryChanges {
                                upserted: vec![source, attempted_destination],
                                ..DirectoryChanges::default()
                            },
                            reveal: Vec::new(),
                            origin: Some(origin),
                        });
                        Report::Error(error.to_string())
                    }
                };
                this.finish_active();
                cx.notify();
                report
            });
            surface::deliver(origin, report.ok(), cx);
        });
        self.set_task(operation_task);
        cx.notify();
    }

    pub fn start_create_directory(
        &mut self,
        parent: PathBuf,
        name: String,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        if !self.begin_simple() {
            return;
        }
        let task = cx
            .background_executor()
            .spawn(smol::unblock(move || create_directory(&parent, &name)));
        let operation_task = cx.spawn(async move |this, cx| {
            let result = task.await;
            let report = this.update(cx, |this, cx| {
                let report = match result {
                    Ok(committed) => {
                        let path = committed.path().to_path_buf();
                        let undoable =
                            this.apply_committed(committed, vec![path.clone()], origin, cx);
                        Report::Success(format!(
                            "Created folder “{}”{}",
                            path.file_name()
                                .map(|name| name.to_string_lossy())
                                .unwrap_or_default(),
                            undo_note(undoable)
                        ))
                    }
                    Err(error) => Report::Error(error.to_string()),
                };
                this.finish_active();
                cx.notify();
                report
            });
            surface::deliver(origin, report.ok(), cx);
        });
        self.set_task(operation_task);
        cx.notify();
    }

    pub fn start_compress(
        &mut self,
        sources: Vec<PathBuf>,
        destination: PathBuf,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        let Some((cancel, _progress)) = self.begin_archive(
            OperationProgressKind::Compress,
            sources.len(),
            format!("to {}", destination.display()),
        ) else {
            return;
        };
        self.start_progress_refresh(cx);
        let task = cx.background_executor().spawn(smol::unblock(move || {
            create_zip_operation(&sources, &destination, cancel)
        }));
        let operation_task = cx.spawn(async move |this, cx| {
            let result = task.await;
            let report = this.update(cx, |this, cx| {
                let report = match result {
                    Ok(committed) => {
                        let path = committed.path().to_path_buf();
                        let undoable =
                            this.apply_committed(committed, vec![path.clone()], origin, cx);
                        Report::Success(format!(
                            "Created ZIP “{}”{}",
                            path.file_name()
                                .map(|name| name.to_string_lossy())
                                .unwrap_or_default(),
                            undo_note(undoable)
                        ))
                    }
                    Err(error) => Report::Error(error.to_string()),
                };
                this.finish_active();
                cx.notify();
                report
            });
            surface::deliver(origin, report.ok(), cx);
        });
        self.set_task(operation_task);
        cx.notify();
    }

    pub fn start_extract(
        &mut self,
        archive: PathBuf,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        let Some((cancel, _progress)) = self.begin_archive(
            OperationProgressKind::Extract,
            1,
            format!("from {}", archive.display()),
        ) else {
            return;
        };
        self.start_progress_refresh(cx);
        let task = cx.background_executor().spawn(smol::unblock(move || {
            extract_archive_operation(&archive, cancel)
        }));
        let operation_task = cx.spawn(async move |this, cx| {
            let result = task.await;
            let report = this.update(cx, |this, cx| {
                let report = match result {
                    Ok(committed) => {
                        let path = committed.path().to_path_buf();
                        let undoable =
                            this.apply_committed(committed, vec![path.clone()], origin, cx);
                        Report::Success(format!(
                            "Extracted “{}”{}",
                            path.file_name()
                                .map(|name| name.to_string_lossy())
                                .unwrap_or_default(),
                            undo_note(undoable)
                        ))
                    }
                    Err(error) => Report::Error(error.to_string()),
                };
                this.finish_active();
                cx.notify();
                report
            });
            surface::deliver(origin, report.ok(), cx);
        });
        self.set_task(operation_task);
        cx.notify();
    }

    pub fn start_undo(&mut self, origin: AnyWindowHandle, cx: &mut Context<Self>) {
        let Some(operation) = self.begin_undo() else {
            return;
        };

        let operation_for_task = operation.clone();
        let task = cx
            .background_executor()
            .spawn(smol::unblock(move || undo_operation(&operation_for_task)));

        let operation_task = cx.spawn(async move |this, cx| {
            let result = task.await;
            let report = this.update(cx, |this, cx| {
                let report = match result {
                    MutationOutcome::Committed(committed) => {
                        let reveal = matches!(&operation, OperationRecord::Rename { .. })
                            .then(|| committed.path().to_path_buf());
                        let changes = committed.changes().clone();
                        let message = operation_history_message(&operation, false);
                        let redoable = committed.is_undoable();
                        let redo_record = committed.into_record();
                        match &operation {
                            OperationRecord::Trash { records } => {
                                cx.emit(OperationEvent::TrashRemoved(
                                    records
                                        .iter()
                                        .map(|record| record.backing_path().to_path_buf())
                                        .collect(),
                                ));
                            }
                            OperationRecord::Restore { .. } => {
                                cx.emit(OperationEvent::TrashUpserted(
                                    redo_record
                                        .as_ref()
                                        .and_then(OperationRecord::trash_records)
                                        .unwrap_or_default()
                                        .to_vec(),
                                ));
                            }
                            _ => {}
                        }
                        cx.emit(OperationEvent::Applied {
                            changes,
                            reveal: reveal.into_iter().collect(),
                            origin: Some(origin),
                        });
                        // The undo committed. Losing its record only costs
                        // Redo, so never report the undo itself as failed.
                        if let Some(record) = redo_record {
                            this.finish_undo(record);
                        }
                        Report::Success(format!("{message}{}", redo_note(redoable)))
                    }
                    // Nothing reached the disk, so the record still describes
                    // it exactly and the user can fix the obstacle and retry.
                    MutationOutcome::Unchanged(error) => {
                        this.cancel_undo(operation);
                        Report::Error(error.to_string())
                    }
                    // The undo crossed its commit point. Whatever compensation
                    // achieved, the record's identities predate it, so keeping
                    // it would only produce a "changed or was replaced" refusal
                    // later that blamed the user for Marcel's own recovery.
                    MutationOutcome::Discarded { changes, error } => {
                        cx.emit(OperationEvent::Applied {
                            changes,
                            reveal: Vec::new(),
                            origin: Some(origin),
                        });
                        Report::Error(format!("{error}; this operation can no longer be undone"))
                    }
                };
                this.finish_active();
                cx.notify();
                report
            });
            surface::deliver(origin, report.ok(), cx);
        });
        self.set_task(operation_task);
        cx.notify();
    }

    pub fn start_redo(&mut self, origin: AnyWindowHandle, cx: &mut Context<Self>) {
        let Some(operation) = self.begin_redo() else {
            return;
        };

        let operation_for_task = operation.clone();
        let task = cx
            .background_executor()
            .spawn(smol::unblock(move || redo_operation(&operation_for_task)));

        let operation_task = cx.spawn(async move |this, cx| {
            let result = task.await;
            let report = this.update(cx, |this, cx| {
                let report = match result {
                    MutationOutcome::Committed(committed) => {
                        let path = committed.path().to_path_buf();
                        let changes = committed.changes().clone();
                        let message = operation_history_message(&operation, true);
                        let undoable = committed.is_undoable();
                        let undo_record = committed.into_record();
                        match &operation {
                            OperationRecord::Trash { .. } => {
                                cx.emit(OperationEvent::TrashUpserted(
                                    undo_record
                                        .as_ref()
                                        .and_then(OperationRecord::trash_records)
                                        .unwrap_or_default()
                                        .to_vec(),
                                ));
                            }
                            OperationRecord::Restore { records } => {
                                cx.emit(OperationEvent::TrashRemoved(
                                    records
                                        .iter()
                                        .map(|record| record.backing_path().to_path_buf())
                                        .collect(),
                                ));
                            }
                            _ => {}
                        }
                        cx.emit(OperationEvent::Applied {
                            changes,
                            reveal: vec![path],
                            origin: Some(origin),
                        });
                        // The redo committed. Losing its record only costs Undo.
                        if let Some(record) = undo_record {
                            this.finish_redo(record);
                        }
                        Report::Success(format!("{message}{}", undo_note(undoable)))
                    }
                    MutationOutcome::Unchanged(error) => {
                        this.cancel_redo(operation);
                        Report::Error(error.to_string())
                    }
                    MutationOutcome::Discarded { changes, error } => {
                        cx.emit(OperationEvent::Applied {
                            changes,
                            reveal: Vec::new(),
                            origin: Some(origin),
                        });
                        Report::Error(format!("{error}; this operation can no longer be redone"))
                    }
                };
                this.finish_active();
                cx.notify();
                report
            });
            surface::deliver(origin, report.ok(), cx);
        });
        self.set_task(operation_task);
        cx.notify();
    }

    pub fn start_trash(
        &mut self,
        paths: Vec<PathBuf>,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() || !self.begin_simple() {
            return;
        }
        let task = cx
            .background_executor()
            .spawn(smol::unblock(move || trash_paths(&paths)));
        let operation_task = cx.spawn(async move |this, cx| {
            let outcome = task.await;
            let report = this.update(cx, |this, cx| {
                let changes = DirectoryChanges {
                    removed: outcome.completed.clone(),
                    ..DirectoryChanges::default()
                };
                if !outcome.records.is_empty() {
                    // A window browsing Trash gains the new entries, and the
                    // journal keeps its own copy for Undo.
                    cx.emit(OperationEvent::TrashUpserted(outcome.records.clone()));
                    this.record(OperationRecord::Trash {
                        records: outcome.records,
                    });
                }
                cx.emit(OperationEvent::Applied {
                    changes,
                    reveal: Vec::new(),
                    origin: Some(origin),
                });
                let report = if outcome.failures.is_empty() {
                    Report::Success(format!(
                        "Moved {} item(s) to Trash",
                        outcome.completed.len()
                    ))
                } else {
                    let mut message = summarize_trash_failures(&outcome.failures);
                    if outcome.undo_unavailable {
                        message.push_str("; some successful items are not available to Undo");
                    }
                    Report::Error(message)
                };
                this.finish_active();
                cx.notify();
                report
            });
            surface::deliver(origin, report.ok(), cx);
        });
        self.set_task(operation_task);
        cx.notify();
    }

    pub fn start_restore(
        &mut self,
        records: Vec<TrashRecord>,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        if records.is_empty() || !self.begin_simple() {
            return;
        }
        let count = records.len();
        let backing_paths = records
            .iter()
            .map(|record| record.backing_path().to_path_buf())
            .collect::<Vec<_>>();
        let restored_paths = records
            .iter()
            .map(|record| record.original_path().to_path_buf())
            .collect::<Vec<_>>();
        let task = cx
            .background_executor()
            .spawn(smol::unblock(move || restore_trash_records(&records)));
        let operation_task = cx.spawn(async move |this, cx| {
            let result = task.await;
            let report = this.update(cx, |this, cx| {
                let report = match result {
                    Ok(restored) => {
                        // The payloads are restored. Losing the journal entry
                        // only costs Undo; the Trash view must still update.
                        let undoable = restored.undoable;
                        if undoable {
                            this.record(OperationRecord::Restore {
                                records: restored.records,
                            });
                        }
                        cx.emit(OperationEvent::TrashRemoved(backing_paths));
                        cx.emit(OperationEvent::Applied {
                            changes: DirectoryChanges {
                                upserted: restored_paths,
                                ..DirectoryChanges::default()
                            },
                            reveal: Vec::new(),
                            origin: Some(origin),
                        });
                        Report::Success(format!("Restored {count} item(s){}", undo_note(undoable)))
                    }
                    Err(failure) => {
                        if failure.committed {
                            // Payloads moved before the failure. Compensation
                            // may not have returned all of them, so the listing
                            // can no longer be trusted to match the Trash.
                            cx.emit(OperationEvent::TrashInvalidated);
                        }
                        Report::Error(failure.error.to_string())
                    }
                };
                this.finish_active();
                cx.notify();
                report
            });
            surface::deliver(origin, report.ok(), cx);
        });
        self.set_task(operation_task);
        cx.notify();
    }

    pub fn start_permanent_delete(
        &mut self,
        paths: Vec<PathBuf>,
        trash_records: Option<Vec<TrashRecord>>,
        kind: OperationProgressKind,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        let source_count = trash_records
            .as_ref()
            .map_or(paths.len(), |records| records.len());
        let Some(progress) = self.begin_permanent_delete(kind, source_count) else {
            return;
        };
        self.start_progress_refresh(cx);
        let task = cx.background_executor().spawn(smol::unblock(move || {
            if let Some(records) = trash_records {
                let outcome = purge_trash_records(&records, progress);
                PermanentDeleteResult::Trash {
                    outcome,
                    requested: records,
                }
            } else {
                PermanentDeleteResult::Files(delete_paths(&paths, progress))
            }
        }));
        let operation_task = cx.spawn(async move |this, cx| {
            let result = task.await;
            let report = this.update(cx, |this, cx| {
                let (completed, failure) = match result {
                    PermanentDeleteResult::Files(outcome) => {
                        let failure = (!outcome.failures.is_empty())
                            .then(|| summarize_delete_failures(&outcome.failures));
                        cx.emit(OperationEvent::Applied {
                            changes: DirectoryChanges {
                                removed: outcome.completed.clone(),
                                ..DirectoryChanges::default()
                            },
                            reveal: Vec::new(),
                            origin: Some(origin),
                        });
                        (outcome.completed.len(), failure)
                    }
                    PermanentDeleteResult::Trash { outcome, requested } => {
                        let failure = (!outcome.failures.is_empty())
                            .then(|| summarize_trash_failures(&outcome.failures));
                        let completed = outcome.completed.iter().cloned().collect::<HashSet<_>>();
                        cx.emit(OperationEvent::TrashRemoved(
                            requested
                                .iter()
                                .filter(|record| completed.contains(record.original_path()))
                                .map(|record| record.backing_path().to_path_buf())
                                .collect(),
                        ));
                        (outcome.completed.len(), failure)
                    }
                };
                let report = if let Some(failure) = failure {
                    Report::Error(if completed > 0 {
                        format!("Permanently deleted {completed} item(s); {failure}")
                    } else {
                        failure
                    })
                } else {
                    Report::Success(match kind {
                        OperationProgressKind::EmptyTrash => "Trash emptied".to_string(),
                        _ => format!("Permanently deleted {completed} item(s)"),
                    })
                };
                this.finish_active();
                cx.notify();
                report
            });
            surface::deliver(origin, report.ok(), cx);
        });
        self.set_task(operation_task);
        cx.notify();
    }

    pub fn start_transfer(
        &mut self,
        sources: Vec<PathBuf>,
        destination: PathBuf,
        mode: TransferMode,
        clipboard: Option<FileClipboard>,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        let Some((cancel, progress)) =
            self.begin_transfer(mode, sources.len(), destination.clone())
        else {
            return;
        };
        self.start_progress_refresh(cx);
        // Questions arrive one at a time: the transfer thread waits for each
        // answer, so a second conflict cannot be raised while the first is on
        // screen.
        let (resolver, questions) = PromptingResolver::new();
        cx.spawn(async move |this, cx| {
            while let Ok(pending) = questions.recv().await {
                let Some(coordinator) = this.upgrade() else {
                    break;
                };
                let Some(handle) = cx.update(|cx| surface::current(Some(origin), cx)) else {
                    // Nothing can answer, so dropping the question refuses it
                    // rather than leaving the worker parked forever.
                    break;
                };
                if handle
                    .update(cx, |_, window, cx| {
                        ask_about_conflict(&coordinator, pending, window, cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let task = cx.background_executor().spawn(smol::unblock(move || {
            let mut policy = ConflictPolicy::interactive(resolver);
            transfer_paths_with_conflicts(
                &sources,
                &destination,
                mode,
                cancel,
                progress,
                &mut policy,
            )
        }));

        let operation_task = cx.spawn(async move |this, cx| {
            let outcome = task.await;
            let report = this.update(cx, |this, cx| {
                // Reconcile from the exact recorded transfers. Deriving this
                // from the undo record instead dropped every item that
                // committed without one, leaving a moved source on screen
                // until a watcher event corrected it.
                let changes = transfer_changes(&outcome, mode);
                let reveal = outcome.completed_destinations();
                if let Some(operation) = outcome.operation {
                    this.record(operation);
                }

                if mode == TransferMode::Move
                    && !outcome.completed.is_empty()
                    && let Some(clipboard) = clipboard
                {
                    this.retain_uncompleted_move(clipboard, &outcome.completed);
                }

                cx.emit(OperationEvent::Applied {
                    changes,
                    reveal,
                    origin: Some(origin),
                });

                this.finish_active();
                cx.notify();

                // Dropping a selection onto the folder it already lives in asks
                // for nothing. Announcing "Moved 0 items" would answer a
                // question the user did not ask.
                if outcome.completed.is_empty()
                    && outcome.failures.is_empty()
                    && outcome.skipped.is_empty()
                    && outcome.cancelled.is_empty()
                    && !outcome.already_in_place.is_empty()
                {
                    return None;
                }

                Some(if outcome.failures.is_empty() {
                    let verb = match mode {
                        TransferMode::Copy => "Copied",
                        TransferMode::Move => "Moved",
                    };
                    let skipped = match outcome.skipped.len() {
                        0 => String::new(),
                        count => format!(", skipped {count}"),
                    };
                    Report::Success(format!(
                        "{verb} {} item(s){skipped}{}",
                        outcome.completed.len(),
                        undo_note(!outcome.undo_unavailable)
                    ))
                } else {
                    let mut message = summarize_failures(&outcome.failures);
                    if outcome.undo_unavailable {
                        message.push_str("; some completed items are not available to Undo");
                    }
                    Report::Error(message)
                })
            });
            surface::deliver(origin, report.ok().flatten(), cx);
        });
        self.set_task(operation_task);
        cx.notify();
    }
}

enum PermanentDeleteResult {
    Files(DeleteOutcome),
    Trash {
        outcome: TrashOutcome,
        requested: Vec<TrashRecord>,
    },
}

/// Ask the user about one destination conflict.
///
/// The transfer thread is blocked on the answer, so every route out of this
/// dialog must produce one. Dropping the pending question answers it as a
/// cancellation, which is why the dialog cannot be dismissed by clicking away:
/// an accidental dismissal would abandon the whole transfer.
fn ask_about_conflict(
    coordinator: &Entity<OperationCoordinator>,
    pending: PendingConflict,
    window: &mut Window,
    cx: &mut App,
) {
    let request = pending.request().clone();
    let name = request
        .destination
        .file_name()
        .map(display_filename)
        .unwrap_or_default();
    let folder = request
        .destination
        .parent()
        .map(|parent| display_filename(parent.as_os_str()))
        .unwrap_or_default();
    let kind = if request.destination_is_directory {
        "folder"
    } else {
        "file"
    };
    let suggestion = request
        .destination
        .file_name()
        .and_then(|name| {
            request
                .destination
                .parent()
                .and_then(|parent| unique_name_in(parent, name, request.source_is_directory))
        })
        .map(|name| display_filename(&name))
        .unwrap_or_else(|| name.clone());

    let input = cx.new(|cx| InputState::new(window, cx).default_value(suggestion));
    // The dialog callbacks are shared closures, so the one-shot answer has to
    // be taken out of somewhere they can all reach.
    let pending = Rc::new(std::cell::RefCell::new(Some(pending)));
    // Scoped to this one question: the sticky answer it produces lives in the
    // operation's policy, not here.
    let apply_to_all = Rc::new(std::cell::Cell::new(false));
    let is_merge = request.is_merge();

    let input_for_dialog = input.clone();
    let coordinator = coordinator.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        let input = input_for_dialog.clone();
        // Read the live value, because the checkbox draws whatever it is told.
        // It cannot come from an entity: this closure runs while the update
        // that opened the dialog still holds its lease, so reading one panics.
        let applies_to_all = apply_to_all.get();
        let answer = {
            let pending = pending.clone();
            move |response: ConflictResponse| {
                if let Some(pending) = pending.borrow_mut().take() {
                    pending.answer(ConflictDecision {
                        response,
                        apply_to_all: applies_to_all,
                    });
                }
            }
        };
        let toggle = apply_to_all.clone();
        let redraw = coordinator.clone();
        let (skip, replace, rename, cancel) = (
            answer.clone(),
            answer.clone(),
            answer.clone(),
            answer.clone(),
        );

        dialog
            .title(if request.destination_is_directory {
                "Folder Already Exists"
            } else {
                "File Already Exists"
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(format!(
                        "A {kind} named “{name}” already exists in “{folder}”."
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_sm().child("Rename it to"))
                            .child(Input::new(&input)),
                    )
                    .child(
                        Checkbox::new("conflict-apply-to-all")
                            .checked(applies_to_all)
                            .label(if is_merge {
                                "Do this for every folder"
                            } else {
                                "Do this for every file"
                            })
                            .on_click(move |checked, _, cx| {
                                toggle.set(*checked);
                                // A click is dispatched outside any entity
                                // update, so asking for a redraw here is safe
                                // and is what makes the new value visible.
                                redraw.update(cx, |_, cx| cx.notify());
                            }),
                    ),
            )
            // Each button answers and dismisses on its own rather than sitting
            // inside a DialogClose wrapper. The wrapper closes on a click of
            // its own surrounding element, which a button with its own handler
            // never delivers — so every answer was sent while the dialog stayed
            // on screen, and the next conflict opened another one on top of it.
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("conflict-cancel")
                            .label("Cancel")
                            .outline()
                            .on_click(move |_, window, cx| {
                                cancel(ConflictResponse::Cancel);
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        Button::new("conflict-skip")
                            .label("Skip")
                            .outline()
                            .on_click(move |_, window, cx| {
                                skip(ConflictResponse::Skip);
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        Button::new("conflict-rename")
                            .label("Rename")
                            .outline()
                            .on_click({
                                let input = input.clone();
                                move |_, window, cx| {
                                    // A typed name cannot answer later
                                    // conflicts, so applying to all means
                                    // Marcel picks the names.
                                    rename(if applies_to_all {
                                        ConflictResponse::AutoRename
                                    } else {
                                        ConflictResponse::Rename(OsString::from(
                                            input.read(cx).value().trim().to_string(),
                                        ))
                                    });
                                    window.close_dialog(cx);
                                }
                            }),
                    )
                    .child(
                        Button::new("conflict-replace")
                            .label(if is_merge { "Merge" } else { "Replace" })
                            .with_variant(ButtonVariant::Danger)
                            .on_click(move |_, window, cx| {
                                replace(ConflictResponse::Replace);
                                window.close_dialog(cx);
                            }),
                    ),
            )
            .overlay_closable(false)
            .close_button(false)
    });
    // The suggested name is selected, so typing over it replaces it and Rename
    // works without reaching for the pointer.
    input.update(cx, |input, cx| input.focus(window, cx));
}

fn operation_history_message(operation: &OperationRecord, redo: bool) -> String {
    let name = operation
        .path()
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    match (operation, redo) {
        (OperationRecord::CreateDirectory { .. }, false) => {
            format!("Undid creation of “{name}”")
        }
        (OperationRecord::CreateDirectory { .. }, true) => format!("Recreated folder “{name}”"),
        (OperationRecord::Copy { .. }, false) => "Undid copy".to_string(),
        (OperationRecord::Copy { .. }, true) => "Repeated copy".to_string(),
        (OperationRecord::Move { .. }, false) => "Undid move".to_string(),
        (OperationRecord::Move { .. }, true) => "Repeated move".to_string(),
        (OperationRecord::Trash { .. }, false) => "Restored item(s) from Trash".to_string(),
        (OperationRecord::Trash { .. }, true) => "Moved item(s) to Trash again".to_string(),
        (OperationRecord::Restore { .. }, false) => {
            "Moved restored item(s) back to Trash".to_string()
        }
        (OperationRecord::Restore { .. }, true) => "Restored item(s) again".to_string(),
        (OperationRecord::Rename { source, .. }, false) => {
            let original = source
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default();
            format!("Restored name “{original}”")
        }
        (OperationRecord::Rename { destination, .. }, true) => {
            let renamed = destination
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default();
            format!("Renamed to “{renamed}” again")
        }
        (OperationRecord::ArchiveCreate { .. }, false) => {
            format!("Removed created ZIP “{name}”")
        }
        (OperationRecord::ArchiveCreate { .. }, true) => {
            format!("Created ZIP “{name}” again")
        }
        (OperationRecord::ArchiveExtract { .. }, false) => {
            format!("Removed extracted item “{name}”")
        }
        (OperationRecord::ArchiveExtract { .. }, true) => {
            format!("Extracted “{name}” again")
        }
    }
}

/// Suffix used when a mutation committed but its undo record was lost. Marcel
/// reports that as success with a caveat, never as a failed operation.
fn undo_note(undoable: bool) -> &'static str {
    if undoable {
        ""
    } else {
        " · not available to Undo"
    }
}

fn redo_note(redoable: bool) -> &'static str {
    if redoable {
        ""
    } else {
        " · not available to Redo"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_ops::create_directory;

    #[test]
    fn busy_and_cancel_transitions_are_explicit() {
        let mut coordinator = OperationCoordinator::default();
        assert!(
            coordinator
                .begin_transfer(TransferMode::Copy, 1, PathBuf::from("/destination"))
                .is_some()
        );
        assert!(coordinator.is_busy());
        assert!(coordinator.request_cancel());
        assert!(coordinator.is_cancelling());

        coordinator.finish_active();
        assert!(!coordinator.is_busy());
        assert!(!coordinator.is_cancelling());
    }

    /// The busy lock is one lock for the application, not one per window. Two
    /// windows could otherwise run conflicting operations on the same paths at
    /// the same time, and each would believe it was the only writer.
    #[test]
    fn one_running_operation_locks_out_every_other_surface() {
        let mut coordinator = OperationCoordinator::default();
        assert!(
            coordinator
                .begin_transfer(TransferMode::Move, 2, PathBuf::from("/destination"))
                .is_some()
        );

        assert!(
            coordinator
                .begin_transfer(TransferMode::Copy, 1, PathBuf::from("/elsewhere"))
                .is_none()
        );
        assert!(!coordinator.begin_simple());
        assert!(
            coordinator
                .begin_archive(OperationProgressKind::Compress, 1, String::new())
                .is_none()
        );
        assert!(
            coordinator
                .begin_permanent_delete(OperationProgressKind::Delete, 1)
                .is_none()
        );
        assert!(coordinator.begin_undo().is_none());

        coordinator.finish_active();
        assert!(coordinator.begin_simple());
    }

    #[test]
    fn undo_and_redo_reserve_the_coordinator_until_finished() {
        let root = tempfile::tempdir().unwrap();
        let operation = create_directory(root.path(), "created")
            .unwrap()
            .into_record()
            .expect("creating a directory retains undo");
        let mut coordinator = OperationCoordinator::default();
        coordinator.record(operation.clone());

        assert_eq!(coordinator.begin_undo(), Some(operation.clone()));
        assert!(coordinator.is_busy());
        coordinator.finish_undo(operation.clone());
        coordinator.finish_active();

        assert_eq!(coordinator.begin_redo(), Some(operation));
        assert!(coordinator.is_busy());
    }

    /// History belongs to the application. A record written while one window
    /// was open is still the next Undo after that window is gone, because the
    /// journal never belonged to the window in the first place.
    #[test]
    fn history_outlives_the_surface_that_created_it() {
        let root = tempfile::tempdir().unwrap();
        let operation = create_directory(root.path(), "created")
            .unwrap()
            .into_record()
            .expect("creating a directory retains undo");
        let mut coordinator = OperationCoordinator::default();
        coordinator.record(operation.clone());

        // Whatever surface asked for it has gone; nothing about the coordinator
        // changes.
        assert!(coordinator.can_undo());
        assert_eq!(coordinator.begin_undo(), Some(operation));
    }

    #[test]
    fn completed_cut_items_leave_the_clipboard_and_failures_remain() {
        let mut coordinator = OperationCoordinator::default();
        let clipboard = FileClipboard {
            mode: TransferMode::Move,
            paths: vec![PathBuf::from("/source/a"), PathBuf::from("/source/b")],
        };

        coordinator.retain_uncompleted_move(
            clipboard,
            &[CompletedTransfer {
                source: PathBuf::from("/source/a"),
                destination: PathBuf::from("/destination/a"),
            }],
        );

        assert_eq!(
            coordinator
                .clipboard()
                .map(|clipboard| clipboard.paths.clone()),
            Some(vec![PathBuf::from("/source/b")])
        );
    }

    /// Same-named sources from different directories must be reconciled by
    /// exact path. Basename matching retired the failed item too, making it
    /// unretryable through Paste.
    #[test]
    fn same_named_sources_are_reconciled_by_exact_path() {
        let mut coordinator = OperationCoordinator::default();
        let clipboard = FileClipboard {
            mode: TransferMode::Move,
            paths: vec![
                PathBuf::from("/a/report.pdf"),
                PathBuf::from("/b/report.pdf"),
            ],
        };

        coordinator.retain_uncompleted_move(
            clipboard,
            &[CompletedTransfer {
                source: PathBuf::from("/a/report.pdf"),
                destination: PathBuf::from("/destination/report.pdf"),
            }],
        );

        assert_eq!(
            coordinator
                .clipboard()
                .map(|clipboard| clipboard.paths.clone()),
            Some(vec![PathBuf::from("/b/report.pdf")])
        );
    }
}
