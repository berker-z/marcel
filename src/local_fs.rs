use std::{fs, io, path::Path};

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

/// Create a cache directory readable only by its owner.
///
/// Thumbnails and rendered PDF pages are derived from the user's files and are
/// exactly as private as the originals; the freedesktop thumbnail specification
/// requires `0700` for that reason. Plain `create_dir_all` leaves the mode to
/// the process umask, which is a weaker promise than the data deserves.
pub fn create_private_dir_all(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

pub fn rename_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(test)]
    if fault::should_fail(destination) {
        return Err(io::Error::other("injected rename failure"));
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
        assert_eq!(
            path_occupancy(&root.path().join("vacant")).unwrap(),
            PathOccupancy::Vacant
        );
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
}
