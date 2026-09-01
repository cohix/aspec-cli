# Work Item: Enhancement

Title: Yolo Enhancements
Issue: issuelink

## Summary

Improve yolo-mode workflows across CLI and API frontends by:

1. Replacing the optional `take_container_io()` with a mandatory, unified `ContainerIo` that every frontend provides — covering stdout, stderr, and stdin channels plus optional PTY fields — so the container engine exclusively mediates all container I/O on all three frontends.
2. Moving stuck detection entirely into the container engine: the engine tracks output activity internally and publishes `StuckEvent::Stuck`/`Unstuck` on a broadcast channel that the workflow engine and TUI frontend subscribe to.
3. Supporting full-screen interactive agents in CLI (when `--non-interactive` is not passed and a TTY is present) while still running stuck detection concurrently.
4. Auto-detecting the absence of a TTY/PTY and enforcing `--non-interactive` so headless environments (CI/CD pipelines) behave correctly without a flag.
5. Throttling yolo countdown messages sent to the message sink (one per 10 s for CLI/API) while retaining per-tick resolution in the TUI dialog.

## User Stories

### User Story 1
As a: user

I want to: run a yolo workflow in CLI mode and have it automatically detect when the agent is stuck and count down to auto-advance — even when the agent is running in full-screen interactive mode

So I can: walk away from a terminal session knowing the workflow will not stall indefinitely if the agent stops producing output

### User Story 2
As a: user

I want to: run `awman exec workflow --yolo` inside a CI/CD pipeline (no TTY) without explicitly passing `--non-interactive`

So I can: integrate awman into automated pipelines without special-casing the flag and have the agent behave headlessly by default

### User Story 3
As a: developer using the API

I want to: receive a structured status message every 10 seconds when a yolo countdown is in progress, rather than one message per tick (every ~100 ms)

So I can: surface meaningful countdown progress to API clients without being overwhelmed by noise

## Implementation Details

### 1. Unified I/O Channel Interface in `ContainerFrontend`

**Problem**: The existing `take_container_io() -> Option<ContainerIo>` is optional and only the TUI provides it. CLI and API fall back to a mix of `Stdio::inherit()`, `/dev/tty`, `Stdio::null()`, and one-shot piped writes. The container engine has no visibility into any byte flowing to or from the container on those paths: stdout/stderr pass through without interception, and stdin is wired directly to the OS-level file descriptor, completely bypassing the engine.

**Solution**: Replace the optional `take_container_io()` with a mandatory version that every frontend must implement. Redesign `ContainerIo` to be a complete, self-contained I/O bundle covering stdout, stderr, and stdin uniformly:

```rust
pub struct ContainerIo {
    // Output: engine writes container stdout/stderr bytes here.
    pub stdout: UnboundedSender<Vec<u8>>,
    pub stderr: UnboundedSender<Vec<u8>>,
    // Input: engine reads from stdin_rx and writes to the container's stdin.
    // The engine also retains a clone of stdin_tx for try_inject_stdin
    // (ContinueInCurrentContainer advances).
    pub stdin_tx: UnboundedSender<Vec<u8>>,
    pub stdin_rx: UnboundedReceiver<Vec<u8>>,
    // PTY-specific: Some for interactive frontends (TUI, CLI with TTY),
    // None for non-interactive frontends (CLI --non-interactive, API).
    // When None, the engine uses Stdio::piped() for the container process.
    // When Some, the engine opens a PTY and bridges it.
    pub resize: Option<UnboundedReceiver<(u16, u16)>>,
    pub initial_size: Option<(u16, u16)>,
}
```

The trait method signature changes from `fn take_container_io(&mut self) -> Option<ContainerIo>` to `fn take_container_io(&mut self) -> ContainerIo` (required, no longer has a default no-op implementation). The `write_stdout`, `write_stderr`, `read_stdin`, and `resize_pty` methods on `ContainerFrontend` become dead code — the engine exclusively uses the channels from `ContainerIo` — and should be removed from the trait and all implementations.

