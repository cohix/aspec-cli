//! The unattended frontend the squad daemon runs agents and workflows with.
//!
//! squad's whole premise is that no human is present, so every question this
//! frontend is asked has exactly one safe answer and none of them block:
//!
//! * the mount-scope question is answered with the scope captured when the
//!   task was created — it is never widened;
//! * the workflow control board auto-advances rather than waiting for a key;
//! * a step failure aborts the run rather than waiting for a choice;
//! * a persisted workflow state is discarded so every scheduled run starts
//!   fresh;
//! * agent setup and credential consent are accepted, because the task's
//!   agents were already validated against the repo at creation time.
//!
//! Agent output goes to a per-container file in the task run directory rather
//! than the daemon log. This module holds no policy of its own: every value it
//! returns is either a constant or the task's own captured setting.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;

use crate::command::commands::agent_auth::{AgentAuthDecision, AgentAuthFrontend};
use crate::command::commands::agent_setup::{AgentSetupDecision, AgentSetupFrontend};
use crate::command::commands::exec_workflow::{ExecWorkflowCommandFrontend, WorkflowSummary};
use crate::command::commands::mount_scope::{MountScopeDecision, MountScopeFrontend};
use crate::command::commands::squad::evaluation::SquadRunFrontends;
use crate::command::commands::worktree_lifecycle::{
    ExistingWorktreeDecision, PostWorkflowWorktreeAction, PostWorkflowWorktreePrompt,
    PreWorktreeDecision, WorktreeLifecycleFrontend, WorktreeMergeMode,
};
use crate::command::error::CommandError;
use crate::data::fs::RunId;
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
    pub fn shared() -> Arc<dyn SquadRunFrontends> {
        Arc::new(Self)
    }
}

impl SquadRunFrontends for UnattendedFrontends {
    fn leader_frontend(
        &self,
        task: &str,
        run_id: &RunId,
        run_log_dir: &Path,
        label: &str,
    ) -> Result<Box<dyn AgentFrontend>, CommandError> {
        Ok(Box::new(UnattendedFrontend::for_run(
            task,
            run_id,
            run_log_dir,
            label,
            MountScopeDecision::MountGitRoot,
        )?))
    }

    fn workflow_frontend(
        &self,
        task: &str,
        run_id: &RunId,
        run_log_dir: &Path,
        mount_scope: MountScopeDecision,
    ) -> Result<Box<dyn ExecWorkflowCommandFrontend>, CommandError> {
        Ok(Box::new(UnattendedFrontend::for_run(
            task,
            run_id,
            run_log_dir,
            "workflow",
            mount_scope,
        )?))
    }
}

/// One unattended run's frontend.
pub struct UnattendedFrontend {
    context: String,
    task: String,
    run_id: RunId,
    /// Created by the scheduler before evaluation is dispatched. Each
    /// `AgentStatus::Running` opens its own `<container-name>.log` here before
    /// the runtime starts the container subprocess.
    run_log_dir: PathBuf,
    pending_log_files: VecDeque<SharedLogFile>,
    /// The task's captured mount scope, returned verbatim when asked.
    mount_scope: MountScopeDecision,
}

impl UnattendedFrontend {
    fn new(context: String) -> Self {
        // Test-only construction. Production always uses `for_run`, which
        // receives a scheduler-created directory and can report setup errors.
        Self::with_mount_scope(context, MountScopeDecision::MountGitRoot)
    }

    fn with_mount_scope(context: String, mount_scope: MountScopeDecision) -> Self {
        Self {
            task: context.clone(),
            context,
            run_id: RunId::new(),
            run_log_dir: std::env::temp_dir().join("awman-unattended-test-logs"),
            pending_log_files: VecDeque::new(),
            mount_scope,
        }
    }

