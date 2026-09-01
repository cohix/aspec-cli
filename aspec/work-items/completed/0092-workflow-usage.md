# awman Workflow File Format

This document describes the complete format for awman workflow files (`.toml` or `.yaml`/`.yml`).
A workflow file defines a multi-agent pipeline: an optional setup phase, a directed graph of
agent steps, and an optional teardown phase.

---

## File Format

Workflows are written in TOML or YAML. The file extension determines the parser:
- `.toml` — TOML format (recommended; shown throughout this document)
- `.yaml` / `.yml` — YAML format (identical fields, different syntax)

`.md` and `.json` are not supported.

---

## Top-Level Fields

```toml
title = "My Workflow"          # Optional. Human-readable name shown in the UI.
# name = "My Workflow"         # Alias for title.

agent = "claude"               # Optional. Default agent for all steps. Overridden per-step.
model = "claude-sonnet-4-6"    # Optional. Default model for all steps. Overridden per-step.

overlays = ["skill(*)", "env(API_KEY)"]  # Optional. Applied to every agent step.

teardown_on_failure = false    # Optional. Default: false. When true, teardown steps run
                               # even if a workflow step fails.
```

---

## Prompt Template Variables

Prompts in `[[step]]` entries support template substitution. Variables are replaced before
the prompt is sent to the agent:

| Variable | Expands to |
|---|---|
| `{{work_item_number}}` | Zero-padded four-digit work item number (e.g. `0042`) |
| `{{work_item}}` | Bare numeric work item number (e.g. `42`) |
| `{{work_item_content}}` | Full text content of the work item file |
| `{{work_item_section:[Section Name]}}` | Content of a named H1 or H2 section within the work item file |

Example:
```toml
prompt = """
Implement work item {{work_item_number}}.

Edge cases to handle:
{{work_item_section:[Edge Case Considerations]}}
"""
```

Substitution only applies when `--work-item` is passed to `awman exec workflow`. Missing
variables expand to empty strings and produce a warning.

---

## Setup Steps (`[[setup]]`)

Setup steps run sequentially on the host machine before any agent step is launched.
They do not run inside agent containers.

**Important constraint:** `skill()` and `skills()` overlays are not valid on setup steps —
those overlay types require an agent container and are only valid on `[[step]]` entries.

### `run_shell`
Run a shell command on the host.
```toml
[[setup]]
type = "run_shell"
command = "cargo fetch --locked"
# env = { VAR = "value" }   # Optional environment variables for this command.
# abort_on_failure = true    # Optional. Default: false. Stop workflow on non-zero exit.
```

### `run_script`
Run a script file on the host.
```toml
[[setup]]
type = "run_script"
path = ".awman/scripts/setup.sh"   # Relative to the repo root.
# env = { VAR = "value" }           # Optional.
# abort_on_failure = true            # Optional. Default: false.
```

### `checkout_create_branch`
Create and check out a new git branch.
```toml
[[setup]]
type = "checkout_create_branch"
branch = "feature/my-feature"
# base = "main"   # Optional. Branch to base off. Defaults to current HEAD.
```

### `pull_branch`
Pull a remote branch.
```toml
[[setup]]
type = "pull_branch"
# remote = "origin"   # Optional.
# branch = "main"     # Optional.
```

### `clone_repo`
Clone a git repository into the working directory.
```toml
[[setup]]
type = "clone_repo"
url = "https://github.com/example/repo.git"
# branch = "main"        # Optional.
# into = "subdir-name"   # Optional. Target directory name. Defaults to repo name.
```

### `poll_ci`
Wait for CI to pass before proceeding.
```toml
[[setup]]
type = "poll_ci"
# interval_secs = 60   # Optional. How often to poll. awman chooses a default if omitted.
# max_retries = 20     # Optional. Maximum number of poll attempts.
```

### Setup step `on_failure`
Any setup step can define an `on_failure` remediation block. When the step fails, awman
launches an agent with the given prompt to attempt a fix, then re-runs the step. This
repeats up to `max_attempts` times.

```toml
[[setup]]
type = "run_shell"
command = "cargo build"

[setup.on_failure]
prompt = "The build failed. Fix the compilation errors and make it pass."
# agent = "claude"           # Optional. Defaults to workflow/repo default.
# model = "claude-opus-4-8"  # Optional.
max_attempts = 2             # Required. Must be >= 1.
```

---

## Agent Steps (`[[step]]`)

Agent steps run inside isolated Docker containers. The `name` and `prompt` fields are required.

```toml
[[step]]
name = "implement"                  # Required. Unique name for this step.
prompt = "Implement the feature."   # Required. Sent to the agent as its initial task.

depends_on = ["other-step"]         # Optional. List of step names that must complete first.
                                    # Steps with no depends_on run in parallel with other
                                    # independent steps.

agent = "claude"                    # Optional. Overrides workflow-level agent for this step.
model = "claude-opus-4-8"           # Optional. Overrides workflow-level model for this step.

overlays = ["skill(*)", "ssh()"]    # Optional. Merged with workflow-level overlays.

abort_on_failure = false            # Optional. Default: false. When true, the entire
                                    # workflow aborts if this step fails.
```

Steps with `depends_on` run after all named dependencies complete successfully. Steps with
no `depends_on` may run in parallel with each other (subject to the configured worker count).

---

## Teardown Steps (`[[teardown]]`)

Teardown steps run sequentially on the host machine after the last agent step completes.
When `teardown_on_failure = true` at the top level, these also run if the workflow fails.

**Same constraint as setup:** `skill()` and `skills()` overlays are not valid here.

