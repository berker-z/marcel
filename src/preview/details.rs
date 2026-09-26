//! What Properties says about an item.
//!
//! Every filesystem object has the same common facts: what it is, how big,
//! where it lives, who owns it, when it changed. What its *kind* adds — an
//! image's dimensions, a PDF's page count, an archive's contents — is read
//! through the same loaders the preview pane uses, so the dialog and the pane
//! can never disagree about a file. Nothing here mutates anything.

use std::{
    fs,
    io::{self, BufReader, Read as _, Seek as _, SeekFrom},
    os::unix::fs::{FileTypeExt as _, MetadataExt as _},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use image::ImageReader;

use crate::{
    browse::entries::display_filename,
    fsops::{
        archive::{ArchiveBackend as _, SevenZipBackend, is_supported_archive},
        local::open_regular_file,
    },
};

use super::{
    MAX_TEXT_BYTES, audio, has_extension, is_image_extension, is_probably_text, media,
    pdf::inspect_pdf, read_up_to, thumbnails,
};

/// Where a folder measurement stops counting. A tree this size is `/` or the
/// Nix store, and "more than five million items" already answers the question
/// the dialog was asked.
pub const MAX_MEASURED_ENTRIES: u64 = 5_000_000;

/// How often a running measurement reports its running totals.
const MEASURE_PUBLISH_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObjectKind {
    Directory,
    File,
    /// `target` is what the link says, unresolved; it may not exist.
    Symlink {
        target: Option<PathBuf>,
    },
    /// A socket, FIFO, or device node, named as such.
    Other(&'static str),
}

/// What one kind of file adds to the common facts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Details {
    None,
    Image {
        width: u32,
        height: u32,
        format: String,
    },
    Pdf {
        pages: usize,
    },
    /// Lines in the first [`MAX_TEXT_BYTES`]; `truncated` when the file is
    /// longer than that and the count is a floor.
    Text {
        lines: usize,
        truncated: bool,
    },
    Audio {
        duration: Option<Duration>,
        codec: String,
        sample_rate: u32,
        channels: usize,
        title: Option<String>,
        artist: Option<String>,
        album: Option<String>,
    },
    Video {
        duration: Option<Duration>,
        width: Option<u32>,
        height: Option<u32>,
        codec: Option<String>,
        audio_codec: Option<String>,
    },
    Archive {
        entries: usize,
        expanded: u64,
    },
}

#[derive(Clone, Debug)]
pub struct ItemProperties {
    pub path: PathBuf,
    pub name: String,
    pub object: ObjectKind,
    /// "Folder", "PNG image", "Plain text", …
    pub kind: String,
    /// The MIME type `kind` was derived from, for a regular file.
    pub mime: Option<String>,
    /// A regular file's length. Folders are measured separately.
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    pub accessed: Option<SystemTime>,
    pub created: Option<SystemTime>,
    /// The permission bits, including setuid, setgid, and sticky.
    pub mode: u32,
    pub owner: String,
    pub group: String,
    /// Space left on the filesystem holding a folder.
    pub free_space: Option<u64>,
    pub details: Details,
}

/// Read everything Properties shows about one path.
pub fn inspect(path: &Path, cancelled: &Arc<AtomicBool>) -> io::Result<ItemProperties> {
    check_cancelled(cancelled)?;
    let metadata = fs::symlink_metadata(path)?;
    let file_type = metadata.file_type();
    let object = if file_type.is_dir() {
        ObjectKind::Directory
    } else if file_type.is_file() {
        ObjectKind::File
    } else if file_type.is_symlink() {
        ObjectKind::Symlink { target: fs::read_link(path).ok() }
    } else if file_type.is_socket() {
        ObjectKind::Other("Socket")
    } else if file_type.is_fifo() {
        ObjectKind::Other("FIFO")
    } else if file_type.is_block_device() {
        ObjectKind::Other("Block device")
    } else if file_type.is_char_device() {
        ObjectKind::Other("Character device")
    } else {
        ObjectKind::Other("Special file")
    };
    let name = path.file_name().map(display_filename).unwrap_or_else(|| path.display().to_string());
    let (kind, mime, details) = match &object {
        ObjectKind::Directory => ("Folder".to_string(), None, Details::None),
        ObjectKind::File => {
            let (mime, details) = inspect_file(path, cancelled)?;
            (kind_label(&mime), Some(mime), details)
        }
        ObjectKind::Symlink { .. } => ("Symbolic link".to_string(), None, Details::None),
        ObjectKind::Other(label) => (label.to_string(), None, Details::None),
    };
    let free_space = (object == ObjectKind::Directory)
        .then(|| rustix::fs::statvfs(path).ok())
        .flatten()
        .map(|space| space.f_bavail.saturating_mul(space.f_frsize));
    check_cancelled(cancelled)?;
    Ok(ItemProperties {
        path: path.to_path_buf(),
        name,
        size: (object == ObjectKind::File).then_some(metadata.len()),
        object,
        kind,
        mime,
        modified: metadata.modified().ok(),
        accessed: metadata.accessed().ok(),
        // Birth time needs `statx` and a filesystem that records it; the
        // dialog simply omits the row otherwise.
        created: metadata.created().ok(),
        mode: metadata.mode() & 0o7777,
        owner: user_name(metadata.uid()),
        group: group_name(metadata.gid()),
        free_space,
        details,
    })
}

/// The MIME type of a regular file and what its kind adds.
///
/// Content is sniffed before the name is consulted, as `load_preview` does,
/// so a renamed file is described by what it holds.
fn inspect_file(path: &Path, cancelled: &Arc<AtomicBool>) -> io::Result<(String, Details)> {
    // A FIFO with no writer blocks `open(2)` forever, which the cancellation
    // flag cannot interrupt; the opened descriptor is the only safe handle.
    let mut file = match open_regular_file(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
            return Ok(("application/octet-stream".to_string(), Details::None));
        }
        Err(error) => return Err(error),
    };
    let mut head = [0_u8; 8192];
    let head_read = read_up_to(&mut file, &mut head)?;
    let sniffed = infer::get(&head[..head_read]).map(|kind| kind.mime_type().to_string());
    let guessed = || mime_guess::from_path(path).first_raw().map(str::to_string);
    let fallback = |sniffed: Option<String>| {
        sniffed.or_else(guessed).unwrap_or_else(|| "application/octet-stream".to_string())
    };

    if is_supported_archive(path) {
        let details = SevenZipBackend::discover()
            .and_then(|backend| backend.list(path, cancelled.clone()))
            .map(|entries| Details::Archive {
                entries: entries.len(),
                expanded: entries.iter().map(|entry| entry.size).sum(),
            })
            .unwrap_or(Details::None);
        check_cancelled(cancelled)?;
        return Ok((fallback(sniffed), details));
    }

    if sniffed.as_deref().is_some_and(|mime| mime.starts_with("image/")) || is_image_extension(path)
    {
        // Dimensions come from the header alone; nothing is decoded.
        let mut mime = sniffed;
        let details = image_details(path)
            .map(|(width, height, format)| {
                if let Some(format) = format {
                    mime.get_or_insert_with(|| format.to_mime_type().to_string());
                }
                // A format decoded through a hook (HEIC and AVIF, by libheif)
                // has no `ImageFormat`, and its extension names it instead.
                let name = format
                    .and_then(|format| format.extensions_str().first().copied())
                    .or_else(|| path.extension().and_then(|extension| extension.to_str()));
                Details::Image { width, height, format: name.unwrap_or("").to_uppercase() }
            })
            .unwrap_or(Details::None);
        return Ok((fallback(mime), details));
    }

    if sniffed.as_deref() == Some("application/pdf") || has_extension(path, &["pdf"]) {
        let details = inspect_pdf(path, cancelled)
            .map(|document| Details::Pdf { pages: document.pages })
            .unwrap_or(Details::None);
        check_cancelled(cancelled)?;
        return Ok(("application/pdf".to_string(), details));
    }

    if sniffed.as_deref().is_some_and(|mime| mime.starts_with("audio/")) || audio::supports(path) {
        // Opening reads the headers and tags only; no samples are decoded.
        let details = audio::AudioSource::open(path)
            .map(|source| Details::Audio {
                duration: source.info.duration,
                codec: source.info.codec.clone(),
                sample_rate: source.info.sample_rate,
                channels: source.info.channels,
                title: source.info.title.clone(),
                artist: source.info.artist.clone(),
                album: source.info.album.clone(),
            })
            .unwrap_or(Details::None);
        check_cancelled(cancelled)?;
        return Ok((fallback(sniffed), details));
    }

    if sniffed.as_deref().is_some_and(|mime| mime.starts_with("video/"))
        || thumbnails::is_video(path)
    {
        let details = if media::available() {
            media::probe(path, cancelled)
                .map(|info| Details::Video {
                    duration: info.duration,
                    width: info.width,
                    height: info.height,
                    codec: info.codec,
                    audio_codec: info.audio_codec,
                })
                .unwrap_or(Details::None)
        } else {
            Details::None
        };
        check_cancelled(cancelled)?;
        return Ok((fallback(sniffed), details));
    }

    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.by_ref().take(MAX_TEXT_BYTES + 1).read_to_end(&mut bytes)?;
    let truncated = bytes.len() as u64 > MAX_TEXT_BYTES;
    bytes.truncate(MAX_TEXT_BYTES as usize);
    if sniffed.is_none() && is_probably_text(&bytes) {
        // The name may say which text this is; the content only says it is text.
        let mime = guessed()
            .filter(|mime| mime.starts_with("text/") || mime == "application/json")
            .unwrap_or_else(|| "text/plain".to_string());
        return Ok((mime, Details::Text { lines: count_lines(&bytes), truncated }));
    }
    Ok((fallback(sniffed), Details::None))
}

