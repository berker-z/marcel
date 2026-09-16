use super::{
    conflict::{ConflictPolicy, ConflictRequest, ConflictResponse},
    copy::{supported_xattr_name, xattrs_unsupported},
    identity::FileIdentity,
    journal::*,
    local::{MAX_NAME_BYTES, fault, quarantined_name},
    mutations::*,
    quarantine::*,
    transfer::*,
    *,
};
use std::time::{Duration, UNIX_EPOCH};
use std::{
    ffi::OsStr,
    fs,
    io::{self, Read as _, Seek as _, Write as _},
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
};

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

/// Snapshotting after the rename turned any directory containing a socket
/// into a phantom failure: the move had happened, but the caller was told
/// it had not.
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
    // A rename never inspects what the tree holds, so the socket costs
    // nothing: the move succeeds *and* stays undoable.
    assert!(!outcome.undo_unavailable);
    assert!(outcome.operation.is_some());
}

/// A compensating rollback renames each root a second time, which bumps its
/// ctime and invalidates the identities in the record that produced the
/// attempt. Reinserting that record made every later Undo fail with
/// "changed or was replaced" — blaming the user for Marcel's own recovery.
///
/// Nautilus discards its undo record whenever an undo fails
/// (`nautilus-file-undo-manager.c`, `undo_info_apply_ready`). Marcel does
/// the same past the commit point, and only there.
#[test]
fn a_rolled_back_undo_discards_the_record_it_invalidated() {
    use std::os::unix::fs::PermissionsExt as _;

    if rustix::process::geteuid().is_root() {
        // Permission bits do not constrain root, so the mid-loop failure
        // this test depends on cannot be provoked.
        return;
    }

    let root = tempfile::tempdir().unwrap();
    // Two source parents, so one can be sealed without blocking the other.
    // `undo_operation` validates every transfer before renaming any, so an
    // obstacle it can see up front yields `Unchanged`; reaching the
    // rolled-back path needs a failure only the rename itself discovers.
    let blocked = root.path().join("blocked");
    let open = root.path().join("open");
    let destination = root.path().join("destination");
    for directory in [&blocked, &open, &destination] {
        fs::create_dir(directory).unwrap();
    }
    fs::create_dir(blocked.join("first")).unwrap();
    fs::write(blocked.join("first/data.txt"), b"first").unwrap();
    fs::create_dir(open.join("second")).unwrap();
    fs::write(open.join("second/data.txt"), b"second").unwrap();

    let outcome = transfer_paths(
        &[blocked.join("first"), open.join("second")],
        &destination,
        TransferMode::Move,
        Arc::new(AtomicBool::new(false)),
    );
    let operation = outcome.operation.expect("the move retains undo");

    // Undo walks transfers in reverse: "second" returns to `open` first and
    // commits, then "first" cannot be created back inside a read-only
    // parent. Stat still succeeds, so the preflight cannot catch it.
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o555)).unwrap();
    let result = undo_operation(&operation);
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
        !result.keeps_history(),
        "a rolled-back undo must discard its record, got {result:?}"
    );
    let MutationOutcome::Discarded { .. } = result else {
        panic!("expected Discarded, got {result:?}");
    };
    // Compensation returned "second" to the destination, so the disk is
    // whole even though the record is gone.
    assert!(destination.join("first/data.txt").exists());
    assert!(destination.join("second/data.txt").exists());
    assert!(!open.join("second").exists());
}

/// The mirror case: a failure that never reached the disk keeps the record,
/// so the user can clear the obstacle and retry. This is where Marcel is
/// deliberately less blunt than Nautilus, which discards either way.
#[test]
fn an_undo_that_never_commits_keeps_its_record() {
    let root = tempfile::tempdir().unwrap();
    let source_dir = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir(&source_dir).unwrap();
    fs::create_dir(&destination).unwrap();
    fs::create_dir(source_dir.join("only")).unwrap();
    fs::write(source_dir.join("only").join("data.txt"), b"payload").unwrap();

    let outcome = transfer_paths(
        std::slice::from_ref(&source_dir.join("only")),
        &destination,
        TransferMode::Move,
        Arc::new(AtomicBool::new(false)),
    );
    let operation = outcome.operation.expect("the move retains undo");
    fs::write(source_dir.join("only"), b"in the way").unwrap();

    let result = undo_operation(&operation);

    assert!(
        result.keeps_history(),
        "a pre-commit refusal must stay retryable, got {result:?}"
    );
    assert!(destination.join("only/data.txt").exists());

    // Clearing the obstacle makes the retained record work.
    fs::remove_file(source_dir.join("only")).unwrap();
    undo_operation(&operation).unwrap();
    assert!(source_dir.join("only/data.txt").exists());
}

/// A move is a rename in both directions, so a tree Marcel could never
/// copy or archive is still fully reversible. Rejecting such trees during
/// snapshotting was a copy concern leaking into move bookkeeping, and it
/// silently cost undo on the whole batch.
#[test]
fn a_moved_tree_holding_a_socket_is_undoable_and_redoable() {
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
    let operation = outcome.operation.expect("a moved socket tree retains undo");

    let redo_record = recorded(undo_operation(&operation).unwrap());
    assert!(
        project.join("daemon.sock").exists(),
        "undo restored the tree"
    );
    assert_eq!(fs::read(project.join("notes.txt")).unwrap(), b"important");
    assert!(!destination.join("project").exists());

    recorded(redo_operation(&redo_record).unwrap());
    assert!(destination.join("project/daemon.sock").exists());
    assert!(!project.exists());
}

/// Recording special files must not leak into paths whose undo deletes the
/// tree: Marcel cannot recreate a socket, so an archive holding one stays
/// success-without-undo rather than gaining an undo that would erase it.
#[test]
fn archive_sources_still_refuse_special_files() {
    use std::os::unix::net::UnixListener;

    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("payload");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("notes.txt"), b"keep").unwrap();
    let _listener = UnixListener::bind(source.join("daemon.sock")).unwrap();

    let error = create_zip_operation(
        std::slice::from_ref(&source),
        &root.path().join("payload.zip"),
        Arc::new(AtomicBool::new(false)),
    )
    .expect_err("an archive cannot carry a socket");

    assert!(error.to_string().contains("Special files"), "{error}");
    assert!(!root.path().join("payload.zip").exists());
}

