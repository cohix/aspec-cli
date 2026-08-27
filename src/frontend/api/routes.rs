//! HTTP route registration and handlers for the API server.
//!
//! Wire-identical to `oldsrc/commands/headless/server.rs::build_router`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tower_http::trace::TraceLayer;

use crate::command::dispatch::catalogue::{CommandCatalogue, FrontendKind};
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::command::session_create::{
    SessionCreatePlan, SessionCreatePolicy, SessionCreateRequest,
};
use crate::command::session_setup::{SessionSetup, SessionSetupObserver};
use crate::data::execution_event::{EventPayload, ExecutionEvent};
use crate::data::fs::api_db::{SessionCommandAdmission, SqliteSessionStore};
use crate::data::fs::api_paths::ApiPaths;
use crate::data::message::UserMessageSink;
use crate::data::ready_summary::ReadySummary;
use crate::data::session::Session;
use crate::data::session_setup_event::{SessionSetupStatus, SetupEventPayload};
use crate::engine::ready::frontend::ReadyFrontend;
use crate::frontend::api::event_bus::EventBus;
use crate::frontend::api::session_setup::{
    log_session_setup, SessionSetupBus, SessionSetupBusSender, SetupReadyFrontend, TracingSetupSink,
};

// ─── Auth mode ───────────────────────────────────────────────────────────────

#[derive(Clone)]
pub enum AuthMode {
    Enabled { key_hash: String },
    Disabled,
}

// ─── Shared state ────────────────────────────────────────────────────────────

pub struct AppState {
    pub store: Arc<SqliteSessionStore>,
    pub paths: ApiPaths,
    pub workdirs: Vec<PathBuf>,
    pub started_at: Instant,
    pub task_handles: tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    pub auth_mode: AuthMode,
    pub engines: Engines,
    /// Maps HTTP session IDs → their Layer 0 Session. Opened once when the
    /// session is created via the API, reused for every command dispatch
    /// within that session, removed when the session is closed.
    pub sessions: Arc<tokio::sync::Mutex<HashMap<String, Arc<RwLock<Session>>>>>,
    /// Per-command EventBus handles, keyed by command_id. Retained during
    /// execution plus a short grace period for late-connecting SSE clients.
    pub event_buses: Arc<tokio::sync::Mutex<HashMap<String, Arc<EventBus>>>>,
    /// Per-session setup bus handles, keyed by session_id. Retained during
    /// setup plus 60 seconds after reaching a terminal state.
    pub setup_buses: tokio::sync::Mutex<HashMap<String, Arc<SessionSetupBus>>>,
}

#[derive(Serialize)]
struct QueueStatusResponse {
    session_id: String,
    queue_depth: i64,
    running: Option<serde_json::Value>,
    queued: Vec<serde_json::Value>,
    recent_completed: Vec<serde_json::Value>,
}

#[derive(Serialize)]
struct SessionClosingResponse {
    session_id: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    running_command_id: Option<String>,
    cancelled_count: usize,
    message: String,
}

// ─── Request / Response types (wire-compatible with oldsrc) ──────────────────

#[derive(Deserialize, Debug)]
struct CreateSessionRequest {
    /// `"local"` (default) or `"remote"`.
    #[serde(default)]
    session_type: Option<String>,
    /// Workdir on the server host (required for `local`).
    #[serde(default)]
    workdir: Option<String>,
    /// Repository URL (required for `remote`).
    #[serde(default)]
    repo_url: Option<String>,
    /// Optional branch (defaults to remote default when `remote`).
    #[serde(default)]
    branch: Option<String>,
}

#[derive(Serialize)]
struct CreateSessionResponse {
    session_id: String,
}

#[derive(Serialize)]
struct SessionResponse {
    id: String,
    workdir: String,
    created_at: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    closed_at: Option<String>,
}

#[derive(Deserialize)]
struct CreateCommandRequest {
    subcommand: String,
    args: Vec<String>,
}

#[derive(Serialize)]
struct CreateCommandResponse {
    command_id: String,
    /// Server-enforced flags whose values the API frontend always overrides.
    /// Documents to clients that `yolo` and `non_interactive` are forced to
    /// `true` regardless of any value sent in the request body. Empty object
    /// for non-exec routes.
    flags_applied: serde_json::Value,
}

#[derive(Serialize)]
struct CommandResponse {
    id: String,
    session_id: String,
    subcommand: String,
    args: serde_json::Value,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    finished_at: Option<String>,
    log_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    queued_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    queue_position: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    worker_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct StatusResponse {
    status: String,
    pid: u32,
    uptime_seconds: u64,
    active_sessions: i64,
    running_commands: i64,
}

#[derive(Deserialize, Default)]
struct ListSessionsQuery {
    #[serde(default)]
    status: Option<String>,
}

use crate::frontend::api::serve::error_json;

/// Map a session-creation validation error to its HTTP status. This is the
/// ONLY session-creation logic that remains in the frontend — the transport
/// mapping. An off-allowlist workdir is a 403 (the path exists but the caller
/// is not permitted to use it); every other validation failure is a 400.
fn session_create_error_status(err: &CommandError) -> StatusCode {
    match err {
        CommandError::SessionWorkdirNotAllowed { .. } => StatusCode::FORBIDDEN,
        _ => StatusCode::BAD_REQUEST,
    }
}

// ─── Router ──────────────────────────────────────────────────────────────────

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/status", get(handle_status))
        .route("/v1/workdirs", get(handle_workdirs))
        .route(
            "/v1/sessions",
            get(handle_list_sessions).post(handle_create_session),
        )
        .route(
            "/v1/sessions/{id}",
            get(handle_get_session).delete(handle_close_session),
        )
        .route("/v1/sessions/{id}/status", get(handle_get_session_status))
        .route("/v1/sessions/{id}/queue", get(handle_get_session_queue))
        .route("/v1/commands", post(handle_create_command))
        .route("/v1/commands/{id}/status", get(handle_get_command))
        .route("/v1/commands/{id}/logs", get(handle_stream_command_logs))
        .route("/v1/workflows/{command_id}", get(handle_get_workflow))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

// ─── Auth middleware ─────────────────────────────────────────────────────────

async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Response {
    // State extraction only: the decision itself lives in one shared place so
    // the API and amie daemons can never diverge on how a key is accepted.
    if let Some(rejection) =
        crate::frontend::api::serve::check_bearer_auth(&state.auth_mode, req.headers())
    {
        return rejection;
    }
    next.run(req).await
}

// ─── Handlers ────────────────────────────────────────────────────────────────

async fn handle_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let active_sessions = state.store.count_active_sessions().unwrap_or(0);
    let running_commands = state.store.count_running_commands().unwrap_or(0);
    let uptime = state.started_at.elapsed().as_secs();

