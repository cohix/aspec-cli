//! Per-tab state.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ratatui::layout::Rect;

use crate::command::dispatch::CommandOutcome;
use crate::command::error::CommandError;
use crate::data::session::Session;
use crate::engine::acp::{PermissionRequest, SessionUpdate};
use crate::engine::agent_runtime::execution::{AgentStats, StuckEvent};
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};
use crate::frontend::tui::git_sidebar::{
    start_git_diff_poll_task, GitSidebarState, SharedGitDiffSummary,
};
use crate::frontend::tui::user_message::SharedStatusLog;

pub mod amie_state;
mod container_slots;
mod git_poll;
mod labels;
mod overlay_lifecycle;
#[cfg(test)]
mod tests;

use amie_state::AmieTabState;

/// Per-tab execution lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionPhase {
    Idle,
    Running { command: String },
    Done { command: String, exit_code: i32 },
    Error { command: String, message: String },
}

/// Container overlay window state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerWindowState {
    Hidden,
    Minimized,
    Maximized,
}

impl ContainerWindowState {
    pub fn cycle(self) -> Self {
        match self {
            Self::Hidden => Self::Maximized,
            Self::Minimized => Self::Maximized,
            Self::Maximized => Self::Minimized,
        }
    }
}

/// Workflow state strip display mode.
///
/// The strip defaults to `Collapsed`: exactly one 3-row box per topological
/// stage, so it never costs the execution window more than 3 rows. A stage
/// with a single step draws that step's normal box; a parallel stage draws a
/// `N steps…` summary box in the stage's aggregate status colour.
///
/// `Expanded` draws every step of every stage as its own box — full name,
/// agent/model label, status colour, nothing rolled up. It grows to whatever
/// the frame can spare between the tab bar and the command box, and it wins
/// that space ahead of the execution window and the container status bars.
///
/// Toggled with `Ctrl-O` ("overview").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkflowStripState {
    #[default]
    Collapsed,
    Expanded,
}

impl WorkflowStripState {
    pub fn toggle(self) -> Self {
        match self {
            Self::Collapsed => Self::Expanded,
            Self::Expanded => Self::Collapsed,
        }
    }

    pub fn is_expanded(self) -> bool {
        matches!(self, Self::Expanded)
    }
}

