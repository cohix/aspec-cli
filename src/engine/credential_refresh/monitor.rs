//! The credential-refresh monitor: owns the lease registry and the tick loop
//! that keeps every live container's staged credential file fresh.
//!
//! Design priorities, in order:
//! 1. **Never write to a dead session's path.** Three independent defenses:
//!    the RAII lease (INV-6), a monotonic generation re-checked before every
//!    write, and a path-existence check inside the atomic writer (INV-7).
//! 2. **Never miss a live credentialed container.** Registration happens at the
//!    single spawn choke point, before the child process starts.
//! 3. **Survive every exit path.** The lease drops on success, error, panic
//!    unwind and teardown; the monitor loop parks (zero CPU) when no lease is
//!    live and never makes a refresh failure fatal to a session (INV-4).
//!
//! The loop runs on a dedicated background thread with its own current-thread
//! Tokio runtime so it depends on no ambient runtime; it is started lazily on
//! the first lease and parks when the registry empties.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime};

use crate::data::fs::auth_paths::AuthPathResolver;
use crate::data::session::AgentName;
use crate::engine::auth::credential::{
    CredentialBinding, CredentialFile, CredentialFingerprint, CredentialReadError,
};
use crate::engine::auth::keychain::refreshable_spec_for;
use crate::engine::auth::RefreshableCredentialDelivery;
use crate::engine::ready::{refresh_host_credential, HostRefreshOutcome};

use super::lease::{CredentialLease, LeaseRegistry, RegistryShared};

/// Backoff is capped at `tick_interval * 2^MAX_BACKOFF_SHIFT` so a persistently
/// unreachable host does not push the retry interval to absurd lengths.
const MAX_BACKOFF_SHIFT: u32 = 6;

const REMEDIATION: &str = "run `claude` on the host / check login";

const STAGED_WRITE_REMEDIATION: &str =
    "staged credential rewrite failed; check disk/permissions — will retry";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorConfig {
    /// Refresh when `expires_at - now` drops below this. Default 20 min.
    pub refresh_threshold: Duration,
    /// Tick period while at least one lease is live. Default 60 s.
    pub tick_interval: Duration,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            refresh_threshold: Duration::from_secs(20 * 60),
            tick_interval: Duration::from_secs(60),
        }
    }
}

/// What one refresh attempt did. `fingerprint` is the 8-hex short form — the
/// ONLY credential-derived value permitted in logs or status output (INV-5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// Credential was still comfortably valid; nothing done.
    NotNeeded { expires_in: Duration },
    /// Host rotated (or independently changed) and live leases were rewritten.
    Refreshed {
        leases_written: usize,
        fingerprint: String,
    },
    /// Host ping ran but `expiresAt` did not advance. Last-known-good file is
    /// retained; `remediation` is user-facing text.
    Stale { remediation: String },
    /// The host credential could not be read at all.
    Unavailable { reason: CredentialReadError },
}

/// Per-agent health for status surfacing. Never carries a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshStatus {
    pub agent: AgentName,
    pub live_leases: usize,
    pub expires_at: Option<SystemTime>,
    pub last_outcome: Option<RefreshOutcome>,
    /// Consecutive failures; drives the retry backoff.
    pub consecutive_failures: u32,
}

#[derive(Default)]
struct AgentState {
    /// Fingerprint of the snapshot the staged files currently reflect. Change
    /// detection compares against this; a match means no rewrite is needed.
    last_materialized: Option<CredentialFingerprint>,
    last_outcome: Option<RefreshOutcome>,
    expires_at: Option<SystemTime>,
    consecutive_failures: u32,
    /// Tick-loop backoff gate. `None` = attempt now.
    retry_after: Option<Instant>,
}

/// Owns the lease registry and the tick loop.
///
/// The loop runs ONLY while the registry is non-empty: it is started by the
/// first `register()` and parks when the registry empties, so an `awman`
/// invocation that never launches an agent does zero polling.
pub struct CredentialRefreshMonitor {
    config: MonitorConfig,
    registry: LeaseRegistry,
    binding: CredentialBinding,
    state: Mutex<HashMap<String, AgentState>>,
    /// Serializes host-refresh operations so the single-use refresh token is
    /// never raced by the tick loop and a concurrent `refresh_now` (INV-4).
    refresh_gate: tokio::sync::Mutex<()>,
    thread_started: std::sync::Once,
}

