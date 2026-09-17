//! The frame around the panes: the top bar, the progress card and
//! notification stack, and `render()` itself, which composes every surface.

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, CursorStyle, Div, IntoElement, MouseButton, Pixels, Render, Stateful,
    Window, canvas, div, px, relative,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Root, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::Input,
    progress::Progress,
    resizable::{h_resizable, resizable_panel},
};

use crate::browse::entries::format_size;

use super::{
    MAX_PREVIEW_WIDTH, MIN_BROWSER_WIDTH, MIN_PREVIEW_WIDTH, Marcel,
    actions::{BROWSER_KEY_CONTEXT, BrowserCommand, bind_actions},
    pointer::{BookmarkDrag, FileDrag},
};

/// A glyph button on the chrome. gpui-component's icon-only Button loses its
/// SVG tint on Marcel's themed surfaces, so these few controls draw a glyph
/// in the semantic foreground the theme supplies.
pub(super) fn icon_button(
    id: &'static str,
    glyph: &'static str,
    enabled: bool,
    cx: &Context<Marcel>,
) -> Stateful<Div> {
    let colors = cx.theme().colors;
    div()
        .id(id)
        .flex()
        .flex_none()
        .size(px(28.0))
        .items_center()
        .justify_center()
        .rounded(cx.theme().radius)
        .font_family(cx.theme().mono_font_family.clone())
        .line_height(relative(1.0))
        .text_lg()
        .text_color(if enabled { colors.sidebar_foreground } else { colors.muted_foreground })
        .when(enabled, |button| {
            button.cursor_pointer().hover(|button| button.bg(colors.sidebar_accent))
        })
        .child(glyph)
}

impl Marcel {
    fn render_topbar(
        &self,
        sidebar_width: Pixels,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors;
        let command_button = |id: &'static str,
                              glyph: &'static str,
                              command: BrowserCommand,
                              cx: &mut Context<Self>| {
            let enabled = self.command_enabled(command, cx);
            icon_button(id, glyph, enabled, cx).when(enabled, |button| {
                button.on_click(
                    cx.listener(move |this, _, window, cx| this.execute(command, window, cx)),
                )
            })
        };
        let location_width =
            (f32::from(window.bounds().size.width) - f32::from(sidebar_width) - 296.0).max(180.0);
        let max_breadcrumbs = ((location_width / 96.0).floor() as usize).clamp(3, 8);
        h_flex()
            .flex_none()
            .h(px(40.0))
            .w_full()
            .bg(colors.sidebar)
            .border_b_1()
            .border_color(colors.border)
            .text_color(colors.sidebar_foreground)
            .child(
                h_flex()
                    .flex_none()
                    .w(sidebar_width)
                    .h_full()
                    .px_2()
                    .gap_1()
                    .border_r_1()
                    .border_color(colors.sidebar_border)
                    .child(command_button("back", "←", BrowserCommand::GoBack, cx))
                    .child(command_button("forward", "→", BrowserCommand::GoForward, cx))
                    .child(command_button("up", "↑", BrowserCommand::GoToParent, cx))
                    .child(command_button(
                        "undo-file-operation",
                        "↶",
                        BrowserCommand::UndoFileOperation,
                        cx,
                    ))
                    .child(command_button(
                        "redo-file-operation",
                        "↷",
                        BrowserCommand::RedoFileOperation,
                        cx,
                    )),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .px_2()
                    .gap_2()
                    .child(self.render_location_bar(max_breadcrumbs, cx))
                    .child(
                        Input::new(&self.ui.search_input)
                            .small()
                            .cleanable(true)
                            .h_7()
                            .w(px(240.0)),
                    ),
            )
            .into_any_element()
    }

