//! Per-variant CommandOutcome → user-facing string renderers.
//!
//! Each `CommandOutcome` variant gets a small, focused renderer that returns
//! the human-readable text the CLI prints to stdout on success. Renderers
//! return `None` when there is nothing additional to say beyond what the
//! engine already streamed via `report_step_status` / `report_summary` (that
//! output is already on stderr by the time the outcome is rendered).
//!
//! The whole module is pure: it never touches I/O or globals. Tests can call
//! any renderer directly with a synthesised outcome.

use crate::command::commands::api_server::{
    ApiServerKillOutcome, ApiServerLogsOutcome, ApiServerOutcome, ApiServerStartOutcome,
    ApiServerStatusOutcome,
};
use crate::command::commands::auth::AuthOutcome;
use crate::command::commands::chat::ChatOutcome;
use crate::command::commands::config::{
    ConfigGetOutcome, ConfigOutcome, ConfigSetOutcome, ConfigShowOutcome,
};
use crate::command::commands::download::DownloadOutcome;
use crate::command::commands::exec_prompt::ExecPromptOutcome;
use crate::command::commands::exec_workflow::ExecWorkflowOutcome;
use crate::command::commands::init::InitOutcome;
use crate::command::commands::new::{
    NewOutcome, NewSkillOutcome, NewSpecOutcome, NewWorkflowOutcome,
};
use crate::command::commands::ready::ReadyOutcome;
use crate::command::commands::remote::{
    RemoteExecOutcome, RemoteOutcome, RemoteSessionKillOutcome, RemoteSessionStartOutcome,
};
use crate::command::commands::specs::{SpecsAmendOutcome, SpecsOutcome};
use crate::command::commands::status::{StatusContainerRow, StatusOutcome};
use crate::command::CommandOutcome;

// ─── Top-level dispatcher ────────────────────────────────────────────────────

/// Format a [`CommandOutcome`] into the success-path stdout text. Returns
/// `None` when no extra output is needed (engines that stream their progress
/// to stderr already and produce no additional summary on stdout).
pub fn render(outcome: &CommandOutcome, json: bool) -> Option<String> {
    match outcome {
        CommandOutcome::Empty => None,
        CommandOutcome::Status(o) => Some(render_status(o)),
        CommandOutcome::Chat(o) => render_chat(o),
        CommandOutcome::Init(o) => render_init(o),
        CommandOutcome::Ready(o) => render_ready(o),
        CommandOutcome::ExecPrompt(o) => render_exec_prompt(o),
        CommandOutcome::ExecWorkflow(o) => render_exec_workflow(o),
        CommandOutcome::Config(o) => render_config(o),
        CommandOutcome::ApiServer(o) => render_api_server(o),
        CommandOutcome::Remote(o) => render_remote(o),
        CommandOutcome::New(o) => render_new(o),
        CommandOutcome::Specs(o) => render_specs(o),
        CommandOutcome::Auth(o) => render_auth(o),
        CommandOutcome::Download(o) => render_download(o),
        CommandOutcome::Clean(o) => render_clean(o),
        CommandOutcome::Squad(o) => super::squad::render_squad(o, json),
    }
}

// ─── clean ─────────────────────────────────────────────────────────────────────
//
// `awman clean` streams its itemized summary, confirmation, and result
// messages through the frontend during the run (CLI: stdout + message queue).
// The success outcome adds nothing further on stdout.

fn render_clean(_o: &crate::command::commands::clean::CleanOutcome) -> Option<String> {
    None
}

// ─── status ──────────────────────────────────────────────────────────────────

pub fn render_status(o: &StatusOutcome) -> String {
    let mut out = String::new();
    out.push_str("AWMAN STATUS DASHBOARD\n\n");

    out.push_str("CODE AGENTS\n");
    if o.containers.is_empty() {
        out.push_str("  No code agents running.\n");
        out.push_str("  To start one: awman exec workflow <file>  or  awman chat\n");
    } else {
        let headers = [
            "●",
            "Container",
            "ID",
            "Image",
            "CPU%",
            "Mem MB",
            "Started",
            "Source",
        ];
        let rows: Vec<Vec<String>> = o.containers.iter().map(render_container_row).collect();
        out.push_str(&format_table(&headers, &rows));
    }

    out.push_str(&format!("\nTip: {}\n", o.tip));
    out
}

fn render_container_row(c: &StatusContainerRow) -> Vec<String> {
    let indicator = if c.stuck { "🟡" } else { "🟢" };
    let cpu = c
        .cpu_percent
        .map(|v| format!("{v:>5.1}"))
        .unwrap_or_else(|| "  -  ".to_string());
    let mem = c
        .memory_mb
        .map(|v| format!("{v:>6.1}"))
        .unwrap_or_else(|| "   -  ".to_string());
    vec![
        indicator.to_string(),
        c.name.clone(),
        c.id.chars().take(12).collect(),
        c.image.clone(),
        cpu,
        mem,
        c.started_at.clone(),
        c.source_label().unwrap_or_else(|| "session".to_string()),
    ]
}

