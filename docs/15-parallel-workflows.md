# Parallel Workflows

A workflow's steps don't have to run one at a time. Any steps that share the same [`depends_on`](05-workflows.md#step-fields) set — meaning neither depends on the other — form a **parallel group**, and awman runs them concurrently, each in its own container. This guide covers what that means in practice: how to control how many agents run at once, how the engine schedules them, and how stuck detection, yolo mode, and the workflow control board behave when more than one agent is active.

For the mechanics of writing workflow files (steps, `depends_on`, agents, models), see [Workflows](05-workflows.md). For how parallel containers actually appear on screen, see [Using the TUI](02-using-the-tui.md#parallel-containers) and [API Mode: Parallel agents in interactive CLI mode](09-api-mode.md#parallel-agents-in-interactive-cli-mode).

---

## What parallelism means here

Consider a workflow where `tests` and `docs` both depend only on `implement`, and nothing depends on either of them:

```
implement → tests
          → docs
       → review (depends on tests, docs)
```

`tests` and `docs` form a parallel group: once `implement` finishes, both become eligible to run, and awman launches both at once instead of waiting for one to finish before starting the other. `review` still waits for both to complete, since it depends on them.

This is entirely driven by your workflow file's `depends_on` graph — you don't opt into parallelism explicitly. Any steps whose dependencies are satisfied at the same time run together, up to the concurrency cap described below.

---

## Configuring `maxConcurrentAgents`

`maxConcurrentAgents` caps how many containers can run at once, machine-wide or per-repo. It's a plain [config field](07-configuration.md#reference), so it follows the same precedence as everything else:

```
--max-concurrent  >  AWMAN_MAX_CONCURRENT_AGENTS  >  repo config  >  global config  >  unlimited
```

```sh
awman config set maxConcurrentAgents 3              # this repo
awman config set --global maxConcurrentAgents 2     # every project on this machine
awman exec workflow workflow.toml --max-concurrent 4   # this run only
```

Left unset at every level, there is **no cap** — every step whose dependencies are satisfied launches immediately. In practice you'll usually want a cap that matches your machine's CPU/memory headroom and your Docker daemon's capacity, since each parallel step is a full container running its own agent.

A `maxConcurrentAgents` of `1` disables parallelism entirely: steps run one at a time, in the same order they would without any concurrency at all. `0` is rejected — if you want to pause parallelism, unset the field or set it to `1`.

> `dynamicWorkflows.maxConcurrentSteps` is a different, unrelated setting: it's an advisory hint passed to the leader agent that *designs* a `--dynamic` workflow. `maxConcurrentAgents` is what the engine actually enforces at run time, for any workflow, dynamic or not.

---

## How the engine schedules steps

When a parallel group becomes ready, awman launches as many of its steps as the concurrency cap allows, in the order they appear in the workflow file. Any remaining steps in the group wait in a queue.

- **A slot frees up** whenever a running step finishes successfully. The next queued step (in file order) starts immediately into that slot.
- **A step that fails** without `abort_on_failure` stops new steps from being queued into the group, but lets its already-running siblings keep going until they finish; you're then prompted the same way you would be for a sequential failure.
- **A step with `abort_on_failure = true` that fails** kills every other active step in the group immediately and cancels anything still queued — the same all-stop behavior `abort_on_failure` has always had, just applied to every running peer at once instead of a single step.

If a workflow resumes from a saved state mid-group, any steps that were interrupted are replayed; steps that had already succeeded stay succeeded.

---

## Stuck and yolo behavior, per container

Every running container is tracked independently — one noisy or slow agent never masks or delays detection on its siblings.

- **Stuck detection (yolo off):** if a container produces no output for 30 seconds, that container alone is marked stuck. Its siblings keep running unaffected. The stuck container's slot stays occupied — no new step launches into it — until you switch to it and send Ctrl-C to kill it, at which point its slot frees up like any other completion.
- **Yolo mode:** each container gets its own independent 60-second auto-advance countdown. When one container's countdown expires, only that container is killed and its step marked advanced; the rest of the group is untouched, and the next queued step (if any) starts into the freed slot. If the group has nothing left queued, the remaining containers simply keep running until they finish.

**Where the countdown appears in the TUI:** a yoloing container that is *not* the focused one shows its countdown in its minimized status bar (`Yolo in Ns`, flashing purple/yellow — see [Using the TUI: Parallel containers](02-using-the-tui.md#parallel-containers)). If it's the focused container, the countdown instead opens the same modal dialog a single container shows. Pressing **Ctrl-S** to rotate focus away closes that modal — the countdown itself keeps running in the background regardless — and rotating back onto a container still counting down reopens the modal automatically.

See [Yolo Mode](06-yolo-mode.md) for the general countdown behavior this builds on.

---

## The workflow control board with multiple agents running

Opening the control board (**Ctrl-W** in the TUI) while more than one agent is running scopes its actions to whichever container is currently **focused** — the one you'd switch to with Ctrl-S. The board makes this explicit: it names the focused step and shows how many peers are still running.

Some actions only make sense once the whole group has settled and are unavailable while any peer is still active:

| Action | Behavior with active peers |
|---|---|
| Restart current step | Disabled while any other agent in the group is still running, with a reason pointing you at Ctrl-S — restarting always targets the focused container, but only once its siblings have finished. |
| Cancel to previous step | Disabled while any peer is still running: rewinding a step in a group that's still mid-flight isn't well-defined until the group finishes. |
| Finish workflow | Disabled while any peer is still running, for the same reason. |
| Pause | Always available — suspends the whole workflow, killing every active container in the group. |
| Abort | Always available — same, but marks the workflow aborted rather than paused. |

When an action is unavailable, the reason is shown alongside it rather than just being greyed out silently.

Each parallel step gets its own control board when it completes or gets stuck; you're never blocked from acting on one step because another is still busy — you just can't ask the workflow as a whole to move backward or forward (cancel to a previous step, or finish) until the whole group has drained.

---

[← Cleaning Up](14-cleaning-up.md) · [Next: squad →](16-squad.md)
