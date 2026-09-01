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

**Docker / Apple Containers:** agents run in a Linux container or lightweight VM respectively. The container is removed (`--rm`) when the session ends. Most agents receive credentials as environment variables. Claude Code is the exception: it receives a live-updated credential file (access token, expiry, and scopes only — never the refresh token) written into its staged settings directory and kept fresh for as long as the session runs. See [Live credential refresh](#live-credential-refresh) below.

**Docker Sandboxes (`docker-sbx-experimental`):** agents run in a dedicated microVM with its own kernel, private Docker daemon, and private filesystem. Host escape requires a hypervisor exploit rather than a container escape. Sandboxes persist between sessions (state survives `sbx stop`); awman runs `sbx rm` only on explicit teardown. Credentials are registered at agent launch with sandbox-scoped `sbx secret set` calls (never global), so removing a sandbox removes its secrets with it. sbx is unaffected by the live credential-file refresh described below — it continues to authenticate Claude only through `ANTHROPIC_API_KEY` or an in-sandbox login. See [Runtimes](11-runtimes.md#docker-sandboxes-experimental) for setup and limitations.

### Live credential refresh

Claude Code's OAuth refresh token — the credential that could mint new access tokens indefinitely — never leaves your host and is never given to a container. Instead, for each session awman writes a sanitized, awman-authored `.credentials.json` (access token, expiry, and scopes only, mode `0600`) into the staged `~/.claude` directory that's bind-mounted into the container; the host's own `~/.claude/.credentials.json` is excluded from that mount entirely.

While a Claude session is running, awman's credential-refresh monitor watches the access token's expiry. Shortly before it would expire, the monitor triggers a host-side refresh: it invokes the same sanctioned, hardcoded ready-check ping that `awman ready` uses to verify your local agent is authenticated (fixed prompt, no user input, no repository content, no working directory) — this causes your host's Claude Code installation to rotate its own Keychain entry. The monitor then atomically replaces the staged credential file for every live session with the new token, so a long-running container observes it on its next request without a restart.

If the host can't refresh (asleep, logged out, offline), awman keeps the last-known-good token in place, warns loudly, and retries with backoff; it never leaves a container with no credential at all. This behavior is on by default and can be tuned or disabled — see [Control credential refresh](07-configuration.md#control-credential-refresh-authrefresh).

### Transparency

Every time awman runs a container or sandbox command, the full CLI invocation is printed before it executes:

```
$ docker run --rm -it -v /home/user/myproject:/workspace -w /workspace \
    -v /tmp/awman-claude-dir-a1b2c3/.claude:/root/.claude -e GEMINI_API_KEY=*** awman-myproject:latest claude "..."
```

For Docker Sandboxes, every `sbx` invocation is announced the same way:

```
Running: sbx create --kit ~/.awman/kits/claude --name awman-ab12-claude claude /home/user/myproject
Running: sbx secret set awman-ab12-claude anthropic (value piped via stdin)
Running: sbx run awman-ab12-claude
```

Credential values never appear in a printed command. Env-delivered agent credentials show as a bare `-e VAR_NAME` — the value is passed straight from awman's own environment to the container CLI's process, never through argv. Claude Code's OAuth credential is instead delivered as a file (see [Live credential refresh](#live-credential-refresh)), so the printed command shows only the staged directory's mount path, never a token. Env vars you configure yourself (`env()` overlays, `envPassthrough`) still show as `VAR_NAME=***`. Everything else is visible in full. You can always see exactly what awman is doing.

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
- Works with `exec workflow`: all steps in the workflow share the same isolated worktree

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
| Combined with `exec workflow` | All workflow-step containers share the same worktree |
| Combined with `--overlay ssh()` | Both flags apply independently |

### Examples

```sh
awman exec workflow path/to/workflow.toml --worktree                              # isolated run; prompt to merge after
awman exec workflow path/to/workflow.toml --worktree --overlay "ssh()"            # worktree + SSH keys in container
```

---

## Overlay mounts

Overlays extend the base isolation model: they are the *only* supported way to give a container anything beyond the project mount. There are five kinds — `dir()`, `env()`, `skill()`, `ssh()`, and `context()` — and they can come from global config, per-repo config, `AWMAN_OVERLAYS`, `--overlay` flags, or an individual workflow step. [Overlays](08-overlays.md) is the full reference for the syntax, the five sources, and the merge and conflict rules; this section covers only what they mean for security.

**What an overlay can and cannot reach.** With overlays in play, the agent can access your Git repo **plus exactly the listed overlay directories, variables, and skills** — nothing more. There is no wildcard that exposes the host filesystem, and no overlay type that grants a shell on the host.

**Read-only by default.** `dir()` mounts default to `:ro` when no permission is given. Only use `:rw` when the agent genuinely has to write there, and only with agent images you trust. Skills overlays are **always** mounted read-only regardless of source, so an agent can never modify a skill file.

**Missing paths never fail open.** If a configured host path does not exist at launch, awman logs a warning and skips that overlay rather than aborting the session or substituting something else:

```
WARN overlay host path '/data/reference' does not exist; skipping
```

**Everything is printed.** Like `ssh()` and `--allow-docker`, every overlay mount appears in the runtime command awman prints before it executes, so you can always see exactly what a container was given. Values of `env()` overlays are masked as `***`.

Two overlay types have host-access implications significant enough to document on their own: [SSH key access](#ssh-key-access) below, and [context overlays](08-overlays.md#context-overlays-in-depth), which mount a writable directory shared across sessions.

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
    -v /tmp/awman-claude-dir-a1b2c3/.claude:/root/.claude \
    awman-myapp:latest claude "Implement work item 0001..."
```

With the Apple Containers runtime, the same commands are shown with `container` instead of `docker`. With the Docker Sandboxes runtime, every `sbx` invocation is announced — `sbx run`, `sbx exec`, `sbx stop`, `sbx rm`, `sbx secret set`, `sbx kit validate` — without sensitive values. Credential values are never printed: env-delivered agent credentials render as a bare `-e VAR_NAME`, Claude's file-delivered OAuth credential shows only as the staged directory's ordinary mount path, and user-configured environment overlays render as `VAR_NAME=***`.

---

[← Agent Sessions](03-agent-sessions.md) · [Next: Workflows →](05-workflows.md)