impl CredentialRefreshMonitor {
    pub fn new(config: MonitorConfig) -> Arc<Self> {
        let resolver = AuthPathResolver::from_process_env()
            .unwrap_or_else(|_| AuthPathResolver::at_home(std::env::temp_dir()));
        Self::with_resolver(config, resolver)
    }

    /// Construct a monitor rooted at an explicit home directory, still reading
    /// whichever credential store the platform uses natively.
    ///
    /// NOTE for tests: on macOS the Claude descriptor reads the Keychain and
    /// ignores the home directory, so this alone does NOT isolate a test from
    /// the developer's real credential. Use [`Self::with_binding`] with
    /// [`CredentialBinding::to_file`] for an isolation that holds on every
    /// platform.
    pub fn with_resolver(config: MonitorConfig, resolver: AuthPathResolver) -> Arc<Self> {
        Self::with_binding(config, CredentialBinding::platform_default(resolver))
    }

    /// Construct a monitor bound to an explicit credential source, so the tick
    /// loop never reads a developer's real credential nor spawns a real host
    /// ping (F3) — on any platform.
    pub fn with_binding(config: MonitorConfig, binding: CredentialBinding) -> Arc<Self> {
        Arc::new(Self {
            config,
            registry: LeaseRegistry::new(),
            binding,
            state: Mutex::new(HashMap::new()),
            refresh_gate: tokio::sync::Mutex::new(()),
            thread_started: std::sync::Once::new(),
        })
    }

    /// Register a live credentialed container. Called from the container
    /// backends' `build()` — the single spawn choke point.
    pub fn register(
        self: &Arc<Self>,
        delivery: &RefreshableCredentialDelivery,
        container: &str,
    ) -> CredentialLease {
        // Seed change detection from the fingerprint the staged file was first
        // written with, so an unchanged first tick performs no rewrite.
        self.seed_state(delivery);
        let lease = self.registry.register(delivery, container);
        self.ensure_thread();
        tracing::debug!(
            agent = %delivery.agent,
            container = container,
            generation = lease.generation().as_u64(),
            "credential refresh: lease registered"
        );
        lease
    }

    /// Synchronous, bounded refresh — the §5 pre-step guard. Triggers the
    /// descriptor's host refresh, waits at most `timeout`, rewrites live leases
    /// if the snapshot changed, and reports what happened. NEVER fatal.
    pub async fn refresh_now(
        self: &Arc<Self>,
        agent: &AgentName,
        timeout: Duration,
    ) -> RefreshOutcome {
        match tokio::time::timeout(timeout, self.refresh_agent(agent)).await {
            Ok(outcome) => outcome,
            Err(_elapsed) => {
                tracing::warn!(
                    agent = %agent,
                    remediation = REMEDIATION,
                    "credential refresh timed out; keeping last-known-good"
                );
                let expires_at = self.recorded_expiry(agent);
                let outcome = RefreshOutcome::Stale {
                    remediation: REMEDIATION.to_string(),
                };
                self.record_status(agent, expires_at, outcome.clone(), true);
                outcome
            }
        }
    }

    /// Per-agent health for status surfacing. Never returns a secret.
    pub fn status(&self) -> Vec<RefreshStatus> {
        let mut live_counts: HashMap<String, usize> = HashMap::new();
        for lease in self.registry.snapshot() {
            *live_counts
                .entry(lease.agent.as_str().to_string())
                .or_default() += 1;
        }
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut names: Vec<String> = state.keys().cloned().collect();
        for name in live_counts.keys() {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        names
            .into_iter()
            .filter_map(|name| {
                let agent = AgentName::new(&name).ok()?;
                let st = state.get(&name);
                Some(RefreshStatus {
                    agent,
                    live_leases: live_counts.get(&name).copied().unwrap_or(0),
                    expires_at: st.and_then(|s| s.expires_at),
                    last_outcome: st.and_then(|s| s.last_outcome.clone()),
                    consecutive_failures: st.map(|s| s.consecutive_failures).unwrap_or(0),
                })
            })
            .collect()
    }

    pub fn live_lease_count(&self) -> usize {
        self.registry.len()
    }

    // ── internals ───────────────────────────────────────────────────────────

    fn seed_state(&self, delivery: &RefreshableCredentialDelivery) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let st = state
            .entry(delivery.agent.as_str().to_string())
            .or_default();
        if st.last_materialized.is_none() {
            st.last_materialized = Some(delivery.initial_fingerprint);
        }
    }

