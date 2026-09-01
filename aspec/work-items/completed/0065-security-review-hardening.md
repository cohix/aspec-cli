# Work Item: Task

Title: security hardening — address High/Medium/Low findings from /security-review (2026-04-29)
Issue: n/a

## Summary:
- Bind the headless server to `127.0.0.1` by default and require an explicit flag for non-loopback exposure; refuse to start on a non-loopback interface unless TLS is enabled.
- Add rustls-based HTTPS support to the headless server with a self-signed certificate generated on first run.
- Replace `curl … | bash` installers in agent Dockerfiles with versioned tarball downloads verified by `sha256sum -c`, and pin apt packages where possible.
- Pin all GitHub Actions to commit SHAs (managed by Dependabot) and add a `cargo-audit` job to `test.yml`.
- Canonicalize overlay host paths before they are handed to `docker run -v`.
- Apply restrictive Windows ACLs to `api_key.hash` equivalent to `0o600` on Unix.
- Extend `.gitignore` with common secret-bearing file patterns.
- When `--dangerously-skip-auth` is active, set an `X-Amux-Auth: disabled` response header on every request and emit a periodic warning log.


## User Stories

### User Story 1:
As a: user running `amux headless start`

I want to:
have the server bind to localhost by default

So I can:
expose the API only to processes on my own machine, and not to anyone on my LAN/VPN unless I explicitly opt in.

### User Story 2:
As a: user running `amux headless start --bind 0.0.0.0`

I want to:
have TLS enabled automatically (or be required to pass `--tls-cert`/`--tls-key`)

So I can:
ensure my Bearer API key is not transmitted in plaintext over the network.

### User Story 3:
As a: maintainer

I want to:
have all third-party GitHub Actions pinned to immutable commit SHAs and managed by Dependabot

So I can:
trust that a tag-overwrite or upstream compromise of an action does not silently inject malicious code into release builds.

### User Story 4:
As a: maintainer

I want to:
have `cargo audit` run on every push and PR

So I can:
be alerted to known CVEs in our dependency tree before they reach a release.

### User Story 5:
As a: user mounting an overlay path that contains `..` segments

I want to:
have those paths canonicalized before they are passed to `docker run -v`

So I can:
avoid surprising mount targets and ensure deduplication keys match the actual mounted location.

### User Story 6:
As a: Windows user running `amux headless start`

I want to:
have `api_key.hash` written with restrictive ACLs equivalent to `0o600` on Unix

So I can:
trust that other local users cannot read the API key hash from my profile directory.


## Implementation Details:

### 1. Headless bind address default and explicit `--bind` flag

**File:** `src/commands/headless/mod.rs`, `src/commands/headless/server.rs`, `src/cli.rs`

Add a `--bind <ADDR>` flag to `amux headless start` (default `127.0.0.1`):

```rust
// src/cli.rs
HeadlessAction::Start {
    /// Address to bind the server to. Defaults to 127.0.0.1 (loopback only).
    /// Pass 0.0.0.0 to expose to all interfaces (TLS required).
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,
    // … existing fields
}
```

In `run_start` (`src/commands/headless/mod.rs`), parse the address and propagate to the server. Replace the hard-coded address construction in `src/commands/headless/server.rs:1172`:

```rust
// Before:
let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));

// After:
let addr = std::net::SocketAddr::new(bind_ip, port);
```

Where `bind_ip: std::net::IpAddr` is parsed from the `--bind` string in `run_start` and passed into the function signature.

If `bind_ip` is not a loopback address (`!bind_ip.is_loopback()`) **and** TLS is not enabled (see §2), refuse to start with:

```
Error: --bind {addr} is a non-loopback address. TLS is required for non-loopback exposure.
       Pass --tls-cert <pem> and --tls-key <pem>, or use --auto-tls for a self-signed cert,
       or bind to 127.0.0.1 (default).
```

### 2. TLS support for the headless server

