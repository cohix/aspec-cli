//! Mouse event routing: Workflow Overview scroll, container/PTY scroll
//! forwarding, and click-drag text selection in the execution window and
//! container overlay.

use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};

use super::app::App;
use super::{mouse, tabs};

pub(super) fn handle_mouse_event(app: &mut App, mouse: crossterm::event::MouseEvent) {
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let is_up = matches!(mouse.kind, MouseEventKind::ScrollUp);

            // Workflow Overview scroll takes priority.
            if let Some(overview_rect) = app.active_tab().last_overview_rect {
                if mouse.row >= overview_rect.y
                    && mouse.row < overview_rect.y + overview_rect.height
                    && mouse.column >= overview_rect.x
                    && mouse.column < overview_rect.x + overview_rect.width
                {
                    let tab = app.active_tab_mut();
                    if is_up {
                        tab.workflow_overview_scroll_offset =
                            tab.workflow_overview_scroll_offset.saturating_sub(1);
                    } else {
                        tab.workflow_overview_scroll_offset += 1;
                    }
                    return;
                }
            }

            let tab = app.active_tab_mut();
            if tab.container_overlay_active() {
                // The focused slot owns the overlay — read the agent's
                // terminal modes from its parser. `container_overlay_active`
                // guarantees a slot exists.
                let (mouse_mode, alt_screen, alternate_scroll) = match tab.focused_slot() {
                    Some(slot) => (
                        slot.vt100_parser.screen().mouse_protocol_mode(),
                        slot.agent_alt_screen,
                        slot.agent_alternate_scroll,
                    ),
                    None => return,
                };
                let agent_wants_mouse = mouse_mode != vt100::MouseProtocolMode::None;
                // Agents like codex never enable mouse tracking; they pair
                // the alternate screen with alternate-scroll mode (DECSET
                // 1007) and expect the terminal to translate wheel events
                // into arrow keys. awman plays the terminal's role here.
                let agent_wants_alt_scroll = alt_screen && alternate_scroll;
                let at_live_view = tab.container_scroll_offset == 0;
                let shift_held = mouse.modifiers.contains(KeyModifiers::SHIFT);

                if agent_wants_mouse && at_live_view && !shift_held {
                    mouse::forward_mouse_scroll_to_pty(tab, &mouse);
                } else if agent_wants_alt_scroll && at_live_view && !shift_held {
                    mouse::forward_alt_scroll_to_pty(tab, &mouse);
                } else {
                    mouse::handle_container_scroll(tab, is_up);
                }
            } else if is_up {
                tab.scroll_offset = tab.scroll_offset.saturating_add(5);
            } else {
                tab.scroll_offset = tab.scroll_offset.saturating_sub(5);
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let dialog_open = app.active_dialog.is_some();
            let tab = app.active_tab_mut();
            if tab.container_overlay_active() {
                // ACP windows have no vt100 grid to select text from; the
                // stdio selection path below is skipped for them.
                if tab.focused_slot().is_some_and(|s| s.is_acp()) {
                    return;
                }
                let inner = match tab.container_inner_area {
                    Some(r) => r,
                    None => return,
                };
                // Only start a selection if the click landed inside the vt100
                // grid (not on the border).
                if mouse.column < inner.x
                    || mouse.row < inner.y
                    || mouse.column >= inner.x + inner.width
                    || mouse.row >= inner.y + inner.height
                {
                    return;
                }
                let vt_col = mouse.column - inner.x;
                let vt_row = mouse.row - inner.y;
                let scroll = tab.container_scroll_offset;
                // Snapshot the focused slot's grid (the overlay's content).
                let focused_idx = tab.focused_slot_idx;
                let Some(slot) = tab.container_slots.get_mut(focused_idx) else {
                    return;
                };
                let snapshot = capture_vt100_snapshot(&mut slot.vt100_parser, scroll);
                tab.mouse_selection = Some(tabs::TextSelection {
                    start_col: vt_col,
                    start_row: vt_row,
                    end_col: vt_col,
                    end_row: vt_row,
                    snapshot,
                });
            } else {
                // Execution window selection — available whenever the
                // container overlay isn't covering it (Hidden or Minimized).
                // Dialogs render on top of the window, so a click on one must
                // not start a selection in the text underneath.
                if dialog_open {
                    return;
                }
                let inner = match tab.exec_inner_area {
                    Some(r) => r,
                    None => return,
                };
                if mouse.column < inner.x
                    || mouse.row < inner.y
                    || mouse.column >= inner.x + inner.width
                    || mouse.row >= inner.y + inner.height
                {
                    return;
                }
                if tab.exec_window_grid.is_empty() {
                    return;
                }
                let col = mouse.column - inner.x;
                let row = mouse.row - inner.y;
                tab.mouse_selection = Some(tabs::TextSelection {
                    start_col: col,
                    start_row: row,
                    end_col: col,
                    end_row: row,
                    snapshot: tab.exec_window_grid.clone(),
                });
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let tab = app.active_tab_mut();
            let inner = if tab.container_overlay_active() {
                tab.container_inner_area
            } else {
                tab.exec_inner_area
            };
            let inner = match inner {
                Some(r) => r,
                None => return,
            };
            if let Some(ref mut sel) = tab.mouse_selection {
                let vt_col = mouse
                    .column
                    .saturating_sub(inner.x)
                    .min(inner.width.saturating_sub(1));
                let vt_row = mouse
                    .row
                    .saturating_sub(inner.y)
                    .min(inner.height.saturating_sub(1));
                sel.end_col = vt_col;
                sel.end_row = vt_row;
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let tab = app.active_tab_mut();
            if let Some(ref sel) = tab.mouse_selection {
                // A click without a drag (zero-area selection) is treated as
                // just a click, so accidental Ctrl+Y copies after a stray
                // click don't yank stale text.
                if sel.start_col == sel.end_col && sel.start_row == sel.end_row {
                    tab.mouse_selection = None;
                }
            }
        }
        _ => {}
    }
}

/// Snapshot the vt100 grid into a `Vec<Vec<String>>` of cell contents.
///
/// Why: the vt100 grid mutates with live PTY output. When the user starts a
/// drag selection, they need the copied text to reflect what they *saw* —
/// not the cells' current values.
fn capture_vt100_snapshot(parser: &mut vt100::Parser, scroll_offset: usize) -> Vec<Vec<String>> {
    let screen = parser.screen_mut();
    if scroll_offset > 0 {
        screen.set_scrollback(scroll_offset);
    }
    let snapshot = {
        let (rows, cols) = screen.size();
        (0..rows)
            .map(|row| {
                (0..cols)
                    .map(|col| {
                        screen
                            .cell(row, col)
                            .map(|c| {
                                let s = c.contents();
                                if s.is_empty() {
                                    " ".to_string()
                                } else {
                                    s.to_string()
                                }
                            })
                            .unwrap_or_else(|| " ".to_string())
                    })
                    .collect()
            })
            .collect()
    };
    if scroll_offset > 0 {
        screen.set_scrollback(0);
    }
    snapshot
}
