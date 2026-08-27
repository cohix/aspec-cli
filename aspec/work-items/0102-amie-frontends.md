# Work Item: Feature

Title: amie — CLI and TUI frontends
Issue: issuelink

## Summary:
- Adds the user-facing surface for amie: the `awman amie` CLI subcommands and a dedicated amie tab inside the main `awman` TUI.
- **Depends on [`0101-amie-daemon.md`](./0101-amie-daemon.md)**, which delivers the daemon, the data model, the scheduler, the `amie` catalogue entries, the `ConditionGateway` trait with its local and remote implementations, the `AmieSupervisor`, and the Layer 1 `AgentRuntimeEngine::attach` primitive. This work item adds **only Layer 3 presentation** on top of them. If any step below appears to require new business logic, that is a signal the logic belongs in WI 0101's Layer 2, not here.
- CLI: `awman amie add|list|show|remove|pause|resume|start|stop|status|logs|attach`. Every CRUD subcommand is a thin call through `AmieSupervisor::ensure_running()` to a `RemoteConditionGateway` — the CLI validates nothing and decides nothing.
- TUI: amie is **not** a separate TUI program and gets no top-level Ratatui app. It is a special, singleton tab inside the same multi-tab TUI every other awman session runs in, opened with `Ctrl-A` from the New Tab dialog or pre-opened by bare `awman amie`. Unlike every other tab it is not bound to a project directory, it renders only amie content rather than the normal command-box/container UX, and it carries its own tab colour so it is never mistaken for a regular tab. Standard tab controls (`Ctrl-T`, `Ctrl-A`, `Ctrl-D`, close) work on it exactly as on any other tab.
- Attach connects to a running container through `AgentRuntimeEngine::attach` — the container I/O is always a direct runtime connection, never proxied through the daemon.
- **Attaching to a running amie workflow from the TUI reproduces the `exec workflow --dynamic` UX exactly**: the workflow state strip, the focused container maximized with the others as minimized bars, and `Ctrl-S` cycling between them. This is near-total reuse — the strip and the slot machinery are already decoupled from the workflow engine, so the work is feeding them from a polled `WorkflowState` instead of an in-process run.
- Only **currently running** agents can be attached to; replaying a finished run is out of scope.
- There is no SSE or log streaming anywhere. List, detail, and workflow state are all ordinary polled request/response; live agent output comes from the container connection, not the daemon.

**Read `aspec/architecture/2026-grand-architecture.md` before implementing.** Tenet 2 is the governing constraint for this entire work item: a frontend is a presentation and user-input vehicle only, and may contain no business logic. Both frontends here consume the same Layer 2 `Command` objects through `Dispatch`, which is what guarantees the CLI and TUI cannot drift.

## User Stories

### User Story 1:
As a: user

I want to:
manage conditions equally well from the CLI (for scripting and quick edits) or from an interactive TUI (for browsing and monitoring)

So I can:
use whichever interface fits how I'm working, consistent with how every other awman feature behaves in both modes

### User Story 2:
As a: user

I want to:
see every condition's status and history in a dedicated tab alongside my normal project tabs, and drop into a live view of whatever an agent is doing right now

So I can:
trust and audit what amie did autonomously, and debug a condition that is not behaving as expected, without leaving the tool I am already in

## Implementation Details:

### Part 1 — CLI (Layer 3)

The `amie` `CommandSpec`s land in `CommandCatalogue` in WI 0101, so the clap surface is generated rather than hand-written — the CLI frontend holds no command or flag list of its own. This work item wires the resulting commands to output.