fn image_details(path: &Path) -> anyhow::Result<(u32, u32, Option<image::ImageFormat>)> {
    super::heif::register();
    let reader =
        ImageReader::new(BufReader::new(open_regular_file(path)?)).with_guessed_format()?;
    let format = reader.format();
    let (width, height) = reader.into_dimensions()?;
    Ok((width, height, format))
}

fn count_lines(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let newlines = bytes.iter().filter(|byte| **byte == b'\n').count();
    newlines + usize::from(bytes.last() != Some(&b'\n'))
}

/// A short name for a MIME type: "PNG image", "PDF document", "Plain text".
pub fn kind_label(mime: &str) -> String {
    let known = match mime {
        "application/pdf" => "PDF document",
        "application/zip" => "ZIP archive",
        "application/x-tar" => "Tar archive",
        "application/gzip" | "application/x-gzip" => "Gzip archive",
        "application/x-bzip2" => "Bzip2 archive",
        "application/x-xz" => "XZ archive",
        "application/zstd" => "Zstandard archive",
        "application/x-7z-compressed" => "7-Zip archive",
        "application/vnd.rar" | "application/x-rar-compressed" => "RAR archive",
        "application/json" => "JSON document",
        "application/octet-stream" => "Binary file",
        "image/x-icon" | "image/vnd.microsoft.icon" => "Icon image",
        "text/javascript" | "application/javascript" => "JavaScript source",
        "text/plain" => "Plain text",
        "text/markdown" => "Markdown document",
        "text/html" => "HTML document",
        _ => "",
    };
    if !known.is_empty() {
        return known.to_string();
    }
    match mime.split_once('/') {
        Some(("image", subtype)) => format!("{} image", subtype_word(subtype)),
        Some(("video", subtype)) => format!("{} video", subtype_word(subtype)),
        Some(("audio", subtype)) => format!("{} audio", subtype_word(subtype)),
        Some(("text", subtype)) => format!("{} text", subtype_word(subtype)),
        Some(("font", subtype)) => format!("{} font", subtype_word(subtype)),
        _ => mime.to_string(),
    }
}

