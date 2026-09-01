//! `SquadPaths` — storage root for the squad daemon.
//!
//! Same shape and env precedence as `ApiPaths::from_env`, rooted at
//! `$HOME/.awman/squad` with an `AWMAN_SQUAD_ROOT` override. Exposes a
//! `daemon()` accessor (key stem `squad_key`) and a validated, persistent
//! per-task context directory.

use std::path::{Path, PathBuf};

use crate::data::config::env::{Env, EnvSnapshot};
use crate::data::error::DataError;
use crate::data::fs::daemon_paths::DaemonPaths;
use crate::data::fs::path_guard::validate_under_root;

/// Subdirectory under the data home that hosts squad state.
pub const SQUAD_SUBDIR: &str = "squad";

/// Subdirectory holding per-task context directories.
const TASKS_SUBDIR: &str = "tasks";

/// Resolves every path under the squad storage root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SquadPaths {
    root: PathBuf,
}

impl SquadPaths {
    /// Build `SquadPaths` rooted at an explicit directory.
    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolve from the current process environment.
    pub fn from_process_env() -> Result<Self, DataError> {
        Self::from_env(&Env::from_process())
    }

    /// Resolve from a supplied env snapshot.
    ///
    /// Precedence: `AWMAN_SQUAD_ROOT` → `AWMAN_CONFIG_HOME/squad` →
    /// `XDG_DATA_HOME/awman/squad` → `$HOME/.awman/squad`.
    pub fn from_env(env: &EnvSnapshot) -> Result<Self, DataError> {
        if let Some(root) = env.squad_root() {
            return Ok(Self::from_root(root));
        }
        if let Some(home) = env.config_home() {
            return Ok(Self::from_root(home.join(SQUAD_SUBDIR)));
        }
        if let Some(xdg) = env.xdg_data_home() {
            return Ok(Self::from_root(xdg.join("awman").join(SQUAD_SUBDIR)));
        }
        let home = dirs::home_dir().ok_or(DataError::HomeNotFound)?;
        Ok(Self::from_root(home.join(".awman").join(SQUAD_SUBDIR)))
    }

    /// The squad root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Daemon-identity paths for the squad daemon (key stem `squad_key`).
    pub fn daemon(&self) -> DaemonPaths {
        DaemonPaths::new(self.root.clone(), "squad_key")
    }

    /// Directory holding per-task context directories.
    pub fn tasks_dir(&self) -> PathBuf {
        self.root.join(TASKS_SUBDIR)
    }

    /// The persistent workspace directory for one task:
    /// `<root>/tasks/<name>/workspace/`. The user-influenced `<name>` component
    /// is validated to stay under the tasks root (a crafted `name` cannot
    /// escape via `..`); the fixed `workspace` leaf is appended afterwards.
    ///
    /// These directories are `context(global)`-style — created once per
    /// task and never recreated per run.
    pub fn task_dir(&self, name: &str) -> Result<PathBuf, DataError> {
        let base = self.tasks_dir().join(name);
        validate_under_root(
            &self.tasks_dir(),
            &base,
            "task directory must reside under the squad tasks root",
        )?;
        Ok(base.join("workspace"))
    }

    /// Create the root directory (and parents) on disk.
    pub fn ensure_root(&self) -> Result<(), DataError> {
        std::fs::create_dir_all(&self.root).map_err(|e| DataError::io(&self.root, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::env::{AWMAN_CONFIG_HOME, AWMAN_SQUAD_ROOT, XDG_DATA_HOME};

    #[test]
    fn squad_root_override_wins() {
        let env = EnvSnapshot::with_overrides([
            (AWMAN_SQUAD_ROOT, "/custom/squad"),
            (XDG_DATA_HOME, "/xdg/data"),
        ]);
        let paths = SquadPaths::from_env(&env).unwrap();
        assert_eq!(paths.root(), Path::new("/custom/squad"));
    }

    #[test]
    fn config_home_produces_squad_subdir() {
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, "/cfg")]);
        let paths = SquadPaths::from_env(&env).unwrap();
        assert_eq!(paths.root(), Path::new("/cfg/squad"));
    }

    #[test]
    fn xdg_data_home_produces_awman_squad() {
        let env = EnvSnapshot::with_overrides([(XDG_DATA_HOME, "/xdg/data")]);
        let paths = SquadPaths::from_env(&env).unwrap();
        assert_eq!(paths.root(), Path::new("/xdg/data/awman/squad"));
    }

    #[test]
    fn daemon_uses_squad_key_stem() {
        let paths = SquadPaths::from_root("/r");
        assert_eq!(paths.daemon().key_stem(), "squad_key");
        assert_eq!(
            paths.daemon().key_hash_file(),
            PathBuf::from("/r/squad_key.hash")
        );
    }

    #[test]
    fn task_dir_is_under_tasks_root() {
        let paths = SquadPaths::from_root("/r");
        assert_eq!(
            paths.task_dir("issue-triage").unwrap(),
            PathBuf::from("/r/tasks/issue-triage/workspace")
        );
    }

    #[test]
    fn task_dir_rejects_escape() {
        // `..` is resolved by canonicalization only when the target exists, so
        // materialize the escape target (matching `validate_context_path`).
        let tmp = tempfile::tempdir().unwrap();
        let paths = SquadPaths::from_root(tmp.path());
        std::fs::create_dir_all(paths.tasks_dir()).unwrap();
        std::fs::create_dir_all(tmp.path().join("escape")).unwrap();
        assert!(paths.task_dir("../escape").is_err());
    }
}
