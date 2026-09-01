//! Layer 1 engine integration tests (WI 0073).
//!
//! Tests that need Docker have "docker" in their name and are skipped by
//! `make test-fast` via `--skip docker`.
//! Tests that need real git have "real_git" in their name.

#[path = "../helpers/mod.rs"]
mod helpers;

mod acp_support;
mod container_docker;
mod container_io;
mod context_overlay_0087;
mod credential_argv_docker;
mod credential_refresh_integration;
mod git_engine;
mod overlay_engine;
mod sbx;
mod stuck_event_wiring;
mod workflow_end_to_end;
mod workflow_on_failure;
