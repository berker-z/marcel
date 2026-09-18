use super::{
    conflict::{
        ConflictDecision, ConflictPolicy, ConflictRequest, ConflictResolver, ConflictResponse,
    },
    copy::{copy_file_cancellable, preserve_metadata, supported_xattr_name, xattrs_unsupported},
    identity::FileIdentity,
    journal::*,
    local::{MAX_NAME_BYTES, fault, quarantined_name},
    mutations::*,
    quarantine::*,
    transfer::*,
    *,
};
use crate::testing::{Sandbox, no_cancel, read, seal, skip_as_root};
use std::{
    ffi::OsStr,
    fs,
    io::{self, Read as _, Seek as _, Write as _},
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, UNIX_EPOCH},
};

/// A transfer nobody can answer conflicts for.
fn transfer(sources: &[PathBuf], destination: &Path, mode: TransferMode) -> TransferOutcome {
    transfer_paths(sources, destination, mode, no_cancel())
}

fn copy(sources: &[PathBuf], destination: &Path) -> TransferOutcome {
    transfer(sources, destination, TransferMode::Copy)
}

fn mv(sources: &[PathBuf], destination: &Path) -> TransferOutcome {
    transfer(sources, destination, TransferMode::Move)
}

/// A resolver that answers every conflict the same way.
struct AlwaysAnswers(ConflictDecision);

impl ConflictResolver for AlwaysAnswers {
    fn resolve(&self, _request: &ConflictRequest) -> ConflictDecision {
        self.0.clone()
    }
}

fn answering(decision: ConflictDecision) -> ConflictPolicy {
    ConflictPolicy::interactive(Arc::new(AlwaysAnswers(decision)))
}

/// A transfer whose every conflict gets `decision`.
fn transfer_deciding(
    sources: &[PathBuf],
    destination: &Path,
    mode: TransferMode,
    decision: ConflictDecision,
) -> TransferOutcome {
    transfer_paths_with_conflicts(
        sources,
        destination,
        mode,
        no_cancel(),
        Arc::new(TransferProgress::default()),
        &mut answering(decision),
    )
}

fn replacing(sources: &[PathBuf], destination: &Path) -> TransferOutcome {
    transfer_deciding(
        sources,
        destination,
        TransferMode::Copy,
        ConflictDecision::once(ConflictResponse::Replace),
    )
}

fn budgeted(
    sources: &[PathBuf],
    destination: &Path,
    mode: TransferMode,
    budget: TransferBudget,
) -> TransferOutcome {
    Transfer::new(sources, destination, mode, no_cancel())
        .with_budget(budget)
        .run(&mut answering(ConflictDecision::once(ConflictResponse::Replace)))
}

fn assert_clean(outcome: &TransferOutcome) {
    assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
}

/// Unwrap a committed operation that the test expects to have retained
/// undo. Failing here means bookkeeping was lost, not that the mutation
/// failed.
fn recorded(committed: CommittedOperation) -> OperationRecord {
    committed.into_record().expect("operation should have retained an undo record")
}

fn no_replacement_quarantines(directory: &Path) -> bool {
    fs::read_dir(directory)
        .unwrap()
        .flatten()
        .all(|entry| !is_replacement_quarantine_name(&entry.file_name()))
}

/// A lexical prefix test misses a symlinked destination. Marcel then placed
/// its staging directory inside the tree it was walking and re-copied its
/// own output once per path component until `PATH_MAX` stopped it, writing
/// roughly 156x the source size into the user's own directory.
#[test]
fn copy_refuses_a_destination_that_resolves_inside_the_source() {
    let sandbox = Sandbox::new();
    let source = sandbox.dir("src");
    sandbox.file("src/sub/payload.bin", vec![7_u8; 4096]);
    let alias = sandbox.path("alias");
    std::os::unix::fs::symlink(source.join("sub"), &alias).unwrap();
    let before = tree_size(&source);

    let outcome = copy(std::slice::from_ref(&source), &alias);

    assert_eq!(outcome.failures.len(), 1);
    assert!(outcome.failures[0].message.contains("into itself"), "{:?}", outcome.failures);
    assert!(outcome.operation.is_none());
    assert_eq!(tree_size(&source), before, "the copy amplified the source");
}

#[test]
fn move_refuses_a_destination_that_resolves_inside_the_source() {
    let sandbox = Sandbox::new();
    let source = sandbox.dir("src");
    sandbox.dir("src/sub");
    let alias = sandbox.path("alias");
    std::os::unix::fs::symlink(source.join("sub"), &alias).unwrap();

    let outcome = mv(std::slice::from_ref(&source), &alias);

    assert_eq!(outcome.failures.len(), 1);
    assert!(source.is_dir());
}

/// Snapshotting after the rename turned any directory containing a socket
/// into a phantom failure: the move had happened, but the caller was told
/// it had not. A move is a rename in both directions, so a tree Marcel could
/// never copy or archive is still fully reversible.
#[test]
fn a_moved_tree_holding_a_socket_is_reported_undoable_and_redoable() {
    let sandbox = Sandbox::new();
    let project = sandbox.dir("source/project");
    let destination = sandbox.dir("destination");
    sandbox.file("source/project/notes.txt", b"important");
    let _listener = sandbox.socket("source/project/daemon.sock");

    let outcome = mv(std::slice::from_ref(&project), &destination);

    assert_clean(&outcome);
    assert_eq!(outcome.completed_destinations(), [destination.join("project")]);
    assert!(!project.exists());
    assert_eq!(read(destination.join("project/notes.txt")), b"important");
    // A rename never inspects what the tree holds, so the socket costs
    // nothing: the move succeeds *and* stays undoable.
    assert!(!outcome.undo_unavailable);
    let operation = outcome.operation.expect("a moved socket tree retains undo");

    let redo_record = recorded(undo_operation(&operation).unwrap());
    assert!(project.join("daemon.sock").exists(), "undo restored the tree");
    assert_eq!(read(project.join("notes.txt")), b"important");
    assert!(!destination.join("project").exists());

    recorded(redo_operation(&redo_record).unwrap());
    assert!(destination.join("project/daemon.sock").exists());
    assert!(!project.exists());
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
    if skip_as_root() {
        return;
    }
    let sandbox = Sandbox::new();
    // Two source parents, so one can be sealed without blocking the other.
    // `undo_operation` validates every transfer before renaming any, so an
    // obstacle it can see up front yields `Unchanged`; reaching the
    // rolled-back path needs a failure only the rename itself discovers.
    let blocked = sandbox.dir("blocked");
    let open = sandbox.dir("open");
    let destination = sandbox.dir("destination");
    sandbox.file("blocked/first/data.txt", b"first");
    sandbox.file("open/second/data.txt", b"second");

    let outcome = mv(&[blocked.join("first"), open.join("second")], &destination);
    let operation = outcome.operation.expect("the move retains undo");

    // Undo walks transfers in reverse: "second" returns to `open` first and
    // commits, then "first" cannot be created back inside a read-only
    // parent. Stat still succeeds, so the preflight cannot catch it.
    seal(&blocked, true);
    let result = undo_operation(&operation);
    seal(&blocked, false);

    assert!(!result.keeps_history(), "a rolled-back undo must discard its record, got {result:?}");
    assert!(matches!(result, MutationOutcome::Discarded { .. }), "{result:?}");
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
    let sandbox = Sandbox::new();
    let only = sandbox.dir("source/only");
    sandbox.file("source/only/data.txt", b"payload");
    let destination = sandbox.dir("destination");

    let outcome = mv(std::slice::from_ref(&only), &destination);
    let operation = outcome.operation.expect("the move retains undo");
    fs::write(&only, b"in the way").unwrap();

    let result = undo_operation(&operation);

    assert!(result.keeps_history(), "a pre-commit refusal must stay retryable, got {result:?}");
    assert!(destination.join("only/data.txt").exists());

    // Clearing the obstacle makes the retained record work.
    fs::remove_file(&only).unwrap();
    undo_operation(&operation).unwrap();
    assert!(only.join("data.txt").exists());
}

/// Recording special files must not leak into paths whose undo deletes the
/// tree: Marcel cannot recreate a socket, so an archive holding one stays
/// success-without-undo rather than gaining an undo that would erase it.
#[test]
fn archive_sources_and_tree_removal_refuse_special_files() {
    let sandbox = Sandbox::new();
    let source = sandbox.dir("payload");
    sandbox.file("payload/notes.txt", b"keep");
    let _listener = sandbox.socket("payload/daemon.sock");

    let error = create_zip_operation(
        std::slice::from_ref(&source),
        &sandbox.path("payload.zip"),
        no_cancel(),
    )
    .expect_err("an archive cannot carry a socket");
    assert!(error.to_string().contains("Special files"), "{error}");
    assert!(!sandbox.path("payload.zip").exists());

    let snapshots = snapshot_tree(&source).expect("the rename walker records specials");
    assert!(remove_snapshotted_tree(&snapshots).is_err());
    assert_eq!(read(source.join("notes.txt")), b"keep");
    assert!(source.join("daemon.sock").exists());
}

