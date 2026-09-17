//! The Marcel window.
//!
//! One `Marcel` is one window: the directory it browses, the preview beside
//! it, the sidebar, and the chrome around them. Each submodule owns one
//! concern of that window — its state and its behaviour together — and
//! `chrome` composes them into the frame:
//!
//! - `actions`: every command the window answers, whether it is enabled,
//!   and the keys that reach it.
//! - `navigation`: loading a folder or the Trash, watching it, moving
//!   through history, and folding operation effects back into the listing.
//! - `edits`: the window's side of every mutation — clipboard, rename, new
//!   folder, trash, delete, compress, extract, drag moves.
//! - `pointer`: marquee selection, drag and drop, and edge autoscroll.
//! - `preview`: what the preview pane loads and how it draws it.
//! - `picker`: the handful of things a file-chooser window does differently.
//! - `sidebar`, `browser`, `location`, `menu`, `dialogs`: the surfaces.
//!
//! A file-chooser window is not a separate UI. It is this same view with
//! `picker: Some(_)`, rendered by the same `render()`, browsing the same
//! `DirectorySession`; see `picker` for the whole list of what differs.

mod actions;
mod browser;
mod chrome;
mod dialogs;
mod edits;
mod location;
mod menu;
mod navigation;
mod picker;
mod pointer;
mod preview;
mod sidebar;
mod state;

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use gpui::prelude::*;
use gpui::{AnyWindowHandle, App, Context, Entity, FocusHandle, Subscription, Window};
use gpui_component::input::{InputEvent, InputState};

use crate::{
    bookmarks::BookmarkStore,
    browse::{directory_session::DirectorySession, entries::FileEntry, history::NavigationHistory},
    config::{self, BrowserState},
    operations::{OperationCoordinator, OperationEvent},
};

pub use actions::init_key_bindings;
use preview::PreviewState;
use state::{DragState, PickerState, SidebarState, UiState};

const DIRECTORY_ROW_HEIGHT: f32 = 36.0;
const GRID_TILE_WIDTH: f32 = 120.0;
const GRID_TILE_HEIGHT: f32 = 188.0;
const GRID_GAP: f32 = 8.0;
const GRID_SIDE_PADDING: f32 = 16.0;
const GRID_ROW_HEIGHT: f32 = GRID_TILE_HEIGHT + GRID_GAP;
const MIN_BROWSER_WIDTH: f32 = 360.0;
const MIN_PREVIEW_WIDTH: f32 = 280.0;
const MAX_PREVIEW_WIDTH: f32 = 900.0;
const POINTER_EDGE_SCROLL_INTERVAL: Duration = Duration::from_millis(16);

pub struct Marcel {
    pub(crate) browser_focus: FocusHandle,
    pub(crate) home_dir: PathBuf,
    pub(crate) directory: DirectorySession,
    /// The application's operation owner, not this window's.
    ///
    /// A window starts operations, shows their questions and reports, and folds
    /// their effects into its own projection. Closing it neither orphans work
    /// nor discards history.
    pub(crate) operations: Entity<OperationCoordinator>,
    /// The user's bookmarks, shared so that two windows cannot write stale
    /// lists over each other.
    pub(crate) bookmarks: Entity<BookmarkStore>,
    _shared: [Subscription; 3],
    pub(crate) drag: DragState,
    pub(crate) preview: PreviewState,
    pub(crate) ui: UiState,
    pub(crate) history: NavigationHistory,
    pub(crate) sidebar: SidebarState,
    /// Present when this window is a file chooser answering a portal request.
    pub(crate) picker: Option<PickerState>,
}

