use std::{
    collections::HashSet,
    ffi::OsString,
    fs,
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result, bail};

use crate::{
    delete_ops::{RenameIdentity, delete_trash_backings},
    file_ops::TransferProgress,
    local_fs::{PathOccupancy, path_occupancy, rename_no_replace},
};

// Conceptually adapted from Yazi's separation between its background file
// scheduler and freedesktop Trash VFS:
// https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-scheduler/src/file/file.rs
// https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-fs/src/trash/freedesktop/trash.rs
//
// Like Yazi, Marcel delegates platform Trash placement to the MIT-licensed
// `trash` crate. Marcel adds operation-journal identities and stricter restore
// rules behind its own interface. No Yazi code is copied here.

#[derive(Clone, Debug, PartialEq, Eq)]
struct TrashIdentity {
    device: u64,
    inode: u64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashRecord {
    original_path: PathBuf,
    info_path: PathBuf,
    backing_path: PathBuf,
    name: OsString,
    original_parent: PathBuf,
    time_deleted: i64,
    info_identity: TrashIdentity,
    payload_identity: TrashIdentity,
}

impl TrashRecord {
    pub fn original_path(&self) -> &Path {
        &self.original_path
    }

    pub fn backing_path(&self) -> &Path {
        &self.backing_path
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashFailure {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug)]
pub struct TrashOutcome {
    /// The exact records this outcome affected. For a trash placement these
    /// carry undo; for a purge they identify what left the Trash, because two
    /// entries can share one original path and only the record can tell a
    /// purged entry from its surviving twin.
    pub records: Vec<TrashRecord>,
    pub completed: Vec<PathBuf>,
    pub failures: Vec<TrashFailure>,
    pub undo_unavailable: bool,
}

pub fn path_overlaps_system_trash(path: &Path) -> Result<bool> {
    let roots = trash::os_limited::trash_folders().context("Could not resolve the system Trash")?;
    Ok(roots
        .iter()
        .any(|root| paths_overlap_trash_root(path, root)))
}

/// What one enumeration of the system Trash found.
///
/// `unreadable` exists because dropping entries Marcel cannot describe made the
/// listing look complete when it was not — and made Empty Trash offer to empty
/// a Trash it had only partly seen.
#[derive(Debug, Default)]
pub struct TrashListing {
    pub records: Vec<TrashRecord>,
    pub unreadable: Vec<String>,
}

/// One sentence naming what a Trash listing could not describe, if anything.
pub fn unreadable_trash_warning(unreadable: &[String]) -> Option<String> {
    let first = unreadable.first()?;
    Some(if unreadable.len() == 1 {
        format!("One Trash entry could not be read and is not shown: {first}")
    } else {
        format!(
            "{} Trash entries could not be read and are not shown (first: {first})",
            unreadable.len()
        )
    })
}

pub fn list_trash_records() -> Result<TrashListing> {
    let mut listing = TrashListing::default();
    for item in trash::os_limited::list().context("Could not inspect the system Trash")? {
        match record_from_item(item) {
            Ok(record) => listing.records.push(record),
            Err(error) => listing.unreadable.push(error.to_string()),
        }
    }
    Ok(listing)
}

pub fn purge_trash_records(
    records: &[TrashRecord],
    progress: Arc<TransferProgress>,
) -> TrashOutcome {
    for record in records {
        if let Err(error) =
            validate_identity(&record.info_path, &record.info_identity, "Trash metadata").and_then(
                |()| {
                    validate_identity(
                        &record.backing_path,
                        &record.payload_identity,
                        "Trash payload",
                    )
                },
            )
        {
            return TrashOutcome {
                records: Vec::new(),
                completed: Vec::new(),
                failures: vec![TrashFailure {
                    path: record.original_path.clone(),
                    message: error.to_string(),
                }],
                undo_unavailable: false,
            };
        }
    }

    // Carry the identity each record was just validated against into the
    // deletion, so the purge and the delete agree on which object they mean.
    let backings = records
        .iter()
        .map(|record| {
            (
                record.backing_path.clone(),
                RenameIdentity::new(
                    record.payload_identity.device,
                    record.payload_identity.inode,
                ),
            )
        })
        .collect::<Vec<_>>();
    let deleted = delete_trash_backings(&backings, progress);
    let mut completed = Vec::new();
    let mut failures = deleted
        .failures
        .into_iter()
        .map(|failure| TrashFailure {
            path: map_backing_to_original(records, &failure.path),
            message: failure.message,
        })
        .collect::<Vec<_>>();

    let mut purged = Vec::new();
    for backing in deleted.completed {
        let Some(record) = records.iter().find(|record| record.backing_path == backing) else {
            continue;
        };
        completed.push(record.original_path.clone());
        purged.push(record.clone());
        if let Err(error) = remove_matching_trash_info(record) {
            failures.push(TrashFailure {
                path: record.original_path.clone(),
                message: format!(
                    "Permanently deleted “{}”, but could not remove its Trash metadata: {error}",
                    record.original_path.display()
                ),
            });
        }
    }

    TrashOutcome {
        records: purged,
        completed,
        failures,
        undo_unavailable: false,
    }
}

pub fn trash_paths(paths: &[PathBuf]) -> TrashOutcome {
    let trash_roots = match trash::os_limited::trash_folders() {
        Ok(roots) => roots,
        Err(error) => {
            return TrashOutcome {
                records: Vec::new(),
                completed: Vec::new(),
                failures: paths
                    .iter()
                    .cloned()
                    .map(|path| TrashFailure {
                        path,
                        message: format!("Could not resolve the system Trash: {error}"),
                    })
                    .collect(),
                undo_unavailable: false,
            };
        }
    };
    let before = match trash::os_limited::list() {
        Ok(items) => items,
        Err(error) => {
            return TrashOutcome {
                records: Vec::new(),
                completed: Vec::new(),
                failures: paths
                    .iter()
                    .cloned()
                    .map(|path| TrashFailure {
                        path,
                        message: format!("Could not inspect the system Trash: {error}"),
                    })
                    .collect(),
                undo_unavailable: false,
            };
        }
    };
    let existing_ids = before
        .into_iter()
        .map(|item| item.id)
        .collect::<HashSet<_>>();

    let mut successful = Vec::new();
    let mut failures = Vec::new();
    for path in paths {
        if trash_roots
            .iter()
            .any(|root| paths_overlap_trash_root(path, root))
        {
            failures.push(TrashFailure {
                path: path.clone(),
                message: format!(
                    "Refusing to trash “{}” because it is inside or contains a system Trash",
                    path.display()
                ),
            });
            continue;
        }
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) => {
                failures.push(TrashFailure {
                    path: path.clone(),
                    message: format!("Could not inspect “{}”: {error}", path.display()),
                });
                continue;
            }
        };
        let source_object = object_key(&metadata);
        match trash::delete(path) {
            Ok(()) => successful.push((path.clone(), source_object)),
            Err(error) => failures.push(TrashFailure {
                path: path.clone(),
                message: format!("Could not move “{}” to Trash: {error}", path.display()),
            }),
        }
    }