pub(crate) fn format_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let ncols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(ncols) {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    out.push('┌');
    for (i, w) in widths.iter().enumerate() {
        out.push_str(&"─".repeat(w + 2));
        out.push(if i + 1 < ncols { '┬' } else { '┐' });
    }
    out.push('\n');
    out.push('│');
    for (h, w) in headers.iter().zip(widths.iter()) {
        let pad = w.saturating_sub(h.chars().count());
        out.push_str(&format!(" {h}{} │", " ".repeat(pad)));
    }
    out.push('\n');
    out.push('├');
    for (i, w) in widths.iter().enumerate() {
        out.push_str(&"─".repeat(w + 2));
        out.push(if i + 1 < ncols { '┼' } else { '┤' });
    }
    out.push('\n');
    for row in rows {
        out.push('│');
        for (cell, w) in row.iter().zip(widths.iter()) {
            let pad = w.saturating_sub(cell.chars().count());
            out.push_str(&format!(" {cell}{} │", " ".repeat(pad)));
        }
        out.push('\n');
    }
    out.push('└');
    for (i, w) in widths.iter().enumerate() {
        out.push_str(&"─".repeat(w + 2));
        out.push(if i + 1 < ncols { '┴' } else { '┘' });
    }
    out.push('\n');
    out
}

// ─── chat / exec prompt / exec workflow ──────────────────────────────────────
//
// These commands stream the container's stdout/stderr directly to the host
// during the run. The success outcome is intentionally minimal — a one-line
// confirmation, only when there's something interesting to say.

fn render_chat(o: &ChatOutcome) -> Option<String> {
    match o.exit_code {
        Some(0) | None => None,
        Some(code) => Some(format!("Chat session ended with exit code {code}.")),
    }
}

fn render_exec_prompt(o: &ExecPromptOutcome) -> Option<String> {
    match o.exit_code {
        Some(0) | None => None,
        Some(code) => Some(format!("exec prompt ended with exit code {code}.")),
    }
}

fn render_exec_workflow(o: &ExecWorkflowOutcome) -> Option<String> {
    let exit = match o.exit_code {
        Some(c) if c != 0 => format!(" (exit {c})"),
        _ => String::new(),
    };
    let wt = if o.worktree_used {
        " in isolated worktree"
    } else {
        ""
    };
    Some(format!("Workflow {} completed{exit}{wt}.", o.workflow))
}

// ─── init / ready ────────────────────────────────────────────────────────────
//
// These engines emit their summary box via `report_summary` (replayed to
// stderr from the message queue). The success-path stdout output is None.

fn render_init(_o: &InitOutcome) -> Option<String> {
    None
}

fn render_ready(o: &ReadyOutcome) -> Option<String> {
    if o.json_requested {
        Some(serde_json::to_string_pretty(&o.to_legacy_json()).unwrap_or_else(|_| "{}".into()))
    } else {
        None
    }
}

// ─── config ──────────────────────────────────────────────────────────────────

fn render_config(o: &ConfigOutcome) -> Option<String> {
    match o {
        ConfigOutcome::Show(s) => Some(render_config_show(s)),
        ConfigOutcome::Get(g) => Some(render_config_get(g)),
        ConfigOutcome::Set(s) => Some(render_config_set(s)),
    }
}

fn render_config_show(o: &ConfigShowOutcome) -> String {
    let na = "—";
    let headers = ["Field", "Global", "Repo", "Effective"];
    let rows: Vec<Vec<String>> = o
        .rows
        .iter()
        .map(|r| {
            let label = if r.read_only {
                format!("{} (read-only)", r.field)
            } else {
                r.field.clone()
            };
            vec![
                label,
                r.global_value.clone().unwrap_or_else(|| na.to_string()),
                r.repo_value.clone().unwrap_or_else(|| na.to_string()),
                r.effective_value.clone().unwrap_or_else(|| na.to_string()),
            ]
        })
        .collect();
    let mut out = String::from("AWMAN CONFIG\n\n");
    out.push_str(&format_table(&headers, &rows));
    out
}

fn render_config_get(o: &ConfigGetOutcome) -> String {
    let na = || "N/A".to_string();
    format!(
        "Field: {}\n  Global:    {}\n  Repo:      {}\n  Effective: {}",
        o.field,
        o.global_value.clone().unwrap_or_else(na),
        o.repo_value.clone().unwrap_or_else(na),
        o.effective_value.clone().unwrap_or_else(na),
    )
}

fn render_config_set(o: &ConfigSetOutcome) -> String {
    format!("Set {} ({}) = {}", o.field, o.scope, o.value)
}

// ─── api ────────────────────────────────────────────────────────────────

fn render_api_server(o: &ApiServerOutcome) -> Option<String> {
    match o {
        // In foreground mode the "started" message is printed at server
        // boot (before the accept loop blocks). When that future finally
        // returns it's because the server has *shut down* — printing
        // "API server started …" at that point is misleading and looked
        // like noise dumped after Ctrl-C.
        ApiServerOutcome::Start(s) if !s.background => None,
        ApiServerOutcome::Start(s) => Some(render_api_server_start(s)),
        ApiServerOutcome::Kill(k) => Some(render_api_server_kill(k)),
        ApiServerOutcome::Logs(l) => Some(render_api_server_logs(l)),
        ApiServerOutcome::Status(s) => Some(render_api_server_status(s)),
    }
}

