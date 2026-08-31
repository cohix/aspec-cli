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

/// Render the full TUI frame.
pub fn render_frame(app: &mut App, frame: &mut Frame) {
    let area = frame.area();

    // Read shape decisions from the active tab (immutable borrow).
    let (workflow_height, container_state, has_summary, git_sidebar_state, slot_count) = {
        let tab = app.active_tab();
        let workflow_height = tab
            .workflow_state
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(workflow_view::workflow_strip_height))
            .unwrap_or(0);
        (
            workflow_height,
            tab.container_window_state,
            tab.last_container_summary.is_some(),
            tab.git_sidebar_state,
            tab.container_slots.len(),
        )
    };

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

    // Container display shape, unified across 1..N slots:
    // - Maximized: the focused slot as an overlay + one bar per other slot.
    // - Minimized: every slot as a bar, no overlay.
    // - Hidden: nothing (the post-exit summary bar shows here, if any).
    let show_overlay = container_state == ContainerWindowState::Maximized && slot_count > 0;
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

    let extra_bar_height = if n_minimized_bars > 0 {
        minimized_bars_height
    } else if has_summary_bar {
        3
    } else {
        0
    };

    let chunks = Layout::vertical([
        Constraint::Length(3),                // tab bar
        Constraint::Min(5),                   // execution window
        Constraint::Length(extra_bar_height), // minimized OR summary
        Constraint::Length(workflow_height),  // workflow strip
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

    if let Some(wf_state) = app
        .active_tab()
        .workflow_state
        .lock()
        .ok()
        .and_then(|g| g.clone())
    {
        let scroll_offset = app.active_tab().workflow_strip_scroll_offset;
        workflow_view::render_workflow_strip(&wf_state, chunks[3], frame, scroll_offset);
        app.active_tab_mut().last_strip_rect = Some(chunks[3]);
    } else {
        app.active_tab_mut().last_strip_rect = None;
    }

    status_bar::render_status_bar(app, chunks[4], frame, sidebar_area.is_some());
    command_box::render_command_box(app, chunks[5], frame);
    command_box::render_suggestion_row(app, chunks[6], frame);

    // Container maximized overlay (rendered on top of execution window only,
    // not over the workflow strip, minimized bars, or bottom chrome).
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
                workflow_height + minimized_bars_height,
                frame,
            );
        } else {
            container_view::render_container_maximized(
                tab,
                main_area,
                workflow_height + minimized_bars_height,
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
