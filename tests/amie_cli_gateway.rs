//! WI 0102 — CLI-side gateway, frontend-parity, and live-daemon tests.
//!
//! The unit/integration half injects a recording gateway into `Dispatch`, so
//! it can prove the CLI and TUI both construct the same Layer-2 amie command
//! and that the frontend itself never opens a condition store. The subprocess
//! half drives the real CLI against a live amie daemon and checks its JSON
//! envelopes against the daemon's wire responses.

use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;

use awman::command::commands::amie::commands::AmieServeConfig;
use awman::command::commands::amie::gateway::{ConditionGateway, CreateCondition, DaemonStatus};
use awman::command::dispatch::catalogue::CommandCatalogue;
use awman::command::dispatch::parsed_input::parse as parse_command_box;
use awman::command::dispatch::{BuiltCommand, Dispatch, Engines};
use awman::command::error::CommandError;
use awman::command::CommandOutcome;
use awman::data::config::env::Env;
use awman::data::fs::condition_store::{Condition, ConditionStatus, MountScope, Run, RunStatus};
use awman::data::fs::daemon_guard::{DaemonGuard, DaemonKind};
use awman::data::fs::{AmiePaths, ApiPaths, AuthPathResolver};
use awman::data::session::Session;
use awman::data::EngineWorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::agent_runtime::frontend::AgentIo;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::frontend::cli::{command_path_from_matches, CliFrontend};
use awman::frontend::tui::command_frontend::TuiCommandFrontend;
use awman::frontend::tui::tabs::{
    SharedActiveWorktreePath, SharedContainerExitCode, SharedContainerName,
    SharedContainerSlotEvents, SharedEngineTx, SharedPtyResetFlag, SharedResizeTx,
    SharedStatusDashboard, SharedStdinTx, SharedStuckSender, SharedTuiContext,
    SharedWorkflowViewState, SharedYoloCancelFlag, SharedYoloState,
};
use awman::frontend::tui::user_message::SharedStatusLog;

#[path = "helpers/mod.rs"]
mod helpers;

static ENV_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

// ─── Recording gateway ───────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct RecordingGateway {
    calls: Arc<Mutex<Vec<String>>>,
}

impl RecordingGateway {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("recording gateway mutex").clone()
    }

    fn push(&self, call: impl Into<String>) {
        self.calls
            .lock()
            .expect("recording gateway mutex")
            .push(call.into());
    }
}

fn condition(name: &str) -> Condition {
    let now = chrono::Utc::now();
    Condition {
        id: format!("condition-{name}"),
        name: name.to_string(),
        description: "recorded condition".into(),
        repo_scope: Path::new("/recorded/repo").to_path_buf(),
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

#[async_trait]
impl ConditionGateway for RecordingGateway {
    async fn create(&self, request: CreateCondition) -> Result<Condition, CommandError> {
        self.push(format!("create:{}", request.name));
        Ok(condition(&request.name))
    }

    async fn list(&self) -> Result<Vec<Condition>, CommandError> {
        self.push("list");
        Ok(vec![condition("recorded")])
    }

    async fn get(&self, name: &str) -> Result<Condition, CommandError> {
        self.push(format!("get:{name}"));
        Ok(condition(name))
    }

    async fn runs(&self, name: &str, limit: usize) -> Result<Vec<Run>, CommandError> {
        self.push(format!("runs:{name}:{limit}"));
        Ok(vec![Run {
            id: "run-recorded".into(),
            condition_id: format!("condition-{name}"),
            status: RunStatus::NotTriggered,
            workflow_path: None,
            workflow_state_path: None,
            session_id: None,
            started_at: chrono::Utc::now(),
            finished_at: None,
            error: None,
        }])
    }

    async fn set_status(&self, name: &str, status: ConditionStatus) -> Result<(), CommandError> {
        self.push(format!(
            "set_status:{name}:{}",
            match status {
                ConditionStatus::Active => "active",
                ConditionStatus::Paused => "paused",
            }
        ));
        Ok(())
    }

    async fn delete(&self, name: &str) -> Result<(), CommandError> {
        self.push(format!("delete:{name}"));
        Ok(())
    }

    async fn status(&self) -> Result<DaemonStatus, CommandError> {
        self.push("status");
        Ok(DaemonStatus {
            running: true,
            pid: Some(42),
            bound_addr: Some("http://127.0.0.1:1234".into()),
            condition_count: 1,
            active_count: 1,
            last_tick: None,
            in_flight: 0,
        })
    }
}

fn engines_at(root: &Path) -> Engines {
    let api_paths = ApiPaths::from_root(root);
    let auth_paths = AuthPathResolver::at_home(root);
    let runtime = Arc::new(ContainerRuntime::docker());
    let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    Engines {
        runtime: runtime.clone(),
        container_runtime: Some(runtime.clone()),
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::new()),
        overlay_engine: overlay_engine.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, api_paths.clone())),
        agent_engine: Arc::new(AgentEngine::new(overlay_engine, runtime)),
        workflow_state_store: Arc::new(EngineWorkflowStateStore::at_git_root(api_paths.root())),
    }
}

