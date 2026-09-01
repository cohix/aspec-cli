# Work Item: Feature

Title: Git Sidebar
Issue: issuelink

## Summary:
- The awman TUI gains a collapsible git sidebar toggled with Ctrl-G. When closed, a compact `+X -Y` diff summary appears in the bottom status bar (far right). When open, the sidebar occupies at most 1/4 of horizontal screen space, the execution window and container window shrink to fill the remaining 3/4, and the sidebar displays per-file change stats with color-coded entries (green=added, red=deleted, blue=modified). The sidebar is a rounded-rect block with a green border. Both the per-file list and summary totals update continuously as agents write files.

## User Stories

### User Story 1:
As a: user

I want to:
see a compact `+X -Y` line count summary in the status bar while an agent is running

So I can:
monitor how much code is being written without interrupting my view of the execution output

### User Story 2:
As a: user

I want to:
press Ctrl-G to open the git sidebar and see every changed file with its individual `+/-` counts, color-coded by change type

So I can:
quickly audit what the agent has touched across the worktree without opening a separate terminal

### User Story 3:
As a: user

I want to:
see the sidebar and status bar summary update automatically as the agent edits files

So I can:
observe progress in real time without polling or manually refreshing

## Implementation Details:

### New module: `src/frontend/tui/git_sidebar.rs`

Define shared data types used across render and polling:

```rust
pub enum GitFileChangeType { Added, Modified, Deleted }

pub struct GitFileEntry {
    pub path: String,
    pub change_type: GitFileChangeType,
    pub additions: u32,
    pub deletions: u32,
}

pub struct GitDiffSummary {
    pub files: Vec<GitFileEntry>,
    pub total_additions: u32,
    pub total_deletions: u32,
}

pub type SharedGitDiffSummary = Arc<Mutex<Option<GitDiffSummary>>>;
```

Also define the sidebar open/close state:

```rust
pub enum GitSidebarState { Open, Closed }
```

### Background polling task

Add `start_git_diff_poll_task(root: PathBuf, summary: SharedGitDiffSummary, cancel: CancellationToken) -> JoinHandle<()>` in `git_sidebar.rs`.

- Runs in a `tokio::spawn` loop, sleeping ~2 seconds between iterations.
- On each tick, run two git subcommands against `root`:
  1. `git status --porcelain` — determines per-file change type (`??` = Added, `D` = Deleted, everything else = Modified).
  2. `git diff --numstat HEAD` — provides per-file `additions\tdeletions\tpath` for tracked changes. For untracked files (`??` in porcelain), count lines with a plain file read (or just mark as `+N -0` where N is the line count).
- Parse results into `GitDiffSummary`, lock the shared mutex, and replace the value.
- If either command fails (not a git repo, no commits yet, etc.), set the shared value to `None`.
- Cancellation via `CancellationToken` (consistent with other async tasks in the codebase).

### Tab struct changes (`src/frontend/tui/tabs.rs`)

Add two fields to the `Tab` struct:

```rust
pub git_sidebar_state: GitSidebarState,
pub git_diff_summary: SharedGitDiffSummary,
```

Initialize `git_sidebar_state` to `GitSidebarState::Closed` and `git_diff_summary` to `Arc::new(Mutex::new(None))` in `Tab::new()`.

Spawn the polling task after the tab is created, using `active_worktree_path` when set or the session's git root when not. When `active_worktree_path` changes (worktree lifecycle sets it), restart the polling task pointed at the new path so the sidebar reflects the tab's actual working directory.

### Keymap changes (`src/frontend/tui/keymap.rs`)

Add `ToggleGitSidebar` to the `Action` enum.

In `map_key()`, map `KeyCode::Char('g') + KeyModifiers::CONTROL` → `Action::ToggleGitSidebar` in all focus contexts including `ContainerMaximized`. Ctrl-G (0x07, BEL) is rarely used by terminal programs; intercept it before the PTY forwarding path in `handle_key_event()` so the toggle is handled at the TUI level and never forwarded to the PTY.

