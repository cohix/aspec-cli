# Work Item: Feature

Title: Persist container output tails on unexpected non-zero exit
Issue: n/a

## Summary:
- While a workflow is running, keep a rolling buffer of the last ~100 lines of
  combined stdout/stderr for every step container it launches.
- When a step container exits with a non-zero code that awman did **not** cause
  (i.e. awman never killed/cancelled it), flush that buffer to
  `~/.awman/logs/{workflow-id}-{step-name}-{container-name}.log` and print an
  error to the user message sink pointing at the file.

## User Stories

### User Story 1:
As a: user

I want to:
be told where to look when a workflow step's container dies unexpectedly, with a
saved tail of what that container printed just before it failed.

So I can:
debug the failure after the fact without having re-run the workflow with extra
logging, and without the TUI having scrolled the output away.

## Implementation Details:
- Layer 1 (`engine::agent_runtime::output_tail::OutputTail`): a bounded,
  line-oriented ring buffer (default 100 lines) fed raw bytes. Combined
  stdout+stderr because the container I/O bridge funnels both into the same
  reader threads.
- Layer 1 (`engine::container::io_bridge`): the PTY and piped reader threads push
  every byte chunk into the tail, unconditionally (even after the frontend sink
  dies), so the buffer always reflects what the container actually emitted.
- The tail rides on `AgentExecution` so the workflow engine can read it after the
  container's `wait()` resolves.
- Layer 0 (`data::fs::log_dirs::WorkflowLogPaths`): resolves `~/.awman/logs/`,
  builds the per-container log path, and performs the file write. All filesystem
  access stays in Layer 0 per the grand architecture — the engine never touches
  `std::fs` for this.
- Workflow engine: tracks whether awman initiated the kill for each live step
  container. On a genuine non-zero exit it flushes the tail through the Layer 0
  writer and emits an `Error`-level user message with the log path.

## Edge Case Considerations:
- awman-initiated kills (yolo auto-advance, WCB abort/pause/finish, stuck cancel,
  startup-grace kill, abort_on_failure peer kill) must NOT write a log — the exit
  was expected.
- Sandbox-paradigm agents have no container I/O bridge, so they carry no tail and
  never write a container log.
- Filenames sanitise step/container names to filesystem-safe characters.
- A container that streams a huge line with no newline must not grow the buffer
  without bound.

## Test Considerations:
- `OutputTail`: line splitting, capacity eviction, CR trimming, partial-line
  snapshotting, no-newline flushing.
- `WorkflowLogPaths`: path shape, sanitisation, directory creation, round-trip
  write/read.
- Workflow engine: genuine failure writes a log + Error message; awman-killed
  exit does not.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture
  patterns from the project's aspec.

## Documentation
- Update `docs/05-workflows.md` (and operations/troubleshooting docs) to describe
  the `~/.awman/logs/` container failure logs.
