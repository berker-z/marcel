//! The listing itself: the list and grid views over the same entries, and
//! the empty-space surface around them that marquees start on.

use std::ops::Range;

use gpui::prelude::*;
use gpui::{
    AnyElement, Bounds, ClickEvent, Context, Div, ElementId, Hsla, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit, Stateful, canvas, div, img, px,
    relative, uniform_list,
};
use gpui_component::{
    ActiveTheme as _, Sizable as _, h_flex, input::Input, scroll::ScrollableElement as _, v_flex,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    browse::entries::{FileEntry, SortKey, format_modified, format_size},
    preview::thumbnails,
};

use super::{
    DIRECTORY_ROW_HEIGHT, GRID_GAP, GRID_ROW_HEIGHT, GRID_SIDE_PADDING, GRID_TILE_HEIGHT,
    GRID_TILE_WIDTH, Marcel,
    pointer::{FileDrag, accept_file_drops, painted_bounds},
    preview::ThumbnailState,
    state::{EntryHitRegion, ViewMode},
};

const GRID_VISUAL_SIZE: f32 = 104.0;
const GRID_ICON_SIZE: f32 = 80.0;
const GRID_LABEL_HEIGHT: f32 = 64.0;
const GRID_LABEL_COLUMNS: usize = 30;

/// The list columns. Rows and the header share these so they line up; the
/// row is content-sized in the sense that it ends where the last column
/// does, and the space beyond stays a marquee start target.
const LIST_ICON_WIDTH: f32 = 20.0;
const LIST_NAME_WIDTH: f32 = 340.0;
const LIST_SIZE_WIDTH: f32 = 72.0;
const LIST_MODIFIED_WIDTH: f32 = 132.0;
pub(super) const LIST_HEADER_HEIGHT: f32 = 26.0;

/// An entry's icon at row size: the themed image, or the glyph fallback.
pub(super) fn entry_icon(entry: &FileEntry, fallback_color: Hsla) -> AnyElement {
    match entry.icon_path.clone() {
        Some(icon_path) => {
            img(icon_path).size(px(20.0)).object_fit(ObjectFit::Contain).into_any_element()
        }
        None => div().w(px(20.0)).text_color(fallback_color).child(entry.icon()).into_any_element(),
    }
}

impl Marcel {
    pub(super) fn grid_columns(&self) -> usize {
        let width = self
            .drag
            .browser_bounds
            .get()
            .map(|bounds| f32::from(bounds.size.width))
            .unwrap_or(GRID_TILE_WIDTH + GRID_GAP);
        grid_column_count(width)
    }

    pub(super) fn set_view_mode(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        if self.ui.view_mode == mode {
            return;
        }
        self.ui.view_mode = mode;
        self.persist_browser_state();
        self.drag.reset_geometry();
        self.ui.directory_scroll = gpui::UniformListScrollHandle::new();
        if let Some(row) = self
            .directory
            .selection
            .primary()
            .cloned()
            .and_then(|primary| self.scroll_row_of(&primary))
        {
            self.ui.directory_scroll.scroll_to_item(row, gpui::ScrollStrategy::Center);
        }
        cx.notify();
    }