/// Current workflow view state (visible when a workflow is running).
#[derive(Debug, Clone, Default)]
pub struct WorkflowViewState {
    pub steps: Vec<WorkflowStepView>,
    pub current_step: Option<String>,
    /// Effective `maxConcurrentAgents` for the running workflow (WI-0096 §11),
    /// set by the frontend when the engine reports a parallel group start.
    /// `None` means unlimited — the strip caps parallel rows at the legacy 3
    /// and renders no "queued" markers, so behavior is unchanged.
    pub max_concurrent: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct WorkflowStepView {
    pub name: String,
    pub status: String,
    /// Resolved agent (e.g. `"claude"`) — fed by `report_workflow_progress`.
    pub agent: Option<String>,
    /// Optional resolved model.
    pub model: Option<String>,
    /// Steps this one waits on. Drives the column-grouping in the strip
    /// renderer (steps with the same sorted `depends_on` set sit in the
    /// same topological column).
    pub depends_on: Vec<String>,
}

/// Cross-thread shared workflow view state.
///
/// `WorkflowFrontend` (engine-driven, in a tokio task) writes to it; the TUI
/// renderer reads from it. Mirrors the pattern used by `SharedStatusLog`.
pub type SharedWorkflowViewState = Arc<Mutex<Option<WorkflowViewState>>>;

/// Snapshot of the status dashboard for TUI table rendering.
#[derive(Debug, Clone)]
pub struct StatusDashboardData {
    pub containers: Vec<crate::command::commands::status::StatusContainerRow>,
    pub tip: String,
}

/// Cross-thread shared status dashboard data. The status command writes here;
/// the TUI renderer reads it to display a proper `Table` widget.
pub type SharedStatusDashboard = Arc<Mutex<Option<StatusDashboardData>>>;

/// Cross-thread shared yolo-countdown state. The engine ticks it every 100ms
/// while a yolo countdown is active; the renderer reads it to display the
/// "Auto-advancing in Ns" non-modal overlay.
pub type SharedYoloState = Arc<Mutex<Option<YoloState>>>;

/// Shared flag: TUI event loop sets this to `true` when the user presses
/// Esc during a yolo countdown. `yolo_countdown_tick` checks it and
/// returns `Cancel` when set, then resets the flag.
pub type SharedYoloCancelFlag = Arc<AtomicBool>;

/// Shared flag set by the workflow frontend to signal the TUI event loop
/// to reset the vt100 parser before the next step's PTY output arrives.
pub type SharedPtyResetFlag = Arc<AtomicBool>;

/// Shared container name. Set by the container frontend when the engine
/// reports `AgentStatus::Running { container_name }`. The TUI event
/// loop reads this to populate `ContainerInfo.container_name` for stats
/// polling.
pub type SharedContainerName = Arc<Mutex<Option<String>>>;

/// Shared container exit code. Set by the workflow frontend when the engine
/// reports `report_container_exited` — the step's container has actually
/// terminated (killed by awman or the agent process exited). The TUI event
/// loop takes it and closes the container window, leaving the summary bar.
pub type SharedContainerExitCode = Arc<Mutex<Option<i32>>>;

/// Shared active-worktree path. Set by the worktree-lifecycle frontend on
/// `report_worktree_created` and cleared on the post-workflow report
/// (kept/discarded). The renderer reads this so the bottom-bar context
/// line can show "Using worktree: <path>" while a workflow runs in a
/// worktree even though the tab's session is rooted at the main repo.
pub type SharedActiveWorktreePath = Arc<Mutex<Option<std::path::PathBuf>>>;

/// Shared stdin sender slot. When a workflow step transition creates fresh
/// stdin channels, the new sender is published here so the TUI event loop
/// can swap `tab.container_stdin_tx` to the new one.
pub type SharedStdinTx = Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>>;

/// Shared resize sender slot, same pattern as `SharedStdinTx`.
pub type SharedResizeTx = Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<(u16, u16)>>>>;

/// Shared engine sender. The engine creates the channel and publishes
/// the sender via `set_engine_sender`; the TUI event loop reads it
/// to send Ctrl-W requests.
pub type SharedEngineTx =
    Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<crate::engine::workflow::EngineRequest>>>>;

/// Shared stuck sender. The engine publishes the container's stuck
/// broadcast sender via `set_stuck_sender`; the TUI event loop subscribes
/// from it for tab-coloring (stuck indicator).
pub type SharedStuckSender = Arc<Mutex<Option<Arc<tokio::sync::broadcast::Sender<StuckEvent>>>>>;

/// Shared TUI context for the status command. The event loop refreshes this
/// on every tick so the status watch loop always sees live tab data.
pub type SharedTuiContext = Arc<Mutex<crate::command::commands::status::StatusCommandTuiContext>>;

#[derive(Debug, Clone)]
pub struct YoloState {
    pub step_name: String,
    pub remaining_secs: u64,
}

/// Mouse text selection.
///
/// Coordinates are stored in window cell space (0-based against the window
/// the selection started in — the maximized container's vt100 grid or the
/// execution window's inner area), not raw terminal coords. The renderer
/// publishes `Tab::container_inner_area` / `Tab::exec_inner_area` so
/// `handle_mouse_event` can subtract the window's screen offset before
/// recording these.
#[derive(Debug, Clone)]
pub struct TextSelection {
    pub start_col: u16,
    pub start_row: u16,
    pub end_col: u16,
    pub end_row: u16,
    /// Snapshot of the window's text grid at selection-start time. Each cell
    /// is the printable contents of that position (or `" "` for empties), so
    /// the copied text reflects what the user *saw* when they started the
    /// drag, not the window's current values (which mutate with live output).
    pub snapshot: Vec<Vec<String>>,
}

/// Live container metadata, populated while a containerized command runs.
#[derive(Debug, Clone)]
pub struct ContainerInfo {
    pub agent_display_name: String,
    pub container_name: String,
    pub start_time: Instant,
    pub latest_stats: Option<AgentStats>,
    /// History of `(cpu_percent, memory_mb)` samples for averaging in the
    /// post-exit summary bar.
    pub stats_history: Vec<(f64, f64)>,
    /// Whether the active runtime is sandbox-class (e.g.
    /// `docker-sbx-experimental`) rather than container-class. Drives the
    /// overlay title — "(sandboxed)" vs "(containerized)".
    pub sandboxed: bool,
}

/// Summary captured after a containerized command exits, displayed in a
/// dashed-border bar below the execution window until the next command starts.
#[derive(Debug, Clone)]
pub struct LastContainerSummary {
    pub agent_display_name: String,
    pub container_name: String,
    pub avg_cpu: String,
    pub avg_memory: String,
    pub total_time: String,
    pub exit_code: i32,
}

/// Upper bound on the rendered ACP update history kept per slot. Once full,
/// the oldest entry is dropped as new ones arrive — a plain ring buffer.
/// There is no raw terminal scrollback to fall back on for an ACP window, so
/// this cap is the only backstop against unbounded growth.
pub const ACP_HISTORY_LIMIT: usize = 2000;

/// Rendered state for an ACP (Agent Client Protocol) agent window.
///
/// Unlike a stdio slot there is no raw terminal byte stream and no vt100
/// grid: the agent speaks structured [`SessionUpdate`] frames, which are kept
/// here as a bounded history and drawn as a scrollable list. A pending
/// permission request is parked here while its modal is open.
///
/// The state is shared (`Arc<Mutex<…>>`, see [`SharedAcpState`]) between the
/// TUI render thread, which reads the history to draw the window, and the ACP
/// frontend running on the engine task, which appends updates in
/// `render_update`. This mirrors the other engine→TUI shared handles
/// (`SharedStatusLog`, `SharedYoloState`): the render loop repaints every
/// tick, so an append is picked up on the next frame with no explicit redraw
/// signal.
#[derive(Debug, Default)]
pub struct AcpSlotState {
    /// Rendered update history, oldest first. Bounded to [`ACP_HISTORY_LIMIT`]
    /// entries (a ring buffer).
    pub history: std::collections::VecDeque<SessionUpdate>,
    /// The permission request currently awaiting a user decision, if any. Set
    /// when the modal opens and cleared when it resolves.
    pub pending_permission: Option<PermissionRequest>,
    /// Lines-from-bottom scroll offset into the rendered history. `0` follows
    /// the latest updates; increasing it scrolls toward older output. This is
    /// the ACP window's own simple list scroll — it never touches the
    /// vt100 scrollback path stdio slots use.
    pub scroll_offset: usize,
}

impl AcpSlotState {
    /// Append an update, enforcing the [`ACP_HISTORY_LIMIT`] ring-buffer cap.
    pub fn push_update(&mut self, update: SessionUpdate) {
        self.history.push_back(update);
        while self.history.len() > ACP_HISTORY_LIMIT {
            self.history.pop_front();
        }
    }
}

/// Cross-thread handle to an ACP window's [`AcpSlotState`]. Held by both the
/// [`ContainerSlot`] (TUI thread) and the ACP frontend (engine task).
pub type SharedAcpState = Arc<Mutex<AcpSlotState>>;

/// Which kind of agent window a [`ContainerSlot`] hosts.
///
/// A **stdio** slot bridges a real PTY/piped byte stream into the vt100 grid,
/// using the `vt100_parser`, `region_scroll`, terminal-mode flags, and I/O
/// channels on [`ContainerSlot`]. An **ACP** slot has no raw terminal stream:
/// its structured updates live in the shared [`AcpSlotState`] instead, and the
/// vt100 fields on the slot stay inert.
///
/// Keeping both behind one `ContainerSlot` type is what lets a parallel
/// workflow group mix stdio and ACP windows in a single `container_slots`
/// vec, so `focused_slot()` and the minimized-bar iteration stay uniform over
/// one slot type.
#[derive(Debug, Clone)]
pub enum AgentWindowKind {
    Stdio,
    Acp(SharedAcpState),
}

/// One running container. This is THE container representation — a plain
/// containerized command (`chat`, `exec prompt`) is simply a tab with one
/// slot, and a parallel workflow group is a tab with N of them (WI-0096).
///
/// Each slot owns its own PTY parser, terminal-mode flags, stats, and I/O
/// channels. The slot at `Tab::focused_slot_idx` renders maximized; the
/// others render as stacked minimized status bars. A slot's [`AgentWindowKind`]
/// selects whether it is driven by that stdio/PTY machinery or by a structured
/// ACP update stream (in which case the vt100 fields stay inert).
pub struct ContainerSlot {
    /// Whether this slot is a stdio (PTY/vt100) window or an ACP window.
    /// Stdio is the default; ACP slots carry their shared render state here.
    pub kind: AgentWindowKind,
    /// Workflow step this container runs, or empty for non-workflow
    /// commands and sequential workflow steps (whose step name comes from
    /// the workflow view state instead).
    pub step_name: String,
    pub vt100_parser: vt100::Parser,
    pub region_scroll: crate::frontend::tui::region_scroll::RegionScrollEmulator,
    pub container_info: Option<ContainerInfo>,
    pub container_stdout_rx: Option<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>,
    pub container_stdin_tx: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    pub container_resize_tx: Option<tokio::sync::mpsc::UnboundedSender<(u16, u16)>>,
    /// Whether the agent has requested the alternate screen buffer. Tracked
    /// here (not via the vt100 parser) because `drain_container_output`
    /// strips alternate-screen sequences before the parser sees them.
    pub agent_alt_screen: bool,
    /// Whether the agent has enabled "alternate scroll" mode (DECSET 1007),
    /// tracked from the raw PTY output for the same reason.
    pub agent_alternate_scroll: bool,
    pub stuck: bool,
    pub yolo_mode: bool,
    pub yolo_state: SharedYoloState,
    pub yolo_cancel_flag: SharedYoloCancelFlag,
    pub stuck_rx: Option<tokio::sync::broadcast::Receiver<StuckEvent>>,
}

pub struct ContainerSlotIo {
    pub stdout_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    pub stdin_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    pub resize_tx: tokio::sync::mpsc::UnboundedSender<(u16, u16)>,
}

impl ContainerSlot {
    /// Create a fresh slot for a newly-launched container. The PTY parser
    /// starts at 80x24 and is sized to the real overlay dimensions as soon
    /// as they are known; live I/O channels are attached by the caller.
    pub fn new(step_name: String, agent_display_name: String, scrollback: usize) -> Self {
        Self {
            kind: AgentWindowKind::Stdio,
            step_name,
            vt100_parser: vt100::Parser::new(24, 80, scrollback),
            region_scroll: crate::frontend::tui::region_scroll::RegionScrollEmulator::new(),
            container_info: Some(ContainerInfo {
                agent_display_name,
                container_name: String::new(),
                start_time: Instant::now(),
                latest_stats: None,
                stats_history: Vec::new(),
                sandboxed: false,
            }),
            container_stdout_rx: None,
            container_stdin_tx: None,
            container_resize_tx: None,
            agent_alt_screen: false,
            agent_alternate_scroll: false,
            stuck: false,
            yolo_mode: false,
            yolo_state: Arc::new(Mutex::new(None)),
            yolo_cancel_flag: Arc::new(AtomicBool::new(false)),
            stuck_rx: None,
        }
    }

