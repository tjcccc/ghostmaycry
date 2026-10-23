//! Human-readable output.

use ghostmc_core::agent::{AgentEvent, AgentState};
use ghostmc_core::matching::Association;
use ghostmc_core::model::Device;
use ghostmc_platform::ghostty::GhosttyTerminal;
use ghostmc_protocol::*;

fn ago(ts: i64) -> String {
    let d = (ghostmc_core::now() - ts).max(0);
    match d {
        0..60 => format!("{d}s ago"),
        60..3600 => format!("{}m ago", d / 60),
        3600..86400 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

pub fn state_label(s: AgentState) -> &'static str {
    match s {
        AgentState::Running => "running",
        AgentState::NeedsApproval => "NEEDS APPROVAL",
        AgentState::Idle => "idle",
        AgentState::TurnEnded => "turn ended",
        AgentState::Failed => "FAILED",
        AgentState::Ended => "ended",
        AgentState::Disconnected => "disconnected",
        AgentState::Unknown => "unknown (stale)",
    }
}

fn assoc_label(a: Option<Association>) -> &'static str {
    match a {
        Some(Association::Tagged) => "ghostmc session",
        Some(Association::PathExact) => "cwd match",
        Some(Association::PathDescendant) => "cwd in subdir",
        None => "",
    }
}

pub fn status(s: &StatusInfo) {
    println!(
        "ghostmc {} (protocol {}.{}, schema v{})",
        s.ghostmc_version, s.protocol.major, s.protocol.minor, s.schema_version
    );
    println!(
        "device   {} ({}, {})",
        s.device.name, s.device.os, s.device.id
    );
    match s.host_pid {
        Some(pid) => println!("host     running (pid {pid})"),
        None => println!("host     not running (direct mode)"),
    }
    println!(
        "tmux     {} [{}]",
        s.tmux_version.as_deref().unwrap_or("NOT FOUND"),
        s.tmux_binary
    );
    for sock in &s.sockets {
        println!("socket   {sock}");
    }
    println!("registry {}", s.db_path);
}

pub fn projects(v: &[ProjectSummary]) {
    if v.is_empty() {
        println!("no projects yet; add one with: ghostmc project add <name> --path <dir>");
        return;
    }
    for p in v {
        let tags = if p.project.tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", p.project.tags.join(", "))
        };
        let attention = if p.attention > 0 {
            format!("  ⚠ {} need attention", p.attention)
        } else {
            String::new()
        };
        println!(
            "{}{}{}  sessions: {}{}",
            if p.project.pinned { "★ " } else { "" },
            p.project.name,
            tags,
            p.live_sessions,
            attention
        );
        for l in &p.locations {
            println!(
                "    {}:{}{}",
                l.device_name,
                l.location.path,
                if l.exists == Some(false) {
                    "  [missing]"
                } else {
                    ""
                }
            );
        }
    }
}

pub fn project(p: &ProjectSummary) {
    println!("{} ({})", p.project.name, p.project.id);
    if !p.project.tags.is_empty() {
        println!("  tags: {}", p.project.tags.join(", "));
    }
    if let Some(n) = &p.project.notes {
        println!("  notes: {n}");
    }
    for l in &p.locations {
        print!(
            "  location {} {}:{}",
            l.location.id, l.device_name, l.location.path
        );
        if let Some(g) = &l.git {
            print!(
                "  git: {}{}",
                g.branch.as_deref().unwrap_or("(detached)"),
                if g.is_linked_worktree {
                    " (linked worktree)"
                } else {
                    ""
                }
            );
        }
        println!();
    }
    println!(
        "  live sessions: {}   needing attention: {}",
        p.live_sessions, p.attention
    );
}

pub fn devices(v: &[Device]) {
    for d in v {
        println!(
            "{:<16} {:<8} {}",
            d.name,
            d.os,
            if d.is_local {
                "this device".to_string()
            } else {
                format!("ssh {}", d.ssh_target.as_deref().unwrap_or("?"))
            }
        );
    }
}

