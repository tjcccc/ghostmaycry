//! The panel: workspace list on the left; clicking a workspace (or
//! Settings) opens its agents (or Settings) on the right, and clicking it
//! again closes that pane. The grouping itself is
//! `ghostmc_host::workspace`; this file only draws it and runs actions.

use crate::input::{Cancelled, Submitted, TextInput};
use crate::theme::{self, Palette};
use ghostmc_core::agent::{AgentKind, AgentState};
use ghostmc_core::config::Config;
use ghostmc_core::model::GitIdentity;
use ghostmc_host::Backend;
use ghostmc_host::workspace::{self, Row, RowAgent, Workspace, WorkspaceKey};
use ghostmc_platform::ghostty::{self, Placement};
use ghostmc_protocol::*;
use ghostmc_tmux::AttachMode;
use gpui::{
    AnyElement, Bounds, ClickEvent, Context, Div, Entity, FocusHandle, Focusable, FontWeight,
    MouseButton, MouseDownEvent, PathPromptOptions, Pixels, Point, SharedString, Stateful,
    Subscription, Window, actions, anchored, canvas, deferred, div, prelude::*, px, size,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

actions!(
    ghostmc,
    [SelectNext, SelectPrev, OpenTerminal, Refresh, Dismiss, Quit]
);

/// The list and the right pane are separate cards, each as tall as its
/// content up to `MAX_CARD_HEIGHT`, on a transparent window of fixed height.
const LIST_CARD: f32 = 330.;
const PANE_CARD: f32 = 430.;
const MAX_CARD_HEIGHT: f32 = 520.;
const GAP: f32 = 8.;
/// Corner radius of the cards, and of the glass behind them.
pub const CARD_RADIUS: f32 = 16.;
/// Room around the cards for their shadows (less at the top, under the
/// menu bar).
pub const MARGIN: f32 = 12.;
const MARGIN_TOP: f32 = 4.;
/// Window size in points: `WIDTH` with the right pane open, `LIST_WIDTH`
/// with only the list.
pub const WIDTH: f32 = 2. * MARGIN + LIST_CARD + GAP + PANE_CARD;
pub const LIST_WIDTH: f32 = 2. * MARGIN + LIST_CARD;
pub const HEIGHT: f32 = MARGIN_TOP + MAX_CARD_HEIGHT + MARGIN;

/// Resizes the window to a new width, for hosts (the menu-bar panel) that
/// must also move it.
type ResizeHandler = Box<dyn Fn(&Window, f32, &gpui::App)>;

/// Draws a native backdrop (Liquid Glass) behind the cards, given their
/// bounds after each layout. While one is set the cards paint no fill.
type Backdrop = Box<dyn FnMut(&[Bounds<Pixels>])>;

/// Shows a native menu of labels at a window position and returns the
/// chosen index. It runs the menu's own event loop, so it is called from a
/// spawned task.
pub type PopUp = Box<dyn FnOnce(&[&str], Point<Pixels>) -> Option<usize>>;
/// Makes a [`PopUp`] for a window, where native menus exist.
type PopUpFor = Box<dyn Fn(&Window) -> Option<PopUp>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Workspace,
    Settings,
}

struct Menu {
    key: WorkspaceKey,
    at: Point<Pixels>,
    confirm_remove: bool,
}

/// Data gathered off the UI thread.
#[derive(Default)]
struct Snapshot {
    status: Option<StatusInfo>,
    workspaces: Vec<Workspace>,
    terminals: Vec<ghostty::GhosttyTerminal>,
    ghostty_error: Option<String>,
    error: Option<String>,
}

pub struct Root {
    backend: Arc<Backend>,
    cfg: Config,
    focus: FocusHandle,
    status: Option<StatusInfo>,
    workspaces: Vec<Workspace>,
    /// Last Ghostty listing, reused while the panel is closed.
    terminals: Vec<ghostty::GhosttyTerminal>,
    loaded: bool,
    selected: Option<WorkspaceKey>,
    /// The right pane; `None` while only the list shows.
    page: Option<Page>,
    /// Width the window was last given.
    width: f32,
    on_resize: Option<ResizeHandler>,
    /// The menu-bar panel: transparent around the cards, and a click there
    /// closes it.
    floating: bool,
    backdrop: Rc<RefCell<Option<Backdrop>>>,
    /// Bounds of the list and pane cards from the last layout.
    cards: Rc<RefCell<[Option<Bounds<Pixels>>; 2]>>,
    /// Live windows removed from the list, until Ghostty forgets them.
    dismissed: HashSet<String>,
    /// Agent states the user has looked at: run id → `state_since`.
    seen: HashMap<String, i64>,
    menu: Option<Menu>,
    pop_up: Option<PopUpFor>,
    /// Workspace asking "Remove from the list?" inline (native-menu path).
    confirm_remove: Option<WorkspaceKey>,
    renaming: Option<(WorkspaceKey, Entity<TextInput>)>,
    message: Option<(String, bool)>,
    ghostty_error: Option<String>,
    refreshing: bool,
    refresh_again: bool,
    /// The panel is on screen.
    visible: bool,
    /// When the panel last closed; lets the menu-bar click that took focus
    /// away from it not reopen it at once.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub closed_at: Option<Instant>,
    /// A system dialog (folder picker) is up: losing focus is expected.
    modal: bool,
    on_attention: Option<Box<dyn Fn(bool)>>,
    window_subscription: Option<Subscription>,
    rename_subscriptions: Vec<Subscription>,
}

