use std::{
    collections::{HashMap, HashSet},
    fs, io,
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context as _, Result, bail};

use crate::{file_ops::TransferProgress, trash_ops::path_overlaps_system_trash};

// Conceptually adapted from Yazi's no-follow, leaf-before-directory delete
// traversal and per-item scheduler outcomes (MIT, upstream commit
// 319f90e0eab185a231eef5562215ba322e320286):
// https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-scheduler/src/file/file.rs
// https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-scheduler/src/file/traverse.rs
// https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-fs/src/engine/traits.rs
//
// Marcel adds atomic top-level quarantine, identity checks, whole-selection
// preflight, and its own synchronous background-worker interface. No Yazi code
// is copied here.

static DELETE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DeleteIdentity {
    device: u64,
    inode: u64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
    mode: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RenameIdentity {
    device: u64,
    inode: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeleteKind {
    Directory,
    Other,
}

#[derive(Clone, Debug)]
struct DeleteEntry {
    path: PathBuf,
    identity: DeleteIdentity,
    kind: DeleteKind,
    bytes: u64,
}

#[derive(Clone, Debug)]
struct QuarantinedRoot {
    original: PathBuf,
    quarantine: PathBuf,
    entries: Vec<DeleteEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteFailure {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug)]
pub struct DeleteOutcome {
    pub completed: Vec<PathBuf>,
    pub failures: Vec<DeleteFailure>,
}

pub fn delete_paths(paths: &[PathBuf], progress: Arc<TransferProgress>) -> DeleteOutcome {
    delete_paths_with_policy(paths, progress, false)
}

pub(crate) fn delete_trash_backings(
    paths: &[PathBuf],
    progress: Arc<TransferProgress>,
) -> DeleteOutcome {
    delete_paths_with_policy(paths, progress, true)
}

fn delete_paths_with_policy(
    paths: &[PathBuf],
    progress: Arc<TransferProgress>,
    allow_trash_backings: bool,
) -> DeleteOutcome {
    progress.set_preparing(true);
    let paths = top_level_paths(paths);
    let mut quarantined = Vec::with_capacity(paths.len());

    for original in paths {
        if !allow_trash_backings {
            match path_overlaps_system_trash(&original) {
                Ok(true) => {
                    return failed_after_rollback(
                        &quarantined,
                        original.clone(),
                        format!(
                            "Refusing to permanently delete “{}” because it is inside or contains a system Trash",
                            original.display()
                        ),
                    );
                }
                Err(error) => {
                    return failed_after_rollback(&quarantined, original, error.to_string());
                }
                Ok(false) => {}
            }
        }
        let metadata = match fs::symlink_metadata(&original) {
            Ok(metadata) => metadata,
            Err(error) => {
                return failed_after_rollback(
                    &quarantined,
                    original.clone(),
                    format!("Could not inspect “{}”: {error}", original.display()),
                );
            }
        };
        if original.file_name().is_none() {
            return failed_after_rollback(
                &quarantined,
                original,
                "Refusing to permanently delete a filesystem root",
            );
        }
        let expected = rename_identity(&metadata);
        let quarantine = match reserve_quarantine_path(&original) {
            Ok(path) => path,
            Err(error) => {
                return failed_after_rollback(&quarantined, original, error.to_string());
            }
        };
        let staging = validate_rename_identity(&original, expected)
            .and_then(|()| rename_no_replace(&original, &quarantine).map_err(Into::into))
            .and_then(|()| {
                if let Err(validation_error) = validate_rename_identity(&quarantine, expected) {
                    return match rename_no_replace(&quarantine, &original) {
                        Ok(()) => Err(validation_error),
                        Err(rollback_error) => Err(anyhow::anyhow!(
                            "{validation_error}; staging rollback also failed: {rollback_error}"
                        )),
                    };
                }
                Ok(())
            });
        if let Err(error) = staging {
            return failed_after_rollback(
                &quarantined,
                original.clone(),
                format!(
                    "Could not safely stage “{}” for permanent deletion: {error}",
                    original.display()
                ),
            );
        }
        quarantined.push(QuarantinedRoot {
            original,
            quarantine,
            entries: Vec::new(),
        });
    }

    for index in 0..quarantined.len() {
        let planning = {
            let root = &mut quarantined[index];
            collect_delete_plan(&root.quarantine, &mut root.entries, &progress)
                .map_err(|error| (root.original.clone(), error))
        };
        if let Err((original, error)) = planning {
            return failed_after_rollback(
                &quarantined,
                original.clone(),
                format!(
                    "Could not prepare “{}” for permanent deletion: {error}",
                    original.display()
                ),
            );
        }
    }
    progress.set_preparing(false);

    let mut completed = Vec::with_capacity(quarantined.len());
    let mut failures = Vec::new();
    for root in quarantined {
        let mut directory_identities = root
            .entries
            .iter()
            .filter(|entry| entry.kind == DeleteKind::Directory)
            .map(|entry| (entry.path.clone(), entry.identity))
            .collect::<HashMap<_, _>>();
        let mut root_failed = false;
        for entry in root.entries.iter().rev() {
            progress.set_current_path(Some(display_path(&root, &entry.path)));
            let expected = match entry.kind {
                DeleteKind::Directory => directory_identities
                    .get(&entry.path)
                    .copied()
                    .unwrap_or(entry.identity),
                DeleteKind::Other => entry.identity,
            };
            let result = validate_ancestors(&entry.path, &root.quarantine, &directory_identities)
                .and_then(|()| validate_identity(&entry.path, expected))
                .and_then(|()| {
                    match entry.kind {
                        DeleteKind::Directory => fs::remove_dir(&entry.path),
                        DeleteKind::Other => fs::remove_file(&entry.path),
                    }
                    .with_context(|| {
                        format!("Could not permanently delete “{}”", entry.path.display())
                    })
                });
            match result {
                Ok(()) => {
                    progress.complete_item();
                    progress.complete_bytes(entry.bytes);
                    if let Err(error) = refresh_parent_identity(
                        &entry.path,
                        &root.quarantine,
                        &mut directory_identities,
                    ) {
                        root_failed = true;
                        failures.push(DeleteFailure {
                            path: display_path(&root, &entry.path),
                            message: error.to_string(),
                        });
                    }
                }
                Err(error) => {
                    root_failed = true;
                    failures.push(DeleteFailure {
                        path: display_path(&root, &entry.path),
                        message: error.to_string(),
                    });
                }
            }
        }
        if !root_failed && fs::symlink_metadata(&root.quarantine).is_err() {
            completed.push(root.original);
        } else if !failures.iter().any(|failure| failure.path == root.original) {
            failures.push(DeleteFailure {
                path: root.original.clone(),
                message: format!(
                    "Permanent deletion of “{}” was incomplete; remaining data is quarantined as “{}”",
                    root.original.display(),
                    root.quarantine.display()
                ),
            });
        }
    }
    progress.set_current_path(None);
    DeleteOutcome {
        completed,
        failures,
    }
}

pub fn summarize_failures(failures: &[DeleteFailure]) -> String {
    match failures {
        [] => "Permanent deletion failed".to_string(),
        [failure] => failure.message.clone(),
        [first, rest @ ..] => format!(
            "{} (and {} more failure{})",
            first.message,
            rest.len(),
            if rest.len() == 1 { "" } else { "s" }
        ),
    }
}

fn collect_delete_plan(
    path: &Path,
    entries: &mut Vec<DeleteEntry>,
    progress: &TransferProgress,
) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Could not inspect “{}”", path.display()))?;
    let kind = if metadata.file_type().is_dir() {
        DeleteKind::Directory
    } else {
        DeleteKind::Other
    };
    let entry = DeleteEntry {
        path: path.to_path_buf(),
        identity: identity(&metadata),
        kind,
        bytes: if metadata.file_type().is_file() {
            metadata.len()
        } else {
            0
        },
    };
    progress.add_total(1, entry.bytes);
    entries.push(entry);

    if kind == DeleteKind::Directory {
        let mut children = fs::read_dir(path)
            .with_context(|| format!("Could not read “{}”", path.display()))?
            .collect::<io::Result<Vec<_>>>()
            .with_context(|| format!("Could not read an entry in “{}”", path.display()))?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            collect_delete_plan(&child.path(), entries, progress)?;
        }
    }
    Ok(())
}

