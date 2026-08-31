//! ACP (Agent Client Protocol) agent-window rendering.
//!
//! Structurally parallel to `container_view.rs`, but for slots whose
//! [`AgentWindowKind`](crate::frontend::tui::tabs::AgentWindowKind) is `Acp`.
//! An ACP window has no PTY and no vt100 grid: the agent emits structured
//! [`SessionUpdate`] frames which are kept in the slot's shared
//! [`AcpSlotState`](crate::frontend::tui::tabs::AcpSlotState) and drawn here as
//! a scrollable list.
//!
//! Render pieces mirror the stdio ones:
//! - **Maximized overlay** (`render_acp_maximized`): the focused ACP slot in
//!   the same centered ~95% overlay `render_container_maximized` uses, with the
//!   agent name / step as the left title and update/elapsed stats on the right.
//!   The border is drawn in [`ACP_BORDER_COLOR`] instead of green.
//! - **Minimized bars** (`render_acp_bars`): one 3-row purple rounded strip per
//!   non-focused ACP slot, positioned so it interleaves cleanly with the stdio
//!   bars `render_container_bars` draws for the same tab.
//!
//! Everything reads the view from `AcpSlotState`; there is no raw terminal
//! stream to forward resize to, so the view simply recomputes from
//! `outer_area` every frame like the container overlay does.

use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};

use crate::engine::acp::{ContentBlock, SessionUpdate};
use crate::frontend::tui::container_view::PARALLEL_BAR_HEIGHT;
use crate::frontend::tui::tabs::{format_duration, ContainerSlot, Tab};

/// Identity color for ACP agent windows: a distinct purple (`#9333EA`).
///
/// Deliberately **not** `Color::Magenta`, which `tab_color` already uses for
/// remote tabs and the yolo-countdown flash — an RGB purple keeps the ACP
/// window visually separate from those states.
pub const ACP_BORDER_COLOR: Color = Color::Rgb(147, 51, 234);

/// Render the focused ACP window when Maximized.
///
/// Geometry matches `render_container_maximized` exactly (a centered overlay
/// covering ~95% of the execution-window area), so switching a slot between
/// stdio and ACP never moves the window. `workflow_strip_height` is the number
/// of rows reserved below the overlay for the workflow strip and minimized
/// bars; the overlay must not cover them.
pub fn render_acp_maximized(
    tab: &mut Tab,
    outer_area: Rect,
    workflow_strip_height: u16,
    frame: &mut Frame,
) {
    // Identical sizing to render_container_maximized.
    let top_reserved: u16 = 3;
    let bottom_reserved: u16 = 5 + workflow_strip_height;
    let exec_height = outer_area
        .height
        .saturating_sub(top_reserved + bottom_reserved);
    let exec_width = outer_area.width;

    let window_height = ((exec_height as u32 * 95 / 100) as u16).max(5);
    let window_width = ((exec_width as u32 * 95 / 100) as u16).max(10);
    let offset_x = (exec_width.saturating_sub(window_width)) / 2;
    let offset_y = top_reserved + (exec_height.saturating_sub(window_height)) / 2;
    let window_area = Rect {
        x: outer_area.x + offset_x,
        y: outer_area.y + offset_y,
        width: window_width,
        height: window_height,
    };

    let Some(slot) = tab.focused_slot() else {
        return;
    };
    let Some(state) = slot.acp_state() else {
        return;
    };

    // Titles.
    let agent_name = slot
        .container_info
        .as_ref()
        .map(|i| i.agent_display_name.as_str())
        .unwrap_or("Agent");
    // While a workflow runs, show the step this window is executing: the
    // slot's own step (parallel group slots), else the workflow's current
    // step (sequential steps run in the backbone slot, whose step_name is
    // empty).
    let step_name: Option<String> = Some(slot.step_name.clone())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            tab.workflow_state
                .lock()
                .ok()
                .and_then(|g| g.as_ref().and_then(|v| v.current_step.clone()))
        });
    let left_title = match step_name {
        Some(step) => format!(" \u{1F512} {} (acp) \u{2014} {} ", agent_name, step),
        None => format!(" \u{1F512} {} (acp) ", agent_name),
    };

    // Read the shared render state.
    let (lines, scroll_offset, pending) = match state.lock() {
        Ok(s) => (
            render_history_lines(&s.history),
            s.scroll_offset,
            s.pending_permission.is_some(),
        ),
        Err(_) => (Vec::new(), 0, false),
    };

    let elapsed = slot
        .container_info
        .as_ref()
        .map(|i| i.start_time.elapsed().as_secs())
        .unwrap_or(0);
    let right_title = format!(
        " {} updates | {} ",
        lines_update_count(&lines),
        format_duration(elapsed)
    );

    frame.render_widget(Clear, window_area);

    let mut block = Block::default()
        .title(Line::from(left_title).alignment(Alignment::Left))
        .title(Line::from(right_title).alignment(Alignment::Right))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACP_BORDER_COLOR));

    if scroll_offset > 0 {
        block = block.title(
            Line::from(Span::styled(
                format!(" \u{2191} scrollback ({} lines) ", scroll_offset),
                Style::default().fg(Color::Yellow),
            ))
            .alignment(Alignment::Center),
        );
    }
    if pending {
        block = block.title_bottom(
            Line::from(Span::styled(
                " \u{23F3} permission requested \u{2014} respond in the dialog ",
                Style::default().fg(Color::Yellow),
            ))
            .alignment(Alignment::Center),
        );
    }

    let inner = block.inner(window_area);
    frame.render_widget(block, window_area);

    // Keep the inner rect published for parity with the container overlay;
    // ACP windows have no vt100 text selection (the mouse handler skips it),
    // but the mouse-scroll routing relies on the overlay being "active".
    tab.container_inner_area = Some(inner);

    render_update_list(&lines, inner, scroll_offset, frame);
}

