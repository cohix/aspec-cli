# Work Item: Task

Title: Architecture hardening and cleanup — credential argv leak, Layer 4 slimming, TUI module splits, API frontend shrinkage
Issue: issuelink

> **Architectural basis**: this work item was produced by an architecture review against
> `aspec/architecture/2026-grand-architecture.md`. None of the items below are hard
> violations of the layered architecture; they are hardening and code-health improvements
> that bring the codebase closer to the spec's spirit (Layer 4 minimalism, small modules,
> frontends as thin translation layers). Read that document before implementing.

## Summary:
- Finding A (hardening) — `src/engine/container/docker.rs` (~lines 784–786) passes agent
  credentials as `-e KEY=VALUE` strings in the `docker run` argument vector. While the
  spawned container receives them as env vars (correct per `aspec/architecture/design.md`),
  the secret VALUE is briefly visible to other host processes via `ps` /
  `/proc/<pid>/cmdline` while the docker client process runs.
- Finding B (Layer 4 slimming) — `src/main.rs` carries knowledge that belongs to the
  catalogue/command layer: a hardcoded intercept for the removed `--mount-ssh` flag
  (lines ~36–43), and runtime-detection fallback decision logic (lines ~63–100).
- Finding C (module size) — the largest TUI modules far exceed the foundation guidance of
  "small, easily understood modules... understandable by an intermediate Rust programmer":
  `src/frontend/tui/mod.rs` (~4,100 lines), `render.rs` (~2,900), `tabs.rs` (~2,600).
- Finding D (API frontend shape) — `src/frontend/api/routes.rs` (~2,200 lines),
  `queue_worker.rs`, and `session_setup.rs` hold most of what makes API mode work. As
  WI 0097 extracts parsing/validation/persistence downward, this package should visibly
  shrink toward the spec's definition: "translate the lower-level package's functionality
  into an HTTP-powered API."

## User Stories

### User Story 1:
As a: user running awman on a shared or multi-user host

I want to: have my agent API keys never appear in any process's command-line arguments

So I can: be confident other local processes cannot scrape my credentials from `ps` output

### User Story 2:
As an: intermediate Rust developer contributing to awman

I want to: navigate TUI code in focused modules (event loop, dialog routing, rendering, tab state)

So I can: understand and modify one concern without reading a 4,000-line file

### User Story 3:
As a: developer maintaining the command catalogue

I want to: removed/renamed flags and their migration hints defined in the catalogue alongside live flags

So I can: keep every scrap of flag knowledge in the single source of truth instead of `main.rs`

## Implementation Details:

### A. Keep credential values out of argv (`docker.rs`)
- Change credential injection from `args.push(format!("{k}={v}"))` to passing `-e KEY`
  (name only) and setting the variable on the spawned docker-client child process via
  `Command::env(k, v)`. Docker's CLI resolves a name-only `-e` from its own environment,
  so the container receives the same value with nothing secret in argv.
- Apply the same treatment to the Apple-containers backend (`src/engine/container/apple.rs`)
  and the sandbox backend (`src/engine/sandbox/dsbx/`) if they inject credentials the same
  way — audit all `agent_credentials` consumers under `src/engine/`.
- Verify `display_command()` / `mask_env_in_args()` in `src/engine/container/display.rs`
  still render a useful (and still-masked) command line after the change — the displayed
  form should show `-e KEY` per the actual invocation.
- This preserves the `design.md` constraint that env vars are "injected at container
  startup only" — no files, no persistence, just a cleaner transport.

### B. Slim `main.rs` to pure wiring
- **Removed-flag migration hints**: add a removed-flags list to the catalogue (e.g.
  `RemovedFlagSpec { name: "--mount-ssh", hint: "Pass `--overlay ssh()` instead... see docs/09-overlays.md" }`)
  and a small catalogue/projection helper that scans argv for removed flags and returns
  the hint. `main.rs` calls that helper instead of hardcoding the flag name and message.
  Future flag removals then require no `main.rs` edits.
- **Runtime detection with fallback**: move the fatal-vs-warn-vs-modal decision logic
  currently inline in `main.rs` (unknown `runtime:` → CLI fatal / TUI modal; unavailable
  runtime → warn and continue on default Docker only when the command doesn't require a
  runtime per `CommandCatalogue::requires_runtime`) into a constructor on the Layer 2
  `Engines` type, e.g.
  `Engines::detect(catalogue: &CommandCatalogue, config: &GlobalConfig, command_path: &[&str]) -> Result<(Engines, Option<String>), ...>`
  returning the engines plus the optional TUI fatal-modal message. `main.rs` keeps only:
  build clap → parse → call `Engines::detect` → open session → route CLI/TUI.
