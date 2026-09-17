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

use crate::{
    browse::entries::{SortKey, SortOrder},
    theme::Palette,
};

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
    pub sort: SortOrder,
    /// The theme chosen in Settings. `None` until one has been: the
    /// environment's default (the Nix module's `settings.theme`) stays in
    /// force, and changing it there keeps working, until the user picks one
    /// in the dialog.
    pub theme: Option<Palette>,
}

impl Default for BrowserState {
    fn default() -> Self {
        Self { view: BrowserView::Grid, show_hidden: true, sort: SortOrder::default(), theme: None }
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

/// The theme the state file records, read before any window exists so the
/// first frame is already in it. Every window later loads the same file and
/// reports what is wrong with it; this read stays quiet.
pub fn chosen_theme() -> Option<Palette> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    load(&path(&home, STATE_FILE)).ok()?.theme
}

/// The one file the browser state lives in.
pub const STATE_FILE: &str = "state.conf";

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
        writeln!(file, "sort={}", state.sort.key.name())?;
        writeln!(
            file,
            "sort_direction={}",
            if state.sort.descending { "descending" } else { "ascending" }
        )?;
        if let Some(theme) = state.theme {
            writeln!(file, "theme={}", theme.name())?;
        }
        Ok(())
    })
}

/// Read a state file. `view` and `show_hidden` have been there since version
/// 1 and are required; the keys added later default when absent, so a file
/// an older Marcel wrote still loads, and one this Marcel wrote still loads
/// in an older one, which ignores what it does not know.
fn parse(contents: &str) -> Result<BrowserState> {
    let mut version = None;
    let mut view = None;
    let mut show_hidden = None;
    let mut sort = SortOrder::default();
    let mut theme = None;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) =
            line.split_once('=').with_context(|| format!("State line has no '=': {line:?}"))?;
        let value = value.trim();
        match key.trim() {
            "version" => version = value.parse::<u32>().ok(),
            "view" => {
                view = match value {
                    "list" => Some(BrowserView::List),
                    "grid" => Some(BrowserView::Grid),
                    _ => None,
                }
            }
            "show_hidden" => show_hidden = value.parse::<bool>().ok(),
            "sort" => sort.key = SortKey::from_name(value).unwrap_or_default(),
            "sort_direction" => sort.descending = value == "descending",
            "theme" => theme = Palette::from_name(value),
            _ => {}
        }
    }

    if version != Some(STATE_VERSION) {
        bail!("Unsupported or missing state version");
    }
    Ok(BrowserState {
        view: view.context("Missing or invalid view")?,
        show_hidden: show_hidden.context("Missing or invalid show_hidden")?,
        sort,
        theme,
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
        let state = BrowserState {
            view: BrowserView::List,
            show_hidden: false,
            sort: SortOrder { key: SortKey::Modified, descending: true },
            theme: Some(Palette::TokyoNight),
        };
        save(&path, state).unwrap();
        assert_eq!(load(&path).unwrap(), state);
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "version=1\nview=list\nshow_hidden=false\nsort=modified\nsort_direction=descending\ntheme=tokyo-night\n"
        );
    }

    /// A theme is written only once one has been chosen, so the environment's
    /// default keeps applying until then.
    #[test]
    fn an_unchosen_theme_is_not_written_and_a_missing_one_stays_unchosen() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.conf");
        save(&path, BrowserState::default()).unwrap();
        let written = fs::read_to_string(&path).unwrap();
        assert!(!written.contains("theme="), "{written}");
        assert_eq!(load(&path).unwrap().theme, None);
    }

    /// The keys added after version 1 default when absent, and an unknown
    /// value for one is the default rather than a refusal to load anything.
    #[test]
    fn a_version_one_file_without_the_newer_keys_still_loads() {
        let state = parse("version=1\nview=grid\nshow_hidden=true\n").unwrap();
        assert_eq!(state.sort, SortOrder::default());
        assert_eq!(state.theme, None);

        let state =
            parse("version=1\nview=grid\nshow_hidden=true\nsort=colour\ntheme=none\n").unwrap();
        assert_eq!(state.sort, SortOrder::default());
        assert_eq!(state.theme, None);
    }

    #[test]
    fn malformed_or_future_state_is_rejected() {
        assert!(parse("version=1\nview=columns\nshow_hidden=true\n").is_err());
        assert!(parse("version=2\nview=grid\nshow_hidden=true\n").is_err());
    }
}
