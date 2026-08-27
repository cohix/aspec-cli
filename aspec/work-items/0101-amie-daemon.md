# Work Item: Feature

Title: amie — daemon, data model, and scheduler
Issue: issuelink

## Summary:
- `amie` is an always-on agent orchestrator, run and managed by `awman`, that watches for user-defined **conditions** and automatically triggers dynamic-workflow executions in response. The name is French for "friend" — the always-present companion that lives inside `awman`. It is styled lowercase everywhere in prose, commands, and UI, matching the project's own `awman` convention; Rust type names follow normal Rust casing (`AmieConfig`, `AmiePaths`).
- A **condition** is a user-authored "if... then..." rule describing an outside-world trigger and the outcome the user wants when it fires. Examples:
  - "Whenever a new issue is opened in the awman repo, analyze it, draft a plan, and comment the plan on the issue."
  - "Whenever I comment `/amie` on an issue, research the comment and post a followup with findings and/or an updated plan."
  - "When the `ready-to-implement` label is added to an issue, implement the approved plan and open a PR."
  - "If any open PR has failing tests, check out the branch, fix the failure, and push the fix."
- On a regular interval (default 5m per condition), the daemon launches a **condition-evaluation agent** bound to a persistent directory at `~/.awman/amie/conditions/{name}/`, which decides whether the condition is met and, if so, writes a dynamic workflow file. The daemon then validates and executes that workflow unattended.
- **This work item covers the daemon and everything below it: Layers 0–2 plus the daemon's own Layer 3 HTTP frontend.** It is complete when the daemon runs, evaluates conditions, executes generated workflows, and exposes condition CRUD over a loopback HTTP socket that can be driven with `curl`. The user-facing `awman amie` CLI surface and the TUI amie tab are specified separately in [`0102-amie-frontends.md`](./0102-amie-frontends.md).
- **The daemon exclusively owns all amie data.** No other process opens the `amie_conditions`/`amie_runs` tables. The CLI and TUI reach that data only through the daemon's HTTP surface. This removes any possibility of multi-process SQLite contention on amie data and establishes the client/server seam that a future remote daemon would need.
- Because both `awman api` and the amie daemon are long-lived processes that would otherwise hold connections against the same shared SQLite file, **only one of them may run on a machine at a time**; starting either while the other is alive fails fast with a clear error. At most one long-lived process ever has the database open.
- Attaching to a running evaluation agent or workflow container is **not** a daemon feature — it is a direct operation between the frontend process and the agent runtime, specified in WI 0102. This work item provides only the Layer 1 primitive it builds on.
- A `amie` section in the global `~/.awman/config.json`, shaped like the repo-local `dynamicWorkflows` config (`aspec/work-items/0095-dynamic-workflow-config.md`), restricts which agents/models amie may use, sets a default leader, caps concurrent evaluations, and defines standing `guidance` every condition-evaluation agent and generated workflow must follow.

**Read `aspec/architecture/2026-grand-architecture.md` in full before implementing.** amie touches every layer, and several deliberate design choices below exist specifically to satisfy its tenets — in particular Tenet 1 (a lower layer never calls a higher one), Tenet 2 (no business logic in a frontend), and Tenet 3 (typed objects over raw `pub fn`s). Where this work item calls for refactoring existing code, that refactoring is not optional polish: it is what keeps the new code from duplicating an existing code path.

## User Stories

### User Story 1:
As a: user

I want to:
define a condition once (e.g. "when a new issue is opened, draft and post a plan") and have a background daemon watch for it and act automatically

So I can:
get routine triage, planning, and remediation work done without babysitting my repo or manually invoking an agent every time something happens

### User Story 2:
As a: user

I want to:
trust that amie's stored state is owned by exactly one process, and that amie and the API server can never fight over the same database

So I can:
run an always-on automation daemon without risking corrupted or contended state, and without having to reason about which process wrote what

## Implementation Details:

### Part 0 — Shared refactors (prerequisites)

These come first because each one is a code path amie would otherwise duplicate. Every item here is an extraction of something that already exists, and each leaves the existing caller behaviourally unchanged.

**0.1 — `DaemonPaths` (Layer 0, `src/data/fs/daemon_paths.rs`)**

`ApiPaths` (`src/data/fs/api_paths.rs`) is already a typed object over a single `root: PathBuf`, but the daemon-identity filenames are hardcoded inside its accessors (`pid_file()` → `awman.pid`, `log_file()` → `awman.log`, `server_meta_file()` → `server.json`, `api_key_hash_file()` → `api_key.hash`). Every other accessor keys off `root`, so a second daemon rooted elsewhere is already isolated.

Extract the four daemon-identity accessors into a `DaemonPaths` value object:

```rust
pub struct DaemonPaths { root: PathBuf, key_stem: &'static str }
impl DaemonPaths {
    pub fn new(root: impl Into<PathBuf>, key_stem: &'static str) -> Self;
    pub fn root(&self) -> &Path;
    pub fn pid_file(&self) -> PathBuf;         // <root>/awman.pid
    pub fn log_file(&self) -> PathBuf;         // <root>/awman.log
    pub fn server_meta_file(&self) -> PathBuf; // <root>/server.json
    pub fn key_hash_file(&self) -> PathBuf;    // <root>/<key_stem>.hash
    pub fn ensure_root(&self) -> Result<(), DataError>;
}
```

`ApiPaths` gains a `daemon(&self) -> DaemonPaths` accessor and delegates its four existing methods to it, so no existing call site changes. `AmiePaths` (below) exposes the same accessor. The filenames stay identical to today's for the API daemon; only the key stem varies (`api_key` vs `amie_key`).

**0.2 — `DaemonProcess` (Layer 0, `src/data/fs/daemon_process.rs`)**

`src/data/fs/api_process.rs` is a module of free functions (`write_pid_exclusive`, `read_pid`, `clear_pid`, `check_already_running`, `is_process_alive`, `pid_is_awman`, `kill_process`, `spawn_background`, `write_server_meta`, `read_server_meta`, `clear_server_meta`). They already take explicit paths, so they are functionally parameterizable — but they violate Tenet 3, and one value blocks a second daemon outright: `try_systemd_run` hardcodes `--unit=awman-api`, and `try_launchd` builds a fixed `io.awman.api` plist label. Two daemons would collide on both.

Wrap them in a typed object owning a `DaemonPaths` and an identity:

```rust
pub struct DaemonProcess { paths: DaemonPaths, unit_name: &'static str, plist_label: &'static str }
impl DaemonProcess {
    pub fn new(paths: DaemonPaths, unit_name: &'static str, plist_label: &'static str) -> Self;
    pub fn running_pid(&self) -> Result<Option<u32>, DataError>;   // was check_already_running
    pub fn claim_pidfile(&self, pid: u32) -> Result<bool, DataError>; // was write_pid_exclusive
    pub fn release_pidfile(&self) -> Result<(), DataError>;
    pub fn spawn_detached(&self, binary: &Path, args: &[String]) -> Result<u32, DataError>;
    pub fn terminate(&self) -> Result<(), DataError>;
    pub fn write_meta(&self, meta: &ServerMeta) -> Result<(), DataError>;
    pub fn read_meta(&self) -> Result<Option<ServerMeta>, DataError>;
    pub fn clear_meta(&self) -> Result<(), DataError>;
}
```

