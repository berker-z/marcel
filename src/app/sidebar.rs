//! The places, devices, and bookmarks column, and what lands on it. The
//! Network section between them is in `network.rs`.

use std::path::{Path, PathBuf};

use gpui::prelude::*;
use gpui::{
    AnyElement, ClickEvent, Context, CursorStyle, Div, Hsla, IntoElement, MouseButton,
    MouseDownEvent, ObjectFit, Pixels, Stateful, TextRun, Window, div, font, img, px,
};
use gpui_component::{
    ActiveTheme as _, WindowExt as _, h_flex, notification::Notification, tooltip::Tooltip,
};

use crate::{
    bookmarks::Bookmark,
    desktop::{
        icons::IconProvider,
        places::{Place, discover as discover_places},
        volumes::Volume,
    },
};

use super::{
    Marcel,
    menu::{clamp_to_window, menu_row, popover},
    navigation::unblock,
    pointer::{BookmarkDrag, FileDrag, accept_file_drops, painted_bounds},
    state::{BookmarkMenu, VolumeMenu},
};

/// Wide enough for the six buttons of the top bar's navigation cluster,
/// which is aligned to the sidebar.
pub(super) const MIN_PLACES_WIDTH: f32 = 208.0;
const MAX_PLACES_WIDTH: f32 = 320.0;
pub(super) const BOOKMARK_MENU_WIDTH: f32 = 152.0;
pub(super) const BOOKMARK_MENU_HEIGHT: f32 = 38.0;

/// A sidebar entry's icon: the themed image, or a marker.
pub(super) fn sidebar_icon(icon: Option<PathBuf>, fallback_color: Hsla) -> AnyElement {
    match icon {
        Some(path) => img(path).size(px(20.0)).object_fit(ObjectFit::Contain).into_any_element(),
        None => div().w(px(20.0)).text_color(fallback_color).child("▸").into_any_element(),
    }
}

/// A 2 px insertion marker, visible when `shown`.
fn insertion_marker(shown: bool, colors: &gpui_component::ThemeColor) -> Div {
    div().h(px(2.0)).mx_2().rounded_full().bg(if shown {
        colors.primary
    } else {
        colors.primary.opacity(0.0)
    })
}