    /// Create a fresh ACP window slot. The vt100 fields exist but stay inert
    /// (no PTY stream feeds them); the window renders from `state` instead.
    /// `state` is the same handle the ACP frontend appends updates to, so the
    /// caller shares one `Arc` between the slot and the frontend.
    pub fn new_acp(
        step_name: String,
        agent_display_name: String,
        state: SharedAcpState,
    ) -> Self {
        let mut slot = Self::new(step_name, agent_display_name, 0);
        slot.kind = AgentWindowKind::Acp(state);
        slot
    }

    /// Whether this slot hosts an ACP window (rather than a stdio/PTY one).
    pub fn is_acp(&self) -> bool {
        matches!(self.kind, AgentWindowKind::Acp(_))
    }

    /// The shared ACP render state, when this is an ACP slot.
    pub fn acp_state(&self) -> Option<&SharedAcpState> {
        match &self.kind {
            AgentWindowKind::Acp(state) => Some(state),
            AgentWindowKind::Stdio => None,
        }
    }

    /// Agent display name for the minimized bar, falling back to "agent".
    pub fn agent_name(&self) -> &str {
        self.container_info
            .as_ref()
            .map(|i| i.agent_display_name.as_str())
            .unwrap_or("agent")
    }

    /// Elapsed run time for the minimized bar.
    pub fn elapsed_secs(&self) -> u64 {
        self.container_info
            .as_ref()
            .map(|i| i.start_time.elapsed().as_secs())
            .unwrap_or(0)
    }
}

/// Lifecycle event published by the workflow frontend (engine thread) and
/// drained by the TUI event loop to maintain `Tab::container_slots`
/// (WI-0096 §12). Kept in a shared queue rather than mutating `Tab` directly
/// because the frontend runs on the engine's tokio task while `Tab` lives on
/// the TUI thread.
pub enum ContainerSlotEvent {
    /// A parallel group is starting: the sequential "backbone" slot (the
    /// command-level container plumbing that sequential steps reuse) goes
    /// dormant while the group's per-step slots take over the display.
    GroupStarted,
    /// A container in the group started running (initial launch or dequeued).
    Launched {
        step_name: String,
        agent: String,
        model: Option<String>,
        io: Option<ContainerSlotIo>,
    },
    /// The engine learned the step's actual container name (published right
    /// after launch). Drives per-slot stats polling and the stats title.
    ContainerName {
        step_name: String,
        container_name: String,
    },
    /// A container exited — evict its slot with no grey summary bar.
    Exited { step_name: String },
    /// A container's stuck timer fired (yolo off).
    Stuck { step_name: String },
    /// A stuck container recovered.
    Unstuck { step_name: String },
    /// A container's yolo countdown started. `cancel_flag` is the same
    /// `Arc` the engine-side frontend checks each tick — stashed on the slot
    /// so the TUI event loop can request cancellation (Esc on the per-slot
    /// countdown modal) without a lookup back into the engine thread.
    YoloStarted {
        step_name: String,
        cancel_flag: SharedYoloCancelFlag,
    },
    /// A per-second countdown update for a slot's yolo timer, mirroring the
    /// sequential path's `yolo_countdown_tick`. Drives both the minimized-bar
    /// countdown text and the per-slot modal shown when the slot is focused.
    YoloTick {
        step_name: String,
        remaining_secs: u64,
    },
    /// A container's yolo countdown ended (cancelled, expired, or advanced).
    YoloFinished { step_name: String },
    /// The whole group drained; clear any remaining group slots and restore
    /// the dormant sequential backbone.
    GroupFinished,
}

/// Shared queue of [`ContainerSlotEvent`]s. Mirrors the other `SharedXxx`
/// slots: the workflow frontend pushes, the event loop drains.
pub type SharedContainerSlotEvents = Arc<Mutex<std::collections::VecDeque<ContainerSlotEvent>>>;

/// Tab state — one per open tab.
pub struct Tab {
    pub session: Session,
    pub execution_phase: ExecutionPhase,
    pub container_window_state: ContainerWindowState,
    /// How many lines from the bottom to skip in the focused slot's vt100
    /// scrollback when the container is Maximized. 0 = follow live output.
    pub container_scroll_offset: usize,
    /// Summary of the last container session, shown in a dashed-border bar
    /// below the exec window after the container exits.
    pub last_container_summary: Option<LastContainerSummary>,
    /// Inner content rect of the container overlay, refreshed each frame by
    /// the renderer. Used by the mouse handler to translate raw terminal
    /// coords into vt100 cell coords.
    pub container_inner_area: Option<Rect>,
    /// Whether the container overlay has been drawn at least once this
    /// session. Set by the renderer when it draws the maximized overlay;
    /// cleared at command start. When the agent exits before any frame was
    /// drawn (fast-failing launch), `close_container_overlay` dumps the
    /// captured terminal contents to the status log instead of silently
    /// discarding them.
    pub container_rendered: bool,
    /// Inner content rect of the execution window, refreshed each frame by
    /// the renderer while the container overlay is not Maximized. Used by
    /// the mouse handler to translate raw terminal coords into execution
    /// window cell coords when starting a text selection there.
    pub exec_inner_area: Option<Rect>,
    /// Visible text grid of the execution window, refreshed each frame by
    /// the renderer while the container overlay is not Maximized. Cloned
    /// into `TextSelection::snapshot` at selection-start so the copied text
    /// reflects what the user saw (the window content shifts as new
    /// status-log lines arrive).
    pub exec_window_grid: Vec<Vec<String>>,
    /// Shared workflow view state. The engine's `WorkflowFrontend` impl
    /// writes here on `report_workflow_progress` / `report_step_status`;
    /// the renderer reads from here when drawing the workflow strip.
    pub workflow_state: SharedWorkflowViewState,
    /// Shared yolo countdown state. Updated by `yolo_countdown_tick` on the
    /// engine side; rendered as a non-modal overlay (avoids the dialog-spam
    /// that a per-tick `ask_dialog` would cause).
    pub yolo_state: SharedYoloState,
    /// Shared cancel flag for yolo countdown. TUI event loop sets this on
    /// Esc; `yolo_countdown_tick` reads + clears it.
    pub yolo_cancel_flag: SharedYoloCancelFlag,
    pub status_log: SharedStatusLog,
    pub status_log_collapsed: bool,
    pub status_dashboard: SharedStatusDashboard,
    pub scroll_offset: usize,
    pub workflow_strip_scroll_offset: usize,
    /// Whether the workflow strip shows one box per stage (the default) or
    /// every parallel step of every stage. Toggled with `Ctrl-O`.
    pub workflow_strip_state: WorkflowStripState,
    pub last_strip_rect: Option<Rect>,
    pub mouse_selection: Option<TextSelection>,
    pub workflow_agent_fallbacks: HashMap<String, String>,
    pub is_remote: bool,
    /// Fixed tab kind, following the `is_remote` precedent. An amie tab is not
    /// bound to a project directory and renders amie content in place of the
    /// execution window. Never toggled after construction.
    pub is_amie: bool,
    /// Amie sub-view state (selection, polled conditions, daemon reachability).
    /// `Some` exactly when `is_amie`.
    pub amie: Option<AmieTabState>,
    pub output_lines: Vec<String>,
    pub stuck: bool,
    pub yolo_mode: bool,
    /// Broadcast receiver for stuck/unstuck events from the container engine.
    /// Drained non-blockingly in `tick_all_tabs` for tab coloring.
    pub stuck_rx: Option<tokio::sync::broadcast::Receiver<StuckEvent>>,

