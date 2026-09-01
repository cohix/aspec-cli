# Work Item: Task

Title: The Great Refocusing — Part 4: Workflow Config Overhaul (Setup/Teardown + Drop Markdown)
Issue: issuelink

## Summary

This work item overhauls awman's workflow configuration format:

1. **Drop Markdown**: The Markdown workflow format is removed entirely. TOML and YAML are the only supported formats going forward. All Markdown parsing code is deleted, not deprecated.

2. **Setup and teardown sections**: Workflow definitions gain two new optional sections — `setup` and `teardown`. Setup steps run before the first workflow step and are intended to prepare the working environment (check out a branch, pull latest changes, install dependencies, run a shell script, etc.). Teardown steps run after the last workflow step (or on failure, if configured) and are intended for post-workflow actions (run tests, commit changes, create a pull request, etc.).

**All setup and teardown steps execute inside the project's base container image** (the same `Dockerfile.dev`-based image used for agent containers), mounted to the session workdir in the same way an agent container would be. No shell commands are ever executed directly on the host by the awman server. For the setup phase, the base container is started in the background, each step is executed via `exec` into that running container, and then the container is killed. The same pattern is used for the teardown phase. Each phase uses its own container instance.

For `type: remote` API sessions (see WI 0079), the repository is already cloned and the branch checked out by `GitEngine` at session creation time — setup steps do not need to handle repo provisioning. Setup is useful for supplementary operations such as branch checkout, dependency installation, config generation, and environment preparation. Both local and remote sessions benefit from setup/teardown.

Before implementing, read and internalize `aspec/architecture/2026-grand-architecture.md` in full. The workflow definition types live in Layer 0. The `BackgroundContainer` type and `WorkflowEngine` setup/teardown methods live in Layer 1. `ExecWorkflowCommand` in Layer 2 manages the container lifecycle and coordinates the three phases. Layer 3 frontends receive output from all phases via `WorkflowFrontend` trait methods.

## User Stories

### User Story 1:
As a: workflow author

I want to:
define `[setup]` and `[teardown]` sections in my TOML workflow file with steps like `checkout_create_branch`, `pull_branch`, `clone_repo`, `run_shell`, `commit_changes`, and `create_pull_request`

So I can:
submit a workflow that installs dependencies, generates config, runs my workflow steps, and creates a PR — with setup and teardown handled automatically inside isolated containers

### User Story 2:
As a: workflow author with existing TOML/YAML workflows

I want to:
continue using my existing workflows unchanged, since setup and teardown sections are optional

So I can:
adopt the new format incrementally without being forced to add setup/teardown to every existing workflow

### User Story 3:
As a: user with legacy Markdown workflows

I want to:
receive a clear, actionable error message when I attempt to load a `.md` workflow file

So I can:
understand that the format is no longer supported and know to convert my workflow to TOML or YAML


## Implementation Details

### Layer 0: Data (`src/data/`)

#### Drop Markdown Support
- Delete all Markdown parsing code from `workflow_definition.rs`. This includes the Markdown parser function, any regex-based step extraction, and any `WorkflowFormat::Markdown` variant.
- Delete any test fixtures (`.md` files under `tests/`) that test Markdown workflow parsing.
- Update `WorkflowDefinition::from_file(path)` to: if the file extension is `.md`, return a `WorkflowError::MarkdownNoLongerSupported { path }` error with a user-readable message: `"Markdown workflow files are no longer supported. Convert to TOML (.toml) or YAML (.yaml/.yml). See docs/04-workflows.md for the current format."` Do not attempt to parse the file.
- The `WorkflowFormat` enum retains only `Toml` and `Yaml` variants.

#### New Setup/Teardown Step Types
- Add the following types to `src/data/workflow_definition.rs`:

```rust
pub enum SetupStep {
    CloneRepo { url: String, branch: Option<String>, into: Option<String> },
    CheckoutCreateBranch { branch: String, base: Option<String> },
    PullBranch { remote: Option<String>, branch: Option<String> },
    RunShell { command: String, env: Option<HashMap<String, String>> },
    RunScript { path: String, env: Option<HashMap<String, String>> },
}

pub enum TeardownStep {
    RunShell { command: String, env: Option<HashMap<String, String>> },
    RunScript { path: String },
    CommitChanges { message: String, add_all: bool },
    CreatePullRequest { title: String, body: Option<String>, base: Option<String> },
    PushBranch { remote: Option<String>, branch: Option<String> },
}
```

- These are Layer 0 data types: pure data, no execution logic. They must be serializable via `serde` for both TOML and YAML.

#### Updated WorkflowDefinition
- `WorkflowDefinition` gains two new optional fields:
  ```rust
  pub setup: Vec<SetupStep>,       // default: empty vec
  pub teardown: Vec<TeardownStep>, // default: empty vec
  ```
- `teardown_on_failure: bool` (default: `false`) — if true, teardown runs even when the workflow fails. If false, teardown is skipped on failure. This is a top-level workflow config field.
- Update TOML and YAML parsers to handle these new fields. Use `#[serde(default)]` so existing workflows without setup/teardown parse correctly.

#### WorkflowState Schema Update
This work item makes several additions to `WorkflowState` in Layer 0. WI 0079 adds further fields — **coordinate with WI 0079 to make all schema changes in a single version bump** and avoid bumping `WORKFLOW_STATE_SCHEMA_VERSION` twice.

Fields added by this work item:
```rust
pub current_phase: WorkflowPhase,    // enum: Setup, Main, Teardown
pub setup_completed: bool,
pub teardown_completed: bool,
pub setup_step_states: Vec<PhaseStepState>,
pub teardown_step_states: Vec<PhaseStepState>,
```
where:
```rust
pub enum WorkflowPhase { Setup, Main, Teardown }

pub struct PhaseStepState {
    pub description: String,
    pub status: PhaseStepStatus,
}

pub enum PhaseStepStatus {
    Pending,
    Running,
    Succeeded,
    Failed { error: String },
}
```

Fields added by WI 0079 (listed here for coordination awareness):
```rust
pub steps: Vec<WorkflowStepInfo>,  // definition-level metadata for remote rendering
```

- `setup_step_states` is initialized from `workflow.setup` at the start of `run_setup`: one `PhaseStepState { description: step_description(&step), status: Pending }` per setup step. `WorkflowEngine` updates each entry's status as steps execute and persists state after each transition.
- `teardown_step_states` is initialized analogously from `workflow.teardown` at the start of `run_teardown`.
- `step_description(step: &SetupStep) -> String` and its teardown equivalent are pure functions in Layer 1 (`step_commands.rs`) that produce a human-readable label, e.g. `"clone_repo: https://github.com/org/repo"`, `"run_shell: cargo test"`, `"create_pull_request: feat: my feature"`.
- These fields allow the API workflow status endpoint (see WI 0079) to surface setup/teardown step progress to polling clients, and allow the TUI workflow strip to display setup and teardown pseudo-steps alongside main workflow steps.
- On resumption of an interrupted workflow (server restart mid-setup): `setup_step_states` and `teardown_step_states` are restored from disk. Steps in `Running` status are reset to `Pending` (same crash recovery as main steps).
- `current_phase` defaults to `Main` when loading an old-format state file that lacks this field, so pre-existing in-progress workflows complete correctly.
- Bump `WORKFLOW_STATE_SCHEMA_VERSION` once (coordinated with WI 0079).

#### TOML Example (for documentation)
```toml
# For remote sessions, the repo is already cloned by GitEngine.
# Setup steps handle branch preparation and supplementary operations.
name = "implement-feature"
teardown_on_failure = true

[[setup]]
type = "checkout_create_branch"
branch = "feature/my-thing"

[[setup]]
type = "run_shell"
command = "cargo fetch"

[[steps]]
name = "implement"
prompt = "Implement the feature described in SPEC.md"

[[teardown]]
type = "run_shell"
command = "cargo test"

[[teardown]]
type = "create_pull_request"
title = "feat: implement my feature"
body = "Automated PR from awman workflow"
```

### Layer 1: Engine (`src/engine/`)

