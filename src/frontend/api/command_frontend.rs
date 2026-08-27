//! `ApiDispatchFrontend` — the single Layer 3 struct that implements
//! every per-command frontend trait for API HTTP command dispatch.
//!
//! When a `POST /v1/commands` request arrives, the route handler constructs
//! a `ApiDispatchFrontend` pre-loaded with the parsed args/flags from
//! the HTTP request body, then hands it to `Dispatch::run_command`. All
//! output (UserMessages, container stdout/stderr) is written to the
//! command's `output.log` file on disk. SSE clients tailing the log see
//! new lines in real time.
//!
//! All interactive Q&A methods return safe non-interactive defaults (the
//! same defaults the CLI uses when stdin is not a TTY).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::data::execution_event::EventPayload;
use crate::frontend::api::event_bus::EventBusSender;

use async_trait::async_trait;

use crate::command::commands::agent_auth::{AgentAuthDecision, AgentAuthFrontend};
use crate::command::commands::agent_setup::{
    AgentSetupDecision, AgentSetupFrontend, HasAgentFrontend,
};
use crate::command::commands::api_server::ApiServeConfig;
use crate::command::commands::api_server::ApiServerCommandFrontend;
use crate::command::commands::auth::AuthCommandFrontend;
use crate::command::commands::chat::ChatCommandFrontend;
use crate::command::commands::config::{ConfigCommandFrontend, ConfigEditRequest, ConfigFieldRow};
use crate::command::commands::download::DownloadCommandFrontend;
use crate::command::commands::exec_prompt::ExecPromptCommandFrontend;
use crate::command::commands::exec_workflow::{ExecWorkflowCommandFrontend, WorkflowSummary};
use crate::command::commands::mount_scope::{MountScopeDecision, MountScopeFrontend};
use crate::command::commands::new::NewCommandFrontend;
use crate::command::commands::remote::RemoteCommandFrontend;
use crate::command::commands::specs::SpecsCommandFrontend;
use crate::command::commands::status::StatusCommandFrontend;
use crate::command::commands::worktree_lifecycle::{
    ExistingWorktreeDecision, PostWorkflowWorktreeAction, PreWorktreeDecision,
    WorktreeLifecycleFrontend, WorktreeMergeMode,
};
use crate::command::dispatch::catalogue::{CommandCatalogue, FrontendKind};
use crate::command::dispatch::projections::raw_args::ParsedArgs;
use crate::command::dispatch::CommandFrontend;
use crate::command::error::CommandError;
use crate::data::config::repo::WorkItemsConfig;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::data::session::AgentName;
use crate::data::workflow_definition::WorkflowStep;
use crate::engine::agent_runtime::execution::AgentExitInfo;
use crate::engine::agent_runtime::frontend::{AgentFrontend, AgentProgress, AgentStatus};
use crate::engine::error::EngineError;
use crate::engine::init::frontend::InitFrontend;
use crate::engine::init::phase::InitPhase;
use crate::engine::init::summary::InitSummary;
use crate::engine::ready::frontend::ReadyFrontend;
use crate::engine::ready::phase::ReadyPhase;
use crate::engine::ready::summary::ReadySummary;
use crate::engine::step_status::StepStatus;
use crate::engine::workflow::actions::{
    AvailableActions, NextAction, ResumeMismatch, StepFailureChoice, StepOutput, WorkflowOutcome,
    WorkflowStepStatus, YoloTickOutcome,
};
use crate::engine::workflow::frontend::WorkflowFrontend;

/// The API dispatch frontend. Emits typed events to an `EventBusSender`
/// for distribution to logfile writers and SSE clients.
pub struct ApiDispatchFrontend {
    /// Typed flags/arguments parsed by Layer 2 (`CommandCatalogue::parse_raw_args`).
    /// Empty when parsing failed; the error is surfaced via `parse_error`.
    parsed: ParsedArgs,
    /// A structured parse error captured at construction time, if any. Every
    /// `CommandFrontend` accessor returns it so Dispatch aborts the command
    /// rather than running with silently-dropped flags. `None` on success.
    parse_error: Option<CommandError>,
    event_bus: EventBusSender,
    line_buffer_stdout: String,
    line_buffer_stderr: String,
    /// Map of step name → 0-based index, populated lazily on the first
    /// `report_step_status` call for each unique step. Used to emit
    /// `WorkflowStepTransition.step_index` accurately.
    step_indices: std::sync::Mutex<HashMap<String, usize>>,
    /// Set to `true` after the first `WorkflowPhaseTransition` event is
    /// emitted. Prevents duplicate phase events for the same workflow run.
    phase_emitted: std::sync::Mutex<bool>,
    /// Latched once `Done` has been emitted, so both `emit_done` and `Drop`
    /// stay idempotent.
    done_emitted: std::sync::atomic::AtomicBool,
    /// Throttle: last time a yolo countdown status message was emitted.
    last_sink_message_time: Option<std::time::Instant>,
}

#[async_trait::async_trait]
impl crate::command::commands::amie::commands::AmieCommandFrontend for ApiDispatchFrontend {}