    // ── Container slots ──────────────────────────────────────────────────
    /// The tab's running containers. A plain containerized command is one
    /// slot; a parallel workflow group is N of them. Empty while nothing
    /// containerized is running. The slot at `focused_slot_idx` renders
    /// maximized; the others render as stacked minimized status bars.
    pub container_slots: Vec<ContainerSlot>,
    /// Index into `container_slots` of the Maximized (focused) slot. Cycled
    /// by Ctrl-S. Always `0` with a single slot.
    pub focused_slot_idx: usize,
    /// The sequential "backbone" slot(s), stashed while a parallel workflow
    /// group runs. Sequential steps reuse the command-level stdout channel,
    /// so the slot holding its receiver must stay alive across the group
    /// and is restored when the group finishes. Non-empty exactly while a
    /// parallel group is active.
    pub dormant_slots: Vec<ContainerSlot>,
    /// Shared queue of slot lifecycle events published by the workflow
    /// frontend; drained each tick to maintain `container_slots`.
    pub container_slot_events: SharedContainerSlotEvents,
    /// Set after a mid-workflow container exit closes the window: PTY bytes
    /// that were still in flight from the dead container must not re-open it
    /// via `drain_container_output`'s auto-open branch. Cleared when the next
    /// container launches (new command, step transition, or a fresh
    /// `Running { container_name }` report).
    pub suppress_container_auto_open: bool,

