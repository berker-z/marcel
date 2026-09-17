//! Archives, through a 7-Zip subprocess that never sees a path it could
//! misread as an option and never writes anywhere but a private staging
//! directory.

use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    io::{self, Read},
    os::unix::{fs::PermissionsExt as _, process::CommandExt as _},
    path::{Component, Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use super::local::PathContext as _;
use anyhow::{Context as _, Result, bail};
use rustix::process::{Pid, Signal, kill_process_group};

use super::local::{ensure_unoccupied, inspect, rename_no_replace};

pub const MAX_ARCHIVE_ENTRIES: usize = 100_000;
pub const MAX_EXPANDED_BYTES: u64 = 100 * 1024 * 1024 * 1024;
const MAX_SUBPROCESS_OUTPUT: usize = 256 * 1024;
const MAX_LISTING_OUTPUT: usize = 64 * 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const STAGING_MONITOR_INTERVAL: Duration = Duration::from_millis(200);

/// Extensions 7-Zip can open that Marcel offers to extract. RAR is separate:
/// its decoder is unfree, and the bundled build leaves it out unless asked.
const SUPPORTED_EXTENSIONS: &[&str] = &[
    "7z", "apk", "bz2", "bzip2", "cab", "cb7", "cbz", "cpio", "deb", "dmg", "gz", "gzip", "img",
    "iso", "jar", "lzma", "rpm", "squashfs", "tar", "tbz", "tbz2", "tgz", "txz", "vhd", "vhdx",
    "wim", "xar", "xz", "zip", "zst",
];
const RAR_EXTENSIONS: &[&str] = &["rar", "cbr"];
const COMPOUND_TAR_EXTENSIONS: &[&str] =
    &[".tar.bz2", ".tar.gz", ".tar.xz", ".tar.zst", ".tbz", ".tbz2", ".tgz", ".txz"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveEntry {
    pub path: PathBuf,
    pub size: u64,
    pub is_dir: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveOutcome {
    pub published: PathBuf,
}

pub trait ArchiveBackend {
    fn list(&self, archive: &Path, cancelled: Arc<AtomicBool>) -> Result<Vec<ArchiveEntry>>;

    fn extract(&self, archive: &Path, destination: &Path, cancelled: Arc<AtomicBool>)
    -> Result<()>;

    fn create_zip(
        &self,
        sources: &[PathBuf],
        destination: &Path,
        cancelled: Arc<AtomicBool>,
    ) -> Result<()>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SevenZipBackend {
    program: PathBuf,
}

impl SevenZipBackend {
    pub fn discover() -> Result<Self> {
        let current_exe = env::current_exe().context("Could not locate Marcel's executable")?;
        discover_with(env::var_os("MARCEL_7ZZ"), &current_exe, env::var_os("PATH"))
            .map(|program| Self { program })
    }

    pub fn from_program(program: PathBuf) -> Self {
        Self { program }
    }

    fn run<I, S>(
        &self,
        arguments: I,
        current_dir: Option<&Path>,
        monitored_directory: Option<&Path>,
        stdout_limit: usize,
        cancelled: Arc<AtomicBool>,
    ) -> Result<CommandOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        check_cancelled(&cancelled)?;

        let mut command = Command::new(&self.program);
        command
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        if let Some(current_dir) = current_dir {
            command.current_dir(current_dir);
        }

        let mut child = command.spawn().at("Could not start archive backend", &self.program)?;
        let stdout = child.stdout.take().context("Archive backend has no stdout")?;
        let stderr = child.stderr.take().context("Archive backend has no stderr")?;
        let stdout_thread = thread::spawn(move || read_bounded(stdout, stdout_limit));
        let stderr_thread = thread::spawn(move || read_bounded(stderr, MAX_SUBPROCESS_OUTPUT));
        let pid =
            Pid::from_raw(child.id() as i32).context("Archive backend returned invalid PID")?;

        // The child is polled rather than waited on, so cancellation and the
        // staging monitor can kill it mid-run.
        let mut last_monitor = Instant::now();
        let mut stopped_by = None;
        let status = loop {
            if cancelled.load(Ordering::Acquire) {
                stopped_by = Some(anyhow::anyhow!("Archive operation cancelled"));
            } else if last_monitor.elapsed() >= STAGING_MONITOR_INTERVAL {
                last_monitor = Instant::now();
                if let Some(directory) = monitored_directory
                    && let Err(error) = walk_staged(directory, true, &cancelled)
                {
                    stopped_by = Some(error);
                }
            }
            if stopped_by.is_some() {
                let _ = kill_process_group(pid, Signal::KILL);
                break child.wait().context("Could not reap stopped archive backend")?;
            }
            if let Some(status) =
                child.try_wait().context("Could not inspect archive backend status")?
            {
                break status;
            }
            thread::sleep(POLL_INTERVAL);
        };

        let stdout = stdout_thread
            .join()
            .map_err(|_| anyhow::anyhow!("Archive stdout reader panicked"))??;
        let stderr = stderr_thread
            .join()
            .map_err(|_| anyhow::anyhow!("Archive stderr reader panicked"))??;
        if let Some(error) = stopped_by {
            return Err(error);
        }
        Ok(CommandOutput {
            status,
            stdout: stdout.bytes,
            stdout_truncated: stdout.truncated,
            stderr: stderr.bytes,
        })
    }
}

impl ArchiveBackend for SevenZipBackend {
    fn list(&self, archive: &Path, cancelled: Arc<AtomicBool>) -> Result<Vec<ArchiveEntry>> {
        let arguments = [
            OsString::from("l"),
            OsString::from("-ba"),
            OsString::from("-slt"),
            OsString::from("-sccUTF-8"),
            OsString::from("-p"),
            OsString::from("--"),
            archive.as_os_str().to_owned(),
        ];
        let output = self.run(arguments, None, None, MAX_LISTING_OUTPUT, cancelled)?;
        output.require_success("inspect", archive)?;
        if output.stdout_truncated {
            bail!(
                "Archive listing exceeds Marcel's {} MiB safety limit",
                MAX_LISTING_OUTPUT / 1024 / 1024
            );
        }
        let stdout = String::from_utf8(output.stdout)
            .context("Archive backend returned non-UTF-8 listing output")?;
        parse_listing(&stdout)
    }

    fn extract(
        &self,
        archive: &Path,
        destination: &Path,
        cancelled: Arc<AtomicBool>,
    ) -> Result<()> {
        let mut output_directory = OsString::from("-o");
        output_directory.push(destination);
        let arguments = [
            OsString::from("x"),
            OsString::from("-y"),
            OsString::from("-aos"),
            OsString::from("-sccUTF-8"),
            OsString::from("-p"),
            output_directory,
            OsString::from("--"),
            archive.as_os_str().to_owned(),
        ];
        let output =
            self.run(arguments, None, Some(destination), MAX_SUBPROCESS_OUTPUT, cancelled)?;
        output.require_success("extract", archive)
    }

    fn create_zip(
        &self,
        sources: &[PathBuf],
        destination: &Path,
        cancelled: Arc<AtomicBool>,
    ) -> Result<()> {
        let parent = shared_parent(sources)?;
        let mut arguments = vec![
            OsString::from("a"),
            OsString::from("-tzip"),
            OsString::from("-mx=5"),
            OsString::from("-y"),
            OsString::from("-sccUTF-8"),
            destination.as_os_str().to_owned(),
            OsString::from("--"),
        ];
        for source in sources {
            arguments
                .push(source.file_name().context("Compression source has no filename")?.to_owned());
        }
        let output = self.run(arguments, Some(parent), None, MAX_SUBPROCESS_OUTPUT, cancelled)?;
        output.require_success("compress", destination)
    }
}

/// The one directory every source lives in, which is where the archive is
/// built from so entries carry bare names.
fn shared_parent(sources: &[PathBuf]) -> Result<&Path> {
    let parent = sources
        .first()
        .context("Select at least one item to compress")?
        .parent()
        .context("Compression source has no parent")?;
    if sources.iter().any(|source| source.parent() != Some(parent)) {
        bail!("Compression sources must share one directory");
    }
    Ok(parent)
}

pub fn extract_archive(archive: &Path, cancelled: Arc<AtomicBool>) -> Result<ArchiveOutcome> {
    extract_archive_with(&SevenZipBackend::discover()?, archive, cancelled)
}

pub fn create_zip_archive(
    sources: &[PathBuf],
    destination: &Path,
    cancelled: Arc<AtomicBool>,
) -> Result<ArchiveOutcome> {
    create_zip_archive_with(&SevenZipBackend::discover()?, sources, destination, cancelled)
}

fn extract_archive_with<B: ArchiveBackend>(
    backend: &B,
    archive: &Path,
    cancelled: Arc<AtomicBool>,
) -> Result<ArchiveOutcome> {
    let parent = archive.parent().context("Archive has no containing directory")?;
    let extract_to_staging = |archive: &Path| -> Result<(tempfile::TempDir, Vec<StagedEntry>)> {
        validate_preflight(&backend.list(archive, cancelled.clone())?)?;
        let staging = archive_staging(parent)?;
        backend.extract(archive, staging.path(), cancelled.clone())?;
        check_cancelled(&cancelled)?;
        let staged = walk_staged(staging.path(), false, &cancelled)?;
        Ok((staging, staged))
    };

    let (mut staging, staged) = extract_to_staging(archive)?;
    // A `.tar.gz` unwraps to one `.tar`; the user wants what is inside that.
    if let [only] = staged.as_slice()
        && only.is_file
        && only.path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("tar"))
        && is_compound_tar_archive(archive)
    {
        staging = extract_to_staging(&only.path)?.0;
    }

    let top_level = fs::read_dir(staging.path())
        .at("Could not inspect archive staging", staging.path())?
        .map(|entry| {
            entry.map(|entry| entry.path()).context("Could not read archive staging entry")
        })
        .collect::<Result<Vec<_>>>()?;
    let published = match top_level.as_slice() {
        [] => bail!("Archive contains no extractable entries"),
        [source] => {
            let destination =
                parent.join(source.file_name().context("Extracted item has no filename")?);
            ensure_unoccupied(&destination)?;
            rename_no_replace(source, &destination)
                .at("Could not publish extracted item", &destination)?;
            destination
        }
        _ => {
            let destination = parent.join(archive_stem(archive));
            ensure_unoccupied(&destination)?;
            let source = staging.keep();
            if let Err(error) = rename_no_replace(&source, &destination) {
                let message = format!(
                    "Could not publish extracted directory “{}”: {error}",
                    destination.display()
                );
                return Err(match fs::remove_dir_all(&source) {
                    Ok(()) => anyhow::anyhow!(message),
                    Err(cleanup) => {
                        anyhow::anyhow!("{message}; staging cleanup also failed: {cleanup}")
                    }
                });
            }
            destination
        }
    };
    Ok(ArchiveOutcome { published })
}

fn create_zip_archive_with<B: ArchiveBackend>(
    backend: &B,
    sources: &[PathBuf],
    destination: &Path,
    cancelled: Arc<AtomicBool>,
) -> Result<ArchiveOutcome> {
    if !destination
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
    {
        bail!("The first archive version creates ZIP files only");
    }
    ensure_unoccupied(destination)?;
    validate_compression_sources(sources, &cancelled)?;
    let parent = destination.parent().context("Archive destination has no containing directory")?;
    let staging = archive_staging(parent)?;
    let staged_archive = staging.path().join("archive.zip");
    backend.create_zip(sources, &staged_archive, cancelled.clone())?;
    check_cancelled(&cancelled)?;
    if !staged_archive.is_file() {
        bail!("Archive backend reported success without creating a ZIP file");
    }
    validate_preflight(&backend.list(&staged_archive, cancelled)?)?;
    ensure_unoccupied(destination)?;
    rename_no_replace(&staged_archive, destination).at("Could not publish ZIP", destination)?;
    Ok(ArchiveOutcome { published: destination.to_path_buf() })
}

fn archive_staging(parent: &Path) -> Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix(".marcel-archive-")
        .tempdir_in(parent)
        .at("Could not create private archive staging in", parent)
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Acquire) {
        bail!("Archive operation cancelled");
    }
    Ok(())
}