    fn ensure_thread(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let shared = self.registry.shared();
        self.thread_started.call_once(move || {
            let spawn = std::thread::Builder::new()
                .name("awman-cred-refresh".to_string())
                .spawn(move || run_monitor_loop(weak, shared));
            if let Err(e) = spawn {
                tracing::warn!(error = %e, "credential refresh: could not start monitor thread");
            }
        });
    }

    /// One tick: refresh every agent that has live leases, honouring per-agent
    /// backoff so a persistently unreachable host is retried, not hammered.
    async fn tick(self: &Arc<Self>) {
        let mut agents: Vec<AgentName> = Vec::new();
        for lease in self.registry.snapshot() {
            if !agents.contains(&lease.agent) {
                agents.push(lease.agent.clone());
            }
        }
        for agent in agents {
            if self.in_backoff(&agent) {
                continue;
            }
            // Bound the background refresh exactly as `refresh_now` bounds the
            // pre-step guard, so a wedged host ping (interactive auth prompt,
            // stuck network) cannot stall the tick loop for every live session
            // (F1). A timeout is a loud, retried failure — never silent.
            match tokio::time::timeout(self.config.tick_interval, self.refresh_agent(&agent)).await
            {
                Ok(_outcome) => {}
                Err(_elapsed) => {
                    tracing::warn!(
                        agent = %agent,
                        remediation = REMEDIATION,
                        "credential refresh: background tick timed out; keeping last-known-good"
                    );
                    let expires_at = self.recorded_expiry(&agent);
                    let outcome = RefreshOutcome::Stale {
                        remediation: REMEDIATION.to_string(),
                    };
                    self.record_status(&agent, expires_at, outcome, true);
                }
            }
        }
    }

