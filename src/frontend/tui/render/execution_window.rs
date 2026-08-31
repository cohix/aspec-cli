//! Execution window rendering: the main output pane (status log or status
//! dashboard), text selection highlighting, and buffer-grid capture for
//! copy support.

use super::*;

/// Render the execution window — rounded border with the phase label as the
/// left-aligned title; border color from `window_border_color(phase, focused)`.
///
/// Body content:
/// - Idle (and the status log is empty): a 3-line welcome stub in DarkGray.
/// - Otherwise: the status log entries, colored per level, with `Wrap{trim:false}`.
///
/// While the container overlay is not Maximized the window's text is mouse-
/// selectable: an active selection is highlighted with `Modifier::REVERSED`,
/// the copy hint is shown on the bottom border, and the inner rect plus the
/// visible text grid are published on the tab so `handle_mouse_event` can
/// start/extend selections (mirrors `render_container_maximized`).
pub(super) fn render_execution_window(app: &mut App, area: Rect, frame: &mut Frame) {
    let tab = app.active_tab();
    let focused = app.focus == Focus::ExecutionWindow;
    // The execution window borrows the ACP identity color in its one green
    // (focused + Done) state when the focused agent slot is an ACP window.
    let acp = tab.focused_slot().is_some_and(|s| s.is_acp());
    let border_color = window_border_color(&tab.execution_phase, focused, acp);
    let title = phase_label(&tab.execution_phase);

    let container_maximized = tab.container_overlay_active();
    // A selection only belongs to this window while the container overlay
    // isn't covering it; when the overlay is active the selection is the
    // overlay's.
    let selection = if container_maximized {
        None
    } else {
        tab.mouse_selection.clone()
    };

    let mut block = Block::default()
        .title(title)
        .title_alignment(Alignment::Left)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color));
    if selection.is_some() {
        block = block.title_bottom(
            Line::from(Span::styled(
                " CTRL-Y to copy/yank text ",
                Style::default().fg(Color::Yellow),
            ))
            .alignment(Alignment::Center),
        );
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Status dashboard takes priority when populated by the status command.
    let has_dashboard = tab
        .status_dashboard
        .lock()
        .map(|d| d.is_some())
        .unwrap_or(false);

    let log_empty = tab
        .status_log
        .lock()
        .map(|log| log.is_empty())
        .unwrap_or(true);

    if has_dashboard {
        render_status_dashboard(tab, inner, frame);
    } else if matches!(tab.execution_phase, ExecutionPhase::Idle) && log_empty {
        // Three-line welcome stub matching old awman exactly.
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  Welcome to awman.",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "  Running `awman ready` to check your environment...",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    } else {
        render_output_content(tab, inner, frame);
    }

    if let Some(ref sel) = selection {
        apply_selection_highlight(frame.buffer_mut(), inner, sel);
    }

    // Publish the inner rect and visible text grid for the mouse handler.
    // When Maximized the overlay covers this window, so clear the rect to
    // keep clicks from starting an execution-window selection underneath.
    let grid = if container_maximized {
        Vec::new()
    } else {
        capture_buffer_grid(frame.buffer_mut(), inner)
    };
    let tab = app.active_tab_mut();
    tab.exec_inner_area = if container_maximized {
        None
    } else {
        Some(inner)
    };
    tab.exec_window_grid = grid;
}

/// Overlay `Modifier::REVERSED` on the cells of an active selection.
/// Selection coordinates are relative to `area` (the window's inner rect).
pub(super) fn apply_selection_highlight(buf: &mut Buffer, area: Rect, sel: &tabs::TextSelection) {
    let norm = Some(container_view::normalize_selection(sel));
    for row in 0..area.height {
        for col in 0..area.width {
            if container_view::cell_in_selection(norm, row, col) {
                if let Some(cell) = buf.cell_mut((area.x + col, area.y + row)) {
                    cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
                }
            }
        }
    }
}

