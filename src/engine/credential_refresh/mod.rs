//! Live-session credential tracking: the lease registry and the refresh
//! monitor (WI-0107 §3).
//!
//! A [`CredentialLease`] is the RAII proof that a credentialed container is
//! live; the [`CredentialRefreshMonitor`] owns the [`LeaseRegistry`] and keeps
//! every live container's staged credential file fresh without ever writing to
//! a dead session's path. See `lease.rs` and `monitor.rs` for the invariant
//! walkthroughs.

pub mod lease;
pub mod monitor;

pub use lease::{CredentialLease, LeaseGeneration, LeaseRegistry, LeaseSnapshot};
pub use monitor::{
    global, install_global, CredentialRefreshMonitor, MonitorConfig, RefreshOutcome, RefreshStatus,
};

use crate::engine::container::options::ResolvedContainerOptions;

/// Take a [`CredentialLease`] for every file-delivered credential this launch
/// carries. Called from the container backends' `build()` — the single spawn
/// choke point — so no per-command frontend ever registers a lease.
///
/// Returns an empty vec when no monitor is installed (the `authRefresh.enabled:
/// false` kill switch) or when the launch carries no file-delivered credential
/// — which is every launch today. A non-empty
/// [`ResolvedContainerOptions::refreshable_credentials`] with a monitor
/// installed MUST yield a non-empty lease vec before the child is spawned
/// (INV-6); the backends assert this.
pub(crate) fn register_container_leases(
    options: &ResolvedContainerOptions,
    container: &str,
) -> Vec<CredentialLease> {
    if options.refreshable_credentials.is_empty() {
        return Vec::new();
    }
    match global() {
        Some(monitor) => options
            .refreshable_credentials
            .iter()
            .map(|delivery| monitor.register(delivery, container))
            .collect(),
        None => Vec::new(),
    }
}
