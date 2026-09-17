//! Holding a replaced object aside so an undo can put it back, and what
//! happens to it when nothing can any more.
//!
//! Nautilus keeps nothing here: it overwrites in place, and undo of a copy
//! deletes the destinations, so whatever was replaced is gone for good. Marcel
//! treats a replacement as reversible or reports that it is not.

use std::{
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use super::local::PathContext as _;
use anyhow::{Context as _, Result, bail};

use super::{
    identity::FileIdentity,
    local::{inspect, quarantined_name, rename_no_replace},
};

static REPLACEMENT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// How many bytes of replaced data one operation may hold aside for undo.
///
/// Quarantining what a replace displaced is what makes replacement reversible,
/// but it is real disk held for as long as the record lives. This follows the
/// rule `UNDO_SNAPSHOT_LIMIT` already sets: past the budget the operation
/// still happens, it simply stops being undoable and says so. The alternative —
/// refusing to replace large files — would be a worse answer to a question the
/// user already asked.
pub const REPLACEMENT_UNDO_BYTE_LIMIT: u64 = 1024 * 1024 * 1024;

/// The name prefix Marcel gives an object it has displaced.
///
/// The process id is part of the name so a later Marcel can tell its own live
/// quarantines from those a dead process abandoned, which is the same rule
/// permanent deletion already uses for its own remnants.
const REPLACEMENT_PREFIX: &[u8] = b".marcel-replaced-";

/// The name prefix Marcel gives data it could not put back.
///
/// Deliberately not a replacement quarantine, because the two mean opposite
/// things. A replacement quarantine holds an original whose replacement
/// *succeeded*: the user asked for that overwrite, only Undo could still want
/// it, and once no record can reach it it is provably unreachable garbage. A
/// recovery remnant holds an original Marcel *failed* to put back, which makes
/// it the user's only copy of that data.
///
/// So it carries no process id — nothing can ever decide it was abandoned by a
/// dead owner — and it is not hidden, because guidance that points at a path
/// the browser refuses to show cannot be followed.
pub const RECOVERY_REMNANT_PREFIX: &str = ".marcel-recovered-";

pub fn is_replacement_quarantine_name(name: &OsStr) -> bool {
    os_bytes(name).starts_with(REPLACEMENT_PREFIX)
}

pub fn is_recovery_remnant_name(name: &OsStr) -> bool {
    os_bytes(name).starts_with(RECOVERY_REMNANT_PREFIX.as_bytes())
}

/// Whether a name belongs to Marcel's own working state rather than the user's
/// data.
///
/// Hidden entries are shown by default, so without this a user who replaces a
/// file watches a cryptic sibling appear beside it and vanish later. Copy and
/// archive staging have the same problem while an operation runs.
///
/// Permanent-delete quarantines and recovery remnants are deliberately
/// excluded: their recovery guidance points the user straight at the path, so
/// hiding them would make that advice impossible to follow.
pub fn is_internal_working_name(name: &OsStr) -> bool {
    let bytes = os_bytes(name);
    bytes.starts_with(REPLACEMENT_PREFIX)
        || bytes.starts_with(b".marcel-copy-")
        || bytes.starts_with(b".marcel-archive-")
}

fn os_bytes(name: &OsStr) -> &[u8] {
    use std::os::unix::ffi::OsStrExt as _;
    name.as_bytes()
}

/// The process that created a replacement quarantine, if the name carries one.
fn quarantine_owner(name: &OsStr) -> Option<u32> {
    let rest = os_bytes(name).strip_prefix(REPLACEMENT_PREFIX)?;
    let end = rest.iter().position(|byte| *byte == b'-')?;
    std::str::from_utf8(&rest[..end]).ok()?.parse().ok()
}

/// Whether a process is still running, and so might still be able to undo.
///
/// A live owner's quarantine is its own business: two Marcel processes can
/// exist when desktop integration is unavailable, and reclaiming another's
/// would destroy data it can still restore. An unknown answer keeps the file.
pub fn process_is_running(process: u32) -> bool {
    Path::new(&format!("/proc/{process}")).exists()
}

/// Release replacement quarantines abandoned by processes that are gone.
///
/// Unlike an interrupted permanent deletion, this needs no user involvement.
/// A replaced file is one the user chose to overwrite, and once the process
/// holding its record is gone nothing can ever restore it, so it is provably
/// unreachable rather than possibly-wanted. Returns how many were released.
pub fn reclaim_abandoned_quarantines(directory: &Path) -> usize {
    let current = std::process::id();
    let Ok(entries) = fs::read_dir(directory) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            quarantine_owner(&entry.file_name())
                .is_some_and(|owner| owner != current && !process_is_running(owner))
        })
        // No record survives to say what this was, so the identity read while
        // scanning stands in for one.
        .filter(|entry| {
            entry.metadata().is_ok_and(|metadata| {
                erase_quarantined_object(&entry.path(), &FileIdentity::of(&metadata))
            })
        })
        .count()
}

