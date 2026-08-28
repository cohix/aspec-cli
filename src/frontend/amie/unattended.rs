//! The unattended frontend the amie daemon runs agents and workflows with.
//!
//! amie's whole premise is that no human is present, so every question this
//! frontend is asked has exactly one safe answer and none of them block:
//!
//! * the mount-scope question is answered with the scope captured when the
//!   condition was created — it is never widened;
//! * the workflow control board auto-advances rather than waiting for a key;
//! * a step failure aborts the run rather than waiting for a choice;
//! * a persisted workflow state is discarded so every scheduled run starts
//!   fresh;
//! * agent setup and credential consent are accepted, because the condition's
//!   agents were already validated against the repo at creation time.
//!
//! Agent output goes to the daemon log through `tracing` rather than a
//! terminal. This module holds no policy of its own: every value it returns is
//! either a constant or the condition's own captured setting.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::command::commands::agent_auth::{AgentAuthDecision, AgentAuthFrontend};
use crate::command::commands::agent_setup::{AgentSetupDecision, AgentSetupFrontend};
use crate::command::commands::amie::evaluation::AmieRunFrontends;
use crate::command::commands::exec_workflow::{ExecWorkflowCommandFrontend, WorkflowSummary};
use crate::command::commands::mount_scope::{MountScopeDecision, MountScopeFrontend};
use crate::command::commands::worktree_lifecycle::{
    ExistingWorktreeDecision, PostWorkflowWorktreeAction, PostWorkflowWorktreePrompt,
    PreWorktreeDecision, WorktreeLifecycleFrontend, WorktreeMergeMode,
};
use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::AgentName;
use crate::data::workflow_definition::WorkflowStep;
use crate::data::workflow_state::WorkflowState;
use crate::engine::agent_runtime::execution::AgentExitInfo;
use crate::engine::agent_runtime::frontend::{AgentFrontend, AgentIo, AgentProgress, AgentStatus};
use crate::engine::error::EngineError;
use crate::engine::workflow::actions::{
    AvailableActions, NextAction, ResumeMismatch, StepFailureChoice, StepOutput, WorkflowOutcome,
    WorkflowStepStatus, YoloTickOutcome,
};
use crate::engine::workflow::frontend::WorkflowFrontend;

/// Builds the daemon's unattended frontends.
pub struct UnattendedFrontends;

impl UnattendedFrontends {
    pub fn shared() -> Arc<dyn AmieRunFrontends> {
        Arc::new(Self)
    }
}

impl AmieRunFrontends for UnattendedFrontends {
    fn leader_frontend(&self, condition: &str, label: &str) -> Box<dyn AgentFrontend> {
        Box::new(UnattendedFrontend::new(format!("{condition}/{label}")))
    }

    fn workflow_frontend(
        &self,
        condition: &str,
        mount_scope: MountScopeDecision,
    ) -> Box<dyn ExecWorkflowCommandFrontend> {
        Box::new(UnattendedFrontend::with_mount_scope(
            condition.to_string(),
            mount_scope,
        ))
    }
}

/// One unattended run's frontend.
pub struct UnattendedFrontend {
    context: String,
    /// The condition's captured mount scope, returned verbatim when asked.
    mount_scope: MountScopeDecision,
}

impl UnattendedFrontend {
    fn new(context: String) -> Self {
        Self::with_mount_scope(context, MountScopeDecision::MountGitRoot)
    }

    fn with_mount_scope(context: String, mount_scope: MountScopeDecision) -> Self {
        Self {
            context,
            mount_scope,
        }
    }
}

impl UserMessageSink for UnattendedFrontend {
    fn write_message(&mut self, message: UserMessage) {
        match message.level {
            MessageLevel::Error => tracing::error!(amie = %self.context, "{}", message.text),
            MessageLevel::Warning => tracing::warn!(amie = %self.context, "{}", message.text),
            _ => tracing::info!(amie = %self.context, "{}", message.text),
        }
    }
    fn replay_queued(&mut self) {}
}

#[async_trait]
impl AgentFrontend for UnattendedFrontend {
    fn report_status(&mut self, status: AgentStatus) {
        tracing::debug!(amie = %self.context, ?status, "amie agent status");
    }

    fn report_progress(&mut self, _progress: AgentProgress) {}

