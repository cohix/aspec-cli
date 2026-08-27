//! The condition gateway keeps amie commands identical for local daemon and
//! remote CLI/TUI callers.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::command::commands::amie::runtime_guard::require_container_tier;
use crate::command::commands::http_core::HttpCore;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::config::global::GlobalConfig;
use crate::data::fs::condition_store::{
    Condition, ConditionStatus, ConditionStore, MountScope, Run,
};
use crate::data::repo_dockerfile_paths::RepoDockerfilePaths;
use crate::engine::amie::SchedulerStatus;
use crate::engine::container::naming::validate_condition_slug;

const MIN_INTERVAL_SECS: u64 = 60;
const MAX_INTERVAL_SECS: u64 = 86_400;

/// How many runs `amie show` carries back. One number, used by the local
/// gateway when it builds the response and by the remote gateway when it
/// truncates one, so both sides agree without a second flag on the wire.
pub const DEFAULT_RUN_HISTORY_LIMIT: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateCondition {
    pub name: String,
    pub description: String,
    pub repo_scope: PathBuf,
    pub mount_scope: MountScope,
    pub interval_secs: u64,
    pub agent: Option<String>,
    pub model: Option<String>,
}

/// The `amie show` response: one condition plus its recent run history.
///
/// Both gateways speak this one shape, so `get` and `runs` cannot drift apart
/// between the daemon-local and HTTP paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConditionDetail {
    pub condition: Condition,
    pub runs: Vec<Run>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub bound_addr: Option<String>,
    pub condition_count: usize,
    pub active_count: usize,
    pub last_tick: Option<DateTime<Utc>>,
    pub in_flight: usize,
}

#[async_trait]
pub trait ConditionGateway: Send + Sync {
    async fn create(&self, req: CreateCondition) -> Result<Condition, CommandError>;
    async fn list(&self) -> Result<Vec<Condition>, CommandError>;
    async fn get(&self, name: &str) -> Result<Condition, CommandError>;
    async fn runs(&self, name: &str, limit: usize) -> Result<Vec<Run>, CommandError>;
    async fn set_status(&self, name: &str, status: ConditionStatus) -> Result<(), CommandError>;
    async fn delete(&self, name: &str) -> Result<(), CommandError>;
    async fn status(&self) -> Result<DaemonStatus, CommandError>;
}

/// Boxable adaptor for Dispatch's shared daemon gateway handle.
pub struct SharedConditionGateway(pub Arc<dyn ConditionGateway>);

#[async_trait]
impl ConditionGateway for SharedConditionGateway {
    async fn create(&self, req: CreateCondition) -> Result<Condition, CommandError> {
        self.0.create(req).await
    }
    async fn list(&self) -> Result<Vec<Condition>, CommandError> {
        self.0.list().await
    }
    async fn get(&self, name: &str) -> Result<Condition, CommandError> {
        self.0.get(name).await
    }
    async fn runs(&self, name: &str, limit: usize) -> Result<Vec<Run>, CommandError> {
        self.0.runs(name, limit).await
    }
    async fn set_status(&self, name: &str, status: ConditionStatus) -> Result<(), CommandError> {
        self.0.set_status(name, status).await
    }
    async fn delete(&self, name: &str) -> Result<(), CommandError> {
        self.0.delete(name).await
    }
    async fn status(&self) -> Result<DaemonStatus, CommandError> {
        self.0.status().await
    }
}

/// Daemon-only gateway. All persistent-condition validation belongs here.
pub struct LocalConditionGateway {
    store: Arc<ConditionStore>,
    engines: Engines,
    status: Arc<Mutex<SchedulerStatus>>,
}

impl LocalConditionGateway {
    pub fn new(
        store: Arc<ConditionStore>,
        engines: Engines,
        status: Arc<Mutex<SchedulerStatus>>,
    ) -> Self {
        Self {
            store,
            engines,
            status,
        }
    }

    fn validate_create(&self, req: &CreateCondition) -> Result<(), CommandError> {
        validate_condition_slug(&req.name)?;
        if !(MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS).contains(&req.interval_secs) {
            return Err(CommandError::Other(format!(
                "condition interval must be between {MIN_INTERVAL_SECS} and {MAX_INTERVAL_SECS} seconds"
            )));
        }
        let canonical = std::fs::canonicalize(&req.repo_scope).map_err(|error| {
            CommandError::Other(format!(
                "condition repository scope {} does not exist or cannot be resolved: {error}",
                req.repo_scope.display()
            ))
        })?;
        if !canonical.is_dir() {
            return Err(CommandError::Other(format!(
                "condition repository scope {} is not a directory",
                canonical.display()
            )));
        }
        let git_root = find_git_root(&canonical).ok_or_else(|| {
            CommandError::Other(format!(
                "condition repository scope {} is not inside a git repository",
                canonical.display()
            ))
        })?;
        let mut agents = BTreeSet::new();
        if let Some(agent) = &req.agent {
            agents.insert(agent.clone());
        }
        if let Some(pool) = GlobalConfig::load()?
            .amie
            .and_then(|cfg| cfg.agents_to_models)
        {
            agents.extend(pool.into_keys());
        }
        validate_agent_dockerfiles(&git_root, &agents)
    }
}

