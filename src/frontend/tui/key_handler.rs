//! Keyboard event handling: focus-context detection, keymap action
//! dispatch, PTY passthrough, clipboard, and command submission.

use crossterm::event::{KeyCode, KeyModifiers};

use super::app::{App, Focus};
use super::dialog_router::{self, CursorDir};
use super::dialogs::{self, Dialog, DialogResponse};
use super::event_loop::resize_slots_to_terminal;
use super::keymap::{Action, FocusContext};
use super::{command_box, git_sidebar, keymap, tabs, text_edit};
use tabs::ContainerWindowState;

/// Returns true when the active tab has a command currently running.
fn command_box_locked(app: &App) -> bool {
    matches!(
        app.active_tab().execution_phase,
        tabs::ExecutionPhase::Running { .. }
    )
}

/// Determine focus context and dispatch the key event through the keymap.
pub(super) fn handle_key_event(app: &mut App, key: crossterm::event::KeyEvent) {
    let ctx = if app.active_dialog.is_some() {
        FocusContext::Dialog
    } else if app.active_tab().container_overlay_active()
        && matches!(
            app.active_tab().execution_phase,
            tabs::ExecutionPhase::Running { .. }
        )
    {
        // Only treat the container overlay as the focus target while a command is
        // actively running.  Once the command finishes the overlay is closed, but
        // guard here too so a race can't leave the user unable to type.
        FocusContext::ContainerMaximized
    } else if app.active_tab().is_amie
        && app.active_tab().container_slots.is_empty()
        && app.focus == Focus::ExecutionWindow
    {
        // WI 0102: the amie condition list holds focus. While an attach session
        // owns the tab's slots (`container_slots` non-empty) this falls through
        // to the ordinary ContainerMaximized/ExecutionWindow handling, so
        // Ctrl-S slot cycling and PTY passthrough behave exactly as in a normal
        // workflow run.
        FocusContext::AmieList
    } else {
        match app.focus {
            Focus::CommandBox => FocusContext::CommandBox,
            Focus::ExecutionWindow => FocusContext::ExecutionWindow,
        }
    };

    // WorkflowControlBoard intercepts arrow keys and Ctrl+Enter before the
    // generic keymap so they map to workflow navigation rather than scroll/cursor.
    if matches!(app.active_dialog, Some(Dialog::WorkflowControlBoard(_)))
        && handle_workflow_control_board_key(app, key)
    {
        return;
    }

    // TUI-2: Yolo countdown dialog allows tab switching — dismiss the dialog
    // (countdown continues in the tab label) and switch tabs. With only 1 tab,
    // swallow the key so the generic char handler doesn't close the dialog.
    if matches!(app.active_dialog, Some(Dialog::WorkflowYoloCountdown(_)))
        && key.modifiers.contains(KeyModifiers::CONTROL)
    {
        match key.code {
            KeyCode::Char('a') | KeyCode::Char('d') => {
                if app.tabs.len() > 1 {
                    // Clear user-activity so the departing tab stays "stuck"
                    // and doesn't send a false StepUnstuck on switch-back.
                    app.active_dialog = None;
                    if key.code == KeyCode::Char('a') {
                        app.switch_to_prev_tab();
                    } else {
                        app.switch_to_next_tab();
                    }
                }
                return;
            }
            _ => {}
        }
    }

    // Ctrl-S also rotates the focused parallel container while the yolo
    // countdown modal is open on it, mirroring the Ctrl-A/D tab-switch
    // carve-out above (a modal shouldn't block the one navigation action
    // that lets the user check on/dismiss a sibling container). The modal is
    // dismissed here; `tick_all_tabs` re-derives it next tick from whichever
    // slot is now focused, so it reopens automatically on rotating back to a
    // slot whose countdown is still running.
    if matches!(app.active_dialog, Some(Dialog::WorkflowYoloCountdown(_)))
        && key.code == KeyCode::Char('s')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && app.active_tab().has_multiple_slots()
    {
        app.active_dialog = None;
        app.active_tab_mut().cycle_focused_slot();
        return;
    }

    // TUI-3: In MultilineInput dialogs, bare Enter inserts a newline while
    // Ctrl+Enter submits. The generic keymap maps Enter → SubmitCommand for
    // all dialogs, so we intercept here where we can inspect the dialog type.
    // Ctrl+S is also accepted as a submit keybinding because many terminals
    // cannot distinguish Ctrl+Enter from bare Enter without the kitty
    // keyboard protocol.
    if matches!(app.active_dialog, Some(Dialog::MultilineInput { .. })) {
        if key.code == KeyCode::Enter {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let shift = key.modifiers.contains(KeyModifiers::SHIFT);
            if ctrl || shift {
                dialog_router::handle_dialog_submit(app);
            } else {
                if let Some(Dialog::MultilineInput { editor, .. }) = &mut app.active_dialog {
                    editor.insert_newline();
                }
            }
            return;
        }
        if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
            dialog_router::handle_dialog_submit(app);
            return;
        }
    }

    // WI-0096 §6: Ctrl-S cycles the focused parallel container when more than
    // one is active. With zero or one slot it falls through untouched, so a
    // single container still receives Ctrl-S (flow control) via the PTY.
    if key.code == KeyCode::Char('s')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && app.active_dialog.is_none()
        && app.active_tab().has_multiple_slots()
    {
        let tab = app.active_tab_mut();
        tab.cycle_focused_slot();
        // No manual resize here: `tick_all_tabs` keeps every slot's parser
        // and PTY in lockstep with the overlay's actual inner rect, so the
        // rotated-in slot is already correctly sized.
        return;
    }

    // WI 0102: Ctrl-A inside the New Tab dialog opens the amie tab. Safe
    // because keymap.rs already gates the global Ctrl-A -> PreviousTab mapping
    // on `ctx != FocusContext::Dialog`, so the key is genuinely unclaimed here.
    // Do NOT add a second global Ctrl-A mapping and do NOT relax that guard.
    if key.code == KeyCode::Char('a')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(&app.active_dialog, Some(Dialog::TextInput { title, .. }) if title == "New Tab")
    {
        app.active_dialog = None;
        app.command_dialog_active = false;
        app.open_or_focus_amie_tab();
        return;
    }

    let action = keymap::map_key(key, ctx);

    match action {
        // ── Global actions ────────────────────────────────────────────
        Action::OpenNewTabDialog => {
            // Ctrl-T while CloseTabConfirm is open closes just this tab.
            if matches!(app.active_dialog, Some(Dialog::CloseTabConfirm)) {
                app.active_dialog = None;
                app.close_active_tab();
                return;
            }
            let cwd = app
                .active_tab()
                .session
                .working_dir()
                .to_string_lossy()
                .to_string();
            app.active_dialog = Some(Dialog::TextInput {
                title: "New Tab".to_string(),
                prompt: "Working directory:\nPress Ctrl-A to open amie".to_string(),
                editor: {
                    let mut ed = text_edit::TextEdit::new(false);
                    ed.set_text(&cwd);
                    ed
                },
            });
            app.command_dialog_active = false;
        }
        Action::PreviousTab => app.switch_to_prev_tab(),
        Action::NextTab => app.switch_to_next_tab(),
        Action::CloseTabOrQuit => {
            // Second Ctrl-C while QuitConfirm or CloseTabConfirm is open
            // confirms the quit action immediately.
            if matches!(app.active_dialog, Some(Dialog::QuitConfirm)) {
                app.active_dialog = None;
                app.should_quit = true;
                return;
            }
            if matches!(app.active_dialog, Some(Dialog::CloseTabConfirm)) {
                app.active_dialog = None;
                app.should_quit = true;
                return;
            }
            // A fatal startup error leaves nothing to return to — Ctrl-C
            // quits outright, same as Enter/Esc.
            if matches!(app.active_dialog, Some(Dialog::FatalError { .. })) {
                app.active_dialog = None;
                app.should_quit = true;
                return;
            }
            if app.active_dialog.is_some() {
                return;
            }
            // If a workflow is active in the focused tab, prefer the
            // workflow-cancel confirmation over the close-tab one — old amux
            // semantics. The user can still escape and Ctrl+C again to close
            // the tab if they really mean it.
            let workflow_active = app
                .active_tab()
                .workflow_state
                .lock()
                .map(|g| g.is_some())
                .unwrap_or(false);
            if workflow_active
                && matches!(
                    app.active_tab().execution_phase,
                    tabs::ExecutionPhase::Running { .. }
                )
            {
                app.active_dialog = Some(Dialog::WorkflowCancelConfirm);
            } else if app.tabs.len() > 1 {
                app.active_dialog = Some(Dialog::CloseTabConfirm);
            } else {
                app.active_dialog = Some(Dialog::QuitConfirm);
            }
        }
        Action::CycleContainerWindow => {
            let tab = app.active_tab_mut();
            tab.container_window_state = tab.container_window_state.cycle();
            // Selection coords are relative to the window the drag started
            // in; cycling swaps which window owns selections, so drop it.
            tab.mouse_selection = None;
            if tab.container_window_state != ContainerWindowState::Hidden {
                resize_slots_to_terminal(tab);
            }
        }
        Action::ToggleWorkflowStrip => {
            let tab = app.active_tab_mut();
            tab.workflow_strip_state = tab.workflow_strip_state.toggle();
            // The strip always opens at the top of the stage; a stale offset
            // from a previous expansion would hide the first steps.
            tab.workflow_strip_scroll_offset = 0;
            // Expanding puts the container overlay away and collapsing brings
            // it back at a different height, so any selection anchored in it
            // no longer means anything.
            tab.mouse_selection = None;
            if tab.container_window_state != ContainerWindowState::Hidden {
                resize_slots_to_terminal(tab);
            }
        }
        Action::ToggleGitSidebar => {
            // WI 0102: the git sidebar is meaningless for the amie tab's
            // synthetic session, so Ctrl-G is a no-op while it is active.
            if app.active_tab().is_amie {
                return;
            }
            let tab = app.active_tab_mut();
            tab.git_sidebar_state = match tab.git_sidebar_state {
                git_sidebar::GitSidebarState::Open => git_sidebar::GitSidebarState::Closed,
                git_sidebar::GitSidebarState::Closed => git_sidebar::GitSidebarState::Open,
            };
            // Opening/closing the sidebar changes the width of the left chunk
            // that the container overlay occupies, so reflow the container PTY
            // to the new width. This is needed even when the container is
            // Maximized (it fills the left chunk, not the whole frame).
            if tab.container_window_state != ContainerWindowState::Hidden {
                resize_slots_to_terminal(tab);
            }
        }
        Action::WorkflowControl => {
            let engine_tx = app
                .active_tab()
                .engine_tx_shared
                .lock()
                .ok()
                .and_then(|g| g.clone());
            if let Some(tx) = engine_tx {
                if matches!(app.active_dialog, Some(Dialog::WorkflowStepConfirm(_))) {
                    app.send_dialog_response(DialogResponse::Char('W'));
                    app.active_dialog = None;
                    app.command_dialog_active = false;
                } else if app.command_dialog_active {
                    dialog_router::dismiss_dialog(app);
                }
                let focused_step = app
                    .active_tab()
                    .focused_slot()
                    .map(|slot| slot.step_name.clone())
                    .unwrap_or_default();
                let _ = tx.send(crate::engine::workflow::EngineRequest::OpenControlBoard {
                    step_name: focused_step,
                });
            }
        }
        Action::OpenConfigShow => {
            // Run `config show` through dispatch so the command layer
            // computes the rows and the frontend trait presents the dialog.
            let parsed = crate::command::dispatch::parsed_input::ParsedCommandBoxInput {
                path: vec!["config".into(), "show".into()],
                flags: Default::default(),
                arguments: Default::default(),
            };
            app.spawn_command("config show", parsed);
        }

        // ── Command box actions ───────────────────────────────────────
        Action::SubmitCommand => {
            if ctx == FocusContext::Dialog {
                dialog_router::handle_dialog_submit(app);
            } else if !command_box_locked(app) {
                handle_command_submit(app);
            }
        }
        Action::AutocompleteNext => {
            app.update_suggestions();
            if !app.suggestion_row.is_empty() {
                let suggestion = app.suggestion_row[0].clone();
                app.command_input.set_text(&suggestion);
            }
        }
        Action::AutocompletePrev => {
            app.update_suggestions();
            if let Some(suggestion) = app.suggestion_row.last().cloned() {
                app.command_input.set_text(&suggestion);
            }
        }
        Action::FocusExecutionWindow => {
            app.focus = Focus::ExecutionWindow;
        }

        // ── Execution window actions ──────────────────────────────────
        Action::FocusCommandBox => {
            app.focus = Focus::CommandBox;
        }
        Action::ScrollUp => {
            if ctx == FocusContext::Dialog {
                dialog_router::handle_dialog_scroll(app, -1);
            } else if ctx == FocusContext::AmieList {
                if let Some(state) = app.active_tab_mut().amie.as_mut() {
                    state.move_selection(-1);
                }
            } else {
                let tab = app.active_tab_mut();
                tab.scroll_offset = tab.scroll_offset.saturating_add(1);
            }
        }
        Action::ScrollDown => {
            if ctx == FocusContext::Dialog {
                dialog_router::handle_dialog_scroll(app, 1);
            } else if ctx == FocusContext::AmieList {
                if let Some(state) = app.active_tab_mut().amie.as_mut() {
                    state.move_selection(1);
                }
            } else {
                let tab = app.active_tab_mut();
                tab.scroll_offset = tab.scroll_offset.saturating_sub(1);
            }
        }
        Action::ScrollPageUp => {
            if ctx == FocusContext::Dialog {
                dialog_router::handle_dialog_scroll(app, -10);
            } else {
                let tab = app.active_tab_mut();
                tab.scroll_offset = tab.scroll_offset.saturating_add(20);
            }
        }
        Action::ScrollPageDown => {
            if ctx == FocusContext::Dialog {
                dialog_router::handle_dialog_scroll(app, 10);
            } else {
                let tab = app.active_tab_mut();
                tab.scroll_offset = tab.scroll_offset.saturating_sub(20);
            }
        }
        Action::ScrollToTop => {
            let tab = app.active_tab_mut();
            tab.scroll_offset = usize::MAX / 2;
        }
        Action::ScrollToBottom => {
            let tab = app.active_tab_mut();
            tab.scroll_offset = 0;
        }
        Action::CopySelection => {
            copy_selection_to_clipboard(app);
        }
        Action::ToggleStatusLog => {
            let tab = app.active_tab_mut();
            tab.status_log_collapsed = !tab.status_log_collapsed;
        }

        // ── Dialog actions ────────────────────────────────────────────
        Action::DismissDialog => {
            // A fatal startup error cannot be dismissed back into a usable
            // app — Esc quits, same as Enter.
            if matches!(app.active_dialog, Some(Dialog::FatalError { .. })) {
                app.active_dialog = None;
                app.should_quit = true;
                return;
            }
            // In ConfigShow editing / add-mapping mode, Esc cancels the edit
            // (back to browse) instead of closing the dialog.
            if let Some(Dialog::ConfigShow(state)) = &mut app.active_dialog {
                if state.editing || state.new_entry.is_some() {
                    state.editing = false;
                    state.new_entry = None;
                    state.error = None;
                    return;
                }
            }
            if matches!(app.active_dialog, Some(Dialog::WorkflowYoloCountdown(_))) {
                let tab = app.active_tab();
                if tab.dormant_slots.is_empty() {
                    tab.yolo_cancel_flag
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                } else if let Some(slot) = tab.focused_slot() {
                    // Parallel group: the modal is only ever shown for the
                    // focused slot (see `tick_all_tabs`), so cancel that
                    // slot's countdown rather than the tab-level one.
                    slot.yolo_cancel_flag
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                app.active_dialog = None;
                return;
            }
            dialog_router::dismiss_dialog(app);
        }
        Action::NewMapEntry => {
            // Ctrl+N in the config dialog: start an add-entry flow. On a
            // guidance section row it starts the single-phase guidance entry
            // flow; on an agentsToModels row it starts the two-phase
            // key→value flow. No-op elsewhere.
            if let Some(Dialog::ConfigShow(state)) = &mut app.active_dialog {
                if state.new_entry.is_none() {
                    let on_guidance_row = state
                        .rows
                        .get(state.selected)
                        .map(|r| {
                            r.field == "dynamicWorkflows.guidance"
                                || r.field.starts_with("dynamicWorkflows.guidance.")
                        })
                        .unwrap_or(false);
                    state.new_entry = Some(if on_guidance_row {
                        dialogs::NewMapEntryPhase::GuidanceEntry
                    } else {
                        dialogs::NewMapEntryPhase::Key
                    });
                    state.editing = true;
                    state.error = None;
                    // Both agentsToModels and guidance entries are repo-scoped.
                    state.edit_column = 1;
                    state.editor = crate::frontend::tui::text_edit::TextEdit::new(false);
                }
            }
        }

        // ── Text input actions ────────────────────────────────────────
        Action::Char(c) => {
            if ctx == FocusContext::Dialog {
                dialog_router::handle_dialog_char(app, c);
            } else if command_box_locked(app) {
                // Command box is read-only while a command is executing.
            } else if c == 'q' && app.command_input.text.is_empty() {
                // `q` with an empty input opens the quit dialog (old-TUI parity).
                app.active_dialog = Some(Dialog::QuitConfirm);
            } else {
                app.command_input.insert_char(c);
                app.input_error = None;
                app.update_suggestions();
            }
        }
        Action::Backspace => {
            if ctx == FocusContext::Dialog {
                dialog_router::handle_dialog_backspace(app);
            } else if !command_box_locked(app) {
                app.command_input.backspace();
                app.input_error = None;
                app.update_suggestions();
            }
        }
        Action::Delete => {
            if ctx == FocusContext::Dialog {
                dialog_router::handle_dialog_delete(app);
            } else if !command_box_locked(app) {
                app.command_input.delete();
                app.input_error = None;
                app.update_suggestions();
            }
        }
        Action::BackspaceWord => {
            if !command_box_locked(app) {
                app.command_input.backspace_word();
                app.input_error = None;
                app.update_suggestions();
            }
        }
        Action::CursorLeft => {
            if ctx == FocusContext::Dialog {
                dialog_router::handle_dialog_cursor(app, CursorDir::Left);
            } else if !command_box_locked(app) {
                app.command_input.move_left();
            }
        }
        Action::CursorRight => {
            if ctx == FocusContext::Dialog {
                dialog_router::handle_dialog_cursor(app, CursorDir::Right);
            } else if !command_box_locked(app) {
                app.command_input.move_right();
            }
        }
        Action::CursorWordLeft => {
            if !command_box_locked(app) {
                app.command_input.move_word_left();
            }
        }
        Action::CursorWordRight => {
            if !command_box_locked(app) {
                app.command_input.move_word_right();
            }
        }
        Action::CursorHome => {
            if ctx == FocusContext::Dialog {
                dialog_router::handle_dialog_cursor(app, CursorDir::Home);
            } else if !command_box_locked(app) {
                app.command_input.move_home();
            }
        }
        Action::CursorEnd => {
            if ctx == FocusContext::Dialog {
                dialog_router::handle_dialog_cursor(app, CursorDir::End);
            } else if !command_box_locked(app) {
                app.command_input.move_end();
            }
        }
        Action::InsertNewline => {
            if !command_box_locked(app) {
                app.command_input.insert_newline();
            }
        }

        // ── amie list actions (WI 0102) ───────────────────────────────
        // Each either opens a dialog or dispatches through `spawn_command`
        // into Layer 2. None calls a gateway method directly — the keys are a
        // shortcut over the same Layer-2 path the command box uses, never a
        // second path.
        Action::AmieShowDetail => {
            let detail = app.active_tab().amie.as_ref().and_then(|state| {
                let condition = state.selected_condition()?;
                let runs = state
                    .snapshot
                    .lock()
                    .ok()
                    .map(|snap| snap.runs.clone())
                    .unwrap_or_default();
                Some(dialogs::AmieDetailState {
                    name: condition.name.clone(),
                    condition,
                    runs,
                    scroll: 0,
                })
            });
            if let Some(detail) = detail {
                app.active_dialog = Some(Dialog::AmieConditionDetail(detail));
            }
        }
        Action::AmieAttach => {
            let name = app
                .active_tab()
                .amie
                .as_ref()
                .and_then(|state| state.selected_name());
            if let Some(name) = name {
                crate::frontend::tui::amie_attach::start_amie_attach(app, &name);
            }
        }
        Action::AmieNew => {
            let mut flags = std::collections::BTreeMap::new();
            flags.insert(
                "interview".to_string(),
                crate::command::dispatch::parsed_input::FlagValue::Bool(true),
            );
            app.spawn_command(
                "amie add --interview",
                crate::command::dispatch::parsed_input::ParsedCommandBoxInput {
                    path: vec!["amie".into(), "add".into()],
                    flags,
                    arguments: Default::default(),
                },
            );
        }
        Action::AmiePause => {
            amie_dispatch_by_name(app, "pause");
        }
        Action::AmieResume => {
            amie_dispatch_by_name(app, "resume");
        }
        Action::AmieDelete => {
            let name = app
                .active_tab()
                .amie
                .as_ref()
                .and_then(|state| state.selected_name());
            if let Some(name) = name {
                app.active_dialog = Some(Dialog::AmieRemoveConfirm { name });
            }
        }

        // ── PTY passthrough ───────────────────────────────────────────
        Action::ForwardToPty(key_event) => {
            forward_key_to_pty(app, key_event);
        }

        Action::None => {
            // When the execution window is focused and the command is finished,
            // any unhandled key press returns focus to the command box.
            if ctx == FocusContext::ExecutionWindow {
                let done_or_error = matches!(
                    app.active_tab().execution_phase,
                    tabs::ExecutionPhase::Done { .. } | tabs::ExecutionPhase::Error { .. }
                );
                if done_or_error {
                    app.focus = Focus::CommandBox;
                }
            }
        }
    }
}