impl Marcel {
    pub(super) fn start_places_load(&mut self, home: PathBuf, cx: &mut Context<Self>) {
        let load_task = unblock(cx, move || {
            let places = discover_places(&home);
            let mut icon_provider = IconProvider::discover();
            let icons = places
                .iter()
                .filter_map(|place| {
                    icon_provider
                        .icon_for_place(&place.label)
                        .map(|icon| (place.path.clone(), icon))
                })
                .collect();
            (places, icons)
        });
        self.sidebar.places_task = Some(cx.spawn(async move |this, cx| {
            let (places, icons) = load_task.await;
            let _ = this.update(cx, |this, cx| {
                this.sidebar.places = places;
                this.sidebar.place_icons = icons;
                this.sidebar.places_loading = false;
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
        self.sidebar.bookmark_insertion = None;
        if paths.is_empty() {
            return;
        }
        // The icon comes from this window's projection, because that is where
        // the drag started; the bookmark itself belongs to the application.
        let candidates = paths
            .iter()
            .map(|path| {
                (path.clone(), self.directory.entry(path).and_then(|entry| entry.icon_path.clone()))
            })
            .collect::<Vec<_>>();
        let origin = Self::origin(window);
        // `None` means the store refused and has already said why.
        let Some(added) = self
            .bookmarks
            .clone()
            .update(cx, |bookmarks, cx| bookmarks.add(&candidates, origin, cx))
        else {
            return;
        };
        let notification = if added == 0 {
            Notification::info("Those folders are already bookmarked")
        } else {
            Notification::success(format!("Added {added} bookmark(s)"))
        };
        window.push_notification(notification, cx);
        cx.notify();
    }

    fn move_bookmark(
        &mut self,
        from: usize,
        dragged: &Path,
        insertion: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar.bookmark_insertion = None;
        let origin = Self::origin(window);
        self.bookmarks.clone().update(cx, |bookmarks, cx| {
            bookmarks.move_to(from, dragged, insertion, origin, cx);
        });
        cx.notify();
    }

    fn remove_bookmark(
        &mut self,
        index: usize,
        expected: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar.bookmark_menu = None;
        let origin = Self::origin(window);
        let removed = self
            .bookmarks
            .clone()
            .update(cx, |bookmarks, cx| bookmarks.remove_at(index, expected, origin, cx));
        if let Some(bookmark) = removed {
            window.push_notification(
                Notification::success(format!("Removed bookmark “{}”", bookmark.label())),
                cx,
            );
        }
        cx.notify();
    }

    pub(super) fn render_bookmark_menu(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let menu = self.sidebar.bookmark_menu.clone()?;
        // The menu names a bookmark, not a slot: if another window changed the
        // list since it opened, the menu no longer describes what a click
        // would act on, so it goes away instead.
        self.bookmarks
            .read(cx)
            .bookmarks()
            .get(menu.index)
            .filter(|bookmark| bookmark.path == menu.path)?;
        let (left, top) =
            clamp_to_window(menu.position, (BOOKMARK_MENU_WIDTH, BOOKMARK_MENU_HEIGHT), window);
        let index = menu.index;
        Some(
            popover("bookmark-context-menu", left, top, BOOKMARK_MENU_WIDTH, cx)
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.sidebar.bookmark_menu = None;
                    cx.notify();
                }))
                .child(menu_row(("bookmark-menu-remove", 0), "Remove Bookmark", true, cx).on_click(
                    cx.listener(move |this, _, window, cx| {
                        this.remove_bookmark(index, &menu.path, window, cx);
                    }),
                ))
                .into_any_element(),
        )
    }

    /// The shared row: a place or a bookmark, highlighted when it is where
    /// the window is, and accepting dropped files.
    pub(super) fn sidebar_row(
        &self,
        id: (&'static str, usize),
        path: &Path,
        active: bool,
        droppable: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let colors = cx.theme().colors;
        let busy = self.operations_busy(cx);
        let navigate_path = path.to_path_buf();
        let row = h_flex()
            .id(id)
            .relative()
            .w_full()
            .h_8()
            .px_2()
            .gap_2()
            .rounded(cx.theme().radius)
            .cursor_pointer()
            .hover(|this| {
                this.bg(colors.sidebar_accent.opacity(0.8))
                    .text_color(colors.sidebar_accent_foreground)
            })
            .when(active, |this| {
                this.bg(colors.sidebar_accent).text_color(colors.sidebar_accent_foreground)
            });
        if !droppable {
            return row;
        }
        accept_file_drops(
            row,
            path,
            busy,
            move |style| style.bg(colors.sidebar_accent).border_1().border_color(colors.primary),
            cx,
        )
        .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
            if !event.is_right_click() {
                this.navigate_to(navigate_path.clone(), true, cx);
            }
        }))
    }

    /// gpui-component's Button centers its inner label by design and its
    /// SidebarMenu only accepts bundled SVG assets. Places need left-aligned
    /// rows and icons from the active freedesktop theme, so this small
    /// navigation surface is intentionally Marcel-owned.
    fn render_place(&self, index: usize, place: Place, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let is_trash = place.is_trash();
        let active = if is_trash {
            self.sidebar.browsing_trash
        } else {
            !self.sidebar.browsing_trash && self.directory.current_dir == place.path
        };
        let icon = sidebar_icon(self.sidebar.place_icons.get(&place.path).cloned(), colors.primary);
        let place_drop_bounds = self.sidebar.place_drop_bounds.clone();
        let bounds_path = place.path.clone();
        self.sidebar_row(("place", index), &place.path, active, !is_trash, cx)
            .when(is_trash, |this| {
                this.on_click(cx.listener(|this, _, _, cx| {
                    this.start_trash_load(true, cx);
                }))
            })
            .child(icon)
            .child(div().flex_none().text_base().child(place.label))
            .when(!is_trash, |this| {
                this.child(painted_bounds(move |bounds| {
                    place_drop_bounds.borrow_mut().insert(bounds_path.clone(), bounds);
                }))
            })
            .into_any_element()
    }

