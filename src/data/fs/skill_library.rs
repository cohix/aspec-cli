//! Metadata and GitHub-slug parsing for pulled skill libraries.
//!
//! A "skill library" is a git clone of a published skills repository (e.g.
//! `github.com/obra/superpowers`) living under
//! `SkillDirs::library_dir(slug)`. This module owns the pure data shape of
//! that clone's persisted metadata file and the parsing of the GitHub
//! owner/repo slugs used to pull one. It has no git/engine/command
//! dependencies, matching the Layer 0 boundary of `src/data/fs/`.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::data::error::DataError;
use crate::data::session::AgentName;

/// Filename of the persisted metadata file inside a pulled library directory.
pub const LIBRARY_META_FILENAME: &str = ".awman.json";

/// Persisted metadata describing where a pulled skill library came from and
/// which subdirectory inside it holds skills.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillLibraryMeta {
    pub source: String,
    pub owner: String,
    pub repo: String,
    #[serde(default = "default_subdir")]
    pub subdir: String,
}

fn default_subdir() -> String {
    "skills".to_string()
}

/// Read a library's persisted metadata from `<library_dir>/.awman.json`.
///
/// A missing file surfaces as `DataError::Io`, not a default value — callers
/// that want "not pulled yet" semantics must check for that themselves.
pub fn read_library_meta(library_dir: &Path) -> Result<SkillLibraryMeta, DataError> {
    let path = library_dir.join(LIBRARY_META_FILENAME);
    let content = std::fs::read_to_string(&path).map_err(|e| DataError::io(&path, e))?;
    serde_json::from_str(&content).map_err(|e| DataError::config_parse(&path, e))
}

/// Write a library's metadata to `<library_dir>/.awman.json`, creating the
/// library directory if it does not already exist.
pub fn write_library_meta(library_dir: &Path, meta: &SkillLibraryMeta) -> Result<(), DataError> {
    std::fs::create_dir_all(library_dir).map_err(|e| DataError::io(library_dir, e))?;
    let path = library_dir.join(LIBRARY_META_FILENAME);
    let content =
        serde_json::to_string_pretty(meta).map_err(|e| DataError::ConfigSerialize { source: e })?;
    std::fs::write(&path, content).map_err(|e| DataError::io(&path, e))
}

/// A parsed `<owner>/<repo>` GitHub slug, sanitized for use as a directory
/// name and inside `skill()` overlay expressions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubSlug {
    pub owner: String,
    pub repo: String,
}

