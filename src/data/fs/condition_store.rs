//! SQLite persistence for amie conditions and their evaluation runs.
//!
//! This store is opened only by the amie daemon process. CLI and TUI code use
//! the command gateway instead of importing or constructing it directly.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use crate::data::error::DataError;

/// A persisted scheduled amie condition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Condition {
    pub id: String,
    pub name: String,
    pub description: String,
    pub repo_scope: PathBuf,
    pub mount_scope: MountScope,
    pub interval_secs: u64,
    pub status: ConditionStatus,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub backoff_until: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_run_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConditionStatus {
    Active,
    Paused,
}

impl ConditionStatus {
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
                "invalid amie condition status {value:?}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MountScope {
    Cwd,
    GitRoot,
}

impl MountScope {
    fn as_db(self) -> &'static str {
        match self {
            Self::Cwd => "cwd",
            Self::GitRoot => "gitroot",
        }
    }

    fn from_db(value: &str) -> Result<Self, DataError> {
        match value {
            "cwd" => Ok(Self::Cwd),
            "gitroot" => Ok(Self::GitRoot),
            _ => Err(DataError::Other(format!(
                "invalid amie mount scope {value:?}"
            ))),
        }
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
                "invalid amie run status {value:?}"
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

/// A persisted amie evaluation attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub condition_id: String,
    pub status: RunStatus,
    pub workflow_path: Option<PathBuf>,
    pub workflow_state_path: Option<PathBuf>,
    pub session_id: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

/// SQLite-backed condition store. The amie daemon is its only constructor.
pub struct ConditionStore {
    conn: Mutex<Connection>,
}

impl ConditionStore {
    /// Open the shared database file and enable WAL. Opening performs no schema
    /// work: the daemon startup order is `open` → [`migrate`](Self::migrate) →
    /// `reconcile_orphaned_runs`, so the migration step is separately callable
    /// and separately testable. Call this only from the amie daemon.
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

    /// Apply amie's schema. Idempotent: every statement is
    /// `CREATE ... IF NOT EXISTS` or an add-column-if-missing, so re-running it
    /// against an already-migrated database is a no-op.
    pub fn migrate(&self) -> Result<(), DataError> {
        Self::migrate_conn(&self.lock())
    }

    fn migrate_conn(conn: &Connection) -> Result<(), DataError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS amie_conditions (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                description TEXT NOT NULL,
                repo_scope TEXT NOT NULL,
                mount_scope TEXT NOT NULL,
                interval_secs INTEGER NOT NULL,
                status TEXT NOT NULL DEFAULT 'active',
                agent TEXT,
                model TEXT,
                backoff_until TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                last_run_at TEXT
            );

            CREATE TABLE IF NOT EXISTS amie_runs (
                id TEXT PRIMARY KEY,
                condition_id TEXT NOT NULL REFERENCES amie_conditions(id),
                status TEXT NOT NULL,
                workflow_path TEXT,
                workflow_state_path TEXT,
                session_id TEXT,
                started_at TEXT NOT NULL,
                finished_at TEXT,
                error TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_amie_runs_condition
            ON amie_runs(condition_id, started_at DESC);",
        )?;
        Self::add_column_if_missing(conn, "amie_runs", "workflow_state_path", "TEXT")?;
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
        self.conn.lock().expect("condition store mutex poisoned")
    }

    pub fn create(&self, condition: &Condition) -> Result<(), DataError> {
        let interval_secs = i64::try_from(condition.interval_secs).map_err(|_| {
            DataError::Other(format!(
                "condition interval_secs {} exceeds SQLite's signed integer range",
                condition.interval_secs
            ))
        })?;
        let conn = self.lock();
        let result = conn.execute(
            "INSERT INTO amie_conditions
             (id, name, description, repo_scope, mount_scope, interval_secs, status, agent, model,
              backoff_until, created_at, updated_at, last_run_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                condition.id,
                condition.name,
                condition.description,
                condition.repo_scope.to_string_lossy(),
                condition.mount_scope.as_db(),
                interval_secs,
                condition.status.as_db(),
                condition.agent,
                condition.model,
                timestamp_opt(condition.backoff_until),
                timestamp(condition.created_at),
                timestamp(condition.updated_at),
                timestamp_opt(condition.last_run_at),
            ],
        );
        match result {
            Ok(_) => Ok(()),
            Err(error) if is_unique_name_violation(&error) => Err(DataError::Other(format!(
                "a condition named {:?} already exists",
                condition.name
            ))),
            Err(error) => Err(DataError::Sqlite(error)),
        }
    }

    pub fn get(&self, name: &str) -> Result<Option<Condition>, DataError> {
        let conn = self.lock();
        let raw = conn
            .query_row(
                "SELECT id, name, description, repo_scope, mount_scope, interval_secs, status, agent, model,
                        backoff_until, created_at, updated_at, last_run_at
                 FROM amie_conditions WHERE name = ?1",
                [name],
                condition_from_row,
            )
            .optional()?;
        raw.map(condition_from_raw).transpose()
    }

    pub fn list(&self) -> Result<Vec<Condition>, DataError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, description, repo_scope, mount_scope, interval_secs, status, agent, model,
                    backoff_until, created_at, updated_at, last_run_at
             FROM amie_conditions ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], condition_from_row)?;
        collect_conditions(rows)
    }

    pub fn set_status(&self, name: &str, status: ConditionStatus) -> Result<bool, DataError> {
        let conn = self.lock();
        Ok(conn.execute(
            "UPDATE amie_conditions SET status = ?1, updated_at = ?2 WHERE name = ?3",
            params![status.as_db(), timestamp(Utc::now()), name],
        )? > 0)
    }

    pub fn delete(&self, name: &str) -> Result<bool, DataError> {
        let conn = self.lock();
        Ok(conn.execute("DELETE FROM amie_conditions WHERE name = ?1", [name])? > 0)
    }

    /// Select due conditions wholly in SQL. Do not duplicate this predicate in
    /// Rust: it is the concurrency and backoff admission rule.
    pub fn due_for_evaluation(&self, now: DateTime<Utc>) -> Result<Vec<Condition>, DataError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT c.id, c.name, c.description, c.repo_scope, c.mount_scope, c.interval_secs, c.status,
                    c.agent, c.model, c.backoff_until, c.created_at, c.updated_at, c.last_run_at
             FROM amie_conditions c
             WHERE c.status = 'active'
               AND (c.backoff_until IS NULL OR c.backoff_until <= :now)
               AND (c.last_run_at IS NULL
                    OR (julianday(:now) - julianday(c.last_run_at)) * 86400.0 >= c.interval_secs)
               AND NOT EXISTS (SELECT 1 FROM amie_runs r
                               WHERE r.condition_id = c.id AND r.status = 'running')
             ORDER BY c.last_run_at IS NOT NULL, c.last_run_at ASC",
        )?;
        let now = timestamp(now);
        let rows = stmt.query_map(&[(":now", &now)], condition_from_row)?;
        collect_conditions(rows)
    }

    pub fn start_run(
        &self,
        condition_id: &str,
        session_id: Option<&str>,
        started_at: DateTime<Utc>,
    ) -> Result<RunId, DataError> {
        let id = RunId::new();
        let conn = self.lock();
        conn.execute(
            "INSERT INTO amie_runs (id, condition_id, status, session_id, started_at)
             VALUES (?1, ?2, 'running', ?3, ?4)",
            params![id.as_str(), condition_id, session_id, timestamp(started_at)],
        )?;
        conn.execute(
            "UPDATE amie_conditions SET last_run_at = ?1, updated_at = ?1 WHERE id = ?2",
            params![timestamp(started_at), condition_id],
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
            "UPDATE amie_runs
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
            "UPDATE amie_conditions SET backoff_until = ?1, updated_at = ?2 WHERE name = ?3",
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
            "UPDATE amie_runs SET workflow_path = ?1, workflow_state_path = ?2 WHERE id = ?3",
            params![
                path_opt(workflow_path),
                path_opt(workflow_state_path),
                run_id.as_str(),
            ],
        )?;
        Ok(())
    }

    /// The most recent runs for a condition, newest first.
    pub fn runs_for(&self, name: &str, limit: usize) -> Result<Vec<Run>, DataError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT r.id, r.condition_id, r.status, r.workflow_path, r.workflow_state_path,
                    r.session_id, r.started_at, r.finished_at, r.error
             FROM amie_runs r
             JOIN amie_conditions c ON c.id = r.condition_id
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

    pub fn running_run_for(&self, condition_id: &str) -> Result<Option<Run>, DataError> {
        let conn = self.lock();
        let raw = conn
            .query_row(
                "SELECT id, condition_id, status, workflow_path, workflow_state_path, session_id, started_at,
                        finished_at, error
                 FROM amie_runs WHERE condition_id = ?1 AND status = 'running'
                 ORDER BY started_at DESC LIMIT 1",
                [condition_id],
                run_from_row,
            )
            .optional()?;
        raw.map(run_from_raw).transpose()
    }

    pub fn reconcile_orphaned_runs(&self, now: DateTime<Utc>) -> Result<usize, DataError> {
        let conn = self.lock();
        Ok(conn.execute(
            "UPDATE amie_runs
             SET status = 'interrupted', finished_at = ?1,
                 error = 'daemon restarted while this run was in flight'
             WHERE status = 'running'",
            [timestamp(now)],
        )?)
    }
}

