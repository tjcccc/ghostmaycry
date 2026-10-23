//! SQLite-backed registry: devices, projects, locations, managed-session
//! records, agent events and runs. One file per device; never synced.

use crate::agent::*;
use crate::ids::*;
use crate::model::*;
use crate::now;
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::path::Path;

/// Bump when adding a migration. Stored in `PRAGMA user_version`.
pub const SCHEMA_VERSION: i64 = 1;

const MIGRATIONS: &[&str] = &[
    // v1
    r#"
    CREATE TABLE devices (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL UNIQUE COLLATE NOCASE,
        os TEXT NOT NULL,
        hostname TEXT,
        is_local INTEGER NOT NULL,
        ssh_target TEXT,
        remote_ghostmc TEXT,
        last_seen INTEGER
    );
    CREATE UNIQUE INDEX devices_one_local ON devices(is_local) WHERE is_local = 1;

    CREATE TABLE projects (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL UNIQUE COLLATE NOCASE,
        notes TEXT,
        pinned INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );

    CREATE TABLE project_tags (
        project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
        tag TEXT NOT NULL,
        PRIMARY KEY (project_id, tag)
    );

    CREATE TABLE locations (
        id TEXT PRIMARY KEY,
        project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
        device_id TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
        path TEXT NOT NULL,
        default_command TEXT,
        created_at INTEGER NOT NULL,
        UNIQUE (project_id, device_id, path)
    );

    CREATE TABLE managed_sessions (
        location_id TEXT NOT NULL REFERENCES locations(id) ON DELETE CASCADE,
        socket_path TEXT NOT NULL,
        server_pid INTEGER NOT NULL,
        server_start INTEGER NOT NULL,
        session_id TEXT NOT NULL,
        session_name TEXT NOT NULL,
        token TEXT NOT NULL UNIQUE,
        created_at INTEGER NOT NULL,
        PRIMARY KEY (socket_path, server_pid, server_start, session_id)
    );

    CREATE TABLE events (
        seq INTEGER PRIMARY KEY AUTOINCREMENT,
        id TEXT NOT NULL UNIQUE,
        dedupe_key TEXT NOT NULL UNIQUE,
        ts INTEGER NOT NULL,
        agent TEXT NOT NULL,
        hook_event TEXT NOT NULL,
        agent_session_id TEXT,
        turn_id TEXT,
        cwd TEXT,
        tmux_socket TEXT,
        tmux_server_pid INTEGER,
        tmux_pane TEXT,
        token TEXT,
        detail TEXT,
        message TEXT
    );
    CREATE INDEX events_ts ON events(ts);

    CREATE TABLE agent_runs (
        id TEXT PRIMARY KEY,
        agent TEXT NOT NULL,
        agent_session_id TEXT NOT NULL,
        cwd TEXT,
        tmux_socket TEXT,
        tmux_server_pid INTEGER,
        tmux_pane TEXT,
        token TEXT,
        attribution TEXT NOT NULL,
        state TEXT NOT NULL,
        state_since INTEGER NOT NULL,
        last_event_at INTEGER NOT NULL,
        last_hook_event TEXT NOT NULL,
        last_message TEXT,
        UNIQUE (agent, agent_session_id)
    );
    "#,
];

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("ambiguous: {0}")]
    Ambiguous(String),
    #[error("already exists: {0}")]
    Conflict(String),
    #[error("database schema v{found} is newer than this ghostmc (v{supported}); upgrade ghostmc")]
    TooNew { found: i64, supported: i64 },
    #[error("invalid: {0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

pub struct Store {
    conn: Connection,
}

fn map_conflict(e: rusqlite::Error, what: &str) -> StoreError {
    match &e {
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            StoreError::Conflict(what.to_owned())
        }
        _ => StoreError::Sqlite(e),
    }
}

