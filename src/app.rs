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
    AnyElement, Bounds, ClickEvent, ClipboardItem, Context, CursorStyle, DragMoveEvent, Entity,
    FocusHandle, Focusable, Hsla, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ObjectFit, ParentElement, Pixels, Point, Render, ScrollStrategy,
    SharedString, Styled, Subscription, Task, TextRun, Timer, UniformListScrollHandle, Window,
    canvas, div, font, img, prelude::*, px, relative, uniform_list,
};
use gpui_component::{
    ActiveTheme, Disableable, Root, Sizable, Theme, WindowExt,
    button::{Button, ButtonVariants},
    dialog::DialogButtonProps,
    h_flex,
    input::{Input, InputEvent, InputState},
    notification::Notification,
    progress::Progress,
    resizable::{h_resizable, resizable_panel},
    scroll::ScrollableElement,
    switch::Switch,
    text::TextView,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    bookmarks::{
        Bookmark, add as add_bookmark, default_path as default_bookmarks_path,
        load as load_bookmarks, remove as remove_bookmark_at, reorder as reorder_bookmark,
        save as save_bookmarks,
    },
    commands::{
        ActivateSelection, BROWSER_KEY_CONTEXT, BrowserCommand, ClearSelection, CopySelection,
        CutSelection, ExtendDown, ExtendLeft, ExtendPageDown, ExtendPageUp, ExtendRight,
        ExtendToFirst, ExtendToLast, ExtendUp, GoBack, GoForward, GoToParent, MoveDown, MoveLeft,
        MoveRight, MoveUp, NewFolder, OpenWithSelection, PasteFiles, RedoFileOperation, SelectAll,
        SelectFirst, SelectLast, SelectPageDown, SelectPageUp, UndoFileOperation,
    },
    file_ops::{
        OperationJournal, TransferMode, TransferProgress, create_directory, redo_operation,
        summarize_failures, transfer_paths_with_progress, undo_operation, validate_entry_name,
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
const POINTER_EDGE_SCROLL_ZONE: f32 = 36.0;
const POINTER_EDGE_MAX_SCROLL_STEP: f32 = 18.0;
const POINTER_EDGE_SCROLL_INTERVAL: Duration = Duration::from_millis(16);
const ENTRY_MENU_WIDTH: f32 = 208.0;
const ENTRY_MENU_HEIGHT: f32 = 430.0;
const DIRECTORY_MENU_HEIGHT: f32 = 342.0;
const ENTRY_MENU_MARGIN: f32 = 8.0;
const BOOKMARK_MENU_WIDTH: f32 = 152.0;
const BOOKMARK_MENU_HEIGHT: f32 = 38.0;
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

#[derive(Clone, Debug)]
struct FileClipboard {
    mode: TransferMode,
    paths: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
struct FileDrag {
    paths: Arc<[PathBuf]>,
    bookmark_candidates: Arc<[PathBuf]>,
}

#[derive(Clone, Debug)]
struct BookmarkDrag {
    index: usize,
    path: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BookmarkInsertion {
    index: usize,
}

#[derive(Clone, Copy)]
struct BookmarkMenu {
    index: usize,
    position: Point<Pixels>,
}

struct DragPreview {
    label: String,
    detail: &'static str,
}

struct ActiveTransferProgress {
    mode: TransferMode,
    source_count: usize,
    destination: PathBuf,
    progress: Arc<TransferProgress>,
}

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors;
        h_flex()
            .max_w(px(280.0))
            .px_3()
            .py_2()
            .gap_2()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(colors.border)
            .bg(colors.popover.opacity(0.94))
            .text_color(colors.popover_foreground)
            .shadow_md()
            .child(
                div()
                    .text_xs()
                    .text_color(colors.primary)
                    .child(self.detail),
            )
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(self.label.clone()),
            )
    }
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
    operation_journal: OperationJournal,
    file_clipboard: Option<FileClipboard>,
    operation_busy: bool,
    operation_cancel: Option<Arc<AtomicBool>>,
    operation_task: Option<Task<()>>,
    operation_progress: Option<ActiveTransferProgress>,
    operation_progress_task: Option<Task<()>>,
    select_after_directory_load: Option<PathBuf>,
    selection: SelectionModel,
    entry_menu: Option<EntryMenu>,
    marquee: Option<MarqueeGesture>,
    marquee_scroll_task: Option<Task<()>>,
    file_drag_pointer: Option<Point<Pixels>>,
    file_drag_scroll_task: Option<Task<()>>,
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
    bookmarks: Vec<Bookmark>,
    bookmark_icons: HashMap<PathBuf, PathBuf>,
    bookmarks_path: PathBuf,
    bookmarks_loading: bool,
    bookmarks_load_task: Option<Task<()>>,
    bookmarks_save_task: Option<Task<()>>,
    bookmark_insertion: Option<BookmarkInsertion>,
    bookmark_menu: Option<BookmarkMenu>,
    bookmark_region_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    bookmark_row_bounds: Rc<RefCell<HashMap<usize, Bounds<Pixels>>>>,
    place_drop_bounds: Rc<RefCell<HashMap<PathBuf, Bounds<Pixels>>>>,
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
        let use_iosevka_ui = iosevka_ui_font.is_some();
        if let Some(font) = &iosevka_ui_font {
            Theme::global_mut(cx).font_family = font.clone();
        }
        let mono_font_size = cx.theme().mono_font_size;

        let mut this = Self {
            browser_focus: cx.focus_handle(),
            system_ui_font,
            use_iosevka_ui,
            iosevka_ui_font,
            current_dir: start_dir.clone(),
            entries: Vec::new(),
            visible_entries: Vec::new(),
            filter_query: String::new(),
            show_hidden: true,
            search_input,
            _search_subscriptions: vec![search_subscription],
            operation_journal: OperationJournal::default(),
            file_clipboard: None,
            operation_busy: false,
            operation_cancel: None,
            operation_task: None,
            operation_progress: None,
            operation_progress_task: None,
            select_after_directory_load: None,
            selection: SelectionModel::default(),
            entry_menu: None,
            marquee: None,
            marquee_scroll_task: None,
            file_drag_pointer: None,
            file_drag_scroll_task: None,
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
            bookmarks: Vec::new(),
            bookmark_icons: HashMap::new(),
            bookmarks_path: default_bookmarks_path(&home_dir),
            bookmarks_loading: true,
            bookmarks_load_task: None,
            bookmarks_save_task: None,
            bookmark_insertion: None,
            bookmark_menu: None,
            bookmark_region_bounds: Rc::new(Cell::new(None)),
            bookmark_row_bounds: Rc::new(RefCell::new(HashMap::new())),
            place_drop_bounds: Rc::new(RefCell::new(HashMap::new())),
        };
        this.start_places_load(home_dir, cx);
        this.start_bookmarks_load(cx);
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

    fn start_bookmarks_load(&mut self, cx: &mut Context<Self>) {
        let path = self.bookmarks_path.clone();
        let load_task = cx.background_executor().spawn(smol::unblock(move || {
            let bookmarks = load_bookmarks(&path)?;
            let mut icon_provider = crate::icons::IconProvider::discover();
            let icons = bookmarks
                .iter()
                .filter_map(|bookmark| {
                    icon_provider
                        .icon_for(&bookmark.path, true)
                        .map(|icon| (bookmark.path.clone(), icon))
                })
                .collect();
            anyhow::Ok((bookmarks, icons))
        }));

        self.bookmarks_load_task = Some(cx.spawn(async move |this, cx| {
            let result = load_task.await;
            let _ = this.update(cx, |this, cx| {
                this.bookmarks_loading = false;
                match result {
                    Ok((bookmarks, icons)) => {
                        this.bookmarks = bookmarks;
                        this.bookmark_icons = icons;
                    }
                    Err(error) => {
                        eprintln!("Could not load Marcel bookmarks: {error:#}");
                    }
                }
                cx.notify();
            });
        }));
    }

    fn start_bookmarks_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.bookmarks_save_task.is_some() {
            return;
        }
        let path = self.bookmarks_path.clone();
        let snapshot = self.bookmarks.clone();
        let saved_snapshot = snapshot.clone();
        let save_task = cx
            .background_executor()
            .spawn(smol::unblock(move || save_bookmarks(&path, &snapshot)));

        self.bookmarks_save_task = Some(cx.spawn_in(window, async move |this, window| {
            let result = save_task.await;
            let _ = this.update_in(window, |this, window, cx| {
                this.bookmarks_save_task = None;
                if let Err(error) = result {
                    window.push_notification(
                        Notification::error(format!("Could not save bookmarks: {error}")),
                        cx,
                    );
                } else if this.bookmarks != saved_snapshot {
                    this.start_bookmarks_save(window, cx);
                }
                cx.notify();
            });
        }));
    }

    fn add_dragged_bookmarks(
        &mut self,
        paths: &[PathBuf],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut added = 0;
        for path in paths {
            if add_bookmark(&mut self.bookmarks, path.clone()) {
                if let Some(icon) = self
                    .entries
                    .iter()
                    .find(|entry| &entry.path == path)
                    .and_then(|entry| entry.icon_path.clone())
                {
                    self.bookmark_icons.insert(path.clone(), icon);
                }
                added += 1;
            }
        }
        self.bookmark_insertion = None;
        if added == 0 {
            window.push_notification(
                Notification::info("Those folders are already bookmarked"),
                cx,
            );
            return;
        }
        self.start_bookmarks_save(window, cx);
        window.push_notification(
            Notification::success(format!("Added {added} bookmark(s)")),
            cx,
        );
        cx.notify();
    }

    fn move_bookmark(
        &mut self,
        from: usize,
        insertion: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.bookmark_insertion = None;
        if reorder_bookmark(&mut self.bookmarks, from, insertion) {
            self.start_bookmarks_save(window, cx);
            cx.notify();
        }
    }

    fn remove_bookmark(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.bookmark_menu = None;
        let Some(bookmark) = remove_bookmark_at(&mut self.bookmarks, index) else {
            return;
        };
        self.bookmark_icons.remove(&bookmark.path);
        self.start_bookmarks_save(window, cx);
        window.push_notification(
            Notification::success(format!("Removed bookmark “{}”", bookmark.label())),
            cx,
        );
        cx.notify();
    }

    fn render_bookmark_menu(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let menu = self.bookmark_menu?;
        self.bookmarks.get(menu.index)?;
        let colors = cx.theme().colors;
        let radius = cx.theme().radius;
        let window_size = window.bounds().size;
        let left = f32::from(menu.position.x)
            .min((f32::from(window_size.width) - BOOKMARK_MENU_WIDTH - ENTRY_MENU_MARGIN).max(0.0))
            .max(ENTRY_MENU_MARGIN);
        let top = f32::from(menu.position.y)
            .min(
                (f32::from(window_size.height) - BOOKMARK_MENU_HEIGHT - ENTRY_MENU_MARGIN).max(0.0),
            )
            .max(ENTRY_MENU_MARGIN);
        let index = menu.index;

        Some(
            div()
                .id("bookmark-context-menu")
                .absolute()
                .left(px(left))
                .top(px(top))
                .w(px(BOOKMARK_MENU_WIDTH))
                .p_1()
                .rounded(radius)
                .border_1()
                .border_color(colors.border)
                .bg(colors.popover)
                .text_color(colors.popover_foreground)
                .occlude()
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.bookmark_menu = None;
                    cx.notify();
                }))
                .child(
                    h_flex()
                        .id("bookmark-menu-remove")
                        .h(px(28.0))
                        .px_3()
                        .rounded(radius)
                        .cursor_pointer()
                        .hover(|this| this.bg(colors.accent))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.remove_bookmark(index, window, cx);
                        }))
                        .child("Remove Bookmark"),
                )
                .into_any_element(),
        )
    }

    fn render_operation_progress(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let active = self.operation_progress.as_ref()?;
        let snapshot = active.progress.snapshot();
        let cancelling = self
            .operation_cancel
            .as_ref()
            .is_some_and(|cancel| cancel.load(Ordering::Acquire));
        let colors = cx.theme().colors;
        let title = if cancelling {
            "Cancelling…"
        } else {
            match active.mode {
                TransferMode::Copy => "Copying",
                TransferMode::Move => "Moving",
            }
        };
        let percentage = if snapshot.total_bytes > 0 {
            snapshot.completed_bytes as f32 / snapshot.total_bytes as f32 * 100.0
        } else if snapshot.total_items > 0 {
            snapshot.completed_items as f32 / snapshot.total_items as f32 * 100.0
        } else {
            0.0
        };
        let progress_text = if snapshot.preparing {
            format!(
                "Preparing {} item(s)",
                snapshot.total_items.max(active.source_count as u64)
            )
        } else if snapshot.total_bytes > 0 {
            format!(
                "{} of {} items · {} of {}",
                snapshot.completed_items,
                snapshot.total_items,
                format_size(Some(snapshot.completed_bytes)),
                format_size(Some(snapshot.total_bytes))
            )
        } else {
            format!(
                "{} of {} items",
                snapshot.completed_items, snapshot.total_items
            )
        };
        let current_name = snapshot.current_path.as_ref().map(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string())
        });
        let destination = format!("to {}", active.destination.display());
        let view = cx.entity();
        let cancel_button = Button::new("cancel-active-transfer")
            .small()
            .danger()
            .label(if cancelling { "Cancelling" } else { "Cancel" })
            .disabled(cancelling)
            .on_click(move |_, _, cx| {
                view.update(cx, |this, cx| this.cancel_active_operation(cx));
            });

        Some(
            div()
                .w_80()
                .max_w_full()
                .p_3()
                .flex()
                .flex_col()
                .gap_2()
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(colors.border)
                .bg(colors.popover)
                .text_color(colors.popover_foreground)
                .occlude()
                .child(
                    h_flex()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(div().text_sm().child(title))
                                .child(
                                    div()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .text_xs()
                                        .text_color(colors.muted_foreground)
                                        .child(destination),
                                ),
                        )
                        .child(cancel_button),
                )
                .child(Progress::new().h_1().value(percentage))
                .child(
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .text_color(colors.muted_foreground)
                        .child(div().flex_none().child(progress_text))
                        .when_some(current_name, |this, name| {
                            this.child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .text_right()
                                    .child(name),
                            )
                        }),
                )
                .into_any_element(),
        )
    }

    fn selected_file_drag(&self) -> Option<FileDrag> {
        let selected = self.selection.selected();
        if selected.is_empty() {
            return None;
        }
        let mut paths = Vec::with_capacity(selected.len());
        let mut bookmark_candidates = Vec::new();
        for index in &self.visible_entries {
            let Some(entry) = self.entries.get(*index) else {
                continue;
            };
            if selected.contains(&entry.path) {
                paths.push(entry.path.clone());
                if entry.navigable {
                    bookmark_candidates.push(entry.path.clone());
                }
            }
        }
        Some(FileDrag {
            paths: paths.into(),
            bookmark_candidates: bookmark_candidates.into(),
        })
    }

    fn single_file_drag(path: &Path, navigable: bool) -> FileDrag {
        let bookmark_candidates = if navigable {
            vec![path.to_path_buf()]
        } else {
            Vec::new()
        };
        FileDrag {
            paths: vec![path.to_path_buf()].into(),
            bookmark_candidates: bookmark_candidates.into(),
        }
    }

    fn set_bookmark_insertion(
        &mut self,
        event: &DragMoveEvent<BookmarkDrag>,
        cx: &mut Context<Self>,
    ) {
        let Some(region) = self.bookmark_region_bounds.get() else {
            return;
        };
        let pointer = event.event.position;
        let index = if !region.contains(&pointer) {
            return;
        } else {
            let rows = self.bookmark_row_bounds.borrow();
            let mut ordered = rows.iter().collect::<Vec<_>>();
            ordered.sort_by_key(|(index, _)| **index);
            ordered
                .iter()
                .find_map(|(index, bounds)| {
                    (pointer.y < bounds.top() + bounds.size.height / 2.0).then_some(**index)
                })
                .unwrap_or(self.bookmarks.len())
        };
        if self.bookmark_insertion != Some(BookmarkInsertion { index }) {
            self.bookmark_insertion = Some(BookmarkInsertion { index });
            cx.notify();
        }
    }

    fn update_file_drag_cursor(
        &mut self,
        event: &DragMoveEvent<FileDrag>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let drag = event.drag(cx).clone();
        let pointer = event.event.position;
        self.file_drag_pointer = Some(pointer);
        if self.file_drag_scroll_task.is_none() {
            self.start_file_drag_autoscroll(cx);
        }
        let can_move_to =
            |path: &Path| !self.operation_busy && can_move_files_to(&drag.paths, path);

        let over_browser_folder = self.entry_hit_bounds.borrow().iter().any(|(path, bounds)| {
            bounds.contains(&pointer)
                && self
                    .entries
                    .iter()
                    .find(|entry| entry.path == *path)
                    .is_some_and(|entry| entry.navigable && can_move_to(path))
        });
        let over_place = self
            .place_drop_bounds
            .borrow()
            .iter()
            .any(|(path, bounds)| bounds.contains(&pointer) && can_move_to(path));
        let over_bookmark = self
            .bookmark_row_bounds
            .borrow()
            .iter()
            .any(|(index, bounds)| {
                bounds.contains(&pointer)
                    && self
                        .bookmarks
                        .get(*index)
                        .is_some_and(|bookmark| can_move_to(&bookmark.path))
            });
        let over_bookmark_region = self
            .bookmark_region_bounds
            .get()
            .is_some_and(|bounds| bounds.contains(&pointer));

        let cursor = if over_browser_folder || over_place || over_bookmark {
            CursorStyle::ClosedHand
        } else if over_bookmark_region && !drag.bookmark_candidates.is_empty() {
            CursorStyle::DragLink
        } else {
            CursorStyle::OperationNotAllowed
        };
        cx.set_active_drag_cursor_style(cursor, window);
    }

    fn start_directory_load(&mut self, clear_filter: bool, cx: &mut Context<Self>) {
        self.entry_menu = None;
        self.bookmark_menu = None;
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
                                this.select_pending_loaded_entry(cx);
                            }
                            DirectoryUpdate::Done => {
                                this.directory_loading = false;
                                this.select_after_directory_load = None;
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

    fn select_pending_loaded_entry(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.select_after_directory_load.as_ref() else {
            return;
        };
        let visible = self.visible_entries.iter().any(|index| {
            self.entries
                .get(*index)
                .is_some_and(|entry| &entry.path == path)
        });
        if !visible {
            return;
        }
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| &entry.path == path)
            .cloned()
        else {
            return;
        };

        self.select_after_directory_load = None;
        self.selection.select_only(entry.path.clone());
        self.start_preview(entry, cx);
    }

    fn navigate_to(&mut self, path: PathBuf, add_to_history: bool, cx: &mut Context<Self>) {
        if path == self.current_dir {
            return;
        }
        if add_to_history {
            self.history.push(&path);
        }
        self.current_dir = path;
        self.select_after_directory_load = None;
        self.start_directory_load(true, cx);
    }

    fn go_back(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.history.go_back() {
            self.current_dir = path;
            self.select_after_directory_load = None;
            self.start_directory_load(true, cx);
        }
    }

    fn go_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.history.go_forward() {
            self.current_dir = path;
            self.select_after_directory_load = None;
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
            BrowserCommand::CopySelection | BrowserCommand::CutSelection => {
                !self.operation_busy && !self.selection.selected().is_empty()
            }
            BrowserCommand::PasteFiles => {
                !self.operation_busy
                    && self.directory_error.is_none()
                    && self
                        .file_clipboard
                        .as_ref()
                        .is_some_and(|clipboard| !clipboard.paths.is_empty())
            }
            BrowserCommand::NewFolder => !self.operation_busy && self.directory_error.is_none(),
            BrowserCommand::UndoFileOperation => {
                !self.operation_busy && self.operation_journal.can_undo()
            }
            BrowserCommand::RedoFileOperation => {
                !self.operation_busy && self.operation_journal.can_redo()
            }
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

    fn execute_browser_command(
        &mut self,
        command: BrowserCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
            BrowserCommand::CopySelection => self.stage_selection(TransferMode::Copy, window, cx),
            BrowserCommand::CutSelection => self.stage_selection(TransferMode::Move, window, cx),
            BrowserCommand::PasteFiles => self.start_paste(window, cx),
            BrowserCommand::NewFolder => self.open_new_folder_dialog(window, cx),
            BrowserCommand::UndoFileOperation => self.start_undo(window, cx),
            BrowserCommand::RedoFileOperation => self.start_redo(window, cx),
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

    fn open_new_folder_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.entry_menu = None;
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Folder name"));
        let input_for_dialog = input.clone();
        let view = cx.entity();

        window.open_dialog(cx, move |dialog, _, _| {
            let input_for_ok = input_for_dialog.clone();
            let view = view.clone();
            dialog
                .title("New Folder")
                .child(Input::new(&input_for_dialog))
                .confirm()
                .button_props(DialogButtonProps::default().ok_text("Create"))
                .on_ok(move |_, window, cx| {
                    let name = input_for_ok.read(cx).value().trim().to_string();
                    if let Err(error) = validate_entry_name(&name) {
                        window.push_notification(Notification::error(error.to_string()), cx);
                        return false;
                    }

                    view.update(cx, |this, cx| {
                        this.start_create_directory(name.clone(), window, cx);
                    });
                    true
                })
        });
        input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn start_create_directory(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.operation_busy {
            return;
        }

        self.operation_busy = true;
        self.operation_cancel = None;
        let parent = self.current_dir.clone();
        let task = cx
            .background_executor()
            .spawn(smol::unblock(move || create_directory(&parent, &name)));

        self.operation_task = Some(cx.spawn_in(window, async move |this, window| {
            let result = task.await;
            let _ = this.update_in(window, |this, window, cx| {
                this.operation_busy = false;
                this.operation_cancel = None;
                match result {
                    Ok(operation) => {
                        let path = operation.path().to_path_buf();
                        this.operation_journal.record(operation);
                        this.refresh_after_operation(&path, true, cx);
                        window.push_notification(
                            Notification::success(format!(
                                "Created folder “{}”",
                                path.file_name()
                                    .map(|name| name.to_string_lossy())
                                    .unwrap_or_default()
                            )),
                            cx,
                        );
                    }
                    Err(error) => {
                        window.push_notification(Notification::error(error.to_string()), cx);
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn start_undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.operation_busy {
            return;
        }
        let Some(operation) = self.operation_journal.begin_undo() else {
            return;
        };

        self.operation_busy = true;
        self.operation_cancel = None;
        let operation_for_task = operation.clone();
        let task = cx
            .background_executor()
            .spawn(smol::unblock(move || undo_operation(&operation_for_task)));

        self.operation_task = Some(cx.spawn_in(window, async move |this, window| {
            let result = task.await;
            let _ = this.update_in(window, |this, window, cx| {
                this.operation_busy = false;
                this.operation_cancel = None;
                match result {
                    Ok(undone) => {
                        let path = operation.path().to_path_buf();
                        this.operation_journal.finish_undo(undone);
                        this.refresh_after_operation(&path, false, cx);
                        window.push_notification(
                            Notification::success(format!(
                                "Undid creation of “{}”",
                                path.file_name()
                                    .map(|name| name.to_string_lossy())
                                    .unwrap_or_default()
                            )),
                            cx,
                        );
                    }
                    Err(error) => {
                        this.operation_journal.cancel_undo(operation);
                        window.push_notification(Notification::error(error.to_string()), cx);
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn start_redo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.operation_busy {
            return;
        }
        let Some(operation) = self.operation_journal.begin_redo() else {
            return;
        };

        self.operation_busy = true;
        self.operation_cancel = None;
        let operation_for_task = operation.clone();
        let task = cx
            .background_executor()
            .spawn(smol::unblock(move || redo_operation(&operation_for_task)));

        self.operation_task = Some(cx.spawn_in(window, async move |this, window| {
            let result = task.await;
            let _ = this.update_in(window, |this, window, cx| {
                this.operation_busy = false;
                this.operation_cancel = None;
                match result {
                    Ok(redone) => {
                        let path = redone.path().to_path_buf();
                        this.operation_journal.finish_redo(redone);
                        this.refresh_after_operation(&path, true, cx);
                        window.push_notification(
                            Notification::success(format!(
                                "Recreated folder “{}”",
                                path.file_name()
                                    .map(|name| name.to_string_lossy())
                                    .unwrap_or_default()
                            )),
                            cx,
                        );
                    }
                    Err(error) => {
                        this.operation_journal.cancel_redo(operation);
                        window.push_notification(Notification::error(error.to_string()), cx);
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn stage_selection(&mut self, mode: TransferMode, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.selection.selected();
        let paths = self
            .visible_paths()
            .into_iter()
            .filter(|path| selected.contains(path))
            .collect::<Vec<_>>();
        if paths.is_empty() {
            return;
        }
        let count = paths.len();
        self.file_clipboard = Some(FileClipboard { mode, paths });
        self.entry_menu = None;
        let verb = match mode {
            TransferMode::Copy => "Copied",
            TransferMode::Move => "Cut",
        };
        window.push_notification(
            Notification::success(format!("{verb} {count} item(s) to the file clipboard")),
            cx,
        );
        cx.notify();
    }

    fn start_paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(clipboard) = self.file_clipboard.clone() else {
            return;
        };
        self.start_transfer(
            clipboard.paths.clone(),
            self.current_dir.clone(),
            clipboard.mode,
            Some(clipboard),
            window,
            cx,
        );
    }

    fn start_drag_move(
        &mut self,
        paths: Vec<PathBuf>,
        destination: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.bookmark_insertion = None;
        self.start_transfer(paths, destination, TransferMode::Move, None, window, cx);
    }

    fn start_operation_progress_refresh(&mut self, cx: &mut Context<Self>) {
        self.operation_progress_task = Some(cx.spawn(async move |this, cx| {
            loop {
                Timer::after(Duration::from_millis(80)).await;
                let keep_running = this
                    .update(cx, |this, cx| {
                        let keep_running = this.operation_progress.is_some();
                        if keep_running {
                            cx.notify();
                        }
                        keep_running
                    })
                    .unwrap_or(false);
                if !keep_running {
                    break;
                }
            }
        }));
    }

    fn cancel_active_operation(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = &self.operation_cancel {
            cancel.store(true, Ordering::Release);
            cx.notify();
        }
    }

    fn start_transfer(
        &mut self,
        sources: Vec<PathBuf>,
        destination: PathBuf,
        mode: TransferMode,
        clipboard: Option<FileClipboard>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.operation_busy || sources.is_empty() {
            return;
        }
        self.entry_menu = None;
        self.operation_busy = true;
        let cancel = Arc::new(AtomicBool::new(false));
        self.operation_cancel = Some(cancel.clone());
        let progress = Arc::new(TransferProgress::default());
        self.operation_progress = Some(ActiveTransferProgress {
            mode,
            source_count: sources.len(),
            destination: destination.clone(),
            progress: progress.clone(),
        });
        self.start_operation_progress_refresh(cx);
        let task = cx.background_executor().spawn(smol::unblock(move || {
            transfer_paths_with_progress(&sources, &destination, mode, cancel, progress)
        }));

        self.operation_task = Some(cx.spawn_in(window, async move |this, window| {
            let outcome = task.await;
            let _ = this.update_in(window, |this, window, cx| {
                this.operation_busy = false;
                this.operation_cancel = None;
                this.operation_progress = None;
                this.operation_progress_task.take();
                if let Some(operation) = outcome.operation {
                    this.operation_journal.record(operation);
                }

                if mode == TransferMode::Move
                    && !outcome.completed.is_empty()
                    && let Some(clipboard) = clipboard
                {
                    let completed_sources = clipboard
                        .paths
                        .iter()
                        .filter(|source| {
                            source.file_name().is_some_and(|name| {
                                outcome
                                    .completed
                                    .iter()
                                    .any(|path| path.file_name() == Some(name))
                            })
                        })
                        .cloned()
                        .collect::<HashSet<_>>();
                    let remaining = clipboard
                        .paths
                        .into_iter()
                        .filter(|path| !completed_sources.contains(path))
                        .collect::<Vec<_>>();
                    this.file_clipboard = (!remaining.is_empty()).then_some(FileClipboard {
                        mode,
                        paths: remaining,
                    });
                }

                if let Some(first) = outcome
                    .completed
                    .iter()
                    .find(|path| path.parent() == Some(this.current_dir.as_path()))
                {
                    this.select_after_directory_load = Some(first.clone());
                }
                this.start_directory_load(false, cx);

                if outcome.failures.is_empty() {
                    let verb = match mode {
                        TransferMode::Copy => "Copied",
                        TransferMode::Move => "Moved",
                    };
                    window.push_notification(
                        Notification::success(format!(
                            "{verb} {} item(s)",
                            outcome.completed.len()
                        )),
                        cx,
                    );
                } else {
                    window.push_notification(
                        Notification::error(summarize_failures(&outcome.failures)),
                        cx,
                    );
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn refresh_after_operation(&mut self, path: &Path, select: bool, cx: &mut Context<Self>) {
        if path.parent() != Some(self.current_dir.as_path()) {
            return;
        }
        self.select_after_directory_load = select.then(|| path.to_path_buf());
        self.start_directory_load(false, cx);
    }

    fn on_move_up(&mut self, _: &MoveUp, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::MoveUp, window, cx);
    }

    fn on_move_down(&mut self, _: &MoveDown, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::MoveDown, window, cx);
    }

    fn on_move_left(&mut self, _: &MoveLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::MoveLeft, window, cx);
    }

    fn on_move_right(&mut self, _: &MoveRight, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::MoveRight, window, cx);
    }

    fn on_extend_up(&mut self, _: &ExtendUp, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::ExtendUp, window, cx);
    }

    fn on_extend_down(&mut self, _: &ExtendDown, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::ExtendDown, window, cx);
    }

    fn on_extend_left(&mut self, _: &ExtendLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::ExtendLeft, window, cx);
    }

    fn on_extend_right(&mut self, _: &ExtendRight, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::ExtendRight, window, cx);
    }

    fn on_extend_to_first(
        &mut self,
        _: &ExtendToFirst,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_browser_command(BrowserCommand::ExtendToFirst, window, cx);
    }

    fn on_extend_to_last(&mut self, _: &ExtendToLast, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::ExtendToLast, window, cx);
    }

    fn on_extend_page_up(&mut self, _: &ExtendPageUp, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::ExtendPageUp, window, cx);
    }

    fn on_extend_page_down(
        &mut self,
        _: &ExtendPageDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_browser_command(BrowserCommand::ExtendPageDown, window, cx);
    }

    fn on_select_first(&mut self, _: &SelectFirst, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::SelectFirst, window, cx);
    }

    fn on_select_last(&mut self, _: &SelectLast, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::SelectLast, window, cx);
    }

    fn on_select_page_up(&mut self, _: &SelectPageUp, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::SelectPageUp, window, cx);
    }

    fn on_select_page_down(
        &mut self,
        _: &SelectPageDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_browser_command(BrowserCommand::SelectPageDown, window, cx);
    }

    fn on_activate_selection(
        &mut self,
        _: &ActivateSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_browser_command(BrowserCommand::ActivateSelection, window, cx);
    }

    fn on_open_with_selection(
        &mut self,
        _: &OpenWithSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_browser_command(BrowserCommand::OpenWithSelection, window, cx);
    }

    fn on_clear_selection(
        &mut self,
        _: &ClearSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.operation_busy
            && let Some(cancel) = &self.operation_cancel
        {
            cancel.store(true, Ordering::Release);
            window.push_notification(Notification::info("Cancelling file operation…"), cx);
            return;
        }
        if self.filter_query.is_empty() {
            self.execute_browser_command(BrowserCommand::ClearSelection, window, cx);
        } else {
            self.clear_filter(window, cx);
        }
    }

    fn on_go_to_parent(&mut self, _: &GoToParent, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::GoToParent, window, cx);
    }

    fn on_go_back(&mut self, _: &GoBack, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::GoBack, window, cx);
    }

    fn on_go_forward(&mut self, _: &GoForward, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::GoForward, window, cx);
    }

    fn on_select_all(&mut self, _: &SelectAll, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::SelectAll, window, cx);
    }

    fn on_copy_selection(
        &mut self,
        _: &CopySelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_browser_command(BrowserCommand::CopySelection, window, cx);
    }

    fn on_cut_selection(&mut self, _: &CutSelection, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::CutSelection, window, cx);
    }

    fn on_paste_files(&mut self, _: &PasteFiles, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::PasteFiles, window, cx);
    }

    fn on_new_folder(&mut self, _: &NewFolder, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::NewFolder, window, cx);
    }

    fn on_undo_file_operation(
        &mut self,
        _: &UndoFileOperation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_browser_command(BrowserCommand::UndoFileOperation, window, cx);
    }

    fn on_redo_file_operation(
        &mut self,
        _: &RedoFileOperation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_browser_command(BrowserCommand::RedoFileOperation, window, cx);
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
        let input_focused = window.has_focused_input(cx);

        // Type-to-filter is global browser chrome, not an input interceptor.
        // Dialog fields and future inline editors must retain every editing
        // key while they own focus.
        if should_defer_global_filter_to_input(search_focused, input_focused) {
            return;
        }

        if stroke.modifiers.control && !stroke.modifiers.alt && stroke.key.eq_ignore_ascii_case("f")
        {
            self.focus_search(window, cx);
            cx.stop_propagation();
            return;
        }

        if search_focused {
            match stroke.key.as_str() {
                "up" => {
                    self.execute_browser_command(BrowserCommand::MoveUp, window, cx);
                    cx.stop_propagation();
                }
                "down" => {
                    self.execute_browser_command(BrowserCommand::MoveDown, window, cx);
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
                    self.execute_browser_command(BrowserCommand::MoveUp, window, cx);
                    cx.stop_propagation();
                    return;
                }
                "down" if !browser_focused => {
                    self.execute_browser_command(BrowserCommand::MoveDown, window, cx);
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
        let new_folder_enabled = self.command_enabled(BrowserCommand::NewFolder);
        let copy_enabled = self.command_enabled(BrowserCommand::CopySelection);
        let cut_enabled = self.command_enabled(BrowserCommand::CutSelection);
        let paste_enabled = self.command_enabled(BrowserCommand::PasteFiles);
        let undo_enabled = self.command_enabled(BrowserCommand::UndoFileOperation);
        let redo_enabled = self.command_enabled(BrowserCommand::RedoFileOperation);
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
                    .child(
                        h_flex()
                            .id("directory-menu-new-folder")
                            .h(px(28.0))
                            .px_3()
                            .rounded(radius)
                            .when(new_folder_enabled, |this| {
                                this.cursor_pointer()
                                    .hover(|this| this.bg(colors.accent))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.entry_menu = None;
                                        this.execute_browser_command(
                                            BrowserCommand::NewFolder,
                                            window,
                                            cx,
                                        );
                                    }))
                            })
                            .when(!new_folder_enabled, |this| {
                                this.text_color(colors.muted_foreground)
                            })
                            .child("New Folder")
                            .child(div().flex_1())
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(colors.muted_foreground)
                                    .child("Ctrl+Shift+N"),
                            ),
                    )
                    .child(planned("– New File"))
                    .child(
                        h_flex()
                            .id("directory-menu-paste")
                            .h(px(28.0))
                            .px_3()
                            .rounded(radius)
                            .when(paste_enabled, |this| {
                                this.cursor_pointer()
                                    .hover(|this| this.bg(colors.accent))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.entry_menu = None;
                                        this.execute_browser_command(
                                            BrowserCommand::PasteFiles,
                                            window,
                                            cx,
                                        );
                                    }))
                            })
                            .when(!paste_enabled, |this| {
                                this.text_color(colors.muted_foreground)
                            })
                            .child(if paste_enabled { "Paste" } else { "– Paste" })
                            .child(div().flex_1())
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(colors.muted_foreground)
                                    .child("Ctrl+V"),
                            ),
                    )
                    .child(
                        h_flex()
                            .id("directory-menu-undo")
                            .h(px(28.0))
                            .px_3()
                            .rounded(radius)
                            .when(undo_enabled, |this| {
                                this.cursor_pointer()
                                    .hover(|this| this.bg(colors.accent))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.entry_menu = None;
                                        this.execute_browser_command(
                                            BrowserCommand::UndoFileOperation,
                                            window,
                                            cx,
                                        );
                                    }))
                            })
                            .when(!undo_enabled, |this| {
                                this.text_color(colors.muted_foreground)
                            })
                            .child(if undo_enabled { "Undo" } else { "– Undo" })
                            .child(div().flex_1())
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(colors.muted_foreground)
                                    .child("Ctrl+Z"),
                            ),
                    )
                    .child(
                        h_flex()
                            .id("directory-menu-redo")
                            .h(px(28.0))
                            .px_3()
                            .rounded(radius)
                            .when(redo_enabled, |this| {
                                this.cursor_pointer()
                                    .hover(|this| this.bg(colors.accent))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.entry_menu = None;
                                        this.execute_browser_command(
                                            BrowserCommand::RedoFileOperation,
                                            window,
                                            cx,
                                        );
                                    }))
                            })
                            .when(!redo_enabled, |this| {
                                this.text_color(colors.muted_foreground)
                            })
                            .child(if redo_enabled { "Redo" } else { "– Redo" })
                            .child(div().flex_1())
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(colors.muted_foreground)
                                    .child("Ctrl+Y"),
                            ),
                    )
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
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.entry_menu = None;
                                        this.execute_browser_command(
                                            BrowserCommand::SelectAll,
                                            window,
                                            cx,
                                        );
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
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.entry_menu = None;
                            this.execute_browser_command(
                                BrowserCommand::ActivateSelection,
                                window,
                                cx,
                            );
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
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.entry_menu = None;
                                    this.execute_browser_command(
                                        BrowserCommand::OpenWithSelection,
                                        window,
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
                .child(
                    h_flex()
                        .id("entry-menu-cut")
                        .h(px(28.0))
                        .px_3()
                        .rounded(radius)
                        .when(cut_enabled, |this| {
                            this.cursor_pointer()
                                .hover(|this| this.bg(colors.accent))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.execute_browser_command(
                                        BrowserCommand::CutSelection,
                                        window,
                                        cx,
                                    );
                                }))
                        })
                        .when(!cut_enabled, |this| {
                            this.text_color(colors.muted_foreground)
                        })
                        .child(if cut_enabled { "Cut" } else { "– Cut" })
                        .child(div().flex_1())
                        .child(
                            div()
                                .text_xs()
                                .text_color(colors.muted_foreground)
                                .child("Ctrl+X"),
                        ),
                )
                .child(
                    h_flex()
                        .id("entry-menu-copy")
                        .h(px(28.0))
                        .px_3()
                        .rounded(radius)
                        .when(copy_enabled, |this| {
                            this.cursor_pointer()
                                .hover(|this| this.bg(colors.accent))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.execute_browser_command(
                                        BrowserCommand::CopySelection,
                                        window,
                                        cx,
                                    );
                                }))
                        })
                        .when(!copy_enabled, |this| {
                            this.text_color(colors.muted_foreground)
                        })
                        .child(if copy_enabled { "Copy" } else { "– Copy" })
                        .child(div().flex_1())
                        .child(
                            div()
                                .text_xs()
                                .text_color(colors.muted_foreground)
                                .child("Ctrl+C"),
                        ),
                )
                .child(
                    h_flex()
                        .id("entry-menu-paste")
                        .h(px(28.0))
                        .px_3()
                        .rounded(radius)
                        .when(paste_enabled, |this| {
                            this.cursor_pointer()
                                .hover(|this| this.bg(colors.accent))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.entry_menu = None;
                                    this.execute_browser_command(
                                        BrowserCommand::PasteFiles,
                                        window,
                                        cx,
                                    );
                                }))
                        })
                        .when(!paste_enabled, |this| {
                            this.text_color(colors.muted_foreground)
                        })
                        .child(if paste_enabled { "Paste" } else { "– Paste" })
                        .child(div().flex_1())
                        .child(
                            div()
                                .text_xs()
                                .text_color(colors.muted_foreground)
                                .child("Ctrl+V"),
                        ),
                )
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
                Timer::after(POINTER_EDGE_SCROLL_INTERVAL).await;
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

    fn start_file_drag_autoscroll(&mut self, cx: &mut Context<Self>) {
        self.file_drag_scroll_task.take();
        self.file_drag_scroll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                Timer::after(POINTER_EDGE_SCROLL_INTERVAL).await;
                let keep_running = this
                    .update(cx, |this, cx| this.tick_file_drag_autoscroll(cx))
                    .unwrap_or(false);
                if !keep_running {
                    break;
                }
            }
        }));
    }

    fn tick_file_drag_autoscroll(&mut self, cx: &mut Context<Self>) -> bool {
        if !cx.has_active_drag() {
            self.file_drag_pointer = None;
            return false;
        }
        let Some(bounds) = self.browser_bounds.get() else {
            return false;
        };
        let Some(pointer) = self.file_drag_pointer else {
            return false;
        };
        if pointer.x < bounds.left() || pointer.x > bounds.right() {
            return true;
        }

        let delta = edge_scroll_delta(pointer.y, bounds);
        if delta == px(0.0) {
            return true;
        }

        let handle = self.directory_scroll.0.borrow().base_handle.clone();
        let mut offset = handle.offset();
        let old_offset = offset;
        offset.y = (offset.y + delta).clamp(-handle.max_offset().height, px(0.0));
        if offset != old_offset {
            handle.set_offset(offset);
        }
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
        let operation_busy = self.operation_busy;
        let drop_path = place.path.clone();
        let can_drop_path = place.path.clone();
        let bounds_path = place.path.clone();
        let place_drop_bounds = self.place_drop_bounds.clone();
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
            .relative()
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
            .can_drop(move |value, _, _| {
                !operation_busy
                    && value
                        .downcast_ref::<FileDrag>()
                        .is_some_and(|drag| can_move_files_to(&drag.paths, &can_drop_path))
            })
            .drag_over::<FileDrag>(move |style, _, _, _| {
                style
                    .bg(colors.sidebar_accent)
                    .border_1()
                    .border_color(colors.primary)
            })
            .on_drop(cx.listener(move |this, drag: &FileDrag, window, cx| {
                this.start_drag_move(drag.paths.to_vec(), drop_path.clone(), window, cx);
            }))
            .child(icon)
            .child(div().flex_none().text_sm().child(place.label))
            .child(
                canvas(
                    move |bounds, _, _| {
                        place_drop_bounds
                            .borrow_mut()
                            .insert(bounds_path.clone(), bounds);
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.navigate_to(place.path.clone(), true, cx);
            }))
            .into_any_element()
    }

    fn render_bookmark(
        &self,
        index: usize,
        bookmark: Bookmark,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors;
        let radius = cx.theme().radius;
        let active = self.current_dir == bookmark.path;
        let operation_busy = self.operation_busy;
        let insertion_here = self.bookmark_insertion == Some(BookmarkInsertion { index });
        let navigate_path = bookmark.path.clone();
        let drop_path = bookmark.path.clone();
        let can_drop_path = bookmark.path.clone();
        let drag = BookmarkDrag {
            index,
            path: bookmark.path.clone(),
        };
        let bookmark_row_bounds = self.bookmark_row_bounds.clone();
        let icon = self
            .bookmark_icons
            .get(&bookmark.path)
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

        div()
            .flex()
            .flex_col()
            .w_full()
            .child(
                div()
                    .h(px(2.0))
                    .mx_2()
                    .rounded_full()
                    .bg(if insertion_here {
                        colors.primary
                    } else {
                        colors.primary.opacity(0.0)
                    }),
            )
            .child(
                h_flex()
                    .id(("bookmark", index))
                    .relative()
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
                    .can_drop(move |value, _, _| {
                        !operation_busy
                            && value
                                .downcast_ref::<FileDrag>()
                                .is_some_and(|drag| can_move_files_to(&drag.paths, &can_drop_path))
                    })
                    .drag_over::<FileDrag>(move |style, _, _, _| {
                        style
                            .bg(colors.sidebar_accent)
                            .border_1()
                            .border_color(colors.primary)
                    })
                    .on_drop(cx.listener(move |this, drag: &FileDrag, window, cx| {
                        this.start_drag_move(drag.paths.to_vec(), drop_path.clone(), window, cx);
                    }))
                    .on_drag(drag, |drag, _, _, cx| {
                        let label = drag
                            .path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| drag.path.display().to_string());
                        cx.new(|_| DragPreview {
                            label,
                            detail: "Bookmark",
                        })
                    })
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            this.entry_menu = None;
                            this.bookmark_menu = Some(BookmarkMenu {
                                index,
                                position: event.position,
                            });
                            cx.notify();
                        }),
                    )
                    .child(icon)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_sm()
                            .child(bookmark.label()),
                    )
                    .child(
                        canvas(
                            move |bounds, _, _| {
                                bookmark_row_bounds.borrow_mut().insert(index, bounds);
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    )
                    .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                        if !event.is_right_click() {
                            this.navigate_to(navigate_path.clone(), true, cx);
                        }
                    })),
            )
            .into_any_element()
    }

    fn render_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let radius = cx.theme().radius;
        // A marquee originates on empty browser space, so no entry drag can
        // begin until that gesture ends. Avoid rebuilding a potentially huge
        // selected-file payload on every marquee repaint.
        let selected_drag = self
            .marquee
            .is_none()
            .then(|| self.selected_file_drag())
            .flatten();
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
                        let drop_path = path.clone();
                        let can_drop_path = path.clone();
                        let selected = this.selection.is_selected(&path);
                        let navigable = entry.navigable;
                        let drag = if selected {
                            selected_drag
                                .clone()
                                .unwrap_or_else(|| Self::single_file_drag(&path, navigable))
                        } else {
                            Self::single_file_drag(&path, navigable)
                        };
                        let operation_busy = this.operation_busy;
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
                            .on_drag(drag, |drag, _, _, cx| {
                                let count = drag.paths.len();
                                let label = if count == 1 {
                                    drag.paths[0]
                                        .file_name()
                                        .map(|name| name.to_string_lossy().into_owned())
                                        .unwrap_or_else(|| drag.paths[0].display().to_string())
                                } else {
                                    format!("{count} selected items")
                                };
                                cx.new(|_| DragPreview {
                                    label,
                                    detail: "Move",
                                })
                            })
                            .on_drag_move::<FileDrag>(cx.listener(|this, event, window, cx| {
                                this.update_file_drag_cursor(event, window, cx);
                            }))
                            .when(navigable, |this| {
                                this.can_drop(move |value, _, _| {
                                    !operation_busy
                                        && value.downcast_ref::<FileDrag>().is_some_and(|drag| {
                                            can_move_files_to(&drag.paths, &can_drop_path)
                                        })
                                })
                                .drag_over::<FileDrag>(move |style, _, _, _| {
                                    style.bg(colors.list_active).border_color(colors.primary)
                                })
                                .on_drop(cx.listener(
                                    move |this, drag: &FileDrag, window, cx| {
                                        this.start_drag_move(
                                            drag.paths.to_vec(),
                                            drop_path.clone(),
                                            window,
                                            cx,
                                        );
                                    },
                                ))
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
        // See the list-view note: marquee repaints do not need file-drag
        // payloads, especially when the rectangle spans thousands of items.
        let selected_drag = self
            .marquee
            .is_none()
            .then(|| self.selected_file_drag())
            .flatten();
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
                            let drop_path = path.clone();
                            let can_drop_path = path.clone();
                            let selected = this.selection.is_selected(&path);
                            let navigable = entry.navigable;
                            let drag = if selected {
                                selected_drag
                                    .clone()
                                    .unwrap_or_else(|| Self::single_file_drag(&path, navigable))
                            } else {
                                Self::single_file_drag(&path, navigable)
                            };
                            let operation_busy = this.operation_busy;
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
                                    .on_drag(drag, |drag, _, _, cx| {
                                        let count = drag.paths.len();
                                        let label = if count == 1 {
                                            drag.paths[0]
                                                .file_name()
                                                .map(|name| name.to_string_lossy().into_owned())
                                                .unwrap_or_else(|| {
                                                    drag.paths[0].display().to_string()
                                                })
                                        } else {
                                            format!("{count} selected items")
                                        };
                                        cx.new(|_| DragPreview {
                                            label,
                                            detail: "Move",
                                        })
                                    })
                                    .on_drag_move::<FileDrag>(cx.listener(
                                        |this, event, window, cx| {
                                            this.update_file_drag_cursor(event, window, cx);
                                        },
                                    ))
                                    .when(navigable, |this| {
                                        this.can_drop(move |value, _, _| {
                                            !operation_busy
                                                && value.downcast_ref::<FileDrag>().is_some_and(
                                                    |drag| {
                                                        can_move_files_to(
                                                            &drag.paths,
                                                            &can_drop_path,
                                                        )
                                                    },
                                                )
                                        })
                                        .drag_over::<FileDrag>(move |style, _, _, _| {
                                            style
                                                .bg(colors.list_active)
                                                .border_color(colors.primary)
                                        })
                                        .on_drop(
                                            cx.listener(
                                                move |this, drag: &FileDrag, window, cx| {
                                                    this.start_drag_move(
                                                        drag.paths.to_vec(),
                                                        drop_path.clone(),
                                                        window,
                                                        cx,
                                                    );
                                                },
                                            ),
                                        )
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
        self.place_drop_bounds.borrow_mut().clear();
        self.bookmark_row_bounds.borrow_mut().clear();
        if !cx.has_active_drag() {
            self.bookmark_insertion = None;
            self.file_drag_pointer = None;
            self.file_drag_scroll_task.take();
        }
        let undo_enabled = self.command_enabled(BrowserCommand::UndoFileOperation);
        let redo_enabled = self.command_enabled(BrowserCommand::RedoFileOperation);
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
        let bookmark_buttons = self
            .bookmarks
            .clone()
            .into_iter()
            .enumerate()
            .map(|(index, bookmark)| self.render_bookmark(index, bookmark, cx))
            .collect::<Vec<_>>();
        let bookmark_final_insertion = self.bookmark_insertion
            == Some(BookmarkInsertion {
                index: self.bookmarks.len(),
            });
        let bookmarks_ready = !self.bookmarks_loading;
        let bookmark_region_bounds = self.bookmark_region_bounds.clone();
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

        let sidebar =
            div()
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
                .child(div().h(px(1.0)).my_1().bg(colors.sidebar_border))
                .child(
                    div()
                        .id("bookmarks-section")
                        .relative()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .rounded(cx.theme().radius)
                        .can_drop(move |value, _, _| {
                            bookmarks_ready
                                && (value.downcast_ref::<BookmarkDrag>().is_some()
                                    || value
                                        .downcast_ref::<FileDrag>()
                                        .is_some_and(|drag| !drag.bookmark_candidates.is_empty()))
                        })
                        .drag_over::<FileDrag>(move |style, _, _, _| {
                            style
                                .bg(colors.sidebar_accent.opacity(0.35))
                                .border_1()
                                .border_color(colors.primary)
                        })
                        .drag_over::<BookmarkDrag>(move |style, _, _, _| {
                            style.bg(colors.sidebar_accent.opacity(0.2))
                        })
                        .on_drag_move::<BookmarkDrag>(cx.listener(|this, event, window, cx| {
                            this.set_bookmark_insertion(event, cx);
                            let cursor = if event.bounds.contains(&event.event.position) {
                                CursorStyle::ClosedHand
                            } else {
                                CursorStyle::OperationNotAllowed
                            };
                            cx.set_active_drag_cursor_style(cursor, window);
                        }))
                        .on_drop(cx.listener(|this, drag: &BookmarkDrag, window, cx| {
                            let insertion = this
                                .bookmark_insertion
                                .map(|insertion| insertion.index)
                                .unwrap_or(this.bookmarks.len());
                            this.move_bookmark(drag.index, insertion, window, cx);
                        }))
                        .on_drop(cx.listener(|this, drag: &FileDrag, window, cx| {
                            this.add_dragged_bookmarks(&drag.bookmark_candidates, window, cx);
                        }))
                        .child(
                            div()
                                .h_7()
                                .px_2()
                                .flex()
                                .items_center()
                                .text_sm()
                                .text_color(colors.muted_foreground)
                                .child("Bookmarks"),
                        )
                        .children(bookmark_buttons)
                        .when(self.bookmarks_loading, |this| {
                            this.child(
                                div()
                                    .px_3()
                                    .py_1()
                                    .text_xs()
                                    .text_color(colors.muted_foreground)
                                    .child("Loading bookmarks…"),
                            )
                        })
                        .when(
                            !self.bookmarks_loading && self.bookmarks.is_empty(),
                            |this| {
                                this.child(
                                    div()
                                        .px_3()
                                        .py_1()
                                        .text_xs()
                                        .text_color(colors.muted_foreground)
                                        .child("Drag folders here"),
                                )
                            },
                        )
                        .child(div().h(px(2.0)).mx_2().rounded_full().bg(
                            if bookmark_final_insertion {
                                colors.primary
                            } else {
                                colors.primary.opacity(0.0)
                            },
                        ))
                        .child(div().flex_1())
                        .child(
                            canvas(
                                move |bounds, _, _| bookmark_region_bounds.set(Some(bounds)),
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .inset_0(),
                        ),
                )
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
            .on_action(cx.listener(Self::on_copy_selection))
            .on_action(cx.listener(Self::on_cut_selection))
            .on_action(cx.listener(Self::on_paste_files))
            .on_action(cx.listener(Self::on_new_folder))
            .on_action(cx.listener(Self::on_undo_file_operation))
            .on_action(cx.listener(Self::on_redo_file_operation))
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
        // Keep these five tiny controls Marcel-owned until the component can
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
                            .id("undo-file-operation")
                            .flex()
                            .flex_none()
                            .size(px(28.0))
                            .items_center()
                            .justify_center()
                            .rounded(cx.theme().radius)
                            .text_lg()
                            .text_color(if undo_enabled {
                                colors.sidebar_foreground
                            } else {
                                colors.muted_foreground
                            })
                            .child("↶")
                            .when(undo_enabled, |button| {
                                button
                                    .cursor_pointer()
                                    .hover(|button| button.bg(colors.sidebar_accent))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.execute_browser_command(
                                            BrowserCommand::UndoFileOperation,
                                            window,
                                            cx,
                                        );
                                    }))
                            }),
                    )
                    .child(
                        div()
                            .id("redo-file-operation")
                            .flex()
                            .flex_none()
                            .size(px(28.0))
                            .items_center()
                            .justify_center()
                            .rounded(cx.theme().radius)
                            .text_lg()
                            .text_color(if redo_enabled {
                                colors.sidebar_foreground
                            } else {
                                colors.muted_foreground
                            })
                            .child("↷")
                            .when(redo_enabled, |button| {
                                button
                                    .cursor_pointer()
                                    .hover(|button| button.bg(colors.sidebar_accent))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.execute_browser_command(
                                            BrowserCommand::RedoFileOperation,
                                            window,
                                            cx,
                                        );
                                    }))
                            }),
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
        let bookmark_menu = self.render_bookmark_menu(window, cx);
        // gpui-component 0.5.1's Root stores dialog and notification state but
        // does not attach those layers in Root::render. Mount its public layer
        // renderers here so WindowExt dialogs/notifications are actually
        // visible while retaining the component implementations.
        let dialog_layer = Root::render_dialog_layer(window, cx);
        // gpui-component 0.5.1 hardcodes NotificationList to top-right and
        // exposes no placement option. Keep the component notifications and
        // lifecycle, but mount their public entities in Marcel's bottom-right
        // stack until the component supports configurable placement.
        let notifications = Root::read(window, cx).notification.read(cx).notifications();
        let visible_from = notifications.len().saturating_sub(10);
        let operation_progress = self.render_operation_progress(cx);
        let status_layer = (operation_progress.is_some() || !notifications.is_empty()).then(|| {
            div()
                .absolute()
                .right_4()
                .bottom_4()
                .flex()
                .flex_col()
                .gap_3()
                .when_some(operation_progress, |this, progress| this.child(progress))
                .children(notifications.into_iter().skip(visible_from))
        });
        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(colors.background)
            .text_color(colors.foreground)
            .on_drag_move::<FileDrag>(|_, window, cx| {
                cx.set_active_drag_cursor_style(CursorStyle::OperationNotAllowed, window);
            })
            .on_drag_move::<BookmarkDrag>(|_, window, cx| {
                cx.set_active_drag_cursor_style(CursorStyle::OperationNotAllowed, window);
            })
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
            .when_some(bookmark_menu, |this, menu| this.child(menu))
            .when_some(dialog_layer, |this, layer| this.child(layer))
            .when_some(status_layer, |this, layer| this.child(layer))
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