pub fn session_list(v: &[SessionView], numbered: bool) {
    for (i, s) in v.iter().enumerate() {
        let n = if numbered {
            format!("[{}] ", i + 1)
        } else {
            String::new()
        };
        let clients = if s.clients.is_empty() {
            "detached".to_string()
        } else {
            format!(
                "{} client(s){}",
                s.clients.len(),
                if s.clients.iter().any(|c| c.ghostty_terminal_id.is_some()) {
                    ", shown in Ghostty"
                } else {
                    ""
                }
            )
        };
        let socket = std::path::Path::new(&s.r#ref.socket_path)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        println!(
            "{n}tmux {}:{} {}  {}  {}{}",
            socket,
            s.r#ref.session_id,
            s.name,
            clients,
            assoc_label(s.association),
            s.project_name
                .as_deref()
                .map(|p| format!("  project: {p}"))
                .unwrap_or_default()
        );
        if let Some(p) = s.active_pane() {
            println!("      {}  {}  {}", p.id, p.command, p.cwd);
        }
        for a in &s.agents {
            println!(
                "      {} {}",
                agent_label(a.run.agent),
                state_label(a.state)
            );
        }
    }
}

pub fn sessions(v: &[SessionView]) {
    if v.is_empty() {
        println!("no tmux sessions found");
        return;
    }
    session_list(v, false);
}

pub fn open_plan(p: &OpenPlan) {
    let loc = &p.location;
    print!("{} — {}", p.project.name, loc.location.path);
    if let Some(g) = &loc.git {
        print!(
            "  ({}{})",
            g.branch.as_deref().unwrap_or("detached"),
            if g.is_linked_worktree {
                ", worktree"
            } else {
                ""
            }
        );
    }
    println!();
    if !p.tmux.is_empty() {
        println!("tmux sessions (attachable, also from other devices):");
        session_list(&p.tmux, true);
    }
    if !p.ghostty.is_empty() {
        println!("Ghostty terminals not in tmux (focus only; not attachable remotely):");
        for (i, g) in p.ghostty.iter().enumerate() {
            println!(
                "[{}] ghostty {}  {}  {}",
                p.tmux.len() + i + 1,
                g.terminal.terminal_id,
                g.terminal.title,
                g.terminal.working_directory.as_deref().unwrap_or("")
            );
        }
    }
    if let Some(e) = &p.ghostty_error {
        println!("(Ghostty not inspected: {e})");
    }
    for n in &p.notes {
        println!("note: {n}");
    }
}

pub fn ghostty(ts: &[GhosttyTerminal]) {
    if ts.is_empty() {
        println!("no Ghostty terminals (or Ghostty not running)");
    }
    for t in ts {
        println!(
            "{}  window {} tab {}  {}  {}  {}",
            t.terminal_id,
            t.window_id,
            t.tab_index,
            t.tty.as_deref().unwrap_or("-"),
            t.title,
            t.working_directory.as_deref().unwrap_or("")
        );
    }
}

pub fn agents(v: &[AgentView]) {
    if v.is_empty() {
        println!("no agent activity recorded (install hooks with `ghostmc hook install`)");
        return;
    }
    for a in v {
        println!(
            "{:<11} {:<16} {:<20} {}  {}",
            agent_label(a.run.agent),
            state_label(a.state),
            a.project_name.as_deref().unwrap_or("-"),
            ago(a.run.last_event_at),
            attribution_label(a.run.attribution)
        );
        if let Some(m) = &a.run.last_message {
            println!("            {m}");
        }
    }
}

pub fn events(v: &[AgentEvent]) {
    for e in v {
        println!(
            "{}  {:<6} {:<18} {}  {}",
            ago(e.ts),
            e.agent.as_str(),
            e.hook_event,
            e.detail.as_deref().unwrap_or(""),
            e.tmux_pane.as_deref().unwrap_or("")
        );
    }
}

pub fn git_status(s: &ghostmc_core::git::GitStatus) {
    println!(
        "branch {}{}",
        s.branch.as_deref().unwrap_or("(detached)"),
        s.upstream
            .as_ref()
            .map(|u| format!(" → {u} (+{} -{})", s.ahead, s.behind))
            .unwrap_or_default()
    );
    if s.files.is_empty() {
        println!("clean");
    }
    for f in &s.files {
        println!("  {} {}", f.xy, f.path);
    }
    println!("(repository-wide changes; not attributed to any agent)");
}
