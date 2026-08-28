//! Dialog request/response routing: dismissal, submit (Enter), and text/
//! cursor editing for the currently active dialog, including the
//! ConfigShow dialog's inline-edit and add-mapping flows.

use super::app::App;
use super::dialogs::{self, Dialog, DialogResponse};
use super::key_handler;

/// Dismiss the active dialog, sending Dismissed to the command thread if needed.
pub(super) fn dismiss_dialog(app: &mut App) {
    if app.command_dialog_active {
        app.send_dialog_response(DialogResponse::Dismissed);
    }
    app.active_dialog = None;
    app.command_dialog_active = false;
}

/// Handle Enter key in a dialog context.
pub(super) fn handle_dialog_submit(app: &mut App) {
    let is_command = app.command_dialog_active;

    match &app.active_dialog {
        Some(Dialog::QuitConfirm) => {}
        Some(Dialog::CloseTabConfirm) => {}
        Some(Dialog::FatalError { .. }) => {
            app.active_dialog = None;
            app.should_quit = true;
        }

        Some(Dialog::TextInput { editor, .. }) if is_command => {
            let text = editor.text.clone();
            app.send_dialog_response(DialogResponse::Text(text));
            app.active_dialog = None;
            app.command_dialog_active = false;
        }
        Some(Dialog::TextInput { editor, .. }) => {
            let path = editor.text.clone();
            app.active_dialog = None;
            key_handler::handle_new_tab_path(app, &path);
        }

        Some(Dialog::MultilineInput { editor, .. }) if is_command => {
            let text = editor.text.clone();
            app.send_dialog_response(DialogResponse::Text(text));
            app.active_dialog = None;
            app.command_dialog_active = false;
        }

        Some(Dialog::ListPicker { selected, .. }) if is_command => {
            let idx = *selected;
            app.send_dialog_response(DialogResponse::Index(idx));
            app.active_dialog = None;
            app.command_dialog_active = false;
        }

        Some(Dialog::ConfigShow(_)) if is_command => {
            config_show_submit(app);
        }

        Some(Dialog::WorkflowStepConfirm(_)) if is_command => {
            app.send_dialog_response(DialogResponse::Char('>'));
            app.active_dialog = None;
            app.command_dialog_active = false;
        }

        _ => {}
    }
}

/// Lexical validation for a new `agentsToModels` key typed in the config
/// dialog. Mirrors `data::session::AgentName` rules so bad keys are rejected
/// before they reach the config writer.
fn is_valid_map_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Handle Enter in the ConfigShow dialog: advance the add-mapping flow, save
/// the active inline edit, or begin editing the selected row.
fn config_show_submit(app: &mut App) {
    use dialogs::NewMapEntryPhase;

    let mut toast: Option<String> = None;
    let mut response: Option<String> = None;
    let mut begin_edit = false;

    if let Some(Dialog::ConfigShow(state)) = &mut app.active_dialog {
        match state.new_entry.clone() {
            // Phase 1 of Ctrl+N: confirm the agent name.
            Some(NewMapEntryPhase::Key) => {
                let key = state.editor.text.trim().to_string();
                if key.is_empty() {
                    toast = Some("Type an agent name, or press Esc to cancel".to_string());
                } else if !is_valid_map_key(&key) {
                    toast = Some(format!(
                        "'{key}' is not a valid agent name: use ASCII letters, digits, '-', '_' \
                         (max 64 chars)"
                    ));
                } else {
                    let field = format!("dynamicWorkflows.agentsToModels.{key}");
                    if let Some(idx) = state.rows.iter().position(|r| r.field == field) {
                        // Already mapped: jump to the existing row and edit it
                        // instead of silently overwriting.
                        let current = state.rows[idx].repo.clone();
                        state.selected = idx;
                        state.new_entry = None;
                        state.editing = true;
                        state.edit_column = 1;
                        state.editor = crate::frontend::tui::text_edit::TextEdit::new(false);
                        state.editor.set_text(&current);
                        toast = Some(format!("'{key}' is already mapped — editing its models"));
                    } else {
                        state.new_entry = Some(NewMapEntryPhase::Value { key });
                        state.editor = crate::frontend::tui::text_edit::TextEdit::new(false);
                    }
                }
            }
            // Phase 2 of Ctrl+N: save the model list for the new key.
            Some(NewMapEntryPhase::Value { key }) => {
                let value = state.editor.text.trim().to_string();
                if value.is_empty() {
                    toast =
                        Some("Enter at least one model name, or press Esc to cancel".to_string());
                } else {
                    response = Some(format!(
                        "dynamicWorkflows.agentsToModels.{key}\t{value}\trepo"
                    ));
                }
            }
            // Ctrl+N (single-phase): append a new guidance entry. The index is
            // the current entry count, so the config layer appends it (WI-0099).
            Some(NewMapEntryPhase::GuidanceEntry) => {
                let value = state.editor.text.trim().to_string();
                if value.is_empty() {
                    toast =
                        Some("Enter a guidance instruction, or press Esc to cancel".to_string());
                } else {
                    let next_index = state
                        .rows
                        .iter()
                        .filter(|r| r.field.starts_with("dynamicWorkflows.guidance."))
                        .count();
                    response = Some(format!(
                        "dynamicWorkflows.guidance.{next_index}\t{value}\trepo"
                    ));
                }
            }
            None if state.editing => {
                // Save the edited value: send "field\tvalue\tscope". The
                // value is trimmed — stray whitespace would otherwise fail
                // validation for numbers and agent::model specs.
                let row = &state.rows[state.selected];
                let scope = if state.edit_column == 0 {
                    "global"
                } else {
                    "repo"
                };
                response = Some(format!(
                    "{}\t{}\t{}",
                    row.field,
                    state.editor.text.trim(),
                    scope
                ));
            }
            None => begin_edit = true,
        }
    }

    if begin_edit {
        config_show_begin_edit(app);
    }
    if let Some(text) = toast {
        app.status_bar.text = text;
    }
    if let Some(edit_str) = response {
        app.send_dialog_response(DialogResponse::Text(edit_str));
        app.active_dialog = None;
        app.command_dialog_active = false;
    }
}

