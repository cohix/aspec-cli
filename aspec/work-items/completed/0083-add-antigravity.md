# Work Item: Feature

Title: Add Google Antigravity 2.0 CLI agent
Issue: issuelink

## Summary:
- Add `antigravity` (`agy`) as a new supported agent in awman, with full config copy-and-mount, auth passthrough, and headless mode flags (`--print`, `--dangerously-skip-permissions`, `--approval-mode`).
- Antigravity replaces Gemini CLI at Google. When any command resolves the agent as `gemini`, emit a deprecation warning to the message sink before the container launches.

## User Stories

### User Story 1:
As a: user who wants to run Google's latest AI coding agent

I want to: run `amux chat antigravity` or set `agent = "antigravity"` in my repo config

So I can: get an Antigravity 2.0 (`agy`) session inside a sandboxed container with my credentials and settings automatically passed through, the same way `claude` and `gemini` sessions work today.

### User Story 2:
As a: user running non-interactive workflows with Antigravity

I want to: use `--yolo`, `--auto`, and `--plan` flags with the `antigravity` agent

So I can: run headless `amux exec-prompt --agent antigravity --yolo "fix the tests"` with full autonomous approval, or a plan-mode review pass, without manually knowing Antigravity's CLI flags.

### User Story 3:
As a: user who has been using the `gemini` agent

I want to: see a clear deprecation warning when I launch a `gemini` session

So I can: know that Google has deprecated Gemini CLI in favor of Antigravity and plan my migration without being surprised by a broken agent.


## Implementation Details:

### 1. Add `antigravity` to the agent matrix (`src/engine/agent/agent_matrix.rs`)

Add `"antigravity"` to `SUPPORTED_AGENTS`:

```rust
pub const SUPPORTED_AGENTS: &[&str] = &[
    "claude", "codex", "opencode", "maki", "gemini", "copilot", "crush", "cline", "antigravity",
];
```

Add a match arm in `matrix_for()`:

```rust
"antigravity" => AgentMatrix {
    agent: "antigravity",
    interactive_entrypoint: vec!["agy"],
    non_interactive_flag: Some("--print"),
    plan_flag: Some(&["--approval-mode=plan"]),
    yolo_flag: Some("--dangerously-skip-permissions"),
    auto_flag: Some(&["--approval-mode=auto_edit"]),
    disallowed_tools_flag: None,
    allowed_tools_flag: None,
    model_flag: ModelFlagDelivery::Unsupported,
    supports_stdin_injection: false,
},
```

`model_flag` is `Unsupported` because `agy` has no `--model` CLI flag; model selection is configured via `~/.gemini/antigravity-cli/settings.json` or the `/model` in-session slash command. Attempting to pass `--model` to `agy` would cause an error, so we reject it early.

### 2. Auth paths (`src/data/fs/auth_paths.rs`)

Add an `"antigravity"` arm to `resolve()`. Antigravity stores its OAuth credentials and session state under `~/.gemini/antigravity-cli/`:

```rust
"antigravity" => AgentAuthPaths {
    agent: agent.to_string(),
    config_file: None,
    settings_dir: Some(self.home.join(".gemini").join("antigravity-cli")),
},
```

No config-file scrubbing is needed (unlike Claude); the directory is mounted as-is, matching the gemini passthrough pattern.

### 3. Container overlay (`src/engine/overlay/mod.rs`)

In `agent_settings_overlays_with()`, add an `"antigravity"` arm alongside gemini:

```rust
"antigravity" => {
    if let Some(dir) = paths.settings_dir.as_ref() {
        if dir.exists() {
            out.push(OverlaySpec {
                host_path: dir.clone(),
                container_path: PathBuf::from(format!(
                    "{container_home}/.gemini/antigravity-cli"
                )),
                permission: OverlayPermission::ReadWrite,
            });
        }
    }
}
```