fn cli_matches(raw: &str) -> clap::ArgMatches {
    let mut argv = vec!["awman".to_string()];
    argv.extend(shell_words::split(raw).expect("test argv must tokenize"));
    CommandCatalogue::get()
        .build_clap_command()
        .try_get_matches_from(argv)
        .unwrap_or_else(|error| panic!("CLI test argv rejected: {raw}: {error}"))
}

fn tui_frontend(raw: &str) -> TuiCommandFrontend {
    let parsed = parse_command_box(raw, CommandCatalogue::get()).expect("TUI test argv must parse");
    let (dialog_tx, _dialog_request_rx) = std::sync::mpsc::channel();
    let (_dialog_response_tx, dialog_rx) = std::sync::mpsc::channel();
    let (stdout_tx, _stdout_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stderr_tx, _stderr_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();

    TuiCommandFrontend::new(
        parsed,
        Arc::new(Mutex::new(Vec::new())) as SharedStatusLog,
        dialog_tx,
        dialog_rx,
        AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        },
        Arc::new(Mutex::new(None)) as SharedWorkflowViewState,
        Arc::new(Mutex::new(None)) as SharedYoloState,
        Arc::new(std::sync::atomic::AtomicBool::new(false)) as SharedYoloCancelFlag,
        Arc::new(std::sync::atomic::AtomicBool::new(false)) as SharedPtyResetFlag,
        Arc::new(Mutex::new(None)) as SharedContainerName,
        Arc::new(Mutex::new(None)) as SharedContainerExitCode,
        Arc::new(Mutex::new(None)) as SharedStdinTx,
        Arc::new(Mutex::new(None)) as SharedResizeTx,
        Arc::new(Mutex::new(None)) as SharedEngineTx,
        Arc::new(Mutex::new(None)) as SharedStuckSender,
        Arc::new(Mutex::new(None)) as SharedActiveWorktreePath,
        Arc::new(Mutex::new(None)) as SharedStatusDashboard,
        Arc::new(Mutex::new(Default::default())) as SharedTuiContext,
        Arc::new(Mutex::new(std::collections::VecDeque::new())) as SharedContainerSlotEvents,
    )
}

async fn run_cli(
    raw: &str,
    session: &Session,
    engines: Engines,
    gateway: Arc<RecordingGateway>,
) -> Result<CommandOutcome, CommandError> {
    let matches = cli_matches(raw);
    let path = command_path_from_matches(&matches);
    let path_refs: Vec<&str> = path.iter().map(String::as_str).collect();
    let frontend = CliFrontend::new(matches);
    let dispatch = Dispatch::new(
        frontend,
        Arc::new(tokio::sync::RwLock::new(session.clone())),
        engines,
    )
    .with_amie_gateway(gateway);
    let built = dispatch
        .build_command(&path_refs)
        .unwrap_or_else(|error| panic!("CLI failed to build {raw}: {error}"));
    assert!(
        matches!(built, BuiltCommand::Amie(_)),
        "{raw} must resolve to AmieCommand"
    );
    dispatch.run_command(&path_refs).await
}

