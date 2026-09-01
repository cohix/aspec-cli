# Work Item: Feature

Title: clean command
Issue: issuelink

## Summary:
- Add a new top-level subcommand `awman clean` that removes Docker containers, workflow data files, and stale images left behind by previous awman runs
- Targets four categories: stopped awman containers, completed workflow files in the current repo, completed workflow context files in `~/.awman/`, and dangling awman container images
- Before deleting anything, display a full itemized list of what will be removed and require explicit user confirmation (TUI modal, CLI stdin prompt)

## User Stories

### User Story 1:
As a: user

I want to: run `awman clean` and see a list of all Docker containers, workflow files, and stale images awman has left behind

So I can: understand my local cleanup scope before committing to any deletion

### User Story 2:
As a: user

I want to: confirm or cancel the deletion after reviewing what will be removed, and have the default be safe (no action) when stdin is not a TTY

So I can: avoid accidentally deleting artifacts I still need, and safely use `awman clean` in scripts by passing `--yes` explicitly

### User Story 3:
As a: user

I want to: clean up stale awman data even when some categories fail (e.g., Docker daemon is unreachable)

So I can: recover filesystem space from completed workflow context directories even if Docker is temporarily unavailable


## Implementation Details:

### Flags
- `--yes` / `-y`: skip confirmation prompt (for scripting); when absent, always prompt
- `--dry-run`: enumerate and display what would be deleted without deleting anything (implies no prompt)

### Discovery Phase
Collect all deletable items across four categories before showing anything to the user:

1. **Stopped containers** — query Docker for all containers with `label=awman=true` AND status `exited` or `dead`; also catch legacy containers with `name=awman-` prefix using the same two-query deduplicated approach already used in `docker.rs`
2. **Repo workflow directories** — enumerate subdirectories of `<git_root>/.awman/workflows/`; include only those whose state file indicates a terminal status (completed, failed, or cancelled)
3. **Global context directories** — enumerate per-invocation directories under `~/.awman/context/workflows/{uuid}/`; include only those whose corresponding workflow is in a terminal state (cross-reference with global workflow registry or presence of a `completed` marker file)
4. **Dangling awman images** — query Docker for images with `label=awman=true` and `dangling=true` (images superseded by a newer build of the same tag)

If Docker is unreachable, log a warning and skip categories 1 and 4; still process filesystem categories 2 and 3.

### Presentation Phase
Display a structured summary grouped by category. Each entry should show the container name/image ID/directory path and a human-readable size or timestamp where available. If a category has zero items, omit it from the list. If all categories are empty, print "Nothing to clean." and exit successfully.

### Confirmation Phase
- **TUI**: send `DialogRequest::YesNo { title: "Confirm clean", body: <summary> }` and proceed only on `DialogResponse::Yes`
- **CLI**: print the itemized list to stdout, then prompt `"Delete the above? [y/N]: "` and read a line from stdin; any input other than `y` or `Y` is treated as No; if stdin is not a TTY and `--yes` was not passed, abort with an error message
- **API**: not applicable — `awman clean` is blocked at the catalogue layer (`api_allowed: false`) and will never reach command dispatch via the API frontend

### Deletion Phase
Execute in order to avoid leaving orphaned images:
1. Remove stopped containers (`docker rm <id>`)
2. Delete repo workflow directories (`fs::remove_dir_all`)
3. Delete global context directories (`fs::remove_dir_all`)
4. Remove dangling images (`docker rmi <id>`)

Treat each item independently — a failure on one should be logged and counted, but deletion continues for the rest. Report a summary at the end: "Deleted N items. M errors." and exit with a non-zero code if any deletion failed.

### Completed Workflow Detection
A workflow directory is considered complete when its state file (e.g., `state.json` or equivalent terminal-state marker inside the directory) contains a terminal status value. Reuse or expose the existing state-reading logic from the workflow engine rather than duplicating it. Directories without a readable state file should be skipped with a warning, not silently deleted.