/// Marcel runs transfers on `blocking` pool threads with Rust's 2 MiB
/// default stack. Recursive walkers aborted the whole process with a stack
/// overflow rather than reporting a failure.
#[test]
fn deep_directory_trees_do_not_exhaust_the_worker_stack() {
    let worker = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let sandbox = Sandbox::new();
            let source = sandbox.dir("deep");
            let leaf = sandbox.dir(&format!("deep{}", "/d".repeat(1_500)));
            fs::write(leaf.join("leaf.txt"), b"leaf").unwrap();
            let destination = sandbox.dir("destination");

            let outcome = transfer_paths_with_progress(
                std::slice::from_ref(&source),
                &destination,
                TransferMode::Copy,
                no_cancel(),
                Arc::new(TransferProgress::default()),
            );
            assert_clean(&outcome);
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
        for entry in fs::read_dir(&directory).into_iter().flatten().flatten() {
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

/// A file the user renamed to look like an abandoned quarantine would be
/// hidden by the browser and swept by the next Marcel to open the folder.
/// Every name Marcel reserves for itself is refused, not only the swept one,
/// so the rule stays one line long and needs no updating.
#[test]
fn rejects_names_reserved_for_marcels_working_files() {
    for name in [
        ".marcel-replaced-999999-0-notes",
        ".marcel-copy-1-0-staging",
        ".marcel-archive-abc",
        ".marcel-delete-1-0-thesis",
        ".marcel-recovered-0-report.txt",
        ".marcel-",
    ] {
        let error = validate_entry_name(name).unwrap_err().to_string();
        assert!(error.contains("reserved"), "{name:?}: {error}");
    }
    assert!(validate_entry_name("marcel-replaced-1-0-notes").is_ok());
    assert!(validate_entry_name(".marcel").is_ok());
    assert!(validate_entry_name(".marcelrc").is_ok());
}

#[test]
fn create_never_overwrites_an_occupied_destination() {
    let sandbox = Sandbox::new();
    let occupied = sandbox.file("occupied", b"keep me");

    assert!(create_directory(sandbox.root(), "occupied").is_err());
    assert_eq!(read(occupied), b"keep me");
}

#[test]
fn rename_is_no_replace_and_supports_undo_redo() {
    let sandbox = Sandbox::new();
    let source = sandbox.file("draft.txt", b"contents");
    let destination = sandbox.path("final.txt");

    let operation = recorded(rename_entry(&source, "final.txt").unwrap());
    assert!(!source.exists());
    assert_eq!(read(&destination), b"contents");
    assert_eq!(
        operation.forward_directory_changes(),
        DirectoryChanges { removed: vec![source.clone()], upserted: vec![destination.clone()] }
    );
    assert_eq!(
        operation.reverse_directory_changes(),
        DirectoryChanges { removed: vec![destination.clone()], upserted: vec![source.clone()] }
    );

    let redo_record = recorded(undo_operation(&operation).unwrap());
    assert_eq!(read(&source), b"contents");
    assert!(!destination.exists());

    let redone = recorded(redo_operation(&redo_record).unwrap());
    assert_eq!(redone.path(), destination);
    assert_eq!(read(&destination), b"contents");
}

#[test]
fn rename_accepts_an_invalid_utf8_source_identity() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};

    let sandbox = Sandbox::new();
    let source = sandbox.root().join(OsString::from_vec(vec![b'n', 0xff]));
    fs::write(&source, b"contents").unwrap();

    let operation = recorded(rename_entry(&source, "readable.txt").unwrap());

    assert!(!source.exists());
    assert_eq!(operation.path(), sandbox.path("readable.txt"));
    assert_eq!(read(sandbox.path("readable.txt")), b"contents");
}

#[test]
fn rename_refuses_an_occupied_destination() {
    let sandbox = Sandbox::new();
    let source = sandbox.file("source.txt", b"source");
    let destination = sandbox.file("occupied.txt", b"keep");

    assert!(rename_entry(&source, "occupied.txt").is_err());
    assert_eq!(read(&source), b"source");
    assert_eq!(read(&destination), b"keep");
}

#[test]
fn rename_undo_refuses_a_modified_result() {
    let sandbox = Sandbox::new();
    let source = sandbox.file("draft.txt", b"original");
    let operation = recorded(rename_entry(&source, "final.txt").unwrap());
    let destination = sandbox.file("final.txt", b"modified");

    assert!(undo_operation(&operation).is_err());
    assert!(!source.exists());
    assert_eq!(read(&destination), b"modified");
}

#[test]
fn set_mode_records_both_modes_and_supports_undo_redo() {
    use std::os::unix::fs::PermissionsExt as _;

    let sandbox = Sandbox::new();
    let script = sandbox.file("run.sh", b"#!/bin/sh\n");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
    let mode_of = |path: &Path| fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777;

    let operation = recorded(set_mode(&script, 0o755).unwrap());
    assert_eq!(mode_of(&script), 0o755);
    assert_eq!(
        operation.forward_directory_changes(),
        DirectoryChanges::upserted(vec![script.clone()])
    );
    assert!(matches!(operation, OperationRecord::SetMode { previous: 0o644, mode: 0o755, .. }));

    let redo_record = recorded(undo_operation(&operation).unwrap());
    assert_eq!(mode_of(&script), 0o644);
    assert!(matches!(redo_record, OperationRecord::SetMode { previous: 0o755, mode: 0o644, .. }));

    recorded(redo_operation(&redo_record).unwrap());
    assert_eq!(mode_of(&script), 0o755);
    assert_eq!(read(&script), b"#!/bin/sh\n");
}

/// The bits of a link belong to its target, which is not what the dialog
/// described; a request for no change is a refusal too, so nothing lands in
/// the journal for it.
#[test]
fn set_mode_refuses_links_and_unchanged_modes() {
    use std::os::unix::fs::PermissionsExt as _;

    let sandbox = Sandbox::new();
    let target = sandbox.file("target", b"x");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    let link = sandbox.path("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let error = set_mode(&link, 0o644).unwrap_err().to_string();
    assert!(error.contains("symbolic link"), "{error}");
    assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o7777, 0o600);

    let error = set_mode(&target, 0o600).unwrap_err().to_string();
    assert!(error.contains("unchanged"), "{error}");
}

/// Undo puts the old bits back only on the object whose bits were changed.
#[test]
fn set_mode_undo_refuses_a_replaced_object() {
    use std::os::unix::fs::PermissionsExt as _;

    let sandbox = Sandbox::new();
    let file = sandbox.file("notes", b"a");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    let operation = recorded(set_mode(&file, 0o600).unwrap());
    fs::remove_file(&file).unwrap();
    let replacement = sandbox.file("notes", b"b");
    fs::set_permissions(&replacement, fs::Permissions::from_mode(0o600)).unwrap();

    assert!(undo_operation(&operation).is_err());
    assert_eq!(fs::metadata(&replacement).unwrap().permissions().mode() & 0o7777, 0o600);
}

#[test]
fn create_undo_and_redo_validate_the_path() {
    let sandbox = Sandbox::new();
    let created = recorded(create_directory(sandbox.root(), "photos").unwrap());

    undo_operation(&created).unwrap();
    assert!(!created.path().exists());

    let recreated = recorded(redo_operation(&created).unwrap());
    assert!(recreated.path().is_dir());
}

#[test]
fn undo_refuses_a_non_empty_or_replaced_created_directory() {
    let sandbox = Sandbox::new();
    let created = recorded(create_directory(sandbox.root(), "work").unwrap());
    fs::write(created.path().join("important.txt"), b"data").unwrap();
    assert!(undo_operation(&created).is_err());
    assert_eq!(read(created.path().join("important.txt")), b"data");

    let created = recorded(create_directory(sandbox.root(), "replace-me").unwrap());
    fs::remove_dir(created.path()).unwrap();
    fs::create_dir(created.path()).unwrap();
    assert!(undo_operation(&created).is_err());
    assert!(created.path().is_dir());
}

#[test]
fn create_file_is_empty_no_replace_and_supports_undo_redo() {
    let sandbox = Sandbox::new();
    let occupied = sandbox.file("notes.txt", b"keep me");
    assert!(create_file(sandbox.root(), "notes.txt").is_err());
    assert_eq!(read(&occupied), b"keep me");

    let created = recorded(create_file(sandbox.root(), "todo.txt").unwrap());
    assert_eq!(created.path(), sandbox.path("todo.txt"));
    assert_eq!(read(created.path()), b"");
    assert_eq!(
        created.forward_directory_changes(),
        DirectoryChanges::upserted(vec![created.path().to_path_buf()])
    );

    let redo_record = recorded(undo_operation(&created).unwrap());
    assert!(!created.path().exists());

    let redone = recorded(redo_operation(&redo_record).unwrap());
    assert_eq!(read(redone.path()), b"");
}

/// Once the user has typed into the file it is theirs, and an undo that
/// deleted it would destroy work rather than reverse a creation.
#[test]
fn undo_refuses_a_written_or_replaced_created_file() {
    let sandbox = Sandbox::new();
    let created = recorded(create_file(sandbox.root(), "draft.md").unwrap());
    fs::write(created.path(), b"# Draft").unwrap();
    assert!(undo_operation(&created).is_err());
    assert_eq!(read(created.path()), b"# Draft");

    let created = recorded(create_file(sandbox.root(), "replaced").unwrap());
    fs::remove_file(created.path()).unwrap();
    fs::write(created.path(), b"").unwrap();
    assert!(undo_operation(&created).is_err());
    assert!(created.path().is_file());

    let created = recorded(create_file(sandbox.root(), "now-a-folder").unwrap());
    fs::remove_file(created.path()).unwrap();
    fs::create_dir(created.path()).unwrap();
    assert!(undo_operation(&created).is_err());
    assert!(created.path().is_dir());
}

#[test]
fn history_is_bounded_and_new_work_clears_redo() {
    let sandbox = Sandbox::new();
    let mut journal = OperationJournal::new(2);
    let first = recorded(create_directory(sandbox.root(), "first").unwrap());
    let second = recorded(create_directory(sandbox.root(), "second").unwrap());
    let third = recorded(create_directory(sandbox.root(), "third").unwrap());
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
    let sandbox = Sandbox::new();
    let album = sandbox.dir("source/album");
    sandbox.file("source/album/notes.txt", b"hello");
    std::os::unix::fs::symlink("notes.txt", album.join("notes-link")).unwrap();
    let destination = sandbox.dir("destination");

    let outcome = copy(std::slice::from_ref(&album), &destination);
    assert_clean(&outcome);
    assert_eq!(read(destination.join("album/notes.txt")), b"hello");
    assert_eq!(
        fs::read_link(destination.join("album/notes-link")).unwrap(),
        PathBuf::from("notes.txt")
    );
    assert_eq!(read(album.join("notes.txt")), b"hello");

    let operation = outcome.operation.unwrap();
    assert_eq!(
        operation.forward_directory_changes(),
        DirectoryChanges::upserted(vec![destination.join("album")])
    );
    let redo_record = recorded(undo_operation(&operation).unwrap());
    assert!(!destination.join("album").exists());
    let redone = recorded(redo_operation(&redo_record).unwrap());
    assert_eq!(read(redone.path().join("notes.txt")), b"hello");
}

