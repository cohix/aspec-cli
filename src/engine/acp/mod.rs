//! ACP (Agent Client Protocol) support.
//!
//! This is a Layer 1 module.  It owns the JSON-RPC transport and the portable
//! session driver; command and frontend layers only supply launch plumbing and
//! an [`AcpFrontend`].

pub mod client;
pub mod frontend;
pub mod protocol;
pub mod session;

pub use client::{
    AcpClient, AcpTransport, AcpTransportFrontend, ContainerFileSystem, DenyHostFilesystem,
    IncomingRequest,
};
pub use frontend::AcpFrontend;
pub use protocol::{
    AvailableCommand, ContentBlock, InitializeRequest, InitializeResponse, JsonRpcId,
    PermissionDecision, PermissionOption, PermissionRequest, PlanEntry, PromptResponse,
    SessionUpdate, ToolCall, ToolCallUpdate,
};
pub use session::AcpSession;
