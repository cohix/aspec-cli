//! WI 0101 — amie refuses to run under the sandbox tier, at both daemon
//! startup and condition creation.
//!
//! `frontend::amie::serve_with` is the injectable bootstrap seam designed
//! for exactly this: it calls `require_container_tier` as its very first
//! action, before any path resolution, database open, or port bind. A fake
//! `AgentRuntimeEngine` reporting the sandbox tier's name lets this test
//! exercise the real refusal without needing a real sandbox backend (which
//! is itself platform-gated to macOS arm64 and unavailable in CI).

use std::sync::{Arc, Mutex};

use tokio::sync::Mutex as AsyncMutex;

use awman::command::commands::amie::commands::AmieServeConfig;
use awman::command::commands::amie::gateway::{
    ConditionGateway, CreateCondition, LocalConditionGateway,
};
use awman::command::dispatch::Engines;
use awman::data::fs::{ApiPaths, AuthPathResolver, ConditionStore, DataPaths, MountScope};
use awman::data::session::{AgentHandle, Session};
use awman::data::EngineWorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::agent_runtime::execution::AgentInstance;
use awman::engine::agent_runtime::{
    AgentRuntimeEngine, AgentStats, Capabilities, DindSupport, ResolvedAgentOptions,
};
use awman::engine::amie::{ConditionEvaluator, EvaluationOutcome, EvaluationRequest};
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::error::EngineError;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;

// ─── A fake runtime that only ever reports the sandbox tier's name ──────────

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
    fn build(&self, _options: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
        unimplemented!("never reached — require_container_tier refuses first")
    }
    fn list_running(&self, _session: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        unimplemented!("never reached — require_container_tier refuses first")
    }
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        unimplemented!("never reached — require_container_tier refuses first")
    }
    fn stats(&self, _handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        unimplemented!("never reached — require_container_tier refuses first")
    }
    fn stop(&self, _handle: &AgentHandle) -> Result<(), EngineError> {
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
    fn attach(&self, _handle: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        unimplemented!("never reached — require_container_tier refuses first")
    }
    fn list_running_with_name_prefix(
        &self,
        _prefix: &str,
    ) -> Result<Vec<AgentHandle>, EngineError> {
        unimplemented!("never reached — require_container_tier refuses first")
    }
    fn cli_binary(&self) -> &'static str {
        "sbx"
    }
}

struct NeverCalledEvaluator;
#[async_trait::async_trait]
impl ConditionEvaluator for NeverCalledEvaluator {
    async fn evaluate(&self, _request: EvaluationRequest) -> EvaluationOutcome {
        panic!("must never be called — the daemon must refuse before the scheduler ever ticks");
    }
}

fn engines_with(
    runtime: Arc<dyn AgentRuntimeEngine>,
    container_runtime: Option<Arc<ContainerRuntime>>,
    root: &std::path::Path,
) -> Engines {
    let api_paths = ApiPaths::from_root(root);
    let auth_paths = AuthPathResolver::at_home(root);
    let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    let agent_engine_runtime = container_runtime
        .clone()
        .unwrap_or_else(|| Arc::new(ContainerRuntime::docker()));
    Engines {
        runtime,
        container_runtime,
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::new()),
        overlay_engine: overlay_engine.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, api_paths.clone())),
        agent_engine: Arc::new(AgentEngine::new(overlay_engine, agent_engine_runtime)),
        workflow_state_store: Arc::new(EngineWorkflowStateStore::at_git_root(api_paths.root())),
    }
}

const EXPECTED_REFUSAL: &str = "amie requires a container runtime. The configured runtime \"docker-sbx-experimental\" cannot mount amie's condition directories or run workflow setup/teardown steps. Set runtime to \"docker\" or \"apple-containers\" to use amie.";

// ─── Daemon startup ──────────────────────────────────────────────────────────

