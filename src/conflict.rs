//! Destination-conflict decisions.
//!
//! Marcel refuses an occupied destination unless the user says otherwise. This
//! module carries that "otherwise": the question an operation asks, the answer
//! it gets back, and the per-operation state that lets one answer stand in for
//! many.
//!
//! The shape follows Nautilus, which has the mature model
//! (`nautilus-file-operations.c`, the conflict branch of `copy_move_file`):
//! four responses, an apply-to-all flag on each, and three *independent* sticky
//! flags, because replacing everything and merging everything are different
//! intentions that must not collapse into one. Yazi has nothing to adopt here —
//! its conflict handling is a `force` boolean chosen before the operation runs,
//! which either overwrites or silently renames to a unique name.
//!
//! Marcel diverges from Nautilus on recoverability. Nautilus cannot restore
//! what a replace destroyed, so accepting a replace there ends that data's
//! reversibility. Marcel treats a replacement as something it must be able to
//! undo, or must report as not undoable.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
};

/// What an operation found in its way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictRequest {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub source_is_directory: bool,
    pub destination_is_directory: bool,
}

impl ConflictRequest {
    /// Whether replacing would mean merging two directories rather than
    /// replacing one object with another.
    ///
    /// Nautilus does not offer merge as a separate response; it is the replace
    /// response, relabelled when both sides are directories. Marcel keeps that,
    /// because a user choosing "replace all" for files has not agreed to merge
    /// directory trees.
    pub fn is_merge(&self) -> bool {
        self.source_is_directory && self.destination_is_directory
    }
}

/// One answer to one conflict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConflictResponse {
    /// Leave both items alone and continue with the next source.
    Skip,
    /// Replace the destination, or merge into it when both are directories.
    Replace,
    /// Retry against a different name in the same destination directory.
    Rename(OsString),
    /// Abandon the whole operation.
    Cancel,
}

/// A response plus whether it stands for every later conflict of its kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictDecision {
    pub response: ConflictResponse,
    pub apply_to_all: bool,
}

impl ConflictDecision {
    pub fn once(response: ConflictResponse) -> Self {
        Self {
            response,
            apply_to_all: false,
        }
    }

    pub fn for_all(response: ConflictResponse) -> Self {
        Self {
            response,
            apply_to_all: true,
        }
    }
}

/// Something that can answer a conflict, normally by asking the user.
///
/// Implementations block the calling thread until an answer arrives. That is
/// safe and intended: operations run on blocking-pool threads, and Marcel's
/// rule is that filesystem work stays off GPUI's foreground executor, not that
/// it never waits. An implementation that cannot reach a user must return
/// promptly rather than parking the worker.
pub trait ConflictResolver: Send + Sync {
    fn resolve(&self, request: &ConflictRequest) -> ConflictDecision;
}

/// The conflict state of one operation.
///
/// Sticky answers live here and nowhere else: they are scoped to a single
/// operation, never persisted, and never inferred from a previous one.
pub struct ConflictPolicy {
    resolver: Option<Arc<dyn ConflictResolver>>,
    skip_all: bool,
    replace_all: bool,
    merge_all: bool,
    cancelled: bool,
}

impl std::fmt::Debug for ConflictPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConflictPolicy")
            .field("interactive", &self.resolver.is_some())
            .field("skip_all", &self.skip_all)
            .field("replace_all", &self.replace_all)
            .field("merge_all", &self.merge_all)
            .field("cancelled", &self.cancelled)
            .finish()
    }
}

impl Default for ConflictPolicy {
    fn default() -> Self {
        Self::refusing()
    }
}

impl ConflictPolicy {
    /// Refuse every conflict without asking.
    ///
    /// This is the default and the fallback. It is what an operation gets when
    /// no user interface can answer — a closed window, a D-Bus request, a
    /// test — and it reproduces Marcel's original no-overwrite behavior
    /// exactly. A conflict must never block a worker on an answer that cannot
    /// arrive.
    pub fn refusing() -> Self {
        Self {
            resolver: None,
            skip_all: false,
            replace_all: false,
            merge_all: false,
            cancelled: false,
        }
    }

    pub fn interactive(resolver: Arc<dyn ConflictResolver>) -> Self {
        Self {
            resolver: Some(resolver),
            ..Self::refusing()
        }
    }

