# awman Documentation

A guide to using awman, the containerized multi-agent terminal multiplexer.

---

## Contents

| # | File | What's covered |
|---|------|----------------|
| 00 | [Getting Started](00-getting-started.md) | Installation, `init`, `ready`, your first agent session |
| 01 | [Concepts](01-concepts.md) | Mental model: containers, agents, modes, overlays |
| 02 | [Using the TUI](02-using-the-tui.md) | TUI layout, tabs, container window, Workflow Overview, keyboard reference |
| 03 | [Agent Sessions](03-agent-sessions.md) | `chat`, permission modes (`--plan`/`--auto`/`--yolo`), ACP, work items, skills, agent authentication |
| 04 | [Security & Isolation](04-security-and-isolation.md) | Worktrees, Docker socket, SSH keys, command transparency |
| 05 | [Workflows](05-workflows.md) | Multi-step workflows, setup/teardown, control board, parallel groups, state persistence |
| 06 | [Dynamic Workflows](06-dynamic-workflows.md) | `--dynamic` — leader agent designs the workflow, repair loop, `--leader` |
| 07 | [Configuration](07-configuration.md) | Config files, precedence, runtime selection, every field |
| 08 | [Overlays](08-overlays.md) | `dir()`, `env()`, `skill()`, `ssh()`, `context()` — sources, merge semantics |
| 09 | [API & Remote Mode](09-api-and-remote-mode.md) | HTTP server, headless operation, CI/automation, and the `awman remote` client |
| 10 | [GitHub Integration](10-github-integration.md) | `--issue` flag, fetching issues, authentication |
| 11 | [Runtimes](11-runtimes.md) | Docker, Apple Containers, Docker Sandboxes — platform support, setup, lifecycle |
| 12 | [squad](12-squad.md) | Scheduled tasks, durable workspaces, the squad daemon, TUI tab, attach, guardrails |
| 13 | [Cleaning Up](13-cleaning-up.md) | `awman clean` — remove containers, workflow files, and dangling images |
| — | [Architecture (Detailed)](architecture.md) | Source layout, in-depth design decisions |

---

Start with [Getting Started](00-getting-started.md) if this is your first time.

### Looking for something that moved?

| Was | Now |
|-----|-----|
| Yolo Mode | [Permission modes](03-agent-sessions.md#permission-modes) (flags, disallowed tools) and [Auto-advance when stuck](05-workflows.md#auto-advance-when-stuck-yolo-mode) (the countdown) |
| ACP Mode | [ACP launch mode](03-agent-sessions.md#acp-launch-mode) |
| Parallel Workflows | [Parallel workflows](05-workflows.md#parallel-workflows) |
| Remote Mode | [Remote mode](09-api-and-remote-mode.md#remote-mode) |
