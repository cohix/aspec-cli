//! `SquadCommandFrontend` impl for the TUI — the task-creation interview
//! (BLOCKER-3, §9.3) and the persistent-directory delete confirmation
//! (BLOCKER-2, §9.2), both driven through `ask_dialog`, exactly as
//! `NewCommandFrontend` collects a workflow.
//!
//! These COLLECT input only. No validation or scheduling decision is made
//! here; the answers flow to Layer 2, which builds the `CreateTask` and
//! reaches `LocalTaskGateway::validate_create` for every rejection.

use std::path::{Path, PathBuf};

use crate::command::commands::squad::commands::{SquadCommandFrontend, TaskWorkspaceChoice};
use crate::command::error::CommandError;
use crate::data::fs::task_store::MountScope;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};

/// The task-description modal's title, spelled out once so the CLI and TUI
/// interviews ask the same question.
pub const TASK_DESCRIPTION_TITLE: &str = "Describe the new squad task including its triggering \
     conditions and how squad should handle the task each time it is triggered";

impl SquadCommandFrontend for TuiCommandFrontend {
    /// The TUI runs in the user's own terminal, so the process's current
    /// directory is theirs and the mount-scope question can be put to them.
    fn is_local_user_session(&self) -> bool {
        true
    }

    fn ask_task_name(&mut self) -> Result<String, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Task name".into(),
            prompt: "Enter the task slug:".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) if !t.trim().is_empty() => Ok(t.trim().to_string()),
            _ => Err(CommandError::Aborted),
        }
    }

    /// One freeform description covering both halves of a task — when it fires
    /// and what to do about it — through the same multiline editor
    /// `new spec --interview` uses. A one-line box could not hold either half.
    fn ask_task_description(&mut self) -> Result<String, CommandError> {
        let response = self.ask_dialog(DialogRequest::MultilineInput {
            title: TASK_DESCRIPTION_TITLE.into(),
            // The frame title clips on a narrow terminal, so the same
            // instruction is repeated in the body, pre-wrapped (the multiline
            // dialog renders its prompt without wrapping).
            prompt: "Describe the new squad task including its triggering conditions\n\
                     and how squad should handle the task each time it is triggered.\n\
                     (Ctrl+Enter to submit)"
                .into(),
        })?;
        match response {
            DialogResponse::Text(t) => Ok(t),
            _ => Err(CommandError::Aborted),
        }
    }

    fn ask_task_workspace_choice(&mut self) -> Result<TaskWorkspaceChoice, CommandError> {
        let response = self.ask_dialog(DialogRequest::KindSelect {
            title: "Task Workspace".into(),
            options: vec![
                ("1".into(), "Default Task Workspace".into()),
                ("2".into(), "Custom Folder / Repo".into()),
            ],
        })?;
        match response {
            DialogResponse::Char('2') | DialogResponse::Index(1) => {
                Ok(TaskWorkspaceChoice::CustomFolderOrRepo)
            }
            DialogResponse::Char('1') | DialogResponse::Index(0) => {
                Ok(TaskWorkspaceChoice::DefaultTaskWorkspace)
            }
            _ => Err(CommandError::Aborted),
        }
    }

    fn confirm_non_git_workspace(&mut self, path: &Path) -> Result<bool, CommandError> {
        let response = self.ask_dialog(DialogRequest::YesNo {
            title: "Not a Git repository".into(),
            body: format!(
                "{} is not the root of a Git repository.\n\n\
                 Keep this path? (No = choose a different one)",
                path.display()
            ),
        })?;
        match response {
            DialogResponse::Yes => Ok(true),
            DialogResponse::No => Ok(false),
            // Dismissing is not "No": "No" asks for a different path, while
            // Esc abandons the interview outright (WI 0106's interrupted-
            // interview rule). Nothing may be persisted after it.
            _ => Err(CommandError::Aborted),
        }
    }

    fn confirm_parent_directory_workspace(
        &mut self,
        path: &Path,
        current_dir: &Path,
    ) -> Result<bool, CommandError> {
        let response = self.ask_dialog(DialogRequest::YesNo {
            title: "Mount a parent directory?".into(),
            body: format!(
                "{} is a parent of {}.\n\n\
                 Mount it anyway? (No = choose a different one)",
                path.display(),
                current_dir.display()
            ),
        })?;
        match response {
            DialogResponse::Yes => Ok(true),
            DialogResponse::No => Ok(false),
            _ => Err(CommandError::Aborted),
        }
    }

    fn ask_task_overlay(&mut self, existing: &[String]) -> Result<Option<String>, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: format!("Overlays ({} added)", existing.len()),
            prompt: "Add an overlay? [dir()/ssh()/env()/skill() syntax, blank to finish]:".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) if !t.trim().is_empty() => Ok(Some(t.trim().to_string())),
            // A *blank submission* means "no more overlays" and ends the loop.
            // A dismissal does not: it abandons the interview, and nothing may
            // be persisted after it.
            DialogResponse::Text(_) => Ok(None),
            _ => Err(CommandError::Aborted),
        }
    }

    fn ask_task_interval(&mut self) -> Result<String, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Evaluation interval".into(),
            prompt: "How often to evaluate (e.g. 6h, 1d):".into(),
            default_text: Some("6h".into()),
        })?;
        match response {
            DialogResponse::Text(t) if !t.trim().is_empty() => Ok(t.trim().to_string()),
            // Submitting an empty box takes the documented default; dismissing
            // the dialog abandons the interview.
            DialogResponse::Text(_) => Ok("6h".to_string()),
            _ => Err(CommandError::Aborted),
        }
    }

    fn ask_task_repo(&mut self) -> Result<PathBuf, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Custom Folder / Repo".into(),
            prompt: "Folder or repository to bind this task to (Enter for current dir):".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) if !t.trim().is_empty() => Ok(PathBuf::from(t.trim())),
            DialogResponse::Text(_) => std::env::current_dir().map_err(|error| {
                CommandError::Other(format!("cannot resolve current dir: {error}"))
            }),
            _ => Err(CommandError::Aborted),
        }
    }

    fn ask_task_agent(&mut self) -> Result<Option<String>, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Leader agent".into(),
            prompt: "Leader agent (optional, Enter to skip):".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) if !t.trim().is_empty() => Ok(Some(t.trim().to_string())),
            DialogResponse::Text(_) => Ok(None),
            _ => Err(CommandError::Aborted),
        }
    }

    fn ask_task_model(&mut self) -> Result<Option<String>, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Leader model".into(),
            prompt: "Leader model (optional, Enter to skip):".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) if !t.trim().is_empty() => Ok(Some(t.trim().to_string())),
            DialogResponse::Text(_) => Ok(None),
            _ => Err(CommandError::Aborted),
        }
    }

    fn ask_task_mount_scope(&mut self) -> Result<MountScope, CommandError> {
        // "Yes" mounts the whole git root; "No" mounts the current directory
        // only. The default (git root) is the safer, more useful scope.
        let response = self.ask_dialog(DialogRequest::YesNo {
            title: "Mount scope".into(),
            body: "Mount the entire git root? (No = current directory only)".into(),
        })?;
        match response {
            DialogResponse::No => Ok(MountScope::Cwd),
            DialogResponse::Yes => Ok(MountScope::GitRoot),
            _ => Err(CommandError::Aborted),
        }
    }

    fn ask_delete_task_dir(&mut self, name: &str, path: &Path) -> Result<bool, CommandError> {
        let response = self.ask_dialog(DialogRequest::YesNo {
            title: format!("Delete {name} directory?"),
            body: format!(
                "Also delete the persistent task directory {}?",
                path.display()
            ),
        })?;
        Ok(matches!(response, DialogResponse::Yes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend::tui::per_command::mount_scope::tests::make_frontend;

    /// Answer the next dialog with `response`, on a helper thread, so the
    /// blocking `ask_dialog` call under test can complete.
    fn answer_with(
        req_rx: std::sync::mpsc::Receiver<DialogRequest>,
        resp_tx: std::sync::mpsc::Sender<DialogResponse>,
        response: DialogResponse,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap();
            resp_tx.send(response).unwrap();
        })
    }

    /// Dismissing any interview dialog abandons task creation. Nothing may be
    /// persisted after an interrupted interview (WI 0106's edge case), so no
    /// step is allowed to quietly substitute a default and let the remaining
    /// prompts carry on to `gateway.create`.
    #[test]
    fn dismissing_any_interview_step_aborts_instead_of_taking_a_default() {
        macro_rules! assert_dismissal_aborts {
            ($label:expr, $call:expr) => {{
                let (mut frontend, req_rx, resp_tx) = make_frontend();
                let handle = answer_with(req_rx, resp_tx, DialogResponse::Dismissed);
                #[allow(clippy::redundant_closure_call)]
                let result = ($call)(&mut frontend);
                handle.join().unwrap();
                assert!(
                    matches!(result, Err(CommandError::Aborted)),
                    "{} must abort when its dialog is dismissed",
                    $label
                );
            }};
        }

        assert_dismissal_aborts!("the name step", |f: &mut TuiCommandFrontend| f
            .ask_task_name()
            .map(|_| ()));
        assert_dismissal_aborts!("the description step", |f: &mut TuiCommandFrontend| f
            .ask_task_description()
            .map(|_| ()));
        assert_dismissal_aborts!("the interval step", |f: &mut TuiCommandFrontend| f
            .ask_task_interval()
            .map(|_| ()));
        assert_dismissal_aborts!("the workspace-choice step", |f: &mut TuiCommandFrontend| f
            .ask_task_workspace_choice()
            .map(|_| ()));
        assert_dismissal_aborts!("the custom-path step", |f: &mut TuiCommandFrontend| f
            .ask_task_repo()
            .map(|_| ()));
        assert_dismissal_aborts!(
            "the not-a-repository warning",
            |f: &mut TuiCommandFrontend| f
                .confirm_non_git_workspace(std::path::Path::new("/tmp"))
                .map(|_| ())
        );
        assert_dismissal_aborts!(
            "the parent-directory warning",
            |f: &mut TuiCommandFrontend| f
                .confirm_parent_directory_workspace(
                    std::path::Path::new("/tmp"),
                    std::path::Path::new("/tmp/sub")
                )
                .map(|_| ())
        );
        assert_dismissal_aborts!("the overlay step", |f: &mut TuiCommandFrontend| f
            .ask_task_overlay(&[])
            .map(|_| ()));
        assert_dismissal_aborts!("the agent step", |f: &mut TuiCommandFrontend| f
            .ask_task_agent()
            .map(|_| ()));
        assert_dismissal_aborts!("the model step", |f: &mut TuiCommandFrontend| f
            .ask_task_model()
            .map(|_| ()));
        assert_dismissal_aborts!("the mount-scope step", |f: &mut TuiCommandFrontend| f
            .ask_task_mount_scope()
            .map(|_| ()));
    }

    /// A *blank submission* is still a real answer: it keeps the documented
    /// default for optional steps and ends the overlay loop. Only dismissal
    /// aborts.
    #[test]
    fn a_blank_submission_still_means_the_documented_default() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let handle = answer_with(req_rx, resp_tx, DialogResponse::Text(String::new()));
        assert_eq!(frontend.ask_task_interval().unwrap(), "6h");
        handle.join().unwrap();

        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let handle = answer_with(req_rx, resp_tx, DialogResponse::Text("  ".into()));
        assert_eq!(frontend.ask_task_overlay(&[]).unwrap(), None);
        handle.join().unwrap();

        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let handle = answer_with(req_rx, resp_tx, DialogResponse::Text(String::new()));
        assert_eq!(frontend.ask_task_agent().unwrap(), None);
        handle.join().unwrap();
    }
}
