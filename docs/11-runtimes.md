# Runtimes

awman supports three agent runtimes. The runtime controls how agent processes are isolated from your host machine. All three use the same `awman` commands, the same workflow files, and the same agent names — the runtime is a configuration choice, not a different tool.

---

## Choosing a runtime

| Config value | Platform | Isolation unit | Requires |
|---|---|---|---|
| `docker` (default) | Linux, macOS, Windows | Linux container (shared kernel) | Docker daemon |
| `apple-containers` | macOS 26+ only | Lightweight VM per container | macOS 26 Tahoe |
| `docker-sbx-experimental` | macOS arm64; Windows x86_64 | MicroVM per session | `sbx` CLI + Docker account |

Set the runtime in your global config:

```sh
awman config set --global runtime docker                   # default
awman config set --global runtime apple-containers         # macOS 26+ only
awman config set --global runtime docker-sbx-experimental  # experimental
```

You can switch runtimes at any time — each keeps its own separate state and does not interfere with the others. See [Switching runtimes](#switching-runtimes).

**Invalid values are a fatal error.** If the `runtime` field holds anything other than the three strings above, awman refuses to start — it does not fall back to Docker, because launching agents under a different isolation model than the one you configured would be unsafe. CLI commands print the invalid value and the valid options, then exit immediately; the interactive TUI opens a modal dialog on startup with the same message (press Enter to quit). Fix the `runtime` value in `$HOME/.awman/config.json` and relaunch.

---

## Docker (default)

Docker is the default runtime on all platforms.

**Requirements:** Docker daemon running.

**Isolation model:** standard Linux containers. The agent runs in a container with your project mounted read-write. The container shares the host kernel; host escape requires a container escape exploit.

**Lifecycle:** containers are ephemeral. The agent container is removed when the session ends. Persistent state lives in your Git repository.

**`awman ready` for Docker** builds per-agent Docker images from the `Dockerfile.<agent>` files in `.awman/`. These images are rebuilt when you run `awman ready --build` or `awman ready --no-cache`.

See [Security & Isolation](04-security-and-isolation.md) for overlay mounts, the Docker socket flag, and worktree isolation.

---

## Apple Containers

Apple Containers is a macOS-native runtime that runs each agent in a lightweight virtual machine. It uses the native `container` CLI rather than Docker.

**Requirements:** macOS 26 Tahoe or later. The `container` CLI comes with macOS 26 — no separate install.

**Isolation model:** each container is a lightweight Linux VM using Apple's native hypervisor. Host escape requires a hypervisor exploit.

**Limitations:**
- macOS only — not available on Linux or Windows. Configuring it on those platforms is an error, not a silent fallback.
- `--allow-docker` (host Docker socket mount) is not supported under this runtime.

**`awman ready` for Apple Containers** builds the same per-agent images used by Docker, using `container build` instead of `docker build`. The workflow is identical.

---

## Docker Sandboxes (experimental)

`docker-sbx-experimental` runs each agent session in a dedicated microVM, providing hypervisor-grade isolation. It uses Docker's `sbx` CLI and is distinct from Docker Desktop — `sbx` is a standalone binary and does not require Docker Desktop to be installed.

This runtime is **experimental**. The `-experimental` suffix in the config string is intentional and durable: `sbx` itself has open known bugs as of June 2026 and its per-VM API is partly undocumented. The suffix will be removed in a future work item once both `sbx` and awman's integration have stabilized.

**Current status:** `awman ready`, `awman chat`, `awman exec`, and `awman exec workflow` all work end-to-end under `docker-sbx-experimental`. Run `awman ready` once to prepare the runtime, then use `awman chat` and `awman exec` exactly as you would with any other runtime.

### Platform support

| Platform | Status |
|---|---|
| macOS arm64 (Apple Silicon) | Supported |
| Windows x86_64 | Supported |
| macOS x86_64 (Intel) | Not supported — sbx requires Apple Silicon on macOS |
| Linux x86_64 | Blocked — a confirmed virtiofs bug (docker/sbx-releases#51, open as of June 2026) prevents agents from creating files in the workspace. Configuring this runtime on Linux returns a clear error. |

### What Docker Sandboxes is

Each sandbox is an isolated microVM with its own kernel (Linux 6.12 in Ubuntu), dedicated CPUs, private filesystem, and private Docker daemon. The agent binary runs natively inside the VM. The VM is created from a published template image pulled by host-side containerd — awman does not push any images to a registry.

The VM's private Docker daemon supports Docker-in-Docker, so agents that need to build and run containers work out of the box. The VM does not see your host Docker daemon, your host Docker images, or any host state outside the mounted workspace directory.

### How it differs from Docker and Apple Containers

| | Docker | Apple Containers | Docker Sandboxes |
|---|---|---|---|
| Isolation unit | Container (shared kernel) | Lightweight VM | MicroVM |
| Host escape requires | Container escape | Hypervisor CVE | Hypervisor CVE |
| Image model | Build from `Dockerfile.<agent>` | Same | Kit YAML — no custom OCI build or registry push |
| Volume mounts | `-v` bind mounts | `-v` bind mounts | virtiofs; workspace appears at the same absolute path inside VM |
| Env vars | `-e` flag or env file | Same | Credentials via `sbx secret set` + the credential proxy (not inherited from host shell) |
| Networking | Host bridge or NAT | Per-container network | All traffic through HTTP/HTTPS proxy; raw TCP/UDP blocked |
| Startup time | Milliseconds | ~1 second | 2–5 seconds cold (first launch per worktree) + startup script; subsequent restarts are faster |
| Stats | `docker stats` | `container stats` | Status only — no per-resource CPU/memory metrics |
| Persistence | Ephemeral (removed on exit) | Ephemeral | **Persistent** — sandbox state survives between sessions |

### Setup

**1. Install `sbx`**

```sh
brew install docker/tap/sbx   # macOS
```

For Windows, download from [Docker's releases page](https://docs.docker.com/go/sbx/).

**2. Authenticate**

```sh
sbx login
```

A free Docker account is sufficient.

**3. Run `awman ready`**

```sh
awman config set --global runtime docker-sbx-experimental
awman ready
```

`awman ready` for the sbx runtime:
- Verifies that `sbx` is on your PATH and that you are logged in.
- Emits per-agent kit files to `~/.awman/kits/<agent>/` for every configured agent.
- Validates each kit with `sbx kit validate`.

No credentials are registered at ready time — all secret registration happens at agent launch, scoped to the launched sandbox. See [Credentials](#credentials) below.

No images are built and nothing is pushed to a registry. `awman ready` for sbx is fast: kit emission and credential registration are text-file writes and short subprocess calls.

Every `sbx` command awman runs is announced in the status log before it executes. You can see exactly what awman is doing on your behalf.

### Kits — what they are and where they live

Instead of `Dockerfile.<agent>` files, the sbx runtime uses **kit YAML specs**. A kit is a directory containing a `spec.yaml` (which declares the base template image, install commands, network rules, and credential mappings) plus optional asset files (startup scripts, config templates).

Kits are generated by awman and stored globally, not per-repo:

```
~/.awman/kits/
├── claude/
│   ├── spec.yaml
│   └── files/home/.awman/apply-session-config.sh
├── codex/
│   ├── spec.yaml
│   └── files/home/.awman/apply-session-config.sh
└── …  (one directory per agent)
```

You do not edit these files by hand. Re-run `awman ready` to regenerate them (e.g., after upgrading awman or changing agent config). Use `awman ready --no-cache` to force a clean rebuild: awman removes any existing sandboxes for the affected agents and re-emits fresh kits. The next `awman chat` invocation pays the install cost again.

**Kit kinds:** Docker ships built-in sandbox templates for five agents (`claude`, `codex`, `gemini`, `copilot`, `opencode`). awman extends these as `kind: mixin` kits, which reuse Docker's published base and add awman-specific install and startup steps on top. For agents without a Docker built-in (`antigravity`, `crush`, `maki`, `cline`), awman emits `kind: agent` kits that build on a generic shell base image.

### Persistent sandbox lifecycle

Unlike Docker and Apple Containers, sbx sandboxes **persist between sessions**. The install cost (agent binary download, apt packages, etc.) is paid once per worktree-and-agent pair. After the first launch, subsequent `awman chat` calls restart the existing sandbox — the install does not re-run.

**First launch:**

```sh
awman chat --agent claude          # sbx creates the sandbox, installs the agent (one-time), attaches
```

awman runs `sbx create --kit ~/.awman/kits/claude --name awman-<hash>-claude claude <worktree>` (the worktree is the positional workspace path; it appears inside the VM at the identical absolute path), registers any sandbox-scoped secrets (see [Credentials](#credentials)), then attaches with `sbx run awman-<hash>-claude`.

**Subsequent launches (same worktree):**

```sh
awman chat --agent claude          # sbx restarts the existing sandbox, re-runs startup script, attaches
```

awman re-registers the sandbox-scoped secrets (so rotated keys apply), re-applies the per-launch session config (`sbx exec <sandbox> bash …/apply-session-config.sh` — the boot-time startup script only runs when the VM boots, so without this step a sandbox still running from a previous session would keep its old config and a changed `--model` would silently not apply), and runs `sbx run awman-<hash>-claude` (the existing sandbox is addressed by its positional name — `--name` is only valid when creating a new sandbox — and no `--kit` flag is passed, so the installed state is preserved).

**Teardown:**

```sh
sbx rm awman-<hash>-claude   # stops and removes the sandbox, clearing its persistent volume
```

**Listing running sandboxes:**

```sh
awman status               # shows awman-managed sandboxes across all runtimes
sbx ls                     # shows all sandboxes including non-awman ones
```

`awman status` includes a source marker for running containers. Regular user
sessions are marked `session`; containers launched by the squad daemon are
marked `squad:<task>` (for example, `squad:issue-triage`). The marker is
informational and may be visible whether or not you use squad.

**Sandbox naming:** awman names sandboxes `awman-<worktree-hash>-<agent>`. The hash is derived from the worktree's absolute path so the same worktree always produces the same name. Multi-agent workflows create one persistent sandbox per agent per worktree. When a workflow step runs, awman reuses the existing sandbox for that agent if one exists (restart, no reinstall), or creates a new one on first use. All steps that share the same agent share one sandbox — the per-agent install cost is paid once per worktree regardless of how many workflow steps use that agent.

### Credentials

The supported way to authenticate an sbx-backed agent is an `env(VAR)` overlay at launch time. Set the provider API key in your shell, then request it as an overlay:

```sh
export ANTHROPIC_API_KEY=sk-ant-...
awman chat --agent claude --overlay 'env(ANTHROPIC_API_KEY)'
```

At agent launch (not `awman ready`), awman reads the value from your shell environment and registers it with a **sandbox-scoped** `sbx secret set <sandbox> <service>`, so the sbx host proxy is authenticated by the time the agent starts. On first launch awman creates the sandbox first (`sbx create --kit …`), sets the scoped secrets, then attaches with `sbx run <sandbox>`. awman never sets global (`-g`) secrets: scoped secrets take effect immediately (global ones only apply at sandbox creation), your keys never leak into sandboxes awman doesn't manage, and removing a sandbox removes its secrets with it. Because rotated keys are re-registered on every launch, you never need to re-run `awman ready` after changing a key.

Each agent accepts a specific allowlist of auth variables (matching the [Docker Sandboxes credential services](https://docs.docker.com/ai/sandboxes/security/credentials/)):

| Agent | Supported env vars | sbx service |
|---|---|---|
| `claude` | `ANTHROPIC_API_KEY` | `anthropic` |
| `codex` | `OPENAI_API_KEY` | `openai` |
| `gemini` | `GEMINI_API_KEY` or `GOOGLE_API_KEY` | `google` |
| `copilot` | `GH_TOKEN` or `GITHUB_TOKEN` | `github` |
| `opencode` | `ANTHROPIC_API_KEY` | `anthropic` |

Rules:

- An `env(VAR)` overlay for a credential-class variable **not** on the agent's allowlist is dropped with a warning (it is never written anywhere workspace-readable).
- Launching with no supported auth overlay does not warn blindly. awman first probes for a pre-existing sbx secret that already covers the agent's provider — it runs `sbx secret ls --service <service>` (the well-known service name, e.g. `anthropic` for `claude`) and `sbx secret ls -g` (the global store). If a matching secret is found (from a previous launch, a manual `sbx secret set`, or a global key), awman prints an informational note that the existing credential will authenticate the agent and launches normally. Only when no pre-existing secret is found does it warn that auth must be configured manually (`sbx secret set <sandbox> <service>` — sandbox-scoped secrets apply immediately, even to a running sandbox) or via the agent's own login flow inside the sandbox — and the launch still proceeds either way.
- Custom-kit agents (`antigravity`, `crush`, `maki`, `cline`) do not participate in launch-time auto-auth yet; register their credentials manually with `sbx secret set`.
- Non-credential `env(VAR)` overlays (e.g. `LOG_LEVEL`) are unaffected — they reach the agent through `session.json` as before.

Credentials are piped to `sbx secret set` via stdin — they never appear in process listings, and they are masked in all awman output.

**Claude subscription tokens:** Docker Sandboxes does not yet support `CLAUDE_CODE_OAUTH_TOKEN` (the token from `claude setup-token`), so awman silently skips it — no warning is printed, since the token is surfaced on every launch whenever you are logged in to Claude Code and auth works without it. To use Claude Code under the sbx runtime, either set `ANTHROPIC_API_KEY` (the missing-auth warning above still fires when no supported variable is configured), or run `/login` inside the sandbox on first launch — the sbx credential proxy completes the OAuth flow and keeps the resulting token on the host. This is unchanged by, and unaffected by, the live-refreshed credential file awman writes for Claude under the Docker and Apple Containers runtimes (see [Agent authentication](03-agent-sessions.md#agent-authentication)) — sbx sessions still authenticate only through `ANTHROPIC_API_KEY` or in-sandbox login.

Non-credential config (model selection, mode flags, system prompts, tool lists) is written to `<workspace>/.awman/session.json` before each launch and read by the startup script inside the VM. Credential values are never written to `session.json`.

### Session config and seeded prompts

How awman delivers a seeded prompt (e.g., from `awman exec prompt --agent claude "fix the bug"`) depends on the kit kind:

| Kit kind | Prompt delivery |
|---|---|
| `kind: mixin` (`claude`, `codex`, `gemini`, `copilot`, `opencode`) | Written into the agent's stdin at launch (typed ahead for interactive sessions, piped for `--non-interactive`) |
| `kind: agent` (`antigravity`, `crush`, `maki`, `cline`) | Appended as a positional argument at launch |

Mixin kits launch their agent through Docker's built-in sandbox template, so awman cannot put the prompt on the command line — stdin injection is the single delivery path. The prompt is also recorded in `session.json` and staged by the startup script at `~/.awman/seeded-prompt.txt` inside the VM for reference, but that copy is informational, not a second delivery.

Mixin startup scripts consume these `session.json` fields where the agent supports them natively:

- `model` — sets the agent's default model
- `permission_mode` — `--plan` / `--auto` / `--yolo`; claude renders it as `permissions.defaultMode` (`plan` / `acceptEdits` / `bypassPermissions`) in its settings file. For `--yolo`, the script also sets `skipDangerousModePermissionPrompt: true` so claude does not ask for interactive confirmation of bypass-permissions mode — the sandbox is the isolation boundary, matching the container runtimes' behavior
- `system_prompt_inline.text` — applies an inline system prompt via the agent's native config
- `allowed_tools` / `disallowed_tools` — configures tool allow/deny lists

If a mixin agent cannot express a field through its native config, awman prints a warning at launch time rather than silently dropping the setting. `kind: agent` kits do not consume these fields from `session.json` — configuration is delivered as launch arguments appended after the `--` delimiter, mirroring the container runtimes' argv: the non-interactive subcommand/flag (e.g. `crush run`, `cline task`), mode flags (`--yolo`, …), the model flag (`--model <NAME>`), then the seeded prompt. (`antigravity` delivers system prompts through extra directory mounts under the container runtimes; those mounts don't exist under sbx, so awman warns that the system prompt was not applied.)

No prompt is delivered twice: mixin kits use stdin; `kind: agent` kits use positional arguments; neither path uses both.

### Supported agents

All nine awman agents are supported under `docker-sbx-experimental`:

| Agent | Kit kind | Notes |
|---|---|---|
| `claude` | mixin | Extends `docker/sandbox-templates:claude-code-docker` |
| `codex` | mixin | Extends `docker/sandbox-templates:codex-docker` |
| `gemini` | mixin | Extends `docker/sandbox-templates:gemini-docker` |
| `copilot` | mixin | Extends `docker/sandbox-templates:copilot-docker` |
| `opencode` | mixin | Extends `docker/sandbox-templates:opencode-docker` |
| `antigravity` | agent | Full install on `shell-docker` base |
| `crush` | agent | Full install on `shell-docker` base |
| `maki` | agent | Full install on `shell-docker` base |
| `cline` | agent | Full install on `shell-docker` base |

### Known limitations

**Platform:** macOS arm64 and Windows x86_64 only. Linux is blocked until docker/sbx-releases#51 is fixed upstream. Intel Macs are not supported.

**Networking:** all traffic goes through an HTTP/HTTPS proxy. Raw TCP, UDP, and ICMP are blocked. This means SSH-based git remotes, database connections, and other raw-socket protocols do not work inside the VM by default. Agents that need npm, pip, or curl to reach the internet will work; agents that need to SSH into a server will not.

**No per-resource stats:** `awman status` and the TUI stats panel show sandbox running/stopped state from `sbx ls`, but cannot report per-sandbox CPU or memory usage. The `docker stats`-style live resource view is unavailable for sbx sandboxes.

**`--allow-docker`:** Docker-in-Docker is always on inside an sbx sandbox (each VM has a private Docker daemon). The `--allow-docker` flag (which mounts the host daemon socket into Docker containers) is a no-op under sbx — it is ignored (noted in the debug log).

**Directory overlays, skills, and context mounts:** the VM can only see the workspace, which is virtiofs-mounted at sandbox creation. `dir(...)` overlays, skill mounts (`--include-all-skills` / `skill(...)`), and context directory mounts (`context(...)`) cannot be honored — awman warns at launch and continues without them. For `context(...)`, the rendered system prompt text is still delivered to the agent; only the directory mount is skipped.

**Mixin launch flags:** mixin kits launch the agent through Docker's built-in template, whose launch command takes no CLI flags from awman. Anything with a native config-file equivalent is delivered through `session.json` (see [Session config](#session-config-and-seeded-prompts)); the rest is warned about at launch instead of silently dropped:

| Option | claude | codex / gemini / opencode | copilot |
|---|---|---|---|
| `--model` | applied (settings file) | applied (settings/config file) | **not deliverable** — warned; use the `/model` slash command in-session |
| `--plan` / `--auto` / `--yolo` | applied (`permissions.defaultMode`) | **not deliverable** — warned | **not deliverable** — warned |
| `--non-interactive` | **not deliverable** — warned; the prompt is still piped to the agent's interactive entrypoint | same | same |

`kind: agent` kits (`antigravity`, `crush`, `maki`, `cline`) are unaffected: awman owns their entrypoint and passes mode flags, `--model`, and the non-interactive subcommand as launch arguments, exactly like the container runtimes. (`antigravity` rejects `--model` on every runtime — it has no model flag.)

**Port mappings:** if a workflow binds a port inside the sandbox, that port mapping is lost when the sandbox stops. Port-based workflows must keep the sandbox running continuously.

**Template drift:** if Docker updates a built-in sandbox template (e.g., changes the default entrypoint for `claude-code`), existing sandboxes keep the old template until they are destroyed and recreated. Run `awman ready --no-cache` to re-emit kits for the new template; existing sandboxes must be removed with `sbx rm` before the next launch picks up the new kit.

### Troubleshooting

**`sbx` not found:**
```
awman: 'sbx' not found on PATH. Install with: brew install docker/tap/sbx
```
Install `sbx` and re-run `awman ready`.

**Not logged in:**
```
awman: sbx ls failed — run 'sbx login' to authenticate Docker Sandboxes
```
Run `sbx login` and re-run `awman ready`.

**Linux or Intel Mac:**
```
awman: docker-sbx-experimental is not supported on this platform
```
Use `runtime: "docker"` or `runtime: "apple-containers"` instead.

**Stale kit version:**
```
awman: session.json schema_version mismatch — re-run 'awman ready'
```
The kit on disk was emitted by an older version of awman. Run `awman ready` to regenerate.

**Kit validation failure:**
```
awman: sbx kit validate failed for ~/.awman/kits/claude: <error from sbx>
```
The error text from `sbx kit validate` is printed verbatim. Common causes are a stale `sbx` binary (upgrade it) or a network issue during template pull. Run `awman ready --no-cache` to force a clean re-emit.

**Launch failure:**
```
sbx exited with code 1; last output:
ERROR: failed to create sandbox: <error from sbx>
```
When `sbx run` exits non-zero, awman replays the tail of sbx's output into the execution window alongside the exit code, so the failure is diagnosable without re-running the announced `sbx` command by hand.

---

## Switching runtimes

You can switch between runtimes at any time by changing the `runtime` setting. Each runtime maintains its own state:

- **Docker / Apple Containers:** per-repo `.awman/Dockerfile.<agent>` files and the local Docker / containerd image store. These are unchanged when you switch to sbx.
- **Docker Sandboxes:** `~/.awman/kits/<agent>/` (host-global kit files) and `<workspace>/.awman/session.json` (per-launch dynamic config written just before each launch). These are not touched when you switch to Docker or Apple Containers.

Switching runtimes does not delete state for the other runtimes. You can keep all three ready simultaneously.

**Switching mid-project:** if you run `awman chat` under Docker and then switch to sbx and run `awman chat` against the same worktree, you start a fresh sbx sandbox. In-VM state from the Docker container is not transferred (containers and microVMs are completely separate environments). Your Git repo state is shared — both runtimes see the same files on disk.

**Example: switch and switch back**

```sh
# Set up Docker (already done at init)
awman config set --global runtime docker
awman ready

# Try the sbx runtime
awman config set --global runtime docker-sbx-experimental
awman ready                         # emits kits, registers credentials
awman chat --agent claude                   # runs in sbx sandbox

# Switch back to Docker — Docker images are still there
awman config set --global runtime docker
awman chat --agent claude                   # runs in Docker container, unchanged
```

`awman ready` is per-runtime: running it with `runtime: "docker"` does not touch sbx kits; running it with `runtime: "docker-sbx-experimental"` does not touch Docker images.

---

[← GitHub Integration](10-github-integration.md) · [Next: squad →](12-squad.md)
