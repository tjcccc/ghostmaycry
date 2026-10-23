//! Ghostty integration.
//!
//! macOS: Ghostty's AppleScript dictionary (Ghostty 1.3+; verified against
//! `macos/Ghostty.sdef`). Terminals expose a stable `id`, `working directory`,
//! foreground `pid` and `tty`. The `tty` is what lets us *verify* that a tmux
//! client is displayed in a given terminal (tmux reports `client_tty`).
//!
//! Linux: `ghostty +new-window` (D-Bus to a running GTK instance) or a new
//! `ghostty` process. Window enumeration/focus is not available, especially
//! on Wayland, and is reported as unsupported.
//!
//! Scripts receive all values through `argv`; nothing is interpolated into
//! AppleScript source.

use crate::{Capabilities, PlatformError, Result};
use ghostmc_core::config::TerminalConfig;
use serde::{Deserialize, Serialize};
use std::process::{Command, Stdio};

/// An existing Ghostty terminal surface. Ids are scoped to the running
/// Ghostty process and may go stale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GhosttyTerminal {
    pub window_id: String,
    pub tab_id: String,
    pub tab_index: u32,
    pub tab_selected: bool,
    pub terminal_id: String,
    pub title: String,
    /// Ghostty-reported cwd of the terminal's process. For a terminal running
    /// tmux this is *not* the active pane's cwd.
    pub working_directory: Option<String>,
    pub tty: Option<String>,
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Placement {
    NewWindow,
    /// New tab; `window_id: None` means the front window.
    NewTab {
        window_id: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Launched {
    /// Ghostty terminal id when the platform reports it (macOS).
    pub terminal_id: Option<String>,
    pub method: String,
}

pub fn capabilities(cfg: &TerminalConfig) -> Capabilities {
    let mut c = Capabilities {
        platform: std::env::consts::OS.into(),
        list_terminals: false,
        focus_terminal: false,
        new_window: true,
        new_tab: false,
        notes: Vec::new(),
    };
    if cfg.kind == "custom" {
        c.notes
            .push("custom terminal: launch only (no discovery or focus)".into());
        return c;
    }
    if cfg!(target_os = "macos") {
        c.list_terminals = true;
        c.focus_terminal = true;
        c.new_tab = true;
        c.notes
            .push("requires Ghostty 1.3+ and Automation permission".into());
    } else {
        c.notes.push(
            "Linux: Ghostty windows cannot be enumerated or focused by other apps; new windows only".into(),
        );
    }
    c
}

// --------------------------------------------------------------- AppleScript

const FS: char = '\u{1f}';
const RS: char = '\u{1e}';

const LIST_SCRIPT: &str = r#"
on safe(v)
  try
    if v is missing value then return ""
    return v as text
  on error
    return ""
  end try
end safe
on run argv
  if application "Ghostty" is not running then return "NOT_RUNNING"
  set fs to (character id 31)
  set rs to (character id 30)
  set out to ""
  tell application "Ghostty"
    repeat with w in windows
      set wid to my safe(id of w)
      repeat with t in tabs of w
        set tid to my safe(id of t)
        set tidx to my safe(index of t)
        set tsel to my safe(selected of t)
        repeat with term in terminals of t
          set out to out & wid & fs & tid & fs & tidx & fs & tsel & fs & my safe(id of term) & fs & my safe(name of term) & fs & my safe(working directory of term) & fs & my safe(tty of term) & fs & my safe(pid of term) & rs
        end repeat
      end repeat
    end repeat
  end tell
  return out
end run
"#;

const FOCUS_SCRIPT: &str = r#"
on run argv
  if application "Ghostty" is not running then return "NOT_RUNNING"
  set wanted to item 1 of argv
  tell application "Ghostty"
    set matches to (every terminal whose id is wanted)
    if (count of matches) is 0 then return "NOT_FOUND"
    focus (item 1 of matches)
    activate
  end tell
  return "OK"
end run
"#;

const LAUNCH_SCRIPT: &str = r#"
on run argv
  set cmd to item 1 of argv
  set cwd to item 2 of argv
  set mode to item 3 of argv
  set target to item 4 of argv
  tell application "Ghostty"
    activate
    set cfg to new surface configuration
    set command of cfg to cmd
    if cwd is not "" then set initial working directory of cfg to cwd
    if mode is "tab" then
      if target is "" then
        if (count of windows) is 0 then
          set w to new window with configuration cfg
          return id of focused terminal of selected tab of w
        end if
        set t to new tab in front window with configuration cfg
      else
        set ws to (every window whose id is target)
        if (count of ws) is 0 then return "NOT_FOUND"
        set t to new tab in (item 1 of ws) with configuration cfg
      end if
      return id of focused terminal of t
    else
      set w to new window with configuration cfg
      return id of focused terminal of selected tab of w
    end if
  end tell
end run
"#;

fn osascript(script: &str, args: &[&str]) -> Result<String> {
    let out = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .args(args)
        .output()
        .map_err(|e| PlatformError::Failed(format!("osascript: {e}")))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        // -1743: errAEEventNotPermitted (Automation/TCC denied).
        if stderr.contains("-1743") || stderr.contains("Not authorized") {
            return Err(PlatformError::AutomationDenied);
        }
        if stderr.contains("-1728")
            || stderr.contains("Can’t get application")
            || stderr.contains("-2700")
        {
            return Err(PlatformError::GhosttyMissing(stderr.trim().into()));
        }
        return Err(PlatformError::Failed(stderr.trim().into()));
    }
    let s = String::from_utf8_lossy(&out.stdout);
    Ok(s.strip_suffix('\n').unwrap_or(&s).to_owned())
}

/// Parse [`LIST_SCRIPT`] output.
pub fn parse_terminal_list(out: &str) -> Vec<GhosttyTerminal> {
    out.split(RS)
        .filter(|r| !r.trim().is_empty())
        .filter_map(|r| {
            let f: Vec<&str> = r.split(FS).collect();
            if f.len() != 9 {
                return None;
            }
            let opt = |s: &str| (!s.is_empty()).then(|| s.to_owned());
            Some(GhosttyTerminal {
                window_id: f[0].into(),
                tab_id: f[1].into(),
                tab_index: f[2].parse().unwrap_or(0),
                tab_selected: f[3] == "true",
                terminal_id: f[4].into(),
                title: f[5].into(),
                working_directory: opt(f[6]),
                tty: opt(f[7]),
                pid: f[8].parse().ok(),
            })
        })
        .collect()
}

/// Existing Ghostty terminals. Never launches Ghostty.
pub fn list_terminals(cfg: &TerminalConfig) -> Result<Vec<GhosttyTerminal>> {
    if cfg.kind == "custom" || !cfg!(target_os = "macos") {
        return Err(PlatformError::Unsupported("listing Ghostty terminals"));
    }
    let out = osascript(LIST_SCRIPT, &[])?;
    if out == "NOT_RUNNING" {
        return Ok(Vec::new());
    }
    Ok(parse_terminal_list(&out))
}

/// Focus an exact terminal by id. Sends no input.
pub fn focus_terminal(cfg: &TerminalConfig, terminal_id: &str) -> Result<()> {
    if cfg.kind == "custom" || !cfg!(target_os = "macos") {
        return Err(PlatformError::Unsupported("focusing a Ghostty terminal"));
    }
    match osascript(FOCUS_SCRIPT, &[terminal_id])?.as_str() {
        "OK" => Ok(()),
        "NOT_RUNNING" => Err(PlatformError::GhosttyNotRunning),
        _ => Err(PlatformError::NotFound(format!("terminal {terminal_id}"))),
    }
}

/// Open `argv` in a new Ghostty window/tab (or the configured terminal).
pub fn launch(
    cfg: &TerminalConfig,
    argv: &[String],
    cwd: Option<&str>,
    placement: &Placement,
) -> Result<Launched> {
    if argv.is_empty() {
        return Err(PlatformError::Failed("empty command".into()));
    }
    if cfg.kind == "custom" {
        if cfg.custom.is_empty() {
            return Err(PlatformError::Failed(
                "terminal.kind = \"custom\" but terminal.custom is empty".into(),
            ));
        }
        let mut full = cfg.custom.clone();
        full.extend_from_slice(argv);
        spawn_detached(&full, cwd)?;
        return Ok(Launched {
            terminal_id: None,
            method: "custom".into(),
        });
    }
    if cfg!(target_os = "macos") {
        // Ghostty runs a string `command` through the shell, so quote argv.
        let cmd = ghostmc_core::shell::join(argv);
        let (mode, target) = match placement {
            Placement::NewWindow => ("window", String::new()),
            Placement::NewTab { window_id } => ("tab", window_id.clone().unwrap_or_default()),
        };
        let id = osascript(LAUNCH_SCRIPT, &[&cmd, cwd.unwrap_or(""), mode, &target])?;
        if id == "NOT_FOUND" {
            return Err(PlatformError::NotFound(format!("window {target}")));
        }
        return Ok(Launched {
            terminal_id: Some(id).filter(|s| !s.is_empty()),
            method: format!("applescript:{mode}"),
        });
    }
    if matches!(placement, Placement::NewTab { .. }) {
        return Err(PlatformError::Unsupported(
            "opening a tab in an existing Ghostty window",
        ));
    }
    linux_launch(cfg, argv, cwd)
}

fn linux_launch(cfg: &TerminalConfig, argv: &[String], cwd: Option<&str>) -> Result<Launched> {
    let mut base: Vec<String> = vec![cfg.ghostty_binary.clone()];
    let mut flags = Vec::new();
    if let Some(d) = cwd {
        flags.push(format!("--working-directory={d}"));
    }
    flags.push("-e".into());
    flags.extend_from_slice(argv);
    if cfg.linux_mode == "new-window" {
        // Ask a running instance over D-Bus. `-e` makes every following
        // argument part of the command.
        let mut c = Command::new(&base[0]);
        c.arg("+new-window").args(&flags).stdin(Stdio::null());
        match c.output() {
            Ok(o) if o.status.success() => {
                return Ok(Launched {
                    terminal_id: None,
                    method: "ghostty +new-window".into(),
                });
            }
            Ok(_) | Err(_) => {} // no running instance / old Ghostty: fall back
        }
    }
    base.extend(flags);
    spawn_detached(&base, cwd)?;
    Ok(Launched {
        terminal_id: None,
        method: "ghostty process".into(),
    })
}

fn spawn_detached(argv: &[String], cwd: Option<&str>) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let mut c = Command::new(&argv[0]);
    c.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    c.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => PlatformError::GhosttyMissing(format!("{}: {e}", argv[0])),
        _ => PlatformError::Failed(format!("{}: {e}", argv[0])),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_applescript_records() {
        let raw = format!(
            "W1{FS}T1{FS}1{FS}true{FS}X1{FS}vim{FS}/w/app{FS}/dev/ttys004{FS}321{RS}W1{FS}T2{FS}2{FS}false{FS}X2{FS}zsh{FS}{FS}{FS}{RS}\n"
        );
        let v = parse_terminal_list(&raw);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].tty.as_deref(), Some("/dev/ttys004"));
        assert_eq!(v[0].pid, Some(321));
        assert!(v[0].tab_selected);
        assert_eq!(v[1].working_directory, None);
    }

    #[test]
    fn scripts_take_values_only_from_argv() {
        for s in [LIST_SCRIPT, FOCUS_SCRIPT, LAUNCH_SCRIPT] {
            assert!(s.contains("on run argv"));
        }
    }

    #[test]
    fn linux_capabilities_are_honest() {
        let c = capabilities(&TerminalConfig::default());
        if cfg!(target_os = "linux") {
            assert!(!c.focus_terminal && !c.list_terminals && !c.new_tab && c.new_window);
            assert!(list_terminals(&TerminalConfig::default()).is_err());
        }
    }

    #[test]
    fn custom_terminal_receives_argv() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let cfg = TerminalConfig {
            kind: "custom".into(),
            custom: vec![
                "sh".into(),
                "-c".into(),
                "printf '%s' \"$1\" > \"$0\"".into(),
            ],
            ..Default::default()
        };
        let arg = "it's $(dangerous)".to_string();
        launch(
            &cfg,
            &[marker.to_string_lossy().into_owned(), arg.clone()],
            None,
            &Placement::NewWindow,
        )
        .unwrap();
        for _ in 0..50 {
            if marker.exists() && !std::fs::read_to_string(&marker).unwrap().is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), arg);
    }
}
