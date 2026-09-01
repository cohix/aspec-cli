# Work Item: Feature

Title: poll_ci and step on_failure
Issue: issuelink

## Summary:
Two new features for awman workflow setup and teardown phases:

1. **`poll_ci` step type** — A new step variant (available in both setup and teardown) that polls GitHub for the CI run status associated with the current branch/commit/PR. Polling interval and max retry count are configurable on the step definition, defaulting to 30s interval and 10 retries. Uses the `gh` CLI (via container exec) if available and authenticated; falls back to direct GitHub API calls via `reqwest` from the host process. All polling events, action status, and errors are emitted to the message sink.

2. **Step `on_failure`** — An optional `on_failure` object on any setup or teardown step definition. When a step fails, awman launches an agent container with the configured prompt to attempt to fix the problem, then retries the failed step. This repeats up to `max_attempts` times. Agent and model default to the workflow's configured defaults if not specified on the `on_failure` object.

## User Stories

### User Story 1:
As a: user

I want to: add a `poll_ci` step to my teardown phase so awman waits for GitHub Actions to go green before completing the workflow

So I can: ensure the full CI pipeline passes on the pushed branch before the workflow is considered done, without writing custom polling shell scripts.

### User Story 2:
As a: user

I want to: configure an `on_failure` block on my `RunShell` test step so that if tests fail, an agent automatically attempts to fix the code and the tests are re-run

So I can: have awman autonomously resolve transient or fixable test failures as part of a fully automated workflow, without requiring manual intervention.

### User Story 3:
As a: user

I want to: configure an `on_failure` block on a `poll_ci` step so that if CI fails, an agent can attempt fixes and the CI check is re-polled after a new push

So I can: chain automated remediation and CI validation into a single self-healing workflow loop.


## Implementation Details:

### 1. Schema changes — `src/data/workflow_definition.rs`

**Add `PollCi` variant to both `SetupStep` and `TeardownStep` enums:**
```rust
PollCi {
    #[serde(default)]
    interval_secs: Option<u32>,   // default: 30
    #[serde(default)]
    max_retries: Option<u32>,     // default: 10
},
```

**Add `RemediationConfig` struct:**
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemediationConfig {
    pub prompt: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    pub max_attempts: u32,
}
```

**Add `on_failure` field to both `SetupStepEntry` and `TeardownStepEntry`:**
```rust
#[serde(default)]
pub on_failure: Option<RemediationConfig>,
```

---

### 2. `poll_ci` execution — `src/engine/workflow/mod.rs`

`PollCi` is **not** translated to a shell command in `step_commands.rs`. Instead, the setup and teardown phase runners in the engine detect this variant and execute native Rust polling logic directly. This avoids the awkwardness of a long-running shell loop and allows clean per-poll message sink updates.

**Polling logic (pseudo-code, runs on the host in the engine):**
```
let interval = step.interval_secs.unwrap_or(30);
let max_retries = step.max_retries.unwrap_or(10);
for attempt in 1..=max_retries {
    msg_info("Polling CI (attempt {attempt}/{max_retries})...");
    let result = fetch_ci_status(git_root, &msg_sink)?;  // gh CLI or reqwest
    match result {
        CiStatus::NotFound       => return Err(StepFailed("No CI run found"));
        CiStatus::Running        => { sleep(interval); continue; }
        CiStatus::Success        => return Ok(());
        CiStatus::Failed(detail) => return Err(StepFailed(detail));
    }
}
return Err(StepFailed("CI did not complete within max_retries attempts"));
```

**`fetch_ci_status` implementation — `src/engine/workflow/poll_ci.rs` (new file):**
- Detect the current branch via `git rev-parse --abbrev-ref HEAD` (run via `std::process::Command` on the host).
- **Primary path:** check if `gh` is available on the host (`which gh`) and authenticated (`gh auth status`). If yes, run `gh run list --branch <branch> --json status,conclusion,name,headSha --limit 5` and parse JSON.
- **Fallback path:** use `reqwest` (blocking or async via tokio) with `GITHUB_TOKEN` env var to call `GET /repos/{owner}/{repo}/actions/runs?branch={branch}&per_page=5`. Parse the JSON response for the most recent run matching the current HEAD commit SHA.
- Return a `CiStatus` enum: `NotFound`, `Running`, `Success`, `Failed(String)`.
- All intermediate results and errors are forwarded to the message sink before returning.

**GitHub repo detection:** parse `git remote get-url origin` to extract `owner/repo` for both the API fallback and for `gh` disambiguation when multiple remotes exist.

---

### 3. Step `on_failure` handling — `src/engine/workflow/mod.rs`

The `on_failure` retry loop wraps the existing step execution logic in both `run_setup` and `run_teardown`. Extract a helper `run_single_step(entry, container) -> StepOutcome` so the loop is not duplicated.

**Remediation loop logic:**
```
let outcome = run_single_step(entry, container);
if outcome.failed() {
    if let Some(ref rem) = entry.on_failure {
        for attempt in 1..=rem.max_attempts {
            msg_info("Step failed — launching on_failure agent (attempt {attempt}/{rem.max_attempts})...");
            let agent = rem.agent.as_deref().unwrap_or(workflow_default_agent);
            let model = rem.model.as_deref().or(workflow_default_model);
            launch_on_failure_agent(rem.prompt, agent, model, &runtime).await;
            let retry = run_single_step(entry, container);
            if retry.succeeded() { break; }
            if attempt == rem.max_attempts {
                // step fully fails
            }
        }
    }
}
```

**`launch_on_failure_agent`:** reuses the existing `ContainerExecutionFactory` machinery (same as main workflow steps). Construct a `WorkflowStep`-equivalent using `rem.prompt`, resolved agent, and resolved model. Launch via `ContainerExecution::wait()`. If `--yolo` is active and the container becomes stuck, treat it as a completed run and proceed to the step retry (matching the existing stuck-container behavior for main steps).

**State/UI:** emit `msg_info` messages before launching the on_failure agent and before each retry. Update `PhaseStepStatus` to reflect that on_failure is in progress (add a `Remediating { attempt: u32, of: u32 }` variant to `PhaseStepStatus` in `workflow_state.rs` and update any frontends that render it).

---

### 4. Dependencies — `Cargo.toml`

Add `reqwest` (with `json` and `blocking` features, or async with `tokio`) if not already present, scoped to the `awman-engine` crate or whichever crate owns `poll_ci.rs`. Add `serde_json` if not already available for JSON parsing.

---

### 5. TOML/YAML config examples (for docs)

```toml
# Setup step
[[setup]]
type = "poll_ci"
interval_secs = 60
max_retries = 15

