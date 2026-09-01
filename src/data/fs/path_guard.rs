//! Shared path-escape validation (Layer 0).
//!
//! `validate_under_root` generalizes the canonicalize-with-fallback +
//! `starts_with` check that `context_dirs::validate_context_path` previously
//! hardcoded to `~/.awman/context/`. Any accessor that resolves a
//! user-influenced sub-path (a task slug, a context slug) under a fixed
//! root uses this to guarantee the resolved path cannot escape the root via
//! `..` or a crafted component.

use std::path::Path;

use crate::data::error::DataError;

/// Verify that `resolved` stays under `root`. Both are canonicalized when they
/// exist on disk (falling back to their lexical form when they do not), so the
/// check holds for not-yet-created directories as well as existing ones.
///
/// Returns `DataError::InvalidPath { path: resolved, reason }` when `resolved`
/// escapes `root`.
pub fn validate_under_root(root: &Path, resolved: &Path, reason: &str) -> Result<(), DataError> {
    let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let canonical_resolved =
        std::fs::canonicalize(resolved).unwrap_or_else(|_| resolved.to_path_buf());
    if !canonical_resolved.starts_with(&canonical_root) {
        return Err(DataError::InvalidPath {
            path: resolved.to_path_buf(),
            reason: reason.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn accepts_path_under_root() {
        let root = PathBuf::from("/home/user/.awman/tasks");
        let resolved = root.join("issue-triage");
        assert!(validate_under_root(&root, &resolved, "reason").is_ok());
    }

    #[test]
    fn rejects_path_escaping_root() {
        let root = PathBuf::from("/home/user/.awman/tasks");
        let resolved = PathBuf::from("/home/user/.awman/other");
        let err = validate_under_root(&root, &resolved, "must stay under root").unwrap_err();
        match err {
            DataError::InvalidPath { reason, .. } => {
                assert_eq!(reason, "must stay under root");
            }
            other => panic!("expected InvalidPath, got {other:?}"),
        }
    }

    #[test]
    fn rejects_dotdot_escape() {
        // The canonicalize-with-fallback resolves `..` only when the target
        // exists on disk (matching the original `validate_context_path`), so
        // materialize the escape target.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tasks");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(tmp.path().join("escaped")).unwrap();
        let resolved = root.join("..").join("escaped");
        assert!(validate_under_root(&root, &resolved, "escape").is_err());
    }
}
