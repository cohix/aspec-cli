//! `AmieCommandFrontend` impl for the TUI — the condition-creation interview
//! (BLOCKER-3, §9.3) and the persistent-directory delete confirmation
//! (BLOCKER-2, §9.2), both driven through `ask_dialog`, exactly as
//! `NewCommandFrontend` collects a workflow.
//!
//! These COLLECT input only. No validation or scheduling decision is made
//! here; the answers flow to Layer 2, which builds the `CreateCondition` and
//! reaches `LocalConditionGateway::validate_create` for every rejection.

use std::path::{Path, PathBuf};

use crate::command::commands::amie::commands::AmieCommandFrontend;
use crate::command::error::CommandError;
use crate::data::fs::condition_store::MountScope;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};

impl AmieCommandFrontend for TuiCommandFrontend {
    fn ask_condition_name(&mut self) -> Result<String, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Condition name".into(),
            prompt: "Enter the condition slug:".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) if !t.trim().is_empty() => Ok(t.trim().to_string()),
            _ => Err(CommandError::Aborted),
        }
    }

    fn ask_condition_description(&mut self) -> Result<String, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Condition description".into(),
            prompt: "Describe when this condition should trigger:".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) => Ok(t),
            _ => Err(CommandError::Aborted),
        }
    }

    fn ask_condition_interval(&mut self) -> Result<String, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Evaluation interval".into(),
            prompt: "How often to evaluate (e.g. 5m, 1h):".into(),
            default_text: Some("5m".into()),
        })?;
        match response {
            DialogResponse::Text(t) if !t.trim().is_empty() => Ok(t.trim().to_string()),
            _ => Ok("5m".to_string()),
        }
    }

    fn ask_condition_repo(&mut self) -> Result<PathBuf, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Repository directory".into(),
            prompt: "Repository directory to evaluate (Enter for current dir):".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) if !t.trim().is_empty() => Ok(PathBuf::from(t.trim())),
            _ => std::env::current_dir().map_err(|error| {
                CommandError::Other(format!("cannot resolve current dir: {error}"))
            }),
        }
    }

    fn ask_condition_agent(&mut self) -> Result<Option<String>, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Leader agent".into(),
            prompt: "Leader agent (optional, Enter to skip):".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) if !t.trim().is_empty() => Ok(Some(t.trim().to_string())),
            _ => Ok(None),
        }
    }

    fn ask_condition_model(&mut self) -> Result<Option<String>, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Leader model".into(),
            prompt: "Leader model (optional, Enter to skip):".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) if !t.trim().is_empty() => Ok(Some(t.trim().to_string())),
            _ => Ok(None),
        }
    }

    fn ask_condition_mount_scope(&mut self) -> Result<MountScope, CommandError> {
        // "Yes" mounts the whole git root; "No" mounts the current directory
        // only. The default (git root) is the safer, more useful scope.
        let response = self.ask_dialog(DialogRequest::YesNo {
            title: "Mount scope".into(),
            body: "Mount the entire git root? (No = current directory only)".into(),
        })?;
        match response {
            DialogResponse::No => Ok(MountScope::Cwd),
            _ => Ok(MountScope::GitRoot),
        }
    }

    fn ask_delete_condition_dir(
        &mut self,
        name: &str,
        path: &Path,
    ) -> Result<bool, CommandError> {
        let response = self.ask_dialog(DialogRequest::YesNo {
            title: format!("Delete {name} directory?"),
            body: format!("Also delete the persistent condition directory {}?", path.display()),
        })?;
        Ok(matches!(response, DialogResponse::Yes))
    }
}
