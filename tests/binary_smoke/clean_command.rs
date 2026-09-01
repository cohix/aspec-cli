//! Binary-level tests for `awman clean`.
//!
//! Covers:
//! - Catalogue: `clean` subcommand is registered with `--yes` and `--dry-run` flags
//! - CLI confirmation: `--yes` skips prompt; non-TTY without `--yes` aborts with exit 2;
//!   `y\n` on stdin confirms (TTY-simulation not tested at this level — real TTY
//!   detection is exercised by the non-TTY path)
//! - E2E dry-run: no files are removed; output lists expected items
//! - E2E full flow (filesystem only, no Docker): completed workflow file is removed,
//!   pending workflow preserved, exit 0

use std::path::Path;
use std::process::{Command, Stdio};

fn awman_bin() -> std::path::PathBuf {
    crate::awman_bin()
}

/// Initialise a minimal git repo at `path`.
fn git_init(path: &Path) {
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(path)
        .status()
        .expect("git init");
}

/// Open `/dev/null` for reading (detaches stdin from any TTY).
fn null_stdin() -> Stdio {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .open("/dev/null")
        .expect("open /dev/null");
    Stdio::from(file)
}

/// Write a completed or non-terminal workflow state JSON.
fn write_workflow_state(dir: &Path, name: &str, complete: bool) {
    use awman::data::workflow_definition::WorkflowStep;
    use awman::data::workflow_state::{StepState, WorkflowState};

    let step = WorkflowStep {
        name: "s1".to_string(),
        depends_on: vec![],
        prompt_template: "p".to_string(),
        agent: None,
        model: None,
        overlays: None,
        abort_on_failure: false,
    };
    let mut state = WorkflowState::new("wf".to_string(), &[step], "hash".to_string(), None);
    if complete {
        state.set_status("s1", StepState::Succeeded);
    }
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, serde_json::to_string(&state).unwrap()).unwrap();
}

// ─── Catalogue tests ──────────────────────────────────────────────────────────

#[test]
fn catalogue_has_clean_command() {
    use awman::command::dispatch::catalogue::CommandCatalogue;
    let names: Vec<&str> = CommandCatalogue::get()
        .root()
        .subcommands
        .iter()
        .map(|s| s.name)
        .collect();
    assert!(
        names.contains(&"clean"),
        "`clean` must be registered in the catalogue; got: {names:?}"
    );
}

#[test]
fn catalogue_clean_has_yes_flag() {
    use awman::command::dispatch::catalogue::CommandCatalogue;
    let cat = CommandCatalogue::get();
    let clean = cat.lookup(&["clean"]).expect("`clean` must exist");
    assert!(
        clean.find_flag("yes").is_some(),
        "`clean` must have --yes flag"
    );
}

#[test]
fn catalogue_clean_has_dry_run_flag() {
    use awman::command::dispatch::catalogue::CommandCatalogue;
    let cat = CommandCatalogue::get();
    let clean = cat.lookup(&["clean"]).expect("`clean` must exist");
    assert!(
        clean.find_flag("dry-run").is_some(),
        "`clean` must have --dry-run flag"
    );
}

#[test]
fn awman_clean_help_exits_zero() {
    let out = Command::new(awman_bin())
        .args(["clean", "--help"])
        .output()
        .expect("awman clean --help");
    assert!(
        out.status.success(),
        "`awman clean --help` must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn awman_clean_help_mentions_yes_and_dry_run() {
    let out = Command::new(awman_bin())
        .args(["clean", "--help"])
        .output()
        .expect("awman clean --help");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("yes") || text.contains("--yes"),
        "`awman clean --help` must mention --yes; got: {text}"
    );
    assert!(
        text.contains("dry-run") || text.contains("--dry-run"),
        "`awman clean --help` must mention --dry-run; got: {text}"
    );
}

// ─── CLI confirmation: non-TTY without --yes aborts ──────────────────────────

