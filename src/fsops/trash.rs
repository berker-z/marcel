//! The freedesktop Trash: placing items in it, listing it, restoring from it,
//! and purging it, with a record for each entry so Undo knows exactly which
//! payload it is talking about.
//!
//! Conceptually adapted from Yazi's separation between its background file
//! scheduler and freedesktop Trash VFS:
//! https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-scheduler/src/file/file.rs
//! https://github.com/sxyazi/yazi/blob/319f90e0eab185a231eef5562215ba322e320286/yazi-fs/src/trash/freedesktop/trash.rs
//!
//! Like Yazi, Marcel delegates platform Trash placement to the MIT-licensed
//! `trash` crate. Marcel adds operation-journal identities and stricter restore
//! rules behind its own interface. No Yazi code is copied here.

use std::{
    collections::HashSet,
    ffi::OsStr,
    fs, io,
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    sync::Arc,
};

use super::local::PathContext as _;
use anyhow::{Context as _, Result, bail};

use super::{
    PathFailure, TransferProgress,
    delete::delete_trash_backings,
    identity::{FileIdentity, ObjectKey},
    local::{PathOccupancy, create_private_dir_all, inspect, path_occupancy, rename_no_replace},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashRecord {
    original_path: PathBuf,
    info_path: PathBuf,
    backing_path: PathBuf,
    original_parent: PathBuf,
    info_identity: FileIdentity,
    payload_identity: FileIdentity,
}

impl TrashRecord {
    pub fn original_path(&self) -> &Path {
        &self.original_path
    }

    pub fn backing_path(&self) -> &Path {
        &self.backing_path
    }

    fn from_item(item: trash::TrashItem) -> Result<Self> {
        let info_path = PathBuf::from(&item.id);
        let backing_path = backing_path_from_info(&info_path)?;
        let info_metadata = inspect(&info_path)?;
        if !info_metadata.file_type().is_file() {
            bail!("Trash metadata is not a regular file");
        }
        let payload_identity = FileIdentity::read(&backing_path)?;
        Ok(Self {
            original_path: item.original_path(),
            info_path,
            backing_path,
            original_parent: item.original_parent,
            info_identity: FileIdentity::of(&info_metadata),
            payload_identity,
        })
    }

    /// Both halves of the entry are still what the record describes.
    fn validate(&self) -> Result<()> {
        self.info_identity.validate(&self.info_path, "continue")?;
        self.payload_identity.validate(&self.backing_path, "continue")
    }

    /// Remove the `.trashinfo` half, but only the one this record describes:
    /// a replacement metadata file for an unrelated entry must never be the
    /// thing that gets removed.
    fn remove_matching_info(&self) -> Result<()> {
        self.info_identity.validate(&self.info_path, "continue")?;
        fs::remove_file(&self.info_path).at("Could not remove Trash metadata", &self.info_path)
    }
}

#[derive(Debug)]
pub struct TrashOutcome {
    /// The exact records this outcome affected. For a trash placement these
    /// carry undo; for a purge they identify what left the Trash, because two
    /// entries can share one original path and only the record can tell a
    /// purged entry from its surviving twin.
    pub records: Vec<TrashRecord>,
    pub completed: Vec<PathBuf>,
    pub failures: Vec<PathFailure>,
    pub undo_unavailable: bool,
}

impl TrashOutcome {
    /// Every path failed the same way, before anything was attempted.
    fn all_failed(paths: &[PathBuf], message: impl Fn() -> String) -> Self {
        Self {
            records: Vec::new(),
            completed: Vec::new(),
            failures: paths.iter().map(|path| PathFailure::new(path, message())).collect(),
            undo_unavailable: false,
        }
    }

    pub fn summarize_failures(&self) -> String {
        PathFailure::summarize(&self.failures, "Trash operation could not be recorded safely")
    }
}

pub fn path_overlaps_system_trash(path: &Path) -> Result<bool> {
    ensure_home_trash();
    let roots = trash::os_limited::trash_folders().context("Could not resolve the system Trash")?;
    Ok(roots.iter().any(|root| paths_overlap_trash_root(path, root)))
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
    ensure_home_trash();
    let mut listing = TrashListing::default();
    for item in trash::os_limited::list().context("Could not inspect the system Trash")? {
        match TrashRecord::from_item(item) {
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
        if let Err(error) = record.validate() {
            return TrashOutcome {
                records: Vec::new(),
                completed: Vec::new(),
                failures: vec![PathFailure::new(&record.original_path, error.to_string())],
                undo_unavailable: false,
            };
        }
    }

    // Carry the key each record was just validated against into the deletion,
    // so the purge and the delete agree on which object they mean.
    let backings = records
        .iter()
        .map(|record| (record.backing_path.clone(), record.payload_identity.key))
        .collect::<Vec<_>>();
    let deleted = delete_trash_backings(&backings, progress);
    let mut failures = deleted
        .failures
        .into_iter()
        .map(|failure| PathFailure {
            path: map_backing_to_original(records, &failure.path),
            message: failure.message,
        })
        .collect::<Vec<_>>();

    let mut completed = Vec::new();
    let mut purged = Vec::new();
    for backing in deleted.completed {
        let Some(record) = records.iter().find(|record| record.backing_path == backing) else {
            continue;
        };
        completed.push(record.original_path.clone());
        purged.push(record.clone());
        if let Err(error) = record.remove_matching_info() {
            failures.push(PathFailure::new(
                &record.original_path,
                format!(
                    "Permanently deleted “{}”, but could not remove its Trash metadata: {error}",
                    record.original_path.display()
                ),
            ));
        }
    }

    TrashOutcome { records: purged, completed, failures, undo_unavailable: false }
}

/// The home Trash directory, from the same rules the `trash` crate resolves by.
fn home_trash_dir() -> Option<PathBuf> {
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME")
        && !data_home.is_empty()
    {
        return Some(PathBuf::from(data_home).join("Trash"));
    }
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
    Some(PathBuf::from(home).join(".local/share/Trash"))
}

/// Create the home Trash if nothing has yet.
///
/// The `trash` crate creates `files/` and `info/` on demand but not the
/// directory holding them, and every Trash operation here begins by resolving
/// the Trash folders — which fails outright when the home Trash is missing. On
/// an account where nothing has trashed anything before, that turned Marcel's
/// first Trash into a failure reported in the crate's own `Debug` output.
///
/// Best effort by design: if this cannot create the directory, the resolution
/// below fails as it did before and reports why, which is a better error than
/// one raised here about a directory the user never asked for.
fn ensure_home_trash() {
    if let Some(home_trash) = home_trash_dir() {
        ensure_trash_dir(&home_trash);
    }
}

/// The Trash holds whatever the user threw away, so it is exactly as private as
/// their files: the specification asks for `0700`, and leaving the mode to the
/// umask on a shared machine would show every deleted document to the group.
/// The directories above it (`~/.local/share`) are shared with every other
/// program and keep the ordinary mode.
fn ensure_trash_dir(trash_dir: &Path) {
    if trash_dir.is_dir() {
        return;
    }
    if let Some(parent) = trash_dir.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = create_private_dir_all(trash_dir);
}

/// Why these paths cannot go to any Trash, if they cannot: the sentence
/// `trash_paths` would fail them with, found before an operation starts so
/// the window can offer a permanent delete instead. `None` when there is a
/// Trash for every one of them, or when that cannot be told yet, in which
/// case the operation itself reports.
pub fn trash_unavailable_for(paths: &[PathBuf]) -> Option<String> {
    ensure_home_trash();
    let sites = home_trash_dir().and_then(|home_trash| TrashSites::discover(&home_trash).ok())?;
    paths.iter().find_map(|path| sites.no_trash_reason(path))
}

pub fn trash_paths(paths: &[PathBuf]) -> TrashOutcome {
    ensure_home_trash();
    let trash_roots = match trash::os_limited::trash_folders() {
        Ok(roots) => roots,
        Err(error) => {
            return TrashOutcome::all_failed(paths, || {
                format!("Could not resolve the system Trash: {error}")
            });
        }
    };
    let sites = match home_trash_dir()
        .context("Neither XDG_DATA_HOME nor HOME is set")
        .and_then(|home_trash| TrashSites::discover(&home_trash))
    {
        Ok(sites) => sites,
        Err(error) => {
            return TrashOutcome::all_failed(paths, || {
                format!("Could not resolve the system Trash: {error:#}")
            });
        }
    };
    let existing_ids = match trash::os_limited::list() {
        Ok(items) => items.into_iter().map(|item| item.id).collect::<HashSet<_>>(),
        Err(error) => {
            return TrashOutcome::all_failed(paths, || {
                format!("Could not inspect the system Trash: {error}")
            });
        }
    };

    let mut successful = Vec::new();
    let mut failures = Vec::new();
    for path in paths {
        if trash_roots.iter().any(|root| paths_overlap_trash_root(path, root)) {
            failures.push(PathFailure::new(
                path,
                format!(
                    "Refusing to trash “{}” because it is inside or contains a system Trash",
                    path.display()
                ),
            ));
            continue;
        }
        let source = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) => {
                failures.push(PathFailure::new(
                    path,
                    format!("Could not inspect “{}”: {error}", path.display()),
                ));
                continue;
            }
        };
        if let Some(refusal) = sites.crossing_refusal(path, &source) {
            failures.push(PathFailure::new(path, refusal));
            continue;
        }
        if let Some(reason) = sites.no_trash_reason(path) {
            failures.push(PathFailure::new(
                path,
                format!("Could not move “{}” to Trash: {reason}", path.display()),
            ));
            continue;
        }
        let source_object = ObjectKey::of(&source);
        match trash::delete(path) {
            Ok(()) => successful.push((path.clone(), source_object)),
            Err(error) => failures.push(PathFailure::new(
                path,
                format!("Could not move “{}” to Trash: {error}", path.display()),
            )),
        }
    }

    let completed = successful.iter().map(|(path, _)| path.clone()).collect::<Vec<_>>();
    if successful.is_empty() {
        return TrashOutcome { records: Vec::new(), completed, failures, undo_unavailable: false };
    }

    let new_items = match trash::os_limited::list() {
        Ok(items) => {
            items.into_iter().filter(|item| !existing_ids.contains(&item.id)).collect::<Vec<_>>()
        }
        Err(error) => {
            failures.push(PathFailure::new(
                &successful[0].0,
                format!(
                    "Items reached Trash, but Marcel could not retain restore metadata: {error}"
                ),
            ));
            return TrashOutcome {
                records: Vec::new(),
                completed,
                failures,
                undo_unavailable: true,
            };
        }
    };

    let records = bind_records(successful, new_items, &mut failures);
    TrashOutcome {
        undo_unavailable: records.len() != completed.len(),
        records,
        completed,
        failures,
    }
}

