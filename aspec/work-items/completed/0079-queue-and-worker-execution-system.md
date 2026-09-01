# Work Item: Task

Title: The Great Refocusing — Part 3: Queue-and-Worker Execution System
Issue: issuelink

## Summary

The awman API frontend's command execution path is reworked from a synchronous one-command-at-a-time model to an async per-session queue-and-worker system. The **existing HTTP API route structure** is preserved — clients continue to submit commands via `POST /v1/commands`, check status via `GET /v1/commands/{id}/status`, stream logs via `GET /v1/commands/{id}/logs`, and read workflow state via `GET /v1/workflows/{command_id}`. What changes is the execution backend: instead of immediately spawning a tokio task and blocking the session, commands are enqueued into a per-session SQLite-backed FIFO queue and processed by worker tasks. A new `GET /v1/sessions/{id}/queue` endpoint lets clients inspect the queue depth and state for a given session.

Key changes from the current implementation:

1. **Per-session job queues**: Each session has its own independent FIFO queue. When a client submits a command via `POST /v1/commands`, the command is enqueued with `status = 'queued'` and the response returns immediately with the `command_id`. **At most one command per session may be running at any time** — this is enforced at the queue claim level, not by the caller. The current per-session concurrency guard (`busy_sessions`) is removed — the queue replaces it.

2. **Worker tasks**: At server startup, N worker tasks are spawned (configurable via `workers` in global config, default 2). Each worker loops: claim the next queued command from **any** session, execute it, mark complete. Workers use atomic SQLite transactions to claim commands, preventing double-execution.

3. **Workflow file references**: When submitting `exec workflow` via the API, the workflow is specified as a **file path relative to the session's workdir** — exactly as on the CLI or TUI. The workflow file (TOML or YAML) must exist within the workdir. Workflows are never passed as inline JSON to the API.

4. **Existing route semantics are preserved**: `POST /v1/commands` continues to accept `{ "subcommand": "exec workflow", "args": ["my-workflow.toml"] }` (the workflow file path is a positional argument, not a flag). The `GET /v1/commands/{id}/status` response gains a `queue_position` field when the command is queued. The SSE log streaming endpoint moves to `GET /v1/commands/{id}/logs`. The workflow state endpoint is unchanged.

5. **Queue status endpoint**: `GET /v1/sessions/{id}/queue` returns the current queue state for a session — pending commands, the currently running command, and recently completed commands.

Sessions have an explicit **type** that governs working directory provisioning:

- **`local`**: Bound to an existing host directory. The client supplies an absolute `workdir` path.
- **`remote`**: Bound to a remote git repository. `GitEngine` clones the repo into `~/.awman/sessions/{session_id}/repo/` on session creation. The clone directory is deleted on session kill.

For `remote` sessions, no git worktree is created — the cloned repo is already isolated. `ExecWorkflowCommand` in Layer 2 detects the session type and skips worktree creation.

Both session types use the same queue-and-worker system. The WI 0078 infrastructure — async session creation, `SessionSetupBus`, `EventBus`, SSE log streaming, always-yolo enforcement — is fully leveraged and not duplicated.

Before implementing, read and internalize `aspec/architecture/2026-grand-architecture.md` in full. Session types, queue schema, and path helpers live in Layer 0. `GitEngine` clone/checkout/delete methods live in Layer 1. `QueueWorker`, worktree suppression, and `QueueWorkerFrontend` live in Layer 2. Layer 3 only exposes HTTP routes and spawns worker tasks at startup.

## User Stories

### User Story 1:
As a: API client

I want to:
submit exec commands via `POST /v1/commands` and receive a `command_id` immediately, then poll `GET /v1/commands/{command_id}/status` to check whether the command is queued, running, completed, or failed

So I can:
submit multiple long-running workflows without blocking and check results when ready, without holding open an HTTP connection

### User Story 2:
As a: platform operator managing multiple sessions

I want to:
submit commands to different sessions and have them execute concurrently (one per session), while commands within a single session are serialized via the queue

So I can:
run workflows against multiple repos in parallel while ensuring that commands within a given session don't interfere with each other

### User Story 3:
As a: platform operator

I want to:
check `GET /v1/sessions/{id}/queue` to see how many commands are queued, which one is running, and which have completed

So I can:
monitor throughput, diagnose stalls, and plan capacity without parsing log files

### User Story 4:
As a: API client submitting a workflow

I want to:
reference a workflow file by its path within the workdir (e.g. `workflows/deploy.toml` as a positional argument), exactly as I would on the CLI

So I can:
use the same workflow files across CLI, TUI, and API without converting them to JSON or any other format


## Implementation Details

