# Work Item: Task

Title: The Great Refocusing — Part 1: Rename & Rebrand (amux → awman, headless → API)
Issue: issuelink

## Summary

amux is being renamed to **awman** (Agentic Workflow Manager). This rename touches the binary name, all Rust identifiers, Cargo crate names, documentation, README, CLI help text, configuration paths, and data directories. Simultaneously, the "headless" frontend and mode is being renamed to "API" throughout — the three frontend modalities are now formally CLI, TUI, and API. This is a prerequisite for all other work items in this series. No functional changes are made in this work item; it is a pure rename/rebrand.

Before implementing, read and internalize `aspec/architecture/2026-grand-architecture.md` in full. Every change must respect the four-layer boundary constraints.

## User Stories

### User Story 1:
As a: user

I want to:
invoke the tool as `awman` from the command line

So I can:
use the new canonical name without ambiguity, with all help text, error messages, config paths, and documentation consistently reflecting the awman brand

### User Story 2:
As a: developer integrating the API frontend

I want to:
see consistent "API mode" naming in all code identifiers, HTTP documentation, and error messages instead of "headless"

So I can:
reason clearly about which frontend I am integrating with, without encountering the legacy "headless" terminology in any user-facing or developer-facing surface

### User Story 3:
As a: user upgrading from amux

I want to:
find my existing config and data migrated or clearly documented as moved from `~/.amux/` to `~/.awman/`

So I can:
continue using my existing sessions, workflows, and configuration without data loss


## Implementation Details

All changes in this work item are mechanical renames. No business logic changes. Implement layer by layer, bottom-up.

### Layer 0: Data (`src/data/`)
- Rename all occurrences of `amux` in identifiers, string literals, error messages, and file path constants to `awman`
- Update global config path from `~/.amux/config.json` (and `config.toml`) to `~/.awman/config.json`
- Update the headless/API SQLite database path and surrounding directory structure from `~/.amux/headless/` to `~/.awman/api/`
- Rename the `headless_db.rs` module to `api_db.rs`; rename `headless_paths.rs` to `api_paths.rs`
- Rename all types that include "Headless" in their name at this layer to use "Api" (e.g. `HeadlessDb` → `ApiDb`, `HeadlessSessionRecord` → `ApiSessionRecord`)
- Update `AMUX_*` env var prefix constants to `AWMAN_*` (e.g. `AMUX_API_KEY` → `AWMAN_API_KEY`)
- On startup in any mode, if `~/.amux/` exists and `~/.awman/` does not, emit a one-time migration notice and rename the directory (do not silently clobber an existing `~/.awman/`). Same for repo-local `$GITROOT/.amux` -> `$GITROOT/.awman`. This should be two simple detect-and-rename with info messages logged. It should happen in main.rs before any CLI/TUI/API logic starts. Do not offer or ask, just do it.

### Layer 1: Engine (`src/engine/`)
- Rename any log strings, tracing spans, or internal labels that refer to "amux" or "headless" to "awman" and "api" respectively
- No structural changes required at this layer

### Layer 2: Command (`src/command/`)
- Rename the `Headless` command variant in `CommandCatalogue` to `Api` (or `ApiServer` if needed for clarity)
- Rename `HeadlessCommand` struct to `ApiServerCommand`
- Update all `FrontendVisibility` annotations and catalogue entries that reference "headless" naming
- Update all help strings and usage text passed to frontend builders to use "awman" and "API mode"
- The dispatch-internal canonical command list must reflect `api` as the frontend name, not `headless`

### Layer 3: Frontend (`src/frontend/`)
- Rename `src/frontend/headless/` directory to `src/frontend/api/`
- Rename `HeadlessFrontend` struct to `ApiFrontend`; rename all associated impl blocks, trait impls, and type aliases
- Update Axum server startup logs and any HTTP response bodies that mention "amux" or "headless"
- The `ApiFrontend` still implements the same `DispatchFrontend` supertrait — this is a pure rename, no logic change
- Update any TLS certificate subject names or API key environment variable references

### Layer 4: Binary (`src/main.rs`)
- Update the binary name in `Cargo.toml` from `amux` to `awman`
- Update `[[bin]]` section name
- Update any `match subcommand_name()` arms that reference "headless" to reference "api"
- The binary entrypoint itself needs no logic changes