impl ApiDispatchFrontend {
    /// Construct a new frontend from the HTTP request's subcommand + args.
    ///
    /// `event_bus` is the sender handle for emitting execution events.
    /// `subcommand` is the command path (e.g. "exec prompt" → ["exec", "prompt"]).
    /// `args` is the raw args vector from the HTTP request body.
    ///
    /// Parsing is delegated wholesale to Layer 2
    /// ([`CommandCatalogue::parse_raw_args_with_profile`]): the API frontend
    /// hands the raw HTTP strings straight to the catalogue and keeps no
    /// parsing, type-coercion, or flag-default policy of its own (work item
    /// 0097, Findings A + D). The `Api` frontend profile is what forces
    /// `non-interactive=true` and defaults `yolo=true`.
    pub fn new(subcommand: &str, args: &[String], event_bus: EventBusSender) -> Self {
        let path: Vec<&str> = subcommand.split_whitespace().collect();
        let (parsed, parse_error) = match CommandCatalogue::get().parse_raw_args_with_profile(
            &path,
            args,
            FrontendKind::Api,
        ) {
            Ok(parsed) => (parsed, None),
            Err(e) => (ParsedArgs::default(), Some(e)),
        };

        Self {
            parsed,
            parse_error,
            event_bus,
            line_buffer_stdout: String::new(),
            line_buffer_stderr: String::new(),
            step_indices: std::sync::Mutex::new(HashMap::new()),
            phase_emitted: std::sync::Mutex::new(false),
            done_emitted: std::sync::atomic::AtomicBool::new(false),
            last_sink_message_time: None,
        }
    }

    /// Look up (or assign on first sight) the 0-based step index for a step
    /// name. The first time a given step name is reported, it gets the next
    /// available index; subsequent reports return the same index.
    fn step_index_for(&self, name: &str) -> usize {
        let mut map = self
            .step_indices
            .lock()
            .expect("step_indices lock poisoned");
        if let Some(idx) = map.get(name) {
            return *idx;
        }
        let idx = map.len();
        map.insert(name.to_string(), idx);
        idx
    }

    /// Flush any remaining partial lines in the stdout/stderr buffers.
    pub fn flush_line_buffers(&mut self) {
        if !self.line_buffer_stdout.is_empty() {
            let line = std::mem::take(&mut self.line_buffer_stdout);
            self.event_bus.emit(EventPayload::StdoutLine(line));
        }
        if !self.line_buffer_stderr.is_empty() {
            let line = std::mem::take(&mut self.line_buffer_stderr);
            self.event_bus.emit(EventPayload::StderrLine(line));
        }
    }

    /// Flush partial line buffers and emit `Done`. Calling this multiple
    /// times — or in addition to `Drop` — is safe; the second emission is
    /// elided via `done_emitted`.
    pub fn emit_done(&mut self) {
        self.flush_line_buffers();
        if !self
            .done_emitted
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            self.event_bus.emit(EventPayload::Done);
        }
    }

    /// Get a clone of the event bus sender (for creating child sinks).
    pub fn event_bus_sender(&self) -> EventBusSender {
        self.event_bus.clone()
    }
}

impl Drop for ApiDispatchFrontend {
    fn drop(&mut self) {
        // The engine writes container output in arbitrary byte chunks. Anything
        // not terminated by `\n` lives in the line buffers — flush it as a
        // final event so SSE clients and `events.log` see the trailing line.
        if !self.line_buffer_stdout.is_empty() {
            let line = std::mem::take(&mut self.line_buffer_stdout);
            self.event_bus.emit(EventPayload::StdoutLine(line));
        }
        if !self.line_buffer_stderr.is_empty() {
            let line = std::mem::take(&mut self.line_buffer_stderr);
            self.event_bus.emit(EventPayload::StderrLine(line));
        }
        if !self
            .done_emitted
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            self.event_bus.emit(EventPayload::Done);
        }
    }
}

// ─── UserMessageSink ────────────────────────────────────────────────────────

impl UserMessageSink for ApiDispatchFrontend {
    fn write_message(&mut self, msg: UserMessage) {
        let phase = match msg.level {
            crate::data::message::MessageLevel::Info => "info",
            crate::data::message::MessageLevel::Warning => "warn",
            crate::data::message::MessageLevel::Error => "error",
            crate::data::message::MessageLevel::Success => "ok",
        };
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: phase.to_string(),
            message: msg.text,
        });
    }

    fn replay_queued(&mut self) {}
}

// ─── CommandFrontend (flag/argument access) ─────────────────────────────────

impl ApiDispatchFrontend {
    /// Re-materialize the construction-time parse error, if any. The first
    /// accessor Dispatch calls returns this so a malformed request aborts the
    /// command instead of running with silently-dropped flags.
    fn check_parse(&self) -> Result<(), CommandError> {
        match &self.parse_error {
            None => Ok(()),
            Some(e) => Err(clone_parse_error(e)),
        }
    }
}

impl CommandFrontend for ApiDispatchFrontend {
    fn flag_bool(&self, _command_path: &[&str], flag: &str) -> Result<Option<bool>, CommandError> {
        self.check_parse()?;
        Ok(self.parsed.flag_bool(flag))
    }

