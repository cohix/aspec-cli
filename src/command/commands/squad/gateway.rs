//! The task gateway keeps squad commands identical for local daemon and
//! remote CLI/TUI callers.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::command::commands::http_core::HttpCore;
use crate::command::commands::squad::commands::resolved_git_root;
use crate::command::commands::squad::runtime_guard::require_container_tier;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::config::global::GlobalConfig;
use crate::data::fs::task_store::{MountScope, Run, Task, TaskStatus, TaskStore, TaskWorkspace};
use crate::data::fs::SquadPaths;
use crate::data::repo_dockerfile_paths::RepoDockerfilePaths;
use crate::engine::container::naming::validate_task_slug;
use crate::engine::squad::SchedulerStatus;

/// The `--workspace` value selecting the durable per-task workspace. One
/// literal, shared by the catalogue default, Dispatch, and the remote gateway.
pub const DEFAULT_WORKSPACE_FLAG_VALUE: &str = "default";

const MIN_INTERVAL_SECS: u64 = 60;
const MAX_INTERVAL_SECS: u64 = 86_400;

/// How many runs `squad show` carries back. One number, used by the local
/// gateway when it builds the response and by the remote gateway when it
/// truncates one, so both sides agree without a second flag on the wire.
pub const DEFAULT_RUN_HISTORY_LIMIT: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTask {
    pub name: String,
    pub description: String,
    /// Which workspace the task is bound to. The gateway resolves this, once,
    /// into the persisted `repo_scope` + `mount_scope` pair — see
    /// [`Task`]'s type-level documentation for the resulting semantics.
    pub workspace: TaskWorkspace,
    /// The requested scope *within* a custom git repository. Ignored (and
    /// overridden with [`MountScope::Directory`]) whenever the resolved
    /// effective root is not a git repository, because there is then no git
    /// root for `cwd` and `gitroot` to differ about.
    pub mount_scope: MountScope,
    pub interval_secs: u64,
    pub agent: Option<String>,
    pub model: Option<String>,
    /// Raw overlay specs in `--overlay` syntax. Validated for *syntax* here at
    /// creation time; host-side existence is deliberately not checked, since
    /// the host's state at creation differs from its state at each future run
    /// (`docs/08-overlays.md`).
    #[serde(default)]
    pub overlays: Vec<String>,
}

/// The `squad show` response: one task plus its recent run history.
///
/// Both gateways speak this one shape, so `get` and `runs` cannot drift apart
/// between the daemon-local and HTTP paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDetail {
    pub task: Task,
    pub runs: Vec<Run>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub bound_addr: Option<String>,
    pub task_count: usize,
    pub active_count: usize,
    pub last_tick: Option<DateTime<Utc>>,
    pub in_flight: usize,
}

#[async_trait]
pub trait TaskGateway: Send + Sync {
    async fn create(&self, req: CreateTask) -> Result<Task, CommandError>;
    async fn list(&self) -> Result<Vec<Task>, CommandError>;
    async fn get(&self, name: &str) -> Result<Task, CommandError>;
    async fn runs(&self, name: &str, limit: usize) -> Result<Vec<Run>, CommandError>;
    async fn set_status(&self, name: &str, status: TaskStatus) -> Result<(), CommandError>;
    async fn delete(&self, name: &str) -> Result<(), CommandError>;
    async fn status(&self) -> Result<DaemonStatus, CommandError>;
}

/// Boxable adaptor for Dispatch's shared daemon gateway handle.
pub struct SharedTaskGateway(pub Arc<dyn TaskGateway>);

#[async_trait]
impl TaskGateway for SharedTaskGateway {
    async fn create(&self, req: CreateTask) -> Result<Task, CommandError> {
        self.0.create(req).await
    }
    async fn list(&self) -> Result<Vec<Task>, CommandError> {
        self.0.list().await
    }
    async fn get(&self, name: &str) -> Result<Task, CommandError> {
        self.0.get(name).await
    }
    async fn runs(&self, name: &str, limit: usize) -> Result<Vec<Run>, CommandError> {
        self.0.runs(name, limit).await
    }
    async fn set_status(&self, name: &str, status: TaskStatus) -> Result<(), CommandError> {
        self.0.set_status(name, status).await
    }
    async fn delete(&self, name: &str) -> Result<(), CommandError> {
        self.0.delete(name).await
    }
    async fn status(&self) -> Result<DaemonStatus, CommandError> {
        self.0.status().await
    }
}

