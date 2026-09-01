//! WI 0102 remediation — the five Layer-2 blocker fixes (§9) plus the
//! CLI/bare sandbox-refusal entry-point gap (edge-case #1).
//!
//! These drive the real Layer-2 command path (`SquadCommand`) and the real CLI
//! entry point (`frontend::cli::run`) with fake frontends/gateways and an
//! isolated environment. Runtime discovery, container naming, and the
//! `SquadContainerIdentity::stamp` unit tests live next to their code
//! (`src/engine/squad/launcher.rs`, `src/command/commands/squad/daemon.rs`).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex as AsyncMutex;

use awman::command::commands::squad::commands::{
    SquadAddRequest, SquadCommand, SquadCommandFrontend, SquadOutcome, SquadSubcommand,
    TaskWorkspaceChoice,
};
use awman::command::commands::squad::gateway::{CreateTask, DaemonStatus, TaskGateway};
use awman::command::commands::Command;
use awman::command::dispatch::Engines;
use awman::command::error::CommandError;
use awman::data::config::env::AWMAN_SQUAD_ROOT;
use awman::data::fs::task_store::{MountScope, Run, Task, TaskStatus};
use awman::data::fs::{ApiPaths, AuthPathResolver, SquadPaths};
use awman::data::message::{UserMessage, UserMessageSink};
use awman::data::session::{AgentHandle, Session};
use awman::data::EngineWorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::agent_runtime::execution::AgentInstance;
use awman::engine::agent_runtime::{
    AgentRuntimeEngine, AgentStats, Capabilities, DindSupport, ResolvedAgentOptions,
};
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;

#[path = "helpers/mod.rs"]
mod helpers;

/// Process-env mutation (AWMAN_SQUAD_ROOT) is global; serialize the tests that
/// touch it so parallel runs never see each other's root.
static ENV_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

// ─── Fakes ──────────────────────────────────────────────────────────────────

