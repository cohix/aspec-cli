# Work Item: Refactor

Title: `AgentRuntimeEngine` Abstraction — split runtime tier into `ContainerRuntime` and `SandboxRuntime`

## Summary

Introduce a Layer 1 trait family — `AgentRuntimeEngine` — that abstracts over the two paradigms of agent-isolation runtime awman now needs to support: **container-class** (the existing Docker and Apple Containers backends) and **sandbox-class** (microVM-per-session runtimes like Docker Sandboxes, and likely future entries). The current `ContainerRuntime` factory is refactored into one of two concrete `AgentRuntimeEngine` impls; a new `SandboxRuntime` is the other. Each runtime defines its own backend trait (`ContainerBackend` and `SandboxBackend` respectively) and houses paradigm-appropriate concrete drivers.

This work item delivers the abstraction with a **stubbed** sandbox driver (`DSbxBackend`). The stub is wired through detection — the config string `runtime: "docker-sbx-experimental"` routes correctly to `SandboxRuntime` + `DSbxBackend` — but every `SandboxBackend` method returns `EngineError::NotImplemented`. The actual sbx implementation lands in WI 0090.

The naming hierarchy is deliberate:

- **`AgentRuntimeEngine`** (Layer 1, `src/engine/agent_runtime/`) — defines the trait family that the rest of the engine and all of Layer 2 see. "Engine" because this is where the cross-paradigm logic and trait surface live.
- **`ContainerRuntime`** and **`SandboxRuntime`** (Layer 1, `src/engine/container/runtime.rs` and `src/engine/sandbox/runtime.rs`) — concrete types that implement the `AgentRuntimeEngine` traits and each define their own internal backend trait. "Runtime" because each maps directly to one paradigm/family of underlying tooling.
- **`DockerBackend`** / **`AppleBackend`** / **`DSbxBackend`** (Layer 1, in their respective runtime subtrees) — concrete drivers that implement the backend trait declared by their runtime. "Backend" because they're concrete driver impls.

All three tiers live at Layer 1 (engine). Nothing in this hierarchy belongs in Layer 2 or above.

## User Stories

### User Story 1
As a: developer maintaining awman
I want to: have two clearly-separated runtime tiers — container-class and sandbox-class — instead of one `ContainerRuntime` that shoehorns both paradigms into the same option type and trait
So I can: reason about each paradigm's capabilities, test them independently, and add future sandbox-class runtimes (Firecracker-based, Apple's next-gen offerings, etc.) without further awkward mappings.

### User Story 2
As a: developer writing a new Layer 1 runtime backend
I want to: implement a single, focused trait (`ContainerBackend` or `SandboxBackend`) whose method shapes match my paradigm
So I can: ship a working backend without writing translation glue for methods that don't apply to my runtime (e.g. `image_home_dir` for a sandbox, `commands.install` for a container).

### User Story 3
As a: developer in Layer 2 (Command tier)
I want to: call into `AgentRuntimeEngine` via one trait surface and let the engine decide whether the underlying tier is a container or a sandbox
So I can: write `ChatCommand`, `ExecCommand`, `WorkflowEngine` etc. once against the facade, ask `capabilities()` when a paradigm-specific decision matters, and otherwise stay paradigm-agnostic.

### User Story 4
As a: user of awman with a `docker-sbx-experimental` config string
I want to: get a clear, actionable error message ("Docker Sandbox backend is not yet implemented; track WI 0090") instead of a panic or silent fallback to Docker
So I can: set up the config in advance of WI 0090 landing, verify my host environment is correct (sbx installed, login complete), and be ready the moment the real implementation ships.

### User Story 5
As a: developer who has both Docker and Apple Containers installed today
I want to: continue using awman exactly as I do now, with no observable behavior change from this refactor
So I can: trust the refactor is structural-only and benefit from the cleaner architecture without paying any regression cost.

## Implementation Details

### Phase 0 — Pre-work: read the grand architecture doc

Re-read `aspec/architecture/2026-grand-architecture.md` end-to-end. Pay particular attention to:

- **Tenet 1**: lower layers expose APIs only to the layer above. Higher layers communicate downward; lower layers communicate upward only via traits the higher layer provides.
- **Tenet 2**: frontends never implement business logic. All routing through Layer 2's `Dispatch`.
- **Tenet 3**: typed objects over raw `pub` functions. Builder/factory patterns. Options structs over long parameter lists.
- The Layer 1 description, specifically the `ContainerRuntime` paragraph — the new `AgentRuntimeEngine` is the generalization of that concept and must respect the same tenets.

If anything in this work item conflicts with the grand architecture, the grand architecture wins. Surface the conflict to the developer rather than papering over it.

### Phase 1 — Module layout

Create new module subtrees and move/rename existing ones as follows. All paths are under `src/engine/`.

```
src/engine/
├── agent_runtime/                     (new)
│   ├── mod.rs                         — AgentRuntimeEngine trait + ResolvedAgentOptions enum + detect()
│   ├── capabilities.rs                — Capabilities struct
│   ├── execution.rs                   — AgentExecution trait + AgentExitInfo + AgentStats + AgentHandle
│   └── frontend.rs                    — common frontend traits (PTY, status reporting) shared by both tiers
│
├── container/                         (existing — refactored in place, no behavioral change)
│   ├── mod.rs                         — re-exports ContainerRuntime
│   ├── runtime.rs                     — ContainerRuntime struct; impls AgentRuntimeEngine
│   ├── backend.rs                     — ContainerBackend trait (pub(super))
│   ├── options.rs                     — ContainerOption / ResolvedContainerOptions (unchanged)
│   ├── docker.rs                      — DockerBackend (unchanged)
│   ├── apple.rs                       — AppleBackend (unchanged)
│   ├── instance.rs, naming.rs, …      — existing files, behavior preserved
│   └── (everything else unchanged)
│
└── sandbox/                           (new)
    ├── mod.rs                         — re-exports SandboxRuntime
    ├── runtime.rs                     — SandboxRuntime struct; impls AgentRuntimeEngine
    ├── backend.rs                     — SandboxBackend trait (pub(super))
    ├── options.rs                     — SandboxOption / ResolvedSandboxOptions
    ├── naming.rs                      — generate_sandbox_name(worktree_hash, agent)
    └── dsbx/                          — Docker Sandbox driver
        ├── mod.rs
        └── backend.rs                 — DSbxBackend (STUBBED in this WI; implemented in WI 0090)
```

