//! WI 0101 — the amie daemon's HTTP surface (`frontend::amie::serve_with`).
//!
//! Boots the real daemon via the injectable `serve_with` bootstrap (the same
//! seam `command-layer`'s eventual `LocalConditionEvaluator` will use) on an
//! ephemeral loopback port, drives it with `reqwest`, and tears down by
//! aborting the task — mirroring `tests/api_parity/live_server.rs`'s
//! established pattern for this codebase's other HTTP daemon.
//!
//! `serve_with` reads `AWMAN_CONFIG_HOME` (and friends) from the real process
//! environment (`Env::from_process()` is hardcoded inside it), so every test
//! here scopes that env var for its duration under a shared lock — the same
//! technique `tests/data_layer/rename_0077.rs` and
//! `tests/overlays_integration.rs` already use for env-mutating tests.

use std::sync::Arc;
use std::time::Duration;

use awman::command::commands::amie::commands::AmieServeConfig;
use awman::command::dispatch::Engines;
use awman::data::fs::daemon_process::{DaemonProcess, AMIE_PLIST_LABEL, AMIE_UNIT_NAME};
use awman::data::fs::{AmiePaths, ApiPaths, AuthPathResolver};
use awman::data::EngineWorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::amie::{ConditionEvaluator, EvaluationOutcome, EvaluationRequest};
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use tokio::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::const_new(());

struct NeverTriggeredEvaluator;
#[async_trait::async_trait]
impl ConditionEvaluator for NeverTriggeredEvaluator {
    async fn evaluate(&self, _request: EvaluationRequest) -> EvaluationOutcome {
        EvaluationOutcome::NotTriggered
    }
}

fn engines_with(container_runtime: Arc<ContainerRuntime>, root: &std::path::Path) -> Engines {
    let api_paths = ApiPaths::from_root(root);
    let auth_paths = AuthPathResolver::at_home(root);
    let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    Engines {
        runtime: container_runtime.clone(),
        container_runtime: Some(container_runtime.clone()),
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::new()),
        overlay_engine: overlay_engine.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, api_paths.clone())),
        agent_engine: Arc::new(AgentEngine::new(overlay_engine, container_runtime)),
        workflow_state_store: Arc::new(EngineWorkflowStateStore::at_git_root(api_paths.root())),
    }
}

/// Sets `AWMAN_CONFIG_HOME` to `root`, starts the daemon in a background
/// task, and polls for its published metadata. Returns the task handle and
/// the resolved `http://127.0.0.1:<port>` base URL. Restores the previous
/// env var value once metadata has been read (the daemon itself already
/// captured its own `EnvSnapshot` by then).
async fn start_daemon(
    root: &std::path::Path,
    container_runtime: Arc<ContainerRuntime>,
) -> (tokio::task::JoinHandle<()>, String) {
    start_daemon_with(root, container_runtime, Arc::new(NeverTriggeredEvaluator)).await
}

