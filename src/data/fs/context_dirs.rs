//! Typed access to context overlay directories.
//!
//! Layer 0: resolves host-side context directory paths and ensures they exist.
//! All paths live under `~/.awman/context/` — see WI-0087 Security
//! Reconciliation for the rationale.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::data::config::env::{Env, EnvSnapshot};
use crate::data::config::global::GlobalConfig;
use crate::data::error::DataError;
use crate::data::fs::path_guard::validate_under_root;

/// Resolves host-side context directory paths.
#[derive(Debug, Clone)]
pub struct ContextDirResolver {
    awman_home: PathBuf,
}

impl ContextDirResolver {
    /// Construct from the current process environment.
    pub fn from_process_env() -> Result<Self, DataError> {
        Self::from_env(&Env::from_process())
    }

    /// Construct from a supplied env snapshot.
    pub fn from_env(env: &EnvSnapshot) -> Result<Self, DataError> {
        let awman_home = GlobalConfig::data_home_with(env)?;
        Ok(Self { awman_home })
    }

    /// Construct with an explicit awman home (for testing).
    pub fn at_home(awman_home: impl Into<PathBuf>) -> Self {
        Self {
            awman_home: awman_home.into(),
        }
    }

    /// The resolved awman home directory (`~/.awman` unless overridden).
    /// Callers need this to invoke [`validate_context_path`] against a
    /// resolved context path.
    pub fn awman_home(&self) -> &Path {
        &self.awman_home
    }

    /// `~/.awman/context/global/`
    pub fn global_dir(&self) -> PathBuf {
        self.awman_home.join("context").join("global")
    }

    /// `~/.awman/context/repo/{owner}/{repo}/`
    ///
    /// Derived from `git remote get-url origin` at `git_root`. Falls back to
    /// `_local/{dirname}` when no remote is configured. Always normalised to
    /// lowercase with non-alphanumeric chars replaced by dashes.
    pub fn repo_dir(&self, git_root: &Path) -> PathBuf {
        let slug = repo_slug(git_root);
        self.awman_home.join("context").join("repo").join(slug)
    }

    /// `~/.awman/context/workflows/{invocation_uuid}/`
    pub fn workflow_dir(&self, invocation_uuid: uuid::Uuid) -> PathBuf {
        self.awman_home
            .join("context")
            .join("workflows")
            .join(invocation_uuid.to_string())
    }

    /// Create the directory if it does not exist. Idempotent.
    pub fn ensure_exists(path: &Path) -> Result<(), DataError> {
        std::fs::create_dir_all(path).map_err(|e| DataError::io(path, e))
    }
}

/// Derive `{owner}/{repo}` slug from the git remote URL at `git_root`.
/// Falls back to `_local/{dirname}` when no remote is configured.
fn repo_slug(git_root: &Path) -> String {
    if let Some(slug) = slug_from_remote(git_root) {
        return slug;
    }
    let dirname = git_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    format!("_local/{}", normalise_slug(dirname))
}

/// Try to extract `{owner}/{repo}` from `git remote get-url origin`.
fn slug_from_remote(git_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(git_root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    parse_owner_repo(&url)
}

/// Extract and normalise owner/repo from git remote URL formats.
fn parse_owner_repo(remote_url: &str) -> Option<String> {
    let remote = remote_url.trim();

    // SSH: git@host:owner/repo.git
    if let Some(colon_idx) = remote.find(':') {
        if remote[..colon_idx].contains('@') {
            let rest = &remote[colon_idx + 1..];
            let rest = rest.strip_suffix(".git").unwrap_or(rest);
            let parts: Vec<&str> = rest.splitn(2, '/').collect();
            if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
                return Some(format!(
                    "{}/{}",
                    normalise_slug(parts[0]),
                    normalise_slug(parts[1])
                ));
            }
        }
    }

    // HTTPS: https://host/owner/repo.git
    if let Some(idx) = remote.find("://") {
        let after_scheme = &remote[idx + 3..];
        // Skip the hostname
        if let Some(path_start) = after_scheme.find('/') {
            let path = &after_scheme[path_start + 1..];
            let path = path.strip_suffix(".git").unwrap_or(path);
            let parts: Vec<&str> = path.splitn(3, '/').collect();
            if parts.len() >= 2 && !parts[0].is_empty() && !parts[1].is_empty() {
                return Some(format!(
                    "{}/{}",
                    normalise_slug(parts[0]),
                    normalise_slug(parts[1])
                ));
            }
        }
    }

    None
}

