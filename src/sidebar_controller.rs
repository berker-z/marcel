use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
};

use gpui::{Bounds, Pixels, Point, Task};

use crate::{
    bookmarks::{Bookmark, default_path as default_bookmarks_path},
    places::Place,
    trash_ops::TrashRecord,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BookmarkInsertion {
    pub index: usize,
}

#[derive(Clone, Copy)]
pub struct BookmarkMenu {
    pub index: usize,
    pub position: Point<Pixels>,
}

pub struct SidebarController {
    pub places: Vec<Place>,
    pub place_icons: HashMap<PathBuf, PathBuf>,
    pub places_loading: bool,
    pub places_task: Option<Task<()>>,
    pub browsing_trash: bool,
    pub trash_records: HashMap<PathBuf, TrashRecord>,
    pub bookmarks: Vec<Bookmark>,
    pub bookmark_icons: HashMap<PathBuf, PathBuf>,
    pub bookmarks_path: PathBuf,
    pub bookmarks_loading: bool,
    pub bookmarks_load_task: Option<Task<()>>,
    pub bookmarks_save_task: Option<Task<()>>,
    pub bookmark_insertion: Option<BookmarkInsertion>,
    pub bookmark_menu: Option<BookmarkMenu>,
    pub bookmark_region_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    pub bookmark_row_bounds: Rc<RefCell<HashMap<usize, Bounds<Pixels>>>>,
    pub place_drop_bounds: Rc<RefCell<HashMap<PathBuf, Bounds<Pixels>>>>,
}

impl SidebarController {
    pub fn new(home: &Path) -> Self {
        Self {
            places: vec![Place::home(home.to_path_buf())],
            place_icons: HashMap::new(),
            places_loading: true,
            places_task: None,
            browsing_trash: false,
            trash_records: HashMap::new(),
            bookmarks: Vec::new(),
            bookmark_icons: HashMap::new(),
            bookmarks_path: default_bookmarks_path(home),
            bookmarks_loading: true,
            bookmarks_load_task: None,
            bookmarks_save_task: None,
            bookmark_insertion: None,
            bookmark_menu: None,
            bookmark_region_bounds: Rc::new(Cell::new(None)),
            bookmark_row_bounds: Rc::new(RefCell::new(HashMap::new())),
            place_drop_bounds: Rc::new(RefCell::new(HashMap::new())),
        }
    }
}
