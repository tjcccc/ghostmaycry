//! Coding-agent observations (Claude Code, Codex) and the evidence-based state
//! machine built from them.
//!
//! Hook payloads are untrusted, versioned by the agent, and may contain
//! sensitive text. We keep only what the dashboard needs: event names,
//! correlation ids and short notification messages. Prompts and assistant
//! messages are never stored; the short end-of-turn brief
//! ([`brief_from_hook`]) is handed to the host in memory only.

use crate::ids::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    Claude,
    Codex,
}

impl AgentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(AgentKind::Claude),
            "codex" => Some(AgentKind::Codex),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Working on a turn.
    Running,
    /// Blocked on a permission/approval prompt.
    NeedsApproval,
    /// Session open, waiting for a prompt.
    Idle,
    /// The agent stopped its turn. This is *not* "task succeeded".
    TurnEnded,
    /// The turn ended with an error reported by the agent.
    Failed,
    /// The agent session ended.
    Ended,
    /// The pane the agent ran in no longer exists.
    Disconnected,
    /// No fresh evidence; last known state too old to trust.
    Unknown,
}

impl AgentState {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Running => "running",
            AgentState::NeedsApproval => "needs_approval",
            AgentState::Idle => "idle",
            AgentState::TurnEnded => "turn_ended",
            AgentState::Failed => "failed",
            AgentState::Ended => "ended",
            AgentState::Disconnected => "disconnected",
            AgentState::Unknown => "unknown",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "running" => AgentState::Running,
            "needs_approval" => AgentState::NeedsApproval,
            "idle" => AgentState::Idle,
            "turn_ended" => AgentState::TurnEnded,
            "failed" => AgentState::Failed,
            "ended" => AgentState::Ended,
            "disconnected" => AgentState::Disconnected,
            _ => AgentState::Unknown,
        }
    }
    pub fn needs_attention(self) -> bool {
        matches!(self, AgentState::NeedsApproval | AgentState::Failed)
    }
}

/// How an agent run was tied to a terminal location.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attribution {
    /// The hook ran with `GHOSTMC_SESSION_TOKEN` from a session ghostmc made.
    Token,
    /// The hook ran with `$TMUX`/`$TMUX_PANE` set: exact pane.
    TmuxPane,
    /// Only the agent-reported cwd is known. Never definitive.
    CwdOnly,
}

impl Attribution {
    pub fn as_str(self) -> &'static str {
        match self {
            Attribution::Token => "token",
            Attribution::TmuxPane => "tmux_pane",
            Attribution::CwdOnly => "cwd_only",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "token" => Attribution::Token,
            "tmux_pane" => Attribution::TmuxPane,
            _ => Attribution::CwdOnly,
        }
    }
}

/// Terminal context captured from the hook process environment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookEnv {
    /// `$TMUX` is `socket_path,server_pid,session_index`.
    pub tmux: Option<String>,
    pub tmux_pane: Option<String>,
    pub token: Option<String>,
    /// Controlling terminal of the agent (see [`controlling_tty`]).
    pub tty: Option<String>,
    /// The process that owns `tty` (the agent, or its wrapper).
    pub tty_pid: Option<u32>,
}

impl HookEnv {
    pub fn from_process_env() -> Self {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Self {
            tmux: get("TMUX"),
            tmux_pane: get("TMUX_PANE"),
            token: get("GHOSTMC_SESSION_TOKEN"),
            tty: None,
            tty_pid: None,
        }
    }

    /// `(socket_path, server_pid)` parsed from `$TMUX`.
    pub fn tmux_server(&self) -> Option<(String, u32)> {
        let t = self.tmux.as_deref()?;
        // The socket path may itself contain commas; the pid and session
        // index are the last two fields.
        let mut parts = t.rsplitn(3, ',');
        let _session = parts.next()?;
        let pid = parts.next()?.parse().ok()?;
        let socket = parts.next()?;
        Some((socket.to_owned(), pid))
    }
}

const MAX_MESSAGE: usize = 200;
const MAX_BRIEF: usize = 600;
/// Ancestors searched for a controlling terminal: hook → shell → agent,
/// plus wrappers such as `npx` or version managers.
const MAX_TTY_HOPS: usize = 8;

/// Whether to look up the agent's terminal for this event. Tool events are
/// skipped: they are the most frequent and the terminal never changes within
/// a session.
pub fn wants_tty(hook_event: &str) -> bool {
    !matches!(
        hook_event,
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure"
    )
}

