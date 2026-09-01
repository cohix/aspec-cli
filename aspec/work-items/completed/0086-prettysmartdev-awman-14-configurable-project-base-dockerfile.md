# Work Item: Task

Title: Configurable project-base Dockerfile and config paths
Issue: https://github.com/prettysmartdev/awman/issues/14

## Summary:
- awman hardcodes `$GITROOT/Dockerfile.dev` as the only valid path for the project base image. This path should be configurable in `.awman/config.json`.
- A codebase audit must identify other hardcoded file paths that are not internal awman state (i.e. not within `.awman/` or `~/.awman/`) and determine if any additional paths should become configurable.
- awman should honour `XDG_` base-directory environment variables when resolving the global `~/.awman/` directory, so that users on XDG-compliant systems can control where awman stores global state.
- During `awman init`, when no project base Dockerfile is found, awman should offer the user a choice: create a new `Dockerfile.dev` from the bundled template, or select an existing Dockerfile in the repo to use as the base (saving the chosen path to `.awman/config.json`).


## User Stories

### User Story 1:
As a: user

I want to:
place my project's base `Dockerfile` at a path other than `Dockerfile.dev` in the repo root (e.g. `docker/Dockerfile.base`)

So I can:
organise my repo's Docker assets however my project conventions require, without awman overriding that choice or refusing to find the file.

### User Story 2:
As a: user

I want to:
set `XDG_DATA_HOME` (or `XDG_STATE_HOME`) in my shell and have awman automatically place its global directory under those paths

So I can:
consolidate all tool data directories under my preferred XDG hierarchy instead of accumulating stray `~/.awman` directories.

### User Story 3:
As a: user

I want to:
get a clear, accurate warning if awman cannot find the configured Dockerfile path

So I can:
debug misconfigured `dockerfile` keys in `.awman/config.json` without silently falling back to the wrong build context.

### User Story 4:
As a: user running `awman init` on a repo that has no `Dockerfile.dev`

I want to:
be asked whether to create a new `Dockerfile.dev` from the default template or point awman at an existing Dockerfile already in the repo

So I can:
set up awman without being forced to adopt the `Dockerfile.dev` naming convention if my project already has a Dockerfile at a different path, and have that choice persisted in `.awman/config.json` automatically.


## Implementation Details:

### 1. Configurable project-base Dockerfile path

**Config schema addition — `src/data/config/repo.rs`**

Add a `dockerfile` field to `RepoConfig`:

```rust
#[serde(rename = "dockerfile", skip_serializing_if = "Option::is_none")]
pub dockerfile: Option<String>,
```

- JSON key: `"dockerfile"`
- Value: path relative to `git_root`, or absolute. Mirrors the pattern used by `WorkItemsConfig.dir`.
- Default (when absent or empty): `Dockerfile.dev` at `git_root` (existing behavior, no regression).

Add resolver helpers to `RepoConfig`:

```rust
/// Resolve the configured Dockerfile path relative to `git_root`.
pub fn dockerfile_path(&self, git_root: &Path) -> Option<PathBuf> { … }

/// Resolve with fallback to `<git_root>/Dockerfile.dev`.
pub fn dockerfile_path_or_default(&self, git_root: &Path) -> PathBuf { … }
```

**Update `RepoDockerfilePaths` — `src/data/repo_dockerfile_paths.rs`**

Add a constructor variant or builder that accepts an optional override path:

```rust
pub fn with_project_dockerfile(git_root: impl Into<PathBuf>, path: Option<PathBuf>) -> Self { … }
```

`project_dockerfile()` returns the override when provided, otherwise `<git_root>/Dockerfile.dev`.

**Callsite updates**

- `src/command/commands/exec_workflow.rs:1299` — `session.git_root().join("Dockerfile.dev")` passed to `detect_home_from_dockerfile(…)`: replace with `repo_config.dockerfile_path_or_default(git_root)`.
- Anywhere `RepoDockerfilePaths::new(git_root)` is constructed and then `project_dockerfile()` is called: thread the resolved `dockerfile` path from the loaded `RepoConfig` into the constructor.

