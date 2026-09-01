# Work Item: Feature

Title: GitHub Integration Part 1 — Issue-Driven Spec Creation and Workflow Execution
Issue: issuelink

## Summary:

This work item introduces an `IssueSource` trait that abstracts external issue tracker integrations behind a provider-generic interface, and an `IssueSourceRouter` that selects the correct provider at runtime based on the input string. GitHub is the first implementation. Future providers (Jira, Linear, etc.) conform to the same trait and produce the same generic types — adding a new provider requires only a new `IssueSource` implementation and a registration entry in the router.

A single `--issue <ref>` flag is added to three commands:

1. `new spec --issue <ref>` — the router resolves the ref to a provider, fetches the issue, and uses its content as the input for spec creation, mirroring the `--interview` flow. When both flags are passed, the issue text is pre-populated in the interview text box for user editing before the agent runs.

2. `exec workflow --issue <ref>` — the router resolves and fetches the issue and provides its content to the workflow engine as if it were a local work item file, supporting the same template substitutions. The content is written to a temp file mounted inside agent containers at the correct configured work-items path (not written to the local repo).

3. `exec prompt --issue <ref>` — the router resolves and fetches the issue and passes its title and description verbatim as the prompt to the agent container. When both `--issue` and a positional prompt argument are provided, the issue content is appended to the user-provided text. When `--issue` is used alone, the positional prompt argument is not required.

A bare integer passed to `--issue` is treated as a GitHub issue number; awman uses the current repo's git remote to resolve the full GitHub URL. A URL or structured short-form (e.g. `owner/repo#84`) is routed to the matching provider via `can_handle()` on each registered `IssueSource`.

GitHub issues are fetched via the `gh` CLI if available and authenticated, with a fallback to the GitHub REST API using `GITHUB_TOKEN` from the environment (unauthenticated API access for public repos is the final fallback).


## User Stories

### User Story 1:
As a: user

I want to: run `new spec --issue 84` (or with a full GitHub issue URL) and have awman automatically pull the issue description and launch an agent to generate a work item spec from it

So I can: bootstrap structured work item specs directly from GitHub issues without copy-pasting content manually

### User Story 2:
As a: user

I want to: run `exec workflow my-workflow --issue 84` and have awman fetch the GitHub issue and treat it as the work item input for the workflow, with template variables like `{{work_item_content}}` resolved from the issue body

So I can: run agentic workflows against GitHub issues without first creating a local work item file

### User Story 3:
As a: user

I want to: run `new spec --issue 84 --interview` and have the GitHub issue description pre-populated in the interview text box so I can review and edit it before the spec agent runs

So I can: use a GitHub issue as a starting point while still curating the prompt before execution

### User Story 4:
As a: user

I want to: run `exec prompt --issue 84` and have the issue title and description sent directly as the prompt to the agent container, and optionally prepend my own context with `exec prompt "additional context" --issue 84`

So I can: direct an agent at any GitHub issue without writing a work item file or workflow, and supplement the issue content with my own framing when needed


## Implementation Details:

### `IssueSource` Trait and Generic Types (`src/data/issue/mod.rs`)

Create `src/data/issue/mod.rs` as the provider-generic foundation. The command layer and workflow engine import only from this module — never from any provider-specific file.

**`Issue` struct** — the generic output of every `IssueSource`:
```rust
pub struct Issue {
    pub source_id: String, // canonical URL of the issue, e.g. "https://github.com/owner/repo/issues/84"
    pub title: String,
    pub body: String,      // empty string if the issue has no description
    pub provider: String,  // display name from IssueSource::provider_name()
}

impl Issue {
    /// Parses the last path segment of `source_id` as a u32, if possible.
    /// Returns Some(84) for ".../issues/84", None for ".../PROJ-123".
    pub fn numeric_id(&self) -> Option<u32> { ... }
}
```

