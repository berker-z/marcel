//! The Properties dialog: what one item is, or what a selection adds up to.
//!
//! The dialog is a view of its own so the facts can arrive while it is open.
//! The common facts and the type details come back from one background read;
//! a folder's totals stream in as the walk finds them. Closing the dialog
//! drops the view, and dropping the view cancels both.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};

use gpui::prelude::*;
use gpui::{AnyElement, Context, FontWeight, Hsla, Subscription, Task, Window, div, px};
use gpui_component::{
    ActiveTheme as _, WindowExt as _, button::ButtonVariant, checkbox::Checkbox,
    dialog::DialogButtonProps, h_flex, v_flex,
};

use crate::{
    browse::entries::format_size,
    names::display_path_name,
    operations::OperationEvent,
    preview::details::{
        self, AccessClass, Details, ItemProperties, ObjectKind, TreeTotals, describe_access,
        symbolic_mode,
    },
    preview::media::{describe_channels, format_duration},
};

use super::{Marcel, dialogs::footer};

const DIALOG_WIDTH: f32 = 520.0;
const LABEL_WIDTH: f32 = 112.0;

/// How many of the selected items are what, for a multi-selection.
#[derive(Clone, Copy, Debug, Default)]
struct RootKinds {
    folders: usize,
    files: usize,
    other: usize,
}

pub(super) struct PropertiesView {
    paths: Vec<PathBuf>,
    /// One item's facts, once read, or why they could not be.
    item: Option<Result<ItemProperties, String>>,
    /// What the selected items are, for more than one.
    roots: Option<RootKinds>,
    /// What lies under the selection, growing while the walk runs.
    totals: Option<TreeTotals>,
    cancelled: Arc<AtomicBool>,
    _tasks: Vec<Task<()>>,
    _subscription: Option<Subscription>,
}

