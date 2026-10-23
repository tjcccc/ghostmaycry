//! tmux driver over the tmux CLI.
//!
//! tmux is the source of truth for live sessions. Everything here targets
//! tmux's stable ids (`$session`, `@window`, `%pane`) on an explicit socket,
//! never names or indices, and passes arguments as argv, never through a
//! shell. Ids are only stable for one server lifetime, so every reference
//! carries the server's pid and start time (its "epoch").

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Field separator in format strings. Recent tmux prints control characters
/// in format output as octal escapes (`\037`), older versions print the raw
/// byte; [`fields`] accepts both. A value containing the literal text `\037`
/// changes the field count and the line is skipped rather than misparsed.
const SEP: char = '\u{1f}';
const SEP_ESCAPED: &str = "\\037";

/// Session user options ghostmc sets on sessions it creates.
pub const OPT_LOCATION: &str = "@ghostmc_location";
pub const OPT_PROJECT: &str = "@ghostmc_project";
pub const OPT_TOKEN: &str = "@ghostmc_token";
/// Environment variable placed in sessions ghostmc creates; hooks read it to
/// correlate agent runs with the session.
pub const ENV_TOKEN: &str = "GHOSTMC_SESSION_TOKEN";

#[derive(Debug, thiserror::Error)]
pub enum TmuxError {
    #[error("tmux not found or not runnable ({binary}): {source}")]
    Spawn {
        binary: String,
        source: std::io::Error,
    },
    #[error("tmux: {0}")]
    Command(String),
    #[error("a session named {0:?} already exists")]
    DuplicateName(String),
    #[error("unexpected tmux output: {0}")]
    Parse(String),
}

pub type Result<T> = std::result::Result<T, TmuxError>;

/// Identity of one tmux server lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerRef {
    pub socket_path: String,
    pub pid: u32,
    /// Server start time (seconds since epoch).
    pub start_time: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pane {
    pub id: String,
    pub index: u32,
    pub active: bool,
    pub current_path: String,
    pub current_command: String,
    pub pid: u32,
    pub width: u32,
    pub height: u32,
    pub dead: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub id: String,
    pub index: u32,
    pub name: String,
    pub active: bool,
    pub panes: Vec<Pane>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub created: i64,
    pub attached_clients: u32,
    pub windows: Vec<Window>,
    /// `@ghostmc_location` / `@ghostmc_project` / `@ghostmc_token`, if set.
    pub ghostmc_location: Option<String>,
    pub ghostmc_project: Option<String>,
    pub ghostmc_token: Option<String>,
}

impl Session {
    pub fn panes(&self) -> impl Iterator<Item = &Pane> {
        self.windows.iter().flat_map(|w| w.panes.iter())
    }
    pub fn active_pane(&self) -> Option<&Pane> {
        self.windows
            .iter()
            .find(|w| w.active)
            .and_then(|w| w.panes.iter().find(|p| p.active))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Client {
    /// Terminal device of the client (e.g. `/dev/ttys004`). Matches the
    /// `tty` Ghostty reports for the terminal displaying it.
    pub tty: String,
    pub pid: u32,
    pub session_id: String,
    pub readonly: bool,
    pub flags: String,
    pub width: u32,
    pub height: u32,
    pub termname: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub server: ServerRef,
    pub version: String,
    pub sessions: Vec<Session>,
    pub clients: Vec<Client>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachMode {
    /// Input-capable client. Sizing follows the user's tmux `window-size`
    /// option (a small writable client may shrink the session).
    Writable,
    /// Input-capable client that does not affect the session size
    /// (`ignore-size`), e.g. for a phone.
    WritableKeepSize,
    /// `read-only` (no input) + `ignore-size` (does not resize the session).
    Observe,
}

#[derive(Debug, Clone)]
pub struct Tmux {
    pub binary: String,
}

impl Default for Tmux {
    fn default() -> Self {
        Self::new("tmux")
    }
}

/// Directory tmux uses for this user's sockets: `$TMUX_TMPDIR/tmux-<uid>`
/// (default `/tmp`).
pub fn user_socket_dir() -> PathBuf {
    let base = std::env::var_os("TMUX_TMPDIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join(format!("tmux-{}", unsafe { libc::getuid() }))
}

/// Resolve a socket spec: an absolute path is used as-is, anything else is a
/// `-L` style name inside [`user_socket_dir`].
pub fn resolve_socket(spec: &str) -> PathBuf {
    if spec.contains('/') {
        PathBuf::from(spec)
    } else {
        user_socket_dir().join(if spec.is_empty() { "default" } else { spec })
    }
}

/// Sockets to inspect: our own socket directory (never other users'), plus
/// configured extras. Paths are returned canonicalized when they exist.
pub fn discover_sockets(extra: &[String]) -> Vec<PathBuf> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let uid = unsafe { libc::getuid() };
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        let p = std::fs::canonicalize(&p).unwrap_or(p);
        if !out.contains(&p) {
            out.push(p);
        }
    };
    let dir = user_socket_dir();
    let dir_ok = std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir() && m.uid() == uid);
    if dir_ok && let Ok(rd) = std::fs::read_dir(&dir) {
        let mut found: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| {
                e.metadata()
                    .is_ok_and(|m| m.file_type().is_socket() && m.uid() == uid)
            })
            .map(|e| e.path())
            .collect();
        found.sort();
        found.into_iter().for_each(&mut push);
    }
    extra.iter().map(|s| resolve_socket(s)).for_each(push);
    out
}

