# awman Architecture

## Overview

**Status**: The grand architecture refactor is complete as of work item 0073 (May 2026).

awman is now built from a single, unified four-layer architecture:

- **`src/`** — the production source tree organized as a four-layer architecture. The `awman` binary is built from `src/main.rs`.

For the authoritative design and rationale, see the spec files under [`aspec/`](../aspec/) — in particular the detailed specification in [`aspec/architecture/2026-grand-architecture.md`](../aspec/architecture/2026-grand-architecture.md).

---

## Grand Architecture Refactor (Completed in WI 0073)

### Purpose

awman initially grew into three execution modes (CLI, TUI, API) that share the same core functionality but implement it separately, producing subtle behavioural drift and making parity across modes impossible to guarantee. The grand architecture refactor (completed May 2026) reorganized the codebase into a strict four-layer system where every frontend is a thin presentation shell over a shared, tested core.

### Tenets

1. **No upward calls.** Lower layers never call functions or use types from higher layers. If a lower layer needs to delegate upward, it defines a trait that a higher layer implements.
2. **Frontends are dumb.** No frontend (CLI, TUI, API) may implement business logic. All logic lives in Layer 2 (`command`) or below.
3. **Typed objects over free functions.** Every significant abstraction is a struct with methods. Free `pub fn` is acceptable only for stateless helpers, constructors, and one-off utilities.

### Layers

```
Layer 4: binary    main.rs — sets up frontends, delegates everything
Layer 3: frontend  CLI, TUI, API — input/output only
Layer 2: command   Dispatch, per-command business logic
Layer 1: engine    ContainerRuntime, WorkflowEngine, GitEngine, OverlayEngine, AuthEngine
Layer 0: data      Session, config, filesystem, database, typed data
```