    // ── Async command plumbing ───────────────────────────────────────────
    /// Receives the command outcome once the spawned task finishes.
    pub command_result_rx: Option<std::sync::mpsc::Receiver<Result<CommandOutcome, CommandError>>>,
    /// Event loop polls for dialog requests from the command thread.
    pub dialog_request_rx: Option<std::sync::mpsc::Receiver<DialogRequest>>,
    /// Event loop sends dialog responses back to the command thread.
    pub dialog_response_tx: Option<std::sync::mpsc::Sender<DialogResponse>>,
    /// Shared flag: workflow frontend sets this to signal the TUI to reset the
    /// vt100 parser between workflow steps.
    pub pty_reset_flag: SharedPtyResetFlag,
    /// Shared container name: set by the container frontend when the engine
    /// reports the running container's name.
    pub container_name_shared: SharedContainerName,
    /// Shared container exit code: set by the workflow frontend when the
    /// engine reports a mid-workflow container has actually terminated.
    pub container_exit_shared: SharedContainerExitCode,
    /// Shared stdin sender slot for workflow step transitions.
    pub stdin_tx_shared: SharedStdinTx,
    /// Shared resize sender slot for workflow step transitions.
    pub resize_tx_shared: SharedResizeTx,
    /// Shared control board sender for mid-step WCB requests.
    pub engine_tx_shared: SharedEngineTx,
    /// Shared stuck sender from the container engine. The event loop
    /// subscribes from it when a new sender appears.
    pub stuck_sender_shared: SharedStuckSender,
    /// Shared active worktree path: set by the worktree-lifecycle frontend
    /// after a worktree is created/resumed, cleared after the workflow
    /// finalize step. Drives the "Using worktree: <path>" bottom-bar line.
    pub active_worktree_path: SharedActiveWorktreePath,
    /// Live TUI context for the status command. The event loop refreshes this
    /// on every tick; `TuiCommandFrontend` reads from it on each watch
    /// iteration so the status table always reflects current tab state.
    pub tui_context_shared: SharedTuiContext,