#[tokio::test]
async fn daemon_startup_under_sandbox_runtime_fails_opens_no_store_binds_no_port() {
    let tmp = tempfile::tempdir().unwrap();
    let engines = engines_with(Arc::new(FakeSandboxRuntime), None, tmp.path());

    let config = AmieServeConfig {
        port: 0,
        dangerously_skip_auth: true,
    };

    // `serve_with` reads `AWMAN_CONFIG_HOME`/friends from the real process
    // environment (`Env::from_process()`), so scope it to this isolated root
    // for the duration of the call.
    let _env_guard = ENV_LOCK.lock().await;
    let previous = std::env::var("AWMAN_CONFIG_HOME").ok();
    std::env::set_var("AWMAN_CONFIG_HOME", tmp.path());

    let result =
        awman::frontend::amie::serve_with(config, engines, Arc::new(NeverCalledEvaluator)).await;

    match previous {
        Some(v) => std::env::set_var("AWMAN_CONFIG_HOME", v),
        None => std::env::remove_var("AWMAN_CONFIG_HOME"),
    }

    let err = result.expect_err("startup under the sandbox tier must fail");
    assert_eq!(err.to_string(), EXPECTED_REFUSAL);

    // Nothing downstream of the guard ran: no database file, no pidfile, no
    // server metadata (which is only written after a successful bind).
    let data_paths = DataPaths::at_root(tmp.path().join("data"));
    assert!(
        !data_paths.db_path().exists(),
        "the condition store must never be opened"
    );
    let amie_daemon = awman::data::fs::AmiePaths::from_root(tmp.path().join("amie")).daemon();
    assert!(
        !amie_daemon.pid_file().exists(),
        "no pidfile must be claimed"
    );
    assert!(
        !amie_daemon.server_meta_file().exists(),
        "no port must be bound / published"
    );
}

static ENV_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

// ─── Condition creation ──────────────────────────────────────────────────────

#[tokio::test]
async fn condition_creation_under_sandbox_runtime_is_rejected_with_the_same_error() {
    if !std::process::Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        eprintln!("SKIP: git not available");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .status()
        .unwrap();
    assert!(status.success());

    // `LocalConditionGateway::validate_create` reads `GlobalConfig::load()`
    // straight from the real process environment — scope it to this
    // isolated root so the test never depends on the host's own
    // `~/.awman/config.json` (e.g. a configured `amie.agentsToModels` there
    // would otherwise demand Dockerfiles this test repo doesn't have).
    let _env_guard = ENV_LOCK.lock().await;
    let previous = std::env::var("AWMAN_CONFIG_HOME").ok();
    std::env::set_var("AWMAN_CONFIG_HOME", tmp.path());

    let store = Arc::new(ConditionStore::open(&tmp.path().join("amie.db")).unwrap());

    store.migrate().unwrap();
    let status_handle = Arc::new(Mutex::new(awman::engine::amie::SchedulerStatus::default()));

    // First, under a real container tier, creation succeeds — this is the
    // "existing condition" the switch-runtime-after-creating case cares
    // about not corrupting.
    let docker_runtime = Arc::new(ContainerRuntime::docker());
    let docker_engines = engines_with(docker_runtime.clone(), Some(docker_runtime), tmp.path());
    let docker_gateway =
        LocalConditionGateway::new(store.clone(), docker_engines, status_handle.clone());
    docker_gateway
        .create(CreateCondition {
            name: "existing-condition".into(),
            description: "created before the runtime switch".into(),
            repo_scope: repo.clone(),
            mount_scope: MountScope::GitRoot,
            interval_secs: 60,
            agent: None,
            model: None,
        })
        .await
        .expect("creation under a container runtime must succeed");
    let count_before = store.list().unwrap().len();
    assert_eq!(count_before, 1);

    // Now the runtime has switched to sandbox — creating a *new* condition
    // must be rejected with the same required error text, and the existing
    // condition's state must be untouched (the guard runs before any store
    // mutation).
    let sandbox_engines = engines_with(Arc::new(FakeSandboxRuntime), None, tmp.path());
    let sandbox_gateway = LocalConditionGateway::new(store.clone(), sandbox_engines, status_handle);
    let err = sandbox_gateway
        .create(CreateCondition {
            name: "new-condition".into(),
            description: "attempted after the runtime switch".into(),
            repo_scope: repo,
            mount_scope: MountScope::GitRoot,
            interval_secs: 60,
            agent: None,
            model: None,
        })
        .await
        .expect_err("creation under the sandbox tier must be rejected");
    assert_eq!(err.to_string(), EXPECTED_REFUSAL);

    // The refusal must not have deleted or rewritten the existing condition.
    let conditions = store.list().unwrap();
    assert_eq!(
        conditions.len(),
        1,
        "the switch-runtime-after-creating case must never mutate existing conditions"
    );
    assert_eq!(conditions[0].name, "existing-condition");

    match previous {
        Some(v) => std::env::set_var("AWMAN_CONFIG_HOME", v),
        None => std::env::remove_var("AWMAN_CONFIG_HOME"),
    }
}
