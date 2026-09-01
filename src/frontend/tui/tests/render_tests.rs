//! Tests for git-sidebar rendering: open/closed width allocation, the
//! green-corner indicator, and the status-bar +/- summary.

use super::*;

// ─── Git sidebar ──────────────────────────────────────────────────────────

fn render_app(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| crate::frontend::tui::render::render_frame(app, frame))
        .unwrap();
    terminal.backend().buffer().clone()
}

/// True if the buffer contains a green rounded top-left corner ('╭'). The
/// only green rounded border in an idle app is the git sidebar (idle tabs
/// are DarkGray), so this uniquely detects a rendered sidebar.
fn has_green_sidebar_corner(buf: &ratatui::buffer::Buffer) -> Option<u16> {
    let area = *buf.area();
    for x in 0..area.width {
        for y in 0..area.height {
            let cell = buf.cell((x, y)).unwrap();
            if cell.symbol() == "\u{256d}" && cell.fg == ratatui::style::Color::Green {
                return Some(x);
            }
        }
    }
    None
}

fn set_summary(app: &App, additions: u32, deletions: u32) {
    use crate::frontend::tui::git_sidebar::GitDiffSummary;
    *app.active_tab().git_diff_summary.lock().unwrap() = Some(GitDiffSummary {
        files: Vec::new(),
        total_additions: additions,
        total_deletions: deletions,
        branch: None,
    });
}

#[test]
fn ctrl_g_toggles_sidebar_twice_returns_to_closed() {
    use crate::frontend::tui::git_sidebar::GitSidebarState;
    let mut app = make_app();
    assert_eq!(
        app.active_tab().git_sidebar_state,
        GitSidebarState::Closed,
        "sidebar starts closed"
    );
    press_key(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    assert_eq!(app.active_tab().git_sidebar_state, GitSidebarState::Open);
    press_key(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    assert_eq!(
        app.active_tab().git_sidebar_state,
        GitSidebarState::Closed,
        "toggling twice returns to Closed"
    );
}

#[test]
fn render_frame_closed_has_no_sidebar_and_uses_full_width() {
    let mut app = make_app();
    let buf = render_app(&mut app, 80, 24);
    assert!(
        has_green_sidebar_corner(&buf).is_none(),
        "closed sidebar must not render a green border"
    );
    // The vertical layout still spans the full width: the tab bar's rounded
    // top-left corner sits at column 0.
    assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "\u{256d}");
}

#[test]
fn render_frame_open_allocates_at_most_a_quarter_to_the_sidebar() {
    use crate::frontend::tui::git_sidebar::GitSidebarState;
    let mut app = make_app();
    app.active_tab_mut().git_sidebar_state = GitSidebarState::Open;
    let width = 80u16;
    let buf = render_app(&mut app, width, 24);
    let sidebar_x =
        has_green_sidebar_corner(&buf).expect("open sidebar must render a green rounded border");
    let sidebar_width = width - sidebar_x;
    assert!(
        sidebar_width <= width / 4,
        "sidebar width {sidebar_width} must be ≤ 25% of {width}"
    );
    assert_eq!(sidebar_width, 20, "80/4 == 20 columns");
}

#[test]
fn render_frame_narrow_terminal_collapses_sidebar() {
    use crate::frontend::tui::git_sidebar::GitSidebarState;
    let mut app = make_app();
    app.active_tab_mut().git_sidebar_state = GitSidebarState::Open;
    set_summary(&app, 7, 2);
    // 60/4 == 15 < MIN_SIDEBAR_WIDTH (20) → sidebar collapses to nothing.
    let buf = render_app(&mut app, 60, 24);
    assert!(
        has_green_sidebar_corner(&buf).is_none(),
        "sidebar must collapse when a quarter of the width is < 20 columns"
    );
    let text: String = {
        let area = *buf.area();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        text.contains("+7") && text.contains("-2"),
        "collapsed sidebar must still show the status-bar summary: {text:?}"
    );
}

#[test]
fn status_bar_shows_plus_minus_when_sidebar_closed_and_summary_present() {
    let mut app = make_app();
    set_summary(&app, 12, 3);
    let buf = render_app(&mut app, 80, 24);
    let text: String = {
        let area = *buf.area();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        text.contains("+12"),
        "status bar shows additions: had lines"
    );
    assert!(text.contains("-3"), "status bar shows deletions");
}

#[test]
fn status_bar_omits_summary_when_none() {
    // No summary set → no `+`/`-` diff readout injected into the status bar.
    let mut app = make_app();
    let buf = render_app(&mut app, 80, 24);
    // The idle status hint contains "ctrl-g git" but never a "+N -N" pair.
    let last_rows: String = {
        let area = *buf.area();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        !last_rows.contains("+0 -0"),
        "no diff summary must be shown when the summary is None"
    );
}

// ─── squad tab rendering (WI 0102) ──────────────────────────────────────────

fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
    let area = *buf.area();
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buf.cell((x, y)).unwrap().symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn push_squad_tab(app: &mut App) -> usize {
    let tab = crate::frontend::tui::tabs::Tab::new_squad(make_session());
    app.tabs.push(tab);
    let idx = app.tabs.len() - 1;
    app.active_tab = idx;
    idx
}

fn fake_task(name: &str) -> crate::data::fs::task_store::Task {
    use crate::data::fs::task_store::{MountScope, TaskStatus};
    let now = chrono::Utc::now();
    crate::data::fs::task_store::Task {
        id: name.to_string(),
        name: name.to_string(),
        description: "a test task".into(),
        repo_scope: std::path::PathBuf::from("/tmp"),
        mount_scope: MountScope::GitRoot,
        overlays: Vec::new(),
        interval_secs: 300,
        status: TaskStatus::Active,
        agent: None,
        model: None,
        backoff_until: None,
        created_at: now,
        updated_at: now,
        last_run_at: None,
        last_run_status: None,
    }
}

#[test]
fn render_frame_squad_tab_no_slots_draws_squad_body_not_execution_window() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    let buf = render_app(&mut app, 80, 24);
    let text = buffer_text(&buf);
    assert!(text.contains("squad"), "the squad body must render: {text}");
    assert!(
        text.contains("enter detail"),
        "the squad key-hint line must render: {text}"
    );
    assert!(
        !text.contains("awman"),
        "the ordinary execution window's idle title must not render for the squad tab: {text}"
    );
}

