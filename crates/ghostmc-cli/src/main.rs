//! `ghostmc` — Ghost May Cry command line.

mod hook;
mod install;
mod open;
mod render;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use ghostmc_host::Backend;
use ghostmc_protocol::*;
use std::io::{BufRead, Write};

#[derive(Parser)]
#[command(
    name = "ghostmc",
    version,
    about = "Ghost May Cry: mission control for projects, tmux sessions and coding agents"
)]
struct Cli {
    /// Machine-readable JSON output.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Host, registry and tmux status.
    Status,
    /// List projects.
    Projects,
    /// Manage projects.
    #[command(subcommand)]
    Project(ProjectCmd),
    /// List devices.
    Devices,
    /// Manage remote devices (reached over SSH).
    #[command(subcommand)]
    Device(DeviceCmd),
    /// List live tmux sessions (optionally for one project or device).
    Sessions {
        project: Option<String>,
        #[arg(long)]
        device: Option<String>,
    },
    /// Find a project's existing terminal and focus or attach it; create a
    /// session only when you ask for it.
    Open(open::OpenArgs),
    /// Attach to an existing tmux session (local or --device).
    Attach(open::AttachArgs),
    /// Create a new tmux session for a project (reuses the ghostmc session
    /// for the location unless --additional).
    New(open::NewArgs),
    /// Focus an existing Ghostty terminal by id (macOS).
    Focus { terminal_id: String },
    /// List Ghostty terminals (macOS).
    Ghostty,
    /// Coding agent runs and their states.
    Agents {
        /// Look back this many hours.
        #[arg(long, default_value_t = 24)]
        hours: i64,
    },
    /// Recent raw agent events.
    Events {
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
    /// Git status / diff for a project location.
    #[command(subcommand)]
    Git(GitCmd),
    /// List one directory of a project.
    Files {
        project: String,
        #[arg(default_value = "")]
        rel: String,
        #[arg(long)]
        hidden: bool,
        #[arg(long)]
        location: Option<String>,
    },
    /// Open a project file or folder in your editor.
    Edit {
        project: String,
        #[arg(default_value = "")]
        rel: String,
        #[arg(long)]
        location: Option<String>,
    },
    /// Reveal a project file or folder in Finder / the file manager.
    Reveal {
        project: String,
        #[arg(default_value = "")]
        rel: String,
        #[arg(long)]
        location: Option<String>,
    },
    /// Agent hook integration.
    #[command(subcommand)]
    Hook(hook::HookCmd),
    /// Run or install the background host.
    #[command(subcommand)]
    Host(install::HostCmd),
    /// Answer one JSON request from stdin (used over SSH by other devices).
    Api {
        /// Permit requests that change state. Off by default for remote use.
        #[arg(long)]
        allow_write: bool,
    },
}

#[derive(Subcommand)]
enum ProjectCmd {
    /// Register a project, optionally with a local path.
    Add {
        name: String,
        #[arg(long)]
        path: Option<String>,
        #[arg(long = "tag", short)]
        tags: Vec<String>,
        #[arg(long)]
        notes: Option<String>,
    },
    /// Show a project with its locations and git facts.
    Show { project: String },
    /// Remove a project from the registry (files and sessions are untouched).
    Rm { project: String },
    /// Rename, pin, annotate or tag.
    Edit {
        project: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        notes: Option<String>,
        #[arg(long)]
        pin: bool,
        #[arg(long, conflicts_with = "pin")]
        unpin: bool,
        #[arg(long = "tag")]
        add_tags: Vec<String>,
        #[arg(long = "untag")]
        remove_tags: Vec<String>,
    },
    /// Add a location (path on this or another device).
    Link {
        project: String,
        path: String,
        #[arg(long)]
        device: Option<String>,
        /// Command offered when creating a session (never run automatically).
        #[arg(long)]
        command: Option<String>,
    },
    /// Remove a location by id.
    Unlink { location_id: String },
}

#[derive(Subcommand)]
enum DeviceCmd {
    /// Register a device reachable with `ssh <target>`.
    Add {
        name: String,
        #[arg(long)]
        ssh: String,
        /// Path of ghostmc on that device if not on its PATH.
        #[arg(long)]
        ghostmc: Option<String>,
    },
    Rm {
        name: String,
    },
    /// Query a device's status over SSH.
    Check {
        name: String,
    },
}

#[derive(Subcommand)]
enum GitCmd {
    Status {
        project: String,
        #[arg(long)]
        location: Option<String>,
    },
    Diff {
        project: String,
        path: Option<String>,
        #[arg(long)]
        staged: bool,
        #[arg(long)]
        location: Option<String>,
    },
}

pub fn backend() -> Result<Backend> {
    Backend::connect_or_direct().map_err(|e| anyhow!(e))
}

pub fn api<T: serde::de::DeserializeOwned>(b: &Backend, r: Request) -> Result<T> {
    b.call(r).map_err(|e| anyhow!("{}", e.message))
}

/// Resolve the local location of a project (first one unless `--location`).
pub fn local_location(b: &Backend, project: &str, location: Option<&str>) -> Result<LocationView> {
    let p: ProjectSummary = api(
        b,
        Request::GetProject {
            key: project.into(),
        },
    )?;
    let pick = p.locations.into_iter().filter(|l| l.is_local).find(|l| {
        location.is_none_or(|id| l.location.id.0 == id || l.location.id.0.starts_with(id))
    });
    pick.ok_or_else(|| anyhow!("{} has no matching location on this device", p.project.name))
}

fn print_json<T: serde::Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn run_api(allow_write: bool) -> Result<()> {
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let backend = backend()?;
    // Re-check the read-only guard here: the host itself trusts local
    // callers, but `api` is the entry point for other devices.
    let parsed: Result<RequestEnvelope, _> = serde_json::from_str(line.trim());
    let resp = match parsed {
        Ok(env) if !allow_write && !is_read_only(&env.request) => ResponseEnvelope {
            id: env.id,
            protocol: PROTOCOL,
            body: ResponseBody::Error {
                error: ApiError::new(
                    ErrorCode::Forbidden,
                    "write requests need `ghostmc api --allow-write` on this device",
                ),
            },
        },
        Ok(env) if !env.protocol.compatible_with(PROTOCOL) => ResponseEnvelope {
            id: env.id,
            protocol: PROTOCOL,
            body: ResponseBody::Error {
                error: ApiError::new(
                    ErrorCode::IncompatibleProtocol,
                    format!(
                        "remote speaks {}.{}, this device {}.{}",
                        env.protocol.major, env.protocol.minor, PROTOCOL.major, PROTOCOL.minor
                    ),
                ),
            },
        },
        Ok(mut env) => {
            // Briefs are assistant text held for the local desktop only.
            if let Request::AgentRuns { briefs, .. } = &mut env.request {
                *briefs = false;
            }
            match backend.call_raw(env.request) {
                Ok(mut r) => {
                    r.id = env.id;
                    r
                }
                Err(error) => ResponseEnvelope {
                    id: env.id,
                    protocol: PROTOCOL,
                    body: ResponseBody::Error { error },
                },
            }
        }
        Err(e) => ResponseEnvelope {
            id: 0,
            protocol: PROTOCOL,
            body: ResponseBody::Error {
                error: ApiError::new(ErrorCode::Invalid, format!("bad request: {e}")),
            },
        },
    };
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, &resp)?;
    writeln!(out)?;
    Ok(())
}