async fn run_tui(
    raw: &str,
    session: &Session,
    engines: Engines,
    gateway: Arc<RecordingGateway>,
) -> Result<CommandOutcome, CommandError> {
    let parsed = parse_command_box(raw, CommandCatalogue::get()).expect("TUI argv must parse");
    let path = parsed.path.clone();
    let path_refs: Vec<&str> = path.iter().map(String::as_str).collect();
    let frontend = tui_frontend(raw);
    let dispatch = Dispatch::new(
        frontend,
        Arc::new(tokio::sync::RwLock::new(session.clone())),
        engines,
    )
    .with_amie_gateway(gateway);
    let built = dispatch
        .build_command(&path_refs)
        .unwrap_or_else(|error| panic!("TUI failed to build {raw}: {error}"));
    assert!(
        matches!(built, BuiltCommand::Amie(_)),
        "{raw} must resolve to AmieCommand"
    );
    dispatch.run_command(&path_refs).await
}

// ─── Dispatch / frontend integration ────────────────────────────────────────

#[tokio::test]
async fn cli_crud_uses_exactly_one_gateway_call_and_no_frontend_validation() {
    let env = helpers::IsolatedEnv::new();
    let session = env.open_session();
    let cases = [
        (
            "amie add --name 'not a slug' --description desc --repo /path/that/does/not/exist --interval 60",
            vec!["create:not a slug".to_string()],
        ),
        ("amie list", vec!["list".to_string()]),
        // `show` is the contract-pinned exception: Layer 2 combines the
        // condition and its history with one `get` plus one `runs` call.
        (
            "amie show recorded",
            vec!["get:recorded".to_string(), "runs:recorded:20".to_string()],
        ),
        ("amie remove recorded", vec!["delete:recorded".to_string()]),
        (
            "amie pause recorded",
            vec!["set_status:recorded:paused".to_string()],
        ),
        (
            "amie resume recorded",
            vec!["set_status:recorded:active".to_string()],
        ),
    ];

    for (raw, expected) in cases {
        let gateway = Arc::new(RecordingGateway::default());
        run_cli(
            raw,
            &session,
            engines_at(env.home_dir.path()),
            gateway.clone(),
        )
        .await
        .unwrap_or_else(|error| panic!("{raw} must reach its gateway: {error}"));
        assert_eq!(gateway.calls(), expected, "gateway sequence for {raw}");
    }
}

#[tokio::test]
async fn cli_and_tui_same_argv_build_amie_and_issue_the_same_gateway_sequence() {
    let env = helpers::IsolatedEnv::new();
    let session = env.open_session();
    for raw in [
        "amie add --name parity --description desc --repo /not/validated --interval 60",
        "amie list",
        "amie show parity",
        "amie remove parity",
        "amie pause parity",
        "amie resume parity",
    ] {
        let cli_gateway = Arc::new(RecordingGateway::default());
        run_cli(
            raw,
            &session,
            engines_at(env.home_dir.path()),
            cli_gateway.clone(),
        )
        .await
        .unwrap_or_else(|error| panic!("CLI {raw} failed: {error}"));

        let tui_gateway = Arc::new(RecordingGateway::default());
        run_tui(
            raw,
            &session,
            engines_at(env.home_dir.path()),
            tui_gateway.clone(),
        )
        .await
        .unwrap_or_else(|error| panic!("TUI {raw} failed: {error}"));

        assert_eq!(
            cli_gateway.calls(),
            tui_gateway.calls(),
            "Layer-2 call sequence for {raw}"
        );
    }
}

