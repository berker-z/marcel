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
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use super::local::PathContext as _;
use anyhow::{Context as _, Result, bail};

use super::{
    identity::FileIdentity,
    local::{inspect, quarantined_name, rename_no_replace},
};

static WORKING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// How many bytes of replaced data one operation may hold aside for undo.
///
/// Quarantining what a replace displaced is what makes replacement reversible,
/// but it is real disk held for as long as the record lives. This follows the
/// rule `UNDO_SNAPSHOT_LIMIT` already sets: past the budget the operation
/// still happens, it simply stops being undoable and says so. The alternative —
/// refusing to replace large files — would be a worse answer to a question the
/// user already asked.
pub const REPLACEMENT_UNDO_BYTE_LIMIT: u64 = 1024 * 1024 * 1024;

/// What Marcel is keeping in a working file or directory of its own.
///
/// Every kind is named `.marcel-<kind>-<boot>-<pid>-<sequence>-<rest>`. The
/// process id lets a later Marcel tell its own live working state from what
/// a dead process abandoned, which is the same rule permanent deletion
/// already uses for its own remnants. The boot id is what makes that rule
/// safe to act on: process ids are only meaningful within one boot and one
/// pid namespace, and a name can arrive from anywhere — a backup restored
/// mid-operation, an `rsync` from another machine, an archive made from a
/// folder that had one. A name from another boot is never hidden, so it
/// cannot become invisible garbage; whether it is reclaimed depends on the
/// kind, see [`WorkingOwner::is_abandoned`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkingKind {
    /// An object a replacement displaced, held so undo can put it back. The
    /// rest of the name is the original's.
    Replaced,
    /// A copy's output, private until one rename publishes it. The rest of
    /// the name is `tempfile`'s random suffix.
    Copy,
    /// An archive's extracted or created output, likewise.
    Archive,
}

impl WorkingKind {
    const ALL: [Self; 3] = [Self::Replaced, Self::Copy, Self::Archive];

    fn prefix(self) -> &'static str {
        match self {
            Self::Replaced => ".marcel-replaced-",
            Self::Copy => ".marcel-copy-",
            Self::Archive => ".marcel-archive-",
        }
    }
}

/// What this process's working names of `kind` begin with, up to the sequence
/// number: `.marcel-<kind>-<boot>-<pid>-`.
fn owner_prefix(kind: WorkingKind) -> String {
    format!("{}{}-{}-", kind.prefix(), boot_id(), std::process::id())
}

/// A fresh `.marcel-<kind>-<boot>-<pid>-<sequence>-`, for a staging directory
/// that `tempfile::Builder` completes with its own random suffix.
///
/// Staging carries an owner for the same reason a replacement quarantine
/// does: a crash mid-copy or mid-extraction leaves the directory behind, and
/// only a name that says who made it lets the next Marcel reclaim it instead
/// of hiding it forever.
pub fn staging_prefix(kind: WorkingKind) -> String {
    let sequence = WORKING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{}{sequence}-", owner_prefix(kind))
}

/// This boot, as `/proc/sys/kernel/random/boot_id` names it: 32 hex digits.
///
/// Read once; the kernel does not change its mind. Without `/proc` — no
/// Linux system Marcel targets — every boot reads as the same zero id, which
/// scopes nothing and breaks nothing.
pub fn boot_id() -> &'static str {
    static BOOT_ID: OnceLock<String> = OnceLock::new();
    BOOT_ID.get_or_init(|| {
        fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()
            .map(|id| id.chars().filter(char::is_ascii_hexdigit).collect::<String>())
            .filter(|id| id.len() == BOOT_ID_LENGTH)
            .unwrap_or_else(|| "0".repeat(BOOT_ID_LENGTH))
    })
}