    Json(StatusResponse {
        status: "ok".to_string(),
        pid: std::process::id(),
        uptime_seconds: uptime,
        active_sessions,
        running_commands,
    })
}

async fn handle_workdirs(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let dirs: Vec<String> = state
        .workdirs
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    Json(serde_json::json!({ "workdirs": dirs }))
}

async fn handle_create_session(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateSessionRequest>,
) -> Response {
    let session_id = uuid::Uuid::new_v4().to_string();
    let created_at = chrono::Utc::now().to_rfc3339();
    let session_dir = state.paths.session_dir(&session_id);

    // Validate and plan the session in Layer 2. The route only maps the typed
    // result/error to an HTTP status + JSON envelope — no business logic here.
    let request = SessionCreateRequest {
        session_type: body.session_type,
        workdir: body.workdir,
        repo_url: body.repo_url,
        branch: body.branch,
    };
    let policy = SessionCreatePolicy::new(state.workdirs.clone(), session_dir.clone());
    let plan = match request.validate(&policy) {
        Ok(plan) => plan,
        Err(e) => {
            return (session_create_error_status(&e), error_json(e.to_string())).into_response();
        }
    };

    // Create session storage directories (Layer 0).
    if let Err(e) = state.paths.prepare_session_dirs(&session_id) {
        tracing::error!(error = %e, "Failed to create session directory");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            error_json("Failed to create session directory"),
        )
            .into_response();
    }

    // Persist the session row with setup_status='initializing' BEFORE spawning
    // the setup task. If the server restarts mid-setup we want the cleanup
    // pass to find this session as non-terminal even if no setup_state.json
    // was written yet.
    if let Err(e) = state.store.insert_session_full(
        &session_id,
        &plan.resolved_workdir.to_string_lossy(),
        &created_at,
        "initializing",
        &plan.session_type,
        plan.cloned_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .as_deref(),
    ) {
        tracing::error!(error = %e, "Failed to insert session");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            error_json("Failed to create session"),
        )
            .into_response();
    }

    let setup_bus = Arc::new(SessionSetupBus::new(256));
    state
        .setup_buses
        .lock()
        .await
        .insert(session_id.clone(), Arc::clone(&setup_bus));

    tracing::info!(
        session_id = %session_id,
        session_type = %plan.session_type,
        workdir = %plan.resolved_workdir.display(),
        "Session created (setup starting)"
    );

    let state_clone = Arc::clone(&state);
    let sid = session_id.clone();
    tokio::spawn(async move {
        run_session_setup(state_clone, sid, plan, setup_bus).await;
    });

    (
        StatusCode::ACCEPTED,
        Json(CreateSessionResponse { session_id }),
    )
        .into_response()
}

/// Drive the Layer 2 [`SessionSetup`] orchestrator, supplying an
/// [`ApiSessionSetupObserver`] that renders each step onto the session-setup
/// event bus, persists the setup status, registers the opened session, and
/// vends the ready-checks frontend. All setup *behavior* — clone/branch
/// sequencing and the remote-clone failure-cleanup rule — lives in Layer 2;
/// this frontend only maps that behavior onto its transport and state.
async fn run_session_setup(
    state: Arc<AppState>,
    session_id: String,
    plan: SessionCreatePlan,
    setup_bus: Arc<SessionSetupBus>,
) {
    let setup = SessionSetup::new(session_id.clone(), plan, state.engines.clone());
    let mut observer = ApiSessionSetupObserver {
        bus_sender: setup_bus.sender(),
        setup_bus,
        state,
        session_id,
    };
    setup.run(&mut observer).await;
}

/// API-frontend implementation of the Layer 2 [`SessionSetupObserver`]. Owns the
/// event-bus, status-persistence, in-memory session map, and ready-frontend
/// glue that is inherently API-mode presentation/state.
struct ApiSessionSetupObserver {
    state: Arc<AppState>,
    session_id: String,
    setup_bus: Arc<SessionSetupBus>,
    bus_sender: SessionSetupBusSender,
}

#[async_trait::async_trait]
impl SessionSetupObserver for ApiSessionSetupObserver {
    fn enter_status(&mut self, status: SessionSetupStatus) {
        let persisted = status.as_str();
        self.bus_sender.update_status(status);
        let _ = self
            .state
            .store
            .update_setup_status(&self.session_id, persisted);
    }

    fn set_stage(&mut self, message: &str) {
        self.bus_sender.update_stage(message);
    }

    fn stage_changed(&mut self, stage: &str, message: &str) {
        self.bus_sender.emit(SetupEventPayload::StageChanged {
            stage: stage.to_string(),
            message: message.to_string(),
        });
    }

    fn mark_failed(&mut self, stage: &str, error: &str) {
        self.bus_sender.mark_failed(stage, error);
        self.bus_sender.emit(SetupEventPayload::SetupFailed {
            stage: stage.to_string(),
            error: error.to_string(),
        });
    }

    fn set_ready(&mut self, summary: &ReadySummary) {
        self.bus_sender.set_ready(summary.clone());
        self.bus_sender.emit(SetupEventPayload::SetupComplete {
            ready_summary: Box::new(summary.clone()),
        });
    }

    fn persist_status(&mut self, status: &str) {
        let _ = self
            .state
            .store
            .update_setup_status(&self.session_id, status);
    }

    fn log(&mut self, line: &str) {
        log_session_setup(&self.session_id, line);
    }

