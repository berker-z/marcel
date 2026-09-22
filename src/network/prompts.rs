//! The questions a share asks while it connects: a password, and "trust
//! this host?".
//!
//! The backend is parked on the answer, so every route out of a dialog must
//! produce one. Dropping the pending reply answers it as a cancellation,
//! which the backend turns into a failure Marcel does not report; that is why
//! the dialogs cannot be dismissed by clicking away.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use async_channel::Receiver;
use gpui::prelude::*;
use gpui::{AnyWindowHandle, App, Context, Window, div};
use gpui_component::{
    WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    checkbox::Checkbox,
    dialog::DialogFooter,
    input::{Input, InputState},
};

use crate::{
    desktop::gvfs::{PasswordReply, PasswordRequest, Prompt, QuestionRequest},
    surface,
};

use super::NetworkStore;

/// Put each prompt from one mount or unmount to the user, on whichever window
/// still speaks for `origin`, until the backend stops asking.
pub(super) fn serve(
    prompts: Receiver<Prompt>,
    origin: AnyWindowHandle,
    cx: &mut Context<NetworkStore>,
) {
    // Whether one of these dialogs is up, so an abort from the backend closes
    // it and not some other dialog the user opened since.
    let open = Rc::new(Cell::new(false));
    cx.spawn(async move |this, cx| {
        while let Ok(prompt) = prompts.recv().await {
            let Some(store) = this.upgrade() else {
                break;
            };
            let Some(handle) = cx.update(|cx| surface::current(Some(origin), cx)) else {
                // Nothing can answer, so dropping the question refuses it
                // rather than leaving the backend parked forever.
                break;
            };
            let open = open.clone();
            let shown = handle.update(cx, |_, window, cx| match prompt {
                Prompt::Password(request) => ask_password(&store, request, open, window, cx),
                Prompt::Question(request) => ask_question(request, open, window, cx),
                Prompt::Aborted => {
                    if open.replace(false) {
                        window.close_dialog(cx);
                    }
                }
            });
            if shown.is_err() {
                break;
            }
        }
    })
    .detach();
}

fn ask_password(
    store: &gpui::Entity<NetworkStore>,
    request: PasswordRequest,
    open: Rc<Cell<bool>>,
    window: &mut Window,
    cx: &mut App,
) {
    let flags = request.flags;
    let username =
        cx.new(|cx| InputState::new(window, cx).default_value(request.default_user.clone()));
    let domain =
        cx.new(|cx| InputState::new(window, cx).default_value(request.default_domain.clone()));
    let password = cx.new(|cx| InputState::new(window, cx).masked(true));
    let remember = Rc::new(Cell::new(false));
    // The dialog callbacks are shared closures, so the one-shot reply has to
    // be taken out of somewhere they can all reach.
    let reply = Rc::new(RefCell::new(Some(request.reply)));
    let (title, body) = split_message(&request.message, "Authentication Required");
    let store = store.clone();
    // Focus lands on the first thing the backend needs typed.
    let first = if flags.needs_username() && request.default_user.is_empty() {
        username.clone()
    } else if flags.needs_password() {
        password.clone()
    } else {
        username.clone()
    };
    open.set(true);

    window.open_dialog(cx, move |dialog, _, _| {
        let remembers = remember.get();
        let answer = {
            let reply = reply.clone();
            let open = open.clone();
            move |value: Option<PasswordReply>| {
                open.set(false);
                if let Some(reply) = reply.borrow_mut().take() {
                    let _ = reply.try_send(value);
                }
            }
        };
        let cancel = answer.clone();
        let anonymous = answer.clone();
        let (username_input, domain_input, password_input) =
            (username.clone(), domain.clone(), password.clone());
        let toggle = remember.clone();
        let redraw = store.clone();
        let field = |label: &'static str, input: &gpui::Entity<InputState>| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_sm().child(label))
                .child(Input::new(input))
        };

        dialog
            .title(title.clone())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .when(!body.is_empty(), |this| this.child(body.clone()))
                    .when(flags.needs_username(), |this| this.child(field("User name", &username)))
                    .when(flags.needs_domain(), |this| this.child(field("Domain", &domain)))
                    .when(flags.needs_password(), |this| this.child(field("Password", &password)))
                    .when(flags.can_save(), |this| {
                        this.child(
                            Checkbox::new("remember-password")
                                .checked(remembers)
                                .label("Remember this password")
                                .on_click(move |checked, _, cx| {
                                    toggle.set(*checked);
                                    redraw.update(cx, |_, cx| cx.notify());
                                }),
                        )
                    }),
            )
            .footer(
                DialogFooter::new()
                    .child(Button::new("password-cancel").label("Cancel").outline().on_click(
                        move |_, window, cx| {
                            cancel(None);
                            window.close_dialog(cx);
                        },
                    ))
                    .when(flags.allows_anonymous(), |this| {
                        this.child(
                            Button::new("password-anonymous")
                                .label("Connect Anonymously")
                                .outline()
                                .on_click(move |_, window, cx| {
                                    anonymous(Some(PasswordReply {
                                        anonymous: true,
                                        ..PasswordReply::default()
                                    }));
                                    window.close_dialog(cx);
                                }),
                        )
                    })
                    .child(
                        Button::new("password-connect")
                            .label("Connect")
                            .with_variant(ButtonVariant::Primary)
                            .on_click(move |_, window, cx| {
                                answer(Some(PasswordReply {
                                    username: username_input.read(cx).value().trim().to_string(),
                                    domain: domain_input.read(cx).value().trim().to_string(),
                                    password: password_input.read(cx).value().to_string(),
                                    anonymous: false,
                                    remember: remembers,
                                }));
                                window.close_dialog(cx);
                            }),
                    ),
            )
            .overlay_closable(false)
            .close_button(false)
    });
    first.update(cx, |input, cx| input.focus(window, cx));
}