/// Extract the selected text from a snapshot. Range is inclusive on both ends;
/// trailing whitespace per line is stripped; rows are joined with `\n`.
pub(super) fn extract_selection_text(sel: &tabs::TextSelection) -> String {
    let (sr, sc, er, ec) = if sel.start_row < sel.end_row
        || (sel.start_row == sel.end_row && sel.start_col <= sel.end_col)
    {
        (
            sel.start_row as usize,
            sel.start_col as usize,
            sel.end_row as usize,
            sel.end_col as usize,
        )
    } else {
        (
            sel.end_row as usize,
            sel.end_col as usize,
            sel.start_row as usize,
            sel.start_col as usize,
        )
    };
    let mut result = String::new();
    for row in sr..=er {
        if row >= sel.snapshot.len() {
            break;
        }
        let row_data = &sel.snapshot[row];
        let col_start = if row == sr { sc } else { 0 };
        let col_end = if row == er {
            (ec + 1).min(row_data.len())
        } else {
            row_data.len()
        };
        let mut line = String::new();
        for col in col_start..col_end {
            if col < row_data.len() {
                line.push_str(&row_data[col]);
            }
        }
        result.push_str(line.trim_end());
        if row < er {
            result.push('\n');
        }
    }
    result
}

// ─── PTY forwarding ──────────────────────────────────────────────────────────

