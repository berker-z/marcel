//! Archives, through a 7-Zip subprocess that never sees a path it could
//! misread as an option or a wildcard and never writes anywhere but a private
//! staging directory.
//!
//! Extraction publishes only regular files and directories: link and special
//! entries are refused from the listing before anything is written, and again
//! from the staged tree in case the listing lied. Mode bits are 7-Zip's to
//! apply, and it drops setuid, setgid and sticky on extraction (verified with
//! 7zz 26.02 for zip, 7z and tar), so an archive cannot publish a privileged
//! binary; the copy path's policy on those bits is its own.

use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    io::{self, Read},
    os::unix::fs::PermissionsExt as _,
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

use super::{
    conflict::ConflictPolicy,
    copy::{MergeStop, merge_directories},
    journal::{PathSnapshot, UNDO_SNAPSHOT_LIMIT},
    local::{ensure_unoccupied, inspect, rename_no_replace},
    mutations::validate_entry_os_name,
    quarantine::{
        ReplacedItem, WorkingKind, preserve_unrestored, quarantine_for_replacement,
        restore_replaced_items, staging_prefix,
    },
    transfer::{SourcePlan, TransferMode, plan_source},
};

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

#[derive(Debug)]
pub struct ArchiveOutcome {
    pub published: PathBuf,
    /// What publishing displaced, held aside so undo can put it back.
    pub replaced: Vec<ReplacedItem>,
    /// Present when the output was folded into a folder already at
    /// `published` rather than published as a new tree.
    pub merged: Option<MergeAdded>,
}

impl ArchiveOutcome {
    fn fresh(published: PathBuf) -> Self {
        Self { published, replaced: Vec::new(), merged: None }
    }
}

/// What a merge added to the folder it joined, for undo to take back.
#[derive(Debug)]
pub struct MergeAdded {
    pub(super) added: Vec<PathSnapshot>,
    /// `false` when the additions stand but could not be described, so undo
    /// is not offered for them.
    pub(super) undoable: bool,
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

    #[cfg(test)]
    pub fn from_program(program: PathBuf) -> Self {
        Self { program }
    }

