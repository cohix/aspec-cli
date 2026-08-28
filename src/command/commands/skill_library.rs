//! Host-side orchestration for pulling managed skill libraries.

use std::path::{Component, Path, PathBuf};

use crate::command::error::CommandError;
use crate::data::fs::skill_library::{
    parse_github_slug, read_library_meta, write_library_meta, GithubSlug, SkillLibraryMeta,
    LIBRARY_META_FILENAME,
};
use crate::data::fs::SkillDirs;
use crate::data::session::AgentName;
use crate::engine::git::GitEngine;

/// The two forms accepted by `new skill --pull`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullTarget {
    /// A complete GitHub owner/repository slug, for a new pull or explicit refresh.
    Slug(GithubSlug),
    /// The short name of an already-pulled library.
    ExistingByName(String),
}

/// Resolve a `--pull` argument without looking at the filesystem.
pub fn resolve_pull_target(input: &str) -> Result<PullTarget, String> {
    if input.contains('/') || input.contains("github.com") {
        return parse_github_slug(input).map(PullTarget::Slug);
    }

    let name = input.trim();
    AgentName::new(name).map_err(|error| error.to_string())?;
    Ok(PullTarget::ExistingByName(name.to_string()))
}

/// A successfully cloned or refreshed library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullOutcome {
    pub slug: String,
    pub dir: PathBuf,
    pub subdir: String,
    pub skills_found: Vec<String>,
    pub was_update: bool,
}

/// Clone or refresh one managed skill library.
pub fn pull_library(
    git_engine: &GitEngine,
    skill_dirs: &SkillDirs,
    target: PullTarget,
    subdir_override: Option<&str>,
) -> Result<PullOutcome, CommandError> {
    if let Some(subdir) = subdir_override {
        validate_subdir(subdir)?;
    }

    let (owner, repo, slug, clone_url) = match target {
        PullTarget::Slug(slug) => {
            let clone_url = format!("https://github.com/{}/{}.git", slug.owner, slug.repo);
            (slug.owner, slug.repo.clone(), slug.repo, clone_url)
        }
        PullTarget::ExistingByName(name) => {
            AgentName::new(&name).map_err(|error| CommandError::Other(error.to_string()))?;
            let dir = skill_dirs.library_dir(&name);
            ensure_not_symlinked(&dir, &name)?;
            if !dir.exists() || !dir.join(LIBRARY_META_FILENAME).is_file() {
                return Err(CommandError::Other(format!(
                    "library '{name}' has not been pulled yet; use the full GitHub slug, e.g. --pull github.com/<owner>/{name}"
                )));
            }
            let meta = read_library_meta(&dir)?;
            (meta.owner, meta.repo, name, meta.source)
        }
    };

    let dir = skill_dirs.library_dir(&slug);
    // A symlink anywhere in the managed surface would let the refresh's hard
    // reset escape `.library/` and destroy content awman does not own, so it is
    // rejected before `dir.exists()` is ever consulted (that predicate follows
    // symlinks).
    ensure_not_symlinked(&dir, &slug)?;
    let (meta, was_update) = if !dir.exists() {
        git_engine.clone_repo(&clone_url, None, &dir)?;
        let meta = SkillLibraryMeta {
            source: clone_url,
            owner,
            repo,
            subdir: subdir_override.unwrap_or("skills").to_string(),
        };
        write_library_meta(&dir, &meta)?;
        (meta, false)
    } else {
        if !dir.join(LIBRARY_META_FILENAME).is_file() || !dir.join(".git").is_dir() {
            return Err(CommandError::Other(format!(
                "{} exists but is not an awman-managed skill library; remove it manually before pulling '{}' here",
                dir.display(),
                slug
            )));
        }

        let mut meta = read_library_meta(&dir)?;
        if meta.owner != owner || meta.repo != repo {
            return Err(CommandError::Other(format!(
                "library '{slug}' was already pulled from {}/{}; refusing to overwrite with {owner}/{repo}. Remove {} first if you want to replace it.",
                meta.owner,
                meta.repo,
                dir.display()
            )));
        }
        validate_subdir(&meta.subdir)?;
        // `.awman.json` is the recorded source of truth, but `pull_latest`
        // fetches whatever `origin` currently points at. Refuse to refresh a
        // clone whose remote has drifted away from the recorded source rather
        // than hard-resetting it to content from an unrecorded upstream.
        let origin = git_engine.remote_url(&dir, "origin")?;
        if normalize_remote_url(&origin) != normalize_remote_url(&meta.source) {
            return Err(CommandError::Other(format!(
                "library '{slug}' has a git origin ({origin}) that does not match its recorded source ({}); refusing to refresh. Remove {} first if you want to re-pull it.",
                meta.source,
                dir.display()
            )));
        }
        git_engine.pull_latest(&dir)?;
        if let Some(subdir) = subdir_override {
            meta.subdir = subdir.to_string();
        }
        // A hard reset may have restored an upstream-tracked metadata file.
        // Reassert awman's retained metadata after every managed refresh.
        write_library_meta(&dir, &meta)?;
        (meta, true)
    };

    validate_subdir(&meta.subdir)?;
    let skills_dir = dir.join(&meta.subdir);
    if !skills_dir.is_dir() {
        return Err(CommandError::Other(format!(
            "subdirectory '{}' not found in {slug}; pass --subdir to point at the folder containing SKILL.md directories",
            meta.subdir
        )));
    }

    let skills_found = discover_skills(&skills_dir)?;
    if skills_found.is_empty() {
        tracing::debug!(
            "skill library '{}' has no immediate SKILL.md children under {}",
            slug,
            skills_dir.display()
        );
    }
    Ok(PullOutcome {
        slug,
        dir,
        subdir: meta.subdir,
        skills_found,
        was_update,
    })
}