fn render_api_server_start(o: &ApiServerStartOutcome) -> String {
    let mode = if o.background {
        "background"
    } else {
        "foreground"
    };
    let workdirs = if o.workdirs.is_empty() {
        "<none>".to_string()
    } else {
        o.workdirs.join(", ")
    };
    let key = if o.refreshed_key {
        " (api key refreshed)"
    } else {
        ""
    };
    format!(
        "API server started on port {} in {mode} mode.\n  workdirs: {workdirs}{key}",
        o.port
    )
}

fn render_api_server_kill(o: &ApiServerKillOutcome) -> String {
    match o.stopped_pid {
        Some(pid) => format!("API server (PID {pid}) stopped."),
        None => "API server is not running.".to_string(),
    }
}

fn render_api_server_logs(o: &ApiServerLogsOutcome) -> String {
    if o.log_path.is_empty() {
        "No API server log found.".to_string()
    } else {
        format!("Tailing API server logs at {}", o.log_path)
    }
}

fn render_api_server_status(o: &ApiServerStatusOutcome) -> String {
    if !o.running {
        return "API server is not running.".to_string();
    }
    let pid_part = o.pid.map(|p| format!(" (PID {p})")).unwrap_or_default();
    let addr_part = o
        .bound_addr
        .as_deref()
        .map(|a| format!(" at {a}"))
        .unwrap_or_default();
    let version_part = o
        .version
        .as_deref()
        .map(|v| format!(", version {v}"))
        .unwrap_or_default();
    let responsive_part = if o.responsive {
        ""
    } else {
        " — process alive but HTTP probe failed"
    };
    format!("API server is running{pid_part}{addr_part}{version_part}{responsive_part}.")
}

// ─── remote ──────────────────────────────────────────────────────────────────

fn render_remote(o: &RemoteOutcome) -> Option<String> {
    match o {
        RemoteOutcome::ExecWorkflow(r) => Some(render_remote_exec(r)),
        RemoteOutcome::ExecPrompt(r) => Some(render_remote_exec(r)),
        RemoteOutcome::SessionStart(s) => Some(render_remote_session_start(s)),
        RemoteOutcome::SessionKill(k) => Some(render_remote_session_kill(k)),
    }
}

fn render_remote_exec(o: &RemoteExecOutcome) -> String {
    let status_part = o
        .status
        .as_deref()
        .map(|s| format!(" [{s}]"))
        .unwrap_or_default();
    format!(
        "Command {}: {} (session {}) via {}{status_part}",
        o.command_id, o.subcommand, o.session, o.remote_addr,
    )
}

fn render_remote_session_start(o: &RemoteSessionStartOutcome) -> String {
    let status_part = o
        .setup_status
        .as_deref()
        .map(|s| format!(" [{s}]"))
        .unwrap_or_default();
    format!(
        "Session {} created via {}{status_part}.",
        o.session_id, o.remote_addr,
    )
}

fn render_remote_session_kill(o: &RemoteSessionKillOutcome) -> String {
    format!("Session {} killed via {}.", o.session_id, o.remote_addr)
}

// ─── new ─────────────────────────────────────────────────────────────────────

fn render_new(o: &NewOutcome) -> Option<String> {
    match o {
        NewOutcome::Spec(s) => Some(render_new_spec(s)),
        NewOutcome::Workflow(w) => Some(render_new_workflow(w)),
        NewOutcome::Skill(s) => render_new_skill(s),
    }
}

fn render_new_spec(o: &NewSpecOutcome) -> String {
    match &o.path {
        Some(p) => format!("Created work item: {p}"),
        None => "Work item created.".to_string(),
    }
}

fn render_new_workflow(o: &NewWorkflowOutcome) -> String {
    let scope = if o.global { "global" } else { "repo" };
    match &o.path {
        Some(p) => format!("Created workflow ({scope}, format={}): {p}", o.format),
        None => format!("Workflow created ({scope}, format={}).", o.format),
    }
}

fn render_new_skill(o: &NewSkillOutcome) -> Option<String> {
    // `--pull` / `--pull-all` create nothing and are not repo-scoped: they
    // clone or refresh a managed library under the global skills store, and
    // already reported each library through the message sink. Rendering a
    // "Created skill (repo)" summary here would contradict both.
    if o.pull {
        return None;
    }
    let scope = if o.global { "global" } else { "repo" };
    Some(match &o.path {
        Some(p) => format!("Created skill ({scope}): {p}"),
        None => format!("Skill created ({scope})."),
    })
}

// ─── specs / auth / download ─────────────────────────────────────────────────

fn render_specs(o: &SpecsOutcome) -> Option<String> {
    match o {
        SpecsOutcome::Amend(a) => Some(render_specs_amend(a)),
    }
}

fn render_specs_amend(o: &SpecsAmendOutcome) -> String {
    format!("Amended work item {}.", o.work_item)
}

