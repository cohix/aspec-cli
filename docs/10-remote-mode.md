# Remote Mode

Remote mode lets you connect to an API awman server running on another machine and run commands there — from your terminal, from a CI pipeline, or from inside the TUI. Live log streaming lets you watch agent output in real time, exactly as if the session were local.

---

## Overview

A typical setup has one machine running `awman api start` (the _remote host_), and one or more developers or pipelines using `awman remote` (the _client_) to dispatch work to it. The remote host manages all sessions and containers. The client only needs the server's address.

```
Local machine                                    Remote host
──────────────                                   ──────────────────────────
awman remote exec workflow my.toml -f ──────────► POST /v1/commands
                                                 ◄── SSE stream: log output
                                                 ◄── [awman:done] sentinel
```

Four subcommands cover the full lifecycle:

| Command | What it does |
|---------|-------------|
| `awman remote exec workflow <file>` | Submit a workflow to a session on the remote host |
| `awman remote exec prompt <prompt>` | Send a one-shot prompt to a session on the remote host |
| `awman remote session start [dir]` | Create a new session on the remote host |
| `awman remote session kill [session-id]` | Close a session on the remote host |

All subcommands work from the terminal (CLI mode) and from inside the TUI (where interactive pickers are also available).

---

## Connecting to a remote host

Every `remote` subcommand needs to know the address of the remote API server. The address is resolved in this order:

| Priority | Source |
|----------|--------|
| 1 | `--remote-addr <URL>` flag on the command |
| 2 | `AWMAN_REMOTE_ADDR` environment variable |
| 3 | `remote.defaultAddr` in `~/.awman/config.json` |

If none of the three sources provides an address, the command fails immediately with:

```
error: No remote address configured. Pass --remote-addr, set AWMAN_REMOTE_ADDR,
       or set remote.defaultAddr in ~/.awman/config.json.
```

The most convenient setup for day-to-day use is to set a default address once:

```sh
awman config set --global remote.defaultAddr http://build-server.example.com:9876
```

After that, every `awman remote` command targets that host by default, with no flag required.

---

## API key authentication

When the remote API server has authentication enabled (the default), every request must include the API key. The key is resolved automatically from one of these sources, in priority order:

| Priority | Source |
|----------|--------|
| 1 | `--api-key <KEY>` flag on the command |
| 2 | `AWMAN_API_KEY` environment variable |
| 3 | `remote.defaultAPIKey` in `~/.awman/config.json` — **only when the target address exactly matches `remote.defaultAddr`** |

The host-match guard on `remote.defaultAPIKey` prevents a stored key from being silently forwarded to a different server. If you change `--remote-addr` or `AWMAN_REMOTE_ADDR` to point at a different host, the config key is ignored and the request proceeds without auth — resulting in an HTTP 401 if that server requires a key.

### Storing the key

The recommended approach for day-to-day use is to store the key in global config:

```sh
awman config set --global remote.defaultAPIKey <your-api-key>
```

With both `remote.defaultAddr` and `remote.defaultAPIKey` configured, every `awman remote` command to the default host is fully authenticated with no extra flags.

For CI pipelines, use the environment variable:

```sh
export AWMAN_REMOTE_ADDR=http://build-server.internal:9876
export AWMAN_API_KEY=<your-api-key>

awman remote exec workflow aspec/workflows/implement-feature.toml --follow
```

### Security note

`AWMAN_API_KEY` is visible in `/proc/<pid>/environ` on Linux. In security-sensitive contexts, prefer passing the key via `--api-key` piped from a secrets manager, or store it in `~/.awman/config.json` with restricted file permissions (`chmod 600 ~/.awman/config.json`).

---

## `awman remote exec workflow`

Submits a workflow file to a session on the remote host.

```sh
awman remote exec workflow <file> [--session <ID>] [--follow] [--remote-addr <URL>] [exec workflow flags]
```