fn sample_task(name: &str) -> Task {
    let now = chrono::Utc::now();
    Task {
        id: format!("id-{name}"),
        name: name.to_string(),
        description: "desc".into(),
        repo_scope: PathBuf::from("/repo"),
        mount_scope: MountScope::GitRoot,
        overlays: Vec::new(),
        interval_secs: 300,
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

/// Records every gateway call and the exact `CreateTask` it received.
#[derive(Default)]
struct RecordingGateway {
    created: Mutex<Vec<CreateTask>>,
    deleted: Mutex<Vec<String>>,
}

#[async_trait]
impl TaskGateway for RecordingGateway {
    async fn create(&self, req: CreateTask) -> Result<Task, CommandError> {
        let task = sample_task(&req.name);
        self.created.lock().unwrap().push(req);
        Ok(task)
    }
    async fn list(&self) -> Result<Vec<Task>, CommandError> {
        Ok(vec![])
    }
    async fn get(&self, name: &str) -> Result<Task, CommandError> {
        Ok(sample_task(name))
    }
    async fn runs(&self, _name: &str, _limit: usize) -> Result<Vec<Run>, CommandError> {
        Ok(vec![])
    }
    async fn set_status(&self, _name: &str, _status: TaskStatus) -> Result<(), CommandError> {
        Ok(())
    }
    async fn delete(&self, name: &str) -> Result<(), CommandError> {
        self.deleted.lock().unwrap().push(name.to_string());
        Ok(())
    }
    async fn status(&self) -> Result<DaemonStatus, CommandError> {
        Ok(DaemonStatus {
            running: true,
            pid: Some(1),
            bound_addr: None,
            task_count: 0,
            active_count: 0,
            last_tick: None,
            in_flight: 0,
        })
    }
}

/// A frontend that answers the interview and the delete prompt from canned
/// values, and captures every message written to it.
struct ScriptedFrontend {
    name: String,
    description: String,
    interval: String,
    workspace_choice: TaskWorkspaceChoice,
    repo: PathBuf,
    overlays: Vec<String>,
    /// Consumed one per `ask_task_overlay` call; `None` ends the loop.
    overlay_cursor: usize,
    agent: Option<String>,
    model: Option<String>,
    mount_scope: MountScope,
    confirm_delete: bool,
    messages: Arc<Mutex<Vec<String>>>,
}

impl ScriptedFrontend {
    fn interview(repo: PathBuf) -> Self {
        Self {
            name: "issue-triage".into(),
            description: "when an issue opens, plan it".into(),
            interval: "10m".into(),
            workspace_choice: TaskWorkspaceChoice::CustomFolderOrRepo,
            repo,
            overlays: Vec::new(),
            overlay_cursor: 0,
            agent: Some("claude".into()),
            model: Some("claude-opus-5".into()),
            mount_scope: MountScope::Cwd,
            confirm_delete: false,
            messages: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn with_delete(confirm: bool) -> Self {
        Self {
            confirm_delete: confirm,
            ..Self::interview(PathBuf::from("/repo"))
        }
    }
}

impl UserMessageSink for ScriptedFrontend {
    fn write_message(&mut self, msg: UserMessage) {
        self.messages.lock().unwrap().push(msg.text);
    }
    fn replay_queued(&mut self) {}
}

impl SquadCommandFrontend for ScriptedFrontend {
    fn ask_task_name(&mut self) -> Result<String, CommandError> {
        Ok(self.name.clone())
    }
    fn ask_task_description(&mut self) -> Result<String, CommandError> {
        Ok(self.description.clone())
    }
    fn ask_task_interval(&mut self) -> Result<String, CommandError> {
        Ok(self.interval.clone())
    }
    fn ask_task_workspace_choice(&mut self) -> Result<TaskWorkspaceChoice, CommandError> {
        Ok(self.workspace_choice)
    }
    fn ask_task_repo(&mut self) -> Result<PathBuf, CommandError> {
        Ok(self.repo.clone())
    }
    fn confirm_non_git_workspace(&mut self, _path: &Path) -> Result<bool, CommandError> {
        // The scripted interview keeps whatever path it was handed; the
        // warn/keep loop itself is exercised separately.
        Ok(true)
    }
    fn confirm_parent_directory_workspace(
        &mut self,
        _path: &Path,
        _current_dir: &Path,
    ) -> Result<bool, CommandError> {
        Ok(true)
    }
    fn ask_task_overlay(&mut self, _existing: &[String]) -> Result<Option<String>, CommandError> {
        let index = self.overlay_cursor;
        self.overlay_cursor += 1;
        Ok(self.overlays.get(index).cloned())
    }
    fn ask_task_agent(&mut self) -> Result<Option<String>, CommandError> {
        Ok(self.agent.clone())
    }
    fn ask_task_model(&mut self) -> Result<Option<String>, CommandError> {
        Ok(self.model.clone())
    }
    fn ask_task_mount_scope(&mut self) -> Result<MountScope, CommandError> {
        Ok(self.mount_scope)
    }
    fn ask_delete_task_dir(&mut self, _name: &str, _path: &Path) -> Result<bool, CommandError> {
        Ok(self.confirm_delete)
    }
}

fn docker_engines(root: &Path) -> Engines {
    engines_with(
        Arc::new(ContainerRuntime::docker()) as Arc<dyn AgentRuntimeEngine>,
        true,
        root,
    )
}

fn sandbox_engines(root: &Path) -> Engines {
    engines_with(
        Arc::new(FakeSandboxRuntime) as Arc<dyn AgentRuntimeEngine>,
        false,
        root,
    )
}

fn engines_with(runtime: Arc<dyn AgentRuntimeEngine>, container: bool, root: &Path) -> Engines {
    let api_paths = ApiPaths::from_root(root);
    let auth_paths = AuthPathResolver::at_home(root);
    let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    let docker = Arc::new(ContainerRuntime::docker());
    Engines {
        runtime,
        container_runtime: if container {
            Some(docker.clone())
        } else {
            None
        },
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::new()),
        overlay_engine: overlay_engine.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, api_paths.clone())),
        agent_engine: Arc::new(AgentEngine::new(overlay_engine, docker)),
        workflow_state_store: Arc::new(EngineWorkflowStateStore::at_git_root(api_paths.root())),
    }
}

// ─── BLOCKER-3: the interview lives in Layer 2 ──────────────────────────────

#[tokio::test]
async fn interview_collects_every_field_and_reaches_create_with_one_task() {
    let tmp = tempfile::tempdir().unwrap();
    let gateway = Arc::new(RecordingGateway::default());
    let frontend = ScriptedFrontend::interview(tmp.path().to_path_buf());

    let command = SquadCommand::new(
        SquadSubcommand::Add(SquadAddRequest {
            interview: true,
            non_interactive: false,
            prefilled: None,
        }),
        Some(Box::new(SharedRecording(gateway.clone()))),
        docker_engines(tmp.path()),
    );
    let outcome = command
        .run_with_frontend(Box::new(frontend))
        .await
        .expect("interview add must succeed");
    assert!(matches!(outcome, SquadOutcome::Task(_)));

    let created = gateway.created.lock().unwrap();
    assert_eq!(created.len(), 1, "exactly one create call");
    let req = &created[0];
    assert_eq!(req.name, "issue-triage");
    assert_eq!(req.description, "when an issue opens, plan it");
    assert_eq!(req.interval_secs, 600, "10m must parse to 600s in Layer 2");
    assert_eq!(
        req.workspace,
        awman::data::fs::TaskWorkspace::Custom(tmp.path().to_path_buf()),
        "the interview's custom-folder answer reaches Layer 2 as the workspace choice"
    );
    assert_eq!(req.agent.as_deref(), Some("claude"));
    assert_eq!(req.model.as_deref(), Some("claude-opus-5"));
    assert_eq!(req.mount_scope, MountScope::Cwd);
}

// ─── BLOCKER-2: remove deletes (or keeps) the persistent directory ──────────

async fn run_remove(root: &Path, name: &str, yes: bool, confirm: bool) -> SquadOutcome {
    let gateway = Arc::new(RecordingGateway::default());
    let mut frontend = ScriptedFrontend::with_delete(confirm);
    frontend.name = name.to_string();
    let command = SquadCommand::new(
        SquadSubcommand::Remove {
            name: name.to_string(),
            yes,
        },
        Some(Box::new(SharedRecording(gateway.clone()))),
        docker_engines(root),
    );
    let outcome = command
        .run_with_frontend(Box::new(frontend))
        .await
        .expect("remove must succeed");
    assert_eq!(
        gateway.deleted.lock().unwrap().as_slice(),
        &[name.to_string()]
    );
    outcome
}

#[tokio::test]
async fn remove_with_yes_deletes_the_persistent_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = ENV_LOCK.lock().await;
    let previous = std::env::var(AWMAN_SQUAD_ROOT).ok();
    std::env::set_var(AWMAN_SQUAD_ROOT, tmp.path());

    let dir = SquadPaths::from_root(tmp.path())
        .task_dir("issue-triage")
        .unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("workflow.toml"), "x").unwrap();

    let outcome = run_remove(tmp.path(), "issue-triage", true, false).await;

    restore(AWMAN_SQUAD_ROOT, previous);
    assert!(!dir.exists(), "`-y` must delete the task directory");
    assert!(matches!(
        outcome,
        SquadOutcome::Removed {
            removed_dir: Some(_),
            ..
        }
    ));
}

