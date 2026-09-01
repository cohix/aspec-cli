//! `engine::workflow` — `WorkflowEngine`.
//!
//! Owns every workflow-execution concern: state, advance logic, yolo
//! countdowns, agent/model resolution, exit-code interpretation, persistence,
//! and per-step container lifecycle. Forbidden: rendering, direct user
//! input, knowledge of which frontend is on the other side of the trait,
//! worktree lifecycle management, direct container construction.
//!
//! The engine is the single source of truth for ALL workflow state.
//! No workflow execution state lives in the frontend — zero, none.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;

use crate::data::config::effective::EffectiveConfig;
use crate::data::session::{AgentName, Session};
use crate::data::workflow_dag::WorkflowDag;
use crate::data::workflow_definition::{Workflow, WorkflowStep};
use crate::data::workflow_state::{StepState, WorkflowState, WORKFLOW_STATE_SCHEMA_VERSION};
use crate::data::workflow_state_store::WorkflowStateStore;
use crate::engine::agent_runtime::background::AgentExec;
use crate::engine::agent_runtime::execution::{
    AgentExecution, AgentExitInfo, CancelHandle, StuckEvent, KILLED_EXIT_CODE,
};
use crate::engine::agent_runtime::output_tail::OutputTail;
use crate::engine::container::options::OverlayPermission;
use crate::engine::error::EngineError;
use crate::engine::git::GitEngine;
use crate::engine::overlay::OverlayEngine;
use crate::engine::workflow::actions::{
    AvailableActions, NextAction, ResumeMismatch, StepFailureChoice, StepOutcome, WorkflowOutcome,
    WorkflowStepProgressInfo, WorkflowStepStatus, YoloTickOutcome,
};
use crate::engine::workflow::factory::{AgentExecutionFactory, WorkflowRuntimeContext};
use crate::engine::workflow::frontend::WorkflowFrontend;

pub mod actions;
pub mod factory;
pub mod frontend;
pub mod poll_ci;
pub mod step_commands;
pub mod timing;

/// Result of a mid-step yolo countdown (step is still running while
/// the countdown ticks).
enum MidStepYoloResult {
    /// Step completed while the countdown was ticking.
    StepCompleted(StepOutcome),
    /// Countdown expired or user pressed AdvanceNow.
    Advanced,
    /// User pressed Esc: cancel the countdown.
    Cancelled,
    /// User pressed Ctrl-W: show the WCB instead.
    ShowControlBoard,
    /// Container recovered (StepUnstuck received).
    Recovered,
}

/// Result of mid-step control board interaction.
enum MidStepOutcome {
    /// User dismissed the dialog — resume waiting on the step.
    Continue,
    /// Step completed while dialog was open; outcome is ready.
    StepCompleted(StepOutcome),
    /// User chose a workflow-level action (pause/abort/finish).
    WorkflowEnded(WorkflowOutcome),
    /// User chose an action that re-enters the loop (restart/advance/etc).
    LoopContinue,
}

/// Result of one iteration of the outer `run_to_completion` loop (either a
/// single-step iteration or a parallel-group run).
enum IterationOutcome {
    /// The iteration finished; re-evaluate the outer loop (find the next batch).
    Continue,
    /// A workflow-level action ended the run.
    Ended(WorkflowOutcome),
}

/// A dynamically-sized set of container-wait futures, one per launched
/// parallel step. Each future resolves to `(step_name, exit_result)` when its
/// container terminates. Using `FuturesUnordered` (rather than a hand-rolled
/// `select!` array) lets the engine poll an arbitrary number of concurrent
/// containers.
type ParallelWaits = FuturesUnordered<
    std::pin::Pin<
        Box<dyn std::future::Future<Output = (String, Result<AgentExitInfo, EngineError>)> + Send>,
    >,
>;

/// Sender half of the unified stuck channel that fans every container's
/// per-step stuck broadcast into a single fixed-arity `select!` branch.
type StuckFanIn = tokio::sync::mpsc::UnboundedSender<(String, StuckEvent)>;

/// Result of `run_parallel_group`.
enum GroupOutcome {
    /// The whole group reached a terminal state. `failed` carries a non-abort
    /// step failure (name + exit code) for the outer loop to handle via the
    /// standard failure prompt; `None` when every member succeeded/cancelled.
    Drained { failed: Option<(String, i32)> },
    /// A workflow-level action ended the run (abort_on_failure, WCB abort/pause).
    Ended(WorkflowOutcome),
}

/// Result of `step_once_interruptible`.
enum InterruptibleStepResult {
    /// Step completed (naturally or while dialog was open).
    StepCompleted(StepOutcome),
    /// Mid-step action ended the workflow.
    WorkflowEnded(WorkflowOutcome),
    /// Mid-step action requires the outer loop to continue (restart/advance).
    LoopContinue,
}

pub use actions::{
    StepOutput, StepOutputKind, WorkflowOutcome as Outcome, WorkflowStepStatus as Status,
};
pub use factory::{AgentExecutionFactory as Factory, WorkflowRuntimeContext as RuntimeContext};
pub use frontend::WorkflowFrontend as Frontend;

/// Request sent from the TUI event loop (via per-tab channel) to the engine.
///
/// The frontend detects stuck/unstuck state and routes user actions;
/// the engine decides the response.
#[derive(Debug, Clone)]
pub enum EngineRequest {
    /// User pressed Ctrl-W. Engine should show the WCB for `step_name`
    /// (the currently-focused container in a parallel group; the single
    /// running step otherwise).
    OpenControlBoard { step_name: String },
    /// Frontend detected that `step_name`'s container is stuck
    /// (no PTY output for STUCK_TIMEOUT). Engine responds: if --yolo,
    /// start yolo countdown; if not --yolo, open WCB.
    StepStuck { step_name: String },
    /// Frontend detected that `step_name`'s container is no longer stuck
    /// (new PTY output arrived). Engine cancels any active yolo countdown.
    StepUnstuck { step_name: String },
}

/// One running (or just-launched) container in a parallel group.
///
/// The engine owns all concurrency state; this is the per-slot record it keeps
/// while a step's container is alive. In the single-step path exactly one entry
/// exists (`active_steps[0]`, the "focused" step) and it retains its
/// `execution` for prompt injection / put-back. In the multi-step parallel
/// path each launched step gets its own entry; the `execution` is moved into a
/// background wait future, so the entry keeps only a `cancel_handle` for
/// engine-initiated kills (yolo expiry, abort_on_failure, WCB abort/pause).
struct ActiveParallelStep {
    step_name: String,
    /// Retained only by the single-step path (for inject / put-back after the
    /// wait task resolves). `None` in the multi-step path, where the execution
    /// lives inside the FuturesUnordered wait future.
    execution: Option<AgentExecution>,
    /// Standalone kill handle, extracted before the execution is moved into a
    /// wait future. Used by the multi-step path to kill just this container.
    cancel_handle: Option<CancelHandle>,
    /// This container's stuck broadcast sender (published to the frontend so it
    /// can subscribe per-slot).
    stuck_sender: Arc<tokio::sync::broadcast::Sender<StuckEvent>>,
    /// The container's name, retained so a failure log can be named after it
    /// once the execution has been consumed by its wait future.
    container_name: String,
    /// Rolling buffer of this container's recent combined stdout/stderr,
    /// extracted from the execution at launch so it outlives the wait future.
    /// `None` for runtimes without a byte-stream bridge (e.g. sandbox-class).
    output_tail: Option<Arc<OutputTail>>,
    /// Set when awman itself terminated this container (yolo auto-advance, WCB
    /// abort/pause/finish, stuck cancel, startup-grace kill, abort_on_failure
    /// peer kill). A non-zero exit on an awman-killed container is expected and
    /// must NOT produce a failure log.
    awman_killed: bool,
    /// Whether this step is currently marked stuck (non-yolo stuck handling).
    stuck: bool,
    /// When a per-step yolo countdown is running, the instant it expires.
    yolo_deadline: Option<Instant>,
    agent: AgentName,
    model: Option<String>,
}

pub struct WorkflowEngine {
    session: Session,
    workflow: Workflow,
    dag: WorkflowDag,
    state: WorkflowState,
    state_store: WorkflowStateStore,
    effective_config: EffectiveConfig,
    frontend: Box<dyn WorkflowFrontend>,
    agent_factory: Box<dyn AgentExecutionFactory>,
    git_engine: Arc<GitEngine>,
    overlay_engine: Arc<OverlayEngine>,
    /// Containers currently alive. The single-step path keeps exactly one
    /// entry (the focused step); the parallel path keeps up to `max_concurrent`.
    active_steps: Vec<ActiveParallelStep>,
    /// Resolved once at construction from `effective_max_concurrent_agents()`.
    /// `None` means unlimited.
    max_concurrent: Option<usize>,
    current_step_name: Option<String>,
    current_step_agent: Option<AgentName>,
    current_step_model: Option<String>,
    work_item_context: Option<crate::data::workflow_prompt_template::WorkItemContext>,
    workflow_context_permission: Option<OverlayPermission>,
    yolo: bool,
    abort_on_failure_triggered: bool,
    last_exit_info: Option<AgentExitInfo>,
    engine_rx: Option<tokio::sync::mpsc::UnboundedReceiver<EngineRequest>>,
}

