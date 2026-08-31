//! JSON-RPC transport, correlation, and inbound ACP dispatch.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};

use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::engine::acp::protocol::{
    encode_line, JsonRpcError, JsonRpcId, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    LineFramer, PermissionDecision, PermissionRequest, ReadTextFileRequest,
    SessionUpdateNotification, WriteTextFileRequest,
};
use crate::engine::agent_runtime::frontend::{AgentFrontend, AgentIo, AgentProgress, AgentStatus};
use crate::engine::error::EngineError;

type SharedSink = Arc<Mutex<Box<dyn UserMessageSink>>>;
type PendingMap = Arc<AsyncMutex<HashMap<JsonRpcId, oneshot::Sender<Result<Value, EngineError>>>>>;

/// I/O detached from an ACP runtime frontend. It is consumed once by
/// [`AcpClient::new`] after the runtime has taken the matching `AgentIo`.
pub struct AcpTransport {
    stdout_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    stderr_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    stdin_tx: mpsc::UnboundedSender<Vec<u8>>,
}

/// Runtime adapter used to select the persistent piped ACP spawn path.
///
/// `initial_size` and `resize` are deliberately `None`; a PTY would corrupt
/// line framing. It carries no presentation logic.
pub struct AcpTransportFrontend {
    io: Option<AgentIo>,
}

impl AcpTransportFrontend {
    #[cfg(test)]
    pub(crate) fn into_io_for_test(mut self) -> AgentIo {
        self.io.take().expect("test ACP frontend has no AgentIo")
    }
}

impl AcpTransport {
    pub fn channel() -> (AcpTransportFrontend, Self) {
        let (stdout_tx, stdout_rx) = mpsc::unbounded_channel();
        let (stderr_tx, stderr_rx) = mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel();
        (
            AcpTransportFrontend {
                io: Some(AgentIo {
                    stdout: stdout_tx,
                    stderr: stderr_tx,
                    stdin_tx: stdin_tx.clone(),
                    stdin_rx,
                    resize: None,
                    initial_size: None,
                }),
            },
            Self {
                stdout_rx,
                stderr_rx,
                stdin_tx,
            },
        )
    }
}

impl UserMessageSink for AcpTransportFrontend {
    fn write_message(&mut self, _msg: UserMessage) {}
    fn replay_queued(&mut self) {}
}

#[async_trait::async_trait]
impl AgentFrontend for AcpTransportFrontend {
    fn report_status(&mut self, _status: AgentStatus) {}
    fn report_progress(&mut self, _progress: AgentProgress) {}
    fn take_io(&mut self) -> AgentIo {
        self.io
            .take()
            .expect("AcpTransportFrontend::take_io called more than once")
    }
}

/// Container-only filesystem callback. An implementation may use a container
/// backend API, but it must never translate an ACP path into a host path.
pub trait ContainerFileSystem: Send + Sync {
    fn read_text_file(&self, request: &ReadTextFileRequest) -> Result<String, EngineError>;
    fn write_text_file(&self, request: &WriteTextFileRequest) -> Result<(), EngineError>;
}

/// Default filesystem policy. It intentionally refuses requests rather than
/// accidentally treating an absolute container path as a host filesystem path.
pub struct DenyHostFilesystem;

impl ContainerFileSystem for DenyHostFilesystem {
    fn read_text_file(&self, _request: &ReadTextFileRequest) -> Result<String, EngineError> {
        Err(EngineError::Acp(
            "container filesystem service is unavailable".into(),
        ))
    }
    fn write_text_file(&self, _request: &WriteTextFileRequest) -> Result<(), EngineError> {
        Err(EngineError::Acp(
            "container filesystem service is unavailable".into(),
        ))
    }
}

/// Requests initiated by an ACP agent which require the portable session UI.
#[derive(Debug, Clone, PartialEq)]
pub enum IncomingRequest {
    Permission(PermissionRequest),
}

/// An ACP JSON-RPC client bound to one agent process's stdin/stdout.
pub struct AcpClient {
    stdin_tx: mpsc::UnboundedSender<Vec<u8>>,
    next_id: AtomicI64,
    pending: PendingMap,
    incoming_rx: Option<mpsc::UnboundedReceiver<IncomingRequest>>,
    updates_rx: Option<mpsc::UnboundedReceiver<crate::engine::acp::protocol::SessionUpdate>>,
}

