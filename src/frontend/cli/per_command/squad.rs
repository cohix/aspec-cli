//! CLI presentation for the squad command family.

use std::process::ExitCode;

use clap::ArgMatches;

use crate::command::commands::squad::commands::SquadOutcome;
use crate::command::commands::squad::daemon::SquadSupervisor;
use crate::command::commands::squad::gateway::TaskGateway;
use crate::command::commands::squad::runtime_guard::require_container_tier;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::config::env::Env;

use super::render::format_table;
use crate::frontend::cli::{error_exit_code, format_error};

/// Bare, non-interactive `awman squad`: make the daemon available and report
/// exactly one gateway status response.
pub(crate) async fn run_bare(matches: &ArgMatches, engines: &Engines) -> ExitCode {
    let json = squad_flag(matches, "json");
    // A non-container runtime cannot host squad: fail fast with the shared
    // refusal rather than spawning a daemon child that will refuse (edge-case #1).
    if let Err(error) = require_container_tier(engines) {
        return render_failure(&error, json);
    }
    let supervisor = match SquadSupervisor::from_env(&Env::from_process()) {
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
            if let Some(output) = render_squad(&SquadOutcome::Status(status), json) {
                println!("{output}");
            }
            ExitCode::SUCCESS
        }
        Err(error) => render_failure(&error, json),
    }
}

/// Render a squad outcome through the CLI's common table/JSON conventions.
pub(crate) fn render_squad(outcome: &SquadOutcome, json: bool) -> Option<String> {
    if json {
        return Some(
            serde_json::to_string_pretty(outcome)
                .unwrap_or_else(|error| format!("failed to serialize squad outcome: {error}")),
        );
    }

    match outcome {
        SquadOutcome::Tasks(tasks) => {
            let rows = tasks
                .iter()
                .map(|task| {
                    let status = format!("{:?}", task.status).to_lowercase();
                    let last_run = task
                        .last_run_at
                        .map(|time| time.to_rfc3339())
                        .unwrap_or_else(|| "—".into());
                    let next = if status == "paused" {
                        "paused".into()
                    } else {
                        task.backoff_until
                            .or_else(|| {
                                task.last_run_at.map(|time| {
                                    time + chrono::Duration::seconds(task.interval_secs as i64)
                                })
                            })
                            .map(|time| time.to_rfc3339())
                            .unwrap_or_else(|| "now".into())
                    };
                    vec![task.name.clone(), status, last_run, next]
                })
                .collect::<Vec<_>>();
            Some(format_table(
                &["Name", "Status", "Last run", "Next evaluation"],
                &rows,
            ))
        }
        SquadOutcome::Detail(detail) => {
            let task = &detail.task;
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
            let workspace = if task.uses_worktree() {
                format!("{} (worktree-isolated)", task.repo_scope.display())
            } else {
                format!(
                    "{} (mounted directly, no worktree)",
                    task.repo_scope.display()
                )
            };
            let overlays = if task.overlays.is_empty() {
                "(none)".to_string()
            } else {
                task.overlays.join(", ")
            };
            Some(format!(
                "Task: {}\nDescription: {}\nWorkspace: {}\nOverlays: {}\nInterval: {}s\nAgent: {}\nModel: {}\n\n{}",
                task.name,
                task.description,
                workspace,
                overlays,
                task.interval_secs,
                task.agent.as_deref().unwrap_or("default"),
                task.model.as_deref().unwrap_or("default"),
                format_table(&["Started", "Status", "Finished", "Error"], &rows),
            ))
        }
        SquadOutcome::Task(task) => Some(format!("Created task {}.", task.name)),
        SquadOutcome::Removed { name, removed_dir } => Some(match removed_dir {
            Some(path) => format!("Removed task {name} (deleted {}).", path.display()),
            None => format!("Removed task {name}."),
        }),
        SquadOutcome::Ok => None,
        SquadOutcome::Status(status) => {
            if !status.running {
                return Some("squad daemon is not running.".into());
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
                "squad daemon running (PID {pid}) at {address}; {} tasks ({} active); last tick {last_tick}",
                status.task_count, status.active_count
            ))
        }
        SquadOutcome::Logs { log_path } => Some(format!("Tailing squad logs at {log_path}")),
        // `--refresh-key` returns before anything is started, so saying
        // "started" here would contradict the key snippet just printed.
        SquadOutcome::Started {
            refreshed_key: true,
            ..
        } => Some("squad API key regenerated. Start the daemon with `awman squad start`.".into()),
        SquadOutcome::Started {
            port, background, ..
        } => Some(if *background {
            format!("squad daemon started in the background on port {port}.")
        } else {
            format!("squad daemon started on port {port}.")
        }),
        SquadOutcome::Stopped { stopped_pid } => Some(match stopped_pid {
            Some(pid) => format!("squad daemon (PID {pid}) stopped."),
            None => "squad daemon is not running.".into(),
        }),
    }
}

/// Render a squad command error, preserving structured stdout for JSON callers.
pub(crate) fn render_failure(error: &CommandError, json: bool) -> ExitCode {
    if json {
        println!("{}", serde_json::json!({ "error": format_error(error) }));
    } else {
        eprintln!("{}", format_error(error));
    }
    ExitCode::from(error_exit_code(error))
}

pub(crate) fn squad_flag(matches: &ArgMatches, flag: &str) -> bool {
    matches
        .subcommand_matches("squad")
        .and_then(|squad| squad.try_get_one::<bool>(flag).ok().flatten().copied())
        .unwrap_or(false)
}
