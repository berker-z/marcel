//! What a file-chooser window does differently.
//!
//! This is the whole list. Everything else about a picker — browsing,
//! preview, sorting, the context menu — is the ordinary window; a feature
//! added to the browser appears in pickers without any code here. The other
//! `self.picker` checks live in `edits::open_entry` (a file is an answer,
//! not something to launch), `menu::activate_entry` (a click proposes a
//! name), `actions` (Escape dismisses the dialog), and `sidebar` (no Trash
//! place). Keep it that way so the list stays greppable.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use gpui::prelude::*;
use gpui::{AnyElement, Context, IntoElement, SharedString, Window, div, px};
use gpui_component::{
    ActiveTheme as _, Disableable as _, IndexPath, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    notification::Notification,
    select::{Select, SelectEvent, SelectState},
};

use crate::{
    browse::{
        directory_session::ContentFilter,
        entries::{FileEntry, display_filename},
    },
    desktop::picker::{PickerMode, PickerRequest, PickerResponse},
    fsops::validate_entry_name,
};

use super::{
    Marcel, dialogs::Confirm, edits::select_stem, navigation::unblock, state::PickerState,
};

impl Marcel {
    /// A window that answers a file-chooser request.
    pub fn new_picker(request: PickerRequest, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let home_dir = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        let mut this = Self::new(request.initial_directory(&home_dir), window, cx);

        let name_input = matches!(request.mode, PickerMode::SaveFile).then(|| {
            let name = request.current_name.clone().unwrap_or_default();
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("File name")
                    .default_value(name)
            });
            let subscription =
                cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        this.confirm_picker(window, cx);
                    }
                });
            (input, subscription)
        });
        let filter_select = (!request.filters.is_empty()).then(|| {
            let labels = request
                .filters
                .iter()
                .map(|filter| SharedString::from(filter.name.clone()))
                .collect::<Vec<_>>();
            let initial = request
                .current_filter
                .unwrap_or(0)
                .min(labels.len().saturating_sub(1));
            let select = cx.new(|cx| {
                SelectState::new(labels, Some(IndexPath::default().row(initial)), window, cx)
            });
            let subscription = cx.subscribe_in(
                &select,
                window,
                |this, select, _: &SelectEvent<Vec<SharedString>>, _, cx| {
                    let index = select.read(cx).selected_index(cx).map(|path| path.row);
                    this.set_picker_filter(index, cx);
                },
            );
            (select, subscription)
        });

        this.directory.selection.set_single(!request.multiple);
        this.picker = Some(PickerState::new(request, name_input, filter_select));
        this.apply_picker_filter(cx);
        this
    }

    /// Where typing should land when a picker opens: the name field of a save
    /// dialog, otherwise the listing.
    pub fn focus_picker(&self, window: &mut Window, cx: &mut Context<Self>) {
        match self
            .picker
            .as_ref()
            .and_then(|picker| picker.name_input.clone())
        {
            // Select the stem, as Rename does: the name is the part that
            // usually changes, the extension the part the caller chose.
            Some(input) => select_stem(input, false, window, cx),
            None => self.focus_browser(window, cx),
        }
    }

    fn set_picker_filter(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        if picker.active_filter != index {
            picker.active_filter = index;
            self.apply_picker_filter(cx);
        }
    }

    /// Project the listing under the picker's active filter.
    fn apply_picker_filter(&mut self, cx: &mut Context<Self>) {
        let filter = self
            .picker
            .as_ref()
            .and_then(PickerState::active_filter)
            .cloned();
        let reconcile = self.directory.set_content_filter(filter.map(|filter| {
            Arc::new(move |entry: &FileEntry| filter.matches(entry)) as ContentFilter
        }));
        self.apply_selection_reconcile(reconcile, cx);
        cx.notify();
    }

    /// Send the answer and close. Only the first answer leaves the window.
    pub(super) fn answer_picker(
        &mut self,
        response: PickerResponse,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(picker) = self.picker.as_mut()
            && picker.answer(response)
        {
            window.remove_window();
        }
        cx.notify();
    }

    pub(super) fn cancel_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.answer_picker(PickerResponse::Cancelled, window, cx);
    }

    /// The caller withdrew the request; there is nobody left to answer.
    pub fn withdraw_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.answer_picker(PickerResponse::Closed, window, cx);
    }

    fn is_navigable_entry(&self, path: &Path) -> bool {
        self.directory
            .entry(path)
            .is_some_and(|entry| entry.navigable)
    }

    /// Turn what the window shows into the caller's answer.
    pub(super) fn confirm_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(picker) = self.picker.as_ref() else {
            return;
        };
        if picker.confirming {
            return;
        }
        if self.sidebar.browsing_trash {
            window.push_notification(
                Notification::info("Restore the item first; the Trash cannot be chosen from"),
                cx,
            );
            return;
        }
        let mode = picker.mode.clone();
        let filter = picker.active_filter;
        let name_input = picker.name_input.clone();
        let (folders, files): (Vec<PathBuf>, Vec<PathBuf>) = self
            .selected_paths()
            .into_iter()
            .partition(|path| self.is_navigable_entry(path));
        let here = self.directory.current_dir.clone();

        match mode {
            PickerMode::OpenFiles => {
                if files.is_empty() {
                    // Open with only a folder selected means "go in", the
                    // same thing Enter means in a browsing window.
                    if let Some(folder) = folders.into_iter().next() {
                        self.navigate_to(folder, true, cx);
                    }
                    return;
                }
                self.answer_picker(
                    PickerResponse::Chosen {
                        paths: files,
                        filter,
                    },
                    window,
                    cx,
                );
            }
            PickerMode::OpenDirectories => {
                // Nothing selected inside it: the folder being looked at is
                // the folder being chosen.
                let paths = if folders.is_empty() {
                    vec![here]
                } else {
                    folders
                };
                self.answer_picker(PickerResponse::Chosen { paths, filter }, window, cx);
            }
            PickerMode::SaveFile => {
                let Some(input) = name_input else {
                    return;
                };
                let name = input.read(cx).value().to_string();
                if let Err(error) = validate_entry_name(&name) {
                    window.push_notification(Notification::error(error.to_string()), cx);
                    input.update(cx, |input, cx| input.focus(window, cx));
                    return;
                }
                let target = here.join(&name);
                if self.is_navigable_entry(&target) {
                    // Typing a folder's name and pressing Save goes into it,
                    // which is what every other save dialog does.
                    self.navigate_to(target, true, cx);
                    return;
                }
                self.check_targets_then_answer(vec![target], filter, window, cx);
            }
            PickerMode::SaveFiles { names } => {
                let folder = folders.into_iter().next().unwrap_or(here);
                let targets = names.iter().map(|name| folder.join(name)).collect();
                self.check_targets_then_answer(targets, filter, window, cx);
            }
        }
    }

    /// Answer with `targets` once it is known which of them already exist,
    /// asking before any of those is overwritten.
    fn check_targets_then_answer(
        &mut self,
        targets: Vec<PathBuf>,
        filter: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        picker.confirming = true;
        cx.notify();

        let probe = targets.clone();
        let check = unblock(cx, move || {
            probe
                .into_iter()
                .filter(|target| std::fs::symlink_metadata(target).is_ok())
                .collect::<Vec<_>>()
        });
        cx.spawn_in(window, async move |this, window| {
            let existing = check.await;
            let _ = this.update_in(window, |this, window, cx| {
                let Some(picker) = this.picker.as_mut() else {
                    return;
                };
                picker.confirming = false;
                let response = PickerResponse::Chosen {
                    paths: targets,
                    filter,
                };
                if existing.is_empty() {
                    this.answer_picker(response, window, cx);
                    return;
                }
                let description = match existing.as_slice() {
                    [only] => format!(
                        "“{}” already exists. Replace it?",
                        only.file_name()
                            .map(display_filename)
                            .unwrap_or_else(|| only.display().to_string())
                    ),
                    _ => format!(
                        "{} of these files already exist. Replace them?",
                        existing.len()
                    ),
                };
                this.confirm(
                    window,
                    cx,
                    Confirm {
                        title: "Replace File",
                        description,
                        note: Some(
                            "The application will overwrite the existing contents.".to_string(),
                        ),
                        action: "Replace",
                        danger: true,
                    },
                    move |this, window, cx| this.answer_picker(response.clone(), window, cx),
                );
            });
        })
        .detach();
    }

    /// The bar along the bottom of a picker: the name field, the filter,
    /// and the two buttons every dialog has.
    pub(super) fn render_picker_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let picker = self.picker.as_ref()?;
        let colors = cx.theme().colors;
        let hint = match &picker.mode {
            PickerMode::OpenFiles if picker.multiple => "Choose one or more files",
            PickerMode::OpenFiles => "Choose a file",
            PickerMode::OpenDirectories if picker.multiple => {
                "Choose folders, or open the one to use"
            }
            PickerMode::OpenDirectories => "Choose a folder, or open the one to use",
            PickerMode::SaveFile => "Name",
            PickerMode::SaveFiles { names } if names.len() == 1 => "Choose where to save the file",
            PickerMode::SaveFiles { .. } => "Choose where to save the files",
        };
        Some(
            h_flex()
                .flex_none()
                .w_full()
                .h(px(56.0))
                .px_4()
                .gap_3()
                .items_center()
                .bg(colors.sidebar)
                .border_t_1()
                .border_color(colors.border)
                .text_color(colors.sidebar_foreground)
                .child(
                    div()
                        .flex_none()
                        .text_sm()
                        .text_color(colors.muted_foreground)
                        .child(hint),
                )
                .child(match &picker.name_input {
                    Some(input) => div()
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(input).small().h_8()),
                    None => div().flex_1(),
                })
                .when_some(picker.filter_select.as_ref(), |this, select| {
                    // The component fills whatever it is given; without a box
                    // of its own it fills the bar and sits against its top.
                    this.child(
                        div()
                            .flex_none()
                            .w(px(200.0))
                            .h(px(28.0))
                            .child(Select::new(select).small()),
                    )
                })
                .child(
                    Button::new("picker-cancel")
                        .label("Cancel")
                        .outline()
                        .small()
                        .on_click(
                            cx.listener(|this, _, window, cx| this.cancel_picker(window, cx)),
                        ),
                )
                .child(
                    Button::new("picker-accept")
                        .label(SharedString::from(picker.accept_label.clone()))
                        .primary()
                        .small()
                        .disabled(picker.confirming)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.confirm_picker(window, cx)),
                        ),
                )
                .into_any_element(),
        )
    }
}