#[test]
fn undo_refuses_to_delete_a_tree_holding_a_special_file() {
    use std::os::unix::net::UnixListener;

    let root = tempfile::tempdir().unwrap();
    let tree = root.path().join("output");
    fs::create_dir(&tree).unwrap();
    fs::write(tree.join("kept.txt"), b"keep").unwrap();
    let _listener = UnixListener::bind(tree.join("daemon.sock")).unwrap();

    let snapshots = snapshot_tree(&tree).expect("the rename walker records specials");

    assert!(remove_snapshotted_tree(&snapshots).is_err());
    assert_eq!(fs::read(tree.join("kept.txt")).unwrap(), b"keep");
    assert!(tree.join("daemon.sock").exists());
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
            super::delete::delete_paths(
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
    let _ = journal.record(first);
    let _ = journal.record(second.clone());
    let _ = journal.record(third.clone());

    assert_eq!(journal.begin(HistoryDirection::Undo), Some(third.clone()));
    assert_eq!(journal.begin(HistoryDirection::Undo), Some(second.clone()));
    assert_eq!(journal.begin(HistoryDirection::Undo), None);

    journal.finish(HistoryDirection::Undo, second.clone());
    assert!(journal.can_redo());
    journal.cancel(HistoryDirection::Undo, third);
    let _ = journal.record(second);
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

/// A resolver that answers every conflict the same way.
struct AlwaysAnswers(super::conflict::ConflictDecision);

impl super::conflict::ConflictResolver for AlwaysAnswers {
    fn resolve(&self, _request: &ConflictRequest) -> super::conflict::ConflictDecision {
        self.0.clone()
    }
}

fn answering(decision: super::conflict::ConflictDecision) -> ConflictPolicy {
    ConflictPolicy::interactive(Arc::new(AlwaysAnswers(decision)))
}

fn occupied_transfer_fixture() -> (tempfile::TempDir, Vec<PathBuf>, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let source_dir = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir(&source_dir).unwrap();
    fs::create_dir(&destination).unwrap();
    fs::write(source_dir.join("taken.txt"), b"new").unwrap();
    fs::write(source_dir.join("free.txt"), b"free").unwrap();
    fs::write(destination.join("taken.txt"), b"keep").unwrap();
    let sources = vec![source_dir.join("taken.txt"), source_dir.join("free.txt")];
    (root, sources, destination)
}

/// Skipping is a deliberate outcome, not a failure, and it must not stop
/// the sources that follow it.
#[test]
fn a_skipped_conflict_leaves_both_items_and_continues() {
    let (_root, sources, destination) = occupied_transfer_fixture();
    let mut policy = answering(super::conflict::ConflictDecision::once(
        ConflictResponse::Skip,
    ));

    let outcome = transfer_paths_with_conflicts(
        &sources,
        &destination,
        TransferMode::Copy,
        Arc::new(AtomicBool::new(false)),
        Arc::new(TransferProgress::default()),
        &mut policy,
    );

    assert_eq!(outcome.skipped, [sources[0].clone()]);
    assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
    assert_eq!(outcome.completed.len(), 1);
    assert_eq!(fs::read(destination.join("taken.txt")).unwrap(), b"keep");
    assert_eq!(fs::read(destination.join("free.txt")).unwrap(), b"free");
    assert_eq!(outcome.accounted(), sources.len());
}

/// Cancelling from a conflict abandons the operation, and every source it
/// never reached is accounted for rather than silently forgotten.
#[test]
fn cancelling_a_conflict_accounts_for_every_unattempted_source() {
    let (_root, sources, destination) = occupied_transfer_fixture();
    let mut policy = answering(super::conflict::ConflictDecision::once(
        ConflictResponse::Cancel,
    ));

    let outcome = transfer_paths_with_conflicts(
        &sources,
        &destination,
        TransferMode::Copy,
        Arc::new(AtomicBool::new(false)),
        Arc::new(TransferProgress::default()),
        &mut policy,
    );

    assert_eq!(outcome.cancelled, sources);
    assert!(outcome.completed.is_empty());
    assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
    assert!(!destination.join("free.txt").exists());
    assert_eq!(outcome.accounted(), sources.len());
}

#[test]
fn renaming_resolves_a_conflict_without_touching_the_occupant() {
    let (_root, sources, destination) = occupied_transfer_fixture();
    let mut policy = answering(super::conflict::ConflictDecision::once(
        ConflictResponse::Rename(OsStr::new("renamed.txt").to_os_string()),
    ));

    let outcome = transfer_paths_with_conflicts(
        &sources,
        &destination,
        TransferMode::Copy,
        Arc::new(AtomicBool::new(false)),
        Arc::new(TransferProgress::default()),
        &mut policy,
    );

    assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
    assert_eq!(fs::read(destination.join("taken.txt")).unwrap(), b"keep");
    assert_eq!(fs::read(destination.join("renamed.txt")).unwrap(), b"new");
    assert_eq!(outcome.accounted(), sources.len());
}

/// A chosen name gets the same scrutiny as one typed into Rename, so a
/// resolver cannot smuggle a path separator past the destination directory.
#[test]
fn a_rename_response_cannot_escape_the_destination_directory() {
    let (_root, sources, destination) = occupied_transfer_fixture();
    let mut policy = answering(super::conflict::ConflictDecision::once(
        ConflictResponse::Rename(OsStr::new("../escaped.txt").to_os_string()),
    ));

    let outcome = transfer_paths_with_conflicts(
        &sources,
        &destination,
        TransferMode::Copy,
        Arc::new(AtomicBool::new(false)),
        Arc::new(TransferProgress::default()),
        &mut policy,
    );

    assert_eq!(outcome.failures.len(), 1);
    assert!(
        outcome.failures[0].message.contains("cannot contain"),
        "{:?}",
        outcome.failures
    );
    assert!(!destination.parent().unwrap().join("escaped.txt").exists());
    assert_eq!(outcome.accounted(), sources.len());
}

/// A crash cannot run the exit path, so the remnants it leaves have to be
/// reclaimed later. A dead owner's quarantine can never be restored, which
/// makes it unreachable garbage rather than data anyone might want.
#[test]
fn quarantines_from_dead_processes_are_reclaimed_and_live_ones_are_left() {
    let root = tempfile::tempdir().unwrap();
    // Process id 0 is never a real process, so it stands in for a Marcel
    // that is gone.
    let abandoned = root.path().join(".marcel-replaced-0-0-report.txt");
    let live = root.path().join(format!(
        ".marcel-replaced-{}-0-report.txt",
        std::process::id()
    ));
    let ordinary = root.path().join("report.txt");
    for path in [&abandoned, &live, &ordinary] {
        fs::write(path, b"payload").unwrap();
    }

    let released = reclaim_abandoned_quarantines(root.path());

    assert_eq!(released, 1);
    assert!(!abandoned.exists(), "a dead owner's quarantine is garbage");
    assert!(
        live.exists(),
        "this process can still undo, so its quarantine stays"
    );
    assert!(ordinary.exists(), "user data is never touched");
}

/// A live second Marcel can still restore its own replacements, so its
/// quarantines must survive another instance listing the same directory.
#[test]
fn a_running_owners_quarantine_is_never_reclaimed() {
    let root = tempfile::tempdir().unwrap();
    // The test process itself is a live owner that is not this process id
    // only in the sense that the check must consult liveness, not equality.
    let parent = std::os::unix::process::parent_id();
    let live = root
        .path()
        .join(format!(".marcel-replaced-{parent}-0-report.txt"));
    fs::write(&live, b"payload").unwrap();

    assert_eq!(reclaim_abandoned_quarantines(root.path()), 0);
    assert!(live.exists());
}

/// The defect this pair of names exists to prevent: a transfer that fails
/// after quarantining its destination, whose restoration then also fails,
/// is holding the user's only copy in storage a later Marcel would sweep.
#[test]
fn a_failed_restoration_preserves_the_original_in_recovery_storage() {
    let root = tempfile::tempdir().unwrap();
    let source_dir = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir(&source_dir).unwrap();
    fs::create_dir(&destination).unwrap();
    fs::write(source_dir.join("report.txt"), b"NEW").unwrap();
    fs::write(destination.join("report.txt"), b"ORIGINAL").unwrap();

    // One name, three renames: the quarantine succeeds because it targets a
    // hidden name, then publication and restoration both fail.
    let _fault = fault::fail_renames_to("report.txt");
    let mut policy = answering(super::conflict::ConflictDecision::once(
        ConflictResponse::Replace,
    ));
    let outcome = transfer_paths_with_conflicts(
        std::slice::from_ref(&source_dir.join("report.txt")),
        &destination,
        TransferMode::Copy,
        Arc::new(AtomicBool::new(false)),
        Arc::new(TransferProgress::default()),
        &mut policy,
    );

    assert_eq!(outcome.failures.len(), 1, "{outcome:?}");
    let message = &outcome.failures[0].message;
    assert!(
        message.contains(RECOVERY_REMNANT_PREFIX),
        "the failure says where the data went: {message}"
    );
    assert!(
        no_replacement_quarantines(&destination),
        "nothing may be left in storage a sweep is entitled to reclaim"
    );

    let preserved = fs::read_dir(&destination)
        .unwrap()
        .flatten()
        .filter(|entry| is_recovery_remnant_name(&entry.file_name()))
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    assert_eq!(preserved.len(), 1, "{preserved:?}");
    assert_eq!(fs::read(&preserved[0]).unwrap(), b"ORIGINAL");
}

/// Recovery storage exists precisely because no rule can prove it is
/// unwanted, so the sweep that reclaims abandoned undo storage must not
/// touch it at any process id.
#[test]
fn the_abandoned_sweep_leaves_recovery_remnants_alone() {
    let root = tempfile::tempdir().unwrap();
    let preserved = root.path().join(".marcel-recovered-0-report.txt");
    let abandoned = root.path().join(".marcel-replaced-0-0-report.txt");
    fs::write(&preserved, b"ORIGINAL").unwrap();
    fs::write(&abandoned, b"overwritten").unwrap();

    assert_eq!(reclaim_abandoned_quarantines(root.path()), 1);
    assert!(
        !abandoned.exists(),
        "a dead owner's undo storage is garbage"
    );
    assert_eq!(fs::read(&preserved).unwrap(), b"ORIGINAL");
}

/// Marcel created the quarantine path by atomic rename, which says who held
/// it then and nothing about who holds it at eviction.
#[test]
fn quarantine_deletion_refuses_an_object_it_did_not_record() {
    let root = tempfile::tempdir().unwrap();
    let quarantine = root.path().join(".marcel-replaced-1-0-report.txt");
    fs::write(&quarantine, b"ORIGINAL").unwrap();
    let identity = FileIdentity::read(&quarantine).unwrap();
    let item = ReplacedItem {
        path: root.path().join("report.txt"),
        quarantine: quarantine.clone(),
        identity,
    };

    // Someone else takes the name in the meantime.
    fs::remove_file(&quarantine).unwrap();
    fs::write(&quarantine, b"SOMEONE ELSE'S").unwrap();
    erase_replacement_quarantine(&item);
    assert_eq!(fs::read(&quarantine).unwrap(), b"SOMEONE ELSE'S");

    // The object it actually recorded is released as before.
    let recorded = ReplacedItem {
        identity: FileIdentity::read(&quarantine).unwrap(),
        ..item
    };
    erase_replacement_quarantine(&recorded);
    assert!(!quarantine.exists());
}

/// Marcel's own bookkeeping must never be why an operation the filesystem
/// would have allowed fails.
#[test]
fn a_replacement_of_a_name_near_the_length_limit_succeeds() {
    let root = tempfile::tempdir().unwrap();
    let source_dir = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir(&source_dir).unwrap();
    fs::create_dir(&destination).unwrap();
    let long = "l".repeat(250);
    fs::write(source_dir.join(&long), b"NEW").unwrap();
    fs::write(destination.join(&long), b"ORIGINAL").unwrap();

    let mut policy = answering(super::conflict::ConflictDecision::once(
        ConflictResponse::Replace,
    ));
    let outcome = transfer_paths_with_conflicts(
        std::slice::from_ref(&source_dir.join(&long)),
        &destination,
        TransferMode::Copy,
        Arc::new(AtomicBool::new(false)),
        Arc::new(TransferProgress::default()),
        &mut policy,
    );

    assert!(outcome.failures.is_empty(), "{outcome:?}");
    assert_eq!(fs::read(destination.join(&long)).unwrap(), b"NEW");

    // And the replacement is still reversible.
    undo_operation(&outcome.operation.expect("a replacement records undo")).unwrap();
    assert_eq!(fs::read(destination.join(&long)).unwrap(), b"ORIGINAL");
}

#[test]
fn a_quarantine_name_stays_within_the_length_limit() {
    let name = quarantined_name(".marcel-replaced-4194304-", 9, OsStr::new(&"é".repeat(200)));
    use std::os::unix::ffi::OsStrExt as _;
    assert!(name.as_bytes().len() <= MAX_NAME_BYTES, "{name:?}");
    assert!(
        name.to_str().is_some(),
        "truncation stays on a character boundary: {name:?}"
    );
}

/// Refreshing an identity after a commit re-reads a path, and a path is not
/// an object. Adopting whatever is there would enter a stranger's file into
/// a record Undo is entitled to delete.
#[test]
fn an_identity_refresh_refuses_an_object_it_did_not_commit() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("published.txt");
    fs::write(&path, b"committed").unwrap();
    let mut snapshots = snapshot_tree(&path).unwrap();

    // The same path, a different object, published the way anything is
    // published atomically. Both files exist at once, so the replacement
    // cannot be handed the inode number the original still holds.
    let replacement = root.path().join("elsewhere.txt");
    fs::write(&replacement, b"someone else's").unwrap();
    fs::rename(&replacement, &path).unwrap();
    assert!(
        !refresh_snapshot_identities(&mut snapshots),
        "a substituted object is not the one that was committed"
    );

    // And the object it did commit still refreshes.
    let mut snapshots = snapshot_tree(&path).unwrap();
    assert!(refresh_snapshot_identities(&mut snapshots));
}

#[test]
fn marcel_working_names_are_recognized_without_catching_user_data() {
    for name in [
        ".marcel-replaced-1-0-report.txt",
        ".marcel-copy-1-0-abc",
        ".marcel-archive-abc",
    ] {
        assert!(is_internal_working_name(OsStr::new(name)), "{name}");
    }
    for name in [
        // Recovery guidance points the user straight at these.
        ".marcel-delete-1-0-report.txt",
        "report.txt",
        ".hidden",
        "marcel-replaced-1-0",
    ] {
        assert!(!is_internal_working_name(OsStr::new(name)), "{name}");
    }
}

/// A record pushed out of the journal can never be undone, so what it was
/// holding aside stops being recoverable data and becomes a hidden file
/// nobody would ever collect.
#[test]
fn evicting_a_record_releases_the_data_it_was_holding() {
    let root = tempfile::tempdir().unwrap();
    let source_dir = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir(&source_dir).unwrap();
    fs::create_dir(&destination).unwrap();
    fs::write(source_dir.join("report.txt"), b"replacement").unwrap();
    fs::write(destination.join("report.txt"), b"the original").unwrap();
    let mut policy = answering(super::conflict::ConflictDecision::once(
        ConflictResponse::Replace,
    ));
    let outcome = transfer_paths_with_conflicts(
        std::slice::from_ref(&source_dir.join("report.txt")),
        &destination,
        TransferMode::Copy,
        Arc::new(AtomicBool::new(false)),
        Arc::new(TransferProgress::default()),
        &mut policy,
    );
    let replacing = outcome.operation.expect("a replacement retains undo");
    assert!(!no_replacement_quarantines(&destination));

    let mut journal = OperationJournal::new(1);
    assert!(journal.record(replacing).is_empty());
    // A second record displaces the first, which can now never be undone.
    let evicted = journal.record(recorded(create_directory(root.path(), "later").unwrap()));

    assert_eq!(evicted.len(), 1);
    for record in &evicted {
        record.release_quarantines();
    }
    assert!(
        no_replacement_quarantines(&destination),
        "an unreachable record must not keep holding disk"
    );
    // The replacement itself is untouched; only its way back is gone.
    assert_eq!(
        fs::read(destination.join("report.txt")).unwrap(),
        b"replacement"
    );
}

/// The whole promise of replacement: what it displaced comes back. Nautilus
/// overwrites in place, so its undo cannot do this at all.
#[test]
fn undoing_a_replacement_puts_the_displaced_file_back() {
    let root = tempfile::tempdir().unwrap();
    let source_dir = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir(&source_dir).unwrap();
    fs::create_dir(&destination).unwrap();
    fs::write(source_dir.join("report.txt"), b"replacement").unwrap();
    fs::write(destination.join("report.txt"), b"the original").unwrap();
    let mut policy = answering(super::conflict::ConflictDecision::once(
        ConflictResponse::Replace,
    ));

    let outcome = transfer_paths_with_conflicts(
        std::slice::from_ref(&source_dir.join("report.txt")),
        &destination,
        TransferMode::Copy,
        Arc::new(AtomicBool::new(false)),
        Arc::new(TransferProgress::default()),
        &mut policy,
    );

    assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
    assert!(!outcome.undo_unavailable);
    assert_eq!(
        fs::read(destination.join("report.txt")).unwrap(),
        b"replacement"
    );
    let operation = outcome.operation.expect("a replacement retains undo");

    undo_operation(&operation).unwrap();

    assert_eq!(
        fs::read(destination.join("report.txt")).unwrap(),
        b"the original",
        "undo must restore what the replacement displaced"
    );
    assert!(no_replacement_quarantines(&destination));
}

/// A transfer that fails after displacing must put the displaced item back
/// rather than leaving the destination empty.
#[test]
fn a_failed_replacement_restores_what_it_displaced() {
    let root = tempfile::tempdir().unwrap();
    let source_dir = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir(&source_dir).unwrap();
    fs::create_dir(&destination).unwrap();
    // A socket cannot be copied, so the transfer fails after the
    // destination has already been moved aside.
    let source = source_dir.join("payload");
    fs::create_dir(&source).unwrap();
    let _listener = std::os::unix::net::UnixListener::bind(source.join("daemon.sock")).unwrap();
    fs::write(destination.join("payload"), b"the original").unwrap();
    let mut policy = answering(super::conflict::ConflictDecision::once(
        ConflictResponse::Replace,
    ));

    let outcome = transfer_paths_with_conflicts(
        std::slice::from_ref(&source),
        &destination,
        TransferMode::Copy,
        Arc::new(AtomicBool::new(false)),
        Arc::new(TransferProgress::default()),
        &mut policy,
    );

    assert_eq!(outcome.failures.len(), 1, "{outcome:?}");
    assert_eq!(
        fs::read(destination.join("payload")).unwrap(),
        b"the original",
        "a failed replacement must not consume the original"
    );
    assert!(no_replacement_quarantines(&destination));
}

/// Past the byte budget the replacement still happens; it simply stops
/// being reversible, and the quarantine is released rather than held.
#[test]
fn an_oversized_replacement_succeeds_without_undo() {
    let root = tempfile::tempdir().unwrap();
    let source_dir = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir(&source_dir).unwrap();
    fs::create_dir(&destination).unwrap();
    fs::write(source_dir.join("blob.bin"), b"replacement").unwrap();
    fs::write(destination.join("blob.bin"), vec![0_u8; 4096]).unwrap();
    let mut policy = answering(super::conflict::ConflictDecision::once(
        ConflictResponse::Replace,
    ));

    let outcome = Transfer::new(
        std::slice::from_ref(&source_dir.join("blob.bin")),
        &destination,
        TransferMode::Copy,
        Arc::new(AtomicBool::new(false)),
    )
    .with_budget(TransferBudget {
        replacement_undo_byte_limit: 1024,
        ..TransferBudget::default()
    })
    .run(&mut policy);

    assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
    assert!(
        outcome.undo_unavailable,
        "an oversized replacement cannot be undone"
    );
    assert_eq!(
        fs::read(destination.join("blob.bin")).unwrap(),
        b"replacement"
    );
    assert!(
        no_replacement_quarantines(&destination),
        "an unreachable quarantine must be released, not left on disk"
    );
}

/// Choosing merge for two directories must never discard what the
/// destination already holds, even when the source has nothing to add.
#[test]
fn merging_an_empty_directory_keeps_the_destination_intact() {
    let root = tempfile::tempdir().unwrap();
    let source_dir = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir_all(source_dir.join("shared")).unwrap();
    fs::create_dir_all(destination.join("shared")).unwrap();
    fs::write(destination.join("shared/keep.txt"), b"keep").unwrap();
    let mut policy = answering(super::conflict::ConflictDecision::for_all(
        ConflictResponse::Replace,
    ));

    let outcome = transfer_paths_with_conflicts(
        std::slice::from_ref(&source_dir.join("shared")),
        &destination,
        TransferMode::Copy,
        Arc::new(AtomicBool::new(false)),
        Arc::new(TransferProgress::default()),
        &mut policy,
    );

    assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
    assert_eq!(
        fs::read(destination.join("shared/keep.txt")).unwrap(),
        b"keep"
    );
    // Nothing was added, so there is nothing to undo.
    assert!(outcome.operation.is_none());
}

fn no_replacement_quarantines(directory: &Path) -> bool {
    fs::read_dir(directory)
        .unwrap()
        .flatten()
        .all(|entry| !is_replacement_quarantine_name(&entry.file_name()))
}

/// Renaming everything keeps every source, each beside the item it
/// collided with, without a single item being lost or overwritten.
#[test]
fn renaming_all_keeps_every_colliding_source() {
    let root = tempfile::tempdir().unwrap();
    let source_dir = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir(&source_dir).unwrap();
    fs::create_dir(&destination).unwrap();
    for name in ["a.txt", "b.txt"] {
        fs::write(source_dir.join(name), b"new").unwrap();
        fs::write(destination.join(name), b"existing").unwrap();
    }
    // Already occupied, so "a.txt" has to land past it.
    fs::write(destination.join("a (2).txt"), b"existing too").unwrap();
    let sources = vec![source_dir.join("a.txt"), source_dir.join("b.txt")];
    let mut policy = answering(super::conflict::ConflictDecision::for_all(
        ConflictResponse::AutoRename,
    ));

    let outcome = transfer_paths_with_conflicts(
        &sources,
        &destination,
        TransferMode::Copy,
        Arc::new(AtomicBool::new(false)),
        Arc::new(TransferProgress::default()),
        &mut policy,
    );

    assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
    assert_eq!(outcome.completed.len(), 2);
    assert_eq!(outcome.accounted(), sources.len());
    // Nothing that was already there changed.
    assert_eq!(fs::read(destination.join("a.txt")).unwrap(), b"existing");
    assert_eq!(fs::read(destination.join("b.txt")).unwrap(), b"existing");
    assert_eq!(
        fs::read(destination.join("a (2).txt")).unwrap(),
        b"existing too"
    );
    // Both sources arrived beside them.
    assert_eq!(fs::read(destination.join("a (3).txt")).unwrap(), b"new");
    assert_eq!(fs::read(destination.join("b (2).txt")).unwrap(), b"new");
}

/// Merging is the union of two trees: the destination keeps everything it
/// has and gains what it lacks. Nothing is displaced, which is what makes
/// undo exact rather than approximate.
#[test]
fn merging_adds_what_is_missing_and_keeps_what_is_there() {
    let root = tempfile::tempdir().unwrap();
    let source_dir = root.path().join("source");
    let destination = root.path().join("destination");
    // Source tree: a colliding file, a new file, a colliding subdirectory
    // holding a new file, and a wholly new subdirectory.
    fs::create_dir_all(source_dir.join("photos/holiday")).unwrap();
    fs::create_dir_all(source_dir.join("photos/new-album")).unwrap();
    fs::write(source_dir.join("photos/shared.txt"), b"NEW").unwrap();
    fs::write(source_dir.join("photos/only-in-source.txt"), b"NEW").unwrap();
    fs::write(source_dir.join("photos/holiday/beach.txt"), b"NEW").unwrap();
    fs::write(source_dir.join("photos/new-album/cover.txt"), b"NEW").unwrap();
    // Destination tree: the same directory, one colliding file, one of its
    // own, and the colliding subdirectory with its own contents.
    fs::create_dir_all(destination.join("photos/holiday")).unwrap();
    fs::write(destination.join("photos/shared.txt"), b"ORIGINAL").unwrap();
    fs::write(
        destination.join("photos/only-in-destination.txt"),
        b"ORIGINAL",
    )
    .unwrap();
    fs::write(destination.join("photos/holiday/sunset.txt"), b"ORIGINAL").unwrap();

    let mut policy = answering(super::conflict::ConflictDecision::once(
        ConflictResponse::Replace,
    ));
    let outcome = transfer_paths_with_conflicts(
        std::slice::from_ref(&source_dir.join("photos")),
        &destination,
        TransferMode::Copy,
        Arc::new(AtomicBool::new(false)),
        Arc::new(TransferProgress::default()),
        &mut policy,
    );

    assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
    let merged = destination.join("photos");
    // Everything the destination already had is untouched.
    assert_eq!(fs::read(merged.join("shared.txt")).unwrap(), b"ORIGINAL");
    assert_eq!(
        fs::read(merged.join("only-in-destination.txt")).unwrap(),
        b"ORIGINAL"
    );
    assert_eq!(
        fs::read(merged.join("holiday/sunset.txt")).unwrap(),
        b"ORIGINAL"
    );
    // Everything it lacked has arrived, at every depth.
    assert_eq!(fs::read(merged.join("only-in-source.txt")).unwrap(), b"NEW");
    assert_eq!(fs::read(merged.join("holiday/beach.txt")).unwrap(), b"NEW");
    assert_eq!(
        fs::read(merged.join("new-album/cover.txt")).unwrap(),
        b"NEW"
    );

    // Undo removes exactly what arrived and nothing else.
    let operation = outcome.operation.expect("a merge retains undo");
    undo_operation(&operation).unwrap();

    assert_eq!(fs::read(merged.join("shared.txt")).unwrap(), b"ORIGINAL");
    assert_eq!(
        fs::read(merged.join("only-in-destination.txt")).unwrap(),
        b"ORIGINAL"
    );
    assert_eq!(
        fs::read(merged.join("holiday/sunset.txt")).unwrap(),
        b"ORIGINAL"
    );
    assert!(!merged.join("only-in-source.txt").exists());
    assert!(!merged.join("holiday/beach.txt").exists());
    assert!(!merged.join("new-album").exists());
    // The source is a copy source, so it is left exactly as it was.
    assert_eq!(
        fs::read(source_dir.join("photos/shared.txt")).unwrap(),
        b"NEW"
    );
}

/// Undo of a merge validates each item on its own, so something added
/// inside the merged tree afterwards is left alone rather than deleted.
#[test]
fn undoing_a_merge_leaves_later_additions_alone() {
    let root = tempfile::tempdir().unwrap();
    let source_dir = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir_all(source_dir.join("photos")).unwrap();
    fs::create_dir_all(destination.join("photos")).unwrap();
    fs::write(source_dir.join("photos/arrived.txt"), b"NEW").unwrap();

    let mut policy = answering(super::conflict::ConflictDecision::once(
        ConflictResponse::Replace,
    ));
    let outcome = transfer_paths_with_conflicts(
        std::slice::from_ref(&source_dir.join("photos")),
        &destination,
        TransferMode::Copy,
        Arc::new(AtomicBool::new(false)),
        Arc::new(TransferProgress::default()),
        &mut policy,
    );
    let operation = outcome.operation.expect("a merge retains undo");
    // Someone adds a file to the merged directory afterwards.
    let later = destination.join("photos/added-later.txt");
    fs::write(&later, b"MINE").unwrap();

    undo_operation(&operation).unwrap();

    assert!(!destination.join("photos/arrived.txt").exists());
    assert_eq!(fs::read(&later).unwrap(), b"MINE");
}

/// The merge's removals commit one at a time, so an undo that will refuse
/// the copy's own output must refuse *before* removing them. It used to
/// remove the merge's additions, then report "nothing happened" and keep a
/// record that could never validate again.
#[test]
fn copy_undo_with_a_merge_refuses_a_modified_output_before_removing_anything() {
    let root = tempfile::tempdir().unwrap();
    let source_dir = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir_all(source_dir.join("new_item")).unwrap();
    fs::create_dir_all(source_dir.join("photos")).unwrap();
    fs::create_dir_all(destination.join("photos")).unwrap();
    fs::write(source_dir.join("new_item/inside.txt"), b"NEW").unwrap();
    fs::write(source_dir.join("photos/arrived.txt"), b"NEW").unwrap();

    let mut policy = answering(super::conflict::ConflictDecision::once(
        ConflictResponse::Replace,
    ));
    let outcome = transfer_paths_with_conflicts(
        &[source_dir.join("new_item"), source_dir.join("photos")],
        &destination,
        TransferMode::Copy,
        Arc::new(AtomicBool::new(false)),
        Arc::new(TransferProgress::default()),
        &mut policy,
    );
    let operation = outcome.operation.expect("the transfer retains undo");
    // Someone adds a file to the copied output afterwards, so its
    // recorded tree no longer matches the disk.
    fs::write(destination.join("new_item/added-later.txt"), b"MINE").unwrap();

    let result = undo_operation(&operation);

    assert!(
        matches!(result, MutationOutcome::Unchanged(_)),
        "a refusal raised before anything was removed must stay retryable: {result:?}"
    );
    assert_eq!(
        fs::read(destination.join("photos/arrived.txt")).unwrap(),
        b"NEW",
        "the merge's additions must survive a refused undo"
    );
}

/// When the copy's output cannot be removed *after* the merge's additions
/// already were, the disk has changed and the record must be discarded —
/// reporting it as unchanged would present a retryable undo that can never
/// validate again.
#[test]
fn copy_undo_that_removed_merge_additions_discards_rather_than_claiming_no_effect() {
    use std::os::unix::fs::PermissionsExt as _;

    if rustix::process::geteuid().is_root() {
        // Permission bits do not constrain root, so the removal failure
        // this test depends on cannot be provoked.
        return;
    }

    let root = tempfile::tempdir().unwrap();
    let source_dir = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir_all(&source_dir).unwrap();
    fs::create_dir_all(destination.join("photos")).unwrap();
    fs::write(source_dir.join("new_item.txt"), b"NEW").unwrap();
    fs::create_dir(source_dir.join("photos")).unwrap();
    fs::write(source_dir.join("photos/arrived.txt"), b"NEW").unwrap();

    let mut policy = answering(super::conflict::ConflictDecision::once(
        ConflictResponse::Replace,
    ));
    let outcome = transfer_paths_with_conflicts(
        &[source_dir.join("new_item.txt"), source_dir.join("photos")],
        &destination,
        TransferMode::Copy,
        Arc::new(AtomicBool::new(false)),
        Arc::new(TransferProgress::default()),
        &mut policy,
    );
    let operation = outcome.operation.expect("the transfer retains undo");

    // The merge's additions can still be removed, but the copied output
    // cannot leave its now read-only parent.
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o555)).unwrap();
    let result = undo_operation(&operation);
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();

    let MutationOutcome::Discarded { changes, .. } = result else {
        panic!("an undo that removed the merge's additions must discard: {result:?}");
    };
    assert!(
        changes
            .removed
            .contains(&destination.join("photos/arrived.txt")),
        "the removals that committed must be reported: {changes:?}"
    );
    assert!(
        !destination.join("photos/arrived.txt").exists(),
        "this scenario depends on the merge's additions being removed"
    );
    assert_eq!(
        fs::read(destination.join("new_item.txt")).unwrap(),
        b"NEW",
        "the copied output could not be removed and must survive"
    );
}

