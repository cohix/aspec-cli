//! WI 0102 attach and workflow-driver regression tests.
//!
//! These deliberately build slots only by publishing `ContainerSlotEvent`s,
//! matching the normal workflow frontend.  No Docker engine is needed for the
//! UX/parity assertions.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use awman::command::dispatch::Engines;
use awman::command::dispatch::catalogue::CommandCatalogue;
use awman::command::error::CommandError;
use awman::data::EngineWorkflowStateStore;
use awman::data::fs::{ApiPaths, AuthPathResolver};
use awman::data::session::{Session, SessionOpenOptions, StaticGitRootResolver};
use awman::data::session_manager::SessionManager;
use awman::data::workflow_definition::WorkflowStep;
use awman::data::workflow_state::{StepState, WorkflowState};
use awman::engine::agent::AgentEngine;
use awman::engine::agent_runtime::AgentRuntimeEngine;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::frontend::tui::amie_attach::{AmieSlotDriver, SlotAction};
use awman::frontend::tui::app::App;
use awman::frontend::tui::tabs::{
    ContainerSlotEvent, SharedContainerSlotEvents, SharedWorkflowViewState, Tab, WorkflowViewState,
};
use awman::frontend::tui::user_message::SharedStatusLog;
use awman::frontend::tui::workflow_view::{render_workflow_strip, workflow_state_to_view_state};
use awman::frontend::tui::{RemoteWorkflowPoller, WorkflowStateSource};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

fn workflow_state(
    steps: &[(&str, &[&str], StepState, Option<&str>, Option<&str>)],
) -> WorkflowState {
    let definition: Vec<WorkflowStep> = steps
        .iter()
        .map(|(name, depends_on, _, agent, model)| WorkflowStep {
            name: (*name).to_string(),
            depends_on: depends_on.iter().map(|name| (*name).to_string()).collect(),
            prompt_template: String::new(),
            agent: agent.map(str::to_string),
            model: model.map(str::to_string),
            overlays: None,
            abort_on_failure: false,
        })
        .collect();
    let mut state = WorkflowState::new("attach-test".into(), &definition, "hash".into(), None);
    for (name, _, status, _, _) in steps {
        state.set_status(name, status.clone());
    }
    state
}

fn driver() -> AmieSlotDriver {
    let runtime: Arc<dyn AgentRuntimeEngine> = Arc::new(ContainerRuntime::docker());
    let events: SharedContainerSlotEvents = Arc::new(Mutex::new(VecDeque::new()));
    let log: SharedStatusLog = Arc::new(Mutex::new(Vec::new()));
    AmieSlotDriver::new(runtime, events, log)
}

#[test]
fn slot_driver_reconciles_running_transitions_without_duplicate_or_phantom_slots() {
    let mut driver = driver();
    let no_id = workflow_state(&[(
        "build",
        &[],
        StepState::Running { container_id: None },
        Some("claude"),
        Some("sonnet"),
    )]);
    assert!(
        driver.reconcile(&no_id).is_empty(),
        "an id-less running step waits"
    );

    let running = workflow_state(&[(
        "build",
        &[],
        StepState::Running {
            container_id: Some("abc123".into()),
        },
        Some("claude"),
        Some("sonnet"),
    )]);
    assert_eq!(
        driver.reconcile(&running),
        vec![SlotAction::Attach {
            step_name: "build".into(),
            container_id: "abc123".into(),
            agent: "claude".into(),
            model: Some("sonnet".into()),
        }],
        "the next poll retries the id-less transition with workflow metadata"
    );
    assert!(
        driver.reconcile(&running).is_empty(),
        "a slotted step is not re-launched"
    );

    let finished = workflow_state(&[("build", &[], StepState::Succeeded, None, None)]);
    assert_eq!(
        driver.reconcile(&finished),
        vec![SlotAction::Exit {
            step_name: "build".into()
        }],
        "leaving Running evicts the existing slot"
    );

    let vanished = workflow_state(&[]);
    assert!(
        driver.reconcile(&vanished).is_empty(),
        "a step that appears and vanishes between polls never acquires a slot"
    );
}

#[derive(Clone)]
struct ScriptedRoute {
    replies: Arc<Mutex<VecDeque<Result<Option<WorkflowState>, CommandError>>>>,
}

impl ScriptedRoute {
    fn new(replies: Vec<Result<Option<WorkflowState>, CommandError>>) -> Self {
        Self {
            replies: Arc::new(Mutex::new(replies.into())),
        }
    }
}

#[async_trait]
impl WorkflowStateSource for ScriptedRoute {
    async fn fetch_workflow_state(&self) -> Result<Option<WorkflowState>, CommandError> {
        self.replies.lock().unwrap().pop_front().unwrap_or(Ok(None))
    }
}

