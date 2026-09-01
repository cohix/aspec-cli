//! Layer 0 cross-module integration tests (WI 0073).
//!
//! Hermetic — no Docker, no git daemon, no network. Uses tempfile for all
//! filesystem operations. Every test here MUST pass under `make test-fast`.

#[path = "../helpers/mod.rs"]
mod helpers;

mod config_session_roundtrip;
mod daemon_primitives;
mod db_migration;
mod issue_e2e;
mod issue_integration;
mod rename_0077;
mod sqlite_upgrade_compat;
mod squad_config;
mod squad_paths;
mod task_store;
