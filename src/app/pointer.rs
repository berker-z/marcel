//! Pointer gestures over the listing: marquee selection, dragging entries
//! out and dropping things in, and scrolling when a gesture reaches the
//! edge of the viewport.

use std::{
    any::Any,
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use gpui::prelude::*;
use gpui::{
    Bounds, Context, CursorStyle, DragMoveEvent, ExternalDragPayload, ExternalPaths, FileDragPaths,
    IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render,
    Task, Window, div, px,
};
use gpui_component::{ActiveTheme as _, h_flex};

use crate::browse::entries::EntryKind;

use super::{
    Marcel, POINTER_EDGE_SCROLL_INTERVAL,
    state::{CachedFileDrag, EntryHitRegion, MarqueeGesture},
};

const MARQUEE_THRESHOLD: f32 = 4.0;
const POINTER_EDGE_SCROLL_ZONE: f32 = 36.0;
const POINTER_EDGE_MAX_SCROLL_STEP: f32 = 18.0;
const MAX_EXTERNAL_DROP_PATHS: usize = 256;

#[derive(Clone, Debug)]
pub struct FileDrag {
    pub paths: Arc<[PathBuf]>,
    pub native_paths: Arc<[(PathBuf, bool)]>,
    pub bookmark_candidates: Arc<[PathBuf]>,
}

impl FileDrag {
    pub fn single(path: &Path, navigable: bool) -> Self {
        Self {
            paths: vec![path.to_path_buf()].into(),
            native_paths: vec![(path.to_path_buf(), navigable)].into(),
            bookmark_candidates: navigable.then(|| path.to_path_buf()).into_iter().collect(),
        }
    }

    /// What the compositor sees when the drag leaves the window.
    pub fn native_payload(&self) -> Option<ExternalDragPayload> {
        (!self.native_paths.is_empty()
            && self.native_paths.len() <= MAX_EXTERNAL_DROP_PATHS
            && self.native_paths.iter().all(|(path, _)| path.is_absolute()))
        .then(|| ExternalDragPayload::Files(FileDragPaths::new(self.native_paths.iter().cloned())))
    }

    /// The floating label under the pointer.
    pub fn preview(&self, cx: &mut gpui::App) -> gpui::Entity<DragPreview> {
        let label = match self.paths.as_ref() {
            [only] => file_name(only),
            paths => format!("{} selected items", paths.len()),
        };
        cx.new(|_| DragPreview {
            label,
            detail: "Move",
        })
    }
}

#[derive(Clone, Debug)]
pub struct BookmarkDrag {
    pub index: usize,
    pub path: PathBuf,
}

impl BookmarkDrag {
    pub fn preview(&self, cx: &mut gpui::App) -> gpui::Entity<DragPreview> {
        cx.new(|_| DragPreview {
            label: file_name(&self.path),
            detail: "Bookmark",
        })
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

pub struct DragPreview {
    label: String,
    detail: &'static str,
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

pub(super) fn can_move_files_to(paths: &[PathBuf], destination: &Path) -> bool {
    !paths.is_empty()
        && paths.iter().all(|source| {
            source != destination
                && source.parent() != Some(destination)
                && !destination.starts_with(source)
        })
}

fn can_accept_external_drop(paths: &[PathBuf], destination: &Path) -> bool {
    !paths.is_empty()
        && paths.len() <= MAX_EXTERNAL_DROP_PATHS
        && paths.iter().all(|path| path.is_absolute())
        && can_move_files_to(paths, destination)
}

pub(super) fn accepted_external_drop_paths(
    paths: &[PathBuf],
    destination: &Path,
) -> Option<Vec<PathBuf>> {
    if !can_accept_external_drop(paths, destination) {
        return None;
    }
    let mut seen = HashSet::with_capacity(paths.len());
    Some(
        paths
            .iter()
            .filter(|path| seen.insert(*path))
            .cloned()
            .collect(),
    )
}

/// Whether whatever is being dragged may land on `destination`.
pub(super) fn can_drop_files_on(value: &dyn Any, destination: &Path) -> bool {
    value
        .downcast_ref::<FileDrag>()
        .is_some_and(|drag| can_move_files_to(&drag.paths, destination))
        || value
            .downcast_ref::<ExternalPaths>()
            .is_some_and(|drag| can_accept_external_drop(drag.paths(), destination))
}

fn can_drop_on_entry_hit(
    hit: &EntryHitRegion,
    pointer: Point<Pixels>,
    sources: &[PathBuf],
    destination: &Path,
) -> bool {
    hit.navigable && hit.bounds.contains(&pointer) && can_move_files_to(sources, destination)
}

fn marquee_bounds(a: Point<Pixels>, b: Point<Pixels>) -> Bounds<Pixels> {
    Bounds::from_corners(
        Point::new(a.x.min(b.x), a.y.min(b.y)),
        Point::new(a.x.max(b.x), a.y.max(b.y)),
    )
}

/// How far to scroll per tick when the pointer is near the top or bottom
/// edge, accelerating toward the edge itself.
fn edge_scroll_delta(pointer_y: Pixels, viewport: Bounds<Pixels>) -> Pixels {
    let zone = px(POINTER_EDGE_SCROLL_ZONE);
    let proximity = if pointer_y < viewport.top() + zone {
        f32::from(viewport.top() + zone - pointer_y) / POINTER_EDGE_SCROLL_ZONE
    } else if pointer_y > viewport.bottom() - zone {
        -f32::from(pointer_y - (viewport.bottom() - zone)) / POINTER_EDGE_SCROLL_ZONE
    } else {
        return px(0.0);
    };
    px(POINTER_EDGE_MAX_SCROLL_STEP * proximity.clamp(-1.0, 1.0))
}

impl Marcel {
    /// The drag payload for the current selection, rebuilt only when the
    /// selection or the visible projection actually changes.
    pub(super) fn selected_file_drag(&mut self) -> Option<FileDrag> {
        let key = (
            self.directory.selection.revision(),
            self.directory.projection_revision(),
        );
        if self.drag.payload.as_ref().map(|cached| cached.key) != Some(key) {
            let payload = self.build_selected_file_drag();
            self.drag.payload = Some(CachedFileDrag { key, payload });
        }
        self.drag
            .payload
            .as_ref()
            .and_then(|cached| cached.payload.clone())
    }

    fn build_selected_file_drag(&self) -> Option<FileDrag> {
        let selected = self.directory.selection.selected();
        if selected.is_empty() {
            return None;
        }
        let mut paths = Vec::with_capacity(selected.len());
        let mut native_paths = Vec::with_capacity(selected.len());
        let mut bookmark_candidates = Vec::new();
        for entry in self
            .directory
            .visible_entries
            .iter()
            .filter_map(|i| self.directory.entries.get(*i))
        {
            if !selected.contains(&entry.path) {
                continue;
            }
            paths.push(entry.path.clone());
            native_paths.push((entry.path.clone(), entry.kind == EntryKind::Directory));
            if entry.navigable {
                bookmark_candidates.push(entry.path.clone());
            }
        }
        Some(FileDrag {
            paths: paths.into(),
            native_paths: native_paths.into(),
            bookmark_candidates: bookmark_candidates.into(),
        })
    }

    pub(super) fn set_bookmark_insertion(
        &mut self,
        event: &DragMoveEvent<BookmarkDrag>,
        cx: &mut Context<Self>,
    ) {
        let Some(region) = self.sidebar.bookmark_region_bounds.get() else {
            return;
        };
        let pointer = event.event.position;
        if !region.contains(&pointer) {
            return;
        }
        let rows = self.sidebar.bookmark_row_bounds.borrow();
        let mut ordered = rows.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|(index, _)| **index);
        let index = ordered
            .iter()
            .find_map(|(index, bounds)| {
                (pointer.y < bounds.top() + bounds.size.height / 2.0).then_some(**index)
            })
            .unwrap_or(self.bookmarks.read(cx).bookmarks().len());
        drop(rows);
        if self.sidebar.bookmark_insertion != Some(index) {
            self.sidebar.bookmark_insertion = Some(index);
            cx.notify();
        }
    }

    pub(super) fn update_file_drag_cursor(
        &mut self,
        event: &DragMoveEvent<FileDrag>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let drag = event.drag(cx).clone();
        let pointer = event.event.position;
        self.drag.file_pointer = Some(pointer);
        if self.drag.file_scroll_task.is_none() {
            self.drag.file_scroll_task = Some(self.autoscroll(cx, Self::tick_file_drag_autoscroll));
        }
        let busy = self.operations_busy(cx);
        let can_move_to = |path: &Path| !busy && can_move_files_to(&drag.paths, path);

        let over_browser_folder = !busy
            && self
                .drag
                .entry_hit_bounds
                .borrow()
                .iter()
                .any(|(path, hit)| can_drop_on_entry_hit(hit, pointer, &drag.paths, path));
        let over_place = self
            .sidebar
            .place_drop_bounds
            .borrow()
            .iter()
            .any(|(path, bounds)| bounds.contains(&pointer) && can_move_to(path));
        let bookmarks = self.bookmarks.read(cx);
        let over_bookmark =
            self.sidebar
                .bookmark_row_bounds
                .borrow()
                .iter()
                .any(|(index, bounds)| {
                    bounds.contains(&pointer)
                        && bookmarks
                            .bookmarks()
                            .get(*index)
                            .is_some_and(|bookmark| can_move_to(&bookmark.path))
                });
        let over_bookmark_region = self
            .sidebar
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

    // Marquee.

    /// Whether `position` is on empty browser space rather than an entry.
    pub(super) fn on_empty_browser_space(&self, position: Point<Pixels>) -> bool {
        self.drag
            .browser_bounds
            .get()
            .is_some_and(|bounds| bounds.contains(&position))
            && !self
                .drag
                .entry_hit_bounds
                .borrow()
                .values()
                .any(|entry| entry.bounds.contains(&position))
    }

    pub(super) fn begin_marquee(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        if !self.on_empty_browser_space(event.position) {
            return;
        }
        let Some(bounds) = self.drag.browser_bounds.get() else {
            return;
        };
        let additive = event.modifiers.secondary();
        let visible_paths = self
            .drag
            .entry_hit_bounds
            .borrow()
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        self.drag
            .entry_content_bounds
            .borrow_mut()
            .retain(|path, _| visible_paths.contains(path));
        let scroll = self.ui.directory_scroll.0.borrow().base_handle.offset();
        let base_selection = self.directory.selection.selected().clone();
        if !additive {
            self.directory.selection.clear();
            self.preview.clear();
        }
        self.drag.marquee = Some(MarqueeGesture {
            start_window: event.position,
            origin_content: event.position - bounds.origin - scroll,
            current_window: event.position,
            base_selection,
            additive,
            active: false,
        });
        cx.notify();
    }

    pub(super) fn update_marquee(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !event.dragging() {
            return;
        }
        let Some(gesture) = self.drag.marquee.as_mut() else {
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
            self.drag.marquee_scroll_task =
                Some(self.autoscroll(cx, Self::tick_marquee_autoscroll));
        }
    }

    fn apply_marquee_selection(&mut self, clear_preview: bool, cx: &mut Context<Self>) {
        let Some(bounds) = self.drag.browser_bounds.get() else {
            return;
        };
        let Some(gesture) = self.drag.marquee.as_ref() else {
            return;
        };
        let scroll = self.ui.directory_scroll.0.borrow().base_handle.offset();
        let rectangle = marquee_bounds(
            gesture.origin_content,
            gesture.current_window - bounds.origin - scroll,
        );
        let intersecting = self
            .drag
            .entry_content_bounds
            .borrow()
            .iter()
            .filter(|(_, entry_bounds)| entry_bounds.intersects(&rectangle))
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        let (base_selection, additive) = (gesture.base_selection.clone(), gesture.additive);
        self.directory
            .selection
            .replace_from_marquee(&base_selection, intersecting, additive);
        if clear_preview {
            self.preview.clear();
        }
        cx.notify();
    }

    pub(super) fn end_marquee(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        if event.button == MouseButton::Left && self.drag.marquee.take().is_some() {
            self.drag.marquee_scroll_task.take();
            let ordered = self.directory.visible_paths();
            self.directory.selection.ensure_primary(&ordered);
            self.preview_primary(cx);
            cx.notify();
        }
    }

    /// The marquee rectangle to paint, clipped to the browser, in browser
    /// coordinates.
    pub(super) fn marquee_rectangle(&self) -> Option<Bounds<Pixels>> {
        let gesture = self
            .drag
            .marquee
            .as_ref()
            .filter(|gesture| gesture.active)?;
        let bounds = self.drag.browser_bounds.get()?;
        let scroll = self.ui.directory_scroll.0.borrow().base_handle.offset();
        let rectangle = marquee_bounds(
            bounds.origin + gesture.origin_content + scroll,
            gesture.current_window,
        );
        let left = rectangle.left().max(bounds.left());
        let right = rectangle.right().min(bounds.right());
        let top = rectangle.top().max(bounds.top());
        let bottom = rectangle.bottom().min(bounds.bottom());
        (right >= left && bottom >= top).then(|| {
            Bounds::from_corners(
                Point::new(left - bounds.left(), top - bounds.top()),
                Point::new(right - bounds.left(), bottom - bounds.top()),
            )
        })
    }

    // Edge autoscroll.

    /// Run `tick` every frame interval until it says to stop.
    fn autoscroll(
        &self,
        cx: &mut Context<Self>,
        tick: fn(&mut Self, &mut Context<Self>) -> bool,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(POINTER_EDGE_SCROLL_INTERVAL)
                    .await;
                if !this.update(cx, tick).unwrap_or(false) {
                    break;
                }
            }
        })
    }

    /// Scroll the listing by the edge delta for `pointer_y`; whether it moved.
    fn scroll_toward_edge(&self, pointer_y: Pixels, bounds: Bounds<Pixels>) -> bool {
        let delta = edge_scroll_delta(pointer_y, bounds);
        if delta == px(0.0) {
            return false;
        }
        let handle = self.ui.directory_scroll.0.borrow().base_handle.clone();
        let mut offset = handle.offset();
        let old_offset = offset;
        offset.y = (offset.y + delta).clamp(-handle.max_offset().y, px(0.0));
        if offset == old_offset {
            return false;
        }
        handle.set_offset(offset);
        true
    }

    fn tick_marquee_autoscroll(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(bounds) = self.drag.browser_bounds.get() else {
            return false;
        };
        let Some(gesture) = self.drag.marquee.as_ref() else {
            return false;
        };
        if gesture.active && self.scroll_toward_edge(gesture.current_window.y, bounds) {
            self.apply_marquee_selection(false, cx);
        }
        true
    }

    fn tick_file_drag_autoscroll(&mut self, cx: &mut Context<Self>) -> bool {
        if !cx.has_active_drag() {
            self.drag.file_pointer = None;
            return false;
        }
        let Some(bounds) = self.drag.browser_bounds.get() else {
            return false;
        };
        let Some(pointer) = self.drag.file_pointer else {
            return false;
        };
        if pointer.x >= bounds.left() && pointer.x <= bounds.right() {
            self.scroll_toward_edge(pointer.y, bounds);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn external_drop_is_bounded_absolute_and_deduplicated() {
        let destination = Path::new("/archive");
        let paths = [
            PathBuf::from("/downloads/report.pdf"),
            PathBuf::from("/downloads/report.pdf"),
            PathBuf::from("/downloads/photos"),
        ];
        assert_eq!(
            accepted_external_drop_paths(&paths, destination),
            Some(vec![
                PathBuf::from("/downloads/report.pdf"),
                PathBuf::from("/downloads/photos"),
            ])
        );
        assert!(!can_accept_external_drop(
            &[PathBuf::from("relative.txt")],
            destination,
        ));
        assert!(!can_accept_external_drop(&[], destination));
        assert!(!can_accept_external_drop(
            &vec![PathBuf::from("/downloads/item"); MAX_EXTERNAL_DROP_PATHS + 1],
            destination,
        ));
        // Copying into the source or its own parent asks for nothing.
        assert!(!can_accept_external_drop(
            &[PathBuf::from("/work/photos")],
            Path::new("/work/photos/edited"),
        ));
        assert!(!can_accept_external_drop(
            &[PathBuf::from("/work/report.txt")],
            Path::new("/work"),
        ));
    }

    #[test]
    fn outbound_drag_preserves_paths_and_directory_metadata() {
        let drag = FileDrag {
            paths: Arc::from([
                PathBuf::from("/downloads/report.pdf"),
                PathBuf::from("/downloads/photos"),
            ]),
            native_paths: Arc::from([
                (PathBuf::from("/downloads/report.pdf"), false),
                (PathBuf::from("/downloads/photos"), true),
            ]),
            bookmark_candidates: Arc::from([]),
        };
        let Some(ExternalDragPayload::Files(paths)) = drag.native_payload() else {
            panic!("expected an outbound file payload");
        };
        assert_eq!(paths.entries(), drag.native_paths.as_ref());

        let mut invalid = drag.clone();
        invalid.native_paths = Arc::from([(PathBuf::from("relative.txt"), false)]);
        assert!(invalid.native_payload().is_none());
        invalid.native_paths = Arc::from([]);
        assert!(invalid.native_payload().is_none());
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
        let hit = |navigable| EntryHitRegion { bounds, navigable };

        assert!(can_drop_on_entry_hit(
            &hit(true),
            pointer,
            &sources,
            destination
        ));
        assert!(!can_drop_on_entry_hit(
            &hit(false),
            pointer,
            &sources,
            destination
        ));
    }
}
