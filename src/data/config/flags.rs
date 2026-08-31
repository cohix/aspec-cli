//! Typed flag values shared across the layered architecture.
//!
//! Frontends (Layer 3) parse user input into `FlagConfig` and pass it down
//! through Layer 2 to Layer 0 / Layer 1. The concrete `clap` definitions live
//! in Layer 2's `Dispatch`; this file only models the shape.

use std::path::PathBuf;
use std::time::Duration;

use crate::data::config::repo::LaunchMode;

/// Flag-derived overrides that Layer 0 honours when computing the effective config.
///
/// Every field is `None` when the user did not pass the corresponding flag.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FlagConfig {
    /// Override the working directory.
    pub working_dir: Option<PathBuf>,
    /// Override the agent name (`--agent <name>`).
    pub agent: Option<String>,
    /// Override the model (`--model <name>`).
    pub model: Option<String>,
    /// `--launch-mode <stdio|acp>`.
    pub launch_mode: Option<LaunchMode>,
    /// Override scrollback line count.
    pub terminal_scrollback_lines: Option<usize>,
    /// Override agent-stuck timeout.
    pub agent_stuck_timeout: Option<Duration>,
    /// `--yolo`: enable yolo mode.
    pub yolo: Option<bool>,
    /// `--auto`: enable auto-advance for workflows.
    pub auto: Option<bool>,
    /// `--non-interactive`: force non-interactive behavior.
    pub non_interactive: Option<bool>,
    /// `--yolo-disallowed-tool` / `--yolo-disallowed-tools`: tool denylist for yolo mode.
    pub yolo_disallowed_tools: Option<Vec<String>>,
    /// `--overlay <…>`: raw overlay specifications (parsed in higher layers).
    pub overlays_raw: Option<Vec<String>>,
    /// `--remote-addr <addr>`.
    pub remote_addr: Option<String>,
    /// `--remote-session <id>`.
    pub remote_session: Option<String>,
    /// `--api-key <key>`.
    pub api_key: Option<String>,
    /// `--work-item <N>`.
    pub work_item: Option<u32>,
    /// `--max-concurrent <N>`: cap on concurrently-running workflow steps.
    pub max_concurrent_agents: Option<usize>,
}

impl FlagConfig {
    /// Construct an empty flag set (all fields `None`).
    pub fn new() -> Self {
        Self::default()
    }
}