async fn wait_for_view(view: &SharedWorkflowViewState) -> WorkflowViewState {
    for _ in 0..50 {
        if let Some(value) = view.lock().unwrap().clone() {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("poller never published a workflow view")
}

async fn stop_poller(cancel: CancellationToken, task: tokio::task::JoinHandle<()>) {
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("poller cancels promptly")
        .expect("poller task does not panic");
}

fn view_fingerprint(
    view: &WorkflowViewState,
) -> Vec<(String, String, Vec<String>, Option<String>, Option<String>)> {
    view.steps
        .iter()
        .map(|step| {
            (
                step.name.clone(),
                step.status.clone(),
                step.depends_on.clone(),
                step.agent.clone(),
                step.model.clone(),
            )
        })
        .collect()
}

#[tokio::test]
async fn amie_and_api_workflow_routes_publish_the_same_view_state() {
    let state = workflow_state(&[
        (
            "done",
            &[],
            StepState::Succeeded,
            Some("claude"),
            Some("sonnet"),
        ),
        (
            "run",
            &["done"],
            StepState::Running {
                container_id: Some("run-1".into()),
            },
            Some("codex"),
            None,
        ),
    ]);
    let api_view: SharedWorkflowViewState = Arc::new(Mutex::new(None));
    let amie_view: SharedWorkflowViewState = Arc::new(Mutex::new(None));
    let api_cancel = CancellationToken::new();
    let amie_cancel = CancellationToken::new();
    let api_task = RemoteWorkflowPoller::new(
        Arc::new(ScriptedRoute::new(vec![Ok(Some(state.clone()))])),
        api_view.clone(),
    )
    .start(api_cancel.clone());
    let amie_task = RemoteWorkflowPoller::new(
        Arc::new(ScriptedRoute::new(vec![Ok(Some(state))])),
        amie_view.clone(),
    )
    .start(amie_cancel.clone());

    let api = wait_for_view(&api_view).await;
    let amie = wait_for_view(&amie_view).await;
    assert_eq!(view_fingerprint(&amie), view_fingerprint(&api));
    assert_eq!(amie.current_step, api.current_step);
    stop_poller(api_cancel, api_task).await;
    stop_poller(amie_cancel, amie_task).await;
}

fn session() -> Session {
    let temp = tempfile::tempdir().unwrap();
    let resolver = StaticGitRootResolver::new(temp.path());
    Session::open(
        temp.path().to_path_buf(),
        &resolver,
        SessionOpenOptions::default(),
    )
    .unwrap()
}

fn amie_app() -> App {
    let runtime_handle = Box::leak(Box::new(tokio::runtime::Runtime::new().unwrap()));
    let runtime = Arc::new(ContainerRuntime::docker());
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(
        AuthPathResolver::at_home(std::path::PathBuf::from("/tmp")),
    ));
    let engines = Engines {
        runtime: runtime.clone(),
        container_runtime: Some(runtime.clone()),
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::new()),
        overlay_engine: overlay.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(
            AuthPathResolver::at_home("/tmp"),
            ApiPaths::at_root("/tmp"),
        )),
        agent_engine: Arc::new(AgentEngine::new(overlay, runtime)),
        workflow_state_store: Arc::new(EngineWorkflowStateStore::at_git_root(
            tempfile::tempdir().unwrap().path(),
        )),
    };
    App::new(
        CommandCatalogue::get(),
        engines,
        Arc::new(RwLock::new(SessionManager::in_memory())),
        Tab::new_amie(session()),
        runtime_handle.handle().clone(),
    )
}

fn push_launch(tab: &Tab, step_name: &str, agent: &str, container_name: &str) {
    let mut events = tab.container_slot_events.lock().unwrap();
    events.push_back(ContainerSlotEvent::Launched {
        step_name: step_name.into(),
        agent: agent.into(),
        model: None,
        io: None,
    });
    events.push_back(ContainerSlotEvent::ContainerName {
        step_name: step_name.into(),
        container_name: container_name.into(),
    });
}

fn slot_fingerprint(tab: &Tab) -> Vec<(String, String, String)> {
    tab.container_slots
        .iter()
        .map(|slot| {
            (
                slot.step_name.clone(),
                slot.agent_name().to_string(),
                slot.container_info.as_ref().unwrap().container_name.clone(),
            )
        })
        .collect()
}

