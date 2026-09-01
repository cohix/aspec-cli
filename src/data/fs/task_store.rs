//! SQLite persistence for squad tasks and their evaluation runs.
//!
//! This store is opened only by the squad daemon process. CLI and TUI code use
//! the command gateway instead of importing or constructing it directly.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use crate::data::error::DataError;

/// A persisted scheduled squad task.
///
/// # Workspace semantics (WI 0106)
///
/// `repo_scope` is the task's **effective root** — the single host directory a
/// run is bound to — captured once when the task is created and never widened
/// or re-derived afterwards. It is one of:
///
/// * the durable per-task workspace `~/.awman/squad/tasks/<name>/workspace/`
///   ("Default Task Workspace"), which is a plain directory and *not* a git
///   repository; or
/// * a custom folder or repository the user chose ("Custom Folder / Repo").
///
/// `mount_scope` records which of those it is, and therefore whether a run may
/// use a git worktree:
///
/// * [`MountScope::GitRoot`] / [`MountScope::Cwd`] — the effective root was
///   inside a git repository at creation time, so every run is worktree-
///   isolated ([`Task::uses_worktree`] is `true`). The two variants differ only
///   in how much of that repository is mounted.
/// * [`MountScope::Directory`] — the effective root is a plain directory (the
///   default task workspace, or a custom folder that was not a git repository
///   at creation). It is mounted whole and directly; no worktree is created,
///   because there is no repository to branch one from.
///
/// Worktree usage is therefore derived from "was the effective task root a git
/// repository", **as evaluated at creation** — exactly like `mount_scope`'s
/// existing capture-once convention. If a custom repository is later deleted,
/// moved, or de-initialised, the run fails loudly at git-root resolution rather
/// than silently degrading to a direct mount.
///
/// Independently of which mode was chosen, the durable workspace directory is
/// always created and always mounted into the task's leader container at the
/// `context(workflow)` path, so a task has one task-scoped persistent location
/// for its whole lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub name: String,
    pub description: String,
    /// The task's effective root. See the type-level docs.
    pub repo_scope: PathBuf,
    /// What kind of root `repo_scope` is, and thus whether runs use a
    /// worktree. See the type-level docs.
    pub mount_scope: MountScope,
    /// Raw overlay specs (`dir()`, `ssh()`, `env()`, `skill()`) captured at
    /// creation, validated for syntax then, and merged additively with the
    /// global/repo/`AWMAN_OVERLAYS` sources at run time.
    pub overlays: Vec<String>,
    pub interval_secs: u64,
    pub status: TaskStatus,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub backoff_until: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_run_at: Option<DateTime<Utc>>,
    /// Outcome of the most recent run, read alongside the task in the same
    /// query. Derived, never stored on the task row: `squad_runs` remains the
    /// only record of a run.
    ///
    /// It travels with the task because the two are always read together — the
    /// task grid shows every card's last-run outcome, and asking for run
    /// history once per card would be an N+1 over the daemon's HTTP surface.
    /// `None` means the task has never run.
    #[serde(default)]
    pub last_run_status: Option<RunStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Active,
    Paused,
}

impl TaskStatus {
    fn as_db(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
        }
    }

    fn from_db(value: &str) -> Result<Self, DataError> {
        match value {
            "active" => Ok(Self::Active),
            "paused" => Ok(Self::Paused),
            _ => Err(DataError::Other(format!(
                "invalid squad task status {value:?}"
            ))),
        }
    }
}

/// What kind of root a task's `repo_scope` is, captured once at creation.
///
/// This is the single source of truth for whether a run is worktree-isolated:
/// see [`Task`]'s type-level documentation and [`Task::uses_worktree`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MountScope {
    /// Inside a git repository; mount only the captured directory.
    Cwd,
    /// Inside a git repository; mount the whole git root.
    GitRoot,
    /// A plain directory that was not a git repository at creation — the
    /// durable default task workspace, or a custom folder the user kept after
    /// being warned it is not a repository root. Mounted whole and directly,
    /// with no worktree, because there is no repository to branch one from.
    Directory,
}

