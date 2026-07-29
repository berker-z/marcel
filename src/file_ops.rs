use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};

pub const OPERATION_HISTORY_LIMIT: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    device: u64,
    inode: u64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationRecord {
    CreateDirectory {
        path: PathBuf,
        identity: FileIdentity,
    },
}

impl OperationRecord {
    pub fn path(&self) -> &Path {
        match self {
            Self::CreateDirectory { path, .. } => path,
        }
    }
}

#[derive(Debug)]
pub struct OperationJournal {
    undo: VecDeque<OperationRecord>,
    redo: VecDeque<OperationRecord>,
    limit: usize,
}

impl Default for OperationJournal {
    fn default() -> Self {
        Self::new(OPERATION_HISTORY_LIMIT)
    }
}

impl OperationJournal {
    pub fn new(limit: usize) -> Self {
        Self {
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            limit,
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn record(&mut self, operation: OperationRecord) {
        self.redo.clear();
        push_bounded(&mut self.undo, operation, self.limit);
    }

    pub fn begin_undo(&mut self) -> Option<OperationRecord> {
        self.undo.pop_back()
    }

    pub fn finish_undo(&mut self, operation: OperationRecord) {
        push_bounded(&mut self.redo, operation, self.limit);
    }

    pub fn cancel_undo(&mut self, operation: OperationRecord) {
        push_bounded(&mut self.undo, operation, self.limit);
    }

    pub fn begin_redo(&mut self) -> Option<OperationRecord> {
        self.redo.pop_back()
    }

    pub fn finish_redo(&mut self, operation: OperationRecord) {
        push_bounded(&mut self.undo, operation, self.limit);
    }

    pub fn cancel_redo(&mut self, operation: OperationRecord) {
        push_bounded(&mut self.redo, operation, self.limit);
    }
}

fn push_bounded(stack: &mut VecDeque<OperationRecord>, operation: OperationRecord, limit: usize) {
    if limit == 0 {
        return;
    }
    while stack.len() >= limit {
        stack.pop_front();
    }
    stack.push_back(operation);
}

pub fn validate_entry_name(name: &str) -> Result<()> {
    if name.is_empty() || name.trim().is_empty() {
        bail!("Enter a folder name");
    }
    if name == "." || name == ".." {
        bail!("“{name}” is reserved and cannot be used as a folder name");
    }
    if name.contains('/') || name.contains('\0') {
        bail!("Folder names cannot contain “/” or a null character");
    }
    Ok(())
}

pub fn create_directory(parent: &Path, name: &str) -> Result<OperationRecord> {
    validate_entry_name(name)?;
    create_directory_at(parent.join(name))
}

pub fn undo_operation(operation: &OperationRecord) -> Result<()> {
    match operation {
        OperationRecord::CreateDirectory { path, identity } => {
            let metadata = fs::symlink_metadata(path)
                .with_context(|| format!("Cannot undo: “{}” no longer exists", path.display()))?;
            if !metadata.file_type().is_dir() {
                bail!("Cannot undo: “{}” is no longer a directory", path.display());
            }
            if fs::read_dir(path)
                .with_context(|| format!("Cannot inspect “{}”", path.display()))?
                .next()
                .is_some()
            {
                bail!("Cannot undo: “{}” is no longer empty", path.display());
            }
            if file_identity(&metadata) != *identity {
                bail!("Cannot undo: “{}” changed or was replaced", path.display());
            }
            fs::remove_dir(path)
                .with_context(|| format!("Could not remove “{}”", path.display()))?;
            Ok(())
        }
    }
}

pub fn redo_operation(operation: &OperationRecord) -> Result<OperationRecord> {
    match operation {
        OperationRecord::CreateDirectory { path, .. } => create_directory_at(path.clone()),
    }
}

fn create_directory_at(path: PathBuf) -> Result<OperationRecord> {
    fs::create_dir(&path).with_context(|| format!("Could not create “{}”", path.display()))?;
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("Created “{}” but could not inspect it", path.display()))?;
    if !metadata.file_type().is_dir() {
        bail!(
            "Created path “{}” is not a directory; refusing to record it",
            path.display()
        );
    }

    Ok(OperationRecord::CreateDirectory {
        path,
        identity: file_identity(&metadata),
    })
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt as _;

    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    }
}

#[cfg(not(unix))]
compile_error!("Marcel's safe file-operation identity checks currently require Unix metadata");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_names_that_escape_the_parent_or_have_no_name() {
        for name in ["", " ", ".", "..", "nested/folder", "bad\0name"] {
            assert!(validate_entry_name(name).is_err(), "{name:?} was accepted");
        }
        assert!(validate_entry_name(".config").is_ok());
        assert!(validate_entry_name("New Folder").is_ok());
    }

    #[test]
    fn create_never_overwrites_an_occupied_destination() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("occupied"), b"keep me").unwrap();

        assert!(create_directory(root.path(), "occupied").is_err());
        assert_eq!(fs::read(root.path().join("occupied")).unwrap(), b"keep me");
    }

    #[test]
    fn create_undo_and_redo_validate_the_path() {
        let root = tempfile::tempdir().unwrap();
        let created = create_directory(root.path(), "photos").unwrap();

        undo_operation(&created).unwrap();
        assert!(!created.path().exists());

        let recreated = redo_operation(&created).unwrap();
        assert!(recreated.path().is_dir());
    }

    #[test]
    fn undo_refuses_a_non_empty_created_directory() {
        let root = tempfile::tempdir().unwrap();
        let created = create_directory(root.path(), "work").unwrap();
        fs::write(created.path().join("important.txt"), b"data").unwrap();

        assert!(undo_operation(&created).is_err());
        assert_eq!(
            fs::read(created.path().join("important.txt")).unwrap(),
            b"data"
        );
    }

    #[test]
    fn undo_refuses_a_replacement_at_the_same_path() {
        let root = tempfile::tempdir().unwrap();
        let created = create_directory(root.path(), "replace-me").unwrap();
        fs::remove_dir(created.path()).unwrap();
        fs::create_dir(created.path()).unwrap();

        assert!(undo_operation(&created).is_err());
        assert!(created.path().is_dir());
    }

    #[test]
    fn history_is_bounded_and_new_work_clears_redo() {
        let root = tempfile::tempdir().unwrap();
        let mut journal = OperationJournal::new(2);
        let first = create_directory(root.path(), "first").unwrap();
        let second = create_directory(root.path(), "second").unwrap();
        let third = create_directory(root.path(), "third").unwrap();
        journal.record(first);
        journal.record(second.clone());
        journal.record(third.clone());

        assert_eq!(journal.begin_undo(), Some(third.clone()));
        assert_eq!(journal.begin_undo(), Some(second.clone()));
        assert_eq!(journal.begin_undo(), None);

        journal.finish_undo(second.clone());
        assert!(journal.can_redo());
        journal.cancel_undo(third);
        journal.record(second);
        assert!(!journal.can_redo());
    }
}
