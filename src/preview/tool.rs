//! Running an external tool for a preview: Poppler for PDFs, ffmpeg for
//! video. The rules every bridge shares live here so each one is only the
//! command line and the parsing.
//!
//! A tool runs with a deadline and under the preview's cancellation flag,
//! its output is read back bounded, its failure is worded with whatever it
//! said on stderr, and what it produced lands in a private cache directory
//! keyed by the source's identity and pruned by age.

use std::{
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

use md5::{Digest, Md5};

const POLL_INTERVAL: Duration = Duration::from_millis(15);
const MAX_OUTPUT_BYTES: u64 = 64 * 1024;
const MAX_CACHE_FILES: usize = 512;

/// Run `command` to completion, killing it when the preview is cancelled or
/// `timeout` passes. `what` names the preview in the errors ("PDF preview");
/// `absent` is the message when the program is not installed at all.
pub(super) fn run_child(
    command: &mut Command,
    cancelled: &AtomicBool,
    timeout: Duration,
    what: &str,
    absent: &str,
) -> io::Result<ExitStatus> {
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            io::Error::new(io::ErrorKind::NotFound, absent.to_string())
        } else {
            error
        }
    })?;
    let deadline = Instant::now() + timeout;

    loop {
        if cancelled.load(Ordering::Acquire) {
            terminate(&mut child);
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                format!("{what} was cancelled"),
            ));
        }
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            terminate(&mut child);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{what} exceeded the {} second time limit", timeout.as_secs()),
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

pub(super) fn check_cancelled(cancelled: &AtomicBool, what: &str) -> io::Result<()> {
    if cancelled.load(Ordering::Acquire) {
        Err(io::Error::new(io::ErrorKind::Interrupted, format!("{what} was cancelled")))
    } else {
        Ok(())
    }
}

/// A tool's captured output, at most 64 KiB of it.
///
/// `File::try_clone` duplicates the descriptor but shares its open-file
/// position, so the tool leaves the reader at EOF; rewind before reading.
pub(super) fn read_bounded(mut file: File) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.by_ref().take(MAX_OUTPUT_BYTES).read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub(super) fn tool_failure(tool: &str, status: ExitStatus, stderr: Vec<u8>) -> io::Error {
    let detail = String::from_utf8_lossy(&stderr);
    let detail = detail.trim();
    let message = if detail.is_empty() {
        format!("`{tool}` exited with {status}")
    } else {
        format!("`{tool}` exited with {status}: {detail}")
    };
    io::Error::other(message)
}

/// Whether `name` is an executable somewhere on `PATH`.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;

    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|directory| {
        let candidate = directory.join(name);
        fs::metadata(&candidate)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .then_some(candidate)
    })
}

/// `$XDG_CACHE_HOME/marcel/<name>`, falling back to `~/.cache`.
pub(super) fn cache_dir(name: &str) -> PathBuf {
    absolute_env_path("XDG_CACHE_HOME")
        .or_else(|| absolute_env_path("HOME").map(|home| home.join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("marcel")
        .join(name)
}

fn absolute_env_path(name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os(name)?);
    path.is_absolute().then_some(path)
}

/// A cache key for `path` as it is now: its path, length, and modification
/// time, plus `salt` for whatever the cached artefact depends on besides
/// the source (a render size, a format version).
pub(super) fn file_identity(path: &Path, salt: &[u8]) -> io::Result<String> {
    let metadata = path.metadata()?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let mut hash = Md5::new();
    hash.update(salt);
    hash.update(path.as_os_str().as_encoded_bytes());
    hash.update(metadata.len().to_le_bytes());
    hash.update(modified.to_le_bytes());
    Ok(format!("{:x}", hash.finalize()))
}

/// Keep the newest `MAX_CACHE_FILES` files in `cache_dir`.
pub(super) fn prune_cache(cache_dir: &Path) {
    let Ok(entries) = fs::read_dir(cache_dir) else {
        return;
    };
    let mut files = entries
        .flatten()
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            metadata.is_file().then(|| (metadata.modified().unwrap_or(UNIX_EPOCH), entry.path()))
        })
        .collect::<Vec<_>>();
    if files.len() <= MAX_CACHE_FILES {
        return;
    }

    files.sort_unstable_by_key(|(modified, _)| *modified);
    let remove_count = files.len() - MAX_CACHE_FILES;
    for (_, path) in files.into_iter().take(remove_count) {
        let _ = fs::remove_file(path);
    }
}
