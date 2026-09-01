//! WI 0102 — the TUI squad tab: `App::open_or_focus_squad_tab` (idempotency and
//! failure surfacing), that opening it auto-spawns nothing, that Ctrl-A/Ctrl-D
//! tab cycling includes it with its sub-view state surviving defocus, that its
//! poller pauses while unfocused, that a stopped daemon renders an explicit
//! "daemon not reachable" state rather than an empty task list, and that
//! last-tab-closes-the-app protection applies to it with no special case.
//!
//! `key_handler`/`event_loop` are private modules, so the actual Ctrl-A/Ctrl-D
//! *keystrokes* are covered in-crate (`src/frontend/tui/tests/key_handler_tests.rs`);
//! this file exercises the same public methods those key bindings call
//! (`App::switch_to_prev_tab`/`switch_to_next_tab`, `App::tick_all_tabs`,
//! `App::open_or_focus_squad_tab`, `App::close_active_tab`) directly, plus the
//! public `render_frame` entry point for the rendering assertion.
//!
//! `App::build_squad_tab` (the daemon-backed constructor `tui::run` and
//! `open_or_focus_squad_tab` share) is `pub(crate)` — deliberately not part of
//! the public API — so tests that need a squad tab to already exist build one
//! directly with `Tab::new_squad`, exactly as `build_squad_tab` itself does
//! after `ensure_running` succeeds. The one test that must exercise a real
//! failure path (`open_or_focus_squad_tab` with no container runtime) forces
//! the sandbox-refusal branch, which is deterministic and touches no real
//! daemon or filesystem state — see `tests/squad_sandbox_refusal.rs` for the
//! same pattern applied to daemon startup.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;

use awman::command::commands::squad::gateway::{CreateTask, DaemonStatus, TaskGateway};
use awman::command::dispatch::catalogue::CommandCatalogue;
use awman::command::dispatch::Engines;
use awman::command::error::CommandError;
use awman::data::fs::{ApiPaths, AuthPathResolver, MountScope, Run, Task, TaskStatus};
use awman::data::session::{Session, SessionOpenOptions, StaticGitRootResolver};
use awman::data::session_manager::SessionManager;
use awman::data::EngineWorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::frontend::tui::app::App;
use awman::frontend::tui::squad_poll::{SquadTaskPoller, SQUAD_POLL_INTERVAL};
use awman::frontend::tui::tabs::squad_state::SquadTabState;
use awman::frontend::tui::tabs::{tab_color, ContainerSlot, Tab};

// ─── Shared fixtures ────────────────────────────────────────────────────────

fn make_session() -> Session {
    let tmp = tempfile::tempdir().unwrap();
    let resolver = StaticGitRootResolver::new(tmp.path());
    Session::open(
        tmp.path().to_path_buf(),
        &resolver,
        SessionOpenOptions::default(),
    )
    .unwrap()
}

/// Mirrors `src/frontend/tui/tests/mod.rs::make_engines`, with the container
/// runtime made optional so the sandbox-refusal path can be exercised without
/// any real Docker/daemon dependency.
fn make_engines(with_container_runtime: bool) -> Engines {
    let runtime = Arc::new(ContainerRuntime::docker());
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(
        AuthPathResolver::at_home(std::path::PathBuf::from("/tmp")),
    ));
    let git_engine = Arc::new(GitEngine::new());
    let agent_engine = Arc::new(AgentEngine::new(overlay.clone(), runtime.clone()));
    let auth_engine = Arc::new(AuthEngine::with_paths(
        AuthPathResolver::at_home("/tmp"),
        ApiPaths::at_root("/tmp"),
    ));
    let workflow_state_store = {
        let tmp = tempfile::tempdir().unwrap();
        Arc::new(EngineWorkflowStateStore::at_git_root(tmp.path()))
    };
    Engines {
        runtime: runtime.clone(),
        container_runtime: if with_container_runtime {
            Some(runtime)
        } else {
            None
        },
        sandbox_runtime: None,
        git_engine,
        overlay_engine: overlay,
        auth_engine,
        agent_engine,
        workflow_state_store,
    }
}