    /// Everything a row and a tile share: selection styling, dragging the
    /// entry out, dropping onto a folder, click and right-click, and the
    /// painted bounds the pointer code hit-tests against.
    ///
    /// gpui-component's ListItem owns the full row width by design. Marcel
    /// needs the unused row canvas to remain a marquee start target, so this
    /// content-sized surface is deliberately its own.
    fn entry_surface(
        &self,
        id: impl Into<ElementId>,
        entry: &FileEntry,
        selected_drag: &Option<FileDrag>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let colors = cx.theme().colors;
        let path = entry.path.clone();
        let selected = self.directory.selection.is_selected(&path);
        let navigable = entry.navigable;
        let drag = match selected_drag {
            Some(drag) if selected => drag.clone(),
            _ => FileDrag::single(&path, navigable),
        };
        let dragging_enabled = !self.sidebar.browsing_trash && self.ui.rename.is_none();
        let busy = self.operations_busy(cx);
        let entry_hit_bounds = self.drag.entry_hit_bounds.clone();
        let entry_content_bounds = self.drag.entry_content_bounds.clone();
        let browser_bounds = self.drag.browser_bounds.clone();
        let directory_scroll = self.ui.directory_scroll.clone();
        let (click_path, context_path, bounds_path) = (path.clone(), path.clone(), path.clone());

        let surface = div()
            .id(id)
            .relative()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(colors.list_active_border.opacity(0.0))
            .cursor_pointer()
            .hover(|this| this.bg(colors.list_hover))
            .when(selected, |this| {
                this.bg(colors.list_active).border_color(colors.list_active_border)
            })
            .when(dragging_enabled, |this| {
                this.on_drag(drag, |drag, _, _, cx| drag.preview(cx))
                    .external_drag_payload(|drag: &FileDrag, _, _| drag.native_payload())
            })
            .on_drag_move::<FileDrag>(cx.listener(|this, event, window, cx| {
                this.update_file_drag_cursor(event, window, cx);
            }));
        let surface = if navigable {
            accept_file_drops(
                surface,
                &path,
                busy,
                move |style| style.bg(colors.list_active).border_color(colors.primary),
                cx,
            )
        } else {
            surface
        };
        surface
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.activate_entry(&click_path, event, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.focus_browser(window, cx);
                    this.prepare_entry_context_menu(&context_path, event.position, cx);
                }),
            )
            .child(painted_bounds(move |bounds| {
                entry_hit_bounds
                    .borrow_mut()
                    .insert(bounds_path.clone(), EntryHitRegion { bounds, navigable });
                if let Some(browser) = browser_bounds.get() {
                    let scroll = directory_scroll.0.borrow().base_handle.offset();
                    entry_content_bounds.borrow_mut().insert(
                        bounds_path.clone(),
                        Bounds {
                            origin: bounds.origin - browser.origin - scroll,
                            size: bounds.size,
                        },
                    );
                }
            }))
    }

    /// A drag payload for the rows to share. A marquee originates on empty
    /// browser space, so no entry drag can begin until that gesture ends;
    /// rebuilding a potentially huge selected-file payload on every marquee
    /// repaint would be wasted.
    fn shared_drag(&mut self) -> Option<FileDrag> {
        self.drag.marquee.is_none().then(|| self.selected_file_drag()).flatten()
    }

    fn render_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let selected_drag = self.shared_drag();
        let rows = uniform_list(
            "directory-entries",
            self.directory.visible_entries.len(),
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                range
                    .filter_map(|index| {
                        let entry = this.directory.visible_entry(index)?.clone();
                        let name = match this.ui.rename_input_for(&entry.path) {
                            Some(input) => div()
                                .w(px(LIST_NAME_WIDTH))
                                .child(Input::new(&input).small())
                                .into_any_element(),
                            None => div()
                                .w(px(LIST_NAME_WIDTH))
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(entry.name.clone())
                                .into_any_element(),
                        };
                        let detail = |width: f32, text: String| {
                            div()
                                .w(px(width))
                                .flex_none()
                                .text_right()
                                .text_xs()
                                .text_color(colors.muted_foreground)
                                .whitespace_nowrap()
                                .child(text)
                        };
                        let row = this
                            .entry_surface(("entry", index), &entry, &selected_drag, cx)
                            .h(px(32.0))
                            .px_3()
                            .gap_2()
                            .flex()
                            .items_center()
                            .child(entry_icon(&entry, colors.primary))
                            .child(name)
                            .child(detail(LIST_SIZE_WIDTH, format_size(entry.size)))
                            .child(detail(
                                LIST_MODIFIED_WIDTH,
                                entry.modified.map(format_modified).unwrap_or_default(),
                            ));
                        Some(
                            div()
                                .flex()
                                .h(px(DIRECTORY_ROW_HEIGHT))
                                .w_full()
                                .items_center()
                                .child(row),
                        )
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.ui.directory_scroll)
        .flex_1()
        .min_h_0();
        v_flex().size_full().child(self.render_list_header(cx)).child(rows).into_any_element()
    }

    /// The column headings. Each names the key it sorts by; the one in force
    /// carries an arrow, and choosing it again reverses the order.
    fn render_list_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let order = self.directory.sort;
        let heading = |id: &'static str, key: SortKey, width: f32, right: bool| {
            let active = order.key == key;
            let label = if active {
                format!("{} {}", key.label(), if order.descending { "▾" } else { "▴" })
            } else {
                key.label().to_string()
            };
            div()
                .id(id)
                .w(px(width))
                .flex_none()
                .px_1()
                .rounded(cx.theme().radius)
                .cursor_pointer()
                .hover(|this| this.bg(colors.list_hover))
                .when(right, |this| this.text_right())
                .when(active, |this| this.text_color(colors.foreground))
                .whitespace_nowrap()
                .child(label)
                // A press here is a click on a heading, not the start of a
                // marquee on empty browser space.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |this, _, _, cx| this.sort_by(key, cx)))
        };
        h_flex()
            .flex_none()
            .h(px(LIST_HEADER_HEIGHT))
            .px_3()
            .gap_2()
            .items_center()
            .text_xs()
            .text_color(colors.muted_foreground)
            .border_b_1()
            .border_color(colors.border)
            .child(div().w(px(LIST_ICON_WIDTH)).flex_none())
            .child(heading("sort-by-name", SortKey::Name, LIST_NAME_WIDTH, false))
            .child(heading("sort-by-size", SortKey::Size, LIST_SIZE_WIDTH, true))
            .child(heading("sort-by-modified", SortKey::Modified, LIST_MODIFIED_WIDTH, true))
            .into_any_element()
    }

    fn render_grid(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let selected_drag = self.shared_drag();
        let columns = self.grid_columns();
        if columns != self.ui.grid_layout_columns {
            self.ui.grid_layout_columns = columns;
            self.drag.entry_content_bounds.borrow_mut().clear();
        }
        let row_count = self.directory.visible_entries.len().div_ceil(columns);

        uniform_list(
            "directory-grid-rows",
            row_count,
            cx.processor(move |this, rows: Range<usize>, _window, cx| {
                let total = this.directory.visible_entries.len();
                let visible = rows.start * columns..(rows.end * columns).min(total);
                let nearby =
                    rows.start.saturating_sub(1) * columns..((rows.end + 1) * columns).min(total);
                this.ensure_thumbnails(visible, nearby, cx);

                rows.map(|row| {
                    let start = row * columns;
                    let tiles = (start..(start + columns).min(total))
                        .filter_map(|index| {
                            let entry = this.directory.visible_entry(index)?.clone();
                            let visual = this.grid_visual(&entry, cx);
                            let name = match this.ui.rename_input_for(&entry.path) {
                                Some(input) => div()
                                    .w_full()
                                    .h(px(GRID_LABEL_HEIGHT))
                                    .flex_none()
                                    .child(Input::new(&input).small())
                                    .into_any_element(),
                                None => div()
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
                                    .child(elide_filename(&entry.name, GRID_LABEL_COLUMNS))
                                    .into_any_element(),
                            };
                            Some(
                                this.entry_surface(
                                    ("grid-entry", index),
                                    &entry,
                                    &selected_drag,
                                    cx,
                                )
                                .flex()
                                .flex_col()
                                .items_center()
                                .w(px(GRID_TILE_WIDTH))
                                .h(px(GRID_TILE_HEIGHT))
                                .p_2()
                                .gap_1()
                                .child(visual)
                                .child(name),
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
        .track_scroll(&self.ui.directory_scroll)
        .h_full()
        .into_any_element()
    }

    /// The picture on a grid tile: the thumbnail, or the icon with a badge
    /// saying why there is no thumbnail yet.
    fn grid_visual(&self, entry: &FileEntry, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let radius = cx.theme().radius;
        let square =
            || div().flex().flex_none().size(px(GRID_VISUAL_SIZE)).items_center().justify_center();
        let fallback = |opacity: f32| match entry.icon_path.clone() {
            Some(icon_path) => square()
                .opacity(opacity)
                .child(img(icon_path).size(px(GRID_ICON_SIZE)).object_fit(ObjectFit::Contain))
                .into_any_element(),
            None => square()
                .opacity(opacity)
                .text_3xl()
                .text_color(colors.primary)
                .child(entry.icon())
                .into_any_element(),
        };
        let badged = |opacity: f32, label: &'static str, color: Hsla| {
            div()
                .relative()
                .size(px(GRID_VISUAL_SIZE))
                .child(fallback(opacity))
                .child(
                    div()
                        .absolute()
                        .right_1()
                        .bottom_1()
                        .flex()
                        .size_5()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .border_1()
                        .border_color(colors.background)
                        .bg(color)
                        .text_color(colors.background)
                        .text_sm()
                        .child(label),
                )
                .into_any_element()
        };
        let supported = !entry.navigable && thumbnails::supports(&entry.path);
        match thumbnail_presentation(
            self.preview.thumbnails.get(&entry.path),
            self.preview.thumbnail_queue.is_pending(&entry.path),
            supported,
        ) {
            ThumbnailPresentation::Ready(thumbnail) => square()
                .overflow_hidden()
                .rounded(radius)
                .child(img(thumbnail).size_full().object_fit(ObjectFit::Contain))
                .into_any_element(),
            ThumbnailPresentation::Loading => badged(0.5, "…", colors.muted_foreground),
            ThumbnailPresentation::Failed => badged(0.72, "!", colors.danger),
            ThumbnailPresentation::Unsupported => fallback(1.0),
        }
    }

    pub(super) fn render_browser(&mut self, cx: &mut Context<Self>) -> AnyElement {
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
        let contents = match empty_message {
            Some(message) => div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_color(colors.muted_foreground)
                .child(message)
                .into_any_element(),
            None => match self.ui.view_mode {
                ViewMode::List => self.render_list(cx),
                ViewMode::Grid => self.render_grid(cx),
            },
        };
        let marquee = self.marquee_rectangle().map(|rectangle| {
            div()
                .absolute()
                .left(rectangle.left())
                .top(rectangle.top())
                .w(rectangle.size.width)
                .h(rectangle.size.height)
                .border_1()
                .border_color(colors.primary.opacity(0.8))
                .bg(colors.primary.opacity(0.16))
        });
        let loading = self.directory.loading.then(|| {
            if self.directory.filter_query.is_empty() {
                format!("Loading… {} items", self.directory.entries.len())
            } else {
                format!(
                    "Loading… {} of {} items match",
                    self.directory.visible_entries.len(),
                    self.directory.entries.len()
                )
            }
        });
        let bounds_state = self.drag.browser_bounds.clone();
        let visible_hit_bounds = self.drag.entry_hit_bounds.clone();
        let gesture_view = cx.entity();
        let directory_scroll = self.ui.directory_scroll.clone();
        // The Trash listing is not a folder anything can be dropped into.
        let refuses_drops = self.operations_busy(cx) || self.sidebar.browsing_trash;
        let current_dir = self.directory.current_dir.clone();

        let surface = div().relative().flex().flex_col().flex_1().min_h_0();
        accept_file_drops(
            surface,
            &current_dir,
            refuses_drops,
            move |style| style.bg(colors.list_hover.opacity(0.55)),
            cx,
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, event, _, cx| this.begin_marquee(event, cx)),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|this, event, _, cx| this.prepare_directory_context_menu(event, cx)),
        )
        .child(
            canvas(
                move |bounds, _, _| {
                    bounds_state.set(Some(bounds));
                    visible_hit_bounds.borrow_mut().clear();
                },
                move |_, _, window, _| {
                    let move_view = gesture_view.clone();
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                        if phase.bubble() {
                            move_view.update(cx, |this, cx| this.update_marquee(event, cx));
                        }
                    });
                    let up_view = gesture_view.clone();
                    window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
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
        .children(marquee)
        .when_some(self.directory.warning.clone(), |this, warning| {
            this.child(
                div()
                    .mx_3()
                    .mb_2()
                    .px_3()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .bg(colors.warning.opacity(0.14))
                    .text_xs()
                    .text_color(colors.warning)
                    .child(warning),
            )
        })
        .when_some(loading, |this, status| {
            this.child(
                div().px_3().py_1().text_xs().text_color(colors.muted_foreground).child(status),
            )
        })
        .vertical_scrollbar(&directory_scroll)
        .into_any_element()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ThumbnailPresentation {
    Ready(std::path::PathBuf),
    Loading,
    Failed,
    Unsupported,
}

fn thumbnail_presentation(
    state: Option<&ThumbnailState>,
    pending: bool,
    supported: bool,
) -> ThumbnailPresentation {
    match state {
        Some(ThumbnailState::Ready(path)) => ThumbnailPresentation::Ready(path.clone()),
        Some(ThumbnailState::Failed) => ThumbnailPresentation::Failed,
        None if supported && pending => ThumbnailPresentation::Loading,
        None => ThumbnailPresentation::Unsupported,
    }
}

fn grid_column_count(viewport_width: f32) -> usize {
    let available = (viewport_width - GRID_SIDE_PADDING * 2.0).max(GRID_TILE_WIDTH);
    ((available + GRID_GAP) / (GRID_TILE_WIDTH + GRID_GAP)).floor().max(1.0) as usize
}

/// Shorten a name to `max_columns` of display width, keeping the extension.
fn elide_filename(name: &str, max_columns: usize) -> String {
    if UnicodeWidthStr::width(name) <= max_columns {
        return name.to_string();
    }
    if max_columns == 0 {
        return String::new();
    }
    const ELLIPSIS: &str = "…";
    if let Some((stem, suffix)) =
        name.rfind('.').filter(|dot| *dot > 0).map(|dot| name.split_at(dot))
    {
        let suffix_width = UnicodeWidthStr::width(suffix);
        if suffix_width + 2 < max_columns {
            let stem_columns = max_columns - suffix_width - 1;
            return format!("{}{ELLIPSIS}{suffix}", take_display_columns(stem, stem_columns));
        }
    }
    format!("{}{ELLIPSIS}", take_display_columns(name, max_columns.saturating_sub(1)))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn grid_columns_reserve_both_side_gutters() {
        assert_eq!(grid_column_count(120.0), 1);
        assert_eq!(grid_column_count(279.0), 1);
        assert_eq!(grid_column_count(280.0), 2);
        assert_eq!(grid_column_count(640.0), 4);
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
    fn thumbnail_presentation_distinguishes_work_failure_and_fallback() {
        let ready = ThumbnailState::Ready(PathBuf::from("/cache/thumbnail.png"));
        let failed = ThumbnailState::Failed;

        assert_eq!(
            thumbnail_presentation(Some(&ready), false, true),
            ThumbnailPresentation::Ready(PathBuf::from("/cache/thumbnail.png"))
        );
        assert_eq!(thumbnail_presentation(None, true, true), ThumbnailPresentation::Loading);
        assert_eq!(
            thumbnail_presentation(Some(&failed), false, true),
            ThumbnailPresentation::Failed
        );
        assert_eq!(thumbnail_presentation(None, false, false), ThumbnailPresentation::Unsupported);
        assert_eq!(thumbnail_presentation(None, true, false), ThumbnailPresentation::Unsupported);
    }
}
