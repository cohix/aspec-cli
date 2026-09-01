//! State for the squad tab's task list and its background poller.
//!
//! `SquadTabState` is `Some` exactly when a `Tab` is the squad tab. It owns the
//! shared snapshot the poller publishes and the renderer reads, plus the
//! focus/cancel/attach handles that drive polling and the attach session. It
//! holds NO business logic: every value in `SquadSnapshot` comes from a
//! `TaskGateway` call, and every field here is UI/lifecycle state.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::data::fs::task_store::{Run, Task};

/// Snapshot the poller publishes; the renderer only ever reads this.
#[derive(Debug, Clone, Default)]
pub struct SquadSnapshot {
    /// All tasks, from the last successful `list()`.
    pub tasks: Vec<Task>,
    /// Runs for the currently selected task. Populated whenever a
    /// selection exists, so the detail modal opens with history already there.
    pub runs: Vec<Run>,
    /// `Some` when the last poll failed. Rendered verbatim as the
    /// "daemon not reachable" state — never replaced by an empty list.
    pub error: Option<String>,
    /// False until the first poll (success or failure) has completed.
    pub loaded: bool,
}

/// Cross-thread snapshot handle: the poll task writes, the renderer reads.
pub type SharedSquadSnapshot = Arc<Mutex<SquadSnapshot>>;

/// Cross-thread handle carrying the currently selected task name.
/// `App::tick_all_tabs` writes it from `SquadTabState::selected_name()`; the
/// poll task reads it, so the poller never touches UI state directly.
pub type SharedSelected = Arc<Mutex<Option<String>>>;

/// Squad sub-view state (selection, polled tasks, daemon reachability).
/// `Some` exactly when the owning `Tab` is the squad tab.
pub struct SquadTabState {
    /// Index into `snapshot.tasks`. Clamped on read/move, never by the
    /// poller. Lives on the `Tab`, so it survives a tab switch.
    ///
    /// Kept as a single linear index rather than a `(row, col)` pair on
    /// purpose (WI 0106 Part 5): a linear index identifies the same *task*
    /// across a terminal resize regardless of how many grid columns that
    /// resize produces, so a reflow can never silently reselect a different
    /// card. `grid_columns` below is the only place column count feeds back
    /// into navigation.
    pub selected: usize,
    /// Column count the renderer last laid the card grid out with. Written
    /// by `render_squad_body` every frame from the actual `area.width`;
    /// read by `move_selection`/`move_selection_col` to turn a linear index
    /// into 2D up/down/left/right movement. Defaults to `1` (never
    /// rendered), which degrades navigation to the old single-column list
    /// behavior.
    pub grid_columns: usize,
    /// Shared snapshot published by the poll task.
    pub snapshot: SharedSquadSnapshot,
    /// Set by `App::tick_all_tabs` each tick: `true` only while this tab is
    /// active. The poll loop skips its fetch while `false` and resumes when it
    /// flips back — it never exits.
    pub focused: Arc<AtomicBool>,
    /// Cancels the poll task (and any attach driver) on tab close.
    pub cancel: CancellationToken,
    /// Handle to the poll task, aborted on `Drop`.
    poll_handle: Option<tokio::task::JoinHandle<()>>,
    /// Set while an `squad attach` session owns this tab's slots and strip.
    pub attached_task: Option<String>,
    /// Written by the attach driver; drives the "daemon not reachable"
    /// indicator without tearing down live slots.
    pub daemon_reachable: Arc<AtomicBool>,
    /// Selected task name, published by `tick_all_tabs` and read by the
    /// poll task (see [`SharedSelected`]).
    pub poll_selected: SharedSelected,
}

impl SquadTabState {
    pub fn new() -> Self {
        Self {
            selected: 0,
            grid_columns: 1,
            snapshot: Arc::new(Mutex::new(SquadSnapshot::default())),
            focused: Arc::new(AtomicBool::new(false)),
            cancel: CancellationToken::new(),
            poll_handle: None,
            attached_task: None,
            daemon_reachable: Arc::new(AtomicBool::new(true)),
            poll_selected: Arc::new(Mutex::new(None)),
        }
    }

    /// Store the poll task handle so `Drop` can abort it.
    pub fn set_poll_handle(&mut self, handle: tokio::task::JoinHandle<()>) {
        self.poll_handle = Some(handle);
    }

    /// Name of the currently selected task, if the list is non-empty.
    pub fn selected_name(&self) -> Option<String> {
        self.snapshot
            .lock()
            .ok()
            .and_then(|snap| snap.tasks.get(self.selected).map(|c| c.name.clone()))
    }

