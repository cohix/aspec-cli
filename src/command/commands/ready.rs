//! `ReadyCommand` — thin wrapper over `ReadyEngine`.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::{resolve_agent, Command};
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage};
use crate::engine::ready::{ReadyEngine, ReadyEngineOptions, ReadyFrontend, ReadySummary};
use crate::engine::step_status::StepStatus;

#[derive(Debug, Clone)]
pub struct ReadyCommandFlags {
    pub refresh: bool,
    pub build: bool,
    pub no_cache: bool,
    pub non_interactive: bool,
    pub allow_docker: bool,
    pub json: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadyOutcome {
    pub runtime: String,
    pub dockerfile: StepStatus,
    pub base_image: StepStatus,
    pub agent_image: StepStatus,
    pub local_agent: StepStatus,
    pub audit: StepStatus,
    pub image_rebuild: StepStatus,
    /// Per non-default agent image status.
    pub non_default_agent_images: Vec<(String, StepStatus)>,
    /// `true` when `--json` was passed; controls how the CLI renders the outcome.
    #[serde(skip)]
    pub json_requested: bool,
    /// `true` when `--refresh` was passed; carried into legacy JSON output.
    #[serde(skip)]
    pub refresh_requested: bool,
}

impl From<ReadySummary> for ReadyOutcome {
    fn from(s: ReadySummary) -> Self {
        Self {
            runtime: s.runtime_name,
            dockerfile: s.dockerfile,
            base_image: s.base_image,
            agent_image: s.agent_image,
            local_agent: s.local_agent,
            audit: s.audit,
            image_rebuild: s.image_rebuild,
            non_default_agent_images: s.non_default_agent_images,
            json_requested: false,
            refresh_requested: false,
        }
    }
}

impl ReadyOutcome {
    /// Render the outcome in the legacy `awman ready --json` schema:
    ///
    /// ```json
    /// { "ready": <bool>,
    ///   "steps": {
    ///     "docker_daemon": {"status": "ok|skipped|failed|pending", "message": "..."},
    ///     "dockerfile": {...}, "aspec_folder": {...}, "work_items_config": {...},
    ///     "local_agent": {...}, "dev_image": {...}, "refresh": {...},
    ///     "image_rebuild": {...}
    ///   }
    /// }
    /// ```
    pub fn to_legacy_json(&self) -> serde_json::Value {
        fn step_to_json(s: &StepStatus) -> serde_json::Value {
            match s {
                StepStatus::Pending => serde_json::json!({"status": "pending", "message": ""}),
                StepStatus::Running => serde_json::json!({"status": "running", "message": ""}),
                StepStatus::Done => serde_json::json!({"status": "ok", "message": ""}),
                StepStatus::Skipped => serde_json::json!({"status": "skipped", "message": ""}),
                StepStatus::Warn(msg) => serde_json::json!({"status": "warn", "message": msg}),
                StepStatus::Failed(msg) => {
                    serde_json::json!({"status": "failed", "message": msg})
                }
            }
        }

        let any_failed = matches!(self.dockerfile, StepStatus::Failed(_))
            || matches!(self.base_image, StepStatus::Failed(_))
            || matches!(self.agent_image, StepStatus::Failed(_))
            || matches!(self.local_agent, StepStatus::Failed(_))
            || matches!(self.image_rebuild, StepStatus::Failed(_));

        // `docker_daemon` isn't tracked as a separate step in the new engine;
        // if we made it this far, the daemon was reachable.
        let docker_daemon = StepStatus::Done;

        // `aspec_folder` and `work_items_config` are owned by `init`, not by
        // `ready`. Report them as Skipped so consumers see a complete schema.
        let aspec_folder = StepStatus::Skipped;
        let work_items_config = StepStatus::Skipped;

        // `refresh` is derived from the flag — Done if user asked for it,
        // Skipped otherwise.
        let refresh = if self.refresh_requested {
            StepStatus::Done
        } else {
            StepStatus::Skipped
        };

        serde_json::json!({
            "ready": !any_failed,
            "runtime": self.runtime,
            "steps": {
                "docker_daemon": step_to_json(&docker_daemon),
                "dockerfile": step_to_json(&self.dockerfile),
                "aspec_folder": step_to_json(&aspec_folder),
                "work_items_config": step_to_json(&work_items_config),
                "local_agent": step_to_json(&self.local_agent),
                "dev_image": step_to_json(&self.agent_image),
                "refresh": step_to_json(&refresh),
                "image_rebuild": step_to_json(&self.image_rebuild),
            }
        })
    }
}

pub trait ReadyCommandFrontend: ReadyFrontend + Send {}
impl<T: ReadyFrontend + Send> ReadyCommandFrontend for T {}

pub struct ReadyCommand {
    flags: ReadyCommandFlags,
    engines: Engines,
    session: crate::data::session::Session,
}

impl ReadyCommand {
    pub fn new(
        flags: ReadyCommandFlags,
        engines: Engines,
        session: crate::data::session::Session,
    ) -> Self {
        Self {
            flags,
            engines,
            session,
        }
    }

