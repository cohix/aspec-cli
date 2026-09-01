//! `ReadyFrontend` impl for the TUI.

use crate::data::message::UserMessageSink;
use crate::engine::agent_runtime::frontend::AgentFrontend;
use crate::engine::error::EngineError;
use crate::engine::ready::frontend::ReadyFrontend;
use crate::engine::ready::phase::ReadyPhase;
use crate::engine::ready::summary::ReadySummary;
use crate::engine::step_status::StepStatus;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};

impl ReadyFrontend for TuiCommandFrontend {
    fn ask_create_dockerfile(
        &mut self,
        dockerfile_path: &std::path::Path,
    ) -> Result<bool, EngineError> {
        let response = self
            .ask_dialog(DialogRequest::YesNo {
                title: "Create Dockerfile?".into(),
                body: format!(
                    "No Dockerfile found at {}. Create one from the default template?",
                    dockerfile_path.display()
                ),
            })
            .map_err(|e| EngineError::Other(e.to_string()))?;
        Ok(matches!(
            response,
            DialogResponse::Yes | DialogResponse::Char('y')
        ))
    }

    fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError> {
        let response = self
            .ask_dialog(DialogRequest::YesNo {
                title: "Run audit?".into(),
                body: "Dockerfile.dev matches the default template. Run the audit to install project dependencies?".into(),
            })
            .map_err(|e| EngineError::Other(e.to_string()))?;
        Ok(matches!(
            response,
            DialogResponse::Yes | DialogResponse::Char('y')
        ))
    }

    fn report_phase(&mut self, phase: &ReadyPhase) {
        self.messages.info(format!("ready: {phase:?}"));
    }

    fn report_step_status(&mut self, step: &str, status: StepStatus) {
        self.messages.info(format!("  {step}: {status:?}"));
    }

    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        Box::new(super::TuiContainerProxy::new(self.status_log.clone()))
    }

    fn report_summary(&mut self, summary: &ReadySummary) {
        use crate::frontend::cli::per_command::helpers::render_summary_box;
        let mut rows: Vec<(&str, &crate::engine::step_status::StepStatus)> = vec![
            ("Dockerfile", &summary.dockerfile),
            ("Base image", &summary.base_image),
            ("Agent image", &summary.agent_image),
            ("Local agent", &summary.local_agent),
            ("Audit", &summary.audit),
            ("aspec folder", &summary.aspec_folder),
            ("Config", &summary.work_items_config),
        ];

        // Ready engine reports a single consolidated row — either
        // "Other agents" (all OK) or "Missing images" (warn). Render the
        // engine-provided label verbatim.
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
        for line in box_str.lines() {
            let s: String = line.to_string();
            self.messages.info(s);
        }

        let has_missing = summary
            .non_default_agent_images
            .iter()
            .any(|(_, s)| matches!(s, crate::engine::step_status::StepStatus::Warn(_)));
        if has_missing {
            self.messages.info(
                "Tip: run \"ready --build\" to build all available agent images.".to_string(),
            );
        }

        self.messages.success("awman is ready.".to_string());
    }
}

#[cfg(test)]
mod tests {
    use crate::engine::ready::frontend::ReadyFrontend;
    use crate::frontend::tui::command_frontend::TuiCommandFrontend;
    use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};

    fn make_frontend() -> (
        TuiCommandFrontend,
        std::sync::mpsc::Receiver<DialogRequest>,
        std::sync::mpsc::Sender<DialogResponse>,
    ) {
        let (req_tx, req_rx) = std::sync::mpsc::channel::<DialogRequest>();
        let (resp_tx, resp_rx) = std::sync::mpsc::channel::<DialogResponse>();
        let (stdout_tx, _stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (_resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();
        let (stderr_tx, _stderr_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let container_io = crate::engine::agent_runtime::frontend::AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: Some(resize_rx),
            initial_size: Some((80, 24)),
        };
        let status_log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let parsed = crate::command::dispatch::parsed_input::ParsedCommandBoxInput {
            path: vec!["ready".into()],
            flags: Default::default(),
            arguments: Default::default(),
        };
        let workflow_view = std::sync::Arc::new(std::sync::Mutex::new(None));
        let yolo_state = std::sync::Arc::new(std::sync::Mutex::new(None));
        let yolo_cancel_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let pty_reset_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let frontend = TuiCommandFrontend::new(
            parsed,
            status_log,
            req_tx,
            resp_rx,
            container_io,
            workflow_view,
            yolo_state,
            yolo_cancel_flag,
            pty_reset_flag,
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::Mutex::new(
                crate::command::commands::status::StatusCommandTuiContext::default(),
            )),
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        );
        (frontend, req_rx, resp_tx)
    }

    #[test]
    fn ask_create_dockerfile_yes_returns_true() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap();
            resp_tx.send(DialogResponse::Yes).unwrap();
        });
        let result = frontend
            .ask_create_dockerfile(std::path::Path::new("/tmp/Dockerfile.dev"))
            .unwrap();
        handle.join().unwrap();
        assert!(result);
    }

    #[test]
    fn ask_create_dockerfile_no_returns_false() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap();
            resp_tx.send(DialogResponse::No).unwrap();
        });
        let result = frontend
            .ask_create_dockerfile(std::path::Path::new("/tmp/Dockerfile.dev"))
            .unwrap();
        handle.join().unwrap();
        assert!(!result);
    }

    #[test]
    fn ask_create_dockerfile_dismissed_returns_false() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap();
            resp_tx.send(DialogResponse::Dismissed).unwrap();
        });
        let result = frontend
            .ask_create_dockerfile(std::path::Path::new("/tmp/Dockerfile.dev"))
            .unwrap();
        handle.join().unwrap();
        assert!(!result);
    }

    #[test]
    fn ask_run_audit_on_template_yes_returns_true() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap();
            resp_tx.send(DialogResponse::Yes).unwrap();
        });
        let result = frontend.ask_run_audit_on_template().unwrap();
        handle.join().unwrap();
        assert!(result);
    }

    #[test]
    fn ask_run_audit_on_template_no_returns_false() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap();
            resp_tx.send(DialogResponse::No).unwrap();
        });
        let result = frontend.ask_run_audit_on_template().unwrap();
        handle.join().unwrap();
        assert!(!result);
    }
}
