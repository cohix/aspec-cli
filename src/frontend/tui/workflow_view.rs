//! Workflow Overview — horizontal display of workflow step progression.
//!
//! Layout:
//! - Steps are grouped into **topological columns** ("stages") by sorted
//!   `depends_on` signature (steps that share the same dependencies sit in
//!   the same column).
//! - Each step renders as a **3-row rounded box** with a status glyph, the
//!   step name, and the resolved `agent/model` on the top border.
//! - **Inter-column `→` arrows** sit on the middle row of the first row of
//!   boxes, joining adjacent columns.
//!
//! The overview has two display modes ([`WorkflowOverviewState`], toggled with
//! `Ctrl-O` — independently of the container PTY's own `Ctrl-M` min/max):
//! - **Minimized** (default) — one box per stage, 3 rows total. A
//!   single-step stage draws that step's normal box; a parallel stage draws
//!   a `N steps…` summary in the stage's aggregate status colour.
//! - **Maximized** — every step of every stage gets its own box, stacked
//!   vertically at the same indent. Nothing is ever rolled up: completed
//!   parallel siblings keep their own box, name, agent label, and colour.
//!   The overview grows into the vertical space the frame can spare between
//!   the tab bar and the command box (the caller's `max_height`, which the
//!   renderer halves while a maximized container PTY is also on screen); when
//!   even that is not enough, the last visible row becomes a `+ N more…`
//!   overflow box and the mouse wheel scrolls the stage.

use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::data::workflow_state::{PhaseStepStatus, StepState, WorkflowState};
use crate::frontend::tui::tabs::{WorkflowOverviewState, WorkflowStepView, WorkflowViewState};

/// Rows occupied by one step box (rounded border + one content row).
pub const STEP_BOX_HEIGHT: u16 = 3;

/// Compute the rows the Workflow Overview wants, clamped to `max_height`.
///
/// - Minimized → one box row (3 rows), whatever the shape of the workflow.
/// - Maximized → one box row per step in the widest stage, with **no** cap
///   other than `max_height` (the share of the body the caller is willing to
///   give up). The result is always a whole number of box rows, so a box is
///   never clipped mid-border.
///
/// Returns 0 when `state` has no steps, or when `max_height` cannot fit even
/// a single box.
pub fn workflow_overview_height(
    state: &WorkflowViewState,
    overview_state: WorkflowOverviewState,
    max_height: u16,
) -> u16 {
    if state.steps.is_empty() {
        return 0;
    }
    let rows = match overview_state {
        WorkflowOverviewState::Minimized => 1u16,
        WorkflowOverviewState::Maximized => {
            let columns = build_workflow_columns(state);
            columns.iter().map(|c| c.len()).max().unwrap_or(1).max(1) as u16
        }
    };
    let desired = rows.saturating_mul(STEP_BOX_HEIGHT);
    let cap = (max_height / STEP_BOX_HEIGHT) * STEP_BOX_HEIGHT;
    desired.min(cap)
}

