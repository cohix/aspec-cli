# Work Item: Task

Title: Porting old-amux remote-bound-tab capability
Issue: issuelink

## Summary:
old-amux had the ability to bind a tab in the TUI to a remote session such that every command executed would be run on the remote headless amux instance. This included listing available sessions from the configured default remote host in the new-tab dialog, automatically using the auth set up in the global config file, automatically running `ready` in the remote session when the tab opens, automatically using --follow semantics so that remote execution logs streamed to the execution window, etc. This work item should include research to ensure that new-amux headless server will behave identically to old-amux server and support all the same features, and ensuring that new-amux's TUI implementation of remote-bound tabs is feature complete compared to old-amux, that all config values etc. are used and work properly, and that general parity is achieved between new-amux and old-amux for remote commands, headless server, and remote-bound TUI tabs.

## User Stories

### User Story 1:
As a: user with a remote headless amux server configured

I want to: open a new TUI tab bound to an active session on my remote host by pressing Ctrl+T and selecting from the live session list

So I can: run amux commands against the remote session without any `remote run` prefix or `--session` flag, with output streamed directly into my TUI execution window exactly as if the session were local

### User Story 2:
As a: user working across multiple machines

I want to: have my remote-bound TUI tab automatically run `ready` when it opens, show the remote host name in the tab bar instead of a local directory, and render all output via the same SSE log-streaming that `remote run --follow` uses

So I can: immediately know the remote session is healthy, see at a glance which tabs are local and which are remote, and watch agent output stream in real time without any manual steps

### User Story 3:
As a: developer or operator

I want to: know that new-amux's headless server is fully compatible with old-amux's server behavior (session lifecycle, command dispatch, SSE streaming, auth, workflow state) and that all remote config values (`remote.defaultAddr`, `remote.defaultAPIKey`, `remote.savedDirs`) are correctly used in every relevant TUI flow

So I can: migrate from old-amux to new-amux with no loss of capability and no manual workarounds for remote workflows


## Implementation Details:

### Phase 0 — Parity Research

Before writing code, audit new-amux against old-amux for:

- **Headless server API surface**: compare every HTTP endpoint new-amux exposes (`GET /v1/status`, `/v1/workdirs`, `/v1/sessions`, `/v1/sessions/:id`, `/v1/commands`, `/v1/commands/:id`, `/v1/commands/:id/logs`, `/v1/commands/:id/logs/stream`, `/v1/workflows/:id`) against old-amux. Document any endpoints old-amux had that new-amux is missing.
- **Auth model parity**: verify SHA-256 Bearer token auth, `--dangerously-skip-auth`, `--refresh-key`, `api_key.hash` persistence, constant-time comparison — confirm all match old-amux behavior.
- **SSE streaming**: verify `[amux:done]` sentinel, historical log replay, incremental write-and-tail, `Content-Type: text/event-stream` match old-amux.
- **Session lifecycle**: `active`/`closed` states, `?status=active` filter, per-session command queue (one command at a time), HTTP 403 `session busy` — confirm parity.
- **Remote CLI commands**: `remote run --follow`, `remote session start`, `remote session kill` — confirm flag handling (including the `remote.defaultAPIKey` host-match guard) matches old-amux behavior.
- **Config field names**: `defaultAddr`, `defaultAPIKey`, `savedDirs` — confirm JSON key names and precedence rules (flag > env > config) match old-amux.
- Record findings in the work item and open separate issues for any gaps found.

### Phase 1 — Extend Tab for Remote Binding

Extend `Tab` in `src/frontend/tui/tabs.rs` with remote binding fields:

```
remote_addr: Option<String>         // bound remote host URL
remote_session_id: Option<String>   // bound session UUID
remote_api_key: Option<String>      // resolved API key at binding time
display_host: Option<String>        // "host:port" extracted from remote_addr for the tab label
```

Set `is_remote = true` whenever `remote_session_id` is `Some`. These fields are `None` for all local tabs and are never modified after the tab is created — the binding is permanent for the tab's lifetime.

Add a `Tab::new_remote(addr, session_id, api_key)` constructor (or similar) that populates all four fields and sets `is_remote = true`. `display_host` is extracted once at construction from `remote_addr` (strip scheme, strip path, keep `host:port`).

Update `Tab::new` to initialize all four new fields as `None` (no behavioral change for local tabs).

### Phase 2 — New-Tab Dialog: Remote Session List

Extend the new-tab dialog in `src/frontend/tui/mod.rs` (the flow that currently leads to `handle_new_tab_path`):