/// A merge that stops part way has still added part of what it planned.
/// Returning a bare failure would tell the caller the disk is unchanged
/// while half a merge sits in the destination with no way to take it back.
#[test]
fn a_merge_stopped_by_failure_records_what_it_added() {
    let root = tempfile::tempdir().unwrap();
    let source_dir = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir_all(source_dir.join("photos/album")).unwrap();
    fs::create_dir_all(destination.join("photos")).unwrap();
    fs::write(destination.join("photos/keep.txt"), b"ORIGINAL").unwrap();
    fs::write(source_dir.join("photos/arrives.txt"), b"NEW").unwrap();
    fs::write(source_dir.join("photos/blocked.txt"), b"NEW").unwrap();
    fs::write(source_dir.join("photos/album/inside.txt"), b"NEW").unwrap();

    // Publishing this one leaf fails, after the merge has already created a
    // directory and published a file.
    let _fault = fault::fail_renames_to("blocked.txt");
    let mut policy = answering(super::conflict::ConflictDecision::once(
        ConflictResponse::Replace,
    ));
    let outcome = transfer_paths_with_conflicts(
        std::slice::from_ref(&source_dir.join("photos")),
        &destination,
        TransferMode::Copy,
        Arc::new(AtomicBool::new(false)),
        Arc::new(TransferProgress::default()),
        &mut policy,
    );

    assert_eq!(outcome.failures.len(), 1, "{outcome:?}");
    assert!(outcome.completed.is_empty(), "{outcome:?}");
    let merged = destination.join("photos");
    assert_eq!(fs::read(merged.join("arrives.txt")).unwrap(), b"NEW");
    assert!(merged.join("album").is_dir());

    // The partial merge is describable, so Undo can take back exactly what
    // arrived and leave what the destination already had.
    let operation = outcome
        .operation
        .expect("a partial merge still records its additions");
    undo_operation(&operation).unwrap();

    assert!(!merged.join("arrives.txt").exists());
    assert!(!merged.join("album").exists());
    assert_eq!(fs::read(merged.join("keep.txt")).unwrap(), b"ORIGINAL");
}