/// "svg+xml" → "SVG", "x-icon" → "Icon", "javascript" → "Javascript".
fn subtype_word(subtype: &str) -> String {
    let word = subtype.split('+').next().unwrap_or(subtype);
    let word = word.strip_prefix("x-").or_else(|| word.strip_prefix("vnd.")).unwrap_or(word);
    if word.len() <= 4 {
        word.to_uppercase()
    } else {
        let mut characters = word.chars();
        match characters.next() {
            Some(first) => first.to_uppercase().chain(characters).collect(),
            None => String::new(),
        }
    }
}

/// `ls -l`'s view of the mode: a type character and nine permission bits.
pub fn symbolic_mode(object: &ObjectKind, mode: u32) -> String {
    let mut out = String::with_capacity(10);
    out.push(match object {
        ObjectKind::Directory => 'd',
        ObjectKind::File => '-',
        ObjectKind::Symlink { .. } => 'l',
        ObjectKind::Other("Socket") => 's',
        ObjectKind::Other("FIFO") => 'p',
        ObjectKind::Other("Block device") => 'b',
        ObjectKind::Other("Character device") => 'c',
        ObjectKind::Other(_) => '?',
    });
    for (shift, special, special_char) in [(6, 0o4000, 's'), (3, 0o2000, 's'), (0, 0o1000, 't')] {
        let bits = (mode >> shift) & 0o7;
        out.push(if bits & 0o4 != 0 { 'r' } else { '-' });
        out.push(if bits & 0o2 != 0 { 'w' } else { '-' });
        let executable = bits & 0o1 != 0;
        out.push(match (mode & special != 0, executable) {
            (true, true) => special_char,
            (true, false) => special_char.to_ascii_uppercase(),
            (false, true) => 'x',
            (false, false) => '-',
        });
    }
    out
}