impl AcpClient {
    pub fn new(transport: AcpTransport, sink: Box<dyn UserMessageSink>) -> Self {
        Self::with_filesystem(transport, sink, Arc::new(DenyHostFilesystem))
    }

    /// Construct a client with a custom filesystem service. Restricted to the
    /// crate: an `fs/read_text_file`/`fs/write_text_file` handler MUST operate
    /// strictly within the running container's own filesystem and MUST NEVER
    /// interpret an agent-supplied path against awman's host filesystem (see
    /// `aspec/architecture/security.md`). Production uses [`DenyHostFilesystem`]
    /// via [`AcpClient::new`]; no container-scoped implementation exists yet, so
    /// keeping this seam crate-private prevents an out-of-tree caller from
    /// wiring a host `std::fs` implementation into the ACP fs handlers.
    pub(crate) fn with_filesystem(
        transport: AcpTransport,
        sink: Box<dyn UserMessageSink>,
        filesystem: Arc<dyn ContainerFileSystem>,
    ) -> Self {
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        let (updates_tx, updates_rx) = mpsc::unbounded_channel();
        let pending = Arc::new(AsyncMutex::new(HashMap::new()));
        let shared_sink = Arc::new(Mutex::new(sink));
        spawn_reader(
            transport.stdout_rx,
            transport.stderr_rx,
            transport.stdin_tx.clone(),
            pending.clone(),
            incoming_tx,
            updates_tx,
            filesystem,
            shared_sink,
        );
        Self {
            stdin_tx: transport.stdin_tx,
            next_id: AtomicI64::new(1),
            pending,
            incoming_rx: Some(incoming_rx),
            updates_rx: Some(updates_rx),
        }
    }

    pub fn take_incoming(&mut self) -> mpsc::UnboundedReceiver<IncomingRequest> {
        self.incoming_rx
            .take()
            .expect("AcpClient incoming receiver already taken")
    }

    pub fn take_updates(
        &mut self,
    ) -> mpsc::UnboundedReceiver<crate::engine::acp::protocol::SessionUpdate> {
        self.updates_rx
            .take()
            .expect("AcpClient update receiver already taken")
    }

    pub async fn request<T: Serialize>(
        &self,
        method: &str,
        params: T,
    ) -> Result<Value, EngineError> {
        let id = JsonRpcId::Number(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: id.clone(),
            method: method.into(),
            params,
        };
        self.send(&request)?;
        rx.await.map_err(|_| {
            EngineError::Acp(format!("ACP connection closed while waiting for {method}"))
        })?
    }

    pub fn notify<T: Serialize>(&self, method: &str, params: T) -> Result<(), EngineError> {
        self.send(&JsonRpcNotification {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params,
        })
    }

    pub fn respond_permission(
        &self,
        request_id: JsonRpcId,
        decision: PermissionDecision,
    ) -> Result<(), EngineError> {
        self.respond_result(request_id, decision.as_result())
    }