/// Running totals for a listing or a staged tree, refused past the limits.
struct Budget {
    entries: usize,
    bytes: u64,
    /// How the archive is described in a refusal: what it "contains" and
    /// "expands" to by its listing, or what it "extracted".
    listed: bool,
}

impl Budget {
    fn listing() -> Self {
        Self { entries: 0, bytes: 0, listed: true }
    }

    fn staged() -> Self {
        Self { entries: 0, bytes: 0, listed: false }
    }

    fn add(&mut self, bytes: u64) -> Result<()> {
        let (entries_verb, bytes_verb) =
            if self.listed { ("contains", "expands") } else { ("extracted", "extracted") };
        self.entries += 1;
        self.bytes = self.bytes.checked_add(bytes).context("Archive size overflowed")?;
        if self.entries > MAX_ARCHIVE_ENTRIES {
            bail!("Archive {entries_verb} more than {MAX_ARCHIVE_ENTRIES} entries");
        }
        if self.bytes > MAX_EXPANDED_BYTES {
            bail!(
                "Archive {bytes_verb} beyond Marcel's {} GiB safety limit",
                MAX_EXPANDED_BYTES / 1024 / 1024 / 1024
            );
        }
        Ok(())
    }
}
fn validate_preflight(entries: &[ArchiveEntry]) -> Result<()> {
    if entries.is_empty() {
        bail!("Archive contains no extractable entries");
    }
    let mut budget = Budget::listing();
    for entry in entries {
        budget.add(entry.size)?;
        validate_archive_path(&entry.path.to_string_lossy())?;
    }
    Ok(())
}

