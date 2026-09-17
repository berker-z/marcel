//! The preview pane: what it is showing, the background work that produces
//! it, and how it is drawn.

use std::{
    cell::Cell,
    collections::{HashMap, HashSet, VecDeque},
    hash::Hash,
    ops::Range,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gpui::prelude::*;
use gpui::{
    AnyElement, App, ClickEvent, Context, Hsla, Img, IntoElement, ObjectFit, Pixels, Stateful,
    Task, TextRun, UniformListScrollHandle, Window, div, font, img, px, uniform_list,
};
use gpui_component::{ActiveTheme as _, h_flex, scroll::ScrollableElement as _, text::TextView};
use unicode_width::UnicodeWidthChar;

use crate::{
    browse::entries::{
        DirectoryUpdate, FileEntry, format_size, merge_sorted_entries, stream_directory,
    },
    preview::{
        Preview, PreviewState as PreviewContent, load_preview, pdf::render_pdf_page, thumbnails,
    },
};

use super::{
    DIRECTORY_ROW_HEIGHT, Marcel,
    navigation::{pump, unblock},
};

const MAX_MEMORY_THUMBNAILS: usize = 512;
const THUMBNAIL_WORKERS: usize = 2;
const PDF_PAGE_WORKERS: usize = 2;
const PDF_PAGE_LOOKAHEAD: usize = 1;
const DEFAULT_PREVIEW_WIDTH: f32 = 420.0;
const PREVIEW_TEXT_CHROME_WIDTH: f32 = 92.0;
const PREVIEW_WRAP_DEBOUNCE: Duration = Duration::from_millis(80);

#[derive(Clone, Debug)]
pub enum ThumbnailState {
    Ready(PathBuf),
    Failed,
}

#[derive(Clone, Debug)]
pub enum PdfPageState {
    Ready(PathBuf),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrappedPreviewLine {
    pub source_line: Option<usize>,
    pub contents: String,
}

#[derive(Clone)]
pub struct WrappedPreview {
    pub ticket: u64,
    pub columns: usize,
    pub lines: Arc<[WrappedPreviewLine]>,
}

/// What one worker runs off the foreground for one key.
type Job<R> = Box<dyn FnOnce() -> R + Send>;

/// A bounded pool of decode workers fed from a priority queue that is rebuilt
/// whenever the viewport moves.
///
/// Adapted from Yazi's paged preloading and superseding scheduler: running
/// decodes may finish, but old queued work cannot sit ahead of newly visible
/// items. Two workers is Yazi's default; it lets decoding overlap without a
/// thumbnail grid saturating every CPU.
/// https://github.com/sxyazi/yazi/blob/e58022b9aafc8dabf586e2cc29b79a230071716f/yazi-core/src/tasks/prework.rs
/// https://github.com/sxyazi/yazi/blob/e58022b9aafc8dabf586e2cc29b79a230071716f/yazi-scheduler/src/scheduler.rs
pub struct WorkQueue<K> {
    queue: VecDeque<K>,
    /// Queued or in flight: what the view may show as loading.
    pending: HashSet<K>,
    inflight: HashSet<K>,
    wake_sender: async_channel::Sender<()>,
    wake_receiver: async_channel::Receiver<()>,
    workers: Vec<Task<()>>,
    width: usize,
}

impl<K: Hash + Eq + Clone> WorkQueue<K> {
    fn new(width: usize) -> Self {
        let (wake_sender, wake_receiver) = async_channel::bounded(width);
        Self {
            queue: VecDeque::new(),
            pending: HashSet::new(),
            inflight: HashSet::new(),
            wake_sender,
            wake_receiver,
            workers: Vec::new(),
            width,
        }
    }

    /// Drop every worker and everything queued.
    fn reset(&mut self) {
        self.workers.clear();
        self.queue.clear();
        self.pending.clear();
        self.inflight.clear();
        while self.wake_receiver.try_recv().is_ok() {}
    }

    pub fn is_pending(&self, key: &K) -> bool {
        self.pending.contains(key)
    }

    /// Replace the not-yet-started queue with `priority`, keeping what is
    /// already in flight and skipping what `done` already has.
    fn schedule(&mut self, priority: impl IntoIterator<Item = K>, done: impl Fn(&K) -> bool) {
        self.queue.clear();
        self.pending = self.inflight.clone();
        for key in priority {
            if done(&key) || !self.pending.insert(key.clone()) {
                continue;
            }
            self.queue.push_back(key);
        }
        for _ in 0..self.width {
            let _ = self.wake_sender.try_send(());
        }
    }

    fn take(&mut self) -> Option<K> {
        let key = self.queue.pop_front()?;
        self.inflight.insert(key.clone());
        Some(key)
    }

    fn finish(&mut self, key: &K) {
        self.inflight.remove(key);
        self.pending.remove(key);
    }

    fn forget(&mut self, key: &K) {
        self.queue.retain(|queued| queued != key);
        self.pending.remove(key);
    }
}

pub struct PreviewState {
    pub thumbnails: HashMap<PathBuf, ThumbnailState>,
    thumbnail_order: VecDeque<PathBuf>,
    pub thumbnail_queue: WorkQueue<PathBuf>,
    /// Invalidated while a decode was in flight: the result is dropped.
    thumbnail_stale: HashSet<PathBuf>,
    pub state: PreviewContent,
    pub ticket: u64,
    task: Option<Task<()>>,
    cancel: Option<Arc<AtomicBool>>,
    pub folder_entries: Vec<FileEntry>,
    pub folder_loading: bool,
    folder_error: Option<String>,
    folder_task: Option<Task<()>>,
    folder_scroll: UniformListScrollHandle,
    pdf_pages: HashMap<usize, PdfPageState>,
    pdf_queue: WorkQueue<usize>,
    pdf_scroll: UniformListScrollHandle,
    wrap: Option<WrappedPreview>,
    wrap_task: Option<Task<()>>,
    resize_task: Option<Task<()>>,
    text_scroll: UniformListScrollHandle,
    pub width: Rc<Cell<Pixels>>,
    mono_cell_width: Rc<Cell<Pixels>>,
    mono_line_height: Rc<Cell<Pixels>>,
}

impl PreviewState {
    pub fn new(mono_font_size: Pixels) -> Self {
        Self {
            thumbnails: HashMap::new(),
            thumbnail_order: VecDeque::new(),
            thumbnail_queue: WorkQueue::new(THUMBNAIL_WORKERS),
            thumbnail_stale: HashSet::new(),
            state: PreviewContent::Empty,
            ticket: 0,
            task: None,
            cancel: None,
            folder_entries: Vec::new(),
            folder_loading: false,
            folder_error: None,
            folder_task: None,
            folder_scroll: UniformListScrollHandle::new(),
            pdf_pages: HashMap::new(),
            pdf_queue: WorkQueue::new(PDF_PAGE_WORKERS),
            pdf_scroll: UniformListScrollHandle::new(),
            wrap: None,
            wrap_task: None,
            resize_task: None,
            text_scroll: UniformListScrollHandle::new(),
            width: Rc::new(Cell::new(px(0.0))),
            mono_cell_width: Rc::new(Cell::new(mono_font_size * 0.6)),
            mono_line_height: Rc::new(Cell::new(mono_font_size * 1.5)),
        }
    }

    pub fn reset_thumbnails(&mut self) {
        self.thumbnail_queue.reset();
        self.thumbnail_stale.clear();
        self.thumbnails.clear();
        self.thumbnail_order.clear();
    }

    pub fn invalidate_thumbnails(&mut self, paths: &[PathBuf]) {
        for path in paths {
            self.thumbnails.remove(path);
            self.thumbnail_order.retain(|existing| existing != path);
            self.thumbnail_queue.forget(path);
            if self.thumbnail_queue.inflight.contains(path) {
                self.thumbnail_stale.insert(path.clone());
            }
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

    /// Supersede whatever is loading and start on `state`, returning the
    /// ticket a late result must match to be shown.
    fn begin(&mut self, state: PreviewContent) -> (u64, Arc<AtomicBool>) {
        self.ticket = self.ticket.wrapping_add(1);
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        self.task.take();
        self.folder_task.take();
        self.folder_entries.clear();
        self.folder_loading = false;
        self.folder_error = None;
        self.folder_scroll = UniformListScrollHandle::new();
        self.pdf_queue.reset();
        self.pdf_pages.clear();
        self.pdf_scroll = UniformListScrollHandle::new();
        self.wrap_task.take();
        self.resize_task.take();
        self.wrap = None;
        self.text_scroll = UniformListScrollHandle::new();
        self.state = state;
        let cancelled = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancelled.clone());
        (self.ticket, cancelled)
    }

    pub fn clear(&mut self) {
        self.begin(PreviewContent::Empty);
        self.cancel = None;
    }

    fn wrap_columns(&self) -> usize {
        let width = f32::from(self.width.get());
        let width = if width > 0.0 { width } else { DEFAULT_PREVIEW_WIDTH };
        let cell_width = f32::from(self.mono_cell_width.get()).max(1.0);
        ((width - PREVIEW_TEXT_CHROME_WIDTH) / cell_width).floor().max(16.0) as usize
    }
}

impl Marcel {
    pub(super) fn start_preview(&mut self, entry: FileEntry, cx: &mut Context<Self>) {
        // Like Yazi's preview task, replacing this handle cancels the previous
        // foreground task. The ticket also prevents a late result from
        // becoming current:
        // https://github.com/sxyazi/yazi/blob/main/yazi-core/src/tab/preview.rs
        if entry.navigable {
            let (ticket, cancelled) = self
                .preview
                .begin(PreviewContent::Ready(Preview::Directory { path: entry.path.clone() }));
            self.start_folder_preview_load(entry.path, ticket, cancelled, cx);
            cx.notify();
            return;
        }
        let (ticket, cancelled) =
            self.preview.begin(PreviewContent::Loading { name: entry.name.clone() });
        let load_task = unblock(cx, move || load_preview(&entry, &cancelled));
        self.preview.task = Some(cx.spawn(async move |this, cx| {
            let result = load_task.await;
            let _ = this.update(cx, |this, cx| {
                if ticket != this.preview.ticket {
                    return;
                }
                this.preview.state = match result {
                    Ok(preview) => PreviewContent::Ready(preview),
                    Err(error) => PreviewContent::Error(error.to_string()),
                };
                this.start_preview_wrap(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Yazi exposes a bounded slice of the hovered folder to its preview
    /// layer and refreshes that folder independently from the main browser.
    /// Marcel adapts that separation with a cancellable partial-update
    /// stream and a virtualized, intentionally non-selectable preview.
    /// https://github.com/sxyazi/yazi/blob/e58022b9aafc8dabf586e2cc29b79a230071716f/yazi-actor/src/mgr/peek.rs
    fn start_folder_preview_load(
        &mut self,
        path: PathBuf,
        ticket: u64,
        cancelled: Arc<AtomicBool>,
        cx: &mut Context<Self>,
    ) {
        self.preview.folder_loading = true;
        let (sender, receiver) = async_channel::unbounded();
        let stream_path = path.clone();
        unblock(cx, move || stream_directory(&stream_path, sender, Some(&cancelled))).detach();

        self.preview.folder_task = Some(pump(cx, receiver, move |this, update, cx| {
            if ticket != this.preview.ticket {
                return false;
            }
            let PreviewContent::Ready(Preview::Directory { path: shown }) = &this.preview.state
            else {
                return false;
            };
            if shown != &path {
                return false;
            }
            let finished = matches!(&update, DirectoryUpdate::Done | DirectoryUpdate::Error(_));
            match update {
                DirectoryUpdate::Batch(batch) => {
                    this.preview.folder_entries = merge_sorted_entries(
                        std::mem::take(&mut this.preview.folder_entries),
                        batch,
                    );
                }
                DirectoryUpdate::Degraded { skipped, examples } => {
                    let examples = if examples.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", examples.join("; "))
                    };
                    this.preview.folder_error =
                        Some(format!("Skipped {skipped} unreadable entries{examples}"));
                }
                DirectoryUpdate::Done => this.preview.folder_loading = false,
                DirectoryUpdate::Error(error) => {
                    this.preview.folder_loading = false;
                    this.preview.folder_error = Some(error);
                }
            }
            cx.notify();
            !finished
        }));
    }

    fn activate_folder_preview_entry(
        &mut self,
        path: &std::path::Path,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.is_right_click() || event.click_count() < 2 {
            return;
        }
        if let Some(entry) =
            self.preview.folder_entries.iter().find(|entry| entry.path == path).cloned()
        {
            self.open_entry(entry, window, cx);
        }
    }

    // Background decode pools.

    /// Start `width` workers over `queue` if none are running. Each worker
    /// takes a key, runs `job` for it off the foreground, and hands the result
    /// to `finish` — unless `ticket` has moved on, in which case it stops.
    fn ensure_workers<K, R>(
        &mut self,
        cx: &mut Context<Self>,
        queue: fn(&mut Self) -> &mut WorkQueue<K>,
        ticket_of: fn(&Self) -> u64,
        job: fn(&Self, &K) -> Option<Job<R>>,
        finish: fn(&mut Self, K, R, &mut Context<Self>),
    ) where
        K: Hash + Eq + Clone + Send + 'static,
        R: Send + 'static,
    {
        if !queue(self).workers.is_empty() {
            return;
        }
        let ticket = ticket_of(self);
        let executor = cx.background_executor().clone();
        for _ in 0..queue(self).width {
            let executor = executor.clone();
            let wake = queue(self).wake_receiver.clone();
            let worker = cx.spawn(async move |this, cx| {
                loop {
                    let request = this
                        .update(cx, |this, _| {
                            let key = queue(this).take()?;
                            match job(this, &key) {
                                Some(work) => Some((key, work)),
                                // Nothing to do for it any more: not a
                                // failure, and not something to retry.
                                None => {
                                    queue(this).finish(&key);
                                    None
                                }
                            }
                        })
                        .ok()
                        .flatten();
                    let Some((key, work)) = request else {
                        if wake.recv().await.is_err() {
                            break;
                        }
                        continue;
                    };
                    let result = executor.spawn(smol::unblock(work)).await;
                    let keep_running = this
                        .update(cx, |this, cx| {
                            if ticket != ticket_of(this) {
                                return false;
                            }
                            queue(this).finish(&key);
                            finish(this, key, result, cx);
                            cx.notify();
                            true
                        })
                        .unwrap_or(false);
                    if !keep_running {
                        break;
                    }
                }
            });
            queue(self).workers.push(worker);
        }
    }

    pub(super) fn ensure_thumbnails(
        &mut self,
        visible: Range<usize>,
        nearby: Range<usize>,
        cx: &mut Context<Self>,
    ) {
        let priority = prioritize_thumbnail_indices(visible, nearby)
            .into_iter()
            .filter_map(|index| self.directory.visible_entry(index))
            .filter(|entry| !entry.navigable && thumbnails::supports(&entry.path))
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        let done = self.preview.thumbnails.keys().cloned().collect::<HashSet<_>>();
        self.preview.thumbnail_queue.schedule(priority, |path| done.contains(path));
        self.ensure_workers(
            cx,
            |this| &mut this.preview.thumbnail_queue,
            |this| this.directory.generation,
            |_, path| {
                let path = path.clone();
                Some(Box::new(move || thumbnails::load_or_create(&path)))
            },
            |this, path, result, _| {
                if this.preview.thumbnail_stale.remove(&path) {
                    return;
                }
                let state = match result {
                    Ok(thumbnail) => ThumbnailState::Ready(thumbnail),
                    Err(_) => ThumbnailState::Failed,
                };
                this.preview.remember_thumbnail(path, state);
            },
        );
    }

    fn ensure_pdf_pages(&mut self, visible: Range<usize>, cx: &mut Context<Self>) {
        let PreviewContent::Ready(Preview::Pdf { pages, .. }) = &self.preview.state else {
            return;
        };
        // Yazi prioritizes the currently visible page and treats PDF renders as
        // discardable preloads. Marcel applies that model to a continuous GUI
        // viewport and retains only a one-page lookahead:
        // https://github.com/sxyazi/yazi/blob/e58022b9aafc8dabf586e2cc29b79a230071716f/yazi-plugin/preset/plugins/pdf.lua
        let priority = prioritize_pdf_pages(visible, *pages);
        let done = self.preview.pdf_pages.keys().copied().collect::<HashSet<_>>();
        self.preview.pdf_queue.schedule(priority, |page| done.contains(page));
        self.ensure_workers(
            cx,
            |this| &mut this.preview.pdf_queue,
            |this| this.preview.ticket,
            |this, page| {
                let PreviewContent::Ready(Preview::Pdf { source, .. }) = &this.preview.state else {
                    return None;
                };
                let (source, page, cancelled) =
                    (source.clone(), *page, this.preview.cancel.clone()?);
                Some(Box::new(move || render_pdf_page(&source, page, &cancelled)))
            },
            |this, page, result, _| {
                let state = match result {
                    Ok(rendered) => PdfPageState::Ready(rendered.path),
                    Err(error) => PdfPageState::Failed(error.to_string()),
                };
                this.preview.pdf_pages.insert(page, state);
            },
        );
    }

    // Text wrapping.

    pub(super) fn start_preview_wrap(&mut self, cx: &mut Context<Self>) {
        let PreviewContent::Ready(Preview::Text { lines, render_rich: false, .. }) =
            &self.preview.state
        else {
            self.preview.wrap_task.take();
            self.preview.wrap = None;
            return;
        };
        let columns = self.preview.wrap_columns();
        if self.preview.wrap.as_ref().is_some_and(|wrapped| {
            wrapped.ticket == self.preview.ticket && wrapped.columns == columns
        }) {
            return;
        }
        let lines = lines.clone();
        let ticket = self.preview.ticket;
        let wrap_task = unblock(cx, move || wrap_preview_lines(&lines, columns));
        self.preview.wrap_task = Some(cx.spawn(async move |this, cx| {
            let lines = wrap_task.await;
            let _ = this.update(cx, |this, cx| {
                if ticket != this.preview.ticket || columns != this.preview.wrap_columns() {
                    return;
                }
                this.preview.wrap = Some(WrappedPreview { ticket, columns, lines });
                cx.notify();
            });
        }));
    }

    pub(super) fn schedule_preview_wrap(&mut self, cx: &mut Context<Self>) {
        self.preview.resize_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PREVIEW_WRAP_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| this.start_preview_wrap(cx));
        }));
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
        let line_height = (layout.ascent + layout.descent).max(mono_font_size * 1.5);
        if (self.preview.mono_cell_width.get() - cell_width).abs() >= px(0.1)
            || (self.preview.mono_line_height.get() - line_height).abs() >= px(0.1)
        {
            self.preview.mono_cell_width.set(cell_width);
            self.preview.mono_line_height.set(line_height);
            self.schedule_preview_wrap(cx);
        }
    }

    // Rendering.

    /// The name and detail lines under the preview, if something is shown.
    pub(super) fn preview_footer_lines(&self, cx: &Context<Self>) -> Vec<(String, Hsla)> {
        let colors = cx.theme().colors;
        let mut lines = Vec::new();
        if let Some(entry) = self.primary_entry() {
            lines.push((entry.name.clone(), colors.foreground));
            let details = if entry.navigable {
                let folders = self.preview.folder_entries.iter().filter(|e| e.navigable).count();
                let files = self.preview.folder_entries.len() - folders;
                let progress = if self.preview.folder_loading { " · Loading…" } else { "" };
                format!("Folder · {folders} folders · {files} files{progress}")
            } else {
                match format_size(entry.size) {
                    size if size.is_empty() => entry.display_kind().to_string(),
                    size => format!("{} · {size}", entry.display_kind()),
                }
            };
            lines.push((details, colors.muted_foreground));
        }
        match &self.preview.state {
            PreviewContent::Ready(Preview::Image { mime, .. }) => {
                lines.push((mime.clone(), colors.muted_foreground));
            }
            PreviewContent::Ready(Preview::Text { truncated, clipped_lines, .. }) => {
                if *truncated {
                    lines
                        .push(("Preview limited to the first 256 KiB".to_string(), colors.warning));
                }
                if *clipped_lines {
                    lines.push((
                        "Very long lines are shortened in the preview".to_string(),
                        colors.warning,
                    ));
                }
            }
            _ => {}
        }
        lines
    }

    pub(super) fn render_preview(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.update_preview_font_metrics(window, cx);
        let colors = cx.theme().colors;
        let (muted, danger) = (colors.muted_foreground, colors.danger);
        match &self.preview.state {
            PreviewContent::Empty => message("Select a file to preview", muted),
            PreviewContent::Loading { name } => message(format!("Loading {name}…"), muted),
            PreviewContent::Error(error) => message(format!("Preview failed\n{error}"), danger),
            PreviewContent::Ready(Preview::Metadata { summary }) => message(summary.clone(), muted),
            PreviewContent::Ready(Preview::Directory { .. }) => self.render_folder_preview(cx),
            PreviewContent::Ready(Preview::Image { image, .. }) => decoded_image(
                img(image.clone()).id(("preview-image", self.preview.ticket)),
                "Decoding image…",
                "This image could not be decoded",
                cx,
            ),
            PreviewContent::Ready(Preview::Pdf { pages, .. }) => {
                let pages = *pages;
                let scroll = self.preview.pdf_scroll.clone();
                let page_width = (f32::from(self.preview.width.get()) - 40.0).max(240.0);
                let page_height = px((page_width * 1.414).clamp(340.0, 1_240.0));
                let list = uniform_list(
                    ("preview-pdf-pages", self.preview.ticket),
                    pages,
                    cx.processor(move |this, range: Range<usize>, _, cx| {
                        this.ensure_pdf_pages(range.clone(), cx);
                        range
                            .map(|index| {
                                let page = index + 1;
                                let content = match this.preview.pdf_pages.get(&page).cloned() {
                                    Some(PdfPageState::Ready(path)) => decoded_image(
                                        img(path).id(("pdf-page-image", page)),
                                        format!("Loading page {page}…"),
                                        format!("Page {page} could not be decoded"),
                                        cx,
                                    ),
                                    Some(PdfPageState::Failed(error)) => message(
                                        format!("Page {page} failed to render\n{error}"),
                                        danger,
                                    ),
                                    None => message(format!("Rendering page {page}…"), muted),
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
                .track_scroll(&scroll)
                .size_full();
                scrolled(list, &scroll)
            }
            PreviewContent::Ready(Preview::Text {
                contents,
                language,
                markdown,
                render_rich: true,
                ..
            }) => {
                let source =
                    if *markdown { contents.clone() } else { code_fence(contents, language) };
                TextView::markdown(("preview-text", self.preview.ticket), source)
                    .selectable(true)
                    .scrollable(true)
                    .size_full()
                    .p_3()
                    .into_any_element()
            }
            PreviewContent::Ready(Preview::Text { .. }) => {
                let Some(wrapped) = self.preview.wrap.as_ref().filter(|wrapped| {
                    wrapped.ticket == self.preview.ticket
                        && wrapped.columns == self.preview.wrap_columns()
                }) else {
                    return message("Preparing wrapped preview…", muted);
                };
                let lines = wrapped.lines.clone();
                let scroll = self.preview.text_scroll.clone();
                let foreground = colors.foreground;
                let mono_font = cx.theme().mono_font_family.clone();
                let mono_font_size = cx.theme().mono_font_size;
                let mono_line_height = self.preview.mono_line_height.get();
                let list = uniform_list(
                    ("preview-text-lines", self.preview.ticket),
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
                .track_scroll(&scroll)
                .size_full()
                .px_3()
                .py_2();
                scrolled(list, &scroll)
            }
        }
    }

    fn render_folder_preview(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let radius = cx.theme().radius;
        if self.preview.folder_entries.is_empty() {
            return match (&self.preview.folder_error, self.preview.folder_loading) {
                (Some(error), _) => {
                    message(format!("Could not read this folder\n{error}"), colors.danger)
                }
                (None, true) => message("Loading folder contents…", colors.muted_foreground),
                (None, false) => message("This folder is empty", colors.muted_foreground),
            };
        }
        let scroll = self.preview.folder_scroll.clone();
        let list = uniform_list(
            ("folder-preview-entries", self.preview.ticket),
            self.preview.folder_entries.len(),
            cx.processor(move |this, range: Range<usize>, _, cx| {
                range
                    .filter_map(|index| {
                        let entry = this.preview.folder_entries.get(index)?.clone();
                        let click_path = entry.path.clone();
                        let detail = match format_size(entry.size) {
                            size if size.is_empty() => entry.display_kind().to_string(),
                            size => size,
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
                                    move |this, event: &ClickEvent, window, cx| {
                                        this.activate_folder_preview_entry(
                                            &click_path,
                                            event,
                                            window,
                                            cx,
                                        );
                                    },
                                ))
                                .child(super::browser::entry_icon(&entry, colors.primary))
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
        .track_scroll(&scroll)
        .size_full()
        .py_2();
        scrolled(list, &scroll)
    }
}

/// A list that fills the pane, with its scrollbar.
fn scrolled(list: impl IntoElement, scroll: &UniformListScrollHandle) -> AnyElement {
    div().relative().size_full().child(list).vertical_scrollbar(scroll).into_any_element()
}

/// An image that fills the pane, saying so while it decodes and if it cannot.
fn decoded_image(
    image: Stateful<Img>,
    loading: impl Into<String>,
    failed: impl Into<String>,
    cx: &App,
) -> AnyElement {
    let colors = cx.theme().colors;
    let (loading, failed) = (loading.into(), failed.into());
    image
        .size_full()
        .object_fit(ObjectFit::Contain)
        .with_loading(move || message(loading.clone(), colors.muted_foreground))
        .with_fallback(move || message(failed.clone(), colors.danger))
        .into_any_element()
}

/// A centred, wrapping message filling the pane.
///
/// A flex item's automatic minimum size is its content, so without `min_w_0`
/// the text is laid out at its natural width no matter how narrow the pane is,
/// centred on a box wider than the pane and clipped at *both* edges. Every
/// message here is user-facing text of unbounded length.
fn message(message: impl Into<String>, color: Hsla) -> AnyElement {
    div()
        .flex()
        .size_full()
        .items_center()
        .justify_center()
        .px_6()
        .text_color(color)
        .child(div().min_w_0().text_center().child(message.into()))
        .into_any_element()
}

fn code_fence(contents: &str, language: &str) -> String {
    let mut fence = "```".to_string();
    while contents.contains(&fence) {
        fence.push('`');
    }
    format!("{fence}{language}\n{contents}\n{fence}")
}

fn prioritize_thumbnail_indices(visible: Range<usize>, nearby: Range<usize>) -> Vec<usize> {
    visible.clone().chain(nearby.filter(|index| !visible.contains(index))).collect()
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

fn wrap_preview_lines(lines: &[String], columns: usize) -> Arc<[WrappedPreviewLine]> {
    let columns = columns.max(1);
    let mut wrapped = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        if line.is_empty() {
            wrapped
                .push(WrappedPreviewLine { source_line: Some(index + 1), contents: String::new() });
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
        let character_width = if character == '\t' { 4 } else { character.width().unwrap_or(0) };
        if width + character_width > columns {
            return last_preferred_break.unwrap_or(if byte_index == 0 {
                character.len_utf8()
            } else {
                byte_index
            });
        }
        width += character_width;
        if character.is_whitespace() && width >= columns / 2 {
            last_preferred_break = Some(byte_index + character.len_utf8());
        }
    }
    line.len()
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
    fn thumbnail_priority_puts_visible_items_before_lookahead() {
        assert_eq!(prioritize_thumbnail_indices(10..13, 8..15), vec![10, 11, 12, 8, 9, 13, 14]);
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

    /// The queue keeps in-flight work, drops what was merely queued, and never
    /// re-queues what is already done.
    #[test]
    fn a_rescheduled_queue_keeps_inflight_work_ahead_of_new_priorities() {
        let mut queue: WorkQueue<u32> = WorkQueue::new(1);
        queue.schedule([1, 2, 3], |_| false);
        assert_eq!(queue.take(), Some(1));
        queue.schedule([3, 4, 1], |key| *key == 4);
        assert!(queue.is_pending(&1), "in flight stays pending");
        assert!(!queue.is_pending(&2), "superseded work is forgotten");
        assert!(!queue.is_pending(&4), "finished work is not queued again");
        assert_eq!(queue.take(), Some(3));
        queue.finish(&1);
        assert!(!queue.is_pending(&1));
    }
}