/// Snapshot the rendered cell contents of `area` from the frame buffer into
/// a grid of per-cell strings (empty cells become `" "`), the same shape
/// `capture_vt100_snapshot` produces for the container overlay. Reading back
/// the buffer guarantees the copied text matches what was displayed,
/// including ratatui's word wrapping.
pub(super) fn capture_buffer_grid(buf: &Buffer, area: Rect) -> Vec<Vec<String>> {
    (0..area.height)
        .map(|row| {
            (0..area.width)
                .map(|col| {
                    buf.cell((area.x + col, area.y + row))
                        .map(|c| {
                            let s = c.symbol();
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
}

/// Render the status-log lines into the execution window.
///
/// PTY/container output is rendered exclusively through the container overlay
/// widget (`render_container_maximized` / `render_container_minimized`), never
/// here — that prevents Claude's TUI from bleeding into the execution window.
///
/// Long lines are wrapped (preserving leading whitespace). The visual scroll
/// offset is computed against wrapped row count so `scroll_offset` is in
/// "screen rows", not log entries — matches old amux's behavior where the
/// scroll is anchored to the bottom and increasing offset moves toward older.
fn render_output_content(tab: &tabs::Tab, area: Rect, frame: &mut Frame) {
    let log = match tab.status_log.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if log.is_empty() {
        return;
    }

    if tab.status_log_collapsed {
        let last = &log[log.len() - 1];
        let color = command_box::status_level_color(&last.level);
        let line = Line::from(Span::styled(&last.text, Style::default().fg(color)));
        frame.render_widget(Paragraph::new(vec![line]), area);
        return;
    }

    let lines: Vec<Line> = log
        .iter()
        .map(|entry| {
            let color = command_box::status_level_color(&entry.level);
            Line::from(Span::styled(
                entry.text.as_str(),
                Style::default().fg(color),
            ))
        })
        .collect();

    let inner_height = area.height as usize;
    let inner_width = area.width as usize;
    let total_visual: usize = if inner_width == 0 {
        lines.len()
    } else {
        lines
            .iter()
            .map(|l| {
                let w = l.width();
                if w == 0 {
                    1
                } else {
                    w.div_ceil(inner_width)
                }
            })
            .sum()
    };
    let max_scroll = total_visual.saturating_sub(inner_height);
    let effective_offset = tab.scroll_offset.min(max_scroll);
    let scroll_y = max_scroll.saturating_sub(effective_offset);

    let para = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll_y as u16, 0));
    frame.render_widget(para, area);
}

/// Render the status dashboard as a proper ratatui `Table` widget.
fn render_status_dashboard(tab: &tabs::Tab, area: Rect, frame: &mut Frame) {
    let dash = match tab.status_dashboard.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let data = match dash.as_ref() {
        Some(d) => d,
        None => return,
    };

    let header_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    // Title + empty line above the table.
    let title_height: u16 = 2;
    let tip_height: u16 = 2;
    let table_area_height = area.height.saturating_sub(title_height + tip_height);

    // Split: title row, table, tip row.
    let chunks = Layout::vertical([
        Constraint::Length(title_height),
        Constraint::Length(table_area_height),
        Constraint::Length(tip_height),
    ])
    .split(area);

    // Title.
    let title = Paragraph::new(vec![
        Line::from(Span::styled(
            " AWMAN STATUS DASHBOARD",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ]);
    frame.render_widget(title, chunks[0]);

    if data.containers.is_empty() {
        let empty = Paragraph::new(vec![
            Line::from(Span::styled(
                " No code agents running.",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
            Line::from(Span::styled(
                " To start one:  awman exec workflow <file>  or  awman chat",
                Style::default().fg(Color::DarkGray),
            )),
        ]);
        frame.render_widget(empty, chunks[1]);
    } else {
        let header = Row::new(vec![
            Cell::from(" "),
            Cell::from("NAME").style(header_style),
            Cell::from("CPU").style(header_style),
            Cell::from("MEM").style(header_style),
            Cell::from("IMAGE").style(header_style),
            Cell::from("TAB").style(header_style),
        ])
        .bottom_margin(0);

        let rows: Vec<Row> = data
            .containers
            .iter()
            .map(|c| {
                let indicator_style = if c.stuck {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                };
                let indicator = "\u{25cf}";

                let cpu = c
                    .cpu_percent
                    .map(|v| format!("{v:.1}%"))
                    .unwrap_or_else(|| "-".into());
                let mem = c
                    .memory_mb
                    .map(|v| format!("{v:.0}MB"))
                    .unwrap_or_else(|| "-".into());
                let tab_label = c
                    .tab_number
                    .map(|t| format!("{t}"))
                    .unwrap_or_else(|| "-".into());

                Row::new(vec![
                    Cell::from(indicator).style(indicator_style),
                    Cell::from(c.name.as_str()),
                    Cell::from(cpu),
                    Cell::from(mem),
                    Cell::from(c.image.as_str()),
                    Cell::from(tab_label),
                ])
            })
            .collect();

        // Column widths: start from header label widths, then expand to the
        // widest value in each column, then add 2 chars of trailing padding.
        let pad: usize = 2;
        let mut w_indicator: usize = 1;
        let mut w_name: usize = "NAME".len();
        let mut w_cpu: usize = "CPU".len();
        let mut w_mem: usize = "MEM".len();
        let mut w_image: usize = "IMAGE".len();
        let mut w_tab: usize = "TAB".len();
        for c in &data.containers {
            w_indicator = w_indicator.max(1);
            w_name = w_name.max(c.name.len());
            let cpu = c
                .cpu_percent
                .map(|v| format!("{v:.1}%"))
                .unwrap_or_else(|| "-".into());
            w_cpu = w_cpu.max(cpu.len());
            let mem = c
                .memory_mb
                .map(|v| format!("{v:.0}MB"))
                .unwrap_or_else(|| "-".into());
            w_mem = w_mem.max(mem.len());
            w_image = w_image.max(c.image.len());
            let tab_label = c
                .tab_number
                .map(|t| format!("{t}"))
                .unwrap_or_else(|| "-".into());
            w_tab = w_tab.max(tab_label.len());
        }
        let widths = [
            Constraint::Length((w_indicator + pad) as u16),
            Constraint::Length((w_name + pad) as u16),
            Constraint::Length((w_cpu + pad) as u16),
            Constraint::Length((w_mem + pad) as u16),
            Constraint::Length((w_image + pad) as u16),
            Constraint::Length((w_tab + pad) as u16),
        ];

        let table = Table::new(rows, widths)
            .header(header)
            .row_highlight_style(Style::default());
        frame.render_widget(table, chunks[1]);
    }

    // Tip.
    let tip = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            format!(" Tip: {}", data.tip),
            Style::default().fg(Color::DarkGray),
        )),
    ]);
    frame.render_widget(tip, chunks[2]);
}