#[derive(Debug)]
struct StagedEntry {
    path: PathBuf,
    is_file: bool,
}

/// Walk what the backend has written into staging, refusing anything Marcel
/// would not publish.
///
/// Runs twice per extraction: periodically while the backend is still writing
/// (`tolerate_missing`, since entries come and go under it) and once more when
/// it has finished, so a dishonest listing cannot get a tree past the limits
/// that the listing itself would have failed.
fn walk_staged(
    root: &Path,
    tolerate_missing: bool,
    cancelled: &AtomicBool,
) -> Result<Vec<StagedEntry>> {
    let vanished = |error: &io::Error| tolerate_missing && error.kind() == io::ErrorKind::NotFound;
    let mut entries = Vec::new();
    let mut budget = Budget::staged();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        check_cancelled(cancelled)?;
        let listing = match fs::read_dir(&directory) {
            Ok(listing) => listing,
            Err(error) if vanished(&error) => continue,
            Err(error) => {
                return Err(error).at("Could not inspect staging", &directory);
            }
        };
        for entry in listing {
            check_cancelled(cancelled)?;
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if vanished(&error) => continue,
                Err(error) => return Err(error).context("Could not read archive staging entry"),
            };
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if vanished(&error) => continue,
                Err(error) => {
                    return Err(error).at("Could not inspect", &path);
                }
            };
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                bail!("Archive extracted an unsupported symbolic link “{}”", path.display());
            }
            let is_file = file_type.is_file();
            if file_type.is_dir() {
                pending.push(path.clone());
            } else if !is_file {
                bail!("Archive extracted unsupported special entry “{}”", path.display());
            }
            budget.add(if is_file { metadata.len() } else { 0 })?;
            entries.push(StagedEntry { path, is_file });
        }
    }
    if entries.is_empty() && !tolerate_missing {
        bail!("Archive contains no extractable entries");
    }
    Ok(entries)
}

