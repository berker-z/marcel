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
    ffi::{OsStr, OsString},
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
    /// Retry against a name the user typed.
    Rename(OsString),
    /// Keep both, under a name Marcel picks.
    ///
    /// This is the only rename that can stand for many conflicts, because a
    /// typed name cannot: fifty sources cannot share one name. Nautilus offers
    /// no bulk rename at all, and Yazi's automatic unique-naming is a silent
    /// default rather than a choice. Offering it as an explicit answer keeps
    /// Yazi's convenience without its surprise.
    AutoRename,
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
    rename_all: bool,
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
            .field("rename_all", &self.rename_all)
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
            rename_all: false,
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
        if self.rename_all {
            return ConflictResponse::AutoRename;
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
                ConflictResponse::AutoRename => self.rename_all = true,
                // A typed name cannot stand in for later conflicts, and
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

/// A conflict waiting for an answer, and the channel to answer it on.
///
/// Dropping this without answering releases the operation rather than stranding
/// it, so a dialog dismissed by any route the interface offers still ends the
/// wait.
pub struct PendingConflict {
    request: ConflictRequest,
    reply: std::sync::mpsc::SyncSender<ConflictDecision>,
}

impl PendingConflict {
    pub fn request(&self) -> &ConflictRequest {
        &self.request
    }

    pub fn answer(self, decision: ConflictDecision) {
        // The operation may have been abandoned while the question was on
        // screen, which is not an error.
        let _ = self.reply.send(decision);
    }
}

/// A resolver that asks a user interface and waits for the answer.
///
/// This is the shape Nautilus uses — its operation threads block on a condition
/// variable while the dialog runs on the main thread, noted directly in its
/// source. Marcel's transfers already run on blocking-pool threads, so waiting
/// here costs nothing the interface can feel.
pub struct PromptingResolver {
    requests: async_channel::Sender<PendingConflict>,
}

impl PromptingResolver {
    /// Build a resolver and the stream of questions it will ask.
    pub fn new() -> (Arc<Self>, async_channel::Receiver<PendingConflict>) {
        let (requests, questions) = async_channel::unbounded();
        (Arc::new(Self { requests }), questions)
    }
}

impl ConflictResolver for PromptingResolver {
    fn resolve(&self, request: &ConflictRequest) -> ConflictDecision {
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        let pending = PendingConflict {
            request: request.clone(),
            reply,
        };
        // Nobody is listening, or nobody answered. Cancelling is the honest
        // outcome: it stops the operation and accounts for every source it did
        // not reach, where skipping would quietly do nothing to each in turn.
        if self.requests.send_blocking(pending).is_err() {
            return ConflictDecision::once(ConflictResponse::Cancel);
        }
        answer
            .recv()
            .unwrap_or_else(|_| ConflictDecision::once(ConflictResponse::Cancel))
    }
}

/// The longest name most Linux filesystems accept, in bytes.
const MAX_NAME_BYTES: usize = 255;

/// How many names a search will try before giving up.
const MAX_UNIQUE_ATTEMPTS: usize = 10_000;

/// Where a name's extension starts, as a byte offset.
///
/// The rules are Nautilus's (`nautilus_filename_get_extension`), which are
/// better tested than anything invented here:
///
/// - a leading dot never starts an extension, so `.bashrc` has none;
/// - the *last* dot wins, so `photo.backup.png` keeps `.png`;
/// - a trailing dot is not an extension;
/// - an extension containing whitespace is not treated as one, because
///   `report.final draft` is a name, not a file type;
/// - `.tar` is folded into the extension, so `archive.tar.gz` stays intact
///   rather than becoming `archive.tar (2).gz`.
fn extension_offset(name: &[u8]) -> usize {
    // Skipping the first byte protects dotfiles. A `.` can never appear inside
    // a multi-byte UTF-8 sequence, so this cannot split a character.
    let Some(dot) = name
        .iter()
        .rposition(|byte| *byte == b'.')
        .filter(|dot| *dot >= 1)
    else {
        return name.len();
    };
    if dot + 1 == name.len() {
        return name.len();
    }
    if name[dot..].iter().any(u8::is_ascii_whitespace) {
        return name.len();
    }
    const TAR: &[u8] = b".tar";
    if dot >= TAR.len() && &name[dot - TAR.len()..dot] == TAR && dot - TAR.len() >= 1 {
        return dot - TAR.len();
    }
    dot
}

/// Split a stem already carrying a ` (N)` suffix into its base and that number.
///
/// Parsing rather than nesting is the whole point: a second conflict on
/// `report (2).txt` produces `report (3).txt`, never `report (2) (2).txt`.
fn parse_existing_count(stem: &[u8]) -> (usize, usize) {
    let full = stem.len();
    if stem.last() != Some(&b')') {
        return (full, 0);
    }
    let Some(open) = stem.iter().rposition(|byte| *byte == b'(') else {
        return (full, 0);
    };
    // The marker Marcel writes is exactly " (" — anything else is part of the
    // user's own name and must be preserved.
    if open < 1 || stem[open - 1] != b' ' {
        return (full, 0);
    }
    let digits = &stem[open + 1..full - 1];
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return (full, 0);
    }
    // A leading zero is not something Marcel writes, so treat it as the user's.
    if digits[0] == b'0' {
        return (full, 0);
    }
    match std::str::from_utf8(digits)
        .ok()
        .and_then(|d| d.parse().ok())
    {
        Some(count) => (open - 1, count),
        None => (full, 0),
    }
}

