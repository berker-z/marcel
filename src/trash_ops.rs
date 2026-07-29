use std::{
    collections::HashSet,
    ffi::OsString,
    fs, io,
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result, bail};

use crate::{delete_ops::delete_trash_backings, file_ops::TransferProgress};

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

pub fn list_trash_records() -> Result<Vec<TrashRecord>> {
    let records = trash::os_limited::list()
        .context("Could not inspect the system Trash")?
        .into_iter()
        .filter_map(|item| record_from_item(item).ok())
        .collect();
    Ok(records)
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

    let backing_paths = records
        .iter()
        .map(|record| record.backing_path.clone())
        .collect::<Vec<_>>();
    let deleted = delete_trash_backings(&backing_paths, progress);
    let mut completed = Vec::new();
    let mut failures = deleted
        .failures
        .into_iter()
        .map(|failure| TrashFailure {
            path: map_backing_to_original(records, &failure.path),
            message: failure.message,
        })
        .collect::<Vec<_>>();

    for backing in deleted.completed {
        let Some(record) = records.iter().find(|record| record.backing_path == backing) else {
            continue;
        };
        completed.push(record.original_path.clone());
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
        records: Vec::new(),
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
        let candidate = candidates
            .iter()
            .find_map(|(index, identity_matches, _)| identity_matches.then_some(*index))
            .or_else(|| {
                let matching_paths = candidates
                    .iter()
                    .filter(|(_, _, path_matches)| *path_matches)
                    .collect::<Vec<_>>();
                (matching_paths.len() == 1).then_some(matching_paths[0].0)
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

pub fn restore_trash_records(records: &[TrashRecord]) -> Result<Vec<TrashRecord>> {
    for record in records {
        validate_identity(&record.info_path, &record.info_identity, "Trash metadata")?;
        validate_identity(
            &record.backing_path,
            &record.payload_identity,
            "Trash payload",
        )?;
        let parent = fs::symlink_metadata(&record.original_parent).with_context(|| {
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
        ensure_unoccupied(record.original_path())?;
    }

    let mut restored = Vec::with_capacity(records.len());
    for record in records {
        let original = record.original_path().to_path_buf();
        if let Err(error) = rename_no_replace(&record.backing_path, &original) {
            let rollback = rollback_restored(&restored);
            let message = format!(
                "Could not restore “{}” from Trash: {error}",
                original.display()
            );
            return match rollback {
                Ok(()) => Err(anyhow::anyhow!(
                    "{message}; earlier restores were rolled back"
                )),
                Err(rollback_error) => Err(anyhow::anyhow!(
                    "{message}; rollback also failed: {rollback_error}"
                )),
            };
        }
        restored.push(record);
    }

    let mut result = Vec::with_capacity(records.len());
    for record in records {
        // The payload is already safely restored. A failed metadata cleanup
        // leaves only an orphaned Trash entry, never missing user data.
        let _ = fs::remove_file(&record.info_path);
        let original = record.original_path();
        let metadata = fs::symlink_metadata(original).with_context(|| {
            format!("Restored “{}” but could not inspect it", original.display())
        })?;
        let mut updated = record.clone();
        updated.payload_identity = identity(&metadata);
        result.push(updated);
    }
    Ok(result)
}

pub fn retrash_records(records: &[TrashRecord]) -> Result<Vec<TrashRecord>> {
    for record in records {
        validate_identity(
            record.original_path(),
            &record.payload_identity,
            "Restored item",
        )?;
    }
    let originals = records
        .iter()
        .map(|record| record.original_path().to_path_buf())
        .collect::<Vec<_>>();
    let outcome = trash_paths(&originals);
    if outcome.failures.is_empty()
        && !outcome.undo_unavailable
        && outcome.records.len() == originals.len()
    {
        return Ok(outcome.records);
    }

    let failure = summarize_failures(&outcome.failures);
    if !outcome.records.is_empty() {
        match restore_trash_records(&outcome.records) {
            Ok(_) => bail!("{failure}; completed items were restored"),
            Err(rollback_error) => {
                bail!("{failure}; restore rollback also failed: {rollback_error}");
            }
        }
    }
    bail!("{failure}");
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
    match fs::symlink_metadata(path) {
        Ok(_) => bail!("Cannot restore: “{}” is already occupied", path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
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

fn rename_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        source,
        rustix::fs::CWD,
        destination,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(Into::into)
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

fn paths_overlap_trash_root(path: &Path, trash_root: &Path) -> bool {
    path.starts_with(trash_root) || trash_root.starts_with(path)
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
        assert_eq!(
            restored[0].original_path(),
            original_parent.join("note.txt")
        );
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