    // ── Git sidebar ──────────────────────────────────────────────────────
    /// Whether the git sidebar is open. Toggled by Ctrl-G.
    pub git_sidebar_state: GitSidebarState,
    /// Shared diff summary written by the background poll task and read by the
    /// renderer for the sidebar and the status-bar `+X -Y` summary.
    pub git_diff_summary: SharedGitDiffSummary,
    /// Handle to the background poll task, aborted on `Drop`.
    git_poll_handle: Option<tokio::task::JoinHandle<()>>,
    /// Cancellation token for the current poll task; triggered before a
    /// restart (worktree change) and on `Drop`.
    git_poll_cancel: Option<tokio_util::sync::CancellationToken>,
    /// The directory the poll task is currently watching. Compared against the
    /// desired root each tick so the task restarts when the worktree changes.
    git_poll_root: Option<std::path::PathBuf>,
}

impl Drop for Tab {
    fn drop(&mut self) {
        // Stop the background git poll task so it doesn't outlive the tab.
        if let Some(cancel) = self.git_poll_cancel.take() {
            cancel.cancel();
        }
        if let Some(handle) = self.git_poll_handle.take() {
            handle.abort();
        }
    }
}

impl Tab {
    pub fn new(session: Session) -> Self {
        let git_root = session.git_root().to_path_buf();
        let mut tab = Self::new_inner(session);
        // Start polling against the session git root. Once a worktree is
        // created, `refresh_git_poll` (called each tick) restarts the task
        // pointed at the worktree path.
        tab.start_git_poll(git_root);
        tab
    }

    /// Construct the singleton amie tab. Unlike [`Tab::new`] this starts **no**
    /// git poll: the synthetic session is rooted at the amie storage root,
    /// which has no meaningful diff. The caller must not auto-spawn a startup
    /// command into this tab.
    pub fn new_amie(session: Session) -> Self {
        let mut tab = Self::new_inner(session);
        tab.is_amie = true;
        tab.amie = Some(AmieTabState::new());
        tab
    }

