//! Ghost May Cry desktop app (GPUI).
//!
//! On macOS it lives in the menu bar: the ghost icon drops a panel down
//! under it, and the panel closes when it loses focus. Elsewhere it opens as
//! a normal window. A thin client: all state comes from the host (or direct
//! mode) through the same protocol the CLI uses. GUI-facing actions (Ghostty
//! focus/launch, Finder) run here, in the user's graphical session.

#[cfg(target_os = "macos")]
mod context_menu;
#[cfg(target_os = "macos")]
mod glass;
mod input;
#[cfg(target_os = "macos")]
mod statusbar;
mod theme;
mod view;

use gpui::{
    App, Application, Bounds, Entity, KeyBinding, WindowBounds, WindowHandle, WindowOptions,
    prelude::*, px, size,
};
use std::cell::RefCell;
use std::rc::Rc;
use view::Root;

/// The open panel, if any.
type Panel = Rc<RefCell<Option<WindowHandle<Root>>>>;

fn main() {
    // SAFETY: no other thread exists yet.
    unsafe { ghostmc_core::agent::clear_session_env() };
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
            KeyBinding::new("down", view::SelectNext, Some("ghostmc")),
            KeyBinding::new("up", view::SelectPrev, Some("ghostmc")),
            KeyBinding::new("enter", view::OpenTerminal, Some("ghostmc")),
            KeyBinding::new("cmd-r", view::Refresh, Some("ghostmc")),
            KeyBinding::new("escape", view::Dismiss, Some("ghostmc")),
            KeyBinding::new("cmd-q", view::Quit, None),
            KeyBinding::new("ctrl-q", view::Quit, None),
        ]);
        cx.bind_keys(input::bindings());
        cx.on_action(|_: &view::Quit, cx| cx.quit());
        let root = cx.new(|cx| Root::new(backend, cx));
        #[cfg(target_os = "macos")]
        root.update(cx, |r, _| {
            r.set_pop_up(|window| {
                let w = statusbar::ns_window(window)?;
                let show: view::PopUp =
                    Box::new(move |items: &[&str], at| context_menu::pop_up(&w, items, at));
                Some(show)
            })
        });
        let panel: Panel = Rc::new(RefCell::new(None));
        #[cfg(target_os = "macos")]
        if menu_bar::install(&root, &panel, cx) {
            return;
        }
        let options = WindowOptions {
            app_id: Some("ghostmaycry".into()),
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(view::LIST_WIDTH), px(view::HEIGHT)),
                cx,
            ))),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("Ghost May Cry".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        open(&root, &panel, options, false, cx);
    });
}

/// Open `root` in a new window. With `close_on_blur` (the menu-bar panel) it
/// closes again when it loses focus.
fn open(
    root: &Entity<Root>,
    panel: &Panel,
    options: WindowOptions,
    close_on_blur: bool,
    cx: &mut App,
) {
    let r = root.clone();
    match cx.open_window(options, move |window, cx| {
        r.update(cx, |r, cx| {
            r.attach(window, close_on_blur, cx);
            // Liquid Glass behind the menu-bar panel's cards (macOS 26+).
            #[cfg(target_os = "macos")]
            if close_on_blur
                && let Some(mut g) = statusbar::ns_window(window)
                    .and_then(|w| glass::Glass::new(w, view::CARD_RADIUS))
            {
                r.set_backdrop(move |cards| g.place(cards));
            }
        });
        r
    }) {
        Ok(h) => *panel.borrow_mut() = Some(h),
        Err(e) => {
            log::error!("opening the panel: {e}");
            return;
        }
    }
    #[cfg(target_os = "macos")]
    if close_on_blur {
        statusbar::style_panel_windows();
    }
    cx.activate(true);
}

#[cfg(target_os = "macos")]
mod menu_bar {
    use super::{Panel, Root, open, statusbar, view};
    use futures::StreamExt;
    use gpui::{
        App, Bounds, Entity, WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions,
        point, px, size,
    };
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Duration;

    /// Install the menu-bar icon; `false` if that failed and a normal
    /// window should open instead.
    pub fn install(root: &Entity<Root>, panel: &Panel, cx: &mut App) -> bool {
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<statusbar::Anchor>();
        if let Err(e) = statusbar::install(move |a| {
            let _ = tx.unbounded_send(a);
        }) {
            log::warn!("menu bar: {e}");
            return false;
        }
        statusbar::become_accessory();
        // The icon the open panel hangs from; resizing keeps it under it.
        let current: Rc<Cell<Option<statusbar::Anchor>>> = Rc::new(Cell::new(None));
        let at = current.clone();
        root.update(cx, |r, _| {
            r.set_attention_handler(statusbar::set_attention);
            r.set_resize_handler(move |window, width, cx| {
                let (Some(a), Some(ns)) = (at.get(), statusbar::ns_window(window)) else {
                    return;
                };
                // Setting the frame calls back into GPUI, so it must run
                // after the current update, as GPUI's own `resize` does.
                cx.foreground_executor()
                    .spawn(async move {
                        statusbar::set_frame(&ns, x_for(a, width), top(a), width, view::HEIGHT);
                    })
                    .detach();
            });
        });
        let (root, panel) = (root.clone(), panel.clone());
        cx.spawn(async move |cx| {
            while let Some(anchor) = rx.next().await {
                current.set(Some(anchor));
                if cx.update(|cx| toggle(&root, &panel, anchor, cx)).is_err() {
                    return;
                }
            }
        })
        .detach();
        true
    }

    fn toggle(root: &Entity<Root>, panel: &Panel, anchor: statusbar::Anchor, cx: &mut App) {
        let open_now = *panel.borrow();
        if let Some(h) = open_now.filter(|_| root.read(cx).is_visible()) {
            let _ = h.update(cx, |r, window, cx| r.close(window, cx));
            return;
        }
        // A click on the icon first takes focus from the panel, which
        // closes it; that same click must not reopen it.
        let just_closed = root
            .read(cx)
            .closed_at
            .is_some_and(|t| t.elapsed() < Duration::from_millis(300));
        if !just_closed {
            open(root, panel, options(anchor, cx), true, cx);
        }
    }

    /// Left edge of a window `width` wide whose first card starts under the
    /// icon, kept on screen (the margin keeps the cards off the edge).
    fn x_for(a: statusbar::Anchor, width: f32) -> f32 {
        (a.left - view::MARGIN).min(a.screen_width - width).max(0.)
    }

    fn top(a: statusbar::Anchor) -> f32 {
        a.bottom
    }

    /// A borderless panel just under the icon, opening with only the list.
    fn options(a: statusbar::Anchor, cx: &App) -> WindowOptions {
        // GPUI may list no displays; without a match it uses the primary
        // one, which is where the menu bar usually is.
        let display = cx
            .displays()
            .into_iter()
            .find(|d| u32::from(d.id()) == a.display);
        WindowOptions {
            app_id: Some("ghostmaycry".into()),
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                point(px(x_for(a, view::LIST_WIDTH)), px(top(a))),
                size(px(view::LIST_WIDTH), px(view::HEIGHT)),
            ))),
            display_id: display.map(|d| d.id()),
            titlebar: None,
            kind: WindowKind::PopUp,
            // Only the cards are drawn; the rest of the window is see-through.
            window_background: WindowBackgroundAppearance::Transparent,
            is_movable: false,
            is_resizable: false,
            is_minimizable: false,
            ..Default::default()
        }
    }
}
