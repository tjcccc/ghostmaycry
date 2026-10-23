//! Main window: project sidebar + project inspector.

use crate::theme;
use ghostmc_core::agent::AgentState;
use ghostmc_core::config::Config;
use ghostmc_core::files::Listing;
use ghostmc_core::git::{ChangeKind, Diff, GitStatus};
use ghostmc_core::matching::Association;
use ghostmc_host::Backend;
use ghostmc_platform::ghostty::{self, Placement};
use ghostmc_protocol::*;
use ghostmc_tmux::AttachMode;
use gpui::{
    AnyElement, ClickEvent, Context, Div, FocusHandle, Focusable, FontWeight, SharedString,
    Stateful, Window, actions, div, prelude::*, px,
};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

actions!(
    ghostmc,
    [
        NextProject,
        PrevProject,
        OpenTerminal,
        Refresh,
        ShowSessions,
        ShowChanges,
        ShowFiles,
        ShowAgents,
        Quit
    ]
);

/// Diff lines rendered at most; the rest is summarized.
const MAX_DIFF_LINES: usize = 1500;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Sessions,
    Changes,
    Files,
    Agents,
}

/// Data loaded off the UI thread.
#[derive(Default)]
struct Snapshot {
    status: Option<StatusInfo>,
    projects: Vec<ProjectSummary>,
    agents: Vec<AgentView>,
    error: Option<String>,
}

pub struct Root {
    backend: Arc<Backend>,
    cfg: Config,
    focus: FocusHandle,
    status: Option<StatusInfo>,
    projects: Vec<ProjectSummary>,
    agents: Vec<AgentView>,
    selected: Option<String>,
    tab: Tab,
    plan: Option<OpenPlan>,
    plan_error: Option<String>,
    git: Option<Result<GitStatus, String>>,
    diff: Option<(String, Diff)>,
    dir: String,
    listing: Option<Result<Listing, String>>,
    message: Option<(String, bool)>,
    busy: bool,
}

fn err_text(e: ApiError) -> String {
    e.message
}

