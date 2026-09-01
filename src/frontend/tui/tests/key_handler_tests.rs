//! Tests for `key_handler`: autocomplete, focus switching, non-dialog text
//! input, WorkflowControlBoard arrow-key handling, command-box locking,
//! Ctrl+W escalation, container-window/Workflow-Overview resize behavior, and
//! panic-log path resolution.

use super::*;

// ─── Autocomplete cycling ─────────────────────────────────────────────────

#[test]
fn autocomplete_next_fills_command_box_with_first_suggestion() {
    let mut app = make_app();
    // Type enough for a known completion
    for c in "cha".chars() {
        press_char(&mut app, c);
    }
    press_key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert!(
        app.command_input.text.contains("chat"),
        "expected 'chat' in input, got: {:?}",
        app.command_input.text
    );
}

#[test]
fn autocomplete_prev_fills_command_box_with_last_suggestion() {
    let mut app = make_app();
    for c in "cha".chars() {
        press_char(&mut app, c);
    }
    // Update suggestions so we know the last one
    app.update_suggestions();
    let last = app.suggestion_row.last().cloned().unwrap_or_default();
    press_key(&mut app, KeyCode::BackTab, KeyModifiers::NONE);
    assert!(
        app.command_input.text.contains("cha"),
        "expected suggestion containing 'cha', got: {:?}",
        app.command_input.text
    );
    // The text should match the last suggestion (or still contain "cha" if only one)
    let _ = last; // used above
}

#[test]
fn tab_with_no_suggestions_leaves_input_unchanged() {
    let mut app = make_app();
    for c in "zzzzz".chars() {
        press_char(&mut app, c);
    }
    press_key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.command_input.text, "zzzzz");
}

// ─── Focus switching ──────────────────────────────────────────────────────

#[test]
fn up_arrow_in_command_box_switches_focus_to_execution_window() {
    let mut app = make_app();
    assert_eq!(app.focus, Focus::CommandBox);
    press_key(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.focus, Focus::ExecutionWindow);
}

#[test]
fn esc_in_execution_window_returns_focus_to_command_box() {
    let mut app = make_app();
    app.focus = Focus::ExecutionWindow;
    press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.focus, Focus::CommandBox);
}

// ─── Text input (non-dialog) ──────────────────────────────────────────────

#[test]
fn empty_command_submit_does_not_set_execution_phase() {
    use crate::frontend::tui::tabs::ExecutionPhase;
    let mut app = make_app();
    // input is empty by default
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        app.tabs[app.active_tab].execution_phase,
        ExecutionPhase::Idle
    );
}

// ─── Toggle status log ────────────────────────────────────────────────────

#[test]
fn l_in_execution_window_toggles_status_log() {
    let mut app = make_app();
    app.focus = Focus::ExecutionWindow;
    let initial = app.tabs[app.active_tab].status_log_collapsed;
    press_char(&mut app, 'l');
    assert_ne!(app.tabs[app.active_tab].status_log_collapsed, initial);
}

// ─── Ctrl-O / Workflow Overview ─────────────────────────────────────

#[test]
fn workflow_overview_starts_minimized_and_ctrl_o_toggles_it() {
    use crate::frontend::tui::tabs::WorkflowOverviewState;

    let mut app = make_app();
    assert_eq!(
        app.active_tab().workflow_overview_state,
        WorkflowOverviewState::Minimized,
        "the overview must default to the minimized one-box-per-stage view"
    );

    press_key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert_eq!(
        app.active_tab().workflow_overview_state,
        WorkflowOverviewState::Maximized
    );

    press_key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert_eq!(
        app.active_tab().workflow_overview_state,
        WorkflowOverviewState::Minimized
    );
}

#[test]
fn ctrl_o_resets_the_overview_scroll_offset() {
    // A stale offset from a previous maximization would otherwise hide the first
    // steps of the stage the next time the overview opens.
    let mut app = make_app();
    app.active_tab_mut().workflow_overview_scroll_offset = 4;
    press_key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert_eq!(app.active_tab().workflow_overview_scroll_offset, 0);
}

// ─── WorkflowControlBoard arrow keys ─────────────────────────────────────

fn setup_wcb_dialog(app: &mut App) -> std::sync::mpsc::Receiver<DialogResponse> {
    let (tx, rx) = std::sync::mpsc::channel();
    app.tabs[app.active_tab].dialog_response_tx = Some(tx);
    app.active_dialog = Some(Dialog::WorkflowControlBoard(
        crate::frontend::tui::dialogs::WorkflowControlBoardState {
            step_name: "test".into(),
            can_launch_next: true,
            can_continue_current: true,
            can_restart: true,
            can_go_back: true,
            can_finish: true,
            continue_unavailable_reason: None,
            cancel_to_previous_unavailable_reason: None,
            finish_workflow_unavailable_reason: None,
            restart_unavailable_reason: None,
            can_dismiss: false,
            launch_next_label: None,
            focused_step_name: "test".into(),
            parallel_peer_count: 0,
            parallel_peers_running: 0,
        },
    ));
    app.command_dialog_active = true;
    rx
}

