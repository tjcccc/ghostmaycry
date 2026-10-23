//! Ghost May Cry desktop shell (GPUI).
//!
//! A thin client: all state comes from the host (or direct mode) through the
//! same protocol the CLI uses. GUI-facing actions — Ghostty focus/launch,
//! editor, file manager — run here, in the user's graphical session.

mod theme;
mod view;

use gpui::{
    App, Application, Bounds, KeyBinding, WindowBounds, WindowOptions, prelude::*, px, size,
};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let backend = match ghostmc_host::Backend::connect_or_direct() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("ghostmc-desktop: {e}");
            std::process::exit(1);
        }
    };
    Application::new().run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("down", view::NextProject, None),
            KeyBinding::new("up", view::PrevProject, None),
            KeyBinding::new("j", view::NextProject, None),
            KeyBinding::new("k", view::PrevProject, None),
            KeyBinding::new("enter", view::OpenTerminal, None),
            KeyBinding::new("r", view::Refresh, None),
            KeyBinding::new("1", view::ShowSessions, None),
            KeyBinding::new("2", view::ShowChanges, None),
            KeyBinding::new("3", view::ShowFiles, None),
            KeyBinding::new("4", view::ShowAgents, None),
            KeyBinding::new("cmd-q", view::Quit, None),
            KeyBinding::new("ctrl-q", view::Quit, None),
        ]);
        cx.on_action(|_: &view::Quit, cx| cx.quit());
        let bounds = Bounds::centered(None, size(px(1180.), px(760.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Ghost May Cry".into()),
                    ..Default::default()
                }),
                app_id: Some("ghostmaycry".into()),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| view::Root::new(backend, window, cx)),
        )
        .expect("open window");
        cx.activate(true);
    });
}
