# Work Item: Feature

Title: ACP support
Issue: issuelink

## Summary:
- Allow any configured agent to be launched over **ACP (Agent Client Protocol)** instead of raw container stdio, with awman rendering a custom, portable, built-in UX to drive the agent's session (messages, tool calls, plans, diffs, permission prompts) rather than displaying the agent's raw terminal output.

This is a divergence from how agents are launched today (direct container stdio/PTY passthrough). It is configured per repo via `.awman/config.json`'s `launchMode` field: `"acp"` opts an agent session into ACP; `"stdio"` (the default when unset) keeps today's behavior unchanged. It can also be overridden per-invocation with `--launch-mode <stdio|acp>` on any agent-launching command, following the CLI flag → env → repo config → global config → default precedence documented in `aspec/uxui/cli.md`.

Agents still run inside the exact same containers/images as today — ACP does not relax containerization, add port exposure, or change mount-scope rules (`aspec/architecture/security.md`). The only difference is *how* awman talks to the agent process's stdio: a newline-delimited JSON-RPC 2.0 channel (ACP) instead of a raw PTY. The agent library (`AgentMatrix`) gains a `supports_acp` flag; launching an agent that doesn't support ACP with `launch_mode: acp` (config or `--launch-mode` flag) is a hard error. A new global config field, `launchModeFallback` (`"stdio"` | `"error"`, default `"error"`), governs what happens when a repo has `launchMode: acp` set but a specific launch (a `chat`/`exec prompt` invocation, or one step of an `exec workflow`) resolves to an agent that doesn't support ACP: `"error"` aborts the launch (workflows are pre-flight validated so this is caught before any container starts); `"stdio"` downgrades that one launch to ordinary stdio mode, with a visible warning so the UX shift isn't mistaken for a bug.

The UX must be portable between frontends: in the CLI it renders as a line-oriented interactive stdio experience (structured updates printed to the terminal, permission prompts read from stdin); in the TUI it renders inside a new **agent window**, structurally parallel to today's container stdio window but outlined **purple** instead of green, showing awman's rendered ACP UX instead of the raw PTY buffer.

No new Dockerfile templates are introduced by this work item — ACP mode launches the same agent binary already installed in the agent's image, with a different entrypoint flag (e.g. `cline --acp`, per `aspec/work-items/completed/0062-copilot-crush-and-cline-agents.md`), so one image continues to serve both `stdio` and `acp` launch modes for a given agent.

## User Stories

### User Story 1:
As a: user who wants richer, structured feedback from an agent (tool calls, plans, diffs, permission prompts) instead of a raw terminal stream

I want to: set `launchMode: "acp"` for an agent that supports it (or pass `--launch-mode acp` for one run)

So I can: drive that agent's session through awman's own rendered UI — in both the CLI and the TUI — instead of parsing whatever the agent chooses to print to its PTY.

### User Story 2:
As a: developer maintaining awman

I want to: have the agent library declare `supports_acp` per agent and have `AgentEngine::build_options` reject `launch_mode: acp` for any agent that doesn't support it

So I can: fail fast with a clear, actionable error before a container ever launches, instead of shipping a hang or garbled JSON-RPC framing against an agent that doesn't speak ACP.

### User Story 3:
As a: user running a multi-agent `exec workflow` with `launchMode: "acp"` set at the repo level

I want to: control what happens when a workflow step's agent doesn't support ACP, via the global `launchModeFallback` setting (`"error"` or `"stdio"`)

So I can: either catch the mismatch immediately before any step runs (`"error"`, the default) or let that one step gracefully run in ordinary stdio mode with a visible warning (`"stdio"`), according to my own workflow's needs — rather than awman guessing or the workflow silently changing behavior with no explanation.

## Implementation Details:

### 1. Config schema (Layer 0 — `src/data/config/`)