    /// Shared field initialisation for [`Tab::new`] and [`Tab::new_amie`].
    /// Starts **no** poll and spawns nothing; the caller decides whether a git
    /// poll runs (normal tab) or not (amie tab).
    fn new_inner(session: Session) -> Self {
        Self {
            session,
            execution_phase: ExecutionPhase::Idle,
            container_window_state: ContainerWindowState::Hidden,
            container_scroll_offset: 0,
            last_container_summary: None,
            container_inner_area: None,
            container_rendered: false,
            exec_inner_area: None,
            exec_window_grid: Vec::new(),
            workflow_state: Arc::new(Mutex::new(None)),
            yolo_state: Arc::new(Mutex::new(None)),
            yolo_cancel_flag: Arc::new(AtomicBool::new(false)),
            status_log: Arc::new(Mutex::new(Vec::new())),
            status_log_collapsed: false,
            status_dashboard: Arc::new(Mutex::new(None)),
            scroll_offset: 0,
            workflow_strip_scroll_offset: 0,
            workflow_strip_state: WorkflowStripState::Collapsed,
            last_strip_rect: None,
            mouse_selection: None,
            workflow_agent_fallbacks: HashMap::new(),
            is_remote: false,
            is_amie: false,
            amie: None,
            output_lines: Vec::new(),
            stuck: false,
            yolo_mode: false,
            stuck_rx: None,
            container_slots: Vec::new(),
            focused_slot_idx: 0,
            dormant_slots: Vec::new(),
            container_slot_events: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            suppress_container_auto_open: false,
            command_result_rx: None,
            dialog_request_rx: None,
            dialog_response_tx: None,
            pty_reset_flag: Arc::new(AtomicBool::new(false)),
            container_name_shared: Arc::new(Mutex::new(None)),
            container_exit_shared: Arc::new(Mutex::new(None)),
            stdin_tx_shared: Arc::new(Mutex::new(None)),
            resize_tx_shared: Arc::new(Mutex::new(None)),
            engine_tx_shared: Arc::new(Mutex::new(None)),
            stuck_sender_shared: Arc::new(Mutex::new(None)),
            active_worktree_path: Arc::new(Mutex::new(None)),
            tui_context_shared: Arc::new(Mutex::new(
                crate::command::commands::status::StatusCommandTuiContext::default(),
            )),
            git_sidebar_state: GitSidebarState::Closed,
            git_diff_summary: Arc::new(Mutex::new(None)),
            git_poll_handle: None,
            git_poll_cancel: None,
            git_poll_root: None,
        }
    }
}

/// Truncate a string to at most `max` characters; if longer, replace the
/// trailing characters with `…`.
fn truncate_with_ellipsis(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let trunc: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{}\u{2026}", trunc)
    } else {
        s.to_string()
    }
}

/// Format an elapsed-seconds count as a short human duration:
/// `"42s"` < 60s, `"7m"` < 1h, `"2h 15m"` otherwise.
pub fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{}h {}m", h, m)
    }
}

/// Tab color based on execution state.
pub fn tab_color(tab: &Tab) -> ratatui::style::Color {
    use ratatui::style::Color;
    // Yolo countdown in progress: alternate yellow/magenta each second so
    // background tabs flash visibly, matching old-amux behavior.
    if let Ok(guard) = tab.yolo_state.lock() {
        if let Some(ref state) = *guard {
            return if state.remaining_secs % 2 == 0 {
                Color::Yellow
            } else {
                Color::Magenta
            };
        }
    }
    if tab.stuck {
        return Color::Yellow;
    }
    // Fixed tab kinds. Both win over execution-phase colouring and both yield
    // to the stuck / yolo indicators above, which are transient run signals.
    if tab.is_amie {
        return Color::Cyan;
    }
    if tab.is_remote {
        return Color::Magenta;
    }
    match &tab.execution_phase {
        ExecutionPhase::Error { .. } => Color::Red,
        ExecutionPhase::Running { .. } => {
            if tab.container_window_state != ContainerWindowState::Hidden {
                // The container-visible "running" color is the agent-window
                // identity color: purple when the focused slot is an ACP
                // window, green for a stdio one. The Blue/Red/DarkGray phase
                // states below and above are unchanged.
                if tab.focused_slot().is_some_and(|s| s.is_acp()) {
                    crate::frontend::tui::acp_view::ACP_BORDER_COLOR
                } else {
                    Color::Green
                }
            } else {
                Color::Blue
            }
        }
        ExecutionPhase::Idle | ExecutionPhase::Done { .. } => Color::DarkGray,
    }
}

/// Execution window border color based on phase and focus.
///
/// `acp` is `true` when the focused agent slot is an ACP window: the only
/// green state (focused + Done) then becomes the ACP identity color, matching
/// [`tab_color`]. The Blue/Gray/Red/DarkGray phase states are unchanged — the
/// stdio window's appearance is identical (callers pass `acp = false`).
pub fn window_border_color(
    phase: &ExecutionPhase,
    focused: bool,
    acp: bool,
) -> ratatui::style::Color {
    use ratatui::style::Color;
    match phase {
        ExecutionPhase::Error { .. } => Color::Red,
        ExecutionPhase::Running { .. } => {
            if focused {
                Color::Blue
            } else {
                Color::Gray
            }
        }
        ExecutionPhase::Done { .. } => {
            if focused {
                if acp {
                    crate::frontend::tui::acp_view::ACP_BORDER_COLOR
                } else {
                    Color::Green
                }
            } else {
                Color::Gray
            }
        }
        ExecutionPhase::Idle => Color::DarkGray,
    }
}

/// Phase label shown in the execution window border.
///
/// Glyphs and text mirror old awman exactly:
/// - Idle → `" awman "`
/// - Running → `" ● running: {cmd} "`  (U+25CF)
/// - Done (exit 0) → `" ✓ done: {cmd} "`  (U+2713)
/// - Done (non-zero exit) → `" ✗ error: {cmd} (exit N) "`  (U+2717)
/// - Error → `" ✗ error: {cmd} "`
pub fn phase_label(phase: &ExecutionPhase) -> String {
    match phase {
        ExecutionPhase::Idle => " awman ".to_string(),
        ExecutionPhase::Running { command } => format!(" \u{25cf} running: {command} "),
        ExecutionPhase::Done { command, exit_code } if *exit_code == 0 => {
            format!(" \u{2713} done: {command} ")
        }
        ExecutionPhase::Done { command, exit_code } => {
            format!(" \u{2717} error: {command} (exit {exit_code}) ")
        }
        ExecutionPhase::Error { command, .. } => format!(" \u{2717} error: {command} "),
    }
}