/// Daemon-only gateway. All persistent-task validation belongs here.
pub struct LocalTaskGateway {
    store: Arc<TaskStore>,
    engines: Engines,
    status: Arc<Mutex<SchedulerStatus>>,
    /// The daemon's own squad root, so the durable workspace this gateway
    /// creates at task creation is the same directory the scheduler later
    /// seeds and mounts. Re-deriving it from the process environment here
    /// would let the two disagree whenever the daemon was started with an
    /// explicit root.
    paths: SquadPaths,
}

impl LocalTaskGateway {
    pub fn new(
        store: Arc<TaskStore>,
        engines: Engines,
        status: Arc<Mutex<SchedulerStatus>>,
        paths: SquadPaths,
    ) -> Self {
        Self {
            store,
            engines,
            status,
            paths,
        }
    }

    /// Validate everything that must hold before a task is persisted, and
    /// resolve the requested workspace into the effective root + scope pair
    /// the task will carry for its whole lifetime.
    ///
    /// Nothing here writes to the store *or the filesystem*, so a rejection at
    /// any step leaves neither a partial task nor a stray directory behind.
    /// The durable workspace is only created once every check has passed —
    /// see [`Self::ensure_durable_workspace`], called from `create`.
    fn validate_create(&self, req: &CreateTask) -> Result<ResolvedWorkspace, CommandError> {
        validate_task_slug(&req.name)?;
        if !(MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS).contains(&req.interval_secs) {
            return Err(CommandError::Other(format!(
                "task interval must be between {MIN_INTERVAL_SECS} and {MAX_INTERVAL_SECS} seconds"
            )));
        }
        // Syntax-only overlay validation, before any store write: a malformed
        // spec is rejected at creation rather than discovered at first run.
        for spec in &req.overlays {
            crate::command::commands::parse_overlay_list(spec).map_err(|reason| {
                CommandError::InvalidOverlaySpec {
                    spec: spec.clone(),
                    reason,
                }
            })?;
        }

        let resolved = self.resolve_workspace(req)?;

        let mut agents = BTreeSet::new();
        if let Some(agent) = &req.agent {
            agents.insert(agent.clone());
        }
        if let Some(pool) = GlobalConfig::load()?
            .squad
            .and_then(|cfg| cfg.agents_to_models)
        {
            agents.extend(pool.into_keys());
        }
        // Agent Dockerfiles are discovered from the repository a task is bound
        // to. A directory-workspace task has no repository, so there is nothing
        // to validate against — a missing agent image surfaces at launch, the
        // same way it does for any other non-repo-rooted run.
        if let Some(git_root) = &resolved.git_root {
            validate_agent_dockerfiles(git_root, &agents)?;
        }
        Ok(resolved)
    }