/// Render the Workflow Overview into the given area.
pub fn render_workflow_overview(
    state: &WorkflowViewState,
    area: Rect,
    frame: &mut Frame,
    scroll_offset: usize,
    overview_state: WorkflowOverviewState,
) {
    if area.width == 0 || area.height == 0 || state.steps.is_empty() {
        return;
    }

    let columns = build_workflow_columns(state);
    let num_cols = columns.len();
    if num_cols == 0 {
        return;
    }

    // Subtract one cell per inter-column arrow gap.
    let arrow_chars = num_cols.saturating_sub(1) as u16;
    let box_space = area.width.saturating_sub(arrow_chars);
    let base_col_w = (box_space / num_cols as u16).max(4);

    // The number of vertical slots for parallel steps in this overview. The
    // minimized mode always draws exactly one row per stage.
    let visible_rows = if overview_state.is_maximized() {
        (area.height / STEP_BOX_HEIGHT).max(1) as usize
    } else {
        1
    };
    // Scrolling only means something when the stage does not fit; the
    // minimized overview is always exactly one row tall.
    let scroll_offset = if overview_state.is_maximized() {
        scroll_offset
    } else {
        0
    };

    let mut col_x = area.x;
    for (col_idx, col_steps) in columns.iter().enumerate() {
        // Last column absorbs the remainder so the overview fills the area.
        let this_col_w = if col_idx + 1 == num_cols {
            area.x + area.width - col_x
        } else {
            base_col_w
        };

        // Build the display rows for this stage. Maximized gives every step
        // its own row (steps beyond `max_concurrent` are marked queued);
        // minimized gives the stage a single row.
        let column_rows = if overview_state.is_maximized() {
            build_column_rows(col_steps, state.max_concurrent)
        } else {
            vec![build_minimized_row(col_steps)]
        };
        // When the stage does not fit, the last visible slot is spent on the
        // `+ N more…` marker rather than on a step box — so that slot's step
        // counts as hidden too.
        let remaining = column_rows.len().saturating_sub(scroll_offset);
        let shown = if remaining > visible_rows {
            visible_rows - 1
        } else {
            remaining
        };
        let hidden = remaining - shown;
        let rows_to_show: Vec<&ColumnRow> =
            column_rows.iter().skip(scroll_offset).take(shown).collect();

        for (row_idx, row) in rows_to_show.iter().enumerate() {
            // WI-0096 §11: truly-parallel siblings share the same box_x — no
            // per-row indent stagger (which used to imply sequential steps).
            let box_x = col_x;
            let box_w = this_col_w.max(4);
            let row_y = area.y + row_idx as u16 * STEP_BOX_HEIGHT;
            if row_y + STEP_BOX_HEIGHT > area.y + area.height {
                break;
            }
            let box_area = Rect::new(box_x, row_y, box_w, STEP_BOX_HEIGHT);

            let (label, style, title) = match row {
                ColumnRow::Step { step, queued } => {
                    let is_current = state
                        .current_step
                        .as_ref()
                        .map(|c| c == &step.name)
                        .unwrap_or(false);
                    // Queued steps (waiting for a concurrency slot) get a `·`
                    // name prefix.
                    let name = if *queued {
                        format!("\u{00b7} {}", step.name)
                    } else {
                        step.name.clone()
                    };
                    let (label, style) =
                        step_box_label_and_style(&name, &step.status, is_current, box_w);
                    let title =
                        step_agent_model_title(step.agent.as_deref(), step.model.as_deref(), box_w);
                    (label, style, title)
                }
                ColumnRow::Stage { count, status } => {
                    let name = format!("{count} steps\u{2026}");
                    // A stage summary stands for several steps that may run
                    // under different agents/models, so it carries no single
                    // agent/model label — press Ctrl-O to see them.
                    let (label, style) = step_box_label_and_style(&name, status, false, box_w);
                    (label, style, None)
                }
            };

            let mut block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(style);
            if let Some(title) = title {
                block = block.title(Span::styled(title, Style::default().fg(Color::DarkGray)));
            }
            let para = Paragraph::new(label).block(block).style(style);
            frame.render_widget(para, box_area);

            // Arrow between this column and the next, on the middle row of
            // the FIRST row of boxes only (so it visually connects column
            // headers without overlapping parallel siblings).
            if col_idx + 1 < num_cols && row_idx == 0 {
                let arrow_x = col_x + this_col_w;
                if arrow_x < area.x + area.width {
                    let arrow_area = Rect::new(arrow_x, row_y + 1, 1, 1);
                    frame.render_widget(
                        Paragraph::new("\u{2192}").style(Style::default().fg(Color::DarkGray)),
                        arrow_area,
                    );
                }
            }
        }

        // Overflow indicator below the last drawn box when there are hidden
        // steps under the fold. Scrolling the overview reveals them.
        if hidden > 0 {
            let row_y = area.y + rows_to_show.len() as u16 * STEP_BOX_HEIGHT;
            if row_y + STEP_BOX_HEIGHT <= area.y + area.height {
                let box_w = this_col_w.max(4);
                let box_area = Rect::new(col_x, row_y, box_w, STEP_BOX_HEIGHT);
                let more_label = format!("+ {} more\u{2026}", hidden);
                let para = Paragraph::new(more_label)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(Color::DarkGray)),
                    )
                    .style(Style::default().fg(Color::DarkGray));
                frame.render_widget(para, box_area);
            }
        }

        col_x += this_col_w + 1;
    }
}

