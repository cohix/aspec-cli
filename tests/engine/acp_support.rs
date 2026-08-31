//! ACP command-boundary integration tests.
//!
//! These use the real `awman` binary and a per-test fake container CLI.  The
//! fake records argv and implements only the image/setup probes plus the
//! minimal ACP initialize exchange needed by `chat`; no Docker daemon is
//! required.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn awman_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_awman"))
}

struct FakeDocker {
    bin_dir: tempfile::TempDir,
    log: PathBuf,
    home: tempfile::TempDir,
}

impl FakeDocker {
    fn new() -> Self {
        let bin_dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let log = bin_dir.path().join("docker.log");
        let script = bin_dir.path().join("docker");
        // `run` exits after the first request for ordinary workflow launches;
        // ACP chat gets initialize/session-new responses and exits on cancel.
        let body = r##"#!/bin/sh
set -eu
log="${AWMAN_TEST_DOCKER_LOG:?}"
printf '%s\n' "$*" >> "$log"
case "${1:-}" in
  info)
    exit 0
    ;;
  image)
    printf 'HOME=/root\n'
    exit 0
    ;;
  build)
    exit 0
    ;;
  run)
    if printf '%s\n' "$*" | grep -q -- '--acp'; then
      # Direct `chat`/`exec prompt` ACP: answer the initialize/session-new
      # handshake and exit on cancel. (Workflow ACP is rejected at pre-flight,
      # so no `--acp` container is ever spawned for a workflow.)
      while IFS= read -r line; do
        case "$line" in
          *'"method":"initialize"'*)
            printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}'
            ;;
          *'"method":"session/new"'*)
            printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"fake-session"}}'
            ;;
          *'"method":"session/cancel"'*)
            exit 0
            ;;
          *)
            exit 0
            ;;
        esac
      done
    fi
    exit 0
    ;;
  stop|rm)
    exit 0
    ;;
esac
exit 0
"##;
        fs::write(&script, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut mode = fs::metadata(&script).unwrap().permissions();
            mode.set_mode(0o755);
            fs::set_permissions(&script, mode).unwrap();
        }
        Self { bin_dir, log, home }
    }

    fn command(&self, repo: &Path, args: &[&str]) -> Output {
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.bin_dir.path().to_path_buf()];
        paths.extend(std::env::split_paths(&old_path));
        let path = std::env::join_paths(paths).unwrap();
        Command::new(awman_bin())
            .current_dir(repo)
            .args(args)
            .env("PATH", path)
            .env("HOME", self.home.path())
            .env("AWMAN_CONFIG_HOME", self.home.path())
            .env("AWMAN_TEST_DOCKER_LOG", &self.log)
            .output()
            .unwrap()
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn run_lines(&self) -> Vec<String> {
        self.log()
            .lines()
            .filter(|line| line.starts_with("run "))
            .map(str::to_owned)
            .collect()
    }
}

fn git_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .current_dir(repo.path())
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git command failed: {args:?}");
    };
    run(&["init", "--quiet"]);
    run(&["config", "user.email", "tests@example.invalid"]);
    run(&["config", "user.name", "awman tests"]);
    fs::write(repo.path().join("README.md"), "test\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "--quiet", "-m", "test"]);
    repo
}

fn write_repo_config(repo: &Path, value: &str) {
    fs::create_dir_all(repo.join(".awman")).unwrap();
    fs::write(repo.join(".awman/config.json"), value).unwrap();
}

fn write_agent_dockerfiles(repo: &Path, agents: &[&str]) {
    fs::create_dir_all(repo.join(".awman")).unwrap();
    for agent in agents {
        fs::write(
            repo.join(format!(".awman/Dockerfile.{agent}")),
            "FROM scratch\n",
        )
        .unwrap();
    }
}

#[test]
fn chat_acp_unsupported_agent_fails_before_container_spawn() {
    let repo = git_repo();
    let fake = FakeDocker::new();
    let out = fake.command(
        repo.path(),
        &["chat", "--agent", "codex", "--launch-mode", "acp"],
    );
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("codex"), "error must name agent: {text}");
    assert!(
        fake.run_lines().is_empty(),
        "unsupported ACP must not spawn a container; log: {}",
        fake.log()
    );
}