| Subcommand | Arguments | Flags | Behaviour |
|---|---|---|---|
| `awman amie` | — | `-n/--non-interactive`, `--json` | TTY without `-n`: `ensure_running()`, then launch the main TUI pre-opened to the amie tab. Non-interactive or piped: `ensure_running()`, print the status summary, exit. |
| `awman amie start` | — | `--interval <duration>` (default `5m`), `--port <n>` (default OS-assigned), `--background` (default true) | Explicit daemon start, subject to the mutual-exclusion guard against `awman api`. |
| `awman amie stop` (alias `kill`) | — | — | SIGTERM, graceful drain, clear pidfile and `server.json`. |
| `awman amie status` | — | `--json` | Daemon liveness, condition count, last scheduler tick. |
| `awman amie logs` | — | `-f/--follow` | Tail the daemon log file locally. Not an API call — the daemon exposes no log route. |
| `awman amie add` | — | `--name <slug>`, `--description <text>`, `--interval <duration>`, `--agent <name>`, `--model <name>`, `--interview`, `-n/--non-interactive` | Create a condition. `--interview` collects fields interactively before sending, consistent with `awman new workflow --interview`. |
| `awman amie list` | — | `--json` | Table of conditions: name, status, last-run outcome, next evaluation. |
| `awman amie show <name>` | `<name>` | `--json` | Condition detail plus execution history. |
| `awman amie remove <name>` | `<name>` | `-y/--yes` | Delete the condition, then prompt (unless `-y`) to also delete its persistent directory. |
| `awman amie pause <name>` / `resume <name>` | `<name>` | — | Toggle status without deleting. |
| `awman amie attach <name>` | `<name>` | `--container <id>` | Attach to the condition's currently running container. Does not contact the daemon — see Part 3. |

Every CRUD subcommand follows the same three-line shape: obtain a gateway from `AmieSupervisor::ensure_running()`, call one gateway method, render the result. Rendering respects the existing `--json` convention (`--json` implies `--non-interactive`, per `aspec/uxui/cli.md`) and reuses whatever table/JSON renderers the CLI frontend already uses for `awman status` and `awman api status` rather than introducing amie-specific formatting helpers.

`main.rs` needs one carve-out: bare `awman amie` (no further subcommand, TTY, no `-n`) must route to the TUI rather than `cli::run`, which today receives every invocation where `matches.subcommand_name().is_some()`. Grow `tui::run`'s signature with an `initial_tab: InitialTab` parameter (`InitialTab::Normal(Session) | InitialTab::Amie`) rather than overloading the existing `ctx.session` plumbing. Every other `awman amie <subcommand>` continues through `cli::run` unchanged.

### Part 2 — TUI amie tab (Layer 3)

The amie tab lives inside the existing multi-tab `App`/event loop (`src/frontend/tui/app.rs`, `src/frontend/tui/tabs.rs`). Only its content and creation path are special-cased; its participation in the tab bar, tab cycling, and closing is entirely ordinary.

**Tab identity and the singleton rule**

- Add `is_amie: bool` to `Tab`, following the existing `is_remote: bool` precedent, which already establishes that a tab can be a fixed special kind with its own colour independent of execution phase. Do not introduce a second tab-kind mechanism.
- `App::open_or_focus_amie_tab(&mut self)`: focus the existing amie tab if one is present; otherwise call `AmieSupervisor::ensure_running()` and, on success, construct and push one. On failure — most commonly because `awman api` is running — surface the error in the status bar and open nothing, rather than leaving a tab with no working backend behind it. The singleton is per-`App`; two TUI processes may each hold one, since single-instance enforcement lives at the daemon's pidfile, not the tab.
- `Tab.session` is non-optional and is threaded through `Dispatch`/`spawn_command` at every call site, so making it optional is a far larger refactor than this work item warrants. Give the amie tab a synthetic `Session` rooted at the `AmiePaths` root via `Session::open_at_git_root(root, root, ...)` — the same non-git fallback already used whenever a tab opens on a plain directory. This session exists solely to satisfy the `Tab` API. Nothing displayed in the tab derives from it: condition data comes from the gateway, and attach resolves through the runtime engine.
- Add a `Tab::new_amie(session)` constructor that skips `start_git_poll` — there is no meaningful git diff for a synthetic directory — and skip the `ready`/`status --watch` startup auto-spawn that normal tab creation performs. `Ctrl-G` (git sidebar) is a no-op while the amie tab is active.
- Special-case `project_name()` (`src/frontend/tui/tabs/labels.rs`) to return a fixed `"amie"` rather than deriving from `working_dir().file_name()`, so the label is correct even when `AWMAN_AMIE_ROOT` points somewhere with an unrelated basename.

