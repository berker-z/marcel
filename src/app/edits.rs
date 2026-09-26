//! The window's side of every mutation: gather what is selected, ask when
//! the operation needs a name or a confirmation, then hand the work to the
//! application's operation owner.

use std::{
    cell::RefCell,
    collections::HashSet,
    path::{Path, PathBuf},
    rc::Rc,
};

use gpui::prelude::*;
use gpui::{App, Context, Entity, Task, Window, div, px};
use gpui_component::{
    WindowExt as _,
    button::{Button, ButtonVariant},
    dialog::DialogButtonProps,
    input::{Input, InputEvent, InputState},
    notification::Notification,
};

use crate::{
    bookmarks::Bookmark,
    browse::entries::{FileEntry, display_filename},
    desktop::{
        launch::{LocationTarget, resolve_location},
        picker::{PickerMode, PickerResponse},
        places::Place,
    },
    fsops::{TransferMode, archive::default_zip_name, validate_entry_name},
    names::{display_path_name, select_stem},
    operations::{FileClipboard, OperationProgressKind},
    surface::Report,
};

use super::{
    Marcel,
    dialogs::{Confirm, NameDialog, footer},
    location::{breadcrumbs, compact, crumb_bar},
    navigation::unblock,
    pointer::accepted_external_drop_paths,
    state::RenameEdit,
};

impl Marcel {
    pub(super) fn stage_selection(
        &mut self,
        mode: TransferMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        let count = paths.len();
        // The clipboard is the desktop's where the compositor lets Marcel
        // reach it, and the application's otherwise, so cutting here and
        // pasting in another window, or another file manager, is one gesture.
        self.operations.update(cx, |operations, _| {
            operations.set_clipboard(Some(FileClipboard { mode, paths }));
        });
        self.ui.entry_menu = None;
        let verb = match mode {
            TransferMode::Copy => "Copied",
            TransferMode::Move => "Cut",
        };
        window.push_notification(
            Notification::success(format!("{verb} {count} item(s) to the clipboard")),
            cx,
        );
        cx.notify();
    }

