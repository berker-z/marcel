//! What a file-chooser request asks for, independent of who asked.
//!
//! The portal backend in [`crate::file_chooser`] decodes the D-Bus dictionary
//! into one of these; the window shows it and answers with a
//! [`PickerResponse`]. Nothing here knows about zbus, so the picker can be
//! driven from a test — or from a future non-portal caller — without a bus.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use async_channel::{Receiver, Sender};
use globset::{Glob, GlobMatcher};

use crate::fs::FileEntry;

/// What kind of answer the caller is waiting for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PickerMode {
    /// One or more existing files.
    OpenFiles,
    /// One or more existing folders.
    OpenDirectories,
    /// A name in a folder, which may not exist yet.
    SaveFile,
    /// A folder to write these names into.
    SaveFiles { names: Vec<OsString> },
}

/// One pattern in a filter, as the caller expressed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FilterPattern {
    /// A shell-style glob on the file name, `*.png`.
    Glob(String),
    /// A MIME type, `image/png`, or a family, `image/*`.
    Mime(String),
}

/// A named set of patterns a file may match to stay in the listing.
///
/// Folders always show, or nothing could be navigated into; the filter only
/// decides which files appear alongside them.
#[derive(Clone, Debug)]
pub struct FileFilter {
    pub name: String,
    pub patterns: Vec<FilterPattern>,
    globs: Vec<GlobMatcher>,
    mimes: Vec<String>,
}

impl PartialEq for FileFilter {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.patterns == other.patterns
    }
}

impl Eq for FileFilter {}

impl FileFilter {
    pub fn new(name: String, patterns: Vec<FilterPattern>) -> Self {
        let mut globs = Vec::new();
        let mut mimes = Vec::new();
        for pattern in &patterns {
            match pattern {
                // Case-insensitive on purpose. Applications write `*.[jJ][pP][gG]`
                // because GTK matches case-sensitively, and a picker that hides
                // `Photo.JPG` behind `*.jpg` is wrong on a filesystem where the
                // user never chose the case.
                FilterPattern::Glob(glob) => {
                    if let Ok(glob) = Glob::new(glob) {
                        globs.push(glob.compile_matcher());
                    } else if let Ok(glob) = Glob::new(&globset::escape(glob)) {
                        globs.push(glob.compile_matcher());
                    }
                }
                FilterPattern::Mime(mime) => mimes.push(mime.to_ascii_lowercase()),
            }
        }
        Self {
            name,
            patterns,
            globs,
            mimes,
        }
    }

    /// Whether `entry` stays in a listing this filter applies to.
    pub fn matches(&self, entry: &FileEntry) -> bool {
        if entry.navigable {
            return true;
        }
        self.matches_name(&entry.name)
    }

    fn matches_name(&self, name: &str) -> bool {
        let folded = name.to_lowercase();
        if self.globs.iter().any(|glob| glob.is_match(&folded)) {
            return true;
        }
        if self.mimes.is_empty() {
            return false;
        }
        let guessed = mime_guess::from_path(name)
            .first_raw()
            .map(str::to_ascii_lowercase);
        let Some(guessed) = guessed else {
            return false;
        };
        self.mimes.iter().any(|mime| mime_matches(mime, &guessed))
    }
}

/// `image/*` accepts every image type; anything else must match exactly.
fn mime_matches(pattern: &str, mime: &str) -> bool {
    match pattern.strip_suffix("/*") {
        Some(family) => mime
            .split_once('/')
            .is_some_and(|(mime_family, _)| mime_family == family),
        None => pattern == mime,
    }
}

/// A request the picker window shows and answers.
pub struct PickerRequest {
    pub title: String,
    pub mode: PickerMode,
    pub multiple: bool,
    /// The confirm button's label, mnemonic already stripped.
    pub accept_label: Option<String>,
    /// Where to start; the home directory when `None` or unusable.
    pub start_directory: Option<PathBuf>,
    /// The name a save dialog starts with.
    pub current_name: Option<String>,
    pub filters: Vec<FileFilter>,
    /// Index into `filters` of the one to start with.
    pub current_filter: Option<usize>,
    /// Where the answer goes. Dropping it without sending is a cancel.
    pub reply: Sender<PickerResponse>,
    /// Fires when the caller withdrew the request; the window closes itself.
    pub closed: Receiver<()>,
}

impl PickerRequest {
    /// The folder the window opens at.
    pub fn initial_directory(&self, home: &Path) -> PathBuf {
        self.start_directory
            .as_deref()
            .filter(|path| path.is_absolute() && path.is_dir())
            .unwrap_or(home)
            .to_path_buf()
    }