    let completed = successful
        .iter()
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    if successful.is_empty() {
        return TrashOutcome {
            records: Vec::new(),
            completed,
            failures,
            undo_unavailable: false,
        };
    }

    let after = match trash::os_limited::list() {
        Ok(items) => items,
        Err(error) => {
            failures.push(TrashFailure {
                path: successful[0].0.clone(),
                message: format!(
                    "Items reached Trash, but Marcel could not retain restore metadata: {error}"
                ),
            });
            return TrashOutcome {
                records: Vec::new(),
                completed,
                failures,
                undo_unavailable: true,
            };
        }
    };
    let mut new_items = after
        .into_iter()
        .filter(|item| !existing_ids.contains(&item.id))
        .collect::<Vec<_>>();

    let mut records = Vec::with_capacity(successful.len());
    for (original, source_object) in successful {
        let candidates = new_items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                let backing = backing_path_from_info(Path::new(&item.id)).ok()?;
                let metadata = fs::symlink_metadata(&backing).ok()?;
                Some((
                    index,
                    object_key(&metadata) == source_object,
                    item.original_path() == original,
                ))
            })
            .collect::<Vec<_>>();
        // Hardlinks share a (device, inode) key, so a concurrent trash of a
        // sibling link can produce two identity matches. Bind the journal only
        // to an unambiguous entry; anything else disables Undo rather than
        // guessing and later restoring to the wrong original path.
        let unique = |mut matching: Vec<&(usize, bool, bool)>| {
            (matching.len() == 1).then(|| matching.remove(0).0)
        };
        let candidate = unique(
            candidates
                .iter()
                .filter(|(_, identity_matches, path_matches)| *identity_matches && *path_matches)
                .collect(),
        )
        .or_else(|| {
            unique(
                candidates
                    .iter()
                    .filter(|(_, identity_matches, _)| *identity_matches)
                    .collect(),
            )
        })
        .or_else(|| {
            unique(
                candidates
                    .iter()
                    .filter(|(_, _, path_matches)| *path_matches)
                    .collect(),
            )
        });

        let Some(index) = candidate else {
            failures.push(TrashFailure {
                path: original,
                message: "Item reached Trash, but its exact restore entry could not be identified"
                    .to_string(),
            });
            continue;
        };
        let item = new_items.swap_remove(index);
        match record_from_item(item) {
            Ok(record) => records.push(record),
            Err(error) => failures.push(TrashFailure {
                path: original,
                message: format!(
                    "Item reached Trash, but Marcel could not retain restore metadata: {error}"
                ),
            }),
        }
    }

    TrashOutcome {
        undo_unavailable: records.len() != completed.len(),
        records,
        completed,
        failures,
    }
}

