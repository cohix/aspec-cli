# CLI Design

Binary name: `awman`
Install path: `/usr/local/bin/`
Storage location: `$HOME/.awman/`

This document is the authoritative specification of the `awman` CLI surface. It is regenerated from `CommandCatalogue` (see `src/command/dispatch/catalogue.rs`); when you change a command, subcommand, flag, or alias, update this file. CI does not block on drift today, but every reviewer should treat divergence between this file and the catalogue as a defect.

## Design principles

- **Single binary, two modes.** `awman` with no arguments launches a Ratatui TUI. `awman <subcommand> …` runs a single command and exits, with output on stdout/stderr.
- **Catalogue-driven.** Every flag, subcommand, and default lives in `CommandCatalogue`. Frontends read from the catalogue rather than hard-coding strings.
- **Non-interactive by default for scripts.** Flags like `--non-interactive` and `--json` are first-class for API and CI use. `--json` always implies `--non-interactive`.
- **Container isolation.** Every agentic operation runs inside a Docker (or Apple Containers) container built from `Dockerfile.dev`. The host never executes agent code directly.

## Top-level commands

| Command | Summary |
|---|---|
| `awman` | Launch the interactive TUI. |
| `awman init` | Initialize the current Git repo for use with awman. |
| `awman ready` | Verify the Docker daemon, ensure `Dockerfile.dev`, build the dev image. |
| `awman chat` | Freeform chat session with the configured agent. |
| `awman specs <subcommand>` | Manage work item specs. |
| `awman new <subcommand>` | Create a new awman artefact (spec, workflow, skill). |
| `awman exec <subcommand>` | Run a one-shot prompt or workflow. |
| `awman config <subcommand>` | View and edit global/repo configuration. |
| `awman status` | Show all running awman containers. |
| `awman api <subcommand>` | Run awman as an API HTTP server. |
| `awman amie <subcommand>` | Manage the amie condition daemon and scheduled conditions. |
| `awman remote <subcommand>` | Connect to a remote API instance. |

### Top-level flags (apply before any subcommand)

| Flag | Kind | Default | Description |
|---|---|---|---|
| `--build` | bool | false | Force rebuild of images on startup. |
| `--no-cache` | bool | false | Disable Docker layer cache during builds. |
| `--refresh` | bool | false | Refresh agent environment (run audit). |
| `-h, --help` | bool | — | Print help. |
| `-V, --version` | bool | — | Print version. |

## Per-command surface

### `awman init`

Initialize the current Git repo for use with awman.

| Flag | Kind | Default | Description |
|---|---|---|---|
| `--agent <name>` | enum | `claude` | One of: `claude`, `codex`, `opencode`, `maki`, `gemini`, `copilot`, `crush`, `cline`. |
| `--aspec` | bool | false | Download aspec templates into the project. |

### `awman ready`

| Flag | Kind | Default | Description |
|---|---|---|---|
| `--refresh` | bool | false | Run the Dockerfile agent audit. |
| `--build` | bool | false | Force rebuild of the dev image. |
| `--no-cache` | bool | false | Pass `--no-cache` to `docker build`. |
| `-n, --non-interactive` | bool | false | Run the agent in non-interactive (print) mode. |
| `--allow-docker` | bool | false | Mount the host Docker daemon socket into the agent container. |
| `--json` | bool | false | Suppress human output and print structured JSON. **Implies `--non-interactive`.** |

### `awman chat`

| Flag | Kind | Default | Description |
|---|---|---|---|
| `-n, --non-interactive` | bool | false | Non-interactive (print) mode. |
| `--plan` | bool | false | Plan mode (read-only). |
| `--allow-docker` | bool | false | Mount the host Docker daemon socket. |
| `--yolo` | bool | false | Fully autonomous mode. |
| `--auto` | bool | false | Auto permission mode. |
| `--agent <name>` | string | — | Override the agent for this run. |
| `--model <name>` | string | — | Override the model for this run. |
| `--launch-mode <stdio\|acp>` | enum | `stdio` | Launch the agent over ACP (Agent Client Protocol) instead of raw container stdio. `acp` requires an agent that supports it (currently `cline`). See `docs/17-acp-mode.md`. |
| `--overlay <spec>` | repeatable string | — | Overlay expression: `dir(host:container[:ro\|rw])`, `ssh()`, `env(VAR_NAME)`, `skill(*)`, or `skill(name)`. To mount `~/.ssh` read-only, pass `--overlay ssh()`. See `docs/08-overlays.md`. |

### `awman specs`

| Subcommand | Arguments | Flags |
|---|---|---|
| `amend <work_item>` | `<work_item>` | `-n/--non-interactive`, `--allow-docker` |

### `awman new`

| Subcommand | Arguments | Flags |
|---|---|---|
| `spec` | — | `--interview`, `-n/--non-interactive`. |
| `workflow` | — | `--interview`, `-n/--non-interactive`, `--global`, `--format <toml\|yaml\|md>` (default `toml`). |
| `skill` | — | `--interview`, `-n/--non-interactive`, `--global`. |

### `awman exec`

