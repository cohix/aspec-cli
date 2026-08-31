//! Per-command frontend trait impls for the CLI.
//!
//! Most per-command frontend traits in `src/command/commands/` are pure
//! marker traits (e.g. `AuthCommandFrontend`, `ConfigCommandFrontend`)
//! whose only requirement is `UserMessageSink + Send + Sync`. Those are
//! satisfied by the umbrella impls in `command_frontend.rs`.
//!
//! The per-command modules in this directory carry the impls for the
//! richer traits — `Init`, `Ready`, `Chat`, `ExecPrompt`,
//! `ExecWorkflow`, `Api` — which require additional Q&A,
//! reporting, or container-frontend hooks.

pub(crate) mod helpers;
pub(crate) mod render;

pub(crate) mod amie;
pub(crate) mod amie_attach;

mod api_server;
mod chat;
mod clean;
mod exec_prompt;
mod exec_workflow;
mod init;
mod ready;

// Engine-level frontend trait impls used by multiple commands.
mod acp_frontend;
mod agent_auth;
mod agent_setup;
mod container_frontend_marker;
mod mount_scope;
mod workflow_frontend_marker;
mod worktree_lifecycle_marker;

pub(super) use container_frontend_marker::CliContainerProxy;