    /// Whether a conflict can be answered by anything other than refusal.
    pub fn is_interactive(&self) -> bool {
        self.resolver.is_some()
    }

    /// Whether the user abandoned the operation from a conflict.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// Answer one conflict, consulting sticky state before asking again.
    pub fn decide(&mut self, request: &ConflictRequest) -> ConflictResponse {
        if self.cancelled {
            return ConflictResponse::Cancel;
        }
        // A standing replace or merge answer applies only to its own kind of
        // conflict.
        if (request.is_merge() && self.merge_all) || (!request.is_merge() && self.replace_all) {
            return ConflictResponse::Replace;
        }
        if self.skip_all {
            return ConflictResponse::Skip;
        }
        let Some(resolver) = self.resolver.clone() else {
            return ConflictResponse::Skip;
        };

        let decision = resolver.resolve(request);
        if decision.apply_to_all {
            match decision.response {
                ConflictResponse::Skip => self.skip_all = true,
                ConflictResponse::Replace if request.is_merge() => self.merge_all = true,
                ConflictResponse::Replace => self.replace_all = true,
                // A chosen name cannot stand in for later conflicts, and
                // cancelling already ends the operation.
                ConflictResponse::Rename(_) | ConflictResponse::Cancel => {}
            }
        }
        if decision.response == ConflictResponse::Cancel {
            self.cancelled = true;
        }
        decision.response
    }
}

