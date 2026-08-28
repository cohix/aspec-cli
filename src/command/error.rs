//! Layer 2 error type — `CommandError`.
//!
//! Wraps `EngineError` (Layer 1) and `DataError` (Layer 0) for failures
//! bubbling up from below. Layer 3 wraps `CommandError` in its own
//! user-facing presentation; Layer 2 does not depend on Layer 3 errors.

use std::path::PathBuf;

use thiserror::Error;

use crate::data::error::DataError;
use crate::engine::error::EngineError;

#[derive(Debug, Error)]
pub enum CommandError {
    #[error(transparent)]
    Engine(#[from] EngineError),

    #[error(transparent)]
    Data(#[from] DataError),

    // ── Dispatch / catalogue ─────────────────────────────────────────────
    #[error("unknown command: {path:?}")]
    UnknownCommand { path: Vec<String> },

    #[error("command '{command}' is not available via the {frontend} frontend")]
    NotAvailableForFrontend { command: String, frontend: String },

    #[error("unknown flag '{flag}' for command {command:?}")]
    UnknownFlag { command: Vec<String>, flag: String },

    #[error("missing required flag '{flag}' for command {command:?}")]
    MissingRequiredFlag { command: Vec<String>, flag: String },

    #[error("missing required argument '{argument}' for command {command:?}")]
    MissingRequiredArgument {
        command: Vec<String>,
        argument: String,
    },

    #[error("unexpected argument '{argument}' for command {command:?}")]
    UnexpectedArgument {
        command: Vec<String>,
        argument: String,
    },

    #[error("flags '{a}' and '{b}' are mutually exclusive on {command:?}")]
    MutuallyExclusive {
        command: Vec<String>,
        a: String,
        b: String,
    },

    #[error("invalid value for flag '{flag}' on {command:?}: {reason}")]
    InvalidFlagValue {
        command: Vec<String>,
        flag: String,
        reason: String,
    },

    #[error("invalid value for argument '{argument}' on {command:?}: {reason}")]
    InvalidArgumentValue {
        command: Vec<String>,
        argument: String,
        reason: String,
    },

    // ── TUI command-box parsing ───────────────────────────────────────────
    #[error("could not parse command-box input: {0}")]
    CommandBoxParse(String),

    // ── Workflow / worktree lifecycle ─────────────────────────────────────
    #[error("command aborted by user")]
    Aborted,

    #[error("merge conflict on branch {branch} (worktree at {worktree_path})")]
    MergeConflict {
        branch: String,
        worktree_path: PathBuf,
    },

    // ── Remote command ────────────────────────────────────────────────────
    #[error("remote target address is missing or invalid")]
    MissingRemoteAddress,

    #[error("remote API key is missing")]
    MissingApiKey,

    #[error("remote request timed out")]
    RemoteTimeout,

    #[error("remote connection refused: {0}")]
    RemoteConnectionRefused(String),

    #[error("remote returned status {status}: {body}")]
    RemoteHttpStatus { status: u16, body: String },

    #[error("malformed SSE event from remote: {0}")]
    MalformedSseEvent(String),

    #[error("remote transport error: {0}")]
    RemoteTransport(String),

    // ── API ───────────────────────────────────────────────────────────────
    #[error("API workdir not found: {path}")]
    ApiWorkdirNotFound { path: PathBuf },

    #[error("API server already running on PID {pid}")]
    ApiServerAlreadyRunning { pid: u32 },

    #[error("API server is not running")]
    ApiServerNotRunning,

    #[error("no API key configured; run `awman api start --refresh-key` first, or pass `--dangerously-skip-auth`")]
    ApiServerAuthMissing,

    // ── Session creation (multi-session frontends: API, future desktop, …) ──
    // Typed so that a frontend can map each to the right transport status
    // (e.g. HTTP 400 vs 403) without inspecting the message string. Display
    // text is kept byte-identical to the previous inline API responses.
    #[error("session_type must be 'local' or 'remote'; got '{got}'")]
    SessionInvalidType { got: String },

    #[error("workdir is required when session_type is 'local'")]
    SessionWorkdirRequired,

    #[error("Cannot resolve path: {path}")]
    SessionWorkdirUnresolvable { path: String },

    #[error("Workdir '{requested}' is not in the allowlist. Allowed: {allowed:?}")]
    SessionWorkdirNotAllowed {
        requested: String,
        allowed: Vec<String>,
    },

    #[error("repo_url is required when session_type is 'remote'")]
    SessionRepoUrlRequired,

    #[error("repo_url must be non-empty")]
    SessionRepoUrlEmpty,

    #[error("repo_url must use http(s), ssh, or git scheme")]
    SessionRepoUrlInvalidScheme { url: String },

    // ── Remote ────────────────────────────────────────────────────────────
    #[error("no remote session id; pass --session <id> or run `awman remote session start`")]
    RemoteSessionMissing,

    #[error("failed to kill remote session '{session_id}': {reason}")]
    RemoteSessionKillFailed { session_id: String, reason: String },

    // ── Work item / spec ──────────────────────────────────────────────────────
    #[error("work item {number} not found in aspec/work-items/")]
    WorkItemNotFound { number: u32 },

    #[error("spec template missing at {path}; run `awman init --aspec` to create it")]
    SpecTemplateMissing { path: std::path::PathBuf },

    #[error("invalid overlay spec '{spec}': {reason}")]
    InvalidOverlaySpec { spec: String, reason: String },

    #[error("unknown config field '{name}'; similar fields: {suggestions}")]
    UnknownConfigField { name: String, suggestions: String },

    #[error("stdin is not a TTY; provide --{prompt} on the command line")]
    InteractiveInputUnavailable { prompt: String },

    #[error("workflow file not found: {path}")]
    WorkflowFileNotFound { path: std::path::PathBuf },

    // ── Catch-all ─────────────────────────────────────────────────────────
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),

    #[error("{0}")]
    Other(String),
}

impl CommandError {
    pub fn unknown_command(path: &[&str]) -> Self {
        CommandError::UnknownCommand {
            path: path.iter().map(|s| s.to_string()).collect(),
        }
    }

    pub fn missing_required_flag(command: &[&str], flag: impl Into<String>) -> Self {
        CommandError::MissingRequiredFlag {
            command: command.iter().map(|s| s.to_string()).collect(),
            flag: flag.into(),
        }
    }

    pub fn missing_required_argument(command: &[&str], argument: impl Into<String>) -> Self {
        CommandError::MissingRequiredArgument {
            command: command.iter().map(|s| s.to_string()).collect(),
            argument: argument.into(),
        }
    }

    pub fn unknown_flag(command: &[&str], flag: impl Into<String>) -> Self {
        CommandError::UnknownFlag {
            command: command.iter().map(|s| s.to_string()).collect(),
            flag: flag.into(),
        }
    }

    pub fn unexpected_argument(command: &[&str], argument: impl Into<String>) -> Self {
        CommandError::UnexpectedArgument {
            command: command.iter().map(|s| s.to_string()).collect(),
            argument: argument.into(),
        }
    }

    pub fn mutually_exclusive(
        command: &[&str],
        a: impl Into<String>,
        b: impl Into<String>,
    ) -> Self {
        CommandError::MutuallyExclusive {
            command: command.iter().map(|s| s.to_string()).collect(),
            a: a.into(),
            b: b.into(),
        }
    }
}