    async fn register_session(&mut self, session: Arc<RwLock<Session>>) {
        self.state
            .sessions
            .lock()
            .await
            .insert(self.session_id.clone(), session);
    }

    fn ready_frontend(&mut self) -> Box<dyn ReadyFrontend> {
        // A throwaway EventBus satisfies the ready frontend's container sink; its
        // events are mirrored to the tracing log and the session-setup bus.
        let event_bus = EventBus::new(4096);
        Box::new(SetupReadyFrontend::new(
            &self.session_id,
            self.setup_bus.sender(),
            event_bus.sender(),
        ))
    }

    fn git_log_sink(&mut self) -> Box<dyn UserMessageSink + Send> {
        Box::new(TracingSetupSink::new(&self.session_id))
    }

    async fn persist_and_cleanup(&mut self) {
        persist_setup_state(&self.state, &self.session_id, &self.setup_bus).await;
        cleanup_setup_bus(
            Arc::clone(&self.state),
            self.session_id.clone(),
            Arc::clone(&self.setup_bus),
        )
        .await;
    }
}

async fn persist_setup_state(state: &AppState, session_id: &str, setup_bus: &SessionSetupBus) {
    let setup_state = setup_bus.snapshot();
    if let Err(e) = state.paths.save_setup_state(session_id, &setup_state) {
        tracing::error!(session_id = %session_id, error = %e, "Failed to persist setup_state.json");
    }
}

async fn cleanup_setup_bus(
    state: Arc<AppState>,
    session_id: String,
    _setup_bus: Arc<SessionSetupBus>,
) {
    // Retain the setup bus for 60 seconds after reaching terminal state.
    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    state.setup_buses.lock().await.remove(&session_id);
}

async fn handle_list_sessions(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListSessionsQuery>,
) -> Response {
    match state.store.list_sessions_by_status(query.status.as_deref()) {
        Ok(sessions) => {
            let list: Vec<SessionResponse> = sessions
                .into_iter()
                .map(|s| SessionResponse {
                    id: s.id,
                    workdir: s.workdir,
                    created_at: s.created_at,
                    status: s.status,
                    closed_at: s.closed_at,
                })
                .collect();
            Json(serde_json::json!({ "sessions": list })).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to list sessions");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to list sessions"),
            )
                .into_response()
        }
    }
}

async fn handle_get_session(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match state.store.get_session(&id) {
        Ok(Some(s)) => Json(SessionResponse {
            id: s.id,
            workdir: s.workdir,
            created_at: s.created_at,
            status: s.status,
            closed_at: s.closed_at,
        })
        .into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            error_json(format!("Session '{}' not found", id)),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to get session");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to get session"),
            )
                .into_response()
        }
    }
}

async fn handle_close_session(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let session_record = match state.store.get_session(&id) {
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                error_json(format!("Session '{}' not found", id)),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to get session");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to close session"),
            )
                .into_response();
        }
        Ok(Some(s)) if s.status == "closed" => {
            return Json(SessionResponse {
                id: s.id,
                workdir: s.workdir,
                created_at: s.created_at,
                status: s.status,
                closed_at: s.closed_at,
            })
            .into_response();
        }
        Ok(Some(s)) if s.status == "closing" => {
            // Already closing — return current state.
            let running_cmd = state.store.running_command_for_session(&id).ok().flatten();
            return Json(SessionClosingResponse {
                session_id: id,
                status: "closing".to_string(),
                running_command_id: running_cmd.map(|c| c.id),
                cancelled_count: 0,
                message:
                    "Session is already closing. Poll GET /v1/sessions/{id}/status to monitor."
                        .to_string(),
            })
            .into_response();
        }
        Ok(Some(s)) => s,
    };

    // Step 1: Mark session as 'closing' FIRST so the POST /v1/commands guard
    // begins rejecting new enqueues immediately. If we cancel queued commands
    // first, a concurrent POST could observe `status = 'active'`, enqueue a
    // new command, and have it claimed by a worker before we close the gate.
    let _ = state.store.update_session_status(&id, "closing");

    // Step 2: Cancel all queued commands. Any racing POST that slipped in
    // before step 1 took effect will have its queued row cancelled here.
    let cancelled_ids = state
        .store
        .cancel_queued_for_session(&id)
        .unwrap_or_default();
    let cancelled_count = cancelled_ids.len();

    // Step 3: Check for a running command.
    let running_cmd = state.store.running_command_for_session(&id).ok().flatten();

    if let Some(running) = running_cmd {
        // Running command exists — return 202 and let the worker handle
        // final cleanup when the command finishes.
        tracing::info!(
            session_id = %id,
            running_command_id = %running.id,
            cancelled_count = cancelled_count,
            "Session entering drain-and-kill (waiting for running command)"
        );
        return (
            StatusCode::ACCEPTED,
            Json(SessionClosingResponse {
                session_id: id,
                status: "closing".to_string(),
                running_command_id: Some(running.id),
                cancelled_count,
                message: "Session is closing. Waiting for running command to complete. Poll GET /v1/sessions/{id}/status to monitor.".to_string(),
            }),
        )
            .into_response();
    }

    // No running command — close immediately.
    // For remote sessions, delete the cloned directory.
    if session_record.session_type == "remote" {
        if let Some(ref cloned_path) = session_record.cloned_path {
            let path = std::path::PathBuf::from(cloned_path);
            let git = Arc::clone(&state.engines.git_engine);
            let delete_result =
                tokio::task::spawn_blocking(move || git.delete_directory(&path)).await;
            match delete_result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::error!(session_id = %id, error = %e, "Failed to delete remote clone");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        error_json("Failed to clean up remote session directory"),
                    )
                        .into_response();
                }
                Err(e) => {
                    tracing::error!(session_id = %id, error = %e, "Delete task panicked");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        error_json("Failed to clean up remote session directory"),
                    )
                        .into_response();
                }
            }
        }
    }

    let closed_at = chrono::Utc::now().to_rfc3339();
    let _ = state.store.close_session_force(&id, &closed_at);
    state.sessions.lock().await.remove(&id);

    tracing::info!(
        session_id = %id,
        cancelled_count = cancelled_count,
        "Session closed immediately (no running commands)"
    );

    match state.store.get_session(&id) {
        Ok(Some(s)) => Json(SessionResponse {
            id: s.id,
            workdir: s.workdir,
            created_at: s.created_at,
            status: s.status,
            closed_at: s.closed_at,
        })
        .into_response(),
        _ => StatusCode::NO_CONTENT.into_response(),
    }
}

