//! Frontend delegation for a portable ACP session UI.

use crate::engine::acp::protocol::{PermissionDecision, PermissionRequest, SessionUpdate};

/// Presentation and input needed by an ACP session.
///
/// The ACP driver intentionally calls no concrete CLI, TUI, or command type;
/// this trait is the complete portable UI boundary.
pub trait AcpFrontend: Send {
    fn render_update(&mut self, update: SessionUpdate);
    fn request_permission(&mut self, request: PermissionRequest) -> PermissionDecision;
    fn next_prompt(&mut self) -> Option<String>;
}