In `skill_overlays()`, add an `"antigravity"` arm mapping the global amux skills dir to the path Antigravity scans for slash-command skills:

```rust
"antigravity" => format!("{container_home}/.gemini/antigravity-cli/skills"),
```

### 4. Gemini deprecation warning

In each of the three command entry points — `src/command/commands/chat.rs`, `src/command/commands/exec_prompt.rs`, and `src/command/commands/exec_workflow.rs` — immediately after `resolve_agent()` returns `Ok`, add:

```rust
if agent.as_str() == "gemini" {
    frontend.write_message(UserMessage {
        level: MessageLevel::Warning,
        text: "The 'gemini' agent is deprecated by Google. \
               Migrate to 'antigravity' — run: amux agent install antigravity".to_string(),
    });
}
```

The warning is written before the PTY is activated so it is immediately visible in both interactive and headless runs. It does not block execution; the gemini session still starts.

### 5. API key env-var passthrough (documentation / default config)

Antigravity supports `ANTIGRAVITY_API_KEY` for headless API-key auth (in addition to OAuth). No special engine code is required — users can use the standard `--env-passthrough ANTIGRAVITY_API_KEY` flag, `AWMAN_ENV_PASSTHROUGH=ANTIGRAVITY_API_KEY` env var, or add `"ANTIGRAVITY_API_KEY"` to `env_passthrough` in their repo or global config. Document this in `docs/09-agents.md` (or the relevant agent reference doc).


## Edge Case Considerations:

- **`--model` flag with `antigravity`** — `agy` has no `--model` CLI flag; model is set in `~/.gemini/antigravity-cli/settings.json`. `ModelFlagDelivery::Unsupported` in the matrix causes the engine to return a clear error before the container is launched: _"agent 'antigravity' does not support a model flag"_. Users should configure the model via the settings file instead.
- **`~/.gemini/antigravity-cli/` does not exist** — first-time users will not have this directory. The overlay block checks `dir.exists()` and silently skips the mount (same behavior as gemini). `agy` creates the directory on first interactive run and will generate a new OAuth session inside the container; the user will be prompted to authenticate.
- **`~/.gemini/antigravity-cli/` and `~/.gemini/` mounted simultaneously** — if a user also launches a gemini session in a parallel worktree and gemini's overlay mounts `~/.gemini`, there is no conflict because amux worktrees are independent Docker invocations each with their own bind-mount namespace.
- **Gemini deprecation warning in headless/workflow mode** — `write_message` queues messages while the PTY is active and drains them after. In headless mode (`--print`), the PTY is never activated, so the warning is written to stderr immediately before the container's stdout starts, preserving parse-friendly output.
- **Gemini agent used in a multi-step workflow** — the deprecation warning is emitted once per `resolve_agent()` call. In a workflow where every step uses `gemini`, the warning fires once per step. This is acceptable and consistent with how info messages work.
- **User has both `~/.gemini/` (Gemini) and `~/.gemini/antigravity-cli/` (Antigravity)** — the two directories are distinct. Running `amux chat gemini` mounts `~/.gemini` and running `amux chat antigravity` mounts `~/.gemini/antigravity-cli`. They do not interfere.
- **`ANTIGRAVITY_API_KEY` not set but `agy` expects it in headless env** — `agy` falls back to OAuth; if OAuth credentials are not mounted either, it will error inside the container. The failure message from `agy` itself is sufficient; no special pre-flight check is needed.
- **Binary named `agy` not in Dockerfile.dev** — the Dockerfile.dev must install `agy`. This work item assumes the Dockerfile.dev is updated as part of the agent-install step. If `agy` is missing, the container will exit immediately with "command not found"; the engine surfaces this as a non-zero exit and reports it.
- **`model_flag: ModelFlagDelivery::Unsupported` must not panic** — confirm that `model_flag_for()` with `Unsupported` returns `Err`, not panics, and that the calling site in `AgentEngine::build_options()` propagates the error cleanly.