### Cargo Workspace
- Rename the workspace root package from `amux` to `awman` in `Cargo.toml`
- Rename any internal sub-crate names if they include "amux" in their package name
- The lib crate name (used in `use amux::...` imports) must be updated to `awman` — update all `use amux::` import paths throughout the codebase

### Documentation & README
- Rename `docs/08-headless-mode.md` to `docs/08-api-mode.md`; update all content within to use "API mode" terminology
- Update `docs/09-remote-mode.md` to reflect `awman` binary name
- Update all other `docs/` files replacing `amux` with `awman` in user-facing text
- Update `README.md` to reflect the new name, tagline, and install instructions
- Update `docs/10-architecture-overview.md` to name the three frontends as CLI, TUI, and API
- Update `aspec/` files that reference "amux" binary or "headless" mode naming (do not alter architecture decisions, only names)

### Configuration File Naming
- As noted above, the repo-config file path is now `$GITROOT/.awman/config.json`
- Update all references to this filename in Layer 0 path resolution and in documentation
- Detect-and-rename of `$GITROOT/.amux/` to `$GITROOT/.awman/` as desribed above.


## Edge Case Considerations

- **Migration collision**: If a user has both `~/.amux/` and `~/.awman/` directories (e.g. they installed a pre-release), do not overwrite `~/.awman/`. Log a warning explaining both directories exist and that `~/.amux/` was not migrated.
- **Env var backward compat**: `AMUX_*` env vars are silently ignored after rename. Add a startup check: if any `AMUX_*` env vars are set, emit a deprecation warning naming the new `AWMAN_*` equivalents. Do not silently accept old env vars.
- **Binary symlinks**: Document in README that any existing `amux` symlinks or PATH aliases will need to be updated.
- **Cargo crate name**: The Rust crate name `amux` appearing in `use amux::...` must be globally replaced — use `cargo fix` or a project-wide `sed` as a starting point, but verify every occurrence manually.
- **TLS certificate CN**: The self-signed cert generated for the API server may embed the old name — update the subject CN to `awman-api`.
- **Test fixtures**: All test fixtures, golden files, and snapshot strings that embed "amux" or "headless" as mode names must be updated.


## Test Considerations

- **Binary name smoke test**: The compiled binary is named `awman`; invoking `awman --help` succeeds and does not contain the string "amux" in any user-facing output.
- **Config path test**: `GlobalConfig::default_path()` returns a path containing `.awman`, not `.amux`.
- **Repo config test**: `RepoConfig::default_path()` returns a path containing `.awmman/...`.
- **Env var test**: Setting `AMUX_API_KEY=x` at startup emits a deprecation warning; setting `AWMAN_API_KEY=x` is read correctly.
- **API server label test**: The Axum server startup log line contains "awman" and "API mode", not "amux" or "headless".
- **Migration test**: If `~/.amux/` exists and `~/.awman/` does not, migration runs and emits the expected notice.
- **No-regression parity test**: All existing CLI, TUI, and API parity tests pass after rename (only binary name and env var names change in test invocations).
- **Docs link test**: No broken internal links in `docs/` after file renames.


## Codebase Integration

- Follow the four-layer architecture from `aspec/architecture/2026-grand-architecture.md` strictly. Path constants and env var name constants live in Layer 0. Startup migration logic (checking for old dir, copying) lives in Layer 0 data init functions called by Layer 3 at server/CLI startup. No migration logic leaks into Layer 3 itself.
- The `CommandCatalogue` in Layer 2 is the single source of truth for all frontend mode names. Update it first; derive all other naming from it.
- Use `replace_all` globally for mechanical string replacements, then audit each changed file for correctness.
- Do not introduce any new logic or behavior. If a rename forces a decision (e.g. how to handle old config), open a follow-up work item rather than silently choosing one path.


## Documentation

After implementation:
- `docs/08-api-mode.md` — full rewrite of headless-mode doc using "API mode" terminology
- All other `docs/` files updated for `awman` binary name
- `docs/07-configuration.md` updated for new config paths (`~/.awman/`, `.awman.json`)
- `README.md` updated with new name, tagline, and install path (`/usr/local/bin/awman`)
