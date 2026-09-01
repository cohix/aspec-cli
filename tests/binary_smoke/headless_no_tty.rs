//! E2E: verify the `awman` binary behaves non-interactively when stdin is
//! not a TTY (CI/CD pipelines, cron, etc.). Replaces the stale
//! `tests/headless_e2e.rs` which referenced the removed
//! `awman::commands::headless::db` API.
//!
//! The check is that `awman exec workflow <missing.toml> --yolo` with stdin
//! redirected from `/dev/null` exits promptly with a non-zero status — it
//! must NOT hang waiting for an interactive prompt. This proves
//! `effective_non_interactive()` auto-enforces correctly when no TTY is
//! present.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn awman_bin() -> std::path::PathBuf {
    crate::awman_bin()
}

/// Open `/dev/null` on Unix; on Windows fall back to `NUL`. Used to detach
/// the child's stdin from the test process so the binary sees no TTY.
fn null_stdin() -> Stdio {
    #[cfg(unix)]
    {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .open("/dev/null")
            .expect("open /dev/null");
        Stdio::from(file)
    }
    #[cfg(windows)]
    {
        Stdio::null()
    }
}

/// `awman exec workflow <missing>.toml --yolo` with `/dev/null` as stdin
/// must exit (non-zero, because the file is missing) within a few seconds,
/// not hang on a prompt.
#[test]
#[cfg(unix)]
fn exec_workflow_yolo_no_tty_exits_without_prompting() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workflow_path = tmp.path().join("missing.toml");

    let mut child = Command::new(awman_bin())
        .args([
            "exec",
            "workflow",
            workflow_path.to_str().unwrap(),
            "--yolo",
        ])
        .stdin(null_stdin())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("RUST_LOG", "off")
        .spawn()
        .expect("failed to spawn awman");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // We expect non-zero — the workflow file doesn't exist. The
                // point is that the binary EXITED rather than hung.
                assert!(
                    !status.success(),
                    "expected non-zero exit for a missing workflow file"
                );
                return;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "awman exec workflow --yolo hung for 10s with no TTY on stdin — \
                         non-interactive auto-enforcement is not working"
                    );
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("try_wait failed: {e}"),
        }
    }
}

/// `awman --help` with `/dev/null` as stdin should print help and exit 0 —
/// trivial baseline that the binary doesn't break on a non-TTY stdin.
#[test]
fn awman_help_with_no_tty_stdin_exits_zero() {
    let out = Command::new(awman_bin())
        .arg("--help")
        .stdin(null_stdin())
        .output()
        .expect("failed to spawn awman --help");
    assert!(
        out.status.success(),
        "awman --help should exit 0 even with no TTY on stdin"
    );
}
