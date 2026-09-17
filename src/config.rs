//! Marcel's configuration directory, and the browser state it keeps there.
//!
//! Every file under it is written the same way: to a temporary file beside
//! the target, flushed, then renamed into place, so a crash mid-write can
//! never leave a half-written file behind.

use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};

const STATE_VERSION: u32 = 1;

/// `$XDG_CONFIG_HOME/marcel/<name>`, falling back to `~/.config`.
///
/// The XDG base-directory spec says a relative value must be ignored; honoring
/// one would scatter the user's files across whatever directory Marcel
/// happened to be launched from.
pub fn path(home: &Path, name: &str) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"))
        .join("marcel")
        .join(name)
}

/// Publish a file atomically, resolving a symlinked target so the rename
/// replaces what the link points at rather than the link itself (to a
/// dotfiles repository, say).
///
/// The temporary file is reserved atomically because every window runs its
/// own writer under the same process id; a predictable name would let two
/// saves truncate or rename each other's work.
pub fn write_atomically(
    path: &Path,
    write: impl FnOnce(&mut fs::File) -> Result<()>,
) -> Result<()> {
    let path = &fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let parent = path.parent().context("Configuration file has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("Could not create “{}”", parent.display()))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("Could not create a temporary file in “{}”", parent.display()))?;
    write(file.as_file_mut())?;
    file.as_file().sync_all().with_context(|| format!("Could not flush “{}”", path.display()))?;
    file.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("Could not update “{}”", path.display()))?;
    // Flush the rename itself, not only the file contents. Without this the
    // publication is atomic but not crash-durable.
    if let Ok(directory) = fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BrowserView {
    List,
    #[default]
    Grid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BrowserState {
    pub view: BrowserView,
    pub show_hidden: bool,
}

impl Default for BrowserState {
    fn default() -> Self {
        Self { view: BrowserView::Grid, show_hidden: true }
    }
}

pub fn load(path: &Path) -> Result<BrowserState> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BrowserState::default());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Could not read state from “{}”", path.display()));
        }
    };
    parse(&contents).with_context(|| format!("Invalid Marcel state in “{}”", path.display()))
}

pub fn save(path: &Path, state: BrowserState) -> Result<()> {
    write_atomically(path, |file| {
        writeln!(file, "version={STATE_VERSION}")?;
        writeln!(
            file,
            "view={}",
            match state.view {
                BrowserView::List => "list",
                BrowserView::Grid => "grid",
            }
        )?;
        writeln!(file, "show_hidden={}", state.show_hidden)?;
        Ok(())
    })
}

fn parse(contents: &str) -> Result<BrowserState> {
    let mut version = None;
    let mut view = None;
    let mut show_hidden = None;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) =
            line.split_once('=').with_context(|| format!("State line has no '=': {line:?}"))?;
        match key.trim() {
            "version" => version = value.trim().parse::<u32>().ok(),
            "view" => {
                view = match value.trim() {
                    "list" => Some(BrowserView::List),
                    "grid" => Some(BrowserView::Grid),
                    _ => None,
                }
            }
            "show_hidden" => show_hidden = value.trim().parse::<bool>().ok(),
            _ => {}
        }
    }

    if version != Some(STATE_VERSION) {
        bail!("Unsupported or missing state version");
    }
    Ok(BrowserState {
        view: view.context("Missing or invalid view")?,
        show_hidden: show_hidden.context("Missing or invalid show_hidden")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_state_uses_grid_with_hidden_files_visible() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(load(&root.path().join("missing")).unwrap(), BrowserState::default());
    }

    #[test]
    fn state_round_trips_atomically() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config/marcel/state.conf");
        let state = BrowserState { view: BrowserView::List, show_hidden: false };
        save(&path, state).unwrap();
        assert_eq!(load(&path).unwrap(), state);
        assert_eq!(fs::read_to_string(path).unwrap(), "version=1\nview=list\nshow_hidden=false\n");
    }

    #[test]
    fn malformed_or_future_state_is_rejected() {
        assert!(parse("version=1\nview=columns\nshow_hidden=true\n").is_err());
        assert!(parse("version=2\nview=grid\nshow_hidden=true\n").is_err());
    }
}