#[test]
fn render_frame_squad_tab_with_slots_draws_normal_execution_rendering() {
    use crate::frontend::tui::tabs::ExecutionPhase;
    let mut app = make_app();
    push_squad_tab(&mut app);
    app.active_tab_mut()
        .start_container("claude".into(), "awman-abc".into(), 80, 24);
    app.active_tab_mut().execution_phase = ExecutionPhase::Running {
        command: "squad attach task-a".into(),
    };
    let buf = render_app(&mut app, 80, 24);
    let text = buffer_text(&buf);
    assert!(
        !text.contains("enter detail"),
        "the squad body must not render while an attach session owns the tab's slots: {text}"
    );
    assert!(
        text.contains("running: squad attach task-a"),
        "the ordinary execution window must render instead: {text}"
    );
}

#[test]
fn squad_task_detail_modal_renders_over_the_body_and_stays_live() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    {
        let state = app.active_tab().squad.as_ref().unwrap();
        let mut snap = state.snapshot.lock().unwrap();
        snap.tasks = vec![fake_task("task-a")];
        snap.loaded = true;
    }
    let task = app
        .active_tab()
        .squad
        .as_ref()
        .unwrap()
        .snapshot
        .lock()
        .unwrap()
        .tasks[0]
        .clone();
    app.active_dialog = Some(Dialog::SquadTaskDetail(
        crate::frontend::tui::dialogs::SquadDetailState {
            name: "task-a".to_string(),
            task,
            runs: Vec::new(),
            scroll: 0,
        },
    ));

    // Mutate the underlying snapshot as the poller would, then let
    // `tick_all_tabs` refresh the open modal from it (app.rs §"WI 0102: keep
    // the squad task-detail modal live").
    {
        let state = app.active_tab().squad.as_ref().unwrap();
        let mut snap = state.snapshot.lock().unwrap();
        snap.tasks[0].description = "updated by the poller".to_string();
    }
    app.tick_all_tabs();
    match &app.active_dialog {
        Some(Dialog::SquadTaskDetail(state)) => {
            assert_eq!(state.task.description, "updated by the poller")
        }
        _ => panic!("the modal must remain open across a tick"),
    }

    let buf = render_app(&mut app, 80, 24);
    let text = buffer_text(&buf);
    assert!(
        text.contains("task: task-a"),
        "the detail modal must render over the squad body: {text}"
    );
    assert!(
        text.contains("updated by the poller"),
        "the modal must render the ticked-in snapshot value, not the stale one it opened with: {text}"
    );
}

