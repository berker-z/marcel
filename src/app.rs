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
    ActiveTheme, Disableable, IndexPath, Root, Sizable, Theme, WindowExt,
    button::{Button, ButtonVariant, ButtonVariants},
    dialog::DialogButtonProps,
    h_flex,
    input::{Input, InputEvent, InputState, Position, SelectToStart as InputSelectToStart},
    notification::Notification,
    progress::Progress,
    resizable::{h_resizable, resizable_panel},
    scroll::ScrollableElement,
    select::{Select, SelectEvent, SelectState},
    switch::Switch,
    text::TextView,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    archive_ops::{default_zip_name, is_supported_archive},
    bookmarks::{
        Bookmark, add as add_bookmark, default_path as default_bookmarks_path,
        load as load_bookmarks, remove as remove_bookmark_at, reorder as reorder_bookmark,
        save as save_bookmarks,
    },
    commands::{
        ActivateSelection, BROWSER_KEY_CONTEXT, BrowserCommand, ClearSelection, CompressSelection,
        CopySelection, CutSelection, DeletePermanently, EmptyTrash, ExtendDown, ExtendLeft,
        ExtendPageDown, ExtendPageUp, ExtendRight, ExtendToFirst, ExtendToLast, ExtendUp,
        ExtractSelection, GoBack, GoForward, GoToParent, MoveDown, MoveLeft, MoveRight, MoveUp,
        NewFolder, OpenTerminal, OpenWithSelection, PasteFiles, RedoFileOperation, RenameSelection,
        RestoreSelection, SelectAll, SelectFirst, SelectLast, SelectPageDown, SelectPageUp,
        TrashSelection, UndoFileOperation,
    },
    delete_ops::{delete_paths, summarize_failures as summarize_delete_failures},
    directory_session::{
        ApplyDirectoryEvents, DirectoryEvent, DirectorySession, ReconcileSelection,
    },
    directory_watcher::{DirectoryWatcherUpdate, revalidate_paths, watch_directory},
    file_ops::{
        DirectoryChanges, OperationRecord, TransferMode, create_directory, create_zip_operation,
        extract_archive_operation, redo_operation, rename_entry, summarize_failures,
        transfer_paths_with_progress, undo_operation, validate_entry_name,
    },
    fs::{
        DirectoryUpdate, EntryKind, FileEntry, format_size, merge_sorted_entries, stream_directory,
        stream_directory_cancellable,
    },
    history::NavigationHistory,
    operations::{FileClipboard, OperationController, OperationProgressKind},
    places::{Place, discover as discover_places},
    preview::{Preview, PreviewState, load_preview},
    theme::{self, Palette},
    thumbnails,
    trash_ops::{
        TrashRecord, list_trash_records, purge_trash_records, restore_trash_records,
        summarize_failures as summarize_trash_failures, trash_paths,
    },
};

const DIRECTORY_ROW_HEIGHT: f32 = 36.0;
const GRID_TILE_WIDTH: f32 = 120.0;
const GRID_TILE_HEIGHT: f32 = 188.0;
const GRID_GAP: f32 = 8.0;
const GRID_SIDE_PADDING: f32 = 16.0;
const GRID_VISUAL_SIZE: f32 = 104.0;
const GRID_ICON_SIZE: f32 = 80.0;
const GRID_LABEL_HEIGHT: f32 = 64.0;
const GRID_LABEL_COLUMNS: usize = 30;
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

#[derive(Clone, Copy, Debug)]
struct EntryHitRegion {
    bounds: Bounds<Pixels>,
    navigable: bool,
}

struct DragPreview {
    label: String,
    detail: &'static str,
}

enum PermanentDeleteResult {
    Files(crate::delete_ops::DeleteOutcome),
    Trash {
        outcome: crate::trash_ops::TrashOutcome,
        requested: Vec<TrashRecord>,
    },
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
    palette: Palette,
    system_ui_font: SharedString,
    use_iosevka_ui: bool,
    iosevka_ui_font: Option<SharedString>,
    directory: DirectorySession,
    search_input: Entity<InputState>,
    _search_subscriptions: Vec<Subscription>,
    rename_path: Option<PathBuf>,
    rename_input: Option<Entity<InputState>>,
    rename_subscription: Option<Subscription>,
    operations: OperationController,
    entry_menu: Option<EntryMenu>,
    marquee: Option<MarqueeGesture>,
    marquee_scroll_task: Option<Task<()>>,
    file_drag_pointer: Option<Point<Pixels>>,
    file_drag_scroll_task: Option<Task<()>>,
    browser_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    entry_hit_bounds: Rc<RefCell<HashMap<PathBuf, EntryHitRegion>>>,
    entry_content_bounds: Rc<RefCell<HashMap<PathBuf, Bounds<Pixels>>>>,
    directory_scroll: UniformListScrollHandle,
    view_mode: ViewMode,
    grid_layout_columns: usize,
    thumbnails: HashMap<PathBuf, ThumbnailState>,
    thumbnail_order: VecDeque<PathBuf>,
    thumbnail_queue: VecDeque<PathBuf>,
    thumbnail_pending: HashSet<PathBuf>,
    thumbnail_inflight: HashSet<PathBuf>,
    thumbnail_stale: HashSet<PathBuf>,
    thumbnail_wake_sender: async_channel::Sender<()>,
    thumbnail_wake_receiver: async_channel::Receiver<()>,
    thumbnail_workers: Vec<Task<()>>,
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
    browsing_trash: bool,
    trash_records: HashMap<PathBuf, TrashRecord>,
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
            palette: theme::active(),
            system_ui_font,
            use_iosevka_ui,
            iosevka_ui_font,
            directory: DirectorySession::new(start_dir.clone()),
            search_input,
            _search_subscriptions: vec![search_subscription],
            rename_path: None,
            rename_input: None,
            rename_subscription: None,
            operations: OperationController::default(),
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
            thumbnail_stale: HashSet::new(),
            thumbnail_wake_sender,
            thumbnail_wake_receiver,
            thumbnail_workers: Vec::new(),
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
            browsing_trash: false,
            trash_records: HashMap::new(),
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
        if paths.is_empty() {
            self.bookmark_insertion = None;
            return;
        }

