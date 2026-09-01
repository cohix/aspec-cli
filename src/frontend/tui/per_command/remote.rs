//! `RemoteCommandFrontend` impl for the TUI, plus `RemoteWorkflowPoller`
//! for live workflow strip updates from either remote service.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::command::commands::remote::RemoteCommandFrontend;
use crate::command::commands::remote_client::RemoteClient;
use crate::command::commands::squad::gateway::RemoteTaskGateway;
use crate::command::error::CommandError;
use crate::data::workflow_state::WorkflowState;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::tabs::SharedWorkflowViewState;
use crate::frontend::tui::workflow_view::workflow_state_to_view_state;

impl RemoteCommandFrontend for TuiCommandFrontend {}

/// How often a remote workflow snapshot is refreshed.
pub const REMOTE_WORKFLOW_POLL_INTERVAL: Duration = Duration::from_millis(500);

pub type WorkflowStateCallback = Arc<dyn Fn(&WorkflowState) + Send + Sync>;

/// Source of workflow snapshots for [`RemoteWorkflowPoller`].
///
/// The API server and squad daemon expose different routes, but both return the
/// same Layer-0 [`WorkflowState`], so the polling and rendering path is shared.
#[async_trait::async_trait]
pub trait WorkflowStateSource: Send + Sync {
    /// Fetch the current workflow snapshot. `Ok(None)` means that no workflow
    /// state exists right now, not that the source is unreachable.
    async fn fetch_workflow_state(&self) -> Result<Option<WorkflowState>, CommandError>;

    /// Whether the remote job reached a terminal status. Sources without a
    /// separate job-status route leave the default in place.
    async fn is_terminal(&self) -> bool {
        false
    }
}

/// Workflow source backed by the API server's per-command routes.
pub struct RemoteApiWorkflowSource {
    client: Arc<RemoteClient>,
    command_id: String,
}

impl RemoteApiWorkflowSource {
    pub fn new(client: Arc<RemoteClient>, command_id: impl Into<String>) -> Self {
        Self {
            client,
            command_id: command_id.into(),
        }
    }
}

#[async_trait::async_trait]
impl WorkflowStateSource for RemoteApiWorkflowSource {
    async fn fetch_workflow_state(&self) -> Result<Option<WorkflowState>, CommandError> {
        let Some(value) = self.client.get_workflow_state(&self.command_id).await? else {
            return Ok(None);
        };
        serde_json::from_value(value).map(Some).map_err(|error| {
            CommandError::RemoteTransport(format!("invalid remote workflow state: {error}"))
        })
    }

    async fn is_terminal(&self) -> bool {
        self.client
            .get_job(&self.command_id)
            .await
            .ok()
            .and_then(|response| response.body["status"].as_str().map(str::to_owned))
            .is_some_and(|status| status == "done" || status == "error")
    }
}

/// Workflow source backed by `GET /v1/tasks/{name}/workflow` on squad.
pub struct SquadTaskWorkflowSource {
    gateway: RemoteTaskGateway,
    task: String,
}

impl SquadTaskWorkflowSource {
    pub fn new(gateway: RemoteTaskGateway, task: impl Into<String>) -> Self {
        Self {
            gateway,
            task: task.into(),
        }
    }
}

#[async_trait::async_trait]
impl WorkflowStateSource for SquadTaskWorkflowSource {
    async fn fetch_workflow_state(&self) -> Result<Option<WorkflowState>, CommandError> {
        let response = match self
            .gateway
            .core()
            .get(&["tasks", &self.task, "workflow"])
            .await
        {
            Ok(response) => response,
            Err(CommandError::RemoteHttpStatus { status: 404, .. }) => return Ok(None),
            Err(error) => return Err(error),
        };
        serde_json::from_value(response.body)
            .map(Some)
            .map_err(|error| {
                CommandError::RemoteTransport(format!(
                    "invalid squad workflow state for {:?}: {error}",
                    self.task
                ))
            })
    }
}

/// Polls a [`WorkflowStateSource`] and feeds the existing shared workflow view
/// consumed by the TUI strip.
pub struct RemoteWorkflowPoller {
    source: Arc<dyn WorkflowStateSource>,
    workflow_view: SharedWorkflowViewState,
    reachable: Arc<AtomicBool>,
    on_state: Option<WorkflowStateCallback>,
}

impl RemoteWorkflowPoller {
    pub fn new(
        source: Arc<dyn WorkflowStateSource>,
        workflow_view: SharedWorkflowViewState,
    ) -> Self {
        Self {
            source,
            workflow_view,
            reachable: Arc::new(AtomicBool::new(true)),
            on_state: None,
        }
    }

    /// Publish source reachability into a caller-owned indicator.
    pub fn with_reachable(mut self, flag: Arc<AtomicBool>) -> Self {
        self.reachable = flag;
        self
    }

    /// Observe each successful workflow snapshot before it reaches the strip.
    pub fn with_on_state(mut self, callback: WorkflowStateCallback) -> Self {
        self.on_state = Some(callback);
        self
    }

    pub fn start(self, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.poll_loop(cancel).await;
        })
    }

    async fn poll_loop(&self, cancel: CancellationToken) {
        let mut saw_state = false;
        loop {
            let should_stop = tokio::select! {
                _ = cancel.cancelled() => break,
                result = self.poll_once(&mut saw_state) => result,
            };

            if should_stop {
                // Preserve the original poller's final refresh: terminal API
                // jobs get their last state, while squad's disappearing route
                // leaves the last terminal snapshot frozen in the strip.
                let _ = tokio::select! {
                    _ = cancel.cancelled() => None,
                    result = self.fetch_and_publish(&mut saw_state) => result,
                };
                break;
            }

            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(REMOTE_WORKFLOW_POLL_INTERVAL) => {}
            }
        }
    }

    /// Returns true when polling should stop after one final refresh.
    async fn poll_once(&self, saw_state: &mut bool) -> bool {
        let terminal = self.source.is_terminal().await;
        match self.fetch_and_publish(saw_state).await {
            // A failed state fetch always keeps polling, even if a separate
            // status route happened to report terminal in the same cycle.
            None => false,
            Some(disappeared_after_state) => terminal || disappeared_after_state,
        }
    }

    /// Fetch and publish one snapshot. `Some(true)` means a source that
    /// previously yielded state now reports no state; `None` means the fetch
    /// failed. Errors freeze the view and never request termination, so live
    /// direct-runtime slots survive a daemon outage.
    async fn fetch_and_publish(&self, saw_state: &mut bool) -> Option<bool> {
        match self.source.fetch_workflow_state().await {
            Err(_) => {
                self.reachable.store(false, Ordering::Relaxed);
                None
            }
            Ok(None) => {
                self.reachable.store(true, Ordering::Relaxed);
                Some(*saw_state)
            }
            Ok(Some(state)) => {
                self.reachable.store(true, Ordering::Relaxed);
                *saw_state = true;
                if let Some(callback) = &self.on_state {
                    callback(&state);
                }
                if let Ok(mut guard) = self.workflow_view.lock() {
                    *guard = Some(workflow_state_to_view_state(&state));
                }
                Some(false)
            }
        }
    }
}
