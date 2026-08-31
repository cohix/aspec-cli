//! Merged configuration view: flags > env > repo > global > built-in default.
//!
//! Every legacy `effective_*` free function in `oldsrc/config/mod.rs` becomes a
//! method on `EffectiveConfig`. The merge precedence is encoded once, in this
//! module, and is the single source of truth.

use std::time::Duration;

use crate::data::config::env::EnvSnapshot;
use crate::data::config::flags::FlagConfig;
use crate::data::config::global::{GlobalConfig, LaunchModeFallback};
use crate::data::config::repo::{AgentAuthMode, LaunchMode, RepoConfig};
use crate::data::config::{DEFAULT_AGENT_STUCK_TIMEOUT_SECS, DEFAULT_SCROLLBACK_LINES};

/// Merged view of every configuration source, in precedence order.
///
/// The fields are intentionally `pub(crate)`-free; access goes through
/// dedicated methods so that callers cannot accidentally bypass the merge.
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    flags: FlagConfig,
    env: EnvSnapshot,
    repo: RepoConfig,
    global: GlobalConfig,
}

impl Default for EffectiveConfig {
    fn default() -> Self {
        Self::new(
            FlagConfig::default(),
            crate::data::config::env::EnvSnapshot::default(),
            RepoConfig::default(),
            GlobalConfig::default(),
        )
    }
}

impl EffectiveConfig {
    /// Construct a merged view from the four source layers.
    ///
    /// Precedence (highest → lowest): `flags` > `env` > `repo` > `global` > built-in.
    pub fn new(
        flags: FlagConfig,
        env: EnvSnapshot,
        repo: RepoConfig,
        global: GlobalConfig,
    ) -> Self {
        Self {
            flags,
            env,
            repo,
            global,
        }
    }

    pub fn flags(&self) -> &FlagConfig {
        &self.flags
    }

    pub fn env(&self) -> &EnvSnapshot {
        &self.env
    }

    pub fn repo(&self) -> &RepoConfig {
        &self.repo
    }

    pub fn global(&self) -> &GlobalConfig {
        &self.global
    }

    /// Resolve the model override (flag only; no repo/global level for model).
    pub fn model(&self) -> Option<String> {
        self.flags.model.clone()
    }

    /// Resolve the agent name (flag > repo.agent > global.default_agent).
    pub fn agent(&self) -> Option<String> {
        if let Some(a) = self.flags.agent.as_deref() {
            return Some(a.to_string());
        }
        if let Some(a) = self.repo.agent.as_deref() {
            return Some(a.to_string());
        }
        self.global.default_agent.clone()
    }

    /// Effective `yoloDisallowedTools` list.
    pub fn yolo_disallowed_tools(&self) -> Vec<String> {
        if let Some(values) = self.flags.yolo_disallowed_tools.as_ref() {
            return values.clone();
        }
        if let Some(values) = self.repo.yolo_disallowed_tools.as_ref() {
            return values.clone();
        }
        if let Some(values) = self.global.yolo_disallowed_tools.as_ref() {
            return values.clone();
        }
        Vec::new()
    }

    /// Effective scrollback line count for the container terminal.
    pub fn scrollback_lines(&self) -> usize {
        if let Some(n) = self.flags.terminal_scrollback_lines {
            return n;
        }
        if let Some(n) = self.repo.terminal_scrollback_lines {
            return n;
        }
        if let Some(n) = self.global.terminal_scrollback_lines {
            return n;
        }
        DEFAULT_SCROLLBACK_LINES
    }

    /// Effective agent-stuck timeout.
    pub fn agent_stuck_timeout(&self) -> Duration {
        if let Some(d) = self.flags.agent_stuck_timeout {
            return d;
        }
        if let Some(secs) = self.repo.agent_stuck_timeout_secs {
            return Duration::from_secs(secs);
        }
        if let Some(secs) = self.global.agent_stuck_timeout_secs {
            return Duration::from_secs(secs);
        }
        Duration::from_secs(DEFAULT_AGENT_STUCK_TIMEOUT_SECS)
    }

