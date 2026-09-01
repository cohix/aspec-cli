//! Application state — the central TUI state object.
//!
//! `App` stores UI state only. All command execution delegates to `Dispatch`
//! and the per-command frontend trait chain.

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::command::dispatch::parsed_input::ParsedCommandBoxInput;
use crate::command::dispatch::{CommandOutcome, Dispatch, Engines};
use crate::command::error::CommandError;
use crate::data::session::Session;
use crate::data::session_manager::SessionManager;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{Dialog, DialogRequest, DialogResponse};
use crate::frontend::tui::tabs::{ExecutionPhase, Tab};
use crate::frontend::tui::text_edit::TextEdit;

/// Resolve the agent name shown in the container overlay title, using the
/// same precedence as the engine (`resolve_agent`): explicit `--agent` flag,
/// then the session's configured default agent, then "claude".
/// Used to seed `ContainerInfo.agent_display_name`.
fn agent_name_from_parsed(parsed: &ParsedCommandBoxInput, session: &Session) -> String {
    use crate::command::dispatch::parsed_input::FlagValue;
    let flag = match parsed.flags.get("agent") {
        Some(FlagValue::String(s)) => Some(s.clone()),
        _ => None,
    };
    match crate::command::commands::resolve_agent(&flag, session) {
        Ok(name) => name.into_string(),
        // resolve_agent only fails on a malformed flag value; the dispatch
        // layer will reject the command with a proper error, so the title
        // fallback here is cosmetic.
        Err(_) => flag.unwrap_or_else(|| "claude".to_string()),
    }
}

/// UI focus target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    CommandBox,
    ExecutionWindow,
}

/// Status bar state.
#[derive(Debug, Clone, Default)]
pub struct StatusBar {
    pub text: String,
}

/// Everything [`App::build_amie_tab`] produces. `key_setup` is `Some` only on
/// the run that minted the amie bearer key, and carries the shell snippet the
/// caller must show before it is lost — the plaintext key exists nowhere else.
pub struct AmieTabBuild {
    pub tab: Tab,
    pub gateway: Arc<dyn crate::command::commands::amie::gateway::ConditionGateway>,
    pub key_setup: Option<String>,
}

/// Central TUI state. Contains NO business logic — only UI state.
pub struct App {
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    pub active_dialog: Option<Dialog>,
    pub focus: Focus,
    pub catalogue: &'static CommandCatalogue,
    pub engines: Engines,
    pub session_manager: Arc<RwLock<SessionManager>>,
    pub command_input: TextEdit,
    pub suggestion_row: Vec<String>,
    pub input_error: Option<String>,
    pub status_bar: StatusBar,
    pub should_quit: bool,
    pub needs_redraw: bool,
    pub command_dialog_active: bool,
    pub runtime_handle: tokio::runtime::Handle,
    /// Gateway for the singleton amie tab, obtained from
    /// `AmieSupervisor::ensure_running()` when the tab is opened. Injected into
    /// `Dispatch` for `amie` in-tab actions so they reach the daemon. `None`
    /// until the amie tab exists.
    pub amie_gateway: Option<Arc<dyn crate::command::commands::amie::gateway::ConditionGateway>>,
    /// Receiver for asynchronous container stats results. The middle element
    /// is the step name of the slot the sample was polled for (empty for the
    /// single/backbone slot of a plain containerized command).
    #[allow(clippy::type_complexity)]
    pub stats_rx: Option<
        std::sync::mpsc::Receiver<(
            usize,
            String,
            crate::engine::agent_runtime::execution::AgentStats,
        )>,
    >,
    /// Sender cloned per stats query — kept alive so the channel stays open.
    #[allow(clippy::type_complexity)]
    pub stats_tx: std::sync::mpsc::Sender<(
        usize,
        String,
        crate::engine::agent_runtime::execution::AgentStats,
    )>,
    /// Tracks when the last stats query was dispatched so we don't spam.
    pub last_stats_poll: std::time::Instant,
    /// `(tab index, slot step name)` pairs with a stats query still running.
    /// A container-runtime `stats()` call can take longer than the poll
    /// interval on a busy daemon; without this guard every tick would pile
    /// another query onto the same slot until the runtime CLI is swamped and
    /// no slot's numbers stay current.
    pub in_flight_stats: Arc<std::sync::Mutex<std::collections::HashSet<(usize, String)>>>,
}