`source_id` is the canonical URL rather than an opaque identifier — it is unambiguous across providers, usable as a stable reference, and allows `numeric_id()` to be a pure derivation rather than a separately stored field.

**`IssueSourceError` enum** — every variant carries a `provider` field sourced from `IssueSource::provider_name()` at the call site, never hardcoded:
```rust
pub enum IssueSourceError {
    NotFound          { provider: String, source_id: String },
    Unauthorized      { provider: String },
    RateLimited       { provider: String },
    InvalidRef        { provider: String, input: String, hint: String },
    NoRemoteDetected  { provider: String },
    NoMatchingProvider{ input: String },
    Network           { provider: String, detail: String },
    ProviderError     { provider: String, detail: String },
}
```

`NoMatchingProvider` has no `provider` field because the provider is precisely what could not be determined. All other variants include `provider` so their `Display` impls produce accurate, context-rich messages without any hardcoded strings in the command layer.

**`IssueSource` trait**:
```rust
pub trait IssueSource {
    /// Human-readable provider name, e.g. "GitHub", "Jira", "Linear".
    fn provider_name(&self) -> &str;

    /// Returns true if this provider can handle the given input string.
    /// Called by IssueSourceRouter to select the correct implementation.
    /// Must be infallible and perform no I/O — pattern matching only.
    fn can_handle(&self, input: &str) -> bool;

    /// Fetch the issue identified by `input`, using `git_root` for context
    /// (e.g. detecting the remote URL for bare numeric refs).
    fn fetch_issue(&self, input: &str, git_root: &Path) -> Result<Issue, IssueSourceError>;

    /// Returns a hyphen-delimited, lowercase string that uniquely identifies
    /// this issue in the provider's native form. Used as the slug component of
    /// work item filenames (e.g. `0084-owner-repo-84-short-title.md`) and as
    /// the workflow-name component of worktree branch names (e.g.
    /// `awman/workflow-owner-repo-84-short-title`, following the existing
    /// `branch_name_for_workflow` convention). Must contain only lowercase
    /// alphanumerics and hyphens, no leading or trailing hyphens, and must
    /// be unique for a particular issue within the provider.
    /// No default — each provider encodes its own canonical identifier form.
    fn title_slug(&self, issue: &Issue) -> String;

    /// Render the issue as markdown for use in prompts and work item files.
    /// Default: `# {title}\n\n{body}`. Providers may override to include
    /// additional metadata (labels, assignee, priority, etc.).
    fn format_as_markdown(&self, issue: &Issue) -> String {
        if issue.body.is_empty() {
            format!("# {}", issue.title)
        } else {
            format!("# {}\n\n{}", issue.title, issue.body)
        }
    }
}
```

**`slugify()` utility** — shared by all provider implementations:
```rust
/// Converts arbitrary text to a hyphen-delimited, lowercase slug safe for
/// use in filenames and git branch names. Lowercases, replaces runs of
/// non-alphanumeric characters with a single hyphen, strips leading/trailing
/// hyphens, and truncates to `max_len` chars (truncation never splits mid-word
/// — it strips trailing hyphens after the cut). Non-ASCII characters are
/// treated as non-alphanumeric.
pub fn slugify(text: &str, max_len: usize) -> String { ... }
```

Lives in `src/data/issue/mod.rs` so all providers share identical slugification behaviour.

**`IssueSourceFlags` struct** — composed into `NewSpecFlags` and `ExecWorkflowCommandFlags`; carries the single `--issue` flag value:
```rust
pub struct IssueSourceFlags {
    pub issue: Option<String>,
}
```

No provider-specific fields. When a second provider is added, only the router registration changes — `IssueSourceFlags` and every command that uses it are unaffected.

### `IssueSourceRouter` (`src/data/issue/router.rs`)

The router holds all registered providers and dispatches `fetch_issue` calls to the correct one. It is the only place in the codebase that references concrete `IssueSource` implementations.

```rust
pub struct IssueSourceRouter {
    sources: Vec<Box<dyn IssueSource>>,
}

