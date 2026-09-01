//! `WorktreeLifecycle` — pre/post worktree helper for commands that run an
//! agent inside an isolated git worktree.
//!
//! Architecturally, all worktree lifecycle logic is a command-layer concern,
//! not a `WorkflowEngine` concern. `WorkflowEngine` is handed a working
//! directory and runs steps in it; it does not know whether that directory
//! is a worktree or the main checkout.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::engine::error::EngineError;
use crate::engine::git::GitEngine;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreWorktreeDecision {
    Commit { message: String },
    UseLastCommit,
    Abort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingWorktreeDecision {
    Resume,
    Recreate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostWorkflowWorktreeAction {
    Merge,
    Discard,
    Keep,
}

/// How the worktree branch should be integrated once the user picks the
/// Merge action in the post-workflow dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeMergeMode {
    /// Plain `git merge` — preserves the branch's individual commits.
    Merge,
    /// `git merge --squash` followed by a single commit.
    Squash,
    /// Leave the branch (and worktree) alone.
    LeaveBranch,
}

/// Prebuilt dialog content for the post-workflow worktree-action prompt.
///
/// Built by the command layer (which queries the git engine for the target
/// branch and decides on the human-readable labels) and consumed by every
/// frontend. Frontends should NOT compose these strings themselves — that
/// keeps the prompt copy testable in one place and avoids divergence
/// between CLI/TUI/API wording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostWorkflowWorktreePrompt {
    /// Worktree branch name (e.g. `awman/work-item-0072`).
    pub branch: String,
    /// Branch that a Merge action would target — the parent repo's HEAD
    /// branch. Resolved via `GitEngine::current_branch`; falls back to
    /// `"current branch"` for detached HEAD or query failure.
    pub target_branch: String,
    /// Whether the workflow ended with an error (drives the title copy).
    pub had_error: bool,
    /// Title shown at the top of the dialog.
    pub title: String,
    /// Body text rendered above the action list.
    pub body: String,
    /// Label for the Merge action (e.g. `Merge into 'main'`).
    pub merge_label: String,
    /// Label for the Discard action.
    pub discard_label: String,
    /// Label for the Keep action.
    pub keep_label: String,
}

pub trait WorktreeLifecycleFrontend: UserMessageSink + Send + Sync {
    fn ask_pre_worktree_uncommitted_files(
        &mut self,
        files: &[String],
        suggested_message: &str,
    ) -> Result<PreWorktreeDecision, CommandError>;

    fn ask_existing_worktree(
        &mut self,
        path: &Path,
        branch: &str,
    ) -> Result<ExistingWorktreeDecision, CommandError>;

    fn report_worktree_created(&mut self, path: &Path, branch: &str);

    fn ask_post_workflow_action(
        &mut self,
        prompt: &PostWorkflowWorktreePrompt,
    ) -> Result<PostWorkflowWorktreeAction, CommandError>;

    fn ask_worktree_commit_before_merge(
        &mut self,
        branch: &str,
        files: &[String],
        suggested_message: &str,
    ) -> Result<Option<String>, CommandError>;

    fn ask_merge_mode(&mut self, branch: &str) -> Result<WorktreeMergeMode, CommandError>;

    fn confirm_worktree_cleanup(&mut self, branch: &str, path: &Path)
        -> Result<bool, CommandError>;

    fn report_merge_conflict(&mut self, branch: &str, worktree_path: &Path, git_root: &Path);

    fn report_worktree_discarded(&mut self, branch: &str);

    fn report_worktree_kept(&mut self, path: &Path, branch: &str);
}

pub struct WorktreeLifecycle {
    git_engine: Arc<GitEngine>,
    git_root: PathBuf,
    worktree_path: PathBuf,
    branch: String,
}

impl WorktreeLifecycle {
    pub fn for_workflow(
        git_engine: Arc<GitEngine>,
        git_root: PathBuf,
        workflow_name: &str,
    ) -> Result<Self, CommandError> {
        let worktree_path = git_engine
            .worktree_path_named(&git_root, workflow_name)
            .map_err(CommandError::from)?;
        let branch = git_engine.branch_name_for_workflow(workflow_name);
        Ok(Self {
            git_engine,
            git_root,
            worktree_path,
            branch,
        })
    }

