//! WI-0077 binary-level rename verification.
//!
//! Verifies:
//! - The compiled binary is named `awman`, not `amux`.
//! - `awman --help` output contains no "amux" string anywhere.
//! - `awman api start --help` contains no "headless" string.
//! - Setting a deprecated `AMUX_*` env var produces a deprecation warning on stderr.
//! - `AWMAN_API_KEY` does NOT produce a spurious deprecation warning.

use std::process::{Command, Stdio};

fn awman_bin() -> std::path::PathBuf {
    crate::awman_bin()
}

fn make_git_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("TempDir::new");
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repo.path())
        .status()
        .expect("git init");
    repo
}

// ─── Binary name ─────────────────────────────────────────────────────────────

/// The CARGO_BIN_EXE_awman env var resolves to a path whose filename contains
/// "awman". If the binary were still named "amux", this env var would not be
/// set at all (causing a compile error) or would resolve to a different name.
#[test]
fn binary_name_is_awman_not_amux() {
    let path = awman_bin();
    let filename = path.file_name().unwrap().to_string_lossy();
    assert!(
        filename.contains("awman"),
        "binary filename should contain 'awman'; got {filename:?}"
    );
    assert!(
        !filename.contains("amux"),
        "binary filename must not contain 'amux'; got {filename:?}"
    );
}

// ─── Help output contains no "amux" ──────────────────────────────────────────

/// `awman --help` must not produce any output that still says "amux".
#[test]
fn help_output_does_not_contain_amux() {
    let out = Command::new(awman_bin())
        .arg("--help")
        .output()
        .expect("run awman --help");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}{stderr}");

    assert!(
        out.status.success(),
        "awman --help should exit 0; got {:?}",
        out.status.code()
    );
    assert!(
        !combined.to_lowercase().contains("amux"),
        "'awman --help' output must not contain 'amux';\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// `awman api --help` must not reference legacy "headless" terminology.
#[test]
fn api_help_does_not_contain_headless() {
    let out = Command::new(awman_bin())
        .args(["api", "--help"])
        .output()
        .expect("run awman api --help");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}{stderr}").to_lowercase();

    assert!(
        !combined.contains("headless"),
        "'awman api --help' must not contain 'headless';\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        combined.contains("api"),
        "'awman api --help' should mention 'api';\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// `awman api start --help` must not reference "headless" or "amux".
#[test]
fn api_start_help_does_not_contain_headless_or_amux() {
    let out = Command::new(awman_bin())
        .args(["api", "start", "--help"])
        .output()
        .expect("run awman api start --help");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}{stderr}").to_lowercase();

    assert!(
        !combined.contains("headless"),
        "'awman api start --help' must not contain 'headless';\n{stdout}"
    );
    assert!(
        !combined.contains("amux"),
        "'awman api start --help' must not contain 'amux';\n{stdout}"
    );
}

// ─── Deprecated AMUX_* env vars produce a deprecation warning ─────────────────

/// Spawns the binary with a real subcommand and the given env var, kills it
/// after a short window, then returns the stderr collected so far.
///
/// The deprecation warning is emitted early (before any Docker or git calls),
/// so 1 second is ample even on slow machines. The binary itself may hang
/// later (Docker unavailable) — we kill it before that becomes a problem.
///
/// `HOME` and `AWMAN_CONFIG_HOME` are pointed at scratch directories so the
/// child process cannot read or migrate the developer's real `~/.amux` or
/// `~/.awman` while tests run.
fn stderr_from_brief_run(args: &[&str], env_key: &str, env_val: &str) -> String {
    let repo = make_git_repo();
    let scratch_home = tempfile::tempdir().expect("scratch HOME");
    let scratch_cfg = tempfile::tempdir().expect("scratch AWMAN_CONFIG_HOME");
    let mut child = Command::new(awman_bin())
        .current_dir(repo.path())
        .env_remove("AMUX_CONFIG_HOME")
        .env_remove("AMUX_API_KEY")
        .env_remove("AMUX_API_ROOT")
        .env_remove("AMUX_OVERLAYS")
        .env_remove("AMUX_REMOTE_ADDR")
        .env_remove("AMUX_REMOTE_SESSION")
        .env_remove("AWMAN_CONFIG_HOME")
        .env_remove("AWMAN_API_KEY")
        .env_remove("AWMAN_API_ROOT")
        .env_remove("AWMAN_OVERLAYS")
        .env_remove("AWMAN_REMOTE_ADDR")
        .env_remove("AWMAN_REMOTE_SESSION")
        .env("HOME", scratch_home.path())
        .env("AWMAN_CONFIG_HOME", scratch_cfg.path())
        .env(env_key, env_val)
        .args(args)
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn awman {args:?}: {e}"));

    // Give the binary time to print the early-startup deprecation warning,
    // then kill it before it blocks indefinitely on Docker/git checks.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let _ = child.kill();
    let out = child.wait_with_output().expect("wait_with_output");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Running any real subcommand with `AMUX_API_KEY` set must emit a deprecation
/// warning on stderr naming `AWMAN_API_KEY` as the replacement.
///
/// `config show` is used because it passes clap validation and reaches the
/// deprecation check (lines 37-39 of `main.rs`) before any blocking I/O.
#[test]
fn deprecated_amux_api_key_emits_deprecation_warning() {
    let stderr = stderr_from_brief_run(&["status"], "AMUX_API_KEY", "legacy-test-value");

    assert!(
        stderr.contains("AMUX_API_KEY"),
        "deprecation warning must mention AMUX_API_KEY;\ngot:\n{stderr}"
    );
    assert!(
        stderr.contains("AWMAN_API_KEY"),
        "deprecation warning must mention AWMAN_API_KEY as replacement;\ngot:\n{stderr}"
    );
    assert!(
        stderr.contains("deprecated"),
        "deprecation warning must contain the word 'deprecated';\ngot:\n{stderr}"
    );
}

/// `AMUX_CONFIG_HOME` must also produce a deprecation warning.
#[test]
fn deprecated_amux_config_home_emits_deprecation_warning() {
    let stderr = stderr_from_brief_run(&["status"], "AMUX_CONFIG_HOME", "/tmp/legacy-amux-test");

    assert!(
        stderr.contains("AMUX_CONFIG_HOME"),
        "deprecation warning must mention AMUX_CONFIG_HOME;\ngot:\n{stderr}"
    );
    assert!(
        stderr.contains("AWMAN_CONFIG_HOME"),
        "deprecation warning must name the AWMAN_CONFIG_HOME replacement;\ngot:\n{stderr}"
    );
}

/// `AWMAN_API_KEY` is the current env var and must NOT produce a deprecation
/// warning — only old `AMUX_*` names trigger warnings.
#[test]
fn awman_api_key_does_not_trigger_deprecation_warning() {
    let stderr = stderr_from_brief_run(&["status"], "AWMAN_API_KEY", "valid-new-key");

    assert!(
        !stderr.contains("AWMAN_API_KEY is deprecated"),
        "AWMAN_API_KEY must not trigger a deprecation warning;\ngot:\n{stderr}"
    );
}
