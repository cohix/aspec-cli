# Work Item: Feature

Title: True Parallel Agents
Issue: issuelink

## Summary

Add proper parallel agent execution to awman workflows. Currently, `WorkflowEngine` runs every ready step sequentially even when the DAG allows parallelism. This work item introduces a `maxConcurrentAgents` config knob, a parallel execution loop in the engine, multi-container UX for the TUI and CLI, per-container stuck/yolo tracking, Workflow Control Board scoping for parallel contexts, and a reworked workflow state strip that reflects actual concurrency.

The change is engine-driven throughout — the engine owns all scheduling and concurrency state; frontends only present what the engine reports.

## User Stories

### User Story 1
As a: user

I want to: run multiple workflow steps in parallel when they are independent in the DAG

So I can: complete large workflows faster without manually wiring sequential pipelines or splitting workflows.

### User Story 2
As a: user

I want to: configure how many agents can run at once per workflow via `maxConcurrentAgents` in my repo or global config (overridable with `--max-concurrent` or `MAX_CONCURRENT_AGENTS`)

So I can: tune parallelism to match my machine and Docker resource budget without editing the workflow file.

### User Story 3
As a: user

I want to: switch between parallel running containers in the TUI with Ctrl-S, and see per-container stuck/yolo status at a glance on the minimized status bars at the bottom of the active tab

So I can: monitor and interact with each parallel agent independently without losing context on the others.

## Implementation Details

### 1. Config — `maxConcurrentAgents`

**Files:** `src/data/config/repo.rs`, `src/data/config/global.rs`, `src/data/config/flags.rs`, `src/data/config/env.rs`, `src/data/config/effective.rs`

Add `max_concurrent_agents: Option<usize>` to `RepoConfig` and `GlobalConfig`, serialized as `"maxConcurrentAgents"` in both JSON files. Validate `>= 1` in `RepoConfig::load` (and the equivalent for `GlobalConfig`), returning `DataError::Other` on violation.

Add `max_concurrent_agents: Option<usize>` to `FlagConfig` (populated from `--max-concurrent <N>`). Add `AWMAN_MAX_CONCURRENT_AGENTS` to `env.rs` constants and `EnvSnapshot`.

Add `effective_max_concurrent_agents() -> Option<usize>` to `EffectiveConfig` following the standard precedence ladder:
```
flags.max_concurrent_agents
  → env.max_concurrent_agents() (parse from AWMAN_MAX_CONCURRENT_AGENTS)
    → repo.max_concurrent_agents
      → global.max_concurrent_agents
        → None  (unlimited)
```

`None` means unlimited — any number of ready steps may launch simultaneously. Resolution happens once per workflow at `WorkflowEngine` construction time and is stored as `max_concurrent: Option<usize>` on the engine struct.

### 2. Engine — Parallel Execution Loop

**File:** `src/engine/workflow/mod.rs`

`WorkflowEngine` currently tracks a single running step via `current_execution: Option<AgentExecution>`. Replace this with:

```rust
active_steps: Vec<ActiveParallelStep>,
```

where `ActiveParallelStep` holds the step name, `AgentExecution`, stuck-channel sender, and yolo timer state for one running container.

Rewrite `run_to_completion` around a new inner method `run_parallel_group`. The outer loop:
1. Calls `dag.ready_steps(&completed)` to find all steps whose dependencies are satisfied.
2. Determines the current *parallel group*: steps that are all blocked only on the same set of already-running-or-just-completed steps. In the simple case this is just the full `ready_steps` result — the engine launches up to `max_concurrent` of them (in source-file order), queuing the rest.
3. Drives `run_parallel_group` until all steps in the group finish.
4. Loops back to find the next batch.

`run_parallel_group` runs an `async` select loop that polls all active `AgentExecution` streams concurrently (e.g. `tokio::select!` with a dynamically-sized set of futures or `futures::stream::FuturesUnordered`). When one completes:
- Record its outcome (`Succeeded`, `Failed`, `Cancelled`).
- If there are queued steps for this group and `active_steps.len() < max_concurrent`, launch the next queued step.
- Notify the frontend via `report_workflow_progress`.
- When all group members finish, return to the outer loop.

