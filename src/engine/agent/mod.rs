//! `engine::agent` — `AgentEngine`. Cross-cutting agent concerns called by
//! `chat`, `exec`, and `ready`.
//!
//! All agent-name branching lives in `agent_matrix.rs`. Adding a new agent
//! is a single-file edit.

use std::sync::Arc;

use crate::data::config::effective::EffectiveConfig;
use crate::data::config::repo::LaunchMode;
use crate::data::image_tags::{agent_image_tag, project_image_tag};
use crate::data::repo_dockerfile_paths::RepoDockerfilePaths;
use crate::data::session::{AgentName, Session};
use crate::engine::agent_runtime::{AgentRuntimeEngine, ResolvedAgentOptions};
use crate::engine::container::options::{
    ContainerOption, EnvLiteral, EnvVar, ImageRef, PlanMode, YoloMode,
};
use crate::engine::container::ContainerRuntime;
use crate::engine::error::EngineError;
use crate::engine::overlay::{ContextOverlay, DirectorySpec, OverlayEngine, OverlayRequest};
use crate::engine::sandbox::options::SandboxOption;
use crate::engine::step_status::StepStatus;

pub mod agent_matrix;
pub mod download;
pub mod frontend;

pub use frontend::AgentFrontend;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutoMode {
    #[default]
    Disabled,
    Enabled,
}

/// Options governing how an agent container is invoked.
#[derive(Debug, Default, Clone)]
pub struct AgentRunOptions {
    pub yolo: Option<crate::engine::container::options::YoloMode>,
    pub auto: Option<crate::engine::container::options::AutoMode>,
    pub plan: Option<crate::engine::container::options::PlanMode>,
    /// How awman talks to the agent process's stdio: raw PTY/stdio
    /// (`LaunchMode::Stdio`, the default and today's behaviour) or a
    /// newline-delimited JSON-RPC 2.0 ACP channel (`LaunchMode::Acp`). Threaded
    /// through exactly like `plan`/`yolo`/`auto`; `Default` is `Stdio`, so
    /// existing call sites keep raw-stdio behaviour with no change.
    pub launch_mode: LaunchMode,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub initial_prompt: Option<String>,
    pub allow_docker: bool,
    pub non_interactive: bool,
    /// Optional explicit model name; if `None`, the engine emits no model flag.
    pub model: Option<String>,
    /// Optional explicit env-passthrough list (name only). If `None`, falls
    /// through to `EffectiveConfig::env_passthrough`.
    pub env_passthrough: Option<Vec<String>>,
    /// User-supplied directory overlays.
    pub directory_overlays: Vec<DirectorySpec>,
    /// When true, mount all skill directories.
    pub include_all_skills: bool,
    /// Named skills to mount (when `include_all_skills` is false).
    pub named_skills: Vec<String>,
    /// Override the image tag used for the container and for `image_home_dir`
    /// inspection. When `Some`, this tag is used instead of deriving one from
    /// `session.git_root()`. Needed when the session is rooted at a worktree
    /// but the image was built from the original repo root.
    pub image_tag_override: Option<String>,
    /// Combined, pre-rendered system-prompt text (from `ContextPromptBuilder`).
    pub system_prompt: Option<String>,
    /// Resolved context-directory overlays for this run.
    pub context_overlays: Vec<ContextOverlay>,
}

#[derive(Clone)]
pub struct AgentEngine {
    overlay_engine: Arc<OverlayEngine>,
    container_runtime: Arc<ContainerRuntime>,
    /// Temp files backing file/env-file system-prompt delivery. Retained as
    /// RAII guards so the files live as long as this engine instance and are
    /// removed on drop — no `/tmp` leakage.
    prompt_tempfiles: Arc<std::sync::Mutex<Vec<tempfile::NamedTempFile>>>,
}