- `src/data/config/repo.rs`: add
  ```rust
  #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
  #[serde(rename_all = "lowercase")]
  pub enum LaunchMode {
      #[default]
      Stdio,
      Acp,
  }
  ```
  and a `pub launch_mode: Option<LaunchMode>` field on `RepoConfig` (`#[serde(rename = "launchMode", skip_serializing_if = "Option::is_none")]`), following the exact pattern of `pub auth: Option<AgentAuthMode>`.
- `src/data/config/global.rs`: add
  ```rust
  #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
  #[serde(rename_all = "lowercase")]
  pub enum LaunchModeFallback {
      Stdio,
      #[default]
      Error,
  }
  ```
  and `pub launch_mode_fallback: Option<LaunchModeFallback>` on `GlobalConfig` (`#[serde(rename = "launchModeFallback", skip_serializing_if = "Option::is_none")]`).
- `src/data/config/flags.rs`: add `launch_mode: Option<LaunchMode>` to `FlagConfig`, populated from `--launch-mode <stdio|acp>`. Reject an unrecognized value with the same "unknown enum value" error shape used elsewhere in flag parsing (do not silently ignore it).
- `src/data/config/effective.rs`: add `EffectiveConfig::launch_mode(&self, repo_agent: &str) -> LaunchMode` and `EffectiveConfig::launch_mode_fallback(&self) -> LaunchModeFallback`, following the precedence chain already implemented by `runtime()`/`auth_mode()`: flag → env (`AWMAN_LAUNCH_MODE`) → repo config → (fallback only) global config → built-in default. `launch_mode()` has no global-level override — it is inherently per-repo/per-invocation, matching the WI's "set at the repo level" framing; `launch_mode_fallback()` is global-only (no repo override), matching "global config.json entry."

### 2. Agent library (Layer 1 — `src/engine/agent/agent_matrix.rs`)

Add to `AgentMatrix`:
```rust
/// Whether this agent's CLI can run in ACP (Agent Client Protocol) mode.
pub supports_acp: bool,
/// Entrypoint used when launching in ACP mode (e.g. `["cline", "--acp"]`).
/// `None` when `supports_acp` is `false`.
pub acp_entrypoint: Option<Vec<&'static str>>,
```

Update every `matrix_for()` arm. Only `cline` is confirmed today (`--acp` — run in Agent Client Protocol mode, per `aspec/work-items/completed/0062-copilot-crush-and-cline-agents.md`):
```rust
"cline" => AgentMatrix {
    // ...unchanged fields...
    supports_acp: true,
    acp_entrypoint: Some(vec!["cline", "--acp"]),
},
```
Every other agent gets `supports_acp: false, acp_entrypoint: None,` with a `// TODO(acp): verify and wire up if/when <agent> ships ACP support` comment — do not guess at unverified per-agent ACP flags. Extend the existing `matrix_supports_all_agents` test (or add a peer test) asserting every `SUPPORTED_AGENTS` entry has a matrix arm with a `supports_acp` value, and add `acp_entrypoint.is_some() == supports_acp` as an invariant check.

Add `agent_matrix::entrypoint_for_acp(matrix: &AgentMatrix) -> Result<Entrypoint, EngineError>` alongside the existing `entrypoint_for()`, returning `EngineError::AcpUnsupported` when `acp_entrypoint` is `None` — this is the low-level building block; the higher-level guard in `AgentEngine::build_options` (below) is what actually stops the launch before any container work happens, but keeping the check here too means the function can never silently produce a broken argv if called from a future call site.

### 3. `AgentEngine::build_options` guard (Layer 1 — `src/engine/agent/mod.rs`)

`AgentRunOptions` gains `pub launch_mode: LaunchMode` (default `Stdio`, mirroring how `plan`/`yolo`/`auto` are threaded through today). In `build_options`, alongside the existing plan-mode-unsupported check:
```rust
if matches!(run.launch_mode, LaunchMode::Acp) && !matrix.supports_acp {
    return Err(EngineError::AcpUnsupported {
        agent: agent.as_str().to_string(),
    });
}
```
This is the single, final guard — every launch path (direct `chat`/`exec prompt`, and each `exec workflow` step) funnels through `build_options`, so this check alone guarantees an unsupported agent can never actually reach a container with ACP framing, regardless of what Layer 2 pre-flight logic (section 6) decided. Mirror `build_options_rejects_plan_for_unsupported_agent` with a new `build_options_rejects_acp_for_unsupported_agent` test.

