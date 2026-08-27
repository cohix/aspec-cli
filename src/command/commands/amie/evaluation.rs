//! `LocalConditionEvaluator` — the Layer-2 half of condition evaluation.
//!
//! The scheduler (Layer 1) decides *when* a condition is evaluated and owns its
//! run row. This module decides *what happens* when one is: resolve the leader
//! agent and model, assemble the leader prompt, launch the leader in a
//! container through [`AmieAgentLauncher`], drive the shared WI-0092
//! [`WorkflowRepairLoop`], and — when the condition triggered — execute the
//! generated workflow through the same `ExecWorkflowCommand` path
//! `awman exec workflow` uses, with worktree isolation forced on.
//!
//! It lives at Layer 2 because every one of those steps is a Layer-2 concern
//! (config precedence, workflow validation, command execution). Layer 1 only
//! ever calls the [`ConditionEvaluator`] trait.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use crate::command::commands::amie::runtime_guard::require_container_tier;
use crate::command::commands::dynamic_repair::{RepairDecision, WorkflowRepairLoop};
use crate::command::commands::exec_workflow::{
    build_effective_agents_to_models, ensure_agent_image, format_agents_with_models,
    format_available_agents, validate_generated_workflow, ExecWorkflowCommand,
    ExecWorkflowCommandFlags, ExecWorkflowCommandFrontend, LeaderSpec,
};
use crate::command::commands::mount_scope::MountScopeDecision;
use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::config::env::EnvSnapshot;
use crate::data::dynamic_workflow_assets::build_amie_leader_prompt;
use crate::data::fs::condition_store::{Condition, MountScope};
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::{AgentName, Session, SessionOpenOptions};
use crate::data::EngineWorkflowStateStore;
use crate::engine::agent::AgentRunOptions;
use crate::engine::agent_runtime::frontend::AgentFrontend;
use crate::engine::amie::{
    AmieAgentLauncher, ConditionEvaluator, EvaluationOutcome, EvaluationRequest, LeaderRunSpec,
};
use crate::engine::container::options::OverlayPermission;
use crate::engine::container::options::YoloMode;
use crate::engine::overlay::{ContextOverlay, ContextScope};

/// Where the condition directory is mounted inside the leader's container.
/// It is the `context(workflow)` mount point, so the leader sees exactly the
/// layout `exec workflow --dynamic`'s leader sees.
pub const CONDITION_DIR_CONTAINER_PATH: &str = "/awman/context/workflow";

/// The frontends an unattended amie run needs.
///
/// Frontends are Layer 3; the evaluator is Layer 2 and therefore accepts them
/// through this seam rather than constructing them. The daemon supplies an
/// implementation that never prompts and never blocks.
pub trait AmieRunFrontends: Send + Sync {
    /// A frontend for one leader/repair agent launch. `label` is the repair
    /// loop's attempt label (`leader`, `leader-repair-1`, …).
    fn leader_frontend(&self, condition: &str, label: &str) -> Box<dyn AgentFrontend>;

    /// A frontend for executing the generated workflow. `mount_scope` is the
    /// scope captured when the condition was created; the frontend must answer
    /// the mount-scope question with it and never widen it.
    fn workflow_frontend(
        &self,
        condition: &str,
        mount_scope: MountScopeDecision,
    ) -> Box<dyn ExecWorkflowCommandFrontend>;
}

/// The leader agent and model one evaluation will run with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLeader {
    pub agent: String,
    pub model: Option<String>,
}

