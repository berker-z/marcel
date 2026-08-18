//! Marcel's windows, and who owns the list of them.
//!
//! [`Sprint 19`](../docs/sprints/019-application-global-operations.md) moved
//! operations and user data off the window and onto the application. This is the
//! same move for the windows themselves: opening one was a private detail of
//! `main.rs`, which meant the only way to get a second one was a D-Bus request,
//! and a list `main.rs` maintained by hand would go stale the moment a window
//! could come from anywhere else.
//!
//! The registry lives on the application, so a window opened from a context menu
//! and a window opened by a desktop request are the same kind of thing.

use std::{path::PathBuf, sync::Arc, sync::OnceLock};

use gpui::{
    App, AppContext as _, Bounds, Entity, Global, Point, TitlebarOptions, WindowBounds,
    WindowHandle, WindowOptions, px, size,
};
use gpui_component::Root;

use crate::Marcel;

/// The size a Marcel window opens at when nothing else decides for it.
const DEFAULT_WINDOW_SIZE: (f32, f32) = (1200.0, 760.0);

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

#[derive(Clone)]
pub struct MarcelWindow {
    pub handle: WindowHandle<Root>,
    pub view: Entity<Marcel>,
}

/// Every Marcel window this process has open, in the order they were opened.
#[derive(Default)]
pub struct WindowRegistry {
    windows: Vec<MarcelWindow>,
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

/// Open a window showing `path`, and register it.
pub fn open(path: PathBuf, cx: &mut App) -> anyhow::Result<MarcelWindow> {
    let registry = global(cx);
    let cascade = registry.update(cx, |registry, cx| {
        registry.prune(cx);
        let step = registry.opened % WINDOW_CASCADE_LENGTH;
        registry.opened += 1;
        WINDOW_CASCADE_STEP * step as f32
    });

    let (width, height) = DEFAULT_WINDOW_SIZE;
    let mut bounds = Bounds::centered(None, size(px(width), px(height)), cx);
    bounds.origin += Point::new(px(cascade), px(cascade));

    let mut view = None;
    let handle = cx.open_window(
        WindowOptions {
            app_id: Some(crate::desktop_integration::APPLICATION_ID.to_string()),
            icon: window_icon(),
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Marcel".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        |window, cx| {
            window.set_window_title("Marcel");
            let marcel = cx.new(|cx| Marcel::new(path, window, cx));
            marcel.update(cx, |view, cx| view.focus_browser(window, cx));
            view = Some(marcel.clone());
            cx.new(|cx| Root::new(marcel, window, cx))
        },
    )?;

    let opened = MarcelWindow {
        handle,
        view: view.expect("window builder must initialize Marcel"),
    };
    registry.update(cx, |registry, _| registry.windows.push(opened.clone()));
    Ok(opened)
}

/// Whether a desktop request may take over a window the user is already using.
///
/// A launch may not. Somebody ran `marcel`, or picked Marcel to open a folder;
/// they asked for Marcel to show them something, and answering by navigating the
/// window they were reading loses their place and gives them nothing extra.
///
/// A reveal may. "Show me where this file is" is a request about a view that
/// already exists — it is the one case where reusing the window in front of the
/// user is the answer rather than a shortcut.
pub fn may_reuse_a_window(request: &crate::desktop_integration::DesktopRequest) -> bool {
    use crate::desktop_integration::DesktopRequest;

    match request {
        DesktopRequest::ShowItems(_) | DesktopRequest::ShowFolders(_) => true,
        DesktopRequest::Open(_)
        | DesktopRequest::Activate
        | DesktopRequest::ShowItemProperties(_) => false,
    }
}

impl WindowRegistry {
    /// Forget windows the user has closed.
    pub fn prune(&mut self, cx: &App) {
        self.windows.retain(|window| window.handle.read(cx).is_ok());
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
    static ICON: OnceLock<Arc<image::RgbaImage>> = OnceLock::new();
    if let Some(icon) = ICON.get() {
        return Some(icon.clone());
    }
    let icon = Arc::new(
        image::load_from_memory(include_bytes!(
            "../assets/icons/hicolor/256x256/apps/io.github.berker_z.Marcel.png"
        ))
        .ok()?
        .into_rgba8(),
    );
    let _ = ICON.set(icon.clone());
    Some(icon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop_integration::{DesktopRequest, RevealedLocation};

    /// The defect this rule exists to prevent: running `marcel` while Marcel is
    /// already open navigated the window the user was reading, or — with no
    /// argument at all — raised it and ignored the folder they were standing in.
    #[test]
    fn a_launch_never_takes_over_a_window_and_a_reveal_may() {
        let location = || RevealedLocation {
            directory: PathBuf::from("/folder"),
            items: Vec::new(),
        };

        assert!(!may_reuse_a_window(&DesktopRequest::Open(vec![location()])));
        assert!(!may_reuse_a_window(&DesktopRequest::Activate));
        assert!(may_reuse_a_window(&DesktopRequest::ShowItems(vec![
            location()
        ])));
        assert!(may_reuse_a_window(&DesktopRequest::ShowFolders(vec![
            PathBuf::from("/folder")
        ])));
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
