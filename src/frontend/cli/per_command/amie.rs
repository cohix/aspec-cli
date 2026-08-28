//! CLI presentation for the amie command family.

use std::process::ExitCode;

use clap::ArgMatches;

use crate::command::commands::amie::commands::AmieOutcome;
use crate::command::commands::amie::daemon::AmieSupervisor;
use crate::command::commands::amie::gateway::ConditionGateway;
use crate::command::commands::amie::runtime_guard::require_container_tier;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::config::env::Env;

use super::render::format_table;
use crate::frontend::cli::{error_exit_code, format_error};

/// Bare, non-interactive `awman amie`: make the daemon available and report
/// exactly one gateway status response.
pub(crate) async fn run_bare(matches: &ArgMatches, engines: &Engines) -> ExitCode {
    let json = amie_flag(matches, "json");
    // A non-container runtime cannot host amie: fail fast with the shared
    // refusal rather than spawning a daemon child that will refuse (edge-case #1).
    if let Err(error) = require_container_tier(engines) {
        return render_failure(&error, json);
    }
    let supervisor = match AmieSupervisor::from_env(&Env::from_process()) {
        Ok(supervisor) => supervisor,
        Err(error) => return render_failure(&error, json),
    };
    let gateway = match supervisor.ensure_running().await {
        Ok(gateway) => gateway,
        Err(error) => return render_failure(&error, json),
    };
    // Disclosed on stderr so `--json` stdout stays machine-parseable.
    if let Some(setup) = supervisor.take_generated_key_setup() {
        eprintln!("{setup}");
    }
    match gateway.status().await {
        Ok(status) => {
            if let Some(output) = render_amie(&AmieOutcome::Status(status), json) {
                println!("{output}");
            }
            ExitCode::SUCCESS
        }
        Err(error) => render_failure(&error, json),
    }
}

/// Render an amie outcome through the CLI's common table/JSON conventions.
pub(crate) fn render_amie(outcome: &AmieOutcome, json: bool) -> Option<String> {
    if json {
        return Some(
            serde_json::to_string_pretty(outcome)
                .unwrap_or_else(|error| format!("failed to serialize amie outcome: {error}")),
        );
    }

    match outcome {
        AmieOutcome::Conditions(conditions) => {
            let rows = conditions
                .iter()
                .map(|condition| {
                    let status = format!("{:?}", condition.status).to_lowercase();
                    let last_run = condition
                        .last_run_at
                        .map(|time| time.to_rfc3339())
                        .unwrap_or_else(|| "—".into());
                    let next = if status == "paused" {
                        "paused".into()
                    } else {
                        condition
                            .backoff_until
                            .or_else(|| {
                                condition.last_run_at.map(|time| {
                                    time + chrono::Duration::seconds(condition.interval_secs as i64)
                                })
                            })
                            .map(|time| time.to_rfc3339())
                            .unwrap_or_else(|| "now".into())
                    };
                    vec![condition.name.clone(), status, last_run, next]
                })
                .collect::<Vec<_>>();
            Some(format_table(
                &["Name", "Status", "Last run", "Next evaluation"],
                &rows,
            ))
        }
        AmieOutcome::Detail(detail) => {
            let condition = &detail.condition;
            let rows = detail
                .runs
                .iter()
                .map(|run| {
                    vec![
                        run.started_at.to_rfc3339(),
                        format!("{:?}", run.status).to_lowercase(),
                        run.finished_at
                            .map(|time| time.to_rfc3339())
                            .unwrap_or_else(|| "—".into()),
                        run.error.clone().unwrap_or_else(|| "—".into()),
                    ]
                })
                .collect::<Vec<_>>();
            Some(format!(
                "Condition: {}\nDescription: {}\nRepository: {}\nInterval: {}s\nAgent: {}\nModel: {}\n\n{}",
                condition.name,
                condition.description,
                condition.repo_scope.display(),
                condition.interval_secs,
                condition.agent.as_deref().unwrap_or("default"),
                condition.model.as_deref().unwrap_or("default"),
                format_table(&["Started", "Status", "Finished", "Error"], &rows),
            ))
        }
        AmieOutcome::Condition(condition) => Some(format!("Created condition {}.", condition.name)),
        AmieOutcome::Removed { name, removed_dir } => Some(match removed_dir {
            Some(path) => format!("Removed condition {name} (deleted {}).", path.display()),
            None => format!("Removed condition {name}."),
        }),
        AmieOutcome::Ok => None,
        AmieOutcome::Status(status) => {
            if !status.running {
                return Some("amie daemon is not running.".into());
            }
            let pid = status
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "unknown".into());
            let address = status.bound_addr.as_deref().unwrap_or("unknown address");
            let last_tick = status
                .last_tick
                .map(|time| time.to_rfc3339())
                .unwrap_or_else(|| "never".into());
            Some(format!(
                "amie daemon running (PID {pid}) at {address}; {} conditions ({} active); last tick {last_tick}",
                status.condition_count, status.active_count
            ))
        }
        AmieOutcome::Logs { log_path } => Some(format!("Tailing amie logs at {log_path}")),
        // `--refresh-key` returns before anything is started, so saying
        // "started" here would contradict the key snippet just printed.
        AmieOutcome::Started {
            refreshed_key: true,
            ..
        } => Some("amie API key regenerated. Start the daemon with `awman amie start`.".into()),
        AmieOutcome::Started {
            port, background, ..
        } => Some(if *background {
            format!("amie daemon started in the background on port {port}.")
        } else {
            format!("amie daemon started on port {port}.")
        }),
        AmieOutcome::Stopped { stopped_pid } => Some(match stopped_pid {
            Some(pid) => format!("amie daemon (PID {pid}) stopped."),
            None => "amie daemon is not running.".into(),
        }),
    }
}

/// Render an amie command error, preserving structured stdout for JSON callers.
pub(crate) fn render_failure(error: &CommandError, json: bool) -> ExitCode {
    if json {
        println!("{}", serde_json::json!({ "error": format_error(error) }));
    } else {
        eprintln!("{}", format_error(error));
    }
    ExitCode::from(error_exit_code(error))
}

pub(crate) fn amie_flag(matches: &ArgMatches, flag: &str) -> bool {
    matches
        .subcommand_matches("amie")
        .and_then(|amie| amie.try_get_one::<bool>(flag).ok().flatten().copied())
        .unwrap_or(false)
}