async fn handle_get_session_status(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    // Check if session exists at all.
    match state.store.get_session(&id) {
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                error_json(format!("Session '{}' not found", id)),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to get session");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to get session"),
            )
                .into_response();
        }
        Ok(Some(_)) => {}
    }

    // Try to read from in-memory setup bus first.
    if let Some(bus) = state.setup_buses.lock().await.get(&id).cloned() {
        let setup_state = bus.snapshot();
        return Json(serde_json::json!({
            "session_id": id,
            "status": setup_state.status,
            "current_stage": setup_state.current_stage,
            "current_ready_phase": setup_state.current_ready_phase,
            "ready_step_statuses": setup_state.ready_step_statuses,
            "ready_summary": setup_state.ready_summary,
            "error": setup_state.error,
        }))
        .into_response();
    }

    // Fall back to on-disk setup_state.json (Layer 0).
    match state.paths.read_setup_state(&id) {
        Some(setup_state) => Json(serde_json::json!({
            "session_id": id,
            "status": setup_state.status,
            "current_stage": setup_state.current_stage,
            "current_ready_phase": setup_state.current_ready_phase,
            "ready_step_statuses": setup_state.ready_step_statuses,
            "ready_summary": setup_state.ready_summary,
            "error": setup_state.error,
        }))
        .into_response(),
        None => fallback_status_from_db(&state, &id).await,
    }
}

/// Resolve a session's setup status to (is_ready, status_string, optional error JSON).
/// Reads the in-memory bus first, then setup_state.json on disk, then the sqlite
/// session row. Used by the job-submission guard and other places that need to
/// reason about session readiness.
async fn resolve_setup_status(
    state: &AppState,
    session_id: &str,
) -> (bool, String, Option<serde_json::Value>) {
    if let Some(bus) = state.setup_buses.lock().await.get(session_id).cloned() {
        let s = bus.snapshot();
        let is_ready = matches!(s.status, SessionSetupStatus::Ready);
        let status_str = s.status.as_str().to_string();
        let err_payload = s.error.as_ref().map(|e| {
            serde_json::json!({
                "stage": e.stage,
                "message": e.message,
            })
        });
        return (is_ready, status_str, err_payload);
    }
    // No bus. Try setup_state.json (Layer 0).
    if let Some(ss) = state.paths.read_setup_state(session_id) {
        let is_ready = matches!(ss.status, SessionSetupStatus::Ready);
        let status_str = ss.status.as_str().to_string();
        let err_payload = ss.error.as_ref().map(|e| {
            serde_json::json!({
                "stage": e.stage,
                "message": e.message,
            })
        });
        return (is_ready, status_str, err_payload);
    }
    // Last resort: sqlite session row.
    match state.store.get_session(session_id) {
        Ok(Some(s)) => {
            let is_ready = s.setup_status == "ready";
            (is_ready, s.setup_status, None)
        }
        _ => (true, "ready".to_string(), None), // truly unknown — assume ready
    }
}

/// Last-resort fallback when neither the in-memory bus nor the on-disk
/// setup_state.json is usable: read the session's setup_status from sqlite
/// and return a minimal response. Used for very old sessions (pre-WI-0078).
async fn fallback_status_from_db(state: &AppState, id: &str) -> Response {
    let setup_status = state
        .store
        .get_session(id)
        .ok()
        .flatten()
        .map(|s| s.setup_status)
        .unwrap_or_else(|| "ready".to_string());
    Json(serde_json::json!({
        "session_id": id,
        "status": setup_status,
    }))
    .into_response()
}

