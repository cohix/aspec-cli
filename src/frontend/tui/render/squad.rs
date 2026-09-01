//! Squad tab body. Sibling of `command_box.rs` / `tab_bar.rs`.
//!
//! Renders the task grid plus the key-hint line. Reads only `Tab::squad`'s
//! shared snapshot — never the tab's synthetic `Session`, never the persistent
//! task store, never a gateway.

use super::*;

use crate::data::fs::task_store::{RunStatus, Task, TaskStatus};
use crate::frontend::tui::tabs::squad_state::SquadSnapshot;

/// Minimum card size, in cells, per WI 0106 Part 5 ("generous size and
/// spacing"). The grid reflows its column count to keep every card at least
/// this wide as the tab is resized; card height is fixed.
const CARD_MIN_WIDTH: u16 = 30;
const CARD_MIN_HEIGHT: u16 = 6;
const CARD_COL_SPACING: u16 = 2;
const CARD_ROW_SPACING: u16 = 1;

/// Render the squad task list into `area`.
pub(super) fn render_squad_body(app: &mut App, area: Rect, frame: &mut Frame) {
    let (snapshot, selected) = {
        let tab = app.active_tab();
        match tab.squad.as_ref() {
            Some(state) => {
                let snap = state
                    .snapshot
                    .lock()
                    .ok()
                    .map(|g| g.clone())
                    .unwrap_or_default();
                (snap, state.selected)
            }
            None => (SquadSnapshot::default(), 0),
        }
    };

    let block = Block::default()
        .title(" squad ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // Optional header: daemon-down state (kept above the last-known list, never
    // replacing it with an empty one) or the pre-first-poll loading line.
    let mut header_lines: Vec<Line> = Vec::new();
    if let Some(msg) = snapshot.error.as_ref() {
        header_lines.push(Line::from(Span::styled(
            "squad daemon not reachable",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
        header_lines.push(Line::from(Span::styled(
            msg.clone(),
            Style::default().fg(Color::Red),
        )));
    } else if !snapshot.loaded {
        header_lines.push(Line::from(Span::styled(
            "Loading\u{2026}",
            Style::default().fg(Color::DarkGray),
        )));
    }
    let header_h = header_lines.len() as u16;

    let hint =
        "enter detail \u{b7} a attach \u{b7} n new \u{b7} p pause \u{b7} r resume \u{b7} d delete";

    let chunks = Layout::vertical([
        Constraint::Length(header_h),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);

    if header_h > 0 {
        frame.render_widget(Paragraph::new(header_lines), chunks[0]);
    }

    let columns = render_task_grid(&snapshot.tasks, selected, chunks[1], frame);
    // Publish the column count the grid was actually laid out with so
    // Left/Right/Up/Down (`SquadTabState::move_selection`/`move_selection_col`)
    // can turn the linear `selected` index into 2D movement. Selection itself
    // stays a linear index into `tasks` (never a `(row, col)` pair), so a
    // reflow that changes `columns` can never silently reselect a different
    // task — see the field doc on `SquadTabState::grid_columns`.
    if let Some(state) = app.active_tab_mut().squad.as_mut() {
        state.grid_columns = columns;
    }

    frame.render_widget(
        Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray))),
        chunks[2],
    );
}

/// The column count a grid of `CARD_MIN_WIDTH`-plus-spacing cards fits into
/// `width`. Always at least 1, even when `width` is narrower than one card —
/// the single column just renders squeezed rather than disappearing.
fn grid_columns_for_width(width: u16) -> usize {
    if width == 0 {
        return 1;
    }
    let cell = CARD_MIN_WIDTH + CARD_COL_SPACING;
    ((width / cell) as usize).max(1)
}

/// Lay `tasks` out as a grid of rounded-rectangle cards and render it into
/// `area`. Returns the column count used, so the caller can publish it for
/// the key handler's 2D navigation. Renders an empty-state message instead
/// of a zero-card grid when `tasks` is empty.
fn render_task_grid(tasks: &[Task], selected: usize, area: Rect, frame: &mut Frame) -> usize {
    if tasks.is_empty() {
        render_empty_state(area, frame);
        return 1;
    }

    let columns = grid_columns_for_width(area.width);
    let row_cell = CARD_MIN_HEIGHT + CARD_ROW_SPACING;
    let visible_rows = ((area.height / row_cell) as usize).max(1);
    let rows_total = tasks.len().div_ceil(columns);

    // When every row fits, show them all; otherwise scroll the row window so
    // the selected card is always on screen (selection is a linear index, so
    // this window never changes what task is selected — only what's drawn).
    let selected_row = selected / columns;
    let start_row = if rows_total <= visible_rows {
        0
    } else {
        selected_row
            .saturating_sub(visible_rows.saturating_sub(1))
            .min(rows_total - visible_rows)
    };
    let end_row = (start_row + visible_rows).min(rows_total);

    let row_constraints: Vec<Constraint> = (start_row..end_row)
        .map(|_| Constraint::Length(CARD_MIN_HEIGHT))
        .collect();
    let row_areas = Layout::vertical(row_constraints)
        .spacing(CARD_ROW_SPACING)
        .split(area);

    // A card never spans more than half the grid width: with few columns the
    // remaining width is left empty rather than stretching one card across the
    // whole tab. On a terminal too narrow for a half-width card to reach the
    // minimum card width, the minimum wins and the card may exceed half.
    let card_width = grid_card_width(area.width, columns);

    for (ri, row_area) in row_areas.iter().enumerate() {
        let row = start_row + ri;
        let row_start = row * columns;
        let row_end = (row_start + columns).min(tasks.len());
        let n = row_end - row_start;
        if n == 0 {
            continue;
        }
        let col_constraints: Vec<Constraint> =
            (0..n).map(|_| Constraint::Length(card_width)).collect();
        // Flex::Start pins every card to its fixed width: leftover row width
        // stays empty instead of stretching the final card back to full width.
        let col_areas = Layout::horizontal(col_constraints)
            .flex(ratatui::layout::Flex::Start)
            .spacing(CARD_COL_SPACING)
            .split(*row_area);
        for (ci, card_area) in col_areas.iter().enumerate() {
            let idx = row_start + ci;
            render_task_card(&tasks[idx], idx == selected, *card_area, frame);
        }
    }

    columns
}

/// The width every card in the grid renders at: an even share of the grid
/// width, capped at half of it (WI fix: a lone column must not produce a
/// full-width card). `CARD_MIN_WIDTH` still wins over the half-width cap so
/// narrow terminals keep a readable card.
fn grid_card_width(area_width: u16, columns: usize) -> u16 {
    let columns = (columns.max(1)) as u16;
    let total_spacing = CARD_COL_SPACING * columns.saturating_sub(1);
    let share = area_width.saturating_sub(total_spacing) / columns;
    let cap = (area_width / 2).max(CARD_MIN_WIDTH.min(area_width));
    share.min(cap).max(1)
}

/// A message in place of the grid when there are no tasks — never a
/// zero-card layout with dangling borders.
fn render_empty_state(area: Rect, frame: &mut Frame) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let line_area = Rect {
        x: area.x,
        y: area.y + area.height / 2,
        width: area.width,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            "No squad tasks yet \u{2014} press 'n' to create one.",
            Style::default().fg(Color::DarkGray),
        ))
        .alignment(Alignment::Center),
        line_area,
    );
}

