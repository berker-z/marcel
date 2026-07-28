use std::{
    fs::File,
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

    let file_path = path.clone();
    let file = smol::unblock(move || File::open(&file_path))
        .await
        .with_context(|| format!("opening {}", path.display()))?;

    let portal_result = async {
        let request = OpenFileRequest::default()
            .ask(false)
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
            "could not open {} through `gio open` or the desktop portal: {error}",
            path.display()
        )
    })
}

async fn run_gio(path: &Path) -> Result<bool> {
    let result = smol::process::Command::new("gio")
        .arg("open")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;

    match result {
        Ok(status) => Ok(status.success()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("invoking `gio open`"),
    }
}