/// Normalise a slug component: lowercase, non-alphanumeric chars replaced by dashes.
fn normalise_slug(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// Validate that a resolved context path stays under `~/.awman/context/`.
/// Returns `Err` if the path escapes (e.g. via `..` in a crafted slug).
pub fn validate_context_path(awman_home: &Path, resolved: &Path) -> Result<(), DataError> {
    validate_under_root(
        &awman_home.join("context"),
        resolved,
        "context directory must reside under ~/.awman/context/",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_dir_is_under_context() {
        let resolver = ContextDirResolver::at_home("/home/user/.awman");
        assert_eq!(
            resolver.global_dir(),
            PathBuf::from("/home/user/.awman/context/global")
        );
    }

    #[test]
    fn workflow_dir_uses_uuid() {
        let resolver = ContextDirResolver::at_home("/home/user/.awman");
        let uuid = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        assert_eq!(
            resolver.workflow_dir(uuid),
            PathBuf::from(
                "/home/user/.awman/context/workflows/550e8400-e29b-41d4-a716-446655440000"
            )
        );
    }

    #[test]
    fn two_different_uuids_yield_different_dirs() {
        let resolver = ContextDirResolver::at_home("/tmp/.awman");
        let u1 = uuid::Uuid::new_v4();
        let u2 = uuid::Uuid::new_v4();
        assert_ne!(resolver.workflow_dir(u1), resolver.workflow_dir(u2));
    }

    #[test]
    fn parse_owner_repo_https_github() {
        let result = parse_owner_repo("https://github.com/org/repo.git");
        assert_eq!(result, Some("org/repo".to_string()));
    }

    #[test]
    fn parse_owner_repo_ssh_github() {
        let result = parse_owner_repo("git@github.com:org/repo.git");
        assert_eq!(result, Some("org/repo".to_string()));
    }

    #[test]
    fn parse_owner_repo_https_no_git_suffix() {
        let result = parse_owner_repo("https://github.com/org/repo");
        assert_eq!(result, Some("org/repo".to_string()));
    }

    #[test]
    fn parse_owner_repo_normalises_case_and_special_chars() {
        let result = parse_owner_repo("https://github.com/My.Org/My_Repo.git");
        assert_eq!(result, Some("my-org/my-repo".to_string()));
    }

    #[test]
    fn normalise_slug_replaces_special_chars() {
        assert_eq!(normalise_slug("My.Repo_Name"), "my-repo-name");
    }

    #[test]
    fn normalise_slug_lowercases() {
        assert_eq!(normalise_slug("UpperCase"), "uppercase");
    }

    #[test]
    fn repo_slug_falls_back_to_local_dirname() {
        let tmp = tempfile::tempdir().unwrap();
        // No git repo, so slug_from_remote will fail
        let slug = repo_slug(tmp.path());
        let dirname = tmp.path().file_name().unwrap().to_str().unwrap();
        assert!(
            slug.starts_with("_local/"),
            "must fall back to _local/; got: {slug}"
        );
        assert_eq!(slug, format!("_local/{}", normalise_slug(dirname)));
    }

    #[test]
    fn ensure_exists_creates_nested_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a").join("b").join("c");
        assert!(!nested.exists());
        ContextDirResolver::ensure_exists(&nested).unwrap();
        assert!(nested.exists());
    }

    #[test]
    fn repo_dir_returns_path_under_context_repo() {
        let resolver = ContextDirResolver::at_home("/home/user/.awman");
        let tmp = tempfile::tempdir().unwrap();
        let dir = resolver.repo_dir(tmp.path());
        assert!(
            dir.starts_with("/home/user/.awman/context/repo"),
            "repo_dir must be under context/repo; got: {}",
            dir.display()
        );
    }
}
