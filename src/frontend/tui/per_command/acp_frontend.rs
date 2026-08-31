//! The TUI [`AcpFrontend`] implementation.
//!
//! Layer 3 presentation glue between an [`AcpSession`](crate::engine::acp::AcpSession)
//! (running on the engine's async task) and the TUI. It owns no business
//! logic: it appends rendered updates to a slot's shared state, opens a
//! permission modal through the existing dialog framework, and blocks for the
//! next prompt typed into the command box.
//!
//! Threading mirrors `TuiCommandFrontend`: the dialog and prompt channels use
//! `std::sync::mpsc`, so the blocking `recv()` in `request_permission` /
//! `next_prompt` parks the OS thread the command runs on rather than stalling
//! a tokio worker — the `AcpFrontend` trait methods are synchronous, so this
//! is the correct blocking strategy.

use std::sync::Mutex;

use crate::data::message::UserMessageSink;
use crate::engine::acp::{AcpFrontend, PermissionDecision, PermissionRequest, SessionUpdate};
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};
use crate::frontend::tui::tabs::SharedAcpState;

/// Sender end of the command-box → session prompt channel. `Some(text)`
/// delivers the next prompt; `None` asks the session to cancel (matching the
/// `AcpFrontend::next_prompt` contract, where `None` triggers `session/cancel`).
pub type AcpPromptSender = std::sync::mpsc::Sender<Option<String>>;
/// Receiver end held by [`TuiAcpFrontend`].
pub type AcpPromptReceiver = std::sync::mpsc::Receiver<Option<String>>;

/// TUI-side [`AcpFrontend`]. Constructed by the command-wiring layer, which
/// shares `state` with the ACP [`ContainerSlot`](crate::frontend::tui::tabs::ContainerSlot)
/// and feeds `prompt_rx` from the command box while an ACP window is focused.
pub struct TuiAcpFrontend {
    /// Shared render state for the ACP window this session drives. Updates are
    /// appended here; the renderer reads it every frame.
    state: SharedAcpState,
    /// Dialog request channel — same mechanism `TuiCommandFrontend::ask_dialog`
    /// uses to open modals from a command thread.
    dialog_tx: std::sync::mpsc::Sender<DialogRequest>,
    dialog_rx: Mutex<std::sync::mpsc::Receiver<DialogResponse>>,
    /// Next-prompt source: the command box routes submitted text here instead
    /// of `Dispatch` while an ACP window is focused.
    prompt_rx: AcpPromptReceiver,
}

impl TuiAcpFrontend {
    pub fn new(
        state: SharedAcpState,
        dialog_tx: std::sync::mpsc::Sender<DialogRequest>,
        dialog_rx: std::sync::mpsc::Receiver<DialogResponse>,
        prompt_rx: AcpPromptReceiver,
    ) -> Self {
        Self {
            state,
            dialog_tx,
            dialog_rx: Mutex::new(dialog_rx),
            prompt_rx,
        }
    }

    /// Send a dialog request and block for the response. Mirrors
    /// `TuiCommandFrontend::ask_dialog`; a closed channel maps to `Dismissed`.
    fn ask_dialog(&self, request: DialogRequest) -> DialogResponse {
        if self.dialog_tx.send(request).is_err() {
            return DialogResponse::Dismissed;
        }
        match self.dialog_rx.lock() {
            Ok(rx) => rx.recv().unwrap_or(DialogResponse::Dismissed),
            Err(_) => DialogResponse::Dismissed,
        }
    }
}

impl AcpFrontend for TuiAcpFrontend {
    fn render_update(&mut self, update: SessionUpdate) {
        // Append to the focused ACP window's history; the event loop repaints
        // every tick, so no explicit redraw signal is needed.
        if let Ok(mut state) = self.state.lock() {
            state.push_update(update);
        }
    }

    fn request_permission(&mut self, request: PermissionRequest) -> PermissionDecision {
        // Each option becomes a numbered hotkey in a `Custom` modal. Keep a
        // parallel char → option_id map so the response resolves back to the
        // ACP option id the engine needs.
        let mut keys: Vec<(char, String)> = Vec::new();
        let mut char_to_id: Vec<(char, String)> = Vec::new();
        for (i, opt) in request.options.iter().enumerate().take(9) {
            let ch = char::from_digit((i + 1) as u32, 10).unwrap_or('1');
            keys.push((ch, opt.name.clone()));
            char_to_id.push((ch, opt.option_id.clone()));
        }

        let tool = request
            .tool_call
            .title
            .clone()
            .unwrap_or_else(|| "(tool call)".to_string());
        let kind = request
            .tool_call
            .kind
            .as_deref()
            .map(|k| format!(" ({k})"))
            .unwrap_or_default();
        let body = format!("The agent wants to run:\n\n  {tool}{kind}\n\nAllow this action?");

        // Park the request on the slot so the window can show a pending hint
        // while the modal is open; clear it once resolved.
        if let Ok(mut state) = self.state.lock() {
            state.pending_permission = Some(request.clone());
        }

        let response = self.ask_dialog(DialogRequest::Custom {
            title: "ACP Permission Request".to_string(),
            body,
            keys,
        });

        if let Ok(mut state) = self.state.lock() {
            state.pending_permission = None;
        }

        match response {
            DialogResponse::Char(c) => char_to_id
                .into_iter()
                .find(|(ch, _)| *ch == c)
                .map(|(_, option_id)| PermissionDecision::Selected { option_id })
                .unwrap_or(PermissionDecision::Cancelled),
            _ => PermissionDecision::Cancelled,
        }
    }

    fn next_prompt(&mut self) -> Option<String> {
        // Block until the command box submits a prompt for this session. A
        // closed channel (command box gone) or an explicit `None` both end the
        // session, which the driver turns into `session/cancel`.
        self.prompt_rx.recv().unwrap_or(None)
    }
}

