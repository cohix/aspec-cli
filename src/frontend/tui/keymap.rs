//! Keyboard shortcut definitions — every shortcut is defined here.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Actions produced by the keymap. The event loop matches these to state
/// transitions; no business logic lives here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    // ── Global ──────────────────────────────────────────────────────────
    OpenNewTabDialog,
    PreviousTab,
    NextTab,
    CloseTabOrQuit,
    CycleContainerWindow,
    /// Ctrl-O ("overview"): toggle the workflow state strip between its
    /// collapsed one-box-per-stage view and the expanded every-step view.
    ToggleWorkflowStrip,
    OpenConfigShow,
    WorkflowControl,
    ToggleGitSidebar,

    // ── Command box ─────────────────────────────────────────────────────
    SubmitCommand,
    AutocompleteNext,
    AutocompletePrev,
    FocusExecutionWindow,

    // ── Execution window ────────────────────────────────────────────────
    FocusCommandBox,
    ScrollUp,
    ScrollDown,
    ScrollPageUp,
    ScrollPageDown,
    ScrollToTop,
    ScrollToBottom,
    CopySelection,
    ToggleStatusLog,

    // ── Dialog ──────────────────────────────────────────────────────────
    DismissDialog,
    /// Ctrl+N in the config dialog: start the add-model-mapping flow.
    NewMapEntry,

    // ── amie list (WI 0102) ─────────────────────────────────────────────
    /// Enter — open the condition detail modal for the selected row.
    AmieShowDetail,
    /// a — attach to the selected condition's running container(s).
    AmieAttach,
    /// n — create a condition (drives the Layer-2 interview dialog chain).
    AmieNew,
    /// p — pause the selected condition.
    AmiePause,
    /// r — resume the selected condition.
    AmieResume,
    /// d — remove the selected condition (opens a confirmation first).
    AmieDelete,

    // ── Text input ──────────────────────────────────────────────────────
    Char(char),
    Backspace,
    Delete,
    BackspaceWord,
    CursorLeft,
    CursorRight,
    CursorWordLeft,
    CursorWordRight,
    CursorHome,
    CursorEnd,
    InsertNewline,

    // ── Passthrough to PTY ──────────────────────────────────────────────
    ForwardToPty(KeyEvent),

    // ── No-op ───────────────────────────────────────────────────────────
    None,
}

/// The focus context determines which key bindings are active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusContext {
    CommandBox,
    ExecutionWindow,
    Dialog,
    ContainerMaximized,
    /// The amie tab's condition list has focus (WI 0102). Only reachable when
    /// the active tab is the amie tab, focus is on the body, and no attach
    /// session owns the tab's container slots.
    AmieList,
}

/// Map a key event + focus context to an [`Action`].
pub fn map_key(key: KeyEvent, ctx: FocusContext) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // Global shortcuts — available in most contexts including maximized container.
    // Tab switching (Ctrl-A/D) is suppressed in Dialog context to prevent
    // dialogs from leaking across tabs (TUI-2). The yolo countdown dialog
    // is handled specially in the event loop.
    if ctrl {
        match key.code {
            KeyCode::Char('t') => return Action::OpenNewTabDialog,
            KeyCode::Char('a') if ctx != FocusContext::Dialog => return Action::PreviousTab,
            KeyCode::Char('d') if ctx != FocusContext::Dialog => return Action::NextTab,
            KeyCode::Char('m') if ctx != FocusContext::Dialog => {
                return Action::CycleContainerWindow
            }
            // Ctrl-O (SI, 0x0f) is intercepted in every context — including
            // ContainerMaximized, before the ForwardToPty path below — so the
            // workflow overview is always one keystroke away.
            KeyCode::Char('o') if ctx != FocusContext::Dialog => {
                return Action::ToggleWorkflowStrip
            }
            KeyCode::Char('w') => return Action::WorkflowControl,
            // Ctrl-G (BEL, 0x07) is rarely used by terminal programs, so we
            // intercept it here — before the ContainerMaximized ForwardToPty
            // path below — in every focus context so it never reaches the PTY.
            KeyCode::Char('g') => return Action::ToggleGitSidebar,
            _ => {}
        }
    }

    // Ctrl-C: forward to PTY when the container is maximized (so the
    // signal reaches the process inside the container); otherwise
    // trigger the close-tab / quit dialog.
    if ctrl && key.code == KeyCode::Char('c') {
        if ctx == FocusContext::ContainerMaximized {
            return Action::ForwardToPty(key);
        }
        return Action::CloseTabOrQuit;
    }
    if key.code == KeyCode::Char(',') && ctrl {
        return Action::OpenConfigShow;
    }

    match ctx {
        FocusContext::CommandBox => map_command_box_key(key, ctrl, shift),
        FocusContext::ExecutionWindow => map_execution_window_key(key, ctrl),
        FocusContext::Dialog => map_dialog_key(key, ctrl),
        FocusContext::ContainerMaximized => {
            if ctrl && key.code == KeyCode::Char('y') {
                Action::CopySelection
            } else {
                Action::ForwardToPty(key)
            }
        }
        FocusContext::AmieList => map_amie_list_key(key, ctrl),
    }
}

