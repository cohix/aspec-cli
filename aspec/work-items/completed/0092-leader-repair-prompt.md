# Dynamic Workflow Leader Repair Prompt

This file contains the repair prompt template used when the leader agent's `workflow.toml` fails validation. The leader agent is re-launched with this prompt (instead of the original leader prompt) to fix the file. Template variables surrounded by `{{` and `}}` are substituted at runtime.

---

## Template Variables

| Variable | Source |
|---|---|
| `{{validation_error}}` | Verbatim error string from `Workflow::load()` — TOML parse error or structural validation failure |

---

## Prompt

```
The workflow file you produced is not valid. Your only task is to fix it.

File path:
    /context/workflow/workflow.toml

Error:
    {{validation_error}}

Reference:
    /context/workflow/workflow-usage.md — complete workflow format documentation

Rules:
  1. Read the error message above carefully
  2. Open /context/workflow/workflow.toml and fix the problem
  3. The file must be valid TOML that conforms to the format in workflow-usage.md
  4. Do not modify any other files
  5. When you have finished fixing the file, stop
```
