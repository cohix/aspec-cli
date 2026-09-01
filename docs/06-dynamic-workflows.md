# Dynamic Workflows

Dynamic workflows let you run `exec workflow --dynamic` without writing a workflow file yourself. Instead of authoring a `.toml` or `.yaml` file by hand, a "leader" agent is launched inside a container to design a purpose-built `workflow.toml` tailored to your work item. Once the leader finishes, awman validates its output and immediately executes the generated workflow — all in one command.

---

## When to use

Dynamic workflows are useful when:

- You have a work item spec and want awman to figure out the right set of steps without manual workflow authoring
- You want a custom workflow per work item rather than reusing a generic template
- You're running in fully autonomous mode and want end-to-end execution from a single command

Dynamic mode always implies `--yolo`, `--worktree`, and a `context(workflow)` overlay. This means all agent work happens in an isolated Git worktree, and the leader and workflow steps share a context directory for coordination.

---

## Quick start

```sh
# Let awman design and run a workflow for work item 42
awman exec workflow --dynamic --work-item 42

# Same, but use a more capable leader agent
awman exec workflow --dynamic --work-item 42 --leader claude::claude-opus-4-8
```

`--work-item` is required with `--dynamic`. Without it, awman cannot provide the leader agent with the work item it is designing the workflow for.

---

## How it works

When you run `exec workflow --dynamic`, awman follows these steps:

1. **Validate flags** — checks for conflicts before doing any work (see [Flag rules](#flag-rules))
2. **Read the work item** — loads the work item file; fails immediately if it cannot be found or read
3. **Set up the worktree** — creates an isolated Git worktree (the same path as `--yolo --worktree`); all subsequent agent work runs inside it
4. **Seed the context directory** — writes `example-workflow.toml` and `workflow-usage.md` into the `context(workflow)` directory so the leader has reference material
5. **Launch the leader agent** — starts a container with a pre-built prompt describing the work item, available agents, and path conventions; the agent's job is to write a `workflow.toml` to the context directory
6. **Wait for the leader to finish** — stuck detection runs normally; after 30 seconds of silence the 60-second yolo countdown starts; when it expires (or you press `[n]`), the container is killed and awman reads the file
7. **Validate the workflow** — checks that `workflow.toml` is valid TOML, references only agents that exist in the project, and has all required fields; if validation fails, a repair agent is launched (see [Repair loop](#repair-loop))
8. **Build missing images** — for any agent referenced in the generated workflow that has a `Dockerfile.<agent>` but no built image, awman builds the image automatically before starting the workflow
9. **Execute the workflow** — runs the generated workflow exactly as if you had written it yourself and passed it to `exec workflow --yolo --work-item 42`

The leader agent cannot see or modify source files beyond the context directory — the worktree is mounted read-only when the runtime supports it, and any source-file modification by the leader aborts the dynamic run before the generated workflow is executed.

---

## Choosing the leader agent

By default, the leader uses whatever agent is configured as the project default (from `.awman/config.json` or `~/.awman/config.json`), unless `dynamicWorkflows.defaultLeader` is set (see [Configuring dynamic workflows](#configuring-dynamic-workflows) below). You can override this per invocation in two ways:

**`--leader agent::model`** — specify both the agent container and model for the leader:

```sh
awman exec workflow --dynamic --work-item 42 --leader claude::claude-opus-4-8
```

**`--model`** — use the default agent, but with a different model:

```sh
awman exec workflow --dynamic --work-item 42 --model claude-opus-4-8
```

When both `--leader` and `--model` are passed, `--leader` governs the leader agent entirely; `--model` still applies as the session-level default model for steps in the generated workflow.

The `--leader` value must be in `agent::model` format — exactly two components separated by `::`. Examples:

```
claude::claude-opus-4-8       # valid
claude::claude-sonnet-4-6     # valid
claude                        # invalid — missing ::model
::claude-opus-4-8             # invalid — missing agent
claude::opus::extra           # invalid — too many components
```

### Leader resolution order

When more than one source could determine the leader, awman applies this precedence:

1. `--leader agent::model` on the command line
2. `dynamicWorkflows.defaultLeader` in `.awman/config.json`
3. `--model`, applied to the project's default agent
4. No override — the project's default agent and model

`defaultLeader` governs both the leader's agent and model. A separate `--model` flag continues to set the session-level default model for the *generated workflow's* steps, but it does not override the model half of `defaultLeader`.

---

## Configuring dynamic workflows

Add a `dynamicWorkflows` section to `.awman/config.json` to pin which agents and models the leader may schedule, cap how many steps run concurrently, and set a repo-wide default leader — all shared with your team via version control instead of passed as flags on every run.

```json
{
  "dynamicWorkflows": {
    "agentsToModels": {
      "claude": ["claude-opus-4-8", "claude-sonnet-4-6"],
      "codex": ["codex-mini-latest"]
    },
    "maxConcurrentSteps": 3,
    "defaultLeader": "claude::claude-opus-4-8",
    "guidance": [
      "Never spawn more than two agents in parallel.",
      "Always include a validation step after each implementation step."
    ]
  }
}
```

All four fields are optional and independent.

### `agentsToModels`

Restricts the leader to a known, approved set of agents and models instead of everything discovered from `.awman/Dockerfile.<agent>` files. When set and non-empty, this list — not Dockerfile discovery — is what the leader prompt's "Available Agents" section shows, so the leader only schedules steps against agents and models your team has vetted.

- Every key must name an agent that has a `.awman/Dockerfile.<agent>` in the project. If any configured agent has no matching Dockerfile, the workflow fails immediately, before any container is spawned:

  ```
  Error: dynamicWorkflows.agentsToModels references agents that have no Dockerfile in this repo: [foo, bar].
  Available agents: [claude, codex, gemini].
  Add a .awman/Dockerfile.<agent> for each missing agent, or remove it from agentsToModels.
  ```

- Agent name matching is case-insensitive as a compatibility aid (`"Claude"` matches a `claude` Dockerfile), but the workflow always uses the lowercase agent name, and a warning is shown when a match only succeeds after case folding. Two configured keys that fold to the same agent (e.g. `"Claude"` and `"claude"`) is a configuration error, since awman won't guess which model list should win.
- An empty map (`{}`) is treated the same as omitting the field — the leader falls back to Dockerfile discovery.
- Each agent's model list must be non-empty, and no model name may be empty or whitespace-only; both are rejected when the config is loaded.

### `maxConcurrentSteps`

An advisory cap the leader is told to plan around:

```
Note: the repository configuration advises a maximum of 3 concurrent steps. Plan your workflow accordingly.
```

This is advisory only — awman does not enforce it in the workflow scheduler. It is a hint the leader agent uses when deciding how much of the workflow to run in parallel via `depends_on`. Must be `>= 1` if set; `0` is rejected when the config is loaded, since it would deadlock any workflow.

Don't confuse this with [`maxConcurrentAgents`](07-configuration.md#reference), a differently-named, differently-scoped setting: `maxConcurrentAgents` is the cap the engine actually enforces at run time, for every workflow (dynamic or not), regardless of what the leader planned. See [Parallel workflows](05-workflows.md#parallel-workflows).

### `defaultLeader`

Sets the repo-wide default leader agent and model, in the same `agent::model` format as `--leader` (see [Leader resolution order](#leader-resolution-order) above). Rejected at config-load time if the format is invalid, if either component is empty or has surrounding whitespace, or if the agent component isn't a valid agent name.

### `guidance`

A list of project-specific instructions the leader agent must always follow when it designs a workflow — constraints you'd otherwise have to repeat in every work item description (e.g. "never spawn more than two agents in parallel", "always include a validation step after each implementation step"). Each entry becomes one bullet in a "Developer Guidance" block injected into the leader prompt, right after the concurrency note:

```
## Developer Guidance
You MUST follow these project-specific instructions when building the workflow:
- Never spawn more than two agents in parallel.
- Always include a validation step after each implementation step.
```

- Omitting the field, or leaving it an empty array (`[]`), skips the block entirely — the leader prompt is unchanged.
- Each entry is capped at 1,000 characters and the array at 50 entries, since every entry is injected verbatim into the leader's prompt. Both caps are enforced when the config is loaded.
- Whitespace-only entries are rejected when the config is loaded.
- Each entry is rendered as exactly one bullet — any newlines inside an entry are flattened to spaces so a multi-line entry can't break the bullet list.

### Managing this config

`dynamicWorkflows.defaultLeader` and `dynamicWorkflows.maxConcurrentSteps` are editable with `awman config set` / the TUI config dialog like any other repo field:

```sh
awman config set dynamicWorkflows.defaultLeader claude::claude-opus-4-8
awman config set dynamicWorkflows.maxConcurrentSteps 3
```

`agentsToModels` is managed one agent at a time. On the command line, set an agent's comma-separated model list (or clear it with an empty value):

```sh
awman config set dynamicWorkflows.agentsToModels.claude "claude-opus-4-8, claude-sonnet-4-6"
awman config set dynamicWorkflows.agentsToModels.claude ""   # remove the mapping
```

In `awman config show` and the TUI config dialog, the map appears as a summary row plus one editable row per agent (`dynamicWorkflows.agentsToModels.<agentName>`); in the TUI, **Ctrl+N** adds a new mapping and per-agent rows are edited inline — see [Using the TUI](02-using-the-tui.md#agentmodel-mappings-dynamicworkflowsagentstomodels). See [Configuration](07-configuration.md#reference) for the full field reference.

`guidance` is managed one entry at a time, addressed by index on the command line:

```sh
awman config set dynamicWorkflows.guidance.0 "Never spawn more than two agents in parallel."
awman config set dynamicWorkflows.guidance.1 "Always include a validation step after each implementation step."
awman config set dynamicWorkflows.guidance.0 ""   # remove entry 0; remaining entries shift down and re-index
```

In `awman config show` and the TUI config dialog, the list appears as a summary row (`dynamicWorkflows.guidance`, showing the entry count or `(none)`) plus one editable row per entry (`dynamicWorkflows.guidance.<index>`). In the TUI:

- **Ctrl+N** while a guidance row is selected appends a new entry — you only type the instruction text, since the index is assigned automatically.
- Existing entries are edited inline like any other field.
- Saving an **empty** value on an entry row removes it; remaining entries re-index automatically.

See [Using the TUI](02-using-the-tui.md#developer-guidance-list-dynamicworkflowsguidance) for the walkthrough and [Configuration](07-configuration.md#reference) for the full field reference.

---

## The leader step

The leader agent runs like a regular workflow step:

- **Stuck detection** fires after 30 seconds of inactivity
- **60-second yolo countdown** starts automatically (dynamic mode always enforces `--yolo`)
- **Auto-advance** kills the container when the countdown expires and reads the generated file
- **`[n] now`** (CLI) or `→` (WCB) advances immediately without waiting for the countdown
- **Ctrl+W** opens the Workflow Control Board at any time

In the Workflow Control Board, the right-arrow action is labelled **"Start dynamic workflow"** instead of the usual "Next: new container", reflecting that advancing past the leader step starts the generated workflow.

In the TUI, the moment the leader container is killed (or exits on its own), its container window closes and the [summary bar](02-using-the-tui.md#when-the-container-exits) takes its place, so the execution window stays visible while awman validates the generated file and launches the workflow.

Other WCB actions work identically to a regular step:

| Action | Effect |
|--------|--------|
| **↑ Restart** | Kill the leader container, delete the current `workflow.toml`, and re-launch a fresh leader with the same prompt |
| **→ Start dynamic workflow** | Kill the leader container and proceed to validate and execute the generated file |
| **Ctrl+C / [a] Abort** | Kill the leader container and abort the entire dynamic run — no workflow is executed |
| **[p] Pause** | Kill the leader container and pause; you can resume later |
| **Esc Dismiss** | Close the WCB without affecting the running leader |

---

## Repair loop

If the leader's `workflow.toml` is missing, fails to parse, or references agents the project does not have, awman does not abort immediately. Instead, it launches a **repair agent** — the same leader container and model — with a prompt that includes the exact validation error and the workflow format documentation.

The repair loop runs up to **3 times** before giving up:

```
Attempt 1: leader writes workflow.toml → validate → error found
Attempt 1 repair: repair agent fixes the file → re-validate → passes → proceed
```

```
Attempt 1: leader writes nothing → validate → missing-file error
Attempt 1 repair: repair agent writes a file → validate → bad agent name
Attempt 2 repair: repair agent fixes agent name → validate → passes → proceed
```

If all 3 repair attempts fail, awman surfaces the final error and the path to the last generated file so you can inspect it manually:

```
leader agent failed to produce a valid workflow.toml after 3 repair attempts
last error: workflow.toml references agents with no Dockerfile in the project:
  - "gemini" (expected .awman/Dockerfile.gemini)
  Available agents: claude, maki
file is at: /home/user/.awman/context/workflow/abc123/workflow.toml
```

Each repair agent runs through the same stuck detection → yolo countdown → auto-advance pipeline as the original leader.

---

## Flag rules

| Rule | Error |
|------|-------|
| `--dynamic` with a positional workflow path | `cannot specify a workflow file path with --dynamic; the path is created automatically` |
| `--dynamic` without `--work-item` | `--dynamic requires --work-item` |
| `--leader` without `--dynamic` | `--leader is only valid with --dynamic` |
| `--dynamic --plan` | `--dynamic cannot be used with --plan because dynamic mode enforces --yolo` |
| Malformed `--leader` value | `invalid --leader value: expected agent::model (e.g. claude::claude-opus-4-8)` |

All flag errors are surfaced before any container work begins.

---

## Flags

| Flag | Description |
|------|-------------|
| `--dynamic` | Enable dynamic mode — the leader agent designs the workflow file |
| `--work-item <N>` | Required with `--dynamic`; the work item number to pass to the leader |
| `--leader <agent::model>` | Override the agent container and model used for the leader step |
| `--model <NAME>` | Default model for the generated workflow's steps; also used as the leader model if `--leader` is not set |

Dynamic mode always enforces `--yolo`, `--worktree`, and `--overlay context(workflow)`. Passing any of these explicitly has no effect (they are already active).

---

## Edge cases

| Situation | Behaviour |
|-----------|-----------|
| Work item file not found or unreadable | Hard error before any container work; the file is required for the leader prompt |
| Leader writes nothing (no `workflow.toml`) | Repair loop: missing-file error passed to repair agent |
| Leader writes invalid TOML | Repair loop: parse error passed to repair agent |
| Leader references an unknown agent | Repair loop: agent validation error lists unknown agents and available ones |
| Repair agent exhausts 3 attempts | Final error surfaced with the file path for manual inspection |
| Repair agent deletes the file instead of fixing it | Treated as missing-file error on next validation pass; repair loop continues |
| Leader references a valid agent with no built image | Image is built automatically before the workflow starts (not a repair-loop error) |
| Agent image build fails | Hard error with build output; workflow does not start; repair loop is not entered |
| Leader modifies source files outside the context directory | Dynamic run aborts before executing the generated workflow; changed paths are reported |
| User restarts the leader via WCB | Current leader container killed, `workflow.toml` deleted, fresh leader launched |
| User aborts during the leader yolo countdown | Leader container killed, entire dynamic run aborts — no workflow executes |
| User pauses during the leader step | Leader container killed; resume semantics follow the standard workflow pause/resume path |
| Leader becomes unstuck during yolo countdown | Countdown cancelled; leader continues running normally |
| `--leader` and `--model` both set | `--leader` controls the leader's agent and model; `--model` applies to the generated workflow's steps |
| Context directory already contains a `workflow.toml` from a previous run | Deleted before the leader launches; stale files are never executed |
| `dynamicWorkflows.agentsToModels` references an agent with no matching Dockerfile | Hard error before any container is spawned; lists the missing agents and the available ones |
| `dynamicWorkflows.agentsToModels` is `{}` (empty map) | Treated as if unset; falls back to Dockerfile discovery |
| `dynamicWorkflows.agentsToModels` key matches a Dockerfile only after case folding | Workflow proceeds using the lowercase agent name; a warning is shown |
| Two `dynamicWorkflows.agentsToModels` keys fold to the same agent (e.g. `"Claude"` and `"claude"`) | Error at workflow start, before any container is spawned; ambiguous model lists are never merged |
| `dynamicWorkflows.maxConcurrentSteps` is `0` | Rejected when the config is loaded — awman never starts with this value |
| `dynamicWorkflows.defaultLeader` is malformed | Rejected when the config is loaded, before any UI or workflow starts |
| `--leader` and `dynamicWorkflows.defaultLeader` both set | `--leader` wins |
| `--model` and `dynamicWorkflows.defaultLeader` both set, no `--leader` | `defaultLeader` controls the leader's model; `--model` still applies to the generated workflow's steps |
| `dynamicWorkflows.guidance` is `[]` (empty array) | Treated as if unset; no guidance block is injected into the leader prompt |
| A `dynamicWorkflows.guidance` entry exceeds 1,000 characters | Rejected when the config is loaded |
| `dynamicWorkflows.guidance` has more than 50 entries | Rejected when the config is loaded |
| A `dynamicWorkflows.guidance` entry is empty or whitespace-only | Rejected when the config is loaded |

---

[← Workflows](05-workflows.md) · [Next: Configuration →](07-configuration.md)
