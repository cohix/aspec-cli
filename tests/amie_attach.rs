//! WI 0101 — `AgentRuntimeEngine::attach` against a container started by a
//! **different process**.
//!
//! Starts a plain container with a raw `docker run` (not through awman at
//! all — the point is that awman never spawned it), attaches to it via
//! `ContainerRuntime::attach`, proves the resulting `AgentInstance`'s
//! `run_with_frontend` streams live output (round-tripping a command through
//! the attached shell's stdin/stdout), and proves that ending the attach
//! session (`AgentExecution::cancel`) does **not** stop the target container
//! — only the local `docker exec` client dies, matching `AttachExecution`'s
//! documented contract (`src/engine/container/docker.rs`).
//!
//! Gated on Docker availability; skips cleanly otherwise.

use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt;

use awman::data::message::{UserMessage, UserMessageSink};
use awman::data::session::AgentHandle;
use awman::engine::agent_runtime::frontend::{AgentFrontend, AgentIo, AgentProgress, AgentStatus};
use awman::engine::container::ContainerRuntime;

fn docker_available() -> bool {
    Command::new("docker")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[derive(Clone, Default)]
struct Captured {
    stdout: Arc<Mutex<Vec<u8>>>,
    stdin_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>>,
}

struct TestAttachFrontend {
    captured: Captured,
}

impl UserMessageSink for TestAttachFrontend {
    fn write_message(&mut self, _msg: UserMessage) {}
    fn replay_queued(&mut self) {}
}

impl AgentFrontend for TestAttachFrontend {
    fn report_status(&mut self, _status: AgentStatus) {}
    fn report_progress(&mut self, _progress: AgentProgress) {}

    fn take_io(&mut self) -> AgentIo {
        let (stdout_tx, mut stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stderr_tx, mut stderr_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (_resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();

        *self.captured.stdin_tx.lock().unwrap() = Some(stdin_tx.clone());

        let buf = self.captured.stdout.clone();
        tokio::spawn(async move {
            while let Some(chunk) = stdout_rx.recv().await {
                buf.lock().unwrap().extend_from_slice(&chunk);
            }
        });
        tokio::spawn(async move { while stderr_rx.recv().await.is_some() {} });

        AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            // Interactive (PTY) path: a piped, non-interactive `docker exec`
            // closes stdin immediately after wiring (see
            // `spawn_piped_attach`'s doc comment), which would prevent this
            // test from driving the attached shell at all.
            resize: Some(resize_rx),
            initial_size: Some((80, 24)),
        }
    }
}

fn docker_inspect_running(name: &str) -> Option<bool> {
    let out = Command::new("docker")
        .args(["inspect", "--format", "{{.State.Running}}", name])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim() == "true")
}

#[tokio::test]
async fn attach_to_foreign_container_streams_output_and_exit_does_not_stop_it() {
    if !docker_available() {
        eprintln!("SKIP: Docker not available");
        return;
    }
    if !Command::new("docker")
        .args(["pull", "alpine:latest"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        eprintln!("SKIP: docker pull alpine:latest failed (no network?)");
        return;
    }

    let name = format!(
        "awman-attach-test-{}",
        std::process::id().wrapping_add(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        )
    );

    // Started by an entirely separate process — a raw `docker run`, never
    // going through awman's own build/run path.
    let status = Command::new("docker")
        .args([
            "run",
            "-d",
            "--name",
            &name,
            "-w",
            "/workspace",
            "alpine:latest",
            "sleep",
            "300",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("docker run");
    assert!(
        status.success(),
        "docker run must start the foreign container"
    );

    let cleanup = |name: &str| {
        let _ = Command::new("docker")
            .args(["rm", "-f", name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    };

    let result = std::panic::AssertUnwindSafe(async {
        let runtime = ContainerRuntime::docker();
        let handle = AgentHandle {
            id: name.clone(),
            image_tag: "alpine:latest".into(),
            name: name.clone(),
            started_at: chrono::Utc::now(),
        };

        let instance = runtime.attach(&handle).expect("attach must succeed");
        let captured = Captured::default();
        let frontend = TestAttachFrontend {
            captured: captured.clone(),
        };
        let mut execution = instance
            .run_with_frontend(Box::new(frontend))
            .expect("run_with_frontend must succeed");

        // Give the exec'd shell a moment to come up, then drive it — this is
        // what proves the bridge is genuinely live, not just "didn't error".
        tokio::time::sleep(Duration::from_millis(800)).await;
        let stdin_tx = captured
            .stdin_tx
            .lock()
            .unwrap()
            .clone()
            .expect("take_io must have handed back a stdin sender");
        stdin_tx
            .send(b"echo attach-stream-marker-xyz\n".to_vec())
            .expect("stdin send must succeed while the shell is alive");

        let mut waited = Duration::ZERO;
        let step = Duration::from_millis(200);
        loop {
            if String::from_utf8_lossy(&captured.stdout.lock().unwrap())
                .contains("attach-stream-marker-xyz")
            {
                break;
            }
            if waited >= Duration::from_secs(10) {
                panic!(
                    "attached session never streamed the echoed marker back; got: {}",
                    String::from_utf8_lossy(&captured.stdout.lock().unwrap())
                );
            }
            tokio::time::sleep(step).await;
            waited += step;
        }

        // End the attach session. This must kill only the local `docker
        // exec` client, never the target container.
        execution.cancel().expect("cancel must succeed");
        let _ = tokio::time::timeout(Duration::from_secs(10), execution.wait()).await;
    })
    .catch_unwind()
    .await;

    // The target container must still be running — attach's exit must never
    // stop a container this process did not start.
    let still_running = docker_inspect_running(&name);
    cleanup(&name);

    result.expect("attach test body must not panic");
    assert_eq!(
        still_running,
        Some(true),
        "the foreign container must still be running after the attach session ended"
    );
}