impl IssueSourceRouter {
    /// Constructs a router with all built-in providers registered.
    /// Adding a new provider means adding it here — nowhere else.
    pub fn default() -> Self;

    /// Returns the first provider whose `can_handle(input)` returns true.
    /// If no provider claims the input, returns `IssueSourceError::NoMatchingProvider`.
    pub fn route(&self, input: &str) -> Result<&dyn IssueSource, IssueSourceError>;

    /// Convenience: route, then fetch.
    pub fn fetch_issue(&self, input: &str, git_root: &Path) -> Result<(Issue, &dyn IssueSource), IssueSourceError>;
}
```

`fetch_issue` returns both the `Issue` and the `&dyn IssueSource` that produced it, so the caller can call `source.format_as_markdown(&issue)` without re-routing.

`IssueSourceRouter::default()` registers providers in priority order. `GithubIssueSource` is registered first (and claims bare integers via `can_handle`); future providers are added after it. Provider ordering determines which is tried first for ambiguous inputs.

### GitHub Implementation (`src/data/issue/github.rs`)

`GithubIssueSource` implements `IssueSource`. All GitHub-specific logic is contained here.

**`provider_name()`**: returns `"GitHub"`.

**`can_handle(input)`**: returns true for:
- Bare integers (e.g. `"84"`) — GitHub is the designated default for numeric-only refs
- `https://github.com/...` or `http://github.com/...` URLs
- `owner/repo#N` short form (contains `#`, no URL scheme)

Returns false for all other inputs (other provider URLs, other structured formats).

**`fetch_issue(input, git_root)`**:
1. Parse `input` into a resolved `(owner, repo, number)` triple using `can_handle` patterns:
   - Bare integer: run `git remote get-url origin` to extract owner/repo from the GitHub remote; return `NoRemoteDetected` (with `provider_name()`) if no GitHub remote is found
   - `owner/repo#N`: split on `#`
   - Full URL: parse path segments
   - Non-`github.com` URL: return `InvalidRef` with a hint string produced by this implementation
2. Try `gh issue view {number} --repo {owner}/{repo} --json number,title,body,url` via `std::process::Command`; on success, use the returned `url` field as `source_id`
3. If `gh` is absent or exits non-zero, fall back to `GET https://api.github.com/repos/{owner}/{repo}/issues/{number}` via `reqwest`; add `Authorization: Bearer {token}` if `GITHUB_TOKEN` is set; construct `source_id` as `https://github.com/{owner}/{repo}/issues/{number}`
4. Map HTTP status codes to `IssueSourceError` variants; always pass `self.provider_name().to_string()` as the `provider` field

**`title_slug(issue)`**: extracts `owner`, `repo`, and `number` by parsing the path segments of `issue.source_id`, then combines them with a slugified title:
```
{owner}-{repo}-{number}-{slugify(issue.title, 40)}
```
Example: `source_id = "https://github.com/prettysmartdev/awman/issues/84"`, `title = "GitHub Integration Part 1"` → `prettysmartdev-awman-84-github-integration-part-1`.

If `slugify(issue.title, 40)` returns an empty string (title was blank or all non-alphanumeric), the title component is omitted: `{owner}-{repo}-{number}`. This ensures the slug is always valid and unique even without a usable title. The overall slug is additionally truncated at a reasonable maximum (e.g. 100 chars) with trailing hyphens stripped, so very long owner or repo names do not produce an unusable result.

`format_as_markdown()` uses the default trait implementation; override when GitHub-specific metadata should be included.

### CLI Flag Changes

Add `--issue` to the CLI arg definitions for both commands, populating `IssueSourceFlags::issue`.

- **`NewSpecFlags`** (`src/command/commands/specs.rs`): add `issue_source: IssueSourceFlags`
- **`ExecWorkflowCommandFlags`** (`src/command/commands/exec_workflow.rs`): add `issue_source: IssueSourceFlags`; validate that `issue_source.issue` and `--work-item` are mutually exclusive before any routing or network call

