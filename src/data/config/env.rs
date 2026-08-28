//! Typed reads of every environment variable awman honours.
//!
//! Reads are funnelled through `Env` so that no scattered `std::env::var(…)`
//! calls leak elsewhere in the data layer.

use std::collections::HashMap;
use std::path::PathBuf;

/// `AWMAN_CONFIG_HOME` — overrides the global config home directory.
pub const AWMAN_CONFIG_HOME: &str = "AWMAN_CONFIG_HOME";

/// `AWMAN_API_ROOT` — overrides the API storage root directory.
pub const AWMAN_API_ROOT: &str = "AWMAN_API_ROOT";

/// `AWMAN_AMIE_ROOT` — overrides the amie storage root directory.
pub const AWMAN_AMIE_ROOT: &str = "AWMAN_AMIE_ROOT";

/// `AWMAN_OVERLAYS` — comma-separated list of overlay specs.
pub const AWMAN_OVERLAYS: &str = "AWMAN_OVERLAYS";

/// `AWMAN_REMOTE_ADDR` — overrides remote server address.
pub const AWMAN_REMOTE_ADDR: &str = "AWMAN_REMOTE_ADDR";

/// `AWMAN_REMOTE_SESSION` — sticky session id for remote operations.
pub const AWMAN_REMOTE_SESSION: &str = "AWMAN_REMOTE_SESSION";

/// `AWMAN_API_KEY` — API key for the remote API server.
pub const AWMAN_API_KEY: &str = "AWMAN_API_KEY";

/// `AWMAN_MAX_CONCURRENT_AGENTS` — overrides the max-concurrent-agents cap
/// for workflow execution.
pub const AWMAN_MAX_CONCURRENT_AGENTS: &str = "AWMAN_MAX_CONCURRENT_AGENTS";

/// `XDG_CONFIG_HOME` — XDG base directory for user-specific configuration.
pub const XDG_CONFIG_HOME: &str = "XDG_CONFIG_HOME";

/// `XDG_DATA_HOME` — XDG base directory for user-specific data files.
pub const XDG_DATA_HOME: &str = "XDG_DATA_HOME";

/// Frozen snapshot of every env var awman reads.
///
/// `EnvSnapshot::from_process()` captures the current process's environment
/// once. Tests construct snapshots directly via `EnvSnapshot::default()` or
/// `EnvSnapshot::with_overrides(…)`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EnvSnapshot {
    values: HashMap<String, String>,
}

impl EnvSnapshot {
    /// Construct an empty snapshot.
    pub fn empty() -> Self {
        Self {
            values: HashMap::new(),
        }
    }

    /// Build a snapshot from a list of `(key, value)` pairs. Useful in tests.
    pub fn with_overrides<I, K, V>(entries: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut values = HashMap::new();
        for (k, v) in entries {
            values.insert(k.into(), v.into());
        }
        Self { values }
    }

    /// Return the raw value of a single var, if set.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(|s| s.as_str())
    }

    /// `AWMAN_CONFIG_HOME` as a `PathBuf` if set.
    pub fn config_home(&self) -> Option<PathBuf> {
        self.get(AWMAN_CONFIG_HOME).map(PathBuf::from)
    }

    /// `AWMAN_API_ROOT` as a `PathBuf` if set.
    pub fn api_root(&self) -> Option<PathBuf> {
        self.get(AWMAN_API_ROOT).map(PathBuf::from)
    }

    /// `AWMAN_AMIE_ROOT` as a `PathBuf` if set.
    pub fn amie_root(&self) -> Option<PathBuf> {
        self.get(AWMAN_AMIE_ROOT).map(PathBuf::from)
    }

    /// `AWMAN_OVERLAYS` raw string if set.
    pub fn overlays(&self) -> Option<&str> {
        self.get(AWMAN_OVERLAYS)
    }

    /// `AWMAN_REMOTE_ADDR` if set.
    pub fn remote_addr(&self) -> Option<&str> {
        self.get(AWMAN_REMOTE_ADDR)
    }

    /// `AWMAN_REMOTE_SESSION` if set.
    pub fn remote_session(&self) -> Option<&str> {
        self.get(AWMAN_REMOTE_SESSION)
    }

    /// `AWMAN_API_KEY` if set.
    pub fn api_key(&self) -> Option<&str> {
        self.get(AWMAN_API_KEY)
    }

    /// `AWMAN_MAX_CONCURRENT_AGENTS` parsed as a `usize`, if set and valid.
    pub fn max_concurrent_agents(&self) -> Option<usize> {
        self.get(AWMAN_MAX_CONCURRENT_AGENTS)?.parse().ok()
    }

    /// `XDG_CONFIG_HOME` as a `PathBuf` if set and non-empty.
    pub fn xdg_config_home(&self) -> Option<PathBuf> {
        self.get(XDG_CONFIG_HOME)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    }

    /// `XDG_DATA_HOME` as a `PathBuf` if set and non-empty.
    pub fn xdg_data_home(&self) -> Option<PathBuf> {
        self.get(XDG_DATA_HOME)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    }
}

/// Namespace for capturing process-environment snapshots.
pub struct Env;

impl Env {
    /// Capture every awman-relevant env var from the current process.
    ///
    /// Reads are limited to the known constants above so that the snapshot
    /// is deterministic and minimal.
    pub fn from_process() -> EnvSnapshot {
        let keys = [
            AWMAN_CONFIG_HOME,
            AWMAN_API_ROOT,
            AWMAN_AMIE_ROOT,
            AWMAN_OVERLAYS,
            AWMAN_REMOTE_ADDR,
            AWMAN_REMOTE_SESSION,
            AWMAN_API_KEY,
            AWMAN_MAX_CONCURRENT_AGENTS,
            XDG_CONFIG_HOME,
            XDG_DATA_HOME,
        ];
        let mut values = HashMap::new();
        for k in keys {
            if let Ok(v) = std::env::var(k) {
                values.insert(k.to_string(), v);
            }
        }
        EnvSnapshot { values }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_config_home_returns_none_when_absent() {
        let snap = EnvSnapshot::empty();
        assert!(snap.xdg_config_home().is_none());
    }

    #[test]
    fn xdg_config_home_returns_none_when_empty_string() {
        let snap = EnvSnapshot::with_overrides([(XDG_CONFIG_HOME, "")]);
        assert!(snap.xdg_config_home().is_none());
    }

    #[test]
    fn xdg_data_home_returns_none_when_absent() {
        let snap = EnvSnapshot::empty();
        assert!(snap.xdg_data_home().is_none());
    }

    #[test]
    fn xdg_data_home_returns_none_when_empty_string() {
        let snap = EnvSnapshot::with_overrides([(XDG_DATA_HOME, "")]);
        assert!(snap.xdg_data_home().is_none());
    }

    #[test]
    fn env_from_process_captures_xdg_config_and_data_home() {
        let cfg_val = "/tmp/awman-test-xdg-cfg-0086";
        let data_val = "/tmp/awman-test-xdg-data-0086";
        std::env::set_var(XDG_CONFIG_HOME, cfg_val);
        std::env::set_var(XDG_DATA_HOME, data_val);
        let snap = Env::from_process();
        std::env::remove_var(XDG_CONFIG_HOME);
        std::env::remove_var(XDG_DATA_HOME);
        assert_eq!(
            snap.xdg_config_home(),
            Some(PathBuf::from(cfg_val)),
            "from_process() must capture XDG_CONFIG_HOME"
        );
        assert_eq!(
            snap.xdg_data_home(),
            Some(PathBuf::from(data_val)),
            "from_process() must capture XDG_DATA_HOME"
        );
    }
}
