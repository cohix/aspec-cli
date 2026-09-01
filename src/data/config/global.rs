//! Global configuration: `$HOME/.awman/config.json`.
//!
//! `AWMAN_CONFIG_HOME` overrides the location for tests and bespoke installs.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::data::config::env::{Env, EnvSnapshot};
use crate::data::config::repo::{
    validate_auth_refresh, ApiConfig, AuthRefreshConfig, RemoteConfig, SquadConfig,
};
use crate::data::error::DataError;

/// Behavior when a configured ACP launch is requested for an agent that does
/// not support ACP.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LaunchModeFallback {
    Stdio,
    #[default]
    Error,
}

/// Filename of the global config inside the resolved global directory.
pub const GLOBAL_CONFIG_FILENAME: &str = "config.json";

/// Subdirectory under `$HOME` that hosts global awman state.
pub const GLOBAL_CONFIG_HOME_SUBDIR: &str = ".awman";

/// Global configuration stored at `$HOME/.awman/config.json` (or `$AWMAN_CONFIG_HOME/config.json`).
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GlobalConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_scrollback_lines: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    #[serde(
        rename = "yoloDisallowedTools",
        skip_serializing_if = "Option::is_none"
    )]
    pub yolo_disallowed_tools: Option<Vec<String>>,
    #[serde(rename = "envPassthrough", default, skip_serializing)]
    pub legacy_env_passthrough: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<ApiConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub squad: Option<SquadConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlays: Option<Vec<String>>,
    #[serde(rename = "agentStuckTimeout", skip_serializing_if = "Option::is_none")]
    pub agent_stuck_timeout_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workers: Option<u8>,
    #[serde(rename = "baseImage", skip_serializing_if = "Option::is_none")]
    pub base_image: Option<String>,
    #[serde(
        rename = "maxConcurrentAgents",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_concurrent_agents: Option<usize>,
    #[serde(rename = "launchModeFallback", skip_serializing_if = "Option::is_none")]
    pub launch_mode_fallback: Option<LaunchModeFallback>,
    #[serde(rename = "authRefresh", skip_serializing_if = "Option::is_none")]
    pub auth_refresh: Option<AuthRefreshConfig>,
}

impl GlobalConfig {
    pub fn workers(&self) -> u8 {
        self.workers.unwrap_or(2)
    }

    /// Resolve the global config home directory. Honours `AWMAN_CONFIG_HOME` for
    /// tests and overrides; otherwise falls back to `$HOME/.awman`.
    pub fn home_dir() -> Result<PathBuf, DataError> {
        Self::home_dir_with(&Env::from_process())
    }

    /// Same as [`home_dir`] but reads env vars from the supplied snapshot.
    ///
    /// Precedence: `AWMAN_CONFIG_HOME` → `XDG_CONFIG_HOME/awman` → `$HOME/.awman`.
    pub fn home_dir_with(env: &EnvSnapshot) -> Result<PathBuf, DataError> {
        if let Some(home) = env.config_home() {
            return Ok(home);
        }
        if let Some(xdg) = env.xdg_config_home() {
            return Ok(xdg.join("awman"));
        }
        let home = dirs::home_dir().ok_or(DataError::HomeNotFound)?;
        Ok(home.join(GLOBAL_CONFIG_HOME_SUBDIR))
    }

    /// Resolve the global data home directory for non-config data (workflows,
    /// skills, worktrees, API state).
    ///
    /// Precedence: `AWMAN_CONFIG_HOME` → `XDG_DATA_HOME/awman` → `$HOME/.awman`.
    pub fn data_home_with(env: &EnvSnapshot) -> Result<PathBuf, DataError> {
        if let Some(home) = env.config_home() {
            return Ok(home);
        }
        if let Some(xdg) = env.xdg_data_home() {
            return Ok(xdg.join("awman"));
        }
        let home = dirs::home_dir().ok_or(DataError::HomeNotFound)?;
        Ok(home.join(GLOBAL_CONFIG_HOME_SUBDIR))
    }

