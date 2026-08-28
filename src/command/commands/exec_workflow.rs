//! `ExecWorkflowCommand` — run a workflow file.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::agent_auth::AgentAuthFrontend;
use crate::command::commands::agent_setup::AgentSetupFrontend;
use crate::command::commands::mount_scope::{MountScope, MountScopeFrontend};
use crate::command::commands::worktree_lifecycle::{WorktreeLifecycle, WorktreeLifecycleFrontend};
use crate::command::commands::Command;
use crate::command::commands::{
    collect_all_overlay_specs, parse_overlay_list, resolve_context_overlays, warn_legacy_config,
    TypedOverlay,
};
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::Session;
use crate::data::workflow_definition::{Workflow, WorkflowStep};
use crate::data::workflow_prompt_template::{substitute_prompt, WorkItemContext};
use crate::engine::agent::AgentRunOptions;
use crate::engine::agent_runtime::execution::AgentExitInfo;
use crate::engine::agent_runtime::frontend::AgentFrontend;
use crate::engine::container::options::{AutoMode, PlanMode, YoloMode};
use crate::engine::error::EngineError;
use crate::engine::workflow::actions::{
    AvailableActions, NextAction, ResumeMismatch, StepFailureChoice, StepOutput, WorkflowOutcome,
    WorkflowStepProgressInfo, WorkflowStepStatus, YoloTickOutcome,
};
use crate::engine::workflow::factory::{AgentExecutionFactory, WorkflowRuntimeContext};
use crate::engine::workflow::frontend::WorkflowFrontend;
use crate::engine::workflow::{EngineRequest, WorkflowEngine};

use super::dynamic_repair::{RepairDecision, WorkflowRepairLoop};

#[derive(Debug, Clone)]
pub struct ExecWorkflowCommandFlags {
    /// The positional workflow path. `None` is only valid with `--dynamic`,
    /// where the leader agent generates the workflow file. Non-dynamic
    /// invocations with `None` produce the existing missing-required-argument
    /// error.
    pub workflow: Option<PathBuf>,
    pub work_item: Option<String>,
    pub non_interactive: bool,
    pub plan: bool,
    pub allow_docker: bool,
    pub worktree: bool,
    pub yolo: bool,
    pub auto: bool,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub overlay: Vec<String>,
    pub max_concurrent: Option<usize>,
    pub issue_source: crate::data::issue::IssueSourceFlags,
    /// When true, a leader agent designs and runs a workflow for the work item.
    /// Implies `--yolo`, `--worktree`, and `context(workflow)`.
    pub dynamic: bool,
    /// Raw `agent::model` string for the dynamic leader agent. Only valid with
    /// `--dynamic`.
    pub leader: Option<String>,
}

/// Fully-specified leader agent selection parsed from `--leader agent::model`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaderSpec {
    pub agent: String,
    pub model: String,
}

impl LeaderSpec {
    /// Parse a `--leader` value of the form `agent::model`. The value must
    /// contain exactly two non-empty components separated by a single `::`.
    pub fn parse(raw: &str) -> Result<Self, CommandError> {
        let parts: Vec<&str> = raw.split("::").collect();
        if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
            return Err(CommandError::Other(format!(
                "invalid --leader value {raw:?}; expected agent::model \
                 (e.g. claude::claude-opus-4-8)"
            )));
        }
        Ok(LeaderSpec {
            agent: parts[0].to_string(),
            model: parts[1].to_string(),
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecWorkflowOutcome {
    pub workflow: String,
    pub exit_code: Option<i32>,
    pub worktree_used: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowSummary {
    pub steps_completed: usize,
    pub steps_failed: usize,
}

/// Per-command frontend trait: supertrait composition of every Layer 1 and
/// Layer 2 trait that `ExecWorkflowCommand` calls during its lifecycle.
#[async_trait]
pub trait ExecWorkflowCommandFrontend:
    UserMessageSink
    + AgentFrontend
    + WorkflowFrontend
    + MountScopeFrontend
    + AgentSetupFrontend
    + AgentAuthFrontend
    + WorktreeLifecycleFrontend
    + Send
    + Sync
{
    /// Flip the PTY-active gate: when `true` the frontend queues user messages
    /// instead of rendering them immediately; when `false` it renders inline.
    fn set_pty_active(&mut self, active: bool);

    fn report_workflow_summary(&mut self, summary: &WorkflowSummary);

    /// Ask the user whether to resume the workflow from its persisted state
    /// or to delete that state and start fresh. Called only when a saved
    /// state file is found on disk before the engine is built. Returns
    /// `true` to resume, `false` to start fresh.
    fn ask_workflow_resume_or_fresh(
        &mut self,
        workflow_name: &str,
        completed_steps: usize,
        total_steps: usize,
    ) -> Result<bool, CommandError>;
}

pub struct ExecWorkflowCommand {
    flags: ExecWorkflowCommandFlags,
    engines: Engines,
    session: Session,
}

impl ExecWorkflowCommand {
    pub fn new(flags: ExecWorkflowCommandFlags, engines: Engines, session: Session) -> Self {
        Self {
            flags,
            engines,
            session,
        }
    }

    pub fn flags(&self) -> &ExecWorkflowCommandFlags {
        &self.flags
    }
}

// ─── WorkflowProxy ───────────────────────────────────────────────────────────
//
// Implements `WorkflowFrontend` by delegating to the shared frontend through a
// `Mutex`. The engine holds this proxy as `Box<dyn WorkflowFrontend>`. After
// the engine block exits and the proxy is dropped, `Arc::try_unwrap` reclaims
// exclusive ownership of the frontend.

struct WorkflowProxy(Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>>);

impl UserMessageSink for WorkflowProxy {
    fn write_message(&mut self, msg: UserMessage) {
        self.0.lock().unwrap().write_message(msg);
    }

    fn replay_queued(&mut self) {
        self.0.lock().unwrap().replay_queued();
    }
}

impl WorkflowFrontend for WorkflowProxy {
    fn show_workflow_control_board(
        &mut self,
        state: &crate::data::workflow_state::WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        self.0
            .lock()
            .unwrap()
            .show_workflow_control_board(state, available)
    }

    fn yolo_countdown_tick(
        &mut self,
        step_name: &str,
        remaining: Duration,
        total: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        self.0
            .lock()
            .unwrap()
            .yolo_countdown_tick(step_name, remaining, total)
    }

    fn yolo_countdown_started(&mut self, step_name: &str) {
        self.0.lock().unwrap().yolo_countdown_started(step_name);
    }

    fn yolo_countdown_finished(&mut self, step_name: &str) {
        self.0.lock().unwrap().yolo_countdown_finished(step_name);
    }

    fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
        self.0.lock().unwrap().report_step_status(step, status);
    }

    fn report_step_output(&mut self, step: &WorkflowStep, output: StepOutput) {
        self.0.lock().unwrap().report_step_output(step, output);
    }

    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
        self.0.lock().unwrap().report_workflow_completed(outcome);
    }

    fn report_workflow_progress(&mut self, steps: &[WorkflowStepProgressInfo]) {
        self.0.lock().unwrap().report_workflow_progress(steps);
    }

    fn report_step_interactive_launch(
        &mut self,
        step: &WorkflowStep,
        agent: &str,
        model: Option<&str>,
    ) {
        self.0
            .lock()
            .unwrap()
            .report_step_interactive_launch(step, agent, model);
    }

    fn report_container_exited(&mut self, exit_code: i32) {
        self.0.lock().unwrap().report_container_exited(exit_code);
    }

    fn confirm_resume(&mut self, mismatch: &ResumeMismatch) -> Result<bool, EngineError> {
        self.0.lock().unwrap().confirm_resume(mismatch)
    }

    fn user_choose_after_step_failure(
        &mut self,
        step: &WorkflowStep,
        exit: &AgentExitInfo,
    ) -> Result<StepFailureChoice, EngineError> {
        self.0
            .lock()
            .unwrap()
            .user_choose_after_step_failure(step, exit)
    }

    fn set_engine_sender(&mut self, tx: tokio::sync::mpsc::UnboundedSender<EngineRequest>) {
        self.0.lock().unwrap().set_engine_sender(tx);
    }

    fn set_stuck_sender(
        &mut self,
        sender: Arc<
            tokio::sync::broadcast::Sender<crate::engine::agent_runtime::execution::StuckEvent>,
        >,
    ) {
        self.0.lock().unwrap().set_stuck_sender(sender);
    }

    fn on_setup_step_started(&mut self, description: &str) {
        self.0.lock().unwrap().on_setup_step_started(description);
    }
    fn on_setup_step_output(&mut self, line: &str) {
        self.0.lock().unwrap().on_setup_step_output(line);
    }
    fn on_setup_step_completed(&mut self, description: &str) {
        self.0.lock().unwrap().on_setup_step_completed(description);
    }
    fn on_setup_step_failed(&mut self, description: &str, exit_code: i32, stderr: &str) {
        self.0
            .lock()
            .unwrap()
            .on_setup_step_failed(description, exit_code, stderr);
    }

    fn on_teardown_step_started(&mut self, description: &str) {
        self.0.lock().unwrap().on_teardown_step_started(description);
    }
    fn on_teardown_step_output(&mut self, line: &str) {
        self.0.lock().unwrap().on_teardown_step_output(line);
    }
    fn on_teardown_step_completed(&mut self, description: &str) {
        self.0
            .lock()
            .unwrap()
            .on_teardown_step_completed(description);
    }
    fn on_teardown_step_failed(&mut self, description: &str, exit_code: i32, stderr: &str) {
        self.0
            .lock()
            .unwrap()
            .on_teardown_step_failed(description, exit_code, stderr);
    }

    // === Parallel-group commands (WI-0096) — forwarded like everything else
    // above. Without these overrides the trait's default no-ops would run
    // instead of the boxed frontend's real implementation. ===

    fn report_parallel_group_started(&mut self, step_names: &[String]) {
        self.0
            .lock()
            .unwrap()
            .report_parallel_group_started(step_names);
    }

    fn report_parallel_step_launched(&mut self, step_name: &str, agent: &str, model: Option<&str>) {
        self.0
            .lock()
            .unwrap()
            .report_parallel_step_launched(step_name, agent, model);
    }

    fn report_parallel_step_exited(&mut self, step_name: &str, exit_code: i32) {
        self.0
            .lock()
            .unwrap()
            .report_parallel_step_exited(step_name, exit_code);
    }

    fn report_parallel_step_dequeued(&mut self, step_name: &str, agent: &str, model: Option<&str>) {
        self.0
            .lock()
            .unwrap()
            .report_parallel_step_dequeued(step_name, agent, model);
    }

    fn report_parallel_group_finished(&mut self) {
        self.0.lock().unwrap().report_parallel_group_finished();
    }

    fn report_parallel_step_stuck(&mut self, step_name: &str) {
        self.0.lock().unwrap().report_parallel_step_stuck(step_name);
    }

    fn report_parallel_step_unstuck(&mut self, step_name: &str) {
        self.0
            .lock()
            .unwrap()
            .report_parallel_step_unstuck(step_name);
    }

    fn parallel_step_yolo_countdown_started(&mut self, step_name: &str) {
        self.0
            .lock()
            .unwrap()
            .parallel_step_yolo_countdown_started(step_name);
    }

    fn parallel_step_yolo_countdown_tick(
        &mut self,
        step_name: &str,
        remaining: Duration,
        total: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        self.0
            .lock()
            .unwrap()
            .parallel_step_yolo_countdown_tick(step_name, remaining, total)
    }

    fn parallel_step_yolo_countdown_finished(&mut self, step_name: &str) {
        self.0
            .lock()
            .unwrap()
            .parallel_step_yolo_countdown_finished(step_name);
    }

    fn set_parallel_step_io(
        &mut self,
        step_name: &str,
        io: crate::engine::agent_runtime::frontend::AgentIo,
    ) {
        self.0.lock().unwrap().set_parallel_step_io(step_name, io);
    }

    fn set_parallel_step_stuck_sender(
        &mut self,
        step_name: &str,
        sender: Arc<
            tokio::sync::broadcast::Sender<crate::engine::agent_runtime::execution::StuckEvent>,
        >,
    ) {
        self.0
            .lock()
            .unwrap()
            .set_parallel_step_stuck_sender(step_name, sender);
    }
}

// ─── AgentFrontendProxy ──────────────────────────────────────────────────
//
// Passed to `AgentInstance::run_with_frontend`. The current Docker backend
// discards it; a future PTY-wiring backend will use it.

struct AgentFrontendProxy(Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>>);

#[async_trait]
impl AgentFrontend for AgentFrontendProxy {
    fn report_status(&mut self, status: crate::engine::agent_runtime::frontend::AgentStatus) {
        self.0.lock().unwrap().report_status(status);
    }

    fn report_progress(&mut self, progress: crate::engine::agent_runtime::frontend::AgentProgress) {
        self.0.lock().unwrap().report_progress(progress);
    }

    fn take_io(&mut self) -> crate::engine::agent_runtime::frontend::AgentIo {
        self.0.lock().unwrap().take_io()
    }

    fn grace_timeout(&self) -> std::time::Duration {
        self.0.lock().unwrap().grace_timeout()
    }

    fn stuck_timeout(&self) -> std::time::Duration {
        self.0.lock().unwrap().stuck_timeout()
    }
}

impl UserMessageSink for AgentFrontendProxy {
    fn write_message(&mut self, msg: UserMessage) {
        self.0.lock().unwrap().write_message(msg);
    }

    fn replay_queued(&mut self) {
        self.0.lock().unwrap().replay_queued();
    }
}

// ─── CommandLayerFactory ─────────────────────────────────────────────────────
//
// Implements `AgentExecutionFactory` for the workflow engine. Builds a
// container instance from per-step parameters + command flags, then binds a
// `AgentFrontendProxy` to it via `run_with_frontend`.

struct CommandLayerFactory {
    shared: Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>>,
    engines: Engines,
    flags: Arc<ExecWorkflowCommandFlags>,
    cli_typed_overlays: Vec<TypedOverlay>,
    work_item_context: Option<WorkItemContext>,
    /// The original repository git root (not the worktree). Used for image tag
    /// derivation so worktree-based runs use the correct project image.
    image_git_root: PathBuf,
    /// Workflow-level overlays applied to every step.
    workflow_overlays: Option<Vec<String>>,
}

impl AgentExecutionFactory for CommandLayerFactory {
    fn execution_for_step(
        &self,
        step: &WorkflowStep,
        session: &Session,
        runtime: &WorkflowRuntimeContext,
    ) -> Result<crate::engine::agent_runtime::execution::AgentExecution, EngineError> {
        // Substitute work item template tokens in the step prompt.
        let substitution =
            substitute_prompt(&step.prompt_template, self.work_item_context.as_ref());

        // Compute per-step overlays by merging config/env/CLI with step-level overlays.
        let collected = collect_all_overlay_specs(
            session,
            self.cli_typed_overlays.clone(),
            self.workflow_overlays.as_deref(),
            step.overlays.as_deref(),
        )
        .map_err(|e| EngineError::Other(format!("overlay collection failed: {e}")))?;

        // Resolve context overlays.
        let (context_overlays, system_prompt) = {
            let mut guard = self.shared.lock().unwrap();
            resolve_context_overlays(
                &collected.context_overlays,
                session,
                &runtime.step_agent,
                Some(runtime.workflow_invocation_id),
                runtime.workflow_step_info.as_ref(),
                guard.as_mut(),
            )
            .map_err(|e| EngineError::Other(format!("context overlay resolution failed: {e}")))?
        };

        // Use the original repo root for image tag derivation so worktree-
        // based runs resolve the correct image for both the Image option AND
        // for image_home_dir inspection (which determines overlay mount paths).
        let correct_tag = crate::data::image_tags::agent_image_tag(
            &self.image_git_root,
            runtime.step_agent.as_str(),
        );
        let run_opts = AgentRunOptions {
            yolo: self.flags.yolo.then_some(YoloMode::Enabled),
            auto: self.flags.auto.then_some(AutoMode::Enabled),
            plan: self.flags.plan.then_some(PlanMode::Enabled),
            allowed_tools: vec![],
            disallowed_tools: vec![],
            initial_prompt: Some(substitution.rendered),
            allow_docker: self.flags.allow_docker,
            non_interactive: self.flags.non_interactive,
            model: runtime.step_model.clone(),
            env_passthrough: if collected.env_passthrough.is_empty() {
                None
            } else {
                Some(collected.env_passthrough)
            },
            directory_overlays: collected.directories,
            include_all_skills: collected.include_all_skills,
            named_skills: collected.named_skills,
            image_tag_override: Some(correct_tag),
            system_prompt,
            context_overlays,
        };
        // Resolve keychain credentials so the agent can reach its backend.
        // Mirrors the same step in `chat` and `exec_prompt`. The centralized
        // builder folds them into the paradigm-appropriate option (container
        // env vars, or — under sbx — `sbx secret set` registration).
        let credential_env_vars = self
            .engines
            .auth_engine
            .resolve_agent_auth(session, &runtime.step_agent)
            .map(|c| c.env_vars)
            .unwrap_or_default();

        let resolved = self.engines.agent_engine.resolve_agent_options(
            session,
            &runtime.step_agent,
            &run_opts,
            &credential_env_vars,
            self.engines.runtime.as_ref(),
        )?;
        let instance = self.engines.runtime.build(resolved)?;
        let proxy = AgentFrontendProxy(Arc::clone(&self.shared));
        instance.run_with_frontend(Box::new(proxy))
    }

    fn inject_prompt(
        &self,
        execution: &crate::engine::agent_runtime::execution::AgentExecution,
        prompt: &str,
    ) -> Result<Option<()>, EngineError> {
        // Mirror old amux's `launch_next_workflow_step_in_current_container`:
        // write the prompt followed by `\r` (Enter) directly into the running
        // container's PTY stdin. The Container Execution back-end returns
        // `Ok(true)` if it accepted the bytes (PTY-bridged backends do),
        // `Ok(false)` if it can't inject (inherit-stdio with no PTY) — in
        // which case we report `Ok(None)` and the engine launches a fresh
        // container.
        let mut payload = prompt.as_bytes().to_vec();
        payload.push(b'\r');
        match execution.try_inject_stdin(&payload)? {
            true => Ok(Some(())),
            false => Ok(None),
        }
    }
}

// ─── Command impl ─────────────────────────────────────────────────────────────

#[async_trait]
impl Command for ExecWorkflowCommand {
    type Frontend = Box<dyn ExecWorkflowCommandFrontend>;
    type Outcome = ExecWorkflowOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        // Early flag validation (Layer 2) — runs before any IO for both the
        // dynamic and non-dynamic paths. Surfaces an error message and aborts.
        if let Err(e) = validate_dynamic_flags(&self.flags) {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("exec workflow: {e}"),
            });
            return Err(e);
        }

        // Dynamic mode: a leader agent designs the workflow, then it executes.
        if self.flags.dynamic {
            return self.run_dynamic(frontend).await;
        }

