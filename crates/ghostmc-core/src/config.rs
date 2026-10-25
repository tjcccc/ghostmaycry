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
    pub desktop: DesktopConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DesktopConfig {
    /// Folder the menu-bar app's `+` opens a new Ghostty window in (`~/`
    /// allowed). Unset means the home directory.
    pub new_terminal_dir: Option<String>,
}

impl DesktopConfig {
    /// `new_terminal_dir` with `~` expanded, or the home directory.
    pub fn new_terminal_dir(&self) -> Option<std::path::PathBuf> {
        match self
            .new_terminal_dir
            .as_deref()
            .filter(|d| !d.trim().is_empty())
        {
            Some(d) => Some(crate::paths::expand_tilde(d)),
            None => ::dirs::home_dir(),
        }
    }
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
    #[error("editing {path}: {message}")]
    Edit { path: String, message: String },
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

/// Set `[desktop] new_terminal_dir` in the file at `path`, keeping the rest
/// of the document (comments, order) as the user wrote it. Refuses a file
/// that does not parse.
pub fn set_new_terminal_dir(path: &Path, dir: &str) -> Result<(), ConfigError> {
    let edit_err = |message: String| ConfigError::Edit {
        path: path.display().to_string(),
        message,
    };
    let text = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.display().to_string(),
                source,
            });
        }
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| edit_err(e.to_string()))?;
    let table = doc
        .entry("desktop")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .ok_or_else(|| edit_err("`desktop` is not a table".into()))?;
    table["new_terminal_dir"] = toml_edit::value(dir);
    let out = doc.to_string();
    // Never write a file this version would then refuse to load.
    toml::from_str::<Config>(&out).map_err(|e| edit_err(e.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
            path: parent.display().to_string(),
            source,
        })?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, out)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|source| ConfigError::Io {
            path: path.display().to_string(),
            source,
        })
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
    fn setting_new_terminal_dir_keeps_the_rest_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(&p, "# mine\n[editor]\ncommand = [\"zed\"] # keep\n").unwrap();
        set_new_terminal_dir(&p, "~/stacks").unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("# mine") && text.contains("# keep"), "{text}");
        let c = Config::load(&p).unwrap();
        assert_eq!(c.desktop.new_terminal_dir.as_deref(), Some("~/stacks"));
        assert_eq!(c.editor.command, vec!["zed"]);
        set_new_terminal_dir(&p, "/w").unwrap();
        let c = Config::load(&p).unwrap();
        assert_eq!(c.desktop.new_terminal_dir.as_deref(), Some("/w"));

        let missing = dir.path().join("sub/new.toml");
        set_new_terminal_dir(&missing, "/x").unwrap();
        assert!(Config::load(&missing).is_ok());

        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "[desktop\n").unwrap();
        assert!(set_new_terminal_dir(&bad, "/x").is_err());
        assert_eq!(std::fs::read_to_string(&bad).unwrap(), "[desktop\n");
    }

    #[test]
    fn typos_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(&p, "[tmux]\nbinnary = \"x\"\n").unwrap();
        assert!(Config::load(&p).is_err());
    }
}
