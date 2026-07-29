use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use gpui::{
    AnyElement, Bounds, ClickEvent, ClipboardItem, Context, Entity, FocusHandle, Focusable, Hsla,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ObjectFit, ParentElement, Pixels, Point, Render, ScrollStrategy, SharedString, Styled,
    Subscription, Task, TextRun, Timer, UniformListScrollHandle, Window, canvas, div, font, img,
    prelude::*, px, relative, uniform_list,
};
use gpui_component::{
    ActiveTheme, Disableable, Sizable, Theme, h_flex,
    input::{Input, InputEvent, InputState},
    resizable::{h_resizable, resizable_panel},
    scroll::ScrollableElement,
    switch::Switch,
    text::TextView,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    commands::{
        ActivateSelection, BROWSER_KEY_CONTEXT, BrowserCommand, ClearSelection, ExtendDown,
        ExtendLeft, ExtendPageDown, ExtendPageUp, ExtendRight, ExtendToFirst, ExtendToLast,
        ExtendUp, GoBack, GoForward, GoToParent, MoveDown, MoveLeft, MoveRight, MoveUp,
        OpenWithSelection, SelectAll, SelectFirst, SelectLast, SelectPageDown, SelectPageUp,
    },
    fs::{
        DirectoryUpdate, FileEntry, format_size, merge_sorted_entries, stream_directory,
        stream_directory_cancellable,
    },
    history::NavigationHistory,
    places::{Place, discover as discover_places},
    preview::{Preview, PreviewState, load_preview},
    selection::SelectionModel,
    thumbnails,
};