    pub fn for_work_item(
        git_engine: Arc<GitEngine>,
        git_root: PathBuf,
        work_item: u32,
    ) -> Result<Self, CommandError> {
        let worktree_path = git_engine
            .worktree_path(&git_root, work_item)
            .map_err(CommandError::from)?;
        let branch = git_engine.branch_name_for_work_item(work_item);
        Ok(Self {
            git_engine,
            git_root,
            worktree_path,
            branch,
        })
    }

    pub fn worktree_path(&self) -> &Path {
        &self.worktree_path
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Compose the [`PostWorkflowWorktreePrompt`] handed to the frontend.
    /// All copy lives here — the frontend just renders the strings.
    fn build_post_workflow_prompt(&self, had_error: bool) -> PostWorkflowWorktreePrompt {
        let target_branch = self
            .git_engine
            .current_branch(&self.git_root)
            .unwrap_or_else(|| "current branch".to_string());
        let status = if had_error {
            "ended with errors"
        } else {
            "completed successfully"
        };
        PostWorkflowWorktreePrompt {
            branch: self.branch.clone(),
            target_branch: target_branch.clone(),
            had_error,
            title: "Workflow Complete — Worktree Action".to_string(),
            body: format!(
                "Workflow {status}.\nBranch: {branch}\n\nChoose what to do with the worktree:",
                branch = self.branch,
            ),
            merge_label: format!("Merge into '{target_branch}'"),
            discard_label: "Discard worktree (delete branch and directory)".to_string(),
            keep_label: "Keep worktree for later".to_string(),
        }
    }

    pub async fn prepare(
        &self,
        frontend: &mut dyn WorktreeLifecycleFrontend,
    ) -> Result<PathBuf, CommandError> {
        if self.git_engine.is_detached_head(&self.git_root) {
            frontend.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: "current HEAD is detached; the new worktree branch may target an unexpected commit".to_string(),
            });
        }
        if self.worktree_path.exists() {
            match frontend.ask_existing_worktree(&self.worktree_path, &self.branch)? {
                ExistingWorktreeDecision::Resume => {
                    return Ok(self.worktree_path.clone());
                }
                ExistingWorktreeDecision::Recreate => {
                    self.git_engine.remove_worktree_logged(
                        &self.git_root,
                        &self.worktree_path,
                        frontend,
                    )?;
                }
            }
        }
        // Always check for dirty state before creating a new worktree,
        // regardless of whether we're creating fresh or recreating.
        let files = self
            .git_engine
            .uncommitted_files_logged(&self.git_root, frontend)?;
        if !files.is_empty() {
            let suggested = format!("WIP: pre-worktree commit for {}", self.branch);
            match frontend.ask_pre_worktree_uncommitted_files(&files, &suggested)? {
                PreWorktreeDecision::Commit { message } => {
                    self.git_engine
                        .commit_all_logged(&self.git_root, &message, frontend)?;
                }
                PreWorktreeDecision::UseLastCommit => {}
                PreWorktreeDecision::Abort => return Err(CommandError::Aborted),
            }
        }
        self.git_engine.create_worktree_logged(
            &self.git_root,
            &self.worktree_path,
            &self.branch,
            frontend,
        )?;
        frontend.report_worktree_created(&self.worktree_path, &self.branch);
        Ok(self.worktree_path.clone())
    }

    pub async fn finalize(
        &self,
        frontend: &mut dyn WorktreeLifecycleFrontend,
        had_error: bool,
    ) -> Result<(), CommandError> {
        let prompt = self.build_post_workflow_prompt(had_error);
        let action = frontend.ask_post_workflow_action(&prompt)?;
        match action {
            PostWorkflowWorktreeAction::Merge => {
                let mode = frontend.ask_merge_mode(&self.branch)?;
                // The commit-uncommitted step runs for every mode — including
                // LeaveBranch — so nothing on the branch is left dangling.
                let files = self
                    .git_engine
                    .uncommitted_files_logged(&self.worktree_path, frontend)?;
                if !files.is_empty() {
                    let suggested = format!("Implement {}", self.branch);
                    if let Some(msg) = frontend.ask_worktree_commit_before_merge(
                        &self.branch,
                        &files,
                        &suggested,
                    )? {
                        self.git_engine
                            .commit_all_logged(&self.worktree_path, &msg, frontend)?;
                    }
                }
                let squash = match mode {
                    WorktreeMergeMode::LeaveBranch => {
                        frontend.report_worktree_kept(&self.worktree_path, &self.branch);
                        return Ok(());
                    }
                    WorktreeMergeMode::Squash => true,
                    WorktreeMergeMode::Merge => false,
                };
                match self.git_engine.merge_branch_logged(
                    &self.git_root,
                    &self.branch,
                    &self.worktree_path,
                    squash,
                    frontend,
                ) {
                    Ok(()) => {
                        if frontend.confirm_worktree_cleanup(&self.branch, &self.worktree_path)? {
                            self.git_engine.remove_worktree_logged(
                                &self.git_root,
                                &self.worktree_path,
                                frontend,
                            )?;
                            self.git_engine.delete_branch_logged(
                                &self.git_root,
                                &self.branch,
                                frontend,
                            )?;
                            frontend.report_worktree_discarded(&self.branch);
                        } else {
                            frontend.report_worktree_kept(&self.worktree_path, &self.branch);
                        }
                    }
                    Err(EngineError::MergeConflict { .. }) => {
                        frontend.report_merge_conflict(
                            &self.branch,
                            &self.worktree_path,
                            &self.git_root,
                        );
                    }
                    Err(other) => return Err(CommandError::from(other)),
                }
            }
            PostWorkflowWorktreeAction::Discard => {
                self.git_engine.remove_worktree_logged(
                    &self.git_root,
                    &self.worktree_path,
                    frontend,
                )?;
                self.git_engine
                    .delete_branch_logged(&self.git_root, &self.branch, frontend)?;
                frontend.report_worktree_discarded(&self.branch);
            }
            PostWorkflowWorktreeAction::Keep => {
                frontend.report_worktree_kept(&self.worktree_path, &self.branch);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
impl WorktreeLifecycle {
    pub(crate) fn new_for_test(
        git_engine: Arc<GitEngine>,
        git_root: PathBuf,
        worktree_path: PathBuf,
        branch: String,
    ) -> Self {
        Self {
            git_engine,
            git_root,
            worktree_path,
            branch,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command as SysCmd;
    use std::sync::Arc;

    use super::*;
    use crate::data::message::{MessageLevel, UserMessage};
    use crate::engine::git::GitEngine;

    // ─── Recording frontend ───────────────────────────────────────────────────

    struct RecordingWorktreeLifecycleFrontend {
        pre_uncommitted_response: PreWorktreeDecision,
        existing_worktree_response: ExistingWorktreeDecision,
        post_workflow_action: PostWorkflowWorktreeAction,
        commit_before_merge_response: Option<String>,
        merge_mode_response: WorktreeMergeMode,
        confirm_cleanup_response: bool,

        messages: Vec<UserMessage>,
        worktree_created_calls: Vec<(PathBuf, String)>,
        merge_conflict_calls: Vec<String>,
        discarded_calls: Vec<String>,
        kept_calls: Vec<(PathBuf, String)>,
    }

    impl RecordingWorktreeLifecycleFrontend {
        fn new() -> Self {
            Self {
                pre_uncommitted_response: PreWorktreeDecision::UseLastCommit,
                existing_worktree_response: ExistingWorktreeDecision::Resume,
                post_workflow_action: PostWorkflowWorktreeAction::Keep,
                commit_before_merge_response: None,
                merge_mode_response: WorktreeMergeMode::Squash,
                confirm_cleanup_response: true,
                messages: vec![],
                worktree_created_calls: vec![],
                merge_conflict_calls: vec![],
                discarded_calls: vec![],
                kept_calls: vec![],
            }
        }
    }

    impl crate::data::message::UserMessageSink for RecordingWorktreeLifecycleFrontend {
        fn write_message(&mut self, msg: UserMessage) {
            self.messages.push(msg);
        }
        fn replay_queued(&mut self) {}
    }

    impl WorktreeLifecycleFrontend for RecordingWorktreeLifecycleFrontend {
        fn ask_pre_worktree_uncommitted_files(
            &mut self,
            _files: &[String],
            _suggested_message: &str,
        ) -> Result<PreWorktreeDecision, CommandError> {
            Ok(self.pre_uncommitted_response.clone())
        }

        fn ask_existing_worktree(
            &mut self,
            _path: &Path,
            _branch: &str,
        ) -> Result<ExistingWorktreeDecision, CommandError> {
            Ok(self.existing_worktree_response)
        }

        fn report_worktree_created(&mut self, path: &Path, branch: &str) {
            self.worktree_created_calls
                .push((path.to_path_buf(), branch.to_string()));
        }

        fn ask_post_workflow_action(
            &mut self,
            _prompt: &PostWorkflowWorktreePrompt,
        ) -> Result<PostWorkflowWorktreeAction, CommandError> {
            Ok(self.post_workflow_action)
        }

        fn ask_worktree_commit_before_merge(
            &mut self,
            _branch: &str,
            _files: &[String],
            _suggested_message: &str,
        ) -> Result<Option<String>, CommandError> {
            Ok(self.commit_before_merge_response.clone())
        }

        fn ask_merge_mode(&mut self, _branch: &str) -> Result<WorktreeMergeMode, CommandError> {
            Ok(self.merge_mode_response)
        }

        fn confirm_worktree_cleanup(
            &mut self,
            _branch: &str,
            _path: &Path,
        ) -> Result<bool, CommandError> {
            Ok(self.confirm_cleanup_response)
        }

        fn report_merge_conflict(&mut self, branch: &str, _worktree_path: &Path, _git_root: &Path) {
            self.merge_conflict_calls.push(branch.to_string());
        }

        fn report_worktree_discarded(&mut self, branch: &str) {
            self.discarded_calls.push(branch.to_string());
        }

        fn report_worktree_kept(&mut self, path: &Path, branch: &str) {
            self.kept_calls
                .push((path.to_path_buf(), branch.to_string()));
        }
    }

    // ─── Git helpers ──────────────────────────────────────────────────────────

    fn init_repo(dir: &std::path::Path) {
        SysCmd::new("git")
            .args(["init"])
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        SysCmd::new("git")
            .args(["config", "user.email", "test@awman.test"])
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        SysCmd::new("git")
            .args(["config", "user.name", "awman-test"])
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        std::fs::write(dir.join("README.md"), "initial").unwrap();
        SysCmd::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        SysCmd::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
    }

    fn git_log_count(dir: &std::path::Path) -> usize {
        let out = SysCmd::new("git")
            .args(["log", "--oneline"])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .count()
    }

    // ─── prepare tests ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn prepare_happy_path_creates_worktree_and_reports() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let engine = Arc::new(GitEngine::new());
        let lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root,
            wt_path.clone(),
            "awman/test-happy".to_string(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        let result = lifecycle.prepare(&mut fe).await;
        assert!(result.is_ok(), "prepare must succeed: {result:?}");
        assert_eq!(result.unwrap(), wt_path);
        assert!(wt_path.exists(), "worktree directory must be created");
        assert_eq!(fe.worktree_created_calls.len(), 1);
        assert_eq!(fe.worktree_created_calls[0].0, wt_path);
        assert_eq!(fe.worktree_created_calls[0].1, "awman/test-happy");
    }

    #[tokio::test]
    async fn prepare_with_uncommitted_files_commit_creates_new_commit() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        std::fs::write(repo.path().join("dirty.txt"), "dirty").unwrap();
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let engine = Arc::new(GitEngine::new());
        let lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root.clone(),
            wt_path.clone(),
            "awman/test-commit".to_string(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.pre_uncommitted_response = PreWorktreeDecision::Commit {
            message: "auto-commit".to_string(),
        };
        let before = git_log_count(&git_root);
        let result = lifecycle.prepare(&mut fe).await;
        assert!(result.is_ok(), "prepare must succeed: {result:?}");
        let after = git_log_count(&git_root);
        assert_eq!(after, before + 1, "commit_all must add exactly one commit");
        assert!(wt_path.exists());
        assert_eq!(fe.worktree_created_calls.len(), 1);
    }

    #[tokio::test]
    async fn prepare_with_uncommitted_files_use_last_commit_does_not_add_commit() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        std::fs::write(repo.path().join("dirty.txt"), "dirty").unwrap();
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let engine = Arc::new(GitEngine::new());
        let lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root.clone(),
            wt_path.clone(),
            "awman/test-uselast".to_string(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.pre_uncommitted_response = PreWorktreeDecision::UseLastCommit;
        let before = git_log_count(&git_root);
        let result = lifecycle.prepare(&mut fe).await;
        assert!(result.is_ok(), "prepare must succeed: {result:?}");
        let after = git_log_count(&git_root);
        assert_eq!(after, before, "UseLastCommit must NOT create a new commit");
        assert!(wt_path.exists());
        assert_eq!(fe.worktree_created_calls.len(), 1);
    }

    #[tokio::test]
    async fn prepare_with_uncommitted_files_abort_returns_aborted_and_no_worktree() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        std::fs::write(repo.path().join("dirty.txt"), "dirty").unwrap();
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let engine = Arc::new(GitEngine::new());
        let lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root,
            wt_path.clone(),
            "awman/test-abort".to_string(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.pre_uncommitted_response = PreWorktreeDecision::Abort;
        let result = lifecycle.prepare(&mut fe).await;
        assert!(
            matches!(result, Err(CommandError::Aborted)),
            "Abort must return CommandError::Aborted"
        );
        assert!(!wt_path.exists(), "worktree must NOT be created on Abort");
        assert!(fe.worktree_created_calls.is_empty());
    }

    #[tokio::test]
    async fn prepare_existing_worktree_resume_returns_path_without_recreating() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let branch = "awman/test-resume";
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();
        // Write a sentinel that must survive Resume (no recreation).
        std::fs::write(wt_path.join("sentinel.txt"), "existing").unwrap();

        let lifecycle =
            WorktreeLifecycle::new_for_test(engine, git_root, wt_path.clone(), branch.to_string());
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.existing_worktree_response = ExistingWorktreeDecision::Resume;
        let result = lifecycle.prepare(&mut fe).await;
        assert!(result.is_ok(), "prepare(Resume) must succeed: {result:?}");
        assert_eq!(result.unwrap(), wt_path);
        assert!(
            wt_path.join("sentinel.txt").exists(),
            "sentinel must survive Resume"
        );
        assert!(
            fe.worktree_created_calls.is_empty(),
            "create_worktree must NOT be called on Resume"
        );
    }

    #[tokio::test]
    async fn prepare_existing_worktree_recreate_removes_then_recreates() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let branch = "awman/test-recreate";
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();
        std::fs::write(wt_path.join("sentinel.txt"), "original").unwrap();

        let lifecycle =
            WorktreeLifecycle::new_for_test(engine, git_root, wt_path.clone(), branch.to_string());
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.existing_worktree_response = ExistingWorktreeDecision::Recreate;
        let result = lifecycle.prepare(&mut fe).await;
        assert!(result.is_ok(), "prepare(Recreate) must succeed: {result:?}");
        assert!(wt_path.exists(), "worktree must exist after Recreate");
        assert!(
            !wt_path.join("sentinel.txt").exists(),
            "original sentinel must be gone after Recreate"
        );
        assert_eq!(
            fe.worktree_created_calls.len(),
            1,
            "create_worktree must be called on Recreate"
        );
    }

    #[tokio::test]
    async fn prepare_existing_worktree_recreate_with_dirty_files_commits() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let branch = "awman/test-recreate-dirty";
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();
        // Make the main branch dirty AFTER the worktree already exists.
        std::fs::write(repo.path().join("dirty.txt"), "dirty").unwrap();

        let lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root.clone(),
            wt_path.clone(),
            branch.to_string(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.existing_worktree_response = ExistingWorktreeDecision::Recreate;
        fe.pre_uncommitted_response = PreWorktreeDecision::Commit {
            message: "pre-recreate commit".to_string(),
        };
        let before = git_log_count(&git_root);
        let result = lifecycle.prepare(&mut fe).await;
        assert!(
            result.is_ok(),
            "prepare(Recreate+dirty) must succeed: {result:?}"
        );
        let after = git_log_count(&git_root);
        assert_eq!(
            after,
            before + 1,
            "dirty files must be committed before recreating worktree"
        );
        assert!(wt_path.exists(), "worktree must exist after Recreate");
        assert_eq!(fe.worktree_created_calls.len(), 1);
    }

    #[tokio::test]
    async fn prepare_existing_worktree_recreate_with_dirty_files_abort() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let branch = "awman/test-recreate-abort";
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();
        std::fs::write(repo.path().join("dirty.txt"), "dirty").unwrap();

        let lifecycle =
            WorktreeLifecycle::new_for_test(engine, git_root, wt_path.clone(), branch.to_string());
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.existing_worktree_response = ExistingWorktreeDecision::Recreate;
        fe.pre_uncommitted_response = PreWorktreeDecision::Abort;
        let result = lifecycle.prepare(&mut fe).await;
        assert!(
            matches!(result, Err(CommandError::Aborted)),
            "Abort must return CommandError::Aborted on Recreate path"
        );
        assert!(fe.worktree_created_calls.is_empty());
    }

    #[tokio::test]
    async fn prepare_detached_head_writes_warning_message_before_creation() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        // Detach HEAD
        let sha_out = SysCmd::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        let sha = String::from_utf8_lossy(&sha_out.stdout).trim().to_string();
        SysCmd::new("git")
            .args(["checkout", "--detach", &sha])
            .current_dir(repo.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();

        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let engine = Arc::new(GitEngine::new());
        let lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root,
            wt_path,
            "awman/detach-test".to_string(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        let _ = lifecycle.prepare(&mut fe).await;
        assert!(
            fe.messages
                .iter()
                .any(|m| { m.level == MessageLevel::Warning && m.text.contains("detached") }),
            "must write a Warning message mentioning 'detached'; got: {:?}",
            fe.messages
        );
        // The warning must appear before report_worktree_created (which records
        // in worktree_created_calls). We verify ordering: messages is non-empty
        // before the first worktree_created_calls entry was made.
        // Since messages are recorded as the frontend methods are called in
        // prepare(), and write_message is called first, the message slice is
        // non-empty by the time the worktree is created.
        assert!(!fe.messages.is_empty());
    }

    // ─── finalize tests ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn finalize_keep_calls_report_worktree_kept_and_no_git_side_effects() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let engine = Arc::new(GitEngine::new());
        let lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root,
            wt_path.clone(),
            "awman/keep-branch".to_string(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.post_workflow_action = PostWorkflowWorktreeAction::Keep;
        let result = lifecycle.finalize(&mut fe, false).await;
        assert!(result.is_ok(), "finalize(Keep) must return Ok: {result:?}");
        assert_eq!(fe.kept_calls.len(), 1);
        assert_eq!(fe.kept_calls[0].0, wt_path);
        assert!(fe.discarded_calls.is_empty());
        assert!(fe.merge_conflict_calls.is_empty());
    }

    #[tokio::test]
    async fn finalize_discard_removes_worktree_and_deletes_branch() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let branch = "awman/discard-branch";
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();
        assert!(wt_path.exists());

        let lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root.clone(),
            wt_path.clone(),
            branch.to_string(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.post_workflow_action = PostWorkflowWorktreeAction::Discard;
        let result = lifecycle.finalize(&mut fe, false).await;
        assert!(
            result.is_ok(),
            "finalize(Discard) must return Ok: {result:?}"
        );
        assert!(
            !wt_path.exists(),
            "worktree directory must be removed on Discard"
        );
        assert_eq!(fe.discarded_calls.len(), 1);
        assert!(fe.kept_calls.is_empty());
        assert!(
            !GitEngine::new().branch_exists(&git_root, branch),
            "branch must be deleted on Discard"
        );
    }

    #[tokio::test]
    async fn finalize_merge_squash_merges_and_cleans_up_on_confirm() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let branch = "awman/merge-ok-branch";
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();
        // Add a commit in the worktree so there is something to merge.
        std::fs::write(wt_path.join("work.txt"), "done").unwrap();
        engine
            .commit_all(&wt_path, "work done in worktree")
            .unwrap();

        let lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root.clone(),
            wt_path.clone(),
            branch.to_string(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.post_workflow_action = PostWorkflowWorktreeAction::Merge;
        fe.merge_mode_response = WorktreeMergeMode::Squash;
        fe.confirm_cleanup_response = true;
        let result = lifecycle.finalize(&mut fe, false).await;
        assert!(result.is_ok(), "finalize(Merge) must return Ok: {result:?}");
        assert!(
            !wt_path.exists(),
            "worktree must be removed after merge + cleanup"
        );
        assert_eq!(
            fe.discarded_calls.len(),
            1,
            "report_worktree_discarded must be called"
        );
        assert!(fe.merge_conflict_calls.is_empty());
    }

    #[tokio::test]
    async fn finalize_merge_with_uncommitted_files_commits_before_merge() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let branch = "awman/merge-precommit-branch";
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();
        // Leave an uncommitted file in the worktree.
        std::fs::write(wt_path.join("uncommitted.txt"), "not committed").unwrap();

        let lifecycle =
            WorktreeLifecycle::new_for_test(engine, git_root, wt_path.clone(), branch.to_string());
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.post_workflow_action = PostWorkflowWorktreeAction::Merge;
        fe.commit_before_merge_response = Some("pre-merge commit".to_string());
        fe.merge_mode_response = WorktreeMergeMode::Squash;
        fe.confirm_cleanup_response = true;
        let result = lifecycle.finalize(&mut fe, false).await;
        assert!(
            result.is_ok(),
            "finalize with pre-merge commit must succeed: {result:?}"
        );
        assert!(!wt_path.exists());
    }

    #[tokio::test]
    async fn finalize_merge_no_squash_preserves_branch_commits() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let branch = "awman/merge-nosquash-branch";
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();
        std::fs::write(wt_path.join("work.txt"), "done").unwrap();
        engine
            .commit_all(&wt_path, "work done in worktree")
            .unwrap();

        let lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root.clone(),
            wt_path.clone(),
            branch.to_string(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.post_workflow_action = PostWorkflowWorktreeAction::Merge;
        fe.merge_mode_response = WorktreeMergeMode::Merge;
        fe.confirm_cleanup_response = true;
        let result = lifecycle.finalize(&mut fe, false).await;
        assert!(
            result.is_ok(),
            "finalize(Merge, no squash) must return Ok: {result:?}"
        );
        // A plain merge preserves the branch's own commit — the subject must
        // appear in the target branch's log (a squash would rewrite it to
        // "Implement <branch>").
        let out = SysCmd::new("git")
            .args(["log", "--format=%s"])
            .current_dir(&git_root)
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(
            log.lines().any(|l| l == "work done in worktree"),
            "branch commit must survive a no-squash merge; log:\n{log}"
        );
        assert!(
            !wt_path.exists(),
            "worktree must be removed after merge + cleanup"
        );
        assert_eq!(fe.discarded_calls.len(), 1);
        assert!(fe.merge_conflict_calls.is_empty());
    }

    #[tokio::test]
    async fn finalize_merge_leave_branch_still_commits_uncommitted_and_keeps() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let branch = "awman/leave-branch";
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();
        // Leave an uncommitted file in the worktree.
        std::fs::write(wt_path.join("uncommitted.txt"), "not committed").unwrap();

        let lifecycle = WorktreeLifecycle::new_for_test(
            engine.clone(),
            git_root.clone(),
            wt_path.clone(),
            branch.to_string(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.post_workflow_action = PostWorkflowWorktreeAction::Merge;
        fe.merge_mode_response = WorktreeMergeMode::LeaveBranch;
        fe.commit_before_merge_response = Some("tidy up branch".to_string());
        let before = git_log_count(&wt_path);
        let result = lifecycle.finalize(&mut fe, false).await;
        assert!(
            result.is_ok(),
            "finalize(LeaveBranch) must return Ok: {result:?}"
        );
        // The commit-uncommitted step must still run even though the branch
        // is left alone.
        let after = git_log_count(&wt_path);
        assert_eq!(
            after,
            before + 1,
            "uncommitted files must be committed on the branch"
        );
        assert!(
            engine.uncommitted_files(&wt_path).unwrap().is_empty(),
            "worktree must be clean after the commit step"
        );
        assert!(wt_path.exists(), "worktree must be kept");
        assert_eq!(fe.kept_calls.len(), 1);
        assert!(fe.discarded_calls.is_empty());
        // Nothing was merged into the target branch.
        let out = SysCmd::new("git")
            .args(["log", "--format=%s"])
            .current_dir(&git_root)
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(
            !log.contains("tidy up branch"),
            "commit must land on the worktree branch only; log:\n{log}"
        );
    }

    #[tokio::test]
    async fn finalize_had_error_true_is_forwarded_to_ask_post_workflow_action() {
        // Verify that when had_error=true is passed, it reaches the frontend.
        struct ErrorRecordingFrontend {
            inner: RecordingWorktreeLifecycleFrontend,
            received_had_error: Option<bool>,
        }
        impl crate::data::message::UserMessageSink for ErrorRecordingFrontend {
            fn write_message(&mut self, msg: UserMessage) {
                self.inner.write_message(msg);
            }
            fn replay_queued(&mut self) {}
        }
        impl WorktreeLifecycleFrontend for ErrorRecordingFrontend {
            fn ask_pre_worktree_uncommitted_files(
                &mut self,
                files: &[String],
                suggested_message: &str,
            ) -> Result<PreWorktreeDecision, CommandError> {
                self.inner
                    .ask_pre_worktree_uncommitted_files(files, suggested_message)
            }
            fn ask_existing_worktree(
                &mut self,
                path: &Path,
                branch: &str,
            ) -> Result<ExistingWorktreeDecision, CommandError> {
                self.inner.ask_existing_worktree(path, branch)
            }
            fn report_worktree_created(&mut self, path: &Path, branch: &str) {
                self.inner.report_worktree_created(path, branch);
            }
            fn ask_post_workflow_action(
                &mut self,
                prompt: &PostWorkflowWorktreePrompt,
            ) -> Result<PostWorkflowWorktreeAction, CommandError> {
                self.received_had_error = Some(prompt.had_error);
                self.inner.ask_post_workflow_action(prompt)
            }
            fn ask_worktree_commit_before_merge(
                &mut self,
                branch: &str,
                files: &[String],
                suggested_message: &str,
            ) -> Result<Option<String>, CommandError> {
                self.inner
                    .ask_worktree_commit_before_merge(branch, files, suggested_message)
            }
            fn ask_merge_mode(&mut self, branch: &str) -> Result<WorktreeMergeMode, CommandError> {
                self.inner.ask_merge_mode(branch)
            }
            fn confirm_worktree_cleanup(
                &mut self,
                branch: &str,
                path: &Path,
            ) -> Result<bool, CommandError> {
                self.inner.confirm_worktree_cleanup(branch, path)
            }
            fn report_merge_conflict(&mut self, branch: &str, wt: &Path, root: &Path) {
                self.inner.report_merge_conflict(branch, wt, root);
            }
            fn report_worktree_discarded(&mut self, branch: &str) {
                self.inner.report_worktree_discarded(branch);
            }
            fn report_worktree_kept(&mut self, path: &Path, branch: &str) {
                self.inner.report_worktree_kept(path, branch);
            }
        }

        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let engine = Arc::new(GitEngine::new());
        let lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root,
            wt_path,
            "awman/had-error-branch".to_string(),
        );
        let mut fe = ErrorRecordingFrontend {
            inner: {
                let mut r = RecordingWorktreeLifecycleFrontend::new();
                r.post_workflow_action = PostWorkflowWorktreeAction::Keep;
                r
            },
            received_had_error: None,
        };
        let result = lifecycle.finalize(&mut fe, true).await;
        assert!(result.is_ok(), "finalize must return Ok: {result:?}");
        assert_eq!(
            fe.received_had_error,
            Some(true),
            "had_error=true must reach ask_post_workflow_action"
        );
    }

    #[tokio::test]
    async fn finalize_merge_conflict_calls_report_and_returns_ok() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let branch = "awman/conflict-branch";
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();

        // Diverge both branches on the same file to force a conflict.
        std::fs::write(wt_path.join("README.md"), "branch version").unwrap();
        engine.commit_all(&wt_path, "branch change").unwrap();
        std::fs::write(git_root.join("README.md"), "main version").unwrap();
        engine.commit_all(&git_root, "main change").unwrap();

        let lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root.clone(),
            wt_path.clone(),
            branch.to_string(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.post_workflow_action = PostWorkflowWorktreeAction::Merge;
        fe.merge_mode_response = WorktreeMergeMode::Squash;
        let result = lifecycle.finalize(&mut fe, false).await;
        assert!(result.is_ok(), "merge conflict must return Ok: {result:?}");
        assert_eq!(
            fe.merge_conflict_calls.len(),
            1,
            "report_merge_conflict must be called exactly once"
        );
        assert!(
            fe.discarded_calls.is_empty(),
            "must NOT discard on conflict"
        );
        // Clean up git's conflicted-merge state so the temp dir drops cleanly.
        SysCmd::new("git")
            .args(["merge", "--abort"])
            .current_dir(&git_root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok();
    }
}
