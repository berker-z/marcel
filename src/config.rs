//! Marcel's configuration directory, and the browser state it keeps there.
//!
//! Every file under it is written the same way: to a temporary file beside
//! the target, flushed, then renamed into place, so a crash mid-write can
//! never leave a half-written file behind.
//!
//! Reading is the guarded half. A file Marcel could not read is a file it
//! must not write either: the first save would replace whatever the user or
//! a newer Marcel put there with this process's defaults, and nothing would
//! be left to say that anything had been lost.

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

/// The most a file in the configuration directory may be before Marcel
/// refuses to read it. Its own files are a few lines; a megabyte there is
/// something else wearing the name, and reading it whole into memory on
/// every window open is not owed to it.
pub(crate) const MAX_FILE_SIZE: u64 = 1024 * 1024;

/// `$XDG_CONFIG_HOME/marcel/<name>`, falling back to `~/.config`.
///
/// The XDG base-directory spec says a relative value must be ignored; honoring
/// one would scatter the user's files across whatever directory Marcel
/// happened to be launched from.
pub(crate) fn path(home: &Path, name: &str) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"))
        .join("marcel")
        .join(name)
}

/// Read one of Marcel's own files: `None` when there is none yet, an error
/// when it cannot be read or is larger than [`MAX_FILE_SIZE`].
pub(crate) fn read_own_file(path: &Path) -> Result<Option<String>> {
    let size = match fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Could not read “{}”", path.display()));
        }
    };
    if size > MAX_FILE_SIZE {
        bail!(
            "“{}” is {size} bytes, larger than the {MAX_FILE_SIZE} bytes Marcel will read",
            path.display()
        );
    }
    fs::read_to_string(path)
        .map(Some)
        .with_context(|| format!("Could not read “{}”", path.display()))
}