#### BackgroundContainer (new type in `ContainerRuntime`)
- Add a `BackgroundContainer` type to `src/engine/container/` (alongside the existing container runtime types).
- `BackgroundContainer` represents a long-running base container that accepts `exec` calls for individual commands. It wraps a running container ID and a reference to the backend, so all operations are runtime-agnostic — both Docker and Apple Containers are supported through the existing `ContainerBackend` trait.
- API:
  ```rust
  impl ContainerRuntime {
      pub async fn start_background(
          &self,
          image: &str,
          workdir: &Path,
          env: &HashMap<String, String>,
          overlays: &[OverlaySpec],
      ) -> Result<BackgroundContainer>
  }

  impl BackgroundContainer {
      pub async fn exec(
          &self,
          command: &str,
          env: Option<&HashMap<String, String>>,
      ) -> Result<ExecOutput>

      pub async fn kill(self) -> Result<()>
  }

  pub struct ExecOutput {
      pub stdout: String,
      pub stderr: String,
      pub exit_code: i32,
  }
  ```
- `start_background` starts the base container image with:
  - The session working directory mounted (same mount scope and security constraints as agent containers — only the session `working_dir()` or git root, never parent directories)
  - All overlays from `overlays` applied (same mechanism used for agent containers — env vars injected, directories mounted, secrets passed in). The `OverlaySpec` slice is constructed by `OverlayEngine` in Layer 1 and passed in from Layer 2 (`ExecWorkflowCommand`) before calling `start_background`.
  - An idle entrypoint (e.g. `sleep infinity`) so the container stays alive for the duration of the phase
  - Detached mode (`-d` flag) — the container runs in the background, not attached to a terminal
- `exec` runs a shell command inside the already-running container via the runtime's exec command. Streams stdout and stderr, captures exit code.
- `kill` stops and removes the background container. Must be called even if exec steps fail — the caller (`ExecWorkflowCommand` in Layer 2) is responsible for ensuring `kill` is always called, using Rust's `Drop` or an explicit guard pattern.
- `BackgroundContainer` should implement `Drop` to attempt a best-effort kill if not already killed, logging a warning if the kill fails at drop time.

#### ContainerBackend Trait Extensions
The existing `ContainerBackend` trait (`src/engine/container/backend.rs`) gains three new methods to support background containers. Both `DockerBackend` and `AppleBackend` must implement them.

```rust
pub(super) trait ContainerBackend: Send + Sync {
    // ... existing methods ...

    fn start_background(
        &self,
        options: ResolvedContainerOptions,
    ) -> Result<String, EngineError>;

    fn exec_in_background(
        &self,
        container_id: &str,
        command: &str,
        working_dir: &str,
        env: Option<&HashMap<String, String>>,
    ) -> Result<ExecOutput, EngineError>;

    fn stop_and_remove(
        &self,
        container_id: &str,
    ) -> Result<(), EngineError>;
}
```

- `start_background` builds and spawns the container in detached mode with an idle entrypoint. Returns the container ID string. The `ResolvedContainerOptions` carry the image, mounts, env vars, overlays, and container name — same resolution path as agent containers.
- `exec_in_background` runs a command inside the container and captures stdout, stderr, and exit code. This is distinct from the existing `exec_args()` method, which only builds argv fragments for interactive exec — `exec_in_background` spawns the process, waits for completion, and returns structured output.
- `stop_and_remove` stops the container (sends SIGTERM, waits, then SIGKILL) and removes it. Equivalent to the existing `stop()` on `ContainerBackend` but also removes the container in a single call to ensure cleanup.

**Docker implementation** (`DockerBackend`):
| Operation | Command |
|-----------|---------|
| Start background | `docker run -d --name <name> -v <workdir>:<mount> -e KEY=VAL <image> sleep infinity` |
| Exec | `docker exec -w <dir> -e KEY=VAL <container_id> sh -c "<command>"` |
| Stop + remove | `docker stop <name> && docker rm <name>` |
| ID retrieval | `docker run -d` prints the container ID to stdout |

