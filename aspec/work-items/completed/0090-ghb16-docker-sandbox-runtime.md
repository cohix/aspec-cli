# Work Item: Feature

Title: Docker Sandbox Runtime (`docker-sbx-experimental`) — `DSbxBackend` implementation
Issue: https://github.com/connerhicks/awman/issues/16

**Prerequisite**: work item 0089 (`AgentRuntimeEngine` abstraction). WI 0089 introduces the `AgentRuntimeEngine` trait family in Layer 1, splits the runtime tier into `ContainerRuntime` and `SandboxRuntime`, defines the `SandboxBackend` trait, and lands `DSbxBackend` as a stub that returns `EngineError::NotImplemented` for every operation. This work item replaces those stubs with a working implementation. Do not start implementation work on WI 0090 until WI 0089 is merged.

## Summary

Implement `DSbxBackend` — the concrete `SandboxBackend` driver for Docker Sandboxes (`sbx`) — as awman's first sandbox-class runtime backend. The runtime tier (`SandboxRuntime`), the backend trait (`SandboxBackend`), the option type (`ResolvedSandboxOptions`), the runtime detection wiring, and the stub `DSbxBackend` were established in WI 0089. This work item makes `DSbxBackend` actually work: kit emission, lifecycle management, credential injection, session config writing, naming, persistence.

Docker Sandboxes (GA since January 30, 2026; CLI binary: `sbx`) runs AI coding agents inside isolated **microVMs** rather than Linux containers, providing hypervisor-grade isolation — each sandbox gets its own dedicated kernel, private Docker daemon, filesystem, and proxied network stack. The `docker-sbx-experimental` config string is already recognized as of WI 0089; today it routes to `SandboxRuntime` and returns a clear "not yet implemented" error from `DSbxBackend`. After this work item, the same config string runs real sandboxes.

This runtime ships as **experimental** and is gated behind the config string `runtime: "docker-sbx-experimental"`. The user-facing display name is `"Docker Sandboxes (experimental)"`. The `-experimental` suffix is intentional and durable for the foreseeable future — it signals to users that the integration is subject to behavior changes (sbx itself has multiple open known bugs as of June 2026, the per-VM API is partly undocumented, and the Linux platform is blocked) and that awman's own integration may need to evolve as Docker iterates on sbx. Do not remove the `-experimental` suffix as part of this work item; that is a separate, deliberate promotion decision that will happen later in its own work item once the underlying sbx ecosystem and awman's coverage are stable.

This work item implements sbx support using **Kit YAML** as the primary integration mechanism, not custom OCI images. Awman's current per-agent `Dockerfile.<agent>` setup is replaced (for the sbx runtime only) by per-agent generated kit specs that lean on Docker's published base templates plus install/startup commands. There is **no registry push, no `bollard` crate, no socket-based image injection**, and no custom OCI image build for sbx — host-side containerd pulls Docker's public sandbox templates and the kit's install commands do the rest. The Docker and Apple paths continue to use their existing Dockerfile-based flow unchanged.

### What docker-sbx-experimental is

Docker Sandboxes is a purpose-built agent isolation product from Docker, distinct from Docker Desktop. It is not a general container runtime — it was designed specifically for running AI coding agents (Claude Code, Codex, Gemini CLI, etc.) in an environment where they can operate in YOLO mode without risking the host system. Isolation is via a custom VMM (not Firecracker) using native hypervisor APIs per platform: `Hypervisor.framework` on macOS, Windows Hypervisor Platform on Windows, and KVM on Linux. Each microVM runs Linux kernel 6.12 in an Ubuntu guest OS with 4 vCPUs and ~50% of host RAM (configurable). The `sbx` binary is standalone — Docker Desktop is not required.

**Critical execution model fact** (governs all image and stats decisions): sbx uses **containerd with the `nerdbox` runtime** on the host to create each microVM. The `--template` image is an OCI image pulled by **host-side containerd** and used directly as the **VM's root filesystem**. The agent binary runs natively as a process in the VM — it is **not** running inside a Docker container managed by any container daemon. Each VM additionally runs its own private Docker daemon, but that daemon serves only the agent's Docker-in-Docker needs; it plays no role in booting the VM or running the agent process.

### How it differs from Docker and Apple Containers

