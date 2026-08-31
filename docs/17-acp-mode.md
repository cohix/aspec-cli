# ACP Mode

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

---

## Start an ACP session

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

## What changes in ACP mode

ACP changes how awman displays and drives the session. You see structured
agent responses and activity through awman's interface, rather than the
agent's raw terminal UI. Permission requests are presented by awman, and an
ACP session can continue with follow-up prompts while it remains open.

The agent still works on the current project through awman's normal isolated
agent session. ACP is a presentation and interaction choice; it does not
change which project you opened or which agent you selected.

---

## ACP in the CLI

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

## ACP in the TUI

The full TUI agent window for ACP — a dedicated purple-bordered window with
inline structured updates, command-box follow-ups, and dialog permission
prompts — is **not yet available**. Launching an ACP session in the TUI renders
update summaries in the message area and does not present interactive follow-up
or permission dialogs; a permission request that is not pre-approved by
`--yolo`/`--auto` is denied.

For the interactive ACP experience today, run `awman chat --launch-mode acp`
from the CLI (outside the TUI).

---

## Selecting a launch mode

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

## Workflows and unsupported agents

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

[← amie](16-amie.md) · [Back to contents](contents.md)
