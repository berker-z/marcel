//! Everything that changes the filesystem.
//!
//! One rule runs through the whole layer: every fallible check happens before
//! the one syscall that commits a change, that commit never overwrites, and
//! whatever follows a commit can only cost undo, never report the commit as
//! having failed. `local` holds the primitives, `identity` the notion of
//! "still the same object", `journal` what a mutation records about itself,
//! and the rest are the mutations.

pub mod archive;
pub mod conflict;
mod copy;
pub mod delete;
mod history;
pub mod identity;
pub mod journal;
pub mod local;
mod mutations;
pub mod quarantine;
pub mod transfer;
pub mod trash;

#[cfg(test)]
mod tests;

use std::{
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

pub use history::{redo_operation, undo_operation};
pub use journal::{
    CommittedOperation, HistoryDirection, MutationOutcome, OperationJournal, OperationRecord,
};
pub use mutations::{
    create_directory, create_file, create_zip_operation, extract_archive_operation, rename_entry,
    set_mode, validate_entry_name, validate_entry_os_name,
};
pub use quarantine::{
    RECOVERY_REMNANT_PREFIX, boot_id, is_internal_working_name, is_quarantine_from_another_boot,
    process_is_running, reclaim_abandoned_quarantines,
};
pub use transfer::{CompletedTransfer, TransferMode, TransferOutcome};

/// One path that could not be processed, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathFailure {
    pub path: PathBuf,
    pub message: String,
}

impl PathFailure {
    pub fn new(path: &Path, message: impl Into<String>) -> Self {
        Self { path: path.to_path_buf(), message: message.into() }
    }

    /// One sentence for a notification: the only failure, or the first with a
    /// count of the rest.
    pub fn summarize(failures: &[Self], when_empty: &str) -> String {
        match failures {
            [] => when_empty.to_string(),
            [failure] => failure.message.clone(),
            [first, rest @ ..] => format!(
                "{} (and {} more failure{})",
                first.message,
                rest.len(),
                if rest.len() == 1 { "" } else { "s" }
            ),
        }
    }
}

/// What a browser projection has to re-check after a mutation: paths that may
/// have gone, and paths that may have appeared or changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirectoryChanges {
    pub removed: Vec<PathBuf>,
    pub upserted: Vec<PathBuf>,
}

impl DirectoryChanges {
    pub fn removed(removed: Vec<PathBuf>) -> Self {
        Self { removed, upserted: Vec::new() }
    }

    pub fn upserted(upserted: Vec<PathBuf>) -> Self {
        Self { removed: Vec::new(), upserted }
    }

    pub fn reversed(self) -> Self {
        Self { removed: self.upserted, upserted: self.removed }
    }
}

/// Progress a worker thread publishes and a window polls.
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

    pub fn set_preparing(&self, preparing: bool) {
        self.preparing.store(preparing, Ordering::Relaxed);
    }

    pub fn add_total(&self, items: u64, bytes: u64) {
        self.total_items.fetch_add(items, Ordering::Relaxed);
        self.total_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn set_current_path(&self, path: Option<PathBuf>) {
        *self.current_path.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = path;
    }

    pub fn complete_item(&self) {
        self.completed_items.fetch_add(1, Ordering::Relaxed);
    }

    pub fn complete_bytes(&self, bytes: u64) {
        self.completed_bytes.fetch_add(bytes, Ordering::Relaxed);
    }
}