/// The three permission classes, in the order the mode stores them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessClass {
    Owner,
    Group,
    Others,
}

impl AccessClass {
    pub const ALL: [Self; 3] = [Self::Owner, Self::Group, Self::Others];

    pub fn label(self) -> &'static str {
        match self {
            Self::Owner => "Owner",
            Self::Group => "Group",
            Self::Others => "Others",
        }
    }

    fn shift(self) -> u32 {
        match self {
            Self::Owner => 6,
            Self::Group => 3,
            Self::Others => 0,
        }
    }

    /// The mode bit `permission` (`0o4`, `0o2`, or `0o1`) is for this class.
    pub fn bit(self, permission: u32) -> u32 {
        permission << self.shift()
    }
}

/// What one class may do, in words: "read, write, execute", or "none".
pub fn describe_access(object: &ObjectKind, mode: u32, class: AccessClass) -> String {
    let bits = (mode >> class.shift()) & 0o7;
    let folder = *object == ObjectKind::Directory;
    let words = [
        (0o4, if folder { "list" } else { "read" }),
        (0o2, if folder { "create and delete" } else { "write" }),
        (0o1, if folder { "enter" } else { "execute" }),
    ]
    .into_iter()
    .filter(|(bit, _)| bits & bit != 0)
    .map(|(_, word)| word)
    .collect::<Vec<_>>();
    if words.is_empty() { "none".to_string() } else { words.join(", ") }
}

/// The name for a uid, or the number when no name is known.
///
/// This reads `/etc/passwd` directly rather than `getpwuid`, which would
/// need `libc` and `unsafe`. Accounts that only exist through NSS — LDAP,
/// systemd-homed — therefore show as numbers.
fn user_name(uid: u32) -> String {
    fs::read_to_string("/etc/passwd")
        .ok()
        .and_then(|database| name_in_database(&database, uid))
        .unwrap_or_else(|| uid.to_string())
}

fn group_name(gid: u32) -> String {
    fs::read_to_string("/etc/group")
        .ok()
        .and_then(|database| name_in_database(&database, gid))
        .unwrap_or_else(|| gid.to_string())
}

/// Both `passwd` and `group` put the name first and the id third.
fn name_in_database(database: &str, id: u32) -> Option<String> {
    database.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let found = fields.nth(1)?.parse::<u32>().ok()?;
        (found == id && !name.is_empty()).then(|| name.to_string())
    })
}

fn check_cancelled(cancelled: &AtomicBool) -> io::Result<()> {
    if cancelled.load(Ordering::Acquire) {
        Err(io::Error::new(io::ErrorKind::Interrupted, "properties inspection was cancelled"))
    } else {
        Ok(())
    }
}