fn fields(line: &str, n: usize) -> Option<Vec<String>> {
    let v: Vec<String> = line
        .replace(SEP_ESCAPED, &SEP.to_string())
        .split(SEP)
        .map(str::to_owned)
        .collect();
    (v.len() == n).then_some(v)
}

fn opt(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_owned())
}

const PANE_FORMAT: &[&str] = &[
    "P",
    "#{pid}",
    "#{start_time}",
    "#{version}",
    "#{session_id}",
    "#{session_name}",
    "#{session_created}",
    "#{session_attached}",
    "#{@ghostmc_location}",
    "#{@ghostmc_project}",
    "#{@ghostmc_token}",
    "#{window_id}",
    "#{window_index}",
    "#{window_name}",
    "#{window_active}",
    "#{pane_id}",
    "#{pane_index}",
    "#{pane_active}",
    "#{pane_current_path}",
    "#{pane_current_command}",
    "#{pane_pid}",
    "#{pane_width}",
    "#{pane_height}",
    "#{pane_dead}",
];

const CLIENT_FORMAT: &[&str] = &[
    "C",
    "#{client_tty}",
    "#{client_pid}",
    "#{session_id}",
    "#{client_readonly}",
    "#{client_flags}",
    "#{client_width}",
    "#{client_height}",
    "#{client_termname}",
];

fn join_format(f: &[&str]) -> String {
    f.join(&SEP.to_string())
}

/// Parse the combined `list-panes -a` + `list-clients` output.
pub fn parse_snapshot(socket_path: &str, out: &str) -> Result<Option<Snapshot>> {
    let mut server: Option<(ServerRef, String)> = None;
    let mut sessions: Vec<Session> = Vec::new();
    let mut clients = Vec::new();
    let num = |s: &String| s.parse::<u32>().unwrap_or(0);
    for line in out.lines() {
        if line.starts_with('P') {
            let Some(f) = fields(line, PANE_FORMAT.len()) else {
                continue; // a name containing the separator; skip rather than misparse
            };
            if server.is_none() {
                server = Some((
                    ServerRef {
                        socket_path: socket_path.to_owned(),
                        pid: num(&f[1]),
                        start_time: f[2].parse().unwrap_or(0),
                    },
                    f[3].to_owned(),
                ));
            }
            let sidx = match sessions.iter().position(|s| s.id == f[4]) {
                Some(i) => i,
                None => {
                    sessions.push(Session {
                        id: f[4].to_owned(),
                        name: f[5].to_owned(),
                        created: f[6].parse().unwrap_or(0),
                        attached_clients: num(&f[7]),
                        windows: Vec::new(),
                        ghostmc_location: opt(&f[8]),
                        ghostmc_project: opt(&f[9]),
                        ghostmc_token: opt(&f[10]),
                    });
                    sessions.len() - 1
                }
            };
            let s = &mut sessions[sidx];
            let widx = match s.windows.iter().position(|w| w.id == f[11]) {
                Some(i) => i,
                None => {
                    s.windows.push(Window {
                        id: f[11].to_owned(),
                        index: num(&f[12]),
                        name: f[13].to_owned(),
                        active: f[14] == "1",
                        panes: Vec::new(),
                    });
                    s.windows.len() - 1
                }
            };
            s.windows[widx].panes.push(Pane {
                id: f[15].to_owned(),
                index: num(&f[16]),
                active: f[17] == "1",
                current_path: f[18].to_owned(),
                current_command: f[19].to_owned(),
                pid: num(&f[20]),
                width: num(&f[21]),
                height: num(&f[22]),
                dead: f[23] == "1",
            });
        } else if line.starts_with('C') {
            let Some(f) = fields(line, CLIENT_FORMAT.len()) else {
                continue;
            };
            clients.push(Client {
                tty: f[1].to_owned(),
                pid: num(&f[2]),
                session_id: f[3].to_owned(),
                readonly: f[4] == "1",
                flags: f[5].to_owned(),
                width: num(&f[6]),
                height: num(&f[7]),
                termname: f[8].to_owned(),
            });
        }
    }
    Ok(server.map(|(server, version)| Snapshot {
        server,
        version,
        sessions,
        clients,
    }))
}