fn forward_key_to_pty(app: &mut App, key: crossterm::event::KeyEvent) {
    if let Some(bytes) = key_to_bytes(&key) {
        // Keystrokes (incl. Ctrl-C) go only to the focused slot's PTY.
        if let Some(slot) = app.active_tab_mut().focused_slot_mut() {
            if let Some(tx) = slot.container_stdin_tx.as_ref() {
                let _ = tx.send(bytes);
            }
        }
    }
}

fn key_to_bytes(key: &crossterm::event::KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let n = (c as u8).to_ascii_lowercase();
                if n.is_ascii_lowercase() {
                    return Some(vec![n - b'a' + 1]);
                }
            }
            let mut buf = [0u8; 4];
            Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        KeyCode::Enter => Some(b"\r".to_vec()),
        KeyCode::Backspace => Some(b"\x7f".to_vec()),
        KeyCode::Tab => Some(b"\t".to_vec()),
        KeyCode::Esc => Some(b"\x1b".to_vec()),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::F(n) => Some(format!("\x1b[{}~", n).into_bytes()),
        _ => None,
    }
}

// ─── Clipboard ───────────────────────────────────────────────────────────────

fn copy_selection_to_clipboard(app: &mut App) {
    let tab = app.active_tab();
    let text = match tab.mouse_selection.as_ref() {
        Some(sel) if !sel.snapshot.is_empty() => extract_selection_text(sel),
        _ => return,
    };
    if text.is_empty() {
        return;
    }
    match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(&text)) {
        Ok(()) => {
            // Drop the selection after a successful copy so the copy hint
            // disappears and a subsequent Ctrl+Y doesn't re-yank.
            app.active_tab_mut().mouse_selection = None;
        }
        Err(e) => {
            app.active_tab_mut()
                .status_log
                .lock()
                .map(|mut log| {
                    log.push(crate::frontend::tui::user_message::StatusLogEntry {
                        level: crate::data::message::MessageLevel::Error,
                        text: format!("clipboard unavailable: {e}"),
                    })
                })
                .ok();
        }
    }
}

