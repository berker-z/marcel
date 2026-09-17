//! Every command the browser answers: what it is bound to, whether it is
//! enabled right now, and what it does. Menus, shortcuts, and toolbar
//! buttons all arrive here, so enablement is decided once.

use std::path::Path;

use gpui::prelude::*;
use gpui::{
    App, Context, Div, Focusable as _, KeyBinding, KeyDownEvent, ScrollStrategy, Stateful, Window,
};
use gpui_component::{
    WindowExt as _,
    input::{InputState, SelectAll as InputSelectAll},
    notification::Notification,
};

use crate::{
    browse::entries::EntryKind,
    fsops::{TransferMode, archive::is_supported_archive},
};

use super::{DIRECTORY_ROW_HEIGHT, GRID_ROW_HEIGHT, Marcel, state::ViewMode};

pub const BROWSER_KEY_CONTEXT: &str = "MarcelBrowser";

/// Declares the browser's commands once: the GPUI action for each, its key
/// binding if it has one, and the `BrowserCommand` variant menus and
/// toolbar buttons use to reach the same code.
///
/// `on_window_key_down` handles the few keys that must keep working while
/// another surface holds focus (Ctrl+L, Ctrl+F, Escape in a picker).
macro_rules! browser_commands {
    ($( $name:ident $( = $key:literal )? ),* $(,)?) => {
        gpui::actions!(marcel, [$($name),*]);

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum BrowserCommand {
            $($name,)*
            /// Only reachable from the context menu.
            OpenInNewWindow,
        }

        pub fn init_key_bindings(cx: &mut App) {
            cx.bind_keys([
                $( $( KeyBinding::new($key, $name, Some(BROWSER_KEY_CONTEXT)), )? )*
            ]);
        }

        /// Route every action to the shared dispatcher.
        pub(super) fn bind_actions(element: Stateful<Div>, cx: &mut Context<Marcel>) -> Stateful<Div> {
            element $( .on_action(cx.listener(|this: &mut Marcel, _: &$name, window, cx| {
                this.on_action(BrowserCommand::$name, window, cx);
            })) )*
        }
    };
}