/// Compute the width of each tab in the tab bar.
///
/// Dynamic sizing:
/// - **Natural**: the widest "untruncated content" across all tabs (project
///   name title vs. subcommand body) plus 2 cells for the borders, with a
///   minimum of 20 (double the old minimum). Tabs grow as wide as needed
///   to fit their content.
/// - **Budget**: when all tabs fit within the area width at their natural
///   size, use the natural size. When they don't fit, shrink to share the
///   full width equally (`area_width / n`).
///
/// Tabs never shrink below 12 cells (enough for a truncated label + ellipsis).
pub fn compute_tab_bar_width(num_tabs: usize, area_width: u16, max_natural_content: u16) -> u16 {
    if num_tabs == 0 || area_width == 0 {
        return 0;
    }
    let n = num_tabs as u16;
    let min_tab_width: u16 = 20;
    let natural = (max_natural_content + 2).max(min_tab_width);
    let total_natural = natural.saturating_mul(n);
    if total_natural <= area_width {
        natural
    } else {
        (area_width / n).max(12)
    }
}

/// Output of [`strip_alternate_screen_sequences`]: the filtered bytes plus
/// the last private-mode toggles observed in the chunk (if any), so the tab
/// can track terminal state the vt100 parser never sees (alternate screen,
/// which is stripped) or ignores (alternate scroll, mode 1007).
struct StrippedOutput {
    bytes: Vec<u8>,
    /// Last alternate-screen toggle in the chunk: `Some(true)` = entered,
    /// `Some(false)` = left, `None` = no toggle seen.
    alt_screen: Option<bool>,
    /// Last alternate-scroll (DECSET/DECRST 1007) toggle in the chunk.
    alternate_scroll: Option<bool>,
}

/// Strip DEC Private Mode Set/Reset sequences that toggle the alternate
/// screen buffer.  Agents running inside the container (e.g. Claude Code
/// in TUI mode) send these, which switches the vt100 parser to an
/// alternate grid with zero scrollback — breaking mouse-wheel scrollback.
/// By filtering these sequences the parser stays on the primary grid and
/// scrollback accumulates normally.
///
/// Recognised sequences (single-parameter forms):
///   ESC[?1049h / ESC[?1049l   (alternate screen + save/restore cursor)
///   ESC[?47h   / ESC[?47l     (alternate screen, legacy)
///   ESC[?1047h / ESC[?1047l   (alternate screen, xterm)
///
/// Additionally *observes* (without stripping) the alternate-scroll mode:
///   ESC[?1007h / ESC[?1007l   (wheel → arrow keys while on alt screen)
///
/// Both observations are reported in [`StrippedOutput`] so the tab can
/// reconstruct the agent's intended terminal state.
fn strip_alternate_screen_sequences(input: &[u8]) -> StrippedOutput {
    const ALT_ON: &[&[u8]] = &[b"\x1b[?1049h", b"\x1b[?47h", b"\x1b[?1047h"];
    const ALT_OFF: &[&[u8]] = &[b"\x1b[?1049l", b"\x1b[?47l", b"\x1b[?1047l"];
    const ALT_SCROLL_ON: &[u8] = b"\x1b[?1007h";
    const ALT_SCROLL_OFF: &[u8] = b"\x1b[?1007l";

    let mut out = Vec::with_capacity(input.len());
    let mut alt_screen = None;
    let mut alternate_scroll = None;
    let mut i = 0;
    while i < input.len() {
        if input[i] == 0x1b {
            if let Some(seq) = ALT_ON.iter().find(|s| input[i..].starts_with(s)) {
                alt_screen = Some(true);
                i += seq.len();
                continue;
            }
            if let Some(seq) = ALT_OFF.iter().find(|s| input[i..].starts_with(s)) {
                alt_screen = Some(false);
                i += seq.len();
                continue;
            }
            if input[i..].starts_with(ALT_SCROLL_ON) {
                alternate_scroll = Some(true);
                out.extend_from_slice(ALT_SCROLL_ON);
                i += ALT_SCROLL_ON.len();
                continue;
            }
            if input[i..].starts_with(ALT_SCROLL_OFF) {
                alternate_scroll = Some(false);
                out.extend_from_slice(ALT_SCROLL_OFF);
                i += ALT_SCROLL_OFF.len();
                continue;
            }
        }
        out.push(input[i]);
        i += 1;
    }
    StrippedOutput {
        bytes: out,
        alt_screen,
        alternate_scroll,
    }
}