#[test]
fn workflow_phase_attach_matches_the_in_process_workflow_slot_and_strip_state() {
    let state = workflow_state(&[
        ("done", &[], StepState::Succeeded, Some("claude"), None),
        (
            "lint",
            &["done"],
            StepState::Running {
                container_id: Some("lint-id".into()),
            },
            Some("claude"),
            Some("sonnet"),
        ),
        (
            "test",
            &["done"],
            StepState::Running {
                container_id: Some("test-id".into()),
            },
            Some("codex"),
            Some("o3"),
        ),
    ]);

    // This is the ordinary `exec workflow` event protocol.  The assertion is
    // deliberately against this in-process path, not hand-written slot state.
    let mut in_process = Tab::new(session());
    push_launch(&in_process, "lint", "claude", "awman-lint-id");
    push_launch(&in_process, "test", "codex", "awman-test-id");
    in_process.drain_container_slot_events();
    *in_process.workflow_state.lock().unwrap() = Some(workflow_state_to_view_state(&state));

    // The attach driver produces actions from the remote snapshot; its normal
    // downstream representation is the exact same event queue.
    let mut attached = Tab::new_amie(session());
    let mut driver = driver();
    for action in driver.reconcile(&state) {
        let SlotAction::Attach {
            step_name,
            agent,
            container_id,
            ..
        } = action
        else {
            unreachable!("first observation only launches")
        };
        push_launch(
            &attached,
            &step_name,
            &agent,
            &format!("awman-{container_id}"),
        );
    }
    attached.drain_container_slot_events();
    *attached.workflow_state.lock().unwrap() = Some(workflow_state_to_view_state(&state));

    assert_eq!(slot_fingerprint(&attached), slot_fingerprint(&in_process));
    assert_eq!(attached.focused_slot_idx, in_process.focused_slot_idx);
    let attached_view = attached.workflow_state.lock().unwrap().clone().unwrap();
    let in_process_view = in_process.workflow_state.lock().unwrap().clone().unwrap();
    assert_eq!(
        view_fingerprint(&attached_view),
        view_fingerprint(&in_process_view)
    );
    assert_eq!(attached_view.current_step, in_process_view.current_step);
}

#[test]
fn externally_populated_slots_cycle_like_normal_slots_and_single_slot_keeps_pty_path() {
    let mut tab = Tab::new_amie(session());
    push_launch(&tab, "one", "claude", "awman-one");
    push_launch(&tab, "two", "codex", "awman-two");
    tab.drain_container_slot_events();
    tab.cycle_focused_slot();
    assert_eq!(
        tab.focused_slot_idx, 1,
        "Ctrl-S's shared slot operation rotates external slots"
    );

    let mut one_slot = Tab::new_amie(session());
    push_launch(&one_slot, "only", "claude", "awman-only");
    one_slot.drain_container_slot_events();
    one_slot.cycle_focused_slot();
    assert_eq!(
        one_slot.focused_slot_idx, 0,
        "one slot is left for the normal PTY Ctrl-S path"
    );

    let source = include_str!("../src/frontend/tui/key_handler.rs");
    assert!(
        source.contains("app.active_tab().has_multiple_slots()")
            && source.contains("falls through untouched"),
        "the real Ctrl-S handler must reserve only multi-slot Ctrl-S and leave one-slot PTY input alone"
    );
}

fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
    let area = *buffer.area();
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer.cell((x, y)).unwrap().symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn poller_driven_strip_uses_existing_grouping_and_completed_collapse() {
    let state = workflow_state(&[
        ("first", &[], StepState::Succeeded, None, None),
        ("second", &[], StepState::Succeeded, None, None),
        ("third", &[], StepState::Succeeded, None, None),
        (
            "after-all",
            &["first", "second", "third"],
            StepState::Running {
                container_id: Some("after-id".into()),
            },
            None,
            None,
        ),
    ]);
    let view: SharedWorkflowViewState = Arc::new(Mutex::new(None));
    let cancel = CancellationToken::new();
    let task = RemoteWorkflowPoller::new(
        Arc::new(ScriptedRoute::new(vec![Ok(Some(state))])),
        view.clone(),
    )
    .start(cancel.clone());
    let published = wait_for_view(&view).await;

    let backend = TestBackend::new(70, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| render_workflow_strip(&published, Rect::new(0, 0, 70, 8), frame, 0))
        .unwrap();
    let text = buffer_text(terminal.backend().buffer());
    assert!(
        text.contains("(+2 completed)"),
        "completed siblings collapse: {text}"
    );
    assert!(
        text.contains("after-all") && text.contains('→'),
        "depends_on groups form columns: {text}"
    );
    stop_poller(cancel, task).await;
}