The existing `ContainerRuntime` in `src/engine/container/runtime.rs` keeps its name and file path. It gains an `impl AgentRuntimeEngine for ContainerRuntime`. None of its existing public methods are renamed or removed; they remain available to anyone who has a `&ContainerRuntime` directly, but the canonical access path from Layer 2 becomes `&dyn AgentRuntimeEngine`.

### Phase 2 — `AgentRuntimeEngine` trait surface

Define in `src/engine/agent_runtime/mod.rs`:

```rust
pub trait AgentRuntimeEngine: Send + Sync {
    /// Stable machine name for this runtime (e.g. "docker", "apple-containers",
    /// "docker-sbx-experimental"). Used for log lines and config round-trips.
    fn runtime_name(&self) -> &'static str;

    /// User-facing display name (e.g. "Docker", "Apple Containers",
    /// "Docker Sandboxes (experimental)").
    fn display_name(&self) -> &'static str;

    /// Static description of what this runtime can do. Layer 2 reads this
    /// to decide how to map cross-paradigm options before calling build().
    fn capabilities(&self) -> &Capabilities;

    /// Probe whether the underlying tooling is reachable. Times out on its own.
    fn is_available(&self) -> bool;

    /// Construct a configured execution from typed options. The caller passes
    /// `ResolvedAgentOptions`; the runtime is free to reject options whose
    /// paradigm doesn't fit (returning a clear EngineError).
    fn build(
        &self,
        options: ResolvedAgentOptions,
    ) -> Result<Box<dyn AgentExecution>, EngineError>;

    /// Enumerate handles for running agents created by this runtime.
    fn list_running(&self, session: &Session) -> Result<Vec<AgentHandle>, EngineError>;

    /// Same as list_running but session-less, for stats polling loops.
    /// Replaces today's inherent `list_running_sync()` on ContainerRuntime.
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError>;

    /// Per-handle resource stats. Returns zeros when the runtime can't
    /// provide per-resource metrics (sandbox-class runtimes today).
    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError>;

    /// Stop a running agent. Semantics vary per runtime:
    ///   - container: stop + rm
    ///   - sandbox:   stop (preserve persistent volume)
    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError>;

    /// Build argv for an exec/re-attach against an existing agent.
    fn exec_args(
        &self,
        agent_id: &str,
        working_dir: &str,
        entrypoint: &[&str],
        env_vars: &[(&str, &str)],
    ) -> Vec<String>;

    /// Name of the CLI binary this runtime drives ("docker", "container", "sbx").
    fn cli_binary(&self) -> &'static str;
}

/// Factory: pick the right runtime based on GlobalConfig.runtime.
/// Returns Box<dyn AgentRuntimeEngine> so callers don't see the concrete type.
pub fn detect(config: &GlobalConfig) -> Result<Box<dyn AgentRuntimeEngine>, EngineError> {
    // Match on config.runtime:
    //   None | Some("docker")                -> Box::new(ContainerRuntime::docker())
    //   Some("apple-containers")             -> Box::new(ContainerRuntime::apple()) (with platform guard)
    //   Some("docker-sbx-experimental")      -> Box::new(SandboxRuntime::dsbx())    (with platform guards)
    //   Some(other)                          -> warn, fall back to Docker
}
```

`Capabilities` is a struct exposing paradigm-specific flags Layer 2 can branch on:

```rust
pub struct Capabilities {
    pub arbitrary_env_vars: bool,        // true: container; false: sandbox
    pub arbitrary_host_mounts: bool,     // true: container; false: sandbox
    pub cpu_limits: bool,                // true: container; false: sandbox
    pub per_resource_stats: bool,        // true: container; false: sandbox
    pub persistent_lifecycle: bool,      // false: container; true: sandbox
    pub kit_declarative: bool,           // false: container; true: sandbox
    pub dind: DindSupport,
    pub host_paths_visible: bool,        // true: container; false: sandbox (workspace only)
    pub session_label_supported: bool,   // true: container; false: sandbox (uses names)
}

pub enum DindSupport {
    Always,        // sandbox: every VM has private DinD
    OnRequest,     // container: --allow-docker
    Never,
}
```

`ResolvedAgentOptions` is the common option carrier between Layer 2 and the runtime tier:

```rust
pub enum ResolvedAgentOptions {
    Container(ResolvedContainerOptions),  // existing type, unchanged
    Sandbox(ResolvedSandboxOptions),      // new type, declared by SandboxRuntime
}
```

Layer 2 constructs whichever variant matches the runtime kind it's targeting. `AgentRuntimeEngine::build()` matches on the variant and rejects (with a clear error) any mismatch — a `ContainerRuntime` given `ResolvedAgentOptions::Sandbox` returns `EngineError::OptionVariantMismatch { runtime: "docker", got: "sandbox" }`. The mismatch case should be unreachable in practice; the typed enum exists to make the cross-paradigm boundary explicit.

`AgentExecution` is the trait the returned execution implements — analogous to today's `ContainerExecution` but defined at the `AgentRuntimeEngine` level so both tiers' executions implement it:

```rust
pub trait AgentExecution: Send {
    fn handle(&self) -> &AgentHandle;
    fn wait_blocking(self: Box<Self>) -> Result<AgentExitInfo, EngineError>;
    fn try_inject_stdin(&self, bytes: &[u8]) -> Result<bool, EngineError>;
    fn cancel(&self) -> Result<(), EngineError>;
}
```

`AgentHandle`, `AgentExitInfo`, `AgentStats` are the unified handle/exit/stats types. They sit in `src/engine/agent_runtime/execution.rs`. They replace today's `ContainerHandle`, `ContainerExitInfo`, `ContainerStats` at the trait-surface level. The rename is **atomic in this PR**: the old type names are deleted in the same commit that adds the new ones, and every call site is updated in the same PR. There are no deprecated aliases, no `pub use` shims, no transitional re-exports. If `cargo check` fails because a call site still uses `ContainerHandle`, that call site is fixed — not aliased around. The change is mechanical (identifier rename across the tree) and the existing test assertions remain semantically identical even when their identifiers update.

### Phase 2.5 — Complete rename inventory (no-alias enforcement)

A regression audit of the current codebase identified **every** symbol that must be addressed by this refactor. The no-alias rule means each entry below is mandatory — leaving any of them as `Container*` after the refactor (or aliased) is a hard fail.

**Types that rename to `Agent*` (cross-paradigm — used by both runtimes through the facade)**:

