//! ACP session lifecycle and portable frontend driver.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{mpsc, Mutex};

/// Deadline for the one-shot ACP handshake requests (`initialize`,
/// `session/new`). These must complete quickly; a peer that answers
/// `initialize` and then never answers `session/new` would otherwise hang awman
/// forever (stdout stays open, so the connection-close path never fires).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

use crate::data::message::UserMessageSink;
use crate::engine::acp::client::{AcpClient, AcpTransport, IncomingRequest};
use crate::engine::acp::frontend::AcpFrontend;
use crate::engine::acp::protocol::{
    ContentBlock, InitializeRequest, InitializeResponse, JsonRpcId, NewSessionRequest,
    NewSessionResponse, PermissionDecision, PermissionRequest, PromptRequest, PromptResponse,
    SessionUpdate,
};
use crate::engine::agent_runtime::execution::{AgentExecution, AgentExitInfo};
use crate::engine::container::options::{AutoMode, YoloMode};
use crate::engine::error::EngineError;

/// A running ACP agent plus its JSON-RPC client.
///
/// Construct the companion [`crate::engine::acp::AcpTransportFrontend`] with
/// `AcpTransport::channel`, pass it to `AgentInstance::run_with_frontend`, and
/// then build this session from the resulting execution and transport.
pub struct AcpSession {
    execution: AgentExecution,
    client: Arc<AcpClient>,
    session_id: Mutex<Option<String>>,
    permissions_rx: mpsc::UnboundedReceiver<IncomingRequest>,
    pending_permissions: Mutex<HashSet<JsonRpcId>>,
    updates_rx: mpsc::UnboundedReceiver<SessionUpdate>,
    auto_approve: bool,
}

impl AcpSession {
    pub fn from_transport(
        execution: AgentExecution,
        transport: AcpTransport,
        sink: Box<dyn UserMessageSink>,
        yolo: YoloMode,
        auto: AutoMode,
    ) -> Self {
        Self::new(execution, AcpClient::new(transport, sink), yolo, auto)
    }

    pub fn new(
        execution: AgentExecution,
        mut client: AcpClient,
        yolo: YoloMode,
        auto: AutoMode,
    ) -> Self {
        let permissions_rx = client.take_incoming();
        let updates_rx = client.take_updates();
        Self {
            execution,
            client: Arc::new(client),
            session_id: Mutex::new(None),
            permissions_rx,
            pending_permissions: Mutex::new(HashSet::new()),
            updates_rx,
            auto_approve: matches!(yolo, YoloMode::Enabled) || matches!(auto, AutoMode::Enabled),
        }
    }

    /// Negotiate ACP v1 then create a session rooted at the supplied absolute
    /// *container* working directory.
    pub async fn initialize(&mut self, cwd: impl AsRef<Path>) -> Result<String, EngineError> {
        let cwd = cwd.as_ref();
        if !cwd.is_absolute() {
            return Err(EngineError::Acp(
                "ACP session cwd must be absolute inside the container".into(),
            ));
        }
        let initialized: InitializeResponse = decode(
            self.handshake_request("initialize", InitializeRequest::default())
                .await?,
        )?;
        // `initialize` returns the *negotiated* version, which the ACP spec
        // permits to be lower than the one we requested. Accept anything up to
        // and including our supported version; only a higher (unknown) version
        // is a hard error.
        if initialized.protocol_version == 0
            || initialized.protocol_version > crate::engine::acp::protocol::ACP_PROTOCOL_VERSION
        {
            return Err(EngineError::Acp(format!(
                "unsupported ACP protocol version {}",
                initialized.protocol_version
            )));
        }
        let created: NewSessionResponse = decode(
            self.handshake_request(
                "session/new",
                NewSessionRequest {
                    cwd: cwd.to_string_lossy().into_owned(),
                    mcp_servers: vec![],
                    additional_directories: vec![],
                },
            )
            .await?,
        )?;
        *self.session_id.lock().await = Some(created.session_id.clone());
        Ok(created.session_id)
    }