/// A single rendered row in a Workflow Overview column.
enum ColumnRow<'a> {
    /// One step's box. `queued` steps (beyond `max_concurrent`) get a `·`
    /// name prefix.
    Step {
        step: &'a WorkflowStepView,
        queued: bool,
    },
    /// The minimized-mode summary of a multi-step stage: `N steps…`, drawn in
    /// the stage's aggregate status colour.
    Stage { count: usize, status: &'static str },
}

/// Whether a step status is terminal (the step will not run again).
fn is_completed_status(status: &str) -> bool {
    matches!(status, "done" | "cancelled" | "skipped")
}

/// Build the ordered display rows for one stage in **maximized** mode.
///
/// Every step keeps its own row, in workflow-definition order, for the whole
/// life of the run — completed siblings are never rolled up, and a step never
/// changes position as its neighbours finish.
///
/// Active steps beyond `max_concurrent` are marked `queued` (rendered with a
/// `·` prefix). `None` (unlimited) marks nothing.
fn build_column_rows<'a>(
    col: &[&'a WorkflowStepView],
    max_concurrent: Option<usize>,
) -> Vec<ColumnRow<'a>> {
    let mut active_idx = 0usize;
    col.iter()
        .map(|s| {
            let queued = if is_completed_status(&s.status) {
                false
            } else {
                let i = active_idx;
                active_idx += 1;
                matches!(max_concurrent, Some(mc) if i >= mc) && s.status == "pending"
            };
            ColumnRow::Step { step: s, queued }
        })
        .collect()
}

/// Build the single **minimized**-mode row for one stage.
///
/// A one-step stage renders that step's normal box (name, agent/model title,
/// status colour). A parallel stage renders a `N steps…` summary instead.
fn build_minimized_row<'a>(col: &[&'a WorkflowStepView]) -> ColumnRow<'a> {
    if col.len() == 1 {
        return ColumnRow::Step {
            step: col[0],
            queued: false,
        };
    }
    ColumnRow::Stage {
        count: col.len(),
        status: stage_status(col),
    }
}

/// Aggregate a stage's steps into the one status its collapsed box shows.
///
/// Worst-news-first: a failure outranks a remediation, which outranks a
/// running step, which outranks anything still pending. Only once every step
/// is terminal does the stage read as finished — as `done` when they all
/// succeeded, otherwise as `cancelled` (the ⊘ glyph, since something was
/// cancelled or skipped).
fn stage_status(col: &[&WorkflowStepView]) -> &'static str {
    let has = |s: &str| col.iter().any(|step| step.status == s);
    if has("error") {
        "error"
    } else if has("fixing") {
        "fixing"
    } else if has("running") {
        "running"
    } else if col.iter().all(|s| is_completed_status(&s.status)) {
        if col.iter().all(|s| s.status == "done") {
            "done"
        } else {
            "cancelled"
        }
    } else {
        "pending"
    }
}

/// Convert a `WorkflowState` (Layer 0 data) to a `WorkflowViewState` (TUI).
///
/// Prepends pseudo-steps from `setup_step_states`, maps main steps from
/// `steps` + `step_states`, and appends pseudo-steps from
/// `teardown_step_states`.
pub fn workflow_state_to_view_state(state: &WorkflowState) -> WorkflowViewState {
    let mut steps: Vec<WorkflowStepView> = Vec::new();

    for ps in &state.setup_step_states {
        steps.push(WorkflowStepView {
            name: format!("[setup] {}", ps.description),
            status: phase_step_status_to_str(&ps.status).to_string(),
            agent: None,
            model: None,
            depends_on: Vec::new(),
        });
    }

    for info in &state.steps {
        let status = state
            .step_states
            .get(&info.name)
            .map(step_state_to_str)
            .unwrap_or("pending")
            .to_string();
        steps.push(WorkflowStepView {
            name: info.name.clone(),
            status,
            agent: info.agent.clone(),
            model: info.model.clone(),
            depends_on: info.depends_on.clone(),
        });
    }

    for ps in &state.teardown_step_states {
        steps.push(WorkflowStepView {
            name: format!("[teardown] {}", ps.description),
            status: phase_step_status_to_str(&ps.status).to_string(),
            agent: None,
            model: None,
            depends_on: Vec::new(),
        });
    }

    let current_step = state.current_step_index.and_then(|idx| {
        let setup_len = state.setup_step_states.len();
        state
            .steps
            .get(idx)
            .map(|s| s.name.clone())
            .or_else(|| steps.get(idx + setup_len).map(|s| s.name.clone()))
    });

    WorkflowViewState {
        steps,
        current_step,
        max_concurrent: None,
    }
}

