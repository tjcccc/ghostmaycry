//! Desktop-side workspace list: projects and open Ghostty windows grouped
//! into the menu-bar app's sidebar.
//!
//! A *saved* workspace is a registry project; its rows are its local
//! locations, each joined by the open Ghostty tabs whose directory lies in
//! it. Tabs that belong to no saved workspace form a *live* workspace per
//! Ghostty window, named after the window's first tab; saving it (pin or
//! rename) turns it into a project. Grouping a tab by directory is display
//! only: a row's terminal is the concrete Ghostty terminal listed, and an
//! agent is tied to a tab by exact tty (or tmux pane) evidence, with a single
//! cwd match shown as unverified.
//!
//! Pure: callers pass the Ghostty listing and a git lookup, so this runs in
//! tests without Ghostty or git.

use ghostmc_core::agent::AgentState;
use ghostmc_core::model::GitIdentity;
use ghostmc_core::paths::{self, PathRelation};
use ghostmc_platform::ghostty::GhosttyTerminal;
use ghostmc_protocol::{AgentView, ProjectSummary, SessionView};
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WorkspaceKey {
    /// A registry project (`prj_…`).
    Project(String),
    /// An unsaved Ghostty window, by its id in the running Ghostty.
    Window(String),
}

#[derive(Debug, Clone)]
pub struct Workspace {
    pub key: WorkspaceKey,
    pub name: String,
    pub pinned: bool,
    pub rows: Vec<Row>,
}

impl Workspace {
    pub fn is_saved(&self) -> bool {
        matches!(self.key, WorkspaceKey::Project(_))
    }

    pub fn agents(&self) -> impl Iterator<Item = (&Row, &RowAgent)> {
        self.rows
            .iter()
            .flat_map(|r| r.agents.iter().map(move |a| (r, a)))
    }
}

/// One line under a workspace: an open Ghostty tab, or a saved location with
/// no open tab.
#[derive(Debug, Clone)]
pub struct Row {
    /// Repository root (or plain directory) the row stands for.
    pub path: String,
    pub branch: Option<String>,
    /// Ghostty terminal to focus; `None` for a location with no open tab.
    pub terminal_id: Option<String>,
    pub window_id: Option<String>,
    pub location_id: Option<String>,
    /// Live tmux session shown in this tab, or the location's session.
    pub session_key: Option<String>,
    pub agents: Vec<RowAgent>,
}

#[derive(Debug, Clone)]
pub struct RowAgent {
    pub view: AgentView,
    /// Tied by tty or tmux pane, not by directory alone.
    pub verified: bool,
}

/// Everything the grouping needs, gathered by the caller.
pub struct Inputs<'a> {
    pub projects: &'a [ProjectSummary],
    pub sessions: &'a [SessionView],
    pub agents: &'a [AgentView],
    pub terminals: &'a [GhosttyTerminal],
    /// Live windows the user removed from the list (until Ghostty forgets
    /// them).
    pub dismissed: &'a HashSet<String>,
}

/// States that can still be shown against a tab; older runs are history.
fn is_current(a: &AgentView) -> bool {
    !matches!(a.state, AgentState::Ended | AgentState::Disconnected)
}

struct Tab<'a> {
    terminal: &'a GhosttyTerminal,
    path: String,
    git: Option<GitIdentity>,
    session: Option<&'a SessionView>,
}