**Apple Containers implementation** (`AppleBackend`):
| Operation | Command |
|-----------|---------|
| Start background | `container run -d --name <name> -v <workdir>:<mount> -e KEY=VAL <image> sleep infinity` |
| Exec | `container exec -w <dir> -e KEY=VAL <container_id> sh -c "<command>"` |
| Stop + remove | `container stop <name> && container rm <name>` |
| ID retrieval | `container run -d` prints the container ID to stdout |

Both runtimes use identical argv structure for these operations — the only difference is the CLI binary name (`docker` vs `container`), obtained via `ContainerRuntime::cli_binary()`. The `build_run_argv` helper in `docker.rs` (already shared by `AppleContainerInstance`) can be extended to support the `-d` flag and idle entrypoint for background containers.

**Per-exec env var injection**: Both runtimes support `-e KEY=VAL` flags on exec. Container-level env vars (set at start time via `--env` on the run command) are inherited by all exec calls automatically. Per-step `env` overrides (from `RunShell { env }`) are passed as additional `-e` flags on the exec call — they are additive, not replacements.

#### Step-to-Command Translation (in Layer 1)
- Add a pure function in Layer 1 (e.g. `src/engine/workflow/step_commands.rs`) that translates `SetupStep` and `TeardownStep` values into shell command strings. This is a stateless mapping with no external dependencies — it belongs in Layer 1 where it is used:
  ```rust
  pub fn setup_step_to_shell(step: &SetupStep) -> (String, Option<HashMap<String, String>>)
  pub fn teardown_step_to_shell(step: &TeardownStep) -> (String, Option<HashMap<String, String>>)
  ```
- Translations:
  - `CloneRepo { url, branch: Some(b), into: Some(d) }` → `git clone -b <b> <url> <d>`
  - `CloneRepo { url, branch: None, into: None }` → `git clone <url>`
  - `CheckoutCreateBranch { branch, base: Some(b) }` → `git fetch origin 2>/dev/null; git checkout -B <branch> <b> 2>/dev/null || git checkout <branch> && git pull origin <branch> 2>/dev/null || true` — attempts to fetch from remote (ignoring failure if no remote exists), then either creates the branch from `<b>` or checks out and pulls the existing remote branch; if all remote operations fail, the branch is created locally from `<b>`
  - `CheckoutCreateBranch { branch, base: None }` → `git fetch origin 2>/dev/null; git checkout <branch> 2>/dev/null && git pull origin <branch> 2>/dev/null || git checkout -b <branch>` — attempts to fetch, then checks out and pulls the remote branch if it exists; if the remote is unavailable or the branch doesn't exist remotely, creates it locally from HEAD
  - `PullBranch { remote: Some(r), branch: Some(b) }` → `git pull <r> <b>`
  - `PullBranch { .. }` → `git pull` (uses git defaults)
  - `RunShell { command, env }` → pass command as-is, merge env
  - `RunScript { path, env }` → `sh <path>`, merge env
  - `CommitChanges { message, add_all: true }` → `git add -A && git commit -m "<message>"`
  - `CommitChanges { message, add_all: false }` → `git commit -m "<message>"`
  - `PushBranch { remote: Some(r), branch: Some(b) }` → `git push <r> <b>`
  - `PushBranch { .. }` → `git push` (uses git defaults)
  - `CreatePullRequest { title, body: Some(b), base: Some(base) }` → `gh pr create --title "<title>" --body "<b>" --base <base>`
  - `CreatePullRequest { title, body: None, base: None }` → `gh pr create --title "<title>"`

#### WorkflowEngine Setup/Teardown Methods
- `WorkflowEngine` gains:
  ```rust
  pub async fn run_setup(
      &self,
      steps: &[SetupStep],
      container: &BackgroundContainer,
      frontend: &dyn WorkflowFrontend,
  ) -> Result<()>

  pub async fn run_teardown(
      &self,
      steps: &[TeardownStep],
      workflow_succeeded: bool,
      teardown_on_failure: bool,
      container: &BackgroundContainer,
      frontend: &dyn WorkflowFrontend,
  ) -> Result<()>
  ```
