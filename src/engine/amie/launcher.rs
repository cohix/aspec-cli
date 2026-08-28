//! Launching a condition's leader agent in a container (Layer 1).
//!
//! [`AmieAgentLauncher`] is the engine-side counterpart to
//! `ExecWorkflowCommand`'s leader drive: it seeds the persistent condition
//! directory with the dynamic-workflow reference assets, resolves the agent's
//! container options, stamps amie's identity (container name + the two labels)
//! onto them, and runs the agent through `Arc<dyn AgentRuntimeEngine>`.
//!
//! It deliberately does *not* build the [`AgentRunOptions`] or resolve the
//! leader prompt — those need Layer-2 config resolution and are handed in via
//! [`LeaderRunSpec`]. What is genuinely Layer 1 lives here: filesystem seeding,
//! option resolution, container naming, label attachment, and the
//! build → run → wait cycle.

use std::path::Path;
use std::sync::Arc;

use crate::data::dynamic_workflow_assets::{EXAMPLE_WORKFLOW_TOML, WORKFLOW_USAGE_MD};
use crate::data::session::{AgentName, Session};
use crate::engine::agent::{AgentEngine, AgentRunOptions};
use crate::engine::agent_runtime::execution::{AgentExitInfo, AgentInstance};
use crate::engine::agent_runtime::frontend::AgentFrontend;
use crate::engine::agent_runtime::{AgentRuntimeEngine, ResolvedAgentOptions};
use crate::engine::container::naming::{generate_amie_container_name, validate_condition_slug};
use crate::engine::container::options::ContainerName;
use crate::engine::error::EngineError;

/// The label key carrying the amie evaluation session id — the same key an
/// interactive session uses, so `docker ps` inspection is uniform.
pub const SESSION_LABEL_KEY: &str = "awman.session";
/// The label key carrying the condition name, so every container a condition's
/// evaluation (and generated workflow) launches is attributable to it.
pub const CONDITION_LABEL_KEY: &str = "awman.amie.condition";

/// The container-name and label identity every container a condition launches
/// must carry. One implementation, used by the evaluation leader
/// ([`AmieAgentLauncher::run_leader`]) and by every step of the workflow that
/// leader generates (`ExecWorkflowCommand`, via `with_amie_identity`), so a
/// condition's whole container set shares one discoverable name prefix and the
/// two attribution labels.
#[derive(Debug, Clone)]
pub struct AmieContainerIdentity {
    pub condition_name: String,
    pub session_id: String,
}

impl AmieContainerIdentity {
    pub fn new(condition_name: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            condition_name: condition_name.into(),
            session_id: session_id.into(),
        }
    }

    /// Stamp a fresh `awman-amie-<slug>-<8 hex>` name and the `awman.session` /
    /// `awman.amie.condition` labels onto resolved options. Rejects the sandbox
    /// variant, exactly as `run_leader` did before this was extracted: amie is
    /// container-only, so a mis-wired caller fails loudly rather than launching
    /// an unlabelled, undiscoverable container.
    pub fn stamp(
        &self,
        resolved: ResolvedAgentOptions,
    ) -> Result<ResolvedAgentOptions, EngineError> {
        match resolved {
            ResolvedAgentOptions::Container(mut options) => {
                let container_name = generate_amie_container_name(&self.condition_name);
                options.name = Some(ContainerName::new(container_name));
                options
                    .labels
                    .push((SESSION_LABEL_KEY.to_string(), self.session_id.clone()));
                options
                    .labels
                    .push((CONDITION_LABEL_KEY.to_string(), self.condition_name.clone()));
                Ok(ResolvedAgentOptions::Container(options))
            }
            ResolvedAgentOptions::Sandbox(_) => Err(EngineError::Config(
                "amie requires a container runtime; the sandbox tier cannot host \
                 condition evaluation"
                    .to_string(),
            )),
        }
    }
}

/// Everything the launcher needs to run one condition's leader agent.
///
/// The caller (Layer 2) has already opened `session` rooted at the repo's
/// captured `mount_scope` and resolved `run_options` — including the condition
/// directory's context overlay and the assembled leader prompt. The launcher
/// mounts nothing beyond what those two imply: the repo (via the session) and
/// the condition directory (via the run's context overlay), never a parent.
pub struct LeaderRunSpec {
    /// Session rooted at the repo mount scope captured when the condition was
    /// created. The repo is mounted from `session.git_root()`; the label's
    /// session id is `session.id()`.
    pub session: Session,
    /// The leader agent to run.
    pub agent: AgentName,
    /// Fully-resolved run options (prompt, model, non-interactive, yolo,
    /// condition-directory context overlay, image tag override).
    pub run_options: AgentRunOptions,
    /// Credential env vars injected into the container at startup only — never
    /// written into the persistent condition directory.
    pub credential_env_vars: Vec<(String, String)>,
    /// The condition name; used both for the container name slug and for the
    /// `awman.amie.condition` label.
    pub condition_name: String,
    /// The condition's persistent context directory, seeded before launch.
    pub condition_dir: std::path::PathBuf,
}

/// Runs a condition's leader agent in a container.
pub struct AmieAgentLauncher {
    agent_engine: Arc<AgentEngine>,
    runtime: Arc<dyn AgentRuntimeEngine>,
}

impl AmieAgentLauncher {
    pub fn new(agent_engine: Arc<AgentEngine>, runtime: Arc<dyn AgentRuntimeEngine>) -> Self {
        Self {
            agent_engine,
            runtime,
        }
    }