/// Resolve the leader agent/model for one condition.
///
/// Precedence, highest first:
/// 1. the condition's own `agent` / `model` columns;
/// 2. `amie.defaultLeader` (`agent::model`) and `amie.agentsToModels`;
/// 3. the session's resolved `default_agent` (flags → repo → global config).
///
/// `agentsToModels` is a default *pool*, not an allowlist: a condition-level
/// agent that is absent from it still wins. When an agent is chosen without a
/// model, `agentsToModels` supplies that agent's first configured model.
pub fn resolve_leader(
    condition: &Condition,
    default_leader: Option<&str>,
    agents_to_models: Option<&std::collections::HashMap<String, Vec<String>>>,
    session_default_agent: Option<&str>,
) -> Result<ResolvedLeader, CommandError> {
    let fallback = default_leader.map(LeaderSpec::parse).transpose()?;

    let agent = condition
        .agent
        .clone()
        .or_else(|| fallback.as_ref().map(|spec| spec.agent.clone()))
        .or_else(|| session_default_agent.map(str::to_string))
        .ok_or_else(|| {
            CommandError::Other(format!(
                "condition {:?} has no agent: set the condition's agent, \
                 amie.defaultLeader, or a global defaultAgent",
                condition.name
            ))
        })?;

    // The model follows whichever level supplied it, then the configured pool
    // for the chosen agent.
    let model = condition.model.clone().or_else(|| {
        fallback
            .as_ref()
            .filter(|spec| spec.agent == agent)
            .map(|spec| spec.model.clone())
            .or_else(|| {
                agents_to_models
                    .and_then(|map| map.get(&agent))
                    .and_then(|models| models.first().cloned())
            })
    });

    Ok(ResolvedLeader { agent, model })
}

/// Evaluates one condition end to end.
pub struct LocalConditionEvaluator {
    engines: Engines,
    env: EnvSnapshot,
    frontends: Arc<dyn AmieRunFrontends>,
}

impl LocalConditionEvaluator {
    pub fn new(engines: Engines, env: EnvSnapshot, frontends: Arc<dyn AmieRunFrontends>) -> Self {
        Self {
            engines,
            env,
            frontends,
        }
    }

    /// Open a session rooted at the condition's captured mount scope. The scope
    /// is read from the stored condition and never recomputed or widened.
    fn open_session(&self, condition: &Condition) -> Result<Session, CommandError> {
        let repo_scope = condition.repo_scope.clone();
        let git_root = self
            .engines
            .git_engine
            .resolve_root(&repo_scope)
            .map_err(CommandError::from)?;
        let working_dir = match condition.mount_scope {
            MountScope::GitRoot => git_root.clone(),
            MountScope::Cwd => repo_scope,
        };
        Session::open_at_git_root(
            working_dir,
            git_root,
            SessionOpenOptions {
                env: Some(self.env.clone()),
                ..Default::default()
            },
        )
        .map_err(|error| CommandError::Other(format!("opening amie condition session: {error}")))
    }

