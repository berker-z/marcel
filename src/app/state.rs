//! What a window holds besides the directory it browses: the chrome's own
//! state, in-flight pointer gestures, the sidebar, and — for a file
//! chooser — the request it is answering.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
};

use async_channel::Sender;
use gpui::{
    Bounds, Entity, Pixels, Point, SharedString, Subscription, Task, UniformListScrollHandle,
};
use gpui_component::{input::InputState, select::SelectState};

use crate::{
    config::{BrowserState, BrowserView},
    desktop::{
        gvfs::{Location, MountSpec},
        picker::{FileFilter, PickerMode, PickerRequest, PickerResponse},
        places::Place,
    },
    fsops::trash::TrashRecord,
};

use super::pointer::FileDrag;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ViewMode {
    #[default]
    List,
    Grid,
}

impl ViewMode {
    pub fn from_state(view: BrowserView) -> Self {
        match view {
            BrowserView::List => Self::List,
            BrowserView::Grid => Self::Grid,
        }
    }

    pub fn to_state(self) -> BrowserView {
        match self {
            Self::List => BrowserView::List,
            Self::Grid => BrowserView::Grid,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ContextMenuTarget {
    Entry,
    CurrentDirectory,
    /// The sort picker, opened from its button in the sidebar.
    Sort,
}

#[derive(Clone, Copy)]
pub struct EntryMenu {
    pub position: Point<Pixels>,
    pub target: ContextMenuTarget,
}

/// An inline rename in progress on one row.
pub struct RenameEdit {
    pub path: PathBuf,
    pub input: Entity<InputState>,
    pub _subscription: Subscription,
}

/// The location bar while it is a text field rather than breadcrumbs.
#[derive(Default)]
pub struct LocationEdit {
    pub active: bool,
    pub resolving: bool,
    pub error: Option<String>,
    /// Bumped whenever the edit changes shape, so a resolution that finishes
    /// after the user moved on is ignored.
    pub ticket: u64,
}

impl LocationEdit {
    pub fn bump(&mut self) -> u64 {
        self.ticket = self.ticket.wrapping_add(1);
        self.ticket
    }

    pub fn begin(&mut self) {
        self.bump();
        self.active = true;
        self.resolving = false;
        self.error = None;
    }

    pub fn end(&mut self) {
        self.bump();
        self.active = false;
        self.resolving = false;
        self.error = None;
    }
}

pub struct UiState {
    pub search_input: Entity<InputState>,
    pub location_input: Entity<InputState>,
    pub _input_subscriptions: [Subscription; 2],
    pub location: LocationEdit,
    pub rename: Option<RenameEdit>,
    pub entry_menu: Option<EntryMenu>,
    /// Whether the sort picker was open when its button was pressed. The
    /// popover dismisses itself on the press, before the click arrives, so
    /// without this the button could only ever open the picker.
    pub sort_menu_was_open: bool,
    pub directory_scroll: UniformListScrollHandle,
    pub view_mode: ViewMode,
    pub grid_layout_columns: usize,
    /// The user's choice, persisted: the sidebar folded away.
    pub sidebar_hidden: bool,
    /// Whether the window is too narrow for the sidebar, as of the last
    /// frame. Crossing that line in either direction clears the override.
    pub narrow: bool,
    /// The user's answer while narrow: `Some(true)` unfolds the sidebar over
    /// a window that folded it, `Some(false)` folds it after that. Neither
    /// is remembered past the next resize across the line.
    pub sidebar_override_while_narrow: Option<bool>,
    /// What the last frame showed, so a toggle knows what it is flipping.
    pub sidebar_shown: bool,
    pub state_save_sender: Sender<BrowserState>,
    pub _state_save_task: Task<()>,
}

impl UiState {
    pub fn new(
        search_input: Entity<InputState>,
        location_input: Entity<InputState>,
        subscriptions: [Subscription; 2],
        browser_state: &BrowserState,
        state_save_sender: Sender<BrowserState>,
        state_save_task: Task<()>,
    ) -> Self {
        Self {
            search_input,
            location_input,
            _input_subscriptions: subscriptions,
            location: LocationEdit::default(),
            rename: None,
            entry_menu: None,
            sort_menu_was_open: false,
            directory_scroll: UniformListScrollHandle::new(),
            view_mode: ViewMode::from_state(browser_state.view),
            grid_layout_columns: 1,
            sidebar_hidden: browser_state.sidebar_hidden,
            narrow: false,
            sidebar_override_while_narrow: None,
            sidebar_shown: !browser_state.sidebar_hidden,
            state_save_sender,
            _state_save_task: state_save_task,
        }
    }

    /// The row being renamed, if `path` is it.
    pub fn rename_input_for(&self, path: &Path) -> Option<Entity<InputState>> {
        self.rename.as_ref().filter(|edit| edit.path == path).map(|edit| edit.input.clone())
    }
}

#[derive(Clone)]
pub struct MarqueeGesture {
    pub start_window: Point<Pixels>,
    pub origin_content: Point<Pixels>,
    pub current_window: Point<Pixels>,
    pub base_selection: HashSet<PathBuf>,
    pub additive: bool,
    pub active: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct EntryHitRegion {
    pub bounds: Bounds<Pixels>,
    pub navigable: bool,
}

/// Drag payload memoized against the selection and projection revisions it was
/// built from.
///
/// Building it walks every visible entry and clones each selected path, so
/// doing it once per frame cost 1.8 ms with a single file selected and
/// 12.3 ms after Select All in a 50,000-entry directory — most of a 60 Hz
/// frame budget, spent on a payload that is only read when a drag starts.
pub struct CachedFileDrag {
    pub key: (u64, u64),
    pub payload: Option<FileDrag>,
}

/// Pointer gestures over the listing, and the painted geometry they hit-test
/// against. The bounds are filled in by canvases during paint and read by the
/// next event, which is why they are shared cells rather than plain fields.
pub struct DragState {
    pub marquee: Option<MarqueeGesture>,
    pub marquee_scroll_task: Option<Task<()>>,
    pub file_pointer: Option<Point<Pixels>>,
    pub file_scroll_task: Option<Task<()>>,
    pub payload: Option<CachedFileDrag>,
    pub browser_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    pub entry_hit_bounds: Rc<RefCell<HashMap<PathBuf, EntryHitRegion>>>,
    pub entry_content_bounds: Rc<RefCell<HashMap<PathBuf, Bounds<Pixels>>>>,
}

impl Default for DragState {
    fn default() -> Self {
        Self {
            marquee: None,
            marquee_scroll_task: None,
            file_pointer: None,
            file_scroll_task: None,
            payload: None,
            browser_bounds: Rc::new(Cell::new(None)),
            entry_hit_bounds: Rc::new(RefCell::new(HashMap::new())),
            entry_content_bounds: Rc::new(RefCell::new(HashMap::new())),
        }
    }
}

impl DragState {
    /// A new projection invalidates every painted rectangle and any gesture
    /// that was reading them.
    pub fn reset_geometry(&mut self) {
        self.entry_hit_bounds.borrow_mut().clear();
        self.entry_content_bounds.borrow_mut().clear();
        self.marquee = None;
        self.marquee_scroll_task.take();
    }
}

/// The context menu on a drive: Unmount, and Eject for a removable one.
#[derive(Clone, Debug)]
pub struct VolumeMenu {
    /// The device node, which is the stable name for a drive across the
    /// re-reads that follow every UDisks2 change.
    pub device: PathBuf,
    pub position: Point<Pixels>,
}

/// What a Network row stands for, which is what its context menu acts on.
#[derive(Clone, Debug)]
pub enum NetworkTarget {
    /// A saved server, by slot and by identity, re-verified on a click the
    /// way bookmarks are.
    Server { index: usize, location: Location },
    /// A share connected by hand or by another application.
    Mount(MountSpec),
}

#[derive(Clone, Debug)]
pub struct NetworkMenu {
    pub target: NetworkTarget,
    pub position: Point<Pixels>,
}

#[derive(Clone)]
pub struct BookmarkMenu {
    pub index: usize,
    /// The bookmark the menu opened on. Another window can mutate the shared
    /// list while the menu is up, so a destructive click re-verifies the path
    /// rather than trusting the index.
    pub path: PathBuf,
    pub position: Point<Pixels>,
}

pub struct SidebarState {
    pub places: Vec<Place>,
    pub place_icons: HashMap<PathBuf, PathBuf>,
    pub places_loading: bool,
    pub places_task: Option<Task<()>>,
    pub browsing_trash: bool,
    pub trash_records: HashMap<PathBuf, TrashRecord>,
    /// How many Trash entries the last listing could not describe. Empty Trash
    /// must not offer to empty a Trash it has only partly seen.
    pub unreadable_trash_entries: usize,
    /// The slot a dragged bookmark would land in, while one is over the list.
    pub bookmark_insertion: Option<usize>,
    pub bookmark_menu: Option<BookmarkMenu>,
    pub volume_menu: Option<VolumeMenu>,
    pub network_menu: Option<NetworkMenu>,
    pub bookmark_region_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    pub bookmark_row_bounds: Rc<RefCell<HashMap<usize, Bounds<Pixels>>>>,
    pub place_drop_bounds: Rc<RefCell<HashMap<PathBuf, Bounds<Pixels>>>>,
}

impl SidebarState {
    pub fn new(home: &Path) -> Self {
        Self {
            places: vec![Place::home(home.to_path_buf())],
            place_icons: HashMap::new(),
            places_loading: true,
            places_task: None,
            browsing_trash: false,
            trash_records: HashMap::new(),
            unreadable_trash_entries: 0,
            bookmark_insertion: None,
            bookmark_menu: None,
            volume_menu: None,
            network_menu: None,
            bookmark_region_bounds: Rc::new(Cell::new(None)),
            bookmark_row_bounds: Rc::new(RefCell::new(HashMap::new())),
            place_drop_bounds: Rc::new(RefCell::new(HashMap::new())),
        }
    }
}

/// What a Marcel window holds when it is answering a file-chooser request.
///
/// The pane is an ordinary Marcel pane; this is the extra state that turns it
/// into a dialog: the question, the controls the question needs, and the one
/// place the answer is sent from.
pub struct PickerState {
    pub mode: PickerMode,
    pub multiple: bool,
    pub accept_label: String,
    pub filters: Vec<FileFilter>,
    /// Index into `filters` of the one currently applied.
    pub active_filter: Option<usize>,
    /// The name field of a save dialog.
    pub name_input: Option<Entity<InputState>>,
    pub filter_select: Option<Entity<SelectState<Vec<SharedString>>>>,
    pub _subscriptions: Vec<Subscription>,
    /// A background existence check is running; confirming again waits.
    pub confirming: bool,
    /// Present until the answer is sent. Exactly one answer per request.
    reply: Option<Sender<PickerResponse>>,
}

impl PickerState {
    pub fn new(
        request: PickerRequest,
        name_input: Option<(Entity<InputState>, Subscription)>,
        filter_select: Option<(Entity<SelectState<Vec<SharedString>>>, Subscription)>,
    ) -> Self {
        let accept_label = request
            .accept_label
            .clone()
            .unwrap_or_else(|| request.default_accept_label().to_string());
        let (name_input, name_subscription) = name_input.unzip();
        let (filter_select, filter_subscription) = filter_select.unzip();
        Self {
            // With filters offered and none chosen, the first one is what the
            // caller expects to see applied; that is what GTK does.
            active_filter: request
                .current_filter
                .or_else(|| (!request.filters.is_empty()).then_some(0)),
            mode: request.mode,
            multiple: request.multiple,
            accept_label,
            filters: request.filters,
            name_input,
            filter_select,
            _subscriptions: name_subscription.into_iter().chain(filter_subscription).collect(),
            confirming: false,
            reply: Some(request.reply),
        }
    }

    /// The filter currently applied to the listing, if any.
    pub fn active_filter(&self) -> Option<&FileFilter> {
        self.active_filter.and_then(|index| self.filters.get(index))
    }

    /// Send the answer. Only the first answer counts; the rest report `false`.
    pub fn answer(&mut self, response: PickerResponse) -> bool {
        let Some(reply) = self.reply.take() else {
            return false;
        };
        // The receiver is the D-Bus method waiting on us. A send can only fail
        // when it stopped waiting, and then there is nobody to tell.
        let _ = reply.try_send(response);
        true
    }
}

impl Drop for PickerState {
    /// A window that closes without answering — the title-bar button, the
    /// compositor killing it — answered "cancel". Anything else leaves the
    /// caller's dialog blocked on a reply that will never come.
    fn drop(&mut self) {
        self.answer(PickerResponse::Cancelled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(reply: Sender<PickerResponse>) -> PickerRequest {
        let (_close, closed) = async_channel::bounded(1);
        PickerRequest {
            title: String::new(),
            mode: PickerMode::OpenFiles,
            multiple: false,
            accept_label: None,
            start_directory: None,
            current_name: None,
            filters: vec![FileFilter::new("All".to_string(), Vec::new())],
            current_filter: None,
            reply,
            closed,
        }
    }

    #[test]
    fn dropping_an_unanswered_picker_cancels_it_once() {
        let (reply, responses) = async_channel::bounded(1);
        let state = PickerState::new(request(reply), None, None);
        assert_eq!(state.active_filter, Some(0));
        assert_eq!(state.accept_label, "Open");
        drop(state);
        assert_eq!(responses.try_recv(), Ok(PickerResponse::Cancelled));
        assert!(responses.try_recv().is_err());
    }

    #[test]
    fn only_the_first_answer_is_sent() {
        let (reply, responses) = async_channel::bounded(1);
        let mut state = PickerState::new(request(reply), None, None);
        assert!(state.answer(PickerResponse::Closed));
        assert!(!state.answer(PickerResponse::Cancelled));
        drop(state);
        assert_eq!(responses.try_recv(), Ok(PickerResponse::Closed));
        assert!(responses.try_recv().is_err());
    }
}