/// Begin inline editing of the selected ConfigShow row (Enter or `e` in
/// browse mode). Snaps the edit column to a writable scope for scope-
/// restricted fields, and refuses read-only rows with a status-bar hint.
fn config_show_begin_edit(app: &mut App) {
    let mut toast: Option<String> = None;

    if let Some(Dialog::ConfigShow(state)) = &mut app.active_dialog {
        if state.editing || state.new_entry.is_some() {
            return;
        }
        let Some(row) = state.rows.get(state.selected) else {
            return;
        };
        let (field, read_only, global_writable, repo_writable, global_val, repo_val) = (
            row.field.clone(),
            row.read_only,
            row.global_writable,
            row.repo_writable,
            row.global.clone(),
            row.repo.clone(),
        );

        if read_only {
            toast = Some(if field == "dynamicWorkflows.agentsToModels" {
                "Press Ctrl+N to add a mapping, or edit a per-agent row below".to_string()
            } else if field == "dynamicWorkflows.guidance" {
                "Press Ctrl+N to add a guidance entry, or edit a per-entry row below".to_string()
            } else {
                "This field is read-only".to_string()
            });
        } else {
            // Snap to a writable column so a repo-only field is never
            // written into the global config (and vice versa).
            let column = match (state.edit_column, global_writable, repo_writable) {
                (0, true, _) | (1, true, false) => Some(0),
                (1, _, true) | (0, false, true) => Some(1),
                _ => None,
            };
            match column {
                None => toast = Some("This field is read-only".to_string()),
                Some(column) => {
                    if column != state.edit_column {
                        toast = Some(if column == 1 {
                            format!("'{field}' is repo-only — editing the Repo value")
                        } else {
                            format!("'{field}' is global-only — editing the Global value")
                        });
                    }
                    state.edit_column = column;
                    state.error = None;
                    // Sensitive values are masked in the table; start the
                    // editor empty rather than seeding it with the mask.
                    let initial = if field == "remote.defaultAPIKey" {
                        String::new()
                    } else if column == 0 {
                        global_val
                    } else {
                        repo_val
                    };
                    state.editing = true;
                    state.editor = crate::frontend::tui::text_edit::TextEdit::new(false);
                    state.editor.set_text(&initial);
                }
            }
        }
    }

    if let Some(text) = toast {
        app.status_bar.text = text;
    }
}

pub(super) enum CursorDir {
    Left,
    Right,
    Home,
    End,
}

pub(super) fn handle_dialog_cursor(app: &mut App, dir: CursorDir) {
    match &mut app.active_dialog {
        Some(Dialog::TextInput { editor, .. }) | Some(Dialog::MultilineInput { editor, .. }) => {
            match dir {
                CursorDir::Left => editor.move_left(),
                CursorDir::Right => editor.move_right(),
                CursorDir::Home => editor.move_home(),
                CursorDir::End => editor.move_end(),
            }
        }
        Some(Dialog::ConfigShow(state)) => {
            if state.editing {
                match dir {
                    CursorDir::Left => state.editor.move_left(),
                    CursorDir::Right => state.editor.move_right(),
                    CursorDir::Home => state.editor.move_home(),
                    CursorDir::End => state.editor.move_end(),
                }
            } else {
                match dir {
                    CursorDir::Left | CursorDir::Home => state.edit_column = 0,
                    CursorDir::Right | CursorDir::End => state.edit_column = 1,
                }
            }
        }
        _ => {}
    }
}