const DIRECTORY_ROW_HEIGHT: f32 = 36.0;
const GRID_TILE_WIDTH: f32 = 120.0;
const GRID_TILE_HEIGHT: f32 = 164.0;
const GRID_GAP: f32 = 8.0;
const GRID_SIDE_PADDING: f32 = 16.0;
const GRID_LABEL_HEIGHT: f32 = 36.0;
const GRID_LABEL_COLUMNS: usize = 28;
const GRID_ROW_HEIGHT: f32 = GRID_TILE_HEIGHT + GRID_GAP;
const MAX_MEMORY_THUMBNAILS: usize = 512;
const THUMBNAIL_WORKERS: usize = 2;
const PDF_PAGE_WORKERS: usize = 2;
const PDF_PAGE_LOOKAHEAD: usize = 1;
const DEFAULT_PREVIEW_WIDTH: f32 = 420.0;
const MIN_BROWSER_WIDTH: f32 = 360.0;
const MIN_PREVIEW_WIDTH: f32 = 280.0;
const MAX_PREVIEW_WIDTH: f32 = 900.0;
const MIN_PLACES_WIDTH: f32 = 176.0;
const MAX_PLACES_WIDTH: f32 = 320.0;
const PREVIEW_TEXT_CHROME_WIDTH: f32 = 92.0;
const PREVIEW_WRAP_DEBOUNCE: Duration = Duration::from_millis(80);
const MARQUEE_THRESHOLD: f32 = 4.0;
const MARQUEE_EDGE_ZONE: f32 = 36.0;
const MARQUEE_MAX_SCROLL_STEP: f32 = 18.0;
const MARQUEE_SCROLL_INTERVAL: Duration = Duration::from_millis(16);
const ENTRY_MENU_WIDTH: f32 = 208.0;
const ENTRY_MENU_HEIGHT: f32 = 430.0;
const DIRECTORY_MENU_HEIGHT: f32 = 286.0;
const ENTRY_MENU_MARGIN: f32 = 8.0;
const IOSEVKA_UI_FONTS: [&str; 2] = ["Iosevka Nerd Font Mono", "Iosevka"];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ViewMode {
    #[default]
    List,
    Grid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectionMotion {
    Up,
    Down,
    Left,
    Right,
    First,
    Last,
    PageUp,
    PageDown,
}

#[derive(Clone, Debug)]
enum ThumbnailState {
    Ready(PathBuf),
    Failed,
}

#[derive(Clone, Debug)]
enum PdfPageState {
    Ready(PathBuf),
    Failed(String),
}

#[derive(Clone)]
struct MarqueeGesture {
    start_window: Point<Pixels>,
    origin_content: Point<Pixels>,
    current_window: Point<Pixels>,
    base_selection: HashSet<PathBuf>,
    additive: bool,
    active: bool,
}

#[derive(Clone, Copy)]
struct EntryMenu {
    position: Point<Pixels>,
    target: ContextMenuTarget,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ContextMenuTarget {
    Entry,
    CurrentDirectory,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WrappedPreviewLine {
    source_line: Option<usize>,
    contents: String,
}

#[derive(Clone)]
struct WrappedPreview {
    ticket: u64,
    columns: usize,
    lines: Arc<[WrappedPreviewLine]>,
}

pub struct Marcel {
    browser_focus: FocusHandle,
    system_ui_font: SharedString,
    use_iosevka_ui: bool,
    iosevka_ui_font: Option<SharedString>,
    current_dir: PathBuf,
    entries: Vec<FileEntry>,
    visible_entries: Vec<usize>,
    filter_query: String,
    show_hidden: bool,
    search_input: Entity<InputState>,
    _search_subscriptions: Vec<Subscription>,
    selection: SelectionModel,
    entry_menu: Option<EntryMenu>,
    marquee: Option<MarqueeGesture>,
    marquee_scroll_task: Option<Task<()>>,
    browser_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    entry_hit_bounds: Rc<RefCell<HashMap<PathBuf, Bounds<Pixels>>>>,
    entry_content_bounds: Rc<RefCell<HashMap<PathBuf, Bounds<Pixels>>>>,
    directory_scroll: UniformListScrollHandle,
    view_mode: ViewMode,
    grid_layout_columns: usize,
    thumbnails: HashMap<PathBuf, ThumbnailState>,
    thumbnail_order: VecDeque<PathBuf>,
    thumbnail_queue: VecDeque<PathBuf>,
    thumbnail_pending: HashSet<PathBuf>,
    thumbnail_inflight: HashSet<PathBuf>,
    thumbnail_wake_sender: async_channel::Sender<()>,
    thumbnail_wake_receiver: async_channel::Receiver<()>,
    thumbnail_workers: Vec<Task<()>>,
    directory_loading: bool,
    directory_error: Option<String>,
    directory_ticket: u64,
    directory_task: Option<Task<()>>,
    preview_state: PreviewState,
    preview_ticket: u64,
    preview_task: Option<Task<()>>,
    preview_cancel: Option<Arc<AtomicBool>>,
    folder_preview_entries: Vec<FileEntry>,
    folder_preview_loading: bool,
    folder_preview_error: Option<String>,
    folder_preview_task: Option<Task<()>>,
    folder_preview_scroll: UniformListScrollHandle,
    pdf_pages: HashMap<usize, PdfPageState>,
    pdf_page_queue: VecDeque<usize>,
    pdf_page_pending: HashSet<usize>,
    pdf_page_inflight: HashSet<usize>,
    pdf_page_wake_sender: async_channel::Sender<()>,
    pdf_page_wake_receiver: async_channel::Receiver<()>,
    pdf_page_workers: Vec<Task<()>>,
    pdf_scroll: UniformListScrollHandle,
    preview_wrap: Option<WrappedPreview>,
    preview_wrap_task: Option<Task<()>>,
    preview_resize_task: Option<Task<()>>,
    preview_text_scroll: UniformListScrollHandle,
    preview_width: Rc<Cell<Pixels>>,
    preview_mono_cell_width: Rc<Cell<Pixels>>,
    preview_mono_line_height: Rc<Cell<Pixels>>,
    history: NavigationHistory,
    places: Vec<Place>,
    place_icons: HashMap<PathBuf, PathBuf>,
    places_loading: bool,
    places_task: Option<Task<()>>,
}

impl Marcel {
    pub fn new(start_dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let start_dir = normalize_start_directory(start_dir);
        let home_dir = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| start_dir.clone());
        let (thumbnail_wake_sender, thumbnail_wake_receiver) =
            async_channel::bounded(THUMBNAIL_WORKERS);
        let (pdf_page_wake_sender, pdf_page_wake_receiver) =
            async_channel::bounded(PDF_PAGE_WORKERS);
        let system_ui_font = cx.theme().font_family.clone();
        let available_fonts = cx.text_system().all_font_names();
        let search_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Filter current folder")
                .clean_on_escape()
        });
        let search_subscription = cx.subscribe_in(
            &search_input,
            window,
            |this, input, event: &InputEvent, window, cx| {
                this.on_search_input_event(input, event, window, cx);
            },
        );
        let iosevka_ui_font = IOSEVKA_UI_FONTS
            .iter()
            .find(|family| available_fonts.iter().any(|name| name == **family))
            .map(|family| SharedString::from(*family));
        let mono_font_size = cx.theme().mono_font_size;

        let mut this = Self {
            browser_focus: cx.focus_handle(),
            system_ui_font,
            use_iosevka_ui: false,
            iosevka_ui_font,
            current_dir: start_dir.clone(),
            entries: Vec::new(),
            visible_entries: Vec::new(),
            filter_query: String::new(),
            show_hidden: true,
            search_input,
            _search_subscriptions: vec![search_subscription],
            selection: SelectionModel::default(),
            entry_menu: None,
            marquee: None,
            marquee_scroll_task: None,
            browser_bounds: Rc::new(Cell::new(None)),
            entry_hit_bounds: Rc::new(RefCell::new(HashMap::new())),
            entry_content_bounds: Rc::new(RefCell::new(HashMap::new())),
            directory_scroll: UniformListScrollHandle::new(),
            view_mode: ViewMode::List,
            grid_layout_columns: 1,
            thumbnails: HashMap::new(),
            thumbnail_order: VecDeque::new(),
            thumbnail_queue: VecDeque::new(),
            thumbnail_pending: HashSet::new(),
            thumbnail_inflight: HashSet::new(),
            thumbnail_wake_sender,
            thumbnail_wake_receiver,
            thumbnail_workers: Vec::new(),
            directory_loading: false,
            directory_error: None,
            directory_ticket: 0,
            directory_task: None,
            preview_state: PreviewState::Empty,
            preview_ticket: 0,
            preview_task: None,
            preview_cancel: None,
            folder_preview_entries: Vec::new(),
            folder_preview_loading: false,
            folder_preview_error: None,
            folder_preview_task: None,
            folder_preview_scroll: UniformListScrollHandle::new(),
            pdf_pages: HashMap::new(),
            pdf_page_queue: VecDeque::new(),
            pdf_page_pending: HashSet::new(),
            pdf_page_inflight: HashSet::new(),
            pdf_page_wake_sender,
            pdf_page_wake_receiver,
            pdf_page_workers: Vec::new(),
            pdf_scroll: UniformListScrollHandle::new(),
            preview_wrap: None,
            preview_wrap_task: None,
            preview_resize_task: None,
            preview_text_scroll: UniformListScrollHandle::new(),
            preview_width: Rc::new(Cell::new(px(0.0))),
            preview_mono_cell_width: Rc::new(Cell::new(mono_font_size * 0.6)),
            preview_mono_line_height: Rc::new(Cell::new(mono_font_size * 1.5)),
            history: NavigationHistory::new(start_dir),
            places: vec![Place::home(home_dir.clone())],
            place_icons: HashMap::new(),
            places_loading: true,
            places_task: None,
        };
        this.start_places_load(home_dir, cx);
        this.start_directory_load(true, cx);
        this
    }

    pub fn focus_browser(&self, window: &mut Window) {
        self.browser_focus.focus(window);
    }

    fn start_places_load(&mut self, home: PathBuf, cx: &mut Context<Self>) {
        let load_task = cx.background_executor().spawn(smol::unblock(move || {
            let places = discover_places(&home);
            let mut icon_provider = crate::icons::IconProvider::discover();
            let icons = places
                .iter()
                .filter_map(|place| {
                    icon_provider
                        .icon_for_place(&place.label)
                        .map(|icon| (place.path.clone(), icon))
                })
                .collect();
            (places, icons)
        }));

        self.places_task = Some(cx.spawn(async move |this, cx| {
            let (places, icons) = load_task.await;
            let _ = this.update(cx, |this, cx| {
                this.places = places;
                this.place_icons = icons;
                this.places_loading = false;
                cx.notify();
            });
        }));
    }

    fn start_directory_load(&mut self, clear_filter: bool, cx: &mut Context<Self>) {
        self.entry_menu = None;
        self.directory_ticket = self.directory_ticket.wrapping_add(1);
        let ticket = self.directory_ticket;
        let path = self.current_dir.clone();

        self.directory_task.take();
        self.thumbnail_workers.clear();
        self.thumbnail_queue.clear();
        self.thumbnail_pending.clear();
        self.thumbnail_inflight.clear();
        while self.thumbnail_wake_receiver.try_recv().is_ok() {}
        self.thumbnails.clear();
        self.thumbnail_order.clear();
        self.entry_content_bounds.borrow_mut().clear();
        self.entries.clear();
        self.visible_entries.clear();
        if clear_filter {
            self.filter_query.clear();
        }
        self.directory_error = None;
        self.directory_loading = true;
        self.clear_selection();

        let (sender, receiver) = async_channel::unbounded();
        cx.background_executor()
            .spawn(smol::unblock(move || stream_directory(&path, sender)))
            .detach();

        self.directory_task = Some(cx.spawn(async move |this, cx| {
            while let Ok(update) = receiver.recv().await {
                let should_continue = this
                    .update(cx, |this, cx| {
                        if ticket != this.directory_ticket {
                            return false;
                        }

                        match update {
                            DirectoryUpdate::Batch(batch) => {
                                this.entries =
                                    merge_sorted_entries(std::mem::take(&mut this.entries), batch);
                                this.rebuild_visible_entries();
                                this.reconcile_filter_selection(cx);
                            }
                            DirectoryUpdate::Done => {
                                this.directory_loading = false;
                            }
                            DirectoryUpdate::Error(error) => {
                                this.directory_loading = false;
                                this.directory_error = Some(error);
                            }
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);

                if !should_continue {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn navigate_to(&mut self, path: PathBuf, add_to_history: bool, cx: &mut Context<Self>) {
        if path == self.current_dir {
            return;
        }
        if add_to_history {
            self.history.push(&path);
        }
        self.current_dir = path;
        self.start_directory_load(true, cx);
    }

    fn go_back(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.history.go_back() {
            self.current_dir = path;
            self.start_directory_load(true, cx);
        }
    }

    fn go_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.history.go_forward() {
            self.current_dir = path;
            self.start_directory_load(true, cx);
        }
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        if let Some(parent) = self.current_dir.parent() {
            self.navigate_to(parent.to_path_buf(), true, cx);
        }
    }

    fn command_enabled(&self, command: BrowserCommand) -> bool {
        match command {
            BrowserCommand::GoToParent => self.current_dir.parent().is_some(),
            BrowserCommand::GoBack => self.history.can_go_back(),
            BrowserCommand::GoForward => self.history.can_go_forward(),
            BrowserCommand::ActivateSelection => self.selection.primary().is_some(),
            BrowserCommand::OpenWithSelection => self
                .selection
                .primary()
                .and_then(|path| self.entries.iter().find(|entry| &entry.path == path))
                .is_some_and(|entry| !entry.navigable),
            BrowserCommand::ClearSelection => !self.selection.selected().is_empty(),
            BrowserCommand::MoveUp
            | BrowserCommand::MoveDown
            | BrowserCommand::MoveLeft
            | BrowserCommand::MoveRight
            | BrowserCommand::ExtendUp
            | BrowserCommand::ExtendDown
            | BrowserCommand::ExtendLeft
            | BrowserCommand::ExtendRight
            | BrowserCommand::ExtendToFirst
            | BrowserCommand::ExtendToLast
            | BrowserCommand::ExtendPageUp
            | BrowserCommand::ExtendPageDown
            | BrowserCommand::SelectFirst
            | BrowserCommand::SelectLast
            | BrowserCommand::SelectPageUp
            | BrowserCommand::SelectPageDown
            | BrowserCommand::SelectAll => !self.visible_entries.is_empty(),
        }
    }

    fn execute_browser_command(&mut self, command: BrowserCommand, cx: &mut Context<Self>) {
        if !self.command_enabled(command) {
            return;
        }

        match command {
            BrowserCommand::MoveUp => self.move_keyboard_selection(SelectionMotion::Up, false, cx),
            BrowserCommand::MoveDown => {
                self.move_keyboard_selection(SelectionMotion::Down, false, cx)
            }
            BrowserCommand::MoveLeft => {
                self.move_keyboard_selection(SelectionMotion::Left, false, cx)
            }
            BrowserCommand::MoveRight => {
                self.move_keyboard_selection(SelectionMotion::Right, false, cx)
            }
            BrowserCommand::ExtendUp => self.move_keyboard_selection(SelectionMotion::Up, true, cx),
            BrowserCommand::ExtendDown => {
                self.move_keyboard_selection(SelectionMotion::Down, true, cx)
            }
            BrowserCommand::ExtendLeft => {
                self.move_keyboard_selection(SelectionMotion::Left, true, cx)
            }
            BrowserCommand::ExtendRight => {
                self.move_keyboard_selection(SelectionMotion::Right, true, cx)
            }
            BrowserCommand::ExtendToFirst => {
                self.move_keyboard_selection(SelectionMotion::First, true, cx)
            }
            BrowserCommand::ExtendToLast => {
                self.move_keyboard_selection(SelectionMotion::Last, true, cx)
            }
            BrowserCommand::ExtendPageUp => {
                self.move_keyboard_selection(SelectionMotion::PageUp, true, cx)
            }
            BrowserCommand::ExtendPageDown => {
                self.move_keyboard_selection(SelectionMotion::PageDown, true, cx)
            }
            BrowserCommand::SelectFirst => {
                self.move_keyboard_selection(SelectionMotion::First, false, cx)
            }
            BrowserCommand::SelectLast => {
                self.move_keyboard_selection(SelectionMotion::Last, false, cx)
            }
            BrowserCommand::SelectPageUp => {
                self.move_keyboard_selection(SelectionMotion::PageUp, false, cx)
            }
            BrowserCommand::SelectPageDown => {
                self.move_keyboard_selection(SelectionMotion::PageDown, false, cx)
            }
            BrowserCommand::ActivateSelection => self.activate_primary(cx),
            BrowserCommand::OpenWithSelection => self.open_primary_with(cx),
            BrowserCommand::ClearSelection => {
                self.clear_selection();
                cx.notify();
            }
            BrowserCommand::GoToParent => self.go_up(cx),
            BrowserCommand::GoBack => self.go_back(cx),
            BrowserCommand::GoForward => self.go_forward(cx),
            BrowserCommand::SelectAll => self.select_all_entries(cx),
        }
    }

    fn move_keyboard_selection(
        &mut self,
        motion: SelectionMotion,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        let current = self.selection.primary().and_then(|path| {
            self.visible_entries.iter().position(|entry_index| {
                self.entries
                    .get(*entry_index)
                    .is_some_and(|entry| &entry.path == path)
            })
        });
        let columns = match self.view_mode {
            ViewMode::List => 1,
            ViewMode::Grid => self.grid_columns(),
        };
        let viewport_items = self.keyboard_page_size(columns);
        let Some(target) = selection_target(
            current,
            self.visible_entries.len(),
            columns,
            viewport_items,
            self.view_mode,
            motion,
        ) else {
            return;
        };
        if current == Some(target) && !extend {
            return;
        }

        let Some(entry) = self.visible_entry(target).cloned() else {
            return;
        };
        if extend {
            let ordered = self.visible_paths();
            self.selection
                .select_range(entry.path.clone(), &ordered, false);
        } else {
            self.selection.select_only(entry.path.clone());
        }

        let scroll_row = match self.view_mode {
            ViewMode::List => target,
            ViewMode::Grid => target / columns,
        };
        self.directory_scroll
            .scroll_to_item(scroll_row, ScrollStrategy::Center);
        self.start_preview(entry, cx);
        cx.notify();
    }

    fn keyboard_page_size(&self, columns: usize) -> usize {
        let height = self
            .browser_bounds
            .get()
            .map(|bounds| f32::from(bounds.size.height))
            .unwrap_or(DIRECTORY_ROW_HEIGHT);
        match self.view_mode {
            ViewMode::List => (height / DIRECTORY_ROW_HEIGHT).floor().max(1.0) as usize,
            ViewMode::Grid => (height / GRID_ROW_HEIGHT).floor().max(1.0) as usize * columns.max(1),
        }
    }

    fn activate_primary(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self
            .selection
            .primary()
            .and_then(|path| self.entries.iter().find(|entry| &entry.path == path))
            .cloned()
        else {
            return;
        };
        self.open_entry(entry, cx);
    }

    fn select_all_entries(&mut self, cx: &mut Context<Self>) {
        let ordered = self.visible_paths();
        self.selection.select_all(&ordered);
        if let Some(entry) = self
            .selection
            .primary()
            .and_then(|path| self.entries.iter().find(|entry| &entry.path == path))
            .cloned()
        {
            self.start_preview(entry, cx);
        }
        cx.notify();
    }

    fn on_move_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::MoveUp, cx);
    }

    fn on_move_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::MoveDown, cx);
    }

    fn on_move_left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::MoveLeft, cx);
    }

    fn on_move_right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::MoveRight, cx);
    }

    fn on_extend_up(&mut self, _: &ExtendUp, _: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::ExtendUp, cx);
    }

    fn on_extend_down(&mut self, _: &ExtendDown, _: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::ExtendDown, cx);
    }

    fn on_extend_left(&mut self, _: &ExtendLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::ExtendLeft, cx);
    }

    fn on_extend_right(&mut self, _: &ExtendRight, _: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::ExtendRight, cx);
    }

    fn on_extend_to_first(&mut self, _: &ExtendToFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::ExtendToFirst, cx);
    }

    fn on_extend_to_last(&mut self, _: &ExtendToLast, _: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::ExtendToLast, cx);
    }

    fn on_extend_page_up(&mut self, _: &ExtendPageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::ExtendPageUp, cx);
    }

    fn on_extend_page_down(&mut self, _: &ExtendPageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::ExtendPageDown, cx);
    }

    fn on_select_first(&mut self, _: &SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::SelectFirst, cx);
    }

    fn on_select_last(&mut self, _: &SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::SelectLast, cx);
    }

    fn on_select_page_up(&mut self, _: &SelectPageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::SelectPageUp, cx);
    }

    fn on_select_page_down(&mut self, _: &SelectPageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::SelectPageDown, cx);
    }

    fn on_activate_selection(
        &mut self,
        _: &ActivateSelection,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_browser_command(BrowserCommand::ActivateSelection, cx);
    }

    fn on_open_with_selection(
        &mut self,
        _: &OpenWithSelection,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_browser_command(BrowserCommand::OpenWithSelection, cx);
    }

    fn on_clear_selection(
        &mut self,
        _: &ClearSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.filter_query.is_empty() {
            self.execute_browser_command(BrowserCommand::ClearSelection, cx);
        } else {
            self.clear_filter(window, cx);
        }
    }

    fn on_go_to_parent(&mut self, _: &GoToParent, _: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::GoToParent, cx);
    }

    fn on_go_back(&mut self, _: &GoBack, _: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::GoBack, cx);
    }

    fn on_go_forward(&mut self, _: &GoForward, _: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::GoForward, cx);
    }

    fn on_select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::SelectAll, cx);
    }

    fn set_view_mode(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        if self.view_mode == mode {
            return;
        }
        self.view_mode = mode;
        self.marquee = None;
        self.marquee_scroll_task.take();
        self.entry_content_bounds.borrow_mut().clear();
        self.directory_scroll = UniformListScrollHandle::new();
        cx.notify();
    }

    fn grid_columns(&self) -> usize {
        let width = self
            .browser_bounds
            .get()
            .map(|bounds| f32::from(bounds.size.width))
            .unwrap_or(GRID_TILE_WIDTH + GRID_GAP);
        grid_column_count(width)
    }

    fn ensure_thumbnails(
        &mut self,
        visible: Range<usize>,
        nearby: Range<usize>,
        cx: &mut Context<Self>,
    ) {
        let priority = prioritize_thumbnail_indices(visible, nearby)
            .into_iter()
            .filter_map(|index| self.visible_entry(index))
            .filter(|entry| !entry.navigable && thumbnails::supports(&entry.path))
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();

        // Adapted from Yazi's paged preloading and superseding scheduler:
        // https://github.com/sxyazi/yazi/blob/e58022b9aafc8dabf586e2cc29b79a230071716f/yazi-core/src/tasks/prework.rs
        // https://github.com/sxyazi/yazi/blob/e58022b9aafc8dabf586e2cc29b79a230071716f/yazi-scheduler/src/scheduler.rs
        //
        // Marcel rebuilds the not-yet-started queue whenever GPUI reports a new
        // viewport. Running decodes may finish, but old queued work cannot sit
        // ahead of newly visible tiles.
        self.thumbnail_queue.clear();
        self.thumbnail_pending = self.thumbnail_inflight.clone();
        for path in priority {
            if self.thumbnails.contains_key(&path) || !self.thumbnail_pending.insert(path.clone()) {
                continue;
            }
            self.thumbnail_queue.push_back(path);
        }
        self.start_thumbnail_workers(cx);
        for _ in 0..THUMBNAIL_WORKERS {
            let _ = self.thumbnail_wake_sender.try_send(());
        }
    }

    fn start_thumbnail_workers(&mut self, cx: &mut Context<Self>) {
        if !self.thumbnail_workers.is_empty() {
            return;
        }

        let ticket = self.directory_ticket;
        let executor = cx.background_executor().clone();
        // Yazi uses a configurable preload worker pool; two workers are its
        // default. Marcel keeps that proven conservative default so decoding
        // can overlap without letting a thumbnail grid saturate every CPU.
        // Source:
        // https://github.com/sxyazi/yazi/blob/e58022b9aafc8dabf586e2cc29b79a230071716f/yazi-scheduler/src/worker.rs
        for _ in 0..THUMBNAIL_WORKERS {
            let executor = executor.clone();
            let wake = self.thumbnail_wake_receiver.clone();
            self.thumbnail_workers.push(cx.spawn(async move |this, cx| {
                loop {
                    let request = this
                        .update(cx, |this, _| {
                            let path = this.thumbnail_queue.pop_front()?;
                            this.thumbnail_inflight.insert(path.clone());
                            Some(path)
                        })
                        .ok()
                        .flatten();
                    let Some(path) = request else {
                        if wake.recv().await.is_err() {
                            break;
                        }
                        continue;
                    };

                    let load_path = path.clone();
                    let result = executor
                        .spawn(smol::unblock(move || {
                            thumbnails::load_or_create(&load_path)
                        }))
                        .await;
                    let keep_running = this
                        .update(cx, |this, cx| {
                            if ticket != this.directory_ticket {
                                return false;
                            }
                            this.thumbnail_inflight.remove(&path);
                            this.thumbnail_pending.remove(&path);
                            let state = match result {
                                Ok(thumbnail) => ThumbnailState::Ready(thumbnail),
                                Err(_) => ThumbnailState::Failed,
                            };
                            this.remember_thumbnail(path, state);
                            cx.notify();
                            true
                        })
                        .unwrap_or(false);
                    if !keep_running {
                        break;
                    }
                }
            }));
        }
        for _ in 0..THUMBNAIL_WORKERS {
            let _ = self.thumbnail_wake_sender.try_send(());
        }
    }

    fn remember_thumbnail(&mut self, path: PathBuf, state: ThumbnailState) {
        self.thumbnail_order.retain(|existing| existing != &path);
        self.thumbnail_order.push_back(path.clone());
        self.thumbnails.insert(path, state);

        while self.thumbnail_order.len() > MAX_MEMORY_THUMBNAILS {
            if let Some(expired) = self.thumbnail_order.pop_front() {
                self.thumbnails.remove(&expired);
            }
        }
    }

    fn clear_selection(&mut self) {
        self.selection.clear();
        self.marquee = None;
        self.marquee_scroll_task.take();
        self.clear_preview();
    }

    fn clear_preview(&mut self) {
        self.preview_ticket = self.preview_ticket.wrapping_add(1);
        if let Some(cancel) = self.preview_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        self.preview_task.take();
        self.reset_folder_preview();
        self.reset_pdf_preview();
        self.preview_wrap_task.take();
        self.preview_resize_task.take();
        self.preview_wrap = None;
        self.preview_text_scroll = UniformListScrollHandle::new();
        self.preview_state = PreviewState::Empty;
    }

    fn reset_folder_preview(&mut self) {
        self.folder_preview_task.take();
        self.folder_preview_entries.clear();
        self.folder_preview_loading = false;
        self.folder_preview_error = None;
        self.folder_preview_scroll = UniformListScrollHandle::new();
    }

    fn reset_pdf_preview(&mut self) {
        self.pdf_page_workers.clear();
        self.pdf_page_queue.clear();
        self.pdf_page_pending.clear();
        self.pdf_page_inflight.clear();
        while self.pdf_page_wake_receiver.try_recv().is_ok() {}
        self.pdf_pages.clear();
        self.pdf_scroll = UniformListScrollHandle::new();
    }

    fn preview_wrap_columns(&self) -> usize {
        let width = f32::from(self.preview_width.get());
        let width = if width > 0.0 {
            width
        } else {
            DEFAULT_PREVIEW_WIDTH
        };
        let cell_width = f32::from(self.preview_mono_cell_width.get()).max(1.0);
        ((width - PREVIEW_TEXT_CHROME_WIDTH) / cell_width)
            .floor()
            .max(16.0) as usize
    }

    fn update_preview_font_metrics(&mut self, window: &Window, cx: &mut Context<Self>) {
        let mono_font_size = cx.theme().mono_font_size;
        let layout = window.text_system().shape_line(
            "M".into(),
            mono_font_size,
            &[TextRun {
                len: 1,
                font: font(cx.theme().mono_font_family.clone()),
                color: Hsla::default(),
                background_color: None,
                strikethrough: None,
                underline: None,
            }],
            None,
        );
        let cell_width = layout.width.max(px(1.0));
        let measured_height = layout.ascent + layout.descent;
        let line_height = measured_height.max(mono_font_size * 1.5);

        if (self.preview_mono_cell_width.get() - cell_width).abs() >= px(0.1)
            || (self.preview_mono_line_height.get() - line_height).abs() >= px(0.1)
        {
            self.preview_mono_cell_width.set(cell_width);
            self.preview_mono_line_height.set(line_height);
            self.schedule_preview_wrap(cx);
        }
    }

    fn set_iosevka_ui(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if enabled && self.iosevka_ui_font.is_none() {
            return;
        }

        self.use_iosevka_ui = enabled;
        Theme::global_mut(cx).font_family = if enabled {
            self.iosevka_ui_font
                .clone()
                .unwrap_or_else(|| self.system_ui_font.clone())
        } else {
            self.system_ui_font.clone()
        };
        cx.refresh_windows();
    }

    fn places_sidebar_width(&self, window: &Window, cx: &Context<Self>) -> Pixels {
        let font_size = cx.theme().font_size * 0.875;
        let font = font(cx.theme().font_family.clone());
        let max_text_width = self
            .places
            .iter()
            .map(|place| place.label.clone())
            .chain(["Iosevka Mono".to_string(), "Finding places…".to_string()])
            .map(|label| {
                window
                    .text_system()
                    .shape_line(
                        label.clone().into(),
                        font_size,
                        &[TextRun {
                            len: label.len(),
                            font: font.clone(),
                            color: Hsla::default(),
                            background_color: None,
                            strikethrough: None,
                            underline: None,
                        }],
                        None,
                    )
                    .width
            })
            .map(f32::from)
            .fold(0.0, f32::max);

        // Outer padding + row padding + themed icon + gap. The footer switch
        // needs approximately the same remaining width as a place icon.
        px((max_text_width + 76.0).clamp(MIN_PLACES_WIDTH, MAX_PLACES_WIDTH))
    }

    fn visible_entry(&self, index: usize) -> Option<&FileEntry> {
        self.visible_entries
            .get(index)
            .and_then(|entry_index| self.entries.get(*entry_index))
    }

    fn visible_paths(&self) -> Vec<PathBuf> {
        self.visible_entries
            .iter()
            .filter_map(|index| self.entries.get(*index))
            .map(|entry| entry.path.clone())
            .collect()
    }

    fn rebuild_visible_entries(&mut self) {
        if self.filter_query.is_empty() {
            self.visible_entries = self
                .entries
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| {
                    (self.show_hidden || !is_hidden_name(&entry.name)).then_some(index)
                })
                .collect();
            return;
        }

        // Yazi keeps finder matches as derived state over the current folder
        // and catches that state up when the folder revision changes. Marcel
        // applies the same separation to a fuzzy-ranked visible-index layer.
        // Source (MIT, upstream commit e58022b9aafc8dabf586e2cc29b79a230071716f):
        // https://github.com/sxyazi/yazi/blob/e58022b9aafc8dabf586e2cc29b79a230071716f/yazi-core/src/tab/finder.rs
        let mut matches = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                if !self.show_hidden && is_hidden_name(&entry.name) {
                    return None;
                }
                fuzzy_score(&entry.name, &self.filter_query).map(|score| (index, score))
            })
            .collect::<Vec<_>>();
        matches.sort_unstable_by(|(left_index, left_score), (right_index, right_score)| {
            right_score
                .cmp(left_score)
                .then_with(|| left_index.cmp(right_index))
        });
        self.visible_entries = matches.into_iter().map(|(index, _)| index).collect();
    }

    fn reconcile_filter_selection(&mut self, cx: &mut Context<Self>) {
        let visible = self
            .visible_entries
            .iter()
            .filter_map(|index| self.entries.get(*index))
            .map(|entry| entry.path.as_path())
            .collect::<HashSet<_>>();
        self.selection.retain(|path| visible.contains(path));

        if self.selection.primary().is_some() {
            return;
        }

        let selected_entry = self
            .visible_entries
            .iter()
            .filter_map(|index| self.entries.get(*index))
            .find(|entry| self.selection.is_selected(&entry.path))
            .cloned();
        let entry = selected_entry.clone().or_else(|| {
            if self.filter_query.is_empty() {
                None
            } else {
                self.visible_entry(0).cloned()
            }
        });
        if let Some(entry) = entry {
            if selected_entry.is_some() {
                self.selection.make_primary(&entry.path);
            } else {
                self.selection.select_only(entry.path.clone());
            }
            self.start_preview(entry, cx);
        } else {
            self.selection.clear();
            self.clear_preview();
        }
    }

    fn set_filter_query(&mut self, query: String, cx: &mut Context<Self>) {
        if query == self.filter_query {
            return;
        }

        self.filter_query = query;
        self.rebuild_visible_entries();
        self.reconcile_filter_selection(cx);
        self.directory_scroll = UniformListScrollHandle::new();
        self.entry_hit_bounds.borrow_mut().clear();
        self.entry_content_bounds.borrow_mut().clear();
        self.marquee = None;
        self.marquee_scroll_task.take();
        cx.notify();
    }

    fn set_show_hidden(&mut self, show_hidden: bool, cx: &mut Context<Self>) {
        if self.show_hidden == show_hidden {
            return;
        }

        self.show_hidden = show_hidden;
        self.rebuild_visible_entries();
        self.reconcile_filter_selection(cx);
        self.directory_scroll = UniformListScrollHandle::new();
        self.entry_hit_bounds.borrow_mut().clear();
        self.entry_content_bounds.borrow_mut().clear();
        self.marquee = None;
        self.marquee_scroll_task.take();
        cx.notify();
    }

    fn on_search_input_event(
        &mut self,
        input: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => {
                let query = input.read(cx).value().to_string();
                let cleared = query.is_empty() && !self.filter_query.is_empty();
                self.set_filter_query(query, cx);
                if cleared {
                    self.browser_focus.focus(window);
                }
            }
            InputEvent::PressEnter { .. } => self.activate_primary(cx),
            InputEvent::Focus | InputEvent::Blur => {}
        }
    }

    fn focus_search(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_input
            .update(cx, |input, cx| input.focus(window, cx));
    }

    fn replace_search_text(&self, value: String, window: &mut Window, cx: &mut Context<Self>) {
        self.search_input
            .update(cx, |input, cx| input.set_value(value, window, cx));
    }

    fn clear_filter(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_search_text(String::new(), window, cx);
        self.browser_focus.focus(window);
    }

    fn on_window_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let stroke = &event.keystroke;
        let search_focused = self.search_input.focus_handle(cx).is_focused(window);
        let browser_focused = self.browser_focus.is_focused(window);

        if stroke.modifiers.control && !stroke.modifiers.alt && stroke.key.eq_ignore_ascii_case("f")
        {
            self.focus_search(window, cx);
            cx.stop_propagation();
            return;
        }

        if search_focused {
            match stroke.key.as_str() {
                "up" => {
                    self.execute_browser_command(BrowserCommand::MoveUp, cx);
                    cx.stop_propagation();
                }
                "down" => {
                    self.execute_browser_command(BrowserCommand::MoveDown, cx);
                    cx.stop_propagation();
                }
                _ => {}
            }
            return;
        }

        if !self.filter_query.is_empty() {
            match stroke.key.as_str() {
                "escape" if !browser_focused => {
                    self.clear_filter(window, cx);
                    cx.stop_propagation();
                    return;
                }
                "backspace" => {
                    let mut query = self.filter_query.clone();
                    query.pop();
                    self.replace_search_text(query, window, cx);
                    self.focus_search(window, cx);
                    cx.stop_propagation();
                    return;
                }
                "enter" if !browser_focused => {
                    self.activate_primary(cx);
                    cx.stop_propagation();
                    return;
                }
                "up" if !browser_focused => {
                    self.execute_browser_command(BrowserCommand::MoveUp, cx);
                    cx.stop_propagation();
                    return;
                }
                "down" if !browser_focused => {
                    self.execute_browser_command(BrowserCommand::MoveDown, cx);
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }

        if stroke.modifiers.control
            || stroke.modifiers.alt
            || stroke.modifiers.platform
            || stroke.modifiers.function
        {
            return;
        }
        let Some(text) = stroke.key_char.as_deref() else {
            return;
        };
        if text.chars().any(char::is_control)
            || (self.filter_query.is_empty() && text.chars().all(char::is_whitespace))
        {
            return;
        }

        let mut query = self.filter_query.clone();
        query.push_str(text);
        self.replace_search_text(query, window, cx);
        self.focus_search(window, cx);
        cx.stop_propagation();
    }

    fn start_preview_wrap(&mut self, cx: &mut Context<Self>) {
        let PreviewState::Ready(Preview::Text {
            lines,
            render_rich: false,
            ..
        }) = &self.preview_state
        else {
            self.preview_wrap_task.take();
            self.preview_wrap = None;
            return;
        };

        let columns = self.preview_wrap_columns();
        if self.preview_wrap.as_ref().is_some_and(|wrapped| {
            wrapped.ticket == self.preview_ticket && wrapped.columns == columns
        }) {
            return;
        }

        let lines = lines.clone();
        let ticket = self.preview_ticket;
        self.preview_wrap_task.take();
        let wrap_task = cx
            .background_executor()
            .spawn(smol::unblock(move || wrap_preview_lines(&lines, columns)));
        self.preview_wrap_task = Some(cx.spawn(async move |this, cx| {
            let lines = wrap_task.await;
            let _ = this.update(cx, |this, cx| {
                if ticket != this.preview_ticket || columns != this.preview_wrap_columns() {
                    return;
                }
                this.preview_wrap = Some(WrappedPreview {
                    ticket,
                    columns,
                    lines,
                });
                cx.notify();
            });
        }));
    }

    fn schedule_preview_wrap(&mut self, cx: &mut Context<Self>) {
        self.preview_resize_task.take();
        self.preview_resize_task = Some(cx.spawn(async move |this, cx| {
            Timer::after(PREVIEW_WRAP_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| this.start_preview_wrap(cx));
        }));
    }

    fn activate_entry(&mut self, path: &Path, event: &ClickEvent, cx: &mut Context<Self>) {
        // Right-click selection is resolved on mouse-down before the context
        // menu is built. Do not apply ordinary click modifiers a second time.
        if event.is_right_click() {
            return;
        }

        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .cloned()
        else {
            return;
        };

        let modifiers = event.modifiers();
        if modifiers.shift {
            let ordered = self.visible_paths();
            self.selection
                .select_range(entry.path.clone(), &ordered, modifiers.secondary());
        } else if modifiers.secondary() {
            self.selection.toggle(entry.path.clone());
        } else {
            self.selection.select_only(entry.path.clone());
        }

        if self.selection.primary() == Some(&entry.path) {
            self.start_preview(entry.clone(), cx);
        } else {
            self.clear_preview();
            cx.notify();
        }

        if event.click_count() >= 2 {
            self.open_entry(entry, cx);
        }
    }

    fn prepare_entry_context_menu(
        &mut self,
        path: &Path,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .cloned()
        else {
            return;
        };

        if self.selection.is_selected(path) {
            self.selection.make_primary(path);
        } else {
            self.selection.select_only(path.to_path_buf());
        }
        self.entry_menu = Some(EntryMenu {
            position,
            target: ContextMenuTarget::Entry,
        });
        self.start_preview(entry, cx);
    }

    fn prepare_directory_context_menu(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let Some(bounds) = self.browser_bounds.get() else {
            return;
        };
        if !bounds.contains(&event.position)
            || self
                .entry_hit_bounds
                .borrow()
                .values()
                .any(|entry_bounds| entry_bounds.contains(&event.position))
        {
            return;
        }

        self.clear_selection();
        self.entry_menu = Some(EntryMenu {
            position: event.position,
            target: ContextMenuTarget::CurrentDirectory,
        });
        cx.notify();
    }

    fn dismiss_entry_menu(&mut self, cx: &mut Context<Self>) {
        if self.entry_menu.take().is_some() {
            cx.notify();
        }
    }

    fn render_entry_menu(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.entry_menu?;
        let colors = cx.theme().colors;
        let radius = cx.theme().radius;
        let open_with_enabled = self.command_enabled(BrowserCommand::OpenWithSelection);
        let select_all_enabled = self.command_enabled(BrowserCommand::SelectAll);
        let window_size = window.bounds().size;
        let menu_height = match menu.target {
            ContextMenuTarget::Entry => ENTRY_MENU_HEIGHT,
            ContextMenuTarget::CurrentDirectory => DIRECTORY_MENU_HEIGHT,
        };
        let left = f32::from(menu.position.x)
            .min((f32::from(window_size.width) - ENTRY_MENU_WIDTH - ENTRY_MENU_MARGIN).max(0.0))
            .max(ENTRY_MENU_MARGIN);
        let top = f32::from(menu.position.y)
            .min((f32::from(window_size.height) - menu_height - ENTRY_MENU_MARGIN).max(0.0))
            .max(ENTRY_MENU_MARGIN);

        let planned = |label: &'static str| {
            div()
                .flex()
                .h(px(28.0))
                .items_center()
                .px_3()
                .text_sm()
                .text_color(colors.muted_foreground)
                .child(label)
        };
        let separator = || div().h(px(1.0)).mx_1().my_1().bg(colors.border);

        // gpui-component 0.5.1 hardcodes shadow_lg() in PopupMenu's private
        // popover style. GPUI 0.2.2 renders that shadow as opaque bands on our
        // Linux target and exposes no override, so this small shell is custom.
        // The active command still goes through Marcel's shared dispatcher.
        if menu.target == ContextMenuTarget::CurrentDirectory {
            return Some(
                div()
                    .id("directory-context-menu")
                    .absolute()
                    .left(px(left))
                    .top(px(top))
                    .w(px(ENTRY_MENU_WIDTH))
                    .p_1()
                    .rounded(radius)
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.popover)
                    .text_color(colors.popover_foreground)
                    .occlude()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.dismiss_entry_menu(cx);
                    }))
                    .child(planned("– New Folder"))
                    .child(planned("– New File"))
                    .child(planned("– Paste"))
                    .child(separator())
                    .child(
                        h_flex()
                            .id("directory-menu-select-all")
                            .h(px(28.0))
                            .px_3()
                            .rounded(radius)
                            .when(select_all_enabled, |this| {
                                this.cursor_pointer()
                                    .hover(|this| this.bg(colors.accent))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.entry_menu = None;
                                        this.execute_browser_command(BrowserCommand::SelectAll, cx);
                                    }))
                            })
                            .when(!select_all_enabled, |this| {
                                this.text_color(colors.muted_foreground)
                            })
                            .child("Select All")
                            .child(div().flex_1())
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(colors.muted_foreground)
                                    .child("Ctrl+A"),
                            ),
                    )
                    .child(
                        h_flex()
                            .id("directory-menu-refresh")
                            .h(px(28.0))
                            .px_3()
                            .rounded(radius)
                            .cursor_pointer()
                            .hover(|this| this.bg(colors.accent))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.start_directory_load(false, cx);
                            }))
                            .child("Refresh"),
                    )
                    .child(separator())
                    .child(
                        h_flex()
                            .id("directory-menu-show-hidden")
                            .h(px(28.0))
                            .px_3()
                            .rounded(radius)
                            .cursor_pointer()
                            .hover(|this| this.bg(colors.accent))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.entry_menu = None;
                                this.set_show_hidden(!this.show_hidden, cx);
                            }))
                            .child("Show Hidden Files")
                            .child(div().flex_1())
                            .when(self.show_hidden, |this| this.child("✓")),
                    )
                    .child(separator())
                    .child(planned("– Open in Terminal"))
                    .child(
                        h_flex()
                            .id("directory-menu-copy-location")
                            .h(px(28.0))
                            .px_3()
                            .rounded(radius)
                            .cursor_pointer()
                            .hover(|this| this.bg(colors.accent))
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    this.current_dir.display().to_string(),
                                ));
                                this.dismiss_entry_menu(cx);
                            }))
                            .child("Copy Location"),
                    )
                    .child(planned("– Properties"))
                    .into_any_element(),
            );
        }

        Some(
            div()
                .id("entry-context-menu")
                .absolute()
                .left(px(left))
                .top(px(top))
                .w(px(ENTRY_MENU_WIDTH))
                .p_1()
                .rounded(radius)
                .border_1()
                .border_color(colors.border)
                .bg(colors.popover)
                .text_color(colors.popover_foreground)
                .occlude()
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.dismiss_entry_menu(cx);
                }))
                .child(
                    h_flex()
                        .id("entry-menu-open")
                        .h(px(28.0))
                        .px_3()
                        .rounded(radius)
                        .cursor_pointer()
                        .hover(|this| this.bg(colors.accent))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.entry_menu = None;
                            this.execute_browser_command(BrowserCommand::ActivateSelection, cx);
                        }))
                        .child("Open")
                        .child(div().flex_1())
                        .child(
                            div()
                                .text_xs()
                                .text_color(colors.muted_foreground)
                                .child("Enter"),
                        ),
                )
                .child(
                    h_flex()
                        .id("entry-menu-open-with")
                        .h(px(28.0))
                        .px_3()
                        .rounded(radius)
                        .when(open_with_enabled, |this| {
                            this.cursor_pointer()
                                .hover(|this| this.bg(colors.accent))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.entry_menu = None;
                                    this.execute_browser_command(
                                        BrowserCommand::OpenWithSelection,
                                        cx,
                                    );
                                }))
                        })
                        .when(!open_with_enabled, |this| {
                            this.text_color(colors.muted_foreground)
                        })
                        .child("Open With…"),
                )
                .child(separator())
                .child(planned("– Cut"))
                .child(planned("– Copy"))
                .child(planned("– Paste"))
                .child(planned("– Duplicate"))
                .child(separator())
                .child(planned("– Rename…"))
                .child(planned("– Move To…"))
                .child(planned("– Move to Trash"))
                .child(planned("– Delete Permanently…"))
                .child(separator())
                .child(planned("– Create Link"))
                .child(planned("– Compress…"))
                .child(planned("– Copy Path"))
                .child(separator())
                .child(planned("– Properties"))
                .into_any_element(),
        )
    }

    fn begin_marquee(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let Some(bounds) = self.browser_bounds.get() else {
            return;
        };
        if !bounds.contains(&event.position)
            || self
                .entry_hit_bounds
                .borrow()
                .values()
                .any(|entry_bounds| entry_bounds.contains(&event.position))
        {
            return;
        }

        let additive = event.modifiers.secondary();
        let visible_paths = self
            .entry_hit_bounds
            .borrow()
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        self.entry_content_bounds
            .borrow_mut()
            .retain(|path, _| visible_paths.contains(path));
        let scroll = self.directory_scroll.0.borrow().base_handle.offset();
        let base_selection = self.selection.selected().clone();
        if !additive {
            self.selection.clear();
            self.clear_preview();
        }
        self.marquee = Some(MarqueeGesture {
            start_window: event.position,
            origin_content: event.position - bounds.origin - scroll,
            current_window: event.position,
            base_selection,
            additive,
            active: false,
        });
        cx.notify();
    }

    fn update_marquee(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !event.dragging() {
            return;
        }
        let Some(gesture) = self.marquee.as_mut() else {
            return;
        };

        gesture.current_window = event.position;
        let delta = gesture.current_window - gesture.start_window;
        if !gesture.active
            && delta.x.abs() < px(MARQUEE_THRESHOLD)
            && delta.y.abs() < px(MARQUEE_THRESHOLD)
        {
            return;
        }

        let became_active = !gesture.active;
        gesture.active = true;
        self.apply_marquee_selection(became_active, cx);
        if became_active {
            self.start_marquee_autoscroll(cx);
        }
    }

    fn apply_marquee_selection(&mut self, clear_preview: bool, cx: &mut Context<Self>) {
        let Some(bounds) = self.browser_bounds.get() else {
            return;
        };
        let Some(gesture) = self.marquee.as_ref() else {
            return;
        };
        let scroll = self.directory_scroll.0.borrow().base_handle.offset();
        let current_content = gesture.current_window - bounds.origin - scroll;
        let rectangle = marquee_bounds(gesture.origin_content, current_content);
        let base_selection = gesture.base_selection.clone();
        let additive = gesture.additive;
        let intersecting = self
            .entry_content_bounds
            .borrow()
            .iter()
            .filter(|(_, entry_bounds)| entry_bounds.intersects(&rectangle))
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();

        self.selection
            .replace_from_marquee(&base_selection, intersecting, additive);
        if clear_preview {
            self.clear_preview();
        }
        cx.notify();
    }

    fn start_marquee_autoscroll(&mut self, cx: &mut Context<Self>) {
        self.marquee_scroll_task.take();
        self.marquee_scroll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                Timer::after(MARQUEE_SCROLL_INTERVAL).await;
                let keep_running = this
                    .update(cx, |this, cx| this.tick_marquee_autoscroll(cx))
                    .unwrap_or(false);
                if !keep_running {
                    break;
                }
            }
        }));
    }

    fn tick_marquee_autoscroll(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(bounds) = self.browser_bounds.get() else {
            return false;
        };
        let Some(gesture) = self.marquee.as_ref() else {
            return false;
        };
        if !gesture.active {
            return true;
        }

        let delta = edge_scroll_delta(gesture.current_window.y, bounds);
        if delta == px(0.0) {
            return true;
        }

        let handle = self.directory_scroll.0.borrow().base_handle.clone();
        let mut offset = handle.offset();
        let old_offset = offset;
        offset.y = (offset.y + delta).clamp(-handle.max_offset().height, px(0.0));
        if offset == old_offset {
            return true;
        }

        handle.set_offset(offset);
        self.apply_marquee_selection(false, cx);
        true
    }

    fn end_marquee(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        if event.button == MouseButton::Left && self.marquee.take().is_some() {
            self.marquee_scroll_task.take();
            cx.notify();
        }
    }

    fn start_preview(&mut self, entry: FileEntry, cx: &mut Context<Self>) {
        self.preview_ticket = self.preview_ticket.wrapping_add(1);
        let ticket = self.preview_ticket;

        // Like Yazi's preview task, replacing this handle cancels the previous
        // foreground task. The ticket also prevents a late result from
        // becoming current:
        // https://github.com/sxyazi/yazi/blob/main/yazi-core/src/tab/preview.rs
        if let Some(cancel) = self.preview_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        self.preview_task.take();
        self.reset_folder_preview();
        self.reset_pdf_preview();
        self.preview_wrap_task.take();
        self.preview_resize_task.take();
        self.preview_wrap = None;
        self.preview_text_scroll = UniformListScrollHandle::new();
        self.preview_state = PreviewState::Loading {
            name: entry.name.clone(),
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        self.preview_cancel = Some(cancelled.clone());

        if entry.navigable {
            let path = entry.path;
            self.preview_state = PreviewState::Ready(Preview::Directory { path: path.clone() });
            self.start_folder_preview_load(path, ticket, cancelled, cx);
            cx.notify();
            return;
        }

        let load_task = cx
            .background_executor()
            .spawn(smol::unblock(move || load_preview(&entry, &cancelled)));

        self.preview_task = Some(cx.spawn(async move |this, cx| {
            let result = load_task.await;
            let _ = this.update(cx, |this, cx| {
                if ticket != this.preview_ticket {
                    return;
                }
                this.preview_state = match result {
                    Ok(preview) => PreviewState::Ready(preview),
                    Err(error) => PreviewState::Error(error.to_string()),
                };
                this.start_preview_wrap(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn start_folder_preview_load(
        &mut self,
        path: PathBuf,
        ticket: u64,
        cancelled: Arc<AtomicBool>,
        cx: &mut Context<Self>,
    ) {
        self.folder_preview_entries.clear();
        self.folder_preview_error = None;
        self.folder_preview_loading = true;
        self.folder_preview_scroll = UniformListScrollHandle::new();

        let (sender, receiver) = async_channel::unbounded();
        let stream_path = path.clone();
        cx.background_executor()
            .spawn(smol::unblock(move || {
                stream_directory_cancellable(&stream_path, sender, cancelled)
            }))
            .detach();

        // Yazi exposes a bounded slice of the hovered folder to its preview
        // layer and refreshes that folder independently from the main browser.
        // Marcel adapts that separation with a cancellable partial-update
        // stream and a virtualized, intentionally non-selectable preview.
        // Sources (upstream commit e58022b9aafc8dabf586e2cc29b79a230071716f):
        // https://github.com/sxyazi/yazi/blob/e58022b9aafc8dabf586e2cc29b79a230071716f/yazi-actor/src/lives/preview.rs
        // https://github.com/sxyazi/yazi/blob/e58022b9aafc8dabf586e2cc29b79a230071716f/yazi-actor/src/mgr/peek.rs
        self.folder_preview_task = Some(cx.spawn(async move |this, cx| {
            while let Ok(update) = receiver.recv().await {
                let should_continue = this
                    .update(cx, |this, cx| {
                        if ticket != this.preview_ticket {
                            return false;
                        }
                        let PreviewState::Ready(Preview::Directory { path: preview_path }) =
                            &this.preview_state
                        else {
                            return false;
                        };
                        if preview_path != &path {
                            return false;
                        }

                        let finished =
                            matches!(&update, DirectoryUpdate::Done | DirectoryUpdate::Error(_));
                        match update {
                            DirectoryUpdate::Batch(batch) => {
                                this.folder_preview_entries = merge_sorted_entries(
                                    std::mem::take(&mut this.folder_preview_entries),
                                    batch,
                                );
                            }
                            DirectoryUpdate::Done => {
                                this.folder_preview_loading = false;
                            }
                            DirectoryUpdate::Error(error) => {
                                this.folder_preview_loading = false;
                                this.folder_preview_error = Some(error);
                            }
                        }
                        cx.notify();
                        !finished
                    })
                    .unwrap_or(false);
                if !should_continue {
                    break;
                }
            }
        }));
    }

    fn activate_folder_preview_entry(
        &mut self,
        path: &Path,
        event: &ClickEvent,
        cx: &mut Context<Self>,
    ) {
        if event.is_right_click() || event.click_count() < 2 {
            return;
        }
        let Some(entry) = self
            .folder_preview_entries
            .iter()
            .find(|entry| entry.path == path)
            .cloned()
        else {
            return;
        };
        self.open_entry(entry, cx);
    }

    fn ensure_pdf_pages(&mut self, visible: Range<usize>, cx: &mut Context<Self>) {
        let PreviewState::Ready(Preview::Pdf { pages, .. }) = &self.preview_state else {
            return;
        };
        let pages = *pages;
        // Yazi prioritizes the currently visible page and treats PDF renders as
        // discardable preloads. Marcel applies that model to a continuous GUI
        // viewport and retains only a one-page lookahead:
        // https://github.com/sxyazi/yazi/blob/e58022b9aafc8dabf586e2cc29b79a230071716f/yazi-plugin/preset/plugins/pdf.lua
        let priority = prioritize_pdf_pages(visible, pages);

        self.pdf_page_queue.clear();
        self.pdf_page_pending = self.pdf_page_inflight.clone();
        for page in priority {
            if self.pdf_pages.contains_key(&page) || !self.pdf_page_pending.insert(page) {
                continue;
            }
            self.pdf_page_queue.push_back(page);
        }

        self.start_pdf_page_workers(cx);
        for _ in 0..PDF_PAGE_WORKERS {
            let _ = self.pdf_page_wake_sender.try_send(());
        }
    }

    fn start_pdf_page_workers(&mut self, cx: &mut Context<Self>) {
        if !self.pdf_page_workers.is_empty() {
            return;
        }

        let ticket = self.preview_ticket;
        let executor = cx.background_executor().clone();
        for _ in 0..PDF_PAGE_WORKERS {
            let executor = executor.clone();
            let wake = self.pdf_page_wake_receiver.clone();
            self.pdf_page_workers.push(cx.spawn(async move |this, cx| {
                loop {
                    let request = this
                        .update(cx, |this, _| {
                            let page = this.pdf_page_queue.pop_front()?;
                            let PreviewState::Ready(Preview::Pdf { source, .. }) =
                                &this.preview_state
                            else {
                                return None;
                            };
                            let cancelled = this.preview_cancel.clone()?;
                            this.pdf_page_inflight.insert(page);
                            Some((page, source.clone(), cancelled))
                        })
                        .ok()
                        .flatten();
                    let Some((page, source, cancelled)) = request else {
                        if wake.recv().await.is_err() {
                            break;
                        }
                        continue;
                    };

                    let result = executor
                        .spawn(smol::unblock(move || {
                            crate::pdf_preview::render_pdf_page(&source, page, &cancelled)
                        }))
                        .await;
                    let keep_running = this
                        .update(cx, |this, cx| {
                            if ticket != this.preview_ticket {
                                return false;
                            }
                            this.pdf_page_inflight.remove(&page);
                            this.pdf_page_pending.remove(&page);
                            let state = match result {
                                Ok(rendered) => PdfPageState::Ready(rendered.path),
                                Err(error) => PdfPageState::Failed(error.to_string()),
                            };
                            this.pdf_pages.insert(page, state);
                            cx.notify();
                            true
                        })
                        .unwrap_or(false);
                    if !keep_running {
                        break;
                    }
                }
            }));
        }
        for _ in 0..PDF_PAGE_WORKERS {
            let _ = self.pdf_page_wake_sender.try_send(());
        }
    }

    fn open_entry(&mut self, entry: FileEntry, cx: &mut Context<Self>) {
        if entry.navigable {
            self.navigate_to(entry.path, true, cx);
            return;
        }

        #[cfg(target_os = "linux")]
        {
            let path = entry.path;
            let ticket = self.preview_ticket;
            let open_task = cx
                .background_executor()
                .spawn(crate::system_open::open_file(path.clone()));
            cx.spawn(async move |this, cx| {
                if let Err(error) = open_task.await {
                    let _ = this.update(cx, |this, cx| {
                        if this.preview_ticket == ticket {
                            this.preview_state = PreviewState::Error(error.to_string());
                            cx.notify();
                        }
                    });
                }
            })
            .detach();
        }

        #[cfg(not(target_os = "linux"))]
        cx.open_with_system(&entry.path);
    }

    fn open_primary_with(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self
            .selection
            .primary()
            .and_then(|path| self.entries.iter().find(|entry| &entry.path == path))
            .filter(|entry| !entry.navigable)
            .map(|entry| entry.path.clone())
        else {
            return;
        };

        #[cfg(target_os = "linux")]
        {
            let open_task = cx
                .background_executor()
                .spawn(crate::system_open::open_file_with(path.clone()));
            cx.spawn(async move |this, cx| {
                if let Err(error) = open_task.await {
                    let _ = this.update(cx, |this, cx| {
                        if this.selection.primary() == Some(&path) {
                            this.preview_state = PreviewState::Error(error.to_string());
                            cx.notify();
                        }
                    });
                }
            })
            .detach();
        }
    }

    fn render_place(&self, index: usize, place: Place, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let radius = cx.theme().radius;
        let active = self.current_dir == place.path;
        let icon = self
            .place_icons
            .get(&place.path)
            .cloned()
            .map(|path| {
                img(path)
                    .size(px(20.0))
                    .object_fit(ObjectFit::Contain)
                    .into_any_element()
            })
            .unwrap_or_else(|| {
                div()
                    .w(px(20.0))
                    .text_color(colors.primary)
                    .child("▸")
                    .into_any_element()
            });

        // gpui-component's Button centers its inner label by design and its
        // SidebarMenu only accepts bundled SVG assets. Places need
        // left-aligned rows and icons from the active freedesktop theme, so
        // this small navigation surface is intentionally Marcel-owned.
        h_flex()
            .id(("place", index))
            .w_full()
            .h_8()
            .px_2()
            .gap_2()
            .rounded(radius)
            .cursor_pointer()
            .hover(|this| {
                this.bg(colors.sidebar_accent.opacity(0.8))
                    .text_color(colors.sidebar_accent_foreground)
            })
            .when(active, |this| {
                this.bg(colors.sidebar_accent)
                    .text_color(colors.sidebar_accent_foreground)
            })
            .child(icon)
            .child(div().flex_none().text_sm().child(place.label))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.navigate_to(place.path.clone(), true, cx);
            }))
            .into_any_element()
    }

    fn render_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let radius = cx.theme().radius;
        uniform_list(
            "directory-entries",
            self.visible_entries.len(),
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                range
                    .filter_map(|index| {
                        let entry = this.visible_entry(index)?.clone();
                        let path = entry.path.clone();
                        let click_path = path.clone();
                        let context_path = path.clone();
                        let bounds_path = path.clone();
                        let selected = this.selection.is_selected(&path);
                        let entry_hit_bounds = this.entry_hit_bounds.clone();
                        let entry_content_bounds = this.entry_content_bounds.clone();
                        let browser_bounds = this.browser_bounds.clone();
                        let directory_scroll = this.directory_scroll.clone();
                        let icon = if let Some(icon_path) = entry.icon_path.clone() {
                            img(icon_path)
                                .size(px(20.0))
                                .object_fit(ObjectFit::Contain)
                                .into_any_element()
                        } else {
                            div()
                                .w(px(20.0))
                                .text_color(colors.primary)
                                .child(entry.icon())
                                .into_any_element()
                        };

                        // gpui-component's ListItem intentionally owns the full row width.
                        // Marcel needs the unused row canvas to remain a marquee start
                        // target, so this content-sized entry is a deliberate custom
                        // interaction surface.
                        let item = h_flex()
                            .id(("entry", index))
                            .relative()
                            .h(px(32.0))
                            .max_w(px(640.0))
                            .px_3()
                            .gap_2()
                            .rounded(radius)
                            .border_1()
                            .border_color(colors.list_active_border.opacity(0.0))
                            .cursor_pointer()
                            .hover(|this| this.bg(colors.list_hover))
                            .when(selected, |this| {
                                this.bg(colors.list_active)
                                    .border_color(colors.list_active_border)
                            })
                            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                                this.activate_entry(&click_path, event, cx);
                            }))
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                    this.browser_focus.focus(window);
                                    this.prepare_entry_context_menu(
                                        &context_path,
                                        event.position,
                                        cx,
                                    );
                                }),
                            )
                            .child(icon)
                            .child(
                                div()
                                    .max_w(px(480.0))
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .child(entry.name),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(colors.muted_foreground)
                                    .child(format_size(entry.size)),
                            )
                            .child(
                                canvas(
                                    move |bounds, _, _| {
                                        entry_hit_bounds
                                            .borrow_mut()
                                            .insert(bounds_path.clone(), bounds);
                                        if let Some(browser) = browser_bounds.get() {
                                            let scroll =
                                                directory_scroll.0.borrow().base_handle.offset();
                                            entry_content_bounds.borrow_mut().insert(
                                                bounds_path.clone(),
                                                Bounds {
                                                    origin: bounds.origin - browser.origin - scroll,
                                                    size: bounds.size,
                                                },
                                            );
                                        }
                                    },
                                    |_, _, _, _| {},
                                )
                                .absolute()
                                .inset_0(),
                            );

                        Some(
                            div()
                                .flex()
                                .h(px(DIRECTORY_ROW_HEIGHT))
                                .w_full()
                                .items_center()
                                .child(item),
                        )
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(self.directory_scroll.clone())
        .h_full()
        .into_any_element()
    }

    fn render_grid(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let radius = cx.theme().radius;
        let columns = self.grid_columns();
        if columns != self.grid_layout_columns {
            self.grid_layout_columns = columns;
            self.entry_content_bounds.borrow_mut().clear();
        }
        let row_count = self.visible_entries.len().div_ceil(columns);

        uniform_list(
            "directory-grid-rows",
            row_count,
            cx.processor(move |this, rows: Range<usize>, _window, cx| {
                let thumbnail_start = rows.start.saturating_sub(1) * columns;
                let thumbnail_end = ((rows.end + 1) * columns).min(this.visible_entries.len());
                let visible_start = rows.start * columns;
                let visible_end = (rows.end * columns).min(this.visible_entries.len());
                this.ensure_thumbnails(
                    visible_start..visible_end,
                    thumbnail_start..thumbnail_end,
                    cx,
                );

                rows.map(|row| {
                    let start = row * columns;
                    let end = (start + columns).min(this.visible_entries.len());
                    let tiles = (start..end)
                        .filter_map(|index| {
                            let entry = this.visible_entry(index)?.clone();
                            let path = entry.path.clone();
                            let click_path = path.clone();
                            let context_path = path.clone();
                            let bounds_path = path.clone();
                            let selected = this.selection.is_selected(&path);
                            let display_name = elide_filename(&entry.name, GRID_LABEL_COLUMNS);
                            let entry_hit_bounds = this.entry_hit_bounds.clone();
                            let entry_content_bounds = this.entry_content_bounds.clone();
                            let browser_bounds = this.browser_bounds.clone();
                            let directory_scroll = this.directory_scroll.clone();

                            let visual = match this.thumbnails.get(&path) {
                                Some(ThumbnailState::Ready(thumbnail)) => div()
                                    .flex()
                                    .flex_none()
                                    .size(px(88.0))
                                    .items_center()
                                    .justify_center()
                                    .overflow_hidden()
                                    .rounded(radius)
                                    .child(
                                        img(thumbnail.clone())
                                            .size_full()
                                            .object_fit(ObjectFit::Contain),
                                    )
                                    .into_any_element(),
                                Some(ThumbnailState::Failed) | None => {
                                    if let Some(icon_path) = entry.icon_path.clone() {
                                        div()
                                            .flex()
                                            .flex_none()
                                            .size(px(88.0))
                                            .items_center()
                                            .justify_center()
                                            .child(
                                                img(icon_path)
                                                    .size(px(56.0))
                                                    .object_fit(ObjectFit::Contain),
                                            )
                                            .into_any_element()
                                    } else {
                                        div()
                                            .flex()
                                            .flex_none()
                                            .size(px(88.0))
                                            .items_center()
                                            .justify_center()
                                            .text_3xl()
                                            .text_color(colors.primary)
                                            .child(entry.icon())
                                            .into_any_element()
                                    }
                                }
                            };

                            Some(
                                div()
                                    .id(("grid-entry", index))
                                    .relative()
                                    .flex()
                                    .flex_col()
                                    .items_center()
                                    .w(px(GRID_TILE_WIDTH))
                                    .h(px(GRID_TILE_HEIGHT))
                                    .p_2()
                                    .gap_1()
                                    .rounded(radius)
                                    .border_1()
                                    .border_color(colors.list_active_border.opacity(0.0))
                                    .cursor_pointer()
                                    .hover(|this| this.bg(colors.list_hover))
                                    .when(selected, |this| {
                                        this.bg(colors.list_active)
                                            .border_color(colors.list_active_border)
                                    })
                                    .on_click(cx.listener(
                                        move |this, event: &ClickEvent, _, cx| {
                                            this.activate_entry(&click_path, event, cx);
                                        },
                                    ))
                                    .on_mouse_down(
                                        MouseButton::Right,
                                        cx.listener(
                                            move |this, event: &MouseDownEvent, window, cx| {
                                                this.browser_focus.focus(window);
                                                this.prepare_entry_context_menu(
                                                    &context_path,
                                                    event.position,
                                                    cx,
                                                );
                                            },
                                        ),
                                    )
                                    .child(visual)
                                    .child(
                                        div()
                                            .w_full()
                                            .max_w(px(GRID_TILE_WIDTH - 16.0))
                                            .h(px(GRID_LABEL_HEIGHT))
                                            .flex_none()
                                            .overflow_hidden()
                                            .whitespace_normal()
                                            .line_clamp(2)
                                            .text_center()
                                            .text_xs()
                                            .child(display_name),
                                    )
                                    .child(
                                        canvas(
                                            move |bounds, _, _| {
                                                entry_hit_bounds
                                                    .borrow_mut()
                                                    .insert(bounds_path.clone(), bounds);
                                                if let Some(browser) = browser_bounds.get() {
                                                    let scroll = directory_scroll
                                                        .0
                                                        .borrow()
                                                        .base_handle
                                                        .offset();
                                                    entry_content_bounds.borrow_mut().insert(
                                                        bounds_path.clone(),
                                                        Bounds {
                                                            origin: bounds.origin
                                                                - browser.origin
                                                                - scroll,
                                                            size: bounds.size,
                                                        },
                                                    );
                                                }
                                            },
                                            |_, _, _, _| {},
                                        )
                                        .absolute()
                                        .inset_0(),
                                    ),
                            )
                        })
                        .collect::<Vec<_>>();

                    h_flex()
                        .h(px(GRID_ROW_HEIGHT))
                        .w_full()
                        .px(px(GRID_SIDE_PADDING))
                        .items_start()
                        .gap(px(GRID_GAP))
                        .children(tiles)
                })
                .collect::<Vec<_>>()
            }),
        )
        .track_scroll(self.directory_scroll.clone())
        .h_full()
        .into_any_element()
    }

    fn render_browser(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let empty_message = self.visible_entries.is_empty().then(|| {
            if let Some(error) = &self.directory_error {
                format!("Could not read this folder\n{error}")
            } else if self.directory_loading && self.entries.is_empty() {
                "Loading folder…".to_string()
            } else if !self.filter_query.is_empty() {
                format!("No matches for “{}”", self.filter_query)
            } else if !self.show_hidden && !self.entries.is_empty() {
                "This folder contains only hidden files".to_string()
            } else {
                "This folder is empty".to_string()
            }
        });

        let bounds_state = self.browser_bounds.clone();
        let visible_hit_bounds = self.entry_hit_bounds.clone();
        let gesture_view = cx.entity();
        let marquee = self.marquee.as_ref().and_then(|gesture| {
            if !gesture.active {
                return None;
            }
            let bounds = self.browser_bounds.get()?;
            let scroll = self.directory_scroll.0.borrow().base_handle.offset();
            let origin_window = bounds.origin + gesture.origin_content + scroll;
            let rectangle = marquee_bounds(origin_window, gesture.current_window);
            let left = rectangle.left().max(bounds.left());
            let right = rectangle.right().min(bounds.right());
            let top = rectangle.top().max(bounds.top());
            let bottom = rectangle.bottom().min(bounds.bottom());
            if right < left || bottom < top {
                return None;
            }

            Some(
                div()
                    .absolute()
                    .left(left - bounds.left())
                    .top(top - bounds.top())
                    .w(right - left)
                    .h(bottom - top)
                    .border_1()
                    .border_color(colors.primary.opacity(0.8))
                    .bg(colors.primary.opacity(0.16))
                    .into_any_element(),
            )
        });
        let contents = match empty_message {
            Some(message) => div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_color(colors.muted_foreground)
                .child(message)
                .into_any_element(),
            None => match self.view_mode {
                ViewMode::List => self.render_list(cx),
                ViewMode::Grid => self.render_grid(cx),
            },
        };
        let directory_scroll = self.directory_scroll.clone();

        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event, _, cx| this.begin_marquee(event, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event, _, cx| {
                    this.prepare_directory_context_menu(event, cx);
                }),
            )
            .child(
                canvas(
                    move |bounds, _, _| {
                        bounds_state.set(Some(bounds));
                        visible_hit_bounds.borrow_mut().clear();
                    },
                    move |_, _, window, _| {
                        let move_view = gesture_view.clone();
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _window, cx| {
                            if phase.bubble() {
                                move_view.update(cx, |this, cx| this.update_marquee(event, cx));
                            }
                        });

                        let up_view = gesture_view.clone();
                        window.on_mouse_event(move |event: &MouseUpEvent, phase, _window, cx| {
                            if phase.bubble() {
                                up_view.update(cx, |this, cx| this.end_marquee(event, cx));
                            }
                        });
                    },
                )
                .absolute()
                .inset_0(),
            )
            .child(contents)
            .when_some(marquee, |this, marquee| this.child(marquee))
            .when(self.directory_loading, |this| {
                this.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(colors.muted_foreground)
                        .child(if self.filter_query.is_empty() {
                            format!("Loading… {} items", self.entries.len())
                        } else {
                            format!(
                                "Loading… {} of {} items match",
                                self.visible_entries.len(),
                                self.entries.len()
                            )
                        }),
                )
            })
            .vertical_scrollbar(&directory_scroll)
            .into_any_element()
    }

    fn render_folder_preview(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let radius = cx.theme().radius;

        if self.folder_preview_entries.is_empty() {
            if let Some(error) = &self.folder_preview_error {
                return centered_preview_message(
                    format!("Could not read this folder\n{error}"),
                    colors.danger,
                );
            }
            if self.folder_preview_loading {
                return centered_preview_message(
                    "Loading folder contents…",
                    colors.muted_foreground,
                );
            }
            return centered_preview_message("This folder is empty", colors.muted_foreground);
        }

        let scroll = self.folder_preview_scroll.clone();
        let ticket = self.preview_ticket;
        div()
            .relative()
            .size_full()
            .child(
                uniform_list(
                    ("folder-preview-entries", ticket),
                    self.folder_preview_entries.len(),
                    cx.processor(move |this, range: Range<usize>, _, cx| {
                        range
                            .filter_map(|index| {
                                let entry = this.folder_preview_entries.get(index)?.clone();
                                let click_path = entry.path.clone();
                                let icon = if let Some(icon_path) = entry.icon_path.clone() {
                                    img(icon_path)
                                        .size(px(20.0))
                                        .object_fit(ObjectFit::Contain)
                                        .into_any_element()
                                } else {
                                    div()
                                        .w(px(20.0))
                                        .text_color(colors.primary)
                                        .child(entry.icon())
                                        .into_any_element()
                                };
                                let detail = format_size(entry.size);
                                let detail = if detail.is_empty() {
                                    entry.display_kind().to_string()
                                } else {
                                    detail
                                };

                                Some(
                                    h_flex()
                                        .id(("folder-preview-entry", index))
                                        .h(px(DIRECTORY_ROW_HEIGHT))
                                        .mx_2()
                                        .px_2()
                                        .gap_2()
                                        .rounded(radius)
                                        .cursor_pointer()
                                        .hover(|this| this.bg(colors.list_hover))
                                        .on_click(cx.listener(
                                            move |this, event: &ClickEvent, _, cx| {
                                                this.activate_folder_preview_entry(
                                                    &click_path,
                                                    event,
                                                    cx,
                                                );
                                            },
                                        ))
                                        .child(icon)
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .overflow_hidden()
                                                .text_ellipsis()
                                                .whitespace_nowrap()
                                                .child(entry.name),
                                        )
                                        .child(
                                            div()
                                                .flex_none()
                                                .text_xs()
                                                .text_color(colors.muted_foreground)
                                                .child(detail),
                                        ),
                                )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(scroll.clone())
                .size_full()
                .py_2(),
            )
            .vertical_scrollbar(&scroll)
            .into_any_element()
    }

    fn render_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.update_preview_font_metrics(window, cx);
        let colors = cx.theme().colors;
        match &self.preview_state {
            PreviewState::Empty => {
                centered_preview_message("Select a file to preview", colors.muted_foreground)
            }
            PreviewState::Loading { name } => {
                centered_preview_message(format!("Loading {name}…"), colors.muted_foreground)
            }
            PreviewState::Error(error) => {
                centered_preview_message(format!("Preview failed\n{error}"), colors.danger)
            }
            PreviewState::Ready(Preview::Directory { .. }) => self.render_folder_preview(cx),
            PreviewState::Ready(Preview::Image { path, .. }) => {
                let muted = colors.muted_foreground;
                let danger = colors.danger;
                img(path.clone())
                    .id(("preview-image", self.preview_ticket))
                    .size_full()
                    .object_fit(ObjectFit::Contain)
                    .with_loading(move || centered_preview_message("Decoding image…", muted))
                    .with_fallback(move || {
                        centered_preview_message("This image could not be decoded", danger)
                    })
                    .into_any_element()
            }
            PreviewState::Ready(Preview::Pdf { pages, .. }) => {
                let pages = *pages;
                let scroll = self.pdf_scroll.clone();
                let page_width = (f32::from(self.preview_width.get()) - 40.0).max(240.0);
                let page_height = px((page_width * 1.414).clamp(340.0, 1_240.0));
                let ticket = self.preview_ticket;

                div()
                    .relative()
                    .size_full()
                    .child(
                        uniform_list(
                            ("preview-pdf-pages", ticket),
                            pages,
                            cx.processor(move |this, range: Range<usize>, _, cx| {
                                this.ensure_pdf_pages(range.clone(), cx);
                                range
                                    .map(|index| {
                                        let page = index + 1;
                                        let content = match this.pdf_pages.get(&page).cloned() {
                                            Some(PdfPageState::Ready(path)) => img(path)
                                                .id(("pdf-page-image", page))
                                                .size_full()
                                                .object_fit(ObjectFit::Contain)
                                                .with_loading({
                                                    let muted = colors.muted_foreground;
                                                    move || {
                                                        centered_preview_message(
                                                            format!("Loading page {page}…"),
                                                            muted,
                                                        )
                                                    }
                                                })
                                                .with_fallback({
                                                    let danger = colors.danger;
                                                    move || {
                                                        centered_preview_message(
                                                            format!(
                                                                "Page {page} could not be decoded"
                                                            ),
                                                            danger,
                                                        )
                                                    }
                                                })
                                                .into_any_element(),
                                            Some(PdfPageState::Failed(error)) => {
                                                centered_preview_message(
                                                    format!(
                                                        "Page {page} failed to render\n{error}"
                                                    ),
                                                    colors.danger,
                                                )
                                            }
                                            None => centered_preview_message(
                                                format!("Rendering page {page}…"),
                                                colors.muted_foreground,
                                            ),
                                        };

                                        div()
                                            .flex()
                                            .w_full()
                                            .h(page_height)
                                            .px_3()
                                            .py_2()
                                            .items_center()
                                            .justify_center()
                                            .child(content)
                                    })
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .track_scroll(scroll.clone())
                        .size_full(),
                    )
                    .vertical_scrollbar(&scroll)
                    .into_any_element()
            }
            PreviewState::Ready(Preview::Text {
                contents,
                language,
                markdown,
                render_rich,
                ..
            }) => {
                if *render_rich {
                    let source = if *markdown {
                        contents.clone()
                    } else {
                        code_fence(contents, language)
                    };
                    TextView::markdown(("preview-text", self.preview_ticket), source, window, cx)
                        .selectable(true)
                        .scrollable(true)
                        .size_full()
                        .p_3()
                        .into_any_element()
                } else {
                    let Some(wrapped) = self.preview_wrap.as_ref().filter(|wrapped| {
                        wrapped.ticket == self.preview_ticket
                            && wrapped.columns == self.preview_wrap_columns()
                    }) else {
                        return centered_preview_message(
                            "Preparing wrapped preview…",
                            colors.muted_foreground,
                        );
                    };
                    let lines = wrapped.lines.clone();
                    let scroll = self.preview_text_scroll.clone();
                    let foreground = colors.foreground;
                    let muted = colors.muted_foreground;
                    let mono_font = cx.theme().mono_font_family.clone();
                    let mono_font_size = cx.theme().mono_font_size;
                    let mono_line_height = self.preview_mono_line_height.get();

                    div()
                        .relative()
                        .size_full()
                        .child(
                            uniform_list(
                                ("preview-text-lines", self.preview_ticket),
                                lines.len(),
                                move |range, _, _| {
                                    range
                                        .map(|index| {
                                            let line = &lines[index];
                                            div()
                                                .flex()
                                                .h(mono_line_height)
                                                .w_full()
                                                .items_center()
                                                .font_family(mono_font.clone())
                                                .text_size(mono_font_size)
                                                .child(
                                                    div()
                                                        .w(px(52.0))
                                                        .flex_none()
                                                        .pr_3()
                                                        .text_color(muted)
                                                        .child(
                                                            line.source_line
                                                                .map(|line| format!("{line:>4}"))
                                                                .unwrap_or_default(),
                                                        ),
                                                )
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .overflow_hidden()
                                                        .whitespace_nowrap()
                                                        .text_color(foreground)
                                                        .child(line.contents.clone()),
                                                )
                                        })
                                        .collect::<Vec<_>>()
                                },
                            )
                            .track_scroll(scroll.clone())
                            .size_full()
                            .px_3()
                            .py_2(),
                        )
                        .vertical_scrollbar(&scroll)
                        .into_any_element()
                }
            }
            PreviewState::Ready(Preview::Metadata { summary }) => {
                centered_preview_message(summary.clone(), colors.muted_foreground)
            }
        }
    }
}

impl Render for Marcel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.search_input.read(cx).value().as_ref() != self.filter_query {
            self.search_input.update(cx, |input, cx| {
                input.set_value(self.filter_query.clone(), window, cx);
            });
        }

        let colors = cx.theme().colors;
        let window_width = f32::from(window.bounds().size.width);
        let sidebar_width = self.places_sidebar_width(window, cx);
        let workspace_width =
            (window_width - f32::from(sidebar_width)).max(MIN_BROWSER_WIDTH + MIN_PREVIEW_WIDTH);
        let browser_default = px(workspace_width * 0.6);
        let preview_default = px(workspace_width * 0.4);
        if self.preview_width.get() == px(0.0) {
            self.preview_width.set(preview_default);
        }
        let place_buttons = self
            .places
            .clone()
            .into_iter()
            .enumerate()
            .map(|(index, place)| self.render_place(index, place, cx))
            .collect::<Vec<_>>();
        let font_view = cx.entity();
        let font_switch = Switch::new("iosevka-ui-font")
            .small()
            .label("Iosevka Mono")
            .checked(self.use_iosevka_ui)
            .disabled(self.iosevka_ui_font.is_none())
            .tooltip(if self.iosevka_ui_font.is_some() {
                "Use Iosevka Mono for the interface"
            } else {
                "Iosevka is not installed"
            })
            .on_click(move |checked, _, cx| {
                font_view.update(cx, |this, cx| {
                    this.set_iosevka_ui(*checked, cx);
                });
            });
        let hidden_switch_view = cx.entity();
        let hidden_switch = Switch::new("show-hidden-files")
            .small()
            .label("Show Hidden")
            .checked(self.show_hidden)
            .tooltip("Show files whose names begin with a dot")
            .on_click(move |checked, _, cx| {
                hidden_switch_view.update(cx, |this, cx| {
                    this.set_show_hidden(*checked, cx);
                });
            });
        let view_switch_view = cx.entity();
        let view_switch = h_flex()
            .gap_2()
            .font_family(cx.theme().mono_font_family.clone())
            .line_height(relative(1.0))
            .child(
                div()
                    .w_5()
                    .text_center()
                    .text_lg()
                    .text_color(if self.view_mode == ViewMode::List {
                        colors.sidebar_primary
                    } else {
                        colors.muted_foreground
                    })
                    .child("☷"),
            )
            .child(
                Switch::new("browser-view-mode")
                    .small()
                    .checked(self.view_mode == ViewMode::Grid)
                    .tooltip("Switch between list and icon views")
                    .on_click(move |checked, _, cx| {
                        view_switch_view.update(cx, |this, cx| {
                            this.set_view_mode(
                                if *checked {
                                    ViewMode::Grid
                                } else {
                                    ViewMode::List
                                },
                                cx,
                            );
                        });
                    }),
            )
            .child(
                div()
                    .w_5()
                    .text_center()
                    .text_lg()
                    .text_color(if self.view_mode == ViewMode::Grid {
                        colors.sidebar_primary
                    } else {
                        colors.muted_foreground
                    })
                    .child("▦"),
            );

        let sidebar = div()
            .flex()
            .flex_col()
            .flex_none()
            .w(sidebar_width)
            .h_full()
            .p_4()
            .gap_2()
            .bg(colors.sidebar)
            .border_r_1()
            .border_color(colors.sidebar_border)
            .text_color(colors.sidebar_foreground)
            .child(
                div()
                    .text_sm()
                    .text_color(colors.muted_foreground)
                    .child("Places"),
            )
            .children(place_buttons)
            .when(self.places_loading, |this| {
                this.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(colors.muted_foreground)
                        .child("Finding places…"),
                )
            })
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(font_switch)
                    .child(hidden_switch)
                    .child(view_switch),
            );

        let browser = div()
            .id("browser-pane")
            .key_context(BROWSER_KEY_CONTEXT)
            .track_focus(&self.browser_focus)
            .on_action(cx.listener(Self::on_move_up))
            .on_action(cx.listener(Self::on_move_down))
            .on_action(cx.listener(Self::on_move_left))
            .on_action(cx.listener(Self::on_move_right))
            .on_action(cx.listener(Self::on_extend_up))
            .on_action(cx.listener(Self::on_extend_down))
            .on_action(cx.listener(Self::on_extend_left))
            .on_action(cx.listener(Self::on_extend_right))
            .on_action(cx.listener(Self::on_extend_to_first))
            .on_action(cx.listener(Self::on_extend_to_last))
            .on_action(cx.listener(Self::on_extend_page_up))
            .on_action(cx.listener(Self::on_extend_page_down))
            .on_action(cx.listener(Self::on_select_first))
            .on_action(cx.listener(Self::on_select_last))
            .on_action(cx.listener(Self::on_select_page_up))
            .on_action(cx.listener(Self::on_select_page_down))
            .on_action(cx.listener(Self::on_activate_selection))
            .on_action(cx.listener(Self::on_open_with_selection))
            .on_action(cx.listener(Self::on_clear_selection))
            .on_action(cx.listener(Self::on_go_to_parent))
            .on_action(cx.listener(Self::on_go_back))
            .on_action(cx.listener(Self::on_go_forward))
            .on_action(cx.listener(Self::on_select_all))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, _| this.browser_focus.focus(window)),
            )
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .p_3()
            .bg(colors.background)
            .border_r_1()
            .border_color(colors.border)
            .child(self.render_browser(cx));

        // gpui-component's icon-only Button currently paints its bundled SVG
        // navigation glyphs invisibly against Marcel's themed title surface.
        // Keep these four tiny controls Marcel-owned until the component can
        // preserve the supplied semantic foreground color.
        let topbar = h_flex()
            .flex_none()
            .h(px(40.0))
            .w_full()
            .bg(colors.sidebar)
            .border_b_1()
            .border_color(colors.border)
            .text_color(colors.sidebar_foreground)
            .child(
                h_flex()
                    .flex_none()
                    .w(sidebar_width)
                    .h_full()
                    .px_2()
                    .gap_1()
                    .border_r_1()
                    .border_color(colors.sidebar_border)
                    .font_family(cx.theme().mono_font_family.clone())
                    .line_height(relative(1.0))
                    .child(
                        div()
                            .id("back")
                            .flex()
                            .flex_none()
                            .size(px(28.0))
                            .items_center()
                            .justify_center()
                            .rounded(cx.theme().radius)
                            .text_lg()
                            .text_color(if self.history.can_go_back() {
                                colors.sidebar_foreground
                            } else {
                                colors.muted_foreground
                            })
                            .child("←")
                            .when(self.history.can_go_back(), |button| {
                                button
                                    .cursor_pointer()
                                    .hover(|button| button.bg(colors.sidebar_accent))
                                    .on_click(cx.listener(|this, _, _, cx| this.go_back(cx)))
                            }),
                    )
                    .child(
                        div()
                            .id("forward")
                            .flex()
                            .flex_none()
                            .size(px(28.0))
                            .items_center()
                            .justify_center()
                            .rounded(cx.theme().radius)
                            .text_lg()
                            .text_color(if self.history.can_go_forward() {
                                colors.sidebar_foreground
                            } else {
                                colors.muted_foreground
                            })
                            .child("→")
                            .when(self.history.can_go_forward(), |button| {
                                button
                                    .cursor_pointer()
                                    .hover(|button| button.bg(colors.sidebar_accent))
                                    .on_click(cx.listener(|this, _, _, cx| this.go_forward(cx)))
                            }),
                    )
                    .child(
                        div()
                            .id("up")
                            .flex()
                            .flex_none()
                            .size(px(28.0))
                            .items_center()
                            .justify_center()
                            .rounded(cx.theme().radius)
                            .text_lg()
                            .text_color(if self.current_dir.parent().is_some() {
                                colors.sidebar_foreground
                            } else {
                                colors.muted_foreground
                            })
                            .child("↑")
                            .when(self.current_dir.parent().is_some(), |button| {
                                button
                                    .cursor_pointer()
                                    .hover(|button| button.bg(colors.sidebar_accent))
                                    .on_click(cx.listener(|this, _, _, cx| this.go_up(cx)))
                            }),
                    )
                    .child(
                        div()
                            .id("refresh")
                            .flex()
                            .flex_none()
                            .size(px(28.0))
                            .items_center()
                            .justify_center()
                            .rounded(cx.theme().radius)
                            .text_lg()
                            .text_color(colors.sidebar_foreground)
                            .child("↻")
                            .cursor_pointer()
                            .hover(|button| button.bg(colors.sidebar_accent))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.start_directory_load(false, cx);
                            })),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .px_2()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_7()
                            .px_3()
                            .flex()
                            .items_center()
                            .rounded(cx.theme().radius)
                            .bg(colors.background)
                            .border_1()
                            .border_color(colors.border)
                            .text_sm()
                            .text_color(colors.muted_foreground)
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(self.current_dir.display().to_string()),
                    )
                    .child(
                        Input::new(&self.search_input)
                            .small()
                            .cleanable(true)
                            .h_7()
                            .w(px(240.0)),
                    ),
            );

        let selected_entry = self
            .selection
            .primary()
            .and_then(|path| self.entries.iter().find(|entry| &entry.path == path));
        let preview_name = selected_entry.map(|entry| entry.name.clone());
        let preview_details = selected_entry.map(|entry| {
            if entry.navigable {
                let folders = self
                    .folder_preview_entries
                    .iter()
                    .filter(|entry| entry.navigable)
                    .count();
                let files = self.folder_preview_entries.len().saturating_sub(folders);
                let progress = if self.folder_preview_loading {
                    " · Loading…"
                } else {
                    ""
                };
                return format!("Folder · {folders} folders · {files} files{progress}");
            }
            let size = format_size(entry.size);
            if size.is_empty() {
                entry.display_kind().to_string()
            } else {
                format!("{} · {size}", entry.display_kind())
            }
        });
        let image_mime = match &self.preview_state {
            PreviewState::Ready(Preview::Image { mime, .. }) => Some(mime.clone()),
            _ => None,
        };
        let truncated = matches!(
            &self.preview_state,
            PreviewState::Ready(Preview::Text {
                truncated: true,
                ..
            })
        );
        let clipped_lines = matches!(
            &self.preview_state,
            PreviewState::Ready(Preview::Text {
                clipped_lines: true,
                ..
            })
        );
        let has_preview_footer = preview_name.is_some()
            || preview_details.is_some()
            || image_mime.is_some()
            || truncated
            || clipped_lines;
        let preview_footer = div()
            .flex()
            .flex_col()
            .gap_1()
            .px_4()
            .py_3()
            .when_some(preview_name, |this, name| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(colors.foreground)
                        .whitespace_normal()
                        .child(name),
                )
            })
            .when_some(preview_details, |this, details| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(colors.muted_foreground)
                        .child(details),
                )
            })
            .when_some(image_mime, |this, mime| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(colors.muted_foreground)
                        .child(mime),
                )
            })
            .when(truncated, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(colors.warning)
                        .child("Preview limited to the first 256 KiB"),
                )
            })
            .when(clipped_lines, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(colors.warning)
                        .child("Very long lines are shortened in the preview"),
                )
            });

        let preview = div()
            .relative()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .bg(colors.sidebar)
            .child(
                canvas(
                    {
                        let preview_width = self.preview_width.clone();
                        let preview_view = cx.entity();
                        move |bounds, _, cx| {
                            if (preview_width.get() - bounds.size.width).abs() >= px(1.0) {
                                preview_width.set(bounds.size.width);
                                preview_view.update(cx, |this, cx| {
                                    this.schedule_preview_wrap(cx);
                                });
                            }
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(self.render_preview(window, cx)),
            )
            .when(has_preview_footer, |this| this.child(preview_footer));

        let pane_view = cx.entity();
        let entry_menu = self.render_entry_menu(window, cx);
        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(colors.background)
            .text_color(colors.foreground)
            .on_key_down(cx.listener(Self::on_window_key_down))
            .child(topbar)
            .child(
                h_flex().flex_1().min_h_0().w_full().child(sidebar).child(
                    div().flex_1().min_w_0().h_full().child(
                        h_resizable("workspace-panes")
                            .on_resize(move |_, _, cx| {
                                pane_view.update(cx, |this, cx| {
                                    this.start_preview_wrap(cx);
                                    cx.notify();
                                });
                            })
                            .child(
                                resizable_panel()
                                    .size(browser_default)
                                    .size_range(px(MIN_BROWSER_WIDTH)..Pixels::MAX)
                                    .child(browser),
                            )
                            .child(
                                resizable_panel()
                                    .size(preview_default)
                                    .size_range(px(MIN_PREVIEW_WIDTH)..px(MAX_PREVIEW_WIDTH))
                                    .child(preview),
                            ),
                    ),
                ),
            )
            .when_some(entry_menu, |this, menu| this.child(menu))
    }
}

fn normalize_start_directory(path: PathBuf) -> PathBuf {
    if path.is_dir() {
        path
    } else {
        path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/"))
    }
}

fn grid_column_count(viewport_width: f32) -> usize {
    let available = (viewport_width - GRID_SIDE_PADDING * 2.0).max(GRID_TILE_WIDTH);
    ((available + GRID_GAP) / (GRID_TILE_WIDTH + GRID_GAP))
        .floor()
        .max(1.0) as usize
}

fn is_hidden_name(name: &str) -> bool {
    name.starts_with('.') && name != "." && name != ".."
}

fn elide_filename(name: &str, max_columns: usize) -> String {
    if UnicodeWidthStr::width(name) <= max_columns {
        return name.to_string();
    }
    if max_columns == 0 {
        return String::new();
    }

    const ELLIPSIS: &str = "…";
    let extension = name
        .rfind('.')
        .filter(|dot| *dot > 0)
        .map(|dot| (&name[..dot], &name[dot..]));

    if let Some((stem, suffix)) = extension {
        let suffix_width = UnicodeWidthStr::width(suffix);
        if suffix_width + 2 < max_columns {
            let stem_columns = max_columns - suffix_width - 1;
            return format!(
                "{}{ELLIPSIS}{suffix}",
                take_display_columns(stem, stem_columns)
            );
        }
    }

    format!(
        "{}{ELLIPSIS}",
        take_display_columns(name, max_columns.saturating_sub(1))
    )
}

fn take_display_columns(value: &str, max_columns: usize) -> &str {
    let mut width = 0;
    let mut end = 0;

    for (index, character) in value.char_indices() {
        let character_width = character.width().unwrap_or(0);
        if width + character_width > max_columns {
            break;
        }
        width += character_width;
        end = index + character.len_utf8();
    }

    &value[..end]
}

fn fuzzy_score(candidate: &str, query: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }

    let candidate = candidate.to_lowercase().chars().collect::<Vec<_>>();
    let query = query.to_lowercase().chars().collect::<Vec<_>>();
    let mut search_from = 0;
    let mut previous_match = None;
    let mut score = 0i64;

    for needle in &query {
        let relative = candidate
            .get(search_from..)?
            .iter()
            .position(|character| character == needle)?;
        let position = search_from + relative;

        score += if previous_match.is_some_and(|previous| position == previous + 1) {
            24
        } else {
            4
        };
        if position == 0
            || candidate
                .get(position.saturating_sub(1))
                .is_some_and(|character| matches!(character, ' ' | '-' | '_' | '.'))
        {
            score += 16;
        }
        score -= position as i64;

        previous_match = Some(position);
        search_from = position + 1;
    }

    score -= candidate.len().saturating_sub(query.len()) as i64;
    Some(score)
}

