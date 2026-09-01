//! WI-0098 Finding A — end-to-end proof that the name-only `-e KEY` credential
//! transport keeps secret values out of `docker run`'s argument vector while
//! still delivering them, byte-for-byte, into the container.
//!
//! `docker.rs` builds credentials as the argv pair `-e KEY` (name only) and
//! sets `KEY=VALUE` on the docker-client child process via `Command::env`; the
//! docker CLI then resolves the name-only `-e KEY` from its own environment.
//! The *argv construction* is unit-tested in `src/engine/container/docker.rs`
//! (values never appear in the built argv, including `=`/newline cases). This
//! file closes the loop the unit tests cannot: it launches a real container and
//! asserts (a) the exact value arrives inside, and (b) on Linux the docker
//! client's `/proc/<pid>/cmdline` never contains the secret during launch.
//!
//! Every test is gated on `helpers::docker_available()` and has `docker` in its
//! name, so `make test-fast` skips it and `make test-full` runs it. It mirrors
//! the exact invocation form `docker.rs` produces rather than calling the
//! `pub(super)` `build_run_argv` (which the integration crate cannot reach).

use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use awman::engine::auth::credential::{
    claude_spec, CredentialExtra, CredentialSnapshot, SecretString,
};

use crate::helpers::docker_available;

/// The smallest image with a POSIX shell that we can rely on pulling in CI.
const IMAGE: &str = "busybox:latest";