Accepts the same flags as `awman exec workflow` (except `--worktree`, which is a server-side concern), plus the remote transport flags below.

### Basic usage

```sh
# Submit a workflow; return a command ID immediately
awman remote exec workflow path/to/workflow.toml --session abc123

# Stream output to your terminal until complete
awman remote exec workflow path/to/workflow.toml --session abc123 --follow

# Short form for --follow
awman remote exec workflow path/to/workflow.toml --session abc123 -f

# Pass exec workflow flags through unchanged
awman remote exec workflow path/to/workflow.toml --yolo --work-item 0042 --session abc123 -f
```

### Flags

| Flag | Short | Description |
|------|-------|-------------|
| `--session <ID>` | | Session ID on the remote host. Required in CLI mode; interactive in TUI mode. Overrides `AWMAN_REMOTE_SESSION` |
| `--follow` | `-f` | Stream log output until the command completes, then print a summary table |
| `--remote-addr <URL>` | | Remote host address. Overrides `AWMAN_REMOTE_ADDR` and `remote.defaultAddr` |
| `--api-key <KEY>` | | API key for the remote server. Overrides `AWMAN_API_KEY` and `remote.defaultAPIKey` |

Plus all `exec workflow` flags (except `--worktree`): `--work-item`, `--yolo`, `--auto`, `--plan`, `--agent`, `--model`, `--overlay`, `--issue`, `-n/--non-interactive`.

### Session resolution

The session is resolved in this order:

| Priority | Source |
|----------|--------|
| 1 | `--session <ID>` flag |
| 2 | `AWMAN_REMOTE_SESSION` environment variable |
| 3 | TUI only — last session used in this tab (not available in CLI) |
| 4 | TUI only — interactive session picker |

In CLI mode, if neither `--session` nor `AWMAN_REMOTE_SESSION` is set, the command fails with:

```
error: No session specified. Pass --session <ID> or set AWMAN_REMOTE_SESSION.
       Use `awman remote session start` to create a session, or list sessions
       with `curl <remote-addr>/v1/sessions`.
```

### Live log streaming (`--follow`)

Without `--follow`, the command submits the workflow and returns immediately with the command ID:

```
Command dispatched: e5f6a7b8-...
```

With `--follow`, awman connects to the SSE log-streaming endpoint and relays output to your terminal in real time:

```
Connecting to log stream for e5f6a7b8-... on http://build-server.example.com:9876...
Implementing work item 0059...
✓ Tests pass
✓ Cargo build successful
...
```

Once the command completes, awman prints a summary table and exits:

```
┌──────────────┬────────────────────────────────────────┐
│ Field        │ Value                                  │
├──────────────┼────────────────────────────────────────┤
│ Command ID   │ e5f6a7b8-…                             │
│ Session ID   │ abc123                                 │
│ Status       │ done                                   │
│ Exit Code    │ 0                                      │
│ Started      │ 2026-04-22T10:00:00Z                   │
│ Finished     │ 2026-04-22T10:02:31Z                   │
└──────────────┴────────────────────────────────────────┘
```

If the command had already completed before you connected, `--follow` replays the full historical log and then prints the summary immediately — there is no gap or missed output.

When output is piped to a non-terminal, log lines are written without ANSI decoration.

---

## `awman remote exec prompt`

Sends a one-shot prompt to a session on the remote host.

```sh
awman remote exec prompt <prompt> [--session <ID>] [--follow] [--remote-addr <URL>] [exec prompt flags]
```

Accepts the same flags as `awman exec prompt` plus the remote transport flags.

```sh
# Submit a prompt; return a command ID immediately
awman remote exec prompt "Fix the failing tests" --session abc123

# Stream output to your terminal until complete
awman remote exec prompt "Fix the failing tests" --session abc123 --follow

# Pass exec prompt flags through
awman remote exec prompt "Fix the tests" --yolo --agent claude --session abc123 -f
```

Session resolution and `--follow` behavior are identical to `remote exec workflow`.