The process-identity helpers that are genuinely stateless and take simple inputs (`is_process_alive(pid)`, `pid_is_awman(pid)`) stay free functions — the grand architecture explicitly permits that category. `spawn_detached` threads `unit_name`/`plist_label` through to `try_systemd_run`/`try_launchd` instead of the current literals. `src/command/commands/api_server.rs`'s `run_start`/`run_kill`/`run_logs`/`run_status` are updated to call methods on a `DaemonProcess` rather than the free functions; the free functions become `pub(crate)` implementation details of the new module.

**0.3 — `DaemonGuard` (Layer 0, `src/data/fs/daemon_guard.rs`)**

New, and the mechanism that eliminates cross-daemon SQLite contention by construction:

```rust
pub enum DaemonKind { Api, amie }
pub struct DaemonGuard { this: DaemonKind, api: DaemonProcess, amie: DaemonProcess }
impl DaemonGuard {
    pub fn for_daemon(this: DaemonKind, env: &EnvSnapshot) -> Result<Self, DataError>;
    /// Errors if the *other* daemon is alive, naming it and its PID.
    pub fn check(&self) -> Result<(), DataError>;
    /// check() → claim this daemon's pidfile → check() again; releases the pidfile
    /// and errors if the second check fails.
    pub fn acquire(&self, pid: u32) -> Result<(), DataError>;
}
```

The double check in `acquire` closes the window where both daemons pass an initial check before either has committed. `awman api start` and the amie daemon both call `acquire` before opening the database.

**0.4 — `serve_router` and daemon bootstrap (Layer 3, `src/frontend/api/serve.rs`)**

`frontend::api::serve(ApiServeConfig)` (`src/frontend/api/mod.rs`) is a ~360-line monolith that inlines path resolution, store opening, auth-mode resolution, engine construction, session restore, worker spawn, router construction, TLS setup, the signal-handling/graceful-shutdown task, EADDRINUSE mapping, and the task drain. Only `build_router(state)` is separable today. Extract the parts a second daemon needs verbatim:

```rust
pub struct ServeOptions { pub addr: SocketAddr, pub tls: Option<TlsMaterial>, pub shutdown_grace: Duration }
/// Binds, serves, handles SIGINT/SIGTERM → graceful shutdown, maps EADDRINUSE.
pub async fn serve_router(router: Router, options: ServeOptions) -> Result<(), CommandError>;

/// Resolves AuthMode from a DaemonPaths key-hash file + a skip-auth flag.
pub fn resolve_auth_mode(paths: &DaemonPaths, skip: bool) -> Result<AuthMode, CommandError>;
```

`frontend::api::serve` is rewritten to call both. Nothing about its behaviour changes; it simply stops being the only place this logic exists. The auth middleware (`auth_middleware`, SHA-256 + `subtle::ConstantTimeEq` against `AuthMode::Enabled { key_hash }`) and the `ErrorResponse { error: String }`/`error_json(...)` shape are already reusable as-is and are used unchanged by the amie server.

**0.5 — `HttpCore` (Layer 2, `src/command/commands/http_core.rs`)**

`RemoteClient` (`src/command/commands/remote_client.rs`) is `{ base_url, http }` and splits cleanly into generic transport and route-specific methods. Extract the generic half:

```rust
pub struct HttpCore { base_url: String, prefix: &'static str, http: reqwest::Client }
impl HttpCore {
    pub fn new(base_url: &str, prefix: &'static str, key: Option<&ApiKey>) -> Result<Self, CommandError>;
    pub fn new_with_pinned_cert(base_url: &str, prefix: &'static str, key: Option<&ApiKey>, pinned_cert_pem: Option<&str>) -> Result<Self, CommandError>;
    pub async fn get(&self, path: &[&str]) -> Result<HttpResponse, CommandError>;
    pub async fn delete(&self, path: &[&str]) -> Result<HttpResponse, CommandError>;
    pub async fn post_command(&self, path: &[&str], flags: &[(&str, Value)], headers: &[(&str, &str)]) -> Result<HttpResponse, CommandError>;
    pub fn map_reqwest_error(e: reqwest::Error) -> CommandError;
    pub fn is_loopback_addr(addr: &str) -> bool;
}
pub struct HttpResponse { pub status: u16, pub body: serde_json::Value }
```

This lifts `CONNECT_TIMEOUT`/`READ_TIMEOUT`, the bearer-token default header, the pinned-cert root, trailing-slash trimming, `canonicalize_url`, the uniform `>= 400 → CommandError::RemoteHttpStatus` mapping, and the timeout/connect/transport error classification. The `/v1/` prefix, currently hardcoded in four places in `RemoteClient`, becomes the `prefix` field. `RemoteClient` becomes a thin typed façade holding an `HttpCore` and keeps its route-specific methods (`start_session`, `kill_session`, `exec_job`, `stream_job_logs`, `resolve_api_key`, and its SSE parsing) unchanged. amie's client (Part 3) is a second façade over the same core — **no second HTTP client implementation.**

### Part 1 — Layer 0: data

**1.1 — Shared database relocation (`~/.awman/api/` → `~/.awman/data/`)**

The database is no longer API-specific once amie owns tables in it, so it moves out of the API-mode directory.

- Add `DataPaths` (`src/data/fs/data_paths.rs`) resolving `<data_home>/data/awman.db`, using the precedence `GlobalConfig::data_home_with` already implements (`AWMAN_CONFIG_HOME` → `XDG_DATA_HOME/awman` → `$HOME/.awman`). No new env var — the DB moves in lockstep with every other relocatable path.
- `ApiPaths::db_path()` delegates to `DataPaths::db_path()`. `ApiPaths` keeps owning everything else under `~/.awman/api/` (pidfile, log, `sessions/`, key hash, `server.json`).
- Add `DataPaths::migrate_legacy_db(&self) -> Result<MigrationOutcome, DataError>`, called by both daemons at startup **before** any connection is opened:
  - New-location file exists → `MigrationOutcome::AlreadyMigrated`, no action.
  - Only the legacy `<data_home>/api/awman.db` exists → create `<data_home>/data/`, copy the `.db` **plus its `-wal` and `-shm` sidecars** (the store runs `PRAGMA journal_mode=WAL`, and an uncheckpointed WAL holds committed data the main file does not) to the new location, verify, then **rename the legacy files in place to `awman.db.pre-migration`** (and matching sidecar names) rather than deleting them. Guard with the same `O_CREAT|O_EXCL` idiom `claim_pidfile` uses so two starting processes cannot both migrate.
  - The retained `*.pre-migration` files are a one-release safety net for an irreversible, one-time data move. They are never read by awman — the new location is authoritative the moment migration completes. `awman clean` gains a rule that removes them (see Codebase Integration), and they are dropped entirely in a later release once the migration has been in the field for a cycle.
  - Neither exists → `MigrationOutcome::FreshInstall`; schema creation makes the file at the new path.
  - Always copy-then-verify-then-rename-aside rather than `rename()`-ing the file to its new home. Retaining a backup makes the atomic-rename fast path moot, and one code path is easier to reason about and test than a fast path plus a cross-filesystem fallback. If the copy fails at any point, leave the legacy files untouched under their original names and fail startup — never leave the user with neither a working database nor an obvious original.
  - Return the outcome rather than logging directly — the caller decides how to surface it, keeping Layer 0 free of presentation concerns.