fn rollback_quarantines(quarantined: &[QuarantinedRoot]) -> Result<()> {
    for root in quarantined.iter().rev() {
        rename_no_replace(&root.quarantine, &root.original).with_context(|| {
            format!(
                "Could not restore staged delete target “{}” from “{}”",
                root.original.display(),
                root.quarantine.display()
            )
        })?;
    }
    Ok(())
}

fn top_level_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut unique = paths
        .iter()
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    unique.sort();
    let all = unique.clone();
    unique.retain(|path| {
        !all.iter()
            .any(|candidate| candidate != path && path.starts_with(candidate))
    });
    unique
}

fn reserve_quarantine_path(original: &Path) -> Result<PathBuf> {
    let parent = original.parent().context("Delete target has no parent")?;
    let name = original.file_name().context("Delete target has no name")?;
    for _ in 0..1024 {
        let sequence = DELETE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut quarantine_name = format!(".marcel-delete-{}-{sequence}-", std::process::id());
        quarantine_name.push_str(&name.to_string_lossy());
        let candidate = parent.join(quarantine_name);
        match fs::symlink_metadata(&candidate) {
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(candidate),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Could not inspect quarantine path “{}”",
                        candidate.display()
                    )
                });
            }
        }
    }
    bail!("Could not reserve a unique permanent-delete quarantine path")
}