/// The result of restoring items out of Trash.
///
/// `undoable` is false when every payload reached its original path but Marcel
/// could not re-read one afterwards. The restore still happened, so the caller
/// must present success without undo rather than an error.
#[derive(Debug)]
pub struct TrashRestore {
    pub records: Vec<TrashRecord>,
    pub undoable: bool,
}

/// A failed Trash mutation, and whether it reached the filesystem.
///
/// Neither upstream models this. Yazi's `Trash::restore` loops with `?` and
/// leaves partial results in place with no rollback and no identity check
/// (`yazi-fs/src/trash/freedesktop/trash.rs`). Nautilus discards the return
/// value of the `g_file_move` that performs each restore and reports success
/// as "at least one entry matched"
/// (`nautilus-file-undo-operations.c`, `trash_retrieve_files_to_restore_thread`).
/// Marcel validates identities, rolls back, and therefore has to say which side
/// of its commit boundary a failure landed on.
#[derive(Debug)]
pub struct TrashMutationFailure {
    pub error: anyhow::Error,
    /// False only when Marcel can prove no rename committed, which keeps the
    /// history record retryable. A rollback still counts as committed: it
    /// renames payloads a second time and moves the ctimes the record holds.
    pub committed: bool,
}

impl TrashMutationFailure {
    fn unchanged(error: impl Into<anyhow::Error>) -> Self {
        Self {
            error: error.into(),
            committed: false,
        }
    }

    fn committed(error: impl Into<anyhow::Error>) -> Self {
        Self {
            error: error.into(),
            committed: true,
        }
    }
}