fn prioritize_thumbnail_indices(visible: Range<usize>, nearby: Range<usize>) -> Vec<usize> {
    visible
        .clone()
        .chain(nearby.filter(|index| !visible.contains(index)))
        .collect()
}

fn wrap_preview_lines(lines: &[String], columns: usize) -> Arc<[WrappedPreviewLine]> {
    let columns = columns.max(1);
    let mut wrapped = Vec::with_capacity(lines.len());

    for (index, line) in lines.iter().enumerate() {
        if line.is_empty() {
            wrapped.push(WrappedPreviewLine {
                source_line: Some(index + 1),
                contents: String::new(),
            });
            continue;
        }

        let mut remaining = line.as_str();
        let mut first = true;
        while !remaining.is_empty() {
            let split = preview_wrap_break(remaining, columns);
            wrapped.push(WrappedPreviewLine {
                source_line: first.then_some(index + 1),
                contents: remaining[..split].to_string(),
            });
            remaining = &remaining[split..];
            first = false;
        }
    }

    wrapped.into()
}

fn preview_wrap_break(line: &str, columns: usize) -> usize {
    let mut width = 0;
    let mut last_preferred_break = None;

    for (byte_index, character) in line.char_indices() {
        let character_width = if character == '\t' {
            4
        } else {
            character.width().unwrap_or(0)
        };
        if width + character_width > columns {
            return last_preferred_break.unwrap_or_else(|| {
                if byte_index == 0 {
                    character.len_utf8()
                } else {
                    byte_index
                }
            });
        }

        width += character_width;
        if character.is_whitespace() && width >= columns / 2 {
            last_preferred_break = Some(byte_index + character.len_utf8());
        }
    }

    line.len()
}