fn display_path(root: &QuarantinedRoot, staged_path: &Path) -> PathBuf {
    let relative = staged_path
        .strip_prefix(&root.quarantine)
        .unwrap_or_else(|_| Path::new(""));
    if relative.as_os_str().is_empty() {
        root.original.clone()
    } else {
        root.original.join(relative)
    }
}

fn validate_identity(path: &Path, expected: DeleteIdentity) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Cannot continue: “{}” is missing", path.display()))?;
    if identity(&metadata) != expected {
        bail!(
            "Cannot continue: “{}” changed or was replaced",
            path.display()
        );
    }
    Ok(())
}

fn validate_rename_identity(path: &Path, expected: RenameIdentity) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Cannot continue: “{}” is missing", path.display()))?;
    if rename_identity(&metadata) != expected {
        bail!(
            "Cannot continue: “{}” changed or was replaced",
            path.display()
        );
    }
    Ok(())
}

fn validate_ancestors(
    path: &Path,
    quarantine_root: &Path,
    directories: &HashMap<PathBuf, DeleteIdentity>,
) -> Result<()> {
    let mut ancestor = path.parent();
    while let Some(directory) = ancestor {
        if !directory.starts_with(quarantine_root) {
            break;
        }
        let expected = directories.get(directory).with_context(|| {
            format!(
                "Cannot continue: ancestor “{}” was not in the delete plan",
                directory.display()
            )
        })?;
        validate_identity(directory, *expected)?;
        if directory == quarantine_root {
            break;
        }
        ancestor = directory.parent();
    }
    Ok(())
}