fn err_text(e: ApiError) -> String {
    e.message
}

impl Root {
    pub fn new(backend: Backend, cx: &mut Context<Self>) -> Self {
        let cfg = Config::load_default().unwrap_or_default();
        let mut root = Self {
            backend: Arc::new(backend),
            cfg,
            focus: cx.focus_handle(),
            status: None,
            workspaces: Vec::new(),
            terminals: Vec::new(),
            loaded: false,
            selected: None,
            page: None,
            width: LIST_WIDTH,
            on_resize: None,
            floating: false,
            backdrop: Rc::new(RefCell::new(None)),
            cards: Rc::new(RefCell::new([None; 2])),
            dismissed: HashSet::new(),
            seen: HashMap::new(),
            menu: None,
            pop_up: None,
            confirm_remove: None,
            renaming: None,
            message: None,
            ghostty_error: None,
            refreshing: false,
            refresh_again: false,
            visible: false,
            closed_at: None,
            modal: false,
            on_attention: None,
            window_subscription: None,
            rename_subscriptions: Vec::new(),
        };
        root.refresh(cx);
        root.start_watch(cx);
        root
    }

    /// Called with `true` while any workspace has news for the user (the
    /// menu-bar icon shows a dot).
    #[cfg(target_os = "macos")]
    pub fn set_attention_handler(&mut self, f: impl Fn(bool) + 'static) {
        self.on_attention = Some(Box::new(f));
    }

    #[cfg(target_os = "macos")]
    pub fn set_resize_handler(&mut self, f: impl Fn(&Window, f32, &gpui::App) + 'static) {
        self.on_resize = Some(Box::new(f));
    }

    /// Use native right-click menus instead of drawing one.
    #[cfg(target_os = "macos")]
    pub fn set_pop_up(&mut self, f: impl Fn(&Window) -> Option<PopUp> + 'static) {
        self.pop_up = Some(Box::new(f));
    }

    /// Set for the current window; cleared when it closes.
    #[cfg(target_os = "macos")]
    pub fn set_backdrop(&mut self, f: impl FnMut(&[Bounds<Pixels>]) + 'static) {
        *self.backdrop.borrow_mut() = Some(Box::new(f));
    }

    #[cfg(target_os = "macos")]
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Bind to a freshly opened window: focus it and refresh. With
    /// `close_on_blur` (the menu-bar panel) it closes when it loses focus.
    pub fn attach(&mut self, window: &mut Window, close_on_blur: bool, cx: &mut Context<Self>) {
        self.visible = true;
        self.menu = None;
        // Each window opens with only the list (see `LIST_WIDTH`).
        self.page = None;
        self.width = LIST_WIDTH;
        self.floating = close_on_blur;
        self.focus.focus(window);
        self.window_subscription = close_on_blur.then(|| {
            cx.observe_window_activation(window, |this, window, cx| {
                if !window.is_window_active() && !this.modal {
                    this.close(window, cx);
                }
            })
        });
        self.refresh(cx);
    }