| Today | Rename to | Location after rename | Notes |
|---|---|---|---|
| `ContainerHandle` | `AgentHandle` | `src/engine/agent_runtime/execution.rs` | Handle returned by list/stats/build paths. |
| `ContainerExitInfo` | `AgentExitInfo` | `src/engine/agent_runtime/execution.rs` | Exit info from `wait_blocking`. |
| `ContainerStats` | `AgentStats` | `src/engine/agent_runtime/execution.rs` | Per-handle resource stats. |
| `ContainerInstance` | `AgentInstance` | `src/engine/agent_runtime/execution.rs` | Configured-but-not-running shape. **See "two-step build/run" below.** |
| `ContainerExecution` | `AgentExecution` | `src/engine/agent_runtime/execution.rs` | Running shape. |
| `ContainerFrontend` | `AgentFrontend` | `src/engine/agent_runtime/frontend.rs` | Layer-2-implemented frontend trait. |
| `ContainerStatus` | `AgentStatus` | `src/engine/agent_runtime/frontend.rs` | Enum used by `report_status`. |
| `ContainerProgress` | `AgentProgress` | `src/engine/agent_runtime/frontend.rs` | Struct used by `report_progress`. |
| `ContainerIo` | `AgentIo` | `src/engine/agent_runtime/frontend.rs` | I/O channel handle returned by `take_container_io` (also renamed to `take_io`). |
| `ContainerExec` | `AgentExec` | `src/engine/agent_runtime/background.rs` | Trait for exec-into-running. Both paradigms support exec. |

**Types that keep `Container*` naming (container-paradigm-specific — `pub(super)` inside `src/engine/container/`)**:

| Type | Why it stays Container* |
|---|---|
| `ContainerBackend` | Trait defining container-paradigm backend driver. Sandbox has its own `SandboxBackend`. |
| `ContainerOption` | Enum of container-paradigm option variants. Sandbox has `SandboxOption`. |
| `ResolvedContainerOptions` | Built from `ContainerOption`. Sandbox has `ResolvedSandboxOptions`. |
| `ContainerName` | A `--name` for `docker run` / `container run`. Sandbox naming is separate. |
| `ContainerRuntime` | The container-paradigm `AgentRuntimeEngine` impl. |
| `BackgroundContainer` | Setup/teardown container with bind-mounts and env. Sandbox lifecycle differs. |
| `ImageRef`, `Entrypoint`, `OverlaySpec`, `OverlayPermission`, `EnvVar`, `EnvLiteral`, `YoloMode`, `AutoMode`, `PlanMode`, `CpuLimit`, `MemoryLimit`, `ModelFlagForm`, `AgentSettings`, `ResolveError` | Component types of `ContainerOption`. Stay container-side. |

**Types that have NO rename and NO prefix (already paradigm-agnostic)**:

| Type | Notes |
|---|---|
| `StuckEvent` | No `Container` prefix today. Used unchanged across paradigms. Moves to `src/engine/agent_runtime/execution.rs`. |
| `CancelHandle` | No `Container` prefix today. Cancel mechanism is paradigm-agnostic. Moves to `src/engine/agent_runtime/execution.rs`. |
| `ExecOutput` | No `Container` prefix today. Stays paradigm-agnostic; moves to `src/engine/agent_runtime/background.rs`. |
| `generate_container_name()` (function) | **Stays as-is.** Container-paradigm naming function. Sandbox naming is `generate_sandbox_name()` introduced in Phase 4. |

**Module-level re-exports** in `src/engine/container/mod.rs` are updated to match: the old `pub use frontend::{ContainerFrontend, ContainerProgress, ContainerStatus};` line is deleted (those types no longer live in `container/frontend.rs`). The container module re-exports only container-paradigm-specific items. Cross-paradigm types are imported from `src/engine/agent_runtime/`.

### Phase 2.6 — Two-step build/run pattern (preserve from current architecture)

Today's code has a deliberate two-step pattern:

1. `ContainerRuntime::build(options) → Box<dyn ContainerInstance>` — configures but does not spawn.
2. `ContainerInstance::run_with_frontend(frontend) → ContainerExecution` — spawns with a runtime-supplied frontend.

This split lets Layer 2 commands construct an instance, run preparatory side-effects (image checks, overlay validation), and then run with the appropriate frontend. **The refactor must preserve this split.** The collapsed `build() -> Box<dyn AgentExecution>` shape sketched in Phase 2 is wrong — it merges configure-time and run-time concerns and loses the ability to separate them.

The correct trait shape is:

```rust
pub trait AgentRuntimeEngine: Send + Sync {
    // … other methods as listed in Phase 2 …
    fn build(
        &self,
        options: ResolvedAgentOptions,
    ) -> Result<Box<dyn AgentInstance>, EngineError>;
}

pub trait AgentInstance: Send + Sync {
    fn handle_preview(&self) -> AgentHandlePreview;  // name+image, no started-at
    fn run_with_frontend(
        self: Box<Self>,
        frontend: Box<dyn AgentFrontend>,
    ) -> Result<Box<dyn AgentExecution>, EngineError>;
}

pub trait AgentExecution: Send {
    fn handle(&self) -> &AgentHandle;
    fn wait_blocking(self: Box<Self>) -> Result<AgentExitInfo, EngineError>;
    fn try_inject_stdin(&self, bytes: &[u8]) -> Result<bool, EngineError>;
    fn cancel(&self) -> Result<(), EngineError>;
    fn cancel_handle(&self) -> Option<CancelHandle>;
    fn subscribe_stuck(&self) -> tokio::sync::broadcast::Receiver<StuckEvent>;
    fn detach(self: Box<Self>) -> AgentHandle;
}
```

Phase 2's earlier `AgentExecution` snippet is **superseded** by this section. The two-step pattern is non-negotiable — it's how every existing Layer 2 command operates and breaking it would cascade across the whole tree.

### Phase 2.7 — Paradigm-specific methods stay off the facade

Some methods on today's `ContainerRuntime` are container-paradigm-specific. They do **not** appear on the `AgentRuntimeEngine` trait. They remain as **inherent methods on the concrete `ContainerRuntime` struct**, callable only by code that holds a typed `Arc<ContainerRuntime>`:

