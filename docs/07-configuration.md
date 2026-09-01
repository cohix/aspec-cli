# Configuration

awman reads two JSON config files — one per repository, one global — and merges them with command-line flags and environment variables. You rarely need to edit the files by hand: the `awman config` subcommand can view and change almost everything.

---

## The two config files

### Per-repository config

**Path:** `<git_root>/.awman/config.json`

Created by `awman init`; commit it so the whole team shares the same setup.

```json
{
  "agent": "claude",
  "launchMode": "stdio",
  "terminal_scrollback_lines": 10000,
  "dockerfile": "docker/Dockerfile.base",
  "yoloDisallowedTools": ["Bash"],
  "agentStuckTimeout": 60,
  "maxConcurrentAgents": 3,
  "overlays": ["env(ANTHROPIC_API_KEY)", "dir(/data/fixtures:/mnt/fixtures:ro)"],
  "workItems": {
    "dir": "docs/work-items",
    "template": "docs/work-items/0000-template.md"
  },
  "dynamicWorkflows": {
    "agentsToModels": {
      "claude": ["claude-opus-4-8", "claude-sonnet-4-6"]
    },
    "maxConcurrentSteps": 3,
    "defaultLeader": "claude::claude-opus-4-8",
    "guidance": ["Never spawn more than two agents in parallel."]
  }
}
```

### Global config