    fn render_bookmark(
        &self,
        index: usize,
        bookmark: Bookmark,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors;
        let active = self.directory.current_dir == bookmark.path;
        let icon = sidebar_icon(
            self.bookmarks.read(cx).icon(&bookmark.path).map(Path::to_path_buf),
            colors.primary,
        );
        let drag = BookmarkDrag { index, path: bookmark.path.clone() };
        let menu_path = bookmark.path.clone();
        let bookmark_row_bounds = self.sidebar.bookmark_row_bounds.clone();
        div()
            .flex()
            .flex_col()
            .w_full()
            .child(insertion_marker(self.sidebar.bookmark_insertion == Some(index), &colors))
            .child(
                self.sidebar_row(("bookmark", index), &bookmark.path, active, true, cx)
                    .on_drag(drag, |drag, _, _, cx| drag.preview(cx))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            this.ui.entry_menu = None;
                            this.sidebar.volume_menu = None;
                            this.sidebar.network_menu = None;
                            this.sidebar.bookmark_menu = Some(BookmarkMenu {
                                index,
                                path: menu_path.clone(),
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
                    .child(painted_bounds(move |bounds| {
                        bookmark_row_bounds.borrow_mut().insert(index, bounds);
                    })),
            )
            .into_any_element()
    }

    // -----------------------------------------------------------------------
    // Devices: the drives UDisks2 reports.

    /// Open a volume: navigate to it if it is mounted, otherwise mount it
    /// and navigate when the mount returns.
    fn open_volume(&mut self, volume: Volume, window: &Window, cx: &mut Context<Self>) {
        let origin = Self::origin(window);
        let view = cx.entity();
        self.volumes.update(cx, |store, cx| {
            store.mount(
                volume,
                origin,
                move |mount_point, cx| {
                    view.update(cx, |this, cx| this.navigate_to(mount_point, true, cx));
                },
                cx,
            );
        });
    }

    /// Leave a volume before it goes away: a window standing on the mount
    /// point would otherwise be left in a directory that no longer exists.
    fn leave_volume(&mut self, volume: &Volume, cx: &mut Context<Self>) {
        if let Some(mount_point) = &volume.mount_point
            && self.directory.current_dir.starts_with(mount_point)
        {
            self.navigate_to(self.home_dir.clone(), true, cx);
        }
    }

    fn unmount_volume(&mut self, volume: Volume, window: &Window, cx: &mut Context<Self>) {
        self.leave_volume(&volume, cx);
        let origin = Self::origin(window);
        self.volumes.update(cx, |store, cx| store.unmount(volume, origin, cx));
    }

    fn eject_volume(&mut self, volume: Volume, window: &Window, cx: &mut Context<Self>) {
        self.leave_volume(&volume, cx);
        let origin = Self::origin(window);
        self.volumes.update(cx, |store, cx| store.eject(volume, origin, cx));
    }

    fn render_volume(&self, index: usize, volume: Volume, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let store = self.volumes.read(cx);
        let active = !self.sidebar.browsing_trash
            && store.volume_containing(&self.directory.current_dir).map(|v| &v.device)
                == Some(&volume.device);
        let busy = store.is_busy(&volume);
        let icon = sidebar_icon(store.icon(&volume).map(Path::to_path_buf), colors.primary);
        let mount_point = volume.mount_point.clone();
        let device = volume.device.clone();
        let eject_volume = volume.clone();
        let open_volume = volume.clone();
        let place_drop_bounds = self.sidebar.place_drop_bounds.clone();
        // A mounted volume is a folder, so it takes drops like a place. An
        // unmounted one has nowhere to put them.
        let row = match &mount_point {
            Some(point) => self.sidebar_row(("volume", index), point, active, true, cx),
            None => self
                .sidebar_row(("volume", index), &volume.device, active, false, cx)
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    if !event.is_right_click() {
                        this.open_volume(open_volume.clone(), window, cx);
                    }
                })),
        };
        row.on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                this.ui.entry_menu = None;
                this.sidebar.bookmark_menu = None;
                this.sidebar.network_menu = None;
                this.sidebar.volume_menu =
                    Some(VolumeMenu { device: device.clone(), position: event.position });
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
                .when(!volume.is_mounted(), |this| this.text_color(colors.muted_foreground))
                .child(volume.name.clone()),
        )
        .when(busy, |this| {
            this.child(div().text_xs().text_color(colors.muted_foreground).child("…"))
        })
        .when(!busy && volume.is_mounted() && volume.can_eject(), |this| {
            this.child(
                div()
                    .id(("volume-eject", index))
                    .flex_none()
                    .px_1()
                    .rounded(cx.theme().radius)
                    .text_color(colors.muted_foreground)
                    .hover(|this| this.text_color(colors.sidebar_accent_foreground))
                    .cursor_pointer()
                    .child("⏏")
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        if !event.is_right_click() {
                            this.eject_volume(eject_volume.clone(), window, cx);
                            cx.stop_propagation();
                        }
                    })),
            )
        })
        .when(mount_point.is_some(), |this| {
            let point = mount_point.clone().unwrap_or_default();
            this.child(painted_bounds(move |bounds| {
                place_drop_bounds.borrow_mut().insert(point.clone(), bounds);
            }))
        })
        .into_any_element()
    }

    pub(super) fn render_volume_menu(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let menu = self.sidebar.volume_menu.clone()?;
        // UDisks2 re-reads the list on every change, so the menu names a
        // device and looks it up again rather than trusting an index.
        let volume =
            self.volumes.read(cx).volumes().iter().find(|v| v.device == menu.device)?.clone();
        if !volume.is_mounted() && !volume.can_eject() {
            return None;
        }
        let rows = usize::from(volume.is_mounted()) + usize::from(volume.can_eject());
        let height = BOOKMARK_MENU_HEIGHT + 30.0 * (rows as f32 - 1.0);
        let (left, top) = clamp_to_window(menu.position, (BOOKMARK_MENU_WIDTH, height), window);
        let unmount = volume.clone();
        let eject = volume.clone();
        Some(
            popover("volume-context-menu", left, top, BOOKMARK_MENU_WIDTH, cx)
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.sidebar.volume_menu = None;
                    cx.notify();
                }))
                .when(volume.is_mounted(), |this| {
                    this.child(menu_row(("volume-menu-unmount", 0), "Unmount", true, cx).on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.sidebar.volume_menu = None;
                            this.unmount_volume(unmount.clone(), window, cx);
                        }),
                    ))
                })
                .when(volume.can_eject(), |this| {
                    this.child(menu_row(("volume-menu-eject", 1), "Eject", true, cx).on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.sidebar.volume_menu = None;
                            this.eject_volume(eject.clone(), window, cx);
                        }),
                    ))
                })
                .into_any_element(),
        )
    }

    /// Wide enough for the widest place label in the current font.
    pub(super) fn sidebar_width(&self, window: &Window, cx: &Context<Self>) -> Pixels {
        let font_size = cx.theme().font_size;
        let font = font(cx.theme().font_family.clone());
        let max_text_width = self
            .sidebar
            .places
            .iter()
            .map(|place| place.label.as_str())
            .chain(["Finding places…"])
            .map(|label| {
                window
                    .text_system()
                    .shape_line(
                        label.to_string().into(),
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
        // Outer padding + row padding + themed icon + gap, and a few pixels
        // over because rows draw at `text_base`, which the measure above may
        // round under. Footer controls need approximately the same remaining
        // width as a place icon.
        px((max_text_width + 84.0).clamp(MIN_PLACES_WIDTH, MAX_PLACES_WIDTH))
    }

    pub(super) fn render_sidebar(&mut self, width: Pixels, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let radius = cx.theme().radius;
        let settings_button = super::chrome::icon_button("open-settings", "⚙", true, cx)
            .tooltip(|window, cx| Tooltip::new("Settings").build(window, cx))
            .on_click(cx.listener(|this, _, window, cx| this.open_settings_dialog(window, cx)));
        let muted = |text: &'static str| {
            div().px_3().py_1().text_xs().text_color(colors.muted_foreground).child(text)
        };
        // A picker chooses from the filesystem; the Trash is where things
        // are not. Hiding the place is simpler than refusing at confirm.
        let is_picker = self.picker.is_some();
        let places = self
            .sidebar
            .places
            .clone()
            .into_iter()
            .enumerate()
            .filter(|(_, place)| !is_picker || !place.is_trash())
            .map(|(index, place)| self.render_place(index, place, cx))
            .collect::<Vec<_>>();
        let (bookmarks, bookmarks_loading) = {
            let store = self.bookmarks.read(cx);
            (store.bookmarks().to_vec(), store.is_loading())
        };
        let bookmark_rows = bookmarks
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, bookmark)| self.render_bookmark(index, bookmark, cx))
            .collect::<Vec<_>>();
        // No UDisks2, no section: the sidebar says nothing about drives it
        // cannot see rather than showing an empty heading.
        let devices = self.volumes.read(cx).available().then(|| {
            self.volumes
                .read(cx)
                .volumes()
                .to_vec()
                .into_iter()
                .enumerate()
                .map(|(index, volume)| self.render_volume(index, volume, cx))
                .collect::<Vec<_>>()
        });
        // Likewise no GVfs, no Network section.
        let network = self.render_network_rows(cx);
        let final_insertion = self.sidebar.bookmark_insertion == Some(bookmarks.len());
        let bookmark_region_bounds = self.sidebar.bookmark_region_bounds.clone();

        // Everything above the footer scrolls as one column: in a short
        // window the sections used to run on under the footer, and the gear
        // painted over whichever bookmark was unlucky enough to be there.
        let sections = div()
            .id("sidebar-sections")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap_2()
            .overflow_y_scroll()
            .child(div().text_sm().text_color(colors.muted_foreground).child("Places"))
            .children(places)
            .when(self.sidebar.places_loading, |this| this.child(muted("Finding places…")))
            .when_some(devices, |this, devices| {
                this.child(div().h(px(1.0)).my_1().bg(colors.sidebar_border))
                    .child(div().text_sm().text_color(colors.muted_foreground).child("Devices"))
                    .when(devices.is_empty(), |this| this.child(muted("No drives")))
                    .children(devices)
            })
            .when_some(network, |this, rows| {
                this.child(div().h(px(1.0)).my_1().bg(colors.sidebar_border))
                    .child(div().text_sm().text_color(colors.muted_foreground).child("Network"))
                    .children(rows)
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
                    .rounded(radius)
                    .can_drop(move |value, _, _| {
                        !bookmarks_loading
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
                            .sidebar
                            .bookmark_insertion
                            .unwrap_or(this.bookmarks.read(cx).bookmarks().len());
                        this.move_bookmark(drag.index, &drag.path, insertion, window, cx);
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
                    .children(bookmark_rows)
                    .when(bookmarks_loading, |this| this.child(muted("Loading bookmarks…")))
                    .when(!bookmarks_loading && bookmarks.is_empty(), |this| {
                        this.child(muted("Drag folders here"))
                    })
                    .child(insertion_marker(final_insertion, &colors))
                    .child(div().flex_1())
                    .child(painted_bounds(move |bounds| bookmark_region_bounds.set(Some(bounds)))),
            );

        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(width)
            .h_full()
            .p_4()
            .gap_2()
            .bg(colors.sidebar)
            .border_r_1()
            .border_color(colors.sidebar_border)
            .text_color(colors.sidebar_foreground)
            .child(sections)
            .child(h_flex().w_full().justify_end().child(settings_button))
            .into_any_element()
    }
}