/// Publish a file atomically, resolving a symlinked target so the rename
/// replaces what the link points at rather than the link itself (to a
/// dotfiles repository, say).
///
/// The temporary file is reserved atomically because every window runs its
/// own writer under the same process id; a predictable name would let two
/// saves truncate or rename each other's work.
pub(crate) fn write_atomically(
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
pub(crate) enum BrowserView {
    List,
    #[default]
    Grid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BrowserState {
    pub view: BrowserView,
    pub show_hidden: bool,
    pub sort: SortOrder,
    /// The sidebar folded away by the user. A window too narrow for it
    /// folds it on its own without writing this, so a drag to half the
    /// screen does not become a preference.
    pub sidebar_hidden: bool,
    /// The theme chosen in Settings. `None` until one has been: the
    /// environment's default (the Nix module's `settings.theme`) stays in
    /// force, and changing it there keeps working, until the user picks one
    /// in the dialog.
    pub theme: Option<Palette>,
}

impl Default for BrowserState {
    fn default() -> Self {
        Self {
            view: BrowserView::Grid,
            show_hidden: true,
            sort: SortOrder::default(),
            sidebar_hidden: false,
            theme: None,
        }
    }
}

pub(crate) fn load(path: &Path) -> Result<BrowserState> {
    let Some(contents) = read_own_file(path)? else {
        return Ok(BrowserState::default());
    };
    parse(&contents).with_context(|| format!("Invalid Marcel state in “{}”", path.display()))
}

/// The state file as one window sees it: what it starts from, and whether
/// it may write back.
///
/// A file that is missing is simply not written yet. A file that is there
/// but cannot be read — a torn edit, a permission problem, a version this
/// Marcel does not know — makes the window run on defaults for this session
/// and refuse every save, because the alternative is quietly replacing the
/// file with those defaults. The reason is kept so the window can say so.
pub(crate) struct StateFile {
    path: PathBuf,
    pub state: BrowserState,
    read_only: Option<String>,
}

impl StateFile {
    pub fn open(path: PathBuf) -> Self {
        let (state, read_only) = match load(&path) {
            Ok(state) => (state, None),
            Err(error) => (
                BrowserState::default(),
                Some(format!(
                    "{error:#}. Marcel is using its default view settings and will not save \
                     them until the file is fixed or removed."
                )),
            ),
        };
        Self { path, state, read_only }
    }

    /// Why saves are refused, when they are.
    pub fn read_only(&self) -> Option<&str> {
        self.read_only.as_deref()
    }

    /// Write `state`, unless the file could not be read: then nothing is
    /// written, so that what it holds survives for the user to look at.
    pub fn save(&self, state: BrowserState) -> Result<()> {
        if let Some(reason) = &self.read_only {
            bail!("{reason}");
        }
        save(&self.path, state)
    }
}

/// The theme the state file records, read before any window exists so the
/// first frame is already in it.
///
/// There is no window to tell yet when this read fails, so it falls back to
/// the environment's default in silence. Nothing is lost by that: every
/// window opens the same file through [`StateFile::open`], reports what is
/// wrong with it, and refuses to write over it.
pub fn chosen_theme() -> Option<Palette> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    load(&path(&home, STATE_FILE)).ok()?.theme
}

/// The one file the browser state lives in.
pub(crate) const STATE_FILE: &str = "state.conf";

pub(crate) fn save(path: &Path, state: BrowserState) -> Result<()> {
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
        writeln!(file, "sidebar={}", if state.sidebar_hidden { "hidden" } else { "shown" })?;
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
    let mut sidebar_hidden = false;
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
            "sidebar" => sidebar_hidden = value == "hidden",
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
        sidebar_hidden,
        theme,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Sandbox;

    #[test]
    fn missing_state_uses_grid_with_hidden_files_visible() {
        let sandbox = Sandbox::new();
        assert_eq!(load(&sandbox.path("missing")).unwrap(), BrowserState::default());
    }

    #[test]
    fn state_round_trips_atomically() {
        let sandbox = Sandbox::new();
        let path = sandbox.path("config/marcel/state.conf");
        let state = BrowserState {
            view: BrowserView::List,
            show_hidden: false,
            sort: SortOrder { key: SortKey::Modified, descending: true },
            sidebar_hidden: true,
            theme: Some(Palette::TokyoNight),
        };
        save(&path, state).unwrap();
        assert_eq!(load(&path).unwrap(), state);
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "version=1\nview=list\nshow_hidden=false\nsort=modified\nsort_direction=descending\nsidebar=hidden\ntheme=tokyo-night\n"
        );
    }

    /// A theme is written only once one has been chosen, so the environment's
    /// default keeps applying until then.
    #[test]
    fn an_unchosen_theme_is_not_written_and_a_missing_one_stays_unchosen() {
        let sandbox = Sandbox::new();
        let path = sandbox.path("state.conf");
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
        assert!(!state.sidebar_hidden);
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

    /// A state file this Marcel cannot read is left exactly as it was: the
    /// window runs on defaults, says why, and every save is refused.
    #[test]
    fn an_unreadable_state_file_is_reported_and_never_overwritten() {
        let sandbox = Sandbox::new();
        let corrupt = "version=2\nview=columns\n";
        let path = sandbox.file("marcel/state.conf", corrupt);

        let file = StateFile::open(path.clone());
        assert_eq!(file.state, BrowserState::default());
        let reason = file.read_only().expect("an unparseable file makes the state read-only");
        assert!(reason.contains("state.conf"), "{reason}");
        assert!(reason.contains("will not save"), "{reason}");

        let refused = file.save(BrowserState { show_hidden: false, ..BrowserState::default() });
        assert!(refused.is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), corrupt);
    }

    /// Not having a state file yet is the ordinary first run, not a failure:
    /// the first save creates it.
    #[test]
    fn a_missing_state_file_is_writable_and_created_on_save() {
        let sandbox = Sandbox::new();
        let path = sandbox.path("marcel/state.conf");

        let file = StateFile::open(path.clone());
        assert_eq!(file.read_only(), None);
        assert_eq!(file.state, BrowserState::default());

        let state = BrowserState { view: BrowserView::List, ..BrowserState::default() };
        file.save(state).unwrap();
        assert_eq!(load(&path).unwrap(), state);
    }

    /// A file far past anything Marcel writes is refused unread, with the
    /// size in the message, rather than pulled into memory to be parsed.
    #[test]
    fn an_oversized_file_is_refused_with_its_size() {
        let sandbox = Sandbox::new();
        let path = sandbox.path("marcel/state.conf");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = fs::File::create(&path).unwrap();
        file.set_len(MAX_FILE_SIZE + 1).unwrap();

        let error = read_own_file(&path).unwrap_err().to_string();
        assert!(error.contains(&(MAX_FILE_SIZE + 1).to_string()), "{error}");
        assert!(error.contains("larger than"), "{error}");
        assert!(StateFile::open(path).read_only().is_some());
    }
}
