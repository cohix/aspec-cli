# Using the TUI

awman has two execution modes:

- **TUI mode** — run `awman` with no arguments to open the interactive terminal UI. This is the primary interface for ongoing agent work: it supports multiple simultaneous sessions, live tab state, and a full in-process terminal emulator for agent output.
- **Command mode** — run `awman <subcommand>` directly from your shell. It executes the command and exits. Useful for scripting, CI, or quick one-off actions.

This guide covers TUI mode.

---

## Startup

When you run `awman` with no arguments, the TUI opens immediately in an alternate terminal screen. What happens next depends on your environment:

**Inside a Git repository:**

The TUI runs `awman ready` automatically on the first tab. This checks that your container runtime is available, that `Dockerfile.dev` and `.awman/Dockerfile.{agent}` exist, and that your agent image is built. If anything needs attention, `ready` will guide you through it. Once `ready` passes, the TUI shows the welcome message and waits for your first command.

**Outside a Git repository:**

If the working directory is not inside a Git repository, the TUI runs `awman status --watch` instead, streaming a live status view. This is useful for monitoring an API server or checking the state of remote sessions. Most agent commands require a Git repo — navigate to one and open a new tab with **Ctrl+T**.

In both cases, terminal raw mode, alternate screen, and mouse capture are enabled on entry and restored unconditionally on exit, even if awman crashes.

---

## Layout

```
┌─ Tab 1: myproject ─────────┬─ Tab 2: myproject ──────────┐
│  exec workflow              │  chat                        │
└─────────────────────────────┴──────────────────────────────┘
┌─── ● running: exec workflow ───────────────────────────────┐
│ $ docker run --rm -it ...                                   │
│                                                             │
│  ╭─ 🔒 Claude Code (containerized) ── myproj | 5% | 200mb ─╮│
│  │                                                          ││
│  │  [agent output here]                                     ││
│  │                                                          ││
│  ╰──────────────────────────────────────────────────────────╯│
│                                                             │
│  Ctrl-M toggle · Ctrl-W workflow · Ctrl-, config            │
└─────────────────────────────────────────────────────────────┘
┌─── command ──────────────────────────────────────────────────┐
│ > _                                                           │
└───────────────────────────────────────────────────────────────┘
  init  ·  ready  ·  chat  ·  specs
```

The TUI is composed of three areas:

- **Tab bar** (top) — one entry per open session, with colour-coded state
- **Execution window** (middle) — shows command output; overlaid by the container window when an agent is running
- **Command box** (bottom) — where you type subcommands

---

## Git Sidebar

Press **Ctrl+G** to toggle the Git Sidebar — a live view of staged and unstaged changes in the current repository. The sidebar appears on the right side of the TUI and displays changed files with line change counts.

The sidebar's border always carries a title summarizing the repository state, like a condensed `git status`:

- `main: 3 changed` — current branch and the number of changed files
- `main: clean` — no uncommitted changes
- `HEAD: 2 changed` — detached HEAD state
- `git status` — no git data available (e.g. not a git repository)

```
┌─ Tab 1: myproject ──────────────┬─ +5 -3 ─┐
│  exec workflow                   │ ┌─ main: 5 changed ─┐
└──────────────────────────────────┴─│ +11 -10           │
┌─── ● running: exec workflow ────────┤ +3 -1 file.rs    │
│ $ docker run --rm -it ...           │ +0 -8 old.rs     │
│                                     │ +8 -0 new.rs     │
│  ╭─ 🔒 Claude Code (containerized)  │ +0 -1 config     │
│  │                                  │ +0 -0 (binary)   │
│  │  [agent output here]              ╰─────────────────┘
│  │                                  │
│  ╰──────────────────────────────────╯
│                                     │
│  Ctrl-M toggle · Ctrl-W workflow   │
└──────────────────────────────────────┘
```

### Opening and closing

When the sidebar is **open**, press **Ctrl+G** again to close it. The execution window expands to fill the space, and a compact `+X -Y` summary appears in the bottom status bar showing the total additions and deletions across all files.

When the sidebar is **closed**, the `+X -Y` summary is always visible on the right side of the status bar (when there are changes to display).

### File list

The sidebar displays each modified file with:
- The **line change counts** in the format `+additions -deletions`
- The **file path**, truncated with `…` if too long to fit

Binary files are marked with a `(binary)` suffix and show `+0 -0` line counts.

### Colors

Each file row is color-coded by the type of change:

- **Green** — added files
- **Red** — deleted files
- **Blue** — modified files

The `+` symbol in the summary is always green, and the `-` symbol is always red, regardless of the actual changes displayed.

### Narrow terminals