    fn for_run(
        task: &str,
        run_id: &RunId,
        run_log_dir: &Path,
        label: &str,
        mount_scope: MountScopeDecision,
    ) -> Result<Self, CommandError> {
        if !run_log_dir.is_dir() {
            return Err(CommandError::Other(format!(
                "squad run log directory was not prepared before container launch: {}",
                run_log_dir.display()
            )));
        }
        Ok(Self {
            context: format!("{task}/{label}"),
            task: task.to_string(),
            run_id: run_id.clone(),
            run_log_dir: run_log_dir.to_path_buf(),
            pending_log_files: VecDeque::new(),
            mount_scope,
        })
    }

    fn prepare_container_log(&mut self, container_name: &str) {
        // squad names are generated by our validated slug helper. Reject a
        // surprising runtime name rather than allowing path traversal through
        // a filename from a container backend.
        if Path::new(container_name)
            .file_name()
            .and_then(|n| n.to_str())
            != Some(container_name)
        {
            tracing::error!(
                task = %self.task,
                run_id = %self.run_id,
                container = %container_name,
                "squad refused unsafe container-log filename"
            );
            return;
        }
        let path = self.run_log_dir.join(format!("{container_name}.log"));
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => {
                self.pending_log_files.push_back(Arc::new(Mutex::new(file)));
                tracing::info!(
                    task = %self.task,
                    run_id = %self.run_id,
                    container = %container_name,
                    log_path = %path.display(),
                    "squad agent container launched"
                );
            }
            Err(error) => tracing::error!(
                task = %self.task,
                run_id = %self.run_id,
                container = %container_name,
                log_path = %path.display(),
                error = %error,
                "squad failed to open per-container log"
            ),
        }
    }
}

type SharedLogFile = Arc<Mutex<File>>;

impl UserMessageSink for UnattendedFrontend {
    fn write_message(&mut self, message: UserMessage) {
        match message.level {
            MessageLevel::Error => {
                tracing::error!(task = %self.task, run_id = %self.run_id, squad = %self.context, "{}", message.text)
            }
            MessageLevel::Warning => {
                tracing::warn!(task = %self.task, run_id = %self.run_id, squad = %self.context, "{}", message.text)
            }
            _ => {
                tracing::info!(task = %self.task, run_id = %self.run_id, squad = %self.context, "{}", message.text)
            }
        }
    }
    fn replay_queued(&mut self) {}
}

#[async_trait]
impl AgentFrontend for UnattendedFrontend {
    fn report_status(&mut self, status: AgentStatus) {
        match status {
            AgentStatus::Running { container_name } => self.prepare_container_log(&container_name),
            AgentStatus::Building | AgentStatus::Pulling | AgentStatus::Starting => tracing::info!(
                task = %self.task,
                run_id = %self.run_id,
                status = ?status,
                "squad agent lifecycle transition"
            ),
            AgentStatus::Stopping | AgentStatus::Exited(_) | AgentStatus::Failed(_) => {
                tracing::info!(
                    task = %self.task,
                    run_id = %self.run_id,
                    status = ?status,
                    "squad agent lifecycle transition"
                )
            }
        }
    }

    fn report_progress(&mut self, _progress: AgentProgress) {}

    /// Every squad agent gets a PTY even when nobody is attached. This makes
    /// its process the same interactive agent TUI a later attach reconnects
    /// to, while the fixed 120x40 size is large enough for supported CLIs and
    /// does not depend on a daemon-owned terminal.
    fn take_io(&mut self) -> AgentIo {
        let (stdout_tx, stdout_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stderr_tx, stderr_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        let log_file = self.pending_log_files.pop_front();
        spawn_file_drain(log_file.clone(), stdout_rx);
        spawn_file_drain(log_file, stderr_rx);
        AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: Some(tokio::sync::mpsc::unbounded_channel().1),
            initial_size: Some((120, 40)),
        }
    }
}

