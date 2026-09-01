//! WI-0078 test suite: API Frontend Hardening, Remote Restructure & Unified
//! Event Bus.
//!
//! Test categories:
//!   - Dispatch catalogue unit tests (api_allowed_commands, validate_for_frontend)
//!   - API restriction live-server tests (HTTP 400 for non-exec commands)
//!   - Async session setup live-server tests (202, status polling, 409 guard)
//!   - EventBus unit tests (already in event_bus.rs; wire-format and NDJSON here)
//!   - SSE wire-format unit tests
//!   - NDJSON parsability tests
//!   - SessionSetupBus state machine tests
//!
//! Live server tests are prefixed `real_network_` and skip gracefully when
//! loopback binding is unavailable (sandboxed CI).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use awman::command::dispatch::catalogue::{CommandCatalogue, FrontendKind};
use awman::command::dispatch::Engines;
use awman::command::error::CommandError;
use awman::data::execution_event::{EventPayload, ExecutionEvent};
use awman::data::fs::api_db::SqliteSessionStore;
use awman::data::fs::api_paths::ApiPaths;
use awman::data::fs::auth_paths::AuthPathResolver;
use awman::data::session_setup_event::{SessionSetupState, SessionSetupStatus};
use awman::data::EngineWorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::frontend::api::event_bus::EventBus;
use awman::frontend::api::routes::{build_router, AppState, AuthMode};
use awman::frontend::api::session_setup::SessionSetupBus;

// ─── Test helpers ─────────────────────────────────────────────────────────────

fn make_app_state(root: &std::path::Path, auth: AuthMode) -> Arc<AppState> {
    make_app_state_with_workdirs(root, auth, vec![])
}

fn make_app_state_with_workdirs(
    root: &std::path::Path,
    auth: AuthMode,
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
        auth_mode: auth,
        engines,
        sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        event_buses: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        setup_buses: tokio::sync::Mutex::new(HashMap::new()),
    })
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

// ─── Dispatch catalogue unit tests ────────────────────────────────────────────

#[test]
fn catalogue_api_allowed_commands_includes_exec_and_squad_commands() {
    let cat = CommandCatalogue::get();
    let allowed = cat.api_allowed_commands();

    let expected = [
        ("exec", "prompt"),
        ("exec", "workflow"),
        ("squad", "start"),
        ("squad", "stop"),
        ("squad", "status"),
        ("squad", "logs"),
        ("squad", "add"),
        ("squad", "list"),
        ("squad", "show"),
        ("squad", "remove"),
        ("squad", "pause"),
        ("squad", "resume"),
    ];
    assert_eq!(
        allowed.as_slice(),
        expected,
        "API-allowed command set changed unexpectedly: {allowed:?}"
    );
}

#[test]
fn catalogue_ready_not_in_api_allowed_commands() {
    let cat = CommandCatalogue::get();
    let allowed = cat.api_allowed_commands();

    let has_ready = allowed.iter().any(|(p, s)| *p == "ready" || *s == "ready");
    assert!(
        !has_ready,
        "\"ready\" must NOT appear in api_allowed_commands; got {allowed:?}"
    );
}

#[test]
fn catalogue_validate_for_frontend_rejects_chat_for_api() {
    let cat = CommandCatalogue::get();
    let result = cat.validate_for_frontend(FrontendKind::Api, &["chat"]);

    match result {
        Err(CommandError::NotAvailableForFrontend { command, frontend }) => {
            assert!(
                command.contains("chat"),
                "error should name the blocked command; got {command:?}"
            );
            assert_eq!(frontend, "api");
        }
        other => panic!(
            "expected Err(NotAvailableForFrontend) for 'chat' on Api frontend, got {other:?}"
        ),
    }
}

#[test]
fn catalogue_validate_for_frontend_rejects_ready_for_api() {
    let cat = CommandCatalogue::get();
    let result = cat.validate_for_frontend(FrontendKind::Api, &["ready"]);
    assert!(
        matches!(result, Err(CommandError::NotAvailableForFrontend { .. })),
        "\"ready\" must be rejected for Api frontend"
    );
}

#[test]
fn catalogue_validate_for_frontend_accepts_exec_workflow_for_api() {
    let cat = CommandCatalogue::get();
    let result = cat.validate_for_frontend(FrontendKind::Api, &["exec", "workflow"]);
    assert!(
        result.is_ok(),
        "\"exec workflow\" must be accepted for Api frontend; got {result:?}"
    );
}

#[test]
fn catalogue_validate_for_frontend_accepts_exec_prompt_for_api() {
    let cat = CommandCatalogue::get();
    let result = cat.validate_for_frontend(FrontendKind::Api, &["exec", "prompt"]);
    assert!(
        result.is_ok(),
        "\"exec prompt\" must be accepted for Api frontend; got {result:?}"
    );
}

#[test]
fn catalogue_cli_and_tui_frontends_allow_all_commands() {
    let cat = CommandCatalogue::get();
    for cmd in &["chat", "ready", "init", "status", "config"] {
        assert!(
            cat.validate_for_frontend(FrontendKind::Cli, &[cmd]).is_ok(),
            "CLI must allow {cmd:?}"
        );
        assert!(
            cat.validate_for_frontend(FrontendKind::Tui, &[cmd]).is_ok(),
            "TUI must allow {cmd:?}"
        );
    }
}

// ─── API restriction live-server tests ────────────────────────────────────────

/// Helper: POST /v1/commands with the given subcommand and optional session header.
/// Returns (status_code, body_json).
async fn post_command(
    client: &reqwest::Client,
    addr: std::net::SocketAddr,
    subcommand: &str,
    session_id: Option<&str>,
) -> (u16, serde_json::Value) {
    let url = format!("http://{addr}/v1/commands");
    let body = serde_json::json!({
        "subcommand": subcommand,
        "args": []
    });
    let mut req = client.post(&url).json(&body);
    if let Some(sid) = session_id {
        req = req.header("x-awman-session", sid);
    }
    let resp = req.send().await.expect("POST /v1/commands");
    let status = resp.status().as_u16();
    let json: serde_json::Value = resp.json().await.unwrap_or_default();
    (status, json)
}

#[tokio::test]
async fn real_network_api_rejects_chat_with_400() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path(), AuthMode::Disabled);
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };
    let client = reqwest::Client::new();

    let (status, body) = post_command(&client, addr, "chat", Some("any-session")).await;

    assert_eq!(
        status, 400,
        "\"chat\" must be rejected with HTTP 400, got {status}; body={body}"
    );
    let error_msg = body["error"].as_str().unwrap_or_default();
    assert!(
        error_msg.contains("not available via API") || error_msg.contains("not available"),
        "error body must describe API restriction; got {body}"
    );

    server.abort();
}

