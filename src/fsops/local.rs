//! The primitives every mutation is built from.
//!
//! Every risky syscall Marcel makes goes through here: the one rename that
//! never replaces, the one open that refuses to block on a FIFO, the one
//! directory creation that keeps derived data private. Keeping them in one
//! short file is what makes the safety story reviewable.

use std::{
    ffi::{OsStr, OsString},
    fs, io,
    os::fd::{AsFd as _, BorrowedFd},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use rustix::fs::OFlags;

/// Name the path an error is about, the way every message in this layer
/// does: `Could not create “/home/me/photos”`.
pub trait PathContext<T> {
    fn at(self, message: &str, path: &Path) -> Result<T>;
}

impl<T, E> PathContext<T> for Result<T, E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn at(self, message: &str, path: &Path) -> Result<T> {
        self.with_context(|| format!("{message} “{}”", path.display()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathOccupancy {
    Vacant,
    Occupied,
}

pub fn path_occupancy(path: &Path) -> io::Result<PathOccupancy> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(PathOccupancy::Occupied),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(PathOccupancy::Vacant),
        Err(error) => Err(error),
    }
}

/// Refuse a destination that something already occupies.
///
/// Symbolic links count, dangling or not: the question is whether the name is
/// taken, not what it points at.
pub fn ensure_unoccupied(path: &Path) -> Result<()> {
    match path_occupancy(path) {
        Ok(PathOccupancy::Occupied) => {
            bail!("“{}” already exists; nothing was overwritten", path.display())
        }
        Ok(PathOccupancy::Vacant) => Ok(()),
        Err(error) => Err(error).at("Could not inspect destination", path),
    }
}

/// `symlink_metadata` with the error every caller would otherwise spell out.
pub fn inspect(path: &Path) -> Result<fs::Metadata> {
    fs::symlink_metadata(path).at("Could not inspect", path)
}

/// A directory's children in name order, fully read before any is used.
///
/// Sorting gives every walk a deterministic order, and reading the whole
/// listing first means a directory that changes underneath the walk cannot
/// make `read_dir` yield an entry twice.
pub fn sorted_children(path: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut children = fs::read_dir(path)
        .at("Could not read", path)?
        .collect::<io::Result<Vec<_>>>()
        .at("Could not read an entry in", path)?;
    children.sort_by_key(fs::DirEntry::file_name);
    Ok(children)
}

/// Create a cache directory readable only by its owner.
///
/// Thumbnails and rendered PDF pages are derived from the user's files and are
/// exactly as private as the originals; the freedesktop thumbnail specification
/// requires `0700` for that reason. Plain `create_dir_all` leaves the mode to
/// the process umask, which is a weaker promise than the data deserves.
pub fn create_private_dir_all(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    fs::DirBuilder::new().recursive(true).mode(0o700).create(path)
}

/// What every open in this file starts from.
///
/// `O_CLOEXEC` keeps a descriptor out of the helpers Marcel spawns, and
/// `O_NOCTTY` means a terminal device planted under a file's name cannot
/// become the process's controlling terminal when Marcel opens it. `std` adds
/// the first on its own; the raw `openat` calls below have to ask for it.
const OPEN_FLAGS: OFlags = OFlags::CLOEXEC.union(OFlags::NOCTTY);

/// Open `path` for reading, refusing anything but a regular file.
///
/// `open(2)` on a FIFO with no writer blocks forever, and a blocked open cannot
/// be interrupted by a cancellation flag — the worker thread is leaked for the
/// life of the process. Opening with `O_NONBLOCK` and verifying the *opened*
/// descriptor closes that hole without a stat-then-open race; on a regular
/// file the flag has no effect on reads.
///
/// Symbolic links are followed: previews and openers work on what the user
/// pointed at. The copier, which recreates links instead, uses
/// [`open_regular_file_at`].
pub fn open_regular_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags((OFlags::NONBLOCK | OFlags::NOCTTY).bits() as i32)
        .open(path)?;
    ensure_regular(file, path)
}

fn ensure_regular(file: fs::File, path: &Path) -> io::Result<fs::File> {
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("“{}” is not a regular file", path.display()),
        ));
    }
    Ok(file)
}

/// The directory a name is resolved in when the caller holds no parent: the
/// working directory, which makes an absolute path behave as itself.
pub const CWD: BorrowedFd<'static> = rustix::fs::CWD;

/// Take hold of `name` inside `dir` without opening it.
///
/// An `O_PATH` descriptor refers to the object itself — its `metadata()` is
/// what `lstat` would report — but opens nothing, so a FIFO cannot block it
/// and a device cannot react to it. It is the copier's `symlink_metadata`,
/// with the difference that a real open of the same name can then be checked
/// against the object that was inspected. The final component is never
/// followed.
pub fn hold_entry(dir: BorrowedFd<'_>, name: &Path) -> io::Result<fs::File> {
    let fd = rustix::fs::openat(
        dir,
        name,
        OPEN_FLAGS | OFlags::PATH | OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )?;
    Ok(fs::File::from(fd))
}