fn prioritize_pdf_pages(visible: Range<usize>, pages: usize) -> Vec<usize> {
    let nearby = visible.start.saturating_sub(PDF_PAGE_LOOKAHEAD)
        ..(visible.end + PDF_PAGE_LOOKAHEAD).min(pages);
    let mut seen = HashSet::new();
    visible
        .chain(nearby)
        .filter(|index| *index < pages)
        .map(|index| index + 1)
        .filter(|page| seen.insert(*page))
        .collect()
}

fn selection_target(
    current: Option<usize>,
    item_count: usize,
    columns: usize,
    page_size: usize,
    view_mode: ViewMode,
    motion: SelectionMotion,
) -> Option<usize> {
    if item_count == 0 {
        return None;
    }
    let last = item_count - 1;
    let current = match current {
        Some(current) if current < item_count => current,
        _ => {
            return Some(match motion {
                SelectionMotion::Up
                | SelectionMotion::Left
                | SelectionMotion::Last
                | SelectionMotion::PageUp => last,
                _ => 0,
            });
        }
    };
    let columns = columns.max(1);
    let page_size = page_size.max(1);

    Some(match motion {
        SelectionMotion::Up => match view_mode {
            ViewMode::List => current.saturating_sub(1),
            ViewMode::Grid => current.saturating_sub(columns),
        },
        SelectionMotion::Down => match view_mode {
            ViewMode::List => (current + 1).min(last),
            ViewMode::Grid => (current + columns).min(last),
        },
        SelectionMotion::Left => match view_mode {
            ViewMode::List => return None,
            ViewMode::Grid => current.saturating_sub(1),
        },
        SelectionMotion::Right => match view_mode {
            ViewMode::List => return None,
            ViewMode::Grid => (current + 1).min(last),
        },
        SelectionMotion::First => 0,
        SelectionMotion::Last => last,
        SelectionMotion::PageUp => current.saturating_sub(page_size),
        SelectionMotion::PageDown => (current + page_size).min(last),
    })
}