| Dimension | Docker (`docker`) | Apple Containers (`container`) | Docker Sandbox (`sbx`) |
|---|---|---|---|
| Isolation unit | Linux container (shared kernel) | Lightweight VM per container | MicroVM per sandbox session |
| Host escape risk | Container escape = host root | Hypervisor CVE required | Hypervisor CVE required |
| Image store | Shared with host | Shared with host | Private per VM; host images not visible |
| Image source | Local or registry | Local or registry | Registry only — template image is the VM root filesystem, pulled by host-side containerd |
| Image reference | `org/img` auto-resolves to `docker.io` | Same | Full domain required (`docker.io/org/img`) |
| Customization model | Custom Dockerfile + `docker build` | Same | **Kit YAML — install/startup commands on top of Docker's published templates** |
| Volume mounts | `-v host:container` bind mounts | `-v host:container` bind mounts | virtiofs passthrough; workspace appears at identical absolute path inside VM |
| User home config | Mountable anywhere | Mountable anywhere | Not accessible — only mounted workspace paths visible |
| Env vars | `-e KEY=VALUE` or `--env-file` | Same | Not inherited from host shell — via Kit `environment.variables` / keychain secrets / proxy |
| Networking | Host network or bridge; raw TCP/UDP | Native macOS network per container | All traffic through HTTP/HTTPS proxy; raw TCP/UDP/ICMP blocked |
| Docker-in-Docker | Requires `--privileged` | Limited | Fully supported (private Docker daemon per VM) |
| Startup time | Milliseconds | ~1 second | 2–5 seconds cold start (sbx) + per-launch startup script |
| Stats | `docker stats` | `container stats` | `sbx ls` (status only; no per-resource metrics) |
| macOS arch support | x86_64 + arm64 | arm64 (Apple Silicon) | arm64 only (Apple Silicon) |
| Linux support | Full | No | x86_64 only; virtiofs file-creation bug (Issue #51) blocks it |
| Windows support | x86_64 | No | x86_64 |
| Platform required | Docker daemon | macOS 26 Tahoe | `sbx login` + Docker account (free tier OK) |

### Platform availability

- **macOS**: arm64 (Apple Silicon) only — no Intel Mac support
- **Windows**: x86_64 via Windows Hypervisor Platform
- **Linux**: x86_64 via KVM, but **a confirmed virtiofs bug (Issue #51, open as of June 2026)** prevents agents from creating any new files inside the workspace. Effectively non-functional for coding agents until upstream-resolved. Linux must be gated with a clear user-facing error.

### Integration model: Kit YAML (not OCI images)

awman integrates with sbx by generating a per-agent **Kit YAML spec** at `awman ready` time, then invoking `sbx run --kit <kit-dir> [--name <name>] <agent>` to launch sandboxes. The kit handles everything awman's current `Dockerfile.<agent>` does, but at the kit level (a directory containing `spec.yaml` plus optional `files/` assets) rather than a built OCI image.

The four sbx kit injection tiers, ordered from one-time/expensive to per-launch/dynamic:

| Tier | Runs when | Re-runs on restart? | Use for |
|---|---|---|---|
| `files/` (static bundled) | At sandbox creation | No | Static assets awman ships per agent (startup scripts, prompt templates) |
| `commands.install` | Once at `sbx create` | **No** (until `sbx rm`) | Heavy one-time setup: agent binary install, apt packages, language toolchains |
| `commands.initFiles` | Every sandbox start | Yes | Templated files using `${WORKDIR}` substitution |
| `commands.startup` | Every sandbox start | Yes (must be idempotent) | Dynamic per-launch config, refresh logic |

`persistence: persistent` makes the post-install VM filesystem survive between `sbx stop` / `sbx run <existing-name>`. This converts awman from its current "ephemeral container per session" model to "persistent sandbox per worktree" on the sbx backend — pay the install cost once per worktree, then every subsequent invocation just restarts and re-runs idempotent startup scripts.

### Built-in agent matrix

Docker ships sbx base templates for these agents; awman uses `kind: mixin` to extend each one, avoiding redundant install steps:

| awman agent | Docker built-in identifier | Template image | Kit kind |
|---|---|---|---|
| `claude` | `claude-code` | `docker/sandbox-templates:claude-code-docker` | `mixin` |
| `codex` | `codex` | `docker/sandbox-templates:codex-docker` | `mixin` |
| `gemini` | `gemini` | `docker/sandbox-templates:gemini-docker` | `mixin` |
| `copilot` | `copilot` | `docker/sandbox-templates:copilot-docker` | `mixin` |
| `opencode` | `opencode` | `docker/sandbox-templates:opencode-docker` | `mixin` |
| `antigravity` | (none) | extends `:shell-docker` | `agent` |
| `crush` | (none) | extends `:shell-docker` | `agent` |
| `maki` | (none) | extends `:shell-docker` | `agent` |
| `cline` | (none) | extends `:shell-docker` | `agent` |

The five mixin agents must verify in Phase 0 that Docker's default entrypoint matches awman's mode-flag matrix (yolo, auto, plan). If not — e.g., awman needs plan mode but Docker's `claude` built-in is hard-coded to `--dangerously-skip-permissions` — that agent falls back to `kind: agent` with awman's preferred entrypoint, extending the same template.

For the four `kind: agent` paths, awman emits a full agent spec that extends `docker/sandbox-templates:shell-docker` (or `:shell` for non-DinD agents) and installs the agent binary via `commands.install`, exactly as Docker's own reference Pi and Amp kits do.

### Credentials and dynamic per-launch state

awman provides three credential delivery mechanisms, used in combination:

1. **`environment.proxyManaged`** (preferred for HTTP API keys): the host-side proxy holds the real key and rewrites outbound request headers. The placeholder value (or a `{rand}` token) goes into the VM; the real key never enters the VM address space.
2. **`sbx secret set <sandbox> <service>`** (preferred for credentials that the agent reads as env vars at boot): awman registers credentials sandbox-scoped, never globally (`-g`) — scoped secrets take effect immediately (global ones only apply at sandbox creation), and awman's keys never leak into sandboxes it does not own. The credential value is piped via stdin (never argv) to avoid process-listing leakage. Run at agent-launch time, after the sandbox exists, so rotated keys take effect without re-running `awman ready`.
3. **Workspace-backed session config** (for non-credential dynamic state): awman writes `<workspace>/.awman/session.json` before each launch with current mode flags, model selection, system prompt, disallowed tools, etc. A `commands.startup` script inside the kit reads this file and renders the corresponding agent config (`.claude/settings.json`, env files, etc.) inside the VM. This is the sandbox-side equivalent of the Container-side `ContainerOption::AgentSettingsPassthrough` — the workspace is the only host→VM channel available at launch time, so `ResolvedSandboxOptions::agent_settings` is delivered via this structured config-passing surface instead of a bind mount.

There is no `sbx run`-level flag to inject env vars or override kit variables at launch time. Anything that needs to change between launches without rebuilding the kit must go through the workspace-backed `session.json` channel.

### Seeded prompts and system prompts

The system-prompt fields on `ResolvedSandboxOptions` — `system_prompt_file`, `system_prompt_env_file`, `system_prompt_inline` — mirror their counterparts on `ContainerOption` (`SystemPromptFile` / `SystemPromptEnvFile` / `SystemPromptInline`) and all work under sbx:
- File-based prompts: workspace paths are visible inside the VM at their identical absolute paths via virtiofs — no path translation needed.
- Env-var-based prompts pointing at a workspace file: same as above; the env var is set via the kit's `environment.variables` block referencing the workspace path.
- Inline prompts: passed as positional arg appended to `agent.entrypoint.run` at kit emission time, or via the startup script reading them from `session.json`.

### Listing and stats

`sbx ls` returns sandbox state; the Phase 0 research must determine whether `sbx ls --json` is available (June 2026 timeline). Stats are degraded: because the agent runs directly in the VM (not as a Docker container), there are no Docker container stats to poll. The unified `AgentStats` type (defined by WI 0089's `AgentRuntimeEngine`) returns zeroed CPU/memory values with a running/stopped status indicator from `sbx ls` when produced by `SandboxBackend`. `bollard` is not added — there's no agent container in the in-VM DinD daemon to query.

### Interactive PTY and re-attach

`sbx exec -it <name> <cmd>` opens an interactive PTY session against a running sandbox. A known PTY bug (Issue #63) causes `stty size` to report `0 0` inside `sbx exec` sessions; awman's workaround is to pass `COLUMNS` and `LINES` as env vars on the `sbx exec` invocation (Phase 0 verifies this is effective).

---

## Non-interference with existing backends (load-bearing)

This work item must not change any user-observable behavior of the `docker` or `apple-containers` backends. Both must continue to function exactly as they do today on every user's machine after this change ships. Specifically:

- **No existing test may be modified to accommodate the sbx backend.** Every existing test in `src/engine/container/{docker,apple,backend,instance,options,runtime,naming,background,io_bridge}.rs` keeps its current assertions verbatim. If a test needs to change to pass after the sbx work, the sbx work is wrong, not the test.
- **No existing `templates/Dockerfile.*` file may be edited.** The sbx kit templates are added as new files (`templates/sbx-kit.<agent>.yaml`, `templates/sbx-apply.<agent>.sh`) with non-colliding names. Anyone who runs `awman ready` against `runtime: "docker"` or `runtime: "apple-containers"` gets exactly the same files in `.awman/` as today.
- **No existing function signatures change.** Specifically: `ContainerRuntime::build_image()` is **not** modified (no new parameters, no new return shape). `ContainerBackend` trait gets no new required methods. `ContainerOption` gets no new variants. The existing `agent_dockerfile_for()`, `project_dockerfile_dev()`, `dockerfile_matches_template()` and friends in `src/data/templates/mod.rs` keep their signatures and bodies; new `sbx_kit_template_for()` / `sbx_apply_script_for()` functions are additive.
- **No new mandatory dependencies on `Cargo.toml`.** Specifically, no `bollard`, no async client crates, no new heavy transitive deps. The sbx backend uses `std::process::Command` like the existing two backends.
- **Default runtime remains `docker`.** Users who do not set `runtime` in `GlobalConfig` continue to get Docker. The unknown-runtime fallback (`runtime: "blarg"` → warn + fall back to Docker) is preserved exactly as today.
- **`Backend::DockerSbx` is added to the internal `Backend` enum without re-ordering or renaming the existing `Docker` and `Apple` variants.** Append at the end. Pattern matches on `Backend` in existing code must keep working with the new variant only added in arms that need it.
- **`display_name()` and `cli_binary()` arms for the new backend are added before any wildcard `_ =>` arm**, but the existing arms keep their order and contents. Adding sbx must not change what `runtime_name() == "docker"` callers see.
- **`is_available()` for sbx must not call `docker info`.** The existing Docker `is_available()` continues to call `docker info`; the sbx variant probes `sbx ls`. The two probes are independent — having sbx installed must not break the Docker check, and vice versa.
- **Existing label, name, and filter conventions are unchanged.** The Docker backend continues filtering on `label=awman=true` and `name=awman-`. The Apple backend continues using its existing JSON parsing. The sbx backend uses a distinct name pattern (`awman-<worktree-hash>-<agent>`) that shares the `awman-` prefix for human-recognizability but does not collide with Docker container IDs because the two daemons see disjoint resources.
- **Existing `generate_container_name()` is not changed.** The sbx name generator is the WI 0089 helper `generate_sandbox_name(worktree_hash, agent)` in `src/engine/sandbox/naming.rs`. Docker and Apple paths keep calling the unchanged `generate_container_name()`.

The verification gate for non-interference is: after this work item ships, running the full existing test suite with `cargo test` on every supported platform (macOS arm64, macOS x86_64, Linux, Windows) must produce identical results to the pre-change suite, modulo the new sbx-specific tests that are added.

---

## Runtime switching and coexistence on the same machine

A user must be able to switch between `runtime: "docker"`, `runtime: "apple-containers"`, and `runtime: "docker-sbx-experimental"` on the **same machine, same project, same working directory** at will, with each runtime working without interruption or degradation. The user may flip the runtime config between two `awman` invocations, run a workflow with one and then immediately switch and run another workflow with a different one, then switch back. Each runtime maintains its own state without interfering with the others' state.

Concretely:

- **Independent state directories**. Per-runtime state is segregated by path:
  - Docker / Apple: existing `.awman/Dockerfile.<agent>` per-repo Dockerfiles and the local Docker / containerd image store. No change.
  - sbx: `$HOME/.awman/kits/<agent>/spec.yaml` (host-global kits), `<workspace>/.awman/session.json` (per-launch dynamic state — same workspace dir is shared but only sbx writes this file), and sbx's own per-VM persistent volumes (managed by `sandboxd`).
  - These three directory sets do not overlap. Switching runtimes does not require deleting or moving files for the other runtimes.
- **`awman ready` is per-runtime, not global.** Running `awman ready` with `runtime: "docker"` does what it does today (build images via `docker build`). Running `awman ready` with `runtime: "docker-sbx-experimental"` emits and validates kits (credential registration happens at agent launch, not ready) — it does NOT touch existing Docker images or local Apple containers. Switching the runtime and re-running `ready` makes the new runtime ready without invalidating the prior one. The user may keep both ready simultaneously.
- **`awman chat`, `awman exec`, `awman exec workflow` are runtime-routed at dispatch time** via `AgentRuntimeEngine::detect(&global_config)`. Each invocation uses the runtime named in the config at that moment. No persistent in-memory state carries between invocations.
- **No daemon shutdown.** Switching to sbx does not stop the Docker daemon and does not stop Apple's containerd. Switching back from sbx does not stop `sandboxd`. All three daemons may run simultaneously; the host has the resources for it (sbx claims ~50% of host RAM per running VM but is dormant when no sandbox is up).
- **No port or socket conflicts.** Docker uses `/var/run/docker.sock`; Apple uses its own socket; sbx uses `~/.docker/sandboxes/sandboxd.sock`. No two of these collide. Awman never mounts any of these into another runtime's containers.
- **Concurrent runs across runtimes are supported.** A user may run `awman chat` on terminal A with `runtime: "docker"` and `awman chat` on terminal B with `runtime: "docker-sbx-experimental"` against the same worktree. Each gets a separate container/sandbox; awman's worktree locking already prevents work-on-same-files conflicts. The runtimes themselves do not require mutex.
- **Switching mid-worktree-lifecycle is allowed but documented.** If a user runs `awman chat claude` under Docker, then switches to sbx and runs `awman chat claude` against the same worktree, they will start a fresh sbx sandbox (cold) because the prior session was in Docker. Awman does not transfer in-VM state between runtimes (impossible by design — Docker containers and sbx VMs are completely separate). Documentation states this clearly: switching runtimes restarts the agent context.
- **Stats polling for each runtime is independent.** The TUI stats panel polls `docker ps` / `container list` / `sbx ls` only for whichever runtime is currently selected in config. It does not eagerly poll all three. If the user has Docker running with no sbx sandboxes and switches to sbx config, polling switches to `sbx ls`.
- **`awman ready` for one runtime does not require the other runtimes to be installed.** Running `awman ready` with `runtime: "docker"` does not require `sbx` to be on PATH. Running with `runtime: "docker-sbx-experimental"` does not require Docker daemon to be running, only the `docker` CLI binary (for credential pre-flight in some agents) and `sbx`.
- **Test gate**: a dedicated integration test, env-gated and platform-gated, exercises the switching path on macOS arm64: run a no-op chat under `runtime: "docker"`, change config to `runtime: "docker-sbx-experimental"`, run a no-op chat under sbx, change back, run another Docker chat. All three must succeed and produce expected outputs without manual cleanup between steps. This test sits next to the existing per-backend integration tests and is gated behind `AWMAN_TEST_SBX=1` plus `#[cfg(all(target_os = "macos", target_arch = "aarch64"))]`.

The implementing agent must add a regression test in `tests/` (or extend an existing harness) that drives the runtime-switching scenario described above and asserts that switching back to Docker after using sbx does not break Docker functionality. This test does not need to actually spawn an sbx sandbox (which requires the runtime to be installed on CI); it can stub `is_available()` and instead assert that the in-process state transitions cleanly when `GlobalConfig::runtime` is mutated between calls to `AgentRuntimeEngine::detect`.

---

## Subprocess transparency via the user message sink (load-bearing)

The user must have full visibility into everything awman does with the `sbx` CLI on their behalf. Every `sbx` subprocess invocation — across `awman ready`, launch, re-attach, stop, remove, listing, and validation — is reported through the existing user message sink (`UserMessageSink`), so the messages surface uniformly in the TUI status log, CLI output, and API event stream.

Concretely:

- **Command announcement**: immediately before spawning any `sbx` subprocess, emit an Info-level message containing the full argv that is about to run (e.g. `Running: sbx run --kit ~/.awman/kits/claude --name awman-ab12-claude claude /path/to/wt`). This applies to every `sbx` invocation without exception: `sbx run`, `sbx create`, `sbx exec`, `sbx stop`, `sbx rm`, `sbx ls`, `sbx secret set`, `sbx kit validate`, and any others added later.
- **stdout/stderr publication**: all stdout and stderr produced by these `sbx` invocations is published on the sink as it is produced (line-buffered streaming where the invocation is long-running, e.g. install steps during first launch; a single post-exit message is acceptable for short, quiet commands like `sbx ls`). stdout lines are emitted at Info level; stderr lines at Warning level (or Error when the command exits non-zero).
- **Exit reporting**: when an `sbx` subprocess exits non-zero, the failure message must include the command that ran and its captured stderr, in addition to the wrapped `EngineError::Sandbox(...)`.
- **Exception — the interactive agent PTY**: the agent's own interactive I/O (the PTY bridged `sbx run` / `sbx exec -it` session the user is typing into) flows through the container/sandbox I/O bridge as today, not through the message sink. The announcement message for that invocation is still emitted before the PTY takes over. However, when the bridged `sbx run` exits **non-zero**, the tail of its captured output (control sequences stripped) is replayed on the message sink at Error level — launch failures (kit compose errors, login problems) otherwise vanish with the PTY, leaving only an exit code and a by-hand sbx rerun to diagnose.
- **Credential masking**: `sbx secret set` invocations are announced like any other command, but the secret value never appears in any message — it is piped via stdin (per Phase 4) and the announcement shows only the service name (e.g. `Running: sbx secret set awman-ab12-claude anthropic (value piped via stdin)`). Any stdout/stderr from `sbx secret set` is scanned by the existing masking helpers before publication in case the CLI echoes input.

This mirrors how the Docker and Apple backends report their lifecycle phases today, but is stricter: because sbx is experimental and its CLI behavior may shift between releases, the user must always be able to see exactly which commands awman ran and what they printed, without raising verbosity flags or consulting external logs.

---

## User Stories

### User Story 1
As a: security-conscious developer
I want to: configure awman to run my coding agents using Docker Sandboxes (`docker-sbx-experimental`) instead of Docker containers
So I can: benefit from microVM-grade isolation when running agents in YOLO/auto mode, ensuring that even a misbehaving agent cannot escape to my host filesystem, host Docker daemon, or local network beyond what the sbx network proxy permits.

### User Story 2
As a: user who has switched to docker-sbx-experimental
I want to: use awman's full workflow suite — multi-agent workflows, seeded prompts, context overlays, and system prompts — with as few behavioral changes as possible
So I can: leverage the same awman workflow files I already have, with clear documentation of any features that behave differently or are unavailable under the sbx runtime.

### User Story 3
As a: user who tries to configure `docker-sbx-experimental` on an unsupported platform or with an unsupported feature
I want to: receive a clear, specific error message explaining what is not supported and why (rather than a panic or a silent fallback to a different runtime)
So I can: understand exactly what I need to change in my setup or workflow to move forward.

### User Story 4
As a: developer using awman with `docker-sbx-experimental`
I want to: pay the agent-install cost only once per worktree, not once per chat invocation
So I can: keep agent invocation responsive after the first launch — every subsequent `awman chat` against the same worktree restarts an existing sandbox rather than rebuilding it.

### User Story 5
As a: developer who has both Docker and sbx installed
I want to: switch between `runtime: "docker"` and `runtime: "docker-sbx-experimental"` on the same machine and same project at any time
So I can: try the experimental sbx runtime for one task, switch back to Docker for another, and never lose functionality or have to manually clean up state when switching — each runtime keeps its own separate state, and changing the config string is the only action needed.

---

## Implementation Details

### Phase 0 — Research gate (before writing any code)

Report findings to the developer for approval before proceeding.

1. **Linux virtiofs bug status (Issue #51 in `docker/sbx-releases`)**: confirm whether resolved upstream. If still open, Linux remains blocked with a descriptive error.
2. **`sbx ls --json` schema**: verify a machine-readable output mode exists, capture the field shape. Determines `list_running()` parsing.
3. **PTY size workaround (Issue #63)**: verify that passing `COLUMNS` and `LINES` to `sbx exec` propagates effectively to TUI apps inside the VM.
4. **`sbx secret set` behavior**: verify that secrets registered for well-known services (`anthropic`, `openai`, `github`, `google`, `aws`, `groq`, `mistral`) are auto-injected at VM boot for matching agents without additional Kit YAML configuration.
5. **`sbx run --name` collision and reattach semantics**: confirm that `sbx run --name <existing-stopped-name>` restarts the existing sandbox (rather than erroring or recreating). This is load-bearing for the per-worktree persistent-sandbox model.
   - **RESOLVED (2026-06-11, observed against real sbx)**: it errors — `--name` is creation-only (`ERROR: sandbox '<name>' already exists; --name can only be used when creating a new sandbox`). An existing sandbox is restarted by passing its name as the positional: `sbx run <name> [-- AGENT_ARGS...]` (no `--kit`, no agent — both are baked into the sandbox). The persistent-sandbox model holds; only the restart argv shape differs from the assumption above.
6. **Per-agent default entrypoints**: for each of the five mixin candidates (`claude-code`, `codex`, `gemini`, `copilot`, `opencode`), capture the default `entrypoint.run` Docker's template uses. If awman needs to override (e.g., to switch yolo/auto/plan mode), that agent must use `kind: agent` with awman's chosen entrypoint, not `kind: mixin`. Produce a per-agent decision table at the end of Phase 0.
7. **Kit feature surface verification**: confirm via the official Docker sandbox-templates that the built-in five agents accept the credential-delivery mechanism awman plans to use (proxyManaged vs sbx secret set) for each agent's required keys.

Do not proceed past Phase 0 without developer approval of the findings.

### Phase 1 — Inherits from WI 0089

Runtime detection, the `docker-sbx-experimental` config-string match, the `SandboxRuntime` struct, the `SandboxBackend` trait, the platform guards (Linux block, Intel-Mac block), `display_name()` returning `"Docker Sandboxes (experimental)"`, `cli_binary()` returning `"sbx"`, and the stub `DSbxBackend` are all delivered by WI 0089. This work item starts at Phase 2 — there is no Phase 1 here. Phase numbering below is preserved from the pre-split work item to keep the body intelligible; the implementing agent should treat "Phase 1" as a no-op gate confirming WI 0089's work landed correctly.

Verification before starting Phase 2: run `awman exec --runtime docker-sbx-experimental ...` against any agent. The expected result is a clear `EngineError::NotImplemented("DSbxBackend: <method>")` from whichever `SandboxBackend` method the call would hit. If the runtime is mis-routed (the call lands on `ContainerRuntime`) or the error type is wrong, return to WI 0089 — do not work around it here.

### Phase 2 — Kit emission

The kit emitter generates a `spec.yaml` (plus optional `files/` directory) per agent at `awman ready` time. Kits live at `$HOME/.awman/kits/<agent>/` and are regenerated on every `awman ready` so they reflect the current per-agent config and credential mappings. Credential values are never written into kit files.

**File layout**: paths resolved through Layer 0 (`src/data/fs/`):
- `$HOME/.awman/kits/<agent>/spec.yaml` — the kit manifest
- `$HOME/.awman/kits/<agent>/files/home/` — bundled assets copied into `/home/agent/` at sandbox creation
- `$HOME/.awman/kits/<agent>/files/home/.awman/apply-session-config.sh` — the per-launch startup script (one per agent, sources from awman's own per-agent templates)

**Mixin path** (built-in five): emit `kind: mixin` with:
- No `agent:` block (the built-in agent provides its own entrypoint).
- No `credentials:` block — the built-in kit the mixin extends already defines the well-known credential source (`anthropic`, `openai`, `google`, `github`), and sbx compose rejects a credential source defined in both a kit and a mixin extending it (`compose: credential source "<service>" defined in both ...`). Values reach sbx via sandbox-scoped `sbx secret set <sandbox> <service>` at launch instead.
- `environment.proxyManaged` for HTTP API keys where proxy interception is preferable.
- `commands.install` for any awman-specific tooling not already in Docker's template.
- `commands.startup` invoking `apply-session-config.sh`.
- `agentContext:` block with awman-context boilerplate. (Originally `memory:`; kit-spec v2 deprecated that name — sbx warns `deprecated field "memory": use 'agentContext' instead` at compose.)

**Agent path** (the other four): emit `kind: agent` with:
- `agent.image: docker/sandbox-templates:shell-docker` (or `:shell` when DinD isn't needed for that agent).
- `agent.entrypoint.run` constructed from awman's per-agent argv (entrypoint + mode flags).
- `agent.persistence: persistent`.
- `commands.install` running the agent's install steps (curl-to-bash, npm install -g, apt, etc.) — equivalent of today's `Dockerfile.<agent>` body.
- `commands.startup` invoking `apply-session-config.sh`.
- Other blocks (network, environment, agentContext) as for the mixin path, plus `credentials.sources` keyed by sbx service id, each value an object with an `env:` list (service → `{env: [...]}`, not env-var → service) — agent kits extend no built-in agent, so they must declare the source themselves and no compose conflict arises.

**Schema corrections validated against `sbx kit validate` v0.32.0** (the shapes
originally sketched above predate testing against a real sbx build):
- `schemaVersion: "1"` and `name:` are required top-level fields in every kit.
- `persistence` exists only under the `agent:` block (`agent.persistence`);
  mixins cannot declare it — the parent template/`sbx run` governs persistence.
- The well-known agent name for Claude Code is `extends: claude`, not
  `claude-code`.
- `commands.startup` entries are objects whose `command` is an argv array
  (`- command: ["bash", "..."]`); `commands.install` entries are objects whose
  `command` is a shell string. Bare string list entries fail strict unmarshal.
- Mixins must not redeclare a credential source their base kit defines: the
  built-in agent kits declare their own well-known sources, and compose fails
  with `compose: credential source "anthropic" defined in both "claude" and
  "awman-claude"`. Only `kind: agent` kits carry a `credentials:` block.
- `CLAUDE_CODE_OAUTH_TOKEN` has no sbx service mapping (docker/sbx-releases#11
  open as of June 2026); awman warns with the supported alternatives
  (`ANTHROPIC_API_KEY` or in-sandbox `/login` via the credential proxy).

**Source of truth — template files live in `templates/` alongside Dockerfile templates**. Per-agent kit templates are stored at:

```
templates/
├── Dockerfile.claude              (existing — unchanged)
├── Dockerfile.codex               (existing — unchanged)
├── Dockerfile.gemini              (existing — unchanged)
├── …                              (other existing Dockerfiles — unchanged)
├── sbx-kit.claude.yaml            (new)
├── sbx-kit.codex.yaml             (new)
├── sbx-kit.gemini.yaml            (new)
├── sbx-kit.copilot.yaml           (new)
├── sbx-kit.opencode.yaml          (new)
├── sbx-kit.antigravity.yaml       (new)
├── sbx-kit.crush.yaml             (new)
├── sbx-kit.maki.yaml              (new)
├── sbx-kit.cline.yaml             (new)
├── sbx-apply.claude.sh            (new — per-agent startup script)
├── sbx-apply.codex.sh             (new)
└── …                              (one per agent)
```

The filename prefix `sbx-kit.` and `sbx-apply.` guarantees no collision with the existing `Dockerfile.<agent>` files even though they share the `templates/` directory. The Dockerfile templates remain at their existing paths with their existing contents — this work item must not modify any existing `templates/Dockerfile.*` file.

**Bundling mechanism — identical to existing Dockerfile templates**: the existing template-inclusion module (`src/data/templates/mod.rs`) embeds Dockerfile templates at compile time via `include_str!("../../../templates/Dockerfile.<agent>")` and surfaces them through `agent_dockerfile_for(agent: &str) -> Option<&'static str>`. The sbx kit templates follow the **exact same pattern**:

```rust
// src/data/templates/mod.rs — additions, NOT replacements
pub fn sbx_kit_template_for(agent: &str) -> Option<&'static str> {
    Some(match agent {
        "claude" => include_str!("../../../templates/sbx-kit.claude.yaml"),
        "codex" => include_str!("../../../templates/sbx-kit.codex.yaml"),
        // … one arm per agent
        _ => return None,
    })
}

pub fn sbx_apply_script_for(agent: &str) -> Option<&'static str> {
    Some(match agent {
        "claude" => include_str!("../../../templates/sbx-apply.claude.sh"),
        // … one arm per agent
        _ => return None,
    })
}
```

The existing `agent_dockerfile_for()` and `project_dockerfile_dev()` functions in this module remain untouched — the sbx functions sit alongside them. The two template families share the directory but not their accessor functions, so a caller that wants a Dockerfile gets a Dockerfile and a caller that wants a kit YAML gets a kit YAML; there is no way to confuse them.

If the project ever adds a remote template-download path for the Dockerfiles (analogous to `download_aspec_tarball` in `src/data/network/`), the sbx kit templates plug into the same mechanism with the same naming convention. They are not a separate distribution surface.

**Module location for the emitter**: `src/engine/sandbox/dsbx/kit.rs`, `pub(super)`. Provides `DSbxKitEmitter` with methods like `emit_for_agent(&self, agent: &AgentSpec, dest: &Path) -> Result<(), EngineError>`. Called from the Layer 1 ready engine's sbx-specific ready phases; `ReadyCommand` only selects and invokes the ready engine through the normal Layer 2 command path. The emitter:
1. Reads the embedded kit YAML template via `sbx_kit_template_for(agent)`.
2. Substitutes per-installation values (awman version, base image tag, any per-agent flags).
3. Writes the rendered `spec.yaml` to `<dest>/spec.yaml`.
4. Reads the embedded apply script via `sbx_apply_script_for(agent)` and writes it to `<dest>/files/home/.awman/apply-session-config.sh` with mode 0755.

**`apply-session-config.sh`**: bash script bundled per agent in `templates/sbx-apply.<agent>.sh`. Reads `$WORKDIR/.awman/session.json`, applies the agent-specific in-VM config (writes `$HOME/.claude/settings.json` for claude, equivalent files for others). Must be idempotent — re-invoked by `commands.startup` on every sandbox restart. Bundled via `include_str!` like the YAML templates, written into the emitted kit's `files/home/.awman/` at `awman ready` time.

### Phase 3 — `DSbxBackend` implementation

Create `src/engine/sandbox/dsbx/backend.rs` with `pub(super) struct DSbxBackend` implementing the `SandboxBackend` trait (defined in WI 0089). Concrete type is invisible outside `src/engine/sandbox/dsbx/`.

**`build()`** — constructs a `DSbxSandboxInstance` from `ResolvedSandboxOptions`:
- Resolves the kit directory path for the agent.
- Resolves the persistent sandbox name (see "Naming and persistence" below).
- Stores the resolved options for use by `run_with_frontend()`.

**`DSbxSandboxInstance::run_with_frontend()`** — performs all subprocess side effects:
1. Write `<workspace>/.awman/session.json` from the resolved options (`DSbxBackend` calls `DSbxSessionConfig::write_for(&options, workspace)`).
2. Ensure the sandbox exists: when no sandbox with the resolved name exists, run `sbx create --kit <kit-dir> --name <name> <agent> <workspace>` (announced). This must precede credential registration — secrets are sandbox-scoped.
3. Register credentials: `auto_auth_env_overlays` (allowlisted `env(VAR)` overlays) then `inject_credentials` (awman-resolved credentials), all via sandbox-scoped `sbx secret set <name> <service>` with values piped via stdin. A failure after a fresh create leaves the sandbox in place (the next launch reuses it) and the error says so.
4. Launch with `sbx run <name> [-- AGENT_ARGS...]` — the positional name addresses the existing sandbox; `--name` is creation-only, and the kit/agent/workspace are baked in at creation.
5. Spawn via the same `portable-pty` / piped-stdio bridge pattern used by `DockerContainerInstance` and `AppleContainerInstance`.

All `sbx` subprocess invocations made by `DSbxBackend` and `DSbxSandboxInstance` go through a single spawn helper that implements the "Subprocess transparency via the user message sink" requirements above: announce the argv on the sink before spawning, stream/publish stdout and stderr on the sink, and report non-zero exits with the command and captured stderr. Do not scatter ad-hoc `Command::new("sbx")` calls that bypass this helper.

**CLI corrections validated against the real `sbx run` (Docker CLI reference,
June 2026)** (the argv shapes originally sketched in this work item predate
testing against a real sbx build):
- The synopsis is `sbx run [flags] SANDBOX | AGENT [PATH...] [-- AGENT_ARGS...]`.
  There is **no `--workspace-dir` flag** — workspace paths are positionals
  after the agent name, extra workspaces may be suffixed `:ro`, and with no
  PATH given sbx uses the invoking cwd. `sbx create` follows the same shape.
- Anything intended for the agent itself (e.g. the seeded prompt for
  `kind: agent` kits) must follow the `--` delimiter; a bare positional after
  the agent is parsed by sbx as another workspace PATH.
- `sbx run` **does** accept `--cpus <n>` (default 0 = auto: host CPUs − 1),
  contradicting the `cpu_limit` row below. awman currently still warns and
  ignores `cpu_limit`; wiring it to `--cpus` is a separate, deliberate change.

**Argv mapping — what `ResolvedSandboxOptions` fields translate to**:

The option type used by `DSbxBackend` is `ResolvedSandboxOptions` (defined in WI 0089 by `SandboxRuntime`). It is **not** `ContainerOption` — those are two distinct option families, owned by their respective runtimes. `ResolvedSandboxOptions` fields and how `DSbxBackend` handles each:

| `ResolvedSandboxOptions` field | `DSbxBackend` handling |
|---|---|
| `agent_id` | Selects the kit at `$HOME/.awman/kits/<agent_id>/`. |
| `entrypoint_override` | For `kind: agent` kits: baked into `agent.entrypoint.run` at kit emission. For `kind: mixin`: must match the built-in's entrypoint; mismatches force `kind: agent` (see Phase 0 #6). |
| `workspace_dir` | Passed as the positional workspace PATH after the agent on `sbx run` / `sbx create` (there is no `--workspace-dir` flag); the VM mounts it at the identical absolute path. Omitted when unset — sbx defaults to the invoking cwd. |
| `extra_overlays` | Workspace-rooted overlays are a no-op (workspace is already mounted). Non-workspace overlays cannot be referenced directly from inside the VM; the host side must either materialize allowed file contents into a workspace-owned staging path under `.awman/` before launch and reference that staged path in `session.json`, or reject the overlay with a clear error. Do not serialize arbitrary outside-workspace file contents into `session.json`. |
| `env_passthrough` | Vars on the launching agent's auto-auth allowlist (see Phase 4) are read from the host environment at launch and registered via `sbx secret set`; non-sensitive config vars may be written to `session.json` and applied inside the VM by the startup script. Credential-class vars outside the allowlist are warned and dropped rather than written to the workspace. |
| `env_literal` | Non-sensitive literals may be written to `session.json` for the startup script to export. Credential-class literals are warned and withheld (the warning points at the `env(VAR)` overlay route), never written to the workspace. |
| `seeded_prompt` | Written to `session.json`; startup script appends it as a positional arg to the agent's launch (for `kind: agent`) or invokes the agent via `sbx exec` with the prompt (for `kind: mixin`). |
| `interactive` | Determines whether to use `sbx run` (attach) or `sbx create` (background). |
| `sandbox_name` | Used as `--name <name>`. Always populated by `DSbxBackend` via `generate_sandbox_name(worktree_hash, agent)` if the caller did not pre-set it. |
| `memory_gb` | `--memory <n>g` on `sbx run`. |
| `cpu_limit` | Not supported by sbx; emit a warning and continue. |
| `agent_settings` | Written to `session.json` for the startup script to apply inside the VM. Replaces the Docker-runtime pattern of mounting `~/.<agent>/` directly. |
| `agent_credentials` | Routed through `dsbx::auth::inject_credentials()` and/or `environment.proxyManaged` based on agent kit config. |
| `system_prompt_file` | Path written to `session.json`; works as-is because workspace paths are identical inside VM. |
| `system_prompt_env_file` | Same — env var name + path written to `session.json`; startup script exports. |
| `system_prompt_inline` | Written to `session.json`; startup script applies. |
| `disallowed_tools` / `allowed_tools` | Written to `session.json`; startup script renders into agent config or argv. |
| `model` | Written to `session.json`; startup script renders. |
| `keep_after_exit` | `sbx stop` instead of `sbx rm` after session ends. With persistence enabled, this is the default — `sbx rm` only happens on explicit teardown. |

The exact shape of `ResolvedSandboxOptions` is defined in WI 0089. Fields above describe the **intent** WI 0090 needs to satisfy; the field names should match whatever WI 0089 landed. If a field name in WI 0089 differs from this table, follow WI 0089's name and adjust the work item history note accordingly.

Note the absence of an `image` field: sandboxes do not take an image ref at run time — the image is the kit's `agent.image`, set at kit emission. Compare against `ResolvedContainerOptions` (used by `DockerBackend` and `AppleBackend`) which does carry an `Image` field. This is the kind of paradigm-level divergence WI 0089 introduced the split to surface honestly.

**`SandboxBackend::list_running()` / `list_running_all()`**: shell out to `sbx ls --json` (verified in Phase 0). Filter by `awman-` name prefix. Return `AgentHandle` (the unified handle type defined by `AgentRuntimeEngine` in WI 0089) with the sandbox name as the ID and a `runtime_kind: Sandbox` tag so `AgentRuntimeEngine` callers can correctly identify the source runtime.

**`SandboxBackend::stats()`**: returns `AgentStats` with zeroed CPU/memory and a running/stopped status from `sbx ls`. The unified `AgentStats` type from WI 0089 reuses the same shape `ContainerStats` had, but is now a sibling under `AgentRuntimeEngine` rather than a Container-only type. Do not add `bollard`.

**`SandboxBackend::stop()`**: `sbx stop <name>` (pauses VM, preserves state). Distinct from `SandboxBackend::remove()` (a new method on `SandboxBackend` that doesn't exist on `ContainerBackend`), which calls `sbx rm <name>` (deletes the VM and persistent volume). Awman's lifecycle uses `stop` for "session ended, may resume" and `remove` for "worktree destroyed."

**`SandboxBackend::exec_args()`**: argv for `sbx exec -it <name> <entrypoint…>` with `COLUMNS` and `LINES` env injected per Phase 0 #3. Returned as `Vec<String>` so the caller (`SandboxRuntime`) can spawn the binary identified by `cli_binary()`.

**Image-introspection methods**: `SandboxBackend` does not expose `image_home_dir()` or `image_exists()` — these are container-paradigm concerns that don't exist for sandboxes. `SandboxRuntime` does not declare them in its trait surface. If Layer 2 needs sandbox metadata such as an in-VM home directory, that metadata must be exposed through the WI 0089 `AgentRuntimeEngine` surface or a Layer 0 data table, not by reaching into `DSbxKitEmitter` or any `pub(super)` sandbox internals.

**Background ops on `SandboxBackend`**: the `SandboxBackend` trait (defined in WI 0089) declares background operations directly in sandbox terms — there are no `default_*` Docker-shaped fallbacks because sandboxes don't share Docker's argv shape. The methods `DSbxBackend` implements:
- `start_background(workspace, agent_id, kit_dir, env, overlays) -> Result<SandboxId, EngineError>` → `sbx create --kit <kit-dir> --name <generated> <agent> <workdir>`.
- `exec_in_sandbox(sandbox_id, command, working_dir, env) -> Result<ExecOutput, EngineError>` → `sbx exec <sandbox_id> <command>`.
- `exec_in_sandbox_streaming(sandbox_id, command, ..., on_line)` → same with streaming bridge.
- `stop_and_remove(sandbox_id) -> Result<(), EngineError>` → `sbx stop <sandbox_id>` followed by `sbx rm <sandbox_id>`.

### Phase 4 — Credential injection

Module: `src/engine/sandbox/dsbx/auth.rs`, `pub(super)`. Contains:
- The awman-credential → sbx-service-name mapping table.
- The per-agent **auto-auth allowlist** (`supported_auth_env_vars`) for launch-time `env(VAR)` overlay auth (see below).
- `inject_credentials(creds, sandbox, sink)` that calls sandbox-scoped `sbx secret set <sandbox> <service>` with each value piped via **stdin** (never argv) to avoid process-listing leakage.
- `auto_auth_env_overlays(...)` implementing the launch-time `env(VAR)` overlay flow.
- Credential-not-found warning logic that mirrors today's behavior for Docker (warn at launch, don't silently fail).
- Sink reporting per the "Subprocess transparency" section: each `sbx secret set` invocation is announced on the user message sink with the service name only (never the value), and its stdout/stderr is masked before publication.

Service mapping (matches the Docker Sandboxes credential-services docs, June 2026):

| awman credential | sbx service name |
|---|---|
| `ANTHROPIC_API_KEY` | `anthropic` |
| `OPENAI_API_KEY` | `openai` |
| `GH_TOKEN` / `GITHUB_TOKEN` | `github` |
| `GEMINI_API_KEY` / `GOOGLE_API_KEY` | `google` |
| `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` | `aws` |
| `GROQ_API_KEY` | `groq` |
| `MISTRAL_API_KEY` | `mistral` |

**Launch-time `env(VAR)` overlay auto-auth** (added 2026-06-11): the supported
way to authenticate a mixin-kit agent. At agent-launch time (`run_interactive`,
NOT `awman ready`, so rotated keys apply per launch), each `env_passthrough`
var on the launching agent's allowlist is read from the host environment and
registered via `sbx secret set` (stdin-piped, masked). Per-agent allowlist —
mixin kits only; agent-kit agents (antigravity, crush, maki, cline) are
deliberately excluded for now:

| Agent | Allowlisted env vars | sbx service |
|---|---|---|
| `claude` | `ANTHROPIC_API_KEY` | `anthropic` |
| `codex` | `OPENAI_API_KEY` | `openai` |
| `gemini` | `GEMINI_API_KEY`, `GOOGLE_API_KEY` | `google` |
| `copilot` | `GH_TOKEN`, `GITHUB_TOKEN` | `github` |
| `opencode` | `ANTHROPIC_API_KEY` | `anthropic` |

Behavioral rules:
- Credential-class `env(VAR)` overlays outside the agent's allowlist: warn and drop (already excluded from `session.json`).
- Allowlisted var unset in the host environment: warn, no registration.
- Mixin agent launched with no allowlisted overlay registering a credential: warn that auth must be set up manually (`sbx secret set <sandbox> <service>`) or via in-sandbox login, then continue the launch.
- A failed `sbx secret set` subprocess remains launch-blocking; when the sandbox was freshly created, the error notes that it is left in place and reused on the next launch.
- **Sandbox-scoped only, never global** (revised 2026-06-11): per the sbx docs, `-g` secrets only take effect at sandbox *creation* while sandbox-scoped secrets apply immediately (running or stopped). awman therefore creates the sandbox first (`sbx create` on first launch) and registers every secret sandbox-scoped — `-g` is never used. Benefits: rotated keys always apply, awman's keys never leak into non-awman sandboxes, and `sbx rm` removes the secret scope with the sandbox. `awman ready` registers no secrets at all (the keychain OAuth token it used to forward is unusable with sbx anyway).
- **`--password-stdin` doc inconsistency**: the sbx docs disagree on whether non-interactive `sbx secret set` requires `--password-stdin`. Plain stdin piping is confirmed working and is the primary form; on a failure that looks like an interactive-prompt error, retry once with `--password-stdin` appended.

Unmapped credentials are not silent failures: emit a warning naming the variable and suggest kit-level `environment.proxyManaged` or a future explicit custom-secret mapping. Credential-class values must not be written to `session.json`; if an unmapped variable is truly non-sensitive configuration, handle it through the normal `env_passthrough` / `env_literal` path. When classification is ambiguous, prefer not passing the value and surface the warning.

**Integration point**: called from `DSbxSandboxInstance::run_with_frontend()` before spawning the sbx subprocess — the same pattern Docker/Apple backends use for side-effecting work. Not from `DSbxBackend::build()`.

### Phase 5 — Session config writer

Module: `src/engine/sandbox/dsbx/session_config.rs`, `pub(super)`. Provides `DSbxSessionConfig::write_for(&options: &ResolvedSandboxOptions, workspace: &Path)`. Writes `<workspace>/.awman/session.json` with the structured shape consumed by `apply-session-config.sh`:

```json
{
  "schema_version": 1,
  "agent": "claude",
  "yolo": true,
  "auto": false,
  "plan": false,
  "model": "sonnet-4.6",
  "seeded_prompt": "…",
  "system_prompt_inline": null,
  "system_prompt_file": "/workspace/.awman/system-prompt.md",
  "system_prompt_env_file": null,
  "disallowed_tools": ["WebFetch"],
  "allowed_tools": [],
  "env_passthrough": { "SOME_NON_CREDENTIAL_VAR": "value" },
  "agent_settings": { "claude": { "version": 1, "settings": {…} } }
}
```

The schema is internal to awman. The startup script and the writer are versioned together via `schema_version`; mismatches at startup must fail loudly with a clear error directing the user to re-run `awman ready`.

The writer **must not** include any credential values (those go through `sbx_auth`). It must include any setting that the startup script needs to render in-VM agent config.

### Phase 6 — Naming, persistence, and lifecycle

**Sandbox naming**: `awman-<worktree-hash>-<agent>` where `<worktree-hash>` is derived from the worktree's absolute path (the same hash used by `generate_container_name()` today, but augmented with the agent name so multi-agent workflows can run concurrent sandboxes against the same worktree). Naming is **deterministic per (worktree, agent)** so subsequent invocations find and restart the existing sandbox.

**`generate_container_name()`** — do not extend or modify it. Sandbox callers use the WI 0089 helper `generate_sandbox_name(worktree_hash: &str, agent: &str) -> String` from `src/engine/sandbox/naming.rs`; existing container callers keep the current behavior.

**Lifecycle:**
- First `awman chat <agent>` against a worktree → `sbx create --kit … --name awman-<hash>-<agent> <agent> <wt>`, sandbox-scoped `sbx secret set` calls, then `sbx run awman-<hash>-<agent>`. Install runs (one-time per worktree+agent), startup runs, agent attaches.
- Agent exits → sandbox transitions to stopped. State preserved via `persistence: persistent` in the kit.
- Second `awman chat <agent>` → `sbx run awman-<hash>-<agent>` (positional name, no `--name`/`--kit`/agent for restarts). Startup re-runs against the latest `session.json`; agent re-attaches.
- `awman destroy <worktree>` or equivalent teardown → `sbx rm awman-<hash>-<agent>` for each agent that has a sandbox against this worktree. Also clears the persistent volume.
- `awman ready --no-cache` → `sbx rm` all awman sandboxes that use the affected agent kit, then re-emit the kit. Next launch re-runs install.

**Naming collisions**: sbx exposes no ownership metadata (no labels; the `sbx ls --json` schema is unverified), so a true "was this created by awman?" check is unimplementable today. The exact deterministic name `awman-<hash>-<agent>` is therefore the de-facto ownership marker: an existing sandbox with that exact name is treated as awman's and restarted (history note: developer accepted this at review). Awman never overwrites or removes a sandbox whose name it did not generate; if sbx later grows ownership metadata, revisit and add the explicit non-awman error.

**Workflow concurrency**: a multi-agent workflow on one worktree creates one sandbox per agent (deterministic, distinct names). Concurrent workflows on different worktrees use different worktree hashes so they don't collide.

### Phase 7 — `awman ready` integration

Update `src/engine/ready/` for the sbx path:

1. Check `sbx` binary in PATH; helpful install hint if missing (`brew install docker/tap/sbx`).
2. Check `sbx login` status via `sbx ls`. Helpful hint if not logged in (`Run sbx login to authenticate Docker Sandboxes`).
3. Emit the per-agent kit at `$HOME/.awman/kits/<agent>/` for every configured agent (via `DSbxKitEmitter`).
4. Copy bundled `apply-session-config.sh` into each kit's `files/home/.awman/`.
5. Validate the kit by invoking `sbx kit validate <kit-dir>` for each emitted kit (if available — confirm in Phase 0). Surface validation errors with the kit path.
6. Report per-phase status messages. Every `sbx` invocation made by the ready flow (`sbx ls`, `sbx kit validate`, any `sbx rm` triggered by `--no-cache`) follows the "Subprocess transparency" section: the command is announced on the user message sink and its stdout/stderr is published there.

`awman ready` registers **no credentials** (revised 2026-06-11): every `sbx secret set` is sandbox-scoped and runs at agent-launch time (Phase 3 step 3), and the keychain-resolved OAuth token ready used to forward is unusable with sbx anyway.

No registry push step. No `docker build` step for sbx. `awman ready` for sbx is text-file emission plus subprocess validation.

The `apple-containers` and `docker` backends keep their existing `awman ready` flow unchanged. The trait-based frontend delegation pattern in `src/engine/ready/` adds sbx-specific phases without branching the existing phases.

### Phase 8 — Documentation

Update `docs/` to reflect docker-sbx-experimental as a supported (experimental) runtime. No work-item-specific docs. Update whichever existing doc covers runtimes; if none does, create `docs/XX-runtimes.md` as a user guide covering all three runtimes.

User docs must cover:
- Platform support matrix.
- How to switch runtime to `docker-sbx-experimental`.
- Where kits live and what they contain.
- The persistent-sandbox lifecycle and how to clean up.
- Credential setup (`sbx login`; launch-time `env(VAR)` overlay auto-auth, with manual sandbox-scoped `sbx secret set` as the fallback).
- Known limitations: Linux blocked, Intel Mac blocked, no per-resource stats, raw TCP/UDP blocked.

---

## Edge Case Considerations

### Platform and architecture guards

- **Linux**: blocked at `AgentRuntimeEngine::detect` with a descriptive error until Issue #51 is fixed upstream.
- **Intel Mac**: blocked on x86_64 macOS with: "Docker Sandboxes requires Apple Silicon (arm64). Intel Macs are not supported."
- **`sbx` binary not in PATH**: `is_available()` returns `false`. Surface a helpful install hint (`brew install docker/tap/sbx`).
- **`sbx` not logged in**: `sbx ls` fails with an auth error. Detect and surface: "Run `sbx login` to authenticate Docker Sandboxes."

### Kits and installation

- **Stale kit on disk**: if `awman ready` was last run against an older awman version, the kit on disk may use an outdated startup script schema. The startup script's `schema_version` check catches this — fail loudly and instruct the user to re-run `awman ready`.
- **Kit validation failure**: `sbx kit validate` errors are surfaced verbatim with the kit path. Don't try to auto-correct; fail loudly.
- **Install command failures**: a non-zero exit from any `commands.install` step fails sandbox creation. Awman wraps the failure in `EngineError::Sandbox` with the kit name and step description. The user can re-run with `awman ready --no-cache` to force a clean kit re-emit + sandbox `sbx rm` to clear the half-installed state.
- **Install command network failures**: ephemeral failures (npm, curl) are not retried by sbx. Per-agent install scripts in `commands.install` should include retry loops where appropriate (cf. Docker's Pi kit, which does `i < 5` retries on npm install).
- **Kit drift after upstream template update**: if Docker updates `docker/sandbox-templates:claude-code-docker` (e.g., new entrypoint default), existing sandboxes keep the old template; only newly-created ones pick up the new version. Document this; `awman ready --refresh-templates` or `sbx rm` is the user-facing fix.

### Credential and overlay passthrough

- **Auto-auth failure — credential not found on host**: warn at launch time with the same message style as Docker/Apple Containers. Never silently launch a sandbox that will fail due to a missing key.
- **`sbx secret set` subprocess error**: treat as launch-blocking. Surface a clear error message and the exact `sbx secret set` command the user can run manually to diagnose.
- **Credential rotation**: because `inject_credentials()` runs at every launch, rotated keys are picked up automatically.
- **`AgentSettingsPassthrough` with a host path outside the workspace**: handled via the `session.json` channel. The startup script renders the equivalent in-VM config from the serialized settings. Do not error or warn for this case under sbx — the channel is functional, just different from the Docker mount approach.
- **`EnvPassthrough` for non-service env vars**: non-sensitive config-class values may be written to `session.json`, and the startup script `export`s them inside the VM. Document which env vars are credential-class (and thus go through `sbx secret set` or proxy management) vs config-class (and can go through `session.json`).
- **`EnvLiteral` with sensitive values**: do not write credential-class literals to `session.json`, because it is workspace-readable. Route recognized secrets through `sbx secret set` / proxy management; reject or warn on unrecognized sensitive literals rather than silently downgrading them to workspace config. Non-sensitive literals may still use `session.json`.

### Networking and `--allow-docker`

- **`AllowDocker` option**: no-op under sbx (private DinD is always on). Emit a debug trace; do not warn.
- **Network proxy and HTTP clients**: only HTTP/HTTPS egress through the proxy is available by default. Raw TCP/UDP (SSH git, databases) won't work unless explicitly allowed in the kit's `network.allowedDomains`. `awman ready` for sbx should print a clear note about this.
- **`network.allowedDomains` per agent**: the emitter must populate this with the registries / APIs the agent needs (Anthropic API for claude, npm registry for pi-style agents, etc.). Insufficient `allowedDomains` causes silent network failures inside the VM.

### PTY and interactive sessions

- **Terminal size not propagated (Issue #63)**: workaround is to pass `COLUMNS` and `LINES` via `sbx exec --env`. Verified in Phase 0.
- **Re-attach to existing sandbox**: `sbx exec -it <name> bash` works; the TUI re-attach uses `exec_args()`.
- **Seeded prompt delivery**: written to `session.json`, applied by startup. For `kind: agent` kits, the entrypoint can be overridden to accept the prompt; for `kind: mixin`, the startup script must invoke the agent with the prompt as an argv tail.

### Lifecycle

- **Sandbox naming collisions**: deterministic naming; an exact `awman-<hash>-<agent>` name match is treated as awman-owned (sbx provides no ownership metadata to check). See Phase 6.
- **Stopped vs. removed sandboxes**: `sbx stop` preserves state across awman invocations; `sbx rm` is reserved for worktree destruction or explicit user cleanup.
- **`sbx reset` danger**: awman never invokes `sbx reset`; this command nukes all sandboxes and the image cache.
- **Port mappings lost on stop**: documented limitation; port-based workflows must keep the sandbox running.

### Multi-agent workflows

- **Concurrent sbx sandboxes per worktree**: one per agent; the deterministic naming ensures no collisions. First-launch cost per agent is paid once per worktree.
- **Sandbox naming per step**: workflow steps within the same agent share one sandbox (multi-step interactions via `sbx exec` re-attach). Steps in different agents get distinct sandboxes by the naming rule above.

---

## Test Considerations

### Unit tests

- **Kit emission, mixin path**: golden-file tests that emitting a kit for `claude`, `codex`, `gemini`, `copilot`, `opencode` produces the expected `spec.yaml` shape (kind: mixin, no agent block, correct network/credentials).
- **Kit emission, agent path**: golden-file tests for `antigravity`, `crush`, `maki`, `cline` — `kind: agent`, correct base image, install commands present.
- **Per-agent `apply-session-config.sh` copy**: verify the emitted kit dir contains the script at `files/home/.awman/apply-session-config.sh` with executable mode.
- **`DSbxSessionConfig::write_for()` round-trip**: serialize `ResolvedSandboxOptions` to JSON and back; verify all option variants are represented and credential values are excluded.
- **Argv construction (`DSbxSandboxInstance::run_with_frontend()`)**: for each `ResolvedSandboxOptions` shape (first launch vs restart), verify the generated `sbx run` argv is correct. No live sbx binary needed.
- **Unsupported option handling**: `Cpu` produces a warning, `AllowDocker` produces a debug trace; neither errors.
- **Platform guard in `AgentRuntimeEngine::detect`**: `"docker-sbx-experimental"` returns `BackendUnsupportedOnPlatform` on Linux and x86_64 macOS.
- **Auto-auth service mapping**: every credential in the mapping table has expected behavior; an unmapped credential produces a warning (not an error) and is not written to `session.json` unless it has been explicitly classified as non-sensitive configuration.
- **Auto-auth credential not found**: clear warning, not a panic, when a required key is absent from keychain and environment.
- **Auto-auth stdin pipe — no argv leakage**: verify `sbx secret set` is invoked with the credential piped via stdin, never argv or env.
- **Naming determinism**: `generate_sandbox_name(worktree_hash, agent)` produces the same name for the same inputs across invocations.
- **Subprocess announcement**: every code path that spawns an `sbx` subprocess emits an Info-level sink message containing the full argv before the spawn (use a mock sink and a stubbed spawn helper; no live sbx binary needed).
- **Subprocess output publication**: stdout lines from a (stubbed) `sbx` invocation arrive on the sink at Info level and stderr lines at Warning level; on non-zero exit the failure message includes the argv and captured stderr.
- **Secret-value redaction on the sink**: the announcement for `sbx secret set` names the service but never contains the credential value, and a stubbed `sbx secret set` that echoes its stdin back does not leak the value to the sink (masking applies).

### Integration tests

- Gate all sbx integration tests behind `#[cfg(target_os = "macos")]` + `#[cfg(target_arch = "aarch64")]` and an env var guard (`AWMAN_TEST_SBX=1`).
- **First-launch lifecycle**: emit a minimal kit, `sbx run`, verify install runs, sandbox reaches running state, `sbx stop`, verify stopped, `sbx run <name>` (positional restart) verifies install does NOT re-run.
- **`is_available()` paths**: sbx installed + logged in vs. not installed.
- **`sbx kit validate`** on emitted kits for each agent.

### End-to-end tests

- `awman ready` with `runtime: "docker-sbx-experimental"` completes without error on macOS arm64 (env-gated).
- `awman exec` with a simple non-interactive prompt runs end-to-end through the sbx backend.
- Multi-agent workflow with two sequential sbx steps completes and produces expected artifacts; verify one sandbox per agent is spawned and reused on re-invocation.

### Regression tests

- All existing `docker` and `apple-containers` backend tests continue to pass **without modification** to any existing assertion. Any test edit that "makes room" for sbx is a sign the sbx work is wrong.
- All existing template tests (Dockerfile content, `dockerfile_matches_template`, etc.) continue to pass unchanged. The `templates/Dockerfile.*` files keep their byte-identical contents.
- `AgentRuntimeEngine::detect` with `runtime: "blarg"` (unknown) still falls back to Docker with a warning.
- **Runtime switching test** (host-side, no live sbx needed): in a single process, mutate `GlobalConfig::runtime` between `Some("docker")`, `Some("docker-sbx-experimental")`, and back. After each mutation, `AgentRuntimeEngine::detect()` returns a runtime whose `runtime_name()` matches the config. No persistent state leaks between detections.
- **Default-runtime test**: `GlobalConfig::default()` with no `runtime` set still resolves to Docker via `detect()`, exactly as today.

---

## Codebase Integration

All sbx work lives under `src/engine/sandbox/dsbx/`. The `src/engine/container/` subtree, the existing `ContainerRuntime`, the `ContainerBackend` trait, and every Docker/Apple file are untouched in WI 0090 — that's the whole point of the WI 0089 split. The structural pieces WI 0089 created (`AgentRuntimeEngine` trait, `SandboxRuntime`, `SandboxBackend` trait, the `DSbxBackend` stub) get filled in here.

- **`src/engine/sandbox/dsbx/backend.rs`**: replace the WI 0089 stub. `pub(super) struct DSbxBackend` implementing the `SandboxBackend` trait declared in `src/engine/sandbox/backend.rs`. `DSbxSandboxInstance` implementing the `SandboxInstance` trait. No type `pub` beyond `pub(super)`.
- **`src/engine/sandbox/dsbx/kit.rs`**: new file, `pub(super)`. `DSbxKitEmitter` and supporting structs. Generates kit `spec.yaml` and copies `apply-session-config.sh`.
- **`src/engine/sandbox/dsbx/auth.rs`**: new file, `pub(super)`. Credential mapping table + `inject_credentials()`. Called from `DSbxSandboxInstance::run_with_frontend()`, never from `build()`.
- **`src/engine/sandbox/dsbx/session_config.rs`**: new file, `pub(super)`. Serializes `ResolvedSandboxOptions` to `<workspace>/.awman/session.json` (no credentials).
- **`src/engine/sandbox/dsbx/mod.rs`**: adds the four submodules above, all `pub(super)`. The DSbx subtree is invisible outside `src/engine/sandbox/`.
- **`src/engine/sandbox/mod.rs`**: no API surface changes beyond what WI 0089 already declared. The `SandboxRuntime` struct that WI 0089 introduced uses the now-functional `DSbxBackend` instead of the stubbed one — but that wiring change happens by virtue of `DSbxBackend` now returning real values rather than `NotImplemented`. No signature changes here.
- **`src/engine/agent_runtime/`** (the facade module WI 0089 introduced): zero changes. The trait was finalized in WI 0089. If something in this module needs to change, that's a signal WI 0089 was incomplete — go back and fix it there.
- **`src/engine/container/`**: zero changes. Verify with `git diff src/engine/container/` showing no modifications.
- **`src/data/config/global.rs`**: no new fields.
- **`src/data/fs/`**: add a path helper for `$HOME/.awman/kits/<agent>/` resolution. Layer 0 owns filesystem-path concerns. Existing `RepoDockerfilePaths` in `src/data/repo_dockerfile_paths.rs` is untouched — sbx kits live at a host-global path, not a per-repo path.
- **`src/data/templates/mod.rs`**: add two new accessor functions, `sbx_kit_template_for(agent)` and `sbx_apply_script_for(agent)`, that read the embedded sbx kit YAML and apply-script templates via `include_str!` (analogous to the existing `agent_dockerfile_for(agent)`). The existing template accessor functions and `dockerfile_matches_template()` are not modified — they keep returning Dockerfile contents for Docker/Apple paths.
- **`templates/`**: add new files `templates/sbx-kit.<agent>.yaml` (one per supported agent) and `templates/sbx-apply.<agent>.sh` (one per agent). These sit beside the existing `Dockerfile.<agent>` files. **No existing `templates/Dockerfile.*` file is modified.**
- **`src/engine/agent/`**: the agent matrix gains a per-agent flag indicating which sbx kit kind to use (`mixin` for the five built-ins, `agent` for the other four). No existing agent matrix data is removed. The flag is consulted only by the DSbx kit emitter; Docker and Apple paths ignore it.
- **`src/engine/ready/`**: add sbx-specific ready phases via the existing trait-based frontend delegation. Phases: check `sbx` binary, check login, emit kits, copy startup scripts, inject credentials, validate kits. The Docker/Apple ready phases are untouched. The ready engine selects which phase set to run by querying `AgentRuntimeEngine::capabilities()` (the trait method WI 0089 added) — no string-matching on runtime name.
- **`src/engine/container/naming.rs`**: untouched. Sandbox naming lives at `src/engine/sandbox/naming.rs` (added by WI 0089) and provides `generate_sandbox_name(worktree_hash, agent)`. Container naming continues to use the existing `generate_container_name()`.
- **Layer discipline**: `DSbxBackend` and its companions live in Layer 1 (engine), inside `src/engine/sandbox/dsbx/`. They must not import from Layer 2 or Layer 3, and must not import from `src/engine/container/` (containers and sandboxes are sibling runtime tiers — no cross-tier imports). Subprocess invocations use `std::process::Command`. No new async dependencies. No `bollard`.
- **Higher-layer interaction**: Layer 2 callers (the Command tier) interact with sandboxes exclusively through the `AgentRuntimeEngine` trait (defined by WI 0089). Layer 2 never sees `SandboxRuntime`, `SandboxBackend`, or `DSbxBackend` as concrete types. This guarantee is enforced by the `pub(super)` visibility on `SandboxBackend` and below.
- **Frontend trait delegation**: when `DSbxSandboxInstance::run_with_frontend()` needs PTY size, stdin, or status reporting, it accepts the frontend trait declared by WI 0089's `SandboxRuntime` (analogous to today's `ContainerFrontend`). The trait is provided by Layer 2 (`ChatCommand`, etc.), implemented by Layer 3 (TUI / CLI / API). This preserves the grand architecture's Tenet 1 — lower layers never call into higher layers directly.
- **Error messages**: all sbx-specific errors are user-facing and actionable. Avoid leaking raw `sbx` CLI error output — wrap in `EngineError::Sandbox(...)` (a new variant added by WI 0089, analogous to the existing `EngineError::Container(...)`) with context. Follow the existing wrapping pattern.
- **Read the grand architecture doc**: re-read `aspec/architecture/2026-grand-architecture.md` plus WI 0089 in full before implementing. Pay attention to: factory pattern (engine returns trait objects, not concrete types), `AgentRuntimeEngine` trait surface, the disjoint `ContainerBackend` / `SandboxBackend` traits, and the principle that runtime-tier and backend-tier types are invisible outside their module subtree.

### Non-interference checklist (must hold at PR-ready)

Before opening the implementation PR, the implementing agent must self-verify each of the following:

**Semantic invariants**:

- [ ] `git diff src/engine/container/` shows zero modifications.
- [ ] `git diff src/engine/agent_runtime/` shows zero modifications (WI 0089's facade is final).
- [ ] `git diff` shows zero modifications to any file under `templates/Dockerfile.*`.
- [ ] `git diff` shows zero modifications to existing functions in `src/data/templates/mod.rs` — only new functions added.
- [ ] `git diff` shows zero modifications to any test in `src/engine/container/*.rs`. (Renames that flowed through WI 0089 are already on the post-WI-0089 baseline — WI 0090 adds no further changes here.)
- [ ] `ContainerRuntime::build_image()` signature is byte-identical to the post-WI-0089 form.
- [ ] `ContainerBackend` trait gains no new required methods.
- [ ] `ContainerOption` enum has no new variants and no removed variants.
- [ ] `GlobalConfig` struct has no new fields and no removed fields beyond what WI 0089 already added.
- [ ] `make test` passes on the local machine after the change (modulo the new sbx-gated tests).
- [ ] Running `awman ready` with `runtime: "docker"` produces byte-identical `.awman/Dockerfile.*` contents to a pre-change checkout.
- [ ] Running `awman ready` with no `runtime` set in config produces byte-identical behavior to today (defaults to Docker).
- [ ] `awman exec --runtime docker-sbx-experimental` no longer returns `EngineError::NotImplemented` — it actually runs (replacing the WI 0089 stub behavior). **Deferred to WI 0091** (developer decision at review): WI 0090's change-set scopes Layer 2 out, so `awman ready` works end-to-end under sbx while `awman chat`/`exec` routing — and mixin seeded-prompt consumption in the apply scripts — land in `0091-sbx-layer2-routing.md`. Until then those commands return a clear "this command does not yet route to the sandbox runtime" error.

**No-alias invariants** (carried forward from WI 0089's standard):

- [ ] WI 0090 introduces no `pub use OldName as NewName;` aliases, no `#[deprecated]` annotations, and no methods suffixed `_legacy` or `_compat`.
- [ ] `DSbxBackend` does not accept any container-paradigm types as inputs "for transitional ergonomics." Its inputs are the sandbox-paradigm types declared by WI 0089 (`ResolvedSandboxOptions`, `AgentHandle`, etc.) and nothing else.
- [ ] Nothing in `src/engine/sandbox/` imports from `src/engine/container/`. Containers and sandboxes are sibling runtime tiers — no cross-tier coupling, even helper-shaped.

---

## Documentation

After implementation, update user-facing documentation in `docs/`:

- Update the existing runtimes doc (whichever covers `docker` and `apple-containers`) to add `docker-sbx-experimental` alongside, including platform support, the kit-based model, the persistent-sandbox lifecycle, and credential setup.
- Create `docs/XX-runtimes.md` only if no existing doc covers runtime selection.
- Never create work-item-specific docs.
- Implementation details, decisions, and technical notes belong in this work item or code comments — not in `docs/`.
- User docs cover the limitation matrix (what works, what doesn't, on which platforms) in plain language.

See `CLAUDE.md` for additional documentation standards.
