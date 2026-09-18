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
//! A conflict decision is interactive, so an operation can block a worker
//! thread on an answer. An operation that outlives its window is re-homed onto
//! another live window; with no window at all its questions resolve to refusal
//! immediately, because a worker must never park on a reply that cannot arrive.

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gpui::{AnyWindowHandle, App, AppContext as _, Context, Entity, EventEmitter, Global, Task};

use crate::{
    fsops::{
        CommittedOperation, CompletedTransfer, DirectoryChanges, HistoryDirection, MutationOutcome,
        OperationJournal, OperationRecord, TransferMode, TransferProgress,
        conflict::{ConflictPolicy, PromptingResolver},
        create_directory, create_file, create_zip_operation,
        delete::{DeleteOutcome, delete_paths},
        extract_archive_operation,
        quarantine::erase_replacement_quarantine,
        redo_operation, rename_entry, set_mode,
        transfer::transfer_paths_with_conflicts,
        trash::{
            TrashOutcome, TrashRecord, purge_trash_records, restore_trash_records, trash_paths,
        },
        undo_operation,
    },
    names::display_path_name,
    surface::{self, Report},
};

mod conflict_dialog;

/// How often an active operation's progress is redrawn.
const PROGRESS_REFRESH_INTERVAL: Duration = Duration::from_millis(80);

#[derive(Clone, Debug)]
pub(crate) struct FileClipboard {
    pub mode: TransferMode,
    pub paths: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationProgressKind {
    Copy,
    Move,
    Compress,
    Extract,
    Delete,
    EmptyTrash,
}

impl OperationProgressKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Copy => "Copying",
            Self::Move => "Moving",
            Self::Compress => "Compressing",
            Self::Extract => "Extracting",
            Self::Delete => "Deleting permanently",
            Self::EmptyTrash => "Emptying Trash",
        }
    }
}

/// What the progress card says about the running operation.
pub(crate) struct ActiveOperationProgress {
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
pub(crate) enum OperationEvent {
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
pub(crate) fn global(cx: &App) -> Entity<OperationCoordinator> {
    cx.global::<GlobalOperations>().0.clone()
}

#[derive(Default)]
pub(crate) struct OperationCoordinator {
    journal: OperationJournal,
    clipboard: Option<FileClipboard>,
    busy: bool,
    cancel: Option<Arc<AtomicBool>>,
    /// The window the running operation was started from, while one is
    /// running. Cancellation shortcuts consult it so Escape pressed in an
    /// unrelated window cannot abort another window's transfer.
    operation_origin: Option<AnyWindowHandle>,
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

/// How an operation shows up on the progress card, if it does at all.
struct ProgressCard {
    kind: OperationProgressKind,
    source_count: usize,
    detail: String,
    cancellable: bool,
}

impl ProgressCard {
    fn transfer(mode: TransferMode, source_count: usize, destination: &std::path::Path) -> Self {
        Self {
            kind: match mode {
                TransferMode::Copy => OperationProgressKind::Copy,
                TransferMode::Move => OperationProgressKind::Move,
            },
            source_count,
            detail: format!("to {}", destination.display()),
            cancellable: true,
        }
    }
}

/// What a started operation runs with.
struct Started {
    cancel: Arc<AtomicBool>,
    progress: Arc<TransferProgress>,
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
        self.cancel.as_ref().is_some_and(|cancel| cancel.load(Ordering::Acquire))
    }

    /// Whether a cancellation *shortcut* pressed in `window` may cancel the
    /// running operation.
    ///
    /// The operation belongs to the application, but aborting it is a
    /// decision, and Escape is easy to press for some other reason — clearing
    /// a selection, closing a filter. The initiating window keeps that
    /// authority while it exists; once it is gone the work has re-homed, so
    /// any surviving window holds it. The explicit Cancel control on the
    /// progress card is not gated by this: clicking it says what it means.
    pub fn can_cancel_from(&self, window: AnyWindowHandle, cx: &App) -> bool {
        self.cancel.is_some()
            && match self.operation_origin {
                Some(origin) => origin == window || !cx.windows().contains(&origin),
                None => true,
            }
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
                erase_replacement_quarantine(item);
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
        let completed_sources =
            completed.iter().map(|transfer| transfer.source.as_path()).collect::<HashSet<_>>();
        let remaining = clipboard
            .paths
            .into_iter()
            .filter(|path| !completed_sources.contains(path.as_path()))
            .collect::<Vec<_>>();
        self.clipboard = (!remaining.is_empty())
            .then_some(FileClipboard { mode: clipboard.mode, paths: remaining });
    }