/// Draw the update list into `area`, honoring a lines-from-bottom
/// `scroll_offset`. This is the ACP window's own simple list scroll — a
/// bottom-anchored `Paragraph` scroll, the same shape the status log uses,
/// with none of the vt100 scrollback machinery.
fn render_update_list(lines: &[Line], area: Rect, scroll_offset: usize, frame: &mut Frame) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let inner_height = area.height as usize;
    let inner_width = area.width as usize;
    let total_visual: usize = lines
        .iter()
        .map(|l| {
            let w = l.width();
            if w == 0 {
                1
            } else {
                w.div_ceil(inner_width)
            }
        })
        .sum();
    let max_scroll = total_visual.saturating_sub(inner_height);
    let effective_offset = scroll_offset.min(max_scroll);
    let scroll_y = max_scroll.saturating_sub(effective_offset);

    let para = Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .scroll((scroll_y as u16, 0));
    frame.render_widget(para, area);
}

/// Number of terminal rows one minimized ACP bar occupies. Kept equal to the
/// container bar height so stdio and ACP bars tile together in one column.
pub const ACP_BAR_HEIGHT: u16 = PARALLEL_BAR_HEIGHT;

/// Render the minimized bars for the ACP slots of a tab.
///
/// Structurally identical to `render_container_bars`, and designed to compose
/// with it: both walk `container_slots` in order, skip the focused slot when
/// `skip_focused` is set, and advance one bar height per non-focused slot —
/// but each only *draws* its own kind (stdio there, ACP here). Calling both
/// over the same area tiles a mixed group's bars in slot order with no
/// overlap. For an all-stdio tab this draws nothing; for an all-ACP tab
/// `render_container_bars` draws nothing.
pub fn render_acp_bars(tab: &Tab, area: Rect, frame: &mut Frame, skip_focused: bool) {
    let mut row: u16 = 0;
    for (idx, slot) in tab.container_slots.iter().enumerate() {
        if skip_focused && idx == tab.focused_slot_idx {
            continue;
        }
        if row + ACP_BAR_HEIGHT > area.height {
            break;
        }
        if slot.is_acp() {
            let bar_area = Rect::new(area.x, area.y + row, area.width, ACP_BAR_HEIGHT);
            render_one_acp_bar(slot, bar_area, frame);
        }
        row += ACP_BAR_HEIGHT;
    }
}

fn render_one_acp_bar(slot: &ContainerSlot, area: Rect, frame: &mut Frame) {
    let update_count = slot
        .acp_state()
        .and_then(|s| s.lock().ok().map(|g| g.history.len()))
        .unwrap_or(0);
    let step_segment = if slot.step_name.is_empty() {
        String::new()
    } else {
        format!(" [{}]", slot.step_name)
    };
    let content = format!(
        "\u{1F512} {}{} | acp | {} updates | {}",
        slot.agent_name(),
        step_segment,
        update_count,
        format_duration(slot.elapsed_secs()),
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACP_BORDER_COLOR));

    let para = Paragraph::new(Line::from(vec![Span::styled(
        format!(" {}", content),
        Style::default().fg(ACP_BORDER_COLOR),
    )]))
    .block(block);

    frame.render_widget(para, area);
}

// ─── Internals ──────────────────────────────────────────────────────────