pub fn restore_trash_records(
    records: &[TrashRecord],
) -> Result<TrashRestore, TrashMutationFailure> {
    // Prepare: every check runs before the first rename, so a refusal here
    // provably left the Trash untouched.
    for record in records {
        let prepared =
            validate_identity(&record.info_path, &record.info_identity, "Trash metadata")
                .and_then(|()| {
                    validate_identity(
                        &record.backing_path,
                        &record.payload_identity,
                        "Trash payload",
                    )
                })
                .and_then(|()| {
                    let parent =
                        fs::symlink_metadata(&record.original_parent).with_context(|| {
                            format!(
                                "Cannot restore “{}”: its original parent no longer exists",
                                record.original_path().display()
                            )
                        })?;
                    if !parent.file_type().is_dir() {
                        bail!(
                            "Cannot restore “{}”: its original parent is no longer a directory",
                            record.original_path().display()
                        );
                    }
                    ensure_unoccupied(record.original_path())
                });
        if let Err(error) = prepared {
            return Err(TrashMutationFailure::unchanged(error));
        }
    }

    let mut restored = Vec::with_capacity(records.len());
    for record in records {
        let original = record.original_path().to_path_buf();
        // Commit.
        if let Err(error) = rename_no_replace(&record.backing_path, &original) {
            let message = format!(
                "Could not restore “{}” from Trash: {error}",
                original.display()
            );
            if restored.is_empty() {
                // The first rename failed, so the Trash is untouched and the
                // record still describes it exactly.
                return Err(TrashMutationFailure::unchanged(anyhow::anyhow!(
                    "{message}"
                )));
            }
            return Err(match rollback_restored(&restored) {
                Ok(()) => TrashMutationFailure::committed(anyhow::anyhow!(
                    "{message}; earlier restores were rolled back"
                )),
                Err(rollback_error) => TrashMutationFailure::committed(anyhow::anyhow!(
                    "{message}; rollback also failed: {rollback_error}"
                )),
            });
        }
        restored.push(record);
    }

    // Finalize. Every payload is already restored, so nothing below may fail
    // the operation; an uninspectable result only costs undo.
    let mut result = Vec::with_capacity(records.len());
    let mut undoable = true;
    for record in records {
        // The payload is already safely restored. A failed metadata cleanup
        // leaves only an orphaned Trash entry, never missing user data — but
        // check identity first so a replacement metadata file for an unrelated
        // Trash entry is never the thing that gets removed.
        let _ = remove_matching_trash_info(record);
        let original = record.original_path();
        match fs::symlink_metadata(original) {
            Ok(metadata) => {
                let mut updated = record.clone();
                updated.payload_identity = identity(&metadata);
                result.push(updated);
            }
            Err(_) => undoable = false,
        }
    }
    Ok(TrashRestore {
        records: result,
        undoable,
    })
}

pub fn retrash_records(records: &[TrashRecord]) -> Result<Vec<TrashRecord>, TrashMutationFailure> {
    // Prepare.
    for record in records {
        if let Err(error) = validate_identity(
            record.original_path(),
            &record.payload_identity,
            "Restored item",
        ) {
            return Err(TrashMutationFailure::unchanged(error));
        }
    }
    let originals = records
        .iter()
        .map(|record| record.original_path().to_path_buf())
        .collect::<Vec<_>>();
    // Commit: `trash_paths` places items one at a time.
    let outcome = trash_paths(&originals);
    if outcome.failures.is_empty()
        && !outcome.undo_unavailable
        && outcome.records.len() == originals.len()
    {
        return Ok(outcome.records);
    }

    let failure = summarize_failures(&outcome.failures);
    if outcome.completed.is_empty() {
        // Nothing reached the Trash, so the record is still accurate.
        return Err(TrashMutationFailure::unchanged(anyhow::anyhow!(
            "{failure}"
        )));
    }
    if outcome.records.is_empty() {
        // Items were trashed but none could be identified, so Marcel cannot
        // compensate for them and must not claim nothing happened.
        return Err(TrashMutationFailure::committed(anyhow::anyhow!(
            "{failure}; items reached Trash but could not be returned"
        )));
    }
    // Compensating action: the trash operation partially committed, so undo
    // what can be identified before reporting the failure.
    Err(match restore_trash_records(&outcome.records) {
        Ok(_) => TrashMutationFailure::committed(anyhow::anyhow!(
            "{failure}; completed items were restored"
        )),
        Err(rollback) => TrashMutationFailure::committed(anyhow::anyhow!(
            "{failure}; restore rollback also failed: {}",
            rollback.error
        )),
    })
}

