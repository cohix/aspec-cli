//! Typed ACP JSON-RPC 2.0 shapes and newline-delimited framing.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const JSONRPC_VERSION: &str = "2.0";
pub const ACP_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    Number(i64),
    String(String),
}

impl Default for JsonRpcId {
    fn default() -> Self {
        // Only used for a serde-skipped, locally-attached request id. ACP
        // wire messages never rely on this placeholder.
        Self::Number(0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest<T> {
    #[serde(default = "jsonrpc_version")]
    pub jsonrpc: String,
    pub id: JsonRpcId,
    pub method: String,
    pub params: T,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcNotification<T> {
    #[serde(default = "jsonrpc_version")]
    pub jsonrpc: String,
    pub method: String,
    pub params: T,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    #[serde(default = "jsonrpc_version")]
    pub jsonrpc: String,
    pub id: JsonRpcId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

fn jsonrpc_version() -> String {
    JSONRPC_VERSION.to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    pub fs: FileSystemCapabilities,
    pub terminal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemCapabilities {
    pub read_text_file: bool,
    pub write_text_file: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImplementationInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeRequest {
    pub protocol_version: u32,
    pub client_capabilities: ClientCapabilities,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_info: Option<ImplementationInfo>,
}

impl Default for InitializeRequest {
    fn default() -> Self {
        Self {
            protocol_version: ACP_PROTOCOL_VERSION,
            // Do not advertise host file access. A container filesystem
            // adapter may explicitly opt in when that path is available.
            client_capabilities: ClientCapabilities {
                fs: FileSystemCapabilities {
                    read_text_file: false,
                    write_text_file: false,
                },
                terminal: false,
            },
            client_info: Some(ImplementationInfo {
                name: "awman".into(),
                title: Some("awman".into()),
                version: env!("CARGO_PKG_VERSION").into(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    pub protocol_version: u32,
    #[serde(default)]
    pub agent_capabilities: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_info: Option<ImplementationInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionRequest {
    pub cwd: String,
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_directories: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResponse {
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRequest {
    pub session_id: String,
    pub prompt: Vec<ContentBlock>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResponse {
    pub stop_reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        data: String,
        mime_type: String,
    },
    Audio {
        data: String,
        mime_type: String,
    },
    ResourceLink {
        name: String,
        uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
    Resource {
        resource: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentChunk {
    pub content: ContentBlock,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub tool_call_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default)]
    pub content: Vec<Value>,
    #[serde(default)]
    pub locations: Vec<ToolCallLocation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallUpdate {
    pub tool_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locations: Option<Vec<ToolCallLocation>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallLocation {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEntry {
    pub content: String,
    pub priority: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableCommand {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum SessionUpdate {
    AgentMessageChunk {
        #[serde(flatten)]
        chunk: ContentChunk,
    },
    AgentThoughtChunk {
        #[serde(flatten)]
        chunk: ContentChunk,
    },
    ToolCall {
        #[serde(flatten)]
        tool_call: ToolCall,
    },
    ToolCallUpdate {
        #[serde(flatten)]
        tool_call: ToolCallUpdate,
    },
    Plan {
        entries: Vec<PlanEntry>,
    },
    AvailableCommandsUpdate {
        available_commands: Vec<AvailableCommand>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUpdateNotification {
    pub session_id: String,
    pub update: SessionUpdate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequest {
    /// The JSON-RPC id to pass to `AcpSession::respond_permission`.
    #[serde(skip)]
    pub request_id: JsonRpcId,
    pub session_id: String,
    pub tool_call: ToolCallUpdate,
    pub options: Vec<PermissionOption>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    Selected { option_id: String },
    Cancelled,
}

impl PermissionDecision {
    pub fn approve(options: &[PermissionOption]) -> Self {
        // Only ever select an option the agent explicitly marked as an "allow"
        // outcome. If none is offered, fail closed (Cancelled) rather than
        // blindly picking the first option — which could be a "reject" or
        // "always-deny" choice that we'd then be treating as approval.
        options
            .iter()
            .find(|o| o.kind == "allow_once" || o.kind == "allow_always")
            .map(|o| Self::Selected {
                option_id: o.option_id.clone(),
            })
            .unwrap_or(Self::Cancelled)
    }

    pub fn as_result(&self) -> Value {
        match self {
            Self::Selected { option_id } => {
                serde_json::json!({"outcome": {"outcome": "selected", "optionId": option_id}})
            }
            Self::Cancelled => serde_json::json!({"outcome": {"outcome": "cancelled"}}),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadTextFileRequest {
    pub session_id: String,
    pub path: String,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteTextFileRequest {
    pub session_id: String,
    pub path: String,
    pub content: String,
}

/// Maximum bytes buffered for a single unterminated JSON-RPC line. ACP frames
/// are small structured messages; a line larger than this is treated as hostile
/// or broken. The cap bounds host memory against a compromised/buggy container
/// that streams an endless prefix with no newline (a direct containment
/// concern: bytes from inside the container must not grow awman's memory without
/// bound).
pub const MAX_ACP_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Incremental newline-delimited JSON framing. A bad line is returned to the
/// caller and never poisons later frames. The buffered length is capped at
/// [`MAX_ACP_LINE_BYTES`]; an over-long line is discarded (with a warning) and
/// framing resumes at the next newline.
#[derive(Debug, Default)]
pub struct LineFramer {
    pending: Vec<u8>,
    /// True while dropping the tail of an over-long line until the next `\n`.
    discarding: bool,
}

impl LineFramer {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Result<Value, String>> {
        let mut frames = Vec::new();
        let mut rest = bytes;

        // If a previous push overflowed mid-line, drop bytes until the next
        // newline, then resume normal framing on the remainder.
        if self.discarding {
            match rest.iter().position(|b| *b == b'\n') {
                Some(pos) => {
                    self.discarding = false;
                    rest = &rest[pos + 1..];
                }
                None => return frames,
            }
        }

        self.pending.extend_from_slice(rest);
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let line = &line[..line.len() - 1];
            if line.iter().all(|b| b.is_ascii_whitespace()) {
                continue;
            }
            match std::str::from_utf8(line) {
                Ok(text) => frames.push(serde_json::from_str(text).map_err(|e| e.to_string())),
                Err(e) => frames.push(Err(format!("invalid UTF-8 ACP line: {e}"))),
            }
        }

        // Cap the unterminated remainder. A peer that never emits `\n` (or emits
        // a pathologically long line) must not grow `pending` without bound.
        // Discard the partial line, warn, and enter the discard state so the
        // next newline realigns framing instead of prepending the dropped bytes.
        if self.pending.len() > MAX_ACP_LINE_BYTES {
            let buffered = self.pending.len();
            self.pending.clear();
            self.discarding = true;
            frames.push(Err(format!(
                "ACP line exceeded {MAX_ACP_LINE_BYTES}-byte cap ({buffered} bytes buffered with \
                 no newline); discarding until next newline"
            )));
        }

        frames
    }
}

pub fn encode_line<T: Serialize>(message: &T) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec(message).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Neutralise C0/C1 terminal-control characters (everything except `\n` and
/// `\t`) in agent-provided text before it is rendered to a terminal. ACP is a
/// structured, host-rendered UI; without this, a malicious agent could embed
/// ANSI/OSC escape sequences (cursor movement, screen clears, OSC 52 clipboard
/// writes, deceptive titles) in message/tool/plan text and recover raw
/// terminal-control effects the structured renderer is meant to prevent.
pub fn sanitize_terminal_text(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\n' | '\t' => c,
            c if c.is_control() => '\u{FFFD}',
            c => c,
        })
        .collect()
}

fn summarize_content(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text { text } => sanitize_terminal_text(text),
        ContentBlock::Image { .. } => "(image)".into(),
        ContentBlock::Audio { .. } => "(audio)".into(),
        ContentBlock::ResourceLink { name, .. } => {
            format!("(resource link: {})", sanitize_terminal_text(name))
        }
        ContentBlock::Resource { .. } => "(resource)".into(),
    }
}

/// One-line, human-readable summary of a session update — never a raw `Debug`
/// JSON dump. Used by fallback/log frontends that don't render the full ACP
/// window. All agent-provided text is passed through [`sanitize_terminal_text`].
pub fn summarize_update(update: &SessionUpdate) -> String {
    let status_suffix =
        |s: &Option<String>| s.as_deref().map(|s| format!(" [{s}]")).unwrap_or_default();
    match update {
        SessionUpdate::AgentMessageChunk { chunk } => {
            format!("agent: {}", summarize_content(&chunk.content))
        }
        SessionUpdate::AgentThoughtChunk { chunk } => {
            format!("thinking: {}", summarize_content(&chunk.content))
        }
        SessionUpdate::ToolCall { tool_call } => format!(
            "tool call: {}{}",
            sanitize_terminal_text(&tool_call.title),
            status_suffix(&tool_call.status),
        ),
        SessionUpdate::ToolCallUpdate { tool_call } => format!(
            "tool update: {}{}",
            tool_call
                .title
                .as_deref()
                .map(sanitize_terminal_text)
                .unwrap_or_else(|| tool_call.tool_call_id.clone()),
            status_suffix(&tool_call.status),
        ),
        SessionUpdate::Plan { entries } => format!("plan: {} step(s)", entries.len()),
        SessionUpdate::AvailableCommandsUpdate { available_commands } => {
            format!("available commands: {}", available_commands.len())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn text_chunk() -> ContentChunk {
        ContentChunk {
            content: ContentBlock::Text {
                text: "hello".into(),
            },
            message_id: Some("m1".into()),
        }
    }
    fn tool() -> ToolCall {
        ToolCall {
            tool_call_id: "tool-1".into(),
            title: "Read".into(),
            kind: Some("read".into()),
            status: Some("pending".into()),
            content: vec![],
            locations: vec![],
            raw_input: None,
            raw_output: None,
        }
    }
    fn update() -> ToolCallUpdate {
        ToolCallUpdate {
            tool_call_id: "tool-1".into(),
            kind: None,
            status: Some("completed".into()),
            title: None,
            content: None,
            locations: None,
            raw_input: None,
            raw_output: None,
        }
    }
    #[test]
    fn every_supported_session_update_round_trips_as_one_line() {
        let updates = vec![
            SessionUpdate::AgentMessageChunk {
                chunk: text_chunk(),
            },
            SessionUpdate::AgentThoughtChunk {
                chunk: text_chunk(),
            },
            SessionUpdate::ToolCall { tool_call: tool() },
            SessionUpdate::ToolCallUpdate {
                tool_call: update(),
            },
            SessionUpdate::Plan {
                entries: vec![PlanEntry {
                    content: "do it".into(),
                    priority: "high".into(),
                    status: "pending".into(),
                }],
            },
            SessionUpdate::AvailableCommandsUpdate {
                available_commands: vec![AvailableCommand {
                    name: "help".into(),
                    description: "help".into(),
                    input: None,
                }],
            },
        ];
        for update in updates {
            let line = encode_line(&update).unwrap();
            let mut framer = LineFramer::default();
            let frame = framer.push(&line).pop().unwrap().unwrap();
            assert_eq!(
                serde_json::from_value::<SessionUpdate>(frame).unwrap(),
                update
            );
        }
    }

    #[test]
    fn line_framer_caps_an_endless_unterminated_line() {
        // An agent that streams bytes forever with no newline must not grow the
        // framer's buffer without bound; the buffer is capped and a warning is
        // emitted instead of an OOM.
        let mut framer = LineFramer::default();
        let chunk = vec![b'a'; 1024 * 1024];
        let mut warned = false;
        // Push ~2x the cap worth of no-newline bytes.
        for _ in 0..(2 * MAX_ACP_LINE_BYTES / chunk.len() + 1) {
            for frame in framer.push(&chunk) {
                assert!(frame.is_err(), "no complete frame can parse from garbage");
                warned = true;
            }
        }
        assert!(warned, "over-cap line must surface a warning frame");
    }

    #[test]
    fn line_framer_realigns_after_discarding_an_over_long_line() {
        // After discarding an over-long line, the next newline resynchronises
        // framing and a following valid frame still parses.
        let mut framer = LineFramer::default();
        let huge = vec![b'x'; MAX_ACP_LINE_BYTES + 16];
        let warnings = framer.push(&huge);
        assert!(warnings.iter().any(|f| f.is_err()), "cap must warn");
        // Terminate the discarded line, then send a real frame on the next line.
        let good = SessionUpdate::Plan { entries: vec![] };
        let mut tail = vec![b'\n'];
        tail.extend_from_slice(&encode_line(&good).unwrap());
        let frames = framer.push(&tail);
        let parsed = frames
            .into_iter()
            .find_map(|f| f.ok())
            .expect("valid frame after realignment");
        assert_eq!(
            serde_json::from_value::<SessionUpdate>(parsed).unwrap(),
            good
        );
    }

    #[test]
    fn sanitize_terminal_text_strips_control_but_keeps_newline_and_tab() {
        let dirty = "safe\x1b]52;c;paste\x07\ttab\nmore\x00end";
        let clean = sanitize_terminal_text(dirty);
        assert!(!clean.contains('\x1b'), "ESC must be stripped: {clean:?}");
        assert!(!clean.contains('\x07'), "BEL must be stripped: {clean:?}");
        assert!(!clean.contains('\x00'), "NUL must be stripped: {clean:?}");
        assert!(
            clean.contains('\t') && clean.contains('\n'),
            "keep \\t and \\n"
        );
    }

    #[test]
    fn summarize_update_is_readable_and_sanitized_not_debug_json() {
        let s = summarize_update(&SessionUpdate::AgentMessageChunk {
            chunk: ContentChunk {
                content: ContentBlock::Text {
                    text: "hi\x1bthere".into(),
                },
                message_id: None,
            },
        });
        assert!(s.starts_with("agent: "), "summary: {s}");
        assert!(!s.contains('\x1b'), "summary must be sanitized: {s:?}");
        assert!(!s.contains('{'), "summary must not be Debug JSON: {s:?}");
    }

    #[test]
    fn approve_fails_closed_when_no_allow_option_offered() {
        // Only reject-style options → approve must NOT pick one; it Cancels.
        let only_reject = vec![PermissionOption {
            option_id: "no".into(),
            name: "Reject".into(),
            kind: "reject_once".into(),
        }];
        assert_eq!(
            PermissionDecision::approve(&only_reject),
            PermissionDecision::Cancelled
        );
        // An allow option present → approve selects it.
        let with_allow = vec![
            PermissionOption {
                option_id: "no".into(),
                name: "Reject".into(),
                kind: "reject_once".into(),
            },
            PermissionOption {
                option_id: "yes".into(),
                name: "Allow".into(),
                kind: "allow_once".into(),
            },
        ];
        assert_eq!(
            PermissionDecision::approve(&with_allow),
            PermissionDecision::Selected {
                option_id: "yes".into()
            }
        );
    }
}