    fn flag_string(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<String>, CommandError> {
        self.check_parse()?;
        Ok(self.parsed.flag_string(flag))
    }

    fn flag_strings(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Vec<String>, CommandError> {
        self.check_parse()?;
        Ok(self.parsed.flag_strings(flag))
    }

    fn flag_path(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<PathBuf>, CommandError> {
        self.check_parse()?;
        Ok(self.parsed.flag_path(flag))
    }

    fn flag_enum(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<String>, CommandError> {
        self.check_parse()?;
        Ok(self.parsed.flag_enum(flag))
    }

    fn flag_u16(&self, _command_path: &[&str], flag: &str) -> Result<Option<u16>, CommandError> {
        self.check_parse()?;
        Ok(self.parsed.flag_u16(flag))
    }

    fn flag_usize(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<usize>, CommandError> {
        self.check_parse()?;
        Ok(self.parsed.flag_usize(flag))
    }

    fn argument(&self, _command_path: &[&str], name: &str) -> Result<Option<String>, CommandError> {
        self.check_parse()?;
        Ok(self.parsed.argument(name))
    }

    fn arguments(&self, _command_path: &[&str], name: &str) -> Result<Vec<String>, CommandError> {
        self.check_parse()?;
        Ok(self.parsed.arguments(name))
    }
}

/// Reconstruct a parse-stage [`CommandError`] so accessors can return it more
/// than once from `&self` (the type is not `Clone` because it wraps engine/data
/// errors, but the parse-stage variants carry only owned string data).
fn clone_parse_error(e: &CommandError) -> CommandError {
    match e {
        CommandError::UnknownCommand { path } => {
            CommandError::UnknownCommand { path: path.clone() }
        }
        CommandError::UnknownFlag { command, flag } => CommandError::UnknownFlag {
            command: command.clone(),
            flag: flag.clone(),
        },
        CommandError::InvalidFlagValue {
            command,
            flag,
            reason,
        } => CommandError::InvalidFlagValue {
            command: command.clone(),
            flag: flag.clone(),
            reason: reason.clone(),
        },
        CommandError::MissingRequiredFlag { command, flag } => CommandError::MissingRequiredFlag {
            command: command.clone(),
            flag: flag.clone(),
        },
        CommandError::MissingRequiredArgument { command, argument } => {
            CommandError::MissingRequiredArgument {
                command: command.clone(),
                argument: argument.clone(),
            }
        }
        // parse_raw_args only produces the variants above; anything else is
        // rendered to a stable string so the command still aborts cleanly.
        other => CommandError::Other(other.to_string()),
    }
}

// ─── AgentFrontend ──────────────────────────────────────────────────────

#[async_trait]
impl AgentFrontend for ApiDispatchFrontend {
    fn report_status(&mut self, status: AgentStatus) {
        let message = match &status {
            AgentStatus::Building => "Building container image...".to_string(),
            AgentStatus::Pulling => "Pulling container image...".to_string(),
            AgentStatus::Starting => "Starting container...".to_string(),
            AgentStatus::Running { container_name } => {
                format!("Container running: {container_name}")
            }
            AgentStatus::Stopping => "Stopping container...".to_string(),
            AgentStatus::Exited(code) => format!("Container exited with code {code}"),
            AgentStatus::Failed(reason) => format!("Container failed: {reason}"),
        };
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "container".to_string(),
            message,
        });
    }

    fn report_progress(&mut self, progress: AgentProgress) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: progress.stage,
            message: progress.message,
        });
    }

    fn take_io(&mut self) -> crate::engine::agent_runtime::frontend::AgentIo {
        let event_bus_stdout = self.event_bus.clone();
        let event_bus_stderr = self.event_bus.clone();

        let (stdout_tx, mut stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stderr_tx, mut stderr_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        // API never has interactive stdin: the engine owns the only sender
        // and drops it after seeding the prompt (see `spawn_piped_docker`)
        // so the container sees EOF promptly.

        // Drain stdout → event bus (line-buffered).
        tokio::spawn(async move {
            let mut buf = String::new();
            while let Some(bytes) = stdout_rx.recv().await {
                let text = String::from_utf8_lossy(&bytes);
                buf.push_str(&text);
                while let Some(pos) = buf.find('\n') {
                    let line = buf[..pos].to_string();
                    buf = buf[pos + 1..].to_string();
                    event_bus_stdout.emit(EventPayload::StdoutLine(line));
                }
            }
            if !buf.is_empty() {
                event_bus_stdout.emit(EventPayload::StdoutLine(buf));
            }
        });

        // Drain stderr → event bus (line-buffered).
        tokio::spawn(async move {
            let mut buf = String::new();
            while let Some(bytes) = stderr_rx.recv().await {
                let text = String::from_utf8_lossy(&bytes);
                buf.push_str(&text);
                while let Some(pos) = buf.find('\n') {
                    let line = buf[..pos].to_string();
                    buf = buf[pos + 1..].to_string();
                    event_bus_stderr.emit(EventPayload::StderrLine(line));
                }
            }
            if !buf.is_empty() {
                event_bus_stderr.emit(EventPayload::StderrLine(buf));
            }
        });

        crate::engine::agent_runtime::frontend::AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        }
    }

    /// API runs unattended workloads — an agent may need minutes to pull
    /// an image or warm up a model before producing output. A 15-minute
    /// startup grace gives the container reasonable runway before the
    /// detector kills it as failed-to-start.
    fn grace_timeout(&self) -> Duration {
        Duration::from_secs(15 * 60)
    }
}

// ─── HasAgentFrontend ───────────────────────────────────────────────────

impl HasAgentFrontend for ApiDispatchFrontend {
    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        Box::new(ApiContainerSink {
            event_bus: self.event_bus.clone(),
            line_buffer_stdout: String::new(),
            line_buffer_stderr: String::new(),
        })
    }
}

/// Standalone container frontend that emits events to the EventBus.
struct ApiContainerSink {
    event_bus: EventBusSender,
    line_buffer_stdout: String,
    line_buffer_stderr: String,
}

impl UserMessageSink for ApiContainerSink {
    fn write_message(&mut self, msg: UserMessage) {
        let phase = match msg.level {
            crate::data::message::MessageLevel::Info => "info",
            crate::data::message::MessageLevel::Warning => "warn",
            crate::data::message::MessageLevel::Error => "error",
            crate::data::message::MessageLevel::Success => "ok",
        };
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: phase.to_string(),
            message: msg.text,
        });
    }
    fn replay_queued(&mut self) {}
}