If your terminal is narrower than 80 columns, the sidebar may not have enough space to display usefully. In this case, the sidebar automatically treats itself as closed even if you toggle it open — the `+X -Y` summary still appears in the status bar, giving you a quick overview of changes on narrow screens.

### Refresh cadence

The sidebar updates every 2 seconds, reflecting any staged or unstaged changes in the repository. Changes are polled automatically — there's no manual refresh needed.

---

## The command box

The command box is where you interact with awman. Type any subcommand and press **Enter**.

| Key | Action |
|-----|--------|
| Type | Update input; suggestions appear below |
| **Enter** | Execute command |
| **Ctrl+Enter** or **Shift+Enter** | Insert a newline (multi-line input) |
| **← / →** | Move cursor within input; long input scrolls to keep cursor visible |
| **Ctrl+← / Ctrl+→** | Move cursor by word |
| **Home / End** | Move cursor to start / end of input |
| **↑** | Focus the execution window (for scrolling) |
| **Backspace / Delete** | Edit input |
| **Ctrl+Backspace** | Delete previous word |
| **Tab** | Cycle to next autocomplete suggestion |
| **Shift+Tab** | Cycle to previous autocomplete suggestion |
| **q** | Quit awman (when command box is empty and idle) |
| **Ctrl+C** | Close tab (multiple tabs) or open quit confirmation (single tab) |

### Input handling

The command box supports long inputs with automatic horizontal scrolling:

- When your input is longer than the visible width, the text scrolls automatically to keep the cursor in view
- You can move freely within the input using **← / →**, **Home**, and **End** — the visible portion scrolls to follow your cursor
- Multi-line inputs are supported via **Ctrl+Enter** or **Shift+Enter**; lines are joined with `↵` in the display

When the command box is empty and the tab is idle (no command running), you'll see a helpful ghost text: `q to quit`. This disappears as soon as you type.

### Autocomplete and suggestions

As you type, matching command completions appear in the suggestion row below the command box:

```
> chat · init · status
```

When you type a partial command, the list narrows. Use **Tab** / **Shift+Tab** to cycle through suggestions and fill them into the input. Every command available in `awman` is also available in the TUI command box. Both `--flag value` and `--flag=value` forms are accepted. For example:

```
chat --agent codex
chat --agent=codex
exec workflow path/to/workflow.toml --agent opencode --plan
```

Suggestions include flag hints from the command catalogue:

```
--yolo — enable auto-advance mode    --plan — read-only run
```

When a suggestion shows a file path (worktree or working directory), long paths are automatically truncated in the middle to fit the display:

```
Using Worktree: /home/user/my…/worktree-branch
```

### Context display

When the input is empty or there are no matching completions, the suggestion row shows contextual information instead:

```
CWD: /home/user/myproject
```

If a worktree is active for the session, it shows the worktree path:

```
Using Worktree: /home/user/myproject-worktree
```

If you type an unrecognised command, awman suggests the closest known one:

```
'exex' is not an awman command.  Did you mean: exec
```

### Quitting

Press **Ctrl+C** from the command box to open the quit confirmation dialog:

```
╭─── Quit awman? ───────────────────╮
│  Are you sure you want to quit?   │
│  [y/n]                            │
╰───────────────────────────────────╯
```

Press **y** to quit, **n** or **Esc** to cancel. With multiple tabs open, **Ctrl+C** instead shows a close-tab dialog:

```
╭─── Close tab? ──────────────────────────────╮
│  [q] Quit awman   [c] Close this tab   [n] Cancel │
╰──────────────────────────────────────────────╯
```

---

## The execution window

The execution window shows plain-text streaming output from commands — Docker build logs, status messages, error output. It is separate from the container window (see below).

### Scrolling

When the window is selected (press **↑** from the command box to select it):

| Key / Action | Effect |
|---|---|
| **↑ / ↓** | Scroll line by line |
| **PageUp / PageDown** | Scroll one full page |
| **b / e** | Jump to beginning / end |
| Mouse scroll | Scroll at any time |
| **Esc** | Return focus to command box |

### Selecting and copying text

Execution window text is mouse-selectable whenever the container window is hidden or minimized (when the container window is maximized, it owns text selection instead — see below):

1. **Click and drag** to highlight text (shown with inverted colors)
2. **Ctrl+Y** to copy the selection to clipboard

While a selection is active, a ` CTRL-Y to copy/yank text ` hint appears on the window's bottom border — the same flow as the container window. The selection captures the text as displayed when you started dragging, so output arriving mid-selection doesn't change what gets copied. Toggling the container window with **Ctrl+M** clears any active selection.

### Status log

awman itself writes informational messages — not agent output, but messages from awman about what it is doing — into a per-tab **status log**. Examples include "container started", "worktree created", "auth token accepted", and error messages from failed commands.