- `build_image(tag, dockerfile, context, no_cache, on_line)` — Docker / Apple `build` command. Sandboxes don't `build` (they pull templates from registries).
- `image_exists(tag)` — Local image-store probe. Sandboxes don't have a local image store.
- `image_home_dir(tag)` — Reads baked-in `$HOME` from image config. Sandboxes use the kit's template default.
- `start_background(image, workdir, env, overlays) -> BackgroundContainer` — Setup/teardown container with bind mounts. Sandboxes have no equivalent (workspace-only mounts).
- `list_running_sync()` — convenience for TUI stats poll. Returns `Vec<AgentHandle>` so it could be on the facade, but the underlying lookup is paradigm-specific. **Decision: lift to the facade** since the return type is paradigm-agnostic; sandboxes implement it via `sbx ls`.

This means `Engines::runtime` field type must be revisited: see Phase 5.

### Phase 2.8 — `ContainerBackend` trait shape after rename

`ContainerBackend` keeps its method names and semantics. Method signatures change to use the renamed types:

```rust
pub(super) trait ContainerBackend: Send + Sync {
    fn build(&self, options: ResolvedContainerOptions) -> Result<Box<dyn AgentInstance>, EngineError>;
    fn list_running(&self, session: &Session) -> Result<Vec<AgentHandle>, EngineError>;
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> { … }
    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError>;
    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError>;
    fn exec_args(&self, container_id: &str, working_dir: &str, entrypoint: &[&str], env_vars: &[(&str, &str)]) -> Vec<String>;
    fn name(&self) -> &'static str;
    fn image_home_dir(&self, _tag: &str) -> Option<String> { None }
    fn cli_binary(&self) -> &'static str { … }
    // Background ops — unchanged: still take container-paradigm inputs.
    fn start_background(&self, image: &str, workdir: &Path, env: &HashMap<String, String>, overlays: &[OverlaySpec]) -> Result<String, EngineError> { … }
    fn exec_in_background(&self, container_id: &str, command: &str, working_dir: &str, env: Option<&HashMap<String, String>>) -> Result<ExecOutput, EngineError> { … }
    fn exec_in_background_streaming(&self, container_id: &str, command: &str, working_dir: &str, env: Option<&HashMap<String, String>>, on_line: &mut dyn FnMut(&str)) -> Result<ExecOutput, EngineError> { … }
    fn stop_and_remove(&self, container_id: &str) -> Result<(), EngineError> { … }
}
```

Default implementations of background ops keep their `cli_binary()`-based dispatch — they shell out to `docker` or `container` exactly as today. These defaults are container-paradigm-correct; they do not need to change.

Returning `Box<dyn AgentInstance>` from `build()` instead of `Box<dyn ContainerInstance>` is the only structural change. `DockerBackend::build()` and `AppleBackend::build()` return the same underlying struct types as today (`DockerContainerInstance`, `AppleContainerInstance`); those types now `impl AgentInstance` instead of `impl ContainerInstance`. **Behavior is unchanged**; only the trait name differs.

### Phase 3 — Refactor `ContainerRuntime` to impl `AgentRuntimeEngine`

The existing `ContainerRuntime` in `src/engine/container/runtime.rs` already has methods that map almost 1:1 to the new trait. The refactor:

1. Add `impl AgentRuntimeEngine for ContainerRuntime { ... }` delegating to the existing inherent methods.
2. `ContainerRuntime::build()`'s inherent signature changes from accepting `IntoIterator<Item = ContainerOption>` to accepting `ResolvedContainerOptions`. Every existing call site that previously passed an iterator is updated in the same PR to pre-resolve via `ResolvedContainerOptions::resolve(...)`. The trait impl `AgentRuntimeEngine::build()` unwraps `ResolvedAgentOptions::Container(opts)` and calls the inherent method. There is no parallel signature kept "for backward compatibility" — the old shape is gone.
3. Replace `ContainerRuntime::detect()` (factory) with two methods: `ContainerRuntime::docker()` and `ContainerRuntime::apple()`, each constructing a runtime with the appropriate backend. The new top-level `agent_runtime::detect()` calls these.
4. Add `ContainerRuntime::capabilities()` returning the container capabilities struct.
5. Add `ContainerRuntime::cli_binary()` returning `"docker"` or `"container"` based on the backend.

The `ContainerBackend` trait stays exactly where it is, with its current shape and `pub(super)` visibility. The existing `DockerBackend` and `AppleBackend` are unchanged. **Zero changes** to their behavior or their tests.

The `ContainerRuntime::build_image()`, `image_exists()`, and `image_home_dir()` methods are container-paradigm concerns and stay as **inherent methods** on `ContainerRuntime`, **not** on the `AgentRuntimeEngine` trait. Code that needs them (the existing `awman ready` flow for Docker/Apple) calls them on the concrete `ContainerRuntime`. If Layer 2 has only `Box<dyn AgentRuntimeEngine>` and needs an image operation, it must downcast — which is a strong signal it's reaching past the abstraction and probably shouldn't be. The downcast helper is intentionally not added; the abstraction is honest.

### Phase 4 — `SandboxRuntime`, `SandboxBackend`, `ResolvedSandboxOptions`

Create `src/engine/sandbox/`:

- `runtime.rs`: `pub struct SandboxRuntime { backend: Arc<dyn SandboxBackend> }`. Impls `AgentRuntimeEngine`. Constructor: `SandboxRuntime::dsbx() -> Self`. Platform guards (Linux block, Intel-Mac block) live in the constructor — same logic as the original WI's `detect()` snippet.
- `backend.rs`: `pub(super) trait SandboxBackend: Send + Sync { ... }`. The trait's method shape is sandbox-paradigm-appropriate — no `image_home_dir`, no `build_image`. Includes `start_sandbox`, `restart_sandbox`, `exec_in_sandbox`, `stop`, `remove`, `list_running`, `stats`. The exact shape is finalized in this phase based on what WI 0090 needs; the goal is "minimum surface that WI 0090's DSbxBackend can satisfy."
- `options.rs`: `pub enum SandboxOption { … }` and `pub struct ResolvedSandboxOptions { … }`. Fields:
  - `agent_id: String` (kit selector)
  - `entrypoint_override: Option<Entrypoint>`
  - `workspace_dir: PathBuf`
  - `extra_overlays: Vec<OverlaySpec>`
  - `env_passthrough: Vec<EnvVar>`
  - `env_literal: Vec<EnvLiteral>`
  - `seeded_prompt: Option<String>`
  - `interactive: bool`
  - `sandbox_name: Option<String>` (deterministic per (worktree, agent); see WI 0090)
  - `memory_gb: Option<u32>`
  - `cpu_limit: Option<f64>` (recorded but unused; capability flag says CPU limits unsupported)
  - `agent_settings: HashMap<String, serde_json::Value>` (serialized into session.json by WI 0090)
  - `agent_credentials: Vec<(String, String)>` (key/value pairs; routed to `sbx secret set` by WI 0090)
  - `system_prompt_file: Option<(PathBuf, PathBuf, String)>` (host, container, flag)
  - `system_prompt_env_file: Option<(String, PathBuf, PathBuf)>`
  - `system_prompt_inline: Option<(String, String)>` (flag, text)
  - `disallowed_tools: Vec<String>`
  - `allowed_tools: Vec<String>`
  - `model: Option<ModelFlagForm>`
  - `keep_after_exit: bool`