impl PropertiesView {
    fn new(paths: Vec<PathBuf>, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            paths,
            item: None,
            roots: None,
            totals: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            _tasks: Vec::new(),
            _subscription: None,
        };
        match this.paths.as_slice() {
            [path] => {
                this.start_inspecting(path.clone(), true, cx);
                // A permission change the dialog asked for lands like any
                // other operation, and the facts are re-read when it does.
                let operations = crate::operations::global(cx);
                this._subscription =
                    Some(cx.subscribe(&operations, |this, _, event: &OperationEvent, cx| {
                        if let OperationEvent::Applied { changes, .. } = event
                            && let [path] = this.paths.as_slice()
                            && changes.upserted.iter().any(|changed| changed == path)
                        {
                            this.start_inspecting(path.clone(), false, cx);
                        }
                    }));
            }
            _ => this.start_summarizing(cx),
        }
        this
    }

    /// Read the item's facts; `measure` starts the folder walk as well,
    /// which a re-read after a permission change has no reason to repeat.
    fn start_inspecting(&mut self, path: PathBuf, measure: bool, cx: &mut Context<Self>) {
        let cancelled = self.cancelled.clone();
        let read = cx.background_executor().spawn(smol::unblock({
            let path = path.clone();
            move || details::inspect(&path, &cancelled).map_err(|error| error.to_string())
        }));
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = read.await;
            let _ = this.update(cx, |this, cx| {
                let is_folder = matches!(&result, Ok(item) if item.object == ObjectKind::Directory);
                this.item = Some(result);
                if is_folder && measure {
                    this.start_measuring(vec![path], cx);
                }
                cx.notify();
            });
        }));
    }

    /// Ask for one permission bit to be set or cleared. The change goes
    /// through the operation coordinator like every other edit, so it is
    /// journalled, undoable, and reported the same way.
    fn set_permission(
        &mut self,
        class: AccessClass,
        bit: u32,
        granted: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Ok(item)) = &self.item else {
            return;
        };
        let mode = if granted { item.mode | class.bit(bit) } else { item.mode & !class.bit(bit) };
        let path = item.path.clone();
        let origin = window.window_handle();
        crate::operations::global(cx)
            .update(cx, |operations, cx| operations.start_set_mode(path, mode, origin, cx));
    }

    fn start_summarizing(&mut self, cx: &mut Context<Self>) {
        let paths = self.paths.clone();
        let count = cx.background_executor().spawn(smol::unblock({
            let paths = paths.clone();
            move || {
                paths.iter().fold(RootKinds::default(), |mut kinds, path| {
                    match fs::symlink_metadata(path) {
                        Ok(metadata) if metadata.is_dir() => kinds.folders += 1,
                        Ok(metadata) if metadata.is_file() => kinds.files += 1,
                        _ => kinds.other += 1,
                    }
                    kinds
                })
            }
        }));
        self._tasks.push(cx.spawn(async move |this, cx| {
            let kinds = count.await;
            let _ = this.update(cx, |this, cx| {
                this.roots = Some(kinds);
                cx.notify();
            });
        }));
        self.start_measuring(paths, cx);
    }

    fn start_measuring(&mut self, roots: Vec<PathBuf>, cx: &mut Context<Self>) {
        let (sender, receiver) = async_channel::unbounded();
        let cancelled = self.cancelled.clone();
        let walk = cx.background_executor().spawn(smol::unblock(move || {
            details::measure_tree(&roots, &cancelled, |totals| {
                let _ = sender.try_send(totals);
            });
        }));
        let pump = cx.spawn(async move |this, cx| {
            while let Ok(totals) = receiver.recv().await {
                let alive = this.update(cx, |this, cx| {
                    this.totals = Some(totals);
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        self._tasks.extend([walk, pump]);
    }
}

impl Drop for PropertiesView {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// Rendering.

/// One labelled fact.
struct Row {
    label: &'static str,
    value: RowValue,
    /// A quieter second line: the raw MIME type, a caveat.
    note: Option<String>,
    color: Option<Hsla>,
}

enum RowValue {
    Text(String),
    /// One class's three permission bits, as checkboxes that change them.
    Access {
        class: AccessClass,
        mode: u32,
        folder: bool,
    },
}

fn row(label: &'static str, value: impl Into<String>) -> Row {
    Row { label, value: RowValue::Text(value.into()), note: None, color: None }
}

fn access_row(class: AccessClass, mode: u32, folder: bool) -> Row {
    Row {
        label: class.label(),
        value: RowValue::Access { class, mode, folder },
        note: None,
        color: None,
    }
}

impl Row {
    fn note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }
}

fn format_time(time: SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(time).format("%-d %B %Y, %H:%M").to_string()
}

/// "4.2 MiB (4,401,234 bytes)"; small sizes are stated once.
fn describe_bytes(bytes: u64) -> String {
    let size = format_size(Some(bytes));
    if bytes < 1024 { size } else { format!("{size} ({} bytes)", group_digits(bytes)) }
}

fn group_digits(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

fn plural(count: u64, one: &str, many: &str) -> String {
    format!("{} {}", group_digits(count), if count == 1 { one } else { many })
}

/// "3 folders, 12 files" and "4.2 MiB", with the walk's state folded in.
fn describe_totals(totals: Option<TreeTotals>) -> (String, String) {
    let Some(totals) = totals else {
        return ("Counting…".to_string(), "Counting…".to_string());
    };
    let mut parts =
        vec![plural(totals.folders, "folder", "folders"), plural(totals.files, "file", "files")];
    if totals.other > 0 {
        parts.push(plural(totals.other, "other item", "other items"));
    }
    let mut contents = parts.join(", ");
    let mut size = describe_bytes(totals.bytes);
    if totals.capped {
        contents = format!("More than {contents}");
        size = format!("More than {size}");
    } else if !totals.finished {
        contents.push_str(" (counting…)");
        size.push_str(" (counting…)");
    }
    if totals.unreadable > 0 {
        contents.push_str(&format!(
            "; {} could not be read",
            plural(totals.unreadable, "entry", "entries")
        ));
    }
    (contents, size)
}

fn location(path: &Path) -> String {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(path)
        .display()
        .to_string()
}

impl PropertiesView {
    fn identity_rows(&self, item: &ItemProperties) -> Vec<Row> {
        let mut rows = Vec::new();
        let kind = match &item.mime {
            Some(mime) if *mime != item.kind => row("Kind", &item.kind).note(mime.clone()),
            _ => row("Kind", &item.kind),
        };
        rows.push(kind);
        if let ObjectKind::Symlink { target } = &item.object {
            rows.push(row(
                "Points to",
                target
                    .as_ref()
                    .map(|target| target.display().to_string())
                    .unwrap_or_else(|| "an unreadable target".to_string()),
            ));
        }
        rows.push(row("Location", location(&item.path)));
        match item.object {
            ObjectKind::Directory => {
                let (contents, size) = describe_totals(self.totals);
                rows.push(row("Contents", contents));
                rows.push(row("Size", size));
                if let Some(free) = item.free_space {
                    rows.push(row("Free space", format_size(Some(free))));
                }
            }
            _ => {
                if let Some(size) = item.size {
                    rows.push(row("Size", describe_bytes(size)));
                }
            }
        }
        rows
    }

    fn detail_rows(item: &ItemProperties) -> Vec<Row> {
        match &item.details {
            Details::None => Vec::new(),
            Details::Image { width, height, format } => {
                let mut rows = vec![row("Dimensions", format!("{width} × {height} pixels"))];
                if !format.is_empty() {
                    rows.push(row("Format", format.clone()));
                }
                rows
            }
            Details::Pdf { pages } => vec![row("Pages", group_digits(*pages as u64))],
            Details::Text { lines, truncated } => {
                let count = plural(*lines as u64, "line", "lines");
                vec![if *truncated {
                    row("Length", format!("More than {count}")).note("Counted in the first 256 KiB")
                } else {
                    row("Length", count)
                }]
            }
            Details::Audio { duration, codec, sample_rate, channels, title, artist, album } => {
                let mut rows = Vec::new();
                if let Some(title) = title {
                    rows.push(row("Title", title.clone()));
                }
                if let Some(artist) = artist {
                    rows.push(row("Artist", artist.clone()));
                }
                if let Some(album) = album {
                    rows.push(row("Album", album.clone()));
                }
                if let Some(duration) = duration {
                    rows.push(row("Duration", format_duration(*duration)));
                }
                let stream = match (*sample_rate, *channels) {
                    (0, _) => codec.clone(),
                    (rate, 0) => format!("{codec}, {rate} Hz"),
                    (rate, channels) => {
                        format!("{codec}, {rate} Hz, {}", describe_channels(channels))
                    }
                };
                rows.push(row("Stream", stream));
                rows
            }
            Details::Video { duration, width, height, codec, audio_codec } => {
                let mut rows = Vec::new();
                if let Some(duration) = duration {
                    rows.push(row("Duration", format_duration(*duration)));
                }
                if let (Some(width), Some(height)) = (width, height) {
                    rows.push(row("Dimensions", format!("{width} × {height} pixels")));
                }
                let streams = [codec.as_deref(), audio_codec.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(", ");
                if !streams.is_empty() {
                    rows.push(row("Streams", streams));
                }
                rows
            }
            Details::Archive { entries, expanded } => vec![
                row("Contains", plural(*entries as u64, "entry", "entries")),
                row("Unpacked size", describe_bytes(*expanded)),
            ],
        }
    }

    fn access_rows(item: &ItemProperties) -> Vec<Row> {
        let mut rows = vec![row("Owner", &item.owner), row("Group", &item.group)];
        // A link's bits are not its own to change — `chmod` follows it — so a
        // link shows its bits in words and everything else gets checkboxes.
        let editable = !matches!(item.object, ObjectKind::Symlink { .. });
        for class in AccessClass::ALL {
            rows.push(if editable {
                access_row(class, item.mode, item.object == ObjectKind::Directory)
            } else {
                row(class.label(), describe_access(&item.object, item.mode, class))
            });
        }
        rows.push(row(
            "Mode",
            format!("{} ({:04o})", symbolic_mode(&item.object, item.mode), item.mode),
        ));
        if let Some(time) = item.modified {
            rows.push(row("Modified", format_time(time)));
        }
        if let Some(time) = item.accessed {
            rows.push(row("Accessed", format_time(time)));
        }
        if let Some(time) = item.created {
            rows.push(row("Created", format_time(time)));
        }
        rows
    }

    fn summary_rows(&self) -> Vec<Row> {
        let kinds = match self.roots {
            None => "Counting…".to_string(),
            Some(kinds) => {
                let mut parts = Vec::new();
                if kinds.folders > 0 {
                    parts.push(plural(kinds.folders as u64, "folder", "folders"));
                }
                if kinds.files > 0 {
                    parts.push(plural(kinds.files as u64, "file", "files"));
                }
                if kinds.other > 0 {
                    parts.push(plural(kinds.other as u64, "other item", "other items"));
                }
                parts.join(", ")
            }
        };
        // The walk counts a selected file as itself, so what lies *inside* the
        // selected folders is the total less the selected files.
        let inside = match (self.totals, self.roots) {
            (Some(totals), Some(roots)) => Some(TreeTotals {
                files: totals.files.saturating_sub(roots.files as u64),
                other: totals.other.saturating_sub(roots.other as u64),
                ..totals
            }),
            (totals, _) => totals,
        };
        let (contents, _) = describe_totals(inside);
        let (_, size) = describe_totals(self.totals);
        let mut rows = vec![row("Selected", kinds)];
        if let Some(first) = self.paths.first() {
            rows.push(row("Location", location(first)));
        }
        if self.roots.is_some_and(|kinds| kinds.folders > 0) {
            rows.push(row("Inside folders", contents));
        }
        rows.push(row("Total size", size));
        rows
    }

    fn render_section(rows: Vec<Row>, first: bool, cx: &Context<Self>) -> Option<AnyElement> {
        if rows.is_empty() {
            return None;
        }
        let colors = cx.theme().colors;
        // A change is refused while another operation runs, so say so with
        // the control rather than with a click that does nothing.
        let busy = crate::operations::global(cx).read(cx).is_busy();
        Some(
            v_flex()
                .gap_2()
                .when(!first, |this| this.pt_3().border_t_1().border_color(colors.border))
                .children(rows.into_iter().map(|row| {
                    let value = match row.value {
                        RowValue::Text(text) => div()
                            .when_some(row.color, |this, color| this.text_color(color))
                            .child(text)
                            .into_any_element(),
                        RowValue::Access { class, mode, folder } => {
                            Self::render_access(class, mode, folder, busy, cx)
                        }
                    };
                    h_flex()
                        .items_start()
                        .gap_3()
                        .child(
                            div()
                                .w(px(LABEL_WIDTH))
                                .flex_shrink_0()
                                .text_color(colors.muted_foreground)
                                .child(row.label),
                        )
                        .child(v_flex().flex_1().min_w_0().child(value).when_some(
                            row.note,
                            |this, note| {
                                this.child(
                                    div().text_xs().text_color(colors.muted_foreground).child(note),
                                )
                            },
                        ))
                }))
                .into_any_element(),
        )
    }

    /// Three checkboxes for one class, worded the way `describe_access` words
    /// the same bits.
    fn render_access(
        class: AccessClass,
        mode: u32,
        folder: bool,
        busy: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        use gpui_component::Disableable as _;

        let permissions = [
            (0o4, if folder { "List" } else { "Read" }),
            (0o2, if folder { "Create and delete" } else { "Write" }),
            (0o1, if folder { "Enter" } else { "Execute" }),
        ];
        h_flex()
            .gap_4()
            .flex_wrap()
            .children(permissions.into_iter().map(|(bit, label)| {
                Checkbox::new((class.label(), bit as usize))
                    .label(label)
                    .checked(mode & class.bit(bit) != 0)
                    .disabled(busy)
                    .on_click(cx.listener(move |this, checked: &bool, window, cx| {
                        this.set_permission(class, bit, *checked, window, cx);
                    }))
            }))
            .into_any_element()
    }
}

impl Render for PropertiesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors;
        let (heading, sections): (String, Vec<Vec<Row>>) = match (self.paths.as_slice(), &self.item)
        {
            ([_], None) => ("Reading…".to_string(), Vec::new()),
            ([path], Some(Err(error))) => (
                display_path_name(path),
                vec![vec![row("Error", error.clone()).color(colors.danger)]],
            ),
            ([_], Some(Ok(item))) => (
                item.name.clone(),
                vec![self.identity_rows(item), Self::detail_rows(item), Self::access_rows(item)],
            ),
            (paths, _) => (format!("{} items", paths.len()), vec![self.summary_rows()]),
        };
        v_flex()
            .w_full()
            .gap_3()
            .text_sm()
            .child(div().font_weight(FontWeight::SEMIBOLD).whitespace_normal().child(heading))
            .children(
                sections
                    .into_iter()
                    .filter(|rows| !rows.is_empty())
                    .enumerate()
                    .filter_map(|(index, rows)| Self::render_section(rows, index == 0, cx)),
            )
    }
}

impl Marcel {
    /// Properties of the selection, or of the folder shown when nothing is.
    pub(super) fn open_selection_properties(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut paths = self.selected_paths();
        if paths.is_empty() {
            paths.push(self.directory.current_dir.clone());
        }
        self.open_properties(paths, window, cx);
    }

    /// Show what `paths` are. One path gets its full description; several
    /// get a summary. Also the answer to the bus's `ShowItemProperties`.
    pub fn open_properties(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        self.ui.entry_menu = None;
        let view = cx.new(|cx| PropertiesView::new(paths, cx));
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title("Properties")
                .w(px(DIALOG_WIDTH))
                .child(view.clone())
                .button_props(DialogButtonProps::default().ok_text("Close"))
                .footer(footer("Close", ButtonVariant::Primary, false))
                .close_button(false)
        });
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_stated_once_when_small_and_exactly_when_large() {
        assert_eq!(describe_bytes(0), "0 B");
        assert_eq!(describe_bytes(999), "999 B");
        assert_eq!(describe_bytes(4_401_234), "4.2 MiB (4,401,234 bytes)");
        assert_eq!(group_digits(1_234_567), "1,234,567");
        assert_eq!(group_digits(12), "12");
    }

    #[test]
    fn totals_say_whether_they_are_still_growing_or_were_cut_short() {
        let (contents, size) = describe_totals(None);
        assert_eq!((contents.as_str(), size.as_str()), ("Counting…", "Counting…"));

        let running = TreeTotals { folders: 2, files: 1, bytes: 10, ..TreeTotals::default() };
        let (contents, size) = describe_totals(Some(running));
        assert_eq!(contents, "2 folders, 1 file (counting…)");
        assert_eq!(size, "10 B (counting…)");

        let capped = TreeTotals { files: 5, finished: true, capped: true, ..running };
        let (contents, _) = describe_totals(Some(capped));
        assert_eq!(contents, "More than 2 folders, 5 files");

        let partial = TreeTotals { finished: true, unreadable: 1, other: 3, ..running };
        let (contents, size) = describe_totals(Some(partial));
        assert_eq!(contents, "2 folders, 1 file, 3 other items; 1 entry could not be read");
        assert_eq!(size, "10 B");
    }

    #[test]
    fn the_root_is_its_own_location() {
        assert_eq!(location(Path::new("/home/me/file.txt")), "/home/me");
        assert_eq!(location(Path::new("/")), "/");
    }
}