    pub fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.visible = false;
        self.closed_at = Some(Instant::now());
        self.menu = None;
        self.renaming = None;
        self.window_subscription = None;
        self.rename_subscriptions.clear();
        self.backdrop.borrow_mut().take();
        window.remove_window();
        cx.notify();
    }

    // ------------------------------------------------------------- loading

    /// Wake on host change notifications, or every few seconds for changes
    /// made outside ghostmc (new Ghostty tabs, tmux).
    fn start_watch(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let poller = match Backend::connect_or_direct() {
                Ok(b) => Arc::new(b),
                Err(_) => return,
            };
            let mut after = 0u64;
            loop {
                let p = poller.clone();
                let timeout = if p.is_host() { 8_000 } else { 5_000 };
                let r = cx
                    .background_executor()
                    .spawn(async move {
                        p.call::<Changed>(Request::WaitForChange {
                            after,
                            timeout_ms: timeout,
                        })
                    })
                    .await;
                match r {
                    Ok(c) => after = c.counter,
                    Err(_) => cx.background_executor().timer(Duration::from_secs(5)).await,
                }
                if this.update(cx, |this, cx| this.refresh(cx)).is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refreshing {
            self.refresh_again = true;
            return;
        }
        self.refreshing = true;
        let b = self.backend.clone();
        let tcfg = self.cfg.terminal.clone();
        let dismissed = self.dismissed.clone();
        // Listing Ghostty runs AppleScript; while the panel is closed reuse
        // the last listing, so agent news (the menu-bar dot) stays current.
        let known = (!self.visible && self.loaded).then(|| self.terminals.clone());
        cx.spawn(async move |this, cx| {
            let snap = cx
                .background_executor()
                .spawn(async move { load(&b, &tcfg, &dismissed, known) })
                .await;
            this.update(cx, |this, cx| {
                this.refreshing = false;
                this.apply(snap);
                if this.refresh_again {
                    this.refresh_again = false;
                    this.refresh(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn apply(&mut self, snap: Snapshot) {
        self.status = snap.status;
        if let Some(e) = snap.error {
            self.message = Some((e, true));
        }
        self.workspaces = snap.workspaces;
        self.terminals = snap.terminals;
        if snap.ghostty_error.is_some() || self.visible {
            self.ghostty_error = snap.ghostty_error;
        }
        self.loaded = true;
        let keys: Vec<&WorkspaceKey> = self.workspaces.iter().map(|w| &w.key).collect();
        if self.selected.as_ref().is_none_or(|s| !keys.contains(&s)) {
            self.selected = self.workspaces.first().map(|w| w.key.clone());
        }
        if self.visible && self.page == Some(Page::Workspace) {
            self.mark_selected_seen();
        }
        let any = self.workspaces.iter().any(|w| self.has_news(w));
        if let Some(f) = &self.on_attention {
            f(any);
        }
    }

    fn mark_selected_seen(&mut self) {
        let Some(w) = self.selected_workspace() else {
            return;
        };
        let marks: Vec<(String, i64)> = w
            .agents()
            .map(|(_, a)| (a.view.run.id.0.clone(), a.view.run.state_since))
            .collect();
        self.seen.extend(marks);
    }

    /// An agent finished or needs the user, and the user has not looked.
    fn has_news(&self, w: &Workspace) -> bool {
        w.agents().any(|(_, a)| {
            matches!(
                a.view.state,
                AgentState::NeedsApproval | AgentState::Failed | AgentState::TurnEnded
            ) && self.seen.get(&a.view.run.id.0) != Some(&a.view.run.state_since)
        })
    }

    fn selected_workspace(&self) -> Option<&Workspace> {
        let k = self.selected.as_ref()?;
        self.workspace(k)
    }

    fn workspace(&self, key: &WorkspaceKey) -> Option<&Workspace> {
        self.workspaces.iter().find(|w| &w.key == key)
    }

    /// A click on a workspace opens its pane, or closes it when it is the
    /// one already open.
    fn toggle_workspace(&mut self, key: WorkspaceKey, cx: &mut Context<Self>) {
        if self.page == Some(Page::Workspace) && self.selected.as_ref() == Some(&key) {
            self.page = None;
            self.menu = None;
            cx.notify();
        } else {
            self.select(key, cx);
        }
    }

    fn toggle_settings(&mut self, cx: &mut Context<Self>) {
        self.page = (self.page != Some(Page::Settings)).then_some(Page::Settings);
        self.menu = None;
        cx.notify();
    }

    /// Match the window's width to whether the right pane is open.
    fn fit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let want = if self.page.is_some() {
            WIDTH
        } else {
            LIST_WIDTH
        };
        if want == self.width {
            return;
        }
        self.width = want;
        match &self.on_resize {
            Some(f) => f(window, want, cx),
            None => window.resize(size(px(want), px(HEIGHT))),
        }
    }

    fn select(&mut self, key: WorkspaceKey, cx: &mut Context<Self>) {
        self.selected = Some(key);
        self.page = Some(Page::Workspace);
        self.menu = None;
        self.mark_selected_seen();
        let any = self.workspaces.iter().any(|w| self.has_news(w));
        if let Some(f) = &self.on_attention {
            f(any);
        }
        cx.notify();
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.workspaces.is_empty() {
            return;
        }
        let cur = self
            .selected
            .as_ref()
            .and_then(|s| self.workspaces.iter().position(|w| &w.key == s))
            .unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(self.workspaces.len() as isize) as usize;
        self.select(self.workspaces[next].key.clone(), cx);
    }

    // ------------------------------------------------------------- actions

    /// Run blocking work off the UI thread and report its outcome; an empty
    /// success message clears the status line.
    fn run(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&Backend, &Config) -> Result<String, String> + Send + 'static,
    ) {
        let b = self.backend.clone();
        let cfg = self.cfg.clone();
        cx.spawn(async move |this, cx| {
            let r = cx
                .background_executor()
                .spawn(async move { f(&b, &cfg) })
                .await;
            this.update(cx, |this, cx| {
                this.message = match r {
                    Ok(m) if m.is_empty() => None,
                    Ok(m) => Some((m, false)),
                    Err(e) => Some((e, true)),
                };
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Go to a row: focus its Ghostty tab, else attach its tmux session,
    /// else open a shell there. Nothing is typed into any terminal.
    fn open_row(&mut self, row: Row, cx: &mut Context<Self>) {
        self.run(cx, move |b, cfg| {
            if let Some(t) = &row.terminal_id {
                ghostty::focus_terminal(&cfg.terminal, t).map_err(|e| e.to_string())?;
                return Ok(String::new());
            }
            if let Some(key) = row.session_key {
                let cmd: AttachCommand = b
                    .call(Request::AttachCommand {
                        key,
                        mode: AttachMode::Writable,
                    })
                    .map_err(err_text)?;
                ghostty::launch(
                    &cfg.terminal,
                    &cmd.argv,
                    cmd.cwd.as_deref(),
                    &Placement::NewWindow,
                )
                .map_err(|e| e.to_string())?;
                return Ok("Attached the tmux session in a new Ghostty window".into());
            }
            ghostty::launch(&cfg.terminal, &[], Some(&row.path), &Placement::NewWindow)
                .map_err(|e| e.to_string())?;
            Ok(format!("Opened a shell in {}", tilde(&row.path)))
        });
    }

    fn open_terminal(&mut self, cx: &mut Context<Self>) {
        let Some(w) = self.selected_workspace() else {
            return;
        };
        let row = w
            .rows
            .iter()
            .find(|r| r.terminal_id.is_some())
            .or(w.rows.first())
            .cloned();
        if let Some(r) = row {
            self.open_row(r, cx);
        }
    }

    fn open_folder(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self
            .selected_workspace()
            .and_then(|w| w.rows.first())
            .map(|r| r.path.clone())
        else {
            return;
        };
        self.run(cx, move |_, _| {
            // The system handler (Finder), not the configured editor.
            ghostmc_platform::open::open_in_editor(&[], Path::new(&path))
                .map_err(|e| e.to_string())?;
            Ok(String::new())
        });
    }

    fn new_terminal(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.cfg.desktop.new_terminal_dir() else {
            self.message = Some(("No home directory to open".into(), true));
            cx.notify();
            return;
        };
        self.run(cx, move |_, cfg| {
            let d = dir.to_string_lossy();
            ghostty::launch(&cfg.terminal, &[], Some(&d), &Placement::NewWindow)
                .map_err(|e| e.to_string())?;
            Ok(String::new())
        });
    }

    fn toggle_pin(&mut self, key: WorkspaceKey, cx: &mut Context<Self>) {
        let Some(w) = self.workspace(&key).cloned() else {
            return;
        };
        let taken = self.project_names();
        self.menu = None;
        self.run(cx, move |b, _| {
            let id = ensure_saved(b, &w, &taken)?;
            b.call::<serde_json::Value>(update(id, None, Some(!w.pinned)))
                .map_err(err_text)?;
            Ok(String::new())
        });
    }

    fn start_rename(&mut self, key: WorkspaceKey, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.workspace(&key).map(|w| w.name.clone()) else {
            return;
        };
        self.menu = None;
        let accent = theme::palette(window).accent;
        let input = cx.new(|cx| {
            let mut i = TextInput::new(&name, cx);
            i.cursor_color = accent;
            i
        });
        let k = key.clone();
        self.rename_subscriptions = vec![
            cx.subscribe(&input, move |this, _, e: &Submitted, cx| {
                this.finish_rename(k.clone(), e.0.clone(), cx);
            }),
            cx.subscribe(&input, |this, _, _: &Cancelled, cx| {
                this.renaming = None;
                cx.notify();
            }),
        ];
        input.read(cx).focus_handle(cx).focus(window);
        self.renaming = Some((key, input));
        cx.notify();
    }

    fn finish_rename(&mut self, key: WorkspaceKey, name: String, cx: &mut Context<Self>) {
        self.renaming = None;
        cx.notify();
        let Some(w) = self.workspace(&key).cloned() else {
            return;
        };
        if name.is_empty() || name == w.name {
            return;
        }
        let taken = self.project_names();
        self.run(cx, move |b, _| {
            let id = ensure_saved(b, &w, &taken)?;
            b.call::<serde_json::Value>(update(id, Some(name), None))
                .map_err(err_text)?;
            Ok(String::new())
        });
    }

    /// Right-click on a workspace: a native menu where there is one, else
    /// the drawn menu.
    fn open_menu(
        &mut self,
        key: WorkspaceKey,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(show) = self.pop_up.as_ref().and_then(|f| f(window)) else {
            self.menu = Some(Menu {
                key,
                at,
                confirm_remove: false,
            });
            cx.notify();
            return;
        };
        let pin = if self.workspace(&key).is_some_and(|w| w.pinned) {
            "Unpin"
        } else {
            "Pin"
        };
        cx.spawn_in(window, async move |this, cx| {
            let choice = show(&[pin, "Rename", "Remove…"], at);
            this.update_in(cx, |this, window, cx| match choice {
                Some(0) => this.toggle_pin(key, cx),
                Some(1) => this.start_rename(key, window, cx),
                Some(2) => {
                    this.confirm_remove = Some(key);
                    cx.notify();
                }
                _ => {}
            })
            .ok();
        })
        .detach();
    }

    fn remove(&mut self, key: WorkspaceKey, cx: &mut Context<Self>) {
        self.menu = None;
        self.confirm_remove = None;
        let Some(w) = self.workspace(&key).cloned() else {
            return;
        };
        // Its open windows would otherwise come straight back as live
        // workspaces.
        self.dismissed
            .extend(w.rows.iter().filter_map(|r| r.window_id.clone()));
        match key {
            WorkspaceKey::Window(_) => {
                self.workspaces.retain(|x| x.key != key);
                self.refresh(cx);
            }
            WorkspaceKey::Project(id) => {
                self.run(cx, move |b, _| {
                    b.call::<serde_json::Value>(Request::DeleteProject { key: id })
                        .map_err(err_text)?;
                    Ok(format!(
                        "Removed “{}”. Its folders and terminals are untouched.",
                        w.name
                    ))
                });
            }
        }
        cx.notify();
    }

    fn project_names(&self) -> Vec<String> {
        self.workspaces
            .iter()
            .filter(|w| w.is_saved())
            .map(|w| w.name.clone())
            .collect()
    }

    fn choose_folder(&mut self, cx: &mut Context<Self>) {
        self.modal = true;
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Use for new terminals".into()),
        });
        cx.spawn(async move |this, cx| {
            let picked = rx.await.ok().and_then(Result::ok).flatten();
            this.update(cx, |this, cx| {
                this.modal = false;
                if let Some(dir) = picked.and_then(|v| v.into_iter().next()) {
                    this.save_folder(dir);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn save_folder(&mut self, dir: PathBuf) {
        let value = tilde(&dir.to_string_lossy());
        let path = ghostmc_core::dirs::config_path();
        match ghostmc_core::config::set_new_terminal_dir(&path, &value) {
            Ok(()) => {
                if let Ok(c) = Config::load_default() {
                    self.cfg = c;
                }
                self.message = Some((format!("New terminals open in {value}"), false));
            }
            Err(e) => self.message = Some((e.to_string(), true)),
        }
    }

    // ------------------------------------------------------------- drawing

    /// `message` is the status line, shown here while the right pane is
    /// closed.
    fn render_list(&self, message: Option<Div>, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let mut list = div()
            .id("workspaces")
            .min_h(px(0.))
            .overflow_y_scroll()
            .p_1p5()
            .flex()
            .flex_col()
            .gap_0p5();
        if self.loaded && self.workspaces.is_empty() {
            list =
                list.child(
                    div().p_4().text_sm().text_color(p.muted).child(
                        "No workspaces yet. Open a Ghostty window, or press + to start one.",
                    ),
                );
            if let Some(e) = &self.ghostty_error {
                list = list.child(
                    div()
                        .px_4()
                        .text_xs()
                        .text_color(p.warn)
                        .child(format!("Ghostty: {e}")),
                );
            }
        }
        for (i, w) in self.workspaces.iter().enumerate() {
            list = list.child(self.render_workspace(i, w, p, cx));
        }
        let settings_color = if self.page == Some(Page::Settings) {
            p.accent
        } else {
            p.text
        };
        let hover = p.hover;
        div()
            .w(px(LIST_CARD))
            .max_h(px(MAX_CARD_HEIGHT))
            .flex()
            .flex_col()
            .flex_shrink_0()
            .bg(p.bg)
            .into_card(p, self.card_probe(0))
            .child(
                div()
                    .px_4()
                    .pt_3()
                    .pb_2()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(p.border)
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.muted)
                            .child("WORKSPACE"),
                    )
                    .child(
                        div()
                            .id("new-terminal")
                            .px_1p5()
                            .rounded_md()
                            .text_lg()
                            .cursor_pointer()
                            .hover(move |s| s.bg(hover))
                            .on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| this.new_terminal(cx)),
                            )
                            .child("+"),
                    ),
            )
            .child(list)
            .children(message)
            .child(
                div()
                    .h(px(40.))
                    .px_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_t_1()
                    .border_color(p.border)
                    .text_sm()
                    .child(
                        div()
                            .id("settings")
                            .px_1()
                            .rounded_md()
                            .cursor_pointer()
                            .text_color(settings_color)
                            .hover(move |s| s.bg(hover))
                            .on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_settings(cx)),
                            )
                            .child("Settings"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.muted)
                            .child(concat!("v", env!("CARGO_PKG_VERSION"))),
                    ),
            )
            .into_any_element()
    }

    fn render_workspace(
        &self,
        i: usize,
        w: &Workspace,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let selected = self.page == Some(Page::Workspace) && self.selected.as_ref() == Some(&w.key);
        let key = w.key.clone();
        let menu_key = w.key.clone();
        let renaming = self
            .renaming
            .as_ref()
            .filter(|(k, _)| k == &w.key)
            .map(|(_, input)| input.clone());
        let title = match renaming {
            Some(input) => div()
                .flex_1()
                .px_1()
                .rounded_sm()
                .border_1()
                .border_color(p.accent)
                .bg(p.raised)
                .child(input)
                .into_any_element(),
            None => div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .font_weight(FontWeight::MEDIUM)
                .child(w.name.clone())
                .into_any_element(),
        };
        let hover = p.hover;
        let mut block = div()
            .id(("ws", i))
            .px_2p5()
            .py_2()
            .flex()
            .flex_col()
            .gap_1()
            .rounded_lg()
            .cursor_pointer()
            .when(selected, |d| d.bg(p.selected))
            .when(!selected, |d| d.hover(move |s| s.bg(hover)))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.toggle_workspace(key.clone(), cx)
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                    this.open_menu(menu_key.clone(), e.position, window, cx)
                }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .child(title)
                    .when(w.pinned, |d| {
                        d.child(div().text_xs().text_color(p.muted).child("pinned"))
                    })
                    .when(self.has_news(w), |d| d.child(div().text_sm().child("🔔"))),
            );
        if self.confirm_remove.as_ref() == Some(&w.key) {
            block = block.child(self.render_confirm_remove(w.key.clone(), p, cx));
        }
        for (j, r) in w.rows.iter().enumerate() {
            block = block.child(self.render_row(i, j, r, p, cx));
        }
        block
    }

    fn render_confirm_remove(&self, key: WorkspaceKey, p: &Palette, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .py_1()
            .child(
                // Full contrast: muted text gets lost on glass over dark windows.
                div().text_xs().child("Remove from the list?"),
            )
            .child(
                div()
                    .flex()
                    .gap_1p5()
                    .child(button(
                        "remove-cancel",
                        "Cancel",
                        p,
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            this.confirm_remove = None;
                            cx.notify();
                        }),
                    ))
                    .child(
                        button(
                            "remove-confirm",
                            "Remove",
                            p,
                            cx.listener(move |this, _: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                this.remove(key.clone(), cx);
                            }),
                        )
                        .text_color(p.bad),
                    ),
            )
    }

    fn render_row(
        &self,
        i: usize,
        j: usize,
        r: &Row,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let row = r.clone();
        let live = r.terminal_id.is_some() || r.session_key.is_some();
        let hover = p.hover;
        div()
            .id(SharedString::from(format!("row-{i}-{j}")))
            .flex()
            .items_center()
            .gap_1p5()
            .text_xs()
            .rounded_sm()
            .hover(move |s| s.bg(hover))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                this.open_row(row.clone(), cx)
            }))
            .child(agent_glyph(r.agents.first(), p))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .font_family(theme::mono())
                    .text_color(if live { p.text } else { p.muted })
                    .child(tilde(&r.path)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(p.muted)
                    .child(r.branch.clone().unwrap_or_default()),
            )
    }

    fn render_panel(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        if self.page == Some(Page::Settings) {
            return self.render_settings(p, cx);
        }
        let Some(w) = self.selected_workspace() else {
            return div()
                .p_6()
                .text_sm()
                .text_color(p.muted)
                .child(if self.loaded {
                    "Select a workspace."
                } else {
                    "Loading…"
                })
                .into_any_element();
        };
        let mut body = div()
            .id("agents")
            .min_h(px(0.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2();
        let agents: Vec<(&Row, &RowAgent)> = w.agents().collect();
        if agents.is_empty() {
            body = body.child(
                div()
                    .text_sm()
                    .text_color(p.muted)
                    .child("No agent activity in this workspace."),
            );
        }
        for (n, (row, a)) in agents.into_iter().enumerate() {
            body = body.child(self.render_agent(n, row, a, p, cx));
        }
        if self.status.as_ref().is_some_and(|s| !s.host_running) {
            body = body.child(div().text_xs().text_color(p.muted).child(
                "Agent replies show here while the ghostmc host runs (`ghostmc host run`).",
            ));
        }
        if let Some(e) = &self.ghostty_error {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(p.warn)
                    .child(format!("Ghostty: {e}")),
            );
        }
        div()
            .min_h(px(0.))
            .p_4()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(button(
                        "focus",
                        "Open / Focus terminal",
                        p,
                        cx.listener(|this, _: &ClickEvent, _, cx| this.open_terminal(cx)),
                    ))
                    .child(button(
                        "folder",
                        "Open Folder",
                        p,
                        cx.listener(|this, _: &ClickEvent, _, cx| this.open_folder(cx)),
                    )),
            )
            .child(body)
            .into_any_element()
    }

    fn render_agent(
        &self,
        n: usize,
        row: &Row,
        a: &RowAgent,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let target = row.clone();
        let v = &a.view;
        let text = v
            .brief
            .clone()
            .or_else(|| v.run.last_message.clone())
            .unwrap_or_default();
        let hover = p.hover;
        div()
            .id(("agent", n))
            .p_2()
            .rounded_md()
            .flex()
            .flex_col()
            .gap_1()
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .on_click(
                cx.listener(move |this, _: &ClickEvent, _, cx| this.open_row(target.clone(), cx)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .child(agent_glyph(Some(a), p))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .font_family(theme::mono())
                            .child(tilde(&row.path)),
                    )
                    .child(
                        div()
                            .text_color(p.muted)
                            .child(row.branch.clone().unwrap_or_default()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .text_xs()
                    .child(
                        div()
                            .text_color(state_color(v.state, p))
                            .child(state_text(v.state)),
                    )
                    .child(div().text_color(p.muted).child(ago(v.run.state_since)))
                    .when(!a.verified, |d| {
                        d.child(div().text_color(p.muted).child("· matched by folder"))
                    }),
            )
            .when(!text.is_empty(), |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(p.muted)
                        .line_clamp(6)
                        .child(text),
                )
            })
    }

    fn render_settings(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let dir = self
            .cfg
            .desktop
            .new_terminal_dir()
            .map(|d| tilde(&d.to_string_lossy()))
            .unwrap_or_else(|| "home folder".into());
        let host = match &self.status {
            Some(s) if s.host_running => match s.host_pid {
                Some(pid) => format!("Running (pid {pid})"),
                None => "Running".into(),
            },
            Some(_) => "Not running. Start it with `ghostmc host run`.".into(),
            None => "Unknown".into(),
        };
        let muted = p.muted;
        let line = move |label: &'static str, value: String| {
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(div().text_xs().text_color(muted).child(label))
                .child(div().text_sm().child(value))
        };
        div()
            .p_4()
            .flex()
            .flex_col()
            .gap_4()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Settings"))
            .child(
                div()
                    .flex()
                    .items_end()
                    .justify_between()
                    .gap_2()
                    .child(line("New terminals (+) open in", dir))
                    .child(button(
                        "choose",
                        "Change…",
                        p,
                        cx.listener(|this, _: &ClickEvent, _, cx| this.choose_folder(cx)),
                    )),
            )
            .child(line("Host", host))
            .child(line(
                "Config file",
                tilde(&ghostmc_core::dirs::config_path().to_string_lossy()),
            ))
            .child(div().flex().child(button(
                "quit",
                "Quit Ghost May Cry",
                p,
                cx.listener(|_, _: &ClickEvent, _, cx| cx.quit()),
            )))
            .into_any_element()
    }

    fn render_menu(&self, m: &Menu, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let hover = p.hover;
        let item = move |id: &'static str, label: String| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .rounded_md()
                .text_sm()
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .child(label)
        };
        let pinned = self.workspace(&m.key).is_some_and(|w| w.pinned);
        let name = self
            .workspace(&m.key)
            .map(|w| w.name.clone())
            .unwrap_or_default();
        let (k1, k2, k3) = (m.key.clone(), m.key.clone(), m.key.clone());
        let mut menu = div()
            .id("menu")
            .min_w(px(160.))
            .p_1()
            .flex()
            .flex_col()
            .rounded(px(10.))
            .border_1()
            .border_color(p.border)
            .bg(p.raised)
            .shadow_md()
            // Without this a click on an item also reaches the row below it.
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.menu = None;
                cx.notify();
            }));
        if m.confirm_remove {
            menu = menu
                .child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(p.muted)
                        .child(format!("Remove “{name}” from the list?")),
                )
                .child(
                    item("remove-yes", "Remove".into())
                        .text_color(p.bad)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.remove(k3.clone(), cx)
                        })),
                )
                .child(item("remove-no", "Cancel".into()).on_click(cx.listener(
                    |this, _: &ClickEvent, _, cx| {
                        this.menu = None;
                        cx.notify();
                    },
                )));
        } else {
            menu = menu
                .child(
                    item("pin", if pinned { "Unpin" } else { "Pin" }.into()).on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| this.toggle_pin(k1.clone(), cx),
                    )),
                )
                .child(item("rename", "Rename".into()).on_click(cx.listener(
                    move |this, _: &ClickEvent, window, cx| {
                        this.start_rename(k2.clone(), window, cx)
                    },
                )))
                .child(item("remove", "Remove…".into()).on_click(cx.listener(
                    |this, _: &ClickEvent, _, cx| {
                        if let Some(m) = &mut this.menu {
                            m.confirm_remove = true;
                        }
                        cx.notify();
                    },
                )));
        }
        deferred(anchored().position(m.at).child(menu))
            .with_priority(1)
            .into_any_element()
    }
}

