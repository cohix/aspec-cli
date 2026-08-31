//! Tests for `mouse_handler`: scroll forwarding to the container PTY vs.
//! awman's own scrollback, and click/drag text selection in both the
//! container overlay and the execution window.

use super::*;

// ─── Mouse passthrough (WI-0088) ──────────────────────────────────────────

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

fn make_mouse_event(kind: MouseEventKind, col: u16, row: u16, mods: KeyModifiers) -> MouseEvent {
    MouseEvent {
        kind,
        column: col,
        row,
        modifiers: mods,
    }
}

/// Install a container slot, set the active tab to Maximized with a
/// known inner area, and wire the slot's PTY stdin channel; returns the
/// receiving end for assertions.
fn setup_container_tab(app: &mut App) -> tokio::sync::mpsc::UnboundedReceiver<Vec<u8>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let tab = app.active_tab_mut();
    tab.start_container("claude".into(), String::new(), 80, 24);
    tab.container_window_state = crate::frontend::tui::tabs::ContainerWindowState::Maximized;
    // inner area starting at (5, 3) with size 80×24
    tab.container_inner_area = Some(Rect::new(5, 3, 80, 24));
    tab.focused_slot_mut().unwrap().container_stdin_tx = Some(tx);
    rx
}

// Pure `encode_mouse_scroll` unit tests live in `crate::frontend::tui::mouse::tests`.

// ── Forwarding decision: mode None → awman scrollback ────────────────────

#[test]
fn scroll_with_no_mouse_mode_does_not_forward_to_pty() {
    let mut app = make_app();
    let mut rx = setup_container_tab(&mut app);
    // vt100 parser has no mouse tracking (default None)
    // coords inside inner_area = Rect::new(5, 3, 80, 24)
    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(MouseEventKind::ScrollUp, 20, 10, KeyModifiers::NONE),
    );
    assert!(
        rx.try_recv().is_err(),
        "scroll with no mouse mode must not forward to PTY"
    );
}

// ── Forwarding decision: shift held → awman scrollback (escape hatch) ────

#[test]
fn scroll_shift_held_does_not_forward_to_pty() {
    let mut app = make_app();
    let mut rx = setup_container_tab(&mut app);
    // Enable mouse tracking so agent_wants_mouse = true
    app.active_tab_mut()
        .focused_parser_mut()
        .process(b"\x1b[?1000h");
    app.active_tab_mut().container_scroll_offset = 0;

    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(MouseEventKind::ScrollUp, 20, 10, KeyModifiers::SHIFT),
    );
    assert!(
        rx.try_recv().is_err(),
        "Shift+scroll must not forward to PTY even when agent tracks mouse"
    );
}

// ── Forwarding decision: in scrollback → awman scrollback ────────────────

#[test]
fn scroll_in_scrollback_does_not_forward_to_pty() {
    let mut app = make_app();
    let mut rx = setup_container_tab(&mut app);
    app.active_tab_mut()
        .focused_parser_mut()
        .process(b"\x1b[?1000h");
    app.active_tab_mut().container_scroll_offset = 10; // user is scrolled back

    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(MouseEventKind::ScrollUp, 20, 10, KeyModifiers::NONE),
    );
    assert!(
        rx.try_recv().is_err(),
        "scroll while in scrollback must not forward to PTY"
    );
}

// ── Forwarding decision: live view + agent mouse → forward to PTY ─────────

#[test]
fn scroll_at_live_view_with_mouse_tracking_forwards_to_pty() {
    let mut app = make_app();
    let mut rx = setup_container_tab(&mut app);
    app.active_tab_mut()
        .focused_parser_mut()
        .process(b"\x1b[?1000h");
    app.active_tab_mut().container_scroll_offset = 0;

    // coords (20, 10) are inside inner_area = Rect::new(5, 3, 80, 24)
    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(MouseEventKind::ScrollUp, 20, 10, KeyModifiers::NONE),
    );
    let data = rx
        .try_recv()
        .expect("PTY must receive scroll bytes when agent wants mouse at live view");
    assert!(!data.is_empty());
}

// ── Scroll outside inner area is discarded ────────────────────────────────

