//! Typed access to global and per-repo skill directories.

use std::path::{Path, PathBuf};

use crate::data::config::env::{Env, EnvSnapshot};
use crate::data::config::global::GlobalConfig;
use crate::data::error::DataError;
use crate::data::fs::skill_library::read_library_meta;

/// Directory name for global skills under the global home.
pub const GLOBAL_SKILLS_SUBDIR: &str = "skills";

/// Directory name for per-repo skills under `<git_root>/.claude/`.
pub const REPO_SKILLS_SUBDIR: &str = "skills";

/// Directory name, under the global skills directory, holding pulled skill
/// libraries (e.g. `~/.awman/skills/.library/<slug>/`).
pub const LIBRARY_SUBDIR: &str = ".library";

/// Resolves global and per-repo skill directories.
#[derive(Debug, Clone)]
pub struct SkillDirs {
    global_home: PathBuf,
    git_root: Option<PathBuf>,
}

impl SkillDirs {
    /// Construct from the current process environment, resolving the global
    /// home via `AWMAN_CONFIG_HOME` (when set) or `$HOME/.awman`.
    pub fn from_process_env(git_root: Option<PathBuf>) -> Result<Self, DataError> {
        Self::from_env(&Env::from_process(), git_root)
    }

    /// Same as [`from_process_env`] but reads from a supplied env snapshot.
    pub fn from_env(env: &EnvSnapshot, git_root: Option<PathBuf>) -> Result<Self, DataError> {
        let global_home = GlobalConfig::data_home_with(env)?;
        Ok(Self {
            global_home,
            git_root,
        })
    }

    /// Path to the global skills directory.
    pub fn global_dir(&self) -> PathBuf {
        self.global_home.join(GLOBAL_SKILLS_SUBDIR)
    }

    /// Path to the per-repo skills directory, if a git root is bound.
    pub fn repo_dir(&self) -> Option<PathBuf> {
        self.git_root
            .as_ref()
            .map(|r| r.join(".claude").join(REPO_SKILLS_SUBDIR))
    }

    /// Path to the per-repo skills directory, given an explicit git root.
    pub fn repo_dir_for(git_root: &Path) -> PathBuf {
        git_root.join(".claude").join(REPO_SKILLS_SUBDIR)
    }

    /// Create the global skills directory on disk, if missing.
    pub fn ensure_global(&self) -> Result<PathBuf, DataError> {
        let dir = self.global_dir();
        std::fs::create_dir_all(&dir).map_err(|e| DataError::io(&dir, e))?;
        Ok(dir)
    }

    /// Create the per-repo skills directory on disk, if a git root is bound.
    pub fn ensure_repo(&self) -> Result<Option<PathBuf>, DataError> {
        let Some(dir) = self.repo_dir() else {
            return Ok(None);
        };
        std::fs::create_dir_all(&dir).map_err(|e| DataError::io(&dir, e))?;
        Ok(Some(dir))
    }

    /// Path to the root of pulled skill libraries: `~/.awman/skills/.library/`.
    pub fn library_root(&self) -> PathBuf {
        self.global_dir().join(LIBRARY_SUBDIR)
    }

    /// Path to a single pulled skill library: `~/.awman/skills/.library/<slug>/`.
    ///
    /// Does not create, delete, or validate the path; callers must pass an
    /// already-validated slug.
    pub fn library_dir(&self, slug: &str) -> PathBuf {
        self.library_root().join(slug)
    }

