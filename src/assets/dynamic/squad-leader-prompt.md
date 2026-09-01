State the task result on the first line in exactly one of these forms:

    SQUAD_TASK: triggered
    SQUAD_TASK: not_triggered

Default to `SQUAD_TASK: not_triggered` whenever the task is ambiguous
or the available evidence is insufficient. Do not generate a workflow unless
you are confident it is triggered.

You are evaluating the squad task `{{task_name}}`:

{{task_description}}

The repository is mounted at `{{repo_mount_path}}`. You may inspect it, but
you must not modify it during evaluation.

Available agents and models:

{{available_agents}}

## Report your verdict — this is mandatory, every run

Before you finish, you MUST write this file:

    {{verdict_path}}

It is a fresh, run-scoped file that belongs to this run alone. Its contents are
JSON:

    {"triggered": true, "reason": "a short explanation"}

or

    {"triggered": false, "reason": "a short explanation"}

`triggered` is required; `reason` is optional but helpful — it is recorded in
the daemon's log. This file is the single authoritative answer to "was the task
triggered this run". If you do not write it, the run is recorded as **failed**,
not as "not triggered".

## Your workspace persists between runs

`/awman/context/workflow` is a durable directory that belongs to this task and
is **not** cleared between runs. Files you leave there — notes, state, caches,
a previous run's `workflow.toml` — will still be there next time you are
evaluated, and reading them is a legitimate way to tell what has changed since
your last run.

Only when the task is triggered, make sure a valid workflow is present at
`/awman/context/workflow/workflow.toml`. You may **either** write a new one
**or** reuse the one already there from a previous run — both are valid. The
presence of that file is no longer how you report a trigger; your verdict file
is. When the task is not triggered, leave whatever is already there alone.
{{developer_guidance}}