#[test]
fn scroll_outside_inner_area_is_discarded() {
    let mut app = make_app();
    let mut rx = setup_container_tab(&mut app);
    app.active_tab_mut()
        .focused_parser_mut()
        .process(b"\x1b[?1000h");
    app.active_tab_mut().container_scroll_offset = 0;

    let before_offset = app.active_tab().container_scroll_offset;

    // col=0 is left of inner_area (starts at col=5) — should be discarded
    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(MouseEventKind::ScrollUp, 0, 10, KeyModifiers::NONE),
    );
    assert!(
        rx.try_recv().is_err(),
        "scroll on left border must be discarded"
    );
    assert_eq!(
        app.active_tab().container_scroll_offset,
        before_offset,
        "discarded scroll must not change container_scroll_offset"
    );

    // row=0 is above inner_area (starts at row=3) — should be discarded
    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(MouseEventKind::ScrollUp, 20, 0, KeyModifiers::NONE),
    );
    assert!(
        rx.try_recv().is_err(),
        "scroll above inner area must be discarded"
    );

    // inner_area right edge is exclusive at col 5+80=85 — must be discarded
    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(MouseEventKind::ScrollUp, 85, 10, KeyModifiers::NONE),
    );
    assert!(
        rx.try_recv().is_err(),
        "scroll on right border (col == inner.x + inner.width) must be discarded"
    );

    // inner_area bottom edge is exclusive at row 3+24=27 — must be discarded
    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(MouseEventKind::ScrollUp, 20, 27, KeyModifiers::NONE),
    );
    assert!(
        rx.try_recv().is_err(),
        "scroll on bottom border (row == inner.y + inner.height) must be discarded"
    );

    // Confirm we still forward when the event lands at the last valid
    // inside-cell — guards against off-by-one in the >= comparisons.
    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(MouseEventKind::ScrollUp, 84, 26, KeyModifiers::NONE),
    );
    assert!(
        rx.try_recv().is_ok(),
        "scroll at the last inside cell (col 84, row 26) must still be forwarded"
    );
}

// ── Alternate scroll (DECSET 1007, e.g. codex) → arrow keys to PTY ────────

#[test]
fn scroll_with_alternate_scroll_mode_forwards_arrow_keys() {
    let mut app = make_app();
    let mut rx = setup_container_tab(&mut app);
    // codex-style: alternate screen + alternate scroll, NO mouse tracking.
    let tab = app.active_tab_mut();
    tab.focused_slot_mut().unwrap().agent_alt_screen = true;
    tab.focused_slot_mut().unwrap().agent_alternate_scroll = true;
    tab.container_scroll_offset = 0;

    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(MouseEventKind::ScrollUp, 20, 10, KeyModifiers::NONE),
    );
    let data = rx
        .try_recv()
        .expect("PTY must receive arrow keys when alternate scroll is active");
    assert_eq!(data, b"\x1b[A\x1b[A\x1b[A");

    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(MouseEventKind::ScrollDown, 20, 10, KeyModifiers::NONE),
    );
    let data = rx.try_recv().expect("scroll down must also forward");
    assert_eq!(data, b"\x1b[B\x1b[B\x1b[B");
}

#[test]
fn alternate_scroll_without_alt_screen_scrolls_awman_scrollback() {
    // codex's inline (non-alt-screen) chat view: 1007 may linger from a
    // previous overlay, but with the alt screen off the wheel must go to
    // awman's own scrollback — mirroring real terminal behavior, where
    // alternate scroll only applies while the alternate screen is active.
    let mut app = make_app();
    let mut rx = setup_container_tab(&mut app);
    let tab = app.active_tab_mut();
    tab.focused_slot_mut().unwrap().agent_alt_screen = false;
    tab.focused_slot_mut().unwrap().agent_alternate_scroll = true;
    tab.container_scroll_offset = 0;

    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(MouseEventKind::ScrollUp, 20, 10, KeyModifiers::NONE),
    );
    assert!(
        rx.try_recv().is_err(),
        "alternate scroll without alt screen must not forward to PTY"
    );
}

