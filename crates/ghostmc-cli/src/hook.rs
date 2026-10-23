//! `ghostmc hook …`: ingest agent hook events; install/uninstall hooks.

use anyhow::{Result, anyhow};
use clap::{Subcommand, ValueEnum};
use ghostmc_core::agent::{AgentEvent, AgentKind, HookEnv};
use ghostmc_core::hooks;
use ghostmc_protocol::*;
use std::io::Read;

#[derive(Clone, Copy, ValueEnum)]
pub enum Which {
    Claude,
    Codex,
    All,
}

impl Which {
    fn kinds(self) -> Vec<AgentKind> {
        match self {
            Which::Claude => vec![AgentKind::Claude],
            Which::Codex => vec![AgentKind::Codex],
            Which::All => vec![AgentKind::Claude, AgentKind::Codex],
        }
    }
}

#[derive(Subcommand)]
pub enum HookCmd {
    /// Called by the agents themselves; reads hook JSON on stdin.
    Ingest {
        #[arg(long)]
        agent: String,
    },
    /// Add ghostmc hooks to the agent's user config (non-destructive, backed up).
    Install {
        #[arg(value_enum, default_value = "all")]
        agent: Which,
        /// Config file to edit instead of the default location.
        #[arg(long)]
        config: Option<std::path::PathBuf>,
        /// ghostmc path the hooks should call (default: this executable).
        #[arg(long)]
        ghostmc: Option<String>,
    },
    /// Remove only ghostmc's hooks.
    Uninstall {
        #[arg(value_enum, default_value = "all")]
        agent: Which,
        #[arg(long)]
        config: Option<std::path::PathBuf>,
    },
    /// Show whether hooks are installed.
    Status,
}

const MAX_PAYLOAD: u64 = 1024 * 1024;

/// Entry point for `ghostmc hook ingest`. Never prints to stdout (agents may
/// parse it) and always exits 0 so a ghostmc problem cannot block an agent.
pub fn ingest_from_args(args: &[String]) {
    let agent = args
        .windows(2)
        .find(|w| w[0] == "--agent")
        .and_then(|w| AgentKind::parse(&w[1]))
        .or_else(|| {
            args.iter()
                .find_map(|a| a.strip_prefix("--agent=").and_then(AgentKind::parse))
        });
    let Some(agent) = agent else { return };
    let mut raw = String::new();
    if std::io::stdin()
        .take(MAX_PAYLOAD)
        .read_to_string(&mut raw)
        .is_err()
    {
        return;
    }
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return;
    };
    let Some(event) = AgentEvent::from_hook(
        agent,
        &payload,
        &HookEnv::from_process_env(),
        ghostmc_core::now(),
    ) else {
        return;
    };
    if send_to_host(&event).is_err()
        && let Ok(mut store) = ghostmc_core::store::Store::open_default()
    {
        let _ = store.ingest_event(&event);
    }
}

fn send_to_host(event: &AgentEvent) -> Result<(), ()> {
    let path = ghostmc_core::dirs::host_socket_path();
    let mut conn = ghostmc_host::client::HostConnection::connect(&path).map_err(|_| ())?;
    let env = RequestEnvelope {
        id: 1,
        protocol: PROTOCOL,
        request: Request::IngestEvent {
            event: event.clone(),
        },
    };
    match conn.roundtrip(&env).map_err(|_| ())?.body {
        ResponseBody::Ok { .. } => Ok(()),
        ResponseBody::Error { .. } => Err(()),
    }
}

fn exe() -> Result<String> {
    let p = std::env::current_exe()?;
    Ok(std::fs::canonicalize(&p)
        .unwrap_or(p)
        .to_string_lossy()
        .into_owned())
}

pub fn run(c: HookCmd, json: bool) -> Result<()> {
    match c {
        HookCmd::Ingest { agent } => {
            ingest_from_args(&["--agent".into(), agent]);
        }
        HookCmd::Install {
            agent,
            config,
            ghostmc,
        } => {
            let ghostmc = match ghostmc {
                Some(g) => g,
                None => exe()?,
            };
            for kind in agent.kinds() {
                let path = config
                    .clone()
                    .or_else(|| hooks::default_config_path(kind))
                    .ok_or_else(|| anyhow!("no home directory"))?;
                let change = hooks::install(&path, &hooks::hook_command(&ghostmc, kind), kind)?;
                println!(
                    "{}: {} {}{}",
                    kind.as_str(),
                    if change.changed {
                        "installed into"
                    } else {
                        "already installed in"
                    },
                    path.display(),
                    change
                        .backup
                        .map(|b| format!(" (backup: {})", b.display()))
                        .unwrap_or_default()
                );
                if kind == AgentKind::Codex && change.changed {
                    println!(
                        "  codex: review and trust the new hooks with `/hooks` inside Codex before they run"
                    );
                }
            }
        }
        HookCmd::Uninstall { agent, config } => {
            for kind in agent.kinds() {
                let path = config
                    .clone()
                    .or_else(|| hooks::default_config_path(kind))
                    .ok_or_else(|| anyhow!("no home directory"))?;
                let change = hooks::uninstall(&path)?;
                println!(
                    "{}: {} {}",
                    kind.as_str(),
                    if change.changed {
                        "removed ghostmc hooks from"
                    } else {
                        "no ghostmc hooks in"
                    },
                    path.display()
                );
            }
        }
        HookCmd::Status => {
            let mut out = serde_json::Map::new();
            for kind in [AgentKind::Claude, AgentKind::Codex] {
                let path =
                    hooks::default_config_path(kind).ok_or_else(|| anyhow!("no home directory"))?;
                let installed = hooks::status(&path, kind).unwrap_or(false);
                if !json {
                    println!(
                        "{:<7} {:<13} {}",
                        kind.as_str(),
                        if installed {
                            "installed"
                        } else {
                            "not installed"
                        },
                        path.display()
                    );
                }
                out.insert(
                    kind.as_str().into(),
                    serde_json::json!({ "installed": installed, "path": path }),
                );
            }
            if json {
                println!("{}", serde_json::Value::Object(out));
            }
        }
    }
    Ok(())
}
