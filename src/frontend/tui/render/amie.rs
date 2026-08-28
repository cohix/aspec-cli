//! Amie tab body. Sibling of `command_box.rs` / `tab_bar.rs`.
//!
//! Renders the condition list plus the key-hint line. Reads only `Tab::amie`'s
//! shared snapshot — never the tab's synthetic `Session`, never the persistent
//! condition store, never a gateway.

use super::*;

use crate::data::fs::condition_store::{Condition, ConditionStatus};
use crate::frontend::tui::tabs::amie_state::AmieSnapshot;

/// Render the amie condition list into `area`.
pub(super) fn render_amie_body(app: &mut App, area: Rect, frame: &mut Frame) {
    let (snapshot, selected) = {
        let tab = app.active_tab();
        match tab.amie.as_ref() {
            Some(state) => {
                let snap = state
                    .snapshot
                    .lock()
                    .ok()
                    .map(|g| g.clone())
                    .unwrap_or_default();
                (snap, state.selected)
            }
            None => (AmieSnapshot::default(), 0),
        }
    };

    let block = Block::default()
        .title(" amie ")
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
            "amie daemon not reachable",
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

    render_condition_table(&snapshot.conditions, selected, chunks[1], frame);

    frame.render_widget(
        Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray))),
        chunks[2],
    );
}

fn render_condition_table(
    conditions: &[Condition],
    selected: usize,
    area: Rect,
    frame: &mut Frame,
) {
    let header_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let header = Row::new(vec![
        Cell::from("Name").style(header_style),
        Cell::from("Status").style(header_style),
        Cell::from("Last run").style(header_style),
        Cell::from("Next evaluation").style(header_style),
    ]);

    let rows: Vec<Row> = conditions
        .iter()
        .enumerate()
        .map(|(i, condition)| {
            let status = match condition.status {
                ConditionStatus::Active => "active",
                ConditionStatus::Paused => "paused",
            };
            let last_run = condition
                .last_run_at
                .map(format_time)
                .unwrap_or_else(|| "\u{2014}".to_string());
            let next = next_evaluation(condition);
            let row = Row::new(vec![
                Cell::from(condition.name.clone()),
                Cell::from(status),
                Cell::from(last_run),
                Cell::from(next),
            ]);
            if i == selected {
                row.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                row
            }
        })
        .collect();

    let widths = [
        Constraint::Percentage(30),
        Constraint::Percentage(15),
        Constraint::Percentage(27),
        Constraint::Percentage(28),
    ];
    let table = Table::new(rows, widths).header(header);
    frame.render_widget(table, area);
}

/// Format a timestamp for the list columns.
fn format_time(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%Y-%m-%d %H:%M").to_string()
}

/// The condition's next scheduled evaluation, as display text:
/// `paused` when paused, `now` when it has never run, otherwise
/// `last_run_at + interval`, or `backoff_until` when that is later.
fn next_evaluation(condition: &Condition) -> String {
    if condition.status == ConditionStatus::Paused {
        return "paused".to_string();
    }
    let Some(last) = condition.last_run_at else {
        return "now".to_string();
    };
    let mut next = last + chrono::Duration::seconds(condition.interval_secs as i64);
    if let Some(backoff) = condition.backoff_until {
        if backoff > next {
            next = backoff;
        }
    }
    format_time(next)
}
