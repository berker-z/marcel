//! Permanent deletion: quarantine each root with one atomic rename, plan the
//! whole removal, then erase leaves before directories with an identity check
//! at every step.
//!
//! Conceptually adapted from Yazi's no-follow, leaf-before-directory delete
//! traversal and per-item scheduler outcomes (MIT, upstream commit
//! 319f90e0eab185a231eef5562215ba322e320286):
//! https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-scheduler/src/file/file.rs
//! https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-scheduler/src/file/traverse.rs
//!
//! Marcel adds atomic top-level quarantine, identity checks, whole-selection
//! preflight, and its own synchronous background-worker interface. No Yazi code
//! is copied here.

use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fs, io,
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use super::local::PathContext as _;
use anyhow::{Context as _, Result, bail};
use rustix::fs::{AtFlags, Mode, OFlags};

use super::{
    PathFailure, TransferProgress,
    identity::{FileIdentity, ObjectKey},
    local::{inspect, quarantined_name, rename_no_replace, sorted_children},
    trash::path_overlaps_system_trash,
};

static DELETE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// What a delete plan records about one object, beyond its identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DeleteIdentity {
    identity: FileIdentity,
    mode: u32,
    links: u64,
}

impl DeleteIdentity {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            identity: FileIdentity::of(metadata),
            mode: metadata.mode(),
            links: metadata.nlink(),
        }
    }

    fn same_object(self, found: Self) -> bool {
        (self.identity.key, self.mode) == (found.identity.key, found.mode)
    }

    /// Whether `found` is still the object this describes.
    ///
    /// Removing one hard link moves the shared inode's ctime, so a plan holding
    /// both links to a file cannot compare ctime for the second one: its own
    /// earlier removal is what changed it. Directories have the same problem
    /// with every child removed and are compared by `same_object` alone; a
    /// file gets the relaxation here instead, and only when it said up front
    /// that another name for it exists.
    ///
    /// Device, inode, and mode still pin the object either way.
    fn describes(self, found: Self) -> bool {
        self.same_object(found) && (self.identity == found.identity || self.links > 1)
    }
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

impl QuarantinedRoot {
    /// The path the user knows an entry by, rather than its staged one.
    fn display_path(&self, staged_path: &Path) -> PathBuf {
        match staged_path.strip_prefix(&self.quarantine) {
            Ok(relative) if !relative.as_os_str().is_empty() => self.original.join(relative),
            _ => self.original.clone(),
        }
    }
}

#[derive(Debug)]
pub struct DeleteOutcome {
    pub completed: Vec<PathBuf>,
    pub failures: Vec<PathFailure>,
}

impl DeleteOutcome {
    fn failed(path: PathBuf, message: impl Into<String>) -> Self {
        Self {
            completed: Vec::new(),
            failures: vec![PathFailure { path, message: message.into() }],
        }
    }

    pub fn summarize_failures(&self) -> String {
        PathFailure::summarize(&self.failures, "Permanent deletion failed")
    }
}

pub fn delete_paths(paths: &[PathBuf], progress: Arc<TransferProgress>) -> DeleteOutcome {
    delete_paths_with_policy(paths, &HashMap::new(), progress, false)
}

/// Delete Trash payloads a purge has already identified.
///
/// Each backing arrives with the key its record was validated against, so the
/// deletion refuses a substitution rather than trusting the path. A fresh stat
/// agrees with itself whatever happened since the caller decided to delete;
/// only the key carried across that boundary can tell the payload it approved
/// from what replaced it.
pub(super) fn delete_trash_backings(
    backings: &[(PathBuf, ObjectKey)],
    progress: Arc<TransferProgress>,
) -> DeleteOutcome {
    let paths = backings.iter().map(|(path, _)| path.clone()).collect::<Vec<_>>();
    let expected = backings.iter().cloned().collect::<HashMap<_, _>>();
    delete_paths_with_policy(&paths, &expected, progress, true)
}