/// Render a single task as a rounded-rectangle card: name as the block
/// title, then the same three fields the table used to show as columns
/// (summary, last run, next evaluation) as body lines.
fn render_task_card(task: &Task, is_selected: bool, area: Rect, frame: &mut Frame) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let (border_style, title_style) = if is_selected {
        (
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (
            Style::default().fg(Color::DarkGray),
            Style::default().add_modifier(Modifier::BOLD),
        )
    };
    let block = Block::default()
        .title(Span::styled(format!(" {} ", task.name), title_style))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let last_run = task
        .last_run_at
        .map(format_time)
        .unwrap_or_else(|| "\u{2014}".to_string());
    let next = next_evaluation(task);

    // The outcome and its timestamp get a line each: together they are wider
    // than a minimum-width card, and the outcome is the half that matters, so
    // it is never the half that gets clipped. Paused is not repeated here —
    // `next_evaluation` already renders it as the next-evaluation answer.
    //
    // No `.wrap()`: each `Line` is horizontally clipped to `inner.width` by
    // the buffer. The description is truncated explicitly (with an ellipsis)
    // to the card's actual inner width so the cut is visible rather than a
    // silent clip.
    let lines = vec![
        Line::from(Span::raw(truncate_to_width(
            &first_line(&task.description),
            inner.width as usize,
        ))),
        Line::from(vec![
            Span::styled("Last run: ", Style::default().fg(Color::DarkGray)),
            Span::raw(last_run_outcome(task)),
        ]),
        Line::from(Span::styled(
            format!("          {last_run}"),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(vec![
            Span::styled("Next: ", Style::default().fg(Color::DarkGray)),
            Span::raw(next),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The outcome of the task's most recent run — what actually happened, not
/// whether the task is scheduled. Paused/active is separate state and is shown
/// separately; a task that has never run reads `never run`.
fn last_run_outcome(task: &Task) -> &'static str {
    match task.last_run_status {
        Some(RunStatus::Running) => "running",
        Some(RunStatus::NotTriggered) => "not triggered",
        Some(RunStatus::WorkflowExecuted) => "workflow executed",
        Some(RunStatus::Failed) => "failed",
        Some(RunStatus::Interrupted) => "interrupted",
        None => "never run",
    }
}

/// The first line of a (possibly multi-line) description, used as the
/// card's short summary.
fn first_line(description: &str) -> String {
    description.lines().next().unwrap_or("").to_string()
}

/// Truncate `text` to at most `width` display cells, ending in an ellipsis
/// when anything was cut. Width-aware so wide characters never overflow the
/// card border.
fn truncate_to_width(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    use unicode_width::UnicodeWidthStr;

    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    let budget = width.saturating_sub(1); // reserve one cell for the ellipsis
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('\u{2026}');
    out
}

/// Format a timestamp for the card fields.
fn format_time(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%Y-%m-%d %H:%M").to_string()
}

/// The task's next scheduled evaluation, as display text:
/// `paused` when paused, `now` when it has never run, otherwise
/// `last_run_at + interval`, or `backoff_until` when that is later.
fn next_evaluation(task: &Task) -> String {
    if task.status == TaskStatus::Paused {
        return "paused".to_string();
    }
    let Some(last) = task.last_run_at else {
        return "now".to_string();
    };
    let mut next = last + chrono::Duration::seconds(task.interval_secs as i64);
    if let Some(backoff) = task.backoff_until {
        if backoff > next {
            next = backoff;
        }
    }
    format_time(next)
}

#[cfg(test)]
mod tests {
    use super::{grid_card_width, truncate_to_width, CARD_MIN_WIDTH};

    #[test]
    fn a_single_column_card_is_capped_at_half_the_grid_width() {
        assert_eq!(
            grid_card_width(120, 1),
            60,
            "one column must not span the tab"
        );
        // Two columns already share the width below the cap.
        assert_eq!(grid_card_width(120, 2), 59);
    }

    #[test]
    fn the_minimum_card_width_survives_a_narrow_terminal() {
        // Half of 40 is 20, below the 30-cell minimum: the minimum wins.
        assert_eq!(grid_card_width(40, 1), CARD_MIN_WIDTH);
        // Narrower than the minimum itself: the card takes what exists.
        assert_eq!(grid_card_width(20, 1), 20);
    }

    #[test]
    fn descriptions_are_truncated_with_an_ellipsis_to_the_card_width() {
        assert_eq!(truncate_to_width("short", 28), "short");
        assert_eq!(
            truncate_to_width("a very long description that cannot fit", 12),
            "a very long\u{2026}"
        );
        // Width-aware: wide characters count as two cells.
        assert_eq!(truncate_to_width("日本語テスト", 5), "日本\u{2026}");
    }
}
