use std::{
    cell::Cell,
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use gpui::{Pixels, Task, UniformListScrollHandle, px};

use crate::{fs::FileEntry, preview::PreviewState};

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

pub struct PreviewController {
    pub thumbnails: HashMap<PathBuf, ThumbnailState>,
    pub thumbnail_order: VecDeque<PathBuf>,
    pub thumbnail_queue: VecDeque<PathBuf>,
    pub thumbnail_pending: HashSet<PathBuf>,
    pub thumbnail_inflight: HashSet<PathBuf>,
    pub thumbnail_stale: HashSet<PathBuf>,
    pub thumbnail_wake_sender: async_channel::Sender<()>,
    pub thumbnail_wake_receiver: async_channel::Receiver<()>,
    pub thumbnail_workers: Vec<Task<()>>,
    pub state: PreviewState,
    pub ticket: u64,
    pub task: Option<Task<()>>,
    pub cancel: Option<Arc<AtomicBool>>,
    pub folder_entries: Vec<FileEntry>,
    pub folder_loading: bool,
    pub folder_error: Option<String>,
    pub folder_task: Option<Task<()>>,
    pub folder_scroll: UniformListScrollHandle,
    pub pdf_pages: HashMap<usize, PdfPageState>,
    pub pdf_queue: VecDeque<usize>,
    pub pdf_pending: HashSet<usize>,
    pub pdf_inflight: HashSet<usize>,
    pub pdf_wake_sender: async_channel::Sender<()>,
    pub pdf_wake_receiver: async_channel::Receiver<()>,
    pub pdf_workers: Vec<Task<()>>,
    pub pdf_scroll: UniformListScrollHandle,
    pub wrap: Option<WrappedPreview>,
    pub wrap_task: Option<Task<()>>,
    pub resize_task: Option<Task<()>>,
    pub text_scroll: UniformListScrollHandle,
    pub width: Rc<Cell<Pixels>>,
    pub mono_cell_width: Rc<Cell<Pixels>>,
    pub mono_line_height: Rc<Cell<Pixels>>,
}

impl PreviewController {
    pub fn new(mono_font_size: Pixels, thumbnail_workers: usize, pdf_workers: usize) -> Self {
        let (thumbnail_wake_sender, thumbnail_wake_receiver) =
            async_channel::bounded(thumbnail_workers);
        let (pdf_wake_sender, pdf_wake_receiver) = async_channel::bounded(pdf_workers);
        Self {
            thumbnails: HashMap::new(),
            thumbnail_order: VecDeque::new(),
            thumbnail_queue: VecDeque::new(),
            thumbnail_pending: HashSet::new(),
            thumbnail_inflight: HashSet::new(),
            thumbnail_stale: HashSet::new(),
            thumbnail_wake_sender,
            thumbnail_wake_receiver,
            thumbnail_workers: Vec::new(),
            state: PreviewState::Empty,
            ticket: 0,
            task: None,
            cancel: None,
            folder_entries: Vec::new(),
            folder_loading: false,
            folder_error: None,
            folder_task: None,
            folder_scroll: UniformListScrollHandle::new(),
            pdf_pages: HashMap::new(),
            pdf_queue: VecDeque::new(),
            pdf_pending: HashSet::new(),
            pdf_inflight: HashSet::new(),
            pdf_wake_sender,
            pdf_wake_receiver,
            pdf_workers: Vec::new(),
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
        self.thumbnail_workers.clear();
        self.thumbnail_queue.clear();
        self.thumbnail_pending.clear();
        self.thumbnail_inflight.clear();
        self.thumbnail_stale.clear();
        while self.thumbnail_wake_receiver.try_recv().is_ok() {}
        self.thumbnails.clear();
        self.thumbnail_order.clear();
    }

    pub fn invalidate_thumbnails(&mut self, paths: &[PathBuf]) {
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

    pub fn begin_loading(&mut self, name: String) -> (u64, Arc<AtomicBool>) {
        self.ticket = self.ticket.wrapping_add(1);
        self.cancel_active();
        self.reset_folder();
        self.reset_pdf();
        self.wrap_task.take();
        self.resize_task.take();
        self.wrap = None;
        self.text_scroll = UniformListScrollHandle::new();
        self.state = PreviewState::Loading { name };
        let cancelled = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancelled.clone());
        (self.ticket, cancelled)
    }

    pub fn clear(&mut self) {
        self.ticket = self.ticket.wrapping_add(1);
        self.cancel_active();
        self.reset_folder();
        self.reset_pdf();
        self.wrap_task.take();
        self.resize_task.take();
        self.wrap = None;
        self.text_scroll = UniformListScrollHandle::new();
        self.state = PreviewState::Empty;
    }

    pub fn reset_folder(&mut self) {
        self.folder_task.take();
        self.folder_entries.clear();
        self.folder_loading = false;
        self.folder_error = None;
        self.folder_scroll = UniformListScrollHandle::new();
    }

    pub fn reset_pdf(&mut self) {
        self.pdf_workers.clear();
        self.pdf_queue.clear();
        self.pdf_pending.clear();
        self.pdf_inflight.clear();
        while self.pdf_wake_receiver.try_recv().is_ok() {}
        self.pdf_pages.clear();
        self.pdf_scroll = UniformListScrollHandle::new();
    }

    fn cancel_active(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        self.task.take();
    }
}
