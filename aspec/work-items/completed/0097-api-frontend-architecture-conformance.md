# Work Item: Task

Title: API frontend architecture conformance — move parsing, validation, persistence, and flag policy out of Layer 3
Issue: issuelink

> **Architectural basis**: this work item was produced by an architecture review against
> `aspec/architecture/2026-grand-architecture.md`. Read that document in full before
> implementing, and adhere to its tenets — in particular Tenet 2 (frontends contain NO
> business logic) and the Dispatch rules in the Layer 2 section (the canonical list of
> commands, subcommands, flags, and argument shapes lives ONLY in the Dispatch package).

## Summary:
- The API frontend (`src/frontend/api/`) currently re-implements four categories of logic that the grand architecture reserves for lower layers. Each one is a mode-parity drift vector: a change to a command's shape or validation rules in Layer 2 will not automatically be reflected in API mode.
- Finding A — `parse_args_to_flags()` in `src/frontend/api/command_frontend.rs` (~lines 192–313) hand-rolls flag parsing, heuristic type coercion, and a hardcoded `match subcommand` block mapping positionals for `"exec prompt"`, `"exec workflow"`, `"specs amend"`, `"config get"`, `"config set"`, `"remote exec workflow"`, `"remote exec prompt"`, `"remote session kill"`.
- Finding B — `handle_create_session` in `src/frontend/api/routes.rs` (~lines 302–402) and `handle_create_command` (~lines 1302–1357) perform request validation (session type, workdir allowlist, repo-URL scheme), remote-clone path planning, and session state-transition checks directly in route handlers.
- Finding C — the API frontend performs persistence directly (`persist_setup_state` writing `setup_state.json` at `routes.rs` ~937–945, plus scattered `tokio::fs::create_dir_all` / `tokio::fs::write` calls in `routes.rs`).
- Finding D — `command_frontend.rs` (~lines 315–323) unconditionally forces `non-interactive=true` and `yolo=true` for every API command; a flag-default policy decision embedded in Layer 3.
- The fix in all four cases is the same shape: move the logic into the catalogue/Dispatch (Layer 2) or data/storage types (Layer 0), and reduce the API frontend to its spec-mandated job — "translate the lower-level package's functionality into an HTTP-powered API."

## User Stories

### User Story 1:
As a: developer adding or changing an awman command

I want to: define the command's flags, positionals, and types once in the `CommandCatalogue`

So I can: have CLI, TUI, and API modes all parse and validate it identically without touching frontend code

### User Story 2:
As a: user of the API mode

I want to: receive the same validation behavior and errors as the CLI for equivalent inputs

So I can: trust that scripting against the API is functionally identical to scripting against the CLI

### User Story 3:
As a: developer implementing a future frontend (desktop app, k8s operator, editor extension)

I want to: reuse session-creation validation (workdir allowlist, repo-URL scheme checks) from a lower layer

So I can: get the security-relevant checks for free instead of re-implementing them per frontend

## Implementation Details:

### A. Catalogue-driven raw-args parsing (replaces `parse_args_to_flags`)
- Add a third projection to `src/command/dispatch/projections/` alongside the existing clap
  (`projections/clap.rs`) and TUI hints (`projections/tui_hints.rs`) projections, e.g.
  `projections/raw_args.rs` exposing
  `CommandCatalogue::parse_raw_args(path: &[&str], args: &[String]) -> Result<ParsedArgs, CommandError>`.
- The projection must derive everything from the canonical `CommandSpec` data: flag names,
  flag value types (bool / string / repeated string / path / enum / u16 / usize), and
  positional-argument names and arity. No per-command `match` arms — if a command's spec
  says its first positional is `workflow` of type path, the projection maps it accordingly.
- Type coercion becomes spec-driven, not heuristic: a value is parsed as the type the
  catalogue declares for that flag, and a type mismatch is a structured parse error
  (mirroring how clap rejects it in CLI mode). The current behavior — storing a numeric
  value in `u16s`+`usizes`+`strings` and a non-numeric one in four maps at once — is removed.
- `--` separator and `--flag=value` / `--flag value` forms must behave identically to the
  clap projection. Add a parity test (see Test Considerations).
- Delete `parse_args_to_flags()` from `src/frontend/api/command_frontend.rs`; the API
  frontend hands the raw strings from the HTTP request straight to the new catalogue method.

### B. Session-creation validation and planning moves to Layer 2
- Introduce a Layer 2 type in `src/command/` (e.g. `session_create.rs`):
  `SessionCreateRequest { session_type, workdir, repo_url, branch }` →
  `SessionCreateRequest::validate(&self, policy: &SessionCreatePolicy) -> Result<SessionCreatePlan, CommandError>`.
  - `SessionCreatePolicy` carries the workdir allowlist (currently `state.workdirs`) and the
    permitted URL schemes.
  - `SessionCreatePlan` carries the resolved workdir, optional clone destination, repo URL,
    and branch — the same tuple the route handler computes today.
- Validation moved verbatim from `routes.rs`: session_type must be `local`|`remote`;
  `local` requires a canonicalizable workdir on the allowlist; `remote` requires a
  non-empty repo_url with scheme in {http, https, git@, ssh, git}; `file:` and unknown
  schemes rejected. Error variants must be typed so the route handler can map them to
  400 vs 403 without inspecting strings.
- Session state-transition validation in `handle_create_command` (active/closing/closed)
  moves to a method on the session/store type it guards, not the route handler.