/// Fetch everything and group it. Runs on a background thread. `known`
/// terminals are reused instead of asking Ghostty again.
fn load(
    b: &Backend,
    tcfg: &ghostmc_core::config::TerminalConfig,
    dismissed: &HashSet<String>,
    known: Option<Vec<ghostty::GhosttyTerminal>>,
) -> Snapshot {
    let mut s = Snapshot::default();
    let projects: Vec<ProjectSummary> = match b.call(Request::ListProjects) {
        Ok(p) => p,
        Err(e) => {
            s.error = Some(err_text(e));
            Vec::new()
        }
    };
    s.status = b.call(Request::Status).ok();
    let sessions: Vec<SessionView> = b
        .call(Request::Sessions { project: None })
        .unwrap_or_default();
    let agents: Vec<AgentView> = b
        .call(Request::AgentRuns {
            since_secs: None,
            briefs: true,
        })
        .unwrap_or_default();
    let terminals = match known {
        Some(t) => t,
        None if ghostty::capabilities(tcfg).list_terminals => ghostty::list_terminals(tcfg)
            .unwrap_or_else(|e| {
                s.ghostty_error = Some(e.to_string());
                Vec::new()
            }),
        None => Vec::new(),
    };
    let cache: RefCell<HashMap<String, Option<GitIdentity>>> = RefCell::new(HashMap::new());
    let git_of = |dir: &str| -> Option<GitIdentity> {
        if let Some(hit) = cache.borrow().get(dir) {
            return hit.clone();
        }
        let g = ghostmc_core::git::detect(Path::new(dir)).ok().flatten();
        cache.borrow_mut().insert(dir.to_owned(), g.clone());
        g
    };
    s.workspaces = workspace::build(
        &workspace::Inputs {
            projects: &projects,
            sessions: &sessions,
            agents: &agents,
            terminals: &terminals,
            dismissed,
        },
        &git_of,
    );
    s.terminals = terminals;
    s
}