### 2. Codebase audit findings

The audit identified one additional hardcoded project-level path requiring a fix, and several that are intentional awman conventions:

**Requires fix (same as §1 above)**
- `src/data/repo_dockerfile_paths.rs:24-25` — `project_dockerfile()` always returns `<git_root>/Dockerfile.dev`.
- `src/command/commands/exec_workflow.rs:1299` — inline `git_root.join("Dockerfile.dev")`.

**Intentional awman conventions — no change needed**
- `<git_root>/.awman/Dockerfile.<agent>` — per-agent Dockerfiles. These are awman-managed files and not user-authored; their location is part of awman's own protocol.
- `<git_root>/.awman/config.json` and `<git_root>/.awman/workflows/` — internal awman state directories. Already under the `.awman/` subdir convention.
- `<git_root>/aspec/work-items/` — already configurable via `workItems.dir` in `RepoConfig`.
- `<git_root>/.claude/skills/` — Claude CLI convention; not awman's to configure.
- Auth paths in `src/data/fs/auth_paths.rs` — agent-tool convention paths (e.g. `~/.claude.json`). These belong to the agent tools, not awman.

### 3. XDG base-directory support

**Background**

awman currently places all global state under `~/.awman/`. The env var `AWMAN_CONFIG_HOME` already overrides the entire global home directory. XDG support should slot in between `AWMAN_CONFIG_HOME` and the `~/.awman` fallback, using the appropriate XDG variable per directory type.

**Proposed XDG variable mapping**

| Content | Current path | XDG override (if set) |
|---------|-------------|----------------------|
| Global config (`config.json`) | `~/.awman/config.json` | `$XDG_CONFIG_HOME/awman/config.json` |
| All other data (workflows, skills, worktrees, API state) | `~/.awman/{…}` | `$XDG_DATA_HOME/awman/{…}` |

**Precedence chain (highest → lowest)**

1. `AWMAN_CONFIG_HOME` — existing override, unchanged.
2. `XDG_CONFIG_HOME` (config only) / `XDG_DATA_HOME` (everything else) — new.
3. `~/.awman/` — existing fallback.

**`AWMAN_CONFIG_HOME` semantics remain unchanged**: when set, it overrides *all* global awman paths (config + data + state) exactly as today. XDG vars only apply when `AWMAN_CONFIG_HOME` is absent.

**Implementation**

Add XDG variable constants to `src/data/config/env.rs`:

```rust
pub const XDG_CONFIG_HOME: &str = "XDG_CONFIG_HOME";
pub const XDG_DATA_HOME: &str = "XDG_DATA_HOME";
```

Extend `EnvSnapshot` with typed accessors:

```rust
pub fn xdg_config_home(&self) -> Option<PathBuf> { … }
pub fn xdg_data_home(&self) -> Option<PathBuf> { … }
```

Update `Env::from_process()` to capture these two additional keys.

Update `GlobalConfig::home_dir_with(env)` in `src/data/config/global.rs`:

```
AWMAN_CONFIG_HOME → as-is
XDG_CONFIG_HOME   → <xdg_config_home>/awman
fallback          → $HOME/.awman
```

Update `GlobalConfig::data_home_with(env)` (new helper) used by workflow, skill, worktree, and API directory resolution:

```
AWMAN_CONFIG_HOME → as-is (keeps everything together)
XDG_DATA_HOME     → <xdg_data_home>/awman
fallback          → $HOME/.awman
```

Update `ApiPaths::root_with(env)` in `src/data/fs/api_paths.rs` to use `data_home_with` rather than a separate env var:

```
AWMAN_API_ROOT    → as-is (existing override, takes precedence)
XDG_DATA_HOME     → <xdg_data_home>/awman/api
fallback          → $HOME/.awman/api
```

`AWMAN_CONFIG_HOME` must continue to override all three when set, so that the existing test isolation pattern (`EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp_dir)])`) keeps working without changes to any existing tests.