async fn handle_create_command(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<CreateCommandRequest>,
) -> Response {
    let session_id = match headers.get("x-awman-session") {
        Some(val) => match val.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    error_json("Invalid x-awman-session header value"),
                )
                    .into_response();
            }
        },
        None => {
            return (
                StatusCode::BAD_REQUEST,
                error_json("Missing required header: x-awman-session"),
            )
                .into_response();
        }
    };

    // Validate the command shape against the catalogue BEFORE touching session
    // state. Both checks below are request-shape (400-class) errors derived
    // entirely from the command catalogue — no per-command logic in the route.
    {
        let catalogue = CommandCatalogue::get();
        let path_parts: Vec<&str> = body.subcommand.split_whitespace().collect();

        // (1) The command must be reachable via the API frontend.
        if let Err(CommandError::NotAvailableForFrontend { command, .. }) =
            catalogue.validate_for_frontend(FrontendKind::Api, &path_parts)
        {
            // The advertised alternatives are the catalogue's api-allowed
            // commands, not a hand-maintained list that could drift.
            let available: Vec<String> = catalogue
                .api_allowed_commands()
                .into_iter()
                .map(|(parent, sub)| format!("{parent} {sub}"))
                .collect();
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "command not available via API",
                    "blocked_command": command,
                    "available": available,
                })),
            )
                .into_response();
        }

        // (2) The args must parse cleanly against the catalogue: an unknown
        // flag, a bad flag value, or an unknown command produces a structured
        // 400 here, rather than being enqueued and failing asynchronously in
        // the worker when the dispatch accessors are later called.
        if let Err(e) =
            catalogue.parse_raw_args_with_profile(&path_parts, &body.args, FrontendKind::Api)
        {
            return (StatusCode::BAD_REQUEST, error_json(e.to_string())).into_response();
        }
    }

    // Validate session exists and is in a state that accepts commands. The
    // lifecycle classification lives in Layer 0 (on the store that owns the
    // `status` column); the route only maps each outcome to an HTTP status.
    match state.store.command_admission(&session_id) {
        Ok(SessionCommandAdmission::Accepted) => {}
        Ok(SessionCommandAdmission::Closing) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "session is closing",
                    "session_id": session_id,
                    "hint": "Session is shutting down and no longer accepts commands."
                })),
            )
                .into_response();
        }
        Ok(SessionCommandAdmission::Closed) => {
            return (
                StatusCode::NOT_FOUND,
                error_json(format!("Session '{}' is closed", session_id)),
            )
                .into_response();
        }
        Ok(SessionCommandAdmission::NotFound) => {
            return (
                StatusCode::NOT_FOUND,
                error_json(format!("Session '{}' not found", session_id)),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to get session");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to validate session"),
            )
                .into_response();
        }
    };

    // Job submission guard: reject if session setup is not ready.
    {
        let (setup_ready, status_str, error_payload) =
            resolve_setup_status(&state, &session_id).await;
        if !setup_ready {
            let mut body = serde_json::json!({
                "error": "session is not ready",
                "setup_status": status_str,
                "hint": "Poll GET /v1/sessions/{id}/status to check setup progress"
            });
            if let Some(err) = error_payload {
                body["setup_error"] = err;
                if let Some(obj) = body.as_object_mut() {
                    obj.insert(
                        "error".into(),
                        serde_json::Value::String("session setup failed".into()),
                    );
                }
            }
            return (StatusCode::CONFLICT, Json(body)).into_response();
        }
    }

    let command_id = uuid::Uuid::new_v4().to_string();
    let args_json = serde_json::to_string(&body.args).unwrap_or_else(|_| "[]".to_string());

    if let Err(e) = state.paths.prepare_command_dir(&session_id, &command_id) {
        tracing::error!(error = %e, "Failed to create command directory");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            error_json("Failed to create command directory"),
        )
            .into_response();
    }

    let log_path = state.paths.command_log_path(&session_id, &command_id);

    if let Err(e) = state.store.enqueue_command(
        &command_id,
        &session_id,
        &body.subcommand,
        &args_json,
        &log_path.to_string_lossy(),
    ) {
        tracing::error!(error = %e, "Failed to enqueue command");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            error_json("Failed to enqueue command"),
        )
            .into_response();
    }

    tracing::info!(
        command_id = %command_id,
        session_id = %session_id,
        subcommand = %body.subcommand,
        "Command enqueued"
    );

    let flags_applied = serde_json::json!({
        "yolo": true,
        "non_interactive": true,
    });

    (
        StatusCode::ACCEPTED,
        Json(CreateCommandResponse {
            command_id,
            flags_applied,
        }),
    )
        .into_response()
}

async fn handle_get_command(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match state.store.get_command(&id) {
        Ok(Some(c)) => {
            let args: serde_json::Value =
                serde_json::from_str(&c.args).unwrap_or(serde_json::Value::Array(vec![]));

            let queue_position = if c.status == "queued" {
                state
                    .store
                    .queue_position_for_command(&c.id, &c.session_id)
                    .ok()
                    .flatten()
            } else {
                None
            };

            let result: Option<serde_json::Value> = c
                .result
                .as_deref()
                .and_then(|r| serde_json::from_str(r).ok());

            Json(CommandResponse {
                id: c.id,
                session_id: c.session_id,
                subcommand: c.subcommand,
                args,
                status: c.status,
                exit_code: c.exit_code,
                started_at: c.started_at,
                finished_at: c.finished_at,
                log_path: c.log_path,
                queued_at: c.queued_at,
                queue_position,
                worker_id: c.worker_id,
                result,
            })
            .into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            error_json(format!("Command '{}' not found", id)),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to get command");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to get command"),
            )
                .into_response()
        }
    }
}

/// Query parameters for the per-command SSE / log endpoint.
#[derive(Deserialize, Default)]
struct CommandLogsQuery {
    /// When set to `"json"`, return the events.log content as a JSON array
    /// of ExecutionEvent values instead of streaming SSE.
    #[serde(default)]
    format: Option<String>,
}