#[tokio::test]
async fn remove_declined_keeps_the_persistent_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = ENV_LOCK.lock().await;
    let previous = std::env::var(AWMAN_SQUAD_ROOT).ok();
    std::env::set_var(AWMAN_SQUAD_ROOT, tmp.path());

    let dir = SquadPaths::from_root(tmp.path())
        .task_dir("issue-triage")
        .unwrap();
    std::fs::create_dir_all(&dir).unwrap();

    // No `-y`, and the frontend declines: the directory survives.
    let outcome = run_remove(tmp.path(), "issue-triage", false, false).await;

    restore(AWMAN_SQUAD_ROOT, previous);
    assert!(dir.exists(), "declining must keep the task directory");
    assert!(matches!(
        outcome,
        SquadOutcome::Removed {
            removed_dir: None,
            ..
        }
    ));
}

#[tokio::test]
async fn remove_confirmed_via_frontend_deletes_the_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = ENV_LOCK.lock().await;
    let previous = std::env::var(AWMAN_SQUAD_ROOT).ok();
    std::env::set_var(AWMAN_SQUAD_ROOT, tmp.path());

    let dir = SquadPaths::from_root(tmp.path())
        .task_dir("issue-triage")
        .unwrap();
    std::fs::create_dir_all(&dir).unwrap();

    // No `-y`, but the frontend confirms: the directory is removed.
    let outcome = run_remove(tmp.path(), "issue-triage", false, true).await;

    restore(AWMAN_SQUAD_ROOT, previous);
    assert!(
        !dir.exists(),
        "a confirmed prompt must delete the directory"
    );
    assert!(matches!(
        outcome,
        SquadOutcome::Removed {
            removed_dir: Some(_),
            ..
        }
    ));
}

