//! Context menus, described as data and drawn by one renderer.
//!
//! gpui-component hardcodes `shadow_lg()` in PopupMenu's private popover
//! style. It renders as opaque bands on our Linux target and exposes no
//! override, so this small shell is Marcel's own. Every active item still goes
//! through the shared command dispatcher, so a menu can never enable
//! something a shortcut would refuse.

use std::path::Path;

use gpui::prelude::*;
use gpui::{
    AnyElement, ClickEvent, Context, Div, IntoElement, MouseDownEvent, Pixels, Point, Stateful,
    Window, div, px,
};
use gpui_component::{ActiveTheme as _, h_flex};

use crate::{browse::entries::SortKey, desktop::picker::PickerMode};

use super::{
    Marcel,
    actions::BrowserCommand,
    state::{ContextMenuTarget, EntryMenu},
};

const MENU_WIDTH: f32 = 208.0;
const MENU_ROW_HEIGHT: f32 = 28.0;
const MENU_SEPARATOR_HEIGHT: f32 = 9.0;
const MENU_CHROME_HEIGHT: f32 = 10.0;
pub(super) const MENU_MARGIN: f32 = 8.0;

/// What one line of a context menu is.
pub(super) enum MenuItem {
    /// A browser command, enabled exactly when the command is.
    Command {
        label: &'static str,
        shortcut: Option<&'static str>,
        command: BrowserCommand,
    },
    /// Something only the menu offers.
    Action {
        label: &'static str,
        checked: bool,
        run: MenuAction,
    },
    /// Not built yet; shown greyed so the shape of the menu is known.
    Planned(&'static str),
    Separator,
}

use MenuItem::{Action, Planned, Separator};

/// What an `Action` item does when chosen.
type MenuAction = fn(&mut Marcel, &mut Window, &mut Context<Marcel>);

fn command(
    label: &'static str,
    shortcut: Option<&'static str>,
    command: BrowserCommand,
) -> MenuItem {
    MenuItem::Command { label, shortcut, command }
}

fn menu_height(items: &[MenuItem]) -> f32 {
    items.iter().fold(MENU_CHROME_HEIGHT, |height, item| {
        height
            + match item {
                Separator => MENU_SEPARATOR_HEIGHT,
                _ => MENU_ROW_HEIGHT,
            }
    })
}

/// Keep a popover of `size` inside the window, near `position`.
pub(super) fn clamp_to_window(
    position: Point<Pixels>,
    size: (f32, f32),
    window: &Window,
) -> (f32, f32) {
    let window_size = window.bounds().size;
    let clamp = |wanted: Pixels, extent: f32, limit: Pixels| {
        f32::from(wanted).min((f32::from(limit) - extent - MENU_MARGIN).max(0.0)).max(MENU_MARGIN)
    };
    (clamp(position.x, size.0, window_size.width), clamp(position.y, size.1, window_size.height))
}

/// The popover shell every menu shares.
pub(super) fn popover(
    id: &'static str,
    left: f32,
    top: f32,
    width: f32,
    cx: &mut Context<Marcel>,
) -> Stateful<Div> {
    let colors = cx.theme().colors;
    div()
        .id(id)
        .absolute()
        .left(px(left))
        .top(px(top))
        .w(px(width))
        .p_1()
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(colors.border)
        .bg(colors.popover)
        .text_color(colors.popover_foreground)
        .text_sm()
        .occlude()
}

/// One menu row. Popovers and `accent` share the raised surface in Marcel
/// palettes, so the active-list tint is the hover colour that stays visible
/// on every theme.
pub(super) fn menu_row(
    id: (&'static str, usize),
    label: &'static str,
    enabled: bool,
    cx: &mut Context<Marcel>,
) -> Stateful<Div> {
    let colors = cx.theme().colors;
    h_flex()
        .id(id)
        .h(px(MENU_ROW_HEIGHT))
        .px_3()
        .rounded(cx.theme().radius)
        .when(enabled, |this| this.cursor_pointer().hover(|this| this.bg(colors.list_active)))
        .when(!enabled, |this| this.text_color(colors.muted_foreground))
        .child(label)
}

impl Marcel {
    fn entry_menu_items(&self, cx: &Context<Self>) -> Vec<MenuItem> {
        use BrowserCommand::*;
        let trash = self.sidebar.browsing_trash;
        let mut items = vec![command("Open", Some("Enter"), ActivateSelection)];
        // Shown only for a folder, and only where a second window makes
        // sense, rather than shown disabled beside every file.
        if self.command_enabled(OpenInNewWindow, cx) {
            items.push(command("Open in New Window", None, OpenInNewWindow));
        }
        items.extend([
            command("Open With…", None, OpenWithSelection),
            Separator,
            command("Cut", Some("Ctrl+X"), CutSelection),
            command("Copy", Some("Ctrl+C"), CopySelection),
            command("Paste", Some("Ctrl+V"), PasteFiles),
            command("Duplicate", Some("Ctrl+D"), DuplicateSelection),
            Separator,
            command("Rename…", Some("F2"), RenameSelection),
            command("Move To…", None, MoveToSelection),
            if trash {
                command("Restore", None, RestoreSelection)
            } else {
                command("Move to Trash", Some("Delete"), TrashSelection)
            },
            command("Delete", Some("Shift+Delete"), DeletePermanently),
            Separator,
            Planned("Create Link"),
            command("Compress…", None, CompressSelection),
        ]);
        if self.selected_archive() {
            items.push(command("Extract", None, ExtractSelection));
        }
        items.extend([
            Action {
                label: "Copy Path",
                checked: false,
                run: |this, _, cx| this.copy_selected_paths(cx),
            },
            Separator,
            command("Properties", Some("Ctrl+I"), ShowProperties),
        ]);
        items
    }

    fn directory_menu_items(&self) -> Vec<MenuItem> {
        use BrowserCommand::*;
        let mut items = vec![
            command("New Folder", Some("Ctrl+Shift+N"), NewFolder),
            command("New File", None, NewFile),
            command("Paste", Some("Ctrl+V"), PasteFiles),
            command("Undo", Some("Ctrl+Z"), UndoFileOperation),
            command("Redo", Some("Ctrl+Y"), RedoFileOperation),
            Separator,
            command("Select All", Some("Ctrl+A"), SelectAll),
            Action {
                label: "Refresh",
                checked: false,
                run: |this, _, cx| this.start_directory_load(false, cx),
            },
            Separator,
            Action {
                label: "Show Hidden Files",
                checked: self.directory.show_hidden,
                run: |this, _, cx| this.set_show_hidden(!this.directory.show_hidden, cx),
            },
            Separator,
            command("Open in Terminal", None, OpenTerminal),
            Action {
                label: "Copy Location",
                checked: false,
                run: |this, _, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                        this.directory.current_dir.display().to_string(),
                    ));
                },
            },
            command("Properties", Some("Ctrl+I"), ShowProperties),
        ];
        if self.sidebar.browsing_trash {
            items.extend([Separator, command("Empty Trash…", None, EmptyTrash)]);
        }
        items
    }

    /// The sort picker: one checked key, and whether the order is reversed.
    fn sort_menu_items(&self) -> Vec<MenuItem> {
        let order = self.directory.sort;
        let key = |label: &'static str, key: SortKey, run: MenuAction| Action {
            label,
            checked: order.key == key,
            run,
        };
        vec![
            key("Name", SortKey::Name, |this, _, cx| this.set_sort_key(SortKey::Name, cx)),
            key("Modified", SortKey::Modified, |this, _, cx| {
                this.set_sort_key(SortKey::Modified, cx)
            }),
            key("Size", SortKey::Size, |this, _, cx| this.set_sort_key(SortKey::Size, cx)),
            key("Kind", SortKey::Kind, |this, _, cx| this.set_sort_key(SortKey::Kind, cx)),
            Separator,
            Action {
                label: "Reverse Order",
                checked: order.descending,
                run: |this, _, cx| this.reverse_sort(cx),
            },
        ]
    }

    /// Open the sort picker at `position`, unless the press that became this
    /// click found it open, in which case the popover has already closed it.
    pub(super) fn toggle_sort_menu(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if !std::mem::take(&mut self.ui.sort_menu_was_open) {
            self.ui.entry_menu = Some(EntryMenu { position, target: ContextMenuTarget::Sort });
        }
        cx.notify();
    }

    /// Every selected path on the clipboard, one per line.
    pub(super) fn copy_selected_paths(&mut self, cx: &mut Context<Self>) {
        let paths = self
            .selected_paths()
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        if !paths.is_empty() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(paths));
        }
    }

    pub(super) fn render_entry_menu(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let menu = self.ui.entry_menu?;
        let colors = cx.theme().colors;
        let (id, items) = match menu.target {
            ContextMenuTarget::Entry => ("entry-context-menu", self.entry_menu_items(cx)),
            ContextMenuTarget::Sort => ("sort-menu", self.sort_menu_items()),
            ContextMenuTarget::CurrentDirectory => {
                ("directory-context-menu", self.directory_menu_items())
            }
        };
        let (left, top) = clamp_to_window(menu.position, (MENU_WIDTH, menu_height(&items)), window);
        let rows = items
            .into_iter()
            .enumerate()
            .map(|(index, item)| match item {
                Separator => div().h(px(1.0)).mx_1().my_1().bg(colors.border).into_any_element(),
                Planned(label) => div()
                    .flex()
                    .h(px(MENU_ROW_HEIGHT))
                    .items_center()
                    .px_3()
                    .text_color(colors.muted_foreground)
                    .child(format!("– {label}"))
                    .into_any_element(),
                MenuItem::Command { label, shortcut, command } => {
                    let enabled = self.command_enabled(command, cx);
                    menu_row(("menu-row", index), label, enabled, cx)
                        .when(enabled, |this| {
                            this.on_click(cx.listener(move |this, _, window, cx| {
                                this.ui.entry_menu = None;
                                this.execute(command, window, cx);
                            }))
                        })
                        .child(div().flex_1())
                        .when_some(shortcut, |this, shortcut| {
                            this.child(
                                div().text_xs().text_color(colors.muted_foreground).child(shortcut),
                            )
                        })
                        .into_any_element()
                }
                Action { label, checked, run } => menu_row(("menu-row", index), label, true, cx)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.ui.entry_menu = None;
                        run(this, window, cx);
                        cx.notify();
                    }))
                    .child(div().flex_1())
                    .when(checked, |this| this.child("✓"))
                    .into_any_element(),
            })
            .collect::<Vec<_>>();
        Some(
            popover(id, left, top, MENU_WIDTH, cx)
                .on_mouse_down_out(cx.listener(|this, _, _, cx| this.dismiss_entry_menu(cx)))
                .children(rows)
                .into_any_element(),
        )
    }

    pub(super) fn activate_entry(
        &mut self,
        path: &Path,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Right-click selection is resolved on mouse-down before the context
        // menu is built. Do not apply ordinary click modifiers a second time.
        if event.is_right_click() {
            return;
        }
        let Some(entry) = self.directory.entry(path).cloned() else {
            return;
        };
        // Clicking a file in a save dialog proposes its name, the way every
        // save dialog does. Only clicks: a name the caller supplied must not
        // be overwritten by the listing settling on a first row.
        if !entry.navigable
            && let Some(input) = self
                .picker
                .as_ref()
                .filter(|picker| picker.mode == PickerMode::SaveFile)
                .and_then(|picker| picker.name_input.clone())
        {
            let name = entry.name.clone();
            input.update(cx, |input, cx| input.set_value(name, window, cx));
        }

        let modifiers = event.modifiers();
        let ordered = self.directory.visible_paths();
        if modifiers.shift {
            self.directory.selection.select_range(
                entry.path.clone(),
                &ordered,
                modifiers.secondary(),
            );
        } else if modifiers.secondary() {
            self.directory.selection.toggle(entry.path.clone(), &ordered);
        } else {
            self.directory.selection.select_only(entry.path.clone());
        }
        if self.directory.selection.primary() == Some(&entry.path) {
            self.start_preview(entry.clone(), cx);
        } else {
            self.preview.clear();
            cx.notify();
        }
        if event.click_count() >= 2 {
            self.open_entry(entry, window, cx);
        }
    }

    pub(super) fn prepare_entry_context_menu(
        &mut self,
        path: &Path,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.directory.entry(path).cloned() else {
            return;
        };
        if self.directory.selection.is_selected(path) {
            self.directory.selection.make_primary(path);
        } else {
            self.directory.selection.select_only(path.to_path_buf());
        }
        self.ui.entry_menu = Some(EntryMenu { position, target: ContextMenuTarget::Entry });
        self.start_preview(entry, cx);
    }

    pub(super) fn prepare_directory_context_menu(
        &mut self,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        if !self.on_empty_browser_space(event.position) {
            return;
        }
        self.clear_selection();
        self.ui.entry_menu = Some(EntryMenu {
            position: event.position,
            target: ContextMenuTarget::CurrentDirectory,
        });
        cx.notify();
    }

    pub(super) fn dismiss_entry_menu(&mut self, cx: &mut Context<Self>) {
        if self.ui.entry_menu.take().is_some() {
            cx.notify();
        }
    }
}