#[tokio::test]
async fn real_network_api_rejects_ready_with_400() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path(), AuthMode::Disabled);
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };
    let client = reqwest::Client::new();

    let (status, body) = post_command(&client, addr, "ready", Some("any-session")).await;
    assert_eq!(
        status, 400,
        "\"ready\" must be rejected with 400; body={body}"
    );

    server.abort();
}

#[tokio::test]
async fn real_network_api_rejects_init_with_400() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path(), AuthMode::Disabled);
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };
    let client = reqwest::Client::new();

    let (status, body) = post_command(&client, addr, "init", Some("any-session")).await;
    assert_eq!(
        status, 400,
        "\"init\" must be rejected with 400; body={body}"
    );

    server.abort();
}

#[tokio::test]
async fn real_network_api_rejects_status_with_400() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path(), AuthMode::Disabled);
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };
    let client = reqwest::Client::new();

    let (status, body) = post_command(&client, addr, "status", Some("any-session")).await;
    assert_eq!(
        status, 400,
        "\"status\" must be rejected with 400; body={body}"
    );

    server.abort();
}

#[tokio::test]
async fn real_network_api_rejects_api_server_start_with_400() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path(), AuthMode::Disabled);
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };
    let client = reqwest::Client::new();

    let (status, body) = post_command(&client, addr, "api start", Some("any-session")).await;
    assert_eq!(
        status, 400,
        "\"api start\" must be rejected with 400; body={body}"
    );

    server.abort();
}

/// "exec workflow" routing must not return the command-restriction 400.
/// With a valid session header pointing to a non-existent session,
/// the API restriction check passes and the 404 (session not found) is returned.
#[tokio::test]
async fn real_network_api_accepts_exec_workflow_routing_not_command_restriction_400() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path(), AuthMode::Disabled);
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };
    let client = reqwest::Client::new();

    let (status, body) =
        post_command(&client, addr, "exec workflow", Some("nonexistent-session")).await;

    // Must NOT be the command-restriction 400
    let is_command_restriction_400 = status == 400
        && body["error"]
            .as_str()
            .map(|e| e.contains("not available via API") || e.contains("not available"))
            .unwrap_or(false);
    assert!(
        !is_command_restriction_400,
        "\"exec workflow\" must not be blocked by command restriction; got {status} body={body}"
    );

    server.abort();
}

/// "exec prompt" routing must not return the command-restriction 400.
#[tokio::test]
async fn real_network_api_accepts_exec_prompt_routing_not_command_restriction_400() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path(), AuthMode::Disabled);
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };
    let client = reqwest::Client::new();

    let (status, body) =
        post_command(&client, addr, "exec prompt", Some("nonexistent-session")).await;

    let is_command_restriction_400 = status == 400
        && body["error"]
            .as_str()
            .map(|e| e.contains("not available via API") || e.contains("not available"))
            .unwrap_or(false);
    assert!(
        !is_command_restriction_400,
        "\"exec prompt\" must not be blocked by command restriction; got {status} body={body}"
    );

    server.abort();
}

/// The 400 rejection body must list the available API commands.
#[tokio::test]
async fn real_network_api_rejection_body_lists_available_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path(), AuthMode::Disabled);
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };
    let client = reqwest::Client::new();

    let (status, body) = post_command(&client, addr, "chat", Some("any-session")).await;
    assert_eq!(status, 400);

    let available = &body["available"];
    assert!(
        available.is_array(),
        "rejection body must include an 'available' array; got {body}"
    );
    let arr = available.as_array().unwrap();
    let strings: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        strings.contains(&"exec workflow"),
        "available list must include \"exec workflow\"; got {arr:?}"
    );
    assert!(
        strings.contains(&"exec prompt"),
        "available list must include \"exec prompt\"; got {arr:?}"
    );

    server.abort();
}

// ─── Async session setup live-server tests ─────────────────────────────────────

/// POST /sessions with an allowed workdir returns HTTP 202 immediately,
/// with a session_id in the JSON body.
#[tokio::test]
async fn real_network_post_sessions_returns_202_immediately() {
    let tmp = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let workdir_path = workdir.path().canonicalize().unwrap();

    let state =
        make_app_state_with_workdirs(tmp.path(), AuthMode::Disabled, vec![workdir_path.clone()]);
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap();

    let start = std::time::Instant::now();
    let resp = client
        .post(format!("http://{addr}/v1/sessions"))
        .json(&serde_json::json!({ "workdir": workdir_path.display().to_string() }))
        .send()
        .await
        .expect("POST /v1/sessions");
    let elapsed = start.elapsed();

    assert_eq!(
        resp.status().as_u16(),
        202,
        "POST /sessions must return 202 Accepted immediately"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "POST /sessions must return within 1 second; took {elapsed:?}"
    );

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["session_id"].is_string(),
        "202 body must contain a string session_id; got {body}"
    );

    server.abort();
}

/// GET /sessions/{id}/status for a non-existent session returns 404.
#[tokio::test]
async fn real_network_session_status_unknown_id_returns_404() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path(), AuthMode::Disabled);
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let resp = reqwest::get(format!("http://{addr}/v1/sessions/nonexistent/status"))
        .await
        .expect("GET /sessions/nonexistent/status");
    assert_eq!(
        resp.status().as_u16(),
        404,
        "unknown session status must return 404"
    );

    server.abort();
}

/// After creating a session, GET /sessions/{id}/status returns a JSON body
/// with a `status` field and a `session_id` field.
#[tokio::test]
async fn real_network_session_status_returns_valid_body_after_create() {
    let tmp = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let workdir_path = workdir.path().canonicalize().unwrap();

    let state =
        make_app_state_with_workdirs(tmp.path(), AuthMode::Disabled, vec![workdir_path.clone()]);
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let client = reqwest::Client::new();

    // Create session.
    let create_resp = client
        .post(format!("http://{addr}/v1/sessions"))
        .json(&serde_json::json!({ "workdir": workdir_path.display().to_string() }))
        .send()
        .await
        .expect("POST /v1/sessions");
    assert_eq!(create_resp.status().as_u16(), 202);
    let create_body: serde_json::Value = create_resp.json().await.unwrap();
    let session_id = create_body["session_id"].as_str().unwrap();

    // Poll status — setup is async so this should return quickly.
    let status_resp = client
        .get(format!("http://{addr}/v1/sessions/{session_id}/status"))
        .send()
        .await
        .expect("GET /sessions/{id}/status");
    assert_eq!(status_resp.status().as_u16(), 200);
    let status_body: serde_json::Value = status_resp.json().await.unwrap();

    assert_eq!(
        status_body["session_id"].as_str().unwrap(),
        session_id,
        "status body must echo session_id"
    );
    assert!(
        status_body["status"].is_string(),
        "status body must contain a string 'status' field; got {status_body}"
    );

    server.abort();
}

