//! WI-0097 regression-pinning suite: API frontend architecture-conformance.
//!
//! These tests capture the **current externally visible behavior** of the API
//! frontend (`src/frontend/api/`) BEFORE the Layer 2/Layer 0 refactor begins.
//! They form the backward-compatibility contract the refactor must preserve.
//!
//! Contract rules for downstream steps:
//!   * **Status-code assertions are the hard contract** — they MUST survive the
//!     refactor unchanged.
//!   * **Error-text assertions are soft** — every line that inspects an error
//!     string is marked `TEXT-ASSERTION (relaxable)`. Later steps may loosen or
//!     change the wording (e.g. when validation moves into Layer 2), but must
//!     NOT change the accompanying status code.
//!
//! Covered surfaces:
//!   1. `handle_create_session` (POST /v1/sessions) validation-failure paths.
//!   2. `handle_create_command` (POST /v1/commands) validation-failure paths
//!      and session state-transition rejections.
//!   3. `parse_args_to_flags()` command-dispatch positional mapping for every
//!      special-cased subcommand (including `exec prompt` prompt-joining) and
//!      the forced `non-interactive`/`yolo` flag defaults (Finding D).
//!
//! Network-bound tests are prefixed `real_network_` so `make test-fast` can
//! skip them, matching the convention in `live_server.rs` / `wi_0079.rs`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use awman::command::dispatch::{CommandFrontend, Engines};
use awman::data::fs::api_db::SqliteSessionStore;
use awman::data::fs::api_paths::ApiPaths;
use awman::data::fs::auth_paths::AuthPathResolver;
use awman::data::EngineWorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::frontend::api::command_frontend::ApiDispatchFrontend;
use awman::frontend::api::event_bus::EventBus;
use awman::frontend::api::routes::{build_router, AppState, AuthMode};

// ─── Test helpers ─────────────────────────────────────────────────────────────

/// Build an `AppState` with the given workdir allowlist (empty by default).
fn make_app_state_with_workdirs(
    root: &std::path::Path,
    workdirs: Vec<std::path::PathBuf>,
) -> Arc<AppState> {
    let paths = ApiPaths::from_root(root);
    paths.ensure_root().expect("ensure_root");
    let store = SqliteSessionStore::open(paths.root()).expect("open sqlite");

    let auth_paths = AuthPathResolver::at_home(root);
    let runtime = Arc::new(ContainerRuntime::docker());
    let git_engine = Arc::new(GitEngine::new());
    let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    let agent_engine = Arc::new(AgentEngine::new(overlay_engine.clone(), runtime.clone()));
    let auth_engine = Arc::new(AuthEngine::with_paths(auth_paths, paths.clone()));
    let workflow_state_store = Arc::new(EngineWorkflowStateStore::at_git_root(paths.root()));

    let engines = Engines {
        runtime: runtime.clone(),
        container_runtime: Some(runtime),
        sandbox_runtime: None,
        git_engine,
        overlay_engine,
        auth_engine,
        agent_engine,
        workflow_state_store,
    };

    Arc::new(AppState {
        store: Arc::new(store),
        paths,
        workdirs,
        started_at: Instant::now(),
        task_handles: tokio::sync::Mutex::new(Vec::new()),
        auth_mode: AuthMode::Disabled,
        engines,
        sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        event_buses: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        setup_buses: tokio::sync::Mutex::new(HashMap::new()),
    })
}

fn make_app_state(root: &std::path::Path) -> Arc<AppState> {
    make_app_state_with_workdirs(root, vec![])
}

async fn spawn_router(
    state: Arc<AppState>,
) -> Option<(std::net::SocketAddr, tokio::task::JoinHandle<()>)> {
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    Some((addr, handle))
}

/// Insert an active + setup-ready local session directly into the store.
fn insert_ready_session(store: &SqliteSessionStore, session_id: &str, workdir: &str) {
    store
        .insert_session_full(
            session_id,
            workdir,
            &chrono::Utc::now().to_rfc3339(),
            "ready",
            "local",
            None,
        )
        .unwrap();
}

/// Construct an `ApiDispatchFrontend` for the given subcommand + args so we can
/// exercise `parse_args_to_flags` through the public `CommandFrontend` trait.
fn make_frontend(subcommand: &str, args: &[&str]) -> ApiDispatchFrontend {
    let bus = EventBus::new(16);
    let sender = bus.sender();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    ApiDispatchFrontend::new(subcommand, &args, sender)
}