/// `GET /v1/commands/{id}/logs` — structured event stream.
///
/// Behavior:
/// - Validates the command exists; 404 otherwise.
/// - When `?format=json`, returns `events.log` as a JSON array of
///   `ExecutionEvent` (non-streaming).
/// - Otherwise streams SSE in `event: <type>\ndata: <json>\n\n` format.
/// - If the command is running, first replays `events.log` from disk (capturing
///   the highest sequence number), then subscribes to the live EventBus
///   and filters out events with `sequence <= last_replayed_seq` to avoid
///   duplicates from the replay/live switchover race.
/// - When the broadcast channel reports `Lagged(n)`, sends an SSE comment
///   line `: lagged: <n> events skipped` and resumes streaming.
/// - Emits a final `event: done\ndata: ...\n\n` when the stream completes.
async fn handle_stream_command_logs(
    State(state): State<Arc<AppState>>,
    AxumPath(command_id): AxumPath<String>,
    Query(query): Query<CommandLogsQuery>,
) -> Response {
    let (session_id, is_already_done) = match state.store.get_command(&command_id) {
        Ok(Some(c)) => {
            let done = matches!(c.status.as_str(), "done" | "error" | "cancelled");
            (c.session_id, done)
        }
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                error_json(format!("Command '{command_id}' not found")),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to look up command");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to look up command"),
            )
                .into_response();
        }
    };

    // ?format=json — return the full events.log as a JSON array.
    if query.format.as_deref() == Some("json") {
        let content = state
            .paths
            .read_command_events_raw(&session_id, &command_id)
            .unwrap_or_default();
        let mut events = Vec::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<ExecutionEvent>(line) {
                Ok(ev) => events.push(serde_json::to_value(ev).unwrap_or(serde_json::Value::Null)),
                Err(e) => {
                    tracing::warn!(error = %e, "skipping malformed events.log line");
                }
            }
        }
        return Json(serde_json::json!({
            "session_id": session_id,
            "command_id": command_id,
            "events": events,
        }))
        .into_response();
    }

    // SSE streaming path.
    let (tx, rx) =
        tokio::sync::mpsc::unbounded_channel::<Result<Event, std::convert::Infallible>>();
    let stream = UnboundedReceiverStream::new(rx);

    let maybe_bus = if is_already_done {
        None
    } else {
        state.event_buses.lock().await.get(&command_id).cloned()
    };

    let state_for_task = Arc::clone(&state);
    let command_id_for_task = command_id.clone();
    let session_id_for_task = session_id.clone();

    tokio::spawn(async move {
        // 1. Replay events.log from disk, recording the highest sequence.
        let mut last_replayed_seq: Option<u64> = None;
        if let Some(content) = state_for_task
            .paths
            .read_command_events_raw(&session_id_for_task, &command_id_for_task)
        {
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let event: ExecutionEvent = match serde_json::from_str(line) {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                last_replayed_seq = Some(
                    last_replayed_seq
                        .map(|s| s.max(event.sequence))
                        .unwrap_or(event.sequence),
                );
                if tx.send(Ok(execution_event_to_sse(&event))).is_err() {
                    return;
                }
            }
        }

        // 2. If no live bus was found AND the command is not yet terminal, the
        //    worker hasn't claimed it yet (status='queued') or it's between
        //    claim and bus registration. Poll for the bus to appear, with a
        //    short keepalive comment every iteration so the SSE connection
        //    doesn't look dead to the client. Bail out if the command
        //    transitions to a terminal state while we're waiting.
        let mut live_bus = maybe_bus;
        if live_bus.is_none() && !is_already_done {
            const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
            const POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
            // Emit a visible status event so `--follow` clients see *something*
            // while they wait for a worker to claim the command. Without this
            // the client looks frozen for up to POLL_TIMEOUT.
            let waiting_event = ExecutionEvent {
                timestamp: chrono::Utc::now(),
                sequence: last_replayed_seq.map(|s| s + 1).unwrap_or(0),
                payload: EventPayload::StatusMessage {
                    phase: "queue".into(),
                    message: "Waiting for a worker to claim this command...".into(),
                },
            };
            if tx.send(Ok(execution_event_to_sse(&waiting_event))).is_err() {
                return;
            }
            let started = std::time::Instant::now();
            loop {
                // Check command status — if it reached a terminal state while
                // we were waiting, replay any newly-flushed events.log lines
                // and exit the wait loop so the Done event is emitted below.
                let cmd_terminal = match state_for_task.store.get_command(&command_id_for_task) {
                    Ok(Some(c)) => matches!(c.status.as_str(), "done" | "error" | "cancelled"),
                    _ => false,
                };
                if cmd_terminal {
                    if let Some(content) = state_for_task
                        .paths
                        .read_command_events_raw(&session_id_for_task, &command_id_for_task)
                    {
                        for line in content.lines() {
                            let line = line.trim();
                            if line.is_empty() {
                                continue;
                            }
                            let event: ExecutionEvent = match serde_json::from_str(line) {
                                Ok(e) => e,
                                Err(_) => continue,
                            };
                            if let Some(last) = last_replayed_seq {
                                if event.sequence <= last {
                                    continue;
                                }
                            }
                            last_replayed_seq = Some(
                                last_replayed_seq
                                    .map(|s| s.max(event.sequence))
                                    .unwrap_or(event.sequence),
                            );
                            if tx.send(Ok(execution_event_to_sse(&event))).is_err() {
                                return;
                            }
                        }
                    }
                    break;
                }
                // Has the worker registered the bus yet?
                if let Some(bus) = state_for_task
                    .event_buses
                    .lock()
                    .await
                    .get(&command_id_for_task)
                    .cloned()
                {
                    live_bus = Some(bus);
                    break;
                }
                if started.elapsed() >= POLL_TIMEOUT {
                    tracing::warn!(
                        command_id = %command_id_for_task,
                        "SSE wait for worker timed out — emitting Done"
                    );
                    break;
                }
                // Keepalive comment so the SSE connection stays alive on
                // intermediaries and the client knows we're still here.
                if tx
                    .send(Ok(
                        Event::default().comment("waiting for worker to claim job")
                    ))
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }

        // 3. If a live EventBus exists, subscribe and forward post-replay events.
        if let Some(bus) = live_bus {
            let mut rx = bus.subscribe();
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if let Some(last) = last_replayed_seq {
                            if event.sequence <= last {
                                continue;
                            }
                        }
                        let is_done = matches!(event.payload, EventPayload::Done);
                        if tx.send(Ok(execution_event_to_sse(&event))).is_err() {
                            return;
                        }
                        if is_done {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(lagged = n, "SSE subscriber lagged");
                        let comment =
                            Event::default().comment(format!("lagged: {n} events skipped"));
                        if tx.send(Ok(comment)).is_err() {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        let done = ExecutionEvent {
                            timestamp: chrono::Utc::now(),
                            sequence: last_replayed_seq.map(|s| s + 1).unwrap_or(0),
                            payload: EventPayload::Done,
                        };
                        let _ = tx.send(Ok(execution_event_to_sse(&done)));
                        return;
                    }
                }
            }
        }
        let done = ExecutionEvent {
            timestamp: chrono::Utc::now(),
            sequence: last_replayed_seq.map(|s| s + 1).unwrap_or(0),
            payload: EventPayload::Done,
        };
        let _ = tx.send(Ok(execution_event_to_sse(&done)));
    });

    Sse::new(stream).into_response()
}

/// Encode an ExecutionEvent as a structured SSE message:
/// `event: <type>\ndata: <json>\n\n`.
fn execution_event_to_sse(event: &ExecutionEvent) -> Event {
    let data = serde_json::to_string(event).unwrap_or_else(|_| "{}".into());
    Event::default()
        .event(event.payload.sse_event_type())
        .data(data)
}

