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

use anyhow::{Context as _, Result, bail};
use rustix::process::{Pid, Signal, kill_process_group};

use crate::local_fs::{PathOccupancy, path_occupancy, rename_no_replace};

pub const MAX_ARCHIVE_ENTRIES: usize = 100_000;
pub const MAX_EXPANDED_BYTES: u64 = 100 * 1024 * 1024 * 1024;
const MAX_SUBPROCESS_OUTPUT: usize = 256 * 1024;
const MAX_LISTING_OUTPUT: usize = 64 * 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const STAGING_MONITOR_INTERVAL: Duration = Duration::from_millis(200);

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

    pub fn program(&self) -> &Path {
        &self.program
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
        if cancelled.load(Ordering::Acquire) {
            bail!("Archive operation cancelled");
        }

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

        let mut child = command.spawn().with_context(|| {
            format!(
                "Could not start archive backend “{}”",
                self.program.display()
            )
        })?;
        let stdout = child
            .stdout
            .take()
            .context("Archive backend has no stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("Archive backend has no stderr")?;
        let stdout_thread = thread::spawn(move || read_bounded(stdout, stdout_limit));
        let stderr_thread = thread::spawn(move || read_bounded(stderr, MAX_SUBPROCESS_OUTPUT));
        let pid =
            Pid::from_raw(child.id() as i32).context("Archive backend returned invalid PID")?;

        let monitored_directory = monitored_directory.map(Path::to_path_buf);
        let mut last_monitor = Instant::now();
        let mut safety_error = None;
        let (status, was_cancelled) = loop {
            if cancelled.load(Ordering::Acquire) {
                let _ = kill_process_group(pid, Signal::KILL);
                break (
                    child
                        .wait()
                        .context("Could not reap cancelled archive backend")?,
                    true,
                );
            }
            if last_monitor.elapsed() >= STAGING_MONITOR_INTERVAL {
                last_monitor = Instant::now();
                if let Some(directory) = &monitored_directory
                    && let Err(error) = check_staged_limits(directory)
                {
                    let _ = kill_process_group(pid, Signal::KILL);
                    safety_error = Some(error);
                    break (
                        child
                            .wait()
                            .context("Could not reap unsafe archive backend")?,
                        false,
                    );
                }
            }
            if let Some(status) = child
                .try_wait()
                .context("Could not inspect archive backend status")?
            {
                break (status, false);
            }
            thread::sleep(POLL_INTERVAL);
        };

        let stdout = stdout_thread
            .join()
            .map_err(|_| anyhow::anyhow!("Archive stdout reader panicked"))??;
        let stderr = stderr_thread
            .join()
            .map_err(|_| anyhow::anyhow!("Archive stderr reader panicked"))??;
        if was_cancelled {
            bail!("Archive operation cancelled");
        }
        if let Some(error) = safety_error {
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
        let arguments = [
            OsString::from("x"),
            OsString::from("-y"),
            OsString::from("-aos"),
            OsString::from("-sccUTF-8"),
            OsString::from("-p"),
            output_directory_argument(destination),
            OsString::from("--"),
            archive.as_os_str().to_owned(),
        ];
        let output = self.run(
            arguments,
            None,
            Some(destination),
            MAX_SUBPROCESS_OUTPUT,
            cancelled,
        )?;
        output.require_success("extract", archive)
    }

    fn create_zip(
        &self,
        sources: &[PathBuf],
        destination: &Path,
        cancelled: Arc<AtomicBool>,
    ) -> Result<()> {
        if sources.is_empty() {
            bail!("Select at least one item to compress");
        }
        let parent = sources[0]
            .parent()
            .context("Compression source has no parent")?;
        if sources.iter().any(|source| source.parent() != Some(parent)) {
            bail!("Compression sources must share one directory");
        }

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
            arguments.push(
                source
                    .file_name()
                    .context("Compression source has no filename")?
                    .to_owned(),
            );
        }
        let output = self.run(
            arguments,
            Some(parent),
            None,
            MAX_SUBPROCESS_OUTPUT,
            cancelled,
        )?;
        output.require_success("compress", destination)
    }
}