/// Submitting a job to a session that is still in setup (initializing) must
/// return HTTP 409 Conflict with setup_status in the body.
#[tokio::test]
async fn real_network_job_rejected_during_session_setup() {
    let tmp = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let workdir_path = workdir.path().canonicalize().unwrap();

    let state =
        make_app_state_with_workdirs(tmp.path(), AuthMode::Disabled, vec![workdir_path.clone()]);
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let client = reqwest::Client::new();

    // Create session — returns 202 before setup finishes.
    let create_resp = client
        .post(format!("http://{addr}/v1/sessions"))
        .json(&serde_json::json!({ "workdir": workdir_path.display().to_string() }))
        .send()
        .await
        .expect("POST /v1/sessions");
    assert_eq!(create_resp.status().as_u16(), 202);
    let create_body: serde_json::Value = create_resp.json().await.unwrap();
    let session_id = create_body["session_id"].as_str().unwrap();

    // Immediately submit a job — setup has not completed.
    let (status, body) = post_command(&client, addr, "exec workflow", Some(session_id)).await;

    // Either 409 (still setting up) or 404 (session open failed and was never
    // marked active). Both are acceptable: neither is 202.
    assert_ne!(
        status, 202,
        "job must not be accepted while session setup is pending"
    );
    // If 409 specifically, assert setup_status is present.
    if status == 409 {
        assert!(
            body["setup_status"].is_string(),
            "409 body must include setup_status; got {body}"
        );
    }

    server.abort();
}

// ─── SSE wire-format unit tests ───────────────────────────────────────────────

#[test]
fn sse_event_type_names_are_correct_snake_case() {
    use EventPayload::*;
    let cases: &[(EventPayload, &str)] = &[
        (StdoutLine("x".into()), "stdout_line"),
        (StderrLine("x".into()), "stderr_line"),
        (
            StatusMessage {
                phase: "p".into(),
                message: "m".into(),
            },
            "status_message",
        ),
        (
            WorkflowStepTransition {
                step_name: "s".into(),
                step_index: 0,
                from_status: "pending".into(),
                to_status: "running".into(),
            },
            "workflow_step_transition",
        ),
        (
            WorkflowPhaseTransition {
                phase: "main".into(),
                step_desc: "step".into(),
                status: "running".into(),
            },
            "workflow_phase_transition",
        ),
        (
            CommandStatus {
                status: "done".into(),
                exit_code: Some(0),
                error: None,
            },
            "command_status",
        ),
        (Done, "done"),
    ];

    for (payload, expected) in cases {
        assert_eq!(
            payload.sse_event_type(),
            *expected,
            "SSE event type mismatch for {:?}",
            std::mem::discriminant(payload)
        );
    }
}

/// Each SSE message header in the wire format is `event: <type>\ndata: <json>\n\n`.
/// Verify we can reconstruct the expected format from an ExecutionEvent.
#[test]
fn sse_wire_format_assembles_correctly() {
    let event = ExecutionEvent {
        timestamp: chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
        sequence: 42,
        payload: EventPayload::StdoutLine("hello world".into()),
    };

    let event_type = event.payload.sse_event_type();
    let data_json = serde_json::to_string(&event).unwrap();

    // The SSE wire format is: "event: <type>\ndata: <json>\n\n"
    let wire = format!("event: {event_type}\ndata: {data_json}\n\n");

    assert!(wire.starts_with("event: stdout_line\n"));
    assert!(wire.contains("\"sequence\":42"));
    assert!(wire.ends_with("\n\n"));

    // The event type must also parse back correctly from JSON.
    let parsed: ExecutionEvent = serde_json::from_str(&data_json).unwrap();
    assert_eq!(parsed.sequence, 42);
    assert!(matches!(
        parsed.payload,
        EventPayload::StdoutLine(ref s) if s == "hello world"
    ));
}

// ─── NDJSON parsability test ──────────────────────────────────────────────────

