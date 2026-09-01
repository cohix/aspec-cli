//! Launching a task's leader agent in a container (Layer 1).
//!
//! [`SquadAgentLauncher`] is the engine-side counterpart to
//! `ExecWorkflowCommand`'s leader drive: it seeds the persistent task
//! directory with the dynamic-workflow reference assets, resolves the agent's
//! container options, stamps squad's identity (container name + the two labels)
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
use crate::data::fs::RunId;
use crate::data::session::{AgentName, Session};
use crate::data::RepoDockerfilePaths;
use crate::engine::agent::{AgentEngine, AgentRunOptions};
use crate::engine::agent_runtime::execution::{AgentExitInfo, AgentInstance};
use crate::engine::agent_runtime::frontend::AgentFrontend;
use crate::engine::agent_runtime::{AgentRuntimeEngine, ResolvedAgentOptions};
use crate::engine::container::naming::{generate_squad_container_name, validate_task_slug};
use crate::engine::container::options::ContainerName;
use crate::engine::error::EngineError;

/// The label key carrying the squad evaluation session id — the same key an
/// interactive session uses, so `docker ps` inspection is uniform.
pub const SESSION_LABEL_KEY: &str = "awman.session";
/// The label key carrying the task name, so every container a task's
/// evaluation (and generated workflow) launches is attributable to it.
pub const TASK_LABEL_KEY: &str = "awman.squad.task";

/// Return the directory that owns the output and metadata for one task run.
///
/// `task_dir` is the durable `<root>/tasks/<task>/workspace` directory, so
/// run data is deliberately its sibling rather than content in the workspace:
/// `<root>/tasks/<task>/runs/<run-id>/`.  This keeps transient execution
/// output out of the leader's durable working area.
pub fn run_log_dir(task_dir: &Path, run_id: &RunId) -> Result<std::path::PathBuf, EngineError> {
    let task_root = task_dir.parent().ok_or_else(|| {
        EngineError::Config(format!(
            "task workspace {} has no task-directory parent",
            task_dir.display()
        ))
    })?;
    Ok(task_root.join("runs").join(run_id.as_str()))
}

/// Create a run's log directory before any container for that run is started.
/// The caller invokes this synchronously after reserving the [`RunId`] and
/// before dispatching evaluation, so output draining never has to create
/// directories lazily on its first byte.
pub fn prepare_run_log_dir(
    task_dir: &Path,
    run_id: &RunId,
) -> Result<std::path::PathBuf, EngineError> {
    let dir = run_log_dir(task_dir, run_id)?;
    std::fs::create_dir_all(&dir).map_err(|error| EngineError::io(&dir, error))?;
    Ok(dir)
}

/// The container-name and label identity every container a task launches
/// must carry. One implementation, used by the evaluation leader
/// ([`SquadAgentLauncher::run_leader`]) and by every step of the workflow that
/// leader generates (`ExecWorkflowCommand`, via `with_squad_identity`), so a
/// task's whole container set shares one discoverable name prefix and the
/// two attribution labels.
#[derive(Debug, Clone)]
pub struct SquadContainerIdentity {
    pub task_name: String,
    pub session_id: String,
}

impl SquadContainerIdentity {
    pub fn new(task_name: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            task_name: task_name.into(),
            session_id: session_id.into(),
        }
    }

    /// Stamp a fresh `awman-squad-<slug>-<8 hex>` name and the `awman.session` /
    /// `awman.squad.task` labels onto resolved options. Rejects the sandbox
    /// variant, exactly as `run_leader` did before this was extracted: squad is
    /// container-only, so a mis-wired caller fails loudly rather than launching
    /// an unlabelled, undiscoverable container.
    pub fn stamp(
        &self,
        resolved: ResolvedAgentOptions,
    ) -> Result<ResolvedAgentOptions, EngineError> {
        match resolved {
            ResolvedAgentOptions::Container(mut options) => {
                let container_name = generate_squad_container_name(&self.task_name);
                options.name = Some(ContainerName::new(container_name));
                options
                    .labels
                    .push((SESSION_LABEL_KEY.to_string(), self.session_id.clone()));
                options
                    .labels
                    .push((TASK_LABEL_KEY.to_string(), self.task_name.clone()));
                Ok(ResolvedAgentOptions::Container(options))
            }
            ResolvedAgentOptions::Sandbox(_) => Err(EngineError::Config(
                "squad requires a container runtime; the sandbox tier cannot host \
                 task evaluation"
                    .to_string(),
            )),
        }
    }
}

