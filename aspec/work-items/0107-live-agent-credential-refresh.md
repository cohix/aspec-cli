# Work Item: Feature

Title: Live agent credential refresh for containerized agents
Issue: n/a

## Summary:
- Containerized Claude agents currently receive a frozen snapshot of the host keychain's OAuth **access token** as a `CLAUDE_CODE_OAUTH_TOKEN` env var. The token expires after ~8–12 hours, env vars are read once at process start, and awman has no expiry/refresh/401 handling — so any step that outlives the token fails with 401s mid-run. This is the largest user complaint about awman.
- Replace env-var delivery (for container-class runtimes) with a **live-updated credential file**: awman writes a sanitized `.credentials.json` (access token only, **never** the refresh token) into the already-mounted staged `~/.claude` overlay, and keeps it fresh for as long as the session lives. Claude Code re-reads that file mid-session (validated empirically on Apple Containers with CLI 2.1.247: a running session picked up a token swapped into the mounted file between two API calls), so refreshes heal running steps with no restart and no user action.
- Host-side token refresh stays where it is officially supported: the sanctioned `ready`-check agent ping (`ReadyPhase::CheckingLocalAgent`) is extended to run **periodically while credentialed sessions are live**, causing the host agent to rotate its own keychain entry. awman never calls the OAuth token endpoint and never lets a container see the (single-use) refresh token — a container-side refresh would consume it and invalidate the host login, and parallel containers would race each other for it.
- The engine implementation must be **agent-generic**: a credential-refresh abstraction that Claude merely plugs into, so a future agent with a similar rotating-credential need is a new descriptor, not a new subsystem.
- Live-session tracking must be **robust by construction**: RAII leases registered at the single container-spawn choke point guarantee (a) no refresh writes for containers that are already dead and (b) no running credentialed container is ever missed.

## User Stories

### User Story 1:
As a: user

I want to:
run long unattended workflows (overnight, multi-step, parallel) with my Claude Pro/Max subscription

So I can:
have every step authenticate successfully regardless of how long the run takes, without ever manually refreshing a token or restarting a step.

### User Story 2:
As a: user

I want to:
see auth health (token time-to-expiry, refresh activity, refresh failures) surfaced in `awman ready` and in running frontends

So I can:
trust that unattended runs will stay authenticated, and diagnose the rare failure instead of hitting silent 401s.

### User Story 3:
As a: developer (awman contributor)

I want to:
add credential refresh for a future agent type by implementing a small descriptor (read host credential → extract expiry → materialize container file)

So I can:
reuse the monitor, lease registry, and delivery machinery without touching them.

## Implementation Details:

### 1. Generic credential model (engine/auth)

- Extend the keychain read (`src/engine/auth/keychain.rs:62`, `claude_keychain_credentials`) to parse the full payload into a generic snapshot instead of discarding everything but the access token:
  - `CredentialSnapshot { secret: SecretString, expires_at: Option<SystemTime>, extra: <agent-specific fields needed for materialization, e.g. scopes/subscriptionType> }`
  - The refresh token and `refreshTokenExpiresAt` MUST NOT be captured into the snapshot at all — parse and drop, so no code path can leak them downstream.
- Introduce an agent-agnostic descriptor owned by `AuthEngine` (`src/engine/auth/mod.rs`), one instance per agent kind that opts in:
  - `RefreshableCredentialSpec` (name illustrative):
    - `read(&HostCredentialSource) -> Option<CredentialSnapshot>` — how to read the current credential from the host (macOS keychain service, or a host file path via `AuthPathResolver` on Linux/fallback hosts).
    - `expiry(&CredentialSnapshot) -> Option<SystemTime>`
    - `materialize(&CredentialSnapshot) -> CredentialFile { relative_path, contents, mode }` — how to render the credential into the agent's staged settings overlay (for Claude: `.claude/.credentials.json` containing `claudeAiOauth` with `accessToken`, `expiresAt`, `scopes`, `subscriptionType` — no `refreshToken` key).
    - `host_refresh() -> HostRefreshAction` — how to cause the host to rotate the credential. For Claude this is the existing sanctioned ready-check ping; see §4.
  - Claude is the only implementation in this work item. The existing agent-name dispatch in `keychain.rs:42` becomes a lookup into these descriptors. Agents without a descriptor keep today's behavior exactly.
