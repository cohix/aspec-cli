//! CLI attach entry point for a running squad task.

use std::process::ExitCode;

use clap::ArgMatches;

use crate::command::commands::squad::daemon::SquadSupervisor;
use crate::command::commands::squad::runtime_guard::SQUAD_SANDBOX_REFUSAL;
use crate::command::error::CommandError;
use crate::data::config::env::Env;
use crate::data::workflow_state::WorkflowState;
use crate::frontend::attach::{
    format_candidates, label_with_step_names, list_task_containers, resolve_attach_target,
    AttachResolution,
};
use crate::frontend::cli::{error_exit_code, format_error, CliFrontend, RuntimeContext};

/// Attach a single CLI terminal to the selected running squad container.
pub(crate) async fn run_attach(matches: &ArgMatches, ctx: &RuntimeContext) -> ExitCode {
    let Some(squad) = matches.subcommand_matches("squad") else {
        return render_error(&CommandError::unknown_command(&["squad", "attach"]));
    };
    let Some(attach) = squad.subcommand_matches("attach") else {
        return render_error(&CommandError::unknown_command(&["squad", "attach"]));
    };
    let Some(name) = attach.get_one::<String>("name") else {
        return render_error(&CommandError::missing_required_argument(
            &["squad", "attach"],
            "name",
        ));
    };

    if ctx.engines.container_runtime.is_none() {
        return render_error(&CommandError::Other(
            SQUAD_SANDBOX_REFUSAL.replace("{runtime}", ctx.engines.runtime.runtime_name()),
        ));
    }

    let mut candidates = match list_task_containers(ctx.engines.runtime.as_ref(), name) {
        Ok(candidates) => candidates,
        Err(error) => return render_error(&error),
    };
    // Prefix discovery above is authoritative. The daemon merely supplies
    // workflow-step labels when it happens to be reachable.
    if let Some(state) = workflow_state_for_labels(name).await {
        label_with_step_names(&mut candidates, &state);
    }
    let requested = attach.get_one::<String>("container").map(String::as_str);
    let target = match resolve_attach_target(candidates, name, requested) {
        Ok(AttachResolution::One(container)) => container,
        Ok(AttachResolution::Ambiguous(candidates)) => {
            eprintln!(
                "{}\nspecify one with --container <id>",
                format_candidates(&candidates)
            );
            return ExitCode::from(2);
        }
        Err(error) => return render_error(&error),
    };

    let instance = match ctx.engines.runtime.attach(&target.handle) {
        Ok(instance) => instance,
        Err(error) => return render_error(&error.into()),
    };
    let mut execution =
        match instance.run_with_frontend(Box::new(CliFrontend::new(matches.clone()))) {
            Ok(execution) => execution,
            Err(error) => return render_error(&error.into()),
        };
    match execution.wait().await {
        Ok(info) => ExitCode::from(u8::try_from(info.exit_code).unwrap_or(1)),
        Err(error) => render_error(&error.into()),
    }
}

fn render_error(error: &CommandError) -> ExitCode {
    eprintln!("{}", format_error(error));
    ExitCode::from(error_exit_code(error))
}

/// Fetch step labels only. Every error is deliberately swallowed: attaching
/// must continue to work while the daemon is stopped or has no workflow.
async fn workflow_state_for_labels(task: &str) -> Option<WorkflowState> {
    let supervisor = SquadSupervisor::from_env(&Env::from_process()).ok()?;
    let gateway = supervisor.gateway_from_meta().ok()??;
    let response = gateway
        .core()
        .get(&["tasks", task, "workflow"])
        .await
        .ok()?;
    serde_json::from_value(response.body).ok()
}