    /// Effective API work-dirs allowlist.
    pub fn api_work_dirs(&self) -> Vec<String> {
        if let Some(api_cfg) = self.global.api.as_ref() {
            if let Some(dirs) = api_cfg.work_dirs.as_ref() {
                return dirs.clone();
            }
        }
        Vec::new()
    }

    /// Effective `alwaysNonInteractive` setting.
    pub fn always_non_interactive(&self) -> bool {
        if let Some(value) = self.flags.non_interactive {
            return value;
        }
        self.global
            .api
            .as_ref()
            .and_then(|h| h.always_non_interactive)
            .unwrap_or(false)
    }

    /// Effective remote default address (flag > env > global config).
    pub fn remote_default_addr(&self) -> Option<String> {
        if let Some(v) = self.flags.remote_addr.as_deref() {
            return Some(v.to_string());
        }
        if let Some(v) = self.env.remote_addr() {
            return Some(v.to_string());
        }
        self.global
            .remote
            .as_ref()
            .and_then(|r| r.default_addr.clone())
    }

    /// Effective remote default API key (flag > env > global config).
    pub fn remote_default_api_key(&self) -> Option<String> {
        if let Some(v) = self.flags.api_key.as_deref() {
            return Some(v.to_string());
        }
        if let Some(v) = self.env.api_key() {
            return Some(v.to_string());
        }
        self.global
            .remote
            .as_ref()
            .and_then(|r| r.default_api_key.clone())
    }

    /// Effective remote saved-dirs list.
    pub fn remote_saved_dirs(&self) -> Vec<String> {
        self.global
            .remote
            .as_ref()
            .and_then(|r| r.saved_dirs.clone())
            .unwrap_or_default()
    }

    /// Effective sticky remote session id (flag > env).
    pub fn remote_session(&self) -> Option<String> {
        if let Some(v) = self.flags.remote_session.as_deref() {
            return Some(v.to_string());
        }
        self.env.remote_session().map(|s| s.to_string())
    }

    /// Effective container runtime name (e.g. `"docker"`, `"apple-containers"`).
    pub fn runtime(&self) -> Option<String> {
        self.global.runtime.clone()
    }

    /// Effective agent launch mode (flag > env > repo > built-in `stdio`).
    ///
    /// Launch mode is intentionally not inherited from global configuration;
    /// it is scoped to an invocation or repository. The repository setting is
    /// agent-independent, so no agent name is needed here (the agent-support
    /// check happens separately, in the command layer and `build_options`).
    pub fn launch_mode(&self) -> LaunchMode {
        if let Some(mode) = self.flags.launch_mode {
            return mode;
        }
        if let Some(mode) = self.env.launch_mode() {
            return mode;
        }
        self.repo.launch_mode.unwrap_or_default()
    }

    /// Effective ACP fallback policy. This is global-only; flags, environment
    /// and repository configuration do not override it.
    pub fn launch_mode_fallback(&self) -> LaunchModeFallback {
        self.global.launch_mode_fallback.unwrap_or_default()
    }

    /// Effective base image tag for setup/teardown containers (repo > global > None).
    pub fn base_image(&self) -> Option<String> {
        if let Some(v) = self.repo.base_image.as_deref() {
            return Some(v.to_string());
        }
        self.global.base_image.clone()
    }

    /// Effective cap on concurrently-running workflow steps
    /// (flags > env > repo > global > `None` = unlimited).
    pub fn effective_max_concurrent_agents(&self) -> Option<usize> {
        if let Some(n) = self.flags.max_concurrent_agents {
            return Some(n);
        }
        if let Some(n) = self.env.max_concurrent_agents() {
            return Some(n);
        }
        if let Some(n) = self.repo.max_concurrent_agents {
            return Some(n);
        }
        self.global.max_concurrent_agents
    }

