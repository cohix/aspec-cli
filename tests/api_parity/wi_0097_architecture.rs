//! WI-0097 — architecture-conformance guard for the API frontend.
//!
//! The refactor's central rule (grand-architecture Tenet 2) is that the
//! presentation layer `src/frontend/api/` must not own storage or per-command
//! dispatch: filesystem side-effects belong to Layer 0 (`ApiPaths` /
//! `CommandLogWriter`) and argument parsing to Layer 2
//! (`CommandCatalogue::parse_raw_args`). This source-scan test enforces both,
//! matching the source-scanning style already used in `rename_0077.rs`
//! (`api_startup_log_message_contains_awman_and_api_mode`).
//!
//! Two properties are asserted over the **runtime** portion of each file (the
//! trailing `#[cfg(test)]` module is excluded so test helpers may still use
//! `tempfile`/`std::fs`):
//!   1. No direct `std::fs` / `tokio::fs` calls — all IO goes through Layer 0.
//!   2. No per-command `match` arm keyed on a command path string — dispatch is
//!      catalogue-driven, so multi-word command paths (e.g. `"exec prompt"`)
//!      must never appear as `"…​" =>` arms.

use awman::command::dispatch::catalogue::{CommandCatalogue, CommandSpec};

/// The API-frontend source files whose runtime code is under the layering rule.
const API_SOURCE_FILES: &[&str] = &[
    "command_frontend.rs",
    "event_bus.rs",
    "mod.rs",
    "queue_worker.rs",
    "routes.rs",
    "session_setup.rs",
];

/// Load one `src/frontend/api/<file>` at compile time and drop the trailing
/// `#[cfg(test)]` module so only runtime code is scanned. This retains the
/// source-level architecture guard when the test binary runs without a live
/// checkout.
fn read_runtime_source(file: &str) -> String {
    let src = match file {
        "command_frontend.rs" => include_str!("../../src/frontend/api/command_frontend.rs"),
        "event_bus.rs" => include_str!("../../src/frontend/api/event_bus.rs"),
        "mod.rs" => include_str!("../../src/frontend/api/mod.rs"),
        "queue_worker.rs" => include_str!("../../src/frontend/api/queue_worker.rs"),
        "routes.rs" => include_str!("../../src/frontend/api/routes.rs"),
        "session_setup.rs" => include_str!("../../src/frontend/api/session_setup.rs"),
        _ => unreachable!("API_SOURCE_FILES contains only known API frontend files"),
    };
    match src.find("#[cfg(test)]") {
        Some(idx) => src[..idx].to_string(),
        None => src.to_string(),
    }
}

/// Collect every command path (segments) in the catalogue with length >= 2.
fn multiword_paths(spec: &'static CommandSpec, path: Vec<&'static str>, out: &mut Vec<String>) {
    if path.len() >= 2 {
        out.push(path.join(" "));
    }
    for sub in spec.subcommands {
        let mut p = path.clone();
        p.push(sub.name);
        multiword_paths(sub, p, out);
    }
}

#[test]
fn api_frontend_has_no_direct_filesystem_calls() {
    for file in API_SOURCE_FILES {
        let src = read_runtime_source(file);
        for (n, line) in src.lines().enumerate() {
            assert!(
                !line.contains("std::fs"),
                "src/frontend/api/{file}:{} calls std::fs directly — route it through Layer 0 (ApiPaths / CommandLogWriter): {}",
                n + 1,
                line.trim()
            );
            assert!(
                !line.contains("tokio::fs"),
                "src/frontend/api/{file}:{} calls tokio::fs directly — route it through Layer 0: {}",
                n + 1,
                line.trim()
            );
        }
    }
}

#[test]
fn api_frontend_has_no_per_command_match_arms() {
    let cat = CommandCatalogue::get();
    let mut paths = Vec::new();
    multiword_paths(cat.root(), Vec::new(), &mut paths);
    assert!(
        paths.iter().any(|p| p == "exec prompt"),
        "sanity: catalogue must yield multi-word command paths"
    );

    for file in API_SOURCE_FILES {
        let src = read_runtime_source(file);
        for path in &paths {
            // A per-command dispatch arm looks like `"exec prompt" =>`. Listing
            // command names as data (e.g. the `available` array in an error
            // body) uses `"exec prompt"` WITHOUT a following `=>`, so it is fine.
            for arm in [format!("\"{path}\" =>"), format!("\"{path}\"=>")] {
                assert!(
                    !src.contains(&arm),
                    "src/frontend/api/{file} dispatches on command path {path:?} via a match arm ({arm:?}); dispatch must be catalogue-driven"
                );
            }
        }
        // Belt-and-suspenders: no `match` on a raw subcommand string.
        assert!(
            !src.contains("match subcommand"),
            "src/frontend/api/{file} must not `match subcommand` — parsing lives in Layer 2 (parse_raw_args)"
        );
    }
}