#[test]
fn copy_never_overwrites_an_occupied_destination() {
    let sandbox = Sandbox::new();
    let source = sandbox.file("source.txt", b"new");
    let destination = sandbox.dir("destination");
    sandbox.file("destination/source.txt", b"keep");

    let outcome = copy(&[source], &destination);
    assert_eq!(outcome.failures.len(), 1);
    assert!(outcome.operation.is_none());
    assert_eq!(read(destination.join("source.txt")), b"keep");
}

/// Two sources, one of which collides at the destination.
fn occupied_transfer_fixture() -> (Sandbox, Vec<PathBuf>, PathBuf) {
    let sandbox = Sandbox::new();
    let sources =
        vec![sandbox.file("source/taken.txt", b"new"), sandbox.file("source/free.txt", b"free")];
    sandbox.file("destination/taken.txt", b"keep");
    let destination = sandbox.path("destination");
    (sandbox, sources, destination)
}

/// Skipping is a deliberate outcome, not a failure, and it must not stop
/// the sources that follow it.
#[test]
fn a_skipped_conflict_leaves_both_items_and_continues() {
    let (_sandbox, sources, destination) = occupied_transfer_fixture();

    let outcome = transfer_deciding(
        &sources,
        &destination,
        TransferMode::Copy,
        ConflictDecision::once(ConflictResponse::Skip),
    );

    assert_eq!(outcome.skipped, [sources[0].clone()]);
    assert_clean(&outcome);
    assert_eq!(outcome.completed.len(), 1);
    assert_eq!(read(destination.join("taken.txt")), b"keep");
    assert_eq!(read(destination.join("free.txt")), b"free");
    assert_eq!(outcome.accounted(), sources.len());
}

/// Cancelling from a conflict abandons the operation, and every source it
/// never reached is accounted for rather than silently forgotten.
#[test]
fn cancelling_a_conflict_accounts_for_every_unattempted_source() {
    let (_sandbox, sources, destination) = occupied_transfer_fixture();

    let outcome = transfer_deciding(
        &sources,
        &destination,
        TransferMode::Copy,
        ConflictDecision::once(ConflictResponse::Cancel),
    );

    assert_eq!(outcome.cancelled, sources);
    assert!(outcome.completed.is_empty());
    assert_clean(&outcome);
    assert!(!destination.join("free.txt").exists());
    assert_eq!(outcome.accounted(), sources.len());
}

#[test]
fn renaming_resolves_a_conflict_without_touching_the_occupant() {
    let (_sandbox, sources, destination) = occupied_transfer_fixture();
    let rename = ConflictDecision::once(ConflictResponse::Rename("renamed.txt".into()));

    let outcome = transfer_deciding(&sources, &destination, TransferMode::Copy, rename);

    assert_clean(&outcome);
    assert_eq!(read(destination.join("taken.txt")), b"keep");
    assert_eq!(read(destination.join("renamed.txt")), b"new");
    assert_eq!(outcome.accounted(), sources.len());
}

/// A chosen name gets the same scrutiny as one typed into Rename, so a
/// resolver cannot smuggle a path separator past the destination directory.
#[test]
fn a_rename_response_cannot_escape_the_destination_directory() {
    let (_sandbox, sources, destination) = occupied_transfer_fixture();
    let rename = ConflictDecision::once(ConflictResponse::Rename("../escaped.txt".into()));

    let outcome = transfer_deciding(&sources, &destination, TransferMode::Copy, rename);

    assert_eq!(outcome.failures.len(), 1);
    assert!(outcome.failures[0].message.contains("cannot contain"), "{:?}", outcome.failures);
    assert!(!destination.parent().unwrap().join("escaped.txt").exists());
    assert_eq!(outcome.accounted(), sources.len());
}

/// A crash cannot run the exit path, so the remnants it leaves have to be
/// reclaimed later. A dead owner's quarantine can never be restored, which
/// makes it unreachable garbage rather than data anyone might want — but a
/// live owner (this process, or its parent) can still restore its own, a
/// name from another boot proves nothing about who made it, and recovery
/// remnants are never anyone's to sweep.
#[test]
fn the_abandoned_sweep_reclaims_only_dead_owners_undo_storage_from_this_boot() {
    let sandbox = Sandbox::new();
    let boot = boot_id();
    // Process id 0 is never a real process, so it stands in for a Marcel
    // that is gone.
    let abandoned = sandbox.file(&format!(".marcel-replaced-{boot}-0-0-report.txt"), b"old");
    let mine = sandbox
        .file(&format!(".marcel-replaced-{boot}-{}-0-report.txt", std::process::id()), b"payload");
    let parents = sandbox.file(
        &format!(".marcel-replaced-{boot}-{}-0-report.txt", std::os::unix::process::parent_id()),
        b"payload",
    );
    // The same dead pid under another boot id: restored from a backup, or
    // from a machine whose pid 0 means something else entirely.
    let other_boot = "f".repeat(32);
    let foreign = sandbox.file(&format!(".marcel-replaced-{other_boot}-0-0-report.txt"), b"?");
    // The shape Marcel wrote before boot ids, which no longer parses as its.
    let legacy = sandbox.file(".marcel-replaced-0-0-report.txt", b"?");
    let preserved = sandbox.file(".marcel-recovered-0-report.txt", b"ORIGINAL");
    let ordinary = sandbox.file("report.txt", b"payload");

    assert_eq!(reclaim_abandoned_quarantines(sandbox.root()), 1);

    assert!(!abandoned.exists(), "a dead owner's undo storage is garbage");
    assert!(mine.exists(), "this process can still undo, so its quarantine stays");
    assert!(parents.exists(), "liveness is consulted, not equality with this process");
    assert!(foreign.exists(), "another boot's pids are not this boot's");
    assert!(legacy.exists(), "a name without a boot id is nobody's to sweep");
    assert_eq!(read(&preserved), b"ORIGINAL");
    assert!(ordinary.exists(), "user data is never touched");
}

/// The defect this pair of names exists to prevent: a transfer that fails
/// after quarantining its destination, whose restoration then also fails,
/// is holding the user's only copy in storage a later Marcel would sweep.
#[test]
fn a_failed_restoration_preserves_the_original_in_recovery_storage() {
    let sandbox = Sandbox::new();
    let source = sandbox.file("source/report.txt", b"NEW");
    sandbox.file("destination/report.txt", b"ORIGINAL");
    let destination = sandbox.path("destination");

    // One name, three renames: the quarantine succeeds because it targets a
    // hidden name, then publication and restoration both fail.
    let _fault = fault::fail_renames_to("report.txt");
    let outcome = replacing(std::slice::from_ref(&source), &destination);

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
    assert_eq!(read(&preserved[0]), b"ORIGINAL");
}

/// Marcel created the quarantine path by atomic rename, which says who held
/// it then and nothing about who holds it at eviction.
#[test]
fn quarantine_deletion_refuses_an_object_it_did_not_record() {
    let sandbox = Sandbox::new();
    let quarantine = sandbox.file(".marcel-replaced-1-0-report.txt", b"ORIGINAL");
    let item = ReplacedItem {
        path: sandbox.path("report.txt"),
        quarantine: quarantine.clone(),
        identity: FileIdentity::read(&quarantine).unwrap(),
    };

    // Someone else takes the name in the meantime.
    fs::remove_file(&quarantine).unwrap();
    fs::write(&quarantine, b"SOMEONE ELSE'S").unwrap();
    erase_replacement_quarantine(&item);
    assert_eq!(read(&quarantine), b"SOMEONE ELSE'S");

    // The object it actually recorded is released as before.
    let recorded = ReplacedItem { identity: FileIdentity::read(&quarantine).unwrap(), ..item };
    erase_replacement_quarantine(&recorded);
    assert!(!quarantine.exists());
}

/// Marcel's own bookkeeping must never be why an operation the filesystem
/// would have allowed fails.
#[test]
fn a_replacement_of_a_name_near_the_length_limit_succeeds() {
    let sandbox = Sandbox::new();
    let long = "l".repeat(250);
    let source = sandbox.file(&format!("source/{long}"), b"NEW");
    let replaced = sandbox.file(&format!("destination/{long}"), b"ORIGINAL");

    let outcome = replacing(std::slice::from_ref(&source), &sandbox.path("destination"));

    assert_clean(&outcome);
    assert_eq!(read(&replaced), b"NEW");
    // And the replacement is still reversible.
    undo_operation(&outcome.operation.expect("a replacement records undo")).unwrap();
    assert_eq!(read(&replaced), b"ORIGINAL");

    let name = quarantined_name(".marcel-replaced-4194304-", 9, OsStr::new(&"é".repeat(200)));
    use std::os::unix::ffi::OsStrExt as _;
    assert!(name.as_bytes().len() <= MAX_NAME_BYTES, "{name:?}");
    assert!(name.to_str().is_some(), "truncation stays on a character boundary: {name:?}");
}