impl Root {
    pub fn new(backend: Backend, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window);
        let cfg = Config::load_default().unwrap_or_default();
        let mut root = Self {
            backend: Arc::new(backend),
            cfg,
            focus,
            status: None,
            projects: Vec::new(),
            agents: Vec::new(),
            selected: None,
            tab: Tab::Sessions,
            plan: None,
            plan_error: None,
            git: None,
            diff: None,
            dir: String::new(),
            listing: None,
            message: None,
            busy: false,
        };
        root.refresh(cx);
        root.start_watch(cx);
        root
    }

    // ------------------------------------------------------------- loading

    /// Wake on host change notifications (or every few seconds in direct
    /// mode / for tmux changes made outside ghostmc).
    fn start_watch(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let poller = match Backend::connect_or_direct() {
                Ok(b) => Arc::new(b),
                Err(_) => return,
            };
            let mut after = 0u64;
            loop {
                let p = poller.clone();
                let timeout = if p.is_host() { 10_000 } else { 5_000 };
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
        let b = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let snap = cx
                .background_executor()
                .spawn(async move {
                    let mut s = Snapshot::default();
                    match b.call::<Vec<ProjectSummary>>(Request::ListProjects) {
                        Ok(p) => s.projects = p,
                        Err(e) => s.error = Some(err_text(e)),
                    }
                    s.status = b.call(Request::Status).ok();
                    s.agents = b
                        .call(Request::AgentRuns { since_secs: None })
                        .unwrap_or_default();
                    s
                })
                .await;
            this.update(cx, |this, cx| {
                this.status = snap.status;
                this.projects = snap.projects;
                this.agents = snap.agents;
                if let Some(e) = snap.error {
                    this.message = Some((e, true));
                }
                if this.selected.is_none() {
                    this.selected = this.projects.first().map(|p| p.project.id.0.clone());
                }
                this.load_selected(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn load_selected(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.selected.clone() else {
            return;
        };
        let b = self.backend.clone();
        let cfg = self.cfg.terminal.clone();
        let tab = self.tab;
        let dir = self.dir.clone();
        cx.spawn(async move |this, cx| {
            let (plan, git, listing) = cx
                .background_executor()
                .spawn(async move {
                    let plan = b
                        .call::<OpenPlan>(Request::OpenPlan {
                            project,
                            location_id: None,
                        })
                        .map(|mut p| {
                            ghostmc_host::desktop::augment_plan(&mut p, &cfg);
                            p
                        });
                    let loc = plan.as_ref().ok().map(|p| p.location.location.id.0.clone());
                    let git = match (&loc, tab) {
                        (Some(l), Tab::Changes) => Some(
                            b.call::<GitStatus>(Request::GitStatus {
                                location_id: l.clone(),
                            })
                            .map_err(err_text),
                        ),
                        _ => None,
                    };
                    let listing = match (&loc, tab) {
                        (Some(l), Tab::Files) => Some(
                            b.call::<Listing>(Request::ListFiles {
                                location_id: l.clone(),
                                rel: dir,
                                show_hidden: false,
                            })
                            .map_err(err_text),
                        ),
                        _ => None,
                    };
                    (plan, git, listing)
                })
                .await;
            this.update(cx, |this, cx| {
                match plan {
                    Ok(p) => {
                        this.plan = Some(p);
                        this.plan_error = None;
                    }
                    Err(e) => {
                        this.plan = None;
                        this.plan_error = Some(e.message);
                    }
                }
                if git.is_some() {
                    this.git = git;
                }
                if listing.is_some() {
                    this.listing = listing;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn select(&mut self, id: String, cx: &mut Context<Self>) {
        if self.selected.as_deref() != Some(id.as_str()) {
            self.selected = Some(id);
            self.plan = None;
            self.git = None;
            self.diff = None;
            self.dir.clear();
            self.listing = None;
        }
        self.load_selected(cx);
        cx.notify();
    }

    fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        self.load_selected(cx);
        cx.notify();
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.projects.is_empty() {
            return;
        }
        let cur = self
            .selected
            .as_ref()
            .and_then(|s| self.projects.iter().position(|p| &p.project.id.0 == s))
            .unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(self.projects.len() as isize) as usize;
        self.select(self.projects[next].project.id.0.clone(), cx);
    }

    // ------------------------------------------------------------- actions

    /// Run blocking work off the UI thread and report its outcome.
    fn run(
        &mut self,
        label: &str,
        cx: &mut Context<Self>,
        f: impl FnOnce(&Backend, &Config) -> Result<String, String> + Send + 'static,
    ) {
        self.busy = true;
        self.message = Some((format!("{label}…"), false));
        cx.notify();
        let b = self.backend.clone();
        let cfg = self.cfg.clone();
        cx.spawn(async move |this, cx| {
            let r = cx
                .background_executor()
                .spawn(async move { f(&b, &cfg) })
                .await;
            this.update(cx, |this, cx| {
                this.busy = false;
                this.message = Some(match r {
                    Ok(m) => (m, false),
                    Err(e) => (e, true),
                });
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    fn focus_terminal(&mut self, id: String, cx: &mut Context<Self>) {
        self.run("Focusing Ghostty", cx, move |_, cfg| {
            ghostty::focus_terminal(&cfg.terminal, &id).map_err(|e| e.to_string())?;
            Ok("Focused the existing Ghostty terminal (no input sent)".into())
        });
    }

    fn attach(&mut self, key: String, mode: AttachMode, cx: &mut Context<Self>) {
        self.run(
            if mode == AttachMode::Observe {
                "Opening observer"
            } else {
                "Attaching"
            },
            cx,
            move |b, cfg| {
                let cmd: AttachCommand = b
                    .call(Request::AttachCommand { key, mode })
                    .map_err(err_text)?;
                let l = ghostty::launch(
                    &cfg.terminal,
                    &cmd.argv,
                    cmd.cwd.as_deref(),
                    &Placement::NewWindow,
                )
                .map_err(|e| e.to_string())?;
                Ok(format!(
                    "Attached {} via {}",
                    if mode == AttachMode::Observe {
                        "read-only"
                    } else {
                        "writable"
                    },
                    l.method
                ))
            },
        );
    }

    fn create(&mut self, location_id: String, intent: CreateIntent, cx: &mut Context<Self>) {
        self.run("Creating tmux session", cx, move |b, cfg| {
            let c: CreatedSession = b
                .call(Request::CreateSession {
                    location_id,
                    intent,
                    command: vec![],
                })
                .map_err(err_text)?;
            let cmd: AttachCommand = b
                .call(Request::AttachCommand {
                    key: c.session.key.clone(),
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
            Ok(format!(
                "{} tmux session {} and attached it",
                if c.created {
                    "Created"
                } else {
                    "Reused existing"
                },
                c.session.name
            ))
        });
    }

    /// Primary action: focus/attach the single verified session, otherwise
    /// ask the user to pick from the Sessions tab.
    fn open_terminal(&mut self, cx: &mut Context<Self>) {
        let Some(plan) = &self.plan else { return };
        let verified: Vec<&SessionView> = plan
            .tmux
            .iter()
            .filter(|s| s.association == Some(Association::Tagged))
            .collect();
        if plan.tmux.is_empty() && plan.ghostty.is_empty() {
            self.message = Some((
                "No existing session. Use “New tmux session” to create one.".into(),
                false,
            ));
        } else if verified.len() == 1 && plan.ghostty.is_empty() {
            let s = verified[0].clone();
            match ghostmc_host::desktop::displayed_in(&s) {
                Some(t) => self.focus_terminal(t, cx),
                None => self.attach(s.key, AttachMode::Writable, cx),
            }
            return;
        } else {
            self.message = Some(("Several candidates: pick one below.".into(), false));
        }
        self.tab = Tab::Sessions;
        cx.notify();
    }

    fn open_path(&mut self, path: String, reveal: bool, cx: &mut Context<Self>) {
        self.run(
            if reveal {
                "Revealing"
            } else {
                "Opening editor"
            },
            cx,
            move |_, cfg| {
                let p = Path::new(&path);
                if reveal {
                    ghostmc_platform::open::reveal(p)
                } else {
                    ghostmc_platform::open::open_in_editor(&cfg.editor.command, p)
                }
                .map_err(|e| e.to_string())?;
                Ok(format!("Opened {path}"))
            },
        );
    }

    fn toggle_pin(&mut self, key: String, pinned: bool, cx: &mut Context<Self>) {
        self.run("Saving", cx, move |b, _| {
            b.call::<serde_json::Value>(Request::UpdateProject {
                key,
                name: None,
                notes: None,
                pinned: Some(!pinned),
                add_tags: vec![],
                remove_tags: vec![],
            })
            .map_err(err_text)?;
            Ok(if pinned {
                "Unpinned".into()
            } else {
                "Pinned".into()
            })
        });
    }

    fn show_diff(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(loc) = self.plan.as_ref().map(|p| p.location.location.id.0.clone()) else {
            return;
        };
        let b = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let p2 = path.clone();
            let d = cx
                .background_executor()
                .spawn(async move {
                    b.call::<Diff>(Request::GitDiff {
                        location_id: loc,
                        path: Some(p2),
                        staged: false,
                    })
                })
                .await;
            this.update(cx, |this, cx| {
                match d {
                    Ok(d) => this.diff = Some((path, d)),
                    Err(e) => this.message = Some((e.message, true)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // -------------------------------------------------------------- render

    fn selected_summary(&self) -> Option<&ProjectSummary> {
        let id = self.selected.as_ref()?;
        self.projects.iter().find(|p| &p.project.id.0 == id)
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let host = match &self.status {
            Some(s) if s.host_running => ("host running", theme::good()),
            Some(_) => ("direct mode (no host)", theme::warn()),
            None => ("connecting…", theme::muted()),
        };
        let attention = self
            .agents
            .iter()
            .filter(|a| a.state.needs_attention())
            .count();
        div()
            .flex()
            .flex_col()
            .w(px(270.))
            .h_full()
            .bg(theme::panel())
            .border_r_1()
            .border_color(theme::border())
            .child(
                div()
                    .px_4()
                    .pt_4()
                    .pb_2()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child("Ghost May Cry"),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .items_center()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(div().size_2().rounded_full().bg(host.1))
                            .child(host.0)
                            .when_some(self.status.as_ref(), |d, s| {
                                d.child(format!("· {}", s.device.name))
                            }),
                    ),
            )
            .when(attention > 0, |d| {
                d.child(
                    div()
                        .mx_3()
                        .mb_2()
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(theme::raised())
                        .text_sm()
                        .text_color(theme::warn())
                        .child(format!("⚠ {attention} agent(s) need attention")),
                )
            })
            .child(
                div()
                    .px_4()
                    .py_1()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(format!("PROJECTS ({})", self.projects.len())),
            )
            .child(
                div()
                    .id("projects")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_2()
                    .children(
                        self.projects
                            .iter()
                            .map(|p| self.render_project_row(p, cx))
                            .collect::<Vec<_>>(),
                    )
                    .when(self.projects.is_empty(), |d| {
                        d.child(div().p_3().text_sm().text_color(theme::muted()).child(
                            "No projects yet.\nAdd one with:\nghostmc project add NAME --path DIR",
                        ))
                    }),
            )
            .child(
                div()
                    .p_3()
                    .border_t_1()
                    .border_color(theme::border())
                    .text_xs()
                    .text_color(theme::muted())
                    .child("↑/↓ select · Enter open · 1-4 tabs · r refresh"),
            )
    }

    fn render_project_row(&self, p: &ProjectSummary, cx: &mut Context<Self>) -> Stateful<Div> {
        let id = p.project.id.0.clone();
        let is_sel = self.selected.as_deref() == Some(id.as_str());
        div()
            .id(SharedString::from(format!("p-{id}")))
            .px_2()
            .py_1p5()
            .mb_0p5()
            .rounded_md()
            .cursor_pointer()
            .when(is_sel, |d| d.bg(theme::selected()))
            .when(!is_sel, |d| d.hover(|s| s.bg(theme::hover())))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.select(id.clone(), cx)))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child(format!(
                                "{}{}",
                                if p.project.pinned { "★ " } else { "" },
                                p.project.name
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .when(p.attention > 0, |d| {
                                d.child(badge(format!("⚠ {}", p.attention), theme::warn()))
                            })
                            .when(p.live_sessions > 0, |d| {
                                d.child(badge(format!("{}", p.live_sessions), theme::accent()))
                            }),
                    ),
            )
            .when(!p.project.tags.is_empty(), |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(p.project.tags.join(" · ")),
                )
            })
    }

    fn render_main(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(summary) = self.selected_summary() else {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme::muted())
                .child("Select or add a project")
                .into_any_element();
        };
        let plan = self.plan.as_ref();
        let loc = plan.map(|p| &p.location);
        let path = loc.map(|l| l.location.path.clone());
        let git_label = loc.and_then(|l| l.git.as_ref()).map(|g| {
            format!(
                "{}{}",
                g.branch.as_deref().unwrap_or("detached"),
                if g.is_linked_worktree {
                    " · worktree"
                } else {
                    ""
                }
            )
        });
        let loc_id = loc.map(|l| l.location.id.0.clone());
        let key = summary.project.id.0.clone();
        let pinned = summary.project.pinned;

        div()
            .flex_1()
            .flex()
            .flex_col()
            .h_full()
            .min_w_0()
            .child(
                div()
                    .px_5()
                    .pt_4()
                    .pb_3()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(FontWeight::BOLD)
                                    .child(summary.project.name.clone()),
                            )
                            .when_some(git_label, |d, g| d.child(badge(g, theme::muted()))),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::muted())
                            .font_family(theme::mono())
                            .child(
                                path.clone()
                                    .unwrap_or_else(|| "no location on this device".into()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .mt_3()
                            .child(button(
                                "open",
                                "Open / Focus terminal",
                                true,
                                cx.listener(|this, _, _, cx| this.open_terminal(cx)),
                            ))
                            .when_some(loc_id.clone(), |d, l| {
                                d.child(button(
                                    "new",
                                    "New tmux session",
                                    false,
                                    cx.listener(move |this, _, _, cx| {
                                        this.create(l.clone(), CreateIntent::Reuse, cx)
                                    }),
                                ))
                            })
                            .when_some(path.clone(), |d, p| {
                                let p2 = p.clone();
                                d.child(button(
                                    "folder",
                                    "Open folder",
                                    false,
                                    cx.listener(move |this, _, _, cx| {
                                        this.open_path(p.clone(), true, cx)
                                    }),
                                ))
                                .child(button(
                                    "edit",
                                    "Open in editor",
                                    false,
                                    cx.listener(move |this, _, _, cx| {
                                        this.open_path(p2.clone(), false, cx)
                                    }),
                                ))
                            })
                            .child(button(
                                "pin",
                                if pinned { "Unpin" } else { "Pin" },
                                false,
                                cx.listener(move |this, _, _, cx| {
                                    this.toggle_pin(key.clone(), pinned, cx)
                                }),
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_1()
                    .px_4()
                    .pt_2()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(self.tab_button("Sessions", Tab::Sessions, cx))
                    .child(self.tab_button("Changes", Tab::Changes, cx))
                    .child(self.tab_button("Files", Tab::Files, cx))
                    .child(self.tab_button("Agents", Tab::Agents, cx)),
            )
            .child(
                div()
                    .id("tab-body")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_4()
                    .child(match self.tab {
                        Tab::Sessions => self.render_sessions(cx),
                        Tab::Changes => self.render_changes(cx),
                        Tab::Files => self.render_files(cx),
                        Tab::Agents => self.render_agents(Some(summary.project.name.as_str())),
                    }),
            )
            .into_any_element()
    }

    fn tab_button(
        &self,
        label: &'static str,
        tab: Tab,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.tab == tab;
        div()
            .id(label)
            .px_3()
            .py_1p5()
            .text_sm()
            .cursor_pointer()
            .border_b_2()
            .border_color(if active {
                theme::accent().into()
            } else {
                theme::transparent()
            })
            .text_color(if active {
                theme::text()
            } else {
                theme::muted()
            })
            .hover(|s| s.text_color(theme::text()))
            .on_click(cx.listener(move |this, _, _, cx| this.set_tab(tab, cx)))
            .child(label)
    }

    fn render_sessions(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(e) = &self.plan_error {
            return div()
                .text_color(theme::bad())
                .child(e.clone())
                .into_any_element();
        }
        let Some(plan) = &self.plan else {
            return div()
                .text_color(theme::muted())
                .child("Loading…")
                .into_any_element();
        };
        let caps = plan.capabilities.clone();
        let can_focus = caps.as_ref().is_some_and(|c| c.focus_terminal);
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(section_title(
                "tmux sessions",
                "attachable here and from other devices",
            ))
            .when(plan.tmux.is_empty(), |d| {
                d.child(
                    div()
                        .text_sm()
                        .text_color(theme::muted())
                        .child("No tmux session matches this location."),
                )
            })
            .children(
                plan.tmux
                    .iter()
                    .map(|s| self.render_session_card(s, can_focus, cx))
                    .collect::<Vec<_>>(),
            )
            .when(!plan.ghostty.is_empty(), |d| {
                d.child(section_title(
                    "Ghostty terminals not in tmux",
                    "focus only — cannot be attached from another device",
                ))
                .children(plan.ghostty.iter().map(|g| {
                    let id = g.terminal.terminal_id.clone();
                    card().child(
                        div()
                            .flex()
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .child(div().text_sm().child(g.terminal.title.clone()))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme::muted())
                                            .font_family(theme::mono())
                                            .child(
                                                g.terminal
                                                    .working_directory
                                                    .clone()
                                                    .unwrap_or_default(),
                                            ),
                                    ),
                            )
                            .child(button(
                                SharedString::from(format!("gf-{id}")),
                                "Focus",
                                true,
                                cx.listener(move |this, _, _, cx| {
                                    this.focus_terminal(id.clone(), cx)
                                }),
                            )),
                    )
                }))
            })
            .when_some(plan.ghostty_error.clone(), |d, e| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(theme::warn())
                        .child(format!("Ghostty not inspected: {e}")),
                )
            })
            .when_some(caps, |d, c| {
                d.children(c.notes.into_iter().map(|n| {
                    div()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(format!("ⓘ {n}"))
                }))
            })
            .children(plan.notes.iter().map(|n| {
                div()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(format!("ⓘ {n}"))
            }))
            .into_any_element()
    }

    fn render_session_card(&self, s: &SessionView, can_focus: bool, cx: &mut Context<Self>) -> Div {
        let (assoc, color) = match s.association {
            Some(Association::Tagged) => ("ghostmc session", theme::good()),
            Some(Association::PathExact) => ("cwd match", theme::warn()),
            Some(Association::PathDescendant) => ("cwd in subdirectory", theme::warn()),
            None => ("", theme::muted()),
        };
        let shown_in = ghostmc_host::desktop::displayed_in(s);
        let socket = Path::new(&s.r#ref.socket_path)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        let key = s.key.clone();
        let key2 = s.key.clone();
        card()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(s.name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .font_family(theme::mono())
                                    .child(format!("{socket}:{}", s.r#ref.session_id)),
                            )
                            .child(badge(assoc.into(), color))
                            .child(badge(
                                if s.clients.is_empty() {
                                    "detached".into()
                                } else {
                                    format!("{} client(s)", s.clients.len())
                                },
                                theme::muted(),
                            ))
                            .when(shown_in.is_some(), |d| {
                                d.child(badge("in Ghostty".into(), theme::accent()))
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .when_some(shown_in.clone().filter(|_| can_focus), |d, t| {
                                d.child(button(
                                    SharedString::from(format!("f-{}", s.key)),
                                    "Focus",
                                    true,
                                    cx.listener(move |this, _, _, cx| {
                                        this.focus_terminal(t.clone(), cx)
                                    }),
                                ))
                            })
                            .child(button(
                                SharedString::from(format!("a-{}", s.key)),
                                "Attach",
                                shown_in.is_none() || !can_focus,
                                cx.listener(move |this, _, _, cx| {
                                    this.attach(key.clone(), AttachMode::Writable, cx)
                                }),
                            ))
                            .child(button(
                                SharedString::from(format!("o-{}", s.key)),
                                "Observe",
                                false,
                                cx.listener(move |this, _, _, cx| {
                                    this.attach(key2.clone(), AttachMode::Observe, cx)
                                }),
                            )),
                    ),
            )
            .children(s.panes.iter().map(|p| {
                div()
                    .flex()
                    .gap_3()
                    .text_xs()
                    .font_family(theme::mono())
                    .text_color(if p.active {
                        theme::text()
                    } else {
                        theme::muted()
                    })
                    .child(p.id.clone())
                    .child(p.command.clone())
                    .child(div().text_color(theme::muted()).child(p.cwd.clone()))
            }))
            .children(s.agents.iter().map(|a| {
                div()
                    .flex()
                    .gap_2()
                    .items_center()
                    .text_xs()
                    .child(state_dot(a.state))
                    .child(format!(
                        "{} — {}",
                        agent_label(a.run.agent),
                        state_text(a.state)
                    ))
                    .when_some(a.run.last_message.clone(), |d, m| {
                        d.child(div().text_color(theme::muted()).child(m))
                    })
            }))
    }

    fn render_changes(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(git) = &self.git else {
            return div()
                .text_color(theme::muted())
                .child("Loading…")
                .into_any_element();
        };
        let st = match git {
            Ok(s) => s,
            Err(e) => {
                return div()
                    .text_color(theme::muted())
                    .child(e.clone())
                    .into_any_element();
            }
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section_title(
                &format!(
                    "{} changed file(s) on {}",
                    st.files.len(),
                    st.branch.as_deref().unwrap_or("detached HEAD")
                ),
                "repository-wide; not attributed to any agent",
            ))
            .children(st.files.iter().map(|f| {
                let path = f.path.clone();
                let color = match f.kind {
                    ChangeKind::Added | ChangeKind::Untracked => theme::good(),
                    ChangeKind::Deleted | ChangeKind::Conflicted => theme::bad(),
                    _ => theme::warn(),
                };
                let sel = self.diff.as_ref().is_some_and(|(p, _)| p == &f.path);
                div()
                    .id(SharedString::from(format!("c-{}", f.path)))
                    .flex()
                    .gap_3()
                    .px_2()
                    .py_0p5()
                    .rounded_sm()
                    .cursor_pointer()
                    .text_sm()
                    .font_family(theme::mono())
                    .when(sel, |d| d.bg(theme::selected()))
                    .hover(|s| s.bg(theme::hover()))
                    .on_click(cx.listener(move |this, _, _, cx| this.show_diff(path.clone(), cx)))
                    .child(div().w(px(24.)).text_color(color).child(f.xy.clone()))
                    .child(f.path.clone())
            }))
            .when_some(self.diff.as_ref(), |d, (path, diff)| {
                d.child(render_diff(path, diff))
            })
            .into_any_element()
    }

    fn render_files(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(listing) = &self.listing else {
            return div()
                .text_color(theme::muted())
                .child("Loading…")
                .into_any_element();
        };
        let root = self
            .plan
            .as_ref()
            .map(|p| p.location.location.path.clone())
            .unwrap_or_default();
        let l = match listing {
            Ok(l) => l,
            Err(e) => {
                return div()
                    .text_color(theme::bad())
                    .child(e.clone())
                    .into_any_element();
            }
        };
        let parent = Path::new(&self.dir)
            .parent()
            .map(|p| p.to_string_lossy().into_owned());
        div()
            .flex()
            .flex_col()
            .gap_0p5()
            .child(section_title(
                if self.dir.is_empty() { "/" } else { &self.dir },
                "hidden and .gitignored entries are not shown",
            ))
            .when_some(parent.filter(|_| !self.dir.is_empty()), |d, up| {
                d.child(file_row(
                    "..",
                    "↩",
                    "up".into(),
                    cx.listener(move |this, _, _, cx| {
                        this.dir = up.clone();
                        this.load_selected(cx);
                    }),
                ))
            })
            .children(l.entries.iter().map(|e| {
                let rel = e.rel_path.clone();
                let abs = format!("{root}/{rel}");
                let is_dir = e.is_dir;
                file_row(
                    &e.name,
                    if e.is_dir { "▸" } else { " " },
                    SharedString::from(format!("f-{rel}")),
                    cx.listener(move |this, _, _, cx| {
                        if is_dir {
                            this.dir = rel.clone();
                            this.load_selected(cx);
                        } else {
                            this.open_path(abs.clone(), false, cx);
                        }
                    }),
                )
            }))
            .when(l.truncated, |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(theme::warn())
                        .child("listing truncated"),
                )
            })
            .into_any_element()
    }

    fn render_agents(&self, project: Option<&str>) -> AnyElement {
        let list: Vec<&AgentView> = self
            .agents
            .iter()
            .filter(|a| project.is_none_or(|p| a.project_name.as_deref() == Some(p)))
            .collect();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section_title(
                "Agent runs",
                "from Claude Code / Codex hooks; install with `ghostmc hook install`",
            ))
            .when(list.is_empty(), |d| {
                d.child(
                    div()
                        .text_sm()
                        .text_color(theme::muted())
                        .child("No agent activity for this project."),
                )
            })
            .children(list.into_iter().map(|a| {
                card()
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(state_dot(a.state))
                                    .child(div().text_sm().child(format!(
                                        "{} — {}",
                                        agent_label(a.run.agent),
                                        state_text(a.state)
                                    )))
                                    .child(badge(
                                        attribution_label(a.run.attribution).into(),
                                        theme::muted(),
                                    )),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .child(ago(a.run.last_event_at)),
                            ),
                    )
                    .when_some(a.run.last_message.clone(), |d, m| {
                        d.child(div().text_xs().text_color(theme::muted()).child(m))
                    })
                    .when_some(a.pane_id.clone(), |d, p| {
                        d.child(
                            div()
                                .text_xs()
                                .font_family(theme::mono())
                                .text_color(theme::muted())
                                .child(format!("pane {p}")),
                        )
                    })
            }))
            .into_any_element()
    }
}

impl Focusable for Root {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Root {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus)
            .key_context("ghostmc")
            .on_action(cx.listener(|this, _: &NextProject, _, cx| this.step(1, cx)))
            .on_action(cx.listener(|this, _: &PrevProject, _, cx| this.step(-1, cx)))
            .on_action(cx.listener(|this, _: &OpenTerminal, _, cx| this.open_terminal(cx)))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.refresh(cx)))
            .on_action(cx.listener(|this, _: &ShowSessions, _, cx| this.set_tab(Tab::Sessions, cx)))
            .on_action(cx.listener(|this, _: &ShowChanges, _, cx| this.set_tab(Tab::Changes, cx)))
            .on_action(cx.listener(|this, _: &ShowFiles, _, cx| this.set_tab(Tab::Files, cx)))
            .on_action(cx.listener(|this, _: &ShowAgents, _, cx| this.set_tab(Tab::Agents, cx)))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::bg())
            .text_color(theme::text())
            .font_family(if cfg!(target_os = "macos") {
                ".SystemUIFont"
            } else {
                "Inter"
            })
            .child(
                div()
                    .flex_1()
                    .flex()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    .child(self.render_main(cx)),
            )
            .child(
                div()
                    .h(px(26.))
                    .px_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .bg(theme::panel())
                    .border_t_1()
                    .border_color(theme::border())
                    .text_xs()
                    .child(match &self.message {
                        Some((m, true)) => div().text_color(theme::bad()).child(m.clone()),
                        Some((m, false)) => div().text_color(theme::muted()).child(m.clone()),
                        None => div().text_color(theme::muted()).child("Ready"),
                    })
                    .child(div().text_color(theme::muted()).child(if self.busy {
                        "working…"
                    } else {
                        ""
                    })),
            )
    }
}

// ------------------------------------------------------------- components

fn badge(text: String, color: gpui::Rgba) -> impl IntoElement {
    div()
        .px_1p5()
        .rounded_sm()
        .border_1()
        .border_color(color)
        .text_xs()
        .text_color(color)
        .child(text)
}

fn button(
    id: impl Into<gpui::ElementId>,
    label: &str,
    primary: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .text_sm()
        .cursor_pointer()
        .when(primary, |d| d.bg(theme::accent()).text_color(theme::bg()))
        .when(!primary, |d| {
            d.bg(theme::raised())
                .border_1()
                .border_color(theme::border())
        })
        .hover(|s| s.opacity(0.85))
        .on_click(on_click)
        .child(label.to_owned())
}

fn card() -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .p_3()
        .rounded_lg()
        .bg(theme::panel())
        .border_1()
        .border_color(theme::border())
}

fn section_title(title: &str, sub: &str) -> impl IntoElement {
    div()
        .flex()
        .items_baseline()
        .gap_2()
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.to_owned()),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme::muted())
                .child(sub.to_owned()),
        )
}

fn file_row(
    name: &str,
    icon: &str,
    id: SharedString,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .gap_2()
        .px_2()
        .py_0p5()
        .rounded_sm()
        .cursor_pointer()
        .text_sm()
        .hover(|s| s.bg(theme::hover()))
        .on_click(on_click)
        .child(
            div()
                .w(px(12.))
                .text_color(theme::muted())
                .child(icon.to_owned()),
        )
        .child(name.to_owned())
}

fn render_diff(path: &str, d: &Diff) -> impl IntoElement {
    let lines: Vec<&str> = d.text.lines().collect();
    let shown = lines.len().min(MAX_DIFF_LINES);
    div()
        .mt_3()
        .p_3()
        .rounded_lg()
        .bg(theme::panel())
        .border_1()
        .border_color(theme::border())
        .font_family(theme::mono())
        .text_xs()
        .child(
            div()
                .text_color(theme::muted())
                .mb_2()
                .child(path.to_owned()),
        )
        .children(lines[..shown].iter().map(|l| {
            let color = if l.starts_with("+++") || l.starts_with("---") {
                theme::muted()
            } else if l.starts_with('+') {
                theme::good()
            } else if l.starts_with('-') {
                theme::bad()
            } else if l.starts_with("@@") {
                theme::accent()
            } else {
                theme::text()
            };
            div().text_color(color).child(if l.is_empty() {
                " ".to_owned()
            } else {
                (*l).to_owned()
            })
        }))
        .when(lines.len() > shown || d.truncated, |el| {
            el.child(div().text_color(theme::warn()).child(format!(
                "… {} more lines not shown",
                lines.len().saturating_sub(shown)
            )))
        })
}

fn state_text(s: AgentState) -> &'static str {
    match s {
        AgentState::Running => "running",
        AgentState::NeedsApproval => "needs approval",
        AgentState::Idle => "idle",
        AgentState::TurnEnded => "turn ended",
        AgentState::Failed => "failed",
        AgentState::Ended => "ended",
        AgentState::Disconnected => "disconnected",
        AgentState::Unknown => "unknown (stale)",
    }
}

fn state_dot(s: AgentState) -> impl IntoElement {
    let c = match s {
        AgentState::Running => theme::accent(),
        AgentState::NeedsApproval | AgentState::Failed => theme::warn(),
        AgentState::TurnEnded | AgentState::Idle => theme::good(),
        _ => theme::muted(),
    };
    div().size_2().rounded_full().bg(c)
}

fn ago(ts: i64) -> String {
    let d = (ghostmc_core::now() - ts).max(0);
    match d {
        0..60 => format!("{d}s ago"),
        60..3600 => format!("{}m ago", d / 60),
        3600..86400 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}