| Subcommand | Arguments | Flags |
|---|---|---|
| `prompt <prompt>` | `<prompt>` | `-n/--non-interactive`, `--plan`, `--allow-docker`, `--yolo`, `--auto`, `--agent <name>`, `--model <name>`, `--launch-mode <stdio\|acp>`, `--overlay <spec>` (repeatable). |
| `workflow <path>` (alias `wf`) | `<path>` | `--work-item <num>`, `-n/--non-interactive`, `--plan`, `--allow-docker`, `--worktree`, `--yolo`, `--auto`, `--agent <name>`, `--model <name>`, `--launch-mode <stdio\|acp>`, `--overlay <spec>` (repeatable). `--yolo`/`--auto` imply `--worktree`. ACP is not yet driven for workflow steps; a step that resolves to `acp` is rejected pre-flight (see `docs/17-acp-mode.md`). |

`--overlay` accepts the same typed overlay expressions everywhere (CLI flags, `AWMAN_OVERLAYS`, repo/global config `overlays` array, and per-step `overlays` in workflow files): `dir(host:container[:ro|rw])`, `ssh()` (shorthand for `~/.ssh` read-only), `env(VAR_NAME)`, `skill(*)`, `skill(name)`. The legacy `--mount-ssh` flag has been removed; pass `--overlay ssh()` instead. See `docs/08-overlays.md` for the full reference.

### `awman config`

| Subcommand | Arguments | Flags |
|---|---|---|
| `show` | — | — |
| `get <field>` | `<field>` | — |
| `set <field> <value>` | `<field>`, `<value>` | `--global` (repo scope by default). |

### `awman status`

| Flag | Description |
|---|---|
| `--watch` | Continuously refresh every 3 seconds. The CLI emits `\x1b[H\x1b[J` clear sequences; the TUI swallows them. |

### `awman api`

| Subcommand | Flags |
|---|---|
| `start` | `--port <n>` (default `9876`), `--workdirs <path>` (repeatable), `--background`, `--refresh-key`, `--dangerously-skip-auth`, `--dangerously-skip-tls`. |
| `kill` | — |
| `logs` | — |
| `status` | — |

### `awman amie`

Manage the amie condition daemon and scheduled conditions. The parent flags
belong to `awman amie` itself; pass them before a subcommand when using one:

| Flag | Kind | Default | Description |
|---|---|---|---|
| `-n, --non-interactive` | bool | false | Print the amie status summary instead of opening the TUI. |
| `--json` | bool | false | Emit JSON output. **Implies `--non-interactive`.** |

| Subcommand | Arguments | Flags |
|---|---|---|
| _(bare)_ `awman amie` | — | Inherits the parent flags above. With a TTY and neither `-n` nor `--json`, opens the singleton amie TUI tab; with `-n`/`--non-interactive` or `--json`, prints the daemon status summary instead. |
| `add` | — | `--name <string>` (required; no default), `--description <string>` (required; no default), `--repo <path>` (default `—`), `--interval <string>` (default `5m`), `--agent <string>` (default `—`), `--model <string>` (default `—`), `--mount-scope <cwd\|gitroot>` (default `gitroot`), `--interview` (bool, default `false`), `-n, --non-interactive` (bool, default `false`). |
| `list` | — | `--json` (bool, default `false`). |
| `show <name>` | `<name>` (required string) | `--json` (bool, default `false`). |
| `remove <name>` | `<name>` (required string) | `-y, --yes` (bool, default `false`). |
| `pause <name>` | `<name>` (required string) | — |
| `resume <name>` | `<name>` (required string) | — |
| `start` | — | `--port <n>` (u16, default `0`; `0` selects an OS-assigned port), `--background` (bool, default `false`), `--refresh-key` (bool, default `false`), `--dangerously-skip-auth` (bool, default `false`). |
| `stop` (alias `kill`) | — | — |
| `status` | — | `--json` (bool, default `false`). |
| `logs` | — | `-f, --follow` (bool, default `false`). |
| `attach <name>` | `<name>` (required string) | `--container <string>` (default `—`; running container ID when multiple are active). |

### `awman remote`

| Subcommand | Arguments | Flags |
|---|---|---|
| `run <command…>` | trailing varargs forwarded verbatim | `--remote-addr <url>`, `--session <id>`, `-f/--follow`, `--api-key <key>`. |
| `session start <dir>` | `<dir>` | — |
| `session kill <session_id>` | `<session_id>` | — |

## Inputs and outputs

- The TUI takes over the terminal via Ratatui; ANSI escapes are forwarded to the agent's PTY.
- CLI commands write human-readable output to stdout and diagnostics to stderr.
- `--json` flips the renderer to a structured-JSON serializer.
- Containers launched by awman plumb the developer's stdin/stdout/stderr through the chosen runtime so the agent runs interactively inside the TUI.

## Configuration

- Per-repo config: `<git-root>/.awman/config.json`.
- Global config: `$HOME/.awman/config.json`.
- Environment overrides: `AWMAN_*` variables (notably `AWMAN_OVERLAYS`, `AWMAN_API_KEY`, `AWMAN_API_ROOT`).

Precedence (highest to lowest): CLI flag → environment variable → repo config → global config → built-in default.