fn delete_paths_with_policy(
    paths: &[PathBuf],
    expected_keys: &HashMap<PathBuf, ObjectKey>,
    progress: Arc<TransferProgress>,
    allow_trash_backings: bool,
) -> DeleteOutcome {
    progress.set_preparing(true);
    let mut quarantined = Vec::with_capacity(paths.len());

    for original in top_level_paths(paths) {
        let staged =
            stage_root(&original, expected_keys.get(&original).copied(), allow_trash_backings);
        match staged {
            Ok(quarantine) => {
                quarantined.push(QuarantinedRoot { original, quarantine, entries: Vec::new() })
            }
            Err(error) => {
                return failed_after_rollback(&quarantined, original, format!("{error:#}"));
            }
        }
    }

    for index in 0..quarantined.len() {
        let root = &mut quarantined[index];
        if let Err(error) = collect_delete_plan(&root.quarantine, &mut root.entries, &progress) {
            let original = root.original.clone();
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
        let root_failures = erase_root(&root, &progress);
        let root_failed = !root_failures.is_empty();
        failures.extend(root_failures);
        if !root_failed && fs::symlink_metadata(&root.quarantine).is_err() {
            completed.push(root.original);
        } else if !failures.iter().any(|failure| failure.path == root.original) {
            failures.push(PathFailure {
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
    DeleteOutcome { completed, failures }
}

/// Move one root aside under a quarantine name, proving it is still the same
/// object on both sides of the rename.
fn stage_root(
    original: &Path,
    expected_key: Option<ObjectKey>,
    allow_trash_backings: bool,
) -> Result<PathBuf> {
    if !allow_trash_backings && path_overlaps_system_trash(original)? {
        bail!(
            "Refusing to permanently delete “{}” because it is inside or contains a system Trash",
            original.display()
        );
    }
    let metadata = inspect(original)?;
    if original.file_name().is_none() {
        bail!("Refusing to permanently delete a filesystem root");
    }
    let key = ObjectKey::of(&metadata);
    if expected_key.is_some_and(|recorded| recorded != key) {
        bail!(
            "Cannot permanently delete “{}”: it changed or was replaced since it was checked",
            original.display()
        );
    }
    let quarantine = reserve_quarantine_path(original)?;
    let staging = key
        .validate(original)
        .and_then(|()| rename_no_replace(original, &quarantine).map_err(Into::into))
        .and_then(|()| match key.validate(&quarantine) {
            Ok(()) => Ok(()),
            Err(validation_error) => match rename_no_replace(&quarantine, original) {
                Ok(()) => Err(validation_error),
                Err(rollback_error) => Err(anyhow::anyhow!(
                    "{validation_error}; staging rollback also failed: {rollback_error}"
                )),
            },
        });
    staging.with_context(|| {
        format!("Could not safely stage “{}” for permanent deletion", original.display())
    })?;
    Ok(quarantine)
}

/// Erase one quarantined root leaf by leaf, returning what could not go.
///
/// Nothing here resolves a whole path. Each entry is unlinked on a descriptor
/// of its parent, and that parent is reached from the quarantine root one
/// component at a time with `O_NOFOLLOW`, each directory checked against the
/// plan as it is opened. A `remove_file` by path would resolve the path all over
/// again after the check, and a subdirectory swapped for a symbolic link in
/// that gap would carry the unlink to wherever the link points; opened this
/// way, the swap fails to open instead.
fn erase_root(root: &QuarantinedRoot, progress: &TransferProgress) -> Vec<PathFailure> {
    let directories = root
        .entries
        .iter()
        .filter(|entry| entry.kind == DeleteKind::Directory)
        .map(|entry| (entry.path.clone(), entry.identity))
        .collect::<HashMap<_, _>>();
    let mut ancestors = match OpenAncestors::open(&root.quarantine, &directories) {
        Ok(ancestors) => ancestors,
        Err(error) => {
            return vec![PathFailure { path: root.original.clone(), message: error.to_string() }];
        }
    };
    let mut failures = Vec::new();
    for entry in root.entries.iter().rev() {
        progress.set_current_path(Some(root.display_path(&entry.path)));
        let result = ancestors.parent_of(&entry.path).and_then(|parent| {
            let name = entry.path.file_name().context("Delete entry has no name")?;
            let pinned = pin_entry(parent, name, &entry.path)?;
            let found =
                DeleteIdentity::of(&pinned.metadata().at("Could not inspect", &entry.path)?);
            // Removing a directory's children moves its ctime, so the plan's
            // ctime cannot be held against it; device, inode, and mode still
            // pin it. `DeleteIdentity::describes` says why a file may relax.
            let still_planned = match entry.kind {
                DeleteKind::Directory => entry.identity.same_object(found),
                DeleteKind::Other => entry.identity.describes(found),
            };
            if !still_planned {
                bail!("Cannot continue: “{}” changed or was replaced", entry.path.display());
            }
            let flags = match entry.kind {
                DeleteKind::Directory => AtFlags::REMOVEDIR,
                DeleteKind::Other => AtFlags::empty(),
            };
            rustix::fs::unlinkat(parent, name, flags)
                .map_err(io::Error::from)
                .at("Could not permanently delete", &entry.path)?;
            progress.complete_item();
            progress.complete_bytes(entry.bytes);
            Ok(())
        });
        if let Err(error) = result {
            failures.push(PathFailure {
                path: root.display_path(&entry.path),
                message: error.to_string(),
            });
        }
    }
    failures
}

/// Descriptors for the directories a delete plan is currently erasing inside.
///
/// Entries arrive children before parents and siblings together, so the parent
/// of one entry is usually the parent of the next; that one descriptor is kept.
/// Any other parent is reached again from the pinned root, which costs one open
/// per component per change of directory and never holds more than three
/// descriptors, however deep the tree. Holding the whole chain open would be
/// faster and would run out of descriptors on a tree a few hundred levels deep.
///
/// The `openat`-based primitives here belong in `local.rs` beside
/// `open_regular_file`; they live here until that file is next touched.
struct OpenAncestors<'a> {
    quarantine: &'a Path,
    directories: &'a HashMap<PathBuf, DeleteIdentity>,
    /// The folder the quarantine root was renamed within. Opened by path, and
    /// following links: it is the folder the user is looking at, and the rename
    /// that staged the root resolved exactly the same way.
    outside: fs::File,
    /// The quarantine root itself, once a child has needed it.
    root: Option<fs::File>,
    /// The most recently used parent below the root.
    parent: Option<(PathBuf, fs::File)>,
}

impl<'a> OpenAncestors<'a> {
    fn open(
        quarantine: &'a Path,
        directories: &'a HashMap<PathBuf, DeleteIdentity>,
    ) -> Result<Self> {
        let outside = quarantine.parent().context("Quarantine path has no parent")?;
        let outside = rustix::fs::open(
            outside,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map(fs::File::from)
        .map_err(io::Error::from)
        .at("Could not open", outside)?;
        Ok(Self { quarantine, directories, outside, root: None, parent: None })
    }

    /// A descriptor for the directory holding `path`, every component below
    /// the quarantine root opened without following links and checked against
    /// the plan.
    fn parent_of(&mut self, path: &Path) -> Result<&fs::File> {
        let parent = path.parent().context("Delete entry has no parent")?;
        if !parent.starts_with(self.quarantine) {
            // The root itself is going: let its descriptors go first.
            self.parent = None;
            self.root = None;
            return Ok(&self.outside);
        }
        if self.root.is_none() {
            let name = self.quarantine.file_name().context("Quarantine path has no name")?;
            let root = open_directory_at(&self.outside, name, self.quarantine)?;
            check_planned(self.directories, self.quarantine, &root)?;
            self.root = Some(root);
        }
        let root = self.root.as_ref().context("Quarantine root is not open")?;
        if parent == self.quarantine {
            self.parent = None;
            return Ok(root);
        }
        if !self.parent.as_ref().is_some_and(|(open, _)| open == parent) {
            let mut current = self.quarantine.to_path_buf();
            let mut directory: Option<fs::File> = None;
            for component in parent.strip_prefix(self.quarantine)?.components() {
                current.push(component);
                let opened = open_directory_at(
                    directory.as_ref().unwrap_or(root),
                    component.as_os_str(),
                    &current,
                )?;
                check_planned(self.directories, &current, &opened)?;
                directory = Some(opened);
            }
            let directory = directory.context("Delete entry has no parent below the quarantine")?;
            self.parent = Some((parent.to_path_buf(), directory));
        }
        let (_, directory) = self.parent.as_ref().context("Parent directory is not open")?;
        Ok(directory)
    }
}

/// Refuse a directory that is not the one the plan recorded at `path`.
fn check_planned(
    directories: &HashMap<PathBuf, DeleteIdentity>,
    path: &Path,
    directory: &fs::File,
) -> Result<()> {
    let expected = directories.get(path).with_context(|| {
        format!("Cannot continue: ancestor “{}” was not in the delete plan", path.display())
    })?;
    let found = DeleteIdentity::of(&directory.metadata().at("Could not inspect", path)?);
    if !expected.same_object(found) {
        bail!("Cannot continue: ancestor “{}” changed or was replaced", path.display());
    }
    Ok(())
}

/// Open the directory named `name` inside `directory`, refusing a link.
///
/// `path` is what the directory is called in messages; the syscall itself
/// never sees more than the one name.
fn open_directory_at(directory: &fs::File, name: &OsStr, path: &Path) -> Result<fs::File> {
    rustix::fs::openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(fs::File::from)
    .map_err(|error| {
        anyhow::anyhow!(
            "Cannot continue: “{}” is no longer a directory that can be opened: {}",
            path.display(),
            io::Error::from(error)
        )
    })
}

/// A descriptor for whatever `name` is inside `directory`, without following a
/// link and without opening the object.
///
/// `O_PATH` is what lets this describe anything the plan may hold: a FIFO is
/// not opened, so nothing blocks; a device is not opened, so nothing is
/// triggered; a socket, which cannot be opened at all, still yields a
/// descriptor to `fstat`.
fn pin_entry(directory: &fs::File, name: &OsStr, path: &Path) -> Result<fs::File> {
    rustix::fs::openat(
        directory,
        name,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(fs::File::from)
    .map_err(|error| {
        anyhow::anyhow!(
            "Cannot continue: “{}” could not be inspected: {}",
            path.display(),
            io::Error::from(error)
        )
    })
}

/// Build the delete plan in pre-order using an explicit stack.
///
/// The plan is executed in reverse, so parents must precede their children.
/// Recursing here risked a stack overflow — which aborts the process rather
/// than reporting a failure — on a deep enough quarantined tree.
fn collect_delete_plan(
    path: &Path,
    entries: &mut Vec<DeleteEntry>,
    progress: &TransferProgress,
) -> Result<()> {
    let mut pending = vec![path.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = inspect(&path)?;
        let kind =
            if metadata.file_type().is_dir() { DeleteKind::Directory } else { DeleteKind::Other };
        let entry = DeleteEntry {
            path: path.clone(),
            identity: DeleteIdentity::of(&metadata),
            kind,
            bytes: if metadata.file_type().is_file() { metadata.len() } else { 0 },
        };
        progress.add_total(1, entry.bytes);
        entries.push(entry);
        if kind == DeleteKind::Directory {
            pending.extend(sorted_children(&path)?.into_iter().rev().map(|e| e.path()));
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

/// Deduplicate and drop paths that another requested path already contains.
fn top_level_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut unique = paths.iter().cloned().collect::<HashSet<_>>().into_iter().collect::<Vec<_>>();
    unique.sort();
    let all = unique.clone();
    unique.retain(|path| {
        !all.iter().any(|candidate| candidate != path && path.starts_with(candidate))
    });
    unique
}

fn reserve_quarantine_path(original: &Path) -> Result<PathBuf> {
    let parent = original.parent().context("Delete target has no parent")?;
    let name = original.file_name().context("Delete target has no name")?;
    let prefix = format!(".marcel-delete-{}-", std::process::id());
    for _ in 0..1024 {
        let sequence = DELETE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(quarantined_name(&prefix, sequence, name));
        match fs::symlink_metadata(&candidate) {
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(candidate),
            Err(error) => {
                return Err(error).at("Could not inspect quarantine path", &candidate);
            }
        }
    }
    bail!("Could not reserve a unique permanent-delete quarantine path")
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
    DeleteOutcome::failed(path, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Sandbox, read};

    fn delete(paths: &[PathBuf]) -> DeleteOutcome {
        delete_paths(paths, Arc::new(TransferProgress::default()))
    }

    fn key(path: &Path) -> ObjectKey {
        ObjectKey::of(&fs::symlink_metadata(path).unwrap())
    }

    /// Removing one hard link moves the shared inode's ctime, which used to
    /// make the plan's own first removal invalidate the entry for the second.
    /// Copy preserves hard links, so a tree like this is one paste away.
    #[test]
    fn a_tree_holding_two_links_to_one_file_is_deleted_completely() {
        let sandbox = Sandbox::new();
        let target = sandbox.dir("tree");
        sandbox.file("tree/first", b"shared");
        fs::hard_link(target.join("first"), target.join("second")).unwrap();

        let outcome = delete(std::slice::from_ref(&target));

        assert!(outcome.failures.is_empty(), "{outcome:?}");
        assert!(!target.exists());
    }

    /// A Trash purge validates the payload, then hands this the path. Between
    /// those two moments the object can be replaced, and a fresh `stat` cannot
    /// tell: only the key the caller carried across can.
    #[test]
    fn a_trash_backing_that_changed_since_it_was_checked_is_refused() {
        let sandbox = Sandbox::new();
        let backing = sandbox.file("payload.txt", b"the object the purge approved");
        let approved = key(&backing);

        // Something else takes the path afterwards, published the way anything
        // is published atomically. Both files exist at once, so the replacement
        // cannot be handed the inode number the original still holds.
        let replacement = sandbox.file("elsewhere.txt", b"someone else's data");
        fs::rename(&replacement, &backing).unwrap();

        let outcome = delete_trash_backings(
            &[(backing.clone(), approved)],
            Arc::new(TransferProgress::default()),
        );

        assert_eq!(outcome.failures.len(), 1, "{outcome:?}");
        assert!(outcome.completed.is_empty(), "{outcome:?}");
        assert_eq!(read(&backing), b"someone else's data");

        // The payload it actually approved is deleted as before.
        let outcome = delete_trash_backings(
            &[(backing.clone(), key(&backing))],
            Arc::new(TransferProgress::default()),
        );
        assert!(outcome.failures.is_empty(), "{outcome:?}");
        assert!(!backing.exists());
    }

    #[test]
    fn permanently_deletes_files_directories_and_symlinks_without_following() {
        let sandbox = Sandbox::new();
        let outside = sandbox.dir("outside");
        let kept = sandbox.file("outside/keep.txt", b"keep");
        let target = sandbox.dir("target");
        sandbox.file("target/file.txt", b"delete");
        std::os::unix::fs::symlink(&outside, target.join("link")).unwrap();
        let progress = Arc::new(TransferProgress::default());

        let outcome = delete_paths(std::slice::from_ref(&target), progress.clone());

        assert_eq!(outcome.completed, vec![target.clone()], "{:#?}", outcome.failures);
        assert!(outcome.failures.is_empty(), "{:#?}", outcome.failures);
        assert!(!target.exists());
        assert_eq!(read(kept), b"keep");
        let snapshot = progress.snapshot();
        assert_eq!(snapshot.completed_items, snapshot.total_items);
    }

    #[test]
    fn occupied_quarantine_names_do_not_overwrite() {
        let sandbox = Sandbox::new();
        let target = sandbox.file("note.txt", b"delete");
        let collision =
            sandbox.file(&format!(".marcel-delete-{}-0-note.txt", std::process::id()), b"keep");

        let outcome = delete(std::slice::from_ref(&target));

        assert!(outcome.failures.is_empty(), "{:#?}", outcome.failures);
        assert_eq!(read(collision), b"keep");
    }

    #[test]
    fn nested_selected_paths_are_deleted_once_by_their_top_level_root() {
        let sandbox = Sandbox::new();
        let target = sandbox.dir("target");
        let child = sandbox.file("target/child.txt", b"delete");

        let outcome = delete(&[target.clone(), child]);

        assert_eq!(outcome.completed, [target], "{:#?}", outcome.failures);
        assert!(outcome.failures.is_empty(), "{:#?}", outcome.failures);
    }

    /// A root already staged and planned, as `delete_paths_with_policy` hands
    /// it to `erase_root`.
    fn planned_root(sandbox: &Sandbox, quarantine: &str) -> QuarantinedRoot {
        let mut root = QuarantinedRoot {
            original: sandbox.path("target"),
            quarantine: sandbox.path(quarantine),
            entries: Vec::new(),
        };
        collect_delete_plan(&root.quarantine, &mut root.entries, &TransferProgress::default())
            .unwrap();
        root
    }

    /// The plan listed `child/` as a directory; before it is erased, something
    /// with write access to the quarantine swaps it for a link to a folder the
    /// user never selected. By path, `remove_file(child/keep.txt)` would
    /// follow the link and delete the target's file.
    #[test]
    fn a_subdirectory_swapped_for_a_link_after_planning_does_not_delete_its_target() {
        let sandbox = Sandbox::new();
        sandbox.file("quarantine/child/keep.txt", b"doomed");
        let outside = sandbox.dir("outside");
        let kept = sandbox.file("outside/keep.txt", b"keep");
        let root = planned_root(&sandbox, "quarantine");
        assert_eq!(root.entries.len(), 3);

        let child = sandbox.path("quarantine/child");
        fs::remove_file(child.join("keep.txt")).unwrap();
        fs::remove_dir(&child).unwrap();
        std::os::unix::fs::symlink(&outside, &child).unwrap();

        let failures = erase_root(&root, &TransferProgress::default());

        assert!(!failures.is_empty(), "the swap must be reported, not walked through");
        assert_eq!(read(&kept), b"keep");
        assert!(fs::symlink_metadata(&child).unwrap().file_type().is_symlink());
        // Nothing was erased, so the quarantine stands for the user to inspect.
        assert!(root.quarantine.is_dir());
    }

    /// The same swap one level up: the quarantine root itself becomes a link.
    /// Its own entry then fails the identity check rather than `rmdir`-ing
    /// through the link.
    #[test]
    fn a_quarantine_root_swapped_for_a_link_is_refused() {
        let sandbox = Sandbox::new();
        sandbox.file("quarantine/note.txt", b"doomed");
        let outside = sandbox.dir("outside");
        let kept = sandbox.file("outside/note.txt", b"keep");
        let root = planned_root(&sandbox, "quarantine");

        fs::remove_file(root.quarantine.join("note.txt")).unwrap();
        fs::remove_dir(&root.quarantine).unwrap();
        std::os::unix::fs::symlink(&outside, &root.quarantine).unwrap();

        let failures = erase_root(&root, &TransferProgress::default());

        assert_eq!(failures.len(), 2, "{failures:#?}");
        assert_eq!(read(&kept), b"keep");
        assert!(outside.is_dir());
    }

    /// Special files cannot be opened without consequence, but they can be
    /// pinned and identified, so a tree holding one is still deleted.
    #[test]
    fn a_tree_holding_a_socket_and_a_fifo_is_deleted_completely() {
        let sandbox = Sandbox::new();
        let target = sandbox.dir("tree");
        let _listener = sandbox.socket("tree/socket");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            target.join("fifo"),
            rustix::fs::FileType::Fifo,
            Mode::RUSR | Mode::WUSR,
            0,
        )
        .unwrap();

        let outcome = delete(std::slice::from_ref(&target));

        assert!(outcome.failures.is_empty(), "{outcome:?}");
        assert!(!target.exists());
    }
}
