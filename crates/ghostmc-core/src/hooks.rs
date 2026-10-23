//! Non-destructive installation of ghostmc hooks into Claude Code
//! (`~/.claude/settings.json`) and Codex (`~/.codex/hooks.json`).
//!
//! Both agents use the same shape:
//! `{"hooks": {"<Event>": [{"matcher": "...", "hooks": [{"type": "command", "command": "..."}]}]}}`.
//! We only ever add or remove handlers whose command contains [`MARKER`];
//! every other key and handler is preserved byte-for-byte in order. A backup
//! is written before the first modification.

use crate::agent::AgentKind;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// Substring identifying handlers we own.
pub const MARKER: &str = "hook ingest --agent";

/// Events registered per agent. Verified against Claude Code's hook reference
/// and Codex's `codex-rs/hooks` schemas (see ADR). Claude-only events
/// (`Notification`, `StopFailure`) are not registered for Codex.
pub fn events_for(agent: AgentKind) -> &'static [&'static str] {
    match agent {
        AgentKind::Claude => &[
            "SessionStart",
            "UserPromptSubmit",
            "PostToolUse",
            "PermissionRequest",
            "Notification",
            "Stop",
            "StopFailure",
            "SessionEnd",
        ],
        AgentKind::Codex => &[
            "SessionStart",
            "UserPromptSubmit",
            "PostToolUse",
            "PermissionRequest",
            "Stop",
            "SessionEnd",
        ],
    }
}

pub fn default_config_path(agent: AgentKind) -> Option<PathBuf> {
    let home = ::dirs::home_dir()?;
    Some(match agent {
        AgentKind::Claude => home.join(".claude").join("settings.json"),
        AgentKind::Codex => std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"))
            .join("hooks.json"),
    })
}

/// Command line the agent will run for each event.
pub fn hook_command(ghostmc_exe: &str, agent: AgentKind) -> String {
    format!(
        "{} hook ingest --agent {}",
        crate::shell::quote(ghostmc_exe),
        agent.as_str()
    )
}

fn is_ours(handler: &Value) -> bool {
    handler
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|c| c.contains(MARKER))
}

#[derive(Debug, thiserror::Error)]
pub enum HookConfigError {
    #[error("{path}: {msg}")]
    Invalid { path: String, msg: String },
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// Pure transformation: add our handler to every event, replacing an older
/// ghostmc handler if present. Returns whether anything changed.
pub fn install_into(doc: &mut Value, command: &str, events: &[&str]) -> Result<bool, String> {
    let root = doc
        .as_object_mut()
        .ok_or("top level is not a JSON object")?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or("\"hooks\" is not an object")?;
    let mut changed = false;
    for ev in events {
        let groups = hooks
            .entry(ev.to_string())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| format!("hooks.{ev} is not an array"))?;
        let mut found = false;
        for g in groups.iter_mut() {
            if let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                for h in hs.iter_mut().filter(|h| is_ours(h)) {
                    found = true;
                    if h.get("command").and_then(Value::as_str) != Some(command) {
                        h["command"] = json!(command);
                        changed = true;
                    }
                }
            }
        }
        if !found {
            groups.push(json!({
                "hooks": [{ "type": "command", "command": command, "timeout": 10 }]
            }));
            changed = true;
        }
    }
    Ok(changed)
}

/// Pure transformation: remove only our handlers, then any group/event we
/// emptied. Returns whether anything changed.
pub fn uninstall_from(doc: &mut Value) -> bool {
    let Some(hooks) = doc.get_mut("hooks").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut changed = false;
    for groups in hooks.values_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        for g in groups.iter_mut() {
            if let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                let before = hs.len();
                hs.retain(|h| !is_ours(h));
                if hs.len() != before {
                    changed = true;
                }
            }
        }
        groups.retain(|g| {
            g.get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|h| !h.is_empty())
        });
    }
    if changed {
        hooks.retain(|_, v| v.as_array().is_none_or(|a| !a.is_empty()));
        if hooks.is_empty() {
            doc.as_object_mut().map(|o| o.remove("hooks"));
        }
    }
    changed
}

pub fn is_installed(doc: &Value, events: &[&str]) -> bool {
    events.iter().all(|ev| {
        doc.pointer(&format!("/hooks/{ev}"))
            .and_then(Value::as_array)
            .is_some_and(|gs| {
                gs.iter().any(|g| {
                    g.get("hooks")
                        .and_then(Value::as_array)
                        .is_some_and(|hs| hs.iter().any(is_ours))
                })
            })
    })
}