const BOOT_ID_LENGTH: usize = 32;

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
    os_bytes(name).starts_with(WorkingKind::Replaced.prefix().as_bytes())
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
/// Only a name this boot's Marcel wrote qualifies. Permanent-delete
/// quarantines and recovery remnants are deliberately excluded: their recovery
/// guidance points the user straight at the path, so hiding them would make
/// that advice impossible to follow. So is anything from another boot, and any
/// staging name from before staging carried an owner: the sweep either
/// reclaims it on the next visit or never can, and a file nothing sweeps and
/// nothing shows is lost disk. Showing it costs a glance; hiding it costs the
/// disk.
pub fn is_internal_working_name(name: &OsStr) -> bool {
    working_owner(name).is_some_and(|owner| owner.boot == boot_id().as_bytes())
}

/// Whether a name is a replacement quarantine some other boot left behind.
///
/// Its owner is certainly gone, but so is the only context in which its name
/// meant anything, so it is left for the user rather than swept.
pub fn is_quarantine_from_another_boot(name: &OsStr) -> bool {
    working_owner(name).is_some_and(|owner| {
        owner.kind == WorkingKind::Replaced && owner.boot != boot_id().as_bytes()
    })
}

fn os_bytes(name: &OsStr) -> &[u8] {
    use std::os::unix::ffi::OsStrExt as _;
    name.as_bytes()
}

/// Who made a working file, and what for, as its name records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WorkingOwner<'a> {
    kind: WorkingKind,
    boot: &'a [u8],
    process: u32,
}

impl WorkingOwner<'_> {
    /// Whether nothing can want this any more.
    ///
    /// Within this boot the rule is the same for every kind: a dead owner can
    /// neither undo nor finish, so what it left is garbage. Across boots the
    /// kinds part ways. A replacement quarantine holds a displaced original,
    /// and a name from another boot cannot prove Marcel made it — it may be
    /// the only copy of something restored from a backup — so it is kept and
    /// shown. Staging holds a partial copy of a source that still exists,
    /// because neither a copy nor an extraction consumes its source, so
    /// another boot is proof enough that nobody is coming back for it.
    fn is_abandoned(&self) -> bool {
        if self.boot == boot_id().as_bytes() {
            self.process != std::process::id() && !process_is_running(self.process)
        } else {
            matches!(self.kind, WorkingKind::Copy | WorkingKind::Archive)
        }
    }
}

/// The owner a working name carries, if it has the shape Marcel writes.
/// Names from before the boot id was added parse as nobody's, which leaves
/// them alone — and visible.
fn working_owner(name: &OsStr) -> Option<WorkingOwner<'_>> {
    let bytes = os_bytes(name);
    let (kind, rest) = WorkingKind::ALL
        .into_iter()
        .find_map(|kind| bytes.strip_prefix(kind.prefix().as_bytes()).map(|rest| (kind, rest)))?;
    let (boot, rest) = rest.split_at_checked(BOOT_ID_LENGTH)?;
    if !boot.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let rest = rest.strip_prefix(b"-")?;
    let end = rest.iter().position(|byte| *byte == b'-')?;
    let process = std::str::from_utf8(&rest[..end]).ok()?.parse().ok()?;
    Some(WorkingOwner { kind, boot, process })
}

/// Whether a process is still running, and so might still be able to undo.
///
/// A live owner's quarantine is its own business: two Marcel processes can
/// exist when desktop integration is unavailable, and reclaiming another's
/// would destroy data it can still restore. An unknown answer keeps the file.
pub fn process_is_running(process: u32) -> bool {
    Path::new(&format!("/proc/{process}")).exists()
}