The status log appears in the execution window. By default it is **collapsed**: only the most recent message is shown as a single line at the bottom of the output area.

Press **l** (lowercase L) while the execution window is focused to toggle between collapsed and expanded view. In expanded view the full message history is visible and scrollable, with color-coded level prefixes:

| Level | Colour |
|-------|--------|
| Info | Dark gray |
| Warning | Yellow |
| Error | Red |
| Success | Green |

The status log is per-tab and accumulates for the lifetime of the session. It does not include agent output (that lives in the container window's scrollback).

### Crash log

If awman hits an internal error (a panic) while the TUI is running, the full error and backtrace are appended to `$HOME/.awman/panic.log`. When a running command dies this way, its tab shows an error in the status log pointing at that file — include its contents when filing a bug report. A crash on the TUI thread itself restores your terminal to a usable state before the error is printed.

### Border colours

| Colour | Meaning |
|--------|---------|
| Blue | Running (selected) |
| Grey | Running (unselected) or idle |
| Green | Completed successfully |
| Red | Completed with error |

---

## The container window

Whenever awman launches a container to run a code agent, a **container window** appears overlaying the execution window. This window contains a full terminal emulator — all keyboard input, ANSI colour codes, cursor movement, and interactive TUI apps (like Claude Code's own UI) work exactly as they would in a real terminal.

```
╭─ 🔒 Claude Code (containerized) ── myproject | 5% | 200mb ──╮
│                                                               │
│  [agent output — full terminal emulation]                    │
│                                                               │
╰───────────────────────────────────────────────────────────────╯
  Ctrl-M toggle  ·  scroll ↕ history  ·  drag select  ·  Ctrl+Y copy
```

The title bar shows the container name, live CPU usage, memory, and total runtime. Stats are polled from the container runtime every 5 seconds.

### Keyboard and mouse

When the container window is visible and maximized, almost all keyboard input is forwarded to the agent:

| Key / Action | Effect |
|---|---|
| Type | Sent directly to the agent |
| **Esc** | Forwarded to the agent (`\x1b`) — for vim, fzf, REPLs, and other interactive programs |
| **Tab / Shift+Tab** | Forwarded to the agent |
| **Ctrl+M** | Toggle: minimize the container window (agent keeps running) |
| Mouse scroll | Sent to agent (if it has mouse tracking enabled), or scroll terminal scrollback (5 lines per tick) |
| Mouse drag | Select text (highlighted with inverted colours) — always under awman's control |
| **Ctrl+Y** | Copy the current selection to clipboard (ANSI stripped) |

> **Note on Ctrl+M:** `Ctrl+M` produces the same byte (`\r`) as carriage return in many terminals. awman intercepts Ctrl+M before it reaches the agent, so agents cannot receive a raw `\r` from this key combination. In practice this is not a problem — agents use Enter (which produces `\r\n` or `\n`) for line input, not Ctrl+M.

#### Mouse scroll behavior

awman forwards mouse scroll events to the agent when all of these conditions are true:
- The agent has enabled mouse tracking (via escape sequences like `CSI ? 1000 h`)
- You are viewing the live output (scrolled to the bottom)
- You are not holding **Shift**

This allows agents with scrollable components (file listings, diff panels, log viewers) to handle scroll events natively.

**Escape hatches for scrollback navigation:**

If an agent has mouse tracking enabled, you can still access awman's scrollback history in two ways:

1. **Shift+Scroll** — Always scrolls awman's scrollback, regardless of agent mouse tracking. Useful when you want to review earlier output while the agent's TUI has mouse-active components.
2. **Scroll up while in scrollback** — Once you scroll back (moving away from live output), ALL scroll events go to awman's scrollback until you scroll back to the bottom, which returns you to live mode and resumes forwarding to the agent.

This mirrors tmux's copy-mode behavior: scrolling enters historical view, reaching the bottom exits it.

**Text selection always under awman's control:**

Mouse click, drag, and drag-release events are never forwarded to the agent. They always perform awman's native text selection, ensuring you can always select and copy text from the container overlay, even when the agent has mouse tracking enabled.

Scrollback holds up to 10,000 lines by default. While scrolled, the title bar shows `↑ scrollback (N / M lines)` where `N` is your current offset and `M` is the total depth.

**Ctrl+Y** with no active selection forwards the key to the agent instead of copying.

### Minimizing and restoring

Press **Ctrl+M** to cycle the container window between three states:

1. **Maximized** — container fills the screen
2. **Minimized** — container collapses to a 1-line status bar
3. **Hidden** — container is not displayed (agent keeps running)

```
─ 🔒 claude | myproject | 5% | 200mb | 1m 23s ─────────────────
```

When you cycle the container window, awman automatically resizes the running container's PTY to match the new display dimensions. This ensures interactive agents see the correct terminal size.

From the minimized or hidden state:

| Key | Effect |
|-----|--------|
| **Ctrl+M** | Cycle to the next state (minimized → hidden → maximized → minimized) |
| **↑ / ↓** | Scroll the execution window (behind the status bar) |
| **b / e** | Jump to beginning / end of execution window |
| **Esc** | Return focus to command box |

### When the container exits

The container window closes and a summary bar appears:

```
── claude · myproject-12345 · avg CPU 4.2% · 210MiB · 1m 47s · exit 0 ──
```

This summary persists until a new container is launched.

The window closes the moment the container actually terminates, for any reason: the agent process exits or you quit it manually, awman kills it (for example when a [yolo countdown](06-yolo-mode.md) expires and auto-advances a workflow), or a workflow action replaces it. During a workflow this happens immediately — the dead container's window never lingers over the execution window between steps; you see the summary bar and the execution window behind it until the next step's container opens a fresh window.

The window does **not** close while the container is still alive: a stuck agent (no output) keeps its window open, and so does an agent with a yolo countdown running — only actual container death closes it.

---

## Parallel containers

Workflow steps that don't depend on each other can run at the same time, each in its own container — see [Parallel Workflows](15-parallel-workflows.md) for how the engine decides what runs concurrently. When more than one container is running, the container window changes shape: one container is **focused** and shown maximized as usual, while each of the others renders as a **minimized status bar** underneath it — the same rounded strip a single container shows when minimized, stacked one per background container. The maximized window's title also names the workflow step its container is running.

```
╭─ 🔒 claude (containerized) — implement ─ awman-impl-1 | 5.0% | 200MiB | 2m ─╮
│                                                                              │
│  [focused container's output — full terminal emulation]                     │
│                                                                              │
╰──────────────────────────────────────────────────────────────────────────────╯
╭──────────────────────────────────────────────────────────────────────────────╮
│ 🔒 codex [tests] | awman-tests-2 | 12.5% | 256MiB | 45s                      │
╰──────────────────────────────────────────────────────────────────────────────╯
╭──────────────────────────────────────────────────────────────────────────────╮
│ ⚠ 🔒 claude [docs] | awman-docs-3 | 0.1% | 64MiB | 1m                        │
╰──────────────────────────────────────────────────────────────────────────────╯
  Ctrl-S switch  ·  Ctrl-M toggle  ·  Ctrl-W workflow
```

Each minimized bar reads `🔒 agent [step_name] | container | cpu | mem | duration` — the same live stats the maximized title shows, polled per container. Every container in the group is polled independently and refreshes about every 3 seconds, whether it is focused or minimized; a container shows `... | ...` in place of CPU and memory for the moment between its launch and its first sample:

| Appearance | Meaning |
|---|---|
| Green border and text | Container running normally |
| Yellow, `⚠` prefix | No output for more than 30 seconds — [stuck](05-workflows.md#stuck-steps) |
| Flashing purple / yellow, `Yolo in Ns` | A [yolo](06-yolo-mode.md) auto-advance countdown is running for that container; the color alternates every second and the bar shows the seconds remaining |

The focused container never shows a bar of its own — its step name, container name, and live stats are already in the maximized window's title line. If the *focused* container is the one counting down, its countdown appears as the same modal dialog a single container shows (see [Yolo Mode](06-yolo-mode.md#4-auto-advances-stuck-workflow-steps)) instead of a minimized bar, since there's no bar for the focused container to show it in.

Only the focused container receives keyboard input; the others keep running in the background regardless of which one is focused.

### Switching containers — Ctrl-S

Press **Ctrl-S** to move focus to the next running container. The newly-focused container swaps into the maximized window and is resized to match the current terminal dimensions; the container it replaces drops into the minimized stack.

Ctrl-S only cycles focus when more than one container is currently running. With a single container, Ctrl-S has no special effect and is passed through to the container's PTY as usual (some programs use it for flow control).

**With a yolo countdown modal open:** Ctrl-S still rotates focus, exactly as it does otherwise — a modal on one container never blocks you from checking on its siblings. The modal closes when you rotate away; if the container you rotate back to is still counting down, its modal reopens automatically (the countdown itself is never paused or restarted by rotating focus, since each container's timer runs independently in the background — see [Stuck and yolo behavior, per container](15-parallel-workflows.md#stuck-and-yolo-behavior-per-container)).

### Ctrl-M with multiple containers

**Ctrl-M** cycles the container display exactly as it does for a single container, applied to the whole group:

- **Maximized** — the focused container fills the overlay; every other container is a status bar underneath.
- **Minimized** — the overlay closes and *every* container (the focused one included) collapses into the stack of status bars, leaving the execution window visible.
- **Hidden** — nothing is shown. The agents keep running — press **Ctrl-M** again to bring the group back into view.

To bring a background container to the front, press **Ctrl-S** until it's focused.

A single container is just the one-container case of the same display: Maximized shows its overlay, Minimized shows its single status bar. There is no separate single-container behavior.

---

## The Workflow Overview — Ctrl-O

While a workflow runs, the **Workflow Overview** sits between the container area and the status bar, showing one rounded box per stage of the workflow with arrows joining the stages. Each box carries the step's status glyph and colour, its name, and — when the step overrides them — its `agent/model` label on the top border. See [Workflow Overview and step status](05-workflows.md#workflow-overview-and-step-status) for what each glyph and colour means.

The overview has two sizes. Press **Ctrl-O** (*o* for "overview") to minimize or maximize it. The status bar advertises whichever direction it can currently go — `ctrl-o maximize workflow overview` while it is minimized, `ctrl-o minimize workflow overview` while it is maximized.

**Ctrl-O and Ctrl-M are independent.** The Workflow Overview and the container window each have their own minimized/maximized state, and neither key touches the other's. You can maximize both at once, minimize both, or any combination — see [Sharing the screen with the container window](#sharing-the-screen-with-the-container-window) for how the two split the available rows.

### Minimized — the default

Every stage is a single box, so the overview is always 3 rows tall and leaves the rest of the screen to your agents. A stage with one step shows that step's normal box. A stage that fans out into parallel steps shows a step count instead:

```
╭─────────────────╮   ╭─────────────────╮   ╭─────────────────╮
│ ✓ plan          │ → │ ● 3 steps…      │ → │ ○ review        │
╰─────────────────╯   ╰─────────────────╯   ╰─────────────────╯
```

The summary box takes the colour of the group as a whole: red if any step failed, magenta while a step is being remediated, blue while any step is running, green once every step succeeded.

### Maximized — every step

Every step of every stage gets its own box, so you can see the whole fan-out at once:

```
╭─────────────────╮   ╭─claude/opus-4-8─╮   ╭─────────────────╮
│ ✓ plan          │ → │ ✓ impl-api      │ → │ ○ review        │
╰─────────────────╯   ╰─────────────────╯   ╰─────────────────╯
                      ╭─────────────────╮
                      │ ✓ impl-cli      │
                      ╰─────────────────╯
                      ╭─────────────────╮
                      │ ● impl-docs     │
                      ╰─────────────────╯
```

Finished steps are never rolled up into a summary — each keeps its own box, name, agent label, and colour for the whole run.

### Sharing the screen with the container window

A maximized Workflow Overview never puts the container window away, and maximizing a container never shrinks the overview back down. The two share the rows between the tab bar and the command box:

- **With no container window maximized**, the overview grows to fill everything between the tab bar and the command box. If there isn't room for both the overview and every container status bar, the overview wins and the bars are truncated to as many as fit.
- **With a container window maximized too**, the overview takes at most half of that space (never less than one box row) and the container PTY keeps the rest, so both stay readable.
- **Very large fan-outs scroll.** When a stage has more steps than the overview can show at its current height, the last box reads `+ N more…`; scroll the **mouse wheel** over the overview to reach the rest.

Press **Ctrl-M** to minimize the container if you want the overview to have the whole body, and **Ctrl-O** to minimize the overview back to its 3-row summary if you want the PTY to have it.

---

## Config dialog

Press **Ctrl+,** from anywhere in the TUI to open the config dialog instantly — even while an agent is running or the container window is maximized. You can also type `config show` in the command box and press **Enter**. Either way opens the same modal overlay for viewing and editing all configuration fields without leaving the TUI. The dialog takes up 90% of the terminal in both dimensions, so as much of the table as possible is visible at once.

```
╭──────────────────────────────── awman config ─────────────────────────────────╮
│  Field                              Global        Repo           Effective     │
│  agent                              claude                       claude        │
│  runtime                            docker                       docker        │
│  terminal_scrollback_lines          10000         5000           5000          │
│  ...                                                                           │
│  dynamicWorkflows.agentsToModels                  2 agents ma…   2 agents ma…  │
│▶ dynamicWorkflows.agentsToModels.…                claude-opus…   claude-opus…  │
│                                                                                 │
│  dynamicWorkflows.agentsToModels.claude = claude-opus-4-8, claude-sonnet-4-6   │
│                                                                                 │
│  comma-separated model names; save an empty value to remove                    │
│  ↑↓=row  PgUp/PgDn=page  ←→=col  Enter/e=edit  Ctrl+N=add model mapping  Esc   │
╰─────────────────────────────────────────────────────────────────────────────────╯
```

### Navigation and editing

| Key | Action |
|-----|--------|
| **↑ / ↓** | Move between rows |
| **PgUp / PgDn** | Jump ten rows at a time |
| **← / →** | Move between the editable columns (Global, Repo) |
| **Enter** or **e** | Start editing the selected field inline |
| **Enter** (while editing) | Save the new value to the focused scope's config file |
| **Esc** (while editing) | Cancel the edit without saving |
| **Ctrl+N** | Add a new agent→models mapping or guidance entry (see below) — the hint for it appears while an `agentsToModels` or `guidance` row is selected |
| **Esc** | Close the dialog |

While editing, the value is edited inline in its table cell with a visible `|` cursor; **← / →**, **Home / End**, **Backspace**, and **Delete** work as expected. Values longer than the cell scroll horizontally so the cursor never leaves view. Row navigation is frozen until you save or cancel.

A detail pane below the table always shows the full, untruncated value of the selected row's focused column — long values wrap across multiple lines there, and while editing it mirrors the complete edit buffer. Beneath it, a hint line shows the accepted value format for the field (e.g. `one of: claude, codex, …` for `agent`, `positive integer` for numeric fields, `comma-separated list` for list fields).

Each edit is validated and written immediately when you press **Enter** (surrounding whitespace is trimmed first). If the value is invalid (an unknown agent name, a non-numeric value for a numeric field, a malformed `agent::model` leader) or the config file cannot be written, nothing is saved: the dialog stays in edit mode with your input intact and shows the reason in red where the format hint normally appears, so you can correct the value instead of retyping it. Press **Esc** to abandon the rejected edit. After a successful save the table refreshes with the selected row preserved, so consecutive edits don't send you back to the top.

Editing targets the column the selection is on: the **Global** column writes `$HOME/.awman/config.json`, the **Repo** column writes `GITROOT/.awman/config.json`. Scope-restricted fields snap automatically — starting an edit on the Global column of a repo-only field (like `dynamicWorkflows.*`) moves the edit to the Repo column with a status-bar note, and vice versa for global-only fields (like `runtime`), so a value can never be written into a scope where it isn't read.

Fields marked read-only — such as `auto_agent_auth_accepted` — are shown dimmed; pressing **Enter** on one shows `This field is read-only` in the status bar.

There are more fields than fit at once — the table scrolls automatically to keep the selected row visible as you move past the bottom or top of the visible window.

### Agent→model mappings (`dynamicWorkflows.agentsToModels`)

The agent→model map for [dynamic workflows](13-dynamic-workflows.md) is fully manageable from the dialog:

- A summary row (`dynamicWorkflows.agentsToModels`) always appears, showing how many agents are mapped (or `(none)`), so the mapping is discoverable even before it's configured.
- Each configured agent gets its own row (`dynamicWorkflows.agentsToModels.<agentName>`) whose comma-separated model list is edited inline like any other field.
- **Ctrl+N** adds a new mapping in two steps: type the agent name and press **Enter**, then type its comma-separated model list and press **Enter** again. **Esc** cancels at either step. Entering the name of an already-mapped agent jumps to that agent's existing row for editing instead of overwriting it.
- Saving an **empty** value on a per-agent row removes that agent's mapping (an agent cannot be mapped to zero models).

The same per-agent entries work on the command line: `awman config set dynamicWorkflows.agentsToModels.claude "claude-opus-4-8, claude-sonnet-4-6"`.

### Developer guidance list (`dynamicWorkflows.guidance`)

The [leader guidance list](13-dynamic-workflows.md#guidance) — project-specific instructions the leader agent must follow whenever it designs a dynamic workflow — is also fully manageable from the dialog:

- A summary row (`dynamicWorkflows.guidance`) always appears, showing how many entries are configured (or `(none)`).
- Each entry gets its own row (`dynamicWorkflows.guidance.<index>`) whose text is edited inline like any other field.
- **Ctrl+N** appends a new entry in a single step: type the instruction text and press **Enter**. **Esc** cancels. Unlike `agentsToModels`, there's no key to type first — the index is assigned automatically as the next position in the list.
- Saving an **empty** value on an entry row removes it; the remaining entries shift down and re-index, and the row list refreshes to reflect the new indices.

The same per-entry edits work on the command line: `awman config set dynamicWorkflows.guidance.0 "Never spawn more than two agents in parallel."`.

---

## Multi-tab support

Press **Ctrl+T** to open a new tab. Each tab has its own working directory, execution window, and container session. Tabs run independently in the background when you switch away.

```
Ctrl+T          open a new tab (prompts for working directory)
Ctrl+A          switch to the previous tab
Ctrl+D          switch to the next tab
Ctrl+C, Ctrl+T  (multiple tabs open) close current tab
```

The tab bar shows each tab's project name, current or last command, and an arrow (`➡`) on the active tab. The active tab's bottom border is suppressed so it visually opens into the content area.

Tab names are truncated with `…` only when they don't fit their tab: at the minimum tab width (20 columns) that means 14 characters of the name, and wider tabs show correspondingly more of a long name. The tab bar distributes width according to the number of open tabs:

| Open tabs | Each tab gets |
|-----------|--------------|
| 1 | ¼ of terminal width |
| 2 | ½ of terminal width |
| 3 | ¾ ÷ 3 of terminal width |
| 4+ | full width ÷ n |

### Tab colours

| Colour | Meaning |
|--------|---------|
| Grey | Idle or completed |
| Blue | Running (no container) |
| Green | Running with active container |
| Cyan | The [amie tab](#the-amie-tab) |
| Purple / Magenta | Permanently bound to a remote API session |
| Red | Exited with error |
| Yellow | Container silent for >30 seconds (stuck warning) |
| Alternating Yellow / Purple | Background yolo countdown in progress: tab label alternates between `⚠️ yolo in Ns` and `🤘 yolo in Ns` every 2 seconds (see [Yolo Mode](06-yolo-mode.md#background-yolo-countdown)) |

The amie tab's cyan and a remote-bound tab's purple/magenta are both fixed,
kind-based colours: they take priority over the execution-phase colours
above (grey/blue/green/red) but still yield to the yellow stuck warning and
the yolo countdown, since those are live signals about a run in progress.

### Remote-bound tabs

When `remote.defaultAddr` is set in `~/.awman/config.json`, opening a new tab with **Ctrl+T** offers an option to bind the tab to a remote API session. A **remote-bound tab** forwards every command you type to the remote host via the API — no extra flags or session arguments needed.

Remote-bound tabs are **purple** in the tab bar. The tab label shows `host:port` of the remote host instead of the local directory name. When a workflow runs on the remote session, the Workflow Overview appears automatically and updates every 5 seconds.

For full details on creating remote-bound tabs, the create-session sub-modal, and Workflow Overview behavior, see [Remote Mode: Remote-bound TUI tabs](10-remote-mode.md#remote-bound-tui-tabs).

### The amie tab

amie — awman's always-on condition-watching daemon — gets its own singleton
tab inside this same multi-tab TUI rather than a separate program. Open it
either of two ways:

- Press **Ctrl+T** to open the New Tab dialog, then press **Ctrl-A** while
  it's focused. The dialog's prompt shows a hint — "Press Ctrl-A to open
  amie" — as a reminder. This doesn't add a second global `Ctrl-A` binding:
  outside the New Tab dialog, `Ctrl-A` still switches to the previous tab as
  usual.
- Run `awman amie` with no subcommand from a terminal (with a TTY attached
  and no `-n`/`--json`); awman opens the TUI pre-focused on the amie tab.

There is at most one amie tab at a time — opening it again just focuses the
existing one. It's **cyan** in the tab bar, distinct from every other tab
colour, and its label is always the fixed word `amie` rather than a
directory name, since it isn't bound to a project directory.

Otherwise it's an ordinary tab: it takes part in **Ctrl-A**/**Ctrl-D** tab
cycling, closes through the normal close-tab flow, and keeps its state
while you're on a different tab. The command box below it still works
exactly as it does on any other tab — you can type `amie <subcommand> ...`
directly into it. The one difference is what fills the execution window
above the command box: amie's condition list instead of plain command
output, and, once you attach to a running condition, the same
container / Workflow Overview view a regular workflow tab shows. **Ctrl-G** (the
git sidebar) is a no-op here, since the tab has no repository to show.

See [amie](16-amie.md) for what conditions are, the amie tab's key
bindings, and attaching to a running condition.

---

### Stuck detection

If a running container produces no output for more than 30 seconds, the tab turns yellow and the subcommand label gains a `⚠️` prefix (e.g. `⚠️ chat`). The warning clears automatically when you:

- Switch to the yellow tab
- Press any key while the tab is active
- Scroll with the mouse wheel

**Active-tab suppression:** On the currently active tab, any keypress or mouse scroll also resets the stuck timer directly. If you are actively reading or scrolling through output, the tab will not turn yellow or show any stuck indicator — the timer only starts when both the container and the user have been idle for 30 seconds. Background tabs are not affected by this; they use output time alone to determine stuck state.

For workflow tabs, awman goes further: the [workflow control board](05-workflows.md#workflow-control-board) opens automatically so you can act without having to notice the yellow indicator. In yolo mode, background tabs show a live countdown directly in the tab bar instead of a dialog. See [Workflows](05-workflows.md) and [Yolo Mode](06-yolo-mode.md) for details.

---

## Reference: all keyboard shortcuts

### Global shortcuts (anywhere in TUI)

| Key | Action |
|-----|--------|
| **Ctrl+T** | Open a new tab (prompts for working directory) |
| **Ctrl+A** | Switch to the previous tab |
| **Ctrl+D** | Switch to the next tab |
| **Ctrl+G** | Toggle Git Sidebar (live view of repository changes) |
| **Ctrl+M** | Toggle container window between maximized, minimized, and hidden |
| **Ctrl+O** | Minimize / maximize the Workflow Overview (independent of Ctrl+M) |
| **Ctrl+S** | Switch focus to the next running container (only when [multiple parallel containers](#parallel-containers) are running; otherwise passed to the container's PTY) |
| **Ctrl+W** | Open workflow control board (between steps or mid-step while running) |
| **Ctrl+,** | Open / close the configuration dialog |
| **Ctrl+C** | Quit awman (single tab) or close current tab (multiple tabs open) |

### Command box

| Key | Action |
|-----|--------|
| **Enter** | Execute the typed command |
| **Ctrl+Enter** or **Shift+Enter** | Insert a newline in multi-line input |
| **Tab** / **Shift+Tab** | Cycle through autocomplete suggestions |
| **← / →** | Move cursor left / right; input scrolls horizontally if needed |
| **Ctrl+← / Ctrl+→** | Move cursor by word |
| **Home / End** | Jump to start / end of input |
| **Backspace / Delete** | Delete characters |
| **Ctrl+Backspace** | Delete the previous word |
| **↑** | Focus the execution window (for scrolling) |
| **q** | Quit awman (when command box is empty and tab is idle) |

### Execution window

| Key | Action |
|-----|--------|
| **↑ / ↓** | Scroll output line by line |
| **PageUp / PageDown** | Scroll output one full page |
| **b** | Jump to beginning of output |
| **e** | Jump to end (return to live view) |
| **l** | Toggle status log between collapsed and expanded view |
| **Esc** | Return focus to command box |
| Mouse scroll | Scroll output at any time (focus not required) |

### Container window (when maximized)

| Key | Action |
|-----|--------|
| **Esc** | Forward `\x1b` to the agent (for vim, fzf, interactive CLIs) |
| **Tab / Shift+Tab** | Forward to the agent |
| Type | Forward input directly to the agent |
| **Ctrl+M** | Minimize the container window |
| **Ctrl+O** | Minimize / maximize the Workflow Overview (intercepted before the agent, like Ctrl+M) |
| Mouse scroll | Scroll terminal scrollback history (5 lines per tick) |
| Mouse drag | Select text in the terminal (highlighted with inverted colors) |
| **Ctrl+Y** | Copy selected text to clipboard (ANSI codes stripped) |

### Workflow control board

| Key | Action |
|-----|--------|
| **↑** | Restart current step (in a fresh container) |
| **←** | Cancel to previous step (rewind) |
| **→** | Next step: advance in a new container |
| **↓** | Next step: same container (reuse current container) |
| **[d]** | Disable auto-advance for this step (toggle) |
| **Enter** | Confirm selected action (lightweight step-confirm dialog) |
| **Ctrl+W** | Escalate from lightweight dialog to full control board (while dialog is open) |
| **Esc** | Dismiss without changing anything (mid-step: step keeps running) |

### Workflow Overview

| Key | Action |
|-----|--------|
| **Ctrl+O** | Minimize / maximize the Workflow Overview |
| Mouse wheel (scroll up) | Scroll an oversized parallel stage upward (reveal hidden steps) |
| Mouse wheel (scroll down) | Scroll an oversized parallel stage downward |

### Configuration dialog

| Key | Action |
|-----|--------|
| **↑ / ↓** | Navigate between config field rows |
| **← / →** | Navigate between columns (Global, Repo, Effective) |
| **e** | Enter edit mode for the selected field |
| **Enter** | Confirm the new value and exit edit mode |
| **Esc** | Cancel edit without saving (edit mode) or close dialog (navigation mode) |
| **Ctrl+Enter** | Save all pending changes to config files |
| **Ctrl+,** | Close the dialog (same as Esc in navigation mode) |

### Dialogs

| Context | Key | Action |
|---------|-----|--------|
| Quit confirmation | **y** | Confirm quit |
| Quit confirmation | **n** or **Esc** | Cancel |
| Close-tab dialog | **q** | Quit awman |
| Close-tab dialog | **c** | Close current tab only |
| Close-tab dialog | **n** or **Esc** | Cancel |
| Lightweight step-confirm | **Enter** | Advance to next step |
| Lightweight step-confirm | **Esc** | Pause workflow |
| Lightweight step-confirm | **Ctrl+W** | Open full control board |

---

[← Getting Started](00-getting-started.md) · [Next: Agent Sessions →](03-agent-sessions.md)
