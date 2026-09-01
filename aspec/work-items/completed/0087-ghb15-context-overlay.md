# Work Item: Feature

Title: Context Overlay
Issue: https://github.com/prettysmartdev/awman/issues/15

> **Architectural basis:** This work item is governed by the layered design in
> [`aspec/architecture/2026-grand-architecture.md`](../architecture/2026-grand-architecture.md).
> The implementing agent **must** read that document first and honour its tenets —
> in particular: lower layers never call into higher layers; `OverlayEngine` is the
> single owner of *all* container overlays; and at every opportunity a typed object
> is preferred over a raw `pub fn`. Security constraints in
> [`aspec/architecture/security.md`](../architecture/security.md) also apply — see
> the **Security Reconciliation** section below.

## Summary

Introduce a new `context()` overlay type that gives agents running inside awman a durable, shared workspace on disk plus descriptive system prompt instructions explaining what that workspace is and how to use it. Context overlays combine a **directory mount** (persistent files on the host) with a **system prompt injection** (agent-specific delivery of context instructions), unified into a single overlay expression.

Three scopes are supported:

- `context(global)` — `~/.awman/context/global/` — personal developer preferences, cross-project
- `context(repo)` — `~/.awman/context/repo/{owner}/{repo}/` — project-specific knowledge
- `context(workflow)` — `~/.awman/context/workflows/{invocation-uuid}/` — per-workflow shared workspace, keyed on the workflow invocation's generated UUID (`WorkflowInvocation.id`); system prompt is **dynamically generated** per step with live workflow state

Any scope can be made read-only: `context(global:ro)`, `context(repo:rw)`, etc. (default is `rw`).

Context overlays may be specified via all existing overlay sources: `--overlay` CLI flag, `AWMAN_OVERLAYS` env var, global/repo `config.json`, and workflow TOML/YAML files. Workflow files gain a new top-level `overlays` array that applies to every step in the workflow.

---

## User Stories

### User Story 1
As a: developer using awman across multiple projects

I want to: configure `context(global)` once in my global `~/.awman/config.json` and have every agent I run automatically receive my personal coding preferences, style rules, and past-mistake notes

So I can: stop duplicating `CLAUDE.md` files across repos and instead maintain a single portable developer context that travels with me regardless of which project I am working in.

### User Story 2
As a: developer running a multi-step workflow

I want to: add `context(workflow)` to my workflow TOML file so that every step's agent container gets a shared workspace directory plus a dynamically generated system prompt that tells the agent which step it is, what came before it, and what is still pending

So I can: coordinate multi-agent workflows where earlier steps leave notes and artifacts for later steps, reducing the need to re-discover context that a prior agent already worked out.

### User Story 3
As a: developer working intensively on a complex codebase

I want to: configure `context(repo)` on a specific project so that agents running in that repo automatically receive accumulated architecture notes, gotchas, and best practices that my team and previous agents have collected in `~/.awman/context/repo/{owner}/{repo}/`

So I can: stop re-explaining the same project context in every prompt and instead build a persistent knowledge base that improves over multiple agent sessions.

---

## System Prompts

> **Status: agreed baseline.** The wording below is the finalised default for
> implementation. It may be refined during code review, but it is not a blocking
> open decision — implementation should proceed against this text.

### context(global) system prompt

```
## Global Developer Context

You have access to a persistent global context directory mounted at /awman/context/global.

This directory is the developer's personal, cross-project workspace — maintained by them and shared across all agents, projects, and workflows they run with awman. It is meant to be portable and durable: a personalized addition to per-project CLAUDE.md files that travels with this developer everywhere.

The directory may contain:
- Personal coding style preferences and conventions the developer always wants followed
- Notes on recurring mistakes, things to avoid, or common gotchas encountered across projects
- Shared tools, scripts, or templates that may be used freely
- Any standing guidance the developer wants applied to all of their work

Instructions:
1. Read all files in /awman/context/global at the start of your task to understand the developer's preferences and any standing guidance they have left.
2. You SHOULD write to files inside /awman/context/global to record any significant insights or mistakes, or leave guidance you want future agent sessions to know based on your interactions with the developer. The developer will review and curate these files over time.
3. Treat the contents of this directory as extremely valuable developer guidance and context — refer to it throughout your work.
```