impl MountScope {
    fn as_db(self) -> &'static str {
        match self {
            Self::Cwd => "cwd",
            Self::GitRoot => "gitroot",
            Self::Directory => "directory",
        }
    }

    fn from_db(value: &str) -> Result<Self, DataError> {
        match value {
            "cwd" => Ok(Self::Cwd),
            "gitroot" => Ok(Self::GitRoot),
            "directory" => Ok(Self::Directory),
            _ => Err(DataError::Other(format!(
                "invalid squad mount scope {value:?}"
            ))),
        }
    }

    /// Whether a task with this scope resolves to a git repository, and
    /// therefore whether its runs are worktree-isolated.
    pub fn is_git_repo(self) -> bool {
        !matches!(self, Self::Directory)
    }
}

/// Which workspace a task is bound to, as chosen at creation.
///
/// This is the *request* shape: the gateway resolves it, once, into the
/// task's stored `repo_scope` + [`MountScope`] pair. It never appears on a
/// persisted [`Task`], because after creation there is only one answer — the
/// effective root — and re-deriving it per run is exactly what the
/// capture-once convention forbids.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "path", rename_all = "lowercase")]
pub enum TaskWorkspace {
    /// The durable `~/.awman/squad/tasks/<name>/workspace/` directory. Created
    /// at task creation and never deleted, emptied, or replaced until the task
    /// itself is removed.
    #[default]
    Default,
    /// A folder or repository the user chose explicitly.
    Custom(PathBuf),
}

impl Task {
    /// Whether this task's runs are worktree-isolated.
    ///
    /// Derived from the effective task root's kind as captured at creation —
    /// never re-derived from the filesystem per run. A custom repository that
    /// disappears between runs fails loudly at git-root resolution instead of
    /// quietly becoming a direct mount.
    pub fn uses_worktree(&self) -> bool {
        self.mount_scope.is_git_repo()
    }
}

/// Typed identifier for one evaluation run.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub String);

impl RunId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    NotTriggered,
    WorkflowExecuted,
    Failed,
    Interrupted,
}

impl RunStatus {
    fn as_db(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::NotTriggered => "not_triggered",
            Self::WorkflowExecuted => "workflow_executed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }

    fn from_db(value: &str) -> Result<Self, DataError> {
        match value {
            "running" => Ok(Self::Running),
            "not_triggered" => Ok(Self::NotTriggered),
            "workflow_executed" => Ok(Self::WorkflowExecuted),
            "failed" => Ok(Self::Failed),
            "interrupted" => Ok(Self::Interrupted),
            _ => Err(DataError::Other(format!(
                "invalid squad run status {value:?}"
            ))),
        }
    }
}

/// Terminal detail supplied when a run finishes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunDetail {
    pub workflow_path: Option<PathBuf>,
    pub workflow_state_path: Option<PathBuf>,
    pub error: Option<String>,
}

/// A persisted squad evaluation attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub task_id: String,
    pub status: RunStatus,
    pub workflow_path: Option<PathBuf>,
    pub workflow_state_path: Option<PathBuf>,
    pub session_id: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

/// SQLite-backed task store. The squad daemon is its only constructor.
pub struct TaskStore {
    conn: Mutex<Connection>,
}

impl TaskStore {
    /// Open the shared database file and enable WAL. Opening performs no schema
    /// work: the daemon startup order is `open` → [`migrate`](Self::migrate) →
    /// `reconcile_orphaned_runs`, so the migration step is separately callable
    /// and separately testable. Call this only from the squad daemon.
    pub fn open(db_file: &Path) -> Result<Self, DataError> {
        let parent = db_file.parent().ok_or_else(|| {
            DataError::Other(format!(
                "database path has no parent: {}",
                db_file.display()
            ))
        })?;
        std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
        let conn = Connection::open(db_file)?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Apply squad's schema. Idempotent: every statement is
    /// `CREATE ... IF NOT EXISTS` or an add-column-if-missing, so re-running it
    /// against an already-migrated database is a no-op.
    pub fn migrate(&self) -> Result<(), DataError> {
        Self::migrate_conn(&self.lock())
    }

    fn migrate_conn(conn: &Connection) -> Result<(), DataError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS squad_tasks (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                description TEXT NOT NULL,
                repo_scope TEXT NOT NULL,
                mount_scope TEXT NOT NULL,
                overlays TEXT,
                interval_secs INTEGER NOT NULL,
                status TEXT NOT NULL DEFAULT 'active',
                agent TEXT,
                model TEXT,
                backoff_until TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                last_run_at TEXT
            );

            CREATE TABLE IF NOT EXISTS squad_runs (
                id TEXT PRIMARY KEY,
                task_id TEXT NOT NULL REFERENCES squad_tasks(id),
                status TEXT NOT NULL,
                workflow_path TEXT,
                workflow_state_path TEXT,
                session_id TEXT,
                started_at TEXT NOT NULL,
                finished_at TEXT,
                error TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_squad_runs_task
            ON squad_runs(task_id, started_at DESC);",
        )?;
        Self::add_column_if_missing(conn, "squad_runs", "workflow_state_path", "TEXT")?;
        // Overlay specs are stored as a JSON array of the raw spec strings —
        // the same text `--overlay` accepts — so the column round-trips a
        // `Vec<String>` without inventing a delimiter that a spec could
        // contain. `NULL` (a row written before this column existed) decodes
        // to an empty list.
        Self::add_column_if_missing(conn, "squad_tasks", "overlays", "TEXT")?;
        Ok(())
    }

