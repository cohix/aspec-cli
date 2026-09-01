//! TUI `squad attach` orchestration.
//!
//! Workflow snapshots feed the existing workflow-view mutex and this module
//! translates their running-step transitions into the existing container-slot
//! event queue. Rendering, slot layout, PTY parsing, and key handling remain
//! the ordinary workflow paths.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::command::commands::squad::daemon::SquadSupervisor;
use crate::command::commands::squad::runtime_guard::SQUAD_SANDBOX_REFUSAL;
use crate::data::config::env::Env;
use crate::data::message::MessageLevel;
use crate::data::session::AgentHandle;
use crate::data::workflow_state::{StepState, WorkflowState};
use crate::engine::agent_runtime::frontend::AgentIo;
use crate::engine::agent_runtime::AgentRuntimeEngine;
use crate::frontend::attach::{list_task_containers, no_run_in_progress};
use crate::frontend::tui::app::App;
use crate::frontend::tui::per_command::TuiContainerProxy;
use crate::frontend::tui::tabs::{
    ContainerSlotEvent, ContainerSlotIo, ExecutionPhase, SharedContainerSlotEvents,
};
use crate::frontend::tui::user_message::{SharedStatusLog, StatusLogEntry};
use crate::frontend::tui::{RemoteWorkflowPoller, SquadTaskWorkflowSource, WorkflowStateSource};

/// What one reconciliation of a polled `WorkflowState` against the currently
/// slotted steps implies. Pure data — no engine calls, no I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotAction {
    Attach {
        step_name: String,
        container_id: String,
        agent: String,
        model: Option<String>,
    },
    Exit {
        step_name: String,
    },
}

/// Drives `Tab::container_slot_events` from polled workflow snapshots.
///
/// The same poller writes `Tab::workflow_state`; this driver is only the
/// state-to-slot half of the attach session.
pub struct SquadSlotDriver {
    runtime: Arc<dyn AgentRuntimeEngine>,
    slot_events: SharedContainerSlotEvents,
    status_log: SharedStatusLog,
    /// Steps currently slotted, keyed by step name → container id.
    slotted: HashMap<String, String>,
    /// Cancels only local attach exec clients when the owning tab closes.
    session_cancel: CancellationToken,
}

impl SquadSlotDriver {
    pub fn new(
        runtime: Arc<dyn AgentRuntimeEngine>,
        slot_events: SharedContainerSlotEvents,
        status_log: SharedStatusLog,
    ) -> Self {
        Self {
            runtime,
            slot_events,
            status_log,
            slotted: HashMap::new(),
            session_cancel: CancellationToken::new(),
        }
    }

    fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.session_cancel = cancel;
        self
    }

    /// Diff the authoritative polled state against the currently slotted step
    /// names. This method performs no runtime calls and no I/O.
    pub fn reconcile(&mut self, state: &WorkflowState) -> Vec<SlotAction> {
        let mut actions = Vec::new();

        // Evict every slot whose step is absent or no longer Running. Sorting
        // makes the pure result stable even though both inputs are hash maps.
        let mut slotted_names: Vec<String> = self.slotted.keys().cloned().collect();
        slotted_names.sort();
        for step_name in slotted_names {
            let still_running = matches!(
                state.step_states.get(&step_name),
                Some(StepState::Running { .. })
            );
            if !still_running {
                self.slotted.remove(&step_name);
                actions.push(SlotAction::Exit { step_name });
            }
        }

        // A Running transition is actionable only after the daemon records an
        // id. Unslotted id-less steps remain absent so the next poll retries.
        let mut running: Vec<_> = state.step_states.iter().collect();
        running.sort_by(|(left, _), (right, _)| left.cmp(right));
        for (step_name, step_state) in running {
            let StepState::Running {
                container_id: Some(container_id),
            } = step_state
            else {
                continue;
            };
            if self.slotted.contains_key(step_name) {
                continue;
            }

            let step_info = state.steps.iter().find(|step| step.name == *step_name);
            let agent = step_info
                .and_then(|step| step.agent.clone())
                .unwrap_or_else(|| "agent".to_string());
            let model = step_info.and_then(|step| step.model.clone());
            self.slotted.insert(step_name.clone(), container_id.clone());
            actions.push(SlotAction::Attach {
                step_name: step_name.clone(),
                container_id: container_id.clone(),
                agent,
                model,
            });
        }

        actions
    }

    /// Resolve and apply one reconciliation action through the existing
    /// runtime attach and container-slot event paths.
    pub fn apply(&mut self, action: SlotAction) {
        match action {
            SlotAction::Exit { step_name } => {
                push_slot_event(&self.slot_events, ContainerSlotEvent::Exited { step_name });
            }
            SlotAction::Attach {
                step_name,
                container_id,
                agent,
                model,
            } => {
                let Some(handle) = handle_for_container_id(self.runtime.as_ref(), &container_id)
                else {
                    // The daemon can publish the id just before the runtime's
                    // list sees it. Remove the optimistic marker and retry on
                    // the next workflow snapshot without treating it as an
                    // attach failure.
                    self.slotted.remove(&step_name);
                    return;
                };
                if let Err(error) =
                    self.attach_handle(step_name.clone(), agent, model, handle, false)
                {
                    self.slotted.remove(&step_name);
                    self.log_error(format!(
                        "failed to attach workflow step {step_name:?}: {error}"
                    ));
                }
            }
        }
    }

    /// Attach a known evaluation handle into the same one-slot machinery.
    fn attach_evaluation(
        &mut self,
        handle: AgentHandle,
        agent: String,
        model: Option<String>,
    ) -> bool {
        match self.attach_handle("evaluation".to_string(), agent, model, handle, true) {
            Ok(()) => true,
            Err(error) => {
                self.log_error(format!("failed to attach task evaluation: {error}"));
                false
            }
        }
    }

    /// The one I/O construction used by both evaluation and workflow attach.
    /// It is the same channel shape as
    /// `TuiCommandFrontend::recreate_parallel_container_io`.
    fn attach_handle(
        &self,
        step_name: String,
        agent: String,
        model: Option<String>,
        handle: AgentHandle,
        exit_slot_when_session_ends: bool,
    ) -> Result<(), String> {
        let instance = self
            .runtime
            .attach(&handle)
            .map_err(|error| error.to_string())?;

        let (stdout_tx, stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let stdin_tx_for_engine = stdin_tx.clone();
        let (resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();
        let initial_size = crossterm::terminal::size()
            .map(|(cols, rows)| {
                crate::frontend::tui::event_loop::compute_container_inner_size(cols, rows)
            })
            .unwrap_or((80, 24));

        let agent_io = AgentIo {
            stdout: stdout_tx.clone(),
            stderr: stdout_tx,
            stdin_tx: stdin_tx_for_engine,
            stdin_rx,
            resize: Some(resize_rx),
            initial_size: Some(initial_size),
        };
        let container_name = Arc::new(Mutex::new(None));
        let proxy = TuiContainerProxy::with_io(self.status_log.clone(), agent_io, container_name);
        let mut execution = instance
            .run_with_frontend(Box::new(proxy))
            .map_err(|error| error.to_string())?;

        push_slot_event(
            &self.slot_events,
            ContainerSlotEvent::Launched {
                step_name: step_name.clone(),
                agent,
                model,
                io: Some(ContainerSlotIo {
                    stdout_rx,
                    stdin_tx,
                    resize_tx,
                }),
            },
        );
        push_slot_event(
            &self.slot_events,
            ContainerSlotEvent::ContainerName {
                step_name: step_name.clone(),
                container_name: handle.name,
            },
        );

        // Keep the AgentExecution alive so its local exec process and bridge
        // live for the whole attach session. Cancellation kills only that local
        // exec client; attach bridge configuration never stops the target.
        let cancel = self.session_cancel.clone();
        let cancel_handle = execution.cancel_handle();
        let slot_events = self.slot_events.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => {
                    if let Some(handle) = cancel_handle {
                        let _ = handle.cancel();
                    }
                }
                _ = execution.wait() => {
                    if exit_slot_when_session_ends {
                        push_slot_event(
                            &slot_events,
                            ContainerSlotEvent::Exited { step_name },
                        );
                    }
                }
            }
        });

        Ok(())
    }

    fn log_error(&self, text: String) {
        if let Ok(mut log) = self.status_log.lock() {
            log.push(StatusLogEntry {
                level: MessageLevel::Error,
                text,
            });
        }
    }
}

/// Resolve a daemon-reported id against currently running runtime handles.
pub fn handle_for_container_id(
    runtime: &dyn AgentRuntimeEngine,
    container_id: &str,
) -> Option<AgentHandle> {
    if container_id.is_empty() {
        return None;
    }
    runtime
        .list_running_all()
        .ok()?
        .into_iter()
        .find(|handle| handle.id.starts_with(container_id) || container_id.starts_with(&handle.id))
}

