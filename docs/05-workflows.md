# Workflows

A workflow breaks a large implementation task into discrete phases — for example: plan → implement → review → docs. Each phase runs as its own agent session. You review the output between phases and decide whether to advance, retry, or redirect.

Workflows are files you write and commit to your repo — in TOML or YAML format. awman parses them into an execution plan and runs them inside Docker containers, pausing between steps for your input. Optional `setup` and `teardown` sections allow you to prepare the environment before the first step (e.g., installing dependencies, checking out branches) and perform post-workflow actions (e.g., running tests, creating pull requests).

**Migration from Markdown:** Markdown workflow files (`.md`) are no longer supported as of this release. If you have existing Markdown workflows, convert them to TOML or YAML using the format examples below. The conversion is straightforward — all step definitions map directly to TOML/YAML syntax.

---

## When to use workflows

Workflows are useful when:

- The task is complex enough that you want the agent to plan before coding
- You want multiple review checkpoints (e.g. review the plan before implementation starts)
- You want documentation generated as a separate step after implementation
- You're running in `--yolo` mode and want structured auto-advancement instead of a single long session

---

## Quick start

```sh
# Run a workflow file
awman exec workflow aspec/workflows/implement-hard.toml

# Run a workflow and associate a work item for template variable substitution
awman exec workflow aspec/workflows/implement-hard.toml --work-item 0027

# Run a workflow against a GitHub issue
awman exec workflow aspec/workflows/implement-hard.toml --issue 84

# Run a workflow without a work item
awman exec workflow aspec/workflows/dependency-upgrade.toml

# Let awman design and run the workflow automatically for a work item
awman exec workflow --dynamic --work-item 42
```

Use `exec workflow` to run any workflow file. If you don't want to write a workflow file yourself, see [Dynamic Workflows](06-dynamic-workflows.md) — `--dynamic` launches a leader agent that designs a purpose-built workflow for your work item and then executes it automatically. The work item is optional — associate one with `--work-item` if you want template variable substitution, or with `--issue` to use a GitHub issue directly. See [API mode](09-api-and-remote-mode.md) for usage in CI and scripting contexts. For more on GitHub integration, see [GitHub Integration](10-github-integration.md).

The TUI shows the **Workflow Overview** between the execution window and the command box, with one coloured box per step. After each step completes, a confirmation dialog appears — press **Enter** to advance, **q** to pause. State is saved to disk so you can resume later.

---

## Creating a workflow file

Use `awman new workflow` to create a workflow file interactively without having to remember the schema by hand.

### Interactive step entry

```sh
# CLI
awman new workflow

# TUI command box
new workflow
```

Both modes prompt for:

1. **Workflow name** — used as the filename slug (e.g. `my-workflow`). Must contain only letters, digits, hyphens, and underscores.
2. **Workflow title** — a human-readable label that appears at the top of the file (may differ from the name).
3. **Steps** — repeat for each step:
   - Step name (required)
   - Agent (optional — press Enter to skip)
   - Model (optional — press Enter to skip)
   - Depends-on (optional — comma-separated step names, press Enter to skip)
   - Prompt text — enter multiple lines and end with a line containing only `.`

After each step you are asked whether to add another. When finished, awman writes the file and prints its path.

**TUI key bindings** (workflow dialog):

| Key | Action |
|-----|--------|
| **Tab** / **Shift-Tab** | Cycle through fields |
| **Ctrl-N** | Commit the current step and start a new one |
| **Ctrl-Enter** | Finish — write the file and close the dialog |
| **Esc** | Cancel without writing |

By default awman writes to `aspec/workflows/<name>.toml` inside the current repo. Pass `--format` to choose a different format:

```sh
awman new workflow --format yaml   # writes aspec/workflows/<name>.yaml
```

### Interview mode

```sh
awman new workflow --interview
```

Enter a one-paragraph summary of what the workflow should accomplish. A code agent writes the complete workflow file for you — filling in step names, dependencies, agents, models, and detailed prompts — the same way `new spec --interview` writes a work item.

In the TUI, the dialog switches to a two-field layout: workflow name and summary. Press **Ctrl-Enter** to start the interview agent.

### Global workflows

```sh
awman new workflow --global
```

Writes to `~/.awman/workflows/<name>.<ext>` instead of the current repo. Use this to build a personal library of reusable workflows that travel with you across projects.

`--global` and `--interview` can be combined. When combined, the agent is given access only to the `~/.awman/workflows/` directory — not the whole repo or home directory — so your other files stay safe. This still requires being inside a git repository (for agent image lookup).

### Flags

| Flag | Description |
|------|-------------|
| `--interview` | Let a code agent complete the workflow from a short summary |
| `--global` | Write to `~/.awman/workflows/` instead of the current repo |
| `--format <fmt>` | Output format: `toml` (default) or `yaml`. Markdown is not supported |

### Edge cases

| Situation | Behaviour |
|-----------|-----------|
| Name contains spaces or path separators | Rejected immediately with a descriptive error |
| Workflow file already exists | Error with the existing path; awman does not overwrite silently |
| Not inside a git repo (non-global) | Error: run with `--global` to write to `~/.awman/` |
| `--global --interview` outside a git repo | Error: agent image lookup requires a git repo |
| Empty step name in TUI | Inline error; dialog stays open |
| No steps added before Ctrl-Enter (TUI) | Inline error: "At least one step is required" |
| Step prompt is empty (CLI) | Warning logged; empty prompt written to file |
| `depends_on` names non-existent steps | Warning logged; file is still written (steps may be added later) |
| Load a `.md` workflow file | Error: "Markdown workflow files are no longer supported. Convert to TOML (.toml) or YAML (.yaml/.yml). See docs/05-workflows.md for the current format." |

---

## Workflow file formats

awman supports two workflow file formats: **TOML** (`.toml`) and **YAML** (`.yml` / `.yaml`). The format is detected automatically from the file extension. Both formats produce identical execution behaviour — you can pass either to `exec workflow` interchangeably.

| Extension | Format |
|-----------|--------|
| `.toml` | TOML |
| `.yml` or `.yaml` | YAML |

Any other extension is rejected with:

```
unsupported workflow format: expected .toml, .yml, or .yaml
```

### Step fields