    /// A handshake JSON-RPC request bounded by [`HANDSHAKE_TIMEOUT`]. Takes
    /// `&mut self` so the future stays `Send` (a shared `&AcpSession` held
    /// across `.await` would require `AcpSession: Sync`, which the command
    /// futures deliberately avoid).
    async fn handshake_request<T: Serialize>(
        &mut self,
        method: &str,
        params: T,
    ) -> Result<serde_json::Value, EngineError> {
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, self.client.request(method, params)).await {
            Ok(result) => result,
            Err(_) => Err(EngineError::Acp(format!(
                "timed out after {}s waiting for ACP {method} response",
                HANDSHAKE_TIMEOUT.as_secs()
            ))),
        }
    }

    /// Cancel the agent process and reap it, returning its exit info. Call on
    /// any error path once the container has launched, so the `docker run`
    /// child is never leaked (an errored `initialize`/`prompt` otherwise drops
    /// the still-running execution without waiting on it).
    pub async fn shutdown(&mut self) -> Result<AgentExitInfo, EngineError> {
        let _ = self.cancel().await;
        let _ = self.execution.cancel();
        self.execution.wait().await
    }

    pub async fn prompt(&mut self, text: impl Into<String>) -> Result<PromptResponse, EngineError> {
        let session_id = self
            .session_id
            .lock()
            .await
            .clone()
            .ok_or_else(|| EngineError::Acp("ACP session has not been initialized".into()))?;
        decode(
            self.client
                .request(
                    "session/prompt",
                    PromptRequest {
                        session_id,
                        prompt: vec![ContentBlock::Text { text: text.into() }],
                    },
                )
                .await?,
        )
    }

    /// Interrupt the active prompt and mark all still-pending permission
    /// requests cancelled, as required by ACP's cancellation semantics.
    pub async fn cancel(&mut self) -> Result<(), EngineError> {
        if let Some(session_id) = self.session_id.lock().await.clone() {
            self.client.notify(
                "session/cancel",
                serde_json::json!({"sessionId": session_id}),
            )?;
        }
        let ids: Vec<_> = self.pending_permissions.lock().await.drain().collect();
        for id in ids {
            self.client
                .respond_permission(id, PermissionDecision::Cancelled)?;
        }
        Ok(())
    }

    /// Send the response for exactly the agent request identified by `request_id`.
    pub async fn respond_permission(
        &mut self,
        request_id: JsonRpcId,
        decision: PermissionDecision,
    ) -> Result<(), EngineError> {
        self.pending_permissions.lock().await.remove(&request_id);
        self.client.respond_permission(request_id, decision)
    }

    pub fn execution(&self) -> &AgentExecution {
        &self.execution
    }
    pub async fn wait(&mut self) -> Result<AgentExitInfo, EngineError> {
        self.execution.wait().await
    }

    /// Drive a complete interactive conversation through the portable
    /// frontend. The only calls made into presentation code are the three
    /// methods on `AcpFrontend`.
    pub async fn drive(
        &mut self,
        frontend: &mut dyn AcpFrontend,
    ) -> Result<AgentExitInfo, EngineError> {
        self.drive_with_initial_prompt(frontend, None).await
    }

    /// Like [`drive`](Self::drive) but runs `initial` as the first turn before
    /// asking the frontend for follow-ups. The update subscription is taken
    /// **before** the first prompt is sent, so no `session/update` of the seeded
    /// turn is lost — a `tokio::broadcast` drops any message published while it
    /// has no subscribers, which previously silenced the entire first turn of a
    /// headless `exec prompt --launch-mode acp` run. Running the seeded turn
    /// inside the same driver loop also services `session/request_permission`
    /// during that turn, avoiding the permission-before-prompt-response
    /// circular wait.
    pub async fn drive_with_initial_prompt(
        &mut self,
        frontend: &mut dyn AcpFrontend,
        initial: Option<String>,
    ) -> Result<AgentExitInfo, EngineError> {
        let mut next = initial;
        loop {
            let prompt = match next.take() {
                Some(prompt) => prompt,
                None => match frontend.next_prompt() {
                    Some(prompt) => prompt,
                    None => {
                        self.cancel().await?;
                        // `None` means the whole awman session ends, so also use
                        // the normal execution cancellation path instead of
                        // waiting indefinitely for a cooperative agent to exit.
                        self.execution.cancel()?;
                        return self.execution.wait().await;
                    }
                },
            };
            let client = self.client.clone();
            let session_id =
                self.session_id.lock().await.clone().ok_or_else(|| {
                    EngineError::Acp("ACP session has not been initialized".into())
                })?;
            let mut turn = Box::pin(async move {
                let value = client
                    .request(
                        "session/prompt",
                        PromptRequest {
                            session_id,
                            prompt: vec![ContentBlock::Text { text: prompt }],
                        },
                    )
                    .await?;
                decode::<PromptResponse>(value)
            });
            loop {
                tokio::select! {
                    result = &mut turn => {
                        // The reader enqueues a turn's `session/update`s before
                        // the prompt response that ends the turn, so anything
                        // still queued here belongs to this turn — drain and
                        // render it before finishing (otherwise the tail of a
                        // turn, or the whole of a fast headless turn, is lost).
                        while let Ok(update) = self.updates_rx.try_recv() {
                            frontend.render_update(update);
                        }
                        if let Err(e) = result {
                            // The turn RPC failed (typically the agent dropped
                            // the connection mid-turn). Reap the child so it is
                            // never leaked, then surface the error.
                            let _ = self.execution.cancel();
                            let _ = self.execution.wait().await;
                            return Err(e);
                        }
                        break;
                    }
                    update = self.updates_rx.recv() => match update {
                        Some(update) => frontend.render_update(update),
                        // Update channel closed => the ACP reader task ended
                        // because stdout closed: the connection dropped. Cancel
                        // and reap the execution and return its AgentExitInfo
                        // (the edge case requires a done/error transition, not a
                        // hang) — cancel covers a peer that closes stdout but
                        // keeps its process alive.
                        None => {
                            let _ = self.execution.cancel();
                            return self.execution.wait().await;
                        }
                    },
                    request = self.permissions_rx.recv() => match request {
                        Some(IncomingRequest::Permission(request)) => self.handle_permission(frontend, request).await?,
                        // Incoming channel closed => connection dropped; reap.
                        None => {
                            let _ = self.execution.cancel();
                            return self.execution.wait().await;
                        }
                    },
                }
            }
        }
    }

    async fn handle_permission(
        &mut self,
        frontend: &mut dyn AcpFrontend,
        request: PermissionRequest,
    ) -> Result<(), EngineError> {
        self.pending_permissions
            .lock()
            .await
            .insert(request.request_id.clone());
        let decision = if self.auto_approve {
            PermissionDecision::approve(&request.options)
        } else {
            frontend.request_permission(request.clone())
        };
        self.respond_permission(request.request_id, decision).await
    }
}