**File:** `src/commands/headless/mod.rs`, `src/commands/headless/server.rs`, `src/commands/headless/tls.rs` (new), `Cargo.toml`

Add new dependencies (rustls already transitively present via `reqwest = { features = ["rustls-tls"] }`):

```toml
axum-server = { version = "0.7", features = ["tls-rustls"] }
rcgen = "0.13"     # for self-signed cert generation
```

Add new flags to `amux headless start`:

```rust
#[arg(long, requires = "tls_key")]
tls_cert: Option<PathBuf>,

#[arg(long, requires = "tls_cert")]
tls_key: Option<PathBuf>,

/// Generate and use a self-signed certificate (stored under <headless_root>/tls/).
/// The cert SHA-256 fingerprint is printed once at startup so clients can pin it.
#[arg(long, conflicts_with_all = ["tls_cert", "tls_key"])]
auto_tls: bool,
```

New module `src/commands/headless/tls.rs`:

```rust
pub struct TlsMaterial {
    pub cert_pem_path: PathBuf,
    pub key_pem_path: PathBuf,
    pub fingerprint_sha256: String,    // hex
}

/// Generate a self-signed cert+key valid for "localhost" and the bind address,
/// store under <headless_root>/tls/{cert.pem,key.pem} with mode 0o600 on Unix,
/// and return the material plus its SHA-256 fingerprint.
pub fn ensure_self_signed(headless_root: &Path, bind_ip: IpAddr) -> Result<TlsMaterial>;

/// Load TLS material from a user-supplied cert/key pair.
pub fn from_paths(cert: PathBuf, key: PathBuf) -> Result<TlsMaterial>;
```

In `run_start`, decide a `Tls` enum:

```rust
enum Tls {
    None,
    Material(TlsMaterial),
}
```

In `server::serve` (the function containing the current `axum::serve(listener, app)` call at `src/commands/headless/server.rs:1238`), branch on `Tls`:

- `Tls::None` → `axum::serve(listener, app).with_graceful_shutdown(shutdown).await`.
- `Tls::Material(m)` → `axum_server::bind_rustls(addr, RustlsConfig::from_pem_file(&m.cert_pem_path, &m.key_pem_path).await?).serve(app.into_make_service()).await`.

Print the cert SHA-256 fingerprint in the startup banner alongside the API key:

```
TLS fingerprint (SHA-256): aa:bb:cc:…
```

Update `print_key_banner` (or add a sibling `print_tls_banner`) in `src/commands/headless/auth.rs` to emit the fingerprint.

### 3. Replace `curl … | bash` installers with verified downloads

**Files:** `templates/Dockerfile.{claude,copilot,crush,cline,gemini,maki,nanoclaw}`, `.amux/Dockerfile.{claude,copilot,crush}`

For each `curl … | bash` install, replace with a versioned download + checksum verification. Example for the Claude installer:

```dockerfile
ARG CLAUDE_INSTALLER_SHA256=<known-good-sha256>
RUN curl -fsSL -o /tmp/claude-install.sh https://claude.ai/install.sh \
    && echo "${CLAUDE_INSTALLER_SHA256}  /tmp/claude-install.sh" | sha256sum -c - \
    && bash /tmp/claude-install.sh \
    && rm /tmp/claude-install.sh
```

For nodesource, prefer the official Debian `nodejs` package (pinned via `apt-get install -y nodejs=<version>`) over `setup_20.x`. If the setup script remains necessary, pin the script SHA the same way.

For each Dockerfile, also pin apt packages where the upstream provides versioned packages:

```dockerfile
RUN apt-get update && apt-get install -y --no-install-recommends \
    git=1:2.* \
    ca-certificates=2024* \
    curl=7.* \
    && rm -rf /var/lib/apt/lists/*
```

(Use the actual versions present in `debian:bookworm-slim` at the time of the change — confirm with `apt-cache policy <pkg>` in a build step.)