pub fn summarize_failures(failures: &[TrashFailure]) -> String {
    match failures {
        [] => "Trash operation could not be recorded safely".to_string(),
        [failure] => failure.message.clone(),
        [first, rest @ ..] => format!(
            "{} (and {} more failure{})",
            first.message,
            rest.len(),
            if rest.len() == 1 { "" } else { "s" }
        ),
    }
}

fn record_from_item(item: trash::TrashItem) -> Result<TrashRecord> {
    let info_path = PathBuf::from(&item.id);
    let backing_path = backing_path_from_info(&info_path)?;
    let info_metadata = fs::symlink_metadata(&info_path)
        .with_context(|| format!("Could not inspect “{}”", info_path.display()))?;
    if !info_metadata.file_type().is_file() {
        bail!("Trash metadata is not a regular file");
    }
    let payload_metadata = fs::symlink_metadata(&backing_path)
        .with_context(|| format!("Could not inspect “{}”", backing_path.display()))?;
    Ok(TrashRecord {
        original_path: item.original_path(),
        info_path,
        backing_path,
        name: item.name,
        original_parent: item.original_parent,
        time_deleted: item.time_deleted,
        info_identity: identity(&info_metadata),
        payload_identity: identity(&payload_metadata),
    })
}

fn backing_path_from_info(info_path: &Path) -> Result<PathBuf> {
    if !info_path.is_absolute() || info_path.extension() != Some(std::ffi::OsStr::new("trashinfo"))
    {
        bail!("Invalid freedesktop Trash metadata path");
    }
    let info_dir = info_path
        .parent()
        .filter(|parent| parent.file_name() == Some(std::ffi::OsStr::new("info")))
        .context("Invalid freedesktop Trash metadata directory")?;
    let trash_root = info_dir
        .parent()
        .context("Invalid freedesktop Trash root")?;
    let name = info_path
        .file_stem()
        .filter(|name| !name.is_empty())
        .context("Invalid freedesktop Trash entry name")?;
    Ok(trash_root.join("files").join(name))
}

fn validate_identity(path: &Path, expected: &TrashIdentity, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Cannot continue: {label} “{}” is missing", path.display()))?;
    if identity(&metadata) != *expected {
        bail!(
            "Cannot continue: {label} “{}” changed or was replaced",
            path.display()
        );
    }
    Ok(())
}

fn ensure_unoccupied(path: &Path) -> Result<()> {
    match path_occupancy(path) {
        Ok(PathOccupancy::Occupied) => {
            bail!("Cannot restore: “{}” is already occupied", path.display())
        }
        Ok(PathOccupancy::Vacant) => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("Could not inspect restore target “{}”", path.display())),
    }
}

fn rollback_restored(restored: &[&TrashRecord]) -> Result<()> {
    for record in restored.iter().rev() {
        rename_no_replace(record.original_path(), &record.backing_path).with_context(|| {
            format!(
                "Could not return “{}” to Trash",
                record.original_path().display()
            )
        })?;
    }
    Ok(())
}

fn identity(metadata: &fs::Metadata) -> TrashIdentity {
    TrashIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    }
}

fn object_key(metadata: &fs::Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

/// Whether `path` lies inside a Trash root, or contains one.
///
/// Compared physically rather than lexically. A symbolic link anywhere above
/// the path gives the same directory two spellings, and a prefix test on the
/// spelling the user happened to type sees only one of them — so a Trash root
/// reachable through a link would not be recognized as one.
///
/// The object itself is deliberately left unresolved: deleting a symbolic link
/// that points into the Trash removes the link, not what it points at, and
/// resolving the leaf would refuse a deletion that is perfectly safe. The
/// lexical answer is kept as well, so a path that cannot be resolved at all
/// stays refused instead of quietly becoming deletable.
fn paths_overlap_trash_root(path: &Path, trash_root: &Path) -> bool {
    if path.starts_with(trash_root) || trash_root.starts_with(path) {
        return true;
    }
    let path = resolve_parent_of(path);
    let root = trash_root
        .canonicalize()
        .unwrap_or_else(|_| trash_root.to_path_buf());
    path.starts_with(&root) || root.starts_with(&path)
}

/// Resolve everything above the final component, and nothing of it.
fn resolve_parent_of(path: &Path) -> PathBuf {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    };
    match parent.canonicalize() {
        Ok(parent) => parent.join(name),
        Err(_) => path.to_path_buf(),
    }
}

