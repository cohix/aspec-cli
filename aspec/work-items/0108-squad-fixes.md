# Work Item: Bug

Title: Squad fixes — attach, keybinding, yolo auto-advance, build logs, image names, TUI polish
Issue: (reported directly)

## Summary:
- Eight fixes to squad reported after WI 0106:
  1. TUI attach (`a`) flashed a screen and immediately returned to the squad tab.
  2. The New Tab dialog's open-squad shortcut should be `Ctrl-S`, not `Ctrl-A`.
  3. The leader agent and generated workflows must run in yolo mode with the
     same stuck detection/auto-advance as dynamic workflows (the leader used to
     finish its work and then nothing happened).
  4. Container image build output must go to per-build log files, not the
     daemon log; the daemon log gets lifecycle lines naming the file.
  5. Task-directory image names all collapsed onto `awman-workspace:latest`;
     the task slug must be part of every image name squad builds.
  6. The new-task description dialog's border title was a full sentence.
  7. The detail modal clipped the description to one line.
  8. Task cards rendered full-width with silently clipped descriptions.

## Implementation Details:

### 1. TUI attach flash (root-caused empirically)
The flash = the local attach client process exiting immediately after launch:
`attach_handle`'s wait task pushed `ContainerSlotEvent::Exited`, the slot was
evicted, and the render fell back to the task grid with the client's error
text discarded. Three concrete defects fixed:
- `docker attach --sig-proxy=false` (docker.rs): every squad agent container
  has a TTY, where signal proxying is inapplicable — and newer Docker CLIs
  reject the flag for TTY containers, so the client exited instantly. The flag
  is dropped (teardown never relied on it; `kill_local_exec` uses SIGKILL,
  which is never proxied).
- Apple runtime: the `container` CLI has **no** `attach` subcommand; the old
  code spawned `container attach` anyway (instant usage-error exit). The dead
  attach machinery was removed; real attach parity for Apple is WI 0109's
  attach rendezvous socket.
- Workflow-phase id mismatch: the engine publishes the container **name** as a
  step's `container_id`, while runtime discovery reports real (hex) ids, so
  `handle_for_container_id` and `label_with_step_names` never matched and
  workflow-phase attach silently attached nothing. Both now match name or id.
Additionally, when the local attach client dies, its exit code and output
tail are written to the tab's status log before the slot is evicted, so any
future failure explains itself instead of flashing.
`tests/squad_attach.rs` was updated to the WI 0106 contract (start the
foreign container with `-it`, drive its PID-1 process through `docker
attach`) — the old version still asserted the pre-0106 exec-shell contract
and could never pass against the attach rewrite.
Known remainder (out of scope here): task names that are prefixes of one
another (`foo`, `foo-bar`) cross-match in `list_task_containers`' name-prefix
discovery.

### 2. `Ctrl-S` opens squad from the New Tab dialog
`key_handler.rs` — the dialog-scoped binding moved from `Ctrl-A` to `Ctrl-S`,
including the dialog prompt hint. Safe because the other `Ctrl-S` meanings
(multiline submit, slot cycling) are gated on dialog states that can never be
the New Tab dialog. The global `Ctrl-A` previous-tab binding is untouched.

### 3. Leader yolo auto-advance (identical machinery to dynamic workflows)
`SquadAgentLauncher::run_leader` used to `execution.wait()` — but the leader
runs interactively (PTY) under yolo, so its agent TUI stays open after
finishing and the wait never returned. It now drives the launched execution
through `drive_unattended_agent` (engine/squad/launcher.rs): the same
`StuckEvent` stream and `YOLO_COUNTDOWN_DURATION` (60s) the workflow engine
and the dynamic leader drive use — stuck → countdown → kill-and-advance, with
`Unstuck` cancelling the countdown. Generated workflows already ran with
`yolo: true` and get stuck/auto-advance from the workflow engine itself with
the unattended frontend answering `LaunchNext`/`Continue`.
Daemon logging is lifecycle-only: countdown started ("advancing automatically
in 60s"), recovered/cancelled, and auto-advanced — never per-tick.
Workflow step-agent images are now also pre-built before execution
(`resolve_and_validate_workflow_agents` + `ensure_agent_image…`), matching the
dynamic path's WI-0092 §9b behavior.

### 4. Build logs
Raw image-build output is redirected to
`<squad root>/builds/<task>/<run-id>-<n>.log` (`SquadPaths::task_builds_dir`).
`ensure_agent_image_with_build_output` takes an optional `BuildOutputTarget`;
the daemon's `SquadBuildLogs` implements it (lazy file creation — an
image-exists fast path leaves no empty file) and writes one lifecycle line to
the daemon log on build start and finish/failure, naming the image and the
log path. Interactive callers keep the previous streaming behavior.

### 5. Image names carry the task slug
Every default task workspace is a folder literally named `workspace`
(`…/tasks/<slug>/workspace`), so folder-derived tags collided across tasks.
`data::image_tags` now detects that trailing layout structurally (valid task
slug charset required; independent of `AWMAN_SQUAD_ROOT` relocation) and tags
those roots `awman-squad-<slug>[-<agent>]:latest`. All build/run call sites
funnel through `project_image_tag`/`agent_image_tag`, so the leader, workflow
steps, base builds, and scaffolded Dockerfile `FROM` lines all agree.

### 6–8. TUI polish
- The description dialog's border title is now the short
  "New squad task description"; the full instruction stays in the body.
- The detail modal renders the description as a wrapped multi-line block
  (width-aware word wrap, capped at half the modal, ellipsis when capped).
- Task cards are laid out at a fixed width capped at half the grid width
  (`Flex::Start`, so the last card is never stretched back to full width;
  `CARD_MIN_WIDTH` still wins on narrow terminals), and the card description
  is explicitly truncated to the card's inner width with an ellipsis.

## Edge Case Considerations:
- Half-width cap vs. minimum card width: the minimum wins when half the grid
  is narrower than a readable card.
- A path like `…/tasks/<x>/workspace` outside squad matches the slug-tag
  detection; the result is still a valid, deterministic image name.
- Attach-exit surfacing runs for both evaluation and workflow-phase sessions.
- Auto-advance falls back to a synthetic `KILLED_EXIT_CODE` exit if the
  backend errors after the kill; the verdict file remains the authority on
  the run's outcome either way.

## Test Considerations:
- Unit: image-tag slug detection; card width cap + ellipsis truncation;
  `drive_unattended_agent` returns a self-exiting agent's exit; step-label
  resolution when the daemon publishes a container name; Ctrl-S dialog tests
  replacing the Ctrl-A ones.
- E2E (docker-gated): `tests/squad_attach.rs` now starts the foreign
  container with `-it` and asserts attach reaches PID-1, not a shell.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.

## Documentation
- `docs/12-squad.md`: Ctrl-S hint, image naming, build-log layout, leader
  auto-advance guardrail, attach failure surfacing + apple-containers
  limitation.
- `docs/02-using-the-tui.md`: Ctrl-S hint.