---

## `awman remote session start`

Creates a new session on the remote host.

```sh
awman remote session start [dir] [--remote-addr <URL>] [--api-key <KEY>]
```

`dir` is the absolute path (on the remote host) of the working directory for the session. It must be in the remote host's `--workdirs` allowlist.

```sh
# Start a session bound to /home/user/my-project
awman remote session start /home/user/my-project

# Specify a non-default remote host and key
awman remote session start /home/user/my-project \
  --remote-addr http://alt-host:9876 \
  --api-key <key>
```

On success, awman prints the new session ID:

```
Session started: a1b2c3d4-e5f6-7890-abcd-ef1234567890
Workdir: /home/user/my-project
```

In CLI mode, `dir` is required. In TUI mode, `dir` is optional — if omitted and saved directories are configured, awman shows an interactive picker (see [TUI interactive flows](#tui-interactive-flows)).

---

## `awman remote session kill`

Closes a session on the remote host.

```sh
awman remote session kill [session-id] [--remote-addr <URL>] [--api-key <KEY>]
```

```sh
# Kill a specific session
awman remote session kill a1b2c3d4-e5f6-7890-abcd-ef1234567890

# Specify a non-default remote host and key
awman remote session kill abc123 \
  --remote-addr http://alt-host:9876 \
  --api-key <key>
```

On success:

```
Session closed: a1b2c3d4-e5f6-7890-abcd-ef1234567890
```

In CLI mode, `session-id` is required. In TUI mode, if omitted, awman fetches the active session list from the remote host and shows an interactive picker.

---

## TUI interactive flows

When used inside the awman TUI, `remote` subcommands gain interactive capabilities that are not available in CLI mode.

### Session picker (`remote exec` without `--session`)

If you run `remote exec workflow` or `remote exec prompt` in the TUI without specifying a session, and no session is stored from previous activity in the current tab, awman fetches the **active** session list from the remote host (using `GET /v1/sessions?status=active`) and displays an interactive picker. Closed sessions are never shown in the picker.

The picker dialog has a dynamic width that adjusts to fit long session IDs and working directory paths. Session IDs are truncated with `…` if necessary to preserve the full workdir path — the part most useful for identification.

```
╭─── Select Session ───────────────────────────────────────────────╮
│                                                                    │
│   ▶  a1b2c3d4  /home/user/my-project                              │
│      b2c3d4e5  /home/user/other-project                           │
│                                                                    │
│  ↑↓ navigate  Enter confirm  Esc cancel                            │
╰────────────────────────────────────────────────────────────────────╯
```

Use `↑` / `↓` to highlight a session and `Enter` to confirm. The TUI remembers your choice for the rest of the tab's lifetime — subsequent `remote exec` commands in the same tab skip the picker and use the remembered session automatically.

If the remote host has no active sessions, the picker displays:

```
No active sessions on http://build-server.example.com:9876. Run `remote session start` first.
```

### Saved-dir picker (`remote session start` without a directory)

If you run `remote session start` in the TUI without a directory argument, and `remote.savedDirs` is configured, awman shows a directory picker:

```
╭─── Select Directory ───────────────────────────────────────────────────────╮
│                                                                              │
│   ▶  /home/user/my-project                                                   │
│      /home/user/other-project                                                │
│      /opt/builds/service-a                                                   │
│                                                                              │
│  ↑↓ navigate  Enter confirm  Esc cancel                                      │
╰──────────────────────────────────────────────────────────────────────────────╯
```

If no saved directories are configured and no directory argument is given:

```
error: No directory specified and no savedDirs configured.
       Pass a directory argument or add paths via: config set remote.savedDirs --global
```

### Save-dir prompt (`remote session start` with a new directory)

When you start a session with a directory that is not in `remote.savedDirs`, the TUI offers to save it:

```
╭────────────────────────────────────────────────────────────────╮
│  Save '/home/user/new-project' to remote.savedDirs? (y/n)      │
╰────────────────────────────────────────────────────────────────╯
```

- `y` — saves the path to `remote.savedDirs` in your global config, then starts the session
- `n` or `Enter` — starts the session without saving
- `Esc` — cancels the session start entirely

### Session kill picker (`remote session kill` without a session ID)

If you run `remote session kill` in the TUI without a session ID, awman fetches the active session list (`?status=active`) and shows a picker titled "Kill Session". Closed sessions are not listed. Navigation is identical to the session picker above.

---

## Remote-bound TUI tabs

When `remote.defaultAddr` is configured in `~/.awman/config.json`, the TUI's **new-tab dialog** (opened with **Ctrl+T**) can permanently bind a new tab to a remote API session. Every command typed in a remote-bound tab is forwarded to the remote host via the API — no `remote exec` prefix or `--session` flag required.

### Creating a remote-bound tab

Press **Ctrl+T**. When `remote.defaultAddr` is configured, awman asynchronously fetches the list of active sessions from the remote host and displays them below the working directory field:

```
┌──── New Tab ─────────────────────────────────────────────┐
│  Working directory:                                       │
│  [ /workspace/myproject                               ]   │
│                                                           │
│  ─── Remote sessions (1.2.3.4:9876) ───────────────────  │
│    abc123  /workspace/proj-a                              │
│  > def456  /workspace/proj-b          ← selected         │
│    + Create new remote session                            │
│                                                           │
│  [Enter] confirm  [Esc] cancel  [↓] move to remote list  │
└───────────────────────────────────────────────────────────┘
```

While the fetch is in-flight, the list area shows `"  Loading remote sessions…"`. The dialog does not block — you can open a local tab immediately by pressing **Enter** in the workdir field if the remote host is slow to respond.

Only **active** sessions are shown; closed sessions are excluded.

| Key | Action |
|-----|--------|
| **↓** (workdir field focused) | Move focus to the remote session list |
| **↑ / ↓** (list focused) | Navigate the session list |
| **↑** (top of list, list focused) | Return focus to the workdir field |
| **Enter** (workdir field focused) | Open a local tab with that working directory — unchanged behavior |
| **Enter** (session selected) | Create a new tab permanently bound to that remote session |
| **Enter** (`+ Create new remote session` selected) | Open the create-session sub-modal |
| **Esc** | Cancel the entire modal |

**Fetch failure messages** (non-fatal — a local tab can still be opened):

| Situation | Message shown |
|-----------|---------------|
| Remote host unreachable | `"  ⚠ Could not reach <host>: <error>"` |
| Auth required but no key configured | `"  ⚠ Auth required for <host>. Set remote.defaultAPIKey or pass --api-key."` |

### Creating a new remote session from the dialog

Selecting **"+ Create new remote session"** transitions the dialog to a session-creation sub-modal:

```
┌──── New Remote Session ──────────────────────────────────┐
│  Remote working directory:                                │
│  [ /workspace/                                        ]   │
│                                                           │
│  Saved directories:                                       │
│    /workspace/proj-a                                      │
│  > /workspace/proj-b          ← selected                 │
│                                                           │
│  [Enter] confirm  [Esc] back  [↑↓] navigate saved dirs   │
└───────────────────────────────────────────────────────────┘
```

Entries from `remote.savedDirs` appear in the list. Selecting one populates the text field at the top. Press **Enter** to create the session on the remote host and open a tab bound to it. Press **Esc** to return to the new-tab dialog.

If session creation fails (e.g. the directory is not in the server's allowlist), the modal shows the error text. Press **Esc** to close and press **Ctrl+T** to start over.

### Remote-bound tab appearance

Remote-bound tabs are **purple** in the tab bar. The tab label shows the `host:port` of the remote host (the `display_host`, extracted from `remote.defaultAddr` at binding time) instead of the local working directory short name.

```
┌─ Tab 1: myproject ──────────┬─ 1.2.3.4:9876 ─────────────┐
│  exec workflow plan.md       │  exec workflow build.md     │
└──────────────────────────────┴─────────────────────────────┘
```

The `display_host` label is fixed for the lifetime of the tab — it does not change if `remote.defaultAddr` is later modified in config. The inner subtitle (below the hostname) shows the command currently running on the remote session, or `(ready)` when idle.

### Command execution in a remote-bound tab

Every command typed in a remote-bound tab is sent to the remote session via `POST /v1/commands`, then output is streamed back in real time via the SSE log-streaming endpoint — identical to `remote exec workflow <file> --follow` but with no flags required. The tab transitions through the same execution phase states as a local tab (running → done / error).

When first created, the tab automatically dispatches a `ready` command to the remote session (matching local tab behavior). The `ready` output appears in the execution window.

If the remote host returns an error (auth failure, session not found, network error), the error appears in the execution window. The tab remains bound to the remote session and accepts subsequent commands normally — the binding is permanent for the lifetime of the tab.

Flags that refer to remote addressing (`--session`, `--remote-addr`, `--api-key`) are stripped from forwarded commands, since the binding already supplies the target.

### Closing a remote-bound tab

Closing a remote-bound tab (with **Ctrl+C** when multiple tabs are open) cancels any in-flight command stream and any active workflow polling task. The remote session itself is **not** closed — it continues running on the remote host and can be accessed again later via a new remote-bound tab or the `remote` CLI subcommands.

### Workflow state strip for remote-bound tabs

When a workflow command is dispatched from a remote-bound tab (`exec workflow`), the workflow state strip appears automatically — exactly as it does for local workflow runs.

Starting 5 seconds after the command is dispatched, awman polls `GET /v1/workflows/:command_id` on the remote API server every 5 seconds. As soon as a workflow state is found, the strip renders and continues updating until the workflow reaches a terminal state (`complete` or `error`).

The remote workflow strip is visually identical to the local strip: parallel steps, paused states, running steps, and completion markers all render the same way, and **Ctrl-O** toggles it between the collapsed stage summary and the full per-step overview just as it does locally. No extra configuration is required.

**Polling behavior:**

| Situation | Behavior |
|-----------|----------|
| No workflow found (HTTP 404) on first poll | Polling stops silently — the command is not a workflow command |
| No workflow found (HTTP 404) after a previous 200 | Polling stops — workflow state was removed |
| Transient network error during polling | Retried on the next 5-second interval; no error shown to the user |
| Workflow reaches `complete` or `error` | Polling stops; strip reflects the final state |
| Tab closed while polling | Poll task cancelled immediately |
| New command dispatched from the same tab | Previous poll task cancelled; new poll task starts for the new command |

---

## Configuration

Remote mode settings live under a `remote` key in the global config (`~/.awman/config.json`). All fields are optional.

```json
{
  "remote": {
    "defaultAddr": "http://build-server.example.com:9876",
    "defaultAPIKey": "a3f8b2c1...64-char-hex...",
    "savedDirs": [
      "/home/user/my-project",
      "/home/user/other-project"
    ]
  }
}
```

### `remote.defaultAddr`

The default address of the remote API awman server. When set, you don't need to pass `--remote-addr` on every command.

```sh
awman config set --global remote.defaultAddr http://build-server.example.com:9876
```

Overridden per-invocation by `--remote-addr` or `AWMAN_REMOTE_ADDR`.

### `remote.defaultAPIKey`

The default API key to send when authenticating to the remote API server. When set alongside `remote.defaultAddr`, every `awman remote` command to that host is authenticated automatically with no extra flags.

```sh
awman config set --global remote.defaultAPIKey <your-api-key>
```

**Security constraint:** this key is **only sent when the target address exactly matches `remote.defaultAddr`** (scheme, host, and port, after stripping trailing slashes). If you use `--remote-addr` or `AWMAN_REMOTE_ADDR` to point at a different server, the stored key is ignored — it is never silently forwarded to an unintended host.

Overridden per-invocation by `--api-key` or `AWMAN_API_KEY`.

### `remote.savedDirs`

A list of working directory paths (absolute paths on the remote host) for use by the TUI's saved-dir picker when running `remote session start` without a directory argument.

```sh
# Set a single directory
awman config set --global remote.savedDirs /home/user/my-project

# Set multiple directories (comma-separated)
awman config set --global remote.savedDirs "/home/user/my-project,/home/user/other-project"

# Clear all saved directories
awman config set --global remote.savedDirs ""
```

Directories can also be added interactively from the TUI: when you start a session with a directory not already in the list, the TUI offers to save it for you.

---

## Full example: end-to-end CLI workflow

```sh
# Configure the remote host and API key once
awman config set --global remote.defaultAddr http://build-server.example.com:9876
awman config set --global remote.defaultAPIKey <your-api-key>

# Create a session on the remote host
SESSION=$(awman remote session start /home/user/my-project | grep 'Session started:' | awk '{print $NF}')
echo "Session: $SESSION"

# Dispatch a workflow and stream its output
awman remote exec workflow path/to/workflow.toml --session "$SESSION" --follow

# Or pipe into a log file (no ANSI decoration)
awman remote exec workflow path/to/workflow.toml --session "$SESSION" --follow > workflow.log

# Kill the session when you are done
awman remote session kill "$SESSION"
```

---

## Full example: CI pipeline

```sh
export AWMAN_REMOTE_ADDR=http://build-server.internal:9876
export AWMAN_API_KEY=<your-api-key>
export AWMAN_REMOTE_SESSION=<pre-provisioned-session-id>

# Dispatch the workflow; exit code reflects the command's exit code
awman remote exec workflow path/to/workflow.toml --follow
```

For CI contexts where a session is long-lived and pre-provisioned, setting `AWMAN_REMOTE_SESSION` and `AWMAN_API_KEY` in the pipeline environment avoids per-command flags entirely.

---

## Using cURL directly

Because `remote exec` is built on the API HTTP API, you can use cURL (or any HTTP client) wherever `awman remote` is inconvenient — for example, in scripts with no awman binary available:

```sh
SERVER=http://build-server.example.com:9876
KEY=<your-api-key>
SESSION=a1b2c3d4-...

# Submit a command
CMD=$(curl -s -X POST "$SERVER/v1/commands" \
  -H "Authorization: Bearer $KEY" \
  -H "x-awman-session: $SESSION" \
  -H 'Content-Type: application/json' \
  -d '{"subcommand":"exec","args":["workflow","path/to/workflow.toml"]}' | jq -r .command_id)

# Stream live output via SSE (prints each log line as it arrives)
curl -s "$SERVER/v1/commands/$CMD/logs/stream" \
  -H "Authorization: Bearer $KEY" \
| while IFS= read -r line; do
  case "$line" in
    "data: [awman:done]") echo "--- done ---"; break ;;
    data:\ *)             echo "${line#data: }" ;;
  esac
done

# Or poll until done, then fetch the full log
while true; do
  STATUS=$(curl -s "$SERVER/v1/commands/$CMD" \
    -H "Authorization: Bearer $KEY" | jq -r .status)
  [ "$STATUS" = "done" ] || [ "$STATUS" = "error" ] && break
  sleep 5
done
curl -s "$SERVER/v1/commands/$CMD/logs" \
  -H "Authorization: Bearer $KEY" | jq -r .output
```

See [API Mode](09-api-mode.md) for the full HTTP API reference, including session management endpoints.

---

## Edge cases

| Situation | Behaviour |
|-----------|-----------|
| No remote address configured | Error with instructions to pass `--remote-addr`, set `AWMAN_REMOTE_ADDR`, or configure `remote.defaultAddr` |
| `remote exec` without `--session` in CLI mode | Error with instructions to pass `--session` or set `AWMAN_REMOTE_SESSION` |
| `remote session start` without a directory in CLI mode | Error with instructions to pass a directory argument |
| `remote session kill` without a session ID in CLI mode | Error with instructions to pass a session ID |
| Session not found on remote | HTTP 404 from the server; error message includes the session ID and suggests `remote session start` |
| Remote host unreachable | Connection timeout after 10 s; error message includes the target address |
| Session is busy (command already running) | HTTP 403 from the server; error message includes the running command ID |
| `--follow` on a command that already completed | Full historical log is replayed, then summary is printed — no output is missed |
| `--follow` command runs longer than 10 minutes with no output | Client times out and prints: `"Request to <addr> timed out after 10 minutes. The remote command may still be running on the server."` The command continues running on the server; reconnect with `remote exec` (without `--follow`) to check status |
| `--follow` receives any output from server within 10 minutes | Timeout is reset on each received event; commands that produce incremental output can run for hours |
| `remote session start` with a new dir (TUI) | Offers to save the path to `remote.savedDirs`; session starts regardless of whether you save |
| TUI session picker: remote host has no active sessions | Picker modal shows "No active sessions" message; `Enter` and `Esc` both cancel |
| TUI session picker: fetch fails | Error shown in command input bar; no modal opens |
| TUI `remote session start` with no saved dirs and no dir argument | Error in command input bar: "No directory specified and no savedDirs configured" |
| `remote session start` with a dir already in `savedDirs` | Dir is not duplicated in the list when the save-dir prompt is accepted |
| Inner command flags (e.g. `--yolo`) | Forwarded to the remote host verbatim; not consumed by the `remote exec` parser |
| No API key provided and server requires auth | HTTP 401; error body explains which header to use |
| `remote.defaultAPIKey` set but target address differs from `remote.defaultAddr` | Config key is ignored; request proceeds without auth (server returns 401 if auth is required) |
| `remote.defaultAPIKey` matches `remote.defaultAddr` with trailing slash difference | Trailing slashes are stripped from both sides before comparison; key is used |
| Session closed between picker fetch and command dispatch | Server returns HTTP 404; client surfaces a clear error; session is not re-opened |
| **Ctrl+T** with `remote.defaultAddr` not configured | New-tab dialog behaves exactly as before — no remote session list, no fetch, no binding option |
| **Ctrl+T** opened while a previous remote session fetch is still in-flight | Previous fetch is cancelled; new fetch starts for the fresh modal open |
| Remote-bound tab: auth error when dispatching a command | Error appears in the execution window; no `command_id` returned; workflow polling does not start; tab stays bound |
| Remote-bound tab: remote session closed externally while tab is open | `POST /v1/commands` returns HTTP 404; error shown in execution window; subsequent commands also fail until the session is recreated on the remote host |
| Remote-bound tab: command contains `--session` or `--remote-addr` flags | These flags are stripped before forwarding; the tab's binding supplies the target |
| Remote-bound tab: tab closed while a command stream is in-flight | SSE stream task and any workflow poll task are cancelled; the remote command continues executing on the server |
| Remote-bound tab: new command dispatched while workflow polling is active | Previous poll task cancelled; new poll task starts 5 seconds after the new command is dispatched |
| Remote-bound tab: create-new-session sub-modal — remote dir creation fails | Modal shows error text; no tab created; press **Esc** and retry with **Ctrl+T** |
| Remote workflow strip: `GET /v1/workflows/:command_id` returns HTTP 404 | Polling stops silently; no error shown; strip does not appear |
| Remote workflow strip: transient network errors during polling | Retried on next 5-second interval; not surfaced to user |
| Remote workflow strip: parallel steps | Rendered stacked in the strip, identical to local workflow parallel steps |
| Remote workflow strip: paused step | Strip shows the paused indicator on the paused step; polling continues since the workflow may resume |

---

[← API Mode](09-api-mode.md) · [Next: GitHub Integration →](11-github-integration.md)
