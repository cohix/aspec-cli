# GitHub Integration

GitHub integration lets you invoke awman commands directly against GitHub issues, without first creating local work item files. This is useful for:

- **Bootstrapping specs from issues**: `new spec --issue 84` fetches a GitHub issue and generates a structured work item spec from it
- **Running workflows against issues**: `exec workflow my-workflow --issue 84` uses the issue as the work item input for template variable substitution
- **Prompting the agent with issues**: `exec prompt --issue 84` sends the issue title and description directly as a prompt to the agent
- **Combining your input with issues**: `exec prompt "additional context" --issue 84` lets you frame the issue with your own context before the agent runs

---

## Quick start

### Create a spec from a GitHub issue

```sh
# Using a bare issue number (requires git remote to be GitHub)
awman new spec --issue 84

# Using a full GitHub URL
awman new spec --issue https://github.com/prettysmartdev/awman/issues/84

# Using owner/repo shorthand
awman new spec --issue prettysmartdev/awman#84
```

All three forms fetch the same GitHub issue and launch an agent to generate a structured work item spec. The generated file is placed in your configured work items directory with a sequential number and a slug derived from the issue title.

### Run a workflow against a GitHub issue

```sh
# Use the issue as the work item input
awman exec workflow my-workflow --issue 84

# Full URL form
awman exec workflow my-workflow --issue https://github.com/prettysmartdev/awman/issues/84
```

When using `--issue`, you do not need to pass `--work-item` separately. The issue is fetched and treated as the work item input — all template variables like `{{work_item_content}}`, `{{work_item_number}}`, and `{{work_item_section:[Name]}}` are populated from the issue.

### Send a GitHub issue to the agent as a prompt

```sh
# Just the issue
awman exec prompt --issue 84

# Issue with additional framing
awman exec prompt "Review this and suggest improvements" --issue 84
```

The first form sends the issue title and description directly to the agent. The second form prepends your context before the issue content, so the agent sees your framing first.

---

## Input formats

The `--issue` flag accepts three input forms. awman automatically detects which one you've provided and routes to the correct provider (currently GitHub only).

### Bare issue number

```sh
awman new spec --issue 84
```

A bare integer is treated as a GitHub issue number in the current repo. awman runs `git remote get-url origin` to extract the owner and repo name from the GitHub remote URL, then constructs the full issue reference as `owner/repo#84`.

If the repo has no GitHub remote (e.g., `origin` points to a non-GitHub URL, or no `origin` remote is configured), awman exits with a clear error message:

```
error: GitHub integration: could not detect GitHub remote
  hint: ensure your 'origin' remote points to github.com (https or ssh)
```

### Owner/repo shorthand

```sh
awman new spec --issue prettysmartdev/awman#84
```

The `owner/repo#N` form explicitly names the GitHub repository and issue number. awman parses `owner`, `repo`, and issue number `N` separately and fetches the issue without accessing the local git remote.

### Full GitHub URL

```sh
awman new spec --issue https://github.com/prettysmartdev/awman/issues/84

# SSH URLs are not currently supported; use https
```

awman parses the path segments from the URL to extract the owner, repo, and issue number.

---

## How issues are fetched

awman uses three strategies, in order of preference:

1. **GitHub CLI (`gh`)**: if `gh` is installed and authenticated, awman runs `gh issue view` for the fastest fetch with the fewest API calls.
2. **GitHub REST API with `GITHUB_TOKEN`**: if the `gh` CLI is absent or not authenticated, awman falls back to the GitHub REST API. If `GITHUB_TOKEN` is set in your environment, the request is authenticated and includes private repos.
3. **Unauthenticated REST API**: if neither `gh` is authenticated nor `GITHUB_TOKEN` is set, awman makes an unauthenticated REST API request. This works for public repos and has a lower rate limit (60 requests/hour).

You don't need to do anything — awman automatically chooses the best available method.

### Setting up GitHub authentication

**Option 1: GitHub CLI (recommended)**

```sh
# Authenticate once
gh auth login

# awman uses your existing session automatically
awman new spec --issue 84
```

The GitHub CLI handles OAuth token management for you. Once authenticated, awman reuses your session.

**Option 2: GitHub token in environment**

```sh
export GITHUB_TOKEN="ghp_..."
awman new spec --issue 84
```

Set `GITHUB_TOKEN` to a personal access token (PAT) or app token. awman reads it from your environment and uses it for API requests. The token is never logged or displayed in command output.