/// Key bindings for the amie condition list (WI 0102). Reached only through
/// `FocusContext::AmieList`; the global `ctrl` block in `map_key` runs first,
/// so `Ctrl-T`/`Ctrl-A`/`Ctrl-D`/`Ctrl-M`/`Ctrl-O`/`Ctrl-W`/`Ctrl-G`/`Ctrl-C`/
/// `Ctrl-,` keep their global meaning here.
fn map_amie_list_key(key: KeyEvent, ctrl: bool) -> Action {
    match key.code {
        KeyCode::Esc => Action::FocusCommandBox,
        KeyCode::Up => Action::ScrollUp,
        KeyCode::Down => Action::ScrollDown,
        KeyCode::PageUp => Action::ScrollPageUp,
        KeyCode::PageDown => Action::ScrollPageDown,
        KeyCode::Enter => Action::AmieShowDetail,
        KeyCode::Char('a') if !ctrl => Action::AmieAttach,
        KeyCode::Char('n') if !ctrl => Action::AmieNew,
        KeyCode::Char('p') if !ctrl => Action::AmiePause,
        KeyCode::Char('r') if !ctrl => Action::AmieResume,
        KeyCode::Char('d') if !ctrl => Action::AmieDelete,
        KeyCode::Char('y') if ctrl => Action::CopySelection,
        _ => Action::None,
    }
}

fn map_command_box_key(key: KeyEvent, ctrl: bool, shift: bool) -> Action {
    match key.code {
        KeyCode::Enter if ctrl || shift => Action::InsertNewline,
        KeyCode::Enter => Action::SubmitCommand,
        // Yank an execution-window mouse selection without moving focus
        // (no-op when no selection exists).
        KeyCode::Char('y') if ctrl => Action::CopySelection,
        KeyCode::BackTab => Action::AutocompletePrev,
        KeyCode::Tab if shift => Action::AutocompletePrev,
        KeyCode::Tab => Action::AutocompleteNext,
        KeyCode::Up => Action::FocusExecutionWindow,
        KeyCode::Backspace if ctrl => Action::BackspaceWord,
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Delete => Action::Delete,
        KeyCode::Left if ctrl => Action::CursorWordLeft,
        KeyCode::Right if ctrl => Action::CursorWordRight,
        KeyCode::Left => Action::CursorLeft,
        KeyCode::Right => Action::CursorRight,
        KeyCode::Home => Action::CursorHome,
        KeyCode::End => Action::CursorEnd,
        KeyCode::Char(c) if !ctrl => Action::Char(c),
        _ => Action::None,
    }
}

fn map_execution_window_key(key: KeyEvent, ctrl: bool) -> Action {
    match key.code {
        KeyCode::Esc => Action::FocusCommandBox,
        KeyCode::Up => Action::ScrollUp,
        KeyCode::Down => Action::ScrollDown,
        KeyCode::PageUp => Action::ScrollPageUp,
        KeyCode::PageDown => Action::ScrollPageDown,
        KeyCode::Char('b') if !ctrl => Action::ScrollToTop,
        KeyCode::Char('e') if !ctrl => Action::ScrollToBottom,
        KeyCode::Char('l') if !ctrl => Action::ToggleStatusLog,
        KeyCode::Char('y') if ctrl => Action::CopySelection,
        _ => Action::None,
    }
}

