//! Pure-presentation dialog widgets for the TUI.
//!
//! Each dialog captures keyboard input while open, renders centered in the
//! terminal, and returns a typed Layer 2 enum value when the user responds.
//! Cancellable with Esc.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::frontend::tui::text_edit::TextEdit;

/// A dialog request sent from the command thread to the event loop.
#[derive(Debug)]
pub enum DialogRequest {
    YesNo {
        title: String,
        body: String,
    },
    YesNoCancel {
        title: String,
        body: String,
    },
    TextInput {
        title: String,
        prompt: String,
        default_text: Option<String>,
    },
    MultilineInput {
        title: String,
        prompt: String,
    },
    ListPicker {
        title: String,
        items: Vec<String>,
    },
    KindSelect {
        title: String,
        options: Vec<(String, String)>,
    },
    WorkflowControlBoard(WorkflowControlBoardState),
    WorkflowStepError(WorkflowStepErrorState),
    WorkflowYoloCountdown(WorkflowYoloCountdownState),
    WorkflowStepConfirm(WorkflowStepConfirmState),
    AgentSetup(AgentSetupState),
    MountScope(MountScopeState),
    AgentAuth(AgentAuthState),
    QuitConfirm,
    CloseTabConfirm,
    /// Confirmation prompt opened when the user presses Ctrl+C while a
    /// workflow is running. `y` aborts the workflow (kills the container,
    /// returns the current step to Pending), `n`/`Esc` keeps it running.
    WorkflowCancelConfirm,
    ConfigShow {
        rows: Vec<ConfigShowRow>,
        /// Initially selected row, so the dialog can reopen on the field the
        /// user just edited instead of jumping back to the top.
        selected: usize,
        /// The previous edit, when it was rejected (invalid value or failed
        /// write). The dialog reopens in edit mode with the input preserved
        /// and the reason displayed.
        rejected: Option<ConfigShowRejectedEdit>,
    },
    Loading {
        title: String,
    },
    Custom {
        title: String,
        body: String,
        keys: Vec<(char, String)>,
    },
}

/// A dialog response returned from the event loop to the command thread.
#[derive(Debug, Clone)]
pub enum DialogResponse {
    Yes,
    No,
    Cancel,
    Text(String),
    Index(usize),
    Char(char),
    Dismissed,
}

/// The active dialog state stored in `App`.
pub enum Dialog {
    YesNo {
        title: String,
        body: String,
    },
    YesNoCancel {
        title: String,
        body: String,
    },
    TextInput {
        title: String,
        prompt: String,
        editor: TextEdit,
    },
    MultilineInput {
        title: String,
        prompt: String,
        editor: TextEdit,
    },
    ListPicker {
        title: String,
        items: Vec<String>,
        selected: usize,
    },
    KindSelect {
        title: String,
        options: Vec<(String, String)>,
    },
    WorkflowControlBoard(WorkflowControlBoardState),
    WorkflowStepError(WorkflowStepErrorState),
    WorkflowYoloCountdown(WorkflowYoloCountdownState),
    WorkflowStepConfirm(WorkflowStepConfirmState),
    AgentSetup(AgentSetupState),
    MountScope(MountScopeState),
    AgentAuth(AgentAuthState),
    QuitConfirm,
    CloseTabConfirm,
    WorkflowCancelConfirm,
    ConfigShow(ConfigShowState),
    /// Condition detail + run history for the amie tab (WI 0102). Kept live by
    /// `App::tick_all_tabs` from the active amie tab's snapshot.
    AmieConditionDetail(AmieDetailState),
    /// Confirmation before removing an amie condition (WI 0102). `y` dispatches
    /// `amie remove <name>`; `n`/`Esc` dismisses.
    AmieRemoveConfirm {
        name: String,
    },
    Loading {
        title: String,
    },
    Custom {
        title: String,
        body: String,
        keys: Vec<(char, String)>,
    },
    /// Unrecoverable startup error (e.g. an invalid `runtime:` value in the
    /// global config). The only action is quitting: Enter, Esc, and Ctrl-C
    /// all exit the TUI.
    FatalError {
        title: String,
        body: String,
    },
    /// One-shot informational modal with no command thread behind it: Enter and
    /// Esc simply dismiss it. Used for the amie key-setup snippet, which must
    /// be readable long enough to copy and must not scroll away in a status bar.
    Notice {
        title: String,
        body: String,
    },
}

