//! Per-repository configuration: `<git_root>/.awman/config.json`.
//!
//! Schema parity with the legacy `RepoConfig` (`oldsrc/config/mod.rs`) is
//! preserved for forward and backward compatibility — users upgrading from a
//! prior release must continue to read their existing files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::data::error::DataError;

/// Optional live credential-refresh settings. This intentionally remains a
/// sibling of the scalar `auth` setting so existing configuration files keep
/// their wire shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AuthRefreshConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold_minutes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tick_seconds: Option<u64>,
}

pub(crate) fn validate_auth_refresh(config: &AuthRefreshConfig) -> Result<(), DataError> {
    if config.threshold_minutes == Some(0) {
        return Err(DataError::Other(
            "authRefresh.thresholdMinutes must be >= 1".to_string(),
        ));
    }
    if config.tick_seconds == Some(0) {
        return Err(DataError::Other(
            "authRefresh.tickSeconds must be >= 1".to_string(),
        ));
    }
    Ok(())
}

/// Subdirectory under the git root in which awman stores per-repo state.
pub const REPO_CONFIG_SUBDIR: &str = ".awman";

/// Filename of the per-repo config inside `REPO_CONFIG_SUBDIR`.
pub const REPO_CONFIG_FILENAME: &str = "config.json";

/// Remote-mode configuration nested inside `GlobalConfig`.
///
/// Lives in `repo.rs` per the work-item layout even though it is consumed
/// by `GlobalConfig`; the entire family of config structs is grouped together.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteConfig {
    #[serde(rename = "defaultAddr", skip_serializing_if = "Option::is_none")]
    pub default_addr: Option<String>,
    #[serde(rename = "savedDirs", skip_serializing_if = "Option::is_none")]
    pub saved_dirs: Option<Vec<String>>,
    #[serde(rename = "defaultAPIKey", skip_serializing_if = "Option::is_none")]
    pub default_api_key: Option<String>,
}

/// API server configuration nested inside `GlobalConfig`.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiConfig {
    #[serde(rename = "workDirs", skip_serializing_if = "Option::is_none")]
    pub work_dirs: Option<Vec<String>>,
    #[serde(
        rename = "alwaysNonInteractive",
        skip_serializing_if = "Option::is_none"
    )]
    pub always_non_interactive: Option<bool>,
}

/// Squad daemon configuration nested inside [`GlobalConfig`](crate::data::config::GlobalConfig).
///
/// It lives beside the other shared configuration structs even though it is
/// global rather than repository-scoped.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SquadConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents_to_models: Option<HashMap<String, Vec<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrent_evaluations: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_leader: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guidance: Option<Vec<String>>,
}

impl SquadConfig {
    pub fn validate(&self) -> Result<(), DataError> {
        if let Some(n) = self.max_concurrent_evaluations {
            if n < 1 {
                return Err(DataError::Other(
                    "squad.maxConcurrentEvaluations must be >= 1".to_string(),
                ));
            }
        }
        if let Some(leader) = &self.default_leader {
            validate_leader_spec("squad", leader)?;
        }
        if let Some(map) = &self.agents_to_models {
            validate_agents_to_models("squad", map)?;
        }
        if let Some(entries) = &self.guidance {
            validate_guidance("squad", entries)?;
        }
        Ok(())
    }

    pub fn max_concurrent_evaluations_or_default(&self) -> usize {
        self.max_concurrent_evaluations.unwrap_or(2)
    }
}

/// Per-repo credential injection mode.
///
/// Controls how awman injects credentials into agent containers.  The default
/// (`keychain`) injects whatever the host keychain resolves, subject to
/// injection-time deduplication (Part A).  `passthrough` skips keychain
/// injection entirely — the harness must supply credentials via `env(VAR)`
/// overlays.  `none` disables KEYCHAIN credential injection; declared `env()`
/// overlays still apply.
///
/// Resolved through `EffectiveConfig::auth_mode()`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentAuthMode {
    /// Inject host keychain credentials (with injection-time dedup). Default.
    #[default]
    Keychain,
    /// Skip KEYCHAIN injection; rely on repo-declared `env()` overlays.
    /// Declared `env()` overlays still apply.
    Passthrough,
    /// No KEYCHAIN credential injection; declared `env()` overlays still apply.
    None,
}

/// How an agent process is connected to awman.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LaunchMode {
    #[default]
    Stdio,
    Acp,
}

/// Work-items configuration nested within `RepoConfig`.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkItemsConfig {
    /// Path to the work items directory (relative to repo root, or absolute).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// Path to the work item template file (relative to repo root, or absolute).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
}