fn spawn_file_drain(
    log_file: Option<SharedLogFile>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
) {
    tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            // The PTY bridge delivers one merged stream through stdout. Keep
            // the same shared file for stderr too, which also preserves a
            // faithful interleaving should a runtime ever take the piped path.
            if let Some(file) = &log_file {
                if let Ok(mut file) = file.lock() {
                    let _ = file.write_all(&bytes);
                    // Flush every bridged chunk. A daemon crash can still
                    // lose bytes in the OS page cache, but this avoids an
                    // application-level buffered tail.
                    let _ = file.flush();
                }
            }
        }
    });
}

impl WorkflowFrontend for UnattendedFrontend {
    /// Auto-advance: there is no operator to consult, and blocking here would
    /// stall the task forever.
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
        tracing::info!(
            task = %self.task,
            run_id = %self.run_id,
            step = %step.name,
            ?status,
            "squad workflow step lifecycle transition"
        );
    }

    fn report_step_output(&mut self, _step: &WorkflowStep, _output: StepOutput) {}

    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
        tracing::info!(task = %self.task, run_id = %self.run_id, ?outcome, "squad workflow completed");
    }

    /// A mismatched saved state is never resumed unattended.
    fn confirm_resume(&mut self, _mismatch: &ResumeMismatch) -> Result<bool, EngineError> {
        Ok(false)
    }

    /// Nobody can choose, so a failed step ends the run; the scheduler records
    /// the failure and backs the task off.
    fn user_choose_after_step_failure(
        &mut self,
        _step: &WorkflowStep,
        _exit: &AgentExitInfo,
    ) -> Result<StepFailureChoice, EngineError> {
        Ok(StepFailureChoice::Abort)
    }
}

impl MountScopeFrontend for UnattendedFrontend {
    /// The scope captured when the task was created, returned verbatim.
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
            task = %self.task,
            run_id = %self.run_id,
            requested = requested.as_str(),
            fallback = fallback.as_str(),
            "squad fell back to a different agent"
        );
    }
}

impl AgentAuthFrontend for UnattendedFrontend {
    /// Credentials are injected as container env vars at startup only; the
    /// task's agents were validated against the repo at creation.
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
        tracing::info!(task = %self.task, run_id = %self.run_id, path = %path.display(), branch, "squad worktree created");
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
            task = %self.task,
            run_id = %self.run_id,
            branch,
            worktree = %wt.display(),
            "squad workflow left a merge conflict"
        );
    }

    fn report_worktree_discarded(&mut self, branch: &str) {
        tracing::info!(task = %self.task, run_id = %self.run_id, branch, "squad worktree discarded");
    }

    fn report_worktree_kept(&mut self, path: &Path, branch: &str) {
        tracing::info!(task = %self.task, run_id = %self.run_id, path = %path.display(), branch, "squad worktree kept");
    }
}

impl ExecWorkflowCommandFrontend for UnattendedFrontend {
    fn set_pty_active(&mut self, _active: bool) {}