XDG vars are only captured — never written — by awman. awman never modifies the user's shell environment.


### 4. `awman init` — Dockerfile setup decision flow

**Background**

The current `InitPhase::SettingUpDockerfile` phase silently creates `Dockerfile.dev` from the bundled template whenever the file is absent. With configurable Dockerfile paths (§1), the engine must instead ask the user what to do so the chosen path can be persisted to `.awman/config.json`.

The `init` command is exposed on the CLI frontend and TUI frontend; the API frontend does not support `init` (already N/A).

**New decision type — `src/engine/init/frontend.rs`**

```rust
pub enum DockerfileSetupDecision {
    /// Create `Dockerfile.dev` from the bundled template.
    CreateNew,
    /// Use an existing Dockerfile at this path (relative to git_root or absolute).
    UseExisting(String),
    /// User dismissed without choosing — skip Dockerfile setup entirely.
    Skip,
}
```

**New `InitFrontend` trait method**

Add to `InitFrontend` in `src/engine/init/frontend.rs`:

```rust
/// Called when no project-base Dockerfile is found during init.
/// Returns the user's choice of how to proceed.
fn ask_dockerfile_setup(&mut self, git_root: &std::path::Path) -> Result<DockerfileSetupDecision, EngineError>;
```

The `git_root` argument is provided so frontends can resolve and validate user-supplied paths before returning.

**Phase state machine changes — `src/engine/init/phase.rs`**

Add two new phases to `InitPhase`:

```rust
/// Shown when no project-base Dockerfile exists; waits for user's decision.
AwaitingDockerfileDecision,
/// Saves the user-chosen existing Dockerfile path to the repo config.
SavingDockerfileConfig,
```

Updated `SettingUpDockerfile` transitions:
- File **exists** at the resolved path → `summary.dockerfile = Done` → proceed to `SettingUpAgentDockerfile` (unchanged).
- File **absent** → transition to `AwaitingDockerfileDecision`.

`AwaitingDockerfileDecision` handler in `InitEngine::step()`:
- Calls `frontend.ask_dockerfile_setup(git_root)`.
- `CreateNew` → go to `SettingUpDockerfile` (which now only handles the creation branch; the existence-check branch skips directly to `SettingUpAgentDockerfile`).
- `UseExisting(path)` → store `path` in a new `engine.pending_dockerfile_path: Option<String>` field → go to `SavingDockerfileConfig`.
- `Skip` → `summary.dockerfile = Skipped` → go to `SettingUpAgentDockerfile`.

`SavingDockerfileConfig` handler:
- Load existing `RepoConfig` (or create default).
- Set `config.dockerfile = Some(pending_dockerfile_path)`.
- Save to disk.
- `summary.dockerfile = Done`.
- Go to `SettingUpAgentDockerfile`.

`WritingConfig` phase: if `pending_dockerfile_path` is set and the config file does not yet exist, include the `dockerfile` field in the initial config write so only one write occurs.

**CLI frontend impl — `src/frontend/cli/per_command/init.rs`**

When stdin is not a TTY, return `DockerfileSetupDecision::CreateNew` (safe non-interactive default, matching the current silent-create behavior).

When stdin is a TTY, print a numbered menu:

```
awman: No Dockerfile found at <path>. How would you like to proceed?
  [1] Create Dockerfile.dev from the built-in template (recommended)
  [2] Use an existing Dockerfile in this repo
  [3] Skip for now (configure manually in .awman/config.json)
Choice [1]:
```

If the user chooses `[2]`, prompt for a path:

```
awman: Enter the path to your Dockerfile (relative to repo root):
```

Validate that the entered path is non-empty before returning `UseExisting`. If the entered path does not exist on disk at resolution time, print a warning but still return `UseExisting` — the engine's existing "configured path missing" edge case handling takes over from there.

