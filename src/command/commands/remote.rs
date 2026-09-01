//! `RemoteCommand` — `remote run | session start | session kill`.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::remote_client::RemoteClient;
use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};

/// Build a `RemoteClient` for `addr`, pinning the locally-stored self-signed
/// cert when (a) the target is loopback AND (b) the cert PEM is on disk.
/// For non-loopback addresses we never pin — standard webpki verification
/// stays in force, and the caller is expected to use a publicly-trusted cert.
fn build_remote_client(
    engines: &Engines,
    addr: &str,
    api_key: Option<&crate::engine::auth::ApiKey>,
) -> Result<RemoteClient, CommandError> {
    let pinned: Option<String> = if RemoteClient::is_loopback_addr(addr) {
        let cert_path = engines.auth_engine.api_paths().tls_cert_file();
        std::fs::read_to_string(&cert_path).ok()
    } else {
        None
    };
    RemoteClient::new_with_pinned_cert(addr, api_key, pinned.as_deref())
}

#[derive(Debug, Clone)]
pub struct RemoteExecWorkflowFlags {
    pub workflow: std::path::PathBuf,
    pub work_item: Option<String>,
    pub agent: Option<String>,
    pub remote_addr: Option<String>,
    pub session: Option<String>,
    pub follow: bool,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RemoteExecPromptFlags {
    pub prompt: String,
    pub agent: Option<String>,
    pub remote_addr: Option<String>,
    pub session: Option<String>,
    pub follow: bool,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RemoteSessionStartFlags {
    pub session_type: String,
    pub workdir: Option<String>,
    pub repo_url: Option<String>,
    pub branch: Option<String>,
    pub remote_addr: Option<String>,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RemoteSessionKillFlags {
    pub session_id: Option<String>,
    pub remote_addr: Option<String>,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone)]
pub enum RemoteSubcommand {
    ExecWorkflow(RemoteExecWorkflowFlags),
    ExecPrompt(RemoteExecPromptFlags),
    SessionStart(RemoteSessionStartFlags),
    SessionKill(RemoteSessionKillFlags),
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoteExecOutcome {
    pub command_id: String,
    pub subcommand: String,
    pub session: String,
    pub remote_addr: String,
    pub status: Option<String>,
    pub exit_code: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoteSessionStartOutcome {
    pub session_id: String,
    pub remote_addr: String,
    pub setup_status: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoteSessionKillOutcome {
    pub session_id: String,
    pub remote_addr: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum RemoteOutcome {
    ExecWorkflow(RemoteExecOutcome),
    ExecPrompt(RemoteExecOutcome),
    SessionStart(RemoteSessionStartOutcome),
    SessionKill(RemoteSessionKillOutcome),
}

/// Frontend hooks for the `remote` command family. Default impls return safe
/// non-interactive choices (first option / declined save) so API dispatch
/// "just works"; CLI/TUI override to actually prompt.
pub trait RemoteCommandFrontend: UserMessageSink + Send + Sync {
    /// Choose one of multiple sessions reported by the server. Default: first.
    fn ask_session_picker(&mut self, sessions: &[String]) -> Result<String, CommandError> {
        sessions
            .first()
            .cloned()
            .ok_or(CommandError::RemoteSessionMissing)
    }

    /// Choose one of the user's saved working directories. Default: first.
    fn ask_saved_dir_picker(&mut self, dirs: &[String]) -> Result<String, CommandError> {
        dirs.first()
            .cloned()
            .ok_or_else(|| CommandError::MissingRequiredArgument {
                command: vec!["remote".into(), "session".into(), "start".into()],
                argument: "dir".into(),
            })
    }

    /// Choose which session to kill from a list. Default: first.
    fn ask_session_kill_picker(&mut self, sessions: &[String]) -> Result<String, CommandError> {
        sessions
            .first()
            .cloned()
            .ok_or(CommandError::RemoteSessionMissing)
    }

    /// Should the just-used directory be persisted to the user's saved-dirs
    /// list? Default: no — API mode never persists side-effects without
    /// an explicit signal.
    fn confirm_save_dir(&mut self, _dir: &str) -> Result<bool, CommandError> {
        Ok(false)
    }
}

pub struct RemoteCommand {
    sub: RemoteSubcommand,
    engines: Engines,
    session: crate::data::session::Session,
}

impl RemoteCommand {
    pub fn new(
        sub: RemoteSubcommand,
        engines: Engines,
        session: crate::data::session::Session,
    ) -> Self {
        Self {
            sub,
            engines,
            session,
        }
    }

    pub fn subcommand(&self) -> &RemoteSubcommand {
        &self.sub
    }
}

#[async_trait]
impl Command for RemoteCommand {
    type Frontend = Box<dyn RemoteCommandFrontend>;
    type Outcome = RemoteOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let session = self.session;
        let outcome = match self.sub {
            RemoteSubcommand::ExecWorkflow(f) => {
                run_remote_exec_workflow(&session, &self.engines, f, &mut *frontend).await?
            }
            RemoteSubcommand::ExecPrompt(f) => {
                run_remote_exec_prompt(&session, &self.engines, f, &mut *frontend).await?
            }
            RemoteSubcommand::SessionStart(f) => {
                run_session_start(&session, &self.engines, f, &mut *frontend).await?
            }
            RemoteSubcommand::SessionKill(f) => {
                run_session_kill(&session, &self.engines, f, &mut *frontend).await?
            }
        };
        frontend.replay_queued();
        Ok(outcome)
    }
}

fn resolve_addr(
    session: &crate::data::session::Session,
    flag: Option<&str>,
) -> Result<String, CommandError> {
    if let Some(a) = flag.filter(|s| !s.is_empty()) {
        return Ok(a.to_string());
    }
    session
        .effective_config()
        .remote_default_addr()
        .ok_or(CommandError::MissingRemoteAddress)
}

fn resolve_session_id(
    session: &crate::data::session::Session,
    flag: Option<&str>,
) -> Result<String, CommandError> {
    if let Some(s) = flag.filter(|s| !s.is_empty()) {
        return Ok(s.to_string());
    }
    session
        .effective_config()
        .remote_session()
        .ok_or(CommandError::RemoteSessionMissing)
}

struct RemoteExecParams<'a> {
    exec: crate::command::commands::remote_client::ExecArg,
    subcommand_name: &'a str,
    extra_args: Vec<String>,
    remote_addr: Option<&'a str>,
    session_flag: Option<&'a str>,
    follow: bool,
    api_key_flag: Option<&'a str>,
}

async fn run_remote_exec(
    session: &crate::data::session::Session,
    engines: &Engines,
    params: RemoteExecParams<'_>,
    frontend: &mut dyn UserMessageSink,
) -> Result<RemoteExecOutcome, CommandError> {
    use crate::command::commands::remote_client::{ExecJobResponse, ExecutionEventSink};
    use crate::data::execution_event::EventPayload;

    let subcommand_name = params.subcommand_name;
    let addr = resolve_addr(session, params.remote_addr)?;
    let session_id = resolve_session_id(session, params.session_flag)?;
    let api_key = RemoteClient::resolve_api_key(session, &addr, params.api_key_flag)?;
    let client = build_remote_client(engines, &addr, api_key.as_ref())?;

    let ExecJobResponse { command_id, .. } = client
        .exec_job(&session_id, params.exec, &params.extra_args)
        .await?;

    frontend.write_message(UserMessage {
        level: MessageLevel::Info,
        text: format!("Command submitted: {command_id}"),
    });

    let mut follow_exit_code: Option<i32> = None;

    if params.follow {
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "Streaming logs from server...".into(),
        });

        // Sink that routes typed events to stdout/stderr appropriately and
        // captures the final exit code.
        struct FollowSink<'a> {
            sink: &'a mut dyn UserMessageSink,
            exit_code: &'a mut Option<i32>,
            interrupted: std::sync::Arc<std::sync::atomic::AtomicBool>,
        }
        impl ExecutionEventSink for FollowSink<'_> {
            fn on_event(&mut self, event: crate::data::execution_event::ExecutionEvent) -> bool {
                if self.interrupted.load(std::sync::atomic::Ordering::Relaxed) {
                    return true;
                }
                match event.payload {
                    EventPayload::StdoutLine(line) => {
                        println!("{line}");
                    }
                    EventPayload::StderrLine(line) => {
                        eprintln!("{line}");
                    }
                    EventPayload::StatusMessage { phase, message } => {
                        self.sink.write_message(UserMessage {
                            level: MessageLevel::Info,
                            text: format!("[{phase}] {message}"),
                        });
                    }
                    EventPayload::WorkflowStepTransition {
                        step_name,
                        step_index,
                        to_status,
                        ..
                    } => {
                        self.sink.write_message(UserMessage {
                            level: MessageLevel::Info,
                            text: format!("[step {step_index}] {step_name} → {to_status}"),
                        });
                    }
                    EventPayload::WorkflowPhaseTransition {
                        phase,
                        step_desc,
                        status,
                    } => {
                        self.sink.write_message(UserMessage {
                            level: MessageLevel::Info,
                            text: format!("[{phase}] {step_desc} → {status}"),
                        });
                    }
                    EventPayload::WorkflowParallelStepLaunched {
                        step_name,
                        agent,
                        model,
                        ..
                    } => {
                        let model_suffix = model
                            .as_deref()
                            .map(|m| format!("::{m}"))
                            .unwrap_or_default();
                        self.sink.write_message(UserMessage {
                            level: MessageLevel::Info,
                            text: format!(
                                "[parallel] {step_name} launched ({agent}{model_suffix})"
                            ),
                        });
                    }
                    EventPayload::WorkflowParallelStepExited {
                        step_name,
                        exit_code,
                        ..
                    } => {
                        self.sink.write_message(UserMessage {
                            level: MessageLevel::Info,
                            text: format!("[parallel] {step_name} exited (exit {exit_code})"),
                        });
                    }
                    EventPayload::WorkflowParallelGroupFinished => {
                        self.sink.write_message(UserMessage {
                            level: MessageLevel::Info,
                            text: "[parallel] group finished".to_string(),
                        });
                    }
                    EventPayload::CommandStatus {
                        status,
                        exit_code,
                        error,
                    } => {
                        if let Some(code) = exit_code {
                            *self.exit_code = Some(code);
                        }
                        let mut text = format!("[status] {status}");
                        if let Some(err) = error {
                            text.push_str(&format!(" ({err})"));
                        }
                        let level = if status == "error" || status == "aborted" {
                            MessageLevel::Error
                        } else {
                            MessageLevel::Info
                        };
                        self.sink.write_message(UserMessage { level, text });
                    }
                    EventPayload::Done => {}
                }
                false
            }
        }

        // Ctrl-C handler: flip a flag the sink checks on every event.
        let interrupted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let interrupted_clone = interrupted.clone();
        let ctrlc_task = tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            interrupted_clone.store(true, std::sync::atomic::Ordering::Relaxed);
        });

        let mut sink = FollowSink {
            sink: frontend,
            exit_code: &mut follow_exit_code,
            interrupted: interrupted.clone(),
        };

        // Try once; on disconnect, retry once after 2s.
        let mut last_err: Option<CommandError> = None;
        for attempt in 0..2 {
            let result = client
                .stream_job_logs(&session_id, &command_id, &mut sink)
                .await;
            match result {
                Ok(()) => {
                    last_err = None;
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    if attempt == 0 && !interrupted.load(std::sync::atomic::Ordering::Relaxed) {
                        sink.sink.write_message(UserMessage {
                            level: MessageLevel::Warning,
                            text: "SSE connection dropped; retrying in 2 seconds...".into(),
                        });
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    } else {
                        break;
                    }
                }
            }
        }

        ctrlc_task.abort();

        if interrupted.load(std::sync::atomic::Ordering::Relaxed) {
            sink.sink.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: format!(
                    "Follow interrupted. Job is still running server-side. \
                     Check status with: awman remote exec {subcommand_name} --session {session_id} --job {command_id} status"
                ),
            });
        } else if let Some(e) = last_err {
            sink.sink.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!(
                    "Connection to server lost. The job may still be running. \
                     Check status with: awman remote exec {subcommand_name} --session {session_id} --job {command_id} status \
                     (last error: {e})"
                ),
            });
        }
    }

    let status_resp = client.get_job(&command_id).await;
    let (status, exit_code) = match status_resp {
        Ok(r) => (
            r.body["status"].as_str().map(|s| s.to_string()),
            r.body["exit_code"]
                .as_i64()
                .or_else(|| follow_exit_code.map(|c| c as i64)),
        ),
        Err(_) => (None, follow_exit_code.map(|c| c as i64)),
    };

    Ok(RemoteExecOutcome {
        command_id,
        subcommand: subcommand_name.to_string(),
        session: session_id,
        remote_addr: addr,
        status,
        exit_code,
    })
}

