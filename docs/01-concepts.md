# Concepts

This page is the mental model for awman: what runs where, what the moving pieces are called, and where to read more. Each section is a summary — the linked guides carry the detail.

---

## Why isolated environments?

An agent running directly on your machine can touch your home directory, SSH keys, credentials, and anything else your user account can. awman never does that. Every agent session runs in an isolated environment — a Docker container, an Apple VM, or a Docker Sandbox microVM — that sees only your project directory plus whatever you explicitly add. Credentials are injected per-session, SSH keys stay out unless you opt in, and the environment is stopped or removed when the session ends.

This is what makes autonomous operation ([Yolo Mode](06-yolo-mode.md)) reasonable: the blast radius of a bad agent decision is the container or VM and the mounted project, nothing else. See [Security & Isolation](04-security-and-isolation.md) for the full model, and [Runtimes](12-runtimes.md) for the difference between container-based and microVM-based isolation.

## The two-layer image system (Docker and Apple Containers)

For the Docker and Apple Containers runtimes, awman builds two images per project:

| File | Image | Contains |
|------|-------|----------|
| `Dockerfile.dev` (Git root) | `awman-{project}:latest` | Your project's OS, language runtimes, build and test tools — nothing agent-specific |
| `.awman/Dockerfile.{agent}` | `awman-{project}-{agent}:latest` | The agent tooling, layered on top (`FROM awman-{project}:latest`) |

The split means you can update project tooling without touching the agent setup, and switch agents without rebuilding your project environment. Both files come from templates: the **agent audit** (run during `awman init` or via `awman ready --refresh`) launches an agent to inspect your codebase and fill `Dockerfile.dev` with the tools your project actually needs; agent Dockerfiles are maintained by awman and rarely need editing. Commit both files.

For the Docker Sandboxes runtime, agent environments are set up using **kit YAML specs** instead of Dockerfiles. awman generates per-agent kit files at `~/.awman/kits/<agent>/` when you run `awman ready`. No custom image build or registry push is required. See [Runtimes](12-runtimes.md#docker-sandboxes-experimental).

## Agents

awman drives nine agents: `claude`, `codex`, `opencode`, `maki`, `gemini`, `copilot`, `crush`, `cline`, and `antigravity`. The repo default is set at init time; override per-command with `--agent`, or change it with `awman config set agent <name>`. Not every agent supports every feature (plan mode, yolo, model override) — see the capability table in [Agent Sessions](03-agent-sessions.md).

## Sessions and modes

The same engine runs in several modes; pick whichever fits the task:

- **TUI** (`awman`) — interactive multiplexer: tabs, live agent terminals, a command box with autocomplete. See [Using the TUI](02-using-the-tui.md).
- **One-shot CLI** (`awman chat`, `awman exec prompt`, `awman exec workflow`) — single commands from your shell. See [Agent Sessions](03-agent-sessions.md).
- **Headless** — non-interactive output for scripts and CI; awman detects the missing TTY or you force it with `-n`. See [Non-interactive operation](09-api-mode.md#non-interactive-headless-operation).
- **API** (`awman api start`) — an HTTP server that queues and executes prompts and workflows. See [API Mode](09-api-mode.md).
- **Remote** (`awman remote …`) — a thin client for an awman API server on another machine. See [Remote Mode](10-remote-mode.md).

Permission levels apply across modes: default (agent asks), `--plan` (read-only), `--auto` (auto-approve edits), `--yolo` (fully autonomous, isolated in a Git worktree). See [Yolo Mode](06-yolo-mode.md).

## Specs and work items

`aspec/` is an optional folder of Markdown specs describing your project to agents — purpose, architecture, security constraints, conventions. With it, agents make decisions consistent with your codebase instead of guessing.

A **work item** is one numbered Markdown file (`0001-add-auth.md`) describing a feature, bug, task, or enhancement in enough detail for an agent to implement, test, and document it. Create them with `awman new spec` (add `--interview` to have an agent draft the spec from a summary), point awman at a custom directory with `awman config set work_items.dir <path>`, and execute them through workflows (`awman exec workflow <file> --work-item 0001`). After implementation, `awman specs amend 0001` updates the spec to match what was actually built.

## Workflows

A workflow is a TOML (or YAML) file describing a multi-step agent run: optional setup steps (clone, branch, shell), a DAG of prompt steps with per-step agent/model overrides, and teardown steps (commit, push, open a PR, poll CI). Workflows live in `.awman/workflows/` (repo) or `~/.awman/workflows/` (personal, via `--global`), and are created by hand, interactively, or by an agent with `awman new workflow --interview`. See [Workflows](05-workflows.md).

## Overlays

Overlays add things to an agent's container beyond the project mount:

- `dir(host:container:ro|rw)` — mount an extra directory
- `env(VAR)` — pass through a host environment variable
- `skill(name)` / `skill(*)` — mount reusable skill files

They can be set per-command (`--overlay`), per-repo or globally (config `overlays`), per workflow step, or via `AWMAN_OVERLAYS`. See [Overlays](08-overlays.md), including [Context overlays in depth](08-overlays.md#context-overlays-in-depth) for persistent context and system prompts.

## Configuration

Two JSON files: `<git root>/.awman/config.json` (per-repo, committed) and `~/.awman/config.json` (global). Effective values resolve as flags > environment variables > repo config > global config > built-in defaults. Inspect and edit with `awman config show|get|set`. See [Configuration](07-configuration.md) for every field.

---

## Reference

### Command surface at a glance

| Command | Purpose | Guide |
|---------|---------|-------|
| `awman` | Open the TUI | [02](02-using-the-tui.md) |
| `awman init` | Set up a repo (agent, Dockerfiles, images) | [00](00-getting-started.md) |
| `awman ready` | Verify environment; `--refresh` re-runs the audit | [00](00-getting-started.md) |
| `awman chat` | Interactive agent session | [03](03-agent-sessions.md) |
| `awman exec prompt` | One-shot prompt | [03](03-agent-sessions.md) |
| `awman exec workflow` | Run a workflow file | [05](05-workflows.md) |
| `awman new spec\|workflow\|skill` | Create artefacts | [03](03-agent-sessions.md), [05](05-workflows.md) |
| `awman specs amend <N>` | Sync a spec with the implementation | [03](03-agent-sessions.md) |
| `awman status` | Show running agent containers | [03](03-agent-sessions.md) |
| `awman config show\|get\|set` | Inspect and edit config | [07](07-configuration.md) |
| `awman api start\|status\|logs\|kill` | HTTP API server | [09](09-api-mode.md) |
| `awman remote …` | Client for a remote awman server | [10](10-remote-mode.md) |

`awman status` marks ordinary containers as `session`. Containers launched by
the squad daemon carry an `squad:<task>` source marker, such as
`squad:issue-triage`, so background evaluations are identifiable even if you
have never configured squad.

### Key locations

| Path | Contents |
|------|----------|
| `Dockerfile.dev` | Project base image definition (committed) |
| `.awman/` | Repo config, agent Dockerfile, workflows, skills (committed) |
| `aspec/` | Optional project specs and work items |
| `~/.awman/` | Global config, personal workflows/skills, worktrees, API state |

---

[← Getting Started](00-getting-started.md) · [Next: Using the TUI →](02-using-the-tui.md)