Engine rules during a parallel group:
- **Stuck detection** — each `ActiveParallelStep` gets its own `stuck_sender` broadcast channel. The engine starts a per-step stuck-timer task for each. If any step's timer fires, the engine sends `StuckEvent::Stuck` on that step's channel and marks the step stuck in internal state.
- **Yolo** — each step has its own yolo countdown; the engine drives them independently. When a yolo countdown expires for one step, the engine kills only that container and advances the queue for that group slot (or notes the group is draining if the queue is empty).
- **WCB** — when the frontend raises `EngineRequest::OpenControlBoard`, the engine pauses only the *currently-focused* step's interaction (the one the user has maximized); other parallel steps continue executing. The engine passes a new `focused_step_name` field on `WorkflowControlBoardState` so the WCB knows which container its actions apply to.
- **abort_on_failure** — if a step with `abort_on_failure: true` fails, the engine kills all other active steps and cancels queued ones, then proceeds with existing abort logic.
- **Non-yolo stuck** — when a step is stuck and yolo is off, the engine marks it stuck but does not block other parallel steps. It continues running the rest of the group. The stuck step's slot stays occupied (no new step is launched into its place) until the user manually sends Ctrl-C to that container through the TUI, which kills it and frees the slot.

Keep the engine's `EngineRequest` channel but extend it with a `focused_step` identifier so the TUI can route Ctrl-W, StepStuck, StepUnstuck events to the correct step.

```rust
pub enum EngineRequest {
    OpenControlBoard { step_name: String },
    StepStuck    { step_name: String },
    StepUnstuck  { step_name: String },
}
```

### 3. `WorkflowFrontend` trait extensions

**File:** `src/engine/workflow/frontend.rs`

Add new trait methods (all have default no-op implementations):

```rust
/// Engine is launching multiple parallel containers for this group.
/// `step_names` is the ordered list of all steps in this parallel batch
/// (including queued ones that are not yet running).
fn report_parallel_group_started(&mut self, _step_names: &[String]) {}

/// One container in a parallel group has started running.
fn report_parallel_step_launched(&mut self, _step_name: &str, _agent: &str, _model: Option<&str>) {}

/// One container in a parallel group has exited.
/// `evict` — the frontend should remove the status bar for this step
/// entirely (not replace it with a grey summary bar).
fn report_parallel_step_exited(&mut self, _step_name: &str, _exit_code: i32) {}

/// A queued step in this parallel group has started (because a slot freed up).
fn report_parallel_step_dequeued(&mut self, _step_name: &str, _agent: &str, _model: Option<&str>) {}

/// The parallel group has fully drained; all steps completed.
fn report_parallel_group_finished(&mut self) {}

/// Per-step stuck notification for a parallel container.
fn report_parallel_step_stuck(&mut self, _step_name: &str) {}
fn report_parallel_step_unstuck(&mut self, _step_name: &str) {}

/// Per-step yolo countdown updates.
fn parallel_step_yolo_countdown_started(&mut self, _step_name: &str) {}
fn parallel_step_yolo_countdown_tick(&mut self, _step_name: &str, _remaining: Duration, _total: Duration) -> Result<YoloTickOutcome, EngineError> { Ok(YoloTickOutcome::Continue) }
fn parallel_step_yolo_countdown_finished(&mut self, _step_name: &str) {}

/// Set per-step I/O channels. Called once per parallel step launch.
fn set_parallel_step_io(&mut self, _step_name: &str, _io: AgentIo) {}

/// Set per-step stuck sender (one per active parallel container).
fn set_parallel_step_stuck_sender(&mut self, _step_name: &str, _sender: Arc<broadcast::Sender<StuckEvent>>) {}
```