/// Refreshing an identity after a commit re-reads a path, and a path is not
/// an object. Adopting whatever is there would enter a stranger's file into
/// a record Undo is entitled to delete.
#[test]
fn an_identity_refresh_refuses_an_object_it_did_not_commit() {
    let sandbox = Sandbox::new();
    let path = sandbox.file("published.txt", b"committed");
    let mut snapshots = snapshot_tree(&path).unwrap();

    // The same path, a different object, published the way anything is
    // published atomically. Both files exist at once, so the replacement
    // cannot be handed the inode number the original still holds.
    let replacement = sandbox.file("elsewhere.txt", b"someone else's");
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
    let mine = format!(".marcel-replaced-{}-1-0-report.txt", boot_id());
    for name in [mine.as_str(), ".marcel-copy-1-0-abc", ".marcel-archive-abc"] {
        assert!(is_internal_working_name(OsStr::new(name)), "{name}");
    }
    // Recovery guidance points the user straight at the first of these, and
    // nothing will ever sweep the next two, so the browser shows them too.
    let other_boot = format!(".marcel-replaced-{}-1-0-report.txt", "a".repeat(32));
    for name in [
        ".marcel-delete-1-0-report.txt",
        other_boot.as_str(),
        ".marcel-replaced-1-0-report.txt",
        "report.txt",
        ".hidden",
        "marcel-replaced-1-0",
    ] {
        assert!(!is_internal_working_name(OsStr::new(name)), "{name}");
    }
    assert!(is_quarantine_from_another_boot(OsStr::new(&other_boot)));
    assert!(!is_quarantine_from_another_boot(OsStr::new(&mine)));
    assert!(!is_quarantine_from_another_boot(OsStr::new(".marcel-replaced-1-0-report.txt")));
}

/// A record pushed out of the journal can never be undone, so what it was
/// holding aside stops being recoverable data and becomes a hidden file
/// nobody would ever collect.
#[test]
fn evicting_a_record_releases_the_data_it_was_holding() {
    let sandbox = Sandbox::new();
    let source = sandbox.file("source/report.txt", b"replacement");
    sandbox.file("destination/report.txt", b"the original");
    let destination = sandbox.path("destination");
    let outcome = replacing(std::slice::from_ref(&source), &destination);
    let replacing = outcome.operation.expect("a replacement retains undo");
    assert!(!no_replacement_quarantines(&destination));

    let mut journal = OperationJournal::new(1);
    assert!(journal.record(replacing).is_empty());
    // A second record displaces the first, which can now never be undone.
    let evicted = journal.record(recorded(create_directory(sandbox.root(), "later").unwrap()));

    assert_eq!(evicted.len(), 1);
    for record in &evicted {
        record.release_quarantines();
    }
    assert!(
        no_replacement_quarantines(&destination),
        "an unreachable record must not keep holding disk"
    );
    // The replacement itself is untouched; only its way back is gone.
    assert_eq!(read(destination.join("report.txt")), b"replacement");
}

/// The whole promise of replacement: what it displaced comes back. Nautilus
/// overwrites in place, so its undo cannot do this at all.
#[test]
fn undoing_a_replacement_puts_the_displaced_file_back() {
    let sandbox = Sandbox::new();
    let source = sandbox.file("source/report.txt", b"replacement");
    let replaced = sandbox.file("destination/report.txt", b"the original");
    let destination = sandbox.path("destination");

    let outcome = replacing(std::slice::from_ref(&source), &destination);

    assert_clean(&outcome);
    assert!(!outcome.undo_unavailable);
    assert_eq!(read(&replaced), b"replacement");
    undo_operation(&outcome.operation.expect("a replacement retains undo")).unwrap();
    assert_eq!(
        read(&replaced),
        b"the original",
        "undo must restore what the replacement displaced"
    );
    assert!(no_replacement_quarantines(&destination));
}

/// A transfer that fails after displacing must put the displaced item back
/// rather than leaving the destination empty.
#[test]
fn a_failed_replacement_restores_what_it_displaced() {
    let sandbox = Sandbox::new();
    // A socket cannot be copied, so the transfer fails after the
    // destination has already been moved aside.
    let source = sandbox.dir("source/payload");
    let _listener = sandbox.socket("source/payload/daemon.sock");
    let replaced = sandbox.file("destination/payload", b"the original");
    let destination = sandbox.path("destination");

    let outcome = replacing(std::slice::from_ref(&source), &destination);

    assert_eq!(outcome.failures.len(), 1, "{outcome:?}");
    assert_eq!(
        read(&replaced),
        b"the original",
        "a failed replacement must not consume the original"
    );
    assert!(no_replacement_quarantines(&destination));
}

/// Past the byte budget the replacement still happens; it simply stops
/// being reversible, and the quarantine is released rather than held.
#[test]
fn an_oversized_replacement_succeeds_without_undo() {
    let sandbox = Sandbox::new();
    let source = sandbox.file("source/blob.bin", b"replacement");
    let replaced = sandbox.file("destination/blob.bin", vec![0_u8; 4096]);
    let destination = sandbox.path("destination");
    let budget = TransferBudget { replacement_undo_byte_limit: 1024, ..TransferBudget::default() };

    let outcome = budgeted(std::slice::from_ref(&source), &destination, TransferMode::Copy, budget);

    assert_clean(&outcome);
    assert!(outcome.undo_unavailable, "an oversized replacement cannot be undone");
    assert_eq!(read(&replaced), b"replacement");
    assert!(
        no_replacement_quarantines(&destination),
        "an unreachable quarantine must be released, not left on disk"
    );
}

/// Choosing merge for two directories must never discard what the
/// destination already holds, even when the source has nothing to add.
#[test]
fn merging_an_empty_directory_keeps_the_destination_intact() {
    let sandbox = Sandbox::new();
    let shared = sandbox.dir("source/shared");
    let kept = sandbox.file("destination/shared/keep.txt", b"keep");
    let merge_all = ConflictDecision::for_all(ConflictResponse::Replace);

    let outcome = transfer_deciding(
        std::slice::from_ref(&shared),
        &sandbox.path("destination"),
        TransferMode::Copy,
        merge_all,
    );

    assert_clean(&outcome);
    assert_eq!(read(&kept), b"keep");
    // Nothing was added, so there is nothing to undo.
    assert!(outcome.operation.is_none());
}

/// Renaming everything keeps every source, each beside the item it
/// collided with, without a single item being lost or overwritten.
#[test]
fn renaming_all_keeps_every_colliding_source() {
    let sandbox = Sandbox::new();
    let sources = vec![sandbox.file("source/a.txt", b"new"), sandbox.file("source/b.txt", b"new")];
    sandbox.file("destination/a.txt", b"existing");
    sandbox.file("destination/b.txt", b"existing");
    // Already occupied, so "a.txt" has to land past it.
    sandbox.file("destination/a (2).txt", b"existing too");
    let destination = sandbox.path("destination");
    let rename_all = ConflictDecision::for_all(ConflictResponse::AutoRename);

    let outcome = transfer_deciding(&sources, &destination, TransferMode::Copy, rename_all);

    assert_clean(&outcome);
    assert_eq!(outcome.completed.len(), 2);
    assert_eq!(outcome.accounted(), sources.len());
    // Nothing that was already there changed.
    assert_eq!(read(destination.join("a.txt")), b"existing");
    assert_eq!(read(destination.join("b.txt")), b"existing");
    assert_eq!(read(destination.join("a (2).txt")), b"existing too");
    // Both sources arrived beside them.
    assert_eq!(read(destination.join("a (3).txt")), b"new");
    assert_eq!(read(destination.join("b (2).txt")), b"new");
}

/// Merging is the union of two trees: the destination keeps everything it
/// has and gains what it lacks. Nothing is displaced, which is what makes
/// undo exact rather than approximate.
#[test]
fn merging_adds_what_is_missing_and_keeps_what_is_there() {
    let sandbox = Sandbox::new();
    // Source tree: a colliding file, a new file, a colliding subdirectory
    // holding a new file, and a wholly new subdirectory.
    let photos = sandbox.dir("source/photos");
    sandbox.file("source/photos/shared.txt", b"NEW");
    sandbox.file("source/photos/only-in-source.txt", b"NEW");
    sandbox.file("source/photos/holiday/beach.txt", b"NEW");
    sandbox.file("source/photos/new-album/cover.txt", b"NEW");
    // Destination tree: the same directory, one colliding file, one of its
    // own, and the colliding subdirectory with its own contents.
    sandbox.file("destination/photos/shared.txt", b"ORIGINAL");
    sandbox.file("destination/photos/only-in-destination.txt", b"ORIGINAL");
    sandbox.file("destination/photos/holiday/sunset.txt", b"ORIGINAL");
    let merged = sandbox.path("destination/photos");

    let outcome = replacing(std::slice::from_ref(&photos), &sandbox.path("destination"));

    assert_clean(&outcome);
    let untouched = || {
        assert_eq!(read(merged.join("shared.txt")), b"ORIGINAL");
        assert_eq!(read(merged.join("only-in-destination.txt")), b"ORIGINAL");
        assert_eq!(read(merged.join("holiday/sunset.txt")), b"ORIGINAL");
    };
    // Everything the destination already had is untouched, and everything
    // it lacked has arrived, at every depth.
    untouched();
    assert_eq!(read(merged.join("only-in-source.txt")), b"NEW");
    assert_eq!(read(merged.join("holiday/beach.txt")), b"NEW");
    assert_eq!(read(merged.join("new-album/cover.txt")), b"NEW");

    // Undo removes exactly what arrived and nothing else.
    undo_operation(&outcome.operation.expect("a merge retains undo")).unwrap();
    untouched();
    assert!(!merged.join("only-in-source.txt").exists());
    assert!(!merged.join("holiday/beach.txt").exists());
    assert!(!merged.join("new-album").exists());
    // The source is a copy source, so it is left exactly as it was.
    assert_eq!(read(photos.join("shared.txt")), b"NEW");
}

/// Undo of a merge validates each item on its own, so something added
/// inside the merged tree afterwards is left alone rather than deleted.
#[test]
fn undoing_a_merge_leaves_later_additions_alone() {
    let sandbox = Sandbox::new();
    let photos = sandbox.dir("source/photos");
    sandbox.file("source/photos/arrived.txt", b"NEW");
    sandbox.dir("destination/photos");

    let outcome = replacing(std::slice::from_ref(&photos), &sandbox.path("destination"));
    let operation = outcome.operation.expect("a merge retains undo");
    // Someone adds a file to the merged directory afterwards.
    let later = sandbox.file("destination/photos/added-later.txt", b"MINE");

    undo_operation(&operation).unwrap();

    assert!(!sandbox.path("destination/photos/arrived.txt").exists());
    assert_eq!(read(&later), b"MINE");
}