/// Refresh every valid, persisted library in deterministic slug order.
pub fn pull_all_libraries(
    git_engine: &GitEngine,
    skill_dirs: &SkillDirs,
) -> Vec<Result<PullOutcome, CommandError>> {
    skill_dirs
        .list_libraries()
        .into_iter()
        .map(|slug| {
            pull_library(
                git_engine,
                skill_dirs,
                PullTarget::ExistingByName(slug),
                None,
            )
        })
        .collect()
}

/// Reject a library path that is (or whose managed marker files are) a
/// symlink.
///
/// `Path::exists`/`is_dir`/`is_file` all traverse symlinks, so without this
/// guard a symlink planted at `.library/<slug>` — or at its `.git` /
/// `.awman.json` — would make `pull_library` treat an arbitrary external
/// checkout as managed content and hard-reset it. awman never deletes or
/// overwrites anything outside `.library/<slug>`.
fn ensure_not_symlinked(dir: &Path, slug: &str) -> Result<(), CommandError> {
    for candidate in [
        dir.to_path_buf(),
        dir.join(".git"),
        dir.join(LIBRARY_META_FILENAME),
    ] {
        let is_symlink = std::fs::symlink_metadata(&candidate)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false);
        if is_symlink {
            return Err(CommandError::Other(format!(
                "{} is a symlink, not an awman-managed skill library; remove it manually before pulling '{slug}' here",
                candidate.display()
            )));
        }
    }
    Ok(())
}

/// Normalize a git remote URL for comparison: trailing whitespace, a trailing
/// `/`, and a trailing `.git` are all insignificant.
fn normalize_remote_url(url: &str) -> String {
    let trimmed = url.trim();
    let trimmed = trimmed.strip_suffix('/').unwrap_or(trimmed);
    trimmed.strip_suffix(".git").unwrap_or(trimmed).to_string()
}

fn validate_subdir(subdir: &str) -> Result<(), CommandError> {
    if subdir.is_empty() {
        return Err(CommandError::Other(
            "skill library subdirectory must be a non-empty relative path".to_string(),
        ));
    }
    let path = Path::new(subdir);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::Prefix(_)
                    | Component::RootDir
                    | Component::CurDir
                    | Component::ParentDir
            )
        })
    {
        return Err(CommandError::Other(format!(
            "skill library subdirectory '{subdir}' must be a relative path inside the clone"
        )));
    }
    Ok(())
}