### Event handler changes (`src/frontend/tui/mod.rs`)

In `handle_key_event()`, handle `Action::ToggleGitSidebar`:

```rust
Action::ToggleGitSidebar => {
    let tab = self.current_tab_mut();
    tab.git_sidebar_state = match tab.git_sidebar_state {
        GitSidebarState::Open => GitSidebarState::Closed,
        GitSidebarState::Closed => GitSidebarState::Open,
    };
}
```

After toggling, compute the new container dimensions (the width available to the left chunk after subtracting the sidebar) and send a PTY resize event on `tab.resize_tx` so the running process reflows to the new width. The height is unchanged. This resize must happen even when `ContainerWindowState::Maximized` because the container occupies the full left chunk width, not the full frame width.

### Layout changes (`src/frontend/tui/render.rs`)

In `render_frame()`, after computing the main vertical layout chunks, check `tab.git_sidebar_state`:

- **Sidebar closed:** pass the full frame area to the existing vertical layout (no change).
- **Sidebar open:** first split the frame area horizontally:
  ```
  Layout::horizontal([
      Constraint::Fill(1),       // left: execution + container (≥75% of width)
      Constraint::Max(area.width / 4),  // right: git sidebar (≤25%)
  ])
  ```
  Pass `chunks[0]` to the existing vertical layout (execution/container windows shrink naturally). Pass `chunks[1]` to `render_git_sidebar()`.

