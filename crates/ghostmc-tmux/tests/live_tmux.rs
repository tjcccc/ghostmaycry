//! Integration tests against a real tmux server on a private socket.

use ghostmc_tmux::*;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Server {
    _dir: tempfile::TempDir,
    socket: PathBuf,
}

impl Server {
    fn new() -> Option<Self> {
        if Command::new("tmux").arg("-V").output().is_err() {
            eprintln!("tmux not installed; skipping");
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("s");
        Some(Self { _dir: dir, socket })
    }
    fn tmux(&self, args: &[&str]) -> std::process::Output {
        Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .arg("-f")
            .arg("/dev/null")
            .args(args)
            .env_remove("TMUX")
            .output()
            .unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.tmux(&["kill-server"]);
    }
}

fn req(name: &str, cwd: &Path) -> CreateSession {
    CreateSession {
        name: name.into(),
        cwd: cwd.to_string_lossy().into(),
        token: "tok123".into(),
        location_id: "loc_a".into(),
        project_id: "prj_a".into(),
        command: vec![],
    }
}

#[test]
fn no_server_is_none_not_error() {
    let Some(s) = Server::new() else { return };
    assert!(Tmux::default().snapshot(&s.socket).unwrap().is_none());
}

#[test]
fn create_tag_and_discover() {
    let Some(s) = Server::new() else { return };
    let work = tempfile::tempdir().unwrap();
    // An unrelated, user-created session first.
    assert!(
        s.tmux(&["new-session", "-d", "-s", "mine", "-c", "/"])
            .status
            .success()
    );
    let t = Tmux::default();
    let created = t
        .create_session(&s.socket, &req("gmc-test", work.path()))
        .unwrap();
    assert!(created.session_id.starts_with('$'));
    assert!(created.server.pid > 0);

    let snap = t.snapshot(&s.socket).unwrap().unwrap();
    assert_eq!(
        snap.server, created.server,
        "epoch must be stable across calls"
    );
    assert_eq!(snap.sessions.len(), 2);
    let ours = snap
        .sessions
        .iter()
        .find(|x| x.id == created.session_id)
        .unwrap();
    assert_eq!(ours.ghostmc_location.as_deref(), Some("loc_a"));
    assert_eq!(ours.ghostmc_token.as_deref(), Some("tok123"));
    let theirs = snap.sessions.iter().find(|x| x.name == "mine").unwrap();
    assert_eq!(
        theirs.ghostmc_location, None,
        "tags must not leak to other sessions"
    );
    let pane = ours.active_pane().unwrap();
    assert!(pane.id.starts_with('%'));
    assert_eq!(
        std::fs::canonicalize(&pane.current_path).unwrap(),
        std::fs::canonicalize(work.path()).unwrap()
    );
    // Token is in the session environment for hooks to pick up.
    let env = s.tmux(&["show-environment", "-t", &created.session_id, ENV_TOKEN]);
    assert_eq!(
        String::from_utf8_lossy(&env.stdout).trim(),
        "GHOSTMC_SESSION_TOKEN=tok123"
    );

    assert!(matches!(
        t.create_session(&s.socket, &req("gmc-test", work.path())),
        Err(TmuxError::DuplicateName(_))
    ));
    assert!(t.has_session(&s.socket, &created.session_id).unwrap());
    assert!(!t.has_session(&s.socket, "$999").unwrap());
}

#[test]
fn hostile_names_and_paths_are_inert() {
    let Some(s) = Server::new() else { return };
    let base = tempfile::tempdir().unwrap();
    let weird = base.path().join("dir with 'quotes' $(touch pwned) ; ls");
    std::fs::create_dir(&weird).unwrap();
    let t = Tmux::default();
    let mut r = req("gmc-x", &weird);
    r.location_id = "loc; kill-server".into();
    let c = t.create_session(&s.socket, &r).unwrap();
    let snap = t.snapshot(&s.socket).unwrap().unwrap();
    let sess = snap.sessions.iter().find(|x| x.id == c.session_id).unwrap();
    assert_eq!(sess.ghostmc_location.as_deref(), Some("loc; kill-server"));
    assert!(!base.path().join("pwned").exists());
    assert!(
        t.create_session(&s.socket, &req("bad:name", &weird))
            .is_err()
    );
}

#[test]
fn explicit_command_runs_as_argv() {
    let Some(s) = Server::new() else { return };
    let work = tempfile::tempdir().unwrap();
    let mut r = req("gmc-cmd", work.path());
    r.command = vec!["sleep".into(), "30".into()];
    let c = Tmux::default().create_session(&s.socket, &r).unwrap();
    let snap = Tmux::default().snapshot(&s.socket).unwrap().unwrap();
    let sess = snap.sessions.iter().find(|x| x.id == c.session_id).unwrap();
    assert_eq!(sess.active_pane().unwrap().current_command, "sleep");
}

#[test]
fn creates_missing_socket_dir_privately() {
    let Some(s) = Server::new() else { return };
    let nested = s.socket.parent().unwrap().join("tmux-test/default");
    let work = tempfile::tempdir().unwrap();
    let c = Tmux::default()
        .create_session(&nested, &req("gmc-dir", work.path()))
        .unwrap();
    assert!(c.session_id.starts_with('$'));
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(nested.parent().unwrap())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
    let _ = Command::new("tmux")
        .arg("-S")
        .arg(&nested)
        .arg("kill-server")
        .output();
}