#[test]
fn tab_bar_shows_squad_label_at_minimum_and_wide_widths() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    let narrow = buffer_text(&render_app(&mut app, 20, 24));
    assert!(
        narrow.contains("squad"),
        "the squad tab label must render even at the minimum tab width: {narrow}"
    );
    let wide = buffer_text(&render_app(&mut app, 100, 24));
    assert!(
        wide.contains("squad"),
        "the squad tab label must render in a wide terminal too: {wide}"
    );
}

#[test]
fn ctrl_g_on_squad_tab_renders_no_git_sidebar() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    press_key(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    let buf = render_app(&mut app, 80, 24);
    assert!(
        has_green_sidebar_corner(&buf).is_none(),
        "Ctrl-G must render no git sidebar for the squad tab (a non-project tab)"
    );
}

// ─── ACP agent windows (WI 0104) ───────────────────────────────────────────

fn push_acp_slot(app: &mut App, agent: &str) -> crate::frontend::tui::tabs::SharedAcpState {
    use crate::frontend::tui::tabs::{AcpSlotState, ContainerSlot};
    let state: crate::frontend::tui::tabs::SharedAcpState =
        std::sync::Arc::new(std::sync::Mutex::new(AcpSlotState::default()));
    app.active_tab_mut()
        .container_slots
        .push(ContainerSlot::new_acp(
            String::new(),
            agent.to_string(),
            state.clone(),
        ));
    state
}

/// Coordinates of the first rounded top-left corner ('╭') rendered in
/// `color`, scanning row-major (top-to-bottom, then left-to-right).
fn find_border_corner_of_color(
    buf: &ratatui::buffer::Buffer,
    color: ratatui::style::Color,
) -> Option<(u16, u16)> {
    let area = *buf.area();
    for y in 0..area.height {
        for x in 0..area.width {
            let cell = buf.cell((x, y)).unwrap();
            if cell.symbol() == "\u{256d}" && cell.fg == color {
                return Some((x, y));
            }
        }
    }
    None
}

#[test]
fn execution_window_border_is_acp_purple_not_green_when_focused_slot_is_acp() {
    use crate::frontend::tui::acp_view::ACP_BORDER_COLOR;
    use crate::frontend::tui::tabs::ExecutionPhase;
    // Full-render companion to the unit-level
    // `window_border_color_done_focused_acp_is_purple`: the focused+Done
    // execution window must actually paint its border in the ACP identity
    // color, not fall back to the stdio green.
    let mut app = make_app();
    push_acp_slot(&mut app, "claude");
    app.focus = Focus::ExecutionWindow;
    app.active_tab_mut().execution_phase = ExecutionPhase::Done {
        command: "chat".into(),
        exit_code: 0,
    };
    let buf = render_app(&mut app, 80, 24);
    assert!(
        find_border_corner_of_color(&buf, ACP_BORDER_COLOR).is_some(),
        "the focused+Done execution window must render its border in the ACP identity color"
    );
    assert!(
        find_border_corner_of_color(&buf, ratatui::style::Color::Green).is_none(),
        "the focused+Done state must not render the stdio green border when the focused slot is ACP"
    );
}

#[test]
fn mixed_parallel_group_renders_one_purple_and_one_green_minimized_bar() {
    use crate::frontend::tui::acp_view::ACP_BORDER_COLOR;
    use crate::frontend::tui::tabs::ContainerWindowState;
    // A two-slot tab mixing stdio + ACP (WI 0104), both minimized: the
    // stdio slot's bar is green (`render_container_bars`), the ACP slot's
    // bar is purple (`render_acp_bars`), and — since the idle execution
    // phase keeps the tab bar and execution window both DarkGray — these
    // are the only green/purple rounded corners in the frame, so finding
    // one of each proves both bars actually drew.
    let mut app = make_app();
    app.active_tab_mut()
        .start_container("claude".into(), "awman-a".into(), 80, 24);
    push_acp_slot(&mut app, "codex");
    app.active_tab_mut().container_window_state = ContainerWindowState::Minimized;

    let buf = render_app(&mut app, 80, 30);
    let green = find_border_corner_of_color(&buf, ratatui::style::Color::Green)
        .expect("the stdio slot must render a green minimized bar");
    let purple = find_border_corner_of_color(&buf, ACP_BORDER_COLOR)
        .expect("the ACP slot must render a purple minimized bar");
    assert!(
        purple.1 > green.1,
        "the bars tile in slot order — stdio (slot 0) above ACP (slot 1): \
         green at {green:?}, purple at {purple:?}"
    );
}

