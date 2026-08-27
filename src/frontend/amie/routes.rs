//! HTTP transport for the amie daemon.
//!
//! This module intentionally contains no condition validation or scheduling
//! policy.  The command route delegates both parsing and execution to
//! `CommandCatalogue` and `Dispatch`; the two read routes expose daemon state.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tower_http::trace::TraceLayer;

use crate::command::commands::amie::commands::AmieOutcome;
use crate::command::commands::amie::gateway::ConditionGateway;
use crate::command::dispatch::catalogue::{CommandCatalogue, FrontendKind};
use crate::command::dispatch::{CommandOutcome, Dispatch};
use crate::data::EngineWorkflowStateStore;
use crate::frontend::api::command_frontend::ApiDispatchFrontend;
use crate::frontend::api::event_bus::EventBus;
use crate::frontend::api::serve::{check_bearer_auth, error_json};

use super::state::AmieAppState;

#[derive(Deserialize)]
struct CreateCommandRequest {
    subcommand: String,
    args: Vec<String>,
}

/// Build amie's independent router.  It is never mounted below the API router.
pub fn build_router(state: Arc<AmieAppState>) -> Router {
    Router::new()
        .route("/v1/commands", post(handle_command))
        .route("/v1/status", get(handle_status))
        .route("/v1/conditions/{name}/workflow", get(handle_workflow))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// State extraction around the one shared bearer-auth decision, so amie's
/// wire behaviour is API mode's by construction rather than by copy.
async fn auth_middleware(
    State(state): State<Arc<AmieAppState>>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Response {
    if let Some(rejection) = check_bearer_auth(&state.auth_mode, req.headers()) {
        return rejection;
    }
    next.run(req).await
}

async fn handle_command(
    State(state): State<Arc<AmieAppState>>,
    Json(body): Json<CreateCommandRequest>,
) -> Response {
    let path_parts: Vec<&str> = body.subcommand.split_whitespace().collect();
    if path_parts.first() != Some(&"amie") {
        return (
            StatusCode::BAD_REQUEST,
            error_json("amie daemon only accepts commands in the amie subtree"),
        )
            .into_response();
    }

    let catalogue = CommandCatalogue::get();
    if let Err(error) = catalogue.validate_for_frontend(FrontendKind::Api, &path_parts) {
        return (StatusCode::BAD_REQUEST, error_json(error.to_string())).into_response();
    }
    if let Err(error) =
        catalogue.parse_raw_args_with_profile(&path_parts, &body.args, FrontendKind::Api)
    {
        return (StatusCode::BAD_REQUEST, error_json(error.to_string())).into_response();
    }

    // ApiDispatchFrontend is the existing non-interactive Dispatch frontend.
    // Its event bus has no subscriber here: amie deliberately exposes neither
    // an SSE route nor a logs route.
    let frontend =
        ApiDispatchFrontend::new(&body.subcommand, &body.args, EventBus::new(1).sender());
    let outcome = Dispatch::new(frontend, state.session.clone(), state.engines.clone())
        .with_amie_gateway(state.gateway())
        .run_command(&path_parts)
        .await;
    match outcome {
        // The remote gateway deserializes the command result into the same
        // concrete type used by local callers.  Keep that synchronous payload
        // direct; the `AmieOutcome` enum is an internal Dispatch wrapper.
        Ok(CommandOutcome::Amie(outcome)) => amie_outcome_response(outcome),
        // The catalogue/front-door restriction above make this unreachable;
        // retain a safe error if a future Dispatch change violates the seam.
        Ok(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            error_json("amie command dispatched to an unexpected command family"),
        )
            .into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, error_json(error.to_string())).into_response(),
    }
}

fn amie_outcome_response(outcome: AmieOutcome) -> Response {
    match outcome {
        AmieOutcome::Condition(condition) => Json(condition).into_response(),
        AmieOutcome::Detail(detail) => Json(detail).into_response(),
        AmieOutcome::Conditions(conditions) => Json(conditions).into_response(),
        AmieOutcome::Removed { name } => Json(serde_json::json!({ "name": name })).into_response(),
        AmieOutcome::Ok => Json(serde_json::json!({})).into_response(),
        AmieOutcome::Status(status) => Json(status).into_response(),
        AmieOutcome::Started {
            port,
            background,
            refreshed_key,
        } => Json(serde_json::json!({
            "port": port,
            "background": background,
            "refreshed_key": refreshed_key,
        }))
        .into_response(),
        AmieOutcome::Stopped { stopped_pid } => {
            Json(serde_json::json!({ "stopped_pid": stopped_pid })).into_response()
        }
        AmieOutcome::Logs { log_path } => {
            Json(serde_json::json!({ "log_path": log_path })).into_response()
        }
    }
}

async fn handle_status(State(state): State<Arc<AmieAppState>>) -> Response {
    match state.gateway.status().await {
        Ok(mut status) => {
            status.bound_addr = state
                .bound_addr
                .lock()
                .expect("amie bound-address mutex poisoned")
                .clone();
            Json(status).into_response()
        }
        Err(error) => {
            tracing::error!(error = %error, "amie: failed to read daemon status");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to read daemon status"),
            )
                .into_response()
        }
    }
}

async fn handle_workflow(
    State(state): State<Arc<AmieAppState>>,
    Path(name): Path<String>,
) -> Response {
    let condition = match state.store.get(&name) {
        Ok(Some(condition)) => condition,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, error_json("condition not found")).into_response()
        }
        Err(error) => {
            tracing::error!(error = %error, "amie: failed to read condition for workflow");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to read condition"),
            )
                .into_response();
        }
    };
    let run = match state.store.running_run_for(&condition.id) {
        Ok(Some(run)) => run,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                error_json("no workflow for this condition"),
            )
                .into_response()
        }
        Err(error) => {
            tracing::error!(error = %error, "amie: failed to read running workflow");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to read workflow"),
            )
                .into_response();
        }
    };
    let Some(path) = run.workflow_state_path else {
        return (
            StatusCode::NOT_FOUND,
            error_json("no workflow for this condition"),
        )
            .into_response();
    };
    match EngineWorkflowStateStore::read_state_path(&path) {
        Ok(Some(workflow)) => Json(workflow).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            error_json("no workflow for this condition"),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(error = %error, "amie: failed to read workflow state");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to read workflow state"),
            )
                .into_response()
        }
    }
}