#[async_trait]
impl AgentFrontend for ApiContainerSink {
    fn report_status(&mut self, status: AgentStatus) {
        let message = match &status {
            AgentStatus::Building => "Building container image...".to_string(),
            AgentStatus::Pulling => "Pulling container image...".to_string(),
            AgentStatus::Starting => "Starting container...".to_string(),
            AgentStatus::Running { container_name } => {
                format!("Container running: {container_name}")
            }
            AgentStatus::Stopping => "Stopping container...".to_string(),
            AgentStatus::Exited(code) => format!("Container exited with code {code}"),
            AgentStatus::Failed(reason) => format!("Container failed: {reason}"),
        };
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "container".to_string(),
            message,
        });
    }
    fn report_progress(&mut self, progress: AgentProgress) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: progress.stage,
            message: progress.message,
        });
    }

    fn take_io(&mut self) -> crate::engine::agent_runtime::frontend::AgentIo {
        let event_bus_stdout = self.event_bus.clone();
        let event_bus_stderr = self.event_bus.clone();

        let (stdout_tx, mut stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stderr_tx, mut stderr_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

        tokio::spawn(async move {
            let mut buf = String::new();
            while let Some(bytes) = stdout_rx.recv().await {
                let text = String::from_utf8_lossy(&bytes);
                buf.push_str(&text);
                while let Some(pos) = buf.find('\n') {
                    let line = buf[..pos].to_string();
                    buf = buf[pos + 1..].to_string();
                    event_bus_stdout.emit(EventPayload::StdoutLine(line));
                }
            }
            if !buf.is_empty() {
                event_bus_stdout.emit(EventPayload::StdoutLine(buf));
            }
        });

        tokio::spawn(async move {
            let mut buf = String::new();
            while let Some(bytes) = stderr_rx.recv().await {
                let text = String::from_utf8_lossy(&bytes);
                buf.push_str(&text);
                while let Some(pos) = buf.find('\n') {
                    let line = buf[..pos].to_string();
                    buf = buf[pos + 1..].to_string();
                    event_bus_stderr.emit(EventPayload::StderrLine(line));
                }
            }
            if !buf.is_empty() {
                event_bus_stderr.emit(EventPayload::StderrLine(buf));
            }
        });

        // Engine owns the single stdin_tx and drops it after seeding so EOF
        // arrives at the container's stdin pipe (see `spawn_piped_docker`).
        crate::engine::agent_runtime::frontend::AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        }
    }

    fn grace_timeout(&self) -> Duration {
        Duration::from_secs(15 * 60)
    }
}

// ─── MountScopeFrontend ─────────────────────────────────────────────────────

impl MountScopeFrontend for ApiDispatchFrontend {
    fn ask_mount_scope(
        &mut self,
        _git_root: &Path,
        _cwd: &Path,
    ) -> Result<MountScopeDecision, CommandError> {
        Ok(MountScopeDecision::MountGitRoot)
    }
}

// ─── AgentSetupFrontend ─────────────────────────────────────────────────────

impl AgentSetupFrontend for ApiDispatchFrontend {
    fn ask_agent_setup(
        &mut self,
        _requested: &AgentName,
        _default: &AgentName,
        default_available: bool,
        _image_only: bool,
    ) -> Result<AgentSetupDecision, CommandError> {
        if default_available {
            Ok(AgentSetupDecision::Setup)
        } else {
            Ok(AgentSetupDecision::Abort)
        }
    }

    fn record_fallback(&mut self, _requested: &AgentName, _fallback: &AgentName) {}
}

// ─── AgentAuthFrontend ──────────────────────────────────────────────────────

impl AgentAuthFrontend for ApiDispatchFrontend {
    fn ask_agent_auth_consent(
        &mut self,
        _agent: &AgentName,
        _env_var_names: &[&str],
    ) -> Result<AgentAuthDecision, CommandError> {
        Ok(AgentAuthDecision::Accept)
    }
}

// ─── WorkflowFrontend ───────────────────────────────────────────────────────

impl WorkflowFrontend for ApiDispatchFrontend {
    fn show_workflow_control_board(
        &mut self,
        _state: &crate::data::workflow_state::WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        if available.can_launch_next {
            Ok(NextAction::LaunchNext)
        } else {
            Ok(NextAction::Abort)
        }
    }

    fn yolo_countdown_tick(
        &mut self,
        step_name: &str,
        remaining: Duration,
        _total: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        use crate::engine::workflow::timing::YOLO_SINK_THROTTLE_INTERVAL;

        let should_emit = self
            .last_sink_message_time
            .map(|t| t.elapsed() >= YOLO_SINK_THROTTLE_INTERVAL)
            .unwrap_or(true);
        if should_emit {
            self.event_bus.emit(EventPayload::StatusMessage {
                phase: "yolo_countdown".to_string(),
                message: format!(
                    "Step '{}': auto-advancing in {}s",
                    step_name,
                    remaining.as_secs()
                ),
            });
            self.last_sink_message_time = Some(std::time::Instant::now());
        }
        Ok(YoloTickOutcome::Continue)
    }

    fn yolo_countdown_finished(&mut self, _step_name: &str) {
        self.last_sink_message_time = None;
    }

    fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
        let (from_str, to_str) = match &status {
            WorkflowStepStatus::Pending => return,
            WorkflowStepStatus::Running => ("pending", "running"),
            WorkflowStepStatus::Succeeded => ("running", "succeeded"),
            WorkflowStepStatus::Failed { .. } => ("running", "failed"),
            WorkflowStepStatus::Cancelled => ("pending", "cancelled"),
            WorkflowStepStatus::Skipped => ("pending", "skipped"),
        };
        let idx = self.step_index_for(&step.name);
        self.event_bus.emit(EventPayload::WorkflowStepTransition {
            step_name: step.name.clone(),
            step_index: idx,
            from_status: from_str.to_string(),
            to_status: to_str.to_string(),
        });
    }

    fn report_step_output(&mut self, _step: &WorkflowStep, _output: StepOutput) {}

    fn report_parallel_step_launched(&mut self, step_name: &str, agent: &str, model: Option<&str>) {
        let idx = self.step_index_for(step_name);
        self.event_bus
            .emit(EventPayload::WorkflowParallelStepLaunched {
                step_name: step_name.to_string(),
                step_index: idx,
                agent: agent.to_string(),
                model: model.map(|m| m.to_string()),
            });
    }

    fn report_parallel_step_exited(&mut self, step_name: &str, exit_code: i32) {
        let idx = self.step_index_for(step_name);
        self.event_bus
            .emit(EventPayload::WorkflowParallelStepExited {
                step_name: step_name.to_string(),
                step_index: idx,
                exit_code,
            });
    }

    fn report_parallel_group_finished(&mut self) {
        self.event_bus
            .emit(EventPayload::WorkflowParallelGroupFinished);
    }

    fn report_workflow_progress(
        &mut self,
        steps: &[crate::engine::workflow::actions::WorkflowStepProgressInfo],
    ) {
        // Emit one WorkflowPhaseTransition event the first time the engine
        // reports progress (the workflow has entered the main phase) and one
        // more on completion (handled in report_workflow_completed).
        let mut phase_emitted = self
            .phase_emitted
            .lock()
            .expect("phase_emitted lock poisoned");
        if !*phase_emitted {
            *phase_emitted = true;
            drop(phase_emitted);
            let total = steps.len();
            self.event_bus.emit(EventPayload::WorkflowPhaseTransition {
                phase: "main".to_string(),
                step_desc: format!(
                    "Running workflow ({total} step{})",
                    if total == 1 { "" } else { "s" }
                ),
                status: "running".to_string(),
            });
        }
    }

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

    fn on_setup_step_started(&mut self, description: &str) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "setup".to_string(),
            message: format!("started: {description}"),
        });
    }

    fn on_setup_step_output(&mut self, line: &str) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "setup".to_string(),
            message: line.to_string(),
        });
    }

    fn on_setup_step_completed(&mut self, description: &str) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "setup".to_string(),
            message: format!("completed: {description}"),
        });
    }

    fn on_setup_step_failed(&mut self, description: &str, exit_code: i32, stderr: &str) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "setup".to_string(),
            message: format!("failed: {description} (exit {exit_code}): {stderr}"),
        });
    }

    fn on_teardown_step_started(&mut self, description: &str) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "teardown".to_string(),
            message: format!("started: {description}"),
        });
    }

    fn on_teardown_step_output(&mut self, line: &str) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "teardown".to_string(),
            message: line.to_string(),
        });
    }

    fn on_teardown_step_completed(&mut self, description: &str) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "teardown".to_string(),
            message: format!("completed: {description}"),
        });
    }

    fn on_teardown_step_failed(&mut self, description: &str, exit_code: i32, stderr: &str) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "teardown".to_string(),
            message: format!("failed: {description} (exit {exit_code}): {stderr}"),
        });
    }

    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
        let (status, exit_code, error, phase_status, step_desc) = match outcome {
            WorkflowOutcome::Completed => (
                "done".to_string(),
                Some(0),
                None,
                "succeeded",
                "Workflow completed".to_string(),
            ),
            WorkflowOutcome::Paused => (
                "paused".to_string(),
                None,
                None,
                "paused",
                "Workflow paused".to_string(),
            ),
            WorkflowOutcome::Aborted => (
                "aborted".to_string(),
                Some(1),
                None,
                "failed",
                "Workflow aborted".to_string(),
            ),
            WorkflowOutcome::CompletedTeardownFailed => (
                "done".to_string(),
                Some(1),
                Some("Teardown failed".to_string()),
                "teardown_failed",
                "Workflow completed but teardown failed".to_string(),
            ),
            WorkflowOutcome::Failed {
                last_step,
                exit_code,
            } => (
                "error".to_string(),
                Some(*exit_code),
                Some(format!("Step '{last_step}' failed")),
                "failed",
                format!("Step '{last_step}' failed"),
            ),
        };
        self.event_bus.emit(EventPayload::WorkflowPhaseTransition {
            phase: "main".to_string(),
            step_desc,
            status: phase_status.to_string(),
        });
        self.event_bus.emit(EventPayload::CommandStatus {
            status,
            exit_code,
            error,
        });
    }
}

// ─── WorktreeLifecycleFrontend ──────────────────────────────────────────────

impl WorktreeLifecycleFrontend for ApiDispatchFrontend {
    fn ask_pre_worktree_uncommitted_files(
        &mut self,
        _files: &[String],
        suggested_message: &str,
    ) -> Result<PreWorktreeDecision, CommandError> {
        Ok(PreWorktreeDecision::Commit {
            message: suggested_message.to_string(),
        })
    }

    fn ask_existing_worktree(
        &mut self,
        _path: &Path,
        _branch: &str,
    ) -> Result<ExistingWorktreeDecision, CommandError> {
        Ok(ExistingWorktreeDecision::Resume)
    }