/// Save a live workspace as a project (its tabs' folders become locations)
/// and return the project id; a saved one returns its id.
fn ensure_saved(b: &Backend, w: &Workspace, taken: &[String]) -> Result<String, String> {
    if let WorkspaceKey::Project(id) = &w.key {
        return Ok(id.clone());
    }
    let mut paths: Vec<&str> = Vec::new();
    for r in &w.rows {
        if !paths.contains(&r.path.as_str()) {
            paths.push(&r.path);
        }
    }
    let first = paths.first().ok_or("workspace has no folders")?;
    let taken: Vec<&str> = taken.iter().map(String::as_str).collect();
    let created: ProjectSummary = b
        .call(Request::CreateProject {
            name: workspace::unique_name(&w.name, &taken),
            tags: vec![],
            notes: None,
            path: Some((*first).to_owned()),
        })
        .map_err(err_text)?;
    let id = created.project.id.0;
    for p in &paths[1..] {
        b.call::<serde_json::Value>(Request::AddLocation {
            project: id.clone(),
            path: (*p).to_owned(),
            device: None,
            default_command: None,
        })
        .map_err(err_text)?;
    }
    Ok(id)
}

fn update(key: String, name: Option<String>, pinned: Option<bool>) -> Request {
    Request::UpdateProject {
        key,
        name,
        notes: None,
        pinned,
        add_tags: vec![],
        remove_tags: vec![],
    }
}