#[async_trait]
impl ConditionGateway for LocalConditionGateway {
    async fn create(&self, mut req: CreateCondition) -> Result<Condition, CommandError> {
        // Must remain first: changing a runtime must never mutate existing
        // condition state before the sandbox refusal is reported.
        require_container_tier(&self.engines)?;
        self.validate_create(&req)?;
        req.repo_scope = std::fs::canonicalize(&req.repo_scope)
            .map_err(|error| CommandError::Other(error.to_string()))?;
        let now = Utc::now();
        let condition = Condition {
            id: uuid::Uuid::new_v4().to_string(),
            name: req.name,
            description: req.description,
            repo_scope: req.repo_scope,
            mount_scope: req.mount_scope,
            interval_secs: req.interval_secs,
            status: ConditionStatus::Active,
            agent: req.agent,
            model: req.model,
            backoff_until: None,
            created_at: now,
            updated_at: now,
            last_run_at: None,
        };
        self.store.create(&condition)?;
        Ok(condition)
    }

    async fn list(&self) -> Result<Vec<Condition>, CommandError> {
        Ok(self.store.list()?)
    }

    async fn get(&self, name: &str) -> Result<Condition, CommandError> {
        self.store
            .get(name)?
            .ok_or_else(|| CommandError::Other(format!("condition {name:?} was not found")))
    }

    async fn runs(&self, name: &str, limit: usize) -> Result<Vec<Run>, CommandError> {
        // Existence is reported through the same not-found error shape the
        // other single-condition methods use, so an unknown name never looks
        // like a condition with no history.
        if self.store.get(name)?.is_none() {
            return Err(CommandError::Other(format!(
                "condition {name:?} was not found"
            )));
        }
        Ok(self.store.runs_for(name, limit)?)
    }

    async fn set_status(&self, name: &str, status: ConditionStatus) -> Result<(), CommandError> {
        if self.store.set_status(name, status)? {
            Ok(())
        } else {
            Err(CommandError::Other(format!(
                "condition {name:?} was not found"
            )))
        }
    }

    async fn delete(&self, name: &str) -> Result<(), CommandError> {
        if self.store.delete(name)? {
            Ok(())
        } else {
            Err(CommandError::Other(format!(
                "condition {name:?} was not found"
            )))
        }
    }

    async fn status(&self) -> Result<DaemonStatus, CommandError> {
        let conditions = self.store.list()?;
        let status = self.status.lock().expect("scheduler status mutex poisoned");
        Ok(DaemonStatus {
            running: true,
            pid: Some(std::process::id()),
            bound_addr: None,
            condition_count: conditions.len(),
            active_count: conditions
                .iter()
                .filter(|c| c.status == ConditionStatus::Active)
                .count(),
            last_tick: status.last_tick,
            in_flight: status.in_flight,
        })
    }
}

/// HTTP-only gateway. It intentionally makes no validation decisions.
pub struct RemoteConditionGateway {
    core: HttpCore,
}

impl RemoteConditionGateway {
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
            CommandError::RemoteTransport(format!("invalid amie daemon response: {error}"))
        })
    }
}

#[async_trait]
impl ConditionGateway for RemoteConditionGateway {
    async fn create(&self, req: CreateCondition) -> Result<Condition, CommandError> {
        let mut args = vec![
            "--name".into(),
            req.name,
            "--description".into(),
            req.description,
            "--repo".into(),
            req.repo_scope.display().to_string(),
            "--interval".into(),
            req.interval_secs.to_string(),
            "--mount-scope".into(),
            match req.mount_scope {
                MountScope::Cwd => "cwd",
                MountScope::GitRoot => "gitroot",
            }
            .into(),
        ];
        if let Some(agent) = req.agent {
            args.extend(["--agent".into(), agent]);
        }
        if let Some(model) = req.model {
            args.extend(["--model".into(), model]);
        }
        self.command("amie add", args).await
    }
    async fn list(&self) -> Result<Vec<Condition>, CommandError> {
        self.command("amie list", vec![]).await
    }
    async fn get(&self, name: &str) -> Result<Condition, CommandError> {
        let detail: ConditionDetail = self.command("amie show", vec![name.into()]).await?;
        Ok(detail.condition)
    }
    async fn runs(&self, name: &str, limit: usize) -> Result<Vec<Run>, CommandError> {
        let detail: ConditionDetail = self.command("amie show", vec![name.into()]).await?;
        let mut runs = detail.runs;
        runs.truncate(limit);
        Ok(runs)
    }
    async fn set_status(&self, name: &str, status: ConditionStatus) -> Result<(), CommandError> {
        let subcommand = match status {
            ConditionStatus::Active => "amie resume",
            ConditionStatus::Paused => "amie pause",
        };
        self.command::<serde_json::Value>(subcommand, vec![name.into()])
            .await
            .map(|_| ())
    }
    async fn delete(&self, name: &str) -> Result<(), CommandError> {
        self.command::<serde_json::Value>("amie remove", vec![name.into()])
            .await
            .map(|_| ())
    }
    async fn status(&self) -> Result<DaemonStatus, CommandError> {
        let response = self.core.get(&["status"]).await?;
        serde_json::from_value(response.body).map_err(|error| {
            CommandError::RemoteTransport(format!("invalid amie daemon status: {error}"))
        })
    }
}

fn find_git_root(path: &std::path::Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .map(PathBuf::from)
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
        String::from("amie condition references agents with no Dockerfile in the project:\n");
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
