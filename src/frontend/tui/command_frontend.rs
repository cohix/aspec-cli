//! `TuiCommandFrontend` — the single Layer 3 struct implementing every
//! per-command frontend trait for the TUI execution mode.
//!
//! Constructed from a `ParsedCommandBoxInput` (produced by
//! `Dispatch::parse_command_box_input`). Flag/argument extraction reads from
//! the parsed input's typed maps. Interactive Q&A methods open modal dialogs
//! via the dialog channel and block until the user responds.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::command::dispatch::catalogue::{CommandCatalogue, FlagKind};
use crate::command::dispatch::parsed_input::{ArgValue, FlagValue, ParsedCommandBoxInput};
use crate::command::dispatch::CommandFrontend;
use crate::command::error::CommandError;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::engine::agent_runtime::frontend::AgentIo;
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};
use crate::frontend::tui::tabs::{
    SharedActiveWorktreePath, SharedContainerExitCode, SharedContainerName, SharedEngineTx,
    SharedPtyResetFlag, SharedResizeTx, SharedStatusDashboard, SharedStdinTx, SharedStuckSender,
    SharedTuiContext, SharedWorkflowViewState, SharedYoloCancelFlag, SharedYoloState,
};
use crate::frontend::tui::user_message::{SharedStatusLog, TuiUserMessageSink};

/// TUI frontend struct. Implements every per-command frontend trait.
///
/// Dialog channels use `std::sync::mpsc` so that the blocking `recv()` in
/// `ask_dialog` parks the OS thread rather than stalling a tokio worker —
/// the engine trait methods are synchronous, so this is the correct
/// blocking strategy.
///
/// Container I/O channels (stdout/stdin/resize) are bundled into a
/// `AgentIo` and detached lazily by the engine via `take_io`.
/// The TUI populates these channels from `App::spawn_command`; the engine's
/// container backend drains them against a real PTY master.
pub struct TuiCommandFrontend {
    parsed: ParsedCommandBoxInput,
    pub(crate) messages: TuiUserMessageSink,
    pub(crate) pty_active: bool,
    pub(crate) dialog_tx: std::sync::mpsc::Sender<DialogRequest>,
    pub(crate) dialog_rx: Mutex<std::sync::mpsc::Receiver<DialogResponse>>,
    pub(crate) container_io: Option<AgentIo>,
    pub(crate) status_log: SharedStatusLog,
    pub(crate) workflow_view: SharedWorkflowViewState,
    pub(crate) yolo_state: SharedYoloState,
    pub(crate) yolo_cancel_flag: SharedYoloCancelFlag,
    pub(crate) pty_reset_flag: SharedPtyResetFlag,
    pub(crate) container_name_shared: SharedContainerName,
    /// Shared exit-code slot: written when the engine reports a workflow
    /// container actually terminated; the TUI event loop takes it and closes
    /// the container window.
    pub(crate) container_exit_shared: SharedContainerExitCode,
    /// Persistent stdout sender — kept alive across workflow steps so each
    /// new `AgentIo` can send output to the same TUI event loop receiver.
    pub(crate) stdout_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Shared slot for the stdin sender. When a new workflow step creates
    /// fresh stdin channels, the new sender is placed here so the TUI event
    /// loop can pick it up and forward keystrokes to the new container.
    pub(crate) stdin_tx_shared:
        std::sync::Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>>,
    /// Shared slot for the resize sender, same pattern as stdin_tx_shared.
    #[allow(clippy::type_complexity)]
    pub(crate) resize_tx_shared:
        std::sync::Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<(u16, u16)>>>>,
    /// Shared slot for the engine sender. The engine publishes the
    /// sender here via `set_engine_sender`; the TUI event loop reads
    /// it to send Ctrl-W requests.
    pub(crate) engine_tx_shared: SharedEngineTx,
    /// Shared stuck sender. The engine publishes the container's stuck
    /// broadcast sender here; the TUI subscribes for tab coloring.
    pub(crate) stuck_sender_shared: SharedStuckSender,
    /// Shared active-worktree path. The worktree-lifecycle frontend sets
    /// this when a worktree is created/resumed and clears it on cleanup;
    /// the renderer reads it for the bottom-bar context line.
    pub(crate) active_worktree_path: SharedActiveWorktreePath,
    /// Shared status dashboard data. The status command writes structured
    /// container data here; the TUI renderer reads it to display a proper
    /// `Table` widget.
    pub(crate) status_dashboard: SharedStatusDashboard,
    /// Live TUI context shared with the event loop. The event loop refreshes
    /// this on every tick; the status command reads it on each watch iteration.
    pub(crate) tui_context_shared: SharedTuiContext,
    /// Shared queue of parallel-slot lifecycle events (WI-0096). The parallel
    /// workflow callbacks push here; the TUI event loop drains it to maintain
    /// `Tab::container_slots`.
    pub(crate) container_slot_events: crate::frontend::tui::tabs::SharedContainerSlotEvents,
    pub(crate) parallel_group_active: bool,
    pub(crate) pending_step_slot_io: HashMap<String, crate::frontend::tui::tabs::ContainerSlotIo>,
    /// Per-step cancel flags for in-flight parallel yolo countdowns, keyed by
    /// step name. Created in `parallel_step_yolo_countdown_started`, shared
    /// with the slot via `ContainerSlotEvent::YoloStarted` so the TUI event
    /// loop can request cancellation (Esc on the per-slot countdown modal);
    /// checked and removed in `parallel_step_yolo_countdown_tick` /
    /// `_finished`.
    pub(crate) pending_parallel_yolo_cancel: HashMap<String, SharedYoloCancelFlag>,
    /// Field name of the most recent config-dialog edit, so the re-presented
    /// table reopens with that row selected (the `config show` edit loop
    /// presents the dialog again after every save).
    pub(crate) last_config_edit_field: Option<String>,
}

