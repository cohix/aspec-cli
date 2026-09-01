# Work Item: Enhancement

Title: Dynamic Workflow Config
Issue: issuelink

## Summary

Add a `dynamicWorkflows` section to the repo-local `.awman/config.json` that lets teams customize which agents and models are available in dynamic workflows, cap concurrent steps, and set a default leader — all without touching CLI flags. The leader prompt is updated to include this information so the leader agent can make informed scheduling decisions. A mismatch between configured agents and available Dockerfiles fails fast with a descriptive error. The TUI config view is audited and improved to handle the resulting long and nested values legibly.

## User Stories

### User Story 1
As a: user

I want to: configure which agents and models are available for dynamic workflows in `.awman/config.json`

So I can: restrict workflow steps to a known, approved set of agents and models without passing flags on every run.

### User Story 2
As a: user

I want to: set `maxConcurrentSteps` and `defaultLeader` in the repo config

So I can: control workflow parallelism and the leader agent/model as repo-level defaults that the whole team shares via version control.

### User Story 3
As a: user

I want to: view and understand `dynamicWorkflows` values in the TUI config dialog (Ctrl-,) without them being truncated or unreadable

So I can: inspect and reason about the current workflow configuration without leaving the TUI.

## Implementation Details

### 1. Extend `RepoConfig` with `DynamicWorkflowsConfig`

**File:** `src/data/config/repo.rs`