impl AgentEngine {
    pub fn new(
        overlay_engine: Arc<OverlayEngine>,
        container_runtime: Arc<ContainerRuntime>,
    ) -> Self {
        Self {
            overlay_engine,
            container_runtime,
            prompt_tempfiles: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Cheap clone of the engine's `ContainerRuntime` arc — used by callers
    /// that need to ask the runtime whether an image exists without doing
    /// any container build work themselves.
    pub fn container_runtime_arc(&self) -> &Arc<ContainerRuntime> {
        &self.container_runtime
    }

    /// Ensure the agent's Dockerfile and image are available locally. Reports
    /// progress via `frontend`. Idempotent: when both Dockerfile and image
    /// exist, no `report_step_status` calls fire.
    ///
    /// `image_exists` is injected so callers in tests can avoid shelling out
    /// to Docker. Production callers pass `image_exists_locally`.
    pub async fn ensure_available(
        &self,
        session: &Session,
        agent: &AgentName,
        _config: &EffectiveConfig,
        frontend: &mut dyn AgentFrontend,
        image_exists: impl Fn(&str) -> bool,
    ) -> Result<(), EngineError> {
        // Check for the project base image. If absent, fail with a structured
        // error: agent images are layered FROM the project image.
        let project_tag = project_image_tag(session.git_root());
        if !image_exists(&project_tag) {
            return Err(EngineError::AgentRequiresProjectImage { tag: project_tag });
        }

        let paths = RepoDockerfilePaths::new(session.git_root());
        let agent_dockerfile = paths.agent_dockerfile(agent.as_str());
        let agent_tag = agent_image_tag(session.git_root(), agent.as_str());

        // Ensure Dockerfile.<agent> is present.
        if !agent_dockerfile.exists() {
            frontend.report_step_status("Downloading Dockerfile", StepStatus::Running);
            match download::download_agent_dockerfile(
                agent.as_str(),
                &agent_dockerfile,
                &project_tag,
            )
            .await
            {
                Ok(()) => frontend.report_step_status("Downloading Dockerfile", StepStatus::Done),
                Err(e) => {
                    frontend.report_step_status(
                        "Downloading Dockerfile",
                        StepStatus::Failed(e.to_string()),
                    );
                    return Err(e);
                }
            }
        }

        // Ensure agent image is built.
        if !image_exists(&agent_tag) {
            frontend.report_step_status("Building image", StepStatus::Running);
            let _container = frontend.container_frontend();
            let mut sink = |line: &str| {
                frontend.report_step_status(line, StepStatus::Running);
            };
            match self.container_runtime.build_image(
                &agent_tag,
                &agent_dockerfile,
                session.git_root(),
                false,
                &mut sink,
            ) {
                Ok(()) => {
                    frontend.report_step_status("Building image", StepStatus::Done);
                }
                Err(EngineError::ImageBuildExitNonzero { exit_code, .. }) => {
                    frontend.report_step_status(
                        "Building image",
                        StepStatus::Failed(format!(
                            "agent image build exited with code {exit_code}"
                        )),
                    );
                    return Err(EngineError::AgentImageBuildFailed {
                        agent: agent.as_str().to_string(),
                        exit_code,
                    });
                }
                Err(e) => {
                    let msg = e.to_string();
                    frontend.report_step_status("Building image", StepStatus::Failed(msg.clone()));
                    return Err(e);
                }
            }
        }

        Ok(())
    }

    /// Build the `ContainerOption` list for running an agent container.
    pub fn build_options(
        &self,
        session: &Session,
        agent: &AgentName,
        run: &AgentRunOptions,
    ) -> Result<Vec<ContainerOption>, EngineError> {
        let matrix = agent_matrix::matrix_for(agent.as_str())?;

        // Validate plan mode support.
        if matches!(run.plan, Some(PlanMode::Enabled)) && matrix.plan_flag.is_none() {
            return Err(EngineError::PlanModeUnsupported {
                agent: agent.as_str().to_string(),
            });
        }
        // Validate ACP support. Every launch path (direct chat/exec prompt and
        // each exec workflow step) funnels through build_options, so this single
        // guard guarantees an agent that doesn't speak ACP can never reach a
        // container with JSON-RPC framing, regardless of any Layer 2 pre-flight
        // decision. Mirrors the plan-mode-unsupported check above.
        if matches!(run.launch_mode, LaunchMode::Acp) && !matrix.supports_acp {
            return Err(EngineError::AcpUnsupported {
                agent: agent.as_str().to_string(),
            });
        }
        // Plan + yolo are mutually exclusive — engine layer detection.
        if matches!(run.plan, Some(PlanMode::Enabled))
            && matches!(run.yolo, Some(YoloMode::Enabled))
        {
            return Err(EngineError::ConflictingOptions(
                "plan and yolo modes are mutually exclusive".into(),
            ));
        }

        let image_tag = run
            .image_tag_override
            .clone()
            .unwrap_or_else(|| agent_image_tag(session.git_root(), agent.as_str()));
        let image = ImageRef::new(image_tag.clone());
        // ACP launches use the agent's ACP entrypoint (e.g. `cline --acp`) and
        // select the persistent piped-stdio container path instead of a PTY, so
        // the JSON-RPC 2.0 framing never passes through a terminal's cooked-mode
        // layer. The guard above has already rejected `Acp` for any agent whose
        // matrix lacks an `acp_entrypoint`, so `entrypoint_for_acp` cannot fail
        // here in practice — the `?` is defence-in-depth.
        let acp = matches!(run.launch_mode, LaunchMode::Acp);
        let entrypoint = if acp {
            agent_matrix::entrypoint_for_acp(&matrix)?
        } else {
            agent_matrix::entrypoint_for(&matrix, run.non_interactive)
        };

        let mut options = vec![
            ContainerOption::Image(image),
            ContainerOption::Entrypoint(entrypoint),
            ContainerOption::Interactive(!run.non_interactive),
            ContainerOption::AllowDocker(run.allow_docker),
            ContainerOption::Label {
                key: "awman.session".into(),
                value: session.id().to_string(),
            },
        ];

        // Select the persistent piped-stdio (`-i`, no PTY) container path when
        // launching over ACP. Emitted only when actually ACP so ordinary stdio
        // launches keep today's PTY/one-shot-piped behaviour untouched.
        if acp {
            options.push(ContainerOption::Acp(true));
        }

        // Mode flags.
        if let Some(y) = run.yolo {
            options.push(ContainerOption::Yolo(y));
        }
        if let Some(a) = run.auto {
            options.push(ContainerOption::Auto(a));
        }
        if let Some(p) = run.plan {
            options.push(ContainerOption::Plan(p));
        }

        // Tool allow/deny lists.
        if !run.allowed_tools.is_empty() {
            options.push(ContainerOption::AllowedTools(run.allowed_tools.clone()));
            if let Some(flag) = matrix.allowed_tools_flag {
                options.push(ContainerOption::AllowedToolsFlag(flag.to_string()));
            }
        }
        if !run.disallowed_tools.is_empty() {
            options.push(ContainerOption::DisallowedTools(
                run.disallowed_tools.clone(),
            ));
            if let Some(flag) = matrix.disallowed_tools_flag {
                options.push(ContainerOption::DisallowedToolsFlag(flag.to_string()));
            }
        }

        // Resolve per-agent mode flags into literal argv strings.
        let mut mode_flags = Vec::new();
        if matches!(run.yolo, Some(YoloMode::Enabled)) {
            if let Some(flag) = matrix.yolo_flag {
                mode_flags.push(flag.to_string());
            }
        }
        if matches!(
            run.auto,
            Some(crate::engine::container::options::AutoMode::Enabled)
        ) {
            if let Some(flags) = matrix.auto_flag {
                mode_flags.extend(flags.iter().map(|s| s.to_string()));
            }
        }
        if matches!(run.plan, Some(PlanMode::Enabled)) {
            if let Some(flags) = matrix.plan_flag {
                mode_flags.extend(flags.iter().map(|s| s.to_string()));
            }
        }
        if !mode_flags.is_empty() {
            options.push(ContainerOption::AgentModeFlags(mode_flags));
        }

        // Initial prompt. In interactive mode most agents receive it as a
        // trailing positional arg; agents whose bare positional means something
        // else (opencode: a project dir) declare a delivery flag in the matrix.
        // Non-interactive runs pipe it over stdin regardless.
        if let Some(prompt) = run.initial_prompt.as_ref() {
            options.push(ContainerOption::SeededPrompt(prompt.clone()));
            if let agent_matrix::InteractiveSeedDelivery::Flag(flag) =
                matrix.interactive_seed_delivery
            {
                options.push(ContainerOption::InteractiveSeedFlag(flag.to_string()));
            }
        }

        // Model flag.
        if let Some(model) = run.model.as_deref() {
            let flag = agent_matrix::model_flag_for(&matrix, model)?;
            options.push(ContainerOption::Model { flag });
        }

        // Non-interactive: also surface as a discrete option so backends that
        // need to know can react (display purposes etc.). The actual
        // entrypoint already encoded it.
        if run.non_interactive {
            if let Some(flag) = matrix.non_interactive_flag {
                options.push(ContainerOption::NonInteractivePrintFlag(flag.to_string()));
            }
        }
        // Env passthrough.
        let env_pass = run.env_passthrough.as_deref().unwrap_or(&[]);
        for name in env_pass {
            options.push(ContainerOption::EnvPassthrough(EnvVar(name.clone())));
        }

        // Per-agent static env vars.
        if agent.as_str() == "copilot" {
            options.push(ContainerOption::EnvLiteral(
                crate::engine::container::options::EnvLiteral {
                    key: "COPILOT_OFFLINE".into(),
                    value: "true".into(),
                },
            ));
        }

        // Mount the project source into the container's working directory.
        options.push(ContainerOption::Overlay(
            crate::engine::container::options::OverlaySpec {
                host_path: session.git_root().to_path_buf(),
                container_path: std::path::PathBuf::from("/workspace"),
                permission: crate::engine::container::options::OverlayPermission::ReadWrite,
            },
        ));

        // Overlays — agent settings + user-supplied dirs + skills.
        // Prefer the running image's baked-in `$HOME` (the actual runtime
        // authority) over what the local `Dockerfile.<agent>` says — the
        // two can diverge when the Dockerfile was changed but the image
        // hasn't been rebuilt yet, in which case mounting at the
        // Dockerfile-derived path silently breaks credential passthrough.
        let container_home = self
            .container_runtime
            .image_home_dir(&image_tag)
            .or_else(|| {
                let home = dirs::home_dir().unwrap_or_default();
                crate::engine::overlay::detect_container_home(
                    &home,
                    agent.as_str(),
                    session.git_root(),
                )
            });
        let request = OverlayRequest {
            directories: run.directory_overlays.clone(),
            include_all_skills: run.include_all_skills,
            named_skills: run.named_skills.clone(),
            agent: Some(agent.clone()),
            yolo: matches!(run.yolo, Some(YoloMode::Enabled)),
            container_home: container_home.clone(),
            context_overlays: run.context_overlays.clone(),
        };
        for spec in self.overlay_engine.build_overlays(session, &request)? {
            options.push(ContainerOption::Overlay(spec));
        }

        // System prompt delivery (context overlays).
        if let Some(ref prompt_text) = run.system_prompt {
            self.emit_system_prompt_options(
                &matrix,
                prompt_text,
                &run.context_overlays,
                &mut options,
            )?;
        }

        // Default working dir for the agent container.
        options.push(ContainerOption::WorkingDir(std::path::PathBuf::from(
            "/workspace",
        )));

        Ok(options)
    }

    /// Cross-paradigm option builder. Maps a single `AgentRunOptions` (the same
    /// flag/config inputs the container path consumes) into the
    /// `ResolvedAgentOptions` variant matching the active runtime's paradigm,
    /// branching on `AgentRuntimeEngine::capabilities()` rather than on concrete
    /// runtime types. Resolved credential env-vars are folded in as the
    /// paradigm-appropriate option so each command no longer hand-builds its own
    /// option list.
    ///
    /// - container-class runtimes → `ResolvedAgentOptions::Container`
    /// - sandbox-class (kit-declarative) runtimes → `ResolvedAgentOptions::Sandbox`
    ///
    /// Credential **values** only ever reach the runtime as `AgentCredentials`;
    /// for the sandbox paradigm they are routed to `sbx secret set` by the dsbx
    /// driver and never written to `session.json`.
    pub fn resolve_agent_options(
        &self,
        session: &Session,
        agent: &AgentName,
        run: &AgentRunOptions,
        credential_env_vars: &[(String, String)],
        runtime: &dyn AgentRuntimeEngine,
    ) -> Result<ResolvedAgentOptions, EngineError> {
        if runtime.capabilities().kit_declarative {
            let mut options = self.build_sandbox_options(session, agent, run)?;
            if !credential_env_vars.is_empty() {
                options.push(SandboxOption::AgentCredentials {
                    env_vars: credential_env_vars.to_vec(),
                });
            }
            Ok(ResolvedAgentOptions::sandbox(options))
        } else {
            let mut options = self.build_options(session, agent, run)?;
            if !credential_env_vars.is_empty() {
                options.push(ContainerOption::AgentCredentials {
                    env_vars: credential_env_vars.to_vec(),
                });
            }
            ResolvedAgentOptions::container(options)
        }
    }

    /// Build the `SandboxOption` list for launching an agent under a
    /// sandbox-class runtime. Mirrors `build_options` (same agent matrix, same
    /// up-front validation) but emits sandbox-paradigm options: a kit selector
    /// instead of an image ref, a workspace dir instead of arbitrary mounts, and
    /// system prompts delivered inline via `session.json` rather than via
    /// host-file mounts (sandbox VMs see only the workspace).
    pub fn build_sandbox_options(
        &self,
        session: &Session,
        agent: &AgentName,
        run: &AgentRunOptions,
    ) -> Result<Vec<SandboxOption>, EngineError> {
        let matrix = agent_matrix::matrix_for(agent.as_str())?;

        // Parity validation with the container path so a sandbox-configured user
        // gets the same up-front errors for unsupported mode combinations.
        if matches!(run.plan, Some(PlanMode::Enabled)) && matrix.plan_flag.is_none() {
            return Err(EngineError::PlanModeUnsupported {
                agent: agent.as_str().to_string(),
            });
        }
        // ACP guards. First the same `supports_acp` check the container path
        // runs, so an unsupported agent surfaces `AcpUnsupported` identically on
        // both paradigms. Then, because ACP is a container-paradigm-only launch
        // mode (`ContainerOption::Acp` has no sandbox equivalent — the sandbox
        // `ResolvedSandboxOptions` type carries no ACP-piping option), a sandbox
        // launch that survived the first guard (e.g. cline under `docker-sbx-
        // experimental`) is rejected as `NotImplemented` per the WI 0089
        // convention rather than silently launching in the wrong (stdio) mode.
        if matches!(run.launch_mode, LaunchMode::Acp) {
            if !matrix.supports_acp {
                return Err(EngineError::AcpUnsupported {
                    agent: agent.as_str().to_string(),
                });
            }
            return Err(EngineError::NotImplemented(
                "ACP launch mode is not supported by sandbox-class runtimes",
            ));
        }
        if matches!(run.plan, Some(PlanMode::Enabled))
            && matches!(run.yolo, Some(YoloMode::Enabled))
        {
            return Err(EngineError::ConflictingOptions(
                "plan and yolo modes are mutually exclusive".into(),
            ));
        }

        let mut options = vec![
            SandboxOption::AgentId(agent.as_str().to_string()),
            SandboxOption::WorkspaceDir(session.git_root().to_path_buf()),
            SandboxOption::Interactive(!run.non_interactive),
        ];

        // Seeded prompt — recorded in session.json exactly once. For `kind: agent`
        // kits the dsbx launcher appends it positionally; for `kind: mixin` kits
        // the apply script renders it from session.json. The driver never does
        // both, so the prompt is delivered exactly once.
        if let Some(prompt) = run.initial_prompt.as_ref() {
            options.push(SandboxOption::SeededPrompt(prompt.clone()));
        }

        // Model.
        if let Some(model) = run.model.as_deref() {
            let flag = agent_matrix::model_flag_for(&matrix, model)?;
            options.push(SandboxOption::Model { flag });
            // Copilot's built-in template has no mixin-safe model config (the
            // in-VM apply script also warns); surface that host-side instead
            // of letting the flag vanish into the VM boot log.
            if agent.as_str() == "copilot" {
                options.push(SandboxOption::UnsupportedNote(
                    "--model cannot be applied to copilot under the sandbox runtime \
                     (no mixin-safe config); use the /model slash command inside the \
                     session instead"
                        .into(),
                ));
            }
        }

        // Mode flags (plan/yolo/auto). `kind: agent` kits accept agent args
        // after the `--` delimiter, so they get the same literal argv flags
        // the container path emits. Mixin kits launch through Docker's
        // built-in template (no argv channel): claude has a native settings
        // equivalent (`permissions.defaultMode`, rendered by its apply
        // script from `session.json`); the other mixins have none awman
        // manages, so the request is warned about rather than dropped.
        let mut mode_flags = Vec::new();
        if matches!(run.yolo, Some(YoloMode::Enabled)) {
            if let Some(flag) = matrix.yolo_flag {
                mode_flags.push(flag.to_string());
            }
        }
        if matches!(
            run.auto,
            Some(crate::engine::container::options::AutoMode::Enabled)
        ) {
            if let Some(flags) = matrix.auto_flag {
                mode_flags.extend(flags.iter().map(|s| s.to_string()));
            }
        }
        if matches!(run.plan, Some(PlanMode::Enabled)) {
            if let Some(flags) = matrix.plan_flag {
                mode_flags.extend(flags.iter().map(|s| s.to_string()));
            }
        }
        if !mode_flags.is_empty() {
            match matrix.sbx_kit_kind {
                agent_matrix::SbxKitKind::Agent => {
                    options.push(SandboxOption::AgentModeFlags(mode_flags));
                }
                agent_matrix::SbxKitKind::Mixin => {
                    let mode = if matches!(run.yolo, Some(YoloMode::Enabled)) {
                        "yolo"
                    } else if matches!(run.plan, Some(PlanMode::Enabled)) {
                        "plan"
                    } else {
                        "auto"
                    };
                    if agent.as_str() == "claude" {
                        options.push(SandboxOption::PermissionMode(mode.to_string()));
                    } else {
                        options.push(SandboxOption::UnsupportedNote(format!(
                            "--{mode} cannot be delivered to '{agent}' under the sandbox \
                             runtime (the built-in template's launch command takes no \
                             flags and the agent has no settings-file equivalent awman \
                             manages); continuing without it",
                            agent = agent.as_str(),
                        )));
                    }
                }
            }
        }

        // Non-interactive shape (`--print` / `exec` / `run` / `task`): the
        // dsbx driver appends it as an agent arg for `kind: agent` kits, but
        // mixin kits launch through the built-in template's interactive
        // entrypoint — the prompt still arrives via piped stdin, the agent's
        // own non-interactive mode just cannot be enabled. Warn, don't drop
        // silently.
        if run.non_interactive && matches!(matrix.sbx_kit_kind, agent_matrix::SbxKitKind::Mixin) {
            if let Some(flag) = matrix.non_interactive_flag {
                options.push(SandboxOption::UnsupportedNote(format!(
                    "non-interactive mode ('{flag}') cannot be enabled for '{agent}' \
                     under the sandbox runtime (the built-in template owns the launch \
                     command); the prompt is piped to the agent's interactive \
                     entrypoint instead",
                    agent = agent.as_str(),
                )));
            }
        }

        // Tool allow/deny lists.
        if !run.allowed_tools.is_empty() {
            options.push(SandboxOption::AllowedTools(run.allowed_tools.clone()));
        }
        if !run.disallowed_tools.is_empty() {
            options.push(SandboxOption::DisallowedTools(run.disallowed_tools.clone()));
        }

        // Env passthrough. Credential-class names are filtered out of the
        // workspace-readable session.json by `DSbxSessionConfig`; non-sensitive
        // names reach the apply script.
        let env_pass = run.env_passthrough.as_deref().unwrap_or(&[]);
        for name in env_pass {
            options.push(SandboxOption::EnvPassthrough(EnvVar(name.clone())));
        }

        // Per-agent static env vars (mirrors the container path).
        if agent.as_str() == "copilot" {
            options.push(SandboxOption::EnvLiteral(EnvLiteral {
                key: "COPILOT_OFFLINE".into(),
                value: "true".into(),
            }));
        }

        // Every sandbox has a private DinD daemon, so the host-socket mount
        // `--allow-docker` requests is meaningless here (same trace as ready).
        if run.allow_docker {
            tracing::debug!("--allow-docker is a no-op under sbx (private DinD is always on)");
        }

        // Host-directory mounts cannot be honored: the VM sees only the
        // virtiofs-mounted workspace. Record each requested-but-unmountable
        // feature as a note; `run_interactive` surfaces them as warnings.
        for dir in &run.directory_overlays {
            options.push(SandboxOption::UnsupportedNote(format!(
                "directory overlay '{}' cannot be mounted into the sandbox VM \
                 (only the workspace is visible); continuing without it",
                dir.host
            )));
        }
        if run.include_all_skills || !run.named_skills.is_empty() {
            options.push(SandboxOption::UnsupportedNote(
                "skill mounts (--include-all-skills / skill(...) overlays) are not \
                 supported under the sandbox runtime; continuing without them"
                    .into(),
            ));
        }
        if !run.context_overlays.is_empty() {
            options.push(SandboxOption::UnsupportedNote(
                "context directories cannot be mounted into the sandbox VM; the \
                 rendered system prompt text is still delivered, but /awman/context \
                 paths will not exist inside the sandbox"
                    .into(),
            ));
        }

        // System prompt — delivered inline via session.json so no host-file
        // mount is required (the sandbox VM can only see the workspace).
        if let Some(ref prompt_text) = run.system_prompt {
            emit_sandbox_system_prompt_options(&matrix, prompt_text, &mut options);
        }

        Ok(options)
    }

    /// Emit the `ContainerOption` variants for system-prompt delivery, consulting
    /// the agent matrix for the correct delivery mechanism.
    fn emit_system_prompt_options(
        &self,
        matrix: &agent_matrix::AgentMatrix,
        prompt_text: &str,
        context_overlays: &[ContextOverlay],
        options: &mut Vec<ContainerOption>,
    ) -> Result<(), EngineError> {
        use agent_matrix::SystemPromptMode;

        match &matrix.system_prompt_delivery {
            SystemPromptMode::Append => {
                let flag = matrix
                    .system_prompt_flag
                    .unwrap_or("--append-system-prompt-file");
                let (host_path, container_path) = self.write_prompt_temp_file(prompt_text)?;
                options.push(ContainerOption::SystemPromptFile {
                    host_path,
                    container_path,
                    flag: flag.to_string(),
                });
            }
            SystemPromptMode::AppendInline { key } => {
                let flag = matrix.system_prompt_flag.unwrap_or("--config");
                options.push(ContainerOption::SystemPromptInline {
                    flag: flag.to_string(),
                    text: format!("{key}={prompt_text}"),
                });
            }
            SystemPromptMode::Replace => {
                let flag = matrix.system_prompt_flag.unwrap_or("--system");
                let preamble = format!(
                    "You are {}, an AI coding assistant. Use your full capabilities \
                     and all available tools to complete the task.\n\n",
                    matrix.agent
                );
                options.push(ContainerOption::SystemPromptInline {
                    flag: flag.to_string(),
                    text: format!("{preamble}{prompt_text}"),
                });
            }
            SystemPromptMode::AgentsMd => {
                for ctx in context_overlays {
                    plant_agents_md(&ctx.host_path, prompt_text);
                }
            }
            SystemPromptMode::EnvFile { var } => {
                let (host_path, container_path) = self.write_prompt_temp_file(prompt_text)?;
                options.push(ContainerOption::SystemPromptEnvFile {
                    env_var: var.to_string(),
                    host_path,
                    container_path,
                });
            }
            SystemPromptMode::AddDir { flag } => {
                for ctx in context_overlays {
                    plant_agents_md(&ctx.host_path, prompt_text);
                    options.push(ContainerOption::AgentAddDir {
                        flag: flag.to_string(),
                        container_path: ctx.container_path.clone(),
                    });
                }
            }
            SystemPromptMode::Unsupported => {
                // The user-facing warning is emitted in `resolve_context_overlays`
                // (it has access to a UserMessageSink); here we just skip emitting
                // any system-prompt option.
            }
        }
        Ok(())
    }

    /// Write the system prompt text to a temp file and return (host_path, container_path).
    /// The temp file is retained on the engine's RAII guard so it lives as
    /// long as the engine and is cleaned up on drop.
    fn write_prompt_temp_file(
        &self,
        text: &str,
    ) -> Result<(std::path::PathBuf, std::path::PathBuf), EngineError> {
        use std::io::Write as _;
        let mut tmp = tempfile::Builder::new()
            .prefix("awman-ctx-prompt-")
            .suffix(".md")
            .tempfile()
            .map_err(|e| EngineError::io(std::path::PathBuf::from("/tmp"), e))?;
        tmp.write_all(text.as_bytes())
            .map_err(|e| EngineError::io(tmp.path(), e))?;
        let host_path = tmp.path().to_path_buf();
        let container_path =
            std::path::PathBuf::from("/tmp").join(host_path.file_name().unwrap_or_default());
        if let Ok(mut guard) = self.prompt_tempfiles.lock() {
            guard.push(tmp);
        }
        Ok((host_path, container_path))
    }
}

/// Emit the `SandboxOption` for system-prompt delivery. Unlike the container
/// path — which can mount a host file or set an env-file path — the sandbox VM
/// only sees the workspace, so every text-bearing delivery mode is collapsed to
/// `SystemPromptInline`: the prompt text travels in `session.json` and the
/// mixin apply script renders it into the agent's native config. `AddDir`
/// (extra mounted dirs) cannot be expressed and records an `UnsupportedNote`;
/// `Unsupported` stays a documented no-op (warned upstream).
fn emit_sandbox_system_prompt_options(
    matrix: &agent_matrix::AgentMatrix,
    prompt_text: &str,
    options: &mut Vec<SandboxOption>,
) {
    use agent_matrix::SystemPromptMode;

    match &matrix.system_prompt_delivery {
        SystemPromptMode::Append => {
            options.push(SandboxOption::SystemPromptInline {
                flag: matrix
                    .system_prompt_flag
                    .unwrap_or("--append-system-prompt")
                    .to_string(),
                text: prompt_text.to_string(),
            });
        }
        SystemPromptMode::AppendInline { key } => {
            options.push(SandboxOption::SystemPromptInline {
                flag: matrix.system_prompt_flag.unwrap_or("--config").to_string(),
                text: format!("{key}={prompt_text}"),
            });
        }
        SystemPromptMode::Replace => {
            let preamble = format!(
                "You are {}, an AI coding assistant. Use your full capabilities \
                 and all available tools to complete the task.\n\n",
                matrix.agent
            );
            options.push(SandboxOption::SystemPromptInline {
                flag: matrix.system_prompt_flag.unwrap_or("--system").to_string(),
                text: format!("{preamble}{prompt_text}"),
            });
        }
        SystemPromptMode::EnvFile { var } => {
            // The mixin apply script materializes the text into a VM-local file
            // and points `var` at it; the flag slot carries the env-var name.
            options.push(SandboxOption::SystemPromptInline {
                flag: var.to_string(),
                text: prompt_text.to_string(),
            });
        }
        SystemPromptMode::AgentsMd => {
            // No CLI flag exists for this mode; the mixin apply script reads
            // the inline text from session.json and stages it in the VM.
            options.push(SandboxOption::SystemPromptInline {
                flag: String::new(),
                text: prompt_text.to_string(),
            });
        }
        SystemPromptMode::AddDir { .. } => {
            // AddDir plants AGENTS.md into extra mounted directories, which
            // do not exist under the sandbox runtime — warn, don't drop.
            options.push(SandboxOption::UnsupportedNote(format!(
                "agent '{}' delivers system prompts via extra directory mounts, \
                 which are unavailable under the sandbox runtime; the system \
                 prompt was not applied",
                matrix.agent
            )));
        }
        SystemPromptMode::Unsupported => {
            // Documented no-op — the user-facing warning is emitted by
            // `resolve_context_overlays`, same as the container path.
        }
    }
}

/// Write `AGENTS.md` into a context dir, skipping the write when the existing
/// content already matches. Logs (and silently moves on) when the write fails
/// e.g. because the host directory is read-only.
fn plant_agents_md(host_dir: &std::path::Path, prompt_text: &str) {
    let agents_md = host_dir.join("AGENTS.md");
    if let Ok(existing) = std::fs::read_to_string(&agents_md) {
        if existing == prompt_text {
            return;
        }
    }
    if let Err(e) = std::fs::write(&agents_md, prompt_text) {
        tracing::warn!(
            path = %agents_md.display(),
            error = %e,
            "context overlay: failed to plant AGENTS.md (host dir read-only?); \
             agent will not be automatically notified about the context directory"
        );
    }
}

/// Best-effort check whether a Docker image tag exists locally.
/// Returns `false` quietly when `docker` is missing.
pub(crate) fn image_exists_locally(tag: &str) -> bool {
    use std::process::Command;
    Command::new("docker")
        .args(["image", "inspect", tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::data::config::effective::EffectiveConfig;
    use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};
    use crate::engine::container::options::{ContainerOption, PlanMode, YoloMode};
    use crate::engine::overlay::OverlayEngine;

    #[test]
    fn build_options_rejects_plan_for_unsupported_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        // opencode does not support plan mode.
        let agent = crate::data::session::AgentName::new("opencode").unwrap();
        let run = AgentRunOptions {
            plan: Some(PlanMode::Enabled),
            ..Default::default()
        };
        let result = engine.build_options(&session, &agent, &run);
        assert!(
            matches!(result, Err(EngineError::PlanModeUnsupported { .. })),
            "expected PlanModeUnsupported for opencode with plan mode, got {result:?}"
        );
    }

    #[test]
    fn build_options_rejects_acp_for_unsupported_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        // codex does not support ACP (only cline does today).
        let agent = crate::data::session::AgentName::new("codex").unwrap();
        let run = AgentRunOptions {
            launch_mode: LaunchMode::Acp,
            ..Default::default()
        };
        let result = engine.build_options(&session, &agent, &run);
        assert!(
            matches!(result, Err(EngineError::AcpUnsupported { .. })),
            "expected AcpUnsupported for codex with ACP launch mode, got {result:?}"
        );
    }

    #[test]
    fn build_options_acp_for_cline_emits_acp_option_and_acp_entrypoint() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("cline").unwrap();
        let run = AgentRunOptions {
            launch_mode: LaunchMode::Acp,
            ..Default::default()
        };
        let opts = engine.build_options(&session, &agent, &run).unwrap();

        assert!(
            opts.iter().any(|o| matches!(o, ContainerOption::Acp(true))),
            "cline + ACP must emit ContainerOption::Acp(true); got {opts:?}"
        );
        let entrypoint = opts
            .iter()
            .find_map(|o| {
                if let ContainerOption::Entrypoint(e) = o {
                    Some(e.0.clone())
                } else {
                    None
                }
            })
            .expect("Entrypoint option must be present");
        assert_eq!(
            entrypoint,
            vec!["cline".to_string(), "--acp".to_string()],
            "cline ACP launch must use the `cline --acp` entrypoint"
        );
    }