fn discover_skills(skills_dir: &Path) -> Result<Vec<String>, CommandError> {
    let entries = std::fs::read_dir(skills_dir)
        .map_err(|error| crate::data::error::DataError::io(skills_dir, error))?;
    let mut skills = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| crate::data::error::DataError::io(skills_dir, error))?;
        let path = entry.path();
        if path.is_dir() && path.join("SKILL.md").is_file() {
            skills.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    skills.sort();
    Ok(skills)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME};
    use std::process::Command;

    /// Serialises tests that toggle the process-global `GIT_CONFIG_*` env vars.
    static GIT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn run_git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|e| panic!("failed to invoke git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A network-free stand-in for a GitHub remote: a `source` working repo
    /// pushed into a `bare` repo whose `file://` URL is used as the upstream.
    struct Upstream {
        source: tempfile::TempDir,
        bare: tempfile::TempDir,
    }

    impl Upstream {
        /// Build an upstream containing the given `(relative_path, contents)`
        /// files, committed and pushed to the bare remote.
        fn new(files: &[(&str, &str)]) -> Self {
            let source = tempfile::tempdir().unwrap();
            run_git(source.path(), &["init"]);
            run_git(source.path(), &["config", "user.email", "test@awman.test"]);
            run_git(source.path(), &["config", "user.name", "awman-test"]);
            for (rel, contents) in files {
                let path = source.path().join(rel);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, contents).unwrap();
            }
            run_git(source.path(), &["add", "."]);
            run_git(source.path(), &["commit", "-m", "initial"]);
            run_git(source.path(), &["branch", "-M", "main"]);

            let bare = tempfile::tempdir().unwrap();
            run_git(bare.path(), &["init", "--bare"]);
            let bare_url = format!("file://{}", bare.path().display());
            run_git(source.path(), &["remote", "add", "origin", &bare_url]);
            run_git(source.path(), &["push", "-u", "origin", "main"]);
            run_git(bare.path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);
            Self { source, bare }
        }

        fn url(&self) -> String {
            format!("file://{}", self.bare.path().display())
        }

        /// Commit and push a new file upstream.
        fn push_file(&self, rel: &str, contents: &str) {
            let path = self.source.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, contents).unwrap();
            run_git(self.source.path(), &["add", "."]);
            run_git(self.source.path(), &["commit", "-m", "update"]);
            run_git(self.source.path(), &["push", "origin", "main"]);
        }

        /// Make the remote unreachable by deleting the bare repo from disk.
        fn destroy_remote(&self) {
            std::fs::remove_dir_all(self.bare.path()).ok();
        }
    }

    fn skill_dirs_at(home: &Path) -> SkillDirs {
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, home.to_str().unwrap())]);
        SkillDirs::from_env(&env, None).unwrap()
    }

    /// Snapshot every file under `root` (path → bytes), so a test can assert a
    /// refused pull left the directory byte-for-byte identical rather than only
    /// spot-checking one metadata field.
    fn snapshot_tree(root: &Path) -> Vec<(String, Vec<u8>)> {
        fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                match entry.file_type() {
                    Ok(t) if t.is_dir() => walk(base, &path, out),
                    Ok(t) if t.is_file() => {
                        let rel = path.strip_prefix(base).unwrap_or(&path);
                        out.push((
                            rel.display().to_string(),
                            std::fs::read(&path).unwrap_or_default(),
                        ));
                    }
                    _ => {}
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }

    /// The commit `HEAD` resolves to, or `None` when `dir` is not a git repo.
    fn git_head(dir: &Path) -> Option<String> {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Pre-seed a managed library by cloning `url` into `.library/<slug>/` and
    /// writing a matching `.awman.json`. Returns the library directory.
    fn preseed_library(
        dirs: &SkillDirs,
        slug: &str,
        owner: &str,
        repo: &str,
        url: &str,
        subdir: &str,
    ) -> PathBuf {
        let dir = dirs.library_dir(slug);
        GitEngine::new().clone_repo(url, None, &dir).unwrap();
        write_library_meta(
            &dir,
            &SkillLibraryMeta {
                source: url.to_string(),
                owner: owner.to_string(),
                repo: repo.to_string(),
                subdir: subdir.to_string(),
            },
        )
        .unwrap();
        dir
    }

    /// Run `f` with a git `insteadOf` rewrite that redirects the fixed GitHub
    /// HTTPS URL a fresh `PullTarget::Slug` builds to a local `file://` remote,
    /// so a fresh clone never touches the network. Injected additively via
    /// `GIT_CONFIG_COUNT` so the developer's global git config is untouched.
    fn with_github_insteadof<F, R>(github_url: &str, local_url: &str, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let _g = GIT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let keys = ["GIT_CONFIG_COUNT", "GIT_CONFIG_KEY_0", "GIT_CONFIG_VALUE_0"];
        let prev: Vec<Option<String>> = keys.iter().map(|k| std::env::var(k).ok()).collect();
        std::env::set_var("GIT_CONFIG_COUNT", "1");
        std::env::set_var("GIT_CONFIG_KEY_0", format!("url.{local_url}.insteadOf"));
        std::env::set_var("GIT_CONFIG_VALUE_0", github_url);
        let out = f();
        for (k, v) in keys.iter().zip(prev) {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        out
    }

    // ─── resolve_pull_target ──────────────────────────────────────────────────

    #[test]
    fn resolve_pull_target_short_name_is_existing_by_name() {
        let target = resolve_pull_target("superpowers").unwrap();
        assert_eq!(
            target,
            PullTarget::ExistingByName("superpowers".to_string())
        );
    }

    #[test]
    fn resolve_pull_target_owner_repo_is_slug() {
        let target = resolve_pull_target("obra/superpowers").unwrap();
        assert_eq!(
            target,
            PullTarget::Slug(GithubSlug {
                owner: "obra".to_string(),
                repo: "superpowers".to_string(),
            })
        );
    }

    #[test]
    fn resolve_pull_target_full_url_is_slug() {
        let target = resolve_pull_target("github.com/obra/superpowers").unwrap();
        assert_eq!(
            target,
            PullTarget::Slug(GithubSlug {
                owner: "obra".to_string(),
                repo: "superpowers".to_string(),
            })
        );
    }

    // ─── pull_library: fresh pull ─────────────────────────────────────────────

    #[test]
    fn fresh_pull_clones_and_writes_default_subdir_meta() {
        let upstream = Upstream::new(&[("skills/brainstorming/SKILL.md", "# brainstorming")]);
        let home = tempfile::tempdir().unwrap();
        let dirs = skill_dirs_at(home.path());
        let git = GitEngine::new();

        let outcome = with_github_insteadof(
            "https://github.com/obra/superpowers.git",
            &upstream.url(),
            || {
                pull_library(
                    &git,
                    &dirs,
                    PullTarget::Slug(GithubSlug {
                        owner: "obra".to_string(),
                        repo: "superpowers".to_string(),
                    }),
                    None,
                )
            },
        )
        .expect("fresh pull must succeed");

        assert!(!outcome.was_update, "a fresh clone is not an update");
        assert_eq!(outcome.subdir, "skills", "subdir defaults to 'skills'");
        assert_eq!(outcome.skills_found, vec!["brainstorming".to_string()]);

        let dir = dirs.library_dir("superpowers");
        assert!(dir.join(".git").is_dir(), "clone must have a .git dir");
        let meta = read_library_meta(&dir).unwrap();
        assert_eq!(meta.subdir, "skills");
        assert_eq!(meta.owner, "obra");
        assert_eq!(meta.repo, "superpowers");
    }

    #[test]
    fn fresh_pull_with_subdir_override_persists_custom_subdir() {
        let upstream = Upstream::new(&[("custom/brainstorming/SKILL.md", "# brainstorming")]);
        let home = tempfile::tempdir().unwrap();
        let dirs = skill_dirs_at(home.path());
        let git = GitEngine::new();

        let outcome = with_github_insteadof(
            "https://github.com/obra/superpowers.git",
            &upstream.url(),
            || {
                pull_library(
                    &git,
                    &dirs,
                    PullTarget::Slug(GithubSlug {
                        owner: "obra".to_string(),
                        repo: "superpowers".to_string(),
                    }),
                    Some("custom"),
                )
            },
        )
        .expect("fresh pull with --subdir must succeed");

        assert_eq!(outcome.subdir, "custom");
        let meta = read_library_meta(&dirs.library_dir("superpowers")).unwrap();
        assert_eq!(
            meta.subdir, "custom",
            "override must be persisted to .awman.json"
        );
    }

    // ─── pull_library: re-pull ────────────────────────────────────────────────

    #[test]
    fn re_pull_matching_origin_preserves_persisted_subdir() {
        let upstream = Upstream::new(&[("myskills/foo/SKILL.md", "# foo")]);
        let home = tempfile::tempdir().unwrap();
        let dirs = skill_dirs_at(home.path());
        let git = GitEngine::new();

        // Pre-seed a managed library that persisted a non-default subdir.
        preseed_library(
            &dirs,
            "superpowers",
            "obra",
            "superpowers",
            &upstream.url(),
            "myskills",
        );
        // Add a new skill upstream so a real refresh is observable.
        upstream.push_file("myskills/newskill/SKILL.md", "# new");

        let outcome = pull_library(
            &git,
            &dirs,
            PullTarget::ExistingByName("superpowers".to_string()),
            None,
        )
        .expect("re-pull must succeed");

        assert!(outcome.was_update, "an existing clone refresh is an update");
        assert_eq!(
            outcome.subdir, "myskills",
            "persisted subdir must be preserved when --subdir is omitted"
        );
        assert!(
            outcome.skills_found.contains(&"newskill".to_string()),
            "the refreshed upstream commit must be reflected; got {:?}",
            outcome.skills_found
        );
        let meta = read_library_meta(&dirs.library_dir("superpowers")).unwrap();
        assert_eq!(meta.subdir, "myskills");
    }

    #[test]
    fn re_pull_with_new_subdir_updates_persisted_subdir() {
        let upstream = Upstream::new(&[
            ("skills/foo/SKILL.md", "# foo"),
            ("custom/bar/SKILL.md", "# bar"),
        ]);
        let home = tempfile::tempdir().unwrap();
        let dirs = skill_dirs_at(home.path());
        let git = GitEngine::new();

        preseed_library(
            &dirs,
            "superpowers",
            "obra",
            "superpowers",
            &upstream.url(),
            "skills",
        );

        let outcome = pull_library(
            &git,
            &dirs,
            PullTarget::ExistingByName("superpowers".to_string()),
            Some("custom"),
        )
        .expect("re-pull with a new --subdir must succeed");

        assert_eq!(outcome.subdir, "custom", "the new subdir must take effect");
        assert_eq!(outcome.skills_found, vec!["bar".to_string()]);
        let meta = read_library_meta(&dirs.library_dir("superpowers")).unwrap();
        assert_eq!(meta.subdir, "custom", "the new subdir must be persisted");
    }

    // ─── pull_library: error cases ────────────────────────────────────────────

    #[test]
    fn re_pull_owner_repo_collision_errors_and_leaves_dir_untouched() {
        let upstream = Upstream::new(&[("skills/foo/SKILL.md", "# foo")]);
        let home = tempfile::tempdir().unwrap();
        let dirs = skill_dirs_at(home.path());
        let git = GitEngine::new();

        // The final path segment 'superpowers' was already pulled from alice.
        preseed_library(
            &dirs,
            "superpowers",
            "alice",
            "superpowers",
            &upstream.url(),
            "skills",
        );

        let before = snapshot_tree(&dirs.library_dir("superpowers"));
        let head_before = git_head(&dirs.library_dir("superpowers"));

        // A different owner (bob) collides on the same final segment.
        let err = pull_library(
            &git,
            &dirs,
            PullTarget::Slug(GithubSlug {
                owner: "bob".to_string(),
                repo: "superpowers".to_string(),
            }),
            None,
        )
        .expect_err("a final-segment collision from a different owner must error");
        let msg = err.to_string();
        assert!(
            msg.contains("already pulled from alice") && msg.contains("refusing to overwrite"),
            "error must name the existing owner and refuse to overwrite; got: {msg}"
        );

        // The error names the real library directory rather than a hardcoded
        // `~/.awman/skills/.library/...` layout literal, which would be wrong
        // under an AWMAN_CONFIG_HOME override.
        let dir = dirs.library_dir("superpowers");
        assert!(
            msg.contains(&dir.display().to_string()),
            "error must name the actual library directory; got: {msg}"
        );
        assert!(
            !msg.contains("~/.awman"),
            "error must not hardcode the default layout; got: {msg}"
        );

        // The on-disk library is untouched, byte for byte, including git state.
        assert_eq!(
            snapshot_tree(&dir),
            before,
            "the existing library must be left untouched by a refused collision"
        );
        assert_eq!(
            git_head(&dir),
            head_before,
            "git HEAD must not move on a refused collision"
        );
    }

    /// A symlink planted at `.library/<slug>` must never be treated as a
    /// managed clone: `pull_latest` hard-resets, and following the link would
    /// destroy a repository awman does not own (WI-0103 remediation).
    #[test]
    fn pull_into_symlinked_library_path_errors_and_leaves_target_untouched() {
        let upstream = Upstream::new(&[("skills/foo/SKILL.md", "# foo")]);
        let home = tempfile::tempdir().unwrap();
        let dirs = skill_dirs_at(home.path());
        let git = GitEngine::new();

        // An external checkout, complete with matching awman metadata, that a
        // symlink under `.library/` points at.
        let external = tempfile::tempdir().unwrap();
        let target = external.path().join("external-checkout");
        GitEngine::new()
            .clone_repo(&upstream.url(), None, &target)
            .unwrap();
        write_library_meta(
            &target,
            &SkillLibraryMeta {
                source: upstream.url(),
                owner: "obra".to_string(),
                repo: "victim".to_string(),
                subdir: "skills".to_string(),
            },
        )
        .unwrap();
        // A local edit that a hard reset would silently discard.
        std::fs::write(
            target.join("skills").join("foo").join("SKILL.md"),
            "# LOCAL",
        )
        .unwrap();
        let before = snapshot_tree(&target);

        std::fs::create_dir_all(dirs.library_root()).unwrap();
        std::os::unix::fs::symlink(&target, dirs.library_dir("victim")).unwrap();

        let err = pull_library(
            &git,
            &dirs,
            PullTarget::ExistingByName("victim".to_string()),
            None,
        )
        .expect_err("a symlinked library path must be refused");
        assert!(
            err.to_string().contains("symlink"),
            "error must explain the symlink refusal; got: {err}"
        );

        assert_eq!(
            snapshot_tree(&target),
            before,
            "the external symlink target must be left completely untouched"
        );
    }

    /// The recorded `source` in `.awman.json` is the source of truth. If the
    /// clone's `origin` has drifted to a different repository, refreshing would
    /// hard-reset the library to content from an unrecorded upstream — refuse
    /// instead (WI-0103 remediation).
    #[test]
    fn re_pull_with_drifted_origin_errors_and_leaves_clone_untouched() {
        let alice = Upstream::new(&[("skills/foo/SKILL.md", "# ALICE")]);
        let bob = Upstream::new(&[("skills/foo/SKILL.md", "# BOB")]);
        let home = tempfile::tempdir().unwrap();
        let dirs = skill_dirs_at(home.path());
        let git = GitEngine::new();

        let dir = preseed_library(&dirs, "shared", "alice", "shared", &alice.url(), "skills");
        // Only the git remote drifts; `.awman.json` still records alice.
        run_git(&dir, &["remote", "set-url", "origin", &bob.url()]);
        let before = snapshot_tree(&dir);

        let err = pull_library(
            &git,
            &dirs,
            PullTarget::ExistingByName("shared".to_string()),
            None,
        )
        .expect_err("a drifted origin must be refused");
        let msg = err.to_string();
        assert!(
            msg.contains("does not match its recorded source"),
            "error must explain the source mismatch; got: {msg}"
        );

        assert_eq!(
            snapshot_tree(&dir),
            before,
            "a refused refresh must not fetch or reset the clone"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("skills").join("foo").join("SKILL.md")).unwrap(),
            "# ALICE",
            "alice's content must survive a drifted-origin refresh attempt"
        );
    }

    #[test]
    fn pull_into_unmanaged_dir_errors_and_leaves_dir_untouched() {
        let home = tempfile::tempdir().unwrap();
        let dirs = skill_dirs_at(home.path());
        let git = GitEngine::new();

        // A directory at the target slug that is NOT an awman-managed clone.
        let dir = dirs.library_dir("superpowers");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("keep.txt"), "user content").unwrap();

        let err = pull_library(
            &git,
            &dirs,
            PullTarget::Slug(GithubSlug {
                owner: "obra".to_string(),
                repo: "superpowers".to_string(),
            }),
            None,
        )
        .expect_err("pulling into a non-managed directory must error");
        assert!(
            err.to_string()
                .contains("not an awman-managed skill library"),
            "error must flag the unmanaged directory; got: {err}"
        );

        // User content is preserved; nothing was cloned over it.
        assert!(dir.join("keep.txt").is_file(), "user content must survive");
        assert!(
            !dir.join(".git").exists(),
            "no clone must have been created"
        );
    }

    #[test]
    fn missing_subdir_errors_after_clone_but_keeps_clone_on_disk() {
        // The upstream has no `skills/` directory at all.
        let upstream = Upstream::new(&[("README.md", "no skills here")]);
        let home = tempfile::tempdir().unwrap();
        let dirs = skill_dirs_at(home.path());
        let git = GitEngine::new();

        let err = with_github_insteadof(
            "https://github.com/obra/superpowers.git",
            &upstream.url(),
            || {
                pull_library(
                    &git,
                    &dirs,
                    PullTarget::Slug(GithubSlug {
                        owner: "obra".to_string(),
                        repo: "superpowers".to_string(),
                    }),
                    None,
                )
            },
        )
        .expect_err("a missing subdir must error");
        assert!(
            err.to_string().contains("subdirectory 'skills' not found"),
            "error must name the missing subdir; got: {err}"
        );

        // Crucially, the clone must remain on disk so the user can re-run with
        // a corrected --subdir instead of re-fetching.
        let dir = dirs.library_dir("superpowers");
        assert!(
            dir.join(".git").is_dir(),
            "the clone must survive the error"
        );
        assert!(
            dir.join("README.md").is_file(),
            "cloned files must survive the error"
        );
    }

    #[test]
    fn skills_found_lists_only_immediate_skill_md_children() {
        let upstream = Upstream::new(&[
            ("skills/alpha/SKILL.md", "# alpha"),
            ("skills/beta/SKILL.md", "# beta"),
            // A directory with no SKILL.md — must be excluded.
            ("skills/gamma/README.md", "no skill file"),
            // A nested SKILL.md is not an *immediate* child — must be excluded.
            ("skills/delta/nested/SKILL.md", "# too deep"),
            // A loose file at the top of the subdir — must be excluded.
            ("skills/loose.txt", "junk"),
        ]);
        let home = tempfile::tempdir().unwrap();
        let dirs = skill_dirs_at(home.path());
        let git = GitEngine::new();

        let outcome = with_github_insteadof(
            "https://github.com/obra/superpowers.git",
            &upstream.url(),
            || {
                pull_library(
                    &git,
                    &dirs,
                    PullTarget::Slug(GithubSlug {
                        owner: "obra".to_string(),
                        repo: "superpowers".to_string(),
                    }),
                    None,
                )
            },
        )
        .expect("pull must succeed");

        assert_eq!(
            outcome.skills_found,
            vec!["alpha".to_string(), "beta".to_string()],
            "only immediate child directories containing SKILL.md must be listed"
        );
    }

    /// A repo whose subdirectory exists but holds no `<name>/SKILL.md` is a
    /// success with an empty list, not an error — the caller reports it
    /// informationally (WI-0103 edge case).
    #[test]
    fn pull_with_present_but_skill_less_subdir_succeeds_with_no_skills() {
        let upstream = Upstream::new(&[("skills/.keep", ""), ("README.md", "docs only")]);
        let home = tempfile::tempdir().unwrap();
        let dirs = skill_dirs_at(home.path());
        let git = GitEngine::new();

        let outcome = with_github_insteadof(
            "https://github.com/obra/superpowers.git",
            &upstream.url(),
            || {
                pull_library(
                    &git,
                    &dirs,
                    PullTarget::Slug(GithubSlug {
                        owner: "obra".to_string(),
                        repo: "superpowers".to_string(),
                    }),
                    None,
                )
            },
        )
        .expect("an empty skills subdir must not be an error");

        assert!(
            outcome.skills_found.is_empty(),
            "no skills must be reported; got {:?}",
            outcome.skills_found
        );
        assert_eq!(outcome.subdir, "skills");
        assert!(
            dirs.library_dir("superpowers").join(".git").is_dir(),
            "the clone must still have landed"
        );
    }

    #[test]
    fn existing_by_name_never_pulled_returns_descriptive_error() {
        let home = tempfile::tempdir().unwrap();
        let dirs = skill_dirs_at(home.path());
        let git = GitEngine::new();

        let err = pull_library(
            &git,
            &dirs,
            PullTarget::ExistingByName("superpowers".to_string()),
            None,
        )
        .expect_err("re-pulling a never-pulled library must error");
        assert!(
            err.to_string().contains("has not been pulled yet"),
            "error must direct the user to the full slug; got: {err}"
        );
    }

    // ─── pull_all_libraries ───────────────────────────────────────────────────

    #[test]
    fn pull_all_with_no_libraries_returns_empty_vec() {
        let home = tempfile::tempdir().unwrap();
        let dirs = skill_dirs_at(home.path());
        let git = GitEngine::new();
        let results = pull_all_libraries(&git, &dirs);
        assert!(
            results.is_empty(),
            "no pulled libraries yields an empty vec"
        );
    }

    #[test]
    fn pull_all_visits_libraries_in_sorted_order() {
        let upstream = Upstream::new(&[("skills/foo/SKILL.md", "# foo")]);
        let home = tempfile::tempdir().unwrap();
        let dirs = skill_dirs_at(home.path());
        let git = GitEngine::new();

        // Pre-seed three libraries out of alphabetical order.
        for slug in ["gamma", "alpha", "beta"] {
            preseed_library(&dirs, slug, "owner", slug, &upstream.url(), "skills");
        }

        let results = pull_all_libraries(&git, &dirs);
        let slugs: Vec<String> = results
            .iter()
            .map(|r| r.as_ref().expect("all pulls must succeed").slug.clone())
            .collect();
        assert_eq!(
            slugs,
            vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()],
            "libraries must be refreshed in ascending slug order"
        );
    }

    #[test]
    fn pull_all_one_unreachable_remote_others_still_refresh() {
        let reachable = Upstream::new(&[("skills/foo/SKILL.md", "# foo")]);
        let unreachable = Upstream::new(&[("skills/bar/SKILL.md", "# bar")]);
        let home = tempfile::tempdir().unwrap();
        let dirs = skill_dirs_at(home.path());
        let git = GitEngine::new();

        preseed_library(&dirs, "good", "owner", "good", &reachable.url(), "skills");
        preseed_library(&dirs, "bad", "owner", "bad", &unreachable.url(), "skills");

        // A new upstream commit for the reachable library, to prove its clone
        // actually refreshes rather than being left stale.
        reachable.push_file("skills/added/SKILL.md", "# added");

        // Break the other remote entirely.
        unreachable.destroy_remote();

        let results = pull_all_libraries(&git, &dirs);
        assert_eq!(results.len(), 2, "one result per pulled library");

        // Results are slug-sorted: "bad" before "good".
        let bad = &results[0];
        let good = &results[1];
        assert!(
            bad.is_err(),
            "the unreachable library must fail; got {bad:?}"
        );
        let good = good
            .as_ref()
            .expect("the reachable library must still refresh");
        assert_eq!(good.slug, "good");
        assert!(
            good.skills_found.contains(&"added".to_string()),
            "the reachable library's content must actually have changed; got {:?}",
            good.skills_found
        );
    }
}