fn marquee_bounds(a: Point<Pixels>, b: Point<Pixels>) -> Bounds<Pixels> {
    Bounds::from_corners(
        Point::new(a.x.min(b.x), a.y.min(b.y)),
        Point::new(a.x.max(b.x), a.y.max(b.y)),
    )
}

fn edge_scroll_delta(pointer_y: Pixels, viewport: Bounds<Pixels>) -> Pixels {
    let zone = px(MARQUEE_EDGE_ZONE);
    if pointer_y < viewport.top() + zone {
        let proximity =
            (f32::from(viewport.top() + zone - pointer_y) / MARQUEE_EDGE_ZONE).clamp(0.0, 1.0);
        px(MARQUEE_MAX_SCROLL_STEP * proximity)
    } else if pointer_y > viewport.bottom() - zone {
        let proximity =
            (f32::from(pointer_y - (viewport.bottom() - zone)) / MARQUEE_EDGE_ZONE).clamp(0.0, 1.0);
        px(-MARQUEE_MAX_SCROLL_STEP * proximity)
    } else {
        px(0.0)
    }
}

fn centered_preview_message(message: impl Into<String>, color: Hsla) -> AnyElement {
    div()
        .flex()
        .size_full()
        .items_center()
        .justify_center()
        .text_color(color)
        .child(message.into())
        .into_any_element()
}

