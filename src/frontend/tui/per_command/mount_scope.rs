//! `MountScopeFrontend` impl for the TUI.

use std::path::Path;

use crate::command::commands::mount_scope::{MountScopeDecision, MountScopeFrontend};
use crate::command::error::CommandError;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse, MountScopeState};

impl MountScopeFrontend for TuiCommandFrontend {
    fn ask_mount_scope(
        &mut self,
        git_root: &Path,
        cwd: &Path,
    ) -> Result<MountScopeDecision, CommandError> {
        let response = self.ask_dialog(DialogRequest::MountScope(MountScopeState {
            git_root: git_root.display().to_string(),
            cwd: cwd.display().to_string(),
        }))?;
        Ok(match response {
            DialogResponse::Char('r') => MountScopeDecision::MountGitRoot,
            DialogResponse::Char('c') => MountScopeDecision::MountCurrentDirOnly,
            _ => MountScopeDecision::Abort,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::frontend::tui::dialogs::DialogResponse;

    pub(crate) fn make_frontend() -> (
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
            path: vec!["status".into()],
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
    fn ask_mount_scope_char_r_returns_mount_git_root() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let git_root = std::path::Path::new("/repo");
        let cwd = std::path::Path::new("/repo/sub");
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap();
            resp_tx.send(DialogResponse::Char('r')).unwrap();
        });
        let result = frontend.ask_mount_scope(git_root, cwd).unwrap();
        handle.join().unwrap();
        assert_eq!(result, MountScopeDecision::MountGitRoot);
    }

    #[test]
    fn ask_mount_scope_char_c_returns_mount_current_dir() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let git_root = std::path::Path::new("/repo");
        let cwd = std::path::Path::new("/repo/sub");
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap();
            resp_tx.send(DialogResponse::Char('c')).unwrap();
        });
        let result = frontend.ask_mount_scope(git_root, cwd).unwrap();
        handle.join().unwrap();
        assert_eq!(result, MountScopeDecision::MountCurrentDirOnly);
    }

    #[test]
    fn ask_mount_scope_dismissed_returns_abort() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let git_root = std::path::Path::new("/repo");
        let cwd = std::path::Path::new("/repo/sub");
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap();
            resp_tx.send(DialogResponse::Dismissed).unwrap();
        });
        let result = frontend.ask_mount_scope(git_root, cwd).unwrap();
        handle.join().unwrap();
        assert_eq!(result, MountScopeDecision::Abort);
    }
}