    async fn evaluate_inner(
        &self,
        request: &EvaluationRequest,
    ) -> Result<EvaluationOutcome, CommandError> {
        // Defence in depth: the real refusal happens at daemon startup and at
        // condition creation, so reaching here under a sandbox tier means
        // something is mis-wired.
        require_container_tier(&self.engines)?;

        let condition = &request.condition;
        let session = self.open_session(condition)?;
        let git_root = session.git_root().to_path_buf();
        let dockerfiles = crate::data::RepoDockerfilePaths::new(&git_root);
        let available_agents = dockerfiles.discover_agent_dockerfiles();

        let mut sink = TracingSink::new(&condition.name);

        // The agent listing the leader sees: the configured pool where one is
        // set (validated against the repo's Dockerfiles), else discovery.
        let agents_section = match request
            .agents_to_models
            .as_ref()
            .filter(|map| !map.is_empty())
        {
            Some(map) => {
                let mut warnings = Vec::new();
                let effective =
                    build_effective_agents_to_models(map, &available_agents, &mut warnings)?;
                for warning in warnings {
                    sink.write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: warning,
                    });
                }
                format_agents_with_models(&effective)
            }
            None => format_available_agents(&available_agents),
        };

        let leader = resolve_leader(
            condition,
            request.default_leader.as_deref(),
            request.agents_to_models.as_ref(),
            session.default_agent().map(|a| a.as_str()),
        )?;
        let agent = AgentName::new(leader.agent.clone()).map_err(CommandError::Data)?;
        ensure_agent_image(
            &self.engines,
            &git_root,
            &dockerfiles,
            agent.as_str(),
            &mut sink,
        )?;

        // `guidance` is additive and never overridden by a condition, so it is
        // rendered into every leader prompt regardless of which level supplied
        // the agent and model.
        let prompt = build_amie_leader_prompt(
            &condition.name,
            &condition.description,
            "/workspace",
            &agents_section,
            request.guidance.as_deref(),
        );

        let launcher = AmieAgentLauncher::new(
            Arc::clone(&self.engines.agent_engine),
            Arc::clone(&self.engines.runtime),
        );
        let credential_env_vars = self
            .engines
            .auth_engine
            .resolve_agent_auth(&session, &agent)
            .map(|creds| creds.env_vars)
            .unwrap_or_default();

        let generated_path = request.condition_dir.join("workflow.toml");
        let mut repair = WorkflowRepairLoop::new(generated_path.clone(), prompt);

        let workflow = loop {
            let label = repair.label();
            let run_options = self.leader_run_options(
                repair.prompt(),
                leader.model.as_deref(),
                &git_root,
                agent.as_str(),
                &request.condition_dir,
            );
            launcher
                .run_leader(
                    LeaderRunSpec {
                        session: session.clone(),
                        agent: agent.clone(),
                        run_options,
                        credential_env_vars: credential_env_vars.clone(),
                        condition_name: condition.name.clone(),
                        condition_dir: request.condition_dir.clone(),
                    },
                    self.frontends.leader_frontend(&condition.name, &label),
                )
                .await
                .map_err(CommandError::from)?;

            // A first-attempt leader that wrote no workflow read the condition
            // as not met (it is instructed to default to "not triggered" when
            // uncertain). That is a normal outcome, not a validation failure —
            // only a *repair* attempt treats a missing file as one.
            if repair.is_first_attempt() && !generated_path.exists() {
                return Ok(EvaluationOutcome::NotTriggered);
            }

            match repair.record(validate_generated_workflow(
                &generated_path,
                &session,
                &dockerfiles,
            )) {
                RepairDecision::Accepted(workflow) => break *workflow,
                RepairDecision::Exhausted(message) => {
                    return Ok(EvaluationOutcome::Failed { error: message })
                }
                RepairDecision::Retry { attempt, error } => {
                    tracing::warn!(
                        condition = %condition.name,
                        attempt,
                        "amie: generated workflow failed validation: {error}"
                    );
                }
            }
        };

        // Record the engine's own state file on the run row *before* the
        // workflow starts, so the daemon's workflow route can serve live state.
        let workflow_name = crate::engine::workflow::workflow_name_for(&workflow);
        let state_path = self.workflow_state_path(&git_root, &workflow_name)?;
        request
            .progress
            .workflow_started(&request.run_id, &generated_path, &state_path);

        let outcome = ExecWorkflowCommand::new(
            amie_workflow_flags(&generated_path),
            self.engines.clone(),
            session,
        )
        .run_with_frontend(
            self.frontends
                .workflow_frontend(&condition.name, mount_scope_decision(condition.mount_scope)),
        )
        .await?;

        Ok(EvaluationOutcome::WorkflowExecuted {
            workflow_path: generated_path,
            workflow_state_path: Some(state_path),
            exit_code: outcome.exit_code,
        })
    }

    /// The leader's run options: yolo (so an unattended agent never blocks on a
    /// permission prompt), non-interactive, and the condition directory mounted
    /// read-write at the `context(workflow)` path.
    fn leader_run_options(
        &self,
        prompt: &str,
        model: Option<&str>,
        image_git_root: &Path,
        agent: &str,
        condition_dir: &Path,
    ) -> AgentRunOptions {
        AgentRunOptions {
            yolo: Some(YoloMode::Enabled),
            non_interactive: true,
            initial_prompt: Some(prompt.to_string()),
            model: model.map(str::to_string),
            image_tag_override: Some(crate::data::image_tags::agent_image_tag(
                image_git_root,
                agent,
            )),
            context_overlays: vec![ContextOverlay {
                scope: ContextScope::Workflow,
                host_path: condition_dir.to_path_buf(),
                container_path: PathBuf::from(CONDITION_DIR_CONTAINER_PATH),
                permission: OverlayPermission::ReadWrite,
            }],
            ..Default::default()
        }
    }

    /// The engine's `WorkflowStateStore` file for the run that is about to
    /// start. Worktree isolation is forced on, so the engine's session — and
    /// therefore its state store — is rooted at the workflow's worktree.
    fn workflow_state_path(
        &self,
        git_root: &Path,
        workflow_name: &str,
    ) -> Result<PathBuf, CommandError> {
        let lifecycle =
            crate::command::commands::worktree_lifecycle::WorktreeLifecycle::for_workflow(
                Arc::clone(&self.engines.git_engine),
                git_root.to_path_buf(),
                workflow_name,
            )?;
        let worktree = lifecycle.worktree_path().to_path_buf();
        // A linked worktree is its own git root; before it exists `resolve_root`
        // cannot say so, and the worktree path is the answer either way.
        let state_root = self
            .engines
            .git_engine
            .resolve_root(&worktree)
            .unwrap_or(worktree);
        Ok(EngineWorkflowStateStore::at_git_root(state_root).state_path(None, workflow_name))
    }
}

