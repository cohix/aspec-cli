# Security & Isolation

awman is built around a simple principle: agents never run on your host machine. Every agent session runs inside an isolated environment — a container or microVM — that only sees what you explicitly give it. This section explains the isolation mechanisms and when to opt into elevated access.

---

## The containment model

By default, an agent environment:

- Sees only your current Git repository (mounted via bind mount or virtiofs)
- Receives your credentials through a secure per-session channel (never exposed to other sessions)
- Has no access to your home directory, SSH keys, or host Docker daemon
- Is stopped or removed when the session ends

This means a misbehaving agent can't access your SSH keys, can't run arbitrary containers on your behalf, and can't touch files outside the project. The worst case is that it makes bad edits inside the repo — which git can undo.

**Docker / Apple Containers:** agents run in a Linux container or lightweight VM respectively. The container is removed (`--rm`) when the session ends. Credentials are injected as environment variables.

**Docker Sandboxes (`docker-sbx-experimental`):** agents run in a dedicated microVM with its own kernel, private Docker daemon, and private filesystem. Host escape requires a hypervisor exploit rather than a container escape. Sandboxes persist between sessions (state survives `sbx stop`); awman runs `sbx rm` only on explicit teardown. Credentials are registered at agent launch with sandbox-scoped `sbx secret set` calls (never global), so removing a sandbox removes its secrets with it. See [Runtimes](12-runtimes.md#docker-sandboxes-experimental) for setup and limitations.

### Transparency

Every time awman runs a container or sandbox command, the full CLI invocation is printed before it executes:

```
$ docker run --rm -it -v /home/user/myproject:/workspace -w /workspace \
    -e CLAUDE_CODE_OAUTH_TOKEN -e GEMINI_API_KEY=*** awman-myproject:latest claude "..."
```

For Docker Sandboxes, every `sbx` invocation is announced the same way:

```
Running: sbx create --kit ~/.awman/kits/claude --name awman-ab12-claude claude /home/user/myproject
Running: sbx secret set awman-ab12-claude anthropic (value piped via stdin)
Running: sbx run awman-ab12-claude
```

Credential values never appear in a printed command. Agent-managed credentials (like the Claude Code OAuth token) show as a bare `-e VAR_NAME` — the value is passed straight from awman's own environment to the container CLI's process, never through argv. Env vars you configure yourself (`env()` overlays, `envPassthrough`) still show as `VAR_NAME=***`. Everything else is visible in full. You can always see exactly what awman is doing.

---

## Worktree isolation

The `--worktree` flag runs agent sessions in an isolated Git worktree rather than your main working directory. The agent's changes land on a separate branch, completely isolated from your current work until you decide what to do with them.

```sh
awman exec workflow path/to/workflow.toml --worktree
```

### Why use it

- The agent can make sweeping changes without your working branch becoming unstable mid-implementation
- You can review the full diff as a coherent unit before it touches your main tree
- If the output isn't useful, discard it with a single keypress — no `git reset` needed
- Works with `--workflow`: all steps in the workflow share the same isolated worktree

### How it works

1. awman creates a branch `awman/work-item-NNNN` from your current `HEAD`
2. A worktree is checked out at `~/.awman/worktrees/<repo-name>/<NNNN>/`
3. The agent container mounts the worktree instead of your repo root
4. After the agent exits, you choose what to do with the branch

### Post-run options

When a worktree run completes (or is aborted), the worktree is preserved on disk and awman asks what to do with it:

| Key | Action |
|-----|--------|
| `m` | Merge into the current branch (opens the merge-mode prompt below) |
| `d` | Discard — remove worktree and delete branch |
| `k` | Keep worktree and branch for manual review; prints the path |

If you abort the workflow (Ctrl+C or the **Abort** action in the workflow control board), awman shows the same merge/discard dialog. The worktree is never automatically deleted on abort — your completed steps' changes are preserved and ready for review.

### Merge modes

Choosing **Merge** asks how the branch should be integrated:

| Key | Mode | Effect |
|-----|------|--------|
| `m` | Merge (no squash) | Plain `git merge` — the branch's individual commits are preserved (fast-forwards when possible) |
| `s` | Squash | `git merge --squash` followed by a single commit `Implement <branch>` |
| `l` | Leave branch alone | No merge; the worktree and branch are kept as-is |

Whichever mode you pick — including *Leave branch alone* — awman then checks for uncommitted files in the worktree and offers to commit them, so nothing on the branch is left dangling. After a successful merge (squash or not), awman offers to clean up the worktree and delete the branch.

The same dialogs appear whether the workflow completed successfully or was aborted, and in both CLI and TUI mode. Your partially completed work is preserved, allowing you to review, manually continue, or discard as you choose.

### Setup steps in worktree runs

If the workflow defines a `checkout_create_branch` setup step, it is **skipped with a warning** when running in a worktree — the run is already isolated on its own branch, so creating another branch inside the worktree is redundant. This is not an error; the remaining setup steps run normally.

### Interrupted runs

If a worktree already exists (previous run was interrupted), awman detects it:

```
Worktree already exists at ~/.awman/worktrees/myrepo/0030.
[r]esume — reuse existing worktree
[R]ecreate — remove it and start fresh
```

### Merge conflicts

If the merge fails, awman prints a recovery message and leaves the worktree in place:

```
Merge failed with conflicts — resolve manually in /path/to/repo,
then run: git branch -d awman/work-item-0030 && git worktree remove ~/.awman/worktrees/myrepo/0030
```

### Commit signing (GPG, SSH, S/MIME)

When Git commit signing is enabled, awman **suspends the TUI** around each `git commit` it runs, allowing your passphrase prompt to work normally. After the commit completes (or fails), the TUI is restored. Users without signing configured see no change.

### Edge cases

| Situation | Behaviour |
|-----------|-----------|
| `git` < 2.5 | Error before launch: "git ≥ 2.5 is required for --worktree support" |
| Detached HEAD | Warning printed; worktree created from current commit; continues |
| Branch exists, no worktree dir | Worktree created using the existing branch |
| Merge conflict | Error with manual resolution instructions; worktree kept |
| Combined with `--workflow` | All workflow-step containers share the same worktree |
| Combined with `--overlay ssh()` | Both flags apply independently |

### Examples

```sh
awman exec workflow path/to/workflow.toml --worktree                              # isolated run; prompt to merge after
awman exec workflow path/to/workflow.toml --worktree --overlay "ssh()"            # worktree + SSH keys in container
```

---

## Overlay mounts

The `--overlay` flag mounts additional host resources into the agent container beyond the default Git repository mount. Supported overlay types:

- `skill(*)` / `skill(name)` — mount all hand-authored global skills or selected skills and pulled libraries
- `dir(host_path:container_path[:ro|rw])` — mount a host directory

This lets you give an agent access to a personal skills library, a reference dataset, a shared prompts directory, or any other host resource without permanently modifying any config file.

### Directory overlay format

```
dir(host_path:container_path[:ro|rw])
```

| Field | Description |
|-------|-------------|
| `host_path` | Absolute path on the host. Leading `~` is expanded to your home directory. |
| `container_path` | Absolute path inside the container where the directory will appear. |
| `ro` / `rw` | Mount permission. Defaults to `ro` when omitted. |

### Skills overlay

```
skill(*)
```

Mounts hand-authored skills from `~/.awman/skills/` read-only into the agent's native skills directory (determined by agent type). Pulled libraries under `~/.awman/skills/.library/` are not included by `skill(*)`; reference a whole library or one of its skills explicitly with `skill(library)` or `skill(library/skill)`.

### Basic examples

```sh
# Mount your personal skills library
awman exec workflow path/to/workflow.toml --overlay "skill(*)"

# Mount a reference dataset read-only
awman exec workflow path/to/workflow.toml --overlay "dir(/data/reference:/mnt/reference:ro)"

# Mount a shared prompts directory read-write
awman chat --overlay "dir(~/prompts:/mnt/prompts:rw)"

# Skills + directories (repeated flag or comma-separated — both are equivalent)
awman exec workflow path/to/workflow.toml --overlay "skill(*)" --overlay "dir(/data/ref:/mnt/ref:ro)" --overlay "dir(~/snippets:/mnt/snippets)"
awman exec workflow path/to/workflow.toml --overlay "skill(*),dir(/data/ref:/mnt/ref:ro),dir(~/snippets:/mnt/snippets)"
```

Available on all agent-launching commands: `chat`, `exec prompt`, and `exec workflow`.

### `AWMAN_OVERLAYS` environment variable

Set `AWMAN_OVERLAYS` in your shell profile to apply overlays automatically to every agent session regardless of which repo you're working in. It uses the same format as `--overlay` — a comma-separated list of typed overlay expressions:

```sh
export AWMAN_OVERLAYS="skill(*),dir(~/personal-prompts:/mnt/prompts),dir(/data/shared-fixtures:/mnt/fixtures:ro)"
```

### Config-based overlays

Overlays can be declared in config files so they are applied automatically without requiring any flags each time. The `overlays` field is a flat array of overlay expression strings — the same syntax as `--overlay`:

**Per-repo config** (`.awman/config.json`):
```json
{
  "overlays": [
    "skill(*)",
    "dir(/data/fixtures:/mnt/fixtures:ro)",
    "dir(~/shared-prompts:/mnt/prompts)"
  ]
}
```

**Global config** (`~/.awman/config.json`):
```json
{
  "overlays": [
    "skill(*)",
    "dir(~/personal-prompts:/mnt/prompts:ro)"
  ]
}
```

### Priority and conflict resolution

Overlays are **additive**: all four sources contribute entries, then conflicts are resolved.

**Priority order**, from lowest to highest:

1. Global config (`~/.awman/config.json`)
2. Per-repo config (`.awman/config.json`)
3. `AWMAN_OVERLAYS` environment variable
4. `--overlay` CLI flags (highest priority)

Overlay sources are merged — entries from all four sources appear in the final mount list unless they conflict. When two sources specify the same container path, the higher-priority source wins.

**Skills overlays are always read-only** and cannot be modified by the agent.

### Missing host paths

If a configured host path does not exist when the container launches, awman logs a warning and skips that overlay — it does not abort the session. This matches the behaviour of other optional mounts (SSH keys, Docker socket).

```
WARN overlay host path '/data/reference' does not exist; skipping
```

### Security note

Overlay mounts extend the base isolation model: the agent still cannot access anything outside your Git repo **plus the explicitly listed overlay directories and skills**. Directory `:ro` mounts prevent the agent from modifying the overlaid directory. Only use `:rw` when the task genuinely requires the agent to write to that directory, and only with agent images you trust.

Skills overlays are always mounted read-only, whether skills are provided by global config, per-repo config, environment variable, or CLI flag. The agent cannot modify any skill files.

Like `--overlay ssh()` and `--allow-docker`, all overlay mounts are printed in the Docker command before execution so you can see exactly what is mounted.

### TUI usage

In the TUI command box, use comma-separated syntax when specifying multiple overlays — the TUI flag parser stores one value per flag, so repeating `--overlay` keeps only the last value:

```
# Correct: comma-separated in one value
exec workflow path/to/workflow.toml --overlay "skill(*),dir(/data/ref:/mnt/ref:ro),dir(~/prompts:/mnt/prompts)"

# Incorrect in TUI (second value silently overwrites first):
exec workflow path/to/workflow.toml --overlay "skill(*)" --overlay "dir(/data/ref:/mnt/ref:ro)"
```

On the CLI, both repeated flags and comma-separated syntax are equivalent.

---

## Docker socket access

The `--allow-docker` flag mounts the host Docker daemon socket into the agent container. This lets the agent build and run Docker containers itself.

### When to use it

Use `--allow-docker` when the task requires the agent to:

- Build Docker images (e.g. testing your app's Dockerfile)
- Run Docker containers (e.g. starting a local database for testing)
- Interact with the Docker daemon in any other way

### What happens

Before launching the container, awman verifies the socket exists and prints a warning:

```
Docker socket: /var/run/docker.sock (found)
WARNING: --allow-docker: mounting host Docker socket into container
(/var/run/docker.sock:/var/run/docker.sock). This grants the agent elevated host access.
```

| Platform | Mount |
|----------|-------|
| Linux / macOS | `-v /var/run/docker.sock:/var/run/docker.sock` |
| Windows | `--mount type=npipe,source=\\.\pipe\docker_engine,target=\\.\pipe\docker_engine` |

### Security note

Mounting the Docker socket gives the agent root-equivalent access to your host — it can start containers, delete images, and interact with any running container. Only use `--allow-docker` for tasks that genuinely require it and when you trust the agent and work item. awman will never mount the socket without this explicit flag.

### Examples

```sh
awman exec workflow path/to/workflow.toml --allow-docker  # workflow that needs to build a Docker image
awman chat --allow-docker                               # freeform session with Docker access
awman ready --refresh --allow-docker                    # Dockerfile audit with Docker access
```

---

## SSH key access

Use the `ssh()` overlay to mount your host `~/.ssh` directory read-only into the container, so the agent can authenticate with remote Git servers using your existing SSH keys.

### When to use it

Use `--overlay ssh()` when the task requires the agent to:

- Clone private repositories over SSH
- Push branches or tags to a remote
- Run `git fetch` / `git pull` against SSH remotes

### What happens

Before launching the container, awman verifies `~/.ssh` exists and prints a warning:

```
WARNING: overlay ssh(): mounting host ~/.ssh into container (read-only). Ensure you trust the agent image.
```

The directory is mounted as `-v /home/user/.ssh:/root/.ssh:ro`. The `:ro` flag prevents the agent from modifying your host SSH keys.

`~/.ssh` is never mounted without an explicit `ssh()` overlay — there is no config option to enable it silently.

### Security notes

- The mount is read-only; the agent can use your keys but cannot modify them
- SSH key permissions must be correct on the host (`600` for private keys); Docker bind mounts inherit host permissions
- Only use the `ssh()` overlay with agent images you trust

### Examples

```sh
awman exec workflow path/to/workflow.toml --overlay "ssh()"              # agent can push/pull over SSH
awman chat --overlay "ssh()"                                             # freeform session with SSH access
awman exec workflow path/to/workflow.toml --worktree --overlay "ssh()"   # combine with worktree isolation
```

When used with a workflow, the SSH directory is mounted into every workflow-step container.

---

## Command transparency

Every command awman issues to the underlying runtime is printed before it executes — in command mode to stdout, in TUI mode as the first line of the execution window.

```
$ docker build -t awman-myapp:latest -f Dockerfile.dev /path/to/repo
$ docker run --rm -it \
    -v /path/to/repo:/workspace \
    -w /workspace \
    -e CLAUDE_CODE_OAUTH_TOKEN \
    awman-myapp:latest claude "Implement work item 0001..."
```

With the Apple Containers runtime, the same commands are shown with `container` instead of `docker`. With the Docker Sandboxes runtime, every `sbx` invocation is announced — `sbx run`, `sbx exec`, `sbx stop`, `sbx rm`, `sbx secret set`, `sbx kit validate` — without sensitive values. Credential values are never printed: agent-managed credentials render as a bare `-e VAR_NAME`, and user-configured environment overlays render as `VAR_NAME=***`.

---

[← Agent Sessions](03-agent-sessions.md) · [Next: Workflows →](05-workflows.md)