    fn report_worktree_created(&mut self, path: &Path, branch: &str) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "worktree".to_string(),
            message: format!("Worktree created: {} (branch: {branch})", path.display()),
        });
    }

    fn ask_post_workflow_action(
        &mut self,
        prompt: &crate::command::commands::worktree_lifecycle::PostWorkflowWorktreePrompt,
    ) -> Result<PostWorkflowWorktreeAction, CommandError> {
        if prompt.had_error {
            Ok(PostWorkflowWorktreeAction::Keep)
        } else {
            Ok(PostWorkflowWorktreeAction::Merge)
        }
    }

    fn ask_worktree_commit_before_merge(
        &mut self,
        _branch: &str,
        _files: &[String],
        suggested_message: &str,
    ) -> Result<Option<String>, CommandError> {
        Ok(Some(suggested_message.to_string()))
    }

    fn ask_merge_mode(&mut self, _branch: &str) -> Result<WorktreeMergeMode, CommandError> {
        // Headless API runs keep the historical behaviour: squash merge.
        Ok(WorktreeMergeMode::Squash)
    }

    fn confirm_worktree_cleanup(
        &mut self,
        _branch: &str,
        _path: &Path,
    ) -> Result<bool, CommandError> {
        Ok(true)
    }

    fn report_merge_conflict(&mut self, branch: &str, worktree_path: &Path, _git_root: &Path) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "worktree".to_string(),
            message: format!(
                "Merge conflict on branch '{branch}' at {}",
                worktree_path.display()
            ),
        });
    }

    fn report_worktree_discarded(&mut self, branch: &str) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "worktree".to_string(),
            message: format!("Worktree discarded: {branch}"),
        });
    }

    fn report_worktree_kept(&mut self, path: &Path, branch: &str) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "worktree".to_string(),
            message: format!("Worktree kept: {} (branch: {branch})", path.display()),
        });
    }
}

// ─── InitFrontend ───────────────────────────────────────────────────────────

impl InitFrontend for ApiDispatchFrontend {
    fn ask_replace_aspec(&mut self) -> Result<bool, EngineError> {
        Ok(false)
    }
    fn ask_run_audit(&mut self) -> Result<bool, EngineError> {
        Ok(false)
    }
    fn ask_work_items_setup(&mut self) -> Result<Option<WorkItemsConfig>, EngineError> {
        Ok(None)
    }
    fn ask_dockerfile_setup(
        &mut self,
        _git_root: &std::path::Path,
    ) -> Result<crate::engine::init::frontend::DockerfileSetupDecision, EngineError> {
        Ok(crate::engine::init::frontend::DockerfileSetupDecision::CreateNew)
    }
    fn report_phase(&mut self, phase: &InitPhase) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "init".to_string(),
            message: format!("Init phase: {phase:?}"),
        });
    }
    fn report_step_status(&mut self, step: &str, status: StepStatus) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "init".to_string(),
            message: format!("Init step '{step}': {status:?}"),
        });
    }
    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        Box::new(ApiContainerSink {
            event_bus: self.event_bus.clone(),
            line_buffer_stdout: String::new(),
            line_buffer_stderr: String::new(),
        })
    }
    fn report_summary(&mut self, _summary: &InitSummary) {}
}

// ─── ReadyFrontend ──────────────────────────────────────────────────────────

impl ReadyFrontend for ApiDispatchFrontend {
    fn ask_create_dockerfile(
        &mut self,
        _dockerfile_path: &std::path::Path,
    ) -> Result<bool, EngineError> {
        Ok(true)
    }
    fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError> {
        Ok(false)
    }
    fn report_phase(&mut self, phase: &ReadyPhase) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "ready".to_string(),
            message: format!("Ready phase: {phase:?}"),
        });
    }
    fn report_step_status(&mut self, step: &str, status: StepStatus) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "ready".to_string(),
            message: format!("Ready step '{step}': {status:?}"),
        });
    }
    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        Box::new(ApiContainerSink {
            event_bus: self.event_bus.clone(),
            line_buffer_stdout: String::new(),
            line_buffer_stderr: String::new(),
        })
    }
    fn report_summary(&mut self, _summary: &ReadySummary) {}
}

// ─── Per-command frontend markers ───────────────────────────────────────────

impl RemoteCommandFrontend for ApiDispatchFrontend {}
impl DownloadCommandFrontend for ApiDispatchFrontend {}

impl StatusCommandFrontend for ApiDispatchFrontend {}

impl AuthCommandFrontend for ApiDispatchFrontend {}

impl ConfigCommandFrontend for ApiDispatchFrontend {
    fn present_config_table(
        &mut self,
        _rows: &[ConfigFieldRow],
        _rejected: Option<&crate::command::commands::config::ConfigEditRejection>,
    ) -> Result<Option<ConfigEditRequest>, CommandError> {
        Ok(None)
    }
}

// `awman clean` is blocked for the API frontend at the catalogue layer
// (`api_allowed: false`); this impl exists only to satisfy the `DispatchFrontend`
// supertrait bound and never runs. It never confirms a deletion.
impl crate::command::commands::clean::CleanCommandFrontend for ApiDispatchFrontend {
    fn confirm_deletion(
        &mut self,
        _summary: &crate::command::commands::clean::CleanSummary,
    ) -> Result<bool, CommandError> {
        Ok(false)
    }
}

#[async_trait]
impl ApiServerCommandFrontend for ApiDispatchFrontend {
    async fn serve_until_shutdown(&mut self, _config: ApiServeConfig) -> Result<(), CommandError> {
        Err(CommandError::Other(
            "Cannot start a nested API server from within API dispatch".into(),
        ))
    }
}

impl ChatCommandFrontend for ApiDispatchFrontend {
    fn set_pty_active(&mut self, _active: bool) {}
}

impl ExecPromptCommandFrontend for ApiDispatchFrontend {}

#[async_trait]
impl ExecWorkflowCommandFrontend for ApiDispatchFrontend {
    fn set_pty_active(&mut self, _active: bool) {}
    fn report_workflow_summary(&mut self, summary: &WorkflowSummary) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "workflow".to_string(),
            message: format!(
                "Workflow summary: {} completed, {} failed",
                summary.steps_completed, summary.steps_failed
            ),
        });
    }
    fn ask_workflow_resume_or_fresh(
        &mut self,
        _workflow_name: &str,
        _completed_steps: usize,
        _total_steps: usize,
    ) -> Result<bool, CommandError> {
        // API mode has no interactive prompt; resume by default.
        Ok(true)
    }
}

