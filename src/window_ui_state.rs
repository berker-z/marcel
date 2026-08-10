use std::path::PathBuf;

use gpui::{Entity, Pixels, Point, Subscription, Task, UniformListScrollHandle};
use gpui_component::input::InputState;

use crate::state::{BrowserState, BrowserView};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ViewMode {
    #[default]
    List,
    Grid,
}

#[derive(Clone, Copy)]
pub struct EntryMenu {
    pub position: Point<Pixels>,
    pub target: ContextMenuTarget,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ContextMenuTarget {
    Entry,
    CurrentDirectory,
}

pub struct WindowUiState {
    pub search_input: Entity<InputState>,
    pub _search_subscriptions: Vec<Subscription>,
    pub location_input: Entity<InputState>,
    pub _location_subscription: Subscription,
    pub location_editing: bool,
    pub location_resolving: bool,
    pub location_error: Option<String>,
    pub location_ticket: u64,
    pub rename_path: Option<PathBuf>,
    pub rename_input: Option<Entity<InputState>>,
    pub rename_subscription: Option<Subscription>,
    pub entry_menu: Option<EntryMenu>,
    /// Whether the open conflict dialog will answer for the rest of the
    /// operation.
    ///
    /// Lives here rather than inside the dialog because the checkbox is a
    /// controlled component: it draws whatever it is told, so the value has to
    /// sit somewhere a click can change and then notify.
    pub conflict_apply_to_all: bool,
    pub directory_scroll: UniformListScrollHandle,
    pub view_mode: ViewMode,
    pub grid_layout_columns: usize,
    pub state_save_sender: async_channel::Sender<BrowserState>,
    pub _state_save_task: Task<()>,
}

impl WindowUiState {
    pub fn new(
        search_input: Entity<InputState>,
        search_subscription: Subscription,
        location_input: Entity<InputState>,
        location_subscription: Subscription,
        browser_state: &BrowserState,
        state_save_sender: async_channel::Sender<BrowserState>,
        state_save_task: Task<()>,
    ) -> Self {
        Self {
            search_input,
            _search_subscriptions: vec![search_subscription],
            location_input,
            _location_subscription: location_subscription,
            location_editing: false,
            location_resolving: false,
            location_error: None,
            location_ticket: 0,
            rename_path: None,
            rename_input: None,
            rename_subscription: None,
            entry_menu: None,
            conflict_apply_to_all: false,
            directory_scroll: UniformListScrollHandle::new(),
            view_mode: match browser_state.view {
                BrowserView::List => ViewMode::List,
                BrowserView::Grid => ViewMode::Grid,
            },
            grid_layout_columns: 1,
            state_save_sender,
            _state_save_task: state_save_task,
        }
    }
}