**Frontend implementations of `take_container_io()`**:

- **TUI** (`src/frontend/tui/per_command/container_frontend.rs`): constructs `ContainerIo` with the existing `stdout_tx` (vt100 parser channel), a new `stderr_tx` routed to the same parser, the existing `stdin_tx`/`stdin_rx` pair (driven by keypresses), the existing `resize` receiver, and the initial PTY size. No behavioral change for the happy path.
- **CLI interactive** (TTY present, `--non-interactive` false): constructs `ContainerIo` with stdout/stderr senders wired to host terminal writer tasks, spawns a raw-mode reader thread on `/dev/tty` that sends bytes into `stdin_tx`, wires `resize` to a `SIGWINCH` listener, and reads initial terminal size via `terminal_size` or similar. The engine opens a PTY bridge (`initial_size` is `Some`).
- **CLI non-interactive** (TTY absent or `--non-interactive`): constructs `ContainerIo` with stdout/stderr senders wired to host terminal writer tasks, drops `stdin_tx` immediately after constructing the pair (so `stdin_rx` receives EOF), and sets `resize = None` / `initial_size = None`. The engine uses `Stdio::piped()` and the writer task terminates immediately on EOF.
- **API** (`src/frontend/api/command_frontend.rs`): constructs `ContainerIo` with stdout/stderr senders wired to tasks that emit `StdoutLine`/`StderrLine` events on the event bus, drops `stdin_tx` immediately, and sets `resize = None` / `initial_size = None`.
- **`TuiContainerProxy`** and **`CliContainerProxy`** (standalone proxies for Init/Ready phases): same pattern as their owning frontend's non-interactive path.

**Seeded prompt injection**: The current `/dev/tty`+`Stdio::piped()` seeded-write code in both backends is replaced. The engine writes the seeded prompt bytes into `stdin_tx` (which it retains a clone of) before the writer task begins draining. The writer task naturally delivers the prompt bytes to the container's piped stdin then keeps the channel open for `try_inject_stdin` continuations.

**Backend changes** — apply identically to both `docker.rs` and `apple.rs`:

- Call `frontend.take_container_io()` unconditionally (no longer conditional on `interactive`).
- When `io.resize` / `io.initial_size` are `Some`: open a PTY bridge via `portable-pty` (existing `spawn_pty_bridged_*` path). The PTY reader thread forwards bytes to `io.stdout` and `io.stderr`. The writer task drains `io.stdin_rx` and writes to the PTY master.
- When `io.resize` / `io.initial_size` are `None`: spawn the container with `Stdio::piped()` for stdout, stderr, and stdin. Spawn reader threads for stdout and stderr that forward to `io.stdout` and `io.stderr`. Spawn a writer task that drains `io.stdin_rx` and writes to the child's stdin pipe.
- Neither backend uses `Stdio::inherit()`, `Stdio::null()`, or opens `/dev/tty` after this change.
- Extract the shared reader-thread + writer-task + stuck-detector setup (covering both the PTY and piped paths) into a helper function in `src/engine/container/` called by both backends, as noted in Section 2.

### 2. Stuck Detection Inside the Container Engine

**Problem**: Stuck detection currently lives entirely in `src/frontend/tui/tabs.rs` (`recompute_stuck()`, `is_stuck()`, the `STUCK_TIMEOUT` constant, and the transition events dispatched in `src/frontend/tui/app.rs`). CLI and API have no stuck detection at all. The previous plan proposed moving it into the workflow engine's polling loop, but that is still one layer too high — the workflow engine should not be polling output timestamps.

**Solution**: Stuck detection runs entirely inside the container engine, started automatically during `run_with_frontend`. The engine proactively publishes `Stuck`/`Unstuck` events via a tokio broadcast channel that any external party can subscribe to.

