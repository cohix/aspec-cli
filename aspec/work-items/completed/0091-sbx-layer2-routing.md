# Work Item: Feature

Title: Route `awman chat` / `awman exec` / workflows to the sandbox runtime (`docker-sbx-experimental`)
Issue: https://github.com/connerhicks/awman/issues/16

**Prerequisite**: work item 0090 (Docker Sandbox Runtime — `DSbxBackend`). WI 0090 delivered the working engine tier (kit emission, lifecycle, credential injection, session config, `run_interactive`, `exec_args`) and the `awman ready` integration. This work item closes the remaining Layer-2 gap: the agent-launching commands still construct container-paradigm options unconditionally and error under the sandbox runtime.

## Summary

As of WI 0090, `awman ready` works end-to-end under `runtime: "docker-sbx-experimental"`, but `awman chat`, `awman exec`, and `awman exec workflow` still call `Engines::require_container_runtime()` (`src/command/commands/chat.rs`) and build `ResolvedAgentOptions::Container(...)` unconditionally — a sandbox-configured user gets a clear `EngineError::NotImplemented` ("this command does not yet route to the sandbox runtime") instead of a running agent. This work item teaches the Layer-2 commands to:

1. Detect the active runtime paradigm via `engines.runtime.capabilities()` / `runtime_name()` and construct the matching `ResolvedAgentOptions` variant from the same flag/config inputs the container path consumes today (agent, workspace, seeded prompt, model, system prompts, tools, env, memory). Do not make Layer 2 depend on `DSbxBackend` or on sandbox kit internals. The typed `engines.sandbox_runtime` handle exists for sandbox-only lifecycle operations, but ordinary launch routing must go through the cross-paradigm `AgentRuntimeEngine` facade.
2. Route the launch through the two-step `AgentRuntimeEngine` trait flow (`engines.runtime.build(...)` → `AgentInstance::run_with_frontend(...)`), which WI 0090 already wired to `dsbx::run_interactive`.
3. Deliver seeded prompts for `kind: mixin` kits. WI 0090 writes the prompt into `<workspace>/.awman/session.json` and appends it positionally for `kind: agent` kits, but the mixin apply-scripts (`templates/sbx-apply.{claude,codex,gemini,copilot,opencode}.sh`) do not yet consume `seeded_prompt` (nor `model`, `system_prompt_*`, `allowed_tools`/`disallowed_tools` where the agent supports settings-file equivalents). For mixin kits, render the supported dynamic fields from `session.json` into the agent's native config/settings files during `commands.startup`; do not append seeded prompts positionally for mixins, or prompts can be delivered twice / to the wrong entrypoint. If a specific mixin cannot support a field through native config, document that explicit unsupported field and surface a clear warning/error rather than silently dropping it.
4. Satisfy WI 0090's deferred checklist item: "`awman exec --runtime docker-sbx-experimental` no longer returns `EngineError::NotImplemented` — it actually runs." That item was deferred from WI 0090 (whose change-set scoped Layer 2 out) to this work item — see the History note in `0090-ghb16-docker-sandbox-runtime.md`.

## User Stories

### User Story 1:
As a: user who has configured `runtime: "docker-sbx-experimental"` and run `awman ready`

I want to:
run `awman chat <agent>` and `awman exec <agent> "<prompt>"` and have them launch microVM sandboxes

So I can:
actually use the sandbox runtime for day-to-day agent work, not just prepare it.

### User Story 2:
As a: user running multi-agent workflows under sbx

I want to:
`awman exec workflow` to create one persistent sandbox per agent and reuse it across steps

So I can:
pay the per-agent install cost once per worktree, as the persistent-sandbox model promises.

## Implementation Details:
- `src/engine/agent/mod.rs`: keep per-agent argument/mode/model/tool/system-prompt mapping centralized. Extend `AgentEngine` with a cross-paradigm option builder (or separate sibling methods) that maps `AgentRunOptions` to either `ContainerOption` or `SandboxOption` based on the active runtime capabilities. Do not duplicate the agent matrix logic separately in `chat.rs`, `exec_prompt.rs`, and `exec_workflow.rs`.
- `src/command/commands/chat.rs` / `exec_prompt.rs` / `exec_workflow.rs`: remove the early `engines.require_container_runtime()` gate and call the centralized agent option builder to obtain the correct `ResolvedAgentOptions` variant; launch via `engines.runtime.build(...)` and then `AgentInstance::run_with_frontend(...)`. User-facing status strings should say "agent" or the runtime display name instead of always saying "container".
- Re-attach: use `AgentRuntimeEngine::exec_args()` (already implemented for sbx with `--env` passthrough) and pass `COLUMNS`/`LINES` per the Issue #63 PTY-size workaround.
- Mixin seeded-prompt delivery: extend the five mixin `templates/sbx-apply.<agent>.sh` scripts to consume `session.json` fields they can faithfully express in native agent config/settings, including `seeded_prompt` where supported. Bump `session.json` `schema_version` if the input contract changes. Keep the writer (`DSbxSessionConfig`) and the scripts versioned together.
- Background/non-interactive exec must register credentials before creating/restarting a sandbox, using the same credential path as `run_interactive` and the command's `UserMessageSink`. Credential values must never be written to `session.json`, argv, logs, or status messages; continue piping secrets to `sbx secret set` via stdin and masking any subprocess output before publication.
- Layer discipline: Layer 2 talks only to `AgentRuntimeEngine` / `Engines` handles; no `DSbxBackend`, `SandboxBackend`, or kit internals leak upward (WI 0089 invariant).

## Edge Case Considerations:
- `awman exec` with a seeded prompt against each kit kind (mixin vs agent) — both must deliver the prompt exactly once.
- `awman exec` with credentials under sbx — credentials must be injected via `sbx secret set` / proxy mechanisms only, and the generated `<workspace>/.awman/session.json` must not contain credential-like keys or values.
- Runtime switching mid-project: chat under Docker, then sbx, then Docker again (the WI 0090 switching test covers detection; extend it to a real launch path when env-gated).
- A stopped persistent sandbox must be restarted (`sbx run --name`, no `--kit`) by chat/exec, not recreated.
- Sandbox-unsupported flags surfaced exactly as in WI 0090 (`--cpus` warning, `--allow-docker` debug trace).

## Test Considerations:
- Unit: option-construction parity tests — for a fixed set of chat/exec flags, the centralized builder emits `ResolvedAgentOptions::Sandbox` with the same intent as the container variant (agent, prompt, model, tools, env classification) and does not require each command to hand-build its own `SandboxOption` list.
- Unit/security: session-config tests proving credential-like env vars and literal secrets are excluded from `session.json`, while non-sensitive dynamic fields still reach the mixin apply scripts.
- Unit: mixin seeded-prompt tests covering all five mixin agents and all four `kind: agent` kits. Mixin prompts must be consumed from `session.json`; `kind: agent` prompts must be appended positionally; no path may deliver a prompt twice.
- Env-gated integration (macOS arm64 + `AWMAN_TEST_SBX=1`): `awman exec` non-interactive prompt end-to-end; two-step workflow reusing one sandbox per agent (these were specified in WI 0090's test plan but blocked on this routing).
- Regression: all Docker/Apple chat/exec tests unchanged.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec, WI 0089 (`AgentRuntimeEngine` abstraction), and WI 0090 (dsbx engine tier).

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- Update `docs/16-runtimes.md`: remove the "chat/exec not yet routed" limitation once it ships; document mixin seeded-prompt behavior.
- **Never create work-item-specific docs**; keep implementation details in this spec or code comments.

See `CLAUDE.md` for more guidance on documentation standards.
