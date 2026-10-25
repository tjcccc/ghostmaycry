//! Typed client ↔ host protocol.
//!
//! Transport-agnostic: the same JSON envelopes travel over the local Unix
//! socket (one JSON object per line) and over `ssh <host> ghostmc api`
//! (one request on stdin, one response on stdout).
//!
//! Compatibility: peers exchange [`ProtocolVersion`]. Same major = compatible;
//! a newer minor may add methods/fields (unknown fields are ignored, unknown
//! methods return `unsupported`). Stored data is never touched by protocol
//! negotiation.

use ghostmc_core::agent::{AgentKind, AgentRun, AgentState, Attribution};
use ghostmc_core::files::Listing;
use ghostmc_core::git::{Diff, GitStatus};
use ghostmc_core::matching::Association;
use ghostmc_core::model::{Device, GitIdentity, Project, ProjectLocation};
use ghostmc_core::{LocationId, agent::AgentEvent};
use ghostmc_platform::Capabilities;
use ghostmc_platform::ghostty::GhosttyTerminal;
use ghostmc_tmux::AttachMode;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolVersion {
    pub major: u32,
    pub minor: u32,
}

pub const PROTOCOL: ProtocolVersion = ProtocolVersion { major: 1, minor: 1 };

impl ProtocolVersion {
    pub fn compatible_with(self, other: ProtocolVersion) -> bool {
        self.major == other.major
    }
}

/// Request envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub id: u64,
    pub protocol: ProtocolVersion,
    #[serde(flatten)]
    pub request: Request,
}

