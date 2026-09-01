//! `ReadyFrontend` impl for the CLI.
//!
//! Prompts on stdin for the Dockerfile creation decision when stdin is a TTY;
//! otherwise returns the safe non-interactive defaults.

use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::engine::agent_runtime::frontend::AgentFrontend;
use crate::engine::error::EngineError;
use crate::engine::ready::{ReadyFrontend, ReadyPhase, ReadySummary};
use crate::engine::step_status::StepStatus;

use crate::frontend::cli::command_frontend::CliFrontend;

use super::helpers::{render_summary_box, step_status_label, yes_no};

impl ReadyFrontend for CliFrontend {
    fn ask_create_dockerfile(
        &mut self,
        dockerfile_path: &std::path::Path,
    ) -> Result<bool, EngineError> {
        Ok(yes_no(
            &format!(
                "No Dockerfile found at {}. Create one from the default template?",
                dockerfile_path.display()
            ),
            true,
        ))
    }

    fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError> {
        Ok(yes_no(
            "Run the agent audit container to scan and customise the Dockerfile?",
            false,
        ))
    }

    fn report_phase(&mut self, _phase: &ReadyPhase) {
        // The ReadyPhase enum is an internal state-machine token; users see
        // progress through `report_step_status` and the final summary box.
    }

    fn report_step_status(&mut self, step: &str, status: StepStatus) {
        // When --json is active, suppress human-readable output on stderr.
        if self.is_json_mode() {
            return;
        }
        let level = match status {
            StepStatus::Failed(_) => MessageLevel::Error,
            _ => MessageLevel::Info,
        };
        self.messages.write_message(UserMessage {
            level,
            text: format!("{step}: {}", step_status_label(&status)),
        });
    }

    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        Box::new(super::container_frontend_marker::CliContainerProxy)
    }

    fn report_summary(&mut self, summary: &ReadySummary) {
        // When --json is active, suppress the human-readable summary box on
        // stderr — only the JSON output on stdout matters.
        if self.is_json_mode() {
            return;
        }
        let mut rows: Vec<(&str, &StepStatus)> = vec![
            ("Dockerfile", &summary.dockerfile),
            ("Base image", &summary.base_image),
            ("Agent image", &summary.agent_image),
            ("Local agent", &summary.local_agent),
            ("Audit", &summary.audit),
        ];

        // The ready engine reports a single consolidated row here — either
        // "Other agents" (all OK) or "Missing images" (warn). No "Agent: "
        // prefix; the engine's name is rendered verbatim.
        for (label, status) in summary.non_default_agent_images.iter() {
            rows.push((label.as_str(), status));
        }

        let credential_rows: Vec<(String, StepStatus)> = summary
            .agent_credentials
            .iter()
            .map(|health| {
                let status = if let Some(error) = &health.read_error {
                    StepStatus::Warn(format!("credential unreadable: {error}"))
                } else if health.expired {
                    StepStatus::Warn("credential expired".to_string())
                } else if health.expires_in_secs.is_some() {
                    StepStatus::Done
                } else {
                    StepStatus::Warn("credential expiry unknown".to_string())
                };
                let label = match health.expires_in_secs {
                    Some(secs) if !health.expired && health.read_error.is_none() => {
                        format!("Credential {} ({secs}s remaining)", health.agent)
                    }
                    _ => format!("Credential {}", health.agent),
                };
                (label, status)
            })
            .collect();
        for (label, status) in &credential_rows {
            rows.push((label.as_str(), status));
        }

        let box_str =
            render_summary_box(&format!("Ready Summary ({})", summary.runtime_name), &rows);
        // Write the summary box directly to stderr without the per-line
        // "awman:" prefix used for status updates — the box is multi-line
        // content that reads better unprefixed.
        let _ = std::io::Write::write_all(
            &mut std::io::stderr(),
            format!("\n{box_str}awman is ready.\n").as_bytes(),
        );

        let has_missing = summary
            .non_default_agent_images
            .iter()
            .any(|(_, s)| matches!(s, StepStatus::Warn(_)));
        if has_missing {
            let _ = std::io::Write::write_all(
                &mut std::io::stderr(),
                b"Tip: run \"ready --build\" to build all available agent images.\n",
            );
        }
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }
}
