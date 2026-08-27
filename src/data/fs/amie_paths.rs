//! `AmiePaths` — storage root for the amie daemon.
//!
//! Same shape and env precedence as `ApiPaths::from_env`, rooted at
//! `$HOME/.awman/amie` with an `AWMAN_AMIE_ROOT` override. Exposes a
//! `daemon()` accessor (key stem `amie_key`) and a validated, persistent
//! per-condition context directory.

use std::path::{Path, PathBuf};

use crate::data::config::env::{Env, EnvSnapshot};
use crate::data::error::DataError;
use crate::data::fs::daemon_paths::DaemonPaths;
use crate::data::fs::path_guard::validate_under_root;

/// Subdirectory under the data home that hosts amie state.
pub const AMIE_SUBDIR: &str = "amie";

/// Subdirectory holding per-condition context directories.
const CONDITIONS_SUBDIR: &str = "conditions";

/// Resolves every path under the amie storage root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmiePaths {
    root: PathBuf,
}

impl AmiePaths {
    /// Build `AmiePaths` rooted at an explicit directory.
    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolve from the current process environment.
    pub fn from_process_env() -> Result<Self, DataError> {
        Self::from_env(&Env::from_process())
    }

    /// Resolve from a supplied env snapshot.
    ///
    /// Precedence: `AWMAN_AMIE_ROOT` → `AWMAN_CONFIG_HOME/amie` →
    /// `XDG_DATA_HOME/awman/amie` → `$HOME/.awman/amie`.
    pub fn from_env(env: &EnvSnapshot) -> Result<Self, DataError> {
        if let Some(root) = env.amie_root() {
            return Ok(Self::from_root(root));
        }
        if let Some(home) = env.config_home() {
            return Ok(Self::from_root(home.join(AMIE_SUBDIR)));
        }
        if let Some(xdg) = env.xdg_data_home() {
            return Ok(Self::from_root(xdg.join("awman").join(AMIE_SUBDIR)));
        }
        let home = dirs::home_dir().ok_or(DataError::HomeNotFound)?;
        Ok(Self::from_root(home.join(".awman").join(AMIE_SUBDIR)))
    }

    /// The amie root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Daemon-identity paths for the amie daemon (key stem `amie_key`).
    pub fn daemon(&self) -> DaemonPaths {
        DaemonPaths::new(self.root.clone(), "amie_key")
    }

    /// Directory holding per-condition context directories.
    pub fn conditions_dir(&self) -> PathBuf {
        self.root.join(CONDITIONS_SUBDIR)
    }

    /// The persistent context directory for one condition:
    /// `<root>/conditions/<name>/`. Validated to stay under the conditions
    /// root (a crafted `name` cannot escape via `..`).
    ///
    /// These directories are `context(global)`-style — created once per
    /// condition and never recreated per run.
    pub fn condition_dir(&self, name: &str) -> Result<PathBuf, DataError> {
        let dir = self.conditions_dir().join(name);
        validate_under_root(
            &self.conditions_dir(),
            &dir,
            "condition directory must reside under the amie conditions root",
        )?;
        Ok(dir)
    }

    /// Create the root directory (and parents) on disk.
    pub fn ensure_root(&self) -> Result<(), DataError> {
        std::fs::create_dir_all(&self.root).map_err(|e| DataError::io(&self.root, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::env::{AWMAN_AMIE_ROOT, AWMAN_CONFIG_HOME, XDG_DATA_HOME};

    #[test]
    fn amie_root_override_wins() {
        let env = EnvSnapshot::with_overrides([
            (AWMAN_AMIE_ROOT, "/custom/amie"),
            (XDG_DATA_HOME, "/xdg/data"),
        ]);
        let paths = AmiePaths::from_env(&env).unwrap();
        assert_eq!(paths.root(), Path::new("/custom/amie"));
    }

    #[test]
    fn config_home_produces_amie_subdir() {
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, "/cfg")]);
        let paths = AmiePaths::from_env(&env).unwrap();
        assert_eq!(paths.root(), Path::new("/cfg/amie"));
    }

    #[test]
    fn xdg_data_home_produces_awman_amie() {
        let env = EnvSnapshot::with_overrides([(XDG_DATA_HOME, "/xdg/data")]);
        let paths = AmiePaths::from_env(&env).unwrap();
        assert_eq!(paths.root(), Path::new("/xdg/data/awman/amie"));
    }

    #[test]
    fn daemon_uses_amie_key_stem() {
        let paths = AmiePaths::from_root("/r");
        assert_eq!(paths.daemon().key_stem(), "amie_key");
        assert_eq!(
            paths.daemon().key_hash_file(),
            PathBuf::from("/r/amie_key.hash")
        );
    }

    #[test]
    fn condition_dir_is_under_conditions_root() {
        let paths = AmiePaths::from_root("/r");
        assert_eq!(
            paths.condition_dir("issue-triage").unwrap(),
            PathBuf::from("/r/conditions/issue-triage")
        );
    }

    #[test]
    fn condition_dir_rejects_escape() {
        // `..` is resolved by canonicalization only when the target exists, so
        // materialize the escape target (matching `validate_context_path`).
        let tmp = tempfile::tempdir().unwrap();
        let paths = AmiePaths::from_root(tmp.path());
        std::fs::create_dir_all(paths.conditions_dir()).unwrap();
        std::fs::create_dir_all(tmp.path().join("escape")).unwrap();
        assert!(paths.condition_dir("../escape").is_err());
    }
}