/// Running totals of a measurement: what lies under the roots.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TreeTotals {
    pub folders: u64,
    pub files: u64,
    /// Symbolic links and special files, which have no size worth adding.
    pub other: u64,
    /// The lengths of the regular files, not their allocation on disk.
    pub bytes: u64,
    /// Directories that could not be read and entries that could not be
    /// inspected, so the user knows the totals are a floor.
    pub unreadable: u64,
    /// The walk ended: everything reachable was counted, or the cap was hit.
    pub finished: bool,
    pub capped: bool,
}

impl TreeTotals {
    pub fn items(&self) -> u64 {
        self.folders + self.files + self.other
    }
}

/// Count and size what the roots hold, without following symbolic links.
///
/// A directory root contributes its contents; anything else contributes
/// itself. Totals are published every [`MEASURE_PUBLISH_INTERVAL`] and once
/// more at the end, so a dialog can show them growing rather than waiting
/// on a large tree. Cancellation stops the walk without a final report.
pub fn measure_tree(
    roots: &[PathBuf],
    cancelled: &AtomicBool,
    mut publish: impl FnMut(TreeTotals),
) {
    let mut totals = TreeTotals::default();
    let mut pending = Vec::new();
    for root in roots {
        match fs::symlink_metadata(root) {
            Ok(metadata) if metadata.is_dir() => pending.push(root.clone()),
            Ok(metadata) => totals.count(&metadata),
            Err(_) => totals.unreadable += 1,
        }
    }
    let mut last_published = Instant::now();
    while let Some(directory) = pending.pop() {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => {
                totals.unreadable += 1;
                continue;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                totals.unreadable += 1;
                continue;
            };
            // `DirEntry::file_type` is free on Linux; the size still needs a
            // `stat`, so files take one and everything else does not.
            match entry.file_type() {
                Ok(file_type) if file_type.is_dir() => {
                    totals.folders += 1;
                    pending.push(entry.path());
                }
                Ok(file_type) if file_type.is_file() => match entry.metadata() {
                    Ok(metadata) => totals.count(&metadata),
                    Err(_) => totals.unreadable += 1,
                },
                Ok(_) => totals.other += 1,
                Err(_) => totals.unreadable += 1,
            }
            if totals.items() >= MAX_MEASURED_ENTRIES {
                totals.capped = true;
                totals.finished = true;
                publish(totals);
                return;
            }
        }
        if last_published.elapsed() >= MEASURE_PUBLISH_INTERVAL {
            publish(totals);
            last_published = Instant::now();
        }
    }
    totals.finished = true;
    publish(totals);
}

