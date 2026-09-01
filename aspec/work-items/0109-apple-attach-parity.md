# Work Item: Feature

Title: Attach parity on Apple Containers (attach rendezvous socket)
Issue: (reported directly — follow-up to WI 0108 fix 1)

## Summary:
- `awman squad attach` (CLI and TUI `a`) must behave identically on the
  docker and apple-containers runtimes. Docker has native `docker attach`;
  Apple's `container` CLI has **no attach subcommand** (verified against
  apple/container's command reference: `run/build/create/start/stop/kill/
  delete/list/exec/logs/...`, with only `start --attach` for *stopped*
  containers), so WI 0108 initially made Apple attach fail with a clear
  error. This work item replaces that error with real parity.

## Implementation Details:

### Mechanism — the launcher is the rendezvous
On Apple, the only holder of a running container's PTY is the awman process
that spawned `container run -it` under portable-pty (the squad daemon for
squad tasks; the TUI/CLI process for interactive sessions). That process now
serves the live PTY over a per-container unix domain socket, and
`AppleBackend::attach` returns a client that connects to it
(`src/engine/container/attach_socket.rs`):

- **Server** (`spawn_attach_socket_server`): wired into
  `spawn_pty_bridged_apple` alongside the PTY bridge. The bridge gained an
  optional `output_broadcast` tap (`BridgeConfig.output_broadcast`) so every
  output chunk the frontend sees is also broadcast to attach clients; client
  stdin merges into the bridge's existing stdin injector; client resizes are
  applied to the real PTY master (via a `Weak` so the server never extends
  the master's lifetime), which SIGWINCHes the agent and triggers the repaint
  that fills a fresh attach screen — the same trigger the docker attach path
  relies on. The listening guard lives on `AppleExecution` and is dropped
  with it: teardown aborts the accept loop and every session (all in one
  `JoinSet`, both socket directions inside one task so aborts close the fd)
  and removes the socket file. A bind failure only logs — it never fails the
  agent launch.
- **Client** (`SocketAttachInstance`): `AgentInstance`-shaped like every
  other attach, so the TUI slot driver, CLI attach, stuck detection
  (`spawn_stuck_detector` over the framed output stream), output tail, and
  cancel semantics all apply unchanged. `cancel` shuts down only the local
  socket. Connecting to a missing/stale socket yields a clear
  "no live attach endpoint" error naming the container.
- **Wire protocol**: symmetric length-prefixed frames
  (`tag u8, len u32 LE, payload`): server→client `OUTPUT`; client→server
  `STDIN` and `RESIZE (cols u16 LE, rows u16 LE)`. 1 MiB frame cap.
- **Socket paths**: `~/.awman/attach/<sha256(name)[..16]>.sock`
  (`AWMAN_ATTACH_DIR` overrides the directory — used by tests). Hashed
  filenames keep paths under the macOS 104-byte `sun_path` limit regardless
  of container-name length; both the serving and attaching process derive
  the path from the container name alone. Directory `0700`, socket `0600` —
  local-user-only, consistent with `aspec/architecture/security.md`.

### Parity properties (matching `docker attach`)
- Attach reaches the actual agent TTY — never a sibling shell (WI 0106 §3c).
- Multiple concurrent clients; stdin merged, output broadcast.
- Detach/cancel ends only the local session; the container keeps running.
- A lagging client drops output chunks rather than backpressuring the agent.

### Known deltas (inherent to the runtime, documented)
- The PTY (and therefore attach) lives only as long as the launching
  process. That was already true of the agent's stdio on Apple before this
  change; attach against a gone launcher reports "no live attach endpoint".
- Output produced before a client connects is not replayed; the initial
  resize-triggered repaint restores the screen (identical to docker attach).
- Stale socket files from a crashed launcher are unlinked on the next bind
  for the same container; files for never-relaunched names linger harmlessly
  in `~/.awman/attach/` until removed.

## Edge Case Considerations:
- A transient `accept()` failure (`ECONNABORTED`, fd exhaustion) never
  permanently disables attach: the accept loop logs, backs off briefly, and
  keeps serving. Only dropping the guard (container teardown) ends it.
- The socket directory is created with mode `0700` directly (no
  create-then-chmod window), and the output tap copies bytes only while at
  least one client is subscribed — an unattached container pays nothing.
- Non-unix builds: stubs keep the tree compiling; the Apple runtime is
  macOS-only in practice.
- Piped/ACP Apple runs get no socket (attach targets PTY agents; squad runs
  everything PTY-backed).
- Docker is untouched — native attach remains the docker path.

## Test Considerations:
- In-process end-to-end over a real unix socket
  (`attach_socket.rs::tests`): output/stdin/resize round trip through the
  real `SocketAttachInstance`, initial-resize-first ordering, second client
  after the first detaches, guard drop ends sessions and removes the socket,
  missing-endpoint error, stale-socket rebind, frame encoding.
- Live verification on macOS with the `container` runtime is still wanted:
  launch a squad task, attach from the TUI and CLI, detach, reattach.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.

## Documentation
- `docs/12-squad.md` — attach section now describes both runtimes and the
  Apple caveat.