pub fn build(inp: &Inputs, git_of: &dyn Fn(&str) -> Option<GitIdentity>) -> Vec<Workspace> {
    let tabs = collect_tabs(inp, git_of);
    let agents: Vec<&AgentView> = inp.agents.iter().filter(|a| is_current(a)).collect();
    let mut claimed_agents: HashSet<String> = HashSet::new();
    let mut tab_agents: Vec<Vec<RowAgent>> = tabs
        .iter()
        .map(|t| {
            let v: Vec<RowAgent> = agents
                .iter()
                .filter(|a| verified_in_tab(a, t))
                .map(|a| RowAgent {
                    view: (*a).clone(),
                    verified: true,
                })
                .collect();
            claimed_agents.extend(v.iter().map(|a| a.view.run.id.0.clone()));
            v
        })
        .collect();
    // An unverified agent goes to a tab only when its directory names
    // exactly one tab and that tab shows no verified agent. An agent whose
    // tty matched no tab runs in another terminal, unless Ghostty reports no
    // ttys at all (1.3.1 has no `tty` property) and the tty proves nothing.
    let tabs_report_tty = tabs.iter().any(|t| t.terminal.tty.is_some());
    for a in &agents {
        if claimed_agents.contains(&a.run.id.0)
            || a.session_key.is_some()
            || (a.run.tty.is_some() && tabs_report_tty)
        {
            continue;
        }
        let Some(root) = a.run.cwd.as_deref().map(|c| root_of(c, git_of)) else {
            continue;
        };
        let hits: Vec<usize> = (0..tabs.len()).filter(|&i| tabs[i].path == root).collect();
        if let [i] = hits[..]
            && tab_agents[i].is_empty()
        {
            tab_agents[i].push(RowAgent {
                view: (*a).clone(),
                verified: false,
            });
            claimed_agents.insert(a.run.id.0.clone());
        }
    }

    let mut out = Vec::new();
    let mut used = vec![false; tabs.len()];
    for p in inp.projects {
        let locations: Vec<_> = p.locations.iter().filter(|l| l.is_local).collect();
        if locations.is_empty() {
            continue;
        }
        let mut rows = Vec::new();
        for l in &locations {
            let mut any = false;
            for (i, t) in tabs.iter().enumerate() {
                if used[i]
                    || best_location(&t.path, &locations).map(|b| b.location.id.0.as_str())
                        != Some(l.location.id.0.as_str())
                {
                    continue;
                }
                used[i] = true;
                any = true;
                rows.push(tab_row(t, Some(l.location.id.0.clone()), &tab_agents[i]));
            }
            if !any {
                let session = inp
                    .sessions
                    .iter()
                    .find(|s| s.location_id.as_deref() == Some(l.location.id.0.as_str()));
                let agents = agents
                    .iter()
                    .filter(|a| !claimed_agents.contains(&a.run.id.0))
                    .filter_map(|a| {
                        let verified = session
                            .is_some_and(|s| a.session_key.as_deref() == Some(s.key.as_str()));
                        let by_cwd = a
                            .run
                            .cwd
                            .as_deref()
                            .is_some_and(|c| root_of(c, git_of) == l.location.path);
                        (verified || by_cwd).then(|| RowAgent {
                            view: (*a).clone(),
                            verified,
                        })
                    })
                    .collect::<Vec<_>>();
                claimed_agents.extend(agents.iter().map(|a| a.view.run.id.0.clone()));
                rows.push(Row {
                    path: l.location.path.clone(),
                    // Project listings skip git detection; ask like for tabs.
                    branch: l
                        .git
                        .clone()
                        .or_else(|| git_of(&l.location.path))
                        .and_then(|g| g.branch),
                    terminal_id: None,
                    window_id: None,
                    location_id: Some(l.location.id.0.clone()),
                    session_key: session.map(|s| s.key.clone()),
                    agents,
                });
            }
        }
        out.push(Workspace {
            key: WorkspaceKey::Project(p.project.id.0.clone()),
            name: p.project.name.clone(),
            pinned: p.project.pinned,
            rows,
        });
    }

    let mut windows: Vec<&str> = Vec::new();
    for (i, t) in tabs.iter().enumerate() {
        if !used[i] && !windows.contains(&t.terminal.window_id.as_str()) {
            windows.push(&t.terminal.window_id);
        }
    }
    for w in windows {
        if inp.dismissed.contains(w) {
            continue;
        }
        let rows: Vec<Row> = tabs
            .iter()
            .enumerate()
            .filter(|(i, t)| !used[*i] && t.terminal.window_id == w)
            .map(|(i, t)| tab_row(t, None, &tab_agents[i]))
            .collect();
        out.push(Workspace {
            key: WorkspaceKey::Window(w.to_owned()),
            name: default_name(&rows[0].path),
            pinned: false,
            rows,
        });
    }

    // Pinned first, then whatever has open tabs, then the rest; stable
    // within each group (registry order, then Ghostty window order).
    out.sort_by_key(|w| (!w.pinned, !w.rows.iter().any(|r| r.terminal_id.is_some())));
    out
}

/// Name for a live workspace: the last path component of its first tab.
pub fn default_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_owned())
}

/// A project name not yet taken (case-insensitive, like the registry):
/// `name`, then `name 2`, `name 3`, …
pub fn unique_name(name: &str, taken: &[&str]) -> String {
    let free = |n: &str| !taken.iter().any(|t| t.eq_ignore_ascii_case(n));
    if free(name) {
        return name.to_owned();
    }
    (2..)
        .map(|i| format!("{name} {i}"))
        .find(|n| free(n))
        .unwrap_or_else(|| name.to_owned())
}

fn root_of(dir: &str, git_of: &dyn Fn(&str) -> Option<GitIdentity>) -> String {
    git_of(dir)
        .map(|g| g.root)
        .unwrap_or_else(|| dir.to_owned())
}