**Path:** `$HOME/.awman/config.json` (relocatable — see [Where global files live](#where-global-files-live))

Applies to every project on the machine unless a repo overrides it.

```json
{
  "default_agent": "claude",
  "launchModeFallback": "error",
  "runtime": "docker",
  "terminal_scrollback_lines": 10000,
  "yoloDisallowedTools": ["Bash"],
  "overlays": ["skill(*)", "env(ANTHROPIC_API_KEY)"],
  "agentStuckTimeout": 30,
  "maxConcurrentAgents": 2,
  "workers": 2,
  "api": {
    "workDirs": ["/home/user/my-project"],
    "alwaysNonInteractive": false
  },
  "squad": {
    "agentsToModels": {
      "claude": ["claude-opus-4-8", "claude-sonnet-4-6"]
    },
    "maxConcurrentEvaluations": 2,
    "defaultLeader": "claude::claude-opus-4-8",
    "guidance": ["Keep automated changes focused."]
  },
  "remote": {
    "defaultAddr": "http://build-server.example.com:9876",
    "defaultAPIKey": "a3f8b2c1...",
    "savedDirs": ["/home/user/my-project"]
  }
}
```

### Squad daemon configuration

The optional `squad` block is global, so it belongs in
`~/.awman/config.json` (or the relocated global config file). It controls the
agents and models available to the squad daemon and the guidance it passes to
task evaluations:

Task-specific workspace, interval, and overlay choices are configured with
`awman squad add` rather than in this global block. See [Squad](16-squad.md)
for the durable workspace, task-creation, and daemon details. The squad daemon
and API server cannot run at the same time because they share the awman
database; see [API Mode: API server and squad daemon](09-api-mode.md#api-server-and-squad-daemon).

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `agentsToModels` | object (agent name → non-empty string array) | unset | Models available for each agent; every map key must be a valid agent name |
| `maxConcurrentEvaluations` | positive integer | `2` | Maximum number of task evaluations running at once |
| `defaultLeader` | string (`agent::model`) | unset | Default agent and model for evaluations when a task does not specify them |
| `guidance` | non-empty string array | unset | Instructions added to every task evaluation and generated workflow |

All four keys are optional. Configuration is rejected when
`maxConcurrentEvaluations` is zero; an `agentsToModels` entry has no models or
contains an empty model name; `defaultLeader` is not exactly two non-empty
components in `agent::model` form, has surrounding whitespace in either
component, or uses an invalid agent name; or `guidance` has an empty or
whitespace-only entry. A `guidance` list may contain at most 50 entries, and
each entry is limited to 1,000 characters. Agent names use ASCII letters,
digits, `-`, and `_`, and are 1–64 characters long.

> **Upgrading from an old config?** The `envPassthrough` field was removed. Express environment passthrough as `env(VAR)` entries in the `overlays` array instead — see [Overlays](08-overlays.md). The old object-style `overlays` block (`{"skills": …, "directories": …}`) is also gone; `overlays` is now a flat array of overlay specs and the old format produces a parse error.

---

## How precedence works

For any setting, the highest-priority source that defines it wins:

```
flags  >  environment variables  >  repo config  >  global config  >  built-in default
```

Examples:

- `awman chat --agent codex` beats `agent` in repo config, which beats `default_agent` in global config.
- `AWMAN_REMOTE_ADDR` beats `remote.defaultAddr` in global config; `--remote-addr` beats both.
- `awman exec workflow ... --max-concurrent 4` beats `AWMAN_MAX_CONCURRENT_AGENTS`, which beats `maxConcurrentAgents` in repo config, which beats `maxConcurrentAgents` in global config.
- With nothing set anywhere, built-in defaults apply: 10,000 scrollback lines, 30-second agent-stuck timeout, 2 API workers, API port 9876, unlimited concurrent agents per workflow.

Launch mode follows the same order where those sources apply:

```
--launch-mode  >  AWMAN_LAUNCH_MODE  >  repo launchMode  >  stdio
```

`launchMode` is a repository setting; there is no global `launchMode` value.
`launchModeFallback` is global-only and follows `global launchModeFallback >
error`. Its setting matters when ACP was requested but the selected agent does
not support ACP: `error` stops the launch, while `stdio` permits a visible
fallback to the regular mode.

Two wrinkles:

- **List fields replace, they don't merge.** A repo `yoloDisallowedTools` list completely replaces the global one — even an empty list. To inherit the global list, omit the field from the repo config.
- **Overlays are additive.** `overlays` entries from global config, repo config, `AWMAN_OVERLAYS`, and `--overlay` flags are all merged. See [Overlays](08-overlays.md).

---

## Managing config from the terminal

```sh
awman config show                          # full table: global, repo, and effective values
awman config get <field>                   # one field, all scopes
awman config set <field> <value>           # write to repo config
awman config set --global <field> <value>  # write to global config
```

- `config show` and `config get` never fail on missing files; absent files are treated as all-unset. `config set` creates the file and its parent directory as needed.
- Each field has a natural scope. Setting a global-only field without `--global` (or a repo-only field with it) is an error that tells you which flag to use.
- Unknown field names get a did-you-mean suggestion list.
- `remote.defaultAPIKey` is masked in `config show`/`config get` output.
- Setting the removed `envPassthrough` field errors with guidance to use `env(VAR)` overlay entries instead.

The full list of accepted field names and their scopes is in the [Reference](#reference).

---

## Common recipes

### Set the default agent

```sh
awman config set --global default_agent gemini   # for all projects
awman config set agent codex                     # for this repo only
```

Valid agents: `claude`, `codex`, `gemini`, `opencode`, `crush`, `cline`, `copilot`, `maki`, `antigravity`. Anything else is rejected at write time.

### Adjust terminal scrollback

```sh
awman config set --global terminal_scrollback_lines 20000   # all projects
awman config set terminal_scrollback_lines 5000             # this repo
```

A 10,000-line buffer at 80 columns uses roughly 3 MB per tab. Increase for long build logs; decrease when running many tabs.

### Pass API keys into agent containers

awman never forwards your whole environment into a container — name each variable explicitly as an `env()` overlay:

```sh
awman config set --global overlays "env(ANTHROPIC_API_KEY),env(OPENAI_API_KEY)"
```

Or per-invocation: `awman chat --overlay "env(ANTHROPIC_API_KEY)"`. See [Overlays](08-overlays.md) for the full syntax and [Agent Sessions](03-agent-sessions.md) for per-agent authentication details.

### Switch container runtime

```sh
awman config set --global runtime docker                  # default
awman config set --global runtime apple-containers        # macOS only
awman config set --global runtime docker-sbx-experimental # experimental
```

See [Runtimes](#runtimes) below.

### Choose ACP launch mode

To use ACP by default in this repository, add the following field to
`.awman/config.json`:

```json
{ "launchMode": "acp" }
```

The allowed values are `"stdio"` (the default) and `"acp"`. To choose what
happens when a workflow step's agent does not support ACP, add
`launchModeFallback` to `$HOME/.awman/config.json`:

```json
{ "launchModeFallback": "stdio" }
```

Its allowed values are `"error"` (the default) and `"stdio"`. The command-line
flag takes priority over both the environment variable and repository setting;
see [ACP Mode](17-acp-mode.md).

### Custom work item paths

By default awman looks for work items in `aspec/work-items/` and uses `aspec/work-items/0000-template.md` as the template for `awman new spec`. To use different paths:

```sh
awman config set work_items.dir docs/work-items
awman config set work_items.template docs/work-items/0000-template.md
```

Paths may be relative to the repo root (recommended) or absolute. Note the CLI names are `work_items.dir` / `work_items.template`, but they are stored in the JSON file under a single `workItems` block.

### Cap concurrent agents in a workflow

```sh
awman config set --global maxConcurrentAgents 2   # machine-wide default
awman config set maxConcurrentAgents 4            # this repo only
```

Or per-invocation, without touching any config file:

```sh
awman exec workflow aspec/workflows/implement-hard.toml --max-concurrent 2
AWMAN_MAX_CONCURRENT_AGENTS=2 awman exec workflow aspec/workflows/implement-hard.toml
```

Leaving `maxConcurrentAgents` unset (the default) means unlimited — every step whose dependencies are satisfied launches immediately. Lower it to match your machine's CPU/memory budget or your Docker daemon's capacity. See [Parallel Workflows](15-parallel-workflows.md) for how the engine uses this cap to schedule steps.

### Configure dynamic workflow agents, models, and leader

```sh
awman config set dynamicWorkflows.defaultLeader claude::claude-opus-4-8
awman config set dynamicWorkflows.maxConcurrentSteps 3
```

`dynamicWorkflows.agentsToModels` is set one agent at a time — `awman config set dynamicWorkflows.agentsToModels.<agentName> "model-a, model-b"` (an empty value removes the mapping), or inline in the TUI config dialog where **Ctrl+N** adds a new mapping.

`dynamicWorkflows.guidance` is a list of instructions the leader agent must follow whenever it designs a workflow. It's set one entry at a time, addressed by index — `awman config set dynamicWorkflows.guidance.0 "Never spawn more than two agents in parallel."` (an empty value removes that entry and re-indexes the rest) — or inline in the TUI config dialog, where **Ctrl+N** appends a new entry.

See [Dynamic Workflows](13-dynamic-workflows.md#configuring-dynamic-workflows) for the full reference.

### Custom Dockerfile path

By default awman builds the project base image from `<git_root>/Dockerfile.dev`. To use a Dockerfile elsewhere, set `dockerfile` in `.awman/config.json` directly (it is not settable via `config set`):

```json
{ "dockerfile": "docker/Dockerfile.base" }
```

If the configured file doesn't exist, commands report the exact configured path rather than silently falling back to the default. `awman init` also offers to point at an existing Dockerfile interactively when no `Dockerfile.dev` is found.

### Restrict tools in yolo mode

```sh
awman config set yoloDisallowedTools "Bash,computer"   # this repo
awman config set yoloDisallowedTools ""                # set an empty list
```

An empty repo list actively overrides a non-empty global list. To stop overriding, remove the field from the repo config file. See [Yolo Mode](06-yolo-mode.md).

### Control credential injection (`auth` mode)

By default awman injects host keychain credentials into agent containers
(`keychain` mode). Two alternatives are available for harnesses that supply
credentials through other means:

```json
{ "auth": "passthrough" }
```

| Value | Behaviour |
|-------|-----------|
| `keychain` (default) | Inject host keychain credentials. When the repo also declares `env(ANTHROPIC_API_KEY)` (or another credential that covers the same provider) **and that variable is set on the host**, the keychain OAuth token for that provider is automatically suppressed at injection time — the container receives exactly one set of credentials per provider. If the declared passthrough var is not set on the host, the keychain credential is retained so the container is not left with zero credentials for that provider. |
| `passthrough` | No KEYCHAIN credential injection; declared `env()` overlays still apply. Supply credentials via `env(VAR)` overlays. |
| `none` | No KEYCHAIN credential injection; declared `env()` overlays still apply. |

Set `auth` in `.awman/config.json` directly (it is not settable via `config set`). The field is per-repo only — cloud harnesses that do not declare an anthropic env var remain on the default `keychain` path and continue to receive keychain OAuth unaffected.

---

## Runtimes

The global `runtime` key selects how agent processes are isolated from your host machine:

| Value | Platform | Notes |
|-------|----------|-------|
| `docker` (default) | Linux, macOS, Windows | Standard Docker; ephemeral containers torn down when the session ends |
| `apple-containers` | macOS 26+ only | Native `container` CLI; same user experience as Docker. On Linux/Windows this value is an error, not a silent fallback. `--allow-docker` is not supported under this runtime |
| `docker-sbx-experimental` | macOS arm64, Windows x86_64 | Docker Sandboxes (persistent microVMs per session; hypervisor-grade isolation). Requires the `sbx` CLI and a Docker account. Linux is blocked by an upstream virtiofs bug. See [Runtimes](12-runtimes.md) |

An unrecognized value (e.g. a typo) is a fatal error — awman never falls back to a different isolation model than the one you configured. CLI commands print the invalid value and the list of valid values, then exit; the TUI shows the same message in a startup modal (Enter quits). Fix the value in `$HOME/.awman/config.json` and relaunch.

`awman ready` validates the configured runtime before any other check and reports which one is active. For full details on platform support, setup, credential registration, and the persistent-sandbox lifecycle see [Runtimes](12-runtimes.md).

---

## Where global files live

awman keeps global config and data (workflows, skills, worktrees, API state) under one home directory, `~/.awman/` by default. You can relocate it:

| Priority | Variable | Config goes to | Data goes to |
|----------|----------|----------------|--------------|
| 1 | `AWMAN_CONFIG_HOME` | `$AWMAN_CONFIG_HOME/` | `$AWMAN_CONFIG_HOME/` |
| 2 | `XDG_CONFIG_HOME` / `XDG_DATA_HOME` | `$XDG_CONFIG_HOME/awman/` | `$XDG_DATA_HOME/awman/` |
| 3 | (none set) | `~/.awman/` | `~/.awman/` |

- `AWMAN_CONFIG_HOME` overrides everything; XDG variables are then ignored.
- The XDG variables are independent — if only one is set, the other falls back to `~/.awman/`.
- An XDG variable set to an empty string is treated as unset.
- awman does **not** migrate existing data when you change these variables; move `~/.awman/` contents yourself if needed.
- The shared SQLite database is `~/.awman/data/awman.db` at the default location. API logs, PID files, credentials, and session files remain under `~/.awman/api/`.
- The API server's storage root can be moved independently with `AWMAN_API_ROOT`; this does not move the shared database.

---

## Reference

### Per-repo config fields (`<git_root>/.awman/config.json`)

| JSON key | Type | Default | Meaning | Settable via `config set` |
|----------|------|---------|---------|---------------------------|
| `agent` | string | (unset → global `default_agent`) | Agent for this repo | yes (repo or global scope) |
| `launchMode` | `"stdio"` \| `"acp"` | `"stdio"` | Default agent launch mode for this repo | no (edit file) |
| `auto_agent_auth_accepted` | bool | (unset) | Records that you accepted the agent auth consent prompt; managed by awman, shown read-only | no (managed) |
| `terminal_scrollback_lines` | integer | 10000 | Scrollback lines in the container terminal | yes |
| `yoloDisallowedTools` | string array | `[]` | Tools forbidden under `--yolo`; replaces the global list entirely | yes |
| `workItems.dir` | string | `aspec/work-items` | Work items directory (relative to repo root or absolute) | yes, as `work_items.dir` |
| `workItems.template` | string | `<workItems.dir>/0000-template.md` | Template for new work items | yes, as `work_items.template` |
| `overlays` | string array | `[]` | Overlay specs (`dir(…)`, `env(…)`, `skill(…)`); merged with all other overlay sources | yes |
| `agentStuckTimeout` | integer (seconds) | 30 | Inactivity period before an agent is flagged as stuck | yes |
| `maxConcurrentAgents` | integer | (unset → unlimited) | Cap on concurrently-running workflow steps; overridden by `--max-concurrent` / `AWMAN_MAX_CONCURRENT_AGENTS` — see [Parallel Workflows](15-parallel-workflows.md) | yes |
| `baseImage` | string | (unset → global) | Image tag for workflow setup/teardown containers — see [Workflows](05-workflows.md) | no (edit file) |
| `dockerfile` | string | `Dockerfile.dev` | Path to the project base Dockerfile, relative to repo root or absolute | no (edit file) |
| `dynamicWorkflows.agentsToModels` | object (agent → string array) | (unset → Dockerfile discovery) | Restricts a dynamic workflow's leader to this agent/model set — see [Dynamic Workflows](13-dynamic-workflows.md#configuring-dynamic-workflows) | yes, per agent as `dynamicWorkflows.agentsToModels.<agentName>` (comma-separated; empty value removes) |
| `dynamicWorkflows.maxConcurrentSteps` | integer | (unset → unlimited) | Advisory cap on concurrent workflow steps passed to the leader prompt | yes, as `dynamicWorkflows.maxConcurrentSteps` |
| `dynamicWorkflows.defaultLeader` | string (`agent::model`) | (unset) | Default leader agent/model for `exec workflow --dynamic`; overridden by `--leader` | yes, as `dynamicWorkflows.defaultLeader` |
| `dynamicWorkflows.guidance` | string array | (unset → no guidance block) | Project-specific instructions injected into the leader prompt as a bullet list — see [Dynamic Workflows](13-dynamic-workflows.md#configuring-dynamic-workflows) | yes, per entry as `dynamicWorkflows.guidance.<index>` (empty value removes) |
| `auth` | `"keychain"` \| `"passthrough"` \| `"none"` | `"keychain"` | Credential injection mode — see [Control credential injection](#control-credential-injection-auth-mode) | no (edit file) |

### Global config fields (`$HOME/.awman/config.json`)

| JSON key | Type | Default | Meaning | Settable via `config set --global` |
|----------|------|---------|---------|-------------------------------------|
| `default_agent` | string | (unset) | Agent used when no repo agent is configured | yes |
| `terminal_scrollback_lines` | integer | 10000 | Default scrollback for all repos | yes |
| `runtime` | string | `docker` | Container runtime: `docker`, `apple-containers`, `docker-sbx-experimental` | yes |
| `yoloDisallowedTools` | string array | `[]` | Machine-wide yolo tool denylist (unless a repo overrides it) | yes |
| `overlays` | string array | `[]` | Overlay specs applied to every project; additive with other sources | yes |
| `agentStuckTimeout` | integer (seconds) | 30 | Default agent-stuck timeout | yes |
| `maxConcurrentAgents` | integer | (unset → unlimited) | Machine-wide default cap on concurrently-running workflow steps (unless a repo overrides it) — see [Parallel Workflows](15-parallel-workflows.md) | yes |
| `workers` | integer | 2 | API server worker tasks processing the command queue in parallel — see [API Mode](09-api-mode.md) | no (edit file) |
| `baseImage` | string | (unset) | Default image tag for workflow setup/teardown containers | no (edit file) |
| `api.workDirs` | string array | `[]` | Directories pre-approved for API session creation; merged with `--workdirs` at server start | yes |
| `api.alwaysNonInteractive` | bool | `false` | Force non-interactive mode for all dispatched commands (useful on API servers with no TTY) | no (edit file) |
| `remote.defaultAddr` | string | (unset) | Default remote awman API server address | yes |
| `remote.defaultAPIKey` | string | (unset) | API key for the default remote server; only sent when the target address matches `remote.defaultAddr` | yes |
| `remote.savedDirs` | string array | `[]` | Remote-host paths shown in the `remote session start` picker — see [Remote Mode](10-remote-mode.md) | no (edit file) |
| `squad` | object | (unset) | Global squad daemon settings; see [Squad daemon configuration](#squad-daemon-configuration) | no (edit file) |
| `launchModeFallback` | `"stdio"` \| `"error"` | `"error"` | What to do when a requested ACP launch uses an agent without ACP support | no (edit file) |

### `awman config` subcommands

| Command | Effect |
|---------|--------|
| `awman config show` | Table of every known field: global, repo, and effective values |
| `awman config get <field>` | Global, repo, and effective value of one field |
| `awman config set <field> <value>` | Write a field to repo config |
| `awman config set --global <field> <value>` | Write a field to global config |

### Field names accepted by `config set` / `config get`

| Field name | Scope |
|------------|-------|
| `agent` | repo or global |
| `auto_agent_auth_accepted` | global only (read-only; managed by the auth flow) |
| `terminal_scrollback_lines` | repo or global |
| `yoloDisallowedTools` | repo or global |
| `workItems` | repo only |
| `overlays` | repo or global |
| `agentStuckTimeout` | repo or global |
| `maxConcurrentAgents` | repo or global |
| `runtime` | global only |
| `default_agent` | global only |
| `api` | global only |
| `remote` | repo or global |
| `work_items.dir` | repo only |
| `work_items.template` | repo only |
| `api.workDirs` | global only |
| `api.port` | global only (default 9876) |
| `api.background` | global only |
| `remote.defaultAddr` | repo or global |
| `remote.defaultAPIKey` | repo or global |
| `dynamicWorkflows.defaultLeader` | repo only |
| `dynamicWorkflows.maxConcurrentSteps` | repo only |

`launchMode` and `launchModeFallback` are currently config-file-only fields;
they are not accepted by `config set` or `config get`. Edit the JSON files
shown above directly.

Value handling:

- `yoloDisallowedTools`, `overlays`, `api.workDirs` — comma-separated values are stored as arrays; an empty string stores an empty array.
- `terminal_scrollback_lines`, `agentStuckTimeout`, `api.port`, `maxConcurrentAgents`, `dynamicWorkflows.maxConcurrentSteps` — must be positive integers; `0` is rejected.
- `agent`, `default_agent` — validated against the supported agent list.
- `dynamicWorkflows.defaultLeader` — must be in `agent::model` format (exactly two non-empty, non-whitespace components).
- `dynamicWorkflows.guidance.<index>` — a single free-form instruction string, capped at 1,000 characters and 50 entries total; empty or whitespace-only values are rejected on load and coerced to a removal when set via `config set`.
- `envPassthrough` — removed; the error message points you to `env(VAR)` overlay entries.

### Environment variables

| Variable | Purpose |
|----------|---------|
| `AWMAN_CONFIG_HOME` | Relocate the entire global home (config + data); overrides XDG variables |
| `XDG_CONFIG_HOME` | Global config goes to `$XDG_CONFIG_HOME/awman/` |
| `XDG_DATA_HOME` | Global data (workflows, skills, worktrees, API state, and the shared database) goes to `$XDG_DATA_HOME/awman/` |
| `AWMAN_API_ROOT` | Relocate only the API server storage root |
| `AWMAN_OVERLAYS` | Comma-separated overlay specs (e.g. `env(TOKEN),dir(/a:/b:ro)`); merged with config and flags — see [Overlays](08-overlays.md) |
| `AWMAN_LAUNCH_MODE` | Choose `stdio` or `acp`; overrides repo `launchMode` and is overridden by `--launch-mode` |
| `AWMAN_MAX_CONCURRENT_AGENTS` | Cap on concurrently-running workflow steps; beats `maxConcurrentAgents` in repo/global config, beaten by `--max-concurrent` — see [Parallel Workflows](15-parallel-workflows.md) |
| `AWMAN_REMOTE_ADDR` | Remote API server address; beats `remote.defaultAddr`, beaten by `--remote-addr` |
| `AWMAN_API_KEY` | Remote API key; beats `remote.defaultAPIKey`, beaten by `--api-key` |
| `AWMAN_SQUAD_KEY` | Bearer key the CLI and TUI authenticate to the squad daemon with; printed once as a shell snippet on the daemon's first start — see [squad: Authenticating to the daemon](16-squad.md#authenticating-to-the-daemon) |
| `AWMAN_REMOTE_SESSION` | Sticky session id for `remote exec` commands; beaten by `--session` |

---

[← Yolo Mode](06-yolo-mode.md) · [Next: Overlays →](08-overlays.md)
