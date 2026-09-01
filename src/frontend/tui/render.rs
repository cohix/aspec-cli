//! UI chrome rendering — frame layout, tab bar, execution window, status bar,
//! command box, suggestion row.

use ratatui::prelude::*;
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, Wrap,
};

use crate::frontend::tui::acp_view;
use crate::frontend::tui::app::{App, Focus};
use crate::frontend::tui::container_view;
use crate::frontend::tui::dialogs;
use crate::frontend::tui::git_sidebar::{self, GitDiffSummary, GitFileChangeType, GitFileEntry};
use crate::frontend::tui::tabs::{
    self, compute_tab_bar_width, phase_label, tab_color, window_border_color, ContainerWindowState,
    ExecutionPhase,
};
use crate::frontend::tui::workflow_view;

mod amie;
mod command_box;
mod dialog;
mod execution_window;
mod sidebar;
mod status_bar;
mod tab_bar;
#[cfg(test)]
mod tests;

/// Rows the tab bar occupies at the top of the frame.
const TAB_BAR_HEIGHT: u16 = 3;
/// Rows the bottom chrome occupies: status bar + command box + suggestion row.
const BOTTOM_CHROME_HEIGHT: u16 = 5;
/// Rows the execution window keeps for itself once the Workflow Overview has
/// taken its share; the container status bars are truncated before this is.
const MIN_EXEC_HEIGHT: u16 = 5;