    /// Resolve the global config file path.
    pub fn path() -> Result<PathBuf, DataError> {
        Self::path_with(&Env::from_process())
    }

    /// Same as [`path`] but reads env vars from the supplied snapshot.
    pub fn path_with(env: &EnvSnapshot) -> Result<PathBuf, DataError> {
        Ok(Self::home_dir_with(env)?.join(GLOBAL_CONFIG_FILENAME))
    }

    /// Load the global config from disk, returning defaults when absent.
    pub fn load() -> Result<Self, DataError> {
        Self::load_with(&Env::from_process())
    }

    /// Same as [`load`] but reads paths via the supplied env snapshot.
    pub fn load_with(env: &EnvSnapshot) -> Result<Self, DataError> {
        let path = Self::path_with(env)?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path).map_err(|e| DataError::io(&path, e))?;
        let cfg: Self =
            serde_json::from_str(&content).map_err(|e| DataError::config_parse(&path, e))?;
        if let Some(n) = cfg.max_concurrent_agents {
            if n < 1 {
                return Err(DataError::Other(
                    "maxConcurrentAgents must be >= 1".to_string(),
                ));
            }
        }
        if let Some(squad) = &cfg.squad {
            squad.validate()?;
        }
        if let Some(auth_refresh) = &cfg.auth_refresh {
            validate_auth_refresh(auth_refresh)?;
        }
        Ok(cfg)
    }

    /// Persist this config to disk, creating parent directories if needed.
    pub fn save(&self) -> Result<(), DataError> {
        self.save_with(&Env::from_process())
    }

    /// Same as [`save`] but reads paths via the supplied env snapshot.
    pub fn save_with(&self, env: &EnvSnapshot) -> Result<(), DataError> {
        let path = Self::path_with(env)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
        }
        let content = serde_json::to_string_pretty(self)
            .map_err(|e| DataError::ConfigSerialize { source: e })?;
        std::fs::write(&path, content).map_err(|e| DataError::io(&path, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::env::AWMAN_CONFIG_HOME;
    use crate::data::config::repo::{ApiConfig, RemoteConfig, SquadConfig};

    fn isolated_env(home_dir: &std::path::Path) -> EnvSnapshot {
        EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, home_dir.to_str().unwrap())])
    }

    #[test]
    fn load_missing_config_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let env = isolated_env(tmp.path());
        let cfg = GlobalConfig::load_with(&env).unwrap();
        assert_eq!(cfg, GlobalConfig::default());
        assert!(cfg.default_agent.is_none());
    }

    #[test]
    fn load_save_load_round_trip_is_byte_stable() {
        let tmp = tempfile::tempdir().unwrap();
        let env = isolated_env(tmp.path());

        let original = GlobalConfig {
            default_agent: Some("claude".to_string()),
            terminal_scrollback_lines: Some(8000),
            runtime: Some("docker".to_string()),
            yolo_disallowed_tools: Some(vec!["rm".to_string()]),
            legacy_env_passthrough: None,
            api: Some(ApiConfig {
                work_dirs: Some(vec!["/work".to_string()]),
                always_non_interactive: Some(true),
            }),
            squad: Some(SquadConfig {
                agents_to_models: Some(std::collections::HashMap::from([(
                    "claude".to_string(),
                    vec!["claude-opus-4-8".to_string()],
                )])),
                max_concurrent_evaluations: Some(2),
                default_leader: Some("claude::claude-opus-4-8".to_string()),
                guidance: Some(vec!["Keep changes focused.".to_string()]),
            }),
            remote: Some(RemoteConfig {
                default_addr: Some("http://localhost:7777".to_string()),
                saved_dirs: Some(vec!["/projects".to_string()]),
                default_api_key: Some("sekret".to_string()),
            }),
            overlays: None,
            agent_stuck_timeout_secs: Some(45),
            workers: None,
            base_image: None,
            max_concurrent_agents: Some(4),
            launch_mode_fallback: None,
            auth_refresh: None,
        };

        original.save_with(&env).unwrap();
        let reloaded = GlobalConfig::load_with(&env).unwrap();
        assert_eq!(original, reloaded);
    }

    #[test]
    fn load_rejects_max_concurrent_agents_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let env = isolated_env(tmp.path());
        let path = GlobalConfig::path_with(&env).unwrap();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, r#"{"maxConcurrentAgents": 0}"#).unwrap();

        let err = GlobalConfig::load_with(&env).unwrap_err();
        assert!(
            err.to_string().contains("maxConcurrentAgents must be >= 1"),
            "error must explain the >= 1 requirement, got: {err}"
        );
    }

    #[test]
    fn load_accepts_max_concurrent_agents_positive() {
        let tmp = tempfile::tempdir().unwrap();
        let env = isolated_env(tmp.path());
        let path = GlobalConfig::path_with(&env).unwrap();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, r#"{"maxConcurrentAgents": 8}"#).unwrap();

        let cfg = GlobalConfig::load_with(&env).unwrap();
        assert_eq!(cfg.max_concurrent_agents, Some(8));
    }

    #[test]
    fn load_rejects_invalid_squad_config() {
        let tmp = tempfile::tempdir().unwrap();
        let env = isolated_env(tmp.path());
        let path = GlobalConfig::path_with(&env).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"squad":{"maxConcurrentEvaluations":0}}"#).unwrap();

        let err = GlobalConfig::load_with(&env).unwrap_err();
        assert!(err
            .to_string()
            .contains("squad.maxConcurrentEvaluations must be >= 1"));
    }

    #[test]
    fn load_malformed_json_returns_config_parse_error() {
        let tmp = tempfile::tempdir().unwrap();
        let env = isolated_env(tmp.path());
        // Write a broken JSON file where the config would be.
        let path = GlobalConfig::path_with(&env).unwrap();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, b"{broken json").unwrap();

        let err = GlobalConfig::load_with(&env).unwrap_err();
        assert!(
            matches!(err, DataError::ConfigParse { .. }),
            "expected ConfigParse, got {err:?}"
        );
    }

    #[test]
    fn awman_config_home_overrides_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let env = isolated_env(tmp.path());
        let path = GlobalConfig::path_with(&env).unwrap();
        assert_eq!(path, tmp.path().join(GLOBAL_CONFIG_FILENAME));
    }

    #[test]
    fn home_dir_with_returns_awman_config_home_when_set() {
        let tmp = tempfile::tempdir().unwrap();
        let env = isolated_env(tmp.path());
        let home = GlobalConfig::home_dir_with(&env).unwrap();
        assert_eq!(home, tmp.path());
    }

    #[test]
    fn home_dir_with_returns_xdg_config_home_awman_when_set() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(
            crate::data::config::env::XDG_CONFIG_HOME,
            tmp.path().to_str().unwrap(),
        )]);
        let home = GlobalConfig::home_dir_with(&env).unwrap();
        assert_eq!(
            home,
            tmp.path().join("awman"),
            "XDG_CONFIG_HOME must produce <xdg>/awman as the config home"
        );
    }

    #[test]
    fn home_dir_with_awman_config_home_wins_over_xdg_config_home() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg_dir = tmp.path().join("xdg");
        let awman_dir = tmp.path().join("awman_override");
        let env = EnvSnapshot::with_overrides([
            (
                crate::data::config::env::AWMAN_CONFIG_HOME,
                awman_dir.to_str().unwrap(),
            ),
            (
                crate::data::config::env::XDG_CONFIG_HOME,
                xdg_dir.to_str().unwrap(),
            ),
        ]);
        let home = GlobalConfig::home_dir_with(&env).unwrap();
        assert_eq!(
            home, awman_dir,
            "AWMAN_CONFIG_HOME must win over XDG_CONFIG_HOME"
        );
    }
}