/// The merge's removals commit one at a time, so an undo that will refuse
/// the copy's own output must refuse *before* removing them. It used to
/// remove the merge's additions, then report "nothing happened" and keep a
/// record that could never validate again.
#[test]
fn copy_undo_with_a_merge_refuses_a_modified_output_before_removing_anything() {
    let sandbox = Sandbox::new();
    sandbox.file("source/new_item/inside.txt", b"NEW");
    sandbox.file("source/photos/arrived.txt", b"NEW");
    sandbox.dir("destination/photos");
    let sources = [sandbox.path("source/new_item"), sandbox.path("source/photos")];

    let outcome = replacing(&sources, &sandbox.path("destination"));
    let operation = outcome.operation.expect("the transfer retains undo");
    // Someone adds a file to the copied output afterwards, so its
    // recorded tree no longer matches the disk.
    sandbox.file("destination/new_item/added-later.txt", b"MINE");

    let result = undo_operation(&operation);

    assert!(
        matches!(result, MutationOutcome::Unchanged(_)),
        "a refusal raised before anything was removed must stay retryable: {result:?}"
    );
    assert_eq!(
        read(sandbox.path("destination/photos/arrived.txt")),
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
    if skip_as_root() {
        return;
    }
    let sandbox = Sandbox::new();
    let sources = [sandbox.file("source/new_item.txt", b"NEW"), sandbox.dir("source/photos")];
    sandbox.file("source/photos/arrived.txt", b"NEW");
    sandbox.dir("destination/photos");
    let destination = sandbox.path("destination");

    let outcome = replacing(&sources, &destination);
    let operation = outcome.operation.expect("the transfer retains undo");

    // The merge's additions can still be removed, but the copied output
    // cannot leave its now read-only parent.
    seal(&destination, true);
    let result = undo_operation(&operation);
    seal(&destination, false);

    let MutationOutcome::Discarded { changes, .. } = result else {
        panic!("an undo that removed the merge's additions must discard: {result:?}");
    };
    let arrived = destination.join("photos/arrived.txt");
    assert!(
        changes.removed.contains(&arrived),
        "the removals that committed must be reported: {changes:?}"
    );
    assert!(!arrived.exists(), "this scenario depends on the merge's additions being removed");
    assert_eq!(
        read(destination.join("new_item.txt")),
        b"NEW",
        "the copied output could not be removed and must survive"
    );
}

/// A merge that stops part way has still added part of what it planned.
/// Returning a bare failure would tell the caller the disk is unchanged
/// while half a merge sits in the destination with no way to take it back.
#[test]
fn a_merge_stopped_by_failure_records_what_it_added() {
    let sandbox = Sandbox::new();
    let photos = sandbox.dir("source/photos");
    sandbox.file("source/photos/arrives.txt", b"NEW");
    sandbox.file("source/photos/blocked.txt", b"NEW");
    sandbox.file("source/photos/album/inside.txt", b"NEW");
    let kept = sandbox.file("destination/photos/keep.txt", b"ORIGINAL");
    let merged = sandbox.path("destination/photos");

    // Publishing this one leaf fails, after the merge has already created a
    // directory and published a file.
    let _fault = fault::fail_renames_to("blocked.txt");
    let outcome = replacing(std::slice::from_ref(&photos), &sandbox.path("destination"));

    assert_eq!(outcome.failures.len(), 1, "{outcome:?}");
    assert!(outcome.completed.is_empty(), "{outcome:?}");
    assert_eq!(read(merged.join("arrives.txt")), b"NEW");
    assert!(merged.join("album").is_dir());

    // The partial merge is describable, so Undo can take back exactly what
    // arrived and leave what the destination already had.
    undo_operation(&outcome.operation.expect("a partial merge still records its additions"))
        .unwrap();

    assert!(!merged.join("arrives.txt").exists());
    assert!(!merged.join("album").exists());
    assert_eq!(read(&kept), b"ORIGINAL");
}

/// Cancelling is an answer, not a fault. A merge that reports cancellation
/// as a failure tells the user their merge broke, and letting the loop
/// continue attempts work they just stopped.
#[test]
fn a_cancelled_merge_is_reported_as_cancellation() {
    let sandbox = Sandbox::new();
    let sources = vec![sandbox.dir("source/photos"), sandbox.dir("source/later")];
    sandbox.file("source/photos/arrives.txt", b"NEW");
    sandbox.dir("destination/photos");

    let outcome = transfer_paths_with_conflicts(
        &sources,
        &sandbox.path("destination"),
        TransferMode::Copy,
        Arc::new(AtomicBool::new(true)),
        Arc::new(TransferProgress::default()),
        &mut answering(ConflictDecision::for_all(ConflictResponse::Replace)),
    );

    assert!(outcome.failures.is_empty(), "cancelling is not a failure: {outcome:?}");
    assert_eq!(outcome.cancelled, sources, "{outcome:?}");
    assert!(!sandbox.path("destination/photos/arrives.txt").exists());
}

/// The snapshot budget bounds one operation, so a merge cannot help itself
/// to a fresh allowance per leaf — and a move never refuses for its sake,
/// since a rename does not care how big the tree is. Past it the work still
/// happens and says it cannot be undone.
#[test]
fn work_past_the_snapshot_budget_succeeds_without_undo() {
    let sandbox = Sandbox::new();
    let photos = sandbox.dir("source/photos");
    let album = sandbox.dir("source/album");
    let plain = sandbox.dir("source/plain");
    for index in 0..4 {
        sandbox.file(&format!("source/photos/{index}.txt"), b"NEW");
        sandbox.file(&format!("source/album/{index}.txt"), b"payload");
    }
    sandbox.file("source/plain/one", b"1");
    sandbox.file("source/plain/two", b"2");
    sandbox.dir("destination/photos");
    let destination = sandbox.path("destination");
    let budget = TransferBudget { undo_snapshot_limit: 2, ..TransferBudget::default() };

    let merged = budgeted(std::slice::from_ref(&photos), &destination, TransferMode::Copy, budget);
    assert_clean(&merged);
    assert_eq!(merged.completed.len(), 1, "{merged:?}");
    assert!(merged.undo_unavailable, "a merge past the budget is not undoable: {merged:?}");
    for index in 0..4 {
        assert!(
            destination.join(format!("photos/{index}.txt")).exists(),
            "the merge still happens"
        );
    }

    let moved = budgeted(std::slice::from_ref(&album), &destination, TransferMode::Move, budget);
    assert_clean(&moved);
    assert_eq!(moved.completed.len(), 1, "{moved:?}");
    assert!(moved.undo_unavailable, "a move past the budget is not undoable: {moved:?}");
    assert!(moved.operation.is_none(), "{moved:?}");
    assert!(!album.exists(), "the move still happens");
    assert_eq!(read(destination.join("album/0.txt")), b"payload");

    let copied = budgeted(std::slice::from_ref(&plain), &destination, TransferMode::Copy, budget);
    assert_clean(&copied);
    assert!(copied.undo_unavailable);
    assert!(copied.operation.is_none());
    assert_eq!(read(destination.join("plain/one")), b"1");
    assert_eq!(read(destination.join("plain/two")), b"2");
}

/// Moving cannot express a merge yet, so it refuses rather than discarding
/// the tree the user expected to be joined.
#[test]
fn moving_a_directory_onto_a_directory_is_refused() {
    let sandbox = Sandbox::new();
    let photos = sandbox.dir("source/photos");
    let kept = sandbox.file("destination/photos/keep.txt", b"keep");
    let merge_all = ConflictDecision::for_all(ConflictResponse::Replace);

    let outcome = transfer_deciding(
        std::slice::from_ref(&photos),
        &sandbox.path("destination"),
        TransferMode::Move,
        merge_all,
    );

    assert_eq!(outcome.failures.len(), 1, "{outcome:?}");
    assert_eq!(read(&kept), b"keep");
    assert!(photos.exists());
}

/// Dropping a selection onto the folder it already lives in asks for
/// nothing. Refusing it would invent a problem the user does not have, and
/// reporting it as skipped would claim they declined something.
#[test]
fn moving_an_item_where_it_already_is_does_nothing() {
    let sandbox = Sandbox::new();
    let source = sandbox.file("folder/report.txt", b"payload");
    let replace_all = ConflictDecision::for_all(ConflictResponse::Replace);

    let outcome = transfer_deciding(
        std::slice::from_ref(&source),
        &sandbox.path("folder"),
        TransferMode::Move,
        replace_all,
    );

    assert_eq!(outcome.already_in_place, std::slice::from_ref(&source));
    assert_clean(&outcome);
    assert!(outcome.skipped.is_empty());
    assert!(outcome.completed.is_empty());
    assert!(outcome.operation.is_none());
    assert_eq!(read(&source), b"payload");
    assert_eq!(outcome.accounted(), 1);
}

/// Copying a file into the folder it lives in is a request to duplicate it,
/// and it has one sensible answer, so it is answered rather than asked.
#[test]
fn copying_an_item_into_its_own_folder_duplicates_it_without_asking() {
    let sandbox = Sandbox::new();
    let source = sandbox.file("folder/report.txt", b"payload");
    let folder = sandbox.path("folder");
    // A resolver that would abandon the transfer if consulted: this must
    // not ask.
    let would_cancel = ConflictDecision::once(ConflictResponse::Cancel);

    let outcome = transfer_deciding(
        std::slice::from_ref(&source),
        &folder,
        TransferMode::Copy,
        would_cancel.clone(),
    );

    assert_clean(&outcome);
    assert_eq!(outcome.completed.len(), 1);
    assert_eq!(read(&source), b"payload");
    assert_eq!(
        read(folder.join("report (2).txt")),
        b"payload",
        "the duplicate lands beside the original"
    );
    // Duplicating again steps past the name it just created.
    let outcome =
        transfer_deciding(std::slice::from_ref(&source), &folder, TransferMode::Copy, would_cancel);
    assert_clean(&outcome);
    assert!(folder.join("report (3).txt").exists());
}

/// A hard link names the same object under a different path, so comparing
/// paths would miss it. Replacing there would quarantine the very object
/// about to be renamed, so a move refuses; a copy is safe because it never
/// destroys the source, and duplicating is what was asked for.
#[test]
fn a_hardlink_to_the_source_is_recognized_as_the_same_object() {
    let sandbox = Sandbox::new();
    let source = sandbox.file("source/report.pdf", b"original");
    let destination = sandbox.dir("destination");
    // Same inode, different directory, same basename: the transfer would
    // land exactly on its own source.
    fs::hard_link(&source, destination.join("report.pdf")).unwrap();
    let replace_all = ConflictDecision::for_all(ConflictResponse::Replace);

    let moved = transfer_deciding(
        std::slice::from_ref(&source),
        &destination,
        TransferMode::Move,
        replace_all.clone(),
    );
    assert_eq!(moved.failures.len(), 1, "{moved:?}");
    assert!(moved.failures[0].message.contains("over itself"), "{:?}", moved.failures);
    assert_eq!(read(&source), b"original");

    let copied = transfer_deciding(
        std::slice::from_ref(&source),
        &destination,
        TransferMode::Copy,
        replace_all,
    );
    assert_clean(&copied);
    assert_eq!(read(&source), b"original");
    // The existing link is untouched and the duplicate lands beside it.
    assert_eq!(read(destination.join("report.pdf")), b"original");
    assert_eq!(read(destination.join("report (2).pdf")), b"original");
}

/// Cancelling the operation through the cancel flag must also account for
/// the sources it never reached, and publishes nothing.
#[test]
fn a_cancelled_transfer_accounts_for_every_requested_source() {
    let (_sandbox, sources, destination) = occupied_transfer_fixture();

    let outcome =
        transfer_paths(&sources, &destination, TransferMode::Copy, Arc::new(AtomicBool::new(true)));

    assert_eq!(outcome.cancelled, sources);
    assert_eq!(outcome.accounted(), sources.len());
    assert!(outcome.operation.is_none());
    assert!(!destination.join("free.txt").exists());
}

#[test]
fn copy_undo_refuses_a_modified_output() {
    let sandbox = Sandbox::new();
    let source = sandbox.file("source.txt", b"original");
    let destination = sandbox.dir("destination");
    let operation = copy(&[source], &destination).operation.unwrap();
    let changed = sandbox.file("destination/source.txt", b"changed");

    assert!(undo_operation(&operation).is_err());
    assert_eq!(read(&changed), b"changed");
}

#[test]
fn copy_undo_refuses_added_children_without_partially_removing_output() {
    let sandbox = Sandbox::new();
    let source = sandbox.dir("source");
    sandbox.file("source/original.txt", b"original");
    let destination = sandbox.dir("destination");
    let operation = copy(&[source], &destination).operation.unwrap();
    let added = sandbox.file("destination/source/added-later.txt", b"keep");

    assert!(undo_operation(&operation).is_err());
    assert_eq!(read(destination.join("source/original.txt")), b"original");
    assert_eq!(read(&added), b"keep");
}

#[test]
fn copy_redo_refuses_new_source_children_without_publishing_output() {
    let sandbox = Sandbox::new();
    let source = sandbox.dir("source");
    sandbox.file("source/original.txt", b"original");
    let destination = sandbox.dir("destination");
    let outcome = copy(std::slice::from_ref(&source), &destination);
    let redo_record = recorded(undo_operation(&outcome.operation.unwrap()).unwrap());
    sandbox.file("source/added-later.txt", b"new");

    assert!(redo_operation(&redo_record).is_err());
    assert!(!destination.join("source").exists());
}

#[test]
fn move_supports_identity_checked_undo_and_redo() {
    let sandbox = Sandbox::new();
    let source = sandbox.file("source/move-me.txt", b"contents");
    let destination = sandbox.dir("destination");

    let outcome = mv(std::slice::from_ref(&source), &destination);
    assert_clean(&outcome);
    let operation = outcome.operation.unwrap();
    assert!(!source.exists());
    assert_eq!(
        operation.forward_directory_changes(),
        DirectoryChanges {
            removed: vec![source.clone()],
            upserted: vec![destination.join("move-me.txt")]
        }
    );

    let redo_record = recorded(undo_operation(&operation).unwrap());
    assert_eq!(read(&source), b"contents");
    let redone = recorded(redo_operation(&redo_record).unwrap());
    assert_eq!(read(redone.path()), b"contents");
    assert!(!source.exists());
}

#[test]
fn move_refuses_to_put_a_directory_inside_itself() {
    let sandbox = Sandbox::new();
    let source = sandbox.dir("source");
    let descendant = sandbox.dir("source/descendant");

    let outcome = mv(std::slice::from_ref(&source), &descendant);
    assert_eq!(outcome.failures.len(), 1);
    assert!(source.is_dir());
}

#[test]
fn copy_reports_item_and_byte_progress() {
    let sandbox = Sandbox::new();
    let source = sandbox.file("source.txt", b"marcel");
    let progress = Arc::new(TransferProgress::default());

    let outcome = transfer_paths_with_progress(
        &[source],
        &sandbox.dir("destination"),
        TransferMode::Copy,
        no_cancel(),
        progress.clone(),
    );

    assert_clean(&outcome);
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

    let sandbox = Sandbox::new();
    let tree = sandbox.dir("source/tree");
    let file = sandbox.file("source/tree/script.sh", b"#!/bin/sh\n");
    fs::set_permissions(&tree, fs::Permissions::from_mode(0o750)).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
    let accessed = UNIX_EPOCH + Duration::from_secs(1_650_000_000);
    let modified = UNIX_EPOCH + Duration::from_secs(1_650_000_123);
    let times = fs::FileTimes::new().set_accessed(accessed).set_modified(modified);
    fs::File::open(&file).unwrap().set_times(times).unwrap();
    fs::File::open(&tree).unwrap().set_times(times).unwrap();
    let destination = sandbox.dir("destination");

    assert_clean(&copy(std::slice::from_ref(&tree), &destination));

    let tree_metadata = fs::metadata(destination.join("tree")).unwrap();
    let file_metadata = fs::metadata(destination.join("tree/script.sh")).unwrap();
    assert_eq!(tree_metadata.permissions().mode() & 0o7777, 0o750);
    assert_eq!(file_metadata.permissions().mode() & 0o7777, 0o640);
    assert_eq!(tree_metadata.modified().unwrap(), modified);
    assert_eq!(file_metadata.modified().unwrap(), modified);
    assert_eq!(file_metadata.accessed().unwrap(), accessed);
}

/// A mode without the owner's read bit is legal on a source Marcel can still
/// read through its group or world bits — a root-owned `0044` file, say —
/// and the copy Marcel owns must end up with that same mode. It only forbids
/// opening the copy afterwards, and applying the mode before the timestamps
/// failed exactly there, after every byte was in place.
#[test]
fn copy_applies_modes_that_forbid_reading_the_copy_back_after_its_times() {
    use std::os::unix::fs::PermissionsExt as _;

    if skip_as_root() {
        return;
    }
    let sandbox = Sandbox::new();
    let modified = UNIX_EPOCH + Duration::from_secs(1_650_000_123);
    let times = fs::FileTimes::new().set_modified(modified);

    // The walker holds each source open from before it reads it, so the
    // handles here predate the modes that would refuse a fresh open.
    let locked = sandbox.file("lock", b"held");
    let mut locked_handle = fs::File::open(&locked).unwrap();
    let copied_lock = sandbox.path("lock copy");
    copy_file_cancellable(&mut locked_handle, &locked, &copied_lock, &no_cancel(), None).unwrap();
    locked_handle.set_times(times).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

    let dropbox = sandbox.dir("dropbox");
    let dropbox_handle = fs::File::open(&dropbox).unwrap();
    let copied_dropbox = sandbox.dir("dropbox copy");
    dropbox_handle.set_times(times).unwrap();
    fs::set_permissions(&dropbox, fs::Permissions::from_mode(0o300)).unwrap();

    let lock_result = preserve_metadata(
        &locked_handle,
        &locked,
        &copied_lock,
        &fs::symlink_metadata(&locked).unwrap(),
    );
    let dropbox_result = preserve_metadata(
        &dropbox_handle,
        &dropbox,
        &copied_dropbox,
        &fs::symlink_metadata(&dropbox).unwrap(),
    );

    // An unlistable folder cannot be torn down with the sandbox, so record
    // what happened and reopen everything before asserting anything.
    let lock_metadata = fs::symlink_metadata(&copied_lock).unwrap();
    let dropbox_metadata = fs::symlink_metadata(&copied_dropbox).unwrap();
    for path in [&dropbox, &copied_dropbox] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    lock_result.unwrap();
    dropbox_result.unwrap();
    assert_eq!(lock_metadata.permissions().mode() & 0o7777, 0o000);
    assert_eq!(lock_metadata.modified().unwrap(), modified);
    assert_eq!(dropbox_metadata.permissions().mode() & 0o7777, 0o300);
    assert_eq!(dropbox_metadata.modified().unwrap(), modified);
}

/// Nobody but the owner can read a copy while it is being written, whatever
/// the umask says. The content copy leaves the file at `0600`; the source's
/// mode is applied by a later step, once the content is final.
#[test]
fn copy_creates_files_owner_only_until_finished() {
    use std::os::unix::fs::PermissionsExt as _;

    let sandbox = Sandbox::new();
    let source = sandbox.file("secret", b"shh");
    fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();
    let staged = sandbox.path("staged");

    let mut input = fs::File::open(&source).unwrap();
    copy_file_cancellable(&mut input, &source, &staged, &no_cancel(), None).unwrap();

    assert_eq!(fs::metadata(&staged).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(read(&staged), b"shh");

    // The whole transfer then publishes it at the source's mode.
    let destination = sandbox.dir("destination");
    assert_clean(&copy(std::slice::from_ref(&source), &destination));
    let copied = fs::metadata(destination.join("secret")).unwrap();
    assert_eq!(copied.permissions().mode() & 0o777, 0o644);
}

#[test]
fn copy_preserves_supported_user_xattrs() {
    let sandbox = Sandbox::new();
    let source = sandbox.file("source.txt", b"contents");
    if let Err(error) = xattr::set(&source, "user.marcel-copy-test", b"kept") {
        assert!(xattrs_unsupported(&error));
        return;
    }
    let destination = sandbox.dir("destination");

    assert_clean(&copy(&[source], &destination));
    assert_eq!(
        xattr::get(destination.join("source.txt"), "user.marcel-copy-test").unwrap().as_deref(),
        Some(b"kept".as_slice())
    );
}

#[test]
fn copy_preserves_posix_access_acl_xattr_when_supported() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let sandbox = Sandbox::new();
    let source = sandbox.file("source.txt", b"contents");
    fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();
    let destination = sandbox.dir("destination");

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
            || matches!(error.kind(), io::ErrorKind::PermissionDenied | io::ErrorKind::InvalidInput)
        {
            return;
        }
        panic!("could not create ACL fixture: {error}");
    }
    let expected =
        xattr::get(&source, "system.posix_acl_access").unwrap().expect("ACL fixture disappeared");

    assert_clean(&copy(&[source], &destination));
    assert_eq!(
        xattr::get(destination.join("source.txt"), "system.posix_acl_access").unwrap(),
        Some(expected)
    );
}

