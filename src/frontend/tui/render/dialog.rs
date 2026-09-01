//! Modal dialog rendering: dispatch over the `Dialog` enum plus the
//! ConfigShow table widget and its cursor-windowing helper.

use super::*;

/// Render the currently active dialog.
pub(super) fn render_dialog(dialog: &dialogs::Dialog, area: Rect, frame: &mut Frame) {
    match dialog {
        dialogs::Dialog::QuitConfirm => {
            dialogs::render_quit_confirm(area, frame);
        }
        dialogs::Dialog::CloseTabConfirm => {
            dialogs::render_close_tab_confirm(area, frame);
        }
        dialogs::Dialog::WorkflowCancelConfirm => {
            dialogs::render_workflow_cancel_confirm(area, frame);
        }
        dialogs::Dialog::YesNo { title, body } => {
            dialogs::render_yes_no(title, body, area, frame);
        }
        dialogs::Dialog::YesNoCancel { title, body } => {
            // Same dynamic sizing as render_yes_no, plus an explicit Cancel.
            let max_w = area.width.saturating_sub(6).max(40);
            let max_body_w = body
                .lines()
                .map(unicode_width::UnicodeWidthStr::width)
                .max()
                .unwrap_or(0) as u16;
            let title_w = unicode_width::UnicodeWidthStr::width(title.as_str()) as u16 + 4;
            let width = max_body_w.saturating_add(6).max(50).max(title_w).min(max_w);
            let inner_w = width.saturating_sub(4) as usize;
            let wrapped_lines: usize = body
                .lines()
                .map(|line| {
                    let w = unicode_width::UnicodeWidthStr::width(line);
                    if inner_w == 0 || w == 0 {
                        1
                    } else {
                        w.div_ceil(inner_w)
                    }
                })
                .sum();
            let body_h = wrapped_lines as u16;
            let height = (body_h + 5).min(area.height.saturating_sub(2)).max(7);
            let dialog_area = dialogs::centered_fixed(width, height, area);
            let inner = dialogs::render_dialog_frame(title, Color::Yellow, dialog_area, frame);
            let text = format!("{body}\n\n  [y] Yes   [n] No   [Esc] Cancel");
            frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
        }
        dialogs::Dialog::TextInput {
            title,
            prompt,
            editor,
        } => {
            // Layout: prompt (multi-line) + spacer + bordered input + spacer +
            // hint row. Width grows with terminal but caps at 80.
            let prompt_lines = prompt.lines().count() as u16;
            let dialog_h = prompt_lines + 9;
            let dialog_w = (area.width.saturating_sub(8)).clamp(50, 80);
            let dialog_area = dialogs::centered_fixed(dialog_w, dialog_h, area);
            let inner = dialogs::render_dialog_frame(title, Color::Cyan, dialog_area, frame);
            let prompt_area = Rect {
                height: prompt_lines,
                ..inner
            };
            frame.render_widget(
                Paragraph::new(prompt.as_str()).style(Style::default().fg(Color::Gray)),
                prompt_area,
            );
            let input_area = Rect {
                y: inner.y + prompt_lines + 1,
                height: 3,
                ..inner
            };
            let input_block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan));
            let input_inner = input_block.inner(input_area);
            frame.render_widget(input_block, input_area);
            let display_text: String = editor
                .text
                .chars()
                .take(input_inner.width as usize)
                .collect();
            frame.render_widget(
                Paragraph::new(display_text).style(Style::default().fg(Color::White)),
                input_inner,
            );
            // Hint row below the input.
            let hint_y = input_area.y + input_area.height + 1;
            if hint_y < inner.y + inner.height {
                let hint_area = Rect {
                    y: hint_y,
                    height: 1,
                    ..inner
                };
                frame.render_widget(
                    Paragraph::new("  [Enter] submit   [Esc] cancel")
                        .style(Style::default().fg(Color::DarkGray)),
                    hint_area,
                );
            }
            let text_before_cursor = &editor.text[..editor.cursor];
            let cursor_display_w = unicode_width::UnicodeWidthStr::width(text_before_cursor) as u16;
            let cursor_x =
                input_inner.x + cursor_display_w.min(input_inner.width.saturating_sub(1));
            let cursor_y = input_inner.y;
            if cursor_x < input_inner.x + input_inner.width {
                frame.set_cursor_position(Position::new(cursor_x, cursor_y));
            }
        }
        dialogs::Dialog::MultilineInput {
            title,
            prompt,
            editor,
        } => {
            let dialog_area = dialogs::centered_rect(70, 60, area);
            let inner = dialogs::render_dialog_frame(title, Color::Cyan, dialog_area, frame);

            // Layout: prompt lines, 1-row gap, bordered textarea, 1-row gap, hint.
            let prompt_lines = prompt.lines().count() as u16;
            let prompt_area = Rect {
                height: prompt_lines,
                ..inner
            };
            frame.render_widget(
                Paragraph::new(prompt.as_str()).style(Style::default().fg(Color::Gray)),
                prompt_area,
            );

            // Textarea with a visible border.
            let textarea_y = inner.y + prompt_lines + 1;
            let hint_reserve: u16 = 2; // 1-row gap + 1-row hint
            let textarea_h = inner
                .height
                .saturating_sub(prompt_lines + 1 + hint_reserve)
                .max(3);
            let textarea_area = Rect {
                x: inner.x,
                y: textarea_y,
                width: inner.width,
                height: textarea_h,
            };
            let textarea_block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan));
            let textarea_inner = textarea_block.inner(textarea_area);
            frame.render_widget(textarea_block, textarea_area);

            // Render editor text inside the bordered textarea with wrapping.
            let inner_w = textarea_inner.width as usize;
            let inner_h = textarea_inner.height as usize;

            // Compute visual lines from the editor text (split by '\n', then
            // wrap each logical line at inner_w).
            let logical_lines: Vec<&str> = editor.text.split('\n').collect();
            let mut visual_lines: Vec<String> = Vec::new();
            for line in &logical_lines {
                if line.is_empty() {
                    visual_lines.push(String::new());
                } else if inner_w == 0 {
                    visual_lines.push(line.to_string());
                } else {
                    let chars: Vec<char> = line.chars().collect();
                    for chunk in chars.chunks(inner_w) {
                        visual_lines.push(chunk.iter().collect());
                    }
                }
            }

            // Compute cursor position in visual-line space.
            let text_before_cursor = &editor.text[..editor.cursor];
            let cursor_logical: Vec<&str> = text_before_cursor.split('\n').collect();
            let cursor_last_line = cursor_logical.last().unwrap_or(&"");
            let cursor_col_chars = cursor_last_line.chars().count();
            let mut cursor_visual_row: usize = 0;
            // Walk logical lines before the cursor line.
            for (i, line) in logical_lines.iter().enumerate() {
                if i >= cursor_logical.len() - 1 {
                    break;
                }
                let line_chars = line.chars().count();
                if line_chars == 0 || inner_w == 0 {
                    cursor_visual_row += 1;
                } else {
                    cursor_visual_row += line_chars.div_ceil(inner_w);
                }
            }
            // Add wrapped rows from the current logical line.
            if inner_w > 0 && cursor_col_chars > 0 {
                cursor_visual_row += cursor_col_chars / inner_w;
            }
            let cursor_visual_col = if inner_w > 0 {
                cursor_col_chars % inner_w
            } else {
                cursor_col_chars
            };

            // Scroll to keep cursor visible.
            let scroll_offset = if cursor_visual_row >= inner_h {
                cursor_visual_row - inner_h + 1
            } else {
                0
            };

            // Render visible lines.
            let visible: Vec<Line> = visual_lines
                .iter()
                .skip(scroll_offset)
                .take(inner_h)
                .map(|s| Line::from(s.as_str()))
                .collect();
            frame.render_widget(
                Paragraph::new(visible).style(Style::default().fg(Color::White)),
                textarea_inner,
            );

            // Hint row below the textarea.
            let hint_y = textarea_area.y + textarea_area.height + 1;
            if hint_y < inner.y + inner.height {
                let hint_area = Rect {
                    y: hint_y,
                    height: 1,
                    ..inner
                };
                frame.render_widget(
                    Paragraph::new(
                        "  [Ctrl+Enter / Ctrl+S] submit   [Enter] newline   [Esc] cancel",
                    )
                    .style(Style::default().fg(Color::DarkGray)),
                    hint_area,
                );
            }

            // Place the cursor at the correct visual position.
            let display_row = cursor_visual_row.saturating_sub(scroll_offset);
            let cx = textarea_inner.x
                + (cursor_visual_col as u16).min(textarea_inner.width.saturating_sub(1));
            let cy = textarea_inner.y + display_row as u16;
            if cx < textarea_inner.x + textarea_inner.width
                && cy < textarea_inner.y + textarea_inner.height
            {
                frame.set_cursor_position(Position::new(cx, cy));
            }
        }
        dialogs::Dialog::ListPicker {
            title,
            items,
            selected,
        } => {
            // Width fits the longest item plus margin/prefix; height fits up
            // to all items plus a hint, capped to the terminal area.
            let max_item_w = items
                .iter()
                .map(|s| unicode_width::UnicodeWidthStr::width(s.as_str()))
                .max()
                .unwrap_or(0) as u16;
            let title_w = unicode_width::UnicodeWidthStr::width(title.as_str()) as u16 + 4;
            let width = (max_item_w + 8)
                .max(title_w)
                .max(50)
                .min(area.width.saturating_sub(4));
            let body_h = items.len() as u16 + 1; // +1 for the hint row
            let height = (body_h + 4).min(area.height.saturating_sub(2)).max(7);
            let dialog_area = dialogs::centered_fixed(width, height, area);
            let inner = dialogs::render_dialog_frame(title, Color::Cyan, dialog_area, frame);
            // Reserve last row for the hint.
            let list_h = inner.height.saturating_sub(1);
            let list_area = Rect {
                height: list_h,
                ..inner
            };
            // Window items so the selection stays visible when the list is
            // taller than the dialog.
            let visible = list_h as usize;
            let start = selected
                .saturating_sub(visible.saturating_sub(1))
                .min(items.len().saturating_sub(visible).max(0));
            let lines: Vec<Line> = items
                .iter()
                .enumerate()
                .skip(start)
                .take(visible)
                .map(|(i, item)| {
                    let prefix = if i == *selected { "▸ " } else { "  " };
                    let style = if i == *selected {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    Line::from(Span::styled(format!("{prefix}{item}"), style))
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), list_area);
            let hint_area = Rect {
                y: inner.y + list_h,
                height: 1,
                ..inner
            };
            frame.render_widget(
                Paragraph::new("  [↑/↓] navigate   [Enter] select   [Esc] cancel")
                    .style(Style::default().fg(Color::DarkGray)),
                hint_area,
            );
        }
        dialogs::Dialog::KindSelect { title, options } => {
            let max_label_w = options
                .iter()
                .map(|(_k, l)| unicode_width::UnicodeWidthStr::width(l.as_str()))
                .max()
                .unwrap_or(0) as u16;
            let title_w = unicode_width::UnicodeWidthStr::width(title.as_str()) as u16 + 4;
            let width = (max_label_w + 12)
                .max(title_w)
                .max(50)
                .min(area.width.saturating_sub(4));
            let body_h = options.len() as u16 + 1; // +1 for hint
            let height = (body_h + 4).min(area.height.saturating_sub(2)).max(7);
            let dialog_area = dialogs::centered_fixed(width, height, area);
            let inner = dialogs::render_dialog_frame(title, Color::Yellow, dialog_area, frame);
            let mut lines: Vec<Line> = options
                .iter()
                .enumerate()
                .map(|(i, (_key, label))| Line::from(format!("  [{}] {label}", i + 1)))
                .collect();
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  [1-9] select   [Esc] cancel",
                Style::default().fg(Color::DarkGray),
            )));
            frame.render_widget(Paragraph::new(lines), inner);
        }
        dialogs::Dialog::WorkflowControlBoard(state) => {
            let extra_reasons = [
                state.continue_unavailable_reason.is_some(),
                state.cancel_to_previous_unavailable_reason.is_some(),
                state.finish_workflow_unavailable_reason.is_some(),
                state.restart_unavailable_reason.is_some(),
            ]
            .iter()
            .filter(|x| **x)
            .count() as u16;
            let base_height: u16 = if state.can_finish { 14 } else { 12 };
            // Width fits the longest reason line (+ left margin) when present;
            // otherwise the diamond layout's natural minimum is comfortable.
            let max_reason_w = [
                state.continue_unavailable_reason.as_deref(),
                state.cancel_to_previous_unavailable_reason.as_deref(),
                state.finish_workflow_unavailable_reason.as_deref(),
                state.restart_unavailable_reason.as_deref(),
            ]
            .into_iter()
            .flatten()
            .map(|s| unicode_width::UnicodeWidthStr::width(s) + 15)
            .max()
            .unwrap_or(0) as u16;
            let step_w =
                unicode_width::UnicodeWidthStr::width(state.step_name.as_str()) as u16 + 10;
            let width = max_reason_w
                .max(step_w)
                .max(56)
                .min(area.width.saturating_sub(4));
            let dialog_area = dialogs::centered_fixed(width, base_height + extra_reasons, area);
            let title = if state.can_dismiss {
                "Workflow Control (step running)"
            } else {
                "Workflow Control"
            };
            let inner = dialogs::render_dialog_frame(title, Color::Yellow, dialog_area, frame);

            let arrow_style = Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD);
            let label_style = Style::default().fg(Color::White);
            let dimmed_style = Style::default().fg(Color::DarkGray);
            let step_style = Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD);
            let (right_arrow_style, right_label_style) = if state.can_launch_next {
                (arrow_style, label_style)
            } else {
                (dimmed_style, dimmed_style)
            };
            let (down_arrow_style, down_label_style) = if state.can_continue_current {
                (arrow_style, label_style)
            } else {
                (dimmed_style, dimmed_style)
            };
            let (left_arrow_style, left_label_style) = if state.can_go_back {
                (arrow_style, label_style)
            } else {
                (dimmed_style, dimmed_style)
            };
            // Restart is disabled in a parallel group unless this is the
            // focused container (WI-0096 §10).
            let restart_disabled = state.restart_unavailable_reason.is_some() || !state.can_restart;
            let (up_arrow_style, up_label_style) = if restart_disabled {
                (dimmed_style, dimmed_style)
            } else {
                (arrow_style, label_style)
            };

            let mut lines: Vec<Line> = vec![
                Line::from(vec![
                    Span::raw(" Step: "),
                    Span::styled(&state.step_name, step_style),
                ]),
                Line::from(""),
                // ↑ Restart (top of diamond)
                Line::from(vec![
                    Span::raw("         "),
                    Span::styled("\u{2191}", up_arrow_style),
                    Span::styled(" Restart current step", up_label_style),
                ]),
            ];
            if let Some(ref reason) = state.restart_unavailable_reason {
                lines.push(Line::from(Span::styled(
                    format!("           {reason}"),
                    dimmed_style,
                )));
            }
            lines.push(Line::from(""));
            lines.extend([
                // ← Cancel to prev    → Next: new container
                Line::from(vec![
                    Span::styled("\u{2190}", left_arrow_style),
                    Span::styled(" Cancel to prev", left_label_style),
                    Span::raw("   "),
                    Span::styled("\u{2192}", right_arrow_style),
                    Span::styled(
                        format!(
                            " {}",
                            state
                                .launch_next_label
                                .as_deref()
                                .unwrap_or("Next: new container")
                        ),
                        right_label_style,
                    ),
                ]),
                Line::from(""),
                // ↓ Next: same container (bottom of diamond)
                Line::from(vec![
                    Span::raw("         "),
                    Span::styled("\u{2193}", down_arrow_style),
                    Span::styled(" Next: same container", down_label_style),
                ]),
            ]);
            if let Some(ref reason) = state.continue_unavailable_reason {
                lines.push(Line::from(Span::styled(
                    format!("           {reason}"),
                    dimmed_style,
                )));
            } else {
                lines.push(Line::from(""));
            }
            if state.can_finish {
                lines.push(Line::from(""));
                let finish_style = Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD);
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled("[Enter]", finish_style),
                    Span::styled(" Finish workflow", finish_style),
                ]));
            }
            lines.push(Line::from(""));
            if state.can_dismiss {
                lines.push(Line::from(Span::styled(
                    "  [^C] Abort   [p] Pause   [Esc] Dismiss",
                    dimmed_style,
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    "  [^C] Abort   [Esc] Pause",
                    dimmed_style,
                )));
            }
            frame.render_widget(Paragraph::new(lines), inner);
        }
        dialogs::Dialog::WorkflowStepError(state) => {
            let max_err_w = state
                .error_lines
                .iter()
                .map(|l| unicode_width::UnicodeWidthStr::width(l.as_str()))
                .max()
                .unwrap_or(0) as u16;
            let step_w =
                unicode_width::UnicodeWidthStr::width(state.step_name.as_str()) as u16 + 10; // "  Step: " prefix.
            let width = max_err_w
                .max(step_w)
                .saturating_add(6)
                .max(60)
                .min(area.width.saturating_sub(4));
            let height = (state.error_lines.len() as u16 + 8)
                .min(area.height.saturating_sub(4))
                .max(9);
            let dialog_area = dialogs::centered_fixed(width, height, area);
            let inner = dialogs::render_dialog_frame("Step failed", Color::Red, dialog_area, frame);
            let mut lines = vec![
                Line::from(format!("  Step: {}", state.step_name)),
                Line::from(""),
            ];
            for line in &state.error_lines {
                lines.push(Line::from(Span::styled(
                    format!("  {line}"),
                    Style::default().fg(Color::Red),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  [r] Retry   [q/Esc] Pause   [a] Abort",
                Style::default().fg(Color::DarkGray),
            )));
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        }
        dialogs::Dialog::WorkflowYoloCountdown(state) => {
            let emoji = if state.remaining_secs % 2 == 0 {
                "\u{26a0}\u{fe0f}"
            } else {
                "\u{1f918}"
            };
            let title = format!("{} Yolo in {}s", emoji, state.remaining_secs);
            let step_w = unicode_width::UnicodeWidthStr::width(state.step_name.as_str()) as u16;
            let width = step_w
                .saturating_add(20)
                .max(56)
                .min(area.width.saturating_sub(4));
            let dialog_area = dialogs::centered_fixed(width, 9, area);
            let inner = dialogs::render_dialog_frame(&title, Color::Magenta, dialog_area, frame);
            let text = format!(
                "  Step: {}\n  Auto-advancing in {}s\n\n  [Esc] Cancel   [Ctrl-W] Control board",
                state.step_name, state.remaining_secs
            );
            frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
        }
        dialogs::Dialog::AgentSetup(state) => {
            let title = if state.image_only {
                format!("Build {} image?", state.agent_name)
            } else {
                format!("Set up {}?", state.agent_name)
            };
            let title_w = unicode_width::UnicodeWidthStr::width(title.as_str()) as u16 + 4;
            let fallback_w = state
                .fallback_name
                .as_deref()
                .map(unicode_width::UnicodeWidthStr::width)
                .unwrap_or(0) as u16
                + 22;
            let width = title_w
                .max(fallback_w)
                .max(55)
                .min(area.width.saturating_sub(4));
            let height = if state.has_fallback && state.fallback_name.is_some() {
                10
            } else {
                9
            };
            let dialog_area = dialogs::centered_fixed(width, height, area);
            let inner = dialogs::render_dialog_frame(&title, Color::Yellow, dialog_area, frame);
            let mut lines = vec![Line::from(""), Line::from("  [y] Yes   [n] No")];
            if state.has_fallback {
                if let Some(ref fb) = state.fallback_name {
                    lines.push(Line::from(format!("  [f] Fallback to {fb}")));
                }
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  [Esc] Abort",
                Style::default().fg(Color::DarkGray),
            )));
            frame.render_widget(Paragraph::new(lines), inner);
        }
        dialogs::Dialog::MountScope(state) => {
            // Paths can be long — auto-grow to fit, but cap to area.
            let path_w = unicode_width::UnicodeWidthStr::width(state.git_root.as_str())
                .max(unicode_width::UnicodeWidthStr::width(state.cwd.as_str()))
                as u16
                + 14; // "  Git root: " / "  CWD:      " prefixes.
            let width = path_w.max(60).min(area.width.saturating_sub(4));
            let dialog_area = dialogs::centered_fixed(width, 11, area);
            let inner =
                dialogs::render_dialog_frame("Mount Scope", Color::Yellow, dialog_area, frame);
            let lines: Vec<Line> = vec![
                Line::from(format!("  Git root: {}", state.git_root)),
                Line::from(format!("  CWD:      {}", state.cwd)),
                Line::from(""),
                Line::from("  [r] Mount git root"),
                Line::from("  [c] Mount current dir only"),
                Line::from(""),
                Line::from(Span::styled(
                    "  [a / Esc] Abort",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        }
        dialogs::Dialog::AgentAuth(state) => {
            let max_var_w = state
                .env_vars
                .iter()
                .map(|s| unicode_width::UnicodeWidthStr::width(s.as_str()))
                .max()
                .unwrap_or(0) as u16
                + 8;
            let agent_w =
                unicode_width::UnicodeWidthStr::width(state.agent_name.as_str()) as u16 + 12;
            let width = max_var_w
                .max(agent_w)
                .max(55)
                .min(area.width.saturating_sub(4));
            let height = (state.env_vars.len() as u16 + 8)
                .min(area.height.saturating_sub(4))
                .max(9);
            let dialog_area = dialogs::centered_fixed(width, height, area);
            let inner = dialogs::render_dialog_frame(
                "Agent credentials?",
                Color::Yellow,
                dialog_area,
                frame,
            );
            let mut lines = vec![
                Line::from(format!("  Agent: {}", state.agent_name)),
                Line::from("  Env vars to inject:"),
            ];
            for var in &state.env_vars {
                lines.push(Line::from(format!("    - {var}")));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  [y] Accept   [n] Decline   [o] Decline once   [Esc] cancel",
                Style::default().fg(Color::DarkGray),
            )));
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        }
        dialogs::Dialog::ConfigShow(state) => {
            render_config_show(state, area, frame);
        }
        dialogs::Dialog::SquadTaskDetail(state) => {
            render_squad_detail(state, area, frame);
        }
        dialogs::Dialog::SquadRemoveConfirm { name } => {
            let width = 60u16.min(area.width.saturating_sub(4).max(40));
            let dialog_area = dialogs::centered_fixed(width, 8, area);
            let inner =
                dialogs::render_dialog_frame("Remove task", Color::Yellow, dialog_area, frame);
            let text = format!("  Remove task \"{name}\"?\n\n  [y] remove   [n / Esc] cancel");
            frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
        }
        dialogs::Dialog::Loading { title } => {
            let title_w = unicode_width::UnicodeWidthStr::width(title.as_str()) as u16 + 4;
            let width = title_w.max(40).min(area.width.saturating_sub(4));
            let dialog_area = dialogs::centered_fixed(width, 6, area);
            let inner = dialogs::render_dialog_frame(title, Color::Cyan, dialog_area, frame);
            frame.render_widget(
                Paragraph::new("  Loading...").style(Style::default().fg(Color::DarkGray)),
                inner,
            );
        }
        dialogs::Dialog::WorkflowStepConfirm(state) => {
            let body_w = unicode_width::UnicodeWidthStr::width(
                format!(
                    "  Step '{}' done. Advance to '{}'?",
                    state.completed_step, state.next_step
                )
                .as_str(),
            ) as u16
                + 4;
            let width = body_w.max(64).min(area.width.saturating_sub(4));
            let dialog_area = dialogs::centered_fixed(width, 8, area);
            let inner =
                dialogs::render_dialog_frame("Step Complete", Color::Green, dialog_area, frame);
            let lines = vec![
                Line::from(format!(
                    "  Step '{}' done. Advance to '{}'?",
                    state.completed_step, state.next_step
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  [Enter] yes   [Esc] pause   [Ctrl+W] full control board",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        }
        dialogs::Dialog::Custom { title, body, keys } => {
            let body_lines = body.lines().count() as u16;
            let title_w = unicode_width::UnicodeWidthStr::width(title.as_str()) as u16 + 4;
            // Use display width, not byte length, so wide chars/emoji size
            // the dialog correctly. Account for padding + borders.
            let max_body_width = body
                .lines()
                .map(unicode_width::UnicodeWidthStr::width)
                .max()
                .unwrap_or(40) as u16;
            let max_key_label_width = keys
                .iter()
                .map(|(_, l)| unicode_width::UnicodeWidthStr::width(l.as_str()) + 6)
                .max()
                .unwrap_or(0) as u16;
            let width = max_body_width
                .max(max_key_label_width)
                .max(title_w)
                .saturating_add(6)
                .clamp(55, area.width.saturating_sub(4));
            let height = (keys.len() as u16 + body_lines + 7)
                .min(area.height.saturating_sub(2))
                .max(9);
            let dialog_area = dialogs::centered_fixed(width, height, area);
            let inner = dialogs::render_dialog_frame(title, Color::Yellow, dialog_area, frame);
            let mut lines: Vec<Line> = body.lines().map(Line::from).collect();
            lines.push(Line::from(""));
            for (ch, label) in keys {
                lines.push(Line::from(format!("  [{ch}] {label}")));
            }
            // Always offer an Esc hint at the bottom — Custom is also used
            // for prompts where the natural cancel key is Esc.
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  [Esc] cancel",
                Style::default().fg(Color::DarkGray),
            )));
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        }
        dialogs::Dialog::FatalError { title, body } => {
            let body_lines = body.lines().count() as u16;
            let title_w = unicode_width::UnicodeWidthStr::width(title.as_str()) as u16 + 4;
            let max_body_width = body
                .lines()
                .map(unicode_width::UnicodeWidthStr::width)
                .max()
                .unwrap_or(40) as u16;
            let width = max_body_width
                .max(title_w)
                .saturating_add(6)
                .clamp(55, area.width.saturating_sub(4));
            let height = (body_lines + 6).min(area.height.saturating_sub(2)).max(8);
            let dialog_area = dialogs::centered_fixed(width, height, area);
            let inner = dialogs::render_dialog_frame(title, Color::Red, dialog_area, frame);
            let mut lines: Vec<Line> = body.lines().map(Line::from).collect();
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  [Enter] quit",
                Style::default().fg(Color::DarkGray),
            )));
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        }
        dialogs::Dialog::Notice { title, body } => {
            let body_lines = body.lines().count() as u16;
            let title_w = unicode_width::UnicodeWidthStr::width(title.as_str()) as u16 + 4;
            // The squad key snippet contains a box-drawn banner and an indented
            // export line; size to the widest line so neither wraps.
            let max_body_width = body
                .lines()
                .map(unicode_width::UnicodeWidthStr::width)
                .max()
                .unwrap_or(40) as u16;
            let width = max_body_width
                .max(title_w)
                .saturating_add(6)
                .clamp(55, area.width.saturating_sub(4));
            let height = (body_lines + 6).min(area.height.saturating_sub(2)).max(8);
            let dialog_area = dialogs::centered_fixed(width, height, area);
            let inner = dialogs::render_dialog_frame(title, Color::Yellow, dialog_area, frame);
            let mut lines: Vec<Line> = body.lines().map(Line::from).collect();
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  [Enter] dismiss",
                Style::default().fg(Color::DarkGray),
            )));
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        }
    }
}