    pub fn flags(&self) -> &ReadyCommandFlags {
        &self.flags
    }
}

#[async_trait]
impl Command for ReadyCommand {
    type Frontend = Box<dyn ReadyCommandFrontend>;
    type Outcome = ReadyOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "ready: checking environment…".into(),
        });

        let session = self.session;
        // Resolve the agent from .awman/config.json (or global default), so
        // the per-agent Dockerfile lookup and image-tag computation match
        // whatever the user actually configured. Falls back to "claude" only
        // when no config sets the default.
        let agent = match resolve_agent(&None, &session) {
            Ok(a) => a,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("ready: failed to resolve agent name: {e}"),
                });
                return Err(e);
            }
        };
        // Sandbox runtime: `awman ready` emits and validates kits instead of
        // building Docker images. Route to the sbx-specific flow and skip the
        // container ReadyEngine entirely. No credential resolution here — all
        // sbx secret registration is sandbox-scoped at agent-launch time.
        if self.engines.sandbox_runtime.is_some() {
            if self.flags.allow_docker {
                // No-op under sbx: every sandbox gets a private DinD daemon.
                tracing::debug!("--allow-docker is a no-op under sbx (private DinD is always on)");
            }
            let result = crate::engine::sandbox::ready_sbx_agent(
                agent.as_str(),
                self.flags.no_cache,
                frontend.as_mut(),
            );
            frontend.replay_queued();
            let agent_status = match &result {
                Ok(()) => StepStatus::Done,
                Err(e) => StepStatus::Failed(e.to_string()),
            };
            let outcome = ReadyOutcome {
                runtime: self.engines.runtime.runtime_name().to_string(),
                dockerfile: StepStatus::Skipped,
                base_image: StepStatus::Skipped,
                agent_image: agent_status,
                local_agent: StepStatus::Skipped,
                audit: StepStatus::Skipped,
                image_rebuild: StepStatus::Skipped,
                non_default_agent_images: Vec::new(),
                json_requested: self.flags.json,
                refresh_requested: self.flags.refresh,
            };
            result.map_err(CommandError::from)?;
            return Ok(outcome);
        }

        let options = ReadyEngineOptions {
            agent,
            refresh: self.flags.refresh,
            build: self.flags.build,
            no_cache: self.flags.no_cache,
            allow_docker: self.flags.allow_docker,
            non_interactive: self.flags.non_interactive,
            env_passthrough: None,
        };

        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!(
                "Checking {} availability…",
                self.engines.runtime.display_name()
            ),
        });

        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "Checking container runtime…".into(),
        });

        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "Checking agent image…".into(),
        });

        let mut engine = ReadyEngine::new(
            std::sync::Arc::new(session),
            self.engines.git_engine.clone(),
            self.engines.overlay_engine.clone(),
            self.engines
                .require_container_runtime()
                .map_err(CommandError::from)?
                .clone(),
            self.engines.agent_engine.clone(),
            options,
        );
        let summary = match engine.run_to_completion(frontend.as_mut()).await {
            Ok(s) => s,
            Err(e) => {
                let cmd_err = CommandError::from(e);
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("ready: engine run failed: {cmd_err}"),
                });
                return Err(cmd_err);
            }
        };
        frontend.replay_queued();
        let mut outcome: ReadyOutcome = summary.into();
        outcome.json_requested = self.flags.json;
        outcome.refresh_requested = self.flags.refresh;

        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!(
                "ready: complete — runtime={}, dockerfile={:?}, base_image={:?}, agent_image={:?}, local_agent={:?}",
                outcome.runtime,
                outcome.dockerfile,
                outcome.base_image,
                outcome.agent_image,
                outcome.local_agent,
            ),
        });

        Ok(outcome)
    }
}
