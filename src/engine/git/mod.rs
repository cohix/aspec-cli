//! `engine::git` — `GitEngine`. Consolidates every git operation awman performs.
//!
//! Replaces the free `pub fn`s in `oldsrc/git.rs` with a typed object whose
//! methods are the only public surface. Implements Layer 0's
//! `GitRootResolver` trait so `Session::open` can use it.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::data::error::DataError;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::GitRootResolver;
use crate::data::worktree_paths::{
    worktree_branch_name, worktree_branch_name_for_workflow, WorktreePaths,
};
use crate::engine::error::EngineError;

/// Run a git command and log both the command line and output to the sink.
fn run_git_logged(
    args: &[&str],
    cwd: &Path,
    sink: &mut dyn UserMessageSink,
) -> Result<std::process::Output, EngineError> {
    let cmd_str = format!("git {}", args.join(" "));
    sink.write_message(UserMessage {
        level: MessageLevel::Info,
        text: format!("$ {cmd_str}"),
    });
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| EngineError::Git(format!("invoke `{cmd_str}`: {e}")))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in stdout.lines().chain(stderr.lines()) {
        if !line.trim().is_empty() {
            sink.write_message(UserMessage {
                level: if output.status.success() {
                    MessageLevel::Info
                } else {
                    MessageLevel::Warning
                },
                text: line.to_string(),
            });
        }
    }
    Ok(output)
}

/// Parsed `git --version` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitVersion {
    pub major: u32,
    pub minor: u32,
}

#[derive(Debug, Default, Clone)]
pub struct GitEngine;

impl GitEngine {
    pub fn new() -> Self {
        Self
    }