/// Dynamic-workflow configuration nested within `RepoConfig` (WI-0095).
///
/// Lets teams pin which agents/models a dynamic workflow's leader may schedule,
/// cap concurrent steps, and set a default leader — all via version-controlled
/// repo config rather than per-run CLI flags. Layer 0 owns schema-shape
/// validation (see [`DynamicWorkflowsConfig::validate`]); Dockerfile-availability
/// validation lives in the command layer.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DynamicWorkflowsConfig {
    /// agent name → list of model name strings available for that agent
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents_to_models: Option<HashMap<String, Vec<String>>>,
    /// advisory cap on concurrent workflow steps (None = unlimited)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrent_steps: Option<usize>,
    /// default leader spec in agent::model format; overridden by --leader flag
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_leader: Option<String>,
    /// project-specific instructions the leader agent must follow when
    /// generating a workflow file; rendered as a bullet list in the leader
    /// prompt (WI-0099). `None` or `[]` injects no guidance block.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guidance: Option<Vec<String>>,
}

/// Maximum number of `dynamicWorkflows.guidance` entries. Caps prompt bloat
/// (WI-0099).
pub const GUIDANCE_MAX_ENTRIES: usize = 50;

/// Maximum length of a single `dynamicWorkflows.guidance` entry, in chars.
/// Prevents one instruction from dominating the leader prompt (WI-0099).
pub const GUIDANCE_MAX_ENTRY_LEN: usize = 1000;

impl DynamicWorkflowsConfig {
    /// Layer 0 semantic validation, run by [`RepoConfig::load`] after
    /// deserialization. Enforces the schema-local invariants that serde cannot
    /// express; returns [`DataError::Other`] with a user-facing message on the
    /// first violation. Dockerfile-availability checks are intentionally *not*
    /// here — they depend on filesystem discovery and belong to the command
    /// layer.
    pub fn validate(&self) -> Result<(), DataError> {
        if let Some(n) = self.max_concurrent_steps {
            if n < 1 {
                return Err(DataError::Other(
                    "dynamicWorkflows.maxConcurrentSteps must be >= 1".to_string(),
                ));
            }
        }
        if let Some(leader) = &self.default_leader {
            validate_leader_spec("dynamicWorkflows", leader)?;
        }
        if let Some(map) = &self.agents_to_models {
            validate_agents_to_models("dynamicWorkflows", map)?;
        }
        if let Some(entries) = &self.guidance {
            validate_guidance("dynamicWorkflows", entries)?;
        }
        Ok(())
    }
}

/// Validate a `dynamicWorkflows.agentsToModels` key against the same lexical
/// rules as [`crate::data::session::AgentName`] (ASCII alphanumerics, `-`, `_`,
/// length 1..=64). Returns [`DataError::Other`] so the failure surfaces as a
/// config-load error rather than a synthesized parse error.
pub(crate) fn validate_agent_name_key(field_path: &str, key: &str) -> Result<(), DataError> {
    crate::data::session::AgentName::new(key).map_err(|_| {
        DataError::Other(format!(
            "{field_path}.agentsToModels key {key:?} is not a valid agent name: only ASCII \
             alphanumerics, '-', and '_' are allowed, length 1..=64"
        ))
    })?;
    Ok(())
}

/// Layer 0 validator for `dynamicWorkflows.defaultLeader`. Enforces the same
/// `agent::model` syntax as `LeaderSpec::parse` (two non-empty components) plus
/// no leading/trailing whitespace in either component and an `AgentName`-valid
/// agent component. Kept in the data layer so the config fails fast at load
/// without depending on the command layer's `LeaderSpec`.
pub(crate) fn validate_leader_spec(field_path: &str, raw: &str) -> Result<(), DataError> {
    let parts: Vec<&str> = raw.split("::").collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(DataError::Other(format!(
            "{field_path}.defaultLeader {raw:?} is invalid; expected agent::model \
             (e.g. claude::claude-opus-4-8)"
        )));
    }
    let (agent, model) = (parts[0], parts[1]);
    if agent != agent.trim() || model != model.trim() {
        return Err(DataError::Other(format!(
            "{field_path}.defaultLeader {raw:?} must not have leading or trailing whitespace \
             in the agent or model component"
        )));
    }
    if crate::data::session::AgentName::new(agent).is_err() {
        return Err(DataError::Other(format!(
            "{field_path}.defaultLeader agent component {agent:?} is not a valid agent name: \
             only ASCII alphanumerics, '-', and '_' are allowed, length 1..=64"
        )));
    }
    Ok(())
}