- `AgentCredentials` grows a variant/flag distinguishing **env-delivered** credentials (unchanged path) from **file-delivered refreshable** credentials, resolved per agent by `resolve_agent_auth` (`src/engine/auth/mod.rs:182`). The `keychain`/`passthrough`/`none` auth-mode config semantics (`docs/07-configuration.md`) are unchanged; `keychain` mode for Claude now yields the file-delivered form on container-class runtimes.

### 2. Delivery: credential file in the staged overlay (engine/overlay)

- For container-class runtimes, **stop emitting `CLAUDE_CODE_OAUTH_TOKEN`** for Claude. Claude Code's credential precedence ranks that env var (rank 5) above the credentials file (rank 7), so leaving it set would mask the refreshable file. The name-only `-e KEY` argv machinery (`src/engine/container/docker.rs:1254`, work item 0098) stays in place for agents that remain env-delivered.
- During overlay staging (`sanitize_claude_settings_dir`, `src/engine/overlay/mod.rs:663`), write the materialized credential file into the staged `~/.claude` TempDir with mode 0600 before the container starts. No new mounts, no new runtime features — this rides the existing RW bind mount, which is why it works identically on Docker and Apple Containers (validated on Apple Containers; both runtimes propagate host writes inside a bind-mounted directory).
- Add `.credentials.json` to `CLAUDE_DENYLIST` (`src/engine/overlay/mod.rs:20`). Today a host `~/.claude/.credentials.json` (always present on Linux hosts; present on macOS when a keychain write ever failed) is silently copied into the staged dir and mounted RW — leaking the live refresh token into every container and contradicting `docs/03-agent-sessions.md` ("credentials are never mounted as files"). After this change the only credential file a container ever sees is the awman-authored, refresh-token-free one.
- sbx (Docker Sandboxes) runtime is out of scope and unchanged: it continues to drop OAuth credentials (`src/engine/sandbox/dsbx/auth.rs:366`) and require `ANTHROPIC_API_KEY` or in-sandbox login.

### 3. Live-session tracking: credential leases (engine)

New engine component, e.g. `CredentialRefreshMonitor`, owning a lease registry. Design constraints, in priority order: never write to a dead session's path, never miss a live credentialed container, survive all exit paths (success, error, panic, user cancel).

- **Lease = RAII guard.** `CredentialLease` holds: agent kind, absolute path of the staged credential file, session/container identity for logging, and a monotonically increasing generation id. Deregistration happens in `Drop`. The lease guard is owned by the container-instance value whose lifetime brackets the spawned child process (`DockerContainerInstance::run_with_frontend`, `src/engine/container/docker.rs:477`, and the Apple backend equivalent in `src/engine/container/apple.rs`), so every exit path — normal exit, spawn failure, error propagation, panic unwind, Ctrl-C teardown — releases the lease without any code path having to remember to.
- **Registration at the choke point, not per caller.** The lease is created inside the shared container-run path at the moment resolved options carry file-delivered agent credentials — not in the per-command frontends (`chat`, `exec prompt`, `exec workflow`, `specs`, `new`, amie). This is what makes "no missed containers" structural: there is no way to spawn a credentialed container without passing through `ResolvedContainerOptions::resolve` + the run path. Add a debug assertion + test: any spawn whose options contain a file-delivered credential must hold a lease before the child process is spawned (registered-before-spawn ordering also closes the startup race where a token could expire between staging and first request).
- **Dead-session safety (beyond RAII), defense in depth:**
  - The monitor snapshots the lease list at each tick; before writing, it re-checks the lease is still registered (generation id match) and that the target file still exists. A lease dropped mid-tick loses the race safely: writing is skipped, and a write to an already-removed TempDir path is treated as a skip, not an error.
  - TempDir lifetime: staged overlay dirs are retained by `OverlayEngine` (`retain_tempdir`, `src/engine/overlay/mod.rs:148`) and outlive the container, so the monitor can never write into a recycled path while a lease is live. The lease must be dropped **before** the corresponding TempDir can be dropped; tie the ordering with the lease held inside the container instance, which is dropped before engine teardown.
  - One-shot vs background containers: only agent containers (which carry agent credentials) get leases. Workflow setup/teardown background containers (`src/engine/container/background.rs`) never receive agent credentials today and are unaffected.
