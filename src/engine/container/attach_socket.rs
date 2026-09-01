//! Attach rendezvous socket — attach parity for container runtimes without a
//! native attach verb.
//!
//! `docker attach` works because dockerd holds every container's TTY
//! server-side, so any client can reconnect. Apple's `container` CLI has no
//! attach subcommand at all: the only process holding a running container's
//! PTY is the awman process that spawned `container run -it` under
//! portable-pty (the squad daemon for squad tasks; the TUI/CLI process for
//! interactive sessions). This module makes that process the rendezvous
//! point: alongside the PTY bridge it serves the live PTY over a per-container
//! unix domain socket, and `attach` on the Apple backend connects to it.
//!
//! Parity properties match `docker attach`:
//! - attach reaches the *actual agent TTY* (same PTY, same bytes) — never a
//!   sibling shell;
//! - multiple concurrent clients are allowed (output broadcast, stdin merged);
//! - ending a client session never stops the target container;
//! - a client resize resizes the real PTY, so the agent repaints (the same
//!   redraw trigger the docker attach path relies on).
//!
//! The one non-parity caveat is inherent to the runtime: on Apple the PTY
//! dies with the launching process, so attach requires that process to still
//! be alive. That is not a new fragility — the PTY (and with it the agent's
//! stdio) already had that lifetime before this module existed.
//!
//! Security: sockets live in a `0700` directory under the user's home
//! (`~/.awman/attach/`), each socket is `0600`, and nothing is ever exposed
//! beyond the local user — consistent with the local-only daemon surface in
//! `aspec/architecture/security.md`.
//!
//! ## Wire protocol
//!
//! Symmetric length-prefixed frames: `tag: u8, len: u32 LE, payload: [u8; len]`.
//!
//! - server → client: [`OUTPUT_FRAME`] — raw PTY output bytes.
//! - client → server: [`STDIN_FRAME`] — raw stdin bytes for the agent;
//!   [`RESIZE_FRAME`] — 4-byte payload `cols: u16 LE, rows: u16 LE`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::data::session::AgentHandle;
use crate::engine::error::EngineError;

/// Server → client: raw PTY output bytes.
pub(crate) const OUTPUT_FRAME: u8 = 0;
/// Client → server: raw stdin bytes.
pub(crate) const STDIN_FRAME: u8 = 1;
/// Client → server: `cols: u16 LE, rows: u16 LE`.
pub(crate) const RESIZE_FRAME: u8 = 2;
/// Upper bound on a single frame's payload; anything larger is a protocol
/// violation and closes the connection.
const MAX_FRAME_LEN: u32 = 1 << 20;

