//! End-to-end tests of the service against real tmux servers on private
//! sockets. Skipped when tmux is not installed.

use ghostmc_core::agent::{AgentEvent, AgentKind, AgentState, HookEnv};
use ghostmc_core::config::Config;
use ghostmc_core::matching::Association;
use ghostmc_core::store::Store;
use ghostmc_host::Service;
use ghostmc_protocol::*;
use ghostmc_tmux::AttachMode;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

struct Env {
    dir: tempfile::TempDir,
    socket: PathBuf,
    project_dir: PathBuf,
}

impl Env {
    fn new() -> Option<Self> {
        Command::new("tmux").arg("-V").output().ok()?;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("tmux.sock");
        let project_dir = dir.path().join("work/app");
        std::fs::create_dir_all(project_dir.join("src")).unwrap();
        Some(Self {
            dir,
            socket,
            project_dir,
        })
    }

    fn service(&self) -> Service {
        let mut cfg = Config::default();
        cfg.tmux.create_on_socket = self.socket.to_string_lossy().into();
        cfg.tmux.sockets = vec![self.socket.to_string_lossy().into()];
        Service::new(
            Store::open(&self.dir.path().join("db.sqlite3")).unwrap(),
            cfg,
            false,
        )
        .with_lock_dir(self.dir.path().join("locks"))
    }

    fn tmux(&self, args: &[&str]) -> String {
        let o = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .arg("-f")
            .arg("/dev/null")
            .args(args)
            .env_remove("TMUX")
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_owned()
    }

    fn socket_canon(&self) -> String {
        let dir = std::fs::canonicalize(self.socket.parent().unwrap()).unwrap();
        dir.join(self.socket.file_name().unwrap())
            .to_string_lossy()
            .into()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.tmux(&["kill-server"]);
    }
}

fn call<T: DeserializeOwned>(s: &Service, r: Request) -> T {
    serde_json::from_value(s.handle(r).unwrap_or_else(|e| panic!("{e:?}"))).unwrap()
}