#[test]
fn alternate_scroll_shift_held_scrolls_awman_scrollback() {
    let mut app = make_app();
    let mut rx = setup_container_tab(&mut app);
    let tab = app.active_tab_mut();
    tab.focused_slot_mut().unwrap().agent_alt_screen = true;
    tab.focused_slot_mut().unwrap().agent_alternate_scroll = true;
    tab.container_scroll_offset = 0;

    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(MouseEventKind::ScrollUp, 20, 10, KeyModifiers::SHIFT),
    );
    assert!(
        rx.try_recv().is_err(),
        "Shift+scroll escape hatch must also apply to alternate scroll"
    );
}

#[test]
fn alternate_scroll_respects_application_cursor_mode() {
    let mut app = make_app();
    let mut rx = setup_container_tab(&mut app);
    let tab = app.active_tab_mut();
    tab.focused_slot_mut().unwrap().agent_alt_screen = true;
    tab.focused_slot_mut().unwrap().agent_alternate_scroll = true;
    tab.focused_parser_mut().process(b"\x1b[?1h"); // DECCKM: application cursor keys
    tab.container_scroll_offset = 0;

    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(MouseEventKind::ScrollUp, 20, 10, KeyModifiers::NONE),
    );
    let data = rx.try_recv().expect("PTY must receive arrow keys");
    assert_eq!(data, b"\x1bOA\x1bOA\x1bOA");
}

#[test]
fn mouse_tracking_takes_precedence_over_alternate_scroll() {
    let mut app = make_app();
    let mut rx = setup_container_tab(&mut app);
    let tab = app.active_tab_mut();
    tab.focused_slot_mut().unwrap().agent_alt_screen = true;
    tab.focused_slot_mut().unwrap().agent_alternate_scroll = true;
    tab.focused_parser_mut().process(b"\x1b[?1000h"); // real mouse tracking too
    tab.container_scroll_offset = 0;

    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(MouseEventKind::ScrollUp, 20, 10, KeyModifiers::NONE),
    );
    let data = rx.try_recv().expect("PTY must receive bytes");
    assert_eq!(
        &data[..3],
        b"\x1b[M",
        "agent with real mouse tracking must get mouse encoding, not arrows"
    );
}

// ── Coordinate translation subtracts inner area origin ────────────────────

#[test]
fn scroll_coordinate_translation_subtracts_inner_origin() {
    let mut app = make_app();
    let mut rx = setup_container_tab(&mut app);
    // SGR encoding: ESC[<button;col+1;row+1M — easy to verify exact coords.
    // inner_area = Rect::new(5, 3, 80, 24)
    app.active_tab_mut()
        .focused_parser_mut()
        .process(b"\x1b[?1006h"); // SGR encoding
    app.active_tab_mut()
        .focused_parser_mut()
        .process(b"\x1b[?1000h"); // enable mouse mode
    app.active_tab_mut().container_scroll_offset = 0;

    // Terminal coords (10, 6) → vt_col = 10-5 = 5, vt_row = 6-3 = 3
    // SGR output: ESC[<64;6;4M (col+1=6, row+1=4)
    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(MouseEventKind::ScrollUp, 10, 6, KeyModifiers::NONE),
    );
    let data = rx.try_recv().expect("PTY must receive scroll bytes");
    let seq = String::from_utf8(data).unwrap();
    assert_eq!(
        seq, "\x1b[<64;6;4M",
        "SGR sequence must use translated vt coords (5,3 → col+1=6, row+1=4)"
    );
}

// ── Click/drag events are never forwarded to PTY ──────────────────────────

#[test]
fn click_down_not_forwarded_to_pty_even_when_agent_tracks_mouse() {
    let mut app = make_app();
    let mut rx = setup_container_tab(&mut app);
    app.active_tab_mut()
        .focused_parser_mut()
        .process(b"\x1b[?1000h");
    app.active_tab_mut().container_scroll_offset = 0;

    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            20,
            10,
            KeyModifiers::NONE,
        ),
    );
    assert!(
        rx.try_recv().is_err(),
        "mouse Down must never be forwarded to PTY"
    );
    // Text selection should be started instead
    assert!(
        app.active_tab().mouse_selection.is_some(),
        "mouse Down must start text selection"
    );
}

