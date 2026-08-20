use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};

const STATE_VERSION: u32 = 1;

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
        Self {
            view: BrowserView::Grid,
            show_hidden: true,
        }
    }
}

pub fn default_path(home: &Path) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        // The XDG base-directory spec says a relative value must be ignored.
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"))
        .join("marcel")
        .join("state.conf")
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
    // Resolve a symlinked state file to its target: `persist` is a rename,
    // which would otherwise replace the user's link with a regular file.
    let path = &fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let parent = path
        .parent()
        .context("State file has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("Could not create “{}”", parent.display()))?;
    // A per-process temporary name collided between windows, because every
    // window runs its own writer. Reserve the temporary file atomically so
    // concurrent saves cannot truncate or rename each other's work.
    let mut file = tempfile::NamedTempFile::new_in(parent).with_context(|| {
        format!(
            "Could not create a temporary file in “{}”",
            parent.display()
        )
    })?;
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
    file.as_file()
        .sync_all()
        .with_context(|| format!("Could not flush state for “{}”", path.display()))?;
    file.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("Could not update “{}”", path.display()))?;
    sync_directory(parent);
    Ok(())
}

/// Flush the rename itself, not only the file contents. Without this the
/// publication is atomic but not crash-durable.
pub(crate) fn sync_directory(path: &Path) {
    if let Ok(directory) = fs::File::open(path) {
        let _ = directory.sync_all();
    }
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
        let (key, value) = line
            .split_once('=')
            .with_context(|| format!("State line has no '=': {line:?}"))?;
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
        assert_eq!(
            load(&root.path().join("missing")).unwrap(),
            BrowserState::default()
        );
    }

    #[test]
    fn state_round_trips_atomically() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config/marcel/state.conf");
        let state = BrowserState {
            view: BrowserView::List,
            show_hidden: false,
        };
        save(&path, state).unwrap();
        assert_eq!(load(&path).unwrap(), state);
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "version=1\nview=list\nshow_hidden=false\n"
        );
    }

    #[test]
    fn malformed_or_future_state_is_rejected() {
        assert!(parse("version=1\nview=columns\nshow_hidden=true\n").is_err());
        assert!(parse("version=2\nview=grid\nshow_hidden=true\n").is_err());
    }
}