## Test Considerations:

**Unit tests (`src/engine/agent/agent_matrix.rs`):**
- `matrix_for("antigravity")` returns `Ok` and the existing `matrix_supports_all_agents` test covers it automatically once the agent is added.
- `matrix_for("antigravity").yolo_flag == Some("--dangerously-skip-permissions")`.
- `matrix_for("antigravity").non_interactive_flag == Some("--print")`.
- `model_flag_for(&matrix, "gemini-3.5-flash")` returns `Err` (Unsupported).

**Unit tests (`src/data/fs/auth_paths.rs`):**
- `resolve("antigravity").settings_dir == Some(home.join(".gemini/antigravity-cli"))`.
- `resolve("antigravity").config_file == None`.

**Unit tests (`src/engine/overlay/mod.rs`):**
- When `~/.gemini/antigravity-cli` exists, `agent_settings_overlays_with("antigravity", ...)` returns one spec with `container_path` ending in `.gemini/antigravity-cli`.
- When `~/.gemini/antigravity-cli` does not exist, the overlay list is empty.
- `skill_overlays("antigravity", ...)` returns a spec whose `container_path` ends in `.gemini/antigravity-cli/skills`.

**Integration / command tests:**
- `amux chat antigravity` (dry-run / mock docker) builds a container argv that includes `agy` as the entrypoint.
- `amux exec-prompt --agent antigravity --yolo "prompt"` includes `--print` and `--dangerously-skip-permissions` in argv.
- `amux exec-prompt --agent antigravity --plan "prompt"` includes `--approval-mode=plan`.
- `amux chat gemini` emits a `Warning`-level message containing the word "deprecated" before the container starts.
- `amux exec-workflow --agent gemini workflow.toml` also emits the deprecation warning.
- `amux chat antigravity --model gemini-3.5-flash` returns a non-zero exit with an error message that names `antigravity` and "does not support a model flag".


## Codebase Integration:

- **`src/engine/agent/agent_matrix.rs`** — only file that branches on agent name in the engine; add `"antigravity"` to `SUPPORTED_AGENTS` and `matrix_for()`. The existing `matrix_supports_all_agents` unit test will catch any mismatch between the two.
- **`src/data/fs/auth_paths.rs`** — add `"antigravity"` arm in `resolve()`; mirror the gemini pattern (settings_dir only, no config_file). Add corresponding unit test following the `resolve_gemini_has_only_settings_dir` pattern.
- **`src/engine/overlay/mod.rs`** — add `"antigravity"` arm in `agent_settings_overlays_with()` and in the `container_path` match inside `skill_overlays()`. No sanitization needed (no secrets in the settings dir beyond OAuth tokens that `agy` manages itself).
- **`src/command/commands/chat.rs`**, **`exec_prompt.rs`**, **`exec_workflow.rs`** — add the gemini deprecation warning block immediately after `resolve_agent()` succeeds, before any overlay resolution or PTY activation. Follow the existing `UserMessage { level: MessageLevel::Warning, text: ... }` call pattern already present in these files.
- Do not add a `Dockerfile.dev` entry for `agy` in this work item — agent image changes belong in a separate Dockerfile work item. Add a `TODO` comment in the matrix entry noting that the image must include `agy`.
- Follow the existing `insert_or_merge()` / `OverlayPermission::ReadWrite` patterns throughout; do not invent parallel mechanisms.
- All new public functions and match arms: unit tests in the same module, following existing test structure.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing agent reference doc** (e.g., `docs/09-agents.md` or equivalent) to list `antigravity` as a supported agent, document `ANTIGRAVITY_API_KEY` passthrough for headless use, and note that `--model` is not supported via CLI.
- **Add a migration note** in the same doc explaining that `gemini` is deprecated in favor of `antigravity` and that users will see a warning when running gemini sessions.
- **Never create work-item-specific docs** (e.g., no "WI 0083 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.