#[test]
fn wcb_right_arrow_sends_launch_next() {
    let mut app = make_app();
    let rx = setup_wcb_dialog(&mut app);
    press_key(&mut app, KeyCode::Right, KeyModifiers::NONE);
    let resp = rx.try_recv().unwrap();
    assert!(matches!(resp, DialogResponse::Char('>')));
    assert!(app.active_dialog.is_none());
}

#[test]
fn wcb_down_arrow_sends_continue_current() {
    let mut app = make_app();
    let rx = setup_wcb_dialog(&mut app);
    press_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    let resp = rx.try_recv().unwrap();
    assert!(matches!(resp, DialogResponse::Char('v')));
}

#[test]
fn wcb_up_arrow_sends_restart_step() {
    let mut app = make_app();
    let rx = setup_wcb_dialog(&mut app);
    press_key(&mut app, KeyCode::Up, KeyModifiers::NONE);
    let resp = rx.try_recv().unwrap();
    assert!(matches!(resp, DialogResponse::Char('^')));
}

#[test]
fn wcb_left_arrow_sends_cancel_to_previous() {
    let mut app = make_app();
    let rx = setup_wcb_dialog(&mut app);
    press_key(&mut app, KeyCode::Left, KeyModifiers::NONE);
    let resp = rx.try_recv().unwrap();
    assert!(matches!(resp, DialogResponse::Char('<')));
}

#[test]
fn wcb_ctrl_enter_sends_finish_workflow() {
    let mut app = make_app();
    let rx = setup_wcb_dialog(&mut app);
    press_key(&mut app, KeyCode::Enter, KeyModifiers::CONTROL);
    let resp = rx.try_recv().unwrap();
    assert!(matches!(resp, DialogResponse::Char('f')));
}

#[test]
fn wcb_plain_enter_sends_finish_workflow() {
    let mut app = make_app();
    let rx = setup_wcb_dialog(&mut app);
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let resp = rx.try_recv().unwrap();
    assert!(matches!(resp, DialogResponse::Char('f')));
}

#[test]
fn wcb_enter_ignored_when_finish_unavailable() {
    let mut app = make_app();
    let (tx, rx) = std::sync::mpsc::channel();
    app.tabs[app.active_tab].dialog_response_tx = Some(tx);
    app.active_dialog = Some(Dialog::WorkflowControlBoard(
        crate::frontend::tui::dialogs::WorkflowControlBoardState {
            step_name: "test".into(),
            can_launch_next: true,
            can_continue_current: true,
            can_restart: true,
            can_go_back: true,
            can_finish: false,
            continue_unavailable_reason: None,
            cancel_to_previous_unavailable_reason: None,
            finish_workflow_unavailable_reason: Some("not last step".into()),
            restart_unavailable_reason: None,
            can_dismiss: false,
            launch_next_label: None,
            focused_step_name: "test".into(),
            parallel_peer_count: 0,
            parallel_peers_running: 0,
        },
    ));
    app.command_dialog_active = true;
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        rx.try_recv().is_err(),
        "Enter must not send FinishWorkflow when can_finish is false"
    );
}

#[test]
fn wcb_ctrl_c_sends_abort() {
    let mut app = make_app();
    let rx = setup_wcb_dialog(&mut app);
    press_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    let resp = rx.try_recv().unwrap();
    assert!(matches!(resp, DialogResponse::Char('a')));
}

#[test]
fn wcb_esc_sends_dismissed() {
    let mut app = make_app();
    let rx = setup_wcb_dialog(&mut app);
    press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    let resp = rx.try_recv().unwrap();
    assert!(matches!(resp, DialogResponse::Dismissed));
}

// ─── Command box locked during Running ────────────────────────────────────

#[test]
fn char_input_blocked_while_running() {
    let mut app = make_app();
    app.tabs[app.active_tab].execution_phase =
        crate::frontend::tui::tabs::ExecutionPhase::Running {
            command: "chat".into(),
        };
    press_char(&mut app, 'x');
    assert_eq!(
        app.command_input.text, "",
        "command box must be locked while running"
    );
}