When `launch_mode` is `Acp`, `build_options` uses `agent_matrix::entrypoint_for_acp` instead of `entrypoint_for`, and sets the new container option from section 4 so the runtime picks the piped (not PTY) code path.

### 4. Container runtime: persistent piped stdio (Layer 1 — `src/engine/container/`)

Today `docker.rs::build_run_argv` picks `-it` (PTY-bridged) whenever `options.interactive`, or `-i` (piped, dropped after one seeded write) for a one-shot non-interactive seeded run — see `spawn_pty_bridged_docker` vs `spawn_piped_docker`. ACP needs a **third** shape: piped stdio (`-i`, no `-t`, since JSON-RPC framing must never pass through a PTY's cooked-mode/echo/ANSI layer) that stays open for the whole session, like the PTY path does, instead of being dropped after one write like today's one-shot `spawn_piped_docker`.

- `src/engine/container/options.rs`: add `ContainerOption::Acp(bool)` and a `pub acp: bool` field on `ResolvedContainerOptions` (default `false`).
- `docker.rs::build_run_argv`: when `options.interactive && options.acp`, emit `-i` (not `-it`).
- `docker.rs`: add `spawn_piped_interactive_docker`, a sibling of `spawn_piped_docker` that does **not** call `drop(bridge.stdin_injector)` — it keeps the stdin channel alive for the session's lifetime, exactly as `spawn_pty_bridged_docker` keeps its PTY master alive, so the bridge can carry the full bidirectional JSON-RPC exchange instead of one write-then-EOF.
- `apple.rs`: mirror the same `-i`-not-`-it` argv change and an equivalent persistent-piped spawn path for the Apple Containers backend, so ACP mode has parity across both container backends (Docker Sandboxes / `SandboxRuntime` are out of scope for this work item — ACP is a container-paradigm-only launch mode for now; `Capabilities` gains no new flag, and `AgentRuntimeEngine::build()` for `SandboxRuntime` simply never sees `ContainerOption::Acp` since that option lives on the container-paradigm `ResolvedContainerOptions`, not the cross-paradigm `ResolvedAgentOptions`).

No new ports, no host network mode, no new mounts — the bytes flow over the exact stdio pipes Docker/Apple Containers already wire up for `-i`. This preserves the security constraint in `aspec/architecture/security.md` verbatim: nothing new is exposed to or from the host beyond what already exists for piped/interactive containers.

### 5. ACP client (new Layer 1 module — `src/engine/acp/`)

- `mod.rs` — re-exports.
- `protocol.rs` — typed JSON-RPC 2.0 message shapes for ACP: `initialize`/`initialized`, `session/new`, `session/prompt`, `session/cancel`, the `session/update` notification and its `SessionUpdate` variants (agent message/thought chunks, tool call + tool call update, plan, available-commands update), and the client-served (agent-initiated) methods awman must answer: `session/request_permission`, `fs/read_text_file`, `fs/write_text_file`. Frame messages as newline-delimited JSON per the ACP wire format; a framer that reads/writes whole lines off the `AgentIo` byte channels used today for PTY bridging (`take_io()` from `AgentFrontend`) — ACP reuses that same handle, it just interprets the bytes as JSON-RPC lines instead of a terminal stream.
- `client.rs` — request/response correlation (pending-request map keyed by JSON-RPC id), dispatch of incoming notifications and agent-initiated requests to the `session.rs` layer.
- `session.rs` — `AcpSession`: wraps a running `AgentExecution` (from `spawn_piped_interactive_docker`) plus the JSON-RPC client, exposing `prompt(text) -> ...`, `cancel()`, `respond_permission(request_id, decision)`, and a `tokio::sync::mpsc` (or `broadcast`) stream of `SessionUpdate` events that Layer 2/3 render. Malformed/non-JSON-RPC lines are never fatal — they're routed to the message sink as a protocol warning (mirroring how `UserMessageSink` info/warning calls are used elsewhere), matching the codebase's existing decoupled-error philosophy rather than crashing the session.
- `fs/read_text_file` and `fs/write_text_file` (agent-initiated, answered by awman): serve these against the **container's own filesystem**, not the host — the agent process is already confined to the container per `aspec/architecture/security.md`, so proxying its own file requests back into that same container introduces no new host-escape surface. This is a defense-in-depth point worth flagging explicitly in code review.
- `EngineError` gains `AcpUnsupported { agent: String }` and `Acp(String)` (protocol/framing failures), following the existing `Container(String)`/`Sandbox(String)` shape.

### 6. `AcpFrontend` trait (Layer 2-defined, Layer 3-implemented)

Following the "Trait-Based Delegation" pattern in `aspec/architecture/design.md` (`WorkflowFrontend`, `InitFrontend`, `AgentFrontend`), define `AcpFrontend` (co-located with `AcpSession`, `src/engine/acp/frontend.rs`):
```rust
pub trait AcpFrontend: Send {
    fn render_update(&mut self, update: SessionUpdate);
    fn request_permission(&mut self, request: PermissionRequest) -> PermissionDecision;
    fn next_prompt(&mut self) -> Option<String>; // None => session should end
}
```
This is what makes the UX portable: `AcpSession`'s driver loop calls only this trait, and each frontend supplies its own implementation (sections 8–9). It is the ACP analogue of `AgentFrontend`, not a replacement for it — a launch either uses `AgentFrontend` (stdio/PTY path, unchanged) or drives an `AcpSession` through `AcpFrontend` (new path); the two never combine for one launch.

`--yolo`/`--auto` map onto ACP's permission model directly: when the effective run has `YoloMode::Enabled` or `AutoMode::Enabled`, the ACP driver auto-answers `session/request_permission` (approve) without calling into `AcpFrontend::request_permission`, exactly mirroring what those flags already do for stdio agents today (skip prompting, not skip the underlying safety semantics).

### 7. Layer 2 command wiring (`src/command/commands/`)

- `chat.rs`, `exec_prompt.rs`: after `resolve_agent()` succeeds, resolve `EffectiveConfig::launch_mode(agent)`. If `Acp` and `agent_matrix::matrix_for(agent)?.supports_acp` is `false`, consult `launch_mode_fallback()`: `Error` → return `EngineError::AcpUnsupported` immediately (before any overlay resolution or container launch, same "fail before touching the container runtime" point the antigravity WI's deprecation-warning placement uses as precedent); `Stdio` → downgrade `run.launch_mode` to `Stdio` for this invocation and emit a `MessageLevel::Warning` via the message sink ("agent '<agent>' does not support ACP; falling back to stdio for this session — see launchModeFallback") before the PTY is activated. Branch to `AcpSession`-driven execution vs the existing `AgentFrontend` path based on the (possibly downgraded) effective launch mode.
- `exec_workflow.rs`: **pre-flight validate every step's agent** against `supports_acp` before launching *any* step, when the workflow's effective launch mode is `Acp` — this is what makes `launchModeFallback: "error"` behave as "workflow launch throws an error" rather than failing mid-run after earlier steps already did work. Fallback `"stdio"` downgrades only the offending step(s), with one warning per step, and the workflow continues; other steps whose agents support ACP still run in ACP mode.
- `session_setup.rs` / `resolve_agent`: no change to agent resolution itself; launch-mode resolution is a separate, orthogonal concern layered on top, consistent with how `--model`/`--agent` overrides already compose today.

### 8. CLI frontend (`src/frontend/cli/`)

New `AcpFrontend` impl for `CliFrontend` (new file, `src/frontend/cli/per_command/acp_frontend.rs`, alongside the existing `container_frontend_marker.rs`): `render_update` prints each structured update as a formatted line to stdout (message chunks streamed as they arrive, tool calls/plans rendered as short labeled blocks — no raw JSON dumped to the user); `request_permission` prints the request and its options and blocks on a line read from stdin; `next_prompt` reads a line from stdin for follow-up turns (`Ctrl+D`/EOF ends the session, `Ctrl+C` cancels — same interactive-session exit conventions documented for `chat` today in `docs/03-agent-sessions.md`). This satisfies "runs as an interactive stdio experience in the CLI" without needing any TUI machinery.

### 9. TUI frontend (`src/frontend/tui/`)

- `tabs.rs`: `ContainerSlot` needs to represent either a stdio (PTY/vt100) slot or an ACP slot. Add an enum, e.g. `pub enum AgentWindowKind { Stdio, Acp(AcpSlotState) }` on `ContainerSlot` (or a sibling `AcpSlot` type alongside `ContainerSlot` in the `Tab.container_slots` collection — pick whichever keeps `focused_slot()`/minimized-bar iteration working with a single slot type, since parallel-workflow groups must be able to mix stdio and ACP slots across steps per Edge Case Considerations). `AcpSlotState` holds the rendered update history (a plain `Vec<SessionUpdate>`/ring buffer — not a vt100 buffer, since there's no raw terminal stream to parse) and any pending `PermissionRequest`.
- New `src/frontend/tui/acp_view.rs`, structurally parallel to `container_view.rs`: `render_acp_maximized` (the "agent window" equivalent of `render_container_maximized`) and `render_acp_bars` (minimized-bar equivalent). Same title/stats conventions, but `.border_style(Style::default().fg(PURPLE))` instead of `Color::Green` — introduce a `pub const ACP_BORDER_COLOR: Color = Color::Rgb(147, 51, 234);` (a true purple) rather than reusing `Color::Magenta`, since `Magenta` is already claimed by `tab_color()` for remote tabs and the yolo-countdown flash and would be visually ambiguous here.
- `tabs.rs::tab_color` / `window_border_color`: extend to consult the focused slot's `AgentWindowKind` — an ACP slot's tab/window border uses `ACP_BORDER_COLOR` in the states where a stdio slot would use `Color::Green`, and falls through to the existing `Blue`/`Gray`/`Red`/`DarkGray` states unchanged for error/idle/unfocused (only the "this is a live agent window" signal color changes, not the phase-based error/idle colors).
- New `AcpFrontend` impl in `src/frontend/tui/per_command/acp_frontend.rs`: `render_update` appends to the focused slot's update history and triggers a redraw; `request_permission` opens a modal dialog reusing the existing dialog framework in `src/frontend/tui/render/dialog.rs` (the same one used for step-complete and other confirmation dialogs) rather than inventing a second dialog system; `next_prompt` reads from the same command-box input widget already used to type `chat`/`exec` commands, routed to the session instead of to `Dispatch` while an ACP window is focused.
- Resize: PTY mode forwards terminal resize into the container (`container_resize_tx`); ACP has no PTY, so there is nothing to forward over the wire — the agent window still needs to reflow its own rendering on terminal resize, which is handled the same way `render_container_maximized` already recomputes its area from `outer_area` on every frame. No new resize plumbing to the engine.

### 10. Dockerfile / image audit

No `templates/Dockerfile.*` changes in this work item — every agent's ACP entrypoint (where `supports_acp: true`) reuses the binary already installed for stdio mode, just with a different argv (`entrypoint_for_acp` vs `entrypoint_for`). If a future agent's ACP support requires an additional package not already in its image, that is a separate, agent-specific Dockerfile work item — do not add speculative package installs here.

## Edge Case Considerations:

- **Direct request against an unsupported agent** (`awman chat --agent codex --launch-mode acp`, or repo config `agent: "codex"` + `launchMode: "acp"`) — always a hard error at the `build_options` guard (section 3), regardless of `launchModeFallback`; that global setting only governs the *workflow pre-flight* / *repo-default-vs-explicit-agent* interaction described in section 7, not an explicit, single-agent `--launch-mode acp` request, which is an unambiguous user intent that must fail loudly rather than silently downgrade.
- **`launchMode: acp` at repo level + workflow step with an unsupported agent + `launchModeFallback` unset** — defaults to `"error"`; the workflow fails pre-flight, before any step's container starts, so no partial workflow progress is lost or masked. The error names the offending step and agent.
- **`launchModeFallback: "stdio"`** — the offending step (and only that step) runs in ordinary PTY/stdio mode; a `Warning`-level message is emitted before that step's container launches so the UX shift (green window instead of purple) isn't mistaken for a rendering bug. Other steps whose agents do support ACP are unaffected.
- **`--launch-mode acp` combined with `--non-interactive`/`-n` or headless `exec prompt`** — ACP's value proposition is the rendered interactive UX; a non-interactive run still works (the CLI's `AcpFrontend::request_permission`/`next_prompt` degrade to auto-approve/no-follow-up exactly as `--yolo`/`--auto` already do for stdio agents), but should not require a human at the keyboard. No new flag is introduced for this — it composes with the existing `--yolo`/`--auto`/`-n` semantics per section 6.
- **Agent process exits or the JSON-RPC connection drops mid-session** — surfaces through the same `AgentExecution`/`AgentExitInfo` machinery every other launch already uses; the agent window transitions to the existing "done/error" visual state (still purple-bordered, matching how a stdio window keeps its identity while changing phase color), not a hang.
- **Agent writes a non-JSON-RPC line to stdout** (e.g. an accidental `println!` debug line from a misbehaving agent build) — never fatal to the session; unparsable lines are routed to the message sink as a protocol warning (section 5), and the JSON-RPC stream keeps being read.
- **`fs/read_text_file` / `fs/write_text_file` / future ACP tool-call callbacks** — must resolve strictly within the container's own filesystem; awman's ACP client-side handlers are not a new host-filesystem access path. Call out explicitly in security review (`/security-review`) once implemented.
- **Parallel-workflow groups mixing launch modes** — a `Tab`'s `container_slots` can hold N slots for a parallel workflow group; different slots may resolve different `launchMode`s if their steps configure different agents (one `supports_acp: true`, one not, under `launchModeFallback: stdio`). `focused_slot()`, minimized-bar rendering, and tab-level aggregate color must handle a mix of `AgentWindowKind::Stdio` and `::Acp` slots within the same tab without conflating their rendering paths.
- **Copy/scrollback** — the stdio window's copy-selection and vt100 scrollback (`TextSelection`, `container_scroll_offset`) do not apply to ACP slots (there is no raw terminal buffer); the agent window needs its own, simpler "scroll the rendered update list" behavior, not a shared code path with `container_view.rs`'s vt100-specific scroll logic.
- **Apple Containers backend parity** — the persistent piped-stdio spawn path (section 4) must exist for both `DockerBackend` and `AppleBackend`; shipping ACP support only under Docker while silently no-op'ing (or hanging) under Apple Containers would be a platform-inconsistent regression.
- **Sandbox-class runtimes (`docker-sbx-experimental`)** — explicitly out of scope; `ContainerOption::Acp` lives on the container-paradigm option type only. If a user is on `SandboxRuntime` and requests `launch_mode: acp`, `build_options` still runs the `supports_acp` guard first (which may pass), but the sandbox `ResolvedSandboxOptions` type has no ACP-piping equivalent yet — this should surface as `EngineError::NotImplemented` from the sandbox path per the existing WI 0089 convention, not a silent wrong-mode launch. Confirm this is the actual behavior once `ResolvedAgentOptions` variant selection is implemented; if it isn't, add the explicit guard.

## Test Considerations:

**Unit tests**
- `agent_matrix.rs`: `matrix_for("cline").supports_acp == true` and `acp_entrypoint == Some(vec!["cline", "--acp"])`; every other `SUPPORTED_AGENTS` entry has `supports_acp == false` and `acp_entrypoint == None`; `acp_entrypoint.is_some() == supports_acp` invariant across all agents (extend/mirror `matrix_supports_all_agents`).
- `entrypoint_for_acp`: returns `Err(EngineError::AcpUnsupported)` for an agent with `acp_entrypoint: None`; returns the expected `Entrypoint` for `cline`.
- `AgentEngine::build_options`: new `build_options_rejects_acp_for_unsupported_agent` test (mirrors `build_options_rejects_plan_for_unsupported_agent`); a passing case for `cline` + `LaunchMode::Acp` produces a `ContainerOption::Entrypoint` matching `["cline", "--acp"]` and `ContainerOption::Acp(true)`.
- `EffectiveConfig::launch_mode`/`launch_mode_fallback`: precedence tests (flag → env → repo → default; global-only for fallback), following the existing precedence-test pattern in `effective.rs`.
- `docker.rs::build_run_argv`: `interactive + acp` emits `-i` and not `-it`; asserts no `-p`/`--network` flags are introduced anywhere in the ACP argv path (regression guard for the "containerization must not be compromised" constraint).
- `spawn_piped_interactive_docker`: `stdin_injector` is **not** dropped after the (optional) seeded write, unlike `spawn_piped_docker`.
- `src/engine/acp/protocol.rs`: JSON-RPC line-framing round-trip for each `SessionUpdate` variant; a malformed/non-JSON line does not panic the parser and is surfaced as a warning event, not an error that tears down the session.
- `src/engine/acp/session.rs`: a `session/request_permission` call routes to `AcpFrontend::request_permission` and the resulting `respond_permission` call resolves the correct pending JSON-RPC request id (test with ≥2 concurrent pending requests to catch id-mixups).

**Integration tests**
- `exec_workflow` with repo `launchMode: acp`, one step's agent lacking `supports_acp`, `launchModeFallback: error` — workflow launch fails pre-flight (no container spawned for *any* step) with an error naming the offending step/agent.
- Same setup with `launchModeFallback: stdio` — the offending step launches via the ordinary stdio/PTY path (argv has `-it` and the stdio entrypoint), other steps launch via ACP (`-i`, acp entrypoint); a `Warning` message is emitted for the downgraded step only.
- `chat --launch-mode acp` against an unsupported agent — non-zero exit, error names the agent, no container is launched (assert via mock-docker call count).
- `chat --launch-mode acp` against `cline` (mock docker) — argv includes `cline --acp` and `-i` without `-t`.
- `--yolo --launch-mode acp` — session driver auto-approves permission requests without invoking `AcpFrontend::request_permission` (fake `AcpFrontend` counts calls).

**TUI render tests** (`src/frontend/tui/tests/render_tests.rs`, `tabs/tests.rs` patterns)
- An ACP-mode focused slot renders `ACP_BORDER_COLOR`, not `Color::Green`, in the same phase/focus states that a stdio slot would render green (mirror `window_border_color_done_focused_is_green`/`tab_color_running_with_pty_container_visible_is_green`).
- A tab whose focused slot is ACP-mode and `ExecutionPhase::Error` still renders `Color::Red` (phase-based color takes precedence over the ACP identity color, matching the stdio window's existing precedence).
- Mixed-mode parallel group: minimized bars render one purple bar and one green bar correctly for a two-slot group with different launch modes.

**End-to-end**
- `awman chat --agent cline --launch-mode acp` in both CLI and TUI modes against the real `cline --acp` binary in CI (env-gated like the existing Apple integration tests): verify the `initialize` handshake completes and at least one `session/update` renders through each frontend's `AcpFrontend` impl.

## Codebase Integration:

- **`src/data/config/repo.rs`**: `LaunchMode` enum + `RepoConfig.launch_mode`.
- **`src/data/config/global.rs`**: `LaunchModeFallback` enum + `GlobalConfig.launch_mode_fallback`.
- **`src/data/config/flags.rs`**, **`effective.rs`**: `--launch-mode` flag plumbing and `EffectiveConfig::launch_mode()`/`launch_mode_fallback()`, following the `runtime()`/`auth_mode()` precedence pattern exactly.
- **`src/command/dispatch/catalogue.rs`**: register `--launch-mode <stdio|acp>` on `chat`, `exec prompt`, `exec workflow`, mirroring how `--allow-docker` is registered today. `aspec/uxui/cli.md` is generated/reviewed from this catalogue per its own header note — update its per-command flag tables in the same PR as the catalogue change (not part of this work item's scope to pre-write, but flag it in the PR description).
- **`src/engine/agent/agent_matrix.rs`**: the only file that branches on agent name (per its own doc comment) — `supports_acp`/`acp_entrypoint` fields and `entrypoint_for_acp()` live here; every other module reads the matrix rather than re-branching on agent identity.
- **`src/engine/agent/mod.rs`**: `AcpUnsupported` guard in `build_options`, alongside the existing `PlanModeUnsupported` guard — same file, same pattern.
- **`src/engine/error.rs`**: `EngineError::AcpUnsupported { agent }`, `EngineError::Acp(String)`.
- **`src/engine/container/options.rs`**: `ContainerOption::Acp` / `ResolvedContainerOptions.acp`.
- **`src/engine/container/docker.rs`**, **`apple.rs`**: `-i`-not-`-it` argv change and `spawn_piped_interactive_docker` (and Apple equivalent).
- **`src/engine/acp/`** (new module): `mod.rs`, `protocol.rs`, `client.rs`, `session.rs`, `frontend.rs` (the `AcpFrontend` trait). Layer 1 — imports only from Layer 0 and `crate::engine::*`, per the layering constraint in `aspec/architecture/design.md`.
- **`src/command/commands/chat.rs`**, **`exec_prompt.rs`**, **`exec_workflow.rs`**: launch-mode resolution, the fallback decision, and pre-flight validation for workflows (section 7).
- **`src/frontend/cli/per_command/acp_frontend.rs`** (new): CLI `AcpFrontend` impl.
- **`src/frontend/tui/acp_view.rs`** (new), **`tabs.rs`**, **`per_command/acp_frontend.rs`** (new): TUI agent window rendering, `AgentWindowKind`, `ACP_BORDER_COLOR`, and the TUI `AcpFrontend` impl.
- All new public functions/enums/match arms: unit tests co-located in the same module, following the existing test-module structure throughout the files above (e.g. `#[cfg(test)] mod tests` at the bottom of `agent_matrix.rs`, `effective.rs`, `docker.rs`).
- Frontends must not implement business logic (`aspec/architecture/design.md` Layer 3 constraint) — launch-mode resolution and the fallback decision live entirely in Layer 2 (section 7); the CLI/TUI `AcpFrontend` impls are presentation-only, exactly like today's `AgentFrontend` impls.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update `docs/03-agent-sessions.md`** to describe ACP launch mode alongside the existing agent-session material, including which agents currently support it.
- **Update `docs/07-configuration.md`** with the new `launchMode` (repo) and `launchModeFallback` (global) config fields.
- **Create `docs/16-acp-mode.md`** (next available number) as a user guide to ACP mode — what it changes, the purple agent window in the TUI, the CLI's interactive-stdio ACP experience, and the `--launch-mode` flag — following the same style as `docs/12-runtimes.md` for a comparable "alternate execution mode" doc.
- **Never create work-item-specific docs** (e.g. no "WI 0104 implementation guide" in published docs).
- **Keep all technical/implementation details in this work item spec or code comments**, not in `docs/`.
- **Docs are for end users**, not for developers trying to understand implementation.

See `CLAUDE.md` for more guidance on documentation standards.