/// Encode one frame.
pub(crate) fn encode_frame(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(tag);
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Encode a resize frame.
pub(crate) fn encode_resize(cols: u16, rows: u16) -> Vec<u8> {
    let mut payload = [0u8; 4];
    payload[..2].copy_from_slice(&cols.to_le_bytes());
    payload[2..].copy_from_slice(&rows.to_le_bytes());
    encode_frame(RESIZE_FRAME, &payload)
}

/// The directory attach sockets live in: `$AWMAN_ATTACH_DIR` when set (used
/// by tests and relocated setups), else `~/.awman/attach/`. Both the serving
/// process and the attaching process derive it the same way, which is what
/// makes the socket discoverable across processes of the same user.
pub(crate) fn default_attach_socket_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("AWMAN_ATTACH_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::home_dir().map(|home| home.join(".awman").join("attach"))
}

/// The socket path for one container. The filename is a hash of the container
/// name rather than the name itself: unix socket paths have a hard length
/// limit (104 bytes on macOS), which a home prefix plus a full squad container
/// name can exceed.
pub(crate) fn attach_socket_path(container_name: &str) -> Option<PathBuf> {
    let digest = crate::data::fs::workflow_state::sha256_hex(container_name);
    default_attach_socket_dir().map(|dir| dir.join(format!("{}.sock", &digest[..16])))
}

/// What the socket server needs from the owning PTY bridge.
pub(crate) struct AttachHooks {
    /// Live PTY output. Each client subscribes; a lagging client drops chunks
    /// rather than backpressuring the bridge.
    pub output: Arc<tokio::sync::broadcast::Sender<Vec<u8>>>,
    /// The bridge's stdin injector — client keystrokes merge with whatever
    /// the launching frontend itself sends, exactly as concurrent
    /// `docker attach` clients merge stdin.
    pub stdin: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Resize the real PTY. The agent receives SIGWINCH and repaints, which
    /// is what gives a freshly-connected client a full screen.
    pub resize: Arc<dyn Fn(u16, u16) + Send + Sync>,
}

/// Owns the listening socket. Dropping it (when the container's execution
/// backend is dropped after `wait()`) aborts the accept loop, every per-client
/// task with it, and removes the socket file.
pub(crate) struct AttachSocketGuard {
    path: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for AttachSocketGuard {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A configured-but-not-connected attach client for a container whose PTY is
/// served over an attach socket. Produced by backends without a native attach
/// (Apple); the Docker backend keeps `docker attach`.
pub(crate) struct SocketAttachInstance {
    pub handle: AgentHandle,
    pub path: PathBuf,
}

// ─── Unix implementation ─────────────────────────────────────────────────────

#[cfg(unix)]
mod unix_impl {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::engine::agent_runtime::execution::{
        AgentExecution, AgentExitInfo, AgentHandlePreview, AgentInstance, CancelHandle,
        ExecutionBackend,
    };
    use crate::engine::agent_runtime::frontend::AgentFrontend;
    use crate::engine::agent_runtime::output_tail::OutputTail;

    /// Bind the socket and start serving. Never fails the container run —
    /// callers log and continue without attach support on error.
    pub(crate) fn spawn_attach_socket_server(
        path: &Path,
        hooks: AttachHooks,
    ) -> Result<AttachSocketGuard, EngineError> {
        use std::os::unix::fs::PermissionsExt;

        if let Some(parent) = path.parent() {
            // Created with 0700 directly — never a wider-mode window — and
            // re-tightened in case a predecessor left it looser.
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder
                .create(parent)
                .map_err(|e| EngineError::io(parent, e))?;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
        // A stale socket from a crashed predecessor is unlinked, never reused.
        let _ = std::fs::remove_file(path);
        let listener =
            std::os::unix::net::UnixListener::bind(path).map_err(|e| EngineError::io(path, e))?;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        listener
            .set_nonblocking(true)
            .map_err(|e| EngineError::io(path, e))?;
        let listener =
            tokio::net::UnixListener::from_std(listener).map_err(|e| EngineError::io(path, e))?;

        let task = tokio::spawn(accept_loop(listener, hooks));
        Ok(AttachSocketGuard {
            path: path.to_path_buf(),
            task,
        })
    }

    /// Accept clients until aborted. Per-client tasks live in a `JoinSet`
    /// owned by this task, so aborting the guard tears every session down —
    /// the guard's abort is the *only* way this loop ends, so a transient
    /// accept failure (`ECONNABORTED`, fd exhaustion, …) never permanently
    /// disables attach for a still-running container.
    async fn accept_loop(listener: tokio::net::UnixListener, hooks: AttachHooks) {
        let mut clients = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        let output_rx = hooks.output.subscribe();
                        let stdin = hooks.stdin.clone();
                        let resize = Arc::clone(&hooks.resize);
                        clients.spawn(serve_client(stream, output_rx, stdin, resize));
                    }
                    Err(error) => {
                        tracing::warn!(%error, "attach socket accept failed; still serving");
                        // Pace retries so a persistent error can't spin hot.
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                },
                // Reap finished sessions so the set doesn't grow unbounded.
                Some(_) = clients.join_next(), if !clients.is_empty() => {}
            }
        }
    }

    /// One attach session: forward broadcast output out, route stdin/resize
    /// frames in. Both directions run inside this one task (a `select!`, not
    /// a second spawn) so that when the accept loop's `JoinSet` aborts the
    /// task — container teardown — both socket halves drop, the fd closes,
    /// and the client sees EOF. Ends when either direction fails.
    async fn serve_client(
        stream: tokio::net::UnixStream,
        output_rx: tokio::sync::broadcast::Receiver<Vec<u8>>,
        stdin: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
        resize: Arc<dyn Fn(u16, u16) + Send + Sync>,
    ) {
        let (read_half, write_half) = stream.into_split();
        tokio::select! {
            _ = forward_output(output_rx, write_half) => {}
            _ = route_client_frames(read_half, stdin, resize) => {}
        }
    }

    /// Broadcast PTY output → client, framed.
    async fn forward_output(
        mut output_rx: tokio::sync::broadcast::Receiver<Vec<u8>>,
        mut write_half: tokio::net::unix::OwnedWriteHalf,
    ) {
        loop {
            match output_rx.recv().await {
                Ok(bytes) => {
                    if write_half
                        .write_all(&encode_frame(OUTPUT_FRAME, &bytes))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
        let _ = write_half.shutdown().await;
    }

    /// Client frames → the agent's stdin / the real PTY size.
    async fn route_client_frames(
        mut read_half: tokio::net::unix::OwnedReadHalf,
        stdin: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
        resize: Arc<dyn Fn(u16, u16) + Send + Sync>,
    ) {
        loop {
            let mut header = [0u8; 5];
            if read_half.read_exact(&mut header).await.is_err() {
                break;
            }
            let tag = header[0];
            let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]);
            if len > MAX_FRAME_LEN {
                break;
            }
            let mut payload = vec![0u8; len as usize];
            if read_half.read_exact(&mut payload).await.is_err() {
                break;
            }
            match tag {
                STDIN_FRAME => {
                    if stdin.send(payload).is_err() {
                        break;
                    }
                }
                RESIZE_FRAME if len == 4 => {
                    let cols = u16::from_le_bytes([payload[0], payload[1]]);
                    let rows = u16::from_le_bytes([payload[2], payload[3]]);
                    (resize)(cols, rows);
                }
                _ => break,
            }
        }
    }

    impl AgentInstance for SocketAttachInstance {
        fn handle_preview(&self) -> AgentHandlePreview {
            AgentHandlePreview {
                id: self.handle.id.clone(),
                name: self.handle.name.clone(),
                image: self.handle.image_tag.clone(),
            }
        }

        fn run_with_frontend(
            self: Box<Self>,
            mut frontend: Box<dyn AgentFrontend>,
        ) -> Result<AgentExecution, EngineError> {
            let started_at = chrono::Utc::now();
            let handle = self.handle.clone();

            frontend.report_status(
                crate::engine::agent_runtime::frontend::AgentStatus::Running {
                    container_name: handle.name.clone(),
                },
            );
            let grace_timeout = frontend.grace_timeout();
            let stuck_timeout = frontend.stuck_timeout();
            let io = frontend.take_io();

            let stream = std::os::unix::net::UnixStream::connect(&self.path).map_err(|e| {
                EngineError::Container(format!(
                    "cannot attach to {}: no live attach endpoint ({e}). On this runtime \
                     the awman process that launched the agent serves its terminal; that \
                     process is no longer running, or the agent predates attach support.",
                    handle.name
                ))
            })?;
            // Extra handles for cancel/cancel_handle: shutting one down closes
            // the shared underlying socket, so the reader sees EOF and wait()
            // resolves — killing only this local session, never the container.
            let shutdown = stream
                .try_clone()
                .map_err(|e| EngineError::Container(format!("clone attach socket: {e}")))?;
            let shutdown_for_handle = stream
                .try_clone()
                .map_err(|e| EngineError::Container(format!("clone attach socket: {e}")))?;
            stream
                .set_nonblocking(true)
                .map_err(|e| EngineError::Container(format!("attach socket nonblocking: {e}")))?;
            let stream = tokio::net::UnixStream::from_std(stream)
                .map_err(|e| EngineError::Container(format!("attach socket register: {e}")))?;
            let (mut read_half, mut write_half) = stream.into_split();

            // Writer: an initial resize first (the repaint trigger), then
            // frontend stdin/resize as they arrive.
            let initial_size = io.initial_size;
            let mut stdin_rx = io.stdin_rx;
            let mut resize_rx = io.resize;
            let stdin_injector = io.stdin_tx.clone();
            tokio::spawn(async move {
                if let Some((cols, rows)) = initial_size {
                    if write_half
                        .write_all(&encode_resize(cols, rows))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                loop {
                    tokio::select! {
                        bytes = stdin_rx.recv() => {
                            let Some(bytes) = bytes else { break };
                            if write_half
                                .write_all(&encode_frame(STDIN_FRAME, &bytes))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        size = recv_resize(&mut resize_rx) => {
                            let Some((cols, rows)) = size else { break };
                            if write_half.write_all(&encode_resize(cols, rows)).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });

            // Reader: framed output → frontend stdout, with the same activity
            // tracking the PTY bridge feeds its stuck detector from.
            let output_tail = Arc::new(OutputTail::with_default_capacity());
            let activity: crate::engine::container::io_bridge::SharedActivity =
                Arc::new(Mutex::new(None));
            let first_byte = Arc::new(AtomicBool::new(false));
            let (exit_tx, exit_rx) = std::sync::mpsc::channel::<()>();
            let stdout_tx = io.stdout;
            let tail = Arc::clone(&output_tail);
            let act = Arc::clone(&activity);
            let fb = Arc::clone(&first_byte);
            tokio::spawn(async move {
                loop {
                    let mut header = [0u8; 5];
                    if read_half.read_exact(&mut header).await.is_err() {
                        break;
                    }
                    let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]);
                    if header[0] != OUTPUT_FRAME || len > MAX_FRAME_LEN {
                        break;
                    }
                    let mut payload = vec![0u8; len as usize];
                    if read_half.read_exact(&mut payload).await.is_err() {
                        break;
                    }
                    if let Ok(mut guard) = act.lock() {
                        *guard = Some(std::time::Instant::now());
                    }
                    fb.store(true, std::sync::atomic::Ordering::Release);
                    tail.push_bytes(&payload);
                    let _ = stdout_tx.send(payload);
                }
                let _ = exit_tx.send(());
            });

            let stuck_tx = crate::engine::container::io_bridge::spawn_stuck_detector(
                activity,
                first_byte,
                grace_timeout,
                stuck_timeout,
                std::time::Duration::ZERO,
                None,
            );

            let backend = SocketAttachExecution {
                exit_rx,
                shutdown,
                shutdown_for_handle,
                stdin_injector,
                started_at,
            };
            Ok(AgentExecution::new(
                handle,
                Box::new(backend),
                stuck_tx,
                Some(output_tail),
            ))
        }
    }

    /// Receive from an optional resize channel, pending forever when absent.
    async fn recv_resize(
        rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<(u16, u16)>>,
    ) -> Option<(u16, u16)> {
        match rx {
            Some(rx) => rx.recv().await,
            None => std::future::pending().await,
        }
    }

    /// Execution backend for a socket attach session. Cancellation shuts the
    /// local socket down — never the target container, which belongs to the
    /// serving process.
    struct SocketAttachExecution {
        exit_rx: std::sync::mpsc::Receiver<()>,
        shutdown: std::os::unix::net::UnixStream,
        shutdown_for_handle: std::os::unix::net::UnixStream,
        stdin_injector: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
        started_at: chrono::DateTime<chrono::Utc>,
    }

    impl ExecutionBackend for SocketAttachExecution {
        fn wait_blocking(self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
            // Resolves when the reader task ends: server gone (container exit,
            // launcher teardown) or this session's own shutdown.
            let _ = self.exit_rx.recv();
            Ok(AgentExitInfo {
                exit_code: 0,
                signal: None,
                started_at: self.started_at,
                ended_at: chrono::Utc::now(),
            })
        }

        fn try_inject_stdin(&self, bytes: &[u8]) -> Result<bool, EngineError> {
            self.stdin_injector
                .send(bytes.to_vec())
                .map_err(|e| EngineError::Container(format!("inject stdin: {e}")))?;
            Ok(true)
        }

        fn cancel(&self) -> Result<(), EngineError> {
            let _ = self.shutdown.shutdown(std::net::Shutdown::Both);
            Ok(())
        }

        fn cancel_handle(&self) -> Option<CancelHandle> {
            let stream = match self.shutdown_for_handle.try_clone() {
                Ok(stream) => stream,
                Err(_) => return None,
            };
            Some(CancelHandle::new(move || {
                let _ = stream.shutdown(std::net::Shutdown::Both);
                Ok(())
            }))
        }
    }
}

#[cfg(unix)]
pub(crate) use unix_impl::spawn_attach_socket_server;

// ─── Non-unix stubs ──────────────────────────────────────────────────────────
//
// Unix sockets don't exist elsewhere; the Apple runtime is macOS-only anyway,
// so these stubs exist purely to keep cross-platform builds compiling.

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    use crate::data::message::{UserMessage, UserMessageSink};
    use crate::engine::agent_runtime::execution::AgentInstance;
    use crate::engine::agent_runtime::frontend::{
        AgentFrontend, AgentIo, AgentProgress, AgentStatus,
    };

    #[test]
    fn frames_are_length_prefixed_and_resize_carries_cols_then_rows() {
        assert_eq!(
            encode_frame(STDIN_FRAME, b"hi"),
            vec![STDIN_FRAME, 2, 0, 0, 0, b'h', b'i']
        );
        assert_eq!(
            encode_resize(0x0102, 0x0304),
            vec![RESIZE_FRAME, 4, 0, 0, 0, 0x02, 0x01, 0x04, 0x03]
        );
    }

    #[test]
    fn socket_paths_are_deterministic_short_and_name_scoped() {
        std::env::set_var("AWMAN_ATTACH_DIR", "/tmp/awman-attach-test");
        let a = attach_socket_path("awman-squad-some-quite-long-task-name-0123abcd").unwrap();
        let b = attach_socket_path("awman-squad-some-quite-long-task-name-0123abcd").unwrap();
        let c = attach_socket_path("awman-squad-other-89abcdef").unwrap();
        std::env::remove_var("AWMAN_ATTACH_DIR");
        assert_eq!(a, b, "the path must be derivable in any process");
        assert_ne!(a, c);
        let file = a.file_name().unwrap().to_str().unwrap();
        // Hashed filename: bounded length regardless of container-name length,
        // because unix socket paths have a hard OS limit (104 bytes on macOS).
        assert_eq!(file.len(), "0123456789abcdef.sock".len(), "{file}");
    }

    /// A test-side handle to a sender the frontend creates lazily.
    type SharedSender<T> = Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<T>>>>;

    /// Frontend fixture mirroring the TUI attach frontend's `AgentIo` shape.
    #[derive(Clone, Default)]
    struct Captured {
        stdout: Arc<Mutex<Vec<u8>>>,
        stdin_tx: SharedSender<Vec<u8>>,
        resize_tx: SharedSender<(u16, u16)>,
    }

    struct TestFrontend {
        captured: Captured,
    }

    impl UserMessageSink for TestFrontend {
        fn write_message(&mut self, _msg: UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    #[async_trait::async_trait]
    impl AgentFrontend for TestFrontend {
        fn report_status(&mut self, _status: AgentStatus) {}
        fn report_progress(&mut self, _progress: AgentProgress) {}
        fn take_io(&mut self) -> AgentIo {
            let (stdout_tx, mut stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
            let (stderr_tx, mut stderr_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
            let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
            let (resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();
            *self.captured.stdin_tx.lock().unwrap() = Some(stdin_tx.clone());
            *self.captured.resize_tx.lock().unwrap() = Some(resize_tx);
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
                resize: Some(resize_rx),
                initial_size: Some((80, 24)),
            }
        }
    }

    struct ServerFixture {
        output: Arc<tokio::sync::broadcast::Sender<Vec<u8>>>,
        stdin_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
        resizes: Arc<Mutex<Vec<(u16, u16)>>>,
        guard: AttachSocketGuard,
        path: PathBuf,
        _dir: tempfile::TempDir,
    }

    fn start_server() -> ServerFixture {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sock");
        let (output_tx, _) = tokio::sync::broadcast::channel::<Vec<u8>>(64);
        let output = Arc::new(output_tx);
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let resizes: Arc<Mutex<Vec<(u16, u16)>>> = Arc::new(Mutex::new(Vec::new()));
        let resizes_hook = Arc::clone(&resizes);
        let guard = spawn_attach_socket_server(
            &path,
            AttachHooks {
                output: Arc::clone(&output),
                stdin: stdin_tx,
                resize: Arc::new(move |cols, rows| {
                    resizes_hook.lock().unwrap().push((cols, rows));
                }),
            },
        )
        .expect("server must bind");
        ServerFixture {
            output,
            stdin_rx,
            resizes,
            guard,
            path,
            _dir: dir,
        }
    }

    fn instance(path: &Path) -> Box<SocketAttachInstance> {
        Box::new(SocketAttachInstance {
            handle: AgentHandle {
                id: "awman-squad-t-00000001".into(),
                name: "awman-squad-t-00000001".into(),
                image_tag: "img".into(),
                started_at: chrono::Utc::now(),
            },
            path: path.to_path_buf(),
        })
    }

    async fn wait_until(mut check: impl FnMut() -> bool, what: &str) {
        for _ in 0..200 {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// The full attach parity contract, end to end over a real unix socket:
    /// PTY output reaches the client, client keystrokes reach the agent's
    /// stdin, resizes (the initial one included) reach the real PTY, ending
    /// one session leaves the server (and thus the container) untouched, and
    /// tearing the server down ends every client's `wait()`.
    #[tokio::test]
    async fn socket_attach_round_trips_output_stdin_and_resize() {
        let mut server = start_server();

        let captured = Captured::default();
        let mut execution = instance(&server.path)
            .run_with_frontend(Box::new(TestFrontend {
                captured: captured.clone(),
            }))
            .expect("attach must connect");

        // The client's very first act is the initial resize — the repaint
        // trigger that fills a fresh attach screen.
        wait_until(
            || server.resizes.lock().unwrap().first() == Some(&(80, 24)),
            "the initial resize",
        )
        .await;

        // Output: broadcast (what the PTY reader taps) → client stdout.
        server.output.send(b"agent screen bytes".to_vec()).unwrap();
        wait_until(
            || captured.stdout.lock().unwrap().as_slice() == b"agent screen bytes",
            "output to reach the attach client",
        )
        .await;

        // Stdin: client keystrokes → the bridge's stdin injector.
        let stdin_tx = captured.stdin_tx.lock().unwrap().clone().unwrap();
        stdin_tx.send(b"ls\r".to_vec()).unwrap();
        let received = tokio::time::timeout(Duration::from_secs(2), server.stdin_rx.recv())
            .await
            .expect("stdin must arrive")
            .expect("stdin channel open");
        assert_eq!(received, b"ls\r");

        // Resize: client → real PTY.
        let resize_tx = captured.resize_tx.lock().unwrap().clone().unwrap();
        resize_tx.send((132, 50)).unwrap();
        wait_until(
            || server.resizes.lock().unwrap().contains(&(132, 50)),
            "the client resize",
        )
        .await;

        // Ending this session must not tear the server down: a second client
        // can still attach (docker-attach parity — multiple clients, and a
        // detach never stops the target).
        execution.cancel().expect("cancel");
        tokio::time::timeout(Duration::from_secs(5), execution.wait())
            .await
            .expect("wait must resolve after cancel")
            .expect("wait result");

        let captured2 = Captured::default();
        let mut execution2 = instance(&server.path)
            .run_with_frontend(Box::new(TestFrontend {
                captured: captured2.clone(),
            }))
            .expect("a second client must connect after the first detached");
        // The server subscribes on accept; wait for the subscription before
        // sending, or the broadcast has zero receivers and drops the send.
        let output = Arc::clone(&server.output);
        wait_until(
            || output.receiver_count() > 0,
            "the second client's server-side subscription",
        )
        .await;
        server.output.send(b"still live".to_vec()).unwrap();
        wait_until(
            || {
                captured2
                    .stdout
                    .lock()
                    .unwrap()
                    .windows(b"still live".len())
                    .any(|w| w == b"still live")
            },
            "output to the second client",
        )
        .await;

        // Dropping the guard (container teardown) ends the remaining session
        // and removes the socket file.
        let path = server.path.clone();
        drop(server.guard);
        tokio::time::timeout(Duration::from_secs(5), execution2.wait())
            .await
            .expect("wait must resolve when the server goes away")
            .expect("wait result");
        wait_until(|| !path.exists(), "socket file removal").await;
    }

    /// No serving process → a clear, attributable error instead of a doomed
    /// spawn (the pre-parity behaviour was an instantly-dying client).
    #[tokio::test]
    async fn attach_without_a_live_endpoint_fails_with_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gone.sock");
        let error = instance(&path)
            .run_with_frontend(Box::new(TestFrontend {
                captured: Captured::default(),
            }))
            .err()
            .expect("connecting to a missing socket must fail");
        let text = error.to_string();
        assert!(text.contains("no live attach endpoint"), "{text}");
        assert!(text.contains("awman-squad-t-00000001"), "{text}");
    }

    /// A stale socket file from a crashed predecessor must not block a new
    /// server for the same container name.
    #[tokio::test]
    async fn rebinding_over_a_stale_socket_file_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stale.sock");
        // A dead socket file: bind and immediately drop the listener.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let (output_tx, _) = tokio::sync::broadcast::channel::<Vec<u8>>(4);
        let (stdin_tx, _stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        let guard = spawn_attach_socket_server(
            &path,
            AttachHooks {
                output: Arc::new(output_tx),
                stdin: stdin_tx,
                resize: Arc::new(|_, _| {}),
            },
        )
        .expect("rebinding over a stale socket must succeed");
        drop(guard);
    }
}

#[cfg(not(unix))]
pub(crate) fn spawn_attach_socket_server(
    _path: &Path,
    _hooks: AttachHooks,
) -> Result<AttachSocketGuard, EngineError> {
    Err(EngineError::Container(
        "attach sockets require a unix host".into(),
    ))
}

#[cfg(not(unix))]
impl crate::engine::agent_runtime::execution::AgentInstance for SocketAttachInstance {
    fn handle_preview(&self) -> crate::engine::agent_runtime::execution::AgentHandlePreview {
        crate::engine::agent_runtime::execution::AgentHandlePreview {
            id: self.handle.id.clone(),
            name: self.handle.name.clone(),
            image: self.handle.image_tag.clone(),
        }
    }

    fn run_with_frontend(
        self: Box<Self>,
        _frontend: Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend>,
    ) -> Result<crate::engine::agent_runtime::execution::AgentExecution, EngineError> {
        Err(EngineError::Container(
            "attach sockets require a unix host".into(),
        ))
    }
}
