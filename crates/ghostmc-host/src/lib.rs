//! The ghostmc host: an unprivileged user process that owns the registry,
//! coordinates tmux, ingests agent events and answers clients.
//!
//! [`service::Service`] implements every request. It runs either inside the
//! long-lived host ([`server`]) or directly in a CLI process when no host is
//! running ("direct mode"), so `ghostmc` keeps working — including over SSH —
//! without a daemon. GUI actions (Ghostty focus/launch) are deliberately not
//! here; see `ghostmc-platform`.

pub mod client;
pub mod desktop;
pub mod lock;
pub mod remote;
pub mod server;
pub mod service;

pub use client::Backend;
pub use service::Service;
