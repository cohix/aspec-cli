//! Per-command frontend trait implementations for the TUI.
//!
//! Each file implements a single per-command frontend trait on
//! `TuiCommandFrontend`, following the same pattern as
//! `src/frontend/cli/per_command/`.

mod agent_auth;
mod agent_setup;
mod amie;
mod api_server;
mod auth;
mod chat;
mod clean;
mod config;
mod container_frontend;
mod download;
mod exec_prompt;
mod exec_workflow;
mod init;
mod mount_scope;
mod new;
mod ready;
pub(crate) mod remote;
mod specs;
mod status;
mod workflow_frontend;
mod worktree_lifecycle;

pub use container_frontend::TuiContainerProxy;