    /// Resolve `workspace` + `mount_scope` into the task's effective root and
    /// the scope actually stored, applying the WI-0106 rules:
    ///
    /// * **Default Task Workspace** → the durable
    ///   `~/.awman/squad/tasks/<name>/workspace/` directory, created here, with
    ///   [`MountScope::Directory`] (a plain directory: mounted whole, never
    ///   worktree-isolated).
    /// * **Custom Folder / Repo that does not exist** → a hard error. Unlike
    ///   the default workspace, an arbitrary user-entered path is never
    ///   silently created.
    /// * **Custom Folder / Repo that IS a git root** → the requested
    ///   `cwd`/`gitroot` scope is kept (both name the root itself), and every
    ///   run is worktree-isolated.
    /// * **Custom Folder / Repo that is not a git root** — a plain directory,
    ///   or a subdirectory of a repository — → kept as the effective root with
    ///   [`MountScope::Directory`]; the user chose that folder precisely
    ///   because they want it used and preserved, so it is treated as no less
    ///   durable than the default workspace and is never widened to the
    ///   enclosing repository by a worktree.
    fn resolve_workspace(&self, req: &CreateTask) -> Result<ResolvedWorkspace, CommandError> {
        // Resolved, not created: nothing touches the filesystem until every
        // validation has passed, so a rejected creation leaves nothing behind.
        let durable = self.paths.task_dir(&req.name)?;
        let requested = match &req.workspace {
            TaskWorkspace::Default => {
                return Ok(ResolvedWorkspace {
                    repo_scope: durable.clone(),
                    mount_scope: MountScope::Directory,
                    git_root: None,
                    durable_workspace: durable,
                });
            }
            TaskWorkspace::Custom(path) => path.clone(),
        };

        // A path that does not exist is a hard error: there is nothing to
        // mount, and creating it would silently invent a workspace the user
        // did not ask for.
        let canonical = std::fs::canonicalize(&requested).map_err(|error| {
            CommandError::Other(format!(
                "task workspace {} does not exist or cannot be resolved: {error}",
                requested.display()
            ))
        })?;
        if !canonical.is_dir() {
            return Err(CommandError::Other(format!(
                "task workspace {} is not a directory",
                canonical.display()
            )));
        }
        // `GitEngine::resolve_root` — the same detector the interview's
        // warning and every run's `open_session` use — decides whether this
        // path is a repository root. A second, weaker `.git`-exists probe
        // could classify a malformed marker as a repository here and then
        // fail at launch, so there is exactly one answer.
        //
        // The comparison is deliberately against the root *itself*, not merely
        // "is inside a repository". A subdirectory of a repository is exactly
        // the "not a git root" case the interview warns about and the user
        // chose to keep, so it is bound and mounted as the plain directory it
        // is. Treating it as repository-backed would force worktree isolation,
        // and a worktree is a checkout of the whole repository — which would
        // silently widen the run's view from the folder the user picked to its
        // entire enclosing repository, exactly the widening `mount_scope`'s
        // capture-once semantics exist to prevent.
        match resolved_git_root(&self.engines.git_engine, &canonical) {
            Some(git_root) if git_root == canonical => Ok(ResolvedWorkspace {
                repo_scope: canonical,
                // The chosen path *is* the root here, so `cwd` and `gitroot`
                // name the same directory: the captured answer is stored
                // verbatim and neither can widen anything.
                mount_scope: req.mount_scope,
                git_root: Some(git_root),
                durable_workspace: durable,
            }),
            // Not a repository root: no worktree, direct mount, and the same
            // durability expectation as the default workspace.
            _ => Ok(ResolvedWorkspace {
                repo_scope: canonical,
                mount_scope: MountScope::Directory,
                git_root: None,
                durable_workspace: durable,
            }),
        }
    }

    /// Create (idempotently) the durable per-task workspace.
    ///
    /// Every task gets one, whichever workspace it is bound to: a
    /// custom-directory task's runs still mount this directory as a context
    /// overlay so task-scoped persistent data survives across runs regardless
    /// of workspace mode. It is created once and never deleted, emptied, or
    /// replaced until the task itself is removed.
    ///
    /// The path came from `SquadPaths::task_dir`, which validates the
    /// user-influenced name against the tasks root before appending the fixed
    /// `workspace` leaf, so a crafted name cannot escape.
    fn ensure_durable_workspace(dir: &Path) -> Result<(), CommandError> {
        std::fs::create_dir_all(dir)
            .map_err(|error| CommandError::Data(crate::data::error::DataError::io(dir, error)))
    }
}