#[test]
fn copy_preserves_hardlinks_within_a_directory_tree() {
    use std::os::unix::fs::MetadataExt as _;

    let sandbox = Sandbox::new();
    let tree = sandbox.dir("source/tree");
    sandbox.file("source/tree/first", b"shared");
    fs::hard_link(tree.join("first"), tree.join("second")).unwrap();
    let destination = sandbox.dir("destination");

    let outcome = copy(std::slice::from_ref(&tree), &destination);
    assert_clean(&outcome);

    let first = fs::metadata(destination.join("tree/first")).unwrap();
    let second = fs::metadata(destination.join("tree/second")).unwrap();
    assert_eq!((first.dev(), first.ino(), first.nlink()), (second.dev(), second.ino(), 2));
    recorded(undo_operation(&outcome.operation.unwrap()).unwrap());
    assert!(!destination.join("tree").exists());
}

#[test]
fn copy_preserves_sparse_layout_when_extents_are_available() {
    use std::os::unix::fs::MetadataExt as _;

    let sandbox = Sandbox::new();
    let source = sandbox.path("sparse.bin");
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
    let destination = sandbox.dir("destination");

    assert_clean(&copy(&[source], &destination));

    let copied = destination.join("sparse.bin");
    let copied_metadata = fs::metadata(&copied).unwrap();
    assert_eq!(copied_metadata.len(), source_metadata.len());
    assert!(copied_metadata.blocks() * 512 < copied_metadata.len());
    let mut copied_file = fs::File::open(copied).unwrap();
    let (mut start, mut end) = ([0; 5], [0; 4]);
    copied_file.read_exact(&mut start).unwrap();
    copied_file.seek(io::SeekFrom::End(-4)).unwrap();
    copied_file.read_exact(&mut end).unwrap();
    assert_eq!((&start, &end), (b"start", b"end!"));
}