### context(repo) system prompt

```
## Repository-Specific Context

You have access to a repository-specific context directory mounted at /awman/context/repo.

This directory contains knowledge and guidance specific to the project you are currently working in. It is maintained collaboratively by the developer and agents who have worked on this codebase before.

The directory may contain:
- Architecture notes and key design decisions
- Project-specific conventions, patterns, and best practices
- Known gotchas, workarounds, and areas of technical debt
- Domain knowledge and business logic documentation
- Notes from previous agent sessions working on this codebase

Instructions:
1. Read all files in /awman/context/repo at the start of your task to orient yourself to the project-specific context.
2. You SHOULD write to files in /awman/context/repo to capture any significant insights discovered during your work, document decisions made, or leave guidance for future agent sessions. The developer will review and curate these files.
3. Treat the contents of this directory as extremely valuable project context — refer to it alongside the codebase itself throughout your work.
```

### context(workflow) system prompt (dynamic template)

This prompt is regenerated for each workflow step. Template variables are resolved at step-execution time.

```
## Workflow Context

You are running as part of the multi-agent "{workflow_title}" workflow, managed by awman.

Your current step: {current_step_name} (step {current_step_index} of {total_steps})

Workflow progress:
{for each step}
  [✓] {step_name} — completed
  [→] {step_name} — in progress (this is you)
  [○] {step_name} — pending
{end for}

{if work_item_number}
Work item: #{work_item_filename}
{work_item_title_and_summary if available}
{end if}

You have access to a shared workflow context directory mounted at /awman/context/workflow. Every agent step in this workflow shares this directory and can read and write files there.

Instructions:
1. At the start of your task, read any files left by previous steps in /awman/context/workflow — they may contain intermediate results, shared state, helpful scripts, or instructions from earlier steps.
2. Write your outputs, notes, intermediate artifacts, scripts, investigation results, and any state that later steps will need into /awman/context/workflow. Use descriptive file names so downstream steps understand what you produced.
3. You are one step in a coordinated multi-agent workflow. Produce your deliverables reliably (no more, no less), document what you produced in the provided directory, and leave the workspace ready for the next step.
```

---

## System Prompt Delivery Matrix

How awman injects the combined context system prompt into each agent. Research-verified as of June 2026.

| Agent | Delivery Method | Flag / Mechanism | Notes |
|---|---|---|---|
| **claude** | CLI flag | `--append-system-prompt-file <path>` | Appends to default system prompt; preserves tool guidance. Recommended method. |
| **codex** | CLI flag via `--config` | `--config developer_instructions=<text>` | Appends developer-role message after default system prompt. |
| **opencode** | File-based | Write `AGENTS.md` to context dir; mount context dir | `opencode` auto-reads `AGENTS.md`; no CLI flag exists. |
| **maki** | ⚠️ Unsupported | None | No system prompt injection method exists. Context directory is still mounted; agent must read it manually from the initial prompt. |
| **gemini** (deprecated) | Env var | `GEMINI_SYSTEM_MD=<path>` (full replacement) | Destructive — replaces entire default system prompt. Prepend a tool-guidance preamble before context prompts. Flag as degraded mode. |
| **copilot** | Env var | `COPILOT_CUSTOM_INSTRUCTIONS_DIRS=<context-dir>` | Points copilot to scan the mounted context dir for instruction files. |
| **crush** | ⚠️ Unsupported | None (PR pending upstream) | No system prompt injection method as of June 2026. Context directory is still mounted; agent must read it manually. |
| **cline** | CLI flag | `--system <text>` (full replacement) | Destructive — replaces default system prompt. Prepend a tool-guidance preamble before context prompts. Flag as degraded mode. |
| **antigravity (agy)** | CLI flag | `--add-dir <context-dir>` + write `AGENTS.md` to context dir | `--add-dir` expands agy's workspace to include the context dir; `AGENTS.md` inside it is picked up automatically. |

**Degraded delivery** (gemini, cline): Because these agents replace the default system prompt rather than appending to it, awman must prepend a brief preamble restoring baseline agent guidance before the context sections. The `AgentMatrix` entry for these agents carries a `system_prompt_mode: SystemPromptMode::Replace` indicator so the builder knows to prepend the preamble.

