use std::{cell::Cell, rc::Rc};

use gpui::App;
use marcel::{
    desktop::{
        bus::{self, DesktopRequest, InstanceStartup, RevealedLocation},
        launch::{self, Invocation},
    },
    surface, window,
};

/// How long a quitting Marcel waits for file-chooser answers to reach the bus.
/// Under GPUI's 200 ms shutdown budget, and well past what one reply needs.
const REPLY_DRAIN_LIMIT: std::time::Duration = std::time::Duration::from_millis(150);

/// How long a bus-activated start waits for its request before opening a
/// window anyway. The request is normally on the bus before the process is;
/// a few seconds is far past that and still short of "Marcel opened nothing".
const BUS_REQUEST_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// The exit status for a command line Marcel could not read, as `getopt`
/// users expect.
const USAGE_EXIT_CODE: i32 = 2;

fn main() {
    let current_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    let (start_path, explicit_launch) =
        match launch::parse_arguments(std::env::args_os().skip(1), current_dir) {
            Ok(Invocation::Open { start_path, explicit }) => (start_path, explicit),
            Ok(Invocation::Help) => {
                print!("{}", launch::USAGE);
                return;
            }
            Ok(Invocation::Version) => {
                println!("marcel-rs {}", launch::VERSION);
                return;
            }
            Err(message) => {
                eprintln!("{message}");
                std::process::exit(USAGE_EXIT_CODE);
            }
        };

    // A bus-activated start has no folder of its own: its working directory
    // is the daemon's. Should another Marcel already be primary (it was
    // started for a portal or FileManager1 name that one does not hold), the
    // forwarded request is "show me Marcel", not "open the daemon's cwd".
    let bus_activated = !explicit_launch && launch::started_by_bus_activation();
    let initial_uris = if bus_activated { None } else { launch::launch_uris(&start_path) };
    // ...and when such a start does have to show a folder — an `Activate`
    // with no window to raise, or no request at all — it shows home.
    let start_path =
        if bus_activated { launch::service_start_path(start_path) } else { start_path };
    let desktop_runtime = match smol::block_on(bus::acquire_or_forward(initial_uris)) {
        InstanceStartup::Primary(runtime) => Some(runtime),
        InstanceStartup::Forwarded => return,
        InstanceStartup::Unavailable(error) => {
            eprintln!("Marcel desktop integration unavailable: {error}");
            None
        }
    };

    // A `DBusActivatable` cold start runs `marcel` with no arguments at the
    // bus daemon's working directory, then delivers the real request — Open,
    // Activate, ShowItems — over the bus. Opening a window first would put a
    // stray window at that directory in front of every such launch, so wait
    // for the request instead. Anything else — a terminal, a launcher running
    // the desktop entry's Exec line directly — still gets its window here.
    let wait_for_bus_request = bus_activated && desktop_runtime.is_some();

    gpui_platform::application().run(move |cx: &mut App| {
        gpui_component::init(cx);
        marcel::fonts::init(cx);
        marcel::theme::init(marcel::config::chosen_theme(), cx);
        marcel::init_key_bindings(cx);
        marcel::operations::init(cx);
        window::init(cx);

        if !wait_for_bus_request {
            window::open(start_path.clone(), cx).expect("failed to open Marcel's initial window");
        }

        if let Some(runtime) = desktop_runtime {
            let requests = runtime.requests();
            let pickers = runtime.pickers();
            let replies = runtime.replies();
            let fallback_path = start_path.clone();
            // Whether any request has arrived, for the grace timer below.
            let request_seen = Rc::new(Cell::new(false));
            let seen = request_seen.clone();
            cx.spawn(async move |cx| {
                while let Ok(request) = requests.recv().await {
                    seen.set(true);
                    cx.update(|cx| handle_desktop_request(request, &fallback_path, cx));
                }
                Ok::<_, anyhow::Error>(())
            })
            .detach();
            // A file chooser is a window of its own kind: it neither reuses
            // nor counts as a browsing window, and its answer goes back over
            // the bus rather than to the user.
            let seen = request_seen.clone();
            cx.spawn(async move |cx| {
                while let Ok(request) = pickers.recv().await {
                    seen.set(true);
                    cx.update(|cx| {
                        if let Err(error) = window::open_picker(request, cx) {
                            eprintln!("Marcel could not open a file chooser: {error}");
                        }
                        cx.activate(true);
                    });
                }
                Ok::<_, anyhow::Error>(())
            })
            .detach();
            // The bus-started detection is a heuristic, and a request that
            // was routed elsewhere or lost leaves a Marcel with no window and
            // no way to get one. Past the grace period, "nothing arrived"
            // means "show the folder we have" rather than "keep waiting".
            if wait_for_bus_request {
                let fallback_path = start_path.clone();
                cx.spawn(async move |cx| {
                    cx.background_executor().timer(BUS_REQUEST_GRACE).await;
                    if request_seen.get() {
                        return;
                    }
                    cx.update(|cx| {
                        eprintln!(
                            "Marcel was started as a bus service but no request arrived within {} s; opening a window",
                            BUS_REQUEST_GRACE.as_secs()
                        );
                        open_window_or_report(fallback_path, cx);
                    })
                })
                .detach();
            }
            // The runtime owns the bus connection. It rebuilds the connection
            // if the bus drops it, and only speaks up once that has failed
            // for good — silently losing the bus used to mean "show in
            // folder" did nothing for the rest of the session.
            cx.spawn(async move |cx| {
                let reason = runtime.serve_until_lost().await;
                eprintln!("Marcel is off the session bus for good: {reason}");
                cx.update(|cx| {
                    if let Some(handle) = surface::current(None, cx) {
                        let _ = handle.update(cx, |_, window, cx| {
                            surface::Report::Error(format!(
                                "Lost the session bus: {reason}. Marcel keeps working, but \"show in folder\" and file dialogs will not reach it until it is restarted"
                            ))
                            .show(window, cx);
                        });
                    }
                })
            })
            .detach();
            // A picker was the last window, the user closed it, and the
            // answer is still being written to the bus from another thread.
            // Leaving now would make the asking application see its dialog
            // backend disappear. GPUI gives a quit hook 200 ms; a reply
            // takes far less than that to leave.
            cx.on_app_quit(move |cx| {
                let replies = replies.clone();
                let executor = cx.background_executor().clone();
                async move {
                    let deadline = std::time::Instant::now() + REPLY_DRAIN_LIMIT;
                    while replies.pending() > 0 && std::time::Instant::now() < deadline {
                        executor.timer(std::time::Duration::from_millis(5)).await;
                    }
                }
            })
            .detach();
        }

        cx.activate(true);
    });
}