/// Build the `count`-th alternative name for `name`.
///
/// Numbering starts at 2, matching Nautilus's explicit choice: the item already
/// on disk is implicitly the first, so its neighbour is the second. `(1)` would
/// suggest the original is somehow the zeroth.
pub fn conflict_name(name: &OsStr, count: usize, is_directory: bool) -> OsString {
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

    let bytes = name.as_bytes();
    // A directory named `backup.2024` has no file type to preserve.
    let split = if is_directory {
        bytes.len()
    } else {
        extension_offset(bytes)
    };
    let (stem, extension) = bytes.split_at(split);
    let (base_length, existing) = parse_existing_count(stem);
    let suffix = format!(" ({})", (existing + count).max(2)).into_bytes();

    let mut base = stem[..base_length].to_vec();
    // Keep the whole name within one directory entry, trimming the base rather
    // than the suffix or the extension, which carry the meaning.
    let budget = MAX_NAME_BYTES.saturating_sub(suffix.len() + extension.len());
    if base.len() > budget {
        base.truncate(floor_char_boundary(&base, budget));
    }

    let mut result = base;
    result.extend_from_slice(&suffix);
    result.extend_from_slice(extension);
    OsString::from_vec(result)
}

/// Trim to `limit` without splitting a UTF-8 character.
fn floor_char_boundary(bytes: &[u8], limit: usize) -> usize {
    let mut end = limit.min(bytes.len());
    while end > 0 && (bytes[end] & 0xC0) == 0x80 {
        end -= 1;
    }
    end
}

/// Find a free name for `name` in `directory`.
///
/// Returns `None` when no free name could be found, which the caller reports
/// rather than looping.
pub fn unique_name_in(directory: &Path, name: &OsStr, is_directory: bool) -> Option<OsString> {
    for count in 1..=MAX_UNIQUE_ATTEMPTS {
        let candidate = conflict_name(name, count, is_directory);
        if matches!(describe_occupant(&directory.join(&candidate)), Ok(None)) {
            return Some(candidate);
        }
    }
    None
}

/// What already occupies a destination path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Occupant {
    pub is_directory: bool,
    /// Device and inode, so an occupant can be recognized as the very object
    /// being transferred even under a different name.
    pub object: (u64, u64),
}

impl Occupant {
    /// Whether this is the same filesystem object as `metadata` describes.
    ///
    /// Nautilus answers the same question by comparing paths — `test_dir_is_parent`
    /// walks parents with `g_file_equal`. That catches a file copied over its
    /// own path, but not a hard link or an aliased path that names the same
    /// inode. Marcel already tracks device and inode everywhere else, so it
    /// compares the object rather than the name.
    pub fn is_same_object_as(&self, metadata: &std::fs::Metadata) -> bool {
        use std::os::unix::fs::MetadataExt as _;

        self.object == (metadata.dev(), metadata.ino())
    }
}