Update `Dockerfile.dev` similarly: pin the rustup installer SHA and the gh apt package version.

### 4. Pin GitHub Actions to commit SHAs

**Files:** `.github/workflows/test.yml`, `.github/workflows/release.yml`, `.github/dependabot.yml` (new)

Replace each `uses:` line with a commit SHA and a comment documenting the tag at pin time:

```yaml
- uses: actions/checkout@a81bbbf8298c0fa03ea29cdc473d45769f953675  # v4.2.2
- uses: actions/cache@1bd1e32a3bdc45362d1e726936510720a7c30a57    # v4.2.0
- uses: actions/upload-artifact@65c4c4a1ddee5b72f698fdd19549f0f0fb45cf08  # v4.6.0
- uses: actions/download-artifact@fa0a91b85d4f404e444e00e005971372dc801d16  # v4.1.8
- uses: dtolnay/rust-toolchain@<sha>  # stable, 2026-04-29
- uses: softprops/action-gh-release@<sha>  # v2.x.y
```

Add `.github/dependabot.yml` to manage SHA bumps:

```yaml
version: 2
updates:
  - package-ecosystem: github-actions
    directory: /
    schedule:
      interval: weekly
  - package-ecosystem: cargo
    directory: /
    schedule:
      interval: weekly
```

### 5. Add `cargo-audit` to CI

**File:** `.github/workflows/test.yml`

Add a new job (parallel to `test`):

```yaml
audit:
  name: Cargo audit
  runs-on: ubuntu-latest
  steps:
    - uses: actions/checkout@<sha>
    - uses: dtolnay/rust-toolchain@<sha>
      with:
        toolchain: "1.94.0"
    - name: Install cargo-audit
      run: cargo install --locked cargo-audit
    - name: Run cargo audit
      run: cargo audit --deny warnings
```

Optionally also `cargo install --locked cargo-deny && cargo deny check` for license + advisory + ban checks.

### 6. Canonicalize overlay host paths before mount

**File:** `src/overlays/mod.rs`, `src/overlays/directory.rs`, `src/overlays/parser.rs`

Today `make_host_path_absolute` (`src/overlays/mod.rs:32`) joins relative paths with cwd but does not collapse `..` segments. `DirectoryOverlay::key()` (`src/overlays/directory.rs:90`) calls `fs::canonicalize` only for dedup, not for the value passed to `docker run -v`.

Add a new function `make_host_path_canonical(path: &str) -> Result<PathBuf>` that:

1. Calls `make_host_path_absolute` for the existing absolute-path resolution.
2. Calls `std::fs::canonicalize` on the result.
3. If canonicalize fails because the path does not yet exist, walk up to the nearest existing ancestor, canonicalize that, and re-append the missing tail (collapsing any `..` segments along the way using `path-clean` or equivalent logic — do **not** silently fall through to the un-canonicalized path).

Use this in `DirectoryOverlay::new` for `host_path` so the canonical form is what gets emitted at `src/runtime/docker.rs:66` (`overlay.host_path.display()`).

Reject paths that escape the user's home directory or the git root unless explicitly allowed — defer the policy decision to a later work item, but at minimum log a `tracing::warn!` when an overlay's canonical path is outside both the home directory and the git root.

### 7. Windows ACL hardening for `api_key.hash`

**File:** `src/commands/headless/auth.rs`, `Cargo.toml`

Add a Windows-only dependency:

```toml
[target.'cfg(windows)'.dependencies]
windows-acl = "0.3"
```

Extend `write_key_hash` (around `src/commands/headless/auth.rs:42`):

```rust
#[cfg(unix)]
{
    use std::os::unix::fs::OpenOptionsExt;
    // existing 0o600 logic
}

#[cfg(windows)]
{
    std::fs::write(&path, hash.as_bytes())?;
    apply_owner_only_acl(&path)?;   // remove inherited ACEs, grant FullControl to current user only
}
```