- `run_teardown` skips all steps and returns `Ok(())` if `!teardown_on_failure && !workflow_succeeded`.
- For each step, `WorkflowEngine`:
  1. Translates the step to a shell command using `setup_step_to_shell` / `teardown_step_to_shell`
  2. Calls `frontend.on_setup_step_started(description)` (or teardown equivalent) for UI output
  3. Calls `container.exec(command, env)` to run it in the background container
  4. Streams each output line via `frontend.on_setup_step_output(line)` (or teardown equivalent)
  5. On non-zero exit code: calls `frontend.on_setup_step_failed(description, exit_code, stderr)` and returns `Err`
  6. On success: calls `frontend.on_setup_step_completed(description)`
- For setup: if any step fails, `run_setup` returns `Err` immediately (no further steps run).
- For teardown: if any step fails, log the error via the frontend and continue to the next step (best-effort).

#### WorkflowFrontend Trait Extensions
- Add output-notification methods to `WorkflowFrontend` (these are display-only — no execution delegation):
  ```rust
  fn on_setup_step_started(&self, description: &str);
  fn on_setup_step_output(&self, line: &str);
  fn on_setup_step_completed(&self, description: &str);
  fn on_setup_step_failed(&self, description: &str, exit_code: i32, stderr: &str);

  fn on_teardown_step_started(&self, description: &str);
  fn on_teardown_step_output(&self, line: &str);
  fn on_teardown_step_completed(&self, description: &str);
  fn on_teardown_step_failed(&self, description: &str, exit_code: i32, stderr: &str);
  ```
- Provide default no-op implementations so existing `WorkflowFrontend` implementations compile without modification.

### Layer 2: Command (`src/command/`)

#### ExecWorkflowCommand — Three-Phase Coordination
`ExecWorkflowCommand::run_with_frontend(...)` is updated to orchestrate three phases. Layer 2 owns the container lifecycle for setup and teardown:

```
1. SETUP PHASE (if workflow.setup is non-empty):
   a. Resolve overlays via OverlayEngine:
      let overlays = overlay_engine.resolve_overlays(&session)?;
   b. Use ContainerRuntime to start a BackgroundContainer:
      container_runtime.start_background(base_image, &session.working_dir(), &env, &overlays)
   c. Call workflow_engine.run_setup(&workflow.setup, &setup_container, frontend)
   d. Call setup_container.kill() — always, even if run_setup returned Err
   e. If run_setup returned Err, abort: do not run the main phase

2. MAIN PHASE:
   Existing workflow step execution, unchanged.

3. TEARDOWN PHASE (if workflow.teardown is non-empty):
   a. Resolve overlays again (config may have changed during the workflow, though
      in practice this is the same call — resolve fresh for correctness)
   b. Start a new BackgroundContainer for teardown (same image, same workdir, same overlays)
   c. Call workflow_engine.run_teardown(&workflow.teardown, succeeded,
        workflow.teardown_on_failure, &teardown_container, frontend)
   d. Call teardown_container.kill() — always
```

- Use a guard/defer pattern (or `scopeguard` crate) to ensure `container.kill()` is called even if an early return occurs due to error. Do not rely on `Drop` alone as container cleanup failure should be logged explicitly.
- `ExecWorkflowCommand` reads the base image name from `EffectiveConfig` (which sources it from global config, repo config, or a default constant). The base image config key is `base_image` — add to `GlobalConfig` and `RepoConfig` in Layer 0 with a default value matching the built `Dockerfile.dev` image tag.
- Overlays applied to setup/teardown containers must be identical to those applied to agent containers for the same session. The `OverlayEngine::resolve_overlays(&session)` call is the single source of truth — do not construct overlays ad-hoc in the setup/teardown path. This ensures that environment variables, secrets, mounted directories, and agent settings configured for the project are all available inside setup and teardown containers.

#### What Layer 2 Does NOT Do
- Layer 2 does NOT translate step types to shell commands — that happens in Layer 1 (`step_commands.rs`).
- Layer 2 does NOT exec into the container directly — that is `BackgroundContainer::exec` in Layer 1.
- Layer 2 does NOT implement git operations for setup/teardown steps — those are shell commands exec'd inside the container (e.g. `git clone`, `git checkout`), not calls to `GitEngine`. `GitEngine` is for awman's own git lifecycle operations on the host (cloning/deleting remote sessions), not for user-defined workflow step execution.