fn map_dialog_key(key: KeyEvent, ctrl: bool) -> Action {
    if key.code == KeyCode::Esc {
        return Action::DismissDialog;
    }
    match key.code {
        KeyCode::Char('n') if ctrl => Action::NewMapEntry,
        // Other Ctrl chords must not fall through as literal characters
        // (e.g. Ctrl+X inserting 'x' into an inline editor).
        KeyCode::Char(c) if !ctrl => Action::Char(c),
        KeyCode::Enter => Action::SubmitCommand,
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Delete => Action::Delete,
        KeyCode::Home => Action::CursorHome,
        KeyCode::End => Action::CursorEnd,
        KeyCode::Up => Action::ScrollUp,
        KeyCode::Down => Action::ScrollDown,
        KeyCode::PageUp => Action::ScrollPageUp,
        KeyCode::PageDown => Action::ScrollPageDown,
        KeyCode::Left => Action::CursorLeft,
        KeyCode::Right => Action::CursorRight,
        _ => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn ctrl_t_opens_new_tab_dialog() {
        let action = map_key(
            key(KeyCode::Char('t'), KeyModifiers::CONTROL),
            FocusContext::CommandBox,
        );
        assert_eq!(action, Action::OpenNewTabDialog);
    }

    #[test]
    fn enter_in_command_box_submits() {
        let action = map_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            FocusContext::CommandBox,
        );
        assert_eq!(action, Action::SubmitCommand);
    }

    #[test]
    fn esc_in_dialog_dismisses() {
        let action = map_key(key(KeyCode::Esc, KeyModifiers::NONE), FocusContext::Dialog);
        assert_eq!(action, Action::DismissDialog);
    }

    #[test]
    fn esc_in_execution_window_returns_to_command_box() {
        let action = map_key(
            key(KeyCode::Esc, KeyModifiers::NONE),
            FocusContext::ExecutionWindow,
        );
        assert_eq!(action, Action::FocusCommandBox);
    }

    #[test]
    fn b_in_execution_window_scrolls_to_top() {
        let action = map_key(
            key(KeyCode::Char('b'), KeyModifiers::NONE),
            FocusContext::ExecutionWindow,
        );
        assert_eq!(action, Action::ScrollToTop);
    }

    #[test]
    fn ctrl_c_closes_tab_or_quits() {
        for ctx in [
            FocusContext::CommandBox,
            FocusContext::ExecutionWindow,
            FocusContext::Dialog,
        ] {
            let action = map_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL), ctx);
            assert_eq!(action, Action::CloseTabOrQuit);
        }
    }

    #[test]
    fn ctrl_c_in_maximized_container_forwards_to_pty() {
        let k = key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let action = map_key(k, FocusContext::ContainerMaximized);
        assert_eq!(action, Action::ForwardToPty(k));
    }

    // ── Ctrl-G / git sidebar ───────────────────────────────────────────────

    #[test]
    fn ctrl_g_toggles_git_sidebar_in_all_contexts() {
        // Covers the Idle / Running / CommandBox scenarios from the work item:
        // the sidebar toggle is a global shortcut in every focus context.
        for ctx in [
            FocusContext::CommandBox,
            FocusContext::ExecutionWindow,
            FocusContext::Dialog,
            FocusContext::ContainerMaximized,
        ] {
            let action = map_key(key(KeyCode::Char('g'), KeyModifiers::CONTROL), ctx);
            assert_eq!(
                action,
                Action::ToggleGitSidebar,
                "Ctrl-G must toggle the git sidebar in {ctx:?}"
            );
        }
    }

    #[test]
    fn ctrl_g_in_maximized_container_is_not_forwarded_to_pty() {
        // Ctrl-G (BEL) must be intercepted before the ContainerMaximized
        // ForwardToPty fallthrough so it never reaches the running process.
        let k = key(KeyCode::Char('g'), KeyModifiers::CONTROL);
        let action = map_key(k, FocusContext::ContainerMaximized);
        assert_eq!(action, Action::ToggleGitSidebar);
        assert_ne!(action, Action::ForwardToPty(k));
    }

    #[test]
    fn plain_g_is_not_a_sidebar_toggle() {
        // Without Ctrl, `g` is ordinary input (a char in the command box).
        let action = map_key(
            key(KeyCode::Char('g'), KeyModifiers::NONE),
            FocusContext::CommandBox,
        );
        assert_ne!(action, Action::ToggleGitSidebar);
    }

    #[test]
    fn tab_in_command_box_autocompletes() {
        let action = map_key(
            key(KeyCode::Tab, KeyModifiers::NONE),
            FocusContext::CommandBox,
        );
        assert_eq!(action, Action::AutocompleteNext);
    }

    // ── Global shortcuts ──────────────────────────────────────────────────────

    #[test]
    fn ctrl_a_switches_to_previous_tab() {
        let action = map_key(
            key(KeyCode::Char('a'), KeyModifiers::CONTROL),
            FocusContext::CommandBox,
        );
        assert_eq!(action, Action::PreviousTab);
    }

    #[test]
    fn ctrl_d_switches_to_next_tab() {
        let action = map_key(
            key(KeyCode::Char('d'), KeyModifiers::CONTROL),
            FocusContext::CommandBox,
        );
        assert_eq!(action, Action::NextTab);
    }

    #[test]
    fn ctrl_m_cycles_container_window() {
        let action = map_key(
            key(KeyCode::Char('m'), KeyModifiers::CONTROL),
            FocusContext::CommandBox,
        );
        assert_eq!(action, Action::CycleContainerWindow);
    }

    #[test]
    fn ctrl_comma_opens_config_show() {
        let action = map_key(
            key(KeyCode::Char(','), KeyModifiers::CONTROL),
            FocusContext::CommandBox,
        );
        assert_eq!(action, Action::OpenConfigShow);
    }

    #[test]
    fn global_shortcuts_available_in_execution_window() {
        let action = map_key(
            key(KeyCode::Char('a'), KeyModifiers::CONTROL),
            FocusContext::ExecutionWindow,
        );
        assert_eq!(action, Action::PreviousTab);
    }

    #[test]
    fn tab_switching_suppressed_in_dialog() {
        let action = map_key(
            key(KeyCode::Char('d'), KeyModifiers::CONTROL),
            FocusContext::Dialog,
        );
        assert_ne!(
            action,
            Action::NextTab,
            "Ctrl-D must not switch tabs while a dialog is open"
        );
        let action = map_key(
            key(KeyCode::Char('a'), KeyModifiers::CONTROL),
            FocusContext::Dialog,
        );
        assert_ne!(
            action,
            Action::PreviousTab,
            "Ctrl-A must not switch tabs while a dialog is open"
        );
    }

    // ── Ctrl-O / workflow strip overview ───────────────────────────────────

    #[test]
    fn ctrl_o_toggles_workflow_strip_in_all_non_dialog_contexts() {
        for ctx in [
            FocusContext::CommandBox,
            FocusContext::ExecutionWindow,
            FocusContext::ContainerMaximized,
            FocusContext::AmieList,
        ] {
            let action = map_key(key(KeyCode::Char('o'), KeyModifiers::CONTROL), ctx);
            assert_eq!(
                action,
                Action::ToggleWorkflowStrip,
                "Ctrl-O must toggle the workflow strip in {ctx:?}"
            );
        }
    }

    #[test]
    fn ctrl_o_in_maximized_container_is_not_forwarded_to_pty() {
        let k = key(KeyCode::Char('o'), KeyModifiers::CONTROL);
        let action = map_key(k, FocusContext::ContainerMaximized);
        assert_eq!(action, Action::ToggleWorkflowStrip);
        assert_ne!(action, Action::ForwardToPty(k));
    }

    #[test]
    fn ctrl_o_suppressed_in_dialog() {
        let action = map_key(
            key(KeyCode::Char('o'), KeyModifiers::CONTROL),
            FocusContext::Dialog,
        );
        assert_ne!(
            action,
            Action::ToggleWorkflowStrip,
            "Ctrl-O must not toggle the workflow strip while a dialog is open"
        );
    }

    #[test]
    fn bare_o_in_command_box_still_types_a_character() {
        let action = map_key(
            key(KeyCode::Char('o'), KeyModifiers::NONE),
            FocusContext::CommandBox,
        );
        assert_eq!(action, Action::Char('o'));
    }

    #[test]
    fn ctrl_m_suppressed_in_dialog() {
        let action = map_key(
            key(KeyCode::Char('m'), KeyModifiers::CONTROL),
            FocusContext::Dialog,
        );
        assert_ne!(
            action,
            Action::CycleContainerWindow,
            "Ctrl-M must not cycle container window while a dialog is open"
        );
    }

    // ── Command box ───────────────────────────────────────────────────────────

    #[test]
    fn shift_tab_in_command_box_autocompletes_prev() {
        let action = map_key(
            key(KeyCode::Tab, KeyModifiers::SHIFT),
            FocusContext::CommandBox,
        );
        assert_eq!(action, Action::AutocompletePrev);
    }

    #[test]
    fn back_tab_in_command_box_autocompletes_prev() {
        let action = map_key(
            key(KeyCode::BackTab, KeyModifiers::NONE),
            FocusContext::CommandBox,
        );
        assert_eq!(action, Action::AutocompletePrev);
    }

    #[test]
    fn up_arrow_in_command_box_focuses_execution_window() {
        let action = map_key(
            key(KeyCode::Up, KeyModifiers::NONE),
            FocusContext::CommandBox,
        );
        assert_eq!(action, Action::FocusExecutionWindow);
    }

    #[test]
    fn ctrl_backspace_in_command_box_deletes_word() {
        let action = map_key(
            key(KeyCode::Backspace, KeyModifiers::CONTROL),
            FocusContext::CommandBox,
        );
        assert_eq!(action, Action::BackspaceWord);
    }

    #[test]
    fn backspace_in_command_box() {
        let action = map_key(
            key(KeyCode::Backspace, KeyModifiers::NONE),
            FocusContext::CommandBox,
        );
        assert_eq!(action, Action::Backspace);
    }

    #[test]
    fn delete_in_command_box() {
        let action = map_key(
            key(KeyCode::Delete, KeyModifiers::NONE),
            FocusContext::CommandBox,
        );
        assert_eq!(action, Action::Delete);
    }

    #[test]
    fn ctrl_left_in_command_box_moves_word_left() {
        let action = map_key(
            key(KeyCode::Left, KeyModifiers::CONTROL),
            FocusContext::CommandBox,
        );
        assert_eq!(action, Action::CursorWordLeft);
    }

    #[test]
    fn ctrl_right_in_command_box_moves_word_right() {
        let action = map_key(
            key(KeyCode::Right, KeyModifiers::CONTROL),
            FocusContext::CommandBox,
        );
        assert_eq!(action, Action::CursorWordRight);
    }

    #[test]
    fn home_in_command_box_moves_cursor_home() {
        let action = map_key(
            key(KeyCode::Home, KeyModifiers::NONE),
            FocusContext::CommandBox,
        );
        assert_eq!(action, Action::CursorHome);
    }

    #[test]
    fn end_in_command_box_moves_cursor_end() {
        let action = map_key(
            key(KeyCode::End, KeyModifiers::NONE),
            FocusContext::CommandBox,
        );
        assert_eq!(action, Action::CursorEnd);
    }

    #[test]
    fn char_in_command_box_inserts() {
        let action = map_key(
            key(KeyCode::Char('x'), KeyModifiers::NONE),
            FocusContext::CommandBox,
        );
        assert_eq!(action, Action::Char('x'));
    }

    // ── Execution window ──────────────────────────────────────────────────────

    #[test]
    fn down_arrow_in_execution_window_scrolls_down() {
        let action = map_key(
            key(KeyCode::Down, KeyModifiers::NONE),
            FocusContext::ExecutionWindow,
        );
        assert_eq!(action, Action::ScrollDown);
    }

    #[test]
    fn up_arrow_in_execution_window_scrolls_up() {
        let action = map_key(
            key(KeyCode::Up, KeyModifiers::NONE),
            FocusContext::ExecutionWindow,
        );
        assert_eq!(action, Action::ScrollUp);
    }

    #[test]
    fn page_up_in_execution_window_scrolls_page_up() {
        let action = map_key(
            key(KeyCode::PageUp, KeyModifiers::NONE),
            FocusContext::ExecutionWindow,
        );
        assert_eq!(action, Action::ScrollPageUp);
    }

    #[test]
    fn page_down_in_execution_window_scrolls_page_down() {
        let action = map_key(
            key(KeyCode::PageDown, KeyModifiers::NONE),
            FocusContext::ExecutionWindow,
        );
        assert_eq!(action, Action::ScrollPageDown);
    }

    #[test]
    fn e_in_execution_window_scrolls_to_bottom() {
        let action = map_key(
            key(KeyCode::Char('e'), KeyModifiers::NONE),
            FocusContext::ExecutionWindow,
        );
        assert_eq!(action, Action::ScrollToBottom);
    }

    #[test]
    fn l_in_execution_window_toggles_status_log() {
        let action = map_key(
            key(KeyCode::Char('l'), KeyModifiers::NONE),
            FocusContext::ExecutionWindow,
        );
        assert_eq!(action, Action::ToggleStatusLog);
    }

    #[test]
    fn ctrl_y_in_command_box_copies_selection() {
        let action = map_key(
            key(KeyCode::Char('y'), KeyModifiers::CONTROL),
            FocusContext::CommandBox,
        );
        assert_eq!(action, Action::CopySelection);
    }

    #[test]
    fn ctrl_y_in_execution_window_copies_selection() {
        let action = map_key(
            key(KeyCode::Char('y'), KeyModifiers::CONTROL),
            FocusContext::ExecutionWindow,
        );
        assert_eq!(action, Action::CopySelection);
    }

    // ── Dialog context ────────────────────────────────────────────────────────

    #[test]
    fn delete_in_dialog_maps_to_delete() {
        let action = map_key(
            key(KeyCode::Delete, KeyModifiers::NONE),
            FocusContext::Dialog,
        );
        assert_eq!(action, Action::Delete);
    }

    #[test]
    fn home_in_dialog_maps_to_cursor_home() {
        let action = map_key(key(KeyCode::Home, KeyModifiers::NONE), FocusContext::Dialog);
        assert_eq!(action, Action::CursorHome);
    }

    #[test]
    fn end_in_dialog_maps_to_cursor_end() {
        let action = map_key(key(KeyCode::End, KeyModifiers::NONE), FocusContext::Dialog);
        assert_eq!(action, Action::CursorEnd);
    }

    #[test]
    fn up_in_dialog_maps_to_scroll_up() {
        let action = map_key(key(KeyCode::Up, KeyModifiers::NONE), FocusContext::Dialog);
        assert_eq!(action, Action::ScrollUp);
    }

    #[test]
    fn ctrl_n_in_dialog_maps_to_new_map_entry() {
        let action = map_key(
            key(KeyCode::Char('n'), KeyModifiers::CONTROL),
            FocusContext::Dialog,
        );
        assert_eq!(action, Action::NewMapEntry);
    }

    #[test]
    fn page_keys_in_dialog_map_to_page_scroll() {
        let action = map_key(
            key(KeyCode::PageUp, KeyModifiers::NONE),
            FocusContext::Dialog,
        );
        assert_eq!(action, Action::ScrollPageUp);
        let action = map_key(
            key(KeyCode::PageDown, KeyModifiers::NONE),
            FocusContext::Dialog,
        );
        assert_eq!(action, Action::ScrollPageDown);
    }

    #[test]
    fn ctrl_chords_in_dialog_do_not_insert_literal_chars() {
        // A Ctrl chord must never leak its letter into an inline editor.
        let action = map_key(
            key(KeyCode::Char('x'), KeyModifiers::CONTROL),
            FocusContext::Dialog,
        );
        assert_ne!(action, Action::Char('x'));
    }

    // ── ContainerMaximized context ────────────────────────────────────────────

    #[test]
    fn ctrl_y_in_maximized_container_copies_selection() {
        let action = map_key(
            key(KeyCode::Char('y'), KeyModifiers::CONTROL),
            FocusContext::ContainerMaximized,
        );
        assert_eq!(action, Action::CopySelection);
    }

    #[test]
    fn ctrl_m_in_maximized_container_cycles_window() {
        let action = map_key(
            key(KeyCode::Char('m'), KeyModifiers::CONTROL),
            FocusContext::ContainerMaximized,
        );
        assert_eq!(action, Action::CycleContainerWindow);
    }

    #[test]
    fn regular_key_in_maximized_container_forwards_to_pty() {
        let k = key(KeyCode::Char('q'), KeyModifiers::NONE);
        let action = map_key(k, FocusContext::ContainerMaximized);
        assert_eq!(action, Action::ForwardToPty(k));
    }

    #[test]
    fn global_ctrl_t_available_in_maximized_container() {
        let k = key(KeyCode::Char('t'), KeyModifiers::CONTROL);
        let action = map_key(k, FocusContext::ContainerMaximized);
        assert_eq!(action, Action::OpenNewTabDialog);
    }
}
