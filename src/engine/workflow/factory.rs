//! `AgentExecutionFactory` — wired by Layer 2 to bridge the workflow
//! engine and the agent runtime without leaking option lists or frontend
//! types into engine internals.

use std::path::PathBuf;

use crate::data::session::{AgentName, Session, SessionId};
use crate::data::workflow_definition::WorkflowStep;
use crate::engine::agent_runtime::execution::AgentExecution;
use crate::engine::context_prompt::WorkflowStepInfo;
use crate::engine::error::EngineError;

/// Resolved per-step runtime context (agent, model, working dir, session id).
#[derive(Debug, Clone)]
pub struct WorkflowRuntimeContext {
    pub step_agent: AgentName,
    pub step_model: Option<String>,
    pub git_root: PathBuf,
    pub session_id: SessionId,
    /// Workflow invocation UUID, used to key `context(workflow)` directories
    /// so resumed runs reuse the same directory.
    pub workflow_invocation_id: uuid::Uuid,
    /// Step progression info for building the workflow context prompt.
    pub workflow_step_info: Option<WorkflowStepInfo>,
}

/// Trait implemented by Layer 2: produce a fresh `AgentExecution` for a
/// step, or inject a prompt into an already-running container.
pub trait AgentExecutionFactory: Send + Sync {
    fn execution_for_step(
        &self,
        step: &WorkflowStep,
        session: &Session,
        runtime: &WorkflowRuntimeContext,
    ) -> Result<AgentExecution, EngineError>;

    /// Inject an additional prompt into a running container rather than
    /// launching a new one. Returns `Ok(None)` when the runtime backend does
    /// not support prompt injection (engine then falls back to a fresh
    /// container).
    fn inject_prompt(
        &self,
        execution: &AgentExecution,
        prompt: &str,
    ) -> Result<Option<()>, EngineError>;

    /// Give the command layer a chance to recover a descriptor-recognised
    /// authentication failure. The default deliberately does nothing so the
    /// workflow engine remains agent-agnostic and existing factories retain
    /// their current behaviour.
    fn recover_auth_failure(
        &self,
        _agent: &AgentName,
        _output_tail: &str,
    ) -> Result<bool, EngineError> {
        Ok(false)
    }
}