- **Monitor loop:** runs only while the registry is non-empty (started on first lease, parked when empty — no idle polling in `awman` invocations that never launch agents). Tick every ~60s:
  1. For each distinct agent kind with live leases, read the host credential via its descriptor (a `security` shell-out; cheap).
  2. If `expires_at − now < refresh_threshold` (default 20 min, configurable), trigger the descriptor's `host_refresh()` (§4), then re-read.
  3. If the snapshot changed since the last materialization (compare a fingerprint, not the secret itself, for logging), atomically rewrite every live lease's file: write to a temp file **in the same staged directory**, `chmod 0600`, then `rename` over the target (rename within a bind-mounted directory is visible to the container on both runtimes; never write the target in place, so a container never observes a partially written JSON).
  4. Record per-lease refresh results for status surfacing; failures warn (frontend status line / log) and retry next tick with backoff — never fatal to the session.

### 4. Host refresh trigger (engine/ready + security spec)

- Reuse the existing sanctioned local-agent check (`src/engine/ready/mod.rs:449`) as the refresh action: same hardcoded greeting, no working directory, no repo data, cheapest available model. Factor it so `awman ready` and the monitor share one implementation.
- Amend `aspec/architecture/security.md`: the sole host-side agent-execution exception is widened from "the ready check" to "the ready check, invoked (a) during `awman ready` and (b) periodically by the credential-refresh monitor while credentialed agent sessions are live, solely to cause the host agent to rotate its own credential." The invariants (hardcoded prompt, no user input, no repo content, no working directory) are unchanged and restated.
- Also amend the credential-delivery rule: from "env var only, never files" to "awman-authored, refresh-token-free credential file inside the staged settings overlay (0600, atomically replaced); the host's own credential files and refresh tokens are never mounted."
- After a triggered refresh, verify the keychain `expiresAt` actually advanced; if it did not (network down, keychain locked, host agent logged out), surface a warning with remediation ("run `claude` on the host / check login") and keep the last-known-good file in place — an unexpired last-known token is still valid.

### 5. Workflow resilience (command layer)

- Pre-step guard: before launching a workflow step with a file-delivered credential, if `expires_at − now < refresh_threshold`, trigger a refresh synchronously first (bounded wait), so short-lived steps don't start on the edge of expiry.
- Auth-failure retry: when an agent step fails and the tail output matches the agent's auth-failure signature (for Claude: `401` / `OAuth access token` / `Failed to authenticate`), trigger a refresh and retry the step **once**. This converts the residual race (host asleep through expiry, refresh landed late) from a dead run into a self-healed step.
- `awman ready` prints credential health for each keychain-integrated agent: time-to-expiry, and a warning when the credential cannot be read or is already expired. Replace the current silent-empty-vec failure modes in `keychain.rs` (missing entry, malformed JSON) with logged warnings surfaced through ready/status.

### 6. Configuration

- New optional config (per-repo overriding global, file-edit only, consistent with the existing `auth` block): refresh threshold (default 20 min), monitor tick interval (default 60 s), and a kill switch (`refresh: off`) restoring today's env-var snapshot behavior for escape-hatch purposes.

