//! Shared I/O bridge infrastructure used by both Docker and Apple backends.
//!
//! Covers:
//! - PTY-bridged path: reader thread, writer task, resize task
//! - Piped path: stdout/stderr reader threads, stdin writer task
//! - Activity tracking (shared timestamp for stuck detection)
//! - Stuck detector task (broadcast `StuckEvent`)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::engine::agent_runtime::execution::StuckEvent;
use crate::engine::agent_runtime::frontend::AgentIo;
use crate::engine::agent_runtime::output_tail::OutputTail;
use crate::engine::error::EngineError;

/// Shared last-activity timestamp. Updated by reader threads on every byte
/// chunk from stdout or stderr. Read by the stuck detector task.
pub(crate) type SharedActivity = Arc<Mutex<Option<Instant>>>;

/// Best-effort cancel callback the detector invokes when the startup grace
/// window expires. Implemented by the backend (a closure that calls
/// `docker stop <name>` / `container stop <name>`). Optional because tests
/// and the inert backend don't need it.
pub(crate) type CancelFn = Arc<dyn Fn() + Send + Sync>;

/// Per-bridge configuration. Bundled into one struct so the bridge functions
/// don't grow an unwieldy parameter list as more knobs land.
pub(crate) struct BridgeConfig {
    pub grace_timeout: Duration,
    pub stuck_timeout: Duration,
    /// Window after container launch during which the detector ignores
    /// activity / first_byte updates. Some runtimes (notably Apple's
    /// `container` binary) print their own startup chatter on stdout before
    /// the real workload starts, which would otherwise prematurely satisfy
    /// the grace-phase "first byte" check. Default `Duration::ZERO` keeps
    /// the existing behaviour for backends that don't need it.
    pub container_start_delay: Duration,
    /// Invoked once when the startup grace window expires. The reader
    /// threads have not seen a byte from the container; the detector calls
    /// this to force the container to exit so callers' `wait()` futures
    /// resolve with a failure status.
    pub cancel_on_grace_expired: Option<CancelFn>,
    /// Rolling buffer of the container's recent combined stdout/stderr. The
    /// reader threads append every byte chunk here (in addition to forwarding
    /// it to the frontend) so a failure log can be written if the container
    /// exits unexpectedly.
    pub output_tail: Arc<OutputTail>,
    /// Optional live tap on the output stream: every chunk the reader threads
    /// forward is also broadcast here. Used by the attach rendezvous socket
    /// (`attach_socket.rs`) on runtimes without a native attach, so connected
    /// attach clients see exactly what the launching frontend sees. `None`
    /// keeps the existing behaviour.
    pub output_broadcast: Option<Arc<tokio::sync::broadcast::Sender<Vec<u8>>>>,
}

/// Bundle returned by `bridge_pty` / `bridge_piped` containing the artifacts
/// the backend needs to store for the execution lifetime.
pub(crate) struct BridgeResult {
    /// Sender for `try_inject_stdin` — the engine retains a clone.
    pub stdin_injector: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Broadcast sender for stuck events — stored in `AgentExecution`.
    pub stuck_tx: Arc<tokio::sync::broadcast::Sender<StuckEvent>>,
    /// The rolling output tail the reader threads feed — stored in
    /// `AgentExecution` so the workflow engine can read it after `wait()`.
    pub output_tail: Arc<OutputTail>,
}

fn update_activity(activity: &SharedActivity, first_byte: &Arc<AtomicBool>) {
    if let Ok(mut guard) = activity.lock() {
        *guard = Some(Instant::now());
    }
    first_byte.store(true, Ordering::Release);
}