fn refresh_parent_identity(
    removed_path: &Path,
    quarantine_root: &Path,
    directories: &mut HashMap<PathBuf, DeleteIdentity>,
) -> Result<()> {
    let Some(parent) = removed_path.parent() else {
        return Ok(());
    };
    if !parent.starts_with(quarantine_root) {
        return Ok(());
    }
    let previous = directories.get(parent).copied().with_context(|| {
        format!(
            "Cannot continue: parent “{}” was not in the delete plan",
            parent.display()
        )
    })?;
    let metadata = fs::symlink_metadata(parent)
        .with_context(|| format!("Cannot continue: parent “{}” is missing", parent.display()))?;
    let current = identity(&metadata);
    if current.device != previous.device
        || current.inode != previous.inode
        || current.mode != previous.mode
    {
        bail!(
            "Cannot continue: parent “{}” changed or was replaced",
            parent.display()
        );
    }
    directories.insert(parent.to_path_buf(), current);
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

fn identity(metadata: &fs::Metadata) -> DeleteIdentity {
    DeleteIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
        mode: metadata.mode(),
    }
}

fn rename_identity(metadata: &fs::Metadata) -> RenameIdentity {
    RenameIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

fn failed(path: PathBuf, message: impl Into<String>) -> DeleteOutcome {
    DeleteOutcome {
        completed: Vec::new(),
        failures: vec![DeleteFailure {
            path,
            message: message.into(),
        }],
    }
}

fn failed_after_rollback(
    quarantined: &[QuarantinedRoot],
    path: PathBuf,
    message: impl Into<String>,
) -> DeleteOutcome {
    let mut message = message.into();
    if let Err(error) = rollback_quarantines(quarantined) {
        message.push_str(&format!("; staging rollback also failed: {error}"));
    }
    failed(path, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permanently_deletes_files_directories_and_symlinks_without_following() {
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep.txt"), b"keep").unwrap();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("file.txt"), b"delete").unwrap();
        std::os::unix::fs::symlink(&outside, target.join("link")).unwrap();
        let progress = Arc::new(TransferProgress::default());

        let outcome = delete_paths(std::slice::from_ref(&target), progress.clone());

        assert_eq!(outcome.completed, vec![target.clone()]);
        assert!(outcome.failures.is_empty());
        assert!(!target.exists());
        assert_eq!(fs::read(outside.join("keep.txt")).unwrap(), b"keep");
        let snapshot = progress.snapshot();
        assert_eq!(snapshot.completed_items, snapshot.total_items);
    }

    #[test]
    fn occupied_quarantine_names_do_not_overwrite() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("note.txt");
        fs::write(&target, b"delete").unwrap();
        let collision = temp
            .path()
            .join(format!(".marcel-delete-{}-0-note.txt", std::process::id()));
        fs::write(&collision, b"keep").unwrap();

        let outcome = delete_paths(
            std::slice::from_ref(&target),
            Arc::new(TransferProgress::default()),
        );

        assert!(outcome.failures.is_empty());
        assert_eq!(fs::read(collision).unwrap(), b"keep");
    }

    #[test]
    fn nested_selected_paths_are_deleted_once_by_their_top_level_root() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        let child = target.join("child.txt");
        fs::write(&child, b"delete").unwrap();

        let outcome = delete_paths(
            &[target.clone(), child],
            Arc::new(TransferProgress::default()),
        );

        assert_eq!(outcome.completed, [target]);
        assert!(outcome.failures.is_empty());
    }

    #[test]
    fn ancestor_replacement_cannot_redirect_deletion_through_a_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let quarantine = temp.path().join("quarantine");
        let child_dir = quarantine.join("child");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&child_dir).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep.txt"), b"keep").unwrap();
        let root_identity = identity(&fs::symlink_metadata(&quarantine).unwrap());
        let child_identity = identity(&fs::symlink_metadata(&child_dir).unwrap());
        let directories = HashMap::from([
            (quarantine.clone(), root_identity),
            (child_dir.clone(), child_identity),
        ]);
        fs::remove_dir(&child_dir).unwrap();
        std::os::unix::fs::symlink(&outside, &child_dir).unwrap();

        assert!(
            validate_ancestors(&child_dir.join("keep.txt"), &quarantine, &directories).is_err()
        );
        assert_eq!(fs::read(outside.join("keep.txt")).unwrap(), b"keep");
    }
}