/// Cancelling is an answer, not a fault. A merge that reports cancellation
/// as a failure tells the user their merge broke, and letting the loop
/// continue attempts work they just stopped.
#[test]
fn a_cancelled_merge_is_reported_as_cancellation() {
    let root = tempfile::tempdir().unwrap();
    let source_dir = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir_all(source_dir.join("photos")).unwrap();
    fs::create_dir(source_dir.join("later")).unwrap();
    fs::create_dir_all(destination.join("photos")).unwrap();
    fs::write(source_dir.join("photos/arrives.txt"), b"NEW").unwrap();

    let mut policy = answering(super::conflict::ConflictDecision::for_all(
        ConflictResponse::Replace,
    ));
    let sources = vec![source_dir.join("photos"), source_dir.join("later")];
    let outcome = transfer_paths_with_conflicts(
        &sources,
        &destination,
        TransferMode::Copy,
        Arc::new(AtomicBool::new(true)),
        Arc::new(TransferProgress::default()),
        &mut policy,
    );

    assert!(
        outcome.failures.is_empty(),
        "cancelling is not a failure: {outcome:?}"
    );
    assert_eq!(outcome.cancelled, sources, "{outcome:?}");
    assert!(!destination.join("photos/arrives.txt").exists());
}

/// The snapshot budget bounds one operation, so a merge cannot help itself
/// to a fresh allowance per leaf. Past it the merge still happens and says
/// it cannot be undone.
#[test]
fn a_merge_past_the_snapshot_budget_succeeds_without_undo() {
    let root = tempfile::tempdir().unwrap();
    let source_dir = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir_all(source_dir.join("photos")).unwrap();
    fs::create_dir_all(destination.join("photos")).unwrap();
    for index in 0..4 {
        fs::write(source_dir.join(format!("photos/{index}.txt")), b"NEW").unwrap();
    }

    let mut policy = answering(super::conflict::ConflictDecision::once(
        ConflictResponse::Replace,
    ));
    let outcome = Transfer::new(
        std::slice::from_ref(&source_dir.join("photos")),
        &destination,
        TransferMode::Copy,
        Arc::new(AtomicBool::new(false)),
    )
    .with_budget(TransferBudget {
        undo_snapshot_limit: 2,
        ..TransferBudget::default()
    })
    .run(&mut policy);

    assert!(outcome.failures.is_empty(), "{outcome:?}");
    assert_eq!(outcome.completed.len(), 1, "{outcome:?}");
    assert!(
        outcome.undo_unavailable,
        "a merge past the budget is not undoable: {outcome:?}"
    );
    for index in 0..4 {
        assert!(
            destination.join(format!("photos/{index}.txt")).exists(),
            "the merge still happens"
        );
    }
}