#[tokio::test]
async fn cli_dispatch_succeeds_with_an_empty_home_and_never_reads_awman_db() {
    let _env_guard = ENV_LOCK.lock().await;
    let home = tempfile::tempdir().expect("temporary home");
    let previous_home = std::env::var("HOME").ok();
    let previous_config = std::env::var("AWMAN_CONFIG_HOME").ok();
    let previous_amie = std::env::var("AWMAN_AMIE_ROOT").ok();
    std::env::set_var("HOME", home.path());
    std::env::set_var("AWMAN_CONFIG_HOME", home.path().join("config"));
    std::env::set_var("AWMAN_AMIE_ROOT", home.path().join("amie"));

    let env = helpers::IsolatedEnv::new();
    let session = env.open_session();
    let gateway = Arc::new(RecordingGateway::default());
    let result = run_cli(
        "amie list",
        &session,
        engines_at(env.home_dir.path()),
        gateway.clone(),
    )
    .await;

    match previous_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    match previous_config {
        Some(value) => std::env::set_var("AWMAN_CONFIG_HOME", value),
        None => std::env::remove_var("AWMAN_CONFIG_HOME"),
    }
    match previous_amie {
        Some(value) => std::env::set_var("AWMAN_AMIE_ROOT", value),
        None => std::env::remove_var("AWMAN_AMIE_ROOT"),
    }

    result.expect("CLI dispatch must use the injected gateway without a database");
    assert_eq!(gateway.calls(), vec!["list".to_string()]);
    assert!(
        !home.path().join(".awman/data/awman.db").exists(),
        "CLI must not create or read ~/.awman/data/awman.db"
    );
}

/// `amie status` overlays live scheduler counts from the injected gateway with
/// exactly one `status()` call (§9.4). A stopped daemon (no gateway injected)
/// falls back to the pidfile-only answer and makes no call — see `cli::run`.
#[tokio::test]
async fn cli_status_uses_exactly_one_gateway_call() {
    let env = helpers::IsolatedEnv::new();
    let session = env.open_session();
    let gateway = Arc::new(RecordingGateway::default());
    run_cli(
        "amie status --json",
        &session,
        engines_at(env.home_dir.path()),
        gateway.clone(),
    )
    .await
    .expect("status command must succeed");
    assert_eq!(gateway.calls(), vec!["status".to_string()]);
}

// ─── Live CLI / daemon JSON integration ─────────────────────────────────────

struct NeverTriggeredEvaluator;

#[async_trait]
impl awman::engine::amie::ConditionEvaluator for NeverTriggeredEvaluator {
    async fn evaluate(
        &self,
        _request: awman::engine::amie::EvaluationRequest,
    ) -> awman::engine::amie::EvaluationOutcome {
        awman::engine::amie::EvaluationOutcome::NotTriggered
    }
}

fn init_repo(path: &Path) {
    std::fs::create_dir_all(path).expect("repo directory");
    let status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(path)
        .status()
        .expect("git must run");
    assert!(status.success(), "git init must succeed");
}

#[cfg(unix)]
struct FakeAwmanProcess {
    _dir: tempfile::TempDir,
    child: std::process::Child,
}

#[cfg(unix)]
impl FakeAwmanProcess {
    fn spawn() -> Self {
        let dir = tempfile::tempdir().expect("fake awman process directory");
        let executable = dir.path().join("awman-amie-test-holder");
        std::fs::copy("/bin/sleep", &executable).expect("copy sleep holder");
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&executable)
            .expect("holder metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).expect("holder permissions");
        let child = Command::new(&executable)
            .arg("60")
            .spawn()
            .expect("awman-named holder must start");
        Self { _dir: dir, child }
    }

    fn id(&self) -> u32 {
        self.child.id()
    }
}