/// Render the full TUI frame.
pub fn render_frame(app: &mut App, frame: &mut Frame) {
    let area = frame.area();

    // Read shape decisions from the active tab (immutable borrow).
    let (wf_state, overview_state, container_state, has_summary, git_sidebar_state, slot_count) = {
        let tab = app.active_tab();
        (
            tab.workflow_state.lock().ok().and_then(|g| g.clone()),
            tab.workflow_overview_state,
            tab.container_window_state,
            tab.last_container_summary.is_some(),
            tab.git_sidebar_state,
            tab.container_slots.len(),
        )
    };

    // Vertical budget: everything between the tab bar and the command box is
    // shared by the execution window, the container status bars, and the
    // Workflow Overview.
    let body_height = area
        .height
        .saturating_sub(TAB_BAR_HEIGHT + BOTTOM_CHROME_HEIGHT);

    // Container display shape, unified across 1..N slots:
    // - Maximized: the focused slot as an overlay + one bar per other slot.
    // - Minimized: every slot as a bar, no overlay.
    // - Hidden: nothing (the post-exit summary bar shows here, if any).
    //
    // Ctrl-M owns this entirely — a maximized Workflow Overview never demotes
    // the container PTY, and vice versa.
    let show_overlay = container_state == ContainerWindowState::Maximized && slot_count > 0;

    // The two windows share the body instead of displacing each other: while
    // the PTY overlay is on screen a maximized overview takes at most half the
    // body (never less than one box row), so both stay usable. On its own the
    // overview may still grow into the whole body. A minimized overview only
    // ever wants one box row, so it is never capped.
    let overview_budget = if show_overlay && overview_state.is_maximized() {
        (body_height / 2)
            .max(workflow_view::STEP_BOX_HEIGHT)
            .min(body_height)
    } else {
        body_height
    };
    let workflow_height = wf_state
        .as_ref()
        .map(|s| workflow_view::workflow_overview_height(s, overview_state, overview_budget))
        .unwrap_or(0);

    // When the git sidebar is open (and wide enough), reserve the right ≤25%
    // of the frame for it; the execution/container windows shrink into the
    // remaining left chunk. Below `MIN_SIDEBAR_WIDTH` the sidebar is treated
    // as closed (only the status-bar summary shows).
    let sidebar_w = git_sidebar::sidebar_width(area.width, git_sidebar_state);
    let (main_area, sidebar_area) = if sidebar_w > 0 {
        let split =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(sidebar_w)]).split(area);
        (split[0], Some(split[1]))
    } else {
        (area, None)
    };

    let n_minimized_bars = if slot_count == 0 {
        0
    } else {
        match container_state {
            ContainerWindowState::Maximized => (slot_count - 1) as u16,
            ContainerWindowState::Minimized => slot_count as u16,
            ContainerWindowState::Hidden => 0,
        }
    };
    let minimized_bars_height = n_minimized_bars * container_view::PARALLEL_BAR_HEIGHT;

    // Show the post-exit summary in the same layout slot as the minimized
    // bars, but only when the container display is Hidden (i.e. the previous
    // run finished and we haven't started another).
    let has_summary_bar = container_state == ContainerWindowState::Hidden && has_summary;

    let wanted_bar_height = if n_minimized_bars > 0 {
        minimized_bars_height
    } else if has_summary_bar {
        3
    } else {
        0
    };

    // Split what is left of the body. The Workflow Overview is served first —
    // it has already been clamped to `overview_budget`, so when it wants
    // everything it may have it gets it. The execution window keeps its 5-row
    // minimum out of the remainder, and the container status bars are truncated
    // into what survives (`render_container_bars` stops at the last bar that
    // fits).
    let rest = body_height.saturating_sub(workflow_height);
    let exec_min = MIN_EXEC_HEIGHT.min(rest);
    let extra_bar_height = wanted_bar_height.min(rest - exec_min);
    let exec_height = rest - extra_bar_height;

    let chunks = Layout::vertical([
        Constraint::Length(TAB_BAR_HEIGHT),   // tab bar
        Constraint::Length(exec_height),      // execution window
        Constraint::Length(extra_bar_height), // minimized OR summary
        Constraint::Length(workflow_height),  // Workflow Overview
        Constraint::Length(1),                // status bar
        Constraint::Length(3),                // command box
        Constraint::Length(1),                // suggestion row
    ])
    .split(main_area);

    tab_bar::render_tab_bar(app, chunks[0], frame);
    // WI 0102: the amie tab replaces the execution-window body with the
    // condition list. While an attach session owns the tab's slots the normal
    // execution/container rendering applies unchanged, which is what makes
    // attach reproduce the `exec workflow --dynamic` UX with no renderer
    // changes (see WI 0102 §0.3).
    if app.active_tab().is_amie && app.active_tab().container_slots.is_empty() {
        amie::render_amie_body(app, chunks[1], frame);
    } else {
        execution_window::render_execution_window(app, chunks[1], frame);
    }

    if n_minimized_bars > 0 {
        // Two passes over the same area: the container pass draws stdio bars,
        // the ACP pass draws ACP bars. Both advance one bar height per
        // non-focused slot in slot order, so a mixed group tiles cleanly.
        container_view::render_container_bars(app.active_tab(), chunks[2], frame, show_overlay);
        acp_view::render_acp_bars(app.active_tab(), chunks[2], frame, show_overlay);
    } else if has_summary_bar {
        if let Some(summary) = app.active_tab().last_container_summary.as_ref() {
            container_view::render_container_summary(summary, chunks[2], frame);
        }
    }

    if let Some(wf_state) = wf_state.as_ref() {
        let scroll_offset = app.active_tab().workflow_overview_scroll_offset;
        workflow_view::render_workflow_overview(
            wf_state,
            chunks[3],
            frame,
            scroll_offset,
            overview_state,
        );
        app.active_tab_mut().last_overview_rect = Some(chunks[3]);
    } else {
        app.active_tab_mut().last_overview_rect = None;
    }

    status_bar::render_status_bar(app, chunks[4], frame, sidebar_area.is_some());
    command_box::render_command_box(app, chunks[5], frame);
    command_box::render_suggestion_row(app, chunks[6], frame);

    // Container maximized overlay (rendered on top of execution window only,
    // not over the Workflow Overview, minimized bars, or bottom chrome).
    // Confined to the left chunk (`main_area`) so it never covers the git
    // sidebar.
    if show_overlay {
        let tab = app.active_tab_mut();
        // The overlay made it to the screen; close_container_overlay no
        // longer needs to replay its contents into the status log.
        tab.container_rendered = true;
        // Dispatch on the focused slot's kind: an ACP window renders the
        // structured update list, a stdio window renders the vt100 grid.
        let is_acp = tab.focused_slot().is_some_and(|s| s.is_acp());
        if is_acp {
            acp_view::render_acp_maximized(
                tab,
                main_area,
                workflow_height + extra_bar_height,
                frame,
            );
        } else {
            container_view::render_container_maximized(
                tab,
                main_area,
                workflow_height + extra_bar_height,
                frame,
            );
        }
    }

    // Git sidebar (right chunk), when open and wide enough.
    if let Some(sidebar_area) = sidebar_area {
        let summary = app
            .active_tab()
            .git_diff_summary
            .lock()
            .ok()
            .and_then(|g| g.clone());
        sidebar::render_git_sidebar(frame, sidebar_area, &summary);
    }

    // Active dialog (rendered on top of everything).
    if let Some(dialog) = &app.active_dialog {
        dialog::render_dialog(dialog, area, frame);
    }
}