**Activity tracking**: The engine's stdout reader thread (which pumps bytes from the container's PTY or pipe into `io.stdout` from step 1) also updates a shared `Arc<Mutex<Option<Instant>>>` timestamp on every byte chunk. This is the sole source of truth for activity; no frontend touches it. This reader thread exists in both `spawn_pty_bridged_docker` / `spawn_pty_bridged_apple` (PTY path) and in the piped path for both backends — the activity update must be present in all four locations. Stderr bytes via `io.stderr` also count as activity and must update the same timestamp.

**Stuck detector task**: Alongside the reader thread, the engine spawns a tokio task:

```rust
tokio::spawn(async move {
    let mut is_stuck = false;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let elapsed = /* read last_activity, fall back to container start time if None */;
        let now_stuck = elapsed >= STUCK_TIMEOUT;
        if now_stuck && !is_stuck {
            is_stuck = true;
            let _ = stuck_tx.send(StuckEvent::Stuck);
        } else if !now_stuck && is_stuck {
            is_stuck = false;
            let _ = stuck_tx.send(StuckEvent::Unstuck);
        }
    }
});
```

**Subscription API**: `ContainerExecution` (in `src/engine/container/instance.rs`) exposes:

```rust
#[derive(Clone, Debug)]
pub enum StuckEvent { Stuck, Unstuck }

impl ContainerExecution {
    /// Subscribe to stuck/unstuck transitions for this container's output.
    /// Multiple subscribers are supported (broadcast semantics).
    pub fn subscribe_stuck(&self) -> tokio::sync::broadcast::Receiver<StuckEvent> { ... }
}
```

The `broadcast::Sender<StuckEvent>` is stored inside `ContainerExecution` behind an `Arc` so it survives `run_with_frontend`. Both `DockerExecution` and `AppleExecution` construct this sender and hand it to `ContainerExecution::new` via a new constructor argument (or by storing it in the shared `ContainerExecution` fields). The stuck detector task is spawned by a shared helper function called by both backends — not duplicated in each backend file.

**Workflow engine integration** (`src/engine/workflow/mod.rs`, `step_once_interruptible()`): After calling `run_with_frontend`, the engine calls `execution.subscribe_stuck()` and adds the receiver to the existing `tokio::select!` loop:

```rust
tokio::select! {
    event = stuck_rx.recv() => match event {
        Ok(StuckEvent::Stuck)   => { /* trigger yolo countdown */ }
        Ok(StuckEvent::Unstuck) => { /* cancel countdown if running */ }
        _ => {}
    },
    // ... existing arms (engine_rx, step completion, etc.)
}
```

The `EngineRequest::StepStuck` / `StepUnstuck` messages that the TUI previously sent via `engine_tx_shared` are no longer needed for stuck detection — the engine receives those events directly from the container. The `engine_tx_shared` channel is retained only for user-driven actions (Ctrl-W, AdvanceNow, etc.).

**TUI integration** (`src/frontend/tui/tabs.rs`, `src/frontend/tui/app.rs`): After `run_with_frontend` returns, the TUI stores the `broadcast::Receiver<StuckEvent>` on the `Tab`. The `tick_all_tabs()` loop drains this receiver non-blockingly to update tab coloring. The `last_output_time`, `STUCK_TIMEOUT` constant, `recompute_stuck()`, and `is_stuck()` are removed from `tabs.rs`; the TUI no longer needs its own stuck-detection logic.

Define `STUCK_TIMEOUT` in `src/engine/container/` (e.g., a new `src/engine/container/timing.rs`) — it governs container-layer behaviour and has no dependency on the workflow engine. `YOLO_COUNTDOWN_DURATION` and `YOLO_SINK_THROTTLE_INTERVAL` remain in `src/engine/workflow/timing.rs` as they govern workflow-layer behaviour.

### 3. CLI Full-Screen Interactive Agent: Terminal Binding and Unbinding

**Problem**: In CLI mode, when `--non-interactive` is false and a TTY is present, the container agent runs full-screen and the terminal must be placed into raw mode for the duration of the container's execution. When a yolo countdown auto-advances the workflow to the next step, the terminal must be cleanly restored to cooked mode before any workflow status output is printed and before the next step's container can take over the terminal. There is currently no mechanism for this.