All steps support the same fields:

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | yes | Unique step identifier within the workflow |
| `prompt` | string | yes | Prompt template sent to the agent |
| `depends_on` | array of strings | no | Names of steps that must complete before this one runs |
| `agent` | string | no | Run this step with a specific agent instead of the default. Valid values: `claude`, `codex`, `opencode`, `maki`, `gemini`, `antigravity`, `copilot`, `crush`, `cline` |
| `model` | string | no | Run this step with a specific model. Overrides any `--model` flag |
| `overlays` | array of strings | no | Overlays applied to this step only — see [Workflow step overlays](08-overlays.md#workflow-step-overlays) |
| `abort_on_failure` | bool | no | When `true`, a failure here stops the whole workflow (and kills any running parallel peers) instead of prompting. Defaults to `false` |

Field names are **lowercase only** (`name`, `depends_on`, `agent`, `model`, `prompt`, `overlays`, `abort_on_failure`). Uppercase variants are not accepted. Unknown fields (e.g. `dependson`, `Prompt`) are rejected as errors so that typos do not silently take effect.

### TOML (`.toml`)

Steps are declared as an array of tables using `[[step]]`. The optional `title` string appears at the top level.

```toml
title = "Implement Feature Workflow"   # optional

[[step]]
name = "plan"
prompt = """
Read the following work item and produce an implementation plan.

{{work_item_content}}
"""

[[step]]
name = "implement"
depends_on = ["plan"]
prompt = """
Implement work item {{work_item_number}} according to the plan.

Follow the spec: {{work_item_section:[Implementation Details]}}
"""

[[step]]
name = "review"
depends_on = ["implement"]
prompt = """
Review the changes from the implement step for correctness and style.
"""

[[step]]
name = "docs"
depends_on = ["implement"]
prompt = """
Write documentation for work item {{work_item_number}}.
"""
```

Use TOML triple-quoted strings (`"""…"""`) for multiline prompts. Newlines and `{{template_vars}}` are preserved exactly.

### YAML (`.yml` / `.yaml`)

Steps are declared as a sequence under the `steps` key. The optional `title` string appears at the top level.

```yaml
title: "Implement Feature Workflow"   # optional

steps:
  - name: plan
    prompt: |
      Read the following work item and produce an implementation plan.

      {{work_item_content}}

  - name: implement
    depends_on: [plan]
    prompt: |
      Implement work item {{work_item_number}} according to the plan.

      Follow the spec: {{work_item_section:[Implementation Details]}}

  - name: review
    depends_on: [implement]
    prompt: |
      Review the changes from the implement step for correctness and style.

  - name: docs
    depends_on: [implement]
    prompt: |
      Write documentation for work item {{work_item_number}}.
```

Use YAML literal blocks (`|`) for multiline prompts. `depends_on` must be a YAML sequence — not a bare string. Newlines and `{{template_vars}}` are preserved exactly.

---

## Setup and teardown phases

Workflows can include optional `setup` and `teardown` sections to prepare the environment before the main steps and perform post-workflow actions.

**Setup phase** runs before the first main step and is intended for:
- Checking out or creating a Git branch
- Pulling latest changes
- Installing dependencies
- Running build or configuration scripts
- Cloning additional repositories needed by the workflow

**Teardown phase** runs after all main steps complete (or on failure, if `teardown_on_failure` is enabled) and is intended for:
- Running tests
- Committing changes
- Creating pull requests
- Pushing branches to a remote
- Cleanup operations

All setup and teardown steps execute inside the project's **base container image** — the same isolated Docker container used for agent steps. No shell commands are ever executed directly on the host. Each phase uses its own container instance: a setup container runs all setup steps, then is killed; later, a teardown container is started for all teardown steps.

### Setup step types

Setup steps are defined in a `[[setup]]` (TOML) or `setup:` (YAML) array. Each step has a `type` field and type-specific fields:

| Type | Fields | Description |
|------|--------|-------------|
| `clone_repo` | `url` (string, required), `branch` (string, optional), `into` (string, optional) | Clone a repository. `branch` checks out a specific branch. `into` specifies the target directory (relative to workdir); omit to use the repo name. Useful for cloning additional repos needed by the workflow (for the primary repo, use the session's `repo_url` and `branch` fields instead). |
| `checkout_create_branch` | `branch` (string, required), `base` (string, optional) | Check out an existing branch or create a new one. If `base` is specified, the branch is created from that ref. Attempts to fetch from the remote first; if unavailable or not configured, falls back to local creation. **Skipped when the workflow runs in an isolated worktree** (`--worktree`, or implied by `--yolo`/`--dynamic`): the run is already on its own branch, so awman skips the step with a warning instead of failing. |
| `pull_branch` | `remote` (string, optional), `branch` (string, optional) | Pull the latest changes from a remote branch. Equivalent to `git pull <remote> <branch>`. Omit both to use `git pull` with defaults. |
| `run_shell` | `command` (string, required), `env` (object, optional) | Execute a shell command. `env` is an optional object of environment variables to inject (`{"KEY": "value"}`). |
| `run_script` | `path` (string, required), `env` (object, optional) | Execute a shell script file (relative to the workdir). `env` is an optional object of environment variables. |
| `poll_ci` | `interval_secs` (integer, optional), `max_retries` (integer, optional) | Poll GitHub for the CI run status of the current branch. Waits for the CI run to complete. `interval_secs` controls the polling interval in seconds (default: 30). `max_retries` limits the number of polling attempts (default: 10). See [Polling CI status](#polling-ci-status) for details. |

Example TOML setup:

```toml
[[setup]]
type = "checkout_create_branch"
branch = "feature/my-feature"
base = "main"

[[setup]]
type = "run_shell"
command = "npm install"

[[setup]]
type = "run_shell"
command = "npm run build"
```

Example YAML setup:

```yaml
setup:
  - type: checkout_create_branch
    branch: feature/my-feature
    base: main
  - type: run_shell
    command: npm install
  - type: run_shell
    command: npm run build
```

### Polling CI status

Both setup and teardown phases can include a `poll_ci` step to wait for GitHub Actions CI to reach a terminal state (success, failure, or timeout). This is useful for workflows that push code and need to verify that CI passes before proceeding.

**How it works:**

1. `poll_ci` detects the current Git branch and HEAD commit SHA
2. Polls GitHub for the CI run associated that commit on that branch
3. Waits for the run to complete (success or failure) or times out after `max_retries` attempts
4. Fails the step if CI fails, succeeds if CI succeeds, or times out if no completion within the retry limit
5. All polling events are logged to the message sink so you can see progress

**Configuration:**

```toml
[[setup]]
type = "poll_ci"
interval_secs = 60
max_retries = 15
```

```yaml
setup:
  - type: poll_ci
    interval_secs: 60
    max_retries: 15
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `interval_secs` | integer | 30 | Number of seconds to wait between polling attempts. |
| `max_retries` | integer | 10 | Maximum number of polling attempts before timing out. Total maximum wait time = `interval_secs × max_retries`. |

**Authentication:**

`poll_ci` uses two strategies in order of preference:

1. **GitHub CLI (`gh`)** — if `gh` is installed and authenticated, awman runs `gh run list` to fetch the most recent run matching the current branch and HEAD commit.
2. **GitHub REST API** — if `gh` is unavailable or not authenticated, awman calls the GitHub REST API directly using the `GITHUB_TOKEN` environment variable. If `GITHUB_TOKEN` is not set, the step fails immediately with a clear error message.

Both methods are secure: `GITHUB_TOKEN` is never logged or exposed in step output.

**Handling missing or multiple CI runs:**

If the branch was just pushed and GitHub hasn't created a run yet, the first few polls may return "not found". awman treats "not found" as a retryable condition (polling continues) for the first 3 attempts, then hard-fails with a message if no run is found. If multiple runs exist for the branch, awman uses the run whose commit SHA matches the current HEAD; if no match is found, it uses the most recent run with a warning.

**Example use case:**

A teardown workflow that pushes code, then waits for CI to pass before reporting success:

```toml
[[teardown]]
type = "push_branch"

[[teardown]]
type = "poll_ci"
interval_secs = 60
max_retries = 20
```

```yaml
teardown:
  - type: push_branch
  - type: poll_ci
    interval_secs: 60
    max_retries: 20
```

### Step remediation with `on_failure`

Any setup or teardown step can include an optional `on_failure` block that automatically launches an agent to fix problems when the step fails. After the agent runs, the failed step is retried. This enables self-healing workflows where transient or fixable failures are resolved without manual intervention.

**When it's useful:**

- A test suite fails with a fixable error (e.g., a race condition, a dependency issue, a broken assertion)
- Build commands fail and can be resolved by adjusting configuration or dependencies
- A CI check fails and you want to automatically push a corrective commit
- You want human-in-the-loop remediation without fully automating the fix (the agent does its best; humans verify the result)

**How it works:**

1. A setup or teardown step executes and fails (non-zero exit)
2. If the step has an `on_failure` block, awman launches a container with the configured agent and model
3. The agent runs the remediation prompt and has full access to the same workdir as the failed step
4. After the agent completes (regardless of success), the original step is retried
5. If the retry succeeds, the workflow continues; if the retry still fails and `max_attempts` is exhausted, the step is marked failed and the workflow continues (or stops, depending on the step's `abort_on_failure` setting)

**Configuration:**

```toml
[[setup]]
type = "run_shell"
command = "npm test"
[setup.on_failure]
prompt = "The test suite failed. Review the output above and fix any issues."
max_attempts = 2
```

```yaml
setup:
  - type: run_shell
    command: npm test
    on_failure:
      prompt: |
        The test suite failed. Review the output above and fix any issues.
      max_attempts: 2
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `prompt` | string | yes | — | The prompt sent to the remediation agent. Should include context about the failure and guidance for fixing it. |
| `agent` | string | no | Inherited from workflow | The agent to use for remediation. If omitted, uses the workflow's default agent or the `--agent` flag value. |
| `model` | string | no | Inherited from workflow | The model to use for remediation. If omitted, uses the workflow's default model or the `--model` flag value. |
| `max_attempts` | integer | yes | — | Maximum number of remediation and retry cycles before the step fails permanently. Must be ≥ 1. |

**Automatic failure output capture for teardown steps:**

When a **teardown** step's `on_failure` remediation agent launches, awman automatically captures the failed command's full stdout and stderr and writes it to a file the agent can read — you don't need to describe the failure yourself or paste logs into `prompt`:

- If the workflow has a writable `context(workflow)` overlay active (see [Overlays](08-overlays.md)), the file is written into that shared directory and shows up in the agent's container at `/awman/context/workflow/teardown-failure-<step-name>.txt`.
- Otherwise, awman creates a dedicated directory for this workflow run and mounts it read-only in the remediation agent's container at `/awman/remediation/teardown-failure-<step-name>.txt`.
- The step name is sanitized into a safe filename (special characters become `-`).
- The file looks like:

  ```
  === FAILED COMMAND: cargo test ===

  --- STDOUT ---
  ...output, or "(empty)" if the command wrote nothing...

  --- STDERR ---
  ...output, or "(empty)" if the command wrote nothing...
  ```

  Output is capped at the last 100 KB per stream; anything beyond that is dropped with a truncation notice so a runaway command can't bloat the agent's context.
- awman automatically prepends a note to the top of the remediation prompt telling the agent where the file is and to read it before attempting a fix — you don't need to reference the path in your own `on_failure.prompt`. You're still free to point at it explicitly for emphasis, e.g.:

  ```toml
  [teardown.on_failure]
  prompt = "Read the captured stdout/stderr file first, then fix the root cause of the failure — don't just retry blindly."
  max_attempts = 2
  ```
- If the same step fails again on a later retry attempt, the file is overwritten with that attempt's output, so the agent always sees the most recent failure, not a stale one.
- This capture applies only to **teardown** steps; `on_failure` on **setup** steps behaves as before, with no automatic file.

**Full example with custom agent and model:**

```toml
[[teardown]]
type = "run_shell"
command = "cargo test"
[teardown.on_failure]
prompt = """
The test suite failed. Analyze the error output and:
1. Check for flaky tests (retry them first)
2. Fix any broken assertions or logic errors
3. Update dependencies if needed
Re-run `cargo test` to verify your fixes work.
"""
agent = "claude"
model = "claude-opus-4-6"
max_attempts = 3
```

```yaml
teardown:
  - type: run_shell
    command: cargo test
    on_failure:
      prompt: |
        The test suite failed. Analyze the error output and:
        1. Check for flaky tests (retry them first)
        2. Fix any broken assertions or logic errors
        3. Update dependencies if needed
        Re-run `cargo test` to verify your fixes work.
      agent: claude
      model: claude-opus-4-6
      max_attempts: 3
```

**Combining `on_failure` with `abort_on_failure`:**

If a step has both `abort_on_failure = true` and an `on_failure` block, the remediation loop runs first. Only after all `max_attempts` are exhausted (and the step still fails) does the workflow abort.

```toml
[[setup]]
type = "run_shell"
command = "critical-build-step"
abort_on_failure = true
[setup.on_failure]
prompt = "The build step failed. This is critical; fix it."
max_attempts = 2
```

```yaml
setup:
  - type: run_shell
    command: critical-build-step
    abort_on_failure: true
    on_failure:
      prompt: The build step failed. This is critical; fix it.
      max_attempts: 2
```

**Best practices:**

- **Be specific in your prompt:** For teardown steps, the failed command's stdout/stderr is captured automatically (see above), but the agent still benefits from you describing what a good fix looks like. For setup steps, there's no automatic capture — include context about what failed and what the agent should try.
- **Keep `max_attempts` small:** Each attempt retries the full step, so 2–3 attempts is usually sufficient.
- **Use for fixable failures:** Remediation works best for transient issues, dependency problems, or test failures with clear causes. For structural errors, fail fast instead.
- **Combine with `poll_ci`:** A common pattern is a teardown that tries to fix code, commits, pushes, then polls CI to verify the fix worked.

**Example: self-healing CI workflow:**

```toml
title = "Implement, Test, and Self-Heal"

[[step]]
name = "implement"
prompt = "Implement the feature according to the spec."

[[teardown]]
type = "run_shell"
command = "cargo test"
[teardown.on_failure]
prompt = "Tests failed. Fix the implementation to make them pass."
max_attempts = 2

[[teardown]]
type = "commit_changes"
message = "Implement feature"
add_all = true

[[teardown]]
type = "push_branch"

[[teardown]]
type = "poll_ci"
interval_secs = 60
max_retries = 20
```

```yaml
title: Implement, Test, and Self-Heal

steps:
  - name: implement
    prompt: Implement the feature according to the spec.

teardown:
  - type: run_shell
    command: cargo test
    on_failure:
      prompt: Tests failed. Fix the implementation to make them pass.
      max_attempts: 2
  - type: commit_changes
    message: Implement feature
    add_all: true
  - type: push_branch
  - type: poll_ci
    interval_secs: 60
    max_retries: 20
```

In this workflow:
1. The implementation step writes code
2. Tests run; if they fail, an agent attempts to fix the code, then tests are re-run
3. If tests pass (either on first run or after remediation), changes are committed and pushed
4. CI is polled to verify the pushed code passes the remote CI pipeline

### Teardown step types

Teardown steps are defined in a `[[teardown]]` (TOML) or `teardown:` (YAML) array. Each step has a `type` field and type-specific fields:

| Type | Fields | Description |
|------|--------|-------------|
| `run_shell` | `command` (string, required), `env` (object, optional) | Execute a shell command. |
| `run_script` | `path` (string, required), `env` (object, optional) | Execute a shell script file. |
| `commit_changes` | `message` (string, required), `add_all` (boolean, optional) | Commit staged changes. If `add_all` is `true`, runs `git add -A` first. |
| `push_branch` | `remote` (string, optional), `branch` (string, optional) | Push the current branch to a remote. Omit both to use `git push` with defaults. |
| `create_pull_request` | `title` (string, optional), `body` (string, optional), `base` (string, optional) | Create a pull request using the GitHub CLI. If `base` is provided, it sets the branch the PR will be opened against (via the `--base` flag). Requires `gh` to be available in the base container image. |
| `poll_ci` | `interval_secs` (integer, optional), `max_retries` (integer, optional) | Poll GitHub for the CI run status of the current branch. Waits for the CI run to complete. `interval_secs` controls the polling interval in seconds (default: 30). `max_retries` limits the number of polling attempts (default: 10). See [Polling CI status](#polling-ci-status) for details. |

Example TOML teardown:

```toml
[[teardown]]
type = "run_shell"
command = "npm test"

[[teardown]]
type = "commit_changes"
message = "automated changes"
add_all = true

[[teardown]]
type = "create_pull_request"
title = "feat: automated implementation"
body = "This PR was created automatically by an awman workflow."
base = "main"
```

Example YAML teardown:

```yaml
teardown:
  - type: run_shell
    command: npm test
  - type: commit_changes
    message: "automated changes"
    add_all: true
  - type: create_pull_request
    title: "feat: automated implementation"
    body: "This PR was created automatically by an awman workflow."
    base: main
```

### Teardown on failure

By default, teardown steps are skipped if the workflow fails. To run teardown even on failure (e.g., to clean up partial artifacts), set `teardown_on_failure = true` at the top level:

```toml
name = "implement-feature"
teardown_on_failure = true

# steps, setup, teardown defined below...
```

If any teardown step fails, the error is logged and execution continues to the next teardown step (best-effort cleanup). Teardown failure does not retroactively change the workflow's success/failure status.

### Container execution model

**Setup container lifecycle:**
1. Before setup runs, awman starts a background container from the base image with the session workdir mounted
2. Each setup step is executed via `exec` into the running container
3. After all setup steps complete (or if any step fails), the setup container is killed
4. If setup fails, the main workflow steps do not run

**Teardown container lifecycle:**
1. After all main steps complete, awman starts a fresh teardown container (separate from the setup container)
2. Each teardown step is executed via `exec` into the teardown container
3. After all teardown steps complete, the teardown container is killed
4. If `teardown_on_failure = false` and the workflow failed, teardown is skipped entirely

All environment variables configured for the project (via overlays, config, or per-step `env` fields) are inherited by both setup and teardown containers, just as they are for main workflow steps.

### Remote sessions and setup/teardown

For `type: remote` API sessions, the repository is already cloned and the branch is checked out by awman at session creation time — the session's working directory points to a fresh clone before any setup steps begin. A `clone_repo` setup step is therefore redundant for provisioning the primary repo and should not be used for that purpose. It remains valid for cloning *additional* repositories needed by the workflow into subdirectories.

### Idempotency

If a workflow is interrupted mid-setup or mid-teardown and then resumed, the full setup or teardown phase is re-run from the beginning. Setup steps should be written to be idempotent — i.e., they should succeed whether or not they have been run before:

- Use `git clone <url> || true` if the directory might already exist
- Use `checkout_create_branch` instead of raw `git checkout -b` (it handles existing branches automatically)
- Ensure package manager commands (`npm install`, `pip install`) are idempotent
- For custom scripts, make them idempotent by checking preconditions

### Base image and `gh` CLI

If your teardown includes a `create_pull_request` step, the base container image **must have the GitHub CLI (`gh`) installed**. By default, the base image is the project's `Dockerfile.dev`. If `gh` is not available, the step will fail with a clear error. Update your `Dockerfile.dev` to include `gh` if you intend to use `create_pull_request` steps.

### Complete example: setup + steps + teardown

This example shows a full workflow that creates a branch, installs dependencies, runs implementation and review steps, then commits, pushes, and opens a PR:

```toml
title = "Implement and Ship"
teardown_on_failure = false

[[setup]]
type = "checkout_create_branch"
branch = "feature/{{work_item_number}}"
base = "main"

[[setup]]
type = "run_shell"
command = "cargo fetch"

[[step]]
name = "implement"
model = "claude-opus-4-6"
prompt = """
Implement work item {{work_item_number}} according to the spec.
"""

[[step]]
name = "review"
depends_on = ["implement"]
prompt = """
Review the changes for correctness and style.
"""

[[teardown]]
type = "run_shell"
command = "make test"

[[teardown]]
type = "commit_changes"
message = "Implement {{work_item_number}}"
add_all = true

[[teardown]]
type = "push_branch"

[[teardown]]
type = "create_pull_request"
title = "Implement {{work_item_number}}"
body = "Automated PR from awman workflow."
base = "main"
```

The equivalent YAML:

```yaml
title: Implement and Ship
teardown_on_failure: false

setup:
  - type: checkout_create_branch
    branch: "feature/{{work_item_number}}"
    base: main
  - type: run_shell
    command: cargo fetch

steps:
  - name: implement
    model: claude-opus-4-6
    prompt: |
      Implement work item {{work_item_number}} according to the spec.
  - name: review
    depends_on: [implement]
    prompt: |
      Review the changes for correctness and style.

teardown:
  - type: run_shell
    command: make test
  - type: commit_changes
    message: "Implement {{work_item_number}}"
    add_all: true
  - type: push_branch
  - type: create_pull_request
    title: "Implement {{work_item_number}}"
    body: Automated PR from awman workflow.
    base: main
```

### Template variables

Template variables are available in **all workflow fields** — step `prompt` values, setup step fields, and teardown step fields. The only field that does not support substitution is `type` (which selects the step kind).

| Variable | Replaced with |
|----------|--------------|
| `{{work_item_number}}` | Zero-padded four-digit work item number (e.g. `0027`) |
| `{{work_item}}` | Bare numeric work item number (e.g. `27`) |
| `{{work_item_content}}` | Full text of the work item Markdown file |
| `{{work_item_section:[Name]}}` | Content of the named `## Name` section from the work item file (case-insensitive heading match, trailing colons stripped) |

All variables require `--work-item` to be passed when running the workflow. If `--work-item` is omitted, `{{work_item_*}}` placeholders are replaced with empty strings and a warning is emitted.

Unknown variables or missing sections are left in place with a warning.

**Examples across workflow phases:**

```toml
# Setup — branch name derived from work item
[[setup]]
type = "checkout_create_branch"
branch = "feature/{{work_item_number}}"

# Step — prompt references work item content
[[step]]
name = "implement"
prompt = "Implement {{work_item_number}}: {{work_item_section:[Summary]}}"

# Teardown — commit message and PR title include work item number
[[teardown]]
type = "commit_changes"
message = "Implement {{work_item_number}}"
add_all = true

[[teardown]]
type = "create_pull_request"
title = "feat: {{work_item_number}}"
body = "{{work_item_section:[Summary]}}"
```

---

## Multi-agent workflows

Each step in a workflow can run in a different agent's container by adding an `agent` key to the step.

```toml
[[step]]
name = "plan"
prompt = "Produce an implementation plan."

[[step]]
name = "implement"
depends_on = ["plan"]
agent = "codex"
prompt = "Implement the plan from the previous step."

[[step]]
name = "review"
depends_on = ["implement"]
agent = "claude"
prompt = "Review the implementation for correctness and style."
```

```yaml
steps:
  - name: plan
    prompt: Produce an implementation plan.
  - name: implement
    depends_on: [plan]
    agent: codex
    prompt: Implement the plan from the previous step.
  - name: review
    depends_on: [implement]
    agent: claude
    prompt: Review the implementation for correctness and style.
```

Steps without an `agent` field use the workflow default agent — the value from repo config (or global config), overridden by the `--agent` flag if one was passed at the command line. The `--agent` flag sets the **default** for steps that do not name an agent; it does **not** override steps that explicitly specify one.

### Agent pre-flight check

Before the first step runs, awman collects every distinct agent name required across all steps and checks that the corresponding image exists. If an image is missing, awman prompts:

```
Agent 'codex' has no Dockerfile. Download and build it? [y/N]:
```

**Accept** — awman downloads the agent Dockerfile template, builds the project base image (if needed), then builds the agent image. If your repo has multiple agents to set up, each is prompted in turn before the workflow begins.

**Decline** — awman asks whether to substitute the default agent for that step instead:

```
Use the default agent (claude) for steps that specify 'codex'? [y/N]:
```

- Accept the fallback: those steps run with the default agent. The workflow starts normally.
- Decline the fallback: the workflow does not start.

If all required images are already available, the pre-flight check completes silently and the first step launches immediately.

### Unknown agents

An unknown agent name in an `agent` field is caught at parse time, before any container runs, and exits with a list of valid options.

### Resuming workflows with per-step agents

When resuming a saved workflow, the per-step agent assignments from the original run are preserved in the state file. If you pass a different `--agent` flag on resume, awman warns you; the persisted assignments still take precedence.

---

## Per-step model overrides

Each step in a workflow can run against a different model by adding a `model` key to the step.

```toml
[[step]]
name = "plan"
agent = "claude"
model = "claude-opus-4-6"
prompt = "Produce a detailed implementation plan."

[[step]]
name = "implement"
depends_on = ["plan"]
agent = "claude"
model = "claude-haiku-4-5"
prompt = "Implement the plan from the previous step."

[[step]]
name = "review"
depends_on = ["implement"]
prompt = "Review the implementation for correctness and style."
```

```yaml
steps:
  - name: plan
    agent: claude
    model: claude-opus-4-6
    prompt: Produce a detailed implementation plan.
  - name: implement
    depends_on: [plan]
    agent: claude
    model: claude-haiku-4-5
    prompt: Implement the plan from the previous step.
  - name: review
    depends_on: [implement]
    prompt: Review the implementation for correctness and style.
```

In this example, `plan` uses a large model for deep reasoning, `implement` uses a smaller model for routine code generation, and `review` inherits whatever model is in effect from the `--model` flag (or the agent's built-in default if no flag was passed).

### Model resolution order

For each step, awman resolves the effective model using this priority:

| Priority | Source | Applies when |
|----------|--------|-------------|
| 1 (highest) | Step's `model` field | The step explicitly declares a model |
| 2 | `--model` flag on the command line | The step has no `model` field |
| 3 (lowest) | Agent built-in default | Neither a step field nor a flag was provided |

The `--model` flag acts as the **default** for all steps without a `model` field; it does **not** override steps that declare their own model.

`agent` and `model` are independent overrides. A step can specify one, both, or neither. When both are present, awman resolves the agent first, then resolves the model.

### Workflow resume and model persistence

Per-step `model` values are persisted in the workflow state file. On resume, the persisted model is used, not any `--model` flag passed on the resumed invocation. This matches the existing behaviour for `agent` fields and ensures the resumed run is identical to the original.

---

## Running a workflow

### In the TUI

```
exec workflow aspec/workflows/implement-hard.toml --work-item 0027
```

The **Workflow Overview** appears, showing each step as a coloured box:

| Colour | Status |
|--------|--------|
| Grey / dim | Pending |
| Blue / bold | Running |
| Green | Done |
| Red / bold | Error |
| Yellow / bold | Stuck (idle for >10 s) |

When a step completes, a confirmation dialog appears. Press **Enter** or **y** to advance, **q** or **Esc** to pause.

### In command mode

```sh
awman exec workflow aspec/workflows/implement-hard.toml --work-item 0027
```

Between steps, awman prints the step summary and prompts:

```
Step 'plan' completed.
Next step(s): implement
Press [Enter] to advance, or [q] to abort:
```

On agent failure:

```
Step 'implement' failed: Container exited with code 1
Press [r] to retry, or any other key to abort:
```

### Flags

`exec workflow` accepts the following flags:

| Flag | Description |
|------|-------------|
| `--agent=<name>` | Default agent for steps that do not specify an `agent` field. Does not override steps with an explicit `agent` value |
| `--model=<NAME>` | Default model for steps that do not specify a `model` field. Does not override steps with an explicit `model` value |
| `--non-interactive` | Run each step's agent in print/batch mode |
| `--plan` | Run each step in read-only mode |
| `--allow-docker` | Mount Docker socket into each step's container |
| `--worktree` | Run all steps in an isolated Git worktree |
| `--overlay=<SPEC>` | Apply overlay(s) to every step; see [Overlays](08-overlays.md) |
| `--yolo` | Fully autonomous mode; implies `--worktree`; auto-advances stuck steps |
| `--dynamic` | Let a leader agent design the workflow file from your work item; see [Dynamic Workflows](06-dynamic-workflows.md) |
| `--leader=<agent::model>` | Override the agent and model used as the leader when `--dynamic` is set; format: `agent::model` |
| `--max-concurrent=<N>` | Cap on concurrently-running steps for this invocation (must be ≥ 1); overrides `maxConcurrentAgents` in config — see [Parallel workflows](05-workflows.md#parallel-workflows) |

---

## Workflow control board (TUI only)

Press **Ctrl+W** at any time to open the **workflow control board** — a popup that lets you redirect execution without waiting for the current step to finish. Ctrl+W works regardless of whether the container window is maximized or minimized.

There are two variants of the control board:

### Lightweight step confirmation (between steps)

When a step completes and the next step is ready, awman shows a compact confirmation dialog:

```
╭─ Step 'implement' done. Advance to 'test'? ─╮
│                                             │
│  [Enter] yes  [Esc] pause  [Ctrl+W] details │
╰─────────────────────────────────────────────╯
```

| Key | Action |
|-----|--------|
| **Enter** | Advance to the next step |
| **Esc** | Pause and wait for your input |
| **Ctrl+W** | Open the full workflow control board for more options |

### Full workflow control board (between or during steps)

The full control board appears when you have multiple options or want fine-grained control. It can be opened mid-step without disrupting the running container:

```
╭───── Workflow Control ──────╮
│ Step: implement             │
│                             │
│    ↑ Restart current step   │
│                             │
│ ← Prev   → Next (new cont.) │
│                             │
│    ↓ Next (same container)  │
│                             │
│ [Arrow] select  [Esc] done  │
╰─────────────────────────────╯
```

#### Between-step actions

| Key | Effect | Container killed? |
|-----|--------|-------------------|
| **↑** | Restart current step — reset to Pending and relaunch in a fresh container | ✓ Yes |
| **←** | Cancel to previous step — mark current step Pending and re-run the most recently completed step | ✓ Yes |
| **→** | Next step: new container — mark current step Done and advance in a new container | ✓ Yes |
| **↓** | Next step: same container — mark current step Done and send the next step's prompt to the existing container via PTY | ✗ No |
| **Esc** | Dismiss and continue waiting | ✗ No |

#### Mid-step actions (when step is running)

When you open the control board **while a step is actively running**, the same actions are available, but with different implications:

| Key | Effect | Container killed? | Step status |
|-----|--------|-------------------|-------------|
| **→** | Force advance — mark current step Done regardless of completion and launch the next step | ✓ Yes | Treated as succeeded |
| **↓** | Continue in current container — queue a message for the running agent to process | ✗ No | Continues running |
| **Esc** | Dismiss — let the step continue running undisturbed | ✗ No | Continues running |
| **↑**, **←** | (same as between-step) | ✓ Yes | (same as between-step) |

The dialog title shows `Workflow Control (step running)` when opened mid-step. Actions that kill the container display a sub-note in gray: `↳ kills running container`. The dismiss action shows: `↳ step keeps running`.

### Next step: same container

The **↓** action reuses the already-running container — the next step's prompt is written directly to its PTY stdin. Useful when the container has already installed dependencies or built artifacts that the next step needs. If the PTY session has closed, awman falls back to a new container and shows a status message.

If the next step requires a **different agent** than the current step, the **↓** option is unavailable. In the TUI it renders greyed out with the message:

```
Next step uses agent 'codex'; cannot reuse current 'claude' container.
```

In command mode, the "same container" prompt is skipped entirely and the explanation is printed instead. Use **→** (new container) to advance, which always works regardless of agent.

### Manual vs. automatic opening

Ctrl+W works at any time when a workflow is active in the current tab — there are no other preconditions. It works mid-step, between steps, during a yolo countdown, or while another dialog is open (the existing dialog is dismissed first).

---

## Workflow Overview and step status

The **Workflow Overview** shows the state of every step in the workflow:

```
Running: plan     ┃  ● implement    ✓ review    ⚠️ docs
```

| Visual | Meaning |
|--------|---------|
| **●** (Blue, bold) | Step is currently running |
| **✓** (Green) | Step completed successfully |
| **⚠️** (Yellow, bold) | Step is stuck (no output for >30 seconds) |
| **●** (Gray, dim) | Step is pending |
| **✗** (Red, bold) | Step encountered an error |
| **🔧** (Magenta, bold) | Step failed and remediation is in progress (on_failure agent running) |

### Agent and model labels

When a step declares its own `agent` and/or `model`, the box shows the resolved **`agent/model`** on its top border (for example `claude/opus-4-8`) so you can see at a glance which agent and model will run each step:

```
 ╭claude/opus-4-8──╮   ╭gemini───────────╮
 │ ● implement     │ → │ ○ review        │
 ╰─────────────────╯   ╰─────────────────╯
```

Steps that declare **neither** an `agent` nor a `model` field inherit the project-default agent and model, so they carry no label — an unlabelled box always means "project defaults". If a step overrides only one of the two, the label shows just that part. The label is truncated to fit narrow boxes.

### Remediation in progress

When a setup or teardown step fails and has an `on_failure` block, the step status changes to **🔧** (remediating) while the agent runs. The workflow status indicator shows which remediation attempt is in progress (e.g., "attempt 1 of 2"). After the agent completes, the original step is automatically retried. The status returns to **●** (running) for the retry.

### Stuck steps

When a step produces no output for more than 30 seconds, it is marked as stuck in the overview. Stuck steps show a warning indicator (⚠️) both in the overview box and in the tab label.

Stuck steps trigger automatic behavior depending on the mode:
- In **yolo mode**: the engine starts a 60-second countdown. When it expires, the step is auto-advanced. If the user cancels (Esc) and the step re-stucks, the countdown restarts from 60 seconds with no backoff.
- In **non-yolo mode**: the workflow control board opens automatically so you can decide what to do.
- In either mode, new PTY output immediately clears the stuck state and cancels any active countdown.

You can always open the control board manually via **Ctrl+W** regardless of stuck status.

### Parallel step groups

Steps that share the same dependencies form a **parallel group** and run concurrently, each in its own container, up to the [`maxConcurrentAgents`](07-configuration.md#reference) cap (unlimited by default).

The overview shows a parallel group in one of two ways, toggled with **Ctrl-O**:

- **Minimized** (the default) — the whole group is one box reading `3 steps…`, coloured by the group's overall state (a failed step colours the box red, otherwise a running step colours it blue, and so on). The overview stays 3 rows tall no matter how wide the workflow fans out.
- **Maximized** — every step in the group gets its own box, stacked vertically at the same indent, keeping its full name, its `agent/model` label, and its own status colour for the whole run. Completed steps are never rolled up or hidden behind finished siblings.

The maximized overview grows to fill the vertical space between the tab bar and the command box — half of it when a container window is maximized too, since **Ctrl-O** and **Ctrl-M** are independent and neither puts the other away. If a group is larger than the overview's height, the last box becomes a `+ N more…` marker — use the **mouse wheel** over the overview to scroll through the rest.

In the TUI, running steps beyond the concurrency cap wait their turn with a `·` prefix on their name until a slot frees up.

See [Parallel workflows](05-workflows.md#parallel-workflows) for the full scheduling model, and [Using the TUI](02-using-the-tui.md#parallel-containers) for how multiple running containers are displayed and switched between.

### Viewing the full control board

When a step completes, awman shows the lightweight confirmation dialog. To see all available actions and options, press **Ctrl+W** to open the full control board. Pressing **Esc** on the lightweight dialog pauses the workflow for manual input.

---

## Auto-advance when stuck (yolo mode)

When a running workflow step produces **no output for 30 seconds**, the engine marks that step stuck. What happens next depends on the permission mode:

- **In [yolo mode](03-agent-sessions.md#--yolo):** a **60-second countdown** starts, and the workflow auto-advances when it expires.
- **In every other mode:** the [workflow control board](#workflow-control-board-tui-only) opens automatically so you can decide what to do.

Stuck detection is unified across all frontends (TUI, CLI, and API) and runs continuously inside the container engine, which tracks output activity on stdout and stderr. Any new output — even a single byte — immediately clears the stuck state. Each container is tracked independently, so one noisy agent never masks detection on its siblings.

**Active-tab suppression:** if you are actively pressing keys or scrolling on the currently active tab, the stuck timer is held back even while the container is silent — the timer starts only once both the container and you have been idle for 30 seconds. Background tabs are always judged on output time alone.

### How the countdown appears

**TUI — Active tab (yolo countdown dialog):**

When the stuck tab is currently active in the TUI, the countdown dialog opens:

```
╭─────── Yolo: Auto-Advance ──────────────╮
│ Step: implement                          │
│                                          │
│  No activity detected.                   │
│  Advancing to next step in  47s...       │
│                                          │
│                    [Esc] cancel          │
╰──────────────────────────────────────────╯
```

The dialog updates every ~100 ms to show the remaining time.

**TUI — Background tab (tab bar countdown):**

When the stuck tab is in the background, no dialog opens. Instead, the tab bar shows a live countdown: the tab alternates between yellow and purple every second, with the label cycling between `⚠️ yolo in N` and `🤘 yolo in N` (where `N` is the remaining seconds):

```
┌─ Tab 1: myproject ─────────┬─ Tab 2 ⚠️  yolo in 38 ─────┐
│  chat                        │                              │
└──────────────────────────────┴──────────────────────────────┘
```

This lets you monitor all tabs' countdown state without leaving your current work.

**CLI and API:** countdown status messages go to the message sink (stderr for the CLI, the event stream for the API) and are **throttled to one every 10 seconds**, even though the countdown updates internally every ~100 ms. The TUI receives every tick and renders the countdown at full granularity.

### Interacting with a countdown

- **Switching to a tab that is counting down** opens the yolo dialog immediately at the remaining time — the timer is never restarted from 60 seconds.
- **Switching away** with **Ctrl+A** / **Ctrl+D** while the dialog is open closes the dialog and lets the countdown continue in the background. You are never forced to resolve it first.
- **Esc** dismisses the active-tab dialog manually. If the container goes silent again, a fresh 60-second countdown begins — there is no backoff.
- **Output resuming mid-countdown** clears the stuck state, cancels the countdown, and returns the tab to its normal colour.

**When the countdown expires:**

- If this is not the last step — awman kills the stuck step's container and advances to the next step in a new container
- If this is the last step — the workflow transitions to complete
- In the TUI, the killed container's window closes immediately, leaving the [summary bar](02-using-the-tui.md#when-the-container-exits) behind — the window only closes on actual container death, never while the container is merely stuck or while the countdown is still running

In the background this happens without you switching to the tab; the tab returns to its normal colour and label as soon as it moves on to the next step.

---

## Container failure logs

While a workflow runs, awman keeps a rolling buffer of the **last ~100 lines** of combined stdout/stderr for each step's container. If a step container exits with a **non-zero exit code that awman did not cause**, awman writes that buffer to a log file and prints an error telling you where it went:

```
Step 'build' container 'awman-quick-brown-fox' exited with code 1. Recent output saved to /home/you/.awman/logs/9f8c…-build-awman-quick-brown-fox.log
```

The log path follows the pattern:

```
~/.awman/logs/{workflow-id}-{step-name}-{container-name}.log
```

- `{workflow-id}` is the workflow's invocation id (a UUID), so re-runs never overwrite each other.
- `{step-name}` and `{container-name}` identify exactly which step and container failed — useful when several steps run in parallel.

This gives you the container's final output for debugging even after the TUI has scrolled it away or the container has been removed.

**When a log is *not* written:** awman only writes a failure log when the container failed on its own. Containers that awman itself stops — yolo auto-advance, control-board **Abort**/**Pause**/**Finish**, a stuck-step cancel, `abort_on_failure` killing sibling steps, or a startup-grace kill — exit as *expected*, so no log is written for them. Sandbox-class runtimes (`docker-sbx-experimental`) don't use the container I/O bridge and don't produce these logs.

The `~/.awman/logs/` directory is created on demand and is never cleaned automatically; delete old logs whenever you like.

---

## Workflow state persistence

awman saves workflow state to:

```
$GITROOT/.awman/workflows/<repo-hash8>-<work-item>-<workflow-name>.json
```

The file records the status of every step, the container ID used for each step, and a SHA-256 hash of the workflow file.

### Resuming

If a saved state file exists when you run `exec workflow`, awman offers to resume:

```
Found a saved workflow state for 'implement-feature' (work item 0027).
  1) Resume from where you left off
  2) Restart from the beginning
  [1/2]:
```

### Workflow file changed

If the workflow file has been modified since the state was saved, awman warns you:

```
WARNING: The workflow file has changed since the last run.
  1) Restart from the beginning
  2) Continue anyway (could be dangerous)
  [1/2]:
```

If you choose `2`, awman verifies that step names and `Depends-on` values are identical. If they differ, it forces a restart.

### Interrupted steps

If a step was running when awman last exited:

```
Step 'implement' was running when the previous session ended.
Start it over (s) or skip to next step (n)? [s/n]:
```

---

## Parallel workflows

A workflow's steps don't have to run one at a time. Any steps that share the same [`depends_on`](#step-fields) set — meaning neither depends on the other — form a **parallel group**, and awman runs them concurrently, each in its own container. This section covers how many agents run at once, how the engine schedules them, and how stuck detection, yolo mode, and the control board behave when more than one agent is active.

For how parallel containers appear on screen, see [Using the TUI](02-using-the-tui.md#parallel-containers) and [Parallel agents in interactive CLI mode](09-api-and-remote-mode.md#parallel-agents-in-interactive-cli-mode).

### What parallelism means here

Consider a workflow where `tests` and `docs` both depend only on `implement`, and nothing depends on either of them:

```
implement → tests
          → docs
       → review (depends on tests, docs)
```

`tests` and `docs` form a parallel group: once `implement` finishes, both become eligible to run, and awman launches both at once instead of waiting for one to finish before starting the other. `review` still waits for both to complete, since it depends on them.

This is entirely driven by your workflow file's `depends_on` graph — you don't opt into parallelism explicitly. Any steps whose dependencies are satisfied at the same time run together, up to the concurrency cap described below.

---

### Configuring `maxConcurrentAgents`

`maxConcurrentAgents` caps how many containers can run at once, machine-wide or per-repo. It's a plain [config field](07-configuration.md#reference), so it follows the same precedence as everything else:

```
--max-concurrent  >  AWMAN_MAX_CONCURRENT_AGENTS  >  repo config  >  global config  >  unlimited
```

```sh
awman config set maxConcurrentAgents 3              # this repo
awman config set --global maxConcurrentAgents 2     # every project on this machine
awman exec workflow workflow.toml --max-concurrent 4   # this run only
```

Left unset at every level, there is **no cap** — every step whose dependencies are satisfied launches immediately. In practice you'll usually want a cap that matches your machine's CPU/memory headroom and your Docker daemon's capacity, since each parallel step is a full container running its own agent.

A `maxConcurrentAgents` of `1` disables parallelism entirely: steps run one at a time, in the same order they would without any concurrency at all. `0` is rejected — if you want to pause parallelism, unset the field or set it to `1`.

> `dynamicWorkflows.maxConcurrentSteps` is a different, unrelated setting: it's an advisory hint passed to the leader agent that *designs* a `--dynamic` workflow. `maxConcurrentAgents` is what the engine actually enforces at run time, for any workflow, dynamic or not.

---

### How the engine schedules steps

When a parallel group becomes ready, awman launches as many of its steps as the concurrency cap allows, in the order they appear in the workflow file. Any remaining steps in the group wait in a queue.

- **A slot frees up** whenever a running step finishes successfully. The next queued step (in file order) starts immediately into that slot.
- **A step that fails** without `abort_on_failure` stops new steps from being queued into the group, but lets its already-running siblings keep going until they finish; you're then prompted the same way you would be for a sequential failure.
- **A step with `abort_on_failure = true` that fails** kills every other active step in the group immediately and cancels anything still queued — the same all-stop behavior `abort_on_failure` has always had, just applied to every running peer at once instead of a single step.

If a workflow resumes from a saved state mid-group, any steps that were interrupted are replayed; steps that had already succeeded stay succeeded.

---

### Stuck and yolo behavior, per container

Every running container is tracked independently — one noisy or slow agent never masks or delays detection on its siblings.

- **Stuck detection (yolo off):** if a container produces no output for 30 seconds, that container alone is marked stuck. Its siblings keep running unaffected. The stuck container's slot stays occupied — no new step launches into it — until you switch to it and send Ctrl-C to kill it, at which point its slot frees up like any other completion.
- **Yolo mode:** each container gets its own independent 60-second auto-advance countdown. When one container's countdown expires, only that container is killed and its step marked advanced; the rest of the group is untouched, and the next queued step (if any) starts into the freed slot. If the group has nothing left queued, the remaining containers simply keep running until they finish.

**Where the countdown appears in the TUI:** a yoloing container that is *not* the focused one shows its countdown in its minimized status bar (`Yolo in Ns`, flashing purple/yellow — see [Using the TUI: Parallel containers](02-using-the-tui.md#parallel-containers)). If it's the focused container, the countdown instead opens the same modal dialog a single container shows. Pressing **Ctrl-S** to rotate focus away closes that modal — the countdown itself keeps running in the background regardless — and rotating back onto a container still counting down reopens the modal automatically.

See [Permission modes](03-agent-sessions.md#permission-modes) for the general countdown behavior this builds on.

---

### The workflow control board with multiple agents running

Opening the control board (**Ctrl-W** in the TUI) while more than one agent is running scopes its actions to whichever container is currently **focused** — the one you'd switch to with Ctrl-S. The board makes this explicit: it names the focused step and shows how many peers are still running.

Some actions only make sense once the whole group has settled and are unavailable while any peer is still active:

| Action | Behavior with active peers |
|---|---|
| Restart current step | Disabled while any other agent in the group is still running, with a reason pointing you at Ctrl-S — restarting always targets the focused container, but only once its siblings have finished. |
| Cancel to previous step | Disabled while any peer is still running: rewinding a step in a group that's still mid-flight isn't well-defined until the group finishes. |
| Finish workflow | Disabled while any peer is still running, for the same reason. |
| Pause | Always available — suspends the whole workflow, killing every active container in the group. |
| Abort | Always available — same, but marks the workflow aborted rather than paused. |

When an action is unavailable, the reason is shown alongside it rather than just being greyed out silently.

Each parallel step gets its own control board when it completes or gets stuck; you're never blocked from acting on one step because another is still busy — you just can't ask the workflow as a whole to move backward or forward (cancel to a previous step, or finish) until the whole group has drained.

---

## Bundled examples

`aspec/workflows/` contains ready-to-use workflow files:

| File | Description |
|------|-------------|
| `implement-hard.toml` | Four-step workflow: implement → tests + docs (parallel) → review. Uses Opus for implementation, Haiku for docs, and a final interactive review step |
| `implement-pr.toml` | Same four steps as `implement-hard.toml`, plus teardown steps that run tests, commit changes, push the branch, and create a pull request |
| `dependency-upgrade-pr.toml` | Two-step workflow: security audit → version audit. Upgrades vulnerable dependencies first, then reviews available version updates, then opens a PR |

---

## Edge cases

| Situation | Behaviour |
|-----------|-----------|
| Cycle in `depends_on` graph | Error before any agent runs |
| Unknown `depends_on` step name | Error at parse time |
| Unknown agent name in `agent` field | Error at parse time, before any containers run |
| Missing agent image at workflow start | Pre-flight prompt: build it, fall back to default, or abort |
| Agent Dockerfile download fails during pre-flight | Error surfaced; workflow does not start |
| Agent image build fails during pre-flight | Error surfaced; partial Dockerfile removed; workflow does not start |
| `--agent` flag + step with explicit `agent` field | Step's `agent` value wins; `--agent` is only the default for unspecified steps |
| `--model` flag + step with explicit `model` field | Step's `model` value wins; `--model` is only the default for steps without a `model` field |
| `model` combined with `agent` in the same step | Independent overrides; agent resolved first, then model |
| `model` field with no value | Treated as absent; agent launches with its built-in default or `--model` flag value |
| Invalid model name in `model` field | Passed verbatim to the agent; the agent surfaces its own error |
| Resume with a different `--model` flag | Persisted per-step model values take precedence; `--model` applies only to steps with no persisted model |
| All steps specify non-default agents | Pre-flight still runs for each; default fallback offered only if setup is declined |
| Parallel steps with different agents | Each step runs in its own container — no cross-step sharing |
| Resume with a different `--agent` flag | Warning printed; persisted per-step agent assignments take precedence |
| Current step and next step use the same agent | "Same container" (**↓**) option available as usual |
| Current step and next step use different agents | "Same container" option greyed out (TUI) or skipped (CLI) with explanation |
| Empty workflow file | Rejected with a helpful message |
| Unsupported file extension (e.g. `.json`) | Error: `unsupported workflow format: expected .toml, .yml, or .yaml` |
| Markdown workflow file (`.md`) | Error: `Markdown workflow files are no longer supported. Convert to TOML (.toml) or YAML (.yaml/.yml).` |
| TOML/YAML step missing `name` field | Parse error including the step index |
| TOML/YAML step missing `prompt` field | Parse error including the step name (or index if unnamed) |
| Empty `[[step]]` / `steps:` array | Error: `"workflow file contains no steps"` |
| `depends_on` as bare YAML string instead of sequence | Parse error; must be a YAML sequence |
| Unknown field in TOML/YAML step (e.g. `dependson`) | Parse error; typos are not silently dropped |
| Uppercase field name in TOML/YAML (e.g. `Name:`) | Parse error; field names must be lowercase |
| Setup step with invalid type | Parse error; type must be one of the supported step types |
| Teardown step with invalid type | Parse error; type must be one of the supported step types |
| `create_pull_request` step but `gh` not in base image | Step fails at execution time with "command not found: gh" |
| `poll_ci` step | Polls GitHub for CI status; see [Polling CI status](#polling-ci-status) for authentication and error handling |
| `poll_ci` with no CI run found yet | First 3 polling attempts treat "not found" as retriable; after 3 attempts, step fails with "No CI run found" |
| `poll_ci` with multiple CI runs on the branch | Uses the run matching the current HEAD commit SHA; if no match, uses the most recent run with a warning |
| `poll_ci` with `gh` CLI available but not authenticated | Falls back to GitHub REST API with `GITHUB_TOKEN` |
| `poll_ci` with neither `gh` nor `GITHUB_TOKEN` | Step fails immediately with "Cannot authenticate with GitHub" error |
| `poll_ci` when CI fails (red status) | Step fails with CI failure details |
| `poll_ci` after pushing new commits | Re-detects HEAD SHA; polls for the new CI run, not the old one |
| Step with `on_failure` block | When step fails, launches a remediation agent; retries the step after agent completes; see [Step remediation](#step-remediation-with-on_failure) |
| Step with `on_failure` and `max_attempts = 0` | Parse error; `max_attempts` must be ≥ 1 |
| `on_failure` agent exits with error | Agent exit code is ignored; the original step is retried regardless |
| `on_failure` exhausts `max_attempts` | Step is marked failed; workflow continues (or stops if `abort_on_failure = true`) |
| `abort_on_failure = true` + `on_failure` block | Remediation loop runs first; only if all attempts fail does abort trigger |
| Setup failure | Main workflow steps do not run; go directly to teardown (if `teardown_on_failure = true`) or exit |
| Teardown step failure (non-zero exit) | Error is logged; execution continues to next teardown step (best-effort); same for `on_failure` remediation |
| Teardown step with `on_failure` fails | Failed command's stdout/stderr is automatically captured to a file and referenced in the remediation agent's prompt — see [Automatic failure output capture](#step-remediation-with-on_failure) |
| Retried teardown step fails again during remediation | The captured output file is overwritten with the latest attempt's stdout/stderr |
| `checkout_create_branch` with no remote configured | Falls back to local branch creation from HEAD or specified `base` |
| `run_script` step with non-existent path | Step fails with file-not-found error |
| Setup interrupted and resumed | Full setup phase re-runs from the beginning; steps should be idempotent |
| Work item file not found | Error before loading the workflow |
| Workflow file not found / unreadable | Clear error with the file path |
| Agent failure mid-workflow | Step marked Error; user prompted to retry or abort |
| Very long step names | Truncated to 12 characters with `…` in the TUI Workflow Overview |
| Large number of parallel steps | Capped at 3 visible rows; extra shown as `+ N more…` |
| Large number of sequential steps | `+ N more…` box at the far right of the Workflow Overview |
| **d** pressed; auto-popup suppressed | Auto-open skipped until workflow advances; Ctrl+W still works |
| Container window maximized (auto-open) | Dialog opens over the maximized terminal; input routes to dialog |
| Another dialog already open | Both Ctrl+W and auto-open suppressed until open dialog is dismissed |
| Step silent on a background tab (non-yolo) | Auto-open deferred; control board appears when you switch to that tab |
| Step silent on a background tab (yolo) | Live countdown shown in tab bar; dialog opens when you switch to the tab; workflow auto-advances when countdown expires |
| Esc dismissed; container still silent | Timer resets; dialog re-opens after another 10 s |
| Output resumes before 10 s threshold | Stuck state clears; auto-open does not trigger |
| User actively scrolling on active tab | Stuck timer suppressed; control board does not open while user is engaged |
| User becomes idle after scrolling | Timer starts from idle moment; control board opens after another 10 s of silence |

### Known limitations

- **TUI resume dialogs**: hash-mismatch and resume prompts use auto-restart behaviour rather than a full dialog.

---

[← Security & Isolation](04-security-and-isolation.md) · [Next: Dynamic Workflows →](06-dynamic-workflows.md)