        let mut added = 0;
        for path in paths {
            if add_bookmark(&mut self.bookmarks, path.clone()) {
                if let Some(icon) = self
                    .directory
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
                .text_sm()
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
        let active = self.operations.progress()?;
        let snapshot = active.progress.snapshot();
        let cancelling = self.operations.is_cancelling();
        let colors = cx.theme().colors;
        let title = if cancelling {
            "Cancelling…"
        } else {
            match active.kind {
                OperationProgressKind::Copy => "Copying",
                OperationProgressKind::Move => "Moving",
                OperationProgressKind::Compress => "Compressing",
                OperationProgressKind::Extract => "Extracting",
                OperationProgressKind::Delete => "Deleting permanently",
                OperationProgressKind::EmptyTrash => "Emptying Trash",
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
        let detail = active.detail.clone();
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
                                        .child(detail),
                                ),
                        )
                        .when(active.cancellable, |this| this.child(cancel_button)),
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
        let selected = self.directory.selection.selected();
        if selected.is_empty() {
            return None;
        }
        let mut paths = Vec::with_capacity(selected.len());
        let mut bookmark_candidates = Vec::new();
        for index in &self.directory.visible_entries {
            let Some(entry) = self.directory.entries.get(*index) else {
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
        let operation_busy = self.operations.is_busy();
        let can_move_to = |path: &Path| !operation_busy && can_move_files_to(&drag.paths, path);

        let over_browser_folder = self.entry_hit_bounds.borrow().iter().any(|(path, hit)| {
            can_drop_on_entry_hit(hit, pointer, &drag.paths, path, operation_busy)
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
        self.rename_path = None;
        self.rename_input = None;
        self.rename_subscription = None;
        if self.browsing_trash {
            self.start_trash_load(clear_filter, cx);
            return;
        }
        self.entry_menu = None;
        self.bookmark_menu = None;
        let (ticket, path) = self.directory.begin_load(clear_filter);
        self.thumbnail_workers.clear();
        self.thumbnail_queue.clear();
        self.thumbnail_pending.clear();
        self.thumbnail_inflight.clear();
        self.thumbnail_stale.clear();
        while self.thumbnail_wake_receiver.try_recv().is_ok() {}
        self.thumbnails.clear();
        self.thumbnail_order.clear();
        self.entry_content_bounds.borrow_mut().clear();
        self.clear_selection();

        let (sender, receiver) = async_channel::unbounded();
        let stream_path = path.clone();
        cx.background_executor()
            .spawn(smol::unblock(move || {
                stream_directory(&stream_path, sender)
            }))
            .detach();

        self.directory.load_task = Some(cx.spawn(async move |this, cx| {
            while let Ok(update) = receiver.recv().await {
                let should_continue = this
                    .update(cx, |this, cx| {
                        if ticket != this.directory.generation {
                            return false;
                        }

                        match update {
                            DirectoryUpdate::Batch(batch) => {
                                let reconcile = this.directory.merge_batch(batch);
                                this.apply_selection_reconcile(reconcile, cx);
                                this.select_pending_loaded_entry(cx);
                            }
                            DirectoryUpdate::Done => {
                                this.directory.finish_load();
                                this.start_directory_watcher(ticket, path.clone(), cx);
                            }
                            DirectoryUpdate::Error(error) => {
                                this.directory.fail_load(error);
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

    fn start_trash_load(&mut self, clear_filter: bool, cx: &mut Context<Self>) {
        self.browsing_trash = true;
        self.entry_menu = None;
        self.bookmark_menu = None;
        let ticket = self.directory.begin_virtual_load(clear_filter);
        self.thumbnail_workers.clear();
        self.thumbnail_queue.clear();
        self.thumbnail_pending.clear();
        self.thumbnail_inflight.clear();
        self.thumbnail_stale.clear();
        while self.thumbnail_wake_receiver.try_recv().is_ok() {}
        self.thumbnails.clear();
        self.thumbnail_order.clear();
        self.entry_content_bounds.borrow_mut().clear();
        self.trash_records.clear();
        self.clear_selection();

        let load = cx.background_executor().spawn(smol::unblock(move || {
            let records = list_trash_records()?;
            let mut icons = crate::icons::IconProvider::discover();
            let mut entries = Vec::with_capacity(records.len());
            let mut by_backing = HashMap::with_capacity(records.len());
            for record in records {
                let Ok(mut entry) = FileEntry::from_path(record.backing_path(), &mut icons) else {
                    continue;
                };
                entry.name = record
                    .original_path()
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| entry.name.clone());
                // The first Trash slice presents top-level items. Directories
                // remain previewable, but nested virtual navigation follows
                // after the location abstraction is generalized.
                entry.navigable = false;
                by_backing.insert(entry.path.clone(), record);
                entries.push(entry);
            }
            crate::fs::sort_entries(&mut entries);
            anyhow::Ok((entries, by_backing))
        }));

        self.directory.load_task = Some(cx.spawn(async move |this, cx| {
            let result = load.await;
            let _ = this.update(cx, |this, cx| {
                if ticket != this.directory.generation || !this.browsing_trash {
                    return;
                }
                match result {
                    Ok((entries, records)) => {
                        this.trash_records = records;
                        let reconcile = this.directory.merge_batch(entries);
                        this.apply_selection_reconcile(reconcile, cx);
                        this.directory.finish_load();
                    }
                    Err(error) => this.directory.fail_load(error.to_string()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn start_directory_watcher(&mut self, ticket: u64, path: PathBuf, cx: &mut Context<Self>) {
        let (sender, receiver) = async_channel::unbounded();
        let cancelled = Arc::new(AtomicBool::new(false));
        let watcher_cancelled = cancelled.clone();
        let watched_path = path.clone();
        cx.background_executor()
            .spawn(smol::unblock(move || {
                watch_directory(&watched_path, sender, watcher_cancelled)
            }))
            .detach();

        let task = cx.spawn(async move |this, cx| {
            while let Ok(update) = receiver.recv().await {
                let should_continue = this
                    .update(cx, |this, cx| {
                        if ticket != this.directory.generation || path != this.directory.current_dir
                        {
                            return false;
                        }

                        match update {
                            DirectoryWatcherUpdate::Events(events) => {
                                let affected = directory_event_paths(&events);
                                match this.directory.apply_events(events) {
                                    ApplyDirectoryEvents::Applied(reconcile) => {
                                        this.invalidate_watched_thumbnails(&affected);
                                        this.apply_selection_reconcile(reconcile, cx);
                                        this.select_pending_loaded_entry(cx);
                                    }
                                    ApplyDirectoryEvents::RescanRequired => {
                                        this.start_directory_load(false, cx);
                                        return false;
                                    }
                                }
                            }
                            DirectoryWatcherUpdate::RescanRequired => {
                                this.start_directory_load(false, cx);
                                return false;
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
        });
        self.directory.set_watcher(cancelled, task);
    }

    fn invalidate_watched_thumbnails(&mut self, paths: &[PathBuf]) {
        for path in paths {
            self.thumbnails.remove(path);
            self.thumbnail_order.retain(|existing| existing != path);
            self.thumbnail_queue.retain(|queued| queued != path);
            self.thumbnail_pending.remove(path);
            if self.thumbnail_inflight.contains(path) {
                self.thumbnail_stale.insert(path.clone());
            }
        }
    }

    fn select_pending_loaded_entry(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.directory.take_pending_visible_entry() {
            self.start_preview(entry, cx);
        }
    }

    fn navigate_to(&mut self, path: PathBuf, add_to_history: bool, cx: &mut Context<Self>) {
        if !self.browsing_trash && path == self.directory.current_dir {
            return;
        }
        self.browsing_trash = false;
        self.trash_records.clear();
        if add_to_history {
            self.history.push(&path);
        }
        self.directory.current_dir = path;
        self.directory.pending_reveal = None;
        self.start_directory_load(true, cx);
    }

    fn go_back(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.history.go_back() {
            self.browsing_trash = false;
            self.trash_records.clear();
            self.directory.current_dir = path;
            self.directory.pending_reveal = None;
            self.start_directory_load(true, cx);
        }
    }

    fn go_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.history.go_forward() {
            self.browsing_trash = false;
            self.trash_records.clear();
            self.directory.current_dir = path;
            self.directory.pending_reveal = None;
            self.start_directory_load(true, cx);
        }
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        if self.browsing_trash {
            return;
        }
        if let Some(parent) = self.directory.current_dir.parent() {
            self.navigate_to(parent.to_path_buf(), true, cx);
        }
    }

    fn command_enabled(&self, command: BrowserCommand) -> bool {
        match command {
            BrowserCommand::GoToParent => {
                !self.browsing_trash && self.directory.current_dir.parent().is_some()
            }
            BrowserCommand::GoBack => self.history.can_go_back(),
            BrowserCommand::GoForward => self.history.can_go_forward(),
            BrowserCommand::ActivateSelection => {
                self.directory.selection.primary().is_some()
                    && (!self.browsing_trash
                        || self
                            .directory
                            .selection
                            .primary()
                            .and_then(|path| {
                                self.directory
                                    .entries
                                    .iter()
                                    .find(|entry| &entry.path == path)
                            })
                            .is_some_and(|entry| entry.kind != EntryKind::Directory))
            }
            BrowserCommand::OpenWithSelection => self
                .directory
                .selection
                .primary()
                .and_then(|path| {
                    self.directory
                        .entries
                        .iter()
                        .find(|entry| &entry.path == path)
                })
                .is_some_and(|entry| !entry.navigable && entry.kind != EntryKind::Directory),
            BrowserCommand::ClearSelection => {
                self.rename_path.is_some() || !self.directory.selection.selected().is_empty()
            }
            BrowserCommand::CopySelection | BrowserCommand::CutSelection => {
                !self.browsing_trash
                    && !self.operations.is_busy()
                    && !self.directory.selection.selected().is_empty()
            }
            BrowserCommand::PasteFiles => {
                !self.browsing_trash
                    && !self.operations.is_busy()
                    && self.directory.error.is_none()
                    && self
                        .operations
                        .clipboard()
                        .is_some_and(|clipboard| !clipboard.paths.is_empty())
            }
            BrowserCommand::NewFolder => {
                !self.browsing_trash && !self.operations.is_busy() && self.directory.error.is_none()
            }
            BrowserCommand::OpenTerminal => {
                !self.browsing_trash
                    && !self.directory.loading
                    && self.directory.error.is_none()
                    && self.directory.current_dir.is_dir()
            }
            BrowserCommand::RenameSelection => {
                !self.browsing_trash
                    && !self.operations.is_busy()
                    && self.directory.error.is_none()
                    && self.directory.selection.selected().len() == 1
                    && self
                        .directory
                        .selection
                        .primary()
                        .and_then(|path| path.file_name())
                        .and_then(|name| name.to_str())
                        .is_some()
            }
            BrowserCommand::CompressSelection => {
                !self.browsing_trash
                    && !self.operations.is_busy()
                    && self.directory.error.is_none()
                    && !self.directory.selection.selected().is_empty()
            }
            BrowserCommand::ExtractSelection => {
                !self.browsing_trash
                    && !self.operations.is_busy()
                    && self.directory.error.is_none()
                    && self.directory.selection.selected().len() == 1
                    && self
                        .directory
                        .selection
                        .primary()
                        .and_then(|path| {
                            self.directory
                                .entries
                                .iter()
                                .find(|entry| &entry.path == path)
                        })
                        .is_some_and(|entry| {
                            entry.kind != EntryKind::Directory && is_supported_archive(&entry.path)
                        })
            }
            BrowserCommand::TrashSelection => {
                !self.browsing_trash
                    && !self.operations.is_busy()
                    && !self.directory.selection.selected().is_empty()
            }
            BrowserCommand::RestoreSelection => {
                self.browsing_trash
                    && !self.operations.is_busy()
                    && !self.directory.selection.selected().is_empty()
                    && self
                        .directory
                        .selection
                        .selected()
                        .iter()
                        .all(|path| self.trash_records.contains_key(path))
            }
            BrowserCommand::DeletePermanently => {
                !self.operations.is_busy()
                    && !self.directory.selection.selected().is_empty()
                    && (!self.browsing_trash
                        || self
                            .directory
                            .selection
                            .selected()
                            .iter()
                            .all(|path| self.trash_records.contains_key(path)))
            }
            BrowserCommand::EmptyTrash => {
                self.browsing_trash && !self.operations.is_busy() && !self.trash_records.is_empty()
            }
            BrowserCommand::UndoFileOperation => self.operations.can_undo(),
            BrowserCommand::RedoFileOperation => self.operations.can_redo(),
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
            | BrowserCommand::SelectAll => !self.directory.visible_entries.is_empty(),
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
                if self.rename_path.is_some() {
                    self.cancel_rename(window, cx);
                } else {
                    self.clear_selection();
                    cx.notify();
                }
            }
            BrowserCommand::GoToParent => self.go_up(cx),
            BrowserCommand::GoBack => self.go_back(cx),
            BrowserCommand::GoForward => self.go_forward(cx),
            BrowserCommand::SelectAll => self.select_all_entries(cx),
            BrowserCommand::CopySelection => self.stage_selection(TransferMode::Copy, window, cx),
            BrowserCommand::CutSelection => self.stage_selection(TransferMode::Move, window, cx),
            BrowserCommand::PasteFiles => self.start_paste(window, cx),
            BrowserCommand::TrashSelection => self.start_trash_selection(window, cx),
            BrowserCommand::RestoreSelection => self.start_restore_selection(window, cx),
            BrowserCommand::DeletePermanently => self.open_permanent_delete_dialog(window, cx),
            BrowserCommand::EmptyTrash => self.open_empty_trash_dialog(window, cx),
            BrowserCommand::NewFolder => self.open_new_folder_dialog(window, cx),
            BrowserCommand::OpenTerminal => self.open_terminal(window, cx),
            BrowserCommand::RenameSelection => self.begin_rename(window, cx),
            BrowserCommand::CompressSelection => self.open_compress_dialog(window, cx),
            BrowserCommand::ExtractSelection => self.start_extract_selection(window, cx),
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
        let current = self.directory.selection.primary().and_then(|path| {
            self.directory
                .visible_entries
                .iter()
                .position(|entry_index| {
                    self.directory
                        .entries
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
            self.directory.visible_entries.len(),
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
            self.directory
                .selection
                .select_range(entry.path.clone(), &ordered, false);
        } else {
            self.directory.selection.select_only(entry.path.clone());
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
            .directory
            .selection
            .primary()
            .and_then(|path| {
                self.directory
                    .entries
                    .iter()
                    .find(|entry| &entry.path == path)
            })
            .cloned()
        else {
            return;
        };
        self.open_entry(entry, cx);
    }

    fn select_all_entries(&mut self, cx: &mut Context<Self>) {
        let ordered = self.visible_paths();
        self.directory.selection.select_all(&ordered);
        if let Some(entry) = self
            .directory
            .selection
            .primary()
            .and_then(|path| {
                self.directory
                    .entries
                    .iter()
                    .find(|entry| &entry.path == path)
            })
            .cloned()
        {
            self.start_preview(entry, cx);
        }
        cx.notify();
    }

    fn begin_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.entry_menu = None;
        let Some(path) = self.directory.selection.primary().cloned() else {
            return;
        };
        let Some(name) = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
        else {
            window.push_notification(
                Notification::error("This filename is not valid UTF-8 and cannot be edited yet"),
                cx,
            );
            return;
        };
        let is_directory = self
            .directory
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .is_some_and(|entry| entry.kind == EntryKind::Directory);
        let selection_end = rename_stem_end(&name, is_directory);
        let input = cx.new(|cx| InputState::new(window, cx).default_value(name));
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } | InputEvent::Blur => {
                    this.submit_rename(input, window, cx);
                }
                InputEvent::Change | InputEvent::Focus => {}
            },
        );
        self.rename_path = Some(path);
        self.rename_input = Some(input.clone());
        self.rename_subscription = Some(subscription);
        cx.notify();

        cx.defer_in(window, move |_, window, cx| {
            input.update(cx, |input, cx| {
                input.set_cursor_position(Position::new(0, selection_end as u32), window, cx);
            });
            window.dispatch_action(Box::new(InputSelectToStart), cx);
        });
    }

    fn submit_rename(
        &mut self,
        input: &Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.rename_path.clone() else {
            return;
        };
        let name = input.read(cx).value().to_string();
        if let Err(error) = validate_entry_name(&name) {
            window.push_notification(Notification::error(error.to_string()), cx);
            input.update(cx, |input, cx| input.focus(window, cx));
            return;
        }
        if path.file_name() == Some(std::ffi::OsStr::new(&name)) {
            self.cancel_rename(window, cx);
            return;
        }

        self.rename_path = None;
        self.rename_input = None;
        self.rename_subscription = None;
        self.browser_focus.focus(window);
        self.start_rename(path, name, window, cx);
    }

    fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rename_path = None;
        self.rename_input = None;
        self.rename_subscription = None;
        self.browser_focus.focus(window);
        cx.notify();
    }

    fn start_rename(
        &mut self,
        source: PathBuf,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.operations.begin_simple() {
            return;
        }
        let attempted_destination = source
            .parent()
            .map(|parent| parent.join(&name))
            .unwrap_or_else(|| source.clone());
        let task_source = source.clone();
        let task = cx
            .background_executor()
            .spawn(smol::unblock(move || rename_entry(&task_source, &name)));
        let operation_task = cx.spawn_in(window, async move |this, window| {
            let result = task.await;
            let _ = this.update_in(window, |this, window, cx| {
                match result {
                    Ok(operation) => {
                        let destination = operation.path().to_path_buf();
                        let display_name = destination
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let changes = operation.forward_directory_changes();
                        this.operations.record(operation);
                        this.apply_directory_changes(changes, Some(destination), cx);
                        window.push_notification(
                            Notification::success(format!("Renamed to “{display_name}”")),
                            cx,
                        );
                    }
                    Err(error) => {
                        window.push_notification(Notification::error(error.to_string()), cx);
                        // Most failures leave the source untouched. A failure
                        // after the no-replace syscall (for example while
                        // inspecting the result) may still have renamed it, so
                        // revalidate both possible paths instead of assuming
                        // either filesystem state.
                        this.apply_directory_changes(
                            DirectoryChanges {
                                upserted: vec![source.clone(), attempted_destination.clone()],
                                ..DirectoryChanges::default()
                            },
                            None,
                            cx,
                        );
                    }
                }
                this.operations.finish_active();
                cx.notify();
            });
        });
        self.operations.set_task(operation_task);
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
        if !self.operations.begin_simple() {
            return;
        }

        let parent = self.directory.current_dir.clone();
        let task = cx
            .background_executor()
            .spawn(smol::unblock(move || create_directory(&parent, &name)));

        let operation_task = cx.spawn_in(window, async move |this, window| {
            let result = task.await;
            let _ = this.update_in(window, |this, window, cx| {
                match result {
                    Ok(operation) => {
                        let path = operation.path().to_path_buf();
                        let changes = operation.forward_directory_changes();
                        this.operations.record(operation);
                        this.apply_directory_changes(changes, Some(path.clone()), cx);
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
                this.operations.finish_active();
                cx.notify();
            });
        });
        self.operations.set_task(operation_task);
        cx.notify();
    }

    fn open_compress_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.entry_menu = None;
        let selected = self.directory.selection.selected();
        let sources = self
            .visible_paths()
            .into_iter()
            .filter(|path| selected.contains(path))
            .collect::<Vec<_>>();
        if sources.is_empty() {
            return;
        }
        let single_is_directory = sources.len() == 1
            && self
                .directory
                .entries
                .iter()
                .find(|entry| entry.path == sources[0])
                .is_some_and(|entry| entry.kind == EntryKind::Directory);
        let proposed_name = default_zip_name(&sources, single_is_directory);
        let input = cx.new(|cx| InputState::new(window, cx).default_value(proposed_name.clone()));
        let input_for_dialog = input.clone();
        let view = cx.entity();

        window.open_dialog(cx, move |dialog, _, _| {
            let input_for_ok = input_for_dialog.clone();
            let view = view.clone();
            let sources = sources.clone();
            dialog
                .title("Compress to ZIP")
                .child(Input::new(&input_for_dialog))
                .confirm()
                .button_props(DialogButtonProps::default().ok_text("Compress"))
                .on_ok(move |_, window, cx| {
                    let name = input_for_ok.read(cx).value().trim().to_string();
                    if let Err(error) = validate_entry_name(&name) {
                        window.push_notification(Notification::error(error.to_string()), cx);
                        return false;
                    }
                    if !name.to_ascii_lowercase().ends_with(".zip") {
                        window.push_notification(
                            Notification::error("The archive name must end in .zip"),
                            cx,
                        );
                        return false;
                    }
                    view.update(cx, |this, cx| {
                        this.start_compress(sources.clone(), name.clone(), window, cx);
                    });
                    true
                })
        });
        input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn start_compress(
        &mut self,
        sources: Vec<PathBuf>,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let destination = self.directory.current_dir.join(name);
        let Some((cancel, _progress)) = self.operations.begin_archive(
            OperationProgressKind::Compress,
            sources.len(),
            format!("to {}", destination.display()),
        ) else {
            return;
        };
        self.start_operation_progress_refresh(cx);
        let task = cx.background_executor().spawn(smol::unblock(move || {
            create_zip_operation(&sources, &destination, cancel)
        }));
        let operation_task = cx.spawn_in(window, async move |this, window| {
            let result = task.await;
            let _ = this.update_in(window, |this, window, cx| {
                match result {
                    Ok(operation) => {
                        let path = operation.path().to_path_buf();
                        let changes = operation.forward_directory_changes();
                        this.operations.record(operation);
                        this.apply_directory_changes(changes, Some(path.clone()), cx);
                        window.push_notification(
                            Notification::success(format!(
                                "Created ZIP “{}”",
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
                this.operations.finish_active();
                cx.notify();
            });
        });
        self.operations.set_task(operation_task);
        cx.notify();
    }

    fn start_extract_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.entry_menu = None;
        let Some(archive) = self.directory.selection.primary().cloned() else {
            return;
        };
        let Some((cancel, _progress)) = self.operations.begin_archive(
            OperationProgressKind::Extract,
            1,
            format!("from {}", archive.display()),
        ) else {
            return;
        };
        self.start_operation_progress_refresh(cx);
        let task = cx.background_executor().spawn(smol::unblock(move || {
            extract_archive_operation(&archive, cancel)
        }));
        let operation_task = cx.spawn_in(window, async move |this, window| {
            let result = task.await;
            let _ = this.update_in(window, |this, window, cx| {
                match result {
                    Ok(operation) => {
                        let path = operation.path().to_path_buf();
                        let changes = operation.forward_directory_changes();
                        this.operations.record(operation);
                        this.apply_directory_changes(changes, Some(path.clone()), cx);
                        window.push_notification(
                            Notification::success(format!(
                                "Extracted “{}”",
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
                this.operations.finish_active();
                cx.notify();
            });
        });
        self.operations.set_task(operation_task);
        cx.notify();
    }

    fn start_undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(operation) = self.operations.begin_undo() else {
            return;
        };

        let operation_for_task = operation.clone();
        let task = cx
            .background_executor()
            .spawn(smol::unblock(move || undo_operation(&operation_for_task)));

        let operation_task = cx.spawn_in(window, async move |this, window| {
            let result = task.await;
            let _ = this.update_in(window, |this, window, cx| {
                match result {
                    Ok(undone) => {
                        let reveal = matches!(&operation, OperationRecord::Rename { .. });
                        let path = if reveal {
                            undone.path().to_path_buf()
                        } else {
                            operation.path().to_path_buf()
                        };
                        let changes = operation.reverse_directory_changes();
                        let message = operation_history_message(&operation, false);
                        if this.browsing_trash {
                            match &operation {
                                OperationRecord::Trash { records } => this.remove_trash_entries(
                                    records
                                        .iter()
                                        .map(|record| record.backing_path().to_path_buf())
                                        .collect(),
                                    cx,
                                ),
                                OperationRecord::Restore { .. } => {
                                    this.upsert_trash_entries(
                                        undone.trash_records().unwrap_or_default().to_vec(),
                                        cx,
                                    );
                                }
                                _ => {}
                            }
                        } else {
                            this.apply_directory_changes(changes, reveal.then_some(path), cx);
                        }
                        this.operations.finish_undo(undone);
                        window.push_notification(Notification::success(message), cx);
                    }
                    Err(error) => {
                        this.operations.cancel_undo(operation);
                        window.push_notification(Notification::error(error.to_string()), cx);
                    }
                }
                this.operations.finish_active();
                cx.notify();
            });
        });
        self.operations.set_task(operation_task);
        cx.notify();
    }

    fn start_redo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(operation) = self.operations.begin_redo() else {
            return;
        };

        let operation_for_task = operation.clone();
        let task = cx
            .background_executor()
            .spawn(smol::unblock(move || redo_operation(&operation_for_task)));

        let operation_task = cx.spawn_in(window, async move |this, window| {
            let result = task.await;
            let _ = this.update_in(window, |this, window, cx| {
                match result {
                    Ok(redone) => {
                        let path = redone.path().to_path_buf();
                        let changes = redone.forward_directory_changes();
                        let message = operation_history_message(&redone, true);
                        if this.browsing_trash {
                            match &redone {
                                OperationRecord::Trash { .. } => {
                                    this.upsert_trash_entries(
                                        redone.trash_records().unwrap_or_default().to_vec(),
                                        cx,
                                    );
                                }
                                OperationRecord::Restore { records } => {
                                    this.remove_trash_entries(
                                        records
                                            .iter()
                                            .map(|record| record.backing_path().to_path_buf())
                                            .collect(),
                                        cx,
                                    );
                                }
                                _ => {}
                            }
                        } else {
                            this.apply_directory_changes(changes, Some(path), cx);
                        }
                        this.operations.finish_redo(redone);
                        window.push_notification(Notification::success(message), cx);
                    }
                    Err(error) => {
                        this.operations.cancel_redo(operation);
                        window.push_notification(Notification::error(error.to_string()), cx);
                    }
                }
                this.operations.finish_active();
                cx.notify();
            });
        });
        self.operations.set_task(operation_task);
        cx.notify();
    }

    fn stage_selection(&mut self, mode: TransferMode, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.directory.selection.selected();
        let paths = self
            .visible_paths()
            .into_iter()
            .filter(|path| selected.contains(path))
            .collect::<Vec<_>>();
        if paths.is_empty() {
            return;
        }
        let count = paths.len();
        self.operations
            .set_clipboard(Some(FileClipboard { mode, paths }));
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
        let Some(clipboard) = self.operations.clipboard().cloned() else {
            return;
        };
        self.start_transfer(
            clipboard.paths.clone(),
            self.directory.current_dir.clone(),
            clipboard.mode,
            Some(clipboard),
            window,
            cx,
        );
    }

    fn start_trash_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.directory.selection.selected();
        let paths = self
            .visible_paths()
            .into_iter()
            .filter(|path| selected.contains(path))
            .collect::<Vec<_>>();
        if paths.is_empty() || !self.operations.begin_simple() {
            return;
        }
        self.entry_menu = None;
        let task = cx
            .background_executor()
            .spawn(smol::unblock(move || trash_paths(&paths)));
        let operation_task = cx.spawn_in(window, async move |this, window| {
            let outcome = task.await;
            let _ = this.update_in(window, |this, window, cx| {
                let changes = DirectoryChanges {
                    removed: outcome.completed.clone(),
                    ..DirectoryChanges::default()
                };
                if !outcome.records.is_empty() {
                    this.operations.record(OperationRecord::Trash {
                        records: outcome.records,
                    });
                }
                this.apply_directory_changes(changes, None, cx);
                if outcome.failures.is_empty() {
                    window.push_notification(
                        Notification::success(format!(
                            "Moved {} item(s) to Trash",
                            outcome.completed.len()
                        )),
                        cx,
                    );
                } else {
                    let mut message = summarize_trash_failures(&outcome.failures);
                    if outcome.undo_unavailable {
                        message.push_str("; some successful items are not available to Undo");
                    }
                    window.push_notification(Notification::error(message), cx);
                }
                this.operations.finish_active();
                cx.notify();
            });
        });
        self.operations.set_task(operation_task);
        cx.notify();
    }

    fn start_restore_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.directory.selection.selected();
        let records = self
            .visible_paths()
            .into_iter()
            .filter(|path| selected.contains(path))
            .filter_map(|path| self.trash_records.get(&path).cloned())
            .collect::<Vec<_>>();
        if records.is_empty() || !self.operations.begin_simple() {
            return;
        }
        self.entry_menu = None;
        let count = records.len();
        let backing_paths = records
            .iter()
            .map(|record| record.backing_path().to_path_buf())
            .collect::<Vec<_>>();
        let task = cx
            .background_executor()
            .spawn(smol::unblock(move || restore_trash_records(&records)));
        let operation_task = cx.spawn_in(window, async move |this, window| {
            let result = task.await;
            let _ = this.update_in(window, |this, window, cx| {
                match result {
                    Ok(records) => {
                        this.operations.record(OperationRecord::Restore { records });
                        this.remove_trash_entries(backing_paths, cx);
                        window.push_notification(
                            Notification::success(format!("Restored {count} item(s)")),
                            cx,
                        );
                    }
                    Err(error) => {
                        window.push_notification(Notification::error(error.to_string()), cx);
                    }
                }
                this.operations.finish_active();
                cx.notify();
            });
        });
        self.operations.set_task(operation_task);
        cx.notify();
    }

    fn open_permanent_delete_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.entry_menu = None;
        let selected = self.directory.selection.selected();
        let paths = self
            .visible_paths()
            .into_iter()
            .filter(|path| selected.contains(path))
            .collect::<Vec<_>>();
        if paths.is_empty() {
            return;
        }
        let trash_records = self.browsing_trash.then(|| {
            paths
                .iter()
                .filter_map(|path| self.trash_records.get(path).cloned())
                .collect::<Vec<_>>()
        });
        if trash_records.as_ref().is_some_and(Vec::is_empty) {
            return;
        }
        let count = paths.len();
        let description = if count == 1 {
            let name = paths[0]
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| paths[0].display().to_string());
            format!("Permanently delete “{name}”?")
        } else {
            format!("Permanently delete {count} selected items?")
        };
        let view = cx.entity();
        let danger = cx.theme().colors.danger;
        window.open_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            let paths = paths.clone();
            let trash_records = trash_records.clone();
            dialog
                .title("Delete Permanently")
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(description.clone())
                        .child(
                            div()
                                .text_sm()
                                .text_color(danger)
                                .child("This action cannot be undone."),
                        ),
                )
                .confirm()
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Delete Permanently")
                        .ok_variant(ButtonVariant::Danger),
                )
                .on_ok(move |_, window, cx| {
                    view.update(cx, |this, cx| {
                        this.start_permanent_delete(
                            paths.clone(),
                            trash_records.clone(),
                            OperationProgressKind::Delete,
                            window,
                            cx,
                        );
                    });
                    true
                })
        });
        cx.notify();
    }

    fn open_empty_trash_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.entry_menu = None;
        let records = self.trash_records.values().cloned().collect::<Vec<_>>();
        if records.is_empty() {
            return;
        }
        let count = records.len();
        let view = cx.entity();
        let danger = cx.theme().colors.danger;
        window.open_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            let records = records.clone();
            dialog
                .title("Empty Trash")
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(format!(
                            "Permanently delete all {count} item(s) currently shown in Trash?"
                        ))
                        .child(
                            div()
                                .text_sm()
                                .text_color(danger)
                                .child("This action cannot be undone."),
                        ),
                )
                .confirm()
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Empty Trash")
                        .ok_variant(ButtonVariant::Danger),
                )
                .on_ok(move |_, window, cx| {
                    view.update(cx, |this, cx| {
                        this.start_permanent_delete(
                            Vec::new(),
                            Some(records.clone()),
                            OperationProgressKind::EmptyTrash,
                            window,
                            cx,
                        );
                    });
                    true
                })
        });
        cx.notify();
    }

    fn start_permanent_delete(
        &mut self,
        paths: Vec<PathBuf>,
        trash_records: Option<Vec<TrashRecord>>,
        kind: OperationProgressKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let source_count = trash_records
            .as_ref()
            .map_or(paths.len(), |records| records.len());
        let Some(progress) = self.operations.begin_permanent_delete(kind, source_count) else {
            return;
        };
        self.start_operation_progress_refresh(cx);
        let task_progress = progress.clone();
        let task = cx.background_executor().spawn(smol::unblock(move || {
            if let Some(records) = trash_records {
                let outcome = purge_trash_records(&records, task_progress);
                PermanentDeleteResult::Trash {
                    outcome,
                    requested: records,
                }
            } else {
                PermanentDeleteResult::Files(delete_paths(&paths, task_progress))
            }
        }));
        let operation_task = cx.spawn_in(window, async move |this, window| {
            let result = task.await;
            let _ = this.update_in(window, |this, window, cx| {
                let (completed, failure, removed_paths) = match result {
                    PermanentDeleteResult::Files(outcome) => {
                        let failure = (!outcome.failures.is_empty())
                            .then(|| summarize_delete_failures(&outcome.failures));
                        let removed = outcome.completed.clone();
                        (outcome.completed.len(), failure, removed)
                    }
                    PermanentDeleteResult::Trash { outcome, requested } => {
                        let failure = (!outcome.failures.is_empty())
                            .then(|| summarize_trash_failures(&outcome.failures));
                        let completed_set =
                            outcome.completed.iter().cloned().collect::<HashSet<_>>();
                        let removed = requested
                            .iter()
                            .filter(|record| completed_set.contains(record.original_path()))
                            .map(|record| record.backing_path().to_path_buf())
                            .collect();
                        (outcome.completed.len(), failure, removed)
                    }
                };
                if this.browsing_trash {
                    this.remove_trash_entries(removed_paths, cx);
                } else {
                    this.apply_directory_changes(
                        DirectoryChanges {
                            removed: removed_paths,
                            ..DirectoryChanges::default()
                        },
                        None,
                        cx,
                    );
                }
                if let Some(failure) = failure {
                    let message = if completed > 0 {
                        format!("Permanently deleted {completed} item(s); {failure}")
                    } else {
                        failure
                    };
                    window.push_notification(Notification::error(message), cx);
                } else {
                    let message = match kind {
                        OperationProgressKind::EmptyTrash => "Trash emptied".to_string(),
                        _ => format!("Permanently deleted {completed} item(s)"),
                    };
                    window.push_notification(Notification::success(message), cx);
                }
                this.operations.finish_active();
                cx.notify();
            });
        });
        self.operations.set_task(operation_task);
        cx.notify();
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
        let progress_task = cx.spawn(async move |this, cx| {
            loop {
                Timer::after(Duration::from_millis(80)).await;
                let keep_running = this
                    .update(cx, |this, cx| {
                        let keep_running = this.operations.progress().is_some();
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
        });
        self.operations.set_progress_task(progress_task);
    }

    fn cancel_active_operation(&mut self, cx: &mut Context<Self>) {
        if self.operations.request_cancel() {
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
        let Some((cancel, progress)) =
            self.operations
                .begin_transfer(mode, sources.len(), destination.clone())
        else {
            return;
        };
        self.entry_menu = None;
        self.start_operation_progress_refresh(cx);
        let change_sources = sources.clone();
        let task = cx.background_executor().spawn(smol::unblock(move || {
            transfer_paths_with_progress(&sources, &destination, mode, cancel, progress)
        }));

        let operation_task = cx.spawn_in(window, async move |this, window| {
            let outcome = task.await;
            let _ = this.update_in(window, |this, window, cx| {
                let changes = outcome.operation.as_ref().map_or_else(
                    || DirectoryChanges {
                        removed: if mode == TransferMode::Move {
                            change_sources
                                .iter()
                                .filter(|source| {
                                    outcome.completed.iter().any(|destination| {
                                        destination.file_name() == source.file_name()
                                    })
                                })
                                .cloned()
                                .collect()
                        } else {
                            Vec::new()
                        },
                        upserted: outcome.completed.clone(),
                    },
                    OperationRecord::forward_directory_changes,
                );
                if let Some(operation) = outcome.operation {
                    this.operations.record(operation);
                }

                if mode == TransferMode::Move
                    && !outcome.completed.is_empty()
                    && let Some(clipboard) = clipboard
                {
                    this.operations
                        .retain_uncompleted_move(clipboard, &outcome.completed);
                }

                let reveal = outcome
                    .completed
                    .iter()
                    .find(|path| path.parent() == Some(this.directory.current_dir.as_path()))
                    .cloned();
                this.apply_directory_changes(changes, reveal, cx);

                if outcome.failures.is_empty() {
                    let verb = match mode {
                        TransferMode::Copy => "Copied",
                        TransferMode::Move => "Moved",
                    };
                    let undo_note = if outcome.undo_unavailable {
                        " · too large to retain in undo history"
                    } else {
                        ""
                    };
                    window.push_notification(
                        Notification::success(format!(
                            "{verb} {} item(s){undo_note}",
                            outcome.completed.len(),
                        )),
                        cx,
                    );
                } else {
                    let mut message = summarize_failures(&outcome.failures);
                    if outcome.undo_unavailable {
                        message.push_str(
                            "; successful items were too large to retain in undo history",
                        );
                    }
                    window.push_notification(Notification::error(message), cx);
                }
                this.operations.finish_active();
                cx.notify();
            });
        });
        self.operations.set_task(operation_task);
        cx.notify();
    }

    fn apply_directory_changes(
        &mut self,
        changes: DirectoryChanges,
        reveal: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if self.browsing_trash {
            return;
        }

        let current_dir = self.directory.current_dir.clone();
        let removed = changes
            .removed
            .into_iter()
            .filter(|path| path.parent() == Some(current_dir.as_path()))
            .collect::<Vec<_>>();
        let upserted = changes
            .upserted
            .into_iter()
            .filter(|path| path.parent() == Some(current_dir.as_path()))
            .collect::<Vec<_>>();
        if removed.is_empty() && upserted.is_empty() {
            return;
        }

        let directory_generation = self.directory.generation;
        let task = cx
            .background_executor()
            .spawn(smol::unblock(move || revalidate_paths(upserted)));
        cx.spawn(async move |this, cx| {
            let mut events = task.await;
            events.extend(removed.into_iter().map(DirectoryEvent::Removed));
            let _ = this.update(cx, |this, cx| {
                if this.browsing_trash
                    || directory_generation != this.directory.generation
                    || current_dir != this.directory.current_dir
                {
                    return;
                }

                let affected = directory_event_paths(&events);
                this.directory.pending_reveal = reveal
                    .filter(|path| path.parent() == Some(this.directory.current_dir.as_path()));
                match this.directory.apply_events(events) {
                    ApplyDirectoryEvents::Applied(reconcile) => {
                        this.invalidate_watched_thumbnails(&affected);
                        this.apply_selection_reconcile(reconcile, cx);
                        this.select_pending_loaded_entry(cx);
                        // Unlike a streamed directory load, this is the only
                        // result batch. Do not retain an impossible reveal
                        // across later unrelated watcher events.
                        this.directory.pending_reveal = None;
                    }
                    ApplyDirectoryEvents::RescanRequired => {
                        this.start_directory_load(false, cx);
                        return;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn remove_trash_entries(&mut self, backing_paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if !self.browsing_trash || backing_paths.is_empty() {
            return;
        }
        for path in &backing_paths {
            self.trash_records.remove(path);
        }
        let events = backing_paths
            .iter()
            .cloned()
            .map(DirectoryEvent::Removed)
            .collect();
        match self.directory.apply_events(events) {
            ApplyDirectoryEvents::Applied(reconcile) => {
                self.invalidate_watched_thumbnails(&backing_paths);
                self.apply_selection_reconcile(reconcile, cx);
            }
            ApplyDirectoryEvents::RescanRequired => self.start_trash_load(false, cx),
        }
        cx.notify();
    }

    fn upsert_trash_entries(&mut self, records: Vec<TrashRecord>, cx: &mut Context<Self>) {
        if !self.browsing_trash || records.is_empty() {
            return;
        }
        let directory_generation = self.directory.generation;
        let task = cx.background_executor().spawn(smol::unblock(move || {
            let mut icons = crate::icons::IconProvider::discover();
            records
                .into_iter()
                .filter_map(|record| {
                    let mut entry = FileEntry::from_path(record.backing_path(), &mut icons).ok()?;
                    entry.name = record
                        .original_path()
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| entry.name.clone());
                    entry.navigable = false;
                    Some((entry, record))
                })
                .collect::<Vec<_>>()
        }));
        cx.spawn(async move |this, cx| {
            let entries = task.await;
            let _ = this.update(cx, |this, cx| {
                if !this.browsing_trash || directory_generation != this.directory.generation {
                    return;
                }
                let mut events = Vec::with_capacity(entries.len());
                for (entry, record) in entries {
                    this.trash_records.insert(entry.path.clone(), record);
                    events.push(DirectoryEvent::Changed(entry));
                }
                match this.directory.apply_events(events) {
                    ApplyDirectoryEvents::Applied(reconcile) => {
                        this.apply_selection_reconcile(reconcile, cx);
                    }
                    ApplyDirectoryEvents::RescanRequired => {
                        this.start_trash_load(false, cx);
                        return;
                    }
                }
                cx.notify();
            });
        })
        .detach();
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
        if self.operations.is_busy() && self.operations.request_cancel() {
            window.push_notification(Notification::info("Cancelling file operation…"), cx);
            return;
        }
        if self.directory.filter_query.is_empty() {
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

    fn on_trash_selection(
        &mut self,
        _: &TrashSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_browser_command(BrowserCommand::TrashSelection, window, cx);
    }

    fn on_restore_selection(
        &mut self,
        _: &RestoreSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_browser_command(BrowserCommand::RestoreSelection, window, cx);
    }

    fn on_delete_permanently(
        &mut self,
        _: &DeletePermanently,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_browser_command(BrowserCommand::DeletePermanently, window, cx);
    }

    fn on_empty_trash(&mut self, _: &EmptyTrash, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::EmptyTrash, window, cx);
    }

    fn on_new_folder(&mut self, _: &NewFolder, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::NewFolder, window, cx);
    }

    fn on_open_terminal(&mut self, _: &OpenTerminal, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_browser_command(BrowserCommand::OpenTerminal, window, cx);
    }

    fn on_rename_selection(
        &mut self,
        _: &RenameSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_browser_command(BrowserCommand::RenameSelection, window, cx);
    }

    fn on_compress_selection(
        &mut self,
        _: &CompressSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_browser_command(BrowserCommand::CompressSelection, window, cx);
    }

    fn on_extract_selection(
        &mut self,
        _: &ExtractSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_browser_command(BrowserCommand::ExtractSelection, window, cx);
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

        let ticket = self.directory.generation;
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
                            if ticket != this.directory.generation {
                                return false;
                            }
                            this.thumbnail_inflight.remove(&path);
                            this.thumbnail_pending.remove(&path);
                            if this.thumbnail_stale.remove(&path) {
                                cx.notify();
                                return true;
                            }
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
        self.directory.selection.clear();
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

    fn open_settings_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let palettes = Palette::ALL
            .iter()
            .map(|palette| SharedString::from(palette.label()))
            .collect::<Vec<_>>();
        let selected_index = Palette::ALL
            .iter()
            .position(|palette| *palette == self.palette)
            .unwrap_or_default();
        let theme_select = cx.new(|cx| {
            SelectState::new(
                palettes,
                Some(IndexPath::default().row(selected_index)),
                window,
                cx,
            )
        });
        let view = cx.entity();
        window
            .subscribe(
                &theme_select,
                cx,
                move |_, event: &SelectEvent<Vec<SharedString>>, _, cx| {
                    let SelectEvent::Confirm(selected) = event;
                    let Some(palette) = selected
                        .as_deref()
                        .and_then(|label| Palette::from_name(label.as_ref()))
                    else {
                        return;
                    };
                    view.update(cx, |this, cx| {
                        this.palette = palette;
                        theme::apply(palette, cx);
                        cx.notify();
                    });
                },
            )
            .detach();

        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title("Settings")
                .w(px(420.0))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(div().text_sm().child("Appearance"))
                        .child(
                            h_flex()
                                .w_full()
                                .justify_between()
                                .gap_4()
                                .child("Theme")
                                .child(Select::new(&theme_select).w(px(240.0))),
                        ),
                )
                .alert()
                .button_props(DialogButtonProps::default().ok_text("Done"))
        });
    }

    fn places_sidebar_width(&self, window: &Window, cx: &Context<Self>) -> Pixels {
        let font_size = cx.theme().font_size;
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
        self.directory.visible_entry(index)
    }

    fn visible_paths(&self) -> Vec<PathBuf> {
        self.directory.visible_paths()
    }

    fn apply_selection_reconcile(&mut self, reconcile: ReconcileSelection, cx: &mut Context<Self>) {
        match reconcile {
            ReconcileSelection::Unchanged => {}
            ReconcileSelection::Preview(entry) => self.start_preview(entry, cx),
            ReconcileSelection::ClearPreview => self.clear_preview(),
        }
    }

    fn set_filter_query(&mut self, query: String, cx: &mut Context<Self>) {
        let Some(reconcile) = self.directory.set_filter_query(query) else {
            return;
        };

        self.apply_selection_reconcile(reconcile, cx);
        self.directory_scroll = UniformListScrollHandle::new();
        self.entry_hit_bounds.borrow_mut().clear();
        self.entry_content_bounds.borrow_mut().clear();
        self.marquee = None;
        self.marquee_scroll_task.take();
        cx.notify();
    }

    fn set_show_hidden(&mut self, show_hidden: bool, cx: &mut Context<Self>) {
        let Some(reconcile) = self.directory.set_show_hidden(show_hidden) else {
            return;
        };

        self.apply_selection_reconcile(reconcile, cx);
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
                let cleared = query.is_empty() && !self.directory.filter_query.is_empty();
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

        if !self.directory.filter_query.is_empty() {
            match stroke.key.as_str() {
                "escape" if !browser_focused => {
                    self.clear_filter(window, cx);
                    cx.stop_propagation();
                    return;
                }
                "backspace" => {
                    let mut query = self.directory.filter_query.clone();
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
            || (self.directory.filter_query.is_empty() && text.chars().all(char::is_whitespace))
        {
            return;
        }

        let mut query = self.directory.filter_query.clone();
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
            .directory
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
            self.directory.selection.select_range(
                entry.path.clone(),
                &ordered,
                modifiers.secondary(),
            );
        } else if modifiers.secondary() {
            self.directory.selection.toggle(entry.path.clone());
        } else {
            self.directory.selection.select_only(entry.path.clone());
        }

        if self.directory.selection.primary() == Some(&entry.path) {
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
            .directory
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .cloned()
        else {
            return;
        };

        if self.directory.selection.is_selected(path) {
            self.directory.selection.make_primary(path);
        } else {
            self.directory.selection.select_only(path.to_path_buf());
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
                .any(|entry| entry.bounds.contains(&event.position))
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
        let open_terminal_enabled = self.command_enabled(BrowserCommand::OpenTerminal);
        let rename_enabled = self.command_enabled(BrowserCommand::RenameSelection);
        let compress_enabled = self.command_enabled(BrowserCommand::CompressSelection);
        let extract_enabled = self.command_enabled(BrowserCommand::ExtractSelection);
        let extract_visible = self.directory.selection.selected().len() == 1
            && self
                .directory
                .selection
                .primary()
                .and_then(|path| {
                    self.directory
                        .entries
                        .iter()
                        .find(|entry| &entry.path == path)
                })
                .is_some_and(|entry| {
                    entry.kind != EntryKind::Directory && is_supported_archive(&entry.path)
                });
        let copy_enabled = self.command_enabled(BrowserCommand::CopySelection);
        let cut_enabled = self.command_enabled(BrowserCommand::CutSelection);
        let paste_enabled = self.command_enabled(BrowserCommand::PasteFiles);
        let undo_enabled = self.command_enabled(BrowserCommand::UndoFileOperation);
        let redo_enabled = self.command_enabled(BrowserCommand::RedoFileOperation);
        let trash_menu_command = if self.browsing_trash {
            BrowserCommand::RestoreSelection
        } else {
            BrowserCommand::TrashSelection
        };
        let trash_menu_enabled = self.command_enabled(trash_menu_command);
        let permanent_delete_enabled = self.command_enabled(BrowserCommand::DeletePermanently);
        let empty_trash_enabled = self.command_enabled(BrowserCommand::EmptyTrash);
        let window_size = window.bounds().size;
        let menu_height = match menu.target {
            ContextMenuTarget::Entry => {
                ENTRY_MENU_HEIGHT + if extract_visible { 28.0 } else { 0.0 }
            }
            ContextMenuTarget::CurrentDirectory if self.browsing_trash => {
                DIRECTORY_MENU_HEIGHT + 36.0
            }
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
                .text_color(colors.muted_foreground)
                .child(format!("– {label}"))
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
                    .text_sm()
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
                    .child(planned("New File"))
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
                            .child("Paste")
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
                            .child("Undo")
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
                            .child("Redo")
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
                                this.set_show_hidden(!this.directory.show_hidden, cx);
                            }))
                            .child("Show Hidden Files")
                            .child(div().flex_1())
                            .when(self.directory.show_hidden, |this| this.child("✓")),
                    )
                    .child(separator())
                    .child(
                        h_flex()
                            .id("directory-menu-open-terminal")
                            .h(px(28.0))
                            .px_3()
                            .rounded(radius)
                            .when(open_terminal_enabled, |this| {
                                this.cursor_pointer()
                                    .hover(|this| this.bg(colors.accent))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.entry_menu = None;
                                        this.execute_browser_command(
                                            BrowserCommand::OpenTerminal,
                                            window,
                                            cx,
                                        );
                                    }))
                            })
                            .when(!open_terminal_enabled, |this| {
                                this.text_color(colors.muted_foreground)
                            })
                            .child("Open in Terminal"),
                    )
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
                                    this.directory.current_dir.display().to_string(),
                                ));
                                this.dismiss_entry_menu(cx);
                            }))
                            .child("Copy Location"),
                    )
                    .child(planned("Properties"))
                    .when(self.browsing_trash, |this| {
                        this.child(separator()).child(
                            h_flex()
                                .id("directory-menu-empty-trash")
                                .h(px(28.0))
                                .px_3()
                                .rounded(radius)
                                .when(empty_trash_enabled, |this| {
                                    this.cursor_pointer()
                                        .hover(|this| this.bg(colors.accent))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.entry_menu = None;
                                            this.execute_browser_command(
                                                BrowserCommand::EmptyTrash,
                                                window,
                                                cx,
                                            );
                                        }))
                                })
                                .when(!empty_trash_enabled, |this| {
                                    this.text_color(colors.muted_foreground)
                                })
                                .child("Empty Trash…"),
                        )
                    })
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
                .text_sm()
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
                        .child("Cut")
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
                        .child("Copy")
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
                        .child("Paste")
                        .child(div().flex_1())
                        .child(
                            div()
                                .text_xs()
                                .text_color(colors.muted_foreground)
                                .child("Ctrl+V"),
                        ),
                )
                .child(planned("Duplicate"))
                .child(separator())
                .child(
                    h_flex()
                        .id("entry-menu-rename")
                        .h(px(28.0))
                        .px_3()
                        .rounded(radius)
                        .when(rename_enabled, |this| {
                            this.cursor_pointer()
                                .hover(|this| this.bg(colors.accent))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.entry_menu = None;
                                    this.execute_browser_command(
                                        BrowserCommand::RenameSelection,
                                        window,
                                        cx,
                                    );
                                }))
                        })
                        .when(!rename_enabled, |this| {
                            this.text_color(colors.muted_foreground)
                        })
                        .child("Rename…")
                        .child(div().flex_1())
                        .child(
                            div()
                                .text_xs()
                                .text_color(colors.muted_foreground)
                                .child("F2"),
                        ),
                )
                .child(planned("Move To…"))
                .child(
                    h_flex()
                        .id("entry-menu-trash-or-restore")
                        .h(px(28.0))
                        .px_3()
                        .rounded(radius)
                        .when(trash_menu_enabled, |this| {
                            this.cursor_pointer()
                                .hover(|this| this.bg(colors.accent))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.entry_menu = None;
                                    this.execute_browser_command(trash_menu_command, window, cx);
                                }))
                        })
                        .when(!trash_menu_enabled, |this| {
                            this.text_color(colors.muted_foreground)
                        })
                        .child(if self.browsing_trash {
                            "Restore"
                        } else {
                            "Move to Trash"
                        })
                        .child(div().flex_1())
                        .when(!self.browsing_trash, |this| {
                            this.child(
                                div()
                                    .text_xs()
                                    .text_color(colors.muted_foreground)
                                    .child("Delete"),
                            )
                        }),
                )
                .child(
                    h_flex()
                        .id("entry-menu-delete-permanently")
                        .h(px(28.0))
                        .px_3()
                        .rounded(radius)
                        .when(permanent_delete_enabled, |this| {
                            this.cursor_pointer()
                                .hover(|this| this.bg(colors.accent))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.entry_menu = None;
                                    this.execute_browser_command(
                                        BrowserCommand::DeletePermanently,
                                        window,
                                        cx,
                                    );
                                }))
                        })
                        .when(!permanent_delete_enabled, |this| {
                            this.text_color(colors.muted_foreground)
                        })
                        .child("Delete")
                        .child(div().flex_1())
                        .child(
                            div()
                                .text_xs()
                                .text_color(colors.muted_foreground)
                                .child("Shift+Delete"),
                        ),
                )
                .child(separator())
                .child(planned("Create Link"))
                .child(
                    h_flex()
                        .id("entry-menu-compress")
                        .h(px(28.0))
                        .px_3()
                        .rounded(radius)
                        .when(compress_enabled, |this| {
                            this.cursor_pointer()
                                .hover(|this| this.bg(colors.accent))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.entry_menu = None;
                                    this.execute_browser_command(
                                        BrowserCommand::CompressSelection,
                                        window,
                                        cx,
                                    );
                                }))
                        })
                        .when(!compress_enabled, |this| {
                            this.text_color(colors.muted_foreground)
                        })
                        .child("Compress…"),
                )
                .when(extract_visible, |this| {
                    this.child(
                        h_flex()
                            .id("entry-menu-extract")
                            .h(px(28.0))
                            .px_3()
                            .rounded(radius)
                            .when(extract_enabled, |this| {
                                this.cursor_pointer()
                                    .hover(|this| this.bg(colors.accent))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.entry_menu = None;
                                        this.execute_browser_command(
                                            BrowserCommand::ExtractSelection,
                                            window,
                                            cx,
                                        );
                                    }))
                            })
                            .when(!extract_enabled, |this| {
                                this.text_color(colors.muted_foreground)
                            })
                            .child("Extract"),
                    )
                })
                .child(planned("Copy Path"))
                .child(separator())
                .child(planned("Properties"))
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
                .any(|entry| entry.bounds.contains(&event.position))
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
        let base_selection = self.directory.selection.selected().clone();
        if !additive {
            self.directory.selection.clear();
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

        self.directory
            .selection
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
            .directory
            .selection
            .primary()
            .and_then(|path| {
                self.directory
                    .entries
                    .iter()
                    .find(|entry| &entry.path == path)
            })
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
                        if this.directory.selection.primary() == Some(&path) {
                            this.preview_state = PreviewState::Error(error.to_string());
                            cx.notify();
                        }
                    });
                }
            })
            .detach();
        }
    }

    fn open_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(target_os = "linux")]
        {
            let directory = self.directory.current_dir.clone();
            let task = cx
                .background_executor()
                .spawn(crate::system_terminal::open_terminal(directory));
            cx.spawn_in(window, async move |this, window| {
                if let Err(error) = task.await {
                    let _ = this.update_in(window, |_, window, cx| {
                        window.push_notification(Notification::error(error.to_string()), cx);
                    });
                }
            })
            .detach();
        }

        #[cfg(not(target_os = "linux"))]
        window.push_notification(
            Notification::error("Open in Terminal is currently available only on Linux"),
            cx,
        );
    }

    fn render_place(&self, index: usize, place: Place, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let radius = cx.theme().radius;
        let is_trash = place.is_trash();
        let active = if is_trash {
            self.browsing_trash
        } else {
            !self.browsing_trash && self.directory.current_dir == place.path
        };
        let operation_busy = self.operations.is_busy();
        let drop_path = place.path.clone();
        let navigate_path = place.path.clone();
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
                !is_trash
                    && !operation_busy
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
                if !is_trash {
                    this.start_drag_move(drag.paths.to_vec(), drop_path.clone(), window, cx);
                }
            }))
            .child(icon)
            .child(div().flex_none().text_base().child(place.label))
            .when(!is_trash, |this| {
                this.child(
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
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                if is_trash {
                    this.start_trash_load(true, cx);
                } else {
                    this.navigate_to(navigate_path.clone(), true, cx);
                }
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
        let active = self.directory.current_dir == bookmark.path;
        let operation_busy = self.operations.is_busy();
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
                            .text_base()
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
            self.directory.visible_entries.len(),
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
                        let selected = this.directory.selection.is_selected(&path);
                        let navigable = entry.navigable;
                        let rename_input = (this.rename_path.as_ref() == Some(&path))
                            .then(|| this.rename_input.clone())
                            .flatten();
                        let drag = if selected {
                            selected_drag
                                .clone()
                                .unwrap_or_else(|| Self::single_file_drag(&path, navigable))
                        } else {
                            Self::single_file_drag(&path, navigable)
                        };
                        let operation_busy = this.operations.is_busy();
                        let dragging_enabled = !this.browsing_trash && rename_input.is_none();
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
                        let name = if let Some(input) = rename_input {
                            div()
                                .w(px(300.0))
                                .child(Input::new(&input).small())
                                .into_any_element()
                        } else {
                            div()
                                .max_w(px(480.0))
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(entry.name)
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
                            .when(dragging_enabled, |this| {
                                this.on_drag(drag, |drag, _, _, cx| {
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
                            .child(name)
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(colors.muted_foreground)
                                    .child(format_size(entry.size)),
                            )
                            .child(
                                canvas(
                                    move |bounds, _, _| {
                                        entry_hit_bounds.borrow_mut().insert(
                                            bounds_path.clone(),
                                            EntryHitRegion { bounds, navigable },
                                        );
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
        let row_count = self.directory.visible_entries.len().div_ceil(columns);

        uniform_list(
            "directory-grid-rows",
            row_count,
            cx.processor(move |this, rows: Range<usize>, _window, cx| {
                let thumbnail_start = rows.start.saturating_sub(1) * columns;
                let thumbnail_end =
                    ((rows.end + 1) * columns).min(this.directory.visible_entries.len());
                let visible_start = rows.start * columns;
                let visible_end = (rows.end * columns).min(this.directory.visible_entries.len());
                this.ensure_thumbnails(
                    visible_start..visible_end,
                    thumbnail_start..thumbnail_end,
                    cx,
                );

                rows.map(|row| {
                    let start = row * columns;
                    let end = (start + columns).min(this.directory.visible_entries.len());
                    let tiles = (start..end)
                        .filter_map(|index| {
                            let entry = this.visible_entry(index)?.clone();
                            let path = entry.path.clone();
                            let click_path = path.clone();
                            let context_path = path.clone();
                            let bounds_path = path.clone();
                            let drop_path = path.clone();
                            let can_drop_path = path.clone();
                            let selected = this.directory.selection.is_selected(&path);
                            let navigable = entry.navigable;
                            let rename_input = (this.rename_path.as_ref() == Some(&path))
                                .then(|| this.rename_input.clone())
                                .flatten();
                            let drag = if selected {
                                selected_drag
                                    .clone()
                                    .unwrap_or_else(|| Self::single_file_drag(&path, navigable))
                            } else {
                                Self::single_file_drag(&path, navigable)
                            };
                            let operation_busy = this.operations.is_busy();
                            let dragging_enabled = !this.browsing_trash && rename_input.is_none();
                            let display_name = elide_filename(&entry.name, GRID_LABEL_COLUMNS);
                            let entry_hit_bounds = this.entry_hit_bounds.clone();
                            let entry_content_bounds = this.entry_content_bounds.clone();
                            let browser_bounds = this.browser_bounds.clone();
                            let directory_scroll = this.directory_scroll.clone();

                            let visual = match this.thumbnails.get(&path) {
                                Some(ThumbnailState::Ready(thumbnail)) => div()
                                    .flex()
                                    .flex_none()
                                    .size(px(GRID_VISUAL_SIZE))
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
                                            .size(px(GRID_VISUAL_SIZE))
                                            .items_center()
                                            .justify_center()
                                            .child(
                                                img(icon_path)
                                                    .size(px(GRID_ICON_SIZE))
                                                    .object_fit(ObjectFit::Contain),
                                            )
                                            .into_any_element()
                                    } else {
                                        div()
                                            .flex()
                                            .flex_none()
                                            .size(px(GRID_VISUAL_SIZE))
                                            .items_center()
                                            .justify_center()
                                            .text_3xl()
                                            .text_color(colors.primary)
                                            .child(entry.icon())
                                            .into_any_element()
                                    }
                                }
                            };
                            let name = if let Some(input) = rename_input {
                                div()
                                    .w_full()
                                    .h(px(GRID_LABEL_HEIGHT))
                                    .flex_none()
                                    .child(Input::new(&input).small())
                                    .into_any_element()
                            } else {
                                div()
                                    .w_full()
                                    .max_w(px(GRID_TILE_WIDTH - 16.0))
                                    .h(px(GRID_LABEL_HEIGHT))
                                    .flex_none()
                                    .overflow_hidden()
                                    .whitespace_normal()
                                    .line_clamp(3)
                                    .line_height(relative(1.2))
                                    .text_center()
                                    .text_base()
                                    .child(display_name)
                                    .into_any_element()
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
                                    .when(dragging_enabled, |this| {
                                        this.on_drag(drag, |drag, _, _, cx| {
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
                                    .child(name)
                                    .child(
                                        canvas(
                                            move |bounds, _, _| {
                                                entry_hit_bounds.borrow_mut().insert(
                                                    bounds_path.clone(),
                                                    EntryHitRegion { bounds, navigable },
                                                );
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
        let empty_message = self.directory.visible_entries.is_empty().then(|| {
            if let Some(error) = &self.directory.error {
                format!("Could not read this folder\n{error}")
            } else if self.directory.loading && self.directory.entries.is_empty() {
                "Loading folder…".to_string()
            } else if !self.directory.filter_query.is_empty() {
                format!("No matches for “{}”", self.directory.filter_query)
            } else if !self.directory.show_hidden && !self.directory.entries.is_empty() {
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
            .when(self.directory.loading, |this| {
                this.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(colors.muted_foreground)
                        .child(if self.directory.filter_query.is_empty() {
                            format!("Loading… {} items", self.directory.entries.len())
                        } else {
                            format!(
                                "Loading… {} of {} items match",
                                self.directory.visible_entries.len(),
                                self.directory.entries.len()
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
        if self.search_input.read(cx).value().as_ref() != self.directory.filter_query {
            self.search_input.update(cx, |input, cx| {
                input.set_value(self.directory.filter_query.clone(), window, cx);
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
            .checked(self.directory.show_hidden)
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
        // gpui-component's icon-only Button currently loses its SVG tint on
        // Marcel's dark sidebar, as do the title-bar navigation glyphs. Keep
        // this one tiny trigger Marcel-owned until the component preserves the
        // semantic foreground supplied by the active theme.
        let settings_button = div()
            .id("open-settings")
            .flex()
            .flex_none()
            .size(px(28.0))
            .items_center()
            .justify_center()
            .rounded(cx.theme().radius)
            .font_family(cx.theme().mono_font_family.clone())
            .line_height(relative(1.0))
            .text_lg()
            .text_color(colors.sidebar_foreground)
            .cursor_pointer()
            .hover(|button| button.bg(colors.sidebar_accent))
            .on_click(cx.listener(|this, _, window, cx| {
                this.open_settings_dialog(window, cx);
            }))
            .child("⚙");

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
                        .child(
                            h_flex()
                                .w_full()
                                .justify_between()
                                .child(view_switch)
                                .child(settings_button),
                        ),
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
            .on_action(cx.listener(Self::on_trash_selection))
            .on_action(cx.listener(Self::on_restore_selection))
            .on_action(cx.listener(Self::on_delete_permanently))
            .on_action(cx.listener(Self::on_empty_trash))
            .on_action(cx.listener(Self::on_new_folder))
            .on_action(cx.listener(Self::on_open_terminal))
            .on_action(cx.listener(Self::on_rename_selection))
            .on_action(cx.listener(Self::on_compress_selection))
            .on_action(cx.listener(Self::on_extract_selection))
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
                            .text_color(
                                if !self.browsing_trash
                                    && self.directory.current_dir.parent().is_some()
                                {
                                    colors.sidebar_foreground
                                } else {
                                    colors.muted_foreground
                                },
                            )
                            .child("↑")
                            .when(
                                !self.browsing_trash
                                    && self.directory.current_dir.parent().is_some(),
                                |button| {
                                    button
                                        .cursor_pointer()
                                        .hover(|button| button.bg(colors.sidebar_accent))
                                        .on_click(cx.listener(|this, _, _, cx| this.go_up(cx)))
                                },
                            ),
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
                            .child(if self.browsing_trash {
                                "Trash".to_string()
                            } else {
                                self.directory.current_dir.display().to_string()
                            }),
                    )
                    .child(
                        Input::new(&self.search_input)
                            .small()
                            .cleanable(true)
                            .h_7()
                            .w(px(240.0)),
                    ),
            );

        let selected_entry = self.directory.selection.primary().and_then(|path| {
            self.directory
                .entries
                .iter()
                .find(|entry| &entry.path == path)
        });
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

fn rename_stem_end(name: &str, is_directory: bool) -> usize {
    if is_directory {
        return name.chars().count();
    }
    name.char_indices()
        .rev()
        .find_map(|(index, character)| {
            (character == '.' && index > 0).then(|| name[..index].chars().count())
        })
        .unwrap_or_else(|| name.chars().count())
}

fn operation_history_message(operation: &OperationRecord, redo: bool) -> String {
    let name = operation
        .path()
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    match (operation, redo) {
        (OperationRecord::CreateDirectory { .. }, false) => {
            format!("Undid creation of “{name}”")
        }
        (OperationRecord::CreateDirectory { .. }, true) => format!("Recreated folder “{name}”"),
        (OperationRecord::Copy { .. }, false) => "Undid copy".to_string(),
        (OperationRecord::Copy { .. }, true) => "Repeated copy".to_string(),
        (OperationRecord::Move { .. }, false) => "Undid move".to_string(),
        (OperationRecord::Move { .. }, true) => "Repeated move".to_string(),
        (OperationRecord::Trash { .. }, false) => "Restored item(s) from Trash".to_string(),
        (OperationRecord::Trash { .. }, true) => "Moved item(s) to Trash again".to_string(),
        (OperationRecord::Restore { .. }, false) => {
            "Moved restored item(s) back to Trash".to_string()
        }
        (OperationRecord::Restore { .. }, true) => "Restored item(s) again".to_string(),
        (OperationRecord::Rename { source, .. }, false) => {
            let original = source
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default();
            format!("Restored name “{original}”")
        }
        (OperationRecord::Rename { destination, .. }, true) => {
            let renamed = destination
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default();
            format!("Renamed to “{renamed}” again")
        }
        (OperationRecord::ArchiveCreate { .. }, false) => {
            format!("Removed created ZIP “{name}”")
        }
        (OperationRecord::ArchiveCreate { .. }, true) => {
            format!("Created ZIP “{name}” again")
        }
        (OperationRecord::ArchiveExtract { .. }, false) => {
            format!("Removed extracted item “{name}”")
        }
        (OperationRecord::ArchiveExtract { .. }, true) => {
            format!("Extracted “{name}” again")
        }
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

fn can_drop_on_entry_hit(
    hit: &EntryHitRegion,
    pointer: Point<Pixels>,
    sources: &[PathBuf],
    destination: &Path,
    operation_busy: bool,
) -> bool {
    !operation_busy
        && hit.navigable
        && hit.bounds.contains(&pointer)
        && can_move_files_to(sources, destination)
}

fn directory_event_paths(events: &[DirectoryEvent]) -> Vec<PathBuf> {
    let mut paths = HashSet::with_capacity(events.len());
    for event in events {
        match event {
            DirectoryEvent::Added(entry) | DirectoryEvent::Changed(entry) => {
                paths.insert(entry.path.clone());
            }
            DirectoryEvent::Removed(path) => {
                paths.insert(path.clone());
            }
            DirectoryEvent::Renamed { from, entry } => {
                paths.insert(from.clone());
                paths.insert(entry.path.clone());
            }
            DirectoryEvent::RescanRequired => {}
        }
    }
    paths.into_iter().collect()
}

fn grid_column_count(viewport_width: f32) -> usize {
    let available = (viewport_width - GRID_SIDE_PADDING * 2.0).max(GRID_TILE_WIDTH);
    ((available + GRID_GAP) / (GRID_TILE_WIDTH + GRID_GAP))
        .floor()
        .max(1.0) as usize
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
    use crate::directory_session::{fuzzy_score, is_hidden_name};

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
    fn browser_drop_hit_uses_painted_entry_metadata() {
        let pointer = Point::new(px(50.0), px(50.0));
        let bounds = Bounds::from_corners(
            Point::new(px(0.0), px(0.0)),
            Point::new(px(100.0), px(100.0)),
        );
        let sources = [PathBuf::from("/work/report.txt")];
        let destination = Path::new("/archive");

        assert!(can_drop_on_entry_hit(
            &EntryHitRegion {
                bounds,
                navigable: true,
            },
            pointer,
            &sources,
            destination,
            false,
        ));
        assert!(!can_drop_on_entry_hit(
            &EntryHitRegion {
                bounds,
                navigable: false,
            },
            pointer,
            &sources,
            destination,
            false,
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
    fn rename_selection_preserves_a_file_extension_but_selects_directory_dots() {
        assert_eq!(rename_stem_end("report.final.txt", false), 12);
        assert_eq!(rename_stem_end(".bashrc", false), 7);
        assert_eq!(rename_stem_end("folder.with.dots", true), 16);
        assert_eq!(rename_stem_end("猫.txt", false), 1);
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
