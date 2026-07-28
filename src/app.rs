use std::{
    ops::Range,
    path::{Path, PathBuf},
};

use gpui::{
    AnyElement, ClickEvent, Context, Hsla, IntoElement, ObjectFit, ParentElement, Render, Styled,
    Task, Window, div, img, prelude::*, px, uniform_list,
};
use gpui_component::{
    ActiveTheme, Disableable, Sizable,
    button::{Button, ButtonVariants},
    h_flex,
    list::ListItem,
    text::TextView,
};

use crate::{
    fs::{DirectoryUpdate, FileEntry, format_size, merge_sorted_entries, stream_directory},
    history::NavigationHistory,
    places::{Place, discover as discover_places},
    preview::{Preview, PreviewState, load_preview},
};

pub struct Marcel {
    current_dir: PathBuf,
    entries: Vec<FileEntry>,
    selected_path: Option<PathBuf>,
    directory_loading: bool,
    directory_error: Option<String>,
    directory_ticket: u64,
    directory_task: Option<Task<()>>,
    preview_state: PreviewState,
    preview_ticket: u64,
    preview_task: Option<Task<()>>,
    history: NavigationHistory,
    places: Vec<Place>,
    places_loading: bool,
    places_task: Option<Task<()>>,
}

impl Marcel {
    pub fn new(start_dir: PathBuf, cx: &mut Context<Self>) -> Self {
        let start_dir = normalize_start_directory(start_dir);
        let home_dir = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| start_dir.clone());

