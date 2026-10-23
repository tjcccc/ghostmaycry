//! Request handling shared by the host daemon and direct mode.

use crate::lock::FileLock;
use ghostmc_core::agent::{AgentEvent, AgentRun, AgentState};
use ghostmc_core::config::Config;
use ghostmc_core::matching::{self, Association, SessionFacts};
use ghostmc_core::model::{ManagedSessionRecord, Project, ProjectLocation};
use ghostmc_core::paths;
use ghostmc_core::store::{Store, StoreError};
use ghostmc_protocol::*;
use ghostmc_tmux::{AttachMode, CreateSession, Snapshot, Tmux, TmuxError};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// Agent runs older than this are not shown by default.
const DEFAULT_AGENT_WINDOW: i64 = 24 * 3600;

/// Called when an agent run changes into a state worth telling the user.
pub type Notifier = Box<dyn Fn(&AgentView) + Send + Sync>;

pub struct Service {
    store: Mutex<Store>,
    cfg: Config,
    tmux: Tmux,
    host_mode: bool,
    lock_dir: PathBuf,
    db_path: String,
    counter: Mutex<u64>,
    changed: Condvar,
    notifier: Option<Notifier>,
}

type ApiResult<T> = Result<T, ApiError>;

fn store_err(e: StoreError) -> ApiError {
    let code = match &e {
        StoreError::NotFound(_) => ErrorCode::NotFound,
        StoreError::Ambiguous(_) => ErrorCode::Ambiguous,
        StoreError::Conflict(_) => ErrorCode::Conflict,
        StoreError::Invalid(_) => ErrorCode::Invalid,
        _ => ErrorCode::Internal,
    };
    ApiError::new(code, e.to_string())
}

fn tmux_err(e: TmuxError) -> ApiError {
    let code = match e {
        TmuxError::Spawn { .. } => ErrorCode::Unavailable,
        TmuxError::DuplicateName(_) => ErrorCode::Conflict,
        _ => ErrorCode::Internal,
    };
    ApiError::new(code, e.to_string())
}

fn invalid(msg: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::Invalid, msg)
}

fn to_value<T: Serialize>(v: T) -> ApiResult<Value> {
    serde_json::to_value(v).map_err(|e| ApiError::new(ErrorCode::Internal, e.to_string()))
}

/// Live tmux state across all inspected servers, taken once per request.
struct Live {
    snapshots: Vec<Snapshot>,
    /// Canonical socket paths inspected successfully (with or without a
    /// running server).
    inspected: Vec<String>,
    errors: Vec<String>,
}

impl Live {
    fn pane_alive(&self, socket: &str, server_pid: u32, pane: &str) -> Option<bool> {
        let Some(snap) = self
            .snapshots
            .iter()
            .find(|s| s.server.socket_path == socket)
        else {
            // We looked and no server runs there: the pane is gone. A socket
            // we never inspected tells us nothing.
            return self.inspected.iter().any(|s| s == socket).then_some(false);
        };
        if snap.server.pid != server_pid {
            // Server restarted: pane ids from the old lifetime are meaningless.
            return Some(false);
        }
        Some(
            snap.sessions
                .iter()
                .any(|s| s.panes().any(|p| p.id == pane)),
        )
    }
}

impl Service {
    pub fn new(store: Store, cfg: Config, host_mode: bool) -> Self {
        let tmux = Tmux::new(cfg.tmux.binary.clone());
        Self {
            store: Mutex::new(store),
            cfg,
            tmux,
            host_mode,
            lock_dir: ghostmc_core::dirs::runtime_dir().join("locks"),
            db_path: ghostmc_core::dirs::db_path().display().to_string(),
            counter: Mutex::new(0),
            changed: Condvar::new(),
            notifier: None,
        }
    }

    /// Open the default registry and config.
    pub fn open_default(host_mode: bool) -> Result<Self, String> {
        let cfg = Config::load_default().map_err(|e| e.to_string())?;
        let store = Store::open_default().map_err(|e| e.to_string())?;
        Ok(Self::new(store, cfg, host_mode))
    }

    pub fn with_notifier(mut self, n: Notifier) -> Self {
        self.notifier = Some(n);
        self
    }

