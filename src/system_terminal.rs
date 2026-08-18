use std::{
    ffi::{OsStr, OsString},
    io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context as _, Result, bail};

pub async fn open_terminal(directory: PathBuf) -> Result<()> {
    smol::unblock(move || open_terminal_blocking(&directory)).await
}

fn open_terminal_blocking(directory: &Path) -> Result<()> {
    if !directory.is_dir() {
        bail!(
            "Cannot open a terminal because “{}” is not a directory",
            directory.display()
        );
    }

    // Prefer the proposed cross-desktop default-terminal interface before
    // falling back to individual emulators:
    // https://github.com/Vladimir-csp/xdg-terminal-exec
    let mut xdg_dir = OsString::from("--dir=");
    xdg_dir.push(directory.as_os_str());
    if spawn("xdg-terminal-exec", [xdg_dir.as_os_str()], directory)? {
        return Ok(());
    }

    if let Some(terminal) = std::env::var_os("TERMINAL")
        && !terminal.is_empty()
        && spawn(&terminal, std::iter::empty::<&OsStr>(), directory)?
    {
        return Ok(());
    }

    for (program, arguments) in terminal_fallbacks(directory) {
        if spawn(
            program,
            arguments.iter().map(OsString::as_os_str),
            directory,
        )? {
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
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(directory)
        // Do not leak the Nix development shell's private native-library
        // search path into an independently packaged terminal.
        .env_remove("LD_LIBRARY_PATH")
        // `nix develop` replaces SHELL with its build shell (normally Bash).
        // Let the terminal resolve the user's configured login shell instead
        // of inheriting that implementation detail from Marcel.
        .env_remove("SHELL")
        .stdin(Stdio::null())
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

fn terminal_fallbacks(directory: &Path) -> Vec<(&'static str, Vec<OsString>)> {
    let path = directory.as_os_str();
    vec![
        ("kitty", vec!["--directory".into(), path.into()]),
        ("ghostty", vec!["--working-directory".into(), path.into()]),
        ("konsole", vec!["--workdir".into(), path.into()]),
        (
            "gnome-terminal",
            vec!["--working-directory".into(), path.into()],
        ),
        ("kgx", vec!["--working-directory".into(), path.into()]),
        ("ptyxis", vec!["--working-directory".into(), path.into()]),
        ("foot", vec!["--working-directory".into(), path.into()]),
        ("alacritty", vec!["--working-directory".into(), path.into()]),
        ("wezterm", vec!["start".into(), "--cwd".into(), path.into()]),
        (
            "xfce4-terminal",
            vec!["--working-directory".into(), path.into()],
        ),
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
    fn fallback_arguments_keep_the_directory_as_one_os_argument() {
        let directory = Path::new("/tmp/a folder");
        let fallbacks = terminal_fallbacks(directory);
        let kitty = fallbacks
            .iter()
            .find(|(program, _)| *program == "kitty")
            .unwrap();
        assert_eq!(
            kitty.1,
            vec![OsString::from("--directory"), directory.as_os_str().into()]
        );
    }
}