fn best_location<'l>(
    path: &str,
    locations: &[&'l ghostmc_protocol::LocationView],
) -> Option<&'l ghostmc_protocol::LocationView> {
    locations
        .iter()
        .filter(|l| {
            matches!(
                paths::relation(Path::new(&l.location.path), Path::new(path)),
                PathRelation::Exact | PathRelation::Descendant(_)
            )
        })
        .max_by_key(|l| l.location.path.len())
        .copied()
}

/// One entry per Ghostty tab (split panes collapse into their tab), with the
/// directory it works in: the tmux session's location or active pane when
/// the tab shows tmux, else Ghostty's reported cwd.
fn collect_tabs<'a>(
    inp: &Inputs<'a>,
    git_of: &dyn Fn(&str) -> Option<GitIdentity>,
) -> Vec<Tab<'a>> {
    let mut seen: HashSet<(&str, &str)> = HashSet::new();
    let mut out = Vec::new();
    for t in inp.terminals {
        if !seen.insert((t.window_id.as_str(), t.tab_id.as_str())) {
            continue;
        }
        let session = t.tty.as_deref().and_then(|tty| {
            inp.sessions
                .iter()
                .find(|s| s.clients.iter().any(|c| c.tty == tty))
        });
        let dir = session
            .and_then(|s| {
                s.location_id
                    .as_deref()
                    .and_then(|l| location_path(inp.projects, l))
                    .or_else(|| s.active_pane().map(|p| p.cwd.clone()))
            })
            .or_else(|| t.working_directory.clone());
        let Some(dir) = dir else { continue };
        let dir = paths::normalize(Path::new(&dir))
            .to_string_lossy()
            .into_owned();
        let git = git_of(&dir);
        let path = git.as_ref().map(|g| g.root.clone()).unwrap_or(dir);
        out.push(Tab {
            terminal: t,
            path,
            git,
            session,
        });
    }
    out
}

fn location_path(projects: &[ProjectSummary], location_id: &str) -> Option<String> {
    projects
        .iter()
        .flat_map(|p| &p.locations)
        .find(|l| l.location.id.0 == location_id)
        .map(|l| l.location.path.clone())
}

fn verified_in_tab(a: &AgentView, t: &Tab) -> bool {
    match t.session {
        Some(s) => a.session_key.as_deref() == Some(s.key.as_str()),
        None => a.run.tty.is_some() && a.run.tty == t.terminal.tty,
    }
}