#### Remote Sessions: Repo Already Provisioned by GitEngine
- For `type: remote` sessions (see WI 0079), `GitEngine` clones the repository and checks out the requested branch at session creation time, before any workflow runs. **The session's `working_dir()` already points to a fresh, isolated clone when setup steps begin.**
- A `clone_repo` setup step is therefore redundant for remote sessions and should not be used for primary repo provisioning. It remains valid for cloning *additional* repositories needed by the workflow into subdirectories (e.g. a shared config repo or test fixtures repo).
- Document this clearly in `docs/04-workflows.md`: for `type: remote` sessions, repo provisioning is handled by the session's `repo_url` and `branch` fields via `GitEngine`; setup steps are for supplementary operations (branch checkout, dependency installation, config generation, additional repo clones, etc.).

### Layer 3: Frontend (`src/frontend/`)
- No new business logic. All three frontends (CLI, TUI, API) must implement the new `WorkflowFrontend` output methods added above.
- CLI: print setup/teardown step descriptions and output lines to stdout, formatted similarly to main workflow step output.
- TUI: render setup/teardown step output in the execution window, similar to main workflow steps.
- API / `ApiDispatchFrontend`: write setup/teardown output to the command's output log via the `EventBus` (same as main workflow output). Per WI 0079, the queue worker reuses `ApiDispatchFrontend` directly — there is no separate `QueueWorkerFrontend`.
- Since default no-op implementations are provided on the trait, frontends that do not yet implement these methods will compile — but all frontends should implement them for a complete user experience.


## Edge Case Considerations

- **Overlays not applied to setup/teardown container**: If `OverlayEngine::resolve_overlays` fails (e.g. a referenced overlay directory does not exist), treat this as a setup phase failure — do not start the background container. Return a clear error naming the missing overlay resource.
- **Overlay env vars in exec'd commands**: Overlays may inject environment variables into the container. These are set at container start time (via `--env` / `-e` flags on the runtime's `run` command), not per-exec. All `exec` calls into the same `BackgroundContainer` instance inherit those env vars automatically. Per-step `env` overrides (from `RunShell { env }`) are passed as additional `-e` flags on the `exec` call and are additive to, not replacements of, the container-level overlay env vars. This behavior is identical for both Docker and Apple Containers.
- **Setup container startup failure**: If `ContainerRuntime::start_background` fails (e.g. image not found, container runtime unavailable), the setup phase fails immediately with a clear error. The main workflow does not run. The command lands in `'error'` status (API mode, per WI 0079 status values).
- **Base image not built**: If the configured base image tag does not exist locally, the container runtime will return a "no such image" error (both Docker and Apple Containers return this). `start_background` must surface this as a `ContainerError::ImageNotFound { image }` with a message directing the user to run `make build` or the equivalent build command for the base image.
- **Container killed mid-exec**: If `BackgroundContainer::kill` is called while an `exec` is in progress (e.g. due to a timeout or user interruption), the in-flight exec should be allowed to surface as an error naturally. The kill cleans up the container; the exec error propagates as a step failure.
- **Setup failure and teardown**: If the setup phase fails, `teardown_on_failure` still applies. If `true`, a teardown container is started and teardown runs (useful for cleanup). If `false`, teardown is also skipped.
- **Teardown step failure — best-effort**: A failing teardown step (non-zero exit code) is logged via `on_teardown_step_failed` and execution continues to the next teardown step. Teardown failure does NOT retroactively change the workflow's success/failure status.
- **Workdir mount scope**: The background container must be mounted to the session's `working_dir` only. If `working_dir` is inside a git repo whose root is a parent directory, and the full repo root is needed (e.g. to run `git` commands that reference the root), follow the existing mount-scope prompt behavior: prompt the user (or in API/queue mode, use the configured mount scope from session config) before mounting parent directories.
- **Idempotent setup steps**: If a setup phase is interrupted and re-run, steps like `CloneRepo` will encounter an already-populated directory. The shell command `git clone` will fail in that case. Document that setup steps must be written to be idempotent (e.g. `git clone <url> || true` or a conditional check) and that awman re-runs the full setup phase on resume. `CheckoutCreateBranch` is idempotent by design — re-running it will check out the branch whether or not it already exists locally.
- **CheckoutCreateBranch without a git remote**: `CheckoutCreateBranch` attempts `git fetch origin` but treats fetch failure as non-fatal (stderr suppressed, exit code ignored via `;` not `&&`). If no `origin` remote is configured or the fetch fails (e.g. network unavailable), the step falls back to local-only branch creation — it will check out the branch if it exists locally, or create it from HEAD (or from `base` if specified). For remote sessions this fallback is unlikely since `GitEngine` sets up the clone with `origin`, but for local sessions with no remote configured the step still succeeds.
- **PullBranch on a detached HEAD**: `PullBranch` with no arguments runs `git pull`, which requires a tracking branch. If the current HEAD is detached (e.g. after a `CheckoutCreateBranch` that just created a new local branch with no upstream), `git pull` will fail. Users should either specify `remote` and `branch` explicitly or ensure the branch has an upstream configured.
- **CreatePullRequest step — `gh` CLI availability**: The base container image must have `gh` (GitHub CLI) installed for `CreatePullRequest` steps to work. If `gh` is not in the image, the exec will fail with a clear error. Document the requirement.
- **CloneRepo into a non-empty workdir**: For remote sessions, the workdir already contains the primary repo clone (provisioned by `GitEngine`). A `CloneRepo` setup step cloning an additional repo must use the `into` field to specify a subdirectory, or `git clone` will fail because the workdir is not empty. For local sessions with an empty temp workdir, cloned repos land inside the workdir root; subsequent steps must reference relative paths correctly.
- **Markdown detection**: Files with `.md` extension must return `WorkflowError::MarkdownNoLongerSupported` regardless of their content. Do not attempt heuristic format detection.
- **Existing workflows without setup/teardown**: `#[serde(default)]` on the new fields ensures all existing TOML/YAML workflows parse correctly with empty `setup` and `teardown` vecs.