#[tokio::test]
async fn daemon_failure_freezes_poller_strip_and_preserves_live_slots() {
    let initial = workflow_state(&[(
        "live",
        &[],
        StepState::Running {
            container_id: Some("live-id".into()),
        },
        Some("claude"),
        None,
    )]);
    let mut app = amie_app();
    let view = app.active_tab().workflow_state.clone();
    let reachable = app
        .active_tab()
        .amie
        .as_ref()
        .unwrap()
        .daemon_reachable
        .clone();
    let callback_count = Arc::new(AtomicBool::new(false));
    let callback_seen = callback_count.clone();
    let cancel = CancellationToken::new();
    let task = RemoteWorkflowPoller::new(
        Arc::new(ScriptedRoute::new(vec![
            Ok(Some(initial.clone())),
            Err(CommandError::RemoteTransport("daemon disappeared".into())),
        ])),
        view.clone(),
    )
    .with_reachable(reachable.clone())
    .with_on_state(Arc::new(move |_| {
        callback_seen.store(true, Ordering::Relaxed)
    }))
    .start(cancel.clone());
    let frozen = wait_for_view(&view).await;

    push_launch(app.active_tab(), "live", "claude", "awman-live");
    app.active_tab_mut().drain_container_slot_events();
    app.active_tab_mut()
        .amie
        .as_mut()
        .unwrap()
        .attached_condition = Some("condition-a".into());
    tokio::time::sleep(Duration::from_millis(650)).await;

    assert!(callback_count.load(Ordering::Relaxed));
    assert!(
        !reachable.load(Ordering::Relaxed),
        "failed poll marks the daemon unreachable"
    );
    assert_eq!(
        view_fingerprint(&view.lock().unwrap().clone().unwrap()),
        view_fingerprint(&frozen)
    );
    assert_eq!(
        slot_fingerprint(app.active_tab()),
        vec![("live".into(), "claude".into(), "awman-live".into())]
    );
    app.tick_all_tabs();
    assert_eq!(
        app.status_bar.text,
        "amie daemon not reachable — strip frozen; attached containers still live",
        "the frozen-strip indicator appears without tearing down direct-runtime slots"
    );
    stop_poller(cancel, task).await;
}

#[test]
fn attaching_mid_workflow_keeps_completed_steps_in_the_existing_strip() {
    let state = workflow_state(&[
        ("already-done", &[], StepState::Succeeded, None, None),
        (
            "currently-running",
            &["already-done"],
            StepState::Running {
                container_id: Some("current-id".into()),
            },
            None,
            None,
        ),
    ]);
    let from_start = workflow_state_to_view_state(&state);
    let mid_workflow_attach = workflow_state_to_view_state(&state);
    assert_eq!(
        view_fingerprint(&mid_workflow_attach),
        view_fingerprint(&from_start)
    );
    assert!(
        mid_workflow_attach
            .steps
            .iter()
            .any(|step| step.name == "already-done" && step.status == "done"),
        "the first snapshot after a mid-workflow attach retains completed history"
    );
}

#[test]
fn tui_attach_uses_existing_container_proxy_without_a_new_frontend_impl() {
    let source = include_str!("../src/frontend/tui/amie_attach.rs");
    assert!(source.contains("TuiContainerProxy::with_io"));
    assert!(source.contains("run_with_frontend(Box::new(proxy))"));
    assert!(
        !source.contains("impl AgentFrontend for"),
        "attach must reuse TuiContainerProxy rather than define another AgentFrontend"
    );
}

#[test]
fn cli_attach_uses_the_existing_cli_frontend_without_a_new_frontend_impl() {
    // Finding #9: pin the CLI attach path to the existing `CliFrontend` and the
    // no-new-`AgentFrontend` guarantee, matching the TUI scan above.
    let source = include_str!("../src/frontend/cli/per_command/amie_attach.rs");
    assert!(
        source.contains("CliFrontend::new(matches.clone())"),
        "CLI attach must run through the existing CliFrontend"
    );
    assert!(
        !source.contains("impl AgentFrontend for"),
        "CLI attach must not define another AgentFrontend"
    );
}

#[test]
fn neither_attach_frontend_names_a_concrete_runtime_backend() {
    // Finding #8: the CLI and TUI attach paths go through the abstract
    // `AgentRuntimeEngine`, so Docker and Apple Containers behave identically.
    // This mirrors the sanctioned mechanical scan: no `"docker"` /
    // `"apple-containers"` literal may appear in an attach frontend. (A
    // `runtime_name()` call to format the shared sandbox-refusal text is
    // permitted — it changes no attach behaviour.)
    for source in [
        include_str!("../src/frontend/tui/amie_attach.rs"),
        include_str!("../src/frontend/cli/per_command/amie_attach.rs"),
        include_str!("../src/frontend/attach.rs"),
    ] {
        assert!(
            !source.contains("\"apple-containers\""),
            "attach must not name the Apple Containers backend"
        );
        assert!(
            !source.contains("\"docker\""),
            "attach must not name the Docker backend"
        );
    }
}
