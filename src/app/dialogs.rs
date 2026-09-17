//! The two shapes of dialog Marcel asks with — "are you sure?" and "what
//! name?" — plus the settings dialog.

use gpui::prelude::*;
use gpui::{Context, Entity, SharedString, Window, div, px};
use gpui_component::{
    ActiveTheme as _, IndexPath, WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    dialog::{DialogAction, DialogButtonProps, DialogClose, DialogFooter},
    h_flex,
    input::{Input, InputState},
    notification::Notification,
    select::{Select, SelectEvent, SelectState},
};

use crate::{
    fsops::validate_entry_name,
    theme::{self, Palette},
};

use super::{Marcel, edits::select_stem};

/// A question with one destructive or affirmative answer and Cancel.
pub(super) struct Confirm {
    pub title: &'static str,
    pub description: String,
    /// A second line under the description, typically what cannot be undone.
    pub note: Option<String>,
    pub action: &'static str,
    pub danger: bool,
}

/// A request for a name, validated the same way every other name is.
pub(super) struct NameDialog {
    pub title: &'static str,
    pub input: Entity<InputState>,
    pub action: &'static str,
}

pub(super) fn footer(
    action: &'static str,
    variant: ButtonVariant,
    show_cancel: bool,
) -> DialogFooter {
    DialogFooter::new()
        .when(show_cancel, |footer| {
            footer.child(DialogClose::new().child(Button::new("cancel").label("Cancel").outline()))
        })
        .child(DialogAction::new().child(Button::new(action).label(action).with_variant(variant)))
}

impl Marcel {
    /// Ask, and run `on_ok` on this window if the user agrees.
    pub(super) fn confirm(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        confirm: Confirm,
        on_ok: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        self.ui.entry_menu = None;
        let view = cx.entity();
        let danger = cx.theme().colors.danger;
        let on_ok = std::rc::Rc::new(on_ok);
        let variant = if confirm.danger { ButtonVariant::Danger } else { ButtonVariant::Primary };
        window.open_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            let on_ok = on_ok.clone();
            dialog
                .title(confirm.title)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(confirm.description.clone())
                        .when_some(confirm.note.clone(), |this, note| {
                            this.child(div().text_sm().text_color(danger).child(note))
                        }),
                )
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(confirm.action)
                        .ok_variant(variant)
                        .show_cancel(true),
                )
                .footer(footer(confirm.action, variant, true))
                .overlay_closable(false)
                .close_button(false)
                .on_ok(move |_, window, cx| {
                    view.update(cx, |this, cx| on_ok(this, window, cx));
                    true
                })
        });
        cx.notify();
    }

    /// Ask for a name; `on_ok` receives one that passed the name rule and
    /// `accept`, and may still refuse it by returning an error to show.
    pub(super) fn ask_name(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        dialog: NameDialog,
        accept: impl Fn(&str) -> Result<(), &'static str> + 'static,
        on_ok: impl Fn(&mut Self, String, &mut Window, &mut Context<Self>) + 'static,
    ) {
        self.ui.entry_menu = None;
        let view = cx.entity();
        let input = dialog.input.clone();
        let accept = std::rc::Rc::new(accept);
        let on_ok = std::rc::Rc::new(on_ok);
        window.open_dialog(cx, move |dialog_builder, _, _| {
            let view = view.clone();
            let input = input.clone();
            let accept = accept.clone();
            let on_ok = on_ok.clone();
            dialog_builder
                .title(dialog.title)
                .child(Input::new(&input))
                .button_props(DialogButtonProps::default().ok_text(dialog.action).show_cancel(true))
                .footer(footer(dialog.action, ButtonVariant::Primary, true))
                .overlay_closable(false)
                .close_button(false)
                .on_ok(move |_, window, cx| {
                    let name = input.read(cx).value().trim().to_string();
                    let refused = validate_entry_name(&name)
                        .map_err(|error| error.to_string())
                        .and_then(|()| accept(&name).map_err(str::to_string));
                    if let Err(message) = refused {
                        window.push_notification(Notification::error(message), cx);
                        return false;
                    }
                    view.update(cx, |this, cx| on_ok(this, name, window, cx));
                    true
                })
        });
        // Compress proposes a name; New Folder and New File start empty, and
        // selecting nothing of nothing is just focus.
        select_stem(dialog.input, false, window, cx);
        cx.notify();
    }

    pub(super) fn open_settings_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let palettes = Palette::ALL
            .iter()
            .map(|palette| SharedString::from(palette.label()))
            .collect::<Vec<_>>();
        let selected =
            Palette::ALL.iter().position(|palette| *palette == theme::active()).unwrap_or_default();
        let theme_select = cx.new(|cx| {
            SelectState::new(palettes, Some(IndexPath::default().row(selected)), window, cx)
        });
        cx.subscribe_in(
            &theme_select,
            window,
            |this, _, event: &SelectEvent<Vec<SharedString>>, _, cx| {
                let SelectEvent::Confirm(selected) = event;
                if let Some(palette) = selected.as_deref().and_then(Palette::from_name) {
                    theme::choose(palette, cx);
                    this.persist_browser_state();
                }
            },
        )
        .detach();

        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title("Settings")
                .w(px(420.0))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(div().text_sm().child("Appearance"))
                        .child(
                            h_flex()
                                .w_full()
                                .justify_between()
                                .gap_4()
                                .child("Theme")
                                .child(Select::new(&theme_select).w(px(240.0))),
                        ),
                )
                .button_props(DialogButtonProps::default().ok_text("Done"))
                .footer(footer("Done", ButtonVariant::Primary, false))
                .overlay_closable(false)
                .close_button(false)
        });
    }
}
