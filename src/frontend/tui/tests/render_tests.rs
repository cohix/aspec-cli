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

// ─── amie tab rendering (WI 0102) ──────────────────────────────────────────

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

fn push_amie_tab(app: &mut App) -> usize {
    let tab = crate::frontend::tui::tabs::Tab::new_amie(make_session());
    app.tabs.push(tab);
    let idx = app.tabs.len() - 1;
    app.active_tab = idx;
    idx
}

fn fake_condition(name: &str) -> crate::data::fs::condition_store::Condition {
    use crate::data::fs::condition_store::{ConditionStatus, MountScope};
    let now = chrono::Utc::now();
    crate::data::fs::condition_store::Condition {
        id: name.to_string(),
        name: name.to_string(),
        description: "a test condition".into(),
        repo_scope: std::path::PathBuf::from("/tmp"),
        mount_scope: MountScope::GitRoot,
        interval_secs: 300,
        status: ConditionStatus::Active,
        agent: None,
        model: None,
        backoff_until: None,
        created_at: now,
        updated_at: now,
        last_run_at: None,
    }
}

#[test]
fn render_frame_amie_tab_no_slots_draws_amie_body_not_execution_window() {
    let mut app = make_app();
    push_amie_tab(&mut app);
    let buf = render_app(&mut app, 80, 24);
    let text = buffer_text(&buf);
    assert!(text.contains("amie"), "the amie body must render: {text}");
    assert!(
        text.contains("enter detail"),
        "the amie key-hint line must render: {text}"
    );
    assert!(
        !text.contains("awman"),
        "the ordinary execution window's idle title must not render for the amie tab: {text}"
    );
}

#[test]
fn render_frame_amie_tab_with_slots_draws_normal_execution_rendering() {
    use crate::frontend::tui::tabs::ExecutionPhase;
    let mut app = make_app();
    push_amie_tab(&mut app);
    app.active_tab_mut()
        .start_container("claude".into(), "awman-abc".into(), 80, 24);
    app.active_tab_mut().execution_phase = ExecutionPhase::Running {
        command: "amie attach cond-a".into(),
    };
    let buf = render_app(&mut app, 80, 24);
    let text = buffer_text(&buf);
    assert!(
        !text.contains("enter detail"),
        "the amie body must not render while an attach session owns the tab's slots: {text}"
    );
    assert!(
        text.contains("running: amie attach cond-a"),
        "the ordinary execution window must render instead: {text}"
    );
}

#[test]
fn amie_condition_detail_modal_renders_over_the_body_and_stays_live() {
    let mut app = make_app();
    push_amie_tab(&mut app);
    {
        let state = app.active_tab().amie.as_ref().unwrap();
        let mut snap = state.snapshot.lock().unwrap();
        snap.conditions = vec![fake_condition("cond-a")];
        snap.loaded = true;
    }
    let condition = app.active_tab().amie.as_ref().unwrap().snapshot.lock().unwrap().conditions[0].clone();
    app.active_dialog = Some(Dialog::AmieConditionDetail(
        crate::frontend::tui::dialogs::AmieDetailState {
            name: "cond-a".to_string(),
            condition,
            runs: Vec::new(),
            scroll: 0,
        },
    ));

    // Mutate the underlying snapshot as the poller would, then let
    // `tick_all_tabs` refresh the open modal from it (app.rs §"WI 0102: keep
    // the amie condition-detail modal live").
    {
        let state = app.active_tab().amie.as_ref().unwrap();
        let mut snap = state.snapshot.lock().unwrap();
        snap.conditions[0].description = "updated by the poller".to_string();
    }
    app.tick_all_tabs();
    match &app.active_dialog {
        Some(Dialog::AmieConditionDetail(state)) => {
            assert_eq!(state.condition.description, "updated by the poller")
        }
        _ => panic!("the modal must remain open across a tick"),
    }

    let buf = render_app(&mut app, 80, 24);
    let text = buffer_text(&buf);
    assert!(
        text.contains("condition: cond-a"),
        "the detail modal must render over the amie body: {text}"
    );
    assert!(
        text.contains("updated by the poller"),
        "the modal must render the ticked-in snapshot value, not the stale one it opened with: {text}"
    );
}

#[test]
fn tab_bar_shows_amie_label_at_minimum_and_wide_widths() {
    let mut app = make_app();
    push_amie_tab(&mut app);
    let narrow = buffer_text(&render_app(&mut app, 20, 24));
    assert!(
        narrow.contains("amie"),
        "the amie tab label must render even at the minimum tab width: {narrow}"
    );
    let wide = buffer_text(&render_app(&mut app, 100, 24));
    assert!(
        wide.contains("amie"),
        "the amie tab label must render in a wide terminal too: {wide}"
    );
}

#[test]
fn ctrl_g_on_amie_tab_renders_no_git_sidebar() {
    let mut app = make_app();
    push_amie_tab(&mut app);
    press_key(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    let buf = render_app(&mut app, 80, 24);
    assert!(
        has_green_sidebar_corner(&buf).is_none(),
        "Ctrl-G must render no git sidebar for the amie tab (a non-project tab)"
    );
}