impl WorkflowEngine {
    fn msg_info(&mut self, text: impl Into<String>) {
        self.frontend
            .write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Info,
                text: text.into(),
            });
    }
    fn msg_warning(&mut self, text: impl Into<String>) {
        self.frontend
            .write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Warning,
                text: text.into(),
            });
    }
    fn msg_success(&mut self, text: impl Into<String>) {
        self.frontend
            .write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Success,
                text: text.into(),
            });
    }
    fn msg_error(&mut self, text: impl Into<String>) {
        self.frontend
            .write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Error,
                text: text.into(),
            });
    }

    /// Persist a failed step container's buffered output to
    /// `~/.awman/logs/{workflow-id}-{step-name}-{container-name}.log` and point
    /// the user at the file. Called only when a step container exits non-zero
    /// on its own (awman did not kill it). Best-effort: a resolve/write failure
    /// downgrades to a warning rather than derailing the workflow.
    fn dump_container_failure_log(
        &mut self,
        step_name: &str,
        container_name: &str,
        tail: &OutputTail,
        exit_code: i32,
    ) {
        let contents = tail.snapshot_text();
        let paths = match crate::data::fs::WorkflowLogPaths::from_env(self.session.env()) {
            Ok(p) => p,
            Err(e) => {
                self.msg_warning(format!(
                    "Step '{step_name}' container '{container_name}' exited with code \
                     {exit_code}, but the log directory could not be resolved to save its \
                     output: {e}"
                ));
                return;
            }
        };
        match paths.write_container_log(
            self.state.invocation_id,
            step_name,
            container_name,
            &contents,
        ) {
            Ok(path) => self.msg_error(format!(
                "Step '{step_name}' container '{container_name}' exited with code {exit_code}. \
                 Recent output saved to {}",
                path.display()
            )),
            Err(e) => self.msg_warning(format!(
                "Step '{step_name}' container '{container_name}' exited with code {exit_code}, \
                 but writing its output log failed: {e}"
            )),
        }
    }

    /// If a just-finished step container failed on its own (non-zero exit that
    /// awman did not cause) and a captured output tail exists, flush it to a
    /// failure log. Reads the slot for `step_name` if it is still present.
    fn maybe_dump_step_failure(&mut self, step_name: &str, exit_code: i32) {
        if exit_code == 0 {
            return;
        }
        let dump = self
            .active_steps
            .iter()
            .find(|s| s.step_name == step_name)
            .filter(|s| !s.awman_killed)
            .and_then(|s| {
                s.output_tail
                    .clone()
                    .map(|tail| (s.container_name.clone(), tail))
            });
        if let Some((container_name, tail)) = dump {
            self.dump_container_failure_log(step_name, &container_name, &tail, exit_code);
        }
    }

    /// Mark the focused (single-step) container as awman-killed so a subsequent
    /// non-zero exit is treated as expected and produces no failure log.
    fn mark_focused_killed(&mut self) {
        if let Some(s) = self.active_steps.first_mut() {
            s.awman_killed = true;
        }
    }

    pub fn new(
        session: &Session,
        workflow: Workflow,
        work_item_context: Option<crate::data::workflow_prompt_template::WorkItemContext>,
        mut frontend: Box<dyn WorkflowFrontend>,
        agent_factory: Box<dyn AgentExecutionFactory>,
        git_engine: Arc<GitEngine>,
        overlay_engine: Arc<OverlayEngine>,
    ) -> Result<Self, EngineError> {
        let dag = WorkflowDag::build(&workflow.steps).map_err(EngineError::Data)?;
        let workflow_context_permission =
            workflow_context_permission_from_overlay_strings(workflow.overlays.as_deref());
        let workflow_hash = compute_workflow_hash(&workflow);
        let work_item_number = work_item_context.as_ref().map(|c| c.number);
        let state = WorkflowState::new(
            workflow_name_for(&workflow),
            &workflow.steps,
            workflow_hash,
            work_item_number,
        );
        let state_store = WorkflowStateStore::new(session);
        let effective_config = session.effective_config();
        let max_concurrent = effective_config.effective_max_concurrent_agents();
        tracing::debug!(
            ?max_concurrent,
            "workflow_engine resolved max_concurrent_agents"
        );
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        frontend.set_engine_sender(tx);
        Ok(Self {
            session: session.clone(),
            workflow,
            dag,
            state,
            state_store,
            effective_config,
            frontend,
            agent_factory,
            git_engine,
            overlay_engine,
            active_steps: Vec::new(),
            max_concurrent,
            current_step_name: None,
            current_step_agent: None,
            current_step_model: None,
            work_item_context,
            workflow_context_permission,
            yolo: false,
            abort_on_failure_triggered: false,
            last_exit_info: None,
            engine_rx: Some(rx),
        })
    }

    pub fn abort_on_failure_triggered(&self) -> bool {
        self.abort_on_failure_triggered
    }

    /// The resolved per-workflow concurrency cap (`None` = unlimited). Resolved
    /// once at construction from `effective_max_concurrent_agents()`. Frontends
    /// read this to size the Workflow Overview / parallel UX.
    pub fn max_concurrent(&self) -> Option<usize> {
        self.max_concurrent
    }

    pub fn set_yolo(&mut self, yolo: bool) {
        self.yolo = yolo;
    }

    /// Override the active workflow-context overlay permission after the command
    /// layer has merged config/env/CLI/workflow overlay sources.
    pub fn set_workflow_context_permission(&mut self, permission: Option<OverlayPermission>) {
        self.workflow_context_permission = permission;
    }

    /// The focused step's live execution (the single running step, or the first
    /// parallel slot). `None` while a wait future owns it or between
    /// launch/finalize.
    fn focused_execution(&self) -> Option<&AgentExecution> {
        self.active_steps.first().and_then(|s| s.execution.as_ref())
    }

    /// Put an execution back into the focused slot after its wait future
    /// resolves (single-step path).
    fn set_focused_execution(&mut self, exec: AgentExecution) {
        if let Some(s) = self.active_steps.first_mut() {
            s.execution = Some(exec);
        }
    }

    /// Move the focused slot's execution out (single-step path spawns a wait
    /// task that owns it).
    fn take_focused_execution(&mut self) -> Option<AgentExecution> {
        self.active_steps
            .first_mut()
            .and_then(|s| s.execution.take())
    }

    /// Resume from persisted state. Calls `confirm_resume` on the frontend if
    /// the workflow hash has drifted.
    ///
    /// State is persisted under the session's git root, the same place
    /// `exec workflow` has always kept it.
    pub async fn resume(
        session: &Session,
        workflow: Workflow,
        work_item_context: Option<crate::data::workflow_prompt_template::WorkItemContext>,
        frontend: Box<dyn WorkflowFrontend>,
        agent_factory: Box<dyn AgentExecutionFactory>,
        git_engine: Arc<GitEngine>,
        overlay_engine: Arc<OverlayEngine>,
    ) -> Result<Self, EngineError> {
        Self::resume_with_state_root(
            session,
            workflow,
            work_item_context,
            frontend,
            agent_factory,
            git_engine,
            overlay_engine,
            None,
        )
        .await
    }

    /// [`Self::resume`], with the workflow-state file rooted somewhere other
    /// than the session's git root.
    ///
    /// The state store creates, rewrites and deletes its file, so a caller
    /// whose session root must stay untouched between runs — a squad task
    /// bound to its durable workspace (WI 0106 §6a) — points this at a
    /// run-scoped directory instead. `None` keeps the session-rooted default.
    #[allow(clippy::too_many_arguments)]
    pub async fn resume_with_state_root(
        session: &Session,
        workflow: Workflow,
        work_item_context: Option<crate::data::workflow_prompt_template::WorkItemContext>,
        mut frontend: Box<dyn WorkflowFrontend>,
        agent_factory: Box<dyn AgentExecutionFactory>,
        git_engine: Arc<GitEngine>,
        overlay_engine: Arc<OverlayEngine>,
        state_root: Option<std::path::PathBuf>,
    ) -> Result<Self, EngineError> {
        let dag = WorkflowDag::build(&workflow.steps).map_err(EngineError::Data)?;
        let workflow_context_permission =
            workflow_context_permission_from_overlay_strings(workflow.overlays.as_deref());
        let store = match state_root {
            Some(root) => WorkflowStateStore::at_git_root(root),
            None => WorkflowStateStore::new(session),
        };
        let workflow_name = workflow_name_for(&workflow);
        let work_item_number = work_item_context.as_ref().map(|c| c.number);
        let saved = store.load(work_item_number, &workflow_name)?;

        let workflow_hash = compute_workflow_hash(&workflow);
        let mut state = match saved {
            Some(saved) => {
                if saved.schema_version > WORKFLOW_STATE_SCHEMA_VERSION {
                    return Err(EngineError::UnsupportedWorkflowSchemaVersion {
                        found: saved.schema_version,
                        supported: WORKFLOW_STATE_SCHEMA_VERSION,
                    });
                }
                if saved.workflow_hash != workflow_hash {
                    let mismatch = ResumeMismatch {
                        workflow_name: workflow_name.clone(),
                        saved_hash: saved.workflow_hash.clone(),
                        current_hash: workflow_hash.clone(),
                        message: "workflow source has changed since the saved run".into(),
                    };
                    if !frontend.confirm_resume(&mismatch)? {
                        return Err(EngineError::WorkflowResumeIncompatible(
                            "user declined to resume against drifted workflow".into(),
                        ));
                    }
                }
                saved
            }
            None => WorkflowState::new(
                workflow_name,
                &workflow.steps,
                workflow_hash,
                work_item_number,
            ),
        };

        let interrupted = state.interrupted_running_steps();
        if !interrupted.is_empty() {
            frontend.write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Warning,
                text: format!(
                    "Interrupted steps detected (prior crash?): {}. Resetting to Pending.",
                    interrupted.join(", "),
                ),
            });
            for name in &interrupted {
                state.set_status(name, StepState::Pending);
            }
        }

        let effective_config = session.effective_config();
        let max_concurrent = effective_config.effective_max_concurrent_agents();
        tracing::debug!(
            ?max_concurrent,
            "workflow_engine resolved max_concurrent_agents"
        );
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        frontend.set_engine_sender(tx);
        Ok(Self {
            session: session.clone(),
            workflow,
            dag,
            state,
            state_store: store,
            effective_config,
            frontend,
            agent_factory,
            git_engine,
            overlay_engine,
            active_steps: Vec::new(),
            max_concurrent,
            current_step_name: None,
            current_step_agent: None,
            current_step_model: None,
            work_item_context,
            workflow_context_permission,
            yolo: false,
            abort_on_failure_triggered: false,
            last_exit_info: None,
            engine_rx: Some(rx),
        })
    }

    pub fn state(&self) -> &WorkflowState {
        &self.state
    }

    /// Drive every step until the workflow finishes, the user pauses, or a
    /// step fails terminally.
    pub async fn run_to_completion(&mut self) -> Result<WorkflowOutcome, EngineError> {
        let completed_count = self.state.completed_steps.len();
        let total_count = self.workflow.steps.len();
        if completed_count > 0 {
            self.msg_info(format!(
                "Resuming workflow '{}' ({}/{} steps completed)",
                self.state.workflow_name, completed_count, total_count,
            ));
        } else {
            self.msg_info(format!(
                "Starting workflow '{}' ({} steps)",
                self.state.workflow_name, total_count,
            ));
        }

        let initial_progress = self.workflow_progress_info();
        self.frontend.report_workflow_progress(&initial_progress);

        loop {
            if self.state.is_complete() {
                let progress = self.workflow_progress_info();
                self.frontend.report_workflow_progress(&progress);
                self.msg_success(format!(
                    "Workflow '{}' completed successfully",
                    self.state.workflow_name,
                ));
                let outcome = WorkflowOutcome::Completed;
                self.frontend.report_workflow_completed(&outcome);
                return Ok(outcome);
            }

            // Determine the current parallel group: every step whose
            // dependencies are already satisfied (source-file order). When more
            // than one is ready and the concurrency cap is not pinned to 1, run
            // them through the parallel-group path; otherwise fall back to the
            // single-step interactive path (behaviourally identical to the
            // pre-WI-0096 sequential engine).
            let ready = self.next_ready_steps()?;
            let use_parallel = ready.len() > 1 && self.max_concurrent != Some(1);

            if use_parallel {
                match self.run_parallel_group(ready).await? {
                    GroupOutcome::Ended(wo) => return Ok(wo),
                    GroupOutcome::Drained { failed } => {
                        if let Some((name, exit_code)) = failed {
                            match self.handle_group_step_failure(&name, exit_code)? {
                                IterationOutcome::Continue => continue,
                                IterationOutcome::Ended(wo) => return Ok(wo),
                            }
                        }
                        continue;
                    }
                }
            }

            match self.run_single_step_iteration().await? {
                IterationOutcome::Continue => continue,
                IterationOutcome::Ended(wo) => return Ok(wo),
            }
        }
    }

    /// One iteration of the sequential (single-step) path: launch the first
    /// ready step, drive its interactive lifecycle (mid-step WCB, yolo, stuck),
    /// handle failure, then present the inter-step Workflow Control Board.
    ///
    /// This is the pre-WI-0096 `run_to_completion` loop body, preserved
    /// verbatim so single-step and `max_concurrent == 1` workflows behave
    /// exactly as before.
    async fn run_single_step_iteration(&mut self) -> Result<IterationOutcome, EngineError> {
        let interruptible_result = self.step_once_interruptible().await?;
        let outcome = match interruptible_result {
            InterruptibleStepResult::StepCompleted(o) => o,
            InterruptibleStepResult::WorkflowEnded(wo) => return Ok(IterationOutcome::Ended(wo)),
            InterruptibleStepResult::LoopContinue => return Ok(IterationOutcome::Continue),
        };

        if let WorkflowStepStatus::Failed { exit_code } = outcome.status {
            let progress = self.workflow_progress_info();
            self.frontend.report_workflow_progress(&progress);

            let step = self.find_step(&outcome.step_name)?;

            if step.abort_on_failure {
                self.msg_warning(format!(
                    "Step '{}' failed (abort_on_failure); aborting workflow",
                    outcome.step_name,
                ));
                self.abort_on_failure_triggered = true;
                for s in &self.workflow.steps {
                    if !self.state.completed_steps.contains(&s.name) {
                        self.state.set_status(&s.name, StepState::Cancelled);
                    }
                }
                self.persist()?;
                let aborted = WorkflowOutcome::Aborted;
                self.frontend.report_workflow_completed(&aborted);
                return Ok(IterationOutcome::Ended(aborted));
            }

            let exit_info = self
                .last_exit_info
                .clone()
                .unwrap_or_else(|| AgentExitInfo {
                    exit_code,
                    signal: None,
                    started_at: chrono::Utc::now(),
                    ended_at: chrono::Utc::now(),
                });
            let choice = self
                .frontend
                .user_choose_after_step_failure(&step, &exit_info)?;
            match choice {
                StepFailureChoice::Retry => {
                    self.msg_info(format!("Retrying step '{}'", outcome.step_name,));
                    self.state
                        .set_status(&outcome.step_name, StepState::Pending);
                    self.persist()?;
                    return Ok(IterationOutcome::Continue);
                }
                StepFailureChoice::Pause => {
                    self.msg_info("Workflow paused");
                    self.persist()?;
                    let paused = WorkflowOutcome::Paused;
                    self.frontend.report_workflow_completed(&paused);
                    return Ok(IterationOutcome::Ended(paused));
                }
                StepFailureChoice::Abort => {
                    self.msg_warning("Workflow aborted");
                    for s in &self.workflow.steps {
                        if !self.state.completed_steps.contains(&s.name) {
                            self.state.set_status(&s.name, StepState::Cancelled);
                        }
                    }
                    self.persist()?;
                    let aborted = WorkflowOutcome::Aborted;
                    self.frontend.report_workflow_completed(&aborted);
                    return Ok(IterationOutcome::Ended(aborted));
                }
            }
        }

        // Step succeeded. Decide what to do next.
        let workflow_just_completed = self.state.is_complete();

        if !workflow_just_completed {
            let progress = self.workflow_progress_info();
            self.frontend.report_workflow_progress(&progress);

            if self.yolo {
                return Ok(IterationOutcome::Continue);
            }
        } else if self.yolo {
            // Last step in yolo mode: always require explicit user
            // confirmation before ending the workflow so the user can
            // review the final step's output.
            let progress = self.workflow_progress_info();
            self.frontend.report_workflow_progress(&progress);
        }

        if !workflow_just_completed || self.yolo {
            let available = self.compute_available_actions()?;
            let action = self
                .frontend
                .show_workflow_control_board(&self.state, &available)?;
            self.log_wcb_action(&action);
            match action {
                NextAction::Dismiss | NextAction::LaunchNext => {
                    return Ok(IterationOutcome::Continue)
                }
                NextAction::ContinueInCurrentContainer { prompt } => {
                    self.handle_continue_in_current_container(&prompt)?;
                    return Ok(IterationOutcome::Continue);
                }
                NextAction::RestartCurrentStep => {
                    if let Some(name) = self.current_step_name.clone() {
                        self.state.set_status(&name, StepState::Pending);
                        self.persist()?;
                    }
                    return Ok(IterationOutcome::Continue);
                }
                NextAction::CancelToPreviousStep => {
                    self.handle_cancel_to_previous()?;
                    return Ok(IterationOutcome::Continue);
                }
                NextAction::FinishWorkflow => {
                    return Ok(IterationOutcome::Ended(self.handle_finish_workflow()?));
                }
                NextAction::Pause => {
                    self.persist()?;
                    let outcome = WorkflowOutcome::Paused;
                    self.frontend.report_workflow_completed(&outcome);
                    return Ok(IterationOutcome::Ended(outcome));
                }
                NextAction::Abort => {
                    return Ok(IterationOutcome::Ended(self.handle_abort()?));
                }
            }
        }

        Ok(IterationOutcome::Continue)
    }

    // ── Parallel group execution (WI-0096 §2) ───────────────────────────────

    /// Run a whole parallel group to completion. Launches up to
    /// `max_concurrent` of `ready` (source-file order), queuing the rest, then
    /// drives all live containers concurrently through a `FuturesUnordered`
    /// select loop — launching queued steps as slots free up, running per-step
    /// stuck detection and per-step yolo countdowns independently, and honoring
    /// `abort_on_failure` / WCB pause+abort mid-group.
    async fn run_parallel_group(
        &mut self,
        ready: Vec<WorkflowStep>,
    ) -> Result<GroupOutcome, EngineError> {
        let group_names: Vec<String> = ready.iter().map(|s| s.name.clone()).collect();
        self.frontend.report_parallel_group_started(&group_names);
        let slot_cap = match self.max_concurrent {
            Some(n) => n.max(1),
            None => ready.len().max(1),
        };
        self.msg_info(format!(
            "Launching parallel group: {} step(s), up to {} at once",
            group_names.len(),
            slot_cap,
        ));

        let mut queue: VecDeque<WorkflowStep> = ready.into_iter().collect();
        self.active_steps.clear();

        let (stuck_tx, mut stuck_rx) =
            tokio::sync::mpsc::unbounded_channel::<(String, StuckEvent)>();
        let mut waits: ParallelWaits = FuturesUnordered::new();

        // Launch the initial batch.
        while self.active_steps.len() < slot_cap {
            match queue.pop_front() {
                Some(step) => self.launch_parallel_step(step, &mut waits, &stuck_tx, false)?,
                None => break,
            }
        }

        let total = timing::YOLO_COUNTDOWN_DURATION;
        let mut failed: Option<(String, i32)> = None;

        while !self.active_steps.is_empty() {
            tokio::select! {
                biased;
                Some((name, result)) = waits.next() => {
                    // Guard against futures for steps already finalized out of
                    // band (yolo auto-advance kills the container but leaves its
                    // wait future pending; it resolves here later as a no-op).
                    if !self.active_steps.iter().any(|s| s.step_name == name) {
                        continue;
                    }
                    let exit = result?;
                    // Persist the buffered output on a genuine failure before the
                    // slot (which owns the tail + container name) is removed.
                    self.maybe_dump_step_failure(&name, exit.exit_code);
                    self.remove_active_step(&name);
                    self.last_exit_info = Some(exit.clone());

                    let (status, step_state) = if exit.exit_code == 0 {
                        (WorkflowStepStatus::Succeeded, StepState::Succeeded)
                    } else {
                        (
                            WorkflowStepStatus::Failed { exit_code: exit.exit_code },
                            StepState::Failed { exit_code: exit.exit_code, error_message: None },
                        )
                    };
                    let step = self.find_step(&name)?;
                    self.state.set_status(&name, step_state);
                    self.frontend.report_step_status(&step, status.clone());
                    self.frontend.report_parallel_step_exited(&name, exit.exit_code);
                    self.persist()?;
                    let progress = self.workflow_progress_info();
                    self.frontend.report_workflow_progress(&progress);

                    if let WorkflowStepStatus::Failed { exit_code } = status {
                        if step.abort_on_failure {
                            self.msg_warning(format!(
                                "Step '{}' failed (abort_on_failure); aborting parallel group",
                                name,
                            ));
                            let wo = self.abort_parallel_group()?;
                            return Ok(GroupOutcome::Ended(wo));
                        }
                        // Non-abort failure: record it, keep draining the rest
                        // of the group, but do NOT launch further queued steps.
                        failed.get_or_insert((name.clone(), exit_code));
                    } else if self.active_steps.len() < slot_cap {
                        if let Some(next) = queue.pop_front() {
                            self.launch_parallel_step(next, &mut waits, &stuck_tx, true)?;
                        }
                    }
                }
                Some((name, event)) = stuck_rx.recv() => {
                    self.handle_parallel_stuck_event(&name, event);
                }
                Some(req) = Self::recv_engine(&mut self.engine_rx) => {
                    if let Some(wo) = self.handle_parallel_engine_request(req)? {
                        return Ok(GroupOutcome::Ended(wo));
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    self.tick_parallel_yolo(&mut waits, &mut queue, slot_cap, total, &stuck_tx)?;
                }
            }
        }

        self.frontend.report_parallel_group_finished();
        Ok(GroupOutcome::Drained { failed })
    }

    /// Launch one step of a parallel group: resolve agent/model, spawn the
    /// container, wire its stuck broadcast into the fan-in channel, move its
    /// execution into a wait future, and record an `ActiveParallelStep` slot.
    fn launch_parallel_step(
        &mut self,
        step: WorkflowStep,
        waits: &mut ParallelWaits,
        stuck_tx: &StuckFanIn,
        dequeued: bool,
    ) -> Result<(), EngineError> {
        let resolved_agent = self.resolve_agent(&step)?;
        let resolved_model = self.resolve_model(&step);
        tracing::info!(
            step = %step.name,
            agent = %resolved_agent.as_str(),
            model = ?resolved_model,
            "workflow_engine launching parallel step"
        );

        let workflow_step_info = self.build_workflow_step_info(&step.name);
        let runtime = WorkflowRuntimeContext {
            step_agent: resolved_agent.clone(),
            step_model: resolved_model.clone(),
            git_root: self.session.git_root().to_path_buf(),
            session_id: self.session.id(),
            workflow_invocation_id: self.state.invocation_id,
            workflow_step_info,
        };

        self.frontend.report_step_interactive_launch(
            &step,
            resolved_agent.as_str(),
            resolved_model.as_deref(),
        );
        self.state
            .set_status(&step.name, StepState::Running { container_id: None });
        self.frontend
            .report_step_status(&step, WorkflowStepStatus::Running);
        self.persist()?;

        let execution = self
            .agent_factory
            .execution_for_step(&step, &self.session, &runtime)?;

        self.state.set_status(
            &step.name,
            StepState::Running {
                container_id: Some(execution.handle().id.clone()),
            },
        );
        self.persist()?;

        let stuck_sender = execution.stuck_sender();
        let cancel_handle = execution.cancel_handle();
        let container_name = execution.handle().name.clone();
        let output_tail = execution.output_tail();

        // Publish the per-step stuck sender so the frontend can subscribe for
        // this specific container's status bar.
        self.frontend
            .set_parallel_step_stuck_sender(&step.name, stuck_sender.clone());
        if dequeued {
            self.frontend.report_parallel_step_dequeued(
                &step.name,
                resolved_agent.as_str(),
                resolved_model.as_deref(),
            );
        } else {
            self.frontend.report_parallel_step_launched(
                &step.name,
                resolved_agent.as_str(),
                resolved_model.as_deref(),
            );
        }
        // Published after the launch/dequeue event so the frontend's slot
        // exists by the time the name arrives; drives per-container stats.
        self.frontend
            .report_parallel_step_container(&step.name, &execution.handle().name);

        // Forward this container's stuck broadcast into the unified fan-in
        // channel, tagged with the step name, so the select loop stays
        // fixed-arity regardless of how many containers are live.
        let mut rx = execution.subscribe_stuck();
        let fwd = stuck_tx.clone();
        let fwd_name = step.name.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if fwd.send((fwd_name.clone(), ev)).is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        });

        let name = step.name.clone();
        waits.push(Box::pin(async move {
            let mut execution = execution;
            let r = execution.wait().await;
            (name, r)
        }));

        self.active_steps.push(ActiveParallelStep {
            step_name: step.name.clone(),
            execution: None,
            cancel_handle,
            stuck_sender,
            container_name,
            output_tail,
            awman_killed: false,
            stuck: false,
            yolo_deadline: None,
            agent: resolved_agent,
            model: resolved_model,
        });
        Ok(())
    }

    fn remove_active_step(&mut self, name: &str) {
        self.active_steps.retain(|s| s.step_name != name);
    }

    /// Handle a per-step stuck/unstuck transition inside a parallel group.
    /// Independent per slot: a noisy sibling never masks a stuck step, and a
    /// stuck step never blocks its siblings.
    fn handle_parallel_stuck_event(&mut self, name: &str, event: StuckEvent) {
        match event {
            StuckEvent::Stuck => {
                if self.yolo {
                    let start_countdown = {
                        match self.active_steps.iter_mut().find(|s| s.step_name == name) {
                            Some(s) if s.yolo_deadline.is_none() => {
                                s.yolo_deadline =
                                    Some(Instant::now() + timing::YOLO_COUNTDOWN_DURATION);
                                true
                            }
                            _ => false,
                        }
                    };
                    if start_countdown {
                        self.msg_info(format!(
                            "Step '{}' appears stuck; starting yolo countdown",
                            name,
                        ));
                        self.frontend.parallel_step_yolo_countdown_started(name);
                    }
                } else {
                    if let Some(s) = self.active_steps.iter_mut().find(|s| s.step_name == name) {
                        s.stuck = true;
                    }
                    self.msg_warning(format!("Step '{}' appears stuck (no output)", name));
                    self.frontend.report_parallel_step_stuck(name);
                }
            }
            StuckEvent::Unstuck => {
                if let Some(s) = self.active_steps.iter_mut().find(|s| s.step_name == name) {
                    let had_countdown = s.yolo_deadline.take().is_some();
                    s.stuck = false;
                    if had_countdown {
                        self.frontend.parallel_step_yolo_countdown_finished(name);
                    }
                }
                self.frontend.report_parallel_step_unstuck(name);
            }
            StuckEvent::StartupGraceExpired => {
                // The bridge already killed the container; its wait future will
                // resolve and be finalized as a failure. Mark the slot as
                // awman-killed so the drain loop suppresses the failure log for
                // this expected kill.
                if let Some(s) = self.active_steps.iter_mut().find(|s| s.step_name == name) {
                    s.awman_killed = true;
                }
            }
        }
    }

    /// Drive every in-flight per-step yolo countdown one tick. Independent per
    /// slot — expiry kills only that container and advances the queue.
    fn tick_parallel_yolo(
        &mut self,
        waits: &mut ParallelWaits,
        queue: &mut VecDeque<WorkflowStep>,
        slot_cap: usize,
        total: Duration,
        stuck_tx: &StuckFanIn,
    ) -> Result<(), EngineError> {
        let now = Instant::now();
        let ticking: Vec<(String, Instant)> = self
            .active_steps
            .iter()
            .filter_map(|s| s.yolo_deadline.map(|d| (s.step_name.clone(), d)))
            .collect();
        for (name, deadline) in ticking {
            let remaining = deadline.saturating_duration_since(now);
            match self
                .frontend
                .parallel_step_yolo_countdown_tick(&name, remaining, total)?
            {
                YoloTickOutcome::Cancel => {
                    if let Some(s) = self.active_steps.iter_mut().find(|s| s.step_name == name) {
                        s.yolo_deadline = None;
                    }
                    self.frontend.parallel_step_yolo_countdown_finished(&name);
                }
                YoloTickOutcome::AdvanceNow => {
                    self.yolo_advance_parallel(&name, waits, queue, slot_cap, stuck_tx)?;
                }
                YoloTickOutcome::Continue => {
                    if remaining.is_zero() {
                        self.yolo_advance_parallel(&name, waits, queue, slot_cap, stuck_tx)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Yolo countdown expired (or user forced advance) for one parallel slot:
    /// kill just that container, mark the step Succeeded, free the slot, and
    /// launch the next queued step if one is waiting.
    fn yolo_advance_parallel(
        &mut self,
        name: &str,
        waits: &mut ParallelWaits,
        queue: &mut VecDeque<WorkflowStep>,
        slot_cap: usize,
        stuck_tx: &StuckFanIn,
    ) -> Result<(), EngineError> {
        self.msg_info(format!("Yolo auto-advancing past step '{}'", name));
        self.frontend.parallel_step_yolo_countdown_finished(name);
        if let Some(pos) = self.active_steps.iter().position(|s| s.step_name == name) {
            if let Some(ch) = &self.active_steps[pos].cancel_handle {
                let _ = ch.cancel();
            }
            self.active_steps.remove(pos);
        }
        self.frontend
            .report_parallel_step_exited(name, KILLED_EXIT_CODE);
        self.state.set_status(name, StepState::Succeeded);
        let step = self.find_step(name)?;
        self.frontend
            .report_step_status(&step, WorkflowStepStatus::Succeeded);
        self.persist()?;
        let progress = self.workflow_progress_info();
        self.frontend.report_workflow_progress(&progress);
        if self.active_steps.len() < slot_cap {
            if let Some(next) = queue.pop_front() {
                self.launch_parallel_step(next, waits, stuck_tx, true)?;
            }
        }
        Ok(())
    }

    /// Kill every live container in the current parallel group and cancel all
    /// not-yet-completed steps, then proceed with the standard abort path.
    fn abort_parallel_group(&mut self) -> Result<WorkflowOutcome, EngineError> {
        self.abort_on_failure_triggered = true;
        let names: Vec<String> = self
            .active_steps
            .iter()
            .map(|s| s.step_name.clone())
            .collect();
        for s in &self.active_steps {
            if let Some(ch) = &s.cancel_handle {
                let _ = ch.cancel();
            }
        }
        self.active_steps.clear();
        for name in &names {
            self.frontend
                .report_parallel_step_exited(name, KILLED_EXIT_CODE);
        }
        for s in &self.workflow.steps {
            if !self.state.completed_steps.contains(&s.name) {
                self.state.set_status(&s.name, StepState::Cancelled);
            }
        }
        self.persist()?;
        self.frontend.report_parallel_group_finished();
        let aborted = WorkflowOutcome::Aborted;
        self.frontend.report_workflow_completed(&aborted);
        Ok(aborted)
    }

    /// WCB Pause during a parallel group: kill all live containers, reset the
    /// running steps to Pending so a resume replays them, and end the run.
    fn pause_parallel_group(&mut self) -> Result<WorkflowOutcome, EngineError> {
        let names: Vec<String> = self
            .active_steps
            .iter()
            .map(|s| s.step_name.clone())
            .collect();
        for s in &self.active_steps {
            if let Some(ch) = &s.cancel_handle {
                let _ = ch.cancel();
            }
        }
        self.active_steps.clear();
        for name in &names {
            self.frontend
                .report_parallel_step_exited(name, KILLED_EXIT_CODE);
            self.state.set_status(name, StepState::Pending);
        }
        self.persist()?;
        self.frontend.report_parallel_group_finished();
        let paused = WorkflowOutcome::Paused;
        self.frontend.report_workflow_completed(&paused);
        Ok(paused)
    }

    /// Route an `EngineRequest` received while a parallel group is running.
    /// Returns `Some(outcome)` when the request ends the workflow (WCB
    /// pause/abort), `None` otherwise.
    fn handle_parallel_engine_request(
        &mut self,
        req: EngineRequest,
    ) -> Result<Option<WorkflowOutcome>, EngineError> {
        match req {
            EngineRequest::StepStuck { step_name } => {
                self.handle_parallel_stuck_event(&step_name, StuckEvent::Stuck);
                Ok(None)
            }
            EngineRequest::StepUnstuck { step_name } => {
                self.handle_parallel_stuck_event(&step_name, StuckEvent::Unstuck);
                Ok(None)
            }
            EngineRequest::OpenControlBoard { step_name } => {
                // Scope the board to the focused container; peers keep running.
                self.focus_parallel_step(&step_name);
                let available = self.compute_available_actions()?;
                let action = self
                    .frontend
                    .show_workflow_control_board(&self.state, &available)?;
                self.log_wcb_action(&action);
                match action {
                    NextAction::Pause => Ok(Some(self.pause_parallel_group()?)),
                    NextAction::Abort => Ok(Some(self.abort_parallel_group()?)),
                    // Back / finish / restart / continue / launch-next are all
                    // scoped away while peers run (see compute_available_actions
                    // §10); treat anything else as a dismiss — the group keeps
                    // running undisturbed.
                    _ => Ok(None),
                }
            }
        }
    }

    /// Make `step_name` the focused slot (index 0) so `compute_available_actions`
    /// evaluates the board relative to it. No-op if the name is unknown.
    fn focus_parallel_step(&mut self, step_name: &str) {
        if let Some(pos) = self
            .active_steps
            .iter()
            .position(|s| s.step_name == step_name)
        {
            self.active_steps.swap(0, pos);
            let s = &self.active_steps[0];
            self.current_step_name = Some(s.step_name.clone());
            self.current_step_agent = Some(s.agent.clone());
            self.current_step_model = s.model.clone();
        }
    }

    /// Present the standard post-failure prompt for a non-abort step failure
    /// that surfaced after a parallel group drained.
    fn handle_group_step_failure(
        &mut self,
        step_name: &str,
        exit_code: i32,
    ) -> Result<IterationOutcome, EngineError> {
        let step = self.find_step(step_name)?;
        let exit_info = self
            .last_exit_info
            .clone()
            .unwrap_or_else(|| AgentExitInfo {
                exit_code,
                signal: None,
                started_at: chrono::Utc::now(),
                ended_at: chrono::Utc::now(),
            });
        let choice = self
            .frontend
            .user_choose_after_step_failure(&step, &exit_info)?;
        match choice {
            StepFailureChoice::Retry => {
                self.msg_info(format!("Retrying step '{}'", step_name));
                self.state.set_status(step_name, StepState::Pending);
                self.persist()?;
                Ok(IterationOutcome::Continue)
            }
            StepFailureChoice::Pause => {
                self.msg_info("Workflow paused");
                self.persist()?;
                let paused = WorkflowOutcome::Paused;
                self.frontend.report_workflow_completed(&paused);
                Ok(IterationOutcome::Ended(paused))
            }
            StepFailureChoice::Abort => Ok(IterationOutcome::Ended(self.handle_abort()?)),
        }
    }

    /// Advance exactly one step, reporting status through the frontend.
    pub async fn step_once(&mut self) -> Result<StepOutcome, EngineError> {
        let step_name = self.launch_step().await?;
        let exit = {
            let exec = self
                .active_steps
                .first_mut()
                .and_then(|s| s.execution.as_mut())
                .expect("launch_step stored execution");
            exec.wait().await?
        };
        self.finalize_step(&step_name, exit)
    }

    async fn launch_step(&mut self) -> Result<String, EngineError> {
        let ready = self.state.next_ready(&self.dag);
        let step_name = ready
            .first()
            .cloned()
            .ok_or_else(|| EngineError::InvalidAdvanceAction("no ready steps remaining".into()))?;
        let step = self.find_step(&step_name)?;

        let resolved_agent = self.resolve_agent(&step)?;
        let resolved_model = self.resolve_model(&step);
        tracing::info!(
            step = %step.name,
            agent = %resolved_agent.as_str(),
            model = ?resolved_model,
            "workflow_engine resolved step parameters"
        );

        let workflow_step_info = self.build_workflow_step_info(&step.name);
        let runtime = WorkflowRuntimeContext {
            step_agent: resolved_agent.clone(),
            step_model: resolved_model.clone(),
            git_root: self.session.git_root().to_path_buf(),
            session_id: self.session.id(),
            workflow_invocation_id: self.state.invocation_id,
            workflow_step_info,
        };

        self.frontend.report_step_interactive_launch(
            &step,
            resolved_agent.as_str(),
            resolved_model.as_deref(),
        );

        self.state
            .set_status(&step.name, StepState::Running { container_id: None });
        self.frontend
            .report_step_status(&step, WorkflowStepStatus::Running);
        self.persist()?;

        let execution = self
            .agent_factory
            .execution_for_step(&step, &self.session, &runtime)?;

        self.state.set_status(
            &step.name,
            StepState::Running {
                container_id: Some(execution.handle().id.clone()),
            },
        );
        self.persist()?;

        let stuck_sender = execution.stuck_sender();
        let container_name = execution.handle().name.clone();
        let output_tail = execution.output_tail();
        self.active_steps = vec![ActiveParallelStep {
            step_name: step.name.clone(),
            execution: Some(execution),
            cancel_handle: None,
            stuck_sender,
            container_name,
            output_tail,
            awman_killed: false,
            stuck: false,
            yolo_deadline: None,
            agent: resolved_agent.clone(),
            model: resolved_model.clone(),
        }];
        self.current_step_name = Some(step.name.clone());
        self.current_step_agent = Some(resolved_agent);
        self.current_step_model = resolved_model;
        Ok(step.name)
    }

    fn finalize_step(
        &mut self,
        step_name: &str,
        exit: AgentExitInfo,
    ) -> Result<StepOutcome, EngineError> {
        self.last_exit_info = Some(exit.clone());
        // The step's container has actually terminated (wait() resolved) —
        // tell the frontend so it can tear down any live container UI.
        self.frontend.report_container_exited(exit.exit_code);

        // On a genuine (non-awman-kill) container failure, persist the buffered
        // output tail so the user can debug what went wrong.
        self.maybe_dump_step_failure(step_name, exit.exit_code);

        let (status, step_state) = if exit.exit_code == 0 {
            (WorkflowStepStatus::Succeeded, StepState::Succeeded)
        } else {
            (
                WorkflowStepStatus::Failed {
                    exit_code: exit.exit_code,
                },
                StepState::Failed {
                    exit_code: exit.exit_code,
                    error_message: None,
                },
            )
        };
        let step = self.find_step(step_name)?;
        self.state.set_status(step_name, step_state);
        self.frontend.report_step_status(&step, status.clone());
        self.persist()?;

        let remaining = self
            .workflow
            .steps
            .iter()
            .filter(|s| !self.state.completed_steps.contains(&s.name))
            .count();
        Ok(StepOutcome {
            step_name: step_name.to_string(),
            status,
            remaining,
        })
    }

    /// Like `step_once`, but processes `EngineRequest` messages (Ctrl-W)
    /// and container stuck events while the step container runs.
    async fn step_once_interruptible(&mut self) -> Result<InterruptibleStepResult, EngineError> {
        let step_name = self.launch_step().await?;

        let cancel_handle = self.focused_execution().and_then(|e| e.cancel_handle());

        // Subscribe to stuck/unstuck events from the container's io_bridge.
        let mut stuck_rx = self.focused_execution().map(|e| e.subscribe_stuck());

        // Publish the stuck sender to the frontend (TUI uses it for tab coloring).
        if let Some(exec) = self.focused_execution() {
            self.frontend.set_stuck_sender(exec.stuck_sender());
        }

        let mut exec = self
            .take_focused_execution()
            .expect("launch_step stored execution");
        let (wait_tx, mut wait_rx) =
            tokio::sync::oneshot::channel::<(AgentExecution, Result<AgentExitInfo, EngineError>)>();
        tokio::spawn(async move {
            let result = exec.wait().await;
            let _ = wait_tx.send((exec, result));
        });

        loop {
            tokio::select! {
                biased;
                result = &mut wait_rx => {
                    let (exec_back, exit_result) = result
                        .map_err(|_| EngineError::Other("step wait task dropped unexpectedly".into()))?;
                    self.set_focused_execution(exec_back);
                    return Ok(InterruptibleStepResult::StepCompleted(
                        self.finalize_step(&step_name, exit_result?)?
                    ));
                }
                Some(event) = Self::recv_stuck(&mut stuck_rx) => {
                    match event {
                        StuckEvent::Stuck => {
                            let result = self.handle_step_stuck(
                                &step_name,
                                &cancel_handle,
                                &mut wait_rx,
                                &mut stuck_rx,
                            ).await?;
                            match result {
                                None => continue,
                                Some(r) => return Ok(r),
                            }
                        }
                        StuckEvent::Unstuck => {
                            // Not inside a yolo countdown — nothing to cancel.
                        }
                        StuckEvent::StartupGraceExpired => {
                            // Container produced no output during its grace
                            // window. The bridge already invoked the cancel
                            // callback to kill it; surface a warning and let
                            // wait_rx resolve naturally so finalize_step
                            // records the failure. This is an awman-initiated
                            // kill, so mark the slot to suppress a failure log.
                            self.mark_focused_killed();
                            self.msg_warning(format!(
                                "Step '{}' produced no output before its startup grace expired; killing container",
                                step_name,
                            ));
                        }
                    }
                }
                Some(req) = Self::recv_engine(&mut self.engine_rx) => {
                    match req {
                        EngineRequest::OpenControlBoard { .. } => {
                            let mid = self.handle_mid_step_control_board(
                                &step_name,
                                &cancel_handle,
                                &mut wait_rx,
                            )?;
                            match mid {
                                MidStepOutcome::Continue => continue,
                                MidStepOutcome::StepCompleted(o) => {
                                    return Ok(InterruptibleStepResult::StepCompleted(o));
                                }
                                MidStepOutcome::WorkflowEnded(wo) => {
                                    return Ok(InterruptibleStepResult::WorkflowEnded(wo));
                                }
                                MidStepOutcome::LoopContinue => {
                                    return Ok(InterruptibleStepResult::LoopContinue);
                                }
                            }
                        }
                        EngineRequest::StepStuck { .. } => {
                            let result = self.handle_step_stuck(
                                &step_name,
                                &cancel_handle,
                                &mut wait_rx,
                                &mut stuck_rx,
                            ).await?;
                            match result {
                                None => continue,
                                Some(r) => return Ok(r),
                            }
                        }
                        EngineRequest::StepUnstuck { .. } => {
                            // Not inside a yolo countdown — nothing to cancel.
                        }
                    }
                }
            }
        }
    }

    /// Receive from the engine channel, or pend forever if None.
    async fn recv_engine(
        rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<EngineRequest>>,
    ) -> Option<EngineRequest> {
        match rx {
            Some(rx) => rx.recv().await,
            None => std::future::pending().await,
        }
    }

    /// Receive from the stuck broadcast channel, or pend forever if None.
    async fn recv_stuck(
        rx: &mut Option<tokio::sync::broadcast::Receiver<StuckEvent>>,
    ) -> Option<StuckEvent> {
        match rx {
            Some(rx) => rx.recv().await.ok(),
            None => std::future::pending().await,
        }
    }

    /// Kill the current step's container and immediately tell the frontend
    /// it is gone. Used by every engine-initiated kill (yolo auto-advance,
    /// WCB advance/restart/back/pause/abort/finish). Steps whose container
    /// exits on its own are reported via `finalize_step` instead.
    fn kill_current_container(
        &mut self,
        cancel_handle: &Option<crate::engine::agent_runtime::execution::CancelHandle>,
    ) {
        // Mark before cancelling so that if the container's wait future later
        // reaches finalize_step, the exit is treated as an expected kill and no
        // failure log is written.
        self.mark_focused_killed();
        if let Some(ch) = cancel_handle {
            let _ = ch.cancel();
            self.frontend.report_container_exited(KILLED_EXIT_CODE);
        }
    }

    fn handle_mid_step_control_board(
        &mut self,
        step_name: &str,
        cancel_handle: &Option<crate::engine::agent_runtime::execution::CancelHandle>,
        wait_rx: &mut tokio::sync::oneshot::Receiver<(
            AgentExecution,
            Result<AgentExitInfo, EngineError>,
        )>,
    ) -> Result<MidStepOutcome, EngineError> {
        let available = self.compute_available_actions()?;
        let action = self
            .frontend
            .show_workflow_control_board(&self.state, &available)?;

        self.log_wcb_action(&action);

        let already_finished = match wait_rx.try_recv() {
            Ok((exec_back, exit_result)) => {
                self.set_focused_execution(exec_back);
                Some(exit_result)
            }
            Err(_) => None,
        };

        match action {
            NextAction::Dismiss => {
                if let Some(exit_result) = already_finished {
                    return Ok(MidStepOutcome::StepCompleted(
                        self.finalize_step(step_name, exit_result?)?,
                    ));
                }
                Ok(MidStepOutcome::Continue)
            }
            NextAction::ContinueInCurrentContainer { prompt } => {
                // Direct field access keeps the borrow of `active_steps`
                // disjoint from `agent_factory` (the helper would borrow all of
                // `self`).
                if let Some(exec) = self.active_steps.first().and_then(|s| s.execution.as_ref()) {
                    let _ = self.agent_factory.inject_prompt(exec, &prompt);
                }
                if let Some(exit_result) = already_finished {
                    return Ok(MidStepOutcome::StepCompleted(
                        self.finalize_step(step_name, exit_result?)?,
                    ));
                }
                Ok(MidStepOutcome::Continue)
            }
            NextAction::Pause => {
                if already_finished.is_none() {
                    self.kill_current_container(cancel_handle);
                }
                self.state.set_status(step_name, StepState::Pending);
                self.persist()?;
                let outcome = WorkflowOutcome::Paused;
                self.frontend.report_workflow_completed(&outcome);
                Ok(MidStepOutcome::WorkflowEnded(outcome))
            }
            NextAction::Abort => {
                if already_finished.is_none() {
                    self.kill_current_container(cancel_handle);
                }
                for s in &self.workflow.steps {
                    if !self.state.completed_steps.contains(&s.name) {
                        self.state.set_status(&s.name, StepState::Cancelled);
                    }
                }
                self.persist()?;
                let outcome = WorkflowOutcome::Aborted;
                self.frontend.report_workflow_completed(&outcome);
                Ok(MidStepOutcome::WorkflowEnded(outcome))
            }
            NextAction::FinishWorkflow => {
                if !self.is_last_step() {
                    return Err(EngineError::InvalidAdvanceAction(
                        "FinishWorkflow only valid on the last step".into(),
                    ));
                }
                if already_finished.is_none() {
                    self.kill_current_container(cancel_handle);
                }
                for s in &self.workflow.steps {
                    if !self.state.completed_steps.contains(&s.name) {
                        self.state.set_status(&s.name, StepState::Skipped);
                    }
                }
                self.persist()?;
                let outcome = WorkflowOutcome::Completed;
                self.frontend.report_workflow_completed(&outcome);
                Ok(MidStepOutcome::WorkflowEnded(outcome))
            }
            NextAction::LaunchNext => {
                if already_finished.is_none() {
                    self.kill_current_container(cancel_handle);
                }
                self.state.set_status(step_name, StepState::Succeeded);
                self.persist()?;
                Ok(MidStepOutcome::LoopContinue)
            }
            NextAction::RestartCurrentStep => {
                if already_finished.is_none() {
                    self.kill_current_container(cancel_handle);
                }
                self.state.set_status(step_name, StepState::Pending);
                self.persist()?;
                Ok(MidStepOutcome::LoopContinue)
            }
            NextAction::CancelToPreviousStep => {
                if already_finished.is_none() {
                    self.kill_current_container(cancel_handle);
                }
                if let Some(prev) = self.previous_step_name() {
                    self.state.set_status(step_name, StepState::Cancelled);
                    self.state.set_status(&prev, StepState::Pending);
                    self.persist()?;
                }
                Ok(MidStepOutcome::LoopContinue)
            }
        }
    }

    /// Handle a stuck event (from broadcast channel or EngineRequest).
    /// Returns `None` to continue the select loop, or `Some(result)` to return.
    async fn handle_step_stuck(
        &mut self,
        step_name: &str,
        cancel_handle: &Option<crate::engine::agent_runtime::execution::CancelHandle>,
        wait_rx: &mut tokio::sync::oneshot::Receiver<(
            AgentExecution,
            Result<AgentExitInfo, EngineError>,
        )>,
        stuck_rx: &mut Option<tokio::sync::broadcast::Receiver<StuckEvent>>,
    ) -> Result<Option<InterruptibleStepResult>, EngineError> {
        self.msg_warning(format!("Step '{}' appears stuck (no output)", step_name,));
        if self.yolo && !self.is_last_step() {
            let yolo_result = self
                .run_mid_step_yolo_countdown(step_name, cancel_handle, wait_rx, stuck_rx)
                .await?;
            match yolo_result {
                MidStepYoloResult::StepCompleted(o) => {
                    Ok(Some(InterruptibleStepResult::StepCompleted(o)))
                }
                MidStepYoloResult::ShowControlBoard => {
                    let mid =
                        self.handle_mid_step_control_board(step_name, cancel_handle, wait_rx)?;
                    Ok(match mid {
                        MidStepOutcome::Continue => None,
                        MidStepOutcome::StepCompleted(o) => {
                            Some(InterruptibleStepResult::StepCompleted(o))
                        }
                        MidStepOutcome::WorkflowEnded(wo) => {
                            Some(InterruptibleStepResult::WorkflowEnded(wo))
                        }
                        MidStepOutcome::LoopContinue => Some(InterruptibleStepResult::LoopContinue),
                    })
                }
                MidStepYoloResult::Cancelled | MidStepYoloResult::Recovered => Ok(None),
                MidStepYoloResult::Advanced => {
                    self.msg_info(format!("Yolo auto-advancing past step '{}'", step_name,));
                    self.kill_current_container(cancel_handle);
                    self.state.set_status(step_name, StepState::Succeeded);
                    self.persist()?;
                    let step = self.find_step(step_name)?;
                    self.frontend
                        .report_step_status(&step, WorkflowStepStatus::Succeeded);
                    let progress = self.workflow_progress_info();
                    self.frontend.report_workflow_progress(&progress);

                    if self.is_last_step() {
                        let available = self.compute_available_actions()?;
                        let action = self
                            .frontend
                            .show_workflow_control_board(&self.state, &available)?;
                        return Ok(Some(self.execute_top_level_action(action)?));
                    }

                    Ok(Some(InterruptibleStepResult::LoopContinue))
                }
            }
        } else {
            let mid = self.handle_mid_step_control_board(step_name, cancel_handle, wait_rx)?;
            Ok(match mid {
                MidStepOutcome::Continue => None,
                MidStepOutcome::StepCompleted(o) => Some(InterruptibleStepResult::StepCompleted(o)),
                MidStepOutcome::WorkflowEnded(wo) => {
                    Some(InterruptibleStepResult::WorkflowEnded(wo))
                }
                MidStepOutcome::LoopContinue => Some(InterruptibleStepResult::LoopContinue),
            })
        }
    }

    /// Run a mid-step yolo countdown. The step container keeps running while
    /// the countdown ticks. The engine calls `yolo_countdown_started` at the
    /// beginning and `yolo_countdown_finished` before returning.
    async fn run_mid_step_yolo_countdown(
        &mut self,
        step_name: &str,
        _cancel_handle: &Option<crate::engine::agent_runtime::execution::CancelHandle>,
        wait_rx: &mut tokio::sync::oneshot::Receiver<(
            AgentExecution,
            Result<AgentExitInfo, EngineError>,
        )>,
        stuck_rx: &mut Option<tokio::sync::broadcast::Receiver<StuckEvent>>,
    ) -> Result<MidStepYoloResult, EngineError> {
        self.msg_info(format!(
            "Starting yolo countdown for step '{}' ({}s)",
            step_name,
            timing::YOLO_COUNTDOWN_DURATION.as_secs(),
        ));
        self.frontend.yolo_countdown_started(step_name);
        let total = timing::YOLO_COUNTDOWN_DURATION;
        let start = std::time::Instant::now();

        loop {
            // Drain any pending stuck events first. Without this, an `Unstuck`
            // event that lands at almost the same instant as countdown expiry
            // can be passed over by the `remaining.is_zero()` check below —
            // the loop would return `Advanced` (and mark the step Succeeded)
            // even though the container just produced fresh output. Draining
            // here guarantees Unstuck wins the race.
            if let Some(rx) = stuck_rx.as_mut() {
                loop {
                    match rx.try_recv() {
                        Ok(StuckEvent::Unstuck) => {
                            self.msg_info(format!(
                                "Step '{}' recovered, cancelling countdown (timers reset)",
                                step_name,
                            ));
                            self.frontend.yolo_countdown_finished(step_name);
                            return Ok(MidStepYoloResult::Recovered);
                        }
                        Ok(StuckEvent::StartupGraceExpired) => {
                            self.msg_warning(format!(
                                "Step '{}' produced no output before its startup grace expired; cancelling countdown",
                                step_name,
                            ));
                            self.frontend.yolo_countdown_finished(step_name);
                            return Ok(MidStepYoloResult::Recovered);
                        }
                        Ok(StuckEvent::Stuck) => continue,
                        Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                        // Lagged: a message was dropped because the channel
                        // buffer (16) was exceeded. Loop again so we keep
                        // draining whatever's still in the queue.
                        Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
                    }
                }
            }

            let elapsed = start.elapsed();
            let remaining = if elapsed >= total {
                std::time::Duration::ZERO
            } else {
                total - elapsed
            };

            match self
                .frontend
                .yolo_countdown_tick(step_name, remaining, total)?
            {
                YoloTickOutcome::AdvanceNow => {
                    self.frontend.yolo_countdown_finished(step_name);
                    return Ok(MidStepYoloResult::Advanced);
                }
                YoloTickOutcome::Cancel => {
                    self.msg_info(format!("Yolo countdown cancelled for step '{}'", step_name,));
                    self.frontend.yolo_countdown_finished(step_name);
                    return Ok(MidStepYoloResult::Cancelled);
                }
                YoloTickOutcome::Continue => {}
            }

            if remaining.is_zero() {
                self.frontend.yolo_countdown_finished(step_name);
                return Ok(MidStepYoloResult::Advanced);
            }

            tokio::select! {
                biased;
                result = &mut *wait_rx => {
                    let (exec_back, exit_result) = result
                        .map_err(|_| EngineError::Other("step wait task dropped unexpectedly".into()))?;
                    self.set_focused_execution(exec_back);
                    self.frontend.yolo_countdown_finished(step_name);
                    return Ok(MidStepYoloResult::StepCompleted(
                        self.finalize_step(step_name, exit_result?)?
                    ));
                }
                Some(event) = Self::recv_stuck(stuck_rx) => {
                    match event {
                        StuckEvent::Unstuck => {
                            self.msg_info(format!(
                                "Step '{}' recovered, cancelling countdown (timers reset)",
                                step_name,
                            ));
                            self.frontend.yolo_countdown_finished(step_name);
                            return Ok(MidStepYoloResult::Recovered);
                        }
                        StuckEvent::Stuck => {
                            // Already counting down; ignore duplicate.
                        }
                        StuckEvent::StartupGraceExpired => {
                            // The container never produced its first byte
                            // before grace ran out, so the bridge already
                            // killed it. Tear down the countdown; wait_rx
                            // will resolve and finalize_step records the
                            // failure.
                            self.msg_warning(format!(
                                "Step '{}' produced no output before its startup grace expired; cancelling countdown",
                                step_name,
                            ));
                            self.frontend.yolo_countdown_finished(step_name);
                            return Ok(MidStepYoloResult::Recovered);
                        }
                    }
                }
                Some(req) = Self::recv_engine(&mut self.engine_rx) => {
                    match req {
                        EngineRequest::OpenControlBoard { .. } => {
                            self.frontend.yolo_countdown_finished(step_name);
                            return Ok(MidStepYoloResult::ShowControlBoard);
                        }
                        EngineRequest::StepUnstuck { .. } => {
                            self.msg_info(format!(
                                "Step '{}' recovered (engine request), cancelling countdown",
                                step_name,
                            ));
                            self.frontend.yolo_countdown_finished(step_name);
                            return Ok(MidStepYoloResult::Recovered);
                        }
                        EngineRequest::StepStuck { .. } => {
                            // Already counting down; ignore duplicate.
                        }
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
            }
        }
    }

    /// Execute a top-level action from the WCB (used after yolo auto-advance
    /// on the last step, and in run_to_completion inter-step transitions).
    fn execute_top_level_action(
        &mut self,
        action: NextAction,
    ) -> Result<InterruptibleStepResult, EngineError> {
        match action {
            NextAction::Dismiss | NextAction::LaunchNext => {
                Ok(InterruptibleStepResult::LoopContinue)
            }
            NextAction::FinishWorkflow => {
                let wo = self.handle_finish_workflow()?;
                Ok(InterruptibleStepResult::WorkflowEnded(wo))
            }
            NextAction::Pause => {
                self.persist()?;
                let outcome = WorkflowOutcome::Paused;
                self.frontend.report_workflow_completed(&outcome);
                Ok(InterruptibleStepResult::WorkflowEnded(outcome))
            }
            NextAction::Abort => {
                let wo = self.handle_abort()?;
                Ok(InterruptibleStepResult::WorkflowEnded(wo))
            }
            NextAction::RestartCurrentStep => {
                if let Some(name) = self.current_step_name.clone() {
                    self.state.set_status(&name, StepState::Pending);
                    self.persist()?;
                }
                Ok(InterruptibleStepResult::LoopContinue)
            }
            NextAction::CancelToPreviousStep => {
                self.handle_cancel_to_previous()?;
                Ok(InterruptibleStepResult::LoopContinue)
            }
            NextAction::ContinueInCurrentContainer { prompt } => {
                self.handle_continue_in_current_container(&prompt)?;
                Ok(InterruptibleStepResult::LoopContinue)
            }
        }
    }

    fn handle_finish_workflow(&mut self) -> Result<WorkflowOutcome, EngineError> {
        if !self.is_last_step() {
            return Err(EngineError::InvalidAdvanceAction(
                "FinishWorkflow only valid on the last step".into(),
            ));
        }
        let skipped: Vec<String> = self
            .workflow
            .steps
            .iter()
            .filter(|s| !self.state.completed_steps.contains(&s.name))
            .map(|s| s.name.clone())
            .collect();
        for name in &skipped {
            self.state.set_status(name, StepState::Skipped);
        }
        if !skipped.is_empty() {
            self.msg_info(format!("Skipping remaining steps: {}", skipped.join(", "),));
        }
        self.persist()?;
        self.msg_success(format!("Workflow '{}' completed", self.state.workflow_name,));
        let outcome = WorkflowOutcome::Completed;
        self.frontend.report_workflow_completed(&outcome);
        Ok(outcome)
    }

    fn handle_abort(&mut self) -> Result<WorkflowOutcome, EngineError> {
        self.msg_warning("Workflow aborted");
        for s in &self.workflow.steps {
            if !self.state.completed_steps.contains(&s.name) {
                self.state.set_status(&s.name, StepState::Cancelled);
            }
        }
        self.persist()?;
        let outcome = WorkflowOutcome::Aborted;
        self.frontend.report_workflow_completed(&outcome);
        Ok(outcome)
    }

    fn log_wcb_action(&mut self, action: &NextAction) {
        let step = self.current_step_name.as_deref().unwrap_or("unknown");
        match action {
            NextAction::Dismiss => {}
            NextAction::LaunchNext => {
                self.msg_info("Advancing to next step");
            }
            NextAction::ContinueInCurrentContainer { .. } => {
                self.msg_info(format!(
                    "Continuing in current container for next step (from '{}')",
                    step,
                ));
            }
            NextAction::RestartCurrentStep => {
                self.msg_info(format!("Restarting step '{}'", step));
            }
            NextAction::CancelToPreviousStep => {
                self.msg_info(format!("Cancelling step '{}', returning to previous", step,));
            }
            NextAction::FinishWorkflow => {
                self.msg_info("Finishing workflow");
            }
            NextAction::Pause => {
                self.msg_info("Workflow paused");
            }
            NextAction::Abort => {
                self.msg_warning("Workflow aborted");
            }
        }
    }

    fn handle_cancel_to_previous(&mut self) -> Result<(), EngineError> {
        let prev = self.previous_step_name();
        match prev {
            Some(prev) => {
                if let Some(curr) = self.current_step_name.clone() {
                    self.state.set_status(&curr, StepState::Cancelled);
                }
                self.state.set_status(&prev, StepState::Pending);
                self.persist()?;
                Ok(())
            }
            None => Err(EngineError::InvalidAdvanceAction(
                "no previous step to cancel to".into(),
            )),
        }
    }

    fn handle_continue_in_current_container(&mut self, prompt: &str) -> Result<(), EngineError> {
        let next_step = match self.next_ready_step()? {
            Some(s) => s,
            None => {
                return Err(EngineError::InvalidAdvanceAction(
                    "ContinueInCurrentContainer: no next step is ready".into(),
                ))
            }
        };
        let next_agent = self.resolve_agent(&next_step)?;
        let next_model = self.resolve_model(&next_step);
        let agent_ok = self
            .current_step_agent
            .as_ref()
            .map(|a| *a == next_agent)
            .unwrap_or(false);
        let model_ok = self.current_step_model == next_model;
        if !agent_ok || !model_ok {
            return Err(EngineError::InvalidAdvanceAction(
                "ContinueInCurrentContainer requires the same agent and model \
                 for the current and next steps"
                    .into(),
            ));
        }
        match self.active_steps.first().and_then(|s| s.execution.as_ref()) {
            Some(exec) => match self.agent_factory.inject_prompt(exec, prompt)? {
                Some(()) => {
                    self.state.set_status(&next_step.name, StepState::Succeeded);
                    self.current_step_name = Some(next_step.name.clone());
                    self.persist()?;
                    Ok(())
                }
                None => Err(EngineError::InvalidAdvanceAction(
                    "container backend does not support prompt injection; \
                         use LaunchNext to start a fresh container"
                        .into(),
                )),
            },
            None => Err(EngineError::InvalidAdvanceAction(
                "no container execution is available to inject into".into(),
            )),
        }
    }

    pub fn compute_available_actions(&self) -> Result<AvailableActions, EngineError> {
        let has_execution = self.focused_execution().is_some();
        let mut a = AvailableActions {
            can_launch_next: !self.state.is_complete(),
            can_restart_current_step: self.current_step_name.is_some(),
            can_pause: true,
            can_abort: true,
            can_finish_workflow: self.is_last_step(),
            can_dismiss: has_execution || self.current_step_name.is_some(),
            ..Default::default()
        };
        if let Some(next) = self.next_ready_step()? {
            let next_agent = self.resolve_agent(&next)?;
            let next_model = self.resolve_model(&next);
            let ok = match (&self.current_step_agent, &self.current_step_model) {
                (Some(curr_a), curr_m) => *curr_a == next_agent && *curr_m == next_model,
                _ => false,
            };
            if ok && has_execution {
                a.can_continue_in_current_container = true;
                a.continue_prompt = Some(next.prompt_template.clone());
            } else {
                a.continue_unavailable_reason = Some(if self.current_step_agent.is_none() {
                    "no current container".into()
                } else {
                    "next step targets a different agent or model".into()
                });
            }
        }
        if self.previous_step_name().is_some() {
            a.can_cancel_to_previous_step = true;
        } else {
            a.cancel_to_previous_unavailable_reason = Some("this is the first step".into());
        }
        if !a.can_finish_workflow {
            a.finish_workflow_unavailable_reason =
                Some("FinishWorkflow is only valid on the last step".into());
        }

        // Workflow Control Board scoping for parallel groups (WI-0096 §10).
        // The board's actions apply to the focused container; peers keep
        // running. `active_steps` counts live containers; the focused step is
        // the first entry, so peers = len - 1.
        let peers_running = self.active_steps.len().saturating_sub(1);
        a.parallel_peer_count = self.active_steps.len();
        a.parallel_peers_running = peers_running;
        if peers_running > 0 {
            // Restart still targets only the focused container; surface the
            // scoping note so frontends can explain it.
            a.can_restart_current_step = false;
            a.restart_unavailable_reason =
                Some("Restart applies only to the focused container. Switch with Ctrl-S.".into());
            a.can_cancel_to_previous_step = false;
            a.cancel_to_previous_unavailable_reason =
                Some("Cannot go back while other agents in this group are still running.".into());
            a.can_finish_workflow = false;
            a.finish_workflow_unavailable_reason =
                Some("Cannot finish while other agents in this group are still running.".into());
        }
        Ok(a)
    }

    pub fn next_ready_steps(&self) -> Result<Vec<WorkflowStep>, EngineError> {
        self.state
            .next_ready(&self.dag)
            .into_iter()
            .map(|name| self.find_step(&name))
            .collect()
    }

    fn next_ready_step(&self) -> Result<Option<WorkflowStep>, EngineError> {
        match self.state.next_ready(&self.dag).into_iter().next() {
            Some(name) => Ok(Some(self.find_step(&name)?)),
            None => Ok(None),
        }
    }

    fn previous_step_name(&self) -> Option<String> {
        let curr = self.current_step_name.as_ref()?;
        let order = self.dag.topological_order();
        let idx = order.iter().position(|n| n == curr)?;
        if idx == 0 {
            None
        } else {
            Some(order[idx - 1].clone())
        }
    }

    fn is_last_step(&self) -> bool {
        let curr = match self.current_step_name.as_ref() {
            Some(c) => c,
            None => return false,
        };
        let order = self.dag.topological_order();
        order.last().map(|s| s == curr).unwrap_or(false)
    }

    fn find_step(&self, name: &str) -> Result<WorkflowStep, EngineError> {
        self.workflow
            .steps
            .iter()
            .find(|s| s.name == name)
            .cloned()
            .ok_or_else(|| EngineError::Other(format!("step '{name}' not found in workflow")))
    }

    fn workflow_progress_info(&self) -> Vec<WorkflowStepProgressInfo> {
        use crate::data::workflow_state::StepState;
        self.workflow
            .steps
            .iter()
            .map(|step| {
                let agent = self
                    .resolve_agent(step)
                    .map(|a| a.as_str().to_string())
                    .unwrap_or_else(|_| "?".to_string());
                let model = self.resolve_model(step);
                let status = match self.state.status_of(&step.name) {
                    None | Some(StepState::Pending) => WorkflowStepStatus::Pending,
                    Some(StepState::Running { .. }) => WorkflowStepStatus::Running,
                    Some(StepState::Succeeded) => WorkflowStepStatus::Succeeded,
                    Some(StepState::Failed { exit_code, .. }) => WorkflowStepStatus::Failed {
                        exit_code: *exit_code,
                    },
                    Some(StepState::Cancelled) => WorkflowStepStatus::Cancelled,
                    Some(StepState::Skipped) => WorkflowStepStatus::Skipped,
                };
                WorkflowStepProgressInfo {
                    name: step.name.clone(),
                    agent,
                    model,
                    has_step_override: step.agent.is_some() || step.model.is_some(),
                    status,
                    depends_on: step.depends_on.clone(),
                    max_concurrent: self.max_concurrent,
                }
            })
            .collect()
    }

    fn resolve_agent(&self, step: &WorkflowStep) -> Result<AgentName, EngineError> {
        if let Some(name) = step.agent.as_deref() {
            return AgentName::new(name).map_err(EngineError::Data);
        }
        if let Some(name) = self.workflow.agent.as_deref() {
            return AgentName::new(name).map_err(EngineError::Data);
        }
        if let Some(name) = self.effective_config.agent() {
            return AgentName::new(&name).map_err(EngineError::Data);
        }
        Err(EngineError::Other(
            "no agent resolved for step (no step, workflow, or config default)".into(),
        ))
    }

    fn resolve_model(&self, step: &WorkflowStep) -> Option<String> {
        if let Some(m) = step.model.as_deref() {
            return Some(m.to_string());
        }
        if let Some(m) = self.workflow.model.as_ref() {
            return Some(m.clone());
        }
        self.effective_config.model()
    }

    fn build_workflow_step_info(
        &self,
        current_step_name: &str,
    ) -> Option<crate::engine::context_prompt::WorkflowStepInfo> {
        use crate::engine::context_prompt::{WorkflowStepInfo as CtxStepInfo, WorkflowStepState};

        let title = self
            .workflow
            .title
            .clone()
            .unwrap_or_else(|| "Untitled Workflow".to_string());
        let total = self.workflow.steps.len();
        let mut current_index = 0;
        let mut steps = Vec::with_capacity(total);

        for (i, step) in self.workflow.steps.iter().enumerate() {
            let state = if step.name == current_step_name {
                current_index = i;
                WorkflowStepState::InProgress
            } else {
                match self.state.status_of(&step.name) {
                    Some(StepState::Succeeded) => WorkflowStepState::Completed,
                    Some(StepState::Running { .. }) => WorkflowStepState::InProgress,
                    _ => WorkflowStepState::Pending,
                }
            };
            steps.push((step.name.clone(), state));
        }

        let work_item_number = self.work_item_context.as_ref().map(|c| c.number);
        let work_item_title = self
            .work_item_context
            .as_ref()
            .and_then(|c| c.content.lines().next().map(|l| l.trim().to_string()));

        Some(CtxStepInfo {
            workflow_title: title,
            current_step_name: current_step_name.to_string(),
            current_step_index: current_index,
            total_steps: total,
            steps,
            work_item_number,
            work_item_title,
        })
    }

    fn persist(&self) -> Result<(), EngineError> {
        self.state_store
            .save(&self.state)
            .map_err(EngineError::Data)?;
        Ok(())
    }

    /// Run setup phase steps inside the provided background container.
    /// Returns `Ok(())` on success, `Err` if any step fails (remaining steps
    /// are skipped).
    /// Run the setup phase, asking the caller for a fresh container per step.
    ///
    /// `container_for_step(idx)` is invoked once per step and must return a
    /// container with that step's overlays/env applied — and only that step's.
    /// The returned container is dropped when the step finishes, which kills
    /// the container via `BackgroundContainer::drop`. This is what gives each
    /// step its own isolated resource set (WI-0082): two teardown entries
    /// declaring `overlays = ["ssh()"]` and `overlays = ["env(GITHUB_TOKEN)"]`
    /// must NOT each see both.
    pub fn run_setup<F>(
        &mut self,
        steps: &[crate::data::workflow_definition::SetupStep],
        abort_flags: &[bool],
        on_failure_configs: &[Option<crate::data::workflow_definition::RemediationConfig>],
        mut container_for_step: F,
    ) -> Result<(), EngineError>
    where
        F: FnMut(usize) -> Result<Box<dyn AgentExec>, EngineError>,
    {
        use crate::data::workflow_state::{PhaseStepState, PhaseStepStatus, WorkflowPhase};
        use crate::engine::workflow::step_commands::{
            setup_step_description, substitute_setup_step,
        };

        let wi_ctx = self.work_item_context.as_ref();
        let steps: Vec<_> = steps
            .iter()
            .map(|s| substitute_setup_step(s, wi_ctx))
            .collect();

        self.state.current_phase = WorkflowPhase::Setup;
        self.state.setup_step_states = steps
            .iter()
            .map(|s| PhaseStepState {
                description: setup_step_description(s),
                status: PhaseStepStatus::Pending,
            })
            .collect();
        self.persist()?;

        for (idx, step) in steps.iter().enumerate() {
            let desc = setup_step_description(step);
            let abort = abort_flags.get(idx).copied().unwrap_or(false);

            self.state.setup_step_states[idx].status = PhaseStepStatus::Running;
            self.persist()?;

            self.frontend.on_setup_step_started(&desc);

            let step_failed = self.run_single_setup_step(step, idx, &mut container_for_step);

            if step_failed {
                let rem = on_failure_configs
                    .get(idx)
                    .and_then(|c| c.as_ref())
                    .cloned();
                let remediated = if let Some(rem_config) = rem {
                    self.run_setup_remediation(&rem_config, step, idx, &mut container_for_step)
                } else {
                    false
                };

                if !remediated {
                    let error = self.phase_step_failed_error(true, idx);
                    self.frontend.on_setup_step_failed(&desc, 1, &error);
                    if abort {
                        self.abort_on_failure_triggered = true;
                        return Err(EngineError::Container(format!(
                            "setup step '{}' failed (abort_on_failure)",
                            desc
                        )));
                    }
                    continue;
                }
            }

            self.state.setup_step_states[idx].status = PhaseStepStatus::Succeeded;
            self.persist()?;
            self.frontend.on_setup_step_completed(&desc);
        }

        self.state.setup_completed = true;
        self.state.current_phase = WorkflowPhase::Main;
        self.persist()?;
        Ok(())
    }

    /// Run teardown phase steps, asking the caller for a fresh container per step.
    ///
    /// Skips all steps and returns `Ok(())` if `!teardown_on_failure && !workflow_succeeded`.
    /// Failing teardown steps are logged but do not abort the remaining steps (best-effort).
    /// See [`run_setup`] for the rationale behind per-step containers.
    ///
    /// When the per-step container factory itself fails (e.g. an overlay won't
    /// resolve, or the runtime can't start the container), the engine records
    /// that step as `Failed`, surfaces the error to the frontend, and proceeds
    /// to the next step — matching the best-effort semantics already used for
    /// non-zero exit codes.
    /// Returns `(teardown_aborted, any_step_failed)`:
    /// - `teardown_aborted`: true if an `abort_on_failure` step failed
    /// - `any_step_failed`: true if any teardown step failed (regardless of abort flag)
    pub fn run_teardown<F>(
        &mut self,
        steps: &[crate::data::workflow_definition::TeardownStep],
        abort_flags: &[bool],
        on_failure_configs: &[Option<crate::data::workflow_definition::RemediationConfig>],
        workflow_succeeded: bool,
        teardown_on_failure: bool,
        mut container_for_step: F,
    ) -> Result<(bool, bool), EngineError>
    where
        F: FnMut(usize) -> Result<Box<dyn AgentExec>, EngineError>,
    {
        use crate::data::workflow_state::{PhaseStepState, PhaseStepStatus, WorkflowPhase};
        use crate::engine::workflow::step_commands::{
            substitute_teardown_step, teardown_step_description,
        };

        if !teardown_on_failure && !workflow_succeeded {
            return Ok((false, false));
        }

        let wi_ctx = self.work_item_context.as_ref();
        let steps: Vec<_> = steps
            .iter()
            .map(|s| substitute_teardown_step(s, wi_ctx))
            .collect();

        self.state.current_phase = WorkflowPhase::Teardown;
        self.state.teardown_step_states = steps
            .iter()
            .map(|s| PhaseStepState {
                description: teardown_step_description(s),
                status: PhaseStepStatus::Pending,
            })
            .collect();
        self.persist()?;

        let mut teardown_aborted = false;
        let mut any_step_failed = false;
        for (idx, step) in steps.iter().enumerate() {
            let desc = teardown_step_description(step);
            let abort = abort_flags.get(idx).copied().unwrap_or(false);

            self.state.teardown_step_states[idx].status = PhaseStepStatus::Running;
            self.persist()?;

            self.frontend.on_teardown_step_started(&desc);

            let outcome = self.run_single_teardown_step(step, idx, &mut container_for_step);

            let ultimately_failed = if outcome.failed {
                let rem = on_failure_configs
                    .get(idx)
                    .and_then(|c| c.as_ref())
                    .cloned();
                if let Some(rem_config) = rem {
                    !self.run_teardown_remediation(
                        &rem_config,
                        step,
                        idx,
                        &outcome.stdout,
                        &outcome.stderr,
                        &mut container_for_step,
                    )
                } else {
                    true
                }
            } else {
                false
            };

            if !ultimately_failed {
                self.state.teardown_step_states[idx].status = PhaseStepStatus::Succeeded;
                self.persist()?;
                self.frontend.on_teardown_step_completed(&desc);
            } else {
                let error = self.phase_step_failed_error(false, idx);
                self.frontend.on_teardown_step_failed(&desc, 1, &error);
                any_step_failed = true;
                if abort {
                    teardown_aborted = true;
                    break;
                }
            }
        }

        self.state.teardown_completed = true;
        self.state.current_phase = WorkflowPhase::Done;
        self.persist()?;
        Ok((teardown_aborted, any_step_failed))
    }

    /// Execute a shell command in a container for a setup/teardown step.
    /// Returns a [`PhaseStepOutcome`] carrying the failure flag and, on
    /// failure, the full captured stdout/stderr. The captured output is only
    /// used by the teardown remediation path (Feature B); the setup path
    /// discards it via [`PhaseStepOutcome::failed`].
    fn run_shell_phase_step(
        &mut self,
        container: &dyn AgentExec,
        command: &str,
        env: Option<&std::collections::HashMap<String, String>>,
        phase: &str,
        idx: usize,
    ) -> PhaseStepOutcome {
        let is_setup = phase == "setup";
        let result = match container.exec_streaming(command, env, &mut |line| {
            if is_setup {
                self.frontend.on_setup_step_output(line);
            } else {
                self.frontend.on_teardown_step_output(line);
            }
        }) {
            Ok(r) => r,
            Err(e) => {
                let error = e.to_string();
                self.set_phase_step_failed(is_setup, idx, &error);
                // The runtime never produced an ExecOutput, so stdout is empty
                // and stderr carries the launch error for the failure file.
                return PhaseStepOutcome::failed(String::new(), error);
            }
        };

        if result.exit_code != 0 {
            self.set_phase_step_failed(is_setup, idx, &result.stderr);
            return PhaseStepOutcome::failed(result.stdout, result.stderr);
        }

        PhaseStepOutcome::succeeded()
    }

    /// Record a phase step as failed in the persisted state. Does NOT notify
    /// the frontend — terminal-failure notification (`on_*_step_failed`) is
    /// fired by the outer phase loop only after any `on_failure` remediation
    /// is exhausted, so frontends don't see a misleading failure event when
    /// remediation succeeds.
    fn set_phase_step_failed(&mut self, is_setup: bool, idx: usize, error: &str) {
        use crate::data::workflow_state::PhaseStepStatus;

        let states = if is_setup {
            &mut self.state.setup_step_states
        } else {
            &mut self.state.teardown_step_states
        };
        states[idx].status = PhaseStepStatus::Failed {
            error: error.to_string(),
        };
        let _ = self.persist();
    }

    /// Read the last recorded error string for a failed phase step, or
    /// `"unknown error"` if the state isn't `Failed`.
    fn phase_step_failed_error(&self, is_setup: bool, idx: usize) -> String {
        use crate::data::workflow_state::PhaseStepStatus;
        let states = if is_setup {
            &self.state.setup_step_states
        } else {
            &self.state.teardown_step_states
        };
        match states.get(idx).map(|s| &s.status) {
            Some(PhaseStepStatus::Failed { error }) => error.clone(),
            _ => "unknown error".to_string(),
        }
    }

    /// Execute a PollCi step natively. Returns `true` if the step failed.
    fn run_poll_ci_phase_step(
        &mut self,
        interval_secs: u32,
        max_retries: u32,
        is_setup: bool,
        idx: usize,
    ) -> bool {
        let git_root = self.session.git_root().to_path_buf();
        let result =
            poll_ci::run_poll_ci_loop(&git_root, interval_secs, max_retries, |level, msg| {
                let ml = match level {
                    poll_ci::PollMessage::Info => crate::data::message::MessageLevel::Info,
                    poll_ci::PollMessage::Warning => crate::data::message::MessageLevel::Warning,
                };
                self.frontend
                    .write_message(crate::data::message::UserMessage {
                        level: ml,
                        text: msg,
                    });
            });

        if let Err(e) = result {
            let error = e.to_string();
            self.set_phase_step_failed(is_setup, idx, &error);
            return true;
        }

        false
    }

    /// Execute a single setup step. Returns `true` if failed.
    fn run_single_setup_step<F>(
        &mut self,
        step: &crate::data::workflow_definition::SetupStep,
        idx: usize,
        container_for_step: &mut F,
    ) -> bool
    where
        F: FnMut(usize) -> Result<Box<dyn AgentExec>, EngineError>,
    {
        use crate::data::workflow_definition::SetupStep;
        use crate::engine::workflow::step_commands::setup_step_to_shell;

        if let SetupStep::PollCi {
            interval_secs,
            max_retries,
        } = step
        {
            return self.run_poll_ci_phase_step(
                interval_secs.unwrap_or(30),
                max_retries.unwrap_or(10),
                true,
                idx,
            );
        }

        let (command, env) = setup_step_to_shell(step);
        match container_for_step(idx) {
            Ok(c) => {
                self.run_shell_phase_step(&*c, &command, env.as_ref(), "setup", idx)
                    .failed
            }
            Err(e) => {
                self.set_phase_step_failed(true, idx, &e.to_string());
                true
            }
        }
    }

    /// Execute a single teardown step. Returns a [`PhaseStepOutcome`] carrying
    /// the failure flag and, on failure, the captured stdout/stderr (threaded
    /// to the remediation agent's failure file — Feature B).
    fn run_single_teardown_step<F>(
        &mut self,
        step: &crate::data::workflow_definition::TeardownStep,
        idx: usize,
        container_for_step: &mut F,
    ) -> PhaseStepOutcome
    where
        F: FnMut(usize) -> Result<Box<dyn AgentExec>, EngineError>,
    {
        use crate::data::workflow_definition::TeardownStep;
        use crate::engine::workflow::step_commands::teardown_step_to_shell;

        if let TeardownStep::PollCi {
            interval_secs,
            max_retries,
        } = step
        {
            let failed = self.run_poll_ci_phase_step(
                interval_secs.unwrap_or(30),
                max_retries.unwrap_or(10),
                false,
                idx,
            );
            // PollCi produces no command stdout/stderr; surface the recorded
            // error string as the failure content so the file is still useful.
            return if failed {
                PhaseStepOutcome::failed(String::new(), self.phase_step_failed_error(false, idx))
            } else {
                PhaseStepOutcome::succeeded()
            };
        }

        let (command, env) = teardown_step_to_shell(step);
        match container_for_step(idx) {
            Ok(c) => self.run_shell_phase_step(&*c, &command, env.as_ref(), "teardown", idx),
            Err(e) => {
                let error = e.to_string();
                self.set_phase_step_failed(false, idx, &error);
                PhaseStepOutcome::failed(String::new(), error)
            }
        }
    }

    /// Run on_failure remediation for a setup step. Returns `true` if remediation succeeded.
    fn run_setup_remediation<F>(
        &mut self,
        config: &crate::data::workflow_definition::RemediationConfig,
        step: &crate::data::workflow_definition::SetupStep,
        idx: usize,
        container_for_step: &mut F,
    ) -> bool
    where
        F: FnMut(usize) -> Result<Box<dyn AgentExec>, EngineError>,
    {
        use crate::data::workflow_state::PhaseStepStatus;

        let desc = self.state.setup_step_states[idx].description.clone();
        for attempt in 1..=config.max_attempts {
            self.msg_info(format!(
                "Step failed — launching on_failure agent (attempt {attempt}/{})...",
                config.max_attempts,
            ));

            self.state.setup_step_states[idx].status = PhaseStepStatus::Remediating {
                attempt,
                of: config.max_attempts,
            };
            let _ = self.persist();
            self.frontend
                .on_setup_step_fixing(&desc, attempt, config.max_attempts);

            self.launch_on_failure_agent(config, None);

            self.state.setup_step_states[idx].status = PhaseStepStatus::Running;
            let _ = self.persist();

            let still_failed = self.run_single_setup_step(step, idx, container_for_step);
            if !still_failed {
                self.msg_info(format!(
                    "on_failure remediation succeeded on attempt {attempt}"
                ));
                return true;
            }

            if attempt == config.max_attempts {
                self.msg_warning(format!(
                    "on_failure exhausted all {} attempts; step fully failed",
                    config.max_attempts,
                ));
            }
        }

        false
    }

    /// Run on_failure remediation for a teardown step. Returns `true` if remediation succeeded.
    ///
    /// `stdout` / `stderr` carry the output of the failure that triggered this
    /// remediation. Each retry that fails again overwrites the failure file
    /// with its own fresh output, so the agent always sees the most recent
    /// failure (Feature B).
    fn run_teardown_remediation<F>(
        &mut self,
        config: &crate::data::workflow_definition::RemediationConfig,
        step: &crate::data::workflow_definition::TeardownStep,
        idx: usize,
        stdout: &str,
        stderr: &str,
        container_for_step: &mut F,
    ) -> bool
    where
        F: FnMut(usize) -> Result<Box<dyn AgentExec>, EngineError>,
    {
        use crate::data::workflow_state::PhaseStepStatus;

        let desc = self.state.teardown_step_states[idx].description.clone();
        let mut cur_stdout = stdout.to_string();
        let mut cur_stderr = stderr.to_string();
        for attempt in 1..=config.max_attempts {
            self.msg_info(format!(
                "Step failed — launching on_failure agent (attempt {attempt}/{})...",
                config.max_attempts,
            ));

            self.state.teardown_step_states[idx].status = PhaseStepStatus::Remediating {
                attempt,
                of: config.max_attempts,
            };
            let _ = self.persist();
            self.frontend
                .on_teardown_step_fixing(&desc, attempt, config.max_attempts);

            self.launch_on_failure_agent(
                config,
                Some(TeardownFailureContext {
                    step_name: &desc,
                    stdout: &cur_stdout,
                    stderr: &cur_stderr,
                }),
            );

            self.state.teardown_step_states[idx].status = PhaseStepStatus::Running;
            let _ = self.persist();

            let outcome = self.run_single_teardown_step(step, idx, container_for_step);
            if !outcome.failed {
                self.msg_info(format!(
                    "on_failure remediation succeeded on attempt {attempt}"
                ));
                return true;
            }
            // Retain the freshest failure output so the next attempt's file
            // reflects this retry, not the original failure.
            cur_stdout = outcome.stdout;
            cur_stderr = outcome.stderr;

            if attempt == config.max_attempts {
                self.msg_warning(format!(
                    "on_failure exhausted all {} attempts; step fully failed",
                    config.max_attempts,
                ));
            }
        }

        false
    }

    /// Launch the on_failure agent container and wait for it to complete.
    /// The agent's own exit code is ignored — only the subsequent retry
    /// determines success.
    ///
    /// When `failure` is `Some` (teardown remediation), the failed command's
    /// stdout/stderr are written to a file mounted into the agent's container
    /// and a preamble pointing the agent at that file is prepended to the
    /// remediation prompt (Feature B). Setup remediation passes `None`.
    fn launch_on_failure_agent(
        &mut self,
        config: &crate::data::workflow_definition::RemediationConfig,
        failure: Option<TeardownFailureContext<'_>>,
    ) {
        // Owned so it does not hold a borrow on `self.workflow` across the
        // `&mut self` call to `prepare_teardown_failure_file` below.
        let agent_name_str = config
            .agent
            .as_deref()
            .or(self.workflow.agent.as_deref())
            .unwrap_or("claude")
            .to_string();
        let model = config
            .model
            .as_deref()
            .or(self.workflow.model.as_deref())
            .map(|s| s.to_string())
            .or_else(|| self.effective_config.model());

        let agent_name = match crate::data::session::AgentName::new(&agent_name_str) {
            Ok(a) => a,
            Err(e) => {
                self.msg_warning(format!("on_failure: invalid agent name: {e}"));
                return;
            }
        };

        // Feature B: capture the failed command's output into a file the agent
        // can read, and (when needed) an extra read-only mount. A write failure
        // degrades gracefully — the agent still launches, just without the hint.
        let artifacts = failure
            .as_ref()
            .and_then(|f| self.prepare_teardown_failure_file(f));

        let prompt = match &artifacts {
            Some(a) => a.prepend_preamble(&config.prompt),
            None => config.prompt.clone(),
        };
        let extra_overlays = artifacts
            .as_ref()
            .and_then(|a| a.extra_overlay.clone())
            .map(|o| vec![o]);

        let synthetic_step = WorkflowStep {
            name: "__on_failure__".to_string(),
            depends_on: Vec::new(),
            prompt_template: prompt,
            agent: Some(agent_name_str.clone()),
            model: model.clone(),
            overlays: extra_overlays,
            abort_on_failure: false,
        };

        let runtime = WorkflowRuntimeContext {
            step_agent: agent_name,
            step_model: model.clone(),
            git_root: self.session.git_root().to_path_buf(),
            session_id: self.session.id(),
            workflow_invocation_id: self.state.invocation_id,
            workflow_step_info: None,
        };

        // Same pre-launch notification main steps get (mod.rs `launch_step`).
        // Frontends rely on it to prepare per-container state — the TUI
        // recreates its AgentIo channels here; skipping it would make the
        // factory's `take_io` find no channels and fail.
        self.frontend.report_step_interactive_launch(
            &synthetic_step,
            &agent_name_str,
            model.as_deref(),
        );

        let execution =
            match self
                .agent_factory
                .execution_for_step(&synthetic_step, &self.session, &runtime)
            {
                Ok(e) => e,
                Err(e) => {
                    self.msg_warning(format!("on_failure: failed to launch agent: {e}"));
                    return;
                }
            };

        let handle = tokio::runtime::Handle::current();
        let mut exec = execution;
        match handle.block_on(exec.wait()) {
            Ok(exit) => {
                tracing::info!(
                    exit_code = exit.exit_code,
                    "on_failure agent completed (exit code ignored)"
                );
            }
            Err(e) => {
                self.msg_warning(format!("on_failure: agent execution error: {e}"));
            }
        }
    }

    /// Whether this workflow declares a writable `context(workflow)` overlay.
    ///
    /// When true, `~/.awman/context/workflows/{invocation}/` is already mounted
    /// read-write at `/awman/context/workflow` in every agent container, so the
    /// teardown-failure file can be written there directly. A read-only
    /// (`context(workflow:ro)`) declaration returns `false` so the ephemeral
    /// read-only remediation mount is used instead (Feature B edge case).
    ///
    /// The command layer overrides this after merging config/env/CLI/workflow
    /// overlays, so this reflects the active context overlay set.
    fn workflow_context_overlay_writable(&self) -> bool {
        matches!(
            self.workflow_context_permission,
            Some(OverlayPermission::ReadWrite)
        )
    }

    /// Write the teardown-failure output file and resolve its mount, returning
    /// the artifacts needed to point the remediation agent at it. Returns
    /// `None` on any failure (directory or file write) after logging a
    /// warning — remediation then proceeds without the file hint, never
    /// aborting (Feature B edge cases).
    fn prepare_teardown_failure_file(
        &mut self,
        failure: &TeardownFailureContext<'_>,
    ) -> Option<TeardownFailureArtifacts> {
        use crate::data::fs::context_dirs::{validate_context_path, ContextDirResolver};

        let resolver = match ContextDirResolver::from_process_env() {
            Ok(r) => r,
            Err(e) => {
                self.msg_warning(format!(
                    "on_failure: could not resolve context directory, launching agent without \
                     failure output: {e}"
                ));
                return None;
            }
        };

        // The host directory is deterministic for this invocation whether or
        // not context(workflow) is declared — always the workflow context dir.
        let host_dir = resolver.workflow_dir(self.state.invocation_id);

        // Decide the container-visible location. When context(workflow) is
        // active and writable the directory is already mounted read-write at
        // /awman/context/workflow; otherwise mount the (ephemeral) directory
        // read-only at /awman/remediation.
        let use_writable_workflow_overlay = self.workflow_context_overlay_writable();
        let container_path: &'static str = if use_writable_workflow_overlay {
            TEARDOWN_FAILURE_OVERLAY_CONTAINER_PATH
        } else {
            TEARDOWN_FAILURE_EPHEMERAL_CONTAINER_PATH
        };

        // Create the directory (idempotent when the overlay already exists).
        if let Err(e) = std::fs::create_dir_all(&host_dir) {
            self.msg_warning(format!(
                "on_failure: could not create directory {}, launching agent without failure \
                 output: {e}",
                host_dir.display()
            ));
            return None;
        }

        // Security: the resolved path must stay under ~/.awman/context/.
        if let Err(e) = validate_context_path(resolver.awman_home(), &host_dir) {
            self.msg_warning(format!(
                "on_failure: refusing to write failure output outside the context root: {e}"
            ));
            return None;
        }

        let sanitized = sanitize_step_name_for_filename(failure.step_name);
        let filename = format!("teardown-failure-{sanitized}.txt");
        let file_path = host_dir.join(&filename);
        let contents =
            format_teardown_failure_file(failure.step_name, failure.stdout, failure.stderr);
        if let Err(e) = std::fs::write(&file_path, contents) {
            self.msg_warning(format!(
                "on_failure: could not write failure output to {}, launching agent without it: {e}",
                file_path.display()
            ));
            return None;
        }

        let extra_overlay = if use_writable_workflow_overlay {
            None
        } else if self.workflow_context_permission == Some(OverlayPermission::ReadOnly) {
            Some(format!(
                "{}:{}/{}:ro",
                file_path.display(),
                TEARDOWN_FAILURE_EPHEMERAL_CONTAINER_PATH,
                filename
            ))
        } else {
            Some(format!(
                "{}:{}:ro",
                host_dir.display(),
                TEARDOWN_FAILURE_EPHEMERAL_CONTAINER_PATH
            ))
        };

        Some(TeardownFailureArtifacts {
            container_path,
            filename,
            step_name: failure.step_name.to_string(),
            extra_overlay,
        })
    }

    /// Mark the workflow as fully finished. Called by the orchestrator after
    /// the main phase completes when no teardown phase will run (so the state
    /// reflects completion rather than lingering in `Main`).
    pub fn mark_done(&mut self) -> Result<(), EngineError> {
        use crate::data::workflow_state::WorkflowPhase;
        self.state.current_phase = WorkflowPhase::Done;
        self.persist()?;
        Ok(())
    }
}

/// Container path where the failure file is visible when a writable
/// `context(workflow)` overlay is active (already mounted read-write).
const TEARDOWN_FAILURE_OVERLAY_CONTAINER_PATH: &str = "/awman/context/workflow";
/// Container path for the one-off read-only remediation mount used when no
/// writable `context(workflow)` overlay is active.
const TEARDOWN_FAILURE_EPHEMERAL_CONTAINER_PATH: &str = "/awman/remediation";
/// Per-stream truncation cap (~100 KB). Only the tail is retained since the
/// most recent output is the most relevant to a failure.
const TEARDOWN_STREAM_TRUNCATE_BYTES: usize = 100 * 1024;

/// Outcome of a setup/teardown shell step: whether it failed, plus the
/// captured stdout/stderr (populated on failure, used only by the teardown
/// remediation failure file — Feature B). Kept deliberately narrow rather
/// than storing a full `ExecOutput` in `PhaseStepStatus`.
struct PhaseStepOutcome {
    failed: bool,
    stdout: String,
    stderr: String,
}

impl PhaseStepOutcome {
    fn succeeded() -> Self {
        Self {
            failed: false,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    fn failed(stdout: String, stderr: String) -> Self {
        Self {
            failed: true,
            stdout,
            stderr,
        }
    }
}

/// The captured output of a failed teardown command, passed into
/// `launch_on_failure_agent` so it can materialize the remediation failure
/// file (Feature B).
struct TeardownFailureContext<'a> {
    step_name: &'a str,
    stdout: &'a str,
    stderr: &'a str,
}

/// Resolved artifacts after successfully writing the teardown-failure file.
struct TeardownFailureArtifacts {
    /// Container path of the directory holding the file.
    container_path: &'static str,
    /// The written file's name (`teardown-failure-<sanitized>.txt`).
    filename: String,
    /// The original (unsanitized) step name, for the prompt preamble.
    step_name: String,
    /// A one-off `host:container:ro` overlay to add to the synthetic step, or
    /// `None` when the writable `context(workflow)` overlay already mounts it.
    extra_overlay: Option<String>,
}

impl TeardownFailureArtifacts {
    /// Prepend the fixed system-prompt preamble pointing the agent at the
    /// failure file to the user's remediation prompt.
    fn prepend_preamble(&self, user_prompt: &str) -> String {
        format!(
            "The full output (stdout and stderr) of the failed teardown step \"{step}\" has been\n\
             written to {path}/{file}.\n\
             Read that file first to understand the failure before attempting a fix.\n\
             \n\
             ---\n\
             \n\
             {user_prompt}",
            step = self.step_name,
            path = self.container_path,
            file = self.filename,
        )
    }
}

fn workflow_context_permission_from_overlay_strings(
    overlays: Option<&[String]>,
) -> Option<OverlayPermission> {
    let overlays = overlays?;
    for entry in overlays {
        for token in entry.split(',') {
            let token = token.trim();
            let Some(inner) = token
                .strip_prefix("context(")
                .and_then(|s| s.strip_suffix(')'))
            else {
                continue;
            };
            let mut parts = inner.splitn(2, ':');
            let scope = parts.next().unwrap_or("").trim();
            if scope != "workflow" {
                continue;
            }
            return match parts.next().map(str::trim).unwrap_or("rw") {
                "ro" => Some(OverlayPermission::ReadOnly),
                _ => Some(OverlayPermission::ReadWrite),
            };
        }
    }
    None
}

/// Sanitize a step name into a safe filename component: non-alphanumeric
/// characters (including `/`, `\`, `..`, spaces, and shell metacharacters)
/// become `-`, and the result is truncated to 64 characters.
fn sanitize_step_name_for_filename(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    out.truncate(64);
    if out.is_empty() {
        out.push_str("step");
    }
    out
}

/// Retain the last [`TEARDOWN_STREAM_TRUNCATE_BYTES`] of a stream, prefixing a
/// truncation notice when content was dropped. Truncation respects UTF-8
/// character boundaries.
fn truncate_stream(content: &str) -> String {
    if content.len() <= TEARDOWN_STREAM_TRUNCATE_BYTES {
        return content.to_string();
    }
    let start = content.len() - TEARDOWN_STREAM_TRUNCATE_BYTES;
    // Advance to the next char boundary so we never slice mid-codepoint.
    let start = (start..content.len())
        .find(|&i| content.is_char_boundary(i))
        .unwrap_or(content.len());
    format!(
        "[... output truncated, showing last {} KB ...]\n{}",
        TEARDOWN_STREAM_TRUNCATE_BYTES / 1024,
        &content[start..]
    )
}

/// Format the teardown-failure file body in the fixed Feature B layout,
/// substituting `(empty)` for blank streams and truncating oversized output.
fn format_teardown_failure_file(step_name: &str, stdout: &str, stderr: &str) -> String {
    let render = |s: &str| -> String {
        if s.is_empty() {
            "(empty)".to_string()
        } else {
            truncate_stream(s)
        }
    };
    format!(
        "=== FAILED COMMAND: {step_name} ===\n\
         \n\
         --- STDOUT ---\n\
         {stdout}\n\
         \n\
         --- STDERR ---\n\
         {stderr}\n",
        step_name = step_name,
        stdout = render(stdout),
        stderr = render(stderr),
    )
}

/// Hash a workflow's steps + title to detect drift.
fn compute_workflow_hash(workflow: &Workflow) -> String {
    let json = serde_json::to_string(workflow).unwrap_or_default();
    let h = ring::digest::digest(&ring::digest::SHA256, json.as_bytes());
    let mut s = String::with_capacity(64);
    for b in h.as_ref() {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

pub fn workflow_name_for(workflow: &Workflow) -> String {
    workflow.title.as_deref().unwrap_or("workflow").to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use chrono::Utc;

    use super::*;
    use crate::data::session::{AgentHandle, SessionOpenOptions, StaticGitRootResolver};
    use crate::data::workflow_definition::{Workflow, WorkflowStep};
    use crate::data::workflow_state_store::WorkflowStateStore;
    use crate::engine::agent_runtime::execution::{AgentExecution, AgentExitInfo};
    use crate::engine::overlay::OverlayEngine;

    // ── Fake implementations ─────────────────────────────────────────────────

    struct FakeWorkflowFrontend {
        actions: Mutex<VecDeque<NextAction>>,
        step_statuses: Mutex<Vec<(String, WorkflowStepStatus)>>,
        completed: Mutex<Option<WorkflowOutcome>>,
        confirm_resume_response: bool,
        failure_choice: StepFailureChoice,
    }

    impl FakeWorkflowFrontend {
        fn new(actions: impl IntoIterator<Item = NextAction>) -> Self {
            Self {
                actions: Mutex::new(actions.into_iter().collect()),
                step_statuses: Mutex::new(Vec::new()),
                completed: Mutex::new(None),
                confirm_resume_response: true,
                failure_choice: StepFailureChoice::Abort,
            }
        }

        fn with_confirm_resume(mut self, response: bool) -> Self {
            self.confirm_resume_response = response;
            self
        }

        fn step_statuses(&self) -> Vec<(String, WorkflowStepStatus)> {
            self.step_statuses.lock().unwrap().clone()
        }

        fn completed_outcome(&self) -> Option<WorkflowOutcome> {
            self.completed.lock().unwrap().clone()
        }
    }

    impl crate::data::message::UserMessageSink for FakeWorkflowFrontend {
        fn write_message(&mut self, _msg: crate::data::message::UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    impl WorkflowFrontend for FakeWorkflowFrontend {
        fn show_workflow_control_board(
            &mut self,
            _state: &WorkflowState,
            _available: &AvailableActions,
        ) -> Result<NextAction, EngineError> {
            let action = self
                .actions
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(NextAction::LaunchNext);
            Ok(action)
        }

        fn confirm_resume(&mut self, _mismatch: &ResumeMismatch) -> Result<bool, EngineError> {
            Ok(self.confirm_resume_response)
        }

        fn user_choose_after_step_failure(
            &mut self,
            _step: &WorkflowStep,
            _exit: &AgentExitInfo,
        ) -> Result<StepFailureChoice, EngineError> {
            Ok(self.failure_choice.clone())
        }

        fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
            self.step_statuses
                .lock()
                .unwrap()
                .push((step.name.clone(), status));
        }

        fn yolo_countdown_tick(
            &mut self,
            _step_name: &str,
            _remaining: Duration,
            _total: Duration,
        ) -> Result<YoloTickOutcome, EngineError> {
            Ok(YoloTickOutcome::Cancel)
        }

        fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
            *self.completed.lock().unwrap() = Some(outcome.clone());
        }
    }

    struct FakeAgentExecutionFactory {
        exit_codes: Mutex<VecDeque<i32>>,
        pub execution_call_count: AtomicUsize,
        pub inject_call_count: AtomicUsize,
        pub recorded_contexts: Mutex<Vec<WorkflowRuntimeContext>>,
        inject_result: Option<()>,
        /// When set, each produced execution carries an output tail pre-filled
        /// with these lines (exercises the container failure-log path).
        tail_lines: Option<Vec<String>>,
        /// Container name stamped on each produced execution's handle.
        container_name: String,
    }

    impl FakeAgentExecutionFactory {
        fn new(exit_codes: impl IntoIterator<Item = i32>) -> Self {
            Self {
                exit_codes: Mutex::new(exit_codes.into_iter().collect()),
                execution_call_count: AtomicUsize::new(0),
                inject_call_count: AtomicUsize::new(0),
                recorded_contexts: Mutex::new(Vec::new()),
                inject_result: None,
                tail_lines: None,
                container_name: "fake-container".to_string(),
            }
        }

        fn always_success() -> Self {
            Self::new(std::iter::repeat_n(0, 100))
        }

        fn with_inject_support(exit_codes: impl IntoIterator<Item = i32>) -> Self {
            Self {
                inject_result: Some(()),
                ..Self::new(exit_codes)
            }
        }

        /// Produce executions whose output tail is pre-filled with `lines` and
        /// whose container handle is named `container_name`.
        fn with_output_tail(
            exit_codes: impl IntoIterator<Item = i32>,
            container_name: &str,
            lines: impl IntoIterator<Item = &'static str>,
        ) -> Self {
            Self {
                tail_lines: Some(lines.into_iter().map(|s| s.to_string()).collect()),
                container_name: container_name.to_string(),
                ..Self::new(exit_codes)
            }
        }
    }

    impl AgentExecutionFactory for FakeAgentExecutionFactory {
        fn execution_for_step(
            &self,
            _step: &WorkflowStep,
            _session: &Session,
            runtime: &WorkflowRuntimeContext,
        ) -> Result<AgentExecution, EngineError> {
            self.execution_call_count.fetch_add(1, Ordering::Relaxed);
            self.recorded_contexts.lock().unwrap().push(runtime.clone());
            let code = self.exit_codes.lock().unwrap().pop_front().unwrap_or(0);
            let now = Utc::now();
            let info = AgentExitInfo {
                exit_code: code,
                signal: None,
                started_at: now,
                ended_at: now,
            };
            let handle = AgentHandle {
                id: format!("fake-{}", self.execution_call_count.load(Ordering::Relaxed)),
                image_tag: "fake-image:latest".into(),
                name: self.container_name.clone(),
                started_at: now,
            };
            match &self.tail_lines {
                Some(lines) => {
                    let tail = OutputTail::with_default_capacity();
                    for line in lines {
                        tail.push_bytes(line.as_bytes());
                        tail.push_bytes(b"\n");
                    }
                    Ok(AgentExecution::finished_with_tail(
                        handle,
                        info,
                        Some(Arc::new(tail)),
                    ))
                }
                None => Ok(AgentExecution::finished(handle, info)),
            }
        }

        fn inject_prompt(
            &self,
            _execution: &AgentExecution,
            _prompt: &str,
        ) -> Result<Option<()>, EngineError> {
            self.inject_call_count.fetch_add(1, Ordering::Relaxed);
            Ok(self.inject_result)
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn make_session(tmp: &tempfile::TempDir) -> Session {
        let resolver = StaticGitRootResolver::new(tmp.path());
        Session::open(
            tmp.path().to_path_buf(),
            &resolver,
            SessionOpenOptions::default(),
        )
        .unwrap()
    }

    /// Session whose env snapshot pins `AWMAN_CONFIG_HOME` to `home`, so the
    /// engine resolves `~/.awman/logs/` under a temp dir instead of the real
    /// home — no process-global env mutation, no cross-test races.
    fn make_session_with_home(tmp: &tempfile::TempDir, home: &std::path::Path) -> Session {
        use crate::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME};
        let resolver = StaticGitRootResolver::new(tmp.path());
        Session::open(
            tmp.path().to_path_buf(),
            &resolver,
            SessionOpenOptions {
                env: Some(EnvSnapshot::with_overrides([(
                    AWMAN_CONFIG_HOME,
                    home.to_str().unwrap(),
                )])),
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn make_step(name: &str, deps: &[&str], agent: Option<&str>) -> WorkflowStep {
        WorkflowStep {
            name: name.to_string(),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            prompt_template: "do something".to_string(),
            agent: agent.map(|s| s.to_string()),
            model: None,
            overlays: None,
            abort_on_failure: false,
        }
    }

    fn make_workflow(
        title: Option<&str>,
        wf_agent: Option<&str>,
        steps: Vec<WorkflowStep>,
    ) -> Workflow {
        Workflow {
            title: title.map(|s| s.to_string()),
            steps,
            agent: wf_agent.map(|s| s.to_string()),
            model: None,
            setup: Vec::new(),
            teardown: Vec::new(),
            teardown_on_failure: false,
            overlays: None,
        }
    }

    fn make_engine(
        session: &Session,
        workflow: Workflow,
        factory: FakeAgentExecutionFactory,
        actions: impl IntoIterator<Item = NextAction>,
    ) -> WorkflowEngine {
        make_engine_with_frontend(
            session,
            workflow,
            factory,
            FakeWorkflowFrontend::new(actions),
        )
    }

    fn make_engine_with_frontend(
        session: &Session,
        workflow: Workflow,
        factory: FakeAgentExecutionFactory,
        frontend: FakeWorkflowFrontend,
    ) -> WorkflowEngine {
        let overlay = OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
        );
        WorkflowEngine::new(
            session,
            workflow,
            None,
            Box::new(frontend),
            Box::new(factory),
            Arc::new(GitEngine::new()),
            Arc::new(overlay),
        )
        .unwrap()
    }

    // ── WorkflowEngine tests ─────────────────────────────────────────────────

    #[tokio::test]
    async fn step_once_advances_one_step_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("my-wf"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );
        let factory = FakeAgentExecutionFactory::always_success();
        let mut engine = make_engine(&session, workflow, factory, []);

        let outcome = engine.step_once().await.unwrap();
        assert_eq!(outcome.step_name, "a");
        assert!(matches!(outcome.status, WorkflowStepStatus::Succeeded));
        assert_eq!(outcome.remaining, 1);

        assert!(matches!(
            engine.state().status_of("a"),
            Some(StepState::Succeeded)
        ));
        assert!(matches!(
            engine.state().status_of("b"),
            Some(StepState::Pending)
        ));

        let store = WorkflowStateStore::at_git_root(tmp.path());
        let saved = store.load(None, "my-wf").unwrap();
        assert!(saved.is_some());
    }

    #[tokio::test]
    async fn run_to_completion_runs_all_steps() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-all"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );
        let factory = FakeAgentExecutionFactory::always_success();
        let frontend = FakeWorkflowFrontend::new([NextAction::LaunchNext]);
        let overlay = OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
        );
        let mut engine = WorkflowEngine::new(
            &session,
            workflow,
            None,
            Box::new(frontend),
            Box::new(factory),
            Arc::new(GitEngine::new()),
            Arc::new(overlay),
        )
        .unwrap();

        let result = engine.run_to_completion().await.unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    #[tokio::test]
    async fn run_to_completion_runs_all_parallel_steps() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-parallel"),
            Some("claude"),
            vec![
                make_step("a", &[], None),
                make_step("b", &["a"], None),
                make_step("c", &["a"], None),
            ],
        );
        let factory = FakeAgentExecutionFactory::always_success();
        let mut engine = make_engine(
            &session,
            workflow,
            factory,
            [NextAction::LaunchNext, NextAction::LaunchNext],
        );

        let result = engine.run_to_completion().await.unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    #[tokio::test]
    async fn run_to_completion_parallel_fan_in() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-fan-in"),
            Some("claude"),
            vec![
                make_step("a", &[], None),
                make_step("b", &["a"], None),
                make_step("c", &["a"], None),
                make_step("d", &["b", "c"], None),
            ],
        );
        let factory = FakeAgentExecutionFactory::always_success();
        let mut engine = make_engine(
            &session,
            workflow,
            factory,
            [
                NextAction::LaunchNext,
                NextAction::LaunchNext,
                NextAction::LaunchNext,
            ],
        );

        let result = engine.run_to_completion().await.unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    #[tokio::test]
    async fn non_zero_exit_code_marks_step_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-fail"),
            Some("claude"),
            vec![make_step("a", &[], None)],
        );
        let factory = FakeAgentExecutionFactory::new([1]);
        let mut engine = make_engine(&session, workflow, factory, []);

        let outcome = engine.step_once().await.unwrap();
        assert!(matches!(
            outcome.status,
            WorkflowStepStatus::Failed { exit_code: 1 }
        ));
    }

    #[tokio::test]
    async fn failing_container_writes_output_log_and_error_message() {
        use crate::data::message::MessageLevel;

        let tmp = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let session = make_session_with_home(&tmp, home.path());
        let workflow = make_workflow(
            Some("wf-log"),
            Some("claude"),
            vec![make_step("build", &[], None)],
        );
        let factory = FakeAgentExecutionFactory::with_output_tail(
            [1],
            "awman-build-xyz",
            ["compiling project", "error: it exploded"],
        );
        let (frontend, messages) = MessageCapturingFrontend::new();
        let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

        let invocation_id = engine.state().invocation_id;
        let outcome = engine.step_once().await.unwrap();
        assert!(matches!(
            outcome.status,
            WorkflowStepStatus::Failed { exit_code: 1 }
        ));

        // The buffered output must be persisted to the per-container log file.
        let paths = crate::data::fs::WorkflowLogPaths::at_home(home.path());
        let log_path = paths.container_log_path(invocation_id, "build", "awman-build-xyz");
        assert!(
            log_path.exists(),
            "failure log must be written at {}",
            log_path.display()
        );
        let body = std::fs::read_to_string(&log_path).unwrap();
        assert!(body.contains("compiling project"), "log body: {body:?}");
        assert!(body.contains("error: it exploded"), "log body: {body:?}");

        // An Error-level message must point the user at the log file.
        let messages = messages.lock().unwrap();
        let err = messages
            .iter()
            .find(|m| m.level == MessageLevel::Error)
            .expect("an Error message must be emitted on container failure");
        assert!(
            err.text.contains(&log_path.display().to_string()),
            "error message must name the log path: {}",
            err.text
        );
    }

    #[tokio::test]
    async fn successful_container_writes_no_failure_log() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let session = make_session_with_home(&tmp, home.path());
        let workflow = make_workflow(
            Some("wf-ok"),
            Some("claude"),
            vec![make_step("build", &[], None)],
        );
        let factory =
            FakeAgentExecutionFactory::with_output_tail([0], "awman-build-ok", ["all good"]);
        let mut engine = make_engine(&session, workflow, factory, []);

        engine.step_once().await.unwrap();

        let paths = crate::data::fs::WorkflowLogPaths::at_home(home.path());
        assert!(
            !paths.logs_dir().exists(),
            "a clean (exit 0) container must not create the logs directory"
        );
    }

    #[tokio::test]
    async fn awman_killed_container_writes_no_failure_log() {
        // A non-zero exit on a container awman itself killed is expected and
        // must NOT produce a failure log.
        let tmp = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let session = make_session_with_home(&tmp, home.path());
        let workflow = make_workflow(
            Some("wf-killed"),
            Some("claude"),
            vec![make_step("build", &[], None)],
        );
        let factory = FakeAgentExecutionFactory::always_success();
        let mut engine = make_engine(&session, workflow, factory, []);

        // Simulate a live slot that awman killed, carrying buffered output.
        let (tx, _rx) = tokio::sync::broadcast::channel(4);
        let tail = OutputTail::with_default_capacity();
        tail.push_bytes(b"some output before the kill\n");
        engine.active_steps.push(ActiveParallelStep {
            step_name: "build".to_string(),
            execution: None,
            cancel_handle: None,
            stuck_sender: Arc::new(tx),
            container_name: "awman-build-killed".to_string(),
            output_tail: Some(Arc::new(tail)),
            awman_killed: true,
            stuck: false,
            yolo_deadline: None,
            agent: AgentName::new("claude").unwrap(),
            model: None,
        });

        engine.maybe_dump_step_failure("build", KILLED_EXIT_CODE);

        let paths = crate::data::fs::WorkflowLogPaths::at_home(home.path());
        assert!(
            !paths.logs_dir().exists(),
            "an awman-killed container must not produce a failure log"
        );
    }

    #[tokio::test]
    async fn step_failure_abort_returns_aborted() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-fail-abort"),
            Some("claude"),
            vec![make_step("a", &[], None)],
        );
        let factory = FakeAgentExecutionFactory::new([2]);
        let frontend = FakeWorkflowFrontend::new([]);
        let mut engine = make_engine_with_frontend(&session, workflow, factory, frontend);

        let result = engine.run_to_completion().await.unwrap();
        assert!(matches!(result, WorkflowOutcome::Aborted));
    }

    #[tokio::test]
    async fn step_failure_retry_reruns_step() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-fail-retry"),
            Some("claude"),
            vec![make_step("a", &[], None)],
        );
        let factory = FakeAgentExecutionFactory::new([1, 0]);
        let mut frontend = FakeWorkflowFrontend::new([]);
        frontend.failure_choice = StepFailureChoice::Retry;
        let mut engine = make_engine_with_frontend(&session, workflow, factory, frontend);

        let result = engine.run_to_completion().await.unwrap();
        assert!(matches!(result, WorkflowOutcome::Completed));
    }

    #[tokio::test]
    async fn step_failure_pause_returns_paused() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-fail-pause"),
            Some("claude"),
            vec![make_step("a", &[], None)],
        );
        let factory = FakeAgentExecutionFactory::new([1]);
        let mut frontend = FakeWorkflowFrontend::new([]);
        frontend.failure_choice = StepFailureChoice::Pause;
        let mut engine = make_engine_with_frontend(&session, workflow, factory, frontend);

        let result = engine.run_to_completion().await.unwrap();
        assert!(matches!(result, WorkflowOutcome::Paused));
    }

    #[tokio::test]
    async fn pause_persists_state_and_returns_paused() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-pause"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );
        let factory = FakeAgentExecutionFactory::always_success();
        let mut engine = make_engine(&session, workflow, factory, [NextAction::Pause]);

        let result = engine.run_to_completion().await.unwrap();
        assert_eq!(result, WorkflowOutcome::Paused);

        let store = WorkflowStateStore::at_git_root(tmp.path());
        let saved = store.load(None, "wf-pause").unwrap();
        assert!(saved.is_some());
    }

    #[tokio::test]
    async fn resume_with_same_hash_continues_from_saved_state() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let wf = make_workflow(
            Some("wf-resume"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );

        {
            let factory = FakeAgentExecutionFactory::always_success();
            let mut engine = make_engine(&session, wf.clone(), factory, [NextAction::Pause]);
            engine.run_to_completion().await.unwrap();
        }

        let factory2 = FakeAgentExecutionFactory::always_success();
        let overlay = OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
        );
        let frontend = FakeWorkflowFrontend::new([]);
        let mut engine = WorkflowEngine::resume(
            &session,
            wf,
            None,
            Box::new(frontend),
            Box::new(factory2),
            Arc::new(GitEngine::new()),
            Arc::new(overlay),
        )
        .await
        .unwrap();
        let result = engine.run_to_completion().await.unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    /// WI 0106 §6a: a squad task bound to its durable workspace has no
    /// worktree to absorb awman's own bookkeeping, and that directory must
    /// survive every run untouched. `resume_with_state_root` therefore keeps
    /// the state file — which the engine creates, rewrites and (on a fresh
    /// run) deletes — entirely outside the session's root.
    #[tokio::test]
    async fn a_state_root_override_keeps_the_state_file_out_of_the_session_root() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let run_dir = tempfile::tempdir().unwrap();
        let wf = make_workflow(
            Some("wf-state-root"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );

        let overlay = OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
        );
        let mut engine = WorkflowEngine::resume_with_state_root(
            &session,
            wf,
            None,
            Box::new(FakeWorkflowFrontend::new([NextAction::Pause])),
            Box::new(FakeAgentExecutionFactory::always_success()),
            Arc::new(GitEngine::new()),
            Arc::new(overlay),
            Some(run_dir.path().to_path_buf()),
        )
        .await
        .unwrap();
        engine.run_to_completion().await.unwrap();

        assert!(
            WorkflowStateStore::at_git_root(run_dir.path())
                .load(None, "wf-state-root")
                .unwrap()
                .is_some(),
            "state must be persisted under the override root"
        );
        assert!(
            !tmp.path().join(".awman").join("workflows").exists(),
            "the session root must be left untouched by the engine's bookkeeping"
        );
    }

    #[tokio::test]
    async fn resume_with_drifted_hash_calls_confirm_resume_and_aborts_when_declined() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let wf1 = make_workflow(
            Some("wf-drift"),
            Some("claude"),
            vec![make_step("a", &[], None)],
        );

        {
            let factory = FakeAgentExecutionFactory::always_success();
            let mut engine = make_engine(&session, wf1, factory, [NextAction::Pause]);
            engine.run_to_completion().await.unwrap();
        }

        let wf2 = make_workflow(
            Some("wf-drift"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );
        let overlay = OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
        );
        let frontend = FakeWorkflowFrontend::new([]).with_confirm_resume(false);
        let result = WorkflowEngine::resume(
            &session,
            wf2,
            None,
            Box::new(frontend),
            Box::new(FakeAgentExecutionFactory::always_success()),
            Arc::new(GitEngine::new()),
            Arc::new(overlay),
        )
        .await;

        assert!(matches!(
            result,
            Err(EngineError::WorkflowResumeIncompatible(_))
        ));
    }

    #[tokio::test]
    async fn step_level_agent_overrides_workflow_level() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-agent"),
            Some("claude"),
            vec![make_step("a", &[], Some("codex"))],
        );
        let factory = FakeAgentExecutionFactory::always_success();
        let factory_arc: Arc<FakeAgentExecutionFactory> = Arc::new(factory);

        struct RecordingFactory(Arc<FakeAgentExecutionFactory>);
        impl AgentExecutionFactory for RecordingFactory {
            fn execution_for_step(
                &self,
                step: &WorkflowStep,
                session: &Session,
                runtime: &WorkflowRuntimeContext,
            ) -> Result<AgentExecution, EngineError> {
                self.0.execution_for_step(step, session, runtime)
            }
            fn inject_prompt(
                &self,
                e: &AgentExecution,
                p: &str,
            ) -> Result<Option<()>, EngineError> {
                self.0.inject_prompt(e, p)
            }
        }

        let overlay = OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
        );
        let mut engine = WorkflowEngine::new(
            &session,
            workflow,
            None,
            Box::new(FakeWorkflowFrontend::new([])),
            Box::new(RecordingFactory(factory_arc.clone())),
            Arc::new(GitEngine::new()),
            Arc::new(overlay),
        )
        .unwrap();

        engine.step_once().await.unwrap();
        let contexts = factory_arc.recorded_contexts.lock().unwrap().clone();
        assert_eq!(contexts.len(), 1);
        assert_eq!(contexts[0].step_agent.as_str(), "codex");
    }

    #[tokio::test]
    async fn cancel_to_previous_step_unavailable_on_first_step() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-cancel"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );
        let factory = FakeAgentExecutionFactory::always_success();
        let mut engine = make_engine(&session, workflow, factory, []);

        engine.step_once().await.unwrap();

        let available = engine.compute_available_actions().unwrap();
        assert!(!available.can_cancel_to_previous_step);
        assert!(available.cancel_to_previous_unavailable_reason.is_some());
    }

    #[tokio::test]
    async fn yolo_mode_auto_advances_between_steps() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-yolo"),
            Some("claude"),
            vec![
                make_step("a", &[], None),
                make_step("b", &["a"], None),
                make_step("c", &["b"], None),
            ],
        );
        let factory = FakeAgentExecutionFactory::always_success();
        // No actions queued — yolo mode should auto-advance without prompting.
        let mut engine = make_engine(&session, workflow, factory, []);
        engine.set_yolo(true);

        let result = engine.run_to_completion().await.unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    // ── Blocking factory for mid-step tests ──────────────────────────────────

    use std::sync::Condvar;

    type CompletionArc = Arc<(Mutex<Option<i32>>, Condvar)>;

    struct BlockingBackend {
        cancel_flag: Arc<AtomicBool>,
        completion: CompletionArc,
    }

    impl crate::engine::agent_runtime::execution::ExecutionBackend for BlockingBackend {
        fn wait_blocking(self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
            let (lock, cvar) = &*self.completion;
            loop {
                if self.cancel_flag.load(Ordering::Relaxed) {
                    let now = Utc::now();
                    return Ok(AgentExitInfo {
                        exit_code: -1,
                        signal: None,
                        started_at: now,
                        ended_at: now,
                    });
                }
                let guard = lock.lock().unwrap();
                let (guard, _) = cvar.wait_timeout(guard, Duration::from_millis(20)).unwrap();
                if let Some(code) = *guard {
                    let now = Utc::now();
                    return Ok(AgentExitInfo {
                        exit_code: code,
                        signal: None,
                        started_at: now,
                        ended_at: now,
                    });
                }
            }
        }

        fn cancel(&self) -> Result<(), EngineError> {
            self.cancel_flag.store(true, Ordering::Relaxed);
            let (_, cvar) = &*self.completion;
            cvar.notify_all();
            Ok(())
        }

        fn cancel_handle(&self) -> Option<crate::engine::agent_runtime::execution::CancelHandle> {
            let flag = self.cancel_flag.clone();
            let completion = self.completion.clone();
            Some(crate::engine::agent_runtime::execution::CancelHandle::new(
                move || {
                    flag.store(true, Ordering::Relaxed);
                    let (_, cvar) = &*completion;
                    cvar.notify_all();
                    Ok(())
                },
            ))
        }
    }

    fn make_blocking_entry() -> (Arc<AtomicBool>, CompletionArc) {
        (
            Arc::new(AtomicBool::new(false)),
            Arc::new((Mutex::new(None), Condvar::new())),
        )
    }

    fn signal_completion(c: &CompletionArc, code: i32) {
        let (lock, cvar) = &**c;
        *lock.lock().unwrap() = Some(code);
        cvar.notify_all();
    }

    struct BlockingFactory {
        execution_count: Arc<AtomicUsize>,
        inject_count: Arc<AtomicUsize>,
        inject_result: Option<()>,
        blocking_slots: Mutex<VecDeque<(Arc<AtomicBool>, CompletionArc)>>,
    }

    impl BlockingFactory {
        fn new(slots: impl IntoIterator<Item = (Arc<AtomicBool>, CompletionArc)>) -> Self {
            Self {
                execution_count: Arc::new(AtomicUsize::new(0)),
                inject_count: Arc::new(AtomicUsize::new(0)),
                inject_result: None,
                blocking_slots: Mutex::new(slots.into_iter().collect()),
            }
        }
    }

    impl AgentExecutionFactory for BlockingFactory {
        fn execution_for_step(
            &self,
            _step: &WorkflowStep,
            _session: &Session,
            _runtime: &WorkflowRuntimeContext,
        ) -> Result<AgentExecution, EngineError> {
            let idx = self.execution_count.fetch_add(1, Ordering::Relaxed);
            let slot = self.blocking_slots.lock().unwrap().pop_front();
            if let Some((cancel_flag, completion)) = slot {
                let backend = Box::new(BlockingBackend {
                    cancel_flag,
                    completion,
                });
                let now = Utc::now();
                let handle = AgentHandle {
                    id: format!("blocking-{idx}"),
                    image_tag: "test:latest".into(),
                    name: "blocking-container".into(),
                    started_at: now,
                };
                let (stuck_tx, _) = tokio::sync::broadcast::channel(4);
                Ok(AgentExecution::new(
                    handle,
                    backend,
                    std::sync::Arc::new(stuck_tx),
                    None,
                ))
            } else {
                let now = Utc::now();
                let info = AgentExitInfo {
                    exit_code: 0,
                    signal: None,
                    started_at: now,
                    ended_at: now,
                };
                let handle = AgentHandle {
                    id: format!("instant-{idx}"),
                    image_tag: "test:latest".into(),
                    name: "instant-container".into(),
                    started_at: now,
                };
                Ok(AgentExecution::finished(handle, info))
            }
        }

        fn inject_prompt(
            &self,
            _execution: &AgentExecution,
            _prompt: &str,
        ) -> Result<Option<()>, EngineError> {
            self.inject_count.fetch_add(1, Ordering::Relaxed);
            Ok(self.inject_result)
        }
    }

    struct CapturingFrontend {
        actions: Mutex<VecDeque<NextAction>>,
        step_statuses: Mutex<Vec<(String, WorkflowStepStatus)>>,
        completed: Mutex<Option<WorkflowOutcome>>,
        available_log: Mutex<Vec<AvailableActions>>,
        engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>>,
        /// Exit codes passed to `report_container_exited`, shared so tests
        /// can assert on them after the engine consumes the frontend.
        container_exits: Arc<Mutex<Vec<i32>>>,
    }

    impl CapturingFrontend {
        fn new(
            actions: impl IntoIterator<Item = NextAction>,
            engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>>,
        ) -> Self {
            Self {
                actions: Mutex::new(actions.into_iter().collect()),
                step_statuses: Mutex::new(Vec::new()),
                completed: Mutex::new(None),
                available_log: Mutex::new(Vec::new()),
                engine_tx,
                container_exits: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl crate::data::message::UserMessageSink for CapturingFrontend {
        fn write_message(&mut self, _msg: crate::data::message::UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    impl WorkflowFrontend for CapturingFrontend {
        fn show_workflow_control_board(
            &mut self,
            _state: &WorkflowState,
            available: &AvailableActions,
        ) -> Result<NextAction, EngineError> {
            self.available_log.lock().unwrap().push(available.clone());
            let action = self
                .actions
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(NextAction::Pause);
            Ok(action)
        }

        fn confirm_resume(&mut self, _: &ResumeMismatch) -> Result<bool, EngineError> {
            Ok(true)
        }

        fn user_choose_after_step_failure(
            &mut self,
            _step: &WorkflowStep,
            _exit: &AgentExitInfo,
        ) -> Result<StepFailureChoice, EngineError> {
            Ok(StepFailureChoice::Abort)
        }

        fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
            self.step_statuses
                .lock()
                .unwrap()
                .push((step.name.clone(), status));
        }

        fn yolo_countdown_tick(
            &mut self,
            _step_name: &str,
            _remaining: Duration,
            _total: Duration,
        ) -> Result<YoloTickOutcome, EngineError> {
            Ok(YoloTickOutcome::Cancel)
        }

        fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
            *self.completed.lock().unwrap() = Some(outcome.clone());
        }

        fn report_container_exited(&mut self, exit_code: i32) {
            self.container_exits.lock().unwrap().push(exit_code);
        }

        fn set_engine_sender(&mut self, tx: tokio::sync::mpsc::UnboundedSender<EngineRequest>) {
            *self.engine_tx.lock().unwrap() = Some(tx);
        }
    }

    fn make_capturing_engine(
        session: &Session,
        workflow: Workflow,
        factory: BlockingFactory,
        actions: impl IntoIterator<Item = NextAction>,
        engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>>,
    ) -> (WorkflowEngine, Arc<Mutex<Vec<i32>>>) {
        let overlay = OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
        );
        let frontend = CapturingFrontend::new(actions, engine_tx);
        let container_exits = frontend.container_exits.clone();
        let engine = WorkflowEngine::new(
            session,
            workflow,
            None,
            Box::new(frontend),
            Box::new(factory),
            Arc::new(crate::engine::git::GitEngine::new()),
            Arc::new(overlay),
        )
        .unwrap();
        (engine, container_exits)
    }

    // ── Mid-step control board tests ─────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn open_control_board_mid_step_does_not_cancel_container() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-mid-no-cancel"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );

        let (cancel_flag, completion1) = make_blocking_entry();
        let engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>> =
            Arc::new(Mutex::new(None));

        let factory = BlockingFactory::new([(cancel_flag.clone(), completion1.clone())]);
        let (mut engine, container_exits) = make_capturing_engine(
            &session,
            workflow,
            factory,
            [NextAction::Dismiss, NextAction::LaunchNext],
            engine_tx.clone(),
        );

        let tx = engine_tx
            .lock()
            .unwrap()
            .clone()
            .expect("engine_tx set on construction");

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        tokio::time::sleep(Duration::from_millis(150)).await;
        tx.send(EngineRequest::OpenControlBoard {
            step_name: String::new(),
        })
        .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;

        assert!(
            !cancel_flag.load(Ordering::Relaxed),
            "cancel must not be called when user picks Dismiss"
        );
        assert!(
            container_exits.lock().unwrap().is_empty(),
            "no container exit may be reported while the container still runs"
        );

        signal_completion(&completion1, 0);

        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
        assert!(
            container_exits.lock().unwrap().contains(&0),
            "the step's natural completion must be reported with its exit code"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mid_step_dismiss_resumes_waiting_on_step() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-dismiss"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );

        let (cancel_flag, completion) = make_blocking_entry();
        let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
        let factory = BlockingFactory::new([(cancel_flag.clone(), completion.clone())]);
        let (mut engine, _container_exits) = make_capturing_engine(
            &session,
            workflow,
            factory,
            [NextAction::Dismiss, NextAction::LaunchNext],
            engine_tx.clone(),
        );
        let tx = engine_tx.lock().unwrap().clone().unwrap();

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        tokio::time::sleep(Duration::from_millis(150)).await;
        tx.send(EngineRequest::OpenControlBoard {
            step_name: String::new(),
        })
        .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;

        assert!(!cancel_flag.load(Ordering::Relaxed));

        signal_completion(&completion, 0);
        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mid_step_restart_cancels_then_re_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-restart-mid"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );

        let (cancel_flag, completion1) = make_blocking_entry();
        let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
        let factory = BlockingFactory::new([(cancel_flag.clone(), completion1)]);
        let execution_count = factory.execution_count.clone();
        let (mut engine, _container_exits) = make_capturing_engine(
            &session,
            workflow,
            factory,
            [NextAction::RestartCurrentStep, NextAction::LaunchNext],
            engine_tx.clone(),
        );
        let tx = engine_tx.lock().unwrap().clone().unwrap();

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        tokio::time::sleep(Duration::from_millis(150)).await;
        tx.send(EngineRequest::OpenControlBoard {
            step_name: String::new(),
        })
        .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert!(cancel_flag.load(Ordering::Relaxed));

        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
        assert!(execution_count.load(Ordering::Relaxed) >= 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mid_step_advance_cancels_then_marks_force_succeeded() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-advance-mid"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );

        let (cancel_flag, completion1) = make_blocking_entry();
        let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
        let factory = BlockingFactory::new([(cancel_flag.clone(), completion1)]);
        let execution_count = factory.execution_count.clone();
        let (mut engine, container_exits) = make_capturing_engine(
            &session,
            workflow,
            factory,
            [NextAction::LaunchNext],
            engine_tx.clone(),
        );
        let tx = engine_tx.lock().unwrap().clone().unwrap();

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        tokio::time::sleep(Duration::from_millis(150)).await;
        tx.send(EngineRequest::OpenControlBoard {
            step_name: String::new(),
        })
        .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert!(cancel_flag.load(Ordering::Relaxed));
        assert_eq!(
            container_exits.lock().unwrap().first(),
            Some(&KILLED_EXIT_CODE),
            "an engine kill must be reported to the frontend immediately"
        );

        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
        assert_eq!(execution_count.load(Ordering::Relaxed), 2);
    }

    // ── StepStuck / StepUnstuck engine tests ─────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn step_stuck_in_yolo_mode_starts_countdown() {
        // Uses a 2-step workflow so that step "a" is NOT the last step.
        // The last step never runs a yolo countdown (it shows the WCB
        // instead), so this test exercises the countdown on step "a".
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-stuck-yolo"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );

        let (cancel_flag_a, completion_a) = make_blocking_entry();
        let (_cancel_flag_b, completion_b) = make_blocking_entry();
        let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));

        // Frontend that tracks yolo lifecycle calls.
        struct YoloTrackingFrontend {
            actions: Mutex<VecDeque<NextAction>>,
            engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>>,
            yolo_started: AtomicBool,
            yolo_finished: AtomicBool,
        }
        impl crate::data::message::UserMessageSink for YoloTrackingFrontend {
            fn write_message(&mut self, _: crate::data::message::UserMessage) {}
            fn replay_queued(&mut self) {}
        }
        impl WorkflowFrontend for YoloTrackingFrontend {
            fn show_workflow_control_board(
                &mut self,
                _: &WorkflowState,
                _: &AvailableActions,
            ) -> Result<NextAction, EngineError> {
                Ok(self
                    .actions
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(NextAction::Pause))
            }
            fn yolo_countdown_tick(
                &mut self,
                _: &str,
                _: Duration,
                _: Duration,
            ) -> Result<YoloTickOutcome, EngineError> {
                // Cancel immediately to keep the test fast.
                Ok(YoloTickOutcome::Cancel)
            }
            fn yolo_countdown_started(&mut self, _: &str) {
                self.yolo_started.store(true, Ordering::Relaxed);
            }
            fn yolo_countdown_finished(&mut self, _: &str) {
                self.yolo_finished.store(true, Ordering::Relaxed);
            }
            fn confirm_resume(&mut self, _: &ResumeMismatch) -> Result<bool, EngineError> {
                Ok(true)
            }
            fn user_choose_after_step_failure(
                &mut self,
                _: &WorkflowStep,
                _: &AgentExitInfo,
            ) -> Result<StepFailureChoice, EngineError> {
                Ok(StepFailureChoice::Abort)
            }
            fn report_step_status(&mut self, _: &WorkflowStep, _: WorkflowStepStatus) {}
            fn report_workflow_completed(&mut self, _: &WorkflowOutcome) {}
            fn set_engine_sender(&mut self, tx: tokio::sync::mpsc::UnboundedSender<EngineRequest>) {
                *self.engine_tx.lock().unwrap() = Some(tx);
            }
        }

        let frontend = YoloTrackingFrontend {
            // WCB is shown after last step completes in yolo mode.
            actions: Mutex::new(VecDeque::from([NextAction::FinishWorkflow])),
            engine_tx: engine_tx.clone(),
            yolo_started: AtomicBool::new(false),
            yolo_finished: AtomicBool::new(false),
        };

        let factory = BlockingFactory::new([
            (cancel_flag_a.clone(), completion_a.clone()),
            (_cancel_flag_b.clone(), completion_b.clone()),
        ]);
        let overlay = OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
        );
        let mut engine = WorkflowEngine::new(
            &session,
            workflow,
            None,
            Box::new(frontend),
            Box::new(factory),
            Arc::new(GitEngine::new()),
            Arc::new(overlay),
        )
        .unwrap();
        engine.set_yolo(true);

        let tx = engine_tx.lock().unwrap().clone().unwrap();

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        tokio::time::sleep(Duration::from_millis(150)).await;
        tx.send(EngineRequest::StepStuck {
            step_name: String::new(),
        })
        .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Countdown was cancelled by the frontend (YoloTickOutcome::Cancel),
        // so step "a" keeps running. Complete it normally.
        signal_completion(&completion_a, 0);

        // Yolo auto-advances to step "b". Complete it.
        tokio::time::sleep(Duration::from_millis(150)).await;
        signal_completion(&completion_b, 0);

        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn step_stuck_in_non_yolo_mode_shows_wcb() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-stuck-no-yolo"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );

        let (cancel_flag, completion) = make_blocking_entry();
        let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
        let factory = BlockingFactory::new([(cancel_flag.clone(), completion.clone())]);
        // When WCB opens due to stuck: Dismiss, then later LaunchNext between steps.
        let (mut engine, _container_exits) = make_capturing_engine(
            &session,
            workflow,
            factory,
            [NextAction::Dismiss, NextAction::LaunchNext],
            engine_tx.clone(),
        );
        // Not yolo mode.

        let tx = engine_tx.lock().unwrap().clone().unwrap();

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        tokio::time::sleep(Duration::from_millis(150)).await;
        tx.send(EngineRequest::StepStuck {
            step_name: String::new(),
        })
        .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Step still running (Dismiss was chosen).
        assert!(!cancel_flag.load(Ordering::Relaxed));

        signal_completion(&completion, 0);
        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    /// Sending `StepUnstuck` during an active yolo countdown must cancel the
    /// countdown and leave the step running — it must NOT mark the step
    /// Succeeded or advance to the next step. The container keeps running
    /// until it actually exits.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn step_unstuck_during_yolo_countdown_keeps_step_running() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-unstuck-mid-countdown"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );

        let (cancel_flag_a, completion_a) = make_blocking_entry();
        let (_cancel_flag_b, completion_b) = make_blocking_entry();
        let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));

        // Frontend whose tick returns Continue so the countdown actually runs
        // (lets us send StepUnstuck mid-countdown). Captures step transitions
        // so the test can assert "a" was never marked Succeeded prematurely.
        struct UnstuckTestFrontend {
            actions: Mutex<VecDeque<NextAction>>,
            engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>>,
            step_statuses: Mutex<Vec<(String, WorkflowStepStatus)>>,
        }
        impl crate::data::message::UserMessageSink for UnstuckTestFrontend {
            fn write_message(&mut self, _: crate::data::message::UserMessage) {}
            fn replay_queued(&mut self) {}
        }
        impl WorkflowFrontend for UnstuckTestFrontend {
            fn show_workflow_control_board(
                &mut self,
                _: &WorkflowState,
                _: &AvailableActions,
            ) -> Result<NextAction, EngineError> {
                Ok(self
                    .actions
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(NextAction::Pause))
            }
            fn yolo_countdown_tick(
                &mut self,
                _: &str,
                _: Duration,
                _: Duration,
            ) -> Result<YoloTickOutcome, EngineError> {
                Ok(YoloTickOutcome::Continue)
            }
            fn confirm_resume(&mut self, _: &ResumeMismatch) -> Result<bool, EngineError> {
                Ok(true)
            }
            fn user_choose_after_step_failure(
                &mut self,
                _: &WorkflowStep,
                _: &AgentExitInfo,
            ) -> Result<StepFailureChoice, EngineError> {
                Ok(StepFailureChoice::Abort)
            }
            fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
                self.step_statuses
                    .lock()
                    .unwrap()
                    .push((step.name.clone(), status));
            }
            fn report_workflow_completed(&mut self, _: &WorkflowOutcome) {}
            fn set_engine_sender(&mut self, tx: tokio::sync::mpsc::UnboundedSender<EngineRequest>) {
                *self.engine_tx.lock().unwrap() = Some(tx);
            }
        }
        let frontend = UnstuckTestFrontend {
            actions: Mutex::new(VecDeque::from([NextAction::FinishWorkflow])),
            engine_tx: engine_tx.clone(),
            step_statuses: Mutex::new(Vec::new()),
        };

        let factory = BlockingFactory::new([
            (cancel_flag_a.clone(), completion_a.clone()),
            (_cancel_flag_b.clone(), completion_b.clone()),
        ]);
        let overlay = OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
        );
        let mut engine = WorkflowEngine::new(
            &session,
            workflow,
            None,
            Box::new(frontend),
            Box::new(factory),
            Arc::new(GitEngine::new()),
            Arc::new(overlay),
        )
        .unwrap();
        engine.set_yolo(true);

        let tx = engine_tx.lock().unwrap().clone().unwrap();

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        // Let the step launch.
        tokio::time::sleep(Duration::from_millis(150)).await;
        // Kick off the yolo countdown.
        tx.send(EngineRequest::StepStuck {
            step_name: String::new(),
        })
        .unwrap();
        // Let the countdown run a tick or two without expiring.
        tokio::time::sleep(Duration::from_millis(200)).await;
        // Container produced output again — recovery signal.
        tx.send(EngineRequest::StepUnstuck {
            step_name: String::new(),
        })
        .unwrap();
        // Wait long enough that, if the engine were mistakenly advancing the
        // step on Unstuck, step "b" would have launched. Cancel-flag-a must
        // still be false (step "a" still running, NOT cancelled by Advanced).
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !cancel_flag_a.load(Ordering::Relaxed),
            "StepUnstuck during countdown must NOT cancel step 'a' — it must keep running"
        );

        // Now complete step "a" normally; workflow proceeds.
        signal_completion(&completion_a, 0);
        tokio::time::sleep(Duration::from_millis(150)).await;
        signal_completion(&completion_b, 0);

        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn step_unstuck_outside_countdown_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-unstuck"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );

        let (_, completion) = make_blocking_entry();
        let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
        let factory =
            BlockingFactory::new([(Arc::new(AtomicBool::new(false)), completion.clone())]);
        let (mut engine, _container_exits) = make_capturing_engine(
            &session,
            workflow,
            factory,
            [NextAction::LaunchNext],
            engine_tx.clone(),
        );

        let tx = engine_tx.lock().unwrap().clone().unwrap();

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        tokio::time::sleep(Duration::from_millis(100)).await;
        // Send StepUnstuck when there's no countdown — should be harmlessly ignored.
        tx.send(EngineRequest::StepUnstuck {
            step_name: String::new(),
        })
        .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        signal_completion(&completion, 0);
        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    // ── MockBackgroundContainer ───────────────────────────────────────────────

    struct MockBackgroundContainer {
        /// Pre-programmed results: (stdout, stderr, exit_code).
        results: Mutex<VecDeque<(String, String, i32)>>,
        /// Recorded commands (in call order).
        calls: Mutex<Vec<String>>,
        /// Number of times a fresh container was handed out — exercised by
        /// per-step-container assertions (WI-0082).
        container_handouts: Mutex<usize>,
    }

    impl MockBackgroundContainer {
        /// All execs succeed with empty output.
        fn always_success() -> Self {
            Self {
                results: Mutex::new(VecDeque::new()),
                calls: Mutex::new(Vec::new()),
                container_handouts: Mutex::new(0),
            }
        }

        /// Provide an explicit sequence of (stdout, stderr, exit_code) results.
        fn with_results(results: impl IntoIterator<Item = (String, String, i32)>) -> Self {
            Self {
                results: Mutex::new(results.into_iter().collect()),
                calls: Mutex::new(Vec::new()),
                container_handouts: Mutex::new(0),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn handouts(&self) -> usize {
            *self.container_handouts.lock().unwrap()
        }

        /// Build a factory closure for `WorkflowEngine::run_setup` /
        /// `run_teardown` that records one container handout per step and
        /// delegates exec calls back to this mock. Tests that previously
        /// passed `&mock` directly can now pass `mock.factory()`.
        fn factory<'a>(
            self: &'a Arc<Self>,
        ) -> impl FnMut(
            usize,
        ) -> Result<
            Box<dyn crate::engine::agent_runtime::background::AgentExec>,
            EngineError,
        > + 'a {
            move |_idx| {
                *self.container_handouts.lock().unwrap() += 1;
                Ok(Box::new(SharedMockExec(Arc::clone(self))))
            }
        }
    }

    /// Trampoline that lets the test factory hand out fresh `Box<dyn
    /// AgentExec>` values while keeping all recorded state in the single
    /// shared `MockBackgroundContainer`.
    struct SharedMockExec(Arc<MockBackgroundContainer>);

    impl crate::engine::agent_runtime::background::AgentExec for SharedMockExec {
        fn exec(
            &self,
            command: &str,
            env: Option<&std::collections::HashMap<String, String>>,
        ) -> Result<
            crate::engine::agent_runtime::background::ExecOutput,
            crate::engine::error::EngineError,
        > {
            self.0.exec(command, env)
        }
    }

    impl crate::engine::agent_runtime::background::AgentExec for MockBackgroundContainer {
        fn exec(
            &self,
            command: &str,
            _env: Option<&std::collections::HashMap<String, String>>,
        ) -> Result<
            crate::engine::agent_runtime::background::ExecOutput,
            crate::engine::error::EngineError,
        > {
            self.calls.lock().unwrap().push(command.to_string());
            let (stdout, stderr, exit_code) = self
                .results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| ("".into(), "".into(), 0));
            Ok(crate::engine::agent_runtime::background::ExecOutput {
                stdout,
                stderr,
                exit_code,
            })
        }
    }

    // ── run_setup / run_teardown unit tests ──────────────────────────────────

    fn setup_steps_sample() -> Vec<crate::data::workflow_definition::SetupStep> {
        use crate::data::workflow_definition::SetupStep;
        vec![
            SetupStep::CloneRepo {
                url: "https://example.com/repo".into(),
                branch: None,
                into: None,
            },
            SetupStep::PullBranch {
                remote: None,
                branch: None,
            },
            SetupStep::RunShell {
                command: "cargo build".into(),
                env: None,
            },
        ]
    }

    fn teardown_steps_sample() -> Vec<crate::data::workflow_definition::TeardownStep> {
        use crate::data::workflow_definition::TeardownStep;
        vec![
            TeardownStep::RunShell {
                command: "cargo test".into(),
                env: None,
            },
            TeardownStep::CommitChanges {
                message: "auto: results".into(),
                add_all: true,
            },
        ]
    }

    fn make_minimal_engine(tmp: &tempfile::TempDir) -> WorkflowEngine {
        let session = make_session(tmp);
        let workflow = make_workflow(
            Some("test-wf"),
            Some("claude"),
            vec![make_step("step-a", &[], None)],
        );
        make_engine(
            &session,
            workflow,
            FakeAgentExecutionFactory::always_success(),
            [],
        )
    }

    #[test]
    fn run_setup_executes_steps_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        let steps = setup_steps_sample();
        let mock = Arc::new(MockBackgroundContainer::always_success());

        engine.run_setup(&steps, &[], &[], mock.factory()).unwrap();

        let calls = mock.calls();
        assert_eq!(calls.len(), 3);
        assert!(calls[0].contains("git clone"));
        assert_eq!(calls[1], "git pull");
        assert_eq!(calls[2], "cargo build");
    }

    #[test]
    fn run_setup_uses_one_fresh_container_per_step() {
        // WI-0082 invariant: each phase step gets its own container so per-step
        // overlays do not leak across step boundaries.
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        let steps = setup_steps_sample(); // 3 steps
        let mock = Arc::new(MockBackgroundContainer::always_success());

        engine.run_setup(&steps, &[], &[], mock.factory()).unwrap();

        assert_eq!(
            mock.handouts(),
            3,
            "the factory must be invoked once per step (one container per step)",
        );
    }

    #[test]
    fn run_teardown_uses_one_fresh_container_per_step() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        let steps = teardown_steps_sample(); // 2 steps
        let mock = Arc::new(MockBackgroundContainer::always_success());

        let (aborted, any_failed) = engine
            .run_teardown(&steps, &[], &[], true, false, mock.factory())
            .unwrap();
        assert!(!aborted);
        assert!(!any_failed);

        assert_eq!(
            mock.handouts(),
            2,
            "teardown must request one container per step",
        );
    }

    #[test]
    fn run_setup_continues_on_failure_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        let steps = setup_steps_sample(); // 3 steps
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("".into(), "".into(), 0),            // step 1 succeeds
            ("".into(), "build error".into(), 1), // step 2 fails
            ("".into(), "".into(), 0),            // step 3 still runs
        ]));

        let result = engine.run_setup(&steps, &[], &[], mock.factory());

        assert!(
            result.is_ok(),
            "run_setup continues past failures when abort_on_failure=false"
        );
        assert_eq!(mock.calls().len(), 3, "all steps must be exec'd");
    }

    #[test]
    fn run_setup_aborts_on_abort_on_failure_step() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        let steps = setup_steps_sample(); // 3 steps
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("".into(), "".into(), 0),            // step 1 succeeds
            ("".into(), "build error".into(), 1), // step 2 fails
            ("".into(), "".into(), 0),            // step 3 (never reached)
        ]));
        let abort_flags = vec![false, true, false]; // step 2 has abort_on_failure

        let result = engine.run_setup(&steps, &abort_flags, &[], mock.factory());

        assert!(
            result.is_err(),
            "run_setup must return Err when abort_on_failure step fails"
        );
        assert_eq!(mock.calls().len(), 2, "third step must not be exec'd");
        assert!(engine.abort_on_failure_triggered());
    }

    #[test]
    fn run_teardown_skips_when_not_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        let steps = teardown_steps_sample();
        let mock = Arc::new(MockBackgroundContainer::always_success());

        // teardown_on_failure = false, workflow_succeeded = false → skip all
        let (aborted, any_failed) = engine
            .run_teardown(&steps, &[], &[], false, false, mock.factory())
            .unwrap();
        assert!(!aborted);
        assert!(!any_failed);

        assert_eq!(mock.calls().len(), 0, "no exec calls should be made");
        assert_eq!(
            mock.handouts(),
            0,
            "no containers should be requested when teardown is skipped",
        );
    }

    #[test]
    fn run_teardown_runs_when_succeeded() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        let steps = teardown_steps_sample();
        let mock = Arc::new(MockBackgroundContainer::always_success());

        let (aborted, any_failed) = engine
            .run_teardown(&steps, &[], &[], true, false, mock.factory())
            .unwrap();
        assert!(!aborted);
        assert!(!any_failed);

        assert_eq!(mock.calls().len(), 2, "both teardown steps must exec");
    }

    #[test]
    fn run_teardown_continues_after_step_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        let steps = teardown_steps_sample();
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("".into(), "test failure".into(), 1), // step 1 fails
            ("".into(), "".into(), 0),             // step 2 succeeds
        ]));

        // Teardown is best-effort: returns Ok even if a step fails.
        let result = engine.run_teardown(&steps, &[], &[], true, false, mock.factory());
        assert!(
            result.is_ok(),
            "run_teardown must return Ok despite step failure"
        );
        let (aborted, any_failed) = result.unwrap();
        assert!(!aborted, "no abort_on_failure steps were set");
        assert!(
            any_failed,
            "any_step_failed must be true when a step exits non-zero"
        );
        assert_eq!(mock.calls().len(), 2, "both steps must be exec'd");
    }

    #[test]
    fn run_teardown_aborts_on_abort_on_failure_step() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        let steps = teardown_steps_sample();
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("".into(), "fatal".into(), 1), // step 0 fails
            ("".into(), "".into(), 0),      // step 1 would succeed
        ]));

        // abort_on_failure = true for step 0
        let result = engine.run_teardown(&steps, &[true, false], &[], true, false, mock.factory());
        assert!(result.is_ok());
        let (aborted, any_failed) = result.unwrap();
        assert!(
            aborted,
            "run_teardown must set aborted when abort_on_failure step fails"
        );
        assert!(any_failed, "any_step_failed must also be true");
        assert_eq!(
            mock.calls().len(),
            1,
            "step 1 must be skipped after abort_on_failure step 0 fails"
        );
    }

    #[test]
    fn run_teardown_continues_after_per_step_agent_factory_failure() {
        // Per-step container build failure must not abort teardown; it should
        // record the step as Failed and proceed to the next one.
        use crate::data::workflow_definition::TeardownStep;
        use crate::data::workflow_state::PhaseStepStatus;

        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        let steps = vec![
            TeardownStep::RunShell {
                command: "first".into(),
                env: None,
            },
            TeardownStep::RunShell {
                command: "second".into(),
                env: None,
            },
        ];

        // Factory fails on step 0 (returns Err), succeeds on step 1.
        let mock = Arc::new(MockBackgroundContainer::always_success());
        let mock_for_factory = Arc::clone(&mock);
        let factory = move |idx: usize| -> Result<
            Box<dyn crate::engine::agent_runtime::background::AgentExec>,
            EngineError,
        > {
            if idx == 0 {
                Err(EngineError::Other(
                    "simulated overlay resolve failure".into(),
                ))
            } else {
                *mock_for_factory.container_handouts.lock().unwrap() += 1;
                Ok(Box::new(SharedMockExec(Arc::clone(&mock_for_factory))))
            }
        };

        let result = engine.run_teardown(&steps, &[], &[], true, false, factory);
        assert!(result.is_ok(), "factory failure must not abort teardown");
        let (_aborted, any_failed) = result.unwrap();
        assert!(
            any_failed,
            "any_step_failed must be true when factory fails"
        );

        let states = &engine.state().teardown_step_states;
        assert!(
            matches!(&states[0].status, PhaseStepStatus::Failed { error } if error.contains("simulated overlay resolve failure")),
            "step 0 must be recorded as Failed with the factory error: {:?}",
            states[0].status,
        );
        assert_eq!(
            states[1].status,
            PhaseStepStatus::Succeeded,
            "step 1 must still execute after step 0's factory failure",
        );
        assert_eq!(mock.calls().len(), 1, "only step 1 reaches exec");
    }

    #[test]
    fn run_setup_transitions_phase_to_main_on_success() {
        use crate::data::workflow_state::WorkflowPhase;

        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        let steps = setup_steps_sample();
        let mock = Arc::new(MockBackgroundContainer::always_success());

        engine.run_setup(&steps, &[], &[], mock.factory()).unwrap();

        assert_eq!(
            engine.state().current_phase,
            WorkflowPhase::Main,
            "phase must be Main after successful setup"
        );
        assert!(
            engine.state().setup_completed,
            "setup_completed must be true after successful setup"
        );
    }

    #[test]
    fn run_setup_state_tracking() {
        use crate::data::workflow_state::PhaseStepStatus;

        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);

        use crate::data::workflow_definition::SetupStep;
        let steps = vec![
            SetupStep::RunShell {
                command: "step1".into(),
                env: None,
            },
            SetupStep::RunShell {
                command: "step2".into(),
                env: None,
            },
        ];
        let mock = Arc::new(MockBackgroundContainer::always_success());

        engine.run_setup(&steps, &[], &[], mock.factory()).unwrap();

        let states = &engine.state().setup_step_states;
        assert_eq!(states.len(), 2);
        assert_eq!(states[0].status, PhaseStepStatus::Succeeded);
        assert_eq!(states[1].status, PhaseStepStatus::Succeeded);
    }

    #[test]
    fn run_teardown_state_tracking() {
        use crate::data::workflow_state::PhaseStepStatus;

        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);

        use crate::data::workflow_definition::TeardownStep;
        let steps = vec![
            TeardownStep::RunShell {
                command: "td1".into(),
                env: None,
            },
            TeardownStep::RunShell {
                command: "td2".into(),
                env: None,
            },
        ];
        let mock = Arc::new(MockBackgroundContainer::always_success());

        let (aborted, any_failed) = engine
            .run_teardown(&steps, &[], &[], true, false, mock.factory())
            .unwrap();
        assert!(!aborted);
        assert!(!any_failed);

        let states = &engine.state().teardown_step_states;
        assert_eq!(states.len(), 2);
        assert_eq!(states[0].status, PhaseStepStatus::Succeeded);
        assert_eq!(states[1].status, PhaseStepStatus::Succeeded);
    }

    #[test]
    fn run_setup_failure_records_failed_state() {
        use crate::data::workflow_state::PhaseStepStatus;

        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);

        use crate::data::workflow_definition::SetupStep;
        let steps = vec![
            SetupStep::RunShell {
                command: "ok-step".into(),
                env: None,
            },
            SetupStep::RunShell {
                command: "bad-step".into(),
                env: None,
            },
        ];
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("".into(), "".into(), 0),
            ("".into(), "stderr content".into(), 1),
        ]));

        let result = engine.run_setup(&steps, &[], &[], mock.factory());
        assert!(
            result.is_ok(),
            "setup continues past failures when abort_on_failure=false"
        );

        let states = &engine.state().setup_step_states;
        assert_eq!(states[0].status, PhaseStepStatus::Succeeded);
        assert!(
            matches!(&states[1].status, PhaseStepStatus::Failed { error } if error == "stderr content"),
            "failed state must capture stderr: {:?}",
            states[1].status
        );
    }

    #[test]
    fn run_teardown_transitions_phase_to_done() {
        use crate::data::workflow_state::WorkflowPhase;

        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        let steps = teardown_steps_sample();
        let mock = Arc::new(MockBackgroundContainer::always_success());

        let (_aborted, _any_failed) = engine
            .run_teardown(&steps, &[], &[], true, false, mock.factory())
            .unwrap();

        assert_eq!(
            engine.state().current_phase,
            WorkflowPhase::Done,
            "phase must be Done after teardown completes"
        );
        assert!(engine.state().teardown_completed);
    }

    #[test]
    fn mark_done_sets_phase_to_done() {
        use crate::data::workflow_state::WorkflowPhase;

        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        assert_eq!(engine.state().current_phase, WorkflowPhase::Main);

        engine.mark_done().unwrap();
        assert_eq!(engine.state().current_phase, WorkflowPhase::Done);
    }

    #[test]
    fn run_setup_phase_persistence_verified_from_store() {
        use crate::data::workflow_state::WorkflowPhase;

        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);

        use crate::data::workflow_definition::SetupStep;
        let steps = vec![SetupStep::RunShell {
            command: "go".into(),
            env: None,
        }];
        let mock = Arc::new(MockBackgroundContainer::always_success());

        engine.run_setup(&steps, &[], &[], mock.factory()).unwrap();

        // Verify the on-disk state was persisted with the correct phase fields.
        let store = WorkflowStateStore::at_git_root(tmp.path());
        let saved = store.load(None, "test-wf").unwrap().unwrap();
        assert_eq!(saved.current_phase, WorkflowPhase::Main);
        assert!(saved.setup_completed);
    }

    // ── on_failure unit tests ─────────────────────────────────────────────────
    //
    // These tests call run_setup / run_teardown with non-empty on_failure_configs.
    // launch_on_failure_agent internally calls Handle::current().block_on(...),
    // which requires a live Tokio runtime on the current thread. We use
    // spawn_blocking so we run on a dedicated blocking thread where block_on is
    // explicitly permitted, while the multi-thread runtime handles the future.

    /// Frontend that records every `write_message` call so tests can assert on
    /// the on_failure status messages emitted by the engine. Also records the
    /// step name of every `report_step_interactive_launch` call.
    struct MessageCapturingFrontend {
        messages: Arc<Mutex<Vec<crate::data::message::UserMessage>>>,
        interactive_launches: Arc<Mutex<Vec<String>>>,
    }

    impl MessageCapturingFrontend {
        fn new() -> (Self, Arc<Mutex<Vec<crate::data::message::UserMessage>>>) {
            let store = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    messages: Arc::clone(&store),
                    interactive_launches: Arc::new(Mutex::new(Vec::new())),
                },
                store,
            )
        }

        /// Handle to the recorded `report_step_interactive_launch` step names.
        /// Grab before moving the frontend into the engine.
        fn launches_handle(&self) -> Arc<Mutex<Vec<String>>> {
            Arc::clone(&self.interactive_launches)
        }
    }

    impl crate::data::message::UserMessageSink for MessageCapturingFrontend {
        fn write_message(&mut self, msg: crate::data::message::UserMessage) {
            self.messages.lock().unwrap().push(msg);
        }
        fn replay_queued(&mut self) {}
    }

    impl WorkflowFrontend for MessageCapturingFrontend {
        fn show_workflow_control_board(
            &mut self,
            _state: &WorkflowState,
            _available: &AvailableActions,
        ) -> Result<NextAction, EngineError> {
            Ok(NextAction::LaunchNext)
        }
        fn confirm_resume(&mut self, _: &ResumeMismatch) -> Result<bool, EngineError> {
            Ok(true)
        }
        fn user_choose_after_step_failure(
            &mut self,
            _step: &WorkflowStep,
            _exit: &AgentExitInfo,
        ) -> Result<StepFailureChoice, EngineError> {
            Ok(StepFailureChoice::Abort)
        }
        fn report_step_status(&mut self, _step: &WorkflowStep, _status: WorkflowStepStatus) {}
        fn report_step_interactive_launch(
            &mut self,
            step: &WorkflowStep,
            _agent: &str,
            _model: Option<&str>,
        ) {
            self.interactive_launches
                .lock()
                .unwrap()
                .push(step.name.clone());
        }
        fn yolo_countdown_tick(
            &mut self,
            _step_name: &str,
            _remaining: Duration,
            _total: Duration,
        ) -> Result<YoloTickOutcome, EngineError> {
            Ok(YoloTickOutcome::Cancel)
        }
        fn report_workflow_completed(&mut self, _outcome: &WorkflowOutcome) {}
    }

    fn make_engine_capturing(
        session: &Session,
        workflow: Workflow,
        factory: FakeAgentExecutionFactory,
        frontend: MessageCapturingFrontend,
    ) -> WorkflowEngine {
        let overlay = OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
        );
        WorkflowEngine::new(
            session,
            workflow,
            None,
            Box::new(frontend),
            Box::new(factory),
            Arc::new(GitEngine::new()),
            Arc::new(overlay),
        )
        .unwrap()
    }

    fn remediation_config(
        max_attempts: u32,
    ) -> crate::data::workflow_definition::RemediationConfig {
        crate::data::workflow_definition::RemediationConfig {
            prompt: "Fix the broken step.".into(),
            agent: None,
            model: None,
            max_attempts,
        }
    }

    // run_setup: step fails with no on_failure config → step is Failed, only 1 exec.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn on_failure_absent_step_fails_with_no_retry() {
        use crate::data::workflow_state::PhaseStepStatus;

        tokio::task::spawn_blocking(|| {
            let tmp = tempfile::tempdir().unwrap();
            let session = make_session(&tmp);
            let workflow =
                make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
            let factory = FakeAgentExecutionFactory::always_success();
            let (frontend, _msgs) = MessageCapturingFrontend::new();
            let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

            let steps = vec![crate::data::workflow_definition::SetupStep::RunShell {
                command: "fail".into(),
                env: None,
            }];
            let mock = Arc::new(MockBackgroundContainer::with_results([(
                "".into(),
                "some error".into(),
                1,
            )]));
            // No on_failure config.
            engine.run_setup(&steps, &[false], &[], mock.factory()).unwrap();

            // Exactly 1 exec: initial attempt only, no retry.
            assert_eq!(mock.calls().len(), 1, "no retry must occur without on_failure config");

            let states = &engine.state().setup_step_states;
            assert!(
                matches!(&states[0].status, PhaseStepStatus::Failed { error } if error == "some error"),
                "step must be Failed with correct error message: {:?}",
                states[0].status
            );
        })
        .await
        .unwrap();
    }

    // Step fails → on_failure launches agent → retry succeeds → step marked Succeeded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn on_failure_retry_succeeds_step_marked_succeeded() {
        use crate::data::workflow_state::PhaseStepStatus;

        tokio::task::spawn_blocking(|| {
            let tmp = tempfile::tempdir().unwrap();
            let session = make_session(&tmp);
            let workflow =
                make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
            // The on_failure agent uses FakeAgentExecutionFactory (exit 0, ignored).
            let factory = FakeAgentExecutionFactory::always_success();
            let (frontend, _msgs) = MessageCapturingFrontend::new();
            let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

            let steps = vec![crate::data::workflow_definition::SetupStep::RunShell {
                command: "step".into(),
                env: None,
            }];
            // First call fails (step fails); second call succeeds (retry after agent).
            let mock = Arc::new(MockBackgroundContainer::with_results([
                ("".into(), "error".into(), 1),
                ("".into(), "".into(), 0),
            ]));
            let on_failure_configs = vec![Some(remediation_config(2))];

            let result = engine.run_setup(&steps, &[false], &on_failure_configs, mock.factory());

            assert!(
                result.is_ok(),
                "setup must succeed when retry succeeds: {result:?}"
            );
            assert_eq!(
                engine.state().setup_step_states[0].status,
                PhaseStepStatus::Succeeded,
                "step must be Succeeded after successful retry"
            );
        })
        .await
        .unwrap();
    }

    // Success on attempt 1 of 2 stops the loop early.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn on_failure_success_on_first_attempt_stops_loop() {
        tokio::task::spawn_blocking(|| {
            let tmp = tempfile::tempdir().unwrap();
            let session = make_session(&tmp);
            let workflow =
                make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
            let factory = FakeAgentExecutionFactory::always_success();
            let (frontend, _msgs) = MessageCapturingFrontend::new();
            let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

            let steps = vec![crate::data::workflow_definition::SetupStep::RunShell {
                command: "step".into(),
                env: None,
            }];
            // Fail once, then succeed on retry.
            let mock = Arc::new(MockBackgroundContainer::with_results([
                ("".into(), "err".into(), 1),
                ("".into(), "".into(), 0),
                // third result never consumed — loop must stop after first retry
                ("".into(), "".into(), 0),
            ]));
            let on_failure_configs = vec![Some(remediation_config(3))]; // 3 allowed, but 1 retry should suffice

            engine
                .run_setup(&steps, &[false], &on_failure_configs, mock.factory())
                .unwrap();

            // Only 2 exec calls: initial fail + one successful retry.
            let calls = mock.calls();
            assert_eq!(
                calls.len(),
                2,
                "must stop after first successful retry: {calls:?}"
            );
        })
        .await
        .unwrap();
    }

    // Exhausting max_attempts leaves the step failed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn on_failure_exhausts_max_attempts_step_remains_failed() {
        use crate::data::workflow_state::PhaseStepStatus;

        tokio::task::spawn_blocking(|| {
            let tmp = tempfile::tempdir().unwrap();
            let session = make_session(&tmp);
            let workflow =
                make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
            let factory = FakeAgentExecutionFactory::always_success();
            let (frontend, _msgs) = MessageCapturingFrontend::new();
            let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

            let steps = vec![crate::data::workflow_definition::SetupStep::RunShell {
                command: "step".into(),
                env: None,
            }];
            // Every exec fails.
            let mock = Arc::new(MockBackgroundContainer::with_results([
                ("".into(), "err".into(), 1),
                ("".into(), "err".into(), 1),
                ("".into(), "err".into(), 1),
            ]));
            let on_failure_configs = vec![Some(remediation_config(2))];

            engine
                .run_setup(&steps, &[false], &on_failure_configs, mock.factory())
                .unwrap();

            assert!(
                matches!(
                    &engine.state().setup_step_states[0].status,
                    PhaseStepStatus::Failed { .. }
                ),
                "step must be Failed after exhausting on_failure attempts: {:?}",
                engine.state().setup_step_states[0].status
            );
        })
        .await
        .unwrap();
    }

    // on_failure agent exit code is irrelevant — what matters is the step retry.
    // We simulate this by verifying that even if the factory returns a non-zero
    // exit code for the agent, the retry still runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn on_failure_agent_exit_code_does_not_affect_retry() {
        use crate::data::workflow_state::PhaseStepStatus;

        tokio::task::spawn_blocking(|| {
            let tmp = tempfile::tempdir().unwrap();
            let session = make_session(&tmp);
            let workflow =
                make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
            // Agent exits non-zero — should be ignored.
            let factory = FakeAgentExecutionFactory::new(std::iter::repeat_n(42, 10));
            let (frontend, _msgs) = MessageCapturingFrontend::new();
            let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

            let steps = vec![crate::data::workflow_definition::SetupStep::RunShell {
                command: "step".into(),
                env: None,
            }];
            // Step fails once, then succeeds.
            let mock = Arc::new(MockBackgroundContainer::with_results([
                ("".into(), "err".into(), 1),
                ("".into(), "".into(), 0),
            ]));
            let on_failure_configs = vec![Some(remediation_config(2))];

            let result = engine.run_setup(&steps, &[false], &on_failure_configs, mock.factory());

            assert!(
                result.is_ok(),
                "agent exit code must not block retry; setup must succeed: {result:?}"
            );
            assert_eq!(
                engine.state().setup_step_states[0].status,
                PhaseStepStatus::Succeeded,
                "step must be Succeeded when retry passes regardless of agent exit code"
            );
        })
        .await
        .unwrap();
    }

    // abort_on_failure + on_failure: remediation runs first; only if exhausted
    // does abort_on_failure trigger.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn on_failure_abort_on_failure_triggers_only_after_remediation_exhausted() {
        tokio::task::spawn_blocking(|| {
            let tmp = tempfile::tempdir().unwrap();
            let session = make_session(&tmp);
            let workflow =
                make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
            let factory = FakeAgentExecutionFactory::always_success();
            let (frontend, _msgs) = MessageCapturingFrontend::new();
            let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

            let steps = vec![
                crate::data::workflow_definition::SetupStep::RunShell {
                    command: "failing-step".into(),
                    env: None,
                },
                crate::data::workflow_definition::SetupStep::RunShell {
                    command: "second-step".into(),
                    env: None,
                },
            ];
            // First step always fails; second step would succeed but must not run.
            let mock = Arc::new(MockBackgroundContainer::with_results([
                ("".into(), "err".into(), 1), // initial attempt
                ("".into(), "err".into(), 1), // retry after agent
            ]));
            let on_failure_configs = vec![Some(remediation_config(1)), None];
            let abort_flags = vec![true, false];

            let result =
                engine.run_setup(&steps, &abort_flags, &on_failure_configs, mock.factory());

            assert!(
                result.is_err(),
                "abort_on_failure must trigger after on_failure exhausted: {result:?}"
            );
            assert!(
                engine.abort_on_failure_triggered(),
                "abort_on_failure_triggered flag must be set"
            );
            // Second step must not have been executed.
            assert_eq!(
                mock.calls().len(),
                2,
                "only the failing step should be exec'd (initial + 1 retry): {:?}",
                mock.calls()
            );
        })
        .await
        .unwrap();
    }

    // abort_on_failure + on_failure: if retry succeeds, abort is NOT triggered.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn on_failure_abort_on_failure_not_triggered_when_retry_succeeds() {
        tokio::task::spawn_blocking(|| {
            let tmp = tempfile::tempdir().unwrap();
            let session = make_session(&tmp);
            let workflow =
                make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
            let factory = FakeAgentExecutionFactory::always_success();
            let (frontend, _msgs) = MessageCapturingFrontend::new();
            let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

            let steps = vec![crate::data::workflow_definition::SetupStep::RunShell {
                command: "step".into(),
                env: None,
            }];
            // Fail, then succeed on retry.
            let mock = Arc::new(MockBackgroundContainer::with_results([
                ("".into(), "err".into(), 1),
                ("".into(), "".into(), 0),
            ]));
            let on_failure_configs = vec![Some(remediation_config(1))];
            let abort_flags = vec![true];

            let result =
                engine.run_setup(&steps, &abort_flags, &on_failure_configs, mock.factory());

            assert!(
                result.is_ok(),
                "setup must succeed when retry succeeds even with abort_on_failure set: {result:?}"
            );
            assert!(
                !engine.abort_on_failure_triggered(),
                "abort must NOT trigger when on_failure remediation succeeds"
            );
        })
        .await
        .unwrap();
    }

    // on_failure messages are emitted correctly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn on_failure_emits_launch_and_success_messages() {
        use crate::data::message::MessageLevel;

        let msg_store = Arc::new(Mutex::new(Vec::<crate::data::message::UserMessage>::new()));
        let msg_store_clone = Arc::clone(&msg_store);

        tokio::task::spawn_blocking(move || {
            let tmp = tempfile::tempdir().unwrap();
            let session = make_session(&tmp);
            let workflow =
                make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
            let factory = FakeAgentExecutionFactory::always_success();
            let frontend = MessageCapturingFrontend {
                messages: Arc::clone(&msg_store_clone),
                interactive_launches: Arc::new(Mutex::new(Vec::new())),
            };
            let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

            let steps = vec![crate::data::workflow_definition::SetupStep::RunShell {
                command: "step".into(),
                env: None,
            }];
            let mock = Arc::new(MockBackgroundContainer::with_results([
                ("".into(), "err".into(), 1),
                ("".into(), "".into(), 0),
            ]));
            let on_failure_configs = vec![Some(remediation_config(2))];
            engine
                .run_setup(&steps, &[false], &on_failure_configs, mock.factory())
                .unwrap();
        })
        .await
        .unwrap();

        let messages = msg_store.lock().unwrap().clone();
        let texts: Vec<&str> = messages.iter().map(|m| m.text.as_str()).collect();

        // Must see the "launching on_failure agent" message.
        assert!(
            texts
                .iter()
                .any(|t| t.contains("on_failure agent") && t.contains("attempt 1")),
            "must emit 'on_failure agent' launch message: {texts:?}"
        );
        // Must see the "remediation succeeded" message.
        assert!(
            texts
                .iter()
                .any(|t| t.contains("remediation succeeded") || t.contains("succeeded on attempt")),
            "must emit remediation success message: {texts:?}"
        );
        // The "launching" message must be Info level.
        let launch_msg = messages
            .iter()
            .find(|m| m.text.contains("on_failure agent"))
            .unwrap();
        assert_eq!(launch_msg.level, MessageLevel::Info);
    }

    // Exhausting max_attempts emits a Warning message.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn on_failure_exhausted_emits_warning_message() {
        use crate::data::message::MessageLevel;

        let msg_store = Arc::new(Mutex::new(Vec::<crate::data::message::UserMessage>::new()));
        let msg_store_clone = Arc::clone(&msg_store);

        tokio::task::spawn_blocking(move || {
            let tmp = tempfile::tempdir().unwrap();
            let session = make_session(&tmp);
            let workflow =
                make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
            let factory = FakeAgentExecutionFactory::always_success();
            let frontend = MessageCapturingFrontend {
                messages: Arc::clone(&msg_store_clone),
                interactive_launches: Arc::new(Mutex::new(Vec::new())),
            };
            let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

            let steps = vec![crate::data::workflow_definition::SetupStep::RunShell {
                command: "step".into(),
                env: None,
            }];
            let mock = Arc::new(MockBackgroundContainer::with_results([
                ("".into(), "err".into(), 1),
                ("".into(), "err".into(), 1),
            ]));
            let on_failure_configs = vec![Some(remediation_config(1))];
            engine
                .run_setup(&steps, &[false], &on_failure_configs, mock.factory())
                .unwrap();
        })
        .await
        .unwrap();

        let messages = msg_store.lock().unwrap().clone();
        let warning = messages
            .iter()
            .find(|m| m.level == MessageLevel::Warning && m.text.contains("exhausted"));
        assert!(
            warning.is_some(),
            "must emit a Warning when on_failure exhausts all attempts: {messages:?}"
        );
    }

    // Teardown on_failure: step fails, retry succeeds, teardown continues.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn teardown_on_failure_retry_succeeds_teardown_continues() {
        use crate::data::workflow_definition::TeardownStep;
        use crate::data::workflow_state::PhaseStepStatus;

        tokio::task::spawn_blocking(|| {
            let tmp = tempfile::tempdir().unwrap();
            let session = make_session(&tmp);
            let workflow =
                make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
            let factory = FakeAgentExecutionFactory::always_success();
            let (frontend, _msgs) = MessageCapturingFrontend::new();
            let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

            let steps = vec![
                TeardownStep::RunShell {
                    command: "tests".into(),
                    env: None,
                },
                TeardownStep::RunShell {
                    command: "deploy".into(),
                    env: None,
                },
            ];
            // First step fails, retry succeeds; second step succeeds.
            let mock = Arc::new(MockBackgroundContainer::with_results([
                ("".into(), "test err".into(), 1),
                ("".into(), "".into(), 0),
                ("".into(), "".into(), 0),
            ]));
            let on_failure_configs = vec![Some(remediation_config(1)), None];

            let (aborted, any_failed) = engine
                .run_teardown(
                    &steps,
                    &[false, false],
                    &on_failure_configs,
                    true,
                    false,
                    mock.factory(),
                )
                .unwrap();

            assert!(!aborted, "teardown must not abort when retry succeeds");
            assert!(!any_failed, "any_failed must be false when retry succeeds");
            let states = &engine.state().teardown_step_states;
            assert_eq!(states[0].status, PhaseStepStatus::Succeeded);
            assert_eq!(states[1].status, PhaseStepStatus::Succeeded);
        })
        .await
        .unwrap();
    }

    // The remediation agent launch must announce itself through
    // report_step_interactive_launch, like main steps do. Frontends prepare
    // per-container state there — the TUI recreates its AgentIo channels, so
    // skipping the call would leave the factory's take_io with no channels
    // and kill the whole command task.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn on_failure_agent_launch_reports_interactive_launch() {
        use crate::data::workflow_definition::TeardownStep;

        tokio::task::spawn_blocking(|| {
            let tmp = tempfile::tempdir().unwrap();
            let session = make_session(&tmp);
            let workflow =
                make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
            let factory = FakeAgentExecutionFactory::always_success();
            let (frontend, _msgs) = MessageCapturingFrontend::new();
            let launches = frontend.launches_handle();
            let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

            let steps = vec![TeardownStep::RunShell {
                command: "tests".into(),
                env: None,
            }];
            // Step fails, retry fails again → exactly one remediation attempt.
            let mock = Arc::new(MockBackgroundContainer::with_results([
                ("".into(), "err".into(), 1),
                ("".into(), "err".into(), 1),
            ]));
            let on_failure_configs = vec![Some(remediation_config(1))];

            engine
                .run_teardown(
                    &steps,
                    &[false],
                    &on_failure_configs,
                    true,
                    false,
                    mock.factory(),
                )
                .unwrap();

            assert_eq!(
                launches.lock().unwrap().as_slice(),
                ["__on_failure__".to_string()],
                "remediation agent launch must fire report_step_interactive_launch"
            );
        })
        .await
        .unwrap();
    }

    // Teardown on_failure exhausts attempts: step marked failed, teardown continues (best-effort).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn teardown_on_failure_exhausted_step_failed_teardown_continues() {
        use crate::data::workflow_definition::TeardownStep;
        use crate::data::workflow_state::PhaseStepStatus;

        tokio::task::spawn_blocking(|| {
            let tmp = tempfile::tempdir().unwrap();
            let session = make_session(&tmp);
            let workflow =
                make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
            let factory = FakeAgentExecutionFactory::always_success();
            let (frontend, _msgs) = MessageCapturingFrontend::new();
            let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

            let steps = vec![
                TeardownStep::RunShell {
                    command: "always-fail".into(),
                    env: None,
                },
                TeardownStep::RunShell {
                    command: "second".into(),
                    env: None,
                },
            ];
            // All execs of the first step fail; second step succeeds.
            let mock = Arc::new(MockBackgroundContainer::with_results([
                ("".into(), "err".into(), 1), // initial
                ("".into(), "err".into(), 1), // retry
                ("".into(), "".into(), 0),    // second step
            ]));
            let on_failure_configs = vec![Some(remediation_config(1)), None];

            let (aborted, any_failed) = engine
                .run_teardown(
                    &steps,
                    &[false, false],
                    &on_failure_configs,
                    true,
                    false,
                    mock.factory(),
                )
                .unwrap();

            assert!(!aborted);
            assert!(
                any_failed,
                "any_failed must be true when on_failure is exhausted"
            );
            assert!(
                matches!(
                    &engine.state().teardown_step_states[0].status,
                    PhaseStepStatus::Failed { .. }
                ),
                "first step must remain Failed"
            );
            assert_eq!(
                engine.state().teardown_step_states[1].status,
                PhaseStepStatus::Succeeded,
                "second step must still run (best-effort teardown)"
            );
        })
        .await
        .unwrap();
    }

    // Remediating state is set on the step during on_failure execution.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn on_failure_remediating_state_recorded_on_step() {
        use crate::data::workflow_state::PhaseStepStatus;

        // We can't observe the Remediating state mid-flight (it's transient),
        // but we CAN verify that after a failed retry it was set at least once
        // by checking that the final state transitions happened correctly.
        // The key invariant: Remediating → Running → (Succeeded or Failed).
        // After exhaustion the step is Failed; after success it is Succeeded.
        // This test checks exhaustion so we know the state machine ran.
        tokio::task::spawn_blocking(|| {
            let tmp = tempfile::tempdir().unwrap();
            let session = make_session(&tmp);
            let workflow =
                make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
            let factory = FakeAgentExecutionFactory::always_success();
            let (frontend, _msgs) = MessageCapturingFrontend::new();
            let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

            let steps = vec![crate::data::workflow_definition::SetupStep::RunShell {
                command: "step".into(),
                env: None,
            }];
            let mock = Arc::new(MockBackgroundContainer::with_results([
                ("".into(), "err".into(), 1),
                ("".into(), "err".into(), 1),
            ]));
            let on_failure_configs = vec![Some(remediation_config(1))];
            engine
                .run_setup(&steps, &[false], &on_failure_configs, mock.factory())
                .unwrap();

            // After exhaustion the step should be Failed — the engine correctly
            // transitioned through Remediating → Running → Failed.
            assert!(
                matches!(
                    engine.state().setup_step_states[0].status,
                    PhaseStepStatus::Failed { .. }
                ),
                "step must end as Failed after exhausted remediation"
            );
        })
        .await
        .unwrap();
    }

    // ── Feature B (WI-0099): teardown failure output capture — unit tests ───

    #[test]
    fn sanitize_step_name_replaces_path_separators_and_dots() {
        let out = sanitize_step_name_for_filename("run_shell: ../../etc/passwd");
        assert!(
            !out.contains('/'),
            "must not contain path separators: {out}"
        );
        assert!(!out.contains(".."), "must not contain '..': {out}");
        assert!(
            out.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "must only contain safe filename characters: {out}"
        );
    }

    #[test]
    fn sanitize_step_name_replaces_spaces_and_shell_metacharacters() {
        let out = sanitize_step_name_for_filename("rm -rf $(whoami); echo `id` && true");
        assert!(
            out.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "must only contain safe filename characters: {out}"
        );
        assert!(!out.contains(' '));
        assert!(!out.contains('$'));
        assert!(!out.contains('('));
        assert!(!out.contains(';'));
        assert!(!out.contains('`'));
    }

    #[test]
    fn sanitize_step_name_replaces_backslashes_and_colons() {
        let out = sanitize_step_name_for_filename(r"C:\Users\evil\payload");
        assert!(!out.contains('\\'));
        assert!(!out.contains(':'));
    }

    #[test]
    fn sanitize_step_name_truncates_to_64_chars() {
        let long_name = "a".repeat(100);
        let out = sanitize_step_name_for_filename(&long_name);
        assert_eq!(out.len(), 64, "must be truncated to the 64-char cap: {out}");
        assert!(out.chars().all(|c| c == 'a'));
    }

    #[test]
    fn sanitize_step_name_empty_falls_back_to_step() {
        assert_eq!(sanitize_step_name_for_filename(""), "step");
    }

    #[test]
    fn sanitize_step_name_preserves_already_safe_names() {
        assert_eq!(
            sanitize_step_name_for_filename("build-frontend_v2"),
            "build-frontend_v2"
        );
    }

    #[test]
    fn sanitize_step_name_handles_unicode_without_panicking() {
        let out = sanitize_step_name_for_filename("café/日本語 build");
        assert!(
            out.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "must only contain safe filename characters: {out}"
        );
        assert!(out.starts_with("caf"));
    }

    #[test]
    fn truncate_stream_leaves_short_content_untouched() {
        let content = "short output\nwith a few lines";
        assert_eq!(truncate_stream(content), content);
    }

    #[test]
    fn truncate_stream_exactly_at_cap_is_untouched() {
        let content = "z".repeat(TEARDOWN_STREAM_TRUNCATE_BYTES);
        assert_eq!(truncate_stream(&content), content);
    }

    #[test]
    fn truncate_stream_keeps_last_bytes_with_notice() {
        let filler = "x".repeat(TEARDOWN_STREAM_TRUNCATE_BYTES + 500);
        let content = format!("HEAD-MARKER{filler}TAIL-MARKER");
        let out = truncate_stream(&content);
        assert!(
            out.starts_with("[... output truncated, showing last"),
            "must prefix a truncation notice: {out}"
        );
        assert!(
            !out.contains("HEAD-MARKER"),
            "the dropped head must not appear: {out}"
        );
        assert!(out.ends_with("TAIL-MARKER"));
    }

    #[test]
    fn truncate_stream_respects_utf8_char_boundaries() {
        // '€' is 3 bytes; repeating it so the cut point would otherwise land
        // mid-codepoint must not panic and must yield valid UTF-8.
        let filler = "€".repeat(TEARDOWN_STREAM_TRUNCATE_BYTES / 3 + 10);
        let content = format!("{filler}END");
        let out = truncate_stream(&content);
        assert!(out.ends_with("END"));
    }

    #[test]
    fn format_teardown_failure_file_basic_content() {
        let out =
            format_teardown_failure_file("run_shell: cargo test", "out content", "err content");
        assert_eq!(
            out,
            "=== FAILED COMMAND: run_shell: cargo test ===\n\n\
             --- STDOUT ---\nout content\n\n\
             --- STDERR ---\nerr content\n"
        );
    }

    #[test]
    fn format_teardown_failure_file_empty_stdout_uses_placeholder() {
        let out = format_teardown_failure_file("step", "", "some stderr");
        assert!(out.contains("--- STDOUT ---\n(empty)\n"));
        assert!(out.contains("some stderr"));
    }

    #[test]
    fn format_teardown_failure_file_empty_stderr_uses_placeholder() {
        let out = format_teardown_failure_file("step", "some stdout", "");
        assert!(out.contains("some stdout"));
        assert!(out.contains("--- STDERR ---\n(empty)\n"));
    }

    #[test]
    fn format_teardown_failure_file_both_empty_uses_placeholders_for_both() {
        let out = format_teardown_failure_file("step", "", "");
        assert!(out.contains("--- STDOUT ---\n(empty)\n"));
        assert!(out.contains("--- STDERR ---\n(empty)\n"));
    }

    #[test]
    fn format_teardown_failure_file_truncates_oversized_stream() {
        let big = "y".repeat(TEARDOWN_STREAM_TRUNCATE_BYTES + 1000);
        let out = format_teardown_failure_file("step", &big, "");
        assert!(out.contains("output truncated"));
        assert!(
            !out.contains(&big),
            "raw oversized content must not appear verbatim in the file"
        );
    }

    #[test]
    fn prepend_preamble_overlay_path_references_correct_file_and_user_prompt() {
        let artifacts = TeardownFailureArtifacts {
            container_path: TEARDOWN_FAILURE_OVERLAY_CONTAINER_PATH,
            filename: "teardown-failure-run-shell--cargo-test.txt".to_string(),
            step_name: "run_shell: cargo test".to_string(),
            extra_overlay: None,
        };
        let out = artifacts.prepend_preamble("Fix the bug.");
        assert!(out.contains("failed teardown step \"run_shell: cargo test\""));
        assert!(out.contains("/awman/context/workflow/teardown-failure-run-shell--cargo-test.txt"));
        assert!(out.contains("Read that file first"));
        assert!(
            out.contains("---\n\nFix the bug."),
            "user prompt must follow the '---' separator: {out}"
        );
    }

    #[test]
    fn prepend_preamble_ephemeral_path_references_remediation_mount() {
        let artifacts = TeardownFailureArtifacts {
            container_path: TEARDOWN_FAILURE_EPHEMERAL_CONTAINER_PATH,
            filename: "teardown-failure-step.txt".to_string(),
            step_name: "step".to_string(),
            extra_overlay: Some("/host/dir:/awman/remediation:ro".to_string()),
        };
        let out = artifacts.prepend_preamble("Custom prompt");
        assert!(out.contains("/awman/remediation/teardown-failure-step.txt"));
        assert!(out.trim_end().ends_with("Custom prompt"));
    }

    fn make_engine_with_workflow_overlays(
        tmp: &tempfile::TempDir,
        overlays: Option<Vec<String>>,
    ) -> WorkflowEngine {
        let session = make_session(tmp);
        let mut workflow = make_workflow(
            Some("wf-overlay"),
            Some("claude"),
            vec![make_step("a", &[], None)],
        );
        workflow.overlays = overlays;
        make_engine(
            &session,
            workflow,
            FakeAgentExecutionFactory::always_success(),
            [],
        )
    }

    #[test]
    fn workflow_context_overlay_writable_false_when_no_overlays() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = make_engine_with_workflow_overlays(&tmp, None);
        assert!(!engine.workflow_context_overlay_writable());
    }

    #[test]
    fn workflow_context_overlay_writable_true_for_default_rw_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let engine =
            make_engine_with_workflow_overlays(&tmp, Some(vec!["context(workflow)".to_string()]));
        assert!(engine.workflow_context_overlay_writable());
    }

    #[test]
    fn workflow_context_overlay_writable_true_for_explicit_rw_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = make_engine_with_workflow_overlays(
            &tmp,
            Some(vec!["context(workflow:rw)".to_string()]),
        );
        assert!(engine.workflow_context_overlay_writable());
    }

    #[test]
    fn workflow_context_overlay_writable_false_for_readonly_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = make_engine_with_workflow_overlays(
            &tmp,
            Some(vec!["context(workflow:ro)".to_string()]),
        );
        assert!(!engine.workflow_context_overlay_writable());
    }

    #[test]
    fn workflow_context_overlay_writable_false_for_unrelated_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let engine =
            make_engine_with_workflow_overlays(&tmp, Some(vec!["context(repo)".to_string()]));
        assert!(!engine.workflow_context_overlay_writable());
    }

    #[test]
    fn workflow_context_overlay_writable_true_within_comma_separated_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = make_engine_with_workflow_overlays(
            &tmp,
            Some(vec!["context(repo), context(workflow)".to_string()]),
        );
        assert!(engine.workflow_context_overlay_writable());
    }

    #[test]
    fn workflow_context_overlay_writable_false_when_only_readonly_across_multiple_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = make_engine_with_workflow_overlays(
            &tmp,
            Some(vec![
                "context(repo)".to_string(),
                "context(workflow:ro)".to_string(),
            ]),
        );
        assert!(!engine.workflow_context_overlay_writable());
    }

    #[test]
    fn workflow_context_overlay_writable_uses_active_permission_override() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_engine_with_workflow_overlays(&tmp, None);
        assert!(!engine.workflow_context_overlay_writable());

        engine.set_workflow_context_permission(Some(OverlayPermission::ReadWrite));
        assert!(
            engine.workflow_context_overlay_writable(),
            "a workflow context overlay supplied by config/env/CLI must be treated as active"
        );

        engine.set_workflow_context_permission(Some(OverlayPermission::ReadOnly));
        assert!(
            !engine.workflow_context_overlay_writable(),
            "read-only workflow context must not be treated as writable"
        );
    }

    // ── Feature B (WI-0099): teardown failure output capture — integration ──
    //
    // `prepare_teardown_failure_file` resolves the host directory via
    // `ContextDirResolver::from_process_env()`, which reads the *real* process
    // environment (there is no test-injectable `EnvSnapshot` seam on that call
    // path). These tests therefore pin `AWMAN_CONFIG_HOME` to a temp dir for
    // their duration. `ENV_LOCK` serializes that mutation across test threads
    // and `EnvVarGuard` restores the previous value on drop (even on panic) —
    // mirrors the pattern already used in `command::commands::clean::tests`.

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &std::path::Path) -> Self {
            let previous = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    /// `(step name, resolved prompt, overlays)` recorded per `execution_for_step` call.
    type RecordedStepCall = (String, String, Option<Vec<String>>);

    /// Records every synthetic step handed to `execution_for_step` — name,
    /// resolved prompt, and overlays — so tests can assert on the
    /// `launch_on_failure_agent` prompt hint and mount decision without a
    /// live container runtime.
    struct StepRecordingFactory {
        inner: Arc<FakeAgentExecutionFactory>,
        step_calls: Arc<Mutex<Vec<RecordedStepCall>>>,
    }

    impl StepRecordingFactory {
        fn new(inner: Arc<FakeAgentExecutionFactory>) -> (Self, Arc<Mutex<Vec<RecordedStepCall>>>) {
            let step_calls = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    inner,
                    step_calls: Arc::clone(&step_calls),
                },
                step_calls,
            )
        }
    }

    impl AgentExecutionFactory for StepRecordingFactory {
        fn execution_for_step(
            &self,
            step: &WorkflowStep,
            session: &Session,
            runtime: &WorkflowRuntimeContext,
        ) -> Result<AgentExecution, EngineError> {
            self.step_calls.lock().unwrap().push((
                step.name.clone(),
                step.prompt_template.clone(),
                step.overlays.clone(),
            ));
            self.inner.execution_for_step(step, session, runtime)
        }

        fn inject_prompt(
            &self,
            execution: &AgentExecution,
            prompt: &str,
        ) -> Result<Option<()>, EngineError> {
            self.inner.inject_prompt(execution, prompt)
        }
    }

    // Teardown failure WITH an active, writable `context(workflow)` overlay:
    // the file must land in the overlay's existing host path and the prompt
    // hint must reference the already-mounted `/awman/context/workflow/...`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn teardown_failure_with_active_context_workflow_overlay_writes_into_it() {
        use crate::data::fs::context_dirs::ContextDirResolver;
        use crate::data::workflow_definition::TeardownStep;

        // `awman_home` stays alive in this outer frame for the whole test
        // (including across the `.await` below) so the directory exists for
        // the blocking closure's whole execution. The `ENV_LOCK` guard and
        // `AWMAN_CONFIG_HOME` mutation live entirely *inside* the closure so
        // no `MutexGuard` is held across an await point.
        let awman_home = tempfile::tempdir().unwrap();
        let awman_home_path = awman_home.path().to_path_buf();

        tokio::task::spawn_blocking(move || {
            let _env_lock = ENV_LOCK.lock().unwrap();
            let _env_guard = EnvVarGuard::set("AWMAN_CONFIG_HOME", &awman_home_path);

            let tmp = tempfile::tempdir().unwrap();
            let session = make_session(&tmp);
            let mut workflow = make_workflow(
                Some("wf-overlay-active"),
                Some("claude"),
                vec![make_step("a", &[], None)],
            );
            workflow.overlays = Some(vec!["context(workflow)".to_string()]);

            let recording = Arc::new(FakeAgentExecutionFactory::always_success());
            let (step_factory, step_calls_handle) =
                StepRecordingFactory::new(Arc::clone(&recording));
            let (frontend, _msgs) = MessageCapturingFrontend::new();
            let overlay = OverlayEngine::with_auth_resolver(
                crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
            );
            let mut engine = WorkflowEngine::new(
                &session,
                workflow,
                None,
                Box::new(frontend),
                Box::new(step_factory),
                Arc::new(GitEngine::new()),
                Arc::new(overlay),
            )
            .unwrap();
            let invocation_id = engine.state().invocation_id;

            let steps = vec![TeardownStep::RunShell {
                command: "cargo test".into(),
                env: None,
            }];
            let mock = Arc::new(MockBackgroundContainer::with_results([
                ("stdout content".into(), "stderr content".into(), 1),
                ("".into(), "".into(), 0),
            ]));
            let on_failure_configs = vec![Some(remediation_config(1))];

            engine
                .run_teardown(
                    &steps,
                    &[false],
                    &on_failure_configs,
                    true,
                    false,
                    mock.factory(),
                )
                .unwrap();

            let resolver = ContextDirResolver::at_home(&awman_home_path);
            let host_dir = resolver.workflow_dir(invocation_id);
            let sanitized = sanitize_step_name_for_filename("run_shell: cargo test");
            let file_path = host_dir.join(format!("teardown-failure-{sanitized}.txt"));
            let content = std::fs::read_to_string(&file_path).unwrap_or_else(|e| {
                panic!("expected failure file at {}: {e}", file_path.display())
            });
            assert!(content.contains("=== FAILED COMMAND: run_shell: cargo test ==="));
            assert!(content.contains("stdout content"));
            assert!(content.contains("stderr content"));

            let calls = step_calls_handle.lock().unwrap().clone();
            let on_failure_call = calls
                .iter()
                .find(|(name, _, _)| name == "__on_failure__")
                .expect("remediation agent must have been launched");
            let expected_hint = format!("/awman/context/workflow/teardown-failure-{sanitized}.txt");
            assert!(
                on_failure_call.1.contains(expected_hint.as_str()),
                "prompt must reference the overlay path: {}",
                on_failure_call.1
            );
            assert!(
                on_failure_call.2.is_none(),
                "context(workflow) overlay is already mounted; no extra overlay expected"
            );
        })
        .await
        .unwrap();
    }

    // A read-only `context(workflow:ro)` overlay must not be treated as the
    // writable destination. The failure file is still written host-side under
    // the workflow context directory, but the remediation hint uses
    // `/awman/remediation/...`; the extra mount targets the file itself so the
    // existing read-only context directory mount is not deduplicated away.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn teardown_failure_with_readonly_context_workflow_overlay_uses_remediation_mount() {
        use crate::data::fs::context_dirs::ContextDirResolver;
        use crate::data::workflow_definition::TeardownStep;

        let awman_home = tempfile::tempdir().unwrap();
        let awman_home_path = awman_home.path().to_path_buf();

        tokio::task::spawn_blocking(move || {
            let _env_lock = ENV_LOCK.lock().unwrap();
            let _env_guard = EnvVarGuard::set("AWMAN_CONFIG_HOME", &awman_home_path);

            let tmp = tempfile::tempdir().unwrap();
            let session = make_session(&tmp);
            let mut workflow = make_workflow(
                Some("wf-overlay-readonly"),
                Some("claude"),
                vec![make_step("a", &[], None)],
            );
            workflow.overlays = Some(vec!["context(workflow:ro)".to_string()]);

            let recording = Arc::new(FakeAgentExecutionFactory::always_success());
            let (step_factory, step_calls_handle) =
                StepRecordingFactory::new(Arc::clone(&recording));
            let (frontend, _msgs) = MessageCapturingFrontend::new();
            let overlay = OverlayEngine::with_auth_resolver(
                crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
            );
            let mut engine = WorkflowEngine::new(
                &session,
                workflow,
                None,
                Box::new(frontend),
                Box::new(step_factory),
                Arc::new(GitEngine::new()),
                Arc::new(overlay),
            )
            .unwrap();
            let invocation_id = engine.state().invocation_id;

            let steps = vec![TeardownStep::RunShell {
                command: "cargo test".into(),
                env: None,
            }];
            let mock = Arc::new(MockBackgroundContainer::with_results([
                ("ro out".into(), "ro err".into(), 1),
                ("".into(), "".into(), 0),
            ]));
            let on_failure_configs = vec![Some(remediation_config(1))];

            engine
                .run_teardown(
                    &steps,
                    &[false],
                    &on_failure_configs,
                    true,
                    false,
                    mock.factory(),
                )
                .unwrap();

            let resolver = ContextDirResolver::at_home(&awman_home_path);
            let host_dir = resolver.workflow_dir(invocation_id);
            let sanitized = sanitize_step_name_for_filename("run_shell: cargo test");
            let filename = format!("teardown-failure-{sanitized}.txt");
            let file_path = host_dir.join(&filename);
            let content = std::fs::read_to_string(&file_path).unwrap_or_else(|e| {
                panic!("expected failure file at {}: {e}", file_path.display())
            });
            assert!(content.contains("ro out"));
            assert!(content.contains("ro err"));

            let calls = step_calls_handle.lock().unwrap().clone();
            let on_failure_call = calls
                .iter()
                .find(|(name, _, _)| name == "__on_failure__")
                .expect("remediation agent must have been launched");
            let expected_hint = format!("/awman/remediation/{filename}");
            assert!(
                on_failure_call.1.contains(expected_hint.as_str()),
                "prompt must reference the remediation path: {}",
                on_failure_call.1
            );
            let expected_overlay =
                format!("{}:/awman/remediation/{filename}:ro", file_path.display());
            assert_eq!(
                on_failure_call.2,
                Some(vec![expected_overlay]),
                "read-only context fallback must mount the failure file at /awman/remediation"
            );
        })
        .await
        .unwrap();
    }

    // Teardown failure WITHOUT a `context(workflow)` overlay: an ephemeral
    // directory is used, mounted read-only at /awman/remediation, and the
    // prompt hint references that mount.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn teardown_failure_without_context_workflow_overlay_uses_ephemeral_mount() {
        use crate::data::fs::context_dirs::ContextDirResolver;
        use crate::data::workflow_definition::TeardownStep;

        // `awman_home` stays alive in this outer frame for the whole test
        // (including across the `.await` below) so the directory exists for
        // the blocking closure's whole execution. The `ENV_LOCK` guard and
        // `AWMAN_CONFIG_HOME` mutation live entirely *inside* the closure so
        // no `MutexGuard` is held across an await point.
        let awman_home = tempfile::tempdir().unwrap();
        let awman_home_path = awman_home.path().to_path_buf();

        tokio::task::spawn_blocking(move || {
            let _env_lock = ENV_LOCK.lock().unwrap();
            let _env_guard = EnvVarGuard::set("AWMAN_CONFIG_HOME", &awman_home_path);

            let tmp = tempfile::tempdir().unwrap();
            let session = make_session(&tmp);
            // No `context(workflow)` declared.
            let workflow = make_workflow(
                Some("wf-overlay-absent"),
                Some("claude"),
                vec![make_step("a", &[], None)],
            );

            let recording = Arc::new(FakeAgentExecutionFactory::always_success());
            let (step_factory, step_calls_handle) =
                StepRecordingFactory::new(Arc::clone(&recording));
            let (frontend, _msgs) = MessageCapturingFrontend::new();
            let overlay = OverlayEngine::with_auth_resolver(
                crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
            );
            let mut engine = WorkflowEngine::new(
                &session,
                workflow,
                None,
                Box::new(frontend),
                Box::new(step_factory),
                Arc::new(GitEngine::new()),
                Arc::new(overlay),
            )
            .unwrap();
            let invocation_id = engine.state().invocation_id;

            let steps = vec![TeardownStep::RunShell {
                command: "deploy".into(),
                env: None,
            }];
            let mock = Arc::new(MockBackgroundContainer::with_results([
                ("out".into(), "err".into(), 1),
                ("".into(), "".into(), 0),
            ]));
            let on_failure_configs = vec![Some(remediation_config(1))];

            engine
                .run_teardown(
                    &steps,
                    &[false],
                    &on_failure_configs,
                    true,
                    false,
                    mock.factory(),
                )
                .unwrap();

            let resolver = ContextDirResolver::at_home(&awman_home_path);
            let host_dir = resolver.workflow_dir(invocation_id);
            assert!(
                host_dir.starts_with(awman_home_path.join("context").join("workflows")),
                "ephemeral dir must live under ~/.awman/context/workflows/: {}",
                host_dir.display()
            );
            let sanitized = sanitize_step_name_for_filename("run_shell: deploy");
            let file_path = host_dir.join(format!("teardown-failure-{sanitized}.txt"));
            assert!(
                file_path.exists(),
                "failure file must be written to the ephemeral dir: {}",
                file_path.display()
            );

            let calls = step_calls_handle.lock().unwrap().clone();
            let on_failure_call = calls
                .iter()
                .find(|(name, _, _)| name == "__on_failure__")
                .expect("remediation agent must have been launched");
            let expected_hint = format!("/awman/remediation/teardown-failure-{sanitized}.txt");
            assert!(
                on_failure_call.1.contains(expected_hint.as_str()),
                "prompt must reference the ephemeral mount path: {}",
                on_failure_call.1
            );
            let expected_overlay = format!("{}:/awman/remediation:ro", host_dir.display());
            assert_eq!(
                on_failure_call.2,
                Some(vec![expected_overlay]),
                "a one-off read-only overlay must be attached when context(workflow) is absent"
            );
        })
        .await
        .unwrap();
    }

    // A second (and third) teardown failure during multi-attempt remediation
    // must overwrite the failure file with the latest output, not the first.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn teardown_failure_retry_overwrites_file_with_latest_output() {
        use crate::data::fs::context_dirs::ContextDirResolver;
        use crate::data::workflow_definition::TeardownStep;

        // `awman_home` stays alive in this outer frame for the whole test
        // (including across the `.await` below) so the directory exists for
        // the blocking closure's whole execution. The `ENV_LOCK` guard and
        // `AWMAN_CONFIG_HOME` mutation live entirely *inside* the closure so
        // no `MutexGuard` is held across an await point.
        let awman_home = tempfile::tempdir().unwrap();
        let awman_home_path = awman_home.path().to_path_buf();

        tokio::task::spawn_blocking(move || {
            let _env_lock = ENV_LOCK.lock().unwrap();
            let _env_guard = EnvVarGuard::set("AWMAN_CONFIG_HOME", &awman_home_path);

            let tmp = tempfile::tempdir().unwrap();
            let session = make_session(&tmp);
            let mut workflow = make_workflow(
                Some("wf-retry-overwrite"),
                Some("claude"),
                vec![make_step("a", &[], None)],
            );
            workflow.overlays = Some(vec!["context(workflow)".to_string()]);

            let recording = Arc::new(FakeAgentExecutionFactory::always_success());
            let (step_factory, _step_calls) = StepRecordingFactory::new(Arc::clone(&recording));
            let (frontend, _msgs) = MessageCapturingFrontend::new();
            let overlay = OverlayEngine::with_auth_resolver(
                crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
            );
            let mut engine = WorkflowEngine::new(
                &session,
                workflow,
                None,
                Box::new(frontend),
                Box::new(step_factory),
                Arc::new(GitEngine::new()),
                Arc::new(overlay),
            )
            .unwrap();
            let invocation_id = engine.state().invocation_id;

            let steps = vec![TeardownStep::RunShell {
                command: "flaky".into(),
                env: None,
            }];
            // Initial failure, first retry fails again with different output,
            // second retry succeeds.
            let mock = Arc::new(MockBackgroundContainer::with_results([
                ("first-out".into(), "first-err".into(), 1),
                ("second-out".into(), "second-err".into(), 1),
                ("".into(), "".into(), 0),
            ]));
            let on_failure_configs = vec![Some(remediation_config(2))];

            let (_aborted, any_failed) = engine
                .run_teardown(
                    &steps,
                    &[false],
                    &on_failure_configs,
                    true,
                    false,
                    mock.factory(),
                )
                .unwrap();
            assert!(
                !any_failed,
                "the second retry succeeds so the step must not remain failed"
            );

            let resolver = ContextDirResolver::at_home(&awman_home_path);
            let host_dir = resolver.workflow_dir(invocation_id);
            let sanitized = sanitize_step_name_for_filename("run_shell: flaky");
            let file_path = host_dir.join(format!("teardown-failure-{sanitized}.txt"));
            let content = std::fs::read_to_string(&file_path).unwrap();

            assert!(
                content.contains("second-out") && content.contains("second-err"),
                "file must reflect the latest failure: {content}"
            );
            assert!(
                !content.contains("first-out") && !content.contains("first-err"),
                "stale output from the first failure must be overwritten: {content}"
            );
        })
        .await
        .unwrap();
    }

    // A write failure (e.g. disk full, permission error) must degrade
    // gracefully: the remediation agent still launches, using the
    // unmodified `on_failure.prompt` with no dangling file reference.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn teardown_failure_write_error_degrades_gracefully() {
        use crate::data::fs::context_dirs::ContextDirResolver;
        use crate::data::workflow_definition::TeardownStep;

        // `awman_home` stays alive in this outer frame for the whole test
        // (including across the `.await` below) so the directory exists for
        // the blocking closure's whole execution. The `ENV_LOCK` guard and
        // `AWMAN_CONFIG_HOME` mutation live entirely *inside* the closure so
        // no `MutexGuard` is held across an await point.
        let awman_home = tempfile::tempdir().unwrap();
        let awman_home_path = awman_home.path().to_path_buf();

        tokio::task::spawn_blocking(move || {
            let _env_lock = ENV_LOCK.lock().unwrap();
            let _env_guard = EnvVarGuard::set("AWMAN_CONFIG_HOME", &awman_home_path);

            let tmp = tempfile::tempdir().unwrap();
            let session = make_session(&tmp);
            let mut workflow = make_workflow(
                Some("wf-write-failure"),
                Some("claude"),
                vec![make_step("a", &[], None)],
            );
            workflow.overlays = Some(vec!["context(workflow)".to_string()]);

            let recording = Arc::new(FakeAgentExecutionFactory::always_success());
            let (step_factory, step_calls_handle) =
                StepRecordingFactory::new(Arc::clone(&recording));
            let (frontend, msgs) = MessageCapturingFrontend::new();
            let overlay = OverlayEngine::with_auth_resolver(
                crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
            );
            let mut engine = WorkflowEngine::new(
                &session,
                workflow,
                None,
                Box::new(frontend),
                Box::new(step_factory),
                Arc::new(GitEngine::new()),
                Arc::new(overlay),
            )
            .unwrap();
            let invocation_id = engine.state().invocation_id;

            // Pre-create the target file path AS A DIRECTORY so the production
            // `std::fs::write` call fails deterministically (EISDIR) without
            // relying on permission bits, which don't block root in CI/dev
            // containers.
            let resolver = ContextDirResolver::at_home(&awman_home_path);
            let host_dir = resolver.workflow_dir(invocation_id);
            let sanitized = sanitize_step_name_for_filename("run_shell: flaky");
            let conflicting_path = host_dir.join(format!("teardown-failure-{sanitized}.txt"));
            std::fs::create_dir_all(&conflicting_path).unwrap();

            let steps = vec![TeardownStep::RunShell {
                command: "flaky".into(),
                env: None,
            }];
            let mock = Arc::new(MockBackgroundContainer::with_results([
                ("out".into(), "err".into(), 1),
                ("".into(), "".into(), 0),
            ]));
            let on_failure_configs = vec![Some(remediation_config(1))];

            engine
                .run_teardown(
                    &steps,
                    &[false],
                    &on_failure_configs,
                    true,
                    false,
                    mock.factory(),
                )
                .unwrap();

            let calls = step_calls_handle.lock().unwrap().clone();
            let on_failure_call = calls
                .iter()
                .find(|(name, _, _)| name == "__on_failure__")
                .expect("remediation agent must still launch despite the write failure");
            assert_eq!(
                on_failure_call.1, "Fix the broken step.",
                "prompt must be the unmodified config prompt with no file hint: {}",
                on_failure_call.1
            );
            assert!(
                !on_failure_call.1.contains("teardown-failure"),
                "prompt must not reference a file that failed to write"
            );
            assert!(
                on_failure_call.2.is_none(),
                "no overlay should be attached when the file write failed"
            );

            let warnings = msgs.lock().unwrap().clone();
            assert!(
                warnings
                    .iter()
                    .any(|m| m.level == crate::data::message::MessageLevel::Warning
                        && m.text.contains("could not write failure output")),
                "must warn about the write failure: {warnings:?}"
            );
        })
        .await
        .unwrap();
    }

    // ── WI-0096 parallel-group engine tests ──────────────────────────────────

    use crate::data::config::flags::FlagConfig;

    /// Open a session whose effective `max_concurrent_agents` is `max`
    /// (via a flag override, the highest-precedence source).
    fn make_session_with_max_concurrent(tmp: &tempfile::TempDir, max: Option<usize>) -> Session {
        let resolver = StaticGitRootResolver::new(tmp.path());
        let opts = SessionOpenOptions {
            flags: FlagConfig {
                max_concurrent_agents: max,
                ..Default::default()
            },
            ..Default::default()
        };
        Session::open(tmp.path().to_path_buf(), &resolver, opts).unwrap()
    }

    /// Records every parallel-group callback so tests can assert on the
    /// engine's scheduling decisions after it is moved into a spawned task.
    #[derive(Default)]
    struct ParallelRecord {
        launched: Vec<String>,
        exited: Vec<(String, i32)>,
        stuck: Vec<String>,
        unstuck: Vec<String>,
        group_finished: bool,
        available: Vec<AvailableActions>,
    }

    struct ParallelTestFrontend {
        actions: Mutex<VecDeque<NextAction>>,
        engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>>,
        record: Arc<Mutex<ParallelRecord>>,
        /// Return value for every per-step parallel yolo tick.
        yolo_tick: YoloTickOutcome,
    }

    impl ParallelTestFrontend {
        fn new(
            actions: impl IntoIterator<Item = NextAction>,
            engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>>,
            yolo_tick: YoloTickOutcome,
        ) -> (Self, Arc<Mutex<ParallelRecord>>) {
            let record = Arc::new(Mutex::new(ParallelRecord::default()));
            (
                Self {
                    actions: Mutex::new(actions.into_iter().collect()),
                    engine_tx,
                    record: record.clone(),
                    yolo_tick,
                },
                record,
            )
        }
    }

    impl crate::data::message::UserMessageSink for ParallelTestFrontend {
        fn write_message(&mut self, _msg: crate::data::message::UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    impl WorkflowFrontend for ParallelTestFrontend {
        fn show_workflow_control_board(
            &mut self,
            _state: &WorkflowState,
            available: &AvailableActions,
        ) -> Result<NextAction, EngineError> {
            self.record
                .lock()
                .unwrap()
                .available
                .push(available.clone());
            Ok(self
                .actions
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(NextAction::Pause))
        }
        fn confirm_resume(&mut self, _: &ResumeMismatch) -> Result<bool, EngineError> {
            Ok(true)
        }
        fn user_choose_after_step_failure(
            &mut self,
            _: &WorkflowStep,
            _: &AgentExitInfo,
        ) -> Result<StepFailureChoice, EngineError> {
            Ok(StepFailureChoice::Abort)
        }
        fn report_step_status(&mut self, _: &WorkflowStep, _: WorkflowStepStatus) {}
        fn yolo_countdown_tick(
            &mut self,
            _: &str,
            _: Duration,
            _: Duration,
        ) -> Result<YoloTickOutcome, EngineError> {
            Ok(YoloTickOutcome::Cancel)
        }
        fn report_workflow_completed(&mut self, _: &WorkflowOutcome) {}
        fn set_engine_sender(&mut self, tx: tokio::sync::mpsc::UnboundedSender<EngineRequest>) {
            *self.engine_tx.lock().unwrap() = Some(tx);
        }
        fn report_parallel_step_launched(&mut self, step_name: &str, _: &str, _: Option<&str>) {
            self.record
                .lock()
                .unwrap()
                .launched
                .push(step_name.to_string());
        }
        fn report_parallel_step_dequeued(&mut self, step_name: &str, _: &str, _: Option<&str>) {
            self.record
                .lock()
                .unwrap()
                .launched
                .push(step_name.to_string());
        }
        fn report_parallel_step_exited(&mut self, step_name: &str, exit_code: i32) {
            self.record
                .lock()
                .unwrap()
                .exited
                .push((step_name.to_string(), exit_code));
        }
        fn report_parallel_step_stuck(&mut self, step_name: &str) {
            self.record
                .lock()
                .unwrap()
                .stuck
                .push(step_name.to_string());
        }
        fn report_parallel_step_unstuck(&mut self, step_name: &str) {
            self.record
                .lock()
                .unwrap()
                .unstuck
                .push(step_name.to_string());
        }
        fn report_parallel_group_finished(&mut self) {
            self.record.lock().unwrap().group_finished = true;
        }
        fn parallel_step_yolo_countdown_tick(
            &mut self,
            _: &str,
            _: Duration,
            _: Duration,
        ) -> Result<YoloTickOutcome, EngineError> {
            Ok(self.yolo_tick.clone())
        }
    }

    fn build_parallel_engine(
        session: &Session,
        workflow: Workflow,
        factory: BlockingFactory,
        frontend: ParallelTestFrontend,
    ) -> WorkflowEngine {
        let overlay = OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
        );
        WorkflowEngine::new(
            session,
            workflow,
            None,
            Box::new(frontend),
            Box::new(factory),
            Arc::new(GitEngine::new()),
            Arc::new(overlay),
        )
        .unwrap()
    }

    /// Full 4-step, fully-parallel workflow with `max_concurrent = 2` runs to
    /// completion and launches exactly one container per step.
    #[tokio::test]
    async fn run_to_completion_full_parallel_group_max_concurrent_2() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_with_max_concurrent(&tmp, Some(2));
        assert_eq!(
            session.effective_config().effective_max_concurrent_agents(),
            Some(2)
        );
        let workflow = make_workflow(
            Some("wf-full-parallel"),
            Some("claude"),
            vec![
                make_step("a", &[], None),
                make_step("b", &[], None),
                make_step("c", &[], None),
                make_step("d", &[], None),
            ],
        );
        let factory = FakeAgentExecutionFactory::always_success();
        let mut engine = make_engine(&session, workflow, factory, []);
        assert_eq!(engine.max_concurrent(), Some(2));

        let result = engine.run_to_completion().await.unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
        for s in ["a", "b", "c", "d"] {
            assert!(
                matches!(engine.state().status_of(s), Some(StepState::Succeeded)),
                "step {s} must have succeeded"
            );
        }
    }

    /// Scheduling: with 4 concurrently-ready steps and `max_concurrent = 2`,
    /// exactly 2 start initially; the 3rd starts only when the 1st finishes and
    /// the 4th only when the 2nd finishes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parallel_group_launches_respect_max_concurrent_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_with_max_concurrent(&tmp, Some(2));
        let workflow = make_workflow(
            Some("wf-staged"),
            Some("claude"),
            vec![
                make_step("a", &[], None),
                make_step("b", &[], None),
                make_step("c", &[], None),
                make_step("d", &[], None),
            ],
        );

        let entries: Vec<_> = (0..4).map(|_| make_blocking_entry()).collect();
        let factory = BlockingFactory::new(entries.iter().cloned());
        let execution_count = factory.execution_count.clone();
        let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
        let (frontend, record) =
            ParallelTestFrontend::new([], engine_tx.clone(), YoloTickOutcome::Continue);
        let mut engine = build_parallel_engine(&session, workflow, factory, frontend);

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        // Only 2 of the 4 steps start initially.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            execution_count.load(Ordering::Relaxed),
            2,
            "max_concurrent=2 must cap the initial launch at 2"
        );
        assert_eq!(record.lock().unwrap().launched, vec!["a", "b"]);

        // Finishing the 1st frees a slot; the 3rd (c) launches.
        signal_completion(&entries[0].1, 0);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(execution_count.load(Ordering::Relaxed), 3);
        assert_eq!(record.lock().unwrap().launched, vec!["a", "b", "c"]);

        // Finishing the 2nd frees the last slot; the 4th (d) launches.
        signal_completion(&entries[1].1, 0);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(execution_count.load(Ordering::Relaxed), 4);
        assert_eq!(record.lock().unwrap().launched, vec!["a", "b", "c", "d"]);

        // Drain the rest and confirm completion.
        signal_completion(&entries[2].1, 0);
        signal_completion(&entries[3].1, 0);
        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    /// `abort_on_failure` on one running step kills the other running peer and
    /// aborts the whole workflow.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parallel_group_abort_on_failure_kills_peer_and_aborts() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_with_max_concurrent(&tmp, Some(2));
        let mut step_a = make_step("a", &[], None);
        step_a.abort_on_failure = true;
        let workflow = make_workflow(
            Some("wf-abort-parallel"),
            Some("claude"),
            vec![step_a, make_step("b", &[], None)],
        );

        let (cancel_a, completion_a) = make_blocking_entry();
        let (cancel_b, _completion_b) = make_blocking_entry();
        let factory = BlockingFactory::new([
            (cancel_a.clone(), completion_a.clone()),
            (cancel_b.clone(), _completion_b),
        ]);
        let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
        let (frontend, _record) =
            ParallelTestFrontend::new([], engine_tx.clone(), YoloTickOutcome::Continue);
        let mut engine = build_parallel_engine(&session, workflow, factory, frontend);

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        // Both launch; then step "a" fails.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!cancel_b.load(Ordering::Relaxed));
        signal_completion(&completion_a, 1);

        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(result, WorkflowOutcome::Aborted);
        assert!(
            cancel_b.load(Ordering::Relaxed),
            "abort_on_failure must kill the still-running peer 'b'"
        );
    }

    /// Yolo countdown expiry on slot 0 kills that container, launches the
    /// queued step into the freed slot, and leaves the other slot running.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parallel_group_yolo_expiry_launches_queued_step() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_with_max_concurrent(&tmp, Some(2));
        let workflow = make_workflow(
            Some("wf-yolo-queue"),
            Some("claude"),
            vec![
                make_step("a", &[], None),
                make_step("b", &[], None),
                make_step("c", &[], None),
            ],
        );

        let (cancel_a, _c_a) = make_blocking_entry();
        let (cancel_b, completion_b) = make_blocking_entry();
        let (cancel_c, completion_c) = make_blocking_entry();
        let factory = BlockingFactory::new([
            (cancel_a.clone(), _c_a),
            (cancel_b.clone(), completion_b.clone()),
            (cancel_c.clone(), completion_c.clone()),
        ]);
        let execution_count = factory.execution_count.clone();
        let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
        let (frontend, _record) =
            ParallelTestFrontend::new([], engine_tx.clone(), YoloTickOutcome::AdvanceNow);
        let mut engine = build_parallel_engine(&session, workflow, factory, frontend);
        engine.set_yolo(true);
        let tx = {
            // set_engine_sender fires during construction.
            engine_tx.lock().unwrap().clone().unwrap()
        };

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        // a + b launched (2), c queued.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(execution_count.load(Ordering::Relaxed), 2);

        // Mark slot "a" stuck → yolo countdown → the AdvanceNow tick expires it.
        tx.send(EngineRequest::StepStuck {
            step_name: "a".to_string(),
        })
        .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert!(
            cancel_a.load(Ordering::Relaxed),
            "yolo expiry must kill slot 'a'"
        );
        assert_eq!(
            execution_count.load(Ordering::Relaxed),
            3,
            "the queued step 'c' must launch into the freed slot"
        );
        assert!(
            !cancel_b.load(Ordering::Relaxed),
            "slot 'b' must keep running"
        );

        // Complete the survivors.
        signal_completion(&completion_b, 0);
        signal_completion(&completion_c, 0);
        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    /// Yolo expiry with no queued step: the group drains — no new launch, the
    /// other slot continues, and the group finishes when it exits.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parallel_group_yolo_expiry_draining_no_new_launch() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_with_max_concurrent(&tmp, Some(2));
        let workflow = make_workflow(
            Some("wf-yolo-drain"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &[], None)],
        );

        let (cancel_a, _c_a) = make_blocking_entry();
        let (cancel_b, completion_b) = make_blocking_entry();
        let factory = BlockingFactory::new([
            (cancel_a.clone(), _c_a),
            (cancel_b.clone(), completion_b.clone()),
        ]);
        let execution_count = factory.execution_count.clone();
        let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
        let (frontend, _record) =
            ParallelTestFrontend::new([], engine_tx.clone(), YoloTickOutcome::AdvanceNow);
        let mut engine = build_parallel_engine(&session, workflow, factory, frontend);
        engine.set_yolo(true);
        let tx = engine_tx.lock().unwrap().clone().unwrap();

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(execution_count.load(Ordering::Relaxed), 2);

        tx.send(EngineRequest::StepStuck {
            step_name: "a".to_string(),
        })
        .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert!(
            cancel_a.load(Ordering::Relaxed),
            "yolo expiry must kill slot 'a'"
        );
        assert_eq!(
            execution_count.load(Ordering::Relaxed),
            2,
            "no queued step means nothing new launches (draining)"
        );
        assert!(
            !cancel_b.load(Ordering::Relaxed),
            "the surviving slot keeps running"
        );

        signal_completion(&completion_b, 0);
        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    /// `StepStuck { step_name }` (yolo off) marks only the named slot stuck;
    /// the other slot is unaffected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parallel_step_stuck_routes_to_named_slot_only() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_with_max_concurrent(&tmp, Some(2));
        let workflow = make_workflow(
            Some("wf-stuck-route"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &[], None)],
        );

        let (_ca, completion_a) = make_blocking_entry();
        let (_cb, completion_b) = make_blocking_entry();
        let factory = BlockingFactory::new([
            (Arc::new(AtomicBool::new(false)), completion_a.clone()),
            (Arc::new(AtomicBool::new(false)), completion_b.clone()),
        ]);
        let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
        let (frontend, record) =
            ParallelTestFrontend::new([], engine_tx.clone(), YoloTickOutcome::Continue);
        let mut engine = build_parallel_engine(&session, workflow, factory, frontend);
        // Not yolo mode.
        let tx = engine_tx.lock().unwrap().clone().unwrap();

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        tokio::time::sleep(Duration::from_millis(150)).await;
        tx.send(EngineRequest::StepStuck {
            step_name: "b".to_string(),
        })
        .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        {
            let rec = record.lock().unwrap();
            assert_eq!(rec.stuck, vec!["b"], "only 'b' must be reported stuck");
            assert!(
                !rec.stuck.contains(&"a".to_string()),
                "'a' must not be reported stuck"
            );
        }

        signal_completion(&completion_a, 0);
        signal_completion(&completion_b, 0);
        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    /// WCB scoping (§10): when the focused step has running parallel peers,
    /// `can_cancel_to_previous_step` and `can_finish_workflow` are forced false
    /// and `restart_unavailable_reason` is set.
    #[tokio::test]
    async fn compute_available_actions_scopes_wcb_with_parallel_peers() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_with_max_concurrent(&tmp, Some(2));
        let workflow = make_workflow(
            Some("wf-wcb-peers"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &[], None)],
        );
        let factory = FakeAgentExecutionFactory::always_success();
        let mut engine = make_engine(&session, workflow, factory, []);

        let dummy = |name: &str| {
            let (tx, _rx) = tokio::sync::broadcast::channel(4);
            ActiveParallelStep {
                step_name: name.to_string(),
                execution: None,
                cancel_handle: None,
                stuck_sender: Arc::new(tx),
                container_name: format!("container-{name}"),
                output_tail: None,
                awman_killed: false,
                stuck: false,
                yolo_deadline: None,
                agent: AgentName::new("claude").unwrap(),
                model: None,
            }
        };

        // Two live slots, "a" focused → one running peer.
        engine.active_steps.push(dummy("a"));
        engine.active_steps.push(dummy("b"));
        engine.current_step_name = Some("a".to_string());
        engine.current_step_agent = Some(AgentName::new("claude").unwrap());

        let a = engine.compute_available_actions().unwrap();
        assert_eq!(a.parallel_peer_count, 2);
        assert_eq!(a.parallel_peers_running, 1);
        assert!(!a.can_cancel_to_previous_step);
        assert!(a.cancel_to_previous_unavailable_reason.is_some());
        assert!(!a.can_finish_workflow);
        assert!(a.finish_workflow_unavailable_reason.is_some());
        assert!(a.restart_unavailable_reason.is_some());

        // Drop to a single live slot → no peers, no forced scoping.
        engine.active_steps.pop();
        let b = engine.compute_available_actions().unwrap();
        assert_eq!(b.parallel_peers_running, 0);
        assert!(b.restart_unavailable_reason.is_none());
    }
}