    fn report_workflow_summary(&mut self, summary: &WorkflowSummary) {
        tracing::info!(
            task = %self.task,
            run_id = %self.run_id,
            completed = summary.steps_completed,
            failed = summary.steps_failed,
            "squad workflow summary"
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

    /// Collect everything written to `tracing` while `body` runs, as text.
    fn captured_tracing(body: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
            type Writer = Sink;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let sink = Sink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();
        tracing::subscriber::with_default(subscriber, body);
        let bytes = sink.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    /// WI 0106 §6c: for a task bound to a repository, the daemon's own log
    /// must record where the run's worktree lives and what happened to it when
    /// the workflow ended. These are the callbacks the shared worktree
    /// lifecycle fires; nothing about squad skips them, and each line carries
    /// the task, the run id, and the path/branch needed to go find it.
    #[test]
    fn worktree_creation_and_disposition_are_recorded_in_the_daemon_log() {
        let created = captured_tracing(|| {
            let mut frontend = UnattendedFrontend::new("nightly".into());
            frontend.report_worktree_created(Path::new("/wt/nightly"), "awman/squad-nightly");
        });
        assert!(created.contains("squad worktree created"), "{created}");
        assert!(created.contains("/wt/nightly"), "{created}");
        assert!(created.contains("awman/squad-nightly"), "{created}");
        assert!(created.contains("INFO"), "{created}");

        let kept = captured_tracing(|| {
            let mut frontend = UnattendedFrontend::new("nightly".into());
            frontend.report_worktree_kept(Path::new("/wt/nightly"), "awman/squad-nightly");
        });
        assert!(kept.contains("squad worktree kept"), "{kept}");
        assert!(kept.contains("/wt/nightly"), "{kept}");

        let discarded = captured_tracing(|| {
            let mut frontend = UnattendedFrontend::new("nightly".into());
            frontend.report_worktree_discarded("awman/squad-nightly");
        });
        assert!(
            discarded.contains("squad worktree discarded"),
            "{discarded}"
        );
        assert!(discarded.contains("awman/squad-nightly"), "{discarded}");

        let conflict = captured_tracing(|| {
            let mut frontend = UnattendedFrontend::new("nightly".into());
            frontend.report_merge_conflict(
                "awman/squad-nightly",
                Path::new("/wt/nightly"),
                Path::new("/repo"),
            );
        });
        assert!(
            conflict.contains("squad workflow left a merge conflict"),
            "{conflict}"
        );
        assert!(conflict.contains("WARN"), "{conflict}");
    }

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
            branch: "awman/squad".into(),
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

    /// A resize channel and an initial terminal size are what make the engine
    /// take the PTY path rather than piping the agent. Squad runs every agent
    /// PTY-backed whether or not anyone is attached (WI 0106 §3c): that is
    /// what makes the running agent a real interactive TUI process, and it is
    /// the prerequisite for `attach` connecting to the actual agent rather
    /// than a shell. The daemon owns no real terminal, so it supplies a fixed
    /// spacious default instead of measuring one.
    #[tokio::test]
    async fn agent_io_is_pty_backed_even_when_nobody_is_attached() {
        let mut frontend = UnattendedFrontend::new("c/leader".into());
        let io = frontend.take_io();
        assert!(
            io.resize.is_some(),
            "a PTY-backed agent needs a resize channel"
        );
        assert!(
            io.initial_size.is_some(),
            "an unattended PTY still needs a terminal size to allocate"
        );
    }

    #[tokio::test]
    async fn leader_and_workflow_container_output_is_written_to_each_run_log() {
        let tmp = tempfile::tempdir().unwrap();
        let run_id = RunId("run-0106".into());

        for (label, container) in [
            ("leader", "awman-squad-task-11111111"),
            ("workflow", "awman-squad-task-22222222"),
        ] {
            let run_dir = tmp.path().join(label);
            std::fs::create_dir(&run_dir).unwrap();
            let mut frontend = UnattendedFrontend::for_run(
                "task",
                &run_id,
                &run_dir,
                label,
                MountScopeDecision::MountGitRoot,
            )
            .unwrap();
            frontend.report_status(AgentStatus::Running {
                container_name: container.to_string(),
            });
            let io = frontend.take_io();
            io.stdout.send(b"stdout from agent\n".to_vec()).unwrap();
            io.stderr.send(b"stderr from agent\n".to_vec()).unwrap();
            drop(io);

            let log_path = run_dir.join(format!("{container}.log"));
            let mut contents = String::new();
            for _ in 0..40 {
                contents = std::fs::read_to_string(&log_path).unwrap_or_default();
                if contents.contains("stdout from agent") && contents.contains("stderr from agent")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert!(
                contents.contains("stdout from agent"),
                "{label}: {contents:?}"
            );
            assert!(
                contents.contains("stderr from agent"),
                "{label}: {contents:?}"
            );
        }
    }
}