#[test]
fn acp_permission_request_modal_renders_through_the_dialog_framework() {
    // `TuiAcpFrontend::request_permission` opens exactly this `Dialog::Custom`
    // shape (see `per_command/acp_frontend.rs`); this confirms the generic
    // dialog framework actually renders it, title/body/hotkeys included.
    let mut app = make_app();
    app.active_dialog = Some(Dialog::Custom {
        title: "ACP Permission Request".to_string(),
        body: "The agent wants to run:\n\n  Write config.json (edit)\n\nAllow this action?"
            .to_string(),
        keys: vec![('1', "Allow once".to_string()), ('2', "Reject".to_string())],
    });
    let buf = render_app(&mut app, 80, 24);
    let text = buffer_text(&buf);
    assert!(
        text.contains("ACP Permission Request"),
        "modal title must render: {text}"
    );
    assert!(
        text.contains("Write config.json"),
        "modal body must render: {text}"
    );
    assert!(
        text.contains("[1] Allow once"),
        "the first hotkey option must render: {text}"
    );
    assert!(
        text.contains("[2] Reject"),
        "the second hotkey option must render: {text}"
    );
}

// ─── Workflow Overview: minimized / maximized ───────────────────────────

/// Publish a parallel workflow of `n` sibling steps into the active tab.
fn set_parallel_workflow(app: &App, n: usize) {
    use crate::frontend::tui::tabs::{WorkflowStepView, WorkflowViewState};
    *app.active_tab().workflow_state.lock().unwrap() = Some(WorkflowViewState {
        steps: (0..n)
            .map(|i| WorkflowStepView {
                name: format!("step-{i}"),
                status: "running".into(),
                agent: None,
                model: None,
                depends_on: vec![],
            })
            .collect(),
        current_step: None,
        max_concurrent: None,
    });
}

#[test]
fn frame_overview_defaults_to_minimized_and_ctrl_o_maximizes_it() {
    let mut app = make_app();
    set_parallel_workflow(&app, 4);

    let minimized = buffer_text(&render_app(&mut app, 80, 40));
    assert!(
        minimized.contains("4 steps\u{2026}"),
        "the default overview summarizes a parallel stage: {minimized}"
    );
    assert!(
        !minimized.contains("step-0"),
        "the minimized overview names no individual step: {minimized}"
    );

    press_key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
    let maximized = buffer_text(&render_app(&mut app, 80, 40));
    for i in 0..4 {
        assert!(
            maximized.contains(&format!("step-{i}")),
            "the maximized overview names every parallel step: {maximized}"
        );
    }
}

/// Append one stdio container slot named `container_name`, the way a parallel
/// workflow group fills the tab (`start_container` replaces the whole group,
/// so it cannot build a multi-slot tab).
fn push_stdio_slot(app: &mut App, container_name: &str) {
    use crate::frontend::tui::tabs::ContainerSlot;
    let mut slot = ContainerSlot::new(String::new(), "claude".into(), 0);
    if let Some(info) = slot.container_info.as_mut() {
        info.container_name = container_name.to_string();
    }
    app.active_tab_mut().container_slots.push(slot);
}