    /// What the confirm button says when the caller did not say.
    pub fn default_accept_label(&self) -> &'static str {
        match self.mode {
            PickerMode::OpenFiles | PickerMode::OpenDirectories => "Open",
            PickerMode::SaveFile | PickerMode::SaveFiles { .. } => "Save",
        }
    }
}

/// What the window said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PickerResponse {
    /// The user confirmed these paths, with this filter active.
    Chosen {
        paths: Vec<PathBuf>,
        filter: Option<usize>,
    },
    /// The user dismissed the window.
    Cancelled,
    /// The caller withdrew the request before the user answered.
    Closed,
}

/// Turn a GTK-style `_Open` into `Open`; `__` is a literal underscore.
pub fn strip_mnemonic(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut chars = label.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '_' {
            if chars.peek() == Some(&'_') {
                chars.next();
                out.push('_');
            }
            continue;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::EntryKind;
    use std::sync::Arc;

    fn entry(name: &str, navigable: bool) -> FileEntry {
        FileEntry {
            path: PathBuf::from("/tmp").join(name),
            name: name.to_string(),
            name_os: OsString::from(name),
            folded_name: Arc::from(name.to_lowercase().chars().collect::<Vec<_>>()),
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
    fn globs_match_file_names_regardless_of_case_and_never_hide_folders() {
        let filter = FileFilter::new(
            "Images".to_string(),
            vec![
                FilterPattern::Glob("*.png".to_string()),
                FilterPattern::Glob("*.[jJ][pP][gG]".to_string()),
            ],
        );
        assert!(filter.matches(&entry("photo.png", false)));
        assert!(filter.matches(&entry("PHOTO.PNG", false)));
        assert!(filter.matches(&entry("photo.JPG", false)));
        assert!(!filter.matches(&entry("notes.txt", false)));
        assert!(filter.matches(&entry("notes.txt", true)));
    }

    #[test]
    fn mime_patterns_match_exact_types_and_families() {
        let filter = FileFilter::new(
            "Media".to_string(),
            vec![
                FilterPattern::Mime("image/*".to_string()),
                FilterPattern::Mime("application/pdf".to_string()),
            ],
        );
        assert!(filter.matches(&entry("photo.png", false)));
        assert!(filter.matches(&entry("scan.PDF", false)));
        assert!(!filter.matches(&entry("song.mp3", false)));
        assert!(!filter.matches(&entry("no-extension", false)));
    }

    #[test]
    fn a_filter_with_no_patterns_hides_every_file() {
        let filter = FileFilter::new("Nothing".to_string(), Vec::new());
        assert!(!filter.matches(&entry("anything", false)));
        assert!(filter.matches(&entry("folder", true)));
    }

    #[test]
    fn a_malformed_glob_is_taken_literally_instead_of_dropping_the_filter() {
        let filter = FileFilter::new(
            "Odd".to_string(),
            vec![FilterPattern::Glob("report[".to_string())],
        );
        assert!(filter.matches(&entry("report[", false)));
        assert!(!filter.matches(&entry("report", false)));
    }

    #[test]
    fn mnemonics_are_stripped_and_double_underscores_kept() {
        assert_eq!(strip_mnemonic("_Open"), "Open");
        assert_eq!(strip_mnemonic("Save _As"), "Save As");
        assert_eq!(strip_mnemonic("snake__case"), "snake_case");
        assert_eq!(strip_mnemonic("plain"), "plain");
    }

    #[test]
    fn the_initial_directory_falls_back_to_home() {
        let (reply, _receiver) = async_channel::bounded(1);
        let (_close, closed) = async_channel::bounded(1);
        let temp = tempfile::tempdir().unwrap();
        let mut request = PickerRequest {
            title: String::new(),
            mode: PickerMode::OpenFiles,
            multiple: false,
            accept_label: None,
            start_directory: Some(temp.path().join("missing")),
            current_name: None,
            filters: Vec::new(),
            current_filter: None,
            reply,
            closed,
        };
        assert_eq!(
            request.initial_directory(Path::new("/home/x")),
            PathBuf::from("/home/x")
        );
        request.start_directory = Some(temp.path().to_path_buf());
        assert_eq!(request.initial_directory(Path::new("/home/x")), temp.path());
        request.start_directory = Some(PathBuf::from("relative"));
        assert_eq!(
            request.initial_directory(Path::new("/home/x")),
            PathBuf::from("/home/x")
        );
    }
}