    /// Clone of the currently selected task, if the list is non-empty.
    pub fn selected_task(&self) -> Option<Task> {
        self.snapshot
            .lock()
            .ok()
            .and_then(|snap| snap.tasks.get(self.selected).cloned())
    }

    /// Number of tasks in the last-published snapshot.
    fn task_count(&self) -> usize {
        self.snapshot
            .lock()
            .ok()
            .map(|snap| snap.tasks.len())
            .unwrap_or(0)
    }

    /// Move the selection by `delta_rows` grid rows (Up/Down), using
    /// `grid_columns` to map the linear `selected` index to a row/column and
    /// back. With `grid_columns == 1` this degrades exactly to the old
    /// linear-list Up/Down behavior.
    ///
    /// Moving past the first/last row clamps rather than wraps. Moving down
    /// onto a ragged last row that doesn't have a card in the current column
    /// clamps to the last task instead of skipping a row or going out of
    /// bounds.
    pub fn move_selection(&mut self, delta_rows: isize) {
        let len = self.task_count();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let columns = self.grid_columns.max(1);
        let rows_total = len.div_ceil(columns);
        let row = self.selected / columns;
        let col = self.selected % columns;
        let next_row =
            (row as isize + delta_rows).clamp(0, rows_total.saturating_sub(1) as isize) as usize;
        let candidate = next_row * columns + col;
        self.selected = candidate.min(len - 1);
    }

    /// Move the selection by `delta_cols` grid columns (Left/Right). Never
    /// crosses into an adjacent row — moving right off the end of a row (a
    /// ragged last row included) or left off its start just clamps in place.
    pub fn move_selection_col(&mut self, delta_cols: isize) {
        let len = self.task_count();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let columns = self.grid_columns.max(1);
        let row = self.selected / columns;
        let col = self.selected % columns;
        let row_width = (len - row * columns).min(columns);
        let next_col =
            (col as isize + delta_cols).clamp(0, row_width.saturating_sub(1) as isize) as usize;
        self.selected = row * columns + next_col;
    }
}

impl Default for SquadTabState {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SquadTabState {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(handle) = self.poll_handle.take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::path::PathBuf;

    use crate::data::fs::task_store::{MountScope, TaskStatus};

    fn task(name: &str) -> Task {
        let now = Utc::now();
        Task {
            id: name.to_string(),
            name: name.to_string(),
            description: "test task".to_string(),
            repo_scope: PathBuf::from("/workspace"),
            mount_scope: MountScope::Directory,
            overlays: Vec::new(),
            interval_secs: 60,
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

    fn state_with_tasks(len: usize, columns: usize) -> SquadTabState {
        let mut state = SquadTabState::new();
        state.grid_columns = columns;
        state.snapshot.lock().unwrap().tasks = (0..len)
            .map(|index| task(&format!("task-{index}")))
            .collect();
        state
    }

    #[test]
    fn grid_navigation_with_one_column_is_the_linear_list_behavior() {
        let mut state = state_with_tasks(4, 1);
        state.selected = 1;
        state.move_selection(1);
        assert_eq!(state.selected, 2);
        state.move_selection_col(1);
        assert_eq!(state.selected, 2, "right cannot leave a one-card row");
        state.move_selection(-9);
        assert_eq!(state.selected, 0, "up clamps at the first card");
    }

    #[test]
    fn grid_navigation_moves_by_rows_and_stays_within_two_column_rows() {
        let mut state = state_with_tasks(6, 2);
        state.selected = 1;
        state.move_selection(1);
        assert_eq!(state.selected, 3, "down preserves the column");
        state.move_selection_col(-1);
        assert_eq!(state.selected, 2, "left stays in the same row");
        state.move_selection_col(-1);
        assert_eq!(state.selected, 2, "left clamps at column zero");
        state.move_selection(1);
        assert_eq!(state.selected, 4);
    }

    #[test]
    fn grid_navigation_handles_a_ragged_last_row_without_wrapping() {
        let mut state = state_with_tasks(5, 2);
        state.selected = 3;
        state.move_selection(1);
        assert_eq!(
            state.selected, 4,
            "down onto a short final row picks its last card"
        );
        state.move_selection_col(1);
        assert_eq!(state.selected, 4, "right cannot wrap out of a ragged row");
        state.move_selection(-1);
        assert_eq!(
            state.selected, 2,
            "up returns to the matching available column"
        );
    }
}