**Unsupported agents** (maki, crush): awman still mounts the context directory. awman logs a `Warning`-level message at startup: `context() overlay mounted for '{agent}' but system prompt injection is not supported for this agent; the agent will not be automatically notified about the context directory`. The mounted directory is still useful if the developer references it in their prompt.

---

## Security Reconciliation

`aspec/architecture/security.md` states that awman must "never mount any directory
to any Docker container other than the current directory" (with the Git root as an
opt-in, prompted exception). Context overlays mount host directories under
`~/.awman/context/` — outside the working directory and Git root — so this section
records how the feature reconciles with that constraint. This is an explicit,
deliberate decision, not an oversight.

- **Sanctioned awman-managed carve-out.** Context dirs live exclusively under
  `~/.awman/` and are created and owned by awman, exactly like the existing skills
  mount (`~/.awman/skills`) and agent-settings passthrough (`~/.claude`, etc.).
  The security rule's intent is to prevent awman from exposing *arbitrary host or
  parent directories* (especially a user's broader filesystem) to agent
  containers. awman-managed subtrees of `~/.awman/` are an established, bounded
  exception. `ContextDirResolver` MUST only ever resolve paths under
  `~/.awman/context/` and MUST reject (hard error) any attempt to escape that root
  (e.g. `..` in a derived repo slug), so the mount surface cannot be widened by
  crafted remote URLs or workflow IDs.
- **`security.md` should be amended** to name this carve-out explicitly (the skills
  and agent-settings mounts predate it and are likewise undocumented there). Until
  amended, treat this section as the governing decision for context overlays.
- **Expanded trust surface — acknowledged.** Because `context(global)` and
  `context(repo)` default to `rw` and agents are instructed to write back notes,
  an agent (or prompt-injected content it processes) can persist files on the host
  that are **automatically injected into future agent runs**. This is a
  cross-session persistence channel that does not exist for ephemeral overlays.
  Mitigations:
  - The directories are awman-managed, never executed on the host (the core
    security guarantee — code only ever runs inside containers — is unchanged).
  - The `:ro` permission is a first-class option for users who want read-only
    context (e.g. `context(repo:ro)` in shared/CI settings).
  - The `rw` default is retained because every user story (US1–US3) depends on
    agents writing accumulated notes back; making `ro` the default would defeat the
    feature's primary purpose. Docs MUST call out the persistence/trust implication
    so users can choose `:ro` deliberately.
- **No new execution paths.** Context overlays add *mounts and prompt text only*.
  They never cause anything to execute on the host, and all agent execution
  remains containerized per `security.md`.

---

## Implementation Details

### 1. New types: `src/command/commands/mod.rs`

Add a `ContextScope` enum and extend `TypedOverlay`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextScope {
    Global,
    Repo,
    Workflow,
}

// Add to TypedOverlay enum:
TypedOverlay::Context {
    scope: ContextScope,
    permission: crate::engine::container::options::OverlayPermission,
}
```

Add `context_overlays: Vec<ContextOverlaySpec>` to `CollectedOverlays` (distinct from resolved directories so callers can build the dynamic system prompt separately):

```rust
pub struct ContextOverlaySpec {
    pub scope: ContextScope,
    pub permission: OverlayPermission,
}

pub struct CollectedOverlays {
    pub directories: Vec<DirectorySpec>,
    pub include_all_skills: bool,
    pub named_skills: Vec<String>,
    pub env_passthrough: Vec<String>,
    pub context_overlays: Vec<ContextOverlaySpec>,  // new
}
```

### 2. Parsing: extend `parse_single_typed_overlay`

```
context(global)          → Context { scope: Global, permission: ReadWrite }
context(repo)            → Context { scope: Repo, permission: ReadWrite }
context(workflow)        → Context { scope: Workflow, permission: ReadWrite }
context(global:ro)       → Context { scope: Global, permission: ReadOnly }
context(workflow:rw)     → Context { scope: Workflow, permission: ReadWrite }
```

### 3. New module: `src/data/fs/context_dirs.rs`

Responsible for resolving host-side context directory paths and ensuring they exist:

```rust
pub struct ContextDirResolver {
    awman_home: PathBuf,  // ~/.awman/
}

impl ContextDirResolver {
    /// ~/.awman/context/global/
    pub fn global_dir(&self) -> PathBuf;

    /// ~/.awman/context/repo/{owner}/{repo}/
    /// Derived from `session.git_root()` via the git remote URL.
    pub fn repo_dir(&self, session: &Session) -> PathBuf;

    /// ~/.awman/context/workflows/{invocation_uuid}/
    /// Keyed on the workflow invocation's generated UUID
    /// (`WorkflowInvocation.id`), so each run gets its own durable directory and
    /// resuming the same invocation reuses it. In non-workflow contexts
    /// (`exec prompt`, `chat`) callers pass the session UUID instead.
    pub fn workflow_dir(&self, invocation_uuid: uuid::Uuid) -> PathBuf;

    /// Create the directory if it does not exist. Idempotent.
    pub fn ensure_exists(&self, path: &Path) -> std::io::Result<()>;
}
```

Container mount paths:
- Global: `/awman/context/global`
- Repo: `/awman/context/repo`
- Workflow: `/awman/context/workflow`

### 4. New module: `src/engine/context_prompt.rs`

Per grand-architecture Tenet 3 (typed object over raw `pub fn`), expose a typed
`ContextPromptBuilder` rather than a bag of free functions. The builder is
stateless w.r.t. the OS (pure text construction), so it carries only the inputs
it needs and yields the combined prompt:

```rust
pub struct WorkflowStepInfo {
    pub workflow_title: String,
    pub current_step_name: String,
    pub current_step_index: usize,
    pub total_steps: usize,
    pub steps: Vec<(String, StepStatus)>,  // (name, status)
    pub work_item_number: Option<u32>,
    pub work_item_title: Option<String>,
}

/// Accumulates the per-scope sections active for one agent run and renders the
/// single combined system-prompt string (sections separated by `\n---\n`,
/// workflow section always last).
pub struct ContextPromptBuilder { /* private */ }

impl ContextPromptBuilder {
    pub fn new() -> Self;
    pub fn with_global(self) -> Self;
    pub fn with_repo(self) -> Self;
    /// Workflow scope inside a workflow run (full step-progression detail).
    pub fn with_workflow(self, info: &WorkflowStepInfo) -> Self;
    /// Workflow scope outside a workflow run (minimal one-shot wording).
    pub fn with_workflow_oneshot(self) -> Self;
    /// `None` when no scopes were added.
    pub fn build(self) -> Option<String>;
}
```

### 5. Extend `AgentMatrix`: `src/engine/agent/agent_matrix.rs`

Add system prompt delivery metadata:

```rust
pub enum SystemPromptMode {
    /// Append to default system prompt (safe).
    Append,
    /// Replace default system prompt (destructive; prepend preamble).
    Replace,
    /// File-based: write AGENTS.md into context dir.
    AgentsMd,
    /// Env var pointing to a file.
    EnvFile { var: &'static str },
    /// Extra workspace dir flag (agy --add-dir).
    AddDir { flag: &'static str },
    /// Not supported.
    Unsupported,
}

pub struct AgentMatrix {
    // ... existing fields ...
    pub system_prompt_delivery: SystemPromptMode,
    pub system_prompt_flag: Option<&'static str>,
}
```

Per-agent assignments:
- `claude`: `Append`, flag `--append-system-prompt-file`
- `codex`: `Append`, flag `--config developer_instructions`
- `opencode`: `AgentsMd`, flag `None`
- `maki`: `Unsupported`, flag `None`
- `gemini`: `Replace`, flag via `EnvFile { var: "GEMINI_SYSTEM_MD" }`
- `copilot`: `EnvFile { var: "COPILOT_CUSTOM_INSTRUCTIONS_DIRS" }`
- `crush`: `Unsupported`, flag `None`
- `cline`: `Replace`, flag `--system`
- `antigravity`: `AddDir { flag: "--add-dir" }` + `AgentsMd`

### 6. New `ContainerOption` variants: `src/engine/container/options.rs`

```rust
ContainerOption::SystemPromptFile {
    host_path: PathBuf,
    container_path: PathBuf,
    flag: String,           // e.g. "--append-system-prompt-file"
},
ContainerOption::SystemPromptEnvFile {
    env_var: String,        // e.g. "GEMINI_SYSTEM_MD"
    container_path: PathBuf,
},
ContainerOption::SystemPromptInline {
    flag: String,           // e.g. "--system"
    text: String,
},
ContainerOption::AgentAddDir {
    flag: String,           // e.g. "--add-dir"
    container_path: PathBuf,
},
```

**Emission ownership (single funnel):** these variants are emitted **only** by
`AgentEngine::build_options` (§7). No higher layer — and in particular no Layer 2
command code — constructs or pushes `ContainerOption`s. This preserves the
grand-architecture invariant that the agent engine is the one place that turns an
agent run into a container option set, mirroring how `build_overlays` is today the
one place that turns an `OverlayRequest` into `ContainerOption::Overlay`s.

### 7. `AgentRunOptions` + system-prompt delivery: `src/engine/agent/mod.rs`

`AgentRunOptions` gains the context inputs (not pre-built container options):

```rust
/// Combined, pre-rendered system-prompt text (from `ContextPromptBuilder`).
pub system_prompt: Option<String>,
/// Resolved context-directory overlays for this run (host path already
/// ensured-to-exist by Layer 0; container path + permission per scope).
pub context_overlays: Vec<ContextOverlay>,
```

`ContextOverlay` is a small Layer-1 value type (lives in `engine::overlay`
alongside `DirectorySpec`):

```rust
pub struct ContextOverlay {
    pub scope: ContextScope,
    pub host_path: PathBuf,
    pub container_path: PathBuf,   // /awman/context/{global,repo,workflow}
    pub permission: OverlayPermission,
}
```

`build_options` is the **single emitter** and does the following, in order:

1. **Directory mounts** — folds `run.context_overlays` into the `OverlayRequest`
   it already constructs (§8) so the context dirs flow through
   `OverlayEngine::build_overlays` and share its dedup-by-host-path /
   most-restrictive-permission-wins logic. The agent engine does **not** build
   `ContainerOption::Overlay` for context dirs itself.
2. **System-prompt delivery** — when `run.system_prompt` is `Some`, it consults
   `AgentMatrix.system_prompt_delivery` for the agent and emits the matching
   variant(s) from §6: writing the prompt to a managed temp file for the
   file/env-file modes, planting `AGENTS.md` into the resolved context dir for the
   `AgentsMd` mode (only when the content hash changed), prepending the
   restore-baseline preamble for the `Replace` modes, or logging the
   unsupported-agent `Warning`. The matrix is the only thing that names agents, so
   this stays the agent engine's responsibility.

**Ownership split (resolves the prior open question of "which engine owns system
prompt injection"):** `OverlayEngine` owns *mounting* the context directory;
`AgentEngine` owns *what instructions the agent receives* (matrix-driven delivery,
including the `AGENTS.md` plant whose content is the prompt text). Both read from
the same resolved `ContextOverlay`, so there is one host-path source of truth and
no Layer-2 stitching.

### 8. `OverlayEngine` owns the context directory mounts: `src/engine/overlay/mod.rs`

Per the grand architecture, `OverlayEngine` is "responsible for constructing and
managing **all** types of overlays granted to agent containers." Extend
`OverlayRequest` so context dirs are first-class overlay inputs rather than a
parallel path:

```rust
pub struct OverlayRequest {
    // ... existing fields ...
    /// Context-directory overlays (global/repo/workflow). Resolved host paths
    /// arrive already ensured-to-exist; build_overlays mounts them through the
    /// same dedup/merge pipeline as user directories and skills.
    pub context_overlays: Vec<ContextOverlay>,
}
```

`build_overlays` gains a step that turns each `ContextOverlay` into an
`OverlaySpec` and inserts it via the existing `insert_or_merge` keyed on the
canonical host path — so a context dir that collides with a user `dir(...)`
overlay is merged (most-restrictive permission wins) exactly like any other
overlay. No new bypass; no second resolution path.

Layer-0 host-path resolution stays in `ContextDirResolver` (§3); Layer-2 callers
(§11/§12) only assemble the `Vec<ContextOverlay>` and the system-prompt text as
**inputs** to `AgentRunOptions` — they never build or push `ContainerOption`s.

### 9. Workflow top-level overlays: `src/data/workflow_definition.rs`

Add to `Workflow` struct:

```rust
/// Overlays applied to every agent step in this workflow (in addition to
/// step-level and global/CLI/config overlays).
#[serde(default)]
pub overlays: Option<Vec<String>>,
```

### 10. `collect_all_overlay_specs` signature change: `src/command/commands/mod.rs`

Add `workflow_overlays` parameter between CLI overlays and step overlays:

```rust
pub fn collect_all_overlay_specs(
    session: &Session,
    cli_typed_overlays: Vec<TypedOverlay>,
    workflow_overlays: Option<&[String]>,  // new — from Workflow.overlays
    step_overlays: Option<&[String]>,
) -> Result<CollectedOverlays, CommandError>
```

Priority order (lowest to highest): global config → repo config → `AWMAN_OVERLAYS` → CLI flags → workflow-level → step-level.

Update all callers: `exec_prompt.rs`, `exec_workflow.rs` (`CommandLayerFactory::execution_for_step`, `collect_single_entry_overlays`).

### 11. `CommandLayerFactory`: `src/command/commands/exec_workflow.rs`

Store `workflow_overlays: Option<Vec<String>>` (from `Workflow.overlays`) and the
workflow invocation UUID (`WorkflowInvocation.id`) on the factory. In
`execution_for_step`, Layer 2 only assembles **inputs** — it never builds or
pushes `ContainerOption`s:

1. Call `collect_all_overlay_specs` with `workflow_overlays` and `step.overlays`.
2. For each `ContextOverlaySpec` in `collected.context_overlays`, resolve its host
   path via `ContextDirResolver` (`global_dir` / `repo_dir(session)` /
   `workflow_dir(invocation_uuid)`, each `ensure_exists`) into a `ContextOverlay`,
   and set `run_opts.context_overlays`.
3. If any `Workflow`-scope overlay is present, build `WorkflowStepInfo` from the
   engine's current state and render the combined prompt with
   `ContextPromptBuilder` (`with_global`/`with_repo`/`with_workflow(info)`); set
   `run_opts.system_prompt = builder.build()`.
4. Call `build_options` as before. `AgentEngine` (§7) emits **all** container
   options, including the context dir mounts (via `OverlayEngine`) and the
   system-prompt delivery options. Nothing is stitched into the options vec
   afterwards.

### 12. `exec_prompt.rs` and `chat.rs`

Same input-assembly pattern, but with no workflow step info: build the prompt with
`ContextPromptBuilder::with_workflow_oneshot()` for `context(workflow)` and emit an
`Info` message ("context(workflow) is most useful inside a workflow; workflow state
will be empty"). For the workflow context **directory**, pass the session UUID
(`session.id().as_uuid()`) to `workflow_dir(...)` so the one-shot run still gets a
stable, unique workspace. Global and repo scopes behave identically to the
workflow case.

---

## Edge Case Considerations

- **Workflow context dir uniqueness**: The directory is keyed on the workflow invocation's generated UUID (`WorkflowInvocation.id`), so every distinct run — including two simultaneous runs of the *same* workflow — gets its own isolated directory, and resuming an invocation reuses the same directory because the UUID is persisted in the saved workflow state. This removes the name-collision hazard entirely; no work-item-based disambiguation is needed. In non-workflow contexts (`exec prompt`, `chat`) the session UUID is used in place of an invocation UUID.

- **Repo context dir derivation**: Derive `{owner}/{repo}` from the git remote URL (`origin`). Fall back to the git root directory name if no remote is configured (e.g. `_local/{dirname}`). Always normalise to lowercase with non-alphanumeric chars replaced by dashes.

- **Read-only context with `AgentsMd` delivery**: If `context(global:ro)` is specified and the delivery method for the agent is `AgentsMd` (which requires writing `AGENTS.md` to the context dir), awman must write `AGENTS.md` before mounting the dir read-only. If the context dir is read-only on the host (unexpected), log a warning and skip `AGENTS.md` generation.

- **Multiple context scopes combined**: If `context(global)` and `context(workflow)` are both active, the combined system prompt is the concatenation of both scope prompts separated by `\n---\n`. The workflow prompt always appears last. Deduplication: if the same scope appears twice (e.g. via both config and step), use it once (first occurrence wins).

- **Destructive system prompt delivery (cline, gemini)**: Prepend a preamble before the context sections that restores baseline agent-tool guidance, e.g. `You are {agent_name}, an AI coding assistant. Use your full capabilities and all available tools to complete the task.` followed by the context section(s). Emit a `Warning` informing the user that the default system prompt has been replaced.

- **`context(workflow)` in `exec prompt`**: No workflow state is available; generate a minimal prompt: `You are running a one-shot awman task. A shared workflow context directory is available at /awman/context/workflow if you need to persist state.` Emit an `Info` message suggesting the workflow scope is most useful inside `exec workflow`.

- **Context dirs do not exist yet**: `ContextDirResolver::ensure_exists` creates the directory (including parents) before mounting. Failure (e.g. permission denied) is a hard error surfaced to the user.

- **System prompt temp file lifecycle**: The temp files backing file/env-file delivery are written and owned by `AgentEngine::build_options` (the single emitter) and retained via the same RAII `tempfile::TempDir` guard the `OverlayEngine` already uses for sanitized agent-settings dirs, so they live as long as the run and are removed on drop — no `/tmp` leakage. The `AgentsMd` plant writes `AGENTS.md` into the resolved context dir itself and is regenerated only when the combined content hash changes.

- **`context()` in setup/teardown steps**: Context overlays in setup/teardown entries follow the same per-entry isolation as other overlays (WI-0082). `context(workflow)` in a setup/teardown step generates a system prompt indicating setup/teardown phase rather than a main step number.

- **Validate no `context()` in setup/teardown skill-banned overlay check**: Extend the existing overlay validation for setup/teardown entries to emit a `Warning` (not error) for `context(workflow)` since workflow state is limited during those phases.

- **`context(repo)` without a git remote**: Log an `Info` message showing the resolved fallback dir path so the user knows where the context is stored.

---

## Test Considerations

- **Unit: parsing** — `parse_overlay_list("context(global)")` → `TypedOverlay::Context { scope: Global, permission: ReadWrite }`. Verify all three scopes, both permissions, and error cases: `context()` (missing scope), `context(unknown)`, `context(global:rx)` (bad permission), `context(global:ro:extra)` (too many parts).

- **Unit: `ContextDirResolver`** — verify `global_dir`, `repo_dir`, `workflow_dir` return expected paths. Verify `workflow_dir(uuid)` places the run under `~/.awman/context/workflows/{uuid}/` and that two different UUIDs yield two different directories. Verify `repo_dir` normalisation for repos with special chars in path. Verify `ensure_exists` creates nested directories.

- **Unit: repo slug derivation** — test remote URL patterns (`https://github.com/org/repo.git`, `git@github.com:org/repo.git`, no remote) → expected slug. Test slug normalisation (uppercase, dots, special chars).

- **Unit: `ContextPromptBuilder`** — `new().build()` is `None`; `with_global()` yields a prompt containing `/awman/context/global`; given a `WorkflowStepInfo` with 3 steps (first completed, second in progress, third pending), `with_workflow(info).build()` contains the step name, `[✓]`, `[→]`, `[○]` markers, and the workflow title; combining two scopes joins them with `\n---\n` and the workflow section is last.

- **Unit: `OverlayEngine` context mounts** — an `OverlayRequest` carrying a `ContextOverlay` produces an `OverlaySpec` with the expected container path, and a context overlay whose host path collides with a user `dir(...)` overlay is merged once with the most-restrictive permission winning.

- **Unit: `build_options` single emitter** — given `AgentRunOptions` with `context_overlays` + `system_prompt` for `claude`, the emitted options contain the context `ContainerOption::Overlay`(s) and a `SystemPromptFile { flag: "--append-system-prompt-file", .. }`, and no `ContainerOption` is produced anywhere outside `build_options`.

- **Unit: `collect_all_overlay_specs` with workflow overlays** — verify priority order: step-level `context(workflow)` overrides nothing (union semantics for context, same as skills); workflow-level `context(global)` is applied to all steps.

- **Unit: deduplication** — `context(global)` appearing in both global config and step overlays results in exactly one `ContextOverlaySpec { scope: Global }` in `CollectedOverlays`.

- **Unit: `AgentMatrix` system prompt delivery** — every entry in `SUPPORTED_AGENTS` has a valid (non-panic) `system_prompt_delivery` value. Verify `claude` maps to `Append`, `maki` maps to `Unsupported`, `cline` maps to `Replace`.

- **Integration: `exec_prompt` with `context(global)`** — in a temp git repo, collect overlays with `context(global)`, resolve it into a `ContextOverlay`, drive `build_options`, and verify the emitted options contain a context overlay mounted at `/awman/context/global` and a system-prompt delivery option.

- **Integration: workflow overlay merging** — a workflow with top-level `overlays: ["context(repo)"]` and a step with `overlays: ["context(global)"]` must produce a `CollectedOverlays` with two `ContextOverlaySpec` entries (both scopes) for that step.

- **Integration: `context(workflow)` dynamic prompt** — across two steps in a minimal workflow, assert the second step's generated prompt marks the first step as `[✓] completed` and the second step as `[→] in progress`.

- **E2E: CLI flag parsing** — `awman exec prompt "hello" --overlay context(global)` parses without error (validate via dispatch + `parse_overlay_list` roundtrip test).

---

## Codebase Integration

- Follow established conventions, best practices, testing, and architecture patterns from the project's `aspec/`, and the layered tenets of [`2026-grand-architecture.md`](../architecture/2026-grand-architecture.md).
- `ContextDirResolver` belongs in Layer 0 (`src/data/fs/`) alongside `overlay_paths.rs` and `skill_dirs.rs`.
- `ContextPromptBuilder` (`src/engine/context_prompt.rs`) and the `ContextOverlay` value type belong in Layer 1 (`src/engine/`); `ContextOverlay` lives in `engine::overlay` next to `DirectorySpec`.
- **`OverlayEngine` is the single owner of the context directory mounts** — context dirs flow through `OverlayRequest`/`build_overlays`, not a parallel resolver. Do not introduce a separate context-overlay path that emits `ContainerOption::Overlay` outside `OverlayEngine`.
- **`AgentEngine::build_options` is the single emitter of all `ContainerOption`s** for an agent run, including system-prompt delivery. Layer 2 (command) assembles inputs (`context_overlays`, `system_prompt`) only — it must never build or push `ContainerOption`s.
- Parsing changes go in `src/command/commands/mod.rs` alongside existing `parse_single_typed_overlay`.
- `AgentMatrix` changes are isolated to `src/engine/agent/agent_matrix.rs` — this is the only file that names agents explicitly, and the only place system-prompt delivery is selected per agent.
- `ContainerOption` changes in `src/engine/container/options.rs` must be handled by the Docker and Apple Containers runtime backends.
- The `collect_all_overlay_specs` signature change requires updating `exec_prompt.rs`, `exec_workflow.rs` (two call sites), and any API session setup that calls it.
- The `Workflow` struct change is non-breaking (new optional field with `#[serde(default)]`).
- Emit appropriate `UserMessage` warnings for unsupported agents (maki, crush) and degraded delivery (cline, gemini) at resolution time, not at parse time.
- Prefer typed objects over free `pub fn`s (Tenet 3): `ContextDirResolver`, `ContextPromptBuilder`, and the `OverlayRequest` extension all follow the existing resolver/engine patterns.
- Every new public item must have unit tests in the same file following the existing pattern of inline `#[cfg(test)]` modules.

---

## Documentation

After implementation is complete, update user-facing documentation in `docs/`:

- **Update `docs/08-overlays.md`** (or equivalent) to document `context()` alongside existing overlay types, with examples for all three scopes, the `ro`/`rw` permission syntax, and a note on which agents support system prompt injection natively.
- **Create `docs/XX-context-overlay.md`** as a dedicated user guide covering: what context scopes are, how to set up the global and repo scopes, how workflow context enables step-to-step coordination, and worked examples of combining context with workflows.
- **Do not document** internal implementation details, system prompt templates, or agent matrix decisions — those belong in code comments or this work item spec.

See `CLAUDE.md` for more guidance on documentation standards.