#[async_trait]
impl ConditionEvaluator for LocalConditionEvaluator {
    async fn evaluate(&self, request: EvaluationRequest) -> EvaluationOutcome {
        match self.evaluate_inner(&request).await {
            Ok(outcome) => outcome,
            // Never panic and never propagate: the scheduler records the error
            // and grows the condition's backoff.
            Err(error) => EvaluationOutcome::Failed {
                error: error.to_string(),
            },
        }
    }
}

/// The flags an amie-generated workflow always runs with: non-interactive (no
/// human is present), yolo (never block on an approval prompt), and worktree
/// isolation forced on so concurrent conditions touching one repo cannot
/// collide.
fn amie_workflow_flags(workflow_path: &Path) -> ExecWorkflowCommandFlags {
    ExecWorkflowCommandFlags {
        workflow: Some(workflow_path.to_path_buf()),
        work_item: None,
        non_interactive: true,
        plan: false,
        allow_docker: false,
        worktree: true,
        yolo: true,
        auto: false,
        agent: None,
        model: None,
        overlay: Vec::new(),
        max_concurrent: None,
        issue_source: Default::default(),
        dynamic: false,
        leader: None,
    }
}

/// Translate the stored mount scope into the answer the workflow frontend gives
/// when asked. The scope is captured at creation and is never widened here.
fn mount_scope_decision(scope: MountScope) -> MountScopeDecision {
    match scope {
        MountScope::GitRoot => MountScopeDecision::MountGitRoot,
        MountScope::Cwd => MountScopeDecision::MountCurrentDirOnly,
    }
}

/// A `UserMessageSink` for the unattended path: messages go to the daemon log.
struct TracingSink {
    condition: String,
}

impl TracingSink {
    fn new(condition: &str) -> Self {
        Self {
            condition: condition.to_string(),
        }
    }
}