    /// Non-interactive I/O: no resize channel and no initial size, so the
    /// engine pipes the agent rather than allocating a PTY. Agent output is
    /// drained into the daemon log so an unattended failure is diagnosable.
    fn take_io(&mut self) -> AgentIo {
        let (stdout_tx, stdout_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stderr_tx, stderr_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        spawn_log_drain(self.context.clone(), stdout_rx, "stdout");
        spawn_log_drain(self.context.clone(), stderr_rx, "stderr");
        AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        }
    }
}

fn spawn_log_drain(
    context: String,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    stream: &'static str,
) {
    tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            let text = String::from_utf8_lossy(&bytes);
            for line in text.lines().filter(|line| !line.trim().is_empty()) {
                tracing::info!(amie = %context, stream, "{line}");
            }
        }
    });
}

impl WorkflowFrontend for UnattendedFrontend {
    /// Auto-advance: there is no operator to consult, and blocking here would
    /// stall the condition forever.
    fn show_workflow_control_board(
        &mut self,
        _state: &WorkflowState,
        _available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        Ok(NextAction::LaunchNext)
    }

    fn yolo_countdown_tick(
        &mut self,
        _step_name: &str,
        _remaining: Duration,
        _total: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        Ok(YoloTickOutcome::Continue)
    }

    fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
        tracing::info!(amie = %self.context, step = %step.name, ?status, "amie workflow step");
    }

    fn report_step_output(&mut self, _step: &WorkflowStep, _output: StepOutput) {}

    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
        tracing::info!(amie = %self.context, ?outcome, "amie workflow completed");
    }

    /// A mismatched saved state is never resumed unattended.
    fn confirm_resume(&mut self, _mismatch: &ResumeMismatch) -> Result<bool, EngineError> {
        Ok(false)
    }

    /// Nobody can choose, so a failed step ends the run; the scheduler records
    /// the failure and backs the condition off.
    fn user_choose_after_step_failure(
        &mut self,
        _step: &WorkflowStep,
        _exit: &AgentExitInfo,
    ) -> Result<StepFailureChoice, EngineError> {
        Ok(StepFailureChoice::Abort)
    }
}

impl MountScopeFrontend for UnattendedFrontend {
    /// The scope captured when the condition was created, returned verbatim.
    fn ask_mount_scope(
        &mut self,
        _git_root: &Path,
        _cwd: &Path,
    ) -> Result<MountScopeDecision, CommandError> {
        Ok(self.mount_scope)
    }
}

impl AgentSetupFrontend for UnattendedFrontend {
    fn ask_agent_setup(
        &mut self,
        _requested: &AgentName,
        _default: &AgentName,
        _default_available: bool,
        _image_only: bool,
    ) -> Result<AgentSetupDecision, CommandError> {
        Ok(AgentSetupDecision::Setup)
    }

    fn record_fallback(&mut self, requested: &AgentName, fallback: &AgentName) {
        tracing::warn!(
            amie = %self.context,
            requested = requested.as_str(),
            fallback = fallback.as_str(),
            "amie fell back to a different agent"
        );
    }
}

impl AgentAuthFrontend for UnattendedFrontend {
    /// Credentials are injected as container env vars at startup only; the
    /// condition's agents were validated against the repo at creation.
    fn ask_agent_auth_consent(
        &mut self,
        _agent: &AgentName,
        _env_var_names: &[&str],
    ) -> Result<AgentAuthDecision, CommandError> {
        Ok(AgentAuthDecision::Accept)
    }
}

impl WorktreeLifecycleFrontend for UnattendedFrontend {
    fn ask_pre_worktree_uncommitted_files(
        &mut self,
        _files: &[String],
        _suggested_message: &str,
    ) -> Result<PreWorktreeDecision, CommandError> {
        // Never commit on a user's behalf unattended: branch from the last
        // commit and leave their working tree exactly as it was.
        Ok(PreWorktreeDecision::UseLastCommit)
    }

    fn ask_existing_worktree(
        &mut self,
        _path: &Path,
        _branch: &str,
    ) -> Result<ExistingWorktreeDecision, CommandError> {
        Ok(ExistingWorktreeDecision::Resume)
    }

    fn report_worktree_created(&mut self, path: &Path, branch: &str) {
        tracing::info!(amie = %self.context, path = %path.display(), branch, "amie worktree ready");
    }