/// Describe an existing object for a conflict request, or `None` when the path
/// is free.
///
/// Symbolic links count as occupying their path and are never followed: the
/// question is whether *this name* is taken, not what it points at.
pub fn describe_occupant(path: &Path) -> std::io::Result<Option<Occupant>> {
    use std::os::unix::fs::MetadataExt as _;

    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(Occupant {
            is_directory: metadata.file_type().is_dir(),
            object: (metadata.dev(), metadata.ino()),
        })),
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

        let directory_occupant = describe_occupant(&directory).unwrap().unwrap();
        let link_occupant = describe_occupant(&link).unwrap().unwrap();

        assert!(directory_occupant.is_directory);
        // The link points at a directory but is not one, so replacing it is
        // not a merge, and it is a different object from its target.
        assert!(!link_occupant.is_directory);
        assert_ne!(directory_occupant.object, link_occupant.object);
        assert!(
            describe_occupant(&root.path().join("free"))
                .unwrap()
                .is_none()
        );
    }

    fn named(name: &str, count: usize) -> String {
        conflict_name(OsStr::new(name), count, false)
            .into_string()
            .unwrap()
    }

    /// Numbering starts at 2 because the item already on disk is implicitly the
    /// first. Nautilus makes the same choice explicitly.
    #[test]
    fn alternative_names_count_from_two_and_keep_the_extension() {
        assert_eq!(named("report.txt", 1), "report (2).txt");
        assert_eq!(named("report.txt", 2), "report (2).txt");
        assert_eq!(named("report.txt", 3), "report (3).txt");
        assert_eq!(named("README", 1), "README (2)");
    }

    /// The whole point of parsing an existing suffix: a second conflict
    /// increments rather than nesting, so names cannot grow without bound.
    #[test]
    fn an_existing_suffix_is_incremented_rather_than_nested() {
        assert_eq!(named("report (2).txt", 1), "report (3).txt");
        assert_eq!(named("report (9).txt", 1), "report (10).txt");
        // Not Marcel's marker, so it belongs to the user's own name.
        assert_eq!(named("report(2).txt", 1), "report(2) (2).txt");
        assert_eq!(named("report (draft).txt", 1), "report (draft) (2).txt");
        assert_eq!(named("report (02).txt", 1), "report (02) (2).txt");
    }

    #[test]
    fn extension_detection_matches_the_documented_rules() {
        // A leading dot is not an extension, so dotfiles keep their name.
        assert_eq!(named(".bashrc", 1), ".bashrc (2)");
        // The last dot wins.
        assert_eq!(named("photo.backup.png", 1), "photo.backup (2).png");
        // `.tar` is folded in so the archive type survives intact.
        assert_eq!(named("archive.tar.gz", 1), "archive (2).tar.gz");
        // A trailing dot is not an extension.
        assert_eq!(named("weird.", 1), "weird. (2)");
        // Whitespace means this is a name, not a file type.
        assert_eq!(named("report.final draft", 1), "report.final draft (2)");
    }

    /// A directory named `backup.2024` has no file type to preserve, so the
    /// suffix belongs at the end.
    #[test]
    fn directories_keep_their_whole_name_before_the_suffix() {
        assert_eq!(
            conflict_name(OsStr::new("backup.2024"), 1, true)
                .into_string()
                .unwrap(),
            "backup.2024 (2)"
        );
    }

    /// One directory entry cannot exceed 255 bytes, and the suffix and
    /// extension carry the meaning, so the base is what gives way.
    #[test]
    fn a_long_name_stays_within_one_directory_entry() {
        use std::os::unix::ffi::OsStrExt as _;

        let long = format!("{}.txt", "n".repeat(300));
        let renamed = conflict_name(OsStr::new(&long), 1, false);

        assert!(renamed.as_bytes().len() <= MAX_NAME_BYTES);
        assert!(renamed.to_string_lossy().ends_with(" (2).txt"));
    }

    /// Non-UTF-8 names are authoritative in Marcel, so renaming one must not
    /// corrupt it or split a character when trimming.
    #[test]
    fn a_non_utf8_name_survives_renaming() {
        use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

        let raw = OsString::from_vec(vec![b'n', 0xff, b'.', b't', b'x', b't']);
        let renamed = conflict_name(&raw, 1, false);

        assert_eq!(renamed.as_bytes(), b"n\xff (2).txt");
    }

    #[test]
    fn a_free_name_is_found_past_every_occupied_candidate() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("report.txt"), b"original").unwrap();
        std::fs::write(root.path().join("report (2).txt"), b"second").unwrap();
        std::fs::write(root.path().join("report (3).txt"), b"third").unwrap();

        let name = unique_name_in(root.path(), OsStr::new("report.txt"), false).unwrap();

        assert_eq!(name, OsString::from("report (4).txt"));
    }

    /// Renaming everything is the one rename that can stand for many
    /// conflicts, because Marcel chooses the names rather than the user.
    #[test]
    fn rename_all_becomes_a_standing_answer_but_a_typed_name_does_not() {
        let resolver = Scripted::new(vec![ConflictDecision::for_all(
            ConflictResponse::AutoRename,
        )]);
        let mut policy = ConflictPolicy::interactive(resolver.clone());

        assert_eq!(
            policy.decide(&request(false, false)),
            ConflictResponse::AutoRename
        );
        assert_eq!(
            policy.decide(&request(true, true)),
            ConflictResponse::AutoRename
        );
        assert_eq!(resolver.asked(), 1);
    }

    /// A worker must never park on an answer that cannot arrive. Losing the
    /// interface cancels the operation, which accounts for every source it did
    /// not reach, rather than skipping each one silently.
    #[test]
    fn losing_the_interface_cancels_instead_of_waiting() {
        let (resolver, questions) = PromptingResolver::new();
        drop(questions);

        let decision = resolver.resolve(&request(false, false));

        assert_eq!(decision.response, ConflictResponse::Cancel);
    }

    #[test]
    fn an_unanswered_question_cancels_rather_than_stranding_the_worker() {
        let (resolver, questions) = PromptingResolver::new();
        let asking = std::thread::spawn(move || resolver.resolve(&request(false, false)));

        // Take the question and drop it, as a dismissed dialog would.
        let pending = questions.recv_blocking().unwrap();
        drop(pending);

        assert_eq!(asking.join().unwrap().response, ConflictResponse::Cancel);
    }

    #[test]
    fn an_answered_question_reaches_the_waiting_worker() {
        let (resolver, questions) = PromptingResolver::new();
        let asking = std::thread::spawn(move || resolver.resolve(&request(false, false)));

        let pending = questions.recv_blocking().unwrap();
        assert_eq!(
            pending.request().destination,
            PathBuf::from("/destination/item")
        );
        pending.answer(ConflictDecision::for_all(ConflictResponse::Replace));

        let decision = asking.join().unwrap();
        assert_eq!(decision.response, ConflictResponse::Replace);
        assert!(decision.apply_to_all);
    }

    #[test]
    fn a_hardlink_is_the_same_object_as_the_file_it_links() {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("original");
        let link = root.path().join("hardlink");
        let other = root.path().join("other");
        std::fs::write(&original, b"payload").unwrap();
        std::fs::hard_link(&original, &link).unwrap();
        std::fs::write(&other, b"payload").unwrap();

        let original_metadata = std::fs::symlink_metadata(&original).unwrap();
        let link_occupant = describe_occupant(&link).unwrap().unwrap();
        let other_occupant = describe_occupant(&other).unwrap().unwrap();

        assert!(link_occupant.is_same_object_as(&original_metadata));
        // Identical content is not the same object, so byte equality is not
        // what this question is asking.
        assert!(!other_occupant.is_same_object_as(&original_metadata));
    }
}