pub fn extract_archive(archive: &Path, cancelled: Arc<AtomicBool>) -> Result<ArchiveOutcome> {
    let backend = SevenZipBackend::discover()?;
    extract_archive_with(&backend, archive, cancelled)
}

pub fn create_zip_archive(
    sources: &[PathBuf],
    destination: &Path,
    cancelled: Arc<AtomicBool>,
) -> Result<ArchiveOutcome> {
    let backend = SevenZipBackend::discover()?;
    create_zip_archive_with(&backend, sources, destination, cancelled)
}

fn extract_archive_with<B: ArchiveBackend>(
    backend: &B,
    archive: &Path,
    cancelled: Arc<AtomicBool>,
) -> Result<ArchiveOutcome> {
    let parent = archive
        .parent()
        .context("Archive has no containing directory")?;
    let entries = backend.list(archive, cancelled.clone())?;
    validate_preflight(&entries)?;

    let mut staging = archive_staging(parent)?;
    backend.extract(archive, staging.path(), cancelled.clone())?;
    check_cancelled(&cancelled)?;
    let mut staged = validate_staged_tree(staging.path(), &cancelled)?;

    if staged.len() == 1
        && staged[0].is_file
        && staged[0]
            .path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("tar"))
        && is_compound_tar_archive(archive)
    {
        let intermediate = staged.remove(0).path;
        let entries = backend.list(&intermediate, cancelled.clone())?;
        validate_preflight(&entries)?;
        let nested_staging = archive_staging(parent)?;
        backend.extract(&intermediate, nested_staging.path(), cancelled.clone())?;
        check_cancelled(&cancelled)?;
        validate_staged_tree(nested_staging.path(), &cancelled)?;
        staging = nested_staging;
    }

    let top_level = top_level_staged_paths(staging.path())?;
    if top_level.is_empty() {
        bail!("Archive contains no extractable entries");
    }

    let published = if top_level.len() == 1 {
        let source = &top_level[0];
        let destination = parent.join(
            source
                .file_name()
                .context("Extracted item has no filename")?,
        );
        ensure_unoccupied(&destination)?;
        rename_no_replace(source, &destination).with_context(|| {
            format!(
                "Could not publish extracted item “{}”",
                destination.display()
            )
        })?;
        destination
    } else {
        let destination = parent.join(archive_stem(archive));
        ensure_unoccupied(&destination)?;
        let source = staging.keep();
        if let Err(error) = rename_no_replace(&source, &destination) {
            let cleanup_error = fs::remove_dir_all(&source).err();
            let message = format!(
                "Could not publish extracted directory “{}”: {error}",
                destination.display()
            );
            return match cleanup_error {
                Some(cleanup_error) => Err(anyhow::anyhow!(
                    "{message}; staging cleanup also failed: {cleanup_error}"
                )),
                None => Err(anyhow::anyhow!(message)),
            };
        }
        destination
    };

    Ok(ArchiveOutcome { published })
}

fn create_zip_archive_with<B: ArchiveBackend>(
    backend: &B,
    sources: &[PathBuf],
    destination: &Path,
    cancelled: Arc<AtomicBool>,
) -> Result<ArchiveOutcome> {
    if sources.is_empty() {
        bail!("Select at least one item to compress");
    }
    if !destination
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
    {
        bail!("The first archive version creates ZIP files only");
    }
    ensure_unoccupied(destination)?;
    validate_compression_sources(sources, &cancelled)?;
    let parent = destination
        .parent()
        .context("Archive destination has no containing directory")?;
    let staging = archive_staging(parent)?;
    let staged_archive = staging.path().join("archive.zip");
    backend.create_zip(sources, &staged_archive, cancelled.clone())?;
    check_cancelled(&cancelled)?;
    if !staged_archive.is_file() {
        bail!("Archive backend reported success without creating a ZIP file");
    }
    let entries = backend.list(&staged_archive, cancelled)?;
    validate_preflight(&entries)?;
    ensure_unoccupied(destination)?;
    rename_no_replace(&staged_archive, destination)
        .with_context(|| format!("Could not publish ZIP “{}”", destination.display()))?;
    Ok(ArchiveOutcome {
        published: destination.to_path_buf(),
    })
}