#[test]
fn click_drag_not_forwarded_to_pty_even_when_agent_tracks_mouse() {
    let mut app = make_app();
    let mut rx = setup_container_tab(&mut app);
    app.active_tab_mut()
        .focused_parser_mut()
        .process(b"\x1b[?1000h");
    app.active_tab_mut().container_scroll_offset = 0;

    // Prime the selection so Drag has something to update
    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            20,
            10,
            KeyModifiers::NONE,
        ),
    );
    let _ = rx.try_recv(); // drain any spurious data

    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            22,
            11,
            KeyModifiers::NONE,
        ),
    );
    assert!(
        rx.try_recv().is_err(),
        "mouse Drag must never be forwarded to PTY"
    );
}

// ─── Execution window text selection ──────────────────────────────────────

/// Give the active tab a published execution-window inner area and a
/// visible text grid, as the renderer would each frame.
fn setup_exec_tab(app: &mut App, state: crate::frontend::tui::tabs::ContainerWindowState) {
    let tab = app.active_tab_mut();
    tab.container_window_state = state;
    // inner area starting at (1, 4) with size 40×10
    tab.exec_inner_area = Some(Rect::new(1, 4, 40, 10));
    tab.exec_window_grid = vec![vec!["x".to_string(); 40]; 10];
}

#[test]
fn exec_click_starts_selection_when_container_hidden() {
    let mut app = make_app();
    setup_exec_tab(
        &mut app,
        crate::frontend::tui::tabs::ContainerWindowState::Hidden,
    );

    // Terminal coords (11, 6) → exec col = 11-1 = 10, row = 6-4 = 2
    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            11,
            6,
            KeyModifiers::NONE,
        ),
    );
    let sel = app
        .active_tab()
        .mouse_selection
        .as_ref()
        .expect("click inside the execution window must start a selection");
    assert_eq!((sel.start_col, sel.start_row), (10, 2));
    assert_eq!(
        sel.snapshot.len(),
        10,
        "selection must snapshot the published exec window grid"
    );
}

#[test]
fn exec_click_starts_selection_when_container_minimized() {
    let mut app = make_app();
    setup_exec_tab(
        &mut app,
        crate::frontend::tui::tabs::ContainerWindowState::Minimized,
    );

    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            11,
            6,
            KeyModifiers::NONE,
        ),
    );
    assert!(
        app.active_tab().mouse_selection.is_some(),
        "selection must also work while the container is minimized"
    );
}

#[test]
fn exec_click_outside_inner_area_does_not_start_selection() {
    let mut app = make_app();
    setup_exec_tab(
        &mut app,
        crate::frontend::tui::tabs::ContainerWindowState::Hidden,
    );

    // (0, 0) is on the chrome, outside Rect::new(1, 4, 40, 10).
    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            0,
            0,
            KeyModifiers::NONE,
        ),
    );
    assert!(app.active_tab().mouse_selection.is_none());
}

#[test]
fn exec_click_with_empty_grid_does_not_start_selection() {
    let mut app = make_app();
    setup_exec_tab(
        &mut app,
        crate::frontend::tui::tabs::ContainerWindowState::Hidden,
    );
    app.active_tab_mut().exec_window_grid = Vec::new();

    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            11,
            6,
            KeyModifiers::NONE,
        ),
    );
    assert!(
        app.active_tab().mouse_selection.is_none(),
        "no selection without a rendered grid to snapshot"
    );
}

#[test]
fn exec_click_with_dialog_open_does_not_start_selection() {
    let mut app = make_app();
    setup_exec_tab(
        &mut app,
        crate::frontend::tui::tabs::ContainerWindowState::Hidden,
    );
    app.active_dialog = Some(Dialog::QuitConfirm);

    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            11,
            6,
            KeyModifiers::NONE,
        ),
    );
    assert!(
        app.active_tab().mouse_selection.is_none(),
        "a click on a dialog must not select text underneath it"
    );
}

#[test]
fn exec_drag_extends_selection() {
    let mut app = make_app();
    setup_exec_tab(
        &mut app,
        crate::frontend::tui::tabs::ContainerWindowState::Hidden,
    );

    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            11,
            6,
            KeyModifiers::NONE,
        ),
    );
    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            15,
            8,
            KeyModifiers::NONE,
        ),
    );
    let sel = app.active_tab().mouse_selection.as_ref().unwrap();
    assert_eq!((sel.start_col, sel.start_row), (10, 2));
    assert_eq!((sel.end_col, sel.end_row), (14, 4));
}