/// Release working state abandoned by processes that are gone: replacement
/// quarantines, and copy and archive staging.
///
/// Unlike an interrupted permanent deletion, this needs no user involvement.
/// A replaced file is one the user chose to overwrite, and once the process
/// holding its record is gone nothing can ever restore it, so it is provably
/// unreachable rather than possibly-wanted. Staging is the unfinished output
/// of a copy or extraction whose source is still where it was, so a crash
/// mid-way leaves nothing worth keeping — only up to the whole expanded size
/// of an archive in the user's folder. Returns how many were released.
///
/// What counts as abandoned is [`WorkingOwner::is_abandoned`]'s call.
pub fn reclaim_abandoned_quarantines(directory: &Path) -> usize {
    let Ok(entries) = fs::read_dir(directory) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| working_owner(&entry.file_name()).is_some_and(|owner| owner.is_abandoned()))
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
    let prefix = owner_prefix(WorkingKind::Replaced);

    for _ in 0..1024 {
        let sequence = WORKING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
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
        let sequence = WORKING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Sandbox;

    /// A staging name says who made it, so the hiding rule and the sweep can
    /// tell live work from what a crash left behind.
    #[test]
    fn staging_names_carry_a_parseable_owner() {
        for kind in [WorkingKind::Copy, WorkingKind::Archive] {
            let name = format!("{}a1b2c3", staging_prefix(kind));
            let owner = working_owner(OsStr::new(&name)).unwrap_or_else(|| panic!("{name}"));
            assert_eq!(owner.kind, kind);
            assert_eq!(owner.boot, boot_id().as_bytes());
            assert_eq!(owner.process, std::process::id());
            assert!(is_internal_working_name(OsStr::new(&name)), "{name}");
            assert!(!is_quarantine_from_another_boot(OsStr::new(&name)), "{name}");
        }
        assert_ne!(staging_prefix(WorkingKind::Copy), staging_prefix(WorkingKind::Copy));
    }

    /// Hidden is a promise that something will sweep. Staging from another
    /// boot will be swept on the next visit but is shown until then, in case
    /// the sweep cannot; a staging name without an owner will never be swept,
    /// so it is shown for good.
    #[test]
    fn only_this_boots_working_names_are_hidden() {
        let other_boot = "a".repeat(BOOT_ID_LENGTH);
        for name in [
            format!(".marcel-copy-{other_boot}-1-0-a1b2c3"),
            format!(".marcel-archive-{other_boot}-1-0-a1b2c3"),
            ".marcel-copy-1-0-staging".to_string(),
            ".marcel-archive-abc".to_string(),
            ".marcel-archive-".to_string(),
        ] {
            assert!(!is_internal_working_name(OsStr::new(&name)), "{name}");
            assert!(!is_quarantine_from_another_boot(OsStr::new(&name)), "{name}");
        }
    }

    /// Process id 0 is never a real process, so it stands in for a Marcel
    /// that is gone.
    #[test]
    fn the_sweep_reclaims_dead_and_foreign_staging_but_only_dead_replacements() {
        let sandbox = Sandbox::new();
        let boot = boot_id();
        let other_boot = "f".repeat(BOOT_ID_LENGTH);
        let live = sandbox.dir(&format!("{}a1b2c3", staging_prefix(WorkingKind::Copy)));
        let dead_copy = sandbox.dir(&format!(".marcel-copy-{boot}-0-0-a1b2c3"));
        let dead_archive = sandbox.dir(&format!(".marcel-archive-{boot}-0-0-a1b2c3"));
        sandbox.file(&format!(".marcel-archive-{boot}-0-0-a1b2c3/extracted/partial.bin"), b"?");
        let foreign_copy = sandbox.dir(&format!(".marcel-copy-{other_boot}-0-0-a1b2c3"));
        let foreign_replaced =
            sandbox.file(&format!(".marcel-replaced-{other_boot}-0-0-report.txt"), b"?");
        let legacy_copy = sandbox.dir(".marcel-copy-1-0-staging");
        let legacy_archive = sandbox.dir(".marcel-archive-abc");
        let ordinary = sandbox.file("report.txt", b"payload");

        assert_eq!(reclaim_abandoned_quarantines(sandbox.root()), 3);

        assert!(live.exists(), "this process may still be writing here");
        assert!(!dead_copy.exists(), "a dead owner's copy staging is partial output");
        assert!(!dead_archive.exists(), "a dead owner's extraction is partial output");
        assert!(!foreign_copy.exists(), "staging from another boot has no one coming back");
        assert!(foreign_replaced.exists(), "a displaced original from another boot is kept");
        assert!(legacy_copy.exists(), "a name without an owner is shown, not swept");
        assert!(legacy_archive.exists(), "a name without an owner is shown, not swept");
        assert!(ordinary.exists(), "user data is never touched");
    }
}
