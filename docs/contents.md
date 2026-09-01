# awman Documentation

A guide to using awman, the containerized multi-agent terminal multiplexer.

---

## Contents

| # | File | What's covered |
|---|------|----------------|
| 00 | [Getting Started](00-getting-started.md) | Installation, first agent session |
| 01 | [Concepts](01-concepts.md) | Mental model: containers, agents, modes, overlays |
| 02 | [Using the TUI](02-using-the-tui.md) | TUI layout, tabs, container window, keyboard reference |
| 03 | [Agent Sessions](03-agent-sessions.md) | `chat`, work items, agent authentication |
| 04 | [Security & Isolation](04-security-and-isolation.md) | Worktrees, overlays, Docker socket, container transparency |
| 05 | [Workflows](05-workflows.md) | Multi-step workflows, control board, state persistence |
| 06 | [Yolo Mode](06-yolo-mode.md) | Fully autonomous operation, disallowed tools, countdown |
| 07 | [Configuration](07-configuration.md) | Config files, runtime selection, all fields |
| 08 | [Overlays](08-overlays.md) | `dir()`, `env()`, `skill()`, `ssh()`, `context()` — sources, merge semantics, context overlays |
| 09 | [API Mode](09-api-mode.md) | HTTP server, sessions, commands, non-interactive/headless operation, CI/automation |
| 10 | [Remote Mode](10-remote-mode.md) | `remote exec`, `remote session`, live log streaming, TUI pickers |
| 11 | [GitHub Integration](11-github-integration.md) | `--issue` flag, fetching issues, authentication |
| 12 | [Runtimes](12-runtimes.md) | Docker, Apple Containers, Docker Sandboxes — platform support, setup, lifecycle |
| 13 | [Dynamic Workflows](13-dynamic-workflows.md) | `--dynamic` mode — leader agent designs the workflow, repair loop, `--leader` flag |
| 14 | [Cleaning Up](14-cleaning-up.md) | `awman clean` — remove containers, workflow files, and dangling images |
| 15 | [Parallel Workflows](15-parallel-workflows.md) | Concurrent workflow steps, `maxConcurrentAgents`, engine scheduling, per-container stuck/yolo, control board scoping |
| 16 | [squad](16-squad.md) | Tasks, durable workspaces and overlays, the squad daemon, card-grid TUI tab, attach, unattended-execution guardrails, `squad` config |
| 17 | [ACP Mode](17-acp-mode.md) | Structured agent sessions (CLI today), launch-mode selection, workflow/TUI limits |
| — | [Architecture (Detailed)](architecture.md) | Source layout, in-depth design decisions |

---

Start with [Getting Started](00-getting-started.md) if this is your first time.