#[cfg(unix)]
impl Drop for FakeAwmanProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(unix)]
async fn start_daemon(root: &Path) -> (tokio::task::JoinHandle<()>, String, FakeAwmanProcess) {
    let engines = engines_at(root);
    let previous_config = std::env::var("AWMAN_CONFIG_HOME").ok();
    let previous_amie = std::env::var("AWMAN_AMIE_ROOT").ok();
    std::env::set_var("AWMAN_CONFIG_HOME", root);
    std::env::set_var("AWMAN_AMIE_ROOT", root.join("amie"));
    let daemon_guard =
        DaemonGuard::for_daemon(DaemonKind::Amie, &Env::from_process()).expect("test daemon guard");
    let holder = FakeAwmanProcess::spawn();
    daemon_guard
        .acquire(holder.id())
        .expect("test daemon guard must claim amie");

    let handle = tokio::spawn(async move {
        let _ = awman::frontend::amie::serve_with(
            AmieServeConfig {
                port: 0,
                dangerously_skip_auth: true,
            },
            engines,
            Arc::new(NeverTriggeredEvaluator),
        )
        .await;
        let _ = daemon_guard.release();
    });

    let daemon = awman::data::fs::daemon_process::DaemonProcess::new(
        AmiePaths::from_root(root.join("amie")).daemon(),
        awman::data::fs::daemon_process::AMIE_UNIT_NAME,
        awman::data::fs::daemon_process::AMIE_PLIST_LABEL,
    );
    let mut waited = Duration::ZERO;
    let meta = loop {
        if let Ok(Some(meta)) = daemon.read_meta() {
            break meta;
        }
        assert!(
            waited < Duration::from_secs(10),
            "amie daemon never published metadata"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        waited += Duration::from_millis(50);
    };

    match previous_config {
        Some(value) => std::env::set_var("AWMAN_CONFIG_HOME", value),
        None => std::env::remove_var("AWMAN_CONFIG_HOME"),
    }
    match previous_amie {
        Some(value) => std::env::set_var("AWMAN_AMIE_ROOT", value),
        None => std::env::remove_var("AWMAN_AMIE_ROOT"),
    }

    (
        handle,
        format!("{}://{}:{}", meta.scheme, meta.bind_ip, meta.port),
        holder,
    )
}

/// A stable, private copy of the built `awman` binary that the subprocess tests
/// exec instead of `CARGO_BIN_EXE_awman` (`target/debug/awman`) directly.
///
/// Cargo publishes that top-level path with an unlink-then-relink sequence, so a
/// spawn racing a concurrent (re)build can momentarily see ENOENT on a binary
/// that assuredly exists — the transient `Os { code: 2, NotFound }` that fails
/// these tests. Exec'ing a copy under our own tempdir removes the race entirely:
/// the shared source is read once per test-binary process (cached below), and
/// every subsequent launch is deterministic. No assertion on CLI behaviour
/// changes — it is still the freshly built binary that runs.
///
/// The `root` argument is retained for call-site compatibility; the copy itself
/// lives in a process-global location so the racy top-level path is touched at
/// most once, no matter how many tests run.
fn awman_under_test(_root: &Path) -> std::path::PathBuf {
    use std::sync::OnceLock;
    static SHARED: OnceLock<std::path::PathBuf> = OnceLock::new();
    SHARED.get_or_init(build_shared_awman_copy).clone()
}

/// Resolve the freshly built `awman` binary and copy it to a stable, private
/// path for the lifetime of this test process.
fn build_shared_awman_copy() -> std::path::PathBuf {
    // Persist for the whole process: `into_path` leaks the tempdir (fine for a
    // test binary — the OS reclaims it on exit) so the copy outlives any single
    // test's `tempfile::tempdir`.
    let dir = tempfile::Builder::new()
        .prefix("awman-under-test-")
        .tempdir()
        .expect("temp dir for awman copy")
        .keep();
    let dest = dir.join("awman");

    // The source (especially the top-level convenience path) can be unlinked
    // between resolution and the copy while cargo republishes it, so re-resolve
    // and retry a few times rather than panicking on a transient ENOENT.
    let mut last_err: Option<(std::path::PathBuf, std::io::Error)> = None;
    for attempt in 0..40 {
        let src = resolve_awman_source();
        match std::fs::copy(&src, &dest) {
            Ok(_) => {
                last_err = None;
                break;
            }
            Err(err) => {
                last_err = Some((src, err));
                std::thread::sleep(Duration::from_millis(25 * (attempt + 1)));
            }
        }
    }
    if let Some((src, err)) = last_err {
        panic!("copying awman binary from {}: {err:?}", src.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
            .expect("mark awman copy executable");
    }
    dest
}

/// Locate the freshly built `awman` binary.
///
/// The primary source is `CARGO_BIN_EXE_awman` (`target/debug/awman`). Because
/// cargo publishes that convenience path with an unlink-then-relink — and a
/// concurrent (re)build can widen that window well beyond a single relink — we
/// retry while it is momentarily absent. If it stays absent, we fall back to a
/// per-hash binary under `target/debug/deps/`.
///
/// The fallback must be careful: `deps/` also holds the bin crate's own *libtest
/// harness* executables, which are likewise named `awman-<hash>` but only accept
/// test-runner flags (running one with `amie add …` yields "Unrecognized
/// option"). We cannot tell them apart by hard-link count — in this build cargo
/// publishes the top-level path as a plain copy (link count 1), so *no* deps
/// binary is ever hard-linked to it — so we positively identify the real CLI by
/// probing each candidate with `--version` and keeping the newest that answers
/// like the CLI.
fn resolve_awman_source() -> std::path::PathBuf {
    let top_level = std::path::PathBuf::from(env!("CARGO_BIN_EXE_awman"));
    for attempt in 0..40 {
        if top_level.exists() {
            return top_level;
        }
        if let Some(from_deps) = newest_real_cli_deps_binary(&top_level) {
            return from_deps;
        }
        std::thread::sleep(Duration::from_millis(25 * (attempt + 1)));
    }
    // Last resort: the newest real-CLI deps binary, else fail loudly.
    newest_real_cli_deps_binary(&top_level).unwrap_or_else(|| {
        panic!(
            "awman binary never settled at {} and no real-CLI deps/awman-<hash> was found",
            top_level.display()
        )
    })
}

/// The newest `target/debug/deps/awman-<hash>` executable that probes as the real
/// `awman` CLI (not one of the bin crate's libtest harnesses of the same name).
#[cfg(unix)]
fn newest_real_cli_deps_binary(top_level: &Path) -> Option<std::path::PathBuf> {
    use std::time::SystemTime;

    let deps = top_level.parent()?.join("deps");
    // Collect candidates newest-first, tolerating transient per-entry errors from
    // a concurrent build (skip the entry rather than abandoning the whole scan).
    let mut candidates: Vec<(SystemTime, std::path::PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(&deps).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Match the bin (`awman-<hash>`), never the `.d` depfiles or unrelated
        // test binaries (`amie_cli_gateway-<hash>`, ...).
        if !name.starts_with("awman-") || name.contains('.') {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(meta) if meta.is_file() => meta,
            _ => continue,
        };
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        candidates.push((mtime, entry.path()));
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates
        .into_iter()
        .map(|(_, path)| path)
        .find(|path| probes_as_awman_cli(path))
}

#[cfg(not(unix))]
fn newest_real_cli_deps_binary(_top_level: &Path) -> Option<std::path::PathBuf> {
    None
}

/// True if `path` responds to `--version` like the real `awman` CLI. A libtest
/// harness rejects the flag (nonzero exit) and never prints an `awman <version>`
/// banner, so this cleanly excludes the same-named test executables in `deps/`.
fn probes_as_awman_cli(path: &Path) -> bool {
    match Command::new(path).arg("--version").output() {
        Ok(out) => out.status.success() && out.stdout.starts_with(b"awman "),
        Err(_) => false,
    }
}

fn run_binary(root: &Path, cwd: &Path, args: &[&str]) -> Output {
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("test home");
    Command::new(awman_under_test(root))
        .args(args)
        .current_dir(cwd)
        .env("HOME", &home)
        .env("AWMAN_CONFIG_HOME", root)
        .env("AWMAN_AMIE_ROOT", root.join("amie"))
        .output()
        .expect("awman subprocess must start")
}

fn stdout_json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "awman failed: status={:?} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "awman --json emitted invalid JSON: {error}; stdout={:?}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

async fn daemon_command(base: &str, subcommand: &str, args: &[&str]) -> Value {
    reqwest::Client::new()
        .post(format!("{base}/v1/commands"))
        .json(&serde_json::json!({ "subcommand": subcommand, "args": args }))
        .send()
        .await
        .expect("daemon command request")
        .json()
        .await
        .expect("daemon command JSON")
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_crud_round_trip_and_json_payloads_match_the_live_daemon() {
    if !helpers::git_available() {
        eprintln!("SKIP: git not available");
        return;
    }
    let _env_guard = ENV_LOCK.lock().await;
    let root = tempfile::tempdir().expect("temporary amie root");
    let repo = root.path().join("repo");
    init_repo(&repo);
    let (daemon, base, _holder) = start_daemon(root.path()).await;

    let repo_arg = repo.to_str().expect("repo path");
    let add = run_binary(
        root.path(),
        &repo,
        &[
            "amie",
            "add",
            "--name",
            "cli-roundtrip",
            "--description",
            "from cli",
            "--repo",
            repo_arg,
            "--interval",
            "60",
        ],
    );
    assert!(
        add.status.success(),
        "amie add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let list_json = stdout_json(&run_binary(root.path(), &repo, &["amie", "list", "--json"]));
    let list_payload = list_json
        .get("payload")
        .expect("CLI list JSON must contain a payload");
    assert_eq!(list_payload, &daemon_command(&base, "amie list", &[]).await);

    let show_json = stdout_json(&run_binary(
        root.path(),
        &repo,
        &["amie", "show", "cli-roundtrip", "--json"],
    ));
    let show_payload = show_json
        .get("payload")
        .expect("CLI show JSON must contain a payload");
    assert_eq!(
        show_payload,
        &daemon_command(&base, "amie show", &["cli-roundtrip"]).await
    );

    let pause = run_binary(root.path(), &repo, &["amie", "pause", "cli-roundtrip"]);
    assert!(pause.status.success(), "amie pause failed");
    let paused = stdout_json(&run_binary(
        root.path(),
        &repo,
        &["amie", "show", "cli-roundtrip", "--json"],
    ));
    assert_eq!(paused["payload"]["condition"]["status"], "paused");

    let resume = run_binary(root.path(), &repo, &["amie", "resume", "cli-roundtrip"]);
    assert!(resume.status.success(), "amie resume failed");
    let resumed = stdout_json(&run_binary(
        root.path(),
        &repo,
        &["amie", "show", "cli-roundtrip", "--json"],
    ));
    assert_eq!(resumed["payload"]["condition"]["status"], "active");

    let status_json = stdout_json(&run_binary(
        root.path(),
        &repo,
        &["amie", "status", "--json"],
    ));
    let status_payload = status_json
        .get("payload")
        .expect("CLI status JSON must contain a payload");
    let daemon_status = reqwest::get(format!("{base}/v1/status"))
        .await
        .expect("daemon status request")
        .json::<Value>()
        .await
        .expect("daemon status JSON");
    let mut cli_keys: Vec<_> = status_payload
        .as_object()
        .expect("CLI status payload must be an object")
        .keys()
        .collect();
    let mut daemon_keys: Vec<_> = daemon_status
        .as_object()
        .expect("daemon status response must be an object")
        .keys()
        .collect();
    cli_keys.sort_unstable();
    daemon_keys.sort_unstable();
    assert_eq!(
        cli_keys, daemon_keys,
        "CLI status JSON must preserve the daemon response shape"
    );

    let remove = run_binary(
        root.path(),
        &repo,
        &["amie", "remove", "cli-roundtrip", "--yes"],
    );
    assert!(remove.status.success(), "amie remove failed");
    let final_list = stdout_json(&run_binary(root.path(), &repo, &["amie", "list", "--json"]));
    assert_eq!(final_list["payload"], serde_json::json!([]));

    daemon.abort();
}

#[test]
fn json_cli_failure_is_a_structured_error_with_nonzero_exit() {
    if !helpers::git_available() {
        eprintln!("SKIP: git not available");
        return;
    }
    let root = tempfile::tempdir().expect("temporary root");
    let repo = root.path().join("repo");
    init_repo(&repo);
    let invalid_amie_root = root.path().join("amie-file");
    std::fs::write(&invalid_amie_root, "not a directory").expect("invalid amie root fixture");

    let home = root.path().join("home");
    std::fs::create_dir_all(&home).expect("test home");
    let output = Command::new(awman_under_test(root.path()))
        .args(["amie", "list", "--json"])
        .current_dir(&repo)
        .env("HOME", &home)
        .env("AWMAN_CONFIG_HOME", root.path().join("config"))
        .env("AWMAN_AMIE_ROOT", &invalid_amie_root)
        .output()
        .expect("awman subprocess must start");

    assert!(
        !output.status.success(),
        "unreachable daemon must return non-zero"
    );
    let body: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "JSON failure must be valid JSON: {error}; stdout={:?}, stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert!(body["error"]
        .as_str()
        .is_some_and(|message| !message.is_empty()));
}