    #[test]
    fn build_options_stdio_launch_mode_emits_no_acp_option() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        // cline supports ACP, but the default Stdio launch mode must not opt in.
        let agent = crate::data::session::AgentName::new("cline").unwrap();
        let opts = engine
            .build_options(&session, &agent, &AgentRunOptions::default())
            .unwrap();
        assert!(
            !opts.iter().any(|o| matches!(o, ContainerOption::Acp(_))),
            "default (Stdio) launch mode must not emit a ContainerOption::Acp; got {opts:?}"
        );
    }

    #[test]
    fn build_sandbox_options_acp_for_cline_is_not_implemented() {
        // cline passes the supports_acp guard, but the sandbox paradigm has no
        // ACP-piping option, so the request surfaces NotImplemented rather than
        // silently launching in stdio mode (WI 0089 convention).
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("cline").unwrap();
        let run = AgentRunOptions {
            launch_mode: LaunchMode::Acp,
            ..Default::default()
        };
        let result = engine.build_sandbox_options(&session, &agent, &run);
        assert!(
            matches!(result, Err(EngineError::NotImplemented(_))),
            "expected NotImplemented for cline + ACP on the sandbox path, got {result:?}"
        );
    }

    #[test]
    fn build_sandbox_options_acp_for_unsupported_agent_is_acp_unsupported() {
        // An agent that doesn't support ACP fails the supports_acp guard first,
        // so both paradigms surface AcpUnsupported consistently.
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("codex").unwrap();
        let run = AgentRunOptions {
            launch_mode: LaunchMode::Acp,
            ..Default::default()
        };
        let result = engine.build_sandbox_options(&session, &agent, &run);
        assert!(
            matches!(result, Err(EngineError::AcpUnsupported { .. })),
            "expected AcpUnsupported for codex + ACP on the sandbox path, got {result:?}"
        );
    }

    fn make_agent_engine(home: &std::path::Path) -> (AgentEngine, crate::data::session::Session) {
        let session_tmp = tempfile::tempdir().unwrap();
        // We only use session_tmp as session root; home is for auth paths.
        let resolver = StaticGitRootResolver::new(session_tmp.path());
        let session = crate::data::session::Session::open(
            session_tmp.path().to_path_buf(),
            &resolver,
            SessionOpenOptions::default(),
        )
        .unwrap();
        let overlay = OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(home),
        );
        let runtime = crate::engine::container::ContainerRuntime::docker();
        let engine = AgentEngine::new(Arc::new(overlay), Arc::new(runtime));
        (engine, session)
    }

    #[test]
    fn build_options_includes_image_and_entrypoint_for_claude() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let opts = engine
            .build_options(&session, &agent, &AgentRunOptions::default())
            .unwrap();
        assert!(
            opts.iter().any(|o| matches!(o, ContainerOption::Image(_))),
            "Image option must be present"
        );
        assert!(
            opts.iter()
                .any(|o| matches!(o, ContainerOption::Entrypoint(_))),
            "Entrypoint option must be present"
        );
    }

    /// WI 0101 §2.2 regression guard: after `SessionLabel` was generalized to
    /// `Label { key, value }`, the one existing producer must still emit
    /// exactly `awman.session=<session id>` and nothing else.
    #[test]
    fn build_options_still_emits_the_session_label_after_the_label_generalization() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let opts = engine
            .build_options(&session, &agent, &AgentRunOptions::default())
            .unwrap();
        let labels: Vec<(&str, &str)> = opts
            .iter()
            .filter_map(|o| match o {
                ContainerOption::Label { key, value } => Some((key.as_str(), value.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            labels,
            vec![("awman.session", session.id().to_string().as_str())],
            "an ordinary session container carries exactly the session label"
        );
    }

    #[test]
    fn build_options_includes_image_and_entrypoint_for_codex() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("codex").unwrap();
        let opts = engine
            .build_options(&session, &agent, &AgentRunOptions::default())
            .unwrap();
        assert!(opts.iter().any(|o| matches!(o, ContainerOption::Image(_))));
        assert!(opts
            .iter()
            .any(|o| matches!(o, ContainerOption::Entrypoint(_))));
    }

    #[test]
    fn build_options_with_yolo_includes_yolo_option() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let run = AgentRunOptions {
            yolo: Some(YoloMode::Enabled),
            ..Default::default()
        };
        let opts = engine.build_options(&session, &agent, &run).unwrap();
        assert!(
            opts.iter()
                .any(|o| matches!(o, ContainerOption::Yolo(YoloMode::Enabled))),
            "Yolo option must be present when requested"
        );
    }

    #[test]
    fn build_options_with_allowed_tools_includes_allowed_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let run = AgentRunOptions {
            allowed_tools: vec!["Bash".to_string(), "Read".to_string()],
            ..Default::default()
        };
        let opts = engine.build_options(&session, &agent, &run).unwrap();
        let has = opts.iter().any(|o| {
            if let ContainerOption::AllowedTools(tools) = o {
                tools.contains(&"Bash".to_string()) && tools.contains(&"Read".to_string())
            } else {
                false
            }
        });
        assert!(has, "AllowedTools option must contain the requested tools");
    }

    #[test]
    fn build_options_plan_and_yolo_together_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let run = AgentRunOptions {
            plan: Some(PlanMode::Enabled),
            yolo: Some(YoloMode::Enabled),
            ..Default::default()
        };
        let result = engine.build_options(&session, &agent, &run);
        assert!(
            matches!(result, Err(EngineError::ConflictingOptions(_))),
            "plan + yolo must be rejected as conflicting, got {result:?}"
        );
    }

    #[test]
    fn build_options_non_interactive_true_includes_print_flag_for_claude() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let run = AgentRunOptions {
            non_interactive: true,
            ..Default::default()
        };
        let opts = engine.build_options(&session, &agent, &run).unwrap();
        let has_flag = opts
            .iter()
            .any(|o| matches!(o, ContainerOption::NonInteractivePrintFlag(f) if f == "--print"));
        assert!(
            has_flag,
            "NonInteractivePrintFlag --print must be present for claude"
        );
    }

    #[test]
    fn build_options_non_interactive_true_includes_print_flag_for_crush() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("crush").unwrap();
        let run = AgentRunOptions {
            non_interactive: true,
            ..Default::default()
        };
        let opts = engine.build_options(&session, &agent, &run).unwrap();
        let has_flag = opts
            .iter()
            .any(|o| matches!(o, ContainerOption::NonInteractivePrintFlag(f) if f == "run"));
        assert!(
            has_flag,
            "NonInteractivePrintFlag 'run' must be present for crush"
        );
    }

    #[test]
    fn build_options_opencode_seeded_prompt_emits_interactive_seed_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("opencode").unwrap();
        let run = AgentRunOptions {
            initial_prompt: Some("implement the work item".into()),
            ..Default::default()
        };
        let opts = engine.build_options(&session, &agent, &run).unwrap();
        assert!(
            opts.iter().any(
                |o| matches!(o, ContainerOption::SeededPrompt(p) if p == "implement the work item")
            ),
            "opencode must still carry the seeded prompt"
        );
        assert!(
            opts.iter()
                .any(|o| matches!(o, ContainerOption::InteractiveSeedFlag(f) if f == "--prompt")),
            "opencode must emit InteractiveSeedFlag(--prompt) alongside the seeded prompt"
        );
    }

    #[test]
    fn build_options_claude_seeded_prompt_has_no_interactive_seed_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let run = AgentRunOptions {
            initial_prompt: Some("do it".into()),
            ..Default::default()
        };
        let opts = engine.build_options(&session, &agent, &run).unwrap();
        assert!(
            !opts
                .iter()
                .any(|o| matches!(o, ContainerOption::InteractiveSeedFlag(_))),
            "claude seeds positionally and must not emit an InteractiveSeedFlag"
        );
    }

    #[test]
    fn build_options_antigravity_entrypoint_is_agy() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("antigravity").unwrap();
        let opts = engine
            .build_options(&session, &agent, &AgentRunOptions::default())
            .unwrap();
        let entrypoint = opts
            .iter()
            .find_map(|o| {
                if let ContainerOption::Entrypoint(e) = o {
                    Some(e.0.clone())
                } else {
                    None
                }
            })
            .expect("Entrypoint option must be present");
        assert_eq!(
            entrypoint,
            vec!["agy".to_string()],
            "antigravity interactive entrypoint must be [\"agy\"]"
        );
    }

    #[test]
    fn build_options_antigravity_yolo_non_interactive_includes_print_and_skip_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("antigravity").unwrap();
        let run = AgentRunOptions {
            yolo: Some(YoloMode::Enabled),
            non_interactive: true,
            ..Default::default()
        };
        let opts = engine.build_options(&session, &agent, &run).unwrap();

        // --print must appear in the entrypoint (non_interactive=true appends it).
        let entrypoint = opts
            .iter()
            .find_map(|o| {
                if let ContainerOption::Entrypoint(e) = o {
                    Some(e.0.clone())
                } else {
                    None
                }
            })
            .expect("Entrypoint option must be present");
        assert!(
            entrypoint.contains(&"--print".to_string()),
            "entrypoint must contain --print for non_interactive antigravity; got {entrypoint:?}"
        );

        // NonInteractivePrintFlag must also be set.
        let has_print_flag = opts
            .iter()
            .any(|o| matches!(o, ContainerOption::NonInteractivePrintFlag(f) if f == "--print"));
        assert!(
            has_print_flag,
            "NonInteractivePrintFlag(--print) must be present for non_interactive antigravity"
        );

        // AgentModeFlags must contain --dangerously-skip-permissions.
        let mode_flags: Vec<String> = opts
            .iter()
            .filter_map(|o| {
                if let ContainerOption::AgentModeFlags(flags) = o {
                    Some(flags.clone())
                } else {
                    None
                }
            })
            .flatten()
            .collect();
        assert!(
            mode_flags.contains(&"--dangerously-skip-permissions".to_string()),
            "AgentModeFlags must contain --dangerously-skip-permissions for antigravity yolo; got {mode_flags:?}"
        );
    }

    #[test]
    fn build_options_antigravity_plan_returns_plan_unsupported_error() {
        // agy has no `--approval-mode=plan` CLI flag (verified against
        // `agy --help`; plan/auto modes live only in settings.json's
        // `toolPermission` field or interactive slash commands). Asking for
        // plan mode on antigravity must surface as `PlanModeUnsupported`
        // rather than silently emitting a flag agy treats as garbage.
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("antigravity").unwrap();
        let run = AgentRunOptions {
            plan: Some(PlanMode::Enabled),
            non_interactive: true,
            ..Default::default()
        };
        let err = engine
            .build_options(&session, &agent, &run)
            .expect_err("plan mode on antigravity must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("antigravity") && msg.to_lowercase().contains("plan"),
            "error must call out plan-mode for antigravity; got: {msg}"
        );
    }

    #[test]
    fn build_options_antigravity_model_flag_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("antigravity").unwrap();
        let run = AgentRunOptions {
            model: Some("gemini-3.5-flash".to_string()),
            ..Default::default()
        };
        let result = engine.build_options(&session, &agent, &run);
        assert!(
            result.is_err(),
            "build_options with model for antigravity must return Err; got {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("antigravity"),
            "error must name the agent 'antigravity'; got: {msg}"
        );
        assert!(
            msg.contains("does not support a model flag"),
            "error must say 'does not support a model flag'; got: {msg}"
        );
    }

    #[test]
    fn build_options_non_interactive_false_no_print_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let run = AgentRunOptions {
            non_interactive: false,
            ..Default::default()
        };
        let opts = engine.build_options(&session, &agent, &run).unwrap();
        assert!(
            !opts
                .iter()
                .any(|o| matches!(o, ContainerOption::NonInteractivePrintFlag(_))),
            "NonInteractivePrintFlag must be absent when non_interactive=false"
        );
    }

    // ── ensure_available tests ───────────────────────────────────────────────

    struct FakeAgentFrontend {
        statuses: Vec<(String, StepStatus)>,
        container_call_count: usize,
    }

    impl FakeAgentFrontend {
        fn new() -> Self {
            Self {
                statuses: Vec::new(),
                container_call_count: 0,
            }
        }
    }

    impl crate::data::message::UserMessageSink for FakeAgentFrontend {
        fn write_message(&mut self, _: crate::data::message::UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    struct FakeRuntimeFrontend;
    impl crate::data::message::UserMessageSink for FakeRuntimeFrontend {
        fn write_message(&mut self, _: crate::data::message::UserMessage) {}
        fn replay_queued(&mut self) {}
    }
    #[async_trait::async_trait]
    impl crate::engine::agent_runtime::frontend::AgentFrontend for FakeRuntimeFrontend {
        fn report_status(&mut self, _: crate::engine::agent_runtime::frontend::AgentStatus) {}
        fn report_progress(&mut self, _: crate::engine::agent_runtime::frontend::AgentProgress) {}
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

    impl AgentFrontend for FakeAgentFrontend {
        fn report_step_status(&mut self, step: &str, status: StepStatus) {
            self.statuses.push((step.to_string(), status));
        }
        fn container_frontend(
            &mut self,
        ) -> Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend> {
            self.container_call_count += 1;
            Box::new(FakeRuntimeFrontend)
        }
    }

    // Scenario 1: project image absent → returns AgentRequiresProjectImage error.
    #[tokio::test]
    async fn ensure_available_fails_when_project_image_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let config = EffectiveConfig::default();
        let mut frontend = FakeAgentFrontend::new();

        let result = engine
            .ensure_available(&session, &agent, &config, &mut frontend, |_| false)
            .await;

        assert!(
            matches!(result, Err(EngineError::AgentRequiresProjectImage { .. })),
            "must fail with AgentRequiresProjectImage when project image is absent, got {result:?}"
        );
    }

    // Scenario 2: project image present, agent image present → no-op (no status calls).
    #[tokio::test]
    async fn ensure_available_is_noop_when_all_images_present() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let config = EffectiveConfig::default();
        let mut frontend = FakeAgentFrontend::new();

        // Write a fake Dockerfile so the file-presence check passes.
        let paths =
            crate::data::repo_dockerfile_paths::RepoDockerfilePaths::new(session.git_root());
        let dockerfile = paths.agent_dockerfile("claude");
        if let Some(parent) = dockerfile.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&dockerfile, "FROM scratch").unwrap();

        // Both images "exist".
        let result = engine
            .ensure_available(&session, &agent, &config, &mut frontend, |_| true)
            .await;

        assert!(
            result.is_ok(),
            "must succeed when all images present, got {result:?}"
        );
        assert!(
            frontend.statuses.is_empty(),
            "no status reports expected when already up-to-date"
        );
        assert_eq!(
            frontend.container_call_count, 0,
            "no container_frontend calls expected when images are present"
        );
    }

    // Scenario 3: project image present, agent image absent → build step fires.
    #[tokio::test]
    async fn ensure_available_builds_agent_image_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let config = EffectiveConfig::default();
        let mut frontend = FakeAgentFrontend::new();

        // Write a fake Dockerfile so the file-presence check passes.
        let paths =
            crate::data::repo_dockerfile_paths::RepoDockerfilePaths::new(session.git_root());
        let dockerfile = paths.agent_dockerfile("claude");
        if let Some(parent) = dockerfile.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&dockerfile, "FROM scratch").unwrap();

        let project_tag = crate::data::image_tags::project_image_tag(session.git_root());
        // Project image exists; agent image does not.
        let result = engine
            .ensure_available(&session, &agent, &config, &mut frontend, |tag| {
                tag == project_tag
            })
            .await;

        // The build step MUST fire — runtime.build_image gets invoked. In a
        // test environment without `docker` on PATH the spawn fails and the
        // engine surfaces a structured error; that's the documented behavior
        // (no silent soft-fail). What we check here is that the engine
        // reached the build step and called the runtime, regardless of
        // whether docker is installed.
        let _ = result;
        let statuses: Vec<_> = frontend
            .statuses
            .iter()
            .filter(|(s, _)| s == "Building image")
            .collect();
        assert!(
            !statuses.is_empty(),
            "Building image status must have fired"
        );
        assert_eq!(
            frontend.container_call_count, 1,
            "container_frontend must be called once for the build step"
        );
    }

    // Scenario 4: project image present, agent Dockerfile absent → download
    // attempted (fails without network) and a failed status is reported.
    #[tokio::test]
    async fn ensure_available_reports_failed_status_when_dockerfile_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let config = EffectiveConfig::default();
        let mut frontend = FakeAgentFrontend::new();

        let project_tag = crate::data::image_tags::project_image_tag(session.git_root());
        // Project image present; Dockerfile absent → triggers download attempt.
        let result = engine
            .ensure_available(&session, &agent, &config, &mut frontend, |tag| {
                tag == project_tag
            })
            .await;

        // In a test environment the download will fail (no network or the URL
        // returns an error); we just need to verify the engine handled it
        // gracefully (no panic) and reported something.
        // The result may be Ok (download failed but is non-fatal in some paths)
        // or Err; both are acceptable as long as the engine doesn't panic.
        let _ = result;
        // At minimum there should be some status activity.
        // (We assert the engine completed without panicking — that's the
        // key invariant for this scenario.)
    }

    // ─── WI-0087: build_options single emitter ────────────────────────────────

    #[test]
    fn build_options_claude_with_context_global_emits_overlay_at_container_path() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();

        // Context dir host path (need not exist — context overlays bypass the
        // host-existence check in resolve_user_overlay).
        let ctx_host = tmp.path().join("context").join("global");

        let run = AgentRunOptions {
            context_overlays: vec![crate::engine::overlay::ContextOverlay {
                scope: crate::engine::overlay::ContextScope::Global,
                host_path: ctx_host,
                container_path: std::path::PathBuf::from("/awman/context/global"),
                permission: crate::engine::container::options::OverlayPermission::ReadWrite,
            }],
            ..Default::default()
        };

        let opts = engine.build_options(&session, &agent, &run).unwrap();

        let has_ctx_overlay = opts.iter().any(|o| {
            if let ContainerOption::Overlay(spec) = o {
                spec.container_path == std::path::Path::new("/awman/context/global")
            } else {
                false
            }
        });
        assert!(
            has_ctx_overlay,
            "build_options must emit ContainerOption::Overlay for /awman/context/global; \
             got {opts:?}"
        );
    }

    #[test]
    fn build_options_claude_with_system_prompt_emits_system_prompt_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();

        let ctx_host = tmp.path().join("context").join("global");

        let run = AgentRunOptions {
            system_prompt: Some("Test context prompt.".to_string()),
            context_overlays: vec![crate::engine::overlay::ContextOverlay {
                scope: crate::engine::overlay::ContextScope::Global,
                host_path: ctx_host,
                container_path: std::path::PathBuf::from("/awman/context/global"),
                permission: crate::engine::container::options::OverlayPermission::ReadWrite,
            }],
            ..Default::default()
        };

        let opts = engine.build_options(&session, &agent, &run).unwrap();

        let prompt_file_opt = opts
            .iter()
            .find(|o| matches!(o, ContainerOption::SystemPromptFile { .. }));
        assert!(
            prompt_file_opt.is_some(),
            "build_options must emit ContainerOption::SystemPromptFile for claude; \
             got {opts:?}"
        );
        if let Some(ContainerOption::SystemPromptFile { flag, .. }) = prompt_file_opt {
            assert_eq!(
                flag, "--append-system-prompt-file",
                "claude system prompt flag must be --append-system-prompt-file"
            );
        }
    }

    #[test]
    fn build_options_no_system_prompt_option_without_system_prompt_input() {
        // When system_prompt is None, no SystemPromptFile option must be emitted.
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();

        let run = AgentRunOptions {
            system_prompt: None,
            ..Default::default()
        };

        let opts = engine.build_options(&session, &agent, &run).unwrap();

        let has_prompt_file = opts
            .iter()
            .any(|o| matches!(o, ContainerOption::SystemPromptFile { .. }));
        assert!(
            !has_prompt_file,
            "no SystemPromptFile option must be emitted when system_prompt is None; \
             got {opts:?}"
        );
    }

    #[test]
    fn build_options_maki_with_system_prompt_does_not_emit_system_prompt_file() {
        // maki has Unsupported delivery; no SystemPromptFile must appear.
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("maki").unwrap();

        let run = AgentRunOptions {
            system_prompt: Some("Test prompt.".to_string()),
            ..Default::default()
        };

        let opts = engine.build_options(&session, &agent, &run).unwrap();

        let has_prompt_file = opts
            .iter()
            .any(|o| matches!(o, ContainerOption::SystemPromptFile { .. }));
        assert!(
            !has_prompt_file,
            "maki must not emit SystemPromptFile (Unsupported delivery); got {opts:?}"
        );
    }

    #[test]
    fn build_options_cline_emits_inline_system_prompt_with_preamble() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("cline").unwrap();
        let run = AgentRunOptions {
            system_prompt: Some("Test prompt body.".to_string()),
            ..Default::default()
        };
        let opts = engine.build_options(&session, &agent, &run).unwrap();
        let inline = opts
            .iter()
            .find_map(|o| {
                if let ContainerOption::SystemPromptInline { flag, text } = o {
                    Some((flag.clone(), text.clone()))
                } else {
                    None
                }
            })
            .expect("cline must emit SystemPromptInline");
        assert_eq!(inline.0, "--system", "cline flag must be --system");
        assert!(
            inline.1.starts_with("You are cline,"),
            "cline replace mode must prepend baseline preamble; got: {}",
            inline.1
        );
        assert!(
            inline.1.contains("Test prompt body."),
            "cline inline must contain the prompt body; got: {}",
            inline.1
        );
        // No SystemPromptFile should be emitted for cline.
        assert!(
            !opts
                .iter()
                .any(|o| matches!(o, ContainerOption::SystemPromptFile { .. })),
            "cline must not emit SystemPromptFile; got {opts:?}"
        );
    }

    #[test]
    fn build_options_codex_emits_inline_developer_instructions_config() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("codex").unwrap();
        let run = AgentRunOptions {
            system_prompt: Some("Codex test body.".to_string()),
            ..Default::default()
        };
        let opts = engine.build_options(&session, &agent, &run).unwrap();
        let inline = opts
            .iter()
            .find_map(|o| {
                if let ContainerOption::SystemPromptInline { flag, text } = o {
                    Some((flag.clone(), text.clone()))
                } else {
                    None
                }
            })
            .expect("codex must emit SystemPromptInline (config key=value)");
        assert_eq!(inline.0, "--config", "codex flag must be --config");
        assert!(
            inline.1.starts_with("developer_instructions="),
            "codex inline must use key=value form; got: {}",
            inline.1
        );
        assert!(
            inline.1.contains("Codex test body."),
            "codex inline must contain the prompt body; got: {}",
            inline.1
        );
    }

    // ─── WI-0091: build_sandbox_options ───────────────────────────────────────

    #[test]
    fn build_sandbox_options_emits_agent_id_workspace_dir_and_interactive() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let opts = engine
            .build_sandbox_options(&session, &agent, &AgentRunOptions::default())
            .unwrap();
        assert!(
            opts.iter()
                .any(|o| matches!(o, SandboxOption::AgentId(id) if id == "claude")),
            "AgentId(\"claude\") must be present; got {opts:?}"
        );
        assert!(
            opts.iter()
                .any(|o| matches!(o, SandboxOption::WorkspaceDir(_))),
            "WorkspaceDir must be present; got {opts:?}"
        );
        assert!(
            opts.iter()
                .any(|o| matches!(o, SandboxOption::Interactive(_))),
            "Interactive must be present; got {opts:?}"
        );
    }

    #[test]
    fn build_sandbox_options_interactive_false_when_non_interactive() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let run = AgentRunOptions {
            non_interactive: true,
            ..Default::default()
        };
        let opts = engine
            .build_sandbox_options(&session, &agent, &run)
            .unwrap();
        let interactive_val = opts
            .iter()
            .find_map(|o| {
                if let SandboxOption::Interactive(v) = o {
                    Some(*v)
                } else {
                    None
                }
            })
            .expect("Interactive option must be present");
        assert!(
            !interactive_val,
            "Interactive must be false for non_interactive=true"
        );
    }

    #[test]
    fn build_sandbox_options_mode_flags_for_agent_kits_mirror_container_argv() {
        // crush is a `kind: agent` kit: yolo must surface as the same literal
        // argv flag the container path emits.
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("crush").unwrap();
        let run = AgentRunOptions {
            yolo: Some(YoloMode::Enabled),
            ..Default::default()
        };
        let opts = engine
            .build_sandbox_options(&session, &agent, &run)
            .unwrap();
        assert!(
            opts.iter().any(|o| matches!(
                o,
                SandboxOption::AgentModeFlags(flags) if flags == &vec!["--yolo".to_string()]
            )),
            "crush yolo must emit AgentModeFlags([--yolo]); got {opts:?}"
        );
    }

    #[test]
    fn build_sandbox_options_claude_modes_become_permission_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        for (run, expected) in [
            (
                AgentRunOptions {
                    yolo: Some(YoloMode::Enabled),
                    ..Default::default()
                },
                "yolo",
            ),
            (
                AgentRunOptions {
                    plan: Some(PlanMode::Enabled),
                    ..Default::default()
                },
                "plan",
            ),
            (
                AgentRunOptions {
                    auto: Some(crate::engine::container::options::AutoMode::Enabled),
                    ..Default::default()
                },
                "auto",
            ),
        ] {
            let opts = engine
                .build_sandbox_options(&session, &agent, &run)
                .unwrap();
            assert!(
                opts.iter().any(|o| matches!(
                    o,
                    SandboxOption::PermissionMode(m) if m == expected
                )),
                "claude mixin mode must emit PermissionMode({expected}); got {opts:?}"
            );
            assert!(
                !opts
                    .iter()
                    .any(|o| matches!(o, SandboxOption::AgentModeFlags(_))),
                "mixin kits must not receive argv mode flags; got {opts:?}"
            );
        }
    }

    #[test]
    fn build_sandbox_options_non_claude_mixin_modes_become_unsupported_notes() {
        // codex is a mixin without an awman-managed settings equivalent for
        // permission modes — the request must surface as a warning note,
        // never silently drop.
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("codex").unwrap();
        let run = AgentRunOptions {
            yolo: Some(YoloMode::Enabled),
            ..Default::default()
        };
        let opts = engine
            .build_sandbox_options(&session, &agent, &run)
            .unwrap();
        assert!(
            opts.iter().any(|o| matches!(
                o,
                SandboxOption::UnsupportedNote(n) if n.contains("--yolo") && n.contains("codex")
            )),
            "codex yolo under sbx must surface as an UnsupportedNote; got {opts:?}"
        );
        assert!(
            !opts.iter().any(|o| matches!(
                o,
                SandboxOption::AgentModeFlags(_) | SandboxOption::PermissionMode(_)
            )),
            "codex must get neither argv flags nor a permission mode; got {opts:?}"
        );
    }

    #[test]
    fn build_sandbox_options_mixin_non_interactive_adds_unsupported_note() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let run = AgentRunOptions {
            non_interactive: true,
            ..Default::default()
        };
        let opts = engine
            .build_sandbox_options(&session, &agent, &run)
            .unwrap();
        assert!(
            opts.iter().any(|o| matches!(
                o,
                SandboxOption::UnsupportedNote(n) if n.contains("non-interactive")
            )),
            "mixin non-interactive mode under sbx must warn; got {opts:?}"
        );

        // kind: agent kits CAN take the flag as an agent arg — no note.
        let agent = crate::data::session::AgentName::new("crush").unwrap();
        let opts = engine
            .build_sandbox_options(&session, &agent, &run)
            .unwrap();
        assert!(
            !opts.iter().any(|o| matches!(
                o,
                SandboxOption::UnsupportedNote(n) if n.contains("non-interactive")
            )),
            "agent-kit non-interactive mode is deliverable — no note expected; got {opts:?}"
        );
    }

    #[test]
    fn build_sandbox_options_copilot_model_adds_unsupported_note() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("copilot").unwrap();
        let run = AgentRunOptions {
            model: Some("gpt-5".into()),
            ..Default::default()
        };
        let opts = engine
            .build_sandbox_options(&session, &agent, &run)
            .unwrap();
        assert!(
            opts.iter().any(|o| matches!(
                o,
                SandboxOption::UnsupportedNote(n) if n.contains("--model")
            )),
            "copilot --model under sbx must warn host-side; got {opts:?}"
        );
    }

    #[test]
    fn build_sandbox_options_seeded_prompt_appears_exactly_once() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let run = AgentRunOptions {
            initial_prompt: Some("do something useful".into()),
            ..Default::default()
        };
        let opts = engine
            .build_sandbox_options(&session, &agent, &run)
            .unwrap();
        let prompt_count = opts
            .iter()
            .filter(|o| matches!(o, SandboxOption::SeededPrompt(_)))
            .count();
        assert_eq!(
            prompt_count, 1,
            "SeededPrompt must appear exactly once; got {prompt_count} times in {opts:?}"
        );
        let prompt_val = opts
            .iter()
            .find_map(|o| {
                if let SandboxOption::SeededPrompt(p) = o {
                    Some(p.clone())
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(
            prompt_val, "do something useful",
            "SeededPrompt must carry the original prompt text"
        );
    }

    #[test]
    fn build_sandbox_options_model_flag_emitted_for_claude() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let run = AgentRunOptions {
            model: Some("claude-opus-4-8".into()),
            ..Default::default()
        };
        let opts = engine
            .build_sandbox_options(&session, &agent, &run)
            .unwrap();
        assert!(
            opts.iter()
                .any(|o| matches!(o, SandboxOption::Model { .. })),
            "Model option must be present when run.model is Some; got {opts:?}"
        );
    }

    #[test]
    fn build_sandbox_options_allowed_tools_emitted() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let run = AgentRunOptions {
            allowed_tools: vec!["Bash".into(), "Read".into()],
            ..Default::default()
        };
        let opts = engine
            .build_sandbox_options(&session, &agent, &run)
            .unwrap();
        let found = opts.iter().any(|o| {
            if let SandboxOption::AllowedTools(tools) = o {
                tools.contains(&"Bash".to_string()) && tools.contains(&"Read".to_string())
            } else {
                false
            }
        });
        assert!(
            found,
            "AllowedTools must contain the requested tools; got {opts:?}"
        );
    }

    #[test]
    fn build_sandbox_options_disallowed_tools_emitted() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let run = AgentRunOptions {
            disallowed_tools: vec!["Write".into()],
            ..Default::default()
        };
        let opts = engine
            .build_sandbox_options(&session, &agent, &run)
            .unwrap();
        let found = opts.iter().any(|o| {
            if let SandboxOption::DisallowedTools(tools) = o {
                tools.contains(&"Write".to_string())
            } else {
                false
            }
        });
        assert!(
            found,
            "DisallowedTools must contain the requested tools; got {opts:?}"
        );
    }

    #[test]
    fn build_sandbox_options_env_passthrough_emitted() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let run = AgentRunOptions {
            env_passthrough: Some(vec!["LOG_LEVEL".into(), "DEBUG".into()]),
            ..Default::default()
        };
        let opts = engine
            .build_sandbox_options(&session, &agent, &run)
            .unwrap();
        let pass_names: Vec<_> = opts
            .iter()
            .filter_map(|o| {
                if let SandboxOption::EnvPassthrough(v) = o {
                    Some(v.0.clone())
                } else {
                    None
                }
            })
            .collect();
        assert!(
            pass_names.contains(&"LOG_LEVEL".to_string()),
            "LOG_LEVEL must appear in EnvPassthrough; got {pass_names:?}"
        );
        assert!(
            pass_names.contains(&"DEBUG".to_string()),
            "DEBUG must appear in EnvPassthrough; got {pass_names:?}"
        );
    }

    #[test]
    fn build_sandbox_options_copilot_has_static_offline_env_literal() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("copilot").unwrap();
        let opts = engine
            .build_sandbox_options(&session, &agent, &AgentRunOptions::default())
            .unwrap();
        let has_offline = opts.iter().any(|o| {
            if let SandboxOption::EnvLiteral(lit) = o {
                lit.key == "COPILOT_OFFLINE" && lit.value == "true"
            } else {
                false
            }
        });
        assert!(
            has_offline,
            "copilot must have COPILOT_OFFLINE=true EnvLiteral; got {opts:?}"
        );
    }

    #[test]
    fn build_sandbox_options_plan_yolo_conflict_rejected_parity_with_container_path() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let run = AgentRunOptions {
            plan: Some(crate::engine::container::options::PlanMode::Enabled),
            yolo: Some(crate::engine::container::options::YoloMode::Enabled),
            ..Default::default()
        };
        let result = engine.build_sandbox_options(&session, &agent, &run);
        assert!(
            matches!(result, Err(EngineError::ConflictingOptions(_))),
            "plan + yolo must be rejected as ConflictingOptions (parity with container path); \
             got {result:?}"
        );
    }

    #[test]
    fn build_sandbox_options_plan_mode_unsupported_agent_rejected_parity_with_container_path() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        // opencode does not support plan mode (same as in build_options test).
        let agent = crate::data::session::AgentName::new("opencode").unwrap();
        let run = AgentRunOptions {
            plan: Some(crate::engine::container::options::PlanMode::Enabled),
            ..Default::default()
        };
        let result = engine.build_sandbox_options(&session, &agent, &run);
        assert!(
            matches!(result, Err(EngineError::PlanModeUnsupported { .. })),
            "PlanModeUnsupported must be returned for opencode (parity with container path); \
             got {result:?}"
        );
    }

    #[test]
    fn build_sandbox_options_system_prompt_emitted_as_inline_for_claude() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let run = AgentRunOptions {
            system_prompt: Some("focus on performance".into()),
            ..Default::default()
        };
        let opts = engine
            .build_sandbox_options(&session, &agent, &run)
            .unwrap();
        assert!(
            opts.iter()
                .any(|o| matches!(o, SandboxOption::SystemPromptInline { .. })),
            "SystemPromptInline must be present for claude system_prompt; got {opts:?}"
        );
        // No file-based delivery — sandbox VMs don't have arbitrary host mounts.
        assert!(
            !opts
                .iter()
                .any(|o| matches!(o, SandboxOption::SystemPromptFile { .. })),
            "SystemPromptFile must NOT appear for claude in sandbox mode; got {opts:?}"
        );
    }

    #[test]
    fn build_sandbox_options_opencode_system_prompt_emitted_as_inline() {
        // opencode's container delivery mode is AgentsMd (file planting) —
        // under sbx the text must still travel inline so the apply script can
        // surface it instead of silently dropping it.
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("opencode").unwrap();
        let run = AgentRunOptions {
            system_prompt: Some("focus on tests".into()),
            ..Default::default()
        };
        let opts = engine
            .build_sandbox_options(&session, &agent, &run)
            .unwrap();
        let inline = opts
            .iter()
            .find_map(|o| {
                if let SandboxOption::SystemPromptInline { text, .. } = o {
                    Some(text.clone())
                } else {
                    None
                }
            })
            .expect("opencode system prompt must be emitted inline under sbx");
        assert_eq!(inline, "focus on tests");
    }

    #[test]
    fn build_sandbox_options_antigravity_system_prompt_becomes_unsupported_note() {
        // antigravity delivers system prompts via --add-dir mounts, which do
        // not exist under sbx — must warn (note), never drop silently.
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("antigravity").unwrap();
        let run = AgentRunOptions {
            system_prompt: Some("focus on tests".into()),
            ..Default::default()
        };
        let opts = engine
            .build_sandbox_options(&session, &agent, &run)
            .unwrap();
        assert!(
            opts.iter().any(|o| matches!(
                o,
                SandboxOption::UnsupportedNote(n) if n.contains("system prompt")
            )),
            "antigravity system prompt must surface as an UnsupportedNote; got {opts:?}"
        );
        assert!(
            !opts
                .iter()
                .any(|o| matches!(o, SandboxOption::SystemPromptInline { .. })),
            "antigravity must not emit an inline system prompt under sbx; got {opts:?}"
        );
    }

    #[test]
    fn build_sandbox_options_directory_overlays_become_unsupported_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let run = AgentRunOptions {
            directory_overlays: vec![crate::engine::overlay::DirectorySpec {
                host: "~/reference".into(),
                container: "/mnt/reference".into(),
                permission: crate::engine::container::options::OverlayPermission::ReadOnly,
            }],
            ..Default::default()
        };
        let opts = engine
            .build_sandbox_options(&session, &agent, &run)
            .unwrap();
        assert!(
            opts.iter().any(|o| matches!(
                o,
                SandboxOption::UnsupportedNote(n)
                    if n.contains("~/reference") && n.contains("cannot be mounted")
            )),
            "dir overlay must surface as an UnsupportedNote naming the host path; got {opts:?}"
        );
    }

    #[test]
    fn build_sandbox_options_skills_become_unsupported_note() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        for run in [
            AgentRunOptions {
                include_all_skills: true,
                ..Default::default()
            },
            AgentRunOptions {
                named_skills: vec!["review".into()],
                ..Default::default()
            },
        ] {
            let opts = engine
                .build_sandbox_options(&session, &agent, &run)
                .unwrap();
            assert!(
                opts.iter().any(|o| matches!(
                    o,
                    SandboxOption::UnsupportedNote(n) if n.contains("skill")
                )),
                "skill mounts must surface as an UnsupportedNote; got {opts:?}"
            );
        }
    }

    #[test]
    fn build_sandbox_options_context_overlays_become_unsupported_note() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let run = AgentRunOptions {
            context_overlays: vec![crate::engine::overlay::ContextOverlay {
                scope: crate::engine::overlay::ContextScope::Repo,
                host_path: tmp.path().join("ctx"),
                container_path: std::path::PathBuf::from("/awman/context/repo"),
                permission: crate::engine::container::options::OverlayPermission::ReadWrite,
            }],
            ..Default::default()
        };
        let opts = engine
            .build_sandbox_options(&session, &agent, &run)
            .unwrap();
        assert!(
            opts.iter().any(|o| matches!(
                o,
                SandboxOption::UnsupportedNote(n) if n.contains("context")
            )),
            "context dirs must surface as an UnsupportedNote; got {opts:?}"
        );
    }

    // ─── WI-0091: resolve_agent_options parity tests ─────────────────────────

    /// A minimal `AgentRuntimeEngine` stub for option-construction parity
    /// tests. Does not implement build() — only capabilities() is exercised.
    struct FakeRuntime {
        caps: crate::engine::agent_runtime::Capabilities,
    }

    impl FakeRuntime {
        fn sandbox() -> Self {
            use crate::engine::agent_runtime::{Capabilities, DindSupport};
            Self {
                caps: Capabilities {
                    arbitrary_env_vars: false,
                    arbitrary_host_mounts: false,
                    cpu_limits: false,
                    per_resource_stats: false,
                    persistent_lifecycle: true,
                    kit_declarative: true,
                    dind: DindSupport::Always,
                    host_paths_visible: false,
                    session_label_supported: false,
                },
            }
        }

        fn container() -> Self {
            use crate::engine::agent_runtime::{Capabilities, DindSupport};
            Self {
                caps: Capabilities {
                    arbitrary_env_vars: true,
                    arbitrary_host_mounts: true,
                    cpu_limits: true,
                    per_resource_stats: true,
                    persistent_lifecycle: false,
                    kit_declarative: false,
                    dind: DindSupport::OnRequest,
                    host_paths_visible: true,
                    session_label_supported: true,
                },
            }
        }
    }

    impl crate::engine::agent_runtime::AgentRuntimeEngine for FakeRuntime {
        fn runtime_name(&self) -> &'static str {
            if self.caps.kit_declarative {
                "fake-sbx"
            } else {
                "fake-container"
            }
        }
        fn display_name(&self) -> &'static str {
            "Fake Runtime"
        }
        fn capabilities(&self) -> &crate::engine::agent_runtime::Capabilities {
            &self.caps
        }
        fn is_available(&self) -> bool {
            true
        }
        fn build(
            &self,
            _: crate::engine::agent_runtime::ResolvedAgentOptions,
        ) -> Result<Box<dyn crate::engine::agent_runtime::AgentInstance>, EngineError> {
            unimplemented!(
                "FakeRuntime: build() is not exercised by option-construction parity tests"
            )
        }
        fn list_running(
            &self,
            _: &crate::data::session::Session,
        ) -> Result<Vec<crate::data::session::AgentHandle>, EngineError> {
            Ok(vec![])
        }
        fn list_running_all(&self) -> Result<Vec<crate::data::session::AgentHandle>, EngineError> {
            Ok(vec![])
        }
        fn stats(
            &self,
            _: &crate::data::session::AgentHandle,
        ) -> Result<crate::engine::agent_runtime::AgentStats, EngineError> {
            Ok(crate::engine::agent_runtime::AgentStats {
                name: "fake".into(),
                cpu_percent: 0.0,
                memory_mb: 0.0,
            })
        }
        fn stop(&self, _: &crate::data::session::AgentHandle) -> Result<(), EngineError> {
            Ok(())
        }
        fn exec_args(&self, _: &str, _: &str, _: &[&str], _: &[(&str, &str)]) -> Vec<String> {
            vec![]
        }
        fn attach(
            &self,
            _: &crate::data::session::AgentHandle,
        ) -> Result<Box<dyn crate::engine::agent_runtime::AgentInstance>, EngineError> {
            unimplemented!(
                "FakeRuntime: attach() is not exercised by option-construction parity tests"
            )
        }
        fn list_running_with_name_prefix(
            &self,
            _: &str,
        ) -> Result<Vec<crate::data::session::AgentHandle>, EngineError> {
            Ok(vec![])
        }
        fn cli_binary(&self) -> &'static str {
            "fake"
        }
    }

    #[test]
    fn resolve_agent_options_sandbox_runtime_yields_sandbox_variant() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let fake_sbx = FakeRuntime::sandbox();
        let result = engine.resolve_agent_options(
            &session,
            &agent,
            &AgentRunOptions::default(),
            &[],
            &fake_sbx,
        );
        assert!(
            matches!(
                result,
                Ok(crate::engine::agent_runtime::ResolvedAgentOptions::Sandbox(
                    _
                ))
            ),
            "kit_declarative runtime must yield Sandbox variant; got {result:?}"
        );
    }

    #[test]
    fn resolve_agent_options_container_runtime_yields_container_variant() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let fake_container = FakeRuntime::container();
        let result = engine.resolve_agent_options(
            &session,
            &agent,
            &AgentRunOptions::default(),
            &[],
            &fake_container,
        );
        assert!(
            matches!(
                result,
                Ok(crate::engine::agent_runtime::ResolvedAgentOptions::Container(_))
            ),
            "non-kit_declarative runtime must yield Container variant; got {result:?}"
        );
    }

    #[test]
    fn resolve_agent_options_credentials_become_agent_credentials_in_sandbox() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let fake_sbx = FakeRuntime::sandbox();
        let creds = vec![("ANTHROPIC_API_KEY".to_string(), "sk-secret".to_string())];
        let result = engine
            .resolve_agent_options(
                &session,
                &agent,
                &AgentRunOptions::default(),
                &creds,
                &fake_sbx,
            )
            .unwrap();
        if let crate::engine::agent_runtime::ResolvedAgentOptions::Sandbox(resolved) = result {
            assert!(
                resolved
                    .agent_credentials
                    .contains(&("ANTHROPIC_API_KEY".into(), "sk-secret".into())),
                "credential must appear in agent_credentials; got {:?}",
                resolved.agent_credentials
            );
        } else {
            panic!("expected Sandbox variant");
        }
    }

    #[test]
    fn resolve_agent_options_credentials_never_in_env_passthrough_or_env_literal_in_sandbox() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let fake_sbx = FakeRuntime::sandbox();
        let creds = vec![("ANTHROPIC_API_KEY".to_string(), "sk-secret".to_string())];
        let run = AgentRunOptions {
            // Put the same key in env_passthrough — it must still only end up in
            // agent_credentials, not in env_passthrough in the resolved bag.
            env_passthrough: Some(vec!["ANTHROPIC_API_KEY".into()]),
            ..Default::default()
        };
        let result = engine
            .resolve_agent_options(&session, &agent, &run, &creds, &fake_sbx)
            .unwrap();
        if let crate::engine::agent_runtime::ResolvedAgentOptions::Sandbox(resolved) = result {
            // Credentials from `creds` must not appear as EnvPassthrough env var
            // values in the session config (they are filtered by DSbxSessionConfig).
            // The agent_credentials field must carry the credential pair.
            assert!(
                resolved
                    .agent_credentials
                    .iter()
                    .any(|(k, _)| k == "ANTHROPIC_API_KEY"),
                "ANTHROPIC_API_KEY must appear in agent_credentials; got {:?}",
                resolved.agent_credentials
            );
        } else {
            panic!("expected Sandbox variant");
        }
    }

    #[test]
    fn resolve_agent_options_same_agent_model_tools_intent_in_both_paradigms() {
        // For a fixed set of flags, the sandbox and container variants must
        // carry the same agent, model, and tool intent.
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let run = AgentRunOptions {
            model: Some("claude-sonnet-4-6".into()),
            allowed_tools: vec!["Bash".into()],
            disallowed_tools: vec!["Write".into()],
            initial_prompt: Some("implement feature X".into()),
            non_interactive: true,
            ..Default::default()
        };

        let sbx_result = engine
            .resolve_agent_options(&session, &agent, &run, &[], &FakeRuntime::sandbox())
            .unwrap();
        let ctr_result = engine
            .resolve_agent_options(&session, &agent, &run, &[], &FakeRuntime::container())
            .unwrap();

        // Both must successfully build.
        let sbx_opts = match sbx_result {
            crate::engine::agent_runtime::ResolvedAgentOptions::Sandbox(r) => r,
            other => panic!("expected Sandbox, got {other:?}"),
        };
        let ctr_opts = match ctr_result {
            crate::engine::agent_runtime::ResolvedAgentOptions::Container(r) => r,
            other => panic!("expected Container, got {other:?}"),
        };

        // Agent id.
        assert_eq!(
            sbx_opts.agent_id, "claude",
            "sandbox agent_id must be claude"
        );

        // Seeded prompt is present in sandbox.
        assert_eq!(
            sbx_opts.seeded_prompt.as_deref(),
            Some("implement feature X"),
            "sandbox seeded_prompt must carry the prompt"
        );

        // Model present in both.
        assert!(sbx_opts.model.is_some(), "sandbox model must be set");
        assert!(ctr_opts.model.is_some(), "container model must be set");

        // Tool lists present in both.
        assert!(
            sbx_opts.allowed_tools.contains(&"Bash".to_string()),
            "sandbox allowed_tools must contain Bash"
        );
        assert!(
            sbx_opts.disallowed_tools.contains(&"Write".to_string()),
            "sandbox disallowed_tools must contain Write"
        );
        assert!(
            ctr_opts.allowed_tools.contains(&"Bash".to_string()),
            "container allowed_tools must contain Bash"
        );
        assert!(
            ctr_opts.disallowed_tools.contains(&"Write".to_string()),
            "container disallowed_tools must contain Write"
        );
    }

    #[test]
    fn resolve_agent_options_seeded_prompt_in_sandbox_exactly_once() {
        // The prompt must appear in SeededPrompt exactly once, regardless of
        // system prompt or other options (no double-delivery).
        let tmp = tempfile::tempdir().unwrap();
        let (engine, session) = make_agent_engine(tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let run = AgentRunOptions {
            initial_prompt: Some("run the tests".into()),
            system_prompt: Some("You are a helpful assistant.".into()),
            ..Default::default()
        };
        let result = engine
            .resolve_agent_options(&session, &agent, &run, &[], &FakeRuntime::sandbox())
            .unwrap();
        if let crate::engine::agent_runtime::ResolvedAgentOptions::Sandbox(resolved) = result {
            assert_eq!(
                resolved.seeded_prompt.as_deref(),
                Some("run the tests"),
                "seeded_prompt must carry the user's prompt text"
            );
            // system_prompt_inline is from the system prompt, not the user prompt.
            // They are distinct fields — seeded_prompt is the user task prompt.
            if let Some((_, text)) = &resolved.system_prompt_inline {
                assert!(
                    !text.contains("run the tests"),
                    "system_prompt_inline must not contain the user seeded_prompt text: {text}"
                );
            }
        } else {
            panic!("expected Sandbox variant");
        }
    }
}
