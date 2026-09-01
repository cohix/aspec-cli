//! `DaemonPaths` — the daemon-identity path accessors shared by the API server
//! and the squad daemon.
//!
//! `ApiPaths` previously hardcoded the four daemon-identity filenames
//! (`awman.pid`, `awman.log`, `server.json`, `api_key.hash`) inside its own
//! accessors. Those filenames are identical for every daemon except the key
//! stem (`api_key` vs `squad_key`), so they factor into this value object over a
//! `root` plus a `key_stem`. A second daemon rooted elsewhere is isolated by
//! construction: every accessor keys off `root`.

use std::path::{Path, PathBuf};

use crate::data::error::DataError;

/// Path accessors for one daemon's identity files, all under a single `root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonPaths {
    root: PathBuf,
    key_stem: &'static str,
}

impl DaemonPaths {
    /// Construct over a root and the API-key filename stem (`"api_key"` /
    /// `"squad_key"`).
    pub fn new(root: impl Into<PathBuf>, key_stem: &'static str) -> Self {
        Self {
            root: root.into(),
            key_stem,
        }
    }

    /// The daemon root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The key-hash filename stem (`"api_key"` / `"squad_key"`).
    pub fn key_stem(&self) -> &'static str {
        self.key_stem
    }

    /// PID file: `<root>/awman.pid`.
    pub fn pid_file(&self) -> PathBuf {
        self.root.join("awman.pid")
    }

    /// Log file: `<root>/awman.log`.
    pub fn log_file(&self) -> PathBuf {
        self.root.join("awman.log")
    }

    /// Server metadata sidecar: `<root>/server.json`.
    pub fn server_meta_file(&self) -> PathBuf {
        self.root.join("server.json")
    }

    /// API-key hash file: `<root>/<key_stem>.hash`.
    pub fn key_hash_file(&self) -> PathBuf {
        self.root.join(format!("{}.hash", self.key_stem))
    }

    /// Create the root directory (and parents) on disk.
    pub fn ensure_root(&self) -> Result<(), DataError> {
        std::fs::create_dir_all(&self.root).map_err(|e| DataError::io(&self.root, e))
    }

    /// Read the persisted key hash, trimmed, or `None` when absent.
    ///
    /// Reproduces `AuthEngine::read_api_key_hash` so Layer 3 / the auth engine
    /// never touch `std::fs` for this file directly.
    pub fn read_key_hash(&self) -> Result<Option<String>, DataError> {
        let path = self.key_hash_file();
        match std::fs::read_to_string(&path) {
            Ok(s) => Ok(Some(s.trim().to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(DataError::io(path, e)),
        }
    }

    /// Persist the key hash (hex) with mode `0o600` on Unix, creating the root
    /// directory if needed. Reproduces `AuthEngine::write_api_key_hash`
    /// byte-for-byte, including the secure-write permissions.
    pub fn write_key_hash(&self, hex: &str) -> Result<(), DataError> {
        let path = self.key_hash_file();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
        }
        write_file_secure(&path, hex.as_bytes())
    }
}

/// Write `content` to `path`, creating it with mode `0o600` on Unix and
/// truncating an existing file. Mirrors `engine::auth::write_file_secure`.
fn write_file_secure(path: &Path, content: &[u8]) -> Result<(), DataError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| DataError::io(path, e))?;
        std::io::Write::write_all(&mut f, content).map_err(|e| DataError::io(path, e))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, content).map_err(|e| DataError::io(path, e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames_are_stable_for_api_stem() {
        let p = DaemonPaths::new("/r", "api_key");
        assert_eq!(p.pid_file(), PathBuf::from("/r/awman.pid"));
        assert_eq!(p.log_file(), PathBuf::from("/r/awman.log"));
        assert_eq!(p.server_meta_file(), PathBuf::from("/r/server.json"));
        assert_eq!(p.key_hash_file(), PathBuf::from("/r/api_key.hash"));
    }

    #[test]
    fn key_stem_varies_the_hash_filename() {
        assert_eq!(
            DaemonPaths::new("/r", "squad_key").key_hash_file(),
            PathBuf::from("/r/squad_key.hash")
        );
    }

    #[test]
    fn key_hash_round_trips_and_trims() {
        let tmp = tempfile::tempdir().unwrap();
        let p = DaemonPaths::new(tmp.path(), "api_key");
        assert_eq!(p.read_key_hash().unwrap(), None);
        p.write_key_hash("deadbeef\n").unwrap();
        assert_eq!(p.read_key_hash().unwrap(), Some("deadbeef".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn key_hash_file_is_mode_600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let p = DaemonPaths::new(tmp.path(), "api_key");
        p.write_key_hash("abc123").unwrap();
        let mode = std::fs::metadata(p.key_hash_file())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