/// Everything the launcher needs to run one task's leader agent.
///
/// The caller (Layer 2) has already opened `session` rooted at the repo's
/// captured `mount_scope` and resolved `run_options` — including the task
/// directory's context overlay and the assembled leader prompt. The launcher
/// mounts nothing beyond what those two imply: the repo (via the session) and
/// the task directory (via the run's context overlay), never a parent.
pub struct LeaderRunSpec {
    /// Session rooted at the repo mount scope captured when the task was
    /// created. The repo is mounted from `session.git_root()`; the label's
    /// session id is `session.id()`.
    pub session: Session,
    /// The leader agent to run.
    pub agent: AgentName,
    /// Fully-resolved run options (prompt, model, non-interactive, yolo,
    /// task-directory context overlay, image tag override).
    pub run_options: AgentRunOptions,
    /// Credential env vars injected into the container at startup only — never
    /// written into the persistent task directory.
    pub credential_env_vars: Vec<(String, String)>,
    /// The task name; used both for the container name slug and for the
    /// `awman.squad.task` label.
    pub task_name: String,
    /// The task's persistent context directory, seeded before launch.
    pub task_dir: std::path::PathBuf,
}

/// Scaffold a plain-directory task root so agent images can be resolved from
/// it, exactly the way any other awman project root resolves them.
///
/// A repository-backed task inherits its project's `Dockerfile.dev` and
/// `.awman/Dockerfile.<agent>`, so `ensure_agent_image` has something to build
/// from. A [`MountScope::Directory`](crate::data::fs::task_store::MountScope)
/// task's root is a plain directory — the durable task workspace, or a custom
/// folder that is not a repository — and starts with neither, so without this
/// the very first evaluation of a default-workspace task fails before the
/// leader is ever launched.
///
/// Every write is **create-if-missing**. The durable workspace belongs to the
/// task for its whole lifetime (WI 0106 §6a), so a Dockerfile the user (or the
/// leader) has since edited is never overwritten — this only fills in what is
/// absent, from the same bundled templates `awman init` writes into a fresh
/// repository.
///
/// Agents with no bundled template are skipped: `ensure_agent_image`'s existing
/// "agent has no Dockerfile" error is the right report for those.
pub fn ensure_directory_workspace_project(
    root: &Path,
    agents: &[String],
) -> Result<(), EngineError> {
    std::fs::create_dir_all(root).map_err(|e| EngineError::io(root, e))?;
    let paths = RepoDockerfilePaths::new(root);

    let project = paths.project_dockerfile();
    if !project.exists() {
        std::fs::write(&project, crate::data::templates::project_dockerfile_dev())
            .map_err(|e| EngineError::io(&project, e))?;
    }

    let base_tag = crate::data::image_tags::project_image_tag(root);
    let awman_dir = paths.awman_dir();
    for agent in agents {
        let Some(template) = crate::data::templates::agent_dockerfile_for(agent) else {
            continue;
        };
        let dest = paths.agent_dockerfile(agent);
        if dest.exists() {
            continue;
        }
        std::fs::create_dir_all(&awman_dir).map_err(|e| EngineError::io(&awman_dir, e))?;
        std::fs::write(&dest, template.replace("{{AWMAN_BASE_IMAGE}}", &base_tag))
            .map_err(|e| EngineError::io(&dest, e))?;
    }
    Ok(())
}

/// Runs a task's leader agent in a container.
pub struct SquadAgentLauncher {
    agent_engine: Arc<AgentEngine>,
    runtime: Arc<dyn AgentRuntimeEngine>,
}

impl SquadAgentLauncher {
    pub fn new(agent_engine: Arc<AgentEngine>, runtime: Arc<dyn AgentRuntimeEngine>) -> Self {
        Self {
            agent_engine,
            runtime,
        }
    }