When `effective_config.remote_default_addr()` returns `Some(addr)`, the new-tab dialog gains a remote session section. The fetch of active sessions (`GET /v1/sessions?status=active`) must be asynchronous and non-blocking — the dialog opens immediately and the workdir field is focusable before the fetch completes.

**Dialog layout** (see `docs/09-remote-mode.md` for the exact ASCII mockup):
- Top: workdir `TextInput` field (existing behavior)
- Middle: `"─── Remote sessions (<host>) ───"` separator, then the live session list once loaded; `"  Loading remote sessions…"` while in-flight; error message on failure
- Bottom: `"  + Create new remote session"` list item (always shown once the section is visible)

**Key bindings**:
- `↓` from the workdir field → move focus to the remote session list
- `↑` from the top of the remote session list → return focus to the workdir field
- `Enter` with workdir field focused → open a local tab (unchanged behavior)
- `Enter` with a remote session selected → call `handle_new_remote_tab(app, addr, session_id, api_key)` (new function)
- `Enter` with `+ Create new remote session` selected → transition to the session-creation sub-modal (see below)
- `Esc` → cancel

**Fetch failure** is non-fatal. Show the error message in the remote session section; the user can still open a local tab by pressing `Enter` with the workdir field focused.

**Session-creation sub-modal** (triggered by `+ Create new remote session`):
- Text field for the remote working directory
- List of `remote.savedDirs` from config (if any)
- `Enter` → `POST /v1/sessions` with the chosen dir; on success, call `handle_new_remote_tab`; on failure, show error text
- `Esc` → return to the new-tab dialog

Only **active** sessions are shown. The session ID is truncated if needed to preserve the full workdir path.

If `remote.defaultAddr` is not configured, the dialog shows no remote session section and behaves exactly as before.

### Phase 3 — Remote-Bound Tab Command Dispatch

Add `handle_new_remote_tab(app, addr, session_id, api_key)` in `src/frontend/tui/mod.rs`. This function:
1. Creates a `Tab` via the new remote constructor
2. Appends it to `app.tabs`, sets it as the active tab
3. Immediately auto-dispatches `ready` to the remote session using the SSE-streaming path (see Phase 4)

For command dispatch from a remote-bound tab, introduce a new code path that:
1. Detects `tab.is_remote` before the normal local dispatch
2. Strips `--session`, `--remote-addr`, and `--api-key` flags from the user's input
3. Sends `POST /v1/commands` with the `x-amux-session` header set to `tab.remote_session_id` and the `Authorization` header set from `tab.remote_api_key`
4. Receives the `command_id`
5. Opens the SSE stream (`GET /v1/commands/<id>/logs/stream`) and feeds each log line into the tab's vt100 parser / execution window, identical to how local container stdout is handled
6. On `[amux:done]`, closes the stream and transitions the tab's `ExecutionPhase` to `Done` or `Error` based on the command's final status

Use `RemoteClient` from `src/command/commands/remote_client.rs` for all HTTP calls. The `resolve_api_key` logic in `RemoteClient` already handles the host-match guard for `remote.defaultAPIKey`; use it consistently.

The tab transitions through the same `ExecutionPhase` states (`Running → Done / Error`) as a local tab. The execution window renders exactly as it does for local commands.

### Phase 4 — RemoteCommandFrontend TUI Implementation

Replace the empty `impl RemoteCommandFrontend for TuiCommandFrontend {}` in `src/frontend/tui/per_command/remote.rs` with full implementations:

- **`ask_session_picker`**: fetch `GET /v1/sessions?status=active` from the configured remote addr, build a `ListPicker` dialog showing `"<truncated-id>  <workdir>"` per session, return the selected session ID. If the remote host has no active sessions, show a message and return `Err(CommandError::Cancelled)`.
- **`ask_saved_dir_picker`**: read `remote.savedDirs` from `EffectiveConfig`, show a `ListPicker` dialog, return the selected directory. If no saved dirs, return the appropriate error described in `docs/09-remote-mode.md`.
- **`ask_session_kill_picker`**: same as `ask_session_picker` but with title `"Kill Session"`.
- **`confirm_save_dir`**: show an inline `y/n` prompt (can use `TextInput` dialog with hint text), return `true` if the user presses `y`.

These implementations use the existing `DialogRequest`/`DialogResponse` channel that `TuiCommandFrontend` already has wired up. The TUI remembers the last-used session ID for the remainder of a tab's lifetime (session resolution priority 3 from `docs/09-remote-mode.md`).

### Phase 5 — Tab Label and Appearance