fn clean_tags(tags: &[String]) -> Vec<String> {
    let mut v: Vec<String> = tags
        .iter()
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    v.sort();
    v.dedup();
    v
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            crate::dirs::ensure_private_dir(dir)?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_default() -> Result<Self> {
        Self::open(&crate::dirs::db_path())
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        // WAL lets hook processes append while the host reads.
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<()> {
        let current: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if current > SCHEMA_VERSION {
            return Err(StoreError::TooNew {
                found: current,
                supported: SCHEMA_VERSION,
            });
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
            tx.commit()?;
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    // ---------------------------------------------------------------- devices

    fn device_from_row(r: &Row) -> rusqlite::Result<Device> {
        Ok(Device {
            id: DeviceId(r.get(0)?),
            name: r.get(1)?,
            os: r.get(2)?,
            hostname: r.get(3)?,
            is_local: r.get(4)?,
            ssh_target: r.get(5)?,
            remote_ghostmc: r.get(6)?,
            last_seen: r.get(7)?,
        })
    }

    const DEVICE_COLS: &'static str =
        "id, name, os, hostname, is_local, ssh_target, remote_ghostmc, last_seen";

    /// The device this registry belongs to, created on first use.
    pub fn local_device(&mut self) -> Result<Device> {
        let sql = format!(
            "SELECT {} FROM devices WHERE is_local = 1",
            Self::DEVICE_COLS
        );
        if let Some(d) = self
            .conn
            .query_row(&sql, [], Self::device_from_row)
            .optional()?
        {
            return Ok(d);
        }
        let hostname = local_hostname();
        let base = hostname.clone().unwrap_or_else(|| "local".into());
        let mut name = base.clone();
        let mut n = 2;
        while self.find_device(&name)?.is_some() {
            name = format!("{base}-{n}");
            n += 1;
        }
        let d = Device {
            id: DeviceId::generate(),
            name,
            os: std::env::consts::OS.into(),
            hostname,
            is_local: true,
            ssh_target: None,
            remote_ghostmc: None,
            last_seen: Some(now()),
        };
        self.insert_device(&d)?;
        Ok(d)
    }

    fn insert_device(&self, d: &Device) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO devices (id, name, os, hostname, is_local, ssh_target, remote_ghostmc, last_seen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![d.id.0, d.name, d.os, d.hostname, d.is_local, d.ssh_target, d.remote_ghostmc, d.last_seen],
            )
            .map_err(|e| map_conflict(e, &format!("device {:?}", d.name)))?;
        Ok(())
    }

    /// Register a remote device reachable over SSH.
    pub fn add_remote_device(
        &mut self,
        name: &str,
        ssh_target: &str,
        remote_ghostmc: Option<&str>,
    ) -> Result<Device> {
        validate_name(name)?;
        if ssh_target.trim().is_empty() || ssh_target.starts_with('-') {
            return Err(StoreError::Invalid(
                "ssh target must be a host, not an option".into(),
            ));
        }
        let d = Device {
            id: DeviceId::generate(),
            name: name.trim().into(),
            os: "unknown".into(),
            hostname: None,
            is_local: false,
            ssh_target: Some(ssh_target.trim().into()),
            remote_ghostmc: remote_ghostmc.map(str::to_owned),
            last_seen: None,
        };
        self.insert_device(&d)?;
        Ok(d)
    }

    pub fn find_device(&self, key: &str) -> Result<Option<Device>> {
        let sql = format!(
            "SELECT {} FROM devices WHERE id = ?1 OR name = ?1 COLLATE NOCASE",
            Self::DEVICE_COLS
        );
        Ok(self
            .conn
            .query_row(&sql, [key], Self::device_from_row)
            .optional()?)
    }

    pub fn device(&self, key: &str) -> Result<Device> {
        self.find_device(key)?
            .ok_or_else(|| StoreError::NotFound(format!("device {key:?}")))
    }

    pub fn list_devices(&self) -> Result<Vec<Device>> {
        let sql = format!(
            "SELECT {} FROM devices ORDER BY is_local DESC, name",
            Self::DEVICE_COLS
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map([], Self::device_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn touch_device(
        &self,
        id: &DeviceId,
        os: Option<&str>,
        hostname: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE devices SET last_seen = ?2, os = COALESCE(?3, os), hostname = COALESCE(?4, hostname) WHERE id = ?1",
            params![id.0, now(), os, hostname],
        )?;
        Ok(())
    }

    pub fn remove_device(&self, id: &DeviceId) -> Result<()> {
        let n = self.conn.execute(
            "DELETE FROM devices WHERE id = ?1 AND is_local = 0",
            [&id.0],
        )?;
        if n == 0 {
            return Err(StoreError::Invalid(
                "unknown device or the local device".into(),
            ));
        }
        Ok(())
    }

    // --------------------------------------------------------------- projects

    fn tags_for(&self, id: &ProjectId) -> Result<Vec<String>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT tag FROM project_tags WHERE project_id = ?1 ORDER BY tag")?;
        let rows = st.query_map([&id.0], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn project_from_row(r: &Row) -> rusqlite::Result<Project> {
        Ok(Project {
            id: ProjectId(r.get(0)?),
            name: r.get(1)?,
            notes: r.get(2)?,
            pinned: r.get(3)?,
            tags: Vec::new(),
            created_at: r.get(4)?,
            updated_at: r.get(5)?,
        })
    }

    pub fn create_project(
        &mut self,
        name: &str,
        tags: &[String],
        notes: Option<&str>,
    ) -> Result<Project> {
        validate_name(name)?;
        let ts = now();
        let p = Project {
            id: ProjectId::generate(),
            name: name.trim().into(),
            notes: notes.map(str::to_owned),
            pinned: false,
            tags: clean_tags(tags),
            created_at: ts,
            updated_at: ts,
        };
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO projects (id, name, notes, pinned, created_at, updated_at) VALUES (?1, ?2, ?3, 0, ?4, ?4)",
            params![p.id.0, p.name, p.notes, ts],
        )
        .map_err(|e| map_conflict(e, &format!("project {:?}", p.name)))?;
        for t in &p.tags {
            tx.execute(
                "INSERT INTO project_tags VALUES (?1, ?2)",
                params![p.id.0, t],
            )?;
        }
        tx.commit()?;
        Ok(p)
    }

    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let mut st = self.conn.prepare(
            "SELECT id, name, notes, pinned, created_at, updated_at FROM projects ORDER BY pinned DESC, name COLLATE NOCASE",
        )?;
        let mut v: Vec<Project> = st
            .query_map([], Self::project_from_row)?
            .collect::<rusqlite::Result<_>>()?;
        for p in &mut v {
            p.tags = self.tags_for(&p.id)?;
        }
        Ok(v)
    }

    /// Resolve by exact id, case-insensitive name, or unique id prefix.
    pub fn project(&self, key: &str) -> Result<Project> {
        let mut st = self.conn.prepare_cached(
            "SELECT id, name, notes, pinned, created_at, updated_at FROM projects
             WHERE id = ?1 OR name = ?1 COLLATE NOCASE",
        )?;
        let found = st.query_row([key], Self::project_from_row).optional()?;
        let mut p = match found {
            Some(p) => p,
            None => {
                let mut st = self.conn.prepare_cached(
                    "SELECT id, name, notes, pinned, created_at, updated_at FROM projects WHERE id LIKE ?1 || '%'",
                )?;
                let mut v: Vec<Project> = st
                    .query_map([key], Self::project_from_row)?
                    .collect::<rusqlite::Result<_>>()?;
                match v.len() {
                    0 => return Err(StoreError::NotFound(format!("project {key:?}"))),
                    1 => v.remove(0),
                    _ => return Err(StoreError::Ambiguous(format!("project prefix {key:?}"))),
                }
            }
        };
        p.tags = self.tags_for(&p.id)?;
        Ok(p)
    }

    pub fn update_project(
        &mut self,
        id: &ProjectId,
        name: Option<&str>,
        notes: Option<Option<&str>>,
        pinned: Option<bool>,
    ) -> Result<Project> {
        if let Some(n) = name {
            validate_name(n)?;
            self.conn
                .execute(
                    "UPDATE projects SET name = ?2 WHERE id = ?1",
                    params![id.0, n.trim()],
                )
                .map_err(|e| map_conflict(e, &format!("project {n:?}")))?;
        }
        if let Some(n) = notes {
            self.conn.execute(
                "UPDATE projects SET notes = ?2 WHERE id = ?1",
                params![id.0, n],
            )?;
        }
        if let Some(p) = pinned {
            self.conn.execute(
                "UPDATE projects SET pinned = ?2 WHERE id = ?1",
                params![id.0, p],
            )?;
        }
        self.conn.execute(
            "UPDATE projects SET updated_at = ?2 WHERE id = ?1",
            params![id.0, now()],
        )?;
        self.project(&id.0)
    }

    pub fn set_tags(
        &mut self,
        id: &ProjectId,
        add: &[String],
        remove: &[String],
    ) -> Result<Project> {
        let tx = self.conn.transaction()?;
        for t in clean_tags(add) {
            tx.execute(
                "INSERT OR IGNORE INTO project_tags VALUES (?1, ?2)",
                params![id.0, t],
            )?;
        }
        for t in clean_tags(remove) {
            tx.execute(
                "DELETE FROM project_tags WHERE project_id = ?1 AND tag = ?2",
                params![id.0, t],
            )?;
        }
        tx.execute(
            "UPDATE projects SET updated_at = ?2 WHERE id = ?1",
            params![id.0, now()],
        )?;
        tx.commit()?;
        self.project(&id.0)
    }

    pub fn delete_project(&mut self, id: &ProjectId) -> Result<()> {
        let n = self
            .conn
            .execute("DELETE FROM projects WHERE id = ?1", [&id.0])?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("project {id}")));
        }
        Ok(())
    }

    // -------------------------------------------------------------- locations

    fn location_from_row(r: &Row) -> rusqlite::Result<ProjectLocation> {
        Ok(ProjectLocation {
            id: LocationId(r.get(0)?),
            project_id: ProjectId(r.get(1)?),
            device_id: DeviceId(r.get(2)?),
            path: r.get(3)?,
            default_command: r.get(4)?,
            created_at: r.get(5)?,
        })
    }

    const LOC_COLS: &'static str = "id, project_id, device_id, path, default_command, created_at";

    /// Bind a project to a path on a device. `path` must already be
    /// normalized for that device (we cannot normalize remote paths).
    pub fn add_location(
        &mut self,
        project: &ProjectId,
        device: &DeviceId,
        path: &str,
        default_command: Option<&str>,
    ) -> Result<ProjectLocation> {
        if !path.starts_with('/') {
            return Err(StoreError::Invalid(format!(
                "location path must be absolute: {path:?}"
            )));
        }
        let l = ProjectLocation {
            id: LocationId::generate(),
            project_id: project.clone(),
            device_id: device.clone(),
            path: path.to_owned(),
            default_command: default_command.map(str::to_owned),
            created_at: now(),
        };
        self.conn
            .execute(
                "INSERT INTO locations (id, project_id, device_id, path, default_command, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![l.id.0, l.project_id.0, l.device_id.0, l.path, l.default_command, l.created_at],
            )
            .map_err(|e| map_conflict(e, &format!("location {path:?}")))?;
        Ok(l)
    }

    pub fn locations_for_project(&self, project: &ProjectId) -> Result<Vec<ProjectLocation>> {
        let sql = format!(
            "SELECT {} FROM locations WHERE project_id = ?1 ORDER BY created_at",
            Self::LOC_COLS
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map([&project.0], Self::location_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn locations_on_device(&self, device: &DeviceId) -> Result<Vec<ProjectLocation>> {
        let sql = format!(
            "SELECT {} FROM locations WHERE device_id = ?1 ORDER BY path",
            Self::LOC_COLS
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map([&device.0], Self::location_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn location(&self, id: &str) -> Result<ProjectLocation> {
        let sql = format!("SELECT {} FROM locations WHERE id = ?1", Self::LOC_COLS);
        self.conn
            .query_row(&sql, [id], Self::location_from_row)
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("location {id:?}")))
    }

    pub fn remove_location(&mut self, id: &LocationId) -> Result<()> {
        let n = self
            .conn
            .execute("DELETE FROM locations WHERE id = ?1", [&id.0])?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("location {id}")));
        }
        Ok(())
    }

    // ------------------------------------------------------ managed sessions

    pub fn record_managed_session(&self, r: &ManagedSessionRecord) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO managed_sessions
             (location_id, socket_path, server_pid, server_start, session_id, session_name, token, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                r.location_id.0, r.socket_path, r.server_pid, r.server_start,
                r.session_id, r.session_name, r.token, r.created_at
            ],
        )?;
        Ok(())
    }

    pub fn managed_sessions(&self) -> Result<Vec<ManagedSessionRecord>> {
        let mut st = self.conn.prepare(
            "SELECT location_id, socket_path, server_pid, server_start, session_id, session_name, token, created_at
             FROM managed_sessions ORDER BY created_at",
        )?;
        let rows = st.query_map([], |r| {
            Ok(ManagedSessionRecord {
                location_id: LocationId(r.get(0)?),
                socket_path: r.get(1)?,
                server_pid: r.get(2)?,
                server_start: r.get(3)?,
                session_id: r.get(4)?,
                session_name: r.get(5)?,
                token: r.get(6)?,
                created_at: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Drop records whose tmux server or session no longer exists. `alive`
    /// decides per record; returns the number removed. Exited sessions cannot
    /// be revived, so keeping their records would only mislead.
    pub fn prune_managed_sessions(
        &self,
        alive: impl Fn(&ManagedSessionRecord) -> bool,
    ) -> Result<usize> {
        let mut removed = 0;
        for r in self.managed_sessions()? {
            if !alive(&r) {
                removed += self
                    .conn
                    .execute("DELETE FROM managed_sessions WHERE token = ?1", [&r.token])?;
            }
        }
        Ok(removed)
    }

    // ----------------------------------------------------------------- agents

    /// Persist an event and fold it into its agent run. Returns `None` when
    /// the event was a duplicate, otherwise the updated run (if the event
    /// carried a session id) and whether its state changed.
    pub fn ingest_event(&mut self, e: &AgentEvent) -> Result<Option<(Option<AgentRun>, bool)>> {
        let tx = self.conn.transaction()?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO events
             (id, dedupe_key, ts, agent, hook_event, agent_session_id, turn_id, cwd, tmux_socket,
              tmux_server_pid, tmux_pane, token, detail, message)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                e.id.0,
                e.dedupe_key,
                e.ts,
                e.agent.as_str(),
                e.hook_event,
                e.agent_session_id,
                e.turn_id,
                e.cwd,
                e.tmux_socket,
                e.tmux_server_pid,
                e.tmux_pane,
                e.token,
                e.detail,
                e.message
            ],
        )?;
        if inserted == 0 {
            return Ok(None);
        }
        let Some(sid) = &e.agent_session_id else {
            tx.commit()?;
            return Ok(Some((None, false)));
        };
        let existing: Option<AgentRun> = tx
            .query_row(
                &format!(
                    "SELECT {} FROM agent_runs WHERE agent = ?1 AND agent_session_id = ?2",
                    RUN_COLS
                ),
                params![e.agent.as_str(), sid],
                run_from_row,
            )
            .optional()?;
        let new_state = e.transition();
        let (run, changed) = match existing {
            Some(mut run) => {
                // Events can arrive out of order (async hooks); never let an
                // older event overwrite newer state.
                if e.ts < run.last_event_at {
                    tx.commit()?;
                    return Ok(Some((Some(run), false)));
                }
                let mut changed = false;
                if let Some(s) = new_state
                    && s != run.state
                {
                    run.state = s;
                    run.state_since = e.ts;
                    changed = true;
                }
                run.last_event_at = e.ts;
                run.last_hook_event = e.hook_event.clone();
                // A message describes the state it arrived with; drop it when
                // the state moves on (e.g. "needs permission" after approval).
                if changed || e.message.is_some() {
                    run.last_message = e.message.clone();
                }
                if e.cwd.is_some() {
                    run.cwd = e.cwd.clone();
                }
                // Better evidence replaces weaker evidence, never the reverse.
                if e.attribution() <= run.attribution {
                    run.attribution = e.attribution();
                    run.tmux_socket = e.tmux_socket.clone().or(run.tmux_socket);
                    run.tmux_server_pid = e.tmux_server_pid.or(run.tmux_server_pid);
                    run.tmux_pane = e.tmux_pane.clone().or(run.tmux_pane);
                    run.token = e.token.clone().or(run.token);
                }
                (run, changed)
            }
            None => (
                AgentRun {
                    id: AgentRunId::generate(),
                    agent: e.agent,
                    agent_session_id: sid.clone(),
                    cwd: e.cwd.clone(),
                    tmux_socket: e.tmux_socket.clone(),
                    tmux_server_pid: e.tmux_server_pid,
                    tmux_pane: e.tmux_pane.clone(),
                    token: e.token.clone(),
                    attribution: e.attribution(),
                    state: new_state.unwrap_or(AgentState::Running),
                    state_since: e.ts,
                    last_event_at: e.ts,
                    last_hook_event: e.hook_event.clone(),
                    last_message: e.message.clone(),
                },
                true,
            ),
        };
        tx.execute(
            "INSERT OR REPLACE INTO agent_runs
             (id, agent, agent_session_id, cwd, tmux_socket, tmux_server_pid, tmux_pane, token,
              attribution, state, state_since, last_event_at, last_hook_event, last_message)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                run.id.0,
                run.agent.as_str(),
                run.agent_session_id,
                run.cwd,
                run.tmux_socket,
                run.tmux_server_pid,
                run.tmux_pane,
                run.token,
                run.attribution.as_str(),
                run.state.as_str(),
                run.state_since,
                run.last_event_at,
                run.last_hook_event,
                run.last_message
            ],
        )?;
        tx.commit()?;
        Ok(Some((Some(run), changed)))
    }

    /// Runs with activity in the last `since_secs` seconds, newest first.
    pub fn agent_runs(&self, since_secs: i64) -> Result<Vec<AgentRun>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {} FROM agent_runs WHERE last_event_at >= ?1 ORDER BY last_event_at DESC",
            RUN_COLS
        ))?;
        let rows = st.query_map([now() - since_secs], run_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn recent_events(&self, limit: usize) -> Result<Vec<AgentEvent>> {
        let mut st = self.conn.prepare(
            "SELECT id, dedupe_key, ts, agent, hook_event, agent_session_id, turn_id, cwd, tmux_socket,
                    tmux_server_pid, tmux_pane, token, detail, message
             FROM events ORDER BY seq DESC LIMIT ?1",
        )?;
        let rows = st.query_map([limit as i64], |r| {
            Ok(AgentEvent {
                id: EventId(r.get(0)?),
                dedupe_key: r.get(1)?,
                ts: r.get(2)?,
                agent: AgentKind::parse(&r.get::<_, String>(3)?).unwrap_or(AgentKind::Claude),
                hook_event: r.get(4)?,
                agent_session_id: r.get(5)?,
                turn_id: r.get(6)?,
                cwd: r.get(7)?,
                tmux_socket: r.get(8)?,
                tmux_server_pid: r.get(9)?,
                tmux_pane: r.get(10)?,
                token: r.get(11)?,
                detail: r.get(12)?,
                message: r.get(13)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Bound storage: drop events older than `max_age` seconds.
    pub fn prune_events(&self, max_age: i64) -> Result<usize> {
        Ok(self
            .conn
            .execute("DELETE FROM events WHERE ts < ?1", [now() - max_age])?)
    }
}

const RUN_COLS: &str =
    "id, agent, agent_session_id, cwd, tmux_socket, tmux_server_pid, tmux_pane, token,
    attribution, state, state_since, last_event_at, last_hook_event, last_message";

fn run_from_row(r: &Row) -> rusqlite::Result<AgentRun> {
    Ok(AgentRun {
        id: AgentRunId(r.get(0)?),
        agent: AgentKind::parse(&r.get::<_, String>(1)?).unwrap_or(AgentKind::Claude),
        agent_session_id: r.get(2)?,
        cwd: r.get(3)?,
        tmux_socket: r.get(4)?,
        tmux_server_pid: r.get(5)?,
        tmux_pane: r.get(6)?,
        token: r.get(7)?,
        attribution: Attribution::parse(&r.get::<_, String>(8)?),
        state: AgentState::parse(&r.get::<_, String>(9)?),
        state_since: r.get(10)?,
        last_event_at: r.get(11)?,
        last_hook_event: r.get(12)?,
        last_message: r.get(13)?,
    })
}

fn validate_name(name: &str) -> Result<()> {
    let n = name.trim();
    if n.is_empty() || n.len() > 120 || n.chars().any(char::is_control) {
        return Err(StoreError::Invalid(format!("bad name {name:?}")));
    }
    Ok(())
}

pub fn local_hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let s = String::from_utf8_lossy(&buf[..end]).trim().to_owned();
    // Strip the macOS ".local" suffix for a friendlier default device name.
    let s = s.strip_suffix(".local").map(str::to_owned).unwrap_or(s);
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    #[test]
    fn migrations_are_idempotent_and_versioned() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("db/x.sqlite3");
        Store::open(&p).unwrap();
        let s = Store::open(&p).unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn refuses_newer_schema() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.sqlite3");
        {
            let c = Connection::open(&p).unwrap();
            c.pragma_update(None, "user_version", 99).unwrap();
        }
        assert!(matches!(
            Store::open(&p),
            Err(StoreError::TooNew { found: 99, .. })
        ));
    }

    #[test]
    fn local_device_is_stable() {
        let mut s = store();
        let a = s.local_device().unwrap();
        let b = s.local_device().unwrap();
        assert_eq!(a.id, b.id);
        assert!(a.is_local);
    }

    #[test]
    fn project_crud_and_tags() {
        let mut s = store();
        let p = s
            .create_project(
                "Ghost",
                &["Rust".into(), " rust ".into(), "tools".into()],
                None,
            )
            .unwrap();
        assert_eq!(p.tags, vec!["rust", "tools"]);
        assert!(matches!(
            s.create_project("ghost", &[], None),
            Err(StoreError::Conflict(_))
        ));
        assert_eq!(s.project("GHOST").unwrap().id, p.id);
        assert_eq!(s.project(&p.id.0[..8]).unwrap().id, p.id);
        let p = s
            .set_tags(&p.id, &["ui".into()], &["tools".into()])
            .unwrap();
        assert_eq!(p.tags, vec!["rust", "ui"]);
        let p = s
            .update_project(&p.id, Some("Ghost May Cry"), None, Some(true))
            .unwrap();
        assert!(p.pinned);
        s.delete_project(&p.id).unwrap();
        assert!(matches!(
            s.project("Ghost May Cry"),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn ambiguous_prefix_is_reported() {
        let mut s = store();
        s.create_project("a", &[], None).unwrap();
        s.create_project("b", &[], None).unwrap();
        assert!(matches!(s.project("prj_"), Err(StoreError::Ambiguous(_))));
    }

    #[test]
    fn locations_cascade_and_require_absolute_paths() {
        let mut s = store();
        let d = s.local_device().unwrap();
        let p = s.create_project("x", &[], None).unwrap();
        assert!(s.add_location(&p.id, &d.id, "relative", None).is_err());
        let l = s.add_location(&p.id, &d.id, "/w/x", Some("codex")).unwrap();
        assert!(matches!(
            s.add_location(&p.id, &d.id, "/w/x", None),
            Err(StoreError::Conflict(_))
        ));
        // The same path may belong to a second project only explicitly.
        let q = s.create_project("y", &[], None).unwrap();
        s.add_location(&q.id, &d.id, "/w/x", None).unwrap();
        assert_eq!(s.locations_for_project(&p.id).unwrap(), vec![l]);
        s.delete_project(&p.id).unwrap();
        assert_eq!(s.locations_on_device(&d.id).unwrap().len(), 1);
    }

    #[test]
    fn remote_devices() {
        let mut s = store();
        assert!(
            s.add_remote_device("mini", "-oProxyCommand=x", None)
                .is_err()
        );
        let d = s.add_remote_device("mini", "me@mini.tail", None).unwrap();
        assert!(!d.is_local);
        assert_eq!(s.device("MINI").unwrap().id, d.id);
        let local = s.local_device().unwrap();
        assert!(s.remove_device(&local.id).is_err());
        s.remove_device(&d.id).unwrap();
    }

    fn ev(store_ts: i64, name: &str, extra: serde_json::Value) -> AgentEvent {
        let mut p = json!({"session_id": "S1", "hook_event_name": name, "cwd": "/w/x"});
        if let (Some(o), Some(e)) = (p.as_object_mut(), extra.as_object()) {
            o.extend(e.clone());
        }
        AgentEvent::from_hook(AgentKind::Claude, &p, &HookEnv::default(), store_ts).unwrap()
    }

    #[test]
    fn ingest_builds_run_state() {
        let mut s = store();
        let (run, changed) = s
            .ingest_event(&ev(10, "SessionStart", json!({})))
            .unwrap()
            .unwrap();
        assert!(changed);
        assert_eq!(run.unwrap().state, AgentState::Idle);
        s.ingest_event(&ev(11, "UserPromptSubmit", json!({})))
            .unwrap();
        let (run, changed) = s
            .ingest_event(&ev(
                12,
                "Notification",
                json!({"notification_type": "permission_prompt", "message": "Allow Bash?"}),
            ))
            .unwrap()
            .unwrap();
        let run = run.unwrap();
        assert!(changed);
        assert_eq!(run.state, AgentState::NeedsApproval);
        assert_eq!(run.last_message.as_deref(), Some("Allow Bash?"));
        s.ingest_event(&ev(13, "Stop", json!({}))).unwrap();
        let runs = s.agent_runs(i64::MAX / 2).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].state, AgentState::TurnEnded);
        assert_eq!(
            runs[0].last_message, None,
            "stale approval text must not outlive the state"
        );
    }

    #[test]
    fn duplicates_and_stale_events_are_ignored() {
        let mut s = store();
        let e = ev(10, "Stop", json!({}));
        assert!(s.ingest_event(&e).unwrap().is_some());
        let dup = AgentEvent {
            id: EventId::generate(),
            ..e.clone()
        };
        assert!(s.ingest_event(&dup).unwrap().is_none());
        s.ingest_event(&ev(20, "UserPromptSubmit", json!({})))
            .unwrap();
        let (run, changed) = s.ingest_event(&ev(15, "Stop", json!({}))).unwrap().unwrap();
        assert!(!changed);
        assert_eq!(run.unwrap().state, AgentState::Running);
    }

    #[test]
    fn weaker_evidence_does_not_replace_pane() {
        let mut s = store();
        let p = json!({"session_id": "S", "hook_event_name": "SessionStart"});
        let env = HookEnv {
            tmux: Some("/t/s,1,0".into()),
            tmux_pane: Some("%3".into()),
            token: None,
        };
        s.ingest_event(&AgentEvent::from_hook(AgentKind::Codex, &p, &env, 1).unwrap())
            .unwrap();
        let p2 = json!({"session_id": "S", "hook_event_name": "Stop"});
        let (run, _) = s
            .ingest_event(
                &AgentEvent::from_hook(AgentKind::Codex, &p2, &HookEnv::default(), 2).unwrap(),
            )
            .unwrap()
            .unwrap();
        let run = run.unwrap();
        assert_eq!(run.tmux_pane.as_deref(), Some("%3"));
        assert_eq!(run.attribution, Attribution::TmuxPane);
    }

    #[test]
    fn managed_session_records_prune() {
        let mut s = store();
        let d = s.local_device().unwrap();
        let p = s.create_project("x", &[], None).unwrap();
        let l = s.add_location(&p.id, &d.id, "/w/x", None).unwrap();
        let rec = |tok: &str, pid| ManagedSessionRecord {
            location_id: l.id.clone(),
            socket_path: "/tmp/s".into(),
            server_pid: pid,
            server_start: 1,
            session_id: format!("${pid}"),
            session_name: "gmc".into(),
            token: tok.into(),
            created_at: 1,
        };
        s.record_managed_session(&rec("a", 1)).unwrap();
        s.record_managed_session(&rec("b", 2)).unwrap();
        assert_eq!(s.prune_managed_sessions(|r| r.server_pid == 1).unwrap(), 1);
        assert_eq!(s.managed_sessions().unwrap().len(), 1);
    }
}