fn code_fence(contents: &str, language: &str) -> String {
    let mut fence = "```".to_string();
    while contents.contains(&fence) {
        fence.push('`');
    }
    format!("{fence}{language}\n{contents}\n{fence}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_fence_grows_past_fences_in_content() {
        let rendered = code_fence("contains ``` here", "text");
        assert!(rendered.starts_with("````text\n"));
        assert!(rendered.ends_with("\n````"));
    }

    #[test]
    fn start_path_falls_back_to_parent_for_files() {
        let path = PathBuf::from("/tmp/file.txt");
        assert_eq!(normalize_start_directory(path), PathBuf::from("/tmp"));
    }

    #[test]
    fn marquee_bounds_normalizes_every_drag_direction() {
        let bounds = marquee_bounds(
            Point::new(px(20.0), px(40.0)),
            Point::new(px(5.0), px(10.0)),
        );

        assert_eq!(bounds.left(), px(5.0));
        assert_eq!(bounds.top(), px(10.0));
        assert_eq!(bounds.right(), px(20.0));
        assert_eq!(bounds.bottom(), px(40.0));
    }

    #[test]
    fn marquee_edge_scroll_accelerates_toward_viewport_edges() {
        let viewport = Bounds::from_corners(
            Point::new(px(0.0), px(100.0)),
            Point::new(px(500.0), px(500.0)),
        );

        assert_eq!(edge_scroll_delta(px(300.0), viewport), px(0.0));
        assert!(edge_scroll_delta(px(110.0), viewport) > px(0.0));
        assert!(edge_scroll_delta(px(490.0), viewport) < px(0.0));
        assert!(edge_scroll_delta(px(101.0), viewport) > edge_scroll_delta(px(125.0), viewport));
    }

    #[test]
    fn grid_columns_reserve_both_side_gutters() {
        assert_eq!(grid_column_count(120.0), 1);
        assert_eq!(grid_column_count(279.0), 1);
        assert_eq!(grid_column_count(280.0), 2);
        assert_eq!(grid_column_count(640.0), 4);
    }

    #[test]
    fn fuzzy_filter_is_case_insensitive_and_ordered() {
        assert!(fuzzy_score("Cargo.lock", "cgl").is_some());
        assert!(fuzzy_score("Documents", "DOC").is_some());
        assert!(fuzzy_score("Documents", "stm").is_none());
    }

    #[test]
    fn hidden_names_follow_unix_dotfile_conventions() {
        assert!(is_hidden_name(".git"));
        assert!(is_hidden_name(".env.local"));
        assert!(!is_hidden_name("visible.txt"));
        assert!(!is_hidden_name("."));
        assert!(!is_hidden_name(".."));
    }

    #[test]
    fn grid_filename_elision_preserves_the_extension() {
        assert_eq!(
            elide_filename("Apartman Özeti ve Gelir Giderleri.xlsx", 28),
            "Apartman Özeti ve Geli….xlsx"
        );
        assert_eq!(elide_filename("short.pdf", 28), "short.pdf");
    }

    #[test]
    fn grid_filename_elision_respects_unicode_display_width() {
        let elided = elide_filename("kimlik_ön_yüzünün_uzun_adı.jpeg", 18);

        assert!(elided.ends_with(".jpeg"));
        assert!(UnicodeWidthStr::width(elided.as_str()) <= 18);
    }

    #[test]
    fn fuzzy_filter_prefers_contiguous_and_early_matches() {
        assert!(
            fuzzy_score("document-backup", "doc").unwrap()
                > fuzzy_score("downloaded-object-copy", "doc").unwrap()
        );
        assert!(fuzzy_score("photo", "pho").unwrap() > fuzzy_score("my-photo", "pho").unwrap());
    }

    #[test]
    fn thumbnail_priority_puts_visible_items_before_lookahead() {
        assert_eq!(
            prioritize_thumbnail_indices(10..13, 8..15),
            vec![10, 11, 12, 8, 9, 13, 14]
        );
    }

    #[test]
    fn pdf_priority_is_visible_first_with_bounded_lookahead() {
        assert_eq!(prioritize_pdf_pages(3..5, 10), vec![4, 5, 3, 6]);
        assert_eq!(prioritize_pdf_pages(0..1, 1), vec![1]);
    }

    #[test]
    fn list_keyboard_navigation_clamps_and_pages() {
        assert_eq!(
            selection_target(Some(0), 10, 1, 4, ViewMode::List, SelectionMotion::Up),
            Some(0)
        );
        assert_eq!(
            selection_target(Some(2), 10, 1, 4, ViewMode::List, SelectionMotion::PageDown),
            Some(6)
        );
        assert_eq!(
            selection_target(Some(2), 10, 1, 4, ViewMode::List, SelectionMotion::Left),
            None
        );
    }

    #[test]
    fn grid_keyboard_navigation_uses_columns() {
        assert_eq!(
            selection_target(Some(5), 10, 3, 6, ViewMode::Grid, SelectionMotion::Up),
            Some(2)
        );
        assert_eq!(
            selection_target(Some(5), 10, 3, 6, ViewMode::Grid, SelectionMotion::Down),
            Some(8)
        );
        assert_eq!(
            selection_target(None, 10, 3, 6, ViewMode::Grid, SelectionMotion::Right),
            Some(0)
        );
    }

    #[test]
    fn preview_wrapping_preserves_all_text_and_source_numbers() {
        let source = vec!["alpha beta gamma delta".to_string(), "猫猫猫猫".to_string()];
        let wrapped = wrap_preview_lines(&source, 10);

        let first = wrapped
            .iter()
            .take_while(|line| line.source_line != Some(2))
            .map(|line| line.contents.as_str())
            .collect::<String>();
        let second = wrapped
            .iter()
            .skip_while(|line| line.source_line != Some(2))
            .map(|line| line.contents.as_str())
            .collect::<String>();

        assert_eq!(first, source[0]);
        assert_eq!(second, source[1]);
        assert_eq!(wrapped[0].source_line, Some(1));
        assert_eq!(wrapped[1].source_line, None);
    }
}