/// Moving had no snapshot budget at all, so one rename could hold an
/// arbitrarily large record. Bounding it must never refuse the move: a
/// rename does not care how big the tree is, and refusing would be a worse
/// answer than losing undo.
#[test]
fn a_move_past_the_snapshot_budget_succeeds_without_undo() {
    let root = tempfile::tempdir().unwrap();
    let source_dir = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir_all(source_dir.join("album")).unwrap();
    fs::create_dir(&destination).unwrap();
    for index in 0..4 {
        fs::write(source_dir.join(format!("album/{index}.txt")), b"payload").unwrap();
    }

    let outcome = Transfer::new(
        std::slice::from_ref(&source_dir.join("album")),
        &destination,
        TransferMode::Move,
        Arc::new(AtomicBool::new(false)),
    )
    .with_budget(TransferBudget {
        undo_snapshot_limit: 2,
        ..TransferBudget::default()
    })
    .run(&mut ConflictPolicy::refusing());

    assert!(outcome.failures.is_empty(), "{outcome:?}");
    assert_eq!(outcome.completed.len(), 1, "{outcome:?}");
    assert!(
        outcome.undo_unavailable,
        "a move past the budget is not undoable: {outcome:?}"
    );
    assert!(outcome.operation.is_none(), "{outcome:?}");
    assert!(!source_dir.join("album").exists(), "the move still happens");
    assert_eq!(
        fs::read(destination.join("album/0.txt")).unwrap(),
        b"payload"
    );
}