- `naming.rs`: `pub fn generate_sandbox_name(worktree_hash: &str, agent: &str) -> String`. Deterministic — same inputs always produce the same output. Used by WI 0090 for the per-worktree persistent-sandbox model.
- `dsbx/backend.rs`: `pub(super) struct DSbxBackend; impl SandboxBackend for DSbxBackend { ... }`. **Every method returns `EngineError::NotImplemented` with a message naming the method and pointing to WI 0090**. Example:
  ```rust
  fn start_sandbox(&self, _opts: &ResolvedSandboxOptions) -> Result<SandboxId, EngineError> {
      Err(EngineError::NotImplemented(
          "DSbxBackend::start_sandbox is stubbed; see work-item 0090 for the implementation"
      ))
  }
  ```
- `dsbx/mod.rs`: re-exports `DSbxBackend` with `pub(super)` visibility.

The platform guards in `SandboxRuntime::dsbx()` still error correctly — a user on Linux gets `BackendUnsupportedOnPlatform` from the constructor, never reaches the stubbed backend. A user on macOS arm64 reaches the stub and gets `NotImplemented` from the first method call — which is the desired UX until WI 0090 ships.

### Phase 5 — Wire `agent_runtime::detect()` into the codebase, fix `Engines::runtime` field type

Replace every Layer 2 call site that does `ContainerRuntime::detect(&config)?` with `agent_runtime::detect(&config)?`. The variable type changes from `ContainerRuntime` to `Box<dyn AgentRuntimeEngine>`. Most call sites only invoke methods that are on the new trait, so the change is mechanical.

**`Engines` struct field type — the load-bearing change**:

Today's `Engines` struct in `src/command/dispatch/mod.rs` holds:
```rust
pub struct Engines {
    pub runtime: Arc<ContainerRuntime>,
    // …
}
```

This is the single shared handle every Layer 2 command uses. Under the refactor it becomes:
```rust
pub struct Engines {
    /// Cross-paradigm trait-object handle. Used for build(), list_running(),
    /// stats(), stop(), exec_args(), is_available(), capabilities() and
    /// other operations that exist on both paradigms.
    pub runtime: Arc<dyn AgentRuntimeEngine>,

    /// Container-paradigm-specific handle, set when `runtime` is a
    /// ContainerRuntime. None when the active runtime is a SandboxRuntime.
    /// Used for image-paradigm operations (build_image, image_exists,
    /// image_home_dir, start_background) that only exist on the container side.
    pub container_runtime: Option<Arc<ContainerRuntime>>,

    /// Sandbox-paradigm-specific handle, mirror of container_runtime for
    /// sandbox-only operations (kit emission helpers, session-config writer
    /// access, etc.). None when running under a ContainerRuntime.
    pub sandbox_runtime: Option<Arc<SandboxRuntime>>,
    // …
}
```

`agent_runtime::detect()` returns the cross-paradigm `Box<dyn AgentRuntimeEngine>` plus a hint of which concrete type it built. The `Engines` constructor populates either `container_runtime` or `sandbox_runtime` based on that hint, never both. Call sites that previously called `self.engines.runtime.build_image(...)` change to `self.engines.container_runtime.as_ref().expect("ready phase requires container runtime").build_image(...)`. The `expect` is fine here because the ready phase that needs `build_image` is only entered for the container path; the dispatch logic enforces the precondition.

This is **not** a back-compat shim. Both the cross-paradigm and the paradigm-specific handles point to the same underlying object (the `ContainerRuntime` is held by both `runtime` and `container_runtime` Arcs). There is no aliasing, no `pub use` re-export, no deprecated method. The struct field shape honestly expresses that some operations are paradigm-specific.

**Alternative considered and rejected**: downcasting `Box<dyn AgentRuntimeEngine>` to `&ContainerRuntime` via `Any`. Rust requires `Any: 'static` plus explicit downcasts (`as_any().downcast_ref::<ContainerRuntime>()`). This works but produces noisier call sites and adds a `dyn Any` requirement on the trait. The two-field approach is cleaner.

**Call sites that need updating** (audit list, every one of these must compile after the refactor with no aliases):

| Caller | Before | After |
|---|---|---|
| `main.rs:59` (`ContainerRuntime::detect`) | `ContainerRuntime::detect(&global_config)` | `agent_runtime::detect(&global_config)` |
| `frontend/api/mod.rs:68` | `ContainerRuntime::detect(...)` | `agent_runtime::detect(...)` |
| `frontend/tui/mod.rs:1382`, `frontend/tui/app.rs:646`, `command/dispatch/mod.rs:873`, `frontend/api/routes.rs:1930`, `command/commands/{specs,new,exec_workflow,api_server}.rs` test-helper sites | `Arc::new(ContainerRuntime::docker())` | `Arc::new(ContainerRuntime::docker())` (unchanged — these are test-helper paths that need the concrete container type and will populate both fields of `Engines` for the test harness) |
| `chat.rs:311`, `exec_prompt.rs:70` | `runtime.image_exists(tag)` | `engines.container_runtime.as_ref().expect("image_exists requires container runtime").image_exists(tag)` |
| `exec_workflow.rs:1382` | `engines.runtime.image_home_dir(tag)` | `engines.container_runtime.as_ref().expect(...).image_home_dir(tag)` |
| `exec_workflow.rs:1051,1127` | `runtime.start_background(...)` | `engines.container_runtime.as_ref().expect(...).start_background(...)` |
| `ready/mod.rs:230,247,261,289,301,337` | `self.container_runtime.is_available()` / `.image_exists()` / `.build_image()` | `self.container_runtime.is_available()` / `.image_exists()` / `.build_image()` (the ReadyEngine internal field is already typed `Arc<ContainerRuntime>` — keep concrete because the ready engine for the container path is paradigm-specific) |
| `status.rs:168`, `app.rs:502` | `runtime.list_running_sync()` | `runtime.list_running_all()` — atomic rename: the old `list_running_sync` method on `ContainerRuntime` is deleted; the equivalent is `list_running_all` on the `AgentRuntimeEngine` trait. Both call sites updated in the same PR. (Old name has no alias.) |
| All call sites of `runtime.build(...)`, `runtime.list_running()`, `runtime.stop()`, `runtime.stats()`, `runtime.exec_args()`, `runtime.is_available()`, `runtime.cli_binary()`, `runtime.display_name()`, `runtime.runtime_name()` | Same call shape | Same — these methods are all on `AgentRuntimeEngine` |