## Edge Case Considerations:
- **Nothing to clean**: exit with a success message and code 0; do not show a confirmation prompt
- **Docker daemon unavailable**: warn and skip container/image categories; proceed with filesystem cleanup; exit non-zero only if filesystem deletions also fail
- **Running containers**: never touch containers in a running or paused state, regardless of name/label; only target `exited` and `dead` statuses
- **Concurrent awman instance**: a container that transitions from stopped to running between discovery and deletion should produce a Docker error; treat this as a per-item failure (log it, continue)
- **Non-TTY stdin without `--yes`**: abort with a clear error ("stdin is not a TTY; use --yes to confirm non-interactively") to prevent silent no-ops in scripts
- **`~/.awman/` does not exist**: skip global context category gracefully
- **No git root**: skip repo workflow category with a warning (command is being run outside a repo)
- **Workflow directory missing state file**: skip that directory (do not delete), emit a per-item warning
- **Partial directory removal failure** (e.g., permission error): log the error and continue; count as a failure in the summary
- **Image still referenced by a stopped container**: Docker will refuse `docker rmi`; treat as a per-item failure and report it — do not force-remove
- **`--dry-run` with `--yes`**: `--yes` is silently ignored; dry-run always skips deletion


## Test Considerations:
- **Unit — discovery**: for each category, test that the discovery function returns the correct items given a mock Docker response / mock filesystem layout; test that running containers are excluded; test that non-terminal workflow directories are excluded
- **Unit — confirmation frontend (CLI)**: test that `confirm_deletion()` returns `Yes` when stdin contains `"y\n"`, returns `No` for any other input, returns `No` (or errors) when stdin is not a TTY and `--yes` is absent, and returns `Yes` immediately when `--yes` flag is set
- **Unit — confirmation frontend (TUI)**: test that `DialogRequest::YesNo` is sent with the correct title/body and that `DialogResponse::No` aborts deletion
- **Unit — deletion ordering**: test that containers are removed before images, so image removal is not blocked by container references
- **Integration — Docker-unavailable path**: mock the Docker backend to return a connection error; assert categories 1 and 4 are skipped with a warning and filesystem cleanup still runs
- **Integration — partial failure**: mock one `docker rm` call to fail; assert the remaining items are still deleted and the exit code is non-zero
- **Integration — empty state**: when all four discovery functions return empty, assert no confirmation prompt is shown and exit code is 0
- **End-to-end — dry-run**: run `awman clean --dry-run` against a repo with known stale artifacts; assert no files or containers are removed and the output lists the expected items
- **End-to-end — full flow**: create a stopped awman container, a completed workflow directory, and a dangling awman image in a test environment; run `awman clean --yes`; assert all three are removed and exit code is 0


## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's aspec
- **Catalogue**: add `const CLEAN: CommandSpec` to `src/command/dispatch/catalogue.rs` with `--yes` and `--dry-run` flag specs, `api_allowed: false` (enforced by `Catalogue::is_allowed_for_frontend()`— no API frontend implementation needed); add `&CLEAN` to `ROOT.subcommands`
- **Command struct**: implement in `src/command/commands/clean.rs`; define `CleanFlags`, `CleanOutcome`, `CleanCommandFrontend` trait (extending `UserMessageSink`) with a `confirm_deletion(summary: &CleanSummary) -> Result<bool, CommandError>` method and a `report_results(result: &CleanResult)` method
- **Dispatch**: add a `["clean"]` match arm in `Dispatch::build_command()` in `src/command/dispatch/mod.rs`; add `BuiltCommand::Clean(CleanCommand)` variant and corresponding `CommandOutcome::Clean(CleanOutcome)` variant
- **Docker interaction**: use `ContainerRuntime` / `DockerBackend` for container and image listing; add a `list_stopped()` method and `list_dangling_images()` method to the backend trait alongside the existing `list()` method; use the label filter `awman=true` combined with a status filter
- **Filesystem paths**: resolve workflow and context directories via `WorkflowDirs` and `ContextDirResolver` in `src/data/fs/`; do not hardcode paths
- **CLI frontend**: implement `CleanCommandFrontend` for the CLI frontend in `src/frontend/cli/command_frontend.rs`; read from `std::io::stdin()` and check `atty::is(atty::Stream::Stdin)` before prompting
- **TUI frontend**: implement `CleanCommandFrontend` in `src/frontend/tui/per_command/clean.rs`; send `DialogRequest::YesNo` via the existing dialog channel and await `DialogResponse`
- **No API frontend**: do not implement `CleanCommandFrontend` for the API frontend; the `api_allowed: false` flag on the `CommandSpec` causes `Catalogue::is_allowed_for_frontend()` to reject the route before dispatch, so the command is unreachable via API by design

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** (e.g., if implementing headless features, update `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0123 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.