#[derive(Debug)]
struct RawCondition {
    id: String,
    name: String,
    description: String,
    repo_scope: String,
    mount_scope: String,
    interval_secs: i64,
    status: String,
    agent: Option<String>,
    model: Option<String>,
    backoff_until: Option<String>,
    created_at: String,
    updated_at: String,
    last_run_at: Option<String>,
}

fn condition_from_row(row: &Row<'_>) -> rusqlite::Result<RawCondition> {
    Ok(RawCondition {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        repo_scope: row.get(3)?,
        mount_scope: row.get(4)?,
        interval_secs: row.get(5)?,
        status: row.get(6)?,
        agent: row.get(7)?,
        model: row.get(8)?,
        backoff_until: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
        last_run_at: row.get(12)?,
    })
}

fn condition_from_raw(raw: RawCondition) -> Result<Condition, DataError> {
    Ok(Condition {
        id: raw.id,
        name: raw.name,
        description: raw.description,
        repo_scope: raw.repo_scope.into(),
        mount_scope: MountScope::from_db(&raw.mount_scope)?,
        interval_secs: u64::try_from(raw.interval_secs).map_err(|_| {
            DataError::Other(format!(
                "invalid negative amie interval_secs {}",
                raw.interval_secs
            ))
        })?,
        status: ConditionStatus::from_db(&raw.status)?,
        agent: raw.agent,
        model: raw.model,
        backoff_until: timestamp_parse_opt(raw.backoff_until)?,
        created_at: timestamp_parse(&raw.created_at)?,
        updated_at: timestamp_parse(&raw.updated_at)?,
        last_run_at: timestamp_parse_opt(raw.last_run_at)?,
    })
}

