//! Marcel's windows, and who owns the list of them.
//!
//! Operations and user data belong to the application rather than to the
//! window that started them, and so do the windows themselves: a window opened
//! from a context menu and a window opened by a desktop request are the same
//! kind of thing, registered in one place.

use std::{path::PathBuf, sync::Arc, sync::OnceLock};

use gpui::{
    App, AppContext as _, Bounds, Entity, Global, Point, TitlebarOptions, Window, WindowBounds,
    WindowHandle, WindowOptions, px, size,
};
use gpui_component::Root;

use crate::{
    Marcel,
    desktop::{
        bus::APPLICATION_ID,
        picker::{PickerMode, PickerRequest, PickerResponse},
    },
};

/// The size a Marcel window opens at when nothing else decides for it.
const DEFAULT_WINDOW_SIZE: (f32, f32) = (1200.0, 760.0);

/// The narrowest window whose layout still fits inside itself.
///
/// The browser and preview panes each refuse to go below a minimum, and the
/// places sidebar is as wide as its widest label in the current font, so below
/// roughly 844 px the three of them together are wider than the window and the
/// preview pane hangs off the right edge. 900 leaves room for a sidebar wider
/// than this machine's.
///
/// This is a floor, not a fix: the panes should shrink instead of overflowing.
/// A floating desktop honours this and stops the user resizing into the broken
/// layout; a tiling compositor is free to ignore it.
const MIN_WINDOW_SIZE: (f32, f32) = (900.0, 480.0);

/// How far each additional window steps down and to the right.
///
/// A window opened exactly on top of the last one looks like nothing happened.
/// Tiling compositors place windows themselves and ignore this, which is why it
/// is a hint rather than a layout: on Hyprland it costs nothing and changes
/// nothing, and on a floating desktop it is the difference between one window
/// and two.
const WINDOW_CASCADE_STEP: f32 = 32.0;

/// How many steps the cascade takes before returning to the centre.
const WINDOW_CASCADE_LENGTH: usize = 8;

/// The most windows Marcel will hold open at once.
///
/// A launch is a window, and launches arrive from the session bus — where any
/// peer may send `Open` with dozens of URIs in a loop. Far past what a person
/// uses, well before a window flood freezes the session.
pub const MAX_LIVE_WINDOWS: usize = 32;

/// The most file-chooser windows open at once.
///
/// The portal frontend serialises requests per application, so one open
/// dialog per application is the ordinary case and this is far past it. A
/// request over the cap is answered with an error rather than queued behind
/// dialogs the user has not noticed yet.
pub const MAX_LIVE_PICKERS: usize = 8;

/// The size a picker opens at: enough for the three panes without taking the
/// whole screen the way a browsing window may.
const PICKER_WINDOW_SIZE: (f32, f32) = (1080.0, 700.0);

#[derive(Clone)]
pub struct MarcelWindow {
    pub handle: WindowHandle<Root>,
    pub view: Entity<Marcel>,
}

/// Every Marcel window this process has open, in the order they were opened.
#[derive(Default)]
pub struct WindowRegistry {
    windows: Vec<MarcelWindow>,
    /// File-chooser windows, kept apart: a reveal must never navigate a
    /// dialog somebody is in the middle of answering, and a dialog is not
    /// "the Marcel I have" for an `Activate`.
    pickers: Vec<MarcelWindow>,
    opened: usize,
}

struct GlobalWindows(Entity<WindowRegistry>);

impl Global for GlobalWindows {}

pub fn init(cx: &mut App) {
    let registry = cx.new(|_| WindowRegistry::default());
    cx.set_global(GlobalWindows(registry.clone()));
    // A closed window's handle stays in the list until something notices, and
    // the registry is consulted far more often than windows close.
    cx.on_window_closed(move |cx, _| {
        registry.update(cx, |registry, cx| registry.prune(cx));
    })
    .detach();
}

/// The application's window registry.
pub fn global(cx: &App) -> Entity<WindowRegistry> {
    cx.global::<GlobalWindows>().0.clone()
}

