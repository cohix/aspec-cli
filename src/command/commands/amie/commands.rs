//! The single Layer-2 amie command family.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::amie::daemon::{
    AmieDaemonCommand, AmieDaemonOutcome, AmieDaemonSubcommand, AmieLogsFlags, AmieStartFlags,
    AmieStatusFlags, AmieStopFlags,
};
use crate::command::commands::amie::gateway::{
    ConditionDetail, ConditionGateway, CreateCondition, DaemonStatus, DEFAULT_RUN_HISTORY_LIMIT,
};
use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::fs::condition_store::{Condition, ConditionStatus, MountScope};
use crate::data::fs::AmiePaths;
use crate::data::message::UserMessageSink;

#[derive(Debug, Clone)]
pub struct AmieServeConfig {
    pub port: u16,
    pub dangerously_skip_auth: bool,
}

#[async_trait]
pub trait AmieCommandFrontend: UserMessageSink + Send + Sync {
    async fn serve_amie_daemon(&mut self, _config: AmieServeConfig) -> Result<(), CommandError> {
        Err(CommandError::NotAvailableForFrontend {
            command: "amie start".into(),
            frontend: "this".into(),
        })
    }

    // ── Condition-creation interview (BLOCKER-3, §9.3) ──────────────────────
    //
    // These COLLECT input; they must not validate or reject an answer — that
    // stays in `LocalConditionGateway::validate_create`. Each defaults to an
    // error so a non-interactive frontend (the daemon's API frontend) refuses
    // an interview rather than inventing answers.

    fn ask_condition_name(&mut self) -> Result<String, CommandError> {
        Err(interview_unavailable())
    }
    fn ask_condition_description(&mut self) -> Result<String, CommandError> {
        Err(interview_unavailable())
    }
    /// Raw interval spec (e.g. `5m`); Layer 2 parses and Layer 1 validates it.
    fn ask_condition_interval(&mut self) -> Result<String, CommandError> {
        Err(interview_unavailable())
    }
    fn ask_condition_repo(&mut self) -> Result<PathBuf, CommandError> {
        Err(interview_unavailable())
    }
    fn ask_condition_agent(&mut self) -> Result<Option<String>, CommandError> {
        Err(interview_unavailable())
    }
    fn ask_condition_model(&mut self) -> Result<Option<String>, CommandError> {
        Err(interview_unavailable())
    }
    fn ask_condition_mount_scope(&mut self) -> Result<MountScope, CommandError> {
        Err(interview_unavailable())
    }

    /// Ask whether to delete a condition's persistent directory on `remove`
    /// (BLOCKER-2, §9.2). Defaults to `Ok(false)` so the daemon's API frontend
    /// never removes a directory; only an interactive frontend answers `true`.
    fn ask_delete_condition_dir(
        &mut self,
        _name: &str,
        _path: &Path,
    ) -> Result<bool, CommandError> {
        Ok(false)
    }
}

fn interview_unavailable() -> CommandError {
    CommandError::NotAvailableForFrontend {
        command: "amie add --interview".into(),
        frontend: "this".into(),
    }
}

/// Fields for `amie add`. In interview mode Layer 2 collects every field
/// through the frontend's `ask_condition_*` methods; otherwise `prefilled`
/// carries the flag-derived condition assembled by Dispatch.
pub struct AmieAddRequest {
    pub interview: bool,
    pub prefilled: Option<CreateCondition>,
}

