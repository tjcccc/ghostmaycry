//! `ghostmc host …`: run the host, or install it as a per-user service
//! (systemd --user on Linux, a LaunchAgent on macOS). Never system-wide.

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use ghostmc_host::Service;
use ghostmc_protocol::*;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

#[derive(Subcommand)]
pub enum HostCmd {
    /// Run the host in the foreground.
    Run,
    /// Install the per-user service definition.
    Install {
        /// Start it now.
        #[arg(long)]
        start: bool,
        /// Start it automatically at login (opt-in).
        #[arg(long)]
        at_login: bool,
    },
    /// Stop and remove the per-user service.
    Uninstall,
    /// Is the host running?
    Status,
}

const UNIT: &str = "ghostmc-host.service";
const LABEL: &str = "com.ghostmaycry.host";

fn exe() -> Result<String> {
    let p = std::env::current_exe()?;
    Ok(std::fs::canonicalize(&p)
        .unwrap_or(p)
        .to_string_lossy()
        .into_owned())
}

fn unit_path() -> Result<PathBuf> {
    Ok(dirs_config()?.join("systemd/user").join(UNIT))
}

fn plist_path() -> Result<PathBuf> {
    Ok(home()?
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn dirs_config() -> Result<PathBuf> {
    Ok(std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or(home()?.join(".config")))
}

fn sh(argv: &[&str]) -> Result<()> {
    let st = Command::new(argv[0])
        .args(&argv[1..])
        .status()
        .with_context(|| argv[0].to_string())?;
    if !st.success() {
        bail!("`{}` failed ({st})", argv.join(" "));
    }
    Ok(())
}

pub fn systemd_unit(exe: &str) -> String {
    format!(
        "[Unit]\nDescription=Ghost May Cry host (ghostmc)\n\n[Service]\nExecStart={} host run\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n",
        ghostmc_core::shell::quote(exe)
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn launch_agent(exe: &str, at_login: bool) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>host</string><string>run</string></array>
  <key>RunAtLoad</key><{}/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ProcessType</key><string>Background</string>
</dict>
</plist>
"#,
        xml_escape(exe),
        if at_login { "true" } else { "false" }
    )
}

fn notifier() -> ghostmc_host::service::Notifier {
    let exe = exe().ok();
    Box::new(move |v: &AgentView| {
        let who = agent_label(v.run.agent);
        let project = v
            .project_name
            .clone()
            .unwrap_or_else(|| v.run.cwd.clone().unwrap_or_default());
        let what = match v.state {
            ghostmc_core::agent::AgentState::NeedsApproval => "needs your approval",
            ghostmc_core::agent::AgentState::Failed => "stopped with an error",
            ghostmc_core::agent::AgentState::TurnEnded => "finished its turn",
            _ => return,
        };
        let title = format!("{who} {what}");
        let body = match &v.run.last_message {
            Some(m) => format!("{project}: {m}"),
            None => project,
        };
        match (&v.session_key, &exe) {
            (Some(key), Some(exe)) => {
                let (key, exe) = (key.clone(), exe.clone());
                ghostmc_platform::notify::notify_actionable(&title, &body, move || {
                    let _ = Command::new(exe).args(["attach", &key]).spawn();
                });
            }
            _ => ghostmc_platform::notify::notify(&title, &body),
        }
    })
}

pub fn run(c: HostCmd, json: bool) -> Result<()> {
    match c {
        HostCmd::Run => {
            let svc = Service::open_default(true)
                .map_err(anyhow::Error::msg)?
                .with_notifier(notifier());
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?;
            rt.block_on(ghostmc_host::server::run(
                Arc::new(svc),
                &ghostmc_core::dirs::host_socket_path(),
            ))?;
        }
        HostCmd::Install { start, at_login } => {
            let exe = exe()?;
            if cfg!(target_os = "macos") {
                let p = plist_path()?;
                std::fs::create_dir_all(p.parent().unwrap())?;
                std::fs::write(&p, launch_agent(&exe, at_login))?;
                println!("wrote {}", p.display());
                if start || at_login {
                    let uid = unsafe { libc::getuid() };
                    let _ = sh(&["launchctl", "bootout", &format!("gui/{uid}/{LABEL}")]);
                    sh(&[
                        "launchctl",
                        "bootstrap",
                        &format!("gui/{uid}"),
                        &p.to_string_lossy(),
                    ])?;
                    if start {
                        sh(&["launchctl", "kickstart", &format!("gui/{uid}/{LABEL}")])?;
                    }
                    println!("loaded {LABEL}");
                } else {
                    println!("start it with: ghostmc host install --start");
                }
            } else {
                let p = unit_path()?;
                std::fs::create_dir_all(p.parent().unwrap())?;
                std::fs::write(&p, systemd_unit(&exe))?;
                println!("wrote {}", p.display());
                sh(&["systemctl", "--user", "daemon-reload"])?;
                if at_login {
                    sh(&["systemctl", "--user", "enable", UNIT])?;
                }
                if start {
                    sh(&["systemctl", "--user", "start", UNIT])?;
                }
                if !start && !at_login {
                    println!("start it with: systemctl --user start {UNIT}");
                }
            }
        }
        HostCmd::Uninstall => {
            if cfg!(target_os = "macos") {
                let uid = unsafe { libc::getuid() };
                let _ = sh(&["launchctl", "bootout", &format!("gui/{uid}/{LABEL}")]);
                let p = plist_path()?;
                if p.exists() {
                    std::fs::remove_file(&p)?;
                }
                println!("removed {}", p.display());
            } else {
                let _ = sh(&["systemctl", "--user", "disable", "--now", UNIT]);
                let p = unit_path()?;
                if p.exists() {
                    std::fs::remove_file(&p)?;
                    let _ = sh(&["systemctl", "--user", "daemon-reload"]);
                }
                println!("removed {}", p.display());
            }
        }
        HostCmd::Status => {
            let sock = ghostmc_core::dirs::host_socket_path();
            let running = ghostmc_host::client::HostConnection::connect(&sock).is_ok();
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "running": running, "socket": sock })
                );
            } else if running {
                println!("host running ({})", sock.display());
            } else {
                println!("host not running; CLI commands run in direct mode");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_files_quote_paths() {
        assert!(
            systemd_unit("/opt/my apps/ghostmc")
                .contains("ExecStart='/opt/my apps/ghostmc' host run")
        );
        let p = launch_agent("/a&b/ghostmc", false);
        assert!(
            p.contains("<string>/a&amp;b/ghostmc</string>")
                && p.contains("<key>RunAtLoad</key><false/>")
        );
    }
}