    fn render_operation_progress(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (kind, source_count, detail, cancellable, snapshot, cancelling) = {
            let operations = self.operations.read(cx);
            let active = operations.progress()?;
            (
                active.kind,
                active.source_count,
                active.detail.clone(),
                active.cancellable,
                active.progress.snapshot(),
                operations.is_cancelling(),
            )
        };
        let colors = cx.theme().colors;
        let title = if cancelling { "Cancelling…" } else { kind.title() };
        let percentage = if snapshot.total_bytes > 0 {
            snapshot.completed_bytes as f32 / snapshot.total_bytes as f32 * 100.0
        } else if snapshot.total_items > 0 {
            snapshot.completed_items as f32 / snapshot.total_items as f32 * 100.0
        } else {
            0.0
        };
        let progress_text = if snapshot.preparing {
            format!("Preparing {} item(s)", snapshot.total_items.max(source_count as u64))
        } else if snapshot.total_bytes > 0 {
            format!(
                "{} of {} items · {} of {}",
                snapshot.completed_items,
                snapshot.total_items,
                format_size(Some(snapshot.completed_bytes)),
                format_size(Some(snapshot.total_bytes))
            )
        } else {
            format!("{} of {} items", snapshot.completed_items, snapshot.total_items)
        };
        let current_name = snapshot.current_path.as_ref().map(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string())
        });
        let cancel_button = Button::new("cancel-active-transfer")
            .small()
            .danger()
            .label(if cancelling { "Cancelling" } else { "Cancel" })
            .disabled(cancelling)
            .on_click(cx.listener(|this, _, _, cx| this.cancel_active_operation(cx)));

        Some(
            div()
                .w_80()
                .max_w_full()
                .p_3()
                .flex()
                .flex_col()
                .gap_2()
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(colors.border)
                .bg(colors.popover)
                .text_color(colors.popover_foreground)
                .occlude()
                .child(
                    h_flex()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(div().text_sm().child(title))
                                .child(
                                    div()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .text_xs()
                                        .text_color(colors.muted_foreground)
                                        .child(detail),
                                ),
                        )
                        .when(cancellable, |this| this.child(cancel_button)),
                )
                .child(Progress::new("operation-progress").h_1().value(percentage))
                .child(
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .text_color(colors.muted_foreground)
                        .child(div().flex_none().child(progress_text))
                        .when_some(current_name, |this, name| {
                            this.child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .text_right()
                                    .child(name),
                            )
                        }),
                )
                .into_any_element(),
        )
    }

    fn render_preview_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let footer_lines = self.preview_footer_lines(cx);
        let preview_width = self.preview.width.clone();
        let preview_view = cx.entity();
        div()
            .relative()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .bg(colors.sidebar)
            .child(
                canvas(
                    move |bounds, _, cx| {
                        if (preview_width.get() - bounds.size.width).abs() >= px(1.0) {
                            preview_width.set(bounds.size.width);
                            preview_view.update(cx, |this, cx| this.schedule_preview_wrap(cx));
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(self.render_preview(window, cx)),
            )
            .when(!footer_lines.is_empty(), |this| {
                this.child(div().flex().flex_col().gap_1().px_4().py_3().children(
                    footer_lines.into_iter().enumerate().map(|(index, (text, color))| {
                        div()
                            .when(index == 0, |line| line.text_sm().whitespace_normal())
                            .when(index > 0, |line| line.text_xs())
                            .text_color(color)
                            .child(text)
                    }),
                ))
            })
            .into_any_element()
    }
}

impl Render for Marcel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.ui.search_input.read(cx).value().as_ref() != self.directory.filter_query {
            let query = self.directory.filter_query.clone();
            self.ui.search_input.update(cx, |input, cx| input.set_value(query, window, cx));
        }
        let colors = cx.theme().colors;
        self.sidebar.place_drop_bounds.borrow_mut().clear();
        self.sidebar.bookmark_row_bounds.borrow_mut().clear();
        if !cx.has_active_drag() {
            self.sidebar.bookmark_insertion = None;
            self.drag.file_pointer = None;
            self.drag.file_scroll_task.take();
        }

        let sidebar_width = self.sidebar_width(window, cx);
        let workspace_width = (f32::from(window.bounds().size.width) - f32::from(sidebar_width))
            .max(MIN_BROWSER_WIDTH + MIN_PREVIEW_WIDTH);
        if self.preview.width.get() == px(0.0) {
            self.preview.width.set(px(workspace_width * 0.4));
        }

        let topbar = self.render_topbar(sidebar_width, window, cx);
        let sidebar = self.render_sidebar(sidebar_width, cx);
        let browser = bind_actions(div().id("browser-pane"), cx)
            .key_context(BROWSER_KEY_CONTEXT)
            .track_focus(&self.browser_focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.focus_browser(window, cx)),
            )
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .p_3()
            .bg(colors.background)
            .border_r_1()
            .border_color(colors.border)
            .child(self.render_browser(cx));
        let preview = self.render_preview_pane(window, cx);

        let pane_view = cx.entity();
        let entry_menu = self.render_entry_menu(window, cx);
        let bookmark_menu = self.render_bookmark_menu(window, cx);
        let picker_bar = self.render_picker_bar(cx);
        // gpui-component's Root stores dialog and notification state but
        // does not attach those layers in Root::render. Mount its public layer
        // renderers here so WindowExt dialogs/notifications are actually
        // visible while retaining the component implementations.
        let dialog_layer = Root::render_dialog_layer(window, cx);
        // gpui-component hardcodes NotificationList to top-right and exposes
        // no placement option. Keep the component notifications and
        // lifecycle, but mount their public entities in Marcel's bottom-right
        // stack until the component supports configurable placement.
        let notifications = Root::read(window, cx).notification.read(cx).notifications();
        let visible_from = notifications.len().saturating_sub(10);
        let operation_progress = self.render_operation_progress(cx);
        let status_layer = (operation_progress.is_some() || !notifications.is_empty()).then(|| {
            div()
                .absolute()
                .right_4()
                .bottom_4()
                .flex()
                .flex_col()
                .gap_3()
                .children(operation_progress)
                .children(notifications.into_iter().skip(visible_from))
        });

        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(colors.background)
            .text_color(colors.foreground)
            .on_drag_move::<FileDrag>(|_, window, cx| {
                cx.set_active_drag_cursor_style(CursorStyle::OperationNotAllowed, window);
            })
            .on_drag_move::<BookmarkDrag>(|_, window, cx| {
                cx.set_active_drag_cursor_style(CursorStyle::OperationNotAllowed, window);
            })
            .on_key_down(cx.listener(Self::on_window_key_down))
            .child(topbar)
            .child(
                h_flex().flex_1().min_h_0().w_full().child(sidebar).child(
                    div().flex_1().min_w_0().h_full().child(
                        h_resizable("workspace-panes")
                            .on_resize(move |_, _, cx| {
                                pane_view.update(cx, |this, cx| {
                                    this.start_preview_wrap(cx);
                                    cx.notify();
                                });
                            })
                            .child(
                                resizable_panel()
                                    .size(px(workspace_width * 0.6))
                                    .size_range(px(MIN_BROWSER_WIDTH)..Pixels::MAX)
                                    .child(browser),
                            )
                            .child(
                                resizable_panel()
                                    .size(px(workspace_width * 0.4))
                                    .size_range(px(MIN_PREVIEW_WIDTH)..px(MAX_PREVIEW_WIDTH))
                                    .child(preview),
                            ),
                    ),
                ),
            )
            .children(picker_bar)
            .children(entry_menu)
            .children(bookmark_menu)
            .children(dialog_layer)
            .children(status_layer)
    }
}