/// Bind each path that reached the Trash to the entry now holding it.
///
/// The entry has to hold the very object that left the original path. A
/// payload with another key is a copy the `trash` crate made when its rename
/// failed with `EXDEV` (see [`TrashSites`]); the restore rename could not bring
/// a copy back across that boundary either, so the entry is reported and kept
/// out of the record rather than promised to Undo.
fn bind_records(
    successful: Vec<(PathBuf, ObjectKey)>,
    mut new_items: Vec<trash::TrashItem>,
    failures: &mut Vec<PathFailure>,
) -> Vec<TrashRecord> {
    let mut records = Vec::with_capacity(successful.len());
    for (original, source_object) in successful {
        let Some(index) = match_trashed_item(&new_items, &original, source_object) else {
            failures.push(PathFailure::new(
                &original,
                "Item reached Trash, but its exact restore entry could not be identified",
            ));
            continue;
        };
        match TrashRecord::from_item(new_items.swap_remove(index)) {
            Ok(record) if record.payload_identity.key != source_object => {
                failures.push(PathFailure::new(
                    &original,
                    format!(
                        "“{}” reached Trash as a copy on another filesystem, so Undo cannot return it",
                        original.display()
                    ),
                ));
            }
            Ok(record) => records.push(record),
            Err(error) => failures.push(PathFailure::new(
                &original,
                format!(
                    "Item reached Trash, but Marcel could not retain restore metadata: {error}"
                ),
            )),
        }
    }
    records
}

