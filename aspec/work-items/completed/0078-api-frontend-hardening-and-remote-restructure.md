# Work Item: Task

Title: The Great Refocusing — Part 2: API Frontend Hardening, Remote Restructure & Unified Event Bus
Issue: issuelink

## Summary

This work item covers five tightly related changes that together define the boundary of what the API frontend accepts, how the remote command is structured, and how ALL execution output — status updates, workflow stage transitions, AND subprocess/container logs — are streamed to both local logfiles and SSE clients.

1. **API frontend scope restriction**: The API frontend (formerly "headless") will ONLY accept `exec workflow` and `exec prompt` commands. Any other command attempted via the API returns HTTP 400 Bad Request. This is enforced at the Layer 2 `CommandCatalogue` level — not ad-hoc in route handlers.

2. **Remote command restructure**: The `awman remote` command, which previously accepted an arbitrary command string as an argument, is restructured into concrete subcommands: `awman remote session start`, `awman remote session kill`, `awman remote exec workflow`, and `awman remote exec prompt`. No other remote subcommands exist. `awman remote session start` accepts `--type local --workdir <path>` or `--type remote --repo-url <url> --branch <branch>` (see WI 0079 for session type details).

3. **Always-yolo enforcement**: The API server always injects `--yolo` and `--non-interactive` for all `exec workflow` and `exec prompt` requests. This is enforced at the Layer 3 API frontend level, by passing these flags unconditionally when constructing the `DispatchFrontend` for API-originated exec requests. Clients cannot override this.

4. **Async session creation with setup pipeline**: `POST /sessions` validates the request and returns HTTP 202 Accepted immediately with a `session_id`. Session setup (cloning repo, setting up branches, running `ready`) continues asynchronously in a background task. The session does not accept jobs until setup completes. A new `GET /sessions/{id}/status` endpoint lets clients poll for readiness — the response includes the overall status, the current setup stage (e.g. which `ReadyPhase` is running), and on completion the full `ReadySummary` as JSON (the same data that drives the CLI/TUI status table). The `remote session start` command gains a `--wait` flag that polls this endpoint every 5 seconds, displays progress via the message sink, and renders the ready status table from the API JSON when setup reaches a terminal state.

5. **Unified Event Bus and log streaming**: The API frontend's direct-to-file logging model (`Arc<Mutex<File>>`) is replaced with an `EventBus` — a `tokio::sync::broadcast` channel internal to the API frontend (Layer 3). The existing engine trait pipeline (`UserMessageSink`, `ContainerFrontend`, `WorkflowFrontend`) remains the single source of execution output. The API frontend's trait implementations now emit typed `ExecutionEvent` values onto the `EventBus` instead of writing raw text to a log file. Subscribers of the bus include: a logfile writer (always active, writes NDJSON to `events.log` and plain text to `output.log`) and zero or more SSE client connections. The `/logs` endpoint is reworked to use SSE push from the `EventBus` rather than file-tailing, and the `--follow` flag in both CLI and TUI consumes this same SSE stream for real-time output.

Before implementing, read and internalize `aspec/architecture/2026-grand-architecture.md` in full. Every change must respect the four-layer boundary constraints.

## User Stories

### User Story 1:
As a: developer integrating the awman API

I want to:
receive a clear HTTP 400 Bad Request (with a descriptive error body) if I attempt to call any command other than `exec workflow` or `exec prompt` through the API

So I can:
understand immediately that those operations are not available via the API, without ambiguity or silent failure

### User Story 2:
As a: user of the remote command

I want to:
use clear, discoverable subcommands (`awman remote session start`, `awman remote exec workflow`, etc.) instead of passing raw command strings as arguments

So I can:
get proper `--help` output, flag validation, and shell completion for each remote operation without having to know the internal command string format

### User Story 3:
As a: workflow author submitting jobs via the API

I want to:
have yolo and non-interactive mode enforced server-side on all exec requests

So I can:
rely on workflows running unattended without needing to pass those flags in every API request, and without any risk of a workflow blocking waiting for interactive input

### User Story 4:
As a: platform operator creating an API session

I want to:
receive an immediate HTTP 202 response with a session ID when I create a session, then poll a status endpoint to know when the session has finished setting up (repo clone, branch setup, `ready` checks)

So I can:
create sessions without blocking on potentially long-running setup work (Docker image builds, repo clones), and control when and how I wait for readiness — whether by polling from a script, using `--wait` from the CLI, or monitoring from a dashboard

### User Story 4a:
As a: developer using the CLI to create a remote session

I want to:
pass `--wait` to `awman remote session start` and see real-time progress — which setup stage is running, and the final ready status table when complete

So I can:
know exactly what's happening during session setup without switching to a browser or writing a polling script

### User Story 5:
As a: developer or operator watching a running workflow

I want to:
see every status update, workflow stage transition, and container log line streamed in real time — whether I'm using `--follow` in the CLI, watching a TUI tab, or consuming the SSE `/logs` endpoint from a custom HTTP client

So I can:
monitor long-running workflows without polling, catch errors the moment they happen, and build integrations (dashboards, alerting) on top of a standardized event stream

### User Story 6:
As a: platform operator auditing completed runs

I want to:
have a complete local logfile at `~/.awman/api/sessions/{session_id}/jobs/{job_id}/events.log` containing every event that occurred during execution — both status transitions and subprocess output lines — with timestamps

So I can:
review the full execution history after the fact, pipe it to log aggregation tools, and debug failures without needing the SSE stream to have been connected at the time


## Implementation Details

### Layer 0: Data (`src/data/`)

#### ExecutionEvent Type
New module `src/data/execution_event.rs`:

```rust
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A single event emitted during command/workflow execution.
/// Every event carries a timestamp and a typed payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionEvent {
    pub timestamp: DateTime<Utc>,
    pub sequence: u64,
    pub payload: EventPayload,
}

/// The payload discriminant for execution events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum EventPayload {
    /// A line of stdout from the container/subprocess.
    StdoutLine(String),
    /// A line of stderr from the container/subprocess.
    StderrLine(String),
    /// A structured status message from the engine (e.g. "Building container image...",
    /// "Mounting overlays...", "Seeding prompt...").
    StatusMessage { phase: String, message: String },
    /// Workflow step state transition.
    WorkflowStepTransition {
        step_name: String,
        step_index: usize,
        from_status: String,
        to_status: String,
    },
    /// Workflow-level phase transition (setup/teardown).
    WorkflowPhaseTransition {
        phase: String,       // "setup" | "main" | "teardown"
        step_desc: String,
        status: String,      // "running" | "succeeded" | "failed"
    },
    /// Overall command status change (pending → running → done/error).
    CommandStatus {
        status: String,
        exit_code: Option<i32>,
        error: Option<String>,
    },
    /// Sentinel: the command has completed. Always the last event.
    Done,
}
```

`ExecutionEvent` is serializable to JSON for SSE transmission and logfile persistence. The `sequence` field is a monotonically increasing counter per-command, enabling clients to detect missed events and resume from a known position.

The serialized SSE wire format for each event is:
```
event: <payload type lowercase, e.g. "stdout_line", "workflow_step_transition", "command_status", "done">
data: <full JSON of ExecutionEvent>

```

This means external SSE clients can filter by `event:` type without parsing the JSON body, while still having the full structured event in `data:`.

#### Event Log Format on Disk
The logfile at `~/.awman/api/sessions/{sid}/jobs/{jid}/events.log` is newline-delimited JSON (NDJSON). Each line is one `ExecutionEvent` serialized as JSON. This makes the file trivially parseable and appendable.

A legacy-compatible `output.log` is ALSO written alongside `events.log`. This file contains only the human-readable output (stdout/stderr lines and status messages, no JSON wrapping), for backward compatibility with tooling that reads plain-text logs. It is derived from the same `EventBus` subscriber — not written separately.

#### API Visibility in the CommandCatalogue
- Add a new `FrontendVisibility` variant: `ExcludeFromApi` (or more precisely, extend the existing enum so that commands can declare themselves as `ApiAllowed` vs the default which is not allowed via API).
- The preferred approach: `FrontendVisibility` gains a boolean flag `api_allowed: bool` on each command's `CommandSpec`. Only `exec workflow` and `exec prompt` have `api_allowed: true`.
- `CommandCatalogue` exposes a method `api_allowed_commands() -> &[CommandSpec]` which returns only the API-permitted subset.
- The `Dispatch` layer uses this catalogue method to validate incoming API requests before routing them to a command — this validation lives in `Dispatch`, NOT in the Layer 3 route handlers.
- `Dispatch` exposes a method such as `validate_frontend_command(frontend: FrontendKind, command: &str, subcommand: &str) -> Result<(), DispatchError>` where `FrontendKind` is a new Layer 2 enum with variants `Cli`, `Tui`, `Api`. This is called by the API frontend before executing any command.