// The command frontend is also the portable ACP boundary for single-command
// launches.  The richer `TuiAcpFrontend` above is used by slot-aware wiring;
// until a command has installed its slot, this fallback renders updates as
// plain summary lines (never raw JSON) and — crucially — FAILS CLOSED on
// permission requests. `AcpSession` only calls `request_permission` when
// neither `--yolo` nor `--auto` is active, so a request reaching this stub is
// by definition a run the user did NOT ask to auto-approve; approving it here
// would silently grant a tool call with no prompt. Deny instead.
impl AcpFrontend for TuiCommandFrontend {
    fn render_update(&mut self, update: SessionUpdate) {
        self.messages
            .info(crate::engine::acp::protocol::summarize_update(&update));
    }

    fn request_permission(&mut self, _request: PermissionRequest) -> PermissionDecision {
        PermissionDecision::Cancelled
    }

    fn next_prompt(&mut self) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::acp::protocol::ContentChunk;
    use crate::engine::acp::{ContentBlock, PermissionOption, ToolCallUpdate};
    use std::sync::Arc;

    fn permission_request(options: Vec<PermissionOption>) -> PermissionRequest {
        PermissionRequest {
            request_id: Default::default(),
            session_id: "s1".into(),
            tool_call: ToolCallUpdate {
                tool_call_id: "t1".into(),
                kind: Some("edit".into()),
                status: None,
                title: Some("Write config.json".into()),
                content: None,
                locations: None,
                raw_input: None,
                raw_output: None,
            },
            options,
        }
    }

    #[test]
    fn render_update_appends_to_shared_state() {
        let state: SharedAcpState = Arc::new(Mutex::new(Default::default()));
        let (dtx, _drx_peer) = std::sync::mpsc::channel();
        let (_dtx_peer, drx) = std::sync::mpsc::channel();
        let (_ptx, prx) = std::sync::mpsc::channel();
        let mut frontend = TuiAcpFrontend::new(state.clone(), dtx, drx, prx);

        frontend.render_update(SessionUpdate::AgentMessageChunk {
            chunk: ContentChunk {
                content: ContentBlock::Text { text: "hi".into() },
                message_id: None,
            },
        });

        assert_eq!(state.lock().unwrap().history.len(), 1);
    }

    #[test]
    fn next_prompt_returns_submitted_text_then_none_on_close() {
        let state: SharedAcpState = Arc::new(Mutex::new(Default::default()));
        let (dtx, _drx_peer) = std::sync::mpsc::channel();
        let (_dtx_peer, drx) = std::sync::mpsc::channel();
        let (ptx, prx) = std::sync::mpsc::channel();
        let mut frontend = TuiAcpFrontend::new(state, dtx, drx, prx);

        ptx.send(Some("do the thing".to_string())).unwrap();
        assert_eq!(frontend.next_prompt(), Some("do the thing".to_string()));

        // Dropping the sender closes the channel → the session ends.
        drop(ptx);
        assert_eq!(frontend.next_prompt(), None);
    }

    #[test]
    fn request_permission_maps_choice_to_option_id() {
        let state: SharedAcpState = Arc::new(Mutex::new(Default::default()));
        let (dtx, drx_peer) = std::sync::mpsc::channel::<DialogRequest>();
        let (dtx_peer, drx) = std::sync::mpsc::channel::<DialogResponse>();
        let (_ptx, prx) = std::sync::mpsc::channel();
        let mut frontend = TuiAcpFrontend::new(state.clone(), dtx, drx, prx);

        // Drive the "event loop" side from another thread: read the request,
        // answer with the second option's hotkey ('2').
        let handle = std::thread::spawn(move || {
            let req = drx_peer.recv().unwrap();
            match req {
                DialogRequest::Custom { keys, .. } => {
                    // Two options → hotkeys '1' and '2'.
                    assert_eq!(keys.len(), 2);
                    dtx_peer.send(DialogResponse::Char('2')).unwrap();
                }
                other => panic!("expected Custom dialog, got {other:?}"),
            }
        });

        let decision = frontend.request_permission(permission_request(vec![
            PermissionOption {
                option_id: "opt-allow".into(),
                name: "Allow once".into(),
                kind: "allow_once".into(),
            },
            PermissionOption {
                option_id: "opt-reject".into(),
                name: "Reject".into(),
                kind: "reject_once".into(),
            },
        ]));
        handle.join().unwrap();

        assert_eq!(
            decision,
            PermissionDecision::Selected {
                option_id: "opt-reject".into()
            }
        );
        // The pending permission is cleared once the modal resolves.
        assert!(state.lock().unwrap().pending_permission.is_none());
    }

    #[test]
    fn request_permission_dismissed_is_cancelled() {
        let state: SharedAcpState = Arc::new(Mutex::new(Default::default()));
        let (dtx, drx_peer) = std::sync::mpsc::channel::<DialogRequest>();
        let (dtx_peer, drx) = std::sync::mpsc::channel::<DialogResponse>();
        let (_ptx, prx) = std::sync::mpsc::channel();
        let mut frontend = TuiAcpFrontend::new(state, dtx, drx, prx);

        let handle = std::thread::spawn(move || {
            let _ = drx_peer.recv().unwrap();
            dtx_peer.send(DialogResponse::Dismissed).unwrap();
        });

        let decision = frontend.request_permission(permission_request(vec![PermissionOption {
            option_id: "opt-allow".into(),
            name: "Allow".into(),
            kind: "allow_once".into(),
        }]));
        handle.join().unwrap();

        assert_eq!(decision, PermissionDecision::Cancelled);
    }
}