    fn add_column_if_missing(
        conn: &Connection,
        table: &str,
        column: &str,
        decl: &str,
    ) -> Result<(), DataError> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            if row.get::<_, String>(1)? == column {
                return Ok(());
            }
        }
        drop(rows);
        drop(stmt);
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl};"))?;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("task store mutex poisoned")
    }

    pub fn create(&self, task: &Task) -> Result<(), DataError> {
        let interval_secs = i64::try_from(task.interval_secs).map_err(|_| {
            DataError::Other(format!(
                "task interval_secs {} exceeds SQLite's signed integer range",
                task.interval_secs
            ))
        })?;
        let conn = self.lock();
        let result = conn.execute(
            "INSERT INTO squad_tasks
             (id, name, description, repo_scope, mount_scope, overlays, interval_secs, status, agent, model,
              backoff_until, created_at, updated_at, last_run_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                task.id,
                task.name,
                task.description,
                task.repo_scope.to_string_lossy(),
                task.mount_scope.as_db(),
                encode_overlays(&task.overlays)?,
                interval_secs,
                task.status.as_db(),
                task.agent,
                task.model,
                timestamp_opt(task.backoff_until),
                timestamp(task.created_at),
                timestamp(task.updated_at),
                timestamp_opt(task.last_run_at),
            ],
        );
        match result {
            Ok(_) => Ok(()),
            Err(error) if is_unique_name_violation(&error) => Err(DataError::Other(format!(
                "a task named {:?} already exists",
                task.name
            ))),
            Err(error) => Err(DataError::Sqlite(error)),
        }
    }

    pub fn get(&self, name: &str) -> Result<Option<Task>, DataError> {
        let conn = self.lock();
        let raw = conn
            .query_row(
                "SELECT id, name, description, repo_scope, mount_scope, overlays, interval_secs, status, agent, model,
                        backoff_until, created_at, updated_at, last_run_at,
                        (SELECT r.status FROM squad_runs r
                          WHERE r.task_id = squad_tasks.id
                          ORDER BY r.started_at DESC LIMIT 1)
                 FROM squad_tasks WHERE name = ?1",
                [name],
                task_from_row,
            )
            .optional()?;
        raw.map(task_from_raw).transpose()
    }

    pub fn list(&self) -> Result<Vec<Task>, DataError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            // The latest run's status rides along in the same statement: the
            // task grid needs it for every card, and one query per card would
            // be an N+1 across the daemon's HTTP surface.
            "SELECT id, name, description, repo_scope, mount_scope, overlays, interval_secs, status, agent, model,
                    backoff_until, created_at, updated_at, last_run_at,
                    (SELECT r.status FROM squad_runs r
                      WHERE r.task_id = squad_tasks.id
                      ORDER BY r.started_at DESC LIMIT 1)
             FROM squad_tasks ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], task_from_row)?;
        collect_tasks(rows)
    }

    pub fn set_status(&self, name: &str, status: TaskStatus) -> Result<bool, DataError> {
        let conn = self.lock();
        Ok(conn.execute(
            "UPDATE squad_tasks SET status = ?1, updated_at = ?2 WHERE name = ?3",
            params![status.as_db(), timestamp(Utc::now()), name],
        )? > 0)
    }

    pub fn delete(&self, name: &str) -> Result<bool, DataError> {
        let conn = self.lock();
        Ok(conn.execute("DELETE FROM squad_tasks WHERE name = ?1", [name])? > 0)
    }

    /// Select due tasks wholly in SQL. Do not duplicate this predicate in
    /// Rust: it is the concurrency and backoff admission rule.
    pub fn due_for_evaluation(&self, now: DateTime<Utc>) -> Result<Vec<Task>, DataError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT c.id, c.name, c.description, c.repo_scope, c.mount_scope, c.overlays, c.interval_secs,
                    c.status, c.agent, c.model, c.backoff_until, c.created_at, c.updated_at,
                    c.last_run_at,
                    (SELECT r2.status FROM squad_runs r2
                      WHERE r2.task_id = c.id
                      ORDER BY r2.started_at DESC LIMIT 1)
             FROM squad_tasks c
             WHERE c.status = 'active'
               AND (c.backoff_until IS NULL OR c.backoff_until <= :now)
               AND (c.last_run_at IS NULL
                    OR (julianday(:now) - julianday(c.last_run_at)) * 86400.0 >= c.interval_secs)
               AND NOT EXISTS (SELECT 1 FROM squad_runs r
                               WHERE r.task_id = c.id AND r.status = 'running')
             ORDER BY c.last_run_at IS NOT NULL, c.last_run_at ASC",
        )?;
        let now = timestamp(now);
        let rows = stmt.query_map(&[(":now", &now)], task_from_row)?;
        collect_tasks(rows)
    }

    pub fn start_run(
        &self,
        task_id: &str,
        session_id: Option<&str>,
        started_at: DateTime<Utc>,
    ) -> Result<RunId, DataError> {
        let id = RunId::new();
        let conn = self.lock();
        conn.execute(
            "INSERT INTO squad_runs (id, task_id, status, session_id, started_at)
             VALUES (?1, ?2, 'running', ?3, ?4)",
            params![id.as_str(), task_id, session_id, timestamp(started_at)],
        )?;
        conn.execute(
            "UPDATE squad_tasks SET last_run_at = ?1, updated_at = ?1 WHERE id = ?2",
            params![timestamp(started_at), task_id],
        )?;
        Ok(id)
    }

    pub fn finish_run(
        &self,
        run_id: &RunId,
        status: RunStatus,
        detail: &RunDetail,
        finished_at: DateTime<Utc>,
    ) -> Result<bool, DataError> {
        let conn = self.lock();
        Ok(conn.execute(
            "UPDATE squad_runs
             SET status = ?1, workflow_path = ?2, workflow_state_path = ?3, error = ?4, finished_at = ?5
             WHERE id = ?6 AND status = 'running'",
            params![
                status.as_db(),
                path_opt(detail.workflow_path.as_deref()),
                path_opt(detail.workflow_state_path.as_deref()),
                detail.error,
                timestamp(finished_at),
                run_id.as_str(),
            ],
        )? > 0)
    }

    pub fn set_backoff(&self, name: &str, until: Option<DateTime<Utc>>) -> Result<(), DataError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE squad_tasks SET backoff_until = ?1, updated_at = ?2 WHERE name = ?3",
            params![timestamp_opt(until), timestamp(Utc::now()), name],
        )?;
        Ok(())
    }

    /// Record the engine's persisted `WorkflowState` file on a run row as soon
    /// as the generated workflow starts, so the daemon's workflow route can
    /// read it back while the run is still in flight.
    pub fn set_workflow_state_path(
        &self,
        run_id: &RunId,
        workflow_path: Option<&Path>,
        workflow_state_path: Option<&Path>,
    ) -> Result<(), DataError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE squad_runs SET workflow_path = ?1, workflow_state_path = ?2 WHERE id = ?3",
            params![
                path_opt(workflow_path),
                path_opt(workflow_state_path),
                run_id.as_str(),
            ],
        )?;
        Ok(())
    }

    /// The most recent runs for a task, newest first.
    pub fn runs_for(&self, name: &str, limit: usize) -> Result<Vec<Run>, DataError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT r.id, r.task_id, r.status, r.workflow_path, r.workflow_state_path,
                    r.session_id, r.started_at, r.finished_at, r.error
             FROM squad_runs r
             JOIN squad_tasks c ON c.id = r.task_id
             WHERE c.name = ?1
             ORDER BY r.started_at DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![name, limit as i64], run_from_row)?;
        let mut runs = Vec::new();
        for row in rows {
            runs.push(run_from_raw(row?)?);
        }
        Ok(runs)
    }

    pub fn running_run_for(&self, task_id: &str) -> Result<Option<Run>, DataError> {
        let conn = self.lock();
        let raw = conn
            .query_row(
                "SELECT id, task_id, status, workflow_path, workflow_state_path, session_id, started_at,
                        finished_at, error
                 FROM squad_runs WHERE task_id = ?1 AND status = 'running'
                 ORDER BY started_at DESC LIMIT 1",
                [task_id],
                run_from_row,
            )
            .optional()?;
        raw.map(run_from_raw).transpose()
    }

    pub fn reconcile_orphaned_runs(&self, now: DateTime<Utc>) -> Result<usize, DataError> {
        let conn = self.lock();
        Ok(conn.execute(
            "UPDATE squad_runs
             SET status = 'interrupted', finished_at = ?1,
                 error = 'daemon restarted while this run was in flight'
             WHERE status = 'running'",
            [timestamp(now)],
        )?)
    }
}