/// Take hold of the directory `path` leads to, following links on the way.
///
/// Like [`hold_entry`] this opens nothing, so it needs only search permission
/// on the directory — the self-containment check walks ancestors the user may
/// not be allowed to list — and it fails with `ENOTDIR` for anything else.
pub fn hold_directory(dir: BorrowedFd<'_>, path: &Path) -> io::Result<fs::File> {
    let fd = rustix::fs::openat(
        dir,
        path,
        OPEN_FLAGS | OFlags::PATH | OFlags::DIRECTORY,
        rustix::fs::Mode::empty(),
    )?;
    Ok(fs::File::from(fd))
}

/// Open the directory `name` inside `dir`, refusing to follow a link there.
///
/// Walking a tree one descriptor per level is what closes the window a
/// path-based walker leaves open: once a level is held, renaming a component
/// above it cannot redirect the rest of the walk.
pub fn open_directory_at(dir: BorrowedFd<'_>, name: &Path) -> io::Result<fs::File> {
    let fd = rustix::fs::openat(
        dir,
        name,
        OPEN_FLAGS | OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )?;
    Ok(fs::File::from(fd))
}

/// [`open_regular_file`] relative to a held directory, refusing to follow a
/// link in the final component.
pub fn open_regular_file_at(dir: BorrowedFd<'_>, name: &Path) -> io::Result<fs::File> {
    let fd = rustix::fs::openat(
        dir,
        name,
        OPEN_FLAGS | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )?;
    ensure_regular(fs::File::from(fd), name)
}

/// The target of a held symbolic link.
///
/// Reading through the descriptor rather than the path means the link whose
/// metadata was inspected is the link whose target is copied.
pub fn read_link_target(link: &fs::File) -> io::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt as _;

    let target = rustix::fs::readlinkat(link.as_fd(), "", Vec::new())?;
    Ok(PathBuf::from(OsString::from_vec(target.into_bytes())))
}

/// [`sorted_children`] for a held directory: its names in name order, fully
/// read before any is used.
pub fn sorted_child_names(dir: &fs::File) -> io::Result<Vec<OsString>> {
    use std::os::unix::ffi::OsStrExt as _;

    let mut names = rustix::fs::Dir::read_from(dir)?
        .map(|entry| {
            entry
                .map(|entry| OsStr::from_bytes(entry.file_name().to_bytes()).to_owned())
                .map_err(io::Error::from)
        })
        .filter(|name| !matches!(name.as_deref(), Ok(name) if name == "." || name == ".."))
        .collect::<io::Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}

pub fn rename_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(test)]
    if fault::should_fail(destination) {
        return Err(io::Error::other("injected rename failure"));
    }
    #[cfg(test)]
    if fault::take_crossing(destination) {
        return Err(io::Error::from(rustix::io::Errno::XDEV));
    }
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        source,
        rustix::fs::CWD,
        destination,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(|error| diagnose_rename(error, destination))
}

/// Name the filesystem, rather than the request, when it is the obstacle.
///
/// Marcel publishes everything through `RENAME_NOREPLACE`, so a filesystem that
/// does not implement it fails every rename, move, publication, restore, and
/// quarantine — with an error that mentions none of that and reads as though
/// Marcel is broken. `EINVAL` is the ambiguous one and is hedged accordingly:
/// the unambiguous causes are ruled out before any caller reaches here.
fn diagnose_rename(error: rustix::io::Errno, destination: &Path) -> io::Error {
    let unsupported = matches!(
        error,
        rustix::io::Errno::NOSYS | rustix::io::Errno::OPNOTSUPP | rustix::io::Errno::INVAL
    );
    let error = io::Error::from(error);
    if !unsupported {
        return error;
    }
    io::Error::new(
        error.kind(),
        format!(
            "{error}; “{}” may be on a filesystem without RENAME_NOREPLACE, which Marcel needs to publish a file without overwriting one",
            destination.display()
        ),
    )
}

/// The longest single path component most Linux filesystems accept, in bytes.
pub const MAX_NAME_BYTES: usize = 255;

/// Compose a hidden name carrying an original name, without exceeding `NAME_MAX`.
///
/// Marcel's own bookkeeping must never be the reason an operation the
/// filesystem would have allowed fails, and prepending a prefix to a name
/// already near the limit is exactly that. The tail of the original is dropped
/// instead: uniqueness comes from the sequence number, and the real path is in
/// the record, so the copied-in name only has to stay recognizable.
pub fn quarantined_name(prefix: &str, sequence: u64, original: &std::ffi::OsStr) -> OsString {
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

    let mut name = format!("{prefix}{sequence}-").into_bytes();
    let original = original.as_bytes();
    let keep = original.len().min(MAX_NAME_BYTES.saturating_sub(name.len()));
    name.extend_from_slice(&original[..floor_char_boundary(original, keep)]);
    OsString::from_vec(name)
}

