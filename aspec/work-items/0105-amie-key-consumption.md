# Work Item: Bug

Title: amie — make the daemon's API token consumable from the CLI and TUI
Issue: issuelink

## Summary:
- The amie daemon has minted a bearer key on its first start since WI 0101, but nothing told the user how to spend it. `awman amie start` printed the plaintext as a bare line ("amie API key (store it; it will not be shown again): …"), and the only consumer — `AmieSupervisor::provision_key` — read it back from an **undocumented** `AWMAN_AMIE_KEY` variable that appeared nowhere in `docs/`, nowhere in `aspec/`, and not even in `EnvSnapshot`'s list of known variables. The key was, in practice, unspendable.
- Worse, the auto-start path minted keys **silently**. `awman amie list` on a clean machine started the daemon, generated a key, wrote `amie_key.hash`, used the key in-process, and dropped the plaintext on exit. Every later process then found a hash it had no key for and was refused with `401 Unauthorized`, with `awman amie start --refresh-key` the only way out.
- `--dangerously-skip-auth` already existed on `awman amie start` and already worked daemon-side, but it was invisible to clients: with auth disabled the daemon checks no token, yet `provision_key` would still mint a key and persist a hash — so a later plain `awman amie start` demanded a key the user had never seen. The flag also emitted no warning.
- This work item closes all three: the key is disclosed **with the export snippet that makes it usable**, the variable is a first-class documented input, and `--dangerously-skip-auth` is a genuine "no key at all" mode that clients can detect.

## User Stories

### User Story 1:
As a: user

I want to:
be handed a copy-pasteable `export AWMAN_AMIE_KEY=…` line for my shell the first time the amie daemon starts

So I can:
authenticate the CLI and the TUI to amie from every later shell, instead of holding a secret with no documented way to present it

### User Story 2:
As a: user on a single-user machine

I want to:
start amie with `--dangerously-skip-auth` and have every client agree that no key is in play

So I can:
skip key management entirely without leaving a stranded `amie_key.hash` that locks me out of the next ordinary start

## Implementation Details:

### 1 — `AWMAN_AMIE_KEY` becomes a known variable (Layer 0)

`src/data/config/env.rs`:

- Add `pub const AWMAN_AMIE_KEY: &str = "AWMAN_AMIE_KEY";` and `EnvSnapshot::amie_key() -> Option<&str>`, filtering empty to `None` (an empty bearer token produces a confusing 401 rather than the ordinary no-key path).
- Add `pub const SHELL: &str = "SHELL";` and `EnvSnapshot::shell()`. Read **only** to choose which startup file the snippet names; nothing executes it.
- Both are added to `Env::from_process`'s key list. The snapshot reads a fixed list, so an omission there silently disables the variable — this is the reason the original `std::env::var("AWMAN_AMIE_KEY")` call in `provision_key` was invisible to the rest of the codebase.

### 2 — The snippet (Layer 2, `src/command/commands/amie/key_setup.rs`)

New module, pure rendering, no I/O:

```rust
pub enum ShellFlavor { Zsh, Bash, Fish, Unknown }
impl ShellFlavor {
    pub fn from_shell_path(shell: Option<&str>) -> Self;  // matches the trailing path component
    pub fn from_env(env: &EnvSnapshot) -> Self;
}
pub fn render_key_setup(key: &str, shell: ShellFlavor) -> String;
```

The output is the boxed key banner (sized to the wider of title and key, so any key length stays aligned) followed by the startup file, the export statement, and the `--dangerously-skip-auth` alternative. fish gets `set -gx AWMAN_AMIE_KEY <key>` for `~/.config/fish/config.fish`; everything else gets a POSIX `export`. An unknown or unset `$SHELL` still yields a correct POSIX export and names candidate files as examples rather than asserting one.

Kept a pure function of `(key, shell)` so the wording is unit-testable and the caller decides where the text goes — stdout/stderr for the CLI, a modal for the TUI.

### 3 — Disclosure at every mint site (Layer 2/3)

`run_start` (`src/command/commands/amie/daemon.rs`) replaces the bare key line with `key_setup::render_key_setup(...)`. Disclosure stays in the **foreground** process that owns a terminal — never in the detached `--background` child, whose stdout is redirected to `~/.awman/amie/awman.log`, a file `awman amie logs` prints verbatim. This constraint predates this work item and is preserved unchanged.

`AmieSupervisor` gains:

```rust
/// Some() only on the run that minted the key, and only once.
pub fn take_generated_key_setup(&self) -> Option<String>;
```

backed by a `key_disclosed: AtomicBool`. The key itself is **not** taken — the supervisor still needs it to authenticate for the rest of the process; only the *disclosure* is one-shot, so two frontends sharing a supervisor cannot both print the same secret.

Callers:

- `frontend/cli/mod.rs` (`amie add|list|show|remove|pause|resume`) and `frontend/cli/per_command/amie.rs` (bare `awman amie -n`) print it to **stderr** — stdout belongs to `--json` consumers.
- `App::build_amie_tab` returns a new `AmieTabBuild { tab, gateway, key_setup }` instead of a bare tuple; both call sites (`App::open_or_focus_amie_tab`, `tui::run`'s `InitialTab::Amie`) raise it as `Dialog::Notice`. A status-bar line would scroll away before the user could copy a 64-character secret.

`Dialog::Notice { title, body }` is new: a one-shot informational modal with **no command thread behind it**, so Enter and Esc simply clear it. `Dialog::Custom` was not reusable here — its router arm calls `send_dialog_response`, which assumes a waiting command thread.

### 4 — `--dangerously-skip-auth` becomes visible to clients (Layer 0/2/3)

`ServerMeta` (`src/data/fs/daemon_process.rs`) gains `#[serde(default)] pub auth_disabled: bool`. `#[serde(default)]` is load-bearing: a sidecar written by an older build has no such field and must read as `false` — "assume auth is required" is the safe direction. Both daemons publish it (`frontend/amie/mod.rs` from `AmieServeConfig`, `api_server.rs` from its flags).

`AmieSupervisor::provision_key` gains a `daemon_auth_disabled()` check, ordered **before** the mint:

1. `env.amie_key()` — the documented input.
2. The key this process already minted.
3. A running daemon whose sidecar says `auth_disabled` → `None`. No mint, no hash.
4. A hash exists but this process has no key → `None` (the daemon refuses with the standard auth error).
5. Otherwise mint, persist the hash, and remember the plaintext for disclosure.

`run_start` also emits a `MessageLevel::Warning` naming the flag, stating that any local process can drive amie, and stating why it is nonetheless acceptable — the daemon binds `127.0.0.1` exclusively and has no flag to bind elsewhere.

### 5 — Incidental correction

`AmieOutcome::Started { refreshed_key: true, .. }` rendered as "amie daemon started on port 0", but `--refresh-key` returns *before* anything starts. Left alone it would directly contradict the snippet printed a line above it. It now renders "amie API key regenerated. Start the daemon with `awman amie start`."

## Edge Case Considerations:
- **Empty `AWMAN_AMIE_KEY`.** Treated as unset. Sending an empty bearer token would yield a 401 that reads like a wrong key rather than a missing one.
- **Legacy `server.json`.** No `auth_disabled` field → parses as `false` → clients assume auth is required. Failing the other way would silently stop provisioning keys against a daemon that wants one.
- **Skip-auth daemon plus an existing hash.** The flag is per-run and never deletes the hash, so an ordinary start still finds it. Clients read the sidecar, not the hash, so they send no token for as long as that daemon runs.
- **`--refresh-key` while a daemon is running.** Unchanged: `run_start` refuses on a live PID before touching the key.
- **`--background` first start.** The parent mints and prints; the child finds the hash already on disk and never emits a key to the log.
- **Unknown or unset `$SHELL`.** Snippet stays correct: a POSIX `export`, with startup files named as examples.
- **Very long keys.** The banner sizes to the wider of title and key, so the box never truncates or misaligns.
- **Disclosure races.** `key_disclosed` is an `AtomicBool` swap; the second caller gets `None`.

## Test Considerations:
- `tests/amie_auth_key.rs` (new target in `Cargo.toml` — amie test files need an explicit `[[test]]` entry or `cargo test` never builds them):
  - First start emits `export AWMAN_AMIE_KEY=`, names `~/.zshrc`, persists the hash, and the exported value is the real key — asserted by confirming the plaintext is *not* what landed in the hash file.
  - `--dangerously-skip-auth` writes no hash, discloses no key, and warns with both the flag name and `127.0.0.1`.
  - `--refresh-key` re-emits the snippet.
  - The variable the snippet exports is the one `EnvSnapshot::amie_key` reads — the exact drift this work item repairs.
  - An empty `AWMAN_AMIE_KEY` reads as unset; `Env::from_process` really captures the variable.
  - A legacy sidecar reads as auth-required; a skip-auth sidecar round-trips.
- Colocated unit tests in `key_setup.rs` cover shell classification, the fish/POSIX split, the skip-auth pointer, and box alignment for an over-long key.
- Tests that mutate the process environment serialise on a `PROCESS_ENV_LOCK`, matching `tests/amie_mutual_exclusion.rs`.

## Codebase Integration:
- `ShellFlavor`/`render_key_setup` are typed values with behaviour rather than loose `pub fn`s over strings (Tenet 3). `key_setup` is Layer 2 and returns text; no layer below it learns about terminals.
- `EnvSnapshot` remains the single funnel for environment reads — the direct `std::env::var("AWMAN_AMIE_KEY")` this replaces was the leak the module's own doc comment warns against.
- `AmieTabBuild` replaces a `Result<(Tab, Arc<dyn ConditionGateway>), String>` rather than growing it to a 3-tuple.
- `--dangerously-skip-auth` keeps its established semantics from API mode (WI 0060/0065): per-run, hash left untouched, prominent warning.

## Documentation

- `docs/16-amie.md` gains "Authenticating to the daemon": the key and `AWMAN_AMIE_KEY`, the snippet with its rendered example, per-shell variants, key rotation via `--refresh-key`, and "Running without a key — `--dangerously-skip-auth`" covering what the flag does and does not give up.
- `docs/07-configuration.md` and the env-var tables in `docs/architecture.md` list `AWMAN_AMIE_KEY` (and `SHELL`).
- `aspec/uxui/cli.md` records the `amie start` flag semantics and adds `AWMAN_AMIE_KEY` to the environment-override list.
