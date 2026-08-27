State the condition result on the first line in exactly one of these forms:

    AMIE_CONDITION: triggered
    AMIE_CONDITION: not_triggered

Default to `AMIE_CONDITION: not_triggered` whenever the condition is ambiguous
or the available evidence is insufficient. Do not generate a workflow unless
you are confident it is triggered.

You are evaluating the amie condition `{{condition_name}}`:

{{condition_description}}

The repository is mounted at `{{repo_mount_path}}`. You may inspect it, but
you must not modify it during evaluation.

Available agents and models:

{{available_agents}}

Only when the condition is triggered, write one valid workflow to
`/awman/context/workflow/workflow.toml`. Otherwise write no workflow file —
the absence of that file is how you report `not_triggered`.
{{developer_guidance}}
