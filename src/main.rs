use std::path::PathBuf;

use gpui::{
    App, AppContext, Application, Bounds, Entity, WindowBounds, WindowHandle, WindowOptions, px,
    size,
};
use gpui_component::Root;
use marcel::Marcel;

fn main() {
    let current_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    let has_location_argument = !arguments.is_empty();
    let start_path = marcel::launch::start_path(arguments, current_dir);

    let desktop_runtime = {
        let initial_uris = has_location_argument.then(|| {
            url::Url::from_file_path(&start_path)
                .map(|uri| vec![uri.into()])
                .unwrap_or_default()
        });
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

    Application::new().run(move |cx: &mut App| {
        gpui_component::init(cx);
        marcel::identity::init(cx);
        marcel::theme::init(cx);
        marcel::commands::init(cx);

        let initial_window = open_marcel_window(start_path.clone(), cx)
            .expect("failed to open Marcel's initial window");

        if let Some(runtime) = desktop_runtime {
            let requests = runtime.requests();
            cx.spawn(async move |cx| {
                let _runtime = runtime;
                let mut windows = vec![initial_window];
                while let Ok(request) = requests.recv().await {
                    cx.update(|cx| handle_desktop_request(request, &mut windows, cx))?;
                }
                Ok::<_, anyhow::Error>(())
            })
            .detach();
        }

        cx.activate(true);
    });
}

#[derive(Clone)]
struct MarcelWindow {
    handle: WindowHandle<Root>,
    view: Entity<Marcel>,
}

fn open_marcel_window(path: PathBuf, cx: &mut App) -> anyhow::Result<MarcelWindow> {
    let bounds = Bounds::centered(None, size(px(1200.0), px(760.0)), cx);
    let mut view = None;
    let handle = cx.open_window(
        WindowOptions {
            app_id: Some(marcel::desktop_integration::APPLICATION_ID.to_string()),
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("Marcel".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        |window, cx| {
            window.set_window_title("Marcel");
            let marcel = cx.new(|cx| Marcel::new(path, window, cx));
            marcel.update(cx, |view, _| view.focus_browser(window));
            view = Some(marcel.clone());
            cx.new(|cx| Root::new(marcel, window, cx))
        },
    )?;

    Ok(MarcelWindow {
        handle,
        view: view.expect("window builder must initialize Marcel"),
    })
}

fn handle_desktop_request(
    request: marcel::desktop_integration::DesktopRequest,
    windows: &mut Vec<MarcelWindow>,
    cx: &mut App,
) {
    use marcel::desktop_integration::{DesktopRequest, RevealedLocation};

    match request {
        DesktopRequest::Activate => {
            cx.activate(true);
            if let Some(active) = windows.last() {
                let _ = active
                    .handle
                    .update(cx, |_, window, _| window.activate_window());
            }
        }
        DesktopRequest::Open(locations) | DesktopRequest::ShowItems(locations) => {
            open_desktop_locations(locations, windows, cx);
        }
        DesktopRequest::ShowFolders(folders) => {
            open_desktop_locations(
                folders
                    .into_iter()
                    .map(|directory| RevealedLocation {
                        directory,
                        items: Vec::new(),
                    })
                    .collect(),
                windows,
                cx,
            );
        }
        DesktopRequest::ShowItemProperties(_) => {}
    }
}

fn open_desktop_locations(
    locations: Vec<marcel::desktop_integration::RevealedLocation>,
    windows: &mut Vec<MarcelWindow>,
    cx: &mut App,
) {
    for (index, location) in locations.into_iter().enumerate() {
        let reveal = location.items.into_iter().next();
        let reused_existing = if index == 0 {
            windows.last().is_some_and(|active| {
                let directory = location.directory.clone();
                let reveal = reveal.clone();
                active
                    .handle
                    .update(cx, |_, window, cx| {
                        active.view.update(cx, |view, cx| {
                            view.open_external_location(directory, reveal, window, cx);
                        });
                        window.activate_window();
                    })
                    .is_ok()
            })
        } else {
            false
        };
        if reused_existing {
            continue;
        }

        if let Ok(window) = open_marcel_window(location.directory, cx) {
            if let Some(reveal) = reveal {
                let _ = window.handle.update(cx, |_, gpui_window, cx| {
                    window.view.update(cx, |view, cx| {
                        view.open_external_location(
                            view_directory_for_reveal(&reveal),
                            Some(reveal),
                            gpui_window,
                            cx,
                        );
                    });
                });
            }
            windows.push(window);
        }
    }
    cx.activate(true);
}

fn view_directory_for_reveal(path: &std::path::Path) -> PathBuf {
    path.parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/"))
}
