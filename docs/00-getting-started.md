# Getting Started

`awman` is a terminal multiplexer for AI code agents. Every agent runs inside a container — never directly on your machine. This quickstart takes you from nothing to a working agent session in about five minutes.

New to awman? Skim [Concepts](01-concepts.md) after this for the mental model behind these steps.

---

## Prerequisites

- **Git** — your project must be in a Git repository
- **A container runtime** — Docker (all platforms, daemon running), Apple Containers (macOS 26+), or Docker Sandboxes (`sbx` CLI, macOS arm64 / Windows)
- **An account with a supported agent** — Claude Code, OpenAI Codex, OpenCode, Maki, Google Gemini, GitHub Copilot CLI, Crush, Cline, or Google Antigravity

## 1. Install

```sh
curl -s https://prettysmart.dev/install/awman.sh | sh
```

The installer detects your platform and installs `awman` to `/usr/local/bin`. Other installation methods are listed in the [reference](#installation-options) below.

## 2. Initialize your project

From your project's Git root:

```sh
awman init
```

Init is interactive. It asks you to:

1. **Pick an agent** — this becomes the repo default (change later with `awman config set agent <name>`).
2. **Run the agent audit** — say yes. An agent inspects your codebase and fills `Dockerfile.dev` with the exact toolchain your project needs.
3. **Set up work items** — optional; configures where structured task specs live (or downloads the `aspec/` template).

Init then creates the two Dockerfiles, builds the container images, and prints a summary. Commit `Dockerfile.dev` and `.awman/` so teammates get the same environment.

## 3. Verify

```sh
awman ready
```

This checks your container runtime, Dockerfiles, images, and agent authentication (it sends the agent a test greeting and shows the response). All green means you're ready.

## 4. Run your first session

```sh
awman
```

This opens the TUI: a tab bar on top, an execution window in the middle, and a command box at the bottom. Type:

```
chat
```

and press **Enter**. An agent session starts in a container, and a terminal window connected to the agent opens. Talk to it as you would in any agent CLI. Press **Ctrl-M** to toggle the agent window between maximized and minimized — it keeps running either way.

That's it — you have an isolated agent working in your repo.

## What's next

- [Concepts](01-concepts.md) — the mental model: images, work items, overlays, modes
- [Using the TUI](02-using-the-tui.md) — tabs, keybindings, the container window
- [Agent Sessions](03-agent-sessions.md) — all `chat` flags and session management
- [Workflows](05-workflows.md) — multi-step agent runs with setup and teardown
- [Dynamic Workflows](06-dynamic-workflows.md) — let a leader agent design and run a workflow for you, no file to write
- [Permission modes](03-agent-sessions.md#permission-modes) — `--plan`, `--auto`, and fully autonomous `--yolo`
- [GitHub Integration](10-github-integration.md) — drive `new spec`, `exec workflow`, and `exec prompt` from GitHub issues with `--issue`
- [Runtimes](11-runtimes.md) — Docker, Apple Containers, and Docker Sandboxes (microVM) isolation

---

## Reference

### Installation options

| Method | Command |
|--------|---------|
| Installer script | `curl -s https://prettysmart.dev/install/awman.sh \| sh` |
| mise | `mise use -g github:prettysmartdev/awman` (pin: `@0.11.1`) |
| GitHub Releases | Download the asset for your platform, `chmod +x`, move onto `PATH` |
| From source | `git clone https://github.com/prettysmartdev/awman.git && cd awman && make install` (Rust 1.94+) |

Release assets: `awman-linux-amd64`, `awman-linux-arm64`, `awman-macos-amd64`, `awman-macos-arm64`, `awman-windows-amd64.exe`.

### `awman init`

| Flag | Effect |
|------|--------|
| `--agent <name>` | Agent to set up: `claude` (default), `codex`, `opencode`, `maki`, `gemini`, `copilot`, `crush`, `cline`, `antigravity` |
| `--aspec` | Download the `aspec/` spec-folder template into the repo |

### `awman ready`

| Flag | Effect |
|------|--------|
| `--refresh` | Re-run the agent audit and update `Dockerfile.dev` (use after toolchain changes) |
| `--build` | Force-rebuild the images |
| `--no-cache` | Disable the Docker layer cache during builds |
| `-n`, `--non-interactive` | Run the verification agent in non-interactive mode |
| `--json` | Print structured JSON instead of human output (implies `-n`) |
| `--allow-docker` | Mount the host Docker socket into the agent container |

---

[Next: Concepts →](01-concepts.md)
