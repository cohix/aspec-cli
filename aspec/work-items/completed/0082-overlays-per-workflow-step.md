# Work Item: Feature

Title: Overlays per workflow step
Issue: issuelink

## Summary:
- Each workflow step can define its own `overlays` array, giving each agent container in the workflow a distinct set of host resources.
- `--mount-ssh` is removed and replaced by a builtin `ssh()` overlay shorthand. `ssh()` is pure syntax sugar — it expands at parse time into a standard read-only directory overlay mounting `~/.ssh` from the host into `~/.ssh` in the container (where `~` resolves to the container user's home directory). There are no special type variants, dedicated fields, or special code paths in the engine beyond the generic `~/` container-path expansion described below.
- The `skill()` overlay is introduced in singular form: `skill(*)` mounts all global skills; `skill(name)` mounts exactly one named skill by its directory name. Multiple `skill(name)` calls are additive — each names a single skill to locate and mount individually. The old plural `skills(...)` form has been removed; encountering it is a parse error whose message directs the user to `skill(*)` or `skill(name)`.
- Per-step overlays are merged with repo-config, env-var, and flag overlays using union/additive semantics. `skill(*)` from any source causes all skills to be mounted; named skills accumulate across all sources; directories accumulate with least-permissive-wins per host path; env vars deduplicate by name.
- Setup and teardown steps also support an `overlays` field. Supported types are `dir()`, `ssh()`, and `env(VAR_NAME)`. `skill()` and `skills()` are invalid on setup/teardown steps and produce a validation error at workflow load time.
- A new `env(VAR_NAME)` overlay type is introduced. Each `env()` call names exactly one host env var. Multiple vars are expressed as multiple `env()` expressions — repeated `--overlay` flags, comma-separated entries in `AWMAN_OVERLAYS`, or multiple items in a step's or config's `overlays` array. For agent container steps the named vars are added to the container env passthrough. For setup/teardown steps the named vars are passed explicitly to the subprocess. The dedicated `envPassthrough` field in `RepoConfig` and `GlobalConfig` is removed; env passthrough is now expressed exclusively via `env()` overlay expressions in the config's `overlays` array, making all overlay sources use the same unified format.
- As a concrete deliverable, `aspec/workflows/implement-pr.toml` is updated: `push_branch` gains `overlays = ["ssh()"]` and `create_pull_request` gains `overlays = ["env(GITHUB_TOKEN)"]`.

Before implementing, read `aspec/architecture/2026-grand-architecture.md` in full and ensure every decision respects the four-layer architecture. The layer constraints are non-negotiable.

## User Stories

### User Story 1:
As a: user authoring a multi-step workflow

I want to: give each step its own `overlays` list in the workflow file

So I can: run one step with SSH access and selected skills while another step runs with a different directory mounted, without those resources leaking across step boundaries.

### User Story 2:
As a: user migrating from `--mount-ssh`

I want to: write `--overlay ssh()`, set `AWMAN_OVERLAYS=ssh()`, or add `"ssh()"` to a step's overlays list

So I can: get the same `~/.ssh` directory mounted read-only into the container through the unified overlay system, and drop the now-removed `--mount-ssh` flag from my scripts and aliases.

### User Story 3:
As a: user configuring a workflow that uses shared global skills

I want to: write `skill(lint)` and `skill(review)` in a step's overlays to mount only those two skills

So I can: avoid exposing unrelated skills to a step that doesn't need them, while another step in the same workflow can use `skill(*)` to mount everything.


## Implementation Details:

Read `aspec/architecture/2026-grand-architecture.md` before writing any code. The four-layer constraint is:
- Layer 0 (`src/data/`): data types, config, serialization — no engine or command imports
- Layer 1 (`src/engine/`): runtime primitives — imports Layer 0 only
- Layer 2 (`src/command/`): business logic — imports Layers 0–1
- Layer 3 (`src/frontend/`): presentation only — no business logic

### 1. `SkillSpec` — define at Layer 2

`SkillSpec` is the per-expression result type produced by `parse_single_typed_overlay()` when it encounters a `skill(...)` expression. It lives at **Layer 2** in `src/command/commands/mod.rs` alongside `TypedOverlay`:

```rust
// src/command/commands/mod.rs  (Layer 2)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSpec {
    All,           // skill(*) — mount all global skills
    Named(String), // skill(name) — locate and mount exactly one named skill
}
```

`SkillSpec` is a parse-time type only. It is never serialized. The accumulated result flowing to Layer 1 is expressed as two plain fields on `OverlayRequest` and `AgentRunOptions` (see section 3), so no Layer 0 type is needed.

`skill()` with no arguments and `skills(...)` (plural form) are both parse errors. They are not processed and do not produce any overlay. The error message for each directs the user to the correct form:
- `skill()` → "skill() requires an argument; use skill(*) to mount all skills or skill(name) for a specific named skill"
- `skills(...)` → "skills() has been removed; use skill(*) to mount all skills or skill(name) for a specific named skill"

### 2. Replace `OverlaysConfig` with flat `Vec<String>` — Layer 0

The `OverlaysConfig` struct (`{ directories, skills }`) and the dedicated `envPassthrough` field are removed from both `RepoConfig` and `GlobalConfig`. All are replaced by a single flat `overlays: Option<Vec<String>>` field in each config struct that uses the same typed overlay expression syntax as every other source.

```rust
// src/data/config/repo.rs
pub struct RepoConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlays: Option<Vec<String>>,   // e.g. ["dir(~/data:/workspace/data:ro)", "env(GH_TOKEN)", "skill(*)"]

    // Legacy field — read from JSON but never written and never used.
    // Presence triggers a deprecation warning emitted by the command layer (see section 10).
    #[serde(rename = "envPassthrough", default, skip_serializing)]
    pub legacy_env_passthrough: Option<Vec<String>>,
    // ...other fields unchanged
}
```

Apply the same change to `GlobalConfig` in `src/data/config/global.rs`.

Delete the `OverlaysConfig` and `DirectoryOverlayConfig` structs and all their serde implementations.

**No automated migration.** The old `"overlays": { ... }` object shape will fail to deserialize as `Vec<String>` and surface a config parse error — users must update their config file manually. `envPassthrough` is silently preserved in the struct only for detection; its value is never used. The command layer emits the warning (see section 10).

**Config example after this change:**
```json
{
  "overlays": [
    "dir(~/my-data:/workspace/data:ro)",
    "env(GITHUB_TOKEN)",
    "env(ANTHROPIC_TOKEN)",
    "skill(lint)"
  ]
}
```

### 3. Update `OverlayRequest` and `AgentRunOptions` — Layer 1

In `src/engine/overlay/mod.rs`, replace `include_skills: bool` in `OverlayRequest` with two plain fields:

```rust
pub include_all_skills: bool,     // true if any skill(*) was seen from any source
pub named_skills: Vec<String>,    // accumulated from individual skill(name) calls; deduplicated
```

In `src/engine/agent/mod.rs`, apply the same replacement to `AgentRunOptions`:

```rust
pub include_all_skills: bool,
pub named_skills: Vec<String>,
```

Remove `mount_ssh: bool` from both `OverlayRequest` and `AgentRunOptions` entirely. SSH is now just another entry in the directory overlays — no dedicated field anywhere in the engine.

Update `build_options()` in `src/engine/agent/mod.rs` to construct `OverlayRequest` from the new fields. Remove the `if run.mount_ssh { ... ContainerOption::MountSsh ... }` block — it is no longer needed.

Remove `ContainerOption::MountSsh` and its handling in `src/engine/container/docker.rs` (the `:650–658` block). SSH mounts now arrive as ordinary `ContainerOption::Overlay` entries, built by the standard overlay pipeline.

### 4. Update `OverlayEngine::skill_overlays()` — Layer 1

In `src/engine/overlay/mod.rs`, update `skill_overlays()` to accept the two plain fields instead of a filter:

```rust
fn skill_overlays(
    &self,
    agent: &AgentName,
    include_all: bool,
    names: &[String],
    ...
) -> Result<Vec<OverlaySpec>, EngineError>
```

When `include_all` is true, list the global skills directory and emit overlays for all entries.  
When `include_all` is false and `names` is non-empty, emit overlays only for entries whose directory name matches one of the listed names. Emit a hard `EngineError` for any named skill that does not exist (typo prevention — fail early before any container launches).  
When `include_all` is false and `names` is empty, emit no overlays.

Update `build_overlays()` to use `request.include_all_skills` and `request.named_skills` instead of `request.include_skills`.

### 5. Add `ssh()` builtin shorthand — Layer 2 parsing, Layer 1 resolution

`ssh()` is pure parse-time sugar. No new `TypedOverlay` variant is needed. The updated `TypedOverlay` enum is:

```rust
pub enum TypedOverlay {
    Directory(DirectorySpec),
    Skill(SkillSpec),   // one expression per call; see SkillSpec in section 1
    Env(String),        // exactly one host env var name per call
}
```

Extend `parse_single_typed_overlay()`:
- `"skill"` tag with `*` → `TypedOverlay::Skill(SkillSpec::All)`
- `"skill"` tag with a single non-empty, non-`*` name → `TypedOverlay::Skill(SkillSpec::Named(name.to_string()))`
- `"skill"` tag with no args → parse error: "skill() requires an argument; use skill(*) to mount all skills or skill(name) for a specific named skill"
- `"skill"` tag with multiple args → parse error: "skill() takes one argument; use separate skill() calls for multiple named skills"
- `"skills"` tag (plural, any args) → parse error: "skills() has been removed; use skill(*) to mount all skills or skill(name) for a specific named skill"
- `"ssh"` tag with no args → expand immediately to `TypedOverlay::Directory(DirectorySpec { host: <expanded ~/.ssh>, container: "~/.ssh".into(), permission: OverlayPermission::ReadOnly })`
- `"ssh"` tag with any args → parse error (ssh takes no arguments)

The host-side `~` is expanded at parse time using the existing `OverlayPathResolver::expand_tilde` helper. The container-side `~/.ssh` is left as a tilde path — the engine resolves it at build time (see below).

**Container-side tilde expansion — Layer 1**

Extend `resolve_user_overlay()` in `src/engine/overlay/mod.rs` to handle container paths that start with `~/`: replace the leading `~` with `request.container_home` when set, or `/root` when absent. This is a general mechanism, not SSH-specific — any `dir()` expression may use `~/` as a portable reference to the container user's home.

The existing validation that rejects non-absolute container paths is updated to permit `~/…` paths before expansion (they become absolute after the `~` substitution).

**Populate `container_home` from the agent's Dockerfile — Layer 1**

`OverlayRequest.container_home` already exists but is always passed as `None` today. Update `AgentEngine.build_options()` to detect the agent's `dockerfile_user` (as the old SSH code did) and pass it as `container_home: Some(format!("/home/{dockerfile_user}"))` when set, or leave it `None` (defaulting to `/root` in `resolve_user_overlay`). This ensures `ssh()` — and any other `~/` overlay — lands at the correct path for both root and non-root agent containers.

After parse and resolution, the SSH overlay is an ordinary `OverlaySpec` that flows through `insert_or_merge` and is emitted as `ContainerOption::Overlay`, identically to every other directory overlay.

Update the error message for unknown overlay types to include `ssh`, `skill`, and `env` in the list of supported types.

### 6. `CollectedOverlays` — Layer 2 return type

Replace the current `(Vec<DirectorySpec>, bool)` return of `collect_all_overlay_specs()` with a named struct (per the grand architecture's preference for typed objects over raw functions):

```rust
pub struct CollectedOverlays {
    pub directories: Vec<DirectorySpec>,
    pub include_all_skills: bool,      // true if any skill(*) was seen from any source
    pub named_skills: Vec<String>,     // union of all skill(name) expressions; deduplicated
    pub env_passthrough: Vec<String>,  // deduplicated env var names from all env() expressions
}
```

There is no `mount_ssh` field. If `ssh()` was requested from any source it already expanded to a `DirectorySpec` inside `directories`.

### 7. Extend `collect_all_overlay_specs()` to include step overlays — Layer 2

`collect_all_overlay_specs()` in `src/command/commands/mod.rs` currently gathers overlays from four sources. With the config schema change in section 2, all five sources now use identical `Vec<String>` inputs parsed by `parse_overlay_list()`:

| Priority | Source |
|---|---|
| 1 (lowest) | `global_config.overlays` strings |
| 2 | `repo_config.overlays` strings |
| 3 | `AWMAN_OVERLAYS` env var (comma-separated) |
| 4 | `--overlay` CLI flags (one expression per flag) |
| 5 (highest) | Step `overlays` array strings |

Extend the function signature to accept an optional step overlay list:

```rust
pub fn collect_all_overlay_specs(
    session: &Session,
    cli_typed_overlays: Vec<TypedOverlay>,
    step_overlays: Option<&[String]>,   // None for non-workflow paths
) -> Result<CollectedOverlays, CommandError>
```

Parse each source's strings using `parse_overlay_list()`. Return `Err` on any parse error in any source (including malformed `AWMAN_OVERLAYS` — fix the existing silent-drop bug at line 292 of `src/command/commands/mod.rs`).

**Merge rules implemented here:**
- **Directories**: accumulate all `dir()` and `ssh()` specs from all sources into a `Vec`; `OverlayEngine::build_overlays()` deduplicates by host path with least-permissive-wins (existing `insert_or_merge` logic).
- **Skills — union/additive**: `include_all_skills` is set to `true` if any source contains `skill(*)`. Named skills from all sources are unioned into `named_skills`, deduplicated by name. When `include_all_skills` is true, `named_skills` is ignored at engine time (all skills are mounted regardless). There is no priority-wins logic — every source contributes. Any `skills(...)` or `skill()` (no-arg) expression in any source is a parse error that aborts collection immediately.
- **Env passthrough**: union of all `env(VAR)` expressions across all sources; deduplicate by var name. `AgentRunOptions.env_passthrough` is populated solely from `collected.env_passthrough` — there is no longer a separate config-level env passthrough field.

All callers of `collect_all_overlay_specs` (in `exec_prompt.rs`, `chat.rs`, `exec_workflow.rs`) pass `None` for `step_overlays` except `exec_workflow.rs`'s `CommandLayerFactory::execution_for_step`, which passes `step.overlays.as_deref()`.

### 8. Add `overlays` field to `WorkflowStep` — Layer 0

In `src/data/workflow_definition.rs`, add to `WorkflowStep` and its raw TOML/YAML counterparts:

```rust
pub overlays: Option<Vec<String>>,
```

Each string is a typed overlay expression using the existing syntax. Absence of the field means the step contributes no additional overlays. Propagate through `raw_to_steps()`.

### 9. Add `env()` overlay type — Layer 2

`env(VAR_NAME)` is a new typed overlay expression that names exactly one host env var per call. Multiple vars are expressed as multiple `env()` expressions — not as comma-separated arguments inside a single call.

Valid forms:
```
env(GITHUB_TOKEN)                        ← one var, as a CLI flag or single AWMAN_OVERLAYS entry
env(GITHUB_TOKEN), env(ANTHROPIC_TOKEN)  ← two vars, comma-separated in AWMAN_OVERLAYS
--overlay env(GITHUB_TOKEN) --overlay env(ANTHROPIC_TOKEN)  ← two vars, repeated CLI flags
```
```toml
# in a config file or workflow step
overlays = ["env(GITHUB_TOKEN)", "env(ANTHROPIC_TOKEN)"]
```

Extend `parse_single_typed_overlay()`:
- `"env"` tag with exactly one non-empty argument → `TypedOverlay::Env(var_name.to_string())`
- `"env"` tag with no argument → parse error
- `"env"` tag with multiple comma-separated arguments → parse error; error message explains that each var requires its own `env()` call and shows the correct form

`env_passthrough` in `CollectedOverlays` (defined in section 6) accumulates all var names produced by `env()` expressions across all sources (config, env var, flags, step). Deduplicate by var name; order is not significant.

For **agent container steps**: `AgentRunOptions.env_passthrough` is set solely from `collected.env_passthrough`. The old `EffectiveConfig::env_passthrough()` call site is removed — that method can be deleted since `envPassthrough` no longer exists in the config structs.

For **setup/teardown host steps**: the workflow engine copies each var name from the host process environment into the subprocess's `std::process::Command::env()` call for that step. Vars unset on the host are silently absent (not an error).

Update the error message for unknown overlay types to include `env` in the list of supported types.

### 10. Emit deprecation warning for legacy `envPassthrough` — Layer 2

Because config loading is at Layer 0 and the message sink is a Layer 1/2 trait, the deprecation warning cannot be emitted during deserialization. Instead, add a shared helper at Layer 2:

```rust
// src/command/commands/mod.rs
pub fn warn_legacy_config(session: &Session, sink: &mut impl UserMessageSink) {
    let ec = session.effective_config();
    if ec.repo().legacy_env_passthrough.is_some() {
        sink.write_message(UserMessage {
            level: MessageLevel::Warning,
            text: "'.awman/config.json' contains a deprecated 'envPassthrough' field. \
                   Move these vars to the 'overlays' array as env() expressions, e.g. \
                   \"env(VAR_NAME)\", then remove 'envPassthrough'.".into(),
        });
    }
    if ec.global().legacy_env_passthrough.is_some() {
        sink.write_message(UserMessage {
            level: MessageLevel::Warning,
            text: "'~/.awman/config.json' contains a deprecated 'envPassthrough' field. \
                   Move these vars to the 'overlays' array as env() expressions, e.g. \
                   \"env(VAR_NAME)\", then remove 'envPassthrough'.".into(),
        });
    }
}
```

Call `warn_legacy_config(&self.session, &mut frontend)` near the start of `run_with_frontend` in `ChatCommand`, `ExecPromptCommand`, and `ExecWorkflowCommand` — before any overlay collection or container launch.

### 11. Add `overlays` to setup and teardown steps — Layer 0

Setup and teardown steps are serde-tagged enums. Rather than adding `overlays` to every variant, introduce thin wrapper structs that use `#[serde(flatten)]` to merge the inner enum fields with the outer `overlays` field at the same TOML/YAML level:

```rust
// src/data/workflow_definition.rs  (Layer 0)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupStepEntry {
    #[serde(default)]
    pub overlays: Option<Vec<String>>,
    #[serde(flatten)]
    pub step: SetupStep,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeardownStepEntry {
    #[serde(default)]
    pub overlays: Option<Vec<String>>,
    #[serde(flatten)]
    pub step: TeardownStep,
}
```

Update `Workflow`:
```rust
pub setup: Vec<SetupStepEntry>,
pub teardown: Vec<TeardownStepEntry>,
```

Update all deserialization intermediates (`TomlWorkflow`, `YamlWorkflow`) and any downstream match arms that destructure setup/teardown steps.

**Validation**: during workflow loading, reject any setup or teardown step whose `overlays` list contains a `skill(...)` or `skills(...)` expression. Setup/teardown steps do not run agent containers, so skills overlays are meaningless and likely a mistake. Emit a descriptive `DataError`.

**Execution**: the workflow engine passes each step entry's `overlays` strings through `collect_all_overlay_specs()` (with `step_overlays: Some(entry.overlays.as_deref())`) to produce a `CollectedOverlays`. For setup/teardown steps, `directories` and `env_passthrough` are applied to the subprocess. Skills fields are always empty (validated above).

### 12. Update `aspec/workflows/implement-pr.toml` — concrete deliverable

As part of implementing this work item, update the workflow file directly:

```toml
[[teardown]]
type = "push_branch"
overlays = ["ssh()"]

[[teardown]]
type = "create_pull_request"
title = "Implement {{work_item_number}}"
overlays = ["env(GITHUB_TOKEN)"]
```

### 13. Wire step overlays in `CommandLayerFactory` — Layer 2

In `exec_workflow.rs`, `CommandLayerFactory::execution_for_step` already builds `AgentRunOptions` per step. Change it to call `collect_all_overlay_specs(..., step.overlays.as_deref())` and map the resulting `CollectedOverlays` into `AgentRunOptions`:

```rust
let collected = collect_all_overlay_specs(&session, cli_overlays.clone(), step.overlays.as_deref())?;
let run_opts = AgentRunOptions {
    directory_overlays: collected.directories,
    include_all_skills: collected.include_all_skills,
    named_skills: collected.named_skills,
    env_passthrough: collected.env_passthrough,
    ...
};
```


### 14. Remove `--mount-ssh` flag — Layer 2 and Layer 3

Remove `mount_ssh: bool` from `ExecWorkflowCommandFlags`, `ExecPromptCommandFlags`, and `ChatCommandFlags`. Remove the flag definitions from `src/command/dispatch/catalogue.rs` and their parsing in `src/command/dispatch/mod.rs`. Remove the field from `CommandLayerFactory`.

If `--mount-ssh` is encountered in parsed input, emit a clear error naming the flag and pointing to `ssh()` as the replacement.

Update `aspec/uxui/cli.md` to remove `--mount-ssh` from the `chat`, `exec prompt`, and `exec workflow` tables, and document the `--overlay ssh()` replacement.

### Workflow file format

TOML example:
```toml
[[step]]
name = "research"
prompt_template = "Research the topic..."
overlays = ["ssh()", "skill(search)", "skill(fetch)"]

[[step]]
name = "write"
prompt_template = "Write the report..."
overlays = ["dir(/data/reports:/workspace/reports:rw)", "skill(*)"]
```

YAML example:
```yaml
steps:
  - name: research
    prompt_template: "Research the topic..."
    overlays:
      - "ssh()"
      - "skill(search)"
      - "skill(fetch)"
  - name: write
    prompt_template: "Write the report..."
    overlays:
      - "dir(/data/reports:/workspace/reports:rw)"
      - "skill(*)"
```


## Edge Case Considerations:

- **Empty `overlays: []` on a step** — step contributes no additional overlays; overlays from repo config, env, and flags still apply. Identical effective result to the field being absent.
- **Named skill that does not exist** — hard `EngineError` emitted before any container launches. Error message names the missing skill and the global skills directory path. Do not silently skip.
- **`skill(*)` as explicit wildcard** — produces `SkillSpec::All`; causes `include_all_skills = true` in `CollectedOverlays`, which mounts all global skills regardless of any `named_skills` entries.
- **`skill(*)` from one source and `skill(foo)` from another** — union semantics: `include_all_skills = true` (from `skill(*)`), `named_skills = ["foo"]`. Since `include_all_skills` is true, all skills are mounted; `foo` is included implicitly.
- **Missing `~/.ssh` when `ssh()` is requested** — `resolve_user_overlay()` canonicalizes the host path and will return an error if it does not exist, same as any other directory overlay. No container launches.
- **Same host directory in step overlays and repo config with conflicting permissions** — least-permissive (ReadOnly) always wins via `insert_or_merge`. No warning needed.
- **Workflow with no step `overlays` field** — behavior identical to pre-WI; no regression.
- **`--mount-ssh` used by an existing caller** — reject with a clear error pointing to `ssh()`. Do not silently ignore.
- **SSH requested at multiple sources** — `ssh()` expands to the same host path everywhere; `insert_or_merge` deduplicates by host path, yielding exactly one mount, same as any duplicated `dir()` overlay.
- **Non-root agent container** — `container_home` on `OverlayRequest` is populated from the agent's detected `dockerfile_user`. `resolve_user_overlay()` expands `~/` in container paths to this value, so `ssh()` mounts to `/home/<user>/.ssh` for non-root containers and `/root/.ssh` for root containers.
- **`skill(foo)` in step and `skill(*)` at flag level** — union semantics: `include_all_skills = true` (from `skill(*)`); all skills mounted for that step.
- **`skill(foo)` at repo config and `skill(bar)` at step** — both accumulate: `named_skills = ["foo", "bar"]`; both skills mounted.
- **Parallel steps with different overlays** — each step's container is independently computed; they do not share overlay state.
- **Malformed `AWMAN_OVERLAYS`** — must be a fatal error (fixes the existing silent-drop bug). Error message must contain "AWMAN_OVERLAYS" so the user knows the source. This aligns with the failing test at `tests/overlays_integration.rs:220-242`.
- **`--help` with malformed `AWMAN_OVERLAYS`** — help display must still succeed. The overlay-parse skip for help is intentional and must be preserved.
- **`envPassthrough` in existing config** — the field deserializes into `legacy_env_passthrough` (ignored at runtime) and triggers a deprecation warning on the message sink. Its values are NOT silently used. Users must manually move vars to `env()` overlay expressions.
- **Old `overlays` object format in existing config** — `"overlays": { "directories": [...], "skills": true }` will fail to deserialize as `Vec<String>` and surface a config parse error. No automatic conversion. Users must update their config manually.
- **`skill()` or `skills()` on a setup/teardown step** — validation error at workflow load time, before any step runs. Error message names the step type and explains that skills overlays are only valid on agent steps.
- **`env(VAR_NAME)` where the named var is unset on the host** — not an error at parse or validation time; the var is simply absent from the subprocess env. This mirrors how `AWMAN_OVERLAYS` passthrough works.
- **`env()` with no argument** — parse error.
- **`env(A, B)` with multiple arguments** — parse error; error message should explain that each var requires its own `env()` call and show the correct form.
- **Same `env(VAR)` from multiple sources** — deduplicate by name; passing the same var twice is harmless but produces only one entry in `env_passthrough`.
- **`dir()` overlay on a setup/teardown step** — accepted by the parser; applied as a mounted path if the step is containerized, silently ignored for host-executed steps. Do not error.
- **`skill(name)` with whitespace in name** — parse error; skill names must be valid directory name components.
- **`skills(...)` (plural) or `skill()` (no args)** — parse error with a migration message; no overlay is produced and collection halts with `Err`. These forms are removed, not deprecated.


## Test Considerations:

**Unit tests (`src/command/commands/mod.rs`):**
- `parse_single_typed_overlay("ssh()")` → `TypedOverlay::Directory` with host `<home>/.ssh`, container `"~/.ssh"`, permission `ReadOnly`
- `parse_single_typed_overlay("ssh(foo)")` → error
- `parse_single_typed_overlay("skill(*)")` → `TypedOverlay::Skill(SkillSpec::All)`
- `parse_single_typed_overlay("skill(foo)")` → `TypedOverlay::Skill(SkillSpec::Named("foo"))`
- `parse_single_typed_overlay("skill(foo, bar)")` → error (multiple args not allowed; use separate `skill()` calls)
- `parse_single_typed_overlay("skill()")` → error with message directing user to `skill(*)` or `skill(name)`
- `parse_single_typed_overlay("skills(foo)")` → error with message directing user to `skill(foo)`
- `parse_single_typed_overlay("skills(*)")` → error with message directing user to `skill(*)`
- `parse_single_typed_overlay("skills()")` → error with message directing user to `skill(*)`
- `parse_single_typed_overlay("env(GH_TOKEN)")` → `TypedOverlay::Env("GH_TOKEN")`
- `parse_single_typed_overlay("env()")` → error
- `parse_single_typed_overlay("env(A, B)")` → error (multiple args not allowed; use separate `env()` calls)
- `parse_overlay_list("env(A), env(B)")` → two `TypedOverlay::Env` entries
- `collect_all_overlay_specs` with malformed `AWMAN_OVERLAYS` → `Err`
- Skills union: `skill(*)` in flags + `skill(foo)` in step → `include_all_skills = true`, `named_skills = ["foo"]`
- Skills union: `skill(foo)` in repo config + `skill(bar)` in step → `include_all_skills = false`, `named_skills = ["foo", "bar"]`
- `collect_all_overlay_specs` with `skills()` in any source → `Err` with migration message
- `collect_all_overlay_specs` with `skills(foo)` in any source → `Err` with migration message
- `ssh()` from flag + `ssh()` from step → `directories` contains exactly one entry for `~/.ssh` host path
- `env(X)` from two sources → `env_passthrough` contains `"X"` exactly once (deduplicated)
- `env(X)` in repo config overlays + `env(Y)` in step overlays → agent container sees both `X` and `Y`
- Deprecated `skills()` form → `CollectedOverlays.warnings` is non-empty

**Unit tests (`src/engine/overlay/mod.rs`):**
- `resolve_user_overlay` with container path `"~/.ssh"` and `container_home: Some("/home/alice")` → resolved container path is `/home/alice/.ssh`
- `resolve_user_overlay` with container path `"~/.ssh"` and `container_home: None` → resolved container path is `/root/.ssh`
- `skill_overlays` with `include_all: false, names: ["foo"]` → only `foo` overlay emitted
- `skill_overlays` with `include_all: false, names: ["nonexistent"]` → `EngineError`
- `skill_overlays` with `include_all: true, names: []` → all skills emitted
- Same host path with `ro` from step and `rw` from flag → resulting spec has `ReadOnly`

**Unit tests (`src/data/config/repo.rs`):**
- New format: `"overlays": ["skill(*)", "env(X)"]` deserializes to `overlays: Some(vec!["skill(*)", "env(X)"])`
- Old format: `"overlays": { "directories": [...], "skills": true }` fails to deserialize (no auto-migration)
- Legacy field: `"envPassthrough": ["VAR"]` deserializes to `legacy_env_passthrough: Some(vec!["VAR".into()])` with `overlays: None`
- New format: `"overlays": ["dir(~/data:/workspace:ro)", "env(TOKEN)"]` round-trips correctly

**Unit tests (`src/command/commands/mod.rs` — `warn_legacy_config`):**
- Session with `repo().legacy_env_passthrough = Some([...])` → warning message written to sink mentioning `.awman/config.json`
- Session with `global().legacy_env_passthrough = Some([...])` → warning message written to sink mentioning `~/.awman/config.json`
- Session with both fields absent → no warning written

**Integration tests (`tests/overlays_integration.rs`):**
- `--overlay ssh()` mounts `~/.ssh` read-only; container path resolves to `/home/<user>/.ssh` for a non-root Dockerfile and `/root/.ssh` for a root Dockerfile.
- `--overlay ssh()` specified twice (or once via flag and once via env) → exactly one `-v` mount in the Docker argv.
- `--mount-ssh` flag rejected with non-zero exit and message pointing to `ssh()`.
- `skill(lint)` with valid skill name → only `lint` skill directory mounted.
- `skill(nonexistent)` → non-zero exit, error names the unknown skill.
- `skill(*)` → all skills directories mounted.
- `skills(lint)` or `skills()` anywhere in input → non-zero exit, error message names the removed form and points to `skill(name)` / `skill(*)`.
- Malformed `AWMAN_OVERLAYS` → non-zero exit, stderr contains "AWMAN_OVERLAYS" (fixes existing failing test at line 220–242).
- `AWMAN_OVERLAYS` malformed + `--help` → zero exit (help still works).

**Unit tests (`src/data/workflow_definition.rs`):**
- `TeardownStepEntry` with `type = "push_branch"` and `overlays = ["ssh()"]` deserializes correctly.
- `TeardownStepEntry` with `type = "create_pull_request"` and `overlays = ["env(GITHUB_TOKEN)"]` deserializes correctly.
- `SetupStepEntry` with `overlays = ["skill(*)"]` → `DataError` at validation time.
- `SetupStepEntry` with `overlays = ["skills()"]` → parse error (removed form) before validation even runs.

**Workflow integration tests:**
- Agent step with `overlays = ["ssh()"]` → container for that step includes SSH mount; sibling step without it does not.
- Agent step with `overlays = ["skill(foo)"]` → only `foo` skill mounted for that step.
- Agent step with `overlays = ["skill(foo)", "skill(bar)"]` → both `foo` and `bar` skills mounted.
- Agent step with `overlays = ["skill(*)"]` → all skills mounted for that step.
- Agent step with `overlays = ["env(MY_VAR)"]` → `MY_VAR` appears in container env passthrough.
- Agent step with no `overlays` field → inherits merged overlays from flag/env/config only.
- Step directory overlay with `rw` combined with repo-config `ro` for same host path → container gets `ro`.
- `skill(foo)` in repo config + `skill(bar)` in step → both skills mounted (union).
- `skill(*)` in flags + `skill(foo)` in step → all skills mounted (absorbing element).
- Workflow with no step-level `overlays` on any step → behavior unchanged from pre-WI.
- Teardown `push_branch` with `overlays = ["ssh()"]` → `~/.ssh` is available to the git push subprocess.
- Teardown `create_pull_request` with `overlays = ["env(GITHUB_TOKEN)"]` → `GITHUB_TOKEN` is present in the subprocess env.
- Teardown step with `overlays = ["skill(*)"]` → workflow load fails with a descriptive `DataError` before any step runs.
- Any step or config with `overlays = ["skills()"]` or `overlays = ["skills(foo)"]` → parse error (removed form) with migration message.
- `skill(foo)` from global config + `skill(bar)` from step → both mounted (union); no priority-wins.
- `skill(*)` anywhere + named skills elsewhere → all skills mounted (`include_all_skills` absorbs named list).


## Codebase Integration:

- **`src/data/overlay_types.rs`** — do NOT create this file; `SkillsFilter` is removed entirely. The accumulated skill state flows as plain `bool` + `Vec<String>` fields on `CollectedOverlays`, `OverlayRequest`, and `AgentRunOptions`.
- **`src/data/config/repo.rs`** (Layer 0) — delete `OverlaysConfig` and `DirectoryOverlayConfig` structs. Replace `env_passthrough: Option<Vec<String>>` with `legacy_env_passthrough: Option<Vec<String>>` (`#[serde(rename = "envPassthrough", default, skip_serializing)]` — read-only, never used). Add `overlays: Option<Vec<String>>` (plain serde, no custom deserializer).
- **`src/data/config/global.rs`** (Layer 0) — same changes.
- **`src/data/config/effective.rs`** (Layer 0) — delete `env_passthrough()` method; update any method that referenced `OverlaysConfig`.
- **`src/command/commands/mod.rs`** (Layer 2) — add `SkillSpec { All, Named(String) }` enum; update `TypedOverlay` to `{ Directory(DirectorySpec), Skill(SkillSpec), Env(String) }`; add `warn_legacy_config(session, sink)` helper; call it at the start of `run_with_frontend` in `ChatCommand`, `ExecPromptCommand`, and `ExecWorkflowCommand`; expand `"ssh"` tag in `parse_single_typed_overlay()` to a `TypedOverlay::Directory` with `container: "~/.ssh"`; implement `"skill"` rules (`skill(*)` → All, `skill(name)` → Named, `skill()` / any `skills(...)` → parse error with migration message); implement `"env"` tag; introduce `CollectedOverlays` struct with `directories`, `include_all_skills`, `named_skills`, and `env_passthrough` fields; extend `collect_all_overlay_specs()` to accept `step_overlays: Option<&[String]>`, parse all config sources as `Vec<String>` via `parse_overlay_list()`, apply union merge rules, and return `Result<CollectedOverlays, CommandError>`; fix the silent-drop bug for malformed `AWMAN_OVERLAYS`; remove any call to `EffectiveConfig::env_passthrough()`.
- **`src/data/workflow_definition.rs`** (Layer 0) — add `overlays: Option<Vec<String>>` to `WorkflowStep` and raw TOML/YAML structs; propagate in `raw_to_steps()`. Introduce `SetupStepEntry` and `TeardownStepEntry` wrapper structs (with `#[serde(flatten)]` inner step and `overlays: Option<Vec<String>>`); update `Workflow.setup` and `Workflow.teardown` to use them. Add validation that rejects any `skill(...)` expression in setup/teardown overlays (note: `skills(...)` never reaches validation — it fails at parse time before workflow loading completes).
- **`src/engine/overlay/mod.rs`** (Layer 1) — replace `include_skills: bool` in `OverlayRequest` with `include_all_skills: bool` + `named_skills: Vec<String>`; remove `mount_ssh: bool` (SSH is now a plain directory entry); extend `resolve_user_overlay()` to expand leading `~/` in container paths using `request.container_home` (defaulting to `/root`); update `skill_overlays()` signature to accept `include_all: bool, names: &[String]`; update `build_overlays()` accordingly.
- **`src/engine/agent/mod.rs`** (Layer 1) — replace `include_skills: bool` in `AgentRunOptions` with `include_all_skills: bool` + `named_skills: Vec<String>`; remove `mount_ssh: bool`; remove the `ContainerOption::MountSsh` emission block; update `build_options()` to populate `container_home` on `OverlayRequest` from the agent's detected `dockerfile_user`.
- **`src/engine/container/docker.rs`** (Layer 1) — remove the `ContainerOption::MountSsh` branch (lines 650–658); remove `ContainerOption::MountSsh` from `src/engine/container/options.rs`.
- **`src/command/commands/exec_workflow.rs`** (Layer 2) — update `CommandLayerFactory`: remove `include_skills: bool`, remove `mount_ssh` field, use `CollectedOverlays` including `env_passthrough`, `include_all_skills`, `named_skills`; call `collect_all_overlay_specs` with `step.overlays.as_deref()` in `execution_for_step`. Also apply `collect_all_overlay_specs` for each `SetupStepEntry` / `TeardownStepEntry` and thread the resulting `env_passthrough` and `directories` into the subprocess calls for those steps.
- **`src/command/commands/{chat,exec_prompt}.rs`** (Layer 2) — remove `mount_ssh` flag field; update calls to `collect_all_overlay_specs` to pass `None` for step overlays.
- **`src/command/dispatch/catalogue.rs` and `dispatch/mod.rs`** (Layer 2) — remove `--mount-ssh` flag definitions and parsing; add rejection error for callers who pass the old flag.
- **`aspec/uxui/cli.md`** — remove `--mount-ssh` from the `chat`, `exec prompt`, and `exec workflow` tables; document `--overlay ssh()` as the replacement.
- **`aspec/workflows/implement-pr.toml`** — add `overlays = ["ssh()"]` to the `push_branch` teardown step and `overlays = ["env(GITHUB_TOKEN)"]` to the `create_pull_request` teardown step.
- All new public functions and types: unit tests in the same module per project convention. Typed structs over raw tuples per the grand architecture's tenet 3.

## Documentation

After implementation is complete:

**Rename existing numbered docs to make room:** `08-api-mode.md` → `09-api-mode.md`, `09-remote-mode.md` → `10-remote-mode.md`, `10-architecture-overview.md` → `11-architecture-overview.md`. Update any cross-links between docs files to reflect the new names.

**Create `docs/08-overlays.md`** — a dedicated end-user guide covering the unified overlay system, inserted at position 08 (after configuration, before API mode). Follow the same format as the other guide files: `# Title`, horizontal rules between sections, prose intro, fenced code blocks for examples, and a table where appropriate. This file is the single authoritative reference for all overlay types and should include:
- What overlays are and how they compose across sources (global config → repo config → `AWMAN_OVERLAYS` → `--overlay` flags → per-step `overlays` array)
- All supported overlay types with syntax and examples: `dir(host:container[:ro|rw])`, `ssh()`, `env(VAR_NAME)`, `skill(*)`, `skill(name)`
- How to use overlays in workflow TOML/YAML files (per-step, setup, teardown)
- How to set overlays in repo and global config files
- How to use `--overlay` on the CLI and `AWMAN_OVERLAYS` in the environment
- Merge semantics: least-permissive-wins for directories, union/additive for skills and env
- Common patterns: SSH for git operations, env vars for API tokens, named skills for isolated steps

**Audit existing `docs/` files** for any overlays-related content (particularly `docs/07-configuration.md`, which has outdated `overlays` and `envPassthrough` config examples) and replace inline overlays explanations with a brief description and a link to `docs/08-overlays.md`. Do not duplicate content across files.

**Do not** create implementation notes, architecture diagrams, or work-item-specific content in `docs/`. Those belong in the work item spec or code comments.

See `CLAUDE.md` for documentation standards.