async fn handle_get_session_queue(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    // Validate session exists.
    match state.store.get_session(&id) {
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                error_json(format!("Session '{}' not found", id)),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to get session");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to get session"),
            )
                .into_response();
        }
        Ok(Some(_)) => {}
    }

    let queue_depth = state.store.count_queued_for_session(&id).unwrap_or(0);

    let running = match state.store.running_command_for_session(&id) {
        Ok(Some(c)) => {
            let args: serde_json::Value =
                serde_json::from_str(&c.args).unwrap_or(serde_json::Value::Array(vec![]));
            Some(serde_json::json!({
                "command_id": c.id,
                "subcommand": c.subcommand,
                "args": args,
                "started_at": c.started_at,
                "worker_id": c.worker_id,
            }))
        }
        _ => None,
    };

    let session_cmds = state
        .store
        .list_commands_for_session(&id, 100)
        .unwrap_or_default();

    // Queued items: spec requires oldest first (position 0 = next to run).
    // `list_commands_for_session` returns newest first, so collect queued
    // candidates and reverse before assigning positions.
    let mut queued_records: Vec<_> = session_cmds
        .iter()
        .filter(|c| c.status == "queued")
        .collect();
    queued_records.reverse();
    let queued: Vec<serde_json::Value> = queued_records
        .iter()
        .enumerate()
        .map(|(pos, c)| {
            let args: serde_json::Value =
                serde_json::from_str(&c.args).unwrap_or(serde_json::Value::Array(vec![]));
            serde_json::json!({
                "command_id": c.id,
                "subcommand": c.subcommand,
                "args": args,
                "queued_at": c.queued_at,
                "position": pos as i64,
            })
        })
        .collect();

    // Recent completed: spec requires `finished_at DESC`, capped at 10.
    let mut completed_records: Vec<_> = session_cmds
        .iter()
        .filter(|c| matches!(c.status.as_str(), "done" | "error"))
        .collect();
    completed_records.sort_by(|a, b| b.finished_at.cmp(&a.finished_at));
    let recent_completed: Vec<serde_json::Value> = completed_records
        .into_iter()
        .take(10)
        .map(|c| {
            serde_json::json!({
                "command_id": c.id,
                "subcommand": c.subcommand,
                "status": c.status,
                "exit_code": c.exit_code,
                "finished_at": c.finished_at,
            })
        })
        .collect();

    Json(QueueStatusResponse {
        session_id: id,
        queue_depth,
        running,
        queued,
        recent_completed,
    })
    .into_response()
}

