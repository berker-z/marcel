use std::{
    cmp::Ordering,
    ffi::{OsStr, OsString},
    fs::DirEntry,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
    time::SystemTime,
};

use async_channel::Sender;

use crate::desktop::icons::IconProvider;

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
    pub name_os: OsString,
    pub(crate) folded_name: Arc<[char]>,
    pub kind: EntryKind,
    pub navigable: bool,
    pub size: Option<u64>,
    /// When the object — the target, for a link — was last written.
    pub modified: Option<SystemTime>,
    pub icon_path: Option<PathBuf>,
}

impl FileEntry {
    fn from_dir_entry(entry: DirEntry, icons: &mut IconProvider) -> io::Result<Self> {
        let path = entry.path();
        let file_type = entry.file_type()?;
        let followed_metadata =
            if file_type.is_symlink() { path.metadata().ok() } else { entry.metadata().ok() };
        Ok(Self::from_parts(path, entry.file_name(), file_type, followed_metadata, icons))
    }

    pub(crate) fn from_path(path: &Path, icons: &mut IconProvider) -> io::Result<Self> {
        let metadata = std::fs::symlink_metadata(path)?;
        let file_type = metadata.file_type();
        let followed_metadata =
            if file_type.is_symlink() { path.metadata().ok() } else { Some(metadata) };
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?
            .to_os_string();
        Ok(Self::from_parts(path.to_path_buf(), name, file_type, followed_metadata, icons))
    }

    fn from_parts(
        path: PathBuf,
        name: OsString,
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
        let display_name = display_filename(&name);
        let folded_name = display_name.to_lowercase().chars().collect();
        Self {
            name: display_name,
            name_os: name,
            folded_name,
            navigable,
            size: followed_metadata.as_ref().filter(|meta| meta.is_file()).map(|meta| meta.len()),
            modified: followed_metadata.as_ref().and_then(|meta| meta.modified().ok()),
            icon_path: icons.icon_for(&path, navigable),
            path,
            kind,
        }
    }

    pub(crate) fn set_name(&mut self, name: OsString) {
        self.name = display_filename(&name);
        self.folded_name = self.name.to_lowercase().chars().collect();
        self.name_os = name;
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

    /// The folded extension, for sorting by kind; empty when there is none.
    /// A leading dot is not an extension: `.bashrc` has none.
    pub(crate) fn folded_extension(&self) -> &[char] {
        let name = &self.folded_name[..];
        match name.iter().rposition(|character| *character == '.') {
            Some(dot) if dot > 0 => &name[dot + 1..],
            _ => &[],
        }
    }
}

#[derive(Debug)]
pub enum DirectoryUpdate {
    Batch(Vec<FileEntry>),
    Degraded { skipped: usize, examples: Vec<String> },
    Done,
    Error(String),
}

/// Enumerate a directory in bounded batches.
///
/// This follows Yazi's partial-update and ticketed directory-loading model:
/// https://github.com/sxyazi/yazi/blob/main/yazi-fs/src/op.rs
/// https://github.com/sxyazi/yazi/blob/main/yazi-fs/src/entries.rs
pub fn stream_directory(
    path: &Path,
    sender: Sender<DirectoryUpdate>,
    cancelled: Option<&AtomicBool>,
    order: SortOrder,
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
    let mut skipped = 0usize;
    let mut examples = Vec::new();
    for entry in reader {
        if cancelled.is_some_and(|cancelled| cancelled.load(AtomicOrdering::Acquire)) {
            return;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                record_degraded_entry(&mut skipped, &mut examples, None, &error);
                continue;
            }
        };
        let path = entry.path();
        let entry = match FileEntry::from_dir_entry(entry, &mut icons) {
            Ok(entry) => entry,
            Err(error) => {
                record_degraded_entry(&mut skipped, &mut examples, Some(&path), &error);
                continue;
            }
        };

        batch.push(entry);
        if batch.len() == DIRECTORY_BATCH_SIZE {
            sort_entries(&mut batch, order);
            if sender.send_blocking(DirectoryUpdate::Batch(std::mem::take(&mut batch))).is_err() {
                return;
            }
            batch = Vec::with_capacity(DIRECTORY_BATCH_SIZE);
        }
    }