/// Render the squad task-detail modal (WI 0102): the field block plus the
/// scrollable run-history table. Reads only the dialog state, which
/// `tick_all_tabs` keeps in sync with the squad tab's snapshot.
fn render_squad_detail(state: &dialogs::SquadDetailState, area: Rect, frame: &mut Frame) {
    use crate::data::fs::task_store::{MountScope, TaskStatus};

    let width = area.width.saturating_sub(6).clamp(50, 90);
    let height = area.height.saturating_sub(4).clamp(12, 30);
    let dialog_area = dialogs::centered_fixed(width, height, area);
    let title = format!("task: {}", state.name);
    let inner = dialogs::render_dialog_frame(&title, Color::Cyan, dialog_area, frame);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let c = &state.task;
    let mount = match c.mount_scope {
        MountScope::Cwd => "cwd",
        MountScope::GitRoot => "gitroot",
        MountScope::Directory => "directory (no worktree)",
    };
    let status = match c.status {
        TaskStatus::Active => "active",
        TaskStatus::Paused => "paused",
    };
    // The description is free text and often longer than the modal is wide,
    // so it renders as its own wrapped multi-line block rather than a single
    // clipped `label: value` line. Capped at half the modal so a very long
    // description can never squeeze out the run history.
    let description_cap = (inner.height / 2).max(1) as usize;
    let mut fields: Vec<Line> =
        squad_description_lines(&c.description, inner.width as usize, description_cap);
    fields.extend(vec![
        squad_field_line("Status", status),
        squad_field_line("Mount scope", mount),
        squad_field_line(
            "Interval",
            &crate::frontend::tui::tabs::format_duration(c.interval_secs),
        ),
        squad_field_line("Agent", c.agent.as_deref().unwrap_or("(default)")),
        squad_field_line("Model", c.model.as_deref().unwrap_or("(default)")),
        squad_field_line("Workspace", &c.repo_scope.display().to_string()),
        squad_field_line(
            "Worktree",
            if c.uses_worktree() {
                "yes"
            } else {
                "no (mounted directly)"
            },
        ),
        squad_field_line(
            "Overlays",
            &if c.overlays.is_empty() {
                "(none)".to_string()
            } else {
                c.overlays.join(", ")
            },
        ),
        squad_field_line(
            "Created",
            &c.created_at.format("%Y-%m-%d %H:%M").to_string(),
        ),
        squad_field_line(
            "Updated",
            &c.updated_at.format("%Y-%m-%d %H:%M").to_string(),
        ),
    ]);
    let field_h = fields.len() as u16;

    let chunks = Layout::vertical([
        Constraint::Length(field_h),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);
    frame.render_widget(Paragraph::new(fields), chunks[0]);
    frame.render_widget(
        Paragraph::new(Span::styled(
            "Run history",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        chunks[1],
    );
    render_squad_run_history(&state.runs, state.scroll, chunks[2], frame);

    // WI 0106 Part 5: the action tooltip — the same per-task actions the list
    // view's footer hints at, scoped to this modal's task and actually wired
    // up (`dialog_router::handle_dialog_char`) so a user doesn't have to
    // close the modal to attach/pause/resume/remove.
    frame.render_widget(
        Paragraph::new(Span::styled(
            "a attach \u{b7} p pause \u{b7} r resume \u{b7} d delete \u{b7} esc close",
            Style::default().fg(Color::DarkGray),
        )),
        chunks[3],
    );
}

/// A `label: value` line for the detail modal's field block.
fn squad_field_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}: "), Style::default().fg(Color::DarkGray)),
        Span::raw(value.to_string()),
    ])
}