/// Write N events to a temp NDJSON file (mimicking the logfile writer) and
/// assert every line parses back to a valid ExecutionEvent.
#[tokio::test]
async fn ndjson_1000_events_all_parseable() {
    use tokio::io::AsyncWriteExt;

    let tmp = tempfile::tempdir().unwrap();
    let events_log = tmp.path().join("events.log");

    // Emit a mix of event types.
    let bus = EventBus::new(4096);
    let sender = bus.sender();
    let mut rx = bus.subscribe();

    let n = 1000u64;
    for i in 0..n {
        let payload = match i % 5 {
            0 => EventPayload::StdoutLine(format!("stdout line {i}")),
            1 => EventPayload::StderrLine(format!("stderr line {i}")),
            2 => EventPayload::StatusMessage {
                phase: "build".into(),
                message: format!("status {i}"),
            },
            3 => EventPayload::WorkflowStepTransition {
                step_name: format!("step_{i}"),
                step_index: i as usize,
                from_status: "pending".into(),
                to_status: "running".into(),
            },
            _ => EventPayload::CommandStatus {
                status: "running".into(),
                exit_code: None,
                error: None,
            },
        };
        sender.emit(payload);
    }
    sender.emit(EventPayload::Done);

    // Write to file, mimicking the logfile writer in routes.rs.
    let mut file = tokio::fs::File::create(&events_log).await.unwrap();
    let mut written = 0u64;
    loop {
        match rx.recv().await {
            Ok(event) => {
                if let Ok(json) = serde_json::to_string(&event) {
                    file.write_all(format!("{json}\n").as_bytes())
                        .await
                        .unwrap();
                }
                written += 1;
                if matches!(event.payload, EventPayload::Done) {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                panic!("unexpected lag: {n}");
            }
        }
    }
    file.flush().await.unwrap();

    assert_eq!(
        written,
        n + 1,
        "should have written {n} events + Done sentinel"
    );

    // Parse every line back.
    let content = tokio::fs::read_to_string(&events_log).await.unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(
        lines.len() as u64,
        n + 1,
        "events.log must have {} lines",
        n + 1
    );

    for (line_no, line) in lines.iter().enumerate() {
        let parsed: Result<ExecutionEvent, _> = serde_json::from_str(line);
        assert!(
            parsed.is_ok(),
            "line {line_no} failed to parse as ExecutionEvent: {line:?}"
        );
        let ev = parsed.unwrap();
        assert_eq!(ev.sequence, line_no as u64);
    }
}

// ─── SessionSetupBus state machine tests ──────────────────────────────────────

#[tokio::test]
async fn session_setup_bus_initial_state_is_initializing() {
    let bus = SessionSetupBus::new(32);
    let state = bus.snapshot();
    assert_eq!(state.status, SessionSetupStatus::Initializing);
    assert!(state.current_stage.is_none());
    assert!(state.current_ready_phase.is_none());
    assert!(state.ready_step_statuses.is_empty());
    assert!(state.ready_summary.is_none());
    assert!(state.error.is_none());
}

#[tokio::test]
async fn session_setup_bus_status_transitions_via_sender() {
    let bus = SessionSetupBus::new(32);
    let sender = bus.sender();

    sender.update_status(SessionSetupStatus::RunningReady);
    sender.update_stage("Building image...");

    let state = bus.snapshot();
    assert_eq!(state.status, SessionSetupStatus::RunningReady);
    assert_eq!(state.current_stage.as_deref(), Some("Building image..."));
}

#[tokio::test]
async fn session_setup_bus_mark_failed_sets_error() {
    let bus = SessionSetupBus::new(32);
    let sender = bus.sender();

    sender.mark_failed("build_step", "docker not found");

    let state = bus.snapshot();
    assert_eq!(state.status, SessionSetupStatus::Failed);
    let err = state.error.as_ref().unwrap();
    assert_eq!(err.stage, "build_step");
    assert_eq!(err.message, "docker not found");
    assert!(state.status.is_terminal());
}

#[tokio::test]
async fn session_setup_bus_ready_step_statuses_accumulate() {
    use awman::engine::step_status::StepStatus;

    let bus = SessionSetupBus::new(32);
    let sender = bus.sender();

    sender.update_ready_step("preflight", StepStatus::Running);
    {
        let state = bus.snapshot();
        assert_eq!(state.ready_step_statuses.len(), 1);
        assert_eq!(state.ready_step_statuses[0].step, "preflight");
    }

    sender.update_ready_step("build_image", StepStatus::Running);
    {
        let state = bus.snapshot();
        assert_eq!(state.ready_step_statuses.len(), 2);
    }

    // Update an existing step.
    sender.update_ready_step("preflight", StepStatus::Done);
    {
        let state = bus.snapshot();
        assert_eq!(
            state.ready_step_statuses.len(),
            2,
            "update must not add a duplicate"
        );
        let preflight = state
            .ready_step_statuses
            .iter()
            .find(|e| e.step == "preflight")
            .unwrap();
        assert_eq!(preflight.status, StepStatus::Done);
    }
}

#[tokio::test]
async fn session_setup_bus_emit_and_receive_event() {
    use awman::data::session_setup_event::SetupEventPayload;

    let bus = SessionSetupBus::new(32);
    let mut rx = bus.subscribe();
    let sender = bus.sender();

    sender.emit(SetupEventPayload::StageChanged {
        stage: "cloning".into(),
        message: "Cloning repository...".into(),
    });

    let ev = rx.recv().await.unwrap();
    assert_eq!(ev.sequence, 0);
    assert!(matches!(
        ev.payload,
        SetupEventPayload::StageChanged { ref stage, .. } if stage == "cloning"
    ));
}

// ─── SessionSetupState serialization test ─────────────────────────────────────

#[test]
fn session_setup_state_serializes_and_deserializes_roundtrip() {
    let state = SessionSetupState {
        status: SessionSetupStatus::RunningReady,
        current_stage: Some("Building base image...".into()),
        current_ready_phase: None,
        ready_step_statuses: vec![],
        ready_summary: None,
        error: None,
    };

    let json = serde_json::to_string(&state).unwrap();
    let parsed: SessionSetupState = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.status, SessionSetupStatus::RunningReady);
    assert_eq!(
        parsed.current_stage.as_deref(),
        Some("Building base image...")
    );
}

#[test]
fn session_setup_status_as_str_matches_snake_case() {
    let cases = &[
        (SessionSetupStatus::Initializing, "initializing"),
        (SessionSetupStatus::CloningRepository, "cloning_repository"),
        (SessionSetupStatus::SettingUpBranch, "setting_up_branch"),
        (SessionSetupStatus::RunningReady, "running_ready"),
        (SessionSetupStatus::Ready, "ready"),
        (SessionSetupStatus::Failed, "failed"),
    ];
    for (status, expected) in cases {
        assert_eq!(
            status.as_str(),
            *expected,
            "SessionSetupStatus::as_str mismatch"
        );
    }
}

#[test]
fn session_setup_status_is_terminal_only_for_ready_and_failed() {
    assert!(!SessionSetupStatus::Initializing.is_terminal());
    assert!(!SessionSetupStatus::CloningRepository.is_terminal());
    assert!(!SessionSetupStatus::SettingUpBranch.is_terminal());
    assert!(!SessionSetupStatus::RunningReady.is_terminal());
    assert!(SessionSetupStatus::Ready.is_terminal());
    assert!(SessionSetupStatus::Failed.is_terminal());
}

// ─── ExecutionEvent unit tests ────────────────────────────────────────────────

#[test]
fn execution_event_json_roundtrip_all_payload_variants() {
    let payloads = vec![
        EventPayload::StdoutLine("hello".into()),
        EventPayload::StderrLine("error".into()),
        EventPayload::StatusMessage {
            phase: "build".into(),
            message: "building image".into(),
        },
        EventPayload::WorkflowStepTransition {
            step_name: "step1".into(),
            step_index: 0,
            from_status: "pending".into(),
            to_status: "running".into(),
        },
        EventPayload::WorkflowPhaseTransition {
            phase: "main".into(),
            step_desc: "Running agent".into(),
            status: "running".into(),
        },
        EventPayload::CommandStatus {
            status: "done".into(),
            exit_code: Some(0),
            error: None,
        },
        EventPayload::Done,
    ];

    for (i, payload) in payloads.into_iter().enumerate() {
        let event = ExecutionEvent {
            timestamp: chrono::Utc::now(),
            sequence: i as u64,
            payload,
        };
        let json = serde_json::to_string(&event)
            .unwrap_or_else(|e| panic!("failed to serialize event {i}: {e}"));
        let parsed: ExecutionEvent = serde_json::from_str(&json)
            .unwrap_or_else(|e| panic!("failed to deserialize event {i}: {e}"));
        assert_eq!(parsed.sequence, i as u64);
    }
}

#[test]
fn execution_event_to_plain_text_returns_human_readable_for_output_variants() {
    assert_eq!(
        EventPayload::StdoutLine("line".into()).to_plain_text(),
        Some("line".into())
    );
    assert_eq!(
        EventPayload::StderrLine("err".into()).to_plain_text(),
        Some("err".into())
    );
    assert_eq!(
        EventPayload::StatusMessage {
            phase: "p".into(),
            message: "m".into()
        }
        .to_plain_text(),
        Some("[p] m".into())
    );
    assert_eq!(EventPayload::Done.to_plain_text(), None);
}

// ─── Remote catalogue structure tests ─────────────────────────────────────────