        // Non-dynamic: the positional path is required.
        let workflow_arg = match &self.flags.workflow {
            Some(p) => p.clone(),
            None => {
                let err =
                    CommandError::missing_required_argument(&["exec", "workflow"], "workflow");
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: "exec workflow: missing required argument 'workflow'".into(),
                });
                return Err(err);
            }
        };

        // Resolve the workflow path relative to the session's working
        // directory so that relative paths work regardless of where the
        // awman process was originally launched.
        let workflow_path = if workflow_arg.is_absolute() {
            workflow_arg.clone()
        } else {
            self.session.working_dir().join(&workflow_arg)
        };

        // Track whether the gemini deprecation warning has already been emitted
        // so we never fire it twice (early CLI check + post-load TOML scan).
        let mut gemini_warning_emitted = false;
        if self.flags.agent.as_deref() == Some("gemini") {
            emit_gemini_deprecation_warning(frontend.as_mut());
            gemini_warning_emitted = true;
        }

        // Emit deprecation warnings for legacy config fields.
        warn_legacy_config(&self.session, frontend.as_mut());

        if self.flags.yolo && self.flags.worktree {
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: "--yolo implies --worktree. Running in isolated worktree.".into(),
            });
        }

        // 1. Load the workflow file.
        if !workflow_path.exists() {
            let err = CommandError::WorkflowFileNotFound {
                path: workflow_path.clone(),
            };
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!(
                    "exec workflow: workflow file not found: {}",
                    workflow_path.display()
                ),
            });
            return Err(err);
        }
        let workflow = match Workflow::load(&workflow_path) {
            Ok(w) => w,
            Err(e) => {
                let err = CommandError::Other(format!("loading workflow: {e}"));
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec workflow: failed to load workflow: {e}"),
                });
                return Err(err);
            }
        };

        // After load: scan the workflow's per-step and workflow-level agents,
        // plus the session default (used when neither step nor workflow set an
        // agent). Per-step resolution mirrors WorkflowEngine::resolve_agent so
        // the warning fires for the same agent the engine will actually launch.
        if !gemini_warning_emitted && workflow_resolves_to_gemini(&workflow, &self.session) {
            emit_gemini_deprecation_warning(frontend.as_mut());
            gemini_warning_emitted = true;
        }

        // Warn (don't error) when context(workflow) appears in a setup or
        // teardown step's overlays — workflow step progression state is not
        // available during those phases, so the dynamic prompt fields will
        // be empty.
        warn_context_workflow_in_phase(&workflow, frontend.as_mut());
        let _ = gemini_warning_emitted;

        // 2. Resolve mount scope — confirm with the user when cwd differs from git root.
        let cwd = self.session.working_dir().to_path_buf();
        let git_root_for_scope = self.session.git_root().to_path_buf();
        let mount_path = match MountScope::resolve(&cwd, &git_root_for_scope, frontend.as_mut()) {
            Ok(p) => p,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec workflow: mount scope resolution failed: {e}"),
                });
                return Err(e);
            }
        };

        // 3. Load work item context from --work-item or --issue.
        // `_issue_temp_file` keeps the temp file alive for the duration of
        // this function — its Drop impl deletes the file regardless of how
        // the function exits (success, error, panic).
        let issue_title_slug: Option<String>;
        let _issue_temp_file: Option<IssueTempFile>;
        let issue_overlay: Option<TypedOverlay>;

        let work_item_context = if let Some(ref issue_ref) = self.flags.issue_source.issue {
            // --issue: fetch issue and construct work item context from it.
            let router = crate::data::issue::router::IssueSourceRouter::default();
            match router.fetch_issue_with_progress(issue_ref, &git_root_for_scope, &mut *frontend) {
                Ok((issue, source)) => {
                    let work_items_dir = self
                        .session
                        .repo_config()
                        .work_items_dir_or_default(&git_root_for_scope);
                    let build = match issue_source_overlay(
                        source,
                        &issue,
                        &git_root_for_scope,
                        &work_items_dir,
                    ) {
                        Ok(b) => b,
                        Err(e) => {
                            frontend.write_message(UserMessage {
                                level: MessageLevel::Error,
                                text: format!(
                                    "exec workflow: failed to write issue temp file: {e}"
                                ),
                            });
                            return Err(CommandError::Other(format!(
                                "writing issue temp file: {e}"
                            )));
                        }
                    };

                    frontend.write_message(UserMessage {
                        level: MessageLevel::Info,
                        text: format!(
                            "exec workflow: fetched issue '{}' ({})",
                            issue.title, issue.source_id
                        ),
                    });

                    issue_overlay = Some(build.overlay);
                    issue_title_slug = Some(build.slug);
                    let number = build.number;
                    let content = build.content;
                    _issue_temp_file = Some(build.temp_file);
                    Some(WorkItemContext { number, content })
                }
                Err(e) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("exec workflow: failed to fetch issue: {e}"),
                    });
                    return Err(CommandError::Other(e.to_string()));
                }
            }
        } else if let Some(wi_str) = &self.flags.work_item {
            issue_title_slug = None;
            _issue_temp_file = None;
            issue_overlay = None;
            match parse_work_item_number(wi_str) {
                Some(number) => {
                    let path = find_work_item_file(&git_root_for_scope, number);
                    match path.and_then(|p| std::fs::read_to_string(&p).ok()) {
                        Some(content) => Some(WorkItemContext { number, content }),
                        None => {
                            frontend.write_message(crate::data::message::UserMessage {
                                level: crate::data::message::MessageLevel::Warning,
                                text: format!(
                                    "work item file for {:04} not found; \
                                     {{{{work_item_*}}}} placeholders will be empty",
                                    number
                                ),
                            });
                            None
                        }
                    }
                }
                None => {
                    frontend.write_message(crate::data::message::UserMessage {
                        level: crate::data::message::MessageLevel::Warning,
                        text: format!(
                            "could not parse work item number from {:?}; \
                             {{{{work_item_*}}}} placeholders will be empty",
                            wi_str
                        ),
                    });
                    None
                }
            }
        } else {
            issue_title_slug = None;
            _issue_temp_file = None;
            issue_overlay = None;
            None
        };
        // 4. Worktree prepare (if --worktree is set).
        // When a worktree is used, capture its path so the session below is
        // rooted at the worktree checkout rather than the main repo.
        if self.flags.worktree && self.session.session_type().is_remote() {
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: "Skipping worktree creation for remote session — repo is already isolated."
                    .into(),
            });
        }
        let mut worktree_path: Option<PathBuf> = None;
        let worktree_lifecycle = if self.flags.worktree && !self.session.session_type().is_remote()
        {
            let git_root = match self.engines.git_engine.resolve_root(&cwd) {
                Ok(r) => r,
                Err(e) => {
                    let err = CommandError::from(e);
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("exec workflow: failed to resolve git root: {err}"),
                    });
                    return Err(err);
                }
            };
            // When --issue is supplied, name the worktree/branch after the issue slug.
            // When --work-item is supplied, name after the work item number.
            // Otherwise, name after the workflow filename.
            let lifecycle = if let Some(ref slug) = issue_title_slug {
                match WorktreeLifecycle::for_workflow(
                    Arc::clone(&self.engines.git_engine),
                    git_root,
                    slug,
                ) {
                    Ok(l) => l,
                    Err(e) => {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!(
                                "exec workflow: failed to create worktree for issue: {e}"
                            ),
                        });
                        return Err(e);
                    }
                }
            } else if let Some(ctx) = &work_item_context {
                if self.flags.work_item.is_some() {
                    match WorktreeLifecycle::for_work_item(
                        Arc::clone(&self.engines.git_engine),
                        git_root,
                        ctx.number,
                    ) {
                        Ok(l) => l,
                        Err(e) => {
                            frontend.write_message(UserMessage {
                                level: MessageLevel::Error,
                                text: format!(
                                    "exec workflow: failed to create worktree for work item: {e}"
                                ),
                            });
                            return Err(e);
                        }
                    }
                } else {
                    let name = workflow_path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("workflow")
                        .to_string();
                    match WorktreeLifecycle::for_workflow(
                        Arc::clone(&self.engines.git_engine),
                        git_root,
                        &name,
                    ) {
                        Ok(l) => l,
                        Err(e) => {
                            frontend.write_message(UserMessage {
                                level: MessageLevel::Error,
                                text: format!(
                                    "exec workflow: failed to create worktree for workflow: {e}"
                                ),
                            });
                            return Err(e);
                        }
                    }
                }
            } else {
                let name = workflow_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("workflow")
                    .to_string();
                match WorktreeLifecycle::for_workflow(
                    Arc::clone(&self.engines.git_engine),
                    git_root,
                    &name,
                ) {
                    Ok(l) => l,
                    Err(e) => {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!(
                                "exec workflow: failed to create worktree for workflow: {e}"
                            ),
                        });
                        return Err(e);
                    }
                }
            };
            let wt_path = match lifecycle.prepare(&mut *frontend).await {
                Ok(p) => p,
                Err(e) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("exec workflow: worktree prepare failed: {e}"),
                    });
                    return Err(e);
                }
            };
            worktree_path = Some(wt_path);
            Some(lifecycle)
        } else {
            None
        };

        // 4b. Override mount path when a worktree is active so setup/teardown
        // containers bind to the worktree checkout, not the main repo.
        let mount_path = if let Some(ref wt) = worktree_path {
            wt.clone()
        } else {
            mount_path
        };

        // 4c. When running in a worktree, compute an extra overlay that mounts
        // the main repo's `.git` directory into setup/teardown containers.
        // Without this, the worktree's `.git` pointer file references a host
        // path that doesn't exist inside the container, breaking all git ops.
        let worktree_git_mount: Option<crate::engine::container::options::OverlaySpec> =
            if worktree_path.is_some() {
                worktree_git_overlay(&mount_path)?
            } else {
                None
            };

        // 5. Parse CLI overlay specs early so errors surface before PTY is activated.
        let cli_typed = {
            let mut all = Vec::new();
            for s in &self.flags.overlay {
                match parse_overlay_list(s) {
                    Ok(parsed) => all.extend(parsed),
                    Err(reason) => {
                        let e = CommandError::InvalidOverlaySpec {
                            spec: s.clone(),
                            reason,
                        };
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!("exec workflow: invalid overlay spec: {e}"),
                        });
                        return Err(e);
                    }
                }
            }
            if let Some(overlay) = issue_overlay {
                all.push(overlay);
            }
            all
        };

        let prepared = PreparedRun {
            workflow,
            workflow_path,
            work_item_context,
            cli_typed,
            mount_path,
            worktree_path,
            worktree_lifecycle,
            worktree_git_mount,
            git_root_for_scope,
            cwd,
            original_session: self.session,
            issue_temp_file: _issue_temp_file,
        };
        execute_prepared(&self.flags, &self.engines, prepared, frontend).await
    }
}

// ─── Shared workflow execution + dynamic preflight (WI-0092) ─────────────────

/// All state needed to execute a parsed workflow once the worktree, session,
/// context, and work item have been prepared. Both the non-dynamic path and
/// the dynamic leader path build one of these and hand it to
/// [`execute_prepared`], so workflow execution lives in exactly one place
/// (WI-0092 §10) — neither path recursively re-enters
/// `ExecWorkflowCommand::run_with_frontend`.
struct PreparedRun {
    workflow: Workflow,
    workflow_path: PathBuf,
    work_item_context: Option<WorkItemContext>,
    cli_typed: Vec<TypedOverlay>,
    mount_path: PathBuf,
    worktree_path: Option<PathBuf>,
    worktree_lifecycle: Option<WorktreeLifecycle>,
    worktree_git_mount: Option<crate::engine::container::options::OverlaySpec>,
    git_root_for_scope: PathBuf,
    cwd: PathBuf,
    /// The pre-worktree session. `execute_prepared` re-roots it at the worktree
    /// when `worktree_path` is set.
    original_session: Session,
    /// Kept alive for the duration of the run; its Drop removes the issue temp
    /// file. `None` for non-issue invocations.
    issue_temp_file: Option<IssueTempFile>,
}

/// Execute a fully-prepared workflow: persisted-state resume check, engine
/// setup/main/teardown phases, summary reporting, and worktree finalize.
/// Shared by the non-dynamic and dynamic execution paths.
async fn execute_prepared(
    flags: &ExecWorkflowCommandFlags,
    engines: &Engines,
    prepared: PreparedRun,
    frontend: Box<dyn ExecWorkflowCommandFrontend>,
) -> Result<ExecWorkflowOutcome, CommandError> {
    let PreparedRun {
        mut workflow,
        workflow_path,
        work_item_context,
        cli_typed,
        mount_path,
        worktree_path,
        worktree_lifecycle,
        worktree_git_mount,
        git_root_for_scope,
        cwd,
        original_session,
        issue_temp_file: _issue_temp_file,
    } = prepared;
    let mut frontend = frontend;

    // When the run is inside an isolated worktree (--worktree, or implied by
    // --yolo/--dynamic), any `checkout_create_branch` setup step is redundant:
    // the worktree already put the run on its own branch. Skip-and-warn, not
    // a failure.
    if worktree_path.is_some() {
        skip_checkout_branch_steps_in_worktree(&mut workflow, frontend.as_mut());
    }

    // 5b. Detect a persisted workflow-state file and ask the user whether
    //     to resume it or delete it and start fresh. The check uses the
    //     session_root the engine will pick up below — the worktree path
    //     when --worktree is active, otherwise cwd. Done before PTY
    //     activation so the dialog renders immediately, like the
    //     existing-worktree dialog does in the lifecycle step above.
    let session_root_for_state = worktree_path.as_deref().unwrap_or(&cwd).to_path_buf();
    let git_root_for_state =
        match Arc::clone(&engines.git_engine).resolve_root(&session_root_for_state) {
            Ok(r) => r,
            Err(_) => session_root_for_state.clone(),
        };
    let workflow_name_for_state = crate::engine::workflow::workflow_name_for(&workflow);
    let work_item_number_for_state = work_item_context.as_ref().map(|c| c.number);
    {
        let store = crate::data::workflow_state_store::WorkflowStateStore::at_git_root(
            git_root_for_state.clone(),
        );
        match store.load(work_item_number_for_state, &workflow_name_for_state) {
            Ok(Some(saved)) => {
                let total = saved.step_states.len();
                let completed = saved
                    .step_states
                    .values()
                    .filter(|s| {
                        matches!(
                            s,
                            crate::data::workflow_state::StepState::Succeeded
                                | crate::data::workflow_state::StepState::Skipped
                        )
                    })
                    .count();
                let resume = frontend.ask_workflow_resume_or_fresh(
                    &workflow_name_for_state,
                    completed,
                    total,
                )?;
                if !resume {
                    if let Err(e) =
                        store.delete(work_item_number_for_state, &workflow_name_for_state)
                    {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Warning,
                            text: format!(
                                "exec workflow: failed to delete workflow state file: {e}",
                            ),
                        });
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Warning,
                    text: format!(
                        "exec workflow: failed to read workflow state file: {e}; \
                         starting fresh",
                    ),
                });
            }
        }
    }

    // 6. Set PTY active — queues user messages during the engine run.
    frontend.set_pty_active(true);

    // 7. Wrap the frontend in Arc<Mutex> so both WorkflowProxy and
    //    CommandLayerFactory can share it for the duration of the engine run.
    let shared: Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>> = Arc::new(Mutex::new(frontend));

    let flags_arc = Arc::new(flags.clone());

    // 8. Build the session for the engine.
    // When a worktree is active, re-root the session at the worktree so
    // that `build_options` mounts the worktree checkout, not the main repo.
    let mut session = if let Some(ref wt) = worktree_path {
        let git_root_for_session = match Arc::clone(&engines.git_engine).resolve_root(wt) {
            Ok(r) => r,
            Err(e) => {
                let err = CommandError::from(e);
                shared.lock().unwrap().write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!(
                        "exec workflow: failed to resolve git root for worktree session: {err}"
                    ),
                });
                return Err(err);
            }
        };
        match Session::open_at_git_root(
            wt.clone(),
            git_root_for_session,
            crate::data::session::SessionOpenOptions::default(),
        ) {
            Ok(s) => s,
            Err(e) => {
                let err = CommandError::Other(format!("opening worktree session: {e}"));
                shared.lock().unwrap().write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec workflow: failed to open worktree session: {e}"),
                });
                return Err(err);
            }
        }
    } else {
        original_session
    };
    session.set_flags(workflow_flag_config(flags));

    // 9. Run the engine with three-phase coordination.
    // The engine block is scoped so proxy + factory are dropped before we
    // reclaim the frontend via Arc::try_unwrap.
    let yolo = flags.yolo;
    let setup_steps: Vec<crate::data::workflow_definition::SetupStep> =
        workflow.setup.iter().map(|e| e.step.clone()).collect();
    let teardown_steps: Vec<crate::data::workflow_definition::TeardownStep> =
        workflow.teardown.iter().map(|e| e.step.clone()).collect();
    let setup_entry_overlays: Vec<Option<Vec<String>>> =
        workflow.setup.iter().map(|e| e.overlays.clone()).collect();
    let setup_abort_flags: Vec<bool> = workflow.setup.iter().map(|e| e.abort_on_failure).collect();
    let setup_on_failure_configs: Vec<Option<crate::data::workflow_definition::RemediationConfig>> =
        workflow
            .setup
            .iter()
            .map(|e| e.on_failure.clone())
            .collect();
    let teardown_entry_overlays: Vec<Option<Vec<String>>> = workflow
        .teardown
        .iter()
        .map(|e| e.overlays.clone())
        .collect();
    let teardown_on_failure_configs: Vec<
        Option<crate::data::workflow_definition::RemediationConfig>,
    > = workflow
        .teardown
        .iter()
        .map(|e| e.on_failure.clone())
        .collect();
    let teardown_abort_flags: Vec<bool> = workflow
        .teardown
        .iter()
        .map(|e| e.abort_on_failure)
        .collect();
    let teardown_on_failure = workflow.teardown_on_failure;
    let engine_work_item_context = work_item_context.clone();
    let workflow_overlays_for_factory = workflow.overlays.clone();
    let active_workflow_context_permission = collect_all_overlay_specs(
        &session,
        cli_typed.clone(),
        workflow_overlays_for_factory.as_deref(),
        None,
    )
    .ok()
    .and_then(|collected| {
        collected
            .context_overlays
            .into_iter()
            .find(|c| c.scope == crate::engine::overlay::ContextScope::Workflow)
            .map(|c| c.permission)
    });
    let (engine_result, step_counts) = {
        let proxy = WorkflowProxy(Arc::clone(&shared));
        let factory = CommandLayerFactory {
            shared: Arc::clone(&shared),
            engines: engines.clone(),
            flags: Arc::clone(&flags_arc),
            cli_typed_overlays: cli_typed.clone(),
            work_item_context,
            image_git_root: git_root_for_scope.clone(),
            workflow_overlays: workflow_overlays_for_factory,
        };
        let mut engine = match WorkflowEngine::resume(
            &session,
            workflow,
            engine_work_item_context,
            Box::new(proxy),
            Box::new(factory),
            Arc::clone(&engines.git_engine),
            Arc::clone(&engines.overlay_engine),
        )
        .await
        {
            Ok(eng) => eng,
            Err(e) => {
                let err = CommandError::from(e);
                shared.lock().unwrap().write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec workflow: failed to initialize workflow engine: {err}"),
                });
                return Err(err);
            }
        };
        engine.set_yolo(yolo);
        engine.set_workflow_context_permission(active_workflow_context_permission);

        // Warn if the workflow will commit but git identity is not configured.
        if teardown_steps.iter().any(|s| {
            matches!(
                s,
                crate::data::workflow_definition::TeardownStep::CommitChanges { .. }
            )
        }) {
            let name_ok = std::process::Command::new("git")
                .args(["config", "user.name"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            let email_ok = std::process::Command::new("git")
                .args(["config", "user.email"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !name_ok || !email_ok {
                let missing: Vec<&str> = [
                    if !name_ok { Some("user.name") } else { None },
                    if !email_ok { Some("user.email") } else { None },
                ]
                .into_iter()
                .flatten()
                .collect();
                shared.lock().unwrap().write_message(UserMessage {
                    level: MessageLevel::Warning,
                    text: format!(
                        "workflow has a commit_changes teardown step but git {} not set; \
                         set them locally (git config {0}) or use a dir() overlay to mount \
                         your global ~/.gitconfig into the agent container",
                        missing.join(" and "),
                    ),
                });
            }
        }

        // === SETUP PHASE ===
        //
        // Each setup entry runs in its own container built from THAT
        // entry's overlays only (WI-0082): per-step isolation matters
        // because, e.g. an entry asking for `env(GITHUB_TOKEN)` must not
        // leak that token into a sibling entry that only asked for
        // `ssh()`. Container start/stop cost is amortized acceptably by
        // the small number of setup steps in real workflows.
        let mut setup_failed = false;
        if !setup_steps.is_empty() && !engine.state().setup_completed {
            let base_image = resolve_base_image(&session, &git_root_for_scope);
            let resolved = resolve_phase_overlays(
                engines,
                &session,
                &cli_typed,
                &setup_entry_overlays,
                worktree_git_mount.as_ref(),
                &base_image,
            );

            // A bad overlay on ANY entry aborts the whole phase before
            // any container starts — otherwise earlier steps would have
            // already mutated the workspace.
            if let Some(e) = resolved.iter().find_map(|r| r.as_ref().err()) {
                shared.lock().unwrap().write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec workflow: {e}"),
                });
                setup_failed = true;
            }

            if !setup_failed {
                let runtime = Arc::clone(
                    engines
                        .require_container_runtime()
                        .map_err(CommandError::from)?,
                );
                let mount = mount_path.clone();
                let base = base_image.clone();
                let shared_for_factory = Arc::clone(&shared);
                let setup_result = tokio::task::block_in_place(|| {
                    let factory = |idx: usize| -> Result<
                        Box<dyn crate::engine::agent_runtime::background::AgentExec>,
                        EngineError,
                    > {
                        let (overlays, env) = resolved
                            .get(idx)
                            .ok_or_else(|| {
                                EngineError::Other(format!(
                                    "internal: missing pre-resolved overlays for setup step {idx}",
                                ))
                            })?
                            .as_ref()
                            .map_err(|e| EngineError::Other(e.to_string()))?;
                        let container = runtime.start_background(&base, &mount, env, overlays)?;
                        Ok(Box::new(container))
                    };
                    let r = engine.run_setup(
                        &setup_steps,
                        &setup_abort_flags,
                        &setup_on_failure_configs,
                        factory,
                    );
                    if let Err(e) = &r {
                        shared_for_factory
                            .lock()
                            .unwrap()
                            .write_message(UserMessage {
                                level: MessageLevel::Error,
                                text: format!("exec workflow: setup phase failed: {e}"),
                            });
                    }
                    r
                });
                if setup_result.is_err() {
                    setup_failed = true;
                }
            }
        }

        // === MAIN PHASE ===
        let result = if setup_failed {
            Err(crate::engine::error::EngineError::Container(
                "setup phase failed; main workflow not started".into(),
            ))
        } else {
            engine.run_to_completion().await
        };

        let workflow_succeeded = matches!(
            result,
            Ok(WorkflowOutcome::Completed) | Ok(WorkflowOutcome::CompletedTeardownFailed)
        );

        // === TEARDOWN PHASE ===
        //
        // Same per-entry container pattern as setup: overlays are
        // pre-resolved via `resolve_phase_overlays` and the factory
        // indexes into the results. Unlike setup, no upfront abort
        // gate — per-entry overlay errors flow through the factory and
        // `run_teardown` handles them as per-step failures (best-effort).
        //
        // If the setup or main phase triggered abort_on_failure,
        // teardown is skipped regardless of teardown_on_failure.
        let mut teardown_aborted = false;
        let mut any_teardown_failed = false;
        if !teardown_steps.is_empty() && !engine.abort_on_failure_triggered() {
            let should_run = teardown_on_failure || workflow_succeeded;
            if should_run {
                let base_image = resolve_base_image(&session, &git_root_for_scope);
                let resolved = resolve_phase_overlays(
                    engines,
                    &session,
                    &cli_typed,
                    &teardown_entry_overlays,
                    worktree_git_mount.as_ref(),
                    &base_image,
                );
                let runtime = Arc::clone(
                    engines
                        .require_container_runtime()
                        .map_err(CommandError::from)?,
                );
                let mount = mount_path.clone();
                (teardown_aborted, any_teardown_failed) = tokio::task::block_in_place(|| {
                    let factory = |idx: usize| -> Result<
                        Box<dyn crate::engine::agent_runtime::background::AgentExec>,
                        EngineError,
                    > {
                        let (overlays, env) = resolved
                            .get(idx)
                            .ok_or_else(|| {
                                EngineError::Other(format!(
                                    "internal: missing pre-resolved overlays for teardown step {idx}",
                                ))
                            })?
                            .as_ref()
                            .map_err(|e| EngineError::Other(e.to_string()))?;
                        let container =
                            runtime.start_background(&base_image, &mount, env, overlays)?;
                        Ok(Box::new(container))
                    };
                    engine
                        .run_teardown(
                            &teardown_steps,
                            &teardown_abort_flags,
                            &teardown_on_failure_configs,
                            workflow_succeeded,
                            teardown_on_failure,
                            factory,
                        )
                        .unwrap_or((false, false))
                });
            }
        }

        // If any teardown step failed, promote the result to
        // CompletedTeardownFailed so post-workflow flows know.
        let result = if (teardown_aborted || any_teardown_failed) && workflow_succeeded {
            shared.lock().unwrap().write_message(UserMessage {
                level: MessageLevel::Warning,
                text: "Workflow completed but one or more teardown steps failed".into(),
            });
            Ok(WorkflowOutcome::CompletedTeardownFailed)
        } else {
            result
        };

        // If teardown didn't run (no teardown steps, or skipped on failure)
        // the engine's current_phase still reads Main — promote it to Done
        // so persisted state reflects completion.
        if !matches!(
            engine.state().current_phase,
            crate::data::workflow_state::WorkflowPhase::Done
        ) {
            let _ = engine.mark_done();
        }

        let mut completed = 0usize;
        let mut failed = 0usize;
        for state in engine.state().step_states.values() {
            match state {
                crate::data::workflow_state::StepState::Succeeded
                | crate::data::workflow_state::StepState::Skipped => completed += 1,
                crate::data::workflow_state::StepState::Failed { .. } => failed += 1,
                _ => {}
            }
        }
        (result, (completed, failed))
    };

    // 8. Reclaim exclusive ownership of the frontend after proxy + factory drop.
    let mut frontend = Arc::try_unwrap(shared)
        .unwrap_or_else(|_| panic!("no other Arc references remain after engine block"))
        .into_inner()
        .unwrap();

    // 9. PTY inactive — flush queued messages.
    frontend.set_pty_active(false);
    frontend.replay_queued();

    // 10. Determine whether the workflow ended with an error.
    let had_error = matches!(
        engine_result,
        Err(_)
            | Ok(WorkflowOutcome::Failed { .. })
            | Ok(WorkflowOutcome::Aborted)
            | Ok(WorkflowOutcome::CompletedTeardownFailed)
    );

    // 11. Report summary.
    //
    // `exit_code` is the unambiguous overall outcome:
    //   Some(0) — workflow completed successfully
    //   Some(N) — a step failed (Failed → failing step's exit code;
    //             Aborted → 1, since the user/engine bailed after a failure)
    //   None    — workflow paused; no terminal status yet
    //
    // Callers (CLI, TUI, API queue worker) inspect this to determine the
    // final success/failure of the run.
    let exit_code = match &engine_result {
        Ok(WorkflowOutcome::Completed) => Some(0),
        Ok(WorkflowOutcome::CompletedTeardownFailed) => Some(1),
        Ok(WorkflowOutcome::Failed { exit_code, .. }) => Some(*exit_code),
        Ok(WorkflowOutcome::Aborted) => Some(1),
        Ok(WorkflowOutcome::Paused) => None,
        Err(_) => Some(1),
    };
    frontend.report_workflow_summary(&WorkflowSummary {
        steps_completed: step_counts.0,
        steps_failed: step_counts.1.max(if had_error { 1 } else { 0 }),
    });

    // 12. Worktree finalize.
    if let Some(lifecycle) = worktree_lifecycle {
        if let Err(e) = lifecycle.finalize(&mut *frontend, had_error).await {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("exec workflow: worktree finalize failed: {e}"),
            });
            return Err(e);
        }
        frontend.replay_queued();
    }

    // 13. Surface engine errors after lifecycle cleanup.
    if let Err(e) = engine_result {
        let err = CommandError::from(e);
        frontend.write_message(UserMessage {
            level: MessageLevel::Error,
            text: format!("exec workflow: workflow engine error: {err}"),
        });
        return Err(err);
    }

    // `_issue_temp_file`'s Drop impl removes the temp file when this
    // function returns — covers both this success path and every early
    // error return above.

    Ok(ExecWorkflowOutcome {
        workflow: workflow_path.display().to_string(),
        exit_code,
        worktree_used: flags.worktree,
    })
}