/// Where the `trash` crate will put an item, worked out by the crate's own
/// rules (`freedesktop.rs`, `delete_all_canonicalized`): the home Trash when
/// the item's mount is the home Trash's mount, otherwise `.Trash/<uid>` or
/// `.Trash-<uid>` at the top of the item's mount.
///
/// Marcel needs the answer before the crate is called. The crate meets a rename
/// that fails with `EXDEV` by copying and deleting instead, which drops
/// extended attributes, ACLs, and times, and the copy it leaves in the Trash is
/// a new object that the restore rename cannot move back either. A mount table
/// cannot see every such boundary — a Btrfs subvolume inside a mount is one —
/// so the deciding comparison is between device numbers, not paths.
struct TrashSites {
    home_trash: PathBuf,
    /// Mount points, longest first, so the first prefix match is the deepest.
    mounts: Vec<PathBuf>,
    uid: u32,
}

impl TrashSites {
    fn discover(home_trash: &Path) -> Result<Self> {
        Ok(Self::new(
            canonicalize_or_parents(home_trash),
            read_mount_points()?,
            rustix::process::getuid().as_raw(),
        ))
    }

    fn new(home_trash: PathBuf, mut mounts: Vec<PathBuf>, uid: u32) -> Self {
        mounts.sort_by_key(|mount| std::cmp::Reverse(mount.as_os_str().len()));
        Self { home_trash, mounts, uid }
    }

    fn topdir_of<'a>(&'a self, path: &Path) -> &'a Path {
        self.mounts
            .iter()
            .find(|mount| path.starts_with(mount))
            .map_or(Path::new("/"), PathBuf::as_path)
    }

    /// The Trash directory the crate will rename `path` into.
    fn trash_for(&self, path: &Path) -> PathBuf {
        // The crate resolves the parent and keeps the final name as given.
        let path = resolve_parent_of(path);
        let topdir = self.topdir_of(&path);
        if topdir == self.topdir_of(&self.home_trash) {
            return self.home_trash.clone();
        }
        let shared = topdir.join(".Trash");
        if is_valid_shared_trash(&shared) {
            let mine = shared.join(self.uid.to_string());
            if mine.is_dir() {
                return mine;
            }
        }
        topdir.join(format!(".Trash-{}", self.uid))
    }

    /// Why `path` has no Trash to go to, if it has none: the directory the
    /// crate would rename it into does not exist and cannot be made.
    ///
    /// Every GVfs share is one FUSE filesystem rooted at `/run/user/<uid>/gvfs`,
    /// and that root is a listing of mounts that refuses a `mkdir`, so a file
    /// on a share can never be trashed; a read-only stick is the other case.
    /// Making the directory here is what the crate would do a moment later,
    /// so a successful attempt changes nothing about what follows.
    fn no_trash_reason(&self, path: &Path) -> Option<String> {
        let trash = self.trash_for(path);
        if trash.is_dir() {
            return None;
        }
        // The OS error adds nothing a person can act on: ENOENT at a FUSE
        // root and EROFS on a stick both mean the same thing here.
        create_private_dir_all(&trash)
            .err()
            .map(|_| "no Trash exists on this filesystem and one cannot be created".to_string())
    }

    /// Why `path` must not be handed to the crate, if its rename would cross
    /// a filesystem boundary. Worded the way a move refuses the same thing.
    fn crossing_refusal(&self, path: &Path, source: &fs::Metadata) -> Option<String> {
        let trash = self.trash_for(path);
        // A Trash whose device cannot be read is left for the crate to report.
        let trash_device = device_of_nearest_existing(&trash)?;
        (source.dev() != trash_device).then(|| {
            format!(
                "Could not move “{}” to Trash: it is on a different filesystem from “{}”, and the Trash keeps the file itself, not a copy",
                path.display(),
                trash.display()
            )
        })
    }
}

/// The spec's shared `$topdir/.Trash`: a real directory with the sticky bit,
/// as the crate's `folder_validity` checks it.
fn is_valid_shared_trash(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.file_type().is_dir() && metadata.mode() & 0o1000 != 0)
}