fn decode<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T, EngineError> {
    serde_json::from_value(value)
        .map_err(|e| EngineError::Acp(format!("invalid ACP response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::message::RecordingMessageSink;
    use crate::engine::acp::frontend::AcpFrontend;
    use crate::engine::container::instance::{handle_now, ContainerId};
    use crate::engine::container::options::{ContainerName, ImageRef};
    use chrono::Utc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ApprovingFrontend {
        permission_calls: Arc<AtomicUsize>,
    }

    impl AcpFrontend for ApprovingFrontend {
        fn render_update(&mut self, _update: SessionUpdate) {}
        fn request_permission(&mut self, request: PermissionRequest) -> PermissionDecision {
            self.permission_calls.fetch_add(1, Ordering::Relaxed);
            PermissionDecision::approve(&request.options)
        }
        fn next_prompt(&mut self) -> Option<String> {
            None
        }
    }

    fn finished_execution() -> AgentExecution {
        let handle = handle_now(
            &ContainerId::new("acp-test"),
            &ContainerName::new("acp-test"),
            &ImageRef::new("acp-test:latest"),
        );
        let now = Utc::now();
        AgentExecution::finished(
            handle,
            AgentExitInfo {
                exit_code: 0,
                signal: None,
                started_at: now,
                ended_at: now,
            },
        )
    }

    #[tokio::test]
    async fn permission_responses_keep_two_concurrent_request_ids() {
        let (frontend, transport) = AcpTransport::channel();
        let mut io = frontend.into_io_for_test();
        let client = AcpClient::new(transport, Box::new(RecordingMessageSink::new()));
        let mut session = AcpSession::new(
            finished_execution(),
            client,
            YoloMode::Disabled,
            AutoMode::Disabled,
        );
        for id in [41, 42] {
            io.stdout.send(serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0", "id": id, "method": "session/request_permission",
                "params": {"sessionId": "s", "toolCall": {"toolCallId": format!("tool-{id}")},
                    "options": [{"optionId": format!("allow-{id}"), "name": "Allow", "kind": "allow_once"}]}
            })).unwrap()).unwrap();
            io.stdout.send(vec![b'\n']).unwrap();
        }
        let permission_calls = Arc::new(AtomicUsize::new(0));
        let mut frontend = ApprovingFrontend {
            permission_calls: permission_calls.clone(),
        };
        let mut observed_ids = Vec::new();
        for _ in 0..2 {
            let Some(IncomingRequest::Permission(request)) = session.permissions_rx.recv().await
            else {
                panic!("expected permission request")
            };
            observed_ids.push(request.request_id.clone());
            session
                .handle_permission(&mut frontend, request)
                .await
                .unwrap();
        }
        let one: serde_json::Value =
            serde_json::from_slice(&io.stdin_rx.recv().await.unwrap()).unwrap();
        let two: serde_json::Value =
            serde_json::from_slice(&io.stdin_rx.recv().await.unwrap()).unwrap();
        assert_eq!(permission_calls.load(Ordering::Relaxed), 2);
        assert_eq!(
            observed_ids,
            vec![JsonRpcId::Number(41), JsonRpcId::Number(42)]
        );
        assert_eq!(one["id"], 41, "first frontend decision must answer id 41");
        assert_eq!(one["result"]["outcome"]["optionId"], "allow-41");
        assert_eq!(two["id"], 42, "second frontend decision must answer id 42");
        assert_eq!(two["result"]["outcome"]["optionId"], "allow-42");
    }

    struct RecordingFrontend {
        rendered: Arc<std::sync::Mutex<Vec<SessionUpdate>>>,
    }
    impl AcpFrontend for RecordingFrontend {
        fn render_update(&mut self, update: SessionUpdate) {
            self.rendered.lock().unwrap().push(update);
        }
        fn request_permission(&mut self, _r: PermissionRequest) -> PermissionDecision {
            PermissionDecision::Cancelled
        }
        fn next_prompt(&mut self) -> Option<String> {
            None
        }
    }

    #[tokio::test]
    async fn without_yolo_a_fail_closed_frontend_denial_is_sent_verbatim() {
        // Regression for the permission-leak blocker: when neither --yolo nor
        // --auto is set, the frontend decides. A fail-closed frontend returns
        // Cancelled and that "cancelled" outcome must be what the agent
        // receives — the engine must NOT substitute an approval.
        let (frontend, transport) = AcpTransport::channel();
        let mut io = frontend.into_io_for_test();
        let client = AcpClient::new(transport, Box::new(RecordingMessageSink::new()));
        let mut session = AcpSession::new(
            finished_execution(),
            client,
            YoloMode::Disabled,
            AutoMode::Disabled,
        );
        io.stdout
            .send(
                serde_json::to_vec(&serde_json::json!({
                    "jsonrpc": "2.0", "id": 7, "method": "session/request_permission",
                    "params": {"sessionId": "s", "toolCall": {"toolCallId": "tool-7"},
                        "options": [{"optionId": "allow-7", "name": "Allow", "kind": "allow_once"}]}
                }))
                .unwrap(),
            )
            .unwrap();
        io.stdout.send(vec![b'\n']).unwrap();

        let mut frontend = RecordingFrontend {
            rendered: Arc::new(std::sync::Mutex::new(Vec::new())),
        };
        let Some(IncomingRequest::Permission(request)) = session.permissions_rx.recv().await else {
            panic!("expected permission request")
        };
        session
            .handle_permission(&mut frontend, request)
            .await
            .unwrap();
        let response: serde_json::Value =
            serde_json::from_slice(&io.stdin_rx.recv().await.unwrap()).unwrap();
        assert_eq!(response["id"], 7);
        assert_eq!(
            response["result"]["outcome"]["outcome"], "cancelled",
            "a run without yolo/auto must never auto-approve: {response}"
        );
    }

    #[tokio::test]
    async fn seeded_turn_updates_are_rendered_not_dropped() {
        // Regression for M3: the update subscription must be taken before the
        // seeded prompt is sent, or the broadcast drops every update of the
        // first turn (silencing headless `exec prompt --launch-mode acp`).
        let (frontend, transport) = AcpTransport::channel();
        let mut io = frontend.into_io_for_test();
        let client = AcpClient::new(transport, Box::new(RecordingMessageSink::new()));
        let mut session = AcpSession::new(
            finished_execution(),
            client,
            YoloMode::Disabled,
            AutoMode::Disabled,
        );
        *session.session_id.lock().await = Some("s".into());

        let the_update = SessionUpdate::AgentMessageChunk {
            chunk: crate::engine::acp::protocol::ContentChunk {
                content: ContentBlock::Text {
                    text: "first-turn output".into(),
                },
                message_id: None,
            },
        };
        let update_for_agent = the_update.clone();
        // Fake agent: wait for the session/prompt, THEN emit the update and the
        // prompt response. If the driver only subscribed after prompting, this
        // update would be lost.
        let agent = tokio::spawn(async move {
            let _prompt = io.stdin_rx.recv().await.expect("session/prompt");
            let notif = crate::engine::acp::protocol::SessionUpdateNotification {
                session_id: "s".into(),
                update: update_for_agent,
            };
            let frame = serde_json::json!({
                "jsonrpc": "2.0", "method": "session/update",
                "params": serde_json::to_value(&notif).unwrap(),
            });
            io.stdout.send(serde_json::to_vec(&frame).unwrap()).unwrap();
            io.stdout.send(vec![b'\n']).unwrap();
            io.stdout
                .send(
                    serde_json::to_vec(&serde_json::json!({
                        "jsonrpc": "2.0", "id": 1, "result": {"stopReason": "end_turn"}
                    }))
                    .unwrap(),
                )
                .unwrap();
            io.stdout.send(vec![b'\n']).unwrap();
            // Keep io (and thus the channels) alive until the driver finishes.
            io.stdin_rx.recv().await
        });

        let mut frontend = RecordingFrontend {
            rendered: Arc::new(std::sync::Mutex::new(Vec::new())),
        };
        let rendered = frontend.rendered.clone();
        session
            .drive_with_initial_prompt(&mut frontend, Some("go".into()))
            .await
            .unwrap();
        agent.abort();
        let seen = rendered.lock().unwrap().clone();
        assert!(
            seen.contains(&the_update),
            "the seeded turn's update must be rendered, got: {seen:?}"
        );
    }

    #[tokio::test]
    async fn drive_returns_and_reaps_when_connection_drops_mid_turn() {
        // Regression for M4: a dropped connection mid-turn must not hang and
        // must not leak the child — drive returns (done or error) promptly by
        // reaping the owned execution.
        let (frontend, transport) = AcpTransport::channel();
        let io = frontend.into_io_for_test();
        let client = AcpClient::new(transport, Box::new(RecordingMessageSink::new()));
        let mut session = AcpSession::new(
            finished_execution(),
            client,
            YoloMode::Disabled,
            AutoMode::Disabled,
        );
        *session.session_id.lock().await = Some("s".into());
        // Drop the container IO so both stdout (reader) and stdin close: the
        // turn's request fails and the incoming channel closes.
        drop(io);
        let mut frontend = RecordingFrontend {
            rendered: Arc::new(std::sync::Mutex::new(Vec::new())),
        };
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            session.drive_with_initial_prompt(&mut frontend, Some("go".into())),
        )
        .await;
        assert!(
            result.is_ok(),
            "drive must not hang when the ACP connection drops mid-turn"
        );
    }

    #[tokio::test]
    async fn yolo_auto_approves_without_calling_frontend_permission_hook() {
        let (frontend, transport) = AcpTransport::channel();
        let mut io = frontend.into_io_for_test();
        let client = AcpClient::new(transport, Box::new(RecordingMessageSink::new()));
        let mut session = AcpSession::new(
            finished_execution(),
            client,
            YoloMode::Enabled,
            AutoMode::Disabled,
        );
        io.stdout
            .send(
                serde_json::to_vec(&serde_json::json!({
                    "jsonrpc": "2.0", "id": 99, "method": "session/request_permission",
                    "params": {"sessionId": "s", "toolCall": {"toolCallId": "tool-99"},
                        "options": [{"optionId": "allow-99", "name": "Allow", "kind": "allow_once"}]}
                }))
                .unwrap(),
            )
            .unwrap();
        io.stdout.send(vec![b'\n']).unwrap();

        let permission_calls = Arc::new(AtomicUsize::new(0));
        let mut frontend = ApprovingFrontend {
            permission_calls: permission_calls.clone(),
        };
        let Some(IncomingRequest::Permission(request)) = session.permissions_rx.recv().await else {
            panic!("expected permission request")
        };
        session
            .handle_permission(&mut frontend, request)
            .await
            .unwrap();

        let response: serde_json::Value =
            serde_json::from_slice(&io.stdin_rx.recv().await.unwrap()).unwrap();
        assert_eq!(permission_calls.load(Ordering::Relaxed), 0);
        assert_eq!(response["id"], 99);
        assert_eq!(response["result"]["outcome"]["optionId"], "allow-99");
    }
}