impl App {
    pub fn new(
        catalogue: &'static CommandCatalogue,
        engines: Engines,
        session_manager: Arc<RwLock<SessionManager>>,
        initial_tab: Tab,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self {
        let (stats_tx, stats_rx) = std::sync::mpsc::channel();
        Self {
            tabs: vec![initial_tab],
            active_tab: 0,
            active_dialog: None,
            focus: Focus::CommandBox,
            catalogue,
            engines,
            session_manager,
            command_input: TextEdit::new(false),
            suggestion_row: Vec::new(),
            input_error: None,
            status_bar: StatusBar::default(),
            should_quit: false,
            needs_redraw: true,
            command_dialog_active: false,
            runtime_handle,
            amie_gateway: None,
            stats_rx: Some(stats_rx),
            stats_tx,
            last_stats_poll: std::time::Instant::now() - std::time::Duration::from_secs(10),
            in_flight_stats: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        }
    }

    pub fn active_tab(&self) -> &Tab {
        &self.tabs[self.active_tab]
    }

    pub fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active_tab]
    }

    pub fn switch_to_prev_tab(&mut self) {
        if self.active_tab > 0 {
            self.active_tab -= 1;
        } else if !self.tabs.is_empty() {
            self.active_tab = self.tabs.len() - 1;
        }
    }

    pub fn switch_to_next_tab(&mut self) {
        if self.active_tab + 1 < self.tabs.len() {
            self.active_tab += 1;
        } else {
            self.active_tab = 0;
        }
    }

    pub fn close_active_tab(&mut self) {
        if self.tabs.len() <= 1 {
            self.should_quit = true;
            return;
        }
        self.tabs.remove(self.active_tab);
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len().saturating_sub(1);
        }
    }

    /// Focus the existing amie tab, or create the singleton one.
    ///
    /// On failure nothing is created: the error text — which already names
    /// `awman api` (daemon conflict) or the sandbox runtime — is surfaced
    /// verbatim in the status bar, never a generic "could not connect".
    /// Idempotent: a second call focuses the tab created by the first.
    pub fn open_or_focus_amie_tab(&mut self) {
        if let Some(idx) = self.tabs.iter().position(|t| t.is_amie) {
            self.active_tab = idx;
            self.needs_redraw = true;
            return;
        }
        match Self::build_amie_tab(&self.engines, &self.runtime_handle) {
            Ok(build) => {
                self.amie_gateway = Some(build.gateway);
                self.tabs.push(build.tab);
                self.active_tab = self.tabs.len() - 1;
                if let Some(body) = build.key_setup {
                    self.active_dialog = Some(Dialog::Notice {
                        title: "amie authentication".to_string(),
                        body,
                    });
                }
                self.needs_redraw = true;
            }
            Err(message) => {
                // The error text already names `awman api` (daemon conflict) or
                // the sandbox runtime; surface it verbatim and open nothing.
                self.status_bar.text = message;
            }
        }
    }

    /// Build the singleton amie tab: ensure the daemon is running, obtain a
    /// gateway, construct the synthetic-session tab, and start its poller.
    /// Shared by [`App::open_or_focus_amie_tab`] and `tui::run`'s
    /// `InitialTab::Amie` path. On failure returns the specific error text
    /// (naming `awman api` or the sandbox runtime), never a generic message.
    pub(crate) fn build_amie_tab(
        engines: &crate::command::dispatch::Engines,
        runtime_handle: &tokio::runtime::Handle,
    ) -> Result<AmieTabBuild, String> {
        use crate::command::commands::amie::daemon::AmieSupervisor;
        use crate::command::commands::amie::gateway::ConditionGateway;
        use crate::command::commands::amie::runtime_guard::require_container_tier;
        use crate::data::config::env::Env;

        // A sandbox-class runtime cannot back amie: report the runtime error,
        // never a generic connection failure.
        require_container_tier(engines).map_err(|e| e.to_string())?;

        let env = Env::from_process();
        let supervisor = Arc::new(AmieSupervisor::from_env(&env).map_err(|e| e.to_string())?);

        // `ensure_running` is async, but the caller may be on a tokio worker
        // thread where `Handle::block_on` would panic. Drive it on the runtime
        // and block on the result channel instead
        // (see /awman/context/workflow/deviations.md, D-tui-3).
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let supervisor = supervisor.clone();
            runtime_handle.spawn(async move {
                let _ = tx.send(supervisor.ensure_running().await);
            });
        }
        let gateway = match rx.recv() {
            Ok(Ok(gateway)) => gateway,
            Ok(Err(error)) => return Err(error.to_string()),
            Err(_) => return Err("amie daemon startup was interrupted".to_string()),
        };
        // A first run mints the bearer key here. It has to be shown, or the
        // user is left with a daemon they cannot authenticate to from any
        // later process; the caller raises it as a dismissable modal.
        let key_setup = supervisor.take_generated_key_setup();

        let session = Self::amie_synthetic_session()
            .map_err(|error| format!("failed to open amie tab: {error}"))?;

        let gateway: Arc<dyn ConditionGateway> = Arc::new(gateway);

        let mut tab = Tab::new_amie(session);
        {
            // Ensure a runtime context so the poller's `tokio::spawn` works even
            // when this runs off a runtime thread.
            let _guard = runtime_handle.enter();
            let (poller, cancel) = {
                let state = tab
                    .amie
                    .as_ref()
                    .expect("new_amie always installs amie state");
                (
                    crate::frontend::tui::amie_poll::AmieConditionPoller::new(
                        gateway.clone(),
                        state,
                    ),
                    state.cancel.clone(),
                )
            };
            let handle = poller.start(cancel);
            tab.amie
                .as_mut()
                .expect("new_amie always installs amie state")
                .set_poll_handle(handle);
        }
        Ok(AmieTabBuild {
            tab,
            gateway,
            key_setup,
        })
    }

    /// Build the amie tab's synthetic session, rooted at the amie storage root.
    /// It exists solely to satisfy the `Tab` API; nothing rendered in the amie
    /// tab derives from it.
    fn amie_synthetic_session() -> Result<Session, crate::data::error::DataError> {
        use crate::data::config::env::Env;
        use crate::data::fs::amie_paths::AmiePaths;
        use crate::data::session::SessionOpenOptions;

        let root = AmiePaths::from_process_env()?.root().to_path_buf();
        std::fs::create_dir_all(&root).ok();
        Session::open_at_git_root(
            root.clone(),
            root,
            SessionOpenOptions {
                env: Some(Env::from_process()),
                ..Default::default()
            },
        )
    }

    /// Spawn a parsed command as an async tokio task, wiring up all channels
    /// between the event loop and the command thread.
    pub fn spawn_command(&mut self, _command_text: &str, parsed: ParsedCommandBoxInput) {
        // WI 0102: `amie attach` has no Layer-2 command; intercept it before
        // the ordinary Dispatch spawn, mirroring `cli::run`'s carve-out.
        if parsed.path.as_slice() == ["amie", "attach"] {
            let name = match parsed.arguments.get("name") {
                Some(crate::command::dispatch::parsed_input::ArgValue::Single(name)) => {
                    name.clone()
                }
                _ => String::new(),
            };
            crate::frontend::tui::amie_attach::start_amie_attach(self, &name);
            return;
        }

        // Capture the runtime class before borrowing the tab mutably: a
        // sandbox-class runtime (e.g. docker-sbx-experimental) labels the
        // overlay "(sandboxed)" rather than "(containerized)".
        let sandboxed = self.engines.sandbox_runtime.is_some();
        let tab = self.active_tab_mut();

        // Clear previous output so the new command starts with a fresh log.
        if let Ok(mut log) = tab.status_log.lock() {
            log.clear();
        }
        if let Ok(mut dash) = tab.status_dashboard.lock() {
            *dash = None;
        }
        tab.scroll_offset = 0;

        // Clear previous workflow state so the overview resets for the new command.
        // Also clear the overview hit-test rect so stale rects from a previous
        // workflow don't intercept mouse-scroll events meant for the container.
        if let Ok(mut guard) = tab.workflow_state.lock() {
            *guard = None;
        }
        tab.last_overview_rect = None;
        if let Ok(mut guard) = tab.yolo_state.lock() {
            *guard = None;
        }

        // Dialog channels (std::sync::mpsc — command thread blocks on recv).
        let (dialog_req_tx, dialog_req_rx) = std::sync::mpsc::channel::<DialogRequest>();
        let (dialog_resp_tx, dialog_resp_rx) = std::sync::mpsc::channel::<DialogResponse>();

        // Container I/O channels — tokio mpsc throughout so the engine PTY
        // bridge can use them from async tasks. The TUI keeps a clone of the
        // stdin sender (for user keystrokes) and the engine receives both
        // sender and receiver for the PTY bridge plus inject_prompt.
        let (stdout_tx, stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let stdin_tx_for_engine = stdin_tx.clone();
        let (resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();

        // Command result channel.
        let (result_tx, result_rx) =
            std::sync::mpsc::channel::<Result<CommandOutcome, CommandError>>();

        // Initial PTY size: derive from the current terminal so the
        // container starts with a correctly-sized grid (otherwise TUI apps
        // inside the container, like Claude, would render against an 80x24
        // default until the first SIGWINCH).
        let initial_size = match crossterm::terminal::size() {
            Ok((cols, rows)) => {
                let sidebar =
                    crate::frontend::tui::git_sidebar::sidebar_width(cols, tab.git_sidebar_state);
                crate::frontend::tui::event_loop::compute_container_inner_size(
                    cols.saturating_sub(sidebar),
                    rows,
                )
            }
            Err(_) => (80u16, 24u16),
        };

        let container_io = crate::engine::agent_runtime::frontend::AgentIo {
            stdout: stdout_tx.clone(),
            stderr: stdout_tx,
            stdin_tx: stdin_tx_for_engine,
            stdin_rx,
            resize: Some(resize_rx),
            initial_size: Some(initial_size),
        };

        // Build the TUI frontend. Workflow + yolo overlays share the same
        // `Arc<Mutex<...>>` between the engine-side frontend impl and the
        // renderer.
        tab.container_name_shared = std::sync::Arc::new(std::sync::Mutex::new(None));
        tab.container_exit_shared = std::sync::Arc::new(std::sync::Mutex::new(None));
        tab.stdin_tx_shared = std::sync::Arc::new(std::sync::Mutex::new(None));
        tab.resize_tx_shared = std::sync::Arc::new(std::sync::Mutex::new(None));
        tab.engine_tx_shared = std::sync::Arc::new(std::sync::Mutex::new(None));
        let frontend = TuiCommandFrontend::new(
            parsed.clone(),
            tab.status_log.clone(),
            dialog_req_tx,
            dialog_resp_rx,
            container_io,
            tab.workflow_state.clone(),
            tab.yolo_state.clone(),
            tab.yolo_cancel_flag.clone(),
            tab.pty_reset_flag.clone(),
            tab.container_name_shared.clone(),
            tab.container_exit_shared.clone(),
            tab.stdin_tx_shared.clone(),
            tab.resize_tx_shared.clone(),
            tab.engine_tx_shared.clone(),
            tab.stuck_sender_shared.clone(),
            tab.active_worktree_path.clone(),
            tab.status_dashboard.clone(),
            tab.tui_context_shared.clone(),
            tab.container_slot_events.clone(),
        );

        tab.command_result_rx = Some(result_rx);
        tab.dialog_request_rx = Some(dialog_req_rx);
        tab.dialog_response_tx = Some(dialog_resp_tx);

        let command_name = parsed.path.join(" ");
        let agent_display = agent_name_from_parsed(&parsed, &tab.session);

        // Install the command's container slot (the N==1 case of the unified
        // slot model), replacing any slots from the previous command. Its
        // `ContainerInfo` lets the overlay title show the agent name and
        // elapsed time before the engine reports the actual container name;
        // the parser starts at the computed overlay size so agents never lay
        // out against an 80x24 default.
        tab.start_container(
            agent_display.clone(),
            String::new(),
            initial_size.0,
            initial_size.1,
        );
        if let Some(slot) = tab.focused_slot_mut() {
            slot.container_stdout_rx = Some(stdout_rx);
            slot.container_stdin_tx = Some(stdin_tx);
            slot.container_resize_tx = Some(resize_tx);
            if let Some(info) = slot.container_info.as_mut() {
                info.sandboxed = sandboxed;
            }
        }
        tab.suppress_container_auto_open = false;

        // Show the "Interactive Mode" banner for containerized commands.
        let is_containerized = matches!(
            parsed.path.first().map(|s| s.as_str()),
            Some("chat" | "exec")
        );
        if is_containerized {
            use crate::data::message::UserMessageSink;
            use crate::frontend::tui::user_message::TuiUserMessageSink;
            let mut sink = TuiUserMessageSink::new(tab.status_log.clone());
            sink.info(
                "╔══════════════════════════════════════════════════════════════╗".to_string(),
            );
            sink.info(
                "║                                                              ║".to_string(),
            );
            sink.info("║     ╦╔╗╔╔╦╗╔═╗╦═╗╔═╗╔═╗╔╦╗╦╦  ╦╔═╗  ╔╦╗╔═╗╔╦╗╔═╗        ║".to_string());
            sink.info("║     ║║║║ ║ ║╣ ╠╦╝╠═╣║   ║ ║╚╗╔╝║╣   ║║║║ ║ ║║║╣         ║".to_string());
            sink.info("║     ╩╝╚╝ ╩ ╚═╝╩╚═╩ ╩╚═╝ ╩ ╩ ╚╝ ╚═╝  ╩ ╩╚═╝═╩╝╚═╝       ║".to_string());
            sink.info(
                "║                                                              ║".to_string(),
            );
            sink.info(format!(
                "║  Agent '{}' is launching in INTERACTIVE mode.{}║",
                agent_display,
                " ".repeat(46usize.saturating_sub(agent_display.len() + 43))
            ));
            sink.info(
                "║  You will need to quit the agent (Ctrl+C or exit)            ║".to_string(),
            );
            sink.info(
                "║  when its work is complete.                                  ║".to_string(),
            );
            sink.info(
                "║                                                              ║".to_string(),
            );
            sink.info(
                "╚══════════════════════════════════════════════════════════════╝".to_string(),
            );
        }

        tab.yolo_mode = parsed
            .flags
            .get("yolo")
            .map(|v| {
                matches!(
                    v,
                    crate::command::dispatch::parsed_input::FlagValue::Bool(true)
                )
            })
            .unwrap_or(false)
            || parsed
                .flags
                .get("auto")
                .map(|v| {
                    matches!(
                        v,
                        crate::command::dispatch::parsed_input::FlagValue::Bool(true)
                    )
                })
                .unwrap_or(false);
        tab.execution_phase = ExecutionPhase::Running {
            command: command_name,
        };

        // Build the dispatch and spawn the command using the tab's session
        // so commands execute in the correct working directory.
        let tab_session = self.active_tab().session.clone();
        let session = Arc::new(RwLock::new(tab_session));
        let engines = self.engines.clone();
        let path_owned: Vec<String> = parsed.path.clone();
        // WI 0102: in-tab `amie` actions reach the daemon through the same
        // gateway the CLI subcommands use, injected into Dispatch. Without it
        // they fail with 0101's "amie conditions are served by the amie daemon"
        // error.
        let amie_gateway = self.amie_gateway.clone();

        self.runtime_handle.spawn(async move {
            let mut dispatch = Dispatch::new(frontend, session, engines);
            if path_owned.first().map(String::as_str) == Some("amie") {
                if let Some(gateway) = amie_gateway {
                    dispatch = dispatch.with_amie_gateway(gateway);
                }
            }
            let path_refs: Vec<&str> = path_owned.iter().map(|s| s.as_str()).collect();
            let result = dispatch.run_command(&path_refs).await;
            let _ = result_tx.send(result);
        });
    }

    /// Add a new tab backed by the given session. Returns the index of the
    /// new tab.
    pub fn add_tab(&mut self, session: Session) -> usize {
        let tab = Tab::new(session);
        self.tabs.push(tab);
        self.tabs.len() - 1
    }

    /// Tick all tabs: drain container output, poll for command completion,
    /// poll for stats results, and recompute the per-tab stuck flag.
    pub fn tick_all_tabs(&mut self) {
        let active = self.active_tab;
        for (idx, tab) in self.tabs.iter_mut().enumerate() {
            // WI 0102: drive the amie tab's poll loop — it fetches only while
            // focused — and publish the selected condition name for the poller
            // so it never reads UI state directly.
            if let Some(state) = tab.amie.as_ref() {
                state
                    .focused
                    .store(idx == active, std::sync::atomic::Ordering::Relaxed);
                if let Ok(mut selected) = state.poll_selected.lock() {
                    *selected = state.selected_name();
                }
            }

            // Maintain container_slots before draining output (a Launched
            // event may carry the channels the drain reads) and before
            // aggregating stuck/yolo (drain_stuck_events reads slot flags).
            tab.drain_container_slot_events();
            tab.drain_container_output();
            tab.poll_container_exit();
            tab.poll_command_completion();
            tab.drain_stuck_events();
            // Restart the git poll task if the worktree path changed.
            tab.refresh_git_poll();

            // TUI-4: keep every slot's parser and PTY in lockstep with the
            // actual rendered overlay dimensions. The overlay size varies
            // with Workflow Overview height and other dynamic chrome; the
            // initial `compute_container_inner_size` estimate may not match.
            // All slots render into the same maximized overlay when focused,
            // so they all track the same size; syncing the background slots
            // too means Ctrl-S never swaps in a stale grid.
            if tab.container_window_state
                != crate::frontend::tui::tabs::ContainerWindowState::Hidden
            {
                if let Some(inner) = tab.container_inner_area {
                    for slot in &mut tab.container_slots {
                        let (vt_rows, vt_cols) = slot.vt100_parser.screen().size();
                        if vt_cols != inner.width || vt_rows != inner.height {
                            slot.vt100_parser
                                .screen_mut()
                                .set_size(inner.height, inner.width);
                            if let Some(ref tx) = slot.container_resize_tx {
                                let _ = tx.send((inner.width, inner.height));
                            }
                        }
                    }
                }
            }

            // Pick up the container name from the engine (set via
            // `report_status(Running { container_name })`) into the focused
            // slot. Also handles sequential workflow step transitions: the
            // engine clears the shared name then sets a new one when the
            // next container reports Running. During a parallel group the
            // backbone slot is dormant and per-slot names arrive via
            // `ContainerSlotEvent::ContainerName` instead, so the shared
            // name is left alone to avoid mislabeling a group slot.
            if tab.dormant_slots.is_empty() {
                let name = tab
                    .container_name_shared
                    .lock()
                    .ok()
                    .and_then(|mut g| g.take());
                if let Some(name) = name {
                    if let Some(info) = tab
                        .focused_slot_mut()
                        .and_then(|s| s.container_info.as_mut())
                    {
                        info.container_name = name;
                        info.latest_stats = None;
                    }
                    // A fresh container is running — let its output
                    // auto-open the window again after a mid-workflow
                    // container exit closed it.
                    tab.suppress_container_auto_open = false;
                }
            }

            // Pick up new stdin/resize senders from sequential workflow step
            // transitions. When `recreate_container_io()` runs on the engine
            // thread, it publishes new senders via the shared slots; swap
            // them into the focused (backbone) slot so keystrokes and resize
            // events reach the new container.
            let new_stdin = tab.stdin_tx_shared.lock().ok().and_then(|mut g| g.take());
            let new_resize = tab.resize_tx_shared.lock().ok().and_then(|mut g| g.take());
            if new_stdin.is_some() || new_resize.is_some() {
                // These only come from the sequential path; if a parallel
                // group is active the backbone is dormant — apply them there
                // so they aren't lost (or misrouted to a group slot).
                let target = if tab.dormant_slots.is_empty() {
                    tab.focused_slot_mut()
                } else {
                    tab.dormant_slots.first_mut()
                };
                if let Some(slot) = target {
                    if let Some(new_tx) = new_stdin {
                        slot.container_stdin_tx = Some(new_tx);
                    }
                    if let Some(new_tx) = new_resize {
                        slot.container_resize_tx = Some(new_tx);
                    }
                }
            }
        }

        // Refresh the TUI context shared with the status command. Each tab
        // holds a shared slot; the status watch loop reads it on every tick
        // so it always sees current container-name and stuck state.
        {
            use crate::command::commands::status::{StatusCommandTuiContext, TuiTabSnapshot};
            let snapshots: Vec<TuiTabSnapshot> = self
                .tabs
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let container_name = t
                        .focused_slot()
                        .and_then(|s| s.container_info.as_ref())
                        .map(|info| info.container_name.clone())
                        .filter(|n| !n.is_empty());
                    let command_label = match &t.execution_phase {
                        ExecutionPhase::Running { command } => command.clone(),
                        ExecutionPhase::Done { command, .. } => command.clone(),
                        ExecutionPhase::Error { command, .. } => command.clone(),
                        ExecutionPhase::Idle => String::new(),
                    };
                    TuiTabSnapshot {
                        tab_number: (i + 1) as u32,
                        container_name,
                        is_stuck: t.stuck,
                        command_label,
                    }
                })
                .collect();
            let ctx = StatusCommandTuiContext::new(snapshots);
            for tab in &self.tabs {
                if let Ok(mut g) = tab.tui_context_shared.lock() {
                    *g = ctx.clone();
                }
            }
        }

        // Drain any completed stats results, routing each sample to the
        // slot it was polled for (matched by step name; a stale sample whose
        // slot has since exited is simply dropped).
        if let Some(ref rx) = self.stats_rx {
            while let Ok((tab_idx, slot_step, stats)) = rx.try_recv() {
                if tab_idx >= self.tabs.len() {
                    continue;
                }
                let tab = &mut self.tabs[tab_idx];
                let info = tab
                    .container_slots
                    .iter_mut()
                    .find(|s| s.step_name == slot_step)
                    .and_then(|s| s.container_info.as_mut());
                if let Some(info) = info {
                    info.stats_history
                        .push((stats.cpu_percent, stats.memory_mb));
                    if info.container_name.is_empty() {
                        info.container_name = stats.name.clone();
                    }
                    info.latest_stats = Some(stats);
                }
            }
        }

        // Dispatch a new stats poll every ~3 seconds for tabs with active containers.
        // Uses spawn_blocking because stats() runs blocking Docker/container
        // CLI commands that must not occupy the async worker thread pool.
        //
        // When the container name is known, we call stats() directly (1 Docker
        // command) instead of list_running_all() + find + stats (4 commands).
        // Falls back to listing only when the name isn't set yet.
        if self.last_stats_poll.elapsed() >= std::time::Duration::from_secs(3) {
            self.last_stats_poll = std::time::Instant::now();
            for (i, tab) in self.tabs.iter().enumerate() {
                if !matches!(
                    tab.execution_phase,
                    crate::frontend::tui::tabs::ExecutionPhase::Running { .. }
                ) {
                    continue;
                }

                for (step_name, target) in stats_poll_targets(tab) {
                    let container_name = match target {
                        StatsPollTarget::Named(name) => name,
                        StatsPollTarget::FirstRunning => String::new(),
                    };
                    let tab_idx = i;
                    // One query per slot at a time; a still-running one keeps
                    // its slot's turn.
                    let key = (tab_idx, step_name.clone());
                    match self.in_flight_stats.lock() {
                        Ok(mut guard) => {
                            if !guard.insert(key.clone()) {
                                continue;
                            }
                        }
                        Err(_) => continue,
                    }
                    let runtime = self.engines.runtime.clone();
                    let tx = self.stats_tx.clone();
                    let in_flight = self.in_flight_stats.clone();
                    self.runtime_handle.spawn_blocking(move || {
                        if !container_name.is_empty() {
                            // Fast path: name is known, query stats directly.
                            let handle = crate::data::session::AgentHandle {
                                id: container_name.clone(),
                                name: container_name,
                                image_tag: String::new(),
                                started_at: chrono::Utc::now(),
                            };
                            if let Ok(stats) = runtime.stats(&handle) {
                                let _ = tx.send((tab_idx, step_name, stats));
                            }
                        } else {
                            // Slow path: name unknown, list containers and
                            // pick the first.
                            if let Ok(handles) = runtime.list_running_all() {
                                if let Some(handle) = handles.first() {
                                    if let Ok(stats) = runtime.stats(handle) {
                                        let _ = tx.send((tab_idx, step_name, stats));
                                    }
                                }
                            }
                        }
                        if let Ok(mut guard) = in_flight.lock() {
                            guard.remove(&key);
                        }
                    });
                }
            }
        }

        // Engine-driven yolo countdown: the engine sets yolo_state via the
        // frontend trait; the TUI renders it as a non-modal overlay dialog.
        //
        // During a parallel group (`dormant_slots` non-empty) yolo countdowns
        // are per-slot: only the slot actually rendered maximized gets the
        // modal here — other yoloing slots show their countdown in the
        // minimized bar instead (`render_one_minimized_bar`). Rotating the
        // focus with Ctrl-S onto/off of a yoloing slot naturally opens/closes
        // the modal on the next tick since this is recomputed fresh each time.
        let active_tab = &self.tabs[active];
        let yolo_snapshot = if active_tab.dormant_slots.is_empty() {
            active_tab.yolo_state.lock().ok().and_then(|g| g.clone())
        } else if active_tab.container_overlay_active() {
            active_tab
                .focused_slot()
                .and_then(|s| s.yolo_state.lock().ok().and_then(|g| g.clone()))
        } else {
            None
        };
        if let Some(state) = yolo_snapshot {
            if !self.command_dialog_active {
                self.active_dialog = Some(Dialog::WorkflowYoloCountdown(
                    crate::frontend::tui::dialogs::WorkflowYoloCountdownState {
                        step_name: state.step_name.clone(),
                        remaining_secs: state.remaining_secs,
                    },
                ));
            }
        } else if matches!(self.active_dialog, Some(Dialog::WorkflowYoloCountdown(_))) {
            self.active_dialog = None;
        }

        // WI 0102: keep the amie condition-detail modal live from the active
        // tab's snapshot, matching on the condition name it was opened for.
        if matches!(self.active_dialog, Some(Dialog::AmieConditionDetail(_))) {
            let snapshot = self.tabs[active]
                .amie
                .as_ref()
                .and_then(|state| state.snapshot.lock().ok().map(|g| g.clone()));
            if let (Some(Dialog::AmieConditionDetail(detail)), Some(snapshot)) =
                (&mut self.active_dialog, snapshot)
            {
                if let Some(condition) = snapshot.conditions.iter().find(|c| c.name == detail.name)
                {
                    detail.condition = condition.clone();
                }
                detail.runs = snapshot.runs.clone();
            }
        }

        // WI 0102: while an attach session owns the amie tab and the daemon is
        // unreachable, surface the frozen-overview indicator (the attached
        // container slots keep streaming from their direct runtime connections).
        if let Some(state) = self.tabs[active].amie.as_ref() {
            if state.attached_condition.is_some()
                && !state
                    .daemon_reachable
                    .load(std::sync::atomic::Ordering::Relaxed)
            {
                self.status_bar.text =
                    "amie daemon not reachable — workflow overview frozen; attached containers still live"
                        .to_string();
            }
        }
    }

    /// Check the active tab's dialog_request_rx and open the corresponding
    /// dialog in the App.
    pub fn poll_dialog_requests(&mut self) {
        let request = {
            let tab = &self.tabs[self.active_tab];
            tab.dialog_request_rx
                .as_ref()
                .and_then(|rx| rx.try_recv().ok())
        };

        if let Some(request) = request {
            let dialog = match request {
                DialogRequest::YesNo { title, body } => Dialog::YesNo { title, body },
                DialogRequest::YesNoCancel { title, body } => Dialog::YesNoCancel { title, body },
                DialogRequest::TextInput {
                    title,
                    prompt,
                    default_text,
                } => {
                    let mut editor = TextEdit::new(false);
                    if let Some(text) = default_text {
                        editor.set_text(&text);
                    }
                    Dialog::TextInput {
                        title,
                        prompt,
                        editor,
                    }
                }
                DialogRequest::MultilineInput { title, prompt } => Dialog::MultilineInput {
                    title,
                    prompt,
                    editor: TextEdit::new(true),
                },
                DialogRequest::ListPicker { title, items } => Dialog::ListPicker {
                    title,
                    items,
                    selected: 0,
                },
                DialogRequest::KindSelect { title, options } => {
                    Dialog::KindSelect { title, options }
                }
                DialogRequest::WorkflowControlBoard(state) => Dialog::WorkflowControlBoard(state),
                DialogRequest::WorkflowStepError(state) => Dialog::WorkflowStepError(state),
                DialogRequest::WorkflowYoloCountdown(state) => Dialog::WorkflowYoloCountdown(state),
                DialogRequest::WorkflowStepConfirm(state) => Dialog::WorkflowStepConfirm(state),
                DialogRequest::AgentSetup(state) => Dialog::AgentSetup(state),
                DialogRequest::MountScope(state) => Dialog::MountScope(state),
                DialogRequest::AgentAuth(state) => Dialog::AgentAuth(state),
                DialogRequest::QuitConfirm => Dialog::QuitConfirm,
                DialogRequest::CloseTabConfirm => Dialog::CloseTabConfirm,
                DialogRequest::WorkflowCancelConfirm => Dialog::WorkflowCancelConfirm,
                DialogRequest::ConfigShow {
                    rows,
                    selected,
                    rejected,
                } => {
                    let selected = selected.min(rows.len().saturating_sub(1));
                    let mut state = crate::frontend::tui::dialogs::ConfigShowState {
                        rows,
                        selected,
                        editing: false,
                        edit_column: 0,
                        editor: TextEdit::new(false),
                        new_entry: None,
                        error: None,
                    };
                    // A rejected edit reopens in edit mode with the typed
                    // value preserved and the reason displayed, so the user
                    // corrects it instead of retyping from scratch.
                    if let Some(rej) = rejected {
                        let has_row = state.rows.iter().any(|r| r.field == rej.field);
                        let new_mapping_key = if has_row {
                            None
                        } else {
                            rej.field
                                .strip_prefix("dynamicWorkflows.agentsToModels.")
                                .map(str::to_string)
                        };
                        if has_row || new_mapping_key.is_some() {
                            state.error = Some(rej.reason);
                            state.editing = true;
                            state.edit_column = if rej.global { 0 } else { 1 };
                            state.editor.set_text(&rej.value);
                            // A rejected Ctrl+N mapping has no row yet —
                            // resume the add flow in its value phase.
                            state.new_entry = new_mapping_key.map(|key| {
                                crate::frontend::tui::dialogs::NewMapEntryPhase::Value { key }
                            });
                        }
                    }
                    Dialog::ConfigShow(state)
                }
                DialogRequest::Loading { title } => Dialog::Loading { title },
                DialogRequest::Custom { title, body, keys } => Dialog::Custom { title, body, keys },
            };
            self.active_dialog = Some(dialog);
            self.command_dialog_active = true;
        }
    }

    /// Send a dialog response through the active tab's dialog_response_tx.
    pub fn send_dialog_response(&mut self, response: DialogResponse) {
        let tab = &self.tabs[self.active_tab];
        if let Some(ref tx) = tab.dialog_response_tx {
            let _ = tx.send(response);
        }
    }

    pub fn update_suggestions(&mut self) {
        let partial = self.command_input.text.as_str();
        if partial.is_empty() {
            self.suggestion_row.clear();
            return;
        }
        let completions = self.catalogue.tui_completions(partial);
        self.suggestion_row = completions.into_iter().map(|c| c.completion).collect();
    }
}