#[tokio::test]
async fn remove_a_crafted_name_cannot_escape_the_tasks_root() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = ENV_LOCK.lock().await;
    let previous = std::env::var(AWMAN_SQUAD_ROOT).ok();
    std::env::set_var(AWMAN_SQUAD_ROOT, tmp.path());

    // `..` is resolved by canonicalization only when the target exists, so
    // materialize both the tasks root and the escape target (mirroring
    // `SquadPaths::task_dir`'s own escape test).
    let paths = SquadPaths::from_root(tmp.path());
    std::fs::create_dir_all(paths.tasks_dir()).unwrap();
    let escape = tmp.path().join("escape");
    std::fs::create_dir_all(&escape).unwrap();

    let gateway = Arc::new(RecordingGateway::default());
    let command = SquadCommand::new(
        SquadSubcommand::Remove {
            name: "../escape".to_string(),
            yes: true,
        },
        Some(Box::new(SharedRecording(gateway.clone()))),
        docker_engines(tmp.path()),
    );
    let result = command
        .run_with_frontend(Box::new(ScriptedFrontend::with_delete(true)))
        .await;

    restore(AWMAN_SQUAD_ROOT, previous);
    // The directory removal refused the escape, and the escape target survives.
    assert!(
        result.is_err(),
        "a name escaping the tasks root must be rejected before any deletion"
    );
    assert!(escape.exists(), "the escape target must never be deleted");
}

// ─── BLOCKER-5: `squad logs` (no follow) dumps the existing log locally ──────

#[tokio::test]
async fn logs_without_follow_emits_the_existing_lines() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = ENV_LOCK.lock().await;
    let previous = std::env::var(AWMAN_SQUAD_ROOT).ok();
    std::env::set_var(AWMAN_SQUAD_ROOT, tmp.path());

    let log_path = SquadPaths::from_root(tmp.path()).daemon().log_file();
    std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
    std::fs::write(&log_path, "line one\nline two\n").unwrap();

    let messages = Arc::new(Mutex::new(Vec::new()));
    let mut frontend = ScriptedFrontend::with_delete(false);
    frontend.messages = messages.clone();

    let command = SquadCommand::new(
        SquadSubcommand::Logs(awman::command::commands::squad::daemon::SquadLogsFlags {
            follow: false,
        }),
        None,
        docker_engines(tmp.path()),
    );
    command
        .run_with_frontend(Box::new(frontend))
        .await
        .expect("logs must succeed");

    restore(AWMAN_SQUAD_ROOT, previous);
    let captured = messages.lock().unwrap();
    assert!(
        captured.iter().any(|m| m == "line one") && captured.iter().any(|m| m == "line two"),
        "logs must emit every existing line: {captured:?}"
    );
}

// ─── Edge-case #1: CLI CRUD, status and bare all refuse under a sandbox ─────