// ════════════════════════════════════════════════════════════════════════════
//  Part 1 — handle_create_session (POST /v1/sessions) validation paths
// ════════════════════════════════════════════════════════════════════════════

/// PINS: unknown session_type → 400 Bad Request.
#[tokio::test]
async fn real_network_create_session_invalid_type_returns_400() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/sessions"))
        .json(&serde_json::json!({ "session_type": "banana" }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        400,
        "unknown session_type must be 400"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    // TEXT-ASSERTION (relaxable): current wording mentions the allowed values.
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("session_type must be"),
        "got {body}"
    );

    server.abort();
}

/// PINS: local session with no `workdir` field → 400 Bad Request.
#[tokio::test]
async fn real_network_create_session_local_missing_workdir_returns_400() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/sessions"))
        .json(&serde_json::json!({ "session_type": "local" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 400, "missing workdir must be 400");
    let body: serde_json::Value = resp.json().await.unwrap();
    // TEXT-ASSERTION (relaxable).
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("workdir is required"),
        "got {body}"
    );

    server.abort();
}

/// PINS: local session whose workdir cannot be canonicalized (does not exist)
/// → 400 Bad Request.
#[tokio::test]
async fn real_network_create_session_local_noncanonical_workdir_returns_400() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/sessions"))
        .json(&serde_json::json!({
            "session_type": "local",
            "workdir": "/this/path/definitely/does/not/exist/awman-0097"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        400,
        "non-canonicalizable workdir must be 400"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    // TEXT-ASSERTION (relaxable).
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("Cannot resolve path"),
        "got {body}"
    );

    server.abort();
}

/// PINS: local session with an existing workdir that is NOT in the allowlist
/// → 403 Forbidden.
#[tokio::test]
async fn real_network_create_session_local_offallowlist_workdir_returns_403() {
    let tmp = tempfile::tempdir().unwrap();
    // Allowlist a *different* directory than the one we request.
    let allowed = tempfile::tempdir().unwrap();
    let allowed_path = allowed.path().canonicalize().unwrap();
    let requested = tempfile::tempdir().unwrap();
    let requested_path = requested.path().canonicalize().unwrap();

    let state = make_app_state_with_workdirs(tmp.path(), vec![allowed_path]);
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/sessions"))
        .json(&serde_json::json!({
            "session_type": "local",
            "workdir": requested_path.display().to_string()
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        403,
        "off-allowlist workdir must be 403"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    // TEXT-ASSERTION (relaxable).
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("not in the allowlist"),
        "got {body}"
    );

    server.abort();
}

/// PINS: local session with an allowlisted workdir → 202 Accepted (happy path).
#[tokio::test]
async fn real_network_create_session_local_allowlisted_workdir_returns_202() {
    let tmp = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let workdir_path = workdir.path().canonicalize().unwrap();

    let state = make_app_state_with_workdirs(tmp.path(), vec![workdir_path.clone()]);
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/sessions"))
        .json(&serde_json::json!({
            "session_type": "local",
            "workdir": workdir_path.display().to_string()
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        202,
        "allowlisted local session must be 202"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["session_id"].as_str().is_some(), "got {body}");

    server.abort();
}

/// PINS: remote session with no `repo_url` field → 400 Bad Request.
#[tokio::test]
async fn real_network_create_session_remote_missing_repo_url_returns_400() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/sessions"))
        .json(&serde_json::json!({ "session_type": "remote", "branch": "main" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 400, "missing repo_url must be 400");
    let body: serde_json::Value = resp.json().await.unwrap();
    // TEXT-ASSERTION (relaxable).
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("repo_url is required"),
        "got {body}"
    );

    server.abort();
}

/// PINS: remote session with a present-but-empty `repo_url` → 400 Bad Request.
#[tokio::test]
async fn real_network_create_session_remote_empty_repo_url_returns_400() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/sessions"))
        .json(&serde_json::json!({ "session_type": "remote", "repo_url": "   " }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 400, "empty repo_url must be 400");
    let body: serde_json::Value = resp.json().await.unwrap();
    // TEXT-ASSERTION (relaxable).
    assert!(
        body["error"].as_str().unwrap_or("").contains("non-empty"),
        "got {body}"
    );

    server.abort();
}

/// PINS: remote session with a rejected `file:` scheme → 400 Bad Request.
#[tokio::test]
async fn real_network_create_session_remote_file_scheme_returns_400() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/sessions"))
        .json(&serde_json::json!({
            "session_type": "remote",
            "repo_url": "file:///etc/passwd"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 400, "file: scheme must be 400");
    let body: serde_json::Value = resp.json().await.unwrap();
    // TEXT-ASSERTION (relaxable).
    assert!(
        body["error"].as_str().unwrap_or("").contains("scheme"),
        "got {body}"
    );

    server.abort();
}

/// PINS: remote session with a bare (schemeless) path → 400 Bad Request.
#[tokio::test]
async fn real_network_create_session_remote_bare_path_returns_400() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/sessions"))
        .json(&serde_json::json!({
            "session_type": "remote",
            "repo_url": "/srv/git/local-repo"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        400,
        "bare schemeless path must be 400"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    // TEXT-ASSERTION (relaxable).
    assert!(
        body["error"].as_str().unwrap_or("").contains("scheme"),
        "got {body}"
    );

    server.abort();
}

/// PINS: each accepted remote URL scheme passes validation → 202 Accepted.
/// The background clone fails asynchronously against these fake URLs; that does
/// not affect the synchronous HTTP status, which is what we pin. We use the
/// reserved `.invalid` TLD (RFC 2606) so the background clone fails DNS
/// resolution near-instantly instead of blocking teardown on a connection
/// timeout.
#[tokio::test]
async fn real_network_create_session_remote_accepted_schemes_return_202() {
    let accepted = [
        "http://awman-nonexistent.invalid/org/repo.git",
        "https://awman-nonexistent.invalid/org/repo.git",
        "git@awman-nonexistent.invalid:org/repo.git",
        "ssh://git@awman-nonexistent.invalid/org/repo.git",
        "git://awman-nonexistent.invalid/org/repo.git",
    ];

    for url in accepted {
        let tmp = tempfile::tempdir().unwrap();
        let state = make_app_state(tmp.path());
        let Some((addr, server)) = spawn_router(state).await else {
            eprintln!("SKIP: cannot bind 127.0.0.1");
            return;
        };

        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/v1/sessions"))
            .json(&serde_json::json!({
                "session_type": "remote",
                "repo_url": url
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(
            resp.status().as_u16(),
            202,
            "accepted scheme '{url}' must return 202; got {}",
            resp.status()
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(
            body["session_id"].as_str().is_some(),
            "accepted scheme '{url}' must return a session_id; got {body}"
        );

        server.abort();
    }
}

// ════════════════════════════════════════════════════════════════════════════
//  Part 2 — handle_create_command (POST /v1/commands) validation paths
// ════════════════════════════════════════════════════════════════════════════

/// PINS: POST /v1/commands without the `x-awman-session` header → 400.
#[tokio::test]
async fn real_network_create_command_missing_session_header_returns_400() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/commands"))
        .json(&serde_json::json!({ "subcommand": "exec prompt", "args": [] }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        400,
        "missing x-awman-session header must be 400"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    // TEXT-ASSERTION (relaxable).
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("x-awman-session"),
        "got {body}"
    );

    server.abort();
}

/// PINS: a command blocked at the catalogue layer (`clean`, api_allowed=false)
/// → 400, with the `command not available via API` error shape. The catalogue
/// check runs before session lookup, so no session is required.
#[tokio::test]
async fn real_network_create_command_not_api_allowed_returns_400() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/commands"))
        .header("x-awman-session", "any-session")
        .json(&serde_json::json!({ "subcommand": "clean", "args": [] }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        400,
        "catalogue-blocked command must be 400"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    // TEXT-ASSERTION (relaxable): the structured error/`blocked_command` shape.
    assert_eq!(
        body["error"].as_str(),
        Some("command not available via API"),
        "got {body}"
    );

    server.abort();
}

/// PINS: POST /v1/commands for a session that does not exist → 404.
#[tokio::test]
async fn real_network_create_command_unknown_session_returns_404() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/commands"))
        .header("x-awman-session", "ghost-session")
        .json(&serde_json::json!({ "subcommand": "exec prompt", "args": ["hi"] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 404, "unknown session must be 404");
    let body: serde_json::Value = resp.json().await.unwrap();
    // TEXT-ASSERTION (relaxable).
    assert!(
        body["error"].as_str().unwrap_or("").contains("not found"),
        "got {body}"
    );

    server.abort();
}

/// PINS: POST /v1/commands for a closed session → 404.
#[tokio::test]
async fn real_network_create_command_closed_session_returns_404() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let ts = chrono::Utc::now().to_rfc3339();
    state
        .store
        .insert_session_full("closed-sess", "/work", &ts, "ready", "local", None)
        .unwrap();
    state.store.close_session_force("closed-sess", &ts).unwrap();

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/commands"))
        .header("x-awman-session", "closed-sess")
        .json(&serde_json::json!({ "subcommand": "exec prompt", "args": ["hi"] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 404, "closed session must be 404");
    let body: serde_json::Value = resp.json().await.unwrap();
    // TEXT-ASSERTION (relaxable).
    assert!(
        body["error"].as_str().unwrap_or("").contains("is closed"),
        "got {body}"
    );

    server.abort();
}

/// PINS: POST /v1/commands for a 'closing' session → 409.
#[tokio::test]
async fn real_network_create_command_closing_session_returns_409() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let ts = chrono::Utc::now().to_rfc3339();
    state
        .store
        .insert_session_full("closing-sess", "/work", &ts, "ready", "local", None)
        .unwrap();
    state
        .store
        .update_session_status("closing-sess", "closing")
        .unwrap();

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/commands"))
        .header("x-awman-session", "closing-sess")
        .json(&serde_json::json!({ "subcommand": "exec prompt", "args": [] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 409, "closing session must be 409");
    let body: serde_json::Value = resp.json().await.unwrap();
    // TEXT-ASSERTION (relaxable).
    assert!(
        body["error"].as_str().unwrap_or("").contains("closing"),
        "got {body}"
    );

    server.abort();
}

/// PINS: POST /v1/commands for an active session whose setup is NOT ready
/// → 409 (job-submission readiness guard).
#[tokio::test]
async fn real_network_create_command_session_not_ready_returns_409() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    // active session, but setup_status = 'running_ready' (not 'ready').
    let ts = chrono::Utc::now().to_rfc3339();
    state
        .store
        .insert_session_full(
            "notready-sess",
            "/work",
            &ts,
            "running_ready",
            "local",
            None,
        )
        .unwrap();

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/commands"))
        .header("x-awman-session", "notready-sess")
        .json(&serde_json::json!({ "subcommand": "exec prompt", "args": [] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 409, "not-ready session must be 409");
    let body: serde_json::Value = resp.json().await.unwrap();
    // TEXT-ASSERTION (relaxable).
    assert!(
        body["error"].as_str().unwrap_or("").contains("not ready"),
        "got {body}"
    );

    server.abort();
}

/// PINS: POST /v1/commands for an active + ready session → 202 Accepted, and
/// the response advertises the forced `flags_applied` (Finding D — yolo /
/// non_interactive forced true).
#[tokio::test]
async fn real_network_create_command_active_ready_session_returns_202() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    insert_ready_session(&state.store, "ok-sess", "/work");

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/commands"))
        .header("x-awman-session", "ok-sess")
        .json(&serde_json::json!({ "subcommand": "exec prompt", "args": ["hi"] }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        202,
        "active + ready session must be 202"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["command_id"].as_str().is_some(), "got {body}");
    // Finding D: the API frontend forces yolo + non_interactive to true and
    // advertises this in flags_applied. This pins the CURRENT policy location.
    assert_eq!(
        body["flags_applied"]["yolo"],
        serde_json::json!(true),
        "got {body}"
    );
    assert_eq!(
        body["flags_applied"]["non_interactive"],
        serde_json::json!(true),
        "got {body}"
    );

    server.abort();
}

// ════════════════════════════════════════════════════════════════════════════
//  Part 3 — parse_args_to_flags() command-dispatch mapping (Findings A + D)
//
//  These exercise the private parser through the public `ApiDispatchFrontend`
//  + `CommandFrontend` trait. They pin how positionals are mapped to named
//  arguments for every special-cased subcommand, plus the always-on
//  non-interactive / yolo flag defaults. No network binding — always run.
// ════════════════════════════════════════════════════════════════════════════

/// PINS: `exec prompt` joins all positionals into a single `prompt` argument
/// with spaces.
#[test]
fn parse_args_exec_prompt_joins_positionals_with_spaces() {
    let f = make_frontend("exec prompt", &["hello", "brave", "world"]);
    assert_eq!(
        f.argument(&["exec", "prompt"], "prompt")
            .unwrap()
            .as_deref(),
        Some("hello brave world"),
        "exec prompt must join positionals with single spaces"
    );
}

/// PINS: `exec prompt` with a single positional maps it verbatim.
#[test]
fn parse_args_exec_prompt_single_positional() {
    let f = make_frontend("exec prompt", &["solo"]);
    assert_eq!(
        f.argument(&["exec", "prompt"], "prompt")
            .unwrap()
            .as_deref(),
        Some("solo")
    );
}

/// PINS: `exec workflow` maps the first positional to both the `workflow`
/// argument and the `workflow` path flag.
#[test]
fn parse_args_exec_workflow_maps_first_positional() {
    let f = make_frontend("exec workflow", &["build.toml"]);
    assert_eq!(
        f.argument(&["exec", "workflow"], "workflow")
            .unwrap()
            .as_deref(),
        Some("build.toml")
    );
    assert_eq!(
        f.flag_path(&["exec", "workflow"], "workflow").unwrap(),
        Some(std::path::PathBuf::from("build.toml"))
    );
}

/// PINS: `specs amend` maps the first positional to the `work_item` argument.
#[test]
fn parse_args_specs_amend_maps_work_item() {
    let f = make_frontend("specs amend", &["0097"]);
    assert_eq!(
        f.argument(&["specs", "amend"], "work_item")
            .unwrap()
            .as_deref(),
        Some("0097")
    );
}

/// PINS: a positional the command never declared is a usage error in the
/// raw-args projection, matching clap. Silently dropping it broke the
/// projection parity this suite exists to guarantee.
#[test]
fn parse_args_rejects_positional_beyond_declared_arguments() {
    let f = make_frontend("specs amend", &["0097", "extra"]);
    let err = f
        .argument(&["specs", "amend"], "work_item")
        .expect_err("an undeclared extra positional must be a usage error");
    assert!(
        format!("{err}").contains("extra"),
        "the error must name the stray token; got: {err}"
    );
}

/// PINS: `config get` maps the first positional to the `field` argument.
#[test]
fn parse_args_config_get_maps_field() {
    let f = make_frontend("config get", &["agent"]);
    assert_eq!(
        f.argument(&["config", "get"], "field").unwrap().as_deref(),
        Some("agent")
    );
}

/// PINS: `config set` maps the first two positionals to `field` and `value`.
#[test]
fn parse_args_config_set_maps_field_and_value() {
    let f = make_frontend("config set", &["agent", "claude"]);
    assert_eq!(
        f.argument(&["config", "set"], "field").unwrap().as_deref(),
        Some("agent")
    );
    assert_eq!(
        f.argument(&["config", "set"], "value").unwrap().as_deref(),
        Some("claude")
    );
}

/// PINS: `remote exec workflow` maps the first positional to the `workflow`
/// argument and path flag.
#[test]
fn parse_args_remote_exec_workflow_maps_first_positional() {
    let f = make_frontend("remote exec workflow", &["deploy.toml"]);
    assert_eq!(
        f.argument(&["remote", "exec", "workflow"], "workflow")
            .unwrap()
            .as_deref(),
        Some("deploy.toml")
    );
    assert_eq!(
        f.flag_path(&["remote", "exec", "workflow"], "workflow")
            .unwrap(),
        Some(std::path::PathBuf::from("deploy.toml"))
    );
}

/// PINS: `remote exec prompt` joins positionals with spaces (like `exec prompt`).
#[test]
fn parse_args_remote_exec_prompt_joins_positionals() {
    let f = make_frontend("remote exec prompt", &["do", "the", "thing"]);
    assert_eq!(
        f.argument(&["remote", "exec", "prompt"], "prompt")
            .unwrap()
            .as_deref(),
        Some("do the thing")
    );
}

/// PINS: `remote session kill` maps the first positional to `session_id`.
#[test]
fn parse_args_remote_session_kill_maps_session_id() {
    let f = make_frontend("remote session kill", &["sess-abc-123"]);
    assert_eq!(
        f.argument(&["remote", "session", "kill"], "session_id")
            .unwrap()
            .as_deref(),
        Some("sess-abc-123")
    );
}

/// PINS (Finding D): `non-interactive` and `yolo` are ALWAYS forced to true in
/// API dispatch, regardless of subcommand or supplied args.
#[test]
fn parse_args_forces_non_interactive_and_yolo() {
    let f = make_frontend("exec prompt", &["hi"]);
    assert_eq!(
        f.flag_bool(&["exec", "prompt"], "non-interactive").unwrap(),
        Some(true),
        "non-interactive must be forced true in API mode"
    );
    assert_eq!(
        f.flag_bool(&["exec", "prompt"], "yolo").unwrap(),
        Some(true),
        "yolo must be forced true in API mode"
    );
}
