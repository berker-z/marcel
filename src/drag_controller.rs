use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    path::PathBuf,
    rc::Rc,
};

use gpui::{Bounds, Pixels, Point, Task};

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

pub struct DragController {
    pub marquee: Option<MarqueeGesture>,
    pub marquee_scroll_task: Option<Task<()>>,
    pub file_pointer: Option<Point<Pixels>>,
    pub file_scroll_task: Option<Task<()>>,
    pub browser_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    pub entry_hit_bounds: Rc<RefCell<HashMap<PathBuf, EntryHitRegion>>>,
    pub entry_content_bounds: Rc<RefCell<HashMap<PathBuf, Bounds<Pixels>>>>,
}

impl Default for DragController {
    fn default() -> Self {
        Self {
            marquee: None,
            marquee_scroll_task: None,
            file_pointer: None,
            file_scroll_task: None,
            browser_bounds: Rc::new(Cell::new(None)),
            entry_hit_bounds: Rc::new(RefCell::new(HashMap::new())),
            entry_content_bounds: Rc::new(RefCell::new(HashMap::new())),
        }
    }
}
