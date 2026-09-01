# Work Item: Enhancement

Title: dynamic workflow guidance + teardown failure output capture
Issue: issuelink

## Summary:
- **Feature A — Leader guidance**: Add a `guidance` field to `DynamicWorkflowsConfig` — an array of strings — where each string is a developer-supplied instruction that the leader agent must follow when generating a workflow file. The array is rendered as a bullet-point list injected into the leader prompt. Users can manage the list via direct `config.json` edits or through the TUI config modal using the same Ctrl-N / edit / remove semantics already established for `agentsToModels`.
- **Feature B — Teardown failure output capture**: When a teardown step fails and a remediation agent is launched, buffer the full stdout and stderr of the failed command and write it to a file so the agent has unambiguous, machine-readable context about what went wrong. If `context(workflow)` is active for the workflow, write the file into the existing workflow context directory (already mounted in the agent container). If `context(workflow)` is not active, create an ephemeral directory under `~/.awman/context/` for this workflow invocation, write the file there, and mount that directory in the remediation agent container. In both cases, add a system-prompt hint directing the agent to read the file.

## User Stories

### User Story 1:
As a: developer using dynamic workflows

I want to: define project-specific constraints that the leader agent must always follow when building a workflow (e.g. "never spawn more than two agents in parallel", "always include a validation step after each implementation step")

So I can: enforce consistent, project-aware workflow structure without having to repeat those constraints in every work item description.

### User Story 2:
As a: developer using dynamic workflows

I want to: add, edit, and remove guidance entries from the awman TUI config modal using Ctrl-N to append a new entry and inline editing to modify or clear existing entries

So I can: manage workflow guidance interactively without manually editing `config.json`.

### User Story 3:
As a: developer using dynamic workflows

I want to: omit the `guidance` field entirely from `config.json` when I have no project-level constraints

So I can: adopt this feature incrementally — existing configs are unaffected and the leader prompt is unchanged when guidance is absent.

### User Story 4:
As a: developer whose dynamic workflow teardown step has failed

I want to: have the remediation agent automatically receive the full stdout and stderr of the failed command as a readable file in its container

So I can: write remediation prompts that say "read the failure output and fix the root cause" rather than having to anticipate and describe every possible failure mode in advance.


## Implementation Details:

### Feature A — Leader guidance

### 1. Config struct (`src/data/config/repo.rs`)

- Add `guidance: Option<Vec<String>>` to `DynamicWorkflowsConfig` (lines 64-76), with `#[serde(skip_serializing_if = "Option::is_none")]`.
- Add validation in `DynamicWorkflowsConfig::validate()` (lines 78-116): each string must be non-empty and under a reasonable length cap (e.g. 1 000 chars); the array itself must not exceed a reasonable item cap (e.g. 50 entries). Return a structured config error on violation, consistent with existing validators.

### 2. Leader prompt injection (`src/data/dynamic_workflow_assets.rs` + `src/assets/dynamic/leader-prompt.md`)

- In `build_leader_prompt()` (lines 32-50), add a `guidance` parameter (`Option<&[String]>`).
- When `guidance` is `Some` and non-empty, build a bullet-point block:
  ```
  ## Developer Guidance
  You MUST follow these project-specific instructions when building the workflow:
  - <entry 1>
  - <entry 2>
  ```
  Inject this block into the leader prompt template via a new `{{developer_guidance}}` placeholder, placed after the concurrency note and before the task section so the leader sees it early.
- When `guidance` is `None` or empty, substitute an empty string for `{{developer_guidance}}` so the template renders cleanly.
- Update the call site in `exec_workflow.rs` (around line 2000) to pass `config.dynamic_workflows.as_ref().and_then(|dw| dw.guidance.as_deref())`.

### 3. TUI config modal — display (`src/frontend/tui/render.rs`, `src/command/commands/config.rs`)

- In the config hints/format logic (`config.rs` lines 419-444), add a hint for the `dynamicWorkflows.guidance` field: `"press Ctrl+N to add a guidance entry; edit per-entry rows inline; save an empty value to remove"`.
- Render each guidance entry as its own `ConfigShowRow` with the field key `dynamicWorkflows.guidance.<index>` (or a sequential display label like `guidance[0]`) and the string value in the value column. Follow the same row-generation pattern used for `agentsToModels` entries.