#[test]
fn ctrl_o_and_ctrl_m_min_max_independently() {
    use crate::frontend::tui::tabs::{ContainerWindowState, WorkflowOverviewState};
    let mut app = make_app();
    set_parallel_workflow(&app, 3);
    push_stdio_slot(&mut app, "awman-only");
    app.active_tab_mut().container_window_state = ContainerWindowState::Maximized;

    // Maximized container: the single slot owns the PTY overlay.
    // `container_inner_area` is published by the renderer exactly when it
    // draws that overlay.
    render_app(&mut app, 80, 40);
    assert!(
        app.active_tab().container_inner_area.is_some(),
        "a maximized slot draws its PTY overlay while the overview is minimized"
    );

    // Ctrl-O maximizes the overview. The PTY overlay stays up: the two
    // windows share the body rather than displacing each other.
    app.active_tab_mut().container_inner_area = None;
    press_key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
    let both_max = buffer_text(&render_app(&mut app, 80, 40));
    assert_eq!(
        app.active_tab().workflow_overview_state,
        WorkflowOverviewState::Maximized
    );
    assert_eq!(
        app.active_tab().container_window_state,
        ContainerWindowState::Maximized,
        "Ctrl-O must not touch the container's own min/max"
    );
    assert!(
        app.active_tab().container_inner_area.is_some(),
        "a maximized overview must not put the PTY overlay away: {both_max}"
    );
    for i in 0..3 {
        assert!(
            both_max.contains(&format!("step-{i}")),
            "the maximized overview names every parallel step: {both_max}"
        );
    }

    // Ctrl-M minimizes the container. The overview stays maximized.
    press_key(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
    app.active_tab_mut().container_inner_area = None;
    let overview_only = buffer_text(&render_app(&mut app, 80, 40));
    assert_eq!(
        app.active_tab().workflow_overview_state,
        WorkflowOverviewState::Maximized,
        "Ctrl-M must not touch the overview's own min/max"
    );
    assert!(
        app.active_tab().container_inner_area.is_none(),
        "a minimized container draws no PTY overlay: {overview_only}"
    );
    assert!(
        overview_only.contains("awman-only"),
        "the minimized container falls back to its status bar: {overview_only}"
    );
    assert!(overview_only.contains("step-2"), "{overview_only}");

    // Ctrl-O minimizes the overview. The container stays minimized.
    press_key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
    let both_min = buffer_text(&render_app(&mut app, 80, 40));
    assert_eq!(
        app.active_tab().container_window_state,
        ContainerWindowState::Minimized
    );
    assert!(
        both_min.contains("3 steps\u{2026}") && !both_min.contains("step-2"),
        "the overview is back to its one-box-per-stage summary: {both_min}"
    );
}

#[test]
fn maximized_overview_leaves_the_pty_overlay_its_share_of_the_body() {
    use crate::frontend::tui::tabs::{ContainerWindowState, WorkflowOverviewState};
    let mut app = make_app();
    // 40 parallel steps want 120 rows — far more than the frame has.
    set_parallel_workflow(&app, 40);
    push_stdio_slot(&mut app, "awman-only");
    app.active_tab_mut().container_window_state = ContainerWindowState::Maximized;
    app.active_tab_mut().workflow_overview_state = WorkflowOverviewState::Maximized;

    // 40 rows: 3 tab bar + 5 bottom chrome leaves 32 for the body. With the
    // PTY overlay on screen the overview may take at most half of that (16 →
    // 5 whole boxes), so the overlay keeps the rest.
    let text = buffer_text(&render_app(&mut app, 80, 40));
    assert!(
        text.contains("+ 36 more\u{2026}"),
        "the overview is capped at half the body and says what it hides: {text}"
    );
    let inner = app
        .active_tab()
        .container_inner_area
        .expect("the PTY overlay is still drawn alongside a maximized overview");
    assert!(
        inner.height >= 10,
        "the PTY overlay keeps a usable share of the body, got {inner:?}"
    );
}

#[test]
fn maximized_overview_wins_space_and_truncates_the_container_status_bars() {
    use crate::frontend::tui::tabs::{ContainerWindowState, WorkflowOverviewState};
    let mut app = make_app();
    set_parallel_workflow(&app, 6);
    for i in 0..6 {
        push_stdio_slot(&mut app, &format!("awman-c{i}"));
    }
    app.active_tab_mut().container_window_state = ContainerWindowState::Minimized;
    app.active_tab_mut().workflow_overview_state = WorkflowOverviewState::Maximized;

    // 30 rows: 3 tab bar + 5 bottom chrome leaves 22 for the body. No PTY
    // overlay is up, so the overview gets the whole body: it asks for 18 (6
    // boxes) and is served first; the execution window keeps its 5-row floor
    // out of the 4 left, so no container bar fits at all.
    let text = buffer_text(&render_app(&mut app, 80, 30));
    assert!(
        text.contains("step-5"),
        "the overview is served its full height first: {text}"
    );
    assert!(
        !text.contains("awman-c0"),
        "container status bars are truncated into whatever the overview leaves: {text}"
    );

    // Given room for both, the bars come back.
    let roomy = buffer_text(&render_app(&mut app, 80, 60));
    assert!(roomy.contains("step-5"), "{roomy}");
    assert!(roomy.contains("awman-c0"), "{roomy}");
}

#[test]
fn maximized_overview_never_grows_past_the_space_between_tab_bar_and_command_box() {
    use crate::frontend::tui::tabs::WorkflowOverviewState;
    let mut app = make_app();
    set_parallel_workflow(&app, 40);
    app.active_tab_mut().workflow_overview_state = WorkflowOverviewState::Maximized;

    // 40 steps want 120 rows; the frame has 24 - 3 - 5 = 16 to give, which is
    // 5 whole boxes. The command box must still render at the bottom.
    let text = buffer_text(&render_app(&mut app, 80, 24));
    assert!(
        text.contains("+ 36 more\u{2026}"),
        "the clipped stage advertises how many steps it is hiding: {text}"
    );
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[lines.len() - 3..]
            .iter()
            .any(|l| l.contains("\u{256d}") || l.contains("\u{2570}")),
        "the command box keeps its rows at the bottom of the frame: {text}"
    );
}