fn validate_compression_sources(sources: &[PathBuf], cancelled: &AtomicBool) -> Result<()> {
    shared_parent(sources)?;
    let mut count = 0_usize;
    let mut pending = sources.to_vec();
    while let Some(path) = pending.pop() {
        check_cancelled(cancelled)?;
        let metadata = inspect(&path)?;
        if metadata.file_type().is_symlink() {
            bail!(
                "Symbolic links are not supported by the first archive version: “{}”",
                path.display()
            );
        }
        if metadata.is_dir() {
            for child in fs::read_dir(&path).at("Could not read", &path)? {
                pending.push(child.context("Could not read compression source")?.path());
            }
        } else if !metadata.is_file() {
            bail!(
                "Special files are not supported by the first archive version: “{}”",
                path.display()
            );
        }
        count += 1;
        if count > MAX_ARCHIVE_ENTRIES {
            bail!("Selection contains more than {MAX_ARCHIVE_ENTRIES} entries");
        }
    }
    Ok(())
}

fn lowercase_name(path: &Path) -> String {
    path.file_name().map(|name| name.to_string_lossy().to_ascii_lowercase()).unwrap_or_default()
}

fn is_compound_tar_archive(path: &Path) -> bool {
    let name = lowercase_name(path);
    COMPOUND_TAR_EXTENSIONS.iter().any(|extension| name.ends_with(extension))
}

struct CommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stdout_truncated: bool,
    stderr: Vec<u8>,
}