impl Focusable for Root {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.fit(window, cx);
        *self.cards.borrow_mut() = [None; 2];
        let solid = theme::palette(window);
        let p = if self.backdrop.borrow().is_some() {
            theme::on_glass(window)
        } else {
            solid
        };
        // The menu can reach past the cards, where there is no glass.
        let menu = self.menu.as_ref().map(|m| self.render_menu(m, &solid, cx));
        let open = self.page.is_some();
        let message = self.message.clone().map(|(m, bad)| {
            div()
                .px_4()
                .py_2()
                .border_t_1()
                .border_color(p.border)
                .text_xs()
                .text_color(if bad { p.bad } else { p.muted })
                .child(m)
        });
        let (list_message, pane_message) = if open {
            (None, message)
        } else {
            (message, None)
        };
        div()
            .track_focus(&self.focus)
            .key_context("ghostmc")
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.step(1, cx)))
            .on_action(cx.listener(|this, _: &SelectPrev, _, cx| this.step(-1, cx)))
            .on_action(cx.listener(|this, _: &OpenTerminal, _, cx| this.open_terminal(cx)))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.refresh(cx)))
            .on_action(cx.listener(|this, _: &Dismiss, window, cx| {
                if this.menu.take().is_some()
                    || this.confirm_remove.take().is_some()
                    || this.page.take().is_some()
                {
                    cx.notify();
                } else {
                    this.close(window, cx);
                }
            }))
            .size_full()
            .flex()
            .items_start()
            .gap(px(GAP))
            .pt(px(MARGIN_TOP))
            .px(px(MARGIN))
            .pb(px(MARGIN))
            .when(!self.floating, |d| d.bg(p.panel))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    // Only clicks around the cards get here.
                    if this.floating {
                        this.close(window, cx);
                    }
                }),
            )
            .text_color(p.text)
            .font_family(theme::ui_font())
            .child(self.render_list(list_message, &p, cx))
            .when(open, |d| {
                d.child(
                    div()
                        .w(px(PANE_CARD))
                        .max_h(px(MAX_CARD_HEIGHT))
                        .flex()
                        .flex_col()
                        .bg(p.panel)
                        .child(self.render_panel(&p, cx))
                        .children(pane_message)
                        .into_card(&p, self.card_probe(1)),
                )
            })
            .children(menu)
            .child(self.backdrop_painter())
    }
}