Update the tab bar renderer in `src/frontend/tui/tabs.rs`:
- When `tab.is_remote`, display `tab.display_host` (e.g. `"1.2.3.4:9876"`) as the tab label instead of the local workdir short name
- Color remains `Color::Magenta` (already implemented; the docs describe this as "purple" — Magenta is the correct ratatui color, matching the intended visual)
- Inner subtitle (below the host name): show the subcommand currently running on the remote session, or `"(ready)"` when idle

### Phase 6 — Remote Workflow Strip

When a command is dispatched from a remote-bound tab, start a background task that:
1. Waits 5 seconds after dispatch
2. Polls `GET /v1/workflows/<command_id>` on the remote host every 5 seconds
3. On HTTP 200, updates `tab.workflow_state` with the parsed state — the existing workflow strip renderer picks this up unchanged
4. On HTTP 404 (first poll) → stop silently (not a workflow command)
5. On HTTP 404 (after a prior 200) → stop polling (workflow state removed)
6. On transient network error → retry on the next interval, do not surface to user
7. On `complete` or `error` status → stop polling

Cancel the poll task when the tab is closed or when a new command is dispatched from the same tab. This matches the local workflow strip behavior exactly.

### Phase 7 — Config Integration Audit

Verify that all `EffectiveConfig` remote accessors are exercised in the TUI paths:
- `remote_default_addr()` — used in new-tab dialog fetch and in remote-bound tab construction
- `remote_default_api_key()` — used via `RemoteClient::resolve_api_key` (already applies host-match guard)
- `remote_saved_dirs()` — used in `ask_saved_dir_picker` and in the session-creation sub-modal
- `remote_session()` — used in the existing `remote run` command resolution; verify it is still respected

Write a test that exercises the host-match guard: when `--remote-addr` differs from `remote.defaultAddr`, `remote.defaultAPIKey` must not be forwarded.


## Edge Case Considerations:

- **`remote.defaultAddr` not configured**: new-tab dialog shows no remote section; all existing local-tab behavior unchanged. No fetch attempted.
- **Remote host unreachable during Ctrl+T**: show `"  ⚠ Could not reach <host>: <error>"` in the remote section; dialog stays open; user can still open a local tab.
- **Auth required but no key**: show `"  ⚠ Auth required for <host>. Set remote.defaultAPIKey or pass --api-key."` in the remote section; non-fatal.
- **Multiple Ctrl+T presses while fetch is in-flight**: cancel previous fetch, start fresh.
- **No active sessions on remote host**: session picker shows `"No active sessions on <host>. Run 'remote session start' first."`; Enter and Esc both cancel.
- **Session closed externally while tab is open**: next `POST /v1/commands` returns HTTP 404; show error in execution window; tab stays bound to the now-invalid session; subsequent commands fail until the session is recreated.
- **Session becomes busy**: `POST /v1/commands` returns HTTP 403 `"session busy"`; show error; do not retry automatically.
- **Auth failure at command dispatch from remote-bound tab**: show error in execution window; no `command_id` returned; workflow polling does not start; tab stays bound.
- **User types `--session`, `--remote-addr`, or `--api-key` in a remote-bound tab**: strip these flags before forwarding; the tab's binding supplies the target.
- **Tab closed while SSE stream in-flight**: cancel the stream task immediately; the remote command continues executing on the server unaffected.
- **New command dispatched while workflow poll is active**: cancel the previous poll task; start a new one 5 seconds after the new dispatch.
- **Session-creation sub-modal: dir not in server allowlist**: server returns HTTP 403; show error text in the sub-modal; no tab created; user presses Esc and retries.
- **`remote.defaultAPIKey` host-match guard**: if `remote_addr` used for binding differs from `remote.defaultAddr` (e.g. overridden by env or flag at an earlier point), the stored key is not forwarded. The resolved key is captured at binding time and stored in `tab.remote_api_key`.
- **Trailing slash normalization**: strip trailing slashes from both sides when matching `remote.defaultAddr` against the target address for the API key guard.
- **Session ID not in saved dirs when creating via sub-modal**: offer to save the new dir to `remote.savedDirs` (matching `confirm_save_dir` behavior); start the session regardless of the user's choice.
- **`remote.savedDirs` empty with no dir argument in `remote session start` (TUI)**: error in command input bar; `ask_saved_dir_picker` returns `Err`.
- **Old-amux parity gaps found in Phase 0**: each gap should be filed as a separate issue; this work item addresses the TUI layer only — server-side gaps are out of scope here unless the parity research reveals blocking issues for TUI functionality.
- **Workflow strip: `GET /v1/workflows/:command_id` returns HTTP 404 on first poll**: polling stops silently; the execution window shows only the SSE log stream; no strip appears.
- **`--follow` timeout (10 minutes of silence)**: if the SSE connection to a remote-bound tab's command times out, show the timeout message in the execution window and transition to an error state.