**Tab colour**

`tab_color()` (`src/frontend/tui/tabs.rs`) checks `is_amie` at the same precedence tier as `is_remote` — both are fixed-kind colours that win over execution-phase colouring. Use `Color::Cyan`, which is unused today (the current palette is Yellow/Magenta for the yolo countdown, Yellow for stuck, Magenta for remote, and Red/Green/Blue/DarkGray for phases).

**Entry points**

- `Ctrl-A`, scoped to the existing New Tab dialog (`Dialog::TextInput { title: "New Tab", .. }`): add a hint line reading "Press Ctrl-A to open amie" to the dialog's rendered prompt, and handle the key in the dialog's key handling using the same `FocusContext::Dialog` scoping that already suppresses `Ctrl-A`/`Ctrl-D` while a dialog holds focus. The global `Ctrl-T`/`Ctrl-A`/`Ctrl-D` bindings are untouched.

  **Why `Ctrl-A` is safe here despite being a global binding.** Globally, `Ctrl-A` maps to `Action::PreviousTab` — but `keymap.rs` already gates that mapping on `ctx != FocusContext::Dialog`, so it is deliberately inert whenever a dialog holds focus. The New Tab dialog is exactly that context, which makes `Ctrl-A` genuinely unclaimed there rather than overloaded. Implement this binding inside the dialog's key handling only; do **not** add a second global `Ctrl-A` mapping or relax the existing `FocusContext` guard, either of which would break previous-tab navigation.

  This also leaves WI 0096's `Ctrl-S` (cycle focused container among parallel workflow slots, passed through to the container PTY when only one slot is active) untouched — amie no longer uses that key at all.
- Bare `awman amie` in a TTY, via the `InitialTab::Amie` parameter from Part 1. From either entry point the user can still press `Ctrl-T` afterwards to open ordinary directory-bound tabs alongside it.

**Content**

- Add `src/frontend/tui/render/amie.rs`, a sibling of `command_box.rs`/`tab_bar.rs`. Branch to it from the existing render-dispatch point that chooses the body for `app.active_tab()`: when the active tab is the amie tab, render only the amie view and never fall through to the command-box/container-slot/workflow rendering. This is what makes the tab show amie content exclusively.
- **List view**: all conditions, arrow-key navigable — name, status, last-run outcome, next scheduled evaluation.
- **Detail view**: description, mount scope, interval, and execution history for the selected condition.
- Both views are populated by polling the gateway (`list`, `get`, `runs`) on a short interval while the tab is focused, using ordinary short-lived requests. Polling pauses when the tab loses focus and resumes when it regains it, so a backgrounded tab does not keep querying the daemon.
- **In-tab actions** (add, pause/resume, remove) call the same gateway methods the CLI's subcommands call. No amie business logic is implemented in the TUI.

### Part 3 — Attach, in both frontends

WI 0101 adds `AgentRuntimeEngine::attach(&AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError>`, which returns an instance that flows into the **existing** `run_with_frontend(Box<dyn AgentFrontend>) -> AgentExecution` path. That choice is what makes attach nearly free here: both frontends already have `AgentFrontend` implementations, and neither needs a new I/O mechanism.

- **CLI**: `CliFrontend` already handles raw-mode stdin, SIGWINCH-driven resize, and PTY binding. Attach reuses it unchanged.
- **TUI**: `TuiContainerProxy::with_io` already bridges engine-side channels into `ContainerSlotIo` for the event loop's vt100 rendering. Attach reuses it unchanged.

Both `attach` and `list_running_with_name_prefix` are on the cross-paradigm trait, so **the attach path holds `Arc<dyn AgentRuntimeEngine>` and needs no per-tier branching**. amie refuses to run under a sandbox runtime entirely (WI 0101, Part 5), so attach never sees that tier; Docker and Apple Containers behave identically.

A condition has two distinct things worth attaching to, and they get different treatment:

**3a — Evaluation phase (one container, no workflow yet).** Before the evaluation agent has decided anything there is no workflow, so this is the simple case: discover the single container by name prefix `awman-amie-<slug>-`, attach, render it in one slot. No state strip. The CLI's `amie attach` always behaves this way when no workflow is running.

**3b — Workflow phase: reproduce the `exec workflow --dynamic` UX exactly.**

Once the condition has generated a workflow and the daemon is executing it, attaching from the TUI must be **visually and behaviourally indistinguishable from having run `awman exec workflow --dynamic` in that tab**: the workflow state strip across the bottom, the focused container maximized, other parallel containers as minimized bars, and `Ctrl-S` cycling between them.

This requires almost no new rendering code, because the TUI's workflow UX is already fully decoupled from the workflow engine:

- The state strip (`src/frontend/tui/workflow_view.rs`) reads **only** `Tab::workflow_state: SharedWorkflowViewState = Arc<Mutex<Option<WorkflowViewState>>>`. It holds no engine reference. Whatever writes that mutex drives the strip.
- Container slots are created **purely from events** pushed onto `Tab::container_slot_events: Arc<Mutex<VecDeque<ContainerSlotEvent>>>`, drained by `drain_container_slot_events` in the tick loop. Nothing in that path references `WorkflowEngine` — the existing tests build slots by pushing events with no engine present. The only real requirement is supplying an `UnboundedReceiver<Vec<u8>>` of PTY bytes, which is exactly what `attach` produces.
- `Ctrl-S` rotation (`cycle_focused_slot`) is a pure function of `container_slots` and `focused_slot_idx`. **It needs no changes and will work on externally-populated slots automatically.**

So the implementation is a driver that feeds those two existing sinks:

1. **Revive `RemoteWorkflowPoller`** (`src/frontend/tui/per_command/remote.rs`). It is already written and correct — 500ms poll loop, terminal-status detection, `workflow_state_to_view_state` conversion, and it already holds an `Arc<Mutex<Option<WorkflowViewState>>>` structurally identical to `SharedWorkflowViewState`. It is dead code only because nothing ever constructed it and the module is private (WI 0101 explains why). Make the module `pub(crate)`, generalize its fetch so it can call `GET /v1/conditions/{name}/workflow` on the amie daemon as well as the API server's route, and start it with `tab.workflow_state.clone()`. **The strip then renders with zero renderer changes.**
2. **Drive slots from the polled state, not from a separate discovery query.** `WorkflowState::step_states` is a `HashMap<String, StepState>` where `StepState::Running { container_id: Option<String> }` already carries the container id, and the map key is the step name. That is exactly the `(step_name, container_id)` pairing `ContainerSlotEvent::Launched { step_name, agent, model, io }` needs, so the workflow state is the authoritative source and no name-parsing is required. On each poll:
   - A step that entered `Running` with a `container_id`: resolve its `AgentHandle`, `attach`, take the `AgentIo`, and push `Launched { step_name, agent, model, io: Some(..) }`.
   - A step that left `Running`: push `Exited { step_name }`.
   - Agent and model come from the matching `WorkflowStepInfo` in `WorkflowState::steps`.
3. Everything downstream — slot creation, vt100 parsing, maximize/minimize, the minimized bars, `Ctrl-S`, the strip's column grouping from `depends_on` — is existing behaviour that now simply has a different data source.

The TUI attaches to **every** running container of the workflow, not one; `--container` disambiguation (below) is a CLI-only concern, because the CLI binds a single host terminal and genuinely cannot show N at once.

Because state now comes from the daemon, this is the one attach path that **does** require the daemon to be running. If it dies mid-workflow the strip freezes and the tab shows a "daemon not reachable" indicator, but already-attached container slots keep streaming, since those are direct runtime connections.

Behaviour shared by both frontends:
- No running container for the condition → fail immediately with "no run currently in progress for condition `<name>`". Never fall back to a finished run.
- More than one running container under that label (a generated workflow with parallel steps, per `aspec/work-items/0096-true-parallel-agents.md`) → list them with short IDs and step names and require `--container <id>`; never guess.
- Detaching (Ctrl-C, closing the tab, killing the CLI process) ends only the attach session. `BridgeConfig::cancel_on_grace_expired` is `None` for attach, so a quiet attached agent is never killed by the attaching process, and the container and daemon are untouched. Re-attaching afterwards starts a fresh session.