### Layer 0: Data (`src/data/`)

#### SessionType
Add a new enum in `src/data/session.rs`:
```rust
pub enum SessionType {
    Local { workdir: PathBuf },
    Remote { repo_url: String, branch: String, cloned_path: PathBuf },
}
```
- `Session` gains a `session_type: SessionType` field, replacing any prior ad-hoc workdir field. `Session::working_dir()` becomes a method that returns the appropriate path: for `Local`, it returns `workdir`; for `Remote`, it returns `cloned_path`.
- `SessionType` derives `serde::Serialize` and `serde::Deserialize` — persisted as a JSON column in the `sessions` table.
- For `Remote` sessions, `cloned_path` is deterministic: `~/.awman/sessions/{session_id}/repo/`. Computed by Layer 0 path helpers in `api_paths.rs`.
- Add `SessionType::is_remote(&self) -> bool` and `SessionType::cloned_path(&self) -> Option<&Path>` helpers.

#### Queue Schema — Extending the Existing `commands` Table

Rather than introducing a separate `jobs` table, the existing `commands` table in `ApiDb` is extended to support queue semantics. This preserves backward compatibility with the existing `GET /v1/commands/{id}/status` endpoint and avoids a parallel ID namespace.

SQLite schema additions (idempotent `ALTER TABLE ADD COLUMN` migrations, same pattern as the `setup_status` column):

```sql
-- Column: worker_id
-- The UUID of the worker task that claimed this command. NULL while queued.
ALTER TABLE commands ADD COLUMN worker_id TEXT;

-- Column: queued_at
-- Timestamp when the command was enqueued. Set on INSERT.
ALTER TABLE commands ADD COLUMN queued_at TEXT;

-- Column: result
-- JSON: exit code, error message, output summary. Written on completion.
ALTER TABLE commands ADD COLUMN result TEXT;
```

The existing `status` column gains new values (in addition to `'pending'`, `'running'`, `'done'`, `'error'`). The semantics:
- `'queued'` — enqueued, waiting for a worker to claim it. This replaces the old `'pending'` for API-submitted commands. (Non-API commands that were inserted as `'pending'` in legacy code remain readable.)
- `'running'` — claimed by a worker, execution in progress
- `'done'` — completed successfully
- `'error'` — completed with failure
- `'cancelled'` — removed from the queue before execution (e.g. by a session kill request)

New `SqliteSessionStore` methods for queue operations:

- `enqueue_command(id, session_id, subcommand, args, log_path) -> Result<()>` — inserts with `status = 'queued'`, `queued_at = now()`.

- `claim_next_command(worker_id: &str) -> Result<Option<CommandRecord>>` — atomically claims the next queued command, enforcing **at most one running command per session**:
  ```sql
  UPDATE commands
  SET status = 'running', worker_id = ?1, started_at = ?2
  WHERE id = (
      SELECT c.id FROM commands c
      WHERE c.status = 'queued'
        AND NOT EXISTS (
            SELECT 1 FROM commands r
            WHERE r.session_id = c.session_id
              AND r.status = 'running'
        )
      ORDER BY c.queued_at ASC
      LIMIT 1
  )
  RETURNING *
  ```
  The `NOT EXISTS` subquery ensures a session's next queued command is not claimed until its current running command completes. This is the **sole enforcement point** for per-session serial execution — callers do not need their own concurrency guards. Returns the claimed record or `None` if no eligible command exists. Workers compete on this; SQLite serializes the transactions.

- `complete_command(id, status, exit_code, result_json) -> Result<()>` — sets `status` to `'done'` or `'error'`, writes `result` JSON, sets `finished_at`.

- `list_commands_for_session(session_id, limit) -> Result<Vec<CommandRecord>>` — returns all commands for a session ordered by `queued_at`, most recent first. Used by the queue status endpoint.

- `count_queued_for_session(session_id) -> Result<i64>` — counts commands with `status = 'queued'` for the given session.

- `running_command_for_session(session_id) -> Result<Option<CommandRecord>>` — returns the currently running command for the session, if any.

- `cancel_queued_for_session(session_id) -> Result<Vec<String>>` — atomically sets `status = 'cancelled'` and `finished_at = now()` for all commands in the session with `status = 'queued'`. Returns the list of cancelled command IDs. Used by the graceful session kill path.

- `recover_stale_commands(timeout_secs: u64) -> Result<Vec<String>>` — finds commands with `status = 'running'` and `started_at` older than the timeout, resets them to `'queued'` (clearing `worker_id` and `started_at`). Returns the list of recovered command IDs. Called at server startup.