## Test Considerations

- **Markdown rejection test**: Attempt to load a `.md` file as a workflow; assert `WorkflowError::MarkdownNoLongerSupported` is returned.
- **TOML parse with setup/teardown test**: Parse the example TOML above; assert `setup.len() == 2` (one `CheckoutCreateBranch`, one `RunShell`), `teardown.len() == 2`, `teardown_on_failure == true`.
- **YAML parse with setup/teardown test**: Equivalent for YAML format.
- **Existing workflow backward compat test**: Parse a pre-existing fixture with no setup/teardown; assert `setup` and `teardown` vecs are empty and parsing succeeds.
- **step_commands unit tests**: For each `SetupStep` and `TeardownStep` variant, assert `setup_step_to_shell` / `teardown_step_to_shell` returns the expected command string and env map.
- **Overlays applied to background container test**: Provide a mock `OverlayEngine` that returns a known env var overlay (`FOO=bar`); start a `BackgroundContainer`; exec `printenv FOO`; assert stdout is `"bar\n"`.
- **Overlay resolve failure test**: If `OverlayEngine::resolve_overlays` returns an error, assert that `start_background` is never called and the setup phase returns an appropriate error.
- **BackgroundContainer start/exec/kill integration test**: Requires a container runtime (Docker or Apple Containers). Start a background container, exec a simple command (`echo hello`), assert stdout is `"hello\n"` and exit code is 0. Kill the container, assert the container no longer appears in the runtime's container listing.
- **BackgroundContainer exec non-zero exit test**: Exec `exit 1` inside a background container; assert `ExecOutput.exit_code == 1`.
- **WorkflowEngine run_setup unit test**: Provide a mock `BackgroundContainer` that records exec calls and a mock `WorkflowFrontend`. Assert each setup step is translated to the correct command and exec'd in order.
- **WorkflowEngine run_setup abort on failure test**: Mock the second exec to return exit code 1; assert `run_setup` returns `Err` and the third step is never exec'd.
- **WorkflowEngine run_teardown skip on failure test**: `teardown_on_failure = false`, `workflow_succeeded = false` → assert no exec calls are made.
- **WorkflowEngine run_teardown continues after step failure test**: Mock the first teardown exec to return exit code 1; assert the second teardown step is still exec'd.
- **ExecWorkflowCommand container lifecycle test**: Use a mock `ContainerRuntime`. Assert: setup container started before `run_setup`, `kill()` called on setup container after setup (even if setup fails), teardown container started before `run_teardown`, `kill()` called after teardown.
- **WorkflowState phase persistence test**: After setup completes, assert `WorkflowState` is saved with `current_phase = Main` and `setup_completed = true`.
- **Setup step state tracking test**: Provide a workflow with 2 setup steps. Run `WorkflowEngine::run_setup` against a mock `BackgroundContainer`. After the first step executes, assert `WorkflowState.setup_step_states[0].status == Succeeded` and `setup_step_states[1].status == Pending`. After both complete, assert both are `Succeeded`.
- **Teardown step state tracking test**: Equivalent — assert `teardown_step_states` entries update correctly as each step executes.
- **Setup step failure state test**: If the second setup step fails, assert `setup_step_states[1].status == Failed { error: "..." }` with the stderr content in the error field.
- **step_description unit test**: For each `SetupStep` and `TeardownStep` variant, assert `step_description` returns the expected human-readable string.
- **WorkflowState backward compat — missing phase fields**: Load a fixture `WorkflowState` JSON that lacks `current_phase`, `setup_step_states`, and `teardown_step_states`; assert deserialization succeeds, `current_phase` defaults to `Main`, and the vec fields default to empty.
- **Base image not found test**: Configure a nonexistent image name; assert `start_background` returns `ContainerError::ImageNotFound` with the image name in the message.