## Resolved Decisions

1. **Tab label is lowercase `amie`**, with no extra marker glyph. Consistent with the always-lowercase naming rule and with how `awman` itself is styled; directory-derived tab labels take whatever casing their folder has, so it does not read as out of place. The distinct tab colour already carries the "this is a special tab" signal, so a glyph would be redundant.
2. **`Ctrl-A` opens the tab from the New Tab dialog** — see Part 2 for why that is safe alongside the global previous-tab binding.

## Edge Case Considerations:
- **`Ctrl-A` with an amie tab already open**: focus it; never create a second.
- **`Ctrl-A` while `awman api` is running**: `ensure_running()` fails, so the dialog surfaces the conflict error and no tab opens. The message must name the API server specifically, not report a generic connection failure.
- **A CRUD subcommand's implicit daemon start fails because `awman api` is running**: same requirement — surface the specific conflict, not "could not connect".
- **Closing the amie tab**: ordinary last-tab protection applies, with no special case. Closing ends any attach session and stops polling; it never stops the daemon or an in-flight evaluation.
- **Cycling away from the amie tab with `Ctrl-A`/`Ctrl-D`**: it participates in the normal wrap-around cycle, its sub-view state survives defocus, and polling pauses until it is focused again.
- **The daemon dies while the amie tab is open**: list and detail views show a clear "daemon not running" state with the error, rather than an empty list that reads as "no conditions". An attach session already in progress is unaffected, since it does not depend on the daemon.
- **Two TUI processes each with an amie tab**: both are ordinary clients of the one daemon; the daemon's pidfile is the single-instance guarantee, not the tab.
- **Tab bar width with many tabs open**: the fixed `"amie"` label and its colour render correctly at any position, using the existing label/width computation in `render_tab_bar`.
- **`--container <id>` naming a container outside the condition's label set**: validate membership in the discovered set first and fail clearly, rather than exec'ing into an unrelated container.
- **A sandbox runtime is active** (`runtime: docker-sbx-experimental`): amie is unsupported entirely (WI 0101, Part 5), so every entry point fails at `ensure_running()` with the runtime error rather than each subcommand inventing its own message. `Ctrl-A` surfaces it in the dialog and opens no tab; `amie attach` surfaces it instead of reporting "no run in progress", which would wrongly imply the condition is merely idle.
- **Docker vs Apple Containers**: no frontend-visible difference. Attach, discovery, the tab, and every subcommand behave identically; neither frontend branches on the backend.
- **A workflow step starts or finishes between polls**: slots are reconciled against the polled `step_states` each cycle, so a step that appeared and vanished within one interval simply never gets a slot. Never leave a slot for a step no longer `Running`, and never create a second slot for a step that already has one — `Launched` handling already dedupes by `step_name`, but the driver must not rely on that alone.
- **The daemon dies mid-workflow while the TUI is attached**: the strip freezes at its last known state and the tab shows a "daemon not reachable" indicator. Already-attached slots keep streaming, because those are direct runtime connections. Do not tear down slots on a failed poll — the containers are still doing useful work.
- **A step's `container_id` is absent while its state is `Running`** (the daemon recorded the transition before the container was created): skip it this cycle and pick it up on the next poll. Do not treat it as an error or as a finished step.
- **The workflow finishes while the tab is attached**: the strip shows the terminal state, slots exit normally via `Exited`, and the tab returns to the detail view. This must match what the user sees at the end of a normal `exec workflow` run.
- **Attaching to a condition mid-workflow, after it has already run several steps**: the strip renders the full DAG including already-completed steps (the polled `WorkflowState` carries `completed_steps` and the full `steps` list), so a late attach shows the same picture as having watched from the start — minus scrollback for containers that already exited, which is unavoidable.
- **`Ctrl-S` with exactly one running workflow container**: falls through to the container's PTY as flow control, exactly as it does in a normal single-container workflow. Do not special-case amie here.
- **Concurrent attach sessions against one container** (CLI and TUI at once): both are independent execs; Docker permits this, and neither disturbs the other or the original process.
- **The condition's container exits mid-attach**: the exec session ends normally and the frontend returns to the previous view; this is an ordinary exit, not an error.
- **`--json` with an unreachable daemon**: emit a structured error object on stdout and a non-zero exit code, consistent with how other `--json` commands report failure, rather than a human-readable message.

