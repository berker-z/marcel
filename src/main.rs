use gpui::{App, AppContext, Application, Bounds, WindowBounds, WindowOptions, px, size};
use gpui_component::Root;
use marcel::Marcel;

fn main() {
    Application::new().run(|cx: &mut App| {
        gpui_component::init(cx);
        marcel::theme::init(cx);
        marcel::commands::init(cx);

        let bounds = Bounds::centered(None, size(px(1200.0), px(760.0)), cx);

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
                    let start_dir =
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
                    let view = cx.new(|cx| Marcel::new(start_dir, window, cx));
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