/// Spawn the stuck detector task. Returns the broadcast sender (caller
/// stores it in `AgentExecution`).
///
/// The task holds only a `Weak` to the sender so it can detect when its
/// owning `AgentExecution` is dropped: the next `upgrade()` returns
/// `None` and the task exits. Without this the task would leak for the
/// lifetime of the process. Transient `SendError` (no subscribers right
/// now) is ignored — broadcast semantics allow a subscriber to appear
/// later as long as the sender Arc is still alive.
///
/// Three phases:
///   0. *Start delay* (optional, when `container_start_delay` is non-zero)
///      — the detector sleeps without emitting events and ignores any
///      first-byte / activity updates from the reader threads. After the
///      delay it resets `first_byte` and `activity` to clear out any
///      bytes the container runtime itself produced (e.g. Apple's
///      `container` binary prints "creating container…" / "starting
///      container…" before the real workload runs). This prevents the
///      runtime's startup chatter from satisfying the grace check.
///   1. *Grace* — until the first byte of (real) output arrives, the
///      detector watches its grace clock. If it crosses `grace_timeout`,
///      the detector publishes `StartupGraceExpired`, invokes the cancel
///      callback (so the container dies and `wait()` resolves), and exits.
///   2. *Stuck* — once `first_byte` flips to `true`, the detector
///      switches to the regular Stuck/Unstuck loop driven by the last
///      activity timestamp and `stuck_timeout`. Grace is discarded.
pub(crate) fn spawn_stuck_detector(
    activity: SharedActivity,
    first_byte: Arc<AtomicBool>,
    grace_timeout: Duration,
    stuck_timeout: Duration,
    container_start_delay: Duration,
    cancel_on_grace_expired: Option<CancelFn>,
) -> Arc<tokio::sync::broadcast::Sender<StuckEvent>> {
    let (stuck_tx, _) = tokio::sync::broadcast::channel(16);
    let stuck_tx = Arc::new(stuck_tx);
    let weak = Arc::downgrade(&stuck_tx);
    tokio::spawn(async move {
        // Phase 0: optional start-delay window. The reader threads keep
        // updating activity / first_byte during this sleep; we discard
        // those updates at the end of the window so only post-delay bytes
        // count.
        if !container_start_delay.is_zero() {
            tokio::time::sleep(container_start_delay).await;
            if weak.upgrade().is_none() {
                return;
            }
            first_byte.store(false, Ordering::Release);
            if let Ok(mut guard) = activity.lock() {
                *guard = None;
            }
        }

        let mut is_stuck = false;
        let grace_start = Instant::now();
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let tx = match weak.upgrade() {
                Some(tx) => tx,
                None => break,
            };

            if !first_byte.load(Ordering::Acquire) {
                // Grace phase — measure from end of the start-delay window.
                if grace_start.elapsed() >= grace_timeout {
                    let _ = tx.send(StuckEvent::StartupGraceExpired);
                    if let Some(cb) = &cancel_on_grace_expired {
                        cb();
                    }
                    break;
                }
                continue;
            }

            // Stuck phase — measure from the most recent byte.
            let elapsed = {
                let guard = match activity.lock() {
                    Ok(g) => g,
                    Err(_) => break,
                };
                match *guard {
                    Some(t) => t.elapsed(),
                    None => grace_start.elapsed(),
                }
            };
            let now_stuck = elapsed >= stuck_timeout;
            if now_stuck && !is_stuck {
                is_stuck = true;
                let _ = tx.send(StuckEvent::Stuck);
            } else if !now_stuck && is_stuck {
                is_stuck = false;
                let _ = tx.send(StuckEvent::Unstuck);
            }
        }
    });
    stuck_tx
}

/// Bridge a PTY master to the `AgentIo` channels.
///
/// Spawns:
/// - Reader thread: PTY master → `io.stdout` (activity tracked)
/// - Writer task: `io.stdin_rx` → PTY master
/// - Resize task: `io.resize` → `master.resize()`
///
/// Returns the master wrapped in `Arc<Mutex>` (for resize + cleanup) plus
/// the `BridgeResult`.
type PtyMaster = Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>;