## Edge Case Considerations:
- **Parallel containers:** all live leases are rewritten from the same host snapshot; containers never refresh themselves (no refresh token present), so there is no rotation race regardless of parallelism.
- **Token expires while a step's tool call is in flight:** validated behavior is that the next API call re-reads the file; as long as the monitor refreshed before expiry, the session continues seamlessly. If the refresh landed late, the step fails once and the workflow-level retry (§5) recovers it.
- **Host cannot refresh** (machine slept through expiry, keychain locked, network down, user logged out of Claude on the host): monitor keeps last-known-good file, warns, retries with backoff; step-level retry recovers once the host can refresh. This is the one residual scenario that can still surface a failed step — it must be loud, not silent.
- **User runs `/login` or `/logout` on the host mid-run:** monitor picks up whatever the keychain now holds on the next tick; a logout empties the source → warn and keep last-known-good until expiry.
- **Two awman processes on one host:** each runs its own monitor; both trigger the host agent, which serializes its own keychain rotation. Redundant pings are harmless (verify-`expiresAt`-advanced makes the second a no-op).
- **awman process killed hard (SIGKILL):** leases die with the process, as do the attached container clients; no writer outlives its containers. Staged TempDirs may leak (existing behavior, unchanged).
- **Lease-drop vs monitor-tick race:** generation check + skip-on-missing-path (§3) makes the loser of the race a no-op, never an error or a write to a recycled path.
- **Container Claude writes to the staged `.credentials.json`** (e.g. on its own failed refresh attempt): the next monitor rewrite clobbers it; atomic rename means no torn state either direction. The staged dir is a per-session throwaway copy, so nothing propagates to the host.
- **Non-macOS hosts:** descriptor reads the host credential from `~/.claude/.credentials.json` via `AuthPathResolver` instead of the keychain; the same access-token-only materialization applies. (Keychain read remains macOS-only per work item 0066; this generalizes the *source*, not the keychain code.)
- **Env-var dedup rule** (`ANTHROPIC_API_KEY` declared and set suppresses the keychain credential, `src/engine/container/options.rs:250`): must suppress the credential *file* the same way it suppresses the env var today — same `service_for_credential` mapping, so users who deliberately run on API-key billing are unaffected.
- **`CLAUDE_CODE_OAUTH_TOKEN` set on the host by the user:** passthrough-declared env vars keep working; the dedup/suppression rules decide whether awman's file delivery yields, mirroring the current env-var precedence behavior.

## Test Considerations:
- **Unit — auth:** snapshot parsing captures access token/expiry and provably drops refresh-token fields (assert the parsed struct cannot contain them); materialized Claude file has no `refreshToken` key, mode 0600; descriptor lookup falls back to legacy env delivery for agents without a descriptor; stub keychain provider throughout (never a developer's real credentials, matching the existing `with_secret_files_provider` pattern).
- **Unit — lease registry:** register/drop ordering; drop deregisters on panic (catch_unwind test); generation mismatch skips writes; write-to-missing-path is a skip; monitor parks when registry empties and restarts on next lease.
- **Unit — overlay:** `.credentials.json` is denylisted from the host copy; awman-authored file is present in the staged dir; atomic-rename writer never leaves a partial file (inject a failing writer).
- **Integration:** fake agent descriptor + temp staged dirs: monitor rewrites exactly the live leases and skips dropped ones; expiry threshold triggers the (stubbed) host-refresh action and verifies `expiresAt` advanced; failure path warns and retains last-known-good; the spawn choke-point assertion (credentialed container ⇒ lease held before spawn) across the PTY, piped, and ACP paths on both docker and apple backends.
- **Existing security tests extended:** `tests/engine/credential_argv_docker.rs` gains the inverse assertion for file-delivered credentials — the secret appears in no argv, no logs, no status output; the staged file path (not contents) may appear in `-v` mounts only.
- **E2E:** a scripted fake "agent" binary in a container that prints the credential file's fingerprint at intervals; drive the monitor to rotate the file (stubbed host source) and assert the running container observes the new fingerprint without restart; assert a second, already-exited container's staged file is not rewritten.
- **Workflow retry:** simulated agent exit with a 401 signature triggers exactly one refresh-and-retry; non-auth failures do not.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- Spec updates in the same change: `aspec/architecture/security.md` (§4 amendments), and note in this work item when the 0066 credential table row changes (Claude → file-delivered on container runtimes).
- The work item 0066 credential table row for Claude changes from env-delivered (`CLAUDE_CODE_OAUTH_TOKEN`) to file-delivered on container-class runtimes; its host credential source remains as specified there.
- Keep the descriptor surface minimal and concrete (one implementor); do not build speculative machinery beyond what Claude needs — "generic" here means the monitor/lease/delivery layers are agent-agnostic, not that we ship abstractions with no second user.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs**: `docs/03-agent-sessions.md` (credential flow: live-updated credential file, refresh-token never enters containers), `docs/04-security-and-isolation.md` (containment model, periodic sanctioned host ping), `docs/07-configuration.md` (new refresh settings under the `auth` block), `docs/12-runtimes.md` (sbx unchanged), and the `awman ready` docs for the new auth-health output.
- **Create new user guides only if a new user-visible feature warrants it** — expected: no new doc file; this is behavior-of-existing-features.
- **Never create work-item-specific docs**.
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`.
- **Docs are for end users**, not for developers trying to understand implementation.

See `CLAUDE.md` for more guidance on documentation standards.