### Layer 1: Engine (`src/engine/`)

#### No Engine Changes for EventBus

The `EventBus` does NOT live in the engine layer. The engine layer continues to use the existing trait-based output pipeline (`UserMessageSink`, `ContainerFrontend`, `WorkflowFrontend`) as its sole mechanism for emitting output and status. This preserves the grand architecture's tenet: lower layers delegate to higher layers via traits, never the reverse.

The engine already emits everything the `EventBus` needs:
- Container stdout/stderr → `ContainerFrontend::write_stdout` / `write_stderr`
- Status messages → `UserMessageSink::write_message(UserMessage)`
- Container lifecycle → `ContainerFrontend::report_status(ContainerStatus)` / `report_progress(ContainerProgress)`
- Workflow step transitions → `WorkflowFrontend::report_step_status(step, WorkflowStepStatus)`
- Workflow completion → `WorkflowFrontend::report_workflow_completed(outcome)`
- Setup/teardown progress → `WorkflowFrontend::report_workflow_progress(steps)`

The `EventBus` is a distribution mechanism that the API frontend uses INTERNALLY to fan out these trait callbacks to multiple consumers (logfile writer, SSE clients). It replaces the current `Arc<Mutex<std::fs::File>>` direct-write pattern — not the trait pipeline itself.

#### Relationship Between Frontend Traits and EventBus

```
┌──────────────────────────────────────────────────────────────┐
│  Engine Layer (Layer 1)                                      │
│                                                              │
│  ContainerRuntime ──write_stdout()──→ ContainerFrontend      │
│  WorkflowEngine  ──report_step()───→ WorkflowFrontend       │
│  (any engine)    ──write_message()─→ UserMessageSink         │
│                                                              │
│  These are the ONLY output paths. The engine does not know   │
│  about EventBus, SSE, logfiles, or broadcast channels.       │
└──────────────────────────────────────────────────────┬───────┘
                                                       │
                              trait implementations    │
                                                       ▼
┌──────────────────────────────────────────────────────────────┐
│  API Frontend (Layer 3)                                      │
│                                                              │
│  ApiDispatchFrontend implements ContainerFrontend,            │
│  WorkflowFrontend, UserMessageSink. Each method:             │
│                                                              │
│    fn write_stdout(&mut self, bytes: &[u8]) {                │
│        // split bytes into lines                             │
│        // for each line:                                     │
│        //   self.event_bus.emit(StdoutLine(line))            │
│    }                                                         │
│                                                              │
│    fn report_step_status(&mut self, step, status) {          │
│        // self.event_bus.emit(WorkflowStepTransition{...})   │
│    }                                                         │
│                                                              │
│    fn write_message(&mut self, msg: UserMessage) {           │
│        // self.event_bus.emit(StatusMessage{...})            │
│    }                                                         │
│                                                              │
│  EventBus subscribers (spawned at command start):             │
│    ├─ Logfile writer task → events.log (NDJSON)              │
│    │                      → output.log (plain text)          │
│    └─ SSE handler(s) → HTTP response streams                 │
└──────────────────────────────────────────────────────────────┘
```

This design means:
- **CLI frontend**: Unchanged. `CliUserMessageQueue` writes to stderr with PTY queuing. No EventBus involved.
- **TUI frontend**: Unchanged. `TuiUserMessageSink` appends to `Arc<Mutex<Vec>>` for render loop. No EventBus involved.
- **API frontend**: `ApiDispatchFrontend` (renamed from `HeadlessDispatchFrontend`) replaces its `log_file: Arc<Mutex<File>>` with an `event_bus: EventBusSender`. Every trait method now emits a typed `ExecutionEvent` instead of writing raw text to a file.
- **TUI/CLI consuming REMOTE commands**: They connect to the server's SSE `/logs` endpoint as an HTTP client. They don't use a local EventBus — they consume the remote server's EventBus output over the network.

This is NOT dual-emission. Each event originates exactly once: the engine calls a frontend trait method, and the frontend's implementation decides what to do with it. For the API frontend, "what to do" is "emit to EventBus." For CLI, it's "print to stderr." For TUI, it's "append to render buffer."

#### Remote Command Restructure
- Delete the existing `RemoteCommand` implementation that accepts an arbitrary command string argument.
- Replace with a new `RemoteCommand` that defines concrete subcommands via the `CommandCatalogue`:
  - `remote session start` — starts a remote awman API session (wraps the `POST /sessions` HTTP call)
  - `remote session kill` — kills a remote awman API session (wraps `DELETE /sessions/{id}`)
  - `remote exec workflow` — submits an `exec workflow` job to a remote awman API server
  - `remote exec prompt` — submits an `exec prompt` job to a remote awman API server