/// How one container slot's stats are obtained this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StatsPollTarget {
    /// The engine published this slot's container name — query it directly.
    Named(String),
    /// The name hasn't arrived yet; ask the runtime for the first running
    /// container instead.
    FirstRunning,
}

/// Which of a tab's container slots to poll for stats this tick, keyed by the
/// slot's step name (empty for the single/backbone slot of a plain command)
/// so the drain can route each sample back to the slot it was polled for.
///
/// Every slot whose container name is known is polled — during a parallel
/// group that means all N containers, not just the focused one.
///
/// The "first running container" fallback covers the brief window before a
/// plain `chat`/`exec` container reports its name. It is deliberately limited
/// to a lone slot outside a parallel group: with several containers live,
/// "the first running container" is an arbitrary sibling, and because the
/// drain adopts a sample's name for a still-unnamed slot, one guess would pin
/// that slot to a sibling's stats for the rest of the run. A group slot with
/// no name yet simply waits for its `ContainerSlotEvent::ContainerName`.
pub(crate) fn stats_poll_targets(tab: &Tab) -> Vec<(String, StatsPollTarget)> {
    let in_parallel_group = !tab.dormant_slots.is_empty();
    let sole_slot = tab.container_slots.len() == 1 && !in_parallel_group;
    let mut targets = Vec::with_capacity(tab.container_slots.len());
    for slot in &tab.container_slots {
        let Some(info) = slot.container_info.as_ref() else {
            continue;
        };
        if !info.container_name.is_empty() {
            targets.push((
                slot.step_name.clone(),
                StatsPollTarget::Named(info.container_name.clone()),
            ));
        } else if sole_slot {
            targets.push((slot.step_name.clone(), StatsPollTarget::FirstRunning));
        }
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    use crate::command::dispatch::catalogue::CommandCatalogue;
    use crate::data::session::{Session, SessionOpenOptions, StaticGitRootResolver};
    use crate::data::session_manager::SessionManager;
    use crate::frontend::tui::tabs::Tab;

    fn make_test_session() -> Session {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = StaticGitRootResolver::new(tmp.path());
        Session::open(
            tmp.path().to_path_buf(),
            &resolver,
            SessionOpenOptions::default(),
        )
        .unwrap()
    }

    fn make_engines() -> crate::command::dispatch::Engines {
        let runtime = Arc::new(crate::engine::container::ContainerRuntime::docker());
        let overlay = Arc::new(crate::engine::overlay::OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(std::path::PathBuf::from(
                "/tmp",
            )),
        ));
        let git_engine = Arc::new(crate::engine::git::GitEngine::new());
        let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            overlay.clone(),
            runtime.clone(),
        ));
        let auth_engine = Arc::new(crate::engine::auth::AuthEngine::with_paths(
            crate::data::fs::auth_paths::AuthPathResolver::at_home("/tmp"),
            crate::data::fs::api_paths::ApiPaths::at_root("/tmp"),
        ));
        let workflow_state_store = {
            let tmp = tempfile::tempdir().unwrap();
            Arc::new(crate::data::EngineWorkflowStateStore::at_git_root(
                tmp.path(),
            ))
        };
        crate::command::dispatch::Engines {
            runtime: runtime.clone(),
            container_runtime: Some(runtime),
            sandbox_runtime: None,
            git_engine,
            overlay_engine: overlay,
            auth_engine,
            agent_engine,
            workflow_state_store,
        }
    }

    fn make_app() -> App {
        let rt = Box::leak(Box::new(tokio::runtime::Runtime::new().unwrap()));
        let catalogue = CommandCatalogue::get();
        let engines = make_engines();
        let session_manager = Arc::new(RwLock::new(SessionManager::in_memory()));
        let session = make_test_session();
        let tab = Tab::new(session);
        App::new(
            catalogue,
            engines,
            session_manager,
            tab,
            rt.handle().clone(),
        )
    }

    // ── agent_name_from_parsed ───────────────────────────────────────────────

    fn make_parsed(flags: Vec<(&str, FlagValue)>) -> ParsedCommandBoxInput {
        ParsedCommandBoxInput {
            path: vec!["chat".to_string()],
            flags: flags.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            arguments: std::collections::BTreeMap::new(),
        }
    }

    use crate::command::dispatch::parsed_input::FlagValue;

    #[test]
    fn agent_name_uses_explicit_agent_flag() {
        let session = make_test_session();
        let parsed = make_parsed(vec![("agent", FlagValue::String("codex".into()))]);
        assert_eq!(agent_name_from_parsed(&parsed, &session), "codex");
    }

    #[test]
    fn agent_name_uses_session_default_agent_when_flag_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = StaticGitRootResolver::new(tmp.path());
        let session = Session::open(
            tmp.path().to_path_buf(),
            &resolver,
            SessionOpenOptions {
                flags: crate::data::config::FlagConfig {
                    agent: Some("codex".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        let parsed = make_parsed(vec![]);
        assert_eq!(
            agent_name_from_parsed(&parsed, &session),
            "codex",
            "title must reflect the configured default agent, not a hardcoded name"
        );
    }

    #[test]
    fn agent_name_falls_back_to_claude_without_config() {
        let session = make_test_session();
        let parsed = make_parsed(vec![]);
        assert_eq!(agent_name_from_parsed(&parsed, &session), "claude");
    }

    // ── update_suggestions ────────────────────────────────────────────────────

    #[test]
    fn update_suggestions_empty_input_clears_suggestions() {
        let mut app = make_app();
        app.suggestion_row = vec!["chat".to_string()];
        app.command_input.set_text("");
        app.command_input.text.clear();
        app.update_suggestions();
        assert!(
            app.suggestion_row.is_empty(),
            "empty input must clear suggestions"
        );
    }

    #[test]
    fn update_suggestions_partial_match_populates_suggestions() {
        let mut app = make_app();
        app.command_input.set_text("cha");
        app.update_suggestions();
        assert!(
            app.suggestion_row.iter().any(|s| s == "chat"),
            "'cha' must suggest 'chat'; got: {:?}",
            app.suggestion_row
        );
    }

    #[test]
    fn update_suggestions_no_match_yields_empty() {
        let mut app = make_app();
        app.command_input.set_text("zzzzzzz");
        app.update_suggestions();
        assert!(app.suggestion_row.is_empty());
    }

    // ── tab switching ─────────────────────────────────────────────────────────

    #[test]
    fn switch_to_next_tab_wraps_around() {
        let mut app = make_app();
        app.tabs.push(Tab::new(make_test_session()));
        app.active_tab = 1;
        app.switch_to_next_tab();
        assert_eq!(app.active_tab, 0, "next tab from last must wrap to first");
    }

    #[test]
    fn switch_to_prev_tab_wraps_around() {
        let mut app = make_app();
        app.tabs.push(Tab::new(make_test_session()));
        app.active_tab = 0;
        app.switch_to_prev_tab();
        assert_eq!(app.active_tab, 1, "prev tab from first must wrap to last");
    }

    #[test]
    fn switch_to_next_advances_index() {
        let mut app = make_app();
        app.tabs.push(Tab::new(make_test_session()));
        app.active_tab = 0;
        app.switch_to_next_tab();
        assert_eq!(app.active_tab, 1);
    }

    #[test]
    fn close_active_tab_with_single_tab_sets_should_quit() {
        let mut app = make_app();
        assert_eq!(app.tabs.len(), 1);
        app.close_active_tab();
        assert!(app.should_quit);
    }

    #[test]
    fn close_active_tab_with_multiple_tabs_removes_tab() {
        let mut app = make_app();
        app.tabs.push(Tab::new(make_test_session()));
        assert_eq!(app.tabs.len(), 2);
        app.close_active_tab();
        assert_eq!(app.tabs.len(), 1);
        assert!(!app.should_quit);
    }

    // ── stats drain pipeline ─────────────────────────────────────────────

    #[test]
    fn stats_drain_populates_latest_stats() {
        let mut app = make_app();
        let tab = app.active_tab_mut();
        tab.execution_phase = crate::frontend::tui::tabs::ExecutionPhase::Running {
            command: "chat".into(),
        };
        // The command's single slot, as spawn_command installs it.
        tab.start_container("Claude".into(), "awman-test-1234".into(), 80, 24);
        assert!(tab
            .focused_slot()
            .and_then(|s| s.container_info.as_ref())
            .unwrap()
            .latest_stats
            .is_none());

        // Simulate a stats result arriving on the channel (empty step name =
        // the single/backbone slot).
        let stats = crate::engine::agent_runtime::execution::AgentStats {
            name: "awman-test-1234".into(),
            cpu_percent: 42.5,
            memory_mb: 256.0,
        };
        app.stats_tx.send((0, String::new(), stats)).unwrap();

        // tick_all_tabs drains the channel.
        app.tick_all_tabs();

        let tab = app.active_tab();
        let info = tab
            .focused_slot()
            .and_then(|s| s.container_info.as_ref())
            .unwrap();
        assert!(
            info.latest_stats.is_some(),
            "latest_stats must be populated after drain"
        );
        let s = info.latest_stats.as_ref().unwrap();
        assert_eq!(s.cpu_percent, 42.5);
        assert_eq!(s.memory_mb, 256.0);
        assert_eq!(s.name, "awman-test-1234");
        assert_eq!(info.stats_history.len(), 1);
    }

    #[test]
    fn stats_drain_routes_parallel_samples_to_the_right_slot() {
        use crate::frontend::tui::tabs::ContainerSlot;

        let mut app = make_app();
        let tab = app.active_tab_mut();
        tab.execution_phase = crate::frontend::tui::tabs::ExecutionPhase::Running {
            command: "exec workflow".into(),
        };
        tab.container_slots
            .push(ContainerSlot::new("build".into(), "claude".into(), 1000));
        tab.container_slots
            .push(ContainerSlot::new("test".into(), "codex".into(), 1000));

        let stats = crate::engine::agent_runtime::execution::AgentStats {
            name: "awman-test-1".into(),
            cpu_percent: 12.5,
            memory_mb: 64.0,
        };
        app.stats_tx.send((0, "test".into(), stats)).unwrap();
        app.tick_all_tabs();

        let tab = app.active_tab();
        let test_info = tab.container_slots[1].container_info.as_ref().unwrap();
        assert_eq!(
            test_info.latest_stats.as_ref().map(|s| s.cpu_percent),
            Some(12.5),
            "the sample must land on the slot it was polled for"
        );
        let build_info = tab.container_slots[0].container_info.as_ref().unwrap();
        assert!(
            build_info.latest_stats.is_none(),
            "the other slot must be untouched"
        );
    }

    // ── stats poll targets ───────────────────────────────────────────────

    fn named_slot(step: &str, container: &str) -> crate::frontend::tui::tabs::ContainerSlot {
        let mut slot =
            crate::frontend::tui::tabs::ContainerSlot::new(step.into(), "claude".into(), 1000);
        if let Some(info) = slot.container_info.as_mut() {
            info.container_name = container.into();
        }
        slot
    }

    #[test]
    fn stats_poll_targets_covers_every_named_slot_in_a_parallel_group() {
        let mut tab = Tab::new(make_test_session());
        // A parallel group: the sequential backbone is dormant and every
        // group slot has had its container name published by the engine.
        tab.dormant_slots
            .push(crate::frontend::tui::tabs::ContainerSlot::new(
                String::new(),
                "claude".into(),
                1000,
            ));
        tab.container_slots.push(named_slot("build", "awman-b-1"));
        tab.container_slots.push(named_slot("test", "awman-t-2"));
        tab.container_slots.push(named_slot("docs", "awman-d-3"));

        assert_eq!(
            stats_poll_targets(&tab),
            vec![
                ("build".into(), StatsPollTarget::Named("awman-b-1".into())),
                ("test".into(), StatsPollTarget::Named("awman-t-2".into())),
                ("docs".into(), StatsPollTarget::Named("awman-d-3".into())),
            ],
            "every container in a parallel group must be polled, not just the focused one"
        );
    }

    #[test]
    fn stats_poll_targets_never_guesses_a_container_inside_a_parallel_group() {
        let mut tab = Tab::new(make_test_session());
        tab.dormant_slots
            .push(crate::frontend::tui::tabs::ContainerSlot::new(
                String::new(),
                "claude".into(),
                1000,
            ));
        // The group is down to one slot and its name hasn't arrived yet.
        tab.container_slots
            .push(crate::frontend::tui::tabs::ContainerSlot::new(
                "build".into(),
                "claude".into(),
                1000,
            ));

        assert!(
            stats_poll_targets(&tab).is_empty(),
            "a group slot with no name yet must wait for it rather than adopting \
             an arbitrary running container's stats"
        );
    }

    #[test]
    fn stats_poll_targets_falls_back_for_a_lone_unnamed_container() {
        let mut tab = Tab::new(make_test_session());
        tab.start_container("Claude".into(), String::new(), 80, 24);

        assert_eq!(
            stats_poll_targets(&tab),
            vec![(String::new(), StatsPollTarget::FirstRunning)],
            "a plain command's slot still gets stats before its name arrives"
        );
    }

    #[test]
    fn tick_all_tabs_shows_yolo_modal_for_focused_slot_in_parallel_group() {
        use crate::frontend::tui::tabs::{ContainerSlot, ContainerWindowState, YoloState};

        let mut app = make_app();
        let tab = app.active_tab_mut();
        // A parallel group is active: the backbone is stashed in
        // `dormant_slots` and the group's own slots are in `container_slots`.
        tab.dormant_slots
            .push(ContainerSlot::new(String::new(), "claude".into(), 1000));
        tab.container_slots
            .push(ContainerSlot::new("build".into(), "claude".into(), 1000));
        tab.container_slots
            .push(ContainerSlot::new("test".into(), "codex".into(), 1000));
        tab.focused_slot_idx = 1; // "test" is the maximized slot
        tab.container_window_state = ContainerWindowState::Maximized;
        *tab.container_slots[1].yolo_state.lock().unwrap() = Some(YoloState {
            step_name: "test".into(),
            remaining_secs: 12,
        });

        app.tick_all_tabs();

        match &app.active_dialog {
            Some(Dialog::WorkflowYoloCountdown(state)) => {
                assert_eq!(state.step_name, "test");
                assert_eq!(state.remaining_secs, 12);
            }
            _ => panic!("expected WorkflowYoloCountdown for the focused slot"),
        }
    }

    #[test]
    fn tick_all_tabs_hides_yolo_modal_when_yoloing_slot_is_not_focused() {
        use crate::frontend::tui::tabs::{ContainerSlot, ContainerWindowState, YoloState};

        let mut app = make_app();
        let tab = app.active_tab_mut();
        tab.dormant_slots
            .push(ContainerSlot::new(String::new(), "claude".into(), 1000));
        tab.container_slots
            .push(ContainerSlot::new("build".into(), "claude".into(), 1000));
        tab.container_slots
            .push(ContainerSlot::new("test".into(), "codex".into(), 1000));
        tab.focused_slot_idx = 0; // "build" is maximized; "test" is a bar
        tab.container_window_state = ContainerWindowState::Maximized;
        *tab.container_slots[1].yolo_state.lock().unwrap() = Some(YoloState {
            step_name: "test".into(),
            remaining_secs: 12,
        });

        app.tick_all_tabs();

        assert!(
            app.active_dialog.is_none(),
            "the countdown for a non-focused slot belongs in its minimized bar, not a modal"
        );
    }

    #[test]
    fn tick_all_tabs_hides_yolo_modal_when_group_is_minimized() {
        use crate::frontend::tui::tabs::{ContainerSlot, ContainerWindowState, YoloState};

        let mut app = make_app();
        let tab = app.active_tab_mut();
        tab.dormant_slots
            .push(ContainerSlot::new(String::new(), "claude".into(), 1000));
        tab.container_slots
            .push(ContainerSlot::new("test".into(), "codex".into(), 1000));
        tab.focused_slot_idx = 0;
        // Nothing is rendered maximized while Minimized — every slot is a bar.
        tab.container_window_state = ContainerWindowState::Minimized;
        *tab.container_slots[0].yolo_state.lock().unwrap() = Some(YoloState {
            step_name: "test".into(),
            remaining_secs: 12,
        });

        app.tick_all_tabs();

        assert!(
            app.active_dialog.is_none(),
            "no slot is maximized, so no modal should show"
        );
    }

    #[test]
    fn container_name_picked_up_from_shared_state() {
        let mut app = make_app();
        let tab = app.active_tab_mut();
        tab.execution_phase = crate::frontend::tui::tabs::ExecutionPhase::Running {
            command: "chat".into(),
        };
        tab.start_container("Claude".into(), String::new(), 80, 24);

        // Simulate the engine reporting the container name.
        if let Ok(mut guard) = tab.container_name_shared.lock() {
            *guard = Some("awman-new-container".into());
        }

        app.tick_all_tabs();

        let info = app
            .active_tab()
            .focused_slot()
            .and_then(|s| s.container_info.as_ref())
            .unwrap();
        assert_eq!(info.container_name, "awman-new-container");
    }

    #[test]
    fn new_container_name_overwrites_old_and_clears_stats() {
        let mut app = make_app();
        let tab = app.active_tab_mut();
        tab.execution_phase = crate::frontend::tui::tabs::ExecutionPhase::Running {
            command: "exec workflow".into(),
        };
        tab.start_container("Claude".into(), "awman-old-container".into(), 80, 24);
        if let Some(info) = tab
            .focused_slot_mut()
            .and_then(|s| s.container_info.as_mut())
        {
            info.latest_stats = Some(crate::engine::agent_runtime::execution::AgentStats {
                name: "awman-old-container".into(),
                cpu_percent: 10.0,
                memory_mb: 100.0,
            });
            info.stats_history = vec![(10.0, 100.0)];
        }

        // Simulate a workflow step transition reporting a new container name.
        if let Ok(mut guard) = tab.container_name_shared.lock() {
            *guard = Some("awman-step2-container".into());
        }

        app.tick_all_tabs();

        let info = app
            .active_tab()
            .focused_slot()
            .and_then(|s| s.container_info.as_ref())
            .unwrap();
        assert_eq!(info.container_name, "awman-step2-container");
        assert!(
            info.latest_stats.is_none(),
            "latest_stats must be cleared when a new container name arrives"
        );
    }

    #[test]
    fn stats_title_shows_values_when_stats_present() {
        use crate::engine::agent_runtime::execution::AgentStats;
        use crate::frontend::tui::tabs::ContainerInfo;

        let info = ContainerInfo {
            agent_display_name: "Claude".into(),
            container_name: "awman-test".into(),
            start_time: std::time::Instant::now(),
            latest_stats: Some(AgentStats {
                name: "awman-test".into(),
                cpu_percent: 42.5,
                memory_mb: 256.0,
            }),
            stats_history: Vec::new(),
            sandboxed: false,
        };

        let title = crate::frontend::tui::container_view::build_stats_title_from_info(&info);
        assert!(title.contains("42.5%"), "title must contain CPU: {title}");
        assert!(
            title.contains("256MiB"),
            "title must contain memory: {title}"
        );
        assert!(
            title.contains("awman-test"),
            "title must contain name: {title}"
        );
    }

    // ── ConfigShow dialog request: rejected-edit reopen ──────────────────

    fn config_show_row(field: &str) -> crate::frontend::tui::dialogs::ConfigShowRow {
        crate::frontend::tui::dialogs::ConfigShowRow {
            field: field.to_string(),
            global: String::new(),
            repo: String::new(),
            effective: String::new(),
            read_only: false,
            global_writable: false,
            repo_writable: true,
            value_hint: None,
        }
    }

    fn deliver_config_show(
        app: &mut App,
        rows: Vec<crate::frontend::tui::dialogs::ConfigShowRow>,
        rejected: Option<crate::frontend::tui::dialogs::ConfigShowRejectedEdit>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        app.tabs[0].dialog_request_rx = Some(rx);
        tx.send(DialogRequest::ConfigShow {
            rows,
            selected: 0,
            rejected,
        })
        .unwrap();
        app.poll_dialog_requests();
    }

    #[test]
    fn config_show_request_without_rejection_opens_in_browse_mode() {
        let mut app = make_app();
        deliver_config_show(&mut app, vec![config_show_row("agent")], None);

        let Some(Dialog::ConfigShow(state)) = &app.active_dialog else {
            panic!("ConfigShow request must open the dialog");
        };
        assert!(!state.editing);
        assert_eq!(state.error, None);
    }

    #[test]
    fn config_show_request_with_rejection_reopens_edit_with_input_preserved() {
        let mut app = make_app();
        deliver_config_show(
            &mut app,
            vec![config_show_row("dynamicWorkflows.defaultLeader")],
            Some(crate::frontend::tui::dialogs::ConfigShowRejectedEdit {
                field: "dynamicWorkflows.defaultLeader".into(),
                value: "claude".into(),
                global: false,
                reason: "expected agent::model".into(),
            }),
        );

        let Some(Dialog::ConfigShow(state)) = &app.active_dialog else {
            panic!("ConfigShow request must open the dialog");
        };
        assert!(
            state.editing,
            "a rejected edit must reopen in edit mode so the input is not lost"
        );
        assert_eq!(state.edit_column, 1, "repo-scoped edit stays on Repo");
        assert_eq!(
            state.editor.text, "claude",
            "the rejected input must be preserved for correction"
        );
        assert_eq!(state.error.as_deref(), Some("expected agent::model"));
    }

    #[test]
    fn config_show_request_with_rejected_new_mapping_resumes_value_phase() {
        // A rejected Ctrl+N mapping has no table row yet; the dialog must
        // resume the add flow in its value phase for the same key.
        let mut app = make_app();
        deliver_config_show(
            &mut app,
            vec![config_show_row("agent")],
            Some(crate::frontend::tui::dialogs::ConfigShowRejectedEdit {
                field: "dynamicWorkflows.agentsToModels.maki".into(),
                value: "model-a, model-b".into(),
                global: false,
                reason: "could not write config".into(),
            }),
        );

        let Some(Dialog::ConfigShow(state)) = &app.active_dialog else {
            panic!("ConfigShow request must open the dialog");
        };
        assert_eq!(
            state.new_entry,
            Some(crate::frontend::tui::dialogs::NewMapEntryPhase::Value { key: "maki".into() }),
            "the add-mapping flow must resume in the value phase"
        );
        assert!(state.editing);
        assert_eq!(state.editor.text, "model-a, model-b");
        assert_eq!(state.error.as_deref(), Some("could not write config"));
    }
}