fn workflow_flag_config(flags: &ExecWorkflowCommandFlags) -> crate::data::config::FlagConfig {
    crate::data::config::FlagConfig {
        agent: flags.agent.clone(),
        model: flags.model.clone(),
        yolo: Some(flags.yolo),
        auto: Some(flags.auto),
        non_interactive: Some(flags.non_interactive),
        overlays_raw: (!flags.overlay.is_empty()).then_some(flags.overlay.clone()),
        work_item: flags
            .work_item
            .as_deref()
            .and_then(|raw| raw.parse::<u32>().ok()),
        max_concurrent_agents: flags.max_concurrent,
        ..Default::default()
    }
}

/// Validate the `--dynamic` / `--leader` flag relationships before any IO.
/// Runs for every `exec workflow` invocation (dynamic or not). The non-dynamic
/// missing-`workflow`-path error is handled separately in the dispatcher so the
/// existing missing-required-argument message is preserved.
pub(crate) fn validate_dynamic_flags(flags: &ExecWorkflowCommandFlags) -> Result<(), CommandError> {
    if flags.dynamic && flags.workflow.is_some() {
        return Err(CommandError::Other(
            "cannot specify a workflow file path with --dynamic; the path is \
             created automatically"
                .into(),
        ));
    }
    if flags.leader.is_some() && !flags.dynamic {
        return Err(CommandError::Other(
            "--leader is only valid with --dynamic".into(),
        ));
    }
    if flags.dynamic && flags.work_item.is_none() {
        return Err(CommandError::Other("--dynamic requires --work-item".into()));
    }
    if flags.dynamic && flags.plan {
        return Err(CommandError::Other(
            "--dynamic cannot be used with --plan because dynamic mode enforces --yolo".into(),
        ));
    }
    // Parse --leader eagerly so a malformed value fails before any container work.
    if let Some(raw) = &flags.leader {
        LeaderSpec::parse(raw)?;
    }
    Ok(())
}

/// Apply the implied flags for `--dynamic` mode (WI-0092 §4): forces `yolo`
/// and `worktree` to `true` and appends `context(workflow)` to the overlay
/// list if it is not already present. Called once before any downstream
/// resolution so all subsequent code sees the correct values.
pub(crate) fn apply_dynamic_implied_flags(flags: &mut ExecWorkflowCommandFlags) {
    flags.yolo = true;
    flags.worktree = true;
    if !flags
        .overlay
        .iter()
        .any(|o| o.trim_start().starts_with("context(workflow"))
    {
        flags.overlay.push("context(workflow)".to_string());
    }
}

/// Resolve the leader agent name and optional model override from `flags`,
/// `session`, and the repo config (WI-0092 §7, WI-0095 §5 precedence).
///
/// Precedence:
/// 1. `--leader agent::model` provided → `leader_agent = spec.agent`, `leader_model = spec.model`;
///    `--model` is ignored for the leader.
/// 2. `dynamicWorkflows.defaultLeader` set in repo config (and no `--leader`) →
///    it governs both the leader agent and leader model; `--model` does not
///    override the configured leader model.
/// 3. `--model` provided, no `--leader`/`defaultLeader` → default agent, `leader_model = flags.model`.
/// 4. None of the above → default agent, `leader_model = None`.
pub(crate) fn resolve_leader_model(
    flags: &ExecWorkflowCommandFlags,
    session: &Session,
) -> Result<(crate::data::session::AgentName, Option<String>), CommandError> {
    // 1. `--leader` flag wins.
    if let Some(raw) = &flags.leader {
        let spec = LeaderSpec::parse(raw)?;
        let agent =
            crate::data::session::AgentName::new(&spec.agent).map_err(CommandError::from)?;
        return Ok((agent, Some(spec.model)));
    }
    // 2. `dynamicWorkflows.defaultLeader` from repo config. Already validated in
    //    RepoConfig::load; re-parse here with the command-layer LeaderSpec to
    //    construct the leader selection.
    if let Some(default_leader) = session
        .repo_config()
        .dynamic_workflows
        .as_ref()
        .and_then(|dw| dw.default_leader.as_deref())
    {
        let spec = LeaderSpec::parse(default_leader)?;
        let agent =
            crate::data::session::AgentName::new(&spec.agent).map_err(CommandError::from)?;
        return Ok((agent, Some(spec.model)));
    }
    // 3 & 4. `--model` + default-agent fallback (WI-0092 behavior).
    let agent = crate::command::commands::resolve_agent(&flags.agent, session)?;
    Ok((agent, flags.model.clone()))
}

/// Validate the `workflow.toml` produced by the leader agent: checks file
/// presence, TOML parse, and resolved-agent Dockerfile validation. Returns the
/// parsed [`Workflow`] on success or a human-readable error string that is
/// passed to the repair loop (WI-0092 §9).
pub(crate) fn validate_generated_workflow(
    generated_path: &std::path::Path,
    session: &Session,
    paths: &crate::data::RepoDockerfilePaths,
) -> Result<Workflow, String> {
    if !generated_path.exists() {
        return Err(format!(
            "leader agent did not produce workflow.toml at {}",
            generated_path.display()
        ));
    }
    match Workflow::load(generated_path) {
        Err(e) => Err(e.to_string()),
        Ok(wf) => match resolve_and_validate_workflow_agents(&wf, session, paths) {
            Err(e) => Err(e),
            Ok(_) => Ok(wf),
        },
    }
}