**Solution**: With step 1 in place (all frontends provide `ContainerIo`, engine always mediates I/O), the CLI gains full control of the terminal binding/unbinding lifecycle.

**Binding**: When `CliFrontend::take_container_io()` is called for an interactive run, it:
1. Enables raw mode on the controlling terminal (e.g., via `crossterm::terminal::enable_raw_mode()`).
2. Stores a RAII `RawModeGuard` on `CliFrontend` that disables raw mode on drop.
3. Returns a `ContainerIo` with `resize` and `initial_size` set to `Some`, the PTY reader thread forwarding stdout to a host-terminal writer task, and a raw-mode reader thread on `/dev/tty` feeding bytes into `stdin_tx`.

**Unbinding**: The `RawModeGuard` is dropped — restoring cooked mode — when `CliFrontend::report_step_status()` receives a terminal status (`Succeeded`, `Failed`, `Cancelled`). This is the same call site used for both normal completion and yolo auto-advance, guaranteeing the terminal is always in a known clean state before the workflow engine prints status messages or starts the next step.

**Yolo countdown overlay**: While the countdown is running and the terminal is in raw mode, `yolo_countdown_tick()` uses ANSI escape sequences (save cursor, move to last terminal line, print countdown, restore cursor) so the agent's viewport is not disrupted. After the countdown expires the workflow engine cancels the container, which causes the PTY reader thread to hit EOF and exit. The engine then calls `report_step_status(step, Cancelled)`, the `RawModeGuard` drops, and the terminal returns to cooked mode in time for the next step's transition output.

The stuck detector task fires `StuckEvent::Stuck` regardless of whether the frontend is in full-screen interactive mode — activity tracking is in the engine, not the frontend.

### 4. Unified Non-Interactive Enforcement (CLI + API)

**Problem**: Non-interactive enforcement is currently duplicated and inconsistent: the CLI checks `stdin_is_tty()` per-method in `workflow_frontend_marker.rs`, while the API hard-codes `non_interactive=true` in its `ParsedArgs` construction. These two paths encode the same underlying fact — "there is no TTY available" — via different mechanisms in different places.

**Solution**: Consolidate enforcement into a single shared function in `src/command/dispatch/` (or `src/frontend/`) that both the CLI and API call:

```rust
/// Resolve the effective non-interactive flag.
/// Returns true when the caller explicitly requested it OR when stdin
/// is not a TTY (headless environment, HTTP server, CI/CD pipeline).
pub fn effective_non_interactive(explicitly_requested: bool) -> bool {
    explicitly_requested || !stdin_is_tty()
}
```

- **CLI**: calls `effective_non_interactive(args.get_flag("non_interactive"))` once at the point where `CliFrontend` is constructed, stores the result in a `non_interactive: bool` field, and threads it through to all `WorkflowFrontend` methods. The per-method `stdin_is_tty()` checks in `workflow_frontend_marker.rs` are removed.
- **API**: removes the hard-coded `non_interactive=true` from `ParsedArgs` construction and instead calls `effective_non_interactive(parsed.get_bool("non_interactive"))` at frontend construction time. Because an HTTP API server never has a controlling TTY on stdin, `stdin_is_tty()` returns false and non-interactive is always enforced — the behaviour is identical to today, but now derived from the same logic as the CLI.

A log message at `INFO` level is emitted when auto-enforcement fires (i.e., when `explicitly_requested` is false but `!stdin_is_tty()` forces it true), so operators can observe the behaviour in logs.

### 5. Throttled Yolo Countdown Messages to Message Sink

**Problem**: `yolo_countdown_tick()` is called every ~100 ms, meaning the message sink receives ~600 messages during a 60-second countdown. This is very noisy for CLI stderr and API event streams.