### 4. TUI config modal — add new entries (Ctrl-N)

- In `ConfigShowState` / `NewMapEntryPhase` (`src/frontend/tui/dialogs/mod.rs` lines 220-252), the `agentsToModels` flow uses a two-phase key→value entry. The `guidance` flow is simpler — only a value is needed (the index is assigned automatically).
- Introduce a new `NewMapEntryPhase` variant (e.g. `GuidanceEntry`) for when the selected row group is `dynamicWorkflows.guidance`.
- In the Ctrl-N handler (`src/frontend/tui/mod.rs` lines 654-667), detect if the cursor is on a `guidance` section row and begin `GuidanceEntry` phase — open the text editor for the string value directly.
- On Enter, emit the config response: `"dynamicWorkflows.guidance.<next_index>\t<value>\trepo"` where `<next_index>` is `current_count` (appending). The config set handler appends to the JSON array rather than keying into a map.
- Update Ctrl-N help text in the browse-mode hint line (`render.rs` lines 2121-2149) to read `"Ctrl+N=add entry"` when on a guidance row (or a combined hint when both sections are relevant).

### 5. TUI config modal — edit and remove existing entries

- Inline editing (`mod.rs` `config_show_begin_edit()` lines 1381-1446) already works for any row with a mutable value column. Guidance rows set `edit_column` to the value column; on save, the response is `"dynamicWorkflows.guidance.<index>\t<new_value>\trepo"`.
- Saving an empty string removes the entry. The config layer should coerce an empty value to remove that array element (extend the null-coercion logic at `config.rs` line 149 to handle array-indexed keys).
- After removal, remaining entries are re-indexed; the TUI refreshes the row list on the next config reload.

### 6. Config set / remove plumbing (`src/command/commands/config.rs`)

- The existing `set_config_field()` / `remove_config_field()` functions work with dot-path keys. `dynamicWorkflows.guidance` is an array, so dot-path navigation needs to support numeric indices (e.g. `guidance.0`, `guidance.1`) for set and remove operations, or a dedicated append path (e.g. `guidance.-`) for new entries.
- Evaluate whether the existing JSON-patch helpers already support this or need a small extension. If extension is needed, add it narrowly — only for array-of-string fields — rather than a general array-mutation facility.

---

### Feature B — Teardown failure output capture

### 7. Capture full stdout and stderr (`src/engine/workflow/mod.rs`)

- In `run_shell_phase_step()` (lines 2313-2319), `container.exec_streaming()` already returns an `ExecOutput` struct with both `stdout: String` and `stderr: String` fields (`src/engine/agent_runtime/background.rs:12-18`). Currently only `stderr` is forwarded to `set_phase_step_failed()` (line 2329); `stdout` is discarded.
- Extend the failure path to retain both: pass the full `ExecOutput` (or a `(stdout, stderr)` pair) from `run_shell_phase_step()` back to `run_single_teardown_step()` (lines 2437-2471) and then to `run_teardown_remediation()` (lines 2526-2575). Add a new field or parameter — keep it narrow, not a general `ExecOutput` stored in `PhaseStepStatus` — since `PhaseStepStatus::Failed` only needs to surface what the TUI already shows (just `error: String` / stderr for display); the full output is only needed for the remediation file.
- Concretely: give `run_teardown_remediation()` a `stdout: &str` and `stderr: &str` parameter pair, and thread them through from the call site in the teardown execution loop (lines 2265-2276).

### 8. Resolve the output file path and write the file (`src/engine/workflow/mod.rs`, `src/data/fs/context_dirs.rs`)

- In `launch_on_failure_agent()` (lines 2577-2658), before constructing the synthetic `__on_failure__` step, determine the output file location:
  - Inspect the workflow's active context overlays to check whether a `ContextScope::Workflow` overlay is present (its host path is already resolved and the directory already exists).
  - **If `context(workflow)` is active**: use the overlay's existing `host_path` (`~/.awman/context/workflows/{uuid}/`) directly. No new mount is needed — the directory is already mounted at `/awman/context/workflow` in the agent container.
  - **If `context(workflow)` is not active**: call `ContextDirResolver::workflow_dir(invocation_uuid)` (`src/data/fs/context_dirs.rs:54-60`) to derive `~/.awman/context/workflows/{uuid}/` and create it with `std::fs::create_dir_all`. This directory is ephemeral — it is not part of any configured overlay — so add it as a one-off read-only volume mount to the remediation agent's container at a dedicated container path (e.g. `/awman/remediation/`).