    pub fn with_lock_dir(mut self, dir: PathBuf) -> Self {
        self.lock_dir = dir;
        self
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn bump(&self) {
        let mut c = self.counter.lock().unwrap_or_else(|p| p.into_inner());
        *c += 1;
        self.changed.notify_all();
    }

    pub fn change_counter(&self) -> u64 {
        *self.counter.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Dispatch one request.
    pub fn handle(&self, req: Request) -> ApiResult<Value> {
        match req {
            Request::Status => to_value(self.status()?),
            Request::ListProjects => to_value(self.list_projects()?),
            Request::GetProject { key } => to_value(self.project_summary(&key)?),
            Request::CreateProject {
                name,
                tags,
                notes,
                path,
            } => to_value(self.create_project(&name, &tags, notes.as_deref(), path.as_deref())?),
            Request::UpdateProject {
                key,
                name,
                notes,
                pinned,
                add_tags,
                remove_tags,
            } => {
                let mut st = self.store();
                let p = st.project(&key).map_err(store_err)?;
                let notes = notes
                    .as_ref()
                    .map(|n| (!n.is_empty()).then_some(n.as_str()));
                st.update_project(&p.id, name.as_deref(), notes, pinned)
                    .map_err(store_err)?;
                let p = st
                    .set_tags(&p.id, &add_tags, &remove_tags)
                    .map_err(store_err)?;
                drop(st);
                self.bump();
                to_value(p)
            }
            Request::DeleteProject { key } => {
                let mut st = self.store();
                let p = st.project(&key).map_err(store_err)?;
                st.delete_project(&p.id).map_err(store_err)?;
                drop(st);
                self.bump();
                to_value(p)
            }
            Request::AddLocation {
                project,
                path,
                device,
                default_command,
            } => to_value(self.add_location(
                &project,
                &path,
                device.as_deref(),
                default_command.as_deref(),
            )?),
            Request::RemoveLocation { location_id } => {
                let mut st = self.store();
                st.remove_location(&location_id.as_str().into())
                    .map_err(store_err)?;
                drop(st);
                self.bump();
                Ok(Value::Null)
            }
            Request::ListDevices => {
                let mut st = self.store();
                st.local_device().map_err(store_err)?;
                to_value(st.list_devices().map_err(store_err)?)
            }
            Request::AddDevice {
                name,
                ssh_target,
                remote_ghostmc,
            } => {
                let d = self
                    .store()
                    .add_remote_device(&name, &ssh_target, remote_ghostmc.as_deref())
                    .map_err(store_err)?;
                self.bump();
                to_value(d)
            }
            Request::RemoveDevice { key } => {
                let st = self.store();
                let d = st.device(&key).map_err(store_err)?;
                st.remove_device(&d.id).map_err(store_err)?;
                drop(st);
                self.bump();
                Ok(Value::Null)
            }
            Request::Sessions { project } => to_value(self.sessions(project.as_deref())?),
            Request::OpenPlan {
                project,
                location_id,
            } => to_value(self.open_plan(&project, location_id.as_deref())?),
            Request::PathPlan { path } => to_value(self.path_plan(&path)?),
            Request::ResolveSession { spec, socket } => {
                to_value(self.resolve_session(&spec, socket.as_deref())?)
            }
            Request::CreateSession {
                location_id,
                intent,
                command,
            } => to_value(self.create_session(&location_id, intent, command)?),
            Request::AttachCommand { key, mode } => to_value(self.attach_command(&key, mode)?),
            Request::AgentRuns { since_secs } => {
                to_value(self.agent_views(since_secs.unwrap_or(DEFAULT_AGENT_WINDOW))?)
            }
            Request::RecentEvents { limit } => to_value(
                self.store()
                    .recent_events(limit.unwrap_or(50).min(1000))
                    .map_err(store_err)?,
            ),
            Request::IngestEvent { event } => {
                self.ingest(&event)?;
                Ok(Value::Null)
            }
            Request::GitStatus { location_id } => {
                let root = self.git_root(&location_id)?;
                to_value(ghostmc_core::git::status(&root).map_err(|e| invalid(e.to_string()))?)
            }
            Request::GitDiff {
                location_id,
                path,
                staged,
            } => {
                let root = self.git_root(&location_id)?;
                to_value(
                    ghostmc_core::git::diff(&root, path.as_deref(), staged)
                        .map_err(|e| invalid(e.to_string()))?,
                )
            }
            Request::ListFiles {
                location_id,
                rel,
                show_hidden,
            } => {
                let loc = self.local_location(&location_id)?;
                to_value(
                    ghostmc_core::files::list_dir(Path::new(&loc.path), &rel, show_hidden)
                        .map_err(|e| invalid(e.to_string()))?,
                )
            }
            Request::WaitForChange { after, timeout_ms } => {
                let deadline = Duration::from_millis(timeout_ms.min(30_000));
                let guard = self.counter.lock().unwrap_or_else(|p| p.into_inner());
                let (guard, _) = self
                    .changed
                    .wait_timeout_while(guard, deadline, |c| *c <= after)
                    .unwrap_or_else(|p| p.into_inner());
                to_value(Changed { counter: *guard })
            }
        }
    }

    // ----------------------------------------------------------------- status

    fn status(&self) -> ApiResult<StatusInfo> {
        let mut st = self.store();
        let device = st.local_device().map_err(store_err)?;
        let schema_version = st.schema_version().map_err(store_err)?;
        drop(st);
        Ok(StatusInfo {
            ghostmc_version: env!("CARGO_PKG_VERSION").into(),
            protocol: PROTOCOL,
            schema_version,
            device,
            host_running: self.host_mode,
            host_pid: self.host_mode.then(std::process::id),
            tmux_version: self.tmux.version().ok().filter(|v| !v.is_empty()),
            tmux_binary: self.tmux.resolved_binary(),
            sockets: ghostmc_tmux::discover_sockets(&self.cfg.tmux.sockets)
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
            db_path: self.db_path.clone(),
            change_counter: self.change_counter(),
        })
    }

    // --------------------------------------------------------------- projects

    fn location_view(&self, st: &Store, loc: &ProjectLocation, with_git: bool) -> LocationView {
        let dev = st.device(&loc.device_id.0).ok();
        let is_local = dev.as_ref().is_some_and(|d| d.is_local);
        let (git, exists) = if is_local {
            let p = Path::new(&loc.path);
            let git = if with_git {
                ghostmc_core::git::detect(p).ok().flatten()
            } else {
                None
            };
            (git, Some(p.is_dir()))
        } else {
            (None, None)
        };
        LocationView {
            location: loc.clone(),
            device_name: dev.map(|d| d.name).unwrap_or_else(|| "?".into()),
            is_local,
            git,
            exists,
        }
    }

    fn list_projects(&self) -> ApiResult<Vec<ProjectSummary>> {
        let live = self.live();
        let runs = self.agent_views_with(&live, DEFAULT_AGENT_WINDOW)?;
        let mut st = self.store();
        let local = st.local_device().map_err(store_err)?;
        let projects = st.list_projects().map_err(store_err)?;
        let mut out = Vec::with_capacity(projects.len());
        for p in projects {
            let locs = st.locations_for_project(&p.id).map_err(store_err)?;
            let local_locs: Vec<&ProjectLocation> =
                locs.iter().filter(|l| l.device_id == local.id).collect();
            let live_sessions = live
                .snapshots
                .iter()
                .flat_map(|s| s.sessions.iter())
                .filter(|s| local_locs.iter().any(|l| associate(l, s).is_some()))
                .count();
            let attention = runs
                .iter()
                .filter(|r| {
                    r.state.needs_attention() && r.project_name.as_deref() == Some(p.name.as_str())
                })
                .count();
            out.push(ProjectSummary {
                locations: locs
                    .iter()
                    .map(|l| self.location_view(&st, l, false))
                    .collect(),
                project: p,
                live_sessions,
                attention,
            });
        }
        Ok(out)
    }

    fn project_summary(&self, key: &str) -> ApiResult<ProjectSummary> {
        let st = self.store();
        let p = st.project(key).map_err(store_err)?;
        let locs = st.locations_for_project(&p.id).map_err(store_err)?;
        let locations = locs
            .iter()
            .map(|l| self.location_view(&st, l, true))
            .collect();
        drop(st);
        let live_sessions = self.sessions(Some(key))?.len();
        let attention = self
            .agent_views(DEFAULT_AGENT_WINDOW)?
            .iter()
            .filter(|r| {
                r.state.needs_attention() && r.project_name.as_deref() == Some(p.name.as_str())
            })
            .count();
        Ok(ProjectSummary {
            project: p,
            locations,
            live_sessions,
            attention,
        })
    }

    fn create_project(
        &self,
        name: &str,
        tags: &[String],
        notes: Option<&str>,
        path: Option<&str>,
    ) -> ApiResult<ProjectSummary> {
        let normalized = path.map(|p| paths::normalize(&paths::expand_tilde(p)));
        if let Some(p) = &normalized
            && !p.is_dir()
        {
            return Err(invalid(format!("{} is not a directory", p.display())));
        }
        let mut st = self.store();
        let project = st.create_project(name, tags, notes).map_err(store_err)?;
        if let Some(p) = normalized {
            let dev = st.local_device().map_err(store_err)?;
            if let Err(e) = st.add_location(&project.id, &dev.id, &p.to_string_lossy(), None) {
                let _ = st.delete_project(&project.id);
                return Err(store_err(e));
            }
        }
        drop(st);
        self.bump();
        self.project_summary(&project.id.0)
    }

    fn add_location(
        &self,
        project: &str,
        path: &str,
        device: Option<&str>,
        cmd: Option<&str>,
    ) -> ApiResult<ProjectLocation> {
        let mut st = self.store();
        let p = st.project(project).map_err(store_err)?;
        let dev = match device {
            Some(d) => st.device(d).map_err(store_err)?,
            None => st.local_device().map_err(store_err)?,
        };
        let stored = if dev.is_local {
            let n = paths::normalize(&paths::expand_tilde(path));
            if !n.is_dir() {
                return Err(invalid(format!("{} is not a directory", n.display())));
            }
            n.to_string_lossy().into_owned()
        } else {
            // A remote path cannot be normalized here; keep it verbatim.
            path.to_owned()
        };
        let loc = st
            .add_location(&p.id, &dev.id, &stored, cmd)
            .map_err(store_err)?;
        drop(st);
        self.bump();
        Ok(loc)
    }

    fn local_location(&self, id: &str) -> ApiResult<ProjectLocation> {
        let mut st = self.store();
        let loc = st.location(id).map_err(store_err)?;
        let local = st.local_device().map_err(store_err)?;
        if loc.device_id != local.id {
            return Err(ApiError::new(
                ErrorCode::Unsupported,
                "location is on another device; query that device instead",
            ));
        }
        Ok(loc)
    }

    fn git_root(&self, location_id: &str) -> ApiResult<PathBuf> {
        let loc = self.local_location(location_id)?;
        match ghostmc_core::git::detect(Path::new(&loc.path)) {
            Ok(Some(g)) => Ok(PathBuf::from(g.root)),
            Ok(None) => Err(invalid(format!("{} is not in a git repository", loc.path))),
            Err(e) => Err(invalid(e.to_string())),
        }
    }

    // ------------------------------------------------------------------- tmux

    fn live(&self) -> Live {
        let mut snapshots = Vec::new();
        let mut inspected = Vec::new();
        let mut errors = Vec::new();
        let mut sockets = ghostmc_tmux::discover_sockets(&self.cfg.tmux.sockets);
        // Always include the socket we create sessions on.
        let create = ghostmc_tmux::resolve_socket(&self.cfg.tmux.create_on_socket);
        let create = std::fs::canonicalize(&create).unwrap_or(create);
        if !sockets.contains(&create) {
            sockets.push(create);
        }
        for s in sockets {
            match self.tmux.snapshot(&s) {
                Ok(Some(snap)) => {
                    inspected.push(snap.server.socket_path.clone());
                    snapshots.push(snap);
                }
                Ok(None) => inspected.push(s.to_string_lossy().into_owned()),
                Err(e) => errors.push(format!("{}: {e}", s.display())),
            }
        }
        Live {
            snapshots,
            inspected,
            errors,
        }
    }

    fn session_view(
        &self,
        st: &Store,
        snap: &Snapshot,
        s: &ghostmc_tmux::Session,
        managed_records: &[ManagedSessionRecord],
        runs: &[AgentView],
    ) -> SessionView {
        let r = SessionRef {
            socket_path: snap.server.socket_path.clone(),
            server_pid: snap.server.pid,
            server_start: snap.server.start_time,
            session_id: s.id.clone(),
        };
        let record = managed_records.iter().find(|m| {
            m.socket_path == r.socket_path
                && m.server_pid == r.server_pid
                && m.server_start == r.server_start
                && m.session_id == r.session_id
        });
        let location_id = s
            .ghostmc_location
            .clone()
            .or_else(|| record.map(|m| m.location_id.0.clone()));
        let location = location_id.as_deref().and_then(|l| st.location(l).ok());
        let project = location
            .as_ref()
            .and_then(|l| st.project(&l.project_id.0).ok());
        let key = r.key();
        let agents = runs
            .iter()
            .filter(|a| a.session_key.as_deref() == Some(key.as_str()))
            .cloned()
            .collect();
        SessionView {
            key,
            r#ref: r,
            name: s.name.clone(),
            created: s.created,
            clients: snap
                .clients
                .iter()
                .filter(|c| c.session_id == s.id)
                .map(|c| ClientView {
                    tty: c.tty.clone(),
                    readonly: c.readonly,
                    width: c.width,
                    height: c.height,
                    ghostty_terminal_id: None,
                })
                .collect(),
            panes: s
                .windows
                .iter()
                .flat_map(|w| {
                    w.panes.iter().map(move |p| PaneView {
                        id: p.id.clone(),
                        window_id: w.id.clone(),
                        window_name: w.name.clone(),
                        active: w.active && p.active,
                        cwd: p.current_path.clone(),
                        command: p.current_command.clone(),
                    })
                })
                .collect(),
            managed: s.ghostmc_location.is_some() || record.is_some(),
            project_id: project.as_ref().map(|p| p.id.0.clone()),
            project_name: project.map(|p| p.name),
            location_id,
            association: None,
            agents,
            tmux_binary: self.tmux.resolved_binary(),
        }
    }

    fn all_session_views(&self, live: &Live) -> ApiResult<Vec<(SessionView, SessionFacts)>> {
        let runs = self.agent_views_with(live, DEFAULT_AGENT_WINDOW)?;
        let st = self.store();
        let records = st.managed_sessions().map_err(store_err)?;
        let mut out = Vec::new();
        for snap in &live.snapshots {
            for s in &snap.sessions {
                let view = self.session_view(&st, snap, s, &records, &runs);
                let facts = SessionFacts {
                    tagged_location: view.location_id.clone(),
                    pane_paths: s.panes().map(|p| p.current_path.clone()).collect(),
                };
                out.push((view, facts));
            }
        }
        Ok(out)
    }

    fn sessions(&self, project: Option<&str>) -> ApiResult<Vec<SessionView>> {
        let live = self.live();
        let all = self.all_session_views(&live)?;
        let Some(key) = project else {
            return Ok(all.into_iter().map(|(v, _)| v).collect());
        };
        let mut st = self.store();
        let p = st.project(key).map_err(store_err)?;
        let local = st.local_device().map_err(store_err)?;
        let locs: Vec<ProjectLocation> = st
            .locations_for_project(&p.id)
            .map_err(store_err)?
            .into_iter()
            .filter(|l| l.device_id == local.id)
            .collect();
        drop(st);
        Ok(all
            .into_iter()
            .filter_map(|(mut v, f)| {
                let best = locs
                    .iter()
                    .filter_map(|l| matching::associate(&l.id.0, Path::new(&l.path), &f))
                    .min()?;
                v.association = Some(best);
                Some(v)
            })
            .collect())
    }

    fn rank(views: &mut [SessionView]) {
        views.sort_by(|a, b| {
            a.association
                .cmp(&b.association)
                .then_with(|| b.is_attached().cmp(&a.is_attached()))
                .then_with(|| b.created.cmp(&a.created))
        });
    }

    fn open_plan(&self, project: &str, location_id: Option<&str>) -> ApiResult<OpenPlan> {
        let mut st = self.store();
        let p: Project = st.project(project).map_err(store_err)?;
        let local = st.local_device().map_err(store_err)?;
        let locs = st.locations_for_project(&p.id).map_err(store_err)?;
        let local_locs: Vec<&ProjectLocation> =
            locs.iter().filter(|l| l.device_id == local.id).collect();
        let loc = match location_id {
            Some(id) => locs.iter().find(|l| l.id.0 == id).ok_or_else(|| {
                ApiError::new(
                    ErrorCode::NotFound,
                    format!("location {id:?} of {}", p.name),
                )
            })?,
            None => *local_locs.first().ok_or_else(|| {
                let remote: Vec<String> = locs
                    .iter()
                    .filter_map(|l| st.device(&l.device_id.0).ok().map(|d| d.name))
                    .collect();
                ApiError::new(
                    ErrorCode::NotFound,
                    if remote.is_empty() {
                        format!(
                            "{} has no location; add one with `ghostmc project link`",
                            p.name
                        )
                    } else {
                        format!(
                            "{} has no location on this device (it has: {}); use --device",
                            p.name,
                            remote.join(", ")
                        )
                    },
                )
            })?,
        };
        if loc.device_id != local.id {
            return Err(ApiError::new(
                ErrorCode::Unsupported,
                "that location is on another device; open it with --device",
            ));
        }
        let location = self.location_view(&st, loc, true);
        let other_locations = local_locs
            .iter()
            .filter(|l| l.id != loc.id)
            .map(|l| self.location_view(&st, l, false))
            .collect();
        drop(st);

        let live = self.live();
        let mut tmux: Vec<SessionView> = self
            .all_session_views(&live)?
            .into_iter()
            .filter_map(|(mut v, f)| {
                v.association = Some(matching::associate(&loc.id.0, Path::new(&loc.path), &f)?);
                Some(v)
            })
            .collect();
        Self::rank(&mut tmux);
        let mut notes = live.errors.clone();
        if tmux.is_empty() {
            notes.push("No tmux session matches this location. Create one explicitly; nothing is started automatically.".into());
        } else if !tmux
            .iter()
            .any(|s| s.association == Some(Association::Tagged))
        {
            notes.push("Matches are by working directory only. Pick one explicitly.".into());
        }
        notes.push(
            "Terminals that are not running inside tmux can be focused locally but cannot be attached from another device.".into(),
        );
        Ok(OpenPlan {
            project: p,
            location,
            other_locations,
            tmux,
            ghostty: Vec::new(),
            ghostty_error: None,
            capabilities: None,
            notes,
        })
    }

    fn path_plan(&self, path: &str) -> ApiResult<PathPlanResult> {
        let norm = paths::normalize(&paths::expand_tilde(path));
        let mut st = self.store();
        let device = st.local_device().map_err(store_err)?;
        let locs: Vec<ProjectLocation> = st
            .locations_on_device(&device.id)
            .map_err(store_err)?
            .into_iter()
            .filter(|l| Path::new(&l.path) == norm)
            .collect();
        drop(st);
        let live = self.live();
        let mut tmux: Vec<SessionView> = self
            .all_session_views(&live)?
            .into_iter()
            .filter_map(|(mut v, f)| {
                let by_tag = locs
                    .iter()
                    .filter_map(|l| matching::associate(&l.id.0, &norm, &f))
                    .min();
                let a = by_tag.or_else(|| matching::associate("", &norm, &f))?;
                v.association = Some(a);
                Some(v)
            })
            .collect();
        Self::rank(&mut tmux);
        Ok(PathPlanResult {
            exists: norm.is_dir(),
            path: norm.to_string_lossy().into_owned(),
            tmux,
            device,
        })
    }

    fn resolve_session(&self, spec: &str, socket: Option<&str>) -> ApiResult<SessionView> {
        let live = self.live();
        let all = self.all_session_views(&live)?;
        if let Some(r) = SessionRef::parse_key(spec) {
            if let Some((v, _)) = all.into_iter().find(|(v, _)| v.r#ref == r) {
                return Ok(v);
            }
            return Err(ApiError::new(
                ErrorCode::NotFound,
                "session is gone (or its tmux server restarted); list sessions again",
            ));
        }
        let socket = socket.map(|s| {
            let p = ghostmc_tmux::resolve_socket(s);
            std::fs::canonicalize(&p)
                .unwrap_or(p)
                .to_string_lossy()
                .into_owned()
        });
        let mut hits: Vec<SessionView> = all
            .into_iter()
            .map(|(v, _)| v)
            .filter(|v| socket.as_deref().is_none_or(|s| v.r#ref.socket_path == s))
            .filter(|v| v.r#ref.session_id == spec || v.name == spec)
            .collect();
        match hits.len() {
            0 => Err(ApiError::new(
                ErrorCode::NotFound,
                format!("no tmux session {spec:?}"),
            )),
            1 => Ok(hits.remove(0)),
            _ => Err(ApiError::new(
                ErrorCode::Ambiguous,
                format!(
                    "{spec:?} matches several sessions: {}",
                    hits.iter()
                        .map(|h| h.key.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )),
        }
    }

    fn create_session(
        &self,
        location_id: &str,
        intent: CreateIntent,
        command: Vec<String>,
    ) -> ApiResult<CreatedSession> {
        let loc = self.local_location(location_id)?;
        if !Path::new(&loc.path).is_dir() {
            return Err(invalid(format!("{} does not exist", loc.path)));
        }
        // Serialize per location across processes: a double click or two
        // clients racing must not create two sessions.
        let _guard = FileLock::acquire(&self.lock_dir.join(format!("{}.lock", loc.id)))
            .map_err(|e| ApiError::new(ErrorCode::Internal, format!("lock: {e}")))?;
        let project = self.store().project(&loc.project_id.0).map_err(store_err)?;

        if intent == CreateIntent::Reuse {
            let live = self.live();
            let existing = self
                .all_session_views(&live)?
                .into_iter()
                .map(|(v, _)| v)
                .filter(|v| v.managed && v.location_id.as_deref() == Some(loc.id.0.as_str()))
                .max_by_key(|v| v.created);
            if let Some(session) = existing {
                return Ok(CreatedSession {
                    created: false,
                    session,
                });
            }
        }

        let socket = ghostmc_tmux::resolve_socket(&self.cfg.tmux.create_on_socket);
        let taken: Vec<String> = match self.tmux.snapshot(&socket).map_err(tmux_err)? {
            Some(s) => s.sessions.into_iter().map(|s| s.name).collect(),
            None => Vec::new(),
        };
        let base = ghostmc_tmux::session_name_for(&project.name);
        let token = uuid::Uuid::new_v4().simple().to_string();
        let mut n = 1;
        let created = loop {
            let name = if n == 1 {
                base.clone()
            } else {
                format!("{base}-{n}")
            };
            n += 1;
            if taken.contains(&name) {
                continue;
            }
            let req = CreateSession {
                name: name.clone(),
                cwd: loc.path.clone(),
                token: token.clone(),
                location_id: loc.id.0.clone(),
                project_id: project.id.0.clone(),
                command: command.clone(),
            };
            match self.tmux.create_session(&socket, &req) {
                Ok(c) => break (c, name),
                // Created by someone else between snapshot and create.
                Err(TmuxError::DuplicateName(_)) if n < 100 => continue,
                Err(e) => return Err(tmux_err(e)),
            }
        };
        let (c, name) = created;
        self.store()
            .record_managed_session(&ManagedSessionRecord {
                location_id: loc.id.clone(),
                socket_path: c.server.socket_path.clone(),
                server_pid: c.server.pid,
                server_start: c.server.start_time,
                session_id: c.session_id.clone(),
                session_name: name,
                token,
                created_at: ghostmc_core::now(),
            })
            .map_err(store_err)?;
        self.bump();
        let key = SessionRef {
            socket_path: c.server.socket_path,
            server_pid: c.server.pid,
            server_start: c.server.start_time,
            session_id: c.session_id,
        }
        .key();
        Ok(CreatedSession {
            created: true,
            session: self.resolve_session(&key, None)?,
        })
    }

    fn attach_command(&self, key: &str, mode: AttachMode) -> ApiResult<AttachCommand> {
        let s = self.resolve_session(key, None)?;
        Ok(AttachCommand {
            argv: ghostmc_tmux::attach_argv(
                &s.tmux_binary,
                &s.r#ref.socket_path,
                &s.r#ref.session_id,
                mode,
            ),
            mode,
            cwd: s.active_pane().map(|p| p.cwd.clone()),
        })
    }

    // ----------------------------------------------------------------- agents

    fn agent_views(&self, since: i64) -> ApiResult<Vec<AgentView>> {
        let live = self.live();
        self.agent_views_with(&live, since)
    }

    fn agent_views_with(&self, live: &Live, since: i64) -> ApiResult<Vec<AgentView>> {
        let st = self.store();
        let runs = st.agent_runs(since).map_err(store_err)?;
        let records = st.managed_sessions().map_err(store_err)?;
        let local = st
            .list_devices()
            .map_err(store_err)?
            .into_iter()
            .find(|d| d.is_local);
        let locations = match &local {
            Some(d) => st.locations_on_device(&d.id).map_err(store_err)?,
            None => Vec::new(),
        };
        let now = ghostmc_core::now();
        let mut out = Vec::with_capacity(runs.len());
        for run in runs {
            let (session_key, pane_id, pane_alive) = locate_run(live, &records, &run);
            // Project: from the session's tag/record if verified, else the
            // location containing the agent's cwd (labelled unverified by
            // the run's attribution).
            let project_name = session_key
                .as_deref()
                .and_then(SessionRef::parse_key)
                .and_then(|r| {
                    live.snapshots
                        .iter()
                        .find(|s| s.server.socket_path == r.socket_path)
                        .and_then(|s| s.sessions.iter().find(|x| x.id == r.session_id))
                        .and_then(|x| x.ghostmc_location.clone())
                        .or_else(|| {
                            records
                                .iter()
                                .find(|m| {
                                    m.socket_path == r.socket_path
                                        && m.session_id == r.session_id
                                        && m.server_pid == r.server_pid
                                })
                                .map(|m| m.location_id.0.clone())
                        })
                })
                .and_then(|l| st.location(&l).ok())
                .or_else(|| {
                    let cwd = run.cwd.as_deref()?;
                    locations
                        .iter()
                        .filter(|l| {
                            matches!(
                                paths::relation(Path::new(&l.path), Path::new(cwd)),
                                paths::PathRelation::Exact | paths::PathRelation::Descendant(_)
                            )
                        })
                        .max_by_key(|l| l.path.len())
                        .cloned()
                })
                .and_then(|l| st.project(&l.project_id.0).ok())
                .map(|p| p.name);
            out.push(AgentView {
                state: run.effective_state(now, pane_alive),
                run,
                project_name,
                session_key,
                pane_id,
            });
        }
        Ok(out)
    }

    /// Store an agent event; notify on attention-worthy transitions.
    pub fn ingest(&self, event: &AgentEvent) -> ApiResult<()> {
        let result = self.store().ingest_event(event).map_err(store_err)?;
        let Some((run, changed)) = result else {
            return Ok(()); // duplicate
        };
        self.bump();
        if let (Some(run), true, Some(notify)) = (run, changed, &self.notifier) {
            let worth = run.state.needs_attention()
                || (self.cfg.notifications.on_turn_end && run.state == AgentState::TurnEnded);
            if self.cfg.notifications.enabled && worth {
                let view = self
                    .agent_views(DEFAULT_AGENT_WINDOW)?
                    .into_iter()
                    .find(|v| v.run.id == run.id);
                if let Some(v) = view {
                    notify(&v);
                }
            }
        }
        Ok(())
    }

    /// Periodic maintenance: forget records of sessions that no longer exist
    /// and bound event storage.
    pub fn reconcile(&self) {
        let live = self.live();
        let alive: HashMap<(String, u32, i64), Vec<String>> = live
            .snapshots
            .iter()
            .map(|s| {
                (
                    (
                        s.server.socket_path.clone(),
                        s.server.pid,
                        s.server.start_time,
                    ),
                    s.sessions.iter().map(|x| x.id.clone()).collect(),
                )
            })
            .collect();
        let st = self.store();
        let removed = st
            .prune_managed_sessions(|r| {
                alive
                    .get(&(r.socket_path.clone(), r.server_pid, r.server_start))
                    .is_some_and(|ids| ids.contains(&r.session_id))
            })
            .unwrap_or(0);
        let _ = st.prune_events(14 * 24 * 3600);
        drop(st);
        if removed > 0 {
            self.bump();
        }
    }
}

fn associate(l: &ProjectLocation, s: &ghostmc_tmux::Session) -> Option<Association> {
    matching::associate(
        &l.id.0,
        Path::new(&l.path),
        &SessionFacts {
            tagged_location: s.ghostmc_location.clone(),
            pane_paths: s.panes().map(|p| p.current_path.clone()).collect(),
        },
    )
}

/// Find the live session/pane an agent run belongs to, using only verified
/// evidence (session token, or tmux socket+server pid+pane from the hook
/// environment). Returns (session key, pane id, pane alive).
fn locate_run(
    live: &Live,
    records: &[ManagedSessionRecord],
    run: &AgentRun,
) -> (Option<String>, Option<String>, Option<bool>) {
    if let Some(tok) = &run.token {
        for snap in &live.snapshots {
            for s in &snap.sessions {
                let rec_match = records.iter().any(|m| {
                    &m.token == tok
                        && m.socket_path == snap.server.socket_path
                        && m.server_pid == snap.server.pid
                        && m.session_id == s.id
                });
                if s.ghostmc_token.as_deref() == Some(tok.as_str()) || rec_match {
                    let key = SessionRef {
                        socket_path: snap.server.socket_path.clone(),
                        server_pid: snap.server.pid,
                        server_start: snap.server.start_time,
                        session_id: s.id.clone(),
                    }
                    .key();
                    let alive = run
                        .tmux_pane
                        .as_ref()
                        .map(|p| s.panes().any(|x| &x.id == p));
                    return (Some(key), run.tmux_pane.clone(), alive.or(Some(true)));
                }
            }
        }
    }
    let (Some(sock), Some(pid), Some(pane)) =
        (&run.tmux_socket, run.tmux_server_pid, &run.tmux_pane)
    else {
        return (None, None, None);
    };
    let sock_canon = std::fs::canonicalize(sock)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| sock.clone());
    let alive = live.pane_alive(&sock_canon, pid, pane);
    let key = live
        .snapshots
        .iter()
        .find(|s| s.server.socket_path == sock_canon && s.server.pid == pid)
        .and_then(|snap| {
            snap.sessions
                .iter()
                .find(|s| s.panes().any(|p| &p.id == pane))
                .map(|s| {
                    SessionRef {
                        socket_path: snap.server.socket_path.clone(),
                        server_pid: snap.server.pid,
                        server_start: snap.server.start_time,
                        session_id: s.id.clone(),
                    }
                    .key()
                })
        });
    (key, Some(pane.clone()), alive)
}