**Solution**: Add a `last_sink_message_time: Option<Instant>` field to `CliFrontend` and `ApiDispatchFrontend`. In their `yolo_countdown_tick()` implementations, only call `self.write_message(...)` when either:
- `last_sink_message_time` is `None`, or
- `last_sink_message_time.unwrap().elapsed() >= Duration::from_secs(10)`.

Reset `last_sink_message_time` in `yolo_countdown_finished()`.

The TUI receives per-tick updates via the existing dialog channel (`yolo_countdown_tick()` on `TuiCommandFrontend`) — no change needed there; the TUI dialog already renders every tick.

The throttle constant (`10 seconds`) should be a named constant in `src/engine/workflow/timing.rs`.

### 6. Message Sink Handling Per Frontend

Audit `report_step_status()`, `report_step_output()`, and `report_workflow_completed()` across all three frontends and confirm they write appropriately to their message sink. Specifically:

- **CLI**: ensure messages are not printed while a container PTY owns the terminal (the existing `CliUserMessageQueue` with `pty_active` flag handles this; confirm it is set/cleared correctly when CLI enters/exits PTY mode).
- **API**: ensure `StatusMessage` events are emitted for all relevant lifecycle transitions.
- **TUI**: no change needed; TUI routes everything to the shared status log.

## Edge Case Considerations

- **Container produces no output at all from the start**: `last_activity` starts as `None`. The stuck detector task must not fire immediately — treat `None` as "clock started at container launch" and use the container-start timestamp as the baseline until the first byte arrives.
- **Container exits cleanly before stuck timeout**: The stuck detector task must be cancelled when `ContainerExecution::wait()` returns. The broadcast channel going dead (sender dropped) is the natural signal; the `subscribe_stuck()` receiver will return `Err(RecvError)`, which the workflow engine's `select!` arm treats as a no-op.
- **Multiple rapid stuck/unstuck transitions**: The workflow engine should debounce: once a yolo countdown starts, ignore further `StuckEvent::Stuck` from the same receiver; once unstuck, reset the countdown and require a fresh `STUCK_TIMEOUT` silence before retriggering.
- **CLI interactive mode + countdown overlay**: If the terminal is in raw mode for a full-screen agent, the ANSI-overlay approach must save/restore cursor position and use a line at the bottom of the viewport. If terminal dimensions are unavailable, fall back to printing on stderr with `\r\n`.
- **Auto-TTY enforcement interaction with `--yolo`**: A user might pass `--yolo` without `--non-interactive` and run in a CI environment. `effective_non_interactive()` is unconditional on stdin being a non-TTY — it applies regardless of `--yolo`.
- **API always non-interactive via TTY check**: After removing the hard-coded `non_interactive=true`, the API relies on `stdin_is_tty()` returning false (which is always true in an HTTP server context). If any future code path accidentally gives the API process a controlling TTY on stdin, it would become interactive — this should be detected in tests.
- **Raw mode not restored on panic**: If `CliFrontend` is dropped due to a panic while a `RawModeGuard` is held, the guard's `Drop` impl must restore cooked mode. Use `crossterm::terminal::disable_raw_mode()` in the `Drop` impl rather than relying on a cleanup call site.
- **Terminal state between workflow steps**: After the `RawModeGuard` drops at the end of a step, the workflow engine may print status messages (step completed, next step launching) before the next `take_container_io()` is called. These messages must print in cooked mode. The guard must be dropped before `report_step_status()` returns, not deferred.
- **Concurrent yolo countdown overlay and raw mode**: The countdown overlay uses ANSI escape sequences while the terminal is in raw mode. Stdout/stderr write calls must use unbuffered writes (or explicit flushes) in raw mode — buffered I/O may not flush until a newline that never comes.
- **Stuck detection during setup/teardown steps**: The stuck detector task is tied to `ContainerExecution`. Setup and teardown steps are short-lived commands — the workflow engine should not subscribe to `subscribe_stuck()` for them, only for main workflow step executions.
- **`StuckEvent::Unstuck` after countdown expires**: If the countdown expires and the engine advances the step, a late `Unstuck` event must be ignored. The workflow engine's `select!` arm for stuck events should be a no-op once the step is in the advancing/completed state.
- **Resize during CLI PTY passthrough**: When the CLI opts into `ContainerIo` with PTY fields, `SIGWINCH` must propagate terminal size changes to the container PTY via the `resize` channel in `ContainerIo`.
- **Dropped output sink receivers**: If the frontend task draining a stdout/stderr sink panics or exits early, the `UnboundedSender` returns `SendError`. The engine's reader thread treats this as non-fatal and continues draining the container's output (discarding bytes) so the container is not blocked.
- **stdin_rx EOF for non-interactive frontends**: Non-interactive frontends (CLI `--non-interactive`, API) drop `stdin_tx` immediately after constructing the channel pair. The engine's writer task must treat `None` from `stdin_rx.recv()` as EOF and close the container's stdin pipe cleanly — not as an error — so agents that probe stdin for EOF work correctly.
- **Raw-mode stdin for CLI interactive**: The CLI's raw-mode reader thread on `/dev/tty` must be spawned only when `initial_size` is `Some` (interactive PTY path). It must be torn down when the container exits — the engine signals this by dropping its end of the `ContainerIo` channels. Failure to tear it down will leave the terminal in raw mode after the container exits.
- **Seeded prompt ordering**: The engine writes the seeded prompt into `stdin_tx` before the container's writer task starts draining, so the prompt is guaranteed to be the first bytes the container reads on stdin. The writer task must flush after writing the seed.
- **try_inject_stdin still works**: `try_inject_stdin` (used by `ContinueInCurrentContainer`) pushes bytes into the `stdin_tx` clone retained by the engine. This works identically in PTY and piped modes — the writer task drains both the frontend's keypresses and the engine's injected bytes through the same channel.