/// When stdin is not a TTY and `--yes` is absent, `awman clean` must abort
/// with a clear error message (exit 2 = InteractiveInputUnavailable).
#[test]
#[cfg(unix)]
fn clean_non_tty_stdin_without_yes_aborts() {
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());

    // Create a completed workflow so discovery finds something (otherwise
    // the command exits 0 with "Nothing to clean." before needing confirmation).
    let wf_dir = repo.path().join(".awman").join("workflows");
    write_workflow_state(&wf_dir, "done.json", true);

    let home = tempfile::tempdir().unwrap();

    let out = Command::new(awman_bin())
        .args(["clean"])
        .current_dir(repo.path())
        .env("AWMAN_CONFIG_HOME", home.path())
        .stdin(null_stdin())
        .output()
        .expect("awman clean");

    assert!(
        !out.status.success(),
        "`awman clean` with no-TTY stdin must fail; exit: {:?}",
        out.status.code()
    );
    let code = out.status.code().unwrap_or(0);
    assert_eq!(
        code, 2,
        "`awman clean` non-TTY must exit 2 (InteractiveInputUnavailable); got: {code}"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("stdin") || combined.contains("--yes") || combined.contains("TTY"),
        "error must mention stdin/TTY or --yes; got: {combined}"
    );
    // The file must NOT have been deleted
    assert!(
        wf_dir.join("done.json").exists(),
        "file must not be deleted when aborted"
    );
}

/// `awman clean` with nothing to clean should exit 0 even with no-TTY stdin
/// (no confirmation needed when there is nothing to clean).
#[test]
#[cfg(unix)]
fn clean_nothing_to_clean_exits_zero_with_no_tty() {
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(awman_bin())
        .args(["clean"])
        .current_dir(repo.path())
        .env("AWMAN_CONFIG_HOME", home.path())
        .stdin(null_stdin())
        .output()
        .expect("awman clean");

    assert!(
        out.status.success(),
        "`awman clean` with nothing to clean must exit 0 even with no TTY; \
         exit: {:?}; stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
}

// ─── E2E: dry-run ─────────────────────────────────────────────────────────────

/// `awman clean --dry-run` against a repo with a completed workflow file must:
/// - Exit 0
/// - List the stale items in stdout/stderr
/// - Leave all files intact
#[test]
fn clean_dry_run_lists_items_and_deletes_nothing() {
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());
    let wf_dir = repo.path().join(".awman").join("workflows");
    write_workflow_state(&wf_dir, "abcd1234-wf.json", true);
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(awman_bin())
        .args(["clean", "--dry-run"])
        .current_dir(repo.path())
        .env("AWMAN_CONFIG_HOME", home.path())
        .stdin(null_stdin())
        .output()
        .expect("awman clean --dry-run");

    assert!(
        out.status.success(),
        "`awman clean --dry-run` must exit 0; exit: {:?}; stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    // Output must mention the file that would be deleted
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("abcd1234-wf.json")
            || combined.contains("workflow")
            || combined.contains("would be removed"),
        "dry-run output must list the stale item; got: {combined}"
    );

    // File must NOT have been deleted
    assert!(
        wf_dir.join("abcd1234-wf.json").exists(),
        "dry-run must not delete files"
    );
}

/// `awman clean --dry-run` with nothing to clean must print "Nothing to clean."
/// and exit 0.
#[test]
fn clean_dry_run_nothing_to_clean_exits_zero() {
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(awman_bin())
        .args(["clean", "--dry-run"])
        .current_dir(repo.path())
        .env("AWMAN_CONFIG_HOME", home.path())
        .stdin(null_stdin())
        .output()
        .expect("awman clean --dry-run (empty)");

    assert!(
        out.status.success(),
        "`awman clean --dry-run` with empty repo must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("Nothing to clean") || combined.contains("nothing"),
        "output must say nothing to clean; got: {combined}"
    );
}

