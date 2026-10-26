//! A tmux server that ghostmc starts from inside an agent's shell must not
//! inherit that agent's session markers, or every agent started in it acts
//! as a child of the original session (Claude Code then saves no transcript).

use std::path::Path;
use std::process::{Command, Output};

fn run(cmd: &mut Command) -> Output {
    let out = cmd.env_remove("TMUX").output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn ghostmc(dir: &Path, args: &[&str]) -> Output {
    run(Command::new(env!("CARGO_BIN_EXE_ghostmc"))
        .args(args)
        .env("GHOSTMC_HOME", dir.join("h"))
        .env("TMUX_TMPDIR", dir)
        .env("CLAUDECODE", "1")
        .env("CLAUDE_CODE_CHILD_SESSION", "1")
        .env("CODEX_THREAD_ID", "t")
        .env("CODEX_HOME", "/kept")
        .current_dir(dir))
}

#[test]
fn new_tmux_server_drops_agent_session_env() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux not installed; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    std::fs::create_dir(dir.join("p")).unwrap();
    let project = dir.join("p").to_string_lossy().into_owned();
    ghostmc(dir, &["project", "add", "demo", "--path", &project]);
    ghostmc(dir, &["new", "demo"]);

    let socket = dir
        .join(format!("tmux-{}", unsafe { libc::getuid() }))
        .join("default");
    let tmux = |args: &[&str]| run(Command::new("tmux").arg("-S").arg(&socket).args(args));
    let env = format!(
        "\n{}",
        String::from_utf8_lossy(&tmux(&["show-environment", "-g"]).stdout)
    );
    tmux(&["kill-server"]);

    for var in ["CLAUDECODE", "CLAUDE_CODE_CHILD_SESSION", "CODEX_THREAD_ID"] {
        assert!(!env.contains(&format!("\n{var}=")), "{var} leaked:\n{env}");
    }
    assert!(env.contains("\nCODEX_HOME=/kept\n"), "{env}");
}