/// The same bootstrap with a caller-supplied evaluator, for the tests that
/// need an evaluation to actually be in flight.
async fn start_daemon_with(
    root: &std::path::Path,
    container_runtime: Arc<ContainerRuntime>,
    evaluator: Arc<dyn ConditionEvaluator>,
) -> (tokio::task::JoinHandle<()>, String) {
    let engines = engines_with(container_runtime, root);
    let config = AmieServeConfig {
        port: 0,
        dangerously_skip_auth: true,
    };

    let previous = std::env::var("AWMAN_CONFIG_HOME").ok();
    std::env::set_var("AWMAN_CONFIG_HOME", root);

    let handle = tokio::spawn(async move {
        let _ = awman::frontend::amie::serve_with(config, engines, evaluator).await;
    });

    let daemon = DaemonProcess::new(
        AmiePaths::from_root(root.join("amie")).daemon(),
        AMIE_UNIT_NAME,
        AMIE_PLIST_LABEL,
    );
    let mut waited = Duration::ZERO;
    let meta = loop {
        if let Ok(Some(meta)) = daemon.read_meta() {
            break meta;
        }
        if waited >= Duration::from_secs(10) {
            panic!("amie daemon never published its server metadata");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        waited += Duration::from_millis(50);
    };

    match previous {
        Some(v) => std::env::set_var("AWMAN_CONFIG_HOME", v),
        None => std::env::remove_var("AWMAN_CONFIG_HOME"),
    }

    (
        handle,
        format!("{}://{}:{}", meta.scheme, meta.bind_ip, meta.port),
    )
}

// ─── Startup succeeds under both Docker and Apple, no tier branching ────────

#[tokio::test]
async fn daemon_startup_succeeds_under_docker_and_apple_with_no_tier_branching() {
    let _env_guard = ENV_LOCK.lock().await;

    for runtime in [
        Arc::new(ContainerRuntime::docker()),
        Arc::new(ContainerRuntime::apple()),
    ] {
        let name = runtime.runtime_name();
        let tmp = tempfile::tempdir().unwrap();
        let (handle, base) = start_daemon(tmp.path(), runtime).await;

        let resp = reqwest::get(format!("{base}/v1/status"))
            .await
            .unwrap_or_else(|e| panic!("{name}: status request must succeed: {e}"));
        assert_eq!(
            resp.status(),
            200,
            "{name}: /v1/status must succeed identically regardless of container tier"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["running"], true);

        handle.abort();
    }
}

// ─── POST /v1/commands rejections ────────────────────────────────────────────

#[tokio::test]
async fn post_commands_rejects_non_amie_subcommand() {
    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let (handle, base) = start_daemon(tmp.path(), Arc::new(ContainerRuntime::docker())).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/commands"))
        .json(&serde_json::json!({"subcommand": "exec workflow", "args": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("amie subtree"),
        "error must explain the amie-only subtree restriction: {body}"
    );

    handle.abort();
}

#[tokio::test]
async fn post_commands_rejects_unknown_flag_with_catalogue_error_shape() {
    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let (handle, base) = start_daemon(tmp.path(), Arc::new(ContainerRuntime::docker())).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/commands"))
        .json(&serde_json::json!({
            "subcommand": "amie add",
            "args": ["--this-flag-does-not-exist", "value"],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body.get("error")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty()),
        "unknown-flag rejection must use the shared {{\"error\": \"...\"}} envelope: {body}"
    );

    handle.abort();
}

#[tokio::test]
async fn attach_is_refused_by_the_catalogue_even_though_it_exists_for_cli_tui() {
    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let (handle, base) = start_daemon(tmp.path(), Arc::new(ContainerRuntime::docker())).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/commands"))
        .json(&serde_json::json!({"subcommand": "amie attach", "args": ["some-condition"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        400,
        "`amie attach` is api_allowed: false in the catalogue and must be refused"
    );

    handle.abort();
}

// ─── Loopback-only bind ──────────────────────────────────────────────────────

/// This host's own non-loopback IPv4 address, discovered without sending a
/// packet: connecting a UDP socket only sets the kernel's route, and
/// `local_addr` then reports the interface it chose. Returns `None` on a host
/// with no outbound route (a fully isolated container), where the property
/// below cannot be observed.
fn non_loopback_local_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("203.0.113.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

#[tokio::test]
async fn daemon_binds_loopback_only() {
    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let (handle, base) = start_daemon(tmp.path(), Arc::new(ContainerRuntime::docker())).await;

    assert!(
        base.starts_with("http://127.0.0.1:"),
        "amie always binds 127.0.0.1 regardless of any configuration input: {base}"
    );

    // The published metadata is the daemon's own record of what it bound;
    // corroborate it actually answers there.
    let resp = reqwest::get(format!("{base}/v1/status")).await.unwrap();
    assert_eq!(resp.status(), 200);

    let port: u16 = base
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .expect("the published endpoint must carry a port");

    // The same port on this host's *external* interface must not accept a
    // connection at all — the socket is bound to 127.0.0.1, not 0.0.0.0.
    match non_loopback_local_ip() {
        Some(ip) => {
            let addr = std::net::SocketAddr::new(ip, port);
            let refused = tokio::task::spawn_blocking(move || {
                std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2))
            })
            .await
            .unwrap();
            assert!(
                refused.is_err(),
                "the daemon must not answer on the non-loopback interface {ip}:{port}"
            );
        }
        None => eprintln!("SKIP (partial): host has no non-loopback IPv4 interface to probe"),
    }

    handle.abort();
}

// ─── The live workflow-state route ──────────────────────────────────────────

/// An evaluator that stands in for a triggered condition whose generated
/// workflow is *currently executing*: it persists a real `WorkflowState`
/// through the engine's own `WorkflowStateStore`, reports the resulting path
/// through the `RunProgress` seam (exactly as `LocalConditionEvaluator` does
/// before it hands off to `ExecWorkflowCommand`), then parks so the run row
/// stays `running` while the test queries the route.
struct WorkflowInFlightEvaluator {
    state_dir: std::path::PathBuf,
    started: tokio::sync::mpsc::UnboundedSender<()>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl ConditionEvaluator for WorkflowInFlightEvaluator {
    async fn evaluate(&self, request: EvaluationRequest) -> EvaluationOutcome {
        let steps = vec![awman::data::workflow_definition::WorkflowStep {
            name: "analyze".to_string(),
            depends_on: Vec::new(),
            prompt_template: "analyze the issue".to_string(),
            agent: Some("claude".to_string()),
            model: None,
            overlays: None,
            abort_on_failure: false,
        }];
        let state = awman::data::workflow_state::WorkflowState::new(
            "amie-generated".to_string(),
            &steps,
            "deadbeef".to_string(),
            None,
        );
        let store = EngineWorkflowStateStore::at_git_root(&self.state_dir);
        let state_path = store.save(&state).expect("persisting workflow state");

        let workflow_path = request.condition_dir.join("workflow.toml");
        request
            .progress
            .workflow_started(&request.run_id, &workflow_path, &state_path);

        let _ = self.started.send(());
        // Park while the run row is still `running`, which is the only window
        // in which the route is supposed to answer.
        self.release.notified().await;

        EvaluationOutcome::WorkflowExecuted {
            workflow_path,
            workflow_state_path: Some(state_path),
            exit_code: Some(0),
        }
    }
}

#[tokio::test]
async fn the_workflow_route_serves_the_live_state_verbatim_while_a_run_is_in_flight() {
    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();

    // A condition that is immediately due, seeded straight into the shared
    // database the daemon is about to open.
    let db = tmp.path().join("data").join("awman.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let store = awman::data::fs::ConditionStore::open(&db).unwrap();
    store.migrate().unwrap();
    let now = chrono::Utc::now();
    store
        .create(&awman::data::fs::Condition {
            id: uuid::Uuid::new_v4().to_string(),
            name: "issue-triage".into(),
            description: "watch new issues".into(),
            repo_scope: tmp.path().to_path_buf(),
            mount_scope: awman::data::fs::MountScope::GitRoot,
            interval_secs: 60,
            status: awman::data::fs::ConditionStatus::Active,
            agent: None,
            model: None,
            backoff_until: None,
            created_at: now,
            updated_at: now,
            last_run_at: None,
        })
        .unwrap();
    drop(store);

    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let evaluator = Arc::new(WorkflowInFlightEvaluator {
        state_dir: tmp.path().join("state"),
        started: started_tx,
        release: release.clone(),
    });

    let (handle, base) =
        start_daemon_with(tmp.path(), Arc::new(ContainerRuntime::docker()), evaluator).await;

    // Wait for the scheduler's first tick to reach the evaluator.
    tokio::time::timeout(Duration::from_secs(10), started_rx.recv())
        .await
        .expect("the scheduler must dispatch the due condition on its first tick")
        .expect("evaluator must signal that the workflow started");

    let resp = reqwest::get(format!("{base}/v1/conditions/issue-triage/workflow"))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "the route must serve the live workflow state while the run is in flight"
    );
    let body: serde_json::Value = resp.json().await.unwrap();

    // The contract is the *verbatim* `WorkflowState`, no projection or DTO —
    // a client must be able to deserialize it straight back into the type.
    let round_tripped: awman::data::workflow_state::WorkflowState =
        serde_json::from_value(body.clone())
            .expect("the route must return a WorkflowState verbatim, not a projection");
    assert_eq!(round_tripped.workflow_name, "amie-generated");
    assert_eq!(round_tripped.workflow_hash, "deadbeef");
    assert!(round_tripped.step_states.contains_key("analyze"));

    release.notify_waiters();
    handle.abort();
}