    /// Keep the worktree and its branch: an unattended run must never discard
    /// or merge work without a human deciding to.
    fn ask_post_workflow_action(
        &mut self,
        _prompt: &PostWorkflowWorktreePrompt,
    ) -> Result<PostWorkflowWorktreeAction, CommandError> {
        Ok(PostWorkflowWorktreeAction::Keep)
    }

    fn ask_worktree_commit_before_merge(
        &mut self,
        _branch: &str,
        _files: &[String],
        _suggested_message: &str,
    ) -> Result<Option<String>, CommandError> {
        Ok(None)
    }

    fn ask_merge_mode(&mut self, _branch: &str) -> Result<WorktreeMergeMode, CommandError> {
        Ok(WorktreeMergeMode::LeaveBranch)
    }

    fn confirm_worktree_cleanup(
        &mut self,
        _branch: &str,
        _path: &Path,
    ) -> Result<bool, CommandError> {
        Ok(false)
    }

    fn report_merge_conflict(&mut self, branch: &str, wt: &Path, _root: &Path) {
        tracing::warn!(
            amie = %self.context,
            branch,
            worktree = %wt.display(),
            "amie workflow left a merge conflict"
        );
    }

    fn report_worktree_discarded(&mut self, branch: &str) {
        tracing::info!(amie = %self.context, branch, "amie worktree discarded");
    }

    fn report_worktree_kept(&mut self, path: &Path, branch: &str) {
        tracing::info!(amie = %self.context, path = %path.display(), branch, "amie worktree kept");
    }
}

impl ExecWorkflowCommandFrontend for UnattendedFrontend {
    fn set_pty_active(&mut self, _active: bool) {}

    fn report_workflow_summary(&mut self, summary: &WorkflowSummary) {
        tracing::info!(
            amie = %self.context,
            completed = summary.steps_completed,
            failed = summary.steps_failed,
            "amie workflow summary"
        );
    }

    /// Start fresh: each scheduled evaluation is its own run, and resuming a
    /// stale state unattended would silently skip steps.
    fn ask_workflow_resume_or_fresh(
        &mut self,
        _workflow_name: &str,
        _completed_steps: usize,
        _total_steps: usize,
    ) -> Result<bool, CommandError> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_workflow_frontend_answers_the_mount_scope_question_with_the_captured_scope() {
        let mut frontend = UnattendedFrontend::with_mount_scope(
            "c".into(),
            MountScopeDecision::MountCurrentDirOnly,
        );
        let decision = frontend
            .ask_mount_scope(Path::new("/repo"), Path::new("/repo/sub"))
            .unwrap();
        assert!(
            matches!(decision, MountScopeDecision::MountCurrentDirOnly),
            "an unattended run must never widen the captured mount scope"
        );
    }

    #[test]
    fn nothing_the_unattended_frontend_answers_can_block_or_destroy_work() {
        let mut frontend = UnattendedFrontend::new("c/leader".into());
        assert!(
            !frontend.ask_workflow_resume_or_fresh("wf", 1, 3).unwrap(),
            "an unattended run must start fresh rather than resume stale state"
        );
        let prompt = PostWorkflowWorktreePrompt {
            branch: "awman/amie".into(),
            target_branch: "main".into(),
            had_error: false,
            title: "t".into(),
            body: "b".into(),
            merge_label: "m".into(),
            discard_label: "d".into(),
            keep_label: "k".into(),
        };
        assert!(matches!(
            frontend.ask_post_workflow_action(&prompt),
            Ok(PostWorkflowWorktreeAction::Keep)
        ));
        assert!(matches!(
            frontend.confirm_worktree_cleanup("b", Path::new("/w")),
            Ok(false)
        ));
        assert!(matches!(
            frontend.ask_merge_mode("b"),
            Ok(WorktreeMergeMode::LeaveBranch)
        ));
        assert!(matches!(
            frontend.ask_pre_worktree_uncommitted_files(&[], ""),
            Ok(PreWorktreeDecision::UseLastCommit)
        ));
    }

    /// Non-interactive I/O is what makes the engine pipe the agent instead of
    /// allocating a PTY nobody is attached to.
    #[tokio::test]
    async fn agent_io_is_non_interactive() {
        let mut frontend = UnattendedFrontend::new("c/leader".into());
        let io = frontend.take_io();
        assert!(io.resize.is_none());
        assert!(io.initial_size.is_none());
    }
}