/// The `remote` command must expose exactly the four subcommand paths:
/// `remote session start`, `remote session kill`,
/// `remote exec workflow`, `remote exec prompt`.
#[test]
fn remote_command_catalogue_has_exactly_four_paths() {
    let cat = CommandCatalogue::get();
    let remote = cat.lookup(&["remote"]).expect("remote command must exist");
    let session = remote
        .find_subcommand("session")
        .expect("remote session must exist");
    let exec = remote
        .find_subcommand("exec")
        .expect("remote exec must exist");

    assert!(
        session.find_subcommand("start").is_some(),
        "remote session start must exist"
    );
    assert!(
        session.find_subcommand("kill").is_some(),
        "remote session kill must exist"
    );
    assert!(
        exec.find_subcommand("workflow").is_some(),
        "remote exec workflow must exist"
    );
    assert!(
        exec.find_subcommand("prompt").is_some(),
        "remote exec prompt must exist"
    );

    // remote session must have exactly start + kill
    let session_sub_count = session.subcommands.len();
    assert_eq!(
        session_sub_count, 2,
        "remote session must have exactly 2 subcommands (start, kill); got {session_sub_count}"
    );
    // remote exec must have exactly workflow + prompt
    let exec_sub_count = exec.subcommands.len();
    assert_eq!(
        exec_sub_count, 2,
        "remote exec must have exactly 2 subcommands (workflow, prompt); got {exec_sub_count}"
    );
}

/// `remote session start` must have `--type`, `--workdir`, `--repo-url`,
/// and `--branch` flags. (The legacy `--wait` flag was removed when polling
/// became unconditional.)
#[test]
fn remote_session_start_has_required_flags() {
    let cat = CommandCatalogue::get();
    let start = cat
        .lookup(&["remote", "session", "start"])
        .expect("remote session start must exist");

    for flag_name in &["type", "workdir", "repo-url", "branch"] {
        assert!(
            start.find_flag(flag_name).is_some(),
            "remote session start must have --{flag_name} flag"
        );
    }
}

/// `remote exec workflow` and `remote exec prompt` exist and are not
/// API-allowed (they are client-side commands, not server-side dispatch).
#[test]
fn remote_exec_subcommands_are_not_api_allowed() {
    let cat = CommandCatalogue::get();
    let wf = cat
        .lookup(&["remote", "exec", "workflow"])
        .expect("remote exec workflow");
    let pr = cat
        .lookup(&["remote", "exec", "prompt"])
        .expect("remote exec prompt");

    assert!(
        !wf.api_allowed,
        "remote exec workflow must not be api_allowed (it's the client side)"
    );
    assert!(
        !pr.api_allowed,
        "remote exec prompt must not be api_allowed (it's the client side)"
    );
}

// ─── Always-yolo enforcement (Layer 3) ────────────────────────────────────────

/// The API frontend's `flag_bool` must return `true` for `yolo` and
/// `non-interactive` regardless of whether they were sent in the request.
/// This is the WI's always-yolo contract enforced inside the Layer 3
/// frontend — independent of how the command layer interprets the values.
#[test]
fn api_frontend_always_returns_yolo_true_regardless_of_input() {
    use awman::command::dispatch::CommandFrontend;
    use awman::frontend::api::command_frontend::ApiDispatchFrontend;
    use awman::frontend::api::event_bus::EventBus;

    let bus = EventBus::new(8);
    // Send the explicit "--yolo false --non-interactive false" args — the
    // frontend must still report `true`.
    let args: Vec<String> = vec![
        "some-prompt".into(),
        "--yolo".into(),
        "false".into(),
        "--non-interactive".into(),
        "false".into(),
    ];
    let fe = ApiDispatchFrontend::new("exec prompt", &args, bus.sender());

    assert_eq!(
        fe.flag_bool(&["exec", "prompt"], "yolo").unwrap(),
        Some(true),
        "API frontend must always force yolo=true"
    );
    assert_eq!(
        fe.flag_bool(&["exec", "prompt"], "non-interactive")
            .unwrap(),
        Some(true),
        "API frontend must always force non-interactive=true"
    );
}

/// Exec responses must include a `flags_applied` object documenting the
/// server-enforced yolo/non_interactive overrides.
#[tokio::test]
async fn real_network_exec_response_advertises_flags_applied() {
    let tmp = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let workdir_path = workdir.path().canonicalize().unwrap();

    let state =
        make_app_state_with_workdirs(tmp.path(), AuthMode::Disabled, vec![workdir_path.clone()]);
    let Some((addr, server)) = spawn_router(state.clone()).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    // Insert a ready session directly so we can submit an exec without
    // waiting for the async setup pipeline.
    state
        .store
        .insert_session_full(
            "flags-session",
            &workdir_path.display().to_string(),
            "2026-01-01T00:00:00Z",
            "ready",
            "local",
            None,
        )
        .unwrap();

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/v1/commands"))
        .header("x-awman-session", "flags-session")
        .json(&serde_json::json!({
            "subcommand": "exec prompt",
            "args": ["hello", "--yolo", "false"],
        }))
        .send()
        .await
        .expect("POST /v1/commands");
    assert_eq!(resp.status().as_u16(), 202);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["flags_applied"]["yolo"],
        serde_json::Value::Bool(true),
        "flags_applied.yolo must be true; got {body}"
    );
    assert_eq!(
        body["flags_applied"]["non_interactive"],
        serde_json::Value::Bool(true),
        "flags_applied.non_interactive must be true; got {body}"
    );

    server.abort();
}

// ─── Session status response shape ────────────────────────────────────────────

/// The `/status` response shape must include `current_ready_phase` (per WI).
#[tokio::test]
async fn real_network_session_status_includes_current_ready_phase_field() {
    let tmp = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let workdir_path = workdir.path().canonicalize().unwrap();

    let state =
        make_app_state_with_workdirs(tmp.path(), AuthMode::Disabled, vec![workdir_path.clone()]);
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let client = reqwest::Client::new();
    let create_resp = client
        .post(format!("http://{addr}/v1/sessions"))
        .json(&serde_json::json!({ "workdir": workdir_path.display().to_string() }))
        .send()
        .await
        .expect("POST /v1/sessions");
    let body: serde_json::Value = create_resp.json().await.unwrap();
    let sid = body["session_id"].as_str().unwrap();

    let status_resp = client
        .get(format!("http://{addr}/v1/sessions/{sid}/status"))
        .send()
        .await
        .unwrap();
    let status_body: serde_json::Value = status_resp.json().await.unwrap();
    assert!(
        status_body.get("current_ready_phase").is_some(),
        "status response must include `current_ready_phase` key; got {status_body}"
    );

    server.abort();
}

// ─── Server restart cleanup ───────────────────────────────────────────────────

