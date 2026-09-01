//! Background poller for the squad tab's task list and selected-task
//! detail.
//!
//! One task per squad tab. It holds no policy: every value it writes into the
//! shared snapshot came verbatim from a `TaskGateway` call. The task ticks
//! on a fixed interval and is a no-op while the tab is unfocused; it exits only
//! when its `CancellationToken` fires (on tab close).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::command::commands::squad::gateway::{TaskGateway, DEFAULT_RUN_HISTORY_LIMIT};
use crate::frontend::tui::tabs::squad_state::{SharedSelected, SharedSquadSnapshot, SquadTabState};

/// How often the squad tab polls the gateway while focused.
pub const SQUAD_POLL_INTERVAL: Duration = Duration::from_millis(2000);

/// Polls `list` (always) and `get` + `runs` (whenever a row is selected) into
/// the tab's shared snapshot.
pub struct SquadTaskPoller {
    gateway: Arc<dyn TaskGateway>,
    snapshot: SharedSquadSnapshot,
    focused: Arc<AtomicBool>,
    selected: SharedSelected,
}

impl SquadTaskPoller {
    pub fn new(gateway: Arc<dyn TaskGateway>, state: &SquadTabState) -> Self {
        Self {
            gateway,
            snapshot: state.snapshot.clone(),
            focused: state.focused.clone(),
            selected: state.poll_selected.clone(),
        }
    }

    /// Ticks every [`SQUAD_POLL_INTERVAL`]; each tick is a no-op while `focused`
    /// is false. Exits only when `cancel` fires.
    pub fn start(self, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(SQUAD_POLL_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = ticker.tick() => {
                        if self.focused.load(Ordering::Relaxed) {
                            self.poll_once().await;
                        }
                    }
                }
            }
        })
    }

    async fn poll_once(&self) {
        // 1. Always refresh the list. On error, set `error` and leave the
        //    last-known tasks untouched so the tab never blanks to what
        //    would read as "no tasks".
        match self.gateway.list().await {
            Ok(tasks) => {
                if let Ok(mut snap) = self.snapshot.lock() {
                    snap.tasks = tasks;
                    snap.error = None;
                    snap.loaded = true;
                }
            }
            Err(error) => {
                if let Ok(mut snap) = self.snapshot.lock() {
                    snap.error = Some(error.to_string());
                    snap.loaded = true;
                }
                return;
            }
        }

        // 2. If a row is selected, refresh its detail + run history. A
        //    not-found error for the selected name sets `error` and clears the
        //    stale runs.
        let selected = self.selected.lock().ok().and_then(|guard| guard.clone());
        let Some(name) = selected else {
            return;
        };
        if let Err(error) = self.gateway.get(&name).await {
            if let Ok(mut snap) = self.snapshot.lock() {
                snap.error = Some(error.to_string());
                snap.runs.clear();
            }
            return;
        }
        match self.gateway.runs(&name, DEFAULT_RUN_HISTORY_LIMIT).await {
            Ok(runs) => {
                if let Ok(mut snap) = self.snapshot.lock() {
                    snap.runs = runs;
                }
            }
            Err(error) => {
                if let Ok(mut snap) = self.snapshot.lock() {
                    snap.error = Some(error.to_string());
                    snap.runs.clear();
                }
            }
        }
    }
}