## Test Considerations

- **Unit — `ContainerIo` channels**: Verify that each frontend's `take_container_io()` returns live channels; send bytes through `stdout`/`stderr` senders and assert they arrive at the correct destination (vt100 parser for TUI, terminal writer for CLI, event bus for API); send bytes into `stdin_tx` and verify the engine's writer task delivers them to a mock container stdin pipe.
- **Unit — stdin EOF for non-interactive frontends**: Construct a non-interactive CLI or API `ContainerIo` (where `stdin_tx` is immediately dropped), have the engine's writer task drain `stdin_rx`, and verify it terminates cleanly without error.
- **Unit — stuck detector task**: Using a fake `last_activity` that can be manually time-shifted, verify that the detector emits `StuckEvent::Stuck` after `STUCK_TIMEOUT` with no activity, emits `StuckEvent::Unstuck` when activity resumes, and emits nothing when the container exits before the timeout. This test covers the shared helper, not a backend-specific path.
- **Unit — `subscribe_stuck()` broadcast**: Subscribe two receivers from a single `ContainerExecution` and verify both receive the same `Stuck`/`Unstuck` events.
- **Unit — both backends wire unified I/O**: For both `DockerContainerInstance` and `AppleContainerInstance`, verify that `run_with_frontend` calls `take_container_io()` and that stdout/stderr bytes from the backend reach the frontend's sinks, and that bytes from `stdin_rx` reach the container's stdin — tested with a fake frontend in both PTY-bridged and piped paths.
- **Unit — `effective_non_interactive()`**: Test the shared function directly: verify it returns `true` when the explicit flag is set, when stdin is not a TTY, and when both are true; returns `false` only when the flag is unset AND stdin is a TTY.
- **Unit — raw mode guard drops on step completion**: Construct a CLI frontend in interactive mode (with a mock `RawModeGuard`), call `report_step_status(step, Succeeded/Cancelled/Failed)`, and verify the guard is dropped (raw mode disabled) before the call returns.
- **Integration — terminal restored after yolo auto-advance**: In a CLI interactive yolo workflow, trigger a stuck condition, let the countdown expire, and assert that the terminal is in cooked mode (not raw mode) after the step advances and before the next step's container starts.
- **Unit — countdown message throttle**: Call `yolo_countdown_tick()` rapidly (simulate 50 ticks at 100 ms each) on CLI and API frontends. Assert the message sink receives at most one message per 10-second window.
- **Integration — CLI non-interactive yolo**: Spawn a fake workflow container that blocks indefinitely. Run `awman exec workflow --yolo` with stdin redirected from `/dev/null` (no TTY). Assert auto-enforcement fires, stuck detection triggers after 30 s, countdown runs for 60 s, and the step auto-advances.
- **Integration — API yolo stuck detection**: POST a workflow run via the API with a blocking container. Assert that a `StatusMessage` event with countdown info appears in the SSE stream every ~10 s, and that the step advances after the countdown.
- **Integration — TUI stuck detection parity**: Verify that the TUI still correctly colors a tab and the workflow engine enters countdown when a tab goes silent, driven by the engine-layer `StuckEvent` subscription rather than the removed `recompute_stuck()`.
- **E2E — CI headless pipeline**: Run `awman exec workflow --yolo` in a pty-less subprocess (using `std::process::Command` with no inherited stdin). Assert exit code and that no interactive prompts were attempted.

