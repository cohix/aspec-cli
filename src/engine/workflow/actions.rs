//! `NextAction`, `AvailableActions`, `StepFailureChoice`, `YoloTickOutcome`.

use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextAction {
    /// Launch a fresh container for the next ready step.
    LaunchNext,
    /// Push an additional prompt into the still-running container, keeping it
    /// alive for the next step. Only valid when the next step targets the
    /// same agent and the running container supports prompt injection.
    ContinueInCurrentContainer { prompt: String },
    /// Re-run the step that just completed.
    RestartCurrentStep,
    /// Revert to the immediately-previous step in topological order.
    CancelToPreviousStep,
    /// Mark every remaining step as Skipped and the workflow as completed.
    /// Only valid when the current step is the last in topological order.
    FinishWorkflow,
    /// Pause execution after the current step completes.
    Pause,
    /// Abort the workflow entirely.
    Abort,
    /// Mid-step only: dismiss the control board dialog without affecting the
    /// running step. The step continues executing undisturbed.
    Dismiss,
}

/// Set of `NextAction` variants the frontend may present to the user. The
/// engine computes this set; the frontend renders only what it permits.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AvailableActions {
    pub can_continue_in_current_container: bool,
    pub can_launch_next: bool,
    pub can_restart_current_step: bool,
    pub can_cancel_to_previous_step: bool,
    pub can_finish_workflow: bool,
    pub can_pause: bool,
    pub can_abort: bool,
    /// The prompt to inject when the user chooses `ContinueInCurrentContainer`.
    /// Set by the engine from the next step's resolved prompt template whenever
    /// `can_continue_in_current_container` is true.
    pub continue_prompt: Option<String>,
    pub continue_unavailable_reason: Option<String>,
    pub cancel_to_previous_unavailable_reason: Option<String>,
    pub finish_workflow_unavailable_reason: Option<String>,
    /// Reason `RestartCurrentStep` is unavailable / scoped. Set when the WCB
    /// is opened inside a parallel group (WI-0096 §10): restart only affects
    /// the focused container.
    pub restart_unavailable_reason: Option<String>,
    /// Total number of steps in the focused step's parallel group (0 when the
    /// focused step is not part of a multi-step parallel batch). Lets frontends
    /// scope the Workflow Control Board to the parallel context.
    pub parallel_peer_count: usize,
    /// Live peers of the focused step still running in the same parallel group
    /// (excludes the focused step itself). Non-zero disables back/finish.
    pub parallel_peers_running: usize,
    /// True when a container is currently running (mid-step). The engine
    /// computes this from `current_execution.is_some()` in
    /// `compute_available_actions`. Changes Esc semantics from Pause to Dismiss.
    pub can_dismiss: bool,
    /// Custom label for the right-arrow (launch-next) action. Defaults to
    /// "Next: new container" when `None`. The dynamic leader step sets this to
    /// "Start dynamic workflow" so CLI, TUI, and API frontends all render the
    /// same presentation hint without forking the rendering code (WI-0092 §8).
    pub launch_next_label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepFailureChoice {
    Retry,
    Pause,
    Abort,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YoloTickOutcome {
    Continue,
    Cancel,
    AdvanceNow,
}

/// What `step_once` returned: the step that just executed plus its outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepOutcome {
    pub step_name: String,
    pub status: WorkflowStepStatus,
    pub remaining: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowStepStatus {
    Pending,
    Running,
    Succeeded,
    Failed { exit_code: i32 },
    Cancelled,
    Skipped,
}

/// What `run_to_completion` returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowOutcome {
    Completed,
    Paused,
    Aborted,
    Failed {
        last_step: String,
        exit_code: i32,
    },
    /// Main workflow completed but a teardown step with `abort_on_failure`
    /// failed. Post-workflow actions (worktree flows) should still run, but
    /// non-interactive contexts should default to keeping the worktree.
    CompletedTeardownFailed,
}

/// What the engine produces while a step's container streams output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepOutput {
    pub step_name: String,
    pub kind: StepOutputKind,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutputKind {
    Stdout,
    Stderr,
}

/// Information that `WorkflowFrontend::confirm_resume` receives when a
/// persisted workflow's hash differs from the current parsed file's hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeMismatch {
    pub workflow_name: String,
    pub saved_hash: String,
    pub current_hash: String,
    pub message: String,
}

/// Yolo-countdown tick metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YoloTick {
    pub remaining: Duration,
}

/// Per-step snapshot used by `WorkflowFrontend::report_workflow_progress`.
/// The engine pre-resolves agent/model so the frontend doesn't need to.
#[derive(Debug, Clone)]
pub struct WorkflowStepProgressInfo {
    pub name: String,
    /// Resolved agent name (step > workflow > config fallback, or "?" on error).
    pub agent: String,
    /// Resolved model, if any.
    pub model: Option<String>,
    /// Whether the step itself declares an `agent` or `model` field. When
    /// `false`, the resolved `agent`/`model` above come entirely from the
    /// project defaults, so the strip renders no agent/model label for the step.
    pub has_step_override: bool,
    pub status: WorkflowStepStatus,
    /// Steps this one depends on. Drives the topological column grouping in
    /// the Workflow Overview renderer.
    pub depends_on: Vec<String>,
    /// Effective workflow concurrency cap. `None` means unlimited.
    pub max_concurrent: Option<usize>,
}
