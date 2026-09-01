//! Binary-level end-to-end tests for WI-0098 (architecture hardening).
//!
//! These invoke the compiled `awman` binary as a subprocess and pin down the
//! two Finding-B behaviours the work item calls out under "Test Considerations":
//!
//!   * `awman --mount-ssh` prints the migration hint and exits with code `2`
//!     (unchanged behaviour after the removed-flag knowledge moved from
//!     `main.rs` into the catalogue).
//!   * An invalid `runtime:` in the global config is a fatal error for CLI
//!     invocations — it names the bad runtime and exits `2`, never silently
//!     falling back to Docker.
//!
//! The unknown-runtime *TUI modal* half of that behaviour is verified at the
//! unit level (`Engines::detect` returns the modal message on the empty-path
//! bare-TUI branch — see `command::dispatch` tests); it needs a live terminal
//! and so is not reachable from a headless subprocess test.

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

// ─── Finding B: removed-flag migration hint exits 2 ───────────────────────────

/// The catalogue's removed-flag scan runs before clap in `main.rs`, so a bare
/// `awman --mount-ssh` (no subcommand) still yields the migration hint and the
/// documented exit code `2` — not a TUI launch and not clap's generic error.
#[test]
fn mount_ssh_prints_migration_hint_and_exits_exactly_2() {
    let repo = make_git_repo();
    let output = awman()
        .current_dir(repo.path())
        .env("HOME", repo.path())
        .arg("--mount-ssh")
        .output()
        .expect("failed to run awman");

    assert_eq!(
        output.status.code(),
        Some(2),
        "`awman --mount-ssh` must exit with code 2; got {:?}",
        output.status.code()
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--mount-ssh has been removed"),
        "stderr must carry the migration hint naming the removed flag; got: {stderr}"
    );
    assert!(
        stderr.contains("--overlay") && stderr.contains("ssh()"),
        "the hint must point at the `--overlay ssh()` replacement; got: {stderr}"
    );
}

/// The `=`-bearing form is intercepted identically (edge case called out in the
/// work item): same hint, same exit code 2.
#[test]
fn mount_ssh_value_form_exits_exactly_2() {
    let repo = make_git_repo();
    let output = awman()
        .current_dir(repo.path())
        .env("HOME", repo.path())
        .arg("--mount-ssh=whatever")
        .output()
        .expect("failed to run awman");

    assert_eq!(
        output.status.code(),
        Some(2),
        "`awman --mount-ssh=whatever` must exit with code 2; got {:?}",
        output.status.code()
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--mount-ssh has been removed"),
        "the `=`-form must produce the same migration hint; got: {stderr}"
    );
}

// ─── Finding B: invalid `runtime:` config is fatal for the CLI ────────────────

/// An unrecognised `runtime:` string in the global config must fatal-error a
/// CLI invocation: the message names the bad runtime and lists the valid ones,
/// and the process exits `2`. This proves the fatal-vs-fallback policy lifted
/// into `Engines::detect` still refuses to silently run on Docker when the
/// configured runtime is a typo.
#[test]
fn invalid_runtime_config_is_fatal_for_cli_with_exit_2() {
    let repo = make_git_repo();
    // Global config lives at `$HOME/.awman/config.json`; point HOME at the temp
    // repo so this test never touches the developer's real config. Crucially,
    // pin `AWMAN_CONFIG_HOME` (highest-precedence config-home override) at that
    // same dir: without it, an ambient `AWMAN_CONFIG_HOME`/`XDG_CONFIG_HOME` on
    // the runner would shadow `$HOME/.awman`, the bad-runtime config would be
    // ignored, and `awman status` would fall through to a real `docker ps` —
    // which blocks indefinitely on a slow/wedged daemon (the CI hang) instead
    // of hitting the fast invalid-runtime fatal this test asserts.
    let awman_dir = repo.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).expect("create .awman");
    std::fs::write(
        awman_dir.join("config.json"),
        br#"{"runtime":"totally-bogus-runtime"}"#,
    )
    .expect("write config.json");

    let output = awman()
        .current_dir(repo.path())
        .env("HOME", repo.path())
        .env("AWMAN_CONFIG_HOME", &awman_dir)
        .env_remove("XDG_CONFIG_HOME")
        .arg("status")
        .output()
        .expect("failed to run awman");

    assert_eq!(
        output.status.code(),
        Some(2),
        "an invalid runtime must fatal-error the CLI with exit 2; got {:?}",
        output.status.code()
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("totally-bogus-runtime"),
        "the fatal message must name the misspelled runtime; got: {stderr}"
    );
    assert!(
        stderr.contains("docker"),
        "the fatal message must list the valid runtimes (incl. docker); got: {stderr}"
    );
}

/// Sanity counter-test: a *valid* runtime config must not trip the fatal path.
/// `config show` needs no runtime and must succeed (exit 0), confirming the
/// fatal exit above is specific to the bad-runtime string and not an artefact
/// of the temp-HOME setup.
#[test]
fn valid_runtime_config_show_succeeds() {
    let repo = make_git_repo();
    let awman_dir = repo.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).expect("create .awman");
    std::fs::write(awman_dir.join("config.json"), br#"{"runtime":"docker"}"#)
        .expect("write config.json");

    let output = awman()
        .current_dir(repo.path())
        .env("HOME", repo.path())
        // Pin the config home so an ambient override on the runner can't shadow
        // the `docker` runtime we just wrote (see the invalid-runtime test).
        .env("AWMAN_CONFIG_HOME", &awman_dir)
        .env_remove("XDG_CONFIG_HOME")
        .args(["config", "show"])
        .output()
        .expect("failed to run awman");

    assert_eq!(
        output.status.code(),
        Some(0),
        "`config show` with a valid runtime must exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