/// Describe an existing object for a conflict request, or `None` when the path
/// is free.
///
/// Symbolic links count as occupying their path and are never followed: the
/// question is whether *this name* is taken, not what it points at.
pub fn describe_occupant(path: &Path) -> std::io::Result<Option<bool>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata.file_type().is_dir())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A resolver that returns scripted answers and records what it was asked.
    struct Scripted {
        answers: Mutex<Vec<ConflictDecision>>,
        asked: Mutex<Vec<ConflictRequest>>,
    }

    impl Scripted {
        fn new(answers: Vec<ConflictDecision>) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(answers.into_iter().rev().collect()),
                asked: Mutex::new(Vec::new()),
            })
        }

        fn asked(&self) -> usize {
            self.asked.lock().unwrap().len()
        }
    }

    impl ConflictResolver for Scripted {
        fn resolve(&self, request: &ConflictRequest) -> ConflictDecision {
            self.asked.lock().unwrap().push(request.clone());
            self.answers
                .lock()
                .unwrap()
                .pop()
                .expect("the policy asked more times than the test scripted")
        }
    }

    fn request(source_is_directory: bool, destination_is_directory: bool) -> ConflictRequest {
        ConflictRequest {
            source: PathBuf::from("/source/item"),
            destination: PathBuf::from("/destination/item"),
            source_is_directory,
            destination_is_directory,
        }
    }

    /// The default must reproduce Marcel's original behavior exactly, so that
    /// every operation that has not opted in still refuses to overwrite.
    #[test]
    fn the_default_policy_refuses_without_asking() {
        let mut policy = ConflictPolicy::refusing();

        assert!(!policy.is_interactive());
        assert_eq!(
            policy.decide(&request(false, false)),
            ConflictResponse::Skip
        );
        assert_eq!(policy.decide(&request(true, true)), ConflictResponse::Skip);
        assert!(!policy.is_cancelled());
    }

    #[test]
    fn a_single_answer_applies_only_to_its_own_conflict() {
        let resolver = Scripted::new(vec![
            ConflictDecision::once(ConflictResponse::Replace),
            ConflictDecision::once(ConflictResponse::Skip),
        ]);
        let mut policy = ConflictPolicy::interactive(resolver.clone());

        assert_eq!(
            policy.decide(&request(false, false)),
            ConflictResponse::Replace
        );
        assert_eq!(
            policy.decide(&request(false, false)),
            ConflictResponse::Skip
        );
        assert_eq!(
            resolver.asked(),
            2,
            "each conflict must be asked separately"
        );
    }

    /// Replacing files and merging directories are different intentions.
    /// Collapsing them would let "replace all" for a pile of files silently
    /// merge a directory tree the user never looked at.
    #[test]
    fn replace_all_does_not_imply_merge_all() {
        let resolver = Scripted::new(vec![
            ConflictDecision::for_all(ConflictResponse::Replace),
            ConflictDecision::once(ConflictResponse::Skip),
        ]);
        let mut policy = ConflictPolicy::interactive(resolver.clone());

        // Replace-all, answered for a file conflict.
        assert_eq!(
            policy.decide(&request(false, false)),
            ConflictResponse::Replace
        );
        // Later file conflicts are answered from sticky state.
        assert_eq!(
            policy.decide(&request(false, false)),
            ConflictResponse::Replace
        );
        assert_eq!(resolver.asked(), 1);

        // A directory-into-directory conflict is a merge, so it must be asked.
        assert_eq!(policy.decide(&request(true, true)), ConflictResponse::Skip);
        assert_eq!(resolver.asked(), 2);
    }

    #[test]
    fn merge_all_does_not_imply_replace_all() {
        let resolver = Scripted::new(vec![
            ConflictDecision::for_all(ConflictResponse::Replace),
            ConflictDecision::once(ConflictResponse::Skip),
        ]);
        let mut policy = ConflictPolicy::interactive(resolver.clone());

        assert_eq!(
            policy.decide(&request(true, true)),
            ConflictResponse::Replace
        );
        assert_eq!(
            policy.decide(&request(true, true)),
            ConflictResponse::Replace
        );
        assert_eq!(resolver.asked(), 1);

        assert_eq!(
            policy.decide(&request(false, false)),
            ConflictResponse::Skip
        );
        assert_eq!(resolver.asked(), 2);
    }

    #[test]
    fn skip_all_answers_every_later_conflict_of_any_kind() {
        let resolver = Scripted::new(vec![ConflictDecision::for_all(ConflictResponse::Skip)]);
        let mut policy = ConflictPolicy::interactive(resolver.clone());

        assert_eq!(
            policy.decide(&request(false, false)),
            ConflictResponse::Skip
        );
        assert_eq!(policy.decide(&request(true, true)), ConflictResponse::Skip);
        assert_eq!(policy.decide(&request(false, true)), ConflictResponse::Skip);
        assert_eq!(resolver.asked(), 1);
    }

    /// A standing replace answer must not survive a cancellation.
    #[test]
    fn cancelling_ends_the_operation_and_overrides_sticky_answers() {
        let resolver = Scripted::new(vec![
            ConflictDecision::for_all(ConflictResponse::Replace),
            ConflictDecision::once(ConflictResponse::Cancel),
        ]);
        let mut policy = ConflictPolicy::interactive(resolver.clone());

        assert_eq!(
            policy.decide(&request(true, true)),
            ConflictResponse::Replace
        );
        assert_eq!(
            policy.decide(&request(false, false)),
            ConflictResponse::Cancel
        );
        assert!(policy.is_cancelled());

        // Even a conflict covered by merge-all is refused once cancelled.
        assert_eq!(
            policy.decide(&request(true, true)),
            ConflictResponse::Cancel
        );
        assert_eq!(resolver.asked(), 2, "nothing is asked after a cancellation");
    }

    /// A chosen name answers exactly one conflict. Applying it to all would
    /// mean writing several different sources to one name.
    #[test]
    fn a_rename_never_becomes_a_standing_answer() {
        let resolver = Scripted::new(vec![
            ConflictDecision::for_all(ConflictResponse::Rename(OsString::from("copy.txt"))),
            ConflictDecision::once(ConflictResponse::Skip),
        ]);
        let mut policy = ConflictPolicy::interactive(resolver.clone());

        assert_eq!(
            policy.decide(&request(false, false)),
            ConflictResponse::Rename(OsString::from("copy.txt"))
        );
        assert_eq!(
            policy.decide(&request(false, false)),
            ConflictResponse::Skip
        );
        assert_eq!(resolver.asked(), 2);
    }

    #[test]
    fn a_symlink_occupies_its_path_without_being_followed() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("real");
        let link = root.path().join("link");
        std::fs::create_dir(&directory).unwrap();
        std::os::unix::fs::symlink(&directory, &link).unwrap();

        assert_eq!(describe_occupant(&directory).unwrap(), Some(true));
        // The link points at a directory but is not one, so replacing it is
        // not a merge.
        assert_eq!(describe_occupant(&link).unwrap(), Some(false));
        assert_eq!(describe_occupant(&root.path().join("free")).unwrap(), None);
    }
}