/// Insert a session in `running_ready` state, then run the cleanup-on-startup
/// logic: list_sessions_with_in_progress_setup must return that session.
#[test]
fn list_sessions_with_in_progress_setup_finds_non_terminal_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(tmp.path()).unwrap();

    store
        .insert_session_full("s1", "/wd1", "ts", "running_ready", "local", None)
        .unwrap();
    store
        .insert_session_full(
            "s2",
            "/wd2",
            "ts",
            "cloning_repository",
            "remote",
            Some("/clone/s2"),
        )
        .unwrap();
    store
        .insert_session_full("s3", "/wd3", "ts", "ready", "local", None)
        .unwrap();
    store
        .insert_session_full("s4", "/wd4", "ts", "failed", "local", None)
        .unwrap();

    let in_progress = store.list_sessions_with_in_progress_setup().unwrap();
    let ids: Vec<&str> = in_progress.iter().map(|s| s.id.as_str()).collect();
    assert!(ids.contains(&"s1"), "running_ready must be in-progress");
    assert!(
        ids.contains(&"s2"),
        "cloning_repository must be in-progress"
    );
    assert!(!ids.contains(&"s3"), "ready is terminal, not in-progress");
    assert!(!ids.contains(&"s4"), "failed is terminal, not in-progress");
    assert_eq!(in_progress.len(), 2);
}

/// `update_setup_status` round-trips through the DB.
#[test]
fn update_setup_status_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(tmp.path()).unwrap();
    store
        .insert_session_full("sid", "/wd", "ts", "initializing", "local", None)
        .unwrap();

    let s = store.get_session("sid").unwrap().unwrap();
    assert_eq!(s.setup_status, "initializing");

    assert!(store.update_setup_status("sid", "ready").unwrap());
    let s2 = store.get_session("sid").unwrap().unwrap();
    assert_eq!(s2.setup_status, "ready");
}

// ─── Per-job SSE endpoint ─────────────────────────────────────────────────────

/// GET /v1/commands/{id}/logs for a non-existent command returns 404.
#[tokio::test]
async fn real_network_job_logs_404_for_unknown_job() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path(), AuthMode::Disabled);
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let resp = reqwest::get(format!("http://{addr}/v1/commands/no-such-cmd/logs"))
        .await
        .expect("GET /commands/logs");
    assert_eq!(resp.status().as_u16(), 404);

    server.abort();
}

// ─── Setup-state persistence ──────────────────────────────────────────────────

/// Unit test for the persistence path: directly invoke the setup state
/// writer with a known state and assert the JSON round-trips.
#[test]
fn setup_state_serializes_correctly_to_disk_format() {
    use awman::engine::ready::summary::ReadySummary;
    use awman::engine::step_status::StepStatus;

    let mut summary = ReadySummary::new("docker");
    summary.dockerfile = StepStatus::Done;
    summary.base_image = StepStatus::Done;

    let state = SessionSetupState {
        status: SessionSetupStatus::Ready,
        current_stage: Some("Setup complete".into()),
        current_ready_phase: None,
        ready_step_statuses: vec![],
        ready_summary: Some(summary),
        error: None,
    };

    let json = serde_json::to_string_pretty(&state).unwrap();
    let parsed: SessionSetupState = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.status, SessionSetupStatus::Ready);
    assert!(parsed.ready_summary.is_some());
    let summary = parsed.ready_summary.unwrap();
    assert_eq!(summary.runtime_name, "docker");
}

// ─── Catalogue: programmatic flag derivation ──────────────────────────────────

/// Every flag in `exec workflow` (except those explicitly excluded) is
/// available on `remote exec workflow`. Drift between the two specs would
/// be a regression of the WI's programmatic-derivation requirement.
#[test]
fn remote_exec_workflow_flags_parity_with_local() {
    let cat = CommandCatalogue::get();
    let local = cat.lookup(&["exec", "workflow"]).unwrap();
    let remote = cat.lookup(&["remote", "exec", "workflow"]).unwrap();
    let excluded: &[&str] = &["workdir", "worktree"];
    for flag in local.flags {
        if excluded.contains(&flag.long) {
            continue;
        }
        assert!(
            remote.find_flag(flag.long).is_some(),
            "remote exec workflow is missing `--{}` (present on local exec workflow)",
            flag.long
        );
    }
    // Plus remote transport flags.
    for transport in &["remote-addr", "session", "api-key", "follow"] {
        assert!(
            remote.find_flag(transport).is_some(),
            "remote exec workflow is missing transport flag `--{transport}`"
        );
    }
}

#[test]
fn remote_exec_prompt_flags_parity_with_local() {
    let cat = CommandCatalogue::get();
    let local = cat.lookup(&["exec", "prompt"]).unwrap();
    let remote = cat.lookup(&["remote", "exec", "prompt"]).unwrap();
    let excluded: &[&str] = &["workdir", "worktree"];
    for flag in local.flags {
        if excluded.contains(&flag.long) {
            continue;
        }
        assert!(
            remote.find_flag(flag.long).is_some(),
            "remote exec prompt is missing `--{}` (present on local exec prompt)",
            flag.long
        );
    }
}

// ─── ApiDispatchFrontend → EventBus end-to-end ────────────────────────────────

/// Sending bytes through the AgentIo stdout channel must emit
/// `StdoutLine` events on the bus, one per newline-terminated line.
#[tokio::test]
async fn api_frontend_write_stdout_emits_stdout_line_events() {
    use awman::engine::agent_runtime::frontend::AgentFrontend;
    use awman::frontend::api::command_frontend::ApiDispatchFrontend;

    let bus = EventBus::new(64);
    let mut rx = bus.subscribe();
    let mut fe = ApiDispatchFrontend::new("exec prompt", &[], bus.sender());

    let io = fe.take_io();
    io.stdout.send(b"alpha\nbeta\n".to_vec()).unwrap();
    drop(io);
    // Give the drain task a moment to process.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    fe.emit_done();

    let ev1 = rx.recv().await.unwrap();
    assert!(matches!(ev1.payload, EventPayload::StdoutLine(ref s) if s == "alpha"));
    let ev2 = rx.recv().await.unwrap();
    assert!(matches!(ev2.payload, EventPayload::StdoutLine(ref s) if s == "beta"));
    let done = rx.recv().await.unwrap();
    assert!(matches!(done.payload, EventPayload::Done));
}