/// Open a Marcel window of `size` at `bounds`, titled `title`, with the view
/// `build` makes for it.
fn open_window(
    title: &str,
    bounds: Bounds<gpui::Pixels>,
    build: impl FnOnce(&mut Window, &mut App) -> Entity<Marcel>,
    cx: &mut App,
) -> anyhow::Result<MarcelWindow> {
    let mut view = None;
    let handle = cx.open_window(
        WindowOptions {
            app_id: Some(APPLICATION_ID.to_string()),
            icon: window_icon(),
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(MIN_WINDOW_SIZE.0), px(MIN_WINDOW_SIZE.1))),
            titlebar: Some(TitlebarOptions {
                title: Some(title.to_string().into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        |window, cx| {
            window.set_window_title(title);
            let marcel = build(window, cx);
            view = Some(marcel.clone());
            cx.new(|cx| Root::new(marcel, window, cx))
        },
    )?;
    Ok(MarcelWindow {
        handle,
        view: view.expect("window builder must initialize Marcel"),
    })
}

/// Open a window showing `path`, and register it.
pub fn open(path: PathBuf, cx: &mut App) -> anyhow::Result<MarcelWindow> {
    let registry = global(cx);
    let cascade = registry.update(cx, |registry, cx| {
        registry.prune(cx);
        if registry.windows.len() >= MAX_LIVE_WINDOWS {
            return None;
        }
        let step = registry.opened % WINDOW_CASCADE_LENGTH;
        registry.opened += 1;
        Some(WINDOW_CASCADE_STEP * step as f32)
    });
    let Some(cascade) = cascade else {
        anyhow::bail!("Marcel already has {MAX_LIVE_WINDOWS} windows open; not opening more");
    };
    let (width, height) = DEFAULT_WINDOW_SIZE;
    let mut bounds = Bounds::centered(None, size(px(width), px(height)), cx);
    bounds.origin += Point::new(px(cascade), px(cascade));

    let opened = open_window(
        "Marcel",
        bounds,
        |window, cx| {
            let marcel = cx.new(|cx| Marcel::new(path, window, cx));
            marcel.update(cx, |view, cx| view.focus_browser(window, cx));
            marcel
        },
        cx,
    )?;
    registry.update(cx, |registry, _| registry.windows.push(opened.clone()));
    Ok(opened)
}

/// Open a window that answers `request`, and register it.
///
/// The answer travels back through the request itself, so this returns
/// nothing. A refused request is answered before this returns, as
/// [`PickerResponse::Closed`]: the caller sees an error, not a hang.
pub fn open_picker(request: PickerRequest, cx: &mut App) -> anyhow::Result<()> {
    let registry = global(cx);
    let allowed = registry.update(cx, |registry, cx| {
        registry.prune(cx);
        registry.pickers.len() < MAX_LIVE_PICKERS
    });
    if !allowed {
        let _ = request.reply.try_send(PickerResponse::Closed);
        anyhow::bail!("Marcel already has {MAX_LIVE_PICKERS} file choosers open; not opening more");
    }
    let title = picker_title(&request);
    let closed = request.closed.clone();
    let (width, height) = PICKER_WINDOW_SIZE;
    let bounds = Bounds::centered(None, size(px(width), px(height)), cx);

    let opened = open_window(
        &title,
        bounds,
        |window, cx| {
            let marcel = cx.new(|cx| Marcel::new_picker(request, window, cx));
            marcel.update(cx, |view, cx| view.focus_picker(window, cx));
            marcel
        },
        cx,
    )?;
    registry.update(cx, |registry, _| registry.pickers.push(opened.clone()));

    // The frontend withdraws a request whose caller went away. The window
    // answers `Closed` and leaves; a request that was already answered has
    // dropped its end of this channel, and then there is nothing to do.
    cx.spawn(async move |cx| {
        if closed.recv().await.is_err() {
            return;
        }
        let _ = opened.handle.update(cx, |_, window, cx| {
            opened
                .view
                .update(cx, |view, cx| view.withdraw_picker(window, cx));
        });
    })
    .detach();
    Ok(())
}

/// The window title: what the caller asked for, or what the dialog is for.
fn picker_title(request: &PickerRequest) -> String {
    let title = request.title.trim();
    if !title.is_empty() {
        return title.to_string();
    }
    match request.mode {
        PickerMode::OpenFiles => "Open File",
        PickerMode::OpenDirectories | PickerMode::SaveFiles { .. } => "Select Folder",
        PickerMode::SaveFile => "Save File",
    }
    .to_string()
}

impl WindowRegistry {
    /// Forget windows the user has closed.
    pub fn prune(&mut self, cx: &App) {
        self.windows.retain(|window| window.handle.read(cx).is_ok());
        self.pickers.retain(|window| window.handle.read(cx).is_ok());
    }

    /// The window a request should speak to when it does not name one.
    ///
    /// The one the user is looking at, then the most recently opened. This is
    /// the same preference [`crate::surface`] applies to reports and questions;
    /// a request that arrives with no window at all has nothing to reuse.
    pub fn current(&self, cx: &App) -> Option<MarcelWindow> {
        let active = cx.active_window();
        // Skipping closed windows here rather than relying on pruning keeps a
        // stale handle from turning a reveal into a new window.
        let mut live = self
            .windows
            .iter()
            .filter(|window| window.handle.read(cx).is_ok());
        let last = live.clone().next_back();
        live.find(|window| active.is_some_and(|active| active == window.handle.into()))
            .or(last)
            .cloned()
    }

    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }
}

pub fn window_icon() -> Option<Arc<image::RgbaImage>> {
    static ICON: OnceLock<Option<Arc<image::RgbaImage>>> = OnceLock::new();
    ICON.get_or_init(|| {
        let icon = image::load_from_memory(include_bytes!(
            "../assets/icons/hicolor/256x256/apps/io.github.berker_z.Marcel.png"
        ))
        .ok()?;
        Some(Arc::new(icon.into_rgba8()))
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A caller's title wins; without one the dialog says what it is for.
    #[test]
    fn a_picker_is_titled_by_its_caller_or_its_purpose() {
        let request = |title: &str, mode: PickerMode| {
            let (reply, _responses) = async_channel::bounded(1);
            let (_close, closed) = async_channel::bounded(1);
            PickerRequest {
                title: title.to_string(),
                mode,
                multiple: false,
                accept_label: None,
                start_directory: None,
                current_name: None,
                filters: Vec::new(),
                current_filter: None,
                reply,
                closed,
            }
        };

        assert_eq!(
            picker_title(&request("Choose a cover", PickerMode::OpenFiles)),
            "Choose a cover"
        );
        assert_eq!(
            picker_title(&request("  ", PickerMode::OpenFiles)),
            "Open File"
        );
        assert_eq!(
            picker_title(&request("", PickerMode::OpenDirectories)),
            "Select Folder"
        );
        assert_eq!(
            picker_title(&request("", PickerMode::SaveFile)),
            "Save File"
        );
        assert_eq!(
            picker_title(&request("", PickerMode::SaveFiles { names: Vec::new() })),
            "Select Folder"
        );
    }

    /// Every window at the same offset looks like one window. The cascade
    /// repeats rather than walking off the screen.
    #[test]
    fn the_cascade_steps_then_returns_to_the_start() {
        let offsets = (0..WINDOW_CASCADE_LENGTH + 2)
            .map(|opened| WINDOW_CASCADE_STEP * (opened % WINDOW_CASCADE_LENGTH) as f32)
            .collect::<Vec<_>>();

        assert_eq!(offsets[0], 0.0);
        assert_eq!(offsets[1], WINDOW_CASCADE_STEP);
        assert_eq!(offsets[WINDOW_CASCADE_LENGTH], 0.0);
        assert_eq!(offsets[WINDOW_CASCADE_LENGTH + 1], WINDOW_CASCADE_STEP);
    }
}