fn step_state_to_str(state: &StepState) -> &'static str {
    match state {
        StepState::Pending => "pending",
        StepState::Running { .. } => "running",
        StepState::Succeeded => "done",
        StepState::Failed { .. } => "error",
        StepState::Cancelled => "cancelled",
        StepState::Skipped => "skipped",
    }
}

fn phase_step_status_to_str(status: &PhaseStepStatus) -> &'static str {
    match status {
        PhaseStepStatus::Pending => "pending",
        PhaseStepStatus::Running => "running",
        PhaseStepStatus::Succeeded => "done",
        PhaseStepStatus::Failed { .. } => "error",
        PhaseStepStatus::Remediating { .. } => "fixing",
    }
}

/// Group steps into columns by topological depth. Steps at the same depth
/// form a parallel group (same column). Depth is the longest path from any
/// root (step with no dependencies) to this step. Steps that share the exact
/// same set of dependencies at the same depth are grouped together — steps
/// that depend on members of the previous parallel group all land in the next
/// column regardless of which specific member they depend on.
fn build_workflow_columns(state: &WorkflowViewState) -> Vec<Vec<&WorkflowStepView>> {
    use std::collections::HashMap;

    let step_names: HashMap<&str, usize> = state
        .steps
        .iter()
        .enumerate()
        .map(|(i, s)| (s.name.as_str(), i))
        .collect();

    let mut depths: Vec<usize> = vec![0; state.steps.len()];
    let mut changed = true;
    while changed {
        changed = false;
        for (i, step) in state.steps.iter().enumerate() {
            for dep in &step.depends_on {
                if let Some(&dep_idx) = step_names.get(dep.as_str()) {
                    let new_depth = depths[dep_idx] + 1;
                    if new_depth > depths[i] {
                        depths[i] = new_depth;
                        changed = true;
                    }
                }
            }
        }
    }

    let max_depth = depths.iter().copied().max().unwrap_or(0);
    let mut columns: Vec<Vec<&WorkflowStepView>> = Vec::with_capacity(max_depth + 1);
    for d in 0..=max_depth {
        let col: Vec<&WorkflowStepView> = state
            .steps
            .iter()
            .enumerate()
            .filter(|(i, _)| depths[*i] == d)
            .map(|(_, s)| s)
            .collect();
        if !col.is_empty() {
            columns.push(col);
        }
    }
    columns
}

/// Build the top-border title for a step box — the `agent/model` the step will
/// run under (e.g. `claude/opus-4-8`).
///
/// Returns `None` when the step declares neither an agent nor a model: such a
/// step inherits the project-default agent AND model, so there is nothing that
/// distinguishes it and the box gets no title. When only one of the two is
/// known, just that part is shown. The result is truncated with an ellipsis to
/// fit `box_width`.
fn step_agent_model_title(
    agent: Option<&str>,
    model: Option<&str>,
    box_width: u16,
) -> Option<String> {
    let text = match (agent, model) {
        (None, None) => return None,
        (Some(a), Some(m)) => format!("{a}/{m}"),
        (Some(a), None) => a.to_string(),
        (None, Some(m)) => m.to_string(),
    };

    // Leave the two rounded corners of the top border untouched.
    let max_chars = (box_width as usize).saturating_sub(2).max(1);
    let title = if text.chars().count() > max_chars {
        let trunc: String = text.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{trunc}\u{2026}")
    } else {
        text
    };
    Some(title)
}