New types in Layer 0:
- `WorkerId` (newtype over UUID, serializable) — identifies a running worker task.
- `CommandResult` struct: `exit_code: Option<i32>`, `error: Option<String>` — serialized as JSON into the `result` column.

The existing `CommandRecord` struct gains the new fields (`worker_id`, `queued_at`, `result`).

#### Remote Session Path Helpers
Add to `api_paths.rs`:
- `fn remote_session_repo_path(session_id: &str) -> PathBuf` → `sessions_dir/{session_id}/repo/`
- `fn remote_session_dir(session_id: &str) -> PathBuf` → `sessions_dir/{session_id}/`

These are pure path functions — no I/O, no side effects.

#### WorkflowState Step Metadata (Layer 0 — coordinate with WI 0080)
WI 0080 extends `WorkflowState` with phase tracking fields. This work item additionally requires that `WorkflowState` be **self-describing** for remote rendering — it must carry enough information for a TUI or CLI client to reconstruct the full step list with dependency topology without separately fetching the `WorkflowDefinition`.

Add to `WorkflowState` in `src/data/workflow_state.rs`:
```rust
pub steps: Vec<WorkflowStepInfo>,
```
where:
```rust
pub struct WorkflowStepInfo {
    pub name: String,
    pub depends_on: Vec<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
}
```
`WorkflowEngine` populates `steps` from the `WorkflowDefinition` when the workflow is first created. This field does not change after initialization. It enables a polling client to render the full topological workflow strip without access to the definition file.

