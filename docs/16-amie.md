# amie

amie is an always-on agent orchestrator, run and managed by `awman`, that
watches for conditions you define and automatically runs a dynamic workflow
when one fires. It lives inside `awman` — there's no separate program to
install or run — and is driven from the same CLI and TUI as everything else.

---

## When to use amie

amie is useful for routine, repeatable agent work you'd otherwise have to
trigger by hand every time it comes up:

- Triaging new issues as they're opened
- Reacting to a comment or label on a GitHub issue or PR
- Watching for failing tests on open PRs and pushing a fix
- Any other "when X happens in my repo, do Y" rule you want running in the
  background without babysitting it

For a single task you want to run right now, use `awman chat` or
`awman exec prompt` instead — see [Agent Sessions](03-agent-sessions.md) and
[API Mode](09-api-mode.md#one-shot-scripted-execution-exec).

---

## Conditions

A **condition** is a user-authored "if... then..." rule: a trigger to watch
for, and what you want done when it fires. Some examples:

- "Whenever a new issue is opened in the awman repo, analyze it, draft a
  plan, and comment the plan on the issue."
- "Whenever I comment `/amie` on an issue, research the comment and post a
  followup with findings and/or an updated plan."
- "When the `ready-to-implement` label is added to an issue, implement the
  approved plan and open a PR."
- "If any open PR has failing tests, check out the branch, fix the failure,
  and push the fix."

On a regular interval (5 minutes by default, configurable per condition),
the amie daemon launches a **condition-evaluation agent** bound to a
persistent directory at `~/.awman/amie/conditions/<name>/`. That agent
decides whether the condition is currently met and, if so, writes a workflow
file describing what to do. The daemon then validates that workflow and runs
it unattended, the same way `awman exec workflow --dynamic` would.

Every condition belongs to a repo (its `repo_scope`) and captures a
**mount scope** — `cwd` or `gitroot` — at creation time; see
[Guardrails for unattended execution](#guardrails-for-unattended-execution)
below for what that controls.

---

## Defining a condition

### From the CLI

```sh
awman amie add \
  --name issue-triage \
  --description "Whenever a new issue is opened, analyze it and post a plan as a comment." \
  --interval 10m \
  --mount-scope gitroot
```

| Flag | Description |
|------|-------------|
| `--name <slug>` | Required. The condition's identifier — lowercase letters, digits, and hyphens only (no leading/trailing hyphen), used to name its data directory and its containers. |
| `--description <text>` | Required. The natural-language "if... then..." rule the evaluation agent reads every tick. |
| `--interval <duration>` | How often the condition is evaluated (default `5m`). |
| `--agent <name>` / `--model <name>` | Override the agent/model used for this condition's evaluations, taking priority over the global `amie` config (see [Configuring amie](#configuring-amie) below). |
| `--mount-scope <cwd\|gitroot>` | What gets mounted into the generated workflow's containers (default `gitroot`) — captured once and never changed later. |
| `-n, --non-interactive` | Suppress the summary the command otherwise prints. |

Condition names are lowercase slugs: letters `a`–`z`, digits `0`–`9`, and
hyphens, starting and ending with a letter or digit. A name with uppercase
letters, underscores, spaces, or a leading or trailing hyphen is rejected
before the condition is created.

Once created, manage it with the rest of the CRUD subcommands:

```sh
awman amie list                   # table of every condition
awman amie show issue-triage      # description, schedule, and run history
awman amie pause issue-triage     # stop evaluating it without deleting it
awman amie resume issue-triage
awman amie remove issue-triage    # delete it
```

Add `--json` to `list`/`show`/`status` for machine-readable output
(`--json` implies non-interactive mode).

### From the TUI

Every action available on the command line is also available as a key
binding inside the amie tab (see [The amie tab](#the-amie-tab) below) —
pressing **n** to create a condition, **p**/**r** to pause/resume, and **d**
to remove all dispatch through the same commands the CLI uses, just from
inside the tab instead of a terminal prompt.

---

## The amie tab

Rather than being a separate program, amie gets a dedicated, singleton tab
inside the ordinary multi-tab `awman` TUI — the same TUI you use for
project work. Unlike a normal tab it isn't bound to a working directory: it
shows the condition list and lets you act on conditions from wherever you
happen to have `awman` open.

### Opening it

Two ways to get there:

- **`Ctrl-A` from the New Tab dialog.** Press **Ctrl-T** to open a new tab,
  and the prompt shows a second line — "Press Ctrl-A to open amie" —
  alongside the usual working-directory prompt. Pressing **Ctrl-A** while
  that dialog is focused closes the dialog and opens (or focuses) the amie
  tab instead of creating a directory-bound tab. This doesn't change what
  `Ctrl-A` does anywhere else — outside that dialog it still switches to the
  previous tab.
- **Bare `awman amie` in a terminal.** Run `awman amie` with no subcommand
  in a TTY (and without `-n`/`--json`) and awman opens the TUI pre-focused
  on the amie tab.

There is at most one amie tab per running `awman` process — opening it again
just focuses the existing one rather than creating a second. You can run
more than one `awman` process, each with its own amie tab; the amie daemon
itself is what enforces there's only ever one instance of the daemon.

### What it looks like

The tab has its own colour — cyan — so it's never mistaken for a normal
project tab, and its label is always the fixed word `amie` regardless of
where amie's data actually lives on disk. Otherwise it behaves like any
other tab: it participates in `Ctrl-A`/`Ctrl-D` tab cycling, closes with the
usual close-tab flow, and keeps its state while you're on another tab.

The body of the tab is a condition list — name, status, last-run outcome,
and next scheduled evaluation — refreshed automatically every couple of
seconds while the tab is focused. Polling pauses while you're on a
different tab and resumes the moment you switch back, so a backgrounded
amie tab doesn't keep hitting the daemon for no reason. If the daemon isn't
reachable, the tab shows that clearly above whatever conditions it last
saw, rather than quietly showing an empty list.

`Ctrl-G` (the git sidebar) is a no-op on the amie tab, since it isn't bound
to a repository.

### Key bindings in the condition list

| Key | Action |
|-----|--------|
| **↑ / ↓** | Move the selection |
| **Enter** | Open a detail modal for the selected condition — description, mount scope, interval, agent/model, repo, timestamps, and its run history |
| **a** | Attach to the condition's currently running container(s) — see [Attaching](#attaching-to-a-running-condition) |
| **n** | Create a new condition |
| **p** | Pause the selected condition |
| **r** | Resume the selected condition |
| **d** | Remove the selected condition (opens a `[y]es / [n]o` confirmation first) |

Global shortcuts (**Ctrl-T**, **Ctrl-A**, **Ctrl-D**, **Ctrl-M**, **Ctrl-W**,
**Ctrl-,**, **Ctrl-C**) keep their usual meaning while the condition list has
focus — none of them are repurposed for amie. You can also type
`amie <subcommand> ...` directly into the command box at any time; the keys
above are shortcuts over the same path, not a separate one.

---

## Attaching to a running condition

Only a **currently running** agent can be attached to — there's no way to
replay a finished run. A condition has two things that can be running at
once:

- **The evaluation agent** — the agent deciding whether the condition is
  met, before any workflow has been generated.
- **The generated workflow's containers** — once the evaluation agent has
  decided to act.

### From the CLI

```sh
awman amie attach issue-triage
```

If exactly one container is running for the condition, this attaches your
terminal directly to it. If the condition has no run in progress right now,
the command fails immediately rather than pretending an old run is still
live. If more than one container is running (a generated workflow with
parallel steps), the command lists each one's short ID and label and asks
you to disambiguate:

```sh
awman amie attach issue-triage --container a1b2c3d4e5f6
```

Detaching (`Ctrl-C`, closing the terminal) only ends your local attach
session — the container and the daemon are left running, and you can
attach again later.

### From the TUI

Pressing **a** in the amie tab attaches to every running container for the
selected condition at once, reproducing the same view you'd get from
`awman exec workflow --dynamic`: the workflow state strip across the
bottom, one container maximized and the rest as minimized bars, and
**Ctrl-S** to cycle focus between them. If the condition is still in its
evaluation phase (no workflow yet), you instead see the single evaluation
container with no strip.

While attached, the daemon dying doesn't interrupt anything already
streaming — those are direct connections to the container runtime, not
proxied through the daemon — but the workflow strip freezes and the tab
shows a "daemon not reachable" indicator until the daemon comes back.

---

## Guardrails for unattended execution

Because a condition's workflow can run with nobody watching, amie applies a
fixed set of guardrails to every run rather than leaving them optional:

- **Mount scope is captured once, at creation, and never widened later.**
  `--mount-scope cwd` or `--mount-scope gitroot` decides what the condition's
  workflow containers can see on disk; a condition can't quietly gain access
  to more of the filesystem after it's created.
- **Worktree isolation is always forced on.** Every generated workflow runs
  in its own isolated Git worktree, exactly as `--worktree` does for a
  manual `exec workflow` run — see [Security & Isolation](04-security-and-isolation.md#worktree-isolation).
  This is what lets two conditions that both touch the same repo run at the
  same time without stepping on each other's working tree.
- **Every run is non-interactive and fully autonomous.** There's no human
  around to answer a permission prompt, so generated workflows always run
  with the same auto-advance guardrails as `--yolo` — see
  [Yolo Mode](06-yolo-mode.md).
- **Credentials are only ever injected at container startup**, the same as
  any other agent container, and are never written into a condition's
  persistent directory where they'd survive across runs.
- **A sandbox runtime (`docker-sbx-experimental`) is refused entirely.**
  amie's condition-directory mounts, its evaluation-agent handshake, and
  workflow setup/teardown steps all depend on a real container runtime.
  Every amie entry point — the daemon, `amie add`, the TUI — fails with a
  clear error naming the configured runtime rather than degrading silently.
  Set `runtime` to `docker` or `apple-containers` to use amie.

---

## Configuring amie

An optional `amie` block in the **global** config
(`~/.awman/config.json`) restricts which agents and models amie may use and
sets instructions every condition evaluation must follow:

```json
{
  "amie": {
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
| `agentsToModels` | The agents and models amie is allowed to schedule. A condition's own `--agent`/`--model` override still wins if set; this is the default pool, not a hard allowlist against per-condition overrides. |
| `maxConcurrentEvaluations` | How many condition evaluations can run at once across the whole daemon. |
| `defaultLeader` | The `agent::model` used for a condition's evaluation when the condition doesn't specify its own. |
| `guidance` | Standing instructions applied to **every** condition evaluation and every workflow it generates — the same mechanism as [dynamic workflow guidance](13-dynamic-workflows.md#guidance), injected as a bulleted "Developer Guidance" block in the agent's prompt. |

This is all global rather than per-repo, because one amie daemon watches
conditions across every repo you point it at. Editing this block takes
effect on the daemon's next scheduling tick — no restart needed. See
[Configuration: Amie daemon configuration](07-configuration.md#amie-daemon-configuration)
for the full field reference, validation rules, and how to edit it with
`awman config set`.

---

## amie and `awman api`

The amie daemon and the `awman api` server are both long-lived processes
that hold the same shared database open, so **only one of them can run on a
machine at a time**. Starting either one while the other is running fails
immediately, before any port is bound or the database is opened, and the
error names the other process and its PID.

To switch from one to the other:

```sh
# Currently running awman api, want to use amie instead
awman api kill
awman amie start

# Currently running amie, want to use awman api instead
awman amie stop
awman api start --port 9876 --workdirs /path/to/repo
```

Any amie CLI command or TUI entry point that needs the daemon (bare
`awman amie`, `amie add`/`list`/`show`/etc., opening the amie tab) starts it
automatically if it isn't already running — you don't have to run
`awman amie start` yourself first, unless you want to pass daemon-specific
flags like `--port`. See [API Mode: API server and amie daemon](09-api-mode.md#api-server-and-amie-daemon)
for the same guarantee from the API server's side.

---

## Daemon lifecycle

`amie start`/`stop`/`status`/`logs` manage the daemon directly, mirroring
`awman api`:

```sh
awman amie start              # explicit start (usually unnecessary — see above)
awman amie status             # liveness and PID
awman amie logs               # the daemon's log output
awman amie stop               # alias: awman amie kill
```

Daemon runtime files live under `~/.awman/amie/`, a sibling of `~/.awman/api/`
(pidfile, log file, key hash). Condition and run records live in the same
shared database as API mode, at `~/.awman/data/awman.db` — see
[API Mode: Storage layout](09-api-mode.md#storage-layout).

---

## Authenticating to the daemon

amie serves its condition data over a small HTTP surface on loopback, and the
CLI and TUI are clients of it. By default that surface requires a bearer key.

### The key and `AWMAN_AMIE_KEY`

The first time the daemon starts, it mints a key, stores only its SHA-256 hash
in `~/.awman/amie/amie_key.hash`, and prints the plaintext **once** together
with the shell snippet that makes it usable:

```
╔════════════════════════════════════════════════════════════════════╗
║  amie API key (store this — it will not be shown again)            ║
║  954ec30c6719074e0ea952588461d079f97675424b8ecb53b5cbe76a9f06c96b  ║
╚════════════════════════════════════════════════════════════════════╝

Add this to ~/.zshrc so the awman CLI and TUI can authenticate to amie:

    export AWMAN_AMIE_KEY=954ec30c6719074e0ea952588461d079f97675424b8ecb53b5cbe76a9f06c96b
```

Add that line to your shell startup file and reload it. Every later
`awman amie` command — and the amie TUI tab — reads `AWMAN_AMIE_KEY` from the
environment and sends it as the bearer token. Without it the daemon answers
`401 Unauthorized`.

The snippet is tailored to your ``: zsh gets `~/.zshrc`, bash gets
`~/.bashrc`, and fish gets `set -gx AWMAN_AMIE_KEY …` for
`~/.config/fish/config.fish`.

Only the hash is stored, so a lost key cannot be recovered — mint a new one:

```sh
awman amie stop
awman amie start --refresh-key   # prints a fresh key and snippet, then exits
awman amie start
```

### Running without a key — `--dangerously-skip-auth`

If you would rather not manage a key at all:

```sh
awman amie start --dangerously-skip-auth
```

This mints no key, writes no hash, and accepts unauthenticated requests. It is
a reasonable choice on a single-user machine because **the amie daemon binds to
127.0.0.1 exclusively** — there is no flag to expose it on another interface,
so nothing off the machine can reach it. What it does give up is isolation from
other local processes and users on the same host: any of them can drive amie,
which means launching agent containers against your repo. Prefer the key on
shared or multi-user machines.

The flag applies to the run it is passed to. It leaves any existing
`amie_key.hash` untouched, so a later plain `awman amie start` requires a key
again. While a skip-auth daemon is running, the CLI and TUI notice and send no
bearer token rather than minting a key you would never see.

---

## Watching for amie's own containers

Containers amie launches — both the evaluation agent and the workflow it
generates — are visible in `awman status` like any other agent container,
marked with an `amie:<condition>` source instead of `session`, so you can
always tell what's running unattended versus what you started yourself. See
[Agent Sessions: Monitoring running agents](03-agent-sessions.md#monitoring-running-agents).

---

[← Parallel Workflows](15-parallel-workflows.md) · [← Back to contents](contents.md)