/// Trim to `limit` without splitting a UTF-8 character.
///
/// Cutting inside a multi-byte character turns a readable name into a mojibake
/// one, so step back to a character boundary. Raw non-UTF-8 names are carried
/// through byte for byte, which lossy conversion would not do.
pub fn floor_char_boundary(bytes: &[u8], limit: usize) -> usize {
    let mut end = limit.min(bytes.len());
    while end > 0 && end < bytes.len() && bytes[end] & 0b1100_0000 == 0b1000_0000 {
        end -= 1;
    }
    end
}

/// Deterministic failure injection at Marcel's one commit-boundary rename.
///
/// Every publication, quarantine, restoration, and move commits through
/// `rename_no_replace`, so a single hook here reaches every commit boundary in
/// the tree without scattering `#[cfg(test)]` through the operation layer.
/// Faults are keyed on the destination's file name, because that is what a test
/// can name before the operation runs, and they are thread-local so tests that
/// run in parallel cannot inject into one another.
#[cfg(test)]
pub mod fault {
    use std::{
        cell::RefCell,
        ffi::{OsStr, OsString},
        path::Path,
    };

    thread_local! {
        static FAILING_DESTINATIONS: RefCell<Vec<OsString>> = const { RefCell::new(Vec::new()) };
        static CROSSING_DESTINATIONS: RefCell<Vec<OsString>> = const { RefCell::new(Vec::new()) };
    }

    /// Make the next rename that would publish something under this file
    /// name fail with `EXDEV`, as a rename onto another filesystem does.
    ///
    /// One-shot, because the copy that a cross-device move falls back to
    /// publishes under the same name, and that rename has to succeed. A test
    /// that needs a real second filesystem uses `/dev/shm`; this is for the
    /// logic around the boundary, which does not care where it is.
    pub fn cross_devices_once(name: impl AsRef<OsStr>) {
        let name = name.as_ref().to_os_string();
        CROSSING_DESTINATIONS.with_borrow_mut(|names| names.push(name));
    }

    pub(super) fn take_crossing(destination: &Path) -> bool {
        let Some(name) = destination.file_name() else {
            return false;
        };
        CROSSING_DESTINATIONS
            .with_borrow_mut(|names| {
                names.iter().rposition(|crossing| crossing == name).map(|index| names.remove(index))
            })
            .is_some()
    }

    /// Fail every rename that would publish something under this file name.
    ///
    /// The returned guard removes the fault, so a fault cannot outlive its test
    /// on a thread the harness reuses.
    #[must_use = "the fault is removed when the guard drops"]
    pub fn fail_renames_to(name: impl AsRef<OsStr>) -> Guard {
        let name = name.as_ref().to_os_string();
        FAILING_DESTINATIONS.with_borrow_mut(|names| names.push(name.clone()));
        Guard { name }
    }

    pub struct Guard {
        name: OsString,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            FAILING_DESTINATIONS.with_borrow_mut(|names| {
                if let Some(index) = names.iter().rposition(|name| *name == self.name) {
                    names.remove(index);
                }
            });
        }
    }

    pub(super) fn should_fail(destination: &Path) -> bool {
        let Some(name) = destination.file_name() else {
            return false;
        };
        FAILING_DESTINATIONS.with_borrow(|names| names.iter().any(|failing| failing == name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occupancy_counts_dangling_symlinks_as_occupied() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let link = root.path().join("link");
        symlink("missing", &link).unwrap();

        assert_eq!(path_occupancy(&link).unwrap(), PathOccupancy::Occupied);
        assert_eq!(path_occupancy(&root.path().join("vacant")).unwrap(), PathOccupancy::Vacant);
        assert!(ensure_unoccupied(&link).is_err());
    }

    #[test]
    fn rename_never_replaces_an_occupied_destination() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        fs::write(&source, b"source").unwrap();
        fs::write(&destination, b"destination").unwrap();

        assert!(rename_no_replace(&source, &destination).is_err());
        assert_eq!(fs::read(source).unwrap(), b"source");
        assert_eq!(fs::read(destination).unwrap(), b"destination");
    }

    #[test]
    fn an_injected_fault_fails_only_its_own_destination_and_only_while_held() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::write(&source, b"source").unwrap();

        let guard = fault::fail_renames_to("blocked");
        assert!(rename_no_replace(&source, &root.path().join("blocked")).is_err());
        assert!(source.exists(), "an injected fault performs no rename");
        rename_no_replace(&source, &root.path().join("allowed")).unwrap();

        drop(guard);
        rename_no_replace(&root.path().join("allowed"), &root.path().join("blocked")).unwrap();
    }

    #[test]
    fn a_quarantine_name_stays_within_the_length_limit_on_a_character_boundary() {
        use std::os::unix::ffi::OsStrExt as _;

        let original = OsString::from("猫".repeat(120));
        let name = quarantined_name(".marcel-replaced-1-", 7, &original);
        assert!(name.as_bytes().len() <= MAX_NAME_BYTES);
        assert!(name.to_str().is_some(), "the cut lands on a character boundary");
        assert!(name.to_string_lossy().starts_with(".marcel-replaced-1-7-猫"));
    }
}
