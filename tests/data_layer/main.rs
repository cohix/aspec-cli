//! Layer 0 cross-module integration tests (WI 0073).
//!
//! Hermetic — no Docker, no git daemon, no network. Uses tempfile for all
//! filesystem operations. Every test here MUST pass under `make test-fast`.

#[path = "../helpers/mod.rs"]
mod helpers;

mod amie_config;
mod amie_paths;
mod condition_store;
mod config_session_roundtrip;
mod daemon_primitives;
mod db_migration;
mod issue_e2e;
mod issue_integration;
mod rename_0077;
mod sqlite_upgrade_compat;