    /// Read the host credential, refresh the host if near expiry, and rewrite
    /// every live lease when the snapshot changed. Shared by the tick loop and
    /// `refresh_now`. Never returns an error: a failure is an outcome.
    async fn refresh_agent(self: &Arc<Self>, agent: &AgentName) -> RefreshOutcome {
        // Serialize host-refresh so parallel containers never race the
        // single-use refresh token, and the tick loop never collides with a
        // concurrent `refresh_now`.
        let _gate = self.refresh_gate.lock().await;

        let Some(spec) = refreshable_spec_for(agent) else {
            return RefreshOutcome::Unavailable {
                reason: CredentialReadError::Unsupported,
            };
        };
        let source = self.binding.source_for(spec);

        let snapshot = match (spec.read)(&source) {
            Ok(snapshot) => snapshot,
            Err(reason) => {
                tracing::warn!(
                    agent = %agent,
                    reason = %reason,
                    remediation = REMEDIATION,
                    "credential refresh: host credential unreadable; keeping last-known-good"
                );
                let outcome = RefreshOutcome::Unavailable {
                    reason: reason.clone(),
                };
                self.record_status(agent, None, outcome.clone(), true);
                return outcome;
            }
        };

        let now = SystemTime::now();
        let near_expiry = match (spec.expiry)(&snapshot) {
            Some(expiry) => {
                expiry <= now
                    || expiry
                        .duration_since(now)
                        .map(|remaining| remaining < self.config.refresh_threshold)
                        .unwrap_or(true)
            }
            None => false,
        };

        // If near expiry, ask the host to rotate and re-read. A host that
        // cannot rotate (asleep, keychain locked, offline, logged out) keeps
        // its last-known-good file — loudly, never silently.
        let mut stale_remediation: Option<String> = None;
        let snapshot = if near_expiry {
            match refresh_host_credential(spec, &self.binding).await {
                HostRefreshOutcome::Advanced { .. } => match (spec.read)(&source) {
                    Ok(fresh) => fresh,
                    Err(reason) => {
                        tracing::warn!(
                            agent = %agent,
                            reason = %reason,
                            "credential refresh: post-refresh read failed; keeping last-known-good"
                        );
                        let outcome = RefreshOutcome::Unavailable {
                            reason: reason.clone(),
                        };
                        self.record_status(agent, None, outcome.clone(), true);
                        return outcome;
                    }
                },
                HostRefreshOutcome::NotAdvanced { remediation } => {
                    tracing::warn!(
                        agent = %agent,
                        remediation = %remediation,
                        "credential refresh: host did not rotate; keeping last-known-good"
                    );
                    stale_remediation = Some(remediation);
                    snapshot
                }
                HostRefreshOutcome::PingFailed { result } => {
                    tracing::warn!(
                        agent = %agent,
                        ?result,
                        remediation = REMEDIATION,
                        "credential refresh: host refresh ping failed; keeping last-known-good"
                    );
                    stale_remediation = Some(REMEDIATION.to_string());
                    snapshot
                }
            }
        } else {
            snapshot
        };

        // Reconcile every live lease against the desired materialization each
        // tick. This rewrites a host rotation AND repairs any staged file a
        // container may have corrupted, even when the host fingerprint is
        // unchanged (INV-4 corollary, HIGH-5). Fingerprint compare — never the
        // secret.
        let fingerprint = CredentialFingerprint::of(&snapshot);
        let expiry_now = (spec.expiry)(&snapshot);
        let file = (spec.materialize)(&snapshot);
        let (leases_written, write_failures) = self.rewrite_live_leases(agent, &file);
        if leases_written > 0 {
            tracing::info!(
                agent = %agent,
                fingerprint = %fingerprint.to_hex(),
                leases_written,
                "credential refresh: staged credential rewritten"
            );
        }
        // Advance the host-change fingerprint only when every live lease is
        // current. A partial write failure must be retried on the next tick,
        // not permanently masked by a matching host fingerprint (HIGH-4).
        if write_failures == 0 {
            self.set_last_materialized(agent, fingerprint);
        }

        // A stale host is a failure (drives backoff) but never fatal.
        if let Some(remediation) = stale_remediation {
            let outcome = RefreshOutcome::Stale { remediation };
            self.record_status(agent, expiry_now, outcome.clone(), true);
            return outcome;
        }

        // A staged-write failure is likewise a loud, retried failure: it keeps
        // backoff/retry state so the affected container is brought current on a
        // later tick instead of silently stranded on the old token (HIGH-4).
        if write_failures > 0 {
            let outcome = RefreshOutcome::Stale {
                remediation: STAGED_WRITE_REMEDIATION.to_string(),
            };
            self.record_status(agent, expiry_now, outcome.clone(), true);
            return outcome;
        }

        let outcome = if leases_written > 0 {
            RefreshOutcome::Refreshed {
                leases_written,
                fingerprint: fingerprint.to_hex(),
            }
        } else {
            let expires_in = expiry_now
                .and_then(|e| e.duration_since(now).ok())
                .unwrap_or(Duration::ZERO);
            RefreshOutcome::NotNeeded { expires_in }
        };
        self.record_status(agent, expiry_now, outcome.clone(), false);
        outcome
    }