fn handle_desktop_request(request: DesktopRequest, fallback_path: &std::path::Path, cx: &mut App) {
    let registry = window::global(cx);
    registry.update(cx, |registry, cx| registry.prune(cx));
    // A launch gets a window of its own; only a reveal may take over the one
    // the user is reading.
    let may_reuse = request.may_reuse_a_window();
    match request {
        // Clicking an application's icon means "show me the Marcel I have",
        // not "give me another one". A launch is the other signal, and it
        // arrives as `Open`.
        DesktopRequest::Activate => {
            cx.activate(true);
            match registry.read(cx).current(cx) {
                Some(current) => {
                    let _ = current.handle.update(cx, |_, window, _| window.activate_window());
                }
                // A cold bus activation deferred its initial window; with
                // nothing to raise, "show me the Marcel I have" means opening
                // one.
                None => {
                    open_window_or_report(fallback_path.to_path_buf(), cx);
                }
            }
        }
        DesktopRequest::Open(locations) | DesktopRequest::ShowItems(locations) => {
            show_locations(locations, may_reuse, cx);
        }
        DesktopRequest::ShowFolders(folders) => show_locations(
            folders
                .into_iter()
                .map(|directory| RevealedLocation { directory, items: Vec::new() })
                .collect(),
            may_reuse,
            cx,
        ),
        DesktopRequest::ShowItemProperties(paths) => show_properties(paths, may_reuse, cx),
    }
}