    /// Seed, launch, and await the leader agent, returning its exit info.
    ///
    /// Seeding is idempotent: the task directory is created once and never
    /// recreated per run (`context(global)` semantics). Leader-written files,
    /// including a prior `workflow.toml`, are preserved; only the static
    /// reference assets are rewritten before each launch.
    pub async fn run_leader(
        &self,
        spec: LeaderRunSpec,
        frontend: Box<dyn AgentFrontend>,
    ) -> Result<AgentExitInfo, EngineError> {
        // `naming.rs` documents slug validation as the caller's responsibility.
        // Re-check it here so this primitive is self-defending: today the only
        // insertion path validates at task creation, but a future second
        // one must not be able to defeat the container-name guarantee.
        validate_task_slug(&spec.task_name)
            .map_err(|error| EngineError::Config(error.to_string()))?;

        Self::seed_task_dir(&spec.task_dir)?;

        let resolved = self.agent_engine.resolve_agent_options(
            &spec.session,
            &spec.agent,
            &spec.run_options,
            &spec.credential_env_vars,
            self.runtime.as_ref(),
        )?;

        // Stamp squad's identity onto the resolved options: a deterministic,
        // parseable container name and the two attribution labels. The same
        // identity is applied to every generated-workflow step container, so it
        // lives in exactly one place — [`SquadContainerIdentity::stamp`].
        let identity =
            SquadContainerIdentity::new(spec.task_name.clone(), spec.session.id().to_string());
        let resolved = identity.stamp(resolved)?;

        let instance: Box<dyn AgentInstance> = self.runtime.build(resolved)?;
        let mut execution = instance.run_with_frontend(frontend)?;
        execution.wait().await
    }