impl CommandOutput {
    fn require_success(&self, action: &str, path: &Path) -> Result<()> {
        if self.status.success() {
            return Ok(());
        }
        let bytes = if self.stderr.is_empty() { &self.stdout } else { &self.stderr };
        let text = String::from_utf8_lossy(bytes);
        let diagnostics = match text.trim() {
            "" => "no diagnostics",
            trimmed => trimmed,
        };
        bail!(
            "Could not {action} “{}” (archive backend exited with {}): {diagnostics}",
            path.display(),
            self.status
        );
    }
}

struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

fn read_bounded(mut reader: impl Read, limit: usize) -> io::Result<CapturedOutput> {
    let mut retained = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(retained.len());
        retained.extend_from_slice(&buffer[..read.min(remaining)]);
        truncated |= read > remaining;
    }
    Ok(CapturedOutput { bytes: retained, truncated })
}

fn discover_with(
    override_program: Option<OsString>,
    current_exe: &Path,
    path: Option<OsString>,
) -> Result<PathBuf> {
    if let Some(program) = override_program.filter(|program| !program.is_empty()) {
        let program = PathBuf::from(program);
        if !is_executable(&program) {
            bail!(
                "MARCEL_7ZZ points to an unavailable program “{}”: file does not exist or is not executable",
                program.display()
            );
        }
        return Ok(program);
    }

    if let Some(prefix) = current_exe.parent().and_then(Path::parent) {
        let private = prefix.join("libexec/marcel/7zz");
        if is_executable(&private) {
            return Ok(private);
        }
    }

    ["7zz", "7z"]
        .into_iter()
        .find_map(|name| find_on_path(name, path.as_deref()))
        .context(
            "Archive support is unavailable because 7-Zip was not found; install `7zz` or set MARCEL_7ZZ",
        )
}

fn find_on_path(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    env::split_paths(path?).find_map(|directory| {
        let candidate = directory.join(name);
        is_executable(&candidate).then_some(candidate)
    })
}
fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

pub fn is_supported_archive(path: &Path) -> bool {
    is_supported_archive_with(path, rar_support_enabled())
}

fn is_supported_archive_with(path: &Path, rar_supported: bool) -> bool {
    let name = lowercase_name(path);
    let has = |extensions: &[&str]| {
        extensions.iter().any(|extension| {
            name.len() > extension.len() && name.ends_with(&format!(".{extension}"))
        })
    };
    has(SUPPORTED_EXTENSIONS) || (rar_supported && has(RAR_EXTENSIONS))
}

fn rar_support_enabled() -> bool {
    env::var("MARCEL_ENABLE_RAR").is_ok_and(|value| {
        matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    })
}

pub fn default_zip_name(sources: &[PathBuf], single_is_directory: bool) -> String {
    if let [source] = sources {
        let stem = if single_is_directory { source.file_name() } else { source.file_stem() }
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();
        format!("{stem}.zip")
    } else {
        "Archive.zip".to_string()
    }
}

/// The archive's name with every archive extension peeled off, so
/// `backup.tar.gz` extracts into `backup`.
pub fn archive_stem(path: &Path) -> String {
    let mut name =
        path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    while let Some((stem, extension)) = name.rsplit_once('.') {
        let known = SUPPORTED_EXTENSIONS
            .iter()
            .chain(RAR_EXTENSIONS)
            .any(|candidate| extension.eq_ignore_ascii_case(candidate));
        if stem.is_empty() || !known {
            break;
        }
        name.truncate(stem.len());
    }
    if name.is_empty() { "Archive".to_string() } else { name }
}

fn parse_listing(output: &str) -> Result<Vec<ArchiveEntry>> {
    let mut entries = Vec::new();
    let mut budget = Budget::listing();
    for block in output.split("\n\n") {
        let mut path = None;
        let mut size = None;
        let mut is_dir = false;
        let mut is_link = false;
        let mut is_encrypted = false;
        for line in block.lines() {
            let Some((key, value)) = line.split_once(" = ") else {
                continue;
            };
            match key {
                "Path" => path = Some(value),
                "Size" => {
                    size = Some(
                        value
                            .parse::<u64>()
                            .with_context(|| format!("Invalid archive entry size “{value}”"))?,
                    );
                }
                "Folder" => is_dir = value == "+",
                "Attributes" => {
                    is_dir |= value.starts_with('D');
                    is_link |= value.contains('L');
                }
                "Symbolic Link" | "Hard Link" => is_link |= !value.is_empty(),
                "Encrypted" => is_encrypted = value == "+",
                _ => {}
            }
        }
        let (Some(path), Some(size)) = (path, size) else {
            continue;
        };
        validate_archive_path(path)?;
        if is_link {
            bail!("Archive contains unsupported link entry “{path}”");
        }
        if is_encrypted {
            bail!("Password-protected archives are not supported yet");
        }
        budget.add(size)?;
        entries.push(ArchiveEntry { path: PathBuf::from(path), size, is_dir });
    }
    if entries.is_empty() {
        bail!("Archive contains no extractable entries");
    }
    Ok(entries)
}