### `new spec` flow (`src/command/commands/specs.rs`)

In `create_new_spec()`:

1. If `flags.issue_source.issue` is `Some(ref_str)`, call `router.fetch_issue(&ref_str, git_root)` to get `(issue, source)`
2. Use `source.title_slug(&issue)` as the filename slug — do not prompt for a title when `--issue` is set; the issue title drives the slug. The generated work item file is named `{NNNN}-{title_slug}.md` using the existing sequential work-item numbering for NNNN
3. Call `source.format_as_markdown(&issue)` to produce the prefill string
4. Determine behavior based on flag combination:
   - **`--issue` only** (no `--interview`): treat as implicit `--interview`; pass the formatted content directly to `render_interview_prompt()` and launch the agent without prompting for a summary
   - **`--issue` + `--interview`**: call `ask_spec_summary_prefilled(content)` on the frontend to show the text box pre-populated with the formatted content for user editing, then proceed as normal `--interview`
5. The command layer holds only `Issue` and `&dyn IssueSource` — no provider-specific types

Extend `SpecsCommandFrontend` with `ask_spec_summary_prefilled(&self, content: &str) -> Result<String>`; the default implementation ignores the prefill (for headless/non-interactive frontends); the TUI implementation renders it in the input widget.

### `exec workflow` flow (`src/command/commands/exec_workflow.rs`)

In the workflow execution path where `work_item` context is resolved:

1. If `flags.issue_source.issue` is `Some(ref_str)`, call `router.fetch_issue(...)` to get `(issue, source)`
2. Call `source.title_slug(&issue)` once and bind the result — it is used for the temp filename, the container-side filename, and (when applicable) the branch name
3. Call `source.format_as_markdown(&issue)` to produce the content string
4. Construct `WorkItemContext { number: issue.numeric_id().unwrap_or(0), content: formatted_markdown }`; this satisfies all template substitutions (`{{work_item_content}}`, `{{work_item_number}}`, `{{work_item_section:[Name]}}`, etc.)
5. Write the content to a temp file at:
   ```
   {temp_dir}/awman-issue-{pid}-{title_slug}.md
   ```
   PID prevents collisions between concurrent workflows targeting the same issue
6. Determine the container-side path:
   - Resolve the configured work-items dir (`repo_config.work_items_dir_or_default(git_root)`)
   - Compute its path relative to `git_root` and combine with the container workspace root (`/workspace`)
   - Filename: `{NNNN}-{title_slug}.md` where NNNN = `issue.numeric_id()` zero-padded to 4 digits, or `0000` if None
7. Add `OverlaySpec { host_path: temp_path, container_path: container_target, permission: ReadOnly }` to every container in the workflow (injected in `CommandLayerFactory::build()` alongside existing overlays)
8. When `--worktree` is also set, use `title_slug` as the workflow-name component passed to the existing `WorktreeLifecycle::for_workflow` / `branch_name_for_workflow` helper, producing `awman/workflow-{title_slug}` (substituting the issue slug in place of the workflow filename slug)
9. After workflow completion (success or error), delete the temp file via a `scopeguard` or `Drop` wrapper to guarantee cleanup

The overlay builder function signature is `issue_source_overlay(source: &dyn IssueSource, issue: &Issue, ...) -> OverlaySpec` — no concrete provider types.

### `exec prompt` flow (`src/command/commands/exec_prompt.rs`)

**`ExecPromptCommandFlags` changes**:
- `prompt: String` → `prompt: Option<String>` — no longer unconditionally required; may be absent when `--issue` supplies the full prompt
- Add `issue_source: IssueSourceFlags`

