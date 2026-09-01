//! Background git-diff polling lifecycle for a tab: (re)starting the poll
//! task when the effective working directory changes, and draining
//! stuck/unstuck events for tab coloring.

use super::*;

impl Tab {
    /// (Re)start the git diff poll task against `root`, cancelling any existing
    /// task first. No-ops (leaving the summary untouched) when called outside a
    /// tokio runtime, e.g. in synchronous unit tests.
    pub(super) fn start_git_poll(&mut self, root: std::path::PathBuf) {
        // Cancel and drop the previous task, if any.
        if let Some(cancel) = self.git_poll_cancel.take() {
            cancel.cancel();
        }
        if let Some(handle) = self.git_poll_handle.take() {
            handle.abort();
        }

        // `tokio::spawn` panics without a runtime; skip gracefully so
        // non-async tests can construct tabs.
        if tokio::runtime::Handle::try_current().is_err() {
            self.git_poll_root = Some(root);
            return;
        }

        let cancel = tokio_util::sync::CancellationToken::new();
        let handle =
            start_git_diff_poll_task(root.clone(), self.git_diff_summary.clone(), cancel.clone());
        self.git_poll_cancel = Some(cancel);
        self.git_poll_handle = Some(handle);
        self.git_poll_root = Some(root);
    }

    /// Restart the poll task if the tab's effective working directory changed.
    /// The effective root is the active worktree path when set, otherwise the
    /// session git root. Called each tick from `tick_all_tabs`.
    pub fn refresh_git_poll(&mut self) {
        // The squad tab's synthetic session has no meaningful git diff and
        // `new_squad` starts no poll; returning early keeps `tick_all_tabs`
        // from restarting one against the squad storage root.
        if self.is_squad {
            return;
        }
        let desired = self
            .active_worktree_path
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| self.session.git_root().to_path_buf());
        if self.git_poll_root.as_ref() != Some(&desired) {
            self.start_git_poll(desired);
        }
    }

    /// Drain pending stuck events from the broadcast channel and update
    /// the `stuck` flag for tab coloring.
    pub fn drain_stuck_events(&mut self) {
        // Pick up a new stuck sender from the engine if available.
        if let Ok(mut guard) = self.stuck_sender_shared.lock() {
            if let Some(sender) = guard.take() {
                self.stuck_rx = Some(sender.subscribe());
            }
        }
        if let Some(ref mut rx) = self.stuck_rx {
            while let Ok(event) = rx.try_recv() {
                match event {
                    StuckEvent::Stuck => self.stuck = true,
                    StuckEvent::Unstuck => self.stuck = false,
                    // Bridge already killed the container; clear the stuck
                    // flag because the step is failing rather than blocked.
                    StuckEvent::StartupGraceExpired => self.stuck = false,
                }
            }
        }

        // WI-0096 §9: while a parallel group is active (the sequential
        // backbone is stashed in `dormant_slots`), aggregate the stuck /
        // yolo indicators across the group's slots for tab coloring.
        // Outside a group the broadcast channel above and the spawn-time
        // yolo flag drive the indicators, exactly as for plain commands.
        if !self.dormant_slots.is_empty() {
            self.stuck = self.container_slots.iter().any(|s| s.stuck);
            self.yolo_mode = self.container_slots.iter().any(|s| s.yolo_mode);
        }
    }
}