pub enum AmieSubcommand {
    Start(AmieStartFlags),
    Stop(AmieStopFlags),
    Status(AmieStatusFlags),
    Logs(AmieLogsFlags),
    Add(AmieAddRequest),
    List,
    Show(String),
    Remove { name: String, yes: bool },
    Pause(String),
    Resume(String),
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum AmieOutcome {
    Started {
        port: u16,
        background: bool,
        refreshed_key: bool,
    },
    Stopped {
        stopped_pid: Option<u32>,
    },
    Status(DaemonStatus),
    Logs {
        log_path: String,
    },
    Condition(Condition),
    Detail(ConditionDetail),
    Conditions(Vec<Condition>),
    Removed {
        name: String,
        /// The persistent condition directory that was deleted, when one was.
        /// `None` means the directory was kept (declined) or absent.
        removed_dir: Option<PathBuf>,
    },
    Ok,
}

pub struct AmieCommand {
    sub: AmieSubcommand,
    gateway: Option<Box<dyn ConditionGateway>>,
    engines: Engines,
}

impl AmieCommand {
    pub fn new(
        sub: AmieSubcommand,
        gateway: Option<Box<dyn ConditionGateway>>,
        engines: Engines,
    ) -> Self {
        Self {
            sub,
            gateway,
            engines,
        }
    }
}

#[async_trait]
impl Command for AmieCommand {
    type Frontend = Box<dyn AmieCommandFrontend>;
    type Outcome = AmieOutcome;
    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let AmieCommand {
            sub,
            gateway,
            engines,
        } = self;
        match sub {
            AmieSubcommand::Start(flags) => daemon_outcome(
                AmieDaemonCommand::new(AmieDaemonSubcommand::Start(flags), engines)
                    .run_with_frontend(frontend)
                    .await?,
            ),
            AmieSubcommand::Stop(flags) => daemon_outcome(
                AmieDaemonCommand::new(AmieDaemonSubcommand::Stop(flags), engines)
                    .run_with_frontend(frontend)
                    .await?,
            ),
            AmieSubcommand::Status(flags) => {
                // Liveness, PID and bound address come from the pidfile/sidecar
                // (correct even when the daemon is down). A gateway is injected
                // only when the daemon has published its endpoint, so its
                // presence is the "daemon reachable" signal: overlay the live
                // scheduler counts from `gateway.status()`. A stopped daemon
                // means no gateway (no HTTP call); a present-but-failing gateway
                // degrades to the pidfile-only answer rather than failing (§9.4).
                let outcome = AmieDaemonCommand::new(AmieDaemonSubcommand::Status(flags), engines)
                    .run_with_frontend(frontend)
                    .await?;
                let AmieDaemonOutcome::Status(mut status) = outcome else {
                    unreachable!("status subcommand yields a status outcome");
                };
                if let Some(gateway) = &gateway {
                    if let Ok(live) = gateway.status().await {
                        status.running = live.running;
                        status.condition_count = live.condition_count;
                        status.active_count = live.active_count;
                        status.last_tick = live.last_tick;
                        status.in_flight = live.in_flight;
                    }
                }
                Ok(AmieOutcome::Status(status))
            }
            AmieSubcommand::Logs(flags) => daemon_outcome(
                AmieDaemonCommand::new(AmieDaemonSubcommand::Logs(flags), engines)
                    .run_with_frontend(frontend)
                    .await?,
            ),
            sub => {
                let gateway = gateway.ok_or_else(|| CommandError::Other("amie conditions are served by the amie daemon; start it with `awman amie start`".into()))?;
                match sub {
                    AmieSubcommand::Add(request) => {
                        // In interview mode Layer 2 collects every field through
                        // the frontend; otherwise Dispatch already assembled the
                        // condition from flags. Validation stays in Layer 1's
                        // `LocalConditionGateway::validate_create`.
                        let req = match request.prefilled {
                            Some(req) => req,
                            None => collect_condition_interview(frontend.as_mut())?,
                        };
                        Ok(AmieOutcome::Condition(gateway.create(req).await?))
                    }
                    AmieSubcommand::List => Ok(AmieOutcome::Conditions(gateway.list().await?)),
                    AmieSubcommand::Show(name) => {
                        // One response shape for both gateways: the condition
                        // and its recent runs travel together, so the remote
                        // façade never has to guess which type came back.
                        let condition = gateway.get(&name).await?;
                        let runs = gateway.runs(&name, DEFAULT_RUN_HISTORY_LIMIT).await?;
                        Ok(AmieOutcome::Detail(ConditionDetail { condition, runs }))
                    }
                    AmieSubcommand::Remove { name, yes } => {
                        gateway.delete(&name).await?;
                        // The persistent directory removal is a filesystem
                        // concern that lives here, in Layer 2, guarded by the
                        // frontend's confirmation answer (or `-y`). The path is
                        // resolved through `AmiePaths::condition_dir`, which is
                        // `validate_under_root`-guarded, so a crafted name can
                        // never escape the conditions root.
                        let removed_dir = remove_condition_dir(frontend.as_mut(), &name, yes)?;
                        Ok(AmieOutcome::Removed { name, removed_dir })
                    }
                    AmieSubcommand::Pause(name) => {
                        gateway.set_status(&name, ConditionStatus::Paused).await?;
                        Ok(AmieOutcome::Ok)
                    }
                    AmieSubcommand::Resume(name) => {
                        gateway.set_status(&name, ConditionStatus::Active).await?;
                        Ok(AmieOutcome::Ok)
                    }
                    _ => unreachable!("daemon commands handled above"),
                }
            }
        }
    }
}

