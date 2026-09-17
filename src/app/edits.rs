//! The window's side of every mutation: gather what is selected, ask when
//! the operation needs a name or a confirmation, then hand the work to the
//! application's operation owner.

use std::path::{Path, PathBuf};

use gpui::prelude::*;
use gpui::{App, Context, Entity, Window};
use gpui_component::{
    WindowExt as _,
    input::{InputEvent, InputState},
    notification::Notification,
};

use crate::{
    browse::entries::{FileEntry, display_filename},
    desktop::picker::{PickerMode, PickerRequest, PickerResponse},
    fsops::{TransferMode, archive::default_zip_name, validate_entry_name},
    operations::{FileClipboard, OperationProgressKind},
    preview::PreviewState as PreviewContent,
};

use super::{
    Marcel,
    dialogs::{Confirm, NameDialog},
    navigation::unblock,
    pointer::accepted_external_drop_paths,
    state::RenameEdit,
};

/// How much of a name is offered for replacement, as a byte offset: up to the
/// extension, or all of it for a folder, whose dots are not extensions.
pub(super) fn rename_stem_end(name: &str, is_directory: bool) -> usize {
    if is_directory {
        return name.len();
    }
    name.rfind('.').filter(|index| *index > 0).unwrap_or(name.len())
}

/// Focus `input` with its stem selected, so typing replaces the name and
/// leaves the extension. Every field that offers a name uses this: inline
/// rename, the save dialog, Compress, and the conflict dialog.
///
/// The selection is set directly rather than through the input's
/// `SelectToStart` action: an action dispatches to whatever has focus, and
/// focus given in the same frame has not landed yet, so the action went
/// nowhere and the caret merely sat before the extension.
pub(crate) fn select_stem(
    input: Entity<InputState>,
    is_directory: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let value = input.read(cx).value().to_string();
    let stem_end = rename_stem_end(&value, is_directory);
    window.defer(cx, move |window, cx| {
        input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.set_selected_range(0..stem_end, cx);
        });
    });
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

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
        // The clipboard is the application's, so cutting here and pasting in
        // another window is one gesture rather than two disconnected ones.
        self.operations.update(cx, |operations, _| {
            operations.set_clipboard(Some(FileClipboard { mode, paths }));
        });
        self.ui.entry_menu = None;
        let verb = match mode {
            TransferMode::Copy => "Copied",
            TransferMode::Move => "Cut",
        };
        window.push_notification(
            Notification::success(format!("{verb} {count} item(s) to the file clipboard")),
            cx,
        );
        cx.notify();
    }

    pub(super) fn start_paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(clipboard) = self.operations.read(cx).clipboard().cloned() else {
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

    pub(super) fn start_trash_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        self.ui.entry_menu = None;
        self.with_operations(window, cx, |ops, origin, cx| ops.start_trash(paths, origin, cx));
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
            [only] => format!("Permanently delete “{}”?", file_name(only)),
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

    /// Ask for a folder, then move the selection there.
    ///
    /// The folder chooser is the same window the portal backend shows, with
    /// the transfer as its caller instead of another application. Its answer
    /// arrives on a channel; the move then starts through the application's
    /// operation owner, so it happens even if this window has gone by then.
    pub(super) fn open_move_to_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sources = self.selected_paths();
        if sources.is_empty() {
            return;
        }
        self.ui.entry_menu = None;
        let (reply, answer) = async_channel::bounded(1);
        // A picker closes itself when its caller withdraws the request; this
        // caller never does, and dropping the sender says so.
        let (_never_withdrawn, closed) = async_channel::bounded::<()>(1);
        let request = PickerRequest {
            title: "Move To".to_string(),
            mode: PickerMode::OpenDirectories,
            multiple: false,
            accept_label: Some("Move".to_string()),
            start_directory: Some(self.directory.current_dir.clone()),
            current_name: None,
            filters: Vec::new(),
            current_filter: None,
            reply,
            closed,
        };
        if let Err(error) = crate::window::open_picker(request, cx) {
            window.push_notification(Notification::error(error.to_string()), cx);
            return;
        }
        let origin = Self::origin(window);
        cx.spawn(async move |_, cx| {
            let Ok(PickerResponse::Chosen { paths, .. }) = answer.recv().await else {
                return;
            };
            let Some(destination) = paths.into_iter().next() else {
                return;
            };
            cx.update(|cx| {
                crate::operations::global(cx).update(cx, |operations, cx| {
                    operations.start_transfer(
                        sources,
                        destination,
                        TransferMode::Move,
                        None,
                        origin,
                        cx,
                    );
                });
            });
        })
        .detach();
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
                        this.preview.state = PreviewContent::Error(error.to_string());
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
        if let Some(directory) = self.selected_directory().map(Path::to_path_buf) {
            let _ = crate::window::open(directory, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_selection_preserves_a_file_extension_but_selects_directory_dots() {
        assert_eq!(rename_stem_end("report.final.txt", false), 12);
        assert_eq!(rename_stem_end(".bashrc", false), 7);
        assert_eq!(rename_stem_end("folder.with.dots", true), 16);
        assert_eq!(rename_stem_end("猫.txt", false), 3);
    }
}