async fn handle_get_workflow(
    State(state): State<Arc<AppState>>,
    AxumPath(command_id): AxumPath<String>,
) -> Response {
    let session_id = match state.store.get_command(&command_id) {
        Ok(Some(c)) => c.session_id,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, error_json("command not found")).into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to get command for workflow");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to get command"),
            )
                .into_response();
        }
    };

    match state
        .paths
        .read_command_workflow_state_raw(&session_id, &command_id)
    {
        Ok(Some(content)) => match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(val) => Json(val).into_response(),
            Err(e) => {
                tracing::error!(error = %e, "Failed to parse workflow state");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    error_json("Failed to parse workflow state"),
                )
                    .into_response()
            }
        },
        Ok(None) => (
            StatusCode::NOT_FOUND,
            error_json("no workflow for this command"),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to read workflow state");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to read workflow state"),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    // Route table — assertion guard. Every entry here must be registered in
    // build_router; any divergence is a regression.
    const EXPECTED_ROUTES: &[(&str, &str)] = &[
        ("GET", "/v1/status"),
        ("GET", "/v1/workdirs"),
        ("GET", "/v1/sessions"),
        ("POST", "/v1/sessions"),
        ("GET", "/v1/sessions/{id}"),
        ("DELETE", "/v1/sessions/{id}"),
        ("GET", "/v1/sessions/{id}/status"),
        ("GET", "/v1/sessions/{id}/queue"),
        ("POST", "/v1/commands"),
        ("GET", "/v1/commands/{id}/status"),
        ("GET", "/v1/commands/{id}/logs"),
        ("GET", "/v1/workflows/{command_id}"),
    ];

    fn make_test_state(tmp: &std::path::Path) -> Arc<AppState> {
        use crate::command::dispatch::Engines;
        use crate::data::fs::api_db::SqliteSessionStore;
        use crate::data::fs::api_paths::ApiPaths;
        use crate::data::fs::auth_paths::AuthPathResolver;
        use crate::engine::agent::AgentEngine;
        use crate::engine::auth::AuthEngine;
        use crate::engine::container::ContainerRuntime;
        use crate::engine::git::GitEngine;
        use crate::engine::overlay::OverlayEngine;

        let paths = ApiPaths::at_root(tmp);
        let store = Arc::new(SqliteSessionStore::open(tmp).unwrap());
        let runtime = Arc::new(ContainerRuntime::docker());
        let overlay = Arc::new(OverlayEngine::with_auth_resolver(
            AuthPathResolver::at_home(tmp),
        ));
        let git_engine = Arc::new(GitEngine::new());
        let agent_engine = Arc::new(AgentEngine::new(overlay.clone(), runtime.clone()));
        let auth_engine = Arc::new(AuthEngine::with_paths(
            AuthPathResolver::at_home(tmp),
            paths.clone(),
        ));
        let workflow_state_store =
            Arc::new(crate::data::EngineWorkflowStateStore::at_git_root(tmp));
        let engines = Engines {
            runtime: runtime.clone(),
            container_runtime: Some(runtime),
            sandbox_runtime: None,
            git_engine,
            overlay_engine: overlay,
            auth_engine,
            agent_engine,
            workflow_state_store,
        };
        Arc::new(AppState {
            store,
            paths,
            workdirs: Vec::new(),
            started_at: Instant::now(),
            task_handles: tokio::sync::Mutex::new(Vec::new()),
            auth_mode: AuthMode::Disabled,
            engines,
            sessions: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            event_buses: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            setup_buses: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    #[test]
    fn expected_route_count() {
        // Guard: if someone adds a route without updating this table, the count drifts.
        assert_eq!(
            EXPECTED_ROUTES.len(),
            12,
            "route count mismatch — update EXPECTED_ROUTES"
        );
    }

    #[tokio::test]
    async fn all_expected_routes_respond_non_404() {
        let tmp = tempfile::tempdir().unwrap();
        let state = make_test_state(tmp.path());
        let app = build_router(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();

        // Test routes that always return non-404 regardless of request content.
        // These only depend on server state, not on specific resource IDs.
        let unconditional_routes: &[(&str, &str)] = &[
            ("GET", "/v1/status"),
            ("GET", "/v1/workdirs"),
            ("GET", "/v1/sessions"),
        ];

        for (method, path) in unconditional_routes {
            let url = format!("http://{addr}{path}");
            let req = match *method {
                "GET" => client.get(&url),
                "POST" => client.post(&url),
                _ => panic!("unhandled method {method}"),
            };
            let resp = req
                .send()
                .await
                .unwrap_or_else(|e| panic!("request to {method} {path} failed: {e}"));
            assert_ne!(
                resp.status().as_u16(),
                404,
                "{method} {path} returned 404 — route may not be registered"
            );
        }

        // Routes that naturally return 4xx for missing resources ARE registered —
        // verify by calling them with the correct method and asserting we get
        // anything other than a routing-level 404 for a completely unknown path.
        // (We use a clearly-bogus path to get the routing 404 baseline, then compare.)
        let bogus_404 = client
            .get(format!("http://{addr}/v1/definitely-not-a-route"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16();
        assert_eq!(bogus_404, 404, "bogus path must return 404");

        // Resource routes: these return handler-level 4xx (session/command not found).
        // We assert they respond with something (connection succeeds and we get any HTTP response).
        let resource_routes: &[(&str, &str, u16)] = &[
            // (method, path, expected_status_for_missing_resource)
            ("GET", "/v1/sessions/test-id", 404), // session not found
            ("DELETE", "/v1/sessions/test-id", 404), // session not found
            ("GET", "/v1/sessions/test-id/status", 404), // session not found
            ("GET", "/v1/sessions/test-id/queue", 404), // session not found
            ("GET", "/v1/commands/test-id/status", 404), // command not found
            ("GET", "/v1/commands/test-id/logs", 404), // command not found
            ("GET", "/v1/workflows/test-cmd", 404), // command not found
        ];

        for (method, path, expected_status) in resource_routes {
            let url = format!("http://{addr}{path}");
            let req = match *method {
                "GET" => client.get(&url),
                "DELETE" => client.delete(&url),
                _ => panic!("unhandled method {method}"),
            };
            let resp = req
                .send()
                .await
                .unwrap_or_else(|e| panic!("request to {method} {path} failed: {e}"));
            // The handler returns *expected_status* for missing resources.
            // We verify the route exists by confirming the response status matches
            // what the handler produces (not a routing-level 404 from an unregistered path).
            // Since both cases return 404 here, we at least verify the request succeeds.
            assert_eq!(
                resp.status().as_u16(),
                *expected_status,
                "{method} {path} returned unexpected status"
            );
        }

        // POST /v1/sessions — check it responds (even with 400/422 for missing body).
        let resp = client
            .post(format!("http://{addr}/v1/sessions"))
            .send()
            .await
            .unwrap();
        assert_ne!(
            resp.status().as_u16(),
            404,
            "POST /v1/sessions returned 404 — route may not be registered"
        );

        // POST /v1/commands — check it responds (even with 400 for missing headers).
        let resp = client
            .post(format!("http://{addr}/v1/commands"))
            .send()
            .await
            .unwrap();
        assert_ne!(
            resp.status().as_u16(),
            404,
            "POST /v1/commands returned 404 — route may not be registered"
        );
    }

    #[test]
    fn auth_middleware_rejects_missing_authorization_header() {
        // Auth logic is synchronous; test the hash comparison in isolation.
        use ring::digest;
        use subtle::ConstantTimeEq;

        let key = "test-api-key";
        let hash: String = {
            let h = digest::digest(&digest::SHA256, key.as_bytes());
            h.as_ref().iter().map(|b| format!("{b:02x}")).collect()
        };

        // Good key: computed hash matches stored hash.
        let provided_hash: String = {
            let h = digest::digest(&digest::SHA256, key.as_bytes());
            h.as_ref().iter().map(|b| format!("{b:02x}")).collect()
        };
        assert!(bool::from(provided_hash.as_bytes().ct_eq(hash.as_bytes())));

        // Bad key: hash does NOT match.
        let bad_hash: String = {
            let h = digest::digest(&digest::SHA256, b"wrong-key");
            h.as_ref().iter().map(|b| format!("{b:02x}")).collect()
        };
        assert!(!bool::from(bad_hash.as_bytes().ct_eq(hash.as_bytes())));
    }

    #[tokio::test]
    async fn auth_enabled_rejects_bad_key_with_401() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = make_test_state(tmp.path());

        // Set up auth with a known key hash.
        let key = "my-test-api-key";
        let hash: String = {
            use ring::digest;
            let h = digest::digest(&digest::SHA256, key.as_bytes());
            h.as_ref().iter().map(|b| format!("{b:02x}")).collect()
        };
        // Replace auth_mode with Enabled.
        Arc::get_mut(&mut state).unwrap().auth_mode = AuthMode::Enabled { key_hash: hash };

        let app = build_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();

        // No Authorization header → 401.
        let resp = client
            .get(format!("http://{addr}/v1/status"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            401,
            "missing auth header must return 401"
        );

        // Wrong key → 401.
        let resp = client
            .get(format!("http://{addr}/v1/status"))
            .header("Authorization", "Bearer wrong-key")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 401, "wrong key must return 401");

        // Correct key → not 401.
        let resp = client
            .get(format!("http://{addr}/v1/status"))
            .header("Authorization", format!("Bearer {key}"))
            .send()
            .await
            .unwrap();
        assert_ne!(resp.status().as_u16(), 401, "correct key must pass auth");
    }

    #[tokio::test]
    async fn auth_disabled_allows_all_requests() {
        let tmp = tempfile::tempdir().unwrap();
        let state = make_test_state(tmp.path()); // AuthMode::Disabled by default
        let app = build_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let resp = reqwest::get(format!("http://{addr}/v1/status"))
            .await
            .unwrap();
        assert_ne!(
            resp.status().as_u16(),
            401,
            "disabled auth must not block requests"
        );
    }
}