fn archive_staging(parent: &Path) -> Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix(".marcel-archive-")
        .tempdir_in(parent)
        .with_context(|| {
            format!(
                "Could not create private archive staging in “{}”",
                parent.display()
            )
        })
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Acquire) {
        bail!("Archive operation cancelled");
    }
    Ok(())
}

fn validate_preflight(entries: &[ArchiveEntry]) -> Result<()> {
    if entries.is_empty() {
        bail!("Archive contains no extractable entries");
    }
    if entries.len() > MAX_ARCHIVE_ENTRIES {
        bail!("Archive contains more than {MAX_ARCHIVE_ENTRIES} entries");
    }
    let expanded = entries.iter().try_fold(0_u64, |total, entry| {
        total
            .checked_add(entry.size)
            .context("Archive expanded size overflowed")
    })?;
    if expanded > MAX_EXPANDED_BYTES {
        bail!(
            "Archive expands beyond Marcel's {} GiB safety limit",
            MAX_EXPANDED_BYTES / 1024 / 1024 / 1024
        );
    }
    for entry in entries {
        validate_archive_path(&entry.path.to_string_lossy())?;
    }
    Ok(())
}

#[derive(Debug)]
struct StagedEntry {
    path: PathBuf,
    is_file: bool,
}

fn validate_staged_tree(root: &Path, cancelled: &AtomicBool) -> Result<Vec<StagedEntry>> {
    let mut entries = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    let mut total_bytes = 0_u64;
    while let Some(directory) = pending.pop() {
        check_cancelled(cancelled)?;
        for entry in fs::read_dir(&directory)
            .with_context(|| format!("Could not inspect staging “{}”", directory.display()))?
        {
            check_cancelled(cancelled)?;
            let entry = entry.context("Could not read archive staging entry")?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("Could not inspect “{}”", path.display()))?;
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                bail!(
                    "Archive extracted an unsupported symbolic link “{}”",
                    path.display()
                );
            }
            if file_type.is_dir() {
                pending.push(path.clone());
            } else if file_type.is_file() {
                total_bytes = total_bytes
                    .checked_add(metadata.len())
                    .context("Extracted size overflowed")?;
            } else {
                bail!(
                    "Archive extracted unsupported special entry “{}”",
                    path.display()
                );
            }
            entries.push(StagedEntry {
                path,
                is_file: file_type.is_file(),
            });
            if entries.len() > MAX_ARCHIVE_ENTRIES {
                bail!("Archive extracted more than {MAX_ARCHIVE_ENTRIES} entries");
            }
            if total_bytes > MAX_EXPANDED_BYTES {
                bail!(
                    "Archive extracted beyond Marcel's {} GiB safety limit",
                    MAX_EXPANDED_BYTES / 1024 / 1024 / 1024
                );
            }
        }
    }
    if entries.is_empty() {
        bail!("Archive contains no extractable entries");
    }
    Ok(entries)
}