Layer 2 code that branches on capabilities uses `engine.capabilities()`:

```rust
let engine = agent_runtime::detect(&config)?;
let options = if engine.capabilities().arbitrary_env_vars {
    ResolvedAgentOptions::Container(build_container_options(...))
} else {
    ResolvedAgentOptions::Sandbox(build_sandbox_options(...))
};
let instance = engine.build(options)?;
let execution = instance.run_with_frontend(frontend)?;
```

(Note the two-step pattern preserved from Phase 2.6: `build()` returns `AgentInstance`, then `run_with_frontend()` returns `AgentExecution`.)

The few call sites that need to disambiguate (e.g., the ready engine deciding to run Dockerfile-based vs Kit-based flows) match on `engine.runtime_name()` or use a capability flag. The string-match path is acceptable for boolean dispatches; for richer decisions, prefer capability-based dispatch.

**`AgentEngine::container_runtime_arc()` (`src/engine/agent/mod.rs:90`)**: today this returns `&Arc<ContainerRuntime>`. It's used by code that needs the concrete runtime to call image methods. Two options:
- Keep it, return `&Arc<ContainerRuntime>` (still the typed concrete handle). Callers who need image methods use this path; they always know they're on the container path because `AgentEngine` is itself container-paradigm-specific in those flows.
- Add a peer `sandbox_runtime_arc()` returning `&Arc<SandboxRuntime>` and let callers pick.

Pick option 1 for this refactor; revisit if sandbox-specific flows in WI 0090 need a peer accessor.

### Phase 6 — Frontend trait re-homing

Today's `ContainerFrontend` trait (in `src/engine/container/frontend.rs`) is the Layer-2-implemented frontend that Layer 1 calls back into for PTY size, stdin, status reporting. After this refactor, both `ContainerRuntime` and `SandboxRuntime` need an equivalent. Two design options:

**Option A (recommended): rename to `AgentFrontend` and lift to `src/engine/agent_runtime/frontend.rs`.** Both runtimes use the same trait. Sandbox-side calls may ignore some methods (e.g., container-specific status messages) but the surface is the same. The rename is atomic — `ContainerFrontend` is deleted in the same PR; every Layer 3 impl (`src/frontend/cli/per_command/container_frontend_marker.rs`, `src/frontend/tui/per_command/container_frontend.rs`, `src/frontend/api/command_frontend.rs`) is updated to `impl AgentFrontend` in the same commit. No `pub use ContainerFrontend = AgentFrontend;` alias.

**Option B: keep `ContainerFrontend` in its current location and add a separate `SandboxFrontend` trait.** Cleaner separation; double the implementation surface in Layer 3.

Pick Option A unless Phase 4 reveals genuine semantic divergence in what each tier needs from its frontend. Document the choice in the PR.

### Phase 7 — Tests

- **Unit tests for `agent_runtime::detect`**: every config-string variant returns a runtime whose `runtime_name()` matches. Unknown strings fall back to Docker with a warning.
- **Unit test for `Capabilities` correctness**: assert each runtime's capabilities struct has the expected flags (e.g., `SandboxRuntime::dsbx().capabilities().kit_declarative == true` and `ContainerRuntime::docker().capabilities().arbitrary_env_vars == true`).
- **Stub behavior test**: every `DSbxBackend` method returns `EngineError::NotImplemented` with a message naming WI 0090.
- **Container path regression**: every existing `ContainerRuntime` test continues to pass with semantically identical assertions. Test bodies may need mechanical identifier updates (e.g. `ContainerHandle` → `AgentHandle` references in test code) because of the atomic rename. No behavioral assertion changes. If a test needs a *semantic* change to pass after the refactor, the refactor has drifted into behavior changes and that change must be reverted.
- **Platform guards**: `SandboxRuntime::dsbx()` errors on Linux and on x86_64 macOS, exactly as the original WI 0090 (now WI 0090) spec'd.
- **Option-variant mismatch test**: `ContainerRuntime::build(ResolvedAgentOptions::Sandbox(_))` returns a clear `EngineError`, and vice versa.

### Phase 8 — Grand architecture doc update

The grand architecture doc (`aspec/architecture/2026-grand-architecture.md`) currently names `ContainerRuntime` as a Layer 1 component. Update it to:

- Name `AgentRuntimeEngine` as the Layer 1 trait family.
- Describe the two runtime tiers (`ContainerRuntime`, `SandboxRuntime`) as concrete impls.
- Describe the backend tier (`ContainerBackend` / `SandboxBackend` traits, concrete backends).
- Preserve every existing tenet — the new abstraction is an instance of Tenet 3 (typed objects), not a departure from it.

This doc edit ships in the same PR as the code refactor.

### Phase 9 — Documentation

User-facing docs need a one-paragraph addition explaining that awman now has two runtime tiers (container-class and sandbox-class), and that `docker-sbx-experimental` is the first sandbox-class option but is not yet functional (track WI 0090). Update wherever the existing `runtime:` config option is documented.

No work-item-specific docs.

## Non-interference with existing Docker/Apple flows (load-bearing)

The refactor must not change any user-observable behavior of the existing Docker or Apple Containers backends. Specifically:

- All existing `ContainerRuntime` inherent methods keep their behavior (semantics). Signatures may change where types they return have been renamed atomically (`ContainerHandle` → `AgentHandle`, etc.); the method names and their effects are unchanged. `build_image()`, `image_exists()`, `image_home_dir()`, `list_running()`, `stats()`, `stop()`, `start_background()`, `exec_args()`, `cli_binary()`, `display_name()`, `runtime_name()`, `is_available()` — all retained.
- `ContainerBackend` trait keeps its current shape. `DockerBackend` and `AppleBackend` are unchanged.
- `ContainerOption` enum and `ResolvedContainerOptions` struct are unchanged.
- No `templates/Dockerfile.*` file is touched.
- Every existing test in `src/engine/container/{docker.rs,apple.rs,backend.rs,instance.rs,options.rs,runtime.rs,naming.rs,background.rs,io_bridge.rs}` continues to pass with assertions that are semantically unchanged. Test source code may be touched only for mechanical identifier renames driven by the no-alias rule; no assertion's meaning changes.
- Default runtime remains `docker`. Unknown `runtime:` strings still fall back to Docker with a warning.
- The rename of `ContainerHandle`/`ContainerExitInfo`/`ContainerStats` to `AgentHandle`/`AgentExitInfo`/`AgentStats` is performed atomically. Every call site is updated in the same PR. No deprecated aliases or `pub use` shims are left behind. Tests use the new identifiers; their assertions are semantically identical to before the rename (they assert the same field values and behaviors, on types that now have different names).

A user who runs `awman` with `runtime: "docker"` (or no runtime set) after this work item ships must see byte-identical CLI/TUI/API behavior to before the change. The same applies to `runtime: "apple-containers"` on macOS.

## Switching to `docker-sbx-experimental` (expected behavior after this WI)

A user can set `runtime: "docker-sbx-experimental"` in `GlobalConfig` and:

1. `awman` starts. The runtime is detected as `SandboxRuntime`.
2. `awman --help` and basic non-runtime operations work.
3. Any operation that actually needs to spawn an agent (`awman chat`, `awman exec`, `awman ready`'s sandbox-specific phases) fails with `EngineError::NotImplemented` carrying a message like: `"docker-sbx-experimental is not yet implemented — track work-item 0090"`.
4. Switching back to `runtime: "docker"` immediately restores full functionality.

This is the contract WI 0090 builds on: WI 0089 establishes the wiring and the failure mode; WI 0090 replaces every `NotImplemented` with the real impl.

## Test Considerations

### Unit tests

- Detection: every `runtime` string maps to the expected concrete engine.
- Capabilities: each runtime's `capabilities()` returns the expected flags.
- Stub: every `DSbxBackend` method returns `EngineError::NotImplemented` and the message names WI 0090.
- Option-variant mismatch: cross-paradigm options yield a clear error.
- Platform guards on `SandboxRuntime::dsbx()`.

### Integration tests

- Run a Docker container end-to-end through `Box<dyn AgentRuntimeEngine>`. Verify it works identically to the pre-refactor inherent-call path. Env-gated, same as today's `apple` integration tests.
- (No sbx integration tests in this WI — those land in WI 0090.)

### Regression tests

- Every test in `src/engine/container/` continues to pass; assertions are semantically unchanged. Source-code edits limited to mechanical identifier renames.
- `make test` produces identical pass/fail counts to the pre-change suite (modulo the new abstraction tests added by this WI).

## Codebase Integration

- **`src/engine/agent_runtime/mod.rs`**: new module. Defines the trait, `detect()` factory, `Capabilities`, `ResolvedAgentOptions` enum, plus `AgentExecution`, `AgentHandle`, `AgentExitInfo`, `AgentStats`.
- **`src/engine/agent_runtime/frontend.rs`**: new module. `AgentFrontend` trait (Option A in Phase 6). The old `ContainerFrontend` trait is deleted from `src/engine/container/frontend.rs` in the same PR.
- **`src/engine/container/runtime.rs`**: refactored. Adds `impl AgentRuntimeEngine for ContainerRuntime`. Adds `ContainerRuntime::docker()` and `::apple()` constructors. Existing inherent methods retained. Existing tests untouched.
- **`src/engine/container/mod.rs`**: re-exports `ContainerRuntime` as before. The `ContainerBackend` trait stays `pub(super)`. No new public types leak.
- **`src/engine/sandbox/`**: new subtree (Phase 4). All types `pub(super)` except `SandboxRuntime` itself.
- **`src/engine/mod.rs`**: declares `pub mod agent_runtime;` and `pub mod sandbox;` alongside the existing `pub mod container;`. The `agent_runtime` module is the only one Layer 2 should import from for runtime operations going forward.
- **`src/engine/error.rs`**: adds `EngineError::NotImplemented(&'static str)` if not present, `EngineError::OptionVariantMismatch { runtime: String, got: &'static str }`, `EngineError::Sandbox(String)` (analogous to existing `EngineError::Container(String)`).
- **`src/data/session.rs`** (or wherever `ContainerHandle` lives): rename the three types to `AgentHandle`/`AgentExitInfo`/`AgentStats`. No alias is added. Every call site across the workspace is updated in the same PR.
- **`src/data/config/global.rs`**: no schema changes. The `runtime` field accepts `"docker-sbx-experimental"` as a valid string starting with this work item — the parser already accepts arbitrary strings and the detection layer interprets them.
- **`src/command/`** (Layer 2): every call site that constructs a runtime now goes through `agent_runtime::detect()`. The variable type becomes `Box<dyn AgentRuntimeEngine>`. Method calls that exist on the trait stay; image-specific method calls require direct construction of `ContainerRuntime`. The number of such direct-construction sites is small (mostly `awman ready` for Docker/Apple); document them.
- **`src/frontend/`** (Layer 3): frontend trait impls are updated in the same PR. Every `impl ContainerFrontend for X` in `src/frontend/cli/`, `src/frontend/tui/`, `src/frontend/api/` becomes `impl AgentFrontend for X`. No back-compat shim. If the rename surfaces a Layer 3 dependency on a container-specific method that doesn't have a sandbox counterpart, that's a real design concern to surface — not something to paper over with a default impl that returns nothing for sandboxes.
- **`aspec/architecture/2026-grand-architecture.md`**: Phase 8 edit (described above).
- **Layer discipline**: all new types live at Layer 1. `AgentRuntimeEngine`, `ContainerRuntime`, `SandboxRuntime`, the backend traits, the concrete backends — none of them belong above Layer 1. Layer 2 sees only `Box<dyn AgentRuntimeEngine>` and the trait surface; Layer 2 must not pattern-match on concrete runtime types (matching on `runtime_name()` strings or `capabilities()` flags is the supported way to branch on paradigm). Layer 3 never sees runtime types — it implements frontend traits that Layer 2 hands to the runtime.
- **No higher-layer dependencies in lower-layer code**: standard grand-architecture discipline. Layer 1 imports from Layer 0 only.

### Non-interference checklist (must hold at PR-ready)

**Semantic invariants** (behavior must not change):

- [ ] `DockerBackend` and `AppleBackend` behavior is unchanged. The methods do exactly what they did before. Source-code edits limited to mechanical identifier updates from the atomic renames (`ContainerHandle` → `AgentHandle`, `ContainerFrontend` → `AgentFrontend`, etc.).
- [ ] `ContainerBackend` trait's semantic shape is unchanged: same method count, same per-method semantics, same return obligations. Method signatures may swap renamed types (e.g. `&ContainerHandle` → `&AgentHandle`) but no method is added, removed, or repurposed.
- [ ] `ContainerOption` enum has zero new variants and zero removed variants. `ResolvedContainerOptions` keeps its existing field set.
- [ ] Every existing test in `src/engine/container/*.rs` passes; assertions are semantically identical to before the refactor. Test source code may be touched only for mechanical identifier renames.
- [ ] `git diff templates/Dockerfile.*` shows zero modifications.
- [ ] `make test` passes with identical pre/post pass/fail counts (modulo new abstraction tests).
- [ ] `awman ready` with `runtime: "docker"` produces byte-identical `.awman/Dockerfile.*` to a pre-change checkout.
- [ ] `awman ready` with no `runtime` set defaults to Docker, identical behavior to today.
- [ ] `awman chat --runtime docker-sbx-experimental` fails with `EngineError::NotImplemented` naming WI 0090, not with a panic and not with a silent fallback to Docker.

**No-alias invariants** (the refactor leaves no legacy passthroughs):

- [ ] `git grep -E '(ContainerHandle|ContainerExitInfo|ContainerStats|ContainerInstance|ContainerExecution|ContainerFrontend|ContainerStatus|ContainerProgress|ContainerIo|ContainerExec|list_running_sync)\b'` returns zero hits in `src/` after the refactor lands. Every occurrence has been atomically renamed. (Note: `ContainerOption`, `ResolvedContainerOptions`, `ContainerRuntime`, `ContainerBackend`, `ContainerName`, and `BackgroundContainer` are intentional retainees per the rename table in Phase 2.5 — they appear in the grep output but are not violations.)
- [ ] No `pub use` statement in `src/` aliases an old type/trait name to a new one for backward compatibility. (Module-level `pub use` for re-exporting the **new** public API is fine; aliases of the form `pub use NewName as OldName;` or `pub type OldName = NewName;` are not.)
- [ ] No deprecated annotations (`#[deprecated(...)]`) introduced by this refactor exist in the source tree.
- [ ] No method on any trait or struct has a "_legacy" or "_compat" suffix, or accepts a now-renamed type for "transitional" reasons.
- [ ] If `cargo check` had to be bypassed at any point during the refactor (e.g. via `#[allow(deprecated)]`), the PR is rejected and the underlying call site is fixed instead.

## Documentation

After implementation, update:

- The runtimes user-doc (or create `docs/XX-runtimes.md` if missing) with a one-paragraph note explaining: (a) awman now has a runtime abstraction; (b) Docker and Apple Containers continue to work identically; (c) `docker-sbx-experimental` is recognized but routes to a stub until WI 0090 lands.
- The grand architecture doc (Phase 8).

No work-item-specific docs.

## Post-implementation notes (review outcomes)

Decisions made during/after implementation that deviate from or extend the
text above. Each was reviewed deliberately; none is an oversight.

- **`AgentExecution` is a concrete struct, not a trait.** Phase 2.6 sketches
  `pub trait AgentExecution` with `run_with_frontend → Box<dyn AgentExecution>`.
  The pre-refactor `ContainerExecution` was already a concrete struct wrapping
  an internal backend trait, and the grand architecture describes
  `AgentExecution` as a *type* handed to `WorkflowEngine`. The implementation
  keeps that shape: `AgentExecution` (struct, `src/engine/agent_runtime/execution.rs`)
  wraps a `pub(crate) ExecutionBackend` trait that both paradigms' backends
  implement. Same polymorphism, one fewer public trait, zero behavior change —
  per Phase 0, the grand architecture wins over the Phase 2.6 sketch.
- **`Engines::require_container_runtime()` replaces the spec'd `.expect(...)`
  pattern.** Phase 5 suggested `container_runtime.as_ref().expect(...)` at
  container-paradigm call sites. The implementation instead returns a clean
  `EngineError::NotImplemented` naming WI 0090, so a sandbox-configured user
  can never hit a panic. Strictly better than the spec'd shape.
- **`awman init` requires a container runtime.** The switching contract above
  says "basic non-runtime operations work"; `init` is not one of them — its
  engine builds the agent image. Under `docker-sbx-experimental` it fails
  up-front with the standard `NotImplemented` error. A kit-based init path is
  WI 0090's concern.
- **`awman config` is exempt from runtime detection.** Platform guards live in
  the runtime constructors (Phase 4), so an unsupported `runtime:` string
  fails at startup. To keep the "switching back immediately restores full
  functionality" promise reachable from the CLI, `main.rs` consults
  `CommandCatalogue::requires_runtime()` and lets `config` proceed on an inert
  default runtime (with a warning) when detection fails. All other commands
  and the bare-TUI invocation still fail.
- **Detect-error/log wording changed** from "container runtime" to "agent
  runtime" (`main.rs`, `frontend/api/mod.rs`). Reachable only on
  detection-failure paths, never in a working Docker/Apple flow; the new
  wording matches the abstraction.
- **Merge fix:** the WI branch landed concurrently with the stuck-detection
  fix (`dcb88068`), which referenced `StuckEvent` at its pre-refactor path.
  The merge compiled neither; the follow-up commit repoints the four
  references at `engine::agent_runtime::StuckEvent`.
- **`ContainerExecutionFactory` → `AgentExecutionFactory`.** The Phase 2.5
  rename inventory missed this trait (the checklist grep's word boundary
  doesn't trip on the `Factory` suffix). It produces `AgentExecution` and is
  paradigm-agnostic, so it follows the cross-paradigm rename rule; renamed in
  a follow-up commit (`container_factory` field → `agent_factory`).
- **`engine::message` moved to `data::message`.** Pre-existing
  `architecture-lint` violations (Layer 0 `data::issue` importing
  `engine::message`) surfaced during this WI's review. The message types and
  sink traits are dependency-free and belong at Layer 0, importable by every
  layer. Mechanical move; no type or trait renamed.