/// Moving cannot express a merge yet, so it refuses rather than discarding
/// the tree the user expected to be joined.
#[test]
fn moving_a_directory_onto_a_directory_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let source_dir = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir_all(source_dir.join("photos")).unwrap();
    fs::create_dir_all(destination.join("photos")).unwrap();
    fs::write(destination.join("photos/keep.txt"), b"keep").unwrap();
    let mut policy = answering(super::conflict::ConflictDecision::for_all(
        ConflictResponse::Replace,
    ));

    let outcome = transfer_paths_with_conflicts(
        std::slice::from_ref(&source_dir.join("photos")),
        &destination,
        TransferMode::Move,
        Arc::new(AtomicBool::new(false)),
        Arc::new(TransferProgress::default()),
        &mut policy,
    );

    assert_eq!(outcome.failures.len(), 1, "{outcome:?}");
    assert_eq!(
        fs::read(destination.join("photos/keep.txt")).unwrap(),
        b"keep"
    );
    assert!(source_dir.join("photos").exists());
}

/// Dropping a selection onto the folder it already lives in asks for
/// nothing. Refusing it would invent a problem the user does not have, and
/// reporting it as skipped would claim they declined something.
#[test]
fn moving_an_item_where_it_already_is_does_nothing() {
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("folder");
    fs::create_dir(&folder).unwrap();
    let source = folder.join("report.txt");
    fs::write(&source, b"payload").unwrap();
    let mut policy = answering(super::conflict::ConflictDecision::for_all(
        ConflictResponse::Replace,
    ));

    let outcome = transfer_paths_with_conflicts(
        std::slice::from_ref(&source),
        &folder,
        TransferMode::Move,
        Arc::new(AtomicBool::new(false)),
        Arc::new(TransferProgress::default()),
        &mut policy,
    );

    assert_eq!(outcome.already_in_place, std::slice::from_ref(&source));
    assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
    assert!(outcome.skipped.is_empty());
    assert!(outcome.completed.is_empty());
    assert!(outcome.operation.is_none());
    assert_eq!(fs::read(&source).unwrap(), b"payload");
    assert_eq!(outcome.accounted(), 1);
}

