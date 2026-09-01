//! WI-0107 integration coverage.  The descriptor is real, but its host file
//! and the `claude` program it pings are fixtures in a dedicated child process.
//! This is deliberately not an inline unit test: it proves the monitor,
//! descriptor, atomic writer, and host-refresh boundary compose correctly.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use awman::data::fs::auth_paths::AuthPathResolver;
use awman::data::message::{UserMessage, UserMessageSink};
use awman::data::session::AgentName;
use awman::data::session::{Session, SessionOpenOptions, StaticGitRootResolver};
use awman::data::workflow_definition::Workflow;
use awman::engine::agent_runtime::frontend::{AgentFrontend, AgentIo, AgentProgress, AgentStatus};
use awman::engine::auth::credential::{claude_spec, CredentialBinding, CredentialFingerprint};
use awman::engine::auth::RefreshableCredentialDelivery;
use awman::engine::container::options::ResolvedContainerOptions;
use awman::engine::container::{
    ContainerName, ContainerOption, ContainerRuntime, Entrypoint, ImageRef,
};
use awman::engine::credential_refresh::{
    install_global, CredentialRefreshMonitor, MonitorConfig, RefreshOutcome,
};
use awman::engine::error::EngineError;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::engine::workflow::actions::{
    AvailableActions, NextAction, ResumeMismatch, StepFailureChoice, WorkflowOutcome,
    WorkflowStepStatus, YoloTickOutcome,
};
use awman::engine::workflow::factory::{AgentExecutionFactory, WorkflowRuntimeContext};
use awman::engine::workflow::{Frontend as WorkflowFrontend, WorkflowEngine};

fn payload(token: &str, expires_at: SystemTime) -> String {
    let millis = expires_at.duration_since(UNIX_EPOCH).unwrap().as_millis();
    format!(
        r#"{{"claudeAiOauth":{{"accessToken":"{token}","expiresAt":{millis},"refreshToken":"fixture-refresh-token-never-delivered"}}}}"#
    )
}

fn host_file(home: &Path) -> PathBuf {
    home.join(".claude/.credentials.json")
}

/// Bind the fixture's planted credential FILE explicitly.
///
/// `claude_spec().source` returns the Keychain on macOS and ignores the home
/// directory, so deriving the source from a temp HOME would read whatever the
/// developer is logged into rather than the fixture — and find nothing on a CI
/// runner. An explicit file binding makes every test below mean the same thing
/// on every platform.
fn binding(home: &Path) -> CredentialBinding {
    CredentialBinding::to_file(AuthPathResolver::at_home(home), host_file(home))
}

fn materialized_delivery(home: &Path, staged_root: &Path) -> RefreshableCredentialDelivery {
    let spec = claude_spec();
    let snapshot =
        (spec.read)(&binding(home).source_for(spec)).expect("fixture credential must parse");
    let file = (spec.materialize)(&snapshot);
    std::fs::create_dir_all(staged_root).unwrap();
    std::fs::write(staged_root.join(&file.relative_path), &file.contents).unwrap();
    RefreshableCredentialDelivery {
        agent: AgentName::new("claude").unwrap(),
        spec_agent: spec.agent,
        credential_env_key: spec.credential_env_key,
        staged_path: staged_root.join(&file.relative_path),
        staged_root: staged_root.to_path_buf(),
        initial_fingerprint: CredentialFingerprint::of(&snapshot),
    }
}

/// The monitor under test, bound to the fixture child's HOME as an explicit
/// file source. `CredentialRefreshMonitor::new` would derive the source from
/// the platform instead, which on macOS means the real Keychain no matter what
/// HOME says — see [`binding`].
fn monitor() -> std::sync::Arc<CredentialRefreshMonitor> {
    let home = PathBuf::from(std::env::var_os("HOME").expect("fixture child sets HOME"));
    CredentialRefreshMonitor::with_binding(
        MonitorConfig {
            refresh_threshold: Duration::from_secs(20 * 60),
            // The direct `refresh_now` calls below are deterministic. A long
            // tick keeps later background ticks out of the way — but note the
            // FIRST tick lands immediately on `register`, so a test that needs
            // to own the rotation must still arrange for that tick to be a
            // no-op (see the docker e2e test).
            tick_interval: Duration::from_secs(60),
        },
        binding(&home),
    )
}