browser_commands! {
    MoveUp = "up",
    MoveDown = "down",
    MoveLeft = "left",
    MoveRight = "right",
    ExtendUp = "shift-up",
    ExtendDown = "shift-down",
    ExtendLeft = "shift-left",
    ExtendRight = "shift-right",
    ExtendToFirst = "shift-home",
    ExtendToLast = "shift-end",
    ExtendPageUp = "shift-pageup",
    ExtendPageDown = "shift-pagedown",
    SelectFirst = "home",
    SelectLast = "end",
    SelectPageUp = "pageup",
    SelectPageDown = "pagedown",
    ActivateSelection = "enter",
    OpenWithSelection,
    ClearSelection = "escape",
    GoToParent = "ctrl-up",
    GoBack = "ctrl-left",
    GoForward = "ctrl-right",
    SelectAll = "ctrl-a",
    CopySelection = "ctrl-c",
    CutSelection = "ctrl-x",
    PasteFiles = "ctrl-v",
    TrashSelection = "delete",
    RestoreSelection,
    DeletePermanently = "shift-delete",
    EmptyTrash,
    NewFolder = "ctrl-shift-n",
    OpenTerminal,
    RenameSelection = "f2",
    CompressSelection,
    ExtractSelection,
    UndoFileOperation = "ctrl-z",
    RedoFileOperation = "ctrl-y",
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SelectionMotion {
    Up,
    Down,
    Left,
    Right,
    First,
    Last,
    PageUp,
    PageDown,
}

impl BrowserCommand {
    /// The keyboard-selection motion this command is, and whether it extends
    /// the selection rather than replacing it.
    fn motion(self) -> Option<(SelectionMotion, bool)> {
        use SelectionMotion::*;
        Some(match self {
            Self::MoveUp => (Up, false),
            Self::MoveDown => (Down, false),
            Self::MoveLeft => (Left, false),
            Self::MoveRight => (Right, false),
            Self::SelectFirst => (First, false),
            Self::SelectLast => (Last, false),
            Self::SelectPageUp => (PageUp, false),
            Self::SelectPageDown => (PageDown, false),
            Self::ExtendUp => (Up, true),
            Self::ExtendDown => (Down, true),
            Self::ExtendLeft => (Left, true),
            Self::ExtendRight => (Right, true),
            Self::ExtendToFirst => (First, true),
            Self::ExtendToLast => (Last, true),
            Self::ExtendPageUp => (PageUp, true),
            Self::ExtendPageDown => (PageDown, true),
            _ => return None,
        })
    }
}

impl Marcel {
    /// Whether the folder shown can be changed from this window right now.
    fn can_mutate_here(&self, cx: &App) -> bool {
        !self.sidebar.browsing_trash && !self.operations_busy(cx) && self.directory.error.is_none()
    }

    fn selection_is_single(&self) -> bool {
        self.directory.selection.selected().len() == 1
            && self.directory.selection.primary().is_some()
    }

    /// Every selected Trash entry has a record to restore or purge from.
    fn selection_has_trash_records(&self) -> bool {
        self.directory
            .selection
            .selected()
            .iter()
            .all(|path| self.sidebar.trash_records.contains_key(path))
    }

    /// The one selected folder, if the selection is exactly that.
    pub(super) fn selected_directory(&self) -> Option<&Path> {
        if !self.selection_is_single() {
            return None;
        }
        let entry = self.primary_entry()?;
        (entry.kind == EntryKind::Directory).then_some(entry.path.as_path())
    }

    /// The one selected archive, if the selection is exactly that.
    pub(super) fn selected_archive(&self) -> bool {
        self.selection_is_single()
            && self.primary_entry().is_some_and(|entry| {
                entry.kind != EntryKind::Directory && is_supported_archive(&entry.path)
            })
    }

    pub(super) fn command_enabled(&self, command: BrowserCommand, cx: &App) -> bool {
        use BrowserCommand::*;
        let trash = self.sidebar.browsing_trash;
        let busy = self.operations_busy(cx);
        let operations = self.operations.read(cx);
        match command {
            GoToParent => !trash && self.directory.current_dir.parent().is_some(),
            GoBack => self.history.can_go_back(),
            GoForward => self.history.can_go_forward(),
            ActivateSelection => self
                .primary_entry()
                .is_some_and(|entry| !trash || entry.kind != EntryKind::Directory),
            OpenWithSelection => self
                .primary_entry()
                .is_some_and(|entry| !entry.navigable && entry.kind != EntryKind::Directory),
            ClearSelection => self.ui.rename.is_some() || self.has_selection(),
            // Staging the clipboard is instantaneous and mutates nothing, so a
            // running operation is no reason to grey it out — only Paste has
            // to wait for the busy lock.
            CopySelection | CutSelection => !trash && self.has_selection(),
            PasteFiles => {
                self.can_mutate_here(cx)
                    && operations.clipboard().is_some_and(|clipboard| !clipboard.paths.is_empty())
            }
            NewFolder => self.can_mutate_here(cx),
            OpenTerminal => {
                !trash
                    && !self.directory.loading
                    && self.directory.error.is_none()
                    && self.directory.current_dir.is_dir()
            }
            // Folders only. A second window showing a file is not a thing
            // Marcel can do, and a Trash entry's backing path is not somewhere
            // the user should be browsing.
            OpenInNewWindow => !trash && self.selected_directory().is_some(),
            RenameSelection => self.can_mutate_here(cx) && self.selection_is_single(),
            CompressSelection => self.can_mutate_here(cx) && self.has_selection(),
            ExtractSelection => self.can_mutate_here(cx) && self.selected_archive(),
            TrashSelection => !trash && !busy && self.has_selection(),
            RestoreSelection => {
                trash && !busy && self.has_selection() && self.selection_has_trash_records()
            }
            DeletePermanently => {
                !busy && self.has_selection() && (!trash || self.selection_has_trash_records())
            }
            EmptyTrash => trash && !busy && !self.sidebar.trash_records.is_empty(),
            UndoFileOperation => operations.can_undo(),
            RedoFileOperation => operations.can_redo(),
            // Every keyboard motion, and Select All.
            _ => !self.directory.visible_entries.is_empty(),
        }
    }

    /// A key binding fired. Escape is the one key with a stack of meanings:
    /// it closes whatever is frontmost before it clears anything.
    fn on_action(&mut self, command: BrowserCommand, window: &mut Window, cx: &mut Context<Self>) {
        if command != BrowserCommand::ClearSelection {
            self.execute(command, window, cx);
            return;
        }
        // A context menu is the only thing above the listing; clearing the
        // selection out from under it left the menu describing a file it no
        // longer had, with every action greyed out.
        if self.ui.entry_menu.is_some() {
            self.dismiss_entry_menu(cx);
            return;
        }
        // Escape is easy to press for some other reason, so it only cancels an
        // operation from the window that started it (or from anywhere once
        // that window is gone). The progress card's Cancel stays available on
        // every window.
        let operations = self.operations.read(cx);
        if operations.is_busy()
            && operations.can_cancel_from(Self::origin(window), cx)
            && operations.request_cancel()
        {
            window.push_notification(Notification::info("Cancelling file operation…"), cx);
            return;
        }
        if !self.directory.filter_query.is_empty() {
            self.clear_filter(window, cx);
        } else if self.picker.is_some() && self.ui.rename.is_none() {
            // A dialog answers Escape by going away, not by deselecting. This
            // is the binding's path; `on_window_key_down` covers focus that
            // has no binding, such as the name field.
            self.cancel_picker(window, cx);
        } else {
            self.execute(BrowserCommand::ClearSelection, window, cx);
        }
    }

    pub(super) fn execute(
        &mut self,
        command: BrowserCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use BrowserCommand::*;
        if !self.command_enabled(command, cx) {
            return;
        }
        if let Some((motion, extend)) = command.motion() {
            self.move_keyboard_selection(motion, extend, cx);
            return;
        }
        match command {
            ActivateSelection => self.activate_primary(window, cx),
            OpenWithSelection => self.open_primary_with(cx),
            ClearSelection => {
                if self.ui.rename.is_some() {
                    self.cancel_rename(window, cx);
                } else {
                    self.clear_selection();
                    cx.notify();
                }
            }
            GoToParent => self.go_up(cx),
            GoBack => self.go_back(cx),
            GoForward => self.go_forward(cx),
            SelectAll => self.select_all_entries(cx),
            CopySelection => self.stage_selection(TransferMode::Copy, window, cx),
            CutSelection => self.stage_selection(TransferMode::Move, window, cx),
            PasteFiles => self.start_paste(window, cx),
            TrashSelection => self.start_trash_selection(window, cx),
            RestoreSelection => self.start_restore_selection(window, cx),
            DeletePermanently => self.open_permanent_delete_dialog(window, cx),
            EmptyTrash => self.open_empty_trash_dialog(window, cx),
            NewFolder => self.open_new_folder_dialog(window, cx),
            OpenTerminal => self.open_terminal(window, cx),
            OpenInNewWindow => self.open_selection_in_new_window(cx),
            RenameSelection => self.begin_rename(window, cx),
            CompressSelection => self.open_compress_dialog(window, cx),
            ExtractSelection => self.start_extract_selection(window, cx),
            UndoFileOperation => {
                self.with_operations(window, cx, |ops, origin, cx| ops.start_undo(origin, cx))
            }
            RedoFileOperation => {
                self.with_operations(window, cx, |ops, origin, cx| ops.start_redo(origin, cx))
            }
            _ => {}
        }
    }

    fn move_keyboard_selection(
        &mut self,
        motion: SelectionMotion,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .directory
            .selection
            .primary()
            .and_then(|path| self.directory.visible_position(path));
        let columns = match self.ui.view_mode {
            ViewMode::List => 1,
            ViewMode::Grid => self.grid_columns(),
        };
        let Some(target) = selection_target(
            current,
            self.directory.visible_entries.len(),
            columns,
            self.keyboard_page_size(columns),
            self.ui.view_mode,
            motion,
        ) else {
            return;
        };
        if current == Some(target) && !extend {
            return;
        }
        let Some(entry) = self.directory.visible_entry(target).cloned() else {
            return;
        };
        if extend {
            let ordered = self.directory.visible_paths();
            self.directory.selection.select_range(entry.path.clone(), &ordered, false);
        } else {
            self.directory.selection.select_only(entry.path.clone());
        }
        self.ui.directory_scroll.scroll_to_item(target / columns.max(1), ScrollStrategy::Center);
        self.start_preview(entry, cx);
        cx.notify();
    }

    fn keyboard_page_size(&self, columns: usize) -> usize {
        let height = self
            .drag
            .browser_bounds
            .get()
            .map(|bounds| f32::from(bounds.size.height))
            .unwrap_or(DIRECTORY_ROW_HEIGHT);
        match self.ui.view_mode {
            ViewMode::List => (height / DIRECTORY_ROW_HEIGHT).floor().max(1.0) as usize,
            ViewMode::Grid => (height / GRID_ROW_HEIGHT).floor().max(1.0) as usize * columns.max(1),
        }
    }

    pub(super) fn activate_primary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry) = self.primary_entry().cloned() {
            self.open_entry(entry, window, cx);
        }
    }

    fn select_all_entries(&mut self, cx: &mut Context<Self>) {
        let ordered = self.directory.visible_paths();
        self.directory.selection.select_all(&ordered);
        self.preview_primary(cx);
        cx.notify();
    }

    /// Preview whatever is primary now, if anything.
    pub(super) fn preview_primary(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.primary_entry().cloned() {
            self.start_preview(entry, cx);
        }
    }

    pub(super) fn clear_selection(&mut self) {
        self.directory.selection.clear();
        self.drag.marquee = None;
        self.drag.marquee_scroll_task.take();
        self.preview.clear();
    }

    /// Keys that have to work whatever holds focus: Ctrl+L and Ctrl+F reach
    /// their fields from anywhere, Escape dismisses whatever is frontmost, and
    /// any printable character starts filtering.
    pub(super) fn on_window_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.route_key(event, window, cx) {
            cx.stop_propagation();
        }
    }

    /// Returns whether the key was consumed here.
    fn route_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let stroke = &event.keystroke;
        let key = stroke.key.as_str();
        let ctrl = |letter: &str| {
            stroke.modifiers.control
                && !stroke.modifiers.alt
                && stroke.key.eq_ignore_ascii_case(letter)
        };
        let search_focused = self.ui.search_input.focus_handle(cx).is_focused(window);
        let location_focused = self.ui.location_input.focus_handle(cx).is_focused(window);
        let browser_focused = self.browser_focus.is_focused(window);
        let input_focused = window.has_focused_input(cx);
        let filtering = !self.directory.filter_query.is_empty();

        // A context menu is in front of every surface this function routes
        // between, so Escape has to reach it before any of them.
        if key == "escape" && self.ui.entry_menu.is_some() {
            self.dismiss_entry_menu(cx);
            return true;
        }
        // Escape in a picker dismisses the dialog, as it does everywhere else
        // on the desktop — once nothing in front of the listing wants it: an
        // edit in progress, a filter to clear, a dialog of its own.
        if key == "escape"
            && self.picker.is_some()
            && !location_focused
            && self.ui.rename.is_none()
            && !filtering
            && !window.has_active_dialog(cx)
        {
            self.cancel_picker(window, cx);
            return true;
        }
        if location_focused {
            if ctrl("l") {
                window.dispatch_action(Box::new(InputSelectAll), cx);
                return true;
            }
            if key == "escape" {
                self.cancel_location_edit(window, cx);
                return true;
            }
            return false;
        }
        if ctrl("l") && (!input_focused || search_focused) {
            self.begin_location_edit(window, cx);
            return true;
        }
        // Type-to-filter is global browser chrome, not an input interceptor.
        // Dialog fields and inline editors keep every editing key while they
        // own focus.
        if input_focused && !search_focused {
            return false;
        }
        if ctrl("f") {
            self.focus_search(window, cx);
            return true;
        }
        let motion = match key {
            "up" => Some(BrowserCommand::MoveUp),
            "down" => Some(BrowserCommand::MoveDown),
            _ => None,
        };
        if search_focused {
            return motion.is_some_and(|command| {
                self.execute(command, window, cx);
                true
            });
        }
        if filtering {
            match key {
                "backspace" => {
                    let mut query = self.directory.filter_query.clone();
                    query.pop();
                    self.replace_search_text(query, window, cx);
                    self.focus_search(window, cx);
                    return true;
                }
                "escape" if !browser_focused => {
                    self.clear_filter(window, cx);
                    return true;
                }
                "enter" if !browser_focused => {
                    self.activate_primary(window, cx);
                    return true;
                }
                "up" | "down" if !browser_focused => {
                    if let Some(command) = motion {
                        self.execute(command, window, cx);
                    }
                    return true;
                }
                _ => {}
            }
        }
        let modifiers = &stroke.modifiers;
        if modifiers.control || modifiers.alt || modifiers.platform || modifiers.function {
            return false;
        }
        let Some(text) = stroke.key_char.as_deref() else {
            return false;
        };
        if text.chars().any(char::is_control)
            || (!filtering && text.chars().all(char::is_whitespace))
        {
            return false;
        }
        let query = format!("{}{text}", self.directory.filter_query);
        self.replace_search_text(query, window, cx);
        self.focus_search(window, cx);
        true
    }

    // Type-to-filter.

    pub(super) fn on_search_input_event(
        &mut self,
        input: &gpui::Entity<InputState>,
        event: &gpui_component::input::InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gpui_component::input::InputEvent;
        match event {
            InputEvent::Change => {
                let query = input.read(cx).value().to_string();
                let cleared = query.is_empty() && !self.directory.filter_query.is_empty();
                self.set_filter_query(query, cx);
                if cleared {
                    self.focus_browser(window, cx);
                }
            }
            InputEvent::PressEnter { .. } => self.activate_primary(window, cx),
            InputEvent::Focus | InputEvent::Blur => {}
        }
    }

    pub(super) fn focus_search(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.ui.search_input.update(cx, |input, cx| input.focus(window, cx));
    }

    fn replace_search_text(&mut self, value: String, window: &mut Window, cx: &mut Context<Self>) {
        // InputState emits Change asynchronously. Keep the directory session,
        // which is the authoritative value used by render(), in sync first so
        // a render triggered by moving focus cannot restore the old query and
        // discard the first type-to-filter character.
        self.set_filter_query(value.clone(), cx);
        self.ui.search_input.update(cx, |input, cx| input.set_value(value, window, cx));
    }

    pub(super) fn clear_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_search_text(String::new(), window, cx);
        self.focus_browser(window, cx);
    }

    pub(super) fn set_filter_query(&mut self, query: String, cx: &mut Context<Self>) {
        if let Some(reconcile) = self.directory.set_filter_query(query) {
            self.reprojected(reconcile, cx);
        }
    }

    pub(super) fn set_show_hidden(&mut self, show_hidden: bool, cx: &mut Context<Self>) {
        if let Some(reconcile) = self.directory.set_show_hidden(show_hidden) {
            self.persist_browser_state();
            self.reprojected(reconcile, cx);
        }
    }

    /// The visible set changed shape: every painted rectangle is stale and the
    /// listing starts from the top.
    fn reprojected(
        &mut self,
        reconcile: crate::browse::directory_session::ReconcileSelection,
        cx: &mut Context<Self>,
    ) {
        self.apply_selection_reconcile(reconcile, cx);
        self.ui.directory_scroll = gpui::UniformListScrollHandle::new();
        self.drag.reset_geometry();
        cx.notify();
    }
}