/// Copying a file into the folder it lives in is a request to duplicate it,
/// and it has one sensible answer, so it is answered rather than asked.
#[test]
fn copying_an_item_into_its_own_folder_duplicates_it_without_asking() {
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("folder");
    fs::create_dir(&folder).unwrap();
    let source = folder.join("report.txt");
    fs::write(&source, b"payload").unwrap();
    // A resolver that would panic if consulted: this must not ask.
    let mut policy = ConflictPolicy::interactive(Arc::new(AlwaysAnswers(
        super::conflict::ConflictDecision::once(ConflictResponse::Cancel),
    )));

    let outcome = transfer_paths_with_conflicts(
        std::slice::from_ref(&source),
        &folder,
        TransferMode::Copy,
        Arc::new(AtomicBool::new(false)),
        Arc::new(TransferProgress::default()),
        &mut policy,
    );

    assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
    assert_eq!(outcome.completed.len(), 1);
    assert_eq!(fs::read(&source).unwrap(), b"payload");
    assert_eq!(
        fs::read(folder.join("report (2).txt")).unwrap(),
        b"payload",
        "the duplicate lands beside the original"
    );
    // Duplicating again steps past the name it just created.
    let outcome = transfer_paths_with_conflicts(
        std::slice::from_ref(&source),
        &folder,
        TransferMode::Copy,
        Arc::new(AtomicBool::new(false)),
        Arc::new(TransferProgress::default()),
        &mut policy,
    );
    assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
    assert!(folder.join("report (3).txt").exists());
}