fn child_env() -> bool {
    std::env::var_os("AWMAN_0107_MONITOR_CHILD").is_some()
}

/// Run this exact test in a fresh process. The fixture credential is keyed off
/// HOME, and `refresh_host_credential` resolves `claude` from PATH; a child
/// process makes both substitutions hermetic even when the parent test binary
/// runs tests in parallel.
fn run_fixture_child(test_name: &str, refresh_mode: &str) {
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fresh = home.path().join("fresh.json");
    std::fs::write(
        &fresh,
        payload(
            "fixture-access-token-refreshed",
            SystemTime::now() + Duration::from_secs(7200),
        ),
    )
    .unwrap();
    let script = bin.join("claude");
    std::fs::write(
        &script,
        "#!/bin/sh\nif [ \"$AWMAN_0107_REFRESH_MODE\" = fail ]; then exit 17; fi\ncp \"$AWMAN_0107_FRESH_SOURCE\" \"$AWMAN_0107_HOST_CREDENTIALS\"\nprintf refresh >> \"$AWMAN_0107_REFRESH_LOG\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let old_path = std::env::var("PATH").unwrap_or_default();
    if test_name.contains("spawn_choke") || test_name.contains("workflow_auth") {
        for runtime in ["docker", "container"] {
            let runtime_script = bin.join(runtime);
            std::fs::write(
                &runtime_script,
                if test_name.contains("workflow_auth") {
                    "#!/bin/sh\nprintf '%s\\n' \"$0 $*\" >> \"$AWMAN_0107_SPAWN_LOG\"\nprintf '%s\\n' \"$AWMAN_0107_AGENT_OUTPUT\" >&2\nsleep 0.1\nexit 1\n"
                } else {
                    "#!/bin/sh\nprintf '%s\\n' \"$0 $*\" >> \"$AWMAN_0107_SPAWN_LOG\"\nprintf ready\nsleep 0.15\n"
                },
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&runtime_script, std::fs::Permissions::from_mode(0o700))
                    .unwrap();
            }
        }
    }
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env("AWMAN_0107_MONITOR_CHILD", "1")
        .env("AWMAN_0107_REFRESH_MODE", refresh_mode)
        .env("AWMAN_0107_FRESH_SOURCE", &fresh)
        .env("AWMAN_0107_HOST_CREDENTIALS", host_file(home.path()))
        .env("AWMAN_0107_REFRESH_LOG", home.path().join("refresh.log"))
        .env("AWMAN_0107_SPAWN_LOG", home.path().join("spawn.log"))
        .env(
            "AWMAN_0107_AGENT_OUTPUT",
            if test_name.contains("non_auth") {
                "ordinary compiler error"
            } else {
                "HTTP 401 Unauthorized"
            },
        )
        .env("HOME", home.path())
        .env("PATH", format!("{}:{old_path}", bin.display()))
        .status()
        .expect("spawn isolated monitor test process");
    assert!(
        status.success(),
        "isolated monitor test failed: {test_name}"
    );
}

struct TestFrontend {
    pty: bool,
}

impl UserMessageSink for TestFrontend {
    fn write_message(&mut self, _msg: UserMessage) {}
    fn replay_queued(&mut self) {}
}

impl AgentFrontend for TestFrontend {
    fn report_status(&mut self, _status: AgentStatus) {}
    fn report_progress(&mut self, _progress: AgentProgress) {}

    fn take_io(&mut self) -> AgentIo {
        let (stdout, _stdout_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stderr, _stderr_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel();
        AgentIo {
            stdout,
            stderr,
            stdin_tx,
            stdin_rx,
            resize: self.pty.then_some(resize_rx),
            initial_size: self.pty.then_some((80, 24)),
        }
    }

    fn grace_timeout(&self) -> Duration {
        Duration::from_secs(5)
    }
    fn stuck_timeout(&self) -> Duration {
        Duration::from_secs(5)
    }
}

struct RetryFactory {
    launches: Arc<AtomicUsize>,
    refreshes: Arc<AtomicUsize>,
}

impl AgentExecutionFactory for RetryFactory {
    fn execution_for_step(
        &self,
        _step: &awman::data::workflow_definition::WorkflowStep,
        _session: &Session,
        _runtime: &WorkflowRuntimeContext,
    ) -> Result<awman::engine::agent_runtime::AgentExecution, EngineError> {
        let n = self.launches.fetch_add(1, Ordering::SeqCst);
        let options = ResolvedContainerOptions::resolve([
            ContainerOption::Image(ImageRef::new("fixture:image")),
            ContainerOption::Entrypoint(Entrypoint::new(["fixture-agent"])),
            ContainerOption::Name(ContainerName::new(format!("awman-auth-retry-{n}"))),
        ])
        .unwrap();
        ContainerRuntime::docker()
            .build(options)?
            .run_with_frontend(Box::new(TestFrontend { pty: false }))
    }