Implement `apply_owner_only_acl` using `windows-acl` to set a DACL with a single ACE granting `GENERIC_ALL` to the current user SID, with no inherited ACEs.

### 8. Extend `.gitignore`

**File:** `.gitignore`

Append:

```
# Secrets and credentials — defence in depth, no such files should be committed
.env
.env.*
*.pem
*.key
*.p12
*.pfx
secrets.json
credentials.json
```

### 9. Per-request indication when authentication is disabled

**File:** `src/commands/headless/server.rs`, `src/commands/headless/mod.rs`

When `auth_mode == AuthMode::Disabled`, the auth middleware (the function around `src/commands/headless/server.rs:85` that currently performs the API-key check) should:

1. Insert `X-Amux-Auth: disabled` into the response headers on every response.
2. Emit a `tracing::warn!` once per minute (gated by a `LastWarned: Mutex<Instant>` in `AppState`) so logs make it obvious the server is running without auth.

The startup `eprintln!` warning at `src/commands/headless/mod.rs:53` should additionally include the bind address and the URL clients should connect to, e.g.:

```
WARNING: authentication is disabled (--dangerously-skip-auth).
         Server listening on http://127.0.0.1:9876 — anyone with access to this address can
         issue commands on your behalf.
```


## Edge Case Considerations:

- **`--bind 127.0.0.1` with `--auto-tls`**: TLS should still be supported on loopback (some clients require it). Allow the combination but skip the "TLS required" check; do not skip cert generation.
- **Existing clients pinned to the old default**: The previous default `0.0.0.0` is a behaviour change. Document the change in `docs/releases/<next-version>.md` and emit a deprecation hint when the user passes `--bind 0.0.0.0` without TLS material that the previous default was insecure.
- **`auto_tls` cert rotation**: The self-signed cert generated under `<headless_root>/tls/` should not be regenerated on every start (clients pin its fingerprint). Regenerate only when `--refresh-key` is also passed, or when the cert is expired (>= 1 year old). Document this in the help text.
- **Dockerfile checksum drift**: Upstream installer scripts can change content under the same URL. The pinned SHA must be updated when the installer legitimately changes; provide a documented procedure (`scripts/refresh-dockerfile-shas.sh` or a `make refresh-shas` target) so this is not done ad hoc.
- **Action SHA pinning + Dependabot**: When Dependabot opens PRs, the SHA changes — confirm CI does not re-pin to the old SHA. The Dependabot config above handles this.
- **`cargo-audit` first-run failure**: If `cargo audit` flags an existing CVE the day this lands, the CI job will fail. Either (a) fix the advisory in the same PR, (b) add `--ignore RUSTSEC-XXXX-YYYY` with a tracked follow-up, or (c) split this into a separate work item that lands after the fix.
- **Path canonicalization with non-existent ancestors**: `fs::canonicalize` errors if any path component is missing. The walk-up-to-existing-ancestor approach handles this; tests must cover (a) all components exist, (b) only the leaf is missing, (c) several components are missing, (d) the path is already canonical.
- **Windows ACL on shared profile**: If `headless_root` is on a network drive or shared volume, the user SID may not exist on the file server. Detect this and fall back to a `tracing::warn!` with instructions; do not silently leave the file world-readable.
- **`.gitignore` patterns and tracked files**: `.env` files already tracked in the repo are unaffected by `.gitignore`. Run `git ls-files` against the new patterns as part of the PR review and fail loudly if anything matches (none should currently).
- **`X-Amux-Auth: disabled` header for clients**: Existing clients should ignore unknown headers; document the header in `aspec/uxui/cli.md` and any client SDK docs.


## Test Considerations:

### Unit tests

- **`src/commands/headless/server.rs`**:
  - `bind_ip == 127.0.0.1` produces a `SocketAddr` with the loopback address.
  - `bind_ip == 0.0.0.0` without TLS material returns the "TLS required" error.
  - `bind_ip == 0.0.0.0` with TLS material starts successfully (use a `tokio::test` with a quickly-shutting-down listener).
  - Auth middleware in `Disabled` mode inserts `X-Amux-Auth: disabled` on every response.
  - Auth middleware in `Disabled` mode logs at most once per minute (use a fake clock).