fn collect_conditions<F>(rows: rusqlite::MappedRows<'_, F>) -> Result<Vec<Condition>, DataError>
where
    F: FnMut(&Row<'_>) -> rusqlite::Result<RawCondition>,
{
    rows.map(|row| row.map_err(DataError::from).and_then(condition_from_raw))
        .collect()
}

#[derive(Debug)]
struct RawRun {
    id: String,
    condition_id: String,
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
        condition_id: row.get(1)?,
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
        condition_id: raw.condition_id,
        status: RunStatus::from_db(&raw.status)?,
        workflow_path: raw.workflow_path.map(PathBuf::from),
        workflow_state_path: raw.workflow_state_path.map(PathBuf::from),
        session_id: raw.session_id,
        started_at: timestamp_parse(&raw.started_at)?,
        finished_at: timestamp_parse_opt(raw.finished_at)?,
        error: raw.error,
    })
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
        .map_err(|error| DataError::Other(format!("invalid amie timestamp {value:?}: {error}")))
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

    fn condition(name: &str, now: DateTime<Utc>) -> Condition {
        Condition {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            description: "test".into(),
            repo_scope: PathBuf::from("/repo"),
            mount_scope: MountScope::GitRoot,
            interval_secs: 60,
            status: ConditionStatus::Active,
            agent: None,
            model: None,
            backoff_until: None,
            created_at: now,
            updated_at: now,
            last_run_at: None,
        }
    }

    #[test]
    fn due_query_enforces_every_admission_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ConditionStore::open(&tmp.path().join("awman.db")).unwrap();
        store.migrate().unwrap();
        let now = DateTime::parse_from_rfc3339("2026-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let due = condition("due", now);
        store.create(&due).unwrap();
        let mut paused = condition("paused", now);
        paused.status = ConditionStatus::Paused;
        store.create(&paused).unwrap();
        let mut backoff = condition("backoff", now);
        backoff.backoff_until = Some(now + chrono::Duration::minutes(1));
        store.create(&backoff).unwrap();
        let mut recent = condition("recent", now);
        recent.last_run_at = Some(now - chrono::Duration::seconds(59));
        store.create(&recent).unwrap();
        let running = condition("running", now);
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
        let store = ConditionStore::open(&tmp.path().join("awman.db")).unwrap();
        store.migrate().unwrap();
        let now = Utc::now();
        store.create(&condition("same", now)).unwrap();
        let error = store.create(&condition("same", now)).unwrap_err();
        assert!(error
            .to_string()
            .contains("a condition named \"same\" already exists"));
    }

    #[test]
    fn startup_reconciles_running_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ConditionStore::open(&tmp.path().join("awman.db")).unwrap();
        store.migrate().unwrap();
        let now = Utc::now();
        let condition = condition("orphan", now);
        store.create(&condition).unwrap();
        let run_id = store.start_run(&condition.id, None, now).unwrap();
        assert_eq!(store.reconcile_orphaned_runs(now).unwrap(), 1);
        assert!(store.running_run_for(&condition.id).unwrap().is_none());
        assert!(!store
            .finish_run(&run_id, RunStatus::Failed, &RunDetail::default(), now)
            .unwrap());
    }
}