**1.2 — Schema**

Two tables added to the existing database via the idempotent `CREATE TABLE IF NOT EXISTS` / `ADD COLUMN IF MISSING` pattern already in `SqliteSessionStore::migrate`. No second database file.

```sql
CREATE TABLE IF NOT EXISTS amie_conditions (
    id            TEXT PRIMARY KEY,               -- uuid
    name          TEXT NOT NULL UNIQUE,           -- user-chosen slug, e.g. "issue-triage"
    description   TEXT NOT NULL,                  -- the natural-language "if...then..." rule
    repo_scope    TEXT NOT NULL,                  -- repo root this condition watches
    mount_scope   TEXT NOT NULL,                  -- 'cwd' | 'gitroot', captured once at creation
    interval_secs INTEGER NOT NULL DEFAULT 300,
    status        TEXT NOT NULL DEFAULT 'active', -- active | paused
    agent         TEXT,                           -- optional agent override
    model         TEXT,                           -- optional model override
    backoff_until TEXT,                           -- set by the failure-backoff rule
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    last_run_at   TEXT
);

CREATE TABLE IF NOT EXISTS amie_runs (
    id            TEXT PRIMARY KEY,               -- uuid
    condition_id  TEXT NOT NULL REFERENCES amie_conditions(id),
    status        TEXT NOT NULL DEFAULT 'running',-- running | not_triggered | workflow_executed | failed | interrupted
    workflow_path TEXT,                           -- generated workflow file, if the condition triggered
    workflow_state_path TEXT,                     -- WorkflowStateStore file for this run, for GET /v1/conditions/{name}/workflow
    session_id    TEXT,                           -- Session backing the evaluation / workflow execution
    started_at    TEXT NOT NULL,
    finished_at   TEXT,
    error         TEXT
);

CREATE INDEX IF NOT EXISTS idx_amie_runs_condition ON amie_runs(condition_id, started_at DESC);
```

**1.3 — `ConditionStore` (`src/data/fs/condition_store.rs`)**

Mirrors `SqliteSessionStore`'s shape. Constructed **only inside the amie daemon process**:

```rust
impl ConditionStore {
    pub fn open(db_path: &Path) -> Result<Self, DataError>;
    pub fn migrate(&self) -> Result<(), DataError>;
    pub fn create(&self, c: &Condition) -> Result<(), DataError>;
    pub fn get(&self, name: &str) -> Result<Option<Condition>, DataError>;
    pub fn list(&self) -> Result<Vec<Condition>, DataError>;
    pub fn set_status(&self, name: &str, status: ConditionStatus) -> Result<(), DataError>;
    pub fn delete(&self, name: &str) -> Result<(), DataError>;
    pub fn due_for_evaluation(&self, now: DateTime<Utc>) -> Result<Vec<Condition>, DataError>;
    pub fn begin_run(&self, condition_id: &str, session_id: &str) -> Result<RunId, DataError>;
    pub fn finish_run(&self, run: RunId, status: RunStatus, detail: RunDetail) -> Result<(), DataError>;
    pub fn runs_for(&self, name: &str, limit: usize) -> Result<Vec<Run>, DataError>;
    pub fn reconcile_orphaned_runs(&self) -> Result<usize, DataError>;
}
```

`due_for_evaluation` encodes the selection rules in SQL, not in the caller: `status = 'active'`, `backoff_until` null or past, `last_run_at` null or older than `interval_secs`, and no existing `amie_runs` row for that condition with `status = 'running'`. Keeping this in one query is what prevents the scheduler from re-deriving the same predicate in Rust.

**1.4 — `AmiePaths` (`src/data/fs/amie_paths.rs`)**

Same shape and env precedence as `ApiPaths::from_env`, rooted at `$HOME/.awman/amie` with an `AWMAN_AMIE_ROOT` override. Exposes `daemon() -> DaemonPaths` (key stem `amie_key`) and `condition_dir(name) -> PathBuf` → `<root>/conditions/{name}/`, validated to stay under the root exactly as `validate_context_path` does. The condition directory's semantics match `context(global)` (`src/data/fs/context_dirs.rs`) — one long-lived directory per condition, never recreated per run — as opposed to `context(workflow)`'s per-invocation UUID directories.

**1.5 — `AmieConfig` (`src/data/config/repo.rs`, hung off `GlobalConfig`)**

Follows the `ApiConfig`/`RemoteConfig` precedent exactly: an all-`Option`, `Default`-deriving struct defined in `repo.rs`, re-exported through `config/mod.rs`, and attached to `GlobalConfig` (not `RepoConfig` — amie spans every repo a condition watches).

```rust
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AmieConfig {
    pub agents_to_models: Option<HashMap<String, Vec<String>>>,
    pub max_concurrent_evaluations: Option<usize>,
    pub default_leader: Option<String>,   // agent::model
    pub guidance: Option<Vec<String>>,
}
```

Validation in `GlobalConfig::load_with`, alongside the existing `maxConcurrentAgents >= 1` check: `maxConcurrentEvaluations >= 1`; `defaultLeader` matches `agent::model` with two non-empty, non-whitespace components; `agentsToModels` keys and the `defaultLeader` agent component satisfy the `AgentName` lexical rules; no empty model list, model name, or `guidance` entry. WI 0095 introduces the same `agent::model` and `AgentName` validators for `DynamicWorkflowsConfig` in `repo.rs` — factor those into one shared Layer 0 helper both config modules call rather than writing them a second time.

### Part 2 — Layer 1: engine

**2.1 — Attach as a first-class runtime operation (`src/engine/agent_runtime/`)**

This is the largest simplification available and it removes an entire I/O code path that would otherwise be written twice.

`AgentRuntimeEngine::exec_args(agent_id, working_dir, entrypoint, env_vars) -> Vec<String>` already exists on the cross-paradigm trait, implemented by both `ContainerRuntime` (`docker exec -it -w … <id> …`) and `SandboxRuntime` (`sbx exec -it …`), documented as "used by TUI re-attach" — and **called by nothing**. It is finished plumbing with no consumer.

Add one method to the trait:

```rust
fn attach(&self, handle: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError>;
```

Returning `Box<dyn AgentInstance>` — not a bespoke handle type — is the whole point: the returned instance flows into the **existing** `run_with_frontend(Box<dyn AgentFrontend>) -> AgentExecution` path. Consequences:

- The CLI's attach uses `CliFrontend` (raw-mode stdio, SIGWINCH resize) and the TUI's uses `TuiContainerProxy::with_io` (channels into `ContainerSlotIo`) — **both already exist and neither needs modification.** Attach gets correct behaviour in both frontends for free.
- `AgentExecution`'s `wait`/`cancel`/`detach`/stuck-detection surface applies to attach sessions unchanged.
- Layer 2 programs against `Arc<dyn AgentRuntimeEngine>` and never pattern-matches a concrete runtime, satisfying the tenet that paradigm-specific decisions branch on `capabilities()`.