/// A hard link names the same object under a different path, so comparing
/// paths would miss it. Replacing there would quarantine the very object
/// about to be renamed, so a move refuses; a copy is safe because it never
/// destroys the source, and duplicating is what was asked for.
#[test]
fn a_hardlink_to_the_source_is_recognized_as_the_same_object() {
    let root = tempfile::tempdir().unwrap();
    let source_dir = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir(&source_dir).unwrap();
    fs::create_dir(&destination).unwrap();
    let source = source_dir.join("report.pdf");
    fs::write(&source, b"original").unwrap();
    // Same inode, different directory, same basename: the transfer would
    // land exactly on its own source.
    fs::hard_link(&source, destination.join("report.pdf")).unwrap();
    let mut policy = answering(super::conflict::ConflictDecision::for_all(
        ConflictResponse::Replace,
    ));

    let moved = transfer_paths_with_conflicts(
        std::slice::from_ref(&source),
        &destination,
        TransferMode::Move,
        Arc::new(AtomicBool::new(false)),
        Arc::new(TransferProgress::default()),
        &mut policy,
    );

    assert_eq!(moved.failures.len(), 1, "{moved:?}");
    assert!(
        moved.failures[0].message.contains("over itself"),
        "{:?}",
        moved.failures
    );
    assert_eq!(fs::read(&source).unwrap(), b"original");

    let copied = transfer_paths_with_conflicts(
        std::slice::from_ref(&source),
        &destination,
        TransferMode::Copy,
        Arc::new(AtomicBool::new(false)),
        Arc::new(TransferProgress::default()),
        &mut policy,
    );

    assert!(copied.failures.is_empty(), "{:?}", copied.failures);
    assert_eq!(fs::read(&source).unwrap(), b"original");
    // The existing link is untouched and the duplicate lands beside it.
    assert_eq!(
        fs::read(destination.join("report.pdf")).unwrap(),
        b"original"
    );
    assert_eq!(
        fs::read(destination.join("report (2).pdf")).unwrap(),
        b"original"
    );
}

/// Cancelling the operation through the cancel flag must also account for
/// the sources it never reached.
#[test]
fn a_cancelled_transfer_accounts_for_every_requested_source() {
    let (_root, sources, destination) = occupied_transfer_fixture();

    let outcome = transfer_paths(
        &sources,
        &destination,
        TransferMode::Copy,
        Arc::new(AtomicBool::new(true)),
    );

    assert_eq!(outcome.cancelled, sources);
    assert_eq!(outcome.accounted(), sources.len());
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
    let outcome = Transfer::new(
        &[source],
        &destination_parent,
        TransferMode::Copy,
        Arc::new(AtomicBool::new(false)),
    )
    .with_budget(TransferBudget {
        undo_snapshot_limit: 2,
        ..TransferBudget::default()
    })
    .run(&mut ConflictPolicy::refusing());

    assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
    assert!(outcome.undo_unavailable);
    assert!(outcome.operation.is_none());
    let destination = destination_parent.join("source");
    assert_eq!(fs::read(destination.join("one")).unwrap(), b"1");
    assert_eq!(fs::read(destination.join("two")).unwrap(), b"2");
}

#[test]
fn archive_create_and_extract_support_identity_validated_undo_redo() {
    if super::archive::SevenZipBackend::discover().is_err() {
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
    let extracted =
        recorded(extract_archive_operation(&archive, Arc::new(AtomicBool::new(false))).unwrap());
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
