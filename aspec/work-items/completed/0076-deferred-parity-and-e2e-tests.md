# Work Item: Task

Title: deferred parity and end-to-end tests from WI 0073
Issue: n/a — follow-up to `0073-grand-architecture-finalize.md`

## Prerequisites

- WI 0073 is complete (the four-layer architecture is in place; `tests/`, `make architecture-lint`, and the parity reports exist).
- A medium-coverage set of e2e tests was added during WI 0073 finalization (one real-Docker container test, a real-git worktree-cycle test, a workflow-state fixture loader, and one live-headless-server route test). This work item picks up everything that was deliberately deferred.

The implementing agent MUST read:

- `aspec/work-items/0073-grand-architecture-finalize.md` — especially §1a (proposed `tests/` layout) and §2e (parity validation matrix).
- `aspec/review-notes/0073-parity-validation.md` — the rows currently marked `NOT-COVERED` are the work to do here.
- `aspec/architecture/2026-grand-architecture.md` — the architectural source of truth.

## Summary

- Build out the deferred test tiers from WI 0073 §1a: `tests/tui_parity/`, the missing `tests/engine/` real-system files, and the `tests/binary_smoke/{tui,headless}_subprocess.rs` files.
- Convert WI 0073's hardcoded route table and synthesized SQLite fixture into real captured fixtures and live-server / on-disk-fixture tests.
- Update `aspec/review-notes/0073-parity-validation.md` rows from `NOT-COVERED` to `PASS` / `MINOR-DRIFT` / `REGRESSION` as each test lands.

## User Stories

### User Story 1:
As a: maintainer
I want to: have a TUI parity test suite that fails CI when behavior drifts
So I can: catch UI regressions automatically rather than via manual smoke testing.

### User Story 2:
As a: maintainer
I want to: have real-system engine tests (Docker, git, TLS) that exercise the new engines end-to-end
So I can: catch container / git / auth regressions before they reach users.

### User Story 3:
As a: maintainer upgrading a user's amux install
I want to: have captured legacy fixtures for SQLite and workflow state
So I can: prove that previously-written on-disk data still loads correctly after the refactor.

---

## Implementation Details

### 1. TUI parity suite (`tests/tui_parity/`)

Build the tier described in WI 0073 §1a using a vt100/expect-style harness. Drive time with `tokio::time::pause` so snapshots are deterministic; never take wall-clock readings.

Coverage at minimum (parity matrix items 47–68 from WI 0073 §2e):

- `startup_and_tabs.rs` — initial tabs, tab opening/closing/switching, color matrix
- `command_box.rs` — command parsing, completions, hint surface
- `workflow_dialog.rs` — control board key paths
- `yolo_countdown.rs` — 30s stuck → countdown → 60s auto-advance
- `keyboard_shortcuts.rs` — every documented shortcut in `aspec/uxui/cli.md`
- `rendering_snapshots.rs` — golden frames for steady state
- `new_dialogs.rs`, `config_show_dialog.rs`, `claws_dialogs.rs`, `worktree_dialogs.rs`

Each test must include "tui_parity" or "vt100" in the test function name so `make test-fast` can include it (TUI tests are hermetic — no Docker required).

### 2. Real-system engine tests (`tests/engine/`)

Add the missing files from WI 0073 §1a, gated behind `helpers::docker_skip!` / `real_git_skip!`:

- `container_docker.rs` — real Docker spawn, stream, cancel, stats, list_running, stop
- `container_apple.rs` — `#[cfg(target_os = "macos")]`, real `container` CLI
- `ready_engine.rs` — full `ReadyPhase` state machine against a real repo
- `init_engine.rs` — full `InitPhase` against a fresh git repo
- `claws_engine.rs` — every `ClawsMode` end-to-end
- `agent_engine.rs` — `ensure_available` download → build → idempotent
- `worktree_lifecycle.rs` — prepare → run → finalize cycle
- `auth_engine_tls.rs` — rustls cert generation, fingerprint stability, key rotation

Existing `tests/engine/git_engine.rs` `real_git_*` tests should stop swallowing failures (`if let Ok(...)`) and fail loudly when git is available but resolution returns an error.

### 3. Binary subprocess smoke (`tests/binary_smoke/`)

Add `tui_subprocess.rs` and `headless_subprocess.rs` per WI 0073 §1a:

- `tui_subprocess.rs` — boot the TUI in a PTY harness, send input, capture screen, send quit. Hermetic (no Docker).
- `headless_subprocess.rs` — boot `amux headless start` on an ephemeral loopback port, hit every documented route with `reqwest`, kill cleanly. Real-network gated.

### 4. Captured fixtures

Replace the hand-rolled SQLite fixture in `tests/data_layer/sqlite_upgrade_compat.rs` with a captured database file checked in at `tests/fixtures/sqlite_upgrade/<version>.db`. To capture: run `amux headless start` once on the prior release, copy `~/.amux/headless/amux.db` into the fixture, document the source version in a sibling `README.md`.

Wire `tests/fixtures/workflow_state/v1.json` into a real test (currently it sits unread).

Add one fixture per supported headless API contract: a captured SSE event sequence (`tests/fixtures/headless_sse/<scenario>.sse`) that the live-server test asserts byte-for-byte.

### 5. Update parity report

After each test lands, change the corresponding row in `aspec/review-notes/0073-parity-validation.md` from `NOT-COVERED` to `PASS` / `MINOR-DRIFT` / `REGRESSION`. The report is the running scoreboard for this work.

### 6. Coverage delta

Run `cargo llvm-cov` (or the project's chosen coverage tool) before and after this work item; record the diff in `aspec/review-notes/0076-coverage-delta.md`. Spec target: net positive coverage on `src/engine/` and `src/frontend/`.

---

## Edge Case Considerations

- **vt100 harness determinism**: any test that depends on real time will flake. Use `tokio::time::pause` consistently. Snapshot tests must be golden-file driven so a developer can update them with `cargo test -- --snapshot-update` (or equivalent).
- **Docker availability on CI runners**: GitHub-hosted Linux runners have Docker; macOS hosted runners do not. Apple-container tests must skip with a clear message on Linux/Windows.
- **Captured SQLite fixtures**: the database file is binary and may bloat the repo. If size concerns grow, gate the test with `cfg(test)` and decompress at test time.
- **TUI tests in CI**: ensure the test binary uses a fake terminal (`portable-pty` or similar) — running ratatui without a real TTY surface would otherwise fail.
- **Headless live-server tests**: must bind to `127.0.0.1` on an ephemeral port (`:0`) to avoid CI port collisions.

---

## Test Considerations

- All new tests run under `cargo test`.
- Real-Docker tests must be gated by `docker_skip!`; real-git tests by `real_git_skip!`; network tests by `real_network_skip!`.
- `make test-fast` continues to skip docker/real_git/real_network; `make test-full` runs everything.
- TUI tests are part of `make test-fast` because they're hermetic.

---

## Codebase Integration

- Follow `aspec/architecture/2026-grand-architecture.md` and `tests/helpers/mod.rs` conventions.
- Do not touch `oldsrc/` — it remains frozen until the developer deletes it manually.
- Keep test helpers DRY: extract `vt100_harness`, `live_headless_server`, `captured_sqlite` into `tests/helpers/` rather than duplicating across files.

---

## Documentation

After implementation:

- Update `aspec/review-notes/0073-parity-validation.md` so every row has a verdict.
- If a new test category warrants it, add a paragraph to `docs/10-architecture-overview.md` describing how it's structured.
- Do NOT create a "WI 0076 implementation guide" doc — implementation lives in this spec.