fn remove_matching_trash_info(record: &TrashRecord) -> Result<()> {
    validate_identity(&record.info_path, &record.info_identity, "Trash metadata")?;
    fs::remove_file(&record.info_path).with_context(|| {
        format!(
            "Could not remove Trash metadata “{}”",
            record.info_path.display()
        )
    })
}

fn map_backing_to_original(records: &[TrashRecord], path: &Path) -> PathBuf {
    records
        .iter()
        .find_map(|record| {
            let relative = path.strip_prefix(&record.backing_path).ok()?;
            Some(if relative.as_os_str().is_empty() {
                record.original_path.clone()
            } else {
                record.original_path.join(relative)
            })
        })
        .unwrap_or_else(|| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    fn seeded_record(root: &Path, original_parent: &Path, name: &str) -> TrashRecord {
        let info_dir = root.join("info");
        let files_dir = root.join("files");
        fs::create_dir_all(&info_dir).unwrap();
        fs::create_dir_all(&files_dir).unwrap();
        let info_path = info_dir.join(format!("{name}.trashinfo"));
        fs::write(
            &info_path,
            format!(
                "[Trash Info]\nPath={}\nDeletionDate=2026-07-29T12:00:00\n",
                original_parent.join(name).display()
            ),
        )
        .unwrap();
        let backing_path = files_dir.join(name);
        fs::write(&backing_path, b"payload").unwrap();
        TrashRecord {
            original_path: original_parent.join(name),
            info_identity: identity(&fs::symlink_metadata(&info_path).unwrap()),
            payload_identity: identity(&fs::symlink_metadata(&backing_path).unwrap()),
            info_path,
            backing_path,
            name: name.into(),
            original_parent: original_parent.to_path_buf(),
            time_deleted: 0,
        }
    }

    #[test]
    fn derives_backing_path_only_from_well_formed_info_paths() {
        assert_eq!(
            backing_path_from_info(Path::new("/tmp/Trash/info/report.pdf.trashinfo")).unwrap(),
            Path::new("/tmp/Trash/files/report.pdf")
        );
        assert!(backing_path_from_info(Path::new("/tmp/Trash/report.trashinfo")).is_err());
        assert!(backing_path_from_info(Path::new("Trash/info/report.trashinfo")).is_err());
    }

    #[test]
    fn refuses_paths_inside_or_containing_a_trash_root() {
        let root = Path::new("/home/test/.local/share/Trash");
        assert!(paths_overlap_trash_root(
            Path::new("/home/test/.local/share/Trash/files/note"),
            root
        ));
        assert!(paths_overlap_trash_root(Path::new("/home/test"), root));
        assert!(!paths_overlap_trash_root(
            Path::new("/home/test/Documents"),
            root
        ));
    }

    /// A symbolic link gives one directory two spellings, and the guard has to
    /// recognize the Trash under either of them.
    #[test]
    fn a_trash_root_reached_through_a_symlink_is_still_refused() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Trash");
        fs::create_dir_all(root.join("files")).unwrap();
        fs::write(root.join("files/note.txt"), b"trashed").unwrap();
        std::os::unix::fs::symlink(&root, temp.path().join("shortcut")).unwrap();

        let through_link = temp.path().join("shortcut/files/note.txt");
        assert!(
            paths_overlap_trash_root(&through_link, &root),
            "the same object under another spelling is still in the Trash"
        );

        // The object itself is never resolved: deleting a link that points into
        // the Trash removes the link, which is safe and must stay allowed.
        let elsewhere = temp.path().join("pointer");
        std::os::unix::fs::symlink(root.join("files/note.txt"), &elsewhere).unwrap();
        assert!(!paths_overlap_trash_root(&elsewhere, &root));
    }

    #[test]
    fn an_unreadable_trash_entry_is_announced_rather_than_dropped() {
        assert!(unreadable_trash_warning(&[]).is_none());
        let one = unreadable_trash_warning(&["bad.trashinfo".to_string()]).unwrap();
        assert!(one.contains("bad.trashinfo"), "{one}");
        let many =
            unreadable_trash_warning(&["bad.trashinfo".to_string(), "worse".to_string()]).unwrap();
        assert!(many.contains('2'), "{many}");
    }

    #[test]
    fn restore_is_no_replace_and_removes_matching_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let original_parent = temp.path().join("original");
        fs::create_dir(&original_parent).unwrap();
        let record = seeded_record(&temp.path().join("Trash"), &original_parent, "note.txt");

        let restored = restore_trash_records(std::slice::from_ref(&record)).unwrap();

        assert_eq!(
            fs::read(original_parent.join("note.txt")).unwrap(),
            b"payload"
        );
        assert!(!record.backing_path.exists());
        assert!(!record.info_path.exists());
        assert!(restored.undoable);
        assert_eq!(
            restored.records[0].original_path(),
            original_parent.join("note.txt")
        );
    }

    /// A refusal raised before the first rename leaves the Trash exactly as the
    /// record describes, so the caller may keep the history entry and retry.
    #[test]
    fn a_restore_refused_before_committing_stays_retryable() {
        let temp = tempfile::tempdir().unwrap();
        let original_parent = temp.path().join("original");
        fs::create_dir(&original_parent).unwrap();
        let record = seeded_record(&temp.path().join("Trash"), &original_parent, "note.txt");
        File::create(original_parent.join("note.txt")).unwrap();

        let failure = restore_trash_records(std::slice::from_ref(&record))
            .expect_err("an occupied destination must refuse");

        assert!(
            !failure.committed,
            "a preflight refusal must stay retryable: {}",
            failure.error
        );
        assert!(record.backing_path.exists());
        assert!(record.info_path.exists());
    }

    /// Once a payload has been renamed out of Trash, compensation renames it
    /// back and moves its ctime, so the record can no longer validate and the
    /// caller must discard it.
    #[test]
    fn a_restore_that_rolls_back_reports_a_committed_failure() {
        use std::os::unix::fs::PermissionsExt as _;

        if rustix::process::geteuid().is_root() {
            // Permission bits do not constrain root, so the mid-loop failure
            // this test depends on cannot be provoked.
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let trash = temp.path().join("Trash");
        // Two original parents, so one can be sealed without blocking the
        // other. The preflight stats each destination and validates each
        // record before the first rename, so an obstacle it can see yields an
        // unchanged failure; reaching the rolled-back path needs one only the
        // rename itself discovers.
        let open = temp.path().join("open");
        let sealed = temp.path().join("sealed");
        fs::create_dir(&open).unwrap();
        fs::create_dir(&sealed).unwrap();
        let first = seeded_record(&trash, &open, "first.txt");
        let second = seeded_record(&trash, &sealed, "second.txt");

        // "first" restores into a writable parent and commits; "second" cannot
        // be created inside a read-only parent, though stat still succeeds.
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o555)).unwrap();
        let failure = restore_trash_records(&[first.clone(), second]);
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o755)).unwrap();

        let failure = failure.expect_err("a read-only parent must fail the restore");
        assert!(
            failure.committed,
            "a rolled-back restore must discard its record: {}",
            failure.error
        );
        // Compensation returned the first payload to Trash, so no user data was
        // stranded outside it.
        assert!(first.backing_path.exists());
        assert!(!first.original_path().exists());
    }

    #[test]
    fn restore_refuses_an_occupied_destination_without_moving_payload() {
        let temp = tempfile::tempdir().unwrap();
        let original_parent = temp.path().join("original");
        fs::create_dir(&original_parent).unwrap();
        let record = seeded_record(&temp.path().join("Trash"), &original_parent, "note.txt");
        File::create(original_parent.join("note.txt")).unwrap();

        assert!(restore_trash_records(std::slice::from_ref(&record)).is_err());
        assert!(record.backing_path.exists());
        assert!(record.info_path.exists());
    }

    #[test]
    fn restore_refuses_to_recreate_a_missing_original_parent() {
        let temp = tempfile::tempdir().unwrap();
        let original_parent = temp.path().join("missing");
        let record = seeded_record(&temp.path().join("Trash"), &original_parent, "note.txt");

        assert!(restore_trash_records(std::slice::from_ref(&record)).is_err());
        assert!(record.backing_path.exists());
    }

    #[test]
    fn restore_refuses_a_replaced_trash_payload() {
        let temp = tempfile::tempdir().unwrap();
        let original_parent = temp.path().join("original");
        fs::create_dir(&original_parent).unwrap();
        let record = seeded_record(&temp.path().join("Trash"), &original_parent, "note.txt");
        fs::remove_file(&record.backing_path).unwrap();
        fs::write(&record.backing_path, b"replacement").unwrap();

        assert!(restore_trash_records(std::slice::from_ref(&record)).is_err());
        assert!(!original_parent.join("note.txt").exists());
    }

    #[test]
    fn permanent_purge_removes_payload_and_matching_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let original_parent = temp.path().join("original");
        fs::create_dir(&original_parent).unwrap();
        let record = seeded_record(&temp.path().join("Trash"), &original_parent, "note.txt");

        let outcome = purge_trash_records(
            std::slice::from_ref(&record),
            Arc::new(TransferProgress::default()),
        );

        assert_eq!(outcome.completed, [original_parent.join("note.txt")]);
        assert!(outcome.failures.is_empty());
        assert!(!record.backing_path.exists());
        assert!(!record.info_path.exists());
        // The outcome names the exact record it purged: a Trash view must
        // reconcile by entry, not by original path, because two entries can
        // share one original.
        assert_eq!(
            outcome
                .records
                .iter()
                .map(|record| record.backing_path())
                .collect::<Vec<_>>(),
            [record.backing_path()]
        );
    }

    /// Two Trash entries can hold the same original path — the same file
    /// trashed twice. Reconciling a purge by original path removed both from
    /// the listing when only one was purged.
    #[test]
    fn purging_one_of_two_entries_sharing_an_original_names_only_the_purged_one() {
        let temp = tempfile::tempdir().unwrap();
        let original_parent = temp.path().join("original");
        fs::create_dir(&original_parent).unwrap();
        let first = seeded_record(&temp.path().join("TrashA"), &original_parent, "note.txt");
        let second = seeded_record(&temp.path().join("TrashB"), &original_parent, "note.txt");
        assert_eq!(first.original_path(), second.original_path());

        let outcome = purge_trash_records(
            std::slice::from_ref(&first),
            Arc::new(TransferProgress::default()),
        );

        assert!(outcome.failures.is_empty(), "{outcome:?}");
        assert_eq!(
            outcome
                .records
                .iter()
                .map(|record| record.backing_path())
                .collect::<Vec<_>>(),
            [first.backing_path()]
        );
        assert!(second.backing_path.exists());
    }

    #[test]
    fn metadata_cleanup_refuses_a_replaced_trash_info_file() {
        let temp = tempfile::tempdir().unwrap();
        let original_parent = temp.path().join("original");
        fs::create_dir(&original_parent).unwrap();
        let record = seeded_record(&temp.path().join("Trash"), &original_parent, "note.txt");
        fs::remove_file(&record.info_path).unwrap();
        fs::write(&record.info_path, b"replacement").unwrap();

        assert!(remove_matching_trash_info(&record).is_err());
        assert_eq!(fs::read(&record.info_path).unwrap(), b"replacement");
    }
}