- Behavior must be bit-for-bit identical: same messages, same exit codes, same TUI modal.
  The existing routing tests in `main.rs` stay; add unit tests for the new `Engines::detect`
  covering the three paths (valid runtime, unknown runtime string, unavailable-on-host
  runtime with and without a runtime-requiring command).

### C. Split oversized TUI modules
- Target: no single file in `src/frontend/tui/` above ~1,500 lines after the split. This is
  a mechanical reorganization — NO behavior changes, NO logic changes, moves only.
- `mod.rs` (~4,100 lines): extract along its existing seams — e.g. the main event loop,
  dialog request/response routing, app-level state transitions, and startup/teardown — into
  submodules (`event_loop.rs`, `dialog_router.rs`, etc.; name by what the code actually
  contains once seams are identified).
- `render.rs` (~2,900) and `tabs.rs` (~2,600): split per widget/region (the existing
  `container_view.rs` / `workflow_view.rs` / `git_sidebar.rs` files show the intended
  granularity).
- Keep `pub(crate)` visibility tight; the TUI package's external surface must not change.
- Do this AFTER any in-flight TUI work (WI 0096 touches `mod.rs`, `render.rs`, `tabs.rs`)
  has merged, to avoid conflict churn.

### D. API frontend shrinkage (follow-on to WI 0097)
- After WI 0097 lands, sweep `src/frontend/api/` (`routes.rs`, `queue_worker.rs`,
  `session_setup.rs`, `event_bus.rs`) for remaining logic that is not HTTP translation:
  anything computing business outcomes, orchestrating engines beyond a single Dispatch
  call, or duplicating logic that CLI/TUI obtain from Layer 2.
- Candidates identified in review: setup orchestration in `session_setup.rs` (ready-phase
  sequencing, remote-clone lifecycle including failure cleanup of cloned directories) and
  job/queue lifecycle rules in `queue_worker.rs`. Where a piece is genuinely API-mode-only
  *behavior* (not presentation), it belongs in Layer 2 as a command/service type that the
  API frontend calls — per the spec, API mode is "just another frontend."
- Treat shrinkage as the acceptance signal: `routes.rs` should trend toward route
  definitions + request/response mapping only. Record anything intentionally left in place
  (with rationale) in this work item's notes on completion.

## Edge Case Considerations:
- **A**: docker CLI versions and the Apple `container` CLI must both support name-only
  `-e` inheritance — verify on both backends before removing the `KEY=VALUE` form; if a
  backend does not support it, fall back to `--env-file` with a 0600 tempfile (RAII
  cleanup, same pattern as existing file-form secrets in `src/engine/overlay/`).
- **A**: credentials containing `=` or newlines must survive the env-inheritance path
  identically to the argv path.
- **B**: `--mount-ssh=value` form (with `=`) must still be intercepted, as today.
- **B**: the unknown-runtime TUI path must still construct inert default engines so the
  fatal modal can render (current documented behavior in `main.rs`).
- **C**: pure-move refactor discipline — if a split reveals a bug, note it and fix it
  separately; do not mix behavior changes into the reorganization commits.
- **D**: API clients must observe no route, schema, or status-code changes from the
  shrinkage sweep; this is internal relocation only.

## Test Considerations:
- **A — unit**: backend arg-construction tests assert credential values never appear in
  the built argv (assert `-e KEY` form) and that the child-process env map contains them;
  masking/display tests updated accordingly.
- **A — integration/e2e**: launch a container with a dummy credential and assert the
  variable is present inside the container with the exact value (including `=`/newline
  cases); on Linux, optionally assert `/proc/<docker-pid>/cmdline` contains no credential
  value during launch.
- **B — unit**: `Engines::detect` three-path coverage (see above); removed-flag helper
  returns the hint for `--mount-ssh` and `--mount-ssh=x`, and nothing for live flags.
- **B — e2e**: `awman --mount-ssh` prints the migration hint and exits 2 (unchanged);
  invalid `runtime:` config still fatal-errors CLI invocations and modals the TUI.
- **C**: full existing TUI test suite passes unchanged; no new tests required for a pure
  move, but any module gaining a public seam should gain a focused unit test.
- **D**: existing API integration/e2e suite passes unchanged; relocated Layer 2 types get
  their own unit tests per the testing spec.

## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from
  the project's aspec — in particular `aspec/architecture/2026-grand-architecture.md`
  (Layer 4 minimalism, Tenet 3 typed objects) and the module-size guidance in
  `aspec/foundation.md`.
- Item B's catalogue additions live in `src/command/dispatch/catalogue.rs` next to the
  live flag specs; the argv-scan helper is a projection like the others.
- Item D depends on WI 0097 and should be scheduled after it; items A–C are independent
  and can land in any order (C after WI 0096 merges).

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** if any user-visible output changes (none expected —
  these are internal hardening/cleanup changes)
- **Never create work-item-specific docs**
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.
