use gpui::App;
use marcel::window;

fn main() {
    let current_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    let start_path = marcel::launch::start_path(arguments, current_dir);

    let desktop_runtime = {
        let initial_uris = marcel::launch::launch_uris(&start_path);
        match smol::block_on(marcel::desktop_integration::acquire_or_forward(
            initial_uris,
        )) {
            marcel::desktop_integration::InstanceStartup::Primary(runtime) => Some(runtime),
            marcel::desktop_integration::InstanceStartup::Forwarded => return,
            marcel::desktop_integration::InstanceStartup::Unavailable(error) => {
                eprintln!("Marcel desktop integration unavailable: {error}");
                None
            }
        }
    };

    gpui_platform::application().run(move |cx: &mut App| {
        gpui_component::init(cx);
        marcel::identity::init(cx);
        marcel::theme::init(cx);
        marcel::commands::init(cx);
        marcel::operations::init(cx);
        marcel::window::init(cx);

        window::open(start_path.clone(), cx).expect("failed to open Marcel's initial window");

        if let Some(runtime) = desktop_runtime {
            let requests = runtime.requests();
            cx.spawn(async move |cx| {
                let _runtime = runtime;
                while let Ok(request) = requests.recv().await {
                    cx.update(|cx| handle_desktop_request(request, cx));
                }
                Ok::<_, anyhow::Error>(())
            })
            .detach();
        }

        cx.activate(true);
    });
}

fn handle_desktop_request(request: marcel::desktop_integration::DesktopRequest, cx: &mut App) {
    use marcel::desktop_integration::{DesktopRequest, RevealedLocation};

    let registry = window::global(cx);
    registry.update(cx, |registry, cx| registry.prune(cx));
    // A launch gets a window of its own; only a reveal may take over the one
    // the user is reading.
    let may_reuse = window::may_reuse_a_window(&request);
    match request {
        // Clicking an application's icon means "show me the Marcel I have",
        // not "give me another one". A launch is the other signal, and it
        // arrives as `Open`.
        DesktopRequest::Activate => {
            cx.activate(true);
            let current = registry.read(cx).current(cx);
            if let Some(current) = current {
                let _ = current
                    .handle
                    .update(cx, |_, window, _| window.activate_window());
            }
        }
        DesktopRequest::Open(locations) | DesktopRequest::ShowItems(locations) => {
            show_locations(locations, may_reuse, cx);
        }
        DesktopRequest::ShowFolders(folders) => show_locations(
            folders
                .into_iter()
                .map(|directory| RevealedLocation {
                    directory,
                    items: Vec::new(),
                })
                .collect(),
            may_reuse,
            cx,
        ),
        DesktopRequest::ShowItemProperties(_) => {}
    }
}

/// Show each location, in a window each, reusing one for the first if allowed.
fn show_locations(
    locations: Vec<marcel::desktop_integration::RevealedLocation>,
    may_reuse: bool,
    cx: &mut App,
) {
    let registry = window::global(cx);
    for (index, location) in locations.into_iter().enumerate() {
        let reused = may_reuse
            && index == 0
            && registry.read(cx).current(cx).is_some_and(|current| {
                let directory = location.directory.clone();
                let items = location.items.clone();
                current
                    .handle
                    .update(cx, |_, window, cx| {
                        current.view.update(cx, |view, cx| {
                            view.open_external_location(directory, items, window, cx);
                        });
                        window.activate_window();
                    })
                    .is_ok()
            });
        if !reused {
            open_new_window(location, cx);
        }
    }
    cx.activate(true);
}

fn open_new_window(location: marcel::desktop_integration::RevealedLocation, cx: &mut App) {
    let Ok(opened) = window::open(location.directory.clone(), cx) else {
        return;
    };
    if location.items.is_empty() {
        return;
    }
    let _ = opened.handle.update(cx, |_, window, cx| {
        opened.view.update(cx, |view, cx| {
            view.open_external_location(location.directory, location.items, window, cx);
        });
    });
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