The single-step path (`set_stuck_sender`, `report_container_exited`, etc.) continues to work unchanged for workflows where `max_concurrent` is 1 or where only one step is ever ready at a time.

### 4. TUI — Unified Container Slot State

**File:** `src/frontend/tui/tabs.rs`

There are NO single-container fields on `Tab`. `ContainerSlot` is the only
container representation — a plain containerized command (`chat`,
`exec prompt`) is a one-slot group, and a parallel workflow group is N slots:

```rust
pub struct ContainerSlot {
    pub step_name: String,   // empty for plain commands / sequential steps
    pub vt100_parser: vt100::Parser,
    pub region_scroll: RegionScrollEmulator,
    pub container_info: Option<ContainerInfo>,
    pub container_stdout_rx: Option<mpsc::UnboundedReceiver<Vec<u8>>>,
    pub container_stdin_tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
    pub container_resize_tx: Option<mpsc::UnboundedSender<(u16, u16)>>,
    pub agent_alt_screen: bool,        // per-slot terminal-mode tracking
    pub agent_alternate_scroll: bool,
    pub stuck: bool,
    pub yolo_mode: bool,
    ...
}
```

`Tab` holds:
```rust
pub container_slots: Vec<ContainerSlot>,   // 0 while idle, 1 for plain commands, N in a group
pub focused_slot_idx: usize,               // which slot renders maximized
pub dormant_slots: Vec<ContainerSlot>,     // sequential backbone, stashed during a group
```

- `spawn_command` installs the command's single slot (via `Tab::start_container`)
  with the spawn-time I/O channels; sequential workflow steps reuse it (the
  persistent stdout channel, `pty_reset_flag` parser reset, and shared
  stdin/resize sender swaps all target this "backbone" slot).
- A parallel group publishes `ContainerSlotEvent::GroupStarted`, which stashes
  the backbone into `dormant_slots` (its stdout receiver must stay alive for
  post-group sequential steps); `Launched`/`Exited` events add/evict per-step
  slots; `GroupFinished` restores the backbone.
- Ctrl-S advances `focused_slot_idx` cyclically among active slots.
- All input routing (keyboard, mouse), rendering, stats polling, and resize
  handling go through `container_slots` — there is no legacy fallback path.
- Stuck/yolo tab coloring: while a group is active (`dormant_slots` non-empty)
  the tab aggregates the slots' flags; otherwise the engine's broadcast channel
  and the spawn-time yolo flag drive them, as for plain commands.

### 5. TUI — Container Window Rendering

**File:** `src/frontend/tui/render.rs`

`ContainerWindowState` selects the display shape, uniformly for 1..N slots:

- **Maximized**: the focused slot renders as the centered overlay; every other
  slot renders as a 3-row rounded-border status bar stacked above the status
  bar. Bar content: `🔒 agent [step_name] | container | cpu | mem | duration`
  (the `[step_name]` segment is omitted when empty), using the slot's own
  per-container stats. The focused slot has **no** bar; its step name,
  container name, and live stats appear in the maximized window's title line
  (`🔒 agent (runtime) — step_name` on the left, stats on the right; sequential
  steps take the step name from the workflow view's current step).
- **Minimized**: every slot renders as a status bar; no overlay. With one slot
  this is exactly the classic single-container minimized bar.
- **Hidden** (Ctrl-M off-state / post-exit): nothing; the grey post-exit
  summary bar occupies the same layout slot when present.
- The maximized container window shrinks to `area.height - 3·N_bars - workflow_height`.
- Stuck slots' bars get a yellow border/text color and a `⚠` prefix;
  yolo-countdown bars flash purple/yellow on the same per-second parity the
  tab header uses.
- Only the focused slot receives keyboard input. Mouse scroll (protocol
  forwarding, alternate-scroll translation, and awman-side scrollback) likewise
  routes to the focused slot's parser/stdin, with alt-screen/alternate-scroll
  modes tracked per slot.
- Each slot's vt100 parser and container PTY are kept in lockstep with the
  maximized overlay's actual inner rect: sized on launch (never left at the
  80x24 default) and re-synced every tick, so background containers are
  already correctly laid out when Ctrl-S rotates them in.