Implementation for `ContainerRuntime` mirrors `spawn_pty_bridged_docker` (`src/engine/container/docker.rs`) but substitutes argv from `exec_args(...)` and reuses `bridge_pty(io, pair, config)` unchanged — that function is already spawn-agnostic, taking only an `AgentIo` and an open `PtyPair` and never seeing the child, so it works on a container this process did not start. Add an `AttachExecution` implementing the existing `pub(crate) trait ExecutionBackend` next to `DockerExecution`. Its `BridgeConfig` sets `cancel_on_grace_expired: None` — the grace-expiry cancel currently issues `docker stop <name>`, which would be wrong for a container owned by another process. `SandboxRuntime::attach` follows the same shape over `sbx exec`.

**2.2 — Container labels (`src/engine/container/options.rs`, `docker.rs`)**

Today there is one hardcoded `--label awman=true` plus an optional `--label awman.session=<id>` driven by `ContainerOption::SessionLabel(String)` → `ResolvedContainerOptions.session_label`. The amie daemon needs a second label, and a third consumer would need a third — so generalize once rather than adding a sibling field:

- Replace `ContainerOption::SessionLabel(String)` with `ContainerOption::Label { key: String, value: String }`, accumulating into `pub labels: Vec<(String, String)>` on `ResolvedContainerOptions`.
- `build_run_argv` emits the `awman=true` label followed by one `--label k=v` per entry. This function is shared by both container backends (`apple.rs` imports it from `docker.rs`), so one edit covers Docker and Apple Containers.
- The single existing producer (`src/engine/agent/mod.rs`, emitting the session label) becomes `ContainerOption::Label { key: "awman.session".into(), value: session.id().to_string() }`. Update the `build_run_argv` tests accordingly.

**2.3 — Discovery by container name, not by label**

Labels look like the obvious discovery key, but they do not work across backends. Docker filters on them (`docker ps --filter label=…`); **Apple cannot read them back at all** — `AppleBackend::list_running` runs `container list --format json` with no filter flags and then filters client-side on `name.starts_with("awman-")`, never inspecting labels. `CONTAINER_CAPABILITIES` advertises `session_label_supported: true` for both backends because they share one static and `capabilities()` never consults the backend, so that flag is simply untrue for Apple in practice. The sandbox tier has no label concept whatsoever — `ResolvedSandboxOptions` has no label or session field, and `sbx create` takes no label flag.

The one identity channel every tier already supports is the **container name prefix**: Docker filters `--filter name=`, Apple filters client-side by prefix, and the sandbox backend lists by `awman-` prefix. Make that the discovery key.

- Extend `src/engine/container/naming.rs` so amie's containers are named `awman-amie-<condition-slug>-<unique>`, where `<unique>` is a fixed-width 8-character hex token. Fixed width matters: it makes parsing the slug back out unambiguous (strip the `awman-amie-` prefix, strip a trailing `-[0-9a-f]{8}`) even though condition slugs may contain hyphens. Constrain condition slugs at creation to `[a-z0-9]([a-z0-9-]*[a-z0-9])?` so the name stays a legal container name on every backend.
- Add to the cross-paradigm `AgentRuntimeEngine` trait — not the container-only backend trait, since all three tiers can honour it:

```rust
fn list_running_with_name_prefix(&self, prefix: &str) -> Result<Vec<AgentHandle>, EngineError>;
```

  `ContainerRuntime` implements it as `docker ps --filter name=<prefix>` for Docker and as an added prefix predicate in `parse_apple_list_output` for Apple. `SandboxRuntime` implements it by filtering the existing `sbx ls` name listing. Every tier gets a correct implementation, so WI 0102's attach needs no per-tier branching.
- Keep emitting the `awman.amie.condition=<name>` label as well. It costs nothing, is genuinely useful for Docker users inspecting `docker ps` by hand, and gives a future Apple label-reading implementation something to find — but **nothing in awman may depend on reading it back**, since Apple cannot.

**`awman status` marks amie containers, by name.** amie containers carry `awman=true` and so already appear in `awman status`. They must stay visible — hiding them would make `docker ps` disagree with `awman status`, and unattended background agents are exactly what a user needs visibility into — but must be distinguishable from the user's own sessions. In `write_status_dashboard` (`src/command/commands/status.rs`, Layer 2), derive the marker from `AgentHandle.name`: a name matching `awman-amie-<slug>-<8 hex>` renders as `amie:<slug>`, everything else renders as it does today. Deriving it from the name rather than a label is what makes this work identically on Docker and Apple. `AgentHandle` needs no new field.

**2.4 — `AmieScheduler` (`src/engine/amie/scheduler.rs`)**

A typed object owning the evaluation loop:

```rust
pub struct AmieScheduler { store: Arc<ConditionStore>, engines: Engines, paths: AmiePaths, max_concurrent: usize }
impl AmieScheduler {
    pub fn new(...) -> Self;
    pub async fn run(self, shutdown: CancellationToken);
}
```