## Test Considerations:
- Parity — the new `amie` catalogue entries pass the existing cross-frontend `parity_test.rs`, proving the CLI, TUI, and API projections expose identical flags.
- Unit — `tab_color()` returns the amie colour for `is_amie` regardless of execution phase, stuck state, or yolo countdown, mirroring the existing `is_remote` precedence test.
- Unit — `project_name()` returns `"amie"` for an amie tab, including when `AWMAN_AMIE_ROOT` has an unrelated basename.
- Unit — `open_or_focus_amie_tab()` is idempotent: a second call focuses the existing tab rather than pushing a duplicate.
- Unit — `open_or_focus_amie_tab()` opens no tab and surfaces the error when `ensure_running()` fails.
- Integration — creating the amie tab auto-spawns neither `ready` nor `status --watch`, and starts no git poll.
- Integration — `Ctrl-T` shows the "Press Ctrl-A to open amie" hint, and `Ctrl-A` while that dialog holds focus closes it and activates the amie tab.
- Integration — **`Ctrl-A` context separation** (the load-bearing test for this binding, since one key now serves two roles by focus context): with no dialog open, `Ctrl-A` still switches to the previous tab and does *not* open amie; with the New Tab dialog open, `Ctrl-A` opens amie and does *not* switch tabs. Assert both directions, including with multiple tabs open so previous-tab navigation is observable.
- Integration — `Ctrl-A`/`Ctrl-D` include the amie tab in the cycle, its sub-view state survives defocus, and polling pauses while unfocused.
- Integration — every CLI CRUD subcommand issues exactly one gateway call and performs no local validation; running the CLI without filesystem access to `~/.awman/data/awman.db` still succeeds, proving it never reads the database directly.
- Integration — CLI and TUI paths for the same operation resolve to the same Layer 2 command object, guarding against divergence.
- Integration — evaluation-phase attach resolves through `list_running_with_name_prefix` and issues no database read; verified with the daemon stopped and a container still up.
- Integration — **workflow-phase attach reproduces the normal workflow UX**: given a polled `WorkflowState` with three steps (one done, two running with container ids), the tab ends up with the same `container_slots`, `focused_slot_idx`, and `WorkflowViewState` as an equivalent in-process `exec workflow` run. Assert against the in-process case rather than against hand-written expectations, so the two cannot drift.
- Integration — `Ctrl-S` cycles between externally-populated slots with no amie-specific handling, and falls through to the PTY when only one slot exists.
- Unit — the poll-to-slot driver: a step entering `Running` emits `Launched` with the right `step_name`/`agent`/`model`; a step leaving `Running` emits `Exited`; a `Running` step with no `container_id` yet emits nothing and is retried; no duplicate `Launched` for a step that already has a slot.
- Unit — `RemoteWorkflowPoller`, once revived, converts a `WorkflowState` from the amie route into the same `WorkflowViewState` it produces for the API server's route.
- Integration — the state strip renders from a poller-driven mutex with no renderer changes, including `depends_on` column grouping and completed-step collapsing.
- Integration — the daemon dying mid-workflow freezes the strip and shows the unreachable indicator without tearing down live slots.
- Integration — attaching mid-workflow renders already-completed steps in the strip, matching a from-the-start attach.
- Integration — attach with no running container fails fast and performs no runtime call beyond the label query.
- Integration — attach with two running containers under one condition lists both and exits non-zero without attaching.
- Integration — under a sandbox runtime every amie entry point (CLI subcommands, `Ctrl-A`, bare `awman amie`) fails at `ensure_running()` with the runtime error; no tab opens and no subcommand reports a misleading "no run in progress".
- Integration — the CLI and TUI behave identically under Docker and Apple Containers, with no backend branching in either frontend.
- Integration — the CLI attach path uses `CliFrontend` and the TUI attach path uses `TuiContainerProxy::with_io`, with no new `AgentFrontend` implementation introduced by this work item.
- Integration — closing the amie tab mid-attach ends the exec session only; the container remains running and is re-attachable.
- Integration — with the daemon stopped, the amie tab renders an explicit "daemon not running" state rather than an empty condition list.
- E2E — `awman amie` (bare, TTY) opens the TUI with exactly one tab: the amie tab, active and distinctly coloured.
- E2E — `awman amie add/list/show/pause/resume/remove` round-trips against a live daemon.
- E2E — `Ctrl-G` inside the amie tab is a no-op, confirming no git UI renders for a non-project tab.
- E2E — `--json` output for `list`/`show`/`status` is well-formed and matches the shape the daemon returned.