- Each subcommand has its own flag set registered in `CommandCatalogue`:
  - `remote session start` flags: `--type <local|remote>` (required), `--workdir <path>` (required when `--type local`), `--repo-url <url>` (required when `--type remote`), `--branch <name>` (optional, used when `--type remote`; defaults to the remote's default branch if omitted), `--wait` (bool, default false in CLI, always true in TUI — polls `GET /sessions/{id}/status` every 5 seconds until setup reaches a terminal state; displays progress and the ready status table on completion)
  - `remote exec workflow` flags: mirrors the local `exec workflow` flag set, minus `--workdir` (which is set server-side by the session); derive this list programmatically from the core `exec workflow` CommandSpec; additionally adds `--follow` (bool, default false in CLI, always true in TUI — see "Remote Follow Mode" below)
  - `remote exec prompt` flags: similarly derived from local `exec prompt`, plus `--follow`
  - `remote session kill`: similarly derived

#### Async Session Creation with Setup Pipeline

Session creation is a two-phase process: the HTTP handler performs fast synchronous validation, then hands off the slow setup work to a background task. The session exists immediately but does not accept jobs until setup completes.

**Phase 1 — Synchronous validation (in the `POST /sessions` handler):**
1. Validate the request body (workdir exists for `local`, repo_url is non-empty for `remote`, etc.)
2. Generate a `session_id` (UUID)
3. Persist a session record to `ApiDb` with `setup_status = 'initializing'`
4. Create a `SessionSetupBus` (see below) and store it in `AppState.setup_buses` keyed by `session_id`
5. Spawn a background `tokio::task` that runs the setup pipeline (Phase 2)
6. Return HTTP 202 Accepted immediately with `{ "session_id": "..." }`

**Phase 2 — Asynchronous setup (background task):**
The background task runs the full setup pipeline, emitting `SessionSetupEvent` values onto the `SessionSetupBus` at each stage transition:

```
1. [remote only] Clone repository
   → emit SetupStage("cloning_repository", "Cloning {repo_url}...")
   → call GitEngine::clone_repo(url, branch, cloned_path)
   → on success: emit SetupStage("cloning_repository_done", "Repository cloned")
   → on failure: emit SetupFailed { stage: "clone", error }; mark session failed; return

2. [remote only] Set up branch
   → emit SetupStage("setting_up_branch", "Checking out branch '{branch}'...")
   → call GitEngine::checkout_or_create_branch(cloned_path, branch)
   → on success: emit SetupStage("branch_ready", "Branch '{branch}' {disposition}")
   → on failure: clean up cloned_path; emit SetupFailed; mark session failed; return

3. [all sessions] Open Session object
   → Create Session via SessionManager (Layer 0)
   → Store in AppState.sessions

4. [all sessions] Run ReadyCommand
   → emit SetupStage("running_ready", "Running ready checks...")
   → Create ReadyEngine with non-interactive options (all ask_* return safe defaults)
   → Run ReadyEngine::run_to_completion() with a SetupReadyFrontend that:
     a. On each report_phase(phase): emit ReadyPhaseChanged { phase } on the SetupBus
     b. On each report_step_status(step, status): emit ReadyStepStatus { step, status }
     c. On report_summary(summary): emit ReadyComplete { summary }
   → on success: emit SetupComplete { ready_summary }; mark session setup_status = 'ready'
   → on failure:
     - For remote sessions: clean up cloned_path via GitEngine::delete_directory
     - For local sessions: no filesystem cleanup (workdir belongs to user)
     - emit SetupFailed { stage: "ready", error }; mark session setup_status = 'failed'
     - Do NOT delete the session record — the session exists in 'failed' state so operators can inspect what happened and retry
```

**`ReadyCommand` is NOT added to the API `CommandCatalogue`** as a directly-invocable command. It is never exposed as an API endpoint. Its execution during session setup is an internal Layer 2 concern coordinated by `CreateSessionCommand`.

**`ReadyCommand` runs non-interactively**: All interactive prompts (`ask_create_dockerfile`, `ask_run_audit_on_template`) return safe non-interactive defaults — the `SetupReadyFrontend` always returns `true` for dockerfile creation and `false` for audit. Add a `run_non_interactive` method or `non_interactive: bool` parameter to `ReadyCommand` if not already present.

**Ready idempotency**: `ReadyEngine::run_to_completion` may be called across multiple session creations within a single server process lifetime. It must be idempotent — if the base image is already built and agents are already configured, it completes quickly without rebuilding.

#### SessionSetupBus and Event Types (Layer 3)

The `SessionSetupBus` follows the same pattern as the command `EventBus` but is scoped to session setup lifecycle. It lives in `src/frontend/api/session_setup.rs`.

```rust
use tokio::sync::broadcast;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use crate::data::session_setup_event::{SessionSetupEvent, SetupEventPayload};

/// Broadcast channel for session setup lifecycle events.
/// Created once per session creation. The setup background task emits events.
/// The /status endpoint reads current state. The bus is dropped after setup
/// reaches a terminal state (ready or failed) plus a grace period.
pub struct SessionSetupBus {
    tx: broadcast::Sender<SessionSetupEvent>,
    sequence: Arc<AtomicU64>,
    /// Current aggregate state, updated on every emit. Read by the /status
    /// endpoint without subscribing to the broadcast channel.
    current_state: Arc<tokio::sync::RwLock<SessionSetupState>>,
}
```

The `current_state` field is the key difference from the command `EventBus`. The `/status` endpoint reads this directly — it does NOT subscribe to the broadcast channel. The broadcast channel exists so that future SSE-based status streaming can be added, but the primary access pattern for session status is polling.

#### SessionSetupState and Event Types (Layer 0)

New module `src/data/session_setup_event.rs`:

```rust
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use crate::engine::ready::phase::ReadyPhase;
use crate::engine::ready::summary::ReadySummary;
use crate::engine::step_status::StepStatus;

/// A single event emitted during session setup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSetupEvent {
    pub timestamp: DateTime<Utc>,
    pub sequence: u64,
    pub payload: SetupEventPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum SetupEventPayload {
    /// A setup stage transition (clone, branch, ready, etc.)
    StageChanged {
        stage: String,
        message: String,
    },
    /// A ReadyPhase transition within the "running_ready" stage.
    ReadyPhaseChanged {
        phase: ReadyPhase,
        message: String,
    },
    /// A ReadyEngine step status update within a phase.
    ReadyStepStatus {
        step: String,
        status: StepStatus,
    },
    /// Setup completed successfully.
    SetupComplete {
        ready_summary: ReadySummary,
    },
    /// Setup failed at a specific stage.
    SetupFailed {
        stage: String,
        error: String,
    },
}
```

Aggregate state type for the `/status` endpoint:

```rust
/// The current aggregate state of session setup.
/// Updated atomically by the setup task on every event.
/// Read by the /status endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSetupState {
    pub status: SessionSetupStatus,
    /// Human-readable description of what's currently happening.
    /// e.g. "Cloning repository...", "Building base image...", "Checking local agent..."
    pub current_stage: Option<String>,
    /// The current ReadyPhase, if setup is in the "running_ready" stage.
    /// Allows clients to show granular progress.
    pub current_ready_phase: Option<ReadyPhase>,
    /// Per-step status updates from the ReadyEngine, accumulated as they arrive.
    /// Allows the /status endpoint to show a partial ready table mid-setup.
    pub ready_step_statuses: Vec<ReadyStepEntry>,
    /// Populated when setup reaches the "ready" terminal state.
    pub ready_summary: Option<ReadySummary>,
    /// Populated when setup reaches the "failed" terminal state.
    pub error: Option<SessionSetupError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadyStepEntry {
    pub step: String,
    pub status: StepStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSetupError {
    pub stage: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSetupStatus {
    Initializing,
    CloningRepository,
    SettingUpBranch,
    RunningReady,
    Ready,
    Failed,
}
```

`SessionSetupStatus` variants:
- `initializing` → session record created, background task starting
- `cloning_repository` → (remote only) `GitEngine::clone_repo` in progress
- `setting_up_branch` → (remote only) `GitEngine::checkout_or_create_branch` in progress
- `running_ready` → `ReadyEngine::run_to_completion` in progress (with `current_ready_phase` and `ready_step_statuses` populated)
- `ready` → terminal success, `ready_summary` populated, session accepts jobs
- `failed` → terminal failure, `error` populated, session does not accept jobs

#### SetupReadyFrontend (Layer 3)

New struct `SetupReadyFrontend` in `src/frontend/api/session_setup.rs`. Implements `ReadyFrontend` and bridges ready engine callbacks to the `SessionSetupBus`:

```rust
pub struct SetupReadyFrontend {
    bus: SessionSetupBusSender,
    state: Arc<tokio::sync::RwLock<SessionSetupState>>,
    event_bus_sender: EventBusSender,  // optional: for writing to session setup log
}

impl ReadyFrontend for SetupReadyFrontend {
    fn ask_create_dockerfile(&mut self) -> Result<bool, EngineError> { Ok(true) }
    fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError> { Ok(false) }

    fn report_phase(&mut self, phase: &ReadyPhase) {
        let message = ready_phase_display(phase);
        // Update aggregate state
        let mut state = self.state.blocking_write();
        state.current_ready_phase = Some(phase.clone());
        state.current_stage = Some(message.clone());
        drop(state);
        // Emit event on bus
        self.bus.emit(SetupEventPayload::ReadyPhaseChanged {
            phase: phase.clone(),
            message,
        });
    }

    fn report_step_status(&mut self, step: &str, status: StepStatus) {
        // Update aggregate state — append or update the step entry
        let mut state = self.state.blocking_write();
        if let Some(entry) = state.ready_step_statuses.iter_mut().find(|e| e.step == step) {
            entry.status = status.clone();
        } else {
            state.ready_step_statuses.push(ReadyStepEntry {
                step: step.to_string(),
                status: status.clone(),
            });
        }
        drop(state);
        // Emit event
        self.bus.emit(SetupEventPayload::ReadyStepStatus {
            step: step.to_string(),
            status,
        });
    }

    fn report_summary(&mut self, summary: &ReadySummary) {
        let mut state = self.state.blocking_write();
        state.status = SessionSetupStatus::Ready;
        state.ready_summary = Some(summary.clone());
        state.current_stage = Some("Setup complete".to_string());
        drop(state);
        self.bus.emit(SetupEventPayload::SetupComplete {
            ready_summary: summary.clone(),
        });
    }

    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend> {
        Box::new(ApiContainerSink {
            event_bus: self.event_bus_sender.clone(),
            line_buffer: String::new(),
        })
    }
}

/// Map ReadyPhase variants to human-readable descriptions.
fn ready_phase_display(phase: &ReadyPhase) -> String {
    match phase {
        ReadyPhase::Preflight => "Running preflight checks...".into(),
        ReadyPhase::AwaitingDockerfileDecision => "Checking Dockerfile...".into(),
        ReadyPhase::CreatingDockerfile => "Creating Dockerfile.dev...".into(),
        ReadyPhase::BuildingBaseImage => "Building base image...".into(),
        ReadyPhase::BuildingAgentImage => "Building agent image...".into(),
        ReadyPhase::CheckingNonDefaultAgents => "Checking non-default agent images...".into(),
        ReadyPhase::CheckingLocalAgent => "Checking local agent...".into(),
        ReadyPhase::RunningAudit => "Running audit...".into(),
        ReadyPhase::RebuildingAfterAudit => "Rebuilding after audit...".into(),
        ReadyPhase::Complete => "Ready checks complete".into(),
        ReadyPhase::Failed(f) => format!("Failed: {}", f.message),
    }
}
```

#### `GET /sessions/{id}/status` Endpoint (Layer 3)

New route in the API router: `GET /v1/sessions/{id}/status`

Response schema:
```json
{
  "session_id": "a1b2c3d4-...",
  "status": "running_ready",
  "current_stage": "Building base image...",
  "current_ready_phase": "BuildingBaseImage",
  "ready_step_statuses": [
    { "step": "Dockerfile", "status": "Done" },
    { "step": "Base image", "status": "Running" }
  ],
  "ready_summary": null,
  "error": null
}
```

When `status` is `"ready"`:
```json
{
  "session_id": "a1b2c3d4-...",
  "status": "ready",
  "current_stage": "Setup complete",
  "current_ready_phase": "Complete",
  "ready_step_statuses": [
    { "step": "Dockerfile", "status": "Done" },
    { "step": "Base image", "status": "Done" },
    { "step": "Agent image", "status": "Done" },
    { "step": "Local agent", "status": "Done" },
    { "step": "Audit", "status": "Skipped" },
    { "step": "Legacy migration", "status": "Skipped" }
  ],
  "ready_summary": {
    "runtime_name": "docker",
    "dockerfile": "Done",
    "base_image": "Done",
    "agent_image": "Done",
    "local_agent": "Done",
    "audit": "Skipped",
    "image_rebuild": "Skipped",
    "aspec_folder": "Done",
    "work_items_config": "Done",
    "non_default_agent_images": []
  },
  "error": null
}
```

When `status` is `"failed"`:
```json
{
  "session_id": "a1b2c3d4-...",
  "status": "failed",
  "current_stage": "Failed: Docker daemon not running",
  "current_ready_phase": "Failed",
  "ready_step_statuses": [
    { "step": "Dockerfile", "status": "Done" },
    { "step": "Base image", "status": { "Failed": "Docker daemon not running" } }
  ],
  "ready_summary": null,
  "error": {
    "stage": "ready",
    "message": "Docker daemon not running"
  }
}
```

Implementation:
- The handler reads `AppState.setup_buses[session_id].current_state` (the `Arc<RwLock<SessionSetupState>>`)
- If no `SessionSetupBus` exists for the session (setup completed long ago and bus was cleaned up), fall back to the session's persisted `setup_status` in `ApiDb`:
  - `setup_status = 'ready'` → return `{ "status": "ready" }` (without the detailed ready_summary, which was only in-memory; or persist it to `ApiDb` — see below)
  - `setup_status = 'failed'` → return `{ "status": "failed", "error": {...} }`
- If session does not exist: HTTP 404

**Persisting `ReadySummary` for durability**: When setup completes (success or failure), persist the `SessionSetupState` to the session's directory as `~/.awman/api/sessions/{sid}/setup_state.json`. The `/status` endpoint reads this file as a fallback when the in-memory `SessionSetupBus` has been cleaned up. This ensures the ready summary survives server restarts.

#### Job Submission Guard

The `POST /sessions/{id}/jobs` handler must check the session's `setup_status` before accepting a job:
- If `setup_status != 'ready'`: return HTTP 409 Conflict with `{ "error": "session is not ready", "setup_status": "<current status>", "hint": "Poll GET /v1/sessions/{id}/status to check setup progress" }`
- If `setup_status == 'failed'`: return HTTP 409 with `{ "error": "session setup failed", "setup_error": {...} }`
- Only `setup_status == 'ready'` allows job submission

This guard lives in the Layer 3 route handler — it reads the session's status from `ApiDb` (or the in-memory state) and rejects the request before reaching `Dispatch`.

#### `remote session start --wait` Flag (Layer 2 / Layer 3)

The `remote session start` command gains a `--wait` flag (boolean, default `false`). When `--wait` is passed:

1. Submit `POST /sessions` and receive `{ "session_id": "..." }`
2. Print `Session {session_id} created. Waiting for setup to complete...`
3. Enter a poll loop: every 5 seconds, call `GET /sessions/{id}/status`
4. After each poll, display the current state via the frontend's message sink:
   - Print `[{status}] {current_stage}` — e.g. `[running_ready] Building base image...`
   - If `ready_step_statuses` has changed since the last poll, print newly-updated step statuses:
     ```
     Dockerfile:     ✓ Done
     Base image:     … Running
     ```
5. When the response has `status == "ready"`:
   - Print the full ready status table, reconstructed from the `ready_summary` JSON using the same `render_summary_box` function used by CLI and TUI (this function already exists in `src/frontend/cli/per_command/helpers.rs`)
   - Convert the API's `ReadySummary` JSON into the `ReadySummary` struct (it already derives `Deserialize`), then build the `Vec<(&str, &StepStatus)>` rows and call `render_summary_box`
   - Print `Session {session_id} is ready.`
   - Exit successfully
6. When the response has `status == "failed"`:
   - Print the error from the `error` field
   - If `ready_step_statuses` is non-empty, print the partial status table showing what succeeded and what failed
   - Exit with code 1
7. Ctrl-C during `--wait` polling: exit cleanly with "Polling interrupted. Session setup is still running. Check status with: awman remote session start --session {id} status". Do not cancel the server-side setup.

**In TUI**: `--wait` is always `true` and not configurable. The TUI opens the remote session start dialog, submits the request, and shows a status indicator in the tab header that updates every 5 seconds. When setup completes, the ready summary is displayed in the tab's status log using the same rendering as local `ready`. When setup fails, the error is displayed and the tab shows the session in an error state.

**`RemoteClient` new method**:
- `get_session_status(session_id: &SessionId) -> Result<SessionSetupState>` — `GET /sessions/{id}/status`; returns the deserialized `SessionSetupState`

#### Always-Yolo Enforcement
- In `ExecWorkflowCommand` and `ExecPromptCommand`, add a method to the per-command frontend trait: `fn is_api_frontend(&self) -> bool`. When `true`, the command unconditionally sets `yolo = true` and `non_interactive = true` in the resolved `EffectiveConfig`, regardless of what was passed in flags.
- Alternatively (preferred for cleaner separation): the `ApiFrontend`'s implementation of `ExecWorkflowCommandFrontend` and `ExecPromptCommandFrontend` always returns `true` for `flag_bool("yolo")` and `flag_bool("non-interactive")`, and `Dispatch` passes those values when constructing the command. This means the enforcement lives in the Layer 3 API frontend's trait implementation — the command itself doesn't need to know it's being called from the API.
- There is no mechanism for API clients to pass `--yolo false` or disable non-interactive mode. The Layer 3 `ApiFrontend` ignores any such flags in the request payload and always returns `true` for these two flags.


### Layer 2: Command (`src/command/`)

#### Remote Follow Mode — SSE-Powered Streaming
`--follow` on `remote exec workflow` and `remote exec prompt` causes the client to connect to the server's SSE `/logs` endpoint and stream ALL events in real time. This replaces the polling-based approach from the original WI 78 — because the `/logs` endpoint now pushes structured events via SSE, there is no need to poll separate status and workflow endpoints.

**In CLI** (`--follow` is a boolean flag, default `false`):
- After the job is submitted and the `job_id` is returned, if `--follow` is `true`:
  - Open an SSE connection to `GET /v1/sessions/{session_id}/jobs/{job_id}/logs` (new per-job SSE endpoint)
  - Process incoming `ExecutionEvent` messages as they arrive:
    - `StdoutLine(line)` → print to stdout
    - `StderrLine(line)` → print to stderr
    - `StatusMessage { phase, message }` → print `[{phase}] {message}` to stderr
    - `WorkflowStepTransition { step_name, step_index, to_status, .. }` → print compact status line to stderr:
      ```
      [step {step_index}]  {step_name}  → {to_status}
      ```
    - `WorkflowPhaseTransition { phase, step_desc, status }` → print:
      ```
      [{phase}]    {step_desc}   → {status}
      ```
    - `CommandStatus { status, exit_code, .. }` → print final status to stderr
    - `Done` → close connection, exit with the command's exit code
  - The SSE connection has a 24-hour read timeout (matching pre-v0.7 behavior)
  - If the connection drops before `Done` is received, retry once after 2 seconds. If retry fails, print "Connection to server lost. Job may still be running. Check status with: awman remote exec workflow --session {id} --job {job_id} status" and exit with code 1.
- If `--follow` is `false` (default): print the `job_id` and return immediately; the user can connect to the SSE endpoint manually or poll the REST endpoints.

**In TUI** (`--follow` is always `true` and not configurable):
- After job submission, the TUI immediately opens an SSE connection to the `/logs` endpoint
- `StdoutLine` / `StderrLine` events are rendered in the container output pane (same rendering as local container output)
- `WorkflowStepTransition` / `WorkflowPhaseTransition` events update the workflow strip (same rendering as local workflows)
- `StatusMessage` events are shown in the tab's status area
- The TUI tab remains active and responsive while the SSE stream runs in the background

**Non-awman HTTP clients** (dashboards, CI integrations, monitoring tools):
- Connect to `GET /v1/sessions/{session_id}/jobs/{job_id}/logs` with `Accept: text/event-stream`
- Receive the standard SSE stream — each `event:` line names the event type, each `data:` line is the full JSON `ExecutionEvent`
- Filter on event types of interest (e.g. only `workflow_step_transition` for a dashboard, or `stdout_line` + `stderr_line` for a log viewer)
- Watch for the `done` event to know when execution is complete
- Parse the `sequence` field to detect gaps (lagged events)

**`RemoteClient` new methods** (Layer 2, `remote_client.rs`):
- `get_job(session_id: &SessionId, job_id: &JobId) -> Result<JobRecord>` — `GET /sessions/{id}/jobs/{job_id}`
- `get_workflow_state(session_id: &SessionId, job_id: &JobId) -> Result<Option<WorkflowState>>` — `GET /sessions/{id}/jobs/{job_id}/workflow`; returns `None` on HTTP 404 (job pending or prompt job)
- `stream_logs(session_id: &SessionId, job_id: &JobId) -> Result<impl Stream<Item = ExecutionEvent>>` — opens an SSE connection to `GET /sessions/{id}/jobs/{job_id}/logs`; returns an async stream of parsed `ExecutionEvent` values. Handles SSE protocol parsing (event/data lines, `\n\n` delimiters). The stream terminates when a `Done` event is received or the connection is closed.
- `RemoteCommand` is instantiated by `Dispatch` like all other commands; it receives a `RemoteCommandFrontend` trait that provides connection details (host, port, API key) from the frontend
- The `RemoteClient` (HTTP client in `remote_client.rs`) is updated to only expose methods matching the concrete operations; remove any general "run arbitrary command" method


### Layer 3: Frontend (`src/frontend/api/`)

#### EventBus — Definition and Placement
The `EventBus` lives in Layer 3 (`src/frontend/api/event_bus.rs`). It is NOT an engine concern — it is the API frontend's internal distribution mechanism, replacing the current `Arc<Mutex<File>>` pattern.

New module `src/frontend/api/event_bus.rs`:

```rust
use tokio::sync::broadcast;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use crate::data::execution_event::{ExecutionEvent, EventPayload};

/// A broadcast channel for execution events within the API server.
///
/// Created once per command/job execution. The API frontend's trait
/// implementations (ContainerFrontend, WorkflowFrontend, UserMessageSink)
/// emit events through the sender. Subscribers (logfile writer task, SSE
/// handler connections) hold Receiver handles.
///
/// Uses `tokio::sync::broadcast` so multiple consumers can independently
/// read the same stream without blocking the producer. Lagging receivers
/// skip to the latest event (with a `Lagged` error the subscriber handles).
pub struct EventBus {
    tx: broadcast::Sender<ExecutionEvent>,
    sequence: Arc<AtomicU64>,
}

impl EventBus {
    /// Create a new event bus with the given channel capacity.
    /// 4096 is a good default — enough to buffer a few seconds of
    /// high-throughput container output without dropping events for
    /// reasonably-paced subscribers.
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            tx,
            sequence: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Get a cloneable sender handle for passing to the dispatch frontend.
    pub fn sender(&self) -> EventBusSender {
        EventBusSender {
            tx: self.tx.clone(),
            sequence: Arc::clone(&self.sequence),
        }
    }

    /// Subscribe to the event stream. Returns a `broadcast::Receiver`.
    pub fn subscribe(&self) -> broadcast::Receiver<ExecutionEvent> {
        self.tx.subscribe()
    }
}

/// Cloneable sender handle. Carried by ApiDispatchFrontend (and
/// QueueWorkerFrontend from WI 0079) to emit events from trait methods.
#[derive(Clone)]
pub struct EventBusSender {
    tx: broadcast::Sender<ExecutionEvent>,
    sequence: Arc<AtomicU64>,
}

impl EventBusSender {
    /// Emit an event with an auto-assigned monotonic sequence number.
    pub fn emit(&self, payload: EventPayload) {
        let seq = self.sequence.fetch_add(1, Ordering::Relaxed);
        let event = ExecutionEvent {
            timestamp: chrono::Utc::now(),
            sequence: seq,
            payload,
        };
        let _ = self.tx.send(event);
    }
}
```

#### EventBus Lifecycle in the API Server
When a command is dispatched by the API server (via `Dispatch::run_command`), the API server:
1. Creates an `EventBus::new(4096)` for this command execution
2. Spawns a **logfile writer task** that subscribes to the `EventBus` and writes every event to two files:
   - `~/.awman/api/sessions/{sid}/jobs/{jid}/events.log` — NDJSON (one `ExecutionEvent` JSON per line)
   - `~/.awman/api/sessions/{sid}/jobs/{jid}/output.log` — plain-text (stdout/stderr lines and status messages only, for backward compatibility)
3. Stores the `EventBus` handle in the per-command state (keyed by `command_id` or `job_id`) in `AppState`
4. Passes an `EventBusSender` to the `HeadlessDispatchFrontend` (or `QueueWorkerFrontend` in the job queue model from WI 0079), which injects it into `ContainerOptions` and `WorkflowEngine` options
5. When the command completes, the dispatch frontend emits `EventPayload::Done`, which signals the logfile writer to flush and exit

The `EventBus` handle is retained in `AppState` for the duration of execution plus a short grace period (5 seconds), so that SSE clients connecting slightly after command start can still subscribe and receive events from the broadcast buffer.

#### Reworked `/logs` Endpoint — Push-Based SSE

**Per-job log streaming** (primary endpoint):
`GET /v1/sessions/{session_id}/jobs/{job_id}/logs` — SSE push endpoint

Behavior:
- If the job is currently **running**:
  1. Subscribe to the `EventBus` for this job (via `AppState.event_buses`)
  2. First, replay any already-emitted events by reading `events.log` from disk up to the current position (ensures clients connecting mid-execution see the full history)
  3. Then, stream new events from the `broadcast::Receiver` as they arrive
  4. Each event is sent as an SSE message:
     ```
     event: stdout_line
     data: {"timestamp":"...","sequence":42,"payload":{"type":"StdoutLine","data":"building module..."}}

     ```
  5. When `Done` is received, send it and close the connection
- If the job is **completed** or **failed** (no active `EventBus`):
  1. Read `events.log` from disk
  2. Replay all events as SSE messages
  3. Send a `Done` event
  4. Close the connection
- If the job does not exist: return HTTP 404

**Legacy per-command log streaming** (backward compatibility):
`GET /v1/commands/{id}/logs/stream` — retained for backward compatibility with pre-restructure clients

This endpoint continues to work as before (file-tailing SSE with `[amux:done]` sentinel) but is internally reimplemented to subscribe to the `EventBus` when available, falling back to file-tailing for completed commands. The wire format is the legacy `data: <line>\n\n` format (NOT the structured `ExecutionEvent` JSON). New clients should use the per-job endpoint instead.

**Static log retrieval**:
`GET /v1/commands/{id}/logs` — returns the full `output.log` content as JSON (unchanged from current behavior)
`GET /v1/sessions/{session_id}/jobs/{job_id}/logs?format=json` — returns the full `events.log` as a JSON array of `ExecutionEvent` objects (non-streaming, for after-the-fact analysis)

#### ApiDispatchFrontend — EventBus Integration (replaces HeadlessDispatchFrontend)

`ApiDispatchFrontend` (renamed from `HeadlessDispatchFrontend` per WI 0077, and `QueueWorkerFrontend` from WI 0079) replaces its `log_file: Arc<Mutex<std::fs::File>>` field with an `event_bus: EventBusSender` field. The frontend no longer writes to disk directly. All disk persistence is handled by the logfile writer task that subscribes to the `EventBus`.

The existing trait implementations are remapped as follows. Each currently calls `self.write_to_log(...)` which writes to the log file — replace with `self.event_bus.emit(...)`:

**`UserMessageSink` trait impl** (status messages from any engine):
```rust
fn write_message(&mut self, msg: UserMessage) {
    let phase = match msg.level {
        MessageLevel::Info => "info",
        MessageLevel::Warning => "warn",
        MessageLevel::Error => "error",
        MessageLevel::Success => "ok",
    };
    self.event_bus.emit(EventPayload::StatusMessage {
        phase: phase.to_string(),
        message: msg.text,
    });
}
```

**`ContainerFrontend` trait impl** (container stdout/stderr):
```rust
fn write_stdout(&mut self, bytes: &[u8]) -> Result<(), EngineError> {
    let text = String::from_utf8_lossy(bytes);
    self.line_buffer_stdout.push_str(&text);
    while let Some(pos) = self.line_buffer_stdout.find('\n') {
        let line = self.line_buffer_stdout[..pos].to_string();
        self.line_buffer_stdout = self.line_buffer_stdout[pos + 1..].to_string();
        self.event_bus.emit(EventPayload::StdoutLine(line));
    }
    Ok(())
}

fn write_stderr(&mut self, bytes: &[u8]) -> Result<(), EngineError> {
    // Same pattern as write_stdout but with StderrLine
}

fn report_status(&mut self, status: ContainerStatus) {
    let message = match &status {
        ContainerStatus::Building => "Building container image...".to_string(),
        ContainerStatus::Starting => "Starting container...".to_string(),
        ContainerStatus::Running { container_name } => format!("Container running: {container_name}"),
        ContainerStatus::Exited(code) => format!("Container exited with code {code}"),
        ContainerStatus::Failed(err) => format!("Container failed: {err}"),
        // etc.
    };
    self.event_bus.emit(EventPayload::StatusMessage {
        phase: "container".to_string(),
        message,
    });
}

fn report_progress(&mut self, progress: ContainerProgress) {
    self.event_bus.emit(EventPayload::StatusMessage {
        phase: progress.stage,
        message: progress.message,
    });
}
```

**`WorkflowFrontend` trait impl** (workflow step transitions):
```rust
fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
    let (from_str, to_str) = match &status {
        WorkflowStepStatus::Running => ("pending", "running"),
        WorkflowStepStatus::Succeeded => ("running", "succeeded"),
        WorkflowStepStatus::Failed { .. } => ("running", "failed"),
        WorkflowStepStatus::Cancelled => ("pending", "cancelled"),
        WorkflowStepStatus::Skipped => ("pending", "skipped"),
        _ => return,
    };
    self.event_bus.emit(EventPayload::WorkflowStepTransition {
        step_name: step.name.clone(),
        step_index: step.index,
        from_status: from_str.to_string(),
        to_status: to_str.to_string(),
    });
}

fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
    let (status, exit_code, error) = match outcome {
        // map WorkflowOutcome variants to status/exit_code/error
    };
    self.event_bus.emit(EventPayload::CommandStatus { status, exit_code, error });
}
```

**`report_step_output`** remains a no-op — step output is already captured via `write_stdout`/`write_stderr` on the `ContainerFrontend` during step execution.

**`ApiContainerSink`** (used by Init/Ready engines during session creation) is similarly updated: replace its `log_file` field with a cloned `EventBusSender`.

**Line buffering**: `ApiDispatchFrontend` gains `line_buffer_stdout: String` and `line_buffer_stderr: String` fields to handle partial lines from `write_stdout`/`write_stderr`. Container output arrives as arbitrary byte chunks, not line-aligned. The frontend buffers until `\n` is found, then emits one `StdoutLine`/`StderrLine` event per complete line. Any remaining partial line is flushed as a final event when the command completes (before emitting `Done`).

#### Route Hardening
- The API route dispatcher calls `Dispatch::validate_frontend_command(FrontendKind::Api, cmd, subcmd)` before routing any request. If validation fails, the handler returns HTTP 400 with a JSON body: `{ "error": "command not available via API", "available": ["exec workflow", "exec prompt"] }`.
- This replaces any ad-hoc command string matching that may currently exist in route handlers.
- Route handlers themselves remain thin — they translate HTTP request bodies into `DispatchFrontend` trait implementations and call `Dispatch::run_command(...)`. No business logic lives in route handlers.

#### Remote Client Updates
- `RemoteCommand`'s HTTP client (`remote_client.rs`) is updated to only call the concrete API routes that exist on the server. The client is now a typed client with methods: `start_session(...)`, `kill_session(...)`, `exec_workflow(...)`, `exec_prompt(...)`, `stream_logs(...)`. No generic "send command string" method.

### Layer 0: Data (`src/data/`)
- `ExecutionEvent` and `EventPayload` live in Layer 0 (`src/data/execution_event.rs`). These are pure serializable data types with no runtime behavior — they can be imported by any layer.
- If `RemoteCommand` persists any connection config (host, port, key fingerprint), ensure those types remain in Layer 0 and are named to reflect the concrete subcommand structure.

### Layer 1: Engine (`src/engine/`)
- No changes to the engine layer for EventBus. The engine emits output exclusively through the existing trait pipeline. The always-yolo enforcement is purely a flag resolution concern handled in Layer 2/3.


## Edge Case Considerations

- **Existing API clients**: Any client that currently calls non-exec endpoints (e.g. `GET /sessions`, `POST /sessions`) is unaffected — those session management routes remain. Only exec-related routes that aren't `exec workflow` or `exec prompt` are rejected. Clarify in the API route table which routes are session management (always allowed) vs command execution (restricted).
- **Remote command backward compat**: The old `awman remote <command-string>` invocation will no longer work. The CLI will emit a clear error: "the `remote` command now requires a subcommand. See `awman remote --help`." Do not silently try to parse old invocations.
- **Flag conflict — yolo override**: If an API client passes `yolo: false` in the exec request body, the server silently overrides it to `true`. Document this behavior clearly in the API response: consider including a `"flags_applied": { "yolo": true, "non_interactive": true }` field in exec responses so clients know what was enforced.
- **Remote exec flags**: `awman remote exec workflow` must accept the same flags as local `awman exec workflow` (minus flags that make no sense remotely, like `--workdir` which is set server-side). Ensure the `CommandCatalogue` for `remote exec workflow` declares the correct flag set — do not manually duplicate the flag list; derive it programmatically from the core `exec workflow` command spec, minus remote-excluded flags.
- **Dispatch error propagation**: `Dispatch::validate_frontend_command` returning an error must propagate cleanly to the Layer 3 handler as a typed `DispatchError::NotAvailableForFrontend` variant — not a raw string. The Layer 3 handler converts this to HTTP 400.
- **Ready failure during async session setup**: If `ReadyCommand` fails during session setup, the session remains in `ApiDb` with `setup_status = 'failed'`. The operator can inspect what happened via `GET /sessions/{id}/status` (which includes partial `ready_step_statuses` showing where it failed). The session does NOT accept jobs. For remote sessions, the cloned repo directory is cleaned up on failure. For local sessions, no filesystem cleanup (workdir belongs to the user). To retry, the operator creates a new session.
- **Ready idempotency**: `ReadyEngine::run_to_completion` may be called across multiple session creations within a single server process lifetime. It must be idempotent — if the base image is already built and agents are already configured, it must complete quickly without rebuilding or re-initializing.
- **Session status after server restart**: On startup, sessions with `setup_status = 'initializing'`, `'cloning_repository'`, `'setting_up_branch'`, or `'running_ready'` (i.e. non-terminal states) must be detected. The setup task was killed by the restart. Mark these sessions as `'failed'` with `error: { stage: "server_restart", message: "Server restarted during session setup" }`. Do not attempt to resume setup — the operator creates a new session.
- **Session status endpoint after bus cleanup**: The `SessionSetupBus` is retained in memory during setup and for 60 seconds after reaching a terminal state. After cleanup, the `/status` endpoint falls back to reading `setup_state.json` from the session's directory on disk. If neither exists (very old session or disk error), return a minimal response with just the `setup_status` from `ApiDb`.
- **POST /sessions returns 202 not 201**: This is a deliberate change from the pre-v0.7 API which returned 201 synchronously. Clients that expect 201 for "session ready to use" must be updated to poll `/sessions/{id}/status` until `"ready"`. Document this clearly as a breaking change.
- **Job submission during setup**: `POST /sessions/{id}/jobs` while `setup_status != 'ready'` returns HTTP 409 Conflict with a clear message and the current `setup_status`. This prevents queueing jobs against sessions that may never finish setup.
- **`--wait` with `--type local`**: Local sessions still go through async setup (running `ready` may build Docker images, which can take minutes). `--wait` works identically for both session types — it just skips the clone/branch stages in the progress output.
- **`--follow` with `exec_prompt` jobs**: Prompt jobs have no workflow state. The SSE stream will contain `StdoutLine`, `StderrLine`, `StatusMessage`, `CommandStatus`, and `Done` events — but no `WorkflowStepTransition` or `WorkflowPhaseTransition` events. CLI `--follow` mode renders only the event types that are present. TUI handles the same way — the container output pane shows stdout/stderr, and the workflow strip is simply absent.
- **`--follow` Ctrl-C handling**: If the user hits Ctrl-C during CLI `--follow` streaming, terminate the SSE connection cleanly — print "Follow interrupted. Job is still running server-side. Check status with: awman remote exec workflow --session {id} --job {job_id} status." Do not kill the server-side job.
- **`--follow` across server restart**: If the awman API server restarts while a CLI client is in `--follow` SSE streaming mode, the SSE connection will drop. After a failed reconnect attempt (single retry with 2s delay), the CLI should exit with an error message explaining the server may have restarted.
- **EventBus capacity and slow consumers**: The `broadcast` channel has a capacity of 4096 events. If an SSE client falls behind by more than 4096 events (extremely slow network or paused consumer), `tokio::sync::broadcast` returns `Lagged(n)`. The SSE handler should: (a) log a warning, (b) send a special SSE comment `; lagged: {n} events skipped` to the client, (c) continue streaming from the next available event. The logfile writer task is always a fast consumer (writing to local disk) and should never lag.
- **EventBus lifetime after command completion**: The `EventBus` is retained in `AppState` for 5 seconds after the `Done` event is emitted. This grace period allows SSE clients to receive the final events. After 5 seconds, the `EventBus` is dropped from memory. Late-connecting clients fall back to reading `events.log` from disk.
- **Concurrent SSE clients**: Multiple SSE clients can subscribe to the same `EventBus` simultaneously. Each receives an independent copy of all events. The `broadcast` channel supports this natively.
- **Large output volume**: Container processes can produce megabytes of stdout/stderr (e.g. compilation output, large test suites). The `EventBus` uses `broadcast` which clones each `ExecutionEvent` per subscriber. For the common case of 1–3 subscribers (logfile + 0–2 SSE clients), this is negligible. If profiling later shows memory pressure, consider using `Arc<str>` for `StdoutLine`/`StderrLine` payloads to reduce clone cost. Do not optimize prematurely in this work item.
- **CLI and TUI are unaffected**: The `EventBus` is entirely internal to the API frontend. CLI and TUI frontends continue using their existing trait implementations (`CliUserMessageQueue` → stderr, `TuiUserMessageSink` → shared render buffer). When CLI/TUI consume remote command output (via `--follow`), they act as SSE HTTP clients connecting to the remote server's `/logs` endpoint — no local `EventBus` is involved.
- **`remote session start --type remote --branch` defaults**: If `--branch` is omitted when creating a remote-type session, the session uses the remote repository's default branch. `GitEngine::clone_repo` without an explicit branch should clone the default branch and `checkout_or_create_branch` should not be called in this case (the default branch is already checked out after clone).
- **SSE event replay on late connect**: When an SSE client connects to the `/logs` endpoint for a running job, they receive the full history from `events.log` on disk first, then switch to the live `EventBus` stream. There is a potential for duplicate events during the switchover. The `sequence` field on each event enables the client to deduplicate: skip any event with a `sequence` ≤ the last sequence received from the replay. The SSE handler should implement this server-side to avoid sending duplicates, by noting the last replayed sequence and filtering the broadcast receiver.
- **events.log file rotation**: `events.log` is NOT rotated within a single command execution. For extremely long-running commands that produce millions of events, the file may grow large. This is acceptable — log rotation is an operator concern. The NDJSON format makes it trivially splittable with standard Unix tools.


## Test Considerations

### API Restriction Tests
- **API rejection tests**: For each command that is NOT `exec workflow` or `exec prompt`, send the request via the API frontend test harness and assert HTTP 400 is returned with the expected JSON error body.
- **API acceptance tests**: `exec workflow` and `exec prompt` via the API frontend return non-400 responses (even if the workflow itself fails, the routing must succeed).
- **Always-yolo test**: Submit an `exec prompt` request via the API with `yolo: false` in the payload; assert the executed command ran with yolo enabled (verify via workflow state or container invocation args).
- **Dispatch catalogue unit test**: `CommandCatalogue::api_allowed_commands()` returns exactly `[("exec", "workflow"), ("exec", "prompt")]` and nothing else.
- **FrontendKind validation unit test**: `Dispatch::validate_frontend_command(FrontendKind::Api, "chat", "")` returns `Err(DispatchError::NotAvailableForFrontend)`.
- **`ready` not in API catalogue test**: Assert `CommandCatalogue::api_allowed_commands()` does not include `ready`.

### Remote Subcommand Tests
- **Remote subcommand help test**: `awman remote --help` lists exactly the four subcommands and no others. `awman remote session --help` lists `start` and `kill`. `awman remote exec --help` lists `workflow` and `prompt`.
- **Remote session start flags test**: `awman remote session start --help` lists `--type`, `--workdir`, `--repo-url`, and `--branch`. Invoking with `--type remote` without `--repo-url` returns a validation error.
- **Remote old-style rejection test**: Invoking `awman remote chat` (old arbitrary-command style) returns a clear error message pointing users to `awman remote --help`.
- **Remote exec workflow integration test**: `awman remote exec workflow --workflow foo` sends the correct HTTP request to a mock API server and the response is correctly parsed.

### Async Session Setup Tests
- **POST /sessions returns 202 immediately test**: Create a session via `POST /sessions`; assert HTTP 202 is returned with a `session_id` within 1 second (i.e. does NOT block on setup). Assert the session exists in `ApiDb` with `setup_status = 'initializing'`.
- **Session status transitions test**: Create a remote session; poll `GET /sessions/{id}/status` repeatedly; assert that `status` transitions through `initializing → cloning_repository → setting_up_branch → running_ready → ready` (or a subset for local sessions: `initializing → running_ready → ready`).
- **Session status current_stage test**: During the `running_ready` phase, poll `/sessions/{id}/status`; assert `current_stage` is a human-readable string matching one of the `ReadyPhase` display messages (e.g. "Building base image...") and `current_ready_phase` is the corresponding `ReadyPhase` variant.
- **Session status ready_step_statuses accumulates test**: During setup, poll `/sessions/{id}/status` multiple times. Assert that `ready_step_statuses` grows as steps complete — early polls show fewer entries, later polls show more. Step statuses transition from `Running` to `Done`/`Skipped`.
- **Session status ready terminal test**: After setup completes, poll `/sessions/{id}/status`; assert `status == "ready"`, `ready_summary` is non-null and deserializes to a valid `ReadySummary`, and `error` is null.
- **Session status failed terminal test**: Mock `ReadyEngine` to fail on `BuildingBaseImage`; poll `/sessions/{id}/status`; assert `status == "failed"`, `error` contains the failure stage and message, and `ready_step_statuses` shows partial progress (Dockerfile done, base image failed).
- **Job rejection during setup test**: Create a session, immediately (before setup completes) attempt `POST /sessions/{id}/jobs`; assert HTTP 409 with `setup_status` in the error body.
- **Job acceptance after setup test**: Create a session, poll until `status == "ready"`, then submit a job; assert HTTP 202 (accepted into job queue).
- **Setup failure cleans up remote repo test**: Create a remote session with a mock ReadyEngine that fails; after status shows `failed`, assert the `cloned_path` directory was deleted.
- **Setup failure preserves local workdir test**: Create a local session with a mock ReadyEngine that fails; assert the workdir is untouched.
- **Ready idempotency test**: Call `ReadyEngine::run_to_completion` twice for separate sessions; assert the second call completes without error and without triggering a full rebuild (use a mock or spy on `ContainerRuntime`).
- **Server restart marks in-progress sessions as failed test**: Insert a session with `setup_status = 'running_ready'`; simulate server startup; assert the session is marked `'failed'` with a restart error message.
- **Setup state persisted to disk test**: After setup completes, assert `~/.awman/api/sessions/{sid}/setup_state.json` exists and contains the correct `SessionSetupState`.
- **Status endpoint falls back to disk test**: After setup completes and the `SessionSetupBus` is cleaned up from memory, poll `/sessions/{id}/status`; assert a valid response is returned from the persisted `setup_state.json`.

### `--wait` Flag Tests
- **`--wait` polls and displays progress test**: Run `awman remote session start --wait` against a mock API server that transitions through setup stages; assert stdout/stderr shows stage transition messages at each poll interval.
- **`--wait` displays ready table on success test**: Run `--wait` against a mock that reaches `ready`; assert the output contains the rendered summary box with correct step statuses matching the API's `ready_summary`.
- **`--wait` displays error on failure test**: Run `--wait` against a mock that reaches `failed`; assert the output contains the error message and a partial status table, and the CLI exits with code 1.
- **`--wait` Ctrl-C exits cleanly test**: Send SIGINT during `--wait` polling; assert the CLI exits with a message about polling interruption and does not cancel server-side setup.
- **`--wait` not passed — immediate return test**: Run `awman remote session start` without `--wait`; assert the CLI prints the session_id and returns immediately without polling.
- **TUI always waits test**: In TUI mode, assert that `remote session start` always polls `/sessions/{id}/status` and shows progress in the tab's status area.

### EventBus and Streaming Tests
- **EventBus unit test — emit and receive**: Create an `EventBus`, subscribe, emit 10 events, assert all 10 are received with correct sequence numbers and timestamps.
- **EventBus unit test — multiple subscribers**: Subscribe 3 receivers, emit 5 events, assert all 3 receivers get all 5 events independently.
- **EventBus unit test — lagged receiver**: Create a bus with capacity 4, subscribe, emit 10 events without reading, assert receiver gets `Lagged(6)` then the last 4 events.
- **Logfile writer test**: Create an `EventBus`, start the logfile writer task, emit a mix of `StdoutLine`, `StderrLine`, `StatusMessage`, and `WorkflowStepTransition` events, then `Done`. Assert:
  - `events.log` contains the correct NDJSON (one JSON line per event, all fields present)
  - `output.log` contains only the human-readable text (stdout/stderr lines and status messages, no JSON structure)
- **SSE endpoint — running job test**: Start a mock command execution with an `EventBus`, connect to the `/logs` SSE endpoint, emit events, assert the SSE client receives each event as a properly formatted SSE message with `event:` and `data:` lines.
- **SSE endpoint — completed job test**: Complete a job, then connect to the `/logs` SSE endpoint. Assert the full event history is replayed from `events.log`, followed by a `done` event.
- **SSE endpoint — late connect deduplication test**: Start a job, emit 50 events, connect an SSE client (which replays from disk), continue emitting events. Assert the client receives exactly all events with no duplicates (sequence numbers are contiguous).
- **SSE endpoint — job not found test**: Connect to `/logs` for a nonexistent job_id; assert HTTP 404.
- **HeadlessDispatchFrontend emits events test**: Construct a `HeadlessDispatchFrontend` with an `EventBusSender`, call `write_stdout`, `write_stderr`, `write_message`. Subscribe to the bus and assert the correct `ExecutionEvent` payloads are received.
- **Container output flows through frontend to EventBus test**: Run a container with `ApiDispatchFrontend` backed by an `EventBusSender`. The engine calls `write_stdout`/`write_stderr` on the frontend trait. Assert that `StdoutLine`/`StderrLine` events appear on the bus — confirming that the trait→EventBus pathway works end-to-end.
- **Workflow transitions flow through frontend to EventBus test**: Run a workflow with `ApiDispatchFrontend` backed by an `EventBusSender`. The engine calls `report_step_status` on the `WorkflowFrontend` trait. Assert that `WorkflowStepTransition` events appear on the bus. Similarly, `report_workflow_completed` should produce a `CommandStatus` event.

### Follow Mode Tests
- **`--follow` CLI SSE stream test**: Submit a mock exec workflow job, enable `--follow`; assert the CLI connects to the SSE endpoint, receives events, and prints stdout lines to stdout, status/step transitions to stderr.
- **`--follow` prompt job test**: Submit a mock exec prompt job with `--follow`; assert the CLI receives `StdoutLine`/`StderrLine` events and exits cleanly when `Done` is received. No workflow transition events should be present.
- **`--follow` Ctrl-C test**: Assert that the CLI exits cleanly on interrupt during SSE streaming and prints the "job still running" message.
- **`--follow` server disconnect test**: Drop the SSE connection server-side mid-stream; assert the CLI retries once, then exits with an appropriate error message after the retry fails.
- **TUI always-follow test**: In TUI mode, assert that submitting `remote exec workflow` always opens an SSE connection to the `/logs` endpoint regardless of any flag value.
- **TUI renders remote events identically to local test**: Submit a remote workflow, feed the TUI with SSE events that mirror a local workflow run. Assert the workflow strip and container output pane render identically to a local run.

### Wire Format Tests
- **SSE wire format test**: Assert that each SSE message has the format `event: <type>\ndata: <json>\n\n` where `<type>` matches the `EventPayload` variant name in snake_case.
- **Legacy endpoint backward compatibility test**: Connect to `/v1/commands/{id}/logs/stream` for a running command; assert the legacy `data: <line>\n\n` format with `[amux:done]` sentinel is still produced.
- **NDJSON parsability test**: Write 1000 events to `events.log` via the logfile writer, then read the file and parse each line as JSON. Assert all 1000 parse successfully as `ExecutionEvent`.


## Codebase Integration

- Strictly follow `aspec/architecture/2026-grand-architecture.md`. The canonical command list and `api_allowed` flags live in Layer 2 (`CommandCatalogue`). Validation logic lives in `Dispatch` (Layer 2). The Layer 3 `ApiFrontend` only calls `Dispatch` to validate — it does not implement its own allowlist.
- `ExecutionEvent` and `EventPayload` are Layer 0 types (data definitions, serializable). `EventBus` and `EventBusSender` are Layer 3 types (API frontend internals in `src/frontend/api/event_bus.rs`). They are NOT engine types — the engine layer has no knowledge of them. The engine continues to emit output exclusively through the existing trait pipeline (`UserMessageSink`, `ContainerFrontend`, `WorkflowFrontend`). The API frontend's trait implementations internally fan out to the `EventBus`, which is how logfile writers and SSE handlers receive events.
- The `FrontendKind` enum must live in Layer 2 (not Layer 3), since it is used by `Dispatch` which is a Layer 2 concern.
- The always-yolo enforcement must NOT be implemented as a special-case `if is_api` branch inside `ExecWorkflowCommand` or `ExecPromptCommand` in Layer 2. Instead, the Layer 3 `ApiFrontend`'s trait implementation returns `true` for these flags unconditionally — this keeps the command layer frontend-agnostic.
- `RemoteCommand` in Layer 2 defines the concrete subcommand structure. `RemoteClient` in Layer 2 implements the HTTP calls. Layer 3 CLI frontend just invokes `RemoteCommand` through `Dispatch` like any other command.
- The `EventBus` replaces the direct `log_file: Arc<Mutex<File>>` pattern in `ApiDispatchFrontend`. The frontend MUST NOT write to disk directly — all persistence flows through the `EventBus` subscriber chain. This is critical for ensuring SSE clients and logfiles see identical data.
- The logfile writer task subscribes to the `EventBus` at command start and runs until `Done` is received. It MUST flush and fsync both `events.log` and `output.log` before exiting.
- The legacy `/v1/commands/{id}/logs/stream` endpoint is maintained for backward compatibility but SHOULD be deprecated in documentation. New integrations should use `/v1/sessions/{sid}/jobs/{jid}/logs`.
- CLI and TUI frontends are completely unaffected by the `EventBus`. Their trait implementations (`CliUserMessageQueue`, `TuiUserMessageSink`, etc.) continue to work exactly as they do today. The `EventBus` is a server-side distribution concern only. When CLI/TUI need to consume remote server output (via `--follow`), they act as HTTP clients connecting to the SSE endpoint — they never instantiate an `EventBus` locally.


## Documentation

After implementation:
- `docs/08-api-mode.md` — major updates:
  - Document the restricted command set: only `exec workflow` and `exec prompt` are accepted; include the JSON error response format for rejected commands
  - Add a "Session Setup" section: document async session creation (HTTP 202), the setup pipeline stages, the `GET /sessions/{id}/status` endpoint with its response schema, how to poll for readiness, and the relationship between setup status and job submission
  - Add the `ReadySummary` JSON schema (returned in the status response) and explain how it maps to the CLI/TUI ready status table
  - Note that `--yolo` and `--non-interactive` are always applied by the server
  - **New "Event Streaming" section**: Document the `/logs` SSE endpoint, the `ExecutionEvent` JSON schema, the available `event:` types, and how to build integrations on top of the SSE stream. Include examples of connecting with `curl`, `EventSource` (JavaScript), and `reqwest` (Rust).
  - **New "Log Files" section**: Document the `events.log` (NDJSON) and `output.log` (plain text) files, their locations, and their relationship to the SSE stream.
  - Update the storage layout diagram to show `events.log` alongside `output.log`
- `docs/09-remote-mode.md` — rewrite to document the four concrete `awman remote` subcommands with flag details and usage examples for both local and remote session types; document `--follow` behavior and how it consumes the SSE stream; document `--wait` on `remote session start` with examples of the progress output and the ready status table; remove any reference to arbitrary command passthrough
