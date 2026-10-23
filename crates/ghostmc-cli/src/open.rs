//! `open`, `attach` and `new`: the session-aware terminal flows.
//!
//! Action semantics are kept distinct on purpose:
//! - **Focus** an existing Ghostty terminal (sends no input, starts nothing);
//! - **Attach** (writable) or **Observe** (read-only, no resize) an existing
//!   tmux session in a new terminal window/tab, locally or over SSH;
//! - **Create** a tmux session, only when explicitly requested.

use crate::{api, backend, render};
use anyhow::{Result, anyhow, bail};
use clap::Args;
use ghostmc_core::config::Config;
use ghostmc_core::matching::Association;
use ghostmc_host::Backend;
use ghostmc_host::remote::RemoteClient;
use ghostmc_platform::ghostty::{self, Placement};
use ghostmc_protocol::*;
use ghostmc_tmux::AttachMode;
use std::io::{BufRead, IsTerminal, Write};

/// Returned when a choice is required but stdin is not interactive.
#[derive(Debug)]
pub struct NeedsChoice(pub String);

impl std::fmt::Display for NeedsChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NeedsChoice {}

#[derive(Args, Clone)]
pub struct TerminalArgs {
    /// Attach read-only: no input, and the session is not resized.
    #[arg(long)]
    pub observe: bool,
    /// Writable attach that does not resize the session (small screens).
    #[arg(long, conflicts_with = "observe")]
    pub keep_size: bool,
    /// Attach in this terminal instead of opening a Ghostty window.
    #[arg(long)]
    pub here: bool,
    /// Open a new tab (macOS) instead of a new window.
    #[arg(long, conflicts_with = "here")]
    pub tab: bool,
    /// With --tab: the Ghostty window id to open the tab in.
    #[arg(long, requires = "tab")]
    pub window_id: Option<String>,
    /// Print the action instead of performing it.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args)]
pub struct OpenArgs {
    pub project: String,
    /// Open the project's location on another device (over SSH).
    #[arg(long)]
    pub device: Option<String>,
    #[arg(long)]
    pub location: Option<String>,
    /// Pick candidate N from the list without prompting.
    #[arg(long)]
    pub choose: Option<usize>,
    /// If no session matches, create one without asking.
    #[arg(long)]
    pub create: bool,
    /// Accept a single candidate even if it only matches by directory.
    #[arg(long, short)]
    pub yes: bool,
    #[command(flatten)]
    pub term: TerminalArgs,
}

#[derive(Args)]
pub struct AttachArgs {
    /// Session key (from `ghostmc sessions --json`), `$id`, or name.
    pub session: String,
    /// tmux socket name or path to disambiguate `$id`/name.
    #[arg(long)]
    pub socket: Option<String>,
    #[arg(long)]
    pub device: Option<String>,
    #[command(flatten)]
    pub term: TerminalArgs,
}

#[derive(Args)]
pub struct NewArgs {
    pub project: String,
    #[arg(long)]
    pub location: Option<String>,
    /// Create another session even if a ghostmc session already exists.
    #[arg(long)]
    pub additional: bool,
    /// Also attach the session in a terminal.
    #[arg(long)]
    pub attach: bool,
    #[command(flatten)]
    pub term: TerminalArgs,
    /// Explicit command for the first pane (default: your shell).
    #[arg(last = true)]
    pub command: Vec<String>,
}

pub fn remote_for(b: &Backend, device: &str) -> Result<RemoteClient> {
    let devices: Vec<ghostmc_core::model::Device> = api(b, Request::ListDevices)?;
    let d = devices
        .into_iter()
        .find(|d| d.name.eq_ignore_ascii_case(device) || d.id.0 == device)
        .ok_or_else(|| anyhow!("unknown device {device:?}; add it with `ghostmc device add`"))?;
    if d.is_local {
        bail!("{} is this device; omit --device", d.name);
    }
    RemoteClient::for_device(&d).map_err(|e| anyhow!("{}", e.message))
}

enum Candidate<'a> {
    Tmux(&'a SessionView),
    Ghostty(&'a GhosttyView),
}

fn prompt(question: &str) -> Result<String> {
    print!("{question}");
    std::io::stdout().flush()?;
    let mut s = String::new();
    std::io::stdin().lock().read_line(&mut s)?;
    Ok(s.trim().to_owned())
}

fn placement(t: &TerminalArgs) -> Placement {
    if t.tab {
        Placement::NewTab {
            window_id: t.window_id.clone(),
        }
    } else {
        Placement::NewWindow
    }
}

fn mode(t: &TerminalArgs) -> AttachMode {
    match (t.observe, t.keep_size) {
        (true, _) => AttachMode::Observe,
        (false, true) => AttachMode::WritableKeepSize,
        _ => AttachMode::Writable,
    }
}

/// Run `argv` in a new terminal or, with `--here`, replace this process.
fn hand_off(
    argv: Vec<String>,
    cwd: Option<String>,
    t: &TerminalArgs,
    local_tmux: bool,
    json: bool,
) -> Result<()> {
    if t.dry_run {
        if json {
            println!(
                "{}",
                serde_json::json!({ "action": if t.here { "exec" } else { "launch" }, "argv": argv, "cwd": cwd })
            );
        } else {
            println!("would run: {}", ghostmc_core::shell::join(&argv));
        }
        return Ok(());
    }
    if t.here {
        if local_tmux && std::env::var_os("TMUX").is_some() {
            bail!("already inside tmux; run from a plain terminal or use `tmux switch-client`");
        }
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&argv[0]).args(&argv[1..]).exec();
        bail!("exec {}: {err}", argv[0]);
    }
    let cfg = Config::load_default()?;
    let launched = ghostty::launch(&cfg.terminal, &argv, cwd.as_deref(), &placement(t))?;
    if json {
        println!(
            "{}",
            serde_json::json!({ "action": "launch", "argv": argv, "method": launched.method, "terminal_id": launched.terminal_id })
        );
    } else {
        println!(
            "opened via {} ({})",
            launched.method,
            if t.observe {
                "read-only observer"
            } else {
                "writable"
            }
        );
    }
    Ok(())
}

fn focus(terminal_id: &str, t: &TerminalArgs, json: bool) -> Result<()> {
    if t.dry_run {
        println!("would focus Ghostty terminal {terminal_id}");
        return Ok(());
    }
    let cfg = Config::load_default()?;
    ghostty::focus_terminal(&cfg.terminal, terminal_id)?;
    if json {
        println!(
            "{}",
            serde_json::json!({ "action": "focus", "terminal_id": terminal_id })
        );
    } else {
        println!("focused existing Ghostty terminal {terminal_id} (no input sent)");
    }
    Ok(())
}

/// Attach to (or focus) a local tmux session.
fn act_on_local_session(b: &Backend, s: &SessionView, t: &TerminalArgs, json: bool) -> Result<()> {
    let caps = ghostty::capabilities(&Config::load_default()?.terminal);
    if mode(t) == AttachMode::Writable
        && !t.here
        && caps.focus_terminal
        && let Some(term) = ghostmc_host::desktop::displayed_in(s)
    {
        return focus(&term, t, json);
    }
    let cmd: AttachCommand = api(
        b,
        Request::AttachCommand {
            key: s.key.clone(),
            mode: mode(t),
        },
    )?;
    hand_off(cmd.argv, cmd.cwd, t, true, json)
}

fn choose_index(
    n: usize,
    args_choose: Option<usize>,
    auto: bool,
    allow_new: bool,
) -> Result<Option<usize>> {
    if let Some(c) = args_choose {
        if c == 0 || c > n {
            bail!("--choose must be between 1 and {n}");
        }
        return Ok(Some(c - 1));
    }
    if auto {
        return Ok(Some(0));
    }
    if !std::io::stdin().is_terminal() {
        return Err(NeedsChoice(format!(
            "{n} candidates; pick one with --choose N{}",
            if allow_new {
                " (or create with `ghostmc new`)"
            } else {
                ""
            }
        ))
        .into());
    }
    let extra = if allow_new { ", n = new session" } else { "" };
    let ans = prompt(&format!("Select 1-{n}{extra}, q = cancel: "))?;
    match ans.as_str() {
        "q" | "" => Ok(None),
        "n" if allow_new => Ok(Some(usize::MAX)),
        s => {
            let i: usize = s.parse().map_err(|_| anyhow!("not a number: {s}"))?;
            if i == 0 || i > n {
                bail!("out of range");
            }
            Ok(Some(i - 1))
        }
    }
}

pub fn open(a: OpenArgs, json: bool) -> Result<()> {
    let b = backend()?;
    if let Some(device) = a.device.clone() {
        return open_remote(&b, &a, &device, json);
    }
    let mut plan: OpenPlan = api(
        &b,
        Request::OpenPlan {
            project: a.project.clone(),
            location_id: a.location.clone(),
        },
    )?;
    let cfg = Config::load_default()?;
    ghostmc_host::desktop::augment_plan(&mut plan, &cfg.terminal);

    let mut cands: Vec<Candidate> = plan.tmux.iter().map(Candidate::Tmux).collect();
    cands.extend(plan.ghostty.iter().map(Candidate::Ghostty));

    if json && a.choose.is_none() && !a.create {
        println!("{}", serde_json::to_string_pretty(&plan)?);
        return Ok(());
    }
    if !json {
        render::open_plan(&plan);
    }

    if cands.is_empty() {
        let create = a.create
            || (std::io::stdin().is_terminal()
                && !a.term.dry_run
                && prompt("No existing session. Create a new tmux session here? [y/N] ")?
                    .eq_ignore_ascii_case("y"));
        if !create {
            bail!(
                "nothing to open; create a session with `ghostmc open {} --create` or `ghostmc new`",
                a.project
            );
        }
        return create_and_attach(
            &b,
            &plan.location.location.id.0,
            CreateIntent::Reuse,
            vec![],
            &a.term,
            json,
        );
    }

    let single_verified = cands.len() == 1
        && matches!(cands[0], Candidate::Tmux(s) if s.association == Some(Association::Tagged));
    let auto = cands.len() == 1 && (single_verified || a.yes);
    let Some(i) = choose_index(cands.len(), a.choose, auto, true)? else {
        println!("cancelled");
        return Ok(());
    };
    if i == usize::MAX {
        return create_and_attach(
            &b,
            &plan.location.location.id.0,
            CreateIntent::Additional,
            vec![],
            &a.term,
            json,
        );
    }
    match cands[i] {
        Candidate::Tmux(s) => act_on_local_session(&b, s, &a.term, json),
        Candidate::Ghostty(g) => {
            if a.term.observe {
                bail!(
                    "an unmanaged Ghostty terminal cannot be observed; only tmux sessions can be attached"
                );
            }
            focus(&g.terminal.terminal_id, &a.term, json)
        }
    }
}

fn create_and_attach(
    b: &Backend,
    location_id: &str,
    intent: CreateIntent,
    command: Vec<String>,
    t: &TerminalArgs,
    json: bool,
) -> Result<()> {
    if t.dry_run {
        println!(
            "would create a tmux session for location {location_id} ({intent:?}) and attach it"
        );
        return Ok(());
    }
    let c: CreatedSession = api(
        b,
        Request::CreateSession {
            location_id: location_id.into(),
            intent,
            command,
        },
    )?;
    if !json {
        if c.created {
            println!(
                "created tmux session {} ({})",
                c.session.name, c.session.r#ref.session_id
            );
        } else {
            println!(
                "reusing existing ghostmc session {} ({})",
                c.session.name, c.session.r#ref.session_id
            );
        }
    }
    act_on_local_session(b, &c.session, t, json)
}

fn open_remote(b: &Backend, a: &OpenArgs, device: &str, json: bool) -> Result<()> {
    let remote = remote_for(b, device)?;
    let p: ProjectSummary = api(
        b,
        Request::GetProject {
            key: a.project.clone(),
        },
    )?;
    let loc = p
        .locations
        .iter()
        .filter(|l| l.device_name.eq_ignore_ascii_case(device))
        .find(|l| a.location.as_deref().is_none_or(|id| l.location.id.0.starts_with(id)))
        .ok_or_else(|| {
            anyhow!(
                "{} has no location on {device}; link one with `ghostmc project link {} <path> --device {device}`",
                p.project.name,
                p.project.name
            )
        })?;
    let plan: PathPlanResult = remote
        .call(
            Request::PathPlan {
                path: loc.location.path.clone(),
            },
            false,
        )
        .map_err(|e| anyhow!("{}", e.message))?;
    if json && a.choose.is_none() {
        println!("{}", serde_json::to_string_pretty(&plan)?);
        return Ok(());
    }
    if !json {
        println!(
            "{} on {} ({}){}",
            p.project.name,
            plan.device.name,
            plan.path,
            if plan.exists { "" } else { "  [missing]" }
        );
        render::session_list(&plan.tmux, true);
    }
    if plan.tmux.is_empty() {
        bail!(
            "no tmux session for that path on {device}. Remote creation is off by default; create one on {device} with `ghostmc new`"
        );
    }
    let auto =
        plan.tmux.len() == 1 && (plan.tmux[0].association == Some(Association::Tagged) || a.yes);
    let Some(i) = choose_index(plan.tmux.len(), a.choose, auto, false)? else {
        println!("cancelled");
        return Ok(());
    };
    let argv = remote.attach_argv(&plan.tmux[i], mode(&a.term));
    hand_off(argv, None, &a.term, false, json)
}

pub fn attach(a: AttachArgs, json: bool) -> Result<()> {
    let b = backend()?;
    if let Some(device) = &a.device {
        let remote = remote_for(&b, device)?;
        let s: SessionView = remote
            .call(
                Request::ResolveSession {
                    spec: a.session.clone(),
                    socket: a.socket.clone(),
                },
                false,
            )
            .map_err(|e| anyhow!("{}", e.message))?;
        let argv = remote.attach_argv(&s, mode(&a.term));
        return hand_off(argv, None, &a.term, false, json);
    }
    let mut s: SessionView = api(
        &b,
        Request::ResolveSession {
            spec: a.session.clone(),
            socket: a.socket.clone(),
        },
    )?;
    let cfg = Config::load_default()?;
    ghostmc_host::desktop::annotate_sessions(std::slice::from_mut(&mut s), &cfg.terminal);
    act_on_local_session(&b, &s, &a.term, json)
}

pub fn new_session(a: NewArgs, json: bool) -> Result<()> {
    let b = backend()?;
    let loc = crate::local_location(&b, &a.project, a.location.as_deref())?;
    let intent = if a.additional {
        CreateIntent::Additional
    } else {
        CreateIntent::Reuse
    };
    if a.attach {
        return create_and_attach(&b, &loc.location.id.0, intent, a.command, &a.term, json);
    }
    let c: CreatedSession = api(
        &b,
        Request::CreateSession {
            location_id: loc.location.id.0.clone(),
            intent,
            command: a.command,
        },
    )?;
    if json {
        println!("{}", serde_json::to_string_pretty(&c)?);
        return Ok(());
    }
    println!(
        "{} tmux session {} ({}) in {}",
        if c.created {
            "created"
        } else {
            "reusing existing"
        },
        c.session.name,
        c.session.r#ref.session_id,
        loc.location.path
    );
    println!("attach: ghostmc attach '{}'", c.session.key);
    Ok(())
}