/// Response envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    pub id: u64,
    pub protocol: ProtocolVersion,
    #[serde(flatten)]
    pub body: ResponseBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ResponseBody {
    Ok { result: Value },
    Error { error: ApiError },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotFound,
    Ambiguous,
    Conflict,
    Invalid,
    Unsupported,
    Forbidden,
    IncompatibleProtocol,
    Unavailable,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Whether a remote caller should be able to run a method without
/// `--allow-write`. This is a UX guard against accidents, not a security
/// boundary: an SSH user with a shell can do anything the account can.
pub fn is_read_only(r: &Request) -> bool {
    matches!(
        r,
        Request::Status
            | Request::ListProjects
            | Request::GetProject { .. }
            | Request::ListDevices
            | Request::Sessions { .. }
            | Request::OpenPlan { .. }
            | Request::PathPlan { .. }
            | Request::ResolveSession { .. }
            | Request::AttachCommand { .. }
            | Request::AgentRuns { .. }
            | Request::RecentEvents { .. }
            | Request::GitStatus { .. }
            | Request::GitDiff { .. }
            | Request::ListFiles { .. }
            | Request::WaitForChange { .. }
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CreateIntent {
    /// Return the existing ghostmc session for the location if there is one.
    #[default]
    Reuse,
    /// The user explicitly asked for another session.
    Additional,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    Status,
    ListProjects,
    GetProject {
        key: String,
    },
    CreateProject {
        name: String,
        #[serde(default)]
        tags: Vec<String>,
        notes: Option<String>,
        /// Local path to register as the first location.
        path: Option<String>,
    },
    UpdateProject {
        key: String,
        name: Option<String>,
        notes: Option<String>,
        pinned: Option<bool>,
        #[serde(default)]
        add_tags: Vec<String>,
        #[serde(default)]
        remove_tags: Vec<String>,
    },
    DeleteProject {
        key: String,
    },
    /// Bind a project to a path. `device: None` = this device (path is
    /// normalized); for a remote device the path is stored verbatim.
    AddLocation {
        project: String,
        path: String,
        device: Option<String>,
        default_command: Option<String>,
    },
    RemoveLocation {
        location_id: String,
    },
    ListDevices,
    AddDevice {
        name: String,
        ssh_target: String,
        remote_ghostmc: Option<String>,
    },
    RemoveDevice {
        key: String,
    },
    /// Live tmux sessions on this device, optionally filtered to a project.
    Sessions {
        project: Option<String>,
    },
    /// Everything needed to decide how to open a project's terminal here.
    OpenPlan {
        project: String,
        location_id: Option<String>,
    },
    /// tmux sessions associated with a path on this device. Used by remote
    /// clients: registries are per-device, so a remote client identifies the
    /// project location by path rather than by this device's project ids.
    PathPlan {
        path: String,
    },
    /// Find a live session from a key, `$id`, or name.
    ResolveSession {
        spec: String,
        socket: Option<String>,
    },
    CreateSession {
        location_id: String,
        #[serde(default)]
        intent: CreateIntent,
        /// Explicit command for the first pane (argv). Empty = login shell.
        #[serde(default)]
        command: Vec<String>,
    },
    /// argv that attaches to a session on this device.
    AttachCommand {
        key: String,
        mode: AttachMode,
    },
    AgentRuns {
        since_secs: Option<i64>,
        /// Include the in-memory end-of-turn briefs (assistant text). Only
        /// the local desktop asks; `ghostmc api` always clears it.
        #[serde(default)]
        briefs: bool,
    },
    RecentEvents {
        limit: Option<usize>,
    },
    /// Used by `ghostmc hook ingest` when the host is running.
    IngestEvent {
        /// Boxed only to keep `Request` small; the wire format is unchanged.
        event: Box<AgentEvent>,
        /// End-of-turn brief from the same hook. The host keeps it in memory
        /// only; it is never written to the registry.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        brief: Option<String>,
    },
    GitStatus {
        location_id: String,
    },
    GitDiff {
        location_id: String,
        path: Option<String>,
        #[serde(default)]
        staged: bool,
    },
    ListFiles {
        location_id: String,
        #[serde(default)]
        rel: String,
        #[serde(default)]
        show_hidden: bool,
    },
    /// Long-poll: returns when the host's change counter exceeds `after`, or
    /// after `timeout_ms`.
    WaitForChange {
        after: u64,
        timeout_ms: u64,
    },
}

// ------------------------------------------------------------------ results

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusInfo {
    pub ghostmc_version: String,
    pub protocol: ProtocolVersion,
    pub schema_version: i64,
    pub device: Device,
    /// `true` when answered by the long-running host, `false` for direct mode.
    pub host_running: bool,
    pub host_pid: Option<u32>,
    pub tmux_version: Option<String>,
    pub tmux_binary: String,
    pub sockets: Vec<String>,
    pub db_path: String,
    pub change_counter: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub project: Project,
    pub locations: Vec<LocationView>,
    pub live_sessions: usize,
    pub attention: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationView {
    pub location: ProjectLocation,
    pub device_name: String,
    pub is_local: bool,
    /// Live Git facts for local locations.
    pub git: Option<GitIdentity>,
    pub exists: Option<bool>,
}

/// Stable reference to a tmux session for one server lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionRef {
    pub socket_path: String,
    pub server_pid: u32,
    pub server_start: i64,
    pub session_id: String,
}

impl SessionRef {
    /// `tmux:<server_pid>:<server_start>:<session_id>:<socket_path>`. The
    /// socket path goes last because it may contain `:`.
    pub fn key(&self) -> String {
        format!(
            "tmux:{}:{}:{}:{}",
            self.server_pid, self.server_start, self.session_id, self.socket_path
        )
    }

    pub fn parse_key(key: &str) -> Option<Self> {
        let rest = key.strip_prefix("tmux:")?;
        let mut it = rest.splitn(4, ':');
        let server_pid = it.next()?.parse().ok()?;
        let server_start = it.next()?.parse().ok()?;
        let session_id = it.next()?.to_owned();
        let socket_path = it.next()?.to_owned();
        if !session_id.starts_with('$') || socket_path.is_empty() {
            return None;
        }
        Some(Self {
            socket_path,
            server_pid,
            server_start,
            session_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientView {
    pub tty: String,
    pub readonly: bool,
    pub width: u32,
    pub height: u32,
    /// Ghostty terminal displaying this client, verified by matching tty.
    /// Filled in by the desktop-side adapter, never guessed by the host.
    pub ghostty_terminal_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneView {
    pub id: String,
    pub window_id: String,
    pub window_name: String,
    pub active: bool,
    pub cwd: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentView {
    pub run: AgentRun,
    pub state: AgentState,
    pub project_name: Option<String>,
    /// Session the run was attributed to, with the evidence used.
    pub session_key: Option<String>,
    pub pane_id: Option<String>,
    /// Short end-of-turn brief, when requested and the host still holds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brief: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionView {
    pub key: String,
    pub r#ref: SessionRef,
    pub name: String,
    pub created: i64,
    pub clients: Vec<ClientView>,
    pub panes: Vec<PaneView>,
    /// Created by ghostmc (tagged); otherwise a user-made tmux session.
    pub managed: bool,
    pub project_id: Option<String>,
    pub project_name: Option<String>,
    pub location_id: Option<String>,
    /// How this session relates to the location being opened (OpenPlan only).
    pub association: Option<Association>,
    /// Agents whose hooks reported this session's panes.
    pub agents: Vec<AgentView>,
    /// Absolute tmux binary on the session's device, for attach commands.
    pub tmux_binary: String,
}

impl SessionView {
    pub fn is_attached(&self) -> bool {
        !self.clients.is_empty()
    }
    pub fn active_pane(&self) -> Option<&PaneView> {
        self.panes.iter().find(|p| p.active).or(self.panes.first())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GhosttyView {
    pub terminal: GhosttyTerminal,
    /// Set when this terminal's tty is an attached tmux client: the terminal
    /// is a *view* of that session, not an unmanaged shell.
    pub tmux_session_key: Option<String>,
    /// Relation of the terminal's reported cwd to the location (candidates
    /// only; never used as identity).
    pub relation: Option<ghostmc_core::paths::PathRelation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenPlan {
    pub project: Project,
    pub location: LocationView,
    /// Other locations of the project on this device.
    pub other_locations: Vec<LocationView>,
    /// Remotely attachable tmux sessions, best association first.
    pub tmux: Vec<SessionView>,
    /// Unmanaged Ghostty terminals (desktop side fills this in).
    pub ghostty: Vec<GhosttyView>,
    pub ghostty_error: Option<String>,
    pub capabilities: Option<Capabilities>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathPlanResult {
    /// Path as normalized on the answering device.
    pub path: String,
    pub exists: bool,
    pub tmux: Vec<SessionView>,
    pub device: Device,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatedSession {
    /// `false` when an existing ghostmc session was reused.
    pub created: bool,
    pub session: SessionView,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachCommand {
    pub argv: Vec<String>,
    pub mode: AttachMode,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Changed {
    pub counter: u64,
}

/// Brief agent label used in UIs.
pub fn agent_label(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Claude => "Claude Code",
        AgentKind::Codex => "Codex",
    }
}

pub fn attribution_label(a: Attribution) -> &'static str {
    match a {
        Attribution::Token => "verified (ghostmc session token)",
        Attribution::TmuxPane => "verified (tmux pane from hook env)",
        Attribution::CwdOnly => "unverified (cwd only)",
    }
}

/// Typed results by method. Clients use these to decode `result`.
pub type FilesResult = Listing;
pub type GitStatusResult = GitStatus;
pub type GitDiffResult = Diff;
pub type LocationResult = ProjectLocation;
pub type LocationIdRef = LocationId;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_key_roundtrip_with_colons() {
        let r = SessionRef {
            socket_path: "/tmp/odd:dir/default".into(),
            server_pid: 42,
            server_start: 1700000000,
            session_id: "$3".into(),
        };
        assert_eq!(SessionRef::parse_key(&r.key()), Some(r));
        assert_eq!(SessionRef::parse_key("tmux:x:1:$3:/s"), None);
        assert_eq!(SessionRef::parse_key("tmux:1:1:3:/s"), None);
    }

    #[test]
    fn envelope_wire_format() {
        let e = RequestEnvelope {
            id: 7,
            protocol: PROTOCOL,
            request: Request::GetProject { key: "x".into() },
        };
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(
            s,
            r#"{"id":7,"protocol":{"major":1,"minor":1},"method":"get_project","params":{"key":"x"}}"#
        );
        let unit = serde_json::to_string(&RequestEnvelope {
            id: 1,
            protocol: PROTOCOL,
            request: Request::Status,
        })
        .unwrap();
        let back: RequestEnvelope = serde_json::from_str(&unit).unwrap();
        assert!(matches!(back.request, Request::Status));
    }

    #[test]
    fn newer_minor_with_unknown_fields_still_parses() {
        let s = r#"{"id":1,"protocol":{"major":1,"minor":9},"method":"sessions","params":{"project":null,"future":true}}"#;
        let e: RequestEnvelope = serde_json::from_str(s).unwrap();
        assert!(e.protocol.compatible_with(PROTOCOL));
        assert!(!ProtocolVersion { major: 2, minor: 0 }.compatible_with(PROTOCOL));
    }

    #[test]
    fn write_methods_are_not_read_only() {
        assert!(!is_read_only(&Request::CreateSession {
            location_id: "l".into(),
            intent: CreateIntent::Reuse,
            command: vec![]
        }));
        assert!(!is_read_only(&Request::DeleteProject { key: "x".into() }));
        assert!(is_read_only(&Request::Sessions { project: None }));
    }

    #[test]
    fn response_envelope_format() {
        let r = ResponseEnvelope {
            id: 1,
            protocol: PROTOCOL,
            body: ResponseBody::Error {
                error: ApiError::new(ErrorCode::NotFound, "nope"),
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""status":"error""#) && s.contains(r#""code":"not_found""#));
    }
}