/// The device of `path`, or of the nearest ancestor that exists when the crate
/// has yet to create it.
fn device_of_nearest_existing(path: &Path) -> Option<u64> {
    let mut candidate = path;
    loop {
        match fs::metadata(candidate) {
            Ok(metadata) => return Some(metadata.dev()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                candidate = candidate.parent()?;
            }
            Err(_) => return None,
        }
    }
}

/// `canonicalize`, extended to a path that does not exist yet by resolving
/// the deepest ancestor that does — the crate's `canonicalize_path_or_parents`.
fn canonicalize_or_parents(path: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut candidate = path;
    loop {
        match candidate.canonicalize() {
            Ok(resolved) => {
                return missing.iter().rev().fold(resolved, |path, name| path.join(name));
            }
            Err(_) => match (candidate.parent(), candidate.file_name()) {
                (Some(parent), Some(name)) => {
                    missing.push(name);
                    candidate = parent;
                }
                _ => return path.to_path_buf(),
            },
        }
    }
}

/// Every mount point the kernel reports, from the same two tables the crate
/// reads with `getmntent`, which is where the escaping of spaces in a mount
/// path is undone.
fn read_mount_points() -> Result<Vec<PathBuf>> {
    let table = fs::read("/proc/self/mounts")
        .or_else(|_| fs::read("/etc/mtab"))
        .context("Could not read the mount table")?;
    Ok(parse_mount_points(&table))
}

fn parse_mount_points(table: &[u8]) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStringExt as _;

    table
        .split(|byte| *byte == b'\n')
        .filter_map(|line| line.split(|byte| *byte == b' ').nth(1))
        .map(|field| PathBuf::from(std::ffi::OsString::from_vec(unescape_mount_field(field))))
        .collect()
}

/// Undo the `\ooo` octal escapes `/proc/mounts` uses for the bytes that would
/// break its space-separated format.
fn unescape_mount_field(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut bytes = field.iter().copied();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            out.push(byte);
            continue;
        }
        let digits = [bytes.next(), bytes.next(), bytes.next()];
        let value = digits.iter().try_fold(0u8, |acc, digit| {
            let digit = digit.filter(|digit| (b'0'..=b'7').contains(digit))? - b'0';
            acc.checked_mul(8)?.checked_add(digit)
        });
        match value {
            Some(value) => out.push(value),
            None => {
                out.push(b'\\');
                out.extend(digits.iter().flatten());
            }
        }
    }
    out
}

/// Which new Trash entry holds `original`, if exactly one can be said to.
///
/// Hardlinks share a (device, inode) key, so a concurrent trash of a sibling
/// link can produce two identity matches. The journal is bound only to an
/// unambiguous entry; anything else disables Undo rather than guessing and
/// later restoring to the wrong original path.
fn match_trashed_item(
    new_items: &[trash::TrashItem],
    original: &Path,
    source_object: ObjectKey,
) -> Option<usize> {
    let candidates = new_items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let backing = backing_path_from_info(Path::new(&item.id)).ok()?;
            let metadata = fs::symlink_metadata(&backing).ok()?;
            Some((
                index,
                ObjectKey::of(&metadata) == source_object,
                item.original_path() == original,
            ))
        })
        .collect::<Vec<_>>();
    let unique = |keep: &dyn Fn(bool, bool) -> bool| {
        let mut matching = candidates
            .iter()
            .filter(|(_, identity_matches, path_matches)| keep(*identity_matches, *path_matches));
        match (matching.next(), matching.next()) {
            (Some((index, _, _)), None) => Some(*index),
            _ => None,
        }
    };
    unique(&|identity, path| identity && path)
        .or_else(|| unique(&|identity, _| identity))
        .or_else(|| unique(&|_, path| path))
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
        Self { error: error.into(), committed: false }
    }

    fn committed(error: impl Into<anyhow::Error>) -> Self {
        Self { error: error.into(), committed: true }
    }
}

