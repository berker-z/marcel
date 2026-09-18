//! Running an external tool for a preview: Poppler for PDFs, ffmpeg for
//! video. The rules every bridge shares live here so each one is only the
//! command line and the parsing.
//!
//! A tool runs with a deadline and under the preview's cancellation flag,
//! its output is read back bounded, its failure is worded with whatever it
//! said on stderr, and what it produced lands in a private cache directory
//! keyed by the source's identity and pruned by age.
//!
//! [`command`] is the one place a child process is set up, for previews and
//! for everything else Marcel spawns.

use std::{
    ffi::OsStr,
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    os::unix::process::CommandExt as _,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

use md5::{Digest, Md5};
use rustix::process::{Pid, Signal, kill_process_group};

const POLL_INTERVAL: Duration = Duration::from_millis(15);
const MAX_OUTPUT_BYTES: u64 = 64 * 1024;
const MAX_CACHE_FILES: usize = 512;

/// A `Command` for a program Marcel runs on its own behalf: a preview tool,
/// an archiver, a desktop handler.
///
/// The Nix development shell and the installed wrapper hand Marcel its
/// native libraries through `LD_LIBRARY_PATH`. An independently packaged
/// program that inherits that private search path can pick up the wrong
/// glibc or graphics stack and die before it starts, so the variable is
/// dropped here once rather than remembered at every spawn. stdin is closed
/// so a tool that wants a terminal reads EOF instead of hanging on Marcel's,
/// and the child gets its own process group so [`terminate`] takes anything
/// it forked along with it.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command.env_remove("LD_LIBRARY_PATH").stdin(Stdio::null()).process_group(0);
    command
}

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

/// Kill a child started through [`command`] and reap it. The signal goes to
/// its process group, not the one pid, so a helper the tool forked does not
/// outlive it; `kill` afterwards covers a child that never got a group.
pub(super) fn terminate(child: &mut Child) {
    let _ = kill_process_group(Pid::from_child(child), Signal::KILL);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything the builder promises, seen from inside the child: no
    /// `LD_LIBRARY_PATH`, EOF on stdin, and a process group of its own.
    #[test]
    fn a_command_is_stripped_of_the_library_path_and_detached() {
        let mut command = command("sh");
        // The removal is recorded on the command whether or not this
        // process has the variable, so it holds under a plain `cargo test`.
        assert!(
            command.get_envs().any(|(key, value)| key == "LD_LIBRARY_PATH" && value.is_none()),
            "the library path is explicitly removed"
        );
        command
            .args(["-c", "printf '%s|%s' \"${LD_LIBRARY_PATH-unset}\" \"$(cat)\""])
            .stdout(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let pid = Pid::from_child(&child);
        assert_eq!(rustix::process::getpgid(Some(pid)).unwrap(), pid, "leads its own group");
        let mut out = String::new();
        child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(out, "unset|", "no library path, and stdin already at EOF");
    }

    /// Killing the group takes a grandchild the tool forked along, which a
    /// plain `kill` on the child's pid would leave running.
    #[test]
    fn terminate_kills_the_whole_group() {
        let mut command = command("sh");
        command.args(["-c", "sleep 30 & echo $!; wait"]).stdout(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let mut line = String::new();
        let mut stdout = io::BufReader::new(child.stdout.take().unwrap());
        io::BufRead::read_line(&mut stdout, &mut line).unwrap();
        let grandchild: i32 = line.trim().parse().unwrap();

        terminate(&mut child);

        // The sleep is reparented when the shell dies and reaped by whoever
        // inherits it, so allow a moment for that; gone or a zombie both do.
        let gone = || {
            fs::read_to_string(format!("/proc/{grandchild}/stat"))
                .map(|stat| stat.contains(") Z "))
                .unwrap_or(true)
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !gone() {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(gone(), "the grandchild outlived the tool");
    }
}