Add a new nested struct and a field to `RepoConfig`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DynamicWorkflowsConfig {
    /// agent name → list of model name strings available for that agent
    pub agents_to_models: Option<HashMap<String, Vec<String>>>,
    /// advisory cap on concurrent workflow steps (None = unlimited)
    pub max_concurrent_steps: Option<usize>,
    /// default leader spec in agent::model format; overridden by --leader flag
    pub default_leader: Option<String>,
}
```

Add to `RepoConfig`:
```rust
#[serde(
    rename = "dynamicWorkflows",
    default,
    skip_serializing_if = "Option::is_none"
)]
pub dynamic_workflows: Option<DynamicWorkflowsConfig>,
```

The `RepoConfig` field must be explicitly renamed to `dynamicWorkflows`; otherwise serde will serialize it as `dynamic_workflows`, which does not match the documented repo config shape. Follow the existing `workItems` field pattern for top-level JSON naming, defaulting, and `skip_serializing_if`.

After deserialization in `RepoConfig::load`, run Layer 0 semantic validation before returning:

- `maxConcurrentSteps` must be absent or `>= 1`
- `defaultLeader`, when present, must have exactly the `agent::model` shape with two non-empty components and no leading/trailing whitespace in either component
- the `defaultLeader` agent component and every `agentsToModels` key must follow the same lexical constraints as `AgentName` (`[A-Za-z0-9_-]`, non-empty, length limit)
- `agentsToModels`, when present, must not contain empty model lists or empty/whitespace model names

Keep this validation in `src/data/config/repo.rs`. Do not call `LeaderSpec::parse` from `src/command/commands/exec_workflow.rs`; the data layer must not depend on the command layer. Instead, add a small data-layer validator that enforces the same syntax and returns `DataError::Other` with the configured error text.

### 2. Validate agents before workflow start

**File:** `src/command/commands/exec_workflow.rs`

After `discover_agent_dockerfiles()` is called and `available_agents` is populated, if `repo_config.dynamic_workflows.agents_to_models` is `Some(map)` and non-empty, validate every configured key against the set of discovered agent names. This check belongs in `exec_workflow.rs` because it depends on filesystem discovery. Collect all missing names and fail with a single descriptive error:

```
Error: dynamicWorkflows.agentsToModels references agents that have no Dockerfile in this repo: [foo, bar].
Available agents: [claude, codex, gemini].
Add a .awman/Dockerfile.<agent> for each missing agent, or remove it from agentsToModels.
```

Fail before spawning any container. Do not warn and continue.

Perform case-insensitive matching only as a compatibility aid. Build a normalized effective map for validation and prompt construction using the canonical discovered Dockerfile agent name; do not silently rewrite `.awman/config.json` during workflow execution. If two configured keys normalize to the same discovered agent, fail with a configuration error instead of letting one override the other.

### 3. Build the agents section for the leader prompt

**File:** `src/command/commands/exec_workflow.rs` (near the `format_available_agents` call, lines ~1513–1523, 1740–1744)

Add a new helper `format_agents_with_models(map: &HashMap<String, Vec<String>>) -> String` that renders as:

```
- claude: claude-opus-4-8, claude-sonnet-4-6
- codex: codex-mini-latest
- gemini: gemini-2.5-pro
```

When `agents_to_models` is `Some` and non-empty, pass its output to `build_leader_prompt` instead of the Dockerfile-derived list. When `None` or empty, keep the existing `format_available_agents(&available_agents)` call.

Sort agent names alphabetically before rendering, and preserve each configured model list order. Do not iterate a `HashMap` directly when producing user-visible prompt content; the leader prompt must be deterministic for stable tests and reproducible workflow design.

### 4. Extend `build_leader_prompt` with a `max_concurrent_steps` advisory

**Files:** `src/data/dynamic_workflow_assets.rs`, `src/assets/dynamic/leader-prompt.md`

Add a `max_concurrent_steps: Option<usize>` parameter to `build_leader_prompt`. Add a `{{max_concurrent_steps_note}}` placeholder to the template. Render it as:

- When `Some(n)`: `"Note: the repository configuration advises a maximum of {n} concurrent steps. Plan your workflow accordingly."`
- When `None`: `""` (empty — omit the line entirely)

This is advisory only. No hard enforcement in the scheduler at this time.

Update every existing `build_leader_prompt` call site and unit test. Add an assertion that no `{{max_concurrent_steps_note}}` placeholder remains in the rendered prompt.

### 5. Leader resolution order

**File:** `src/command/commands/exec_workflow.rs` (near `LeaderSpec::parse`, lines ~71–87)

Apply this priority order when resolving the leader for a dynamic workflow:

1. `--leader` CLI flag (existing behavior)
2. `repo_config.dynamic_workflows.default_leader` (new)
3. Existing `--model` + repo/global default-agent fallback from WI-0092

`defaultLeader` is a full `agent::model` leader selection. When it is present and `--leader` is absent, it governs both the leader agent and leader model. A separate `--model` flag should continue to behave as the generated workflow's session-level model default, but it must not override the `defaultLeader` model. Parse `defaultLeader` with the Layer 0 validator in `RepoConfig::load`, then either reuse a small command-layer conversion helper or parse again in `resolve_leader_model` to construct `LeaderSpec`.

### 6. TUI config display — audit and improvements

**Files:** `src/frontend/tui/render.rs` (`render_config_show`), `src/command/commands/config.rs` (`collect_config_rows`, `config_field_value`)

**Audit findings to address:**

- `config_field_value` currently calls `.to_string()` on array/object JSON values, which produces compact single-line JSON that can be extremely wide.
- Long values can overflow table cells because the current renderer does not wrap or scroll within a cell.

**Changes:**

- Add the new user-facing field names to `VALID_CONFIG_FIELDS` using the JSON names `dynamicWorkflows.defaultLeader`, `dynamicWorkflows.maxConcurrentSteps`, and `dynamicWorkflows.agentsToModels`. These are repo-only fields.
- In `collect_config_rows`, flatten `DynamicWorkflowsConfig` sub-fields as dot-keyed rows so they appear as individual navigable rows rather than one unreadable blob.
- For `agentsToModels`, render each entry as a separate display row with key `dynamicWorkflows.agentsToModels.<agentName>` and value as the comma-separated model list.
- In `render_config_show`, truncate cell values that exceed the available column width with a `…` suffix, and show the full value in a detail line at the bottom of the dialog when that row is selected (similar to how a status bar shows details).
- Mark `dynamicWorkflows.agentsToModels` and all `dynamicWorkflows.agentsToModels.<agentName>` rows as `read_only` in the `ConfigFieldRow` produced by the command layer. The TUI should consume that row metadata and show the existing read-only path with the message text updated to: "Edit this value directly in .awman/config.json". The other scalar fields (`defaultLeader`, `maxConcurrentSteps`) remain inline-editable.
- Add `validate_and_coerce` support for the scalar dynamic workflow fields: `defaultLeader` validates the `agent::model` shape, and `maxConcurrentSteps` parses a positive integer and rejects `0`.
- Keep CLI `config show` output untruncated; truncation/detail rendering is TUI-only presentation logic.

## Edge Case Considerations

- **Empty `agentsToModels` map (`{}`)**: treat the same as `None` — fall back to Dockerfile discovery. Emit a debug-level message only; do not show a user-facing warning.
- **`agentsToModels` agent exists in Dockerfiles but has an empty model list (`[]`)**: treat as a configuration error. Fail with: `"dynamicWorkflows.agentsToModels.claude has an empty model list. Provide at least one model name."`.
- **`agentsToModels` contains an empty or whitespace model name**: treat as a configuration error during `RepoConfig::load`. Fail with: `"dynamicWorkflows.agentsToModels.claude contains an empty model name."`.
- **`agentsToModels` contains an invalid agent key**: reject during `RepoConfig::load` using the same allowed-character and length rules as `AgentName`. This prevents malformed config from reaching prompt construction or later workflow agent resolution.
- **`maxConcurrentSteps: 0`**: reject as invalid during config load — zero concurrent steps would deadlock any workflow. Error: `"dynamicWorkflows.maxConcurrentSteps must be >= 1"`.
- **`defaultLeader` format invalid**: validate during `RepoConfig::load` using a Layer 0 validator equivalent to `LeaderSpec::parse`, plus whitespace and agent-name checks. Surface as a config load error so it fails before any UI or workflow starts.
- **`--leader` flag + `defaultLeader` both set**: `--leader` wins silently (no warning needed — precedence is documented).
- **`--model` flag + `defaultLeader` both set, without `--leader`**: `defaultLeader` wins for the leader's model; `--model` remains the session default model available to the generated workflow's steps.
- **`dynamicWorkflows` key present but all sub-fields absent**: deserializes to `DynamicWorkflowsConfig::default()` with all `None`; treat as if section is absent.
- **Leader agent referenced in `--leader` or `defaultLeader` is not in `agentsToModels`**: proceed without error — the flag takes precedence over the configured subset, and the leader is not a step agent.
- **`agentsToModels` keys use inconsistent casing** (e.g., `"Claude"` vs `"claude"`): compare against discovered agent names case-insensitively, but use the canonical discovered Dockerfile agent name in the effective map passed to validation and prompt rendering. Emit a warning if a case-folded match is the only match.
- **`agentsToModels` contains duplicate keys after case folding** (e.g., `"Claude"` and `"claude"`): fail with a configuration error so model lists are not merged or overwritten ambiguously.
- **Repo has no agent Dockerfiles at all**: `dynamicWorkflows.agentsToModels` being set is still validated, but an empty discovery result with `None` agents_to_models is not an error (existing behavior).

## Test Considerations

- **Unit — `DynamicWorkflowsConfig` serde**: deserialize with all fields, missing fields, extra unknown fields (verify `deny_unknown_fields` is NOT set), and the empty-object case.
- **Unit — `RepoConfig` JSON key names**: round-trip a config containing `dynamicWorkflows` and verify it does not serialize as `dynamic_workflows`.
- **Unit — `maxConcurrentSteps: 0` rejection**: `RepoConfig::load` returns an error for `"maxConcurrentSteps": 0`.
- **Unit — `defaultLeader` format validation**: invalid format (no `::`, empty component, whitespace around a component, invalid agent characters) returns a config load error; valid format parses successfully.
- **Unit — model-list validation**: empty list and empty/whitespace model name both fail during config load.
- **Unit — `agentsToModels` key validation**: empty, whitespace, too-long, or invalid-character agent keys fail during config load.
- **Unit — agent validation**: test all-match, partial-mismatch (error lists only missing), complete-mismatch, empty-map (no error), empty-model-list-for-one-agent (error), case-insensitive match, and duplicate keys after case folding.
- **Unit — `format_agents_with_models`**: correct output for a typical map; sorted output (alphabetical by agent name for determinism); handles a single model and multiple models.
- **Unit — `build_leader_prompt` with `max_concurrent_steps`**: advisory note appears when `Some(n)`, absent when `None`.
- **Unit — leader resolution order**: mock all relevant sources (`--leader`, `defaultLeader`, `--model`, default agent/model fallback) and verify priority; verify config `defaultLeader` is used when flag is absent and is not overridden by `--model`.
- **Unit — `collect_config_rows` for `dynamicWorkflows`**: flattened dot-keyed rows appear; `agentsToModels` entries each get their own row.
- **Unit — `validate_and_coerce` for dynamic workflow fields**: `defaultLeader` validates syntax; `maxConcurrentSteps` accepts positive integers and rejects `0`.
- **Integration — workflow start with valid `dynamicWorkflows` config**: leader prompt contains the configured agent/model list and the `maxConcurrentSteps` advisory.
- **Integration — workflow start with mismatched agents**: fails before container spawn with the expected error message.
- **Integration — `config show` CLI output**: `dynamicWorkflows` fields appear, values are not truncated in CLI text output.
- **E2E — TUI config display**: long `agentsToModels` values are truncated in the cell with `…` and the full value appears in the detail line when the row is focused.

## Codebase Integration

- Follow the `WorkItemsConfig` precedent in `repo.rs` for struct definition, `serde` attributes, and `Default` derive.
- Use `#[serde(rename_all = "camelCase")]` on `DynamicWorkflowsConfig` to match the JSON key style used throughout the config file.
- `HashMap` imports: use `std::collections::HashMap`; it is already used in `exec_workflow.rs`.
- Keep `format_agents_with_models` alongside `format_available_agents` in `exec_workflow.rs`; they share the same call site and conceptual purpose.
- The leader prompt template at `src/assets/dynamic/leader-prompt.md` is embedded via `include_str!`; changes to it require no separate asset registration.
- For TUI cell truncation, follow the pattern used in other table renderers in `render.rs` — measure column width using `ratatui::layout` constraints and shorten the string before rendering.
- `collect_config_rows` returns `Vec<ConfigFieldRow>` in the current codebase; add the flattened dynamic workflow rows in the same command-layer row-building path as other repo-config rows, guarded by `if let Some(dw) = &repo_config.dynamic_workflows`.
- New command-layer errors in `exec_workflow.rs` should use the existing `CommandError` patterns in that file. New config-load validation errors in `repo.rs` should return `DataError::Other(...)` unless a dedicated semantic config error variant is added.
- Preserve the architecture boundary: JSON schema and schema-local validation live in Layer 0 (`src/data/config/repo.rs`); Dockerfile availability validation, leader resolution, and prompt construction live in Layer 2 (`src/command/commands/exec_workflow.rs`); truncation and detail-line rendering live in Layer 3 (`src/frontend/tui/render.rs`).
- Keep read-only/editability decisions in the command-layer row metadata so CLI, TUI, and API consumers see the same field capabilities.
- In `src/data/config/repo.rs`, do not synthesize a fake `serde_json::Error` for semantic validation failures.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** — update `docs/13-dynamic-workflows.md` to describe the `dynamicWorkflows` config section, its fields, leader resolution precedence, and the agent-validation behavior. Also update `docs/07-configuration.md` if it contains the repo config reference table.
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-workflow-config.md` covering the full `dynamicWorkflows` reference if no existing doc covers it)
- **Never create work-item-specific docs** (e.g., no "WI 0095 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.
