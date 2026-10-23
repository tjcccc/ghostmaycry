//! Desktop-side helpers shared by the CLI (interactive use) and the GPUI app:
//! merging Ghostty terminals into an [`OpenPlan`] and performing GUI actions.
//! These run in the user's GUI session, not in the host.

use ghostmc_core::config::TerminalConfig;
use ghostmc_core::paths::{self, PathRelation};
use ghostmc_platform::ghostty::{self, GhosttyTerminal};
use ghostmc_protocol::*;
use std::path::Path;

/// Cross-reference Ghostty terminals with tmux clients by tty. A terminal
/// whose tty is an attached tmux client is a *view* of that session; others
/// are unmanaged and listed only if their cwd relates to the location.
pub fn merge_terminals(plan: &mut OpenPlan, terminals: Vec<GhosttyTerminal>) {
    let loc = Path::new(&plan.location.location.path).to_path_buf();
    for s in plan.tmux.iter_mut() {
        for c in s.clients.iter_mut() {
            c.ghostty_terminal_id = terminals
                .iter()
                .find(|t| t.tty.as_deref() == Some(c.tty.as_str()))
                .map(|t| t.terminal_id.clone());
        }
    }
    let mut views = Vec::new();
    for t in terminals {
        let tmux_session_key = plan
            .tmux
            .iter()
            .find(|s| {
                s.clients
                    .iter()
                    .any(|c| c.ghostty_terminal_id.as_deref() == Some(t.terminal_id.as_str()))
            })
            .map(|s| s.key.clone());
        if tmux_session_key.is_some() {
            continue; // shown under its tmux session
        }
        let relation = t
            .working_directory
            .as_deref()
            .map(|d| paths::relation(&loc, &paths::normalize(Path::new(d))));
        if matches!(
            relation,
            Some(PathRelation::Exact | PathRelation::Descendant(_))
        ) {
            views.push(GhosttyView {
                terminal: t,
                tmux_session_key,
                relation,
            });
        }
    }
    plan.ghostty = views;
}

/// Fill Ghostty data into a plan where the platform supports it.
pub fn augment_plan(plan: &mut OpenPlan, cfg: &TerminalConfig) {
    let caps = ghostty::capabilities(cfg);
    if caps.list_terminals {
        match ghostty::list_terminals(cfg) {
            Ok(t) => merge_terminals(plan, t),
            Err(e) => plan.ghostty_error = Some(e.to_string()),
        }
    }
    plan.capabilities = Some(caps);
}

/// The Ghostty terminal that verifiably displays `session` (writable client
/// preferred).
pub fn displayed_in(session: &SessionView) -> Option<String> {
    session
        .clients
        .iter()
        .filter(|c| c.ghostty_terminal_id.is_some())
        .min_by_key(|c| c.readonly)
        .and_then(|c| c.ghostty_terminal_id.clone())
}

/// Terminal IDs with tty of every tmux client of every session (used by the
/// session list to show "focus" buttons outside an open plan).
pub fn annotate_sessions(sessions: &mut [SessionView], cfg: &TerminalConfig) -> Option<String> {
    if !ghostty::capabilities(cfg).list_terminals {
        return None;
    }
    match ghostty::list_terminals(cfg) {
        Ok(ts) => {
            for s in sessions.iter_mut() {
                for c in s.clients.iter_mut() {
                    c.ghostty_terminal_id = ts
                        .iter()
                        .find(|t| t.tty.as_deref() == Some(c.tty.as_str()))
                        .map(|t| t.terminal_id.clone());
                }
            }
            None
        }
        Err(e) => Some(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostmc_core::model::*;

    fn term(id: &str, tty: &str, cwd: &str) -> GhosttyTerminal {
        GhosttyTerminal {
            window_id: "W".into(),
            tab_id: format!("T{id}"),
            tab_index: 1,
            tab_selected: false,
            terminal_id: id.into(),
            title: "t".into(),
            working_directory: Some(cwd.into()),
            tty: Some(tty.into()),
            pid: None,
        }
    }

    fn plan(loc: &str) -> OpenPlan {
        let now = 0;
        OpenPlan {
            project: Project {
                id: "prj_1".into(),
                name: "p".into(),
                notes: None,
                pinned: false,
                tags: vec![],
                created_at: now,
                updated_at: now,
            },
            location: LocationView {
                location: ProjectLocation {
                    id: "loc_1".into(),
                    project_id: "prj_1".into(),
                    device_id: "dev_1".into(),
                    path: loc.into(),
                    default_command: None,
                    created_at: now,
                },
                device_name: "here".into(),
                is_local: true,
                git: None,
                exists: Some(true),
            },
            other_locations: vec![],
            tmux: vec![SessionView {
                key: "tmux:1:1:$1:/s".into(),
                r#ref: SessionRef {
                    socket_path: "/s".into(),
                    server_pid: 1,
                    server_start: 1,
                    session_id: "$1".into(),
                },
                name: "gmc-p".into(),
                created: 0,
                clients: vec![ClientView {
                    tty: "/dev/ttys001".into(),
                    readonly: false,
                    width: 80,
                    height: 24,
                    ghostty_terminal_id: None,
                }],
                panes: vec![],
                managed: true,
                project_id: None,
                project_name: None,
                location_id: Some("loc_1".into()),
                association: None,
                agents: vec![],
                tmux_binary: "tmux".into(),
            }],
            ghostty: vec![],
            ghostty_error: None,
            capabilities: None,
            notes: vec![],
        }
    }

    #[test]
    fn tty_match_links_terminal_to_tmux_session() {
        let mut p = plan("/nonexistent-gmc/w/app");
        merge_terminals(
            &mut p,
            vec![
                // Displays the tmux session; its own cwd is irrelevant.
                term("A", "/dev/ttys001", "/Users/me"),
                // Unmanaged shell inside the project.
                term("B", "/dev/ttys002", "/nonexistent-gmc/w/app/src"),
                // Unrelated.
                term("C", "/dev/ttys003", "/nonexistent-gmc/w/other"),
            ],
        );
        assert_eq!(
            p.tmux[0].clients[0].ghostty_terminal_id.as_deref(),
            Some("A")
        );
        assert_eq!(displayed_in(&p.tmux[0]).as_deref(), Some("A"));
        assert_eq!(p.ghostty.len(), 1);
        assert_eq!(p.ghostty[0].terminal.terminal_id, "B");
        assert_eq!(p.ghostty[0].tmux_session_key, None);
    }
}