- Sanitize the failed step's name to a safe filename component (replace non-alphanumeric characters with `-`, truncate to 64 chars). Write the failure output file to `{host_path}/teardown-failure-{sanitized_step_name}.txt` with the following format:
  ```
  === FAILED COMMAND: <step name> ===

  --- STDOUT ---
  <stdout content, or "(empty)" if blank>

  --- STDERR ---
  <stderr content, or "(empty)" if blank>
  ```
- Write the file before launching the agent so it is present when the agent starts.

### 9. Inject the system-prompt hint

- The remediation agent's task prompt already comes from `RemediationConfig.prompt` (the `prompt_template` on the synthetic `__on_failure__` step). Prepend a fixed preamble to that prompt at launch time — do not require users to mention the file in their config:
  ```
  The full output (stdout and stderr) of the failed teardown step "<step name>" has been
  written to <container_path>/teardown-failure-<sanitized_step_name>.txt.
  Read that file first to understand the failure before attempting a fix.

  ---
  ```
  Where `<container_path>` is `/awman/context/workflow` when `context(workflow)` is active, or `/awman/remediation/` when the ephemeral mount is used.
- This prepend happens in `launch_on_failure_agent()` just before building the synthetic `WorkflowStep`, so the user's own prompt follows naturally after the separator.


## Edge Case Considerations:

### Feature A — Leader guidance

- **Empty array**: `guidance: []` in config.json should be treated the same as omitting the field — no guidance block is injected into the prompt and no validation errors are raised.
- **Very long strings**: Each guidance string is injected verbatim into the LLM prompt. Enforce a per-entry length cap (e.g. 1 000 chars) during `validate()` to prevent accidentally bloating the prompt or hitting context limits.
- **Many entries**: Cap the array at a reasonable maximum (e.g. 50 entries) for the same reason.
- **Re-indexing after removal**: Deleting a middle entry shifts indices of all subsequent entries. The TUI must reload the config row list after any removal to reflect the new indices; stale index references must not be written back.
- **Concurrent config edits**: If the user edits `config.json` externally while the modal is open, the next save may overwrite the file's changes. This is pre-existing behaviour; document it in the hint text if not already noted.
- **Whitespace-only strings**: A guidance entry containing only whitespace is functionally empty. Trim strings during validation and reject (or silently skip) whitespace-only entries.
- **Special characters in guidance strings**: Entries may contain markdown, backticks, or newlines. The prompt renderer should escape or strip literal newlines within a single entry (a guidance item is one bullet point) to avoid breaking the bullet-list structure.
- **No `dynamicWorkflows` object present**: `RepoConfig.dynamic_workflows` is `Option<DynamicWorkflowsConfig>`. The config modal and prompt builder must handle `None` gracefully — no crash, no guidance block.

### Feature B — Teardown failure output capture