# Teardown step with on_failure
[[teardown]]
type = "run_shell"
command = "cargo test"
[teardown.on_failure]
prompt = "The test suite failed. Review the output above and fix any failing tests."
max_attempts = 2
```


## Edge Case Considerations:

- **No CI run found:** The branch was just pushed and GitHub hasn't created a run yet. The first few polls may return `NotFound`. Treat `NotFound` as `Running` for the first N polls (e.g., first 3 attempts) before hard-failing, with a clear message distinguishing "not found yet" from "definitely absent."
- **Multiple CI runs:** If multiple runs exist for the branch (re-runs, duplicate triggers), use the run whose `head_sha` matches the current `HEAD` commit. If no run matches HEAD, use the most recent run on the branch with a warning message.
- **`gh` available but unauthenticated:** `gh auth status` exits non-zero. Fall through to the reqwest path cleanly without surfacing a confusing error from `gh`.
- **`GITHUB_TOKEN` absent for reqwest fallback:** Emit a clear error message via the message sink that neither `gh` nor a `GITHUB_TOKEN` env var is available, and fail the step immediately rather than making unauthenticated API calls that will 403.
- **`on_failure` agent exits non-zero:** Treat as completed regardless of the agent's own exit code — the retry of the original step is what determines success, not the agent's exit code.
- **`max_attempts = 0`:** Treat as invalid config; surface a validation error at workflow parse time (not at runtime).
- **`poll_ci` with on_failure:** A `poll_ci` step that fails (CI red) can have a `on_failure` block. After the agent runs, the branch may have new commits. The retry poll should re-detect the HEAD SHA so it polls for the new CI run, not the old failed one.
- **Container isolation:** The on_failure agent container runs with the same overlay/mount configuration as a regular workflow step (same `git_root` mount, same security constraints). The on_failure prompt receives no automatic context about what failed — that must be included explicitly in the `prompt` field by the user.
- **`abort_on_failure` + on_failure:** If a step has both `abort_on_failure: true` and a `on_failure` block, the on_failure loop runs first. Only if all on_failure attempts are exhausted does `abort_on_failure` trigger.
- **Teardown best-effort semantics:** Teardown step failures already do not abort the remaining teardown. Remediation should not change this — if on_failure exhausts `max_attempts`, the step is marked failed and teardown continues.
- **Rate limiting:** GitHub API rate-limits unauthenticated requests aggressively. The reqwest fallback must include the `Authorization: Bearer <token>` header and surface rate-limit responses (HTTP 403/429) as a specific error message rather than a generic failure.


## Test Considerations:

- **Unit tests for `poll_ci.rs`:**
  - Test `fetch_ci_status` with mocked `gh` CLI responses (success, failure, running, not-found).
  - Test JSON parsing of the GitHub API response format for all status/conclusion combinations (`queued`, `in_progress`, `completed`/`success`, `completed`/`failure`, `completed`/`cancelled`).
  - Test HEAD SHA extraction and run matching when multiple runs are present.
  - Test the `gh`-unavailable/unauthenticated fallback path triggers correctly.
  - Test that missing `GITHUB_TOKEN` produces the correct error without making any HTTP calls.

- **Unit tests for on_failure logic in `mod.rs`:**
  - Test that a step that fails but has no `on_failure` block fails immediately.
  - Test that a step that fails with `on_failure` launches the agent and retries.
  - Test that success on retry (attempt 1 of 2) stops the loop and marks the step succeeded.
  - Test that exhausting `max_attempts` marks the step failed.
  - Test that on_failure agent exit code does not affect whether the retry runs.
  - Test `abort_on_failure` + on_failure interaction.

- **Unit tests for schema (`workflow_definition.rs`):**
  - Test TOML and YAML deserialization of `PollCi` with and without optional fields (verify defaults are applied).
  - Test deserialization of `RemediationConfig` with and without `agent`/`model`.
  - Test that `max_attempts = 0` is rejected at validation time.

- **Integration tests:**
  - A workflow with a `poll_ci` step that succeeds (mock GitHub API or use a test double).
  - A workflow where a `RunShell` step fails, on_failure runs, and the step succeeds on retry.
  - A workflow where on_failure exhausts all attempts and the step fully fails.
  - A teardown `poll_ci` with on_failure that fails CI, runs the agent, and re-polls successfully.

- **Message sink output:** Assert that the correct info/warning/error messages are emitted at each polling attempt, on on_failure launch, and on retry — ensuring users see actionable status updates throughout.


## Codebase Integration:

- Follow the established conventions, best practices, testing, and architecture patterns from the project's aspec.
- **Schema changes** go in `src/data/workflow_definition.rs`. Add `PollCi` to both `SetupStep` and `TeardownStep` enums; add `RemediationConfig` struct; add `on_failure` field to `SetupStepEntry` and `TeardownStepEntry`. Keep `serde` attribute conventions consistent with existing fields (use `#[serde(default)]` for all optional fields).
- **`step_commands.rs`** does NOT need a case for `PollCi` — the engine detects this variant before calling into the command translator. Add a `unreachable!()` or explicit compile-time guard to make this clear.
- **`poll_ci.rs`** is a new file under `src/engine/workflow/`. Register it as a module in `src/engine/workflow/mod.rs`.
- **Remediation loop** belongs inside the existing `run_setup` / `run_teardown` functions in `src/engine/workflow/mod.rs`. Extract step execution into a `run_single_phase_step` helper to avoid duplicating the retry logic. The on_failure agent launch reuses `ContainerExecutionFactory::execution_for_step` with a synthetic `WorkflowStep` carrying the on_failure prompt, agent, and model.
- **`PhaseStepStatus`** in `src/data/workflow_state.rs` needs a `Remediating { attempt: u32, of: u32 }` variant. Update all match sites (TUI renderer, any frontend that renders setup/teardown step status) to handle this new variant.
- **Message sink usage:** call `self.msg_info(...)` / `self.msg_warning(...)` / `self.msg_error(...)` (or the equivalent helpers on the engine) — do not print directly to stdout/stderr.
- **`reqwest` dependency:** add only if not already present. Prefer async (`reqwest` with the tokio runtime already in use) over blocking. Scope it to the engine crate. Use `serde_json::Value` for JSON parsing to avoid coupling to a rigid GitHub response struct that may change.
- **Security:** never log the `GITHUB_TOKEN` value to the message sink. Treat it as a secret; only emit masked or absent/present status.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** (e.g., if implementing headless features, update `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0123 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.