**Catalogue changes** (`src/command/dispatch/catalogue.rs`):
- The `prompt` positional argument's `optional` field changes from `false` to `true`
- `EXEC_PROMPT` must define its own flags slice (`EXEC_PROMPT_FLAGS`) rather than reusing `&AGENT_RUN_FLAGS_NO_WORKTREE`, because `--issue` belongs on `exec prompt` only and must not affect `chat`. `EXEC_PROMPT_FLAGS` contains all entries from `AGENT_RUN_FLAGS_NO_WORKTREE` plus the `--issue` flag:
  ```
  FlagSpec { long: "issue", kind: FlagKind::String, optional: true, ... }
  ```

**Command handler logic** in `run_with_frontend()`:

1. Validate that at least one of `flags.prompt` and `flags.issue_source.issue` is `Some`; return `CommandError` if neither is provided — this guards the case where the user omits the positional argument without supplying `--issue`
2. If `flags.issue_source.issue` is `Some(ref_str)`, call `router.fetch_issue(&ref_str, session.git_root())` to get `(issue, source)`; call `source.format_as_markdown(&issue)` to produce the issue string
3. Construct the final prompt string:
   - Only user text (`--issue` absent): use `flags.prompt` as-is — identical to current behaviour
   - Only `--issue` (no positional prompt): use the issue markdown string directly
   - Both: `format!("{}\n\n{}", user_prompt, issue_markdown)` — user text first, issue appended
4. Pass the final prompt string as `initial_prompt: Some(final_prompt)` in `AgentRunOptions` — exactly as today, just with a dynamically constructed value

No files are created, no temp files are written, no branch names are derived. `title_slug()` is not called. The issue content is inlined into the prompt string and the container is launched identically to the non-issue path.

### Module layout

```
src/data/issue/
  mod.rs      — IssueSource trait (with title_slug()), Issue (with numeric_id()),
                IssueSourceError, IssueSourceFlags, slugify()
  router.rs   — IssueSourceRouter
  github.rs   — GithubIssueSource
```

Add `pub mod issue;` to `src/data/mod.rs`.


## Edge Case Considerations:

- **Bare integer with no GitHub remote**: `GithubIssueSource::fetch_issue` returns `IssueSourceError::NoRemoteDetected { provider: "GitHub" }`; the `provider` field in the error drives the displayed message — no literal provider name in the command layer
- **Input not claimed by any provider**: `IssueSourceRouter::route` returns `IssueSourceError::NoMatchingProvider { input }`; the command layer surfaces this directly
- **Issue does not exist**: `IssueSourceError::NotFound { provider, source_id }` — `source_id` is populated with whatever canonical URL the provider was attempting to fetch
- **Private repo / hidden issue**: `gh` auth error or API 404; `GithubIssueSource` maps these to `Unauthorized` or `NotFound`; hint text (e.g. "ensure gh is authenticated or GITHUB_TOKEN is set") is produced by the implementation
- **`gh` not installed**: caught as an OS "not found" error from `std::process::Command`; silently fall through to the REST API with no user-visible log noise
- **`gh` installed but not authenticated**: non-zero exit; fall through to REST API
- **API rate limiting (403/429)**: `IssueSourceError::RateLimited { provider }`; the provider may embed a hint via `ProviderError`
- **Network unavailable**: `IssueSourceError::Network { provider, detail }`
- **Issue body is empty or null**: `Issue::body` is an empty string; `format_as_markdown` produces just the title line
- **`--issue` and `--work-item` both set**: command validates mutual exclusion before calling the router; error message does not reference any specific provider
- **`--interview` with non-TUI frontend**: `ask_spec_summary_prefilled` default impl silently ignores the prefill; no crash
- **Temp file cleanup on error**: `scopeguard`/`Drop` wrapper ensures deletion regardless of workflow exit path
- **Concurrent workflows on the same issue**: PID component in the temp filename prevents collisions
- **Issue title is empty or entirely non-alphanumeric**: `slugify()` returns `""`; `GithubIssueSource::title_slug()` omits the title component and returns `{owner}-{repo}-{number}` — still unique and valid
- **Very long owner or repo name**: `title_slug()` applies an overall maximum length (e.g. 100 chars); the unique identifier portion (owner/repo/number for GitHub) is prepended before the title so truncation only ever cuts the title, never the uniqueness-bearing component
- **Title produces the same slug for two different issues**: impossible for GitHub because the number is always included; all providers must include their unique identifier before any title component — this is an explicit invariant of `title_slug()`
- **Non-ASCII characters in title**: `slugify()` treats them as non-alphanumeric and replaces with hyphens; the resulting slug is always pure ASCII
- **`title_slug()` result used as a git branch suffix**: the result contains only lowercase alphanumeric and hyphens, no consecutive hyphens, no leading/trailing hyphens — git-ref-safe by construction; no additional sanitization required in the branch naming path. The composed branch name is `awman/workflow-{title_slug}` (via `branch_name_for_workflow`).
- **Future provider whose issues have non-numeric IDs**: `numeric_id()` returns `None`; `WorkItemContext::number` and the NNNN container filename prefix both fall back to `0`/`"0000"` without any special casing in the command layer; the provider's `title_slug()` encodes uniqueness via its own identifier form
- **Two providers with overlapping `can_handle` patterns**: router returns the first match in registration order; `IssueSourceRouter::default()` documents the priority order explicitly
- **`exec prompt` with neither positional prompt nor `--issue`**: caught in the command handler before any routing or network call; returns a `CommandError` — not a router error, since neither flag has been examined yet
- **`exec prompt --issue` with an empty issue body and no user prompt**: `format_as_markdown` produces just the title line; this single-line prompt is still valid — the command proceeds; no special casing required
- **`exec prompt` with both a user prompt and `--issue`**: concatenation is always `{user_text}\n\n{issue_markdown}`; the user text comes first so the agent sees the user's framing before the raw issue content


