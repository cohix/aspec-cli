//! Filesystem and database concerns for awman.
//!
//! Every direct file or database access in Layer 0 is encapsulated in a typed
//! object here. Higher layers consume these types; they never call
//! `std::fs::*` or `rusqlite::*` directly.

pub mod api_command_log;
pub mod api_db;
pub mod api_paths;
pub mod auth_paths;
pub mod context_dirs;
pub mod daemon_guard;
pub mod daemon_paths;
pub mod daemon_process;
pub mod data_paths;
pub mod kit_paths;
pub mod log_dirs;
pub mod overlay_paths;
pub mod path_guard;
pub mod skill_dirs;
pub mod skill_library;
pub mod squad_paths;
pub mod task_store;
pub mod workflow_dirs;
pub mod workflow_state;

pub use api_db::{CommandRecord, SessionRecord, SqliteSessionStore};
pub use api_paths::ApiPaths;
pub use auth_paths::{AgentAuthPaths, AuthPathResolver};
pub use context_dirs::ContextDirResolver;
pub use daemon_guard::{AcquireError, DaemonGuard, DaemonKind};
pub use daemon_paths::DaemonPaths;
pub use daemon_process::{DaemonProcess, ServerMeta, Termination};
pub use data_paths::DataPaths;
pub use kit_paths::SandboxKitPaths;
pub use log_dirs::WorkflowLogPaths;
pub use overlay_paths::OverlayPathResolver;
pub use skill_dirs::SkillDirs;
pub use squad_paths::SquadPaths;
pub use task_store::{
    MountScope, Run, RunDetail, RunId, RunStatus, Task, TaskStatus, TaskStore, TaskWorkspace,
};
pub use workflow_dirs::WorkflowDirs;
pub use workflow_state::WorkflowStateStore;
