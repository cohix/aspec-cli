# Agent Sessions

An agent session is a Docker container running your configured AI agent (Claude Code, Codex, OpenCode, Maki, Gemini, Antigravity, GitHub Copilot CLI, Crush, or Cline) against your project. awman handles starting the container, injecting your credentials, and connecting your terminal to the agent's input/output.

There are two session types: **freeform chat** and **work item implementation**.
Either type normally uses the standard stdio launch mode. You can also use
[ACP mode](03-agent-sessions.md#acp-launch-mode) when you want awman to render structured agent
activity instead of the agent's raw terminal stream. ACP is currently
supported only by **Cline**.

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

## ACP launch mode

ACP (Agent Client Protocol) mode gives awman a structured way to present an
agent session. Instead of showing the agent's terminal stream directly, awman
renders the agent's messages, tool activity, plans, and permission requests in
its own interface.

ACP is currently supported only by **Cline**. Other agents continue to use the
standard stdio launch mode.

ACP is available with the Docker and Apple Containers runtimes. The
`docker-sbx-experimental` runtime does not currently support ACP.

The interactive ACP experience currently ships in the **CLI**. See
[ACP in the TUI](#acp-in-the-tui) and [Workflows](#workflows-and-unsupported-agents)
below for the current limits.

### Start an ACP session

Use `--launch-mode acp` for a single session. Put the flags before the prompt
positional so they are parsed:

```sh
awman chat --agent cline --launch-mode acp
awman exec prompt --agent cline --launch-mode acp "Review the failing tests"
```

The flag is available on `chat` and `exec prompt`. The shortest form uses your
configured agent, provided it supports ACP:

```sh
awman chat --launch-mode acp
```

To return to the normal terminal experience for one invocation, use
`--launch-mode stdio`.

---

### What changes in ACP mode

ACP changes how awman displays and drives the session. You see structured
agent responses and activity through awman's interface, rather than the
agent's raw terminal UI. Permission requests are presented by awman, and an
ACP session can continue with follow-up prompts while it remains open.

The agent still works on the current project through awman's normal isolated
agent session. ACP is a presentation and interaction choice; it does not
change which project you opened or which agent you selected.

---

### ACP in the CLI

The CLI uses an interactive stdio experience for ACP sessions: agent messages
stream to the terminal, while tool activity and other structured updates are
printed as readable blocks. When the agent asks for permission, the CLI shows
the available choices and reads your selection from standard input. After a
turn, it displays a `>` prompt for a follow-up message.

For `exec prompt`, the supplied prompt starts the first turn. For `chat`, you
can begin by entering prompts as the session runs. Press **Ctrl+D** when you
are finished entering follow-up prompts, or **Ctrl+C** to exit the session.

In a non-interactive run (`--non-interactive`, or with `--yolo`/`--auto`), the
CLI does not prompt for follow-ups or permissions — it runs the seeded turn and
exits, matching the headless behavior of stdio agents.

---

### ACP in the TUI

The full TUI agent window for ACP — a dedicated purple-bordered window with
inline structured updates, command-box follow-ups, and dialog permission
prompts — is **not yet available**. Launching an ACP session in the TUI renders
update summaries in the message area and does not present interactive follow-up
or permission dialogs; a permission request that is not pre-approved by
`--yolo`/`--auto` is denied.

For the interactive ACP experience today, run `awman chat --launch-mode acp`
from the CLI (outside the TUI).

---

### Selecting a launch mode

The `--launch-mode` flag accepts exactly two values:

```sh
awman chat --agent cline --launch-mode acp
awman chat --agent cline --launch-mode stdio
```

For persistent settings, set `launchMode` in the repository's
`.awman/config.json`. The default is `stdio`. The effective launch mode is
chosen in this order:

```
--launch-mode  >  AWMAN_LAUNCH_MODE  >  repo launchMode  >  stdio
```

See [Configuration](07-configuration.md) for the config fields and the
environment variable.

---

### Workflows and unsupported agents

ACP is **not yet available for `exec workflow` steps**. If a repository sets
`launchMode: acp` and a workflow step resolves to an ACP-capable agent, the
workflow fails before any step starts, directing you to run that agent over ACP
with `awman chat` or `awman exec prompt` instead — or to set `launchMode: stdio`
for workflows.

Workflow steps whose agents do **not** support ACP are governed by the global
`launchModeFallback` setting, which defaults to `error`:

- `error` stops the workflow before any step starts and reports the step and
  agent that cannot use ACP.
- `stdio` runs those steps in the regular stdio mode and prints a warning per
  step, and the workflow proceeds.

Set the policy in `$HOME/.awman/config.json`:

```json
{ "launchModeFallback": "stdio" }
```

An explicit `--launch-mode acp` request for a single unsupported agent remains
an error, so a one-off request cannot silently change modes. See
[Workflows](05-workflows.md) for workflow setup and execution.

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
| `copilot` | `--model <NAME>` | bare model ID; cannot be applied under the `docker-sbx-experimental` runtime (use the `/model` slash command there) |

For agents that support multiple providers (`opencode`, `crush`, `maki`), the `provider/model` slash form lets you target a specific provider when more than one is configured. awman passes the value through verbatim — the agent does the routing.

If an agent does not support `--model`, the behaviour varies. For Antigravity, the command exits with an error; configure the model via `~/.gemini/antigravity-cli/settings.json` or the `/model` slash command inside the agent session instead. GitHub Copilot CLI selects models via the `/model` interactive slash command rather than a CLI flag, so `--model` is silently dropped for copilot sessions.

`--model` can be combined freely with `--agent`, `--yolo`, `--auto`, and all other flags. When used with `exec workflow`, the flag value acts as the default model for every workflow step that does not define its own `model` field. See [Per-step model overrides](05-workflows.md#per-step-model-overrides).

Under the `docker-sbx-experimental` runtime, the flag is delivered through the sandbox's per-launch session config (built-in template agents) or as launch arguments (custom-kit agents) rather than directly on the command line; the supported agents and modes are the same, except copilot, which cannot receive a model override there. See [Runtimes](11-runtimes.md#known-limitations).

### `--non-interactive` / `-n`

Run the agent in print/batch mode — no interactivity required. The agent executes, produces output, and exits. `-n` is a short alias for `--non-interactive` and works on all commands that support the flag (`chat`, `exec prompt`, `exec workflow`, `ready`, `specs amend`).

| Agent | Flag used |
|-------|-----------|
| Claude | `--print` |
| Codex | `exec` subcommand |
| OpenCode | `run` subcommand |
| Maki | *(not supported — agent launches in interactive mode)* |
| Gemini | *(not supported — agent launches in interactive mode)* |
| Antigravity | `--print` |
| Copilot | *(not supported — agent launches in interactive mode)* |
| Crush | `run` subcommand |
| Cline | `task` subcommand |

Useful for CI pipelines, scripting, or when you want the output captured rather than live.

Under the `docker-sbx-experimental` runtime, agents that launch through Docker's built-in sandbox templates (`claude`, `codex`, `gemini`, `copilot`, `opencode`) cannot have their non-interactive flag enabled — awman warns at launch and pipes the prompt to the interactive entrypoint instead. See [Runtimes](11-runtimes.md#known-limitations).

### `--overlay <SPEC>`

Mount additional host resources into the agent container. Accepts typed overlay expressions:

- `dir(host_path:container_path[:ro|rw])` — mount a host directory (permission defaults to `:ro`)
- `env(VAR)` — pass a host environment variable into the container
- `skill(name)` / `skill(*)` — mount a named skill, pulled library, or all hand-authored global skills
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

(`exec workflow` only) Run in an isolated Git worktree under `~/.awman/worktrees/`. Implied by `--yolo` and `--auto` when used with `exec workflow`. See [Security & Isolation](04-security-and-isolation.md).

---

## Permission modes

Every agent-launching command (`chat`, `exec prompt`, `exec workflow`, and their `remote` equivalents) runs at one of four permission levels. They differ only in how much the agent may do without asking you first:

| Level | Flag | The agent… |
|-------|------|-----------|
| **Ask** (default) | *(none)* | Prompts before every file edit and shell command |
| **Plan** | `--plan` | Reads and analyses only — cannot modify files at all |
| **Auto** | `--auto` | Auto-approves file edits and writes; still prompts before shell commands and other high-risk operations |
| **Yolo** | `--yolo` | Skips every permission prompt and never pauses for confirmation |

When both `--yolo` and `--auto` are passed, `--yolo` wins.

Not every agent implements every level. Where an agent has no equivalent, awman simply omits the flag — the session still launches, but at the agent's own default permission level rather than the one you asked for. **Under the container runtimes this is silent**, so check the table for your agent before relying on a mode. (Under `docker-sbx-experimental` the unsupported combinations are warned about at launch — see [Runtimes](11-runtimes.md#known-limitations).)

### `--plan`

Run the agent in read-only mode — it can analyse the codebase and suggest changes, but cannot modify files. Useful for getting a second opinion on an approach before committing to implementation.

| Agent | Plan mode |
|-------|-----------|
| `claude` | `--permission-mode plan` |
| `codex` | `--approval-mode plan` |
| `opencode` | *(no equivalent — flag omitted)* |
| `maki` | *(no equivalent — flag omitted)* |
| `gemini` | `--approval-mode=plan` |
| `antigravity` | *(no equivalent — flag omitted)* |
| `copilot` | `--plan` |
| `crush` | *(no equivalent — flag omitted)* |
| `cline` | `--plan` (on the `task` subcommand) |

`--plan` can be combined with `--non-interactive`.

### `--auto`

Enable intermediate autonomous operation — the agent auto-approves file edits and writes, but still prompts before shell commands and other high-risk operations.

| Agent | `--auto` flag |
|-------|--------------|
| `claude` | `--permission-mode auto` |
| `codex` | `--sandbox workspace-write` |
| `opencode` | *(no equivalent — flag omitted)* |
| `maki` | *(no equivalent — flag omitted)* |
| `gemini` | `--approval-mode=auto_edit` |
| `antigravity` | *(no equivalent — flag omitted)* |
| `copilot` | *(no equivalent — flag omitted)* |
| `crush` | *(no equivalent — flag omitted)* |
| `cline` | `--auto-approve-all` (auto-approves actions while keeping interactive mode) |

`--auto` applies `yoloDisallowedTools` the same way `--yolo` does. Combined with `exec workflow` it implies `--worktree`, but it does **not** auto-advance stuck steps — that countdown is `--yolo`-only.

### `--yolo`

Fully autonomous operation. Use it when you want to hand a task to the agent and come back to a finished result.

```sh
awman exec workflow aspec/workflows/implement-feature.toml --yolo
awman chat --yolo
```

Yolo mode is a good fit when you have a well-specified work item you trust the agent to implement, when you want a multi-step workflow to run end-to-end without manual advancement, or when you have already reviewed the approach in a `--plan` session. It is a poor fit for work that will hit decisions genuinely needing your input, for open-ended `chat` sessions, and for anything difficult to undo.

`--yolo` does four things:

**1. Skips all agent permission prompts.** The agent-specific flag is appended to the container entrypoint before launch:

| Agent | Flag appended |
|-------|--------------|
| `claude` | `--dangerously-skip-permissions` |
| `codex` | `--dangerously-bypass-approvals-and-sandbox` |
| `opencode` | *(no equivalent — flag omitted)* |
| `maki` | `--yolo` |
| `gemini` | `--yolo` |
| `antigravity` | `--dangerously-skip-permissions` |
| `copilot` | `--autopilot` (copilot's only CLI autonomous mode) |
| `crush` | `--yolo` (inserted before the `run` subcommand: `crush --yolo run`) |
| `cline` | `--yolo` (on the `task` subcommand) |

**2. Applies `yoloDisallowedTools`** — see [Disallowed tools](#disallowed-tools) below. Only Claude implements a deny list (`--disallowedTools tool1,tool2,…`); for every other agent the setting is omitted, so a yolo session with those agents runs unrestricted.

**3. Implies `--worktree` for workflow execution.** Running a workflow with `--yolo` automatically creates an isolated Git worktree, and prints:

```
--yolo with workflow execution implies --worktree. Running in isolated worktree.
```

Passing `--worktree` as well is silently accepted — no duplicate worktree is created. With other commands (`chat`, `exec prompt`) `--worktree` is **not** implied; pass it explicitly if you want isolation.

**4. Auto-advances stuck workflow steps.** When a step goes silent, a countdown advances the workflow for you. See [Auto-advance when stuck](05-workflows.md#auto-advance-when-stuck-yolo-mode) in the workflow guide for the countdown, how it appears in each frontend, and how to cancel it.

### Disallowed tools

Add `yoloDisallowedTools` to your per-repo or global config to restrict which tools the agent may use even under full autonomy:

```json
{
  "yoloDisallowedTools": ["Bash", "computer"]
}
```

This is your safety net for operations you never want the agent to perform autonomously, regardless of how well-specified the task is. `"Bash"` prevents arbitrary shell command execution; `"computer"` prevents GUI automation.

**Config precedence:** per-repo config replaces global config entirely (lists are not merged). To inherit the global list for a repo, omit the field from the repo config. See [Configuration](07-configuration.md).

### Yolo through the HTTP API

Commands submitted through the API (`POST /v1/commands`) run with `--yolo` applied **by default** for `chat`, `exec prompt`, and `exec workflow` — the server assumes unattended execution unless your request specifies the flag itself. The response's `flags_applied` field confirms which defaults were used; see [Submit a command](09-api-and-remote-mode.md#submit-a-command).

### Security considerations

- `--yolo` removes the human checkpoints that catch unintended agent actions. Only use it with agents and work items you trust.
- `yoloDisallowedTools` is a floor the agent can never cross — but only Claude enforces it.
- `exec workflow --yolo` is the recommended autonomous pattern: isolated branch, structured phases, auto-advancing, easy to discard if the output isn't right.
- Gemini's `--yolo` skips all tool confirmations including shell commands; `--auto` (`--approval-mode=auto_edit`) is the more conservative choice.
- Copilot and Crush support `--yolo` only — `--auto` has no equivalent for either and is dropped.
- Cline's `--auto` keeps interactive mode while auto-approving; its `--yolo` fully skips confirmations and implies non-interactive operation.

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

If no template is found or confirmed, the new file is created with a minimal stub (`# Kind: Title`). See [Configuration: Work item paths](07-configuration.md#custom-work-item-paths) for full details on path resolution and auto-discovery.

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

For full details on GitHub integration, authentication, and input formats, see [GitHub Integration](10-github-integration.md).

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

### Pulled skill libraries

You can pull a published skills library from GitHub into your global skills store:

```sh
awman new skill --pull https://github.com/obra/superpowers
awman new skill --pull github.com/obra/superpowers
awman new skill --pull obra/superpowers
```

All three forms refer to the same repository. The library is stored at `~/.awman/skills/.library/superpowers/`; the final repository name is used as the library name. By default, awman looks for skill directories under the repository's `skills/` folder.

To refresh a library you have already pulled, use its short name:

```sh
awman new skill --pull superpowers
```

To refresh every pulled library:

```sh
awman new skill --pull-all
```

If the skills are in a different folder, provide a relative path inside the repository with `--subdir`:

```sh
awman new skill --pull github.com/example/team-skills --subdir .agents/skills
```

The selected subdirectory is remembered, so later `--pull team-skills` and `--pull-all` refreshes continue to use it. A successful pull reports the destination, the subdirectory, and the skills it found, in this form:

```
Pulled 'team-skills' into <library-directory> (3 skill(s) found under .agents/skills/): review, test, release
```

`--pull-all` continues refreshing the other libraries if one library fails and reports the number that succeeded and failed, for example `Skill library refresh complete: 2 succeeded, 1 failed.` Every reachable library is still refreshed, but the command exits non-zero when any library failed, so a scripted or CI refresh notices. When none have been pulled, it reports `no skill libraries pulled yet` and exits successfully. A short-name refresh for a library that has not been pulled yet reports that the library has not been pulled and directs you to use the full GitHub slug.

Pulled libraries are fetched, managed content. Refreshing one hard-resets its directory to the upstream version, so hand edits inside `~/.awman/skills/.library/` are discarded. Put personal skills in `~/.awman/skills/<name>/` instead. Pull operations are non-interactive, run entirely on the host as plain `git` commands (no container is launched), and do not create or edit a skill body.

awman only ever refreshes directories it created under `~/.awman/skills/.library/`, and never deletes or overwrites anything else. A pull is refused, with the offending path named, when:

- the target already holds something that is not an awman-managed clone (no `.git/` or no `.awman.json`);
- the target is a symlink rather than a real directory;
- the library was pulled from a different owner with the same repository name;
- the clone's git `origin` no longer matches the source recorded in its `.awman.json`.

In each case, remove the named directory yourself if you want to replace it.

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
| `--pull <repo>` | Pull a GitHub skills library, or refresh one by its short name |
| `--pull-all` | Refresh every previously-pulled skills library |
| `--subdir <path>` | Use a different relative folder inside the pulled repository instead of `skills/` |

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
sessions are marked `session`; a container launched by the squad daemon is
marked `squad:<task>`, such as `squad:issue-triage`. This marker identifies
which task owns the background evaluation or generated workflow. It can appear
even if you have never enabled squad; it does not indicate a problem with a
regular session. See [Squad](12-squad.md) for task management and unattended
execution.

---

## Agent authentication

awman automatically passes your agent's credentials into the container — you never have to log in manually inside a container session.

For Claude Code, awman reads your OAuth credential from the macOS Keychain (service: `Claude Code-credentials`) — or, on non-macOS hosts, from `~/.claude/.credentials.json` — and writes an awman-authored `.credentials.json` (access token, expiry, and scopes only) into the staged `~/.claude` directory that's mounted into the container. Your refresh token is never captured from the host credential and never mounted into a container.

While the session is running, a background monitor tracks the token's expiry and, before it runs out, pings your host's local Claude Code installation — the same sanctioned check `awman ready` uses — so it rotates its own Keychain entry. awman then atomically rewrites the staged credential file for every live session, so a long-running container picks up the new token on its next request with no restart. If the host can't refresh (asleep, logged out, offline), awman keeps the last-known-good token, warns you, and retries; a workflow step that fails on an auth error is retried once automatically after a fresh refresh. See [Control credential refresh](07-configuration.md#control-credential-refresh-authrefresh) to change the refresh threshold or turn this off.

The token value is never shown in displayed container commands; the mounted `~/.claude` directory appears only as an ordinary bind-mount path.

| Agent | Auth mechanism |
|-------|---------------|
| `claude` | OAuth credential read from macOS Keychain (or `~/.claude/.credentials.json`), delivered as a live-refreshed `.credentials.json` file in the staged `~/.claude` mount — never as an env var |
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

The `gemini` agent is deprecated by Google in favor of Antigravity. When you launch a `gemini` session using `awman chat --agent gemini` or set `agent = "gemini"` in your config, a deprecation warning appears before the container starts:

```
The 'gemini' agent is deprecated by Google. Migrate to 'antigravity' — run 'awman chat antigravity' (or 'awman config set agent antigravity' to change your default).
```

The warning does not block execution — your gemini session still starts. However, you should plan to migrate to `antigravity`:

1. Try it once — `awman chat --agent antigravity` automatically downloads `Dockerfile.antigravity` and builds the agent image on first use.
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

```sh
awman init [--agent=<name>] [--aspec]
```

Initialises the current Git repository for use with awman. See [Getting Started](00-getting-started.md) for a full walkthrough.

| Flag | Values | Default |
|------|--------|---------|
| `--agent` | `claude`, `codex`, `opencode`, `maki`, `gemini`, `antigravity`, `copilot`, `crush`, `cline` | `claude` |
| `--aspec` | (flag) | off |

`--aspec` downloads the `aspec/` folder from `github.com/prettysmartdev/aspec`, providing spec templates and work item scaffolding. Skipped without the flag.

When `--aspec` is not passed and no `aspec/` folder exists, `init` offers to configure a custom work items directory and template path interactively. This sets `work_items.dir` (and optionally `work_items.template`) in the repo config so commands like `new spec` and `exec workflow` work without requiring the `aspec/` folder layout. See [Work item paths](07-configuration.md#custom-work-item-paths).

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

### Credential health

`awman ready` also reports the health of any live-refreshed agent credential (currently Claude's Keychain-backed OAuth token), as read on the host at the time you ran `ready`:

```
Credential claude (68432s remaining)   ✓
```

If the credential can't be read, has already expired, or its expiry can't be determined, the row shows a warning instead (`credential unreadable: ...`, `credential expired`, or `credential expiry unknown`) rather than failing the whole `ready` check.

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

Alongside the fields above, the output includes an `agent_credentials` array — one entry per agent with a live-refreshed credential — carrying only non-secret health data:

```json
"agent_credentials": [
  { "agent": "claude", "refreshable": true, "expires_in_secs": 68432, "expired": false, "read_error": null }
]
```

`expires_in_secs` is `null` when the expiry couldn't be determined; `read_error` is set instead of `expires_in_secs` when the credential couldn't be read at all. No token value ever appears in this output.

---

## Reference: all `chat` and `exec` flags

| Flag | `chat` | `exec prompt` | `exec workflow` | Description |
|------|--------|---------------|-----------------|-------------|
| `--agent=<name>` | ✓ | ✓ | ✓ | Override the agent for this session |
| `--model=<NAME>` | ✓ | ✓ | ✓ | Override the model used by the agent |
| `--non-interactive` / `-n` | ✓ | ✓ | ✓ | Print/batch mode |
| `--launch-mode <stdio\|acp>` | ✓ | ✓ | ✓ | Choose standard stdio or ACP mode |
| `--plan` | ✓ | ✓ | ✓ | Read-only analysis mode |
| `--allow-docker` | ✓ | ✓ | ✓ | Mount host Docker socket |
| `--overlay=<SPEC>` | ✓ | ✓ | ✓ | `dir()`, `env()`, `skill()`, `ssh()` overlays (repeatable) |
| `--worktree` | — | — | ✓ | Run in isolated Git worktree |
| `--auto` | ✓ | ✓ | ✓ | Auto-approve file edits, prompt for shell commands |
| `--yolo` | ✓ | ✓ | ✓ | Fully autonomous mode |
| `--work-item <N>` | — | — | ✓ | Work item number for template variable substitution |
| `--issue <N|URL>` | — | ✓ | ✓ | Use a GitHub issue as the prompt / work item input — see [GitHub Integration](10-github-integration.md) |
| `--dynamic` | — | — | ✓ | Have a leader agent design the workflow; implies `--yolo`, `--worktree`, `context(workflow)` |
| `--leader <agent::model>` | — | — | ✓ | Leader agent and model for `--dynamic` |
| `--max-concurrent <N>` | — | — | ✓ | Cap concurrently-running workflow steps |

---

[← Using the TUI](02-using-the-tui.md) · [Next: Security & Isolation →](04-security-and-isolation.md)
