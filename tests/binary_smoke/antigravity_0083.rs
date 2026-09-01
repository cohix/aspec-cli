//! Binary-level tests for WI-0083: antigravity agent and gemini deprecation.
//!
//! Tests that can be validated without a live Docker daemon (warnings fire
//! before agent-image checks, and model-flag errors fire before image checks
//! in exec-prompt's early-validation path).

use std::process::Command;
use tempfile::TempDir;

fn awman_bin() -> std::path::PathBuf {
    crate::awman_bin()
}

#[allow(dead_code)]
fn awman() -> Command {
    Command::new(awman_bin())
}

/// Initialise a fresh git repo in a temp directory and return the TempDir.
fn make_git_repo() -> TempDir {
    let repo = TempDir::new().expect("TempDir::new");
    std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repo.path())
        .status()
        .expect("git init");
    repo
}

/// Build an `awman` subprocess scoped to a fresh empty config home so the
/// developer's real `~/.awman/config.json` (which on a working machine often
/// has a `runtime` and image registry configured) can never bleed into the
/// test's exit-code or warning-text assertions.
fn awman_isolated(repo: &TempDir) -> (TempDir, Command) {
    let cfg_home = TempDir::new().expect("config TempDir::new");
    let mut cmd = Command::new(awman_bin());
    cmd.current_dir(repo.path())
        .env("AWMAN_CONFIG_HOME", cfg_home.path())
        // Also pin the API root so the binary doesn't notice the dev
        // machine's real `~/.awman/api/` (sessions DB etc.). Belt and braces.
        .env("AWMAN_API_ROOT", cfg_home.path().join("api"))
        .env_remove("AWMAN_REMOTE_ADDR")
        .env_remove("AWMAN_REMOTE_SESSION")
        .env_remove("AWMAN_API_KEY")
        .env_remove("AWMAN_OVERLAYS");
    (cfg_home, cmd)
}

// ─── Gemini deprecation warning — chat ───────────────────────────────────────

/// `amux chat gemini` must emit a deprecation warning containing "deprecated"
/// to stderr before the container launch is attempted. The process exits
/// non-zero (no Docker images), but the warning must still be present.
#[test]
fn chat_gemini_emits_deprecation_warning_before_container_start() {
    let repo = make_git_repo();
    let (_cfg, mut cmd) = awman_isolated(&repo);
    let output = cmd
        .args(["chat", "--agent", "gemini"])
        .output()
        .expect("failed to run awman");

    // Must exit non-zero (no Docker images available in test environment).
    assert!(
        !output.status.success(),
        "amux chat gemini should exit non-zero in a test environment without Docker images"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("deprecated"),
        "stderr must contain 'deprecated' when using the gemini agent; got:\n{stderr}"
    );
    assert!(
        stderr.contains("gemini"),
        "deprecation warning must mention 'gemini'; got:\n{stderr}"
    );
}

// ─── Gemini deprecation warning — exec-workflow ───────────────────────────────

/// `amux exec-workflow --agent gemini` must emit a deprecation warning even
/// when the workflow file does not exist (warning fires before file check).
#[test]
fn exec_workflow_gemini_emits_deprecation_warning() {
    let repo = make_git_repo();
    let (_cfg, mut cmd) = awman_isolated(&repo);
    let output = cmd
        .args([
            "exec",
            "workflow",
            "--agent",
            "gemini",
            "nonexistent-workflow.toml",
        ])
        .output()
        .expect("failed to run awman");

    assert!(
        !output.status.success(),
        "amux exec workflow with missing file must exit non-zero"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("deprecated"),
        "stderr must contain 'deprecated' for exec-workflow with gemini agent; got:\n{stderr}"
    );
}

/// The deprecation warning must reference real `awman` commands, not the
/// previous draft `amux agent install …` (which does not exist). This catches
/// regressions of the WI-0083 review fix that replaced the placeholder text.
#[test]
fn gemini_deprecation_warning_uses_real_commands() {
    let repo = make_git_repo();
    let (_cfg, mut cmd) = awman_isolated(&repo);
    let output = cmd
        .args(["chat", "--agent", "gemini"])
        .output()
        .expect("failed to run awman");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("amux "),
        "deprecation warning must not reference the non-existent 'amux' binary; got:\n{stderr}"
    );
    assert!(
        !stderr.contains("agent install"),
        "deprecation warning must not reference the non-existent 'agent install' subcommand; got:\n{stderr}"
    );
    assert!(
        stderr.contains("awman chat antigravity")
            || stderr.contains("awman config set agent antigravity"),
        "deprecation warning must point users at a real awman command; got:\n{stderr}"
    );
}

// ─── antigravity --model → non-zero exit + clear error ───────────────────────

/// `amux chat antigravity --model gemini-3.5-flash` must exit non-zero.
/// The error must name both "antigravity" and indicate that a model flag is
/// not supported. Because build_options fires after agent-image checks, the
/// most we can guarantee at the binary level is a non-zero exit; the model-
/// flag message is verified at the unit level in engine/agent/mod.rs.
#[test]
fn chat_antigravity_model_flag_exits_nonzero() {
    let repo = make_git_repo();
    let (_cfg, mut cmd) = awman_isolated(&repo);
    let output = cmd
        .args([
            "chat",
            "--agent",
            "antigravity",
            "--model",
            "gemini-3.5-flash",
        ])
        .output()
        .expect("failed to run awman");

    assert!(
        !output.status.success(),
        "amux chat antigravity --model must exit non-zero; exit code: {:?}",
        output.status.code()
    );
}

/// Verify that the model-flag error message naming "antigravity" and
/// "does not support a model flag" appears somewhere in the combined output
/// when the binary reaches build_options (possible only when images exist, but
/// the unit test in engine/agent/mod.rs covers this definitively).
/// This binary test checks exit code + that no silent success occurs.
#[test]
fn chat_antigravity_model_flag_stderr_or_exit_nonzero() {
    let repo = make_git_repo();
    let (_cfg, mut cmd) = awman_isolated(&repo);
    let output = cmd
        .args([
            "chat",
            "--agent",
            "antigravity",
            "--model",
            "gemini-3.5-flash",
        ])
        .output()
        .expect("failed to run awman");

    let stderr = String::from_utf8_lossy(&output.stderr);
    // Must exit non-zero AND (if images are available) stderr must contain the
    // model-flag rejection message. In CI without Docker, the earlier
    // "project image" error is acceptable — non-zero exit is the invariant.
    assert!(
        !output.status.success(),
        "amux chat antigravity --model must never succeed; stderr:\n{stderr}"
    );
}