**Layer 0 (data)** owns every data definition, config concern, filesystem access, and database interaction. No business logic, no container calls, no git operations, no workflow execution. See [Layer 0 reference](#layer-0-data-srcdata) below.

**Layer 1 (engine)** owns core runtime primitives: container lifecycle, workflow execution, git operations, overlay construction, authentication, agent management, and the multi-phase `ready`/`init` engines. See [Layer 1 reference](#layer-1-engine-srcengine) below.

**Layer 2 (command)** owns higher-level business logic: the `Dispatch` type that routes input to typed command objects, and command-specific types (`ChatCommand`, `InitCommand`, etc.). Implemented in work item 0068.

**Layer 3 (frontend)** contains the CLI, TUI, and API server. Each is a presentation layer only: it translates user input into `Dispatch` calls and renders command output. All three frontends are fully functional. See [Layer 3 reference](#layer-3-frontend-srcfrontend) below.

**Layer 4 (binary)** is `src/main.rs` — the real entrypoint that builds clap from `CommandCatalogue`, constructs engines, opens a `Session`, and routes to the CLI or TUI frontend. See [Layer 4 reference](#layer-4-binary-srcmainrs) below.

### Implementation Timeline

| Phase | Work Items | Status | Completion Date |
|-------|-----------|--------|---|
| Layer 0 (data) | WI 0066 | ✓ Complete | Apr 2026 |
| Layer 1 (engine) | WI 0067 | ✓ Complete | Apr 2026 |
| Layer 2 (command) | WI 0068 | ✓ Complete | Apr 2026 |
| Layer 3 (frontend) | WI 0069, 0070, 0071 | ✓ Complete | Apr 2026 |
| Layer 4 (binary) | WI 0069 | ✓ Complete | Apr 2026 |
| Validation & Audit | WI 0073 | ✓ Complete | May 2026 |

**Summary**: All layers fully implemented and validated. Full parity across CLI, TUI, and API frontends. Architecture lint passes. Test suite (>100 tests) covers all layers.

---

## New Source Tree (`src/`)

```
src/
  main.rs                 Layer 4 entry point (the `awman` binary)
  lib.rs                  Re-exports the four layers
  data/                   Layer 0 — fully implemented
    mod.rs
    session.rs            Session, SessionState, SessionId, AgentName, …
    session_manager.rs    SessionManager, SessionStore, InMemorySessionStore
    error.rs              DataError
    workflow_dag.rs       WorkflowDag, validate_references, detect_cycle
    workflow_definition.rs  Workflow, WorkflowStep, format detection
    workflow_state.rs     WorkflowState, StepState, WORKFLOW_STATE_SCHEMA_VERSION
    workflow_state_store.rs WorkflowStateStore (git-root-scoped persistence)
    workflow_prompt_template.rs  Prompt-template substitution
    worktree_paths.rs     WorktreePaths, worktree_branch_name helpers
    config/
      mod.rs
      repo.rs             RepoConfig and related types
      global.rs           GlobalConfig
      env.rs              EnvSnapshot, Env, env var constants
      flags.rs            FlagConfig
      effective.rs        EffectiveConfig (merged view)
    fs/
      mod.rs
      api_db.rs      SqliteSessionStore, SessionRecord, CommandRecord
      api_paths.rs   ApiPaths
      workflow_state.rs   WorkflowStateStore (legacy alias kept for compat)
      skill_dirs.rs       SkillDirs
      workflow_dirs.rs    WorkflowDirs
      overlay_paths.rs    OverlayPathResolver
      auth_paths.rs       AuthPathResolver, AgentAuthPaths
  engine/                 Layer 1 — fully implemented
    mod.rs                Re-exports: EngineError, UserMessage*, StepStatus
    error.rs              EngineError
    message.rs            UserMessage, MessageLevel, UserMessageSink, RecordingMessageSink
    step_status.rs        StepStatus (shared by ReadyEngine, InitEngine)
    container/
      mod.rs              Re-exports: ContainerRuntime, ContainerOption*, ContainerFrontend, …
      runtime.rs          ContainerRuntime::detect / build / list_running / stats / stop
      options.rs          ContainerOption enum + surrounding types (ImageRef, Entrypoint, …)
      instance.rs         ContainerInstance trait, ContainerExecution, ContainerExitInfo
      frontend.rs         ContainerFrontend trait (defined by Layer 1, implemented by Layer 3)
      backend.rs          ContainerBackend trait (pub(super) — opaque to callers)
      docker.rs           DockerBackend (pub(super))
      apple.rs            AppleBackend (pub(super); macOS only)
      naming.rs           generate_container_name()
    workflow/
      mod.rs              WorkflowEngine struct + all public methods
      actions.rs          NextAction, AvailableActions, WorkflowOutcome, StepOutcome, …
      factory.rs          ContainerExecutionFactory trait, WorkflowRuntimeContext
      frontend.rs         WorkflowFrontend trait
      timing.rs           YOLO_COUNTDOWN_DURATION, STUCK_DIALOG_BACKOFF constants
    git/
      mod.rs              GitEngine; impl GitRootResolver for GitEngine
    overlay/
      mod.rs              OverlayEngine, OverlayRequest, DirectorySpec, CLAUDE_DENYLIST
    auth/
      mod.rs              AuthEngine (API API keys, TLS, keychain credentials)
      keychain.rs         Per-OS keychain backend (keyring crate)
    agent/
      mod.rs              AgentEngine, AgentRunOptions
      agent_matrix.rs     Per-agent entrypoint/flag translation table
      frontend.rs         AgentFrontend trait
      download.rs         Dockerfile download URL constants
    ready/
      mod.rs              ReadyEngine, ReadyEngineOptions
      phase.rs            ReadyPhase state machine, ReadyFailure
      frontend.rs         ReadyFrontend trait
      summary.rs          ReadySummary
    init/
      mod.rs              InitEngine, InitEngineOptions
      phase.rs            InitPhase state machine, InitFailure
      frontend.rs         InitFrontend trait
      summary.rs          InitSummary
  command/
    mod.rs                Re-exports: CommandCatalogue, Dispatch, CommandFrontend, CommandOutcome, CommandError
    error.rs              CommandError (wraps EngineError and DataError)
    dispatch/
      mod.rs              Dispatch<F>, Engines, CommandFrontend, CommandOutcome, BuiltCommand
      catalogue.rs        CommandCatalogue, CommandSpec, FlagSpec, ArgumentSpec, FlagKind, FlagDefault, ArgumentKind, FrontendVisibility
      parsed_input.rs     ParsedCommandBoxInput (TUI command-box tokenized result)
      projections/
        mod.rs            Re-exports
        clap.rs           CommandCatalogue::build_clap_command()
        tui_hints.rs      CommandCatalogue::tui_hint_for(), tui_completions()
        api_schema.rs CommandCatalogue::openapi_schema(), rest_route_table()
    commands/
      mod.rs              Re-exports all *Command types
      command_trait.rs    Command trait (run_with_frontend)
      agent_auth.rs       AgentAuthFrontend trait, AgentAuthDecision
      agent_setup.rs      AgentSetupFrontend trait, AgentSetupDecision
      auth.rs             AuthCommand, AuthCommandFrontend, AuthOutcome
      chat.rs             ChatCommand, ChatCommandFrontend, ChatCommandFlags, ChatOutcome
      config.rs           ConfigCommand, ConfigSubcommand, ConfigShowFlags, ConfigGetFlags, ConfigSetFlags, ConfigOutcome
      download.rs         DownloadCommand, DownloadOutcome
      exec_prompt.rs      ExecPromptCommand, ExecPromptCommandFrontend, ExecPromptCommandFlags, ExecPromptOutcome
      exec_workflow.rs    ExecWorkflowCommand, ExecWorkflowCommandFrontend, ExecWorkflowCommandFlags, ExecWorkflowOutcome, WorkflowSummary
      api.rs         ApiCommand, ApiSubcommand, ApiStartFlags, ApiKillFlags, ApiLogsFlags, ApiStatusFlags, ApiOutcome
      API/
        banner.rs         Legacy API banner format constants
      prompt_templates.rs Interview/amend prompt builders for `specs amend` and `new {spec,workflow,skill}`
      init.rs             InitCommand, InitCommandFrontend, InitCommandFlags, InitOutcome
      mount_scope.rs      MountScope, MountScopeFrontend, MountScopeDecision
      new.rs              NewCommand, NewSubcommand, NewSkillFlags, NewSpecFlags, NewWorkflowFlags, NewOutcome
      ready.rs            ReadyCommand, ReadyCommandFrontend, ReadyCommandFlags, ReadyOutcome
      remote.rs           RemoteCommand, RemoteSubcommand, RemoteRunFlags, RemoteSessionStartFlags, RemoteSessionKillFlags, RemoteOutcome
      remote_client.rs    RemoteClient, RemoteResponse, RemoteEventSink
      specs.rs            SpecsCommand, SpecsSubcommand, SpecsAmendFlags, SpecsOutcome
      status.rs           StatusCommand, StatusCommandFrontend, StatusCommandFlags, StatusCommandTuiContext, TuiTabSnapshot, StatusOutcome
      worktree_lifecycle.rs WorktreeLifecycle, WorktreeLifecycleFrontend, PreWorktreeDecision, ExistingWorktreeDecision, PostWorkflowWorktreeAction
  frontend/
    mod.rs                Declares cli, tui, API sub-modules
    cli/
      mod.rs              RuntimeContext; run() entry point; render_outcome/render_error; error_exit_code
      command_frontend.rs CliFrontend (implements CommandFrontend + all *CommandFrontend marker traits); command_path_from_matches
      output.rs           stderr_is_tty(), stdin_is_tty() — pure TTY detection helpers
      user_message.rs     CliUserMessageQueue — UserMessageSink with PTY-active queueing
      per_command/
        mod.rs
        chat.rs           ChatCommandFrontend impl
        exec_prompt.rs    ExecPromptCommandFrontend impl
        exec_workflow.rs  ExecWorkflowCommandFrontend + ContainerFrontend + WorkflowFrontend impls
        api.rs       ApiStartCommandFrontend impl (calls frontend::api::serve)
        init.rs           InitCommandFrontend + InitFrontend impls
        ready.rs          ReadyCommandFrontend + ReadyFrontend impls
        agent_auth.rs     AgentAuthFrontend impl
        agent_setup.rs    AgentSetupFrontend impl
        container_frontend_marker.rs  ContainerFrontend marker impl
        mount_scope.rs    MountScopeFrontend impl
        workflow_frontend_marker.rs   WorkflowFrontend marker impl
        worktree_lifecycle_marker.rs  WorktreeLifecycleFrontend marker impl
    tui/
      mod.rs              run() — TUI entry point; run_event_loop(); main_loop()
      app.rs              App — central TUI state; Focus, StatusBar
      tabs.rs             Tab — per-tab state; ExecutionPhase, ContainerWindowState; tab_color, compute_tab_bar_width, window_border_color, phase_label
      command_box.rs      parse_input(), format_parse_error() — command-box tokenization and error formatting
      command_frontend.rs TuiCommandFrontend — implements CommandFrontend + all *CommandFrontend traits
      container_view.rs   render_container_maximized/minimized() — vt100 overlay rendering
      dialogs/
        mod.rs            Dialog enum, DialogRequest/Response, all dialog state types and rendering helpers
      hints.rs            hint_for_input(), format_suggestion_row() — catalogue-driven hint text
      keymap.rs           Action enum, FocusContext, map_key() — complete keyboard shortcut map
      per_command/        TUI per-command *CommandFrontend trait implementations (one file per command)
        mod.rs
        agent_auth.rs     AgentAuthFrontend impl
        agent_setup.rs    AgentSetupFrontend impl
        auth.rs           AuthCommandFrontend impl
        chat.rs           ChatCommandFrontend impl
        config.rs         ConfigCommandFrontend impl
        container_frontend.rs  ContainerFrontend impl
        download.rs       DownloadCommandFrontend impl
        exec_prompt.rs    ExecPromptCommandFrontend impl
        exec_workflow.rs  ExecWorkflowCommandFrontend impl
        api.rs       ApiCommandFrontend impl
        init.rs           InitCommandFrontend impl
        mount_scope.rs    MountScopeFrontend impl
        new.rs            NewCommandFrontend impl
        ready.rs          ReadyCommandFrontend impl
        remote.rs         RemoteCommandFrontend impl
        specs.rs          SpecsCommandFrontend impl
        status.rs         StatusCommandFrontend impl
        workflow_frontend.rs   WorkflowFrontend impl
        worktree_lifecycle.rs  WorktreeLifecycleFrontend impl
      pty.rs              PtySession — portable-pty wrapper; PtyEvent; spawn_text_command()
      render.rs           render_frame() — full frame layout; tab bar, execution window, status bar, command box, dialogs
      tabs.rs             (also see above)
      text_edit.rs        TextEdit — single-line/multiline text editing with cursor and word movement
      user_message.rs     TuiUserMessageSink, SharedStatusLog, StatusLogEntry
      workflow_view.rs    render_workflow_strip() — per-step status strip
    API/
      mod.rs              ApiServeConfig; placeholder serve() — ships in 0072
  main.rs                 Layer 4 binary entrypoint
```

---

## Layer 0: Data (`src/data/`)

Layer 0 is the foundation every other layer builds on. It owns:

- The `Session` ruling type and its runtime state
- The `SessionManager` collection and persistence interface
- All configuration loading, saving, and merging
- All filesystem and database interactions
- The typed `DataError` error enum

Nothing in `src/data/` ever spawns a process, opens a network socket, calls `git`, or manages a container. Those are Layer 1 concerns.

---

### Session (`src/data/session.rs`)

`Session` is the ruling type for every awman operation. It ties together a working directory, a resolved git root, loaded configurations, and the in-flight runtime state. Every command and workflow invocation starts with a `Session`.

- The **CLI** creates one `Session` per invocation.
- The **TUI** creates one `Session` per tab.
- The **API server** creates one `Session` per API session.

#### `SessionId`

```rust
pub struct SessionId(Uuid);
```

Newtype over `uuid::Uuid`. Implements `Display` (UUID string format), `Hash`, and `Eq`. `SessionId::new()` generates a random v4 UUID; `SessionId::from_uuid(uuid)` wraps an existing one for persistence round-trips.

#### `AgentName`

```rust
pub struct AgentName(String);
```

Newtype over `String` with validation: ASCII alphanumerics, hyphens, and underscores; 1–64 characters. `AgentName::new("claude")` returns `Result<AgentName, DataError>`. `as_str()` and `Display` give the inner string.

#### `ContainerHandle`

```rust
pub struct ContainerHandle {
    pub id: String,
    pub image_tag: String,
    pub name: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
}
```

The persistable identity of a running container. Layer 0 holds only the identity; the runtime object that controls a container (start/stop/wait) is Layer 1.

#### `SessionState`

```rust
pub struct SessionState {
    pub current_command: Option<CommandInvocation>,
    pub current_workflow: Option<WorkflowInvocation>,
    pub current_container: Option<ContainerHandle>,
    pub errors: Vec<SessionLogEntry>,
    pub notes: Vec<SessionLogEntry>,
}
```

Mutable runtime state owned by a `Session`. `record_error(msg)` and `record_note(kind, msg)` append to the respective logs. `SessionLogEntry` carries a UTC timestamp, a `SessionLogKind` (Info / Warning / Error / Diagnostic), and a message string.

#### `CommandInvocation` and `WorkflowInvocation`

`CommandInvocation` is the persistable record of a single in-flight command (subcommand name, args, status, exit code, timestamps). `WorkflowInvocation` is the persistable record of a running workflow (workflow name and hash, work item, step records, paused/yolo/auto flags, current step index).

Both are serializable via serde and stored in `SessionState` for persistence by the API server.

#### `GitRootResolver` trait

```rust
pub trait GitRootResolver: Send + Sync {
    fn resolve(&self, working_dir: &Path) -> Result<PathBuf, DataError>;
}
```

Layer 0 never calls `git rev-parse` directly. Instead, `Session::open` accepts a `&dyn GitRootResolver` and delegates resolution to Layer 1's `GitEngine` (wired in work item 0067). `StaticGitRootResolver` is the test-only implementation that returns a fixed path.

#### `Session` constructors and accessors

```rust
impl Session {
    pub fn open(
        working_dir: PathBuf,
        resolver: &dyn GitRootResolver,
        opts: SessionOpenOptions,
    ) -> Result<Self, DataError>;

    pub fn open_at_git_root(
        working_dir: PathBuf,
        git_root: PathBuf,
        opts: SessionOpenOptions,
    ) -> Result<Self, DataError>;

    // Read accessors
    pub fn id(&self) -> SessionId;
    pub fn working_dir(&self) -> &Path;
    pub fn git_root(&self) -> &Path;
    pub fn repo_config(&self) -> &RepoConfig;
    pub fn global_config(&self) -> &GlobalConfig;
    pub fn env(&self) -> &EnvSnapshot;
    pub fn flags(&self) -> &FlagConfig;
    pub fn default_agent(&self) -> Option<&AgentName>;
    pub fn available_agents(&self) -> &[AgentName];
    pub fn state(&self) -> &SessionState;
    pub fn created_at(&self) -> SystemTime;
    pub fn last_active_at(&self) -> SystemTime;
    pub fn uptime(&self) -> Duration;

    // Mutation
    pub fn state_mut(&mut self) -> &mut SessionState;
    pub fn touch(&mut self);
    pub fn set_flags(&mut self, flags: FlagConfig);
    pub fn set_env(&mut self, env: EnvSnapshot);
    pub fn set_available_agents(&mut self, agents: Vec<AgentName>);

    // Merged config view
    pub fn effective_config(&self) -> EffectiveConfig;
}
```

`Session::open` resolves the git root via the resolver, loads `RepoConfig` and `GlobalConfig` from disk, resolves the default agent using the precedence order (flags > repo config > global config), and records creation timestamps. It returns `DataError::GitRootNotFound` if the resolver fails.

`SessionOpenOptions` carries optional `FlagConfig`, an optional `EnvSnapshot`, and an optional `Vec<AgentName>` for available agents.

---

### SessionManager (`src/data/session_manager.rs`)

```rust
pub struct SessionManager { … }
```

A concurrency-safe collection of `Session` values backed by a `tokio::sync::RwLock`. All methods are `async`.

```rust
impl SessionManager {
    pub fn in_memory() -> Self;
    pub fn with_persistence(store: Arc<dyn SessionStore>) -> Self;

    pub async fn create(&self, session: Session) -> Result<SessionId, DataError>;
    pub async fn get(&self, id: SessionId) -> Result<Session, DataError>;
    pub async fn update<F, T>(&self, id: SessionId, f: F) -> Result<T, DataError>
    where F: FnOnce(&mut Session) -> T;
    pub async fn list(&self) -> Vec<Session>;
    pub async fn len(&self) -> usize;
    pub async fn is_empty(&self) -> bool;
    pub async fn remove(&self, id: SessionId) -> Result<(), DataError>;
    pub fn has_persistence(&self) -> bool;
}
```

`SessionManager::in_memory()` creates a manager with no persistence backend (used by the CLI for its single session and by the TUI for per-tab sessions). `SessionManager::with_persistence(store)` attaches a `SessionStore` backend that receives an `upsert` call on every `create` or `update` and a `remove` call on every `remove`. The API server uses this variant with `SqliteSessionStore`.

`update` takes a closure instead of returning `&mut Session` to avoid exposing an unguarded mutable reference across an `await` point.

`create` returns `DataError::SessionIdCollision` (instead of panicking) in the astronomically unlikely event of a UUID v4 collision.

#### `SessionStore` trait

```rust
pub trait SessionStore: Send + Sync {
    fn upsert(&self, session: &Session) -> Result<(), DataError>;
    fn remove(&self, id: SessionId) -> Result<(), DataError>;
}
```

The persistence interface implemented by Layer 0's `SqliteSessionStore` (and by `InMemorySessionStore` for tests).

---

### Configuration (`src/data/config/`)

All configuration concerns live in `src/data/config/`. Four source layers are merged in a fixed priority order:

```
flags  >  env  >  repo config  >  global config  >  built-in default
```

The merge is enforced by `EffectiveConfig` and is never duplicated elsewhere.

#### `RepoConfig` (`config/repo.rs`)

Per-repository configuration stored at `<git_root>/.awman/config.json`.

```rust
pub struct RepoConfig {
    pub agent: Option<String>,
    pub auto_agent_auth_accepted: Option<bool>,
    pub terminal_scrollback_lines: Option<usize>,
    pub yolo_disallowed_tools: Option<Vec<String>>,  // "yoloDisallowedTools" in JSON
    pub env_passthrough: Option<Vec<String>>,          // "envPassthrough" in JSON
    pub work_items: Option<WorkItemsConfig>,           // "workItems" in JSON
    pub overlays: Option<OverlaysConfig>,
    pub agent_stuck_timeout_secs: Option<u64>,        // "agentStuckTimeout" in JSON
}
```

Key methods:

| Method | Description |
|--------|-------------|
| `RepoConfig::path(git_root)` | Returns `<git_root>/.awman/config.json` |
| `RepoConfig::legacy_path(git_root)` | Returns `<git_root>/.awman/config.json` (pre-migration path) |
| `RepoConfig::load(git_root)` | Loads from disk; returns `default()` when absent, `DataError::ConfigParse` on malformed JSON |
| `RepoConfig::save(&self, git_root)` | Persists to disk, creating parent dirs as needed |
| `RepoConfig::migrate_legacy(git_root)` | Moves `.awman/config.json` → `.awman/config.json` if and only if legacy exists and new path does not; returns `true` when migration occurred |
| `RepoConfig::work_items_dir(git_root)` | Resolves configured work items directory |
| `RepoConfig::work_items_template(git_root)` | Resolves configured work item template path |

Nested types: `WorkItemsConfig` (dir, template), `OverlaysConfig` (directories list), `DirectoryOverlayConfig` (host, container, permission), `APIConfig` (workDirs, alwaysNonInteractive), `RemoteConfig` (defaultAddr, savedDirs, defaultAPIKey).

#### `GlobalConfig` (`config/global.rs`)

Global configuration stored at `$HOME/.awman/config.json`. The path is overridden by the `AWMAN_CONFIG_HOME` environment variable (used by tests to isolate state).

```rust
pub struct GlobalConfig {
    pub default_agent: Option<String>,
    pub terminal_scrollback_lines: Option<usize>,
    pub runtime: Option<String>,
    pub yolo_disallowed_tools: Option<Vec<String>>,
    pub env_passthrough: Option<Vec<String>>,
    pub API: Option<APIConfig>,
    pub remote: Option<RemoteConfig>,
    pub overlays: Option<OverlaysConfig>,
    pub agent_stuck_timeout_secs: Option<u64>,
}
```

Key methods:

| Method | Description |
|--------|-------------|
| `GlobalConfig::home_dir()` | Resolves `$AWMAN_CONFIG_HOME` or `$HOME/.awman` |
| `GlobalConfig::home_dir_with(env)` | Same, reading from an `EnvSnapshot` |
| `GlobalConfig::path()` / `path_with(env)` | Resolves the full config file path |
| `GlobalConfig::load()` / `load_with(env)` | Loads from disk; returns `default()` when absent |
| `GlobalConfig::save()` / `save_with(env)` | Persists to disk |

#### `EnvSnapshot` and `Env` (`config/env.rs`)

`EnvSnapshot` is a frozen snapshot of every environment variable awman reads. No scattered `std::env::var()` calls appear elsewhere in Layer 0.

```rust
pub struct EnvSnapshot { … }

impl EnvSnapshot {
    pub fn empty() -> Self;
    pub fn with_overrides<I, K, V>(entries: I) -> Self;
    pub fn get(&self, key: &str) -> Option<&str>;

    // Typed accessors for known vars
    pub fn config_home(&self) -> Option<PathBuf>;    // AWMAN_CONFIG_HOME
    pub fn api_root(&self) -> Option<PathBuf>;  // AWMAN_API_ROOT
    pub fn overlays(&self) -> Option<&str>;          // AWMAN_OVERLAYS
    pub fn remote_addr(&self) -> Option<&str>;       // AWMAN_REMOTE_ADDR
    pub fn remote_session(&self) -> Option<&str>;    // AWMAN_REMOTE_SESSION
    pub fn api_key(&self) -> Option<&str>;           // AWMAN_API_KEY
}
```

`Env` is a stateless namespace used to read from the real process environment at startup. Tests use `EnvSnapshot::with_overrides([("AWMAN_CONFIG_HOME", tmp_path)])` to avoid touching the filesystem.

Defined constants for every env var awman reads:

| Constant | Variable | Purpose |
|----------|----------|---------|
| `AWMAN_CONFIG_HOME` | `AWMAN_CONFIG_HOME` | Override global config home dir |
| `AWMAN_API_ROOT` | `AWMAN_API_ROOT` | Override API storage root |
| `AWMAN_OVERLAYS` | `AWMAN_OVERLAYS` | Comma-separated overlay specs |
| `AWMAN_REMOTE_ADDR` | `AWMAN_REMOTE_ADDR` | Override remote server address |
| `AWMAN_REMOTE_SESSION` | `AWMAN_REMOTE_SESSION` | Sticky session id for remote ops |
| `AWMAN_API_KEY` | `AWMAN_API_KEY` | API key for API server |

#### `FlagConfig` (`config/flags.rs`)

Typed struct carrying the flag values parsed by a frontend. Frontends (CLI via clap, TUI via the flag parser) populate a `FlagConfig` and pass it into `SessionOpenOptions`. The config layer itself never parses command-line strings.

Key fields: `agent`, `terminal_scrollback_lines`, `agent_stuck_timeout`, `non_interactive`, `env_passthrough`, `yolo_disallowed_tools`, `remote_addr`, `remote_session`, `api_key`.

#### `EffectiveConfig` (`config/effective.rs`)

The merged view of all four config sources. `Session::effective_config()` returns a fresh `EffectiveConfig` on demand; it is not cached on the session because flags can be updated via `Session::set_flags`.

```rust
pub struct EffectiveConfig {
    flags: FlagConfig,
    env: EnvSnapshot,
    repo: RepoConfig,
    global: GlobalConfig,
}

impl EffectiveConfig {
    pub fn new(flags, env, repo, global) -> Self;

    // Raw source access
    pub fn flags(&self) -> &FlagConfig;
    pub fn env(&self) -> &EnvSnapshot;
    pub fn repo(&self) -> &RepoConfig;
    pub fn global(&self) -> &GlobalConfig;

    // Merged accessors (precedence enforced internally)
    pub fn agent(&self) -> Option<String>;           // flag > repo > global
    pub fn env_passthrough(&self) -> Vec<String>;    // flag > repo > global > []
    pub fn yolo_disallowed_tools(&self) -> Vec<String>; // flag > repo > global > []
    pub fn scrollback_lines(&self) -> usize;         // flag > repo > global > 10_000
    pub fn agent_stuck_timeout(&self) -> Duration;   // flag > repo > global > 30s
    pub fn api_work_dirs(&self) -> Vec<String>; // global only
    pub fn always_non_interactive(&self) -> bool;    // flag > global > false
    pub fn remote_default_addr(&self) -> Option<String>;  // flag > env > global
    pub fn remote_default_api_key(&self) -> Option<String>; // flag > env > global
    pub fn remote_saved_dirs(&self) -> Vec<String>;  // global only
    pub fn remote_session(&self) -> Option<String>;  // flag > env
    pub fn runtime(&self) -> Option<String>;         // global only
}
```

Built-in defaults: `scrollback_lines` = 10,000 lines; `agent_stuck_timeout` = 30 seconds.

---

### Filesystem Stores (`src/data/fs/`)

Every direct filesystem or database interaction in Layer 0 is encapsulated in a typed object in this module. Higher layers consume these objects; they never call `std::fs::*` or `rusqlite::*` directly.

#### `SqliteSessionStore` (`fs/api_db.rs`)

Sqlite-backed persistence for API-mode session and command metadata. Schema is compatible with `oldsrc/commands/API/db.rs` so that existing on-disk databases written by earlier awman releases remain readable.

```rust
pub struct SqliteSessionStore { conn: Mutex<Connection> }

impl SqliteSessionStore {
    pub fn open(root: &Path) -> Result<Self, DataError>;
    pub fn open_from_paths(paths: &ApiPaths) -> Result<Self, DataError>;

    pub fn insert_session(&self, id, workdir, created_at) -> Result<(), DataError>;
    pub fn close_session(&self, id, closed_at) -> Result<(), DataError>;
    pub fn list_sessions(&self) -> Result<Vec<SessionRecord>, DataError>;
    pub fn get_session(&self, id) -> Result<Option<SessionRecord>, DataError>;

    pub fn insert_command(&self, id, session_id, subcommand, args, log_path) -> Result<(), DataError>;
    pub fn update_command_status(&self, id, status, exit_code, finished_at) -> Result<(), DataError>;
    pub fn list_commands(&self, session_id) -> Result<Vec<CommandRecord>, DataError>;
    pub fn get_command(&self, id) -> Result<Option<CommandRecord>, DataError>;
}
```

`SqliteSessionStore::open_at(path)` opens the database at `path`, enables WAL mode, and runs schema migrations idempotently. The database is shared by API mode and the amie daemon and lives at `<data_home>/data/awman.db`; it carries the `sessions` and `commands` tables plus amie's `amie_conditions` and `amie_runs`.

`SessionRecord` and `CommandRecord` are plain structs (no Arc, no async) that carry the persisted metadata fields.

#### `ApiPaths` (`fs/api_paths.rs`)

Typed accessors for every path used by the API server. Replaces ad-hoc `dirs::data_dir().join("awman/API/…")` calls scattered through the legacy code.

```rust
pub struct ApiPaths { root: PathBuf }

impl ApiPaths {
    pub fn from_env(env: &EnvSnapshot) -> Result<Self, DataError>;
    pub fn root(&self) -> &Path;
    pub fn db_path(&self) -> PathBuf;          // <data_home>/data/awman.db (shared)
    pub fn log_path(&self) -> PathBuf;         // <root>/awman.log
    pub fn pid_path(&self) -> PathBuf;         // <root>/awman.pid
    pub fn tls_dir(&self) -> PathBuf;          // <root>/tls/
    pub fn sessions_dir(&self) -> PathBuf;     // <root>/sessions/
    pub fn session_dir(&self, id) -> PathBuf;  // <root>/sessions/<id>/
    pub fn command_dir(&self, session_id, command_id) -> PathBuf;
    pub fn stdout_log(&self, session_id, command_id) -> PathBuf;
    pub fn stderr_log(&self, session_id, command_id) -> PathBuf;
}
```

`ApiPaths::from_env` reads `AWMAN_API_ROOT` from the snapshot; if unset, uses `$HOME/.awman/API`.

#### `WorkflowStateStore` (`fs/workflow_state.rs`)

Persists and retrieves `WorkflowInvocation` to/from disk. Replaces the free `pub fn` helpers `workflow_state_path`, `save_workflow_state`, `load_workflow_state`, and `validate_resume_compatibility` in the legacy code.

```rust
pub struct WorkflowStateStore { base_dir: PathBuf }

impl WorkflowStateStore {
    pub fn new(base_dir: PathBuf) -> Self;
    pub fn for_session(session: &Session) -> Self;

    pub fn state_path(&self, workflow_name: &str) -> PathBuf;
    pub fn save(&self, invocation: &WorkflowInvocation) -> Result<(), DataError>;
    pub fn load(&self, workflow_name: &str) -> Result<Option<WorkflowInvocation>, DataError>;
    pub fn validate_resume(&self, invocation: &WorkflowInvocation) -> Result<(), DataError>;
    pub fn remove(&self, workflow_name: &str) -> Result<(), DataError>;
}
```

Workflow state is stored as JSON at `<base_dir>/workflow-state/<workflow_name>.json`. `validate_resume` checks that the workflow hash in the stored invocation matches the hash of the workflow file on disk, returning `DataError::WorkflowResumeIncompatible` if they differ.

#### `SkillDirs` (`fs/skill_dirs.rs`)

Typed access to global and per-repo skill directories.

```rust
pub struct SkillDirs {
    global_dir: Option<PathBuf>,
    repo_dir: Option<PathBuf>,
}

impl SkillDirs {
    pub fn resolve(session: &Session) -> Self;
    pub fn global_dir(&self) -> Option<&Path>;
    pub fn repo_dir(&self) -> Option<&Path>;
    pub fn all_dirs(&self) -> Vec<&Path>;
}
```

Global skills live at `$HOME/.awman/skills/` (or `$AWMAN_CONFIG_HOME/skills/`). Per-repo skills live at `<git_root>/.awman/skills/`.

#### `WorkflowDirs` (`fs/workflow_dirs.rs`)

Typed access to global and per-repo workflow directories. Same structure as `SkillDirs`: global at `$HOME/.awman/workflows/`, per-repo at `<git_root>/.awman/workflows/`.

#### `WorkflowLogPaths` (`fs/log_dirs.rs`)

Resolves and writes per-container failure logs under `$HOME/.awman/logs/` (or `$AWMAN_CONFIG_HOME/logs/`). When a workflow step's container exits with a non-zero code that awman did not cause, the engine hands this type the buffered output tail and a `(workflow-id, step-name, container-name)` identity; it sanitises the name components, ensures the directory exists, and writes `{workflow-id}-{step-name}-{container-name}.log`. All filesystem access for the feature stays in Layer 0; the engine never touches `std::fs`.

#### `OverlayPathResolver` (`fs/overlay_paths.rs`)

Resolves overlay host paths from raw user input. Path *mounting* into containers is Layer 1; path *resolution* is Layer 0.

```rust
pub struct OverlayPathResolver;

impl OverlayPathResolver {
    pub fn new() -> Self;
    pub fn expand_tilde(path: &str) -> PathBuf;
    pub fn make_absolute_with_cwd(path: &str, cwd: &Path) -> PathBuf;
    pub fn make_absolute(path: &str) -> PathBuf;
    pub fn canonicalize_lossy(path: &Path) -> PathBuf;
}
```

`canonicalize_lossy` handles the common case of overlay paths that don't exist yet: it walks up to the nearest existing ancestor, canonicalises that, and re-appends the missing trailing components. This mirrors the behaviour of `oldsrc/overlays/make_host_path_canonical` from work item 0065.

#### `AuthPathResolver` (`fs/auth_paths.rs`)

Resolves host-side credential and settings paths for each supported agent. The *passthrough* of those paths into containers (file copying, scrubbing, bind-mount construction) is a Layer 1 concern.

```rust
pub struct AuthPathResolver { home: PathBuf }

impl AuthPathResolver {
    pub fn at_home(home: impl Into<PathBuf>) -> Self;
    pub fn from_process_env() -> Result<Self, DataError>;
    pub fn home(&self) -> &Path;
    pub fn resolve(&self, agent: &str) -> AgentAuthPaths;
}

pub struct AgentAuthPaths {
    pub agent: String,
    pub config_file: Option<PathBuf>,
    pub settings_dir: Option<PathBuf>,
}
```

`resolve("claude")` returns `config_file = Some(~/.claude.json)`, `settings_dir = Some(~/.claude)`. Each supported agent maps to its own file locations.

---

### Error Types (`src/data/error.rs`)

All Layer 0 errors are variants of `DataError`. Higher layers wrap `DataError` in their own error enums.

```rust
#[derive(Debug, Error)]
pub enum DataError {
    GitRootNotFound { working_dir: PathBuf },
    GitRootResolution { working_dir: PathBuf, message: String },
    SessionNotFound { id: Uuid },
    SessionIdCollision { id: Uuid },
    InvalidAgentName { name: String, reason: String },
    ConfigParse { path: PathBuf, source: serde_json::Error },
    ConfigSerialize { source: serde_json::Error },
    Io { path: PathBuf, source: std::io::Error },
    HomeNotFound,
    Sqlite(rusqlite::Error),
    WorkflowState(String),
    WorkflowResumeIncompatible(String),
    InvalidPath { path: PathBuf, reason: String },
}
```

`DataError::io(path, err)` and `DataError::config_parse(path, err)` are convenience constructors. `DataError` uses `thiserror` for `Display` and `Error::source` implementations.

---

## Layer 1: Engine (`src/engine/`)

Layer 1 is the engine layer: typed objects that own every runtime concern Layer 2 commands need to compose. It is built on top of Layer 0 and never calls into Layer 2, 3, or 4. When an engine needs user input or output it accepts a **frontend trait** defined by Layer 1 — higher layers implement that trait and pass it in at construction.

Three rules govern every engine in this layer:

1. **No direct I/O.** No `println!`, `eprintln!`, `tracing::info!` to user-facing output. All user-visible output flows through `UserMessageSink::write_message` or the appropriate frontend trait.
2. **No PTY, no `clap`, no `crossterm`, no `ratatui`.** Those are Layer 3 concerns.
3. **Typed objects over free functions.** Every significant abstraction is a struct with methods.

---

### `UserMessageSink` and `UserMessage` (`src/engine/message.rs`)

`UserMessageSink` is a supertrait of every frontend trait in Layer 1. Any type that implements `ContainerFrontend`, `WorkflowFrontend`, `ReadyFrontend`, `InitFrontend`, or `AgentFrontend` also implements `UserMessageSink`, so engine code can call `frontend.info(…)`, `frontend.warning(…)`, etc. anywhere a frontend reference is held.

```rust
pub struct UserMessage {
    pub level: MessageLevel,   // Info | Warning | Error | Success
    pub text: String,
}

pub trait UserMessageSink: Send + Sync {
    fn write_message(&mut self, msg: UserMessage);
    fn replay_queued(&mut self);

    // Convenience defaults:
    fn info(&mut self, text: impl Into<String>);
    fn warning(&mut self, text: impl Into<String>);
    fn error_msg(&mut self, text: impl Into<String>);
    fn success(&mut self, text: impl Into<String>);
}
```

**CLI queueing contract**: when a PTY-bound container owns the terminal, `write_message` queues the message instead of writing. `replay_queued` drains the queue after the container releases the terminal. TUI and API implementations render live and treat `replay_queued` as a no-op.

`RecordingMessageSink` (also in `message.rs`) records every message passed to it and is used by all engine unit tests.

---

### `EngineError` (`src/engine/error.rs`)

All Layer 1 failures are variants of `EngineError`. It wraps `DataError` for failures from Layer 0; higher layers wrap `EngineError` in their own error types.

Key variants:

| Variant | Meaning |
|---------|---------|
| `Data(DataError)` | Propagated from Layer 0 |
| `Git(String)` | Git subprocess failure |
| `Container(String)` | Backend container operation failure |
| `ConflictingOptions(String)` | Mutually exclusive `ContainerOption`s |
| `OptionNotSupportedByBackend { option, backend }` | Option irrelevant to chosen backend |
| `BackendUnsupportedOnPlatform { backend, platform }` | e.g. Apple Containers on Linux |
| `InvalidAdvanceAction(String)` | `NextAction` rejected by `WorkflowEngine` |
| `UnsupportedWorkflowSchemaVersion { found, supported }` | Persisted state is too new |
| `WorkflowResumeIncompatible(String)` | User declined drift-resume |
| `PlanModeUnsupported { agent }` | Agent does not support `--plan` |
| `AgentRequiresProjectImage { tag }` | Base image not built yet |

---

### `StepStatus` (`src/engine/step_status.rs`)

Shared across `ReadyEngine` and `InitEngine` for their summary structs.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StepStatus {
    Pending,
    Skipped,
    Running,
    Done,
    Failed(String),   // human-readable reason
}
```

---

### Container Engine (`src/engine/container/`)

The container engine provides a single typed factory for building and running containers. The concrete backend (Docker or Apple Containers) is selected once at construction and never exposed to callers outside the module.

#### `ContainerRuntime`

```rust
pub struct ContainerRuntime { /* holds Box<dyn ContainerBackend> — opaque */ }

impl ContainerRuntime {
    /// Inspect global_config to pick Docker (default) or Apple Containers.
    /// Returns BackendUnsupportedOnPlatform if Apple Containers is requested on non-macOS.
    /// Unknown runtime values are a fatal UnknownRuntime error (no Docker fallback).
    pub fn detect(global_config: &GlobalConfig) -> Result<Self, EngineError>;

    /// Name of the chosen backend ("docker" or "apple-containers"). Safe for display.
    pub fn runtime_name(&self) -> &'static str;

    /// Build a fully-configured ContainerInstance from the given options.
    pub fn build(&self, options: impl IntoIterator<Item = ContainerOption>)
        -> Result<Box<dyn ContainerInstance>, EngineError>;

    pub fn list_running(&self, session: &Session) -> Result<Vec<ContainerHandle>, EngineError>;
    pub fn stats(&self, handle: &ContainerHandle) -> Result<ContainerStats, EngineError>;
    pub fn stop(&self, handle: &ContainerHandle) -> Result<(), EngineError>;
}
```

Backend selection rules: `"docker"` or absent → Docker; `"apple-containers"` on macOS → Apple; `"apple-containers"` on non-macOS → `EngineError::BackendUnsupportedOnPlatform`; unknown value → warn + Docker.

#### `ContainerOption`

Every knob a container invocation accepts. Adding a new option is one new variant plus one branch in `ResolvedContainerOptions::ingest` — no changes to call sites needed.

```rust
pub enum ContainerOption {
    Image(ImageRef),
    Entrypoint(Entrypoint),
    Overlay(OverlaySpec),
    EnvPassthrough(EnvVar),
    EnvLiteral(EnvLiteral),
    SeededPrompt(String),
    Interactive(bool),
    AllowDocker(bool),
    MountSsh { source: PathBuf },
    Yolo(YoloMode),
    Auto(AutoMode),
    Plan(PlanMode),
    WorkingDir(PathBuf),
    Name(ContainerName),
    Cpu(CpuLimit),
    Memory(MemoryLimit),
    AgentSettingsPassthrough(AgentSettings),
    AgentCredentials { env_vars: Vec<(String, String)> },
    DisallowedTools(Vec<String>),
    AllowedTools(Vec<String>),
    Model { flag: ModelFlagForm },
    NonInteractivePrintFlag(String),
    DockerfileUser(String),
}
```

`ModelFlagForm` distinguishes `--model NAME` (Argument) from standalone shorthands like `--model-claude-opus-4-6` (Shorthand).

#### `ContainerInstance` and `ContainerExecution`

```rust
pub trait ContainerInstance: Send + Sync {
    fn id(&self) -> &ContainerId;
    fn name(&self) -> &ContainerName;
    fn image(&self) -> &ImageRef;
    fn run_with_frontend(self: Box<Self>, frontend: Box<dyn ContainerFrontend>)
        -> Result<ContainerExecution, EngineError>;
}

pub struct ContainerExecution { /* owns running handle + exit futures */ }

impl ContainerExecution {
    pub async fn wait(self) -> Result<ContainerExitInfo, EngineError>;
    pub fn handle(&self) -> &ContainerHandle;
    pub fn cancel(&self) -> Result<(), EngineError>;
    /// Hand ownership of the running container back to the caller without joining.
    pub fn detach(self) -> ContainerHandle;
}
```

`ContainerExitInfo` carries `exit_code`, `signal` (if applicable), `started_at`, and `ended_at`.

#### `ContainerFrontend` trait

Defined by Layer 1, implemented by Layer 3. Governs all I/O the container runtime needs from the outside world.

```rust
pub trait ContainerFrontend: UserMessageSink + Send + Sync {
    fn write_stdout(&mut self, bytes: &[u8]) -> Result<(), EngineError>;
    fn write_stderr(&mut self, bytes: &[u8]) -> Result<(), EngineError>;
    fn read_stdin(&mut self, buf: &mut [u8]) -> Result<usize, EngineError>;
    fn report_status(&mut self, status: ContainerStatus);
    fn report_progress(&mut self, progress: ContainerProgress);
    fn resize_pty(&mut self, cols: u16, rows: u16);
}
```

PTY allocation is a Layer 3 concern. Layer 1 passes raw bytes to the frontend and lets it decide whether they route through a PTY (TUI), straight to fds (CLI), or over a socket (API).

#### What is forbidden in `src/engine/container/`

- No `pub fn run_container_with_*` style APIs.
- `docker.rs` and `apple.rs` are `pub(super)` — no caller outside the module can name the backend type.
- No direct PTY allocation or `crossterm` use.
- No `println!` / `eprintln!`. All output goes through `ContainerFrontend`.

---

### Workflow Engine (`src/engine/workflow/`)

`WorkflowEngine` owns every workflow execution concern: step ordering, state advancement, yolo/auto countdowns, stuck detection, per-step agent and model resolution, exit-code interpretation, step persistence, and container lifecycle management per step.

```rust
pub struct WorkflowEngine {
    session: Session,
    workflow: Workflow,                    // parsed definition (Layer 0)
    dag: WorkflowDag,                      // Layer 0 — cycle-free adjacency
    state: WorkflowState,                  // Layer 0 — serializable snapshot
    state_store: WorkflowStateStore,       // Layer 0 — git-root-scoped I/O
    effective_config: EffectiveConfig,     // Layer 0 — for agent/model fallbacks
    frontend: Box<dyn WorkflowFrontend>,
    container_factory: Box<dyn ContainerExecutionFactory>,
    git_engine: Arc<GitEngine>,
    overlay_engine: Arc<OverlayEngine>,
    // … current_execution, current_step tracking fields …
}

impl WorkflowEngine {
    pub fn new(session, workflow, frontend, factory, git_engine, overlay_engine)
        -> Result<Self, EngineError>;
    pub async fn resume(session, workflow, frontend, factory, git_engine, overlay_engine)
        -> Result<Self, EngineError>;

    pub async fn run_to_completion(&mut self) -> Result<WorkflowOutcome, EngineError>;
    pub async fn step_once(&mut self) -> Result<StepOutcome, EngineError>;
    pub fn compute_available_actions(&self) -> Result<AvailableActions, EngineError>;
    pub fn state(&self) -> &WorkflowState;
}
```

#### Per-step agent and model resolution

Resolution order (each level overrides the previous):
1. Step-level `agent`/`model` fields.
2. Workflow-level `agent`/`model` defaults.
3. `EffectiveConfig` fallback (flags > env > repo > global).

The resolved pair is logged via `tracing` and passed to the factory via `WorkflowRuntimeContext { step_agent, step_model, git_root, session_id }`.

#### `NextAction` and `AvailableActions`

After each step completes, `WorkflowEngine` asks the frontend which action to take:

```rust
pub enum NextAction {
    LaunchNext,
    ContinueInCurrentContainer { prompt: String },
    RestartCurrentStep,
    CancelToPreviousStep,
    FinishWorkflow,    // mark remaining steps Skipped; only valid on last step
    Pause,
    Abort,
}
```

The engine computes `AvailableActions` before calling `user_choose_next_action`, encoding which actions are legal given the current step configuration. The frontend renders only the available set.

`ContinueInCurrentContainer` is unavailable when: the next step targets a different agent or model; the running container has already exited; or the factory's `inject_prompt` returns `None`. `CancelToPreviousStep` is unavailable on the first step.

#### `ContainerExecutionFactory` trait

Layer 2 builds a factory that `WorkflowEngine` calls per step. The engine never sees raw `ContainerOption` lists or frontend implementations.

```rust
pub trait ContainerExecutionFactory: Send + Sync {
    fn execution_for_step(&self, step, session, runtime) -> Result<ContainerExecution, EngineError>;
    fn inject_prompt(&self, execution, prompt) -> Result<Option<()>, EngineError>;
}
```

#### `WorkflowFrontend` trait

```rust
pub trait WorkflowFrontend: UserMessageSink + Send + Sync {
    fn user_choose_next_action(&mut self, state, available) -> Result<NextAction, EngineError>;
    fn confirm_resume(&mut self, mismatch: &ResumeMismatch) -> Result<bool, EngineError>;
    fn user_choose_after_step_failure(&mut self, step, exit) -> Result<StepFailureChoice, EngineError>;
    fn report_step_status(&mut self, step, status: WorkflowStepStatus);
    fn report_step_output(&mut self, step, output: StepOutput);
    fn report_step_stuck(&mut self, step);
    fn report_step_unstuck(&mut self, step);
    fn yolo_countdown_tick(&mut self, remaining: Duration) -> Result<YoloTickOutcome, EngineError>;
    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome);
}
```

#### Stuck detection and yolo countdown

`WorkflowEngine` owns two timers:

1. **Stuck timer** — fires when the agent produces no PTY output for `EffectiveConfig::agent_stuck_timeout` (default 30 s). Triggers `report_step_stuck`.
2. **Yolo countdown** — only when `--yolo` is set and the stuck timer has fired. Counts down `YOLO_COUNTDOWN_DURATION` (60 s, defined in `timing.rs`) before auto-advancing via `NextAction::LaunchNext`. Backoff: `STUCK_DIALOG_BACKOFF` (60 s) prevents re-firing immediately after a dismissed countdown.

#### Workflow state persistence

State is persisted to `<git-root>/.awman/workflows/<hash8>-<name>.json` after every step transition. On resume, the engine checks `schema_version`; if the persisted version is newer than `WORKFLOW_STATE_SCHEMA_VERSION`, it returns `EngineError::UnsupportedWorkflowSchemaVersion`. If the workflow hash has drifted, it calls `confirm_resume`; if declined, it returns `WorkflowResumeIncompatible`.

#### What is forbidden in `WorkflowEngine`

- No direct container construction. Containers arrive pre-built via `ContainerExecutionFactory`.
- No rendering, no `eprintln!`, no user-console `tracing`. Status flows through the frontend.
- No worktree lifecycle management. The engine operates on a given `git_root` and is unaware of whether it is a worktree. Worktree creation/removal is a Layer 2 concern.
- No `clap`, no `crossterm`, no `ratatui`.
- No DAG logic or state persistence code — those live in `src/data/`.

---

### Git Engine (`src/engine/git/`)

`GitEngine` consolidates every git operation awman performs. It is a stateless struct whose methods are the only public surface. It implements Layer 0's `GitRootResolver` trait so `Session::open` can use it.

```rust
pub struct GitEngine;

impl GitEngine {
    pub fn new() -> Self;
    pub fn version_check(&self) -> Result<GitVersion, EngineError>;
    pub fn resolve_root(&self, working_dir: &Path) -> Result<PathBuf, EngineError>;
    pub fn is_clean(&self, path: &Path) -> Result<bool, EngineError>;
    pub fn uncommitted_files(&self, path: &Path) -> Result<Vec<String>, EngineError>;

    // Worktree paths (convention: ~/.awman/worktrees/<repo>/<NNNN>/ or wf-<name>/)
    pub fn worktree_path(&self, git_root: &Path, work_item: u32) -> Result<PathBuf, EngineError>;
    pub fn worktree_path_named(&self, git_root: &Path, name: &str) -> Result<PathBuf, EngineError>;
    pub fn branch_name_for_work_item(&self, work_item: u32) -> String;  // awman/work-item-NNNN
    pub fn branch_name_for_workflow(&self, name: &str) -> String;       // awman/workflow-<name>

    pub fn create_worktree(&self, git_root, worktree_path, branch) -> Result<(), EngineError>;
    pub fn remove_worktree(&self, git_root, worktree_path) -> Result<(), EngineError>;

    // Merge strategies:
    //   squash: git merge --squash <branch> + git commit -m "Implement <branch>"
    //   plain:  git merge -m "Merge <branch>" <branch> (preserves branch commits)
    pub fn merge_branch(&self, git_root: &Path, branch: &str) -> Result<(), EngineError>;
    pub fn commit_all(&self, path: &Path, message: &str) -> Result<(), EngineError>;
    pub fn delete_branch(&self, git_root: &Path, branch: &str) -> Result<(), EngineError>;
    pub fn branch_exists(&self, git_root: &Path, branch: &str) -> bool;
    pub fn is_detached_head(&self, git_root: &Path) -> bool;
}
```

Naming conventions enforced by `GitEngine`:
- Worktree path (work-item): `$HOME/.awman/worktrees/<repo-name>/<NNNN>/`
- Worktree path (workflow): `$HOME/.awman/worktrees/<repo-name>/wf-<workflow-name>/`
- Branch (work-item): `awman/work-item-<NNNN>` (zero-padded 4 digits)
- Branch (workflow): `awman/workflow-<workflow-name>`
- Merge commit: `"Implement <branch>"` (verbatim format preserved)

---

### Overlay Engine (`src/engine/overlay/`)

`OverlayEngine` consolidates overlay construction and management. Layer 0 resolves host paths; Layer 1 builds `OverlaySpec` values that `ContainerOption::Overlay` accepts.

```rust
pub struct OverlayEngine {
    path_resolver: OverlayPathResolver,
    auth_resolver: AuthPathResolver,
}

impl OverlayEngine {
    pub fn new(session: &Session) -> Result<Self, EngineError>;

    pub fn build_overlays(
        &self,
        session: &Session,
        request: &OverlayRequest,
    ) -> Result<Vec<OverlaySpec>, EngineError>;

    pub fn resolve_user_overlay(&self, spec: &str) -> Result<DirectoryOverlay, EngineError>;
    pub fn agent_settings_overlays(&self, agent: &AgentName) -> Result<Vec<OverlaySpec>, EngineError>;
}
```

`OverlayRequest` describes the desired overlays for a given invocation. `build_overlays` returns the resolved, deduplicated, canonicalized set; callers pass each item as `ContainerOption::Overlay`.

Per-agent settings handling (`agent_settings_overlays`) replicates the legacy `HostSettings` machinery:

- **Claude**: mounts `~/.claude.json` (with `oauthAccount` field stripped), mounts `~/.claude/` (applying `CLAUDE_DENYLIST` to exclude telemetry/history entries), sets `skipDangerousModePermissionPrompt: true` in `settings.json` when yolo mode is active, and always sets `hasShownLspRecommendation: true`.
- **Minimal fallback**: when `~/.claude.json` is absent, produces a synthesized overlay with `/workspace` project trust and LSP suppression.
- **Non-Claude agents**: each maps to a single agent-config-dir overlay (host path + container path per the agent matrix).

`CLAUDE_DENYLIST` is a named constant — adding a new excluded entry is a one-line change.

---

### Auth Engine (`src/engine/auth/`)

`AuthEngine` consolidates two previously-separate concerns:

1. **Host-side agent credential discovery** — resolving credentials from the OS keychain to inject into agent containers.
2. **API server authentication** — API key generation, hashing, comparison, persistence, and TLS material.

```rust
pub struct AuthEngine {
    auth_paths: AuthPathResolver,
    api_paths: ApiPaths,
}

impl AuthEngine {
    pub fn new(session: &Session) -> Self;

    // Keychain credentials
    pub fn agent_keychain_credentials(&self, agent: &AgentName) -> Result<AgentCredentials, EngineError>;
    pub fn resolve_agent_auth(&self, session: &Session, agent: &AgentName)
        -> Result<AgentCredentials, EngineError>;

    // API API-key lifecycle
    pub fn generate_api_key(&self) -> Result<ApiKey, EngineError>;
    pub fn write_api_key_hash(&self, hash: &ApiKeyHash) -> Result<(), EngineError>;
    pub fn read_api_key_hash(&self) -> Result<Option<ApiKeyHash>, EngineError>;
    pub fn verify_api_key(&self, presented: &ApiKey) -> Result<AuthOutcome, EngineError>;
    pub fn refresh_api_key(&self) -> Result<ApiKey, EngineError>;

    // TLS material
    pub fn ensure_self_signed_tls(&self, bind_ip: IpAddr) -> Result<TlsMaterial, EngineError>;
    pub fn load_tls_from_paths(&self, cert: &Path, key: &Path) -> Result<TlsMaterial, EngineError>;
}
```

All cryptographic comparisons in `verify_api_key` use `subtle::ConstantTimeEq`, including the case where no hash file exists (compared against a fixed-length sentinel to prevent timing-based "is auth disabled?" leaks).

`keychain.rs` provides the per-OS keychain backend (macOS Keychain, Linux libsecret, Windows credential manager) via the `keyring` crate. The per-agent env-var name set (e.g. `ANTHROPIC_API_KEY` for Claude) is co-located with the agent matrix.

`AuthEngine` only resolves credentials; the "offer to use keychain credentials silently vs. prompt every time" decision is a Layer 2 concern driven by `EffectiveConfig::auto_agent_auth_accepted`.

---

### Agent Engine (`src/engine/agent/`)

`AgentEngine` centralises the cross-cutting agent concerns called from multiple commands (`chat`, `exec`, `ready`, `specs amend`): ensuring the agent is available (Dockerfile + image), and building the `ContainerOption` list for a given invocation. Centralising here ensures adding a new agent type or changing model-flag injection is a single-file edit.

```rust
pub struct AgentEngine {
    overlay_engine: Arc<OverlayEngine>,
    container_runtime: Arc<ContainerRuntime>,
}

pub struct AgentRunOptions {
    pub yolo: Option<YoloMode>,
    pub auto: Option<AutoMode>,
    pub plan: Option<PlanMode>,
    pub allowed_tools: Vec<String>,
    pub initial_prompt: Option<String>,
    pub allow_docker: bool,
    pub non_interactive: bool,
}

impl AgentEngine {
    pub fn new(overlay_engine, container_runtime) -> Self;

    /// Ensure the agent Dockerfile and image exist; download/build if absent.
    /// Idempotent: no steps fire and no container_frontend is requested when
    /// both already exist.
    pub async fn ensure_available(
        &self, agent, config, frontend: &mut dyn AgentFrontend,
    ) -> Result<(), EngineError>;

    /// Build the ContainerOption list for running an agent container.
    /// Resolves overlays, injects model flags, autonomous flags, and all
    /// agent-specific entrypoint options. Pass the result to ContainerRuntime::build.
    pub fn build_options(
        &self, agent, model, run_options, session,
    ) -> Result<Vec<ContainerOption>, EngineError>;
}
```

`ensure_available` steps:
1. Check for `<git-root>/.awman/Dockerfile.<agent>`; download if absent.
2. Check for `<repo-hash>:<agent>:latest` locally; build if absent.
3. If the project base image (`<repo-hash>:latest`) is missing, fail with `EngineError::AgentRequiresProjectImage` — `AgentEngine` does not build the project image (`ReadyEngine`'s job).

#### Agent matrix (`agent_matrix.rs`)

All per-agent branching — entrypoints, non-interactive flags, plan-mode flags, yolo flags, model flags, image tags, Dockerfile paths, and download URLs — lives exclusively in `agent_matrix.rs`. Adding a new agent is a single-file edit.

Supported agents: `claude`, `codex`, `opencode`, `maki`, `gemini`, `copilot`, `crush`, `cline`.

Key per-agent distinctions:

| Agent | Interactive entrypoint | Non-interactive flag | Plan-mode flag |
|-------|------------------------|----------------------|----------------|
| `claude` | `claude` | `--print` / `-p` | `--permission-mode plan` |
| `codex` | `codex` | `exec`/`run` subcommand | `--approval-mode plan` |
| `opencode` | `opencode` | `run` subcommand | (unsupported — error) |
| `gemini` | `gemini` | varies | `--approval-mode=plan` |
| `copilot` | `copilot -i` | varies | `--plan` |
| `cline` | `cline` | `task` subcommand | `--plan` |
| `crush` | `crush` | `run` subcommand | (unsupported — error) |
| `maki` | `maki` | varies | (unsupported — error) |

`AgentEngine::build_options` with `PlanMode::Enabled` for an agent that does not support plan returns `EngineError::PlanModeUnsupported { agent }`.

#### `AgentFrontend` trait

```rust
pub trait AgentFrontend: UserMessageSink + Send + Sync {
    fn report_step_status(&mut self, step: &str, status: StepStatus);
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend>;
}
```

#### Canonical usage pattern

```rust
// The only sanctioned way to prepare and run an agent container:
agent_engine.ensure_available(&agent, &config, &mut frontend).await?;
let opts = agent_engine.build_options(&agent, &model, &run_options, &session)?;
let instance = container_runtime.build(opts)?;
let execution = instance.run_with_frontend(Box::new(container_frontend))?;
let exit = execution.wait().await?;
```

Duplicating `ensure_available` or `build_options` logic in any other module is a violation.

---

### Ready Engine (`src/engine/ready/`)

`ReadyEngine` owns all multi-phase logic for `awman ready`: preflight checks, Dockerfile.dev creation, Docker image builds, local agent check, audit container run, and post-audit rebuild. The legacy code (`oldsrc/commands/ready.rs`: 2239 lines, `oldsrc/commands/ready_flow.rs`: 726 lines) is replaced entirely.

#### Phase state machine

```rust
pub enum ReadyPhase {
    Preflight,                    // runtime detection, git root, config, env vars
    AwaitingDockerfileDecision,   // Dockerfile.dev absent or unmodified template
    CreatingDockerfile,           // write Dockerfile.dev from project template
    BuildingBaseImage,            // build/rebuild project Docker image
    BuildingAgentImage,           // build/rebuild agent Docker image
    CheckingLocalAgent,           // send random greeting to local agent
    RunningAudit,                 // audit container scans/updates Dockerfile.dev
    RebuildingAfterAudit,         // rebuild after audit modifies Dockerfile.dev
    Complete,
    Failed(ReadyFailure),
}
```

The state machine is forward-only. If the process is interrupted the user re-runs `awman ready` from the beginning; no partial checkpoint is written.

#### `ReadyEngine` API

```rust
pub struct ReadyEngine { /* session, engines, options, phase */ }

pub struct ReadyEngineOptions {
    pub agent: AgentName,
    pub refresh: bool,
    pub build: bool,
    pub no_cache: bool,
    pub allow_docker: bool,
}

impl ReadyEngine {
    pub fn new(session, git_engine, overlay_engine, container_runtime, agent_engine, options) -> Self;
    pub fn phase(&self) -> &ReadyPhase;

    /// Advance exactly one phase, calling appropriate ReadyFrontend methods. Returns new phase.
    pub async fn step(&mut self, frontend: &mut dyn ReadyFrontend) -> Result<ReadyPhase, EngineError>;

    /// Drive to completion (calls step in a loop). Returns ReadySummary.
    pub async fn run_to_completion(&mut self, frontend: &mut dyn ReadyFrontend) -> Result<ReadySummary, EngineError>;

    pub fn summary(&self) -> ReadySummary;
}
```

#### `ReadyFrontend` trait

```rust
pub trait ReadyFrontend: UserMessageSink + Send + Sync {
    fn ask_create_dockerfile(&mut self) -> Result<bool, EngineError>;
    fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError>;
    fn report_phase(&mut self, phase: &ReadyPhase);
    fn report_step_status(&mut self, step: &str, status: StepStatus);
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend>;
    fn report_summary(&mut self, summary: &ReadySummary);
}
```

#### `ReadySummary`

```rust
pub struct ReadySummary {
    pub runtime_name: String,
    pub base_image: StepStatus,
    pub agent_image: StepStatus,
    pub local_agent: StepStatus,
    pub audit: StepStatus,
}
```

---

### Init Engine (`src/engine/init/`)

`InitEngine` owns all multi-phase logic for `awman init`: git root resolution, aspec folder creation, Dockerfile.dev setup, `.awman.json` config write, optional audit container, image build, and work-items configuration. Replaces `oldsrc/commands/init.rs` + `oldsrc/commands/init_flow.rs` (2702 lines combined).

#### Phase state machine

```rust
pub enum InitPhase {
    Preflight,                  // resolve git root, validate environment
    AwaitingAspecDecision,      // existing aspec folder found
    CreatingAspecFolder,        // write aspec template into repo
    SettingUpDockerfile,        // create/confirm Dockerfile.dev
    WritingConfig,              // write or update .awman.json
    AwaitingAuditDecision,      // ask whether to run audit
    BuildingImage,              // build base Docker image
    RunningAudit,               // agent scans and updates Dockerfile.dev
    AwaitingWorkItemsDecision,  // ask whether to configure work items
    WritingWorkItemsConfig,     // write work-items config into .awman.json
    Complete,
    Failed(InitFailure),
}
```

Forward-only. If the user declines `AwaitingAspecDecision`, `aspec_folder` is `StepStatus::Skipped` and remaining phases continue.

#### `InitEngine` API

```rust
pub struct InitEngineOptions {
    pub agent: AgentName,
    pub run_aspec_setup: bool,
    pub git_root: PathBuf,
}

impl InitEngine {
    pub fn new(session, git_engine, overlay_engine, container_runtime, options) -> Self;
    pub fn phase(&self) -> &InitPhase;
    pub async fn step(&mut self, frontend: &mut dyn InitFrontend) -> Result<InitPhase, EngineError>;
    pub async fn run_to_completion(&mut self, frontend: &mut dyn InitFrontend) -> Result<InitSummary, EngineError>;
    pub fn summary(&self) -> &InitSummary;
}
```

#### `InitFrontend` trait

```rust
pub trait InitFrontend: UserMessageSink + Send + Sync {
    fn ask_replace_aspec(&mut self) -> Result<bool, EngineError>;
    fn ask_run_audit(&mut self) -> Result<bool, EngineError>;
    fn ask_work_items_setup(&mut self) -> Result<Option<WorkItemsConfig>, EngineError>;
    fn report_phase(&mut self, phase: &InitPhase);
    fn report_step_status(&mut self, step: &str, status: StepStatus);
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend>;
    fn report_summary(&mut self, summary: &InitSummary);
}
```

#### `InitSummary`

```rust
pub struct InitSummary {
    pub config: StepStatus,
    pub aspec_folder: StepStatus,
    pub dockerfile: StepStatus,
    pub audit: StepStatus,
    pub image_build: StepStatus,
    pub work_items_setup: StepStatus,
}
```

---

### Layer 0 additions required by Layer 1 (`src/data/`)

Three modules were added to Layer 0 as part of work item 0067 because they are stateless functions over serializable types — not engine logic.

#### `WorkflowDag` (`src/data/workflow_dag.rs`)

```rust
pub struct WorkflowDag { /* adjacency; constructed via WorkflowDag::build */ }

impl WorkflowDag {
    pub fn build(steps: &[WorkflowStep]) -> Result<Self, DataError>;
    pub fn ready_steps(&self, completed: &HashSet<String>) -> Vec<String>;
    pub fn topological_order(&self) -> Vec<String>;
}

pub fn validate_references(steps: &[WorkflowStep]) -> Result<(), DataError>;
pub fn detect_cycle(steps: &[WorkflowStep]) -> Result<(), DataError>;
```

`build` returns `DataError::MissingDependency` for unknown `depends_on` entries and `DataError::CyclicDependency` for cycles. `topological_order` is deterministic across calls.

#### `WorkflowState` and `StepState` (`src/data/workflow_state.rs`)

Fully serializable snapshot of workflow execution state. Stored per-workflow at `<git-root>/.awman/workflows/`.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowState {
    pub schema_version: u32,
    pub workflow_name: String,
    pub workflow_hash: String,
    pub step_states: HashMap<String, StepState>,
    pub completed_steps: HashSet<String>,
    pub current_step_index: Option<usize>,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub enum StepState {
    Pending,
    Running,
    Succeeded,
    Failed { exit_code: i32, error_message: Option<String> },
    Skipped,
    Cancelled,
}
```

`WorkflowEngine` rejects state whose `schema_version` exceeds `WORKFLOW_STATE_SCHEMA_VERSION` with `EngineError::UnsupportedWorkflowSchemaVersion`.

#### `WorkflowStateStore` (`src/data/workflow_state_store.rs`)

```rust
pub struct WorkflowStateStore { base_dir: PathBuf }

impl WorkflowStateStore {
    pub fn new(session: &Session) -> Self;       // base_dir = <git-root>/.awman/workflows/
    pub fn at_git_root(git_root: &Path) -> Self; // convenience for tests
    pub fn load(&self, workflow_name: &str) -> Result<Option<WorkflowState>, DataError>;
    pub fn save(&self, state: &WorkflowState) -> Result<(), DataError>;
    pub fn delete(&self, workflow_name: &str) -> Result<(), DataError>;
}
```

---

## Layer 2: Command (`src/command/`)

Layer 2 is the command layer: typed objects that own every piece of business logic a frontend needs to express. It is built on top of Layer 0 (data) and Layer 1 (engine) and never calls into Layer 3 (frontends) or Layer 4 (the binary). When a command needs user input or output it accepts a **frontend trait defined by Layer 2** — Layer 3 implements that trait and passes it in at invocation time.

Four rules govern this layer:

1. **Layer 2 consumes Layer 0 and Layer 1 only.** No upward calls into frontends or the binary.
2. **Frontends contain no business logic.** Every command knob — every flag, every prompt, every dialog — flows through Layer 2's `Dispatch` system or a per-command frontend trait.
3. **Typed objects over `pub fn`.** Each command is a `*Command` struct that implements `Command` and exposes `run_with_frontend(frontend) -> Result<Outcome, CommandError>`.
4. **The full list of available commands and flags lives only in `CommandCatalogue`.** Frontends never hard-code command names, flag names, or defaults; they ask the catalogue (or its projections) for what's available. This is the single most important guarantee against mode drift across CLI, TUI, and API.

---

### `Command` trait (`src/command/commands/command_trait.rs`)

Every `*Command` struct implements this trait:

```rust
#[async_trait]
pub trait Command {
    type Frontend: Send;
    type Outcome;

    async fn run_with_frontend(
        self,
        frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError>;
}
```

`Frontend` is the per-command associated type — e.g. `Box<dyn ExecWorkflowCommandFrontend>`. `Outcome` is the typed value the command returns on success, always `Serialize`-able for `--json` callers.

---

### `CommandError` (`src/command/error.rs`)

All Layer 2 failures are variants of `CommandError`. It wraps `EngineError` (Layer 1) and `DataError` (Layer 0) for failures from below. Layer 3 wraps `CommandError` in its own user-facing presentation.

Key variants:

| Variant | Meaning |
|---------|---------|
| `Engine(EngineError)` | Propagated from Layer 1 |
| `Data(DataError)` | Propagated from Layer 0 |
| `UnknownCommand { path }` | `Dispatch::run_command` received an unrecognised path |
| `UnknownFlag { command, flag }` | Frontend supplied a flag not in the catalogue |
| `MissingRequiredFlag { command, flag }` | Required flag was absent |
| `MissingRequiredArgument { command, argument }` | Required positional argument was absent |
| `MutuallyExclusive { command, a, b }` | Two conflicting flags were both supplied |
| `InvalidFlagValue { command, flag, reason }` | Flag value failed type/enum validation |
| `InvalidArgumentValue { command, argument, reason }` | Positional argument failed validation |
| `CommandBoxParse(String)` | TUI command-box input could not be tokenised |
| `Aborted` | User chose to abort in an interactive prompt |
| `MergeConflict { branch, worktree_path }` | `WorktreeLifecycle::finalize` encountered a git merge conflict |
| `MissingRemoteAddress` | No `--remote-addr` / `AWMAN_REMOTE_ADDR` supplied |
| `MissingApiKey` | API key could not be resolved from any source |
| `RemoteTimeout` | HTTP request to remote server timed out |
| `RemoteConnectionRefused(String)` | Connection to remote server was refused |
| `RemoteHttpStatus { status, body }` | Remote returned a non-2xx HTTP status |
| `MalformedSseEvent(String)` | SSE stream contained an unparseable event |
| `RemoteTransport(String)` | Underlying HTTP transport error |
| `ApiWorkdirNotFound { path }` | A workdir path supplied to `api start` does not exist |
| `APIAlreadyRunning { pid }` | API server is already running on the given PID |

Convenience constructors: `CommandError::unknown_command`, `missing_required_flag`, `missing_required_argument`, `unknown_flag`, `mutually_exclusive`.

---

### `CommandCatalogue` (`src/command/dispatch/catalogue.rs`)

`CommandCatalogue` is a single static (via `OnceLock`) data structure that enumerates every command, subcommand, argument, and flag exactly once. It is the sole source of truth for the command surface; no frontend or projection may hard-code names, defaults, or types independently.

#### Supporting types

```rust
pub enum FrontendVisibility {
    All,          // CLI, TUI, and API
    CliOnly,
    TuiOnly,
    CliAndTui,
    Hidden,
}

pub enum FlagKind {
    Bool,
    String,
    OptionalString,
    Enum(&'static [&'static str]),
    VecString,    // repeatable: --foo a --foo b
    Path,
    OptionalPath,
    U16,
}

pub enum FlagDefault { None, Bool(bool), Str(&'static str), U16(u16), EmptyVec }

pub struct FlagSpec {
    pub long: &'static str,
    pub short: Option<char>,
    pub help: &'static str,
    pub kind: FlagKind,
    pub default: FlagDefault,
    pub frontends: FrontendVisibility,
    pub conflicts_with: &'static [&'static str],
    pub implies: &'static [&'static str],
    pub optional: bool,
}

pub enum ArgumentKind {
    String,
    OptionalString,
    Path,
    OptionalPath,
    TrailingVarArgs,  // <COMMAND>... style; triggers trailing_var_arg + allow_hyphen_values
}

pub struct ArgumentSpec {
    pub name: &'static str,
    pub help: &'static str,
    pub kind: ArgumentKind,
    pub optional: bool,
}

pub struct CommandSpec {
    pub name: &'static str,
    pub aliases: &'static [&'static str],    // string aliases ("wf" for "exec workflow")
    pub path_aliases: &'static [&'static [&'static str]],  // path aliases (["specs","new"] → ["new","spec"])
    pub help: &'static str,
    pub long_help: Option<&'static str>,
    pub arguments: &'static [ArgumentSpec],
    pub flags: &'static [FlagSpec],
    pub subcommands: &'static [&'static CommandSpec],
}
```

#### Catalogue API

```rust
impl CommandCatalogue {
    pub fn get() -> &'static CommandCatalogue;
    pub fn root() -> &'static CommandSpec;
    pub fn lookup(path: &[&str]) -> Option<&'static CommandSpec>;
    pub fn lookup_with_aliases(path: &[&str]) -> Option<&'static CommandSpec>;
}
```

`lookup_with_aliases` resolves string aliases (e.g. `"wf"` → `["exec", "workflow"]`) so frontends get the canonical spec regardless of invocation form.

#### Commands enumerated

The catalogue covers:

`init`, `ready`, `chat`, `specs` (with `amend`), `status`, `config` (with `show`, `get`, `set`), `exec` (with `prompt`, `workflow`/`wf`), `api` (with `start`, `kill`, `logs`, `status`), `remote` (with `run`, `session start`, `session kill`), `new` (with `spec`, `workflow`, `skill`).

---

### Catalogue Projections (`src/command/dispatch/projections/`)

Frontends never build their own argument parsers or schema documents. Instead they call projection methods on `CommandCatalogue` that derive the frontend-specific structure from the single catalogue definition. Adding a flag is a one-line edit in the catalogue; every projection updates automatically.

#### `clap.rs`

```rust
impl CommandCatalogue {
    pub fn build_clap_command(&self) -> clap::Command;
}
```

Walks the catalogue tree and produces a `clap::Command` with all subcommands, flags, arguments, aliases, help text, `conflicts_with` constraints, and `requires` chains. `ArgumentSpec::TrailingVarArgs` sets `trailing_var_arg(true)` and `allow_hyphen_values(true)` (used by `remote run <COMMAND>...`). The CLI frontend calls this once and passes the resulting `ArgMatches` to a `CliCommandFrontend`.

#### `tui_hints.rs`

```rust
impl CommandCatalogue {
    pub fn tui_hint_for(&self, path: &[&str]) -> Option<TuiHint>;
    pub fn tui_completions(&self, partial: &str) -> Vec<TuiCompletion>;
}
```

Generates the hint string shown above the TUI command box for the currently typed command path, and the autocomplete entries shown as the user types. The TUI frontend never maintains its own hint or completion lists.

#### `api_schema.rs`

```rust
impl CommandCatalogue {
    pub fn openapi_schema(&self) -> serde_json::Value;
    pub fn rest_route_table(&self) -> Vec<RestRoute>;
}
```

Generates the OpenAPI JSON schema and the REST route table used by the API server. The API frontend derives its API surface entirely from these projections.

#### Projection consistency guarantee

A suite of catalogue unit tests (`catalogue_clap_consistency`, `catalogue_tui_consistency`, `catalogue_api_consistency`) walks every `Arg` in the clap output, every hint entry, and every route in the REST table and asserts each is present in the catalogue with a matching kind, default, and help string. If a flag exists in a projection but not the catalogue (or vice versa), the test fails.

---

### `Dispatch` (`src/command/dispatch/mod.rs`)

`Dispatch` is the gateway through which frontends invoke commands. It reads flag values from the frontend, applies catalogue-driven validation, enforces implication rules, and constructs a typed `*Command` struct populated with all the engines and flag values it needs.

#### `Engines` bundle

```rust
#[derive(Clone)]
pub struct Engines {
    pub runtime: Arc<ContainerRuntime>,
    pub git_engine: Arc<GitEngine>,
    pub overlay_engine: Arc<OverlayEngine>,
    pub auth_engine: Arc<AuthEngine>,
    pub agent_engine: Arc<AgentEngine>,
    pub workflow_state_store: Arc<WorkflowStateStore>,
}
```

`ReadyEngine` and `InitEngine` are **not** pre-constructed on `Dispatch` — their constructors accept per-invocation flag values. The corresponding commands construct them fresh from the `Engines` references above.

#### `CommandFrontend` trait

Implemented by Layer 3 (CLI, TUI, API). Supplies flag values and positional arguments to Dispatch, and extends `UserMessageSink` so commands can write status messages through the same frontend object.

```rust
pub trait CommandFrontend: UserMessageSink + Send + Sync {
    fn flag_bool(&self, command_path: &[&str], flag: &str) -> Result<Option<bool>, CommandError>;
    fn flag_string(&self, command_path: &[&str], flag: &str) -> Result<Option<String>, CommandError>;
    fn flag_strings(&self, command_path: &[&str], flag: &str) -> Result<Vec<String>, CommandError>;
    fn flag_path(&self, command_path: &[&str], flag: &str) -> Result<Option<PathBuf>, CommandError>;
    fn flag_enum(&self, command_path: &[&str], flag: &str) -> Result<Option<String>, CommandError>;
    fn flag_u16(&self, command_path: &[&str], flag: &str) -> Result<Option<u16>, CommandError>;
    fn argument(&self, command_path: &[&str], name: &str) -> Result<Option<String>, CommandError>;
    fn arguments(&self, command_path: &[&str], name: &str) -> Result<Vec<String>, CommandError>;
}
```

Validation (type checking, required vs. optional, mutual exclusion, implication) lives entirely in Dispatch — Layer 3 never validates user input.

#### `Dispatch` struct

```rust
pub struct Dispatch<F: CommandFrontend> {
    catalogue: &'static CommandCatalogue,
    frontend: F,
    session: Arc<RwLock<Session>>,
    engines: Engines,
}

impl<F: CommandFrontend> Dispatch<F> {
    pub fn new(frontend: F, session: Arc<RwLock<Session>>, engines: Engines) -> Self;
    pub fn catalogue(&self) -> &'static CommandCatalogue;
    pub fn frontend(&self) -> &F;
    pub async fn run_command(self, path: &[&str]) -> Result<CommandOutcome, CommandError>;
    pub fn build_command(self, path: &[&str]) -> Result<BuiltCommand, CommandError>;
    pub fn parse_command_box_input(raw: &str) -> Result<ParsedCommandBoxInput, CommandError>;
}
```

`build_command` resolves aliases, reads flag values, applies implication rules (e.g. `--yolo` implies `--worktree` for `exec workflow`; `--json` implies `--non-interactive` for `ready`), and constructs the typed `BuiltCommand`. `run_command` calls `build_command` then dispatches to the command's `run_with_frontend`.

`parse_command_box_input` tokenises a raw TUI command-box string against the catalogue and returns a `ParsedCommandBoxInput { path, flags, arguments }`. All command-string interpretation lives here, never in the TUI.

#### `CommandOutcome` and `BuiltCommand`

```rust
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum CommandOutcome {
    Init(InitOutcome),
    Ready(ReadyOutcome),
    Chat(ChatOutcome),
    Status(StatusOutcome),
    Config(ConfigOutcome),
    ExecPrompt(ExecPromptOutcome),
    ExecWorkflow(ExecWorkflowOutcome),
    API(ApiOutcome),
    Remote(RemoteOutcome),
    New(NewOutcome),
    Specs(SpecsOutcome),
    Auth(AuthOutcome),
    Download(DownloadOutcome),
    Empty,
}

pub enum BuiltCommand {
    Init(InitCommand),
    Ready(ReadyCommand),
    Chat(ChatCommand),
    /* … one variant per command … */
}
```

Every `*Outcome` derives `Serialize`. JSON serialisation is a frontend concern (Layer 3 renders the outcome as JSON when `--json` is active); the command itself is unaware of the output format.

---

### Per-Command Structs (`src/command/commands/`)

Each awman command is one module under `src/command/commands/` containing:

- A `*Command` struct that owns every flag value and engine reference it needs.
- A `*CommandFlags` struct carrying the typed flag values.
- A `*CommandFrontend` trait listing the per-command user-input and reporting methods.
- An `impl Command for *Command` block.
- Colocated unit tests using fake engines and a recording frontend.

#### Command roster

| Module | Command(s) | Notes |
|--------|-----------|-------|
| `init.rs` | `awman init` | Thin wrapper over `InitEngine`; `InitCommandFrontend: InitFrontend + Send` |
| `ready.rs` | `awman ready` | Thin wrapper over `ReadyEngine`; `ReadyCommandFrontend: ReadyFrontend + Send`; `--json` implies `--non-interactive` |
| `chat.rs` | `awman chat` | Agent-launching command |
| `exec_prompt.rs` | `awman exec prompt` | Agent-launching command with inline prompt |
| `exec_workflow.rs` | `awman exec workflow` | Agent-launching command with full workflow file; `--yolo`/`--auto` imply `--worktree` |
| `status.rs` | `awman status` | Accepts optional `StatusCommandTuiContext` for tab annotations; `--watch` for continuous refresh |
| `specs.rs` | `awman specs amend` | Review/amend agent runs; shares `create_new_spec` with `new spec` |
| `config.rs` | `awman config {show,get,set}` | Config read/write; `config set --global` writes to global config |
| `api.rs` | `awman api {start,kill,logs,status}` | Daemonization, PID management, workdir allowlist; delegates HTTP server boot to Layer 3 frontend |
| `remote.rs` | `awman remote {run, session start, session kill}` | Uses `RemoteClient` for HTTP + SSE |
| `new.rs` | `awman new {spec,workflow,skill}` | Work-item and artefact creation |
| `auth.rs` | `awman auth` | Keychain credential accept/decline per-repo |
| `download.rs` | `awman download` | Internal helper for Dockerfile downloads |

#### Agent-launching command canonical order

Every command that launches an agent (`chat`, `exec prompt`, `exec workflow`, `specs amend`, `new spec --interview`, `init` audit, `ready` audit) follows this sequence in `run_with_frontend`:

1. Resolve mount path via `MountScope::resolve`.
2. Resolve effective agent + model (flags > repo config > global config).
3. If agent is not available: call `AgentSetupFrontend::ask_agent_setup`. On `Setup` → `AgentEngine::ensure_available`. On `FallbackToDefault` → swap agent. On `Abort` → `CommandError::Aborted`.
4. Check `EffectiveConfig::auto_agent_auth_accepted`: if `None`, call `AgentAuthFrontend::ask_agent_auth_consent`; persist the result.
5. If `--worktree`: call `WorktreeLifecycle::prepare(frontend)` → use the returned worktree path as the mount root.
6. Build `ContainerOption` list via `AgentEngine::build_options`; run via `WorkflowEngine` or `ContainerRuntime`.
7. If worktree was used: call `WorktreeLifecycle::finalize(frontend, had_error)`.
8. Map exit info to `*Outcome`.

---

### `WorktreeLifecycle` (`src/command/commands/worktree_lifecycle.rs`)

Worktree lifecycle is a command-layer concern, not a `WorkflowEngine` concern. `WorkflowEngine` operates on a given directory and is unaware of whether it is a worktree.

#### Decision types

```rust
pub enum PreWorktreeDecision {
    Commit { message: String },
    UseLastCommit,
    Abort,
}

pub enum ExistingWorktreeDecision { Resume, Recreate }

pub enum PostWorkflowWorktreeAction { Merge, Discard, Keep }

pub enum WorktreeMergeMode { Merge /* no squash */, Squash, LeaveBranch }
```

#### `WorktreeLifecycleFrontend` trait

Defined by Layer 2, implemented by Layer 3:

```rust
pub trait WorktreeLifecycleFrontend: UserMessageSink + Send + Sync {
    fn ask_pre_worktree_uncommitted_files(&mut self, files: &[String]) -> Result<PreWorktreeDecision, CommandError>;
    fn ask_existing_worktree(&mut self, path: &Path, branch: &str) -> Result<ExistingWorktreeDecision, CommandError>;
    fn report_worktree_created(&mut self, path: &Path, branch: &str);
    fn ask_post_workflow_action(&mut self, branch: &str, had_error: bool) -> Result<PostWorkflowWorktreeAction, CommandError>;
    fn ask_worktree_commit_before_merge(&mut self, branch: &str, files: &[String]) -> Result<Option<String>, CommandError>;
    fn ask_merge_mode(&mut self, branch: &str) -> Result<WorktreeMergeMode, CommandError>;
    fn confirm_worktree_cleanup(&mut self, branch: &str, path: &Path) -> Result<bool, CommandError>;
    fn report_merge_conflict(&mut self, branch: &str, worktree_path: &Path, git_root: &Path);
    fn report_worktree_discarded(&mut self, branch: &str);
    fn report_worktree_kept(&mut self, path: &Path, branch: &str);
}
```

#### `WorktreeLifecycle` struct

```rust
pub struct WorktreeLifecycle {
    git_engine: Arc<GitEngine>,
    git_root: PathBuf,
    worktree_path: PathBuf,
    branch: String,
}

impl WorktreeLifecycle {
    /// Branch name: `awman/workflow-<name>`; path: `~/.awman/worktrees/<repo>/wf-<name>/`
    pub fn for_workflow(git_engine: Arc<GitEngine>, git_root: PathBuf, workflow_name: &str) -> Self;

    pub fn worktree_path(&self) -> &Path;
    pub fn branch(&self) -> &str;

    /// Pre-creation checks and worktree setup. Returns the worktree path (= mount root).
    pub async fn prepare(&self, frontend: &mut dyn WorktreeLifecycleFrontend) -> Result<PathBuf, CommandError>;

    /// Post-completion merge / discard / keep flow.
    pub async fn finalize(&self, frontend: &mut dyn WorktreeLifecycleFrontend, had_error: bool) -> Result<(), CommandError>;
}
```

`prepare` steps: check for existing worktree → if exists call `ask_existing_worktree` (Resume or Recreate); check for uncommitted files → if present call `ask_pre_worktree_uncommitted_files`; create worktree; report.

`finalize` steps: call `ask_post_workflow_action`; on Merge → `ask_merge_mode` (plain merge / squash / leave branch alone) → optional commit of uncommitted files (runs for every mode, including LeaveBranch) → merge per mode → optional cleanup; on Discard → remove worktree + branch; on Keep → report.

Merge conflicts are non-fatal: `finalize` catches `EngineError::MergeConflict`, calls `report_merge_conflict`, and returns `Ok(())`. The user resolves the conflict manually.

This module is `pub(super)` within `src/command/commands/` — not re-exported from `src/command/mod.rs`.

---

### `MountScope` (`src/command/commands/mount_scope.rs`)

When the process `cwd` differs from the git root, every agent-launching command must ask the user which directory to mount into the container.

```rust
pub enum MountScopeDecision { MountGitRoot, MountCurrentDirOnly, Abort }

pub trait MountScopeFrontend: UserMessageSink + Send + Sync {
    fn ask_mount_scope(&mut self, git_root: &Path, cwd: &Path) -> Result<MountScopeDecision, CommandError>;
}

pub struct MountScope;

impl MountScope {
    /// Returns `git_root` when `cwd == git_root`; otherwise calls `ask_mount_scope`.
    pub fn resolve(cwd: &Path, git_root: &Path, frontend: &mut dyn MountScopeFrontend) -> Result<PathBuf, CommandError>;
}
```

Default behaviors per frontend (implemented by Layer 3): CLI prompts with `[r]oot / [c]urrent dir / [a]bort`; TUI shows the `MountScope` modal dialog; API returns `MountGitRoot` unless the request body specifies `mount_scope: "cwd"`.

Every agent-launching command frontend trait adds `MountScopeFrontend` as a supertrait bound.

---

### `AgentSetupFrontend` (`src/command/commands/agent_setup.rs`)

When `AgentEngine::ensure_available` would download or build (the agent is not yet ready), Layer 2 commands interpose a user decision before calling the engine. `AgentEngine` reports state; the choice belongs to the command layer.

```rust
pub enum AgentSetupDecision { Setup, FallbackToDefault, Abort }

pub trait AgentSetupFrontend: UserMessageSink + Send + Sync {
    fn ask_agent_setup(
        &mut self,
        requested: &AgentName,
        default: &AgentName,
        default_available: bool,
        image_only: bool,  // true = Dockerfile exists, only image build needed
    ) -> Result<AgentSetupDecision, CommandError>;

    fn record_fallback(&mut self, requested: &AgentName, fallback: &AgentName);
}
```

Per-step / per-tab caching of fallback decisions (`workflow_agent_fallbacks`) lives in the `ExecWorkflowCommand` body, not in the engine. The command consults its own cache before calling `ask_agent_setup`.

Added as a supertrait bound on every agent-launching command frontend trait.

---

### `AgentAuthFrontend` (`src/command/commands/agent_auth.rs`)

On first run (`auto_agent_auth_accepted: None` in repo config), Layer 2 commands prompt the user before silently injecting keychain credentials into containers.

```rust
pub enum AgentAuthDecision { Accept, Decline, DeclineOnce }

pub trait AgentAuthFrontend: UserMessageSink + Send + Sync {
    fn ask_agent_auth_consent(
        &mut self,
        agent: &AgentName,
        env_var_names: &[&str],
    ) -> Result<AgentAuthDecision, CommandError>;
}
```

Decision handling by commands:
- `Some(true)` → silently inject credentials via `AgentEngine::resolve_agent_auth`.
- `Some(false)` → do not inject (no prompt).
- `None` → call `ask_agent_auth_consent`. On `Accept`/`Decline`, persist via `RepoConfig::update` **before** the agent container launches. `DeclineOnce` does not persist.

---

### `RemoteClient` (`src/command/commands/remote_client.rs`)

A typed HTTP client for communicating with a remote awman api server. Constructed fresh per `RemoteCommand` invocation; not exported beyond `src/command/commands/`.

```rust
pub struct RemoteClient {
    base_url: String,
    http: reqwest::Client,
}

pub struct RemoteResponse {
    pub status: u16,
    pub body: serde_json::Value,
}

pub trait RemoteEventSink: Send + Sync {
    fn on_event(&mut self, event_type: &str, data: &str);
    fn on_done(&mut self);
}

impl RemoteClient {
    pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    pub const READ_TIMEOUT: Duration = Duration::from_secs(600);

    pub fn new(base_url: &str, api_key: Option<&ApiKey>) -> Result<Self, CommandError>;

    /// Resolution order: explicit arg > AWMAN_API_KEY env > GlobalConfig::remote.default_api_key
    /// (only when target_addr matches GlobalConfig::remote.default_addr after URL canonicalization).
    /// Returns None when no key is available (server may have --dangerously-skip-auth).
    pub fn resolve_api_key(session: &Session, target_addr: &str, explicit: Option<&str>) -> Result<Option<ApiKey>, CommandError>;

    pub async fn send_command(&self, path: &[&str], flags: &[(&str, serde_json::Value)]) -> Result<RemoteResponse, CommandError>;
    pub async fn stream_command(&self, path: &[&str], flags: &[(&str, serde_json::Value)], sink: &mut dyn RemoteEventSink) -> Result<(), CommandError>;
}
```

The API key sent as `Authorization: Bearer <key>` on every request. `stream_command` disables the read timeout (or uses a generous value) so SSE streams don't hit the 600 s ceiling. URL canonicalization for the `resolve_api_key` config match normalises scheme case, hostname case, default-port elision, and trailing slash.

All HTTP error variants map to specific `CommandError` variants: timeout → `RemoteTimeout`; connection refused → `RemoteConnectionRefused`; non-2xx → `RemoteHttpStatus`; malformed SSE → `MalformedSseEvent`; transport error → `RemoteTransport`.

---

### `StatusCommand` — TUI tab annotations

`StatusCommand` accepts an optional `StatusCommandTuiContext` populated by the TUI before invocation:

```rust
pub struct StatusCommandTuiContext {
    pub tabs: Vec<TuiTabSnapshot>,
}

pub struct TuiTabSnapshot {
    pub tab_number: u32,
    pub container_name: Option<String>,
    pub is_stuck: bool,
    pub command_label: String,
}
```

In CLI and API mode the context is `None` and the status output contains no tab annotation columns. In TUI mode the frontend provides the context via `StatusCommandFrontend::tui_context()`.

---

### `APILifecycle` — server process management

The API server process lifecycle (PID files, daemonization, log rotation, SIGTERM) is encapsulated in `APILifecycle` in `src/engine/API/` (Layer 1, introduced alongside work item 0068):

```rust
pub struct APILifecycle { paths: ApiPaths }

impl APILifecycle {
    pub fn new(session: &Session) -> Self;
    pub fn current_pid(&self) -> Result<Option<u32>, CommandError>;
    pub fn write_pid(&self) -> Result<(), CommandError>;
    pub fn clear_pid(&self) -> Result<(), CommandError>;
    pub async fn kill(&self, timeout: Duration) -> Result<KillOutcome, CommandError>;
    pub fn daemonize(&self, args: &[OsString]) -> Result<u32, CommandError>;
    pub fn open_log_for_append(&self) -> Result<File, CommandError>;
}

pub enum KillOutcome { ExitedCleanly, ExitedAfterSigKill, NotRunning }
```

`APIStartCommand` (Layer 2) uses this lifecycle to: refuse if already running; generate/refresh the API key; optionally daemonize; write the PID file; hand the assembled `ApiServeConfig` to the frontend (which boots the actual HTTP server in Layer 3).

The `--workdirs` flag is merged with `GlobalConfig::API.work_dirs`, canonicalized via `OverlayPathResolver`, deduplicated, and validated (non-existent paths → `CommandError::ApiWorkdirNotFound`).

---

### What is forbidden in Layer 2

- No `eprintln!`, `println!`, or direct console I/O. All status messages flow through `UserMessageSink::write_message`; all structured output flows through per-command `report_*` frontend trait methods.
- No `clap::ArgMatches` references inside `*Command` bodies. Flag values arrive as typed fields in `*CommandFlags`, populated by Dispatch.
- No `crossterm`, no `ratatui`, no `axum`. Those are Layer 3.
- No "if this is CLI vs TUI vs API" checks. Commands never know which frontend is on the other side of the trait object.
- No git worktree calls (`create_worktree`, `merge_branch`, `remove_worktree`) directly from command bodies. All worktree operations must flow through `WorktreeLifecycle::prepare` and `WorktreeLifecycle::finalize`.
- No business logic in projections. Projections derive structure from the catalogue; they do not interpret flag semantics.
- No upward calls into Layer 3 or Layer 4 types.

---

## Layer 3: Frontend (`src/frontend/`)

Layer 3 is the presentation layer. It has three sub-modules — `cli`, `tui`, and `api` — each of which translates user input into `Dispatch` calls and renders the typed outcomes back to the user. Frontends contain **no business logic**: any behavioral decision lives in Layer 2 (`command`) or below.

Layer 3 is the only layer that may:

- Read from and write to terminal I/O (stdout, stderr, stdin)
- Allocate PTYs or open raw-mode terminal sessions
- Bind HTTP server sockets (API mode)
- Render Ratatui widgets (TUI mode)

Layer 3 may call into Layer 0 (`data`), Layer 1 (`engine`), and Layer 2 (`command`), but **never into Layer 4** (no upward calls).

---

### `src/frontend/mod.rs`

Declares the three sub-modules: `pub mod cli; pub mod API; pub mod tui;`. All public symbols used by `main.rs` are re-exported from here.

---

### CLI Frontend (`src/frontend/cli/`)

The CLI frontend is the fully implemented Layer 3 sub-module for `awman <subcommand>` invocations. Its entry point is `run(matches, ctx)`. It extracts the command path from clap's `ArgMatches`, constructs a `CliFrontend`, hands it to `Dispatch`, and renders the resulting `CommandOutcome` or `CommandError` to stdout/stderr.

#### `RuntimeContext` (`mod.rs`)

```rust
pub struct RuntimeContext {
    pub session: Arc<RwLock<Session>>,
    pub engines: Engines,
}
```

The bundle that `main.rs` constructs once at startup and passes to either `cli::run` or `tui::run`. Contains the current `Session` (wrapped for shared ownership) and all six engine handles. Constructed via `RuntimeContext::new(session, engines)`.

#### Entry point (`mod.rs`)

```rust
pub async fn run(matches: ArgMatches, ctx: RuntimeContext) -> ExitCode
```

Extracts the command path via `command_path_from_matches`, builds a `CliFrontend`, creates a `Dispatch`, calls `dispatch.run_command(&path)`, and routes the result to `render_outcome` or `render_error`. The function body is intentionally small — all behavioral decisions live in Layer 2.

```rust
fn render_outcome(outcome: &CommandOutcome) -> ExitCode
fn render_error(err: &CommandError) -> ExitCode
pub(crate) fn error_exit_code(err: &CommandError) -> u8
```

`render_outcome` pattern-matches on typed outcome variants and writes to stdout; every variant has a dedicated human-readable rendering completed in work item 0070. `render_error` writes the error message to stderr. `error_exit_code` is the pure mapping factored out for unit testing:

| Error category | Exit code |
|----------------|-----------|
| `Aborted` | 130 |
| Usage errors (`UnknownCommand`, `UnknownFlag`, `MissingRequiredFlag`, `MissingRequiredArgument`, `MutuallyExclusive`, `InvalidFlagValue`, `InvalidArgumentValue`, `CommandBoxParse`) | 2 |
| All other errors | 1 |

#### `CliFrontend` (`command_frontend.rs`)

```rust
pub struct CliFrontend {
    matches: ArgMatches,
    command_path: Vec<String>,
    messages: CliUserMessageQueue,
}
```

The single CLI frontend struct. Implements `CommandFrontend` (flag extraction from `ArgMatches`), `UserMessageSink` (via the message queue), and every `*CommandFrontend` trait — either as marker impls (`AuthCommandFrontend`, `ConfigCommandFrontend`, `DownloadCommandFrontend`, `NewCommandFrontend`, `RemoteCommandFrontend`, `SpecsCommandFrontend`, `ApiCommandFrontend`, `StatusCommandFrontend`) or via richer per-command modules.

`CliFrontend::new(matches)` pre-computes `command_path` so it doesn't re-traverse the matches tree on every call.

**`CommandFrontend` flag methods:**

| Method | clap equivalent | Notes |
|--------|----------------|-------|
| `flag_bool(path, flag)` | `get_flag(flag)` | Returns `Some(false)` for known Bool flags absent from argv; `None` for unknown paths |
| `flag_string(path, flag)` | `get_one::<String>(flag)` | Returns `None` when absent |
| `flag_strings(path, flag)` | `get_many::<String>(flag)` | Returns empty `Vec` when absent |
| `flag_path(path, flag)` | `get_one::<String>(flag)` then `PathBuf::from` | Returns `None` when absent |
| `flag_enum(path, flag)` | delegates to `flag_string` | Enum flags are stored as strings in the clap projection |
| `flag_u16(path, flag)` | `get_one::<u16>(flag)` | Used for `--port` on `api start` |
| `argument(path, name)` | `get_one::<String>(name)` or `get_many` joined | `TrailingVarArgs` arguments are joined with spaces |
| `arguments(path, name)` | `get_many::<String>(name)` | Returns the raw token vector |

`matches_for(path)` resolves the correct `ArgMatches` sub-tree for nested subcommands by walking the clap matches tree one segment at a time.

**`command_path_from_matches(matches) -> Vec<String>`** (exported):

Walks `ArgMatches::subcommand()` recursively and collects the subcommand names into a path vector. The resulting vector is what `Dispatch::run_command` consumes. A bare invocation returns an empty vector.

#### Output helpers (`output.rs`)

```rust
pub fn stderr_is_tty() -> bool
pub fn stdin_is_tty() -> bool
```

Pure TTY-detection helpers used by per-command frontends to decide whether to apply ANSI color codes and whether to fall back to safe defaults when stdin is not a TTY (e.g., piped). No business logic — the _decision_ of what to do with the detection result lives in the per-command module.

#### Message queue (`user_message.rs`)

```rust
pub struct CliUserMessageQueue {
    pty_active: bool,
    queue: Vec<UserMessage>,
}
```

Implements `UserMessageSink`. The `pty_active` flag controls two modes:

- **`pty_active = false`** (default): `write_message` writes immediately to stderr with a level-prefixed format (`awman:`, `awman warning:`, `awman error:`).
- **`pty_active = true`**: `write_message` pushes to the queue instead. Used when a PTY-bound container owns the terminal — messages accumulated during container execution are replayed after the container exits via `replay_queued`.

`replay_queued` drains the queue to stderr in insertion order and clears it. `set_pty_active(bool)` toggles the mode; the per-command frontends for container-running commands call this before and after `ContainerExecution::wait`.

#### Per-command modules (`per_command/`)

Each module in this directory implements the richer `*CommandFrontend` trait (and related engine frontend traits) for commands that require more than just flag extraction:

| Module | Traits implemented | Key behavior |
|--------|--------------------|-------------|
| `chat.rs` | `ChatCommandFrontend` | Marker (no extra methods beyond `UserMessageSink`) |
| `exec_prompt.rs` | `ExecPromptCommandFrontend` | Marker |
| `exec_workflow.rs` | `ExecWorkflowCommandFrontend`, `ContainerFrontend`, `WorkflowFrontend` | Integrates container output, workflow control, and worktree lifecycle for the exec-workflow command path |
| `api.rs` | `ApiStartCommandFrontend` | Calls `crate::frontend::api::serve(config)` — a peer Layer 3 call, not an upward call |
| `init.rs` | `InitCommandFrontend`, `InitFrontend` | Reports `InitPhase` transitions to stderr; prompts on stdin for aspec replacement, audit, and work-items config |
| `ready.rs` | `ReadyCommandFrontend`, `ReadyFrontend` | Reports `ReadyPhase` transitions to stderr; prompts for Dockerfile creation and legacy-migration decisions |
| `agent_auth.rs` | `AgentAuthFrontend` | Asks auth consent on stdin; defaults to `DeclineOnce` when stdin is not a TTY |
| `agent_setup.rs` | `AgentSetupFrontend` | Asks agent setup decision on stdin; defaults to `Setup` when stdin is not a TTY |
| `container_frontend_marker.rs` | `ContainerFrontend` | Shared marker impl for commands that don't use a PTY container |
| `mount_scope.rs` | `MountScopeFrontend` | Asks mount scope on stdin; defaults to `MountGitRoot` when stdin is not a TTY |
| `workflow_frontend_marker.rs` | `WorkflowFrontend` | Shared marker impl for commands that don't use workflows |
| `worktree_lifecycle_marker.rs` | `WorktreeLifecycleFrontend` | Shared marker impl for commands that don't use worktrees |

The **safe default policy** (applied when `stdin_is_tty()` returns `false`) matches the API defaults from WI 0069 §7u: interactive prompts return the non-destructive option rather than blocking.

---

### TUI Frontend (`src/frontend/tui/`)

The TUI frontend is the Ratatui-based interactive terminal UI invoked by bare `awman` (no subcommand). It is a pure presentation layer: it translates keystrokes into `Dispatch` calls and renders typed outcomes via Ratatui widgets. No business logic lives here — any behavioral decision belongs in Layer 2.

#### Entry point (`mod.rs`)

```rust
pub async fn run(_matches: clap::ArgMatches, ctx: RuntimeContext) -> ExitCode
```

`run` constructs an in-memory `SessionManager`, opens an initial `Tab` bound to the working directory session, creates an `App`, and enters the terminal event loop. Terminal cleanup (raw mode off, alternate screen off, mouse capture off) runs unconditionally on exit, even on error.

**Startup branching:** After the initial tab is open, `run` dispatches a startup command through the standard `Dispatch` → `Command` → `Frontend` chain before entering the event loop:

- **Inside a Git repository:** dispatches `["ready"]` through `TuiReadyFrontend`. This checks that the container runtime, Dockerfiles, and agent images are present. Phase transitions render as an in-place progress dialog.
- **Not inside a Git repository:** dispatches `["status", "--watch"]` so the TUI immediately shows a live status stream.

No startup logic is special-cased in `App::new`; both branches go through the normal `Dispatch::run_command` path.

`run_event_loop` sets up the Crossterm backend and drives `main_loop`. The main loop renders on every iteration, polls for input events with a 50 ms timeout, and dispatches key events through the keymap.

#### Application state (`app.rs`)

```rust
pub struct App {
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    pub active_dialog: Option<Dialog>,
    pub focus: Focus,
    pub catalogue: &'static CommandCatalogue,
    pub engines: Engines,
    pub session_manager: Arc<RwLock<SessionManager>>,
    pub command_input: TextEdit,
    pub suggestion_row: Vec<String>,
    pub input_error: Option<String>,
    pub status_bar: StatusBar,
    pub should_quit: bool,
    pub needs_redraw: bool,
}
```

`App` is the single shared mutable state object. It stores only UI state; commands are dispatched through `Dispatch` and results flow back through the per-command frontend trait chain.

Key methods:

| Method | Description |
|--------|-------------|
| `active_tab()` / `active_tab_mut()` | Borrow the current tab |
| `switch_to_prev_tab()` / `switch_to_next_tab()` | Wrap-around tab switching |
| `close_active_tab()` | Remove tab; set `should_quit` if only one tab remains |
| `update_suggestions()` | Refresh `suggestion_row` from `CommandCatalogue::tui_completions(partial)` |

`Focus` enum has two variants: `CommandBox` and `ExecutionWindow`.

#### Per-tab state (`tabs.rs`)

```rust
pub struct Tab {
    pub session: Session,           // Layer 0 session for this tab
    pub execution_phase: ExecutionPhase,
    pub vt100_parser: vt100::Parser,          // 10000-line scrollback
    pub container_window_state: ContainerWindowState,
    pub workflow_state: Option<WorkflowViewState>,
    pub status_log: SharedStatusLog,          // Arc<Mutex<Vec<StatusLogEntry>>>
    pub status_log_collapsed: bool,
    pub scroll_offset: usize,
    pub mouse_selection: Option<TextSelection>,
    pub workflow_agent_fallbacks: HashMap<String, String>,
    pub auto_workflow_disabled_steps: HashSet<String>,
    pub is_remote: bool,
    pub output_lines: Vec<String>,
    pub stuck: bool,
    pub yolo_countdown: Option<u64>,
    pub last_output_time: Option<std::time::Instant>,
}
```

**`ExecutionPhase`** drives border colour and title:

| Variant | Phase label | Border (focused) |
|---------|-------------|-----------------|
| `Idle` | ` awman ` | DarkGray |
| `Running { command }` | ` ● running: {command} ` | Blue |
| `Done { command, exit_code: 0 }` | ` ✓ done: {command} ` | Green (focused) / Gray |
| `Done { command, exit_code: n }` | ` ✗ error: {command} (exit N) ` | Green (focused) / Gray |
| `Error { command, .. }` | ` ✗ error: {command} ` | Red |

**`ContainerWindowState`** cycles Hidden → Minimized → Maximized → Hidden via `Ctrl+M`.

**Pure functions** in `tabs.rs` — safe to unit-test without a terminal:

| Function | Purpose |
|----------|---------|
| `tab_color(tab)` | Stuck→Yellow, Remote→Magenta, Error→Red, Running+PTY→Green, Running→Blue, Idle/Done→DarkGray |
| `window_border_color(phase, focused)` | Maps phase + focus to a Ratatui `Color` |
| `phase_label(phase)` | Phase label string for the execution window border title |
| `compute_tab_bar_width(n, width)` | 1 tab → ¼ width; 2 → ½; 3 → ¾/3; N → 1/N |

#### Keyboard shortcuts (`keymap.rs`)

Every key binding is defined in one place. `map_key(key, ctx) -> Action` is pure: no state mutation, no side effects.

**`FocusContext`** determines which bindings are active:

| Context | When active |
|---------|-------------|
| `CommandBox` | No dialog, no maximized container, focus on command box |
| `ExecutionWindow` | No dialog, no maximized container, focus on execution window |
| `Dialog` | A dialog is open |
| `ContainerMaximized` | Container window is in Maximized state |

Global shortcuts (available in all contexts except `ContainerMaximized`):

| Key | Action |
|-----|--------|
| `Ctrl+T` | `OpenNewTabDialog` |
| `Ctrl+A` | `PreviousTab` |
| `Ctrl+D` | `NextTab` |
| `Ctrl+C` | `CloseTabOrQuit` |
| `Ctrl+M` | `CycleContainerWindow` |
| `Ctrl+,` | `OpenConfigShow` |

`ContainerMaximized` context: all keys except `Ctrl+Y` (copy) and `Ctrl+M` (toggle) are forwarded to the PTY as `Action::ForwardToPty(key)`. Global shortcuts are suppressed.

#### Command box (`command_box.rs`)

`parse_input(text)` tokenizes the raw command-box string by calling `Dispatch::parse_command_box_input`. Returns `Ok(ParsedCommandBoxInput)` or a `CommandError`.

`format_parse_error(err)` converts a `CommandError` into a user-visible string:
- `UnknownCommand` with a close match (Levenshtein ≤ 4): `"did you mean: <suggestion>?"`
- `UnknownCommand` with no close match: `"unknown command: <name>"`
- `UnknownFlag`: `"unknown flag: --<name>"`
- `CommandBoxParse`: the error message verbatim

#### Hints and suggestions (`hints.rs`)

`hint_for_input(input)` returns a one-line inline hint for the command currently being typed, by delegating to `CommandCatalogue::tui_hint_for`. No command names or flag names are hard-coded here.

`format_suggestion_row(suggestions)` formats the suggestion list as:
```
> chat · exec · implement · ready · …
```
Suggestions are separated by middots (` · `). An empty list produces an empty string.

#### Dialog system (`dialogs/mod.rs`)

The `Dialog` enum holds the state for every modal overlay. One dialog is active at a time (`App::active_dialog: Option<Dialog>`). Dialogs are pure presentation: they render centered on the terminal frame and map key presses to typed Layer 2 enum values.

Available dialog variants:

| Variant | Purpose |
|---------|---------|
| `QuitConfirm` | Quit confirmation: `[y]` quits, `[n]`/Esc cancels |
| `CloseTabConfirm` | Multi-tab close: `[q]` quits app, `[c]` closes tab, `[n]`/Esc cancels |
| `YesNo { title, body }` | Generic yes/no prompt |
| `YesNoCancel { title, body }` | Generic yes/no/cancel prompt |
| `TextInput { title, prompt, editor }` | Single-line text input |
| `MultilineInput { title, prompt, editor }` | Multiline text input; Ctrl+Enter submits |
| `ListPicker { title, items, selected }` | Arrow-key selection list; Enter selects |
| `KindSelect { title, options }` | Numbered option select |
| `WorkflowControlBoard(..)` | Workflow step navigation (→ ← ↑ ↓ d Ctrl+Enter Ctrl+C Esc) |
| `WorkflowStepError(..)` | Step failure prompt: `[r]`/`[1]` retry, `[q]`/`[2]`/Esc pause, `[a]` abort |
| `WorkflowYoloCountdown(..)` | Yolo countdown display; Esc dismisses |
| `AgentSetup(..)` | Agent build/setup confirmation |
| `MountScope(..)` | Git root vs CWD mount selection |
| `AgentAuth(..)` | Agent credential injection consent |
| `ConfigShow(..)` | Full-screen config editor table |
| `Loading { title }` | "loading…" placeholder during async data fetch |
| `Custom { title, body, keys }` | Ad-hoc dialog with arbitrary key/label pairs |

`DialogRequest` and `DialogResponse` are the channel types for async communication between the command thread and the event loop.

#### Per-command frontend traits (`per_command/`)

Each file implements the `*CommandFrontend` trait for one command, opening the appropriate `Dialog` variant for each interactive Q&A method. The pattern:

1. The command (Layer 2) calls a trait method (e.g. `ask_agent_setup(decision_info)`)
2. The TUI implementation sends a `DialogRequest` to the event loop
3. The event loop renders the dialog and waits for a `DialogResponse`
4. The TUI implementation maps the response to the typed Layer 2 enum and returns it

Commands with no interactive methods use marker impls that delegate to `UserMessageSink` only.

#### PTY management (`pty.rs`)

`PtySession` wraps `portable-pty` to provide interactive shell access inside container windows. Background threads handle read (PTY → channel), exit-wait, and write (keystrokes → PTY).

`PtyEvent` enum: `Data(Vec<u8>)`, `Exit(i32)`.

`spawn_text_command()` runs non-PTY commands (init, ready) as async tasks piping stdout/stderr to the vt100 parser as plain text.

#### UI rendering (`render.rs`)

`render_frame(app, frame)` lays out the full terminal area top-to-bottom:

| Slot | Height | Content |
|------|--------|---------|
| Tab bar | 3 rows | Colored tabs with project name and command label |
| Execution window | fills remaining (min 5) | Status log or PTY output; border color by phase |
| Minimized container bar | 3 rows (conditional) | One-line PTY summary |
| Workflow strip | 3 rows (conditional) | Step status boxes |
| Status bar | 1 row | Git root path; optional status text |
| Command box | 3 rows | Text input with inline hint |
| Suggestion row | 1 row | `> sugg1 · sugg2 · …` |

Container overlay (Maximized) and active dialogs are rendered as floating layers on top of the base layout.

**Welcome message** (Idle phase, no output): two dark-gray lines:
```
Welcome to awman.
Running 'awman ready' to check your environment...
```

#### Text editing widget (`text_edit.rs`)

`TextEdit` is the shared single-line/multiline text editing primitive used by the command box and dialog text inputs.

| Key | Action |
|-----|--------|
| `←` / `→` | Move cursor |
| `Ctrl+←` / `Ctrl+→` | Move by word |
| `Home` / `End` | Move to line start/end |
| `Backspace` | Delete previous character |
| `Ctrl+Backspace` | Delete previous word |
| `Delete` | Delete next character |
| `Ctrl+Enter` or `Shift+Enter` | Insert newline (multiline mode) |

#### Message sink (`user_message.rs`)

`TuiUserMessageSink` implements `UserMessageSink` by appending to the active tab's `SharedStatusLog` with level-colored prefixes:

| Level | Color |
|-------|-------|
| Info | DarkGray |
| Warning | Yellow |
| Error | Red |
| Success | Green |

`SharedStatusLog` is `Arc<Mutex<Vec<StatusLogEntry>>>`. The status log is collapsed by default (shows only the most recent entry); press `l` in the execution window to toggle expanded view.

---

### API Frontend (`src/frontend/API/`)

The API frontend is a full HTTP server (Axum + axum-server with optional rustls TLS) that dispatches commands through `Dispatch::run_command` rather than spawning child `awman` processes. It was completed in WI 0072 and is exercised end-to-end by `tests/api_parity/`.

The HTTP routes are defined in `src/frontend/api/routes.rs`; the per-command frontends live alongside in `per_command/`. Sessions and commands are persisted to SQLite via `SqliteSessionStore` (`src/data/fs/api_db.rs`).

`ApiServeConfig` is the configuration type that the CLI's `ApiStartCommandFrontend` impl populates and passes into `serve`:

```rust
pub struct ApiServeConfig {
    pub port: u16,
    pub workdirs: Vec<PathBuf>,
    pub dangerously_skip_auth: bool,
}
```

The `serve(config)` function signature is the public contract that WI 0072 must preserve:

```rust
pub async fn serve(config: ApiServeConfig) -> Result<(), CommandError>
```

---

## Layer 4: Binary (`src/main.rs`)

`main.rs` is the Layer 4 binary entrypoint. It contains no business logic: its sole responsibility is to construct the runtime context and route to the appropriate frontend.

### Startup sequence

1. **Build clap**: `CommandCatalogue::get().build_clap_command()` — the clap command is derived entirely from the catalogue; `main.rs` does not hard-code any subcommand or flag name.
2. **Parse argv**: `clap_cmd.get_matches()` — clap handles `--help`, `--version`, and error formatting.
3. **Load global config**: `GlobalConfig::load()` — used to select the container runtime.
4. **Construct engines**:
   - `ContainerRuntime::detect(&global_config)` — selects Docker or Apple Containers
   - `GitEngine::new()` — used to resolve the git root
   - `Session::open(working_dir, &git_engine, SessionOpenOptions::default())` — resolves git root, loads per-repo and global config, records timestamps
   - `OverlayEngine::new(&session)` — resolves overlay paths from config
   - `AuthEngine::new(&session)` — sets up the keychain credential path
   - `AgentEngine::new(overlay_engine, runtime)` — wraps the overlay and runtime for agent execution
   - `EngineWorkflowStateStore::at_git_root(session.git_root())` — filesystem workflow state store
5. **Construct `RuntimeContext`**: `RuntimeContext::new(session, engines)` — wraps the session in `Arc<RwLock<Session>>`.
6. **Route**: `matches.subcommand_name().is_some()` → `cli::run(matches, ctx)` (CLI); otherwise → `tui::run(matches, ctx)` (TUI).

### Routing rule

```rust
if matches.subcommand_name().is_some() {
    cli::run(matches, ctx).await
} else {
    tui::run(matches, ctx).await
}
```

The API server is launched by the `api start` *command* (Layer 2 → Layer 3), not by `main.rs`. `main.rs` never branches on `api`.

### Size constraint

Per the architecture tenet, the `main.rs` function body must remain small (under ~100 lines). Any logic that wants to live in `main.rs` belongs in Layer 2 or below. This is enforced by code review, not by the compiler.

### `#![forbid(unsafe_code)]`

The binary crate opts out of all unsafe code at the crate level. Layer 3 and Layer 4 are entirely safe Rust.

---

[← Runtimes](12-runtimes.md) · [Contents](contents.md)