// ─── Command submission ──────────────────────────────────────────────────────

/// Handle command submission from the command box.
fn handle_command_submit(app: &mut App) {
    let text = app.command_input.text.clone();
    if text.trim().is_empty() {
        return;
    }

    match command_box::parse_input(&text) {
        Ok(parsed) => {
            app.input_error = None;
            app.command_input.set_text("");
            app.suggestion_row.clear();
            app.spawn_command(&text, parsed);
        }
        Err(err) => {
            app.input_error = Some(command_box::format_parse_error(&err));
        }
    }
}

// ─── amie list helpers (WI 0102) ─────────────────────────────────────────────

/// Dispatch an `amie <subcommand> <name>` action (pause/resume) for the
/// selected condition through the ordinary Layer-2 path. No-op when the list
/// is empty.
fn amie_dispatch_by_name(app: &mut App, subcommand: &str) {
    let name = app
        .active_tab()
        .amie
        .as_ref()
        .and_then(|state| state.selected_name());
    let Some(name) = name else {
        return;
    };
    let mut arguments = std::collections::BTreeMap::new();
    arguments.insert(
        "name".to_string(),
        crate::command::dispatch::parsed_input::ArgValue::Single(name.clone()),
    );
    app.spawn_command(
        &format!("amie {subcommand} {name}"),
        crate::command::dispatch::parsed_input::ParsedCommandBoxInput {
            path: vec!["amie".into(), subcommand.into()],
            flags: Default::default(),
            arguments,
        },
    );
}

