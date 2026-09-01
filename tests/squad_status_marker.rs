//! WI 0101 — `awman status` renders the `squad:<task>` source marker.
//!
//! The primary test drives the real `StatusCommand` end-to-end against a fake
//! `AgentRuntimeEngine` (no live Docker/Apple daemon needed) whose
//! `list_running_all()` returns one squad-launched container and one plain
//! session container, proving the marker is additive (the session row renders
//! exactly as it always has) and derived purely from the container **name**
//! (`AgentHandle` has no label field to read back).
//!
//! Docker- and Apple-gated tiers then plant a real squad-named container and
//! a real session-named container on each live backend and assert the same
//! marker survives a round trip through that backend's own
//! `list_running_all()`, skipping cleanly when the backend is unavailable.

use std::sync::Arc;

use awman::command::commands::status::{
    ContainerKind, ContainerSource, StatusCommand, StatusCommandFlags, StatusCommandFrontend,
    StatusOutcome,
};
use awman::command::commands::Command;
use awman::command::dispatch::Engines;
use awman::data::fs::{ApiPaths, AuthPathResolver};
use awman::data::message::{UserMessage, UserMessageSink};
use awman::data::session::{AgentHandle, Session};
use awman::data::EngineWorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::agent_runtime::execution::AgentInstance;
use awman::engine::agent_runtime::{
    AgentRuntimeEngine, AgentStats, Capabilities, DindSupport, ResolvedAgentOptions,
};
use awman::engine::auth::AuthEngine;
use awman::engine::container::naming::generate_squad_container_name;
use awman::engine::container::ContainerRuntime;
use awman::engine::error::EngineError;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;

// ─── A fake runtime returning canned handles, no label concept ───────────────

struct FakeRuntime {
    handles: Vec<AgentHandle>,
}

impl AgentRuntimeEngine for FakeRuntime {
    fn runtime_name(&self) -> &'static str {
        "fake"
    }
    fn display_name(&self) -> &'static str {
        "Fake"
    }
    fn capabilities(&self) -> &Capabilities {
        static CAPS: Capabilities = Capabilities {
            arbitrary_env_vars: true,
            arbitrary_host_mounts: true,
            cpu_limits: true,
            per_resource_stats: true,
            persistent_lifecycle: false,
            kit_declarative: false,
            dind: DindSupport::Never,
            host_paths_visible: true,
            session_label_supported: false,
        };
        &CAPS
    }
    fn is_available(&self) -> bool {
        true
    }
    fn build(&self, _options: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
        unimplemented!("not exercised by the status test")
    }
    fn list_running(&self, _session: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(self.handles.clone())
    }
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(self.handles.clone())
    }
    fn stats(&self, _handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        // `StatusCommand` treats a stats failure as best-effort (`.ok()`), so
        // returning an error here (rather than panicking) exercises exactly
        // the path a real backend takes for a container it can't inspect.
        Err(EngineError::NotImplemented(
            "stats not exercised in this test",
        ))
    }
    fn stop(&self, _handle: &AgentHandle) -> Result<(), EngineError> {
        unimplemented!("not exercised by the status test")
    }
    fn exec_args(
        &self,
        _agent_id: &str,
        _working_dir: &str,
        _entrypoint: &[&str],
        _env_vars: &[(&str, &str)],
    ) -> Vec<String> {
        unimplemented!("not exercised by the status test")
    }
    fn attach(&self, _handle: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        unimplemented!("not exercised by the status test")
    }
    fn list_running_with_name_prefix(&self, prefix: &str) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(self
            .handles
            .iter()
            .filter(|h| h.name.starts_with(prefix))
            .cloned()
            .collect())
    }
    fn cli_binary(&self) -> &'static str {
        "fake"
    }
}

fn handle(name: &str) -> AgentHandle {
    AgentHandle {
        id: format!("id-{name}"),
        image_tag: "awman/dev:latest".into(),
        name: name.to_string(),
        started_at: chrono::Utc::now(),
    }
}