#[test]
fn chat_acp_cline_uses_acp_entrypoint_and_piped_stdin() {
    let repo = git_repo();
    write_agent_dockerfiles(repo.path(), &["cline"]);
    write_repo_config(repo.path(), r#"{"auth":"none"}"#);
    let fake = FakeDocker::new();
    let out = fake.command(
        repo.path(),
        &["chat", "--agent", "cline", "--launch-mode", "acp"],
    );
    assert!(
        out.status.success(),
        "fake ACP chat should complete: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run = fake
        .run_lines()
        .into_iter()
        .next()
        .expect("chat must spawn one container");
    assert!(run.split_whitespace().any(|arg| arg == "-i"), "argv: {run}");
    assert!(
        !run.split_whitespace().any(|arg| arg == "-it"),
        "argv: {run}"
    );
    assert!(run.contains("cline --acp"), "argv: {run}");
}

#[test]
fn exec_workflow_acp_error_fails_preflight_without_run() {
    let repo = git_repo();
    write_repo_config(repo.path(), r#"{"launchMode":"acp","auth":"none"}"#);
    let workflow = r#"
[[step]]
name = "supported"
agent = "cline"
prompt = "one"

[[step]]
name = "unsupported"
agent = "claude"
prompt = "two"
"#;
    fs::write(repo.path().join("workflow.toml"), workflow).unwrap();
    let fake = FakeDocker::new();
    let out = fake.command(
        repo.path(),
        &["exec", "workflow", "workflow.toml", "--non-interactive"],
    );
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("unsupported"), "error must name step: {text}");
    assert!(text.contains("claude"), "error must name agent: {text}");
    assert!(
        fake.run_lines().is_empty(),
        "workflow preflight must launch no steps; log: {}",
        fake.log()
    );
}

#[test]
fn exec_workflow_acp_stdio_fallback_launches_each_step_in_stdio() {
    // With `launchModeFallback: stdio`, a workflow of unsupported-agent steps
    // downgrades every step to ordinary stdio and runs them all. No step
    // resolves to ACP, so the not-yet-implemented workflow-ACP guard stays out
    // of the way — the whole workflow completes over PTY/stdio.
    let repo = git_repo();
    write_agent_dockerfiles(repo.path(), &["claude", "codex"]);
    write_repo_config(repo.path(), r#"{"launchMode":"acp","auth":"none"}"#);
    let workflow = r#"
[[step]]
name = "first"
agent = "claude"
prompt = "one"

[[step]]
name = "second"
agent = "codex"
prompt = "two"
"#;
    fs::write(repo.path().join("workflow.toml"), workflow).unwrap();
    let fake = FakeDocker::new();
    fs::write(
        fake.home.path().join("config.json"),
        r#"{"launchModeFallback":"stdio"}"#,
    )
    .unwrap();
    let out = fake.command(
        repo.path(),
        &["exec", "workflow", "workflow.toml", "--non-interactive"],
    );
    assert!(
        out.status.success(),
        "stdio fallback workflow should run with fake docker: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let runs = fake.run_lines();
    assert_eq!(runs.len(), 2, "both steps must launch; log: {}", fake.log());
    assert!(
        !runs.iter().any(|line| line.contains("--acp")),
        "no step may launch over ACP after stdio fallback: {runs:?}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let fallback_warnings = stderr
        .lines()
        .filter(|line| line.contains("does not support ACP"))
        .count();
    assert_eq!(
        fallback_warnings, 2,
        "one downgrade warning per unsupported step: {stderr}"
    );
}

#[test]
fn exec_workflow_acp_supported_step_rejected_before_any_run() {
    // An ACP-capable step (cline) under `launchMode: acp` cannot be driven as a
    // workflow ACP session yet, so pre-flight rejects the whole workflow before
    // launching anything — no `run` line is ever logged. This is the fix for
    // the "workflow launches an ACP container it never talks to" blocker.
    let repo = git_repo();
    write_agent_dockerfiles(repo.path(), &["cline"]);
    write_repo_config(repo.path(), r#"{"launchMode":"acp","auth":"none"}"#);
    let workflow = r#"
[[step]]
name = "solo"
agent = "cline"
prompt = "one"
"#;
    fs::write(repo.path().join("workflow.toml"), workflow).unwrap();
    let fake = FakeDocker::new();
    fs::write(
        fake.home.path().join("config.json"),
        r#"{"launchModeFallback":"stdio"}"#,
    )
    .unwrap();
    let out = fake.command(
        repo.path(),
        &["exec", "workflow", "workflow.toml", "--non-interactive"],
    );
    assert!(!out.status.success(), "workflow ACP must fail pre-flight");
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(
        text.contains("not yet supported for workflow"),
        "error must explain workflow ACP is unimplemented: {text}"
    );
    assert!(
        fake.run_lines().is_empty(),
        "no container may spawn for a rejected ACP workflow; log: {}",
        fake.log()
    );
}