fn ask_question(request: QuestionRequest, open: Rc<Cell<bool>>, window: &mut Window, cx: &mut App) {
    let reply = Rc::new(RefCell::new(Some(request.reply)));
    let (title, body) = split_message(&request.message, "Question");
    let choices = request.choices;
    open.set(true);

    window.open_dialog(cx, move |dialog, _, _| {
        let answer = {
            let reply = reply.clone();
            let open = open.clone();
            move |choice: Option<usize>| {
                open.set(false);
                if let Some(reply) = reply.borrow_mut().take() {
                    let _ = reply.try_send(choice);
                }
            }
        };
        let mut footer = DialogFooter::new();
        // Choice 0 is the one a backend treats as "go ahead" ("Log In Anyway"
        // before "Cancel Login" in the SFTP backend), and GTK's mount
        // operation makes it the default and adds the buttons in reverse so
        // it lands on the right; the same here, so the two dialogs agree.
        for (index, choice) in choices.iter().enumerate().rev() {
            let answer = answer.clone();
            let variant =
                if index == 0 { ButtonVariant::Primary } else { ButtonVariant::Secondary };
            footer = footer.child(
                Button::new(("question-choice", index))
                    .label(choice.clone())
                    .with_variant(variant)
                    .on_click(move |_, window, cx| {
                        answer(Some(index));
                        window.close_dialog(cx);
                    }),
            );
        }
        dialog
            .title(title.clone())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .when(!body.is_empty(), |this| this.child(body.clone())),
            )
            .footer(footer)
            .overlay_closable(false)
            .close_button(false)
    });
}

/// GVfs's messages come as a headline, a blank line, and the explanation;
/// the headline is the title and the rest the body.
fn split_message(message: &str, fallback_title: &str) -> (String, String) {
    let message = message.trim();
    match message.split_once('\n') {
        Some((title, body)) if !title.trim().is_empty() => {
            (title.trim().to_string(), body.trim().to_string())
        }
        _ if message.is_empty() => (fallback_title.to_string(), String::new()),
        _ => (fallback_title.to_string(), message.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::split_message;

    #[test]
    fn a_headline_becomes_the_title() {
        assert_eq!(
            split_message(
                "Can’t verify the identity of “wired”.\n\nThis happens when you log in for the first time.",
                "Question"
            ),
            (
                "Can’t verify the identity of “wired”.".into(),
                "This happens when you log in for the first time.".into()
            )
        );
        assert_eq!(
            split_message("Enter password for wired", "Authentication Required"),
            ("Authentication Required".into(), "Enter password for wired".into())
        );
        assert_eq!(split_message("  ", "Question"), ("Question".into(), String::new()));
    }
}