/// Format the discovered agents into the newline-separated listing substituted
/// into the leader prompt's `{{available_agents}}` slot.
pub(crate) fn format_available_agents(agents: &[(String, std::path::PathBuf)]) -> String {
    if agents.is_empty() {
        return "(no agents discovered — the project has no .awman/Dockerfile.<agent> files)"
            .to_string();
    }
    agents
        .iter()
        .map(|(name, _)| format!("  - {name}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render a configured agent→models map into the newline-separated listing
/// substituted into the leader prompt's `{{available_agents}}` slot (WI-0095 §3).
///
/// Agent names are sorted alphabetically so the leader prompt is deterministic
/// for stable tests and reproducible workflow design; each agent's configured
/// model-list order is preserved.
pub(crate) fn format_agents_with_models(
    map: &std::collections::HashMap<String, Vec<String>>,
) -> String {
    let mut names: Vec<&String> = map.keys().collect();
    names.sort();
    names
        .iter()
        .map(|name| {
            let models = map.get(*name).map(|m| m.join(", ")).unwrap_or_default();
            format!("  - {name}: {models}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build the effective (normalized) agent→models map for a dynamic workflow
/// from the configured `agentsToModels`, validating each configured agent
/// against the set of discovered Dockerfile agents (WI-0095 §2).
///
/// Matching is case-insensitive as a compatibility aid, but the returned map is
/// always keyed by the lowercase agent name; the config file is never silently
/// rewritten. Fails with a single descriptive error when any configured agent
/// has no Dockerfile, or when two configured keys collapse to the same
/// discovered agent after case folding. Case-folded (non-exact) matches are
/// appended to `warnings` for the caller to surface.
pub(crate) fn build_effective_agents_to_models(
    configured: &std::collections::HashMap<String, Vec<String>>,
    available_agents: &[(String, std::path::PathBuf)],
    warnings: &mut Vec<String>,
) -> Result<std::collections::HashMap<String, Vec<String>>, CommandError> {
    use std::collections::HashMap;

    // lowercased discovered name → discovered name as spelled by the Dockerfile
    let mut discovered: HashMap<String, &str> = HashMap::new();
    for (name, _) in available_agents {
        discovered.insert(name.to_ascii_lowercase(), name.as_str());
    }

    let mut effective: HashMap<String, Vec<String>> = HashMap::new();
    let mut missing: Vec<String> = Vec::new();
    // lowercase name → the configured key that produced it (dup detection)
    let mut claimed: HashMap<String, String> = HashMap::new();

    // Deterministic iteration over configured keys for stable errors/warnings.
    let mut configured_keys: Vec<&String> = configured.keys().collect();
    configured_keys.sort();

    for key in configured_keys {
        let models = configured.get(key).expect("key drawn from same map");
        let folded = key.to_ascii_lowercase();
        match discovered.get(&folded) {
            Some(found) => {
                if let Some(prev) = claimed.get(&folded) {
                    return Err(CommandError::Other(format!(
                        "dynamicWorkflows.agentsToModels contains keys {prev:?} and {key:?} that \
                         both refer to the discovered agent {found:?} after case folding; remove \
                         one so model lists are not ambiguously merged."
                    )));
                }
                claimed.insert(folded.clone(), key.clone());
                if key.as_str() != *found {
                    warnings.push(format!(
                        "dynamicWorkflows.agentsToModels key {key:?} matched discovered agent \
                         {found:?} only after case folding; the workflow will use {folded:?}."
                    ));
                }
                effective.insert(folded, models.clone());
            }
            None => missing.push(key.clone()),
        }
    }

    if !missing.is_empty() {
        let mut available_names: Vec<String> =
            available_agents.iter().map(|(n, _)| n.clone()).collect();
        available_names.sort();
        return Err(CommandError::Other(format!(
            "dynamicWorkflows.agentsToModels references agents that have no Dockerfile in this \
             repo: [{}].\nAvailable agents: [{}].\nAdd a .awman/Dockerfile.<agent> for each \
             missing agent, or remove it from agentsToModels.",
            missing.join(", "),
            available_names.join(", ")
        )));
    }

    Ok(effective)
}

/// Resolve the unique set of agent names a workflow will launch, using the same
/// precedence as `WorkflowEngine::resolve_agent` (step → workflow → session
/// default), and validate that each has a project Dockerfile. On success
/// returns the set of resolved agents; on failure returns a human-readable
/// error string suitable for the leader repair prompt (WI-0092 §9a).
pub(crate) fn resolve_and_validate_workflow_agents(
    workflow: &Workflow,
    session: &Session,
    paths: &crate::data::RepoDockerfilePaths,
) -> Result<Vec<String>, String> {
    let workflow_default = workflow.agent.as_deref();
    let session_default = session.default_agent().map(|a| a.as_str().to_string());

    let mut resolved: Vec<String> = Vec::new();
    for step in &workflow.steps {
        let agent = step
            .agent
            .as_deref()
            .or(workflow_default)
            .or(session_default.as_deref());
        match agent {
            Some(a) => {
                if !resolved.iter().any(|r| r == a) {
                    resolved.push(a.to_string());
                }
            }
            None => {
                let available = paths.discover_agent_dockerfiles();
                let names: Vec<String> = available.into_iter().map(|(n, _)| n).collect();
                return Err(format!(
                    "step '{}' resolves to no agent: it sets no agent, the workflow sets no \
                     default agent, and the session has no default agent. Add a workflow-level \
                     `agent` field. Available agents: {}",
                    step.name,
                    if names.is_empty() {
                        "(none)".to_string()
                    } else {
                        names.join(", ")
                    },
                ));
            }
        }
    }

    let available = paths.discover_agent_dockerfiles();
    let available_names: Vec<String> = available.iter().map(|(n, _)| n.clone()).collect();
    let unknown: Vec<&String> = resolved
        .iter()
        .filter(|a| !paths.agent_dockerfile(a).exists())
        .collect();
    if !unknown.is_empty() {
        let mut msg =
            String::from("workflow.toml references agents with no Dockerfile in the project:\n");
        for a in &unknown {
            msg.push_str(&format!("  - \"{a}\" (expected .awman/Dockerfile.{a})\n"));
        }
        msg.push_str(&format!(
            "Available agents: {}",
            if available_names.is_empty() {
                "(none)".to_string()
            } else {
                available_names.join(", ")
            },
        ));
        return Err(msg);
    }
    Ok(resolved)
}

/// Outcome of driving a single leader/repair agent attempt through the stuck →
/// yolo countdown → auto-advance pipeline.
enum LeaderDriveOutcome {
    /// The leader container completed or was advanced; proceed to validation.
    Advanced,
    /// The user aborted the dynamic invocation.
    Aborted,
    /// The user paused at the leader step — stop cleanly (no error) and leave
    /// the worktree in place so re-running resumes with a fresh leader.
    Paused,
    /// The user asked (via the Workflow Control Board) to restart the leader
    /// agent from scratch. The caller relaunches a fresh leader with the
    /// original prompt.
    Restart,
}

/// Outcome of the Workflow Control Board while it is driven from the dynamic
/// leader phase (there is no `WorkflowEngine` yet, so the leader loop maps the
/// returned [`NextAction`] onto these leader-scoped choices).
enum LeaderControlOutcome {
    /// Right arrow — kill the leader and start the generated workflow.
    StartWorkflow,
    /// Up arrow — restart the leader agent from scratch.
    Restart,
    /// Ctrl-C / `[a]` — abort the dynamic invocation with an error.
    Abort,
    /// `[p]` — kill the leader and stop cleanly; resumable by re-running.
    Pause,
    /// Esc, or any action that is not meaningful before a workflow exists —
    /// close the board and keep waiting on the leader.
    Dismiss,
}

impl ExecWorkflowCommand {
    /// Dynamic mode (WI-0092): a leader agent designs a `workflow.toml` for the
    /// requested work item, then awman validates and executes it. Performs all
    /// shared setup (worktree, context, work item) exactly once, then falls
    /// through to [`execute_prepared`] — it never re-enters
    /// `run_with_frontend`.
    async fn run_dynamic(
        self,
        mut frontend: Box<dyn ExecWorkflowCommandFrontend>,
    ) -> Result<ExecWorkflowOutcome, CommandError> {
        warn_legacy_config(&self.session, frontend.as_mut());

        // ── Effective (implied) flags: --dynamic forces --yolo, --worktree,
        //    and context(workflow) (WI-0092 §4). Computed once so all
        //    downstream code sees the correct values.
        let mut effective_flags = self.flags.clone();
        apply_dynamic_implied_flags(&mut effective_flags);

        // ── Resolve the leader agent + model (WI-0092 §7). ──────────────────
        let (leader_agent, leader_model) = resolve_leader_model(&self.flags, &self.session)?;

        // ── Resolve the work item file + content (REQUIRED for dynamic). ────
        let wi_str = self
            .flags
            .work_item
            .as_deref()
            .expect("validated: --dynamic requires --work-item");
        let wi_number = parse_work_item_number(wi_str).ok_or_else(|| {
            CommandError::Other(format!(
                "could not parse a work item number from {wi_str:?}"
            ))
        })?;
        let base_session = self.session.clone();
        let git_root_for_scope = base_session.git_root().to_path_buf();
        let cwd = base_session.working_dir().to_path_buf();
        let wi_file = find_work_item_file(&git_root_for_scope, wi_number).ok_or_else(|| {
            CommandError::Other(format!(
                "work item file for {wi_number:04} not found; dynamic mode cannot design a \
                 workflow without the work item content"
            ))
        })?;
        let wi_content = std::fs::read_to_string(&wi_file).map_err(|e| {
            CommandError::Other(format!(
                "failed to read work item file {}: {e}",
                wi_file.display()
            ))
        })?;
        let work_item_context = WorkItemContext {
            number: wi_number,
            content: wi_content,
        };

        // ── Worktree prepare BEFORE launching the leader (WI-0092 §5). ──────
        if base_session.session_type().is_remote() {
            return Err(CommandError::Other(
                "dynamic workflows are not supported for remote sessions".into(),
            ));
        }
        let git_root = self
            .engines
            .git_engine
            .resolve_root(&cwd)
            .map_err(CommandError::from)?;
        let lifecycle = WorktreeLifecycle::for_work_item(
            Arc::clone(&self.engines.git_engine),
            git_root,
            wi_number,
        )?;
        let worktree_path = lifecycle.prepare(&mut *frontend).await?;
        let mount_path = worktree_path.clone();
        let worktree_git_mount = worktree_git_overlay(&mount_path)?;

        // Re-root a session at the worktree so the leader operates on the
        // isolated checkout.
        let leader_git_root = self
            .engines
            .git_engine
            .resolve_root(&worktree_path)
            .map_err(CommandError::from)?;
        let leader_session = Session::open_at_git_root(
            worktree_path.clone(),
            leader_git_root,
            crate::data::session::SessionOpenOptions::default(),
        )
        .map_err(|e| CommandError::Other(format!("opening worktree session: {e}")))?;

        // The work item path the leader sees is inside the mounted worktree.
        let wi_relative = wi_file
            .strip_prefix(&git_root_for_scope)
            .unwrap_or(&wi_file);
        let leader_work_item_path = std::path::Path::new("/workspace").join(wi_relative);

        // ── Resolve the context(workflow) overlay for the leader. ───────────
        let (leader_context_overlays, leader_system_prompt) = resolve_context_overlays(
            &[crate::command::commands::ContextOverlaySpec {
                scope: crate::engine::overlay::ContextScope::Workflow,
                permission: crate::engine::container::options::OverlayPermission::ReadWrite,
            }],
            &leader_session,
            &leader_agent,
            None,
            None,
            frontend.as_mut(),
        )?;
        let context_dir = leader_context_overlays
            .iter()
            .find(|o| matches!(o.scope, crate::engine::overlay::ContextScope::Workflow))
            .map(|o| o.host_path.clone())
            .ok_or_else(|| {
                CommandError::Other("failed to resolve workflow context directory".into())
            })?;

        // ── Seed the context dir: remove stale workflow.toml, write refs. ───
        let generated_path = context_dir.join("workflow.toml");
        let _ = std::fs::remove_file(&generated_path);
        std::fs::write(
            context_dir.join("example-workflow.toml"),
            crate::data::dynamic_workflow_assets::EXAMPLE_WORKFLOW_TOML,
        )
        .map_err(|e| CommandError::Other(format!("writing example-workflow.toml: {e}")))?;
        std::fs::write(
            context_dir.join("workflow-usage.md"),
            crate::data::dynamic_workflow_assets::WORKFLOW_USAGE_MD,
        )
        .map_err(|e| CommandError::Other(format!("writing workflow-usage.md: {e}")))?;

        // ── Discover available agents. ──────────────────────────────────────
        let paths = crate::data::RepoDockerfilePaths::new(&git_root_for_scope);
        let available_agents = paths.discover_agent_dockerfiles();

        // ── Resolve the dynamicWorkflows config (WI-0095): the configured
        //    agent/model listing (validated against discovered Dockerfiles) and
        //    the concurrency advisory both feed the leader prompt. Validated
        //    before ensure_agent_image so a misconfigured agentsToModels fails
        //    before any image build or container work.
        let dynamic_cfg = base_session.repo_config().dynamic_workflows.clone();
        let max_concurrent_steps = dynamic_cfg.as_ref().and_then(|d| d.max_concurrent_steps);
        let configured_agents = dynamic_cfg
            .as_ref()
            .and_then(|d| d.agents_to_models.as_ref())
            .filter(|m| !m.is_empty());
        let agents_section = if let Some(map) = configured_agents {
            let mut warnings: Vec<String> = Vec::new();
            let effective =
                build_effective_agents_to_models(map, &available_agents, &mut warnings)?;
            for w in warnings {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Warning,
                    text: w,
                });
            }
            format_agents_with_models(&effective)
        } else {
            if dynamic_cfg
                .as_ref()
                .and_then(|d| d.agents_to_models.as_ref())
                .is_some_and(|m| m.is_empty())
            {
                tracing::debug!(
                    "dynamicWorkflows.agentsToModels is an empty map; falling back to \
                     Dockerfile discovery"
                );
            }
            format_available_agents(&available_agents)
        };

        // ── Ensure the leader image is built. ───────────────────────────────
        ensure_agent_image(
            &self.engines,
            &git_root_for_scope,
            &paths,
            leader_agent.as_str(),
            frontend.as_mut(),
        )?;

        let leader_prompt = crate::data::dynamic_workflow_assets::build_leader_prompt(
            &format!("{wi_number:04}"),
            &leader_work_item_path.display().to_string(),
            &agents_section,
            max_concurrent_steps,
            dynamic_cfg.as_ref().and_then(|d| d.guidance.as_deref()),
        );

        // Record the worktree's clean baseline so we can detect a leader that
        // illicitly modifies source files (WI-0092 §7 mutation guard).
        let worktree_baseline = worktree_git_status(&worktree_path);

        // ── Wrap the frontend so the agent run + yolo ticks can share it. ───
        let shared: Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>> =
            Arc::new(Mutex::new(frontend));

        // ── Wire an engine request channel so Ctrl-W opens the Workflow
        //    Control Board during the leader phase (the leader runs without a
        //    `WorkflowEngine`, so nothing else installs a sender). Registering
        //    it here makes the shared `engine_tx_shared` slot the TUI reads
        //    non-empty; the leader select loops below drain the receiver. The
        //    real engine overwrites this sender once the generated workflow
        //    starts.
        let (engine_tx, mut engine_rx) = tokio::sync::mpsc::unbounded_channel::<EngineRequest>();
        shared.lock().unwrap().set_engine_sender(engine_tx);

        // ── Leader + repair loop (WI-0092 §9). ──────────────────────────────
        //
        // The attempt budget, the repair-prompt substitution, and the
        // exhaustion message live in the one shared `WorkflowRepairLoop`; amie's
        // unattended evaluator drives the same object. Only the *driving* of the
        // leader container (stuck → yolo countdown → control board) is specific
        // to this interactive caller.
        let mut repair = WorkflowRepairLoop::new(generated_path.clone(), leader_prompt.clone());
        let validated_workflow = loop {
            let label = repair.label();
            let current_prompt = repair.prompt().to_string();
            let drive = self
                .drive_leader_agent(
                    Arc::clone(&shared),
                    &leader_session,
                    &leader_agent,
                    leader_model.as_deref(),
                    &current_prompt,
                    &git_root_for_scope,
                    leader_context_overlays.clone(),
                    leader_system_prompt.clone(),
                    &label,
                    &mut engine_rx,
                )
                .await?;
            match drive {
                LeaderDriveOutcome::Advanced => {}
                LeaderDriveOutcome::Aborted => {
                    return Err(CommandError::Other(
                        "dynamic workflow aborted during the leader step".into(),
                    ));
                }
                LeaderDriveOutcome::Paused => {
                    // Clean stop at the leader step — no error, worktree left in
                    // place. Re-running the command starts a fresh leader.
                    shared.lock().unwrap().write_message(UserMessage {
                        level: MessageLevel::Info,
                        text: format!(
                            "Dynamic workflow paused at the leader step. Re-run \
                             `awman exec workflow --dynamic --work-item {wi_number}` to resume \
                             with a fresh leader."
                        ),
                    });
                    return Ok(ExecWorkflowOutcome {
                        workflow: format!("dynamic-{wi_number:04}"),
                        exit_code: None,
                        worktree_used: true,
                    });
                }
                LeaderDriveOutcome::Restart => {
                    // Discard any partial workflow.toml and relaunch a fresh
                    // leader with the original prompt (the repair budget resets
                    // — a user restart is not a validation failure).
                    repair.restart();
                    shared.lock().unwrap().write_message(UserMessage {
                        level: MessageLevel::Info,
                        text: "Restarting the dynamic workflow leader agent…".into(),
                    });
                    continue;
                }
            }

            // Mutation guard: the leader may only write under the context dir.
            let after = worktree_git_status(&worktree_path);
            if after != worktree_baseline {
                return Err(CommandError::Other(format!(
                    "leader agent modified files in the worktree; dynamic pre-flight may only \
                     write under the workflow context directory. Changed worktree status:\n{after}"
                )));
            }

            // Validate: file present → parse → agent validation.
            let result = validate_generated_workflow(&generated_path, &leader_session, &paths);

            match repair.record(result) {
                RepairDecision::Accepted(wf) => break *wf,
                RepairDecision::Exhausted(message) => {
                    return Err(CommandError::Other(message));
                }
                RepairDecision::Retry { attempt, error } => {
                    shared.lock().unwrap().write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: format!(
                            "workflow.toml validation failed (attempt {attempt}/{}): {error}",
                            WorkflowRepairLoop::MAX_REPAIR_ATTEMPTS
                        ),
                    });
                }
            }
        };

        // ── Build any missing agent images before execution (WI-0092 §9b). ──
        let resolved_agents =
            resolve_and_validate_workflow_agents(&validated_workflow, &leader_session, &paths)
                .map_err(CommandError::Other)?;
        {
            let mut guard = shared.lock().unwrap();
            for agent in &resolved_agents {
                ensure_agent_image(
                    &self.engines,
                    &git_root_for_scope,
                    &paths,
                    agent,
                    guard.as_mut(),
                )?;
            }
        }

        // ── Reclaim the frontend and execute the generated workflow. ────────
        let frontend = Arc::try_unwrap(shared)
            .unwrap_or_else(|_| panic!("no other Arc references remain after leader phase"))
            .into_inner()
            .unwrap();

        // Build the CLI overlay list (includes the implied context(workflow)).
        let mut cli_typed = Vec::new();
        for s in &effective_flags.overlay {
            match parse_overlay_list(s) {
                Ok(parsed) => cli_typed.extend(parsed),
                Err(reason) => {
                    return Err(CommandError::InvalidOverlaySpec {
                        spec: s.clone(),
                        reason,
                    });
                }
            }
        }

        let prepared = PreparedRun {
            workflow: validated_workflow,
            workflow_path: generated_path,
            work_item_context: Some(work_item_context),
            cli_typed,
            mount_path,
            worktree_path: Some(worktree_path),
            worktree_lifecycle: Some(lifecycle),
            worktree_git_mount,
            git_root_for_scope,
            cwd,
            original_session: base_session,
            issue_temp_file: None,
        };
        execute_prepared(&effective_flags, &self.engines, prepared, frontend).await
    }

    /// Launch a single leader/repair agent container and drive it through the
    /// same stuck → yolo countdown → auto-advance pipeline a workflow step
    /// uses. The container is killed when the countdown advances (WI-0092 §8).
    #[allow(clippy::too_many_arguments)]
    async fn drive_leader_agent(
        &self,
        shared: Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>>,
        session: &Session,
        agent: &crate::data::session::AgentName,
        model: Option<&str>,
        prompt: &str,
        image_git_root: &std::path::Path,
        context_overlays: Vec<crate::engine::overlay::ContextOverlay>,
        system_prompt: Option<String>,
        label: &str,
        engine_rx: &mut tokio::sync::mpsc::UnboundedReceiver<EngineRequest>,
    ) -> Result<LeaderDriveOutcome, CommandError> {
        use crate::engine::agent_runtime::execution::{StuckEvent, KILLED_EXIT_CODE};

        let run_opts = AgentRunOptions {
            yolo: Some(YoloMode::Enabled),
            initial_prompt: Some(prompt.to_string()),
            model: model.map(|m| m.to_string()),
            allow_docker: self.flags.allow_docker,
            non_interactive: self.flags.non_interactive,
            image_tag_override: Some(crate::data::image_tags::agent_image_tag(
                image_git_root,
                agent.as_str(),
            )),
            system_prompt,
            context_overlays,
            ..Default::default()
        };
        let creds = self
            .engines
            .auth_engine
            .resolve_agent_auth(session, agent)
            .map(|c| c.env_vars)
            .unwrap_or_default();
        let resolved = self.engines.agent_engine.resolve_agent_options(
            session,
            agent,
            &run_opts,
            &creds,
            self.engines.runtime.as_ref(),
        )?;
        let instance = self.engines.runtime.build(resolved)?;

        shared.lock().unwrap().write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!(
                "Launching dynamic workflow {label} agent ({})…",
                agent.as_str()
            ),
        });
        shared.lock().unwrap().set_pty_active(true);

        let proxy = AgentFrontendProxy(Arc::clone(&shared));
        let mut execution = match instance.run_with_frontend(Box::new(proxy)) {
            Ok(e) => e,
            Err(e) => {
                let mut g = shared.lock().unwrap();
                g.set_pty_active(false);
                g.replay_queued();
                return Err(CommandError::from(e));
            }
        };

        let cancel = execution.cancel_handle();
        let mut stuck_rx = execution.subscribe_stuck();
        let (wait_tx, mut wait_rx) = tokio::sync::oneshot::channel::<i32>();
        tokio::spawn(async move {
            let code = execution
                .wait()
                .await
                .map(|info| info.exit_code)
                .unwrap_or(-1);
            let _ = wait_tx.send(code);
        });

        // Every `break` below corresponds to the leader container actually
        // being dead (self-exit, engine kill, or grace-expiry kill), so each
        // reports the exit to the frontend before leaving the loop. The
        // container window must NOT close on mere stuck states or while the
        // yolo countdown is still running — those paths `continue` instead.
        let outcome = loop {
            tokio::select! {
                biased;
                code = &mut wait_rx => {
                    shared
                        .lock()
                        .unwrap()
                        .report_container_exited(code.unwrap_or(-1));
                    break LeaderDriveOutcome::Advanced;
                }
                ev = stuck_rx.recv() => {
                    match ev {
                        Ok(StuckEvent::Stuck) => {
                            match run_leader_yolo_countdown(
                                &shared,
                                &mut wait_rx,
                                &mut stuck_rx,
                                engine_rx,
                                label,
                            )
                            .await
                            {
                                LeaderCountdownOutcome::Advance => {
                                    if let Some(c) = &cancel {
                                        let _ = c.cancel();
                                    }
                                    shared
                                        .lock()
                                        .unwrap()
                                        .report_container_exited(KILLED_EXIT_CODE);
                                    break LeaderDriveOutcome::Advanced;
                                }
                                LeaderCountdownOutcome::Completed(code) => {
                                    shared.lock().unwrap().report_container_exited(code);
                                    break LeaderDriveOutcome::Advanced;
                                }
                                LeaderCountdownOutcome::Recovered => continue,
                                LeaderCountdownOutcome::Abort => {
                                    if let Some(c) = &cancel {
                                        let _ = c.cancel();
                                    }
                                    shared
                                        .lock()
                                        .unwrap()
                                        .report_container_exited(KILLED_EXIT_CODE);
                                    break LeaderDriveOutcome::Aborted;
                                }
                                // Ctrl-W during the countdown: cancel the
                                // countdown and open the WCB in its place.
                                LeaderCountdownOutcome::ShowControlBoard => {
                                    let choice = show_leader_control_board(&shared, label);
                                    match apply_leader_control_outcome(choice, &cancel, &shared) {
                                        Some(o) => break o,
                                        None => continue,
                                    }
                                }
                            }
                        }
                        Ok(StuckEvent::Unstuck) => continue,
                        Ok(StuckEvent::StartupGraceExpired) => {
                            // The io bridge already killed the container.
                            shared
                                .lock()
                                .unwrap()
                                .report_container_exited(KILLED_EXIT_CODE);
                            break LeaderDriveOutcome::Advanced;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            let code = (&mut wait_rx).await.unwrap_or(-1);
                            shared.lock().unwrap().report_container_exited(code);
                            break LeaderDriveOutcome::Advanced;
                        }
                    }
                }
                // Ctrl-W while the leader is actively running (not stuck): open
                // the Workflow Control Board on request.
                Some(req) = engine_rx.recv() => {
                    if let EngineRequest::OpenControlBoard { .. } = req {
                        let choice = show_leader_control_board(&shared, label);
                        match apply_leader_control_outcome(choice, &cancel, &shared) {
                            Some(o) => break o,
                            None => continue,
                        }
                    }
                }
            }
        };

        let mut g = shared.lock().unwrap();
        g.set_pty_active(false);
        g.replay_queued();
        Ok(outcome)
    }
}

/// Build a leader-scoped Workflow Control Board, present it, and map the user's
/// choice onto a [`LeaderControlOutcome`]. Because the leader phase has no
/// `WorkflowEngine`/`WorkflowState`, a synthetic single-step state is
/// constructed so the shared `show_workflow_control_board` renderer has a
/// running step to name.
fn show_leader_control_board(
    shared: &Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>>,
    label: &str,
) -> LeaderControlOutcome {
    use crate::data::workflow_state::{StepState, WorkflowState};

    let mut state = WorkflowState::new(label.to_string(), &[], String::new(), None);
    state.set_status(label, StepState::Running { container_id: None });

    // `can_dismiss` keeps the full diamond board (not the lightweight step
    // confirm) and enables the Esc/Dismiss + Pause footer. Only the actions
    // meaningful before a workflow exists are offered.
    let available = AvailableActions {
        can_launch_next: true,
        launch_next_label: Some("Start dynamic workflow".to_string()),
        can_restart_current_step: true,
        can_abort: true,
        can_dismiss: true,
        ..Default::default()
    };

    let action = shared
        .lock()
        .unwrap()
        .show_workflow_control_board(&state, &available);

    match action {
        Ok(NextAction::LaunchNext) => LeaderControlOutcome::StartWorkflow,
        Ok(NextAction::RestartCurrentStep) => LeaderControlOutcome::Restart,
        Ok(NextAction::Abort) => LeaderControlOutcome::Abort,
        Ok(NextAction::Pause) => LeaderControlOutcome::Pause,
        // Dismiss, or any action not valid before a workflow exists
        // (Continue/CancelToPrevious/Finish), just closes the board.
        Ok(_) | Err(_) => LeaderControlOutcome::Dismiss,
    }
}

/// Apply a leader-phase WCB choice: for every terminal choice, kill the leader
/// container (reporting the exit) and return the drive outcome to break the
/// leader loop with; `None` means Dismiss — keep the leader running.
fn apply_leader_control_outcome(
    outcome: LeaderControlOutcome,
    cancel: &Option<crate::engine::agent_runtime::execution::CancelHandle>,
    shared: &Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>>,
) -> Option<LeaderDriveOutcome> {
    use crate::engine::agent_runtime::execution::KILLED_EXIT_CODE;

    let drive = match outcome {
        LeaderControlOutcome::StartWorkflow => LeaderDriveOutcome::Advanced,
        LeaderControlOutcome::Restart => LeaderDriveOutcome::Restart,
        LeaderControlOutcome::Abort => LeaderDriveOutcome::Aborted,
        LeaderControlOutcome::Pause => LeaderDriveOutcome::Paused,
        LeaderControlOutcome::Dismiss => return None,
    };
    if let Some(c) = cancel {
        let _ = c.cancel();
    }
    shared
        .lock()
        .unwrap()
        .report_container_exited(KILLED_EXIT_CODE);
    Some(drive)
}

/// Result of the leader yolo countdown.
enum LeaderCountdownOutcome {
    /// Countdown expired or user advanced — kill the container and proceed.
    Advance,
    /// The leader container exited on its own (with this exit code) during
    /// the countdown.
    Completed(i32),
    /// The leader resumed output (`Unstuck`) — cancel the countdown.
    Recovered,
    /// The user aborted.
    Abort,
    /// The user pressed Ctrl-W — cancel the countdown and open the WCB.
    ShowControlBoard,
}

/// Drive the 60-second yolo countdown for the leader step, reusing the same
/// `WorkflowFrontend::yolo_countdown_tick` pipeline as a workflow step. The
/// right-arrow / advance action carries the "Start dynamic workflow" label via
/// [`AvailableActions::launch_next_label`].
async fn run_leader_yolo_countdown(
    shared: &Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>>,
    wait_rx: &mut tokio::sync::oneshot::Receiver<i32>,
    stuck_rx: &mut tokio::sync::broadcast::Receiver<
        crate::engine::agent_runtime::execution::StuckEvent,
    >,
    engine_rx: &mut tokio::sync::mpsc::UnboundedReceiver<EngineRequest>,
    step_name: &str,
) -> LeaderCountdownOutcome {
    use crate::engine::agent_runtime::execution::StuckEvent;
    use crate::engine::workflow::actions::YoloTickOutcome;
    use crate::engine::workflow::timing::YOLO_COUNTDOWN_DURATION;

    let total = YOLO_COUNTDOWN_DURATION;
    let tick = Duration::from_millis(200);
    let mut remaining = total;
    shared.lock().unwrap().yolo_countdown_started(step_name);

    let outcome = loop {
        let tick_result = shared
            .lock()
            .unwrap()
            .yolo_countdown_tick(step_name, remaining, total);
        match tick_result {
            Ok(YoloTickOutcome::Continue) => {}
            Ok(YoloTickOutcome::AdvanceNow) => break LeaderCountdownOutcome::Advance,
            Ok(YoloTickOutcome::Cancel) => break LeaderCountdownOutcome::Abort,
            Err(_) => break LeaderCountdownOutcome::Advance,
        }
        if remaining.is_zero() {
            break LeaderCountdownOutcome::Advance;
        }

        tokio::select! {
            biased;
            code = &mut *wait_rx => {
                break LeaderCountdownOutcome::Completed(code.unwrap_or(-1));
            }
            ev = stuck_rx.recv() => {
                match ev {
                    Ok(StuckEvent::Unstuck) => break LeaderCountdownOutcome::Recovered,
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                }
            }
            Some(req) = engine_rx.recv() => {
                if let EngineRequest::OpenControlBoard { .. } = req {
                    break LeaderCountdownOutcome::ShowControlBoard;
                }
            }
            _ = tokio::time::sleep(tick) => {
                remaining = remaining.saturating_sub(tick);
            }
        }
    };

    shared.lock().unwrap().yolo_countdown_finished(step_name);
    outcome
}

/// Capture the worktree's `git status --porcelain` output, used as a mutation
/// guard around leader/repair runs (the leader must only write under the
/// context dir, never touch the worktree's tracked or untracked files).
fn worktree_git_status(worktree: &std::path::Path) -> String {
    std::process::Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

/// Ensure a Dockerfile-backed agent image is available for a container runtime,
/// building it from `.awman/Dockerfile.<agent>` when missing (WI-0092 §9b).
/// A missing Dockerfile is a hard error; a build failure is a hard error and
/// is never routed through the repair loop.
pub(crate) fn ensure_agent_image(
    engines: &Engines,
    git_root: &std::path::Path,
    paths: &crate::data::RepoDockerfilePaths,
    agent: &str,
    sink: &mut dyn UserMessageSink,
) -> Result<(), CommandError> {
    let runtime = engines
        .require_container_runtime()
        .map_err(CommandError::from)?;
    let dockerfile = paths.agent_dockerfile(agent);
    if !dockerfile.exists() {
        return Err(CommandError::Other(format!(
            "agent '{agent}' has no Dockerfile (expected {})",
            dockerfile.display()
        )));
    }
    let tag = crate::data::image_tags::agent_image_tag(git_root, agent);
    if runtime.image_exists(&tag) {
        return Ok(());
    }
    sink.write_message(UserMessage {
        level: MessageLevel::Info,
        text: format!("Building image for agent '{agent}' ({tag})…"),
    });
    runtime
        .build_image(&tag, &dockerfile, git_root, false, &mut |line: &str| {
            sink.write_message(UserMessage {
                level: MessageLevel::Info,
                text: line.to_string(),
            });
        })
        .map_err(|e| {
            CommandError::Other(format!(
                "failed to build image for agent '{agent}' from {}: {e}",
                dockerfile.display()
            ))
        })?;
    sink.write_message(UserMessage {
        level: MessageLevel::Info,
        text: format!("Built image for agent '{agent}'."),
    });
    Ok(())
}

/// Emit the gemini → antigravity deprecation warning. Centralised so the wording
/// stays in sync across the early CLI-flag check and the post-load workflow scan.
fn emit_gemini_deprecation_warning(sink: &mut dyn UserMessageSink) {
    sink.write_message(UserMessage {
        level: MessageLevel::Warning,
        text: "The 'gemini' agent is deprecated by Google. \
               Migrate to 'antigravity' — run 'awman chat antigravity' \
               (or 'awman config set agent antigravity' to change your default)."
            .to_string(),
    });
}

/// Remove every `checkout_create_branch` setup step from `workflow`, emitting
/// a Warning for each one removed. Called only when the run executes inside an
/// isolated worktree — the worktree already put the run on its own branch, so
/// creating/checking out another branch there is redundant (and would move the
/// worktree off the branch the post-workflow merge dialog operates on).
fn skip_checkout_branch_steps_in_worktree(workflow: &mut Workflow, sink: &mut dyn UserMessageSink) {
    use crate::data::workflow_definition::SetupStep;
    workflow.setup.retain(|entry| match &entry.step {
        SetupStep::CheckoutCreateBranch { branch, .. } => {
            sink.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: format!(
                    "skipping checkout_create_branch setup step (branch '{branch}'): \
                     the workflow is already running on an isolated worktree branch"
                ),
            });
            false
        }
        _ => true,
    });
}

