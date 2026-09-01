//! WI 0101 — E2E: the real `curl` executable drives the full task
//! lifecycle against a running squad daemon with **no `awman` frontend
//! involved** — proving the daemon is complete on its own, exactly the
//! exercise documented in the `daemon-frontend` handoff
//! (`part4-daemon-summary.md`'s curl walkthrough).
//!
//! Every request below is issued by the `curl` binary, not by an in-process
//! HTTP client, so none of awman's own client stack (`HttpCore`,
//! `RemoteTaskGateway`, the CLI) is on the path — only the daemon's
//! loopback socket is.
//!
//! Boots the real daemon via `frontend::squad::serve_with` (the same
//! injectable seam `tests/squad_daemon_http.rs` uses) and drives it purely
//! over HTTP: add → list → show → pause → resume → workflow (404, none
//! running yet) → remove.

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use awman::command::commands::squad::commands::SquadServeConfig;
use awman::command::dispatch::Engines;
use awman::data::fs::api_db::SqliteSessionStore;
use awman::data::fs::daemon_process::{DaemonProcess, SQUAD_PLIST_LABEL, SQUAD_UNIT_NAME};
use awman::data::fs::{ApiPaths, AuthPathResolver, SquadPaths};
use awman::data::EngineWorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::engine::squad::{EvaluationOutcome, EvaluationRequest, TaskEvaluator};
use serde_json::Value;
use tokio::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::const_new(());

struct NeverTriggeredEvaluator;
#[async_trait::async_trait]
impl TaskEvaluator for NeverTriggeredEvaluator {
    async fn evaluate(&self, _request: EvaluationRequest) -> EvaluationOutcome {
        EvaluationOutcome::NotTriggered
    }
}

fn tool_available(bin: &str, arg: &str) -> bool {
    Command::new(bin)
        .arg(arg)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn init_repo(path: &std::path::Path) {
    std::fs::create_dir_all(path).unwrap();
    let status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(path)
        .status()
        .unwrap();
    assert!(status.success(), "git init must succeed");
}

// ─── the curl driver ────────────────────────────────────────────────────────

/// Run one `curl` invocation and return `(status_code, parsed_json_body)`.
/// `-w '\n%{http_code}'` appends the status on its own final line, so the body
/// and the code come back from a single process without a temp file.
fn curl_blocking(args: &[String]) -> (u16, Value) {
    let output = Command::new("curl")
        .args(["-sS", "-w", "\n%{http_code}"])
        .args(args)
        .output()
        .expect("curl must be executable");
    assert!(
        output.status.success(),
        "curl exited {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let (body, code) = text
        .rsplit_once('\n')
        .expect("curl -w must append the status code on its own line");
    let status: u16 = code
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("unparsable curl status {code:?}"));
    let value = if body.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(body)
            .unwrap_or_else(|e| panic!("daemon returned non-JSON body {body:?}: {e}"))
    };
    (status, value)
}

/// `curl` blocks its thread, so every call is pushed onto the blocking pool
/// rather than parking a worker the daemon is also running on.
async fn curl(args: Vec<String>) -> (u16, Value) {
    tokio::task::spawn_blocking(move || curl_blocking(&args))
        .await
        .expect("curl task must not panic")
}

async fn curl_get(url: String) -> (u16, Value) {
    curl(vec![url]).await
}

async fn curl_command(base: &str, subcommand: &str, args: &[&str]) -> (u16, Value) {
    let payload = serde_json::json!({ "subcommand": subcommand, "args": args }).to_string();
    curl(vec![
        "-X".into(),
        "POST".into(),
        "-H".into(),
        "Content-Type: application/json".into(),
        "-d".into(),
        payload,
        format!("{base}/v1/commands"),
    ])
    .await
}

