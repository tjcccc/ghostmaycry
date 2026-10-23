//! Desktop-facing integrations: Ghostty, editors, file managers and system
//! notifications.
//!
//! These calls need the user's graphical session (and on macOS, Automation
//! permission for Ghostty). They belong to the desktop app / interactive CLI,
//! not to the headless host. Every capability is reported honestly per
//! platform instead of being faked.

pub mod ghostty;
pub mod notify;
pub mod open;

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("{0} is not supported on this platform")]
    Unsupported(&'static str),
    #[error("Ghostty is not installed or not reachable: {0}")]
    GhosttyMissing(String),
    #[error("Ghostty is not running")]
    GhosttyNotRunning,
    #[error(
        "macOS denied Automation access to Ghostty. Allow it in System Settings → Privacy & Security → Automation"
    )]
    AutomationDenied,
    #[error("no such Ghostty {0}")]
    NotFound(String),
    #[error("{0}")]
    Failed(String),
}

pub type Result<T> = std::result::Result<T, PlatformError>;

/// What the terminal integration can do here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub platform: String,
    /// Enumerate existing Ghostty windows/tabs/terminals.
    pub list_terminals: bool,
    /// Focus a specific existing terminal.
    pub focus_terminal: bool,
    pub new_window: bool,
    /// New tab in an existing window.
    pub new_tab: bool,
    pub notes: Vec<String>,
}
