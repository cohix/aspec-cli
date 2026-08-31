//! Status hint bar rendering (the 1-row bar above the command box).

use super::*;

pub(super) fn render_status_bar(app: &App, area: Rect, frame: &mut Frame, sidebar_visible: bool) {
    use crate::frontend::tui::tabs::{ContainerWindowState, ExecutionPhase};

    let tab = app.active_tab();
    let workflow_active = tab
        .workflow_state
        .lock()
        .map(|g| g.is_some())
        .unwrap_or(false);

    let mut spans: Vec<Span> = match (&tab.execution_phase, app.focus, tab.container_window_state) {
        // Running + ExecWindow + Maximized container
        (
            ExecutionPhase::Running { .. },
            Focus::ExecutionWindow,
            ContainerWindowState::Maximized,
        ) => {
            if workflow_active {
                vec![Span::styled(
                    " ctrl-m minimize  \u{00b7}  ctrl-w workflow controls ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )]
            } else {
                vec![Span::styled(
                    " ctrl-m minimize  \u{00b7}  scroll \u{2195} history ",
                    Style::default().fg(Color::Yellow),
                )]
            }
        }
        // Running + ExecWindow + Minimized container
        (
            ExecutionPhase::Running { .. },
            Focus::ExecutionWindow,
            ContainerWindowState::Minimized,
        ) => {
            vec![Span::styled(
                " \u{2191}/\u{2193} scroll  \u{00b7}  b/e jump  \u{00b7}  ctrl-m restore container  \u{00b7}  Esc deselect ",
                Style::default().fg(Color::DarkGray),
            )]
        }
        // Running + ExecWindow + no container
        (ExecutionPhase::Running { .. }, Focus::ExecutionWindow, ContainerWindowState::Hidden) => {
            vec![Span::styled(
                " Press Esc to deselect the window ",
                Style::default().fg(Color::Yellow),
            )]
        }
        // Running + CommandBox
        (ExecutionPhase::Running { .. }, Focus::CommandBox, _) => {
            if workflow_active {
                vec![Span::styled(
                    " Press ctrl-w for workflow controls ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )]
            } else {
                vec![Span::styled(
                    " Press \u{2191} to focus the window ",
                    Style::default().fg(Color::DarkGray),
                )]
            }
        }
        // Done + ExecWindow
        (ExecutionPhase::Done { .. }, Focus::ExecutionWindow, _) => vec![Span::styled(
            " \u{2191}/\u{2193} scroll  \u{00b7}  b/e jump  \u{00b7}  Esc deselect ",
            Style::default().fg(Color::DarkGray),
        )],
        // Done + CommandBox
        (ExecutionPhase::Done { .. }, Focus::CommandBox, _) => vec![Span::styled(
            " Press \u{2191} to focus the window ",
            Style::default().fg(Color::DarkGray),
        )],
        // Error + ExecWindow
        (ExecutionPhase::Error { .. }, Focus::ExecutionWindow, _) => {
            let exit_code = match &tab.execution_phase {
                ExecutionPhase::Error { .. } => -1,
                ExecutionPhase::Done { exit_code, .. } => *exit_code,
                _ => 0,
            };
            vec![
                Span::styled(
                    format!(" Exit code: {} ", exit_code),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " \u{00b7}  \u{2191}/\u{2193} scroll  \u{00b7}  b/e jump  \u{00b7}  Esc deselect ",
                    Style::default().fg(Color::DarkGray),
                ),
            ]
        }
        // Error + CommandBox
        (ExecutionPhase::Error { .. }, Focus::CommandBox, _) => {
            let exit_code = match &tab.execution_phase {
                ExecutionPhase::Error { .. } => -1,
                ExecutionPhase::Done { exit_code, .. } => *exit_code,
                _ => 0,
            };
            vec![
                Span::styled(
                    format!(" Exit code: {} ", exit_code),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " \u{00b7}  Press \u{2191} to focus the window ",
                    Style::default().fg(Color::DarkGray),
                ),
            ]
        }
        // Idle: just the git-sidebar hint.
        _ => vec![Span::styled(
            " \u{00b7} ctrl-g git ",
            Style::default().fg(Color::DarkGray),
        )],
    };

    // A live workflow always has a strip on screen, so advertise the toggle
    // between its one-box-per-stage summary and the full per-step overview.
    if workflow_active {
        let label = if tab.workflow_strip_state.is_expanded() {
            " \u{00b7} ctrl-o collapse steps "
        } else {
            " \u{00b7} ctrl-o step overview "
        };
        spans.push(Span::styled(label, Style::default().fg(Color::DarkGray)));
    }

    // When the sidebar is not visible (closed or collapsed for a narrow
    // terminal), show the compact `+A -D` diff summary at the far right of the
    // 1-row status bar (green `+`, red `-`).
    if !sidebar_visible {
        if let Some(summary) = tab.git_diff_summary.lock().ok().and_then(|g| g.clone()) {
            let git_spans = vec![
                Span::styled(
                    format!("+{}", summary.total_additions),
                    Style::default().fg(Color::Green),
                ),
                Span::raw(" "),
                Span::styled(
                    format!("-{} ", summary.total_deletions),
                    Style::default().fg(Color::Red),
                ),
            ];
            let left_w: usize = spans.iter().map(|s| s.content.chars().count()).sum();
            let git_w: usize = git_spans.iter().map(|s| s.content.chars().count()).sum();
            let total = area.width as usize;
            if total > left_w + git_w {
                spans.push(Span::raw(" ".repeat(total - left_w - git_w)));
                spans.extend(git_spans);
            }
        }
    }

    let bar = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Black));
    frame.render_widget(bar, area);
}