fn engines_with(runtime: Arc<dyn AgentRuntimeEngine>) -> Engines {
    let tmp = tempfile::tempdir().unwrap();
    let api_paths = ApiPaths::from_root(tmp.path());
    let auth_paths = AuthPathResolver::at_home(tmp.path());
    let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    let container_runtime = Arc::new(ContainerRuntime::docker());
    Engines {
        runtime,
        container_runtime: Some(container_runtime.clone()),
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::new()),
        overlay_engine: overlay_engine.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, api_paths.clone())),
        agent_engine: Arc::new(AgentEngine::new(overlay_engine, container_runtime)),
        workflow_state_store: Arc::new(EngineWorkflowStateStore::at_git_root(api_paths.root())),
    }
}

// ─── A minimal frontend capturing the outcome ─────────────────────────────────

#[derive(Default)]
struct CapturingFrontend {
    queue: Vec<UserMessage>,
}

impl UserMessageSink for CapturingFrontend {
    fn write_message(&mut self, msg: UserMessage) {
        self.queue.push(msg);
    }
    fn replay_queued(&mut self) {
        self.queue.clear();
    }
}

impl StatusCommandFrontend for CapturingFrontend {}

async fn run_status(runtime: Arc<dyn AgentRuntimeEngine>) -> StatusOutcome {
    let engines = engines_with(runtime);
    let cmd = StatusCommand::new(StatusCommandFlags { watch: false }, engines);
    cmd.run_with_frontend(Box::new(CapturingFrontend::default()))
        .await
        .expect("status command must succeed")
}

#[tokio::test]
async fn status_marks_squad_container_and_leaves_session_container_unmarked() {
    let squad_name = generate_squad_container_name("issue-triage");
    let runtime = Arc::new(FakeRuntime {
        handles: vec![handle(&squad_name), handle("awman-1234-5678")],
    });

    let outcome = run_status(runtime).await;
    assert_eq!(outcome.containers.len(), 2);

    let squad_row = outcome
        .containers
        .iter()
        .find(|c| c.name == squad_name)
        .expect("squad container row must be present");
    assert_eq!(
        squad_row.source,
        ContainerSource::Squad("issue-triage".to_string())
    );
    assert_eq!(
        squad_row.source_label().as_deref(),
        Some("squad:issue-triage")
    );
    assert_eq!(squad_row.kind, ContainerKind::Agent);

    let session_row = outcome
        .containers
        .iter()
        .find(|c| c.name == "awman-1234-5678")
        .expect("session container row must be present");
    assert_eq!(
        session_row.source,
        ContainerSource::Session,
        "a plain session container must render exactly as today — unmarked"
    );
    assert_eq!(session_row.source_label(), None);
}

#[tokio::test]
async fn status_renders_squad_marker_in_the_dashboard_text() {
    // Exercise the default `write_status_dashboard` text path too (used by
    // the API/fallback frontend), not just the structured `StatusOutcome`.
    #[derive(Clone, Default)]
    struct TextCapturingFrontend {
        lines: Arc<std::sync::Mutex<Vec<String>>>,
    }
    impl UserMessageSink for TextCapturingFrontend {
        fn write_message(&mut self, msg: UserMessage) {
            self.lines.lock().unwrap().push(msg.text);
        }
        fn replay_queued(&mut self) {}
    }
    impl StatusCommandFrontend for TextCapturingFrontend {}

    let squad_name = generate_squad_container_name("nightly-audit");
    let runtime = Arc::new(FakeRuntime {
        handles: vec![handle(&squad_name)],
    });
    let engines = engines_with(runtime);
    let cmd = StatusCommand::new(StatusCommandFlags { watch: false }, engines);
    let frontend = TextCapturingFrontend::default();
    let captured = frontend.lines.clone();
    // `write_status_dashboard`'s default body runs inline during
    // `run_with_frontend`; capture it by driving the real command and
    // inspecting what it queued through `write_message`.
    let _ = cmd.run_with_frontend(Box::new(frontend)).await;
    let combined = captured.lock().unwrap().join("\n");
    assert!(
        combined.contains("squad:nightly-audit"),
        "dashboard text must contain the squad marker: {combined}"
    );
}