/// Open the Properties dialog for `paths` on the window the user is looking
/// at, or on a new one at the first item's folder when there is none.
fn show_properties(paths: Vec<std::path::PathBuf>, may_reuse: bool, cx: &mut App) {
    let registry = window::global(cx);
    let target = registry.read(cx).current(cx).filter(|_| may_reuse).or_else(|| {
        let directory = paths
            .first()
            .and_then(|path| path.parent())
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("/"));
        open_window_or_report(directory, cx)
    });
    let Some(target) = target else {
        return;
    };
    // Through the untyped handle: the typed one leases the root view for the
    // closure, and opening a dialog needs to update that same root.
    let handle: gpui::AnyWindowHandle = target.handle.into();
    let _ = handle.update(cx, |_, window, cx| {
        target.view.update(cx, |view, cx| view.open_properties(paths, window, cx));
        window.activate_window();
    });
    cx.activate(true);
}

/// Open a window at `path`, and say so somewhere when that is refused.
///
/// The one refusal is the window cap, and a request that hits it used to
/// vanish: no window, no message, nothing in the log. The user asked for
/// something, so the answer goes to whichever window is speaking for Marcel,
/// and to stderr for the case where none is.
fn open_window_or_report(path: std::path::PathBuf, cx: &mut App) -> Option<window::MarcelWindow> {
    match window::open(path, cx) {
        Ok(opened) => Some(opened),
        Err(error) => {
            eprintln!("Marcel could not open a window: {error}");
            if let Some(handle) = surface::current(None, cx) {
                let _ = handle.update(cx, |_, window, cx| {
                    surface::Report::Error(error.to_string()).show(window, cx);
                });
            }
            None
        }
    }
}

/// Show each location, in a window each, reusing one for the first if allowed.
fn show_locations(locations: Vec<RevealedLocation>, may_reuse: bool, cx: &mut App) {
    let registry = window::global(cx);
    let mut refused = false;
    for (index, location) in locations.into_iter().enumerate() {
        let reused = may_reuse
            && index == 0
            && registry.read(cx).current(cx).is_some_and(|current| {
                current
                    .handle
                    .update(cx, |_, window, cx| {
                        current.view.update(cx, |view, cx| {
                            view.open_external_location(
                                location.directory.clone(),
                                location.items.clone(),
                                window,
                                cx,
                            );
                        });
                        window.activate_window();
                    })
                    .is_ok()
            });
        if !reused {
            refused |= !open_new_window(location, cx);
        }
    }
    if refused && let Some(handle) = surface::current(None, cx) {
        let _ = handle.update(cx, |_, window, cx| {
            surface::Report::Error(format!(
                "Refusing to open more than {} windows",
                window::MAX_LIVE_WINDOWS
            ))
            .show(window, cx);
        });
    }
    cx.activate(true);
}

/// Open one window for `location`, saying whether one could be opened.
fn open_new_window(location: RevealedLocation, cx: &mut App) -> bool {
    let Ok(opened) = window::open(location.directory.clone(), cx) else {
        return false;
    };
    if !location.items.is_empty() {
        let _ = opened.handle.update(cx, |_, window, cx| {
            opened.view.update(cx, |view, cx| {
                view.open_external_location(location.directory, location.items, window, cx);
            });
        });
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_window_icon_decodes_at_the_declared_size() {
        let icon = window::window_icon().expect("bundled Marcel icon must decode");
        assert_eq!((icon.width(), icon.height()), (256, 256));
    }
}