    pub(super) fn start_paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(clipboard) = self.operations.read(cx).clipboard() else {
            return;
        };
        let destination = self.directory.current_dir.clone();
        self.start_transfer(
            clipboard.paths.clone(),
            destination,
            clipboard.mode,
            Some(clipboard),
            window,
            cx,
        );
    }

    /// Move the selection to the Trash, or, where there is no Trash to move
    /// it to (a network share, a read-only stick), offer to delete it for
    /// good instead, which is what Nautilus asks in the same spot.
    ///
    /// The check reads the mount table and touches the filesystem, and on a
    /// stalled share that can hang, so it runs off the foreground like the
    /// operation it precedes.
    pub(super) fn start_trash_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        self.ui.entry_menu = None;
        let checked = paths.clone();
        let check = unblock(cx, move || crate::fsops::trash::trash_unavailable_for(&checked));
        cx.spawn_in(window, async move |this, window| {
            let unavailable = check.await;
            let _ = this.update_in(window, |this, window, cx| match unavailable {
                None => this.with_operations(window, cx, |ops, origin, cx| {
                    ops.start_trash(paths, origin, cx);
                }),
                Some(reason) => this.offer_permanent_delete_instead(paths, reason, window, cx),
            });
        })
        .detach();
    }

    fn offer_permanent_delete_instead(
        &mut self,
        paths: Vec<PathBuf>,
        reason: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let subject = match paths.as_slice() {
            [only] => format!("“{}” cannot", display_path_name(only)),
            _ => format!("These {} items cannot", paths.len()),
        };
        self.confirm(
            window,
            cx,
            Confirm {
                title: "No Trash Here",
                description: format!(
                    "{subject} be moved to the Trash: {reason}. Delete permanently instead?"
                ),
                note: Some("This action cannot be undone.".to_string()),
                action: "Delete Permanently",
                danger: true,
            },
            move |this, window, cx| {
                let paths = paths.clone();
                this.with_operations(window, cx, |ops, origin, cx| {
                    ops.start_permanent_delete(
                        paths,
                        None,
                        OperationProgressKind::Delete,
                        origin,
                        cx,
                    );
                });
            },
        );
    }

    /// The Trash records behind the selection, in visible order.
    fn selected_trash_records(&self) -> Vec<crate::fsops::trash::TrashRecord> {
        self.selected_paths()
            .into_iter()
            .filter_map(|path| self.sidebar.trash_records.get(&path).cloned())
            .collect()
    }

    pub(super) fn start_restore_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let records = self.selected_trash_records();
        if records.is_empty() {
            return;
        }
        self.ui.entry_menu = None;
        self.with_operations(window, cx, |ops, origin, cx| ops.start_restore(records, origin, cx));
    }

    pub(super) fn open_permanent_delete_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        let trash_records = self.sidebar.browsing_trash.then(|| self.selected_trash_records());
        if trash_records.as_ref().is_some_and(Vec::is_empty) {
            return;
        }
        let description = match paths.as_slice() {
            [only] => format!("Permanently delete “{}”?", display_path_name(only)),
            _ => format!("Permanently delete {} selected items?", paths.len()),
        };
        self.confirm(
            window,
            cx,
            Confirm {
                title: "Delete Permanently",
                description,
                note: Some("This action cannot be undone.".to_string()),
                action: "Delete Permanently",
                danger: true,
            },
            move |this, window, cx| {
                let (paths, trash_records) = (paths.clone(), trash_records.clone());
                this.with_operations(window, cx, |ops, origin, cx| {
                    ops.start_permanent_delete(
                        paths,
                        trash_records,
                        OperationProgressKind::Delete,
                        origin,
                        cx,
                    );
                });
            },
        );
    }

    pub(super) fn open_empty_trash_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let records = self.sidebar.trash_records.values().cloned().collect::<Vec<_>>();
        if records.is_empty() {
            return;
        }
        // Emptying what Marcel could enumerate is still the right action, but
        // calling it "empty the Trash" when part of it was never read would
        // promise something this operation cannot deliver.
        let unreadable = self.sidebar.unreadable_trash_entries;
        let mut description =
            format!("Permanently delete all {} item(s) currently shown in Trash?", records.len());
        if unreadable > 0 {
            description.push_str(&format!(
                "\n{unreadable} further Trash entr(y/ies) could not be read and will be left behind."
            ));
        }
        self.confirm(
            window,
            cx,
            Confirm {
                title: "Empty Trash",
                description,
                note: Some("This action cannot be undone.".to_string()),
                action: "Empty Trash",
                danger: true,
            },
            move |this, window, cx| {
                let records = records.clone();
                this.with_operations(window, cx, |ops, origin, cx| {
                    ops.start_permanent_delete(
                        Vec::new(),
                        Some(records),
                        OperationProgressKind::EmptyTrash,
                        origin,
                        cx,
                    );
                });
            },
        );
    }

    pub(super) fn start_drag_move(
        &mut self,
        paths: Vec<PathBuf>,
        destination: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar.bookmark_insertion = None;
        self.start_transfer(paths, destination, TransferMode::Move, None, window, cx);
    }

    pub(super) fn start_external_copy(
        &mut self,
        paths: &[PathBuf],
        destination: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(paths) = accepted_external_drop_paths(paths, &destination) else {
            window.push_notification(
                Notification::error("Those files cannot be copied to this folder"),
                cx,
            );
            return;
        };
        self.sidebar.bookmark_insertion = None;
        self.start_transfer(paths, destination, TransferMode::Copy, None, window, cx);
    }

    fn start_transfer(
        &mut self,
        sources: Vec<PathBuf>,
        destination: PathBuf,
        mode: TransferMode,
        clipboard: Option<FileClipboard>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ui.entry_menu = None;
        self.with_operations(window, cx, |ops, origin, cx| {
            ops.start_transfer(sources, destination, mode, clipboard, origin, cx);
        });
    }

    pub(super) fn cancel_active_operation(&mut self, cx: &mut Context<Self>) {
        if self.operations.read(cx).request_cancel() {
            cx.notify();
        }
    }

    // Rename.

    pub(super) fn begin_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ui.entry_menu = None;
        let Some(entry) = self.primary_entry() else {
            return;
        };
        let (path, name, is_directory) = (
            entry.path.clone(),
            entry.name.clone(),
            entry.kind == crate::browse::entries::EntryKind::Directory,
        );
        let input = cx.new(|cx| InputState::new(window, cx).default_value(name));
        let subscription =
            cx.subscribe_in(&input, window, |this, input, event: &InputEvent, window, cx| {
                match event {
                    InputEvent::PressEnter { .. } => this.submit_rename(input, false, window, cx),
                    InputEvent::Blur => this.submit_rename(input, true, window, cx),
                    InputEvent::Change | InputEvent::Focus => {}
                }
            });
        self.ui.rename =
            Some(RenameEdit { path, input: input.clone(), _subscription: subscription });
        cx.notify();
        select_stem(input, is_directory, window, cx);
    }

    fn submit_rename(
        &mut self,
        input: &Entity<InputState>,
        on_blur: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.ui.rename.as_ref().map(|edit| edit.path.clone()) else {
            return;
        };
        let name = input.read(cx).value().to_string();
        if let Err(error) = validate_entry_name(&name) {
            // Clicking away means "never mind". Pulling focus back into the
            // input on every blur trapped the pointer in a field the user was
            // trying to leave; only an explicit Enter argues back.
            if on_blur {
                self.cancel_rename(window, cx);
                return;
            }
            window.push_notification(Notification::error(error.to_string()), cx);
            input.update(cx, |input, cx| input.focus(window, cx));
            return;
        }
        let unchanged = path.file_name().is_some_and(|current| {
            current == std::ffi::OsStr::new(&name) || display_filename(current) == name
        });
        self.cancel_rename(window, cx);
        if !unchanged {
            self.with_operations(window, cx, |ops, origin, cx| {
                ops.start_rename(path, name, origin, cx)
            });
        }
    }

    pub(super) fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ui.rename = None;
        self.focus_browser(window, cx);
        cx.notify();
    }

    // Create, compress, extract.

    pub(super) fn open_new_folder_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Folder name"));
        self.ask_name(
            window,
            cx,
            NameDialog { title: "New Folder", input, action: "Create" },
            |_| Ok(()),
            |this, name, window, cx| {
                let parent = this.directory.current_dir.clone();
                this.with_operations(window, cx, |ops, origin, cx| {
                    ops.start_create_directory(parent, name, origin, cx);
                });
            },
        );
    }

    pub(super) fn open_new_file_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("File name"));
        self.ask_name(
            window,
            cx,
            NameDialog { title: "New File", input, action: "Create" },
            |_| Ok(()),
            |this, name, window, cx| {
                let parent = this.directory.current_dir.clone();
                this.with_operations(window, cx, |ops, origin, cx| {
                    ops.start_create_file(parent, name, origin, cx);
                });
            },
        );
    }

    /// Copy the selection beside itself. The transfer picks the free names.
    pub(super) fn start_duplicate_selection(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sources = self.selected_paths();
        if sources.is_empty() {
            return;
        }
        self.ui.entry_menu = None;
        let destination = self.directory.current_dir.clone();
        self.with_operations(window, cx, |ops, origin, cx| {
            ops.start_duplicate(sources, destination, origin, cx);
        });
    }

    /// Ask where to move the selection, in a small dialog rather than a
    /// window. The destination is shown as breadcrumbs, as the location bar
    /// shows the current folder: a crumb goes up, a folder listed below goes
    /// down, the empty end of the row turns into a path field, and Places and
    /// Bookmarks sit underneath as one-click shortcuts.
    pub(super) fn open_move_to_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sources = self.selected_paths();
        if sources.is_empty() {
            return;
        }
        self.ui.entry_menu = None;
        let current = self.directory.current_dir.clone();
        let home = self.home_dir.clone();
        let state = MoveTo::new(current.clone(), self.directory.show_hidden, window, cx);
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Folder path"));
        let shortcuts =
            Rc::new(move_to_shortcuts(&self.sidebar.places, self.bookmarks.read(cx).bookmarks()));

        // Typing a path: Enter resolves it into the crumbs, leaving the field
        // cancels. The subscription lives as long as the dialog's closure.
        let subscription = cx.subscribe_in(&input, window, {
            let state = state.clone();
            let (current, home) = (current.clone(), home.clone());
            move |_, input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => {
                    let value = input.read(cx).value().to_string();
                    match resolve_location(&value, &current, Some(&home)) {
                        Ok(LocationTarget { directory, reveal: None }) => {
                            state.go_to(directory, window, cx);
                        }
                        Ok(_) => window.push_notification(
                            Notification::error("Enter a folder, not a file"),
                            cx,
                        ),
                        Err(error) => window.push_notification(Notification::error(error), cx),
                    }
                }
                InputEvent::Blur => state.end_editing(window),
                InputEvent::Change | InputEvent::Focus => {}
            }
        });

        let count = sources.len();
        let sources = Rc::new(sources);
        let view = cx.entity();
        window.open_dialog(cx, move |dialog, window, cx| {
            use gpui_component::{
                ActiveTheme as _, Disableable as _, Sizable as _, button::ButtonVariants as _,
            };
            let _keep = &subscription;
            let colors = cx.theme().colors;
            let radius = cx.theme().radius;
            let (destination, folders, editing, can_go_back) = state.snapshot();
            let back = Button::new("move-to-back")
                .xsmall()
                .compact()
                .ghost()
                .label("←")
                .disabled(!can_go_back)
                .on_click({
                    let state = state.clone();
                    move |_, window, cx| state.go_back(window, cx)
                });

            // The destination, as crumbs or as a field.
            let location: gpui::AnyElement = if editing {
                Input::new(&input).into_any_element()
            } else {
                let (go_to, edit) = (state.clone(), state.clone());
                let (input, destination) = (input.clone(), destination.clone());
                crumb_bar(
                    "move-to-crumbs",
                    compact(breadcrumbs(&destination), 6),
                    move |path, window, cx| go_to.go_to(path, window, cx),
                    move |window, cx| {
                        edit.begin_editing(window);
                        let value = destination.display().to_string();
                        input.update(cx, |input, cx| {
                            input.set_value(value, window, cx);
                            input.focus(window, cx);
                            let end = input.value().len();
                            input.set_selected_range(0..end, cx);
                        });
                    },
                    cx,
                )
                .w_full()
                .into_any_element()
            };

            // Marcel's own rows rather than gpui-component's List: that widget
            // is a selection (a `ListState` entity, a delegate, a selected row
            // to confirm), and these rows have no selected state to keep — a
            // click descends into the folder, as a crumb does.
            let folder_row = |index: usize, label: &String, path: &PathBuf| {
                let state = state.clone();
                let path = path.clone();
                gpui_component::h_flex()
                    .id(("move-to-folder", index))
                    .h(px(26.0))
                    .px_2()
                    .gap_2()
                    .rounded(radius)
                    .cursor_pointer()
                    .hover(|this| this.bg(colors.list_active))
                    .child(div().text_color(colors.muted_foreground).child("▸"))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(label.clone()),
                    )
                    .on_click(move |_, window, cx| state.go_to(path.clone(), window, cx))
            };
            let quiet_row = |text: &'static str| {
                div().h(px(26.0)).px_2().text_color(colors.muted_foreground).child(text)
            };
            let folder_list = match folders.as_deref() {
                None => quiet_row("Reading…").into_any_element(),
                Some([]) => quiet_row("No folders inside").into_any_element(),
                Some(folders) => div()
                    .id("move-to-folders")
                    .max_h(px(220.0))
                    .overflow_y_scroll()
                    .children(
                        folders
                            .iter()
                            .enumerate()
                            .map(|(index, (label, path))| folder_row(index, label, path)),
                    )
                    .into_any_element(),
            };

            let chips = shortcuts.iter().enumerate().map(|(index, (label, path))| {
                let state = state.clone();
                let path = path.clone();
                Button::new(("move-to-shortcut", index))
                    .xsmall()
                    .compact()
                    .outline()
                    .label(label.clone())
                    .on_click(move |_, window, cx| state.go_to(path.clone(), window, cx))
            });

            let (view, sources, state) = (view.clone(), sources.clone(), state.clone());
            let _ = window;
            dialog
                .title("Move To")
                .w(px(500.0))
                .child(
                    gpui_component::v_flex()
                        .gap_3()
                        .text_sm()
                        .child(
                            div()
                                .text_color(colors.muted_foreground)
                                .child(format!("Move {count} item(s) to")),
                        )
                        .child(
                            gpui_component::h_flex()
                                .gap_1()
                                .child(back)
                                .child(div().flex_1().min_w_0().child(location)),
                        )
                        .child(folder_list)
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .gap_1()
                                .pt_2()
                                .border_t_1()
                                .border_color(colors.border)
                                .children(chips),
                        ),
                )
                .button_props(DialogButtonProps::default().ok_text("Move").show_cancel(true))
                .footer(footer("Move", ButtonVariant::Primary, true))
                .overlay_closable(false)
                .close_button(false)
                .on_ok(move |_, window, cx| {
                    let destination = state.destination();
                    view.update(cx, |this, cx| {
                        this.start_transfer(
                            sources.as_ref().clone(),
                            destination,
                            TransferMode::Move,
                            None,
                            window,
                            cx,
                        );
                    });
                    true
                })
        });
        cx.notify();
    }

    pub(super) fn open_compress_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sources = self.selected_paths();
        let Some(first) = sources.first() else {
            return;
        };
        let single_is_directory = sources.len() == 1 && self.is_directory_entry(first);
        let proposed = default_zip_name(&sources, single_is_directory);
        let input = cx.new(|cx| InputState::new(window, cx).default_value(proposed));
        self.ask_name(
            window,
            cx,
            NameDialog { title: "Compress to ZIP", input, action: "Compress" },
            |name| {
                if name.to_ascii_lowercase().ends_with(".zip") {
                    Ok(())
                } else {
                    Err("The archive name must end in .zip")
                }
            },
            move |this, name, window, cx| {
                let sources = sources.clone();
                let destination = this.directory.current_dir.join(name);
                this.with_operations(window, cx, |ops, origin, cx| {
                    ops.start_compress(sources, destination, origin, cx);
                });
            },
        );
    }

    pub(super) fn start_extract_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ui.entry_menu = None;
        let Some(archive) = self.directory.selection.primary().cloned() else {
            return;
        };
        self.with_operations(window, cx, |ops, origin, cx| ops.start_extract(archive, origin, cx));
    }

    // Opening things.

    pub(super) fn open_entry(
        &mut self,
        entry: FileEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if entry.navigable {
            self.navigate_to(entry.path, true, cx);
            return;
        }
        // In a picker a file is an answer, not something to launch.
        if let Some(picker) = self.picker.as_ref() {
            match picker.mode {
                PickerMode::OpenFiles => {
                    // Activating one of the selected files means the selection;
                    // activating something outside it means that file alone.
                    if self.directory.selection.is_selected(&entry.path) {
                        self.confirm_picker(window, cx);
                    } else {
                        let filter = picker.active_filter;
                        self.answer_picker(
                            PickerResponse::Chosen { paths: vec![entry.path], filter },
                            window,
                            cx,
                        );
                    }
                }
                PickerMode::SaveFile => {
                    // The click that came first already proposed the name; an
                    // activation from elsewhere (the folder preview) is about
                    // a file the name field cannot describe.
                    if entry.path.parent() == Some(self.directory.current_dir.as_path()) {
                        self.confirm_picker(window, cx);
                    }
                }
                // A folder picker has nothing to say about a file.
                PickerMode::OpenDirectories | PickerMode::SaveFiles { .. } => {}
            }
            return;
        }
        let ticket = self.preview.ticket;
        self.launch(crate::desktop::open::open_file(entry.path), cx, move |this| {
            this.preview.ticket == ticket
        });
    }

    pub(super) fn open_primary_with(&mut self, cx: &mut Context<Self>) {
        let Some(path) =
            self.primary_entry().filter(|entry| !entry.navigable).map(|entry| entry.path.clone())
        else {
            return;
        };
        let still_primary = path.clone();
        self.launch(crate::desktop::open::open_file_with(path), cx, move |this| {
            this.directory.selection.primary() == Some(&still_primary)
        });
    }

    /// Hand a file to another application, showing a failure in the preview
    /// pane while `still_relevant` says the pane is about the same file.
    fn launch(
        &mut self,
        open: impl Future<Output = anyhow::Result<()>> + Send + 'static,
        cx: &mut Context<Self>,
        still_relevant: impl Fn(&Self) -> bool + 'static,
    ) {
        let task = cx.background_executor().spawn(open);
        cx.spawn(async move |this, cx| {
            if let Err(error) = task.await {
                let _ = this.update(cx, |this, cx| {
                    if still_relevant(this) {
                        this.preview.show_error(error.to_string());
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    pub(super) fn open_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let directory = self.directory.current_dir.clone();
        let task = unblock(cx, move || crate::desktop::terminal::open_terminal(&directory));
        cx.spawn_in(window, async move |this, window| {
            if let Err(error) = task.await {
                let _ = this.update_in(window, |_, window, cx| {
                    window.push_notification(Notification::error(error.to_string()), cx);
                });
            }
        })
        .detach();
    }

    /// Show the selected folder in a window of its own.
    ///
    /// Nothing is carried over: the new window is a surface like any other, and
    /// the journal, clipboard, and bookmarks it reads are the application's
    /// already.
    pub(super) fn open_selection_in_new_window(&mut self, cx: &mut Context<Self>) {
        let Some(directory) = self.selected_directory().map(Path::to_path_buf) else {
            return;
        };
        // The only refusal is the window cap, and a menu item that silently
        // does nothing looks broken; the reason belongs on this window.
        if let Err(error) = crate::window::open(directory, cx) {
            self.report(Report::Error(error.to_string()), cx);
        }
    }
}

/// The Places and Bookmarks a Move To dialog offers as one-click shortcuts:
/// every place that is a folder, then every bookmark, without repeats.
///
/// The Trash is a place but not a folder. Its path is the `trash:///`
/// sentinel, which a move would treat as a directory relative to the current
/// one and fail on, item by item.
fn move_to_shortcuts(places: &[Place], bookmarks: &[Bookmark]) -> Vec<(String, PathBuf)> {
    let mut seen = HashSet::new();
    places
        .iter()
        .filter(|place| !place.is_trash())
        .map(|place| (place.label.clone(), place.path.clone()))
        .chain(bookmarks.iter().map(|bookmark| (bookmark.label(), bookmark.path.clone())))
        .filter(|(_, path)| seen.insert(path.clone()))
        .collect()
}

/// The subfolders of a destination, by display name.
type Folders = Vec<(String, PathBuf)>;

/// What the Move To dialog is pointing at, shared by every closure the
/// dialog is made of.
#[derive(Clone)]
struct MoveTo(Rc<RefCell<MoveToState>>);

struct MoveToState {
    destination: PathBuf,
    /// Where the dialog pointed before each jump, newest last, so a crumb
    /// or chip that went somewhere unhelpful can be taken back.
    back: Vec<PathBuf>,
    /// The folders inside `destination`, once read; `None` while the read
    /// is still out.
    folders: Option<Folders>,
    /// The crumbs have become a text field.
    editing: bool,
    show_hidden: bool,
    /// Which destination the read in flight is for. A read of a folder the
    /// dialog has since left — on a stalled mount, say — must not list itself
    /// under the folder that replaced it.
    ticket: u64,
    _read: Option<Task<()>>,
}

impl MoveTo {
    fn new(destination: PathBuf, show_hidden: bool, window: &mut Window, cx: &mut App) -> Self {
        let this = Self(Rc::new(RefCell::new(MoveToState {
            destination,
            back: Vec::new(),
            folders: None,
            editing: false,
            show_hidden,
            ticket: 0,
            _read: None,
        })));
        this.read_folders(window, cx);
        this
    }

    fn destination(&self) -> PathBuf {
        self.0.borrow().destination.clone()
    }

    /// What the dialog draws: the destination, its folders if read, whether
    /// the crumbs are a field, and whether there is anywhere to go back to.
    fn snapshot(&self) -> (PathBuf, Option<Folders>, bool, bool) {
        let state = self.0.borrow();
        (state.destination.clone(), state.folders.clone(), state.editing, !state.back.is_empty())
    }

    fn go_to(&self, destination: PathBuf, window: &mut Window, cx: &mut App) {
        {
            let mut state = self.0.borrow_mut();
            state.editing = false;
            if destination == state.destination {
                window.refresh();
                return;
            }
            let previous = std::mem::replace(&mut state.destination, destination);
            state.back.push(previous);
        }
        self.read_folders(window, cx);
    }

    fn go_back(&self, window: &mut Window, cx: &mut App) {
        {
            let mut state = self.0.borrow_mut();
            let Some(previous) = state.back.pop() else {
                return;
            };
            state.destination = previous;
            state.editing = false;
        }
        self.read_folders(window, cx);
    }

    fn begin_editing(&self, window: &mut Window) {
        self.0.borrow_mut().editing = true;
        window.refresh();
    }

    fn end_editing(&self, window: &mut Window) {
        self.0.borrow_mut().editing = false;
        window.refresh();
    }

    /// List the destination's folders off the foreground. A directory read
    /// can hang on a stalled network mount, and a dialog is no place to
    /// freeze the window from.
    fn read_folders(&self, window: &mut Window, cx: &mut App) {
        let (destination, show_hidden, ticket) = {
            let mut state = self.0.borrow_mut();
            state.ticket += 1;
            state.folders = None;
            (state.destination.clone(), state.show_hidden, state.ticket)
        };
        window.refresh();
        let read = cx
            .background_executor()
            .spawn(smol::unblock(move || folders_in(&destination, show_hidden)));
        // Weak, so that closing the dialog drops the state, and with it the
        // wait on a read that may never return.
        let state = Rc::downgrade(&self.0);
        let task = window.spawn(cx, async move |cx| {
            let folders = read.await;
            let accepted =
                state.upgrade().is_some_and(|state| state.borrow_mut().accept(ticket, folders));
            if accepted {
                let _ = cx.update(|window, _| window.refresh());
            }
        });
        // Replacing the handle drops a superseded read, so at most one is
        // ever waited on; the ticket covers the one that already finished.
        self.0.borrow_mut()._read = Some(task);
    }
}

impl MoveToState {
    /// Take the folders a read produced, unless the dialog has moved on since
    /// it was asked for.
    fn accept(&mut self, ticket: u64, folders: Folders) -> bool {
        if ticket != self.ticket {
            return false;
        }
        self.folders = Some(folders);
        true
    }
}

/// How many entries the Move To dialog reads before it stops listing
/// subfolders. The dialog is for choosing a folder, not browsing one.
const MOVE_TO_SCAN_LIMIT: usize = 5_000;

/// The subfolders of `directory`, sorted by name, from at most
/// [`MOVE_TO_SCAN_LIMIT`] entries. `file_type` comes free with the entry on
/// Linux; only a symbolic link costs a `stat` to see what it points at.
fn folders_in(directory: &Path, show_hidden: bool) -> Folders {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut folders = entries
        .flatten()
        .take(MOVE_TO_SCAN_LIMIT)
        .filter(|entry| {
            entry
                .file_type()
                .is_ok_and(|kind| kind.is_dir() || (kind.is_symlink() && entry.path().is_dir()))
        })
        .map(|entry| (display_filename(&entry.file_name()), entry.path()))
        .filter(|(name, _)| show_hidden || !name.starts_with('.'))
        .collect::<Vec<_>>();
    folders.sort_by_key(|(name, _)| name.to_lowercase());
    folders
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Sandbox;

    fn labels(shortcuts: &[(String, PathBuf)]) -> Vec<&str> {
        shortcuts.iter().map(|(label, _)| label.as_str()).collect()
    }

    /// The Trash is somewhere things go, not somewhere they can be moved to:
    /// its sentinel path is not a directory. A bookmark that repeats a place
    /// is one chip, not two.
    #[test]
    fn move_to_shortcuts_skip_the_trash_and_repeats() {
        let home = PathBuf::from("/home/me");
        let places = [
            Place::home(home.clone()),
            Place::trash(),
            Place {
                label: "Downloads".to_string(),
                path: home.join("Downloads"),
                kind: crate::desktop::places::PlaceKind::Filesystem,
            },
        ];
        let bookmarks =
            [Bookmark { path: home.join("Downloads") }, Bookmark { path: home.join("Projects") }];

        let shortcuts = move_to_shortcuts(&places, &bookmarks);

        assert_eq!(labels(&shortcuts), ["Home", "Downloads", "Projects"]);
        assert!(shortcuts.iter().all(|(_, path)| path.is_absolute()));
    }

    /// The list is what the destination contains that a move could land in:
    /// folders, and links to folders, but no files and no dot-folders unless
    /// the window is showing hidden entries too.
    #[test]
    fn folders_in_lists_directories_and_links_to_them_in_name_order() {
        let sandbox = Sandbox::new();
        sandbox.dir("root/zeta");
        sandbox.dir("root/Alpha");
        sandbox.dir("root/.hidden");
        sandbox.file("root/notes.txt", "");
        sandbox.dir("elsewhere");
        std::os::unix::fs::symlink(sandbox.path("elsewhere"), sandbox.path("root/link")).unwrap();
        std::os::unix::fs::symlink(sandbox.path("root/notes.txt"), sandbox.path("root/filelink"))
            .unwrap();

        let names = |show_hidden| {
            folders_in(&sandbox.path("root"), show_hidden)
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(false), ["Alpha", "link", "zeta"]);
        assert_eq!(names(true), [".hidden", "Alpha", "link", "zeta"]);
        assert!(folders_in(&sandbox.path("missing"), true).is_empty());
    }

    /// A read that comes back for a destination the dialog has already left
    /// is dropped, so a slow folder cannot list itself under a fast one.
    #[test]
    fn a_read_for_a_left_destination_is_ignored() {
        let mut state = MoveToState {
            destination: PathBuf::from("/b"),
            back: vec![PathBuf::from("/a")],
            folders: None,
            editing: false,
            show_hidden: true,
            ticket: 2,
            _read: None,
        };
        let stale = vec![("from-a".to_string(), PathBuf::from("/a/from-a"))];
        let current = vec![("from-b".to_string(), PathBuf::from("/b/from-b"))];

        assert!(!state.accept(1, stale));
        assert_eq!(state.folders, None);
        assert!(state.accept(2, current.clone()));
        assert_eq!(state.folders, Some(current));
    }
}