Use `yes_no` / `stdin.read_line` helpers already present in the CLI frontend; add a small `pick_numbered` helper in `src/frontend/cli/per_command/helpers.rs` for the numbered menu pattern if one does not already exist.

**TUI frontend impl — `src/frontend/tui/per_command/init.rs`**

Use two sequential `ask_dialog` calls:

1. `DialogRequest::KindSelect` with three options:
   - `("1", "Create Dockerfile.dev from the built-in template")`
   - `("2", "Use an existing Dockerfile in this repo")`
   - `("3", "Skip for now")`
2. If the user selects option `"2"`, follow immediately with `DialogRequest::TextInput { title: "Dockerfile path", prompt: "Path relative to repo root:", default_text: None }`.
   - If the text response is non-empty → `UseExisting(text)`.
   - If empty or `Dismissed` → fall back to `CreateNew`.
3. Option `"1"` or `Dismissed` on the first dialog → `CreateNew`.
4. Option `"3"` → `Skip`.

**API frontend — N/A**

`init` is not a supported API command. No changes required. The `ask_dockerfile_setup` method should not be added to any API frontend impl; if it is ever reachable from that path an `unreachable!()` or `EngineError::Other("init not supported via API")` is appropriate.


## Edge Case Considerations:

- **Configured `dockerfile` path does not exist**: `ready` and `init` workflows check for Dockerfile existence before building. They must report the exact configured path in the missing-file error or prompt, not the default `Dockerfile.dev`. Do not silently fall back to `Dockerfile.dev` if a non-default path is explicitly configured and missing — surface the error immediately.
- **Absolute vs. relative `dockerfile` path**: Follow the same resolution logic as `WorkItemsConfig.dir` — absolute paths used as-is, relative paths joined to `git_root`.
- **`AWMAN_CONFIG_HOME` set alongside XDG vars**: `AWMAN_CONFIG_HOME` wins unconditionally; XDG vars are ignored when it is present. Document this clearly.
- **XDG vars set to empty string**: Treat the same as unset (do not create `//awman/` paths). Guard with a non-empty check before constructing paths.
- **XDG data vs. config split with `AWMAN_CONFIG_HOME`**: When `AWMAN_CONFIG_HOME` is set, it overrides config, data, and state uniformly (backward-compat). Individual XDG vars only apply when `AWMAN_CONFIG_HOME` is absent.
- **Existing `~/.awman/` directories**: No migration is performed. If a user sets XDG vars after having used awman, the new path starts empty. awman must not attempt to move or merge existing directories.
- **`XDG_DATA_HOME` unset but `XDG_CONFIG_HOME` set**: Only the config path changes; all data paths (including API) fall back to `~/.awman/`. This split-home scenario is valid and must not cause startup errors.
- **`detect_home_from_dockerfile` called with missing configured path**: The function currently reads the Dockerfile to infer the container home directory; it already handles a missing file gracefully (returns `None`). No change needed to its error handling, but callers should log a warning when the configured path is missing.
- **`init` run when Dockerfile already exists at the configured path**: `SettingUpDockerfile` must detect the existing file and skip the `AwaitingDockerfileDecision` phase entirely — no prompt, no overwrite. This preserves current behavior for repos that already have `Dockerfile.dev`.
- **`init` run when a non-default `dockerfile` is already set in `.awman/config.json`**: The engine must resolve the configured path and check for its existence. If it exists, skip the decision prompt. If it does not exist, enter `AwaitingDockerfileDecision` with a message that names the configured path, not `Dockerfile.dev`.
- **User enters a relative path for `UseExisting` that traverses outside the repo** (e.g. `../../other/Dockerfile`): Accept the path as entered — awman does not restrict path values in config — but the user accepts full responsibility. Do not attempt to canonicalize or reject.
- **User chooses `UseExisting` but later deletes the file**: Handled by the same "configured path missing" error path in `ready` / `init`.
- **`Init` run in non-interactive mode (piped stdin / API)**: Return `CreateNew` as the non-interactive default so that `awman init` called from scripts continues to create `Dockerfile.dev` automatically without hanging on a prompt.
- **`SavingDockerfileConfig` phase and `WritingConfig` phase both touch the config file**: Ensure only one disk write occurs. If `pending_dockerfile_path` is set before `WritingConfig` runs, include it in the initial config write and skip `SavingDockerfileConfig`, or ensure the two phases do not race to overwrite each other.


