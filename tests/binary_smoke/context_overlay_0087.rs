//! E2E binary smoke tests for WI-0087 context overlay CLI flag parsing.
//!
//! Validates that `--overlay context(global)` (and other context overlay
//! expressions) are accepted by the CLI dispatch layer without a parse error,
//! and that the roundtrip through `parse_overlay_list` produces the expected
//! typed overlay.

use std::process::Command;
use tempfile::TempDir;

fn awman_bin() -> std::path::PathBuf {
    crate::awman_bin()
}

fn awman() -> Command {
    Command::new(awman_bin())
}

fn make_git_repo() -> TempDir {
    let repo = TempDir::new().expect("TempDir::new");
    std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repo.path())
        .status()
        .expect("git init");
    repo
}

// ─── CLI dispatch roundtrip ───────────────────────────────────────────────────

/// Verify that `--overlay context(global)` is accepted by the CLI without a
/// parse error. The command may fail for other reasons (no Docker, no project
/// image), but the overlay expression itself must not trigger an "unknown
/// overlay type" or similar parse-level rejection.
#[test]
fn exec_prompt_context_global_overlay_accepted_by_cli() {
    let repo = make_git_repo();
    let output = awman()
        .current_dir(repo.path())
        .args([
            "exec",
            "prompt",
            "--non-interactive",
            "--overlay",
            "context(global)",
            "hello",
        ])
        .output()
        .expect("failed to run awman");

    let stderr = String::from_utf8_lossy(&output.stderr);
    // The CLI must not reject the overlay expression with a parse error.
    assert!(
        !stderr.contains("unknown overlay type"),
        "--overlay context(global) must not trigger 'unknown overlay type'; stderr: {stderr}"
    );
    assert!(
        !stderr.contains("context() requires"),
        "--overlay context(global) must not trigger 'context() requires'; stderr: {stderr}"
    );
    assert!(
        !stderr.contains("unknown context scope"),
        "--overlay context(global) must not trigger 'unknown context scope'; stderr: {stderr}"
    );
    // Any failure must be for a reason unrelated to overlay parsing (e.g. no
    // Docker / no project image / non-interactive limitation).
}

/// Verify that `--overlay context(repo)` is also accepted by the CLI.
#[test]
fn exec_prompt_context_repo_overlay_accepted_by_cli() {
    let repo = make_git_repo();
    let output = awman()
        .current_dir(repo.path())
        .args([
            "exec",
            "prompt",
            "--non-interactive",
            "--overlay",
            "context(repo)",
            "hello",
        ])
        .output()
        .expect("failed to run awman");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("unknown overlay type"),
        "--overlay context(repo) must not trigger 'unknown overlay type'; stderr: {stderr}"
    );
    assert!(
        !stderr.contains("context() requires"),
        "--overlay context(repo) must not trigger a parse error; stderr: {stderr}"
    );
}

/// Verify that `--overlay context(global:ro)` is accepted by the CLI.
#[test]
fn exec_prompt_context_global_ro_overlay_accepted_by_cli() {
    let repo = make_git_repo();
    let output = awman()
        .current_dir(repo.path())
        .args([
            "exec",
            "prompt",
            "--non-interactive",
            "--overlay",
            "context(global:ro)",
            "hello",
        ])
        .output()
        .expect("failed to run awman");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("unknown overlay type"),
        "--overlay context(global:ro) must not trigger 'unknown overlay type'; stderr: {stderr}"
    );
    assert!(
        !stderr.contains("unknown permission"),
        "--overlay context(global:ro) must not trigger 'unknown permission'; stderr: {stderr}"
    );
}

/// Verify that `--overlay context(unknown_scope)` is REJECTED with a parse error.
#[test]
fn exec_prompt_context_unknown_scope_rejected_with_parse_error() {
    let repo = make_git_repo();
    let output = awman()
        .current_dir(repo.path())
        .args([
            "exec",
            "prompt",
            "--non-interactive",
            "--overlay",
            "context(notascope)",
            "hello",
        ])
        .output()
        .expect("failed to run awman");

    assert!(
        !output.status.success(),
        "awman must exit non-zero for context(notascope)"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("notascope") || stderr.contains("unknown") || stderr.contains("scope"),
        "rejection must name the bad scope or say 'unknown'; stderr: {stderr}"
    );
}

// ─── parse_overlay_list roundtrip (library level) ────────────────────────────

/// Roundtrip test using the library API directly — ensures the CLI dispatch
/// would correctly plumb `context(global)` through `parse_overlay_list`.
#[test]
fn parse_overlay_list_context_global_roundtrip() {
    use awman::command::commands::{parse_overlay_list, ContextOverlaySpec, TypedOverlay};
    use awman::engine::container::options::OverlayPermission;
    use awman::engine::overlay::ContextScope;

    let result = parse_overlay_list("context(global)").expect("context(global) must parse");
    assert_eq!(result.len(), 1);
    match &result[0] {
        TypedOverlay::Context(ContextOverlaySpec { scope, permission }) => {
            assert_eq!(*scope, ContextScope::Global);
            assert_eq!(*permission, OverlayPermission::ReadWrite);
        }
        other => panic!("expected TypedOverlay::Context, got {other:?}"),
    }
}

#[test]
fn parse_overlay_list_context_workflow_ro_roundtrip() {
    use awman::command::commands::{parse_overlay_list, ContextOverlaySpec, TypedOverlay};
    use awman::engine::container::options::OverlayPermission;
    use awman::engine::overlay::ContextScope;

    let result =
        parse_overlay_list("context(workflow:ro)").expect("context(workflow:ro) must parse");
    assert_eq!(result.len(), 1);
    match &result[0] {
        TypedOverlay::Context(ContextOverlaySpec { scope, permission }) => {
            assert_eq!(*scope, ContextScope::Workflow);
            assert_eq!(*permission, OverlayPermission::ReadOnly);
        }
        other => panic!("expected TypedOverlay::Context, got {other:?}"),
    }
}