    /// Seed, launch, and await the leader agent, returning its exit info.
    ///
    /// Seeding is idempotent: the condition directory is created once and never
    /// recreated per run (`context(global)` semantics), but any stale
    /// `workflow.toml` from a previous attempt is removed and the reference
    /// assets are rewritten before every launch — exactly as
    /// `exec workflow --dynamic` does for `context(workflow)`.
    pub async fn run_leader(
        &self,
        spec: LeaderRunSpec,
        frontend: Box<dyn AgentFrontend>,
    ) -> Result<AgentExitInfo, EngineError> {
        // `naming.rs` documents slug validation as the caller's responsibility.
        // Re-check it here so this primitive is self-defending: today the only
        // insertion path validates at condition creation, but a future second
        // one must not be able to defeat the container-name guarantee.
        validate_condition_slug(&spec.condition_name)
            .map_err(|error| EngineError::Config(error.to_string()))?;

        Self::seed_condition_dir(&spec.condition_dir)?;

        let resolved = self.agent_engine.resolve_agent_options(
            &spec.session,
            &spec.agent,
            &spec.run_options,
            &spec.credential_env_vars,
            self.runtime.as_ref(),
        )?;

        // Stamp amie's identity onto the resolved options: a deterministic,
        // parseable container name and the two attribution labels. The same
        // identity is applied to every generated-workflow step container, so it
        // lives in exactly one place — [`AmieContainerIdentity::stamp`].
        let identity =
            AmieContainerIdentity::new(spec.condition_name.clone(), spec.session.id().to_string());
        let resolved = identity.stamp(resolved)?;

        let instance: Box<dyn AgentInstance> = self.runtime.build(resolved)?;
        let mut execution = instance.run_with_frontend(frontend)?;
        execution.wait().await
    }

    /// Write the dynamic-workflow reference assets into the condition directory
    /// and clear any stale generated workflow. Mirrors
    /// `exec_workflow.rs`'s context-dir seeding so the leader sees the same
    /// files under either entry point.
    fn seed_condition_dir(dir: &Path) -> Result<(), EngineError> {
        std::fs::create_dir_all(dir).map_err(|e| EngineError::io(dir, e))?;
        // Remove any stale generated workflow so a failed prior attempt cannot
        // be mistaken for this attempt's output.
        let _ = std::fs::remove_file(dir.join("workflow.toml"));
        let example = dir.join("example-workflow.toml");
        std::fs::write(&example, EXAMPLE_WORKFLOW_TOML)
            .map_err(|e| EngineError::io(&example, e))?;
        let usage = dir.join("workflow-usage.md");
        std::fs::write(&usage, WORKFLOW_USAGE_MD).map_err(|e| EngineError::io(&usage, e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_writes_assets_and_clears_stale_workflow() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("conditions").join("issue-triage");
        // A stale workflow.toml from a previous attempt must not survive.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("workflow.toml"), "stale").unwrap();

        AmieAgentLauncher::seed_condition_dir(&dir).unwrap();

        assert!(!dir.join("workflow.toml").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("example-workflow.toml")).unwrap(),
            EXAMPLE_WORKFLOW_TOML
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("workflow-usage.md")).unwrap(),
            WORKFLOW_USAGE_MD
        );
    }

    #[test]
    fn seed_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("c");
        AmieAgentLauncher::seed_condition_dir(&dir).unwrap();
        // A second seed over an existing directory must not error.
        AmieAgentLauncher::seed_condition_dir(&dir).unwrap();
        assert!(dir.join("example-workflow.toml").exists());
    }

    // ── BLOCKER-1: one stamping implementation for every condition container ──

    use crate::engine::container::naming::parse_amie_condition_slug;

    #[test]
    fn stamp_applies_a_parseable_amie_name_and_both_labels() {
        let identity = AmieContainerIdentity::new("issue-triage", "sess-123");
        let resolved = ResolvedAgentOptions::container(Vec::new()).unwrap();
        let ResolvedAgentOptions::Container(options) = identity.stamp(resolved).unwrap() else {
            panic!("stamp must keep the container variant");
        };
        let name = options.name.expect("stamp must set a container name");
        let name = name.as_str();
        assert!(
            name.starts_with("awman-amie-issue-triage-"),
            "unexpected container name: {name}"
        );
        assert_eq!(
            parse_amie_condition_slug(name),
            Some("issue-triage"),
            "the amie name must round-trip back to the condition slug"
        );
        assert!(options
            .labels
            .iter()
            .any(|(k, v)| k == SESSION_LABEL_KEY && v == "sess-123"));
        assert!(options
            .labels
            .iter()
            .any(|(k, v)| k == CONDITION_LABEL_KEY && v == "issue-triage"));
    }

    #[test]
    fn stamp_generates_a_fresh_name_each_call() {
        // Every generated-workflow step gets its own container, so two stamps
        // of the same identity must not collide on the unique suffix.
        let identity = AmieContainerIdentity::new("release-notes", "s");
        let first = match identity
            .stamp(ResolvedAgentOptions::container(Vec::new()).unwrap())
            .unwrap()
        {
            ResolvedAgentOptions::Container(o) => o.name.unwrap().as_str().to_string(),
            _ => unreachable!(),
        };
        let second = match identity
            .stamp(ResolvedAgentOptions::container(Vec::new()).unwrap())
            .unwrap()
        {
            ResolvedAgentOptions::Container(o) => o.name.unwrap().as_str().to_string(),
            _ => unreachable!(),
        };
        assert_ne!(first, second, "each stamp must mint a fresh unique suffix");
    }

    #[test]
    fn stamp_rejects_the_sandbox_variant() {
        let identity = AmieContainerIdentity::new("issue-triage", "s");
        let sandbox = ResolvedAgentOptions::sandbox(Vec::new());
        let error = identity.stamp(sandbox).unwrap_err();
        assert!(
            error.to_string().contains("container runtime"),
            "sandbox refusal must explain the container requirement: {error}"
        );
    }
}
