//! The "already exists" dialog a transfer raises for each conflict.
//!
//! The transfer thread is blocked on the answer, so every route out of the
//! dialog must produce one. Dropping the pending question answers it as a
//! cancellation, which is why the dialog cannot be dismissed by clicking away:
//! an accidental dismissal would abandon the whole transfer.

use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    rc::Rc,
};

use async_channel::Receiver;
use gpui::{
    AnyWindowHandle, App, AppContext as _, Context, Entity, ParentElement as _, Styled as _,
    Window, div,
};
use gpui_component::{
    WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    checkbox::Checkbox,
    dialog::DialogFooter,
    input::{Input, InputState},
};

use crate::{
    browse::entries::display_filename,
    fsops::conflict::{ConflictDecision, ConflictResponse, PendingConflict, unique_name_in},
    surface,
};

use super::OperationCoordinator;

/// Put each question from `questions` to the user, on whichever window still
/// speaks for `origin`, until the transfer stops asking.
///
/// Questions arrive one at a time: the transfer thread waits for each answer,
/// so a second conflict cannot be raised while the first is on screen.
pub(super) fn serve(
    questions: Receiver<PendingConflict>,
    origin: AnyWindowHandle,
    cx: &mut Context<OperationCoordinator>,
) {
    cx.spawn(async move |this, cx| {
        while let Ok(pending) = questions.recv().await {
            let Some(coordinator) = this.upgrade() else {
                break;
            };
            let Some(handle) = cx.update(|cx| surface::current(Some(origin), cx)) else {
                // Nothing can answer, so dropping the question refuses it
                // rather than leaving the worker parked forever.
                break;
            };
            if handle.update(cx, |_, window, cx| ask(&coordinator, pending, window, cx)).is_err() {
                break;
            }
        }
    })
    .detach();
}

/// Ask the user about one destination conflict.
fn ask(
    coordinator: &Entity<OperationCoordinator>,
    pending: PendingConflict,
    window: &mut Window,
    cx: &mut App,
) {
    let request = pending.request().clone();
    let name = request.destination.file_name().map(display_filename).unwrap_or_default();
    let folder = request
        .destination
        .parent()
        .map(|parent| display_filename(parent.as_os_str()))
        .unwrap_or_default();
    let kind = if request.destination_is_directory { "folder" } else { "file" };
    let suggestion = request
        .destination
        .file_name()
        .and_then(|name| {
            request
                .destination
                .parent()
                .and_then(|parent| unique_name_in(parent, name, request.source_is_directory))
        })
        .map(|name| display_filename(&name))
        .unwrap_or_else(|| name.clone());

    let input = cx.new(|cx| InputState::new(window, cx).default_value(suggestion));
    // The dialog callbacks are shared closures, so the one-shot answer has to
    // be taken out of somewhere they can all reach.
    let pending = Rc::new(RefCell::new(Some(pending)));
    // Scoped to this one question: the sticky answer it produces lives in the
    // operation's policy, not here.
    let apply_to_all = Rc::new(Cell::new(false));
    let is_merge = request.is_merge();

    let input_for_dialog = input.clone();
    let coordinator = coordinator.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        let input = input_for_dialog.clone();
        // Read the live value, because the checkbox draws whatever it is told.
        // It cannot come from an entity: this closure runs while the update
        // that opened the dialog still holds its lease, so reading one panics.
        let applies_to_all = apply_to_all.get();
        let answer = {
            let pending = pending.clone();
            move |response: ConflictResponse| {
                if let Some(pending) = pending.borrow_mut().take() {
                    pending.answer(ConflictDecision { response, apply_to_all: applies_to_all });
                }
            }
        };
        // Each button answers and dismisses on its own rather than sitting
        // inside a DialogClose wrapper. The wrapper closes on a click of its
        // own surrounding element, which a button with its own handler never
        // delivers — so every answer was sent while the dialog stayed on
        // screen, and the next conflict opened another one on top of it.
        let answer_button =
            |id: &'static str,
             label: &'static str,
             respond: Box<dyn Fn(&App) -> ConflictResponse>| {
                let answer = answer.clone();
                Button::new(id).label(label).outline().on_click(move |_, window, cx| {
                    answer(respond(cx));
                    window.close_dialog(cx);
                })
            };
        let fixed = |response: ConflictResponse| -> Box<dyn Fn(&App) -> ConflictResponse> {
            Box::new(move |_| response.clone())
        };
        let toggle = apply_to_all.clone();
        let redraw = coordinator.clone();
        let typed_name = input.clone();

        dialog
            .title(if request.destination_is_directory {
                "Folder Already Exists"
            } else {
                "File Already Exists"
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(format!("A {kind} named “{name}” already exists in “{folder}”."))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_sm().child("Rename it to"))
                            .child(Input::new(&input)),
                    )
                    .child(
                        Checkbox::new("conflict-apply-to-all")
                            .checked(applies_to_all)
                            .label(if is_merge {
                                "Do this for every folder"
                            } else {
                                "Do this for every file"
                            })
                            .on_click(move |checked, _, cx| {
                                toggle.set(*checked);
                                // A click is dispatched outside any entity
                                // update, so asking for a redraw here is safe
                                // and is what makes the new value visible.
                                redraw.update(cx, |_, cx| cx.notify());
                            }),
                    ),
            )
            .footer(
                DialogFooter::new()
                    .child(answer_button(
                        "conflict-cancel",
                        "Cancel",
                        fixed(ConflictResponse::Cancel),
                    ))
                    .child(answer_button("conflict-skip", "Skip", fixed(ConflictResponse::Skip)))
                    .child(answer_button(
                        "conflict-rename",
                        "Rename",
                        // A typed name cannot answer later conflicts, so
                        // applying to all means Marcel picks the names.
                        Box::new(move |cx| {
                            if applies_to_all {
                                ConflictResponse::AutoRename
                            } else {
                                ConflictResponse::Rename(OsString::from(
                                    typed_name.read(cx).value().trim().to_string(),
                                ))
                            }
                        }),
                    ))
                    .child(
                        answer_button(
                            "conflict-replace",
                            if is_merge { "Merge" } else { "Replace" },
                            fixed(ConflictResponse::Replace),
                        )
                        .with_variant(ButtonVariant::Danger),
                    ),
            )
            .overlay_closable(false)
            .close_button(false)
    });
    // The suggested name is selected, so typing over it replaces it and Rename
    // works without reaching for the pointer.
    input.update(cx, |input, cx| input.focus(window, cx));
}