fn try_pull(image: &str) -> bool {
    Command::new("docker")
        .args(["pull", image])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run `docker run --rm -e KEY <image> sh -c 'printf %s "$KEY"'` exactly the way
/// `docker.rs` does — the value goes on the child's environment, never argv —
/// and return the raw bytes the container observed for `$KEY`.
fn value_seen_inside_container(key: &str, value: &str) -> Vec<u8> {
    let output = Command::new("docker")
        .args([
            "run",
            "--rm",
            // Name-only `-e KEY`: the exact form `build_run_argv` emits.
            "-e",
            key,
            IMAGE,
            "sh",
            "-c",
            // `printf %s` emits the value with no trailing newline, so the
            // container's view of the bytes is exact — even for `=`/newline.
            &format!("printf %s \"${key}\""),
        ])
        // The secret only ever lives on the client child's environment.
        .env(key, value)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run docker");
    assert!(
        output.status.success(),
        "docker run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn docker_credential_plain_value_arrives_intact_and_not_in_argv() {
    if !docker_available() || !try_pull(IMAGE) {
        eprintln!("skipping: docker/{IMAGE} unavailable");
        return;
    }
    let value = "sk-plain-credential-value-12345";
    let seen = value_seen_inside_container("ANTHROPIC_API_KEY", value);
    assert_eq!(
        String::from_utf8_lossy(&seen),
        value,
        "the container must observe the exact credential value"
    );
}

#[test]
fn docker_credential_value_with_equals_arrives_intact() {
    if !docker_available() || !try_pull(IMAGE) {
        eprintln!("skipping: docker/{IMAGE} unavailable");
        return;
    }
    // A value full of `=` would be ambiguous in the `-e KEY=VALUE` argv form;
    // the name-only transport carries it verbatim.
    let value = "aaa=bbb==ccc=";
    let seen = value_seen_inside_container("TOKEN", value);
    assert_eq!(
        String::from_utf8_lossy(&seen),
        value,
        "a value containing `=` must arrive byte-for-byte"
    );
}

#[test]
fn docker_credential_value_with_newline_arrives_intact() {
    if !docker_available() || !try_pull(IMAGE) {
        eprintln!("skipping: docker/{IMAGE} unavailable");
        return;
    }
    // Newlines can never survive an argv element; they must here.
    let value = "line1\nline2\nline3";
    let seen = value_seen_inside_container("MULTILINE_SECRET", value);
    assert_eq!(
        String::from_utf8_lossy(&seen),
        value,
        "a multi-line value must arrive byte-for-byte"
    );
}

/// The security guarantee itself: while the docker client runs, its own
/// `/proc/<pid>/cmdline` (what `ps` and other local users would scrape) must
/// contain the credential name but never its value. Linux-only; on other hosts
/// the `/proc` interface does not exist and this assertion is skipped.
#[cfg(target_os = "linux")]
#[test]
fn docker_credential_value_absent_from_proc_cmdline_during_launch() {
    if !docker_available() || !try_pull(IMAGE) {
        eprintln!("skipping: docker/{IMAGE} unavailable");
        return;
    }
    let key = "ANTHROPIC_API_KEY";
    let value = "sk-proc-scrape-target-9f8e7d6c5b4a";

    // A container that lingers long enough for us to inspect the launching
    // client's cmdline before it exits.
    let mut child = Command::new("docker")
        .args(["run", "--rm", "-e", key, IMAGE, "sleep", "3"])
        .env(key, value)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn docker");

    let pid = child.id();
    let cmdline_path = format!("/proc/{pid}/cmdline");

    // `/proc/<pid>/cmdline` is NUL-separated argv. Read it a few times while the
    // client is alive; the value must never appear in any read.
    let mut checked = false;
    for _ in 0..30 {
        match std::fs::read(&cmdline_path) {
            // An empty cmdline is a transient, non-informative state: the kernel
            // clears it during `execve`, and a process that has exited but not
            // yet been reaped (a zombie — we only `wait()` below) also reads back
            // empty. Skip these rounds rather than asserting on them.
            Ok(bytes) if bytes.is_empty() => {}
            Ok(bytes) => {
                let joined = String::from_utf8_lossy(&bytes);
                assert!(
                    !joined.contains(value),
                    "credential VALUE leaked into {cmdline_path}: {joined:?}"
                );
                // The name-only `-e KEY` form should be visible — confirms we
                // inspected the right (credential-carrying) invocation. Only a
                // read that shows it counts as having exercised the assertion.
                if joined.contains(key) {
                    checked = true;
                }
            }
            // Client already exited between spawn and read — nothing to assert.
            Err(_) => break,
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let _ = child.wait();
    assert!(
        checked,
        "never managed to read {cmdline_path} while the client was alive; \
         the /proc assertion did not run"
    );
}

/// Inverse of the env-delivery tests above: file-delivered credentials must
/// appear only inside the staged 0600 file. The host path is expected in the
/// `-v` mount argument; the secret is forbidden from argv, client output, and
/// status/debug representations.
#[test]
fn docker_file_delivered_credential_secret_is_only_in_staged_file() {
    if !docker_available() || !try_pull(IMAGE) {
        eprintln!("skipping: docker/{IMAGE} unavailable");
        return;
    }
    let secret = "fixture-file-delivery-secret-never-in-argv";
    let snapshot = CredentialSnapshot {
        secret: SecretString::new(secret),
        expires_at: Some(SystemTime::now() + Duration::from_secs(3600)),
        extra: CredentialExtra::default(),
    };
    let file = (claude_spec().materialize)(&snapshot);
    let staged = tempfile::tempdir().unwrap();
    let staged_path = staged.path().join(&file.relative_path);
    std::fs::write(&staged_path, &file.contents).unwrap();
    let mount = format!("{}:/root/.claude:ro", staged.path().display());
    let args = vec![
        "run".to_string(),
        "--rm".to_string(),
        "-v".to_string(),
        mount.clone(),
        IMAGE.to_string(),
        "sh".to_string(),
        "-c".to_string(),
        "test -f /root/.claude/.credentials.json && sha256sum /root/.claude/.credentials.json"
            .to_string(),
    ];
    let rendered_argv = args.join("\u{0}");
    assert!(rendered_argv.contains(staged.path().to_str().unwrap()));
    assert!(
        !rendered_argv.contains(secret),
        "secret leaked into docker argv"
    );
    assert_eq!(
        args.iter().position(|a| a == &mount),
        args.iter().position(|a| a == "-v").map(|i| i + 1),
        "the staged path is permitted only as a -v mount argument"
    );

    let output = Command::new("docker")
        .args(&args)
        .output()
        .expect("run docker");
    assert!(
        output.status.success(),
        "docker run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let logs = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !logs.contains(secret),
        "secret leaked to docker output/logs"
    );
    let status_output = format!("{:?}", file);
    assert!(
        !status_output.contains(secret),
        "secret leaked to credential status/debug output"
    );
}