// `SquadCommandFrontend` for `TuiCommandFrontend` (interview dialogs) lives in
// `per_command::squad`, alongside the other per-command TUI frontend impls.

impl TuiCommandFrontend {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        parsed: ParsedCommandBoxInput,
        status_log: SharedStatusLog,
        dialog_tx: std::sync::mpsc::Sender<DialogRequest>,
        dialog_rx: std::sync::mpsc::Receiver<DialogResponse>,
        container_io: AgentIo,
        workflow_view: SharedWorkflowViewState,
        yolo_state: SharedYoloState,
        yolo_cancel_flag: SharedYoloCancelFlag,
        pty_reset_flag: SharedPtyResetFlag,
        container_name_shared: SharedContainerName,
        container_exit_shared: SharedContainerExitCode,
        stdin_tx_shared: SharedStdinTx,
        resize_tx_shared: SharedResizeTx,
        engine_tx_shared: SharedEngineTx,
        stuck_sender_shared: SharedStuckSender,
        active_worktree_path: SharedActiveWorktreePath,
        status_dashboard: SharedStatusDashboard,
        tui_context_shared: SharedTuiContext,
        container_slot_events: crate::frontend::tui::tabs::SharedContainerSlotEvents,
    ) -> Self {
        let stdout_tx = container_io.stdout.clone();
        Self {
            parsed,
            messages: TuiUserMessageSink::new(status_log.clone()),
            pty_active: false,
            dialog_tx,
            dialog_rx: Mutex::new(dialog_rx),
            container_io: Some(container_io),
            status_log,
            workflow_view,
            yolo_state,
            yolo_cancel_flag,
            pty_reset_flag,
            container_name_shared,
            container_exit_shared,
            stdout_tx,
            stdin_tx_shared,
            resize_tx_shared,
            engine_tx_shared,
            stuck_sender_shared,
            active_worktree_path,
            status_dashboard,
            tui_context_shared,
            container_slot_events,
            parallel_group_active: false,
            pending_step_slot_io: HashMap::new(),
            pending_parallel_yolo_cancel: HashMap::new(),
            last_config_edit_field: None,
        }
    }

    /// Recreate `AgentIo` channels for a new workflow step. The stdout
    /// sender is reused (same TUI event loop receiver), but stdin and resize
    /// get fresh channels. The new senders are published via shared slots so
    /// the TUI event loop can swap to them.
    pub(crate) fn recreate_container_io(&mut self) {
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let stdin_tx_for_engine = stdin_tx.clone();
        let (resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();

        let initial_size = match crossterm::terminal::size() {
            Ok((cols, rows)) => {
                crate::frontend::tui::event_loop::compute_container_inner_size(cols, rows)
            }
            Err(_) => (80u16, 24u16),
        };

        // Publish new senders so the TUI event loop picks them up.
        if let Ok(mut guard) = self.stdin_tx_shared.lock() {
            *guard = Some(stdin_tx);
        }
        if let Ok(mut guard) = self.resize_tx_shared.lock() {
            *guard = Some(resize_tx);
        }

        self.container_io = Some(AgentIo {
            stdout: self.stdout_tx.clone(),
            stderr: self.stdout_tx.clone(),
            stdin_tx: stdin_tx_for_engine,
            stdin_rx,
            resize: Some(resize_rx),
            initial_size: Some(initial_size),
        });
    }

    pub(crate) fn recreate_parallel_container_io(&mut self, step_name: &str) {
        let (stdout_tx, stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let stdin_tx_for_engine = stdin_tx.clone();
        let (resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();

        let initial_size = match crossterm::terminal::size() {
            Ok((cols, rows)) => {
                crate::frontend::tui::event_loop::compute_container_inner_size(cols, rows)
            }
            Err(_) => (80u16, 24u16),
        };

        self.container_io = Some(AgentIo {
            stdout: stdout_tx.clone(),
            stderr: stdout_tx,
            stdin_tx: stdin_tx_for_engine,
            stdin_rx,
            resize: Some(resize_rx),
            initial_size: Some(initial_size),
        });
        self.pending_step_slot_io.insert(
            step_name.to_string(),
            crate::frontend::tui::tabs::ContainerSlotIo {
                stdout_rx,
                stdin_tx,
                resize_tx,
            },
        );
    }

    /// Push a parallel-slot lifecycle event into the shared queue for the
    /// TUI event loop to drain (WI-0096).
    pub(crate) fn push_container_slot_event(
        &self,
        event: crate::frontend::tui::tabs::ContainerSlotEvent,
    ) {
        if let Ok(mut q) = self.container_slot_events.lock() {
            q.push_back(event);
        }
    }

    /// Send a dialog request and block waiting for the response.
    ///
    /// This uses `std::sync::mpsc::Receiver::recv()` which blocks the OS
    /// thread. Since engine trait methods are synchronous this is correct —
    /// no tokio executor is blocked.
    pub(crate) fn ask_dialog(
        &self,
        request: DialogRequest,
    ) -> Result<DialogResponse, CommandError> {
        let _ = self.dialog_tx.send(request);
        self.dialog_rx
            .lock()
            .map_err(|_| CommandError::Aborted)?
            .recv()
            .map_err(|_| CommandError::Aborted)
    }

    /// Check if a flag-path flag is a known Bool flag in the catalogue.
    fn is_known_bool_flag(&self, command_path: &[&str], flag: &str) -> bool {
        let cat = CommandCatalogue::get();
        cat.lookup(command_path)
            .and_then(|spec| spec.find_flag(flag))
            .map(|f| matches!(f.kind, FlagKind::Bool))
            .unwrap_or(false)
    }
}

// ─── UserMessageSink ──────────────────────────────────────────────────────

impl UserMessageSink for TuiCommandFrontend {
    fn write_message(&mut self, msg: UserMessage) {
        self.messages.write_message(msg);
    }

    fn replay_queued(&mut self) {
        self.messages.replay_queued();
    }
}

// ─── CommandFrontend ──────────────────────────────────────────────────────

impl CommandFrontend for TuiCommandFrontend {
    fn flag_bool(&self, _command_path: &[&str], flag: &str) -> Result<Option<bool>, CommandError> {
        match self.parsed.flags.get(flag) {
            Some(FlagValue::Bool(v)) => Ok(Some(*v)),
            Some(_) => Ok(Some(true)),
            None => {
                if self.is_known_bool_flag(
                    &self
                        .parsed
                        .path
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>(),
                    flag,
                ) {
                    Ok(Some(false))
                } else {
                    Ok(None)
                }
            }
        }
    }

    fn flag_string(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<String>, CommandError> {
        match self.parsed.flags.get(flag) {
            Some(FlagValue::String(v)) => Ok(Some(v.clone())),
            _ => Ok(None),
        }
    }

    fn flag_strings(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Vec<String>, CommandError> {
        match self.parsed.flags.get(flag) {
            Some(FlagValue::Strings(v)) => Ok(v.clone()),
            Some(FlagValue::String(v)) => Ok(vec![v.clone()]),
            _ => Ok(Vec::new()),
        }
    }

    fn flag_path(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<PathBuf>, CommandError> {
        match self.parsed.flags.get(flag) {
            Some(FlagValue::String(v)) => Ok(Some(PathBuf::from(v))),
            _ => Ok(None),
        }
    }

    fn flag_enum(&self, command_path: &[&str], flag: &str) -> Result<Option<String>, CommandError> {
        self.flag_string(command_path, flag)
    }

    fn flag_u16(&self, _command_path: &[&str], flag: &str) -> Result<Option<u16>, CommandError> {
        match self.parsed.flags.get(flag) {
            Some(FlagValue::String(v)) => {
                v.parse::<u16>()
                    .map(Some)
                    .map_err(|_| CommandError::InvalidFlagValue {
                        command: self.parsed.path.clone(),
                        flag: flag.to_string(),
                        reason: format!("'{v}' is not a valid u16"),
                    })
            }
            _ => Ok(None),
        }
    }

    fn flag_usize(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<usize>, CommandError> {
        match self.parsed.flags.get(flag) {
            Some(FlagValue::String(v)) => {
                let n: usize = v.parse().map_err(|_| CommandError::InvalidFlagValue {
                    command: self.parsed.path.clone(),
                    flag: flag.to_string(),
                    reason: format!("'{v}' is not a valid number"),
                })?;
                if n < 1 {
                    return Err(CommandError::InvalidFlagValue {
                        command: self.parsed.path.clone(),
                        flag: flag.to_string(),
                        reason: format!("'{v}' must be >= 1"),
                    });
                }
                Ok(Some(n))
            }
            _ => Ok(None),
        }
    }

    fn argument(&self, _command_path: &[&str], name: &str) -> Result<Option<String>, CommandError> {
        match self.parsed.arguments.get(name) {
            Some(ArgValue::Single(v)) => Ok(Some(v.clone())),
            Some(ArgValue::Multi(v)) => Ok(Some(v.join(" "))),
            None => Ok(None),
        }
    }

    fn arguments(&self, _command_path: &[&str], name: &str) -> Result<Vec<String>, CommandError> {
        match self.parsed.arguments.get(name) {
            Some(ArgValue::Multi(v)) => Ok(v.clone()),
            Some(ArgValue::Single(v)) => Ok(vec![v.clone()]),
            None => Ok(Vec::new()),
        }
    }
}
