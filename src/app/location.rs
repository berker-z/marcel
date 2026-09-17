//! The location bar: breadcrumbs that become a text field on click or
//! Ctrl+L, and resolve what is typed into a folder to show.

use std::path::{Path, PathBuf};

use gpui::prelude::*;
use gpui::{AnyElement, Context, Entity, IntoElement, Window, div};
use gpui_component::{
    ActiveTheme as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState, SelectAll as InputSelectAll},
    notification::Notification,
};

use crate::desktop::launch::{LocationTarget, resolve_location};

use super::{Marcel, navigation::unblock};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Breadcrumb {
    label: String,
    /// `None` for the ellipsis that stands in for elided segments.
    path: Option<PathBuf>,
}

fn breadcrumbs(path: &Path) -> Vec<Breadcrumb> {
    let mut current = PathBuf::new();
    let mut crumbs = Vec::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let label = match component {
            std::path::Component::RootDir => "root".to_string(),
            std::path::Component::Prefix(prefix) => {
                prefix.as_os_str().to_string_lossy().into_owned()
            }
            std::path::Component::CurDir => continue,
            std::path::Component::ParentDir => "..".to_string(),
            std::path::Component::Normal(name) => name.to_string_lossy().into_owned(),
        };
        crumbs.push(Breadcrumb { label, path: Some(current.clone()) });
    }
    crumbs
}

/// Keep the root and the deepest segments, eliding the middle.
fn compact(crumbs: Vec<Breadcrumb>, max_items: usize) -> Vec<Breadcrumb> {
    if crumbs.len() <= max_items || max_items < 3 {
        return crumbs;
    }
    let tail_start = crumbs.len() - (max_items - 2);
    let mut compacted = Vec::with_capacity(max_items);
    compacted.push(crumbs[0].clone());
    compacted.push(Breadcrumb { label: "…".to_string(), path: None });
    compacted.extend_from_slice(&crumbs[tail_start..]);
    compacted
}

