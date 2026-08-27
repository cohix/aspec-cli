//! Headless parity tests (WI 0073).
//!
//! routes.rs       — verifies the route table and API paths.
//! auth_modes.rs   — auth-mode-related path/type smoke checks.
//! live_server.rs  — boots the real Axum router on an ephemeral port and
//!                   hits it with reqwest. Tests are prefixed `real_network_`
//!                   so `make test-fast` can skip them.
//!
//! SSE wire-format and WebSocket tests against a fully booted `awman api
//! start` subprocess are deferred to WI 0076.

#[path = "../helpers/mod.rs"]
mod helpers;

mod auth_modes;
mod http_core;
mod live_server;
mod rename_0077;
mod routes;
mod wi_0078;
mod wi_0079;
mod wi_0097;
mod wi_0097_architecture;