Also add (coordinate with WI 0080's phase step tracking):
```rust
pub setup_step_states: Vec<PhaseStepState>,
pub teardown_step_states: Vec<PhaseStepState>,
```
where:
```rust
pub struct PhaseStepState {
    pub description: String,
    pub status: PhaseStepStatus,  // Pending | Running | Succeeded | Failed { error: String }
}
```
`WorkflowEngine::run_setup` and `run_teardown` update these vecs after each step transitions.

Bump `WORKFLOW_STATE_SCHEMA_VERSION` once for both this and WI 0080's changes.

### Layer 1: Engine (`src/engine/`)

#### GitEngine — Methods for Remote Session Lifecycle
These methods were specified in WI 0078 and should already be implemented:
- `GitEngine::clone_repo(url, branch, into_path) -> Result<()>`
- `GitEngine::checkout_or_create_branch(repo_path, branch) -> Result<BranchDisposition>`
- `GitEngine::delete_directory(path) -> Result<()>`

No new engine methods are needed for the queue system. The queue is a persistence and coordination concern (Layer 0 + Layer 2), not an engine concern.

#### WorkflowEngine — No Changes
No changes to `WorkflowEngine` for session type handling. The worktree suppression decision is made in Layer 2 before `WorkflowEngine` is invoked.

### Layer 2: Command (`src/command/`)

#### ExecWorkflowCommand — Worktree Suppression for Remote Sessions
In `ExecWorkflowCommand::run_with_frontend(session, ...)`, before the worktree creation step, check `session.session_type.is_remote()`. If `true`, skip worktree creation entirely. The workflow runs directly in `session.working_dir()` (the isolated `cloned_path`). Log a debug note: "Skipping worktree creation for remote session — repo is already isolated."

This check lives in `ExecWorkflowCommand` at Layer 2, not in `WorkflowEngine` at Layer 1.

#### QueueWorker
New type: `QueueWorker` in `src/command/queue_worker.rs`

`QueueWorker` holds a reference to the `SqliteSessionStore` (which now includes queue operations), a `WorkerId`, and access to `Engines` and the `AppState` sessions map.

```rust
pub struct QueueWorker {
    worker_id: String,
    store: SqliteSessionStore,  // shared via Arc in practice
    engines: Engines,
    sessions: /* shared handle to AppState.sessions */,
    event_buses: /* shared handle to AppState.event_buses */,
    paths: ApiPaths,
}
```

- `QueueWorker::new(worker_id, store, engines, sessions, event_buses, paths) -> QueueWorker`
- `QueueWorker::run(self) -> !` — async loop:
  1. Call `store.claim_next_command(worker_id)`.
  2. If a command is returned, execute it (see below).
  3. If no command, sleep 250ms and retry.
  4. This loop runs indefinitely as a `tokio::task`.

**Command execution within `QueueWorker::run`**:
1. Look up the in-memory `Session` from the sessions map using `command.session_id`.
2. Compute the command state directory via `paths.command_dir(&command.session_id, &command.id)`. Create this directory on disk.
3. Create an `EventBus` for this command execution. Spawn the logfile writer task (same pattern as current `execute_command`). Store the bus in `event_buses`.
4. Construct an `ApiDispatchFrontend` with the `EventBusSender` — the same frontend type used by the current execution path.
5. Build `Dispatch` with the frontend, session, and engines.
6. Call `dispatch.run_command(&path_parts)`.
7. On completion, call `store.complete_command(id, status, exit_code, result_json)`.
8. Clean up the `EventBus` after a 5-second grace period (same as current behavior).
9. Write metadata.json with the command's final state.

The `QueueWorker` reuses `ApiDispatchFrontend` directly — it does NOT need a separate `QueueWorkerFrontend`. The always-yolo enforcement already lives in `ApiDispatchFrontend`'s trait implementation (per WI 0078), so it applies automatically.

#### Worker Spawn Count
`GlobalConfig` in Layer 0 gains a `workers: Option<u8>` field (defaults to 2 if not set). Layer 3 reads this value at server startup and spawns that many `QueueWorker::run()` tokio tasks.

### Layer 3: Frontend (`src/frontend/api/`)

#### Server Startup
After session restore (existing logic), run stale command recovery (`store.recover_stale_commands()`), then spawn N worker tasks:
```rust
for i in 0..global_config.workers() {
    let worker = QueueWorker::new(
        Uuid::new_v4().to_string(),
        store.clone(),
        engines.clone(),
        sessions.clone(),
        event_buses.clone(),
        paths.clone(),
    );
    tokio::spawn(worker.run());
}
```
Workers are fire-and-forget tasks; the server does not await them.

#### Route Changes

All existing routes are preserved. The changes are to the execution backend, not the HTTP surface.

**`POST /v1/commands` — now enqueues instead of direct execution**

The handler's existing responsibilities are preserved:
1. Extract `session_id` from `x-awman-session` header
2. Validate command is API-allowed via `CommandCatalogue`
3. Validate session exists, is active, and setup is ready (job submission guard from WI 0078)
4. **Changed**: Remove the per-session concurrency guard (`busy_sessions` / `has_running_command_for_session`). The queue handles serialization.
5. Generate `command_id`, create the command directory
6. **Changed**: Call `store.enqueue_command(...)` instead of `store.insert_command(...)`. This sets `status = 'queued'`.
7. **Changed**: Do NOT spawn a tokio task to execute the command. The worker will pick it up.
8. Return `202 Accepted` with the existing `CreateCommandResponse` body: `{ "command_id": "...", "flags_applied": { "yolo": true, "non_interactive": true } }`

The request body is unchanged:
```json
{
  "subcommand": "exec workflow",
  "args": ["workflows/deploy.toml", "--agent", "claude"]
}
```

Workflow files are referenced by path relative to the session's workdir, just as on the CLI. The positional workflow path argument is a file path (TOML or YAML). The workflow file must exist within the session's working directory. `ExecWorkflowCommand` in Layer 2 resolves the path and loads the file — no special handling needed in the API layer.

**`GET /v1/commands/{id}/status` — extended response**

The existing `CommandResponse` struct gains new fields:
```rust
struct CommandResponse {
    // ... existing fields ...
    id: String,
    session_id: String,
    subcommand: String,
    args: serde_json::Value,
    status: String,           // now includes "queued" as a possible value
    exit_code: Option<i32>,
    started_at: Option<String>,
    finished_at: Option<String>,
    log_path: String,
    // New fields:
    #[serde(skip_serializing_if = "Option::is_none")]
    queued_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    queue_position: Option<i64>,  // 0 = next to run, None when running/done
    #[serde(skip_serializing_if = "Option::is_none")]
    worker_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
}
```

When the command has `status = 'queued'`, the handler computes `queue_position` by counting how many commands in the same session are queued with an earlier `queued_at` timestamp.

**`GET /v1/sessions/{id}/queue` — new endpoint (queue status)**

New route added to the router. Returns the current queue state for a session.

Response schema:
```json
{
  "session_id": "abc-123",
  "queue_depth": 3,
  "running": {
    "command_id": "cmd-456",
    "subcommand": "exec workflow",
    "args": ["deploy.toml"],
    "started_at": "2026-05-23T10:00:00Z",
    "worker_id": "worker-789"
  },
  "queued": [
    {
      "command_id": "cmd-457",
      "subcommand": "exec workflow",
      "args": ["test.toml"],
      "queued_at": "2026-05-23T10:01:00Z",
      "position": 0
    },
    {
      "command_id": "cmd-458",
      "subcommand": "exec prompt",
      "args": ["--prompt", "review the code"],
      "queued_at": "2026-05-23T10:02:00Z",
      "position": 1
    }
  ],
  "recent_completed": [
    {
      "command_id": "cmd-455",
      "subcommand": "exec workflow",
      "status": "done",
      "exit_code": 0,
      "finished_at": "2026-05-23T09:59:00Z"
    }
  ]
}
```

Implementation:
- `queue_depth`: count of commands with `status = 'queued'` for this session.
- `running`: the command with `status = 'running'` for this session (at most one), or `null`.
- `queued`: list of commands with `status = 'queued'`, ordered by `queued_at ASC`, with 0-indexed position.
- `recent_completed`: the last 10 commands with `status IN ('done', 'error')`, ordered by `finished_at DESC`. Provides quick visibility into recent history without needing to list all commands.

If the session does not exist, return HTTP 404. If the session has no commands, return the response with `queue_depth: 0`, `running: null`, empty arrays.

**All other routes are unchanged**:
- `POST /v1/sessions` — async session creation (WI 0078), unchanged
- `GET /v1/sessions` — list sessions, unchanged
- `GET /v1/sessions/{id}` — get session, unchanged
- `DELETE /v1/sessions/{id}` — graceful session kill (see dedicated section below). Cancels all queued commands immediately, waits for any running command to finish, then closes the session.
- `GET /v1/sessions/{id}/status` — setup status (WI 0078), unchanged
- `GET /v1/commands/{id}/status` — extended (see above)
- `GET /v1/commands/{id}/logs` — SSE streaming (WI 0078 event bus), moved from the old `/v1/sessions/{id}/jobs/{job_id}/logs` path. The command ID is the only identifier needed — the session is looked up from the command record.
- `GET /v1/workflows/{command_id}` — workflow state, unchanged
- `GET /v1/status` — server status, unchanged
- `GET /v1/workdirs` — workdir list, unchanged

#### TUI Workflow Strip for Remote Sessions
When the TUI submits a `remote exec workflow` command, it displays the workflow strip and updates it in real time from the API. Follow mode is always active in the TUI.

**RemoteWorkflowPoller** (Layer 3 TUI):
```rust
struct RemoteWorkflowPoller {
    client: Arc<RemoteClient>,
    session_id: String,
    command_id: String,
    workflow_view: Arc<Mutex<Option<WorkflowViewState>>>,
}
```
- `RemoteWorkflowPoller::start(self) -> JoinHandle<()>` — spawns a `tokio::task` that:
  1. Every 500ms: call `client.get_command_status(command_id)` for overall status
  2. Every 500ms (same tick): call `client.get_workflow_state(command_id)`
  3. On a new `WorkflowState` response: convert to `WorkflowViewState` via `workflow_state_to_view_state(&state)`
  4. Lock `workflow_view`, replace, release
  5. The TUI render loop picks up the updated `WorkflowViewState` on the next tick
  6. When command status is `done` or `error`: do one final state poll, update the view, stop polling

The `workflow_view` Arc is the **same one already used by the tab for local workflow rendering** — no special remote strip path; the strip renders identically.

**`workflow_state_to_view_state` conversion function** (Layer 3 TUI):
- Input: `&WorkflowState`
- Output: `WorkflowViewState` (TUI type containing `Vec<WorkflowStepView>`)
- Conversion:
  1. Prepend pseudo-steps from `state.setup_step_states`
  2. Map main steps from `state.steps` + `state.step_states`
  3. Append pseudo-steps from `state.teardown_step_states`
  4. Return `WorkflowViewState { steps, current_step: state.current_step_index }`

**CLI `--follow` step output** (Layer 2 `RemoteCommand`):
- Track last-seen step states
- On each poll that returns a changed `WorkflowState`, print status changes:
  ```
  [setup]      clone_repo: https://github.com/org/repo   → succeeded
  [step 1]     analyze                                    → running
  [step 2]     implement                                  → pending
  [teardown]   commit_changes                             → pending
  ```

**TUI strip for prompt commands**: When a `remote exec prompt` command is submitted, do NOT start `RemoteWorkflowPoller` — there is no workflow state. Show a simple "running" indicator.

#### `DELETE /v1/sessions/{id}` — Graceful Drain-and-Kill

Session kill is a graceful operation: queued commands are cancelled immediately, but any currently running command is allowed to finish before the session is closed. This avoids aborting mid-execution while still preventing new work from starting.

**Behavior:**
1. Validate the session exists and is active. Return 404 if not found, 200 if already closed.
2. Call `store.cancel_queued_for_session(session_id)` — atomically sets all queued commands to `'cancelled'`. This prevents workers from claiming any new work for this session.
3. Mark the session's status as `'closing'` in `ApiDb` (new status value). A `'closing'` session rejects new command submissions (`POST /v1/commands` returns HTTP 409).
4. Check if the session has a command with `status = 'running'`:
   - **No running command**: proceed to step 5 immediately.
   - **Running command exists**: return HTTP 202 Accepted with:
     ```json
     {
       "session_id": "abc-123",
       "status": "closing",
       "running_command_id": "cmd-456",
       "cancelled_count": 2,
       "message": "Session is closing. Waiting for running command to complete. Poll GET /v1/sessions/{id}/status to monitor."
     }
     ```
     The session remains in `'closing'` state. When the running command finishes (detected by the worker's post-execution path), the worker checks if the session is `'closing'` and triggers final cleanup (step 5).
5. **Final cleanup** (runs either inline or deferred after the running command completes):
   a. If `session.session_type` is `Remote`, call `git_engine.delete_directory(cloned_path)`.
   b. Mark the session as `'closed'` in `ApiDb`, set `closed_at`.
   c. Remove from in-memory `sessions` map.
   d. Return HTTP 200 (if inline) with the final session state.

**Worker integration**: After a worker completes a command, it checks whether the session's status is `'closing'`. If so, the worker triggers step 5 (final cleanup) instead of claiming the next command for that session. This is a simple check added to the worker's post-execution path — no separate background task is needed.

**`GET /v1/sessions/{id}/status` during closing**: The existing status endpoint already returns the session's current state. When a session is in `'closing'` status, the response reflects this, allowing clients to poll until the session reaches `'closed'`.

**`POST /v1/commands` guard**: The handler checks the session's status before enqueuing. If the session status is `'closing'` or `'closed'`, return HTTP 409 Conflict:
```json
{
  "error": "session is closing",
  "session_id": "abc-123",
  "hint": "Session is shutting down and no longer accepts commands."
}
```


## Edge Case Considerations

- **Queue ordering**: Commands within a session are processed in strict FIFO order by `queued_at`. If two commands are enqueued at the same millisecond, SQLite's `ORDER BY queued_at, rowid` breaks the tie deterministically.
- **Worker starvation**: With N workers and M active sessions, if one session queues many commands, workers may spend most of their time on that session. This is acceptable for single-node — workers claim globally, so all sessions are served fairly (next-in-queue regardless of session). If session-level fairness is needed later, `claim_next_command` can be changed to round-robin across sessions, but this is out of scope.
- **Atomic claim**: `claim_next_command` uses `UPDATE ... WHERE id = (SELECT ... LIMIT 1)` in a single statement. SQLite's write serialization ensures exactly one worker claims each command. No explicit transaction wrapper is needed beyond SQLite's implicit one.
- **Clone failure**: If `GitEngine::clone_repo` fails (network error, invalid URL), session creation fails (per WI 0078). No session record is created. The Layer 3 handler returns the error.
- **Branch checkout failure**: If `checkout_or_create_branch` fails after clone, the cloned directory is cleaned up (per WI 0078). No session created.
- **Session kill with queued commands**: `DELETE /sessions/{id}` immediately cancels all queued commands and transitions the session to `'closing'`. If a command is running, the session remains in `'closing'` until it completes, then the worker triggers final cleanup. Clients poll `GET /sessions/{id}/status` to observe the transition to `'closed'`.
- **Session kill with no in-flight commands**: `DELETE /sessions/{id}` transitions directly to `'closed'` and returns HTTP 200 synchronously.
- **Remote session kill — partial cleanup**: If `delete_directory` fails during final cleanup, log the error and return HTTP 500. The session is NOT marked as closed — the operator must resolve the filesystem issue and retry.
- **Double delete**: If `DELETE /sessions/{id}` is called while the session is already `'closing'`, return HTTP 200 with the current state (including `running_command_id` if still draining). If already `'closed'`, return HTTP 200 with the closed session record.
- **`local` session with non-existent workdir**: Return HTTP 400 at session creation. This check happens during async setup (per WI 0078).
- **Session restore on server restart**: On startup, sessions in `active` status are restored from `ApiDb`. For `remote` sessions, verify `cloned_path` exists. If missing, mark as `closed`. Commands with `status = 'running'` are recovered to `'queued'` via `recover_stale_commands()`.
- **Single active command per session**: This is enforced by the `NOT EXISTS` subquery in `claim_next_command`. Workers naturally skip sessions that have a running command and pick work from other sessions instead. With N workers and M sessions with queued work, up to min(N, M) commands run concurrently — one per session. This is not a performance optimization but a correctness requirement: workflows hold locks on the workdir and containers, so concurrent execution within a session would fail.
- **Worker efficiency**: Because `claim_next_command` skips busy sessions, workers never waste cycles attempting and failing a claim. If all sessions with queued work already have a running command, `claim_next_command` returns `None` and the worker sleeps.
- **Worktree suppression — audit**: All existing callers in `ExecWorkflowCommand` that create git worktrees must check `session.session_type.is_remote()`. No code path should create a worktree for a remote session.
- **API vs CLI/TUI sessions**: The queue system is API-only. CLI and TUI create `Session` objects directly without the queue. `SessionType` lives in Layer 0 but CLI/TUI sessions are always `Local`.
- **Workflow file resolution**: When `args` contains `["deploy.toml"]` (positional argument), `ExecWorkflowCommand` resolves `deploy.toml` relative to the session's `working_dir()`. For local sessions this is the user's repo; for remote sessions it's the `cloned_path`. The file must exist and be TOML or YAML. Markdown workflows are rejected per WI 0080.
- **Workflow state file timing**: `WorkflowEngine` writes state after each step transition. The first write happens when the first step enters `Running`. A client polling `GET /v1/workflows/{command_id}` immediately after submission may receive HTTP 404 — this is expected. Clients must tolerate 404 while the command is queued or during initial execution lag.
- **Workflow state for completed commands**: After workflow completion, the state file persists in the command directory. It is NOT deleted on session kill (only the remote session's `repo/` directory is deleted).
- **`status` field backward compatibility**: Legacy commands inserted with `status = 'pending'` (by pre-queue code) are treated as `'queued'` by workers. The `claim_next_command` query matches `status IN ('queued', 'pending')`.
- **Worker count zero**: If `workers: 0` is configured, no workers spawn. Commands will be enqueued but never processed. Emit a startup warning.
- **EventBus per command**: Each command execution creates its own `EventBus` (unchanged from current behavior). The `EventBus` is created by the worker when it starts executing the command, not at enqueue time. SSE clients connecting while the command is queued will get HTTP 404 from the log endpoint (no events yet).
- **Per-command `Engines` customization**: For workflow commands, `QueueWorker` may need to adjust the `WorkflowStateStore` path to point to the command's directory. This is done by creating a per-command copy of `Engines` with the store replaced, same pattern as the current `execute_command` function.


## Test Considerations

- **Enqueue and claim test**: Insert 3 commands via `enqueue_command`; call `claim_next_command` 3 times; assert each returns a different command in FIFO order; 4th call returns `None`.
- **Atomic claim test**: Spawn 4 workers claiming from a queue of 4 commands simultaneously; assert each command is claimed by exactly one worker (no duplicates).
- **Session-exclusive execution test**: Enqueue 2 commands for the same session; spawn 2 workers; assert only one command enters `'running'` at a time — the second worker gets `None` from `claim_next_command` and does not execute concurrently.
- **Cross-session concurrency test**: Enqueue 1 command each for 2 different sessions; assert both can be claimed and run concurrently by different workers.
- **Queue position test**: Enqueue 3 commands for a session; call `GET /v1/commands/{id}/status` for each; assert `queue_position` is 0, 1, 2 respectively.
- **Queue status endpoint test**: Enqueue 3 commands, let 1 run and 1 complete; call `GET /v1/sessions/{id}/queue`; assert `queue_depth` is 1, `running` is the active command, `queued` has 1 entry, `recent_completed` has 1 entry.
- **Stale command recovery test**: Insert a command with `status = 'running'` and `started_at` older than timeout; call `recover_stale_commands()`; assert status reset to `'queued'`.
- **Worker count config test**: Set `workers: 0`; assert no workers spawned and startup emits a warning.
- **POST /v1/commands enqueues test**: Submit a command via `POST /v1/commands`; assert the DB has it with `status = 'queued'`, NOT `'pending'` or `'running'`.
- **POST /v1/commands no longer blocks session test**: Submit 2 commands to the same session in quick succession; assert both return 202 (not 403). The second is queued behind the first.
- **DELETE /sessions/{id} with queued commands test**: Enqueue 3 commands, attempt `DELETE /sessions/{id}`; assert all 3 are cancelled, session status is `'closed'` (no running command to wait for), HTTP 200.
- **DELETE /sessions/{id} with running command test**: Enqueue 2 commands, let 1 start running, call `DELETE`; assert queued command is cancelled, session status is `'closing'`, HTTP 202 with `running_command_id`. When the running command completes, assert session transitions to `'closed'`.
- **DELETE /sessions/{id} with empty queue test**: Close a session with no commands; assert immediate `'closed'`, HTTP 200.
- **POST /v1/commands rejected on closing session test**: Set session to `'closing'`, attempt `POST /v1/commands`; assert HTTP 409.
- **Double delete test**: Call `DELETE` on an already-closing session; assert HTTP 200 with current state, no error.
- **Workflow file reference test**: Submit `POST /v1/commands` with `args: ["test.toml"]` (positional argument); assert the worker resolves `test.toml` relative to the session workdir and loads the workflow file.
- **Workflow file not found test**: Submit with a nonexistent workflow file path; assert the command fails with a clear error in the `result` field.
- **Remote session worktree suppression test**: Execute a workflow against a remote session; assert `GitEngine::create_worktree` is never called.
- **Local session creation test**: `POST /sessions` with `type: local, workdir: /tmp/test-repo`; assert session created.
- **Remote session creation test**: `POST /sessions` with `type: remote, repo_url: ..., branch: main`; assert `cloned_path` exists.
- **Remote session kill cleanup test**: Kill a remote session; assert `cloned_path` deleted.
- **Server restart recovery test**: Insert a session with a running command; simulate restart; assert command recovered to `'queued'`.
- **GET /v1/commands/{id}/status response shape test**: Verify the response includes `queued_at`, `queue_position`, `worker_id`, and `result` fields with correct types.
- **SSE log streaming still works test**: Enqueue a command, let worker execute it, connect to SSE endpoint; assert events stream correctly.
- **Backward compat — legacy pending commands test**: Insert a command with `status = 'pending'` (old code path); assert `claim_next_command` picks it up.
- **Workflow state endpoint unchanged test**: After workflow execution, `GET /v1/workflows/{command_id}` returns the correct state JSON.
- **Queue depth under load test**: Enqueue 100 commands across 10 sessions; assert queue status is accurate and workers drain all commands without deadlock or duplicate execution.
- **`workflow_state_to_view_state` unit test**: Construct a `WorkflowState` with setup/main/teardown steps; assert correct conversion to `WorkflowViewState`.
- **TUI strip poller integration test**: Submit a remote workflow via mock TUI; assert `RemoteWorkflowPoller` polls and updates the view.


## Codebase Integration

- Strictly follow `aspec/architecture/2026-grand-architecture.md`. Queue schema and types (`CommandRecord` extensions, `WorkerId`, `CommandResult`) live in Layer 0. `GitEngine` methods live in Layer 1. `QueueWorker` lives in Layer 2. Layer 3 only exposes HTTP routes and spawns workers.
- `SessionType` is a Layer 0 type. Worktree suppression based on `SessionType::is_remote()` is decided in Layer 2 (`ExecWorkflowCommand`). `WorkflowEngine` in Layer 1 must NOT be aware of session types.
- `QueueWorker` reuses `ApiDispatchFrontend` for its frontend — the same type used by the current `execute_command` function. No separate `QueueWorkerFrontend` is needed. Always-yolo enforcement comes from `ApiDispatchFrontend` automatically.
- The `EventBus` lifecycle is unchanged: created per command execution by the worker, retained for 5 seconds after completion, then dropped. SSE clients connect to the `/v1/commands/{id}/logs` endpoint.
- The existing `busy_sessions` mutex in `AppState` is removed. Per-session serial execution is enforced by `claim_next_command`'s `NOT EXISTS` subquery — this is the single enforcement point.
- After completing a command, the worker checks if the session's status is `'closing'`. If so, the worker runs the session's final cleanup (remote dir deletion, status → `'closed'`) instead of returning to the claim loop. This keeps the drain-and-kill logic in the worker's post-execution path, not in a separate background task.
- New SQLite columns must be added as idempotent `ALTER TABLE ADD COLUMN` migrations, consistent with the existing pattern in `SqliteSessionStore::migrate`.
- `workers` config field lives in `GlobalConfig` (Layer 0). Spawning worker tasks is a Layer 3 server startup concern.
- The `POST /v1/commands` handler no longer spawns execution tasks. It only validates, enqueues, and returns. Execution is the worker's responsibility.
- Route table tests must be updated: add `GET /v1/sessions/:id/queue`, replace `GET /v1/commands/:id` with `GET /v1/commands/:id/status`, replace `GET /v1/sessions/:id/jobs/:job_id/logs` with `GET /v1/commands/:id/logs`.


## Documentation

After implementation:
- `docs/08-api-mode.md` — add a "Job Queue" section: explain that commands are enqueued and processed by workers; document the `GET /v1/sessions/{id}/queue` endpoint; document the `queue_position` field in command responses; document the `workers` config option; explain that workflows are specified as file paths in `args`, not inline JSON
- `docs/07-configuration.md` — document the `workers` global config option
- Update `docs/08-api-mode.md` session lifecycle section to explain `local` vs `remote` session types (if not already covered by WI 0078 docs)
- Update the API endpoint reference table to include `GET /v1/sessions/{id}/queue`