        let mut this = Self {
            current_dir: start_dir.clone(),
            entries: Vec::new(),
            selected_path: None,
            directory_loading: false,
            directory_error: None,
            directory_ticket: 0,
            directory_task: None,
            preview_state: PreviewState::Empty,
            preview_ticket: 0,
            preview_task: None,
            history: NavigationHistory::new(start_dir),
            places: vec![Place::home(home_dir.clone())],
            places_loading: true,
            places_task: None,
        };
        this.start_places_load(home_dir, cx);
        this.start_directory_load(cx);
        this
    }

    fn start_places_load(&mut self, home: PathBuf, cx: &mut Context<Self>) {
        let load_task = cx
            .background_executor()
            .spawn(smol::unblock(move || discover_places(&home)));

        self.places_task = Some(cx.spawn(async move |this, cx| {
            let places = load_task.await;
            let _ = this.update(cx, |this, cx| {
                this.places = places;
                this.places_loading = false;
                cx.notify();
            });
        }));
    }

    fn start_directory_load(&mut self, cx: &mut Context<Self>) {
        self.directory_ticket = self.directory_ticket.wrapping_add(1);
        let ticket = self.directory_ticket;
        let path = self.current_dir.clone();

        self.directory_task.take();
        self.entries.clear();
        self.directory_error = None;
        self.directory_loading = true;
        self.clear_selection();

        let (sender, receiver) = async_channel::unbounded();
        cx.background_executor()
            .spawn(smol::unblock(move || stream_directory(&path, sender)))
            .detach();

        self.directory_task = Some(cx.spawn(async move |this, cx| {
            while let Ok(update) = receiver.recv().await {
                let should_continue = this
                    .update(cx, |this, cx| {
                        if ticket != this.directory_ticket {
                            return false;
                        }

                        match update {
                            DirectoryUpdate::Batch(batch) => {
                                this.entries =
                                    merge_sorted_entries(std::mem::take(&mut this.entries), batch);
                            }
                            DirectoryUpdate::Done => {
                                this.directory_loading = false;
                            }
                            DirectoryUpdate::Error(error) => {
                                this.directory_loading = false;
                                this.directory_error = Some(error);
                            }
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);

                if !should_continue {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn navigate_to(&mut self, path: PathBuf, add_to_history: bool, cx: &mut Context<Self>) {
        if path == self.current_dir {
            return;
        }
        if add_to_history {
            self.history.push(&path);
        }
        self.current_dir = path;
        self.start_directory_load(cx);
    }

    fn go_back(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.history.go_back() {
            self.current_dir = path;
            self.start_directory_load(cx);
        }
    }

    fn go_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.history.go_forward() {
            self.current_dir = path;
            self.start_directory_load(cx);
        }
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        if let Some(parent) = self.current_dir.parent() {
            self.navigate_to(parent.to_path_buf(), true, cx);
        }
    }

    fn clear_selection(&mut self) {
        self.selected_path = None;
        self.preview_ticket = self.preview_ticket.wrapping_add(1);
        self.preview_task.take();
        self.preview_state = PreviewState::Empty;
    }

    fn activate_entry(&mut self, path: &Path, click_count: usize, cx: &mut Context<Self>) {
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .cloned()
        else {
            return;
        };

        self.selected_path = Some(entry.path.clone());
        self.start_preview(entry.clone(), cx);

        if click_count >= 2 {
            self.open_entry(entry, cx);
        }
    }

    fn start_preview(&mut self, entry: FileEntry, cx: &mut Context<Self>) {
        self.preview_ticket = self.preview_ticket.wrapping_add(1);
        let ticket = self.preview_ticket;

        // Like Yazi's preview task, replacing this handle cancels the previous
        // foreground task. The ticket also prevents a late result from
        // becoming current:
        // https://github.com/sxyazi/yazi/blob/main/yazi-core/src/tab/preview.rs
        self.preview_task.take();
        self.preview_state = PreviewState::Loading {
            name: entry.name.clone(),
        };

        let load_task = cx
            .background_executor()
            .spawn(smol::unblock(move || load_preview(&entry)));

        self.preview_task = Some(cx.spawn(async move |this, cx| {
            let result = load_task.await;
            let _ = this.update(cx, |this, cx| {
                if ticket != this.preview_ticket {
                    return;
                }
                this.preview_state = match result {
                    Ok(preview) => PreviewState::Ready(preview),
                    Err(error) => PreviewState::Error(error.to_string()),
                };
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn open_entry(&mut self, entry: FileEntry, cx: &mut Context<Self>) {
        if entry.navigable {
            self.navigate_to(entry.path, true, cx);
            return;
        }

        #[cfg(target_os = "linux")]
        {
            let path = entry.path;
            let open_task = cx
                .background_executor()
                .spawn(crate::system_open::open_file(path.clone()));
            cx.spawn(async move |this, cx| {
                if let Err(error) = open_task.await {
                    let _ = this.update(cx, |this, cx| {
                        if this.selected_path.as_ref() == Some(&path) {
                            this.preview_state = PreviewState::Error(error.to_string());
                            cx.notify();
                        }
                    });
                }
            })
            .detach();
        }

        #[cfg(not(target_os = "linux"))]
        cx.open_with_system(&entry.path);
    }

    fn render_place(&self, index: usize, place: Place, cx: &mut Context<Self>) -> Button {
        Button::new(("place", index))
            .ghost()
            .label(place.label)
            .w_full()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.navigate_to(place.path.clone(), true, cx);
            }))
    }

    fn render_browser(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        if self.entries.is_empty() {
            let message = if let Some(error) = &self.directory_error {
                format!("Could not read this folder\n{error}")
            } else if self.directory_loading {
                "Loading folder…".to_string()
            } else {
                "This folder is empty".to_string()
            };

            return div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_color(colors.muted_foreground)
                .child(message)
                .into_any_element();
        }

        let selected_path = self.selected_path.clone();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(
                uniform_list(
                    "directory-entries",
                    self.entries.len(),
                    cx.processor(move |this, range: Range<usize>, _window, cx| {
                        range
                            .filter_map(|index| {
                                let entry = this.entries.get(index)?.clone();
                                let path = entry.path.clone();
                                let selected =
                                    selected_path.as_ref().is_some_and(|value| value == &path);

                                Some(
                                    ListItem::new(index)
                                        .h(px(36.0))
                                        .rounded_md()
                                        .selected(selected)
                                        .on_click(cx.listener(
                                            move |this, event: &ClickEvent, _, cx| {
                                                this.activate_entry(&path, event.click_count(), cx);
                                            },
                                        ))
                                        .child(
                                            h_flex()
                                                .w_full()
                                                .gap_2()
                                                .child(
                                                    div()
                                                        .w(px(16.0))
                                                        .text_color(colors.primary)
                                                        .child(entry.icon()),
                                                )
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .overflow_hidden()
                                                        .text_ellipsis()
                                                        .child(entry.name),
                                                )
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(colors.muted_foreground)
                                                        .child(format_size(entry.size)),
                                                ),
                                        ),
                                )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .h_full(),
            )
            .when(self.directory_loading, |this| {
                this.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(colors.muted_foreground)
                        .child(format!("Loading… {} items", self.entries.len())),
                )
            })
            .into_any_element()
    }

    fn render_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        match &self.preview_state {
            PreviewState::Empty => {
                centered_preview_message("Select a file to preview", colors.muted_foreground)
            }
            PreviewState::Loading { name } => {
                centered_preview_message(format!("Loading {name}…"), colors.muted_foreground)
            }
            PreviewState::Error(error) => {
                centered_preview_message(format!("Preview failed\n{error}"), colors.danger)
            }
            PreviewState::Ready(Preview::Image { path, .. }) => {
                let muted = colors.muted_foreground;
                let danger = colors.danger;
                img(path.clone())
                    .id(("preview-image", self.preview_ticket))
                    .size_full()
                    .object_fit(ObjectFit::Contain)
                    .with_loading(move || centered_preview_message("Decoding image…", muted))
                    .with_fallback(move || {
                        centered_preview_message("This image could not be decoded", danger)
                    })
                    .into_any_element()
            }
            PreviewState::Ready(Preview::Text {
                contents,
                lines,
                language,
                markdown,
                render_rich,
                ..
            }) => {
                if *render_rich {
                    let source = if *markdown {
                        contents.clone()
                    } else {
                        code_fence(contents, language)
                    };
                    TextView::markdown(("preview-text", self.preview_ticket), source, window, cx)
                        .selectable(true)
                        .scrollable(true)
                        .size_full()
                        .p_3()
                        .into_any_element()
                } else {
                    let lines = lines.clone();
                    let foreground = colors.foreground;
                    let muted = colors.muted_foreground;
                    let mono_font = cx.theme().mono_font_family.clone();
                    let mono_font_size = cx.theme().mono_font_size;

                    uniform_list(
                        ("preview-text-lines", self.preview_ticket),
                        lines.len(),
                        move |range, _, _| {
                            range
                                .map(|index| {
                                    div()
                                        .flex()
                                        .h(px(20.0))
                                        .w_full()
                                        .items_center()
                                        .font_family(mono_font.clone())
                                        .text_size(mono_font_size)
                                        .child(
                                            div()
                                                .w(px(52.0))
                                                .flex_none()
                                                .pr_3()
                                                .text_color(muted)
                                                .child(format!("{:>4}", index + 1)),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                .text_color(foreground)
                                                .child(lines[index].clone()),
                                        )
                                })
                                .collect::<Vec<_>>()
                        },
                    )
                    .size_full()
                    .px_3()
                    .py_2()
                    .into_any_element()
                }
            }
            PreviewState::Ready(Preview::Metadata { summary }) => {
                centered_preview_message(summary.clone(), colors.muted_foreground)
            }
        }
    }
}

impl Render for Marcel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors;
        let place_buttons = self
            .places
            .clone()
            .into_iter()
            .enumerate()
            .map(|(index, place)| self.render_place(index, place, cx))
            .collect::<Vec<_>>();

        let sidebar = div()
            .flex()
            .flex_col()
            .w(px(220.0))
            .h_full()
            .p_4()
            .gap_2()
            .bg(colors.sidebar)
            .border_r_1()
            .border_color(colors.sidebar_border)
            .text_color(colors.sidebar_foreground)
            .child(div().text_lg().child("Marcel"))
            .child(
                div()
                    .pt_3()
                    .text_sm()
                    .text_color(colors.muted_foreground)
                    .child("Places"),
            )
            .children(place_buttons)
            .when(self.places_loading, |this| {
                this.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(colors.muted_foreground)
                        .child("Finding places…"),
                )
            });

        let browser = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .p_4()
            .gap_3()
            .bg(colors.background)
            .border_r_1()
            .border_color(colors.border)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("back")
                            .ghost()
                            .small()
                            .label("Back")
                            .disabled(!self.history.can_go_back())
                            .on_click(cx.listener(|this, _, _, cx| this.go_back(cx))),
                    )
                    .child(
                        Button::new("forward")
                            .ghost()
                            .small()
                            .label("Forward")
                            .disabled(!self.history.can_go_forward())
                            .on_click(cx.listener(|this, _, _, cx| this.go_forward(cx))),
                    )
                    .child(
                        Button::new("up")
                            .ghost()
                            .small()
                            .label("Up")
                            .disabled(self.current_dir.parent().is_none())
                            .on_click(cx.listener(|this, _, _, cx| this.go_up(cx))),
                    )
                    .child(
                        Button::new("refresh")
                            .ghost()
                            .small()
                            .label("Refresh")
                            .on_click(cx.listener(|this, _, _, cx| this.start_directory_load(cx))),
                    ),
            )
            .child(
                div()
                    .px_2()
                    .text_sm()
                    .text_color(colors.muted_foreground)
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(self.current_dir.display().to_string()),
            )
            .child(self.render_browser(cx));

        let selected_entry = self
            .selected_path
            .as_ref()
            .and_then(|path| self.entries.iter().find(|entry| &entry.path == path));
        let preview_details = selected_entry.map(|entry| {
            let size = format_size(entry.size);
            if size.is_empty() {
                entry.display_kind().to_string()
            } else {
                format!("{} · {size}", entry.display_kind())
            }
        });
        let image_mime = match &self.preview_state {
            PreviewState::Ready(Preview::Image { mime, .. }) => Some(mime.clone()),
            _ => None,
        };
        let truncated = matches!(
            &self.preview_state,
            PreviewState::Ready(Preview::Text {
                truncated: true,
                ..
            })
        );
        let clipped_lines = matches!(
            &self.preview_state,
            PreviewState::Ready(Preview::Text {
                clipped_lines: true,
                ..
            })
        );
        let has_preview_footer =
            preview_details.is_some() || image_mime.is_some() || truncated || clipped_lines;
        let preview_footer = div()
            .flex()
            .flex_col()
            .gap_1()
            .px_4()
            .py_3()
            .when_some(preview_details, |this, details| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(colors.muted_foreground)
                        .child(details),
                )
            })
            .when_some(image_mime, |this, mime| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(colors.muted_foreground)
                        .child(mime),
                )
            })
            .when(truncated, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(colors.warning)
                        .child("Preview limited to the first 256 KiB"),
                )
            })
            .when(clipped_lines, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(colors.warning)
                        .child("Very long lines are shortened in the preview"),
                )
            });

        let preview = div()
            .flex()
            .flex_col()
            .w(px(420.0))
            .h_full()
            .bg(colors.sidebar)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(self.render_preview(window, cx)),
            )
            .when(has_preview_footer, |this| this.child(preview_footer));

        div()
            .flex()
            .size_full()
            .bg(colors.background)
            .text_color(colors.foreground)
            .child(sidebar)
            .child(browser)
            .child(preview)
    }
}

fn normalize_start_directory(path: PathBuf) -> PathBuf {
    if path.is_dir() {
        path
    } else {
        path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/"))
    }
}

fn centered_preview_message(message: impl Into<String>, color: Hsla) -> AnyElement {
    div()
        .flex()
        .size_full()
        .items_center()
        .justify_center()
        .text_color(color)
        .child(message.into())
        .into_any_element()
}

fn code_fence(contents: &str, language: &str) -> String {
    let mut fence = "```".to_string();
    while contents.contains(&fence) {
        fence.push('`');
    }
    format!("{fence}{language}\n{contents}\n{fence}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_fence_grows_past_fences_in_content() {
        let rendered = code_fence("contains ``` here", "text");
        assert!(rendered.starts_with("````text\n"));
        assert!(rendered.ends_with("\n````"));
    }

    #[test]
    fn start_path_falls_back_to_parent_for_files() {
        let path = PathBuf::from("/tmp/file.txt");
        assert_eq!(normalize_start_directory(path), PathBuf::from("/tmp"));
    }
}