/// Count of "real" content lines (a proxy for the number of rendered updates)
/// used only for the overlay's right-hand title.
fn lines_update_count(lines: &[Line]) -> usize {
    lines.iter().filter(|l| l.width() > 0).count()
}

/// Turn the update history into styled display lines. Each update contributes
/// one or more lines; a blank spacer separates updates for readability.
pub(crate) fn render_history_lines(
    history: &std::collections::VecDeque<SessionUpdate>,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    for update in history {
        push_update_lines(&mut lines, update);
    }
    lines
}

fn push_update_lines(lines: &mut Vec<Line<'static>>, update: &SessionUpdate) {
    match update {
        SessionUpdate::AgentMessageChunk { chunk } => {
            for text_line in content_block_text(&chunk.content).lines() {
                lines.push(Line::from(text_line.to_string()));
            }
        }
        SessionUpdate::AgentThoughtChunk { chunk } => {
            for text_line in content_block_text(&chunk.content).lines() {
                lines.push(Line::from(Span::styled(
                    format!("\u{1F4AD} {}", text_line),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                )));
            }
        }
        SessionUpdate::ToolCall { tool_call } => {
            let status = tool_call
                .status
                .as_deref()
                .map(|s| format!(" [{}]", s))
                .unwrap_or_default();
            lines.push(Line::from(Span::styled(
                format!("\u{1F527} {}{}", tool_call.title, status),
                Style::default().fg(Color::Cyan),
            )));
        }
        SessionUpdate::ToolCallUpdate { tool_call } => {
            let title = tool_call.title.clone().unwrap_or_default();
            let status = tool_call
                .status
                .as_deref()
                .map(|s| format!(" [{}]", s))
                .unwrap_or_default();
            let text = format!("   \u{2514} {}{}", title, status);
            lines.push(Line::from(Span::styled(
                text,
                Style::default().fg(Color::DarkGray),
            )));
        }
        SessionUpdate::Plan { entries } => {
            lines.push(Line::from(Span::styled(
                "\u{1F4CB} Plan",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            )));
            for entry in entries {
                lines.push(Line::from(Span::styled(
                    format!("   \u{2022} {} [{}]", entry.content, entry.status),
                    Style::default().fg(Color::Magenta),
                )));
            }
        }
        SessionUpdate::AvailableCommandsUpdate { available_commands } => {
            let names: Vec<&str> = available_commands
                .iter()
                .map(|c| c.name.as_str())
                .collect();
            lines.push(Line::from(Span::styled(
                format!("\u{2318} commands: {}", names.join(", ")),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
}

/// Flatten a [`ContentBlock`] to display text. Non-text blocks render a short
/// bracketed placeholder rather than their raw (often base64) payload.
fn content_block_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text { text } => text.clone(),
        ContentBlock::Image { .. } => "[image]".to_string(),
        ContentBlock::Audio { .. } => "[audio]".to_string(),
        ContentBlock::ResourceLink { name, uri, .. } => format!("[link: {} <{}>]", name, uri),
        ContentBlock::Resource { .. } => "[resource]".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::acp::protocol::ContentChunk;
    use crate::engine::acp::ToolCall;

    fn text_update(s: &str) -> SessionUpdate {
        SessionUpdate::AgentMessageChunk {
            chunk: ContentChunk {
                content: ContentBlock::Text { text: s.into() },
                message_id: None,
            },
        }
    }

    #[test]
    fn history_lines_render_agent_text() {
        let mut history = std::collections::VecDeque::new();
        history.push_back(text_update("hello world"));
        let lines = render_history_lines(&history);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
            .collect();
        assert!(joined.contains("hello world"), "got: {joined}");
    }

    #[test]
    fn history_lines_render_tool_call_title() {
        let mut history = std::collections::VecDeque::new();
        history.push_back(SessionUpdate::ToolCall {
            tool_call: ToolCall {
                tool_call_id: "t1".into(),
                title: "Read file".into(),
                kind: Some("read".into()),
                status: Some("pending".into()),
                content: vec![],
                locations: vec![],
                raw_input: None,
                raw_output: None,
            },
        });
        let lines = render_history_lines(&history);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
            .collect();
        assert!(joined.contains("Read file"), "got: {joined}");
        assert!(joined.contains("pending"), "got: {joined}");
    }

    #[test]
    fn acp_border_color_is_not_magenta() {
        assert_ne!(ACP_BORDER_COLOR, Color::Magenta);
        assert_eq!(ACP_BORDER_COLOR, Color::Rgb(147, 51, 234));
    }
}