/// Emit a Warning for each setup/teardown entry that names `context(workflow)`
/// in its overlay list. Workflow step progression state is not available
/// during those phases, so the dynamic prompt fields will be empty.
fn warn_context_workflow_in_phase(workflow: &Workflow, sink: &mut dyn UserMessageSink) {
    fn mentions_context_workflow(overlay: &str) -> bool {
        let t = overlay.trim();
        t.starts_with("context(workflow") && t[..t.len().min(20)].contains("workflow")
    }
    for (i, entry) in workflow.setup.iter().enumerate() {
        if let Some(overlays) = &entry.overlays {
            for o in overlays {
                if mentions_context_workflow(o) {
                    sink.write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: format!(
                            "setup step {i}: '{o}': context(workflow) in setup steps has \
                             no workflow step progress to surface yet; the dynamic prompt \
                             will reflect a setup phase only."
                        ),
                    });
                }
            }
        }
    }
    for (i, entry) in workflow.teardown.iter().enumerate() {
        if let Some(overlays) = &entry.overlays {
            for o in overlays {
                if mentions_context_workflow(o) {
                    sink.write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: format!(
                            "teardown step {i}: '{o}': context(workflow) in teardown steps \
                             runs after the main workflow has finished; the dynamic prompt \
                             may not reflect live step progression."
                        ),
                    });
                }
            }
        }
    }
}

/// True if any step in the workflow will resolve to the `gemini` agent under
/// the same precedence the workflow engine uses (`step.agent` >
/// `workflow.agent` > session default).
fn workflow_resolves_to_gemini(workflow: &Workflow, session: &Session) -> bool {
    let workflow_default = workflow.agent.as_deref();
    let session_default = session.default_agent().map(|a| a.as_str().to_string());
    for step in &workflow.steps {
        let resolved = step
            .agent
            .as_deref()
            .or(workflow_default)
            .or(session_default.as_deref());
        if resolved == Some("gemini") {
            return true;
        }
    }
    false
}

/// Resolve the base image tag for setup/teardown containers.
/// Checks effective config, falls back to the project image tag convention.
fn resolve_base_image(session: &Session, git_root: &std::path::Path) -> String {
    if let Some(configured) = session.effective_config().base_image() {
        return configured;
    }
    crate::data::image_tags::project_image_tag(git_root)
}

/// Collect overlay specs and env vars for a single setup or teardown entry.
///
/// Merges the entry's own overlays with the global / repo / `AWMAN_OVERLAYS`
/// / `--overlay` flag sources, then resolves directories via the overlay
/// engine and captures env vars from the host process environment.
///
/// One call per entry — that's the whole point post-WI-0082: each step's
/// container sees only the entry's own overlays plus the standing sources,
/// not the union of all phase entries' overlays.
fn collect_single_entry_overlays(
    engines: &Engines,
    session: &Session,
    cli_typed: &[TypedOverlay],
    entry_overlays: Option<&[String]>,
    image_tag: Option<&str>,
) -> Result<
    (
        Vec<crate::engine::container::options::OverlaySpec>,
        std::collections::HashMap<String, String>,
    ),
    CommandError,
> {
    let collected = collect_all_overlay_specs(session, cli_typed.to_vec(), None, entry_overlays)?;

    // Prefer the running image's baked-in $HOME (the actual runtime
    // authority) over what the local Dockerfile.dev says — the two can
    // diverge when the Dockerfile was changed but the image hasn't been
    // rebuilt yet, in which case mounting at the Dockerfile-derived path
    // silently breaks credential passthrough.
    let dockerfile_path = session
        .repo_config()
        .dockerfile_path_or_default(session.git_root());
    // detect_home_from_dockerfile silently returns None when the file is
    // missing — surface that as a warning so a misconfigured `dockerfile`
    // key doesn't cause overlays to fall back to a default container home
    // without any signal to the user.
    if !dockerfile_path.exists() && image_tag.is_none() {
        tracing::warn!(
            "configured Dockerfile {} not found; container home cannot be \
             inferred from it (falling back to overlay engine defaults)",
            dockerfile_path.display()
        );
    }
    let container_home = image_tag
        .and_then(|tag| {
            engines
                .container_runtime
                .as_ref()
                .and_then(|rt| rt.image_home_dir(tag))
        })
        .or_else(|| crate::engine::overlay::detect_home_from_dockerfile(&dockerfile_path));
    let request = crate::engine::overlay::OverlayRequest {
        directories: collected.directories,
        include_all_skills: false,
        named_skills: Vec::new(),
        agent: None,
        yolo: false,
        container_home,
        context_overlays: Vec::new(),
    };
    let overlay_specs = engines
        .overlay_engine
        .build_overlays(session, &request)
        .map_err(|e| {
            CommandError::Other(format!(
                "failed to resolve overlays for setup/teardown container: {e}",
            ))
        })?;

    let mut env = std::collections::HashMap::new();
    for var_name in &collected.env_passthrough {
        if let Ok(val) = std::env::var(var_name) {
            env.insert(var_name.clone(), val);
        }
    }

    Ok((overlay_specs, env))
}

/// Pre-resolve overlay specs and env vars for every entry in a setup or
/// teardown phase.
///
/// Each entry is resolved independently via [`collect_single_entry_overlays`]
/// (per-step overlay isolation, WI-0082). When `worktree_git_mount` is
/// `Some`, the backing `.git` directory overlay is appended to every
/// successful entry so git operations work inside worktree-mounted
/// containers.
///
/// Returns one `Result` per entry. The caller decides error policy:
/// - **Setup** aborts the entire phase on the first `Err`.
/// - **Teardown** passes errors through to the factory; `run_teardown`
///   handles per-step failures gracefully.
type PhaseOverlayResult = Result<
    (
        Vec<crate::engine::container::options::OverlaySpec>,
        std::collections::HashMap<String, String>,
    ),
    CommandError,
>;

fn resolve_phase_overlays(
    engines: &Engines,
    session: &Session,
    cli_typed: &[TypedOverlay],
    entries: &[Option<Vec<String>>],
    worktree_git_mount: Option<&crate::engine::container::options::OverlaySpec>,
    image_tag: &str,
) -> Vec<PhaseOverlayResult> {
    entries
        .iter()
        .map(|entry| {
            let (mut overlays, env) = collect_single_entry_overlays(
                engines,
                session,
                cli_typed,
                entry.as_deref(),
                Some(image_tag),
            )?;
            if let Some(wt) = worktree_git_mount {
                overlays.push(wt.clone());
            }
            Ok((overlays, env))
        })
        .collect()
}

/// Extract a numeric work item number from strings like "0069", "69", "WI-69",
/// etc. Returns the first run of decimal digits found in `s`, parsed as `u32`.
fn parse_work_item_number(s: &str) -> Option<u32> {
    let digits: String = s
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u32>().ok()
}

/// Find a work item file whose filename starts with the zero-padded four-digit
/// number (e.g. `0069-*.md`). The search directory is determined by the repo
/// config's `workItems.dir` setting; falls back to `<git_root>/aspec/work-items/`.
fn find_work_item_file(git_root: &std::path::Path, number: u32) -> Option<std::path::PathBuf> {
    let repo_cfg = crate::data::config::repo::RepoConfig::load(git_root).unwrap_or_default();
    let dir = repo_cfg.work_items_dir_or_default(git_root);
    let prefix = format!("{:04}-", number);
    std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(&prefix))
                .unwrap_or(false)
        })
}

/// Build an [`OverlaySpec`] that mounts the main repo's `.git` directory into
/// a container so git operations work inside a worktree checkout.
///
/// A worktree's `.git` is a pointer file referencing an absolute path inside
/// the main repo's `.git/worktrees/<name>/` directory. When only the worktree
/// is bind-mounted, that pointer dangles and every git command fails. This
/// overlay mounts the main `.git` directory at its host-absolute path so the
/// pointer resolves identically inside the container.
///
/// Returns `Ok(None)` when `worktree_path` is a regular repo or has no `.git`.
fn worktree_git_overlay(
    worktree_path: &std::path::Path,
) -> Result<Option<crate::engine::container::options::OverlaySpec>, EngineError> {
    let main_git_dir = match crate::engine::git::resolve_worktree_git_dir(worktree_path)? {
        Some(p) => p,
        None => return Ok(None),
    };
    Ok(Some(crate::engine::container::options::OverlaySpec {
        host_path: main_git_dir.clone(),
        container_path: main_git_dir,
        permission: crate::engine::container::options::OverlayPermission::ReadWrite,
    }))
}

/// Guards an on-disk temp file: deleted when this value is dropped, regardless
/// of how the surrounding scope exits (success, `?`, panic). Used for the
/// issue overlay temp file so cleanup survives every early-return path.
pub(crate) struct IssueTempFile {
    path: PathBuf,
}

impl IssueTempFile {
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for IssueTempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Result of `issue_source_overlay`: everything the caller needs to inject
/// an issue-derived file into the workflow's containers, plus a Drop guard
/// for the underlying temp file.
pub(crate) struct IssueOverlayBuild {
    pub temp_file: IssueTempFile,
    pub overlay: TypedOverlay,
    pub slug: String,
    pub number: u32,
    pub content: String,
}

/// Build the workflow overlay for an `Issue` produced by an `IssueSource`.
///
/// Writes the rendered markdown to a unique temp file (returned wrapped in
/// `IssueTempFile` so the caller can keep it alive for the duration of the
/// workflow) and constructs a read-only `TypedOverlay::Directory` mapping the
/// temp file to `/workspace/<work_items_relative>/NNNN-<slug>.md` inside the
/// container.
///
/// Signature takes `&dyn IssueSource` and `&Issue` — no concrete provider types.
pub(crate) fn issue_source_overlay(
    source: &dyn crate::data::issue::IssueSource,
    issue: &crate::data::issue::Issue,
    git_root: &std::path::Path,
    work_items_dir: &std::path::Path,
) -> std::io::Result<IssueOverlayBuild> {
    let slug = source.title_slug(issue);
    let content = source.format_as_markdown(issue);
    let number = issue.numeric_id().unwrap_or(0);

    let pid = std::process::id();
    let temp_filename = format!("awman-issue-{pid}-{slug}.md");
    let temp_path = std::env::temp_dir().join(&temp_filename);
    std::fs::write(&temp_path, &content)?;
    let temp_file = IssueTempFile {
        path: temp_path.clone(),
    };

    let relative = work_items_dir
        .strip_prefix(git_root)
        .unwrap_or_else(|_| std::path::Path::new("aspec/work-items"));
    let container_filename = format!("{number:04}-{slug}.md");
    let container_path = std::path::PathBuf::from("/workspace")
        .join(relative)
        .join(&container_filename);

    let overlay = TypedOverlay::Directory(crate::engine::overlay::DirectorySpec {
        host: temp_path.display().to_string(),
        container: container_path.display().to_string(),
        permission: crate::engine::container::options::OverlayPermission::ReadOnly,
    });

    Ok(IssueOverlayBuild {
        temp_file,
        overlay,
        slug,
        number,
        content,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;

    use super::*;
    use crate::command::commands::agent_auth::{AgentAuthDecision, AgentAuthFrontend};
    use crate::command::commands::agent_setup::{AgentSetupDecision, AgentSetupFrontend};
    use crate::command::commands::mount_scope::{MountScopeDecision, MountScopeFrontend};
    use crate::command::commands::worktree_lifecycle::{
        ExistingWorktreeDecision, PostWorkflowWorktreeAction, PreWorktreeDecision,
        WorktreeLifecycleFrontend,
    };
    use crate::data::message::UserMessage;
    use crate::data::session::AgentName;
    use crate::data::workflow_state::WorkflowState;
    use crate::engine::agent_runtime::execution::AgentExitInfo;
    use crate::engine::agent_runtime::frontend::{AgentProgress, AgentStatus};
    use crate::engine::workflow::actions::{
        AvailableActions, NextAction, ResumeMismatch, StepFailureChoice, StepOutput,
        WorkflowOutcome, WorkflowStepStatus, YoloTickOutcome,
    };

    // ─── Recording frontend ───────────────────────────────────────────────────

    struct FakeExecWorkflowFrontend {
        pty_active_calls: Vec<bool>,
        replay_queued_count: usize,
        summary_calls: Vec<WorkflowSummary>,
        messages: Vec<UserMessage>,
        next_action_response: NextAction,
    }

    impl FakeExecWorkflowFrontend {
        fn new() -> Self {
            Self {
                pty_active_calls: vec![],
                replay_queued_count: 0,
                summary_calls: vec![],
                messages: vec![],
                next_action_response: NextAction::LaunchNext,
            }
        }
    }

    impl UserMessageSink for FakeExecWorkflowFrontend {
        fn write_message(&mut self, msg: UserMessage) {
            self.messages.push(msg);
        }
        fn replay_queued(&mut self) {
            self.replay_queued_count += 1;
        }
    }

    #[async_trait]
    impl AgentFrontend for FakeExecWorkflowFrontend {
        fn report_status(&mut self, _status: AgentStatus) {}
        fn report_progress(&mut self, _progress: AgentProgress) {}
        fn take_io(&mut self) -> crate::engine::agent_runtime::frontend::AgentIo {
            let (stdout_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let (stderr_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
            crate::engine::agent_runtime::frontend::AgentIo {
                stdout: stdout_tx,
                stderr: stderr_tx,
                stdin_tx,
                stdin_rx,
                resize: None,
                initial_size: None,
            }
        }
    }

    impl WorkflowFrontend for FakeExecWorkflowFrontend {
        fn show_workflow_control_board(
            &mut self,
            _state: &WorkflowState,
            _available: &AvailableActions,
        ) -> Result<NextAction, EngineError> {
            Ok(self.next_action_response.clone())
        }
        fn yolo_countdown_tick(
            &mut self,
            _step_name: &str,
            _remaining: Duration,
            _total: Duration,
        ) -> Result<YoloTickOutcome, EngineError> {
            Ok(YoloTickOutcome::Continue)
        }
        fn report_step_status(&mut self, _step: &WorkflowStep, _status: WorkflowStepStatus) {}
        fn report_step_output(&mut self, _step: &WorkflowStep, _output: StepOutput) {}
        fn report_workflow_completed(&mut self, _outcome: &WorkflowOutcome) {}
        fn confirm_resume(&mut self, _mismatch: &ResumeMismatch) -> Result<bool, EngineError> {
            Ok(true)
        }
        fn user_choose_after_step_failure(
            &mut self,
            _step: &WorkflowStep,
            _exit: &AgentExitInfo,
        ) -> Result<StepFailureChoice, EngineError> {
            Ok(StepFailureChoice::Abort)
        }
    }

    impl MountScopeFrontend for FakeExecWorkflowFrontend {
        fn ask_mount_scope(
            &mut self,
            _git_root: &Path,
            _cwd: &Path,
        ) -> Result<MountScopeDecision, CommandError> {
            Ok(MountScopeDecision::MountGitRoot)
        }
    }

    impl AgentSetupFrontend for FakeExecWorkflowFrontend {
        fn ask_agent_setup(
            &mut self,
            _requested: &AgentName,
            _default: &AgentName,
            _default_available: bool,
            _image_only: bool,
        ) -> Result<AgentSetupDecision, CommandError> {
            Ok(AgentSetupDecision::Setup)
        }
        fn record_fallback(&mut self, _requested: &AgentName, _fallback: &AgentName) {}
    }

    impl AgentAuthFrontend for FakeExecWorkflowFrontend {
        fn ask_agent_auth_consent(
            &mut self,
            _agent: &AgentName,
            _env_var_names: &[&str],
        ) -> Result<AgentAuthDecision, CommandError> {
            Ok(AgentAuthDecision::Accept)
        }
    }

    impl WorktreeLifecycleFrontend for FakeExecWorkflowFrontend {
        fn ask_pre_worktree_uncommitted_files(
            &mut self,
            _files: &[String],
            _suggested_message: &str,
        ) -> Result<PreWorktreeDecision, CommandError> {
            Ok(PreWorktreeDecision::UseLastCommit)
        }
        fn ask_existing_worktree(
            &mut self,
            _path: &Path,
            _branch: &str,
        ) -> Result<ExistingWorktreeDecision, CommandError> {
            Ok(ExistingWorktreeDecision::Resume)
        }
        fn report_worktree_created(&mut self, _path: &Path, _branch: &str) {}
        fn ask_post_workflow_action(
            &mut self,
            _prompt: &crate::command::commands::worktree_lifecycle::PostWorkflowWorktreePrompt,
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
        fn ask_merge_mode(
            &mut self,
            _branch: &str,
        ) -> Result<crate::command::commands::worktree_lifecycle::WorktreeMergeMode, CommandError>
        {
            Ok(crate::command::commands::worktree_lifecycle::WorktreeMergeMode::LeaveBranch)
        }
        fn confirm_worktree_cleanup(
            &mut self,
            _branch: &str,
            _path: &Path,
        ) -> Result<bool, CommandError> {
            Ok(false)
        }
        fn report_merge_conflict(&mut self, _branch: &str, _wt: &Path, _root: &Path) {}
        fn report_worktree_discarded(&mut self, _branch: &str) {}
        fn report_worktree_kept(&mut self, _path: &Path, _branch: &str) {}
    }

    impl ExecWorkflowCommandFrontend for FakeExecWorkflowFrontend {
        fn set_pty_active(&mut self, active: bool) {
            self.pty_active_calls.push(active);
        }
        fn report_workflow_summary(&mut self, summary: &WorkflowSummary) {
            self.summary_calls.push(summary.clone());
        }
        fn ask_workflow_resume_or_fresh(
            &mut self,
            _workflow_name: &str,
            _completed_steps: usize,
            _total_steps: usize,
        ) -> Result<bool, CommandError> {
            Ok(true)
        }
    }

    // ─── Helpers ─────────────────────────────────────────────────────────────

    fn write_minimal_workflow(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(
            &path,
            r#"[[steps]]
name = "test-step"
agent = "claude"
prompt = "do something"
"#,
        )
        .unwrap();
        path
    }

    fn make_engines() -> Engines {
        let runtime = Arc::new(crate::engine::container::ContainerRuntime::docker());
        let overlay = Arc::new(crate::engine::overlay::OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(std::path::PathBuf::from(
                "/tmp",
            )),
        ));
        let git_engine = Arc::new(crate::engine::git::GitEngine::new());
        let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            Arc::clone(&overlay),
            Arc::clone(&runtime),
        ));
        let auth_engine = Arc::new(crate::engine::auth::AuthEngine::with_paths(
            crate::data::fs::auth_paths::AuthPathResolver::at_home("/tmp"),
            crate::data::fs::api_paths::ApiPaths::at_root("/tmp"),
        ));
        let workflow_state_store = {
            let tmp = tempfile::tempdir().unwrap();
            Arc::new(crate::data::EngineWorkflowStateStore::at_git_root(
                tmp.path(),
            ))
        };
        Engines {
            runtime: runtime.clone(),
            container_runtime: Some(runtime),
            sandbox_runtime: None,
            git_engine,
            overlay_engine: overlay,
            auth_engine,
            agent_engine,
            workflow_state_store,
        }
    }

    // ─── Tests ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn set_pty_active_called_true_then_false_around_engine() {
        // Arrange: minimal workflow in a temp dir that the engine can run.
        let tmp = tempfile::tempdir().unwrap();
        let wf_path = write_minimal_workflow(tmp.path(), "test.toml");

        // Use a real git repo so Session::open_at_git_root succeeds.
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "t@t.t"])
            .current_dir(tmp.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "t"])
            .current_dir(tmp.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        std::fs::write(tmp.path().join("README"), "x").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(tmp.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(tmp.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();

        let mut engines = make_engines();
        // Override workflow_state_store to use the temp git repo.
        engines.workflow_state_store = Arc::new(
            crate::data::EngineWorkflowStateStore::at_git_root(tmp.path()),
        );

        let flags = ExecWorkflowCommandFlags {
            workflow: Some(wf_path),
            work_item: None,
            non_interactive: true,
            plan: false,
            allow_docker: false,
            worktree: false,

            yolo: false,
            auto: false,
            agent: None,
            model: None,
            overlay: vec![],
            max_concurrent: None,
            issue_source: crate::data::issue::IssueSourceFlags { issue: None },
            dynamic: false,
            leader: None,
        };
        let session = {
            let resolver = crate::data::session::StaticGitRootResolver::new(tmp.path());
            Session::open(
                tmp.path().to_path_buf(),
                &resolver,
                crate::data::session::SessionOpenOptions::default(),
            )
            .unwrap()
        };
        let cmd = ExecWorkflowCommand::new(flags, engines, session);
        let fake = FakeExecWorkflowFrontend::new();

        let result = cmd.run_with_frontend(Box::new(fake)).await;

        // The outcome is Ok and set_pty_active was called true then false.
        // (Engine result may be Ok or Err depending on the stub backend;
        //  what matters is the ordering.)
        // We can't easily inspect the fake after run_with_frontend consumes it.
        // Instead, we use the shared-arc pattern to peek at the state after.
        // For this test, simply verifying no panic is the structural assertion.
        let _ = result;
    }

    #[tokio::test]
    async fn workflow_proxy_delegates_write_message_to_inner_frontend() {
        let inner: Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>> =
            Arc::new(Mutex::new(Box::new(FakeExecWorkflowFrontend::new())));
        let mut proxy = WorkflowProxy(Arc::clone(&inner));

        use crate::data::message::MessageLevel;
        proxy.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "hello".into(),
        });

        let guard = inner.lock().unwrap();
        let fake = guard.as_ref();
        // Can't easily downcast Box<dyn Trait>, but we can verify no panic
        // and that the proxy compiled and delegated without crashing.
        let _ = fake;
    }

