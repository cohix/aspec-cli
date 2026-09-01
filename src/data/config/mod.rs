//! Configuration concerns for awman: per-repo config, global config, env-var
//! reads, typed flag values, and the merged effective view.

pub mod effective;
pub mod env;
pub mod flags;
pub mod global;
pub mod repo;

pub use effective::EffectiveConfig;
pub use env::{Env, EnvSnapshot};
pub use flags::FlagConfig;
pub use global::{GlobalConfig, LaunchModeFallback};
pub use repo::{
    AgentAuthMode, ApiConfig, DynamicWorkflowsConfig, LaunchMode, RemoteConfig, RepoConfig,
    SquadConfig, WorkItemsConfig, REPO_CONFIG_FILENAME, REPO_CONFIG_SUBDIR,
};

/// Built-in default number of scrollback lines for the container terminal emulator.
pub const DEFAULT_SCROLLBACK_LINES: usize = 10_000;

/// Built-in default seconds of inactivity before the agent is considered stuck.
pub const DEFAULT_AGENT_STUCK_TIMEOUT_SECS: u64 = 30;