fn check_staged_limits(root: &Path) -> Result<()> {
    let mut pending = vec![root.to_path_buf()];
    let mut entries = 0_usize;
    let mut total_bytes = 0_u64;
    while let Some(directory) = pending.pop() {
        let read_dir = match fs::read_dir(&directory) {
            Ok(read_dir) => read_dir,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Could not monitor staging “{}”", directory.display())
                });
            }
        };
        for entry in read_dir {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error).context("Could not monitor archive staging entry"),
            };
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Could not monitor “{}”", path.display()));
                }
            };
            if metadata.file_type().is_symlink() {
                bail!(
                    "Archive extracted an unsupported symbolic link “{}”",
                    path.display()
                );
            }
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() {
                total_bytes = total_bytes
                    .checked_add(metadata.len())
                    .context("Extracted size overflowed")?;
            } else {
                bail!(
                    "Archive extracted unsupported special entry “{}”",
                    path.display()
                );
            }
            entries += 1;
            if entries > MAX_ARCHIVE_ENTRIES {
                bail!("Archive extracted more than {MAX_ARCHIVE_ENTRIES} entries");
            }
            if total_bytes > MAX_EXPANDED_BYTES {
                bail!(
                    "Archive extracted beyond Marcel's {} GiB safety limit",
                    MAX_EXPANDED_BYTES / 1024 / 1024 / 1024
                );
            }
        }
    }
    Ok(())
}

fn top_level_staged_paths(root: &Path) -> Result<Vec<PathBuf>> {
    fs::read_dir(root)
        .with_context(|| format!("Could not inspect archive staging “{}”", root.display()))?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .context("Could not read archive staging entry")
        })
        .collect()
}

fn validate_compression_sources(sources: &[PathBuf], cancelled: &AtomicBool) -> Result<()> {
    let parent = sources[0]
        .parent()
        .context("Compression source has no parent")?;
    let mut count = 0_usize;
    for source in sources {
        if source.parent() != Some(parent) {
            bail!("Compression sources must share one directory");
        }
        let mut pending = vec![source.clone()];
        while let Some(path) = pending.pop() {
            check_cancelled(cancelled)?;
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("Could not inspect “{}”", path.display()))?;
            if metadata.file_type().is_symlink() {
                bail!(
                    "Symbolic links are not supported by the first archive version: “{}”",
                    path.display()
                );
            }
            if metadata.is_dir() {
                for child in fs::read_dir(&path)
                    .with_context(|| format!("Could not read “{}”", path.display()))?
                {
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
    }
    Ok(())
}

fn is_compound_tar_archive(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    [
        ".tar.bz2", ".tar.gz", ".tar.xz", ".tar.zst", ".tbz", ".tbz2", ".tgz", ".txz",
    ]
    .iter()
    .any(|extension| name.ends_with(extension))
}

fn ensure_unoccupied(path: &Path) -> Result<()> {
    match path_occupancy(path) {
        Ok(PathOccupancy::Occupied) => bail!(
            "“{}” already exists; nothing was overwritten",
            path.display()
        ),
        Ok(PathOccupancy::Vacant) => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("Could not inspect destination “{}”", path.display())),
    }
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
        let diagnostics = bounded_diagnostics(&self.stderr, &self.stdout);
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
    Ok(CapturedOutput {
        bytes: retained,
        truncated,
    })
}

fn bounded_diagnostics(primary: &[u8], fallback: &[u8]) -> String {
    let bytes = if primary.is_empty() {
        fallback
    } else {
        primary
    };
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "no diagnostics".to_string()
    } else {
        trimmed.to_string()
    }
}

fn discover_with(
    override_program: Option<OsString>,
    current_exe: &Path,
    path: Option<OsString>,
) -> Result<PathBuf> {
    if let Some(program) = override_program.filter(|program| !program.is_empty()) {
        let program = PathBuf::from(program);
        ensure_executable(&program).with_context(|| {
            format!(
                "MARCEL_7ZZ points to an unavailable program “{}”",
                program.display()
            )
        })?;
        return Ok(program);
    }

    if let Some(prefix) = current_exe.parent().and_then(Path::parent) {
        let private = prefix.join("libexec/marcel/7zz");
        if is_executable(&private) {
            return Ok(private);
        }
    }

    for name in ["7zz", "7z"] {
        if let Some(program) = find_on_path(name, path.as_deref()) {
            return Ok(program);
        }
    }

    bail!(
        "Archive support is unavailable because 7-Zip was not found; install `7zz` or set MARCEL_7ZZ"
    )
}

