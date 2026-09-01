//! Shared path-escape validation (Layer 0).
//!
//! `validate_under_root` generalizes the canonicalize-with-fallback +
//! `starts_with` check that `context_dirs::validate_context_path` previously
//! hardcoded to `~/.awman/context/`. Any accessor that resolves a
//! user-influenced sub-path (a task slug, a context slug) under a fixed
//! root uses this to guarantee the resolved path cannot escape the root via
//! `..` or a crafted component.

use std::path::{Component, Path, PathBuf};

use crate::data::error::DataError;

/// Verify that `resolved` stays under `root`. Both are resolved with
/// [`canonicalize_lenient`], so the check holds for not-yet-created
/// directories as well as existing ones.
///
/// Returns `DataError::InvalidPath { path: resolved, reason }` when `resolved`
/// escapes `root`.
pub fn validate_under_root(root: &Path, resolved: &Path, reason: &str) -> Result<(), DataError> {
    let canonical_root = canonicalize_lenient(root);
    let canonical_resolved = canonicalize_lenient(resolved);
    if !canonical_resolved.starts_with(&canonical_root) {
        return Err(DataError::InvalidPath {
            path: resolved.to_path_buf(),
            reason: reason.to_string(),
        });
    }
    Ok(())
}

/// Canonicalize as much of `path` as exists on disk.
///
/// `std::fs::canonicalize` fails outright when the leaf does not exist yet, and
/// the previous fallback — treating the whole path lexically — compared a
/// symlink-resolved root against an unresolved child. Wherever the root sits
/// behind a symlink (macOS resolves `/var/folders/...` to
/// `/private/var/folders/...`) that made a perfectly contained path look like
/// an escape.
///
/// So canonicalize the deepest existing ancestor, then re-apply the trailing
/// components lexically. Those components do not exist, so none of them can be
/// a symlink and resolving them lexically cannot hide a traversal — a trailing
/// `..` pops a directory exactly as the kernel would.
fn canonicalize_lenient(path: &Path) -> PathBuf {
    // Walk up to the deepest existing ancestor, collecting what we skip past.
    let mut tail: Vec<Component<'_>> = Vec::new();
    let mut cursor = path;
    let base = loop {
        if let Ok(canonical) = std::fs::canonicalize(cursor) {
            break canonical;
        }
        let mut components = cursor.components();
        let Some(last) = components.next_back() else {
            // Nothing exists anywhere along the path, so keep it lexical.
            break PathBuf::new();
        };
        tail.push(last);
        cursor = components.as_path();
    };

    let mut resolved = base;
    for component in tail.into_iter().rev() {
        match component {
            Component::CurDir => {}
            // `pop` at the root is a no-op, matching the kernel's `/.. == /`.
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other.as_os_str()),
        }
    }
    resolved
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

    #[cfg(unix)]
    #[test]
    fn accepts_a_not_yet_created_child_of_a_symlinked_root() {
        // Every macOS temp dir is reached through a symlink
        // (`/var/folders/...` -> `/private/var/folders/...`), so a root that
        // exists and a child that does not must still resolve to the same
        // prefix. Reproduced here with an explicit symlink so the guarantee is
        // pinned on every platform, not just the one that exposed it.
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir_all(real.join("tasks")).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let root = link.join("tasks");
        let resolved = root.join("not-created-yet");
        assert!(validate_under_root(&root, &resolved, "containment").is_ok());

        // The symlink must not become an escape hatch either.
        let escape = root.join("..").join("..").join("elsewhere");
        assert!(validate_under_root(&root, &escape, "containment").is_err());
    }
}