fn can_move_files_to(paths: &[PathBuf], destination: &Path) -> bool {
    !paths.is_empty()
        && paths.iter().all(|source| {
            source != destination
                && source.parent() != Some(destination)
                && !destination.starts_with(source)
        })
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

fn should_defer_global_filter_to_input(search_focused: bool, input_focused: bool) -> bool {
    input_focused && !search_focused
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
    let zone = px(POINTER_EDGE_SCROLL_ZONE);
    if pointer_y < viewport.top() + zone {
        let proximity = (f32::from(viewport.top() + zone - pointer_y) / POINTER_EDGE_SCROLL_ZONE)
            .clamp(0.0, 1.0);
        px(POINTER_EDGE_MAX_SCROLL_STEP * proximity)
    } else if pointer_y > viewport.bottom() - zone {
        let proximity = (f32::from(pointer_y - (viewport.bottom() - zone))
            / POINTER_EDGE_SCROLL_ZONE)
            .clamp(0.0, 1.0);
        px(-POINTER_EDGE_MAX_SCROLL_STEP * proximity)
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
    fn internal_move_drop_rejects_noops_and_descendants() {
        assert!(!can_move_files_to(
            &[PathBuf::from("/work/report.txt")],
            Path::new("/work")
        ));
        assert!(!can_move_files_to(
            &[PathBuf::from("/work/photos")],
            Path::new("/work/photos/edited")
        ));
        assert!(can_move_files_to(
            &[PathBuf::from("/work/report.txt")],
            Path::new("/archive")
        ));
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
    fn global_filter_yields_to_non_search_editors() {
        assert!(should_defer_global_filter_to_input(false, true));
        assert!(!should_defer_global_filter_to_input(true, true));
        assert!(!should_defer_global_filter_to_input(false, false));
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