/// An object a transfer displaced, held aside so undo can put it back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplacedItem {
    /// Where it lived, and where undo must put it back.
    pub(super) path: PathBuf,
    /// Where it is being held meanwhile.
    pub(super) quarantine: PathBuf,
    pub(super) identity: FileIdentity,
}

impl ReplacedItem {
    /// The hidden path holding this item, so an evicted record can release it.
    pub fn quarantine(&self) -> &Path {
        &self.quarantine
    }

    /// Total bytes this quarantine is holding.
    pub(super) fn bytes(&self) -> u64 {
        let mut total: u64 = 0;
        let mut pending = vec![self.quarantine.clone()];
        while let Some(path) = pending.pop() {
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_file() {
                total = total.saturating_add(metadata.len());
            }
            if metadata.file_type().is_dir()
                && let Ok(entries) = fs::read_dir(&path)
            {
                pending.extend(entries.flatten().map(|entry| entry.path()));
            }
        }
        total
    }
}

/// Move the object at `path` aside, returning where it went.
///
/// The rename is atomic, so the destination is never briefly absent in a way
/// that another writer could occupy, and the displaced object is never
/// destroyed before its replacement is safely published.
pub(super) fn quarantine_for_replacement(path: &Path) -> Result<ReplacedItem> {
    let parent = path.parent().context("Replacement target has no parent directory")?;
    let name = path.file_name().context("Replacement target has no file name")?;
    let expected = FileIdentity::read(path)?;
    let prefix = format!(".marcel-replaced-{}-", std::process::id());

    for _ in 0..1024 {
        let sequence = REPLACEMENT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(quarantined_name(&prefix, sequence, name));
        match fs::symlink_metadata(&candidate) {
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).at("Could not inspect replacement quarantine", &candidate);
            }
        }
        rename_no_replace(path, &candidate)
            .with_context(|| format!("Could not move “{}” aside to replace it", path.display()))?;
        // Finalize: the rename bumped the moved root's ctime, so the identity
        // has to be re-read rather than carried over — the same discipline the
        // move path already follows. Device and inode survive a rename, so
        // they prove this is still the object that was moved and not something
        // that took its place.
        let identity = FileIdentity::of(&inspect(&candidate).with_context(|| {
            format!("Could not inspect “{}” after moving it aside", candidate.display())
        })?);
        if identity.key != expected.key {
            bail!("“{}” changed while being moved aside to replace it", path.display());
        }
        return Ok(ReplacedItem { path: path.to_path_buf(), quarantine: candidate, identity });
    }
    bail!("Could not reserve a unique replacement quarantine path")
}

/// Release a quarantined object, because nothing can restore it any more.
///
/// The path alone does not justify the deletion. Marcel created it by an atomic
/// rename under a process-namespaced name, which proves who held it *then*;
/// eviction happens arbitrarily later, and another process is free to remove
/// that quarantine and leave something else at the same name. The recorded
/// identity is what makes this safe, so a mismatch leaves the object alone.
pub fn erase_replacement_quarantine(item: &ReplacedItem) {
    erase_quarantined_object(&item.quarantine, &item.identity);
}