#[derive(Debug)]
struct RawTask {
    id: String,
    name: String,
    description: String,
    repo_scope: String,
    mount_scope: String,
    overlays: Option<String>,
    interval_secs: i64,
    status: String,
    agent: Option<String>,
    model: Option<String>,
    backoff_until: Option<String>,
    created_at: String,
    updated_at: String,
    last_run_at: Option<String>,
    last_run_status: Option<String>,
}

fn task_from_row(row: &Row<'_>) -> rusqlite::Result<RawTask> {
    Ok(RawTask {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        repo_scope: row.get(3)?,
        mount_scope: row.get(4)?,
        overlays: row.get(5)?,
        interval_secs: row.get(6)?,
        status: row.get(7)?,
        agent: row.get(8)?,
        model: row.get(9)?,
        backoff_until: row.get(10)?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
        last_run_at: row.get(13)?,
        last_run_status: row.get(14)?,
    })
}

fn task_from_raw(raw: RawTask) -> Result<Task, DataError> {
    Ok(Task {
        id: raw.id,
        name: raw.name,
        description: raw.description,
        repo_scope: raw.repo_scope.into(),
        mount_scope: MountScope::from_db(&raw.mount_scope)?,
        overlays: decode_overlays(raw.overlays.as_deref())?,
        interval_secs: u64::try_from(raw.interval_secs).map_err(|_| {
            DataError::Other(format!(
                "invalid negative squad interval_secs {}",
                raw.interval_secs
            ))
        })?,
        status: TaskStatus::from_db(&raw.status)?,
        agent: raw.agent,
        model: raw.model,
        backoff_until: timestamp_parse_opt(raw.backoff_until)?,
        created_at: timestamp_parse(&raw.created_at)?,
        updated_at: timestamp_parse(&raw.updated_at)?,
        last_run_at: timestamp_parse_opt(raw.last_run_at)?,
        last_run_status: raw
            .last_run_status
            .as_deref()
            .map(RunStatus::from_db)
            .transpose()?,
    })
}