/// The detail modal's description block: the label line followed by the full
/// description word-wrapped to `width`, indented two cells, and capped at
/// `max_lines` total (an ellipsis line marks a capped description).
fn squad_description_lines(
    description: &str,
    width: usize,
    max_lines: usize,
) -> Vec<Line<'static>> {
    const INDENT: &str = "  ";
    let wrap_width = width.saturating_sub(INDENT.len()).max(1);
    let mut lines = vec![Line::from(Span::styled(
        "Description:",
        Style::default().fg(Color::DarkGray),
    ))];
    let mut wrapped: Vec<String> = description
        .lines()
        .flat_map(|line| wrap_display_width(line, wrap_width))
        .collect();
    if wrapped.is_empty() {
        wrapped.push(String::new());
    }
    let cap = max_lines.saturating_sub(1).max(1);
    let truncated = wrapped.len() > cap;
    wrapped.truncate(cap);
    if truncated {
        if let Some(last) = wrapped.last_mut() {
            *last = format!("{last}\u{2026}");
        }
    }
    lines.extend(
        wrapped
            .into_iter()
            .map(|line| Line::from(Span::raw(format!("{INDENT}{line}")))),
    );
    lines
}

/// Greedy word wrap by display width. A single word wider than `width` is
/// split mid-word rather than overflowing the modal.
fn wrap_display_width(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    use unicode_width::UnicodeWidthStr;

    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_w = 0usize;
    for word in text.split_whitespace() {
        let word_w = UnicodeWidthStr::width(word);
        let sep_w = if current.is_empty() { 0 } else { 1 };
        if current_w + sep_w + word_w <= width {
            if sep_w == 1 {
                current.push(' ');
            }
            current.push_str(word);
            current_w += sep_w + word_w;
            continue;
        }
        if !current.is_empty() {
            out.push(std::mem::take(&mut current));
            current_w = 0;
        }
        if word_w <= width {
            current.push_str(word);
            current_w = word_w;
        } else {
            // Split an over-wide word across as many lines as it needs.
            for ch in word.chars() {
                let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0);
                if current_w + ch_w > width && !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                    current_w = 0;
                }
                current.push(ch);
                current_w += ch_w;
            }
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// The run-history table (Started | Status | Finished | Error), scrolled by
/// `scroll` rows.
fn render_squad_run_history(
    runs: &[crate::data::fs::task_store::Run],
    scroll: usize,
    area: Rect,
    frame: &mut Frame,
) {
    use crate::data::fs::task_store::RunStatus;
    let header_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let header = Row::new(vec![
        Cell::from("Started").style(header_style),
        Cell::from("Status").style(header_style),
        Cell::from("Finished").style(header_style),
        Cell::from("Error").style(header_style),
    ]);
    let rows: Vec<Row> = runs
        .iter()
        .skip(scroll)
        .map(|r| {
            let status = match r.status {
                RunStatus::Running => "running",
                RunStatus::NotTriggered => "not triggered",
                RunStatus::WorkflowExecuted => "executed",
                RunStatus::Failed => "failed",
                RunStatus::Interrupted => "interrupted",
            };
            let started = r.started_at.format("%Y-%m-%d %H:%M").to_string();
            let finished = r
                .finished_at
                .map(|f| f.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "\u{2014}".to_string());
            let error = r.error.clone().unwrap_or_default();
            Row::new(vec![
                Cell::from(started),
                Cell::from(status),
                Cell::from(finished),
                Cell::from(error),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(17),
        Constraint::Length(14),
        Constraint::Length(17),
        Constraint::Min(6),
    ];
    let table = Table::new(rows, widths).header(header);
    frame.render_widget(table, area);
}

/// Render `text` with a visible `|` cursor at byte offset `cursor`, windowed
/// to at most `max` characters so the cursor never scrolls out of a narrow
/// table cell. `…` marks clipped content on either side.
pub(super) fn cursor_window(text: &str, cursor: usize, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let cursor = cursor.min(text.len());
    let mut with_cursor = String::with_capacity(text.len() + 1);
    with_cursor.push_str(&text[..cursor]);
    with_cursor.push('|');
    with_cursor.push_str(&text[cursor..]);
    let chars: Vec<char> = with_cursor.chars().collect();
    if chars.len() <= max {
        return with_cursor;
    }
    let cursor_idx = text[..cursor].chars().count();
    // Keep the cursor visible with a char of context to its right.
    let start = cursor_idx
        .saturating_sub(max.saturating_sub(2))
        .min(chars.len() - max);
    let mut window: Vec<char> = chars[start..start + max].to_vec();
    if start > 0 {
        window[0] = '\u{2026}';
    }
    if start + max < chars.len() {
        let last = window.len() - 1;
        window[last] = '\u{2026}';
    }
    window.into_iter().collect()
}

/// Render the config show dialog using a Ratatui `Table` widget.
///
/// The popup takes 90% of the terminal in both dimensions. The bottom pane
/// shows the full (wrapped) value of the selected row — or the inline editor
/// or the Ctrl+N add-mapping prompt — so long values are never lost to cell
/// truncation.
pub(super) fn render_config_show(state: &dialogs::ConfigShowState, area: Rect, frame: &mut Frame) {
    let popup = dialogs::centered_rect(90, 90, area);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" awman config ")
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // ── Bottom pane content, computed first so its height can flex ──────
    let editing_value = state.editing && state.new_entry.is_none();
    let edit_buffer = || {
        let text = &state.editor.text;
        let cursor = state.editor.cursor.min(text.len());
        format!("{}|{}", &text[..cursor], &text[cursor..])
    };
    let (detail_text, format_hint): (String, Option<String>) = match &state.new_entry {
        Some(dialogs::NewMapEntryPhase::Key) => (
            format!("  New model mapping \u{2014} agent name: {}", edit_buffer()),
            Some("ASCII letters, digits, '-' and '_'".to_string()),
        ),
        Some(dialogs::NewMapEntryPhase::Value { key }) => (
            format!(
                "  dynamicWorkflows.agentsToModels.{key} = {}",
                edit_buffer()
            ),
            Some("comma-separated model names".to_string()),
        ),
        Some(dialogs::NewMapEntryPhase::GuidanceEntry) => (
            format!("  New guidance entry: {}", edit_buffer()),
            Some("a single instruction the leader must follow".to_string()),
        ),
        None => match state.rows.get(state.selected) {
            Some(row) if editing_value => {
                let scope = if state.edit_column == 0 {
                    "global"
                } else {
                    "repo"
                };
                (
                    format!("  {} ({scope}) = {}", row.field, edit_buffer()),
                    row.value_hint.clone(),
                )
            }
            Some(row) => {
                let full = if state.edit_column == 0 {
                    &row.global
                } else {
                    &row.repo
                };
                let full = if full.is_empty() {
                    &row.effective
                } else {
                    full
                };
                let text = if full.is_empty() {
                    format!("  {} (no value set)", row.field)
                } else {
                    format!("  {} = {full}", row.field)
                };
                let text = if row.read_only {
                    format!("{text}  [read-only]")
                } else {
                    text
                };
                (text, row.value_hint.clone())
            }
            None => (String::new(), None),
        },
    };

    // The detail pane grows with its content (wrapped at the popup width) up
    // to a third of the popup, so long values stay fully readable.
    let detail_width = inner.width.max(1) as usize;
    let detail_lines = detail_text.chars().count().div_ceil(detail_width).max(1) as u16;
    let max_detail = (inner.height / 3).max(2);
    let detail_height = detail_lines.clamp(2, max_detail);
    let hint_height: u16 = 2;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(detail_height),
            Constraint::Length(hint_height),
        ])
        .split(inner);
    let table_area = chunks[0];
    let detail_area = chunks[1];
    let hint_area = chunks[2];

    // Column widths mirror the `widths` constraints below (34/22/22/22),
    // minus the 3 single-cell gaps ratatui inserts between the 4 columns.
    // Cell values are truncated to these widths so long values don't
    // overflow; the Field column gets the largest share because dotted
    // names (dynamicWorkflows.agentsToModels.<agent>) are the longest.
    let usable = table_area.width.saturating_sub(3) as u32;
    let field_w = (usable * 34 / 100) as usize;
    let col_w = (usable * 22 / 100) as usize;

    let header_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let header = Row::new(vec![
        Cell::from("Field").style(header_style),
        Cell::from("Global").style(header_style),
        Cell::from("Repo").style(header_style),
        Cell::from("Effective").style(header_style),
    ])
    .height(1);

    // Window rows so the selection stays visible when the table has more
    // rows than fit (e.g. dynamicWorkflows.agentsToModels.* expansions),
    // mirroring the ListPicker windowing pattern above.
    let visible = (table_area.height.saturating_sub(1)) as usize;
    let start = state
        .selected
        .saturating_sub(visible.saturating_sub(1))
        .min(state.rows.len().saturating_sub(visible));

    let rows: Vec<Row> = state
        .rows
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(i, row)| {
            let is_selected = i == state.selected;
            let cell_editing = is_selected && editing_value;

            let gval = if cell_editing && state.edit_column == 0 {
                cursor_window(&state.editor.text, state.editor.cursor, col_w)
            } else {
                sidebar::truncate_path(&row.global, col_w)
            };
            let rval = if cell_editing && state.edit_column == 1 {
                cursor_window(&state.editor.text, state.editor.cursor, col_w)
            } else {
                sidebar::truncate_path(&row.repo, col_w)
            };

            let (gcell, rcell) = if is_selected && !state.editing {
                let col_style = Style::default().fg(Color::Black).bg(Color::White);
                if state.edit_column == 0 {
                    (Cell::from(gval).style(col_style), Cell::from(rval))
                } else {
                    (Cell::from(gval), Cell::from(rval).style(col_style))
                }
            } else if cell_editing {
                let edit_style = Style::default().fg(Color::Black).bg(Color::Green);
                if state.edit_column == 0 {
                    (Cell::from(gval).style(edit_style), Cell::from(rval))
                } else {
                    (Cell::from(gval), Cell::from(rval).style(edit_style))
                }
            } else {
                (Cell::from(gval), Cell::from(rval))
            };

            let r = Row::new(vec![
                Cell::from(sidebar::truncate_path(&row.field, field_w)),
                gcell,
                rcell,
                Cell::from(sidebar::truncate_path(&row.effective, col_w)),
            ]);
            if is_selected {
                r.style(Style::default().fg(Color::White).bg(Color::DarkGray))
            } else if row.read_only {
                r.style(Style::default().fg(Color::DarkGray))
            } else {
                r
            }
        })
        .collect();

    let widths = [
        Constraint::Percentage(34),
        Constraint::Percentage(22),
        Constraint::Percentage(22),
        Constraint::Percentage(22),
    ];
    let table = Table::new(rows, widths).header(header);
    frame.render_widget(table, table_area);

    let detail_style = if state.editing {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::Gray)
    };
    frame.render_widget(
        Paragraph::new(detail_text)
            .style(detail_style)
            .wrap(ratatui::widgets::Wrap { trim: false }),
        detail_area,
    );

    // Hint pane: one line with the rejection reason (when the last save
    // attempt failed) or the field's expected value format, one line with
    // the active key bindings.
    let mut hint_lines: Vec<Line> = Vec::new();
    hint_lines.push(match &state.error {
        Some(reason) => Line::from(Span::styled(
            format!("  ✗ {reason}"),
            Style::default().fg(Color::Red),
        )),
        None => Line::from(Span::styled(
            match format_hint {
                Some(hint) => format!("  {hint}"),
                None => String::new(),
            },
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )),
    });
    let key = |k: &str| Span::styled(k.to_string(), Style::default().fg(Color::Yellow));
    hint_lines.push(match &state.new_entry {
        Some(dialogs::NewMapEntryPhase::Key) => Line::from(vec![
            Span::styled("  New mapping", Style::default().fg(Color::Green)),
            Span::raw("  |  "),
            key("Enter"),
            Span::raw("=confirm agent  "),
            key("Esc"),
            Span::raw("=cancel"),
        ]),
        Some(dialogs::NewMapEntryPhase::Value { .. }) => Line::from(vec![
            Span::styled("  New mapping", Style::default().fg(Color::Green)),
            Span::raw("  |  "),
            key("Enter"),
            Span::raw("=save mapping  "),
            key("Esc"),
            Span::raw("=cancel"),
        ]),
        Some(dialogs::NewMapEntryPhase::GuidanceEntry) => Line::from(vec![
            Span::styled("  New guidance entry", Style::default().fg(Color::Green)),
            Span::raw("  |  "),
            key("Enter"),
            Span::raw("=save entry  "),
            key("Esc"),
            Span::raw("=cancel"),
        ]),
        None if state.editing => Line::from(vec![
            Span::styled("  Editing", Style::default().fg(Color::Green)),
            Span::raw("  |  "),
            key("Enter"),
            Span::raw("=save  "),
            key("Esc"),
            Span::raw("=cancel  "),
            key("\u{2190}\u{2192}"),
            Span::raw("=cursor  "),
            key("Home/End"),
            Span::raw("=jump"),
        ]),
        None => {
            // The Ctrl+N add-entry hint only makes sense on the agentsToModels
            // or guidance rows; elsewhere it is noise. The label differs so
            // users know what will be added.
            let selected_field = state.rows.get(state.selected).map(|r| r.field.as_str());
            let on_mapping_row = selected_field
                .map(|f| {
                    f == "dynamicWorkflows.agentsToModels"
                        || f.starts_with("dynamicWorkflows.agentsToModels.")
                })
                .unwrap_or(false);
            let on_guidance_row = selected_field
                .map(|f| {
                    f == "dynamicWorkflows.guidance" || f.starts_with("dynamicWorkflows.guidance.")
                })
                .unwrap_or(false);
            let mut spans = vec![
                key("  \u{2191}\u{2193}"),
                Span::raw("=row  "),
                key("PgUp/PgDn"),
                Span::raw("=page  "),
                key("\u{2190}\u{2192}"),
                Span::raw("=col  "),
                key("Enter/e"),
                Span::raw("=edit  "),
            ];
            if on_mapping_row {
                spans.push(key("Ctrl+N"));
                spans.push(Span::raw("=add model mapping  "));
            } else if on_guidance_row {
                spans.push(key("Ctrl+N"));
                spans.push(Span::raw("=add entry  "));
            }
            spans.push(key("Esc"));
            spans.push(Span::raw("=close"));
            Line::from(spans)
        }
    });
    frame.render_widget(Paragraph::new(hint_lines), hint_area);
}