async fn start_daemon(root: &std::path::Path) -> (tokio::task::JoinHandle<()>, String) {
    let api_paths = ApiPaths::from_root(root);
    let auth_paths = AuthPathResolver::at_home(root);
    let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    let container_runtime = Arc::new(ContainerRuntime::docker());
    let engines = Engines {
        runtime: container_runtime.clone(),
        container_runtime: Some(container_runtime.clone()),
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::new()),
        overlay_engine: overlay_engine.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, api_paths.clone())),
        agent_engine: Arc::new(AgentEngine::new(overlay_engine, container_runtime)),
        workflow_state_store: Arc::new(EngineWorkflowStateStore::at_git_root(api_paths.root())),
    };
    let config = SquadServeConfig {
        port: 0,
        dangerously_skip_auth: true,
    };

    let previous = std::env::var("AWMAN_CONFIG_HOME").ok();
    std::env::set_var("AWMAN_CONFIG_HOME", root);

    let handle = tokio::spawn(async move {
        let _ =
            awman::frontend::squad::serve_with(config, engines, Arc::new(NeverTriggeredEvaluator))
                .await;
    });

    let daemon = DaemonProcess::new(
        SquadPaths::from_root(root.join("squad")).daemon(),
        SQUAD_UNIT_NAME,
        SQUAD_PLIST_LABEL,
    );
    let mut waited = Duration::ZERO;
    let meta = loop {
        if let Ok(Some(meta)) = daemon.read_meta() {
            break meta;
        }
        if waited >= Duration::from_secs(10) {
            panic!("squad daemon never published its server metadata");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn curl_drives_the_full_task_lifecycle_with_no_awman_frontend() {
    if !tool_available("git", "--version") {
        eprintln!("SKIP: git not available");
        return;
    }
    if !tool_available("curl", "--version") {
        eprintln!("SKIP: curl not available");
        return;
    }

    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let (handle, base) = start_daemon(tmp.path()).await;

    // status — daemon is up, no tasks yet.
    let (code, status) = curl_get(format!("{base}/v1/status")).await;
    assert_eq!(code, 200);
    assert_eq!(status["running"], true);
    assert_eq!(status["task_count"], 0);

    // add
    let repo_str = repo.to_str().unwrap();
    let (code, created) = curl_command(
        &base,
        "squad add",
        &[
            "--name",
            "issue-triage",
            "--description",
            "watch new issues",
            "--repo",
            repo_str,
            "--interval",
            "60",
            "--mount-scope",
            "gitroot",
        ],
    )
    .await;
    assert_eq!(code, 200, "squad add must succeed: {created}");
    assert_eq!(created["name"], "issue-triage");
    assert_eq!(created["status"], "active");

    // list
    let (code, list) = curl_command(&base, "squad list", &[]).await;
    assert_eq!(code, 200);
    assert_eq!(list.as_array().unwrap().len(), 1);

    // show — one response shape carrying the task *and* its run history,
    // which is what both gateway implementations deserialize.
    let (code, shown) = curl_command(&base, "squad show", &["issue-triage"]).await;
    assert_eq!(code, 200);
    assert_eq!(shown["task"]["name"], "issue-triage");
    assert_eq!(
        shown["runs"].as_array().map(Vec::len),
        Some(0),
        "a never-evaluated task has an empty (not absent) run history: {shown}"
    );

    // pause
    let (code, _) = curl_command(&base, "squad pause", &["issue-triage"]).await;
    assert_eq!(code, 200);
    let (_, shown) = curl_command(&base, "squad show", &["issue-triage"]).await;
    assert_eq!(shown["task"]["status"], "paused");

    // resume
    let (code, _) = curl_command(&base, "squad resume", &["issue-triage"]).await;
    assert_eq!(code, 200);
    let (_, shown) = curl_command(&base, "squad show", &["issue-triage"]).await;
    assert_eq!(shown["task"]["status"], "active");

    // workflow — 404 until a running evaluation persists a workflow state path.
    let (code, _) = curl_get(format!("{base}/v1/tasks/issue-triage/workflow")).await;
    assert_eq!(code, 404);

    // an unknown task is a 404, not a 500 or an empty 200.
    let (code, _) = curl_get(format!("{base}/v1/tasks/no-such-task/workflow")).await;
    assert_eq!(code, 404);

    // status reflects the one active task.
    let (_, status) = curl_get(format!("{base}/v1/status")).await;
    assert_eq!(status["task_count"], 1);
    assert_eq!(status["active_count"], 1);

    // remove
    let (code, _) = curl_command(&base, "squad remove", &["issue-triage"]).await;
    assert_eq!(code, 200);
    let (_, list) = curl_command(&base, "squad list", &[]).await;
    assert_eq!(list.as_array().unwrap().len(), 0);

    handle.abort();
}

/// WI 0101 — E2E: a **pre-migration install** (a legacy `<api_root>/awman.db`
/// holding real session and command rows) starts the squad daemon, gains the
/// squad tables, and retains every prior API-mode row.
///
/// This is the upgrade path an existing user actually takes: they have been
/// running `awman api`, they upgrade, and the first thing that touches the
/// database is the squad daemon. Nothing may be lost, and the legacy original
/// must survive under its `.pre-migration` name for `awman clean` to remove.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pre_migration_install_starts_squad_gains_its_tables_and_keeps_api_rows() {
    if !tool_available("git", "--version") || !tool_available("curl", "--version") {
        eprintln!("SKIP: git and curl are both required");
        return;
    }

    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);

    // ── the pre-upgrade world: an API-mode database at the legacy location ──
    let legacy_root = tmp.path().join("api");
    std::fs::create_dir_all(&legacy_root).unwrap();
    let legacy = SqliteSessionStore::open(&legacy_root).unwrap();
    legacy
        .insert_session(
            "pre-upgrade-session",
            "/pre/upgrade/repo",
            "2026-08-01T00:00:00Z",
        )
        .unwrap();
    legacy
        .insert_command(
            "pre-upgrade-command",
            "pre-upgrade-session",
            "status",
            "[\"--json\"]",
            "/pre/upgrade/output.log",
        )
        .unwrap();
    let expected_session = legacy.get_session("pre-upgrade-session").unwrap();
    let expected_command = legacy.get_command("pre-upgrade-command").unwrap();
    drop(legacy);
    assert!(legacy_root.join("awman.db").exists());

    // ── upgrade: the squad daemon is the first process to open the database ──
    let (handle, base) = start_daemon(tmp.path()).await;

    // The database moved, and the original is retained under its backup name
    // (which `awman clean` is the one thing allowed to remove).
    let data_db = tmp.path().join("data").join("awman.db");
    assert!(
        data_db.exists(),
        "the database must relocate to <data_home>/data"
    );
    assert!(
        !legacy_root.join("awman.db").exists(),
        "the legacy original must be renamed aside, not left in place"
    );
    assert!(
        legacy_root.join("awman.db.pre-migration").exists(),
        "the one-release safety net must be retained"
    );

    // ── the prior API-mode rows are intact at the new location ──
    let migrated = SqliteSessionStore::open_at(&data_db).unwrap();
    assert_eq!(
        migrated.get_session("pre-upgrade-session").unwrap(),
        expected_session,
        "an upgrade must not lose a pre-existing session row"
    );
    assert_eq!(
        migrated.get_command("pre-upgrade-command").unwrap(),
        expected_command,
        "an upgrade must not lose a pre-existing command row"
    );
    drop(migrated);

    // ── and the same file now carries squad's tables, driven over HTTP ──
    let (code, created) = curl_command(
        &base,
        "squad add",
        &[
            "--name",
            "post-upgrade",
            "--description",
            "created after the upgrade",
            "--repo",
            repo.to_str().unwrap(),
            "--interval",
            "300",
            "--mount-scope",
            "gitroot",
        ],
    )
    .await;
    assert_eq!(
        code, 200,
        "squad add must succeed on a migrated database: {created}"
    );
    let (_, list) = curl_command(&base, "squad list", &[]).await;
    assert_eq!(list.as_array().unwrap().len(), 1);

    // The squad rows landed in the shared database, not a second file.
    let conn = rusqlite::Connection::open(&data_db).unwrap();
    let tasks: i64 = conn
        .query_row("SELECT COUNT(*) FROM squad_tasks", [], |row| row.get(0))
        .expect("the shared database must carry squad_tasks after startup");
    assert_eq!(tasks, 1);
    let sessions: i64 = conn
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
        .expect("the shared database must still carry the API-mode tables");
    assert_eq!(
        sessions, 1,
        "both daemons' tables live in one database file"
    );

    handle.abort();
}