fn main() {
    // Hook ingestion must never print or fail loudly: it runs inside agents.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("hook")
        && args.get(2).map(String::as_str) == Some("ingest")
    {
        hook::ingest_from_args(&args[3..]);
        return;
    }
    // SAFETY: no other thread exists yet.
    unsafe { ghostmc_core::agent::clear_session_env() };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("GHOSTMC_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("ghostmc: {e:#}");
        std::process::exit(match e.downcast_ref::<open::NeedsChoice>() {
            Some(_) => 2,
            None => 1,
        });
    }
}

fn run(cli: Cli) -> Result<()> {
    let json = cli.json;
    match cli.cmd {
        Cmd::Status => {
            let b = backend()?;
            let s: StatusInfo = api(&b, Request::Status)?;
            if json {
                return print_json(&s);
            }
            render::status(&s);
        }
        Cmd::Projects => {
            let b = backend()?;
            let v: Vec<ProjectSummary> = api(&b, Request::ListProjects)?;
            if json {
                return print_json(&v);
            }
            render::projects(&v);
        }
        Cmd::Project(c) => project_cmd(c, json)?,
        Cmd::Devices => {
            let b = backend()?;
            let v: Vec<ghostmc_core::model::Device> = api(&b, Request::ListDevices)?;
            if json {
                return print_json(&v);
            }
            render::devices(&v);
        }
        Cmd::Device(c) => device_cmd(c, json)?,
        Cmd::Sessions { project, device } => {
            let b = backend()?;
            let mut v: Vec<SessionView> = match &device {
                Some(d) => {
                    let remote = open::remote_for(&b, d)?;
                    remote
                        .call(Request::Sessions { project: None }, false)
                        .map_err(|e| anyhow!("{}", e.message))?
                }
                None => api(&b, Request::Sessions { project })?,
            };
            if device.is_none() {
                let cfg = ghostmc_core::config::Config::load_default()?;
                ghostmc_host::desktop::annotate_sessions(&mut v, &cfg.terminal);
            }
            if json {
                return print_json(&v);
            }
            render::sessions(&v);
        }
        Cmd::Open(a) => open::open(a, json)?,
        Cmd::Attach(a) => open::attach(a, json)?,
        Cmd::New(a) => open::new_session(a, json)?,
        Cmd::Focus { terminal_id } => {
            let cfg = ghostmc_core::config::Config::load_default()?;
            ghostmc_platform::ghostty::focus_terminal(&cfg.terminal, &terminal_id)?;
            println!("focused Ghostty terminal {terminal_id}");
        }
        Cmd::Ghostty => {
            let cfg = ghostmc_core::config::Config::load_default()?;
            let caps = ghostmc_platform::ghostty::capabilities(&cfg.terminal);
            let ts = ghostmc_platform::ghostty::list_terminals(&cfg.terminal);
            if json {
                return print_json(&serde_json::json!({
                    "capabilities": caps,
                    "terminals": ts.as_ref().ok(),
                    "error": ts.as_ref().err().map(|e| e.to_string()),
                }));
            }
            match ts {
                Ok(ts) => render::ghostty(&ts),
                Err(e) => {
                    println!("{e}");
                    for n in caps.notes {
                        println!("  note: {n}");
                    }
                }
            }
        }
        Cmd::Agents { hours } => {
            let b = backend()?;
            let v: Vec<AgentView> = api(
                &b,
                Request::AgentRuns {
                    since_secs: Some(hours * 3600),
                    briefs: false,
                },
            )?;
            if json {
                return print_json(&v);
            }
            render::agents(&v);
        }
        Cmd::Events { limit } => {
            let b = backend()?;
            let v: Vec<ghostmc_core::agent::AgentEvent> =
                api(&b, Request::RecentEvents { limit: Some(limit) })?;
            if json {
                return print_json(&v);
            }
            render::events(&v);
        }
        Cmd::Git(GitCmd::Status { project, location }) => {
            let b = backend()?;
            let loc = local_location(&b, &project, location.as_deref())?;
            let s: ghostmc_core::git::GitStatus = api(
                &b,
                Request::GitStatus {
                    location_id: loc.location.id.0,
                },
            )?;
            if json {
                return print_json(&s);
            }
            render::git_status(&s);
        }
        Cmd::Git(GitCmd::Diff {
            project,
            path,
            staged,
            location,
        }) => {
            let b = backend()?;
            let loc = local_location(&b, &project, location.as_deref())?;
            let d: ghostmc_core::git::Diff = api(
                &b,
                Request::GitDiff {
                    location_id: loc.location.id.0,
                    path,
                    staged,
                },
            )?;
            if json {
                return print_json(&d);
            }
            print!("{}", d.text);
            if d.truncated {
                eprintln!("(diff truncated)");
            }
        }
        Cmd::Files {
            project,
            rel,
            hidden,
            location,
        } => {
            let b = backend()?;
            let loc = local_location(&b, &project, location.as_deref())?;
            let l: ghostmc_core::files::Listing = api(
                &b,
                Request::ListFiles {
                    location_id: loc.location.id.0,
                    rel,
                    show_hidden: hidden,
                },
            )?;
            if json {
                return print_json(&l);
            }
            for e in &l.entries {
                println!("{}{}", e.rel_path, if e.is_dir { "/" } else { "" });
            }
            if l.truncated {
                eprintln!("(listing truncated)");
            }
        }
        Cmd::Edit {
            project,
            rel,
            location,
        } => {
            let b = backend()?;
            let loc = local_location(&b, &project, location.as_deref())?;
            let path = ghostmc_core::paths::resolve_within(
                std::path::Path::new(&loc.location.path),
                &rel,
            )?;
            let cfg = ghostmc_core::config::Config::load_default()?;
            ghostmc_platform::open::open_in_editor(&cfg.editor.command, &path)?;
            println!("opened {}", path.display());
        }
        Cmd::Reveal {
            project,
            rel,
            location,
        } => {
            let b = backend()?;
            let loc = local_location(&b, &project, location.as_deref())?;
            let path = ghostmc_core::paths::resolve_within(
                std::path::Path::new(&loc.location.path),
                &rel,
            )?;
            ghostmc_platform::open::reveal(&path)?;
        }
        Cmd::Hook(c) => hook::run(c, json)?,
        Cmd::Host(c) => install::run(c, json)?,
        Cmd::Api { allow_write } => run_api(allow_write)?,
    }
    Ok(())
}

fn project_cmd(c: ProjectCmd, json: bool) -> Result<()> {
    let b = backend()?;
    match c {
        ProjectCmd::Add {
            name,
            path,
            tags,
            notes,
        } => {
            let p: ProjectSummary = api(
                &b,
                Request::CreateProject {
                    name,
                    tags,
                    notes,
                    path,
                },
            )?;
            if json {
                return print_json(&p);
            }
            println!("added project {} ({})", p.project.name, p.project.id);
            render::project(&p);
        }
        ProjectCmd::Show { project } => {
            let p: ProjectSummary = api(&b, Request::GetProject { key: project })?;
            if json {
                return print_json(&p);
            }
            render::project(&p);
        }
        ProjectCmd::Rm { project } => {
            let p: ghostmc_core::model::Project = api(&b, Request::DeleteProject { key: project })?;
            println!(
                "removed {} from the registry (files and tmux sessions were not touched)",
                p.name
            );
        }
        ProjectCmd::Edit {
            project,
            name,
            notes,
            pin,
            unpin,
            add_tags,
            remove_tags,
        } => {
            let pinned = if pin {
                Some(true)
            } else if unpin {
                Some(false)
            } else {
                None
            };
            let p: ghostmc_core::model::Project = api(
                &b,
                Request::UpdateProject {
                    key: project,
                    name,
                    notes,
                    pinned,
                    add_tags,
                    remove_tags,
                },
            )?;
            if json {
                return print_json(&p);
            }
            println!(
                "{} [{}]{}",
                p.name,
                p.tags.join(", "),
                if p.pinned { " pinned" } else { "" }
            );
        }
        ProjectCmd::Link {
            project,
            path,
            device,
            command,
        } => {
            let l: ghostmc_core::model::ProjectLocation = api(
                &b,
                Request::AddLocation {
                    project,
                    path,
                    device,
                    default_command: command,
                },
            )?;
            if json {
                return print_json(&l);
            }
            println!("linked {} ({})", l.path, l.id);
        }
        ProjectCmd::Unlink { location_id } => {
            let _: serde_json::Value = api(&b, Request::RemoveLocation { location_id })?;
            println!("location removed");
        }
    }
    Ok(())
}

fn device_cmd(c: DeviceCmd, json: bool) -> Result<()> {
    let b = backend()?;
    match c {
        DeviceCmd::Add { name, ssh, ghostmc } => {
            let d: ghostmc_core::model::Device = api(
                &b,
                Request::AddDevice {
                    name,
                    ssh_target: ssh,
                    remote_ghostmc: ghostmc,
                },
            )?;
            if json {
                return print_json(&d);
            }
            println!(
                "added device {} → ssh {}",
                d.name,
                d.ssh_target.as_deref().unwrap_or("")
            );
            println!("check it with: ghostmc device check {}", d.name);
        }
        DeviceCmd::Rm { name } => {
            let _: serde_json::Value = api(&b, Request::RemoveDevice { key: name })?;
            println!("device removed");
        }
        DeviceCmd::Check { name } => {
            let remote = open::remote_for(&b, &name)?;
            let s: StatusInfo = remote
                .call(Request::Status, false)
                .map_err(|e| anyhow!("{}", e.message))
                .with_context(|| format!("querying {name} over ssh"))?;
            if json {
                return print_json(&s);
            }
            render::status(&s);
        }
    }
    Ok(())
}