/// Parse a GitHub repo reference into an owner/repo slug.
///
/// Accepts, in order of precedence:
/// - `https://github.com/<owner>/<repo>` or `http://github.com/<owner>/<repo>`
///   (optionally with a trailing `.git` or `/`)
/// - `github.com/<owner>/<repo>`
/// - `<owner>/<repo>` (bare short form)
///
/// A bare single-segment input (no `/`, e.g. `superpowers`) is rejected here —
/// it is a re-pull-by-name request, resolved elsewhere against already-pulled
/// libraries, not an owner/repo pair.
pub fn parse_github_slug(input: &str) -> Result<GithubSlug, String> {
    let trimmed = input.trim();
    let without_scheme = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
        .or_else(|| trimmed.strip_prefix("github.com/"))
        .unwrap_or(trimmed);
    let without_trailing_slash = without_scheme.strip_suffix('/').unwrap_or(without_scheme);

    let segments: Vec<&str> = without_trailing_slash.split('/').collect();
    if segments.len() != 2 {
        return Err(format!(
            "expected a GitHub owner/repo slug (e.g. \"github.com/<owner>/<repo>\" or \"<owner>/<repo>\"), got '{input}'"
        ));
    }

    let owner = segments[0];
    let repo_raw = segments[1];
    if owner.is_empty() || repo_raw.is_empty() {
        return Err(format!(
            "owner and repo must not be empty in GitHub slug '{input}'"
        ));
    }

    let repo_no_git = repo_raw.strip_suffix(".git").unwrap_or(repo_raw);
    let repo = repo_no_git.to_lowercase();

    AgentName::new(&repo).map_err(|e| match e {
        DataError::InvalidAgentName { reason, .. } => {
            format!("invalid repo name '{repo_raw}' in GitHub slug '{input}': {reason}")
        }
        other => other.to_string(),
    })?;

    Ok(GithubSlug {
        owner: owner.to_string(),
        repo,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── parse_github_slug ────────────────────────────────────────────────────

    #[test]
    fn parse_github_slug_accepts_all_url_forms() {
        for input in [
            "https://github.com/obra/superpowers",
            "https://github.com/obra/superpowers.git",
            "http://github.com/obra/superpowers",
            "github.com/obra/superpowers",
            "obra/superpowers",
            // Trailing slash variants are tolerated too.
            "https://github.com/obra/superpowers/",
            "github.com/obra/superpowers/",
        ] {
            let slug = parse_github_slug(input)
                .unwrap_or_else(|e| panic!("'{input}' must parse; got error: {e}"));
            assert_eq!(
                slug,
                GithubSlug {
                    owner: "obra".to_string(),
                    repo: "superpowers".to_string(),
                },
                "'{input}' must produce owner=obra repo=superpowers"
            );
        }
    }

    #[test]
    fn parse_github_slug_lowercases_repo() {
        let slug = parse_github_slug("obra/SuperPowers").unwrap();
        assert_eq!(slug.repo, "superpowers", "repo must be lowercased");
        // Owner is passed through unchanged.
        assert_eq!(slug.owner, "obra");
    }

    #[test]
    fn parse_github_slug_rejects_bare_single_segment() {
        let err = parse_github_slug("superpowers")
            .expect_err("a bare single-segment input must be rejected as a slug");
        assert!(
            err.contains("owner/repo") || err.contains("superpowers"),
            "error must describe the expected owner/repo shape; got: {err}"
        );
    }

    #[test]
    fn parse_github_slug_rejects_more_than_two_segments() {
        let err = parse_github_slug("obra/superpowers/extra")
            .expect_err("more than two path segments must be rejected");
        assert!(
            err.contains("owner/repo") || err.contains("obra/superpowers/extra"),
            "error must describe the expected shape; got: {err}"
        );
    }

    #[test]
    fn parse_github_slug_rejects_invalid_repo_characters() {
        // A space in the repo segment.
        let err = parse_github_slug("obra/super powers")
            .expect_err("a space in the repo name must be rejected");
        assert!(
            err.contains("invalid repo name") || err.contains("ASCII"),
            "error must explain the invalid repo name; got: {err}"
        );

        // A shell/path-hostile character in the repo segment.
        parse_github_slug("obra/super$powers")
            .expect_err("a '$' in the repo name must be rejected");
    }

    #[test]
    fn parse_github_slug_rejects_empty_owner_or_repo() {
        parse_github_slug("/superpowers").expect_err("empty owner must be rejected");
        parse_github_slug("obra/").expect_err("empty repo must be rejected");
    }

    // ─── SkillLibraryMeta round-trip / defaulting ─────────────────────────────

    #[test]
    fn write_then_read_library_meta_round_trips_all_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("superpowers");
        let meta = SkillLibraryMeta {
            source: "https://github.com/obra/superpowers.git".to_string(),
            owner: "obra".to_string(),
            repo: "superpowers".to_string(),
            subdir: "custom-skills".to_string(),
        };

        write_library_meta(&dir, &meta).expect("write_library_meta must succeed");
        // The metadata file lands at the documented location.
        assert!(
            dir.join(LIBRARY_META_FILENAME).is_file(),
            "{LIBRARY_META_FILENAME} must exist after write"
        );

        let read_back = read_library_meta(&dir).expect("read_library_meta must succeed");
        assert_eq!(read_back, meta, "metadata must round-trip byte-for-byte");
    }

    #[test]
    fn read_library_meta_defaults_subdir_when_key_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // Hand-write a `.awman.json` that omits the `subdir` key entirely.
        std::fs::write(
            dir.join(LIBRARY_META_FILENAME),
            r#"{
                "source": "https://github.com/obra/superpowers.git",
                "owner": "obra",
                "repo": "superpowers"
            }"#,
        )
        .unwrap();

        let meta = read_library_meta(dir).expect("read must succeed with subdir omitted");
        assert_eq!(
            meta.subdir, "skills",
            "subdir must default to 'skills' when the key is absent"
        );
    }

    #[test]
    fn read_library_meta_missing_file_is_io_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = read_library_meta(tmp.path())
            .expect_err("a missing .awman.json must surface as an error, not a default");
        assert!(
            matches!(err, DataError::Io { .. }),
            "a missing metadata file must be DataError::Io; got: {err:?}"
        );
    }
}