async fn run_remote_exec_workflow(
    session: &crate::data::session::Session,
    engines: &Engines,
    flags: RemoteExecWorkflowFlags,
    frontend: &mut dyn UserMessageSink,
) -> Result<RemoteOutcome, CommandError> {
    use crate::command::commands::remote_client::ExecArg;
    let mut extra: Vec<String> = Vec::new();
    if let Some(ref wi) = flags.work_item {
        extra.push("--work-item".into());
        extra.push(wi.clone());
    }
    if let Some(ref agent) = flags.agent {
        extra.push("--agent".into());
        extra.push(agent.clone());
    }

    let outcome = run_remote_exec(
        session,
        engines,
        RemoteExecParams {
            exec: ExecArg::Workflow(flags.workflow.to_string_lossy().into_owned()),
            subcommand_name: "workflow",
            extra_args: extra,
            remote_addr: flags.remote_addr.as_deref(),
            session_flag: flags.session.as_deref(),
            follow: flags.follow,
            api_key_flag: flags.api_key.as_deref(),
        },
        frontend,
    )
    .await?;

    Ok(RemoteOutcome::ExecWorkflow(outcome))
}

async fn run_remote_exec_prompt(
    session: &crate::data::session::Session,
    engines: &Engines,
    flags: RemoteExecPromptFlags,
    frontend: &mut dyn UserMessageSink,
) -> Result<RemoteOutcome, CommandError> {
    use crate::command::commands::remote_client::ExecArg;
    let mut extra: Vec<String> = Vec::new();
    if let Some(ref agent) = flags.agent {
        extra.push("--agent".into());
        extra.push(agent.clone());
    }

    let outcome = run_remote_exec(
        session,
        engines,
        RemoteExecParams {
            exec: ExecArg::Prompt(flags.prompt.clone()),
            subcommand_name: "prompt",
            extra_args: extra,
            remote_addr: flags.remote_addr.as_deref(),
            session_flag: flags.session.as_deref(),
            follow: flags.follow,
            api_key_flag: flags.api_key.as_deref(),
        },
        frontend,
    )
    .await?;

    Ok(RemoteOutcome::ExecPrompt(outcome))
}