- **`src/commands/headless/tls.rs`**:
  - `ensure_self_signed` creates `cert.pem` and `key.pem` under `<headless_root>/tls/` with mode `0o600` on Unix.
  - `ensure_self_signed` is idempotent — calling it twice with no expiry returns the same fingerprint.
  - `ensure_self_signed` regenerates on `--refresh-key`.
  - `from_paths` returns the same fingerprint as `openssl x509 -fingerprint -sha256 -in cert.pem`.

- **`src/overlays/mod.rs`**:
  - `make_host_path_canonical("/foo/baz/../bar")` returns `/foo/bar` even when `/foo/bar` does not exist.
  - `make_host_path_canonical` follows symlinks when intermediate components exist.
  - Outside-home-and-repo paths produce a `tracing::warn!` (use `tracing-test` to assert).

- **`src/commands/headless/auth.rs`**:
  - On Windows, `write_key_hash` produces a file whose effective DACL contains exactly one ACE for the current user with `GENERIC_ALL`. Use a Windows-only `#[cfg(windows)]` test.
  - On Unix, mode is `0o600` (existing test continues to pass).

### Integration tests

- `amux headless start` (no flags) binds to `127.0.0.1:9876`, no TLS, and a non-loopback client (e.g. `curl http://<lan-ip>:9876/v1/status`) is rejected by the OS (connection refused).
- `amux headless start --bind 0.0.0.0` without `--auto-tls`/cert material exits with the "TLS required" error.
- `amux headless start --bind 0.0.0.0 --auto-tls` starts and serves over HTTPS; the cert fingerprint is printed at startup.
- `amux headless start --dangerously-skip-auth` returns `X-Amux-Auth: disabled` on `GET /v1/status`.

### CI/CD tests

- `.github/workflows/test.yml` includes a job `audit` that runs `cargo audit --deny warnings` and passes on a clean dependency tree.
- All `uses:` lines in both workflows match the regex `^[\w./-]+@[0-9a-f]{40}\b` (commit SHA, not tag). Add a small repo-level lint script `scripts/check-action-pins.sh` invoked by `make lint`.

### End-to-end

- Build each agent Dockerfile (`templates/Dockerfile.{claude,copilot,crush,cline,gemini}`) end-to-end with the new checksum-verified installers and confirm the resulting image still passes the existing agent smoke tests.
- Tamper test: change one byte of a pinned SHA and confirm the build fails at `sha256sum -c`.


## Codebase Integration:

- Follow established conventions, best practices, testing, and architecture patterns from the project's `aspec/`.
- New TLS module (`src/commands/headless/tls.rs`) lives alongside `auth.rs`; declare it in `src/commands/headless/mod.rs` and re-export only the public types.
- Reuse `subtle::ConstantTimeEq` for any new credential comparison; do not introduce custom timing-safe code.
- Reuse `ring` for any new randomness; do not introduce a second RNG dependency.
- Use `axum-server` (small, well-maintained) rather than rolling rustls into an `axum::serve` adapter manually.
- All new config fields are `Option<…>` with `#[serde(skip_serializing_if = "Option::is_none")]`, matching the rest of `src/config/mod.rs`.
- Update `aspec/architecture/security.md` to document: (a) the new default loopback bind, (b) the TLS requirement for non-loopback, (c) the threat model that motivates each.
- Update `aspec/uxui/cli.md` with the new `--bind`, `--tls-cert`, `--tls-key`, and `--auto-tls` flags.
- Update `docs/` to reflect the new configuration surface and migration guidance after implementation.
- Reference the originating /security-review report findings #1, #2, and #4–#11 in the commit message and the release notes for the version that ships this work.