#[derive(Debug, Clone)]
pub struct WorkflowControlBoardState {
    pub step_name: String,
    pub can_launch_next: bool,
    pub can_continue_current: bool,
    pub can_restart: bool,
    pub can_go_back: bool,
    pub can_finish: bool,
    /// Human-readable reason explaining why "continue in current container"
    /// is not available (e.g. "next step uses a different agent"). Rendered
    /// in DarkGray underneath the disabled `[↓]` line so users understand
    /// why it's greyed out.
    pub continue_unavailable_reason: Option<String>,
    pub cancel_to_previous_unavailable_reason: Option<String>,
    pub finish_workflow_unavailable_reason: Option<String>,
    /// Reason "Restart current step" is unavailable — set in a parallel group
    /// where restart applies only to the focused container (WI-0096 §10).
    /// Rendered in DarkGray under the dimmed Restart line.
    pub restart_unavailable_reason: Option<String>,
    /// True when a container is currently running (mid-step). The engine
    /// computes this from `can_dismiss` in `AvailableActions`.
    /// Changes rendering: Esc = dismiss (step keeps running), [p] = pause.
    pub can_dismiss: bool,
    /// Custom label for the right-arrow (launch-next) action, copied from
    /// `AvailableActions.launch_next_label`. `None` renders the default
    /// "Next: new container". The dynamic leader step sets this to
    /// "Start dynamic workflow".
    pub launch_next_label: Option<String>,
    /// The step this Workflow Control Board's actions apply to (WI-0096 §10).
    /// In a parallel group this is the currently-focused container. Defaults to
    /// `step_name` for single-step workflows.
    pub focused_step_name: String,
    /// Number of steps in the focused step's parallel group. `0` means the
    /// focused step is not part of a multi-step parallel batch.
    pub parallel_peer_count: usize,
    /// Live peers still running in the focused step's parallel group (excludes
    /// the focused step). Non-zero disables back/finish in the WCB.
    pub parallel_peers_running: usize,
}

#[derive(Debug, Clone)]
pub struct WorkflowStepErrorState {
    pub step_name: String,
    pub error_lines: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct WorkflowYoloCountdownState {
    pub step_name: String,
    pub remaining_secs: u64,
}

#[derive(Debug, Clone)]
pub struct WorkflowStepConfirmState {
    pub completed_step: String,
    pub next_step: String,
}

#[derive(Debug, Clone)]
pub struct AgentSetupState {
    pub agent_name: String,
    pub image_only: bool,
    pub has_fallback: bool,
    pub fallback_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MountScopeState {
    pub git_root: String,
    pub cwd: String,
}

#[derive(Debug, Clone)]
pub struct AgentAuthState {
    pub agent_name: String,
    pub env_vars: Vec<String>,
}

/// State for the amie condition-detail modal (WI 0102). `name` is the identity
/// used by `tick_all_tabs` to refresh `condition` and `runs` from the tab
/// snapshot each tick; `scroll` offsets the run-history table.
#[derive(Debug, Clone)]
pub struct AmieDetailState {
    pub name: String,
    pub condition: crate::data::fs::condition_store::Condition,
    pub runs: Vec<crate::data::fs::condition_store::Run>,
    pub scroll: usize,
}

pub struct ConfigShowState {
    pub rows: Vec<ConfigShowRow>,
    pub selected: usize,
    pub editing: bool,
    pub edit_column: usize,
    pub editor: TextEdit,
    /// In-progress Ctrl+N "add model mapping" flow, if any. While `Some`,
    /// `editing` is also true so text input routes to `editor`.
    pub new_entry: Option<NewMapEntryPhase>,
    /// Why the last save attempt was rejected. Rendered in the dialog until
    /// the user cancels the edit or starts a new one.
    pub error: Option<String>,
}

/// A rejected config edit carried in `DialogRequest::ConfigShow`: the value
/// the user typed plus the rejection reason, so the reopened dialog restores
/// the edit instead of discarding the input.
#[derive(Debug, Clone)]
pub struct ConfigShowRejectedEdit {
    pub field: String,
    pub value: String,
    pub global: bool,
    pub reason: String,
}

/// Phase of a Ctrl+N add-entry flow in the config dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewMapEntryPhase {
    /// Typing the agent name (the new `agentsToModels` map key).
    Key,
    /// Typing the comma-separated model list for the confirmed key.
    Value { key: String },
    /// Typing a new `dynamicWorkflows.guidance` entry (WI-0099). Single-phase:
    /// only the instruction text is needed; its array index is assigned
    /// automatically by appending.
    GuidanceEntry,
}

#[derive(Debug)]
pub struct ConfigShowRow {
    pub field: String,
    pub global: String,
    pub repo: String,
    pub effective: String,
    pub read_only: bool,
    /// Whether the value may be written to the global config scope.
    pub global_writable: bool,
    /// Whether the value may be written to the repo config scope.
    pub repo_writable: bool,
    /// Short format hint shown while editing (e.g. "true or false").
    pub value_hint: Option<String>,
}

/// Compute a centered rect for a dialog.
pub fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(popup_layout[1])[1]
}