- **stdout is empty**: Many shell commands write nothing to stdout on failure. The file should still be created with the `(empty)` placeholder so the agent is not confused by a missing section.
- **Both stdout and stderr are empty**: Create the file anyway — its presence is the hint to the agent that a failure occurred. The empty-output case is itself informative.
- **Very large output**: A runaway command could produce megabytes of output. Cap the buffered content before writing (e.g. last 100 KB of stdout and last 100 KB of stderr, with a truncation notice) to avoid filling disk or bloating the agent context window when the agent reads the file.
- **Step name contains path separators or shell metacharacters**: Sanitize thoroughly before using the name in a filename — strip or replace `/`, `\`, `..`, spaces, and other unsafe characters.
- **`ContextDirResolver::workflow_dir()` fails**: If the directory cannot be created (e.g. permissions error on `~/.awman/`), log a warning and launch the remediation agent without the file — do not abort the remediation attempt. The agent will work with its configured prompt only.
- **File write fails after directory creation**: Same as above — warn and continue.
- **Remediation agent is retried (multiple attempts)**: `run_teardown_remediation()` loops up to `max_attempts` times. Each attempt re-runs the teardown command, which may produce new output on failure. Overwrite the file on each new failure so the agent always sees the most recent output.
- **`context(workflow)` is mounted read-only (`context(workflow:ro)`)**: Do not write to a read-only overlay. Detect the permission on the overlay; if read-only, fall back to the ephemeral-mount path instead.
- **Cleanup**: The ephemeral directory created under `~/.awman/context/workflows/{uuid}/` follows the same lifecycle as the workflow context directory — it is subject to the existing clean command logic (`src/command/commands/clean.rs:307-368`). No separate cleanup mechanism is needed.


## Test Considerations:

### Feature A — Leader guidance

- **Unit — config deserialization**: Parse a `config.json` containing `dynamicWorkflows.guidance` with valid entries, an empty array, and a missing field; assert correct struct values in each case.
- **Unit — config validation**: Assert errors for entries exceeding the length cap, entries that are whitespace-only, and arrays exceeding the count cap. Assert no error for valid arrays.
- **Unit — prompt builder**: Call `build_leader_prompt()` with a non-empty guidance slice and assert the output contains the `## Developer Guidance` section with the correct bullet points. Call it with `None` / empty and assert the section is absent and no stray placeholder token appears.
- **Unit — config set (array append)**: Invoke the config set path with `dynamicWorkflows.guidance` and assert the JSON array grows by one element with the correct value.
- **Unit — config remove (by index)**: Invoke remove with `dynamicWorkflows.guidance.1` on a three-element array and assert the remaining array is correct and indices are compact.
- **Integration — Ctrl-N add flow**: Simulate the Ctrl-N key event on a guidance section row in the config modal state machine, complete the entry phase, and assert the emitted config response string is well-formed.
- **Integration — inline edit and empty-string remove**: Simulate editing an existing guidance row to a new value and assert the correct response string. Simulate editing to an empty string and assert the entry is removed.
- **Integration — prompt end-to-end**: Load a repo config with two guidance entries, run `build_leader_prompt()` through the full exec_workflow path (or a unit-level approximation), and assert the leader prompt text contains both entries as bullets.

### Feature B — Teardown failure output capture

- **Unit — output file content**: Call the file-writing helper with known stdout/stderr strings and assert the file contents match the expected format (headers, separators, content). Test the empty-string case (expect `(empty)` placeholder) and the truncation case (oversized input → truncated with notice).
- **Unit — filename sanitization**: Pass step names containing `/`, spaces, `..`, and special characters; assert the resulting filename is safe and within the length limit.
- **Unit — context overlay detection**: Given a list of `ContextOverlay` values, assert that the helper correctly identifies whether a `ContextScope::Workflow` overlay is present and whether it is writable.
- **Unit — ephemeral dir path**: Call `ContextDirResolver::workflow_dir(uuid)` and assert the returned path is under `~/.awman/context/workflows/`.
- **Unit — prompt preamble**: Given a step name, container path, and whether the file was written successfully, assert the preamble string is well-formed and references the correct container path.
- **Integration — teardown failure with `context(workflow)` active**: Simulate a teardown step failure in a workflow that has a `context(workflow)` overlay; assert the output file is written to the overlay's host path and that the remediation agent's effective prompt contains the file hint referencing `/awman/context/workflow/teardown-failure-{step}.txt`.
- **Integration — teardown failure without `context(workflow)`**: Same simulation with no workflow context overlay; assert the ephemeral directory is created under `~/.awman/context/workflows/{uuid}/`, the file is written there, and the prompt hint references `/awman/remediation/teardown-failure-{step}.txt`. Assert the ephemeral directory is mounted as a volume in the remediation agent.
- **Integration — retry overwrites file**: Simulate two consecutive teardown failures during a multi-attempt remediation; assert the output file reflects the second failure's output, not the first.
- **Integration — write failure degrades gracefully**: Mock a filesystem write error; assert the remediation agent is still launched and its prompt does not contain a broken file reference.


## Codebase Integration:

### Feature A — Leader guidance

