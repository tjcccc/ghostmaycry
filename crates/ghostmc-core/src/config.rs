//! User preferences (`config.toml`). Durable registry data lives in SQLite;
//! this file only holds settings a user may want to hand-edit.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub tmux: TmuxConfig,
    pub terminal: TerminalConfig,
    pub editor: EditorConfig,
    pub notifications: NotificationConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TmuxConfig {
    /// tmux executable.
    pub binary: String,
    /// Extra servers to inspect: socket paths, or names as given to `tmux -L`.
    /// The default server and sockets in our own `tmux-<uid>` directory are
    /// always inspected.
    pub sockets: Vec<String>,
    /// Socket name ghostmc uses when it creates sessions (`tmux -L`). Empty
    /// means the user's default server.
    pub create_on_socket: String,
}

impl Default for TmuxConfig {
    fn default() -> Self {
        Self {
            binary: "tmux".into(),
            sockets: Vec::new(),
            create_on_socket: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TerminalConfig {
    /// `ghostty` (default) or `custom`.
    pub kind: String,
    /// Ghostty CLI on Linux. On macOS ghostmc talks to Ghostty.app via
    /// AppleScript instead.
    pub ghostty_binary: String,
    /// Linux only: `new-window` asks a running Ghostty over D-Bus
    /// (`ghostty +new-window`), `spawn` starts a new Ghostty process.
    pub linux_mode: String,
    /// `custom` terminal: argv prefix; the command to run is appended, e.g.
    /// `["foot", "--"]` or `["kitty", "--"]`.
    pub custom: Vec<String>,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            kind: "ghostty".into(),
            ghostty_binary: "ghostty".into(),
            linux_mode: "new-window".into(),
            custom: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EditorConfig {
    /// argv prefix for opening files, e.g. `["zed"]` or `["code", "-g"]`.
    /// Empty uses the system default (`open` / `xdg-open`).
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationConfig {
    pub enabled: bool,
    /// Notify when an agent turn ends, not only when it needs attention.
    pub on_turn_end: bool,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            on_turn_end: true,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("parsing {path}: {source}")]
    Parse {
        path: String,
        source: toml::de::Error,
    },
}

impl Config {
    /// Load from `path`; a missing file yields defaults.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).map_err(|source| ConfigError::Parse {
                path: path.display().to_string(),
                source,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Io {
                path: path.display().to_string(),
                source,
            }),
        }
    }

    pub fn load_default() -> Result<Self, ConfigError> {
        Self::load(&crate::dirs::config_path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_gives_defaults() {
        let c = Config::load(Path::new("/nonexistent/ghostmc.toml")).unwrap();
        assert_eq!(c.tmux.binary, "tmux");
        assert_eq!(c.terminal.kind, "ghostty");
    }

    #[test]
    fn partial_file_merges_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(
            &p,
            "[editor]\ncommand = [\"zed\"]\n[tmux]\nsockets = [\"work\"]\n",
        )
        .unwrap();
        let c = Config::load(&p).unwrap();
        assert_eq!(c.editor.command, vec!["zed"]);
        assert_eq!(c.tmux.sockets, vec!["work"]);
        assert_eq!(c.tmux.binary, "tmux");
    }

    #[test]
    fn typos_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(&p, "[tmux]\nbinnary = \"x\"\n").unwrap();
        assert!(Config::load(&p).is_err());
    }
}
