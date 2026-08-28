//! The single Layer-2 amie command family.

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
use crate::data::fs::condition_store::{Condition, ConditionStatus};
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
}

pub enum AmieSubcommand {
    Start(AmieStartFlags),
    Stop(AmieStopFlags),
    Status(AmieStatusFlags),
    Logs(AmieLogsFlags),
    Add(CreateCondition),
    List,
    Show(String),
    Remove(String),
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
        frontend: Self::Frontend,
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
            AmieSubcommand::Status(flags) => daemon_outcome(
                AmieDaemonCommand::new(AmieDaemonSubcommand::Status(flags), engines)
                    .run_with_frontend(frontend)
                    .await?,
            ),
            AmieSubcommand::Logs(flags) => daemon_outcome(
                AmieDaemonCommand::new(AmieDaemonSubcommand::Logs(flags), engines)
                    .run_with_frontend(frontend)
                    .await?,
            ),
            sub => {
                let gateway = gateway.ok_or_else(|| CommandError::Other("amie conditions are served by the amie daemon; start it with `awman amie start`".into()))?;
                match sub {
                    AmieSubcommand::Add(req) => {
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
                    AmieSubcommand::Remove(name) => {
                        gateway.delete(&name).await?;
                        Ok(AmieOutcome::Removed { name })
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