/// The terminal device (`/dev/ttys003`, `/dev/pts/2`) of the nearest ancestor
/// process that has one, and that process's pid. Agents start hooks without a
/// controlling terminal, so the hook's own is usually empty; the agent's is
/// the terminal it runs in. The pid lets readers tell a live run from a
/// closed tab whose tty number was handed out again. Uses `ps` with argv only.
pub fn controlling_tty() -> Option<(String, u32)> {
    let mut pid = std::process::id();
    for _ in 0..MAX_TTY_HOPS {
        let out = std::process::Command::new("ps")
            .args(["-o", "ppid=,tty=", "-p", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        let line = String::from_utf8_lossy(&out.stdout);
        let mut f = line.split_whitespace();
        let ppid: u32 = f.next()?.parse().ok()?;
        if let Some(tty) = f.next().and_then(tty_device) {
            return Some((tty, pid));
        }
        if ppid <= 1 {
            return None;
        }
        pid = ppid;
    }
    None
}

/// `ps` tty column → device path; `None` for "no terminal" (`??`, `?`, `-`).
fn tty_device(col: &str) -> Option<String> {
    let ok = !col.is_empty()
        && col.chars().all(|c| c.is_ascii_alphanumeric() || c == '/')
        && col.chars().any(|c| c.is_ascii_digit());
    ok.then(|| format!("/dev/{col}"))
}

/// The agent's final message of a turn (`Stop.last_assistant_message`, sent
/// by both Claude Code and Codex), cut to a short brief. Callers must keep it
/// in memory only: it is assistant text.
pub fn brief_from_hook(payload: &Value) -> Option<String> {
    if payload.get("hook_event_name").and_then(Value::as_str) != Some("Stop") {
        return None;
    }
    let text = str_field(payload, "last_assistant_message")?;
    let text = text.trim();
    (!text.is_empty()).then(|| truncate(text, MAX_BRIEF))
}

/// A normalized, privacy-reduced hook observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentEvent {
    pub id: EventId,
    pub ts: i64,
    pub agent: AgentKind,
    pub hook_event: String,
    pub agent_session_id: Option<String>,
    pub turn_id: Option<String>,
    pub cwd: Option<String>,
    pub tmux_socket: Option<String>,
    pub tmux_server_pid: Option<u32>,
    pub tmux_pane: Option<String>,
    pub token: Option<String>,
    /// Controlling terminal of the agent, when looked up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tty: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tty_pid: Option<u32>,
    /// Claude `notification_type`, Codex/Claude `source`/`reason`, tool name.
    pub detail: Option<String>,
    /// Short human message (Claude `Notification.message`), truncated.
    pub message: Option<String>,
    pub dedupe_key: String,
}

fn str_field(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

impl AgentEvent {
    /// Build from a hook's stdin JSON plus the hook process environment.
    pub fn from_hook(agent: AgentKind, payload: &Value, env: &HookEnv, ts: i64) -> Option<Self> {
        let hook_event = str_field(payload, "hook_event_name")?;
        let agent_session_id = str_field(payload, "session_id");
        let turn_id = str_field(payload, "turn_id");
        let detail = match hook_event.as_str() {
            "Notification" => str_field(payload, "notification_type"),
            "SessionStart" => str_field(payload, "source"),
            "SessionEnd" => str_field(payload, "reason"),
            "PreToolUse" | "PostToolUse" | "PostToolUseFailure" | "PermissionRequest" => {
                str_field(payload, "tool_name")
            }
            "StopFailure" => str_field(payload, "error").or_else(|| str_field(payload, "reason")),
            _ => None,
        };
        let message = match hook_event.as_str() {
            "Notification" => str_field(payload, "message").map(|m| truncate(&m, MAX_MESSAGE)),
            _ => None,
        };
        let (tmux_socket, tmux_server_pid) = match env.tmux_server() {
            Some((s, p)) => (Some(s), Some(p)),
            None => (None, None),
        };
        // Tool events carry a unique id; other events are deduplicated per
        // second, which absorbs double-registered hooks.
        let unique = str_field(payload, "tool_use_id")
            .map(|t| format!("tool:{t}"))
            .unwrap_or_else(|| format!("ts:{ts}"));
        let dedupe_key = format!(
            "{}|{}|{}|{}|{}|{}",
            agent.as_str(),
            agent_session_id.as_deref().unwrap_or("-"),
            hook_event,
            turn_id.as_deref().unwrap_or("-"),
            detail.as_deref().unwrap_or("-"),
            unique
        );
        Some(Self {
            id: EventId::generate(),
            ts,
            agent,
            hook_event,
            agent_session_id,
            turn_id,
            cwd: str_field(payload, "cwd"),
            tmux_socket,
            tmux_server_pid,
            tmux_pane: env.tmux_pane.clone(),
            token: env.token.clone(),
            tty: env.tty.clone(),
            tty_pid: env.tty_pid,
            detail,
            message,
            dedupe_key,
        })
    }

    pub fn attribution(&self) -> Attribution {
        if self.token.is_some() {
            Attribution::Token
        } else if self.tmux_pane.is_some() && self.tmux_socket.is_some() {
            Attribution::TmuxPane
        } else {
            Attribution::CwdOnly
        }
    }

    /// The state this event moves a run into, if any. Event names follow the
    /// verified Claude Code and Codex hook contracts; both agents share most
    /// names but not all (`StopFailure`, `Notification` are Claude-only).
    pub fn transition(&self) -> Option<AgentState> {
        match self.hook_event.as_str() {
            "SessionStart" => Some(AgentState::Idle),
            "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "PostToolUseFailure"
            | "PreCompact" | "PostCompact" | "SubagentStart" => Some(AgentState::Running),
            "PermissionRequest" => Some(AgentState::NeedsApproval),
            "Notification" => match self.detail.as_deref() {
                Some("permission_prompt") => Some(AgentState::NeedsApproval),
                _ => None,
            },
            "Stop" => Some(AgentState::TurnEnded),
            "StopFailure" => Some(AgentState::Failed),
            "SessionEnd" => Some(AgentState::Ended),
            _ => None,
        }
    }
}

/// Current knowledge about one agent session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRun {
    pub id: AgentRunId,
    pub agent: AgentKind,
    pub agent_session_id: String,
    pub cwd: Option<String>,
    pub tmux_socket: Option<String>,
    pub tmux_server_pid: Option<u32>,
    pub tmux_pane: Option<String>,
    pub token: Option<String>,
    /// Terminal the agent runs in. Outside tmux this is the Ghostty
    /// terminal's tty; inside tmux it is the pane's. Only meaningful while
    /// `tty_pid` is alive: tty numbers are reused.
    #[serde(default)]
    pub tty: Option<String>,
    #[serde(default)]
    pub tty_pid: Option<u32>,
    pub attribution: Attribution,
    pub state: AgentState,
    pub state_since: i64,
    pub last_event_at: i64,
    pub last_hook_event: String,
    pub last_message: Option<String>,
}

/// After this long without events a `Running` run is reported `Unknown`.
pub const RUNNING_STALE_AFTER: i64 = 30 * 60;

impl AgentRun {
    /// State to display, combining recorded state with liveness evidence.
    /// `pane_alive`: `Some(false)` when the recorded pane was verified gone.
    pub fn effective_state(&self, now: i64, pane_alive: Option<bool>) -> AgentState {
        if matches!(self.state, AgentState::Ended) {
            return AgentState::Ended;
        }
        if pane_alive == Some(false) {
            return AgentState::Disconnected;
        }
        if self.state == AgentState::Running && now - self.last_event_at > RUNNING_STALE_AFTER {
            return AgentState::Unknown;
        }
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env_in_tmux() -> HookEnv {
        HookEnv {
            tmux: Some("/tmp/tmux-501/default,4242,3".into()),
            tmux_pane: Some("%7".into()),
            token: None,
            tty: None,
            tty_pid: None,
        }
    }

    #[test]
    fn parses_tmux_env_with_commas_in_path() {
        let env = HookEnv {
            tmux: Some("/tmp/we,ird/sock,99,0".into()),
            ..Default::default()
        };
        assert_eq!(env.tmux_server(), Some(("/tmp/we,ird/sock".into(), 99)));
    }

    #[test]
    fn claude_notification_needs_approval() {
        let p = json!({
            "session_id": "s1", "cwd": "/w/app", "hook_event_name": "Notification",
            "message": "Claude needs your permission to use Bash",
            "notification_type": "permission_prompt"
        });
        let e = AgentEvent::from_hook(AgentKind::Claude, &p, &env_in_tmux(), 100).unwrap();
        assert_eq!(e.transition(), Some(AgentState::NeedsApproval));
        assert_eq!(e.attribution(), Attribution::TmuxPane);
        assert_eq!(e.tmux_server_pid, Some(4242));
        assert_eq!(e.tmux_pane.as_deref(), Some("%7"));
    }

    #[test]
    fn idle_notification_does_not_change_state() {
        let p = json!({"session_id": "s1", "hook_event_name": "Notification", "notification_type": "idle_prompt"});
        let e = AgentEvent::from_hook(AgentKind::Claude, &p, &HookEnv::default(), 1).unwrap();
        assert_eq!(e.transition(), None);
        assert_eq!(e.attribution(), Attribution::CwdOnly);
    }

    #[test]
    fn codex_stop_is_turn_end_not_success() {
        let p = json!({
            "session_id": "th1", "turn_id": "t9", "cwd": "/w", "hook_event_name": "Stop",
            "model": "gpt", "permission_mode": "default", "stop_hook_active": false,
            "transcript_path": null, "last_assistant_message": "secret details"
        });
        let e = AgentEvent::from_hook(AgentKind::Codex, &p, &HookEnv::default(), 1).unwrap();
        assert_eq!(e.transition(), Some(AgentState::TurnEnded));
        // Assistant text never enters the stored event; only the separate
        // in-memory brief carries it.
        assert!(!serde_json::to_string(&e).unwrap().contains("secret"));
        assert_eq!(brief_from_hook(&p).as_deref(), Some("secret details"));
    }

    #[test]
    fn brief_only_from_stop_and_truncated() {
        let p = json!({"hook_event_name": "Notification", "last_assistant_message": "x"});
        assert_eq!(brief_from_hook(&p), None);
        let p = json!({"hook_event_name": "Stop", "last_assistant_message": "  "});
        assert_eq!(brief_from_hook(&p), None);
        let p = json!({"hook_event_name": "Stop", "last_assistant_message": "y".repeat(5000)});
        assert_eq!(brief_from_hook(&p).unwrap().chars().count(), MAX_BRIEF + 1);
    }

    #[test]
    fn ps_tty_column_maps_to_device() {
        assert_eq!(tty_device("ttys004").as_deref(), Some("/dev/ttys004"));
        assert_eq!(tty_device("pts/3").as_deref(), Some("/dev/pts/3"));
        assert_eq!(tty_device("??"), None);
        assert_eq!(tty_device("?"), None);
        assert_eq!(tty_device("-"), None);
        assert_eq!(tty_device("../x1"), None);
    }

    #[test]
    fn prompts_are_not_stored() {
        let p = json!({"session_id": "s", "hook_event_name": "UserPromptSubmit", "prompt": "my API key is hunter2"});
        let e = AgentEvent::from_hook(AgentKind::Claude, &p, &HookEnv::default(), 1).unwrap();
        assert!(!serde_json::to_string(&e).unwrap().contains("hunter2"));
    }

    #[test]
    fn long_messages_are_truncated() {
        let long = "x".repeat(1000);
        let p = json!({"session_id": "s", "hook_event_name": "Notification", "message": long});
        let e = AgentEvent::from_hook(AgentKind::Claude, &p, &HookEnv::default(), 1).unwrap();
        assert!(e.message.unwrap().chars().count() <= MAX_MESSAGE + 1);
    }

    #[test]
    fn missing_event_name_is_rejected() {
        assert!(
            AgentEvent::from_hook(AgentKind::Claude, &json!({}), &HookEnv::default(), 1).is_none()
        );
    }

    #[test]
    fn token_wins_attribution() {
        let env = HookEnv {
            token: Some("tok".into()),
            ..env_in_tmux()
        };
        let p = json!({"session_id": "s", "hook_event_name": "Stop"});
        let e = AgentEvent::from_hook(AgentKind::Claude, &p, &env, 1).unwrap();
        assert_eq!(e.attribution(), Attribution::Token);
    }

    #[test]
    fn effective_state_rules() {
        let run = AgentRun {
            id: AgentRunId::generate(),
            agent: AgentKind::Claude,
            agent_session_id: "s".into(),
            cwd: None,
            tmux_socket: None,
            tmux_server_pid: None,
            tmux_pane: None,
            token: None,
            tty: None,
            tty_pid: None,
            attribution: Attribution::CwdOnly,
            state: AgentState::Running,
            state_since: 0,
            last_event_at: 0,
            last_hook_event: "PostToolUse".into(),
            last_message: None,
        };
        assert_eq!(run.effective_state(10, None), AgentState::Running);
        assert_eq!(
            run.effective_state(RUNNING_STALE_AFTER + 1, None),
            AgentState::Unknown
        );
        assert_eq!(
            run.effective_state(10, Some(false)),
            AgentState::Disconnected
        );
    }
}