    /// Verify `git` is installed and version >= 2.5 (worktree support).
    pub fn version_check(&self) -> Result<GitVersion, EngineError> {
        let output = Command::new("git")
            .args(["--version"])
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git --version`: {e}")))?;
        let s = String::from_utf8_lossy(&output.stdout);
        let ver_str = s.trim().strip_prefix("git version ").ok_or_else(|| {
            EngineError::Git(format!("could not parse git version from: {}", s.trim()))
        })?;
        let parts: Vec<&str> = ver_str.split('.').collect();
        let major = parts
            .first()
            .and_then(|s| s.parse::<u32>().ok())
            .ok_or_else(|| EngineError::Git(format!("malformed git version: {ver_str}")))?;
        let minor = parts
            .get(1)
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        if major > 2 || (major == 2 && minor >= 5) {
            Ok(GitVersion { major, minor })
        } else {
            Err(EngineError::Git(format!(
                "git >= 2.5 is required for --worktree support (found {ver_str})"
            )))
        }
    }

    /// Resolve the git root for the given working directory via `git rev-parse
    /// --show-toplevel`.
    pub fn resolve_root(&self, working_dir: &Path) -> Result<PathBuf, EngineError> {
        let output = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(working_dir)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git rev-parse`: {e}")))?;
        if !output.status.success() {
            return Err(EngineError::Data(DataError::GitRootNotFound {
                working_dir: working_dir.to_path_buf(),
            }));
        }
        let s = String::from_utf8_lossy(&output.stdout);
        Ok(PathBuf::from(s.trim()))
    }

    /// Returns whether the worktree at `path` has zero uncommitted changes.
    pub fn is_clean(&self, path: &Path) -> Result<bool, EngineError> {
        Ok(self.uncommitted_files(path)?.is_empty())
    }

    /// `git status --porcelain` lines (one per uncommitted file).
    pub fn uncommitted_files(&self, path: &Path) -> Result<Vec<String>, EngineError> {
        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(path)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git status --porcelain`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git status failed: {}",
                stderr.trim()
            )));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.to_string())
            .collect())
    }

    /// `~/.awman/worktrees/<repo-name>/<NNNN>/` for a work-item.
    pub fn worktree_path(&self, git_root: &Path, work_item: u32) -> Result<PathBuf, EngineError> {
        let p = WorktreePaths::from_home().map_err(EngineError::Data)?;
        Ok(p.for_work_item(git_root, work_item))
    }

    /// `~/.awman/worktrees/<repo-name>/wf-<name>/` for a named workflow.
    pub fn worktree_path_named(&self, git_root: &Path, name: &str) -> Result<PathBuf, EngineError> {
        let p = WorktreePaths::from_home().map_err(EngineError::Data)?;
        Ok(p.for_workflow(git_root, name))
    }

    /// Branch name for a work-item (`awman/work-item-NNNN`).
    pub fn branch_name_for_work_item(&self, work_item: u32) -> String {
        worktree_branch_name(work_item)
    }

    /// Branch name for a named workflow (`awman/workflow-<name>`).
    pub fn branch_name_for_workflow(&self, name: &str) -> String {
        worktree_branch_name_for_workflow(name)
    }

    /// `git worktree add <path> [-b] <branch>`.
    pub fn create_worktree(
        &self,
        git_root: &Path,
        worktree_path: &Path,
        branch: &str,
    ) -> Result<(), EngineError> {
        std::fs::create_dir_all(worktree_path.parent().unwrap_or(worktree_path))
            .map_err(|e| EngineError::io(worktree_path, e))?;
        let wt_str = worktree_path
            .to_str()
            .ok_or_else(|| EngineError::Git("worktree path not UTF-8".into()))?;
        let args: Vec<&str> = if self.branch_exists(git_root, branch) {
            vec!["worktree", "add", wt_str, branch]
        } else {
            vec!["worktree", "add", wt_str, "-b", branch]
        };
        let output = Command::new("git")
            .args(&args)
            .current_dir(git_root)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git worktree add`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git worktree add failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    pub fn remove_worktree(
        &self,
        git_root: &Path,
        worktree_path: &Path,
    ) -> Result<(), EngineError> {
        let wt_str = worktree_path
            .to_str()
            .ok_or_else(|| EngineError::Git("worktree path not UTF-8".into()))?;
        let output = Command::new("git")
            .args(["worktree", "remove", "--force", wt_str])
            .current_dir(git_root)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git worktree remove`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git worktree remove failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    /// Squash-merge `branch` into the current branch and commit `Implement <branch>`.
    /// Returns `EngineError::MergeConflict` when the merge produces conflicts.
    pub fn merge_branch(
        &self,
        git_root: &Path,
        branch: &str,
        worktree_path: &Path,
    ) -> Result<(), EngineError> {
        let output = Command::new("git")
            .args(["merge", "--squash", branch])
            .current_dir(git_root)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git merge --squash`: {e}")))?;
        if !output.status.success() {
            return Err(EngineError::MergeConflict {
                branch: branch.to_string(),
                worktree_path: worktree_path.to_path_buf(),
            });
        }
        let message = format!("Implement {branch}");
        let output = Command::new("git")
            .args(["commit", "-m", &message])
            .current_dir(git_root)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git commit`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git commit failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    pub fn commit_all(&self, path: &Path, message: &str) -> Result<(), EngineError> {
        let add = Command::new("git")
            .args(["add", "-A"])
            .current_dir(path)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git add -A`: {e}")))?;
        if !add.status.success() {
            let stderr = String::from_utf8_lossy(&add.stderr);
            return Err(EngineError::Git(format!(
                "git add -A failed: {}",
                stderr.trim()
            )));
        }
        let commit = Command::new("git")
            .args(["commit", "-m", message])
            .current_dir(path)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git commit`: {e}")))?;
        if !commit.status.success() {
            let stderr = String::from_utf8_lossy(&commit.stderr);
            return Err(EngineError::Git(format!(
                "git commit failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    pub fn delete_branch(&self, git_root: &Path, branch: &str) -> Result<(), EngineError> {
        let output = Command::new("git")
            .args(["branch", "-D", branch])
            .current_dir(git_root)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git branch -D`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git branch -D failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    /// `git clone [-b <branch>] <url> <dest>`.
    /// When `branch` is `None`, clones the repository's default branch.
    pub fn clone_repo(
        &self,
        url: &str,
        branch: Option<&str>,
        dest: &Path,
    ) -> Result<(), EngineError> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| EngineError::io(parent, e))?;
        }
        let dest_str = dest
            .to_str()
            .ok_or_else(|| EngineError::Git("clone dest path not UTF-8".into()))?;
        let mut args: Vec<&str> = vec!["clone"];
        if let Some(b) = branch {
            args.push("-b");
            args.push(b);
        }
        args.push(url);
        args.push(dest_str);
        let output = Command::new("git")
            .args(&args)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git clone`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git clone failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    /// Read the URL configured for `remote` in `repo_dir`, exactly as stored
    /// in that repository's git config.
    ///
    /// Deliberately uses `git config --get remote.<name>.url` rather than
    /// `git remote get-url`: the latter applies the user's `url.*.insteadOf`
    /// rewrites, which would mask the stored value this is meant to inspect.
    pub fn remote_url(&self, repo_dir: &Path, remote: &str) -> Result<String, EngineError> {
        let key = format!("remote.{remote}.url");
        let output = Command::new("git")
            .args(["config", "--get", &key])
            .current_dir(repo_dir)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git config --get {key}`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git config --get {key} failed: {}",
                stderr.trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Fetch the latest commit from `origin` and reset the worktree to the
    /// remote's default branch. This intentionally discards local changes in
    /// a managed library clone.
    pub fn pull_latest(&self, repo_dir: &Path) -> Result<(), EngineError> {
        let fetch = Command::new("git")
            .args(["fetch", "origin"])
            .current_dir(repo_dir)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git fetch origin`: {e}")))?;
        if !fetch.status.success() {
            let stderr = String::from_utf8_lossy(&fetch.stderr);
            return Err(EngineError::Git(format!(
                "git fetch origin failed: {}",
                stderr.trim()
            )));
        }

        let reset = Command::new("git")
            .args(["reset", "--hard", "origin/HEAD"])
            .current_dir(repo_dir)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git reset --hard origin/HEAD`: {e}")))?;
        if !reset.status.success() {
            let stderr = String::from_utf8_lossy(&reset.stderr);
            return Err(EngineError::Git(format!(
                "git reset --hard origin/HEAD failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    /// Check out `branch` if it already exists locally or on a remote;
    /// otherwise create it from HEAD. Returns the disposition:
    /// `"checked-out"` for an existing branch (local or remote-tracking) or
    /// `"created"` for a new one. Errors propagate as `EngineError::Git`.
    pub fn checkout_or_create_branch(
        &self,
        path: &Path,
        branch: &str,
    ) -> Result<&'static str, EngineError> {
        // Plain `git checkout <branch>` succeeds when (a) the local branch
        // exists or (b) exactly one remote has the branch (git auto-creates
        // a tracking branch). Try this first so we don't need a separate
        // remote-branch probe.
        let output = Command::new("git")
            .args(["checkout", branch])
            .current_dir(path)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git checkout`: {e}")))?;
        if output.status.success() {
            return Ok("checked-out");
        }

        // Fall back: branch exists neither locally nor on any remote — create
        // it from the current HEAD.
        let output = Command::new("git")
            .args(["checkout", "-b", branch])
            .current_dir(path)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git checkout -b`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git checkout -b {branch} failed: {}",
                stderr.trim()
            )));
        }
        Ok("created")
    }

    /// Recursively delete a directory, ignoring missing paths. Used to clean up
    /// a cloned repo when remote-session setup fails.
    pub fn delete_directory(&self, path: &Path) -> Result<(), EngineError> {
        match std::fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(EngineError::io(path, e)),
        }
    }

    pub fn branch_exists(&self, git_root: &Path, branch: &str) -> bool {
        Command::new("git")
            .args(["rev-parse", "--verify", &format!("refs/heads/{branch}")])
            .current_dir(git_root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    pub fn is_detached_head(&self, git_root: &Path) -> bool {
        !Command::new("git")
            .args(["symbolic-ref", "--quiet", "HEAD"])
            .current_dir(git_root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Return the current branch name at `git_root` (the branch HEAD points
    /// to). Returns `None` if HEAD is detached or git invocation fails.
    pub fn current_branch(&self, git_root: &Path) -> Option<String> {
        let output = Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(git_root)
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if name.is_empty() || name == "HEAD" {
            None
        } else {
            Some(name)
        }
    }

    // ─── Logged variants ──────────────────────────────────────────────
    //
    // These methods mirror the unlogged methods above but push every git
    // command and its output to a `UserMessageSink`. Used from the
    // `WorktreeLifecycle` command layer so the user can see exactly what
    // awman is doing.

    pub fn uncommitted_files_logged(
        &self,
        path: &Path,
        sink: &mut dyn UserMessageSink,
    ) -> Result<Vec<String>, EngineError> {
        let output = run_git_logged(&["status", "--porcelain"], path, sink)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git status failed: {}",
                stderr.trim()
            )));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.to_string())
            .collect())
    }

    pub fn commit_all_logged(
        &self,
        path: &Path,
        message: &str,
        sink: &mut dyn UserMessageSink,
    ) -> Result<(), EngineError> {
        let add = run_git_logged(&["add", "-A"], path, sink)?;
        if !add.status.success() {
            let stderr = String::from_utf8_lossy(&add.stderr);
            return Err(EngineError::Git(format!(
                "git add -A failed: {}",
                stderr.trim()
            )));
        }
        let commit = run_git_logged(&["commit", "-m", message], path, sink)?;
        if !commit.status.success() {
            let stderr = String::from_utf8_lossy(&commit.stderr);
            return Err(EngineError::Git(format!(
                "git commit failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    pub fn create_worktree_logged(
        &self,
        git_root: &Path,
        worktree_path: &Path,
        branch: &str,
        sink: &mut dyn UserMessageSink,
    ) -> Result<(), EngineError> {
        std::fs::create_dir_all(worktree_path.parent().unwrap_or(worktree_path))
            .map_err(|e| EngineError::io(worktree_path, e))?;
        let wt_str = worktree_path
            .to_str()
            .ok_or_else(|| EngineError::Git("worktree path not UTF-8".into()))?;
        let args: Vec<&str> = if self.branch_exists(git_root, branch) {
            vec!["worktree", "add", wt_str, branch]
        } else {
            vec!["worktree", "add", wt_str, "-b", branch]
        };
        let output = run_git_logged(&args, git_root, sink)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git worktree add failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    pub fn remove_worktree_logged(
        &self,
        git_root: &Path,
        worktree_path: &Path,
        sink: &mut dyn UserMessageSink,
    ) -> Result<(), EngineError> {
        let wt_str = worktree_path
            .to_str()
            .ok_or_else(|| EngineError::Git("worktree path not UTF-8".into()))?;
        let output = run_git_logged(&["worktree", "remove", "--force", wt_str], git_root, sink)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git worktree remove failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    /// Merge `branch` into the current branch. With `squash: true` this stages
    /// the changes via `git merge --squash` and commits `Implement <branch>`;
    /// with `squash: false` it runs a plain `git merge`, preserving the
    /// branch's individual commits (fast-forwarding when possible).
    /// Returns `EngineError::MergeConflict` when the merge fails.
    pub fn merge_branch_logged(
        &self,
        git_root: &Path,
        branch: &str,
        worktree_path: &Path,
        squash: bool,
        sink: &mut dyn UserMessageSink,
    ) -> Result<(), EngineError> {
        if !squash {
            let message = format!("Merge {branch}");
            let output = run_git_logged(&["merge", "-m", &message, branch], git_root, sink)?;
            if !output.status.success() {
                return Err(EngineError::MergeConflict {
                    branch: branch.to_string(),
                    worktree_path: worktree_path.to_path_buf(),
                });
            }
            return Ok(());
        }
        let output = run_git_logged(&["merge", "--squash", branch], git_root, sink)?;
        if !output.status.success() {
            return Err(EngineError::MergeConflict {
                branch: branch.to_string(),
                worktree_path: worktree_path.to_path_buf(),
            });
        }
        let has_staged = {
            let check = run_git_logged(&["diff", "--cached", "--quiet"], git_root, sink)?;
            !check.status.success()
        };
        if !has_staged {
            sink.write_message(UserMessage {
                level: MessageLevel::Info,
                text: "squash merge staged no changes (branch already up to date)".to_string(),
            });
            return Ok(());
        }
        let message = format!("Implement {branch}");
        let output = run_git_logged(&["commit", "-m", &message], git_root, sink)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git commit failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    pub fn delete_branch_logged(
        &self,
        git_root: &Path,
        branch: &str,
        sink: &mut dyn UserMessageSink,
    ) -> Result<(), EngineError> {
        let output = run_git_logged(&["branch", "-D", branch], git_root, sink)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git branch -D failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    /// Logged variant of [`clone_repo`]. Streams the command line and combined
    /// stdout/stderr through `sink`. Used by the API server's session-setup
    /// path so a remote-clone failure is captured in the server log file.
    pub fn clone_repo_logged(
        &self,
        url: &str,
        branch: Option<&str>,
        dest: &Path,
        sink: &mut dyn UserMessageSink,
    ) -> Result<(), EngineError> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| EngineError::io(parent, e))?;
        }
        let dest_str = dest
            .to_str()
            .ok_or_else(|| EngineError::Git("clone dest path not UTF-8".into()))?;
        let mut args: Vec<&str> = vec!["clone"];
        if let Some(b) = branch {
            args.push("-b");
            args.push(b);
        }
        args.push(url);
        args.push(dest_str);
        // `git clone` doesn't care about cwd (dest is absolute); pick a path
        // that's guaranteed to exist so `Command::current_dir` doesn't fail.
        let cwd = dest
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        let output = run_git_logged(&args, &cwd, sink)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git clone failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    /// Logged variant of [`pull_latest`]. Streams each command and its
    /// combined stdout/stderr through `sink`.
    pub fn pull_latest_logged(
        &self,
        repo_dir: &Path,
        sink: &mut dyn UserMessageSink,
    ) -> Result<(), EngineError> {
        let fetch = run_git_logged(&["fetch", "origin"], repo_dir, sink)?;
        if !fetch.status.success() {
            let stderr = String::from_utf8_lossy(&fetch.stderr);
            return Err(EngineError::Git(format!(
                "git fetch origin failed: {}",
                stderr.trim()
            )));
        }

        let reset = run_git_logged(&["reset", "--hard", "origin/HEAD"], repo_dir, sink)?;
        if !reset.status.success() {
            let stderr = String::from_utf8_lossy(&reset.stderr);
            return Err(EngineError::Git(format!(
                "git reset --hard origin/HEAD failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    /// Logged variant of [`checkout_or_create_branch`]. Forwards every git
    /// invocation and its output to `sink`. The first `git checkout <branch>`
    /// failure is expected (it's how we detect "branch doesn't exist yet") so
    /// its noise is downgraded by [`run_git_logged`] to a warning rather than
    /// surfacing as an error.
    pub fn checkout_or_create_branch_logged(
        &self,
        path: &Path,
        branch: &str,
        sink: &mut dyn UserMessageSink,
    ) -> Result<&'static str, EngineError> {
        let output = run_git_logged(&["checkout", branch], path, sink)?;
        if output.status.success() {
            return Ok("checked-out");
        }
        let output = run_git_logged(&["checkout", "-b", branch], path, sink)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git checkout -b {branch} failed: {}",
                stderr.trim()
            )));
        }
        Ok("created")
    }
}

impl GitRootResolver for GitEngine {
    fn resolve(&self, working_dir: &Path) -> Result<PathBuf, DataError> {
        match self.resolve_root(working_dir) {
            Ok(p) => Ok(p),
            Err(EngineError::Data(d)) => Err(d),
            Err(e) => Err(DataError::GitRootResolution {
                working_dir: working_dir.to_path_buf(),
                message: e.to_string(),
            }),
        }
    }
}

/// Resolve the main `.git` directory backing a worktree checkout.
///
/// A worktree's `.git` entry is a *file* containing `gitdir: <path>` where
/// `<path>` points to `.git/worktrees/<name>/` in the main repository.
/// This function reads that pointer and returns the main `.git/` directory
/// (two levels up from the worktree entry).
///
/// Returns `Ok(None)` when `worktree_path/.git` is a directory (regular
/// repo) or does not exist.
pub fn resolve_worktree_git_dir(worktree_path: &Path) -> Result<Option<PathBuf>, EngineError> {
    let dot_git = worktree_path.join(".git");
    if !dot_git.exists() || dot_git.is_dir() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&dot_git).map_err(|e| EngineError::io(&dot_git, e))?;
    let gitdir_line = content.trim().strip_prefix("gitdir: ").ok_or_else(|| {
        EngineError::Git(format!(
            "unexpected .git file format at {}: {}",
            dot_git.display(),
            content.trim(),
        ))
    })?;
    let gitdir = if Path::new(gitdir_line).is_absolute() {
        PathBuf::from(gitdir_line)
    } else {
        worktree_path.join(gitdir_line)
    };
    // gitdir → .git/worktrees/<name>  →  parent .git/worktrees/  →  parent .git/
    let main_git_dir = gitdir.parent().and_then(|p| p.parent()).ok_or_else(|| {
        EngineError::Git(format!(
            "cannot derive main .git dir from worktree gitdir: {}",
            gitdir.display(),
        ))
    })?;
    Ok(Some(main_git_dir.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_name_for_work_item_format() {
        let g = GitEngine::new();
        assert_eq!(g.branch_name_for_work_item(7), "awman/work-item-0007");
    }

    #[test]
    fn branch_name_for_workflow_format() {
        let g = GitEngine::new();
        assert_eq!(g.branch_name_for_workflow("x"), "awman/workflow-x");
    }

    fn init_repo(dir: &std::path::Path) {
        Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@awman.test"])
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "awman-test"])
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        // Create an initial commit so branch operations work.
        std::fs::write(dir.join("README.md"), "init").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
    }

    #[test]
    fn resolve_root_returns_input_when_input_is_root() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let g = GitEngine::new();
        let resolved = g.resolve_root(tmp.path()).unwrap();
        // Canonicalize both to handle any symlink differences.
        assert_eq!(
            resolved.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn branch_exists_detects_existing_branch() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let g = GitEngine::new();
        // "main" or "master" should exist after init.
        let initial_branch = {
            let out = Command::new("git")
                .args(["rev-parse", "--abbrev-ref", "HEAD"])
                .current_dir(tmp.path())
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        assert!(
            g.branch_exists(tmp.path(), &initial_branch),
            "default branch must exist"
        );
        assert!(
            !g.branch_exists(tmp.path(), "branch-that-does-not-exist"),
            "nonexistent branch must not be found"
        );
    }

    #[test]
    fn is_detached_head_is_false_on_normal_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let g = GitEngine::new();
        assert!(!g.is_detached_head(tmp.path()));
    }

    #[test]
    fn is_detached_head_is_true_in_detached_state() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        // Detach HEAD by checking out the commit hash directly.
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Command::new("git")
            .args(["checkout", "--detach", &sha])
            .current_dir(tmp.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        let g = GitEngine::new();
        assert!(g.is_detached_head(tmp.path()));
    }

    #[test]
    fn create_then_remove_worktree_is_idempotent() {
        let repo_tmp = tempfile::tempdir().unwrap();
        let wt_tmp = tempfile::tempdir().unwrap();
        init_repo(repo_tmp.path());
        let g = GitEngine::new();
        let wt_path = wt_tmp.path().join("my-worktree");
        let branch = "awman/test-wt-branch";

        g.create_worktree(repo_tmp.path(), &wt_path, branch)
            .expect("create_worktree should succeed");
        assert!(wt_path.exists(), "worktree directory must exist");

        g.remove_worktree(repo_tmp.path(), &wt_path)
            .expect("remove_worktree should succeed");
        assert!(!wt_path.exists(), "worktree directory must be gone");
    }

    #[test]
    fn resolve_worktree_git_dir_returns_none_for_regular_repo() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let result = resolve_worktree_git_dir(tmp.path()).unwrap();
        assert!(result.is_none(), "regular repo should return None");
    }

    #[test]
    fn resolve_worktree_git_dir_returns_none_for_non_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let result = resolve_worktree_git_dir(tmp.path()).unwrap();
        assert!(result.is_none(), "non-git dir should return None");
    }

    #[test]
    fn resolve_worktree_git_dir_finds_main_git_dir() {
        let repo_tmp = tempfile::tempdir().unwrap();
        let wt_tmp = tempfile::tempdir().unwrap();
        init_repo(repo_tmp.path());
        let g = GitEngine::new();
        let wt_path = wt_tmp.path().join("my-worktree");
        g.create_worktree(repo_tmp.path(), &wt_path, "awman/test-resolve")
            .expect("create_worktree should succeed");

        let result = resolve_worktree_git_dir(&wt_path)
            .expect("should not error")
            .expect("worktree should resolve to Some");

        let expected = repo_tmp.path().join(".git").canonicalize().unwrap();
        let actual = result.canonicalize().unwrap();
        assert_eq!(actual, expected, "should resolve to main repo .git dir");

        g.remove_worktree(repo_tmp.path(), &wt_path).unwrap();
    }

    // ─── pull_latest ──────────────────────────────────────────────────────────

    fn run_git(dir: &std::path::Path, args: &[&str]) {
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

    /// Create a local upstream: a `source` working repo pushed to a `bare`
    /// repo. The `bare` repo's `file://` URL is a network-free stand-in for a
    /// GitHub remote. Returns `(source, bare)`; keep both alive for the test.
    fn setup_upstream() -> (tempfile::TempDir, tempfile::TempDir) {
        let source = tempfile::tempdir().unwrap();
        init_repo(source.path());
        let bare = tempfile::tempdir().unwrap();
        run_git(bare.path(), &["init", "--bare"]);
        // Deterministic default branch regardless of the host git version.
        run_git(source.path(), &["branch", "-M", "main"]);
        let bare_url = format!("file://{}", bare.path().display());
        run_git(source.path(), &["remote", "add", "origin", &bare_url]);
        run_git(source.path(), &["push", "-u", "origin", "main"]);
        // Point the bare repo's HEAD at main so `origin/HEAD` resolves on clone.
        run_git(bare.path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);
        (source, bare)
    }

    fn bare_url(bare: &tempfile::TempDir) -> String {
        format!("file://{}", bare.path().display())
    }

    #[test]
    fn pull_latest_picks_up_new_upstream_commit() {
        let (source, bare) = setup_upstream();
        let g = GitEngine::new();
        let work = tempfile::tempdir().unwrap();
        let clone_dir = work.path().join("clone");
        g.clone_repo(&bare_url(&bare), None, &clone_dir).unwrap();
        assert!(
            !clone_dir.join("NEW.md").exists(),
            "new file must not exist before the upstream commit is pulled"
        );

        // Add a commit upstream and push it.
        std::fs::write(source.path().join("NEW.md"), "new upstream content").unwrap();
        run_git(source.path(), &["add", "."]);
        run_git(source.path(), &["commit", "-m", "add NEW.md"]);
        run_git(source.path(), &["push", "origin", "main"]);

        g.pull_latest(&clone_dir)
            .expect("pull_latest must succeed against a reachable remote");
        assert!(
            clone_dir.join("NEW.md").exists(),
            "pull_latest must bring the new upstream commit into the working tree"
        );
    }

    #[test]
    fn pull_latest_hard_resets_dirty_working_tree() {
        let (_source, bare) = setup_upstream();
        let g = GitEngine::new();
        let work = tempfile::tempdir().unwrap();
        let clone_dir = work.path().join("clone");
        g.clone_repo(&bare_url(&bare), None, &clone_dir).unwrap();

        // `init_repo` committed README.md == "init". Dirty it locally.
        std::fs::write(clone_dir.join("README.md"), "LOCAL UNCOMMITTED EDIT").unwrap();

        g.pull_latest(&clone_dir)
            .expect("pull_latest must succeed and discard local changes");
        assert_eq!(
            std::fs::read_to_string(clone_dir.join("README.md")).unwrap(),
            "init",
            "a dirty working tree must be hard-reset back to the upstream content"
        );
    }

    #[test]
    fn pull_latest_errors_on_nonexistent_dir() {
        let g = GitEngine::new();
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let err = g
            .pull_latest(&missing)
            .expect_err("pull_latest on a non-existent dir must error");
        assert!(
            matches!(err, EngineError::Git(_)),
            "must be EngineError::Git; got {err:?}"
        );
    }

    #[test]
    fn pull_latest_errors_on_non_git_dir() {
        let g = GitEngine::new();
        let tmp = tempfile::tempdir().unwrap();
        // A real directory that is not a git repository.
        let err = g
            .pull_latest(tmp.path())
            .expect_err("pull_latest on a non-git dir must error");
        assert!(
            matches!(err, EngineError::Git(_)),
            "must be EngineError::Git; got {err:?}"
        );
    }
}