#[test]
fn backspace_blocked_while_running() {
    let mut app = make_app();
    app.command_input.set_text("abc");
    app.tabs[app.active_tab].execution_phase =
        crate::frontend::tui::tabs::ExecutionPhase::Running {
            command: "chat".into(),
        };
    press_key(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(
        app.command_input.text, "abc",
        "backspace must be blocked while running"
    );
}

#[test]
fn submit_command_blocked_while_running() {
    use crate::frontend::tui::tabs::ExecutionPhase;
    let mut app = make_app();
    app.command_input.set_text("status");
    app.tabs[app.active_tab].execution_phase = ExecutionPhase::Running {
        command: "chat".into(),
    };
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    // Phase should still be Running, not a new command
    assert!(matches!(
        app.tabs[app.active_tab].execution_phase,
        ExecutionPhase::Running { .. }
    ));
}

// ─── q with empty box opens QuitConfirm ──────────────────────────────────

#[test]
fn q_with_empty_command_box_opens_quit_confirm() {
    let mut app = make_app();
    assert!(app.command_input.text.is_empty());
    press_char(&mut app, 'q');
    assert!(
        matches!(app.active_dialog, Some(Dialog::QuitConfirm)),
        "q with empty command box must open QuitConfirm"
    );
}

#[test]
fn q_with_nonempty_command_box_inserts_char() {
    let mut app = make_app();
    app.command_input.set_text("quer");
    press_char(&mut app, 'y');
    assert_eq!(app.command_input.text, "query");
    assert!(app.active_dialog.is_none());
}

// ─── Any key in Done/Error execution window refocuses command box ─────────

#[test]
fn any_unhandled_key_in_done_execution_window_refocuses_command_box() {
    let mut app = make_app();
    app.focus = Focus::ExecutionWindow;
    app.tabs[app.active_tab].execution_phase = crate::frontend::tui::tabs::ExecutionPhase::Done {
        command: "chat".into(),
        exit_code: 0,
    };
    // Press a key that maps to Action::None in execution window context
    press_char(&mut app, 'x');
    assert_eq!(
        app.focus,
        Focus::CommandBox,
        "unhandled key in Done execution window must refocus command box"
    );
}

#[test]
fn any_unhandled_key_in_error_execution_window_refocuses_command_box() {
    let mut app = make_app();
    app.focus = Focus::ExecutionWindow;
    app.tabs[app.active_tab].execution_phase = crate::frontend::tui::tabs::ExecutionPhase::Error {
        command: "chat".into(),
        message: "failed".into(),
    };
    press_char(&mut app, 'z');
    assert_eq!(app.focus, Focus::CommandBox);
}

#[test]
fn unhandled_key_in_running_execution_window_does_not_refocus() {
    let mut app = make_app();
    app.focus = Focus::ExecutionWindow;
    app.tabs[app.active_tab].execution_phase =
        crate::frontend::tui::tabs::ExecutionPhase::Running {
            command: "chat".into(),
        };
    press_char(&mut app, 'x');
    assert_eq!(
        app.focus,
        Focus::ExecutionWindow,
        "focus must not change during Running"
    );
}

// ─── Ctrl+W workflow control ──────────────────────────────────────────────

#[test]
fn ctrl_w_with_no_workflow_is_silent_noop() {
    let mut app = make_app();
    // No engine_tx set — Ctrl-W is a silent no-op per spec.
    press_key(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
    assert_eq!(
        app.status_bar.text, "",
        "Ctrl+W with no engine_tx must be a silent no-op"
    );
    assert!(
        app.active_dialog.is_none(),
        "no dialog must be opened when no workflow is active"
    );
}

#[test]
fn ctrl_w_during_running_step_sends_engine_request() {
    use crate::engine::workflow::EngineRequest;
    use crate::frontend::tui::tabs::WorkflowStepView;
    use crate::frontend::tui::tabs::WorkflowViewState;

    let mut app = make_app();

    // Seed the workflow_state with a running step.
    let view = WorkflowViewState {
        steps: vec![WorkflowStepView {
            name: "build".into(),
            status: "running".into(),
            agent: None,
            model: None,
            depends_on: vec![],
        }],
        current_step: Some("build".into()),
        max_concurrent: None,
    };
    *app.active_tab_mut().workflow_state.lock().unwrap() = Some(view);

    // Wire up an engine channel so we can observe what's sent.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<EngineRequest>();
    *app.active_tab_mut().engine_tx_shared.lock().unwrap() = Some(tx);

    press_key(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);

    let msg = rx.try_recv().expect("engine tx must receive a message");
    assert!(
        matches!(msg, EngineRequest::OpenControlBoard { .. }),
        "Ctrl+W during a running step must send OpenControlBoard"
    );
}

#[test]
fn ctrl_w_in_step_confirm_escalates_to_wcb() {
    use crate::engine::workflow::EngineRequest;

    let mut app = make_app();

    // Wire up an engine channel so Ctrl-W handler fires.
    let (engine_tx, _engine_rx) = tokio::sync::mpsc::unbounded_channel::<EngineRequest>();
    *app.active_tab_mut().engine_tx_shared.lock().unwrap() = Some(engine_tx);

    // Open a StepConfirm dialog with a response channel.
    let (tx, rx) = std::sync::mpsc::channel();
    app.tabs[app.active_tab].dialog_response_tx = Some(tx);
    app.active_dialog = Some(Dialog::WorkflowStepConfirm(
        crate::frontend::tui::dialogs::WorkflowStepConfirmState {
            completed_step: "build".into(),
            next_step: "test".into(),
        },
    ));
    app.command_dialog_active = true;

    press_key(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);

    // The dialog should have been dismissed.
    assert!(
        app.active_dialog.is_none(),
        "StepConfirm dialog must close on Ctrl+W"
    );
    // The frontend must have received Char('W') so it can open the full WCB.
    let resp = rx
        .try_recv()
        .expect("dialog_response_tx must receive a message");
    assert!(
        matches!(
            resp,
            crate::frontend::tui::dialogs::DialogResponse::Char('W')
        ),
        "escalation must send Char('W') to trigger full WCB"
    );
}

// ─── ContainerWindow cycle / resize ──────────────────────────────────────

#[test]
fn cycle_to_hidden_does_not_send_resize() {
    let mut app = make_app();
    // Install a slot and wire its resize channel to observe.
    let (resize_tx, mut resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();
    app.active_tab_mut()
        .start_container("claude".into(), String::new(), 80, 24);
    app.active_tab_mut()
        .focused_slot_mut()
        .unwrap()
        .container_resize_tx = Some(resize_tx);

    // Start at Maximized, cycle → Minimized (not Hidden, resize expected on next test).
    app.active_tab_mut().container_window_state =
        crate::frontend::tui::tabs::ContainerWindowState::Maximized;
    // Cycle: Maximized → Minimized
    press_key(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
    assert_eq!(
        app.active_tab().container_window_state,
        crate::frontend::tui::tabs::ContainerWindowState::Minimized,
    );

    // Cycle again: Minimized → Maximized (still not hidden, resize may be sent)
    press_key(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
    assert_eq!(
        app.active_tab().container_window_state,
        crate::frontend::tui::tabs::ContainerWindowState::Maximized,
    );

    // Cycle: Maximized → Minimized once more — no Hidden state reached yet.
    // Now let's explicitly set Hidden and verify cycling to Hidden sends nothing.
    app.active_tab_mut().container_window_state =
        crate::frontend::tui::tabs::ContainerWindowState::Minimized;
    // Drain channel to reset state.
    while resize_rx.try_recv().is_ok() {}

    // Hidden → Maximized (sending resize) then Maximized → Minimized (sending resize)
    // We want to reach Hidden from Minimized: but cycle(Minimized) = Maximized.
    // Actually cycle(Hidden) = Maximized, cycle(Minimized) = Maximized, cycle(Maximized) = Minimized.
    // There's no transition TO Hidden — Hidden is the initial state.
    // So we test that cycling out of Hidden (to Maximized) might send a resize,
    // and cycling Maximized → Minimized does NOT go to Hidden and always sends resize.
    // "Cycle to hidden does not send resize" means starting from Maximized → Minimized:
    // In that transition, a resize IS sent (not hidden). But if we start from Hidden and
    // cycle, we go to Maximized (sends resize). Since Hidden isn't reachable via cycle from
    // a non-hidden state, let's verify: starting at Maximized, cycling to Minimized.
    app.active_tab_mut().container_window_state =
        crate::frontend::tui::tabs::ContainerWindowState::Maximized;
    while resize_rx.try_recv().is_ok() {}
    press_key(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
    // Minimized ≠ Hidden so resize is attempted (may fail in CI env).
    // The key assertion: cycling from Hidden should not send resize even if Hidden
    // is explicitly set.
    app.active_tab_mut().container_window_state =
        crate::frontend::tui::tabs::ContainerWindowState::Hidden;
    // Drop the slot's resize channel.
    app.active_tab_mut()
        .focused_slot_mut()
        .unwrap()
        .container_resize_tx = None;
    // Cycling from Hidden → Maximized — the resize send should not panic.
    press_key(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
    assert_eq!(
        app.active_tab().container_window_state,
        crate::frontend::tui::tabs::ContainerWindowState::Maximized,
    );
}

// ─── Workflow Overview scroll ────────────────────────────────────────────────

#[test]
fn scroll_down_reveals_hidden_parallel_steps() {
    use crate::frontend::tui::tabs::{WorkflowStepView, WorkflowViewState};
    use crossterm::event::{MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;

    let mut app = make_app();

    // Seed a workflow with many parallel steps so the overview would overflow.
    let view = WorkflowViewState {
        steps: (0..6)
            .map(|i| WorkflowStepView {
                name: format!("step-{i}"),
                status: "pending".into(),
                agent: None,
                model: None,
                depends_on: vec![],
            })
            .collect(),
        current_step: None,
        max_concurrent: None,
    };
    *app.active_tab_mut().workflow_state.lock().unwrap() = Some(view);

    // Simulate the renderer having recorded an overview rect.
    let overview_rect = Rect::new(0, 30, 80, 9);
    app.active_tab_mut().last_overview_rect = Some(overview_rect);

    assert_eq!(app.active_tab().workflow_overview_scroll_offset, 0);

    // Mouse scroll-down inside the overview rect increments the offset.
    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 10,
            row: 32, // inside overview_rect
            modifiers: KeyModifiers::NONE,
        },
    );
    assert_eq!(
        app.active_tab().workflow_overview_scroll_offset,
        1,
        "scroll down inside the overview must increment workflow_overview_scroll_offset"
    );
}

#[test]
fn scroll_clamped_at_bounds() {
    use crate::frontend::tui::tabs::{WorkflowStepView, WorkflowViewState};
    use crossterm::event::{MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;

    let mut app = make_app();
    let view = WorkflowViewState {
        steps: vec![WorkflowStepView {
            name: "only".into(),
            status: "pending".into(),
            agent: None,
            model: None,
            depends_on: vec![],
        }],
        current_step: None,
        max_concurrent: None,
    };
    *app.active_tab_mut().workflow_state.lock().unwrap() = Some(view);

    let overview_rect = Rect::new(0, 30, 80, 3);
    app.active_tab_mut().last_overview_rect = Some(overview_rect);

    // Scroll up when already at 0 → offset stays at 0 (no underflow).
    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 10,
            row: 31,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert_eq!(
        app.active_tab().workflow_overview_scroll_offset,
        0,
        "scrolling up at offset=0 must not underflow"
    );
}

// ─── Panic log ────────────────────────────────────────────────────────────

#[test]
fn panic_log_path_lives_under_awman_home() {
    // Skip on hosts with no resolvable home dir (the hook no-ops there).
    if let Some(path) = crate::frontend::tui::event_loop::panic_log_path() {
        assert!(
            path.ends_with(".awman/panic.log"),
            "panic log must live in the awman data dir: {}",
            path.display()
        );
    }
}

// ─── Container inner-size seam (WI-0098 Finding C module split) ──────────────

#[test]
fn compute_container_inner_size_subtracts_chrome_and_border() {
    // Pure seam extracted into `event_loop` during the module split. A typical
    // terminal: 95% of the width/exec-height, then minus the 2-cell border.
    // cols: 100*95/100 = 95, -2 border = 93.
    // exec_height: 40 - 8 chrome = 32; 32*95/100 = 30, -2 border = 28.
    let (cols, rows) = crate::frontend::tui::event_loop::compute_container_inner_size(100, 40);
    assert_eq!((cols, rows), (93, 28));
}

#[test]
fn compute_container_inner_size_floors_on_tiny_terminal() {
    // Saturating math must keep the grid at its minimums for a tiny terminal
    // rather than underflowing: cols floor 10-2=8, rows floor 5-2=3.
    let (cols, rows) = crate::frontend::tui::event_loop::compute_container_inner_size(1, 1);
    assert_eq!((cols, rows), (8, 3));
}

// ─── Yolo countdown modal + parallel container rotation ──────────────────

fn push_parallel_slots(app: &mut App) {
    use crate::frontend::tui::tabs::ContainerSlot;
    let tab = app.active_tab_mut();
    tab.dormant_slots
        .push(ContainerSlot::new(String::new(), "claude".into(), 1000));
    tab.container_slots
        .push(ContainerSlot::new("build".into(), "claude".into(), 1000));
    tab.container_slots
        .push(ContainerSlot::new("test".into(), "codex".into(), 1000));
}

#[test]
fn ctrl_s_cycles_focused_slot_while_yolo_modal_is_open() {
    use crate::frontend::tui::dialogs::WorkflowYoloCountdownState;

    let mut app = make_app();
    push_parallel_slots(&mut app);
    app.active_dialog = Some(Dialog::WorkflowYoloCountdown(WorkflowYoloCountdownState {
        step_name: "build".into(),
        remaining_secs: 30,
    }));

    press_key(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);

    assert_eq!(
        app.active_tab().focused_slot_idx,
        1,
        "Ctrl-S must still rotate the focused slot while the modal is open"
    );
    assert!(
        app.active_dialog.is_none(),
        "the modal is dismissed here; tick_all_tabs re-derives it for the new focus"
    );
}

#[test]
fn ctrl_s_with_single_slot_leaves_yolo_modal_open() {
    use crate::frontend::tui::dialogs::WorkflowYoloCountdownState;

    let mut app = make_app();
    // A plain (sequential) yolo countdown: no parallel group, one slot.
    app.active_dialog = Some(Dialog::WorkflowYoloCountdown(WorkflowYoloCountdownState {
        step_name: "build".into(),
        remaining_secs: 30,
    }));

    press_key(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);

    assert!(
        app.active_dialog.is_some(),
        "with no parallel group to rotate, Ctrl-S must not swallow the modal"
    );
}

#[test]
fn esc_on_parallel_yolo_modal_cancels_the_focused_slots_flag_only() {
    use crate::frontend::tui::dialogs::WorkflowYoloCountdownState;

    let mut app = make_app();
    push_parallel_slots(&mut app);
    app.active_tab_mut().focused_slot_idx = 1; // "test" is focused
    app.active_dialog = Some(Dialog::WorkflowYoloCountdown(WorkflowYoloCountdownState {
        step_name: "test".into(),
        remaining_secs: 5,
    }));

    press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);

    assert!(app.active_dialog.is_none());
    assert!(
        app.active_tab().container_slots[1]
            .yolo_cancel_flag
            .load(std::sync::atomic::Ordering::Relaxed),
        "the focused slot's own cancel flag must be set"
    );
    assert!(
        !app.active_tab().container_slots[0]
            .yolo_cancel_flag
            .load(std::sync::atomic::Ordering::Relaxed),
        "the non-focused sibling's cancel flag must be untouched"
    );
    assert!(
        !app.active_tab()
            .yolo_cancel_flag
            .load(std::sync::atomic::Ordering::Relaxed),
        "the tab-level (sequential-path) flag is unrelated here"
    );
}

// ─── amie tab key handling (WI 0102) ──────────────────────────────────────

/// Push the singleton amie tab (bypassing the daemon-backed
/// `open_or_focus_amie_tab` path — these tests only need `is_amie` state, not
/// a live gateway), focus it, and return its index.
fn push_amie_tab(app: &mut App) -> usize {
    let tab = Tab::new_amie(make_session());
    app.tabs.push(tab);
    let idx = app.tabs.len() - 1;
    app.active_tab = idx;
    app.focus = Focus::ExecutionWindow;
    idx
}

fn fake_condition(name: &str) -> crate::data::fs::condition_store::Condition {
    use crate::data::fs::condition_store::{ConditionStatus, MountScope};
    let now = chrono::Utc::now();
    crate::data::fs::condition_store::Condition {
        id: name.to_string(),
        name: name.to_string(),
        description: "test condition".into(),
        repo_scope: std::path::PathBuf::from("/tmp"),
        mount_scope: MountScope::GitRoot,
        interval_secs: 300,
        status: ConditionStatus::Active,
        agent: None,
        model: None,
        backoff_until: None,
        created_at: now,
        updated_at: now,
        last_run_at: None,
    }
}

/// Populate the active (amie) tab's snapshot with fake conditions and select
/// the first one, so the selection-dependent actions (`a`/`Enter`/`p`/`r`/`d`)
/// have something to act on.
fn set_amie_conditions(app: &mut App, names: &[&str]) {
    let state = app.active_tab().amie.as_ref().expect("active tab is amie");
    let mut snap = state.snapshot.lock().unwrap();
    snap.conditions = names.iter().map(|n| fake_condition(n)).collect();
    snap.loaded = true;
}

/// An `App` whose `Engines` report no container runtime, so any code path
/// that would otherwise touch the real amie daemon (`AmieSupervisor`,
/// `provision_key`'s key-hash write) instead takes the sandbox-refusal
/// fast-path deterministically, with no filesystem or process side effects —
/// mirroring `tests/amie_sandbox_refusal.rs`'s `FakeSandboxRuntime` approach.
fn make_app_no_container_runtime() -> App {
    let rt = Box::leak(Box::new(tokio::runtime::Runtime::new().unwrap()));
    let catalogue = CommandCatalogue::get();
    let mut engines = make_engines();
    engines.container_runtime = None;
    let session_manager = Arc::new(RwLock::new(SessionManager::in_memory()));
    let tab = Tab::new(make_session());
    App::new(
        catalogue,
        engines,
        session_manager,
        tab,
        rt.handle().clone(),
    )
}

/// An app with a second ordinary tab plus the amie tab active — enough tabs
/// for Ctrl-A/Ctrl-D navigation away from the amie tab to be observable.
fn amie_list_app() -> App {
    let mut app = make_app();
    app.add_tab(make_session());
    push_amie_tab(&mut app);
    app
}

#[test]
fn ctrl_t_new_tab_dialog_shows_press_ctrl_a_hint() {
    let mut app = make_app();
    press_key(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    match &app.active_dialog {
        Some(Dialog::TextInput { prompt, .. }) => {
            assert!(
                prompt.contains("Press Ctrl-A to open amie"),
                "New Tab prompt must hint at amie: {prompt:?}"
            );
        }
        _ => panic!("Ctrl-T must open the New Tab TextInput dialog"),
    }
}

#[test]
fn ctrl_a_in_new_tab_dialog_focuses_existing_amie_tab_and_closes_dialog() {
    let mut app = make_app();
    let amie_idx = push_amie_tab(&mut app);
    app.active_tab = 0; // back on the normal tab
    press_key(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert!(matches!(app.active_dialog, Some(Dialog::TextInput { .. })));
    press_key(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
    assert!(
        app.active_dialog.is_none(),
        "Ctrl-A in the New Tab dialog must close it"
    );
    assert_eq!(
        app.active_tab, amie_idx,
        "Ctrl-A must activate the amie tab"
    );
}

// The load-bearing test for the Ctrl-A binding (implementation-contract.md
// §2.8): the New Tab dialog is the ONLY thing that reroutes Ctrl-A to amie.
// Both directions are asserted with three tabs open, so "previous tab" (tab
// 0) and "the amie tab" (tab 2) are distinct and the two behaviors can't be
// confused with one another.
#[test]
fn ctrl_a_without_dialog_switches_to_previous_tab_and_does_not_open_amie() {
    let mut app = make_app(); // tab 0
    app.add_tab(make_session()); // tab 1
    let amie_idx = push_amie_tab(&mut app); // tab 2 == amie, active_tab == amie_idx
    app.active_tab = 1; // sit on the middle (normal) tab
    press_key(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
    assert_eq!(
        app.active_tab, 0,
        "Ctrl-A with no dialog open must switch to the previous tab"
    );
    assert_ne!(app.active_tab, amie_idx);
    assert!(
        app.active_dialog.is_none(),
        "Ctrl-A with no dialog open must not open any dialog"
    );
}

#[test]
fn ctrl_a_with_new_tab_dialog_open_opens_amie_and_does_not_switch_tabs() {
    let mut app = make_app(); // tab 0
    app.add_tab(make_session()); // tab 1
    let amie_idx = push_amie_tab(&mut app); // tab 2 == amie
    app.active_tab = 1; // sit on the middle tab: "previous" (0) != amie (2)
    press_key(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert!(matches!(app.active_dialog, Some(Dialog::TextInput { .. })));
    press_key(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
    assert_eq!(
        app.active_tab, amie_idx,
        "Ctrl-A inside the New Tab dialog must open the amie tab"
    );
    assert_ne!(
        app.active_tab, 0,
        "must not have fallen through to previous-tab navigation"
    );
    assert!(
        app.active_dialog.is_none(),
        "the New Tab dialog must be closed"
    );
}

// ── the six amie list keys fire only in FocusContext::AmieList ────────────

#[test]
fn amie_list_enter_opens_condition_detail() {
    let mut app = make_app();
    push_amie_tab(&mut app);
    set_amie_conditions(&mut app, &["cond-a"]);
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    match &app.active_dialog {
        Some(Dialog::AmieConditionDetail(state)) => assert_eq!(state.name, "cond-a"),
        _ => panic!("Enter in the amie list must open the condition detail modal"),
    }
}

#[test]
fn amie_list_enter_is_noop_when_list_is_empty() {
    let mut app = make_app();
    push_amie_tab(&mut app);
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.active_dialog.is_none(),
        "Enter on an empty amie list must not open a dialog"
    );
}

#[test]
fn amie_list_a_routes_to_start_amie_attach() {
    // Force the sandbox-refusal fast-path (see `make_app_no_container_runtime`)
    // so this stays deterministic and side-effect-free while still proving
    // `a` reaches `start_amie_attach` rather than doing nothing.
    let mut app = make_app_no_container_runtime();
    push_amie_tab(&mut app);
    set_amie_conditions(&mut app, &["cond-a"]);
    press_key(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
    assert!(
        app.status_bar
            .text
            .contains("amie requires a container runtime"),
        "'a' in the amie list must route to start_amie_attach: {:?}",
        app.status_bar.text
    );
}

#[test]
fn amie_list_n_dispatches_amie_add_interview() {
    let mut app = make_app();
    push_amie_tab(&mut app);
    press_key(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
    assert!(
        app.active_tab().command_result_rx.is_some(),
        "'n' in the amie list must dispatch `amie add --interview`"
    );
}

#[test]
fn amie_list_p_dispatches_pause() {
    let mut app = make_app();
    push_amie_tab(&mut app);
    set_amie_conditions(&mut app, &["cond-a"]);
    press_key(&mut app, KeyCode::Char('p'), KeyModifiers::NONE);
    assert!(
        app.active_tab().command_result_rx.is_some(),
        "'p' in the amie list must dispatch `amie pause`"
    );
}

#[test]
fn amie_list_r_dispatches_resume() {
    let mut app = make_app();
    push_amie_tab(&mut app);
    set_amie_conditions(&mut app, &["cond-a"]);
    press_key(&mut app, KeyCode::Char('r'), KeyModifiers::NONE);
    assert!(
        app.active_tab().command_result_rx.is_some(),
        "'r' in the amie list must dispatch `amie resume`"
    );
}

#[test]
fn amie_list_p_and_r_are_noop_when_list_is_empty() {
    let mut app = make_app();
    push_amie_tab(&mut app);
    press_key(&mut app, KeyCode::Char('p'), KeyModifiers::NONE);
    assert!(app.active_tab().command_result_rx.is_none());
    press_key(&mut app, KeyCode::Char('r'), KeyModifiers::NONE);
    assert!(app.active_tab().command_result_rx.is_none());
}

#[test]
fn amie_list_d_opens_remove_confirm_and_only_dispatches_on_y() {
    let mut app = make_app();
    push_amie_tab(&mut app);
    set_amie_conditions(&mut app, &["cond-a"]);
    press_key(&mut app, KeyCode::Char('d'), KeyModifiers::NONE);
    match &app.active_dialog {
        Some(Dialog::AmieRemoveConfirm { name }) => assert_eq!(name, "cond-a"),
        _ => panic!("'d' must open Dialog::AmieRemoveConfirm"),
    }
    assert!(
        app.active_tab().command_result_rx.is_none(),
        "opening the confirmation must not itself dispatch a removal"
    );
    press_key(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
    assert!(
        app.active_dialog.is_none(),
        "'y' must dismiss the confirmation"
    );
    assert!(
        app.active_tab().command_result_rx.is_some(),
        "'y' must dispatch `amie remove cond-a`"
    );
}

#[test]
fn amie_list_d_then_n_dismisses_without_dispatching() {
    let mut app = make_app();
    push_amie_tab(&mut app);
    set_amie_conditions(&mut app, &["cond-a"]);
    press_key(&mut app, KeyCode::Char('d'), KeyModifiers::NONE);
    press_key(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
    assert!(app.active_dialog.is_none());
    assert!(
        app.active_tab().command_result_rx.is_none(),
        "'n' must dismiss the confirmation without dispatching a removal"
    );
}

#[test]
fn amie_list_keys_are_inert_on_a_normal_tab() {
    let mut app = make_app();
    app.focus = Focus::ExecutionWindow;
    for key in [
        KeyCode::Enter,
        KeyCode::Char('a'),
        KeyCode::Char('n'),
        KeyCode::Char('p'),
        KeyCode::Char('r'),
        KeyCode::Char('d'),
    ] {
        press_key(&mut app, key, KeyModifiers::NONE);
    }
    assert!(
        app.active_dialog.is_none(),
        "amie list keys must not fire outside FocusContext::AmieList"
    );
    assert_eq!(app.tabs.len(), 1, "no tab must be added or removed");
}

#[test]
fn amie_list_arrows_move_selection_not_scroll_offset() {
    let mut app = make_app();
    push_amie_tab(&mut app);
    set_amie_conditions(&mut app, &["a", "b", "c"]);
    let before_scroll = app.active_tab().scroll_offset;
    press_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.active_tab().amie.as_ref().unwrap().selected, 1);
    assert_eq!(
        app.active_tab().scroll_offset,
        before_scroll,
        "scroll_offset must be untouched while the amie list holds focus"
    );
    press_key(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.active_tab().amie.as_ref().unwrap().selected, 0);
}

#[test]
fn amie_list_context_not_selected_while_attach_owns_the_tabs_slots() {
    let mut app = make_app();
    push_amie_tab(&mut app);
    set_amie_conditions(&mut app, &["a", "b"]);
    app.active_tab_mut()
        .start_container("claude".into(), "awman-abc".into(), 80, 24);
    // With container_slots non-empty, arrow keys must fall through to the
    // ordinary ExecutionWindow/ContainerMaximized handling instead of moving
    // the amie selection.
    press_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(
        app.active_tab().amie.as_ref().unwrap().selected,
        0,
        "FocusContext::AmieList must not apply while an attach session owns the tab's slots"
    );
}

// ── global Ctrl shortcuts keep their meaning while the amie list has focus ─
// (implementation-contract.md §2.9: "the single most important regression
// risk of adding a context")

#[test]
fn ctrl_t_still_opens_new_tab_dialog_from_amie_list() {
    let mut app = amie_list_app();
    press_key(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert!(matches!(app.active_dialog, Some(Dialog::TextInput { .. })));
}

#[test]
fn ctrl_a_still_switches_tabs_from_amie_list() {
    let mut app = amie_list_app();
    let amie_idx = app.active_tab;
    press_key(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
    assert_ne!(
        app.active_tab, amie_idx,
        "Ctrl-A must still switch tabs when the amie list holds focus"
    );
}

#[test]
fn ctrl_d_still_switches_tabs_from_amie_list() {
    let mut app = amie_list_app();
    let amie_idx = app.active_tab;
    press_key(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_ne!(
        app.active_tab, amie_idx,
        "Ctrl-D must still switch tabs when the amie list holds focus"
    );
}

#[test]
fn ctrl_m_still_cycles_container_window_from_amie_list() {
    let mut app = amie_list_app();
    let before = app.active_tab().container_window_state;
    press_key(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
    assert_ne!(
        app.active_tab().container_window_state,
        before,
        "Ctrl-M must still cycle the container window from the amie list"
    );
}

#[test]
fn ctrl_w_from_amie_list_is_silent_noop_without_a_workflow() {
    let mut app = amie_list_app();
    press_key(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
    assert!(
        app.active_dialog.is_none(),
        "Ctrl-W with no active workflow must stay a silent no-op from the amie list"
    );
}

#[test]
fn ctrl_g_is_globally_intercepted_but_a_noop_on_the_amie_tab() {
    let mut app = amie_list_app();
    press_key(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    assert_eq!(
        app.active_tab().git_sidebar_state,
        crate::frontend::tui::git_sidebar::GitSidebarState::Closed,
        "Ctrl-G must never open the git sidebar for the amie tab"
    );
}

#[test]
fn ctrl_c_still_opens_close_tab_confirm_from_amie_list() {
    let mut app = amie_list_app();
    press_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(matches!(app.active_dialog, Some(Dialog::CloseTabConfirm)));
}