fn tab_row(t: &Tab, location_id: Option<String>, agents: &[RowAgent]) -> Row {
    Row {
        path: t.path.clone(),
        branch: t.git.as_ref().and_then(|g| g.branch.clone()),
        terminal_id: Some(t.terminal.terminal_id.clone()),
        window_id: Some(t.terminal.window_id.clone()),
        location_id,
        session_key: t.session.map(|s| s.key.clone()),
        agents: agents.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostmc_core::agent::{AgentKind, AgentRun, Attribution};
    use ghostmc_core::ids::*;
    use ghostmc_core::model::{Project, ProjectLocation};
    use ghostmc_protocol::{ClientView, LocationView, PaneView, SessionRef};

    fn term(win: &str, tab: &str, id: &str, tty: &str, cwd: &str) -> GhosttyTerminal {
        GhosttyTerminal {
            window_id: win.into(),
            tab_id: tab.into(),
            tab_index: 1,
            tab_selected: false,
            terminal_id: id.into(),
            title: "t".into(),
            working_directory: Some(cwd.into()),
            tty: Some(tty.into()),
            pid: None,
        }
    }

    fn project(id: &str, name: &str, pinned: bool, paths: &[&str]) -> ProjectSummary {
        ProjectSummary {
            project: Project {
                id: ProjectId(id.into()),
                name: name.into(),
                notes: None,
                pinned,
                tags: vec![],
                created_at: 0,
                updated_at: 0,
            },
            locations: paths
                .iter()
                .enumerate()
                .map(|(i, p)| LocationView {
                    location: ProjectLocation {
                        id: LocationId(format!("loc_{id}_{i}")),
                        project_id: ProjectId(id.into()),
                        device_id: DeviceId("dev".into()),
                        path: (*p).into(),
                        default_command: None,
                        created_at: 0,
                    },
                    device_name: "here".into(),
                    is_local: true,
                    git: None,
                    exists: Some(true),
                })
                .collect(),
            live_sessions: 0,
            attention: 0,
        }
    }

    fn agent(id: &str, cwd: &str, tty: Option<&str>, state: AgentState) -> AgentView {
        AgentView {
            run: AgentRun {
                id: AgentRunId(id.into()),
                agent: AgentKind::Claude,
                agent_session_id: id.into(),
                cwd: Some(cwd.into()),
                tmux_socket: None,
                tmux_server_pid: None,
                tmux_pane: None,
                token: None,
                tty: tty.map(Into::into),
                tty_pid: None,
                attribution: Attribution::CwdOnly,
                state,
                state_since: 0,
                last_event_at: 0,
                last_hook_event: "Stop".into(),
                last_message: None,
            },
            state,
            project_name: None,
            session_key: None,
            pane_id: None,
            brief: None,
        }
    }

    /// Paths under `/nonexistent-gmc` so `normalize` leaves them alone; the
    /// repo root is the first two components below it.
    fn git(dir: &str) -> Option<GitIdentity> {
        let rest = dir.strip_prefix("/nonexistent-gmc/")?;
        let repo = rest.split('/').next()?;
        Some(GitIdentity {
            root: format!("/nonexistent-gmc/{repo}"),
            common_dir: String::new(),
            is_linked_worktree: false,
            branch: Some("dev".into()),
            head: None,
        })
    }

    const R: &str = "/nonexistent-gmc";

    fn run(inp: Inputs) -> Vec<Workspace> {
        build(&inp, &git)
    }

    #[test]
    fn live_window_groups_its_tabs_and_takes_first_tab_name() {
        let terms = vec![
            term(
                "W1",
                "T1",
                "a",
                "/dev/ttys001",
                &format!("{R}/ghostmaycry/src"),
            ),
            term("W1", "T2", "b", "/dev/ttys002", &format!("{R}/jack-ai-lab")),
            term("W2", "T3", "c", "/dev/ttys003", &format!("{R}/qchub")),
        ];
        let ws = run(Inputs {
            projects: &[],
            sessions: &[],
            agents: &[],
            terminals: &terms,
            dismissed: &HashSet::new(),
        });
        assert_eq!(ws.len(), 2);
        assert_eq!(ws[0].name, "ghostmaycry");
        assert_eq!(ws[0].key, WorkspaceKey::Window("W1".into()));
        let paths: Vec<_> = ws[0].rows.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(
            paths,
            [format!("{R}/ghostmaycry"), format!("{R}/jack-ai-lab")]
        );
        assert_eq!(ws[0].rows[0].branch.as_deref(), Some("dev"));
        assert_eq!(ws[1].name, "qchub");
    }

    #[test]
    fn saved_project_claims_tabs_and_keeps_dormant_locations() {
        let projects = vec![project(
            "p1",
            "Marifold",
            false,
            &[&format!("{R}/marifold"), &format!("{R}/marifold-docs")],
        )];
        let terms = vec![
            term(
                "W1",
                "T1",
                "a",
                "/dev/ttys001",
                &format!("{R}/marifold/deep/dir"),
            ),
            term("W1", "T2", "b", "/dev/ttys002", &format!("{R}/other")),
        ];
        let ws = run(Inputs {
            projects: &projects,
            sessions: &[],
            agents: &[],
            terminals: &terms,
            dismissed: &HashSet::new(),
        });
        assert_eq!(ws[0].key, WorkspaceKey::Project("p1".into()));
        assert_eq!(ws[0].rows[0].terminal_id.as_deref(), Some("a"));
        assert_eq!(ws[0].rows[1].terminal_id, None, "dormant location row");
        assert_eq!(ws[0].rows[1].branch.as_deref(), Some("dev"));
        // The remaining tab of the same window forms its own live workspace.
        assert_eq!(ws[1].key, WorkspaceKey::Window("W1".into()));
        assert_eq!(ws[1].rows.len(), 1);
        assert_eq!(ws[1].name, "other");
    }

    #[test]
    fn agent_by_tty_is_verified_and_cwd_only_needs_a_unique_tab() {
        let terms = vec![
            term("W1", "T1", "a", "/dev/ttys001", &format!("{R}/app")),
            term("W1", "T2", "b", "/dev/ttys002", &format!("{R}/app")),
            term("W2", "T3", "c", "/dev/ttys003", &format!("{R}/solo")),
        ];
        let agents = vec![
            agent(
                "by-tty",
                &format!("{R}/app"),
                Some("/dev/ttys002"),
                AgentState::Running,
            ),
            // Two tabs share this directory: not placed.
            agent("ambiguous", &format!("{R}/app"), None, AgentState::Idle),
            agent("cwd", &format!("{R}/solo/src"), None, AgentState::TurnEnded),
            agent(
                "gone",
                &format!("{R}/solo"),
                Some("/dev/ttys003"),
                AgentState::Ended,
            ),
        ];
        let ws = run(Inputs {
            projects: &[],
            sessions: &[],
            agents: &agents,
            terminals: &terms,
            dismissed: &HashSet::new(),
        });
        let ids = |w: &Workspace| -> Vec<(String, bool)> {
            w.agents()
                .map(|(_, a)| (a.view.run.id.0.clone(), a.verified))
                .collect()
        };
        assert_eq!(ws[0].rows[0].agents.len(), 0);
        assert_eq!(ids(&ws[0]), [("by-tty".to_owned(), true)]);
        assert_eq!(ws[0].rows[1].terminal_id.as_deref(), Some("b"));
        assert_eq!(ids(&ws[1]), [("cwd".to_owned(), false)]);
    }

    #[test]
    fn agent_tty_rules_out_folder_match_only_when_tabs_report_ttys() {
        let agents = vec![agent(
            "elsewhere",
            &format!("{R}/solo"),
            Some("/dev/ttys009"),
            AgentState::Running,
        )];
        let with_tty = vec![term("W1", "T1", "a", "/dev/ttys001", &format!("{R}/solo"))];
        let mut without_tty = with_tty.clone();
        without_tty[0].tty = None;
        let placed = |terms: &[GhosttyTerminal]| {
            run(Inputs {
                projects: &[],
                sessions: &[],
                agents: &agents,
                terminals: terms,
                dismissed: &HashSet::new(),
            })[0]
                .agents()
                .count()
        };
        assert_eq!(placed(&with_tty), 0, "its tty names another terminal");
        assert_eq!(placed(&without_tty), 1, "Ghostty gave no ttys to compare");
    }

    #[test]
    fn tmux_tab_uses_session_location_not_ghostty_cwd() {
        let projects = vec![project("p1", "App", false, &[&format!("{R}/app")])];
        let session = SessionView {
            key: "tmux:1:1:$1:/s".into(),
            r#ref: SessionRef {
                socket_path: "/s".into(),
                server_pid: 1,
                server_start: 1,
                session_id: "$1".into(),
            },
            name: "gmc-app".into(),
            created: 0,
            clients: vec![ClientView {
                tty: "/dev/ttys009".into(),
                readonly: false,
                width: 80,
                height: 24,
                ghostty_terminal_id: None,
            }],
            panes: vec![PaneView {
                id: "%1".into(),
                window_id: "@1".into(),
                window_name: "w".into(),
                active: true,
                cwd: "/elsewhere".into(),
                command: "zsh".into(),
            }],
            managed: true,
            project_id: None,
            project_name: None,
            location_id: Some("loc_p1_0".into()),
            association: None,
            agents: vec![],
            tmux_binary: "tmux".into(),
        };
        let mut a = agent("in-tmux", "/elsewhere", None, AgentState::NeedsApproval);
        a.session_key = Some(session.key.clone());
        let terms = vec![term("W1", "T1", "a", "/dev/ttys009", "/Users/me")];
        let ws = run(Inputs {
            projects: &projects,
            sessions: std::slice::from_ref(&session),
            agents: &[a],
            terminals: &terms,
            dismissed: &HashSet::new(),
        });
        assert_eq!(ws.len(), 1);
        let row = &ws[0].rows[0];
        assert_eq!(row.terminal_id.as_deref(), Some("a"));
        assert_eq!(row.session_key.as_deref(), Some("tmux:1:1:$1:/s"));
        assert!(row.agents[0].verified);
    }

    #[test]
    fn ordering_and_dismissal() {
        let projects = vec![
            project("p1", "Dormant", false, &[&format!("{R}/dormant")]),
            project("p2", "Pinned", true, &[&format!("{R}/pinned")]),
        ];
        let terms = vec![
            term("W1", "T1", "a", "/dev/ttys001", &format!("{R}/live")),
            term("W2", "T2", "b", "/dev/ttys002", &format!("{R}/hidden")),
        ];
        let dismissed: HashSet<String> = ["W2".to_owned()].into();
        let ws = run(Inputs {
            projects: &projects,
            sessions: &[],
            agents: &[],
            terminals: &terms,
            dismissed: &dismissed,
        });
        let names: Vec<_> = ws.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["Pinned", "live", "Dormant"]);
    }

    #[test]
    fn unique_names() {
        assert_eq!(unique_name("app", &["App", "app 2"]), "app 3");
        assert_eq!(unique_name("app", &["other"]), "app");
    }
}