#[test]
fn copy_rejects_special_files_without_publishing_them() {
    let sandbox = Sandbox::new();
    let _listener = sandbox.socket("socket");
    let destination = sandbox.dir("destination");

    let outcome = copy(&[sandbox.path("socket")], &destination);
    assert_eq!(outcome.failures.len(), 1);
    assert!(outcome.operation.is_none());
    assert!(!destination.join("socket").exists());
}

#[test]
fn xattr_policy_includes_user_and_posix_acl_namespaces_only() {
    for name in ["user.comment", "system.posix_acl_access", "system.posix_acl_default"] {
        assert!(supported_xattr_name(OsStr::new(name)), "{name}");
    }
    for name in ["security.selinux", "trusted.overlay"] {
        assert!(!supported_xattr_name(OsStr::new(name)), "{name}");
    }
}

#[test]
fn archive_create_and_extract_support_identity_validated_undo_redo() {
    if super::archive::SevenZipBackend::discover().is_err() {
        return;
    }
    let sandbox = Sandbox::new();
    let source = sandbox.file("report.txt", b"archive history");
    let archive = sandbox.path("report.zip");

    let created = recorded(
        create_zip_operation(std::slice::from_ref(&source), &archive, no_cancel()).unwrap(),
    );
    assert!(archive.is_file());
    let undone = recorded(undo_operation(&created).unwrap());
    assert!(!archive.exists());
    let recreated = recorded(redo_operation(&undone).unwrap());
    assert!(archive.is_file());

    fs::remove_file(&source).unwrap();
    let extracted = recorded(
        extract_archive_operation(&archive, no_cancel(), &mut ConflictPolicy::refusing()).unwrap(),
    );
    assert_eq!(read(&source), b"archive history");
    let undone = recorded(undo_operation(&extracted).unwrap());
    assert!(!source.exists());
    let redone = recorded(redo_operation(&undone).unwrap());
    assert_eq!(read(redone.path()), b"archive history");

    fs::write(redone.path(), b"changed").unwrap();
    assert!(undo_operation(&redone).is_err());
    assert_eq!(read(redone.path()), b"changed");

    // Keep the compiler and test honest that the recreated record remains
    // a normal archive operation rather than a special test-only path.
    assert!(matches!(recreated, OperationRecord::ArchiveCreate { .. }));
}

/// Replacing while extracting holds the occupant aside the way a copy does,
/// so undo removes the extracted item and brings the occupant back.
#[test]
fn extraction_that_replaces_is_undone_by_restoring_the_occupant() {
    if super::archive::SevenZipBackend::discover().is_err() {
        return;
    }
    let sandbox = Sandbox::new();
    let source = sandbox.file("report.txt", b"from the archive");
    let archive = sandbox.path("report.zip");
    create_zip_operation(std::slice::from_ref(&source), &archive, no_cancel()).unwrap();
    fs::write(&source, b"edited since").unwrap();

    let extracted = recorded(
        extract_archive_operation(
            &archive,
            no_cancel(),
            &mut answering(ConflictDecision::once(ConflictResponse::Replace)),
        )
        .unwrap(),
    );
    assert_eq!(read(&source), b"from the archive");
    assert_eq!(extracted.replaced_items().len(), 1);

    // Undoing a replacement is not redoable, as with a copy: redoing would
    // displace the restored item again, a decision the user has not made.
    let undone = undo_operation(&extracted).unwrap();
    assert_eq!(read(&source), b"edited since");
    assert!(no_replacement_quarantines(sandbox.root()));
    assert!(!undone.is_undoable());
}

// ---------------------------------------------------------------------------
// The window between inspecting a source entry and opening it.

/// Everything Marcel leaves in a directory while it works, or fails to take
/// back afterwards.
fn no_working_names(directory: &Path) -> bool {
    fs::read_dir(directory).unwrap().flatten().all(|entry| {
        let name = entry.file_name();
        !is_internal_working_name(&name) && !is_recovery_remnant_name(&name)
    })
}

fn make_fifo(path: &Path) {
    use rustix::fs::{CWD, FileType, Mode, mknodat};

    mknodat(CWD, path, FileType::Fifo, Mode::RUSR | Mode::WUSR, 0).unwrap();
}

/// Put `replacement` where `victim` is, the way a co-writer would: with a
/// rename over the name, so the old inode cannot be handed straight back to
/// the new object and make the identity check pass by coincidence.
fn swap_in(victim: &Path, replacement: &Path) {
    fs::rename(replacement, victim).unwrap();
}

/// Run a copy on its own thread and give up waiting after a while, so a copy
/// that blocks on a FIFO fails the test instead of hanging it.
fn copy_within(
    timeout: Duration,
    sources: Vec<PathBuf>,
    destination: PathBuf,
    interference: impl FnMut(&Path) + Send + 'static,
) -> TransferOutcome {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _guard = copy::fault::between_inspection_and_open_do(interference);
        let _ = sender.send(copy(&sources, &destination));
    });
    receiver.recv_timeout(timeout).expect("the copy must finish rather than block")
}

/// `open(2)` on a FIFO with no writer blocks until one arrives; a copy that
/// opened its source by path after inspecting it could be parked forever by
/// a FIFO swapped in between, with the cancel flag unable to reach it.
#[test]
fn copy_refuses_a_fifo_swapped_in_for_a_file_without_blocking() {
    let sandbox = Sandbox::new();
    let source = sandbox.dir("source");
    let report = sandbox.file("source/report.txt", b"report");
    let destination = sandbox.dir("destination");
    let fifo = sandbox.path("fifo");
    make_fifo(&fifo);

    let trigger = report.clone();
    let outcome =
        copy_within(Duration::from_secs(10), vec![source], destination.clone(), move |path| {
            if path == trigger {
                swap_in(&trigger, &fifo);
            }
        });

    assert_eq!(outcome.failures.len(), 1, "{outcome:?}");
    assert!(outcome.failures[0].message.contains("report.txt"), "{:?}", outcome.failures);
    assert!(!destination.join("source").exists(), "nothing is published from a refused copy");
    assert!(no_working_names(&destination));
}