## Test Considerations:

- **Unit tests for `IssueSourceRouter::route`**: each registered provider's `can_handle` patterns (bare integer, URL, short form); input claimed by no provider returns `NoMatchingProvider`; ambiguous input resolves to the higher-priority provider
- **Unit tests for `GithubIssueSource::can_handle`**: bare integers, GitHub URLs, `owner/repo#N`, non-GitHub URLs, empty string, malformed input — all expected return values
- **Unit tests for `GithubIssueSource::fetch_issue` input parsing**: each accepted form produces the correct `(owner, repo, number)` triple; non-GitHub URL returns `InvalidRef` with a non-empty hint
- **Unit tests for remote detection in `GithubIssueSource`**: mock `git remote get-url` output; SSH remotes, HTTPS remotes, non-GitHub remotes (`NoRemoteDetected`), no remote configured
- **Unit tests for `GithubIssueSource` fetch logic**: mock `gh` CLI (via PATH-injectable fake binary or command-abstraction seam) and mock `reqwest` responses (200, 404, 401, 403, 429); verify `Issue.source_id` is the canonical GitHub URL; verify every `IssueSourceError` variant's `provider` field equals `provider_name()` — not a hardcoded literal
- **Unit tests for `slugify()`**: standard ASCII text, leading/trailing special chars stripped, consecutive special chars collapsed to one hyphen, non-ASCII treated as non-alphanumeric, truncation at `max_len` with trailing hyphen stripped, empty input returns `""`
- **Unit tests for `GithubIssueSource::title_slug()`**: standard case produces `{owner}-{repo}-{number}-{title_slug}`; empty title omits title component; non-ASCII title; very long title truncated before overall max; very long owner/repo name truncated at overall max without cutting the number component; malformed `source_id` falls back gracefully
- **Unit tests for `Issue::numeric_id()`**: URL ending in integer returns `Some(n)`; URL ending in alphanumeric slug returns `None`; malformed `source_id` returns `None`
- **Unit tests for `IssueSource::format_as_markdown` default impl**: body present, body empty, Unicode, special characters
- **Unit tests for `new spec` filename derivation**: when `--issue` is set, the generated filename is `{NNNN}-{title_slug}.md` and the user is not prompted for a title; verify `title_slug()` output drives the slug portion; combined with `--interview` is valid and prefill content reaches `render_interview_prompt`
- **Unit tests for `exec workflow` flag parsing**: `--issue` populates `issue_source.issue`; `--issue` + `--work-item` returns a mutual-exclusion error before routing; `WorkItemContext` is populated from `Issue`; `numeric_id() = None` produces `number: 0`
- **Integration test for workflow overlay injection**: `OverlaySpec` for the temp file appears in container options when `--issue` is used; temp file is named `awman-issue-{pid}-{title_slug}.md`; container filename is `{NNNN}-{title_slug}.md`; container path is derived correctly from the configured work-items dir
- **Integration test for worktree branch naming**: when `--issue` and `--worktree` are both set, the created branch is `awman/workflow-{title_slug}` (the `title_slug` is fed to the existing `branch_name_for_workflow` helper)
- **Integration test for temp file cleanup**: temp file deleted after workflow completion on both success and error paths
- **Unit tests for `exec prompt` flag parsing**: `--issue` populates `issue_source.issue`; positional `prompt` absent with `--issue` present is valid; both absent returns an error; both present constructs `{user}\n\n{issue}` with user text first
- **Unit tests for `exec prompt` prompt construction**: only user text passes through unchanged; only issue uses `format_as_markdown` output; combined appends with double newline separator; issue with empty body produces title-only string without trailing whitespace
- **End-to-end test (gated)**: guarded by `AWMAN_E2E_ISSUES=1` env var; tests the GitHub provider against a known public issue using both a bare integer and a full URL; verifies the generated work item filename contains the expected `title_slug()`; verifies `exec prompt --issue` passes the expected markdown string as `initial_prompt`