// ─── Live-backend tiers ──────────────────────────────────────────────────────
//
// The fake-runtime tests above prove the classification and rendering. These
// prove the same marker survives a round trip through a *real* backend's
// `list_running_all()`: each tier plants one squad-named container and one
// plain session-named container with a raw CLI call, then asserts `awman
// status` marks exactly the first. Skips cleanly when the backend or the
// image is unavailable.

use std::process::{Command as OsCommand, Stdio};

fn unique_suffix() -> String {
    format!(
        "{:x}",
        (std::process::id() as u128) << 32
            | u128::from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .subsec_nanos()
            )
    )
}

fn cli_ok(cli: &str, args: &[&str]) -> bool {
    OsCommand::new(cli)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn remove_container(cli: &str, name: &str) {
    let _ = cli_ok(cli, &["rm", "-f", name]);
}

/// Plant one squad container and one session container on a live backend and
/// assert `awman status` marks exactly the squad one.
async fn assert_status_marks_squad_on(cli: &str, runtime: Arc<ContainerRuntime>) {
    let suffix = unique_suffix();
    let slug = format!("audit-{suffix}");
    let squad_name = generate_squad_container_name(&slug);
    let session_name = format!("awman-{suffix}-0");

    for name in [&squad_name, &session_name] {
        if !cli_ok(
            cli,
            &["run", "-d", "--name", name, "alpine:latest", "sleep", "120"],
        ) {
            remove_container(cli, &squad_name);
            remove_container(cli, &session_name);
            eprintln!("SKIP: `{cli} run` failed for the {name} fixture container");
            return;
        }
    }

    let outcome = run_status(runtime).await;
    remove_container(cli, &squad_name);
    remove_container(cli, &session_name);

    let squad_row = outcome
        .containers
        .iter()
        .find(|c| c.name == squad_name)
        .unwrap_or_else(|| panic!("`awman status` must list the squad container {squad_name}"));
    assert_eq!(
        squad_row.source,
        ContainerSource::Squad(slug.clone()),
        "a squad-launched container must render as squad:{slug}"
    );
    assert_eq!(
        squad_row.source_label().as_deref(),
        Some(&*format!("squad:{slug}"))
    );

    let session_row = outcome
        .containers
        .iter()
        .find(|c| c.name == session_name)
        .unwrap_or_else(|| panic!("`awman status` must list the session container {session_name}"));
    assert_eq!(
        session_row.source,
        ContainerSource::Session,
        "a user session's container must render exactly as it does today"
    );
    assert_eq!(session_row.source_label(), None);
}

// ─── Docker tier (skip cleanly if unavailable) ──────────────────────────────

#[tokio::test]
async fn docker_status_marks_squad_containers_when_backend_is_available() {
    let runtime = Arc::new(ContainerRuntime::docker());
    if !runtime.is_available() {
        eprintln!("SKIP: Docker daemon not available");
        return;
    }
    if !cli_ok("docker", &["pull", "alpine:latest"]) {
        eprintln!("SKIP: docker pull alpine:latest failed (no network?)");
        return;
    }
    assert_status_marks_squad_on("docker", runtime).await;
}

// ─── Apple tier (skip cleanly if unavailable) ───────────────────────────────

#[tokio::test]
async fn apple_status_marks_squad_containers_when_backend_is_available() {
    let runtime = Arc::new(ContainerRuntime::apple());
    if !runtime.is_available() {
        eprintln!("SKIP: Apple Containers not available");
        return;
    }
    if !cli_ok("container", &["pull", "alpine:latest"]) {
        eprintln!("SKIP: `container pull alpine:latest` failed");
        return;
    }
    assert_status_marks_squad_on("container", runtime).await;
}
