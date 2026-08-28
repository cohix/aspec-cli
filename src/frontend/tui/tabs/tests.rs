use super::*;
use crate::data::session::{Session, SessionOpenOptions, StaticGitRootResolver};

fn make_test_session() -> Session {
    let tmp = tempfile::tempdir().unwrap();
    let resolver = StaticGitRootResolver::new(tmp.path());
    Session::open(
        tmp.path().to_path_buf(),
        &resolver,
        SessionOpenOptions::default(),
    )
    .unwrap()
}

fn make_tab() -> Tab {
    Tab::new(make_test_session())
}

/// Install a single container slot (as `spawn_command` would) and return
/// a sender feeding its PTY output channel.
fn attach_slot_stdout(tab: &mut Tab) -> tokio::sync::mpsc::UnboundedSender<Vec<u8>> {
    if tab.container_slots.is_empty() {
        tab.start_container("claude".into(), String::new(), 80, 24);
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    tab.focused_slot_mut().unwrap().container_stdout_rx = Some(rx);
    tx
}

/// Tab whose working-dir basename is `name`, for project_name tests.
/// Returns the TempDir so the directory outlives `Session::open`.
fn make_named_tab(name: &str) -> (Tab, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let resolver = StaticGitRootResolver::new(&dir);
    let session = Session::open(dir.clone(), &resolver, SessionOpenOptions::default()).unwrap();
    (Tab::new(session), tmp)
}

// ── project_name width behavior ─────────────────────────────────────────

#[test]
fn project_name_at_minimum_tab_width_matches_historical_cap() {
    // 20 cols is the minimum tab width; 6 chars of title chrome leave 14
    // for the name — the old fixed truncation limit.
    let (tab, _tmp) = make_named_tab("a-very-long-project-directory-name");
    let out = tab.project_name(20);
    assert_eq!(out.chars().count(), 14);
    assert!(out.ends_with('\u{2026}'), "clipped name must mark: {out}");
}

#[test]
fn project_name_uses_extra_space_in_wide_tabs() {
    let name = "a-very-long-project-directory-name";
    let (tab, _tmp) = make_named_tab(name);
    assert_eq!(
        tab.project_name(name.chars().count() as u16 + 6),
        name,
        "a tab wide enough for the full name must not truncate it"
    );
    let out = tab.project_name(30);
    assert_eq!(
        out.chars().count(),
        24,
        "a 30-col tab must show 24 chars of the name, not clip at 14: {out}"
    );
    assert!(out.ends_with('\u{2026}'));
}

// ── git sidebar state ──────────────────────────────────────────────────

#[test]
fn new_tab_git_sidebar_is_closed() {
    let tab = make_tab();
    assert_eq!(tab.git_sidebar_state, GitSidebarState::Closed);
}

#[test]
fn new_tab_git_diff_summary_is_none() {
    let tab = make_tab();
    assert!(
        tab.git_diff_summary.lock().unwrap().is_none(),
        "a fresh tab has no diff summary until the poll task populates it"
    );
}

// ── mid-workflow container exit (poll_container_exit) ─────────────────

#[test]
fn container_exit_report_closes_window_and_leaves_summary() {
    let mut tab = make_tab();
    tab.start_container("claude".into(), "awman-abc".into(), 80, 24);
    tab.container_window_state = ContainerWindowState::Maximized;
    tab.container_rendered = true; // pretend a frame made it to screen
    *tab.container_exit_shared.lock().unwrap() = Some(137);

    tab.poll_container_exit();

    assert_eq!(tab.container_window_state, ContainerWindowState::Hidden);
    let summary = tab
        .last_container_summary
        .as_ref()
        .expect("closing on container exit must capture the summary bar");
    assert_eq!(summary.exit_code, 137);
    assert_eq!(summary.container_name, "awman-abc");
    assert!(
        tab.focused_slot()
            .and_then(|s| s.container_info.as_ref())
            .is_some(),
        "the slot's container_info must survive so later workflow steps keep stats polling"
    );
    assert!(
        tab.container_exit_shared.lock().unwrap().is_none(),
        "the exit slot is consumed"
    );
}

#[test]
fn poll_container_exit_is_noop_without_a_reported_exit() {
    let mut tab = make_tab();
    tab.start_container("claude".into(), "awman-abc".into(), 80, 24);
    tab.container_window_state = ContainerWindowState::Maximized;

    tab.poll_container_exit();

    // No exit was reported (stuck container / yolo countdown running /
    // container alive) — the window must stay open.
    assert_eq!(tab.container_window_state, ContainerWindowState::Maximized);
    assert!(tab.last_container_summary.is_none());
}

#[test]
fn late_bytes_after_container_exit_do_not_reopen_window() {
    let mut tab = make_tab();
    tab.start_container("claude".into(), "awman-abc".into(), 80, 24);
    tab.container_window_state = ContainerWindowState::Maximized;
    tab.container_rendered = true;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    tab.focused_slot_mut().unwrap().container_stdout_rx = Some(rx);

    *tab.container_exit_shared.lock().unwrap() = Some(0);
    tab.poll_container_exit();
    assert_eq!(tab.container_window_state, ContainerWindowState::Hidden);

    // Bytes still in flight from the dead container arrive afterwards.
    tx.send(b"leftover output".to_vec()).unwrap();
    tab.drain_container_output();
    assert_eq!(
        tab.container_window_state,
        ContainerWindowState::Hidden,
        "a dead container's late bytes must not resurrect the window"
    );

    // The next step launches: the engine sets pty_reset_flag, after which
    // fresh output auto-opens the window again.
    tab.pty_reset_flag.store(true, Ordering::Relaxed);
    tx.send(b"next step output".to_vec()).unwrap();
    tab.drain_container_output();
    assert_eq!(tab.container_window_state, ContainerWindowState::Maximized);
}

#[test]
fn container_window_cycles() {
    assert_eq!(
        ContainerWindowState::Hidden.cycle(),
        ContainerWindowState::Maximized
    );
    assert_eq!(
        ContainerWindowState::Minimized.cycle(),
        ContainerWindowState::Maximized
    );
    assert_eq!(
        ContainerWindowState::Maximized.cycle(),
        ContainerWindowState::Minimized
    );
}

/// Reproduces TUI-3: vt100 0.15.2's `Grid::visible_rows()` panicked in
/// debug builds when `scrollback_offset > rows_len` (an unchecked
/// `rows_len - scrollback_offset` subtraction). vt100-ctt 0.17 fixes
/// the panic with `saturating_sub`, so we can scroll the full
/// configured scrollback depth (5000 lines by default) without
/// hitting an arithmetic overflow.
#[test]
fn deep_scroll_past_screen_rows_does_not_panic() {
    let mut tab = make_tab();
    tab.start_container("agent".into(), "container".into(), 80, 24);
    // Feed enough lines that the vt100 scrollback grows well past the
    // screen height. Each "line\n" becomes one row of scrollback.
    for i in 0..500 {
        let s = format!("line {i}\r\n");
        tab.focused_parser_mut().process(s.as_bytes());
    }
    // Probe depth.
    let depth = {
        let screen = tab.focused_parser_mut().screen_mut();
        screen.set_scrollback(usize::MAX);
        let d = screen.scrollback();
        screen.set_scrollback(0);
        d
    };
    assert!(
        depth > 24,
        "test setup: scrollback depth must exceed screen height; got {depth}"
    );
    // Set offset to a value much larger than screen_rows. Pre-fix
    // (vt100 0.15.2) this would panic in debug; vt100-ctt 0.17 must
    // handle it safely.
    let screen = tab.focused_parser_mut().screen_mut();
    screen.set_scrollback(depth);
    let eff = screen.scrollback();
    assert_eq!(
        eff, depth,
        "set_scrollback must clamp to depth, not screen_rows"
    );
    // Reading cells at this offset must not panic.
    let _ = screen.cell(0, 0);
    let _ = screen.cell(23, 79);
    screen.set_scrollback(0);
}

// ── truncate_with_ellipsis ─────────────────────────────────────────────────

#[test]
fn truncate_with_ellipsis_no_change_when_short() {
    assert_eq!(truncate_with_ellipsis("hello", 14), "hello");
}

#[test]
fn truncate_with_ellipsis_at_limit() {
    // Exactly 14 chars: no ellipsis.
    assert_eq!(
        truncate_with_ellipsis("aaaaaaaaaaaaaa", 14),
        "aaaaaaaaaaaaaa"
    );
}

#[test]
fn truncate_with_ellipsis_when_too_long() {
    let s = "aaaaaaaaaaaaaaaaaa"; // 18 chars
    let result = truncate_with_ellipsis(s, 14);
    assert!(result.ends_with('\u{2026}'));
    assert_eq!(result.chars().count(), 14);
}

// ── tab_subcommand_label ───────────────────────────────────────────────────

#[test]
fn tab_subcommand_label_idle_is_empty() {
    let tab = make_tab();
    assert_eq!(tab.tab_subcommand_label(20, true), "");
}

#[test]
fn tab_subcommand_label_running_returns_command() {
    let mut tab = make_tab();
    tab.execution_phase = ExecutionPhase::Running {
        command: "chat".into(),
    };
    assert_eq!(tab.tab_subcommand_label(20, true), "chat");
}

#[test]
fn tab_subcommand_label_truncates_to_fit_cell() {
    let mut tab = make_tab();
    tab.execution_phase = ExecutionPhase::Running {
        command: "very-long-subcommand-name".into(),
    };
    // tab_width=10 → max_chars=6; truncated to 5 chars + …
    let label = tab.tab_subcommand_label(10, true);
    assert!(label.ends_with('\u{2026}'));
    assert!(label.chars().count() <= 6);
}

// ── compute_tab_bar_width ──────────────────────────────────────────────────

#[test]
fn tab_bar_width_single_tab_uses_min_when_content_small() {
    // 1 tab, content 5 → natural = max(7, 20) = 20, fits in 200.
    assert_eq!(compute_tab_bar_width(1, 200, 5), 20);
}

#[test]
fn tab_bar_width_single_tab_uses_natural_when_fits() {
    // 1 tab, content 80 → natural = 82, fits in 100.
    assert_eq!(compute_tab_bar_width(1, 100, 80), 82);
}

#[test]
fn tab_bar_width_two_tabs_shrinks_when_overflow() {
    // 2 tabs, content 90 → natural = 92, total = 184 > 100. Shrink: 100/2 = 50.
    assert_eq!(compute_tab_bar_width(2, 100, 90), 50);
}

#[test]
fn tab_bar_width_three_tabs_shrinks_when_overflow() {
    // 3 tabs, content 90 → natural = 92, total = 276 > 100. Shrink: 100/3 = 33.
    assert_eq!(compute_tab_bar_width(3, 100, 90), 33);
}

#[test]
fn tab_bar_width_four_tabs_uses_min_when_content_small() {
    // 4 tabs, content 10 → natural = max(12, 20) = 20, total = 80 ≤ 100.
    assert_eq!(compute_tab_bar_width(4, 100, 10), 20);
}

#[test]
fn tab_bar_width_zero_tabs() {
    assert_eq!(compute_tab_bar_width(0, 100, 5), 0);
}

// ── phase_label ───────────────────────────────────────────────────────────

#[test]
fn phase_label_idle() {
    assert_eq!(phase_label(&ExecutionPhase::Idle), " awman ");
}

#[test]
fn phase_label_running() {
    let label = phase_label(&ExecutionPhase::Running {
        command: "chat".into(),
    });
    assert!(label.contains("running"));
    assert!(label.contains("chat"));
}

#[test]
fn phase_label_done_exit_zero_shows_checkmark() {
    let label = phase_label(&ExecutionPhase::Done {
        command: "chat".into(),
        exit_code: 0,
    });
    assert!(label.contains('✓'), "exit-0 done must use checkmark");
    assert!(label.contains("done"));
    assert!(label.contains("chat"));
}

#[test]
fn phase_label_done_nonzero_exit_shows_cross_and_code() {
    let label = phase_label(&ExecutionPhase::Done {
        command: "chat".into(),
        exit_code: 1,
    });
    assert!(label.contains('✗'), "non-zero exit must use cross");
    assert!(label.contains("exit 1"));
    assert!(label.contains("chat"));
}

#[test]
fn phase_label_error_shows_cross_and_command() {
    let label = phase_label(&ExecutionPhase::Error {
        command: "ready".into(),
        message: "something broke".into(),
    });
    assert!(label.contains('✗'));
    assert!(label.contains("error"));
    assert!(label.contains("ready"));
}

// ── window_border_color matrix ────────────────────────────────────────────

#[test]
fn window_border_color_error_always_red() {
    use ratatui::style::Color;
    let phase = ExecutionPhase::Error {
        command: "x".into(),
        message: "y".into(),
    };
    assert_eq!(window_border_color(&phase, true), Color::Red);
    assert_eq!(window_border_color(&phase, false), Color::Red);
}

#[test]
fn window_border_color_running_focused_is_blue() {
    use ratatui::style::Color;
    let phase = ExecutionPhase::Running {
        command: "x".into(),
    };
    assert_eq!(window_border_color(&phase, true), Color::Blue);
}

#[test]
fn window_border_color_running_unfocused_is_gray() {
    use ratatui::style::Color;
    let phase = ExecutionPhase::Running {
        command: "x".into(),
    };
    assert_eq!(window_border_color(&phase, false), Color::Gray);
}

#[test]
fn window_border_color_done_focused_is_green() {
    use ratatui::style::Color;
    let phase = ExecutionPhase::Done {
        command: "x".into(),
        exit_code: 0,
    };
    assert_eq!(window_border_color(&phase, true), Color::Green);
}

#[test]
fn window_border_color_done_unfocused_is_gray() {
    use ratatui::style::Color;
    let phase = ExecutionPhase::Done {
        command: "x".into(),
        exit_code: 0,
    };
    assert_eq!(window_border_color(&phase, false), Color::Gray);
}

#[test]
fn window_border_color_idle_is_dark_gray_regardless_of_focus() {
    use ratatui::style::Color;
    assert_eq!(
        window_border_color(&ExecutionPhase::Idle, true),
        Color::DarkGray
    );
    assert_eq!(
        window_border_color(&ExecutionPhase::Idle, false),
        Color::DarkGray
    );
}

// ── tab_color ─────────────────────────────────────────────────────────────

#[test]
fn tab_color_stuck_is_yellow() {
    use ratatui::style::Color;
    let mut tab = make_tab();
    tab.stuck = true;
    assert_eq!(tab_color(&tab), Color::Yellow);
}

#[test]
fn tab_color_remote_is_magenta() {
    use ratatui::style::Color;
    let mut tab = make_tab();
    tab.is_remote = true;
    assert_eq!(tab_color(&tab), Color::Magenta);
}

#[test]
fn tab_color_stuck_takes_priority_over_remote() {
    use ratatui::style::Color;
    let mut tab = make_tab();
    tab.stuck = true;
    tab.is_remote = true;
    assert_eq!(tab_color(&tab), Color::Yellow);
}

#[test]
fn tab_color_error_is_red() {
    use ratatui::style::Color;
    let mut tab = make_tab();
    tab.execution_phase = ExecutionPhase::Error {
        command: "chat".into(),
        message: "oops".into(),
    };
    assert_eq!(tab_color(&tab), Color::Red);
}

#[test]
fn tab_color_running_with_pty_container_visible_is_green() {
    use ratatui::style::Color;
    let mut tab = make_tab();
    tab.execution_phase = ExecutionPhase::Running {
        command: "chat".into(),
    };
    tab.container_window_state = ContainerWindowState::Minimized;
    assert_eq!(tab_color(&tab), Color::Green);
}

#[test]
fn tab_color_running_maximized_container_is_green() {
    use ratatui::style::Color;
    let mut tab = make_tab();
    tab.execution_phase = ExecutionPhase::Running {
        command: "chat".into(),
    };
    tab.container_window_state = ContainerWindowState::Maximized;
    assert_eq!(tab_color(&tab), Color::Green);
}

#[test]
fn tab_color_running_no_container_is_blue() {
    use ratatui::style::Color;
    let mut tab = make_tab();
    tab.execution_phase = ExecutionPhase::Running {
        command: "chat".into(),
    };
    tab.container_window_state = ContainerWindowState::Hidden;
    assert_eq!(tab_color(&tab), Color::Blue);
}

#[test]
fn tab_color_idle_is_dark_gray() {
    use ratatui::style::Color;
    let tab = make_tab();
    assert_eq!(tab_color(&tab), Color::DarkGray);
}

#[test]
fn tab_color_done_is_dark_gray() {
    use ratatui::style::Color;
    let mut tab = make_tab();
    tab.execution_phase = ExecutionPhase::Done {
        command: "chat".into(),
        exit_code: 0,
    };
    assert_eq!(tab_color(&tab), Color::DarkGray);
}

// ── strip_alternate_screen_sequences ─────────────────────────────

#[test]
fn strip_alt_screen_removes_1049h() {
    let input = b"hello\x1b[?1049hworld";
    let out = strip_alternate_screen_sequences(input);
    assert_eq!(out.bytes, b"helloworld");
    assert_eq!(out.alt_screen, Some(true));
}

#[test]
fn strip_alt_screen_removes_1049l() {
    let input = b"\x1b[?1049lafter";
    let out = strip_alternate_screen_sequences(input);
    assert_eq!(out.bytes, b"after");
    assert_eq!(out.alt_screen, Some(false));
}

#[test]
fn strip_alt_screen_removes_47h_and_47l() {
    let input = b"a\x1b[?47hb\x1b[?47lc";
    let out = strip_alternate_screen_sequences(input);
    assert_eq!(out.bytes, b"abc");
    // Last toggle in the chunk wins.
    assert_eq!(out.alt_screen, Some(false));
}

#[test]
fn strip_alt_screen_removes_1047h() {
    let input = b"\x1b[?1047hx";
    let out = strip_alternate_screen_sequences(input);
    assert_eq!(out.bytes, b"x");
    assert_eq!(out.alt_screen, Some(true));
}

#[test]
fn strip_alt_screen_preserves_other_escapes() {
    let input = b"\x1b[31mred\x1b[0m";
    let out = strip_alternate_screen_sequences(input);
    assert_eq!(out.bytes, input.to_vec());
    assert_eq!(out.alt_screen, None);
    assert_eq!(out.alternate_scroll, None);
}

#[test]
fn strip_alt_screen_passthrough_no_sequences() {
    let input = b"plain text without escapes";
    let out = strip_alternate_screen_sequences(input);
    assert_eq!(out.bytes, input.to_vec());
    assert_eq!(out.alt_screen, None);
    assert_eq!(out.alternate_scroll, None);
}

#[test]
fn strip_alt_screen_empty_input() {
    let out = strip_alternate_screen_sequences(b"");
    assert!(out.bytes.is_empty());
    assert_eq!(out.alt_screen, None);
    assert_eq!(out.alternate_scroll, None);
}

#[test]
fn strip_alt_screen_consecutive_sequences() {
    let input = b"\x1b[?1049h\x1b[?1049l";
    let out = strip_alternate_screen_sequences(input);
    assert!(out.bytes.is_empty());
    assert_eq!(out.alt_screen, Some(false));
}

#[test]
fn strip_observes_alternate_scroll_enable_without_stripping() {
    // codex's alt-screen entry: CSI ?1049h then CSI ?1007h. The 1049
    // must be stripped, the 1007 observed but left in the stream.
    let input = b"\x1b[?1049h\x1b[?1007h";
    let out = strip_alternate_screen_sequences(input);
    assert_eq!(out.bytes, b"\x1b[?1007h");
    assert_eq!(out.alt_screen, Some(true));
    assert_eq!(out.alternate_scroll, Some(true));
}

#[test]
fn strip_observes_alternate_scroll_disable() {
    // codex's alt-screen exit: CSI ?1007l then CSI ?1049l.
    let input = b"\x1b[?1007l\x1b[?1049l";
    let out = strip_alternate_screen_sequences(input);
    assert_eq!(out.bytes, b"\x1b[?1007l");
    assert_eq!(out.alt_screen, Some(false));
    assert_eq!(out.alternate_scroll, Some(false));
}

#[test]
fn drain_container_output_tracks_alt_screen_and_alternate_scroll() {
    let mut tab = make_tab();
    let tx = attach_slot_stdout(&mut tab);

    tx.send(b"\x1b[?1049h\x1b[?1007h".to_vec()).unwrap();
    tab.drain_container_output();
    let slot = tab.focused_slot().unwrap();
    assert!(slot.agent_alt_screen, "1049h must set agent_alt_screen");
    assert!(
        slot.agent_alternate_scroll,
        "1007h must set agent_alternate_scroll"
    );

    tx.send(b"\x1b[?1007l\x1b[?1049l".to_vec()).unwrap();
    tab.drain_container_output();
    let slot = tab.focused_slot().unwrap();
    assert!(!slot.agent_alt_screen, "1049l must clear agent_alt_screen");
    assert!(
        !slot.agent_alternate_scroll,
        "1007l must clear agent_alternate_scroll"
    );
}

#[test]
fn codex_inline_history_insertion_lands_in_scrollback() {
    // Reproduces codex's inline-viewport history insertion
    // (codex-rs/tui/src/insert_history.rs): a scroll region anchored at
    // the top of the screen ending above the inline viewport, the cursor
    // parked on the region's bottom row, and one "\r\n" + line per
    // history entry. Each newline scrolls the region; the rows pushed
    // off the top of the screen must accumulate in vt100 scrollback so
    // mouse-wheel scrollback has something to show. Relies on the
    // RegionScrollEmulator in the drain pipeline (vt100 alone discards
    // these rows).
    let mut tab = make_tab(); // 24x80 parser
    let tx = attach_slot_stdout(&mut tab);
    // Steady-state: the overlay is already open and the parser sized
    // (start_container). Skips drain's auto-open branch.
    tab.container_window_state = ContainerWindowState::Maximized;

    // Viewport occupies the bottom 6 rows (0-based top = row 18), so the
    // scroll region is 1-based rows 1..18.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"\x1b[1;18r"); // DECSTBM, top-anchored
    bytes.extend_from_slice(b"\x1b[18;1H"); // cursor to region bottom
    for i in 0..30 {
        bytes.extend_from_slice(format!("\r\nhistory line {i}").as_bytes());
    }
    bytes.extend_from_slice(b"\x1b[r"); // reset region
    tx.send(bytes).unwrap();
    tab.drain_container_output();

    let screen = tab.focused_parser_mut().screen_mut();
    screen.set_scrollback(usize::MAX);
    let depth = screen.scrollback();
    assert!(
        depth >= 12,
        "30 lines through an 18-row region must overflow into scrollback \
         (got depth {depth})"
    );
    let scrolled_back = screen.contents();
    screen.set_scrollback(0);
    assert!(
        scrolled_back.contains("history line 0"),
        "earliest history line must be reachable in scrollback"
    );
}

// ── Agent exit-code reporting and fast-exit output capture ──────────

fn finish_with_chat_outcome(tab: &mut Tab, exit_code: Option<i32>) {
    let (result_tx, result_rx) = std::sync::mpsc::channel::<Result<CommandOutcome, CommandError>>();
    tab.command_result_rx = Some(result_rx);
    tab.execution_phase = ExecutionPhase::Running {
        command: "chat".into(),
    };
    result_tx
        .send(Ok(CommandOutcome::Chat(
            crate::command::commands::chat::ChatOutcome {
                agent: Some("claude".into()),
                exit_code,
            },
        )))
        .unwrap();
    tab.poll_command_completion();
}

fn log_texts(tab: &Tab) -> Vec<(crate::data::message::MessageLevel, String)> {
    tab.status_log
        .lock()
        .unwrap()
        .iter()
        .map(|e| (e.level, e.text.clone()))
        .collect()
}

#[test]
fn poll_completion_reports_nonzero_agent_exit_code() {
    let mut tab = make_tab();
    finish_with_chat_outcome(&mut tab, Some(2));

    assert!(
        matches!(
            tab.execution_phase,
            ExecutionPhase::Done { exit_code: 2, .. }
        ),
        "Done phase must carry the agent's exit code: {:?}",
        tab.execution_phase
    );
    let logs = log_texts(&tab);
    assert!(
        logs.iter().any(|(level, text)| {
            *level == crate::data::message::MessageLevel::Error
                && text.contains("agent exited with code 2")
        }),
        "non-zero agent exit must be reported as an Error, got: {logs:?}"
    );
    assert!(
        !logs
            .iter()
            .any(|(_, text)| text.contains("completed successfully")),
        "non-zero agent exit must not be reported as success: {logs:?}"
    );
}

#[test]
fn poll_completion_zero_exit_reports_success() {
    let mut tab = make_tab();
    finish_with_chat_outcome(&mut tab, Some(0));

    let logs = log_texts(&tab);
    assert!(
        logs.iter().any(|(level, text)| {
            *level == crate::data::message::MessageLevel::Success
                && text.contains("completed successfully")
        }),
        "clean agent exit keeps the success message: {logs:?}"
    );
}

#[test]
fn unrendered_container_output_is_surfaced_to_status_log() {
    let mut tab = make_tab();
    let tx = attach_slot_stdout(&mut tab);

    // The agent prints an error and dies before the renderer draws a
    // single frame — drain opens the overlay, poll closes it in the same
    // tick, container_rendered stays false.
    tx.send(b"ERROR: unknown flag: --workspace-dir\r\n".to_vec())
        .unwrap();
    tab.drain_container_output();
    assert!(!tab.container_rendered);
    finish_with_chat_outcome(&mut tab, Some(1));

    let logs = log_texts(&tab);
    assert!(
        logs.iter()
            .any(|(_, text)| text.contains("before its output could be displayed")),
        "must announce the captured-output replay: {logs:?}"
    );
    assert!(
        logs.iter()
            .any(|(_, text)| text.contains("ERROR: unknown flag: --workspace-dir")),
        "the agent's dying words must land in the status log: {logs:?}"
    );
}

#[test]
fn rendered_container_output_is_not_duplicated_into_status_log() {
    let mut tab = make_tab();
    let tx = attach_slot_stdout(&mut tab);

    tx.send(b"normal session output\r\n".to_vec()).unwrap();
    tab.drain_container_output();
    // The renderer drew the overlay at least once.
    tab.container_rendered = true;
    finish_with_chat_outcome(&mut tab, Some(0));

    let logs = log_texts(&tab);
    assert!(
        !logs
            .iter()
            .any(|(_, text)| text.contains("normal session output")),
        "output the user already saw must not be replayed: {logs:?}"
    );
}

// ── WI-0096 parallel-slot behavior ───────────────────────────────────────

fn slot(name: &str) -> ContainerSlot {
    ContainerSlot::new(name.to_string(), "claude".to_string(), 1000)
}

#[test]
fn container_slots_aggregate_stuck_and_yolo_flags() {
    let mut tab = make_tab();
    tab.container_slots.push(slot("a"));
    tab.container_slots.push(slot("b"));
    // Slot-flag aggregation only runs while a parallel group is active,
    // marked by the stashed sequential backbone.
    tab.dormant_slots.push(slot(""));

    // All slots clear → aggregate is false.
    tab.drain_stuck_events();
    assert!(!tab.stuck);
    assert!(!tab.yolo_mode);

    // Any slot stuck → aggregate stuck is true.
    tab.container_slots[1].stuck = true;
    tab.drain_stuck_events();
    assert!(tab.stuck, "any stuck slot makes the tab aggregate stuck");
    assert!(!tab.yolo_mode);

    // Clear stuck, set yolo on the other slot → aggregate yolo is true.
    tab.container_slots[1].stuck = false;
    tab.container_slots[0].yolo_mode = true;
    tab.drain_stuck_events();
    assert!(!tab.stuck);
    assert!(tab.yolo_mode, "any yolo slot makes the tab aggregate yolo");

    // Everything clear again → both false.
    tab.container_slots[0].yolo_mode = false;
    tab.drain_stuck_events();
    assert!(!tab.stuck);
    assert!(!tab.yolo_mode);
}

#[test]
fn evicting_focused_slot_advances_focus_to_next_live_slot() {
    let mut tab = make_tab();
    tab.container_slots.push(slot("a"));
    tab.container_slots.push(slot("b"));
    tab.container_slots.push(slot("c"));
    tab.focused_slot_idx = 1; // focus "b"

    tab.container_slot_events
        .lock()
        .unwrap()
        .push_back(ContainerSlotEvent::Exited {
            step_name: "b".to_string(),
        });
    tab.drain_container_slot_events();

    assert_eq!(tab.active_slot_count(), 2);
    assert!(
        !tab.container_slots.iter().any(|s| s.step_name == "b"),
        "the exited slot must be gone"
    );
    assert_eq!(tab.focused_slot_idx, 1);
    assert_eq!(
        tab.focused_slot().unwrap().step_name,
        "c",
        "focus advances to the slot that shifted into the freed index"
    );
}

#[test]
fn evicting_slot_before_focused_shifts_index_down() {
    let mut tab = make_tab();
    tab.container_slots.push(slot("a"));
    tab.container_slots.push(slot("b"));
    tab.container_slots.push(slot("c"));
    tab.focused_slot_idx = 2; // focus "c"

    tab.container_slot_events
        .lock()
        .unwrap()
        .push_back(ContainerSlotEvent::Exited {
            step_name: "a".to_string(),
        });
    tab.drain_container_slot_events();

    assert_eq!(tab.active_slot_count(), 2);
    assert_eq!(tab.focused_slot_idx, 1, "index shifts down by one");
    assert_eq!(
        tab.focused_slot().unwrap().step_name,
        "c",
        "the same slot stays focused after the shift"
    );
}

#[test]
fn evicting_last_slot_hides_the_container_window() {
    let mut tab = make_tab();
    tab.container_slots.push(slot("a"));
    tab.container_window_state = ContainerWindowState::Maximized;

    tab.container_slot_events
        .lock()
        .unwrap()
        .push_back(ContainerSlotEvent::Exited {
            step_name: "a".to_string(),
        });
    tab.drain_container_slot_events();

    assert_eq!(tab.active_slot_count(), 0);
    assert_eq!(tab.focused_slot_idx, 0);
    assert_eq!(tab.container_window_state, ContainerWindowState::Hidden);
}

#[test]
fn cycle_focused_slot_advances_cyclically_through_three_slots() {
    let mut tab = make_tab();
    tab.container_slots.push(slot("a"));
    tab.container_slots.push(slot("b"));
    tab.container_slots.push(slot("c"));

    assert_eq!(tab.focused_slot_idx, 0);
    tab.cycle_focused_slot();
    assert_eq!(tab.focused_slot_idx, 1);
    tab.cycle_focused_slot();
    assert_eq!(tab.focused_slot_idx, 2);
    tab.cycle_focused_slot();
    assert_eq!(
        tab.focused_slot_idx, 0,
        "one full cycle of three slots returns to slot 0"
    );
}

#[test]
fn cycle_focused_slot_is_noop_with_a_single_slot() {
    let mut tab = make_tab();
    tab.container_slots.push(slot("a"));
    tab.cycle_focused_slot();
    assert_eq!(tab.focused_slot_idx, 0);
}

#[test]
fn cycle_focused_slot_resets_scrollback_to_live_view() {
    let mut tab = make_tab();
    tab.container_slots.push(slot("a"));
    tab.container_slots.push(slot("b"));
    tab.container_scroll_offset = 42;
    tab.cycle_focused_slot();
    assert_eq!(
        tab.container_scroll_offset, 0,
        "the rotated-in slot must start at its live view"
    );
}

#[test]
fn launched_slot_parser_is_sized_to_the_overlay_not_80x24() {
    let mut tab = make_tab();
    // The renderer published the overlay's inner rect on a prior frame.
    tab.container_inner_area = Some(ratatui::layout::Rect::new(1, 1, 150, 40));

    let (resize_tx, mut resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();
    let (_stdout_tx, stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let (stdin_tx, _stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    tab.container_slot_events
        .lock()
        .unwrap()
        .push_back(ContainerSlotEvent::Launched {
            step_name: "build".to_string(),
            agent: "claude".to_string(),
            model: None,
            io: Some(ContainerSlotIo {
                stdout_rx,
                stdin_tx,
                resize_tx,
            }),
        });
    tab.drain_container_slot_events();

    let (rows, cols) = tab.container_slots[0].vt100_parser.screen().size();
    assert_eq!(
        (cols, rows),
        (150, 40),
        "the fresh slot parser must match the overlay, not the 80x24 default"
    );
    assert_eq!(
        resize_rx.try_recv().ok(),
        Some((150, 40)),
        "the container PTY must receive the real size at launch"
    );
}

#[test]
fn container_name_event_updates_the_matching_slot() {
    let mut tab = make_tab();
    tab.container_slots.push(slot("build"));
    tab.container_slots.push(slot("test"));

    tab.container_slot_events
        .lock()
        .unwrap()
        .push_back(ContainerSlotEvent::ContainerName {
            step_name: "test".to_string(),
            container_name: "awman-test-4242".to_string(),
        });
    tab.drain_container_slot_events();

    assert_eq!(
        tab.container_slots[1]
            .container_info
            .as_ref()
            .unwrap()
            .container_name,
        "awman-test-4242"
    );
    assert!(
        tab.container_slots[0]
            .container_info
            .as_ref()
            .unwrap()
            .container_name
            .is_empty(),
        "the other slot's name must be untouched"
    );
}

#[test]
fn parallel_output_tracks_per_slot_alt_screen_flags() {
    let mut tab = make_tab();
    let mut s = slot("build");
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    s.container_stdout_rx = Some(rx);
    tab.container_slots.push(s);
    tab.container_slots.push(slot("test"));

    // Agent enables the alternate screen and alternate scroll (codex-style).
    tx.send(b"\x1b[?1049h\x1b[?1007h".to_vec()).unwrap();
    tab.drain_container_output();

    assert!(tab.container_slots[0].agent_alt_screen);
    assert!(tab.container_slots[0].agent_alternate_scroll);
    assert!(
        !tab.container_slots[1].agent_alt_screen,
        "flags are per-slot, not shared"
    );
}

#[test]
fn first_parallel_output_auto_opens_the_container_window() {
    let mut tab = make_tab();
    let mut s = slot("build");
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    s.container_stdout_rx = Some(rx);
    tab.container_slots.push(s);
    assert_eq!(tab.container_window_state, ContainerWindowState::Hidden);

    tx.send(b"hello from the agent".to_vec()).unwrap();
    tab.drain_container_output();

    assert_eq!(
        tab.container_window_state,
        ContainerWindowState::Maximized,
        "a workflow starting directly with a parallel group must open the overlay"
    );
}

#[test]
fn container_overlay_active_requires_a_slot_and_maximized() {
    let mut tab = make_tab();
    // No slots: never active, regardless of window state.
    tab.container_window_state = ContainerWindowState::Maximized;
    assert!(!tab.container_overlay_active());

    // With a slot: only Maximized shows the overlay; Minimized renders
    // every slot as a status bar instead.
    tab.container_slots.push(slot("a"));
    assert!(tab.container_overlay_active());
    tab.container_window_state = ContainerWindowState::Minimized;
    assert!(!tab.container_overlay_active());
    tab.container_window_state = ContainerWindowState::Hidden;
    assert!(!tab.container_overlay_active());
}

#[test]
fn group_started_stashes_backbone_and_group_finished_restores_it() {
    let mut tab = make_tab();
    // The command-level backbone slot (as spawn_command installs it).
    tab.start_container("claude".into(), "awman-backbone".into(), 80, 24);

    // Parallel group starts: the backbone goes dormant, group slots join.
    {
        let mut q = tab.container_slot_events.lock().unwrap();
        q.push_back(ContainerSlotEvent::GroupStarted);
        q.push_back(ContainerSlotEvent::Launched {
            step_name: "a".into(),
            agent: "claude".into(),
            model: None,
            io: None,
        });
        q.push_back(ContainerSlotEvent::Launched {
            step_name: "b".into(),
            agent: "codex".into(),
            model: None,
            io: None,
        });
    }
    tab.drain_container_slot_events();
    assert_eq!(tab.active_slot_count(), 2, "only the group slots display");
    assert_eq!(tab.dormant_slots.len(), 1, "the backbone is stashed");

    // Group drains and finishes: the backbone is restored.
    {
        let mut q = tab.container_slot_events.lock().unwrap();
        q.push_back(ContainerSlotEvent::Exited {
            step_name: "a".into(),
        });
        q.push_back(ContainerSlotEvent::Exited {
            step_name: "b".into(),
        });
        q.push_back(ContainerSlotEvent::GroupFinished);
    }
    tab.drain_container_slot_events();
    assert_eq!(tab.active_slot_count(), 1);
    assert!(tab.dormant_slots.is_empty());
    assert_eq!(
        tab.focused_slot()
            .and_then(|s| s.container_info.as_ref())
            .map(|i| i.container_name.as_str()),
        Some("awman-backbone"),
        "the restored slot is the original backbone"
    );
}

// ── GroupStarted resets stuck summary bar and unblocks auto-open ──────

#[test]
fn group_started_evicts_summary_bar_and_unblocks_auto_open() {
    let mut tab = make_tab();
    tab.start_container("claude".into(), "awman-leader".into(), 80, 24);
    tab.container_window_state = ContainerWindowState::Maximized;
    tab.container_rendered = true;

    // Leader is killed — leaves a red summary bar and suppresses auto-open.
    *tab.container_exit_shared.lock().unwrap() = Some(137);
    tab.poll_container_exit();
    assert_eq!(tab.container_window_state, ContainerWindowState::Hidden);
    assert!(tab.last_container_summary.is_some());
    assert!(tab.suppress_container_auto_open);

    // Engine fires GroupStarted for the first parallel group.
    tab.container_slot_events
        .lock()
        .unwrap()
        .push_back(ContainerSlotEvent::GroupStarted);
    tab.drain_container_slot_events();
    assert!(
        tab.last_container_summary.is_none(),
        "GroupStarted must evict the stuck summary bar"
    );
    assert!(
        !tab.suppress_container_auto_open,
        "GroupStarted must unblock auto-open for new group containers"
    );

    // First container in the new group launches and produces output.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    tab.container_slot_events
        .lock()
        .unwrap()
        .push_back(ContainerSlotEvent::Launched {
            step_name: "build".to_string(),
            agent: "claude".to_string(),
            model: None,
            io: Some(ContainerSlotIo {
                stdout_rx: rx,
                stdin_tx: tokio::sync::mpsc::unbounded_channel().0,
                resize_tx: tokio::sync::mpsc::unbounded_channel().0,
            }),
        });
    tab.drain_container_slot_events();
    tx.send(b"building...".to_vec()).unwrap();
    tab.drain_container_output();
    assert_eq!(
        tab.container_window_state,
        ContainerWindowState::Maximized,
        "first output from the new group must auto-open the window"
    );
}

#[test]
fn yolo_started_shares_cancel_flag_and_tick_updates_slot_state() {
    let mut tab = make_tab();
    tab.container_slots.push(slot("a"));
    tab.container_slots.push(slot("b"));

    let cancel_flag: SharedYoloCancelFlag = Arc::new(AtomicBool::new(false));
    {
        let mut q = tab.container_slot_events.lock().unwrap();
        q.push_back(ContainerSlotEvent::YoloStarted {
            step_name: "b".into(),
            cancel_flag: cancel_flag.clone(),
        });
        q.push_back(ContainerSlotEvent::YoloTick {
            step_name: "b".into(),
            remaining_secs: 42,
        });
    }
    tab.drain_container_slot_events();

    let b = tab
        .container_slots
        .iter()
        .find(|s| s.step_name == "b")
        .unwrap();
    assert!(b.yolo_mode, "yolo_mode set on the ticking slot only");
    assert_eq!(
        b.yolo_state
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.remaining_secs),
        Some(42)
    );
    // The slot's cancel flag is the SAME Arc the engine-side frontend
    // holds, so setting it here (as Esc does) is visible to the engine's
    // next `parallel_step_yolo_countdown_tick` check.
    assert!(Arc::ptr_eq(&b.yolo_cancel_flag, &cancel_flag));

    let a = tab
        .container_slots
        .iter()
        .find(|s| s.step_name == "a")
        .unwrap();
    assert!(!a.yolo_mode, "sibling slot is untouched");
    assert!(a.yolo_state.lock().unwrap().is_none());

    // Finishing clears both the flag and the displayed countdown.
    {
        let mut q = tab.container_slot_events.lock().unwrap();
        q.push_back(ContainerSlotEvent::YoloFinished {
            step_name: "b".into(),
        });
    }
    tab.drain_container_slot_events();
    let b = tab
        .container_slots
        .iter()
        .find(|s| s.step_name == "b")
        .unwrap();
    assert!(!b.yolo_mode);
    assert!(b.yolo_state.lock().unwrap().is_none());
}

// ── amie tab (WI 0102) ──────────────────────────────────────────────────

/// A tab whose `is_amie` bit is set, for the `tab_color`/`new_amie` tests
/// below. Its session basename is deliberately unrelated to "amie" — mirrors
/// the fact that the real synthetic session is rooted at whatever
/// `AWMAN_AMIE_ROOT` resolves to, which `project_name` must never leak.
fn make_amie_tab() -> Tab {
    Tab::new_amie(make_test_session())
}

#[test]
fn tab_color_amie_is_cyan_regardless_of_execution_phase() {
    use ratatui::style::Color;
    // D8 (implementation-contract.md §0.0): `is_amie` sits at the same tier
    // as `is_remote`, below `stuck`/yolo but above every execution-phase
    // colour. Vary phase and container-window state; the colour must never
    // move off Cyan.
    let mut tab = make_amie_tab();
    assert_eq!(tab_color(&tab), Color::Cyan, "idle amie tab");

    tab.execution_phase = ExecutionPhase::Running {
        command: "chat".into(),
    };
    tab.container_window_state = ContainerWindowState::Hidden;
    assert_eq!(tab_color(&tab), Color::Cyan, "running, no container");

    tab.container_window_state = ContainerWindowState::Maximized;
    assert_eq!(tab_color(&tab), Color::Cyan, "running, container visible");

    tab.execution_phase = ExecutionPhase::Done {
        command: "chat".into(),
        exit_code: 0,
    };
    assert_eq!(tab_color(&tab), Color::Cyan, "done");

    tab.execution_phase = ExecutionPhase::Error {
        command: "chat".into(),
        message: "oops".into(),
    };
    assert_eq!(tab_color(&tab), Color::Cyan, "error phase");
}

#[test]
fn tab_color_stuck_takes_priority_over_amie() {
    // Companion to `tab_color_stuck_takes_priority_over_remote`: D8 narrowed
    // the amie unit test to "regardless of execution phase" only — `stuck`
    // legitimately wins at the tier above, exactly as it does over `is_remote`.
    use ratatui::style::Color;
    let mut tab = make_amie_tab();
    tab.stuck = true;
    assert_eq!(tab_color(&tab), Color::Yellow);
}

#[test]
fn project_name_is_amie_regardless_of_session_basename() {
    // The synthetic session's working-dir basename comes from wherever
    // `AWMAN_AMIE_ROOT` resolves to (see `App::amie_synthetic_session`), which
    // has nothing to do with "amie" in general. `project_name` must ignore it
    // entirely for an `is_amie` tab.
    let (mut tab, _tmp) = make_named_tab("totally-unrelated-directory-name");
    tab.is_amie = true;
    assert_eq!(tab.project_name(30), "amie");
    // Narrow tabs must not truncate the fixed "amie" label either.
    assert_eq!(tab.project_name(20), "amie");
}

#[test]
fn new_amie_starts_no_git_poll() {
    let tab = make_amie_tab();
    assert!(tab.is_amie, "Tab::new_amie must set is_amie");
    assert!(tab.amie.is_some(), "Tab::new_amie must install AmieTabState");
    assert!(
        tab.git_poll_root.is_none(),
        "Tab::new_amie must not start a git poll (no meaningful diff for the amie storage root)"
    );
    assert!(
        tab.git_poll_handle.is_none(),
        "Tab::new_amie must not spawn a git poll task"
    );
}