pub(crate) fn validate_agents_to_models(
    field_path: &str,
    map: &HashMap<String, Vec<String>>,
) -> Result<(), DataError> {
    for (agent, models) in map {
        validate_agent_name_key(field_path, agent)?;
        if models.is_empty() {
            return Err(DataError::Other(format!(
                "{field_path}.agentsToModels.{agent} has an empty model list. \
                 Provide at least one model name."
            )));
        }
        for model in models {
            if model.trim().is_empty() {
                return Err(DataError::Other(format!(
                    "{field_path}.agentsToModels.{agent} contains an empty model name."
                )));
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_guidance(field_path: &str, entries: &[String]) -> Result<(), DataError> {
    if entries.len() > GUIDANCE_MAX_ENTRIES {
        return Err(DataError::Other(format!(
            "{field_path}.guidance has {} entries; the maximum is {GUIDANCE_MAX_ENTRIES}.",
            entries.len()
        )));
    }
    for (i, entry) in entries.iter().enumerate() {
        if entry.trim().is_empty() {
            return Err(DataError::Other(format!(
                "{field_path}.guidance[{i}] is empty or whitespace-only. \
                 Remove it or provide a non-empty instruction."
            )));
        }
        if entry.chars().count() > GUIDANCE_MAX_ENTRY_LEN {
            return Err(DataError::Other(format!(
                "{field_path}.guidance[{i}] is {} characters; the maximum is \
                 {GUIDANCE_MAX_ENTRY_LEN}.",
                entry.chars().count()
            )));
        }
    }
    Ok(())
}

/// Per-repository configuration stored at `<git_root>/.awman/config.json`.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_agent_auth_accepted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_scrollback_lines: Option<usize>,
    #[serde(
        rename = "yoloDisallowedTools",
        skip_serializing_if = "Option::is_none"
    )]
    pub yolo_disallowed_tools: Option<Vec<String>>,
    #[serde(rename = "envPassthrough", default, skip_serializing)]
    pub legacy_env_passthrough: Option<Vec<String>>,
    #[serde(rename = "workItems", skip_serializing_if = "Option::is_none")]
    pub work_items: Option<WorkItemsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlays: Option<Vec<String>>,
    #[serde(rename = "agentStuckTimeout", skip_serializing_if = "Option::is_none")]
    pub agent_stuck_timeout_secs: Option<u64>,
    #[serde(rename = "baseImage", skip_serializing_if = "Option::is_none")]
    pub base_image: Option<String>,
    #[serde(rename = "dockerfile", skip_serializing_if = "Option::is_none")]
    pub dockerfile: Option<String>,
    #[serde(
        rename = "dynamicWorkflows",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub dynamic_workflows: Option<DynamicWorkflowsConfig>,
    #[serde(
        rename = "maxConcurrentAgents",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_concurrent_agents: Option<usize>,
    /// Credential injection mode for agent containers. See [`AgentAuthMode`].
    /// Resolved per-repo; `keychain` (default) injects host keychain creds.
    #[serde(rename = "auth", skip_serializing_if = "Option::is_none")]
    pub auth: Option<AgentAuthMode>,
    #[serde(rename = "authRefresh", skip_serializing_if = "Option::is_none")]
    pub auth_refresh: Option<AuthRefreshConfig>,
    #[serde(rename = "launchMode", skip_serializing_if = "Option::is_none")]
    pub launch_mode: Option<LaunchMode>,
}

impl RepoConfig {
    /// Path to the per-repo config under a git root.
    pub fn path(git_root: &Path) -> PathBuf {
        git_root.join(REPO_CONFIG_SUBDIR).join(REPO_CONFIG_FILENAME)
    }

    /// Load the repo config from disk.
    ///
    /// Returns `RepoConfig::default()` when no file is present.
    /// Returns `DataError::ConfigParse` when the file is present but malformed.
    pub fn load(git_root: &Path) -> Result<Self, DataError> {
        let path = Self::path(git_root);
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path).map_err(|e| DataError::io(&path, e))?;
        let cfg: Self =
            serde_json::from_str(&content).map_err(|e| DataError::config_parse(&path, e))?;
        // Layer 0 semantic validation of the dynamicWorkflows section (WI-0095):
        // schema shape that serde cannot express is enforced here so malformed
        // config fails at load, before any UI or workflow starts.
        if let Some(dw) = &cfg.dynamic_workflows {
            dw.validate()?;
        }
        if let Some(n) = cfg.max_concurrent_agents {
            if n < 1 {
                return Err(DataError::Other(
                    "maxConcurrentAgents must be >= 1".to_string(),
                ));
            }
        }
        if let Some(auth_refresh) = &cfg.auth_refresh {
            validate_auth_refresh(auth_refresh)?;
        }
        Ok(cfg)
    }

    /// Persist this config to disk, creating parent directories if needed.
    pub fn save(&self, git_root: &Path) -> Result<(), DataError> {
        let path = Self::path(git_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
        }
        let content = serde_json::to_string_pretty(self)
            .map_err(|e| DataError::ConfigSerialize { source: e })?;
        std::fs::write(&path, content).map_err(|e| DataError::io(&path, e))
    }

    /// Resolve the configured work items directory relative to `git_root`.
    pub fn work_items_dir(&self, git_root: &Path) -> Option<PathBuf> {
        let dir = self.work_items.as_ref()?.dir.as_deref()?;
        if dir.is_empty() {
            return None;
        }
        let p = Path::new(dir);
        if p.is_absolute() {
            Some(p.to_path_buf())
        } else {
            Some(git_root.join(p))
        }
    }

    /// Resolve the configured work item template path relative to `git_root`.
    pub fn work_items_template(&self, git_root: &Path) -> Option<PathBuf> {
        let tmpl = self.work_items.as_ref()?.template.as_deref()?;
        if tmpl.is_empty() {
            return None;
        }
        let p = Path::new(tmpl);
        if p.is_absolute() {
            Some(p.to_path_buf())
        } else {
            Some(git_root.join(p))
        }
    }

    /// Resolve the work items directory, falling back to `<git_root>/aspec/work-items/`.
    pub fn work_items_dir_or_default(&self, git_root: &Path) -> PathBuf {
        self.work_items_dir(git_root)
            .unwrap_or_else(|| git_root.join("aspec").join("work-items"))
    }

    /// Resolve the work item template path, falling back to `<work_items_dir>/0000-template.md`.
    pub fn work_items_template_or_default(&self, git_root: &Path) -> PathBuf {
        self.work_items_template(git_root).unwrap_or_else(|| {
            self.work_items_dir_or_default(git_root)
                .join("0000-template.md")
        })
    }

    /// Replace the `workItems` config block. The chained `save(git_root)` call
    /// persists the change. Pass `None` to clear the block entirely.
    pub fn set_work_items_config(&mut self, cfg: Option<WorkItemsConfig>) {
        self.work_items = cfg;
    }

    /// Resolve the configured Dockerfile path relative to `git_root`.
    /// Returns `None` when the field is absent or empty.
    pub fn dockerfile_path(&self, git_root: &Path) -> Option<PathBuf> {
        let df = self.dockerfile.as_deref()?;
        if df.is_empty() {
            return None;
        }
        let p = Path::new(df);
        if p.is_absolute() {
            Some(p.to_path_buf())
        } else {
            Some(git_root.join(p))
        }
    }

    /// Resolve the configured Dockerfile path, falling back to `<git_root>/Dockerfile.dev`.
    pub fn dockerfile_path_or_default(&self, git_root: &Path) -> PathBuf {
        self.dockerfile_path(git_root)
            .unwrap_or_else(|| git_root.join("Dockerfile.dev"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_git_root() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn load_missing_config_returns_default() {
        let tmp = make_git_root();
        let cfg = RepoConfig::load(tmp.path()).unwrap();
        assert_eq!(cfg, RepoConfig::default());
        assert!(cfg.agent.is_none());
    }

    #[test]
    fn load_save_load_round_trip_is_byte_stable() {
        let tmp = make_git_root();
        let original = RepoConfig {
            agent: Some("claude".to_string()),
            terminal_scrollback_lines: Some(5000),
            yolo_disallowed_tools: Some(vec!["bash".to_string(), "python".to_string()]),
            agent_stuck_timeout_secs: Some(60),
            ..Default::default()
        };
        original.save(tmp.path()).unwrap();
        let reloaded = RepoConfig::load(tmp.path()).unwrap();
        assert_eq!(original, reloaded);
    }

    #[test]
    fn load_malformed_json_returns_config_parse_error() {
        let tmp = make_git_root();
        let awman_dir = tmp.path().join(REPO_CONFIG_SUBDIR);
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(awman_dir.join(REPO_CONFIG_FILENAME), b"{not valid json").unwrap();

        let err = RepoConfig::load(tmp.path()).unwrap_err();
        assert!(
            matches!(err, DataError::ConfigParse { .. }),
            "expected ConfigParse, got {err:?}"
        );
    }

    #[test]
    fn work_items_dir_resolves_relative_path() {
        let tmp = make_git_root();
        let cfg = RepoConfig {
            work_items: Some(WorkItemsConfig {
                dir: Some("aspec/work-items".to_string()),
                template: None,
            }),
            ..Default::default()
        };
        let resolved = cfg.work_items_dir(tmp.path()).unwrap();
        assert_eq!(resolved, tmp.path().join("aspec/work-items"));
    }

    #[test]
    fn work_items_dir_resolves_absolute_path() {
        let tmp = make_git_root();
        let cfg = RepoConfig {
            work_items: Some(WorkItemsConfig {
                dir: Some("/abs/path".to_string()),
                template: None,
            }),
            ..Default::default()
        };
        let resolved = cfg.work_items_dir(tmp.path()).unwrap();
        assert_eq!(resolved, PathBuf::from("/abs/path"));
    }

    #[test]
    fn work_items_dir_none_when_not_set() {
        let cfg = RepoConfig::default();
        let tmp = make_git_root();
        assert!(cfg.work_items_dir(tmp.path()).is_none());
    }

    #[test]
    fn path_is_inside_awman_subdir() {
        let tmp = make_git_root();
        let p = RepoConfig::path(tmp.path());
        assert_eq!(
            p,
            tmp.path()
                .join(REPO_CONFIG_SUBDIR)
                .join(REPO_CONFIG_FILENAME)
        );
    }

    // ─── Overlay field deserialization ───────────────────────────────────────

    #[test]
    fn overlays_new_flat_format_deserializes_correctly() {
        let tmp = make_git_root();
        let awman_dir = tmp.path().join(REPO_CONFIG_SUBDIR);
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join(REPO_CONFIG_FILENAME),
            r#"{"overlays": ["skill(*)", "env(X)"]}"#,
        )
        .unwrap();

        let cfg = RepoConfig::load(tmp.path()).unwrap();
        assert_eq!(
            cfg.overlays,
            Some(vec!["skill(*)".to_string(), "env(X)".to_string()]),
            "new flat overlays array must deserialize correctly"
        );
        assert!(cfg.legacy_env_passthrough.is_none());
    }

    #[test]
    fn overlays_old_object_format_fails_to_deserialize() {
        // Old format: {"overlays": {"directories": [...], "skills": true}}
        // Must not silently migrate — must surface a config parse error.
        let tmp = make_git_root();
        let awman_dir = tmp.path().join(REPO_CONFIG_SUBDIR);
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join(REPO_CONFIG_FILENAME),
            r#"{"overlays": {"directories": [], "skills": true}}"#,
        )
        .unwrap();

        let result = RepoConfig::load(tmp.path());
        assert!(
            result.is_err(),
            "old object-format overlays must fail to deserialize (no auto-migration); got Ok"
        );
        assert!(
            matches!(result.unwrap_err(), DataError::ConfigParse { .. }),
            "error must be ConfigParse"
        );
    }

    #[test]
    fn legacy_env_passthrough_deserializes_with_overlays_absent() {
        let tmp = make_git_root();
        let awman_dir = tmp.path().join(REPO_CONFIG_SUBDIR);
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join(REPO_CONFIG_FILENAME),
            r#"{"envPassthrough": ["MY_VAR", "OTHER_VAR"]}"#,
        )
        .unwrap();

        let cfg = RepoConfig::load(tmp.path()).unwrap();
        assert_eq!(
            cfg.legacy_env_passthrough,
            Some(vec!["MY_VAR".to_string(), "OTHER_VAR".to_string()]),
            "legacy envPassthrough must deserialize into legacy_env_passthrough"
        );
        assert!(
            cfg.overlays.is_none(),
            "overlays must be absent when only envPassthrough is present"
        );
    }

    #[test]
    fn overlays_round_trip_with_dir_and_env_expressions() {
        let tmp = make_git_root();
        let original = RepoConfig {
            overlays: Some(vec![
                "dir(~/data:/workspace:ro)".to_string(),
                "env(TOKEN)".to_string(),
            ]),
            ..Default::default()
        };
        original.save(tmp.path()).unwrap();
        let reloaded = RepoConfig::load(tmp.path()).unwrap();
        assert_eq!(
            original.overlays, reloaded.overlays,
            "overlays with dir() and env() expressions must round-trip correctly"
        );
    }

    // ─── Dockerfile field ─────────────────────────────────────────────────────

    #[test]
    fn dockerfile_path_or_default_returns_dockerfile_dev_when_absent() {
        let tmp = make_git_root();
        let cfg = RepoConfig::default();
        let result = cfg.dockerfile_path_or_default(tmp.path());
        assert_eq!(result, tmp.path().join("Dockerfile.dev"));
    }

    #[test]
    fn dockerfile_path_or_default_resolves_relative_path() {
        let tmp = make_git_root();
        let cfg = RepoConfig {
            dockerfile: Some("docker/Dockerfile.base".to_string()),
            ..Default::default()
        };
        let result = cfg.dockerfile_path_or_default(tmp.path());
        assert_eq!(result, tmp.path().join("docker/Dockerfile.base"));
    }

    #[test]
    fn dockerfile_path_or_default_uses_absolute_path_as_is() {
        let tmp = make_git_root();
        let cfg = RepoConfig {
            dockerfile: Some("/abs/Dockerfile".to_string()),
            ..Default::default()
        };
        let result = cfg.dockerfile_path_or_default(tmp.path());
        assert_eq!(result, PathBuf::from("/abs/Dockerfile"));
    }

    #[test]
    fn dockerfile_path_returns_none_when_field_absent() {
        let tmp = make_git_root();
        let cfg = RepoConfig::default();
        assert!(cfg.dockerfile_path(tmp.path()).is_none());
    }

    #[test]
    fn dockerfile_path_returns_none_when_field_is_empty_string() {
        let tmp = make_git_root();
        let cfg = RepoConfig {
            dockerfile: Some(String::new()),
            ..Default::default()
        };
        assert!(cfg.dockerfile_path(tmp.path()).is_none());
    }

    #[test]
    fn dockerfile_field_round_trips_through_save_and_load() {
        let tmp = make_git_root();
        let original = RepoConfig {
            dockerfile: Some("docker/Dockerfile.base".to_string()),
            ..Default::default()
        };
        original.save(tmp.path()).unwrap();
        let loaded = RepoConfig::load(tmp.path()).unwrap();
        assert_eq!(
            loaded.dockerfile.as_deref(),
            Some("docker/Dockerfile.base"),
            "dockerfile field must survive a save/load round-trip unmodified"
        );
    }

    // ─── DynamicWorkflowsConfig serde (WI-0095) ──────────────────────────────

    #[test]
    fn dynamic_workflows_config_deserializes_with_all_fields() {
        let json = r#"{
            "agentsToModels": {"claude": ["claude-opus-4-8", "claude-sonnet-4-6"]},
            "maxConcurrentSteps": 3,
            "defaultLeader": "claude::claude-opus-4-8"
        }"#;
        let cfg: DynamicWorkflowsConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg.agents_to_models,
            Some(HashMap::from([(
                "claude".to_string(),
                vec![
                    "claude-opus-4-8".to_string(),
                    "claude-sonnet-4-6".to_string()
                ]
            )]))
        );
        assert_eq!(cfg.max_concurrent_steps, Some(3));
        assert_eq!(
            cfg.default_leader.as_deref(),
            Some("claude::claude-opus-4-8")
        );
    }

    #[test]
    fn dynamic_workflows_config_deserializes_with_missing_fields() {
        let json = r#"{"maxConcurrentSteps": 5}"#;
        let cfg: DynamicWorkflowsConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.max_concurrent_steps, Some(5));
        assert!(cfg.agents_to_models.is_none());
        assert!(cfg.default_leader.is_none());
    }

    #[test]
    fn dynamic_workflows_config_deserializes_with_extra_unknown_fields() {
        // deny_unknown_fields must NOT be set on DynamicWorkflowsConfig: unknown
        // keys are ignored rather than causing a deserialize error, so forward
        // config files written by newer awman versions still parse.
        let json = r#"{"maxConcurrentSteps": 2, "someFutureField": "ignored"}"#;
        let cfg: DynamicWorkflowsConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.max_concurrent_steps, Some(2));
    }

    #[test]
    fn dynamic_workflows_config_empty_object_deserializes_to_default() {
        let cfg: DynamicWorkflowsConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg, DynamicWorkflowsConfig::default());
    }

    // ─── guidance field deserialization (WI-0099) ────────────────────────────

    #[test]
    fn guidance_deserializes_with_valid_entries() {
        let json = r#"{
            "guidance": [
                "never spawn more than two agents in parallel",
                "always include a validation step after each implementation step"
            ]
        }"#;
        let cfg: DynamicWorkflowsConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg.guidance,
            Some(vec![
                "never spawn more than two agents in parallel".to_string(),
                "always include a validation step after each implementation step".to_string(),
            ])
        );
    }

    #[test]
    fn guidance_deserializes_with_empty_array() {
        let json = r#"{"guidance": []}"#;
        let cfg: DynamicWorkflowsConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.guidance, Some(Vec::new()));
    }

    #[test]
    fn guidance_deserializes_with_missing_field() {
        let json = r#"{"maxConcurrentSteps": 2}"#;
        let cfg: DynamicWorkflowsConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.guidance.is_none());
    }

    // ─── guidance validation (WI-0099) ───────────────────────────────────────

    #[test]
    fn guidance_validate_rejects_entry_over_length_cap() {
        let cfg = DynamicWorkflowsConfig {
            guidance: Some(vec!["a".repeat(GUIDANCE_MAX_ENTRY_LEN + 1)]),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains(&GUIDANCE_MAX_ENTRY_LEN.to_string()),
            "error must name the length cap, got: {err}"
        );
    }

    #[test]
    fn guidance_validate_rejects_whitespace_only_entry() {
        let cfg = DynamicWorkflowsConfig {
            guidance: Some(vec!["   \t  ".to_string()]),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("empty or whitespace-only"),
            "whitespace-only entries must be rejected, got: {err}"
        );
    }

    #[test]
    fn guidance_validate_rejects_array_over_count_cap() {
        let cfg = DynamicWorkflowsConfig {
            guidance: Some(
                (0..GUIDANCE_MAX_ENTRIES + 1)
                    .map(|i| format!("entry {i}"))
                    .collect(),
            ),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains(&GUIDANCE_MAX_ENTRIES.to_string()),
            "error must name the count cap, got: {err}"
        );
    }

    #[test]
    fn guidance_validate_accepts_valid_array() {
        let cfg = DynamicWorkflowsConfig {
            guidance: Some(vec![
                "never spawn more than two agents in parallel".to_string(),
                "always run make test before finishing".to_string(),
            ]),
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn guidance_validate_accepts_empty_array() {
        let cfg = DynamicWorkflowsConfig {
            guidance: Some(Vec::new()),
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn load_rejects_guidance_whitespace_only_entry_via_config_json() {
        let tmp = make_git_root();
        let awman_dir = tmp.path().join(REPO_CONFIG_SUBDIR);
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join(REPO_CONFIG_FILENAME),
            r#"{"dynamicWorkflows": {"guidance": ["  "]}}"#,
        )
        .unwrap();

        let err = RepoConfig::load(tmp.path()).unwrap_err();
        assert!(
            err.to_string().contains("empty or whitespace-only"),
            "error must explain the whitespace-only rejection, got: {err}"
        );
    }

    #[test]
    fn load_accepts_guidance_valid_array_via_config_json() {
        let tmp = make_git_root();
        let awman_dir = tmp.path().join(REPO_CONFIG_SUBDIR);
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join(REPO_CONFIG_FILENAME),
            r#"{"dynamicWorkflows": {"guidance": ["entry one", "entry two"]}}"#,
        )
        .unwrap();

        let cfg = RepoConfig::load(tmp.path()).unwrap();
        assert_eq!(
            cfg.dynamic_workflows.and_then(|dw| dw.guidance),
            Some(vec!["entry one".to_string(), "entry two".to_string()])
        );
    }

    // ─── RepoConfig::load semantic validation (WI-0095) ──────────────────────

    #[test]
    fn load_rejects_max_concurrent_steps_zero() {
        let tmp = make_git_root();
        let awman_dir = tmp.path().join(REPO_CONFIG_SUBDIR);
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join(REPO_CONFIG_FILENAME),
            r#"{"dynamicWorkflows": {"maxConcurrentSteps": 0}}"#,
        )
        .unwrap();

        let err = RepoConfig::load(tmp.path()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("maxConcurrentSteps must be >= 1"),
            "error must explain the >= 1 requirement, got: {msg}"
        );
    }

    #[test]
    fn load_rejects_max_concurrent_agents_zero() {
        let tmp = make_git_root();
        let awman_dir = tmp.path().join(REPO_CONFIG_SUBDIR);
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join(REPO_CONFIG_FILENAME),
            r#"{"maxConcurrentAgents": 0}"#,
        )
        .unwrap();

        let err = RepoConfig::load(tmp.path()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("maxConcurrentAgents must be >= 1"),
            "error must explain the >= 1 requirement, got: {msg}"
        );
    }

    #[test]
    fn load_accepts_max_concurrent_agents_positive() {
        let tmp = make_git_root();
        let awman_dir = tmp.path().join(REPO_CONFIG_SUBDIR);
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join(REPO_CONFIG_FILENAME),
            r#"{"maxConcurrentAgents": 4}"#,
        )
        .unwrap();

        let cfg = RepoConfig::load(tmp.path()).unwrap();
        assert_eq!(cfg.max_concurrent_agents, Some(4));
    }

    #[test]
    fn load_rejects_default_leader_missing_double_colon() {
        let tmp = make_git_root();
        let awman_dir = tmp.path().join(REPO_CONFIG_SUBDIR);
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join(REPO_CONFIG_FILENAME),
            r#"{"dynamicWorkflows": {"defaultLeader": "claude"}}"#,
        )
        .unwrap();

        let err = RepoConfig::load(tmp.path()).unwrap_err();
        assert!(
            err.to_string().contains("expected agent::model"),
            "error must explain the expected agent::model format, got: {err}"
        );
    }

    #[test]
    fn load_rejects_default_leader_empty_component() {
        let tmp = make_git_root();
        let awman_dir = tmp.path().join(REPO_CONFIG_SUBDIR);
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join(REPO_CONFIG_FILENAME),
            r#"{"dynamicWorkflows": {"defaultLeader": "claude::"}}"#,
        )
        .unwrap();

        let err = RepoConfig::load(tmp.path()).unwrap_err();
        assert!(
            err.to_string().contains("expected agent::model"),
            "empty model component must be rejected, got: {err}"
        );
    }

    #[test]
    fn load_accepts_valid_default_leader() {
        let tmp = make_git_root();
        let awman_dir = tmp.path().join(REPO_CONFIG_SUBDIR);
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join(REPO_CONFIG_FILENAME),
            r#"{"dynamicWorkflows": {"defaultLeader": "claude::claude-opus-4-8"}}"#,
        )
        .unwrap();

        let cfg = RepoConfig::load(tmp.path()).unwrap();
        assert_eq!(
            cfg.dynamic_workflows
                .as_ref()
                .and_then(|dw| dw.default_leader.as_deref()),
            Some("claude::claude-opus-4-8")
        );
    }

    #[test]
    fn load_rejects_empty_model_list_for_one_agent() {
        // Edge case from the WI-0095 spec: an agent key with an empty model
        // list is a configuration error, not a silent no-op.
        let tmp = make_git_root();
        let awman_dir = tmp.path().join(REPO_CONFIG_SUBDIR);
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join(REPO_CONFIG_FILENAME),
            r#"{"dynamicWorkflows": {"agentsToModels": {"claude": []}}}"#,
        )
        .unwrap();

        let err = RepoConfig::load(tmp.path()).unwrap_err();
        assert!(
            err.to_string()
                .contains("dynamicWorkflows.agentsToModels.claude has an empty model list"),
            "error must name the empty-model-list agent, got: {err}"
        );
    }

    #[test]
    fn legacy_env_passthrough_is_not_serialized_in_save() {
        // legacy_env_passthrough has skip_serializing — it must not appear in the
        // JSON written by save(), so it doesn't persist across a load/save cycle.
        let tmp = make_git_root();
        let awman_dir = tmp.path().join(REPO_CONFIG_SUBDIR);
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join(REPO_CONFIG_FILENAME),
            r#"{"envPassthrough": ["VAR"]}"#,
        )
        .unwrap();

        let cfg = RepoConfig::load(tmp.path()).unwrap();
        cfg.save(tmp.path()).unwrap();

        let written = std::fs::read_to_string(RepoConfig::path(tmp.path())).unwrap();
        assert!(
            !written.contains("envPassthrough"),
            "legacy envPassthrough must not be re-serialized by save(); got: {written}"
        );
    }

    #[test]
    fn squad_config_uses_camel_case_and_validates_shared_agent_rules() {
        let config = SquadConfig {
            agents_to_models: Some(HashMap::from([(
                "claude".to_string(),
                vec!["claude-opus-4-8".to_string()],
            )])),
            max_concurrent_evaluations: Some(3),
            default_leader: Some("claude::claude-opus-4-8".to_string()),
            guidance: Some(vec!["Use focused workflows.".to_string()]),
        };
        config.validate().unwrap();
        let serialized = serde_json::to_value(&config).unwrap();
        assert!(serialized.get("maxConcurrentEvaluations").is_some());
        assert!(serialized.get("defaultLeader").is_some());
    }

    #[test]
    fn squad_config_rejects_zero_and_malformed_leader() {
        let zero = SquadConfig {
            max_concurrent_evaluations: Some(0),
            ..Default::default()
        };
        assert!(zero
            .validate()
            .unwrap_err()
            .to_string()
            .contains("squad.maxConcurrentEvaluations must be >= 1"));

        let malformed = SquadConfig {
            default_leader: Some(" claude::model".to_string()),
            ..Default::default()
        };
        assert!(malformed
            .validate()
            .unwrap_err()
            .to_string()
            .contains("squad.defaultLeader"));
    }
}