fn validate_archive_path(value: &str) -> Result<()> {
    let unsafe_shape = value.is_empty()
        || value.starts_with(['/', '\\'])
        || value.contains('\\')
        || value
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
        || value.as_bytes().get(1) == Some(&b':')
        || Path::new(value)
            .components()
            .any(|component| !matches!(component, Component::Normal(name) if !name.is_empty()));
    if unsafe_shape {
        bail!("Archive contains unsafe path “{value}”");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Sandbox, no_cancel, read};

    /// What the fake backend leaves in the staging directory.
    #[derive(Clone, Copy)]
    enum Fake {
        One,
        Multiple,
        Symlink,
        Empty,
        OversizedSparse,
    }

    impl ArchiveBackend for Fake {
        fn list(&self, _archive: &Path, _cancelled: Arc<AtomicBool>) -> Result<Vec<ArchiveEntry>> {
            Ok(vec![ArchiveEntry { path: PathBuf::from("file.txt"), size: 4, is_dir: false }])
        }

        fn extract(
            &self,
            _archive: &Path,
            destination: &Path,
            _cancelled: Arc<AtomicBool>,
        ) -> Result<()> {
            match self {
                Fake::One => fs::write(destination.join("file.txt"), b"test")?,
                Fake::Multiple => {
                    fs::write(destination.join("one.txt"), b"one")?;
                    fs::write(destination.join("two.txt"), b"two")?;
                }
                Fake::Symlink => {
                    std::os::unix::fs::symlink("../outside", destination.join("link"))?;
                }
                Fake::Empty => {}
                Fake::OversizedSparse => {
                    let file = fs::File::create(destination.join("dishonest.bin"))?;
                    file.set_len(MAX_EXPANDED_BYTES + 1)?;
                }
            }
            Ok(())
        }

        fn create_zip(
            &self,
            _sources: &[PathBuf],
            destination: &Path,
            _cancelled: Arc<AtomicBool>,
        ) -> Result<()> {
            fs::write(destination, b"fake zip")?;
            Ok(())
        }
    }

    fn extract(fake: Fake, archive: &Path) -> Result<ArchiveOutcome> {
        extract_archive_with(&fake, archive, no_cancel())
    }

    /// The real 7-Zip, when the environment has one.
    fn official_backend() -> Option<(SevenZipBackend, PathBuf)> {
        let program = find_on_path("7zz", env::var_os("PATH").as_deref())?;
        Some((SevenZipBackend::from_program(program.clone()), program))
    }

    /// Have 7-Zip itself build `archive` of `kind` from `member`, then
    /// remove the member so extraction has to recreate it.
    fn seven_zip_pack(program: &Path, sandbox: &Sandbox, kind: &str, archive: &str, member: &str) {
        let status = Command::new(program)
            .current_dir(sandbox.root())
            .args(["a", &format!("-t{kind}"), archive, member])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let member = sandbox.path(member);
        if member.is_dir() {
            fs::remove_dir_all(member).unwrap();
        } else {
            fs::remove_file(member).unwrap();
        }
    }

    #[test]
    fn discovery_order_is_override_private_7zz_then_7z() {
        let sandbox = Sandbox::new();
        let override_program = sandbox.script("override", "exit 0");
        let current_exe = sandbox.path("prefix/bin/marcel-rs");
        let private = sandbox.script("prefix/libexec/marcel/7zz", "exit 0");
        let path_dir = sandbox.dir("path");
        sandbox.script("path/7zz", "exit 0");
        sandbox.script("path/7z", "exit 0");
        let path = env::join_paths([&path_dir]).unwrap();
        let discover = |override_program: Option<&Path>| {
            discover_with(
                override_program.map(|program| program.as_os_str().to_owned()),
                &current_exe,
                Some(path.clone()),
            )
            .unwrap()
        };

        assert_eq!(discover(Some(&override_program)), override_program);
        assert_eq!(discover(None), private);
        fs::remove_file(&private).unwrap();
        assert_eq!(discover(None), path_dir.join("7zz"));
        fs::remove_file(path_dir.join("7zz")).unwrap();
        assert_eq!(discover(None), path_dir.join("7z"));
    }

    #[test]
    fn listing_parser_accepts_regular_entries_and_rejects_escapes_and_links() {
        let listing = "\
Path = folder
Size = 0
Folder = +

Path = folder/file.txt
Size = 12
Folder = -
";
        assert_eq!(
            parse_listing(listing).unwrap(),
            vec![
                ArchiveEntry { path: PathBuf::from("folder"), size: 0, is_dir: true },
                ArchiveEntry { path: PathBuf::from("folder/file.txt"), size: 12, is_dir: false },
            ]
        );

        for path in [
            "../escape",
            "folder/../escape",
            "folder/./file",
            "folder//file",
            "/absolute",
            r"C:\\escape",
            r"folder\\escape",
        ] {
            let listing = format!("Path = {path}\nSize = 1\nFolder = -\n");
            assert!(parse_listing(&listing).is_err(), "{path:?} was accepted");
        }
        assert!(parse_listing("Path = link\nSize = 0\nSymbolic Link = ../target\n").is_err());
        assert!(
            parse_listing("Path = secret.txt\nSize = 1\nEncrypted = +\n")
                .unwrap_err()
                .to_string()
                .contains("Password-protected")
        );
    }

    #[test]
    fn archive_names_cover_compound_extensions() {
        assert_eq!(archive_stem(Path::new("backup.tar.gz")), "backup");
        assert_eq!(archive_stem(Path::new("photos.zip")), "photos");
        assert_eq!(default_zip_name(&[PathBuf::from("/tmp/report.pdf")], false), "report.zip");
        assert_eq!(
            default_zip_name(&[PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")], false),
            "Archive.zip"
        );
        assert!(is_supported_archive(Path::new("BOOKS.CBZ")));
        assert!(!is_supported_archive(Path::new("notes.txt")));
        for name in ["books.cbr", "archive.rar"] {
            assert!(!is_supported_archive_with(Path::new(name), false));
            assert!(is_supported_archive_with(Path::new(name), true));
        }
    }

    #[test]
    fn explicit_missing_override_is_not_silently_ignored() {
        let error = discover_with(
            Some(OsString::from("/definitely/missing/7zz")),
            Path::new("/prefix/bin/marcel-rs"),
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("MARCEL_7ZZ"));
    }

    #[test]
    fn subprocess_capture_is_bounded_and_reports_truncation() {
        let capture = read_bounded(io::Cursor::new(b"0123456789"), 4).unwrap();
        assert_eq!(capture.bytes, b"0123");
        assert!(capture.truncated);
    }

    #[test]
    fn extraction_tidies_one_or_multiple_top_level_items_without_overwrite() {
        let sandbox = Sandbox::new();
        let archive = sandbox.file("bundle.zip", b"archive");

        let outcome = extract(Fake::One, &archive).unwrap();
        assert_eq!(outcome.published, sandbox.path("file.txt"));
        assert_eq!(read(&outcome.published), b"test");

        fs::remove_file(&outcome.published).unwrap();
        let outcome = extract(Fake::Multiple, &archive).unwrap();
        assert_eq!(outcome.published, sandbox.path("bundle"));
        assert_eq!(read(outcome.published.join("one.txt")), b"one");

        fs::remove_dir_all(&outcome.published).unwrap();
        let occupied = sandbox.file("file.txt", b"occupied");
        assert!(extract(Fake::One, &archive).is_err());
        assert_eq!(read(occupied), b"occupied");
    }

    #[test]
    fn unsafe_extractions_are_rejected_and_staging_is_cleaned() {
        let sandbox = Sandbox::new();
        let archive = sandbox.file("unsafe.zip", b"archive");

        let error = extract(Fake::Symlink, &archive).unwrap_err();
        assert!(error.to_string().contains("symbolic link"));
        for fake in [Fake::Empty, Fake::OversizedSparse] {
            assert!(extract(fake, &archive).is_err());
        }
        assert_eq!(sandbox.names(""), ["unsafe.zip"], "staging must be cleaned up");
    }

    #[test]
    fn listing_limits_entry_count_and_declared_expanded_size() {
        let oversized = format!("Path = huge.bin\nSize = {}\nFolder = -\n", MAX_EXPANDED_BYTES + 1);
        assert!(parse_listing(&oversized).is_err());

        let mut too_many = String::new();
        for index in 0..=MAX_ARCHIVE_ENTRIES {
            use std::fmt::Write as _;
            writeln!(too_many, "Path = {index}\nSize = 0\nFolder = -\n").unwrap();
        }
        assert!(parse_listing(&too_many).is_err());
    }

    #[test]
    fn compression_stages_validates_and_publishes_without_overwrite() {
        let sandbox = Sandbox::new();
        let source = sandbox.file("report.txt", b"report");
        let destination = sandbox.path("report.zip");
        let create = || {
            create_zip_archive_with(
                &Fake::One,
                std::slice::from_ref(&source),
                &destination,
                no_cancel(),
            )
        };

        let outcome = create().unwrap();
        assert_eq!(outcome.published, destination);
        assert_eq!(read(&destination), b"fake zip");

        let error = create().unwrap_err();
        assert!(error.to_string().contains("already exists"));
        assert_eq!(read(&destination), b"fake zip");
    }

    #[test]
    fn cancellation_terminates_the_archive_process_group() {
        let sandbox = Sandbox::new();
        let backend = SevenZipBackend::from_program(sandbox.script("slow-7zz", "sleep 30"));
        let cancelled = no_cancel();
        let cancel_for_thread = cancelled.clone();
        let trigger = thread::spawn(move || {
            thread::sleep(Duration::from_millis(60));
            cancel_for_thread.store(true, Ordering::Release);
        });

        let error = backend.list(Path::new("unused.zip"), cancelled).unwrap_err();
        trigger.join().unwrap();
        assert!(error.to_string().contains("cancelled"));
    }

    #[test]
    fn official_backend_round_trips_zip_when_available() {
        let Some((backend, _)) = official_backend() else {
            return;
        };
        let sandbox = Sandbox::new();
        let source = sandbox.file("report.txt", b"round trip");
        let archive = sandbox.path("report.zip");

        create_zip_archive_with(&backend, std::slice::from_ref(&source), &archive, no_cancel())
            .unwrap();
        fs::remove_file(&source).unwrap();
        let outcome = extract_archive_with(&backend, &archive, no_cancel()).unwrap();

        assert_eq!(outcome.published, source);
        assert_eq!(read(outcome.published), b"round trip");
    }

    /// A `.tar.gz` unwraps one layer, to the tar's members; a native `.7z`
    /// extracts directly.
    #[test]
    fn official_backend_extracts_compound_tar_and_native_7z_when_available() {
        let Some((backend, program)) = official_backend() else {
            return;
        };
        let sandbox = Sandbox::new();
        let payload = sandbox.file("payload.txt", b"compound tar");
        seven_zip_pack(&program, &sandbox, "tar", "bundle.tar", "payload.txt");
        seven_zip_pack(&program, &sandbox, "gzip", "bundle.tar.gz", "bundle.tar");
        let native = sandbox.file("native.txt", b"native 7z");
        seven_zip_pack(&program, &sandbox, "7z", "native.7z", "native.txt");

        for (archive, member, contents) in [
            ("bundle.tar.gz", &payload, b"compound tar".as_slice()),
            ("native.7z", &native, b"native 7z"),
        ] {
            let outcome =
                extract_archive_with(&backend, &sandbox.path(archive), no_cancel()).unwrap();
            assert_eq!(&outcome.published, member);
            assert_eq!(read(outcome.published), contents);
        }
    }

    #[test]
    fn official_backend_creates_zip_from_directory_and_multiselection_when_available() {
        let Some((backend, _)) = official_backend() else {
            return;
        };
        let sandbox = Sandbox::new();
        sandbox.file("folder/nested.txt", b"nested");
        let sources = [sandbox.path("folder"), sandbox.file("loose.txt", b"loose")];
        let archive = sandbox.path("selection.zip");

        create_zip_archive_with(&backend, &sources, &archive, no_cancel()).unwrap();
        let entries = backend.list(&archive, no_cancel()).unwrap();
        for member in ["folder/nested.txt", "loose.txt"] {
            assert!(entries.iter().any(|entry| entry.path == Path::new(member)), "{member}");
        }
    }
}
