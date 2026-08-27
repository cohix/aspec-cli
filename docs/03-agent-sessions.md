# Agent Sessions

An agent session is a Docker container running your configured AI agent (Claude Code, Codex, OpenCode, Maki, Gemini, Antigravity, GitHub Copilot CLI, Crush, or Cline) against your project. awman handles starting the container, injecting your credentials, and connecting your terminal to the agent's input/output.

There are two session types: **freeform chat** and **work item implementation**.

---

## Freeform chat

```sh
awman chat
# or, in the TUI command box:
chat
```

`chat` launches an agent with no pre-configured prompt — a clean, blank slate. Use it for exploring the codebase, asking questions, prototyping ideas, or any task where you want to drive the conversation yourself.

In the TUI, the container window opens immediately and all keyboard input is forwarded to the agent. In command mode, the container's stdin/stdout/stderr are directly connected to your terminal.

Press **Ctrl+C** to exit the agent session when you're done.

---

## Flags common to `chat` and other agent-launching commands

### `--agent <name>`

Override the configured agent for this session. Available agents: `claude`, `codex`, `opencode`, `maki`, `gemini`, `antigravity`, `copilot`, `crush`, `cline`.

```sh
# CLI
awman chat --agent codex               # launch a Codex session for this project
awman exec workflow path/to/workflow.toml --agent gemini    # run workflow with Gemini instead of the configured agent
awman chat --agent=copilot             # --flag=value form is also accepted

# TUI command box
chat --agent crush
exec workflow path/to/workflow.toml --agent=cline
```

Both `--agent NAME` and `--agent=NAME` forms are accepted in both the CLI and the TUI command box. The TUI command box honours the flag and passes the correct agent to the container — it is not silently ignored.

This overrides the `agent` field in your repo config for this run only — no config file is modified. awman uses the agent-specific image (`awman-{project}-{agent}:latest`) for the session.

If the agent image does not yet exist, awman offers to download the template and build both the project base image (if needed) and the agent image before launching.

Passing an unknown agent name exits immediately with a list of valid options:

```
error: unknown agent "foo"; available agents: claude, codex, opencode, maki, gemini, antigravity, copilot, crush, cline
```

### `--model <NAME>`

Override the model used by the launched agent for this session.

```sh
# CLI
awman chat --model claude-opus-4-6
awman exec workflow path/to/workflow.toml --model claude-haiku-4-5
awman chat --model=gpt-4o               # --flag=value form is also accepted

# TUI command box
chat --model claude-opus-4-6
exec workflow path/to/workflow.toml --model=claude-haiku-4-5
```

Both `--model NAME` and `--model=NAME` forms are accepted in both the CLI and the TUI command box.

The model name is passed verbatim to the agent's own model flag — awman does not validate the value. If the name is not recognised by the agent, the agent surfaces its own error. This means any model the agent supports can be used without awman needing updates when providers release new models.

Per-agent translation and expected `<NAME>` format:

| Agent | Flag appended | Expected format |
|-------|--------------|-----------------|
| `claude` | `--model <NAME>` | bare model ID (e.g. `claude-opus-4-6`) |
| `codex` | `--model <NAME>` | bare model ID (e.g. `gpt-4o`) |
| `gemini` | `--model <NAME>` | bare model ID (e.g. `gemini-2.0-flash`) |
| `antigravity` | *(not supported — an error is returned)* | — |
| `opencode` | `--model <NAME>` | **`provider/model` required** (e.g. `anthropic/claude-3-5-sonnet`) |
| `maki` | `--model <NAME>` | `provider/model-id` (e.g. `anthropic/claude-opus-4-6`) |
| `crush` | `--model <NAME>` (on the `run` subcommand) | bare model ID *or* `provider/model` to disambiguate when multiple providers expose the same model name |
| `cline` | `--model <NAME>` (on the `task` subcommand) | bare model ID; the provider is selected separately via `cline auth -p <provider>` and is not switchable per-invocation |
| `copilot` | *(not supported — a `WARNING:` is printed, flag omitted)* | — |

For agents that support multiple providers (`opencode`, `crush`, `maki`), the `provider/model` slash form lets you target a specific provider when more than one is configured. awman passes the value through verbatim — the agent does the routing.