/// The effective root a task will be bound to, as resolved once at creation.
#[derive(Debug, Clone)]
pub struct ResolvedWorkspace {
    /// The task's effective root — what gets persisted as `Task::repo_scope`.
    pub repo_scope: PathBuf,
    /// The scope actually persisted, which also decides worktree usage.
    pub mount_scope: MountScope,
    /// The enclosing git root, when the effective root is inside one. `None`
    /// means the effective root is a plain directory.
    pub git_root: Option<PathBuf>,
    /// The durable `tasks/<name>/workspace/` directory, always created.
    pub durable_workspace: PathBuf,
}

#[async_trait]
impl TaskGateway for LocalTaskGateway {
    async fn create(&self, req: CreateTask) -> Result<Task, CommandError> {
        tracing::info!(task = %req.name, "squad administrator requested task creation");
        // Must remain first: changing a runtime must never mutate existing
        // task state before the sandbox refusal is reported.
        require_container_tier(&self.engines)?;
        let resolved = self.validate_create(&req)?;
        // Only now, with every check passed, does anything reach the disk.
        Self::ensure_durable_workspace(&resolved.durable_workspace)?;
        let now = Utc::now();
        let task = Task {
            id: uuid::Uuid::new_v4().to_string(),
            name: req.name,
            description: req.description,
            repo_scope: resolved.repo_scope,
            mount_scope: resolved.mount_scope,
            overlays: req.overlays,
            interval_secs: req.interval_secs,
            status: TaskStatus::Active,
            agent: req.agent,
            model: req.model,
            backoff_until: None,
            created_at: now,
            updated_at: now,
            last_run_at: None,
            last_run_status: None,
        };
        self.store.create(&task)?;
        tracing::info!(
            task = %task.name,
            repo_scope = %task.repo_scope.display(),
            mount_scope = ?task.mount_scope,
            worktree = task.uses_worktree(),
            durable_workspace = %resolved.durable_workspace.display(),
            overlays = task.overlays.len(),
            "squad task created"
        );
        Ok(task)
    }

    async fn list(&self) -> Result<Vec<Task>, CommandError> {
        Ok(self.store.list()?)
    }

    async fn get(&self, name: &str) -> Result<Task, CommandError> {
        self.store
            .get(name)?
            .ok_or_else(|| CommandError::Other(format!("task {name:?} was not found")))
    }

    async fn runs(&self, name: &str, limit: usize) -> Result<Vec<Run>, CommandError> {
        // Existence is reported through the same not-found error shape the
        // other single-task methods use, so an unknown name never looks
        // like a task with no history.
        if self.store.get(name)?.is_none() {
            return Err(CommandError::Other(format!("task {name:?} was not found")));
        }
        Ok(self.store.runs_for(name, limit)?)
    }

    async fn set_status(&self, name: &str, status: TaskStatus) -> Result<(), CommandError> {
        if self.store.set_status(name, status)? {
            tracing::info!(
                task = %name,
                status = ?status,
                "squad administrator changed task status"
            );
            Ok(())
        } else {
            Err(CommandError::Other(format!("task {name:?} was not found")))
        }
    }

    async fn delete(&self, name: &str) -> Result<(), CommandError> {
        if self.store.delete(name)? {
            tracing::info!(task = %name, "squad administrator removed task");
            Ok(())
        } else {
            Err(CommandError::Other(format!("task {name:?} was not found")))
        }
    }

    async fn status(&self) -> Result<DaemonStatus, CommandError> {
        let tasks = self.store.list()?;
        let status = self.status.lock().expect("scheduler status mutex poisoned");
        Ok(DaemonStatus {
            running: true,
            pid: Some(std::process::id()),
            bound_addr: None,
            task_count: tasks.len(),
            active_count: tasks
                .iter()
                .filter(|c| c.status == TaskStatus::Active)
                .count(),
            last_tick: status.last_tick,
            in_flight: status.in_flight,
        })
    }
}

/// HTTP-only gateway. It intentionally makes no validation decisions.
pub struct RemoteTaskGateway {
    core: HttpCore,
}

impl RemoteTaskGateway {
    pub fn new(core: HttpCore) -> Self {
        Self { core }
    }
    pub fn core(&self) -> &HttpCore {
        &self.core
    }