    /// Reconcile `file` into every live lease of `agent`, returning
    /// `(written, failures)`.
    ///
    /// This runs every tick, not only when the host token changes, so a staged
    /// file a container may have truncated, replaced, or grown a `refreshToken`
    /// in is repaired even while the host fingerprint is unchanged (INV-4
    /// corollary, HIGH-5). A file whose bytes already equal `file` is left
    /// untouched (no write, no log noise). A target that has gone missing is a
    /// SKIP, never a recreate (INV-7 defense 3, HIGH-7): a missing file means
    /// the lease is racing its own drop.
    ///
    /// Each candidate re-checks the generation (defense 2) immediately before
    /// the write; a lease dropped since the snapshot loses the race safely. A
    /// write error warns, counts as a failure (so the fingerprint is NOT
    /// advanced and the write is retried next tick), and is never fatal.
    fn rewrite_live_leases(&self, agent: &AgentName, file: &CredentialFile) -> (usize, usize) {
        let mut written = 0;
        let mut failures = 0;
        for lease in self.registry.snapshot() {
            if &lease.agent != agent {
                continue;
            }
            // Generation re-check immediately before the write: a lease dropped
            // since the snapshot loses the race safely.
            if !self.registry.is_live(lease.generation) {
                continue;
            }
            // Skip a target that no longer exists — the monitor never recreates
            // a file a live lease's container may have deleted; that is treated
            // as a dropped-lease race, not repaired (INV-7 defense 3).
            match std::fs::read(&lease.staged_path) {
                Ok(existing) if existing == file.contents => continue,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::debug!(
                        container = %lease.container,
                        "credential refresh: staged file gone; skipping (dropped-lease race)"
                    );
                    continue;
                }
                _ => {}
            }
            // Re-check liveness once more, immediately before the write, to
            // minimize the TOCTOU window between the snapshot and the rename
            // (HIGH-7). Combined with the monotonic, never-reused generation and
            // the process-retained staged TempDir (INV-7), a write that still
            // races a lease drop lands only in that lease's own throwaway dir,
            // which is never recycled for another session.
            if !self.registry.is_live(lease.generation) {
                continue;
            }
            match crate::engine::overlay::write_credential_file_atomic(&lease.staged_root, file) {
                Ok(true) => written += 1,
                Ok(false) => {
                    tracing::debug!(
                        container = %lease.container,
                        "credential refresh: staged path gone; skipping (dropped-lease race)"
                    );
                }
                Err(e) => {
                    failures += 1;
                    tracing::warn!(
                        container = %lease.container,
                        error = %e,
                        "credential refresh: staged rewrite failed; will retry next tick"
                    );
                }
            }
        }
        (written, failures)
    }

    fn set_last_materialized(&self, agent: &AgentName, fingerprint: CredentialFingerprint) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .entry(agent.as_str().to_string())
            .or_default()
            .last_materialized = Some(fingerprint);
    }

    fn record_status(
        &self,
        agent: &AgentName,
        expires_at: Option<SystemTime>,
        outcome: RefreshOutcome,
        failed: bool,
    ) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let st = state.entry(agent.as_str().to_string()).or_default();
        st.expires_at = expires_at;
        st.last_outcome = Some(outcome);
        if failed {
            st.consecutive_failures = st.consecutive_failures.saturating_add(1);
            let shift = st.consecutive_failures.min(MAX_BACKOFF_SHIFT);
            let backoff = self.config.tick_interval.saturating_mul(1u32 << shift);
            st.retry_after = Some(Instant::now() + backoff);
        } else {
            st.consecutive_failures = 0;
            st.retry_after = None;
        }
    }

    fn in_backoff(&self, agent: &AgentName) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(agent.as_str())
            .and_then(|s| s.retry_after)
            .map(|t| Instant::now() < t)
            .unwrap_or(false)
    }

    fn recorded_expiry(&self, agent: &AgentName) -> Option<SystemTime> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(agent.as_str())
            .and_then(|s| s.expires_at)
    }
}

impl Drop for CredentialRefreshMonitor {
    fn drop(&mut self) {
        // Wake the parked background thread so it observes shutdown and exits.
        self.registry.request_shutdown();
    }
}

/// The tick loop body, run on the dedicated background thread. Holds only a
/// `Weak` to the monitor, so a monitor that is dropped (e.g. in tests) lets the
/// loop exit rather than leaking the thread.
fn run_monitor_loop(weak: Weak<CredentialRefreshMonitor>, shared: Arc<RegistryShared>) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "credential refresh: monitor runtime build failed");
            return;
        }
    };
    loop {
        // Park (zero CPU) until at least one lease is live, or shutdown.
        if !shared.wait_until_active() {
            break;
        }
        let Some(monitor) = weak.upgrade() else {
            break;
        };
        let tick_interval = monitor.config.tick_interval;
        // A panic in a tick must not silently kill the refresh thread and strand
        // every live session unrefreshed (F2). Catch it, log, and continue; the
        // `state` mutex is accessed poison-tolerantly so a poisoned lock from an
        // earlier panic does not cascade into permanent tick death.
        let tick_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(monitor.tick());
        }));
        if tick_result.is_err() {
            tracing::error!("credential refresh: tick panicked; continuing next tick");
        }
        drop(monitor);
        if shared.sleep_or_shutdown(tick_interval) {
            break;
        }
    }
}

// ── Process-global installation ─────────────────────────────────────────────
//
// The container backends have no access to `Engines`, so the monitor is reached
// through a process-global. `global()` returning `None` means "no monitor
// installed" — every lease call becomes a no-op and behaviour is exactly
// today's. That is also how the `authRefresh.enabled: false` kill switch is
// implemented: install nothing.

static GLOBAL_MONITOR: OnceLock<Arc<CredentialRefreshMonitor>> = OnceLock::new();