    if !batch.is_empty() {
        sort_entries(&mut batch, order);
        if sender.send_blocking(DirectoryUpdate::Batch(batch)).is_err() {
            return;
        }
    }
    if skipped > 0 && sender.send_blocking(DirectoryUpdate::Degraded { skipped, examples }).is_err()
    {
        return;
    }
    let _ = sender.send_blocking(DirectoryUpdate::Done);
}

fn record_degraded_entry(
    skipped: &mut usize,
    examples: &mut Vec<String>,
    path: Option<&Path>,
    error: &io::Error,
) {
    const MAX_EXAMPLES: usize = 3;
    const MAX_DETAIL_CHARS: usize = 240;
    *skipped += 1;
    if examples.len() == MAX_EXAMPLES {
        return;
    }
    let detail = match path.and_then(Path::file_name) {
        Some(name) => format!("{}: {error}", display_filename(name)),
        None => error.to_string(),
    };
    examples.push(detail.chars().take(MAX_DETAIL_CHARS).collect());
}

/// What a listing is ordered by. Folders come first whatever the key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortKey {
    #[default]
    Name,
    Modified,
    Size,
    Kind,
}

impl SortKey {
    pub const ALL: [Self; 4] = [Self::Name, Self::Modified, Self::Size, Self::Kind];

    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Modified => "Modified",
            Self::Size => "Size",
            Self::Kind => "Kind",
        }
    }

    /// The name the state file records.
    pub fn name(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Modified => "modified",
            Self::Size => "size",
            Self::Kind => "kind",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|key| key.name() == name)
    }

    /// The direction a key starts in when first chosen: names read forwards,
    /// but the point of sorting by date or size is the newest or the largest.
    pub fn descends_first(self) -> bool {
        matches!(self, Self::Modified | Self::Size)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SortOrder {
    pub key: SortKey,
    pub descending: bool,
}

impl SortOrder {
    /// The order choosing `key` gives: its natural direction, or the reverse
    /// when it was already the key.
    pub fn choose(self, key: SortKey) -> Self {
        let descending = if self.key == key { !self.descending } else { key.descends_first() };
        Self { key, descending }
    }

    pub fn compare(self, a: &FileEntry, b: &FileEntry) -> Ordering {
        b.navigable.cmp(&a.navigable).then_with(|| {
            let ordering = match self.key {
                SortKey::Name => Ordering::Equal,
                SortKey::Modified => a.modified.cmp(&b.modified),
                SortKey::Size => a.size.cmp(&b.size),
                SortKey::Kind => a.folded_extension().cmp(b.folded_extension()),
            }
            .then_with(|| compare_names(a, b));
            if self.descending { ordering.reverse() } else { ordering }
        })
    }
}

/// Merge two listings each already in `order`.
pub fn merge_sorted_entries(
    left: Vec<FileEntry>,
    right: Vec<FileEntry>,
    order: SortOrder,
) -> Vec<FileEntry> {
    let mut left = left.into_iter().peekable();
    let mut right = right.into_iter().peekable();
    let mut merged = Vec::with_capacity(left.len() + right.len());

    while let (Some(a), Some(b)) = (left.peek(), right.peek()) {
        if order.compare(a, b).is_le() {
            merged.push(left.next().expect("peeked left entry"));
        } else {
            merged.push(right.next().expect("peeked right entry"));
        }
    }
    merged.extend(left);
    merged.extend(right);
    merged
}

pub(crate) fn sort_entries(entries: &mut [FileEntry], order: SortOrder) {
    entries.sort_by(|a, b| order.compare(a, b));
}

/// Case-insensitively by name, with the raw bytes breaking ties so two names
/// that fold alike keep one fixed order.
fn compare_names(a: &FileEntry, b: &FileEntry) -> Ordering {
    a.folded_name.cmp(&b.folded_name).then_with(|| a.name_os.cmp(&b.name_os))
}

pub fn display_filename(name: &OsStr) -> String {
    if let Some(name) = name.to_str() {
        if name.starts_with("⟦bytes:") || name.starts_with("⟦text:") {
            return format!("⟦text:{name}⟧");
        }
        return name.to_string();
    }

    use std::{fmt::Write as _, os::unix::ffi::OsStrExt as _};
    let bytes = name.as_bytes();
    let mut display = String::with_capacity(bytes.len() * 2 + 10);
    display.push_str("⟦bytes:");
    for byte in bytes {
        let _ = write!(display, "{byte:02x}");
    }
    display.push('⟧');
    display
}

/// A modification time as a list column shows it: the time alone for
/// today, day and month with the time for this year, and the date for
/// anything older. Short, and the most telling part is always there.
pub fn format_modified(time: SystemTime) -> String {
    format_modified_relative_to(time, chrono::Local::now())
}

fn format_modified_relative_to(time: SystemTime, now: chrono::DateTime<chrono::Local>) -> String {
    use chrono::Datelike as _;

    let time = chrono::DateTime::<chrono::Local>::from(time);
    if time.date_naive() == now.date_naive() {
        time.format("%H:%M").to_string()
    } else if time.year() == now.year() {
        time.format("%-d %b %H:%M").to_string()
    } else {
        time.format("%-d %b %Y").to_string()
    }
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
        let name_os = OsString::from(name);
        FileEntry {
            path: PathBuf::from(name),
            name: name.to_string(),
            name_os,
            folded_name: name.to_lowercase().chars().collect(),
            kind: if navigable { EntryKind::Directory } else { EntryKind::File },
            navigable,
            size: None,
            modified: None,
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
        sort_entries(&mut entries, SortOrder::default());

        assert_eq!(
            entries.iter().map(|entry| entry.name.as_str()).collect::<Vec<_>>(),
            ["Alpha", "beta", "A.txt", "z.txt"]
        );
    }

    #[test]
    fn merges_sorted_batches() {
        let left = vec![entry("Alpha", true), entry("b.txt", false)];
        let right = vec![entry("Beta", true), entry("a.txt", false)];

        let merged = merge_sorted_entries(left, right, SortOrder::default());

        assert_eq!(
            merged.iter().map(|entry| entry.name.as_str()).collect::<Vec<_>>(),
            ["Alpha", "Beta", "a.txt", "b.txt"]
        );
    }

    fn names(entries: &[FileEntry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.name.as_str()).collect()
    }

    #[test]
    fn every_key_keeps_folders_first_and_breaks_ties_by_name() {
        use std::time::Duration;

        let at = |seconds: u64| Some(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds));
        let mut entries = [
            FileEntry { size: Some(30), modified: at(3), ..entry("old.txt", false) },
            FileEntry { size: Some(10), modified: at(1), ..entry("Notes.md", false) },
            FileEntry { size: Some(10), modified: at(2), ..entry("archive.zip", false) },
            FileEntry { modified: at(9), ..entry("Folder", true) },
            FileEntry { modified: at(0), ..entry("bin", true) },
            FileEntry { modified: None, ..entry("broken", false) },
        ];

        sort_entries(&mut entries, SortOrder { key: SortKey::Modified, descending: true });
        assert_eq!(
            names(&entries),
            ["Folder", "bin", "old.txt", "archive.zip", "Notes.md", "broken"]
        );

        sort_entries(&mut entries, SortOrder { key: SortKey::Size, descending: true });
        assert_eq!(
            names(&entries),
            ["Folder", "bin", "old.txt", "Notes.md", "archive.zip", "broken"]
        );

        sort_entries(&mut entries, SortOrder { key: SortKey::Size, descending: false });
        assert_eq!(
            names(&entries),
            ["bin", "Folder", "broken", "archive.zip", "Notes.md", "old.txt"]
        );

        sort_entries(&mut entries, SortOrder { key: SortKey::Kind, descending: false });
        assert_eq!(
            names(&entries),
            ["bin", "Folder", "broken", "Notes.md", "old.txt", "archive.zip"]
        );

        sort_entries(&mut entries, SortOrder { key: SortKey::Name, descending: true });
        assert_eq!(
            names(&entries),
            ["Folder", "bin", "old.txt", "Notes.md", "broken", "archive.zip"]
        );
    }

    #[test]
    fn choosing_a_key_starts_in_its_natural_direction_and_repeats_to_reverse() {
        let order = SortOrder::default();
        assert_eq!(
            order.choose(SortKey::Modified),
            SortOrder { key: SortKey::Modified, descending: true }
        );
        assert_eq!(
            order.choose(SortKey::Modified).choose(SortKey::Modified),
            SortOrder { key: SortKey::Modified, descending: false }
        );
        assert_eq!(order.choose(SortKey::Name), SortOrder { key: SortKey::Name, descending: true });
        assert_eq!(
            order.choose(SortKey::Kind),
            SortOrder { key: SortKey::Kind, descending: false }
        );
        assert_eq!(SortKey::from_name(SortKey::Size.name()), Some(SortKey::Size));
    }

    #[test]
    fn a_leading_dot_is_not_an_extension() {
        assert_eq!(entry(".bashrc", false).folded_extension(), &[] as &[char]);
        assert_eq!(entry("Photo.JPG", false).folded_extension(), &['j', 'p', 'g']);
        assert_eq!(entry("archive.tar.gz", false).folded_extension(), &['g', 'z']);
        assert_eq!(entry("README", false).folded_extension(), &[] as &[char]);
    }

    #[test]
    fn formats_file_sizes() {
        assert_eq!(format_size(Some(42)), "42 B");
        assert_eq!(format_size(Some(1536)), "1.5 KiB");
        assert_eq!(format_size(None), "");
    }

    #[test]
    fn modification_times_shorten_with_nearness() {
        use chrono::TimeZone as _;

        let now = chrono::Local.with_ymd_and_hms(2026, 9, 17, 15, 30, 0).unwrap();
        let at = |year, month, day, hour| {
            SystemTime::from(chrono::Local.with_ymd_and_hms(year, month, day, hour, 5, 0).unwrap())
        };
        assert_eq!(format_modified_relative_to(at(2026, 9, 17, 9), now), "09:05");
        assert_eq!(format_modified_relative_to(at(2026, 3, 2, 9), now), "2 Mar 09:05");
        assert_eq!(format_modified_relative_to(at(2025, 12, 31, 23), now), "31 Dec 2025");
    }

    #[test]
    fn cancelled_directory_stream_publishes_nothing() {
        let cancelled = Arc::new(AtomicBool::new(true));
        let (sender, receiver) = async_channel::unbounded();

        stream_directory(Path::new("."), sender, Some(&cancelled), SortOrder::default());

        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn degraded_entry_details_are_counted_and_bounded() {
        let mut skipped = 0;
        let mut examples = Vec::new();
        let error = io::Error::new(io::ErrorKind::PermissionDenied, "x".repeat(500));
        for index in 0..10 {
            record_degraded_entry(
                &mut skipped,
                &mut examples,
                Some(Path::new(if index == 0 { "first" } else { "later" })),
                &error,
            );
        }

        assert_eq!(skipped, 10);
        assert_eq!(examples.len(), 3);
        assert!(examples.iter().all(|example| example.chars().count() <= 240));
    }

    #[test]
    fn invalid_utf8_names_keep_raw_identity_and_have_distinct_labels() {
        use std::{collections::HashSet, os::unix::ffi::OsStringExt as _};

        let root = tempfile::tempdir().unwrap();
        let mut icons = IconProvider::discover();
        let mut displays = HashSet::new();
        for byte in 0x80..=0xff {
            let raw = OsString::from_vec(vec![b'n', byte]);
            let path = root.path().join(&raw);
            std::fs::write(&path, b"x").unwrap();
            let entry = FileEntry::from_path(&path, &mut icons).unwrap();

            assert_eq!(entry.name_os, raw);
            assert!(displays.insert(entry.name));
        }
    }

    #[test]
    fn valid_names_cannot_impersonate_escaped_byte_labels() {
        let invalid_label = "⟦bytes:6eff⟧";
        assert_eq!(display_filename(OsStr::new(invalid_label)), "⟦text:⟦bytes:6eff⟧⟧");
    }
}