    async fn command<T: serde::de::DeserializeOwned>(
        &self,
        subcommand: &str,
        args: Vec<String>,
    ) -> Result<T, CommandError> {
        let response = self
            .core
            .post_command(
                &["commands"],
                &[("subcommand", json!(subcommand)), ("args", json!(args))],
                &[],
            )
            .await?;
        serde_json::from_value(response.body).map_err(|error| {
            CommandError::RemoteTransport(format!("invalid squad daemon response: {error}"))
        })
    }
}

#[async_trait]
impl TaskGateway for RemoteTaskGateway {
    async fn create(&self, req: CreateTask) -> Result<Task, CommandError> {
        let mut args = vec![
            "--name".into(),
            req.name,
            "--description".into(),
            req.description,
            "--workspace".into(),
            match &req.workspace {
                TaskWorkspace::Default => DEFAULT_WORKSPACE_FLAG_VALUE.to_string(),
                TaskWorkspace::Custom(path) => path.display().to_string(),
            },
            "--interval".into(),
            req.interval_secs.to_string(),
            "--mount-scope".into(),
            match req.mount_scope {
                MountScope::Cwd => "cwd",
                // A directory workspace has no cwd/gitroot distinction; the
                // daemon-side resolution decides it either way, so send the
                // safe default rather than a value the catalogue enum rejects.
                MountScope::GitRoot | MountScope::Directory => "gitroot",
            }
            .into(),
        ];
        if let Some(agent) = req.agent {
            args.extend(["--agent".into(), agent]);
        }
        if let Some(model) = req.model {
            args.extend(["--model".into(), model]);
        }
        for overlay in req.overlays {
            args.extend(["--overlay".into(), overlay]);
        }
        self.command("squad add", args).await
    }
    async fn list(&self) -> Result<Vec<Task>, CommandError> {
        self.command("squad list", vec![]).await
    }
    async fn get(&self, name: &str) -> Result<Task, CommandError> {
        let detail: TaskDetail = self.command("squad show", vec![name.into()]).await?;
        Ok(detail.task)
    }
    async fn runs(&self, name: &str, limit: usize) -> Result<Vec<Run>, CommandError> {
        let detail: TaskDetail = self.command("squad show", vec![name.into()]).await?;
        let mut runs = detail.runs;
        runs.truncate(limit);
        Ok(runs)
    }
    async fn set_status(&self, name: &str, status: TaskStatus) -> Result<(), CommandError> {
        let subcommand = match status {
            TaskStatus::Active => "squad resume",
            TaskStatus::Paused => "squad pause",
        };
        self.command::<serde_json::Value>(subcommand, vec![name.into()])
            .await
            .map(|_| ())
    }
    async fn delete(&self, name: &str) -> Result<(), CommandError> {
        self.command::<serde_json::Value>("squad remove", vec![name.into()])
            .await
            .map(|_| ())
    }
    async fn status(&self) -> Result<DaemonStatus, CommandError> {
        let response = self.core.get(&["status"]).await?;
        serde_json::from_value(response.body).map_err(|error| {
            CommandError::RemoteTransport(format!("invalid squad daemon status: {error}"))
        })
    }
}

fn validate_agent_dockerfiles(
    git_root: &std::path::Path,
    agents: &BTreeSet<String>,
) -> Result<(), CommandError> {
    let paths = RepoDockerfilePaths::new(git_root);
    let unknown: Vec<_> = agents
        .iter()
        .filter(|agent| !paths.agent_dockerfile(agent).exists())
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let available = paths
        .discover_agent_dockerfiles()
        .into_iter()
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    let mut message =
        String::from("squad task references agents with no Dockerfile in the project:\n");
    for agent in unknown {
        message.push_str(&format!(
            "  - \"{agent}\" (expected .awman/Dockerfile.{agent})\n"
        ));
    }
    message.push_str(&format!(
        "Available agents: {}",
        if available.is_empty() {
            "(none)".to_string()
        } else {
            available.join(", ")
        }
    ));
    Err(CommandError::Other(message))
}