fn render_auth(o: &AuthOutcome) -> Option<String> {
    let head = if o.accepted {
        "Agent auth consent accepted for this repo."
    } else {
        "Agent auth consent declined for this repo."
    };
    Some(format!("{head} persisted={}", o.persisted))
}

fn render_download(o: &DownloadOutcome) -> Option<String> {
    let dest = o
        .dest_path
        .as_deref()
        .map(|p| format!(" -> {p}"))
        .unwrap_or_default();
    Some(format!(
        "Downloaded asset: {}{} ({} bytes)",
        o.asset, dest, o.bytes_written
    ))
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::commands::status::{ContainerKind, ContainerSource, StatusOutcome};
    use crate::engine::step_status::StepStatus;

    #[test]
    fn render_empty_returns_none() {
        assert!(render(&CommandOutcome::Empty, false).is_none());
    }

    #[test]
    fn render_status_empty_state_message() {
        let o = StatusOutcome {
            containers: vec![],
            watched: false,
            tip: "test tip".into(),
        };
        let s = render_status(&o);
        assert!(s.contains("AWMAN STATUS DASHBOARD"));
        assert!(s.contains("No code agents running"));
        assert!(s.contains("Tip: test tip"));
    }

    #[test]
    fn render_status_with_one_agent_container() {
        let o = StatusOutcome {
            containers: vec![StatusContainerRow {
                id: "abc1234567890".into(),
                name: "awman-1".into(),
                image: "awman/dev:latest".into(),
                started_at: "2025-01-01T00:00:00Z".into(),
                kind: ContainerKind::Agent,
                source: ContainerSource::Session,
                tab_number: None,
                stuck: false,
                command_label: None,
                cpu_percent: None,
                memory_mb: None,
            }],
            watched: false,
            tip: "test tip".into(),
        };
        let s = render_status(&o);
        assert!(s.contains("CODE AGENTS"), "{s}");
        assert!(s.contains("awman-1"), "{s}");
    }

    #[test]
    fn render_status_marks_squad_container_and_leaves_session_row_plain() {
        let o = StatusOutcome {
            containers: vec![
                StatusContainerRow {
                    id: "abc1234567890".into(),
                    name: "awman-1-2".into(),
                    image: "awman/dev:latest".into(),
                    started_at: "2025-01-01T00:00:00Z".into(),
                    kind: ContainerKind::Agent,
                    source: ContainerSource::Session,
                    tab_number: None,
                    stuck: false,
                    command_label: None,
                    cpu_percent: None,
                    memory_mb: None,
                },
                StatusContainerRow {
                    id: "def1234567890".into(),
                    name: "awman-squad-issue-triage-12ab34cd".into(),
                    image: "awman/dev:latest".into(),
                    started_at: "2025-01-01T00:00:00Z".into(),
                    kind: ContainerKind::Agent,
                    source: ContainerSource::Squad("issue-triage".into()),
                    tab_number: None,
                    stuck: false,
                    command_label: None,
                    cpu_percent: None,
                    memory_mb: None,
                },
            ],
            watched: false,
            tip: "test tip".into(),
        };
        let s = render_status(&o);
        assert!(s.contains("squad:issue-triage"), "{s}");
        assert!(s.contains("session"), "{s}");
    }

    #[test]
    fn render_chat_clean_exit_returns_none() {
        let o = ChatOutcome {
            agent: Some("claude".into()),
            exit_code: Some(0),
        };
        assert!(render_chat(&o).is_none());
        let o2 = ChatOutcome {
            agent: None,
            exit_code: None,
        };
        assert!(render_chat(&o2).is_none());
    }

    #[test]
    fn render_chat_nonzero_exit_returns_some() {
        let o = ChatOutcome {
            agent: None,
            exit_code: Some(2),
        };
        assert_eq!(
            render_chat(&o).unwrap(),
            "Chat session ended with exit code 2."
        );
    }

    #[test]
    fn render_init_returns_none_so_summary_box_is_only_output() {
        let o = InitOutcome {
            agent: "claude".into(),
            aspec_requested: true,
            summary: crate::command::commands::init::SerializableInitSummary {
                aspec_folder: StepStatus::Done,
                dockerfile: StepStatus::Done,
                config: StepStatus::Done,
                audit: StepStatus::Skipped,
                image_build: StepStatus::Skipped,
                work_items_setup: StepStatus::Skipped,
            },
        };
        assert!(render_init(&o).is_none());
    }

    #[test]
    fn render_ready_returns_none_so_summary_box_is_only_output() {
        let o = ReadyOutcome {
            runtime: "docker".into(),
            dockerfile: StepStatus::Done,
            base_image: StepStatus::Done,
            agent_image: StepStatus::Done,
            local_agent: StepStatus::Done,
            audit: StepStatus::Skipped,
            image_rebuild: StepStatus::Skipped,
            agent_credentials: Vec::new(),
            non_default_agent_images: Vec::new(),
            json_requested: false,
            refresh_requested: false,
        };
        assert!(render_ready(&o).is_none());
    }

    #[test]
    fn render_config_get_handles_missing_values() {
        let o = ConfigGetOutcome {
            field: "agent".into(),
            global_value: Some("claude".into()),
            repo_value: None,
            effective_value: Some("claude".into()),
        };
        let s = render_config_get(&o);
        assert!(s.contains("Field: agent"));
        assert!(s.contains("Global:    claude"));
        assert!(s.contains("Repo:      N/A"));
        assert!(s.contains("Effective: claude"));
    }

    #[test]
    fn render_api_server_status_running_with_pid() {
        let s = render_api_server_status(&ApiServerStatusOutcome {
            running: true,
            pid: Some(1234),
            bound_addr: Some("https://127.0.0.1:9876".into()),
            version: Some("0.7.0".into()),
            responsive: true,
        });
        assert!(s.contains("API server is running"));
        assert!(s.contains("PID 1234"));
        assert!(s.contains("at https://127.0.0.1:9876"));
        assert!(s.contains("version 0.7.0"));
        assert!(!s.contains("HTTP probe failed"));
    }

    #[test]
    fn render_api_server_status_alive_but_unresponsive() {
        let s = render_api_server_status(&ApiServerStatusOutcome {
            running: true,
            pid: Some(1234),
            bound_addr: None,
            version: None,
            responsive: false,
        });
        assert!(s.contains("HTTP probe failed"));
    }

    #[test]
    fn render_api_server_status_not_running() {
        let s = render_api_server_status(&ApiServerStatusOutcome {
            running: false,
            pid: None,
            bound_addr: None,
            version: None,
            responsive: false,
        });
        assert_eq!(s, "API server is not running.");
    }

    #[test]
    fn render_remote_exec_includes_session_when_present() {
        let s = render_remote_exec(&RemoteExecOutcome {
            command_id: "cmd-1".into(),
            subcommand: "exec workflow".into(),
            session: "abc123".into(),
            remote_addr: "localhost:9876".into(),
            status: None,
            exit_code: None,
        });
        assert!(s.contains("exec workflow"));
        assert!(s.contains("abc123"));
    }

    #[test]
    fn render_auth_accepted_vs_declined() {
        assert!(render_auth(&AuthOutcome {
            accepted: true,
            persisted: true
        })
        .unwrap()
        .contains("accepted"));
        assert!(render_auth(&AuthOutcome {
            accepted: false,
            persisted: true
        })
        .unwrap()
        .contains("declined"));
    }

    // ── render_ready ──────────────────────────────────────────────────────────

    #[test]
    fn render_ready_json_requested_emits_legacy_schema() {
        let o = ReadyOutcome {
            runtime: "docker".into(),
            dockerfile: StepStatus::Done,
            base_image: StepStatus::Done,
            agent_image: StepStatus::Done,
            local_agent: StepStatus::Done,
            audit: StepStatus::Skipped,
            image_rebuild: StepStatus::Skipped,
            agent_credentials: Vec::new(),
            non_default_agent_images: Vec::new(),
            json_requested: true,
            refresh_requested: false,
        };
        let s = render_ready(&o).expect("json_requested=true must produce output");
        let parsed: serde_json::Value = serde_json::from_str(&s).expect("must be valid JSON");
        // Top-level keys per legacy schema.
        assert_eq!(parsed["ready"], true);
        assert_eq!(parsed["runtime"], "docker");
        assert!(parsed["steps"].is_object(), "must include steps wrapper");
        // Each legacy step must be a `{status, message}` object.
        for key in [
            "docker_daemon",
            "dockerfile",
            "aspec_folder",
            "work_items_config",
            "local_agent",
            "dev_image",
            "refresh",
            "image_rebuild",
        ] {
            let s = &parsed["steps"][key];
            assert!(s.is_object(), "steps.{key} must be an object");
            assert!(
                s.get("status").is_some(),
                "steps.{key} must have a status field"
            );
            assert!(
                s.get("message").is_some(),
                "steps.{key} must have a message field"
            );
        }
        assert_eq!(parsed["steps"]["dev_image"]["status"], "ok");
        assert_eq!(parsed["steps"]["aspec_folder"]["status"], "skipped");
    }

    #[test]
    fn render_ready_json_failure_marks_ready_false() {
        let o = ReadyOutcome {
            runtime: "docker".into(),
            dockerfile: StepStatus::Done,
            base_image: StepStatus::Failed("boom".into()),
            agent_image: StepStatus::Pending,
            local_agent: StepStatus::Pending,
            audit: StepStatus::Pending,
            image_rebuild: StepStatus::Pending,
            agent_credentials: Vec::new(),
            non_default_agent_images: Vec::new(),
            json_requested: true,
            refresh_requested: true,
        };
        let s = render_ready(&o).expect("json_requested=true must produce output");
        let parsed: serde_json::Value = serde_json::from_str(&s).expect("must be valid JSON");
        assert_eq!(parsed["ready"], false);
        assert_eq!(parsed["steps"]["dev_image"]["status"], "pending");
        assert_eq!(parsed["steps"]["refresh"]["status"], "ok");
    }

    // ── render_config ─────────────────────────────────────────────────────────

    #[test]
    fn render_config_show_renders_4_column_table_with_field_values() {
        use crate::command::commands::config::{ConfigFieldKind, ConfigFieldRow};
        let o = ConfigShowOutcome {
            global: serde_json::json!({"agent": "claude"}),
            repo: serde_json::json!({}),
            rows: vec![
                ConfigFieldRow {
                    field: "agent".into(),
                    global_value: Some("claude".into()),
                    repo_value: None,
                    effective_value: Some("claude".into()),
                    kind: ConfigFieldKind::Enum,
                    read_only: false,
                    global_writable: true,
                    repo_writable: true,
                    value_hint: None,
                },
                ConfigFieldRow {
                    field: "auto_agent_auth_accepted".into(),
                    global_value: Some("true".into()),
                    repo_value: None,
                    effective_value: Some("true".into()),
                    kind: ConfigFieldKind::Bool,
                    read_only: true,
                    global_writable: false,
                    repo_writable: false,
                    value_hint: None,
                },
            ],
        };
        let s = render_config_show(&o);
        assert!(s.contains("AWMAN CONFIG"), "must have header");
        assert!(s.contains("Field"), "must have Field column");
        assert!(s.contains("Global"), "must have Global column");
        assert!(s.contains("Repo"), "must have Repo column");
        assert!(s.contains("Effective"), "must have Effective column");
        assert!(s.contains("agent"), "agent field row must appear");
        assert!(s.contains("claude"), "global agent value must appear");
        assert!(
            s.contains("(read-only)"),
            "read-only fields must be marked: {s}"
        );
    }

    #[test]
    fn render_config_show_does_not_truncate_dynamic_workflows_values() {
        use crate::command::commands::config::{ConfigFieldKind, ConfigFieldRow};
        let long_models = "claude-opus-4-8, claude-sonnet-4-6, claude-haiku-4-5, \
                            gemini-2.5-pro, codex-mini-latest";
        let o = ConfigShowOutcome {
            global: serde_json::json!({}),
            repo: serde_json::json!({}),
            rows: vec![
                ConfigFieldRow {
                    field: "dynamicWorkflows.defaultLeader".into(),
                    global_value: None,
                    repo_value: Some("claude::claude-opus-4-8".into()),
                    effective_value: Some("claude::claude-opus-4-8".into()),
                    kind: ConfigFieldKind::String,
                    read_only: false,
                    global_writable: false,
                    repo_writable: true,
                    value_hint: None,
                },
                ConfigFieldRow {
                    field: "dynamicWorkflows.maxConcurrentSteps".into(),
                    global_value: None,
                    repo_value: Some("3".into()),
                    effective_value: Some("3".into()),
                    kind: ConfigFieldKind::Number,
                    read_only: false,
                    global_writable: false,
                    repo_writable: true,
                    value_hint: None,
                },
                ConfigFieldRow {
                    field: "dynamicWorkflows.agentsToModels.claude".into(),
                    global_value: None,
                    repo_value: Some(long_models.into()),
                    effective_value: Some(long_models.into()),
                    kind: ConfigFieldKind::String,
                    read_only: false,
                    global_writable: false,
                    repo_writable: true,
                    value_hint: None,
                },
            ],
        };
        let s = render_config_show(&o);

        assert!(
            s.contains("dynamicWorkflows.defaultLeader"),
            "defaultLeader field must appear: {s}"
        );
        assert!(
            s.contains("dynamicWorkflows.maxConcurrentSteps"),
            "maxConcurrentSteps field must appear: {s}"
        );
        assert!(
            s.contains("dynamicWorkflows.agentsToModels.claude"),
            "per-agent agentsToModels row must appear, got: {s}"
        );
        assert!(
            !s.contains("dynamicWorkflows.agentsToModels.claude (read-only)"),
            "per-agent agentsToModels rows are inline-editable and must not be marked \
             read-only, got: {s}"
        );
        assert!(
            s.contains(long_models),
            "long agentsToModels value must appear untruncated in CLI text output, got: {s}"
        );
        assert!(
            !s.contains('\u{2026}'),
            "CLI config show output must never truncate values with an ellipsis, got: {s}"
        );
    }

    #[test]
    fn render_config_set_formats_field_scope_and_value() {
        let o = ConfigSetOutcome {
            field: "agent".into(),
            value: "gemini".into(),
            scope: "repo".into(),
        };
        let s = render_config_set(&o);
        assert!(s.contains("agent"), "field name must appear");
        assert!(s.contains("repo"), "scope must appear");
        assert!(s.contains("gemini"), "value must appear");
    }

    // ── render_new ────────────────────────────────────────────────────────────

    use crate::command::commands::new::{
        NewSkillOutcome, NewSpecOutcome, NewWorkflowOutcome, PullLibraryOutcome,
    };

    #[test]
    fn render_new_spec_with_path_shows_created_path() {
        let o = NewSpecOutcome {
            interview: false,
            path: Some("/aspec/work-items/0001-foo.md".into()),
        };
        let s = render_new_spec(&o);
        assert!(s.contains("0001-foo.md"), "path must appear in output: {s}");
        assert!(s.contains("Created"), "must say Created: {s}");
    }

    #[test]
    fn render_new_spec_without_path_shows_fallback() {
        let o = NewSpecOutcome {
            interview: false,
            path: None,
        };
        let s = render_new_spec(&o);
        assert!(!s.is_empty());
    }

    #[test]
    fn render_new_workflow_repo_scope_shows_format() {
        let o = NewWorkflowOutcome {
            interview: false,
            global: false,
            format: "toml".into(),
            path: Some("/aspec/workflows/my-wf.toml".into()),
        };
        let s = render_new_workflow(&o);
        assert!(s.contains("repo"), "must mention repo scope");
        assert!(s.contains("toml"), "must mention format");
        assert!(s.contains("my-wf.toml"), "path must appear");
    }

    #[test]
    fn render_new_workflow_global_scope() {
        let o = NewWorkflowOutcome {
            interview: false,
            global: true,
            format: "yaml".into(),
            path: Some("/home/user/.awman/workflows/my-wf.yaml".into()),
        };
        let s = render_new_workflow(&o);
        assert!(s.contains("global"), "must mention global scope");
    }

    #[test]
    fn render_new_skill_global_shows_global_scope() {
        let o = NewSkillOutcome {
            interview: false,
            global: true,
            path: Some("/home/user/.awman/skills/my-skill/SKILL.md".into()),
            pull: false,
            libraries: vec![],
        };
        let s = render_new_skill(&o).expect("skill creation must render a summary line");
        assert!(s.contains("global"), "must mention global scope");
        assert!(s.contains("SKILL.md"), "path must appear");
    }

    /// A `--pull` run clones a managed library into the *global* store and
    /// already reported it through the message sink. It must never render the
    /// "Created skill (repo)" creation line (WI-0103 remediation).
    #[test]
    fn render_new_skill_pull_renders_no_creation_line() {
        let o = NewSkillOutcome {
            interview: false,
            global: false,
            path: Some("/home/user/.awman/skills/.library/superpowers".into()),
            pull: true,
            libraries: vec![PullLibraryOutcome {
                slug: "superpowers".into(),
                dir: "/home/user/.awman/skills/.library/superpowers".into(),
                updated: false,
                skills_found: vec!["brainstorming".into()],
                error: None,
            }],
        };
        assert_eq!(
            render_new_skill(&o),
            None,
            "a pull must not render a skill-creation line"
        );
        assert_eq!(
            render_new(&NewOutcome::Skill(o)),
            None,
            "the NewOutcome renderer must pass the pull's silence through"
        );
    }

    /// `--pull-all` with nothing pulled yet has no libraries *and* no path;
    /// it must still not be rendered as "Skill created (repo)."
    #[test]
    fn render_new_skill_empty_pull_all_renders_no_creation_line() {
        let o = NewSkillOutcome {
            interview: false,
            global: false,
            path: None,
            pull: true,
            libraries: vec![],
        };
        assert_eq!(
            render_new_skill(&o),
            None,
            "an empty --pull-all must not claim a skill was created"
        );
    }

    // ── render_specs ──────────────────────────────────────────────────────────

    use crate::command::commands::specs::SpecsAmendOutcome;

    #[test]
    fn render_specs_amend_shows_work_item_number() {
        let o = SpecsAmendOutcome {
            work_item: "0042".into(),
            non_interactive: false,
            allow_docker: false,
        };
        let s = render_specs_amend(&o);
        assert!(s.contains("0042"), "work item number must appear: {s}");
    }

    // ── render_exec_workflow ──────────────────────────────────────────────────

    use crate::command::commands::exec_workflow::ExecWorkflowOutcome;

    #[test]
    fn render_exec_workflow_shows_workflow_name() {
        let o = ExecWorkflowOutcome {
            workflow: "deploy.toml".into(),
            exit_code: Some(0),
            worktree_used: false,
        };
        let s = render_exec_workflow(&o).expect("exec_workflow must produce output");
        assert!(s.contains("deploy.toml"), "workflow name must appear: {s}");
        assert!(s.contains("completed"), "must say completed: {s}");
    }

    #[test]
    fn render_exec_workflow_nonzero_exit_shows_exit_code() {
        let o = ExecWorkflowOutcome {
            workflow: "build.yaml".into(),
            exit_code: Some(2),
            worktree_used: false,
        };
        let s = render_exec_workflow(&o).expect("exec_workflow must produce output");
        assert!(
            s.contains("2") || s.contains("exit"),
            "exit code must appear: {s}"
        );
    }

    // ── render_exec_prompt ────────────────────────────────────────────────────

    use crate::command::commands::exec_prompt::ExecPromptOutcome;

    #[test]
    fn render_exec_prompt_zero_exit_returns_none() {
        let o = ExecPromptOutcome {
            agent: Some("claude".into()),
            exit_code: Some(0),
        };
        assert!(render_exec_prompt(&o).is_none());
    }

    #[test]
    fn render_exec_prompt_nonzero_exit_returns_message() {
        let o = ExecPromptOutcome {
            agent: None,
            exit_code: Some(3),
        };
        let s = render_exec_prompt(&o).expect("nonzero exit must produce output");
        assert!(
            s.contains("3") || s.contains("exit"),
            "exit code must appear: {s}"
        );
    }

    // ── render_download ───────────────────────────────────────────────────────

    use crate::command::commands::download::DownloadOutcome;

    #[test]
    fn render_download_shows_asset_and_bytes() {
        let o = DownloadOutcome {
            asset: "aspec".into(),
            bytes_written: 12345,
            dest_path: Some("/some/path/aspec".into()),
        };
        let s = render_download(&o).expect("download must produce output");
        assert!(s.contains("aspec"), "asset name must appear: {s}");
        assert!(s.contains("12345"), "bytes_written must appear: {s}");
    }

    #[test]
    fn render_download_without_dest_path() {
        let o = DownloadOutcome {
            asset: "dockerfile-claude".into(),
            bytes_written: 42,
            dest_path: None,
        };
        let s = render_download(&o).expect("download must produce output even without dest_path");
        assert!(
            s.contains("dockerfile-claude"),
            "asset name must appear: {s}"
        );
    }

    // ── render_api_server ───────────────────────────────────────────────────────

    use crate::command::commands::api_server::{
        ApiServerKillOutcome, ApiServerLogsOutcome, ApiServerStartOutcome,
    };

    #[test]
    fn render_api_server_start_shows_port_and_mode() {
        let o = ApiServerStartOutcome {
            port: 9876,
            background: true,
            workdirs: vec!["/repo".into()],
            refreshed_key: false,
        };
        let s = render_api_server_start(&o);
        assert!(s.contains("9876"), "port must appear: {s}");
        assert!(s.contains("background"), "mode must appear: {s}");
    }

    #[test]
    fn render_api_server_start_foreground_mode() {
        let o = ApiServerStartOutcome {
            port: 8080,
            background: false,
            workdirs: vec![],
            refreshed_key: true,
        };
        let s = render_api_server_start(&o);
        assert!(s.contains("foreground"), "must say foreground: {s}");
        assert!(
            s.contains("api key refreshed"),
            "refreshed_key must be mentioned: {s}"
        );
    }

    #[test]
    fn render_api_server_kill_with_stopped_pid() {
        let s = render_api_server_kill(&ApiServerKillOutcome {
            stopped_pid: Some(5678),
        });
        assert!(s.contains("5678"), "PID must appear: {s}");
        assert!(s.contains("stopped"), "must say stopped: {s}");
    }

    #[test]
    fn render_api_server_kill_without_pid_says_not_running() {
        let s = render_api_server_kill(&ApiServerKillOutcome { stopped_pid: None });
        assert!(s.contains("not running"), "must say not running: {s}");
    }

    #[test]
    fn render_api_server_logs_with_path() {
        let s = render_api_server_logs(&ApiServerLogsOutcome {
            log_path: "/tmp/awman.log".into(),
        });
        assert!(s.contains("/tmp/awman.log"), "log path must appear: {s}");
    }

    #[test]
    fn render_api_server_logs_empty_path() {
        let s = render_api_server_logs(&ApiServerLogsOutcome {
            log_path: String::new(),
        });
        assert!(s.contains("No API server log"), "must say no log: {s}");
    }

    // ── render_remote ─────────────────────────────────────────────────────────

    use crate::command::commands::remote::{RemoteSessionKillOutcome, RemoteSessionStartOutcome};

    #[test]
    fn render_remote_session_start_shows_session_id() {
        let s = render_remote_session_start(&RemoteSessionStartOutcome {
            session_id: "sess-1".into(),
            remote_addr: "localhost:9876".into(),
            setup_status: Some("ready".into()),
        });
        assert!(s.contains("sess-1"), "session_id must appear: {s}");
    }

    #[test]
    fn render_remote_session_start_shows_remote_addr() {
        let s = render_remote_session_start(&RemoteSessionStartOutcome {
            session_id: "sess-2".into(),
            remote_addr: "localhost:9876".into(),
            setup_status: None,
        });
        assert!(s.contains("localhost:9876"), "remote_addr must appear: {s}");
    }

    #[test]
    fn render_remote_session_kill_with_session_id() {
        let s = render_remote_session_kill(&RemoteSessionKillOutcome {
            session_id: "abc123".into(),
            remote_addr: "localhost:9876".into(),
        });
        assert!(s.contains("abc123"), "session id must appear: {s}");
    }

    #[test]
    fn render_remote_session_kill_shows_remote_addr() {
        let s = render_remote_session_kill(&RemoteSessionKillOutcome {
            session_id: "sess-1".into(),
            remote_addr: "localhost:9876".into(),
        });
        assert!(s.contains("localhost:9876"), "remote_addr must appear: {s}");
    }

    // ── render_status (tip flows from outcome) ───────────────────────────────

    #[test]
    fn render_status_displays_outcome_tip_verbatim() {
        let o = StatusOutcome {
            containers: vec![],
            watched: false,
            tip: "this is the unique tip text".into(),
        };
        let s = render_status(&o);
        assert!(
            s.contains("Tip: this is the unique tip text"),
            "renderer must print the outcome tip verbatim: {s}"
        );
    }
}
