use std::{
    ffi::{OsStr, OsString},
    fs, io,
    io::Read as _,
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::{Context as _, Result, bail};

/// Open the user's terminal at `directory`. Blocks on the spawn, so call it
/// off the foreground.
pub fn open_terminal(directory: &Path) -> Result<()> {
    if !directory.is_dir() {
        bail!("Cannot open a terminal because “{}” is not a directory", directory.display());
    }

    // Prefer the proposed cross-desktop default-terminal interface before
    // falling back to individual emulators:
    // https://github.com/Vladimir-csp/xdg-terminal-exec
    let launcher = find_in_path(OsStr::new(XDG_TERMINAL_EXEC), std::env::var_os("PATH").as_deref());
    let arguments = xdg_terminal_exec_arguments(launcher.as_deref(), directory);
    if spawn(XDG_TERMINAL_EXEC, arguments.iter().map(OsString::as_os_str), directory)? {
        return Ok(());
    }

    if let Some(terminal) = std::env::var_os("TERMINAL")
        && !terminal.is_empty()
        && spawn(&terminal, std::iter::empty::<&OsStr>(), directory)?
    {
        return Ok(());
    }

    for (program, arguments) in terminal_fallbacks(directory) {
        if spawn(program, arguments.iter().map(OsString::as_os_str), directory)? {
            return Ok(());
        }
    }

    bail!(
        "No terminal launcher was found. Install `xdg-terminal-exec` or set the TERMINAL environment variable"
    )
}

fn spawn<I, S>(program: S, arguments: I, directory: &Path) -> Result<bool>
where
    I: IntoIterator,
    I::Item: AsRef<OsStr>,
    S: AsRef<OsStr>,
{
    // The shared builder keeps the Nix shell's private LD_LIBRARY_PATH out of
    // an independently packaged terminal; `tool::command` explains the hazard.
    let mut command = crate::preview::tool::command(program);
    command
        .args(arguments)
        .current_dir(directory)
        // `nix develop` replaces SHELL with its build shell (normally Bash).
        // Let the terminal resolve the user's configured login shell instead
        // of inheriting that implementation detail from Marcel.
        .env_remove("SHELL")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match command.spawn() {
        Ok(child) => {
            reap(child);
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("launching a terminal"),
    }
}

/// Wait for a launched terminal, elsewhere, so it does not become a zombie.
///
/// The child deliberately outlives the request that started it — a terminal
/// stays open for as long as the user wants one — so nothing on the operation
/// path can wait for it. Marcel is its parent regardless, and a parent that
/// never waits leaves an entry in the process table for the life of the
/// application. One detached thread per launch ends when that terminal closes.
fn reap(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

const XDG_TERMINAL_EXEC: &str = "xdg-terminal-exec";
/// The launcher is a shell script of a few hundred lines; anything larger is
/// not the script this check knows how to read.
const LAUNCHER_READ_LIMIT: u64 = 1024 * 1024;

/// What to hand `xdg-terminal-exec`. Releases before 0.12 have no `--dir`
/// option and treat the first argument as the command to run, so the
/// terminal opens, fails to execute `--dir=/some/folder`, and exits while
/// `spawn` has already reported success. The launcher is a script, so
/// whether it documents the option is readable; a launcher that does not, or
/// that cannot be read, gets no arguments and inherits the working directory.
fn xdg_terminal_exec_arguments(launcher: Option<&Path>, directory: &Path) -> Vec<OsString> {
    if launcher.is_some_and(|launcher| mentions(launcher, b"--dir=")) {
        let mut option = OsString::from("--dir=");
        option.push(directory.as_os_str());
        vec![option]
    } else {
        Vec::new()
    }
}

fn mentions(path: &Path, needle: &[u8]) -> bool {
    let Ok(file) = fs::File::open(path) else { return false };
    let mut contents = Vec::new();
    if file.take(LAUNCHER_READ_LIMIT).read_to_end(&mut contents).is_err() {
        return false;
    }
    contents.windows(needle.len()).any(|window| window == needle)
}

/// Where `spawn` will find `program`: the first regular file of that name on
/// `search_path`, as `execvp` resolves it.
fn find_in_path(program: &OsStr, search_path: Option<&OsStr>) -> Option<PathBuf> {
    let search_path = search_path?;
    std::env::split_paths(search_path)
        .filter(|directory| !directory.as_os_str().is_empty())
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

fn terminal_fallbacks(directory: &Path) -> Vec<(&'static str, Vec<OsString>)> {
    let path = directory.as_os_str();
    vec![
        ("kitty", vec!["--directory".into(), path.into()]),
        ("ghostty", vec!["--working-directory".into(), path.into()]),
        ("konsole", vec!["--workdir".into(), path.into()]),
        ("gnome-terminal", vec!["--working-directory".into(), path.into()]),
        ("kgx", vec!["--working-directory".into(), path.into()]),
        ("ptyxis", vec!["--working-directory".into(), path.into()]),
        ("foot", vec!["--working-directory".into(), path.into()]),
        ("alacritty", vec!["--working-directory".into(), path.into()]),
        ("wezterm", vec!["start".into(), "--cwd".into(), path.into()]),
        ("xfce4-terminal", vec!["--working-directory".into(), path.into()]),
        // These inherit the process working directory without a dedicated
        // option.
        ("xterm", Vec::new()),
        ("urxvt", Vec::new()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_terminal_exec_gets_the_folder_only_when_it_documents_the_option() {
        let sandbox = crate::testing::Sandbox::new();
        let directory = Path::new("/tmp/a folder");
        let modern =
            sandbox.file("modern", "#!/bin/sh\n# --dir=DIR  start in DIR\nexec foot \"$@\"\n");
        let legacy = sandbox.file("legacy", "#!/bin/sh\nexec foot \"$@\"\n");
        let mut option = OsString::from("--dir=");
        option.push(directory.as_os_str());
        assert_eq!(xdg_terminal_exec_arguments(Some(&modern), directory), vec![option]);
        assert!(xdg_terminal_exec_arguments(Some(&legacy), directory).is_empty());
        assert!(xdg_terminal_exec_arguments(None, directory).is_empty());
        assert!(xdg_terminal_exec_arguments(Some(&sandbox.path("absent")), directory).is_empty());
    }

    #[test]
    fn the_launcher_is_looked_up_on_path_as_a_regular_file() {
        let sandbox = crate::testing::Sandbox::new();
        let bin = sandbox.dir("bin");
        sandbox.dir("bin/not-a-file");
        let launcher = sandbox.file("bin/launcher", "#!/bin/sh\n");
        let search_path = std::env::join_paths([sandbox.path("elsewhere"), bin.clone()]).unwrap();
        assert_eq!(find_in_path(OsStr::new("launcher"), Some(&search_path)), Some(launcher));
        assert_eq!(find_in_path(OsStr::new("not-a-file"), Some(&search_path)), None);
        assert_eq!(find_in_path(OsStr::new("launcher"), None), None);
    }

    #[test]
    fn fallback_arguments_keep_the_directory_as_one_os_argument() {
        let directory = Path::new("/tmp/a folder");
        let fallbacks = terminal_fallbacks(directory);
        let kitty = fallbacks.iter().find(|(program, _)| *program == "kitty").unwrap();
        assert_eq!(kitty.1, vec![OsString::from("--directory"), directory.as_os_str().into()]);
    }
}