Each tick (30s, independent of any condition's own interval) it calls `store.due_for_evaluation(now)` and dispatches each due condition onto a bounded task set sized by `max_concurrent`. Config is re-read at the top of each tick rather than cached at startup, so `guidance`/`agentsToModels`/`defaultLeader` edits take effect on the next tick without a restart.

Note on reuse: `QueueWorker` (`src/frontend/api/queue_worker.rs`) is not reusable here. Its public surface is `new` + `run`, it is hardcoded to `CommandRecord` rows and `ApiPaths`/`EventBus`/`ApiDispatchFrontend`, and its work model is "claim the next queued row" whereas amie's is "select rows whose interval has elapsed". Genericizing it would require inventing `WorkQueue`/`WorkExecutor` traits and a `WorkerPool<Q, E>` with no second real consumer today. **Do not genericize it for this work item** — the honest shared surface is small (a poll loop with backoff), and duplicating ~20 lines of loop is cheaper than a speculative abstraction. This is called out explicitly so a reviewer does not mistake it for an oversight.

**2.5 — Condition evaluation and workflow execution (`src/engine/amie/evaluator.rs`)**

Per due condition:
1. Build a `Session` rooted at `AmiePaths::condition_dir(name)`, via `SessionManager` exactly as the API and TUI frontends do for concurrent sessions.
2. Seed the condition directory with the dynamic-workflow assets (`example-workflow.toml`, `workflow-usage.md` from `src/data/dynamic_workflow_assets.rs`) plus a new `src/assets/dynamic/amie-leader-prompt.md`, which instructs the agent to state whether the condition is met and, if so, write `workflow.toml` into its context directory.
3. Resolve options via `AgentEngine::resolve_agent_options` and launch through `Arc<dyn AgentRuntimeEngine>`, mounting only the condition directory plus the repo at the `mount_scope` captured at creation — never a parent directory. Attach two labels: `awman.session=<session id>` and `awman.amie.condition=<name>`.
4. If the agent reports the condition met, validate the generated workflow and run the **existing** repair loop from WI 0092 unchanged — same schema (`src/data/workflow_definition.rs`), same 3-attempt cap, same error-injection prompt. Do not fork that logic; parameterize it with the amie leader prompt asset.
5. Execute the validated workflow through the same path as `awman exec workflow`, non-interactively, worktree isolation forced on, propagating both labels to every container the workflow launches.
6. Record the outcome via `finish_run`.

The prompt for both the evaluation agent and the generated workflow's leader is built from the effective agents/models map using `format_agents_with_models` (added by WI 0095 in `exec_workflow.rs`) with `amie.guidance` rendered as a bulleted block — the same guidance binds the evaluation and the workflow it produces.

Precedence for agent/model, highest first: (1) the condition's own `agent`/`model` columns; (2) `amie.defaultLeader`/`agentsToModels`; (3) the global `default_agent` fallback. `guidance` is always additive and never overridden by a condition.

### Part 3 — Layer 2: command

**3.1 — Catalogue entries**

Add the `amie` command tree to `CommandCatalogue` (`src/command/dispatch/catalogue.rs`) as `CommandSpec`s with `api_allowed: true`. This single addition is what produces the CLI's clap commands, the TUI's hint strings, and the daemon's request validation — no frontend holds its own list. WI 0102 consumes these projections; they are defined here because the daemon's HTTP surface validates against them.

Subcommands: `start`, `stop` (alias `kill`), `status`, `logs`, `add`, `list`, `show <name>`, `remove <name>`, `pause <name>`, `resume <name>`, `attach <name>`.

The authoritative flag-by-flag surface is the table in WI 0102, which is where the user-facing behaviour of each flag is specified; this work item implements the `CommandSpec`s that table describes. Deliberately kept in one place rather than restated here — the catalogue is the single source of truth at runtime, and `parity_test.rs` plus `aspec/uxui/cli.md` are what keep it honest. `attach` is listed in the catalogue for surface completeness but is dispatched entirely frontend-side in WI 0102 and never reaches the daemon; mark it `api_allowed: false` so the daemon's endpoint rejects it rather than appearing to accept an operation it cannot perform.

**3.2 — `ConditionGateway` (`src/command/commands/amie/gateway.rs`)**

The one design question this split raises: `amie list` runs in two very different processes — inside the daemon (where it reads SQLite) and inside the CLI/TUI (where it must ask the daemon). Writing two command implementations would guarantee drift, which is exactly what the grand architecture exists to prevent.

Resolve it with one command family over a gateway trait:

```rust
#[async_trait]
pub trait ConditionGateway: Send + Sync {
    async fn create(&self, req: CreateCondition) -> Result<Condition, CommandError>;
    async fn list(&self) -> Result<Vec<Condition>, CommandError>;
    async fn get(&self, name: &str) -> Result<Condition, CommandError>;
    async fn runs(&self, name: &str, limit: usize) -> Result<Vec<Run>, CommandError>;
    async fn set_status(&self, name: &str, status: ConditionStatus) -> Result<(), CommandError>;
    async fn delete(&self, name: &str) -> Result<(), CommandError>;
    async fn status(&self) -> Result<DaemonStatus, CommandError>;
}
```

Two implementations, both Layer 2:
- `LocalConditionGateway` — wraps `Arc<ConditionStore>`; constructed only by the daemon. Owns all validation (duplicate names, interval bounds, agents-vs-Dockerfile checks against the target repo).
- `RemoteConditionGateway` — wraps an `HttpCore` (Part 0.5) pointed at the daemon; constructed by the CLI and TUI. Pure transport: it forwards and deserializes, and performs no validation of its own, so business rules exist in exactly one place.

The `AmieCommand` family in `src/command/commands/amie/` is constructed with a `Box<dyn ConditionGateway>` and implements the `Command` trait like every other command, exposing `run_with_frontend(...)`. `Dispatch` selects the gateway from its construction context. The result: one implementation of every amie business rule, reachable identically from the CLI, the TUI, and the daemon's HTTP surface.

**3.3 — `AmieDaemonCommand` (`src/command/commands/amie/daemon.rs`)**

Owns `start`/`stop`/`status`/`logs` against a `DaemonProcess` and a `DaemonGuard`, mirroring `ApiServerCommand`. Also provides the shared ensure-running path used by every CRUD subcommand and both TUI entry points:

```rust
pub struct AmieSupervisor { process: DaemonProcess, guard: DaemonGuard, paths: AmiePaths }
impl AmieSupervisor {
    /// Returns a gateway, starting the daemon first if it is not already running.
    /// Fails immediately (without attempting a start) if the API server is running.
    pub async fn ensure_running(&self) -> Result<RemoteConditionGateway, CommandError>;
}
```

A typed object rather than a free function, per Tenet 3, and one implementation rather than N per-subcommand copies.

### Part 4 — Layer 3: the daemon's HTTP frontend

`src/frontend/amie/` — a new sibling of `src/frontend/api/`, never merged into it.

The API server's established pattern is a **single generic command endpoint**: `POST /v1/commands` with `{"subcommand": "...", "args": [...]}`, validated by `catalogue.validate_for_frontend(FrontendKind::Api, &path_parts)` and `catalogue.parse_raw_args_with_profile(...)`, then executed via `Dispatch::new(frontend, session, engines).run_command(&path_parts)`. The grand architecture requires exactly this ("Server endpoint handler should be nearly identical to their CLI and TUI counterparts, using `Dispatch` to parse inputs and then execute the resolved `Command`").

The amie server therefore serves the **same generic endpoint over the same machinery**, not a hand-written REST resource set:

- `POST /v1/commands` — catalogue-validated, restricted to the `amie` subtree, dispatched with a `LocalConditionGateway`. Responses are synchronous (amie CRUD is fast and has no queue), which is the one deliberate divergence from the API server's 202-and-poll model.
- `GET /v1/status` — daemon liveness, condition count, last tick.
- `GET /v1/conditions/{name}/workflow` — the live `WorkflowState` of the workflow that condition is currently executing, or 404 when it is not running one. This is what lets the TUI reproduce the full workflow UX for an amie run (WI 0102, Part 3). Mirror the existing `handle_get_workflow` handler in `src/frontend/api/routes.rs` exactly: return the serialized `WorkflowState` **verbatim as JSON, with no projection or DTO**, so the client can `serde_json::from_value::<WorkflowState>(..)` directly. A projection here would fork the schema and guarantee drift.

No `/logs` route and no SSE anywhere: `amie logs` reads the log file locally, and live container output is attach's job, not the daemon's. The workflow-state route is deliberately poll-based request/response, consistent with that.

**Workflow state must actually be persisted and locatable.** `WorkflowEngine` already persists `WorkflowState` on every step and phase transition via `WorkflowStateStore::save` (~20 call sites in `src/engine/workflow/mod.rs`), writing to `<git_root>/.awman/workflows/<hash8>-<name>.json`. amie's generated workflows go through that same engine, so persistence is inherited rather than built — but the daemon must be able to find the file for a given condition. Record the resolved state-file path on the `amie_runs` row when the workflow starts (add a `workflow_state_path TEXT` column alongside `workflow_path`), and have the route read that path. Do not re-derive the filename in the route from `workflow_name` and a git-root hash; that duplicates `WorkflowStateStore::filename_for`'s private naming rule and will silently break the first time it changes.

Two pre-existing gaps are worth knowing about here, because amie must not inherit them:

- `ApiPaths::command_workflow_state_path` (`~/.awman/api/sessions/<sid>/commands/<cid>/workflow.state.json`) is **never written by anything in production** — only by a test that hand-writes it. Consequently the API server's `GET /v1/workflows/{command_id}` returns 404 for every real run. amie sidesteps this by reading the engine's own `WorkflowStateStore` file rather than inventing a second persistence location. Fixing the API-mode gap is out of scope here, but the route amie adds is the working reference implementation for whoever does.
- `RemoteWorkflowPoller` (`src/frontend/tui/per_command/remote.rs`) is written, correct, and **entirely dead** — never constructed, in a private module, not re-exported, its warning suppressed by the crate-level `allow(dead_code)`. WI 0102 revives it rather than writing a new poller.

Server construction reuses Part 0.4 (`serve_router`, `resolve_auth_mode`), the existing `auth_middleware`, and the existing `ErrorResponse`/`error_json` shape. It binds strictly to `127.0.0.1` on an OS-assigned ephemeral port by default (`--port` pins one), then writes `ServerMeta` via `DaemonProcess::write_meta` so short-lived CLI processes can discover it. Auth uses the same bearer-token scheme against `amie_key.hash`.

Daemon startup order is fixed and load-bearing: `DaemonGuard::acquire` → `DataPaths::migrate_legacy_db` → open store → `ConditionStore::migrate` → `reconcile_orphaned_runs` → spawn `AmieScheduler` → `serve_router`.

Note that `CommandCatalogue::rest_route_table()` and `openapi_schema()` (`src/command/dispatch/projections/api_schema.rs`) exist but are dead code — no production caller, and they emit POST-only leaf routes that do not match the real API server's resource model. **Do not build the amie server on them.** They are noted here only so the next reader does not mistake them for the intended mechanism; whether to delete or repair them is out of scope for this work item.

### Part 5 — Runtime support: Docker, Apple Containers, and sandboxes

amie must respect the global `runtime:` setting rather than assuming Docker. The three tiers are not equivalent, and the differences were verified against the code rather than inferred from the capability table — which turns out to overstate what Apple and sandbox actually do.

**Docker and Apple Containers: full parity, with two corrections.**

Both are backends of the same `ContainerRuntime`, share `build_run_argv`, and share one `CONTAINER_CAPABILITIES` static that `capabilities()` returns without consulting the backend. Everything amie needs — host mounts for the condition directory and repo, arbitrary env vars for credential injection, `docker exec`/`container exec` for attach, worktree `.git` overlays, `start_background` for workflow setup/teardown — works on both. Two places where the shared static is optimistic about Apple and amie must not rely on it:

1. **Label read-back** (see 2.3): Apple can write labels but never reads them. amie's discovery and `awman status` marker are therefore name-based, which works on both.
2. **`per_resource_stats`**: `AppleBackend::stats` is implemented (`container stats --no-stream --format json`), so this one is honest — but `AppleBackend` does *not* override `list_stopped` or `list_dangling_images`, so the trait defaults return empty. Any amie cleanup that reasons about stopped containers must not assume Apple will report them.

**Sandbox (`docker-sbx-experimental`): amie must refuse to run.**

This is not a matter of degraded features. Every one of amie's core mechanisms is independently broken under the sandbox tier:

| amie requirement | sandbox reality |
|---|---|
| Persistent per-condition context directory mounted into the agent | `arbitrary_host_mounts: false`. Only the single workspace positional is mounted. `context(...)` overlays are **warned and dropped** (`src/engine/agent/mod.rs`), yet the injected system prompt still instructs the agent to use `/awman/context/...` paths that do not exist. |
| Evaluation agent writes `workflow.toml` for the daemon to read back | The entire leader handshake is a host-directory exchange (`exec_workflow.rs` resolves a host `context_dir` and reads `workflow.toml` back). Under sbx the agent cannot write to it, so the run burns all 3 repair attempts and fails. |
| Generated workflows run `commit_changes` / `create_pull_request` — how unattended work is persisted | Setup/teardown steps need an `AgentExec`, and `BackgroundContainer` is the **only** impl in the codebase. `require_container_runtime()` returns `NotImplemented` under sbx. |
| Dynamic workflow execution at all | `ensure_agent_image` calls `require_container_runtime()` unconditionally, aborting before the leader launches. |
| Worktree isolation (forced for every dynamic run) | A linked worktree's `.git` points outside the workspace; sbx **hard-errors** on any overlay outside the workspace. In-VM git fails for every run. |
| `--yolo` so an unattended agent never blocks on a prompt | Not deliverable to mixin kits for codex/gemini/opencode/copilot — warned, not applied. An unattended agent can silently block forever on a permission prompt. |
| Per-condition container identity | Sandbox names are `awman-<fnv1a32(git_root)>-<agent>`. Two conditions on the same repo with the same agent **collide onto one sandbox**, with no ownership metadata to detect it. |
| Post-hoc debugging of an unattended failure | Sandboxes bypass the container I/O bridge and produce no failure logs. |

Two of these (the leader handshake and setup/teardown) are the load-bearing mechanisms of the feature, not peripheral conveniences. sbx is also macOS-arm64/Windows only and explicitly, durably experimental per WI 0090.

**Therefore: amie refuses to operate under a sandbox runtime, and says so clearly.** Enforce at two points, following the `require_container_runtime()` precedent:

- **Daemon startup** — after `DaemonGuard::acquire` and before opening the store, check the detected runtime. Under a sandbox tier, fail startup with: `amie requires a container runtime. The configured runtime "docker-sbx-experimental" cannot mount amie's condition directories or run workflow setup/teardown steps. Set runtime to "docker" or "apple-containers" to use amie.` Do not start a daemon that would fail every evaluation.
- **Condition creation** — the same check in `LocalConditionGateway::create`, so a user who switches runtimes after creating conditions gets a clear error at the next touch rather than silent, repeated evaluation failures.

Gate on the tier — `Engines::container_runtime.is_some()`, the established idiom — rather than on `capabilities()`, since the capability flags that describe this constraint (`arbitrary_host_mounts`, `host_paths_visible`) exist but have **no production consumers** and are unreliable for Apple. If the sandbox tier later gains an `AgentExec` impl, in-workspace context exchange, and worktree support, revisit this as a separate work item; the check is one place to change.

## Resolved Decisions

Recorded here so implementers and reviewers can see what was decided deliberately rather than by default.

1. **Legacy database is retained, not deleted.** Migration copies to the new location, verifies, then renames the originals aside as `awman.db.pre-migration` (plus sidecars). They are never read; `awman clean` removes them, and they are dropped entirely a release later. Rationale: the move is one-time and irreversible, and a backup is cheap insurance against a verification bug. This also removed the atomic-`rename()` fast path — retaining a backup makes it moot, and one copy path is easier to test than a fast path plus a cross-filesystem fallback.
2. **`amie logs` stays local-only.** It reads the daemon's log file directly rather than going through the daemon. Remote daemons are explicitly future work, and this keeps the daemon's HTTP surface to pure CRUD with no log or streaming route. The work item that adds remote support adds a route then.
3. **amie containers remain visible in `awman status`, visually marked.** They render with an `amie:<condition>` source marker rather than being filtered out. Rationale: filtering them would make `docker ps` disagree with `awman status`, and unattended background agents are exactly what a user needs visibility into.
4. **Identity and discovery are name-based, not label-based.** amie containers are named `awman-amie-<slug>-<8 hex>`, and both attach discovery and the `awman status` marker derive from that name. Rationale: Apple writes labels but never reads them back, and the sandbox tier has no label concept at all — the name prefix is the only identity channel all three tiers support. The `awman.amie.condition` label is still emitted for human inspection, but nothing in awman depends on reading it.
5. **amie refuses to run under `docker-sbx-experimental`.** Checked at daemon startup and at condition creation, with an error naming the configured runtime and the supported alternatives. Rationale: the sandbox tier cannot mount amie's condition directories, cannot support the leader's `workflow.toml` handshake, and has no `AgentExec` impl for workflow setup/teardown — the feature's three load-bearing mechanisms. Degrading silently would produce a daemon that fails every evaluation.

## Edge Case Considerations:
- **`awman api` running when the amie daemon starts** (directly or via `ensure_running`): fail immediately, naming the API server and its PID, instructing the user to run `awman api kill`. Never queue or wait.
- **The amie daemon is running when `awman api start` is invoked**: symmetric failure via the same guard, before the port is bound or the database opened.
- **Both daemons starting at nearly the same instant**: the double check in `DaemonGuard::acquire` means at most one wins; the loser releases its just-claimed pidfile rather than leaving a stale one.
- **Ambiguous or unmet conditions**: the evaluation agent defaults to "not triggered" when uncertain. No workflow is generated on a low-confidence read.
- **Workflow generation exhausts repair attempts**: record `failed` with the validation error, execute nothing, and wait for the next tick rather than retrying immediately.
- **Previous run still in flight**: `due_for_evaluation` excludes it in SQL, so no second concurrent evaluation is ever launched for one condition.
- **Repeatedly failing conditions** (persistent auth or rate-limit errors): set `backoff_until` with exponential growth rather than retrying every tick, and surface it in `status`.
- **Concurrent triggers touching the same repo**: generated workflows force worktree isolation exactly as `exec workflow --yolo` does.
- **No human present for approval prompts**: `mount_scope` is captured once at creation and never widened during a scheduled run. Generated workflows run under `--yolo`-equivalent guardrails; there is no unattended approval UI.
- **Credential handling**: credentials are injected as container env vars at startup only, never written into the persistent condition directory where they would survive across runs. `amie_key.hash` is generated and permissioned exactly as `api_key.hash` is.
- **Daemon crash or host reboot**: conditions live in SQLite, not daemon memory. `reconcile_orphaned_runs` at startup moves any row still marked `running` to `interrupted` — an orphaned status cannot be trusted.
- **Duplicate condition names**: the unique constraint rejects them; the gateway surfaces a clear error rather than overwriting.
- **`agentsToModels` names an agent with no Dockerfile in the target repo**: reject at creation with the missing-agents error shape `exec_workflow.rs` already uses. If the repo's Dockerfiles change later, fail that one tick's run — never the whole daemon.
- **A condition's own `agent`/`model` names an agent absent from `agentsToModels`**: the condition-level override wins. `agentsToModels` is a default pool, not an allowlist.
- **`maxConcurrentEvaluations: 0`, malformed `defaultLeader`, empty `guidance` entry**: all rejected at config load, before the daemon starts.
- **Migration interrupted partway**: if a target file exists alongside a legacy `awman.db` that has *not* been renamed aside, the previous run died between copy and verify, so the target cannot be trusted. Discard it, restart the copy from the legacy original, and log that it happened. This is safe precisely because the original is still under its own name — the rename-aside being the last step is what makes the operation resumable.
- **A `*.pre-migration` backup already exists from an earlier upgrade**: do not overwrite it and do not fail. The existing backup is older and therefore the more conservative thing to preserve; log that a second migration ran and leave it in place.
- **Copy fails partway** (disk full, permissions): leave the legacy files untouched under their original names, remove the partial target, and fail startup with an actionable error — never leave the user with neither a working database nor a recognisable original.
- **Permission denied creating `~/.awman/data/`**: fail startup with an actionable error. A silent fallback to the legacy path would let two processes disagree about which file is authoritative.
- **Uncheckpointed WAL at migration time**: the `-wal`/`-shm` sidecars move with the main file in the same operation; moving `awman.db` alone would silently lose recent commits.
- **Attaching to a container the daemon no longer tracks**: `attach` operates purely on an `AgentHandle` from a name-prefix query, so it works whenever the container is alive — including after the daemon has died.
- **The user switches `runtime:` to a sandbox while amie conditions already exist**: conditions are not deleted or rewritten. The running daemon fails its next startup and condition creation is rejected, both with the runtime-specific error, so the state is preserved and becomes usable again the moment the runtime is switched back.
- **The user switches between Docker and Apple Containers**: fully supported and needs no migration. Container names, mounts, attach, and the status marker all behave identically; only already-running containers from the previous runtime become unreachable, which is true of every awman feature.
- **A condition slug that is legal in SQLite but illegal as a container name** (uppercase, leading hyphen): rejected at condition creation, not discovered later when the first evaluation fails to launch.
- **Grace-expiry cancellation during attach**: `BridgeConfig::cancel_on_grace_expired` is `None` for attach sessions, so a quiet attached agent is never killed by the attaching process.

## Test Considerations:
- Unit — `DaemonPaths` produces today's exact filenames for the API daemon (guards against the refactor changing an existing path) and distinct ones for amie.
- Unit — `DaemonProcess` round-trips pidfile claim/read/release and `ServerMeta` write/read; `spawn_detached` threads distinct unit/plist names per daemon.
- Unit — `DaemonGuard::check` returns `Ok` for absent or stale pidfiles and a descriptive error naming the process and PID for a live one, in both directions.
- Integration — mutual exclusion both ways: with one daemon running, starting the other fails without binding a port or opening the database.
- Integration — mutual-exclusion race: start both concurrently from a clean state; exactly one wins, the loser leaves no pidfile, and its error names the winner.
- Unit — `HttpCore` applies the bearer header, trims the base URL, honours the configurable prefix, and maps timeout/connect/status errors as `RemoteClient` does today. Existing `RemoteClient` tests must pass unchanged after it is refactored onto the core.
- Unit — schema migration is idempotent; re-running against a migrated database is a no-op.
- Unit — `due_for_evaluation` excludes paused conditions, conditions inside `backoff_until`, conditions whose interval has not elapsed, and conditions with a `running` row; includes everything else.
- Unit — `ConditionStore` CRUD including the unique-name violation, exercised only through the daemon-side gateway.
- Unit — `AmieConfig` serde round-trips through `GlobalConfig` with `amie` as the JSON key; each validation rule rejects its bad input, mirroring the existing `maxConcurrentAgents` test style.
- Unit — agent/model precedence: a condition-level override beats `defaultLeader`, which beats `default_agent`; `guidance` is present regardless of which level supplied the agent.
- Unit — `DataPaths::db_path` matches `GlobalConfig::data_home_with` precedence with `/data/awman.db` appended.
- Unit — `migrate_legacy_db`: no-op when neither file exists; no-op when the target exists and the legacy file is already renamed aside; copies the `.db` plus both sidecars and renames the originals aside when only the legacy file exists.
- Unit — after a successful migration the legacy `awman.db` no longer exists, `awman.db.pre-migration` holds identical bytes, and the sidecars are renamed to match.
- Integration — migration preserves data byte-for-byte: rows written to a legacy database are readable from the new location afterwards.
- Integration — interrupted-migration recovery: with a target present but the legacy file still under its original name, startup discards the target and re-copies from the original.
- Integration — a pre-existing `awman.db.pre-migration` is not overwritten by a second migration.
- Integration — a failed copy (simulated write error) leaves the legacy files under their original names, removes the partial target, and fails startup.
- Integration — `awman clean` removes `*.pre-migration` files and leaves the live database untouched.
- Integration — label generalization: `build_run_argv` emits `awman=true` plus one `--label k=v` per entry, and the session label still appears exactly as before the refactor (regression guard for the existing behaviour).
- Unit — amie container names round-trip: `awman-amie-<slug>-<8 hex>` parses back to `<slug>` for slugs containing hyphens, and a non-amie name (`awman-<pid>-<nanos>`) parses to `None`.
- Unit — condition-slug validation rejects uppercase, leading/trailing hyphens, and characters illegal in a container name, so a stored condition can always produce a legal name.
- Integration — `list_running_with_name_prefix` returns only matching containers, tested against **each** tier: Docker (`--filter name=`), Apple (client-side prefix predicate), and sandbox (`sbx ls` name filter).
- Integration — `awman status` renders an amie-launched container as `amie:<condition>` and a user session's container exactly as today, on both Docker and Apple, proving the marker is additive and backend-independent.
- Integration — amie never depends on reading a label back: with label read-back stubbed out entirely, discovery and the status marker still work.
- Integration — daemon startup under a sandbox runtime fails with the runtime-specific error, opens no store, claims no pidfile, and binds no port.
- Integration — condition creation under a sandbox runtime is rejected with the same error, covering the switch-runtime-after-creating case.
- Integration — daemon startup succeeds under both Docker and Apple with no tier-specific branching in the startup path.
- Integration — `AgentRuntimeEngine::attach` against a container started by a different process yields an `AgentInstance` whose `run_with_frontend` streams live output, and whose exit does not stop the target container.
- Integration — full condition lifecycle with a mocked evaluation agent: create, tick, agent writes `workflow.toml`, daemon validates and executes it, run recorded correctly.
- Integration — not-triggered path records `not_triggered` and generates no workflow.
- Integration — the repair loop behaves identically when driven by the amie leader prompt as by the WI 0092 leader flow.
- Integration — restart reconciliation moves an orphaned `running` row to `interrupted`.
- Integration — condition directory persists across two sequential runs (contrast with `context(workflow)`'s per-invocation directory).
- Integration — editing `amie.*` while the daemon runs takes effect on the next tick without a restart.
- Integration — the daemon's `POST /v1/commands` rejects a non-`amie` subcommand and an unknown flag with the catalogue's error shape.
- Integration — the daemon binds loopback only; a request to a non-loopback interface is refused.
- E2E — `curl` drives the full condition lifecycle against a running daemon with no `awman` frontend involved, proving the daemon is complete on its own.
- E2E — a pre-migration install (legacy DB with real session/command rows) starts the amie daemon successfully, gains the amie tables, and retains prior API-mode rows.

## Codebase Integration:
- Layer ownership: `DataPaths`/`DaemonPaths`/`DaemonProcess`/`DaemonGuard`/`ConditionStore`/`AmiePaths`/`AmieConfig` in Layer 0; `AmieScheduler`, the evaluator, and `attach` in Layer 1; the catalogue entries, `ConditionGateway` and both impls, `AmieCommand`, and `AmieSupervisor` in Layer 2; the HTTP frontend in Layer 3. Nothing in Layer 3 validates input or decides scheduling.
- Every refactor in Part 0 is behaviour-preserving for its existing caller. `frontend::api::serve` must still pass its existing tests after being rewritten onto `serve_router`, and `RemoteClient`'s public surface must not change when it moves onto `HttpCore`.
- `ConditionStore` is constructed only inside the daemon. Enforce by module visibility where possible, and by review otherwise: no CLI or TUI module may import it.
- amie's HTTP frontend is a separate router and server instance from `awman api`'s. It shares conventions (framework, auth middleware, error shape, JSON style) and the extracted bootstrap, never a mounted route tree.
- Prefer typed objects throughout, per Tenet 3. The stateless process-identity helpers (`is_process_alive`, `pid_is_awman`) are the permitted exception.
- Respect `aspec/architecture/security.md` without exception: every agent runs in a container, mount scope is captured once and never widened during an unattended run, and credentials are injected at container startup only.
- `awman clean` (`src/command/commands/clean.rs`, `docs/14-cleaning-up.md`) gains one rule: remove `awman.db.pre-migration` and its sidecars from `~/.awman/api/`. Add it alongside the existing cleanup rules rather than as a separate mechanism, and never touch the live database at `~/.awman/data/awman.db`.
- Extending `AgentHandle` with `labels` touches every runtime backend's listing path. Keep the parsing in one place rather than duplicating it per backend, and ensure a container with no labels beyond `awman=true` still deserializes to an empty-but-present map so existing consumers need no `Option` handling.
- Add `awman amie` to `aspec/uxui/cli.md`'s command table and per-command sections when WI 0102 lands the user-facing surface; this work item changes no user-visible CLI behaviour on its own.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs**: `docs/09-api-mode.md` — the database now lives at `~/.awman/data/awman.db`, migrates automatically on first start after upgrade (leaving a `awman.db.pre-migration` backup that `awman clean` removes), and the API server and amie daemon cannot run simultaneously; `docs/07-configuration.md` — the `amie` block of `~/.awman/config.json`, and the new database path wherever the old one is referenced; `docs/08-overlays.md` — the amie condition directory as a sibling of `context(global)`; `docs/14-cleaning-up.md` — that `awman clean` removes the retained pre-migration database backup.
- **`awman status` output changes for all users**, not just amie users: containers launched by the amie daemon now show an `amie:<condition>` source marker. Document this wherever `awman status` output is described, so a user who has never enabled amie understands what the column means if they see it.
- **Do not create an amie user guide in this work item** — the feature has no user-facing surface until WI 0102 lands. `docs/16-amie.md` is written there.
- **Never create work-item-specific docs** (e.g. no "WI 0101 implementation guide" in published docs).
- **Keep all technical/implementation details in this work item spec or code comments**, not in `docs/`.
- **Docs are for end users**, not for developers trying to understand implementation.

See `CLAUDE.md` for more guidance on documentation standards.