    /// Take the one busy lock.
    ///
    /// `card` puts the operation on every window's progress card and, when
    /// cancellable, arms the cancel flag; an operation without a card is
    /// instantaneous from the user's point of view.
    fn begin(&mut self, card: Option<ProgressCard>) -> Option<Started> {
        if self.busy {
            return None;
        }
        let started = Started {
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(TransferProgress::default()),
        };
        self.busy = true;
        self.cancel = None;
        if let Some(card) = card {
            if card.cancellable {
                self.cancel = Some(started.cancel.clone());
            }
            self.progress = Some(ActiveOperationProgress {
                kind: card.kind,
                source_count: card.source_count,
                detail: card.detail,
                cancellable: card.cancellable,
                progress: started.progress.clone(),
            });
        }
        Some(started)
    }

    fn finish_active(&mut self) {
        self.busy = false;
        self.cancel = None;
        self.operation_origin = None;
        self.progress = None;
        // Dropping the progress loop is how it stops. The operation's own task
        // is deliberately left in place: this runs inside it, and dropping a
        // running task cancels whatever it has left to do — here, reporting the
        // result. The next operation replaces the handle.
        self.progress_task.take();
    }

    /// Run `work` on the blocking pool, then fold its result in with `finish`
    /// and report to whichever window still speaks for `origin`.
    ///
    /// Every operation ends here: the lock is released and the result is
    /// reported in one place, so no path can forget either.
    fn run<T: Send + 'static>(
        &mut self,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
        work: impl FnOnce() -> T + Send + 'static,
        finish: impl FnOnce(&mut Self, T, &mut Context<Self>) -> Option<Report> + 'static,
    ) {
        self.operation_origin = Some(origin);
        if self.progress.is_some() {
            self.start_progress_refresh(cx);
        }
        let task = cx.background_executor().spawn(smol::unblock(work));
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let report = this.update(cx, |this, cx| {
                let report = finish(this, result, cx);
                this.finish_active();
                cx.notify();
                report
            });
            surface::deliver(origin, report.ok().flatten(), cx);
        }));
        cx.notify();
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

    /// Redraw the progress surface of every window while an operation runs.
    fn start_progress_refresh(&mut self, cx: &mut Context<Self>) {
        self.progress_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(PROGRESS_REFRESH_INTERVAL).await;
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

    /// Tell every window what changed on disk.
    fn applied(
        &self,
        changes: DirectoryChanges,
        reveal: Vec<PathBuf>,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        cx.emit(OperationEvent::Applied { changes, reveal, origin: Some(origin) });
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
        self.applied(changes, reveal, origin, cx);
        undoable
    }

    /// Publish a single-path commit and word its success as `verb “name”`.
    fn report_committed(
        &mut self,
        committed: CommittedOperation,
        verb: &str,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) -> Report {
        let path = committed.path().to_path_buf();
        let name = display_path_name(&path);
        let undoable = self.apply_committed(committed, vec![path], origin, cx);
        Report::Success(format!(
            "{verb} “{name}”{}",
            history_note(HistoryDirection::Undo, undoable)
        ))
    }

    pub fn start_rename(
        &mut self,
        source: PathBuf,
        name: String,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        if self.begin(None).is_none() {
            return;
        }
        let attempted_destination =
            source.parent().map(|parent| parent.join(&name)).unwrap_or_else(|| source.clone());
        let task_source = source.clone();
        self.run(
            origin,
            cx,
            move || rename_entry(&task_source, &name),
            move |this, result, cx| match result {
                Ok(committed) => Some(this.report_committed(committed, "Renamed to", origin, cx)),
                Err(error) => {
                    // Most failures leave the source untouched. A failure
                    // after the no-replace syscall (for example while
                    // inspecting the result) may still have renamed it, so
                    // revalidate both possible paths instead of assuming
                    // either filesystem state.
                    this.applied(
                        DirectoryChanges::upserted(vec![source, attempted_destination]),
                        Vec::new(),
                        origin,
                        cx,
                    );
                    Some(Report::Error(error.to_string()))
                }
            },
        );
    }

    pub fn start_create_directory(
        &mut self,
        parent: PathBuf,
        name: String,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        if self.begin(None).is_none() {
            return;
        }
        self.run_committing(origin, cx, "Created folder", move || create_directory(&parent, &name));
    }

    pub fn start_create_file(
        &mut self,
        parent: PathBuf,
        name: String,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        if self.begin(None).is_none() {
            return;
        }
        self.run_committing(origin, cx, "Created", move || create_file(&parent, &name));
    }

    /// Change one object's permission bits, journalled like any other edit.
    pub fn start_set_mode(
        &mut self,
        path: PathBuf,
        mode: u32,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        if self.begin(None).is_none() {
            return;
        }
        self.run_committing(origin, cx, "Changed permissions of", move || set_mode(&path, mode));
    }

    /// Copy the selection into the folder it lives in.
    ///
    /// The transfer already answers "copy this onto itself" by choosing a
    /// free name, so Duplicate is that transfer with no question to ask.
    pub fn start_duplicate(
        &mut self,
        sources: Vec<PathBuf>,
        destination: PathBuf,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        self.start_transfer(sources, destination, TransferMode::Copy, None, origin, cx);
    }

    pub fn start_compress(
        &mut self,
        sources: Vec<PathBuf>,
        destination: PathBuf,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        if sources.is_empty() {
            return;
        }
        let card = ProgressCard {
            kind: OperationProgressKind::Compress,
            source_count: sources.len(),
            detail: format!("to {}", destination.display()),
            cancellable: true,
        };
        let Some(started) = self.begin_prepared(card) else {
            return;
        };
        self.run_committing(origin, cx, "Created ZIP", move || {
            create_zip_operation(&sources, &destination, started.cancel)
        });
    }

    pub fn start_extract(
        &mut self,
        archive: PathBuf,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        let card = ProgressCard {
            kind: OperationProgressKind::Extract,
            source_count: 1,
            detail: format!("from {}", archive.display()),
            cancellable: true,
        };
        let Some(started) = self.begin_prepared(card) else {
            return;
        };
        // Publishing the output can meet an occupied destination, which is
        // the same question a copy asks and gets the same dialog.
        let (resolver, questions) = PromptingResolver::new();
        conflict_dialog::serve(questions, origin, cx);
        self.run_committing(origin, cx, "Extracted", move || {
            let mut policy = ConflictPolicy::interactive(resolver);
            extract_archive_operation(&archive, started.cancel, &mut policy)
        });
    }

    /// An archive shows "preparing" on the card until its backend reports.
    fn begin_prepared(&mut self, card: ProgressCard) -> Option<Started> {
        let started = self.begin(Some(card))?;
        started.progress.set_preparing(true);
        Some(started)
    }

    /// Run a single-path mutation whose failure leaves the disk untouched:
    /// success is worded as `verb “name”`, failure as the error.
    fn run_committing(
        &mut self,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
        verb: &'static str,
        work: impl FnOnce() -> anyhow::Result<CommittedOperation> + Send + 'static,
    ) {
        self.run(origin, cx, work, move |this, result, cx| {
            Some(match result {
                Ok(committed) => this.report_committed(committed, verb, origin, cx),
                Err(error) => Report::Error(error.to_string()),
            })
        });
    }

    pub fn start_undo(&mut self, origin: AnyWindowHandle, cx: &mut Context<Self>) {
        self.start_history(HistoryDirection::Undo, origin, cx);
    }

    pub fn start_redo(&mut self, origin: AnyWindowHandle, cx: &mut Context<Self>) {
        self.start_history(HistoryDirection::Redo, origin, cx);
    }

    fn start_history(
        &mut self,
        direction: HistoryDirection,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        if self.begin(None).is_none() {
            return;
        }
        let Some(record) = self.journal.begin(direction) else {
            self.finish_active();
            return;
        };
        let stepped = record.clone();
        self.run(
            origin,
            cx,
            move || match direction {
                HistoryDirection::Undo => undo_operation(&stepped),
                HistoryDirection::Redo => redo_operation(&stepped),
            },
            move |this, outcome, cx| {
                Some(this.finish_history(direction, record, outcome, origin, cx))
            },
        );
    }

    fn finish_history(
        &mut self,
        direction: HistoryDirection,
        record: OperationRecord,
        outcome: MutationOutcome,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) -> Report {
        let step = match direction {
            HistoryDirection::Undo => "undone",
            HistoryDirection::Redo => "redone",
        };
        match outcome {
            MutationOutcome::Committed(committed) => {
                // A redo lands the item where it was; an undo only has
                // somewhere to point at for a rename.
                let reveal = match direction {
                    HistoryDirection::Undo if !matches!(record, OperationRecord::Rename { .. }) => {
                        Vec::new()
                    }
                    _ => vec![committed.path().to_path_buf()],
                };
                let changes = committed.changes().clone();
                let message = history_message(&record, direction);
                let reversible = committed.is_undoable();
                let next = committed.into_record();
                // A Trash view reconciles from the records themselves: what
                // this step took out of the Trash, or what it put in.
                if let Some(records) = record.trash_records() {
                    let leaves_trash = matches!(
                        (&record, direction),
                        (OperationRecord::Trash { .. }, HistoryDirection::Undo)
                            | (OperationRecord::Restore { .. }, HistoryDirection::Redo)
                    );
                    cx.emit(if leaves_trash {
                        OperationEvent::TrashRemoved(backing_paths(records))
                    } else {
                        OperationEvent::TrashUpserted(
                            next.as_ref()
                                .and_then(OperationRecord::trash_records)
                                .unwrap_or_default()
                                .to_vec(),
                        )
                    });
                }
                self.applied(changes, reveal, origin, cx);
                // The step committed. Losing its record only costs the
                // reverse step, so never report the step itself as failed.
                if let Some(next) = next {
                    self.journal.finish(direction, next);
                }
                Report::Success(format!(
                    "{message}{}",
                    history_note(direction.opposite(), reversible)
                ))
            }
            // Nothing reached the disk, so the record still describes it
            // exactly and the user can fix the obstacle and retry.
            MutationOutcome::Unchanged(error) => {
                self.journal.cancel(direction, record);
                Report::Error(error.to_string())
            }
            // The step crossed its commit point. Whatever compensation
            // achieved, the record's identities predate it, so keeping it
            // would only produce a "changed or was replaced" refusal later
            // that blamed the user for Marcel's own recovery.
            MutationOutcome::Discarded { changes, error } => {
                self.applied(changes, Vec::new(), origin, cx);
                Report::Error(format!("{error}; this operation can no longer be {step}"))
            }
        }
    }

    pub fn start_trash(
        &mut self,
        paths: Vec<PathBuf>,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() || self.begin(None).is_none() {
            return;
        }
        self.run(
            origin,
            cx,
            move || trash_paths(&paths),
            move |this, mut outcome, cx| {
                let records = std::mem::take(&mut outcome.records);
                if !records.is_empty() {
                    // A window browsing Trash gains the new entries, and the
                    // journal keeps its own copy for Undo.
                    cx.emit(OperationEvent::TrashUpserted(records.clone()));
                    this.record(OperationRecord::Trash { records });
                }
                this.applied(
                    DirectoryChanges::removed(outcome.completed.clone()),
                    Vec::new(),
                    origin,
                    cx,
                );
                Some(if outcome.failures.is_empty() {
                    Report::Success(format!("Moved {} item(s) to Trash", outcome.completed.len()))
                } else {
                    let mut message = outcome.summarize_failures();
                    if outcome.undo_unavailable {
                        message.push_str("; some successful items are not available to Undo");
                    }
                    Report::Error(message)
                })
            },
        );
    }

    pub fn start_restore(
        &mut self,
        records: Vec<TrashRecord>,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        if records.is_empty() || self.begin(None).is_none() {
            return;
        }
        let count = records.len();
        let backing = backing_paths(&records);
        let restored_paths =
            records.iter().map(|record| record.original_path().to_path_buf()).collect::<Vec<_>>();
        self.run(
            origin,
            cx,
            move || restore_trash_records(&records),
            move |this, result, cx| {
                Some(match result {
                    Ok(restored) => {
                        // The payloads are restored. Losing the journal entry
                        // only costs Undo; the Trash view must still update.
                        let undoable = restored.undoable;
                        if undoable {
                            this.record(OperationRecord::Restore { records: restored.records });
                        }
                        cx.emit(OperationEvent::TrashRemoved(backing));
                        this.applied(
                            DirectoryChanges::upserted(restored_paths),
                            Vec::new(),
                            origin,
                            cx,
                        );
                        Report::Success(format!(
                            "Restored {count} item(s){}",
                            history_note(HistoryDirection::Undo, undoable)
                        ))
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
                })
            },
        );
    }

    pub fn start_permanent_delete(
        &mut self,
        paths: Vec<PathBuf>,
        trash_records: Option<Vec<TrashRecord>>,
        kind: OperationProgressKind,
        origin: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        let source_count = trash_records.as_ref().map_or(paths.len(), |records| records.len());
        let card = ProgressCard {
            kind,
            source_count,
            detail: "This cannot be undone".to_string(),
            cancellable: false,
        };
        if source_count == 0 {
            return;
        }
        let Some(started) = self.begin(Some(card)) else {
            return;
        };
        self.run(
            origin,
            cx,
            move || match trash_records {
                Some(records) => {
                    PermanentDeleteResult::Trash(purge_trash_records(&records, started.progress))
                }
                None => PermanentDeleteResult::Files(delete_paths(&paths, started.progress)),
            },
            move |this, result, cx| {
                let (completed, failure) = match result {
                    PermanentDeleteResult::Files(outcome) => {
                        let failure =
                            (!outcome.failures.is_empty()).then(|| outcome.summarize_failures());
                        this.applied(
                            DirectoryChanges::removed(outcome.completed.clone()),
                            Vec::new(),
                            origin,
                            cx,
                        );
                        (outcome.completed.len(), failure)
                    }
                    PermanentDeleteResult::Trash(outcome) => {
                        let failure =
                            (!outcome.failures.is_empty()).then(|| outcome.summarize_failures());
                        // Reconcile the Trash view from the exact purged
                        // records. Matching by original path removed a
                        // surviving twin entry — the same file trashed twice —
                        // from the listing when only one purge succeeded.
                        cx.emit(OperationEvent::TrashRemoved(backing_paths(&outcome.records)));
                        (outcome.completed.len(), failure)
                    }
                };
                Some(match failure {
                    Some(failure) if completed > 0 => {
                        Report::Error(format!("Permanently deleted {completed} item(s); {failure}"))
                    }
                    Some(failure) => Report::Error(failure),
                    None => Report::Success(match kind {
                        OperationProgressKind::EmptyTrash => "Trash emptied".to_string(),
                        _ => format!("Permanently deleted {completed} item(s)"),
                    }),
                })
            },
        );
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
        if sources.is_empty() {
            return;
        }
        let card = ProgressCard::transfer(mode, sources.len(), &destination);
        let Some(started) = self.begin(Some(card)) else {
            return;
        };
        let (resolver, questions) = PromptingResolver::new();
        conflict_dialog::serve(questions, origin, cx);

        self.run(
            origin,
            cx,
            move || {
                let mut policy = ConflictPolicy::interactive(resolver);
                transfer_paths_with_conflicts(
                    &sources,
                    &destination,
                    mode,
                    started.cancel,
                    started.progress,
                    &mut policy,
                )
            },
            move |this, mut outcome, cx| {
                // Reconcile from the exact recorded transfers. Deriving this
                // from the undo record instead dropped every item that
                // committed without one, leaving a moved source on screen
                // until a watcher event corrected it.
                let changes = outcome.changes(mode);
                let reveal = outcome.completed_destinations();
                if let Some(operation) = outcome.operation.take() {
                    this.record(operation);
                }
                if mode == TransferMode::Move
                    && !outcome.completed.is_empty()
                    && let Some(clipboard) = clipboard
                {
                    this.retain_uncompleted_move(clipboard, &outcome.completed);
                }
                this.applied(changes, reveal, origin, cx);

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
                    let skipped = match outcome.skipped.len() {
                        0 => String::new(),
                        count => format!(", skipped {count}"),
                    };
                    Report::Success(format!(
                        "{} {} item(s){skipped}{}",
                        mode.done(),
                        outcome.completed.len(),
                        history_note(HistoryDirection::Undo, !outcome.undo_unavailable)
                    ))
                } else {
                    let mut message = outcome.summarize_failures();
                    if outcome.undo_unavailable {
                        message.push_str("; some completed items are not available to Undo");
                    }
                    Report::Error(message)
                })
            },
        );
    }
}