/// Start an `squad attach <name>` session in the active squad tab.
pub fn start_squad_attach(app: &mut App, task: &str) {
    if app.engines.container_runtime.is_none() {
        app.status_bar.text =
            SQUAD_SANDBOX_REFUSAL.replace("{runtime}", app.engines.runtime.runtime_name());
        return;
    }
    if !app.active_tab().is_squad {
        app.status_bar.text = "squad attach is only available from the squad tab".to_string();
        return;
    }

    let supervisor = match SquadSupervisor::from_env(&Env::from_process()) {
        Ok(supervisor) => supervisor,
        Err(error) => {
            app.status_bar.text = error.to_string();
            return;
        }
    };
    let gateway = match supervisor.gateway_from_meta() {
        Ok(Some(gateway)) => gateway,
        Ok(None) => {
            app.status_bar.text = "squad daemon not reachable".to_string();
            return;
        }
        Err(error) => {
            app.status_bar.text = error.to_string();
            return;
        }
    };

    let runtime_handle = app.runtime_handle.clone();
    let task_name = task.to_string();
    let source: Arc<dyn WorkflowStateSource> =
        Arc::new(SquadTaskWorkflowSource::new(gateway, task_name.clone()));

    // This is a one-shot phase discriminator, not a second poller. It runs on
    // the existing runtime because this entry point is synchronous, following
    // the same spawn-and-result-channel pattern used to open the squad tab.
    let (probe_tx, probe_rx) = std::sync::mpsc::channel();
    let probe_source = source.clone();
    runtime_handle.spawn(async move {
        let _ = probe_tx.send(probe_source.fetch_workflow_state().await);
    });
    let initial_workflow = match probe_rx.recv() {
        Ok(result) => result,
        Err(_) => {
            app.status_bar.text = "squad workflow probe was interrupted".to_string();
            return;
        }
    };
    let workflow_phase = matches!(&initial_workflow, Ok(Some(_)));
    let evaluation_phase = matches!(&initial_workflow, Ok(None));
    let initially_reachable = initial_workflow.is_ok();

    // Runtime prefix discovery is authoritative and also provides the
    // evaluation-phase handle. Workflow slot identity itself always comes
    // from WorkflowState rather than from parsing names.
    let candidates = match list_task_containers(app.engines.runtime.as_ref(), task) {
        Ok(candidates) => candidates,
        Err(error) => {
            app.status_bar.text = error.to_string();
            return;
        }
    };
    // A known workflow may legitimately be between its Running transition
    // and container creation. In that case the poller remains alive and the
    // id-less step is retried. Without workflow state, an empty authoritative
    // runtime set means the task is idle and fails immediately.
    if candidates.is_empty() && !workflow_phase {
        app.status_bar.text = no_run_in_progress(task).to_string();
        return;
    }
    let evaluation_candidate = if evaluation_phase && candidates.len() == 1 {
        candidates.into_iter().next()
    } else {
        None
    };
    let runtime = app.engines.runtime.clone();

    let tab = app.active_tab_mut();
    let squad_state = tab
        .squad
        .as_mut()
        .expect("is_squad tabs always carry squad state");

    // A fresh attach session owns these existing sinks. There is no command
    // dialog channel and no alternate rendering path.
    if let Ok(mut view) = tab.workflow_state.lock() {
        *view = None;
    }
    if let Ok(mut events) = tab.container_slot_events.lock() {
        events.clear();
    }
    tab.dialog_request_rx = None;
    tab.dialog_response_tx = None;
    tab.execution_phase = ExecutionPhase::Running {
        command: format!("squad attach {task}"),
    };
    tab.suppress_container_auto_open = false;
    squad_state.attached_task = Some(task.to_string());
    squad_state
        .daemon_reachable
        .store(initially_reachable, Ordering::Relaxed);

    let workflow_view = tab.workflow_state.clone();
    let slot_events = tab.container_slot_events.clone();
    let status_log = tab.status_log.clone();
    let reachable = squad_state.daemon_reachable.clone();
    let cancel = squad_state.cancel.clone();
    let evaluation_metadata = squad_state
        .snapshot
        .lock()
        .ok()
        .and_then(|snapshot| {
            snapshot
                .tasks
                .iter()
                .find(|candidate| candidate.name == task_name)
                .map(|task| {
                    (
                        task.agent.clone().unwrap_or_else(|| "agent".to_string()),
                        task.model.clone(),
                    )
                })
        })
        .unwrap_or_else(|| ("agent".to_string(), None));

    let driver = Arc::new(Mutex::new(
        SquadSlotDriver::new(runtime, slot_events.clone(), status_log).with_cancel(cancel.clone()),
    ));
    let evaluation_attached = Arc::new(AtomicBool::new(false));

    let callback_driver = driver.clone();
    let callback_events = slot_events;
    let callback_evaluation_attached = evaluation_attached.clone();
    let on_state: Arc<dyn Fn(&WorkflowState) + Send + Sync> = Arc::new(move |state| {
        // The evaluation phase has no strip. As soon as a workflow snapshot
        // exists, remove that temporary slot before applying authoritative
        // workflow-step transitions.
        if callback_evaluation_attached.swap(false, Ordering::Relaxed) {
            push_slot_event(
                &callback_events,
                ContainerSlotEvent::Exited {
                    step_name: "evaluation".to_string(),
                },
            );
        }
        if let Ok(mut driver) = callback_driver.lock() {
            let actions = driver.reconcile(state);
            for action in actions {
                driver.apply(action);
            }
        }
    });

    let poller = RemoteWorkflowPoller::new(source.clone(), workflow_view)
        .with_reachable(reachable)
        .with_on_state(on_state);
    let _runtime_guard = runtime_handle.enter();
    if let Some(candidate) = evaluation_candidate {
        let (agent, model) = evaluation_metadata;
        if driver
            .lock()
            .ok()
            .is_some_and(|mut driver| driver.attach_evaluation(candidate.handle, agent, model))
        {
            evaluation_attached.store(true, Ordering::Relaxed);
        }
    }
    let poll_task = poller.start(cancel);
    squad_state.set_poll_handle(poll_task);
}

fn push_slot_event(events: &SharedContainerSlotEvents, event: ContainerSlotEvent) {
    if let Ok(mut queue) = events.lock() {
        queue.push_back(event);
    }
}