## Test Considerations:

- **Unit — Tab remote fields**: `Tab::new_remote` sets `is_remote = true`, all fields populated, `display_host` correctly extracted from various URL forms (with/without port, with/without trailing slash, loopback vs. hostname).
- **Unit — `display_host` extraction**: cover `http://1.2.3.4:9876/`, `https://build.example.com`, `http://localhost:9876` — verify scheme stripped, path stripped, port preserved when present.
- **Unit — `RemoteCommandFrontend` TUI impl (mocked HTTP)**: `ask_session_picker` shows sessions from a mocked `GET /v1/sessions?status=active` response; returns the selected ID; handles empty session list; handles HTTP 401.
- **Unit — flag stripping**: verify that `--session`, `--remote-addr`, and `--api-key` are removed from forwarded command args; verify that other flags (e.g. `--yolo`, `--non-interactive`) pass through unchanged.
- **Unit — host-match guard**: `resolve_api_key` returns `None` (not the stored key) when the target address differs from `remote.defaultAddr`; returns the stored key when they match (after trailing slash normalization).
- **Unit — new-tab dialog remote section**: when `remote.defaultAddr` is configured, the dialog state includes the remote section; when not configured, the section is absent.
- **Integration — remote-bound tab creation**: simulate selecting a remote session in the new-tab dialog; verify `Tab` is created with correct remote fields; verify `ready` is auto-dispatched.
- **Integration — remote command dispatch**: remote-bound tab dispatches a command; verify `POST /v1/commands` is called with correct headers; verify SSE log lines feed into the tab's output; verify `[amux:done]` transitions `ExecutionPhase` to `Done`.
- **Integration — workflow polling**: remote-bound tab dispatches a workflow command; verify polling starts after 5 s; verify `workflow_state` on the tab is updated from the mock HTTP response; verify polling stops on `complete`.
- **Integration — fetch failure graceful**: `GET /v1/sessions` returns a connection error; new-tab dialog shows the error message; local tab creation still works.
- **Integration — session picker dialog flow**: `remote run` without `--session` in TUI; verify `ask_session_picker` opens `ListPicker`; verify selected session is used for dispatch.
- **Integration — `confirm_save_dir`**: `remote session start` with a new dir in TUI; verify save prompt appears; verify `y` saves the dir to config; verify `n` skips saving but starts the session.
- **E2E (manual)**: start a real headless server; open TUI; press Ctrl+T; verify session list loads; select a session; verify remote-bound tab opens with `ready` output streaming; run `implement <N>` in the remote-bound tab; verify SSE output streams and workflow strip appears.


## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- **`src/frontend/tui/tabs.rs`**: extend `Tab` struct with `remote_addr`, `remote_session_id`, `remote_api_key`, `display_host`; add `Tab::new_remote` constructor; update tab bar renderer for `display_host` label.
- **`src/frontend/tui/mod.rs`**: extend the new-tab dialog flow to support the remote session list and session-creation sub-modal; add `handle_new_remote_tab`; add remote command dispatch path that detects `tab.is_remote` and routes to SSE-based execution.
- **`src/frontend/tui/per_command/remote.rs`**: implement all four `RemoteCommandFrontend` methods for `TuiCommandFrontend` using the existing `DialogRequest`/`DialogResponse` channel.
- **`src/command/commands/remote_client.rs`**: no changes expected; use `RemoteClient` as-is for all HTTP calls from the TUI paths.
- **`src/data/config/effective.rs`**: no changes expected; all remote accessors (`remote_default_addr`, `remote_default_api_key`, `remote_saved_dirs`, `remote_session`) are already present — the task is to use them consistently in new TUI paths.
- **`src/frontend/tui/dialogs.rs`** (or equivalent dialog types file): may need a new `Dialog` variant for the new-tab remote section if the existing `TextInput` + `ListPicker` variants are insufficient to represent the compound new-tab dialog state.
- Async fetch tasks in the TUI must use the existing async runtime plumbing (tokio tasks + channel messages back to the event loop) — do not block the render loop.
- The SSE log stream for remote-bound tab execution must feed into the same `container_stdout_rx` / vt100 parser pipeline that local container output uses, so rendering is identical.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update `docs/09-remote-mode.md`** to reflect any behavior changes or additions discovered during parity research (Phase 0); ensure the remote-bound TUI tab section accurately describes the final implemented behavior including the new-tab dialog UI, tab label format, and workflow strip behavior.
- **Update `docs/08-headless-mode.md`** if parity research (Phase 0) reveals server-side additions or corrections needed.
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0076 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.