- The route handlers become: deserialize body → build `SessionCreateRequest` → call the
  Layer 2 API → map typed result/error to `StatusCode` + JSON. HTTP-specific concerns
  (status codes, JSON envelope) are the ONLY logic that remains in `routes.rs`.

### C. Persistence moves to Layer 0
- Add methods to the Layer 0 session-storage surface (wherever `state.paths.session_dir()`
  is defined, or the session store type) covering every direct filesystem call currently in
  the API frontend:
  - `save_setup_state(session_id, &SessionSetupState)` (replaces `persist_setup_state`'s
    `tokio::fs::write` of `setup_state.json`)
  - `prepare_session_dirs(session_id)` creating `jobs/`, `commands/` (legacy), `worktree/`,
    `agent-settings/` (replaces the `create_dir_all` block in `handle_create_session`)
- Sweep `src/frontend/api/` for any remaining `tokio::fs` / `std::fs` usage after the
  extraction; the end state is zero direct filesystem calls in the API frontend package.

### D. Declarative per-frontend flag defaults in the catalogue
- Extend the catalogue's flag/command spec with a declarative frontend-defaults mechanism —
  e.g. a `FrontendProfile::Api` resolution step or per-flag `api_default: Option<...>` —
  so that Dispatch, not the frontend, decides that API-dispatched commands run with
  `non-interactive=true` (technical requirement: no TTY is attached to HTTP workers; on
  Apple's `container` CLI a PTY request with piped stdin fails with `ENOTTY`).
- **Developer decision required before implementation** (per the grand architecture doc:
  ask, do not assume): should `yolo=true` remain FORCED for API callers, become a
  default that request payloads can override, or become opt-in? It is a behavioral policy
  with security implications (auto-approval of all agent actions) that is currently an
  unadvertised side effect. Implement whichever the developer chooses — but implement it
  in the catalogue, visible next to the flag definition, either way.
- Remove the hardcoded `bools.insert("non-interactive", true)` / `bools.insert("yolo", true)`
  from `src/frontend/api/command_frontend.rs`.

## Edge Case Considerations:
- **Parse parity with clap**: `--flag=value` vs `--flag value`, repeated flags, values that
  look like flags after `--`, negative numbers as values — the raw-args projection must
  match clap's interpretation for every command in the catalogue.
- **Unknown flags/commands over the API**: must produce a structured 400-class error from
  the catalogue parse, not a silent drop into the generic string map (current behavior
  accepts any `--anything`).
- **Prompt joining**: `exec prompt` currently joins all positionals with spaces; the
  catalogue spec must express "greedy trailing positional" so this behavior is preserved
  spec-driven rather than special-cased.
- **Workdir allowlist with symlinks**: canonicalize before comparing against the allowlist
  (current behavior via `std::fs::canonicalize`) — keep this inside the Layer 2 validator
  so no frontend can skip it.
- **Backward-compatible API responses**: existing API clients must see the same status
  codes and error shapes for the same invalid inputs; changes to error text are
  acceptable, changes to status codes are not.
- **`FrontendProfile` leakage**: TUI/CLI behavior must be provably unchanged — the API
  defaults mechanism must not alter flag resolution for the other frontends.

## Test Considerations:
- **Unit — raw-args projection**: for every command in the catalogue, feed a representative
  argv through both `build_clap_command()` and `parse_raw_args()` and assert identical
  typed results (a catalogue-iterating parity test, so new commands are covered
  automatically).
- **Unit — type coercion errors**: non-numeric value for a u16 flag, bad enum value, and
  unknown flag each produce the expected typed error.
- **Unit — `SessionCreateRequest::validate`**: table-driven cases for local (missing
  workdir, non-canonicalizable path, path off allowlist, symlink resolving onto/off the
  allowlist) and remote (missing/empty repo_url, each accepted scheme, `file://` and
  bare-path rejection), asserting typed error variants.
- **Unit — Layer 0 storage methods**: `save_setup_state` round-trips JSON;
  `prepare_session_dirs` creates the full directory set idempotently.
- **Integration — API route parity**: HTTP requests exercising each validation failure
  return the same status codes as before the refactor (regression-pin the current codes
  first).
- **Integration — flag defaults**: a command dispatched via the API resolves
  `non-interactive=true` (and the decided `yolo` behavior) via the catalogue; the same
  command via CLI dispatch is unaffected.
- **End-to-end**: `awman api start` + create-session + exec-prompt happy path behaves
  identically pre/post refactor.
- **Architecture guard**: add/extend a test asserting `src/frontend/api/` contains no
  direct `std::fs`/`tokio::fs` calls and no per-command match arms (e.g. a source-scan
  test or clippy-style lint in CI, matching however other layer rules are enforced).

## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from
  the project's aspec — in particular `aspec/architecture/2026-grand-architecture.md`
  (Tenet 2, Tenet 3, and the Dispatch single-source-of-truth rules).
- New projection lives beside the existing ones in `src/command/dispatch/projections/` and
  is generated from the same canonical `CommandCatalogue` structures — never a parallel
  list.
- Layer 2 validation types follow the existing command-object pattern (typed request →
  typed plan/outcome, `CommandError` variants for failures).
- Layer 0 storage additions follow the existing patterns in `src/data/fs/`.
- No changes to CLI or TUI behavior; this is an API-frontend + Layer 0/2 refactor.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** — if API-mode error responses or the `yolo` default
  behavior change user-visibly, update the API/headless docs (e.g. `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it**
- **Never create work-item-specific docs**
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.