/// Collect a full `CreateCondition` from the frontend's interview answers.
/// Every field is asked; the frontend supplies the values and Layer 1 validates
/// them, so this reproduces the same condition from CLI and TUI given the same
/// answers.
fn collect_condition_interview(
    frontend: &mut dyn AmieCommandFrontend,
) -> Result<CreateCondition, CommandError> {
    let name = frontend.ask_condition_name()?;
    let description = frontend.ask_condition_description()?;
    let interval_raw = frontend.ask_condition_interval()?;
    let interval_secs =
        crate::command::dispatch::parse_amie_interval(&["amie", "add"], &interval_raw)?;
    let repo_scope = frontend.ask_condition_repo()?;
    let agent = frontend.ask_condition_agent()?;
    let model = frontend.ask_condition_model()?;
    let mount_scope = frontend.ask_condition_mount_scope()?;
    Ok(CreateCondition {
        name,
        description,
        repo_scope,
        mount_scope,
        interval_secs,
        agent,
        model,
    })
}

/// Remove a condition's persistent directory when confirmed. Resolves the path
/// through the `validate_under_root`-guarded `AmiePaths::condition_dir`, asks
/// the frontend (unless `-y`), and deletes only on a `true` answer. A missing
/// directory is not an error. Returns the directory actually removed, if any.
fn remove_condition_dir(
    frontend: &mut dyn AmieCommandFrontend,
    name: &str,
    yes: bool,
) -> Result<Option<PathBuf>, CommandError> {
    let paths = AmiePaths::from_process_env()?;
    let dir = paths.condition_dir(name)?;
    // Deletion requires an explicit `true`: `-y`, or a frontend that confirms.
    // A declined prompt (or one no frontend can answer — the daemon's API
    // frontend, an aborted dialog) keeps the directory rather than failing the
    // remove, whose gateway delete has already succeeded.
    let confirmed = yes || matches!(frontend.ask_delete_condition_dir(name, &dir), Ok(true));
    if !confirmed {
        return Ok(None);
    }
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(Some(dir)),
        // A condition with no persistent directory is a no-op, not a failure.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(crate::data::error::DataError::io(&dir, error).into()),
    }
}

fn daemon_outcome(value: AmieDaemonOutcome) -> Result<AmieOutcome, CommandError> {
    Ok(match value {
        AmieDaemonOutcome::Started {
            port,
            background,
            refreshed_key,
        } => AmieOutcome::Started {
            port,
            background,
            refreshed_key,
        },
        AmieDaemonOutcome::Stopped { stopped_pid } => AmieOutcome::Stopped { stopped_pid },
        AmieDaemonOutcome::Status(status) => AmieOutcome::Status(status),
        AmieDaemonOutcome::Logs { log_path } => AmieOutcome::Logs { log_path },
    })
}