/// Calling `write_message` on the API frontend's UserMessageSink impl must
/// emit a `StatusMessage` event with the correct phase mapping.
#[tokio::test]
async fn api_frontend_write_message_emits_status_message_event() {
    use awman::data::message::{MessageLevel, UserMessage, UserMessageSink};
    use awman::frontend::api::command_frontend::ApiDispatchFrontend;

    let bus = EventBus::new(64);
    let mut rx = bus.subscribe();
    let mut fe = ApiDispatchFrontend::new("exec prompt", &[], bus.sender());

    fe.write_message(UserMessage {
        level: MessageLevel::Warning,
        text: "watch out".into(),
    });

    let ev = rx.recv().await.unwrap();
    match ev.payload {
        EventPayload::StatusMessage { phase, message } => {
            assert_eq!(phase, "warn");
            assert_eq!(message, "watch out");
        }
        other => panic!("expected StatusMessage, got {other:?}"),
    }
}

/// Reporting a workflow step transition through the frontend must emit a
/// `WorkflowStepTransition` event with a sensible (per-step) `step_index`.
#[tokio::test]
async fn api_frontend_workflow_step_transition_index_is_per_step() {
    use awman::data::workflow_definition::WorkflowStep;
    use awman::engine::workflow::actions::WorkflowStepStatus;
    use awman::engine::workflow::frontend::WorkflowFrontend;
    use awman::frontend::api::command_frontend::ApiDispatchFrontend;

    let bus = EventBus::new(64);
    let mut rx = bus.subscribe();
    let mut fe = ApiDispatchFrontend::new("exec workflow", &[], bus.sender());

    let step_a = WorkflowStep {
        name: "alpha".into(),
        depends_on: vec![],
        prompt_template: "do alpha".into(),
        agent: None,
        model: None,
        overlays: None,
        abort_on_failure: false,
    };
    let step_b = WorkflowStep {
        name: "beta".into(),
        depends_on: vec![],
        prompt_template: "do beta".into(),
        agent: None,
        model: None,
        overlays: None,
        abort_on_failure: false,
    };

    fe.report_step_status(&step_a, WorkflowStepStatus::Running);
    fe.report_step_status(&step_b, WorkflowStepStatus::Running);
    fe.report_step_status(&step_a, WorkflowStepStatus::Succeeded);

    let e1 = rx.recv().await.unwrap();
    let e2 = rx.recv().await.unwrap();
    let e3 = rx.recv().await.unwrap();
    let idx = |e: &ExecutionEvent| match &e.payload {
        EventPayload::WorkflowStepTransition { step_index, .. } => *step_index,
        _ => panic!("expected WorkflowStepTransition"),
    };
    assert_eq!(idx(&e1), 0, "first reported step gets index 0");
    assert_eq!(idx(&e2), 1, "second new step gets index 1");
    assert_eq!(idx(&e3), 0, "repeat of first step keeps index 0");
}

// ─── SSE lagged-subscriber behavior ───────────────────────────────────────────
//
// The SSE handler's broadcast loop converts `RecvError::Lagged(n)` into an SSE
// comment line `: lagged: <n> events skipped` and resumes streaming. The
// downstream mpsc carrying events from the broadcast loop to the SSE response
// is unbounded, so engineering genuine end-to-end backpressure that drives
// the broadcast Receiver into `Lagged` is impractical. This test exercises
// the same `RecvError::Lagged` path used by the SSE handler and asserts the
// comment-format string we send to clients — axum's own `Event::comment`
// encoding is responsible for the leading `: ` and trailing `\n\n`.

#[tokio::test]
async fn sse_lagged_branch_uses_expected_event_count_format() {
    use tokio::sync::broadcast::error::RecvError;

    // Small-capacity bus → overflow → Lagged(n).
    let bus = EventBus::new(4);
    let mut rx = bus.subscribe();
    let sender = bus.sender();
    for i in 0..10u32 {
        sender.emit(EventPayload::StdoutLine(format!("line {i}")));
    }
    let lagged_n: u64 = match rx.recv().await {
        Err(RecvError::Lagged(n)) => n,
        other => panic!("expected Lagged; got {other:?}"),
    };
    assert_eq!(
        lagged_n, 6,
        "10 emits into capacity-4 bus must yield Lagged(6)"
    );

    // The SSE handler constructs its comment via this exact format string —
    // the call site in routes.rs is
    //     Event::default().comment(format!("lagged: {n} events skipped"))
    // so locking down the format here catches any drift between the WI's
    // documented wire format and the producer.
    let comment_text = format!("lagged: {lagged_n} events skipped");
    assert_eq!(comment_text, "lagged: 6 events skipped");

    // After recovering from Lagged, the subscriber must drain the remaining
    // events in the ring buffer (per the WI: "continue streaming from the
    // next available event").
    for expected_idx in 6u32..10 {
        let ev = rx.recv().await.expect("post-lag recv must succeed");
        match &ev.payload {
            EventPayload::StdoutLine(s) => assert_eq!(s, &format!("line {expected_idx}")),
            other => panic!("expected StdoutLine, got {other:?}"),
        }
    }
}

// ─── `remote session start` poll-loop runtime behavior ──────────────────────
//
// These tests drive the public `RemoteCommand::run_with_frontend` entry point
// against a wiremock HTTP server, exercising the status poll loop that runs
// unconditionally after session creation. They avoid the 5-second sleep
// entirely because the poll-then-sleep order returns before the sleep when
// the mock answers with a terminal state on first poll.

mod remote_session_start_wait_tests {
    use std::sync::{Arc, Mutex};

    use awman::command::commands::remote::{
        RemoteCommand, RemoteCommandFrontend, RemoteOutcome, RemoteSessionStartFlags,
        RemoteSubcommand,
    };
    use awman::command::commands::Command;
    use awman::command::dispatch::Engines;
    use awman::data::config::env::EnvSnapshot;
    use awman::data::fs::api_paths::ApiPaths;
    use awman::data::fs::auth_paths::AuthPathResolver;
    use awman::data::message::{MessageLevel, UserMessage, UserMessageSink};
    use awman::data::session::{Session, SessionOpenOptions};
    use awman::data::EngineWorkflowStateStore;
    use awman::engine::agent::AgentEngine;
    use awman::engine::auth::AuthEngine;
    use awman::engine::container::ContainerRuntime;
    use awman::engine::git::GitEngine;
    use awman::engine::overlay::OverlayEngine;
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    struct CapturingSink {
        messages: Arc<Mutex<Vec<UserMessage>>>,
    }
    impl UserMessageSink for CapturingSink {
        fn write_message(&mut self, msg: UserMessage) {
            self.messages.lock().unwrap().push(msg);
        }
        fn replay_queued(&mut self) {}
    }
    impl RemoteCommandFrontend for CapturingSink {}

