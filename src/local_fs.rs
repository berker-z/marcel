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

pub fn rename_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        source,
        rustix::fs::CWD,
        destination,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(Into::into)
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
}