impl TreeTotals {
    fn count(&mut self, metadata: &fs::Metadata) {
        if metadata.is_dir() {
            self.folders += 1;
        } else if metadata.is_file() {
            self.files += 1;
            self.bytes = self.bytes.saturating_add(metadata.len());
        } else {
            self.other += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Sandbox, no_cancel};

    #[test]
    fn a_text_file_is_described_by_its_lines_and_named_by_its_extension() {
        let sandbox = Sandbox::new();
        let path = sandbox.file("notes.md", "# Title\n\nbody");
        let item = inspect(&path, &no_cancel()).unwrap();

        assert_eq!(item.object, ObjectKind::File);
        assert_eq!(item.name, "notes.md");
        assert_eq!(item.mime.as_deref(), Some("text/markdown"));
        assert_eq!(item.kind, "Markdown document");
        assert_eq!(item.size, Some(13));
        assert_eq!(item.details, Details::Text { lines: 3, truncated: false });
        assert!(item.modified.is_some());
        assert!(item.free_space.is_none(), "free space is a folder fact");
    }

    #[test]
    fn an_image_reports_its_dimensions_without_decoding_it() {
        let sandbox = Sandbox::new();
        let path = sandbox.path("photo.png");
        image::RgbaImage::new(37, 11).save(&path).unwrap();
        let item = inspect(&path, &no_cancel()).unwrap();

        assert_eq!(item.mime.as_deref(), Some("image/png"));
        assert_eq!(item.kind, "PNG image");
        assert_eq!(
            item.details,
            Details::Image { width: 37, height: 11, format: "PNG".to_string() }
        );
    }

    /// The size shown is the upright one, after the container's rotation.
    #[test]
    fn a_heic_reports_its_upright_dimensions() {
        let item = inspect(&crate::preview::fixture("rotated.heic"), &no_cancel()).unwrap();

        assert!(item.mime.as_deref().is_some_and(|mime| mime.starts_with("image/hei")));
        assert_eq!(
            item.details,
            Details::Image { width: 240, height: 320, format: "HEIC".to_string() }
        );
    }

    #[test]
    fn a_folder_and_a_link_have_no_type_details() {
        let sandbox = Sandbox::new();
        let folder = sandbox.dir("photos");
        let item = inspect(&folder, &no_cancel()).unwrap();
        assert_eq!(item.object, ObjectKind::Directory);
        assert_eq!(item.kind, "Folder");
        assert_eq!(item.size, None);
        assert!(item.free_space.is_some());

        let link = sandbox.path("shortcut");
        std::os::unix::fs::symlink("photos", &link).unwrap();
        let item = inspect(&link, &no_cancel()).unwrap();
        assert_eq!(item.object, ObjectKind::Symlink { target: Some(PathBuf::from("photos")) });
        assert_eq!(symbolic_mode(&item.object, item.mode).chars().next(), Some('l'));
    }

    #[test]
    fn modes_read_like_ls() {
        assert_eq!(symbolic_mode(&ObjectKind::File, 0o644), "-rw-r--r--");
        assert_eq!(symbolic_mode(&ObjectKind::Directory, 0o1777), "drwxrwxrwt");
        assert_eq!(symbolic_mode(&ObjectKind::File, 0o4755), "-rwsr-xr-x");
        assert_eq!(symbolic_mode(&ObjectKind::File, 0o4644), "-rwSr--r--");
        assert_eq!(describe_access(&ObjectKind::File, 0o640, AccessClass::Owner), "read, write");
        assert_eq!(describe_access(&ObjectKind::File, 0o640, AccessClass::Others), "none");
        assert_eq!(
            describe_access(&ObjectKind::Directory, 0o755, AccessClass::Group),
            "list, enter"
        );
    }

    #[test]
    fn kind_labels_are_short_and_readable() {
        assert_eq!(kind_label("image/jpeg"), "JPEG image");
        assert_eq!(kind_label("image/svg+xml"), "SVG image");
        assert_eq!(kind_label("image/x-icon"), "Icon image");
        assert_eq!(kind_label("text/x-python"), "Python text");
        assert_eq!(kind_label("application/pdf"), "PDF document");
        assert_eq!(kind_label("application/x-msdownload"), "application/x-msdownload");
    }

    #[test]
    fn names_come_from_the_third_field_of_a_database_line() {
        let database = "root:x:0:0:root:/root:/bin/sh\nberker:x:1000:100::/home/berker:/bin/fish\n";
        assert_eq!(name_in_database(database, 1000).as_deref(), Some("berker"));
        assert_eq!(name_in_database(database, 0).as_deref(), Some("root"));
        assert_eq!(name_in_database(database, 7), None);
        assert_eq!(name_in_database("malformed line\n::::\n", 0), None);
    }

    #[test]
    fn measuring_counts_folder_contents_and_file_roots_without_following_links() {
        let sandbox = Sandbox::new();
        sandbox.file("tree/a.txt", "12345");
        sandbox.file("tree/nested/b.txt", "1234567");
        sandbox.dir("tree/empty");
        std::os::unix::fs::symlink("/", sandbox.path("tree/escape")).unwrap();
        let loose = sandbox.file("loose.bin", "123");

        let mut reports = Vec::new();
        measure_tree(&[sandbox.path("tree"), loose], &AtomicBool::new(false), |totals| {
            reports.push(totals);
        });

        let last = *reports.last().unwrap();
        assert!(last.finished);
        assert!(!last.capped);
        assert_eq!((last.folders, last.files, last.other), (2, 3, 1));
        assert_eq!(last.bytes, 15);
        assert_eq!(last.unreadable, 0);
    }

    #[test]
    fn a_cancelled_measurement_never_reports_completion() {
        let sandbox = Sandbox::new();
        sandbox.file("tree/a.txt", "");
        let cancelled = AtomicBool::new(true);
        let mut reports = 0;
        measure_tree(&[sandbox.path("tree")], &cancelled, |_| reports += 1);
        assert_eq!(reports, 0);
    }
}
