use std::{
    cmp::Ordering,
    ffi::OsStr,
    fs::DirEntry,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
};

use async_channel::Sender;

use crate::icons::IconProvider;

const DIRECTORY_BATCH_SIZE: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub path: PathBuf,
    pub name: String,
    pub kind: EntryKind,
    pub navigable: bool,
    pub size: Option<u64>,
    pub icon_path: Option<PathBuf>,
}

impl FileEntry {
    fn from_dir_entry(entry: DirEntry, icons: &mut IconProvider) -> io::Result<Self> {
        let path = entry.path();
        let file_type = entry.file_type()?;
        let followed_metadata = if file_type.is_symlink() {
            path.metadata().ok()
        } else {
            entry.metadata().ok()
        };
        Ok(Self::from_parts(
            path,
            entry.file_name(),
            file_type,
            followed_metadata,
            icons,
        ))
    }

    pub(crate) fn from_path(path: &Path, icons: &mut IconProvider) -> io::Result<Self> {
        let metadata = std::fs::symlink_metadata(path)?;
        let file_type = metadata.file_type();
        let followed_metadata = if file_type.is_symlink() {
            path.metadata().ok()
        } else {
            Some(metadata)
        };
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?
            .to_os_string();
        Ok(Self::from_parts(
            path.to_path_buf(),
            name,
            file_type,
            followed_metadata,
            icons,
        ))
    }

    fn from_parts(
        path: PathBuf,
        name: std::ffi::OsString,
        file_type: std::fs::FileType,
        followed_metadata: Option<std::fs::Metadata>,
        icons: &mut IconProvider,
    ) -> Self {
        let kind = if file_type.is_dir() {
            EntryKind::Directory
        } else if file_type.is_file() {
            EntryKind::File
        } else if file_type.is_symlink() {
            EntryKind::Symlink
        } else {
            EntryKind::Other
        };

        let navigable =
            file_type.is_dir() || followed_metadata.as_ref().is_some_and(|meta| meta.is_dir());
        Self {
            name: name.to_string_lossy().into_owned(),
            navigable,
            size: followed_metadata
                .as_ref()
                .filter(|meta| meta.is_file())
                .map(|meta| meta.len()),
            icon_path: icons.icon_for(&path, navigable),
            path,
            kind,
        }
    }

    pub fn display_kind(&self) -> &'static str {
        match self.kind {
            EntryKind::Directory => "Folder",
            EntryKind::File => "File",
            EntryKind::Symlink => "Symbolic link",
            EntryKind::Other => "Special file",
        }
    }

    pub fn icon(&self) -> &'static str {
        if self.navigable { "▸" } else { "·" }
    }
}

#[derive(Debug)]
pub enum DirectoryUpdate {
    Batch(Vec<FileEntry>),
    Done,
    Error(String),
}

/// Enumerate a directory in bounded batches.
///
/// This follows Yazi's partial-update and ticketed directory-loading model:
/// https://github.com/sxyazi/yazi/blob/main/yazi-fs/src/op.rs
/// https://github.com/sxyazi/yazi/blob/main/yazi-fs/src/entries.rs
pub fn stream_directory(path: &Path, sender: Sender<DirectoryUpdate>) {
    stream_directory_inner(path, sender, None);
}

pub fn stream_directory_cancellable(
    path: &Path,
    sender: Sender<DirectoryUpdate>,
    cancelled: Arc<AtomicBool>,
) {
    stream_directory_inner(path, sender, Some(&cancelled));
}