    /// Write the dynamic-workflow reference assets into the durable task
    /// workspace.
    ///
    /// **This must never delete or truncate anything the leader or its
    /// workflow wrote.** The task workspace is durable for the task's whole
    /// lifetime (WI 0106 §6a): it is created once and only removed when the
    /// task itself is removed. Before WI 0106 this function deleted
    /// `workflow.toml` before every run, because a leftover file would
    /// otherwise be misread as "this run triggered". That inference is gone —
    /// the leader now reports its verdict explicitly through the per-run
    /// [`verdict`](crate::engine::squad::verdict) file — so the deletion is
    /// gone with it, and the leader is free to reuse a `workflow.toml` it
    /// wrote on an earlier run.
    ///
    /// What remains is idempotently rewriting the two *static* reference
    /// assets. They are read-only documentation shipped with the binary, not
    /// task state, so rewriting them cannot lose anything.
    fn seed_task_dir(dir: &Path) -> Result<(), EngineError> {
        std::fs::create_dir_all(dir).map_err(|e| EngineError::io(dir, e))?;
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
    fn seed_writes_reference_assets_and_preserves_leader_written_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tasks").join("issue-triage");
        // The durable workspace survives between runs: a `workflow.toml` (and
        // anything else) the leader wrote on an earlier run must still be
        // there afterwards. The leader is explicitly allowed to reuse it.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("workflow.toml"), "from a previous run").unwrap();
        std::fs::write(dir.join("leader-notes.md"), "durable state").unwrap();

        SquadAgentLauncher::seed_task_dir(&dir).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.join("workflow.toml")).unwrap(),
            "from a previous run",
            "seeding must never delete or rewrite the leader's workflow.toml"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("leader-notes.md")).unwrap(),
            "durable state",
            "seeding must never touch any other leader-written file"
        );
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
        SquadAgentLauncher::seed_task_dir(&dir).unwrap();
        // A second seed over an existing directory must not error.
        SquadAgentLauncher::seed_task_dir(&dir).unwrap();
        assert!(dir.join("example-workflow.toml").exists());
    }

    // ── BLOCKER-1: one stamping implementation for every task container ──

    use crate::engine::container::naming::parse_squad_task_slug;

    #[test]
    fn stamp_applies_a_parseable_squad_name_and_both_labels() {
        let identity = SquadContainerIdentity::new("issue-triage", "sess-123");
        let resolved = ResolvedAgentOptions::container(Vec::new()).unwrap();
        let ResolvedAgentOptions::Container(options) = identity.stamp(resolved).unwrap() else {
            panic!("stamp must keep the container variant");
        };
        let name = options.name.expect("stamp must set a container name");
        let name = name.as_str();
        assert!(
            name.starts_with("awman-squad-issue-triage-"),
            "unexpected container name: {name}"
        );
        assert_eq!(
            parse_squad_task_slug(name),
            Some("issue-triage"),
            "the squad name must round-trip back to the task slug"
        );
        assert!(options
            .labels
            .iter()
            .any(|(k, v)| k == SESSION_LABEL_KEY && v == "sess-123"));
        assert!(options
            .labels
            .iter()
            .any(|(k, v)| k == TASK_LABEL_KEY && v == "issue-triage"));
    }

    #[test]
    fn stamp_generates_a_fresh_name_each_call() {
        // Every generated-workflow step gets its own container, so two stamps
        // of the same identity must not collide on the unique suffix.
        let identity = SquadContainerIdentity::new("release-notes", "s");
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
        let identity = SquadContainerIdentity::new("issue-triage", "s");
        let sandbox = ResolvedAgentOptions::sandbox(Vec::new());
        let error = identity.stamp(sandbox).unwrap_err();
        assert!(
            error.to_string().contains("container runtime"),
            "sandbox refusal must explain the container requirement: {error}"
        );
    }

    // ── Directory-workspace image sources (WI 0106 remediation, B1) ────────

    /// A plain directory task root starts with no `Dockerfile.dev` and no
    /// `.awman/Dockerfile.<agent>`, so `ensure_agent_image` has nothing to
    /// build from and a default-workspace task cannot launch its leader at
    /// all. Scaffolding fills both in from the bundled templates.
    #[test]
    fn a_plain_directory_workspace_gains_the_image_sources_it_needs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");

        ensure_directory_workspace_project(&root, &["claude".to_string()]).unwrap();

        let paths = RepoDockerfilePaths::new(&root);
        assert!(
            paths.project_dockerfile().exists(),
            "the base image needs a Dockerfile.dev at the task root"
        );
        let agent_dockerfile = paths.agent_dockerfile("claude");
        assert!(agent_dockerfile.exists());
        let body = std::fs::read_to_string(&agent_dockerfile).unwrap();
        assert!(
            !body.contains("{{AWMAN_BASE_IMAGE}}"),
            "the base-image placeholder must be substituted, as it is on the \
             `awman init` path: {body}"
        );
        assert!(
            body.contains(&crate::data::image_tags::project_image_tag(&root)),
            "the agent image must layer on this task root's own base image"
        );
        assert_eq!(
            paths
                .discover_agent_dockerfiles()
                .into_iter()
                .map(|(agent, _)| agent)
                .collect::<Vec<_>>(),
            vec!["claude".to_string()],
            "the scaffolded Dockerfile must be discoverable for the leader's \
             agent listing and for workflow validation"
        );
    }

    /// The durable workspace belongs to the task for its whole lifetime, so
    /// scaffolding is create-if-missing: an edited Dockerfile survives every
    /// later run untouched.
    #[test]
    fn scaffolding_never_overwrites_an_existing_dockerfile() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let paths = RepoDockerfilePaths::new(&root);
        std::fs::create_dir_all(paths.awman_dir()).unwrap();
        std::fs::write(paths.project_dockerfile(), "FROM my-own-base\n").unwrap();
        std::fs::write(paths.agent_dockerfile("claude"), "FROM my-own-agent\n").unwrap();

        ensure_directory_workspace_project(&root, &["claude".to_string()]).unwrap();
        ensure_directory_workspace_project(&root, &["claude".to_string()]).unwrap();

        assert_eq!(
            std::fs::read_to_string(paths.project_dockerfile()).unwrap(),
            "FROM my-own-base\n"
        );
        assert_eq!(
            std::fs::read_to_string(paths.agent_dockerfile("claude")).unwrap(),
            "FROM my-own-agent\n"
        );
    }

    /// An agent with no bundled template is left to `ensure_agent_image`'s
    /// existing "agent has no Dockerfile" error rather than being invented.
    #[test]
    fn an_unknown_agent_is_left_for_the_existing_missing_dockerfile_error() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_directory_workspace_project(tmp.path(), &["not-a-real-agent".to_string()]).unwrap();
        assert!(!RepoDockerfilePaths::new(tmp.path())
            .agent_dockerfile("not-a-real-agent")
            .exists());
    }
}