## Test Considerations:

**`RepoConfig` — `src/data/config/repo.rs`**
- `dockerfile_path_or_default` returns `<git_root>/Dockerfile.dev` when field is absent.
- `dockerfile_path_or_default` resolves a relative path against `git_root`.
- `dockerfile_path_or_default` uses an absolute path as-is.
- `dockerfile_path` returns `None` when the field is absent or empty string.
- Round-trip: `"dockerfile": "docker/Dockerfile.base"` survives save → load without modification.

**`RepoDockerfilePaths` — `src/data/repo_dockerfile_paths.rs`**
- `project_dockerfile()` returns override path when one is supplied.
- `project_dockerfile()` returns `<git_root>/Dockerfile.dev` when no override is supplied (existing test must continue to pass).

**`EnvSnapshot` / `Env` — `src/data/config/env.rs`**
- `xdg_config_home()` returns `None` when `XDG_CONFIG_HOME` is absent.
- `xdg_config_home()` returns `None` when `XDG_CONFIG_HOME` is empty string.
- `xdg_data_home()` mirrors the above.
- `Env::from_process()` captures both XDG keys.

**`GlobalConfig` — `src/data/config/global.rs`**
- `home_dir_with` returns `$XDG_CONFIG_HOME/awman` when `XDG_CONFIG_HOME` is set and `AWMAN_CONFIG_HOME` is absent.
- `home_dir_with` returns `AWMAN_CONFIG_HOME` when both `AWMAN_CONFIG_HOME` and `XDG_CONFIG_HOME` are set (`AWMAN_CONFIG_HOME` wins).
- `home_dir_with` returns `$HOME/.awman` when neither override is set (existing test must continue to pass).

**API paths — `src/data/fs/api_paths.rs`**
- `root_with` returns `$XDG_DATA_HOME/awman/api` when `XDG_DATA_HOME` is set and `AWMAN_API_ROOT` is absent.
- `root_with` returns `AWMAN_API_ROOT` when both are set (`AWMAN_API_ROOT` wins).
- Fallback to `$HOME/.awman/api` when neither is set.

**`exec_workflow` — `src/command/commands/exec_workflow.rs`**
- `collect_single_entry_overlays` (and any other caller of `detect_home_from_dockerfile`) uses the repo-config-resolved Dockerfile path, not the inline `git_root.join("Dockerfile.dev")`.

**End-to-end / integration**
- `awman ready` with `"dockerfile": "infra/Dockerfile.base"` in `.awman/config.json` reports the configured path in missing-file prompts when the file is absent.
- `awman ready` with `"dockerfile": "infra/Dockerfile.base"` uses the correct file when it is present.

**`InitEngine` — `src/engine/init/mod.rs`**
- When `Dockerfile.dev` (or the configured path) already exists, `SettingUpDockerfile` transitions directly to `SettingUpAgentDockerfile` without entering `AwaitingDockerfileDecision`. `ask_dockerfile_setup` is never called.
- When no Dockerfile exists and `ask_dockerfile_setup` returns `CreateNew`, the engine creates `Dockerfile.dev` from the template and `summary.dockerfile == StepStatus::Done`.
- When `ask_dockerfile_setup` returns `UseExisting("docker/Dockerfile.custom")`, the engine saves that path to `RepoConfig.dockerfile` on disk and `summary.dockerfile == StepStatus::Done`.
- When `ask_dockerfile_setup` returns `Skip`, `summary.dockerfile == StepStatus::Skipped` and no file is created or written.
- `UseExisting` path is present in `RepoConfig` after init completes: load `.awman/config.json` from disk and assert `config.dockerfile == Some("docker/Dockerfile.custom")`.
- Running init twice on a repo with an existing Dockerfile: `AwaitingDockerfileDecision` is never entered on the second run.
- `FakeInitFrontend` in `src/engine/init/mod.rs` tests must be extended with an `ask_dockerfile_setup` response field (default `CreateNew`) to avoid breaking existing tests.

