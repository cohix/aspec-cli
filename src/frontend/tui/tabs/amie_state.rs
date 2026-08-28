//! State for the amie tab's condition list and its background poller.
//!
//! `AmieTabState` is `Some` exactly when a `Tab` is the amie tab. It owns the
//! shared snapshot the poller publishes and the renderer reads, plus the
//! focus/cancel/attach handles that drive polling and the attach session. It
//! holds NO business logic: every value in `AmieSnapshot` comes from a
//! `ConditionGateway` call, and every field here is UI/lifecycle state.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::data::fs::condition_store::{Condition, Run};

/// Snapshot the poller publishes; the renderer only ever reads this.
#[derive(Debug, Clone, Default)]
pub struct AmieSnapshot {
    /// All conditions, from the last successful `list()`.
    pub conditions: Vec<Condition>,
    /// Runs for the currently selected condition. Populated whenever a
    /// selection exists, so the detail modal opens with history already there.
    pub runs: Vec<Run>,
    /// `Some` when the last poll failed. Rendered verbatim as the
    /// "daemon not reachable" state — never replaced by an empty list.
    pub error: Option<String>,
    /// False until the first poll (success or failure) has completed.
    pub loaded: bool,
}

/// Cross-thread snapshot handle: the poll task writes, the renderer reads.
pub type SharedAmieSnapshot = Arc<Mutex<AmieSnapshot>>;

/// Cross-thread handle carrying the currently selected condition name.
/// `App::tick_all_tabs` writes it from `AmieTabState::selected_name()`; the
/// poll task reads it, so the poller never touches UI state directly.
pub type SharedSelected = Arc<Mutex<Option<String>>>;

/// Amie sub-view state (selection, polled conditions, daemon reachability).
/// `Some` exactly when the owning `Tab` is the amie tab.
pub struct AmieTabState {
    /// Index into `snapshot.conditions`. Clamped on read/move, never by the
    /// poller. Lives on the `Tab`, so it survives a tab switch.
    pub selected: usize,
    /// Shared snapshot published by the poll task.
    pub snapshot: SharedAmieSnapshot,
    /// Set by `App::tick_all_tabs` each tick: `true` only while this tab is
    /// active. The poll loop skips its fetch while `false` and resumes when it
    /// flips back — it never exits.
    pub focused: Arc<AtomicBool>,
    /// Cancels the poll task (and any attach driver) on tab close.
    pub cancel: CancellationToken,
    /// Handle to the poll task, aborted on `Drop`.
    poll_handle: Option<tokio::task::JoinHandle<()>>,
    /// Set while an `amie attach` session owns this tab's slots and strip.
    pub attached_condition: Option<String>,
    /// Written by the attach driver; drives the "daemon not reachable"
    /// indicator without tearing down live slots.
    pub daemon_reachable: Arc<AtomicBool>,
    /// Selected condition name, published by `tick_all_tabs` and read by the
    /// poll task (see [`SharedSelected`]).
    pub poll_selected: SharedSelected,
}

impl AmieTabState {
    pub fn new() -> Self {
        Self {
            selected: 0,
            snapshot: Arc::new(Mutex::new(AmieSnapshot::default())),
            focused: Arc::new(AtomicBool::new(false)),
            cancel: CancellationToken::new(),
            poll_handle: None,
            attached_condition: None,
            daemon_reachable: Arc::new(AtomicBool::new(true)),
            poll_selected: Arc::new(Mutex::new(None)),
        }
    }

    /// Store the poll task handle so `Drop` can abort it.
    pub fn set_poll_handle(&mut self, handle: tokio::task::JoinHandle<()>) {
        self.poll_handle = Some(handle);
    }

    /// Name of the currently selected condition, if the list is non-empty.
    pub fn selected_name(&self) -> Option<String> {
        self.snapshot
            .lock()
            .ok()
            .and_then(|snap| snap.conditions.get(self.selected).map(|c| c.name.clone()))
    }

    /// Clone of the currently selected condition, if the list is non-empty.
    pub fn selected_condition(&self) -> Option<Condition> {
        self.snapshot
            .lock()
            .ok()
            .and_then(|snap| snap.conditions.get(self.selected).cloned())
    }

    /// Move the selection by `delta`, saturating and clamped to the current
    /// list length.
    pub fn move_selection(&mut self, delta: isize) {
        let len = self
            .snapshot
            .lock()
            .ok()
            .map(|snap| snap.conditions.len())
            .unwrap_or(0);
        if len == 0 {
            self.selected = 0;
            return;
        }
        let max = (len - 1) as isize;
        let next = (self.selected as isize).saturating_add(delta).clamp(0, max);
        self.selected = next as usize;
    }
}

impl Default for AmieTabState {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AmieTabState {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(handle) = self.poll_handle.take() {
            handle.abort();
        }
    }
}