fn find_on_path(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    env::split_paths(path?).find_map(|directory| {
        let candidate = directory.join(name);
        is_executable(&candidate).then_some(candidate)
    })
}

fn ensure_executable(path: &Path) -> Result<()> {
    if is_executable(path) {
        Ok(())
    } else {
        bail!("file does not exist or is not executable")
    }
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

pub fn is_supported_archive(path: &Path) -> bool {
    is_supported_archive_with(path, rar_support_enabled())
}

fn is_supported_archive_with(path: &Path, rar_supported: bool) -> bool {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let supported = [
        ".7z",
        ".apk",
        ".bz2",
        ".bzip2",
        ".cab",
        ".cb7",
        ".cbz",
        ".cpio",
        ".deb",
        ".dmg",
        ".gz",
        ".gzip",
        ".img",
        ".iso",
        ".jar",
        ".lzma",
        ".rpm",
        ".squashfs",
        ".tar",
        ".tar.bz2",
        ".tar.gz",
        ".tar.xz",
        ".tar.zst",
        ".tbz",
        ".tbz2",
        ".tgz",
        ".txz",
        ".vhd",
        ".vhdx",
        ".wim",
        ".xar",
        ".xz",
        ".zip",
        ".zst",
    ];
    supported.iter().any(|extension| name.ends_with(extension))
        || (rar_supported
            && [".rar", ".cbr"]
                .iter()
                .any(|extension| name.ends_with(extension)))
}

fn rar_support_enabled() -> bool {
    env::var("MARCEL_ENABLE_RAR").is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

pub fn default_zip_name(sources: &[PathBuf], single_is_directory: bool) -> String {
    if sources.len() == 1 {
        let source = &sources[0];
        let stem = if single_is_directory {
            source.file_name()
        } else {
            source.file_stem()
        }
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
        format!("{stem}.zip")
    } else {
        "Archive.zip".to_string()
    }
}

pub fn archive_stem(path: &Path) -> String {
    let mut name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Archive".to_string());
    const EXTENSIONS: &[&str] = &[
        "7z", "apk", "bz2", "bzip2", "cab", "cb7", "cbr", "cbz", "cpio", "deb", "dmg", "gz",
        "gzip", "img", "iso", "jar", "lzma", "rar", "rpm", "squashfs", "tar", "tbz", "tbz2", "tgz",
        "txz", "vhd", "vhdx", "wim", "xar", "xz", "zip", "zst",
    ];
    while let Some((stem, extension)) = name.rsplit_once('.') {
        if stem.is_empty()
            || !EXTENSIONS
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        {
            break;
        }
        let stem_length = stem.len();
        name.truncate(stem_length);
    }
    if name.is_empty() {
        "Archive".to_string()
    } else {
        name
    }
}

fn parse_listing(output: &str) -> Result<Vec<ArchiveEntry>> {
    let mut entries = Vec::new();
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
        entries.push(ArchiveEntry {
            path: PathBuf::from(path),
            size,
            is_dir,
        });
        if entries.len() > MAX_ARCHIVE_ENTRIES {
            bail!("Archive contains more than {MAX_ARCHIVE_ENTRIES} entries");
        }
    }

    let expanded = entries.iter().try_fold(0_u64, |total, entry| {
        total
            .checked_add(entry.size)
            .context("Archive expanded size overflowed")
    })?;
    if expanded > MAX_EXPANDED_BYTES {
        bail!(
            "Archive expands beyond Marcel's {} GiB safety limit",
            MAX_EXPANDED_BYTES / 1024 / 1024 / 1024
        );
    }
    if entries.is_empty() {
        bail!("Archive contains no extractable entries");
    }
    Ok(entries)
}

fn output_directory_argument(destination: &Path) -> OsString {
    let mut argument = OsString::from("-o");
    argument.push(destination);
    argument
}

fn validate_archive_path(value: &str) -> Result<()> {
    if value.is_empty()
        || value.starts_with(['/', '\\'])
        || value.contains('\\')
        || value
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
        || value
            .as_bytes()
            .get(1)
            .is_some_and(|character| *character == b':')
    {
        bail!("Archive contains unsafe path “{value}”");
    }
    let path = Path::new(value);
    if path.components().any(|component| {
        !matches!(component, Component::Normal(_))
            || matches!(component, Component::Normal(name) if name.is_empty())
    }) {
        bail!("Archive contains unsafe path “{value}”");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[derive(Clone, Copy)]
    enum FakeExtraction {
        One,
        Multiple,
        Symlink,
        Empty,
        OversizedSparse,
    }

    struct FakeBackend {
        extraction: FakeExtraction,
    }

    impl ArchiveBackend for FakeBackend {
        fn list(&self, _archive: &Path, _cancelled: Arc<AtomicBool>) -> Result<Vec<ArchiveEntry>> {
            Ok(vec![ArchiveEntry {
                path: PathBuf::from("file.txt"),
                size: 4,
                is_dir: false,
            }])
        }

        fn extract(
            &self,
            _archive: &Path,
            destination: &Path,
            _cancelled: Arc<AtomicBool>,
        ) -> Result<()> {
            match self.extraction {
                FakeExtraction::One => fs::write(destination.join("file.txt"), b"test")?,
                FakeExtraction::Multiple => {
                    fs::write(destination.join("one.txt"), b"one")?;
                    fs::write(destination.join("two.txt"), b"two")?;
                }
                FakeExtraction::Symlink => {
                    std::os::unix::fs::symlink("../outside", destination.join("link"))?;
                }
                FakeExtraction::Empty => {}
                FakeExtraction::OversizedSparse => {
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

    fn executable(path: &Path) {
        fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn discovery_order_is_override_private_7zz_then_7z() {
        let root = tempfile::tempdir().unwrap();
        let override_program = root.path().join("override");
        let prefix = root.path().join("prefix");
        let current_exe = prefix.join("bin/marcel");
        let private = prefix.join("libexec/marcel/7zz");
        let path_dir = root.path().join("path");
        fs::create_dir_all(private.parent().unwrap()).unwrap();
        fs::create_dir_all(&path_dir).unwrap();
        executable(&override_program);
        executable(&private);
        executable(&path_dir.join("7zz"));
        executable(&path_dir.join("7z"));
        let path = env::join_paths([&path_dir]).unwrap();

        assert_eq!(
            discover_with(
                Some(override_program.as_os_str().to_owned()),
                &current_exe,
                Some(path.clone())
            )
            .unwrap(),
            override_program
        );
        assert_eq!(
            discover_with(None, &current_exe, Some(path.clone())).unwrap(),
            private
        );
        fs::remove_file(&private).unwrap();
        assert_eq!(
            discover_with(None, &current_exe, Some(path.clone())).unwrap(),
            path_dir.join("7zz")
        );
        fs::remove_file(path_dir.join("7zz")).unwrap();
        assert_eq!(
            discover_with(None, &current_exe, Some(path)).unwrap(),
            path_dir.join("7z")
        );
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
                ArchiveEntry {
                    path: PathBuf::from("folder"),
                    size: 0,
                    is_dir: true,
                },
                ArchiveEntry {
                    path: PathBuf::from("folder/file.txt"),
                    size: 12,
                    is_dir: false,
                },
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
        assert_eq!(
            default_zip_name(&[PathBuf::from("/tmp/report.pdf")], false),
            "report.zip"
        );
        assert_eq!(
            default_zip_name(&[PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")], false),
            "Archive.zip"
        );
        assert!(is_supported_archive(Path::new("BOOKS.CBZ")));
        assert!(!is_supported_archive(Path::new("notes.txt")));
        assert!(!is_supported_archive_with(Path::new("books.cbr"), false));
        assert!(!is_supported_archive_with(Path::new("archive.rar"), false));
        assert!(is_supported_archive_with(Path::new("books.cbr"), true));
        assert!(is_supported_archive_with(Path::new("archive.rar"), true));
    }

    #[test]
    fn explicit_missing_override_is_not_silently_ignored() {
        let error = discover_with(
            Some(OsString::from("/definitely/missing/7zz")),
            Path::new("/prefix/bin/marcel"),
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
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("bundle.zip");
        fs::write(&archive, b"archive").unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));

        let outcome = extract_archive_with(
            &FakeBackend {
                extraction: FakeExtraction::One,
            },
            &archive,
            cancelled.clone(),
        )
        .unwrap();
        assert_eq!(outcome.published, root.path().join("file.txt"));
        assert_eq!(fs::read(&outcome.published).unwrap(), b"test");

        fs::remove_file(&outcome.published).unwrap();
        let outcome = extract_archive_with(
            &FakeBackend {
                extraction: FakeExtraction::Multiple,
            },
            &archive,
            cancelled.clone(),
        )
        .unwrap();
        assert_eq!(outcome.published, root.path().join("bundle"));
        assert_eq!(fs::read(outcome.published.join("one.txt")).unwrap(), b"one");

        fs::remove_dir_all(&outcome.published).unwrap();
        fs::write(root.path().join("file.txt"), b"occupied").unwrap();
        assert!(
            extract_archive_with(
                &FakeBackend {
                    extraction: FakeExtraction::One,
                },
                &archive,
                cancelled,
            )
            .is_err()
        );
        assert_eq!(fs::read(root.path().join("file.txt")).unwrap(), b"occupied");
    }

    #[test]
    fn extracted_symlinks_are_rejected_and_staging_is_cleaned() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("links.zip");
        fs::write(&archive, b"archive").unwrap();
        let error = extract_archive_with(
            &FakeBackend {
                extraction: FakeExtraction::Symlink,
            },
            &archive,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap_err();
        assert!(error.to_string().contains("symbolic link"));
        assert!(fs::read_dir(root.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".marcel-archive-")
        }));
    }

    #[test]
    fn empty_and_dishonestly_oversized_extractions_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("unsafe.zip");
        fs::write(&archive, b"archive").unwrap();
        for extraction in [FakeExtraction::Empty, FakeExtraction::OversizedSparse] {
            assert!(
                extract_archive_with(
                    &FakeBackend { extraction },
                    &archive,
                    Arc::new(AtomicBool::new(false)),
                )
                .is_err()
            );
        }
        assert_eq!(
            fs::read_dir(root.path())
                .unwrap()
                .filter_map(Result::ok)
                .count(),
            1
        );
    }

    #[test]
    fn listing_limits_entry_count_and_declared_expanded_size() {
        let oversized = format!(
            "Path = huge.bin\nSize = {}\nFolder = -\n",
            MAX_EXPANDED_BYTES + 1
        );
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
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("report.txt");
        let destination = root.path().join("report.zip");
        fs::write(&source, b"report").unwrap();
        let backend = FakeBackend {
            extraction: FakeExtraction::One,
        };

        let outcome = create_zip_archive_with(
            &backend,
            std::slice::from_ref(&source),
            &destination,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert_eq!(outcome.published, destination);
        assert_eq!(fs::read(&destination).unwrap(), b"fake zip");

        let error = create_zip_archive_with(
            &backend,
            &[source],
            &destination,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap_err();
        assert!(error.to_string().contains("already exists"));
        assert_eq!(fs::read(&destination).unwrap(), b"fake zip");
    }

    #[test]
    fn cancellation_terminates_the_archive_process_group() {
        let root = tempfile::tempdir().unwrap();
        let program = root.path().join("slow-7zz");
        fs::write(&program, "#!/bin/sh\nsleep 30\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let backend = SevenZipBackend::from_program(program);
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_for_thread = cancelled.clone();
        let trigger = thread::spawn(move || {
            thread::sleep(Duration::from_millis(60));
            cancel_for_thread.store(true, Ordering::Release);
        });

        let error = backend
            .list(Path::new("unused.zip"), cancelled)
            .unwrap_err();
        trigger.join().unwrap();
        assert!(error.to_string().contains("cancelled"));
    }

    #[test]
    fn official_backend_round_trips_zip_when_available() {
        let Some(program) = find_on_path("7zz", env::var_os("PATH").as_deref()) else {
            return;
        };
        let backend = SevenZipBackend::from_program(program);
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("report.txt");
        let archive = root.path().join("report.zip");
        fs::write(&source, b"round trip").unwrap();

        create_zip_archive_with(
            &backend,
            std::slice::from_ref(&source),
            &archive,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        fs::remove_file(&source).unwrap();
        let outcome =
            extract_archive_with(&backend, &archive, Arc::new(AtomicBool::new(false))).unwrap();

        assert_eq!(outcome.published, source);
        assert_eq!(fs::read(outcome.published).unwrap(), b"round trip");
    }

    #[test]
    fn official_backend_unwraps_one_compound_tar_layer_when_available() {
        let Some(program) = find_on_path("7zz", env::var_os("PATH").as_deref()) else {
            return;
        };
        let backend = SevenZipBackend::from_program(program.clone());
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("payload.txt");
        let tar = root.path().join("bundle.tar");
        let archive = root.path().join("bundle.tar.gz");
        fs::write(&source, b"compound tar").unwrap();
        let status = Command::new(&program)
            .current_dir(root.path())
            .args(["a", "-ttar"])
            .arg(&tar)
            .arg("payload.txt")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let status = Command::new(program)
            .current_dir(root.path())
            .args(["a", "-tgzip"])
            .arg(&archive)
            .arg("bundle.tar")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        fs::remove_file(&source).unwrap();
        fs::remove_file(&tar).unwrap();

        let outcome =
            extract_archive_with(&backend, &archive, Arc::new(AtomicBool::new(false))).unwrap();
        assert_eq!(outcome.published, source);
        assert_eq!(fs::read(outcome.published).unwrap(), b"compound tar");
    }

    #[test]
    fn official_backend_extracts_native_7z_when_available() {
        let Some(program) = find_on_path("7zz", env::var_os("PATH").as_deref()) else {
            return;
        };
        let backend = SevenZipBackend::from_program(program.clone());
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("native.txt");
        let archive = root.path().join("native.7z");
        fs::write(&source, b"native 7z").unwrap();
        let status = Command::new(program)
            .current_dir(root.path())
            .args(["a", "-t7z"])
            .arg(&archive)
            .arg("native.txt")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        fs::remove_file(&source).unwrap();

        let outcome =
            extract_archive_with(&backend, &archive, Arc::new(AtomicBool::new(false))).unwrap();
        assert_eq!(outcome.published, source);
        assert_eq!(fs::read(outcome.published).unwrap(), b"native 7z");
    }

    #[test]
    fn official_backend_creates_zip_from_directory_and_multiselection_when_available() {
        let Some(program) = find_on_path("7zz", env::var_os("PATH").as_deref()) else {
            return;
        };
        let backend = SevenZipBackend::from_program(program);
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("folder");
        let loose = root.path().join("loose.txt");
        let archive = root.path().join("selection.zip");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("nested.txt"), b"nested").unwrap();
        fs::write(&loose, b"loose").unwrap();

        create_zip_archive_with(
            &backend,
            &[folder, loose],
            &archive,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let entries = backend
            .list(&archive, Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert!(
            entries
                .iter()
                .any(|entry| entry.path == Path::new("folder/nested.txt"))
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry.path == Path::new("loose.txt"))
        );
    }
}