pub(super) fn selection_target(
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
            return match motion {
                // Left and Right are deliberate no-ops in List view, with or
                // without a selection; answering them from nothing jumped the
                // selection to the end of the folder.
                SelectionMotion::Left | SelectionMotion::Right if view_mode == ViewMode::List => {
                    None
                }
                SelectionMotion::Up
                | SelectionMotion::Left
                | SelectionMotion::Last
                | SelectionMotion::PageUp => Some(last),
                _ => Some(0),
            };
        }
    };
    let columns = columns.max(1);
    let page_size = page_size.max(1);

    Some(match motion {
        SelectionMotion::Up => match view_mode {
            ViewMode::List => current.saturating_sub(1),
            // The top row has nowhere up to go; jumping to index 0 silently
            // changed columns.
            ViewMode::Grid if current < columns => current,
            ViewMode::Grid => current - columns,
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn every_bound_key_is_a_command_with_a_motion_or_an_effect() {
        assert_eq!(
            BrowserCommand::ExtendPageDown.motion(),
            Some((SelectionMotion::PageDown, true))
        );
        assert_eq!(BrowserCommand::MoveLeft.motion(), Some((SelectionMotion::Left, false)));
        assert_eq!(BrowserCommand::PasteFiles.motion(), None);
    }
}