// ------------------------------------------------------------- components

trait Card {
    /// Round a card, outline and lift it when it paints its own fill (on
    /// glass the glass draws the edge), record its bounds with `probe`, and
    /// keep its clicks from reaching the transparent area around it (which
    /// closes the panel).
    fn into_card(self, p: &Palette, probe: Option<AnyElement>) -> Self;
}

impl Card for Div {
    fn into_card(self, p: &Palette, probe: Option<AnyElement>) -> Self {
        let on_glass = probe.is_some();
        self.relative()
            .rounded(px(CARD_RADIUS))
            .overflow_hidden()
            .when(!on_glass, |d| {
                d.border_1().border_color(p.border).shadow_lg()
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .children(probe)
    }
}

impl Root {
    /// An invisible child recording card `slot`'s bounds, when a backdrop
    /// needs them.
    fn card_probe(&self, slot: usize) -> Option<AnyElement> {
        self.backdrop.borrow().as_ref()?;
        let cards = self.cards.clone();
        Some(
            canvas(
                move |bounds, _, _| cards.borrow_mut()[slot] = Some(bounds),
                |_, _, _, _| {},
            )
            .absolute()
            .size_full()
            .into_any_element(),
        )
    }

    /// Hands the cards' bounds to the backdrop once all of them are laid
    /// out (paint runs after every prepaint).
    fn backdrop_painter(&self) -> impl IntoElement {
        let (cards, backdrop) = (self.cards.clone(), self.backdrop.clone());
        canvas(
            |_, _, _| {},
            move |_, _, _, _| {
                let frames: Vec<Bounds<Pixels>> =
                    cards.borrow().iter().flatten().copied().collect();
                if let Some(f) = backdrop.borrow_mut().as_mut() {
                    f(&frames);
                }
            },
        )
        .absolute()
        .size_0()
    }
}

fn button(
    id: &'static str,
    label: &'static str,
    p: &Palette,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> Stateful<Div> {
    let hover = p.hover;
    div()
        .id(id)
        .px_3p5()
        .py_1()
        .rounded_full()
        .border_1()
        .border_color(p.border)
        .bg(p.raised)
        .text_sm()
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .on_click(on_click)
        .child(label)
}

/// Agent mark in front of a path: Claude's asterisk or Codex's diamond, or
/// a blank of the same width.
fn agent_glyph(a: Option<&RowAgent>, p: &Palette) -> Div {
    let (glyph, color) = match a.map(|a| a.view.run.agent) {
        Some(AgentKind::Claude) => ("✳", p.claude),
        Some(AgentKind::Codex) => ("◆", p.codex),
        None => ("", p.muted),
    };
    div()
        .w(px(14.))
        .flex_shrink_0()
        .text_color(color)
        .child(glyph)
}

fn state_text(s: AgentState) -> &'static str {
    match s {
        AgentState::Running => "Working",
        AgentState::NeedsApproval => "Needs approval",
        AgentState::Idle => "Idle",
        AgentState::TurnEnded => "Finished its turn",
        AgentState::Failed => "Failed",
        AgentState::Ended => "Ended",
        AgentState::Disconnected => "Disconnected",
        AgentState::Unknown => "No recent activity",
    }
}

fn state_color(s: AgentState, p: &Palette) -> gpui::Rgba {
    match s {
        AgentState::Running => p.accent,
        AgentState::NeedsApproval | AgentState::Failed => p.warn,
        _ => p.muted,
    }
}

fn ago(ts: i64) -> String {
    let d = (ghostmc_core::now() - ts).max(0);
    match d {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", d / 60),
        3600..86400 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

/// `~/…` for paths under the home directory.
fn tilde(path: &str) -> String {
    let Some(home) = std::env::var("HOME").ok().filter(|h| !h.is_empty()) else {
        return path.to_owned();
    };
    if path == home {
        return "~".into();
    }
    match path.strip_prefix(&format!("{home}/")) {
        Some(rest) => format!("~/{rest}"),
        None => path.to_owned(),
    }
}