// ─── E2E: full flow with --yes (filesystem only) ──────────────────────────────

/// `awman clean --yes` must:
/// - Delete completed workflow files
/// - Preserve non-terminal (pending) workflow files
/// - Exit 0
#[test]
fn clean_yes_deletes_completed_workflow_and_preserves_pending() {
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());
    let wf_dir = repo.path().join(".awman").join("workflows");
    write_workflow_state(&wf_dir, "done.json", true);
    write_workflow_state(&wf_dir, "pending.json", false);
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(awman_bin())
        .args(["clean", "--yes"])
        .current_dir(repo.path())
        .env("AWMAN_CONFIG_HOME", home.path())
        .stdin(null_stdin())
        .output()
        .expect("awman clean --yes");

    assert!(
        out.status.success(),
        "`awman clean --yes` must exit 0; exit: {:?}; stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !wf_dir.join("done.json").exists(),
        "completed workflow file must be deleted"
    );
    assert!(
        wf_dir.join("pending.json").exists(),
        "non-terminal workflow must be preserved"
    );
}

/// `awman clean --yes` on a clean repo exits 0 and prints "Nothing to clean."
#[test]
fn clean_yes_empty_repo_exits_zero() {
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(awman_bin())
        .args(["clean", "--yes"])
        .current_dir(repo.path())
        .env("AWMAN_CONFIG_HOME", home.path())
        .stdin(null_stdin())
        .output()
        .expect("awman clean --yes (empty)");

    assert!(
        out.status.success(),
        "`awman clean --yes` on empty repo must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `awman clean --dry-run --yes` — `--yes` is silently ignored with dry-run.
/// Migration safety copies belong to the old API root and are the only
/// database files clean may remove. The live shared database stays untouched.
#[test]
fn clean_removes_pre_migration_backups_and_preserves_live_database() {
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());
    let home = tempfile::tempdir().unwrap();
    let api_root = home.path().join("api");
    let data_root = home.path().join("data");
    std::fs::create_dir_all(&api_root).unwrap();
    std::fs::create_dir_all(&data_root).unwrap();
    for suffix in ["", "-wal", "-shm"] {
        std::fs::write(
            api_root.join(format!("awman.db{suffix}.pre-migration")),
            format!("backup-{suffix}"),
        )
        .unwrap();
    }
    let live_path = data_root.join("awman.db");
    let live_bytes = b"live database must survive clean".to_vec();
    std::fs::write(&live_path, &live_bytes).unwrap();

    let out = Command::new(awman_bin())
        .args(["clean", "--yes"])
        .current_dir(repo.path())
        .env("AWMAN_CONFIG_HOME", home.path())
        .stdin(null_stdin())
        .output()
        .expect("awman clean");
    assert!(
        out.status.success(),
        "awman clean must remove migration backups; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    for suffix in ["", "-wal", "-shm"] {
        assert!(
            !api_root
                .join(format!("awman.db{suffix}.pre-migration"))
                .exists(),
            "pre-migration backup {suffix:?} must be removed"
        );
    }
    assert_eq!(
        std::fs::read(&live_path).unwrap(),
        live_bytes,
        "clean must not touch the live shared database"
    );
}

#[test]
fn clean_dry_run_and_yes_deletes_nothing() {
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());
    let wf_dir = repo.path().join(".awman").join("workflows");
    write_workflow_state(&wf_dir, "done.json", true);
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(awman_bin())
        .args(["clean", "--dry-run", "--yes"])
        .current_dir(repo.path())
        .env("AWMAN_CONFIG_HOME", home.path())
        .stdin(null_stdin())
        .output()
        .expect("awman clean --dry-run --yes");

    assert!(
        out.status.success(),
        "`awman clean --dry-run --yes` must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wf_dir.join("done.json").exists(),
        "dry-run must not delete files even when --yes is also passed"
    );
}