fn collect_tasks<F>(rows: rusqlite::MappedRows<'_, F>) -> Result<Vec<Task>, DataError>
where
    F: FnMut(&Row<'_>) -> rusqlite::Result<RawTask>,
{
    rows.map(|row| row.map_err(DataError::from).and_then(task_from_raw))
        .collect()
}

#[derive(Debug)]
struct RawRun {
    id: String,
    task_id: String,
    status: String,
    workflow_path: Option<String>,
    workflow_state_path: Option<String>,
    session_id: Option<String>,
    started_at: String,
    finished_at: Option<String>,
    error: Option<String>,
}

fn run_from_row(row: &Row<'_>) -> rusqlite::Result<RawRun> {
    Ok(RawRun {
        id: row.get(0)?,
        task_id: row.get(1)?,
        status: row.get(2)?,
        workflow_path: row.get(3)?,
        workflow_state_path: row.get(4)?,
        session_id: row.get(5)?,
        started_at: row.get(6)?,
        finished_at: row.get(7)?,
        error: row.get(8)?,
    })
}

fn run_from_raw(raw: RawRun) -> Result<Run, DataError> {
    Ok(Run {
        id: raw.id,
        task_id: raw.task_id,
        status: RunStatus::from_db(&raw.status)?,
        workflow_path: raw.workflow_path.map(PathBuf::from),
        workflow_state_path: raw.workflow_state_path.map(PathBuf::from),
        session_id: raw.session_id,
        started_at: timestamp_parse(&raw.started_at)?,
        finished_at: timestamp_parse_opt(raw.finished_at)?,
        error: raw.error,
    })
}

/// Encode a task's raw overlay specs for the `squad_tasks.overlays` column.
/// A JSON array of the same strings `--overlay` accepts, so no delimiter can
/// collide with a spec's own punctuation.
fn encode_overlays(value: &[String]) -> Result<String, DataError> {
    serde_json::to_string(value)
        .map_err(|error| DataError::Other(format!("encoding squad task overlays: {error}")))
}

/// Decode the `squad_tasks.overlays` column. `NULL` — a row written before the
/// column existed — decodes to an empty list rather than an error.
fn decode_overlays(value: Option<&str>) -> Result<Vec<String>, DataError> {
    match value {
        None => Ok(Vec::new()),
        Some(raw) if raw.trim().is_empty() => Ok(Vec::new()),
        Some(raw) => serde_json::from_str(raw).map_err(|error| {
            DataError::Other(format!("invalid squad task overlays {raw:?}: {error}"))
        }),
    }
}

fn timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339()
}
fn timestamp_opt(value: Option<DateTime<Utc>>) -> Option<String> {
    value.map(timestamp)
}
fn path_opt(value: Option<&Path>) -> Option<String> {
    value.map(|path| path.to_string_lossy().into_owned())
}
fn timestamp_parse(value: &str) -> Result<DateTime<Utc>, DataError> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|error| DataError::Other(format!("invalid squad timestamp {value:?}: {error}")))
}
fn timestamp_parse_opt(value: Option<String>) -> Result<Option<DateTime<Utc>>, DataError> {
    value.as_deref().map(timestamp_parse).transpose()
}
fn is_unique_name_violation(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(code, _) if code.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(name: &str, now: DateTime<Utc>) -> Task {
        Task {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            description: "test".into(),
            repo_scope: PathBuf::from("/repo"),
            mount_scope: MountScope::GitRoot,
            overlays: Vec::new(),
            interval_secs: 60,
            status: TaskStatus::Active,
            agent: None,
            model: None,
            backoff_until: None,
            created_at: now,
            updated_at: now,
            last_run_at: None,
            last_run_status: None,
        }
    }

    #[test]
    fn due_query_enforces_every_admission_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TaskStore::open(&tmp.path().join("awman.db")).unwrap();
        store.migrate().unwrap();
        let now = DateTime::parse_from_rfc3339("2026-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let due = task("due", now);
        store.create(&due).unwrap();
        let mut paused = task("paused", now);
        paused.status = TaskStatus::Paused;
        store.create(&paused).unwrap();
        let mut backoff = task("backoff", now);
        backoff.backoff_until = Some(now + chrono::Duration::minutes(1));
        store.create(&backoff).unwrap();
        let mut recent = task("recent", now);
        recent.last_run_at = Some(now - chrono::Duration::seconds(59));
        store.create(&recent).unwrap();
        let running = task("running", now);
        store.create(&running).unwrap();
        store.start_run(&running.id, None, now).unwrap();
        assert_eq!(
            store
                .due_for_evaluation(now)
                .unwrap()
                .into_iter()
                .map(|c| c.name)
                .collect::<Vec<_>>(),
            vec!["due"]
        );
    }

    #[test]
    fn duplicate_name_gets_clear_error() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TaskStore::open(&tmp.path().join("awman.db")).unwrap();
        store.migrate().unwrap();
        let now = Utc::now();
        store.create(&task("same", now)).unwrap();
        let error = store.create(&task("same", now)).unwrap_err();
        assert!(error
            .to_string()
            .contains("a task named \"same\" already exists"));
    }

    #[test]
    fn startup_reconciles_running_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TaskStore::open(&tmp.path().join("awman.db")).unwrap();
        store.migrate().unwrap();
        let now = Utc::now();
        let task = task("orphan", now);
        store.create(&task).unwrap();
        let run_id = store.start_run(&task.id, None, now).unwrap();
        assert_eq!(store.reconcile_orphaned_runs(now).unwrap(), 1);
        assert!(store.running_run_for(&task.id).unwrap().is_none());
        assert!(!store
            .finish_run(&run_id, RunStatus::Failed, &RunDetail::default(), now)
            .unwrap());
    }
}