pub(super) fn handle_dialog_backspace(app: &mut App) {
    match &mut app.active_dialog {
        Some(Dialog::TextInput { editor, .. }) | Some(Dialog::MultilineInput { editor, .. }) => {
            editor.backspace();
        }
        Some(Dialog::ConfigShow(state)) if state.editing => {
            state.editor.backspace();
        }
        _ => {}
    }
}

pub(super) fn handle_dialog_delete(app: &mut App) {
    match &mut app.active_dialog {
        Some(Dialog::TextInput { editor, .. }) | Some(Dialog::MultilineInput { editor, .. }) => {
            editor.delete();
        }
        Some(Dialog::ConfigShow(state)) if state.editing => {
            state.editor.delete();
        }
        _ => {}
    }
}

/// Handle arrow-key / page-key scrolling in list-based dialogs. `direction`
/// is a signed step count (e.g. -1 for one row up, +10 for a page down).
pub(super) fn handle_dialog_scroll(app: &mut App, direction: i32) {
    let step = direction.unsigned_abs() as usize;
    match &mut app.active_dialog {
        Some(Dialog::ListPicker {
            items, selected, ..
        }) => {
            let len = items.len();
            if len == 0 {
                return;
            }
            if direction < 0 {
                *selected = selected.saturating_sub(step);
            } else {
                *selected = (*selected + step).min(len - 1);
            }
        }
        Some(Dialog::AmieConditionDetail(state)) => {
            if direction < 0 {
                state.scroll = state.scroll.saturating_sub(step);
            } else {
                state.scroll = state.scroll.saturating_add(step);
            }
        }
        Some(Dialog::ConfigShow(state)) => {
            // Row navigation is frozen mid-edit: the editor holds the value
            // of the row the edit started on.
            if state.editing || state.new_entry.is_some() {
                return;
            }
            let len = state.rows.len();
            if len == 0 {
                return;
            }
            if direction < 0 {
                state.selected = state.selected.saturating_sub(step);
            } else {
                state.selected = (state.selected + step).min(len - 1);
            }
        }
        _ => {}
    }
}