## Codebase Integration:

- `specs.rs` and `exec_workflow.rs` import only from `src/data/issue/mod.rs` (`IssueSource`, `Issue`, `IssueSourceError`, `IssueSourceFlags`) and `src/data/issue/router.rs` (`IssueSourceRouter`); they never import from `src/data/issue/github.rs`
- `GithubIssueSource` is referenced only inside `IssueSourceRouter::default()` in `router.rs`
- Follow the `reqwest` client construction pattern from `src/engine/agent/download.rs` — same `user_agent("awman")`, `connect_timeout`, and `timeout` values; factor a shared `http_client()` constructor into `src/data/network/` if one does not already exist
- `std::process::Command` for `gh` CLI follows the same pattern as git invocations in `exec_workflow.rs` (lines 832–845)
- The workflow overlay builder (`issue_source_overlay`) follows the `worktree_git_overlay()` pattern in `exec_workflow.rs` (lines 1291–1303); its signature takes `&dyn IssueSource` and `&Issue` — no concrete types
- `IssueSourceError` integrates with the existing error enum hierarchy — add a wrapping variant rather than a new top-level error type
- Temp file paths use `std::env::temp_dir()` — never hardcode `/tmp`
- `slugify()` lives in `src/data/issue/mod.rs`; all `title_slug()` implementations call it for the title component so slugification behaviour is consistent across providers and not duplicated; the provider is responsible for prepending its own unique identifier before the slugified title
- For `exec prompt`: `EXEC_PROMPT` in the catalogue must be updated to use its own `EXEC_PROMPT_FLAGS` slice (not `&AGENT_RUN_FLAGS_NO_WORKTREE`) so that `--issue` is scoped to `exec prompt` only; `chat` retains `&AGENT_RUN_FLAGS_NO_WORKTREE` unmodified
- For `exec prompt`: `ExecPromptCommandFlags.prompt` changes from `String` to `Option<String>`; callers that construct this struct (CLI dispatch, API frontend, TUI frontend) must be updated to pass `None` when the positional argument is absent rather than an empty string

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** (e.g., if implementing headless features, update `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0123 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.