/// Compute a fixed-size centered rect.
pub fn centered_fixed(cols: u16, rows: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(cols) / 2;
    let y = area.y + area.height.saturating_sub(rows) / 2;
    Rect::new(x, y, cols.min(area.width), rows.min(area.height))
}

/// Render a dialog frame with the given title and border color.
/// Returns the padded inner area (1-cell horizontal padding, 1-row vertical
/// padding inside the border) so dialog content doesn't touch the frame.
pub fn render_dialog_frame(title: &str, color: Color, area: Rect, frame: &mut Frame) -> Rect {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .border_type(ratatui::widgets::BorderType::Rounded);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Add padding: 1 col each side, 1 row top/bottom
    Rect {
        x: inner.x.saturating_add(1),
        y: inner.y.saturating_add(1),
        width: inner.width.saturating_sub(2),
        height: inner.height.saturating_sub(2),
    }
}

/// Render the YesNo dialog.
///
/// Sizes dynamically: width grows to fit the longest body line (clamped to a
/// usable range) and height grows to fit the body, a blank-line separator,
/// and the hint row. Body wraps so content is never silently clipped.
pub fn render_yes_no(title: &str, body: &str, area: Rect, frame: &mut Frame) {
    let max_w = area.width.saturating_sub(6).max(40);
    let max_body_w = body
        .lines()
        .map(unicode_width::UnicodeWidthStr::width)
        .max()
        .unwrap_or(0) as u16;
    // +6 = 2 borders + 2 padding + 2 leading-space margin used in the hint.
    let title_w = unicode_width::UnicodeWidthStr::width(title) as u16 + 4;
    let width = max_body_w.saturating_add(6).max(50).max(title_w).min(max_w);
    // Body lines (after wrapping at content width), blank separator, hint.
    let inner_w = width.saturating_sub(4) as usize; // subtract borders+padding
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
    let dialog_area = centered_fixed(width, height, area);
    let inner = render_dialog_frame(title, Color::Yellow, dialog_area, frame);
    let text = format!("{body}\n\n  [y] Yes   [n] No   [Esc] Cancel");
    frame.render_widget(
        Paragraph::new(text).wrap(ratatui::widgets::Wrap { trim: false }),
        inner,
    );
}

/// Render the quit confirmation dialog (single tab).
pub fn render_quit_confirm(area: Rect, frame: &mut Frame) {
    let width = 56u16.min(area.width.saturating_sub(4).max(40));
    let dialog_area = centered_fixed(width, 8, area);
    let inner = render_dialog_frame("Quit awman?", Color::Yellow, dialog_area, frame);
    let text = "  Press Ctrl-C again to quit awman\n\n  [Esc] cancel";
    frame.render_widget(
        Paragraph::new(text)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .style(Style::default()),
        inner,
    );
}

/// Render the close-tab confirmation dialog (multiple tabs).
pub fn render_close_tab_confirm(area: Rect, frame: &mut Frame) {
    let width = 60u16.min(area.width.saturating_sub(4).max(40));
    let dialog_area = centered_fixed(width, 9, area);
    let inner = render_dialog_frame("Close tab?", Color::Yellow, dialog_area, frame);
    let text =
        "  Press Ctrl-C again to quit awman\n  Press Ctrl-T to close this tab\n\n  [Esc] cancel";
    frame.render_widget(
        Paragraph::new(text).wrap(ratatui::widgets::Wrap { trim: false }),
        inner,
    );
}