/// A link swapped in for a file must not be read through: that would copy
/// another readable file under the original's name and mode.
#[test]
fn copy_refuses_a_link_swapped_in_for_a_file() {
    let sandbox = Sandbox::new();
    let source = sandbox.dir("source");
    let report = sandbox.file("source/report.txt", b"report");
    let secret = sandbox.file("secret", b"do not copy");
    let link = sandbox.path("link");
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    let destination = sandbox.dir("destination");

    let trigger = report.clone();
    let outcome =
        copy_within(Duration::from_secs(10), vec![source], destination.clone(), move |path| {
            if path == trigger {
                swap_in(&trigger, &link);
            }
        });

    assert_eq!(outcome.failures.len(), 1, "{outcome:?}");
    assert!(!destination.join("source").exists());
    assert!(no_working_names(&destination));
}

/// A regular file swapped in for a regular file passes every type check, so
/// only the identity of the opened descriptor can tell it apart from what the
/// walker decided to copy.
#[test]
fn copy_refuses_a_file_replaced_between_inspection_and_open() {
    let sandbox = Sandbox::new();
    let source = sandbox.dir("source");
    let report = sandbox.file("source/report.txt", b"report");
    let impostor = sandbox.file("impostor", b"impostor");
    let destination = sandbox.dir("destination");

    let trigger = report.clone();
    let outcome =
        copy_within(Duration::from_secs(10), vec![source], destination.clone(), move |path| {
            if path == trigger {
                swap_in(&trigger, &impostor);
            }
        });

    assert_eq!(outcome.failures.len(), 1, "{outcome:?}");
    assert!(outcome.failures[0].message.contains("was replaced"), "{:?}", outcome.failures);
    assert!(!destination.join("source").exists());
}

/// A path-based walker resolved every entry from the root again, so a
/// directory component swapped for a link mid-walk redirected the rest of the
/// walk into whatever the link pointed at. Each level is held open now, and
/// the entries below it are read through that descriptor.
#[test]
fn copy_reads_through_held_directories_when_a_component_is_swapped_for_a_link() {
    let sandbox = Sandbox::new();
    let source = sandbox.dir("source");
    sandbox.file("source/sub/deep/a.txt", b"original");
    sandbox.file("source/sub/deep/b.txt", b"original too");
    sandbox.file("outside/deep/a.txt", b"planted");
    sandbox.file("outside/deep/b.txt", b"planted too");
    let destination = sandbox.dir("destination");
    let sub = sandbox.path("source/sub");
    let aside = sandbox.path("sub-aside");
    let outside = sandbox.path("outside");

    let trigger = sandbox.path("source/sub/deep/a.txt");
    let outcome =
        copy_within(Duration::from_secs(10), vec![source], destination.clone(), move |path| {
            if path == trigger {
                fs::rename(&sub, &aside).unwrap();
                std::os::unix::fs::symlink(&outside, &sub).unwrap();
            }
        });

    assert_clean(&outcome);
    assert_eq!(read(destination.join("source/sub/deep/a.txt")), b"original");
    assert_eq!(read(destination.join("source/sub/deep/b.txt")), b"original too");
    assert!(
        destination.join("source/sub").symlink_metadata().unwrap().is_dir(),
        "the copy holds the directory the walk entered, not the link swapped in"
    );
}

/// A link swapped in for a directory the walker is about to enter is refused
/// rather than entered.
#[test]
fn copy_refuses_a_link_swapped_in_for_a_directory_it_is_about_to_enter() {
    let sandbox = Sandbox::new();
    let source = sandbox.dir("source");
    sandbox.file("source/sub/a.txt", b"original");
    sandbox.file("outside/a.txt", b"planted");
    let destination = sandbox.dir("destination");
    let sub = sandbox.path("source/sub");
    let aside = sandbox.path("sub-aside");
    let outside = sandbox.path("outside");

    let trigger = sub.clone();
    let outcome =
        copy_within(Duration::from_secs(10), vec![source], destination.clone(), move |path| {
            if path == trigger {
                fs::rename(&sub, &aside).unwrap();
                std::os::unix::fs::symlink(&outside, &sub).unwrap();
            }
        });

    assert_eq!(outcome.failures.len(), 1, "{outcome:?}");
    assert!(!destination.join("source").exists());
}

/// The copy primitives themselves refuse what the walker must never open:
/// a FIFO returns at once instead of blocking, and a link is not followed.
#[test]
fn descriptor_opens_refuse_fifos_and_links_without_blocking() {
    use super::local::{CWD, open_directory_at, open_regular_file_at};

    let sandbox = Sandbox::new();
    let fifo = sandbox.path("fifo");
    make_fifo(&fifo);
    let file = sandbox.file("file", b"content");
    let directory = sandbox.dir("directory");
    let file_link = sandbox.path("file-link");
    let directory_link = sandbox.path("directory-link");
    std::os::unix::fs::symlink(&file, &file_link).unwrap();
    std::os::unix::fs::symlink(&directory, &directory_link).unwrap();

    let started = std::time::Instant::now();
    assert!(open_regular_file_at(CWD, &fifo).is_err());
    assert!(started.elapsed() < Duration::from_secs(5), "a FIFO must not block the open");
    assert!(open_regular_file_at(CWD, &file_link).is_err());
    assert!(open_regular_file_at(CWD, &directory).is_err());
    assert!(open_regular_file_at(CWD, &file).is_ok());
    assert!(open_directory_at(CWD, &directory_link).is_err());
    assert!(open_directory_at(CWD, &file).is_err());
    assert!(open_directory_at(CWD, &directory).is_ok());
}

/// The self-containment check compares identities up the destination's
/// ancestor chain, so a plainly nested destination is refused at every depth,
/// not only when the source is the immediate parent.
#[test]
fn copy_refuses_a_destination_nested_anywhere_below_the_source() {
    let sandbox = Sandbox::new();
    let source = sandbox.dir("source");
    let deep = sandbox.dir("source/a/b/c");

    let outcome = copy(std::slice::from_ref(&source), &deep);

    assert_eq!(outcome.failures.len(), 1, "{outcome:?}");
    assert!(outcome.failures[0].message.contains("into itself"), "{:?}", outcome.failures);
    assert!(!deep.join("source").exists());
    // A sibling of the source is not inside it, whatever it is called.
    let sibling = sandbox.dir("source-sibling/a/b/c");
    assert_clean(&copy(std::slice::from_ref(&source), &sibling));
}

// ---------------------------------------------------------------------------
// Replacements whose transfer lost its own undo.

/// Giving up the copy ledger empties it of every earlier copy too, so an item
/// one of those copies displaced had nowhere to return to: undo tried to put
/// it back over a copy it no longer knew, failed with `EEXIST`, and left the
/// original as a `.marcel-recovered-*` remnant. The quarantine is released as
/// soon as it stops being restorable, whether the ledger was given up after
/// the replacement or before it.
#[test]
fn a_replacement_whose_copy_lost_its_undo_is_released_rather_than_recorded() {
    for overflow_first in [false, true] {
        let sandbox = Sandbox::new();
        // Merging stays within budget, so the operation keeps a record for
        // undo to act on.
        let photos = sandbox.dir("source/photos");
        sandbox.file("source/photos/new.txt", b"new");
        sandbox.dir("destination/photos");
        let report = sandbox.file("source/report.txt", b"replacement");
        let replaced = sandbox.file("destination/report.txt", b"the original");
        let album = sandbox.dir("source/album");
        for index in 0..6 {
            sandbox.file(&format!("source/album/{index}.txt"), b"payload");
        }
        let destination = sandbox.path("destination");
        let budget = TransferBudget { undo_snapshot_limit: 6, ..TransferBudget::default() };
        let sources =
            if overflow_first { vec![photos, album, report] } else { vec![photos, report, album] };

        let outcome = budgeted(&sources, &destination, TransferMode::Copy, budget);

        assert_clean(&outcome);
        assert!(outcome.undo_unavailable, "overflow_first={overflow_first}: {outcome:?}");
        assert_eq!(read(&replaced), b"replacement");
        assert!(
            no_working_names(&destination),
            "overflow_first={overflow_first}: an unrestorable quarantine must be released"
        );
        let record = outcome.operation.expect("the merge keeps a record");
        let undone = undo_operation(&record);
        assert!(!undone.is_err(), "overflow_first={overflow_first}: undo must not fail");
        assert!(!destination.join("photos/new.txt").exists(), "the merge is taken back");
        assert_eq!(read(&replaced), b"replacement", "an unrecorded replacement stands");
        assert!(no_working_names(&destination), "undo must leave no remnants");
    }
}

/// A move whose own undo record was lost still stands, and so does what it
/// displaced: recording the displaced item would have undo restore it over a
/// destination that the record can no longer clear.
#[test]
fn a_replacement_whose_move_lost_its_undo_is_released_rather_than_recorded() {
    let sandbox = Sandbox::new();
    let note = sandbox.file("source/note.txt", b"note");
    let album = sandbox.dir("source/album");
    for index in 0..6 {
        sandbox.file(&format!("source/album/{index}.txt"), b"payload");
    }
    let replaced = sandbox.file("destination/album", b"the original");
    let destination = sandbox.path("destination");
    let budget = TransferBudget { undo_snapshot_limit: 4, ..TransferBudget::default() };

    let outcome = budgeted(&[note.clone(), album], &destination, TransferMode::Move, budget);

    assert_clean(&outcome);
    assert!(outcome.undo_unavailable, "{outcome:?}");
    assert!(replaced.is_dir(), "the move stands");
    assert!(no_working_names(&destination), "an unrestorable quarantine must be released");
    let record = outcome.operation.expect("the first move keeps its record");
    let undone = undo_operation(&record);
    assert!(!undone.is_err(), "undo must not fail");
    assert_eq!(read(&note), b"note", "the recorded move is taken back");
    assert!(replaced.is_dir(), "the unrecorded move stands");
    assert!(no_working_names(&destination), "undo must leave no remnants");
}
