# squad

squad is an always-on agent orchestrator, run and managed by `awman`, that
watches for tasks you define and automatically runs a dynamic workflow
when one fires. It lives inside `awman` — there's no separate program to
install or run — and is driven from the same CLI and TUI as everything else.

---

## When to use squad

squad is useful for routine, repeatable agent work you'd otherwise have to
trigger by hand every time it comes up:

- Triaging new issues as they're opened
- Reacting to a comment or label on a GitHub issue or PR
- Watching for failing tests on open PRs and pushing a fix
- Any other "when X happens in my repo, do Y" rule you want running in the
  background without babysitting it

For a single task you want to run right now, use `awman chat` or
`awman exec prompt` instead — see [Agent Sessions](03-agent-sessions.md) and
[API mode](09-api-and-remote-mode.md#one-shot-scripted-execution-exec).

---

## Tasks

A **task** is a user-authored "if... then..." rule: a trigger to watch
for, and what you want done when it fires. Some examples:

- "Whenever a new issue is opened in the awman repo, analyze it, draft a
  plan, and comment the plan on the issue."
- "Whenever I comment `/squad` on an issue, research the comment and post a
  followup with findings and/or an updated plan."
- "When the `ready-to-implement` label is added to an issue, implement the
  approved plan and open a PR."
- "If any open PR has failing tests, check out the branch, fix the failure,
  and push the fix."

On a regular interval (6 hours by default, configurable per task),
the squad daemon launches a **task-evaluation agent** with access to the
task's durable workspace at `~/.awman/squad/tasks/<name>/workspace/`. That agent
decides whether the task is currently met and, if so, writes or reuses a
workflow file describing what to do. The daemon then validates that workflow
and runs it unattended, the same way `awman exec workflow --dynamic` would.

Every task captures its effective workspace and mount scope at creation time.
The effective workspace may be the durable default workspace or a custom
folder/repository; see [Task workspaces](#task-workspaces) and [Guardrails for
unattended execution](#guardrails-for-unattended-execution) below.

---

## Defining a task

### From the CLI

```sh
awman squad add \
  --name issue-triage \
  --description "Whenever a new issue is opened, analyze it and post a plan as a comment." \
  --workspace default \
  --overlay "env(GITHUB_TOKEN)" \
  --interval 10m \
  --mount-scope gitroot
```

| Flag | Description |
|------|-------------|
| `--name <slug>` | Required. The task's identifier — lowercase letters, digits, and hyphens only (no leading/trailing hyphen), used to name its data directory and its containers. |
| `--description <text>` | Required. The natural-language "if... then..." rule the evaluation agent reads every tick. |
| `--workspace <default\|path>` | Bind the task to its durable default workspace, or to an existing custom folder/repository. If omitted, the default workspace is used. `--repo <path>` remains a legacy synonym for a custom workspace. |
| `--overlay <spec>` | Add a `dir()`, `ssh()`, `env()`, or `skill()` overlay to every container for this task. Repeat the flag for multiple overlays; malformed syntax is rejected when the task is created. See [Overlays](08-overlays.md). |
| `--interval <duration>` | How often the task is evaluated (default `6h`). |
| `--agent <name>` / `--model <name>` | Override the agent/model used for this task's evaluations, taking priority over the global `squad` config (see [Configuring squad](#configuring-squad) below). |
| `--mount-scope <cwd\|gitroot>` | Only meaningful for a custom workspace that is a Git repository root, where both values name that same root (default `gitroot`). Captured once and never changed later. Every other workspace — the default durable workspace, a plain directory, or a subdirectory of a repository — is mounted directly, as given. |
| `--interview` | Collect the task fields through the interactive interview, including the multiline description, workspace choice, and overlays. |
| `-n, --non-interactive` | Never prompt: refuse anything that would need a confirmation instead of asking. Cannot be combined with `--interview`. |

Task names are lowercase slugs: letters `a`–`z`, digits `0`–`9`, and
hyphens, starting and ending with a letter or digit. A name with uppercase
letters, underscores, spaces, or a leading or trailing hyphen is rejected
before the task is created.

Once created, manage it with the rest of the CRUD subcommands:

```sh
awman squad list                   # table of every task
awman squad show issue-triage      # description, schedule, and run history
awman squad pause issue-triage     # stop evaluating it without deleting it
awman squad resume issue-triage
awman squad remove issue-triage    # delete it
```

Add `--json` to `list`/`show`/`status` for machine-readable output
(`--json` implies non-interactive mode).

### Task workspaces

By default, a task uses its own durable workspace:
`~/.awman/squad/tasks/<name>/workspace/`. It is created when the task is
created and reused for every evaluation and workflow run. Files written there
survive between runs, including files left by a task whose agent stops
unexpectedly. The workspace is removed only when the task itself is removed
and you confirm deletion (or use `--yes`).

With `--workspace <path>`, the path must already exist. A path that is the
*root* of a Git repository is worktree-isolated for runs. Any other path — a
plain directory, or a subdirectory inside a repository — is mounted directly,
exactly as it was given: squad never widens a run's view from the folder you
picked to its enclosing repository. In the interactive interview, a custom
path that is not a Git repository root is kept only after a warning and
confirmation. A missing path is an error and is never created automatically.
These choices are captured when the task is created; squad does not silently
change workspace mode later.

If the custom path is a *parent* of the directory you are standing in, squad
asks you to confirm the wider mount scope first — the same confirmation every
other awman mount-scope flow applies. This holds whether you answered the
interview or passed `--workspace` on the command line; with `-n` the widening
is refused outright instead of being asked about.

A task whose workspace is a plain directory (the default workspace, or a
custom folder that is not a Git repository) has no project of its own to build
agent images from, so on its first run squad writes a `Dockerfile.dev` and a
`.awman/Dockerfile.<agent>` into that directory from the same bundled
templates `awman init` uses. Both are written only if absent: edit either one
to control how the task's containers are built, and squad will leave your
version alone from then on.

Images built from a default task workspace are tagged with the task's own
name — `awman-squad-<name>:latest` for the base image and
`awman-squad-<name>-<agent>:latest` per agent — so two tasks never share or
overwrite each other's images. (A custom workspace that is a repository uses
the same folder-derived tags as any other awman project in that repository.)

Whether the task uses the default workspace or a custom path, the durable
task workspace is also available to its containers through the
`context(workflow)` location. This gives custom-workspace tasks a stable place
for task-scoped files and data. Overlay specifications configured on a task
are additive with global, repository, environment, and workflow overlays; see
[Overlays](08-overlays.md) for the syntax and merge rules.

### From the TUI

Every action available on the command line is also available as a key
binding inside the squad tab (see [The squad tab](#the-squad-tab) below) —
pressing **n** to create a task, **p**/**r** to pause/resume, and **d**
to remove all dispatch through the same commands the CLI uses, just from
inside the tab instead of a terminal prompt.

Task creation is an all-or-nothing interview. The description uses a large
multiline editor with the prompt:

> Describe the new squad task including its triggering conditions and how
> squad should handle the task each time it is triggered

After the description and interval, choose **Default Task Workspace** or
**Custom Folder / Repo**. The custom choice asks for an existing path; a path
that is not the root of a Git repository produces a warning and offers
**keep this path** or **choose a different path**. You can then add overlays
one at a time using the same syntax as other awman commands; submit a blank
overlay entry when finished. Submitting an empty box is an answer — it keeps
the documented default, or ends the overlay list — but pressing **Esc**
dismisses the interview entirely, and nothing is saved.

---

## The squad tab

Rather than being a separate program, squad gets a dedicated, singleton tab
inside the ordinary multi-tab `awman` TUI — the same TUI you use for
project work. Unlike a normal tab it isn't bound to a working directory: it
shows the task list and lets you act on tasks from wherever you
happen to have `awman` open.

### Opening it

Two ways to get there:

- **`Ctrl-S` from the New Tab dialog.** Press **Ctrl-T** to open a new tab,
  and the prompt shows a second line — "Press Ctrl-S to open squad" —
  alongside the usual working-directory prompt. Pressing **Ctrl-S** while
  that dialog is focused closes the dialog and opens (or focuses) the squad
  tab instead of creating a directory-bound tab. This doesn't change what
  `Ctrl-S` does anywhere else — outside that dialog it keeps its usual
  meanings (cycling parallel container slots, submitting multiline dialogs).
- **Bare `awman squad` in a terminal.** Run `awman squad` with no subcommand
  in a TTY (and without `-n`/`--json`) and awman opens the TUI pre-focused
  on the squad tab.

There is at most one squad tab per running `awman` process — opening it again
just focuses the existing one rather than creating a second. You can run
more than one `awman` process, each with its own squad tab; the squad daemon
itself is what enforces there's only ever one instance of the daemon.

### What it looks like

The tab has its own colour — cyan — so it's never mistaken for a normal
project tab, and its label is always the fixed word `squad` regardless of
where squad's data actually lives on disk. Otherwise it behaves like any
other tab: it participates in `Ctrl-A`/`Ctrl-D` tab cycling, closes with the
usual close-tab flow, and keeps its state while you're on another tab.

The body of the tab is a generously spaced grid of rounded **task cards**.
Each card shows the task name, a short description summary, the outcome and
time of its last run (`workflow executed`, `not triggered`, `failed`,
`interrupted`, `running`, or `never run`), and its next scheduled evaluation —
which reads `paused` while the task is paused. Cards reflow as the terminal
is resized. Use the arrow keys to move in two dimensions, including across
the final partially filled row. Polling refreshes automatically every couple
of seconds while the tab is focused. If the daemon isn't reachable, the tab
shows that clearly above whatever tasks it last saw, rather than quietly
showing an empty list. With no tasks, the grid shows an empty-state prompt to
press **n** and create one.

`Ctrl-G` (the git sidebar) is a no-op on the squad tab, since it isn't bound
to a repository.

### Key bindings in the task list

| Key | Action |
|-----|--------|
| **↑ / ↓ / ← / →** | Move between task cards |
| **Enter** | Open a detail modal for the selected task — description, workspace, mount scope, interval, overlays, agent/model, timestamps, and run history |
| **a** | Attach to the task's currently running container(s) — see [Attaching](#attaching-to-a-running-task) |
| **n** | Create a new task |
| **p** | Pause the selected task |
| **r** | Resume the selected task |
| **d** | Remove the selected task (opens a `[y]es / [n]o` confirmation first) |

The detail modal includes the same task-scoped action hints: **a** attach,
**p** pause, **r** resume, **d** delete, and **Esc** close. Those keys act on
the task shown in the modal, even if the underlying card list has changed.

Global shortcuts (**Ctrl-T**, **Ctrl-A**, **Ctrl-D**, **Ctrl-M**, **Ctrl-O**,
**Ctrl-W**, **Ctrl-,**, **Ctrl-C**) keep their usual meaning while the task list has
focus — none of them are repurposed for squad. You can also type
`squad <subcommand> ...` directly into the command box at any time; the keys
above are shortcuts over the same path, not a separate one.

---

## Attaching to a running task

Only a **currently running** agent can be attached to — there's no way to
replay a finished run. A task has two things that can be running at
once:

- **The evaluation agent** — the agent deciding whether the task is
  met, before any workflow has been generated.
- **The generated workflow's containers** — once the evaluation agent has
  decided to act.

### From the CLI

```sh
awman squad attach issue-triage
```

If exactly one container is running for the task, this attaches your
terminal directly to the running agent's terminal UI. Squad-launched agents
run with a PTY even when nobody is attached, so attach reconnects to the
actual agent process rather than opening a shell beside it. If the task has
no run in progress right now,
the command fails immediately rather than pretending an old run is still
live. If more than one container is running (a generated workflow with
parallel steps), the command lists each one's short ID and label and asks
you to disambiguate:

```sh
awman squad attach issue-triage --container a1b2c3d4e5f6
```

Detaching (`Ctrl-C`, closing the terminal) only ends your local attach
session — the container and the daemon are left running, and you can
attach again later.

### From the TUI

Pressing **a** in the squad tab attaches to every running container for the
selected task at once, showing the actual agent TUIs and reproducing the same view you'd get from
`awman exec workflow --dynamic`: the Workflow Overview across the
bottom, one container maximized and the rest as minimized bars, and
**Ctrl-S** to cycle focus between them. If the task is still in its
evaluation phase (no workflow yet), you instead see the single evaluation
container with no Workflow Overview.

While attached, the daemon dying doesn't interrupt anything already
streaming — those are direct connections to the container runtime, not
proxied through the daemon — but the Workflow Overview freezes and the tab
shows a "daemon not reachable" indicator until the daemon comes back.

If the local attach client itself dies (for example, the container stopped a
moment earlier), the session ends and the client's exit code and final output
are written to the tab's status log, so a failed attach explains itself
rather than silently returning to the task grid.

Attach works on both runtimes, through different plumbing with the same
semantics. On **docker**, attach is a native `docker attach` to the agent's
TTY. Apple's `container` CLI has no attach verb, so on **apple-containers**
the awman process that launched the agent (the squad daemon, for squad tasks)
serves the agent's live terminal on a local, user-private socket, and attach
connects to that. Either way you reach the real agent TUI, several clients
can attach at once, and detaching never stops the container.

The one Apple-specific caveat: the launching process is the only holder of
the agent's terminal there, so if it has exited (say, the daemon was
restarted mid-run), attach reports that there is no live attach endpoint —
the agent's per-run log file still has everything it printed.

---

## Guardrails for unattended execution

Because a task's workflow can run with nobody watching, squad applies a
fixed set of guardrails to every run rather than leaving them optional:

- **Mount scope is captured once, at creation, and never widened later.**
  A custom workspace that is a repository root is the repository root, so
  `--mount-scope cwd` and `--mount-scope gitroot` name the same directory
  there. Every other workspace mounts directly; there is no repository root to
  widen to.
- **Worktree isolation follows the workspace type.** A custom workspace that
  is a Git repository *root* always runs in its own isolated worktree, exactly
  as `--worktree` does for a manual `exec workflow` run — see [Security &
  Isolation](04-security-and-isolation.md#worktree-isolation). The default
  durable workspace, custom non-Git directories, and subdirectories of a
  repository are mounted directly and never use a worktree. This decision is
  made when the task is created.
- **Every run is autonomous and PTY-backed.** There's no human around to
  answer a permission prompt, so the evaluation leader and every generated
  workflow run under the same auto-advance guardrails as `--yolo` — see
  [Permission modes](03-agent-sessions.md#permission-modes). An agent that
  goes quiet starts the standard stuck detection and 60-second yolo
  countdown; if it stays idle the run advances past it automatically, exactly
  as a dynamic workflow would. The countdown's start, cancellation, and
  auto-advance are recorded in the daemon log (never each tick). Agents still
  run in a terminal-sized PTY so attaching later shows the real interactive
  agent interface.
- **The durable workspace is preserved.** Squad never clears task files
  between runs. The task workspace is also mounted at the stable
  `context(workflow)` location, including for custom-workspace tasks.
- **Credentials are only ever injected at container startup**, the same as
  any other agent container, and are never written into a task's
  persistent directory where they'd survive across runs.
- **A sandbox runtime (`docker-sbx-experimental`) is refused entirely.**
  squad's task-directory mounts, its evaluation-agent handshake, and
  workflow setup/teardown steps all depend on a real container runtime.
  Every squad entry point — the daemon, `squad add`, the TUI — fails with a
  clear error naming the configured runtime rather than degrading silently.
  Set `runtime` to `docker` or `apple-containers` to use squad.

---

## Configuring squad

An optional `squad` block in the **global** config
(`~/.awman/config.json`) restricts which agents and models squad may use and
sets instructions every task evaluation must follow:

```json
{
  "squad": {
    "agentsToModels": {
      "claude": ["claude-opus-4-8", "claude-sonnet-4-6"]
    },
    "maxConcurrentEvaluations": 2,
    "defaultLeader": "claude::claude-opus-4-8",
    "guidance": ["Keep automated changes focused."]
  }
}
```

| Key | Meaning |
|-----|---------|
| `agentsToModels` | The agents and models squad is allowed to schedule. A task's own `--agent`/`--model` override still wins if set; this is the default pool, not a hard allowlist against per-task overrides. |
| `maxConcurrentEvaluations` | How many task evaluations can run at once across the whole daemon. |
| `defaultLeader` | The `agent::model` used for a task's evaluation when the task doesn't specify its own. |
| `guidance` | Standing instructions applied to **every** task evaluation and every workflow it generates — the same mechanism as [dynamic workflow guidance](06-dynamic-workflows.md#guidance), injected as a bulleted "Developer Guidance" block in the agent's prompt. |

This is all global rather than per-repo, because one squad daemon watches
tasks across every repo you point it at. Editing this block takes
effect on the daemon's next scheduling tick — no restart needed. See
[Configuration: Squad daemon configuration](07-configuration.md#squad-daemon-configuration)
for the full field reference, validation rules, and how to edit it with
`awman config set`.

---

## squad and `awman api`

The squad daemon and the `awman api` server are both long-lived processes
that hold the same shared database open, so **only one of them can run on a
machine at a time**. Starting either one while the other is running fails
immediately, before any port is bound or the database is opened, and the
error names the other process and its PID.

To switch from one to the other:

```sh
# Currently running awman api, want to use squad instead
awman api kill
awman squad start

# Currently running squad, want to use awman api instead
awman squad stop
awman api start --port 9876 --workdirs /path/to/repo
```

Any squad CLI command or TUI entry point that needs the daemon (bare
`awman squad`, `squad add`/`list`/`show`/etc., opening the squad tab) starts it
automatically if it isn't already running — you don't have to run
`awman squad start` yourself first, unless you want to pass daemon-specific
flags like `--port`. See [API server and squad daemon](09-api-and-remote-mode.md#api-server-and-squad-daemon)
for the same guarantee from the API server's side.

---

## Daemon lifecycle

`squad start`/`stop`/`status`/`logs` manage the daemon directly, mirroring
`awman api`:

```sh
awman squad start              # explicit start (usually unnecessary — see above)
awman squad status             # liveness and PID
awman squad logs               # the daemon's log output
awman squad stop               # alias: awman squad kill
```

Daemon runtime files live under `~/.awman/squad/`, a sibling of `~/.awman/api/`:

```text
~/.awman/squad/
  awman.pid
  awman.log
  squad_key.hash
```

Task and run records live in the same
shared database as API mode, at `~/.awman/data/awman.db` — see
[Storage layout](09-api-and-remote-mode.md#storage-layout).

### Logs for the daemon and its agents

`awman squad logs` tails the daemon log. It contains the chronological squad
lifecycle — administrator actions, scheduler ticks, task decisions, container
launches, workflow steps, worktrees, and outcomes — but not the raw output of
agents.

Each run keeps the output of each container in its own file:

```text
~/.awman/squad/tasks/<name>/runs/<run-id>/<container-name>.log
```

The evaluation agent and every generated-workflow container use this layout.
The files are written as the run progresses, so output remains available even
if a run stops unexpectedly. The daemon log and these per-container logs are
separate: use the latter when you need an agent's detailed terminal output.

Container *image build* output is kept out of the daemon log the same way.
Each build a task triggers writes its full output to its own file:

```text
~/.awman/squad/builds/<name>/<run-id>-<n>.log
```

The daemon log records one lifecycle line when a build starts and one when it
finishes or fails, each naming the image and the path of that build's log
file.

At the end of an evaluation, the leader records whether the task was
triggered for that specific run. An older `workflow.toml` by itself is not
enough to trigger a later run: an explicit not-triggered result wins, and a
missing or invalid result is reported as a failed evaluation.

---

## Authenticating to the daemon

squad serves its task data over a small HTTP surface on loopback, and the
CLI and TUI are clients of it. By default that surface requires a bearer key.

### The key and `AWMAN_SQUAD_KEY`

The first time the daemon starts, it mints a key, stores only its SHA-256 hash
in `~/.awman/squad/squad_key.hash`, and prints the plaintext **once** together
with the shell snippet that makes it usable:

```
╔════════════════════════════════════════════════════════════════════╗
║  squad API key (store this — it will not be shown again)            ║
║  954ec30c6719074e0ea952588461d079f97675424b8ecb53b5cbe76a9f06c96b  ║
╚════════════════════════════════════════════════════════════════════╝

Add this to ~/.zshrc so the awman CLI and TUI can authenticate to squad:

    export AWMAN_SQUAD_KEY=954ec30c6719074e0ea952588461d079f97675424b8ecb53b5cbe76a9f06c96b
```

Add that line to your shell startup file and reload it. Every later
`awman squad` command — and the squad TUI tab — reads `AWMAN_SQUAD_KEY` from the
environment and sends it as the bearer token. Without it the daemon answers
`401 Unauthorized`.

The snippet is tailored to your ``: zsh gets `~/.zshrc`, bash gets
`~/.bashrc`, and fish gets `set -gx AWMAN_SQUAD_KEY …` for
`~/.config/fish/config.fish`.

Only the hash is stored, so a lost key cannot be recovered — mint a new one:

```sh
awman squad stop
awman squad start --refresh-key   # prints a fresh key and snippet, then exits
awman squad start
```

### Running without a key — `--dangerously-skip-auth`

If you would rather not manage a key at all:

```sh
awman squad start --dangerously-skip-auth
```

This mints no key, writes no hash, and accepts unauthenticated requests. It is
a reasonable choice on a single-user machine because **the squad daemon binds to
127.0.0.1 exclusively** — there is no flag to expose it on another interface,
so nothing off the machine can reach it. What it does give up is isolation from
other local processes and users on the same host: any of them can drive squad,
which means launching agent containers against your repo. Prefer the key on
shared or multi-user machines.

The flag applies to the run it is passed to. It leaves any existing
`squad_key.hash` untouched, so a later plain `awman squad start` requires a key
again. While a skip-auth daemon is running, the CLI and TUI notice and send no
bearer token rather than minting a key you would never see.

---

## Watching for squad's own containers

Containers squad launches — both the evaluation agent and the workflow it
generates — are visible in `awman status` like any other agent container,
marked with an `squad:<task>` source instead of `session`, so you can
always tell what's running unattended versus what you started yourself. See
[Agent Sessions: Monitoring running agents](03-agent-sessions.md#monitoring-running-agents).

---

[← Runtimes](11-runtimes.md) · [Next: Cleaning Up →](13-cleaning-up.md)
