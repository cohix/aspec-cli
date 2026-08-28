//! The condition-evaluation delegation seam.
//!
//! [`ConditionEvaluator`] is defined here, at Layer 1, but implemented at
//! Layer 2 (`command::commands::amie::LocalConditionEvaluator`). This is the
//! same pattern `WorkflowEngine` uses to reach higher layers: a lower layer
//! that needs a higher layer's capability accepts a trait the higher layer
//! provides, rather than importing it directly (grand-architecture Tenet 1).
//!
//! The scheduler owns the run's lifecycle in the store — it writes the
//! `amie_runs` row before calling [`ConditionEvaluator::evaluate`] and records
//! the terminal status afterwards. The evaluator therefore never touches the
//! store; it is a pure function from an [`EvaluationRequest`] to an
//! [`EvaluationOutcome`], which keeps Layer-1 store ownership intact and makes
//! the scheduler trivially testable against a mocked evaluator.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use crate::data::fs::{Condition, RunId};

/// The evaluator's one write-back into the run row, called the moment the
/// generated workflow starts executing.
///
/// The scheduler still owns the run's lifecycle; this narrow seam exists only so
/// the daemon's `GET /v1/conditions/{name}/workflow` route can read a live
/// workflow's state while the run is still in flight, instead of waiting for the
/// terminal `finish_run`.
pub trait RunProgress: Send + Sync {
    /// The workflow validated and is about to run. `state_path` is the engine's
    /// own `WorkflowStateStore` file for it — never a re-derived filename.
    fn workflow_started(&self, run_id: &RunId, workflow_path: &Path, state_path: &Path);
}

/// A `RunProgress` that records nothing. Used by tests and by any caller with no
/// run row to update.
pub struct NoRunProgress;

impl RunProgress for NoRunProgress {
    fn workflow_started(&self, _run_id: &RunId, _workflow_path: &Path, _state_path: &Path) {}
}

/// Everything the Layer-2 evaluator needs to evaluate one due condition.
///
/// The `guidance` / `agents_to_models` / `default_leader` fields are read from
/// `amie` config *at the top of the tick that produced this request*, so a
/// config edit takes effect on the next tick without a daemon restart. They
/// are the effective *defaults*; the condition's own `agent`/`model` columns
/// (carried on `condition`) still win where present.
pub struct EvaluationRequest {
    /// The due condition, exactly as selected by `due_for_evaluation`.
    pub condition: Condition,
    /// The run row the scheduler already opened for this evaluation.
    pub run_id: RunId,
    /// The condition's persistent context directory
    /// (`<amie_root>/conditions/<name>`), resolved and validated by the
    /// scheduler. The launcher seeds it; the leader writes `workflow.toml` here.
    pub condition_dir: PathBuf,
    /// `amie.guidance`, always additive to the leader prompt, never overridden
    /// by a condition.
    pub guidance: Option<Vec<String>>,
    /// `amie.agentsToModels`: the default agent/model pool.
    pub agents_to_models: Option<HashMap<String, Vec<String>>>,
    /// `amie.defaultLeader`: the fallback leader spec (`agent::model`).
    pub default_leader: Option<String>,
    /// Where the evaluator reports that the generated workflow has started.
    pub progress: Arc<dyn RunProgress>,
}

/// The terminal result of evaluating one condition. The scheduler maps each
/// variant onto a [`RunStatus`](crate::data::fs::RunStatus) and, for
/// [`Failed`](EvaluationOutcome::Failed), grows the condition's backoff.
pub enum EvaluationOutcome {
    /// The agent read the condition as not met (or was uncertain — the leader
    /// defaults to "not triggered" on a low-confidence read). No workflow was
    /// generated or executed.
    NotTriggered,
    /// The condition was met, a valid `workflow.toml` was produced, and the
    /// workflow ran.
    WorkflowExecuted {
        /// The validated `workflow.toml` the leader produced.
        workflow_path: PathBuf,
        /// The engine's persisted `WorkflowState` file, recorded on the run row
        /// so the daemon route can read it back without re-deriving the name.
        workflow_state_path: Option<PathBuf>,
        /// The workflow's exit code, when one was observed.
        exit_code: Option<i32>,
    },
    /// Evaluation failed — the agent could not run, workflow generation
    /// exhausted its repair attempts, or execution errored. The scheduler
    /// records the error and grows an exponential backoff.
    Failed { error: String },
}

/// The seam the scheduler calls for every due condition. Layer 2 provides the
/// implementation; Layer 1 only ever calls it.
#[async_trait]
pub trait ConditionEvaluator: Send + Sync {
    /// Evaluate one condition. Implementations must never panic on a bad
    /// condition — every failure path returns [`EvaluationOutcome::Failed`] so
    /// the scheduler can record it and back the condition off.
    async fn evaluate(&self, request: EvaluationRequest) -> EvaluationOutcome;
}