fn stream_directory_inner(
    path: &Path,
    sender: Sender<DirectoryUpdate>,
    cancelled: Option<&AtomicBool>,
) {
    if cancelled.is_some_and(|cancelled| cancelled.load(AtomicOrdering::Acquire)) {
        return;
    }

    let reader = match std::fs::read_dir(path) {
        Ok(reader) => reader,
        Err(error) => {
            let _ = sender.send_blocking(DirectoryUpdate::Error(error.to_string()));
            return;
        }
    };

    let mut icons = IconProvider::discover();
    let mut batch = Vec::with_capacity(DIRECTORY_BATCH_SIZE);
    for entry in reader {
        if cancelled.is_some_and(|cancelled| cancelled.load(AtomicOrdering::Acquire)) {
            return;
        }
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(entry) = FileEntry::from_dir_entry(entry, &mut icons) else {
            continue;
        };

        batch.push(entry);
        if batch.len() == DIRECTORY_BATCH_SIZE {
            batch.sort_by(compare_entries);
            if sender
                .send_blocking(DirectoryUpdate::Batch(std::mem::take(&mut batch)))
                .is_err()
            {
                return;
            }
            batch = Vec::with_capacity(DIRECTORY_BATCH_SIZE);
        }
    }

    if !batch.is_empty() {
        batch.sort_by(compare_entries);
        if sender.send_blocking(DirectoryUpdate::Batch(batch)).is_err() {
            return;
        }
    }
    let _ = sender.send_blocking(DirectoryUpdate::Done);
}

pub fn merge_sorted_entries(left: Vec<FileEntry>, right: Vec<FileEntry>) -> Vec<FileEntry> {
    let mut left = left.into_iter().peekable();
    let mut right = right.into_iter().peekable();
    let mut merged = Vec::with_capacity(left.len() + right.len());

    while let (Some(a), Some(b)) = (left.peek(), right.peek()) {
        if compare_entries(a, b).is_le() {
            merged.push(left.next().expect("peeked left entry"));
        } else {
            merged.push(right.next().expect("peeked right entry"));
        }
    }
    merged.extend(left);
    merged.extend(right);
    merged
}

pub(crate) fn sort_entries(entries: &mut [FileEntry]) {
    entries.sort_by(compare_entries);
}

fn compare_entries(a: &FileEntry, b: &FileEntry) -> Ordering {
    b.navigable
        .cmp(&a.navigable)
        .then_with(|| compare_names(&a.name, &b.name))
}

fn compare_names(a: &str, b: &str) -> Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| OsStr::new(a).cmp(OsStr::new(b)))
}

pub fn format_size(size: Option<u64>) -> String {
    let Some(size) = size else {
        return String::new();
    };

    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{size} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, navigable: bool) -> FileEntry {
        FileEntry {
            path: PathBuf::from(name),
            name: name.to_string(),
            kind: if navigable {
                EntryKind::Directory
            } else {
                EntryKind::File
            },
            navigable,
            size: None,
            icon_path: None,
        }
    }

    #[test]
    fn directories_sort_before_files_case_insensitively() {
        let mut entries = [
            entry("z.txt", false),
            entry("beta", true),
            entry("Alpha", true),
            entry("A.txt", false),
        ];
        entries.sort_by(compare_entries);

        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["Alpha", "beta", "A.txt", "z.txt"]
        );
    }

    #[test]
    fn merges_sorted_batches() {
        let left = vec![entry("Alpha", true), entry("b.txt", false)];
        let right = vec![entry("Beta", true), entry("a.txt", false)];

        let merged = merge_sorted_entries(left, right);

        assert_eq!(
            merged
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["Alpha", "Beta", "a.txt", "b.txt"]
        );
    }

    #[test]
    fn formats_file_sizes() {
        assert_eq!(format_size(Some(42)), "42 B");
        assert_eq!(format_size(Some(1536)), "1.5 KiB");
        assert_eq!(format_size(None), "");
    }

    #[test]
    fn cancelled_directory_stream_publishes_nothing() {
        let cancelled = Arc::new(AtomicBool::new(true));
        let (sender, receiver) = async_channel::unbounded();

        stream_directory_cancellable(Path::new("."), sender, cancelled);

        assert!(receiver.try_recv().is_err());
    }
}