pub fn restore_trash_records(
    records: &[TrashRecord],
) -> Result<TrashRestore, TrashMutationFailure> {
    // Prepare: every check runs before the first rename, so a refusal here
    // provably left the Trash untouched.
    let mut targets = Vec::with_capacity(records.len());
    for record in records {
        match record.validate().and_then(|()| RestoreTarget::prepare(record)) {
            Ok(target) => targets.push(target),
            Err(error) => return Err(TrashMutationFailure::unchanged(error)),
        }
    }

    let mut restored: Vec<&RestoreTarget> = Vec::with_capacity(records.len());
    for target in &targets {
        let original = target.record.original_path();
        // Commit, into the directory the preparation resolved and not into
        // whatever the path resolves to now.
        let committed = target.parent_key.validate(&target.parent).and_then(|()| {
            rename_no_replace(&target.record.backing_path, &target.path).map_err(Into::into)
        });
        if let Err(error) = committed {
            let message = format!("Could not restore “{}” from Trash: {error}", original.display());
            if restored.is_empty() {
                // The first rename failed, so the Trash is untouched and the
                // record still describes it exactly.
                return Err(TrashMutationFailure::unchanged(anyhow::anyhow!("{message}")));
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
        restored.push(target);
    }

    // Finalize. Every payload is already restored, so nothing below may fail
    // the operation; an uninspectable result only costs undo. A failed
    // metadata cleanup leaves only an orphaned Trash entry, never missing
    // user data.
    let mut result = Vec::with_capacity(records.len());
    let mut undoable = true;
    for target in &targets {
        let _ = target.record.remove_matching_info();
        match fs::symlink_metadata(&target.path) {
            Ok(metadata) => result.push(TrashRecord {
                payload_identity: FileIdentity::of(&metadata),
                ..target.record.clone()
            }),
            Err(_) => undoable = false,
        }
    }
    Ok(TrashRestore { records: result, undoable })
}

/// Where one record's payload goes back to, resolved once.
///
/// The original parent is resolved through any symbolic links, so a user whose
/// `~/Documents` is a link to another disk can restore into it; the identity of
/// the directory it resolves to is what the commit then checks. The parent is
/// never created: a Trash entry whose home has gone is for the user to place,
/// not for Marcel to guess a directory for.
struct RestoreTarget<'r> {
    record: &'r TrashRecord,
    parent: PathBuf,
    parent_key: ObjectKey,
    path: PathBuf,
}

impl<'r> RestoreTarget<'r> {
    fn prepare(record: &'r TrashRecord) -> Result<Self> {
        let original = record.original_path();
        let parent = record.original_parent.canonicalize().with_context(|| {
            format!("Cannot restore “{}”: its original parent no longer exists", original.display())
        })?;
        let parent_metadata = inspect(&parent)?;
        if !parent_metadata.file_type().is_dir() {
            bail!(
                "Cannot restore “{}”: its original parent is no longer a directory",
                original.display()
            );
        }
        let name = original
            .file_name()
            .with_context(|| format!("Cannot restore “{}”: it has no name", original.display()))?;
        let path = parent.join(name);
        match path_occupancy(&path) {
            Ok(PathOccupancy::Occupied) => {
                bail!("Cannot restore: “{}” is already occupied", original.display())
            }
            Ok(PathOccupancy::Vacant) => {}
            Err(error) => return Err(error).at("Could not inspect restore target", original),
        }
        Ok(Self { record, parent, parent_key: ObjectKey::of(&parent_metadata), path })
    }
}

pub fn retrash_records(records: &[TrashRecord]) -> Result<Vec<TrashRecord>, TrashMutationFailure> {
    // Prepare.
    for record in records {
        if let Err(error) = record.payload_identity.validate(record.original_path(), "continue") {
            return Err(TrashMutationFailure::unchanged(error));
        }
    }
    let originals =
        records.iter().map(|record| record.original_path().to_path_buf()).collect::<Vec<_>>();
    // Commit: `trash_paths` places items one at a time.
    let outcome = trash_paths(&originals);
    if outcome.failures.is_empty()
        && !outcome.undo_unavailable
        && outcome.records.len() == originals.len()
    {
        return Ok(outcome.records);
    }

    let failure = outcome.summarize_failures();
    if outcome.completed.is_empty() {
        // Nothing reached the Trash, so the record is still accurate.
        return Err(TrashMutationFailure::unchanged(anyhow::anyhow!("{failure}")));
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

fn backing_path_from_info(info_path: &Path) -> Result<PathBuf> {
    if !info_path.is_absolute() || info_path.extension() != Some(OsStr::new("trashinfo")) {
        bail!("Invalid freedesktop Trash metadata path");
    }
    let info_dir = info_path
        .parent()
        .filter(|parent| parent.file_name() == Some(OsStr::new("info")))
        .context("Invalid freedesktop Trash metadata directory")?;
    let trash_root = info_dir.parent().context("Invalid freedesktop Trash root")?;
    let name = info_path
        .file_stem()
        .filter(|name| !name.is_empty())
        .context("Invalid freedesktop Trash entry name")?;
    Ok(trash_root.join("files").join(name))
}

fn rollback_restored(restored: &[&RestoreTarget<'_>]) -> Result<()> {
    for target in restored.iter().rev() {
        rename_no_replace(&target.path, &target.record.backing_path).with_context(|| {
            format!("Could not return “{}” to Trash", target.record.original_path().display())
        })?;
    }
    Ok(())
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
    let root = trash_root.canonicalize().unwrap_or_else(|_| trash_root.to_path_buf());
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
    use crate::testing::{Sandbox, read, seal, skip_as_root};

    /// A Trash holding one entry for `name`, once at `original_parent`.
    fn seeded_record(
        sandbox: &Sandbox,
        trash: &str,
        original_parent: &Path,
        name: &str,
    ) -> TrashRecord {
        let info_path = sandbox.file(
            &format!("{trash}/info/{name}.trashinfo"),
            format!(
                "[Trash Info]\nPath={}\nDeletionDate=2026-07-29T12:00:00\n",
                original_parent.join(name).display()
            ),
        );
        let backing_path = sandbox.file(&format!("{trash}/files/{name}"), b"payload");
        TrashRecord {
            original_path: original_parent.join(name),
            info_identity: FileIdentity::read(&info_path).unwrap(),
            payload_identity: FileIdentity::read(&backing_path).unwrap(),
            info_path,
            backing_path,
            original_parent: original_parent.to_path_buf(),
        }
    }

    /// One note in the Trash, originally from `original/`.
    fn one_note() -> (Sandbox, PathBuf, TrashRecord) {
        let sandbox = Sandbox::new();
        let original_parent = sandbox.dir("original");
        let record = seeded_record(&sandbox, "Trash", &original_parent, "note.txt");
        (sandbox, original_parent, record)
    }

    #[test]
    fn missing_home_trash_is_created_rather_than_failing_the_operation() {
        // The `trash` crate creates `files/` and `info/` on demand but not the
        // directory holding them, and resolving the Trash folders fails when it
        // is absent — so on an account that has never trashed anything, every
        // Trash operation failed until this directory existed.
        let sandbox = Sandbox::new();
        let trash_dir = sandbox.path("Trash");
        assert!(!trash_dir.exists());

        ensure_trash_dir(&trash_dir);
        assert!(trash_dir.is_dir());

        // Idempotent, and it must not disturb what an existing Trash holds.
        let kept = sandbox.file("Trash/files/kept.txt", b"");
        ensure_trash_dir(&trash_dir);
        assert!(kept.is_file());
    }

    /// The Trash holds what the user threw away, so it gets the mode the
    /// specification asks for rather than whatever the umask allows.
    #[test]
    fn a_new_home_trash_is_private_and_an_existing_one_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let sandbox = Sandbox::new();
        let trash_dir = sandbox.path("share/Trash");

        ensure_trash_dir(&trash_dir);

        assert_eq!(fs::metadata(&trash_dir).unwrap().permissions().mode() & 0o777, 0o700);
        // The directories above it are shared with every other program.
        assert_ne!(
            fs::metadata(sandbox.path("share")).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let existing = sandbox.dir("other/Trash");
        fs::set_permissions(&existing, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_trash_dir(&existing);
        assert_eq!(fs::metadata(&existing).unwrap().permissions().mode() & 0o777, 0o755);
    }

    #[test]
    fn an_unwritable_parent_leaves_the_trash_directory_to_the_caller() {
        // Best effort by design: the resolution that follows reports why it
        // could not proceed, which beats an error raised here about a directory
        // the user never asked for.
        let sandbox = Sandbox::new();
        let blocked = sandbox.file("file-not-a-dir", b"");

        ensure_trash_dir(&blocked.join("Trash"));
        assert!(blocked.is_file());
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
        assert!(!paths_overlap_trash_root(Path::new("/home/test/Documents"), root));
    }

    /// A symbolic link gives one directory two spellings, and the guard has to
    /// recognize the Trash under either of them.
    #[test]
    fn a_trash_root_reached_through_a_symlink_is_still_refused() {
        let sandbox = Sandbox::new();
        let root = sandbox.path("Trash");
        sandbox.file("Trash/files/note.txt", b"trashed");
        std::os::unix::fs::symlink(&root, sandbox.path("shortcut")).unwrap();

        let through_link = sandbox.path("shortcut/files/note.txt");
        assert!(
            paths_overlap_trash_root(&through_link, &root),
            "the same object under another spelling is still in the Trash"
        );

        // The object itself is never resolved: deleting a link that points into
        // the Trash removes the link, which is safe and must stay allowed.
        let elsewhere = sandbox.path("pointer");
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

    /// A Trash laid out the way the `trash` crate expects one, so its choice
    /// of destination can be predicted without calling it. Mount points are
    /// given resolved, as the kernel reports them.
    fn sites(sandbox: &Sandbox, mounts: &[&str]) -> TrashSites {
        TrashSites::new(
            sandbox.dir("home/.local/share/Trash"),
            mounts.iter().map(|mount| sandbox.dir(mount).canonicalize().unwrap()).collect(),
            4242,
        )
    }

    #[test]
    fn the_trash_the_crate_would_choose_is_predicted_by_its_rules() {
        use std::os::unix::fs::PermissionsExt as _;

        let sandbox = Sandbox::new();
        let sites = sites(&sandbox, &["disk", "disk/nested"]);
        let disk = sandbox.path("disk").canonicalize().unwrap();

        // The same mount as the home Trash: the home Trash.
        assert_eq!(sites.trash_for(&sandbox.path("home/report.pdf")), sites.home_trash);
        // Another mount without a shared Trash: `.Trash-<uid>` at its top,
        // and the deepest mount wins.
        assert_eq!(sites.trash_for(&sandbox.path("disk/a.txt")), disk.join(".Trash-4242"));
        assert_eq!(
            sites.trash_for(&sandbox.path("disk/nested/b.txt")),
            disk.join("nested/.Trash-4242")
        );
        // A path spelled through a link is placed by where it resolves.
        std::os::unix::fs::symlink(&disk, sandbox.path("shortcut")).unwrap();
        assert_eq!(sites.trash_for(&sandbox.path("shortcut/a.txt")), disk.join(".Trash-4242"));

        // A shared `.Trash` counts only with the sticky bit and a directory
        // for this user inside it.
        let shared = sandbox.dir("disk/.Trash");
        sandbox.dir("disk/.Trash/4242");
        assert_eq!(sites.trash_for(&sandbox.path("disk/a.txt")), disk.join(".Trash-4242"));
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o1777)).unwrap();
        assert_eq!(sites.trash_for(&sandbox.path("disk/a.txt")), disk.join(".Trash/4242"));
    }

    /// A mount whose top refuses a new directory has no Trash and cannot
    /// get one; a GVfs share is the everyday case, and a read-only root
    /// stands in for it here.
    #[test]
    fn a_mount_that_cannot_hold_a_trash_is_named_before_the_crate_is_called() {
        use std::os::unix::fs::PermissionsExt as _;

        let sandbox = Sandbox::new();
        let sites = sites(&sandbox, &["share"]);
        let share = sandbox.path("share").canonicalize().unwrap();
        fs::set_permissions(&share, fs::Permissions::from_mode(0o555)).unwrap();

        let reason = sites.no_trash_reason(&share.join("photo.jpg")).expect("must refuse");
        assert!(reason.contains("cannot be created"), "{reason}");
        assert!(!share.join(".Trash-4242").exists());

        fs::set_permissions(&share, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(sites.no_trash_reason(&share.join("photo.jpg")), None);
        assert!(share.join(".Trash-4242").is_dir(), "a Trash the mount allows is made on the spot");
        assert_eq!(sites.no_trash_reason(&sandbox.path("home/report.pdf")), None);
    }

    /// The deciding comparison is between devices, because a mount table
    /// cannot see every boundary a rename fails to cross. `/proc` is the one
    /// filesystem guaranteed to be another device than any writable one.
    #[test]
    fn a_source_on_another_device_than_its_trash_is_refused_before_the_crate_copies_it() {
        let sandbox = Sandbox::new();
        let source = sandbox.file("home/report.pdf", b"payload");
        let metadata = fs::symlink_metadata(&source).unwrap();

        let same_device = sites(&sandbox, &[]);
        assert_eq!(same_device.crossing_refusal(&source, &metadata), None);

        let Ok(proc_metadata) = fs::metadata("/proc") else {
            return;
        };
        assert_ne!(proc_metadata.dev(), metadata.dev(), "the fixture needs two devices");
        let other_device =
            TrashSites::new(PathBuf::from("/proc/marcel-test/Trash"), Vec::new(), 4242);
        let refusal = other_device.crossing_refusal(&source, &metadata).expect("must refuse");
        assert!(refusal.contains("different filesystem"), "{refusal}");
        assert_eq!(read(&source), b"payload", "a refusal touches nothing");
    }

    /// One new Trash entry claiming the original path, as the crate lists it.
    fn listed_item(sandbox: &Sandbox, original: &Path) -> trash::TrashItem {
        let name = original.file_name().unwrap();
        let info_path = sandbox.file(
            &format!("Trash/info/{}.trashinfo", name.to_string_lossy()),
            format!("[Trash Info]\nPath={}\n", original.display()),
        );
        trash::TrashItem {
            id: info_path.into_os_string(),
            name: name.to_os_string(),
            original_parent: original.parent().unwrap().to_path_buf(),
            time_deleted: 0,
        }
    }

    /// Should the crate copy anyway, the entry holds a new object. Binding it
    /// would promise an Undo whose rename fails across the same boundary, so
    /// it is reported instead and the outcome says Undo is unavailable.
    #[test]
    fn an_entry_holding_a_copy_rather_than_the_object_is_not_bound_for_undo() {
        let sandbox = Sandbox::new();
        let original = sandbox.file("original/note.txt", b"payload");
        let source_object = ObjectKey::of(&fs::symlink_metadata(&original).unwrap());
        let items = vec![listed_item(&sandbox, &original)];
        sandbox.dir("Trash/files");

        // A copy, as the crate leaves one after `EXDEV`.
        fs::copy(&original, sandbox.path("Trash/files/note.txt")).unwrap();
        fs::remove_file(&original).unwrap();
        let mut failures = Vec::new();
        let records =
            bind_records(vec![(original.clone(), source_object)], items.clone(), &mut failures);
        assert!(records.is_empty(), "{records:?}");
        assert_eq!(failures.len(), 1);
        assert!(failures[0].message.contains("copy"), "{}", failures[0].message);

        // The object itself, as a rename leaves it: bound.
        fs::remove_file(sandbox.path("Trash/files/note.txt")).unwrap();
        let original = sandbox.file("original/note.txt", b"payload");
        let source_object = ObjectKey::of(&fs::symlink_metadata(&original).unwrap());
        fs::rename(&original, sandbox.path("Trash/files/note.txt")).unwrap();
        let mut failures = Vec::new();
        let records = bind_records(vec![(original.clone(), source_object)], items, &mut failures);
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].original_path(), original);
    }

    #[test]
    fn mount_points_are_read_the_way_getmntent_reads_them() {
        let table = b"tmpfs /tmp tmpfs rw 0 0\n/dev/sda1 /mnt/my\\040disk ext4 rw 0 0\nnone /odd\\x path 0 0\n\n";
        assert_eq!(
            parse_mount_points(table),
            [PathBuf::from("/tmp"), PathBuf::from("/mnt/my disk"), PathBuf::from("/odd\\x")]
        );
    }

    #[test]
    fn restore_is_no_replace_and_removes_matching_metadata() {
        let (_sandbox, original_parent, record) = one_note();

        let restored = restore_trash_records(std::slice::from_ref(&record)).unwrap();

        assert_eq!(read(original_parent.join("note.txt")), b"payload");
        assert!(!record.backing_path.exists());
        assert!(!record.info_path.exists());
        assert!(restored.undoable);
        assert_eq!(restored.records[0].original_path(), original_parent.join("note.txt"));
    }

    /// A refusal raised before the first rename leaves the Trash exactly as the
    /// record describes, so the caller may keep the history entry and retry.
    #[test]
    fn a_restore_refused_before_committing_stays_retryable() {
        let (sandbox, _, record) = one_note();
        sandbox.file("original/note.txt", b"");

        let failure = restore_trash_records(std::slice::from_ref(&record))
            .expect_err("an occupied destination must refuse");

        assert!(!failure.committed, "a preflight refusal must stay retryable: {}", failure.error);
        assert!(record.backing_path.exists());
        assert!(record.info_path.exists());
    }

    /// Once a payload has been renamed out of Trash, compensation renames it
    /// back and moves its ctime, so the record can no longer validate and the
    /// caller must discard it.
    #[test]
    fn a_restore_that_rolls_back_reports_a_committed_failure() {
        if skip_as_root() {
            return;
        }
        let sandbox = Sandbox::new();
        // Two original parents, so one can be sealed without blocking the
        // other. The preflight stats each destination and validates each
        // record before the first rename, so an obstacle it can see yields an
        // unchanged failure; reaching the rolled-back path needs one only the
        // rename itself discovers.
        let open = sandbox.dir("open");
        let sealed = sandbox.dir("sealed");
        let first = seeded_record(&sandbox, "Trash", &open, "first.txt");
        let second = seeded_record(&sandbox, "Trash", &sealed, "second.txt");

        // "first" restores into a writable parent and commits; "second" cannot
        // be created inside a read-only parent, though stat still succeeds.
        seal(&sealed, true);
        let failure = restore_trash_records(&[first.clone(), second]);
        seal(&sealed, false);

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
    fn restore_refuses_to_recreate_a_missing_original_parent() {
        let sandbox = Sandbox::new();
        let record = seeded_record(&sandbox, "Trash", &sandbox.path("missing"), "note.txt");

        assert!(restore_trash_records(std::slice::from_ref(&record)).is_err());
        assert!(record.backing_path.exists());
    }

    /// `~/Documents` as a link to another disk is an ordinary setup, and an
    /// entry another program trashed from there names the link as its parent.
    /// Refusing that as "no longer a directory" made such entries unrestorable.
    #[test]
    fn restore_follows_a_linked_original_parent_into_the_directory_it_names() {
        let sandbox = Sandbox::new();
        let real = sandbox.dir("disk/Documents");
        let link = sandbox.path("home/Documents");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let record = seeded_record(&sandbox, "Trash", &link, "note.txt");

        let restored = restore_trash_records(std::slice::from_ref(&record)).unwrap();

        assert_eq!(read(real.join("note.txt")), b"payload");
        assert!(!record.backing_path.exists());
        assert!(!record.info_path.exists());
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link stays");
        assert!(restored.undoable);
        assert_eq!(restored.records[0].original_path(), link.join("note.txt"));
    }

    /// A link whose target is gone is a missing parent, and a link to a file
    /// is not a directory; neither is created or replaced.
    #[test]
    fn restore_refuses_a_linked_parent_that_does_not_resolve_to_a_directory() {
        let sandbox = Sandbox::new();
        let dangling = sandbox.path("dangling");
        std::os::unix::fs::symlink(sandbox.path("gone"), &dangling).unwrap();
        let record = seeded_record(&sandbox, "TrashA", &dangling, "note.txt");
        let error = restore_trash_records(std::slice::from_ref(&record)).unwrap_err();
        assert!(error.error.to_string().contains("no longer exists"), "{}", error.error);
        assert!(!error.committed);
        assert!(record.backing_path.exists());
        assert!(!sandbox.path("gone").exists());

        let file = sandbox.file("file", b"");
        let to_file = sandbox.path("to-file");
        std::os::unix::fs::symlink(&file, &to_file).unwrap();
        let record = seeded_record(&sandbox, "TrashB", &to_file, "note.txt");
        let error = restore_trash_records(std::slice::from_ref(&record)).unwrap_err();
        assert!(error.error.to_string().contains("no longer a directory"), "{}", error.error);
        assert!(record.backing_path.exists());
    }

    #[test]
    fn restore_refuses_a_replaced_trash_payload() {
        let (_sandbox, original_parent, record) = one_note();
        fs::remove_file(&record.backing_path).unwrap();
        fs::write(&record.backing_path, b"replacement").unwrap();

        assert!(restore_trash_records(std::slice::from_ref(&record)).is_err());
        assert!(!original_parent.join("note.txt").exists());
    }

    #[test]
    fn permanent_purge_removes_payload_and_matching_metadata() {
        let (_sandbox, original_parent, record) = one_note();

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
            outcome.records.iter().map(|record| record.backing_path()).collect::<Vec<_>>(),
            [record.backing_path()]
        );
    }

    /// Two Trash entries can hold the same original path — the same file
    /// trashed twice. Reconciling a purge by original path removed both from
    /// the listing when only one was purged.
    #[test]
    fn purging_one_of_two_entries_sharing_an_original_names_only_the_purged_one() {
        let sandbox = Sandbox::new();
        let original_parent = sandbox.dir("original");
        let first = seeded_record(&sandbox, "TrashA", &original_parent, "note.txt");
        let second = seeded_record(&sandbox, "TrashB", &original_parent, "note.txt");
        assert_eq!(first.original_path(), second.original_path());

        let outcome = purge_trash_records(
            std::slice::from_ref(&first),
            Arc::new(TransferProgress::default()),
        );

        assert!(outcome.failures.is_empty(), "{outcome:?}");
        assert_eq!(
            outcome.records.iter().map(|record| record.backing_path()).collect::<Vec<_>>(),
            [first.backing_path()]
        );
        assert!(second.backing_path.exists());
    }

    #[test]
    fn metadata_cleanup_refuses_a_replaced_trash_info_file() {
        let (_sandbox, _, record) = one_note();
        fs::remove_file(&record.info_path).unwrap();
        fs::write(&record.info_path, b"replacement").unwrap();

        assert!(record.remove_matching_info().is_err());
        assert_eq!(read(&record.info_path), b"replacement");
    }
}