/// Install the process monitor. Idempotent; the first install wins. Called once
/// from the command layer when the effective config enables refresh.
pub fn install_global(monitor: Arc<CredentialRefreshMonitor>) {
    let _ = GLOBAL_MONITOR.set(monitor);
}

/// `None` when no monitor was installed.
pub fn global() -> Option<Arc<CredentialRefreshMonitor>> {
    GLOBAL_MONITOR.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_20min_and_60s() {
        let c = MonitorConfig::default();
        assert_eq!(c.refresh_threshold, Duration::from_secs(1200));
        assert_eq!(c.tick_interval, Duration::from_secs(60));
    }

    #[test]
    fn empty_monitor_reports_no_leases_and_no_status() {
        let monitor = CredentialRefreshMonitor::new(MonitorConfig::default());
        assert_eq!(monitor.live_lease_count(), 0);
        assert!(monitor.status().is_empty());
    }

    /// HIGH-7 / INV-7 defense 3: the monitor never RECREATES a staged file that
    /// a live lease's container has deleted — a missing target is a skip, not a
    /// write. Only present-but-drifted files are repaired.
    #[test]
    fn reconcile_skips_missing_target_instead_of_recreating() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        let host_file = home.path().join(".claude/.credentials.json");
        let expiry_ms = (SystemTime::now() + Duration::from_secs(7200))
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        std::fs::write(
            &host_file,
            format!(r#"{{"claudeAiOauth":{{"accessToken":"tokA","expiresAt":{expiry_ms}}}}}"#),
        )
        .unwrap();

        // Bind the credential source to the planted FILE, not to the platform
        // default: on macOS the default is the Keychain, which would ignore
        // this temp HOME and read the developer's real credential.
        let binding =
            CredentialBinding::to_file(AuthPathResolver::at_home(home.path()), &host_file);
        let spec = refreshable_spec_for(&AgentName::new("claude").unwrap()).unwrap();
        let snapshot = (spec.read)(&binding.source_for(spec)).unwrap();
        let fingerprint = CredentialFingerprint::of(&snapshot);

        let staged = tempfile::tempdir().unwrap();
        let staged_path = staged.path().join(".credentials.json");
        // Deliberately do NOT create the staged file: the container "deleted" it.

        let monitor = CredentialRefreshMonitor::with_binding(
            MonitorConfig {
                refresh_threshold: Duration::from_secs(60),
                tick_interval: Duration::from_secs(3600),
            },
            binding,
        );
        let delivery = RefreshableCredentialDelivery {
            agent: AgentName::new("claude").unwrap(),
            spec_agent: "claude",
            credential_env_key: "CLAUDE_CODE_OAUTH_TOKEN",
            staged_path: staged_path.clone(),
            staged_root: staged.path().to_path_buf(),
            initial_fingerprint: fingerprint,
        };
        let lease = monitor.register(&delivery, "awman-missing");

        let _ = tokio::runtime::Runtime::new().unwrap().block_on(
            monitor.refresh_now(&AgentName::new("claude").unwrap(), Duration::from_secs(2)),
        );

        assert!(
            !staged_path.exists(),
            "the monitor must not recreate a staged file the container deleted"
        );
        drop(lease);
    }

    /// HIGH-5: a container that corrupts its staged `.credentials.json` is
    /// repaired on the next reconciliation even when the HOST token is
    /// unchanged (same fingerprint). The monitor no longer skips the write just
    /// because the host fingerprint matches `last_materialized`.
    ///
    #[test]
    fn reconcile_repairs_drifted_staged_file_when_host_unchanged() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        let host_file = home.path().join(".claude/.credentials.json");
        let expiry_ms = (SystemTime::now() + Duration::from_secs(7200))
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        std::fs::write(
            &host_file,
            format!(
                r#"{{"claudeAiOauth":{{"accessToken":"tokA","expiresAt":{expiry_ms},"refreshToken":"never"}}}}"#
            ),
        )
        .unwrap();

        let spec = refreshable_spec_for(&AgentName::new("claude").unwrap()).unwrap();
        // Explicit file binding, so this fixture means the same thing on macOS
        // as it does on Linux (see the sibling test).
        let binding =
            CredentialBinding::to_file(AuthPathResolver::at_home(home.path()), &host_file);
        let snapshot = (spec.read)(&binding.source_for(spec)).unwrap();
        let fingerprint = CredentialFingerprint::of(&snapshot);
        let file = (spec.materialize)(&snapshot);

        let staged = tempfile::tempdir().unwrap();
        let staged_path = staged.path().join(".credentials.json");
        std::fs::write(&staged_path, &file.contents).unwrap();

        // Use a long tick so only our explicit refresh_now reconciles (the
        // background thread stays parked between our call and the assertion).
        let monitor = CredentialRefreshMonitor::with_binding(
            MonitorConfig {
                refresh_threshold: Duration::from_secs(60),
                tick_interval: Duration::from_secs(3600),
            },
            binding,
        );
        let delivery = RefreshableCredentialDelivery {
            agent: AgentName::new("claude").unwrap(),
            spec_agent: "claude",
            credential_env_key: "CLAUDE_CODE_OAUTH_TOKEN",
            staged_path: staged_path.clone(),
            staged_root: staged.path().to_path_buf(),
            initial_fingerprint: fingerprint,
        };
        let lease = monitor.register(&delivery, "awman-drift");

        // A container truncates/replaces the staged file.
        std::fs::write(&staged_path, b"{ corrupted by container }").unwrap();

        let _ = tokio::runtime::Runtime::new().unwrap().block_on(
            monitor.refresh_now(&AgentName::new("claude").unwrap(), Duration::from_secs(2)),
        );

        assert_eq!(
            std::fs::read(&staged_path).unwrap(),
            file.contents,
            "the drifted staged file must be repaired even though the host token is unchanged"
        );
        drop(lease);
    }

    #[test]
    fn register_seeds_state_and_counts_lease() {
        // A per-test temp HOME bound as an explicit FILE source, so nothing
        // here can touch the developer's real credential on ANY platform and no
        // two tests share a home directory. The file is deliberately absent:
        // the tick thread's read is a guaranteed miss rather than, on macOS, a
        // Keychain hit on whatever the developer happens to be logged into.
        let home = tempfile::tempdir().unwrap();
        let monitor = CredentialRefreshMonitor::with_binding(
            MonitorConfig::default(),
            CredentialBinding::to_file(
                AuthPathResolver::at_home(home.path()),
                home.path().join(".claude/.credentials.json"),
            ),
        );
        let staged = tempfile::tempdir().unwrap();
        let agent = AgentName::new("claude").unwrap();
        let delivery = RefreshableCredentialDelivery {
            agent: agent.clone(),
            spec_agent: "claude",
            credential_env_key: "CLAUDE_CODE_OAUTH_TOKEN",
            staged_path: staged.path().join(".credentials.json"),
            staged_root: staged.path().to_path_buf(),
            initial_fingerprint: CredentialFingerprint::zeroed(),
        };

        // Seeding is asserted through the `seed_state` call `register` itself
        // makes, BEFORE any monitor thread exists. Asserting it after
        // `register` would race that thread's first tick, which reads the host
        // credential and then legitimately rewrites either the failure counter
        // or `last_materialized`.
        monitor.seed_state(&delivery);
        assert_eq!(
            monitor
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(agent.as_str())
                .and_then(|st| st.last_materialized),
            Some(CredentialFingerprint::zeroed()),
            "register must seed change detection from the delivery's fingerprint"
        );

        // Lease counting has no such race: the tick thread never adds to or
        // removes from the registry.
        let lease = monitor.register(&delivery, "awman-test");
        assert_eq!(monitor.live_lease_count(), 1);
        let status = monitor.status();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].live_leases, 1);
        drop(lease);
        assert_eq!(monitor.live_lease_count(), 0);
    }

    #[test]
    fn backoff_grows_then_resets() {
        let monitor = CredentialRefreshMonitor::new(MonitorConfig {
            refresh_threshold: Duration::from_secs(1200),
            tick_interval: Duration::from_secs(60),
        });
        let agent = AgentName::new("claude").unwrap();
        assert!(!monitor.in_backoff(&agent));
        monitor.record_status(
            &agent,
            None,
            RefreshOutcome::Unavailable {
                reason: CredentialReadError::NotFound,
            },
            true,
        );
        assert!(monitor.in_backoff(&agent));
        // A success clears the gate.
        monitor.record_status(
            &agent,
            None,
            RefreshOutcome::NotNeeded {
                expires_in: Duration::from_secs(3600),
            },
            false,
        );
        assert!(!monitor.in_backoff(&agent));
    }
}
