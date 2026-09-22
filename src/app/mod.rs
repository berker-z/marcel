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
//!   folder and file, duplicate, trash, delete, compress, extract, drag moves.
//! - `pointer`: marquee selection, drag and drop, and edge autoscroll.
//! - `preview`: what the preview pane loads and how it draws it.
//! - `properties`: the Properties dialog and the view that fills it in.
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
mod image_cache;
mod location;
mod media_pane;
mod menu;
mod navigation;
mod network;
mod picker;
mod pointer;
mod preview;
mod properties;
mod sidebar;
mod state;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use gpui::prelude::*;
use gpui::{AnyWindowHandle, App, Context, Entity, FocusHandle, Subscription, Window};
use gpui_component::input::{InputEvent, InputState};

use crate::{
    bookmarks::BookmarkStore,
    browse::{directory_session::DirectorySession, entries::FileEntry, history::NavigationHistory},
    config::{self, BrowserState},
    network::NetworkStore,
    operations::{OperationCoordinator, OperationEvent},
    surface::{self, Report},
    volumes::VolumeStore,
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
    /// The drives UDisks2 reports, one list for every window.
    pub(crate) volumes: Entity<VolumeStore>,
    /// Saved servers and the shares GVfs has connected, likewise shared.
    pub(crate) network: Entity<NetworkStore>,
    _shared: [Subscription; 5],
    pub(crate) drag: DragState,
    pub(crate) preview: PreviewState,
    pub(crate) ui: UiState,
    pub(crate) history: NavigationHistory,
    pub(crate) sidebar: SidebarState,
    /// Present when this window is a file chooser answering a portal request.
    pub(crate) picker: Option<PickerState>,
}

impl Marcel {
    pub(crate) fn new(start_dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
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
        let state_file = config::StateFile::open(config::path(&home_dir, config::STATE_FILE));
        let browser_state = state_file.state;
        let state_unreadable = state_file.read_only().map(str::to_string);
        // One writer per window, saving in order on the blocking pool. A save
        // that fails — including one the file refuses because it could not be
        // read — is said on this window, not on stderr.
        let (state_save_sender, state_save_receiver) = async_channel::unbounded();
        let state_file = Arc::new(state_file);
        let origin = Self::origin(window);
        let state_save_task = cx.spawn(async move |_, cx| {
            while let Ok(browser_state) = state_save_receiver.recv().await {
                let file = state_file.clone();
                let saved = cx
                    .background_executor()
                    .spawn(smol::unblock(move || file.save(browser_state)))
                    .await;
                if let Err(error) = saved {
                    let report = Report::Error(format!("Could not save view settings: {error:#}"));
                    surface::deliver(origin, Some(report), cx);
                }
            }
        });
        let mut directory = DirectorySession::new(start_dir.clone());
        directory.show_hidden = browser_state.show_hidden;
        directory.sort = browser_state.sort;

        // Effects arrive from the application, so a mutation another window
        // started still reconciles here, and progress it is running still
        // redraws here.
        let operations = crate::operations::global(cx);
        let bookmarks = crate::bookmarks::global(&home_dir, cx);
        let volumes = crate::volumes::global(&home_dir, cx);
        let network = crate::network::global(&home_dir, cx);
        let shared = [
            cx.observe(&operations, |_, _, cx| cx.notify()),
            cx.observe(&bookmarks, |_, _, cx| cx.notify()),
            cx.observe(&volumes, |_, _, cx| cx.notify()),
            cx.observe(&network, |_, _, cx| cx.notify()),
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
            volumes,
            network,
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
        // Said once the window is up: the file is left as it is, and the
        // user should know why their settings will not stick this session.
        if let Some(reason) = state_unreadable {
            this.report(Report::Error(reason), cx);
        }
        this
    }

    pub(crate) fn focus_browser(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.browser_focus.focus(window, cx);
    }

    pub(crate) fn origin(window: &Window) -> AnyWindowHandle {
        window.window_handle()
    }

    /// Show `report` on this window from a place that has no `Window` in hand.
    ///
    /// The window cannot be borrowed while this view is being updated inside
    /// it, so the report waits for the update to finish and then goes to the
    /// window this view was last drawn in.
    pub(crate) fn report(&self, report: Report, cx: &mut Context<Self>) {
        let view = cx.entity_id();
        cx.defer(move |cx| {
            cx.with_window(view, |window, cx| report.show(window, cx));
        });
    }

    /// Make the filter field show `query` from a place that has no `Window`
    /// in hand: a navigation that cleared the session's filter.
    ///
    /// The field lays its text out against the window, so, as with `report`,
    /// the write waits for the current update to finish. Nothing else keeps
    /// the field and the session's query aligned: the field's own edits reach
    /// the session through its Change event, and a programmatic write emits
    /// no such event.
    pub(crate) fn show_filter_text(&self, query: String, cx: &mut Context<Self>) {
        if self.ui.search_input.read(cx).value().as_ref() == query {
            return;
        }
        let input = self.ui.search_input.clone();
        let view = cx.entity_id();
        cx.defer(move |cx| {
            cx.with_window(view, |window, cx| {
                input.update(cx, |input, cx| input.set_value(query, window, cx));
            });
        });
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
            sort: self.directory.sort,
            sidebar_hidden: self.ui.sidebar_hidden,
            theme: crate::theme::chosen(),
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
        modified: None,
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