/// Handle a character key press in a dialog.
pub(super) fn handle_dialog_char(app: &mut App, c: char) {
    let is_command = app.command_dialog_active;

    match app.active_dialog.as_ref() {
        // ── Always UI-originated ─────────────────────────────────────
        Some(Dialog::QuitConfirm) => {
            // Only Ctrl-C (handled via Action::CloseTabOrQuit) or Esc
            // (handled via Action::DismissDialog) are valid here. Ignore
            // all regular char keys.
        }
        Some(Dialog::CloseTabConfirm) => {
            // Only Ctrl-C, Ctrl-T, or Esc are valid. Ignore regular chars.
        }
        Some(Dialog::WorkflowCancelConfirm) => match c {
            'y' | 'Y' => {
                // Tell the engine to abort via the dialog response channel.
                app.send_dialog_response(DialogResponse::Char('a'));
                app.active_dialog = None;
                app.command_dialog_active = false;
            }
            'n' | 'N' => {
                // Just dismiss — the engine keeps running.
                app.active_dialog = None;
            }
            _ => {}
        },
        // WI 0102: `y` confirms removal by dispatching `amie remove <name>`
        // through the ordinary Layer-2 path; `n`/`Esc` dismisses. This dialog
        // decides nothing itself — it only collects the confirmation.
        Some(Dialog::AmieRemoveConfirm { name }) => {
            let name = name.clone();
            match c {
                'y' | 'Y' => {
                    app.active_dialog = None;
                    let mut arguments = std::collections::BTreeMap::new();
                    arguments.insert(
                        "name".to_string(),
                        crate::command::dispatch::parsed_input::ArgValue::Single(name.clone()),
                    );
                    // This dialog IS the confirmation: pass `--yes` so Layer 2
                    // removes the persistent directory without a second prompt.
                    let mut flags = std::collections::BTreeMap::new();
                    flags.insert(
                        "yes".to_string(),
                        crate::command::dispatch::parsed_input::FlagValue::Bool(true),
                    );
                    app.spawn_command(
                        &format!("amie remove {name} --yes"),
                        crate::command::dispatch::parsed_input::ParsedCommandBoxInput {
                            path: vec!["amie".into(), "remove".into()],
                            flags,
                            arguments,
                        },
                    );
                }
                'n' | 'N' => {
                    app.active_dialog = None;
                }
                _ => {}
            }
        }

        // ── Command-originated dialogs ───────────────────────────────
        Some(Dialog::YesNo { .. }) if is_command => match c {
            'y' => {
                app.send_dialog_response(DialogResponse::Yes);
                app.active_dialog = None;
                app.command_dialog_active = false;
            }
            'n' => {
                app.send_dialog_response(DialogResponse::No);
                app.active_dialog = None;
                app.command_dialog_active = false;
            }
            _ => {}
        },
        Some(Dialog::YesNoCancel { .. }) if is_command => match c {
            'y' => {
                app.send_dialog_response(DialogResponse::Yes);
                app.active_dialog = None;
                app.command_dialog_active = false;
            }
            'n' => {
                app.send_dialog_response(DialogResponse::No);
                app.active_dialog = None;
                app.command_dialog_active = false;
            }
            _ => {}
        },

        Some(Dialog::MountScope { .. }) => {
            app.send_dialog_response(DialogResponse::Char(c));
            app.active_dialog = None;
            app.command_dialog_active = false;
        }
        Some(Dialog::AgentSetup { .. }) => {
            app.send_dialog_response(DialogResponse::Char(c));
            app.active_dialog = None;
            app.command_dialog_active = false;
        }
        Some(Dialog::AgentAuth { .. }) => {
            app.send_dialog_response(DialogResponse::Char(c));
            app.active_dialog = None;
            app.command_dialog_active = false;
        }
        Some(Dialog::Custom { ref keys, .. }) => {
            if keys.iter().any(|(ch, _)| *ch == c) {
                app.send_dialog_response(DialogResponse::Char(c));
                app.active_dialog = None;
                app.command_dialog_active = false;
            }
        }

        Some(Dialog::WorkflowControlBoard { .. }) => {
            app.send_dialog_response(DialogResponse::Char(c));
            app.active_dialog = None;
            app.command_dialog_active = false;
        }
        Some(Dialog::WorkflowStepError { .. }) => {
            app.send_dialog_response(DialogResponse::Char(c));
            app.active_dialog = None;
            app.command_dialog_active = false;
        }
        Some(Dialog::WorkflowYoloCountdown { .. }) => {
            app.send_dialog_response(DialogResponse::Char(c));
            app.active_dialog = None;
            app.command_dialog_active = false;
        }

        Some(Dialog::WorkflowStepConfirm(_)) => {
            // Only Ctrl+W is handled as a char here — it escalates to the full WCB.
            // Enter and Esc are handled by SubmitCommand and DismissDialog actions.
        }

        Some(Dialog::KindSelect { options, .. }) if is_command => {
            if let Some(digit) = c.to_digit(10) {
                let idx = digit as usize;
                if idx >= 1 && idx <= options.len() {
                    app.send_dialog_response(DialogResponse::Index(idx - 1));
                    app.active_dialog = None;
                    app.command_dialog_active = false;
                }
            }
        }

        // ── Text input in dialogs ────────────────────────────────────
        Some(Dialog::TextInput { .. }) | Some(Dialog::MultilineInput { .. }) => {
            if let Some(Dialog::TextInput { editor, .. })
            | Some(Dialog::MultilineInput { editor, .. }) = &mut app.active_dialog
            {
                editor.insert_char(c);
            }
        }

        Some(Dialog::ConfigShow(state)) if state.editing => {
            if let Some(Dialog::ConfigShow(state)) = &mut app.active_dialog {
                state.editor.insert_char(c);
            }
        }
        Some(Dialog::ConfigShow(_)) => {
            // Browse mode: `e` starts editing the selected row (same as
            // Enter); other char keys are ignored.
            if c == 'e' {
                config_show_begin_edit(app);
            }
        }

        // ── Non-interactive / fallback dialogs ─────────────────────
        // The amie detail modal ignores char keys — Esc dismisses and the
        // arrow/page keys scroll via `handle_dialog_scroll`.
        Some(Dialog::AmieConditionDetail(_))
        | Some(Dialog::Loading { .. })
        | Some(Dialog::ListPicker { .. })
        | Some(Dialog::KindSelect { .. })
        | Some(Dialog::YesNo { .. })
        | Some(Dialog::YesNoCancel { .. })
        | Some(Dialog::FatalError { .. }) => {}

        None => {}
    }
}
