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
    AnyElement, Bounds, ClickEvent, Context, Hsla, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ObjectFit, ParentElement, Pixels, Point, Render, Styled, Task,
    Timer, UniformListScrollHandle, Window, canvas, div, img, prelude::*, px, uniform_list,
};
use gpui_component::{
    ActiveTheme, Disableable, Sizable,
    button::{Button, ButtonVariants},
    h_flex,
    resizable::{h_resizable, resizable_panel},
    scroll::ScrollableElement,
    text::TextView,
};
use unicode_width::UnicodeWidthChar;

use crate::{
    fs::{DirectoryUpdate, FileEntry, format_size, merge_sorted_entries, stream_directory},
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
const GRID_LABEL_HEIGHT: f32 = 48.0;
const GRID_ROW_HEIGHT: f32 = GRID_TILE_HEIGHT + GRID_GAP;
const MAX_MEMORY_THUMBNAILS: usize = 512;
const THUMBNAIL_WORKERS: usize = 2;
const PDF_PAGE_WORKERS: usize = 2;
const PDF_PAGE_LOOKAHEAD: usize = 1;
const DEFAULT_PREVIEW_WIDTH: f32 = 420.0;
const MIN_SIDEBAR_WIDTH: f32 = 160.0;
const MAX_SIDEBAR_WIDTH: f32 = 420.0;
const MIN_BROWSER_WIDTH: f32 = 360.0;
const MIN_PREVIEW_WIDTH: f32 = 280.0;
const MAX_PREVIEW_WIDTH: f32 = 900.0;
const PREVIEW_TEXT_CHROME_WIDTH: f32 = 92.0;
const PREVIEW_MONO_CELL_WIDTH: f32 = 8.0;
const PREVIEW_WRAP_DEBOUNCE: Duration = Duration::from_millis(80);
const MARQUEE_THRESHOLD: f32 = 4.0;
const MARQUEE_EDGE_ZONE: f32 = 36.0;
const MARQUEE_MAX_SCROLL_STEP: f32 = 18.0;
const MARQUEE_SCROLL_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ViewMode {
    #[default]
    List,
    Grid,
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
    current_dir: PathBuf,
    entries: Vec<FileEntry>,
    selection: SelectionModel,
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
    history: NavigationHistory,
    places: Vec<Place>,
    places_loading: bool,
    places_task: Option<Task<()>>,
}

impl Marcel {
    pub fn new(start_dir: PathBuf, cx: &mut Context<Self>) -> Self {
        let start_dir = normalize_start_directory(start_dir);
        let home_dir = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| start_dir.clone());
        let (thumbnail_wake_sender, thumbnail_wake_receiver) =
            async_channel::bounded(THUMBNAIL_WORKERS);
        let (pdf_page_wake_sender, pdf_page_wake_receiver) =
            async_channel::bounded(PDF_PAGE_WORKERS);

        let mut this = Self {
            current_dir: start_dir.clone(),
            entries: Vec::new(),
            selection: SelectionModel::default(),
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
            history: NavigationHistory::new(start_dir),
            places: vec![Place::home(home_dir.clone())],
            places_loading: true,
            places_task: None,
        };
        this.start_places_load(home_dir, cx);
        this.start_directory_load(cx);
        this
    }

    fn start_places_load(&mut self, home: PathBuf, cx: &mut Context<Self>) {
        let load_task = cx
            .background_executor()
            .spawn(smol::unblock(move || discover_places(&home)));

        self.places_task = Some(cx.spawn(async move |this, cx| {
            let places = load_task.await;
            let _ = this.update(cx, |this, cx| {
                this.places = places;
                this.places_loading = false;
                cx.notify();
            });
        }));
    }

    fn start_directory_load(&mut self, cx: &mut Context<Self>) {
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
        self.start_directory_load(cx);
    }

    fn go_back(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.history.go_back() {
            self.current_dir = path;
            self.start_directory_load(cx);
        }
    }

    fn go_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.history.go_forward() {
            self.current_dir = path;
            self.start_directory_load(cx);
        }
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        if let Some(parent) = self.current_dir.parent() {
            self.navigate_to(parent.to_path_buf(), true, cx);
        }
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
            .filter_map(|index| self.entries.get(index))
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
        self.reset_pdf_preview();
        self.preview_wrap_task.take();
        self.preview_resize_task.take();
        self.preview_wrap = None;
        self.preview_text_scroll = UniformListScrollHandle::new();
        self.preview_state = PreviewState::Empty;
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
        ((width - PREVIEW_TEXT_CHROME_WIDTH) / PREVIEW_MONO_CELL_WIDTH)
            .floor()
            .max(16.0) as usize
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
            let ordered = self
                .entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>();
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
            let open_task = cx
                .background_executor()
                .spawn(crate::system_open::open_file(path.clone()));
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

        #[cfg(not(target_os = "linux"))]
        cx.open_with_system(&entry.path);
    }

    fn render_place(&self, index: usize, place: Place, cx: &mut Context<Self>) -> Button {
        Button::new(("place", index))
            .ghost()
            .label(place.label)
            .w_full()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.navigate_to(place.path.clone(), true, cx);
            }))
    }

    fn render_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        uniform_list(
            "directory-entries",
            self.entries.len(),
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                range
                    .filter_map(|index| {
                        let entry = this.entries.get(index)?.clone();
                        let path = entry.path.clone();
                        let click_path = path.clone();
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
                            .rounded_md()
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
        let columns = self.grid_columns();
        if columns != self.grid_layout_columns {
            self.grid_layout_columns = columns;
            self.entry_content_bounds.borrow_mut().clear();
        }
        let row_count = self.entries.len().div_ceil(columns);

        uniform_list(
            "directory-grid-rows",
            row_count,
            cx.processor(move |this, rows: Range<usize>, _window, cx| {
                let thumbnail_start = rows.start.saturating_sub(1) * columns;
                let thumbnail_end = ((rows.end + 1) * columns).min(this.entries.len());
                let visible_start = rows.start * columns;
                let visible_end = (rows.end * columns).min(this.entries.len());
                this.ensure_thumbnails(
                    visible_start..visible_end,
                    thumbnail_start..thumbnail_end,
                    cx,
                );

                rows.map(|row| {
                    let start = row * columns;
                    let end = (start + columns).min(this.entries.len());
                    let tiles = (start..end)
                        .filter_map(|index| {
                            let entry = this.entries.get(index)?.clone();
                            let path = entry.path.clone();
                            let click_path = path.clone();
                            let bounds_path = path.clone();
                            let selected = this.selection.is_selected(&path);
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
                                    .rounded_md()
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
                                    .rounded_md()
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
                                    .child(visual)
                                    .child(
                                        div()
                                            .w_full()
                                            .max_w(px(GRID_TILE_WIDTH - 16.0))
                                            .h(px(GRID_LABEL_HEIGHT))
                                            .flex_none()
                                            .overflow_hidden()
                                            .whitespace_normal()
                                            .line_clamp(3)
                                            .text_center()
                                            .text_xs()
                                            .child(entry.name),
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
        if self.entries.is_empty() {
            let message = if let Some(error) = &self.directory_error {
                format!("Could not read this folder\n{error}")
            } else if self.directory_loading {
                "Loading folder…".to_string()
            } else {
                "This folder is empty".to_string()
            };

            return div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_color(colors.muted_foreground)
                .child(message)
                .into_any_element();
        }

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
        let contents = match self.view_mode {
            ViewMode::List => self.render_list(cx),
            ViewMode::Grid => self.render_grid(cx),
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
                        .child(format!("Loading… {} items", self.entries.len())),
                )
            })
            .vertical_scrollbar(&directory_scroll)
            .into_any_element()
    }

    fn render_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
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
                                                .h(px(20.0))
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
        let colors = cx.theme().colors;
        let window_width = f32::from(window.bounds().size.width);
        let sidebar_default = px(window_width * 2.0 / 12.0);
        let browser_default = px(window_width * 6.0 / 12.0);
        let preview_default = px(window_width * 4.0 / 12.0);
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

        let sidebar = div()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .p_4()
            .gap_2()
            .bg(colors.sidebar)
            .border_r_1()
            .border_color(colors.sidebar_border)
            .text_color(colors.sidebar_foreground)
            .child(div().text_lg().child("Marcel"))
            .child(
                div()
                    .pt_3()
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
            });

        let browser = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .p_4()
            .gap_3()
            .bg(colors.background)
            .border_r_1()
            .border_color(colors.border)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("back")
                            .ghost()
                            .small()
                            .label("Back")
                            .disabled(!self.history.can_go_back())
                            .on_click(cx.listener(|this, _, _, cx| this.go_back(cx))),
                    )
                    .child(
                        Button::new("forward")
                            .ghost()
                            .small()
                            .label("Forward")
                            .disabled(!self.history.can_go_forward())
                            .on_click(cx.listener(|this, _, _, cx| this.go_forward(cx))),
                    )
                    .child(
                        Button::new("up")
                            .ghost()
                            .small()
                            .label("Up")
                            .disabled(self.current_dir.parent().is_none())
                            .on_click(cx.listener(|this, _, _, cx| this.go_up(cx))),
                    )
                    .child(
                        Button::new("refresh")
                            .ghost()
                            .small()
                            .label("Refresh")
                            .on_click(cx.listener(|this, _, _, cx| this.start_directory_load(cx))),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("list-view")
                            .small()
                            .label("List")
                            .when(self.view_mode == ViewMode::List, |button| button.primary())
                            .when(self.view_mode != ViewMode::List, |button| button.ghost())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_view_mode(ViewMode::List, cx);
                            })),
                    )
                    .child(
                        Button::new("grid-view")
                            .small()
                            .label("Icons")
                            .when(self.view_mode == ViewMode::Grid, |button| button.primary())
                            .when(self.view_mode != ViewMode::Grid, |button| button.ghost())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_view_mode(ViewMode::Grid, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .px_2()
                    .text_sm()
                    .text_color(colors.muted_foreground)
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(self.current_dir.display().to_string()),
            )
            .child(self.render_browser(cx));

        let selected_entry = self
            .selection
            .primary()
            .and_then(|path| self.entries.iter().find(|entry| &entry.path == path));
        let preview_details = selected_entry.map(|entry| {
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
        let has_preview_footer =
            preview_details.is_some() || image_mime.is_some() || truncated || clipped_lines;
        let preview_footer = div()
            .flex()
            .flex_col()
            .gap_1()
            .px_4()
            .py_3()
            .when_some(preview_details, |this, details| {
                this.child(
                    div()
                        .text_sm()
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
        div()
            .flex()
            .size_full()
            .bg(colors.background)
            .text_color(colors.foreground)
            .child(
                h_resizable("main-panes")
                    .on_resize(move |_, _, cx| {
                        pane_view.update(cx, |this, cx| {
                            this.start_preview_wrap(cx);
                            cx.notify();
                        });
                    })
                    .child(
                        resizable_panel()
                            .size(sidebar_default)
                            .size_range(px(MIN_SIDEBAR_WIDTH)..px(MAX_SIDEBAR_WIDTH))
                            .child(sidebar),
                    )
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
            )
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