- Per-slot container stats: the engine publishes each parallel step's
  container name via `report_parallel_step_container` right after launch (a
  plain command's name arrives via the shared container-name slot); the TUI
  polls stats per container (Docker and Apple runtimes both accept per-name
  queries) and routes each sample to its slot by step name.

### 6. TUI — Ctrl-S Keybinding

**File:** `src/frontend/tui/mod.rs` (or wherever key events are dispatched)

Add `Ctrl-S` to the keybinding table. When pressed and `container_slots.len() > 1`:
- Advance `focused_slot_idx = (focused_slot_idx + 1) % active_slot_count`.
- Reset the container scrollback offset so the rotated-in slot starts at its live view. No manual resize is needed — the per-tick size sync (§5) keeps every slot's parser and PTY matched to the overlay already.
- If `ContainerWindowState` is Minimized, switch to Maximized so the rotated container becomes visible.

When `container_slots.len() <= 1`, Ctrl-S is passed through to the container's PTY (some programs use it for flow control).

### 7. CLI — Lightweight Parallel TUI

**File:** `src/frontend/cli/` (or `src/command/commands/exec_workflow.rs` for the CLI workflow frontend)

When running in an interactive terminal (detect via `std::io::IsTerminal` on stdout) and `active_steps.len() > 1`, the CLI frontend activates a minimal VT100 display:

- Reserve two rows at the bottom of the terminal using ANSI cursor addressing.
- Bottom row: `[N agents running | showing: <step_name> | Ctrl-S: switch]`
- The remaining terminal height above this status row shows the PTY output of the focused agent (raw passthrough or a vt100 window), resizing as the focused agent changes.
- Ctrl-S cycles the focused agent, exactly as in the TUI.

When `active_steps.len() == 1` or when stdout is not a TTY, the CLI reverts to the existing sequential passthrough output (no VT100 chrome added).

This mini-TUI is implemented as a new `CliParallelFrontend` struct that implements `WorkflowFrontend`. It uses `crossterm` (already a transitive dep) for terminal control rather than Ratatui, keeping it deliberately lightweight.

### 8. API Frontend

**File:** `src/command/commands/api_server.rs` (API workflow frontend impl)

The API frontend is non-interactive. Extend `report_parallel_step_launched`, `report_parallel_step_exited`, and `report_parallel_group_finished` to emit structured SSE/JSON events so API consumers can track parallel progress. No visual changes needed. Follow the existing event-emission pattern in the API frontend.

### 9. Tab State — Stuck and Yolo Indicator Aggregation

**File:** `src/frontend/tui/tabs.rs` (in `tick_all_tabs` / stuck drain logic)

When draining stuck events for tab coloring, aggregate across all `parallel_slots`:
- `tab.stuck = parallel_slots.iter().any(|s| s.stuck)`
- `tab.yolo_mode = parallel_slots.iter().any(|s| s.yolo_mode)`

Tab header color (`tab_color` in `tabs.rs`) reads `tab.stuck` and `tab.yolo_mode` as before; no change to that function.

### 10. Workflow Control Board Scoping

**File:** `src/engine/workflow/mod.rs`, `src/engine/workflow/actions.rs`, `src/frontend/tui/per_command/workflow_frontend.rs`

Extend `WorkflowControlBoardState` with:
```rust
pub focused_step_name: String,           // the step the WCB actions apply to
pub parallel_peer_count: usize,          // 0 = not in parallel group
pub parallel_peers_running: usize,       // live peers (excludes focused)
```

Extend `AvailableActions` with per-action reason strings that reflect the parallel context:
- `RestartCurrentStep`: valid only for the focused container. Reason when inapplicable: `"Restart applies only to the focused container. Switch with Ctrl-S."`
- `ContinueInCurrentContainer`: valid only for the focused container (same reasoning as now, plus parallelism restriction).
- `CancelToPreviousStep`: disabled when any parallel peer is still running. Reason: `"Cannot go back while other agents in this group are still running."`
- `FinishWorkflow`: disabled when any parallel peer is still running. Reason: `"Cannot finish while other agents in this group are still running."`
- `Abort`: always valid; kills all active parallel containers.
- `Pause`: always valid; suspends the entire workflow (kills all active containers).

The engine enforces these constraints by computing `AvailableActions` with awareness of `active_steps` count. The frontend renders the `unavailable_reason` strings already (established in WI-0095).

### 11. Workflow State Strip — True Parallelism Rendering

**File:** `src/frontend/tui/workflow_view.rs`

Update `build_workflow_columns` so that steps in the same `depends_on` group that are actually concurrently runnable render with the **same** horizontal indent (no stagger). The indent-per-row stagger currently signals "these run sequentially"; remove it for truly parallel siblings.

Layout changes:
- Steps within a parallel group are stacked vertically **without** the per-row indent offset. They share the same `box_x` within their column.
- If a parallel group has more steps than `max_concurrent` permits at once, show the first `max_concurrent` steps at the "active" indent and render queued steps with a `·` prefix on their name to indicate "waiting for a slot".
- Completed steps within a large parallel group collapse: the topmost visible step in the group shows `<step_name> (+N completed)` when `N > 0` completed siblings exist. The `scroll_offset` on the strip allows the user to scroll down the group to see steps below the fold.
- The `workflow_strip_height` function should account for the effective `max_concurrent` (passed in via `WorkflowViewState` — add a `max_concurrent: Option<usize>` field) rather than always capping at 3.

Pass `max_concurrent` down from the engine through `WorkflowStepProgressInfo` metadata or as a separate `WorkflowViewState` field set by the frontend when the engine reports group start.

### 12. Post-Exit Slot Eviction

**File:** `src/frontend/tui/tabs.rs`, event-loop drain code

When `report_parallel_step_exited` fires for a parallel slot:
- Remove the corresponding `ParallelContainerSlot` from `parallel_slots` immediately.
- Do **not** push a `LastContainerSummary` grey bar — no post-exit summary for parallel slots.
- If a new step is being dequeued into that slot (via `report_parallel_step_dequeued`), add its `ParallelContainerSlot` to the list.
- Recompute `focused_slot_idx`: if the exited slot was focused, advance to the next live slot (wrapping). If no live slots remain, set `focused_slot_idx = 0` and let the container window go Hidden.

## Edge Case Considerations

- **`max_concurrent = 1`**: parallel group code still runs; it just never launches more than one container at a time. Behavior is identical to the current sequential model.
- **Single step in a group**: parallel group code is exercised with one item. No multi-container UX is shown (no minimized bars, no Ctrl-S).
- **All steps are sequentially dependent**: `ready_steps` always returns exactly one step; the engine runs it through the parallel group path with a group of size 1. No behavioral change.
- **Step failure with `abort_on_failure` mid-parallel-group**: engine kills all other active steps, cancels the queue, marks them `Cancelled`, then proceeds with existing abort path. No new state is needed.
- **Yolo countdown expires on one of N parallel steps**: kill only that step. Check the queue: if more steps are waiting for a slot, launch the next one. If no queue, the group is now "draining" — the remaining `N-1` steps continue, and the group finishes when they all complete or time out.
- **User sends Ctrl-C to a non-focused parallel container**: not possible via keyboard (only the focused slot receives input). The user must switch (Ctrl-S) to make the target slot focused before sending Ctrl-C.
- **Two parallel steps have `continue_in_current_container`-compatible next steps**: the WCB only offers "Continue in current container" for the focused step's next step. The other parallel step's continuation is handled when it finishes and the WCB is re-opened for that slot.
- **Workflow resumes (persisted state) with a partially-completed parallel group**: the resume path already calls `interrupted_running_steps()` and resets them to `Pending`. On resume, the engine re-runs the entire group from scratch (steps that were `Succeeded` remain succeeded; only the interrupted ones replay).
- **Terminal resize while parallel slots are active**: each slot's `container_resize_tx` must receive the new terminal dimensions. The event loop broadcasts the resize to all active slots, not just the focused one, since each container's PTY should track the real terminal size.
- **CLI with `max_concurrent > 1` but non-interactive stdout**: fall back to sequential output passthrough. Log a warning that parallel output merging is not supported in non-interactive mode.
- **`maxConcurrentAgents` in both repo and global config**: effective resolution is standard (repo wins over global). The resolved value is logged at workflow start at `debug` level.
- **Dynamic workflows**: the dynamic workflow leader schedules steps; `max_concurrent_agents` still caps how many the engine will run at once, independently of what the leader plans. The existing `max_concurrent_steps` advisory in the leader prompt (WI-0095) remains advisory; `max_concurrent_agents` is enforced in the engine.
- **Stuck detection timeout**: each slot's stuck timer resets independently when new PTY output arrives on that slot's channel. A noisy container doesn't prevent its sibling from being detected as stuck.
- **`report_parallel_step_exited` arrives before `report_parallel_step_dequeued`**: both may fire in the same engine tick. The frontend handles them in order: evict first, then add the new slot. The net slot count is unchanged.

## Test Considerations

- **Unit — `effective_max_concurrent_agents`**: verify flag > env > repo > global > None precedence with a mock `EffectiveConfig`.
- **Unit — `maxConcurrentAgents: 0` rejection**: `RepoConfig::load` returns an error; same for `GlobalConfig`.
- **Unit — `WorkflowDag::ready_steps`**: existing tests cover correctness; add a case with 4 concurrent-ready steps to confirm all four are returned.
- **Unit — parallel engine scheduling**: construct a workflow with 4 steps that are all concurrently ready, set `max_concurrent = 2`, and drive the engine with a mock `AgentExecutionFactory` that returns controllable `AgentExecution` futures. Assert only 2 start initially; the 3rd starts when the 1st finishes; the 4th starts when the 2nd finishes.
- **Unit — `abort_on_failure` in parallel group**: one of two running steps fails with `abort_on_failure`; assert the other is killed and the workflow outcome is `Aborted`.
- **Unit — yolo timeout in parallel group (with queue)**: yolo expires on slot 0; assert slot 0 is killed, slot 2 (queued) is launched, slot 1 continues.
- **Unit — yolo timeout in parallel group (draining)**: yolo expires on the only remaining queued step; assert no new launch; group finishes when the surviving step exits.
- **Unit — WCB available actions with parallel peers**: `can_cancel_to_previous_step` and `can_finish_workflow` are `false` when `parallel_peers_running > 0`.
- **Unit — `EngineRequest` routing**: `StepStuck { step_name }` event correctly identifies the stuck slot; other slots' stuck state is unaffected.
- **Unit — `tab.stuck` / `tab.yolo_mode` aggregation**: aggregate is `true` when any slot is stuck/yolo; `false` only when all are clear.
- **Unit — parallel slot eviction**: after `report_parallel_step_exited`, the corresponding slot is absent from `parallel_slots`; `focused_slot_idx` advances if the evicted slot was focused.
- **Unit — `build_workflow_columns` parallel rendering**: assert that steps sharing the same dependency set render at the same `box_x` with no per-row indent.
- **Unit — workflow strip completed-step collapse**: a group of 5 steps where 3 are `Succeeded` collapses to `<name> (+3 completed)` at the top.
- **Integration — TUI Ctrl-S rotation**: simulate 3 parallel slots; each Ctrl-S press advances `focused_slot_idx` cyclically; the 4th press returns to slot 0.
- **Integration — CLI parallel mode activated**: when `active_steps.len() > 1` and stdout is a TTY, assert the status bar row is drawn; when not a TTY, assert plain passthrough.
- **Integration — full parallel workflow run**: run a 4-step fully-parallel workflow end-to-end with `max_concurrent = 2`; assert all 4 steps complete successfully.
- **E2E — TUI minimized bar rendering**: 2 parallel steps running; the non-focused one renders as a minimized status bar at the bottom; Ctrl-S swaps them; bar disappears when the step exits.

## Codebase Integration

- **Layer discipline**: all scheduling, concurrency decisions, stuck detection, and yolo logic live in `src/engine/workflow/`. Frontends (`src/frontend/tui/`, `src/frontend/cli/`) are pure presentation. No frontend file may read `active_steps` directly — it only observes what the engine reports via trait callbacks.
- **`FuturesUnordered`**: use `futures::stream::FuturesUnordered` (already in Cargo.toml transitively via `tokio`) to poll multiple `AgentExecution` streams concurrently in `run_parallel_group`. Avoid hand-rolled `select!` arrays — they require a fixed arity at compile time.
- **`ParallelContainerSlot` and `Tab`**: the new slot vec lives in `src/frontend/tui/tabs.rs` alongside all existing shared-state types. Keep the naming convention (`SharedXxx` for `Arc<Mutex<Option<…>>>` types).
- **`CliParallelFrontend`**: place it in `src/frontend/cli/parallel.rs`. It implements `WorkflowFrontend`; delegate single-container methods to the existing `CliWorkflowFrontend` where appropriate.
- **API events**: extend the existing workflow event serialization in `src/command/commands/api_server.rs`; do not create a separate event type hierarchy.
- **`EngineRequest` extension**: the existing `UnboundedSender<EngineRequest>` channel is already shared between the engine and TUI. Extending the enum is non-breaking for existing match arms that use `_` catch-alls; add explicit arms where needed.
- **`WorkflowControlBoardState` in dialogs**: `src/frontend/tui/dialogs.rs` holds this struct; add the two new fields there and update all construction sites in `workflow_frontend.rs`.
- **Strip renderer**: `src/frontend/tui/workflow_view.rs` receives `max_concurrent` through `WorkflowViewState` (add the field). The engine sets it once via `report_parallel_group_started`; the TUI stores it in `SharedWorkflowViewState`. Strip rendering reads it from there.
- **`VALID_CONFIG_FIELDS`**: add `"maxConcurrentAgents"` to the allowlist in `src/command/commands/config.rs` so `config get/set` works for the new field.
- **Clap definition**: add `--max-concurrent <N>` to the `exec workflow` subcommand in `src/command/dispatch/projections/clap.rs`. Validate `>= 1` at parse time and store in `FlagConfig`.
- **Existing single-container tests**: none should break — the parallel group path is a strict superset of the sequential path and falls back gracefully when `max_concurrent == 1` or only one step is ready.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update `docs/02-using-the-tui.md`** to document Ctrl-S (switch between parallel containers), the minimized container status bars, the stuck/yolo coloring on minimized bars, and how Ctrl-M now hides all parallel containers at once.
- **Update `docs/07-configuration.md`** to add the `maxConcurrentAgents` field reference for both repo and global config, the `--max-concurrent` flag, and the `MAX_CONCURRENT_AGENTS` env var with their precedence.
- **Update `docs/08-headless-mode.md` (or CLI doc)** to describe the lightweight parallel VT100 display activated in interactive CLI mode when multiple agents are running.
- **Create `docs/XX-parallel-workflows.md`** as a new user guide covering: what parallelism means in awman workflows, how to configure `maxConcurrentAgents`, how the engine schedules slots, how stuck/yolo behave per-container, and the WCB scoping rules when multiple agents are active. Do not include implementation details — write for a user, not a developer.
- **Never create work-item-specific docs** — no "WI 0096 implementation guide" in published docs.
- **Keep all technical/implementation details in this work item spec or code comments**, not in `docs/`.

See `CLAUDE.md` for more guidance on documentation standards.