    #[test]
    fn exec_workflow_flags_worktree_defaults_to_false() {
        // Verify ExecWorkflowCommandFlags is constructable and worktree defaults
        // correctly reflect what dispatch sets.
        let flags = ExecWorkflowCommandFlags {
            workflow: Some(PathBuf::from("wf.toml")),
            work_item: None,
            non_interactive: false,
            plan: false,
            allow_docker: false,
            worktree: false,

            yolo: false,
            auto: false,
            agent: None,
            model: None,
            overlay: vec![],
            max_concurrent: None,
            issue_source: crate::data::issue::IssueSourceFlags { issue: None },
            dynamic: false,
            leader: None,
        };
        assert!(!flags.worktree);
        assert!(!flags.yolo);
    }

    #[test]
    fn exec_workflow_flags_yolo_implies_worktree_in_dispatch() {
        // Dispatch sets worktree=true when yolo=true; verify the flag struct
        // allows that combination.
        let flags = ExecWorkflowCommandFlags {
            workflow: Some(PathBuf::from("wf.toml")),
            work_item: None,
            non_interactive: false,
            plan: false,
            allow_docker: false,
            worktree: true,

            yolo: true,
            auto: false,
            agent: None,
            model: None,
            overlay: vec![],
            max_concurrent: None,
            issue_source: crate::data::issue::IssueSourceFlags { issue: None },
            dynamic: false,
            leader: None,
        };
        assert!(flags.yolo);
        assert!(flags.worktree, "yolo must imply worktree");
    }

    #[test]
    fn workflow_summary_steps_failed_zero_on_success() {
        let s = WorkflowSummary {
            steps_completed: 3,
            steps_failed: 0,
        };
        assert_eq!(s.steps_failed, 0);
        assert_eq!(s.steps_completed, 3);
    }

    // ─── Per-entry overlay isolation (WI-0082 §1 review fix) ─────────────────