#[tokio::test]
async fn cli_squad_entry_points_fail_fast_under_a_sandbox_runtime() {
    let env = helpers::IsolatedEnv::new();
    let tmp = tempfile::tempdir().unwrap();

    let _guard = ENV_LOCK.lock().await;
    let previous = std::env::var(AWMAN_SQUAD_ROOT).ok();
    // Point the squad root at a throwaway dir: if the tier check regressed and a
    // daemon child were provisioned, its key hash / pidfile would land here.
    std::env::set_var(AWMAN_SQUAD_ROOT, tmp.path());

    for raw in [
        vec!["awman", "squad", "list"],
        vec!["awman", "squad", "status"],
        vec!["awman", "squad", "add", "--name", "x", "--description", "y"],
        vec!["awman", "squad", "-n"],
    ] {
        let matches = awman::command::dispatch::catalogue::CommandCatalogue::get()
            .build_clap_command()
            .try_get_matches_from(&raw)
            .unwrap_or_else(|e| panic!("argv {raw:?} rejected: {e}"));
        let ctx = awman::frontend::cli::RuntimeContext::new(
            env.open_session(),
            sandbox_engines(tmp.path()),
        );
        // Must return promptly: without the tier pre-check it would provision a
        // key and block ~10s polling for a daemon child that refuses to start.
        let _exit: ExitCode = tokio::time::timeout(
            Duration::from_secs(5),
            awman::frontend::cli::run(matches, ctx),
        )
        .await
        .unwrap_or_else(|_| panic!("squad {raw:?} hung under a sandbox runtime"));
    }

    // No entry point may have provisioned a key or claimed a pidfile: the tier
    // refusal fires before `SquadSupervisor::ensure_running`.
    let daemon = SquadPaths::from_root(tmp.path()).daemon();
    restore(AWMAN_SQUAD_ROOT, previous);
    assert!(
        !daemon.key_hash_file().exists(),
        "sandbox refusal must fire before any key is provisioned"
    );
    assert!(
        !daemon.pid_file().exists(),
        "sandbox refusal must spawn no daemon"
    );
}

// ─── helpers ────────────────────────────────────────────────────────────────

fn restore(key: &str, previous: Option<String>) {
    match previous {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
}

/// Boxable adaptor so an `Arc<RecordingGateway>` can be handed to `SquadCommand`
/// as a `Box<dyn TaskGateway>` while the test keeps its own handle.
struct SharedRecording(Arc<RecordingGateway>);

#[async_trait]
impl TaskGateway for SharedRecording {
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

// A fake runtime that only ever reports the sandbox tier's name, mirroring the
// one in `squad_sandbox_refusal.rs`.
struct FakeSandboxRuntime;

impl AgentRuntimeEngine for FakeSandboxRuntime {
    fn runtime_name(&self) -> &'static str {
        "docker-sbx-experimental"
    }
    fn display_name(&self) -> &'static str {
        "Docker Sandboxes (experimental)"
    }
    fn capabilities(&self) -> &Capabilities {
        static CAPS: Capabilities = Capabilities {
            arbitrary_env_vars: false,
            arbitrary_host_mounts: false,
            cpu_limits: false,
            per_resource_stats: false,
            persistent_lifecycle: true,
            kit_declarative: true,
            dind: DindSupport::Always,
            host_paths_visible: false,
            session_label_supported: false,
        };
        &CAPS
    }
    fn is_available(&self) -> bool {
        unimplemented!("never reached — require_container_tier refuses first")
    }
    fn build(
        &self,
        _options: ResolvedAgentOptions,
    ) -> Result<Box<dyn AgentInstance>, awman::engine::error::EngineError> {
        unimplemented!("never reached — require_container_tier refuses first")
    }
    fn list_running(
        &self,
        _session: &Session,
    ) -> Result<Vec<AgentHandle>, awman::engine::error::EngineError> {
        unimplemented!("never reached — require_container_tier refuses first")
    }
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, awman::engine::error::EngineError> {
        unimplemented!("never reached — require_container_tier refuses first")
    }
    fn stats(
        &self,
        _handle: &AgentHandle,
    ) -> Result<AgentStats, awman::engine::error::EngineError> {
        unimplemented!("never reached — require_container_tier refuses first")
    }
    fn stop(&self, _handle: &AgentHandle) -> Result<(), awman::engine::error::EngineError> {
        unimplemented!("never reached — require_container_tier refuses first")
    }
    fn exec_args(
        &self,
        _agent_id: &str,
        _working_dir: &str,
        _entrypoint: &[&str],
        _env_vars: &[(&str, &str)],
    ) -> Vec<String> {
        unimplemented!("never reached — require_container_tier refuses first")
    }
    fn attach(
        &self,
        _handle: &AgentHandle,
    ) -> Result<Box<dyn AgentInstance>, awman::engine::error::EngineError> {
        unimplemented!("never reached — require_container_tier refuses first")
    }
    fn list_running_with_name_prefix(
        &self,
        _prefix: &str,
    ) -> Result<Vec<AgentHandle>, awman::engine::error::EngineError> {
        unimplemented!("never reached — require_container_tier refuses first")
    }
    fn cli_binary(&self) -> &'static str {
        "sbx"
    }
}