impl Marcel {
    pub fn new(start_dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let start_dir = normalize_start_directory(start_dir);
        let home_dir =
            std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| start_dir.clone());
        let search_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Filter current folder").clean_on_escape()
        });
        let location_input = cx.new(|cx| InputState::new(window, cx).placeholder("Enter a path"));
        let subscriptions = [
            cx.subscribe_in(
                &search_input,
                window,
                |this, input, event: &InputEvent, window, cx| {
                    this.on_search_input_event(input, event, window, cx);
                },
            ),
            cx.subscribe_in(
                &location_input,
                window,
                |this, input, event: &InputEvent, window, cx| {
                    this.on_location_input_event(input, event, window, cx);
                },
            ),
        ];
        let mono_font_size = gpui_component::ActiveTheme::theme(&**cx).mono_font_size;
        let state_path = config::path(&home_dir, "state.conf");
        let browser_state = config::load(&state_path).unwrap_or_else(|error| {
            eprintln!("Could not load Marcel browser state: {error:#}");
            BrowserState::default()
        });
        let (state_save_sender, state_save_receiver) = async_channel::unbounded();
        let state_save_task = cx.background_executor().spawn(async move {
            while let Ok(browser_state) = state_save_receiver.recv().await {
                if let Err(error) = config::save(&state_path, browser_state) {
                    eprintln!("Could not save Marcel browser state: {error:#}");
                }
            }
        });
        let mut directory = DirectorySession::new(start_dir.clone());
        directory.show_hidden = browser_state.show_hidden;

        // Effects arrive from the application, so a mutation another window
        // started still reconciles here, and progress it is running still
        // redraws here.
        let operations = crate::operations::global(cx);
        let bookmarks = crate::bookmarks::global(&home_dir, cx);
        let shared = [
            cx.observe(&operations, |_, _, cx| cx.notify()),
            cx.observe(&bookmarks, |_, _, cx| cx.notify()),
            cx.subscribe_in(&operations, window, |this, _, event: &OperationEvent, window, cx| {
                this.on_operation_event(event, window, cx);
            }),
        ];

        let mut this = Self {
            browser_focus: cx.focus_handle(),
            home_dir: home_dir.clone(),
            directory,
            operations,
            bookmarks,
            _shared: shared,
            drag: DragState::default(),
            preview: PreviewState::new(mono_font_size),
            ui: UiState::new(
                search_input,
                location_input,
                subscriptions,
                &browser_state,
                state_save_sender,
                state_save_task,
            ),
            history: NavigationHistory::new(start_dir),
            sidebar: SidebarState::new(&home_dir),
            picker: None,
        };
        this.start_places_load(home_dir, cx);
        this.start_directory_load(true, cx);
        this
    }

    pub fn focus_browser(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.browser_focus.focus(window, cx);
    }

    pub(crate) fn origin(window: &Window) -> AnyWindowHandle {
        window.window_handle()
    }

    /// Hand work to the application's operation owner on this window's behalf.
    pub(crate) fn with_operations(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
        start: impl FnOnce(
            &mut OperationCoordinator,
            AnyWindowHandle,
            &mut Context<OperationCoordinator>,
        ),
    ) {
        let origin = Self::origin(window);
        self.operations.clone().update(cx, |operations, cx| start(operations, origin, cx));
    }

    pub(crate) fn operations_busy(&self, cx: &App) -> bool {
        self.operations.read(cx).is_busy()
    }

    /// The entry the preview and single-item commands act on.
    pub(crate) fn primary_entry(&self) -> Option<&FileEntry> {
        self.directory.selection.primary().and_then(|path| self.directory.entry(path))
    }

    /// The selected paths in visible order, which is the order every
    /// operation — and a picker's caller — receives them in.
    pub(crate) fn selected_paths(&self) -> Vec<PathBuf> {
        let selected = self.directory.selection.selected();
        self.directory.visible_paths().into_iter().filter(|path| selected.contains(path)).collect()
    }

    pub(crate) fn has_selection(&self) -> bool {
        !self.directory.selection.selected().is_empty()
    }

    pub(crate) fn is_directory_entry(&self, path: &Path) -> bool {
        self.directory
            .entry(path)
            .is_some_and(|entry| entry.kind == crate::browse::entries::EntryKind::Directory)
    }

    pub(crate) fn persist_browser_state(&self) {
        let _ = self.ui.state_save_sender.try_send(BrowserState {
            view: self.ui.view_mode.to_state(),
            show_hidden: self.directory.show_hidden,
        });
    }
}

fn normalize_start_directory(path: PathBuf) -> PathBuf {
    if path.is_dir() {
        path
    } else {
        path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"))
    }
}

/// A `FileEntry` for tests that only need a path and a kind.
#[cfg(test)]
pub(crate) fn test_file_entry(path: &str, navigable: bool) -> FileEntry {
    use crate::browse::entries::EntryKind;

    let path = PathBuf::from(path);
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    FileEntry {
        path,
        name: name.clone(),
        name_os: name.clone().into(),
        folded_name: name.to_lowercase().chars().collect(),
        kind: if navigable { EntryKind::Directory } else { EntryKind::File },
        navigable,
        size: Some(0),
        icon_path: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_path_falls_back_to_parent_for_files() {
        let path = PathBuf::from("/tmp/file.txt");
        assert_eq!(normalize_start_directory(path), PathBuf::from("/tmp"));
    }
}