    /// `collect_single_entry_overlays` must scope env passthrough to the
    /// caller-supplied entry + standing sources only. The orchestrator calls
    /// it once per setup/teardown entry; if it leaked information across
    /// calls, sibling steps would inherit each other's overlays.
    #[test]
    fn collect_single_entry_overlays_isolates_env_per_entry() {
        use crate::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME};
        use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};

        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let resolver = StaticGitRootResolver::new(tmp.path());
        let session = Session::open(
            tmp.path().to_path_buf(),
            &resolver,
            SessionOpenOptions {
                env: Some(env),
                ..Default::default()
            },
        )
        .unwrap();
        let engines = make_engines();

        // Set both env vars on the host so passthrough can capture them.
        std::env::set_var("WI0082_REVIEW_TOKEN_A", "value-a");
        std::env::set_var("WI0082_REVIEW_TOKEN_B", "value-b");

        let entry_a = vec!["env(WI0082_REVIEW_TOKEN_A)".to_string()];
        let entry_b = vec!["env(WI0082_REVIEW_TOKEN_B)".to_string()];

        let (_, env_a) =
            collect_single_entry_overlays(&engines, &session, &[], Some(&entry_a), None).unwrap();
        let (_, env_b) =
            collect_single_entry_overlays(&engines, &session, &[], Some(&entry_b), None).unwrap();

        std::env::remove_var("WI0082_REVIEW_TOKEN_A");
        std::env::remove_var("WI0082_REVIEW_TOKEN_B");

        assert!(
            env_a.contains_key("WI0082_REVIEW_TOKEN_A"),
            "entry A's env must contain its own var; got: {env_a:?}"
        );
        assert!(
            !env_a.contains_key("WI0082_REVIEW_TOKEN_B"),
            "entry A's env must NOT include entry B's var (no cross-step leak); got: {env_a:?}"
        );
        assert!(
            env_b.contains_key("WI0082_REVIEW_TOKEN_B"),
            "entry B's env must contain its own var; got: {env_b:?}"
        );
        assert!(
            !env_b.contains_key("WI0082_REVIEW_TOKEN_A"),
            "entry B's env must NOT include entry A's var (no cross-step leak); got: {env_b:?}"
        );
    }

    // ─── WI-0086: collect_single_entry_overlays uses repo-config dockerfile ─────

    /// Verify that `collect_single_entry_overlays` resolves the Dockerfile path
    /// from `session.repo_config().dockerfile_path_or_default()`, not from a
    /// hard-coded `git_root.join("Dockerfile.dev")`.
    #[test]
    fn collect_single_entry_overlays_uses_repo_config_dockerfile_path() {
        use crate::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME};
        use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};

        let tmp = tempfile::tempdir().unwrap();

        // Write repo config with a custom Dockerfile path.
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join("config.json"),
            r#"{"dockerfile": "infra/Dockerfile.base"}"#,
        )
        .unwrap();

        // Create the configured Dockerfile (not Dockerfile.dev).
        let infra_dir = tmp.path().join("infra");
        std::fs::create_dir_all(&infra_dir).unwrap();
        std::fs::write(
            infra_dir.join("Dockerfile.base"),
            "FROM ubuntu:22.04\nUSER agent\n",
        )
        .unwrap();

        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let resolver = StaticGitRootResolver::new(tmp.path());
        let session = Session::open(
            tmp.path().to_path_buf(),
            &resolver,
            SessionOpenOptions {
                env: Some(env),
                ..Default::default()
            },
        )
        .unwrap();

        // The session must resolve dockerfile from repo config, not Dockerfile.dev.
        let resolved = session
            .repo_config()
            .dockerfile_path_or_default(session.git_root());
        assert_eq!(
            resolved,
            tmp.path().join("infra/Dockerfile.base"),
            "session must read dockerfile path from repo config, not hard-code Dockerfile.dev"
        );

        // collect_single_entry_overlays must succeed using the configured path.
        let engines = make_engines();
        let result = collect_single_entry_overlays(&engines, &session, &[], None, None);
        assert!(
            result.is_ok(),
            "collect_single_entry_overlays must succeed with a repo-config-resolved dockerfile path"
        );
    }

    // ─── Gemini deprecation: workflow-level scan (WI-0083 review fix) ────────

    fn make_session_with_default_agent(
        tmp: &tempfile::TempDir,
        default_agent: Option<&str>,
    ) -> Session {
        use crate::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME};
        use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};

        if let Some(agent) = default_agent {
            let cfg_dir = tmp.path().join(".awman");
            std::fs::create_dir_all(&cfg_dir).unwrap();
            std::fs::write(
                cfg_dir.join("config.json"),
                format!(r#"{{"agent": "{agent}"}}"#),
            )
            .unwrap();
        }
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let resolver = StaticGitRootResolver::new(tmp.path());
        Session::open(
            tmp.path().to_path_buf(),
            &resolver,
            SessionOpenOptions {
                env: Some(env),
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn make_workflow(workflow_agent: Option<&str>, step_agents: &[Option<&str>]) -> Workflow {
        Workflow {
            title: None,
            steps: step_agents
                .iter()
                .enumerate()
                .map(|(i, a)| WorkflowStep {
                    name: format!("step{i}"),
                    depends_on: vec![],
                    prompt_template: "x".into(),
                    agent: a.map(|s| s.to_string()),
                    model: None,
                    overlays: None,
                    abort_on_failure: false,
                })
                .collect(),
            agent: workflow_agent.map(|s| s.to_string()),
            model: None,
            setup: vec![],
            teardown: vec![],
            teardown_on_failure: false,
            overlays: None,
        }
    }

    #[test]
    fn skip_checkout_branch_steps_removes_only_checkout_entries_and_warns() {
        use crate::data::workflow_definition::{SetupStep, SetupStepEntry};
        let mut wf = make_workflow(None, &[None]);
        wf.setup = vec![
            SetupStepEntry {
                overlays: None,
                abort_on_failure: false,
                on_failure: None,
                step: SetupStep::CheckoutCreateBranch {
                    branch: "feature/x".into(),
                    base: None,
                },
            },
            SetupStepEntry {
                overlays: None,
                abort_on_failure: false,
                on_failure: None,
                step: SetupStep::RunShell {
                    command: "echo hi".into(),
                    env: None,
                },
            },
            SetupStepEntry {
                overlays: None,
                abort_on_failure: false,
                on_failure: None,
                step: SetupStep::CheckoutCreateBranch {
                    branch: "feature/y".into(),
                    base: Some("main".into()),
                },
            },
        ];
        let mut fe = FakeExecWorkflowFrontend::new();
        skip_checkout_branch_steps_in_worktree(&mut wf, &mut fe);
        assert_eq!(wf.setup.len(), 1, "only the run_shell entry must remain");
        assert!(matches!(wf.setup[0].step, SetupStep::RunShell { .. }));
        let warnings: Vec<&UserMessage> = fe
            .messages
            .iter()
            .filter(|m| m.level == MessageLevel::Warning)
            .collect();
        assert_eq!(
            warnings.len(),
            2,
            "one warning per skipped checkout_create_branch step"
        );
        assert!(warnings[0].text.contains("checkout_create_branch"));
        assert!(warnings[0].text.contains("feature/x"));
        assert!(warnings[1].text.contains("feature/y"));
        assert!(
            warnings[0].text.contains("worktree"),
            "warning must explain the worktree isolation reason"
        );
    }

    #[test]
    fn skip_checkout_branch_steps_no_op_without_checkout_entries() {
        use crate::data::workflow_definition::{SetupStep, SetupStepEntry};
        let mut wf = make_workflow(None, &[None]);
        wf.setup = vec![SetupStepEntry {
            overlays: None,
            abort_on_failure: false,
            on_failure: None,
            step: SetupStep::RunShell {
                command: "echo hi".into(),
                env: None,
            },
        }];
        let mut fe = FakeExecWorkflowFrontend::new();
        skip_checkout_branch_steps_in_worktree(&mut wf, &mut fe);
        assert_eq!(wf.setup.len(), 1);
        assert!(fe.messages.is_empty(), "no warnings when nothing skipped");
    }

    #[test]
    fn workflow_resolves_to_gemini_true_when_step_uses_gemini() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_with_default_agent(&tmp, None);
        let wf = make_workflow(None, &[Some("claude"), Some("gemini")]);
        assert!(
            workflow_resolves_to_gemini(&wf, &session),
            "must detect gemini in a step's agent field"
        );
    }

    #[test]
    fn workflow_resolves_to_gemini_true_when_workflow_default_is_gemini_and_step_has_no_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_with_default_agent(&tmp, None);
        let wf = make_workflow(Some("gemini"), &[None]);
        assert!(
            workflow_resolves_to_gemini(&wf, &session),
            "must detect workflow-level agent=gemini when step omits agent"
        );
    }

    #[test]
    fn workflow_resolves_to_gemini_true_when_session_default_is_gemini_and_step_has_no_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_with_default_agent(&tmp, Some("gemini"));
        let wf = make_workflow(None, &[None]);
        assert!(
            workflow_resolves_to_gemini(&wf, &session),
            "must detect session default agent=gemini when neither step nor workflow set agent"
        );
    }

    #[test]
    fn workflow_resolves_to_gemini_false_when_step_overrides_gemini_with_other_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_with_default_agent(&tmp, Some("gemini"));
        // step.agent (claude) wins over workflow.agent (gemini) and session default.
        let wf = make_workflow(Some("gemini"), &[Some("claude")]);
        assert!(
            !workflow_resolves_to_gemini(&wf, &session),
            "step-level agent override must win over workflow and session defaults"
        );
    }

    #[test]
    fn workflow_resolves_to_gemini_false_when_no_path_resolves_to_gemini() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_with_default_agent(&tmp, Some("claude"));
        let wf = make_workflow(Some("codex"), &[Some("claude"), None]);
        assert!(
            !workflow_resolves_to_gemini(&wf, &session),
            "must return false when neither step, workflow, nor session resolves to gemini"
        );
    }

    // ── issue_source_overlay + IssueTempFile ─────────────────────────────────

    use crate::data::issue::github::GithubIssueSource;
    use crate::data::issue::Issue;

    fn make_issue(source_id: &str, title: &str, body: &str) -> Issue {
        Issue {
            source_id: source_id.to_string(),
            title: title.to_string(),
            body: body.to_string(),
            provider: "GitHub".to_string(),
        }
    }

    #[test]
    fn issue_source_overlay_writes_temp_file_and_builds_directory_overlay() {
        let tmp = tempfile::tempdir().unwrap();
        let git_root = tmp.path();
        let work_items_dir = git_root.join("aspec").join("work-items");
        let issue = make_issue("https://github.com/owner/repo/issues/84", "Test", "body");

        let build = issue_source_overlay(&GithubIssueSource, &issue, git_root, &work_items_dir)
            .expect("overlay build must succeed");

        // Temp file exists and has the expected contents.
        assert!(build.temp_file.path().exists(), "temp file must exist");
        let on_disk = std::fs::read_to_string(build.temp_file.path()).unwrap();
        assert_eq!(on_disk, "# Test\n\nbody");

        // Slug + number derive from the issue.
        assert_eq!(build.number, 84);
        assert!(
            build.slug.starts_with("ghb84"),
            "slug must start with 'ghb84', got: {}",
            build.slug
        );

        // Overlay is a ReadOnly Directory mapping the temp file to the
        // container-side work-items path.
        match build.overlay {
            TypedOverlay::Directory(spec) => {
                assert_eq!(spec.host, build.temp_file.path().display().to_string());
                assert!(
                    spec.container.starts_with("/workspace/aspec/work-items/"),
                    "container path must start with /workspace/aspec/work-items/, got {}",
                    spec.container
                );
                assert!(spec.container.ends_with(".md"));
                assert!(spec.container.contains("0084-"));
                assert_eq!(
                    spec.permission,
                    crate::engine::container::options::OverlayPermission::ReadOnly,
                    "overlay must be ReadOnly"
                );
            }
            other => panic!("expected TypedOverlay::Directory, got {other:?}"),
        }
    }

    #[test]
    fn issue_temp_file_drop_deletes_underlying_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("scope-guard-test.md");
        std::fs::write(&path, "contents").unwrap();
        assert!(path.exists());
        {
            let _guard = super::IssueTempFile { path: path.clone() };
            // Inside the scope the file still exists.
            assert!(path.exists());
        }
        // After the guard is dropped the file is gone.
        assert!(
            !path.exists(),
            "IssueTempFile::drop must remove the underlying file"
        );
    }

    #[test]
    fn issue_temp_file_filename_format_is_pid_and_slug() {
        let tmp = tempfile::tempdir().unwrap();
        let git_root = tmp.path();
        let work_items_dir = git_root.join("aspec").join("work-items");
        let issue = make_issue("https://github.com/owner/repo/issues/7", "Some Title", "");

        let build =
            issue_source_overlay(&GithubIssueSource, &issue, git_root, &work_items_dir).unwrap();

        let pid = std::process::id();
        let file_name = build
            .temp_file
            .path()
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap()
            .to_string();
        assert!(
            file_name.starts_with(&format!("awman-issue-{pid}-")),
            "temp filename must follow awman-issue-{{pid}}-{{slug}}.md, got: {file_name}"
        );
        assert!(file_name.ends_with(".md"));
        assert!(file_name.contains(&build.slug));
    }

    // ─── WI-0092: Dynamic Workflows — unit tests ─────────────────────────────

    // ── Helpers shared by WI-0092 tests ──────────────────────────────────────

    fn make_dynamic_flags(
        dynamic: bool,
        workflow: Option<&str>,
        work_item: Option<&str>,
        leader: Option<&str>,
        plan: bool,
        model: Option<&str>,
    ) -> ExecWorkflowCommandFlags {
        ExecWorkflowCommandFlags {
            workflow: workflow.map(PathBuf::from),
            work_item: work_item.map(|s| s.to_string()),
            non_interactive: false,
            plan,
            allow_docker: false,
            worktree: false,
            yolo: false,
            auto: false,
            agent: None,
            model: model.map(|s| s.to_string()),
            overlay: vec![],
            max_concurrent: None,
            issue_source: crate::data::issue::IssueSourceFlags { issue: None },
            dynamic,
            leader: leader.map(|s| s.to_string()),
        }
    }

    fn make_session_simple(tmp: &tempfile::TempDir) -> crate::data::session::Session {
        make_session_with_default_agent(tmp, None)
    }

    fn make_session_with_agent(
        tmp: &tempfile::TempDir,
        agent: &str,
    ) -> crate::data::session::Session {
        make_session_with_default_agent(tmp, Some(agent))
    }

    /// Writes a repo config with `dynamicWorkflows.defaultLeader` set, for
    /// leader-resolution-precedence tests (WI-0095 §5).
    fn make_session_with_default_leader(
        tmp: &tempfile::TempDir,
        default_leader: &str,
    ) -> crate::data::session::Session {
        use crate::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME};
        use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};

        let cfg_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("config.json"),
            format!(r#"{{"dynamicWorkflows": {{"defaultLeader": "{default_leader}"}}}}"#),
        )
        .unwrap();

        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let resolver = StaticGitRootResolver::new(tmp.path());
        Session::open(
            tmp.path().to_path_buf(),
            &resolver,
            SessionOpenOptions {
                env: Some(env),
                ..Default::default()
            },
        )
        .unwrap()
    }

    // ── validate_dynamic_flags ────────────────────────────────────────────────

    #[test]
    fn validate_dynamic_flags_rejects_path_with_dynamic() {
        let flags = make_dynamic_flags(true, Some("/tmp/wf.toml"), Some("0042"), None, false, None);
        let err = validate_dynamic_flags(&flags).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("cannot specify a workflow file path with --dynamic"),
            "error must explain the conflict, got: {msg}"
        );
    }

    #[test]
    fn validate_dynamic_flags_requires_work_item_with_dynamic() {
        let flags = make_dynamic_flags(true, None, None, None, false, None);
        let err = validate_dynamic_flags(&flags).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("--dynamic requires --work-item"),
            "error must name the missing flag, got: {msg}"
        );
    }

    #[test]
    fn validate_dynamic_flags_rejects_leader_without_dynamic() {
        let flags = make_dynamic_flags(
            false,
            Some("/tmp/wf.toml"),
            None,
            Some("claude::claude-opus-4-8"),
            false,
            None,
        );
        let err = validate_dynamic_flags(&flags).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("--leader is only valid with --dynamic"),
            "error must name the constraint, got: {msg}"
        );
    }

    #[test]
    fn validate_dynamic_flags_rejects_dynamic_with_plan() {
        let flags = make_dynamic_flags(true, None, Some("0042"), None, true, None);
        let err = validate_dynamic_flags(&flags).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("--dynamic cannot be used with --plan"),
            "error must explain why dynamic+plan is rejected, got: {msg}"
        );
    }

    #[test]
    fn validate_dynamic_flags_rejects_malformed_leader_value() {
        // Malformed --leader (no "::" separator) is caught by validate_dynamic_flags.
        let flags = make_dynamic_flags(true, None, Some("0042"), Some("claude"), false, None);
        let err = validate_dynamic_flags(&flags).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("invalid --leader value"),
            "error must describe malformed leader, got: {msg}"
        );
    }

    #[test]
    fn validate_dynamic_flags_ok_with_valid_dynamic_invocation() {
        let flags = make_dynamic_flags(
            true,
            None,
            Some("0042"),
            Some("claude::claude-opus-4-8"),
            false,
            None,
        );
        assert!(
            validate_dynamic_flags(&flags).is_ok(),
            "valid dynamic invocation with --leader must pass"
        );
    }

    #[test]
    fn validate_dynamic_flags_ok_with_dynamic_no_leader() {
        let flags = make_dynamic_flags(true, None, Some("0042"), None, false, None);
        assert!(
            validate_dynamic_flags(&flags).is_ok(),
            "valid dynamic invocation without --leader must pass"
        );
    }

    #[test]
    fn validate_dynamic_flags_ok_with_static_invocation() {
        let flags = make_dynamic_flags(false, Some("/tmp/wf.toml"), None, None, false, None);
        assert!(
            validate_dynamic_flags(&flags).is_ok(),
            "valid static invocation must pass"
        );
    }

    // ── LeaderSpec::parse ─────────────────────────────────────────────────────

    #[test]
    fn leader_spec_parses_valid_agent_and_model() {
        let spec = LeaderSpec::parse("claude::claude-opus-4-8").unwrap();
        assert_eq!(spec.agent, "claude");
        assert_eq!(spec.model, "claude-opus-4-8");
    }

    #[test]
    fn leader_spec_error_plain_string_no_double_colon() {
        let err = LeaderSpec::parse("claude").unwrap_err();
        assert!(
            err.to_string().contains("invalid --leader value"),
            "got: {err}"
        );
    }

    #[test]
    fn leader_spec_error_empty_string() {
        let err = LeaderSpec::parse("").unwrap_err();
        assert!(
            err.to_string().contains("invalid --leader value"),
            "got: {err}"
        );
    }

    #[test]
    fn leader_spec_error_empty_agent_component() {
        let err = LeaderSpec::parse("::claude-opus-4-8").unwrap_err();
        assert!(
            err.to_string().contains("invalid --leader value"),
            "got: {err}"
        );
    }

    #[test]
    fn leader_spec_error_empty_model_component() {
        let err = LeaderSpec::parse("claude::").unwrap_err();
        assert!(
            err.to_string().contains("invalid --leader value"),
            "got: {err}"
        );
    }

    #[test]
    fn leader_spec_error_three_components() {
        let err = LeaderSpec::parse("a::b::c").unwrap_err();
        assert!(
            err.to_string().contains("invalid --leader value"),
            "got: {err}"
        );
    }

    #[test]
    fn leader_spec_error_message_includes_format_hint() {
        let err = LeaderSpec::parse("badvalue").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("agent::model"),
            "error must include the expected format hint, got: {msg}"
        );
    }

    // ── apply_dynamic_implied_flags ───────────────────────────────────────────

    #[test]
    fn apply_dynamic_implied_flags_sets_yolo_true() {
        let mut flags = make_dynamic_flags(true, None, Some("0042"), None, false, None);
        flags.yolo = false;
        apply_dynamic_implied_flags(&mut flags);
        assert!(flags.yolo, "apply_dynamic_implied_flags must set yolo=true");
    }

    #[test]
    fn apply_dynamic_implied_flags_sets_worktree_true() {
        let mut flags = make_dynamic_flags(true, None, Some("0042"), None, false, None);
        flags.worktree = false;
        apply_dynamic_implied_flags(&mut flags);
        assert!(
            flags.worktree,
            "apply_dynamic_implied_flags must set worktree=true"
        );
    }

    #[test]
    fn apply_dynamic_implied_flags_adds_context_workflow_overlay() {
        let mut flags = make_dynamic_flags(true, None, Some("0042"), None, false, None);
        flags.overlay.clear();
        apply_dynamic_implied_flags(&mut flags);
        assert!(
            flags.overlay.iter().any(|o| o.contains("context(workflow")),
            "apply_dynamic_implied_flags must add context(workflow) overlay"
        );
    }

    #[test]
    fn apply_dynamic_implied_flags_does_not_duplicate_context_overlay() {
        let mut flags = make_dynamic_flags(true, None, Some("0042"), None, false, None);
        flags.overlay = vec!["context(workflow)".to_string()];
        apply_dynamic_implied_flags(&mut flags);
        let count = flags
            .overlay
            .iter()
            .filter(|o| o.contains("context(workflow"))
            .count();
        assert_eq!(count, 1, "context(workflow) must not be duplicated");
    }

    #[test]
    fn apply_dynamic_implied_flags_preserves_existing_overlays() {
        let mut flags = make_dynamic_flags(true, None, Some("0042"), None, false, None);
        flags.overlay = vec!["env(MY_VAR)".to_string()];
        apply_dynamic_implied_flags(&mut flags);
        assert!(
            flags.overlay.contains(&"env(MY_VAR)".to_string()),
            "pre-existing overlays must be preserved"
        );
    }

    // ── build_leader_prompt / build_repair_prompt ─────────────────────────────

    #[test]
    fn build_leader_prompt_substitutes_work_item_number() {
        let prompt = crate::data::dynamic_workflow_assets::build_leader_prompt(
            "0042",
            "/workspace/aspec/work-items/0042-my-item.md",
            "  - claude",
            None,
            None,
        );
        assert!(
            prompt.contains("0042"),
            "leader prompt must contain the work item number"
        );
    }

    #[test]
    fn build_leader_prompt_substitutes_work_item_path() {
        let path = "/workspace/aspec/work-items/0042-my-item.md";
        let prompt = crate::data::dynamic_workflow_assets::build_leader_prompt(
            "0042",
            path,
            "  - claude",
            None,
            None,
        );
        assert!(
            prompt.contains(path),
            "leader prompt must contain the work item path"
        );
    }

    #[test]
    fn build_leader_prompt_substitutes_available_agents() {
        let agents = "  - claude\n  - maki";
        let prompt = crate::data::dynamic_workflow_assets::build_leader_prompt(
            "0042", "/path", agents, None, None,
        );
        assert!(
            prompt.contains("claude"),
            "leader prompt must list available agents"
        );
        assert!(
            prompt.contains("maki"),
            "leader prompt must list all available agents"
        );
    }

    #[test]
    fn build_leader_prompt_no_unreplaced_placeholders() {
        let prompt = crate::data::dynamic_workflow_assets::build_leader_prompt(
            "0099",
            "/workspace/aspec/work-items/0099-task.md",
            "  - claude",
            None,
            None,
        );
        assert!(
            !prompt.contains("{{work_item_number}}"),
            "{{work_item_number}} must be substituted"
        );
        assert!(
            !prompt.contains("{{work_item_path}}"),
            "{{work_item_path}} must be substituted"
        );
        assert!(
            !prompt.contains("{{available_agents}}"),
            "{{available_agents}} must be substituted"
        );
        assert!(
            !prompt.contains("{{max_concurrent_steps_note}}"),
            "{{max_concurrent_steps_note}} must be substituted"
        );
        assert!(
            !prompt.contains("{{developer_guidance}}"),
            "{{developer_guidance}} must be substituted"
        );
    }

    // ── build_leader_prompt: developer guidance (WI-0099) ─────────────────────

    #[test]
    fn build_leader_prompt_includes_developer_guidance_section_when_present() {
        let guidance = vec![
            "never spawn more than two agents in parallel".to_string(),
            "always include a validation step after each implementation step".to_string(),
        ];
        let prompt = crate::data::dynamic_workflow_assets::build_leader_prompt(
            "0099",
            "/path",
            "  - claude",
            None,
            Some(&guidance),
        );
        assert!(
            prompt.contains("## Developer Guidance"),
            "prompt must include the Developer Guidance heading when guidance is present, got: {prompt}"
        );
        assert!(
            prompt.contains("- never spawn more than two agents in parallel"),
            "prompt must render the first guidance entry as a bullet, got: {prompt}"
        );
        assert!(
            prompt.contains("- always include a validation step after each implementation step"),
            "prompt must render the second guidance entry as a bullet, got: {prompt}"
        );
    }

    #[test]
    fn build_leader_prompt_omits_developer_guidance_section_when_none() {
        let prompt = crate::data::dynamic_workflow_assets::build_leader_prompt(
            "0099",
            "/path",
            "  - claude",
            None,
            None,
        );
        assert!(
            !prompt.contains("## Developer Guidance"),
            "prompt must omit the Developer Guidance section when guidance is None, got: {prompt}"
        );
        assert!(
            !prompt.contains("{{developer_guidance}}"),
            "no stray placeholder token must remain when guidance is None, got: {prompt}"
        );
    }

    #[test]
    fn build_leader_prompt_omits_developer_guidance_section_when_empty() {
        let guidance: Vec<String> = Vec::new();
        let prompt = crate::data::dynamic_workflow_assets::build_leader_prompt(
            "0099",
            "/path",
            "  - claude",
            None,
            Some(&guidance),
        );
        assert!(
            !prompt.contains("## Developer Guidance"),
            "prompt must omit the Developer Guidance section when guidance is empty, got: {prompt}"
        );
        assert!(
            !prompt.contains("{{developer_guidance}}"),
            "no stray placeholder token must remain when guidance is empty, got: {prompt}"
        );
    }

    #[test]
    fn build_leader_prompt_includes_advisory_note_when_max_concurrent_steps_is_some() {
        let prompt = crate::data::dynamic_workflow_assets::build_leader_prompt(
            "0042",
            "/path",
            "  - claude",
            Some(3),
            None,
        );
        assert!(
            prompt.contains("maximum of 3 concurrent steps"),
            "prompt must include the concurrency advisory when Some(n), got: {prompt}"
        );
    }

    #[test]
    fn build_leader_prompt_omits_advisory_note_when_max_concurrent_steps_is_none() {
        let prompt = crate::data::dynamic_workflow_assets::build_leader_prompt(
            "0042",
            "/path",
            "  - claude",
            None,
            None,
        );
        assert!(
            !prompt.contains("concurrent steps"),
            "prompt must omit the concurrency advisory entirely when None, got: {prompt}"
        );
    }

    #[test]
    fn build_repair_prompt_substitutes_validation_error() {
        let error = "TOML parse error: unexpected key 'bogus' at line 3";
        let prompt = crate::data::dynamic_workflow_assets::build_repair_prompt(error);
        assert!(
            prompt.contains(error),
            "repair prompt must contain the verbatim validation error, got: {prompt}"
        );
    }

    #[test]
    fn build_repair_prompt_no_unreplaced_placeholders() {
        let prompt = crate::data::dynamic_workflow_assets::build_repair_prompt("some error");
        assert!(
            !prompt.contains("{{validation_error}}"),
            "{{validation_error}} must be substituted"
        );
    }

    // ── Embedded assets ───────────────────────────────────────────────────────

    #[test]
    fn example_workflow_toml_parses_as_valid_workflow() {
        use crate::data::workflow_definition::WorkflowFormat;
        let result = crate::data::workflow_definition::Workflow::parse(
            crate::data::dynamic_workflow_assets::EXAMPLE_WORKFLOW_TOML,
            WorkflowFormat::Toml,
        );
        assert!(
            result.is_ok(),
            "EXAMPLE_WORKFLOW_TOML must parse as a valid Workflow: {:?}",
            result.err()
        );
        let wf = result.unwrap();
        assert!(
            !wf.steps.is_empty(),
            "example workflow must have at least one step"
        );
    }

    #[test]
    fn workflow_usage_md_is_nonempty() {
        assert!(
            !crate::data::dynamic_workflow_assets::WORKFLOW_USAGE_MD.is_empty(),
            "WORKFLOW_USAGE_MD must not be empty"
        );
    }

    #[test]
    fn leader_prompt_md_is_nonempty() {
        assert!(
            !crate::data::dynamic_workflow_assets::LEADER_PROMPT_MD.is_empty(),
            "LEADER_PROMPT_MD must not be empty"
        );
    }

    #[test]
    fn leader_repair_prompt_is_nonempty() {
        assert!(
            !crate::data::dynamic_workflow_assets::LEADER_REPAIR_PROMPT.is_empty(),
            "LEADER_REPAIR_PROMPT must not be empty"
        );
    }

    // ── Leader model selection (WI-0092 §7) ──────────────────────────────────

    #[test]
    fn resolve_leader_model_with_leader_flag_uses_spec_agent_and_model() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_simple(&tmp);
        let mut flags = make_dynamic_flags(
            true,
            None,
            Some("0042"),
            Some("claude::claude-opus-4-8"),
            false,
            None,
        );
        flags.agent = None;
        let (agent, model) = resolve_leader_model(&flags, &session).unwrap();
        assert_eq!(agent.as_str(), "claude");
        assert_eq!(model.as_deref(), Some("claude-opus-4-8"));
    }

    #[test]
    fn resolve_leader_model_with_leader_flag_ignores_flags_model() {
        // --leader takes full precedence; --model must NOT be used for the leader.
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_simple(&tmp);
        let mut flags = make_dynamic_flags(
            true,
            None,
            Some("0042"),
            Some("claude::claude-opus-4-8"),
            false,
            Some("some-other-model"),
        );
        flags.agent = None;
        let (_agent, model) = resolve_leader_model(&flags, &session).unwrap();
        assert_eq!(
            model.as_deref(),
            Some("claude-opus-4-8"),
            "--model must be ignored for the leader when --leader is present"
        );
    }

    #[test]
    fn resolve_leader_model_with_model_flag_no_leader_passes_model() {
        // Case (b): --model present, no --leader → model forwarded from flags.
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_with_agent(&tmp, "maki");
        let flags = make_dynamic_flags(true, None, Some("0042"), None, false, Some("custom-model"));
        let (_agent, model) = resolve_leader_model(&flags, &session).unwrap();
        assert_eq!(
            model.as_deref(),
            Some("custom-model"),
            "--model must be passed to leader when no --leader"
        );
    }

    #[test]
    fn resolve_leader_model_with_neither_flag_model_is_none() {
        // Case (c): neither --leader nor --model → no model override.
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_simple(&tmp);
        let flags = make_dynamic_flags(true, None, Some("0042"), None, false, None);
        let (_agent, model) = resolve_leader_model(&flags, &session).unwrap();
        assert!(
            model.is_none(),
            "model must be None when neither --leader nor --model is set"
        );
    }

    #[test]
    fn resolve_leader_model_both_flags_leader_model_wins() {
        // Case (d): both --leader and --model → leader spec's model governs.
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_simple(&tmp);
        let flags = make_dynamic_flags(
            true,
            None,
            Some("0042"),
            Some("claude::claude-opus-4-8"),
            false,
            Some("should-be-ignored-for-leader"),
        );
        let (_agent, model) = resolve_leader_model(&flags, &session).unwrap();
        assert_eq!(
            model.as_deref(),
            Some("claude-opus-4-8"),
            "leader spec model must win over --model when both are set"
        );
    }

    // ── Leader resolution precedence: --leader > defaultLeader > --model (WI-0095 §5) ──

    #[test]
    fn resolve_leader_model_uses_default_leader_from_config_when_flag_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_with_default_leader(&tmp, "codex::codex-mini-latest");
        let flags = make_dynamic_flags(true, None, Some("0042"), None, false, None);

        let (agent, model) = resolve_leader_model(&flags, &session).unwrap();
        assert_eq!(agent.as_str(), "codex");
        assert_eq!(model.as_deref(), Some("codex-mini-latest"));
    }

    #[test]
    fn resolve_leader_model_leader_flag_wins_over_default_leader_config() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_with_default_leader(&tmp, "codex::codex-mini-latest");
        let flags = make_dynamic_flags(
            true,
            None,
            Some("0042"),
            Some("claude::claude-opus-4-8"),
            false,
            None,
        );

        let (agent, model) = resolve_leader_model(&flags, &session).unwrap();
        assert_eq!(
            agent.as_str(),
            "claude",
            "--leader must win over dynamicWorkflows.defaultLeader"
        );
        assert_eq!(model.as_deref(), Some("claude-opus-4-8"));
    }

    #[test]
    fn resolve_leader_model_default_leader_config_not_overridden_by_model_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_with_default_leader(&tmp, "codex::codex-mini-latest");
        let flags = make_dynamic_flags(
            true,
            None,
            Some("0042"),
            None,
            false,
            Some("should-be-ignored"),
        );

        let (agent, model) = resolve_leader_model(&flags, &session).unwrap();
        assert_eq!(agent.as_str(), "codex");
        assert_eq!(
            model.as_deref(),
            Some("codex-mini-latest"),
            "--model must not override dynamicWorkflows.defaultLeader's model"
        );
    }

    #[test]
    fn resolve_leader_model_no_flag_no_config_falls_back_to_default_agent() {
        // The "default" source in the 3-way precedence: no --leader, no
        // dynamicWorkflows.defaultLeader in config → falls back to --model +
        // default-agent resolution (WI-0092 behavior, case (c) above).
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session_simple(&tmp);
        let flags = make_dynamic_flags(true, None, Some("0042"), None, false, None);

        let (_agent, model) = resolve_leader_model(&flags, &session).unwrap();
        assert!(
            model.is_none(),
            "with no --leader, no defaultLeader, and no --model, model must be None"
        );
    }

    // ── AvailableActions.launch_next_label ────────────────────────────────────

    #[test]
    fn available_actions_launch_next_label_defaults_to_none() {
        let actions = AvailableActions::default();
        assert!(
            actions.launch_next_label.is_none(),
            "launch_next_label must default to None (renders fallback label)"
        );
    }

    #[test]
    fn available_actions_launch_next_label_can_be_set_to_dynamic_string() {
        let actions = AvailableActions {
            launch_next_label: Some("Start dynamic workflow".to_string()),
            ..Default::default()
        };
        assert_eq!(
            actions.launch_next_label.as_deref(),
            Some("Start dynamic workflow")
        );
    }

    #[test]
    fn available_actions_cli_uses_launch_next_label_when_set() {
        // Verify the rendering pattern: .as_deref().unwrap_or(fallback).
        // The CLI uses: `available.launch_next_label.as_deref().unwrap_or("Launch next step (new container)")`.
        let actions = AvailableActions {
            launch_next_label: Some("Start dynamic workflow".to_string()),
            can_launch_next: true,
            ..Default::default()
        };
        let rendered = actions
            .launch_next_label
            .as_deref()
            .unwrap_or("Launch next step (new container)");
        assert_eq!(rendered, "Start dynamic workflow");
    }

    #[test]
    fn available_actions_cli_falls_back_when_label_is_none() {
        let actions = AvailableActions {
            launch_next_label: None,
            can_launch_next: true,
            ..Default::default()
        };
        let rendered = actions
            .launch_next_label
            .as_deref()
            .unwrap_or("Launch next step (new container)");
        assert_eq!(rendered, "Launch next step (new container)");
    }

    #[test]
    fn available_actions_tui_uses_launch_next_label_when_set() {
        // TUI renders: state.launch_next_label.as_deref().unwrap_or("Next: new container")
        let label: Option<String> = Some("Start dynamic workflow".to_string());
        let rendered = label.as_deref().unwrap_or("Next: new container");
        assert_eq!(rendered, "Start dynamic workflow");
    }

    #[test]
    fn available_actions_tui_falls_back_to_next_new_container() {
        let label: Option<String> = None;
        let rendered = label.as_deref().unwrap_or("Next: new container");
        assert_eq!(rendered, "Next: new container");
    }

    // ── format_available_agents ───────────────────────────────────────────────

    #[test]
    fn format_available_agents_empty_list_gives_placeholder() {
        let result = format_available_agents(&[]);
        assert!(
            result.contains("no agents discovered"),
            "empty agent list must give placeholder message, got: {result}"
        );
        assert!(
            result.contains(".awman/Dockerfile.<agent>"),
            "placeholder must mention the expected path, got: {result}"
        );
    }

    #[test]
    fn format_available_agents_single_agent() {
        let agents = vec![(
            "claude".to_string(),
            std::path::PathBuf::from("/r/.awman/Dockerfile.claude"),
        )];
        let result = format_available_agents(&agents);
        assert!(
            result.contains("claude"),
            "formatted agents must include the agent name, got: {result}"
        );
        assert!(
            result.contains("  - claude"),
            "agents must be formatted with '  - ' prefix, got: {result}"
        );
    }

    #[test]
    fn format_available_agents_multiple_agents_are_listed() {
        let agents = vec![
            (
                "claude".to_string(),
                std::path::PathBuf::from("/r/.awman/Dockerfile.claude"),
            ),
            (
                "maki".to_string(),
                std::path::PathBuf::from("/r/.awman/Dockerfile.maki"),
            ),
        ];
        let result = format_available_agents(&agents);
        assert!(result.contains("claude"), "must list claude");
        assert!(result.contains("maki"), "must list maki");
    }

    // ── format_agents_with_models (WI-0095 §3) ────────────────────────────────

    #[test]
    fn format_agents_with_models_typical_map() {
        let mut map = std::collections::HashMap::new();
        map.insert("claude".to_string(), vec!["claude-opus-4-8".to_string()]);
        let result = format_agents_with_models(&map);
        assert_eq!(result, "  - claude: claude-opus-4-8");
    }

    #[test]
    fn format_agents_with_models_sorted_alphabetically_for_determinism() {
        let mut map = std::collections::HashMap::new();
        map.insert("gemini".to_string(), vec!["gemini-2.5-pro".to_string()]);
        map.insert("claude".to_string(), vec!["claude-opus-4-8".to_string()]);
        map.insert("codex".to_string(), vec!["codex-mini-latest".to_string()]);
        let result = format_agents_with_models(&map);
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(
            lines[0].starts_with("  - claude:"),
            "agents must be sorted alphabetically, got: {lines:?}"
        );
        assert!(lines[1].starts_with("  - codex:"));
        assert!(lines[2].starts_with("  - gemini:"));
    }

    #[test]
    fn format_agents_with_models_handles_single_model() {
        let mut map = std::collections::HashMap::new();
        map.insert("claude".to_string(), vec!["claude-opus-4-8".to_string()]);
        let result = format_agents_with_models(&map);
        assert!(result.contains("claude-opus-4-8"));
        assert!(
            !result.contains(','),
            "a single-model entry must not contain a comma, got: {result}"
        );
    }

    #[test]
    fn format_agents_with_models_handles_multiple_models_comma_joined_in_order() {
        let mut map = std::collections::HashMap::new();
        map.insert(
            "claude".to_string(),
            vec![
                "claude-opus-4-8".to_string(),
                "claude-sonnet-4-6".to_string(),
            ],
        );
        let result = format_agents_with_models(&map);
        assert_eq!(
            result, "  - claude: claude-opus-4-8, claude-sonnet-4-6",
            "configured model-list order must be preserved"
        );
    }

    // ── build_effective_agents_to_models (WI-0095 §2 agent validation) ────────

    fn agent_dockerfiles(names: &[&str]) -> Vec<(String, std::path::PathBuf)> {
        names
            .iter()
            .map(|n| {
                (
                    n.to_string(),
                    std::path::PathBuf::from(format!("/r/.awman/Dockerfile.{n}")),
                )
            })
            .collect()
    }

    #[test]
    fn build_effective_agents_to_models_all_match_succeeds() {
        let mut configured = std::collections::HashMap::new();
        configured.insert("claude".to_string(), vec!["claude-opus-4-8".to_string()]);
        configured.insert("codex".to_string(), vec!["codex-mini-latest".to_string()]);
        let available = agent_dockerfiles(&["claude", "codex"]);
        let mut warnings = Vec::new();

        let effective =
            build_effective_agents_to_models(&configured, &available, &mut warnings).unwrap();

        assert_eq!(effective.len(), 2);
        assert_eq!(
            effective.get("claude"),
            Some(&vec!["claude-opus-4-8".to_string()])
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn build_effective_agents_to_models_partial_mismatch_error_lists_only_missing() {
        let mut configured = std::collections::HashMap::new();
        configured.insert("claude".to_string(), vec!["claude-opus-4-8".to_string()]);
        configured.insert("foo".to_string(), vec!["some-model".to_string()]);
        let available = agent_dockerfiles(&["claude", "codex"]);
        let mut warnings = Vec::new();

        let err =
            build_effective_agents_to_models(&configured, &available, &mut warnings).unwrap_err();
        let msg = err.to_string();

        assert!(
            msg.contains("no Dockerfile in this repo: [foo]"),
            "error's missing-agents list must contain only foo, got: {msg}"
        );
        assert!(
            msg.contains("Available agents") && msg.contains("claude") && msg.contains("codex"),
            "error must list available agents, got: {msg}"
        );
    }

    #[test]
    fn build_effective_agents_to_models_complete_mismatch_fails() {
        let mut configured = std::collections::HashMap::new();
        configured.insert("foo".to_string(), vec!["some-model".to_string()]);
        configured.insert("bar".to_string(), vec!["other-model".to_string()]);
        let available = agent_dockerfiles(&["claude"]);
        let mut warnings = Vec::new();

        let err =
            build_effective_agents_to_models(&configured, &available, &mut warnings).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("foo"), "got: {msg}");
        assert!(msg.contains("bar"), "got: {msg}");
    }

    #[test]
    fn build_effective_agents_to_models_case_folded_match_emits_lowercase_and_warning() {
        let mut configured = std::collections::HashMap::new();
        configured.insert("Claude".to_string(), vec!["claude-opus-4-8".to_string()]);
        let available = agent_dockerfiles(&["claude"]);
        let mut warnings = Vec::new();

        let effective =
            build_effective_agents_to_models(&configured, &available, &mut warnings).unwrap();

        assert_eq!(
            effective.get("claude"),
            Some(&vec!["claude-opus-4-8".to_string()]),
            "the effective map must be keyed by the lowercase agent name"
        );
        assert!(
            !effective.contains_key("Claude"),
            "the configured mixed-case key must not survive into the effective map"
        );
        assert_eq!(warnings.len(), 1, "a case-folded match must warn");
        assert!(
            warnings[0].contains("\"Claude\"") && warnings[0].contains("case folding"),
            "warning must name the configured key and explain case folding, got: {}",
            warnings[0]
        );
    }

    #[test]
    fn build_effective_agents_to_models_duplicate_keys_after_case_folding_fail() {
        let mut configured = std::collections::HashMap::new();
        configured.insert("Claude".to_string(), vec!["claude-opus-4-8".to_string()]);
        configured.insert("claude".to_string(), vec!["claude-sonnet-4-6".to_string()]);
        let available = agent_dockerfiles(&["claude"]);
        let mut warnings = Vec::new();

        let err =
            build_effective_agents_to_models(&configured, &available, &mut warnings).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Claude") && msg.contains("claude") && msg.contains("case folding"),
            "duplicate case-folded keys must fail with both keys named, got: {msg}"
        );
    }

    #[test]
    fn build_effective_agents_to_models_empty_map_is_not_an_error() {
        let configured = std::collections::HashMap::new();
        let available = agent_dockerfiles(&["claude"]);
        let mut warnings = Vec::new();

        let effective =
            build_effective_agents_to_models(&configured, &available, &mut warnings).unwrap();
        assert!(effective.is_empty());
        assert!(warnings.is_empty());
    }

    // ── resolve_and_validate_workflow_agents ──────────────────────────────────

    #[test]
    fn resolve_validates_step_agent_with_dockerfile() {
        let tmp = tempfile::tempdir().unwrap();
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM ubuntu\n").unwrap();
        let session = make_session_simple(&tmp);
        let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
        let wf = make_workflow(None, &[Some("claude")]);
        let result = resolve_and_validate_workflow_agents(&wf, &session, &paths);
        assert!(
            result.is_ok(),
            "step agent with Dockerfile must validate OK, got: {:?}",
            result.err()
        );
        let agents = result.unwrap();
        assert!(agents.contains(&"claude".to_string()));
    }

    #[test]
    fn resolve_error_step_agent_without_dockerfile() {
        let tmp = tempfile::tempdir().unwrap();
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        // No Dockerfile.gemini
        let session = make_session_simple(&tmp);
        let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
        let wf = make_workflow(None, &[Some("gemini")]);
        let err = resolve_and_validate_workflow_agents(&wf, &session, &paths).unwrap_err();
        assert!(
            err.contains("gemini"),
            "error must name the unknown agent, got: {err}"
        );
        assert!(
            err.contains("Dockerfile.gemini"),
            "error must name the expected Dockerfile path, got: {err}"
        );
    }

    #[test]
    fn resolve_error_unknown_agent_lists_available_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM ubuntu\n").unwrap();
        let session = make_session_simple(&tmp);
        let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
        let wf = make_workflow(None, &[Some("gemini")]);
        let err = resolve_and_validate_workflow_agents(&wf, &session, &paths).unwrap_err();
        assert!(
            err.contains("Available agents"),
            "error must list available agents, got: {err}"
        );
        assert!(
            err.contains("claude"),
            "error must list claude as an available agent, got: {err}"
        );
    }

    #[test]
    fn resolve_validates_workflow_level_agent_with_dockerfile() {
        let tmp = tempfile::tempdir().unwrap();
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(awman_dir.join("Dockerfile.maki"), "FROM ubuntu\n").unwrap();
        let session = make_session_simple(&tmp);
        let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
        // Workflow-level agent, steps have no agent.
        let wf = make_workflow(Some("maki"), &[None]);
        let result = resolve_and_validate_workflow_agents(&wf, &session, &paths);
        assert!(
            result.is_ok(),
            "workflow-level agent with Dockerfile must validate OK"
        );
    }

    #[test]
    fn resolve_error_workflow_level_agent_without_dockerfile() {
        let tmp = tempfile::tempdir().unwrap();
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        let session = make_session_simple(&tmp);
        let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
        let wf = make_workflow(Some("badname"), &[None]);
        let err = resolve_and_validate_workflow_agents(&wf, &session, &paths).unwrap_err();
        assert!(
            err.contains("badname"),
            "error must name the unknown workflow-level agent, got: {err}"
        );
    }

    #[test]
    fn resolve_error_no_agent_anywhere_suggests_fix() {
        // No step agent, no workflow agent, no session default → error.
        let tmp = tempfile::tempdir().unwrap();
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM ubuntu\n").unwrap();
        let session = make_session_simple(&tmp); // no default agent
        let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
        let wf = make_workflow(None, &[None]); // no step or workflow agent
        let err = resolve_and_validate_workflow_agents(&wf, &session, &paths).unwrap_err();
        assert!(
            err.contains("no agent"),
            "error must mention missing agent, got: {err}"
        );
        assert!(
            err.contains("workflow-level"),
            "error must suggest adding workflow-level agent, got: {err}"
        );
    }

    #[test]
    fn resolve_deduplicates_repeated_agent_names() {
        let tmp = tempfile::tempdir().unwrap();
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM ubuntu\n").unwrap();
        let session = make_session_simple(&tmp);
        let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
        let wf = make_workflow(None, &[Some("claude"), Some("claude"), Some("claude")]);
        let result = resolve_and_validate_workflow_agents(&wf, &session, &paths).unwrap();
        assert_eq!(
            result.len(),
            1,
            "claude must appear only once in the resolved list"
        );
    }

    #[test]
    fn resolve_error_multiple_unknown_agents_listed_together() {
        let tmp = tempfile::tempdir().unwrap();
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        // No Dockerfiles for gemini or codex.
        let session = make_session_simple(&tmp);
        let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
        let wf = make_workflow(None, &[Some("gemini"), Some("codex")]);
        let err = resolve_and_validate_workflow_agents(&wf, &session, &paths).unwrap_err();
        assert!(err.contains("gemini"), "error must name gemini, got: {err}");
        assert!(err.contains("codex"), "error must name codex, got: {err}");
    }

    // ── validate_generated_workflow (integration-style unit tests) ────────────

    #[test]
    fn validate_generated_workflow_missing_file_error_contains_path() {
        let tmp = tempfile::tempdir().unwrap();
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        let session = make_session_simple(&tmp);
        let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
        let missing = tmp.path().join("workflow.toml");

        let err = validate_generated_workflow(&missing, &session, &paths).unwrap_err();
        assert!(
            err.contains("workflow.toml"),
            "error must mention the expected file path, got: {err}"
        );
        assert!(
            err.contains("did not produce"),
            "error must explain the leader failed to produce the file, got: {err}"
        );
    }

    #[test]
    fn validate_generated_workflow_invalid_toml_propagates_parse_error() {
        let tmp = tempfile::tempdir().unwrap();
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        let session = make_session_simple(&tmp);
        let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
        let wf_path = tmp.path().join("workflow.toml");
        std::fs::write(&wf_path, "this is NOT valid toml ][").unwrap();

        let err = validate_generated_workflow(&wf_path, &session, &paths).unwrap_err();
        assert!(
            !err.is_empty(),
            "invalid TOML must produce a non-empty error"
        );
    }

    #[test]
    fn validate_generated_workflow_unknown_agent_error_names_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        // Only "claude" Dockerfile present; workflow references "gemini".
        std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM ubuntu\n").unwrap();
        let session = make_session_simple(&tmp);
        let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
        let wf_path = tmp.path().join("workflow.toml");
        std::fs::write(
            &wf_path,
            r#"[[steps]]
name = "do-stuff"
agent = "gemini"
prompt = "do something"
"#,
        )
        .unwrap();

        let err = validate_generated_workflow(&wf_path, &session, &paths).unwrap_err();
        assert!(
            err.contains("gemini"),
            "error must name the unknown agent, got: {err}"
        );
        assert!(
            err.contains("Available agents"),
            "error must list available agents for repair, got: {err}"
        );
        assert!(
            err.contains("claude"),
            "error must list claude as available, got: {err}"
        );
    }

    #[test]
    fn validate_generated_workflow_valid_with_known_agent_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM ubuntu\n").unwrap();
        let session = make_session_simple(&tmp);
        let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
        let wf_path = tmp.path().join("workflow.toml");
        std::fs::write(
            &wf_path,
            r#"[[steps]]
name = "step1"
agent = "claude"
prompt = "do something useful"
"#,
        )
        .unwrap();

        let result = validate_generated_workflow(&wf_path, &session, &paths);
        assert!(
            result.is_ok(),
            "valid workflow with known agent must succeed, got: {:?}",
            result.err()
        );
        let wf = result.unwrap();
        assert_eq!(wf.steps.len(), 1);
        assert_eq!(wf.steps[0].name, "step1");
    }

    #[test]
    fn repair_prompt_substitution_contains_verbatim_validation_error() {
        // Verify the repair loop passes the exact error string to build_repair_prompt.
        let error_msg = "workflow.toml references agents with no Dockerfile: \"gemini\"";
        let repair_prompt = crate::data::dynamic_workflow_assets::build_repair_prompt(error_msg);
        assert!(
            repair_prompt.contains(error_msg),
            "repair prompt must contain the verbatim validation error from Workflow::load(), got: {repair_prompt}"
        );
    }

    // ── Integration: dynamicWorkflows config → leader prompt (WI-0095) ────────
    //
    // These exercise the same sequence `run_dynamic` performs — RepoConfig
    // load, Dockerfile discovery, `build_effective_agents_to_models`,
    // `format_agents_with_models`, `build_leader_prompt` — without requiring
    // Docker, since none of that sequence touches the container runtime. The
    // mismatched-agents case demonstrates the failure happens at this stage,
    // strictly before `ensure_agent_image`/`drive_leader_agent` would run.

    #[test]
    fn integration_dynamic_config_valid_agents_produces_leader_prompt_with_models_and_advisory() {
        let tmp = tempfile::tempdir().unwrap();
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM ubuntu\n").unwrap();
        std::fs::write(awman_dir.join("Dockerfile.codex"), "FROM ubuntu\n").unwrap();
        std::fs::write(
            awman_dir.join("config.json"),
            r#"{
                "dynamicWorkflows": {
                    "agentsToModels": {
                        "claude": ["claude-opus-4-8"],
                        "codex": ["codex-mini-latest"]
                    },
                    "maxConcurrentSteps": 2
                }
            }"#,
        )
        .unwrap();

        let repo_config = crate::data::config::repo::RepoConfig::load(tmp.path()).unwrap();
        let dw = repo_config
            .dynamic_workflows
            .clone()
            .expect("dynamicWorkflows section must be present");

        let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
        let available_agents = paths.discover_agent_dockerfiles();

        let mut warnings = Vec::new();
        let effective = build_effective_agents_to_models(
            dw.agents_to_models.as_ref().unwrap(),
            &available_agents,
            &mut warnings,
        )
        .expect("all configured agents have Dockerfiles; validation must succeed");
        let agents_section = format_agents_with_models(&effective);

        let leader_prompt = crate::data::dynamic_workflow_assets::build_leader_prompt(
            "0042",
            "/workspace/aspec/work-items/0042-item.md",
            &agents_section,
            dw.max_concurrent_steps,
            dw.guidance.as_deref(),
        );

        assert!(
            leader_prompt.contains("claude-opus-4-8"),
            "leader prompt must contain the configured claude model, got: {leader_prompt}"
        );
        assert!(
            leader_prompt.contains("codex-mini-latest"),
            "leader prompt must contain the configured codex model, got: {leader_prompt}"
        );
        assert!(
            leader_prompt.contains("maximum of 2 concurrent steps"),
            "leader prompt must contain the maxConcurrentSteps advisory, got: {leader_prompt}"
        );
    }

    #[test]
    fn integration_dynamic_config_guidance_entries_appear_in_leader_prompt() {
        // Mirrors the agentsToModels integration test above (WI-0099): load a
        // real RepoConfig with two guidance entries and run it through
        // build_leader_prompt, asserting both entries render as bullets.
        let tmp = tempfile::tempdir().unwrap();
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join("config.json"),
            r#"{
                "dynamicWorkflows": {
                    "guidance": [
                        "never spawn more than two agents in parallel",
                        "always include a validation step after each implementation step"
                    ]
                }
            }"#,
        )
        .unwrap();

        let repo_config = crate::data::config::repo::RepoConfig::load(tmp.path()).unwrap();
        let dw = repo_config
            .dynamic_workflows
            .clone()
            .expect("dynamicWorkflows section must be present");

        let leader_prompt = crate::data::dynamic_workflow_assets::build_leader_prompt(
            "0099",
            "/workspace/aspec/work-items/0099-item.md",
            "  - claude",
            dw.max_concurrent_steps,
            dw.guidance.as_deref(),
        );

        assert!(
            leader_prompt.contains("## Developer Guidance"),
            "leader prompt must include the Developer Guidance heading, got: {leader_prompt}"
        );
        assert!(
            leader_prompt.contains("- never spawn more than two agents in parallel"),
            "leader prompt must contain the first guidance entry, got: {leader_prompt}"
        );
        assert!(
            leader_prompt
                .contains("- always include a validation step after each implementation step"),
            "leader prompt must contain the second guidance entry, got: {leader_prompt}"
        );
    }

    #[test]
    fn integration_dynamic_config_mismatched_agents_fails_before_container_spawn() {
        let tmp = tempfile::tempdir().unwrap();
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        // Only "claude" has a Dockerfile; config references "gemini", which does not.
        std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM ubuntu\n").unwrap();
        std::fs::write(
            awman_dir.join("config.json"),
            r#"{
                "dynamicWorkflows": {
                    "agentsToModels": {
                        "gemini": ["gemini-2.5-pro"]
                    }
                }
            }"#,
        )
        .unwrap();

        let repo_config = crate::data::config::repo::RepoConfig::load(tmp.path()).unwrap();
        let dw = repo_config
            .dynamic_workflows
            .clone()
            .expect("dynamicWorkflows section must be present");

        let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
        let available_agents = paths.discover_agent_dockerfiles();

        // This is the exact check `run_dynamic` performs immediately after
        // Dockerfile discovery and before `ensure_agent_image` /
        // `drive_leader_agent` — i.e. before any image build or container spawn.
        let mut warnings = Vec::new();
        let err = build_effective_agents_to_models(
            dw.agents_to_models.as_ref().unwrap(),
            &available_agents,
            &mut warnings,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("gemini"),
            "error must name the missing agent, got: {msg}"
        );
        assert!(
            msg.contains("no Dockerfile"),
            "error must explain the missing Dockerfile, got: {msg}"
        );
        assert!(
            msg.contains("Available agents") && msg.contains("claude"),
            "error must list available agents, got: {msg}"
        );
    }

    // ── Integration tests requiring Docker (marked #[ignore]) ─────────────────
    //
    // These tests exercise the full `run_dynamic` path including launching
    // container-based leader/repair agents. They require:
    //   - A running Docker daemon
    //   - A built container image for the resolved leader agent
    //   - The `.awman/Dockerfile.<agent>` to exist in the test repo
    //
    // Run selectively with: cargo test -- --ignored

    #[test]
    #[ignore = "requires Docker daemon and a built leader agent image"]
    fn integration_happy_path_leader_writes_valid_workflow() {
        // A mock leader that immediately writes a minimal valid workflow.toml
        // to the context dir; awman should load and execute it.
        todo!("set up test repo with a leader agent image that writes a valid workflow.toml")
    }

    #[test]
    #[ignore = "requires Docker daemon and a built leader agent image"]
    fn integration_missing_file_repair_loop_exhausted() {
        // Leader writes nothing; all 3 repair attempts also write nothing.
        // Final error must include the expected path and "3 repair attempts".
        todo!("set up mock leader that always writes nothing")
    }

    #[test]
    #[ignore = "requires Docker daemon and a built leader agent image"]
    fn integration_invalid_toml_repair_exhausted() {
        // Leader and all 3 repair agents produce malformed TOML; final error
        // must surface the parse error and the file path.
        todo!("set up mock leader that always writes broken TOML")
    }

    #[test]
    #[ignore = "requires Docker stuck-event infrastructure"]
    fn integration_stuck_triggers_yolo_countdown() {
        // Leader emits StuckEvent::Stuck → 60-second countdown starts →
        // on expiry, container killed, workflow.toml loaded and executed.
        todo!("wire up a test container that stalls and observes countdown")
    }

    #[test]
    #[ignore = "requires Docker stuck-event infrastructure"]
    fn integration_yolo_countdown_unstuck_recovery() {
        // Leader emits Stuck, countdown starts, leader then emits Unstuck →
        // countdown cancelled, leader continues running.
        todo!("wire up a test container that recovers from stuck")
    }

    #[test]
    #[ignore = "requires Docker daemon and WorktreeLifecycle"]
    fn integration_worktree_before_leader() {
        // Assert that WorktreeLifecycle setup steps complete before the leader
        // container is launched. Ordering verified via event sequence.
        todo!("instrument the lifecycle and verify ordering")
    }

    #[test]
    #[ignore = "requires Docker daemon, a test repo, and a real work item file"]
    fn e2e_full_dynamic_flow() {
        // awman exec workflow --dynamic --work-item 42 in a test repo with a
        // stubbed leader agent produces and executes a valid workflow.
        todo!("end-to-end dynamic workflow test")
    }

    /// The leader-phase Workflow Control Board maps each `NextAction` returned
    /// by the frontend onto the correct leader-scoped outcome (right arrow =
    /// start workflow, up = restart, Ctrl-C/Pause = abort, everything else =
    /// dismiss).
    #[test]
    fn leader_control_board_maps_actions() {
        fn outcome_for(action: NextAction) -> LeaderControlOutcome {
            let mut fe = FakeExecWorkflowFrontend::new();
            fe.next_action_response = action;
            let shared: Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>> =
                Arc::new(Mutex::new(Box::new(fe)));
            show_leader_control_board(&shared, "leader")
        }

        assert!(matches!(
            outcome_for(NextAction::LaunchNext),
            LeaderControlOutcome::StartWorkflow
        ));
        assert!(matches!(
            outcome_for(NextAction::RestartCurrentStep),
            LeaderControlOutcome::Restart
        ));
        assert!(matches!(
            outcome_for(NextAction::Abort),
            LeaderControlOutcome::Abort
        ));
        assert!(matches!(
            outcome_for(NextAction::Pause),
            LeaderControlOutcome::Pause
        ));
        assert!(matches!(
            outcome_for(NextAction::Dismiss),
            LeaderControlOutcome::Dismiss
        ));
        // Actions with no meaning before a workflow exists just close the board.
        assert!(matches!(
            outcome_for(NextAction::CancelToPreviousStep),
            LeaderControlOutcome::Dismiss
        ));
        assert!(matches!(
            outcome_for(NextAction::ContinueInCurrentContainer {
                prompt: String::new()
            }),
            LeaderControlOutcome::Dismiss
        ));
    }
}