async fn run_session_start(
    session: &crate::data::session::Session,
    engines: &Engines,
    flags: RemoteSessionStartFlags,
    frontend: &mut dyn UserMessageSink,
) -> Result<RemoteOutcome, CommandError> {
    use crate::command::commands::remote_client::StartSessionRequest;

    let addr = resolve_addr(session, flags.remote_addr.as_deref())?;
    let api_key = RemoteClient::resolve_api_key(session, &addr, flags.api_key.as_deref())?;
    let client = build_remote_client(engines, &addr, api_key.as_ref())?;

    let session_type = flags.session_type.to_lowercase();
    let req =
        match session_type.as_str() {
            "local" => {
                let workdir =
                    flags
                        .workdir
                        .clone()
                        .ok_or_else(|| CommandError::MissingRequiredArgument {
                            command: vec!["remote".into(), "session".into(), "start".into()],
                            argument: "workdir".into(),
                        })?;
                StartSessionRequest {
                    session_type: "local".into(),
                    workdir: Some(workdir),
                    repo_url: None,
                    branch: None,
                }
            }
            "remote" => {
                let repo_url = flags.repo_url.clone().ok_or_else(|| {
                    CommandError::MissingRequiredArgument {
                        command: vec!["remote".into(), "session".into(), "start".into()],
                        argument: "repo-url".into(),
                    }
                })?;
                StartSessionRequest {
                    session_type: "remote".into(),
                    workdir: None,
                    repo_url: Some(repo_url),
                    branch: flags.branch.clone(),
                }
            }
            other => {
                return Err(CommandError::Other(format!(
                    "--type must be 'local' or 'remote'; got '{other}'"
                )));
            }
        };

    // Detached-HEAD warning: only relevant for local sessions.
    if session_type == "local" && engines.git_engine.is_detached_head(session.git_root()) {
        frontend.write_message(UserMessage {
            level: MessageLevel::Warning,
            text: "detached HEAD — proceeding".into(),
        });
    }

    let resp = client.start_session(&req).await?;
    let session_id = resp.session_id;

    frontend.write_message(UserMessage {
        level: MessageLevel::Success,
        text: format!("Session created: {session_id}"),
    });

    frontend.write_message(UserMessage {
        level: MessageLevel::Info,
        text: format!("Waiting for session {session_id} setup to complete..."),
    });

    let interrupted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let interrupted_clone = interrupted.clone();
    let ctrlc_task = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        interrupted_clone.store(true, std::sync::atomic::Ordering::Relaxed);
    });

    let mut last_status_str = String::new();
    let mut printed_steps: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    loop {
        if interrupted.load(std::sync::atomic::Ordering::Relaxed) {
            ctrlc_task.abort();
            frontend.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: format!(
                    "Polling interrupted. Session setup is still running server-side. \
                     Check status with: awman remote session status {session_id}"
                ),
            });
            return Ok(RemoteOutcome::SessionStart(RemoteSessionStartOutcome {
                session_id,
                remote_addr: addr,
                setup_status: None,
            }));
        }

        // Poll first so a session that is already in a terminal state exits
        // immediately, then sleep before the next iteration.
        let status_resp = match client.get_session_status(&session_id).await {
            Ok(s) => s,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Warning,
                    text: format!("Failed to poll status: {e}"),
                });
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };
        let st = status_resp.state;
        let status_str = st.status.as_str().to_string();
        let stage = st.current_stage.clone().unwrap_or_default();
        if status_str != last_status_str || !stage.is_empty() {
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: format!("[{status_str}] {stage}"),
            });
            last_status_str = status_str.clone();
        }

        // Print any newly-updated step statuses.
        for entry in &st.ready_step_statuses {
            let key = entry.step.clone();
            let val = format!("{:?}", entry.status);
            match printed_steps.get(&key) {
                Some(prev) if prev == &val => continue,
                _ => {
                    printed_steps.insert(key.clone(), val.clone());
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Info,
                        text: format!("  {key:<20} {val}"),
                    });
                }
            }
        }

        match st.status {
            crate::data::session_setup_event::SessionSetupStatus::Ready => {
                ctrlc_task.abort();
                // Render the full ready summary box.
                if let Some(summary) = &st.ready_summary {
                    let box_str = render_ready_summary(summary);
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Info,
                        text: box_str,
                    });
                }
                frontend.write_message(UserMessage {
                    level: MessageLevel::Success,
                    text: format!("Session {session_id} is ready."),
                });
                return Ok(RemoteOutcome::SessionStart(RemoteSessionStartOutcome {
                    session_id,
                    remote_addr: addr,
                    setup_status: Some("ready".into()),
                }));
            }
            crate::data::session_setup_event::SessionSetupStatus::Failed => {
                ctrlc_task.abort();
                // Print partial step table if any.
                if !st.ready_step_statuses.is_empty() {
                    let mut text = String::from("Partial step status:\n");
                    for entry in &st.ready_step_statuses {
                        text.push_str(&format!("  {:<20} {:?}\n", entry.step, entry.status));
                    }
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text,
                    });
                }
                let err_msg = st
                    .error
                    .as_ref()
                    .map(|e| format!("{}: {}", e.stage, e.message))
                    .unwrap_or_else(|| "unknown error".into());
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("Session setup failed: {err_msg}"),
                });
                return Err(CommandError::Other(format!(
                    "Session setup failed: {err_msg}"
                )));
            }
            _ => {} // non-terminal, keep polling
        }

        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// Render the ReadySummary into a multi-line box similar to the CLI/TUI
