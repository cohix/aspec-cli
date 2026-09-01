//! Binary-level smoke tests (WI 0073).
//!
//! These tests invoke the real `amux` binary as a subprocess and verify
//! exit codes, stdout shapes, and basic CLI behaviour.
//!
//! All tests here run under `make test-fast` because they don't need Docker.
//! Tests that need a real server or real git include those keywords.

mod awman_binary;
#[path = "../helpers/mod.rs"]
mod helpers;

pub(crate) fn awman_bin() -> std::path::PathBuf {
    awman_binary::awman_bin()
}

mod antigravity_0083;
mod clean_command;
mod cli_subprocess;
mod context_overlay_0087;
mod hardening_0098;
mod headless_no_tty;
mod overlay_0082;
mod rename_0077;
mod skill_libraries;
