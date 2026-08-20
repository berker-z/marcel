use std::{
    os::fd::AsFd,
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::{Context, Result, anyhow};
use ashpd::desktop::open_uri::OpenFileRequest;

pub async fn open_file(path: PathBuf) -> Result<()> {
    // Opening an existing file from a file manager should honor its configured
    // MIME default without prompting. Unlike xdg-open's compositor-dependent
    // shell script, `gio open` consults the MIME application database directly.
    if run_gio(&path).await? {
        return Ok(());
    }

    open_through_portal(path, false).await
}

pub async fn open_file_with(path: PathBuf) -> Result<()> {
    open_through_portal(path, true).await
}

async fn open_through_portal(path: PathBuf, ask: bool) -> Result<()> {
    let file_path = path.clone();
    // A FIFO would block this open forever and strand the worker thread;
    // `open_regular_file` refuses anything but a regular file on the opened
    // descriptor itself.
    let file = smol::unblock(move || crate::local_fs::open_regular_file(&file_path))
        .await
        .with_context(|| format!("opening {}", path.display()))?;

    let portal_result = async {
        let request = OpenFileRequest::default()
            .ask(ask)
            .send_file(&file.as_fd())
            .await
            .context("sending the file to the desktop portal")?;
        request
            .response()
            .context("the desktop portal rejected the open request")
    }
    .await;

    portal_result.map_err(|error| {
        anyhow!(
            "could not open {} through the desktop portal: {error}",
            path.display()
        )
    })
}

async fn run_gio(path: &Path) -> Result<bool> {
    let mut command = smol::process::Command::new("gio");
    command
        .arg("open")
        // A path is data, never more options, whatever it starts with.
        .arg("--")
        .arg(path)
        // The Nix development shell supplies Marcel's native runtime libraries
        // through LD_LIBRARY_PATH. Letting that private search path leak into
        // a desktop handler can make an independently packaged application
        // load an incompatible glibc or graphics stack.
        .env_remove("LD_LIBRARY_PATH")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let result = command.status().await;

    match result {
        Ok(status) => Ok(status.success()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("invoking `gio open`"),
    }
}