/// Render the workflow-cancel confirmation dialog (Ctrl+C while a workflow
/// is running).
pub fn render_workflow_cancel_confirm(area: Rect, frame: &mut Frame) {
    let width = 64u16.min(area.width.saturating_sub(4).max(40));
    let dialog_area = centered_fixed(width, 11, area);
    let inner = render_dialog_frame(
        "Cancel Workflow Execution",
        Color::Yellow,
        dialog_area,
        frame,
    );
    let text =
        "  Cancel workflow execution?\n\n  The running container will be killed and the\n  current step returned to Pending for resumption.\n\n  [y] cancel execution   [n / Esc] keep running";
    frame.render_widget(
        Paragraph::new(text).wrap(ratatui::widgets::Wrap { trim: false }),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    // ─── Helper ───────────────────────────────────────────────────────────────

    fn render_to_string(
        width: u16,
        height: u16,
        f: impl FnOnce(ratatui::layout::Rect, &mut ratatui::Frame),
    ) -> String {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| f(frame.area(), frame)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| {
                        buffer
                            .cell((x, y))
                            .map(|c| c.symbol().to_string())
                            .unwrap_or(" ".to_string())
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ─── Geometry: centered_fixed ─────────────────────────────────────────────

    #[test]
    fn centered_fixed_center_within_large_area() {
        let area = Rect::new(0, 0, 100, 50);
        let result = centered_fixed(40, 10, area);
        assert_eq!(result.x, (100 - 40) / 2);
        assert_eq!(result.y, (50 - 10) / 2);
        assert_eq!(result.width, 40);
        assert_eq!(result.height, 10);
    }

    #[test]
    fn centered_fixed_clips_width_when_smaller_than_area() {
        let area = Rect::new(0, 0, 20, 50);
        let result = centered_fixed(40, 10, area);
        assert_eq!(result.width, 20);
    }

    #[test]
    fn centered_fixed_clips_height_when_smaller_than_area() {
        let area = Rect::new(0, 0, 100, 5);
        let result = centered_fixed(40, 10, area);
        assert_eq!(result.height, 5);
    }

    #[test]
    fn centered_fixed_zero_dialog_is_at_center() {
        let area = Rect::new(0, 0, 100, 50);
        let result = centered_fixed(0, 0, area);
        assert_eq!(result.width, 0);
        assert_eq!(result.height, 0);
    }

    // ─── Geometry: centered_rect ──────────────────────────────────────────────

    #[test]
    fn centered_rect_centers_percentage_area() {
        let area = Rect::new(0, 0, 100, 100);
        let result = centered_rect(50, 50, area);
        // With 50% of 100 = 50 cols/rows centered: margins are 25 each side
        assert!(result.x >= 24 && result.x <= 26, "x={}", result.x);
        assert!(result.y >= 24 && result.y <= 26, "y={}", result.y);
    }

    // ─── Rendering tests ──────────────────────────────────────────────────────

    #[test]
    fn render_quit_confirm_contains_quit_text() {
        let output = render_to_string(80, 24, |area, frame| {
            render_quit_confirm(area, frame);
        });
        let lower = output.to_lowercase();
        assert!(
            lower.contains("quit"),
            "expected 'quit' in output:\n{output}"
        );
    }

    #[test]
    fn render_yes_no_shows_y_and_n_keys() {
        let output = render_to_string(80, 24, |area, frame| {
            render_yes_no("Test?", "Test body", area, frame);
        });
        assert!(
            output.contains("[y]"),
            "expected '[y]' in output:\n{output}"
        );
        assert!(
            output.contains("[n]"),
            "expected '[n]' in output:\n{output}"
        );
    }

    #[test]
    fn render_close_tab_confirm_shows_options() {
        let output = render_to_string(80, 24, |area, frame| {
            render_close_tab_confirm(area, frame);
        });
        assert!(
            output.contains("Ctrl-C"),
            "expected 'Ctrl-C' in output:\n{output}"
        );
        assert!(
            output.contains("Ctrl-T"),
            "expected 'Ctrl-T' in output:\n{output}"
        );
        assert!(
            output.contains("Esc"),
            "expected 'Esc' in output:\n{output}"
        );
    }
}