fn is_no_server(stderr: &str) -> bool {
    stderr.contains("no server running")
        || stderr.contains("error connecting to")
        || stderr.contains("No such file or directory")
        || stderr.contains("Connection refused")
}

impl Tmux {
    pub fn new(binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
        }
    }

    fn cmd(&self, socket: &Path) -> Command {
        let mut c = Command::new(&self.binary);
        c.arg("-S").arg(socket);
        // Never inherit a parent session's TMUX: it would make tmux refuse
        // nesting or target the wrong server.
        c.env_remove("TMUX");
        c
    }

    fn output(&self, mut c: Command) -> Result<Output> {
        c.output().map_err(|source| TmuxError::Spawn {
            binary: self.binary.clone(),
            source,
        })
    }

    /// `tmux -V`, e.g. `tmux 3.4`.
    pub fn version(&self) -> Result<String> {
        let out = self.output({
            let mut c = Command::new(&self.binary);
            c.arg("-V");
            c
        })?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// Absolute path of the tmux binary (for remote attach commands, where a
    /// non-interactive SSH `PATH` may not include it).
    pub fn resolved_binary(&self) -> String {
        if self.binary.contains('/') {
            return self.binary.clone();
        }
        std::env::var_os("PATH")
            .and_then(|paths| {
                std::env::split_paths(&paths)
                    .map(|d| d.join(&self.binary))
                    .find(|p| p.is_file())
            })
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.binary.clone())
    }

    /// Full state of the server on `socket`, or `None` if no server runs there.
    pub fn snapshot(&self, socket: &Path) -> Result<Option<Snapshot>> {
        let mut c = self.cmd(socket);
        c.args(["list-panes", "-a", "-F", &join_format(PANE_FORMAT)]);
        c.args([";", "list-clients", "-F", &join_format(CLIENT_FORMAT)]);
        let out = self.output(c)?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            if is_no_server(&stderr) {
                return Ok(None);
            }
            return Err(TmuxError::Command(stderr.trim().to_owned()));
        }
        let canonical = std::fs::canonicalize(socket).unwrap_or_else(|_| socket.to_path_buf());
        parse_snapshot(
            &canonical.to_string_lossy(),
            &String::from_utf8_lossy(&out.stdout),
        )
    }

    /// Create a detached session and tag it, in one tmux command list so the
    /// tags can never land on another session. `command` is passed to tmux
    /// as argv (no shell); empty means the user's default shell.
    pub fn create_session(&self, socket: &Path, req: &CreateSession) -> Result<CreatedSession> {
        if req.name.is_empty() || req.name.contains([':', '.']) || req.name.starts_with('-') {
            return Err(TmuxError::Command(format!(
                "invalid session name {:?}",
                req.name
            )));
        }
        // `tmux -L` creates its socket directory, `tmux -S` does not (and
        // still exits 0). Create it like tmux would: private to this user.
        if let Some(dir) = socket.parent()
            && !dir.exists()
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|e| TmuxError::Command(format!("creating {}: {e}", dir.display())))?;
        }
        let mut c = self.cmd(socket);
        c.args(["new-session", "-d", "-P", "-F"]);
        c.arg(join_format(&["#{session_id}", "#{pid}", "#{start_time}"]));
        c.args(["-s", &req.name, "-c", &req.cwd]);
        c.args(["-e", &format!("{ENV_TOKEN}={}", req.token)]);
        if !req.command.is_empty() {
            c.arg("--");
            c.args(&req.command);
        }
        for (k, v) in [
            (OPT_LOCATION, &req.location_id),
            (OPT_PROJECT, &req.project_id),
            (OPT_TOKEN, &req.token),
        ] {
            // Untargeted set-option applies to the session new-session just
            // made (the command list's current target).
            c.args([";", "set-option", k, v]);
        }
        let out = self.output(c)?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            if stderr.contains("duplicate session") {
                return Err(TmuxError::DuplicateName(req.name.clone()));
            }
            return Err(TmuxError::Command(stderr.trim().to_owned()));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let line = stdout.lines().next().unwrap_or("");
        if line.is_empty() && !stderr.trim().is_empty() {
            return Err(TmuxError::Command(stderr.trim().to_owned()));
        }
        let f = fields(line, 3).ok_or_else(|| TmuxError::Parse(line.to_owned()))?;
        let canonical = std::fs::canonicalize(socket).unwrap_or_else(|_| socket.to_path_buf());
        Ok(CreatedSession {
            server: ServerRef {
                socket_path: canonical.to_string_lossy().into_owned(),
                pid: f[1].parse().map_err(|_| TmuxError::Parse(line.into()))?,
                start_time: f[2].parse().map_err(|_| TmuxError::Parse(line.into()))?,
            },
            session_id: f[0].to_owned(),
        })
    }

    pub fn has_session(&self, socket: &Path, session_id: &str) -> Result<bool> {
        let mut c = self.cmd(socket);
        c.args(["has-session", "-t", session_id]);
        Ok(self.output(c)?.status.success())
    }
}