    /// The backend process before its arguments: both outputs captured, and
    /// the shared builder's hygiene (no stdin, no leaked `LD_LIBRARY_PATH`,
    /// its own process group so cancellation can kill whatever it spawned).
    fn command(&self) -> Command {
        let mut command = crate::preview::tool::command(&self.program);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        command
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

        let mut command = self.command();
        command.args(arguments);
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

/// Take file names literally. 7-Zip expands `*` and `?` in every name it is
/// given, the archive's included, and `--` does not stop it: without this, a
/// file called `*` beside secrets puts the secrets in the ZIP too, and
/// extracting `*.zip` extracts every ZIP beside it.
const LITERAL_NAMES: &str = "-spd";

impl ArchiveBackend for SevenZipBackend {
    fn list(&self, archive: &Path, cancelled: Arc<AtomicBool>) -> Result<Vec<ArchiveEntry>> {
        let arguments = [
            OsString::from("l"),
            OsString::from("-ba"),
            OsString::from("-slt"),
            OsString::from("-sccUTF-8"),
            OsString::from("-p"),
            OsString::from(LITERAL_NAMES),
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
            OsString::from(LITERAL_NAMES),
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
            OsString::from(LITERAL_NAMES),
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

pub fn extract_archive(
    archive: &Path,
    cancelled: Arc<AtomicBool>,
    policy: &mut ConflictPolicy,
) -> Result<ArchiveOutcome> {
    extract_archive_with(&SevenZipBackend::discover()?, archive, cancelled, policy)
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
    policy: &mut ConflictPolicy,
) -> Result<ArchiveOutcome> {
    let parent = archive.parent().context("Archive has no containing directory")?;
    let extract_to_staging = |archive: &Path| -> Result<(Staging, Vec<StagedEntry>)> {
        validate_preflight(&backend.list(archive, cancelled.clone())?)?;
        let staging = Staging::reserve(parent)?;
        backend.extract(archive, &staging.root, cancelled.clone())?;
        check_cancelled(&cancelled)?;
        let staged = walk_staged(&staging.root, false, &cancelled)?;
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

    let top_level = fs::read_dir(&staging.root)
        .at("Could not inspect archive staging", &staging.root)?
        .map(|entry| {
            entry.map(|entry| entry.path()).context("Could not read archive staging entry")
        })
        .collect::<Result<Vec<_>>>()?;
    // One item is published as itself; several are published together under
    // the archive's name, which means publishing the output root.
    let (source, destination) = match top_level.as_slice() {
        [] => bail!("Archive contains no extractable entries"),
        [source] => {
            let name = source.file_name().context("Extracted item has no filename")?;
            // The archive's own naming is published verbatim here, so it has
            // to clear the bar Rename does: a `.marcel-` name would be hidden
            // and perhaps swept.
            if let Err(error) = validate_entry_os_name(name) {
                bail!(
                    "Cannot publish the archive's only entry “{}”: {error}",
                    name.to_string_lossy()
                );
            }
            (source.clone(), parent.join(name))
        }
        _ => (staging.root.clone(), parent.join(archive_stem(archive))),
    };
    // Staging outlives publication and is removed on the way out, whatever
    // the outcome; whether the rename emptied it or a merge left the rest.
    publish_extracted(source, parent, destination, &cancelled, policy)
}

/// One extraction's private working space.
///
/// The directory is `tempfile`'s, so it is created atomically under a unique
/// name and removed on drop, and it is `0700`: nothing is readable by anyone
/// else until published. The output root inside it is what a multi-entry
/// archive publishes as its folder, so it is created the way New Folder would
/// be — with the umask's mode — rather than inheriting the staging directory's.
struct Staging {
    /// Held for its drop, which removes whatever publication left behind.
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Staging {
    fn reserve(parent: &Path) -> Result<Self> {
        let directory = archive_staging(parent)?;
        let root = directory.path().join("extracted");
        fs::create_dir(&root).at("Could not create archive output root in", directory.path())?;
        Ok(Self { _directory: directory, root })
    }
}

/// Move the extracted output out of staging to where the user will find it.
///
/// An occupied destination is answered the way a copy's is: the same
/// question, the same skip, rename, replace, and merge. A replacement holds
/// the displaced item aside for undo and puts it back if publishing fails; a
/// merge copies into the existing folder and keeps what is already there.
fn publish_extracted(
    source: PathBuf,
    parent: &Path,
    destination: PathBuf,
    cancelled: &AtomicBool,
    policy: &mut ConflictPolicy,
) -> Result<ArchiveOutcome> {
    let plan = plan_source(&source, parent, destination, TransferMode::Copy, policy);
    let (target, displaced) = match plan {
        SourcePlan::Transfer(target) => (target, None),
        SourcePlan::Replace(target) => {
            let item = quarantine_for_replacement(&target)?;
            (target, Some(item))
        }
        SourcePlan::Merge(target) => {
            // Staging is dropped afterwards whether or not the merge finished,
            // since what it still holds is what the merge chose to leave.
            let outcome = merge_directories(&source, &target, cancelled, None, UNDO_SNAPSHOT_LIMIT);
            match outcome.stopped {
                None => {}
                Some(MergeStop::Cancelled) => bail!(
                    "Extraction cancelled; “{}” kept what had been merged into it so far",
                    target.display()
                ),
                Some(MergeStop::Failed(error)) => {
                    return Err(error.context(format!(
                        "Could not merge the archive into “{}”",
                        target.display()
                    )));
                }
            }
            return Ok(ArchiveOutcome {
                published: target,
                replaced: Vec::new(),
                merged: Some(MergeAdded { added: outcome.created, undoable: outcome.undoable }),
            });
        }
        SourcePlan::Skip | SourcePlan::Cancel => bail!("Extraction cancelled; nothing was written"),
        SourcePlan::AlreadyInPlace => bail!("Extracted output cannot already be in place"),
        SourcePlan::Failed(message) => bail!("{message}"),
    };

    // Commit: one rename.
    let published =
        rename_no_replace(&source, &target).at("Could not publish extracted item", &target);
    match published {
        Ok(()) => Ok(ArchiveOutcome {
            published: target,
            replaced: displaced.into_iter().collect(),
            merged: None,
        }),
        Err(error) => {
            // Put back what was displaced rather than leaving the destination
            // empty; if even that fails the quarantine is the user's only
            // copy, so it moves to recovery storage before this is reported.
            let mut message = error.to_string();
            if let Some(item) = displaced
                && let Err(unrestored) = restore_replaced_items(std::slice::from_ref(&item))
            {
                message.push_str(&format!("; {}", preserve_unrestored(unrestored)));
            }
            bail!(message)
        }
    }
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
    Ok(ArchiveOutcome::fresh(destination.to_path_buf()))
}

fn archive_staging(parent: &Path) -> Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix(&staging_prefix(WorkingKind::Archive))
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
///
/// The result becomes a folder the user did not name, so it has to clear the
/// bar New Folder does. `..zip` peels to `.`, `...zip` to `..`, and
/// `.marcel-copy-x.zip` to a name the browser would hide; each falls back to
/// `Archive` instead.
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
    if validate_entry_os_name(OsStr::new(&name)).is_err() { "Archive".to_string() } else { name }
}

fn parse_listing(output: &str) -> Result<Vec<ArchiveEntry>> {
    let mut entries = Vec::new();
    let mut budget = Budget::listing();
    for block in output.split("\n\n") {
        let mut path = None;
        let mut size = None;
        let mut is_dir = false;
        let mut is_link = false;
        let mut special = None;
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
                // `Attributes` is zip and 7z: Windows flags, then for entries
                // made on Unix the `ls -l` mode after a space. `Mode` is
                // tar's, the mode alone. `L` is the Windows reparse flag; the
                // mode's type character is what says link on Unix.
                "Attributes" | "Mode" => {
                    is_dir |= value.starts_with('D');
                    is_link |= value.contains('L');
                    if let Some(kind) = posix_file_type(value) {
                        is_dir |= kind == 'd';
                        if !matches!(kind, '-' | 'd') {
                            special = Some(kind);
                        }
                    }
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
        if let Some(kind) = special {
            bail!("Archive contains unsupported {} entry “{path}”", describe_file_type(kind));
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

/// The type character of an `ls -l`-style mode string ending `value`, if one
/// does.
///
/// 7zz prints ` lrwxrwxrwx`, `A lrwxrwxrwx` and `D drwxr-xr-x`; p7zip prefixes
/// the mode with its own flag spellings such as `_` and `D_`. Only the last
/// token matters, and only when it has a mode's ten characters, so a
/// Windows-made entry with flags alone (`Attributes = A`) reads as no mode
/// rather than as a file of type `A`.
fn posix_file_type(value: &str) -> Option<char> {
    let mode = value.split_whitespace().next_back()?;
    (mode.len() == 10).then(|| mode.chars().next()).flatten()
}

/// What a mode's type character calls the entry, for a refusal.
fn describe_file_type(kind: char) -> &'static str {
    match kind {
        'l' => "symbolic link",
        'p' => "named pipe",
        'c' => "character device",
        'b' => "block device",
        's' => "socket",
        _ => "special",
    }
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
    use crate::{
        fsops::{
            conflict::{ConflictDecision, ConflictRequest, ConflictResolver, ConflictResponse},
            quarantine::{
                erase_replacement_quarantine, is_internal_working_name,
                is_replacement_quarantine_name,
            },
        },
        testing::{Sandbox, no_cancel, read},
    };

    /// What the fake backend leaves in the staging directory.
    #[derive(Clone, Copy)]
    enum Fake {
        One,
        Multiple,
        Symlink,
        Empty,
        OversizedSparse,
        ReservedName,
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
            // The output root sits inside staging that the browser must hide
            // while this runs and a later Marcel must be able to reclaim.
            let staging = destination.parent().unwrap().file_name().unwrap();
            assert!(is_internal_working_name(staging), "{staging:?}");
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
                Fake::ReservedName => {
                    fs::create_dir(destination.join(".marcel-copy-x"))?;
                    fs::write(destination.join(".marcel-copy-x/file.txt"), b"hidden")?;
                }
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
        extract_archive_with(&fake, archive, no_cancel(), &mut ConflictPolicy::refusing())
    }

    /// The real 7-Zip, when the environment has one.
    ///
    /// Without it the tests that need it pass without testing anything, which
    /// a CI run should not be allowed to call green: `MARCEL_TEST_REQUIRE_7ZZ=1`
    /// turns the skip into a failure.
    fn official_backend() -> Option<(SevenZipBackend, PathBuf)> {
        let Some(program) = find_on_path("7zz", env::var_os("PATH").as_deref()) else {
            let required = env::var_os("MARCEL_TEST_REQUIRE_7ZZ").is_some_and(|value| value == "1");
            assert!(!required, "MARCEL_TEST_REQUIRE_7ZZ=1 is set but no 7zz is on PATH");
            eprintln!("skipping: no 7zz on PATH; set MARCEL_TEST_REQUIRE_7ZZ=1 to fail instead");
            return None;
        };
        Some((SevenZipBackend::from_program(program.clone()), program))
    }

    /// Have 7-Zip itself build `archive` of `kind` from `member`, then
    /// remove the member so extraction has to recreate it.
    fn seven_zip_pack(program: &Path, sandbox: &Sandbox, kind: &str, archive: &str, member: &str) {
        let status = Command::new(program)
            .current_dir(sandbox.root())
            .args(["a", &format!("-t{kind}"), LITERAL_NAMES, archive, member])
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

    /// `7zz l -slt` output for archives 7zz and GNU tar made from a tree with
    /// links and special files; see `fixtures/`. Only tar has a `Symbolic
    /// Link` key, so zip and 7z links are found by the mode 7zz appends to
    /// `Attributes`, and the same reading catches every other non-file type.
    #[test]
    fn listing_parser_refuses_links_and_special_files_in_zip_7z_and_tar_listings() {
        const ZIP: &str = include_str!("fixtures/listing-zip-symlink.txt");
        const SEVEN_Z: &str = include_str!("fixtures/listing-7z-symlink.txt");
        const TAR: &str = include_str!("fixtures/listing-tar-special.txt");

        // The unabridged listings all name a link before anything else.
        for (listing, first_link) in [(ZIP, "tree/abslink"), (SEVEN_Z, "tree/abslink")] {
            let error = parse_listing(listing).unwrap_err().to_string();
            assert!(error.contains("symbolic link") && error.contains(first_link), "{error}");
        }
        let error = parse_listing(TAR).unwrap_err().to_string();
        assert!(error.contains("named pipe") && error.contains("special/fifo"), "{error}");

        // Block by block: each unsupported type is refused by name, and the
        // regular entries beside them are accepted with their kinds intact.
        let blocks =
            |listing: &'static str| listing.split("\n\n").filter(|block| !block.trim().is_empty());
        // Tar's `Symbolic Link` key is read before its mode, so its refusal
        // says "link" without the "symbolic".
        let refused = [
            ("tree/abslink", "symbolic link"),
            ("tree/link", "symbolic link"),
            ("special/link", "link"),
            ("special/fifo", "named pipe"),
            ("special/null", "character device"),
        ];
        let mut seen = 0;
        for block in blocks(ZIP).chain(blocks(SEVEN_Z)).chain(blocks(TAR)) {
            let path = block.lines().find_map(|line| line.strip_prefix("Path = ")).unwrap();
            let result = parse_listing(block);
            match refused.iter().find(|(refused, _)| *refused == path) {
                Some((_, kind)) => {
                    let error = result.unwrap_err().to_string();
                    assert!(error.contains(kind), "{path}: {error}");
                    seen += 1;
                }
                None => {
                    let [entry] = result.unwrap().try_into().unwrap_or_else(|_| panic!("{path}"));
                    assert_eq!(entry.path, Path::new(path));
                    assert_eq!(entry.is_dir, matches!(path, "tree" | "special"), "{path}");
                }
            }
        }
        assert_eq!(seen, 7, "two links each in zip and 7z, three non-files in tar");

        // Types the fixtures could not carry (a block device needs one to
        // exist; tar skips sockets), in the exact shapes 7zz and p7zip print.
        for (attributes, kind) in [
            ("Attributes =  brw-rw----", "block device"),
            ("Attributes = A srwxr-xr-x", "socket"),
            ("Attributes = _ prw-r--r--", "named pipe"),
            ("Attributes = D_ lrwxrwxrwx", "symbolic link"),
            ("Mode = crw-rw-rw-", "character device"),
            ("Mode = brw-rw----", "block device"),
        ] {
            let listing = format!("Path = entry\nSize = 0\n{attributes}\n");
            let error = parse_listing(&listing).unwrap_err().to_string();
            assert!(error.contains(kind), "{attributes}: {error}");
        }
        // Windows flags alone are not a mode, and a `D` flag is still a folder.
        let entries = parse_listing("Path = made-on-windows\nSize = 0\nAttributes = A\n").unwrap();
        assert!(!entries[0].is_dir);
        let entries = parse_listing("Path = folder\nSize = 0\nAttributes = D\n").unwrap();
        assert!(entries[0].is_dir);
        let entries =
            parse_listing("Path = folder\nSize = 0\nAttributes = _ drwxr-xr-x\n").unwrap();
        assert!(entries[0].is_dir);
    }

    /// The stem names a folder the user did not choose, so it must be a name
    /// the user could have chosen.
    #[test]
    fn archive_stem_never_yields_a_name_marcel_would_refuse() {
        for name in ["..zip", "...zip", ".marcel-copy-x.zip", " .zip", "..tar.gz"] {
            assert_eq!(archive_stem(Path::new(name)), "Archive", "{name}");
        }
        assert_eq!(archive_stem(Path::new(".hidden.zip")), ".hidden");
        assert_eq!(archive_stem(Path::new("a.b.zip")), "a.b");
        assert_eq!(archive_stem(Path::new(".zip")), ".zip", "no stem to peel is not an error");
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

    /// A policy that answers every conflict the same way.
    fn answering(response: ConflictResponse) -> ConflictPolicy {
        struct Always(ConflictResponse);
        impl ConflictResolver for Always {
            fn resolve(&self, _: &ConflictRequest) -> ConflictDecision {
                ConflictDecision::once(self.0.clone())
            }
        }
        ConflictPolicy::interactive(Arc::new(Always(response)))
    }

    /// An occupied destination is a question, answered the way a copy's is:
    /// keep both under a free name, give up, or hold the occupant aside so
    /// undo can bring it back. Whatever the answer, staging leaves no trace.
    #[test]
    fn extraction_answers_an_occupied_destination_like_a_copy() {
        let sandbox = Sandbox::new();
        let archive = sandbox.file("bundle.zip", b"archive");
        let occupied = sandbox.file("file.txt", b"occupied");

        let outcome = extract_archive_with(
            &Fake::One,
            &archive,
            no_cancel(),
            &mut answering(ConflictResponse::AutoRename),
        )
        .unwrap();
        assert_eq!(outcome.published, sandbox.path("file (2).txt"));
        assert_eq!(read(&outcome.published), b"test");
        assert_eq!(read(&occupied), b"occupied");
        assert!(outcome.replaced.is_empty());
        fs::remove_file(&outcome.published).unwrap();

        let error = extract_archive_with(
            &Fake::One,
            &archive,
            no_cancel(),
            &mut answering(ConflictResponse::Skip),
        )
        .unwrap_err();
        assert!(error.to_string().contains("nothing was written"), "{error}");
        assert_eq!(sandbox.names(""), ["bundle.zip", "file.txt"]);

        let outcome = extract_archive_with(
            &Fake::One,
            &archive,
            no_cancel(),
            &mut answering(ConflictResponse::Replace),
        )
        .unwrap();
        assert_eq!(outcome.published, occupied);
        assert_eq!(read(&occupied), b"test");
        let [displaced] = outcome.replaced.as_slice() else {
            panic!("the occupant should be held aside");
        };
        assert_eq!(read(displaced.quarantine()), b"occupied");
        assert!(is_replacement_quarantine_name(displaced.quarantine().file_name().unwrap()));
        erase_replacement_quarantine(displaced);
        assert_eq!(sandbox.names(""), ["bundle.zip", "file.txt"]);
    }

    /// Two folders meeting is a merge: the folder that was there keeps
    /// everything it had and gains what the archive adds.
    #[test]
    fn extraction_merges_into_an_existing_folder_when_asked() {
        let sandbox = Sandbox::new();
        let archive = sandbox.file("bundle.zip", b"archive");
        sandbox.file("bundle/existing.txt", b"keep");
        sandbox.file("bundle/one.txt", b"mine");

        let outcome = extract_archive_with(
            &Fake::Multiple,
            &archive,
            no_cancel(),
            &mut answering(ConflictResponse::Replace),
        )
        .unwrap();
        assert_eq!(outcome.published, sandbox.path("bundle"));
        assert_eq!(read(sandbox.path("bundle/existing.txt")), b"keep");
        assert_eq!(read(sandbox.path("bundle/one.txt")), b"mine", "a merge keeps what is there");
        assert_eq!(read(sandbox.path("bundle/two.txt")), b"two");
        let merged = outcome.merged.expect("a merge records what it added");
        assert!(merged.undoable);
        assert_eq!(merged.added.len(), 1);
        assert_eq!(sandbox.names(""), ["bundle", "bundle.zip"], "staging must be cleaned up");
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

    /// An archive whose one entry is named like Marcel's working files would
    /// be published straight into hiding, and possibly swept; the archive's
    /// own stem gets the same scrutiny when it names the folder.
    #[test]
    fn extraction_refuses_to_publish_under_a_reserved_name() {
        let sandbox = Sandbox::new();
        let archive = sandbox.file("bundle.zip", b"archive");
        let error = extract(Fake::ReservedName, &archive).unwrap_err().to_string();
        assert!(error.contains("reserved") && error.contains(".marcel-copy-x"), "{error}");
        assert_eq!(sandbox.names(""), ["bundle.zip"], "staging must be cleaned up");

        let archive = sandbox.file("..zip", b"archive");
        let outcome = extract(Fake::Multiple, &archive).unwrap();
        assert_eq!(outcome.published, sandbox.path("Archive"));
        assert_eq!(read(outcome.published.join("two.txt")), b"two");
    }

    /// Staging is private while the backend writes; the folder that comes out
    /// of it is the user's and gets the mode a folder they made would.
    #[test]
    fn multi_entry_extraction_publishes_a_folder_with_a_normal_mode() {
        let sandbox = Sandbox::new();
        let archive = sandbox.file("bundle.zip", b"archive");
        let outcome = extract(Fake::Multiple, &archive).unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let probe = sandbox.dir("made-by-hand");
        assert_eq!(mode(&outcome.published), mode(&probe), "not the 0700 of staging");
    }

    /// The private runtime path the installed wrapper sets must not reach a
    /// 7-Zip linked against another glibc.
    #[test]
    fn backend_process_does_not_inherit_the_private_library_path() {
        let backend = SevenZipBackend::from_program(PathBuf::from("/nonexistent/7zz"));
        let command = backend.command();
        let removed = command
            .get_envs()
            .any(|(key, value)| key == OsStr::new("LD_LIBRARY_PATH") && value.is_none());
        assert!(removed);
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
        let outcome =
            extract_archive_with(&backend, &archive, no_cancel(), &mut ConflictPolicy::refusing())
                .unwrap();

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
            let outcome = extract_archive_with(
                &backend,
                &sandbox.path(archive),
                no_cancel(),
                &mut ConflictPolicy::refusing(),
            )
            .unwrap();
            assert_eq!(&outcome.published, member);
            assert_eq!(read(outcome.published), contents);
        }
    }

    /// 7-Zip treats `*` and `?` in a name as patterns unless told not to, and
    /// `--` does not tell it. A file called `*.txt` must archive alone, and an
    /// archive called `*.zip` must extract alone.
    #[test]
    fn official_backend_takes_wildcard_names_literally_when_available() {
        let Some((backend, _)) = official_backend() else {
            return;
        };
        let sandbox = Sandbox::new();
        let star = sandbox.file("*.txt", b"star");
        sandbox.file("z.txt", b"secret");
        sandbox.file("a?.txt", b"question");
        let archive = sandbox.path("*.zip");
        create_zip_archive_with(&backend, std::slice::from_ref(&star), &archive, no_cancel())
            .unwrap();
        let listed = backend.list(&archive, no_cancel()).unwrap();
        assert_eq!(
            listed.iter().map(|entry| entry.path.as_path()).collect::<Vec<_>>(),
            [Path::new("*.txt")]
        );

        let other = sandbox.file("other.txt", b"other");
        let other_archive = sandbox.path("other.zip");
        create_zip_archive_with(
            &backend,
            std::slice::from_ref(&other),
            &other_archive,
            no_cancel(),
        )
        .unwrap();
        fs::remove_file(&star).unwrap();
        fs::remove_file(&other).unwrap();
        let outcome =
            extract_archive_with(&backend, &archive, no_cancel(), &mut ConflictPolicy::refusing())
                .unwrap();
        assert_eq!(outcome.published, star);
        assert_eq!(read(&star), b"star");
        assert!(!other.exists(), "the sibling archive was not extracted too");
        assert_eq!(sandbox.names(""), ["*.txt", "*.zip", "a?.txt", "other.zip", "z.txt"]);
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