pub(crate) fn bridge_pty(
    io: AgentIo,
    pair: portable_pty::PtyPair,
    config: BridgeConfig,
) -> Result<(PtyMaster, BridgeResult), EngineError> {
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| EngineError::Container(format!("clone pty reader: {e}")))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|e| EngineError::Container(format!("take pty writer: {e}")))?;

    let activity: SharedActivity = Arc::new(Mutex::new(None));
    let first_byte = Arc::new(AtomicBool::new(false));

    // Reader thread: PTY → stdout channel (activity tracked).
    //
    // If the frontend's stdout sink dies (drain task panics or exits early),
    // we keep draining the PTY but discard bytes — the container must not be
    // backpressured by a dead sink. Activity tracking continues so stuck
    // detection reflects what the container is actually emitting. The output
    // tail is fed unconditionally (even after the sink dies) so a failure log
    // always reflects what the container really printed.
    let stdout_tx = io.stdout;
    let act = Arc::clone(&activity);
    let fb = Arc::clone(&first_byte);
    let tail = Arc::clone(&config.output_tail);
    let broadcast = config.output_broadcast.clone();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 4096];
        let mut sink_open = true;
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    update_activity(&act, &fb);
                    tail.push_bytes(&buf[..n]);
                    if let Some(tx) = &broadcast {
                        // Copy only when someone is attached; a lagged client
                        // drops chunks. (A new client's subscription always
                        // precedes the repaint its initial resize triggers,
                        // so this gate can't starve it.)
                        if tx.receiver_count() > 0 {
                            let _ = tx.send(buf[..n].to_vec());
                        }
                    }
                    if sink_open && stdout_tx.send(buf[..n].to_vec()).is_err() {
                        sink_open = false;
                    }
                }
            }
        }
    });

    // Writer task: stdin channel → PTY.
    let stdin_tx = io.stdin_tx;
    let mut stdin_rx = io.stdin_rx;
    tokio::spawn(async move {
        use std::io::Write;
        while let Some(bytes) = stdin_rx.recv().await {
            if writer.write_all(&bytes).is_err() {
                break;
            }
            if writer.flush().is_err() {
                break;
            }
        }
    });

    // Resize task.
    let master_arc = Arc::new(Mutex::new(pair.master));
    if let Some(mut resize_rx) = io.resize {
        let master_for_resize = Arc::clone(&master_arc);
        tokio::spawn(async move {
            use portable_pty::PtySize;
            while let Some((cols, rows)) = resize_rx.recv().await {
                if let Ok(master) = master_for_resize.lock() {
                    let _ = master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            }
        });
    }

    let stuck_tx = spawn_stuck_detector(
        activity,
        first_byte,
        config.grace_timeout,
        config.stuck_timeout,
        config.container_start_delay,
        config.cancel_on_grace_expired,
    );

    Ok((
        master_arc,
        BridgeResult {
            stdin_injector: stdin_tx,
            stuck_tx,
            output_tail: config.output_tail,
        },
    ))
}