#[test]
fn exec_copy_selection_uses_grid_snapshot() {
    let mut app = make_app();
    setup_exec_tab(
        &mut app,
        crate::frontend::tui::tabs::ContainerWindowState::Hidden,
    );
    let grid = vec![
        vec!["h".into(), "i".into(), " ".into()],
        vec!["y".into(), "o".into(), " ".into()],
    ];
    app.active_tab_mut().mouse_selection = Some(crate::frontend::tui::tabs::TextSelection {
        start_col: 0,
        start_row: 0,
        end_col: 1,
        end_row: 1,
        snapshot: grid,
    });
    let text = crate::frontend::tui::key_handler::extract_selection_text(
        app.active_tab().mouse_selection.as_ref().unwrap(),
    );
    assert_eq!(text, "hi\nyo");
}

#[test]
fn cycle_container_window_clears_selection() {
    let mut app = make_app();
    setup_exec_tab(
        &mut app,
        crate::frontend::tui::tabs::ContainerWindowState::Hidden,
    );
    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            11,
            6,
            KeyModifiers::NONE,
        ),
    );
    assert!(app.active_tab().mouse_selection.is_some());

    press_key(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
    assert!(
        app.active_tab().mouse_selection.is_none(),
        "cycling the container window must drop a selection — its coords \
         are relative to the window it started in"
    );
}

// ─── ACP window scroll (WI 0104) ───────────────────────────────────────────

/// Install a single ACP slot (as command-wiring's launch path would) and put
/// the tab in the same Maximized-with-known-inner-area state
/// `setup_container_tab` uses for stdio, so the two are directly comparable.
fn setup_acp_tab(app: &mut App) -> crate::frontend::tui::tabs::SharedAcpState {
    use crate::frontend::tui::tabs::{AcpSlotState, ContainerSlot, ContainerWindowState};
    let state: crate::frontend::tui::tabs::SharedAcpState =
        std::sync::Arc::new(std::sync::Mutex::new(AcpSlotState::default()));
    let tab = app.active_tab_mut();
    tab.container_slots.push(ContainerSlot::new_acp(
        String::new(),
        "claude".into(),
        state.clone(),
    ));
    tab.container_window_state = ContainerWindowState::Maximized;
    tab.container_inner_area = Some(Rect::new(5, 3, 80, 24));
    state
}

#[test]
fn scroll_on_acp_slot_moves_its_own_offset_not_vt100_scrollback() {
    use crate::engine::acp::protocol::ContentChunk;
    use crate::engine::acp::{ContentBlock, SessionUpdate};

    let mut app = make_app();
    let state = setup_acp_tab(&mut app);
    {
        let mut s = state.lock().unwrap();
        for i in 0..5 {
            s.history.push_back(SessionUpdate::AgentMessageChunk {
                chunk: ContentChunk {
                    content: ContentBlock::Text {
                        text: format!("line {i}"),
                    },
                    message_id: None,
                },
            });
        }
    }

    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(MouseEventKind::ScrollUp, 20, 10, KeyModifiers::NONE),
    );
    assert_eq!(
        state.lock().unwrap().scroll_offset,
        3,
        "scrolling up over an ACP slot must advance the ACP window's own offset"
    );
    assert_eq!(
        app.active_tab().container_scroll_offset,
        0,
        "the ACP scroll path must never touch the vt100 scrollback offset \
         a stdio slot uses"
    );

    // Capped at the history length (5 updates), not free to scroll further.
    for _ in 0..5 {
        crate::frontend::tui::mouse_handler::handle_mouse_event(
            &mut app,
            make_mouse_event(MouseEventKind::ScrollUp, 20, 10, KeyModifiers::NONE),
        );
    }
    assert_eq!(
        state.lock().unwrap().scroll_offset,
        5,
        "the offset must clamp at the history length"
    );

    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        make_mouse_event(MouseEventKind::ScrollDown, 20, 10, KeyModifiers::NONE),
    );
    assert_eq!(
        state.lock().unwrap().scroll_offset,
        2,
        "scrolling down must move the ACP offset back toward the live view"
    );
    assert_eq!(
        app.active_tab().container_scroll_offset,
        0,
        "vt100 scrollback must remain untouched throughout"
    );
}
