//! Terminal setup/teardown, the main render→input event loop, panic
//! recovery, and terminal-resize handling.

use std::io;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;

use super::app::App;
use super::tabs::{self, ContainerWindowState};
use super::{git_sidebar, key_handler, mouse_handler, render, TUI_ACTIVE};

/// Restore the terminal to a clean state. Idempotent and best-effort: each
/// step is attempted independently so a failure in one doesn't leave later
/// steps un-run. Called from both the normal teardown path and the panic
/// hook so an unexpected panic doesn't leave the shell in raw mode with the
/// kitty keyboard protocol still active.
fn restore_terminal(keyboard_enhanced: bool) {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    if keyboard_enhanced {
        let _ = execute!(stdout, crossterm::event::PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        stdout,
        LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture,
        crossterm::cursor::Show,
    );
}

/// Where TUI panics are recorded: `$HOME/.awman/panic.log`. `None` when the
/// home directory can't be resolved.
pub(super) fn panic_log_path() -> Option<std::path::PathBuf> {
    Some(dirs::home_dir()?.join(".awman").join("panic.log"))
}

/// Append a panic report (message, location, thread, backtrace) to
/// [`panic_log_path`]. Best-effort — a panic hook must never itself panic
/// or block on errors.
fn log_panic_to_file(info: &std::panic::PanicHookInfo<'_>) {
    let Some(path) = panic_log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let thread = std::thread::current();
    let entry = format!(
        "──── panic at {} ────\nthread: {}\n{}\nbacktrace:\n{}\n",
        chrono::Utc::now().to_rfc3339(),
        thread.name().unwrap_or("<unnamed>"),
        info,
        std::backtrace::Backtrace::force_capture(),
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write as _;
        let _ = f.write_all(entry.as_bytes());
    }
}

/// Set up the terminal, run the main loop, and restore on exit.
pub(super) fn run_event_loop(app: &mut App) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();

    // Enable the kitty keyboard protocol so the terminal can distinguish
    // modifier+key combos (e.g. Ctrl+Enter vs bare Enter). Terminals that
    // don't support this silently ignore the escape sequence.
    let keyboard_enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if keyboard_enhanced {
        execute!(
            stdout,
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        )?;
    }

    execute!(
        stdout,
        EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;

    // Install a panic hook that restores the terminal before the default
    // hook prints the panic message — without this, a panic inside the
    // event loop would leave the shell in raw mode with the kitty
    // keyboard protocol pushed, so every keystroke (arrows, Ctrl-C, …)
    // would appear as a literal escape sequence in the user's prompt.
    //
    // Every panic is also appended to `$HOME/.awman/panic.log` with a full
    // backtrace. For panics on a worker thread (e.g. a spawned command task)
    // the TUI keeps running and would immediately repaint over anything the
    // default hook printed, so the log file is the only readable record —
    // the terminal is left alone and the default hook is skipped.
    let original_hook = std::panic::take_hook();
    let main_thread = std::thread::current().id();
    std::panic::set_hook(Box::new(move |info| {
        log_panic_to_file(info);
        if std::thread::current().id() == main_thread {
            TUI_ACTIVE.store(false, Ordering::Relaxed);
            restore_terminal(keyboard_enhanced);
            original_hook(info);
        }
    }));

    TUI_ACTIVE.store(true, Ordering::Relaxed);

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = main_loop(&mut terminal, app);

    TUI_ACTIVE.store(false, Ordering::Relaxed);
    restore_terminal(keyboard_enhanced);
    let _ = std::panic::take_hook();

    result
}

/// The main event loop: render → tick → poll → handle input → repeat.
fn main_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> io::Result<()> {
    loop {
        if app.should_quit {
            break;
        }

        terminal.draw(|frame| {
            render::render_frame(app, frame);
        })?;

        app.tick_all_tabs();
        app.poll_dialog_requests();

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key_event) => {
                    if key_event.kind != KeyEventKind::Press {
                        continue;
                    }
                    key_handler::handle_key_event(app, key_event);
                }
                Event::Mouse(mouse) => {
                    mouse_handler::handle_mouse_event(app, mouse);
                }
                Event::Resize(cols, rows) => {
                    handle_resize(app, cols, rows);
                }
                _ => {}
            }
        }
    }
    Ok(())
}

// ─── Resize ──────────────────────────────────────────────────────────────────

fn handle_resize(app: &mut App, cols: u16, rows: u16) {
    for tab in &mut app.tabs {
        tab.mouse_selection = None;
        if tab.container_window_state != ContainerWindowState::Hidden {
            let sidebar = git_sidebar::sidebar_width(cols, tab.git_sidebar_state);
            let left_cols = cols.saturating_sub(sidebar);
            let (inner_cols, inner_rows) = compute_container_inner_size(left_cols, rows);
            resize_slots(tab, inner_cols, inner_rows);
        }
    }
}

/// Resize every slot's parser and PTY to the estimated overlay inner size
/// for the current terminal dimensions. Broadcast to all slots (not just the
/// focused one) so each container tracks the real size even while minimized;
/// the per-tick sync in `tick_all_tabs` corrects any residual mismatch
/// against the actually-rendered overlay rect. Forwarding the size to the
/// PTY master triggers the SIGWINCH that reflows TUI apps inside the
/// container.
pub(super) fn resize_slots_to_terminal(tab: &mut tabs::Tab) {
    if let Ok((cols, rows)) = crossterm::terminal::size() {
        let sidebar = git_sidebar::sidebar_width(cols, tab.git_sidebar_state);
        let left_cols = cols.saturating_sub(sidebar);
        let (inner_cols, inner_rows) = compute_container_inner_size(left_cols, rows);
        resize_slots(tab, inner_cols, inner_rows);
    }
}

fn resize_slots(tab: &mut tabs::Tab, inner_cols: u16, inner_rows: u16) {
    for slot in &mut tab.container_slots {
        slot.vt100_parser
            .screen_mut()
            .set_size(inner_rows, inner_cols);
        if let Some(ref tx) = slot.container_resize_tx {
            let _ = tx.send((inner_cols, inner_rows));
        }
    }
}

/// Compute the vt100 grid size that fits inside the container overlay,
/// accounting for the 95% sizing within the execution window area and the
/// 2-cell border subtraction. The container window lives between the tab
/// bar (3 rows) and the bottom chrome (5 rows: status bar + command box +
/// suggestion row), plus any Workflow Overview or extra bar below.
///
/// `extra_bottom` accounts for the Workflow Overview height and the
/// minimized/summary bar (3 rows each when present). Callers that don't
/// know the exact extra height can pass 0 for a best-effort estimate.
pub(super) fn compute_container_inner_size(term_cols: u16, term_rows: u16) -> (u16, u16) {
    compute_container_inner_size_with_extra(term_cols, term_rows, 0)
}

fn compute_container_inner_size_with_extra(
    term_cols: u16,
    term_rows: u16,
    extra_bottom: u16,
) -> (u16, u16) {
    let exec_height = term_rows.saturating_sub(8 + extra_bottom); // 3 top + 5 bottom + extras
    let outer_cols = ((term_cols as u32 * 95 / 100) as u16).max(10);
    let outer_rows = ((exec_height as u32 * 95 / 100) as u16).max(5);
    (outer_cols.saturating_sub(2), outer_rows.saturating_sub(2))
}
