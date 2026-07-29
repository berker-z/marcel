use gpui::{App, AppContext, Application, Bounds, WindowBounds, WindowOptions, px, size};
use gpui_component::Root;
use marcel::Marcel;

fn main() {
    let current_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    let start_path = marcel::launch::start_path(std::env::args_os().skip(1), current_dir);

    Application::new().run(move |cx: &mut App| {
        gpui_component::init(cx);
        marcel::theme::init(cx);
        marcel::commands::init(cx);

        let bounds = Bounds::centered(None, size(px(1200.0), px(760.0)), cx);
        let start_path = start_path.clone();

        cx.spawn(async move |cx| {
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("Marcel".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(|cx| Marcel::new(start_path, window, cx));
                    view.update(cx, |view, _| view.focus_browser(window));
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )?;

            Ok::<_, anyhow::Error>(())
        })
        .detach();

        cx.activate(true);
    });
}