fn ours(sessions: Vec<SessionView>, env: &Env) -> Vec<SessionView> {
    let sock = env.socket_canon();
    sessions
        .into_iter()
        .filter(|s| s.r#ref.socket_path == sock)
        .collect()
}

fn setup(env: &Env, s: &Service) -> ProjectSummary {
    call(
        s,
        Request::CreateProject {
            name: "Ghost App".into(),
            tags: vec!["rust".into()],
            notes: None,
            path: Some(env.project_dir.to_string_lossy().into()),
        },
    )
}

#[test]
fn open_create_reuse_additional() {
    let Some(env) = Env::new() else { return };
    let s = env.service();
    let p = setup(&env, &s);
    let loc = p.locations[0].location.id.0.clone();

    let plan: OpenPlan = call(
        &s,
        Request::OpenPlan {
            project: "ghost app".into(),
            location_id: None,
        },
    );
    assert!(ours(plan.tmux, &env).is_empty());
    assert!(
        plan.notes
            .iter()
            .any(|n| n.contains("nothing is started automatically"))
    );

    let first: CreatedSession = call(
        &s,
        Request::CreateSession {
            location_id: loc.clone(),
            intent: CreateIntent::Reuse,
            command: vec![],
        },
    );
    assert!(first.created);
    assert!(first.session.managed);
    assert_eq!(first.session.name, "gmc-ghost-app");
    assert_eq!(first.session.project_name.as_deref(), Some("Ghost App"));

    let again: CreatedSession = call(
        &s,
        Request::CreateSession {
            location_id: loc.clone(),
            intent: CreateIntent::Reuse,
            command: vec![],
        },
    );
    assert!(!again.created, "reuse must not create a duplicate");
    assert_eq!(again.session.key, first.session.key);

    let extra: CreatedSession = call(
        &s,
        Request::CreateSession {
            location_id: loc.clone(),
            intent: CreateIntent::Additional,
            command: vec![],
        },
    );
    assert!(extra.created);
    assert_eq!(extra.session.name, "gmc-ghost-app-2");

    let plan: OpenPlan = call(
        &s,
        Request::OpenPlan {
            project: "Ghost App".into(),
            location_id: None,
        },
    );
    let tm = ours(plan.tmux, &env);
    assert_eq!(tm.len(), 2);
    assert!(
        tm.iter()
            .all(|t| t.association == Some(Association::Tagged))
    );

    let attach: AttachCommand = call(
        &s,
        Request::AttachCommand {
            key: first.session.key.clone(),
            mode: AttachMode::Observe,
        },
    );
    assert!(
        attach
            .argv
            .ends_with(&["-f".into(), "read-only,ignore-size".into()])
    );
    assert!(attach.argv.contains(&"attach-session".to_string()));
}

#[test]
fn concurrent_creates_yield_one_session() {
    let Some(env) = Env::new() else { return };
    let s = Arc::new(env.service());
    let p = setup(&env, &s);
    let loc = p.locations[0].location.id.0.clone();
    // Separate Service instances model separate processes sharing the
    // registry and lock directory.
    let others: Vec<Arc<Service>> = (0..3).map(|_| Arc::new(env.service())).collect();
    let handles: Vec<_> = others
        .into_iter()
        .chain(std::iter::repeat_n(s.clone(), 3))
        .map(|svc| {
            let loc = loc.clone();
            std::thread::spawn(move || {
                let r: CreatedSession = serde_json::from_value(
                    svc.handle(Request::CreateSession {
                        location_id: loc,
                        intent: CreateIntent::Reuse,
                        command: vec![],
                    })
                    .unwrap(),
                )
                .unwrap();
                (r.created, r.session.key)
            })
        })
        .collect();
    let results: Vec<(bool, String)> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.0).count(), 1, "{results:?}");
    assert!(results.iter().all(|r| r.1 == results[0].1));
    let sessions: Vec<SessionView> = call(&s, Request::Sessions { project: None });
    assert_eq!(ours(sessions, &env).len(), 1);
}

#[test]
fn user_sessions_are_candidates_not_verified() {
    let Some(env) = Env::new() else { return };
    let s = env.service();
    setup(&env, &s);
    let sub = env.project_dir.join("src");
    env.tmux(&[
        "new-session",
        "-d",
        "-s",
        "handmade",
        "-c",
        sub.to_str().unwrap(),
    ]);
    env.tmux(&["new-session", "-d", "-s", "elsewhere", "-c", "/"]);
    let plan: OpenPlan = call(
        &s,
        Request::OpenPlan {
            project: "Ghost App".into(),
            location_id: None,
        },
    );
    let tm = ours(plan.tmux, &env);
    assert_eq!(tm.len(), 1);
    assert_eq!(tm[0].name, "handmade");
    assert!(!tm[0].managed);
    assert_eq!(tm[0].association, Some(Association::PathDescendant));
    assert!(
        plan.notes
            .iter()
            .any(|n| n.contains("working directory only"))
    );

    // Names are not identity: two servers could both have "handmade".
    let r: SessionView = call(
        &s,
        Request::ResolveSession {
            spec: "handmade".into(),
            socket: Some(env.socket.to_string_lossy().into()),
        },
    );
    assert_eq!(r.key, tm[0].key);
}