fn make_app(with_container_runtime: bool) -> App {
    let rt = Box::leak(Box::new(tokio::runtime::Runtime::new().unwrap()));
    let catalogue = CommandCatalogue::get();
    let engines = make_engines(with_container_runtime);
    let session_manager = Arc::new(RwLock::new(SessionManager::in_memory()));
    let tab = Tab::new(make_session());
    App::new(
        catalogue,
        engines,
        session_manager,
        tab,
        rt.handle().clone(),
    )
}

/// An `App` whose only tab is the squad tab, built the same way
/// `App::build_squad_tab` builds one after a successful `ensure_running` (minus
/// the gateway/poller, which the individual tests that need them install).
fn make_squad_only_app() -> App {
    let rt = Box::leak(Box::new(tokio::runtime::Runtime::new().unwrap()));
    let catalogue = CommandCatalogue::get();
    let engines = make_engines(true);
    let session_manager = Arc::new(RwLock::new(SessionManager::in_memory()));
    let tab = Tab::new_squad(make_session());
    App::new(
        catalogue,
        engines,
        session_manager,
        tab,
        rt.handle().clone(),
    )
}

fn fake_task(name: &str) -> Task {
    let now = chrono::Utc::now();
    Task {
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

fn render_app(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| awman::frontend::tui::render::render_frame(app, frame))
        .unwrap();
    terminal.backend().buffer().clone()
}

// ─── open_or_focus_squad_tab: idempotency and failure surfacing ────────────

#[test]
fn open_or_focus_squad_tab_focuses_existing_tab_without_duplicating() {
    let mut app = make_app(true);
    let squad_tab = Tab::new_squad(make_session());
    app.tabs.push(squad_tab);
    let squad_idx = app.tabs.len() - 1;
    app.active_tab = 0; // switch away before the second call

    app.open_or_focus_squad_tab();

    assert_eq!(
        app.active_tab, squad_idx,
        "a second call must focus the existing squad tab"
    );
    assert_eq!(
        app.tabs.iter().filter(|t| t.is_squad).count(),
        1,
        "must not push a duplicate squad tab"
    );
}

#[test]
fn open_or_focus_squad_tab_opens_no_tab_and_surfaces_the_error_on_failure() {
    // No container runtime -> `build_squad_tab`'s `require_container_tier`
    // check refuses before ever touching `SquadSupervisor`/a real daemon,
    // exactly the sandbox-refusal branch `implementation-contract.md` §2.6
    // describes. Deterministic, no Docker required.
    let mut app = make_app(false);
    let tabs_before = app.tabs.len();

    app.open_or_focus_squad_tab();

    assert_eq!(
        app.tabs.len(),
        tabs_before,
        "no tab must be created on failure"
    );
    assert!(
        !app.tabs.iter().any(|t| t.is_squad),
        "no squad tab must exist after a failed open"
    );
    assert!(
        app.status_bar.text.contains("squad requires a container runtime"),
        "the specific failure reason must land in the status bar verbatim, not a generic message: {:?}",
        app.status_bar.text
    );
}

// ─── creating the squad tab auto-spawns nothing ─────────────────────────────

#[test]
fn creating_the_squad_tab_auto_spawns_nothing() {
    let app = make_squad_only_app();
    let tab = &app.tabs[0];
    assert!(tab.is_squad);
    assert!(
        tab.command_result_rx.is_none(),
        "no command (ready / status --watch) must have been spawned into a fresh squad tab"
    );
    assert!(
        matches!(
            tab.execution_phase,
            awman::frontend::tui::tabs::ExecutionPhase::Idle
        ),
        "a fresh squad tab must stay Idle — spawn_command always flips this to Running"
    );
    assert!(
        tab.container_slots.is_empty(),
        "no container slot must exist — spawn_command always installs one"
    );
}

// ─── Ctrl-A/Ctrl-D tab cycling and defocus survival ────────────────────────

#[test]
fn tab_cycle_includes_the_squad_tab_and_selection_survives_a_defocus_round_trip() {
    let mut app = make_app(true); // tab 0, ordinary
    app.add_tab(make_session()); // tab 1, ordinary
    app.tabs.push(Tab::new_squad(make_session())); // tab 2 == squad
    let squad_idx = app.tabs.len() - 1;
    app.active_tab = squad_idx;

    {
        let state = app.tabs[squad_idx].squad.as_mut().unwrap();
        let mut snap = state.snapshot.lock().unwrap();
        snap.tasks = vec![fake_task("a"), fake_task("b")];
    }
    app.tabs[squad_idx]
        .squad
        .as_mut()
        .unwrap()
        .move_selection(1);
    assert_eq!(app.tabs[squad_idx].squad.as_ref().unwrap().selected, 1);

    // `Action::NextTab`/`Action::PreviousTab` call exactly these two methods.
    app.switch_to_next_tab();
    assert_eq!(
        app.active_tab, 0,
        "next tab from the last tab must wrap around to 0"
    );

    app.switch_to_prev_tab();
    assert_eq!(
        app.active_tab, squad_idx,
        "previous tab from 0 must wrap around to the squad tab"
    );

    assert_eq!(
        app.tabs[squad_idx].squad.as_ref().unwrap().selected,
        1,
        "the squad sub-view selection must survive the defocus/refocus round trip"
    );
}

#[test]
fn tick_all_tabs_flips_focused_based_on_the_active_tab() {
    let mut app = make_squad_only_app();
    app.add_tab(make_session()); // tab 1, ordinary; squad tab (0) still active

    app.tick_all_tabs();
    assert!(
        app.tabs[0]
            .squad
            .as_ref()
            .unwrap()
            .focused
            .load(Ordering::Relaxed),
        "the active squad tab must be focused=true after a tick"
    );

    app.active_tab = 1;
    app.tick_all_tabs();
    assert!(
        !app.tabs[0]
            .squad
            .as_ref()
            .unwrap()
            .focused
            .load(Ordering::Relaxed),
        "a defocused squad tab must flip to focused=false"
    );
}

// ─── polling pauses while unfocused ─────────────────────────────────────────

struct RecordingGateway {
    list_calls: AtomicUsize,
}

#[async_trait::async_trait]
impl TaskGateway for RecordingGateway {
    async fn create(&self, _req: CreateTask) -> Result<Task, CommandError> {
        unimplemented!("not exercised by this test")
    }
    async fn list(&self) -> Result<Vec<Task>, CommandError> {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }
    async fn get(&self, _name: &str) -> Result<Task, CommandError> {
        unimplemented!("not exercised by this test")
    }
    async fn runs(&self, _name: &str, _limit: usize) -> Result<Vec<Run>, CommandError> {
        unimplemented!("not exercised by this test")
    }
    async fn set_status(&self, _name: &str, _status: TaskStatus) -> Result<(), CommandError> {
        unimplemented!("not exercised by this test")
    }
    async fn delete(&self, _name: &str) -> Result<(), CommandError> {
        unimplemented!("not exercised by this test")
    }
    async fn status(&self) -> Result<DaemonStatus, CommandError> {
        unimplemented!("not exercised by this test")
    }
}

#[tokio::test]
async fn poller_skips_its_fetch_while_the_tab_is_unfocused() {
    let state = SquadTabState::new();
    let gateway = Arc::new(RecordingGateway {
        list_calls: AtomicUsize::new(0),
    });
    let poller = SquadTaskPoller::new(gateway.clone(), &state);
    let cancel = state.cancel.clone();
    let handle = poller.start(cancel.clone());

    // `SquadTabState::new()` starts unfocused; only `App::tick_all_tabs` ever
    // flips it. A full interval (plus slack) must pass with zero fetches.
    assert!(!state.focused.load(Ordering::Relaxed));
    tokio::time::sleep(SQUAD_POLL_INTERVAL + Duration::from_millis(800)).await;
    assert_eq!(
        gateway.list_calls.load(Ordering::Relaxed),
        0,
        "an unfocused poller must perform no fetch"
    );

    state.focused.store(true, Ordering::Relaxed);
    tokio::time::sleep(SQUAD_POLL_INTERVAL + Duration::from_millis(800)).await;
    assert!(
        gateway.list_calls.load(Ordering::Relaxed) >= 1,
        "a focused poller must resume fetching"
    );

    cancel.cancel();
    let _ = handle.await;
}

// ─── daemon stopped: explicit "not reachable" state, never an empty list ──

#[test]
fn daemon_stopped_squad_tab_renders_daemon_not_reachable_not_an_empty_list() {
    let mut app = make_squad_only_app();
    {
        let state = app.tabs[0].squad.as_ref().unwrap();
        let mut snap = state.snapshot.lock().unwrap();
        // Last-known tasks from before the daemon went down.
        snap.tasks = vec![fake_task("still-known")];
        snap.error = Some("connection refused".to_string());
        snap.loaded = true;
    }

    let buf = render_app(&mut app, 80, 24);
    let text = buffer_text(&buf);

    assert!(
        text.contains("squad daemon not reachable"),
        "the explicit daemon-down state must render: {text}"
    );
    assert!(
        text.contains("connection refused"),
        "the underlying error must render verbatim: {text}"
    );
    assert!(
        text.contains("still-known"),
        "the last-known tasks must survive — never blanked to an empty list: {text}"
    );
}

// ─── `awman squad` (bare) opens exactly one tab: the squad tab ───────────────

#[test]
fn bare_squad_initial_tab_yields_exactly_one_active_distinctly_colored_squad_tab() {
    // `main.rs` routes bare, interactive `awman squad` to `tui::InitialTab::Squad`,
    // which (per its doc comment in src/frontend/tui/mod.rs) constructs the App
    // with `Tab::new_squad(synthetic_session)` as the ONLY tab and
    // `active_tab = 0`, then runs a real event loop `tui::run` can't be
    // exercised headlessly. `make_squad_only_app` builds that exact shape.
    let app = make_squad_only_app();
    assert_eq!(
        app.tabs.len(),
        1,
        "InitialTab::Squad opens no directory-bound tab alongside the squad tab"
    );
    assert!(app.tabs[0].is_squad);
    assert_eq!(app.active_tab, 0, "the squad tab must be the active tab");
    assert_eq!(
        tab_color(&app.tabs[0]),
        ratatui::style::Color::Cyan,
        "the squad tab must render with its own distinct colour"
    );
}

// ─── last-tab protection applies to the squad tab, no special case ─────────

#[test]
fn last_tab_protection_applies_to_a_lone_squad_tab() {
    let mut app = make_squad_only_app();
    assert_eq!(app.tabs.len(), 1);

    app.close_active_tab();

    assert!(
        app.should_quit,
        "closing the only tab — even the squad tab — must quit, exactly like any other lone tab"
    );
    assert_eq!(
        app.tabs.len(),
        1,
        "the tab itself is not removed; should_quit is set instead"
    );
}

// ─── closing the squad tab mid-attach ends the exec session only ────────────

#[test]
fn closing_the_squad_tab_mid_attach_removes_it_stops_polling_and_leaves_the_container() {
    // A project tab plus a focused squad tab that owns a live attach slot.
    let mut app = make_app(true);
    let mut squad = Tab::new_squad(make_session());
    squad
        .container_slots
        .push(ContainerSlot::new("leader".into(), "claude".into(), 1000));
    // The squad poller's cancellation token: closing the tab must trip it.
    let cancel = squad.squad.as_ref().unwrap().cancel.clone();
    app.tabs.push(squad);
    app.active_tab = app.tabs.len() - 1;
    let tabs_before = app.tabs.len();
    assert!(!cancel.is_cancelled());

    app.close_active_tab();

    // The tab (and its exec slot's local I/O) is dropped, but a second tab
    // remains so the app does not quit. `close_active_tab` issues no
    // `runtime.stop`, so the container — a separate process the daemon owns —
    // keeps running and is re-attachable; only the local exec session ends.
    assert!(
        !app.should_quit,
        "closing a non-lone tab must not quit the app"
    );
    assert_eq!(app.tabs.len(), tabs_before - 1, "the squad tab is removed");
    assert!(
        !app.tabs.iter().any(|t| t.is_squad),
        "no squad tab remains after close"
    );
    assert!(
        cancel.is_cancelled(),
        "closing the squad tab must cancel its task poller"
    );
}