    /// Effective credential injection mode (repo > built-in default `keychain`).
    ///
    /// `passthrough` skips keychain injection entirely; `none` disables keychain
    /// injection; `keychain` (default) injects host keychain creds, subject to
    /// injection-time dedup of any repo-declared env vars.
    ///
    /// Single-layer resolution (repo only, no global flag override) is a
    /// deliberate conservative choice: a credential toggle must be an explicit
    /// per-repo decision, not silently inherited from a global default that the
    /// operator may not realize is set.
    pub fn auth_mode(&self) -> AgentAuthMode {
        self.repo.auth.unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::env::{
        EnvSnapshot, AWMAN_API_KEY, AWMAN_REMOTE_ADDR, AWMAN_REMOTE_SESSION,
    };
    use crate::data::config::repo::{AgentAuthMode, ApiConfig, RemoteConfig};
    use std::time::Duration;

    fn make_effective(
        flags: FlagConfig,
        env: EnvSnapshot,
        repo: RepoConfig,
        global: GlobalConfig,
    ) -> EffectiveConfig {
        EffectiveConfig::new(flags, env, repo, global)
    }

    // ─── agent ────────────────────────────────────────────────────────────────

    #[test]
    fn agent_flag_beats_repo_and_global() {
        let flags = FlagConfig {
            agent: Some("flag-agent".to_string()),
            ..Default::default()
        };
        let repo = RepoConfig {
            agent: Some("repo-agent".to_string()),
            ..Default::default()
        };
        let global = GlobalConfig {
            default_agent: Some("global-agent".to_string()),
            ..Default::default()
        };
        let ec = make_effective(flags, EnvSnapshot::empty(), repo, global);
        assert_eq!(ec.agent().as_deref(), Some("flag-agent"));
    }

    #[test]
    fn agent_repo_beats_global() {
        let repo = RepoConfig {
            agent: Some("repo-agent".to_string()),
            ..Default::default()
        };
        let global = GlobalConfig {
            default_agent: Some("global-agent".to_string()),
            ..Default::default()
        };
        let ec = make_effective(FlagConfig::default(), EnvSnapshot::empty(), repo, global);
        assert_eq!(ec.agent().as_deref(), Some("repo-agent"));
    }

    #[test]
    fn agent_global_is_used_when_repo_unset() {
        let global = GlobalConfig {
            default_agent: Some("global-agent".to_string()),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert_eq!(ec.agent().as_deref(), Some("global-agent"));
    }

    #[test]
    fn agent_none_when_all_unset() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(ec.agent(), None);
    }

    // ─── scrollback_lines ─────────────────────────────────────────────────────

    #[test]
    fn scrollback_flag_beats_repo_and_global() {
        let flags = FlagConfig {
            terminal_scrollback_lines: Some(9999),
            ..Default::default()
        };
        let repo = RepoConfig {
            terminal_scrollback_lines: Some(5000),
            ..Default::default()
        };
        let global = GlobalConfig {
            terminal_scrollback_lines: Some(2000),
            ..Default::default()
        };
        let ec = make_effective(flags, EnvSnapshot::empty(), repo, global);
        assert_eq!(ec.scrollback_lines(), 9999);
    }

    #[test]
    fn scrollback_repo_beats_global() {
        let repo = RepoConfig {
            terminal_scrollback_lines: Some(5000),
            ..Default::default()
        };
        let global = GlobalConfig {
            terminal_scrollback_lines: Some(2000),
            ..Default::default()
        };
        let ec = make_effective(FlagConfig::default(), EnvSnapshot::empty(), repo, global);
        assert_eq!(ec.scrollback_lines(), 5000);
    }

    #[test]
    fn scrollback_global_beats_built_in_default() {
        let global = GlobalConfig {
            terminal_scrollback_lines: Some(3333),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert_eq!(ec.scrollback_lines(), 3333);
    }

    #[test]
    fn scrollback_built_in_default_is_10000() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(ec.scrollback_lines(), DEFAULT_SCROLLBACK_LINES);
        assert_eq!(ec.scrollback_lines(), 10_000);
    }

    // ─── agent_stuck_timeout ──────────────────────────────────────────────────

    #[test]
    fn timeout_flag_beats_repo_and_global() {
        let flags = FlagConfig {
            agent_stuck_timeout: Some(Duration::from_secs(999)),
            ..Default::default()
        };
        let repo = RepoConfig {
            agent_stuck_timeout_secs: Some(100),
            ..Default::default()
        };
        let global = GlobalConfig {
            agent_stuck_timeout_secs: Some(50),
            ..Default::default()
        };
        let ec = make_effective(flags, EnvSnapshot::empty(), repo, global);
        assert_eq!(ec.agent_stuck_timeout(), Duration::from_secs(999));
    }

    #[test]
    fn timeout_repo_beats_global() {
        let repo = RepoConfig {
            agent_stuck_timeout_secs: Some(77),
            ..Default::default()
        };
        let global = GlobalConfig {
            agent_stuck_timeout_secs: Some(50),
            ..Default::default()
        };
        let ec = make_effective(FlagConfig::default(), EnvSnapshot::empty(), repo, global);
        assert_eq!(ec.agent_stuck_timeout(), Duration::from_secs(77));
    }

    #[test]
    fn timeout_global_beats_built_in_default() {
        let global = GlobalConfig {
            agent_stuck_timeout_secs: Some(120),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert_eq!(ec.agent_stuck_timeout(), Duration::from_secs(120));
    }

    #[test]
    fn timeout_built_in_default_is_30s() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(
            ec.agent_stuck_timeout(),
            Duration::from_secs(DEFAULT_AGENT_STUCK_TIMEOUT_SECS)
        );
        assert_eq!(ec.agent_stuck_timeout(), Duration::from_secs(30));
    }

    // ─── yolo_disallowed_tools ────────────────────────────────────────────────

    #[test]
    fn yolo_disallowed_tools_flag_beats_repo() {
        let flags = FlagConfig {
            yolo_disallowed_tools: Some(vec!["flag-tool".to_string()]),
            ..Default::default()
        };
        let repo = RepoConfig {
            yolo_disallowed_tools: Some(vec!["repo-tool".to_string()]),
            ..Default::default()
        };
        let ec = make_effective(flags, EnvSnapshot::empty(), repo, GlobalConfig::default());
        assert_eq!(ec.yolo_disallowed_tools(), vec!["flag-tool"]);
    }

    #[test]
    fn yolo_disallowed_tools_repo_beats_global() {
        let repo = RepoConfig {
            yolo_disallowed_tools: Some(vec!["repo-tool".to_string()]),
            ..Default::default()
        };
        let global = GlobalConfig {
            yolo_disallowed_tools: Some(vec!["global-tool".to_string()]),
            ..Default::default()
        };
        let ec = make_effective(FlagConfig::default(), EnvSnapshot::empty(), repo, global);
        assert_eq!(ec.yolo_disallowed_tools(), vec!["repo-tool"]);
    }

    #[test]
    fn yolo_disallowed_tools_empty_when_all_unset() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert!(ec.yolo_disallowed_tools().is_empty());
    }