/// Remove `path`, but only while it is still the object `expected` describes.
///
/// Returns whether it was removed. The abandoned sweep has no record to compare
/// against, so it passes the identity it read while scanning: that narrows the
/// gap between deciding and deleting to a single `stat` rather than the lifetime
/// of a directory listing. It cannot close the gap — which is why data Marcel
/// failed to restore never carries a name that sweep will consider.
fn erase_quarantined_object(path: &Path, expected: &FileIdentity) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if FileIdentity::of(&metadata) != *expected {
        return false;
    }
    let removed = if metadata.file_type().is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    removed.is_ok()
}

/// Originals a restoration could not put back, and the failure that stopped it.
pub(super) struct UnrestoredItems<'a> {
    error: anyhow::Error,
    /// Still in quarantine, and so still Marcel's responsibility.
    remaining: &'a [ReplacedItem],
}

/// Put displaced objects back where they came from.
///
/// Restoration walks in reverse, so the items still in quarantine when it stops
/// are exactly the ones it had not reached yet plus the one that failed. The
/// caller owes those items a home; it cannot treat this as a plain error.
pub(super) fn restore_replaced_items(replaced: &[ReplacedItem]) -> Result<(), UnrestoredItems<'_>> {
    for (index, item) in replaced.iter().enumerate().rev() {
        let unrestored =
            |error: anyhow::Error| UnrestoredItems { error, remaining: &replaced[..=index] };
        item.identity
            .validate(&item.quarantine, "restore the replaced item")
            .map_err(unrestored)?;
        rename_no_replace(&item.quarantine, &item.path).map_err(|error| {
            unrestored(anyhow::Error::new(error).context(format!(
                "Could not put “{}” back after undoing a replacement",
                item.path.display()
            )))
        })?;
    }
    Ok(())
}

/// Make every original a restoration could not put back findable again, and
/// say where each one is.
///
/// This is the whole difference between a failed rollback and data loss. The
/// object is sitting in storage named for undo, which a later Marcel is
/// entitled to reclaim once this process is gone; moving it into recovery
/// storage takes it out of that sweep's reach and puts it where the browser
/// points the user at it.
pub(super) fn preserve_unrestored(unrestored: UnrestoredItems<'_>) -> anyhow::Error {
    let UnrestoredItems { error, remaining } = unrestored;
    let notes = remaining
        .iter()
        .map(|item| match promote_to_recovery(item) {
            Ok(recovery) => {
                format!("“{}” is preserved at “{}”", item.path.display(), recovery.display())
            }
            Err(failure) => format!(
                "“{}” remains at “{}” ({failure})",
                item.path.display(),
                item.quarantine.display()
            ),
        })
        .collect::<Vec<_>>();
    anyhow::anyhow!("{error}; your original {}", notes.join("; your original "))
}

/// Move a quarantine Marcel could not restore into recovery storage.
///
/// Returns where the data is now. A plain rename within one directory is about
/// as reliable as a filesystem operation gets; when even this fails, the object
/// keeps its quarantine name and the caller says so, because a message naming
/// the wrong path would be worse than a long one.
fn promote_to_recovery(item: &ReplacedItem) -> Result<PathBuf> {
    let parent = item.quarantine.parent().context("Quarantined item has no parent directory")?;
    let name = item.path.file_name().context("Replaced item has no file name")?;
    for _ in 0..1024 {
        let sequence = REPLACEMENT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(quarantined_name(RECOVERY_REMNANT_PREFIX, sequence, name));
        match rename_no_replace(&item.quarantine, &candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(anyhow::Error::new(error).context(format!(
                    "Could not move “{}” into recovery storage",
                    item.quarantine.display()
                )));
            }
        }
    }
    bail!("Could not reserve a unique recovery path")
}