If an agent does not support `--model`, the behaviour varies. For Antigravity, the command exits with an error; configure the model via `~/.gemini/antigravity-cli/settings.json` or the `/model` slash command inside the agent session instead. GitHub Copilot CLI selects models via the `/model` interactive slash command rather than a CLI flag, so `--model` is silently dropped for copilot sessions.

`--model` can be combined freely with `--agent`, `--yolo`, `--auto`, and all other flags. When used with `exec workflow`, the flag value acts as the default model for every workflow step that does not define its own `model` field. See [Per-step model overrides](05-workflows.md#per-step-model-overrides).

Under the `docker-sbx-experimental` runtime, the flag is delivered through the sandbox's per-launch session config (built-in template agents) or as launch arguments (custom-kit agents) rather than directly on the command line; the supported agents and modes are the same, except copilot, which cannot receive a model override there. See [Runtimes](12-runtimes.md#known-limitations).

### `--non-interactive` / `-n`

Run the agent in print/batch mode — no interactivity required. The agent executes, produces output, and exits. `-n` is a short alias for `--non-interactive` and works on all commands that support the flag (`chat`, `exec prompt`, `exec workflow`, `ready`, `specs amend`).

| Agent | Flag used |
|-------|-----------|
| Claude | `--print` |
| Codex | `exec` subcommand |
| OpenCode | `run` subcommand |
| Maki | *(not supported — warning printed, agent launches in interactive mode)* |
| Gemini | *(not supported — warning printed, agent launches in interactive mode)* |
| Antigravity | `--print` |
| Copilot | *(not supported — warning printed, agent launches in interactive mode)* |
| Crush | `run` subcommand |
| Cline | `task` subcommand |

Useful for CI pipelines, scripting, or when you want the output captured rather than live.

Under the `docker-sbx-experimental` runtime, agents that launch through Docker's built-in sandbox templates (`claude`, `codex`, `gemini`, `copilot`, `opencode`) cannot have their non-interactive flag enabled — awman warns at launch and pipes the prompt to the interactive entrypoint instead. See [Runtimes](12-runtimes.md#known-limitations).

### `--plan`

Run the agent in read-only mode — it can analyse the codebase and suggest changes, but cannot modify files. Useful for getting a second opinion on an approach before committing to implementation.

| Agent | Plan mode |
|-------|-----------|
| Claude | `--plan` |
| Codex | `--approval-mode plan` |
| OpenCode | Not supported (flag is silently ignored) |
| Maki | Not supported (flag is silently ignored) |
| Gemini | `--approval-mode=plan` |
| Antigravity | `--approval-mode=plan` |
| Copilot | `--plan` |
| Crush | Not supported (flag is silently ignored) |
| Cline | `--plan` (on the `task` subcommand) |

`--plan` can be combined with `--non-interactive`.

Under the `docker-sbx-experimental` runtime, `--plan` / `--auto` / `--yolo` reach claude through its settings file (`permissions.defaultMode`) and the custom-kit agents (`crush`, `maki`, `cline`, `antigravity`) as launch arguments; the other built-in-template agents (`codex`, `gemini`, `copilot`, `opencode`) cannot receive them — awman warns at launch. See [Runtimes](12-runtimes.md#known-limitations).

### `--overlay <SPEC>`

Mount additional host resources into the agent container. Accepts typed overlay expressions:

- `dir(host_path:container_path[:ro|rw])` — mount a host directory (permission defaults to `:ro`)
- `env(VAR)` — pass a host environment variable into the container
- `skill(name)` / `skill(*)` — mount a named skill or all global skills
- `ssh()` — mount `~/.ssh` read-only (for Git operations over SSH)

May be repeated or comma-separated. Available on `chat`, `exec prompt`, and `exec workflow`.

```sh
awman chat --overlay "env(ANTHROPIC_API_KEY)"
awman chat --overlay "ssh()"
awman chat --overlay "dir(/data/reference:/mnt/reference:ro)"
awman exec workflow path/to/workflow.toml --overlay "env(GITHUB_TOKEN),ssh(),skill(*)"
```

See [Overlays](08-overlays.md) for the full reference including config-based overlays, the `AWMAN_OVERLAYS` env var, and conflict resolution rules.
See [Security & Isolation](04-security-and-isolation.md#overlay-mounts) for security considerations.

### `--allow-docker`

Mount the host Docker socket into the container, giving the agent the ability to build and run Docker containers. See [Security & Isolation](04-security-and-isolation.md#docker-socket-access) for details on when to use this.

### `--worktree`

Run the agent in an isolated Git worktree instead of your main working tree. After the agent finishes you choose to merge, discard, or keep the branch. See [Security & Isolation](04-security-and-isolation.md#worktree-isolation).

### `--auto`

Enable intermediate autonomous operation — the agent auto-approves file edits and writes, but still prompts before shell commands and other high-risk operations. Less permissive than `--yolo`.

| Agent | Flag used |
|-------|-----------|
| `claude` | `--permission-mode auto` |
| `codex` | `--sandbox workspace-write` |
| `opencode` | *(no equivalent — a warning is printed, flag omitted)* |
| `maki` | `--yolo` (maki's own flag) |
| `gemini` | `--approval-mode=auto_edit` |
| `antigravity` | *(no equivalent — warning printed, flag omitted)* |
| `copilot` | `--autopilot` (copilot's only CLI autonomous mode — same flag as `--yolo`) |
| `crush` | `--yolo` (crush's only autonomous flag — same as `--yolo`; a warning is printed that no intermediate mode exists) |
| `cline` | `--auto-approve-all` (auto-approves actions while keeping interactive mode) |

`--auto` applies `yoloDisallowedTools` config the same way `--yolo` does. Combined with `--workflow`, it implies `--worktree` but does **not** auto-advance stuck workflow steps.

When both `--yolo` and `--auto` are passed, `--yolo` wins.

### `--yolo`

Enable fully autonomous operation — the agent skips all permission prompts. See [Yolo Mode](06-yolo-mode.md).

### `--worktree`

(`exec workflow` only) Run in an isolated Git worktree under `~/.awman/worktrees/`. Implied by `--yolo` and `--auto` when used with `exec workflow`. See [Security & Isolation](04-security-and-isolation.md).

---

## Work item management

### Creating a work item

```sh
awman new spec
# or in TUI:
new spec
```

Prompts for a type (Feature, Bug, Task, or Enhancement) and a title, then creates a numbered work item file in the configured work items directory using the project's template.

By default, awman writes to `aspec/work-items/` and uses `aspec/work-items/0000-template.md`. If neither exists, awman auto-discovers any `*template.md` file in the work items directory and prompts you to confirm it. You can also configure the paths explicitly:

```sh
awman config set work_items.dir docs/work-items
awman config set work_items.template docs/work-items/my-template.md
```

If no template is found or confirmed, the new file is created with a minimal stub (`# Kind: Title`). See [Configuration: Work item paths](07-configuration.md#work-item-paths) for full details on path resolution and auto-discovery.

```sh
awman new spec --interview
```

After creating the file, prompts for a brief summary of the work, then launches an agent session to complete the spec — filling in user stories, implementation plan, edge cases, and test plan based on your summary. More thorough specs lead to better implementations.

In the TUI, a freeform text box dialog opens for the summary input. Use **Ctrl+Enter** to submit or **Esc** to cancel.

### Creating a spec from a GitHub issue

```sh
awman new spec --issue 84                                                      # bare number
awman new spec --issue prettysmartdev/awman#84                                 # owner/repo shorthand
awman new spec --issue https://github.com/prettysmartdev/awman/issues/84      # full URL
```

Fetches the GitHub issue and launches an agent to generate a structured work item spec from its content. Combined with `--interview`, the issue description is pre-populated in the text box for editing before the agent runs.

For full details on GitHub integration, authentication, and input formats, see [GitHub Integration](11-github-integration.md).

### Updating a spec after implementation

```sh
awman specs amend 0001
```

After implementing a work item, the actual implementation sometimes differs from the original spec. `specs amend` launches the agent to review the code that was written and update the spec to match — adding an "Agent implementation notes" section describing what changed and why. Useful for keeping specs accurate as a long-term reference.

---

## Creating skills

Claude Code skills are reusable instruction files (YAML frontmatter + Markdown) that teach an agent how to perform a specific task when invoked with `/skill-name`. Use `awman new skill` to create one interactively without copying and editing an existing file by hand.

```sh
# CLI
awman new skill

# TUI command box
new skill
```

Both modes prompt for:

1. **Skill name** — a kebab-case slug used as the filename and as the slash-command trigger (e.g. `run-tests`). Must contain only letters, digits, hyphens, and underscores.
2. **Description** — a one-line summary shown in the skill picker and in `/help` output.
3. **Body** — the skill's instruction text. Enter multiple lines and end with a line containing only `.`.

The resulting file is written to `.claude/skills/<name>/SKILL.md` inside the current repo.

### Skill file format

```markdown
---
name: run-tests
description: Run the full test suite and report failures
---

# Run Tests

Run `make test` and wait for output.
If tests fail, show the failing test names and exit codes.
If all tests pass, confirm success and stop.
```

The `name` field is the skill's slug; the `description` is a single sentence; the body is free-form Markdown written in second-person imperative ("Run …", "Check …", "If … then …").

### Interview mode

```sh
awman new skill --interview
```

Enter a brief summary of what the skill should do. A code agent writes the complete skill body for you, following the second-person imperative style and adding any necessary commands, code examples, or decision trees.

In the TUI, the dialog replaces the Body field with a Summary field. Press **Ctrl-Enter** to start the interview agent.

**TUI key bindings** (skill dialog):

| Key | Action |
|-----|--------|
| **Tab** / **Shift-Tab** | Cycle through fields |
| **Ctrl-Enter** | Finish — write the file (or start the interview agent) and close |
| **Esc** | Cancel without writing |

### Global skills

```sh
awman new skill --global
```

Writes to `~/.awman/skills/<name>/SKILL.md` instead of the current repo. Use this to maintain a personal library of skills that travel with you across projects.

To make global skills available inside agent containers, enable the skills overlay via config:

```json
{ "overlays": ["skill(*)"] }
```

Or pass it at the command line:

```sh
awman exec workflow path/to/workflow.toml --overlay "skill(*)"
```

Once enabled, your global skills appear as slash commands. See [Overlays](08-overlays.md) for details.

`--global` and `--interview` can be combined. When combined, the agent is given access only to the `~/.awman/skills/<name>/` directory — not the whole repo or home directory. This still requires being inside a git repository (for agent image lookup).

### Flags

| Flag | Description |
|------|-------------|
| `--interview` | Let a code agent complete the skill body from a short summary |
| `--global` | Write to `~/.awman/skills/<name>/` instead of the current repo |

### Edge cases

| Situation | Behaviour |
|-----------|-----------|
| Name contains spaces or path separators | Rejected immediately with a descriptive error |
| Skill already exists at the destination | Error with the existing path; awman does not overwrite silently |
| Empty description | Error before any file is written |
| Not inside a git repo (non-global) | Error: run with `--global` to write to `~/.awman/` |
| `--global --interview` outside a git repo | Error: agent image lookup requires a git repo |
| Skill body is empty (CLI) | Warning logged; empty body written to file |

---

## Monitoring running agents

```sh
awman status          # one-shot snapshot
awman status --watch  # auto-refreshing dashboard (every 3 seconds)
```

`status` works outside the TUI. It shows every active code agent container with CPU usage, memory, project path, and runtime.

```
CODE AGENTS
┌────────────────────────────┬────────┬───────┬─────────┐
│ Project                    │ Agent  │ CPU   │ Memory  │
├────────────────────────────┼────────┼───────┼─────────┤
│ /home/user/myproject       │ claude │ 5.23% │ 210MiB  │
└────────────────────────────┴────────┴───────┴─────────┘
```

If awman is launched outside of any Git repository, `status --watch` runs automatically instead of the normal startup.

The status output includes a source marker for each container. Ordinary user
sessions are marked `session`; a container launched by the amie daemon is
marked `amie:<condition>`, such as `amie:issue-triage`. This marker identifies
which condition owns the background evaluation. It can appear even if you have
never enabled amie; it does not indicate a problem with a regular session.

---

## Agent authentication

awman automatically passes your agent's credentials into the container — you never have to log in manually inside a container session.

For Claude Code, awman reads the OAuth token from the macOS Keychain (service: `Claude Code-credentials`) and passes it into the container as the `CLAUDE_CODE_OAUTH_TOKEN` environment variable. Credentials are never mounted as files, and the token value is never shown in displayed Docker commands; the command renders a bare `-e CLAUDE_CODE_OAUTH_TOKEN`.

| Agent | Auth mechanism |
|-------|---------------|
| `claude` | OAuth token read from macOS Keychain (`Claude Code-credentials`), injected as `CLAUDE_CODE_OAUTH_TOKEN` |
| `codex` | — |
| `opencode` | — |
| `maki` | API key via `env()` overlay |
| `gemini` | API key via `env()` overlay and/or `~/.gemini/` OAuth directory (auto-mounted) |
| `antigravity` | API key via `env()` overlay (`ANTIGRAVITY_API_KEY`) and/or `~/.gemini/antigravity-cli/` OAuth directory (auto-mounted) |
| `copilot` | GitHub token via `env()` overlay (`COPILOT_GITHUB_TOKEN` or `GH_TOKEN`) |
| `crush` | Provider API key(s) via `env()` overlay |
| `cline` | `~/.cline/data/` directory mount (contains `secrets.json` with API keys) — auto-mounted |

Maki, Gemini, Copilot, and Crush authenticate via API keys passed from your host environment using `env()` overlays. Cline uses an auto-mounted directory. See [Gemini authentication](#gemini-authentication) for the full Gemini auth options, and [Copilot authentication](#copilot-authentication), [Crush authentication](#crush-authentication), and [Cline authentication](#cline-authentication) below.

### Host settings injection

For Claude sessions, awman also mounts sanitized copies of your Claude Code settings so the agent starts pre-configured with your model preferences, plugins, and onboarding state:

| Host file | Container path | Notes |
|-----------|----------------|-------|
| `~/.claude.json` | `/root/.claude.json:ro` | `oauthAccount` field stripped to prevent broken auth state |
| `~/.claude/settings.json` | `/root/.claude/settings.json:ro` | Model preferences, plugins — copied as-is |

Your original files are never modified. The copies are created in a temporary directory before each launch and cleaned up when the container exits.

---

## Gemini authentication

Gemini supports two authentication paths. You can use either or both — awman sets up both automatically.

### API key (`env()` overlay)

Add `GEMINI_API_KEY` (or one of the Vertex AI variables) to your overlays config:

```json
{ "overlays": ["env(GEMINI_API_KEY)"] }
```

Or pass it per-command: `awman chat --agent gemini --overlay "env(GEMINI_API_KEY)"`.

Get a free API key from [Google AI Studio](https://aistudio.google.com/apikey) (1,000 requests/day on the free tier). awman reads the value from your host shell and injects it into the container. The value is masked (`***`) in all displayed Docker commands.

Supported Gemini auth environment variables:

| Variable | Description |
|----------|-------------|
| `GEMINI_API_KEY` | API key from Google AI Studio |
| `GOOGLE_API_KEY` | Vertex AI API key (takes precedence over `GEMINI_API_KEY`) |
| `GOOGLE_CLOUD_PROJECT` | Vertex AI project ID |
| `GOOGLE_CLOUD_LOCATION` | Vertex AI region |
| `GOOGLE_GENAI_USE_VERTEXAI` | Set to `true` to enable the Vertex AI auth path |

> **Note on `GOOGLE_APPLICATION_CREDENTIALS`:** This variable points to a file path on the host. Passing it via `env()` overlay injects the path string but not the file itself, so the container cannot read it. Service account JSON authentication requires either embedding the key in your `Dockerfile.dev` or mounting it with a `dir()` overlay. For most users, `GEMINI_API_KEY` is simpler.

### OAuth token (`~/.gemini/` mount)

Gemini's default interactive auth stores OAuth tokens in `~/.gemini/settings.json` on your host after you run `gemini` for the first time and complete the browser login flow. awman automatically copies `~/.gemini/` into a temporary directory and mounts it into the container at `/root/.gemini`, so the agent picks up your existing OAuth session without a manual login step.

If `~/.gemini/` does not exist on the host (you've never run `gemini` locally), awman creates an empty directory and mounts that instead. Gemini will prompt for authentication inside the container on first use.

The mount is a copy, not a bind mount — changes the agent makes to its auth state inside the container are isolated and do not affect the live `~/.gemini/` on your host.

### Auth precedence

When both an API key env var and OAuth tokens are present, Gemini uses the API key. This is Gemini's own resolution logic — awman does not arbitrate. If you want to use OAuth auth exclusively, omit the key variables from your overlays config.

---

## Antigravity authentication

Antigravity supports two authentication paths, similar to Gemini. You can use either or both — awman sets up both automatically.

### API key (`env()` overlay)

Add `ANTIGRAVITY_API_KEY` to your overlays config:

```json
{ "overlays": ["env(ANTIGRAVITY_API_KEY)"] }
```

Or pass it per-command: `awman chat --agent antigravity --overlay "env(ANTIGRAVITY_API_KEY)"`.

Get an API key from [Google AI Studio](https://aistudio.google.com/apikey) or through your Antigravity account. awman reads the value from your host shell and injects it into the container. The value is masked (`***`) in all displayed Docker commands.

Supported Antigravity auth environment variables:

| Variable | Description |
|----------|-------------|
| `ANTIGRAVITY_API_KEY` | Antigravity API key |
| `GOOGLE_API_KEY` | Vertex AI API key (takes precedence over `ANTIGRAVITY_API_KEY`) |
| `GOOGLE_CLOUD_PROJECT` | Vertex AI project ID |
| `GOOGLE_CLOUD_LOCATION` | Vertex AI region |

### OAuth token (`~/.gemini/antigravity-cli/` mount)

Antigravity's interactive auth stores OAuth tokens in `~/.gemini/antigravity-cli/settings.json` after you run `agy` for the first time and complete authentication. awman automatically copies `~/.gemini/antigravity-cli/` into a temporary directory and mounts it into the container at `/root/.gemini/antigravity-cli`, so the agent picks up your existing OAuth session without a manual login step.

If `~/.gemini/antigravity-cli/` does not exist on the host (you've never run `agy` locally), awman creates an empty directory and mounts that instead. Antigravity will prompt for authentication inside the container on first interactive use.

The mount is a copy, not a bind mount — changes the agent makes to its auth state inside the container do not affect the live `~/.gemini/antigravity-cli/` on your host.

### Auth precedence

When both an API key env var and OAuth tokens are present, Antigravity uses the API key. If you want to use OAuth auth exclusively, omit the key variables from your overlays config.

### Model configuration

Antigravity does not support the `--model` flag. Configure the model in `~/.gemini/antigravity-cli/settings.json` on your host, or use the `/model` slash command inside an interactive session to change the model for that session only.

---

## Gemini deprecation notice

The `gemini` agent is deprecated by Google in favor of Antigravity. When you launch a `gemini` session using `awman chat gemini` or set `agent = "gemini"` in your config, a deprecation warning appears before the container starts:

```
The 'gemini' agent is deprecated by Google. Migrate to 'antigravity' — run 'awman chat antigravity' (or 'awman config set agent antigravity' to change your default).
```

The warning does not block execution — your gemini session still starts. However, you should plan to migrate to `antigravity`:

1. Try it once — `awman chat antigravity` automatically downloads `Dockerfile.antigravity` and builds the agent image on first use.
2. Make it your default: `awman config set agent antigravity` (add `--global` to apply across all repos).
3. Set up authentication as described in [Antigravity authentication](#antigravity-authentication) above.

Antigravity is a drop-in replacement for Gemini with the same CLI interface and Docker-based isolation.

---

## Copilot authentication

GitHub Copilot CLI authenticates entirely via a GitHub token — there is no OAuth config directory to mount. Set your token via overlays config:

```json
{ "overlays": ["env(COPILOT_GITHUB_TOKEN)"] }
```

Or pass it per-command: `awman chat --agent copilot --overlay "env(COPILOT_GITHUB_TOKEN)"`.

Copilot reads the following environment variables in precedence order:

| Variable | Description |
|----------|-------------|
| `COPILOT_GITHUB_TOKEN` | Dedicated Copilot token (highest precedence) |
| `GH_TOKEN` | Standard GitHub CLI token |
| `GITHUB_TOKEN` | Fallback GitHub token |
| `COPILOT_GH_HOST` | GitHub Enterprise hostname override |

The token must have the "Copilot Requests" fine-grained PAT permission, or be a standard GitHub OAuth token obtained via `gh auth token`. Values are masked (`***`) in all displayed Docker commands.

For GitHub Enterprise users, add `COPILOT_GH_HOST` alongside the token:

```json
{ "overlays": ["env(COPILOT_GITHUB_TOKEN)", "env(COPILOT_GH_HOST)"] }
```

---

## Crush authentication

Crush authenticates entirely via provider API keys passed as environment variables — there is no config directory to mount. Add whichever API key(s) match your chosen provider to your overlays config:

```json
{ "overlays": ["env(ANTHROPIC_API_KEY)"] }
```

Supported Crush auth environment variables:

| Variable | Provider |
|----------|---------|
| `ANTHROPIC_API_KEY` | Anthropic Claude |
| `OPENAI_API_KEY` | OpenAI |
| `GEMINI_API_KEY`, `GOOGLE_API_KEY` | Google Gemini |
| `GROQ_API_KEY` | Groq |
| `OPENROUTER_API_KEY` | OpenRouter |
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION` | AWS Bedrock |
| `AZURE_OPENAI_API_ENDPOINT`, `AZURE_OPENAI_API_KEY` | Azure OpenAI |
| `VERTEXAI_PROJECT`, `VERTEXAI_LOCATION` | Google Vertex AI |

Only variables present in your host shell are injected — unlisted or unset variables are silently skipped. Values are masked (`***`) in all displayed Docker commands.

Crush's project-local config file (`.crush.json` at the repo root) is automatically available inside the container since the working directory is mounted as `/workspace`. No additional mounts are needed.

---

## Cline authentication

Cline stores API keys in `~/.cline/data/secrets.json` on your host, written there by `cline auth`. awman automatically copies `~/.cline/data/` into a temporary directory and mounts it into the container at `/home/awman/.cline/data`, so the agent picks up your existing credentials without re-running `cline auth` inside every container.

No overlay configuration is needed — credentials travel with the auto-mounted directory.

If `~/.cline/data/` does not exist on the host (you've never run `cline auth`), awman creates an empty temporary directory and mounts that instead. Cline will prompt for authentication inside the container on first interactive use.

The mount is a copy, not a bind mount — changes the agent makes to its credentials inside the container do not affect the live `~/.cline/data/` on your host. Task history (`tasks/`) and workspace state (`workspace/`) are excluded from the copy; only the config and secrets files are included.

To set up credentials on the host before running awman:

```sh
# Authenticate with Anthropic (example)
cline auth -p anthropic -k <your-api-key> -m claude-sonnet-4-6

# Verify credentials were written
cat ~/.cline/data/secrets.json
```


## Reference: `awman init`
## Reference: `awman init`

```sh
awman init [--agent=<name>] [--aspec]
```

Initialises the current Git repository for use with awman. See [Getting Started](00-getting-started.md) for a full walkthrough.

| Flag | Values | Default |
|------|--------|---------|
| `--agent` | `claude`, `codex`, `opencode`, `maki`, `gemini`, `antigravity`, `copilot`, `crush`, `cline` | `claude` |
| `--aspec` | (flag) | off |

`--aspec` downloads the `aspec/` folder from `github.com/prettysmartdev/aspec`, providing spec templates and work item scaffolding. Skipped without the flag.

When `--aspec` is not passed and no `aspec/` folder exists, `init` offers to configure a custom work items directory and template path interactively. This sets `work_items.dir` (and optionally `work_items.template`) in the repo config so commands like `new spec` and `exec workflow` work without requiring the `aspec/` folder layout. See [Work item paths](07-configuration.md#work-item-paths).

---

## Reference: `awman ready`

```sh
awman ready [--refresh] [--build] [--no-cache] [--non-interactive] [-n] [--allow-docker] [--json]
```

Verifies your environment is ready for agent sessions.

| Flag | Description |
|------|-------------|
| `--refresh` | Run the Dockerfile agent audit, update `Dockerfile.dev`, and rebuild both images |
| `--build` | Rebuild the project base image and agent images in `.awman/`. When multiple agent Dockerfiles exist, awman asks which to build |
| `--no-cache` | Pass `--no-cache` to every `docker build` invocation, including the project base image and all agent images |
| `--non-interactive` / `-n` | Run the audit agent in print mode |
| `--allow-docker` | Give the audit container access to the host Docker socket |
| `--json` | Emit machine-readable JSON instead of the human-readable table. Implies `--non-interactive`. See [`ready --json`](#ready---json) |

Use `--refresh` after your project's toolchain changes to update `Dockerfile.dev` (the project base) and rebuild both images. The agent dockerfile is not touched by the audit.

### Rebuilding multiple agent images

If your `.awman/` directory contains Dockerfiles for more than one agent (for example, `.awman/Dockerfile.claude` and `.awman/Dockerfile.codex`), running `awman ready --build` prompts before starting any builds:

```
Found 2 agent Dockerfiles:
  claude  (default)
  codex   (extra)

Build all agent images, or only the default (claude)? [all/default]:
```

- **all** — builds the project base image, then all agent images in `.awman/`, in sequence.
- **default** — builds the project base image and only the default agent image from config.

The `--no-cache` flag applies to every image built in this sequence.

### Build output

Each image build — project base or agent — is framed with prominent start and end markers so you can track progress across a multi-image sequence:

```
══════════════════════════════════════════════════
  Building project base image: awman-myproject:latest
══════════════════════════════════════════════════
[build output...]

══════════════════════════════════════════════════
  ✓ Built awman-myproject:latest
══════════════════════════════════════════════════


══════════════════════════════════════════════════
  Building agent image: awman-myproject-codex:latest
══════════════════════════════════════════════════
[build output...]
```

This applies whenever `ready` starts a build — `--build`, `--refresh`, or the initial `awman init` sequence.

`awman ready` also checks whether work item paths are configured. If neither `aspec/work-items/` exists nor `work_items.dir` is set, the summary shows a `⚠ not configured` warning (not a failure) for the `work items config` row, and prints a tip to run `awman config set work_items.dir <path>`.

### `ready --json`

When `--json` is set, `awman ready` suppresses the human-readable table and instead prints structured JSON summarising the environment check results. This is useful for CI pipelines and scripts that need to inspect readiness programmatically.

```sh
awman ready --json
```

```json
{
  "docker": { "available": true },
  "dockerfile": { "exists": true, "path": "/home/user/my-project/Dockerfile.dev" },
  "base_image": { "built": true, "tag": "awman-myproject:latest" },
  "agent_image": { "built": true, "tag": "awman-myproject-claude:latest" },
  "audit": { "ran": false }
}
```

When `--refresh` is also set, the audit runs and its results are included once complete:

```json
{
  "docker": { "available": true },
  "dockerfile": { "exists": true, "path": "/home/user/my-project/Dockerfile.dev" },
  "base_image": { "built": true, "tag": "awman-myproject:latest" },
  "agent_image": { "built": true, "tag": "awman-myproject-claude:latest" },
  "audit": { "ran": true, "exit_code": 0 }
}
```

`--json` implies `--non-interactive` — no interactive prompts are shown regardless of environment state. Streaming audit output is buffered internally and not printed; only the final JSON is written to stdout.

---

## Reference: all `chat` and `exec` flags

| Flag | `chat` | `exec prompt` | `exec workflow` | Description |
|------|--------|---------------|-----------------|-------------|
| `--agent=<name>` | ✓ | ✓ | ✓ | Override the agent for this session |
| `--model=<NAME>` | ✓ | ✓ | ✓ | Override the model used by the agent |
| `--non-interactive` / `-n` | ✓ | ✓ | ✓ | Print/batch mode |
| `--plan` | ✓ | ✓ | ✓ | Read-only analysis mode |
| `--allow-docker` | ✓ | ✓ | ✓ | Mount host Docker socket |
| `--overlay=<SPEC>` | ✓ | ✓ | ✓ | `dir()`, `env()`, `skill()`, `ssh()` overlays (repeatable) |
| `--worktree` | — | — | ✓ | Run in isolated Git worktree |
| `--auto` | ✓ | ✓ | ✓ | Auto-approve file edits, prompt for shell commands |
| `--yolo` | ✓ | ✓ | ✓ | Fully autonomous mode |
| `--work-item <N>` | — | — | ✓ | Work item number for template variable substitution |

---

[← Using the TUI](02-using-the-tui.md) · [Next: Security & Isolation →](04-security-and-isolation.md)