#[test]
fn hook_events_attach_agents_to_exact_panes() {
    let Some(env) = Env::new() else { return };
    let s = env.service();
    let p = setup(&env, &s);
    let loc = p.locations[0].location.id.0.clone();
    let c: CreatedSession = call(
        &s,
        Request::CreateSession {
            location_id: loc,
            intent: CreateIntent::Reuse,
            command: vec![],
        },
    );
    let pane = c.session.panes[0].id.clone();
    let tmux_env = format!("{},{},0", env.socket_canon(), c.session.r#ref.server_pid);

    let hook_env = HookEnv {
        tmux: Some(tmux_env),
        tmux_pane: Some(pane.clone()),
        token: None,
        tty: None,
        tty_pid: None,
    };
    let ev = |name: &str, extra: serde_json::Value, ts: i64| {
        let mut payload =
            json!({"session_id": "claude-1", "hook_event_name": name, "cwd": "/somewhere/else"});
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        AgentEvent::from_hook(AgentKind::Claude, &payload, &hook_env, ts).unwrap()
    };
    let now = ghostmc_core::now();
    s.handle(Request::IngestEvent {
        event: Box::new(ev("UserPromptSubmit", json!({"prompt": "secret"}), now)),
        brief: None,
    })
    .unwrap();
    s.handle(Request::IngestEvent {
        event: Box::new(ev(
            "PermissionRequest",
            json!({"tool_name": "Bash"}),
            now + 1,
        )),
        brief: None,
    })
    .unwrap();

    let agents: Vec<AgentView> = call(
        &s,
        Request::AgentRuns {
            since_secs: None,
            briefs: false,
        },
    );
    let a = agents
        .iter()
        .find(|a| a.run.agent_session_id == "claude-1")
        .unwrap();
    assert_eq!(a.state, AgentState::NeedsApproval);
    assert_eq!(a.session_key.as_deref(), Some(c.session.key.as_str()));
    assert_eq!(
        a.project_name.as_deref(),
        Some("Ghost App"),
        "cwd differs but pane evidence wins"
    );

    let sessions: Vec<SessionView> = call(
        &s,
        Request::Sessions {
            project: Some("Ghost App".into()),
        },
    );
    assert_eq!(sessions[0].agents.len(), 1);
    let summary: Vec<ProjectSummary> = call(&s, Request::ListProjects);
    assert_eq!(summary[0].attention, 1);

    // Kill the pane's session: the run becomes Disconnected, not "running".
    env.tmux(&["kill-session", "-t", &c.session.r#ref.session_id]);
    let agents: Vec<AgentView> = call(
        &s,
        Request::AgentRuns {
            since_secs: None,
            briefs: false,
        },
    );
    let a = agents
        .iter()
        .find(|a| a.run.agent_session_id == "claude-1")
        .unwrap();
    assert_eq!(a.state, AgentState::Disconnected);
}

#[test]
fn stale_keys_after_server_restart_are_rejected() {
    let Some(env) = Env::new() else { return };
    let s = env.service();
    let p = setup(&env, &s);
    let loc = p.locations[0].location.id.0.clone();
    let c: CreatedSession = call(
        &s,
        Request::CreateSession {
            location_id: loc.clone(),
            intent: CreateIntent::Reuse,
            command: vec![],
        },
    );
    env.tmux(&["kill-server"]);
    std::thread::sleep(std::time::Duration::from_millis(200));
    env.tmux(&["new-session", "-d", "-s", "fresh", "-c", "/"]);
    let err = s
        .handle(Request::ResolveSession {
            spec: c.session.key.clone(),
            socket: None,
        })
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    s.reconcile();
    // The old managed record is gone; reuse creates a new session.
    let c2: CreatedSession = call(
        &s,
        Request::CreateSession {
            location_id: loc,
            intent: CreateIntent::Reuse,
            command: vec![],
        },
    );
    assert!(c2.created);
    assert_ne!(c2.session.key, c.session.key);
}

#[test]
fn files_and_git_are_confined() {
    let Some(env) = Env::new() else { return };
    let s = env.service();
    let p = setup(&env, &s);
    let loc = p.locations[0].location.id.0.clone();
    std::fs::write(env.project_dir.join("README.md"), "x").unwrap();
    let l: ghostmc_core::files::Listing = call(
        &s,
        Request::ListFiles {
            location_id: loc.clone(),
            rel: String::new(),
            show_hidden: false,
        },
    );
    assert!(l.entries.iter().any(|e| e.name == "README.md"));
    let err = s
        .handle(Request::ListFiles {
            location_id: loc.clone(),
            rel: "../..".into(),
            show_hidden: false,
        })
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Invalid);
    let err = s
        .handle(Request::GitStatus { location_id: loc })
        .unwrap_err();
    assert!(err.message.contains("not in a git repository"));
}

#[test]
fn path_plan_for_remote_clients() {
    let Some(env) = Env::new() else { return };
    let s = env.service();
    setup(&env, &s);
    env.tmux(&[
        "new-session",
        "-d",
        "-s",
        "byhand",
        "-c",
        env.project_dir.to_str().unwrap(),
    ]);
    let r: PathPlanResult = call(
        &s,
        Request::PathPlan {
            path: env.project_dir.to_string_lossy().into(),
        },
    );
    assert!(r.exists);
    let tm: Vec<_> = r
        .tmux
        .into_iter()
        .filter(|t| t.r#ref.socket_path == env.socket_canon())
        .collect();
    assert_eq!(tm.len(), 1);
    assert_eq!(tm[0].association, Some(Association::PathExact));
}

#[test]
fn uds_server_roundtrip_and_protocol_guard() {
    let Some(env) = Env::new() else { return };
    let svc = Arc::new(env.service());
    let sock = env.dir.path().join("run/host.sock");
    let rt = tokio::runtime::Runtime::new().unwrap();
    let sock2 = sock.clone();
    let svc2 = svc.clone();
    rt.spawn(async move { ghostmc_host::server::run(svc2, &sock2).await.unwrap() });
    for _ in 0..100 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(sock.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );

    let b = ghostmc_host::Backend::connect_or_direct_at(&sock).unwrap();
    assert!(b.is_host());
    let p: ProjectSummary = b
        .call(Request::CreateProject {
            name: "via host".into(),
            tags: vec![],
            notes: None,
            path: None,
        })
        .unwrap();
    assert_eq!(p.project.name, "via host");

    // A second host must refuse to start on the same socket.
    assert!(ghostmc_host::server::bind(&sock).is_err());

    // Incompatible major version.
    let mut conn = ghostmc_host::client::HostConnection::connect(&sock).unwrap();
    let resp = conn
        .roundtrip(&RequestEnvelope {
            id: 9,
            protocol: ProtocolVersion {
                major: 99,
                minor: 0,
            },
            request: Request::Status,
        })
        .unwrap();
    match resp.body {
        ResponseBody::Error { error } => assert_eq!(error.code, ErrorCode::IncompatibleProtocol),
        _ => panic!("expected error"),
    }
    // Change notification wakes long-pollers.
    let before: Changed = b
        .call(Request::WaitForChange {
            after: 0,
            timeout_ms: 10,
        })
        .unwrap();
    let svc3 = svc.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        svc3.handle(Request::CreateProject {
            name: "later".into(),
            tags: vec![],
            notes: None,
            path: None,
        })
        .unwrap();
    });
    let t0 = std::time::Instant::now();
    let after: Changed = b
        .call(Request::WaitForChange {
            after: before.counter,
            timeout_ms: 5000,
        })
        .unwrap();
    assert!(after.counter > before.counter);
    assert!(t0.elapsed() < std::time::Duration::from_secs(4));
}

#[test]
fn read_only_api_guard() {
    let Some(env) = Env::new() else { return };
    let s = env.service();
    let line = serde_json::to_string(&RequestEnvelope {
        id: 1,
        protocol: PROTOCOL,
        request: Request::CreateProject {
            name: "x".into(),
            tags: vec![],
            notes: None,
            path: None,
        },
    })
    .unwrap();
    let r = ghostmc_host::server::handle_envelope(&s, &line, false);
    assert!(matches!(r.body, ResponseBody::Error { error } if error.code == ErrorCode::Forbidden));
    let r = ghostmc_host::server::handle_envelope(
        &s,
        &line.replace("create_project", "status").replace(
            r#","params":{"name":"x","tags":[],"notes":null,"path":null}"#,
            "",
        ),
        false,
    );
    assert!(matches!(r.body, ResponseBody::Ok { .. }), "{r:?}");
    let _ = Path::new("");
}

#[test]
fn briefs_stay_in_memory_and_only_when_asked() {
    let Some(env) = Env::new() else { return };
    let s = env.service();
    let now = ghostmc_core::now();
    let payload = json!({
        "session_id": "c-brief", "hook_event_name": "Stop", "cwd": "/w",
        "last_assistant_message": "pushed 411bb49 to dev"
    });
    let event =
        AgentEvent::from_hook(AgentKind::Claude, &payload, &HookEnv::default(), now).unwrap();
    s.handle(Request::IngestEvent {
        event: Box::new(event),
        brief: ghostmc_core::agent::brief_from_hook(&payload),
    })
    .unwrap();
    let brief_of = |briefs| {
        let v: Vec<AgentView> = call(
            &s,
            Request::AgentRuns {
                since_secs: None,
                briefs,
            },
        );
        v.into_iter()
            .find(|a| a.run.agent_session_id == "c-brief")
            .unwrap()
            .brief
    };
    assert_eq!(brief_of(true).as_deref(), Some("pushed 411bb49 to dev"));
    assert_eq!(brief_of(false), None);
    // A new prompt makes the brief stale.
    let prompt = json!({"session_id": "c-brief", "hook_event_name": "UserPromptSubmit"});
    s.handle(Request::IngestEvent {
        event: Box::new(
            AgentEvent::from_hook(AgentKind::Claude, &prompt, &HookEnv::default(), now + 1)
                .unwrap(),
        ),
        brief: None,
    })
    .unwrap();
    assert_eq!(brief_of(true), None);
    s.handle(Request::IngestEvent {
        event: Box::new(
            AgentEvent::from_hook(AgentKind::Claude, &payload, &HookEnv::default(), now + 2)
                .unwrap(),
        ),
        brief: ghostmc_core::agent::brief_from_hook(&payload),
    })
    .unwrap();
    drop(s);
    // Never written to the registry (database file and its WAL).
    for name in ["db.sqlite3", "db.sqlite3-wal"] {
        if let Ok(bytes) = std::fs::read(env.dir.path().join(name)) {
            assert!(
                !String::from_utf8_lossy(&bytes).contains("411bb49"),
                "{name}"
            );
        }
    }

    let s = env.service();
    assert_eq!(
        call::<Vec<AgentView>>(
            &s,
            Request::AgentRuns {
                since_secs: None,
                briefs: true
            }
        )
        .into_iter()
        .find(|a| a.run.agent_session_id == "c-brief")
        .unwrap()
        .brief,
        None,
        "a new host process starts without briefs"
    );
}

#[test]
fn run_whose_terminal_process_exited_is_disconnected() {
    let Some(env) = Env::new() else { return };
    let s = env.service();
    let mut child = Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    let now = ghostmc_core::now();
    for (sid, pid) in [("tty-dead", dead), ("tty-live", std::process::id())] {
        let hook_env = HookEnv {
            tty: Some("/dev/ttys099".into()),
            tty_pid: Some(pid),
            ..Default::default()
        };
        let p = json!({"session_id": sid, "hook_event_name": "Stop", "cwd": "/w"});
        s.handle(Request::IngestEvent {
            event: Box::new(AgentEvent::from_hook(AgentKind::Claude, &p, &hook_env, now).unwrap()),
            brief: None,
        })
        .unwrap();
    }
    let v: Vec<AgentView> = call(
        &s,
        Request::AgentRuns {
            since_secs: None,
            briefs: false,
        },
    );
    let state = |sid: &str| {
        v.iter()
            .find(|a| a.run.agent_session_id == sid)
            .unwrap()
            .state
    };
    assert_eq!(state("tty-dead"), AgentState::Disconnected);
    assert_eq!(state("tty-live"), AgentState::TurnEnded);
}
