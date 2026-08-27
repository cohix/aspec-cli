# Cleaning Up

awman creates and manages various resources — Docker containers, workflow data files, database migration backups, and images — as you use the tool. Over time, completed workflows leave behind data that can accumulate. The `awman clean` command safely removes these resources.

---

## What gets removed

`awman clean` targets five categories of resources:

1. **Stopped containers** — Docker containers from previous awman runs that have exited or died. These typically remain on disk even after they complete, and can accumulate quickly if you run many workflows.

2. **Completed repo workflow files** — Workflow state files in `<git_root>/.awman/workflows/` that have finished (all steps succeeded, failed, skipped, or were cancelled). In-progress workflows are left untouched.

3. **Completed workflow context directories** — Per-invocation context directories under `~/.awman/context/workflows/` that belong to terminal (completed) workflows. These contain logs, temporary files, and other state from the workflow run. Directories are considered complete when their UUID matches a terminal workflow in the current repo, or when they contain a `completed` marker file.

4. **Dangling images** — Awman-labeled Docker images that Docker reports as dangling, usually because a newer build replaced the same tag. If Docker refuses to remove an image because a container still references it, awman reports that item as a deletion error and continues.

5. **Pre-migration database backup** — The retained `awman.db.pre-migration` backup from an automatic database migration (and any matching SQLite sidecar backups). The live database at `~/.awman/data/awman.db` is never removed or modified by `awman clean`.

---

## Running the command

The simplest form lists what would be removed and prompts for confirmation:

```bash
awman clean
```

This displays an itemized list of all removable items grouped by category, then waits for your confirmation. The prompt respects your TTY status: if stdin is not a terminal (e.g., in a script or pipe), the command will abort unless you pass `--yes`.

---

## Flags

### `--yes`, `-y`

Skip the confirmation prompt and delete immediately. Use in scripts and automation:

```bash
awman clean --yes
```

Without this flag, `awman clean` always prompts for confirmation when stdin is a TTY. In non-TTY contexts (CI/CD, shell scripts without interactive input), the command aborts unless `--yes` is passed, preventing silent deletion in automated contexts.

### `--dry-run`

List what would be removed without deleting anything. Useful for previewing the impact:

```bash
awman clean --dry-run
```

When `--dry-run` is set, the command displays the full itemized list and reports the count of items that would be deleted. No deletion occurs, and no confirmation is needed.

---

## Behavior in different contexts

### Interactive terminal

When you run `awman clean` in an interactive terminal:

```
Stopped containers (2):
  - awman-abc123 (abc123def456)
  - awman-test-1 (def456ghi789)

Completed repo workflow files (1):
  - repohash8-workflow-name.json

Dangling images (1):
  - sha256:abc123de (awman:latest) 25MB

Delete the above? [y/N]: 
```

Type `y` or `Y` to proceed, or anything else (or press Enter) to abort. After deletion:

```
Deleted 4 items. 0 errors.
```

### Non-TTY / scripting contexts

Without `--yes`, the command aborts:

```bash
awman clean </dev/null
# error: interactive input unavailable (prompt: yes)
# exit code: 2
```

Pass `--yes` to proceed:

```bash
awman clean --yes
# Deleted 4 items. 0 errors.
# exit code: 0
```

Deletions that fail are counted and reported. If any deletions failed, the exit code is 1:

```bash
awman clean --yes
# Deleted 3 items. 1 errors.
#   /some/path: permission denied
# exit code: 1
```

### Nothing to clean

If no items match the deletion criteria:

```bash
awman clean
# Nothing to clean.
# exit code: 0
```

No confirmation prompt is shown; the command exits immediately.

### Docker unavailable

If the Docker daemon is unreachable or not configured, container and image categories are skipped with a warning, but filesystem cleanup (completed workflow files and context directories) still runs:

```bash
awman clean
# clean: container runtime unavailable; skipping container and image cleanup
# 
# Completed repo workflow files (2):
#   - repohash8-workflow-1.json
#   - repohash8-workflow-2.json
# 
# Delete the above? [y/N]: y
# Deleted 2 items. 0 errors.
```

The exit code is 0 if all filesystem deletions succeed.

---

## TUI behavior

When you run `awman clean` inside the TUI (e.g., via a command session), a modal dialog appears instead of a terminal prompt:

```
┌─────────────────────────────────────────┐
│         Confirm clean                   │
├─────────────────────────────────────────┤
│                                         │
│ Stopped containers (1):                 │
│   - awman-session (abc123def456)        │
│                                         │
│ Completed repo workflow files (2):      │
│   - repohash8-wf-1.json                 │
│   - repohash8-wf-2.json                 │
│                                         │
│                      [Yes]    [No]      │
└─────────────────────────────────────────┘
```

Select `[Yes]` to proceed or `[No]` to abort. The `--yes` flag also skips this dialog in the TUI.

---

## Exit codes

| Exit code | Meaning |
|-----------|---------|
| 0 | Deletion succeeded (or nothing to clean, or dry-run). |
| 1 | One or more deletions failed. Summary and per-item errors are reported. |
| 2 | Interactive input was required but unavailable (e.g., non-TTY without `--yes`). |

---

## Safety and design

`awman clean` is conservative by design:

- **Confirmation always required** (unless `--yes` or `--dry-run`). No silent deletions.
- **In-progress workflows are protected.** Only workflows that have reached a terminal state (completed, failed, or cancelled) are marked for deletion.
- **Per-item failures don't stop the process.** If one container fails to delete, the command continues and reports all failures at the end.
- **Context directories are verified.** A global context directory is only deleted if it matches a terminal workflow in the current repo or carries an explicit `completed` marker.
- **Docker-unavailable is not an error.** If the container runtime is unreachable, filesystem cleanup still runs, and the exit code is 0 if those deletions succeed.

---

## Common patterns

### Clean up after a batch of workflows

Run a few workflows, then clean up before committing:

```bash
awman exec workflow some-workflow.toml --yes
awman exec workflow another-workflow.toml --yes
awman clean --yes
```

### Preview before deleting in CI

Use `--dry-run` in CI logs to see what would be cleaned:

```bash
awman clean --dry-run
awman clean --yes
```

### Keep your machine tidy with a cron job

Schedule periodic cleanup:

```bash
0 2 * * * awman clean --yes >/dev/null 2>&1
```

This runs `awman clean` at 2 AM daily, deleting all completed resources without output.

### Dry-run in the TUI

Open a command session and type:

```
awman clean --dry-run
```

The command output lists what would be removed; no confirmation dialog is shown and nothing is deleted.

---

[← Dynamic Workflows](13-dynamic-workflows.md) · [Next: Parallel Workflows →](15-parallel-workflows.md)