/// Compute the label text + style for a step box.
///
/// Status → glyph + color:
/// - Pending → `○` DarkGray
/// - Running → `●` Blue + Bold
/// - Done → `✓` Green
/// - Error → `✗` Red + Bold
/// - Fixing → `🔧` Magenta + Bold (on_failure remediation in progress)
/// - Cancelled / Skipped → `⊘` DarkGray
///
/// Current step is rendered with extra Bold on top of its status style.
/// Auto-advance-disabled steps get a small `🔒` prefix.
fn step_box_label_and_style(
    name: &str,
    status: &str,
    is_current: bool,
    box_width: u16,
) -> (String, Style) {
    let max_name_chars = (box_width as usize).saturating_sub(6).max(1);
    let truncated_name = if name.chars().count() > max_name_chars {
        let trunc: String = name
            .chars()
            .take(max_name_chars.saturating_sub(1))
            .collect();
        format!("{trunc}\u{2026}")
    } else {
        name.to_string()
    };

    let (glyph, mut style) = match status {
        "pending" => ("\u{25cb}", Style::default().fg(Color::DarkGray)),
        "running" => (
            "\u{25cf}",
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        ),
        "done" => ("\u{2713}", Style::default().fg(Color::Green)),
        "error" => (
            "\u{2717}",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        "fixing" => (
            "\u{1f527}",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        "cancelled" | "skipped" => ("\u{2298}", Style::default().fg(Color::DarkGray)),
        _ => ("\u{25cb}", Style::default().fg(Color::DarkGray)),
    };
    if is_current {
        style = style.add_modifier(Modifier::BOLD);
    }
    let label = format!(" {glyph} {truncated_name} ");
    (label, style)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(name: &str, status: &str, deps: Vec<&str>) -> WorkflowStepView {
        WorkflowStepView {
            name: name.into(),
            status: status.into(),
            agent: None,
            model: None,
            depends_on: deps.into_iter().map(|s| s.into()).collect(),
        }
    }

    fn view(steps: Vec<WorkflowStepView>) -> WorkflowViewState {
        WorkflowViewState {
            steps,
            current_step: None,
            max_concurrent: None,
        }
    }

    #[test]
    fn build_workflow_columns_groups_by_topological_depth() {
        let v = view(vec![
            step("a", "done", vec![]),
            step("b", "done", vec![]),
            step("c", "running", vec!["a", "b"]),
        ]);
        let cols = build_workflow_columns(&v);
        // a + b at depth 0 → same column. c at depth 1 → next column.
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].len(), 2);
        assert_eq!(cols[1].len(), 1);
        assert_eq!(cols[1][0].name, "c");
    }

    #[test]
    fn build_workflow_columns_parallel_deps_land_same_column() {
        // D depends on B, E depends on C. Both B and C are at depth 1,
        // so D and E should both be at depth 2 (same column).
        let v = view(vec![
            step("a", "done", vec![]),
            step("b", "done", vec!["a"]),
            step("c", "done", vec!["a"]),
            step("d", "running", vec!["b"]),
            step("e", "running", vec!["c"]),
        ]);
        let cols = build_workflow_columns(&v);
        assert_eq!(cols.len(), 3);
        assert_eq!(cols[0].len(), 1); // a
        assert_eq!(cols[1].len(), 2); // b, c
        assert_eq!(cols[2].len(), 2); // d, e
    }

    const ROOMY: u16 = 200;

    fn maximized_height(v: &WorkflowViewState, max: u16) -> u16 {
        workflow_overview_height(v, WorkflowOverviewState::Maximized, max)
    }

    fn minimized_height(v: &WorkflowViewState, max: u16) -> u16 {
        workflow_overview_height(v, WorkflowOverviewState::Minimized, max)
    }

    #[test]
    fn workflow_overview_height_is_zero_when_no_steps() {
        let v = view(vec![]);
        assert_eq!(maximized_height(&v, ROOMY), 0);
        assert_eq!(minimized_height(&v, ROOMY), 0);
    }

    #[test]
    fn workflow_overview_height_3_when_sequential() {
        let v = view(vec![
            step("a", "done", vec![]),
            step("b", "running", vec!["a"]),
        ]);
        assert_eq!(maximized_height(&v, ROOMY), 3);
    }

    #[test]
    fn workflow_overview_height_grows_with_parallel_group() {
        let v = view(vec![
            step("a", "done", vec![]),
            step("b", "done", vec![]),
            step("c", "running", vec![]),
        ]);
        // 3 parallel steps → 3 * 3 = 9 rows.
        assert_eq!(maximized_height(&v, ROOMY), 9);
    }

    #[test]
    fn workflow_overview_height_maximized_has_no_cap_but_the_budget() {
        let steps: Vec<WorkflowStepView> = (0..40)
            .map(|i| step(&format!("s{i}"), "running", vec![]))
            .collect();
        let v = view(steps);
        // No concurrency cap and no legacy row cap: 40 parallel siblings each
        // get their own box when the frame is tall enough.
        assert_eq!(maximized_height(&v, ROOMY), 120);
    }

    #[test]
    fn workflow_overview_height_maximized_is_capped_to_available_space() {
        let steps: Vec<WorkflowStepView> = (0..40)
            .map(|i| step(&format!("s{i}"), "running", vec![]))
            .collect();
        let v = view(steps);
        // 20 rows of space → 6 whole boxes (18 rows); the overview never returns
        // a height that would clip a box mid-border.
        assert_eq!(maximized_height(&v, 20), 18);
    }

    #[test]
    fn workflow_overview_height_minimized_is_one_box_whatever_the_shape() {
        let steps: Vec<WorkflowStepView> = (0..40)
            .map(|i| step(&format!("s{i}"), "running", vec![]))
            .collect();
        let v = view(steps);
        assert_eq!(minimized_height(&v, ROOMY), 3);
    }

    #[test]
    fn workflow_overview_height_is_zero_when_not_even_one_box_fits() {
        let v = view(vec![step("a", "running", vec![])]);
        assert_eq!(minimized_height(&v, 2), 0);
        assert_eq!(maximized_height(&v, 2), 0);
    }

    // ── step_box_label_and_style ──────────────────────────────────────────────

    #[test]
    fn step_box_label_pending_uses_circle_glyph_and_dark_gray() {
        let (label, style) = step_box_label_and_style("foo", "pending", false, 20);
        assert!(label.contains('\u{25cb}'));
        assert!(label.contains("foo"));
        assert_eq!(style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn step_box_label_running_uses_filled_circle_blue_bold() {
        let (label, style) = step_box_label_and_style("foo", "running", false, 20);
        assert!(label.contains('\u{25cf}'));
        assert_eq!(style.fg, Some(Color::Blue));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn step_box_label_done_uses_check_glyph_green() {
        let (label, style) = step_box_label_and_style("foo", "done", false, 20);
        assert!(label.contains('\u{2713}'));
        assert_eq!(style.fg, Some(Color::Green));
    }

    #[test]
    fn step_box_label_error_uses_cross_glyph_red_bold() {
        let (label, style) = step_box_label_and_style("foo", "error", false, 20);
        assert!(label.contains('\u{2717}'));
        assert_eq!(style.fg, Some(Color::Red));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn step_box_label_current_step_adds_bold_on_top_of_status() {
        let (_, style) = step_box_label_and_style("foo", "done", true, 20);
        // Done is not bold by default, but is_current adds BOLD.
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    // ── step_agent_model_title ────────────────────────────────────────────────

    #[test]
    fn agent_model_title_none_when_neither_declared() {
        // No agent and no model → the step inherits the project defaults and
        // gets no title.
        assert_eq!(step_agent_model_title(None, None, 40), None);
    }

    #[test]
    fn agent_model_title_shows_agent_slash_model() {
        assert_eq!(
            step_agent_model_title(Some("claude"), Some("opus-4-8"), 40),
            Some("claude/opus-4-8".to_string())
        );
    }

    #[test]
    fn agent_model_title_agent_only() {
        assert_eq!(
            step_agent_model_title(Some("claude"), None, 40),
            Some("claude".to_string())
        );
    }

    #[test]
    fn agent_model_title_model_only() {
        assert_eq!(
            step_agent_model_title(None, Some("opus-4-8"), 40),
            Some("opus-4-8".to_string())
        );
    }

    #[test]
    fn agent_model_title_truncates_to_box_width() {
        let title = step_agent_model_title(Some("claude"), Some("opus-4-8"), 8).unwrap();
        assert!(title.chars().count() <= 6, "title should fit box_width - 2");
        assert!(title.contains('\u{2026}'));
    }

    #[test]
    fn step_box_label_truncates_long_name() {
        let (label, _) = step_box_label_and_style("very-long-step-name", "pending", false, 12);
        assert!(label.contains('\u{2026}'));
    }

    // ── WI-0096 §11 parallel overview rendering ────────────────────────────────

    #[test]
    fn parallel_siblings_share_one_column_no_row_indent() {
        // Three steps with the exact same (empty) dependency set are a parallel
        // group: they all land in the same column, which means the renderer
        // gives every one the same `box_x = col_x` — no per-row indent stagger.
        let v = view(vec![
            step("a", "running", vec![]),
            step("b", "running", vec![]),
            step("c", "running", vec![]),
        ]);
        let cols = build_workflow_columns(&v);
        assert_eq!(cols.len(), 1, "same dep-set siblings share a single column");
        assert_eq!(cols[0].len(), 3);
        let names: Vec<&str> = cols[0].iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn column_rows_never_roll_up_completed_parallel_siblings() {
        // A parallel group of 5 where 3 are completed keeps five rows: every
        // step holds its own box, in definition order, for the whole run.
        let steps = [
            step("a", "done", vec![]),
            step("b", "done", vec![]),
            step("c", "cancelled", vec![]),
            step("d", "running", vec![]),
            step("e", "pending", vec![]),
        ];
        let col: Vec<&WorkflowStepView> = steps.iter().collect();
        let rows = build_column_rows(&col, None);

        let names: Vec<&str> = rows
            .iter()
            .map(|r| match r {
                ColumnRow::Step { step, .. } => step.name.as_str(),
                ColumnRow::Stage { .. } => panic!("expanded rows must never summarize a stage"),
            })
            .collect();
        assert_eq!(names, vec!["a", "b", "c", "d", "e"]);
    }

    #[test]
    fn minimized_row_for_single_step_stage_is_the_step_itself() {
        // A one-step stage is not a parallel group, so the minimized overview
        // draws its normal box — name, agent label, and all.
        let steps = [step("only", "running", vec![])];
        let col: Vec<&WorkflowStepView> = steps.iter().collect();
        assert!(matches!(
            build_minimized_row(&col),
            ColumnRow::Step { step, queued: false } if step.name == "only"
        ));
    }

    #[test]
    fn minimized_row_for_parallel_stage_counts_the_steps() {
        let steps = [
            step("a", "done", vec![]),
            step("b", "running", vec![]),
            step("c", "pending", vec![]),
        ];
        let col: Vec<&WorkflowStepView> = steps.iter().collect();
        match build_minimized_row(&col) {
            ColumnRow::Stage { count, status } => {
                assert_eq!(count, 3);
                // One sibling is still running, so the stage reads as running.
                assert_eq!(status, "running");
            }
            _ => panic!("a parallel stage must collapse to a step-count summary"),
        }
    }

    #[test]
    fn stage_status_reports_worst_news_first() {
        let running = [step("a", "running", vec![]), step("b", "pending", vec![])];
        let failed = [step("a", "error", vec![]), step("b", "running", vec![])];
        let fixing = [step("a", "fixing", vec![]), step("b", "running", vec![])];
        let all_done = [step("a", "done", vec![]), step("b", "done", vec![])];
        let mixed_terminal = [step("a", "done", vec![]), step("b", "skipped", vec![])];
        let waiting = [step("a", "pending", vec![]), step("b", "pending", vec![])];

        let s = |steps: &[WorkflowStepView]| stage_status(&steps.iter().collect::<Vec<_>>());
        assert_eq!(s(&running), "running");
        assert_eq!(s(&failed), "error");
        assert_eq!(s(&fixing), "fixing");
        assert_eq!(s(&all_done), "done");
        assert_eq!(s(&mixed_terminal), "cancelled");
        assert_eq!(s(&waiting), "pending");
    }

    // ── overview renders agent/model title on the box border ───────────────────

    fn render_overview_text(v: &WorkflowViewState, width: u16, height: u16) -> String {
        render_overview_text_in(v, width, height, WorkflowOverviewState::Maximized)
    }

    fn render_overview_text_in(
        v: &WorkflowViewState,
        width: u16,
        height: u16,
        overview_state: WorkflowOverviewState,
    ) -> String {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_workflow_overview(v, frame.area(), frame, 0, overview_state))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let area = *buf.area();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn overview_shows_agent_model_title_for_overridden_step() {
        let mut s = step("build", "running", vec![]);
        s.agent = Some("claude".into());
        s.model = Some("opus-4-8".into());
        let v = view(vec![s]);
        let text = render_overview_text(&v, 40, 3);
        assert!(
            text.contains("claude/opus-4-8"),
            "expected agent/model title on the box border, got:\n{text}"
        );
    }

    #[test]
    fn overview_omits_title_for_default_step() {
        // Neither agent nor model declared → box carries no agent/model title.
        let v = view(vec![step("build", "running", vec![])]);
        let text = render_overview_text(&v, 40, 3);
        assert!(
            !text.contains('/'),
            "default step should have no agent/model title, got:\n{text}"
        );
    }

    #[test]
    fn minimized_overview_shows_step_count_for_a_parallel_stage() {
        let v = view(vec![
            step("alpha", "running", vec![]),
            step("beta", "running", vec![]),
            step("gamma", "pending", vec![]),
        ]);
        let text = render_overview_text_in(&v, 40, 3, WorkflowOverviewState::Minimized);
        assert!(
            text.contains("3 steps\u{2026}"),
            "collapsed parallel stage must summarize as \"3 steps…\", got:\n{text}"
        );
        assert!(
            !text.contains("alpha"),
            "collapsed stage must not name individual steps, got:\n{text}"
        );
    }

    #[test]
    fn minimized_overview_shows_the_normal_box_for_a_single_step_stage() {
        let v = view(vec![
            step("build", "running", vec![]),
            step("test", "pending", vec!["build"]),
        ]);
        let text = render_overview_text_in(&v, 60, 3, WorkflowOverviewState::Minimized);
        assert!(text.contains("build"), "got:\n{text}");
        assert!(text.contains("test"), "got:\n{text}");
        assert!(!text.contains("steps\u{2026}"), "got:\n{text}");
    }

    #[test]
    fn maximized_overview_names_every_completed_parallel_sibling() {
        // The old overview rolled completed siblings into "(+N completed)". Now
        // each keeps its own named box.
        let v = view(vec![
            step("alpha", "done", vec![]),
            step("beta", "done", vec![]),
            step("gamma", "done", vec![]),
            step("delta", "running", vec![]),
        ]);
        let text = render_overview_text_in(&v, 40, 12, WorkflowOverviewState::Maximized);
        for name in ["alpha", "beta", "gamma", "delta"] {
            assert!(
                text.contains(name),
                "expected {name} in overview, got:\n{text}"
            );
        }
        assert!(!text.contains("completed)"), "got:\n{text}");
    }

    #[test]
    fn maximized_overview_keeps_the_agent_label_on_completed_siblings() {
        let mut a = step("alpha", "done", vec![]);
        a.agent = Some("claude".into());
        let mut b = step("beta", "done", vec![]);
        b.agent = Some("codex".into());
        let v = view(vec![a, b]);
        let text = render_overview_text_in(&v, 40, 6, WorkflowOverviewState::Maximized);
        assert!(text.contains("claude"), "got:\n{text}");
        assert!(text.contains("codex"), "got:\n{text}");
    }

    #[test]
    fn maximized_overview_overflows_into_a_more_box_when_the_frame_is_too_short() {
        let steps: Vec<WorkflowStepView> = (0..6)
            .map(|i| step(&format!("s{i}"), "running", vec![]))
            .collect();
        let v = view(steps);
        // 9 rows fit 3 boxes; the third becomes the overflow marker for the
        // 4 steps it hides.
        let text = render_overview_text_in(&v, 40, 9, WorkflowOverviewState::Maximized);
        assert!(
            text.contains("+ 4 more\u{2026}"),
            "expected an overflow box, got:\n{text}"
        );
    }

    #[test]
    fn column_rows_mark_steps_beyond_max_concurrent_as_queued() {
        // With max_concurrent = 2, pending siblings past the second are marked
        // queued (rendered with a `·` prefix).
        let steps = [
            step("a", "running", vec![]),
            step("b", "running", vec![]),
            step("c", "pending", vec![]),
            step("d", "pending", vec![]),
        ];
        let col: Vec<&WorkflowStepView> = steps.iter().collect();
        let rows = build_column_rows(&col, Some(2));
        let queued: Vec<bool> = rows
            .iter()
            .map(|r| matches!(r, ColumnRow::Step { queued, .. } if *queued))
            .collect();
        // a, b (running) not queued; c, d (pending, index >= 2) queued.
        assert_eq!(queued, vec![false, false, true, true]);
    }
}