/// WI 0106 Part 5: the card carries the task name, a summary, the **last-run
/// outcome** and time, and the next evaluation — the outcome being what
/// actually happened on the last run, not whether the task is scheduled.
#[test]
fn squad_task_cards_render_rounded_borders_and_the_last_run_outcome() {
    use crate::data::fs::task_store::{RunStatus, TaskStatus};
    let mut app = make_app();
    push_squad_tab(&mut app);
    {
        let state = app.active_tab().squad.as_ref().unwrap();
        let mut snap = state.snapshot.lock().unwrap();
        let mut triggered = fake_task("issue-triage");
        triggered.description = "watch the issue tracker".into();
        triggered.last_run_at = Some(chrono::Utc::now());
        triggered.last_run_status = Some(RunStatus::WorkflowExecuted);

        let mut quiet = fake_task("nightly-sweep");
        quiet.last_run_at = Some(chrono::Utc::now());
        quiet.last_run_status = Some(RunStatus::NotTriggered);
        // Paused is scheduling state, reported separately from the outcome.
        quiet.status = TaskStatus::Paused;

        let never = fake_task("brand-new");

        snap.tasks = vec![triggered, quiet, never];
        snap.loaded = true;
    }

    let buf = render_app(&mut app, 100, 30);
    let text = buffer_text(&buf);

    assert!(
        text.contains('\u{256d}') && text.contains('\u{256f}'),
        "cards must use ratatui's rounded borders: {text}"
    );
    assert!(text.contains("issue-triage"), "{text}");
    assert!(text.contains("watch the issue tracker"), "{text}");
    assert!(
        text.contains("workflow executed"),
        "a card must show its last run's outcome, not its active/paused state: {text}"
    );
    assert!(
        text.contains("not triggered"),
        "an un-triggered last run must read as such: {text}"
    );
    assert!(
        text.contains("never run"),
        "a task that has never run must say so rather than showing a blank outcome: {text}"
    );
    assert!(
        text.contains("paused"),
        "paused remains visible, as scheduling state alongside the outcome: {text}"
    );
    assert!(text.contains("Next:"), "{text}");
}

#[test]
fn a_squad_tab_with_no_tasks_renders_an_empty_state_instead_of_empty_cards() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    {
        let state = app.active_tab().squad.as_ref().unwrap();
        state.snapshot.lock().unwrap().loaded = true;
    }
    let text = buffer_text(&render_app(&mut app, 80, 24));
    assert!(
        text.contains("No squad tasks yet"),
        "an empty grid must render its empty state: {text}"
    );
}

/// The detail modal repeats the per-task action keys, so a user who opened it
/// does not have to close it to remember or trigger them.
#[test]
fn the_squad_detail_modal_shows_the_task_scoped_action_tooltip() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    let task = fake_task("issue-triage");
    {
        let state = app.active_tab().squad.as_ref().unwrap();
        let mut snap = state.snapshot.lock().unwrap();
        snap.tasks = vec![task.clone()];
        snap.loaded = true;
    }
    app.active_dialog = Some(Dialog::SquadTaskDetail(
        crate::frontend::tui::dialogs::SquadDetailState {
            name: "issue-triage".to_string(),
            task,
            runs: Vec::new(),
            scroll: 0,
        },
    ));

    let text = buffer_text(&render_app(&mut app, 100, 30));
    for key in ["a attach", "p pause", "r resume", "d delete"] {
        assert!(
            text.contains(key),
            "the modal's action tooltip must offer {key:?}: {text}"
        );
    }
}