enum PermanentDeleteResult {
    Files(DeleteOutcome),
    Trash(TrashOutcome),
}

fn backing_paths(records: &[TrashRecord]) -> Vec<PathBuf> {
    records.iter().map(|record| record.backing_path().to_path_buf()).collect()
}

fn history_message(operation: &OperationRecord, direction: HistoryDirection) -> String {
    let name = display_path_name(operation.path());
    let redo = direction == HistoryDirection::Redo;
    match (operation, redo) {
        (OperationRecord::CreateDirectory { .. }, false) => {
            format!("Undid creation of “{name}”")
        }
        (OperationRecord::CreateDirectory { .. }, true) => format!("Recreated folder “{name}”"),
        (OperationRecord::CreateFile { .. }, false) => format!("Undid creation of “{name}”"),
        (OperationRecord::CreateFile { .. }, true) => format!("Recreated “{name}”"),
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
            let original = display_path_name(source);
            format!("Restored name “{original}”")
        }
        (OperationRecord::Rename { .. }, true) => format!("Renamed to “{name}” again"),
        (OperationRecord::SetMode { previous, .. }, false) => {
            format!("Restored permissions {previous:04o} on “{name}”")
        }
        (OperationRecord::SetMode { mode, .. }, true) => {
            format!("Set permissions {mode:04o} on “{name}” again")
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

/// Suffix used when a mutation committed but its record for the next step was
/// lost. Marcel reports that as success with a caveat, never as a failed
/// operation.
fn history_note(next: HistoryDirection, available: bool) -> &'static str {
    match (next, available) {
        (_, true) => "",
        (HistoryDirection::Undo, false) => " · not available to Undo",
        (HistoryDirection::Redo, false) => " · not available to Redo",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_and_cancel_transitions_are_explicit() {
        let mut coordinator = OperationCoordinator::default();
        let card = ProgressCard::transfer(TransferMode::Copy, 1, &PathBuf::from("/d"));
        assert!(coordinator.begin(Some(card)).is_some());
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
        let card = ProgressCard::transfer(TransferMode::Move, 2, &PathBuf::from("/d"));
        assert!(coordinator.begin(Some(card)).is_some());

        let other = ProgressCard::transfer(TransferMode::Copy, 1, &PathBuf::from("/e"));
        assert!(coordinator.begin(Some(other)).is_none());
        assert!(coordinator.begin(None).is_none());

        coordinator.finish_active();
        assert!(coordinator.begin(None).is_some());
    }

    /// An operation that never shows a card is not cancellable: there is no
    /// flag for Escape to set, so it cannot be aborted halfway.
    #[test]
    fn an_instantaneous_operation_cannot_be_cancelled() {
        let mut coordinator = OperationCoordinator::default();
        assert!(coordinator.begin(None).is_some());
        assert!(!coordinator.request_cancel());
        assert!(coordinator.progress().is_none());
    }

    #[test]
    fn undo_and_redo_reserve_the_journal_until_finished() {
        let root = tempfile::tempdir().unwrap();
        let operation = create_directory(root.path(), "created")
            .unwrap()
            .into_record()
            .expect("creating a directory retains undo");
        let mut coordinator = OperationCoordinator::default();
        coordinator.record(operation.clone());

        // History belongs to the application. Whatever surface asked for it
        // has gone; nothing about the coordinator changes.
        assert!(coordinator.can_undo());
        assert_eq!(coordinator.journal.begin(HistoryDirection::Undo), Some(operation.clone()));
        assert!(!coordinator.can_undo());
        coordinator.journal.finish(HistoryDirection::Undo, operation.clone());
        assert_eq!(coordinator.journal.begin(HistoryDirection::Redo), Some(operation));
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
            coordinator.clipboard().map(|clipboard| clipboard.paths.clone()),
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
            paths: vec![PathBuf::from("/a/report.pdf"), PathBuf::from("/b/report.pdf")],
        };

        coordinator.retain_uncompleted_move(
            clipboard,
            &[CompletedTransfer {
                source: PathBuf::from("/a/report.pdf"),
                destination: PathBuf::from("/destination/report.pdf"),
            }],
        );

        assert_eq!(
            coordinator.clipboard().map(|clipboard| clipboard.paths.clone()),
            Some(vec![PathBuf::from("/b/report.pdf")])
        );
    }
}