#[derive(Debug, Clone)]
pub struct CreateSession {
    pub name: String,
    pub cwd: String,
    pub token: String,
    pub location_id: String,
    pub project_id: String,
    pub command: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatedSession {
    pub server: ServerRef,
    pub session_id: String,
}

/// argv that attaches to an existing session. Never detaches other clients.
pub fn attach_argv(
    tmux_binary: &str,
    socket_path: &str,
    session_id: &str,
    mode: AttachMode,
) -> Vec<String> {
    let mut v = vec![
        tmux_binary.to_owned(),
        "-S".into(),
        socket_path.to_owned(),
        "attach-session".into(),
        "-t".into(),
        session_id.to_owned(),
    ];
    match mode {
        AttachMode::Writable => {}
        AttachMode::WritableKeepSize => v.extend(["-f".into(), "ignore-size".into()]),
        AttachMode::Observe => v.extend(["-f".into(), "read-only,ignore-size".into()]),
    }
    v
}

/// A tmux-safe session name derived from a label: `gmc-<slug>`. Labels are
/// user input; the slug keeps only `[a-z0-9-]`.
pub fn session_name_for(label: &str) -> String {
    let mut slug = String::new();
    for ch in label.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    let slug: String = slug.chars().take(40).collect();
    if slug.is_empty() {
        "gmc".into()
    } else {
        format!("gmc-{slug}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_sanitized() {
        assert_eq!(session_name_for("Ghost May Cry"), "gmc-ghost-may-cry");
        assert_eq!(session_name_for("a:b.c;rm -rf"), "gmc-a-b-c-rm-rf");
        assert_eq!(session_name_for("///"), "gmc");
    }

    #[test]
    fn observe_argv_is_read_only_and_ignores_size() {
        let v = attach_argv("tmux", "/s", "$3", AttachMode::Observe);
        assert_eq!(
            v,
            [
                "tmux",
                "-S",
                "/s",
                "attach-session",
                "-t",
                "$3",
                "-f",
                "read-only,ignore-size"
            ]
        );
        let w = attach_argv("tmux", "/s", "$3", AttachMode::Writable);
        assert!(!w.iter().any(|a| a.contains("read-only") || a == "-d"));
        let k = attach_argv("tmux", "/s", "$3", AttachMode::WritableKeepSize);
        assert_eq!(k[6..], ["-f", "ignore-size"]);
    }

    #[test]
    fn parse_skips_malformed_lines() {
        let s = parse_snapshot("/s", "P\u{1f}too few\nC\u{1f}x\n").unwrap();
        assert!(s.is_none());
    }

    #[test]
    fn resolve_socket_names_and_paths() {
        assert_eq!(resolve_socket("/x/y"), PathBuf::from("/x/y"));
        assert!(resolve_socket("work").ends_with("work"));
        assert!(resolve_socket("").ends_with("default"));
    }
}