    // ─── remote_default_addr ──────────────────────────────────────────────────

    #[test]
    fn remote_addr_flag_beats_env_and_global() {
        let flags = FlagConfig {
            remote_addr: Some("flag-addr".to_string()),
            ..Default::default()
        };
        let env = EnvSnapshot::with_overrides([(AWMAN_REMOTE_ADDR, "env-addr")]);
        let global = GlobalConfig {
            remote: Some(RemoteConfig {
                default_addr: Some("global-addr".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ec = make_effective(flags, env, RepoConfig::default(), global);
        assert_eq!(ec.remote_default_addr().as_deref(), Some("flag-addr"));
    }

    #[test]
    fn remote_addr_env_beats_global() {
        let env = EnvSnapshot::with_overrides([(AWMAN_REMOTE_ADDR, "env-addr")]);
        let global = GlobalConfig {
            remote: Some(RemoteConfig {
                default_addr: Some("global-addr".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ec = make_effective(FlagConfig::default(), env, RepoConfig::default(), global);
        assert_eq!(ec.remote_default_addr().as_deref(), Some("env-addr"));
    }

    #[test]
    fn remote_addr_global_is_used_when_flag_and_env_unset() {
        let global = GlobalConfig {
            remote: Some(RemoteConfig {
                default_addr: Some("global-addr".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert_eq!(ec.remote_default_addr().as_deref(), Some("global-addr"));
    }

    #[test]
    fn remote_addr_none_when_all_unset() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(ec.remote_default_addr(), None);
    }

    // ─── remote_default_api_key ───────────────────────────────────────────────

    #[test]
    fn remote_api_key_flag_beats_env() {
        let flags = FlagConfig {
            api_key: Some("flag-key".to_string()),
            ..Default::default()
        };
        let env = EnvSnapshot::with_overrides([(AWMAN_API_KEY, "env-key")]);
        let ec = make_effective(flags, env, RepoConfig::default(), GlobalConfig::default());
        assert_eq!(ec.remote_default_api_key().as_deref(), Some("flag-key"));
    }

    #[test]
    fn remote_api_key_env_beats_global() {
        let env = EnvSnapshot::with_overrides([(AWMAN_API_KEY, "env-key")]);
        let global = GlobalConfig {
            remote: Some(RemoteConfig {
                default_api_key: Some("global-key".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ec = make_effective(FlagConfig::default(), env, RepoConfig::default(), global);
        assert_eq!(ec.remote_default_api_key().as_deref(), Some("env-key"));
    }

    // ─── remote_session ───────────────────────────────────────────────────────

    #[test]
    fn remote_session_flag_beats_env() {
        let flags = FlagConfig {
            remote_session: Some("flag-session".to_string()),
            ..Default::default()
        };
        let env = EnvSnapshot::with_overrides([(AWMAN_REMOTE_SESSION, "env-session")]);
        let ec = make_effective(flags, env, RepoConfig::default(), GlobalConfig::default());
        assert_eq!(ec.remote_session().as_deref(), Some("flag-session"));
    }

    #[test]
    fn remote_session_from_env_when_flag_unset() {
        let env = EnvSnapshot::with_overrides([(AWMAN_REMOTE_SESSION, "env-session")]);
        let ec = make_effective(
            FlagConfig::default(),
            env,
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(ec.remote_session().as_deref(), Some("env-session"));
    }

    #[test]
    fn remote_session_none_when_both_unset() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(ec.remote_session(), None);
    }

    // ─── always_non_interactive ───────────────────────────────────────────────

    #[test]
    fn always_non_interactive_flag_wins() {
        let flags = FlagConfig {
            non_interactive: Some(true),
            ..Default::default()
        };
        let global = GlobalConfig {
            api: Some(ApiConfig {
                always_non_interactive: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ec = make_effective(flags, EnvSnapshot::empty(), RepoConfig::default(), global);
        assert!(ec.always_non_interactive());
    }

    #[test]
    fn always_non_interactive_from_global_when_flag_unset() {
        let global = GlobalConfig {
            api: Some(ApiConfig {
                always_non_interactive: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert!(ec.always_non_interactive());
    }

    #[test]
    fn always_non_interactive_default_is_false() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert!(!ec.always_non_interactive());
    }

    // ─── api_work_dirs ───────────────────────────────────────────────────

    #[test]
    fn api_work_dirs_from_global() {
        let global = GlobalConfig {
            api: Some(ApiConfig {
                work_dirs: Some(vec!["/data".to_string(), "/work".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert_eq!(ec.api_work_dirs(), vec!["/data", "/work"]);
    }

    #[test]
    fn api_work_dirs_empty_when_not_set() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert!(ec.api_work_dirs().is_empty());
    }

    // ─── runtime ─────────────────────────────────────────────────────────────

    #[test]
    fn runtime_from_global() {
        let global = GlobalConfig {
            runtime: Some("podman".to_string()),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert_eq!(ec.runtime().as_deref(), Some("podman"));
    }

    #[test]
    fn runtime_none_when_not_set() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(ec.runtime(), None);
    }

    // ─── full-stack precedence tests ─────────────────────────────────────────

    #[test]
    fn full_stack_agent_precedence_flag_beats_repo_beats_global_beats_none() {
        let flags = FlagConfig {
            agent: Some("flag-agent".to_string()),
            ..Default::default()
        };
        let repo = RepoConfig {
            agent: Some("repo-agent".to_string()),
            ..Default::default()
        };
        let global = GlobalConfig {
            default_agent: Some("global-agent".to_string()),
            ..Default::default()
        };

        // Flag wins over all.
        let ec = make_effective(
            flags.clone(),
            EnvSnapshot::empty(),
            repo.clone(),
            global.clone(),
        );
        assert_eq!(
            ec.agent().as_deref(),
            Some("flag-agent"),
            "flag should beat repo and global"
        );

        // Remove flag → repo wins.
        let ec2 = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            repo.clone(),
            global.clone(),
        );
        assert_eq!(
            ec2.agent().as_deref(),
            Some("repo-agent"),
            "repo should beat global"
        );

        // Remove repo → global wins.
        let ec3 = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert_eq!(
            ec3.agent().as_deref(),
            Some("global-agent"),
            "global used when flag and repo absent"
        );

        // Remove all → None.
        let ec4 = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(ec4.agent(), None, "None when nothing is set");
    }

    #[test]
    fn full_stack_flag_wins_over_all_levels_for_scrollback() {
        // Set scrollback at every level; flag must win.
        let flags = FlagConfig {
            terminal_scrollback_lines: Some(1111),
            ..Default::default()
        };
        let repo = RepoConfig {
            terminal_scrollback_lines: Some(2222),
            ..Default::default()
        };
        let global = GlobalConfig {
            terminal_scrollback_lines: Some(3333),
            ..Default::default()
        };
        let ec = make_effective(flags, EnvSnapshot::empty(), repo, global);
        assert_eq!(ec.scrollback_lines(), 1111);

        // Remove flag — repo wins.
        let flags2 = FlagConfig::default();
        let repo2 = RepoConfig {
            terminal_scrollback_lines: Some(2222),
            ..Default::default()
        };
        let global2 = GlobalConfig {
            terminal_scrollback_lines: Some(3333),
            ..Default::default()
        };
        let ec2 = make_effective(flags2, EnvSnapshot::empty(), repo2, global2);
        assert_eq!(ec2.scrollback_lines(), 2222);

        // Remove repo — global wins.
        let ec3 = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig {
                terminal_scrollback_lines: Some(3333),
                ..Default::default()
            },
        );
        assert_eq!(ec3.scrollback_lines(), 3333);

        // Remove global — built-in default wins.
        let ec4 = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(ec4.scrollback_lines(), DEFAULT_SCROLLBACK_LINES);
    }

    // ─── effective_max_concurrent_agents (WI-0096) ──────────────────────────

    fn env_with_max_concurrent(n: &str) -> EnvSnapshot {
        EnvSnapshot::with_overrides([(crate::data::config::env::AWMAN_MAX_CONCURRENT_AGENTS, n)])
    }

    #[test]
    fn max_concurrent_flag_beats_env_repo_and_global() {
        let flags = FlagConfig {
            max_concurrent_agents: Some(1),
            ..Default::default()
        };
        let repo = RepoConfig {
            max_concurrent_agents: Some(3),
            ..Default::default()
        };
        let global = GlobalConfig {
            max_concurrent_agents: Some(4),
            ..Default::default()
        };
        let ec = make_effective(flags, env_with_max_concurrent("2"), repo, global);
        assert_eq!(ec.effective_max_concurrent_agents(), Some(1));
    }

    #[test]
    fn max_concurrent_env_beats_repo_and_global() {
        let repo = RepoConfig {
            max_concurrent_agents: Some(3),
            ..Default::default()
        };
        let global = GlobalConfig {
            max_concurrent_agents: Some(4),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            env_with_max_concurrent("2"),
            repo,
            global,
        );
        assert_eq!(ec.effective_max_concurrent_agents(), Some(2));
    }

    #[test]
    fn max_concurrent_repo_beats_global() {
        let repo = RepoConfig {
            max_concurrent_agents: Some(3),
            ..Default::default()
        };
        let global = GlobalConfig {
            max_concurrent_agents: Some(4),
            ..Default::default()
        };
        let ec = make_effective(FlagConfig::default(), EnvSnapshot::empty(), repo, global);
        assert_eq!(ec.effective_max_concurrent_agents(), Some(3));
    }

    #[test]
    fn max_concurrent_global_used_when_repo_env_flag_unset() {
        let global = GlobalConfig {
            max_concurrent_agents: Some(4),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert_eq!(ec.effective_max_concurrent_agents(), Some(4));
    }

    #[test]
    fn max_concurrent_none_when_all_unset_means_unlimited() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(ec.effective_max_concurrent_agents(), None);
    }

    // ── Part B: auth_mode ─────────────────────────────────────────────────────

    #[test]
    fn auth_mode_default_is_keychain() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(
            ec.auth_mode(),
            AgentAuthMode::Keychain,
            "default auth mode must be keychain"
        );
    }

    #[test]
    fn auth_mode_passthrough_from_repo() {
        let repo = RepoConfig {
            auth: Some(AgentAuthMode::Passthrough),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            repo,
            GlobalConfig::default(),
        );
        assert_eq!(
            ec.auth_mode(),
            AgentAuthMode::Passthrough,
            "passthrough set in repo config must be effective"
        );
    }

    #[test]
    fn auth_mode_none_from_repo() {
        let repo = RepoConfig {
            auth: Some(AgentAuthMode::None),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            repo,
            GlobalConfig::default(),
        );
        assert_eq!(
            ec.auth_mode(),
            AgentAuthMode::None,
            "none set in repo config must be effective"
        );
    }

    #[test]
    fn auth_mode_keychain_explicitly_set_in_repo() {
        let repo = RepoConfig {
            auth: Some(AgentAuthMode::Keychain),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            repo,
            GlobalConfig::default(),
        );
        assert_eq!(ec.auth_mode(), AgentAuthMode::Keychain);
    }

    // ── launch_mode ──────────────────────────────────────────────────────────

    #[test]
    fn launch_mode_defaults_to_stdio_and_ignores_global_config() {
        let global = GlobalConfig {
            launch_mode_fallback: Some(LaunchModeFallback::Stdio),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert_eq!(ec.launch_mode(), LaunchMode::Stdio);
    }

    #[test]
    fn launch_mode_uses_repo_when_flag_and_env_are_unset() {
        let repo = RepoConfig {
            launch_mode: Some(LaunchMode::Acp),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            repo,
            GlobalConfig::default(),
        );
        assert_eq!(ec.launch_mode(), LaunchMode::Acp);
    }

    #[test]
    fn launch_mode_env_beats_repo() {
        let env =
            EnvSnapshot::with_overrides([(crate::data::config::env::AWMAN_LAUNCH_MODE, "stdio")]);
        let repo = RepoConfig {
            launch_mode: Some(LaunchMode::Acp),
            ..Default::default()
        };
        let ec = make_effective(FlagConfig::default(), env, repo, GlobalConfig::default());
        assert_eq!(ec.launch_mode(), LaunchMode::Stdio);
    }

    #[test]
    fn launch_mode_flag_beats_env_and_repo() {
        let flags = FlagConfig {
            launch_mode: Some(LaunchMode::Acp),
            ..Default::default()
        };
        let env =
            EnvSnapshot::with_overrides([(crate::data::config::env::AWMAN_LAUNCH_MODE, "stdio")]);
        let repo = RepoConfig {
            launch_mode: Some(LaunchMode::Stdio),
            ..Default::default()
        };
        let ec = make_effective(flags, env, repo, GlobalConfig::default());
        assert_eq!(ec.launch_mode(), LaunchMode::Acp);
    }

    // ── launch_mode_fallback ─────────────────────────────────────────────────

    #[test]
    fn launch_mode_fallback_defaults_to_error() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(ec.launch_mode_fallback(), LaunchModeFallback::Error);
    }

    #[test]
    fn launch_mode_fallback_uses_global_config_only() {
        let global = GlobalConfig {
            launch_mode_fallback: Some(LaunchModeFallback::Stdio),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert_eq!(ec.launch_mode_fallback(), LaunchModeFallback::Stdio);
    }
}