To create a token, visit [github.com/settings/tokens](https://github.com/settings/tokens) and create a "Fine-grained personal access token" with read access to the `issues` scope.

---

## Command behavior

### `new spec --issue <ref>`

Creates a numbered work item file in your configured work items directory, using the issue content as the input for spec generation.

The generated filename follows the pattern `{NNNN}-{title-slug}.md`, where:
- `NNNN` is the next sequential number in your work items directory
- `{title-slug}` is a slugified version of the issue title (lowercase, hyphens instead of spaces)

Example:
```
Issue: #84 "GitHub Integration Part 1"
Generated file: 0084-github-integration-part-1.md
```

#### With `--interview` flag

```sh
awman new spec --issue 84 --interview
```

When both `--issue` and `--interview` are passed, the GitHub issue content is pre-populated in the interview text box. You can review and edit the content before the agent runs — useful if you want to clean up the issue description or add context before spec generation.

#### Without `--interview` flag

```sh
awman new spec --issue 84
```

When only `--issue` is passed (no `--interview`), the issue content is sent directly to the spec agent, treating it as if you had run `new spec --interview` and submitted the issue description immediately without editing.

### `exec workflow <path> --issue <ref>`

Runs a workflow and uses the GitHub issue as the work item input. The issue is fetched and treated as if you had passed `--work-item 0084` with a local file — all template variable substitutions work identically.

```sh
awman exec workflow my-workflow --issue 84
```

The issue content is written to a temporary file and mounted inside the agent container at the configured work items directory. After the workflow completes, the temporary file is automatically cleaned up.

#### Worktree naming

When both `--issue` and `--worktree` are passed, the created git branch is named `awman/{issue-slug}`, where `{issue-slug}` is derived from the issue repository and number:

```sh
awman exec workflow my-workflow --issue 84 --worktree

# Creates branch: awman/prettysmartdev-awman-84-github-integration-part-1
```

If a branch with that name already exists, awman prompts you for a different name or offers to reuse the existing one.

#### Exclusive with `--work-item`

The `--issue` and `--work-item` flags are mutually exclusive:

```sh
# Error: cannot specify both --issue and --work-item
awman exec workflow my-workflow --issue 84 --work-item 0027
```

If you need the workflow to use a local work item file, use `--work-item` alone. If you want to use a GitHub issue, use `--issue` alone.

### `exec prompt --issue <ref>`

Sends the GitHub issue title and description directly to the agent as the prompt.

```sh
# Send just the issue
awman exec prompt --issue 84

# Append the issue to your own prompt
awman exec prompt "Please review this for security issues" --issue 84
```

When only `--issue` is passed, the positional `prompt` argument is optional:

```sh
awman exec prompt --issue 84          # Valid: no positional argument needed
awman exec prompt --issue 84 "text"   # Error: text appears where flags should be
```

When both a positional prompt and `--issue` are passed, your text appears first, followed by the issue content:

```sh
awman exec prompt "context" --issue 84

# The agent receives:
# context
#
# # GitHub Integration Part 1
#
# [full issue body here]
```

No files are created or cleaned up — the issue content is inlined into the prompt and the container is launched immediately.

---

## Error handling

### Issue not found

```
error: GitHub integration: issue not found
  remote: https://github.com/prettysmartdev/awman/issues/84
  hint: check the issue number and repo name; ensure you have access to private repos (authenticate with 'gh auth login' or set GITHUB_TOKEN)
```

Common causes:
- The issue number is wrong
- The repository name is wrong
- The issue is in a private repo and you're not authenticated

### Authentication required

```
error: GitHub integration: authentication required
  remote: https://github.com/owner/private-repo/issues/84
  hint: authenticate with 'gh auth login' or set GITHUB_TOKEN
```

You're trying to fetch a private issue without authentication. Log in with the GitHub CLI or set your `GITHUB_TOKEN` environment variable.

### No GitHub remote detected

```
error: GitHub integration: could not detect GitHub remote
  hint: ensure your 'origin' remote points to github.com (https or ssh)
```

You used a bare issue number (e.g., `--issue 84`), but the current repo's `origin` remote doesn't point to GitHub. Either:
- Use the full `owner/repo#N` or URL form instead
- Add a GitHub remote: `git remote add origin https://github.com/owner/repo.git`

### Rate limiting

```
error: GitHub integration: rate limited
  remote: GitHub
  hint: wait a few minutes and try again; authenticate with 'gh auth login' or set GITHUB_TOKEN for higher limits
```

You've exceeded the unauthenticated API rate limit (60 requests/hour). Authenticate with `gh auth login` or set `GITHUB_TOKEN` to get a higher limit (5000 requests/hour).

### Network unavailable

```
error: GitHub integration: network error
  detail: failed to connect to api.github.com
```

Check your internet connection and try again.

---

## Examples

### Example 1: Spec from issue with interview

```sh
cd my-project

# Fetch issue #42 and start the spec interview with the content pre-filled
awman new spec --issue 42 --interview

# Review the issue description in the text box, edit if needed, press Ctrl-Enter
# Agent generates the spec and writes to aspec/work-items/0023-my-feature.md
```

### Example 2: Workflow from issue

```sh
# Run a planning workflow against a GitHub issue
awman exec workflow aspec/workflows/plan.toml --issue 42

# The workflow receives the issue as {{work_item_content}} and {{work_item_number}}
# After the workflow completes, the temporary file is cleaned up automatically
```

### Example 3: Prompt with issue context

```sh
# Send an issue to the agent with your own framing
awman exec prompt "Security review: check for SQL injection, XSS, and auth bypass" --issue 42

# Agent receives:
# Security review: check for SQL injection, XSS, and auth bypass
#
# # Feature: Better error messages
#
# [issue body...]
```

### Example 4: Full URL form

```sh
# Use the full GitHub URL directly
awman new spec --issue https://github.com/prettysmartdev/awman/issues/84

# Or the shorthand
awman new spec --issue prettysmartdev/awman#84

# Both are equivalent to the bare number if you're in the awman repo
awman new spec --issue 84
```

### Example 5: Private repo with authentication

```sh
# Set your token
export GITHUB_TOKEN="ghp_..."

# Fetch from a private repo
awman exec prompt --issue owner/private-repo#99

# Or use gh auth
gh auth login
awman exec prompt --issue owner/private-repo#99
```

---

[← API & Remote Mode](09-api-and-remote-mode.md) · [Next: Runtimes →](11-runtimes.md)