impl SpecsCommandFrontend for ApiDispatchFrontend {}

impl NewCommandFrontend for ApiDispatchFrontend {}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_frontend(subcommand: &str, args: &[&str]) -> ApiDispatchFrontend {
        let bus = crate::frontend::api::event_bus::EventBus::new(16);
        let sender = bus.sender();
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        ApiDispatchFrontend::new(subcommand, &args, sender)
    }

    // ─── flag_bool ────────────────────────────────────────────────────────────

    #[test]
    fn flag_bool_bare_flag_is_true() {
        let f = make_frontend("chat", &["--yolo"]);
        assert_eq!(f.flag_bool(&["chat"], "yolo").unwrap(), Some(true));
    }

    #[test]
    fn flag_bool_presence_sets_true() {
        // Bools are `SetTrue` (clap parity): presence implies true; no value
        // token is consumed. `--background` is a real flag on `api start`.
        let f = make_frontend("api start", &["--background"]);
        assert_eq!(
            f.flag_bool(&["api", "start"], "background").unwrap(),
            Some(true)
        );
    }

    #[test]
    fn unknown_flag_surfaces_structured_error_from_accessor() {
        // Catalogue-driven parsing rejects an undeclared flag rather than
        // silently dropping it; the error surfaces on the first accessor call.
        let f = make_frontend("exec prompt", &["--not-a-real-flag"]);
        let err = f.flag_bool(&["exec", "prompt"], "yolo").unwrap_err();
        assert!(matches!(err, CommandError::UnknownFlag { .. }));
    }

    // ─── flag_string ──────────────────────────────────────────────────────────

    #[test]
    fn flag_string_parses_value_after_flag() {
        let f = make_frontend("exec prompt", &["--agent", "claude"]);
        assert_eq!(
            f.flag_string(&["exec", "prompt"], "agent")
                .unwrap()
                .as_deref(),
            Some("claude")
        );
    }

    #[test]
    fn flag_string_parses_equals_syntax() {
        let f = make_frontend("exec prompt", &["--agent=claude"]);
        assert_eq!(
            f.flag_string(&["exec", "prompt"], "agent")
                .unwrap()
                .as_deref(),
            Some("claude")
        );
    }

    #[test]
    fn flag_string_absent_returns_none() {
        let f = make_frontend("exec prompt", &[]);
        assert_eq!(f.flag_string(&["exec", "prompt"], "agent").unwrap(), None);
    }

    // ─── flag_u16 ─────────────────────────────────────────────────────────────

    #[test]
    fn flag_u16_parses_port_value() {
        let f = make_frontend("api start", &["--port", "9876"]);
        assert_eq!(f.flag_u16(&["api", "start"], "port").unwrap(), Some(9876));
    }

    // ─── argument (positional) ────────────────────────────────────────────────

    #[test]
    fn argument_exec_prompt_maps_positional_to_prompt() {
        let f = make_frontend("exec prompt", &["hello", "world"]);
        assert_eq!(
            f.argument(&["exec", "prompt"], "prompt")
                .unwrap()
                .as_deref(),
            Some("hello world")
        );
    }

    // ─── non-interactive and yolo flags are always set ────────────────────────

    #[test]
    fn non_interactive_flag_always_set() {
        let f = make_frontend("chat", &[]);
        assert_eq!(
            f.flag_bool(&["chat"], "non-interactive").unwrap(),
            Some(true),
            "non-interactive must always be set in API mode"
        );
    }

    #[test]
    fn yolo_flag_always_set() {
        let f = make_frontend("chat", &[]);
        assert_eq!(
            f.flag_bool(&["chat"], "yolo").unwrap(),
            Some(true),
            "yolo must always be set in API mode"
        );
    }

    // ─── flag_strings (multi-value) ───────────────────────────────────────────

    #[test]
    fn flag_strings_collects_multiple_values() {
        let f = make_frontend("api start", &["--workdirs", "/a", "--workdirs", "/b"]);
        let dirs = f.flag_strings(&["api", "start"], "workdirs").unwrap();
        assert!(dirs.contains(&"/a".to_string()));
        assert!(dirs.contains(&"/b".to_string()));
    }

    // ─── Drop emits Done and flushes partial lines ─────────────────────────────

    #[tokio::test]
    async fn drop_emits_done_sentinel_when_emit_done_was_not_called() {
        use crate::engine::agent_runtime::frontend::AgentFrontend;
        let bus = crate::frontend::api::event_bus::EventBus::new(16);
        let mut rx = bus.subscribe();
        let mut fe = ApiDispatchFrontend::new("exec prompt", &[], bus.sender());
        let io = fe.take_io();
        io.stdout.send(b"a line\n".to_vec()).unwrap();
        drop(io);
        // Give the drain task a moment to process.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(fe);

        let line = rx.recv().await.unwrap();
        assert!(matches!(line.payload, EventPayload::StdoutLine(ref s) if s == "a line"));
        let done = rx.recv().await.unwrap();
        assert!(
            matches!(done.payload, EventPayload::Done),
            "Drop must emit Done; got {:?}",
            done.payload
        );
    }

    #[tokio::test]
    async fn drop_flushes_partial_stdout_line_before_done() {
        use crate::engine::agent_runtime::frontend::AgentFrontend;
        let bus = crate::frontend::api::event_bus::EventBus::new(16);
        let mut rx = bus.subscribe();
        let mut fe = ApiDispatchFrontend::new("exec prompt", &[], bus.sender());
        let io = fe.take_io();
        // No trailing newline — the line lives in the drain task buffer until
        // the sender is dropped and the drain flushes.
        io.stdout.send(b"trailing partial".to_vec()).unwrap();
        drop(io);
        // Give the drain task a moment to process and flush.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(fe);

        let line = rx.recv().await.unwrap();
        assert!(
            matches!(line.payload, EventPayload::StdoutLine(ref s) if s == "trailing partial"),
            "partial stdout line must be flushed by drain task; got {:?}",
            line.payload
        );
        let done = rx.recv().await.unwrap();
        assert!(matches!(done.payload, EventPayload::Done));
    }

    #[tokio::test]
    async fn drop_flushes_partial_stderr_line_before_done() {
        use crate::engine::agent_runtime::frontend::AgentFrontend;
        let bus = crate::frontend::api::event_bus::EventBus::new(16);
        let mut rx = bus.subscribe();
        let mut fe = ApiDispatchFrontend::new("exec prompt", &[], bus.sender());
        let io = fe.take_io();
        io.stderr.send(b"err partial".to_vec()).unwrap();
        drop(io);
        // Give the drain task a moment to process and flush.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(fe);

        let line = rx.recv().await.unwrap();
        assert!(
            matches!(line.payload, EventPayload::StderrLine(ref s) if s == "err partial"),
            "partial stderr line must be flushed by drain task; got {:?}",
            line.payload
        );
        let done = rx.recv().await.unwrap();
        assert!(matches!(done.payload, EventPayload::Done));
    }

    #[tokio::test]
    async fn explicit_emit_done_then_drop_does_not_double_emit() {
        let bus = crate::frontend::api::event_bus::EventBus::new(16);
        let mut rx = bus.subscribe();
        let mut fe = ApiDispatchFrontend::new("exec prompt", &[], bus.sender());
        fe.emit_done();
        drop(fe);

        let done = rx.recv().await.unwrap();
        assert!(matches!(done.payload, EventPayload::Done));
        // Second recv must time out — no second Done.
        let again = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(
            again.is_err(),
            "Drop must NOT emit a second Done after explicit emit_done; got {:?}",
            again
        );
    }

    // ── yolo countdown message throttle ──────────────────────────────────────

    fn count_countdown_events(
        rx: &mut tokio::sync::broadcast::Receiver<crate::data::execution_event::ExecutionEvent>,
    ) -> usize {
        let mut count = 0;
        while let Ok(evt) = rx.try_recv() {
            if matches!(
                &evt.payload,
                EventPayload::StatusMessage { phase, .. } if phase == "yolo_countdown"
            ) {
                count += 1;
            }
        }
        count
    }

    /// Ten rapid ticks must produce exactly one `yolo_countdown` status message
    /// because all subsequent calls fall within the 10-second throttle window.
    #[tokio::test]
    async fn yolo_countdown_throttles_api_messages_within_window() {
        use crate::engine::workflow::frontend::WorkflowFrontend as _;
        let bus = crate::frontend::api::event_bus::EventBus::new(64);
        let mut rx = bus.subscribe();
        let mut fe = ApiDispatchFrontend::new("exec workflow", &[], bus.sender());

        for i in 0..10u64 {
            fe.yolo_countdown_tick(
                "step",
                std::time::Duration::from_secs(60u64.saturating_sub(i)),
                std::time::Duration::from_secs(60),
            )
            .unwrap();
        }

        // Allow any async tasks to flush.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let count = count_countdown_events(&mut rx);
        assert_eq!(
            count, 1,
            "only the first tick should emit within the 10-second throttle window"
        );
    }

    /// After `yolo_countdown_finished` resets the timer, the very next tick
    /// must emit a fresh message.
    #[tokio::test]
    async fn yolo_countdown_tick_emits_again_after_finished_resets_timer() {
        use crate::engine::workflow::frontend::WorkflowFrontend as _;
        let bus = crate::frontend::api::event_bus::EventBus::new(64);
        let mut rx = bus.subscribe();
        let mut fe = ApiDispatchFrontend::new("exec workflow", &[], bus.sender());

        // First tick: emits.
        fe.yolo_countdown_tick(
            "step",
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(60),
        )
        .unwrap();
        // Rapid second tick: throttled.
        fe.yolo_countdown_tick(
            "step",
            std::time::Duration::from_secs(59),
            std::time::Duration::from_secs(60),
        )
        .unwrap();

        // Reset.
        fe.yolo_countdown_finished("step");

        // Next tick after reset: must emit again.
        fe.yolo_countdown_tick(
            "step",
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(60),
        )
        .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let count = count_countdown_events(&mut rx);
        assert_eq!(
            count, 2,
            "expect 2 events: initial tick + tick after countdown_finished reset"
        );
    }

    /// Simulating an elapsed throttle window by rewinding `last_sink_message_time`
    /// makes the next tick emit a new message.
    #[tokio::test]
    async fn yolo_countdown_tick_emits_after_throttle_window_elapses() {
        use crate::engine::workflow::frontend::WorkflowFrontend as _;
        let bus = crate::frontend::api::event_bus::EventBus::new(64);
        let mut rx = bus.subscribe();
        let mut fe = ApiDispatchFrontend::new("exec workflow", &[], bus.sender());

        // First tick emits.
        fe.yolo_countdown_tick(
            "step",
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(60),
        )
        .unwrap();

        // Rewind the throttle timestamp to simulate 11 seconds having passed.
        fe.last_sink_message_time =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(11));

        // Next tick must emit (window elapsed).
        fe.yolo_countdown_tick(
            "step",
            std::time::Duration::from_secs(58),
            std::time::Duration::from_secs(60),
        )
        .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let count = count_countdown_events(&mut rx);
        assert_eq!(
            count, 2,
            "expected 2 events: initial tick + tick after simulated 11-second gap"
        );
    }
}