impl Marcel {
    pub(super) fn render_location_bar(
        &self,
        max_breadcrumbs: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors;
        if self.ui.location.active {
            let suffix = if self.ui.location.resolving {
                Some(div().text_xs().text_color(colors.muted_foreground).child("…"))
            } else {
                self.ui
                    .location
                    .error
                    .as_ref()
                    .map(|_| div().text_sm().text_color(colors.danger).child("!"))
            };
            return Input::new(&self.ui.location_input)
                .small()
                .h_7()
                .w_full()
                .when(self.ui.location.error.is_some(), |input| input.border_color(colors.danger))
                .when_some(suffix, |input, suffix| input.suffix(suffix))
                .into_any_element();
        }

        let crumbs = if self.sidebar.browsing_trash {
            vec![Breadcrumb { label: "Trash".to_string(), path: None }]
        } else {
            compact(breadcrumbs(&self.directory.current_dir), max_breadcrumbs)
        };
        let last = crumbs.len().saturating_sub(1);
        let mut items = Vec::with_capacity(crumbs.len() * 2);
        for (index, crumb) in crumbs.into_iter().enumerate() {
            if index > 0 {
                items.push(
                    div()
                        .flex_none()
                        .text_sm()
                        .text_color(colors.muted_foreground)
                        .child("/")
                        .into_any_element(),
                );
            }
            let button = |label: String| {
                Button::new(("location-breadcrumb", index)).xsmall().compact().ghost().label(label)
            };
            items.push(match crumb.path {
                Some(path) if index != last => button(crumb.label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.navigate_to(path.clone(), true, cx);
                    }))
                    .into_any_element(),
                None if crumb.label == "…" => button(crumb.label)
                    .on_click(cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        this.begin_location_edit(window, cx);
                    }))
                    .into_any_element(),
                _ => div()
                    .flex_none()
                    .text_sm()
                    .text_color(colors.foreground)
                    .child(crumb.label)
                    .into_any_element(),
            });
        }

        div()
            .id("location-breadcrumbs")
            .flex_1()
            .min_w_0()
            .h_7()
            .px_3()
            .flex()
            .items_center()
            .overflow_hidden()
            .rounded(cx.theme().radius)
            .bg(colors.background)
            .border_1()
            .border_color(colors.border)
            .cursor_text()
            .on_click(cx.listener(|this, _, window, cx| this.begin_location_edit(window, cx)))
            .child(h_flex().min_w_0().gap_1().overflow_hidden().children(items))
            .into_any_element()
    }

    pub(super) fn on_location_input_event(
        &mut self,
        input: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => {
                self.ui.location.error = None;
                if self.ui.location.resolving {
                    self.ui.location.bump();
                    self.ui.location.resolving = false;
                }
                cx.notify();
            }
            InputEvent::PressEnter { .. } => self.submit_location(input, window, cx),
            InputEvent::Blur => {
                if self.ui.location.active && !self.ui.location.resolving {
                    self.ui.location.end();
                    cx.notify();
                }
            }
            InputEvent::Focus => {}
        }
    }

    pub(super) fn begin_location_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ui.location.begin();
        let value = if self.sidebar.browsing_trash {
            String::new()
        } else {
            self.directory.current_dir.display().to_string()
        };
        let input = self.ui.location_input.clone();
        input.update(cx, |input, cx| input.set_value(value, window, cx));
        cx.notify();
        cx.defer_in(window, move |_, window, cx| {
            input.update(cx, |input, cx| input.focus(window, cx));
            window.dispatch_action(Box::new(InputSelectAll), cx);
        });
    }

    pub(super) fn cancel_location_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ui.location.end();
        self.focus_browser(window, cx);
        cx.notify();
    }

    fn submit_location(
        &mut self,
        input: &Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.ui.location.resolving {
            return;
        }
        let value = input.read(cx).value().to_string();
        let current_dir = self.directory.current_dir.clone();
        let home_dir = self.home_dir.clone();
        let ticket = self.ui.location.bump();
        self.ui.location.resolving = true;
        self.ui.location.error = None;
        cx.notify();

        let resolve = unblock(cx, move || resolve_location(&value, &current_dir, Some(&home_dir)));
        cx.spawn_in(window, async move |this, window| {
            let result = resolve.await;
            let _ = this.update_in(window, |this, window, cx| {
                if ticket != this.ui.location.ticket || !this.ui.location.active {
                    return;
                }
                this.ui.location.resolving = false;
                match result {
                    Ok(LocationTarget { directory, reveal }) => {
                        this.ui.location.end();
                        this.open_external_location(
                            directory,
                            reveal.into_iter().collect(),
                            window,
                            cx,
                        );
                    }
                    Err(error) => {
                        this.ui.location.error = Some(error.clone());
                        window.push_notification(Notification::error(error), cx);
                        this.ui.location_input.update(cx, |input, cx| input.focus(window, cx));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crumb(label: &str, path: &str) -> Breadcrumb {
        Breadcrumb { label: label.to_string(), path: Some(PathBuf::from(path)) }
    }

    #[test]
    fn breadcrumbs_build_clickable_progressive_paths() {
        assert_eq!(
            breadcrumbs(Path::new("/home/test/Projects/marcel")),
            vec![
                crumb("root", "/"),
                crumb("home", "/home"),
                crumb("test", "/home/test"),
                crumb("Projects", "/home/test/Projects"),
                crumb("marcel", "/home/test/Projects/marcel"),
            ]
        );
    }

    #[test]
    fn narrow_breadcrumbs_keep_root_and_the_deepest_segments() {
        let compacted = compact(breadcrumbs(Path::new("/home/test/Projects/marcel/src")), 4);
        assert_eq!(
            compacted,
            vec![
                crumb("root", "/"),
                Breadcrumb { label: "…".to_string(), path: None },
                crumb("marcel", "/home/test/Projects/marcel"),
                crumb("src", "/home/test/Projects/marcel/src"),
            ]
        );
    }
}
