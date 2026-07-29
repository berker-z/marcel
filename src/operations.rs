use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use gpui::Task;

use crate::file_ops::{OperationJournal, OperationRecord, TransferMode, TransferProgress};

#[derive(Clone, Debug)]
pub struct FileClipboard {
    pub mode: TransferMode,
    pub paths: Vec<PathBuf>,
}

pub struct ActiveTransferProgress {
    pub mode: TransferMode,
    pub source_count: usize,
    pub destination: PathBuf,
    pub progress: Arc<TransferProgress>,
}

#[derive(Default)]
pub struct OperationController {
    journal: OperationJournal,
    clipboard: Option<FileClipboard>,
    busy: bool,
    cancel: Option<Arc<AtomicBool>>,
    task: Option<Task<()>>,
    progress: Option<ActiveTransferProgress>,
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

    pub fn retain_uncompleted_move(
        &mut self,
        clipboard: FileClipboard,
        completed_destinations: &[PathBuf],
    ) {
        let completed_sources = clipboard
            .paths
            .iter()
            .filter(|source| {
                source.file_name().is_some_and(|name| {
                    completed_destinations
                        .iter()
                        .any(|path| path.file_name() == Some(name))
                })
            })
            .cloned()
            .collect::<HashSet<_>>();
        let remaining = clipboard
            .paths
            .into_iter()
            .filter(|path| !completed_sources.contains(path))
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
        self.progress = Some(ActiveTransferProgress {
            mode,
            source_count,
            destination,
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

    pub fn progress(&self) -> Option<&ActiveTransferProgress> {
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
        let operation = create_directory(root.path(), "created").unwrap();
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

        controller.retain_uncompleted_move(clipboard, &[PathBuf::from("/destination/a")]);

        assert_eq!(
            controller
                .clipboard()
                .map(|clipboard| clipboard.paths.clone()),
            Some(vec![PathBuf::from("/source/b")])
        );
    }
}
