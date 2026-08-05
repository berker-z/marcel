use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use gpui::Task;

use crate::file_ops::{
    CompletedTransfer, OperationJournal, OperationRecord, TransferMode, TransferProgress,
};

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

#[derive(Default)]
pub struct OperationController {
    journal: OperationJournal,
    clipboard: Option<FileClipboard>,
    busy: bool,
    cancel: Option<Arc<AtomicBool>>,
    task: Option<Task<()>>,
    progress: Option<ActiveOperationProgress>,
    progress_task: Option<Task<()>>,
}

impl OperationController {
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

    /// Drop the sources that actually moved, matched by exact recorded path.
    ///
    /// Reconstructing this from file names conflates same-named sources as
    /// soon as one transfer can span directories, silently dropping a failed
    /// item from the clipboard and making it unretryable through Paste.
    pub fn retain_uncompleted_move(
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

    pub fn begin_simple(&mut self) -> bool {
        if self.busy {
            return false;
        }
        self.busy = true;
        self.cancel = None;
        true
    }

    pub fn begin_transfer(
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

    pub fn begin_permanent_delete(
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

    pub fn begin_archive(
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

    pub fn finish_active(&mut self) {
        self.busy = false;
        self.cancel = None;
        self.progress = None;
        self.progress_task.take();
        self.task.take();
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

    pub fn progress(&self) -> Option<&ActiveOperationProgress> {
        self.progress.as_ref()
    }

    pub fn set_task(&mut self, task: Task<()>) {
        self.task = Some(task);
    }

    pub fn set_progress_task(&mut self, task: Task<()>) {
        self.progress_task = Some(task);
    }

    pub fn record(&mut self, operation: OperationRecord) {
        self.journal.record(operation);
    }

    pub fn begin_undo(&mut self) -> Option<OperationRecord> {
        if !self.begin_simple() {
            return None;
        }
        let operation = self.journal.begin_undo();
        if operation.is_none() {
            self.finish_active();
        }
        operation
    }

    pub fn finish_undo(&mut self, operation: OperationRecord) {
        self.journal.finish_undo(operation);
    }

    pub fn cancel_undo(&mut self, operation: OperationRecord) {
        self.journal.cancel_undo(operation);
    }

    pub fn begin_redo(&mut self) -> Option<OperationRecord> {
        if !self.begin_simple() {
            return None;
        }
        let operation = self.journal.begin_redo();
        if operation.is_none() {
            self.finish_active();
        }
        operation
    }

    pub fn finish_redo(&mut self, operation: OperationRecord) {
        self.journal.finish_redo(operation);
    }

    pub fn cancel_redo(&mut self, operation: OperationRecord) {
        self.journal.cancel_redo(operation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_ops::create_directory;

    #[test]
    fn busy_and_cancel_transitions_are_explicit() {
        let mut controller = OperationController::default();
        assert!(
            controller
                .begin_transfer(TransferMode::Copy, 1, PathBuf::from("/destination"))
                .is_some()
        );
        assert!(controller.is_busy());
        assert!(controller.request_cancel());
        assert!(controller.is_cancelling());

        controller.finish_active();
        assert!(!controller.is_busy());
        assert!(!controller.is_cancelling());
    }

    #[test]
    fn undo_and_redo_reserve_the_controller_until_finished() {
        let root = tempfile::tempdir().unwrap();
        let operation = create_directory(root.path(), "created")
            .unwrap()
            .into_record()
            .expect("creating a directory retains undo");
        let mut controller = OperationController::default();
        controller.record(operation.clone());

        assert_eq!(controller.begin_undo(), Some(operation.clone()));
        assert!(controller.is_busy());
        controller.finish_undo(operation.clone());
        controller.finish_active();

        assert_eq!(controller.begin_redo(), Some(operation));
        assert!(controller.is_busy());
    }

    #[test]
    fn completed_cut_items_leave_the_clipboard_and_failures_remain() {
        let mut controller = OperationController::default();
        let clipboard = FileClipboard {
            mode: TransferMode::Move,
            paths: vec![PathBuf::from("/source/a"), PathBuf::from("/source/b")],
        };

        controller.retain_uncompleted_move(
            clipboard,
            &[CompletedTransfer {
                source: PathBuf::from("/source/a"),
                destination: PathBuf::from("/destination/a"),
            }],
        );

        assert_eq!(
            controller
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
        let mut controller = OperationController::default();
        let clipboard = FileClipboard {
            mode: TransferMode::Move,
            paths: vec![
                PathBuf::from("/a/report.pdf"),
                PathBuf::from("/b/report.pdf"),
            ],
        };

        controller.retain_uncompleted_move(
            clipboard,
            &[CompletedTransfer {
                source: PathBuf::from("/a/report.pdf"),
                destination: PathBuf::from("/destination/report.pdf"),
            }],
        );

        assert_eq!(
            controller
                .clipboard()
                .map(|clipboard| clipboard.paths.clone()),
            Some(vec![PathBuf::from("/b/report.pdf")])
        );
    }
}