**CLI `ask_dockerfile_setup` — `src/frontend/cli/per_command/init.rs`**
- Non-TTY stdin returns `CreateNew` without prompting.
- TTY stdin choosing `[1]` returns `CreateNew`.
- TTY stdin choosing `[2]` then entering `"docker/Dockerfile"` returns `UseExisting("docker/Dockerfile")`.
- TTY stdin choosing `[3]` returns `Skip`.
- TTY stdin choosing `[2]` then entering an empty string falls back to `CreateNew`.

**TUI `ask_dockerfile_setup` — `src/frontend/tui/per_command/init.rs`**
- `KindSelect` index 0 (first option) returns `CreateNew`.
- `KindSelect` index 1 (second option) followed by `TextInput` with non-empty text returns `UseExisting(text)`.
- `KindSelect` index 1 followed by dismissed or empty `TextInput` returns `CreateNew`.
- `KindSelect` index 2 (third option) returns `Skip`.
- `Dismissed` on the `KindSelect` returns `CreateNew`.


## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's `aspec/`.
- Path resolution helpers in `RepoConfig` must follow the same relative/absolute logic already used by `work_items_dir` and `work_items_template` in `src/data/config/repo.rs`.
- Env var handling must go through `EnvSnapshot` — no scattered `std::env::var(…)` calls in other modules. Add new XDG accessors to `EnvSnapshot` in `src/data/config/env.rs` and update `Env::from_process()` to capture them.
- `AWMAN_CONFIG_HOME` isolation pattern used in all existing config tests must remain unbroken. XDG vars added to the env snapshot must not affect any test that only sets `AWMAN_CONFIG_HOME`.
- Configurable `dockerfile` path should mirror the JSON key naming convention established by `workItems` / `baseImage` fields (camelCase in JSON, snake_case in Rust struct fields with `#[serde(rename = "…")]`).
- The two callsites that inline `git_root.join("Dockerfile.dev")` (`repo_dockerfile_paths.rs:25` and `exec_workflow.rs:1299`) must be updated to use the config-resolved path. Search for additional inline uses before closing the work item.
- `ask_dockerfile_setup` must be added to the `InitFrontend` trait in `src/engine/init/frontend.rs` and implemented on `CliFrontend` (`src/frontend/cli/per_command/init.rs`) and `TuiCommandFrontend` (`src/frontend/tui/per_command/init.rs`). The `FakeInitFrontend` used in engine unit tests (`src/engine/init/mod.rs`) must also implement it.
- New `InitPhase` variants (`AwaitingDockerfileDecision`, `SavingDockerfileConfig`) must be added to the `match` arms in `InitEngine::step()`. They must appear in the serializable phase enum (`src/engine/init/phase.rs`) so that any JSON-serialized phase snapshots remain round-trippable.
- Coordinate the `SavingDockerfileConfig` and `WritingConfig` phases to avoid redundant disk writes: the preferred approach is to pass `pending_dockerfile_path` into the `WritingConfig` phase and let it fold the value into the single initial config write, skipping `SavingDockerfileConfig` unless the config already existed on disk.


## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** — add the `dockerfile` config key to the configuration reference doc (wherever `workItems` and `baseImage` are documented).
- **Update the XDG / environment variable section** — document `XDG_CONFIG_HOME` and `XDG_DATA_HOME` alongside the existing `AWMAN_*` env var reference.
- **Never create work-item-specific docs** (e.g., no "WI 0086 implementation guide" in published docs).
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`.
- **Docs are for end users**, not for developers trying to understand implementation.

See `CLAUDE.md` for more guidance on documentation standards.
