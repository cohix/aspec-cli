# awman 0.11: dynamic workflows, any model, any harness

Up to now, using `awman` meant keeping a handful of generic workflows around and reaching for whichever one fit. A workflow is just a TOML file that defines a setup phase, a graph of agent steps, and a teardown phase. So for each given project I'm working on I would write workflows like `implement-feature`, `debug-error`, `write-tests`, etc. and reuse them across every work item. This aproach does work, but it's a blunt way of tackling the goals at hand. The steps that actually go into "add a git sidebar to the TUI" look nothing like the steps for "fix this flaky integration test," and a one-size-fits-all `implement-feature` pipeline needed to be vague enough to cover both, therefore it was tuned for neither. What I wanted was a workflow built for *this* feature and *that* error, shaped around exactly what each one needs.

The obvious fix is to let an agent write the workflow. That idea isn't new, since the "big harnesses" have their own flavour of agents-orchestrating-agents now. But they all share the same ceiling: one vendor, one model family, one harness. You get Claude planning Claude, or nothing. I wanted something open, and something that could mix and match harnesses and model families to ensure that the biases and blind spots of one vendor get caught by another by design.

So this `awman` release adds `awman exec workflow --dynamic`, which is the piece that has been missing from awman since the concept of workflows was added so many releases ago.

---

```sh
# install or upgrade
curl -s https://prettysmart.dev/install/awman.sh | sh
```

---

## A leader agent builds the workflow, awman runs it

With v0.11, you can point `awman` at a work item and get out of the way:

```sh
awman exec workflow --dynamic --work-item 42
```

`awman` spins up a "leader" agent (containerized, as always), hands it your work item, a list of agents you configure, and your 'workflow guidance' (more below). It instructs the leader to design a `workflow.toml` that is hyper-specific to the work item at hand. When the leader finishes, `awman` validates the file (is it real TOML, does it only reference agents you actually have, etc). Once it's valid, `awman` executes it immediately, exactly as if you'd written it yourself. The entire thing happens in an isolated worktree with `--yolo` mode enabled by default for smooth hands-off execution.

The part I care about most is that the leader isn't confined to one model or one harness. It can schedule a Claude step, a Codex step, and an OpenCode step in the same graph, each in its own container, each on the model you've approved. `awman` already knew how to run all of those harnesses, so dynamic workflows just lets an agent compose them for you per task.

## Why this matters for the big stuff

Small tasks don't need any of this. "Rename this function," "bump a dependency," "fix an off-by-one" can be handled by a single agent in a single container, and a hand-written workflow would be overkill. Point this at the work that *doesn't* fit in one agent's head at once, though, and it changes how the whole task feels to run.

Think about what "add a git sidebar to the TUI" actually involves: a data layer that shells out to git and parses the output, a rendering layer in Ratatui, keybindings and focus handling, a status-bar summary, tests for each piece, and docs at the end. If you were to hand a single agent all of that in one prompt, it does what people do under load; it holds the first two concerns in focus and lets the rest blur. It writes the parser, starts the widget, loses the thread on focus handling, and forgets the tests entirely. The context fills with its own half-finished output, and the work gets worse exactly when the integration details matter most.

Decomposition is how we keep the current generation of agents on task, and watching the `awman` workflow leader do it automatically is what convinced me this approach was a huge improvement over hand-rolled workflows. It reads project code, sees the real 'edges' of the problem, and divvies up the work along them: a step for the git data layer, a dependent step for the widget that consumes it, a parallel step for docs, adverserial reviews, and remediation. Every step gets its own agent with a clean context window and one well-defined job to do well. A step that fails does so in isolation — you re-run that node, not the whole feature. And because the leader saw your actual source before planning, the jobs match your codebase instead of some generic template's idea of how a feature "should" be structured.

The multi-model aspect of `awman` dynamic workflows helps take this even further. Large features are not uniformly hard. The concurrency-safe git parsing wants your strongest model, but the doc updates and the boilerplate wiring do not. Pinning one expensive model to the entire job means you overpay for the easy 70% to get the hard 30% right. A dynamic workflow puts the strong model on the steps that need it, and a cheap one on the rest. Because the steps are independent nodes in a DAG, the cheap ones run in parallel while the expensive one grinds on the part that deserves the compute. This ensures you get the benefit of the best model exactly where it counts and the throughput of the fast ones everywhere else.

## Keeping the leader on the rails

An agent designing your pipeline is only useful if you can constrain it. A `dynamicWorkflows` block in `.awman/config.json` does that, and it lives in version control so your whole team shares it:

```json
{
  "dynamicWorkflows": {
    "agentsToModels": {
      "claude": ["claude-opus-4-8", "claude-sonnet-5"],
      "codex": ["gpt-5.6-luna", "gpt-5.6-terra", "gpt-5.6-sol"]
    },
    "defaultLeader": "claude::claude-fable-5",
    "maxConcurrentSteps": 3,
    "guidance": [
      "Always add a validation step after each implementation step.",
      "Emulate the BMAD 'adverserial review' technique when finalizing the work item"
    ]
  }
}
```

`agentsToModels` pins the exact agents and models the leader may pick from. `guidance` is a list of house rules that get injected straight into the leader's prompt (i.e. the constraints you're tired of repeating in every work item description). And because 0.11 also ships real DAG parallelism (`maxConcurrentAgents`, plus Ctrl-S to flip between running containers in the TUI), the workflows a leader designs actually run their independent steps at the same time instead of sequentially.

There's a full [Dynamic Workflows guide](https://github.com/prettysmartdev/awman/blob/main/docs/06-dynamic-workflows.md) covering the leader resolution order, the repair loop, and every edge case.

## Also in 0.11

A live git sidebar in the TUI (**Ctrl-G**) with a `+X -Y` summary in the status bar, an `awman clean` command to reclaim stopped containers and stale workflow data, and saved container failure logs so a step that dies on its own leaves a tail behind in `~/.awman/logs/`. Full details in the [release notes](https://github.com/prettysmartdev/awman/blob/main/docs/releases/v0.11.0.md).

---

Source and issues at [github.com/prettysmartdev/awman](https://github.com/prettysmartdev/awman). More at [prettysmart.dev](https://prettysmart.dev). Feedback, issues, and contributions all welcome.