impl UserMessageSink for TracingSink {
    fn write_message(&mut self, message: UserMessage) {
        match message.level {
            MessageLevel::Error => {
                tracing::error!(condition = %self.condition, "{}", message.text)
            }
            MessageLevel::Warning => {
                tracing::warn!(condition = %self.condition, "{}", message.text)
            }
            _ => tracing::info!(condition = %self.condition, "{}", message.text),
        }
    }
    fn replay_queued(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::collections::HashMap;

    fn condition(agent: Option<&str>, model: Option<&str>) -> Condition {
        let now = Utc::now();
        Condition {
            id: "id".into(),
            name: "issue-triage".into(),
            description: "when an issue opens, plan it".into(),
            repo_scope: PathBuf::from("/repo"),
            mount_scope: MountScope::GitRoot,
            interval_secs: 300,
            status: crate::data::fs::condition_store::ConditionStatus::Active,
            agent: agent.map(str::to_string),
            model: model.map(str::to_string),
            backoff_until: None,
            created_at: now,
            updated_at: now,
            last_run_at: None,
        }
    }

    fn pool() -> HashMap<String, Vec<String>> {
        HashMap::from([(
            "codex".to_string(),
            vec!["gpt-5".to_string(), "gpt-5-mini".to_string()],
        )])
    }

    #[test]
    fn a_condition_level_agent_and_model_beat_every_default() {
        let resolved = resolve_leader(
            &condition(Some("claude"), Some("claude-opus-5")),
            Some("codex::gpt-5"),
            Some(&pool()),
            Some("gemini"),
        )
        .unwrap();
        assert_eq!(resolved.agent, "claude");
        assert_eq!(resolved.model.as_deref(), Some("claude-opus-5"));
    }

    #[test]
    fn a_condition_level_agent_wins_even_when_absent_from_the_configured_pool() {
        // `agentsToModels` is a default pool, not an allowlist.
        let resolved =
            resolve_leader(&condition(Some("claude"), None), None, Some(&pool()), None).unwrap();
        assert_eq!(resolved.agent, "claude");
        assert_eq!(resolved.model, None);
    }

    #[test]
    fn default_leader_beats_the_global_default_agent() {
        let resolved = resolve_leader(
            &condition(None, None),
            Some("codex::gpt-5"),
            Some(&pool()),
            Some("gemini"),
        )
        .unwrap();
        assert_eq!(resolved.agent, "codex");
        assert_eq!(resolved.model.as_deref(), Some("gpt-5"));
    }

    #[test]
    fn the_global_default_agent_is_the_last_resort() {
        let resolved = resolve_leader(&condition(None, None), None, None, Some("gemini")).unwrap();
        assert_eq!(resolved.agent, "gemini");
        assert_eq!(resolved.model, None);
    }

    #[test]
    fn agents_to_models_supplies_a_model_for_an_agent_chosen_without_one() {
        let resolved =
            resolve_leader(&condition(Some("codex"), None), None, Some(&pool()), None).unwrap();
        assert_eq!(resolved.agent, "codex");
        assert_eq!(resolved.model.as_deref(), Some("gpt-5"));
    }

    #[test]
    fn a_condition_level_model_overrides_the_configured_pool() {
        let resolved = resolve_leader(
            &condition(Some("codex"), Some("gpt-5-mini")),
            None,
            Some(&pool()),
            None,
        )
        .unwrap();
        assert_eq!(resolved.model.as_deref(), Some("gpt-5-mini"));
    }

    #[test]
    fn no_agent_at_any_level_is_an_actionable_error() {
        let error = resolve_leader(&condition(None, None), None, None, None).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("issue-triage"), "{text}");
        assert!(text.contains("amie.defaultLeader"), "{text}");
    }

    #[test]
    fn a_malformed_default_leader_is_rejected() {
        let error = resolve_leader(&condition(None, None), Some("codex"), None, Some("gemini"))
            .unwrap_err();
        assert!(error.to_string().contains("agent::model"));
    }

    #[test]
    fn guidance_is_rendered_into_the_prompt_whichever_level_supplied_the_agent() {
        let guidance = vec!["always run the tests".to_string()];
        for (default_leader, session_default) in [
            (Some("codex::gpt-5"), None),
            (None, Some("gemini")),
            (None, None),
        ] {
            let condition = if default_leader.is_none() && session_default.is_none() {
                condition(Some("claude"), None)
            } else {
                condition(None, None)
            };
            let leader = resolve_leader(&condition, default_leader, None, session_default).unwrap();
            let prompt = build_amie_leader_prompt(
                &condition.name,
                &condition.description,
                "/workspace",
                &format!("  - {}", leader.agent),
                Some(&guidance),
            );
            assert!(
                prompt.contains("always run the tests"),
                "guidance must be additive regardless of which level chose the agent \
                 (leader {:?})",
                leader.agent
            );
            assert!(prompt.contains("issue-triage"));
        }
    }

    #[test]
    fn generated_workflows_always_force_worktree_isolation_and_unattended_guardrails() {
        let flags = amie_workflow_flags(Path::new("/c/workflow.toml"));
        assert!(flags.worktree, "unattended runs must be worktree-isolated");
        assert!(
            flags.yolo,
            "an unattended agent must never block on a prompt"
        );
        assert!(flags.non_interactive);
        assert!(!flags.allow_docker);
        assert!(!flags.dynamic, "the workflow is already generated");
        assert_eq!(
            flags.workflow.as_deref(),
            Some(Path::new("/c/workflow.toml"))
        );
    }

    #[test]
    fn the_captured_mount_scope_is_answered_verbatim_and_never_widened() {
        assert!(matches!(
            mount_scope_decision(MountScope::Cwd),
            MountScopeDecision::MountCurrentDirOnly
        ));
        assert!(matches!(
            mount_scope_decision(MountScope::GitRoot),
            MountScopeDecision::MountGitRoot
        ));
    }
}