## Codebase Integration

- Strictly follow `aspec/architecture/2026-grand-architecture.md`. `SetupStep`, `TeardownStep`, `WorkflowDefinition`, and `WorkflowState` updates are Layer 0. `BackgroundContainer`, `ExecOutput`, `setup_step_to_shell`, `teardown_step_to_shell`, and `WorkflowEngine::run_setup/run_teardown` are Layer 1. Container lifecycle management (start/kill for each phase), overlay resolution, and phase sequencing live in `ExecWorkflowCommand` at Layer 2. Layer 3 implements `WorkflowFrontend` output methods for display.
- `WorkflowEngine` (Layer 1) uses `BackgroundContainer` (also Layer 1) — same-layer interaction is permitted. `WorkflowEngine` does NOT hold a reference to `ContainerRuntime` itself; it receives a pre-started `BackgroundContainer` from Layer 2.
- No shell commands are ever executed on the host by the awman server process. All setup and teardown execution goes through `BackgroundContainer::exec`. This is a security constraint, not a suggestion.
- Delete all Markdown parsing code completely — no `#[deprecated]` wrappers, no feature flags.
- The `base_image` config key must be added to `GlobalConfig` and `RepoConfig` in Layer 0, with a sensible default (the tag produced by `make build` for `Dockerfile.dev`). Consult existing image tag conventions in the codebase.
- `WORKFLOW_STATE_SCHEMA_VERSION` must be bumped. Provide an upgrade path: old state files without `current_phase` default to `WorkflowPhase::Main`.


## Documentation

After implementation:
- `docs/04-workflows.md` — full rewrite: TOML/YAML-only format, `setup` and `teardown` sections with all step types and their fields, `teardown_on_failure`, container execution model, idempotency note, base image requirement for `gh` CLI; remove all Markdown format documentation and add migration notice
- `docs/08-api-mode.md` — end-to-end example of an API remote-session workflow showing that the repo is provisioned by `GitEngine` at session creation (via `repo_url` and `branch`), with setup steps for supplementary operations (e.g. dependency installation) and `create_pull_request` teardown
- `docs/07-configuration.md` — document the `base_image` config option