fn read_doc(path: &Path) -> Result<Option<Value>, HookConfigError> {
    match std::fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(Some(json!({}))),
        Ok(s) => serde_json::from_str(&s)
            .map(Some)
            .map_err(|e| HookConfigError::Invalid {
                path: path.display().to_string(),
                msg: format!("not valid JSON ({e}); refusing to modify"),
            }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn write_atomic(path: &Path, doc: &Value) -> Result<(), HookConfigError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.ghostmc-tmp");
    let mut text = serde_json::to_string_pretty(doc).expect("serializable");
    text.push('\n');
    std::fs::write(&tmp, text)?;
    // Keep the original file mode.
    if let Ok(m) = std::fs::metadata(path) {
        std::fs::set_permissions(&tmp, m.permissions())?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookChange {
    pub path: PathBuf,
    pub changed: bool,
    pub backup: Option<PathBuf>,
}

pub fn install(
    path: &Path,
    command: &str,
    agent: AgentKind,
) -> Result<HookChange, HookConfigError> {
    let existing = read_doc(path)?;
    let mut doc = existing.clone().unwrap_or_else(|| json!({}));
    let changed = install_into(&mut doc, command, events_for(agent)).map_err(|msg| {
        HookConfigError::Invalid {
            path: path.display().to_string(),
            msg,
        }
    })?;
    let mut backup = None;
    if changed {
        if existing.is_some() {
            let b = path.with_extension(format!("json.ghostmc-backup-{}", crate::now()));
            std::fs::copy(path, &b)?;
            backup = Some(b);
        }
        write_atomic(path, &doc)?;
    }
    Ok(HookChange {
        path: path.into(),
        changed,
        backup,
    })
}

pub fn uninstall(path: &Path) -> Result<HookChange, HookConfigError> {
    let Some(mut doc) = read_doc(path)? else {
        return Ok(HookChange {
            path: path.into(),
            changed: false,
            backup: None,
        });
    };
    let changed = uninstall_from(&mut doc);
    if changed {
        write_atomic(path, &doc)?;
    }
    Ok(HookChange {
        path: path.into(),
        changed,
        backup: None,
    })
}

pub fn status(path: &Path, agent: AgentKind) -> Result<bool, HookConfigError> {
    Ok(read_doc(path)?.is_some_and(|d| is_installed(&d, events_for(agent))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_settings() -> Value {
        json!({
            "model": "opus",
            "permissions": {"allow": ["Bash(git *)"]},
            "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "say done"}]}],
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "guard.sh"}]}]
            }
        })
    }

    #[test]
    fn install_preserves_user_config_and_is_idempotent() {
        let mut doc = user_settings();
        let cmd = hook_command("/usr/local/bin/ghostmc", AgentKind::Claude);
        assert!(install_into(&mut doc, &cmd, events_for(AgentKind::Claude)).unwrap());
        assert!(!install_into(&mut doc, &cmd, events_for(AgentKind::Claude)).unwrap());
        assert_eq!(doc["model"], "opus");
        assert_eq!(doc["hooks"]["Stop"][0]["hooks"][0]["command"], "say done");
        assert_eq!(doc["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert_eq!(doc["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert!(is_installed(&doc, events_for(AgentKind::Claude)));
        // Key order is preserved (user's keys stay first).
        let keys: Vec<_> = doc.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, vec!["model", "permissions", "hooks"]);
    }

    #[test]
    fn reinstall_updates_moved_binary() {
        let mut doc = json!({});
        install_into(
            &mut doc,
            &hook_command("/old/ghostmc", AgentKind::Codex),
            events_for(AgentKind::Codex),
        )
        .unwrap();
        let new = hook_command("/new/ghostmc", AgentKind::Codex);
        assert!(install_into(&mut doc, &new, events_for(AgentKind::Codex)).unwrap());
        assert_eq!(doc["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert_eq!(doc["hooks"]["Stop"][0]["hooks"][0]["command"], new.as_str());
    }

    #[test]
    fn uninstall_restores_original() {
        let original = user_settings();
        let mut doc = original.clone();
        install_into(
            &mut doc,
            &hook_command("ghostmc", AgentKind::Claude),
            events_for(AgentKind::Claude),
        )
        .unwrap();
        assert!(uninstall_from(&mut doc));
        assert_eq!(doc, original);
        assert!(!uninstall_from(&mut doc));
    }

    #[test]
    fn uninstall_removes_hooks_key_we_created() {
        let mut doc = json!({"model": "x"});
        install_into(&mut doc, "ghostmc hook ingest --agent claude", &["Stop"]).unwrap();
        uninstall_from(&mut doc);
        assert_eq!(doc, json!({"model": "x"}));
    }

    #[test]
    fn file_roundtrip_with_backup_and_invalid_json_refusal() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("settings.json");
        std::fs::write(&p, serde_json::to_string(&user_settings()).unwrap()).unwrap();
        let c = install(&p, "ghostmc hook ingest --agent claude", AgentKind::Claude).unwrap();
        assert!(c.changed);
        let backup = c.backup.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&std::fs::read_to_string(&backup).unwrap()).unwrap(),
            user_settings()
        );
        assert!(status(&p, AgentKind::Claude).unwrap());
        uninstall(&p).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&std::fs::read_to_string(&p).unwrap()).unwrap(),
            user_settings()
        );
        std::fs::write(&p, "{ not json").unwrap();
        assert!(install(&p, "x hook ingest --agent claude", AgentKind::Claude).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{ not json");
        // Missing file: created fresh, no backup.
        let fresh = d.path().join("codex/hooks.json");
        let c = install(
            &fresh,
            "ghostmc hook ingest --agent codex",
            AgentKind::Codex,
        )
        .unwrap();
        assert!(c.changed && c.backup.is_none());
    }
}