## Codebase Integration:
- This work item adds Layer 3 code and nothing else. Any requirement that appears to need validation, scheduling, or persistence logic belongs in WI 0101's Layer 2 or below — do not implement it here.
- The amie tab is a special case of the existing `Tab`/`App` model, not a new frontend. Reuse `Tab`, `App::tabs`, `App::active_tab`, the `Dialog`/`FocusContext` scoping mechanism, and the existing tab-bar and render-dispatch structure. Do not add a second Ratatui `Terminal`, a second event loop, or any path that bypasses `event_loop::run_event_loop`. The only genuinely new pieces are the `is_amie` marker, the amie render module, the `Ctrl-A` dialog binding, the `InitialTab` parameter, and gateway polling.
- Attach must not introduce a second I/O mechanism. It reuses the existing `AgentFrontend` implementations on both sides; if either frontend appears to need a new one, the attach primitive in WI 0101 is shaped wrong and should be fixed there instead.
- **The workflow UX must be reused, not reimplemented.** Do not write an amie-specific state strip, an amie-specific slot type, or an amie-specific key handler. The only new code is the driver that writes `Tab::workflow_state` and pushes `ContainerSlotEvent`s; everything the user sees is existing rendering. If a change to `workflow_view.rs`, `container_slots.rs`, or `cycle_focused_slot` seems necessary, that is a strong signal the driver is emitting the wrong events — fix the driver.
- Revive `RemoteWorkflowPoller` rather than writing a second poller. Generalizing its fetch to serve both the API server's route and amie's is the intended change; a parallel implementation would leave two pollers that must be kept in step.
- No frontend module may import `ConditionStore`. Condition data reaches both frontends only through a `ConditionGateway`.
- Reuse the existing CLI table and JSON renderers rather than adding amie-specific formatting helpers, so `awman amie list` looks like `awman status` and `awman api status`.
- Keep the amie tab's synthetic `Session` internal. Never surface its `working_dir` or `git_root` in the amie UI.
- Once implemented, add `awman amie` to the top-level command table and a per-command section in `aspec/uxui/cli.md`, in the style of the existing `awman api` section, and confirm the catalogue and that document agree.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Create a new user guide**: `docs/16-amie.md` — what conditions are, how to define them from the CLI and the TUI, how to open and use the amie tab, how to inspect and attach to running conditions, how the unattended execution guardrails (mount scope, forced worktree isolation) work, how the global `amie` config restricts agents/models and sets standing guidance, and that amie and `awman api` cannot run at the same time, with the commands to switch between them.
- **Update existing feature docs**: `docs/02-using-the-tui.md` — the amie tab, how to open it (`Ctrl-A` from the New Tab dialog, or bare `awman amie`), its distinct colour, and that it behaves like a normal tab for `Ctrl-A`/`Ctrl-D`/close while showing amie-only content.
- **Never create work-item-specific docs** (e.g. no "WI 0102 implementation guide" in published docs).
- **Keep all technical/implementation details in this work item spec or code comments**, not in `docs/`.
- **Docs are for end users**, not for developers trying to understand implementation.

See `CLAUDE.md` for more guidance on documentation standards.