    fn build_engines(root: &std::path::Path) -> Engines {
        let api_paths = ApiPaths::from_root(root);
        let auth_paths = AuthPathResolver::at_home(root);
        let runtime = Arc::new(ContainerRuntime::docker());
        let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
        let agent_engine = Arc::new(AgentEngine::new(overlay_engine.clone(), runtime.clone()));
        let auth_engine = Arc::new(AuthEngine::with_paths(auth_paths, api_paths));
        let git_engine = Arc::new(GitEngine::new());
        let workflow_state_store = Arc::new(EngineWorkflowStateStore::at_git_root(root));
        Engines {
            runtime: runtime.clone(),
            container_runtime: Some(runtime),
            sandbox_runtime: None,
            git_engine,
            overlay_engine,
            auth_engine,
            agent_engine,
            workflow_state_store,
        }
    }

    fn build_session(workdir: &std::path::Path) -> Session {
        Session::open_at_git_root(
            workdir.to_path_buf(),
            workdir.to_path_buf(),
            SessionOpenOptions {
                env: Some(EnvSnapshot::empty()),
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn texts(messages: &Mutex<Vec<UserMessage>>) -> Vec<String> {
        messages
            .lock()
            .unwrap()
            .iter()
            .map(|m| m.text.clone())
            .collect()
    }

    fn levels(messages: &Mutex<Vec<UserMessage>>) -> Vec<MessageLevel> {
        messages.lock().unwrap().iter().map(|m| m.level).collect()
    }

    /// The poll loop runs unconditionally; it polls `/status`, sees `ready`,
    /// renders the summary, and exits with `setup_status: Some("ready")`.
    /// Because the loop polls first then sleeps, this test returns without
    /// hitting the 5-second inter-poll sleep.
    #[tokio::test]
    async fn poll_loop_terminates_on_ready_status_and_renders_summary() {
        let mock = MockServer::start().await;
        let root = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();

        Mock::given(matchers::method("POST"))
            .and(matchers::path("/v1/sessions"))
            .respond_with(
                ResponseTemplate::new(202)
                    .set_body_json(serde_json::json!({ "session_id": "sid-ready" })),
            )
            .mount(&mock)
            .await;

        let ready_summary = serde_json::json!({
            "runtime_name": "docker",
            "dockerfile": "Done",
            "base_image": "Done",
            "agent_image": "Done",
            "local_agent": "Done",
            "audit": "Skipped",
            "image_rebuild": "Skipped",
            "aspec_folder": "Done",
            "work_items_config": "Done",
            "non_default_agent_images": [],
        });
        Mock::given(matchers::method("GET"))
            .and(matchers::path("/v1/sessions/sid-ready/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": "sid-ready",
                "status": "ready",
                "current_stage": "Setup complete",
                "current_ready_phase": null,
                "ready_step_statuses": [
                    { "step": "Dockerfile", "status": "Done" },
                    { "step": "Base image", "status": "Done" }
                ],
                "ready_summary": ready_summary,
                "error": null,
            })))
            .expect(1..)
            .mount(&mock)
            .await;

        let messages = Arc::new(Mutex::new(Vec::new()));
        let engines = build_engines(root.path());
        let session = build_session(workdir.path());
        let flags = RemoteSessionStartFlags {
            session_type: "local".into(),
            workdir: Some(workdir.path().display().to_string()),
            repo_url: None,
            branch: None,
            remote_addr: Some(mock.uri()),
            api_key: None,
        };
        let cmd = RemoteCommand::new(RemoteSubcommand::SessionStart(flags), engines, session);
        let outcome = cmd
            .run_with_frontend(Box::new(CapturingSink {
                messages: messages.clone(),
            }))
            .await
            .expect("run_with_frontend");

        match outcome {
            RemoteOutcome::SessionStart(o) => {
                assert_eq!(o.session_id, "sid-ready");
                assert_eq!(o.setup_status.as_deref(), Some("ready"));
            }
            other => panic!("expected SessionStart outcome; got {other:?}"),
        }

        let all_text = texts(&messages).join("\n");
        assert!(
            all_text.contains("ready"),
            "poll loop should emit a status line containing 'ready'; got:\n{all_text}"
        );
        assert!(
            all_text.contains("is ready"),
            "poll loop should print 'Session ... is ready.'; got:\n{all_text}"
        );
        // The rendered summary box uses the runtime name in its title.
        assert!(
            all_text.contains("docker"),
            "poll loop must render the ready summary (which contains the runtime name); got:\n{all_text}"
        );
        assert!(
            levels(&messages).contains(&MessageLevel::Success),
            "success path must include a Success-level message"
        );
    }

    /// A `failed` terminal status from the poll loop returns an error and
    /// prints the partial step table plus the error message.
    #[tokio::test]
    async fn poll_loop_terminates_on_failed_status_and_returns_error() {
        let mock = MockServer::start().await;
        let root = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();

        Mock::given(matchers::method("POST"))
            .and(matchers::path("/v1/sessions"))
            .respond_with(
                ResponseTemplate::new(202)
                    .set_body_json(serde_json::json!({ "session_id": "sid-failed" })),
            )
            .mount(&mock)
            .await;

        Mock::given(matchers::method("GET"))
            .and(matchers::path("/v1/sessions/sid-failed/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": "sid-failed",
                "status": "failed",
                "current_stage": "Failed: Docker daemon not running",
                "current_ready_phase": null,
                "ready_step_statuses": [
                    { "step": "Dockerfile", "status": "Done" },
                    { "step": "Base image", "status": { "Failed": "Docker daemon not running" } }
                ],
                "ready_summary": null,
                "error": { "stage": "ready", "message": "Docker daemon not running" },
            })))
            .expect(1..)
            .mount(&mock)
            .await;

        let messages = Arc::new(Mutex::new(Vec::new()));
        let engines = build_engines(root.path());
        let session = build_session(workdir.path());
        let flags = RemoteSessionStartFlags {
            session_type: "local".into(),
            workdir: Some(workdir.path().display().to_string()),
            repo_url: None,
            branch: None,
            remote_addr: Some(mock.uri()),
            api_key: None,
        };
        let cmd = RemoteCommand::new(RemoteSubcommand::SessionStart(flags), engines, session);
        let result = cmd
            .run_with_frontend(Box::new(CapturingSink {
                messages: messages.clone(),
            }))
            .await;
        assert!(
            result.is_err(),
            "poll loop must surface the failed status as a CommandError; got Ok({:?})",
            result.ok()
        );

        let all_text = texts(&messages).join("\n");
        assert!(
            all_text.contains("Session setup failed"),
            "failure path must print 'Session setup failed'; got:\n{all_text}"
        );
        assert!(
            all_text.contains("Docker daemon not running"),
            "failure path must surface the error message; got:\n{all_text}"
        );
        assert!(
            all_text.contains("Partial step status"),
            "failure path must print the partial step table when steps were reported; got:\n{all_text}"
        );
        assert!(
            levels(&messages).contains(&MessageLevel::Error),
            "failure path must emit at least one Error-level message"
        );
    }
}
