use crate::ids::*;
use serde::{Deserialize, Serialize};

/// A machine that can host project locations and tmux servers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub id: DeviceId,
    /// User-facing name, unique per registry.
    pub name: String,
    pub os: String,
    pub hostname: Option<String>,
    /// `true` for the device this registry lives on.
    pub is_local: bool,
    /// SSH destination (`user@host`, a `Host` alias, or a tailnet name) for a
    /// remote device. Never used for the local device.
    pub ssh_target: Option<String>,
    /// Path of `ghostmc` on the remote device, when not on its `PATH`.
    pub remote_ghostmc: Option<String>,
    pub last_seen: Option<i64>,
}

/// A logical project. Deliberately not a path: the same project can live in
/// several places on several devices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    pub notes: Option<String>,
    pub pinned: bool,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A project's checkout on one device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectLocation {
    pub id: LocationId,
    pub project_id: ProjectId,
    pub device_id: DeviceId,
    /// Normalized absolute path on `device_id`. Only meaningful on that device.
    pub path: String,
    /// Command offered when creating a new session (e.g. `codex`); never run
    /// automatically.
    pub default_command: Option<String>,
    pub created_at: i64,
}

/// Git facts about a location, detected on demand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitIdentity {
    /// Top of the working tree containing the location.
    pub root: String,
    /// Shared `.git` directory; equal for all worktrees of one repository.
    pub common_dir: String,
    /// `true` when `root` is a linked worktree (`git worktree add`).
    pub is_linked_worktree: bool,
    pub branch: Option<String>,
    pub head: Option<String>,
}

/// Record of a tmux session that ghostmc itself created. Used for idempotent
/// create and to label sessions as managed even after tmux options are lost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedSessionRecord {
    pub location_id: LocationId,
    pub socket_path: String,
    pub server_pid: u32,
    pub server_start: i64,
    pub session_id: String,
    pub session_name: String,
    pub token: String,
    pub created_at: i64,
}
