//! Binary-level overlay tests for WI-0082.
//!
//! These tests invoke the compiled `awman` binary to verify overlay-related
//! CLI behaviour introduced in WI-0082: removal of `--mount-ssh`, removal of
//! the `skills()` plural form, and fatal handling of malformed `AWMAN_OVERLAYS`.

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

// ─── --mount-ssh removed ──────────────────────────────────────────────────────

#[test]
fn mount_ssh_flag_rejected_with_nonzero_exit() {
    let repo = make_git_repo();
    let output = awman()
        .current_dir(repo.path())
        .args(["chat", "--non-interactive", "--mount-ssh"])
        .output()
        .expect("failed to run awman");

    assert!(
        !output.status.success(),
        "--mount-ssh must be rejected with a non-zero exit; got 0"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("unrecognized argument --overlay")
            && !stderr.contains("unexpected argument '--overlay'"),
        "--overlay must remain a recognised flag; stderr: {stderr}"
    );
    assert!(
        stderr.contains("--mount-ssh"),
        "rejection message must name the removed --mount-ssh flag; stderr: {stderr}"
    );
    assert!(
        stderr.contains("ssh()") || stderr.contains("--overlay"),
        "rejection message must point users at `--overlay ssh()` as the replacement; stderr: {stderr}"
    );
}

#[test]
fn mount_ssh_with_value_form_also_rejected_with_guidance() {
    let repo = make_git_repo();
    let output = awman()
        .current_dir(repo.path())
        .args(["chat", "--non-interactive", "--mount-ssh=true"])
        .output()
        .expect("failed to run awman");

    assert!(
        !output.status.success(),
        "--mount-ssh=true must be rejected with a non-zero exit; got 0"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--mount-ssh")
            && (stderr.contains("ssh()") || stderr.contains("--overlay")),
        "--mount-ssh=true must produce the same guidance message; stderr: {stderr}"
    );
}

// ─── skills() plural form removed ────────────────────────────────────────────

#[test]
fn skills_plural_named_in_overlay_flag_exits_nonzero() {
    let repo = make_git_repo();
    let output = awman()
        .current_dir(repo.path())
        .args(["chat", "--non-interactive", "--overlay", "skills(lint)"])
        .output()
        .expect("failed to run awman");

    assert!(
        !output.status.success(),
        "--overlay skills(lint) must exit non-zero (removed form)"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("skill(") || stderr.contains("removed"),
        "error must mention the replacement skill() form or that skills() was removed; got: {stderr}"
    );
}

#[test]
fn skills_plural_empty_in_overlay_flag_exits_nonzero() {
    let repo = make_git_repo();
    let output = awman()
        .current_dir(repo.path())
        .args(["chat", "--non-interactive", "--overlay", "skills()"])
        .output()
        .expect("failed to run awman");

    assert!(
        !output.status.success(),
        "--overlay skills() must exit non-zero (removed form)"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("skill(") || stderr.contains("removed"),
        "error must mention skill(*) replacement or that skills() was removed; got: {stderr}"
    );
}

#[test]
fn skills_plural_in_awman_overlays_env_exits_nonzero() {
    let repo = make_git_repo();
    let output = awman()
        .current_dir(repo.path())
        .env("AWMAN_OVERLAYS", "skills(foo)")
        .args(["chat", "--non-interactive"])
        .output()
        .expect("failed to run awman");

    assert!(
        !output.status.success(),
        "skills(foo) in AWMAN_OVERLAYS must exit non-zero (removed form)"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("skill(") || stderr.contains("removed"),
        "error must mention migration guidance; got: {stderr}"
    );
}

// ─── Malformed AWMAN_OVERLAYS ─────────────────────────────────────────────────

#[test]
fn malformed_awman_overlays_causes_fatal_exit_and_names_env_var() {
    let repo = make_git_repo();
    let output = awman()
        .current_dir(repo.path())
        .env("AWMAN_OVERLAYS", "###not-an-overlay###")
        .args(["chat", "--non-interactive"])
        .output()
        .expect("failed to run awman");

    assert!(
        !output.status.success(),
        "malformed AWMAN_OVERLAYS must cause a non-zero exit when running a command"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("AWMAN_OVERLAYS"),
        "error message must mention AWMAN_OVERLAYS so the user knows the source; got: {stderr}"
    );
}

#[test]
fn malformed_awman_overlays_does_not_prevent_help() {
    let output = awman()
        .env("AWMAN_OVERLAYS", "###not-an-overlay###")
        .args(["chat", "--help"])
        .output()
        .expect("failed to run awman chat --help");

    assert!(
        output.status.success(),
        "amux chat --help must exit 0 regardless of AWMAN_OVERLAYS content; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ─── --overlay flag still present in help ────────────────────────────────────

#[test]
fn chat_help_shows_overlay_flag() {
    let output = awman()
        .args(["chat", "--help"])
        .output()
        .expect("failed to run amux chat --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--overlay"),
        "chat --help must mention --overlay flag; got: {stdout}"
    );
}

#[test]
fn exec_workflow_help_shows_overlay_flag() {
    let output = awman()
        .args(["exec", "workflow", "--help"])
        .output()
        .expect("failed to run amux exec workflow --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--overlay"),
        "exec workflow --help must mention --overlay flag; got: {stdout}"
    );
}