// ─── WorkflowControlBoard special handler ────────────────────────────────────

/// Handle arrow keys, Ctrl+Enter, and `[d]` for the WorkflowControlBoard dialog.
///
/// Returns `true` if the key was consumed; `false` to let it fall through to
/// the generic dialog handler (for char keys like 'a', Esc, etc.).
fn handle_workflow_control_board_key(app: &mut App, key: crossterm::event::KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    let can_finish = matches!(
        &app.active_dialog,
        Some(Dialog::WorkflowControlBoard(state)) if state.can_finish
    );

    let response = match key.code {
        KeyCode::Right => DialogResponse::Char('>'),
        KeyCode::Down => DialogResponse::Char('v'),
        KeyCode::Up => DialogResponse::Char('^'),
        KeyCode::Left => DialogResponse::Char('<'),
        // Many terminals cannot distinguish Ctrl+Enter from bare Enter
        // without the kitty keyboard protocol, so accept plain Enter too.
        KeyCode::Enter if can_finish => DialogResponse::Char('f'),
        KeyCode::Enter if ctrl => return false,
        KeyCode::Char('c') if ctrl => DialogResponse::Char('a'),
        _ => return false,
    };
    app.send_dialog_response(response);
    app.active_dialog = None;
    app.command_dialog_active = false;
    true
}