- Follow established conventions, best practices, testing, and architecture patterns from the project's `aspec/`.
- The new `guidance` field on `DynamicWorkflowsConfig` follows the same `Option<...>` / `skip_serializing_if` / `camelCase` pattern as the existing `agents_to_models`, `max_concurrent_steps`, and `default_leader` fields (`src/data/config/repo.rs:64-76`).
- Validation should be added to `DynamicWorkflowsConfig::validate()` (`repo.rs:85`) in the same style as `validate_dynamic_agent_key()` and `validate_default_leader()` below it (`repo.rs:122-159`).
- The leader prompt template substitution pattern is in `build_leader_prompt()` (`src/data/dynamic_workflow_assets.rs:32-50`). Add the new `{{developer_guidance}}` placeholder and pass it through from the call site in `exec_workflow.rs` around line 2000.
- The `NewMapEntryPhase` enum and `ConfigShowState` struct live in `src/frontend/tui/dialogs/mod.rs:220-252`. Add a new variant for the simpler single-phase guidance entry flow; avoid duplicating the two-phase key→value logic unnecessarily.
- The Ctrl-N dispatch lives in `src/frontend/tui/mod.rs:654-667`. Check which config section the cursor is on before deciding which `NewMapEntryPhase` to start.
- Config hint strings for fields are registered in `src/command/commands/config.rs:419-444`; add a hint for `dynamicWorkflows.guidance` matching the format of the existing hints.
- The empty-value-to-removal coercion is at `src/command/commands/config.rs:149`. Extend it to handle array-indexed keys (`guidance.<n>`) so the existing remove path can be reused by the TUI without a separate delete action.
- All new code paths must have unit tests; new TUI state transitions must have integration tests. Follow the test organisation already present in each module.

### Feature B — Teardown failure output capture

- The teardown failure and remediation flow lives in `src/engine/workflow/mod.rs`. The key functions are `run_shell_phase_step()` (lines 2302-2334), `run_single_teardown_step()` (lines 2437-2471), `run_teardown_remediation()` (lines 2526-2575), and `launch_on_failure_agent()` (lines 2577-2658). Confine all new logic to these functions — do not add failure-capture behaviour to the setup phase or to regular workflow step execution.
- `ExecOutput` (`src/engine/agent_runtime/background.rs:12-18`) already carries `stdout`, `stderr`, and `exit_code`. Thread the full struct (or just the two string fields) from `run_shell_phase_step()` through the call chain to `launch_on_failure_agent()` rather than re-capturing output separately.
- The `ContextDirResolver` is in `src/data/fs/context_dirs.rs`. Use `workflow_dir(uuid)` (line 54) to derive the host path in both branches (with and without an active overlay). Do not hard-code the `~/.awman/` prefix; always go through the resolver so the path respects any configured `awman_home` override.
- Overlay detection: the active `ContextOverlay` list is available at the point `launch_on_failure_agent()` is called (it is part of the resolved workflow context). Check for a `ContextScope::Workflow` overlay with `OverlayPermission::ReadWrite` using the same overlay types in `src/engine/overlay/mod.rs:43-50`.
- For the ephemeral mount, follow the same volume-mount construction used for regular context overlays (`src/engine/overlay/mod.rs:207-216`) rather than inventing a new mounting mechanism. The only difference is that the overlay is not declared in the workflow file — it is injected at runtime by `launch_on_failure_agent()`.
- The system-prompt hint is prepended to `RemediationConfig.prompt` in `launch_on_failure_agent()`. Do not modify the `RemediationConfig` struct or the workflow TOML schema — the preamble is an implementation detail of the launch function, invisible to the user's workflow file.
- Security: the ephemeral directory path must be validated through `validate_context_path()` (`src/data/fs/context_dirs.rs:151-163`) before use, consistent with all other context directory operations.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs**: update `docs/15-parallel-workflows.md` (or whichever doc covers dynamic workflows) to describe: (1) the `guidance` field — what it does, an example `config.json` snippet, and how to manage entries in the TUI config modal; (2) teardown failure output capture — what the remediation file contains, where it is written, and how to reference it in a `on_failure.prompt`.
- **Never create work-item-specific docs** (e.g., no "WI 0099 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.