### `run_shell`
```toml
[[teardown]]
type = "run_shell"
command = "make test"
# env = { CI = "true" }     # Optional.
# abort_on_failure = true    # Optional. Default: false.
```

### `run_script`
```toml
[[teardown]]
type = "run_script"
path = ".awman/scripts/post-workflow.sh"
```

### `commit_changes`
Commit all staged (or all) changes.
```toml
[[teardown]]
type = "commit_changes"
message = "Implement {{work_item_number}}"   # Prompt templates are supported here.
add_all = true                               # Optional. Default: false. Stage all changes.
```

### `push_branch`
Push the current branch to a remote.
```toml
[[teardown]]
type = "push_branch"
overlays = ["ssh()"]      # Typically needed for SSH authentication.
# remote = "origin"       # Optional.
# branch = "HEAD"         # Optional.
```

### `create_pull_request`
Open a pull request via the GitHub API.
```toml
[[teardown]]
type = "create_pull_request"
overlays = ["env(GITHUB_TOKEN)"]   # Required for GitHub API access.
# title = "Implement {{work_item_number}}"  # Optional.
# body = "Automated PR."                   # Optional.
# base = "main"                            # Optional. Target branch. Defaults to repo default.
```

### `poll_ci`
```toml
[[teardown]]
type = "poll_ci"
# interval_secs = 30
# max_retries = 30
```

### Teardown step `on_failure`
Same as setup `on_failure` — remediation agent + retry loop.
```toml
[[teardown]]
type = "run_shell"
command = "make test"

[teardown.on_failure]
prompt = "Tests are failing. Fix them."
max_attempts = 3
```

---

## Overlays

Overlays mount additional resources into agent containers. They are specified as strings in
a list and can appear at the workflow level (`overlays = [...]` at the top), per `[[step]]`,
or per `[[setup]]`/`[[teardown]]` (except `skill()` overlays, which are step-only).

Overlays from all scopes are merged; the most restrictive permission wins on conflicts.

### `dir()` — mount a host directory
```
dir(/host/path:/container/path)
dir(/host/path:/container/path:ro)
dir(/host/path:/container/path:rw)
```
Bare `host:container` and `host:container:perm` forms are also accepted.

### `skill()` — mount an agent skill directory
Valid on `[[step]]` entries only (not setup or teardown).
```
skill(*)         # Mount all skills for the selected agent.
skill(lint)      # Mount only the "lint" skill.
```

### `ssh()` — mount SSH credentials
```
ssh()
```
Mounts `~/.ssh` read-only into the container. Useful for `push_branch` teardown steps.

### `env()` — pass an environment variable
```
env(GITHUB_TOKEN)
env(API_KEY)
```
Reads the named variable from the host environment and injects it into the container.

### `context()` — mount an awman context directory
```
context(global)      # ~/.awman/context/global/
context(repo)        # <git-root>/.awman/context/
context(workflow)    # Workflow-run-scoped context directory (read-write by default)
context(repo:ro)     # Read-only variant
context(repo:rw)     # Explicit read-write (default)
```
Context directories persist across agent steps within a workflow run (for `workflow` scope)
and across runs (for `global` and `repo` scopes). The `workflow` scope context is created
fresh for each workflow invocation.

---

## Step Per-Entry Flags

Both `[[setup]]` and `[[teardown]]` entries support:

| Field | Type | Default | Description |
|---|---|---|---|
| `overlays` | `[string]` | none | Overlays for this step (note: `skill()` not valid on setup/teardown) |
| `abort_on_failure` | bool | `false` | Stop the workflow if this step fails |
| `on_failure` | table | none | Remediation config (see above) |

`[[step]]` entries support:

| Field | Type | Default | Description |
|---|---|---|---|
| `name` | string | **required** | Unique step identifier |
| `prompt` | string | **required** | Initial task sent to the agent |
| `depends_on` | `[string]` | `[]` | Step names that must complete first |
| `agent` | string | workflow default | Agent container for this step |
| `model` | string | workflow default | Model for this step |
| `overlays` | `[string]` | none | Step-level overlays (merged with workflow overlays) |
| `abort_on_failure` | bool | `false` | Abort workflow on step failure |

---

## Minimal Valid Workflow

A workflow must have at least one `[[step]]`:

```toml
[[step]]
name = "implement"
prompt = "Implement the feature described in the work item."
```

---

## Practical Tips for Dynamic Workflow Design

When designing a workflow for a specific work item, consider:

1. **Read the work item carefully** — use `{{work_item_content}}` in your first step's
   prompt, or pull specific sections with `{{work_item_section:[Section Name]}}`

2. **Parallelize independent work** — steps with no shared `depends_on` run concurrently;
   e.g. a `tests` step and a `docs` step can both depend on `implement` and run in parallel

3. **Use `abort_on_failure`** on critical steps like a final `review` or a `run_shell` test
   command — fail fast rather than continuing with broken output

4. **Use `on_failure` remediation** on `run_shell` teardown steps that run the test suite —
   this gives an agent a chance to fix failures before the workflow aborts

5. **Keep prompts specific** — reference the work item number, paste relevant spec sections,
   and give the agent precise success criteria rather than open-ended instructions

6. **Match model to task complexity** — use a capable model (e.g. `claude-opus-4-8`) for
   the implementation step; a lighter model (e.g. `claude-haiku-4-5`) works well for docs

7. **Teardown for automation** — include `commit_changes`, `push_branch`, and
   `create_pull_request` teardown steps if you want the workflow to produce a PR automatically;
   include `ssh()` on `push_branch` and `env(GITHUB_TOKEN)` on `create_pull_request`
