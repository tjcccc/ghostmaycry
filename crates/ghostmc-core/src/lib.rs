//! Domain model, durable registry and local integrations shared by the
//! ghostmc host, CLI and desktop app.
//!
//! Identity rules (see `docs/adr/0001-architecture.md`):
//! - A [`model::Project`] is a logical grouping, never a path.
//! - A [`model::ProjectLocation`] binds a project to a path on one device.
//! - Live terminal state (tmux, Ghostty) is never stored as truth here; only
//!   observations with provenance are persisted.

pub mod agent;
pub mod config;
pub mod dirs;
pub mod files;
pub mod git;
pub mod hooks;
pub mod ids;
pub mod matching;
pub mod model;
pub mod paths;
pub mod shell;
pub mod store;

pub use ids::*;

/// Seconds since the Unix epoch.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