Add `render_git_sidebar(f, area, summary: &Option<GitDiffSummary>)`:
- Draw a `Block` with `BorderType::Rounded`, `Borders::ALL`, border style `Color::Green`.
- Title line inside the block: `+{total_additions} -{total_deletions}` in bold.
- Below the title, render a `List` widget. Each `ListItem` is a `Line` composed of:
  - A fixed-width `+A -D` span in the file's accent color.
  - A filename span (truncated with `…` if wider than the block's inner width minus the stat prefix).
  - Color mapping: `Added` → `Color::Green`, `Deleted` → `Color::Red`, `Modified` → `Color::Blue`.
- If `summary` is `None`, show a single dimmed line: `"no git data"`.

### Status bar changes (`src/frontend/tui/render.rs`)

In `render_status_bar()`, when `tab.git_sidebar_state == GitSidebarState::Closed`:
- Lock `tab.git_diff_summary`.
- If the summary is `Some`, append a right-aligned `+{total_additions} -{total_deletions}` span to the status bar line (green `+`, red `-`).
- Use `Span` alignment or a `Table`/`Line` with a right-justified segment to push the stat to the far right of the 1-row area.

## Edge Case Considerations:
- **Not a git repo or no commits:** `git diff --numstat HEAD` fails if there are no commits. Fall back to `git status --porcelain` only, treating every file as Added with 0 line counts. If even that fails, set `SharedGitDiffSummary` to `None` and show nothing in the sidebar/status bar.
- **Binary files:** `git diff --numstat` reports `-\t-\tpath` for binaries. Represent these as `+0 -0` and show `(binary)` suffix in the file list.
- **Untracked files (new, not yet staged):** Appear as `??` in porcelain. Read the file to count lines for the additions field; deletions = 0.
- **Deleted files:** Deletions come from `git diff --numstat HEAD`; the file no longer exists on disk so no read needed.
- **Very long paths:** Truncate at the inner block width minus the `+A -D` prefix length, appending `…`.
- **Many files:** If the file list exceeds the sidebar height, clip to visible rows. A scroll offset field on the Tab can be added in a follow-up; for now, show as many entries as fit.
- **Container window maximized:** When `ContainerWindowState::Maximized`, the container expands to fill the left chunk of the horizontal split, not the entire frame. The git sidebar still occupies its ≤25% right chunk regardless of container window state. The terminal PTY must receive a resize event (via `resize_tx`) reflecting the narrower left-chunk width so the running process reflows correctly — failure to resize will cause the PTY to render into a width it believes is wider than the actual area, producing line-wrap artifacts.
- **Tab with no worktree yet (Idle phase):** The polling task still runs against the git root, so the sidebar shows main-branch diffs correctly even before an agent starts.
- **Rapid file changes:** The 2-second poll interval is intentional to avoid hammering the disk and git index during heavy agent writes. Users see updates that lag by at most ~2 seconds.
- **Sidebar minimum width:** If 1/4 of the terminal width is less than ~20 columns, the sidebar is unusably narrow. Add a minimum threshold (e.g., 20 columns); if the terminal is too narrow, treat the sidebar as closed and show only the status bar summary.

## Test Considerations:
- **Unit tests for `git_sidebar.rs`:**
  - `parse_porcelain_status()`: verify `??` → Added, `D ` → Deleted, `M ` and ` M` → Modified.
  - `parse_numstat()`: verify correct parsing of `5\t2\tsrc/foo.rs`, binary `-\t-\timg.png`, and renamed `3\t1\t{old.rs => new.rs}`.
  - `build_summary()`: given combined porcelain + numstat output, verify totals and per-file entries.
  - Cancellation: confirm that the polling loop exits promptly when the token is cancelled.
- **Unit tests for keymap:**
  - `Ctrl+G` maps to `ToggleGitSidebar` in Idle, Running, and CommandBox contexts.
  - `Ctrl+G` maps to `ToggleGitSidebar` even when `FocusContext::ContainerMaximized`, and is NOT forwarded to the PTY.
- **Integration tests for Tab state:**
  - Toggling `ToggleGitSidebar` twice returns to `Closed`.
  - `GitSidebarState` is `Closed` on new tab creation.
- **Render tests (snapshot or rect checks):**
  - When sidebar is `Closed`, the existing vertical layout areas are unchanged.
  - When sidebar is `Open`, the horizontal split allocates ≤25% of width to the right chunk.
  - Status bar contains `+X -Y` text when sidebar is `Closed` and summary is `Some`.
  - Sidebar block uses `BorderType::Rounded` and green border style.
- **End-to-end / manual:**
  - Open a repo with staged and unstaged changes; verify colors and counts match `git diff --stat HEAD`.
  - Resize terminal to narrow width and confirm sidebar collapses gracefully.

## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- Use `Arc<Mutex<T>>` for `SharedGitDiffSummary` consistent with `SharedActiveWorktreePath` and `SharedStatusLog` patterns in `tabs.rs`.
- Spawn the polling `JoinHandle` alongside the other async tasks created in `Tab::new()` (or wherever the tab spawns its background tasks); store it on the `Tab` and abort it in the tab's `Drop` impl or teardown path.
- The horizontal layout split in `render_frame()` should be introduced with minimal refactoring: extract the "main vertical area" as a variable computed before the layout call, then conditionally replace it with the left chunk of a horizontal split. This avoids restructuring the existing 7-section vertical layout.
- Add `ToggleGitSidebar` to the `Action` enum in `keymap.rs` and document the new keybinding in the status bar hint strings rendered by `render_status_bar()` (e.g., append `· ctrl-g git` to the idle hint line).
- The git polling runs `git` as a subprocess via `std::process::Command` (async via `tokio::process::Command`), consistent with how `GitEngine` in `src/engine/git/mod.rs` invokes git. Reuse the `run_git_logged` helper or a simplified equivalent — do not shell out through a user-controlled string.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** — add a "Git Sidebar" section to the relevant TUI navigation/interface doc describing Ctrl-G, the status bar summary, and color conventions.
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-git-sidebar.md` as a short standalone guide if the feature is substantial enough).
- **Never create work-item-specific docs** (e.g., no "WI 0093 implementation guide" in published docs).
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`.
- **Docs are for end users**, not for developers trying to understand implementation.

See `CLAUDE.md` for more guidance on documentation standards.