    pub fn respond_result(&self, id: JsonRpcId, result: Value) -> Result<(), EngineError> {
        self.send(&JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        })
    }

    fn send<T: Serialize>(&self, message: &T) -> Result<(), EngineError> {
        let bytes = encode_line(message).map_err(EngineError::Acp)?;
        self.stdin_tx
            .send(bytes)
            .map_err(|_| EngineError::Acp("ACP stdin connection closed".into()))
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_reader(
    mut stdout_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    mut stderr_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    stdin_tx: mpsc::UnboundedSender<Vec<u8>>,
    pending: PendingMap,
    incoming_tx: mpsc::UnboundedSender<IncomingRequest>,
    updates_tx: mpsc::UnboundedSender<crate::engine::acp::protocol::SessionUpdate>,
    filesystem: Arc<dyn ContainerFileSystem>,
    sink: SharedSink,
) {
    tokio::spawn(async move {
        // ACP permits diagnostics on stderr. Surface them without trying to
        // interpret them as protocol frames.
        let stderr_sink = sink.clone();
        tokio::spawn(async move {
            while let Some(bytes) = stderr_rx.recv().await {
                let text = String::from_utf8_lossy(&bytes).trim().to_owned();
                if !text.is_empty() {
                    warning(&stderr_sink, format!("ACP agent stderr: {text}"));
                }
            }
        });

        let mut framer = LineFramer::default();
        while let Some(bytes) = stdout_rx.recv().await {
            for frame in framer.push(&bytes) {
                match frame {
                    Ok(value) => {
                        dispatch(
                            value,
                            &stdin_tx,
                            &pending,
                            &incoming_tx,
                            &updates_tx,
                            filesystem.as_ref(),
                            &sink,
                        )
                        .await
                    }
                    Err(error) => warning(
                        &sink,
                        format!("ACP protocol warning: ignoring malformed line: {error}"),
                    ),
                }
            }
        }
        let mut pending = pending.lock().await;
        for (_, sender) in pending.drain() {
            let _ = sender.send(Err(EngineError::Acp("ACP connection closed".into())));
        }
    });
}

async fn dispatch(
    value: Value,
    stdin_tx: &mpsc::UnboundedSender<Vec<u8>>,
    pending: &PendingMap,
    incoming_tx: &mpsc::UnboundedSender<IncomingRequest>,
    updates_tx: &mpsc::UnboundedSender<crate::engine::acp::protocol::SessionUpdate>,
    filesystem: &dyn ContainerFileSystem,
    sink: &SharedSink,
) {
    let Some(object) = value.as_object() else {
        warning(
            sink,
            "ACP protocol warning: ignoring non-object JSON-RPC frame".into(),
        );
        return;
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        warning(
            sink,
            "ACP protocol warning: ignoring frame without jsonrpc: 2.0".into(),
        );
        return;
    }
    if object.contains_key("result") || object.contains_key("error") {
        let Some(id) = object.get("id").and_then(parse_id) else {
            warning(
                sink,
                "ACP protocol warning: response has no usable id".into(),
            );
            return;
        };
        if let Some(sender) = pending.lock().await.remove(&id) {
            let result = if let Some(error) = object.get("error") {
                Err(EngineError::Acp(format!("agent RPC error: {error}")))
            } else {
                Ok(object.get("result").cloned().unwrap_or(Value::Null))
            };
            let _ = sender.send(result);
        }
        return;
    }
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        warning(
            sink,
            "ACP protocol warning: request/notification has no method".into(),
        );
        return;
    };
    let params = object.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "session/update" => match serde_json::from_value::<SessionUpdateNotification>(params) {
            Ok(notification) => {
                let _ = updates_tx.send(notification.update);
            }
            Err(e) => warning(
                sink,
                format!("ACP protocol warning: invalid session/update: {e}"),
            ),
        },
        "session/request_permission" => {
            let Some(id) = object.get("id").and_then(parse_id) else {
                warning(
                    sink,
                    "ACP protocol warning: permission request has no id".into(),
                );
                return;
            };
            match serde_json::from_value::<PermissionRequestWire>(params) {
                Ok(request) => {
                    let request = PermissionRequest {
                        request_id: id,
                        session_id: request.session_id,
                        tool_call: request.tool_call,
                        options: request.options,
                    };
                    let _ = incoming_tx.send(IncomingRequest::Permission(request));
                }
                Err(e) => {
                    warning(
                        sink,
                        format!("ACP protocol warning: invalid permission request: {e}"),
                    );
                    send_error(stdin_tx, id, -32602, "invalid permission request");
                }
            }
        }
        "fs/read_text_file" => {
            let Some(id) = object.get("id").and_then(parse_id) else {
                warning(sink, "ACP protocol warning: file request has no id".into());
                return;
            };
            match serde_json::from_value::<ReadTextFileRequest>(params).and_then(|r| {
                filesystem
                    .read_text_file(&r)
                    .map_err(|e| serde_json::Error::io(std::io::Error::other(e.to_string())))
            }) {
                Ok(content) => send_result(stdin_tx, id, serde_json::json!({"content": content})),
                Err(e) => send_error(stdin_tx, id, -32603, &e.to_string()),
            }
        }
        "fs/write_text_file" => {
            let Some(id) = object.get("id").and_then(parse_id) else {
                warning(sink, "ACP protocol warning: file request has no id".into());
                return;
            };
            match serde_json::from_value::<WriteTextFileRequest>(params).and_then(|r| {
                filesystem
                    .write_text_file(&r)
                    .map_err(|e| serde_json::Error::io(std::io::Error::other(e.to_string())))
            }) {
                Ok(()) => send_result(stdin_tx, id, serde_json::json!({})),
                Err(e) => send_error(stdin_tx, id, -32603, &e.to_string()),
            }
        }
        _ => {
            if let Some(id) = object.get("id").and_then(parse_id) {
                warning(
                    sink,
                    format!("ACP protocol warning: unsupported agent request '{method}'"),
                );
                send_error(stdin_tx, id, -32601, "method not supported by awman");
            }
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PermissionRequestWire {
    session_id: String,
    tool_call: crate::engine::acp::protocol::ToolCallUpdate,
    options: Vec<crate::engine::acp::protocol::PermissionOption>,
}

fn parse_id(value: &Value) -> Option<JsonRpcId> {
    serde_json::from_value(value.clone()).ok()
}
fn send_result(stdin: &mpsc::UnboundedSender<Vec<u8>>, id: JsonRpcId, result: Value) {
    let _ = send_response(
        stdin,
        JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        },
    );
}
fn send_error(stdin: &mpsc::UnboundedSender<Vec<u8>>, id: JsonRpcId, code: i64, message: &str) {
    let _ = send_response(
        stdin,
        JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        },
    );
}
fn send_response(
    stdin: &mpsc::UnboundedSender<Vec<u8>>,
    response: JsonRpcResponse,
) -> Result<(), EngineError> {
    stdin
        .send(encode_line(&response).map_err(EngineError::Acp)?)
        .map_err(|_| EngineError::Acp("ACP stdin connection closed".into()))
}
fn warning(sink: &SharedSink, text: String) {
    if let Ok(mut sink) = sink.lock() {
        sink.write_message(UserMessage {
            level: MessageLevel::Warning,
            text,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::message::{RecordingMessageSink, UserMessage};
    use crate::engine::acp::protocol::ContentBlock;

    struct CapturingSink(Arc<Mutex<Vec<UserMessage>>>);
    impl UserMessageSink for CapturingSink {
        fn write_message(&mut self, msg: UserMessage) {
            self.0.lock().unwrap().push(msg);
        }
        fn replay_queued(&mut self) {}
    }

    #[tokio::test]
    async fn malformed_line_is_a_warning_and_later_update_arrives() {
        let (frontend, transport) = AcpTransport::channel();
        let io = frontend.into_io_for_test();
        let messages = Arc::new(Mutex::new(Vec::new()));
        let mut client = AcpClient::new(transport, Box::new(CapturingSink(messages.clone())));
        let mut updates = client.take_updates();
        io.stdout.send(b"not json\n".to_vec()).unwrap();
        io.stdout.send(serde_json::to_vec(&serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"ok"}}}})).unwrap()).unwrap();
        io.stdout.send(vec![b'\n']).unwrap();
        let update = updates.recv().await.unwrap();
        assert_eq!(
            update,
            crate::engine::acp::protocol::SessionUpdate::AgentMessageChunk {
                chunk: crate::engine::acp::protocol::ContentChunk {
                    content: ContentBlock::Text { text: "ok".into() },
                    message_id: None
                }
            }
        );
        assert!(messages.lock().unwrap().iter().any(|message| {
            message.level == MessageLevel::Warning
                && message.text.contains("ignoring malformed line")
        }));
    }

    #[tokio::test]
    async fn two_concurrent_requests_keep_their_ids() {
        let (frontend, transport) = AcpTransport::channel();
        let mut io = frontend.into_io_for_test();
        let client = Arc::new(AcpClient::new(
            transport,
            Box::new(RecordingMessageSink::new()),
        ));
        let a = {
            let c = client.clone();
            tokio::spawn(async move { c.request("one", serde_json::json!({})).await.unwrap() })
        };
        let b = {
            let c = client.clone();
            tokio::spawn(async move { c.request("two", serde_json::json!({})).await.unwrap() })
        };
        let first: Value = serde_json::from_slice(&io.stdin_rx.recv().await.unwrap()).unwrap();
        let second: Value = serde_json::from_slice(&io.stdin_rx.recv().await.unwrap()).unwrap();
        let first_id = first["id"].clone();
        let second_id = second["id"].clone();
        io.stdout
            .send(
                format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"value\":\"second\"}}}}\n",
                    second_id
                )
                .into_bytes(),
            )
            .unwrap();
        io.stdout
            .send(
                format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"value\":\"first\"}}}}\n",
                    first_id
                )
                .into_bytes(),
            )
            .unwrap();
        assert_eq!(a.await.unwrap()["value"], "first");
        assert_eq!(b.await.unwrap()["value"], "second");
    }
}