/// Handle path selection from the new-tab dialog.
pub(super) fn handle_new_tab_path(app: &mut App, path: &str) {
    let path = path.trim();
    if path.is_empty() {
        return;
    }
    let raw = std::path::PathBuf::from(path);
    let dir = if raw.is_absolute() {
        raw
    } else {
        app.active_tab().session.working_dir().join(raw)
    };
    if !dir.is_dir() {
        app.status_bar.text = format!("Not a directory: {path}");
        return;
    }

    let session = {
        let resolver = crate::data::session::StaticGitRootResolver::new(&dir);
        match crate::data::session::Session::open(
            dir.clone(),
            &resolver,
            crate::data::session::SessionOpenOptions::default(),
        ) {
            Ok(s) => s,
            Err(_) => {
                // Fallback for non-git directories: use dir as git root.
                match crate::data::session::Session::open_at_git_root(
                    dir.clone(),
                    dir.clone(),
                    crate::data::session::SessionOpenOptions::default(),
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        app.status_bar.text = format!("Failed to open session: {e}");
                        return;
                    }
                }
            }
        }
    };

    let is_git = session.git_root().join(".git").exists();
    let idx = app.add_tab(session);
    app.active_tab = idx;

    if is_git {
        app.spawn_command(
            "ready",
            crate::command::dispatch::parsed_input::ParsedCommandBoxInput {
                path: vec!["ready".into()],
                flags: Default::default(),
                arguments: Default::default(),
            },
        );
    } else {
        let mut flags = std::collections::BTreeMap::new();
        flags.insert(
            "watch".to_string(),
            crate::command::dispatch::parsed_input::FlagValue::Bool(true),
        );
        app.spawn_command(
            "status --watch",
            crate::command::dispatch::parsed_input::ParsedCommandBoxInput {
                path: vec!["status".into()],
                flags,
                arguments: Default::default(),
            },
        );
    }
}