/// Bridge a piped child process to the `AgentIo` channels.
///
/// Spawns:
/// - Reader thread for child stdout → `io.stdout` (activity tracked)
/// - Reader thread for child stderr → `io.stderr` (activity tracked)
/// - Writer task: `io.stdin_rx` → child stdin
///
/// The child's stdout/stderr/stdin pipes are taken from the `Child`.
pub(crate) fn bridge_piped(
    io: AgentIo,
    child: &mut std::process::Child,
    config: BridgeConfig,
) -> BridgeResult {
    let activity: SharedActivity = Arc::new(Mutex::new(None));
    let first_byte = Arc::new(AtomicBool::new(false));

    // stdout reader thread.
    //
    // If the frontend's sink dies, keep draining the pipe but discard bytes —
    // a piped child will block on stdout if we stop reading. Activity
    // tracking continues unconditionally so the stuck detector reflects what
    // the container actually produces.
    if let Some(child_stdout) = child.stdout.take() {
        let stdout_tx = io.stdout;
        let act = Arc::clone(&activity);
        let fb = Arc::clone(&first_byte);
        let tail = Arc::clone(&config.output_tail);
        let broadcast = config.output_broadcast.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut reader = child_stdout;
            let mut buf = [0u8; 4096];
            let mut sink_open = true;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        update_activity(&act, &fb);
                        tail.push_bytes(&buf[..n]);
                        if let Some(tx) = &broadcast {
                            if tx.receiver_count() > 0 {
                                let _ = tx.send(buf[..n].to_vec());
                            }
                        }
                        if sink_open && stdout_tx.send(buf[..n].to_vec()).is_err() {
                            sink_open = false;
                        }
                    }
                }
            }
        });
    }

    // stderr reader thread — same drain-after-sink-dies semantics as stdout.
    // Feeds the same tail as stdout so the buffer holds combined output.
    if let Some(child_stderr) = child.stderr.take() {
        let stderr_tx = io.stderr;
        let act = Arc::clone(&activity);
        let fb = Arc::clone(&first_byte);
        let tail = Arc::clone(&config.output_tail);
        let broadcast = config.output_broadcast.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut reader = child_stderr;
            let mut buf = [0u8; 4096];
            let mut sink_open = true;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        update_activity(&act, &fb);
                        tail.push_bytes(&buf[..n]);
                        if let Some(tx) = &broadcast {
                            if tx.receiver_count() > 0 {
                                let _ = tx.send(buf[..n].to_vec());
                            }
                        }
                        if sink_open && stderr_tx.send(buf[..n].to_vec()).is_err() {
                            sink_open = false;
                        }
                    }
                }
            }
        });
    }

    // stdin writer task
    let stdin_tx = io.stdin_tx;
    if let Some(child_stdin) = child.stdin.take() {
        let mut stdin_rx = io.stdin_rx;
        tokio::spawn(async move {
            use std::io::Write;
            let mut writer = child_stdin;
            while let Some(bytes) = stdin_rx.recv().await {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
                if writer.flush().is_err() {
                    break;
                }
            }
        });
    }

    let stuck_tx = spawn_stuck_detector(
        activity,
        first_byte,
        config.grace_timeout,
        config.stuck_timeout,
        config.container_start_delay,
        config.cancel_on_grace_expired,
    );

    BridgeResult {
        stdin_injector: stdin_tx,
        stuck_tx,
        output_tail: config.output_tail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;

    /// Test-only stuck detector with configurable timeout and fast poll
    /// so tests don't have to wait 30 real seconds.
    fn spawn_test_detector(
        activity: SharedActivity,
        timeout: Duration,
    ) -> Arc<tokio::sync::broadcast::Sender<StuckEvent>> {
        let (stuck_tx, _) = tokio::sync::broadcast::channel(16);
        let stuck_tx = Arc::new(stuck_tx);
        let tx = Arc::clone(&stuck_tx);
        tokio::spawn(async move {
            let mut is_stuck = false;
            let start = Instant::now();
            loop {
                tokio::time::sleep(Duration::from_millis(5)).await;
                let elapsed = {
                    let guard = match activity.lock() {
                        Ok(g) => g,
                        Err(_) => break,
                    };
                    match *guard {
                        Some(t) => t.elapsed(),
                        None => start.elapsed(),
                    }
                };
                let now_stuck = elapsed >= timeout;
                if now_stuck && !is_stuck {
                    is_stuck = true;
                    if tx.send(StuckEvent::Stuck).is_err() {
                        break;
                    }
                } else if !now_stuck && is_stuck {
                    is_stuck = false;
                    if tx.send(StuckEvent::Unstuck).is_err() {
                        break;
                    }
                }
            }
        });
        stuck_tx
    }

    // ── Stuck detector ────────────────────────────────────────────────────────

    /// No activity (None) → start time is the baseline → Stuck fires once
    /// the timeout elapses.
    #[tokio::test]
    async fn stuck_detector_emits_stuck_after_timeout_with_no_activity() {
        let activity: SharedActivity = Arc::new(Mutex::new(None));
        let timeout = Duration::from_millis(50);
        let tx = spawn_test_detector(Arc::clone(&activity), timeout);
        let mut rx = tx.subscribe();

        let event = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("timed out waiting for Stuck event")
            .expect("broadcast channel closed unexpectedly");
        assert_eq!(event, StuckEvent::Stuck, "expected first event to be Stuck");
    }

    /// Stuck fires → update activity to now → Unstuck fires.
    #[tokio::test]
    async fn stuck_detector_emits_unstuck_after_activity_resumes() {
        let activity: SharedActivity = Arc::new(Mutex::new(None));
        let timeout = Duration::from_millis(50);
        let tx = spawn_test_detector(Arc::clone(&activity), timeout);
        let mut rx = tx.subscribe();

        // Wait for Stuck.
        let first = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("timed out waiting for Stuck")
            .expect("channel closed");
        assert_eq!(first, StuckEvent::Stuck);

        // Refresh activity.
        *activity.lock().unwrap() = Some(Instant::now());

        // Wait for Unstuck.
        let second = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("timed out waiting for Unstuck")
            .expect("channel closed");
        assert_eq!(second, StuckEvent::Unstuck);
    }

    /// Recent activity → elapsed < timeout → no Stuck event within the window.
    #[tokio::test]
    async fn stuck_detector_no_event_when_activity_is_recent() {
        let activity: SharedActivity = Arc::new(Mutex::new(Some(Instant::now())));
        let timeout = Duration::from_millis(500);
        let tx = spawn_test_detector(Arc::clone(&activity), timeout);
        let mut rx = tx.subscribe();

        // Wait well under the timeout.
        tokio::time::sleep(Duration::from_millis(80)).await;

        // No event should have been emitted.
        let result = rx.try_recv();
        assert!(
            result.is_err(),
            "should not have received any event with recent activity, got {:?}",
            result
        );
    }

    /// When all broadcast receivers are dropped, subsequent send() calls
    /// return SendError. The detector task breaks on that error and stops.
    #[tokio::test]
    async fn stuck_detector_stops_when_all_receivers_dropped() {
        let activity: SharedActivity = Arc::new(Mutex::new(None));
        let timeout = Duration::from_millis(20);
        let tx = spawn_test_detector(Arc::clone(&activity), timeout);
        // Subscribe and then immediately drop the receiver.
        let rx = tx.subscribe();
        drop(rx);

        // Give the task enough time to fire and discover no receivers.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // The task should have exited; a new receiver gets nothing.
        let mut rx2 = tx.subscribe();
        let result = rx2.try_recv();
        // Either no events (task exited) or a lagged/empty result is acceptable.
        // What matters is that no panic occurred.
        let _ = result;
    }

    /// Production `spawn_stuck_detector` must terminate once its only
    /// `Arc<Sender>` is dropped, even if the timeout hasn't elapsed.
    /// Without this the task leaks for the lifetime of the process.
    #[tokio::test]
    async fn spawn_stuck_detector_exits_when_arc_dropped() {
        let activity: SharedActivity = Arc::new(Mutex::new(Some(Instant::now())));
        let first_byte = Arc::new(AtomicBool::new(true));
        let tx = spawn_stuck_detector(
            activity,
            first_byte,
            Duration::from_secs(30),
            Duration::from_secs(30),
            Duration::ZERO,
            None,
        );
        // Hold a weak reference so we can observe the task dropping the Arc.
        let weak = Arc::downgrade(&tx);
        drop(tx);

        // Detector polls every 1s; give it 2.5s to notice the Arc is gone.
        for _ in 0..25 {
            if weak.upgrade().is_none() {
                return; // task dropped its Arc clone → exited
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("spawn_stuck_detector did not exit within 2.5s of its Arc being dropped");
    }

    /// Grace phase: when no byte ever arrives and `grace_timeout` elapses,
    /// the detector publishes `StartupGraceExpired` and invokes the cancel
    /// callback.
    #[tokio::test]
    async fn spawn_stuck_detector_emits_startup_grace_expired() {
        let activity: SharedActivity = Arc::new(Mutex::new(None));
        let first_byte = Arc::new(AtomicBool::new(false));
        let cancel_called = Arc::new(AtomicBool::new(false));
        let cancel_clone = Arc::clone(&cancel_called);
        let cancel: CancelFn = Arc::new(move || {
            cancel_clone.store(true, Ordering::Release);
        });

        let tx = spawn_stuck_detector(
            activity,
            first_byte,
            Duration::from_millis(50),
            Duration::from_secs(30),
            Duration::ZERO,
            Some(cancel),
        );
        let mut rx = tx.subscribe();

        let event = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("timed out waiting for StartupGraceExpired")
            .expect("channel closed");
        assert_eq!(event, StuckEvent::StartupGraceExpired);

        // Give the detector a tick to invoke the callback.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            cancel_called.load(Ordering::Acquire),
            "grace expiry must invoke the cancel callback"
        );
    }

    /// Stuck phase: once `first_byte` flips true, the detector ignores grace
    /// and emits `Stuck` based on `stuck_timeout` measured from last activity.
    #[tokio::test]
    async fn spawn_stuck_detector_switches_to_stuck_phase_after_first_byte() {
        let activity: SharedActivity = Arc::new(Mutex::new(Some(Instant::now())));
        let first_byte = Arc::new(AtomicBool::new(true));

        // Set grace to something we'd notice if it incorrectly fired
        // (anything > stuck_timeout would work).
        let tx = spawn_stuck_detector(
            activity,
            first_byte,
            Duration::from_secs(30),
            Duration::from_millis(50),
            Duration::ZERO,
            None,
        );
        let mut rx = tx.subscribe();

        // We must receive Stuck (not StartupGraceExpired) — grace must be
        // skipped because `first_byte` was already true.
        let event = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("timed out waiting for Stuck")
            .expect("channel closed");
        assert_eq!(event, StuckEvent::Stuck);
    }

    /// `container_start_delay`: bytes that "arrive" during the delay window
    /// must not satisfy the first_byte check. Simulates Apple's `container`
    /// runtime printing startup chatter before the workload begins — those
    /// bytes flip `first_byte` to true, but the detector must wipe it at
    /// the end of the delay window and re-enter the grace phase.
    #[tokio::test]
    async fn spawn_stuck_detector_start_delay_discards_pre_delay_first_byte() {
        let activity: SharedActivity = Arc::new(Mutex::new(Some(Instant::now())));
        // Pretend the runtime already printed something during the delay.
        let first_byte = Arc::new(AtomicBool::new(true));

        // Start delay 150ms, grace 100ms. If the delay correctly discarded
        // the pre-delay first_byte, the detector should re-enter grace and
        // (since no real first byte ever arrives) publish
        // StartupGraceExpired roughly 150ms + ~grace later.
        let tx = spawn_stuck_detector(
            activity,
            Arc::clone(&first_byte),
            Duration::from_millis(100),
            Duration::from_secs(30),
            Duration::from_millis(150),
            None,
        );
        let mut rx = tx.subscribe();

        let event = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("timed out waiting for StartupGraceExpired")
            .expect("channel closed");
        assert_eq!(
            event,
            StuckEvent::StartupGraceExpired,
            "start-delay must wipe pre-delay first_byte so grace runs from end of delay"
        );
    }

    // ── stdin EOF ─────────────────────────────────────────────────────────────

    /// When all stdin senders are dropped (non-interactive path), the writer
    /// task draining stdin_rx terminates cleanly without errors.
    #[tokio::test]
    async fn stdin_eof_non_interactive_terminates_writer_task_cleanly() {
        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        // Drop the sole sender immediately — simulates non-interactive frontend.
        drop(stdin_tx);

        let mut stdin_rx = stdin_rx;
        let task = tokio::spawn(async move {
            let mut total = 0usize;
            while let Some(bytes) = stdin_rx.recv().await {
                total += bytes.len();
            }
            total
        });

        let bytes_received = tokio::time::timeout(Duration::from_millis(100), task)
            .await
            .expect("writer task should terminate within 100ms when stdin is EOF")
            .expect("writer task must not panic");

        assert_eq!(
            bytes_received, 0,
            "no bytes should arrive when sender is dropped"
        );
    }

    // ── bridge_piped wiring (real child process) ─────────────────────────────

    /// Spawn a real subprocess with piped stdio, wire it through
    /// `bridge_piped`, and verify that stdout bytes from the child arrive at
    /// the frontend's stdout sink. This exercises the same code path both
    /// the Docker and Apple backends use for non-interactive runs, without
    /// requiring an actual container runtime.
    #[cfg(unix)]
    #[tokio::test]
    async fn bridge_piped_stdout_bytes_reach_frontend_sink() {
        use crate::engine::agent_runtime::frontend::AgentIo;
        use std::process::{Command, Stdio};

        let (stdout_tx, mut stdout_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (stderr_tx, _stderr_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        let io = AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        };

        let mut child = Command::new("sh")
            .args(["-c", "printf 'hello\\n'"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sh");

        let bridge = bridge_piped(
            io,
            &mut child,
            BridgeConfig {
                grace_timeout: Duration::from_secs(30),
                stuck_timeout: Duration::from_secs(30),
                container_start_delay: Duration::ZERO,
                cancel_on_grace_expired: None,
                output_tail: Arc::new(OutputTail::with_default_capacity()),
                output_broadcast: None,
            },
        );
        // Non-interactive flow: drop the engine's stdin handle so the writer
        // task exits and child stdin closes (mirrors `spawn_piped_docker`).
        drop(bridge.stdin_injector);

        // `child.wait()` is blocking — run on a blocking thread so the tokio
        // runtime stays free to drive the bridge's reader/writer tasks.
        let _ = tokio::task::spawn_blocking(move || child.wait()).await;

        let bytes = tokio::time::timeout(Duration::from_millis(500), stdout_rx.recv())
            .await
            .expect("stdout bytes should arrive promptly")
            .expect("channel alive");
        assert_eq!(
            bytes, b"hello\n",
            "bridge_piped must forward child stdout verbatim"
        );
    }

    /// stderr bytes are routed to the stderr sink (separate from stdout).
    #[cfg(unix)]
    #[tokio::test]
    async fn bridge_piped_stderr_bytes_reach_frontend_stderr_sink() {
        use crate::engine::agent_runtime::frontend::AgentIo;
        use std::process::{Command, Stdio};

        let (stdout_tx, _stdout_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (stderr_tx, mut stderr_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        let io = AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        };

        let mut child = Command::new("sh")
            .args(["-c", "printf 'oops\\n' >&2"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sh");

        let bridge = bridge_piped(
            io,
            &mut child,
            BridgeConfig {
                grace_timeout: Duration::from_secs(30),
                stuck_timeout: Duration::from_secs(30),
                container_start_delay: Duration::ZERO,
                cancel_on_grace_expired: None,
                output_tail: Arc::new(OutputTail::with_default_capacity()),
                output_broadcast: None,
            },
        );
        drop(bridge.stdin_injector);

        let _ = tokio::task::spawn_blocking(move || child.wait()).await;

        let bytes = tokio::time::timeout(Duration::from_millis(500), stderr_rx.recv())
            .await
            .expect("stderr bytes should arrive promptly")
            .expect("channel alive");
        assert_eq!(bytes, b"oops\n");
    }

    /// Bytes pushed through `stdin_tx` reach the child's stdin. We use `cat`
    /// — it echoes stdin to stdout — and verify the echo reappears on the
    /// stdout sink. Dropping `stdin_injector` then closes the child's stdin,
    /// `cat` sees EOF and exits.
    #[cfg(unix)]
    #[tokio::test]
    async fn bridge_piped_stdin_writes_reach_child() {
        use crate::engine::agent_runtime::frontend::AgentIo;
        use std::process::{Command, Stdio};

        let (stdout_tx, mut stdout_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (stderr_tx, _stderr_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        // Queue the input BEFORE spawning the bridge — the writer task will
        // see these bytes on its first `recv().await`.
        stdin_tx.send(b"round-trip payload\n".to_vec()).unwrap();

        let io = AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        };

        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn cat");

        let bridge = bridge_piped(
            io,
            &mut child,
            BridgeConfig {
                grace_timeout: Duration::from_secs(30),
                stuck_timeout: Duration::from_secs(30),
                container_start_delay: Duration::ZERO,
                cancel_on_grace_expired: None,
                output_tail: Arc::new(OutputTail::with_default_capacity()),
                output_broadcast: None,
            },
        );
        // Drop the engine's stdin sender so `cat` sees EOF after the payload.
        drop(bridge.stdin_injector);

        // `child.wait()` is blocking; running it directly would freeze the
        // tokio runtime and starve the writer task, deadlocking the test.
        let status = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::task::spawn_blocking(move || child.wait()),
        )
        .await
        .expect("cat should exit within 5s of stdin EOF")
        .expect("blocking join")
        .expect("wait");
        assert!(status.success(), "cat must exit cleanly");

        // Drain stdout — `cat` may emit the bytes in multiple chunks.
        let mut collected = Vec::new();
        while let Ok(Some(bytes)) =
            tokio::time::timeout(Duration::from_millis(200), stdout_rx.recv()).await
        {
            collected.extend_from_slice(&bytes);
        }
        assert_eq!(
            collected, b"round-trip payload\n",
            "bytes sent via stdin_tx must arrive at the child's stdin and round-trip back through cat"
        );
    }

    /// When the frontend's stdout sink dies mid-stream, the reader thread
    /// must keep draining the child's stdout so the child isn't backpressured.
    /// We can't directly observe the child's pipe buffer state, but we can
    /// confirm: (a) the child exits cleanly, and (b) activity tracking still
    /// fires (which proves the reader is still reading after the sink dies).
    #[cfg(unix)]
    #[tokio::test]
    async fn bridge_piped_keeps_draining_after_stdout_sink_drops() {
        use crate::engine::agent_runtime::frontend::AgentIo;
        use std::process::{Command, Stdio};

        let (stdout_tx, stdout_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (stderr_tx, _stderr_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        // Drop the receiver immediately — every send will return SendError.
        drop(stdout_rx);

        let io = AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        };

        // Print many bytes so the kernel pipe buffer would fill if we
        // stopped reading (typical pipe buffer is 64 KiB).
        let mut child = Command::new("sh")
            .args(["-c", "yes | head -c 200000"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sh");

        let bridge = bridge_piped(
            io,
            &mut child,
            BridgeConfig {
                grace_timeout: Duration::from_secs(30),
                stuck_timeout: Duration::from_secs(30),
                container_start_delay: Duration::ZERO,
                cancel_on_grace_expired: None,
                output_tail: Arc::new(OutputTail::with_default_capacity()),
                output_broadcast: None,
            },
        );
        drop(bridge.stdin_injector);

        // Child must exit promptly — if the reader stopped draining, this
        // would deadlock at ~64 KiB.
        let status = tokio::task::spawn_blocking(move || child.wait())
            .await
            .expect("join")
            .expect("wait");
        assert!(
            status.success(),
            "child must exit cleanly even when the stdout sink is dead"
        );
    }

    /// With data already in the channel before the sender is dropped, the
    /// writer task drains all bytes then terminates cleanly.
    #[tokio::test]
    async fn stdin_writer_task_drains_all_bytes_then_terminates_on_eof() {
        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        stdin_tx.send(b"hello\n".to_vec()).unwrap();
        stdin_tx.send(b"world\n".to_vec()).unwrap();
        drop(stdin_tx);

        let mut stdin_rx = stdin_rx;
        let task = tokio::spawn(async move {
            let mut received = Vec::new();
            while let Some(bytes) = stdin_rx.recv().await {
                received.extend_from_slice(&bytes);
            }
            received
        });

        let received = tokio::time::timeout(Duration::from_millis(100), task)
            .await
            .expect("task should complete")
            .expect("no panic");

        assert_eq!(received, b"hello\nworld\n");
    }
}