## Codebase Integration

- Follow established conventions, best practices, testing, and architecture patterns from the project's `aspec/`.
- The unified `ContainerIo` struct, `StuckEvent`, and `subscribe_stuck()` belong in `src/engine/container/` — they are engine-layer types with no frontend dependencies.
- The stuck detector task and the full I/O bridge infrastructure (stdout/stderr reader threads, stdin writer task, activity timestamp update) must be implemented identically for both `docker.rs` and `apple.rs`. Extract this shared logic into a helper in `src/engine/container/` so neither backend duplicates it. Both `spawn_pty_bridged_*` functions and both piped-path `run_with_frontend` impls call this helper.
- `STUCK_TIMEOUT` belongs in `src/engine/container/timing.rs` (new file) — it is a container-layer concern. `YOLO_COUNTDOWN_DURATION` and the new `YOLO_SINK_THROTTLE_INTERVAL` (10 s) belong in `src/engine/workflow/timing.rs` — they are workflow-layer concerns.
- The `write_stdout`, `write_stderr`, `read_stdin`, and `resize_pty` methods on `ContainerFrontend` become dead code after this change and should be removed from the trait and all implementations to avoid confusion.
- No new channel types are needed — reuse `tokio::sync::mpsc::UnboundedSender<Vec<u8>>` / `UnboundedReceiver<Vec<u8>>` throughout.
- The workflow engine's `step_once_interruptible()` acquires a `subscribe_stuck()` receiver after `run_with_frontend` and adds it to the existing `tokio::select!` — no polling loop.
- `effective_non_interactive()` is the single point of truth for both CLI and API; per-method `stdin_is_tty()` checks in `workflow_frontend_marker.rs` and the hard-coded `non_interactive=true` in `ApiDispatchFrontend` are both removed in favour of this shared function.
- The CLI's `RawModeGuard` is a RAII type stored on `CliFrontend`; its `Drop` impl calls `crossterm::terminal::disable_raw_mode()`. It must be stored as `Option<RawModeGuard>` so it can be explicitly dropped (via `take()`) in `report_step_status()` without waiting for `CliFrontend` to be dropped.
- Prefer pushing logic toward `src/engine/` and keeping `src/frontend/*/` as thin I/O adapters — consistent with the existing layering in `aspec/architecture/design.md`.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** — update `docs/08-headless-mode.md` to document the TTY auto-detection behavior and that `--non-interactive` is no longer required in CI/CD environments.
- **Update yolo/workflow docs** — update whichever doc covers `--yolo` to describe stuck detection, the countdown, and the 10-second sink message throttle.
- **Never create work-item-specific docs** (e.g., no "WI 0081 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.
