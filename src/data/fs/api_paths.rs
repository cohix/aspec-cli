//! Typed accessors for API-mode storage paths.
//!
//! Replaces ad-hoc `dirs::data_dir().join("awman/api/...")` calls scattered
//! through `oldsrc/commands/headless/`.

use std::path::{Path, PathBuf};

use crate::data::config::env::{Env, EnvSnapshot};
use crate::data::error::DataError;
use crate::data::fs::daemon_paths::DaemonPaths;
use crate::data::fs::data_paths::DataPaths;
use crate::data::session_setup_event::SessionSetupState;

/// Filename of the API sqlite database.
pub const API_DB_FILENAME: &str = "awman.db";

/// Subdirectory under the global home that hosts API state.
const API_SUBDIR: &str = "api";

/// Subdirectory holding per-session command logs.
const SESSIONS_SUBDIR: &str = "sessions";

/// Subdirectory holding TLS materials.
const TLS_SUBDIR: &str = "tls";

/// Resolves every path under the API storage root.
///
/// `root` owns everything under `~/.awman/api/` (pidfile, log, `sessions/`,
/// key hash, `server.json`, TLS). `data_root` locates the shared database,
/// which lives under `~/.awman/data/` for env-scoped construction and stays
/// root-local for explicit-root construction (so `from_root(R).db_path()` is
/// still `R/awman.db`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiPaths {
    root: PathBuf,
    data_root: PathBuf,
}