    /// List the slugs of all currently-pulled skill libraries, in ascending
    /// slug-sorted order.
    ///
    /// A valid entry is a *real* directory under `.library/` (never a symlink)
    /// whose `.awman.json` can be read and deserialized. Missing `.library/`
    /// returns an empty list. Per-entry read/metadata failures are skipped with
    /// a `tracing::warn!`; a failure to enumerate the root itself also warns
    /// and returns an empty list. Never panics.
    ///
    /// Symlinked entries are deliberately excluded: a refresh hard-resets the
    /// library directory, and following a symlink would let that destructive
    /// operation escape the managed store.
    pub fn list_libraries(&self) -> Vec<String> {
        let root = self.library_root();
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        "failed to read skill library root {}: {}",
                        root.display(),
                        e
                    );
                }
                return Vec::new();
            }
        };

        let mut slugs = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    tracing::warn!(
                        "failed to read an entry in skill library root {}: {}",
                        root.display(),
                        e
                    );
                    continue;
                }
            };
            let path = entry.path();
            // `DirEntry::file_type` does not traverse symlinks, unlike
            // `Path::is_dir`; a symlinked entry is never a managed library.
            match entry.file_type() {
                Ok(file_type) if file_type.is_symlink() => {
                    tracing::warn!(
                        "skipping symlinked skill library entry at {}; managed libraries must be real directories",
                        path.display()
                    );
                    continue;
                }
                Ok(file_type) if file_type.is_dir() => {}
                Ok(_) => continue,
                Err(e) => {
                    tracing::warn!(
                        "failed to stat skill library entry {}: {}",
                        path.display(),
                        e
                    );
                    continue;
                }
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                tracing::warn!(
                    "skipping skill library entry with non-UTF-8 name at {}",
                    path.display()
                );
                continue;
            };
            match read_library_meta(&path) {
                Ok(_) => slugs.push(name),
                Err(e) => {
                    tracing::warn!("skipping invalid skill library '{}': {}", name, e);
                }
            }
        }
        slugs.sort();
        slugs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME};
    use crate::data::fs::skill_library::{write_library_meta, SkillLibraryMeta};

    /// Build a `SkillDirs` whose global home is `home`, with no git root bound.
    fn skill_dirs_at(home: &Path) -> SkillDirs {
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, home.to_str().unwrap())]);
        SkillDirs::from_env(&env, None).unwrap()
    }

    fn valid_meta(slug: &str) -> SkillLibraryMeta {
        SkillLibraryMeta {
            source: format!("https://github.com/someone/{slug}.git"),
            owner: "someone".to_string(),
            repo: slug.to_string(),
            subdir: "skills".to_string(),
        }
    }

    #[test]
    fn library_paths_are_under_dot_library() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = skill_dirs_at(tmp.path());
        assert_eq!(dirs.library_root(), dirs.global_dir().join(".library"));
        assert_eq!(
            dirs.library_dir("superpowers"),
            dirs.global_dir().join(".library").join("superpowers")
        );
    }

    #[test]
    fn list_libraries_returns_empty_when_library_root_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = skill_dirs_at(tmp.path());
        // Deliberately never create `.library/`.
        assert!(
            dirs.list_libraries().is_empty(),
            "a missing .library/ must yield an empty list, not an error"
        );
    }

    #[test]
    fn list_libraries_returns_only_valid_libraries_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = skill_dirs_at(tmp.path());

        // Two valid libraries (created out of alphabetical order on purpose).
        write_library_meta(&dirs.library_dir("gamma"), &valid_meta("gamma")).unwrap();
        write_library_meta(&dirs.library_dir("alpha"), &valid_meta("alpha")).unwrap();

        // A directory under `.library/` that lacks `.awman.json` — must be
        // skipped, NOT cause an error or panic.
        std::fs::create_dir_all(dirs.library_dir("orphan")).unwrap();
        std::fs::write(
            dirs.library_dir("orphan").join("README.md"),
            "not a library",
        )
        .unwrap();

        // A stray regular file under `.library/` — must be ignored.
        std::fs::write(dirs.library_root().join("loose.txt"), "junk").unwrap();

        let libs = dirs.list_libraries();
        assert_eq!(
            libs,
            vec!["alpha".to_string(), "gamma".to_string()],
            "only valid libraries must be listed, in ascending slug order"
        );
    }

    #[test]
    fn list_libraries_skips_directory_with_malformed_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = skill_dirs_at(tmp.path());

        write_library_meta(&dirs.library_dir("good"), &valid_meta("good")).unwrap();

        // A `.awman.json` that is not valid JSON must be skipped, not fatal.
        std::fs::create_dir_all(dirs.library_dir("broken")).unwrap();
        std::fs::write(
            dirs.library_dir("broken").join(".awman.json"),
            "{ this is not json",
        )
        .unwrap();

        assert_eq!(
            dirs.list_libraries(),
            vec!["good".to_string()],
            "a library with malformed metadata must be skipped"
        );
    }

    /// A symlink under `.library/` is never a managed library, even when its
    /// target is a perfectly valid one: refreshing hard-resets the directory,
    /// and following the link would let that escape the managed store
    /// (WI-0103 remediation).
    #[test]
    fn list_libraries_skips_symlinked_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = skill_dirs_at(tmp.path());

        write_library_meta(&dirs.library_dir("good"), &valid_meta("good")).unwrap();

        // A valid library living outside the store, linked into it.
        let external = tmp.path().join("external");
        write_library_meta(&external, &valid_meta("linked")).unwrap();
        std::os::unix::fs::symlink(&external, dirs.library_dir("linked")).unwrap();

        assert_eq!(
            dirs.list_libraries(),
            vec!["good".to_string()],
            "a symlinked entry must be skipped even when its target is valid"
        );
    }
}