/// `ready` rendering. Uses `render_summary_box` from the CLI helpers.
fn render_ready_summary(summary: &crate::engine::ready::summary::ReadySummary) -> String {
    use crate::data::step_status::{render_summary_box, StepStatus};
    let rows: Vec<(&str, &StepStatus)> = vec![
        ("Dockerfile", &summary.dockerfile),
        ("Base image", &summary.base_image),
        ("Agent image", &summary.agent_image),
        ("Local agent", &summary.local_agent),
        ("Audit", &summary.audit),
        ("Image rebuild", &summary.image_rebuild),
        ("aspec/", &summary.aspec_folder),
        ("Work items config", &summary.work_items_config),
    ];
    render_summary_box(&format!("Ready Summary ({})", summary.runtime_name), &rows)
}

async fn run_session_kill(
    session: &crate::data::session::Session,
    engines: &Engines,
    flags: RemoteSessionKillFlags,
    frontend: &mut dyn UserMessageSink,
) -> Result<RemoteOutcome, CommandError> {
    let session_id = flags
        .session_id
        .ok_or_else(|| CommandError::MissingRequiredArgument {
            command: vec!["remote".into(), "session".into(), "kill".into()],
            argument: "session_id".into(),
        })?;

    let addr = resolve_addr(session, flags.remote_addr.as_deref())?;
    let api_key = RemoteClient::resolve_api_key(session, &addr, flags.api_key.as_deref())?;
    let client = build_remote_client(engines, &addr, api_key.as_ref())?;

    match client.kill_session(&session_id).await {
        Ok(()) => {}
        Err(CommandError::RemoteHttpStatus { status: 404, .. }) => {}
        Err(CommandError::RemoteHttpStatus { status, body }) => {
            return Err(CommandError::RemoteSessionKillFailed {
                session_id,
                reason: format!("HTTP {status}: {body}"),
            });
        }
        Err(other) => {
            return Err(CommandError::RemoteSessionKillFailed {
                session_id,
                reason: other.to_string(),
            });
        }
    }

    frontend.write_message(UserMessage {
        level: MessageLevel::Success,
        text: format!("Session {} killed.", session_id),
    });

    Ok(RemoteOutcome::SessionKill(RemoteSessionKillOutcome {
        session_id,
        remote_addr: addr,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::env::EnvSnapshot;
    use crate::data::session::{Session, SessionOpenOptions};

    fn make_session_empty() -> (tempfile::TempDir, Session) {
        let tmp = tempfile::tempdir().unwrap();
        // Pin `AWMAN_CONFIG_HOME` at an empty tempdir so the session can't
        // fall through to the developer's real `~/.awman/config.json` (which
        // on a working machine may legitimately have `remote.defaultAddr`
        // configured and would silently invalidate the "no source" assertion).
        let env =
            EnvSnapshot::with_overrides([("AWMAN_CONFIG_HOME", tmp.path().to_str().unwrap())]);
        let opts = SessionOpenOptions {
            env: Some(env),
            ..Default::default()
        };
        let session =
            Session::open_at_git_root(tmp.path().to_path_buf(), tmp.path().to_path_buf(), opts)
                .unwrap();
        (tmp, session)
    }

    fn make_session_with_remote_addr(addr: &str) -> (tempfile::TempDir, Session) {
        let tmp = tempfile::tempdir().unwrap();
        let config_json = format!(r#"{{"remote":{{"defaultAddr":"{addr}"}}}}"#);
        std::fs::write(tmp.path().join("config.json"), &config_json).unwrap();
        let env =
            EnvSnapshot::with_overrides([("AWMAN_CONFIG_HOME", tmp.path().to_str().unwrap())]);
        let opts = SessionOpenOptions {
            env: Some(env),
            ..Default::default()
        };
        let session =
            Session::open_at_git_root(tmp.path().to_path_buf(), tmp.path().to_path_buf(), opts)
                .unwrap();
        (tmp, session)
    }

    // ─── resolve_addr ─────────────────────────────────────────────────────────

    #[test]
    fn resolve_addr_flag_wins_over_config() {
        let (_tmp, session) = make_session_with_remote_addr("http://config-host:9876");
        let addr = resolve_addr(&session, Some("http://flag-host:1234")).unwrap();
        assert_eq!(addr, "http://flag-host:1234");
    }

    #[test]
    fn resolve_addr_falls_back_to_config() {
        let (_tmp, session) = make_session_with_remote_addr("http://config-host:9876");
        let addr = resolve_addr(&session, None).unwrap();
        assert_eq!(addr, "http://config-host:9876");
    }

    #[test]
    fn resolve_addr_empty_flag_falls_back_to_config() {
        let (_tmp, session) = make_session_with_remote_addr("http://config-host:9876");
        let addr = resolve_addr(&session, Some("")).unwrap();
        assert_eq!(addr, "http://config-host:9876");
    }

    #[test]
    fn resolve_addr_errors_when_no_source_available() {
        let (_tmp, session) = make_session_empty();
        let result = resolve_addr(&session, None);
        assert!(
            matches!(result, Err(CommandError::MissingRemoteAddress)),
            "must error when no addr source: {result:?}"
        );
    }
}