impl ApiPaths {
    /// Build an `ApiPaths` rooted at an explicit directory. The database stays
    /// root-local (`R/awman.db`), preserving every `from_root`/`at_root` test
    /// fixture.
    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            data_root: root.clone(),
            root,
        }
    }

    /// Resolve from the current process environment, honouring `AWMAN_API_ROOT`
    /// when set, otherwise falling back to `$HOME/.awman/api`.
    pub fn from_process_env() -> Result<Self, DataError> {
        Self::from_env(&Env::from_process())
    }

    /// Same as [`from_process_env`] but reads from a supplied env snapshot.
    ///
    /// Precedence for the API root: `AWMAN_API_ROOT` → `AWMAN_CONFIG_HOME/api`
    /// → `XDG_DATA_HOME/awman/api` → `$HOME/.awman/api`. The database is
    /// resolved separately via `DataPaths` (`<data_home>/data/awman.db`), so it
    /// relocates in lockstep with every other relocatable path rather than
    /// tracking `AWMAN_API_ROOT`.
    pub fn from_env(env: &EnvSnapshot) -> Result<Self, DataError> {
        let root = if let Some(root) = env.api_root() {
            root
        } else if let Some(home) = env.config_home() {
            home.join(API_SUBDIR)
        } else if let Some(xdg) = env.xdg_data_home() {
            xdg.join("awman").join(API_SUBDIR)
        } else {
            dirs::home_dir()
                .ok_or(DataError::HomeNotFound)?
                .join(".awman")
                .join(API_SUBDIR)
        };
        let data_root = DataPaths::from_env(env)?.root().to_path_buf();
        Ok(Self { root, data_root })
    }

    /// The API root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Daemon-identity paths for the API daemon (key stem `api_key`).
    pub fn daemon(&self) -> DaemonPaths {
        DaemonPaths::new(self.root.clone(), "api_key")
    }

    /// The shared data paths (locating the database).
    pub fn data_paths(&self) -> DataPaths {
        DataPaths::at_root(self.data_root.clone())
    }

    /// Path to the shared sqlite database.
    pub fn db_path(&self) -> PathBuf {
        self.data_root.join(API_DB_FILENAME)
    }

    /// Directory holding per-session subdirectories.
    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join(SESSIONS_SUBDIR)
    }

    /// Directory for a single session's command output.
    pub fn session_dir(&self, session_id: &str) -> PathBuf {
        self.sessions_dir().join(session_id)
    }

    /// Directory for command logs within a session.
    pub fn session_commands_dir(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("commands")
    }

    /// Directory for one command's logs.
    pub fn command_dir(&self, session_id: &str, command_id: &str) -> PathBuf {
        self.session_commands_dir(session_id).join(command_id)
    }

    /// Default log path for a single command run.
    pub fn command_log_path(&self, session_id: &str, command_id: &str) -> PathBuf {
        self.command_dir(session_id, command_id).join("output.log")
    }

    /// NDJSON `ExecutionEvent` log path for a single command/job.
    pub fn command_events_log_path(&self, session_id: &str, command_id: &str) -> PathBuf {
        self.command_dir(session_id, command_id).join("events.log")
    }

    /// TLS material directory.
    pub fn tls_dir(&self) -> PathBuf {
        self.root.join(TLS_SUBDIR)
    }

    /// PEM-encoded TLS certificate.
    pub fn tls_cert_file(&self) -> PathBuf {
        self.tls_dir().join("cert.pem")
    }

    /// PEM-encoded TLS private key (mode 0o600 on Unix).
    pub fn tls_key_file(&self) -> PathBuf {
        self.tls_dir().join("key.pem")
    }

    /// Sidecar file recording the bind IP that the cert was generated for.
    /// Used to detect SAN-mismatch and trigger regeneration safely without
    /// having to parse DER.
    pub fn tls_bind_ip_file(&self) -> PathBuf {
        self.tls_dir().join("bind_ip")
    }

    /// Sidecar file recording the SHA-256 fingerprint of the cert's DER
    /// bytes (hex). Cached at cert-generation time so we never need to
    /// re-parse PEM to recompute it on subsequent loads.
    pub fn tls_fingerprint_file(&self) -> PathBuf {
        self.tls_dir().join("fingerprint.sha256")
    }

    /// API server PID file. Delegates to `DaemonPaths` (`<root>/awman.pid`).
    pub fn pid_file(&self) -> PathBuf {
        self.daemon().pid_file()
    }

    /// Sidecar metadata for the running server (port, scheme). Delegates to
    /// `DaemonPaths` (`<root>/server.json`).
    pub fn server_meta_file(&self) -> PathBuf {
        self.daemon().server_meta_file()
    }

    /// API server log file. Delegates to `DaemonPaths` (`<root>/awman.log`).
    pub fn log_file(&self) -> PathBuf {
        self.daemon().log_file()
    }

    /// API key hash file (mode 0o600 on Unix). Delegates to `DaemonPaths`
    /// (`<root>/api_key.hash`).
    pub fn api_key_hash_file(&self) -> PathBuf {
        self.daemon().key_hash_file()
    }

    /// Workflow state file for a single command run.
    pub fn command_workflow_state_path(&self, session_id: &str, command_id: &str) -> PathBuf {
        self.command_dir(session_id, command_id)
            .join("workflow.state.json")
    }

    /// Metadata file for a single command run.
    pub fn command_metadata_path(&self, session_id: &str, command_id: &str) -> PathBuf {
        self.command_dir(session_id, command_id)
            .join("metadata.json")
    }

    /// Per-session worktree directory.
    pub fn session_worktree_dir(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("worktree")
    }

    /// Per-session agent settings directory.
    pub fn session_agent_settings_dir(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("agent-settings")
    }

    /// Directory for a remote session's cloned repository.
    pub fn remote_session_repo_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("repo")
    }

    /// Directory for a remote session (parent of repo/).
    pub fn remote_session_dir(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id)
    }

    /// On-disk snapshot of a session's async setup progress.
    pub fn session_setup_state_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("setup_state.json")
    }

    /// Alias for `from_root` to match the legacy `at_root` naming.
    pub fn at_root(root: impl Into<PathBuf>) -> Self {
        Self::from_root(root)
    }

    /// Create the root directory (and parents) on disk.
    pub fn ensure_root(&self) -> Result<(), DataError> {
        std::fs::create_dir_all(&self.root).map_err(|e| DataError::io(&self.root, e))
    }

    // ── Session / command storage surface (Layer 0) ──────────────────────────
    //
    // These own every filesystem side-effect the API frontend previously
    // performed inline. Frontends call them so no presentation layer touches
    // `std::fs`/`tokio::fs` directly (grand-architecture Tenet 2).

    /// Create the full per-session directory set idempotently: `jobs/`,
    /// `commands/` (legacy, pre-WI-0079 clients), `worktree/`, and
    /// `agent-settings/`. Creating `jobs/` first materializes the session
    /// directory itself.
    pub fn prepare_session_dirs(&self, session_id: &str) -> Result<(), DataError> {
        let session_dir = self.session_dir(session_id);
        for sub in ["jobs", "commands", "worktree", "agent-settings"] {
            let dir = session_dir.join(sub);
            std::fs::create_dir_all(&dir).map_err(|e| DataError::io(&dir, e))?;
        }
        Ok(())
    }

    /// Create the output directory for a single command run.
    pub fn prepare_command_dir(&self, session_id: &str, command_id: &str) -> Result<(), DataError> {
        let dir = self.command_dir(session_id, command_id);
        std::fs::create_dir_all(&dir).map_err(|e| DataError::io(&dir, e))
    }

    /// Persist a session's setup-progress snapshot to `setup_state.json`,
    /// creating the session directory if needed (idempotent).
    pub fn save_setup_state(
        &self,
        session_id: &str,
        state: &SessionSetupState,
    ) -> Result<(), DataError> {
        let session_dir = self.session_dir(session_id);
        std::fs::create_dir_all(&session_dir).map_err(|e| DataError::io(&session_dir, e))?;
        let path = self.session_setup_state_path(session_id);
        let json = serde_json::to_string_pretty(state)
            .map_err(|source| DataError::ConfigSerialize { source })?;
        std::fs::write(&path, json).map_err(|e| DataError::io(&path, e))
    }

    /// Read and parse a session's `setup_state.json`. Returns `None` when the
    /// file is absent or unparseable (callers fall back to the DB row).
    pub fn read_setup_state(&self, session_id: &str) -> Option<SessionSetupState> {
        let path = self.session_setup_state_path(session_id);
        let content = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str::<SessionSetupState>(&content).ok()
    }

    /// Read a command's `events.log` as raw NDJSON text. Returns `None` when
    /// the file is absent or unreadable (equivalent to an empty log).
    pub fn read_command_events_raw(&self, session_id: &str, command_id: &str) -> Option<String> {
        let path = self.command_events_log_path(session_id, command_id);
        std::fs::read_to_string(&path).ok()
    }

    /// Write a command's `metadata.json` (pretty-printed).
    pub fn write_command_metadata(
        &self,
        session_id: &str,
        command_id: &str,
        metadata: &serde_json::Value,
    ) -> Result<(), DataError> {
        let path = self.command_metadata_path(session_id, command_id);
        let json = serde_json::to_string_pretty(metadata)
            .map_err(|source| DataError::ConfigSerialize { source })?;
        std::fs::write(&path, json).map_err(|e| DataError::io(&path, e))
    }

    /// Read a command's persisted workflow state as raw JSON text. Returns
    /// `Ok(None)` when no workflow state exists (the file is absent) and
    /// `Err` for any other IO failure, so callers can distinguish "no
    /// workflow" from "read failed".
    pub fn read_command_workflow_state_raw(
        &self,
        session_id: &str,
        command_id: &str,
    ) -> Result<Option<String>, DataError> {
        let path = self.command_workflow_state_path(session_id, command_id);
        match std::fs::read_to_string(&path) {
            Ok(content) => Ok(Some(content)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(DataError::io(&path, e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::env::{EnvSnapshot, AWMAN_API_ROOT, AWMAN_CONFIG_HOME, XDG_DATA_HOME};

    #[test]
    fn from_env_returns_xdg_data_home_awman_api_when_xdg_set() {
        let env = EnvSnapshot::with_overrides([(XDG_DATA_HOME, "/xdg/data")]);
        let paths = ApiPaths::from_env(&env).unwrap();
        assert_eq!(paths.root(), std::path::Path::new("/xdg/data/awman/api"));
    }

    #[test]
    fn from_env_awman_api_root_wins_over_xdg_data_home() {
        let env = EnvSnapshot::with_overrides([
            (AWMAN_API_ROOT, "/custom/api"),
            (XDG_DATA_HOME, "/xdg/data"),
        ]);
        let paths = ApiPaths::from_env(&env).unwrap();
        assert_eq!(paths.root(), std::path::Path::new("/custom/api"));
    }

    #[test]
    fn from_env_falls_back_to_home_awman_api() {
        // No overrides — must fall back to $HOME/.awman/api.
        let env = EnvSnapshot::empty();
        let paths = ApiPaths::from_env(&env).unwrap();
        let root = paths.root();
        assert!(
            root.ends_with(".awman/api"),
            "fallback root must end with .awman/api; got: {root:?}"
        );
    }

    #[test]
    fn from_env_awman_config_home_produces_api_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let paths = ApiPaths::from_env(&env).unwrap();
        assert_eq!(paths.root(), tmp.path().join("api"));
    }

    // ── Layer 0 storage methods (WI-0097 Finding C) ──────────────────────────

    use crate::data::session_setup_event::{SessionSetupState, SessionSetupStatus};

    #[test]
    fn save_setup_state_round_trips_json() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = ApiPaths::from_root(tmp.path());

        let mut state = SessionSetupState::new();
        state.status = SessionSetupStatus::RunningReady;
        state.current_stage = Some("cloning".to_string());

        paths.save_setup_state("sess-1", &state).expect("save");

        // The file lands at the documented path.
        assert!(paths.session_setup_state_path("sess-1").exists());

        let read = paths.read_setup_state("sess-1").expect("read back state");
        assert_eq!(read.status, SessionSetupStatus::RunningReady);
        assert_eq!(read.current_stage.as_deref(), Some("cloning"));
    }

    #[test]
    fn save_setup_state_creates_session_dir_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = ApiPaths::from_root(tmp.path());
        // No prior prepare_session_dirs — save must create the dir itself.
        assert!(!paths.session_dir("fresh").exists());

        paths
            .save_setup_state("fresh", &SessionSetupState::new())
            .expect("save into non-existent session dir");
        assert!(paths.session_setup_state_path("fresh").exists());
    }

    #[test]
    fn read_setup_state_absent_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = ApiPaths::from_root(tmp.path());
        assert!(paths.read_setup_state("nope").is_none());
    }

    #[test]
    fn prepare_session_dirs_creates_full_set_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = ApiPaths::from_root(tmp.path());

        // First call materializes the session dir and every subdir.
        paths.prepare_session_dirs("s").expect("first prepare");
        let session_dir = paths.session_dir("s");
        for sub in ["jobs", "commands", "worktree", "agent-settings"] {
            assert!(
                session_dir.join(sub).is_dir(),
                "prepare_session_dirs must create {sub}/"
            );
        }
        assert_eq!(
            paths.session_worktree_dir("s"),
            session_dir.join("worktree")
        );
        assert_eq!(
            paths.session_agent_settings_dir("s"),
            session_dir.join("agent-settings")
        );

        // Second call on the existing tree must succeed (idempotent).
        paths
            .prepare_session_dirs("s")
            .expect("prepare_session_dirs must be idempotent");
        for sub in ["jobs", "commands", "worktree", "agent-settings"] {
            assert!(session_dir.join(sub).is_dir());
        }
    }

    #[test]
    fn prepare_command_dir_creates_command_output_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = ApiPaths::from_root(tmp.path());

        paths
            .prepare_command_dir("s", "c")
            .expect("prepare command dir");
        assert!(paths.command_dir("s", "c").is_dir());
        // Idempotent.
        paths.prepare_command_dir("s", "c").expect("idempotent");
    }
}