    fn inject_prompt(
        &self,
        _execution: &awman::engine::agent_runtime::AgentExecution,
        _prompt: &str,
    ) -> Result<Option<()>, EngineError> {
        Ok(None)
    }

    fn recover_auth_failure(
        &self,
        _agent: &AgentName,
        output_tail: &str,
    ) -> Result<bool, EngineError> {
        if output_tail.contains("401") {
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

struct AbortWorkflowFrontend;
impl UserMessageSink for AbortWorkflowFrontend {
    fn write_message(&mut self, _msg: UserMessage) {}
    fn replay_queued(&mut self) {}
}
impl WorkflowFrontend for AbortWorkflowFrontend {
    fn show_workflow_control_board(
        &mut self,
        _state: &awman::data::workflow_state::WorkflowState,
        _available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        Ok(NextAction::LaunchNext)
    }
    fn yolo_countdown_tick(
        &mut self,
        _step_name: &str,
        _remaining: Duration,
        _total: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        Ok(YoloTickOutcome::Cancel)
    }
    fn report_step_status(
        &mut self,
        _step: &awman::data::workflow_definition::WorkflowStep,
        _status: WorkflowStepStatus,
    ) {
    }
    fn report_workflow_completed(&mut self, _outcome: &WorkflowOutcome) {}
    fn confirm_resume(&mut self, _mismatch: &ResumeMismatch) -> Result<bool, EngineError> {
        Ok(true)
    }
    fn user_choose_after_step_failure(
        &mut self,
        _step: &awman::data::workflow_definition::WorkflowStep,
        _exit: &awman::engine::agent_runtime::AgentExitInfo,
    ) -> Result<StepFailureChoice, EngineError> {
        Ok(StepFailureChoice::Abort)
    }
}

fn retry_workflow() -> Workflow {
    Workflow {
        title: Some("auth-retry".into()),
        steps: vec![awman::data::workflow_definition::WorkflowStep {
            name: "auth-step".into(),
            depends_on: vec![],
            prompt_template: "fixture".into(),
            agent: None,
            model: None,
            overlays: None,
            abort_on_failure: false,
        }],
        agent: Some("claude".into()),
        model: None,
        setup: vec![],
        teardown: vec![],
        teardown_on_failure: false,
        overlays: None,
    }
}

fn run_retry_workflow() -> (usize, usize) {
    let root = tempfile::tempdir().unwrap();
    let resolver = StaticGitRootResolver::new(root.path());
    let session = Session::open(
        root.path().to_path_buf(),
        &resolver,
        SessionOpenOptions::default(),
    )
    .unwrap();
    let launches = Arc::new(AtomicUsize::new(0));
    let refreshes = Arc::new(AtomicUsize::new(0));
    let factory = RetryFactory {
        launches: launches.clone(),
        refreshes: refreshes.clone(),
    };
    let overlay = OverlayEngine::with_auth_resolver(AuthPathResolver::at_home(root.path()));
    let mut engine = WorkflowEngine::new(
        &session,
        retry_workflow(),
        None,
        Box::new(AbortWorkflowFrontend),
        Box::new(factory),
        Arc::new(GitEngine::new()),
        Arc::new(overlay),
    )
    .unwrap();
    let outcome = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(engine.run_to_completion())
        .unwrap();
    assert!(matches!(outcome, WorkflowOutcome::Aborted));
    (
        launches.load(Ordering::SeqCst),
        refreshes.load(Ordering::SeqCst),
    )
}

#[test]
fn integration_monitor_rewrites_only_live_leases_and_skips_dropped() {
    if !child_env() {
        run_fixture_child(
            "credential_refresh_integration::integration_monitor_rewrites_only_live_leases_and_skips_dropped",
            "success",
        );
        return;
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(
        host_file(&home),
        payload(
            "fixture-access-token-initial",
            SystemTime::now() + Duration::from_secs(7200),
        ),
    )
    .unwrap();
    let live = tempfile::tempdir().unwrap();
    let dropped = tempfile::tempdir().unwrap();
    let live_delivery = materialized_delivery(&home, live.path());
    let dropped_delivery = materialized_delivery(&home, dropped.path());
    let before_live = std::fs::read(&live_delivery.staged_path).unwrap();
    let before_dropped = std::fs::read(&dropped_delivery.staged_path).unwrap();
    let monitor = monitor();
    let live_lease = monitor.register(&live_delivery, "awman-live");
    let dropped_lease = monitor.register(&dropped_delivery, "awman-dropped");
    drop(dropped_lease);
    std::fs::write(
        host_file(&home),
        payload(
            "fixture-access-token-rotated",
            SystemTime::now() + Duration::from_secs(7200),
        ),
    )
    .unwrap();

    let _outcome = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(monitor.refresh_now(&AgentName::new("claude").unwrap(), Duration::from_secs(2)));
    // The monitor thread is intentionally live too, so it may have won the
    // tick race. The observable contract is that exactly the still-live file
    // changed; the dropped lease's staged file must remain byte-identical.
    assert_ne!(
        std::fs::read(&live_delivery.staged_path).unwrap(),
        before_live
    );
    assert_eq!(
        std::fs::read(&dropped_delivery.staged_path).unwrap(),
        before_dropped
    );
    drop(live_lease);
}

#[test]
fn integration_monitor_refreshes_near_expiry_and_advances_expiry() {
    if !child_env() {
        run_fixture_child(
            "credential_refresh_integration::integration_monitor_refreshes_near_expiry_and_advances_expiry",
            "success",
        );
        return;
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    let initial_expiry = SystemTime::now() + Duration::from_secs(10);
    std::fs::write(
        host_file(&home),
        payload("fixture-access-token-expiring", initial_expiry),
    )
    .unwrap();
    let staged = tempfile::tempdir().unwrap();
    let delivery = materialized_delivery(&home, staged.path());
    let monitor = monitor();
    let lease = monitor.register(&delivery, "awman-expiring");
    let _outcome = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(monitor.refresh_now(&AgentName::new("claude").unwrap(), Duration::from_secs(2)));
    let refreshed = std::fs::read_to_string(&delivery.staged_path).unwrap();
    assert!(refreshed.contains("fixture-access-token-refreshed"));
    let status = monitor
        .status()
        .into_iter()
        .find(|s| s.agent.as_str() == "claude")
        .unwrap();
    assert!(
        status.expires_at.unwrap() > initial_expiry,
        "expiry must strictly advance"
    );
    assert_eq!(
        std::fs::read_to_string(std::env::var("AWMAN_0107_REFRESH_LOG").unwrap()).unwrap(),
        "refresh"
    );
    drop(lease);
}

#[test]
fn integration_monitor_failed_host_refresh_warns_and_keeps_last_known_good() {
    if !child_env() {
        run_fixture_child(
            "credential_refresh_integration::integration_monitor_failed_host_refresh_warns_and_keeps_last_known_good",
            "fail",
        );
        return;
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(
        host_file(&home),
        payload(
            "fixture-access-token-last-known-good",
            SystemTime::now() + Duration::from_secs(10),
        ),
    )
    .unwrap();
    let staged = tempfile::tempdir().unwrap();
    let delivery = materialized_delivery(&home, staged.path());
    let before = std::fs::read(&delivery.staged_path).unwrap();
    let monitor = monitor();
    let lease = monitor.register(&delivery, "awman-stale");
    let outcome = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(monitor.refresh_now(&AgentName::new("claude").unwrap(), Duration::from_secs(2)));
    assert!(
        matches!(outcome, RefreshOutcome::Stale { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        std::fs::read(&delivery.staged_path).unwrap(),
        before,
        "failed refresh must retain the complete last-known-good file"
    );
    let status = monitor
        .status()
        .into_iter()
        .find(|s| s.agent.as_str() == "claude")
        .unwrap();
    assert!(
        matches!(status.last_outcome, Some(RefreshOutcome::Stale { .. })),
        "failure must be surfaced in monitor status"
    );
    drop(lease);
}

/// Covers the real container-backend choke point without a Docker/Apple
/// runtime. The test substitutes tiny `docker` and `container` executables,
/// then drives the PTY, ordinary-piped, and ACP-piped paths of each backend.
/// In every case `build()` has registered the lease before `run_with_frontend`
/// can execute the fake runtime, and the execution drop releases it again.
#[test]
fn integration_spawn_choke_holds_lease_before_pty_piped_and_acp_on_docker_and_apple() {
    if !child_env() {
        run_fixture_child(
            "credential_refresh_integration::integration_spawn_choke_holds_lease_before_pty_piped_and_acp_on_docker_and_apple",
            "success",
        );
        return;
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(
        host_file(&home),
        payload(
            "fixture-choke-token",
            SystemTime::now() + Duration::from_secs(7200),
        ),
    )
    .unwrap();
    let staged = tempfile::tempdir().unwrap();
    let delivery = materialized_delivery(&home, staged.path());
    let monitor = monitor();
    install_global(monitor.clone());

    tokio::runtime::Runtime::new().unwrap().block_on(async {
        for (runtime, label) in [
            (ContainerRuntime::docker(), "docker"),
            (ContainerRuntime::apple(), "apple"),
        ] {
            for (mode, pty, acp) in [
                ("pty", true, false),
                ("piped", false, false),
                ("acp", false, true),
            ] {
                let options = ResolvedContainerOptions::resolve([
                    ContainerOption::Image(ImageRef::new("fixture:image")),
                    ContainerOption::Entrypoint(Entrypoint::new(["fixture-agent"])),
                    ContainerOption::Name(ContainerName::new(format!("awman-{label}-{mode}"))),
                    ContainerOption::Acp(acp),
                    ContainerOption::RefreshableCredential(delivery.clone()),
                ])
                .unwrap();
                let instance = runtime
                    .build(options)
                    .expect("credentialed build must succeed");
                assert_eq!(
                    monitor.live_lease_count(),
                    1,
                    "{label}/{mode}: build must register before spawn"
                );
                let mut execution = instance
                    .run_with_frontend(Box::new(TestFrontend { pty }))
                    .unwrap_or_else(|e| panic!("{label}/{mode}: fake runtime spawn failed: {e}"));
                assert_eq!(
                    monitor.live_lease_count(),
                    1,
                    "{label}/{mode}: execution must own the lease while child is live"
                );
                execution.wait().await.unwrap();
                assert_eq!(
                    monitor.live_lease_count(),
                    0,
                    "{label}/{mode}: child exit must release the lease"
                );
            }
        }
    });
    let calls = std::fs::read_to_string(std::env::var("AWMAN_0107_SPAWN_LOG").unwrap()).unwrap();
    assert_eq!(
        calls.lines().count(),
        6,
        "all six backend spawn paths must execute exactly once: {calls}"
    );
}

#[test]
fn docker_e2e_live_container_observes_rotated_fingerprint_and_exited_stage_is_untouched() {
    if !child_env() {
        if !crate::helpers::docker_available()
            || !Command::new("docker")
                .args(["pull", "busybox:latest"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        {
            eprintln!("SKIP: Docker/busybox unavailable");
            return;
        }
        run_fixture_child(
            "credential_refresh_integration::docker_e2e_live_container_observes_rotated_fingerprint_and_exited_stage_is_untouched",
            "success",
        );
        return;
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    // Start FAR from expiry. `register` starts the monitor thread, and
    // `run_monitor_loop` ticks before it sleeps, so the first tick lands
    // immediately no matter how long `tick_interval` is. Planting a
    // near-expiry credential here would let that first tick perform the whole
    // rotation before the container below is even running: the explicit
    // `refresh_now` would then find every staged file already current and
    // report `NotNeeded`, and the container would only ever observe one
    // fingerprint. Arm the rotation further down, once the container is live.
    std::fs::write(
        host_file(&home),
        payload(
            "fixture-e2e-initial",
            SystemTime::now() + Duration::from_secs(7200),
        ),
    )
    .unwrap();
    let live = tempfile::tempdir().unwrap();
    let exited = tempfile::tempdir().unwrap();
    let live_delivery = materialized_delivery(&home, live.path());
    let exited_delivery = materialized_delivery(&home, exited.path());
    let exited_before = std::fs::read(&exited_delivery.staged_path).unwrap();
    let monitor = monitor();
    let live_lease = monitor.register(&live_delivery, "awman-e2e-live");
    let exited_lease = monitor.register(&exited_delivery, "awman-e2e-exited");
    // Barrier on that first tick rather than assuming it is quick: it must be
    // done reconciling the far-expiry credential before the rotation is armed,
    // or it could be the one that rotates. A recorded outcome is the signal
    // that `refresh_agent` ran to completion.
    let first_tick = std::time::Instant::now();
    while !monitor
        .status()
        .iter()
        .any(|s| s.agent.as_str() == "claude" && s.last_outcome.is_some())
    {
        assert!(
            first_tick.elapsed() < Duration::from_secs(30),
            "the monitor's first tick never recorded an outcome"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let exited_mount = format!("{}:/root/.claude:ro", exited.path().display());
    assert!(Command::new("docker")
        .args(["run", "--rm", "-v", &exited_mount, "busybox:latest", "true"])
        .status()
        .unwrap()
        .success());
    drop(exited_lease);

    let fake_agent_dir = tempfile::tempdir().unwrap();
    let fake_agent = fake_agent_dir.path().join("fake-agent");
    std::fs::write(
        &fake_agent,
        "#!/bin/sh\nwhile :; do sha256sum \"$1\"; sleep 0.1; done\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_agent, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let live_mount = format!("{}:/root/.claude:ro", live.path().display());
    let agent_mount = format!("{}:/agent:ro", fake_agent_dir.path().display());
    let mut child = Command::new("docker")
        .args([
            "run",
            "--rm",
            "-v",
            &live_mount,
            "-v",
            &agent_mount,
            "busybox:latest",
            "sh",
            "/agent/fake-agent",
            "/root/.claude/.credentials.json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(250));
    // The container has now read the pre-rotation staged file at least once.
    // Arm the rotation: a near-expiry host credential is what drives
    // `refresh_agent` to run the fixture `claude` binary, which copies the
    // refreshed token over the host file. The next tick is 60s away, so this
    // `refresh_now` is unambiguously the call that performs it.
    std::fs::write(
        host_file(&home),
        payload(
            "fixture-e2e-expiring",
            SystemTime::now() + Duration::from_secs(10),
        ),
    )
    .unwrap();
    let outcome = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(monitor.refresh_now(&AgentName::new("claude").unwrap(), Duration::from_secs(2)));
    assert!(
        matches!(
            outcome,
            RefreshOutcome::Refreshed {
                leases_written: 1,
                ..
            }
        ),
        "{outcome:?}"
    );
    std::thread::sleep(Duration::from_millis(350));
    let _ = child.kill();
    let output = child.wait_with_output().unwrap();
    let observed = String::from_utf8_lossy(&output.stdout);
    let fingerprints: std::collections::BTreeSet<_> = observed
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    assert!(fingerprints.len() >= 2, "running fake agent must observe old and new staged-file fingerprints without restart: {fingerprints:?}");
    assert_eq!(
        std::fs::read(&exited_delivery.staged_path).unwrap(),
        exited_before,
        "exited container's dropped lease must never be rewritten"
    );
    drop(live_lease);
}

#[test]
fn integration_workflow_auth_401_refreshes_and_retries_exactly_once() {
    if !child_env() {
        run_fixture_child(
            "credential_refresh_integration::integration_workflow_auth_401_refreshes_and_retries_exactly_once",
            "success",
        );
        return;
    }
    let (launches, refreshes) = run_retry_workflow();
    assert_eq!(refreshes, 1, "a 401 may authorize one host refresh only");
    assert_eq!(launches, 2, "a 401 must receive exactly one retry");
}

#[test]
fn integration_workflow_auth_non_auth_failure_does_not_refresh_or_retry() {
    if !child_env() {
        run_fixture_child(
            "credential_refresh_integration::integration_workflow_auth_non_auth_failure_does_not_refresh_or_retry",
            "success",
        );
        return;
    }
    let (launches, refreshes) = run_retry_workflow();
    assert_eq!(
        refreshes, 0,
        "non-auth failures must not refresh credentials"
    );
    assert_eq!(
        launches, 1,
        "non-auth failures must not retry automatically"
    );
}
