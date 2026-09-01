//! `ConfigCommand` — view and edit global / repo configuration.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::message::UserMessageSink;

/// Scope metadata for each config field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldScope {
    /// May only be written to global config.
    GlobalOnly,
    /// May only be written to repo config.
    RepoOnly,
    /// May be written to either global or repo config.
    Both,
}

/// Entry in the config field table: `(dotted_name, scope)`.
const VALID_CONFIG_FIELDS: &[(&str, FieldScope)] = &[
    ("agent", FieldScope::Both),
    ("auto_agent_auth_accepted", FieldScope::GlobalOnly),
    ("terminal_scrollback_lines", FieldScope::Both),
    ("yoloDisallowedTools", FieldScope::Both),
    ("workItems", FieldScope::RepoOnly),
    ("overlays", FieldScope::Both),
    ("agentStuckTimeout", FieldScope::Both),
    ("maxConcurrentAgents", FieldScope::Both),
    ("runtime", FieldScope::GlobalOnly),
    ("default_agent", FieldScope::GlobalOnly),
    ("api", FieldScope::GlobalOnly),
    ("remote", FieldScope::Both),
    // Dot-notation nested fields
    ("work_items.dir", FieldScope::RepoOnly),
    ("work_items.template", FieldScope::RepoOnly),
    ("api.workDirs", FieldScope::GlobalOnly),
    ("api.port", FieldScope::GlobalOnly),
    ("api.background", FieldScope::GlobalOnly),
    ("remote.defaultAddr", FieldScope::Both),
    ("remote.defaultAPIKey", FieldScope::Both),
    // Dynamic-workflow config (WI-0095), repo-only.
    ("dynamicWorkflows.defaultLeader", FieldScope::RepoOnly),
    ("dynamicWorkflows.maxConcurrentSteps", FieldScope::RepoOnly),
    ("dynamicWorkflows.agentsToModels", FieldScope::RepoOnly),
    ("dynamicWorkflows.guidance", FieldScope::RepoOnly),
];

/// Field names that were removed in WI-0082 (overlay unification). Naming any
/// of these in `awman config get|set` returns a guidance error instead of the
/// generic "unknown field" suggestion list.
const REMOVED_CONFIG_FIELDS: &[(&str, &str)] = &[(
    "envPassthrough",
    "the 'envPassthrough' field was removed; express env passthrough as overlay entries \
     instead (e.g. `awman config set overlays \"env(VAR_NAME)\"` or add `\"env(VAR_NAME)\"` \
     to the `overlays` array in `.awman/config.json`). See `docs/09-overlays.md`.",
)];

fn removed_field_message(name: &str) -> Option<&'static str> {
    REMOVED_CONFIG_FIELDS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, msg)| *msg)
}

/// Flat list of all valid field names (for suggestions / validation).
fn valid_field_names() -> Vec<&'static str> {
    VALID_CONFIG_FIELDS.iter().map(|(name, _)| *name).collect()
}

/// Dot-path prefix of per-agent `agentsToModels` entries.
const AGENTS_TO_MODELS_PREFIX: &str = "dynamicWorkflows.agentsToModels.";

/// If `name` addresses a single agent's model list
/// (`dynamicWorkflows.agentsToModels.<agent>`), return the agent key.
fn agents_to_models_entry_key(name: &str) -> Option<&str> {
    let key = name.strip_prefix(AGENTS_TO_MODELS_PREFIX)?;
    if key.is_empty() || key.contains('.') {
        return None;
    }
    Some(key)
}

/// Dot-path prefix of per-index `dynamicWorkflows.guidance` entries (WI-0099).
const GUIDANCE_PREFIX: &str = "dynamicWorkflows.guidance.";

/// If `name` addresses a single guidance entry
/// (`dynamicWorkflows.guidance.<index>`), return the parsed numeric index.
/// The trailing segment must be a bare non-negative integer with no further
/// dots — this is what keeps the numeric-index dot-path handling narrow to
/// the guidance array rather than a general array facility.
fn guidance_entry_index(name: &str) -> Option<usize> {
    let idx = name.strip_prefix(GUIDANCE_PREFIX)?;
    if idx.is_empty() || idx.contains('.') {
        return None;
    }
    idx.parse::<usize>().ok()
}

/// Lexical validation for an `agentsToModels` key, mirroring
/// `data::session::AgentName` (ASCII alphanumerics, `-`, `_`, length 1..=64).
fn validate_agents_to_models_key(key: &str) -> Result<(), String> {
    if key.is_empty()
        || key.len() > 64
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "'{key}' is not a valid agent name: only ASCII alphanumerics, '-', and '_' are \
             allowed, length 1..=64"
        ));
    }
    Ok(())
}

/// Whether `name` is an accepted field name for `config get`/`config set`.
/// Per-agent `agentsToModels` entries are valid even though they are not in
/// the static table (the set of agents is user-defined).
fn is_valid_field_name(name: &str) -> bool {
    valid_field_names().contains(&name)
        || agents_to_models_entry_key(name).is_some()
        || guidance_entry_index(name).is_some()
}

/// Look up the scope for a field name.
fn field_scope(name: &str) -> Option<FieldScope> {
    if agents_to_models_entry_key(name).is_some() || guidance_entry_index(name).is_some() {
        return Some(FieldScope::RepoOnly);
    }
    VALID_CONFIG_FIELDS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, s)| *s)
}

/// Valid agent names for config set agent=<value>. Must stay in sync with
/// `engine::agent::agent_matrix::SUPPORTED_AGENTS` — the matrix is authoritative;
/// this list is checked at config-write time so unsupported values are rejected
/// before they reach the engine.
const VALID_AGENT_VALUES: &[&str] = &[
    "claude",
    "codex",
    "gemini",
    "opencode",
    "crush",
    "cline",
    "copilot",
    "maki",
    "antigravity",
];

/// Validate and coerce a string value into the appropriate JSON type for the
/// given field. Returns the coerced `serde_json::Value` or a user-facing error.
fn validate_and_coerce(field: &str, value: &str) -> Result<serde_json::Value, String> {
    if let Some(agent) = agents_to_models_entry_key(field) {
        validate_agents_to_models_key(agent)?;
        let items: Vec<&str> = value
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        // An agent cannot be mapped to zero models (RepoConfig validation
        // rejects empty lists), so an empty value means "remove this entry".
        if items.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        return Ok(serde_json::Value::Array(
            items
                .iter()
                .map(|s| serde_json::Value::String(s.to_string()))
                .collect(),
        ));
    }
    if guidance_entry_index(field).is_some() {
        // A guidance entry is a single free-form instruction (it may contain
        // commas, markdown, etc.), so it is NOT comma-split. An empty or
        // whitespace-only value coerces to Null, which the config layer treats
        // as "remove this array element".
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        if trimmed.chars().count() > crate::data::config::repo::GUIDANCE_MAX_ENTRY_LEN {
            return Err(format!(
                "guidance entry is {} characters; the maximum is {}",
                trimmed.chars().count(),
                crate::data::config::repo::GUIDANCE_MAX_ENTRY_LEN
            ));
        }
        return Ok(serde_json::Value::String(trimmed.to_string()));
    }
    match field {
        "agent" | "default_agent" => {
            if !VALID_AGENT_VALUES.contains(&value) {
                return Err(format!(
                    "'{}' is not a known agent; valid agents: {}",
                    value,
                    VALID_AGENT_VALUES.join(", ")
                ));
            }
            Ok(serde_json::Value::String(value.to_string()))
        }
        "yoloDisallowedTools" | "overlays" | "api.workDirs" => {
            // Parse comma-separated into array
            let items: Vec<&str> = value
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            Ok(serde_json::Value::Array(
                items
                    .iter()
                    .map(|s| serde_json::Value::String(s.to_string()))
                    .collect(),
            ))
        }
        "terminal_scrollback_lines" | "agentStuckTimeout" | "api.port" => {
            // Must be a positive integer
            value
                .parse::<u64>()
                .map(|n| serde_json::Value::Number(n.into()))
                .map_err(|_| format!("'{}' is not a valid number", value))
        }
        "dynamicWorkflows.maxConcurrentSteps" => {
            // Positive integer; zero would deadlock any workflow (WI-0095).
            let n = value
                .parse::<u64>()
                .map_err(|_| format!("'{}' is not a valid number", value))?;
            if n < 1 {
                return Err("dynamicWorkflows.maxConcurrentSteps must be >= 1".to_string());
            }
            Ok(serde_json::Value::Number(n.into()))
        }
        "maxConcurrentAgents" => {
            let n = value
                .parse::<u64>()
                .map_err(|_| format!("'{}' is not a valid number", value))?;
            if n < 1 {
                return Err("maxConcurrentAgents must be >= 1".to_string());
            }
            Ok(serde_json::Value::Number(n.into()))
        }
        "dynamicWorkflows.defaultLeader" => {
            validate_default_leader_value(value)?;
            Ok(serde_json::Value::String(value.to_string()))
        }
        "dynamicWorkflows.agentsToModels" => {
            // A map value can't be expressed as a `config set` string; without
            // this rejection the coerced string would fail RepoConfig
            // deserialization and the set would silently write nothing.
            Err(
                "dynamicWorkflows.agentsToModels is a map; set one agent at a time, e.g. \
                 `awman config set dynamicWorkflows.agentsToModels.claude \"model-a, model-b\"`"
                    .to_string(),
            )
        }
        "dynamicWorkflows.guidance" => {
            // The guidance array is edited one entry at a time by index; the
            // bare field can't be set as a single string (it would fail
            // RepoConfig deserialization). Point users at the per-index syntax.
            Err(
                "dynamicWorkflows.guidance is a list; set one entry at a time by index, e.g. \
                 `awman config set dynamicWorkflows.guidance.0 \"never spawn more than two \
                 agents in parallel\"`"
                    .to_string(),
            )
        }
        _ => {
            // Default: try bool, then number, then string
            if value == "true" {
                Ok(serde_json::Value::Bool(true))
            } else if value == "false" {
                Ok(serde_json::Value::Bool(false))
            } else if let Ok(n) = value.parse::<u64>() {
                Ok(serde_json::Value::Number(n.into()))
            } else {
                Ok(serde_json::Value::String(value.to_string()))
            }
        }
    }
}

/// Validate a `dynamicWorkflows.defaultLeader` value for `awman config set`
/// (WI-0095). Enforces the same `agent::model` shape as the Layer 0 config-load
/// validator: exactly two non-empty components, no surrounding whitespace, and
/// an `AgentName`-valid agent component.
fn validate_default_leader_value(value: &str) -> Result<(), String> {
    let parts: Vec<&str> = value.split("::").collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(format!(
            "'{value}' is not a valid leader; expected agent::model \
             (e.g. claude::claude-opus-4-8)"
        ));
    }
    let (agent, model) = (parts[0], parts[1]);
    if agent != agent.trim() || model != model.trim() {
        return Err(
            "leader agent and model components must not have leading or trailing whitespace"
                .to_string(),
        );
    }
    if agent.len() > 64
        || !agent
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "'{agent}' is not a valid agent name: only ASCII alphanumerics, '-', and '_' are \
             allowed, length 1..=64"
        ));
    }
    Ok(())
}

/// Levenshtein edit distance between two strings.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let m = a.len();
    let n = b.len();
    // Row 0: dp[0][j] = j for j in 0..=n
    let first_row: Vec<usize> = (0..=n).collect();
    let mut dp: Vec<Vec<usize>> = std::iter::once(first_row)
        .chain((1..=m).map(|i| {
            let mut row = vec![0usize; n + 1];
            row[0] = i;
            row
        }))
        .collect();
    for i in 1..=m {
        for j in 1..=n {
            dp[i][j] = if a[i - 1] == b[j - 1] {
                dp[i - 1][j - 1]
            } else {
                1 + dp[i - 1][j - 1].min(dp[i - 1][j]).min(dp[i][j - 1])
            };
        }
    }
    dp[m][n]
}

/// Return candidates with levenshtein distance <= 3, sorted by distance ascending.
fn levenshtein_suggestions<'a>(input: &str, candidates: &[&'a str]) -> Vec<&'a str> {
    let mut scored: Vec<(usize, &'a str)> = candidates
        .iter()
        .filter_map(|c| {
            let dist = levenshtein(input, c);
            if dist <= 3 {
                Some((dist, *c))
            } else {
                None
            }
        })
        .collect();
    scored.sort_by_key(|(d, _)| *d);
    scored.into_iter().map(|(_, c)| c).collect()
}

#[derive(Debug, Clone)]
pub struct ConfigShowFlags {}

#[derive(Debug, Clone)]
pub struct ConfigGetFlags {
    pub field: String,
}

#[derive(Debug, Clone)]
pub struct ConfigSetFlags {
    pub field: String,
    pub value: String,
    pub global: bool,
}

#[derive(Debug, Clone)]
pub enum ConfigSubcommand {
    Show(ConfigShowFlags),
    Get(ConfigGetFlags),
    Set(ConfigSetFlags),
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigShowOutcome {
    pub global: serde_json::Value,
    pub repo: serde_json::Value,
    /// One row per known field, computed in Layer 2 so the renderer doesn't
    /// need to know which fields exist or which are read-only.
    pub rows: Vec<ConfigFieldRow>,
}

/// Per-field row used by `ConfigShow` rendering.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigFieldRow {
    pub field: String,
    pub global_value: Option<String>,
    pub repo_value: Option<String>,
    pub effective_value: Option<String>,
    /// What kind of value the field accepts. Lets the renderer (or a
    /// programmatic consumer) format the value cell appropriately and lets
    /// `set` validate input early.
    pub kind: ConfigFieldKind,
    pub read_only: bool,
    /// Whether the value may be written to the global config scope.
    pub global_writable: bool,
    /// Whether the value may be written to the repo config scope.
    pub repo_writable: bool,
    /// Short human-readable format hint shown while editing the value
    /// (e.g. "true or false", "comma-separated list").
    pub value_hint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigFieldKind {
    Bool,
    Number,
    /// Fixed enum (e.g. agent name); the `set` validator rejects values
    /// outside the documented set.
    Enum,
    String,
}

/// Map a known config field name to its `ConfigFieldKind`. Mirrors the
/// schema in `RepoConfig` / `GlobalConfig`. Unknown fields default to
/// `String` (callers should reject them before reaching this function).
fn config_field_kind(name: &str) -> ConfigFieldKind {
    match name {
        "agent" | "default_agent" => ConfigFieldKind::Enum,
        "auto_agent_auth_accepted" | "api.background" => ConfigFieldKind::Bool,
        "terminal_scrollback_lines"
        | "agentStuckTimeout"
        | "api.port"
        | "dynamicWorkflows.maxConcurrentSteps"
        | "maxConcurrentAgents" => ConfigFieldKind::Number,
        _ => ConfigFieldKind::String,
    }
}

/// Fields whose value is computed by awman itself and cannot be set by the
/// user via `awman config set`. Surfaced with `(read-only)` in the table.
const READ_ONLY_FIELDS: &[&str] = &["auto_agent_auth_accepted"];

/// Whether a config field is read-only in the show/edit UI. Per-agent
/// `dynamicWorkflows.agentsToModels.<agent>` rows are inline-editable; only
/// the summary row for the map itself (added in `collect_config_rows`) and
/// awman-computed fields are read-only.
fn is_read_only_field(name: &str) -> bool {
    READ_ONLY_FIELDS.contains(&name)
        || name == "dynamicWorkflows.agentsToModels"
        || name == "dynamicWorkflows.guidance"
}

/// Per-scope writability for a field, derived from its scope. Read-only
/// fields are writable in neither scope.
fn field_writability(name: &str) -> (bool, bool) {
    if is_read_only_field(name) {
        return (false, false);
    }
    match field_scope(name) {
        Some(FieldScope::GlobalOnly) => (true, false),
        Some(FieldScope::RepoOnly) => (false, true),
        Some(FieldScope::Both) => (true, true),
        None => (false, false),
    }
}

/// Human-readable format hint for a field's value, shown in the TUI config
/// dialog while editing.
fn config_field_hint(name: &str) -> Option<String> {
    if agents_to_models_entry_key(name).is_some() {
        return Some("comma-separated model names; save an empty value to remove".to_string());
    }
    if guidance_entry_index(name).is_some() {
        return Some("a single instruction; save an empty value to remove".to_string());
    }
    match name {
        "agent" | "default_agent" => Some(format!("one of: {}", VALID_AGENT_VALUES.join(", "))),
        "auto_agent_auth_accepted" | "api.background" => Some("true or false".to_string()),
        "terminal_scrollback_lines" | "agentStuckTimeout" | "api.port" => {
            Some("positive integer".to_string())
        }
        "dynamicWorkflows.maxConcurrentSteps" => Some("integer >= 1".to_string()),
        "maxConcurrentAgents" => Some("integer >= 1 (unset = unlimited)".to_string()),
        "dynamicWorkflows.defaultLeader" => {
            Some("agent::model (e.g. claude::claude-opus-4-8)".to_string())
        }
        "dynamicWorkflows.agentsToModels" => {
            Some("press Ctrl+N to add an agent; edit per-agent rows inline".to_string())
        }
        "dynamicWorkflows.guidance" => Some(
            "press Ctrl+N to add a guidance entry; edit per-entry rows inline; save an empty \
             value to remove"
                .to_string(),
        ),
        "yoloDisallowedTools" | "overlays" | "api.workDirs" => {
            Some("comma-separated list".to_string())
        }
        _ => None,
    }
}

/// Render an `agentsToModels` model-list JSON value as a comma-separated string
/// for a display row.
fn render_models_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|x| x.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

const SENSITIVE_FIELDS: &[&str] = &["remote.defaultAPIKey"];

fn mask_sensitive(field: &str, value: Option<String>) -> Option<String> {
    if !SENSITIVE_FIELDS.contains(&field) {
        return value;
    }
    value.map(|v| {
        if v.len() > 12 {
            format!("{}…{}", &v[..4], &v[v.len() - 4..])
        } else {
            "(set)".to_string()
        }
    })
}

pub fn collect_config_rows(
    global: &serde_json::Value,
    repo: &serde_json::Value,
) -> Vec<ConfigFieldRow> {
    let mut rows: Vec<ConfigFieldRow> = Vec::new();
    for (name, _scope) in VALID_CONFIG_FIELDS {
        // The agentsToModels map and the guidance array are expanded into one
        // row per entry below rather than shown as a single unreadable JSON
        // blob (WI-0095, WI-0099).
        if *name == "dynamicWorkflows.agentsToModels" || *name == "dynamicWorkflows.guidance" {
            continue;
        }
        let g = config_field_value(global, name);
        let r = config_field_value(repo, name);
        let effective = r.clone().or_else(|| g.clone());
        let (global_writable, repo_writable) = field_writability(name);
        rows.push(ConfigFieldRow {
            field: (*name).to_string(),
            global_value: mask_sensitive(name, g),
            repo_value: mask_sensitive(name, r),
            effective_value: mask_sensitive(name, effective),
            kind: config_field_kind(name),
            read_only: is_read_only_field(name),
            global_writable,
            repo_writable,
            value_hint: config_field_hint(name),
        });
    }
    // The agent→models map always gets a read-only summary row so the mapping
    // is discoverable even when empty, followed by one editable repo-only row
    // per agent (`dynamicWorkflows.agentsToModels.<agentName>`). Keys are
    // sorted for deterministic ordering.
    let map = repo
        .get("dynamicWorkflows")
        .and_then(|dw| dw.get("agentsToModels"))
        .and_then(|m| m.as_object());
    let summary = match map {
        Some(m) if !m.is_empty() => {
            let n = m.len();
            format!("{n} agent{} mapped", if n == 1 { "" } else { "s" })
        }
        _ => "(none)".to_string(),
    };
    rows.push(ConfigFieldRow {
        field: "dynamicWorkflows.agentsToModels".to_string(),
        global_value: None,
        repo_value: Some(summary.clone()),
        effective_value: Some(summary),
        kind: ConfigFieldKind::String,
        read_only: true,
        global_writable: false,
        repo_writable: false,
        value_hint: config_field_hint("dynamicWorkflows.agentsToModels"),
    });
    if let Some(map) = map {
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        for key in keys {
            let field = format!("dynamicWorkflows.agentsToModels.{key}");
            let value = map.get(key).map(render_models_value);
            let value_hint = config_field_hint(&field);
            rows.push(ConfigFieldRow {
                field,
                global_value: None,
                repo_value: value.clone(),
                effective_value: value,
                kind: ConfigFieldKind::String,
                read_only: false,
                global_writable: false,
                repo_writable: true,
                value_hint,
            });
        }
    }
    // The leader-guidance array (WI-0099) gets a read-only summary row so the
    // section is discoverable (and Ctrl+N works) even when empty, followed by
    // one editable repo-only row per entry
    // (`dynamicWorkflows.guidance.<index>`). Indices preserve config order.
    let guidance = repo
        .get("dynamicWorkflows")
        .and_then(|dw| dw.get("guidance"))
        .and_then(|g| g.as_array());
    let guidance_summary = match guidance {
        Some(arr) if !arr.is_empty() => {
            let n = arr.len();
            format!("{n} entr{}", if n == 1 { "y" } else { "ies" })
        }
        _ => "(none)".to_string(),
    };
    rows.push(ConfigFieldRow {
        field: "dynamicWorkflows.guidance".to_string(),
        global_value: None,
        repo_value: Some(guidance_summary.clone()),
        effective_value: Some(guidance_summary),
        kind: ConfigFieldKind::String,
        read_only: true,
        global_writable: false,
        repo_writable: false,
        value_hint: config_field_hint("dynamicWorkflows.guidance"),
    });
    if let Some(arr) = guidance {
        for (i, entry) in arr.iter().enumerate() {
            let field = format!("dynamicWorkflows.guidance.{i}");
            let value = entry.as_str().map(|s| s.to_string());
            let value_hint = config_field_hint(&field);
            rows.push(ConfigFieldRow {
                field,
                global_value: None,
                repo_value: value.clone(),
                effective_value: value,
                kind: ConfigFieldKind::String,
                read_only: false,
                global_writable: false,
                repo_writable: true,
                value_hint,
            });
        }
    }
    rows
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigGetOutcome {
    pub field: String,
    pub global_value: Option<String>,
    pub repo_value: Option<String>,
    pub effective_value: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigSetOutcome {
    pub field: String,
    pub value: String,
    pub scope: String,
}

/// A user edit returned from the config show dialog.
#[derive(Debug, Clone)]
pub struct ConfigEditRequest {
    pub field: String,
    pub value: String,
    pub global: bool,
}

/// An edit that failed validation or could not be written to disk. Handed
/// back to the frontend on the next `present_config_table` call so the UI
/// can preserve the user's input and show the reason, instead of silently
/// discarding the edit.
#[derive(Debug, Clone)]
pub struct ConfigEditRejection {
    pub field: String,
    pub value: String,
    pub global: bool,
    pub reason: String,
}

pub trait ConfigCommandFrontend: UserMessageSink + Send + Sync {
    /// Present the config table to the user and block until they either
    /// dismiss the dialog or edit a value. Returns `Ok(None)` on dismiss,
    /// `Ok(Some(edit))` when the user changes a field.
    ///
    /// `rejected` carries the previous edit when it failed validation or
    /// could not be persisted; the frontend should surface the reason and
    /// let the user correct the value rather than retype it.
    fn present_config_table(
        &mut self,
        rows: &[ConfigFieldRow],
        rejected: Option<&ConfigEditRejection>,
    ) -> Result<Option<ConfigEditRequest>, CommandError>;
}

pub struct ConfigCommand {
    sub: ConfigSubcommand,
    engines: Engines,
    session: crate::data::session::Session,
}

impl ConfigCommand {
    pub fn new(
        sub: ConfigSubcommand,
        engines: Engines,
        session: crate::data::session::Session,
    ) -> Self {
        Self {
            sub,
            engines,
            session,
        }
    }

    pub fn subcommand(&self) -> &ConfigSubcommand {
        &self.sub
    }
}

/// Outcome enum used by the `Command` trait impl.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum ConfigOutcome {
    Show(ConfigShowOutcome),
    Get(ConfigGetOutcome),
    Set(ConfigSetOutcome),
}

#[async_trait]
impl Command for ConfigCommand {
    type Frontend = Box<dyn ConfigCommandFrontend>;
    type Outcome = ConfigOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let _ = self.engines;
        let session = self.session;
        let names = valid_field_names();
        let outcome = match self.sub {
            ConfigSubcommand::Show(_) => {
                let mut session = session;
                let mut rejected: Option<ConfigEditRejection> = None;
                loop {
                    let global = serde_json::to_value(session.global_config())
                        .unwrap_or(serde_json::Value::Null);
                    let repo = serde_json::to_value(session.repo_config())
                        .unwrap_or(serde_json::Value::Null);
                    let rows = collect_config_rows(&global, &repo);

                    match frontend.present_config_table(&rows, rejected.take().as_ref())? {
                        None => {
                            break ConfigOutcome::Show(ConfigShowOutcome { global, repo, rows });
                        }
                        Some(edit) => {
                            // Any failure — validation, schema mismatch, or
                            // disk write — is handed back to the frontend as
                            // a rejection so the user's input is preserved
                            // and the reason is visible in the dialog. It is
                            // also logged for the post-dialog record.
                            let result =
                                validate_and_coerce(&edit.field, &edit.value).and_then(|coerced| {
                                    write_config_field(&session, &edit.field, coerced, edit.global)
                                });
                            if let Err(reason) = result {
                                frontend.write_message(crate::data::message::UserMessage {
                                    level: crate::data::message::MessageLevel::Warning,
                                    text: format!("Config edit not saved: {reason}"),
                                });
                                rejected = Some(ConfigEditRejection {
                                    field: edit.field,
                                    value: edit.value,
                                    global: edit.global,
                                    reason,
                                });
                                continue;
                            }
                            session = {
                                let wd = session.working_dir().to_path_buf();
                                let gr = session.git_root().to_path_buf();
                                crate::data::session::Session::open_at_git_root(
                                    wd,
                                    gr,
                                    crate::data::session::SessionOpenOptions::default(),
                                )
                                .map_err(CommandError::from)?
                            };
                        }
                    }
                }
            }
            ConfigSubcommand::Get(f) => {
                // Validate field name.
                if let Some(msg) = removed_field_message(&f.field) {
                    return Err(CommandError::Other(msg.to_string()));
                }
                if !is_valid_field_name(&f.field) {
                    let suggestions = levenshtein_suggestions(&f.field, &names);
                    return Err(CommandError::UnknownConfigField {
                        name: f.field.clone(),
                        suggestions: if suggestions.is_empty() {
                            "(none)".to_string()
                        } else {
                            suggestions.join(", ")
                        },
                    });
                }
                let global_value = config_field_value(
                    &serde_json::to_value(session.global_config())
                        .unwrap_or(serde_json::Value::Null),
                    &f.field,
                );
                let repo_value = config_field_value(
                    &serde_json::to_value(session.repo_config()).unwrap_or(serde_json::Value::Null),
                    &f.field,
                );
                let effective_value = repo_value.clone().or_else(|| global_value.clone());
                ConfigOutcome::Get(ConfigGetOutcome {
                    field: f.field,
                    global_value,
                    repo_value,
                    effective_value,
                })
            }
            ConfigSubcommand::Set(f) => {
                // Validate field name.
                if let Some(msg) = removed_field_message(&f.field) {
                    return Err(CommandError::Other(msg.to_string()));
                }
                if !is_valid_field_name(&f.field) {
                    let suggestions = levenshtein_suggestions(&f.field, &names);
                    return Err(CommandError::UnknownConfigField {
                        name: f.field.clone(),
                        suggestions: if suggestions.is_empty() {
                            "(none)".to_string()
                        } else {
                            suggestions.join(", ")
                        },
                    });
                }
                // Validate scope: enforce GlobalOnly / RepoOnly constraints.
                if let Some(scope) = field_scope(&f.field) {
                    if scope == FieldScope::GlobalOnly && !f.global {
                        return Err(CommandError::InvalidFlagValue {
                            command: vec!["config".into(), "set".into()],
                            flag: "global".into(),
                            reason: format!(
                                "field '{}' can only be set in global config; add --global",
                                f.field
                            ),
                        });
                    }
                    if scope == FieldScope::RepoOnly && f.global {
                        return Err(CommandError::InvalidFlagValue {
                            command: vec!["config".into(), "set".into()],
                            flag: "global".into(),
                            reason: format!(
                                "field '{}' can only be set in repo config; omit --global",
                                f.field
                            ),
                        });
                    }
                }
                // Validate and coerce the value per field type.
                let coerced = validate_and_coerce(&f.field, &f.value).map_err(|reason| {
                    CommandError::InvalidFlagValue {
                        command: vec!["config".into(), "set".into()],
                        flag: f.field.clone(),
                        reason,
                    }
                })?;
                write_config_field(&session, &f.field, coerced, f.global).map_err(|reason| {
                    CommandError::Other(format!("failed to set '{}': {reason}", f.field))
                })?;
                ConfigOutcome::Set(ConfigSetOutcome {
                    field: f.field,
                    value: f.value,
                    scope: if f.global {
                        "global".into()
                    } else {
                        "repo".into()
                    },
                })
            }
        };
        frontend.replay_queued();
        Ok(outcome)
    }
}

/// Apply a coerced field value to a config snapshot, returning the updated
/// struct or a user-facing reason when the resulting JSON no longer fits the
/// config schema. Shared by the interactive edit loop and `config set` so a
/// schema mismatch is never silently dropped.
fn updated_config<T: Serialize + serde::de::DeserializeOwned>(
    cfg: &T,
    field: &str,
    coerced: serde_json::Value,
) -> Result<T, String> {
    let mut json =
        serde_json::to_value(cfg).map_err(|e| format!("could not serialize config: {e}"))?;
    apply_config_field(&mut json, field, coerced);
    serde_json::from_value(json).map_err(|e| format!("'{field}' does not accept this value ({e})"))
}

/// Validate, apply, and persist one field edit to the chosen config scope.
/// Returns a user-facing reason on any failure — validation, schema
/// mismatch, or disk write — so callers can surface it instead of
/// pretending the edit succeeded.
fn write_config_field(
    session: &crate::data::session::Session,
    field: &str,
    coerced: serde_json::Value,
    global: bool,
) -> Result<(), String> {
    if global {
        let cfg = updated_config(session.global_config(), field, coerced)?;
        cfg.save()
            .map_err(|e| format!("could not write global config: {e}"))
    } else {
        let cfg = updated_config(session.repo_config(), field, coerced)?;
        cfg.save(session.git_root()).map_err(|e| {
            format!(
                "could not write {}: {e}",
                crate::data::config::repo::RepoConfig::path(session.git_root()).display()
            )
        })
    }
}

/// Look up a JSON field value, supporting dot-notation (e.g. "work_items.dir").
fn config_field_value(json: &serde_json::Value, field: &str) -> Option<String> {
    let parts: Vec<&str> = field.split('.').collect();
    let mut current = json;
    for part in &parts {
        // A numeric segment indexes into an array (e.g. the
        // dynamicWorkflows.guidance.<n> entries); everything else is an
        // object key.
        current = match current {
            serde_json::Value::Array(arr) => arr.get(part.parse::<usize>().ok()?)?,
            _ => current.get(*part)?,
        };
    }
    Some(match current {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Null => return None,
        // Arrays of strings display comma-separated — the same shape the
        // user types when setting a list field, so edits round-trip.
        serde_json::Value::Array(arr) if arr.iter().all(|x| x.is_string()) => arr
            .iter()
            .filter_map(|x| x.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    })
}

/// Write a coerced value into the config JSON: `Null` removes the field
/// (used for `agentsToModels` entry deletion), anything else is set.
fn apply_config_field(json: &mut serde_json::Value, field: &str, value: serde_json::Value) {
    if value.is_null() {
        remove_config_field(json, field);
    } else {
        set_config_field(json, field, value);
    }
}

/// Remove a JSON field, supporting dot-notation for nested objects and a
/// trailing numeric segment that indexes into an array-of-strings field
/// (e.g. `dynamicWorkflows.guidance.1`). Removing an array element via
/// `Vec::remove` compacts the remaining elements, so subsequent entries are
/// automatically re-indexed (WI-0099). Missing intermediate objects make this
/// a no-op.
fn remove_config_field(json: &mut serde_json::Value, field: &str) {
    let parts: Vec<&str> = field.split('.').collect();
    let last = *parts.last().expect("split never yields an empty vec");
    // Trailing numeric segment: remove the element from the parent array.
    if let Ok(index) = last.parse::<usize>() {
        if parts.len() >= 2 {
            let mut current = json;
            for part in &parts[..parts.len() - 1] {
                match current.get_mut(*part) {
                    Some(v) => current = v,
                    None => return,
                }
            }
            if let serde_json::Value::Array(arr) = current {
                if index < arr.len() {
                    arr.remove(index);
                }
            }
            return;
        }
    }
    let mut current = json;
    for part in &parts[..parts.len() - 1] {
        match current.get_mut(*part) {
            Some(v) => current = v,
            None => return,
        }
    }
    if let serde_json::Value::Object(obj) = current {
        obj.remove(last);
    }
}

/// Set a JSON field, supporting dot-notation for nested objects.
/// E.g. "work_items.dir" sets `json["work_items"]["dir"]`.
fn set_config_field(json: &mut serde_json::Value, field: &str, value: serde_json::Value) {
    let parts: Vec<&str> = field.split('.').collect();
    // Trailing numeric segment: set (or append) an element in the parent
    // array-of-strings field (e.g. `dynamicWorkflows.guidance.<n>`). An index
    // at or past the current length appends, which is how the TUI Ctrl+N flow
    // adds a new entry (WI-0099). Intermediate objects and the array itself
    // are created on demand so guidance can be added to a config that has no
    // `dynamicWorkflows` block yet.
    if let Some(index) = parts.last().and_then(|p| p.parse::<usize>().ok()) {
        if parts.len() >= 2 {
            set_array_element(json, &parts[..parts.len() - 1], index, value);
            return;
        }
    }
    if parts.len() == 1 {
        // Top-level field
        if let serde_json::Value::Object(obj) = json {
            obj.insert(field.to_string(), value);
        }
    } else {
        // Navigate into nested objects, creating intermediate objects as needed.
        let mut current = json;
        for (i, part) in parts.iter().enumerate() {
            if i == parts.len() - 1 {
                // Last segment: insert the value.
                if let serde_json::Value::Object(obj) = current {
                    obj.insert(part.to_string(), value);
                }
                return;
            }
            // Intermediate segment: ensure a nested object exists.
            if !current.get(*part).map(|v| v.is_object()).unwrap_or(false) {
                if let serde_json::Value::Object(obj) = current {
                    obj.insert(
                        part.to_string(),
                        serde_json::Value::Object(serde_json::Map::new()),
                    );
                }
            }
            current = current.get_mut(*part).expect("just inserted nested object");
        }
    }
}

/// Set or append an element in an array-of-strings field addressed by
/// `array_path` (the dot-path segments up to and including the array field,
/// e.g. `["dynamicWorkflows", "guidance"]`). Intermediate objects and the
/// array are created on demand. `index >= len` appends; `index < len`
/// overwrites in place. Used for the `dynamicWorkflows.guidance` array
/// (WI-0099).
fn set_array_element(
    json: &mut serde_json::Value,
    array_path: &[&str],
    index: usize,
    value: serde_json::Value,
) {
    let Some((array_field, obj_path)) = array_path.split_last() else {
        return;
    };
    // Navigate (creating as needed) the object path that holds the array.
    let mut current = json;
    for part in obj_path {
        if !current.get(*part).map(|v| v.is_object()).unwrap_or(false) {
            if let serde_json::Value::Object(obj) = current {
                obj.insert(
                    part.to_string(),
                    serde_json::Value::Object(serde_json::Map::new()),
                );
            } else {
                return;
            }
        }
        current = current.get_mut(*part).expect("just inserted nested object");
    }
    // Ensure the array field exists and is an array.
    if !current
        .get(*array_field)
        .map(|v| v.is_array())
        .unwrap_or(false)
    {
        if let serde_json::Value::Object(obj) = current {
            obj.insert(
                array_field.to_string(),
                serde_json::Value::Array(Vec::new()),
            );
        } else {
            return;
        }
    }
    if let Some(serde_json::Value::Array(arr)) = current.get_mut(*array_field) {
        if index < arr.len() {
            arr[index] = value;
        } else {
            arr.push(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── config_field_value ───────────────────────────────────────────────────

    #[test]
    fn config_field_value_returns_string_field() {
        let json = serde_json::json!({"agent": "claude", "model": null});
        assert_eq!(
            config_field_value(&json, "agent"),
            Some("claude".to_string())
        );
    }

    #[test]
    fn config_field_value_returns_bool_field_as_string() {
        let json = serde_json::json!({"yolo": true});
        assert_eq!(config_field_value(&json, "yolo"), Some("true".to_string()));
    }

    #[test]
    fn config_field_value_returns_number_field_as_string() {
        let json = serde_json::json!({"port": 9876u64});
        assert_eq!(config_field_value(&json, "port"), Some("9876".to_string()));
    }

    #[test]
    fn config_field_value_returns_none_for_missing_field() {
        let json = serde_json::json!({"agent": "claude"});
        assert_eq!(config_field_value(&json, "nonexistent"), None);
    }

    #[test]
    fn config_field_value_returns_none_for_null_value() {
        let json = serde_json::json!({"model": null});
        assert_eq!(config_field_value(&json, "model"), None);
    }

    #[test]
    fn config_field_value_supports_dot_notation() {
        let json = serde_json::json!({"work_items": {"dir": "aspec/work-items"}});
        assert_eq!(
            config_field_value(&json, "work_items.dir"),
            Some("aspec/work-items".to_string())
        );
    }

    // ── set_config_field ─────────────────────────────────────────────────────

    #[test]
    fn set_config_field_inserts_string_value() {
        let mut json = serde_json::json!({});
        set_config_field(
            &mut json,
            "agent",
            serde_json::Value::String("codex".into()),
        );
        assert_eq!(json["agent"], serde_json::Value::String("codex".into()));
    }

    #[test]
    fn set_config_field_inserts_bool_value() {
        let mut json = serde_json::json!({});
        set_config_field(&mut json, "yolo", serde_json::Value::Bool(true));
        assert_eq!(json["yolo"], serde_json::Value::Bool(true));
    }

    #[test]
    fn set_config_field_inserts_number_value() {
        let mut json = serde_json::json!({});
        set_config_field(&mut json, "port", serde_json::json!(9876u64));
        assert_eq!(json["port"], serde_json::json!(9876u64));
    }

    #[test]
    fn set_config_field_overwrites_existing_value() {
        let mut json = serde_json::json!({"agent": "claude"});
        set_config_field(
            &mut json,
            "agent",
            serde_json::Value::String("gemini".into()),
        );
        assert_eq!(json["agent"], serde_json::Value::String("gemini".into()));
    }

    #[test]
    fn set_config_field_does_not_modify_non_object() {
        // If the json is not an Object, set_config_field is a no-op.
        let mut json = serde_json::Value::Null;
        set_config_field(
            &mut json,
            "agent",
            serde_json::Value::String("claude".into()),
        );
        // Should still be Null — no panic.
        assert!(json.is_null());
    }

    #[test]
    fn set_config_field_dot_notation_creates_nested() {
        let mut json = serde_json::json!({});
        set_config_field(
            &mut json,
            "work_items.dir",
            serde_json::Value::String("custom/dir".into()),
        );
        assert_eq!(json["work_items"]["dir"], "custom/dir");
    }

    #[test]
    fn set_config_field_dot_notation_preserves_siblings() {
        let mut json = serde_json::json!({"work_items": {"template": "tmpl.md"}});
        set_config_field(
            &mut json,
            "work_items.dir",
            serde_json::Value::String("custom/dir".into()),
        );
        assert_eq!(json["work_items"]["dir"], "custom/dir");
        assert_eq!(json["work_items"]["template"], "tmpl.md");
    }

    // ── set_config_field: dynamicWorkflows.guidance array append (WI-0099) ────

    #[test]
    fn set_config_field_guidance_appends_to_new_array() {
        let mut json = serde_json::json!({});
        set_config_field(
            &mut json,
            "dynamicWorkflows.guidance.0",
            serde_json::Value::String("first entry".into()),
        );
        assert_eq!(
            json["dynamicWorkflows"]["guidance"],
            serde_json::json!(["first entry"]),
            "setting index 0 on an absent array must create it: {json}"
        );
    }

    #[test]
    fn set_config_field_guidance_appends_at_current_length() {
        let mut json = serde_json::json!({
            "dynamicWorkflows": {"guidance": ["first entry"]}
        });
        set_config_field(
            &mut json,
            "dynamicWorkflows.guidance.1",
            serde_json::Value::String("second entry".into()),
        );
        assert_eq!(
            json["dynamicWorkflows"]["guidance"],
            serde_json::json!(["first entry", "second entry"]),
            "index == current length must append, not overwrite: {json}"
        );
    }

    #[test]
    fn set_config_field_guidance_overwrites_in_place_when_index_in_range() {
        let mut json = serde_json::json!({
            "dynamicWorkflows": {"guidance": ["a", "b"]}
        });
        set_config_field(
            &mut json,
            "dynamicWorkflows.guidance.0",
            serde_json::Value::String("a-edited".into()),
        );
        assert_eq!(
            json["dynamicWorkflows"]["guidance"],
            serde_json::json!(["a-edited", "b"]),
            "index < length must overwrite in place: {json}"
        );
    }

    // ── remove_config_field: dynamicWorkflows.guidance by index (WI-0099) ─────

    #[test]
    fn remove_config_field_guidance_by_index_compacts_and_reindexes() {
        let mut json = serde_json::json!({
            "dynamicWorkflows": {"guidance": ["a", "b", "c"]}
        });
        remove_config_field(&mut json, "dynamicWorkflows.guidance.1");
        assert_eq!(
            json["dynamicWorkflows"]["guidance"],
            serde_json::json!(["a", "c"]),
            "removing index 1 must leave a compact, re-indexed array: {json}"
        );
    }

    #[test]
    fn remove_config_field_guidance_out_of_range_index_is_noop() {
        let mut json = serde_json::json!({
            "dynamicWorkflows": {"guidance": ["a"]}
        });
        remove_config_field(&mut json, "dynamicWorkflows.guidance.5");
        assert_eq!(
            json["dynamicWorkflows"]["guidance"],
            serde_json::json!(["a"]),
            "an out-of-range index must not modify the array: {json}"
        );
    }

    // ── validate_and_coerce ──────────────────────────────────────────────────

    #[test]
    fn validate_and_coerce_agent_valid() {
        let v = validate_and_coerce("agent", "claude").unwrap();
        assert_eq!(v, serde_json::Value::String("claude".into()));
    }

    #[test]
    fn validate_and_coerce_agent_invalid() {
        let err = validate_and_coerce("agent", "notareal").unwrap_err();
        assert!(err.contains("not a known agent"));
    }

    #[test]
    fn validate_and_coerce_list_field() {
        let v = validate_and_coerce("yoloDisallowedTools", "tool1, tool2, tool3").unwrap();
        assert_eq!(v, serde_json::json!(["tool1", "tool2", "tool3"]));
    }

    #[test]
    fn validate_and_coerce_number_field() {
        let v = validate_and_coerce("terminal_scrollback_lines", "5000").unwrap();
        assert_eq!(v, serde_json::json!(5000u64));
    }

    #[test]
    fn validate_and_coerce_number_field_invalid() {
        let err = validate_and_coerce("terminal_scrollback_lines", "abc").unwrap_err();
        assert!(err.contains("not a valid number"));
    }

    #[test]
    fn validate_and_coerce_default_bool() {
        assert_eq!(
            validate_and_coerce("some_field", "true").unwrap(),
            serde_json::Value::Bool(true)
        );
    }

    #[test]
    fn validate_and_coerce_default_string() {
        assert_eq!(
            validate_and_coerce("some_field", "hello").unwrap(),
            serde_json::Value::String("hello".into())
        );
    }

    // ── validate_and_coerce: dynamicWorkflows (WI-0095) ─────────────────────

    #[test]
    fn validate_and_coerce_max_concurrent_steps_accepts_positive() {
        let v = validate_and_coerce("dynamicWorkflows.maxConcurrentSteps", "3").unwrap();
        assert_eq!(v, serde_json::json!(3u64));
    }

    #[test]
    fn validate_and_coerce_max_concurrent_steps_rejects_zero() {
        let err = validate_and_coerce("dynamicWorkflows.maxConcurrentSteps", "0").unwrap_err();
        assert!(
            err.contains("must be >= 1"),
            "zero must be rejected with the >= 1 message, got: {err}"
        );
    }

    #[test]
    fn validate_and_coerce_default_leader_valid_shape() {
        let v = validate_and_coerce("dynamicWorkflows.defaultLeader", "claude::claude-opus-4-8")
            .unwrap();
        assert_eq!(
            v,
            serde_json::Value::String("claude::claude-opus-4-8".into())
        );
    }

    #[test]
    fn validate_and_coerce_default_leader_invalid_shape() {
        let err = validate_and_coerce("dynamicWorkflows.defaultLeader", "claude").unwrap_err();
        assert!(
            err.contains("expected agent::model"),
            "malformed leader must be rejected, got: {err}"
        );
    }

    #[test]
    fn validate_and_coerce_rejects_agents_to_models_map() {
        // The map field itself cannot be written through `config set`;
        // without this rejection the coerced string would fail RepoConfig
        // deserialization and the set would silently write nothing.
        let err = validate_and_coerce("dynamicWorkflows.agentsToModels", "claude:foo").unwrap_err();
        assert!(
            err.contains("dynamicWorkflows.agentsToModels.claude"),
            "the map rejection must point at the per-agent syntax, got: {err}"
        );
    }

    #[test]
    fn validate_and_coerce_agents_to_models_entry_parses_comma_list() {
        let v = validate_and_coerce(
            "dynamicWorkflows.agentsToModels.claude",
            "opus-4-8, sonnet-4-6",
        )
        .unwrap();
        assert_eq!(v, serde_json::json!(["opus-4-8", "sonnet-4-6"]));
    }

    #[test]
    fn validate_and_coerce_agents_to_models_entry_empty_value_means_remove() {
        let v = validate_and_coerce("dynamicWorkflows.agentsToModels.claude", "  ").unwrap();
        assert!(
            v.is_null(),
            "an empty model list must coerce to Null (entry removal), got: {v:?}"
        );
    }

    #[test]
    fn validate_and_coerce_agents_to_models_entry_rejects_bad_agent_key() {
        let err =
            validate_and_coerce("dynamicWorkflows.agentsToModels.bad name!", "m1").unwrap_err();
        assert!(
            err.contains("not a valid agent name"),
            "invalid agent keys must be rejected, got: {err}"
        );
    }

    // ── validate_and_coerce: dynamicWorkflows.guidance (WI-0099) ────────────

    #[test]
    fn validate_and_coerce_rejects_bare_guidance_field() {
        // The bare array field can't be set as a single string; the coerced
        // string would fail RepoConfig deserialization and silently write
        // nothing, so it must be rejected up front pointing at the per-index
        // syntax.
        let err = validate_and_coerce("dynamicWorkflows.guidance", "some instruction").unwrap_err();
        assert!(
            err.contains("dynamicWorkflows.guidance.0"),
            "the list rejection must point at the per-index syntax, got: {err}"
        );
    }

    #[test]
    fn validate_and_coerce_guidance_entry_valid() {
        let v = validate_and_coerce(
            "dynamicWorkflows.guidance.0",
            "never spawn more than two agents in parallel",
        )
        .unwrap();
        assert_eq!(
            v,
            serde_json::Value::String("never spawn more than two agents in parallel".into())
        );
    }

    #[test]
    fn validate_and_coerce_guidance_entry_is_not_comma_split() {
        // Unlike agentsToModels, a guidance entry is one free-form string that
        // may itself contain commas.
        let v = validate_and_coerce("dynamicWorkflows.guidance.0", "do X, then Y, then Z").unwrap();
        assert_eq!(v, serde_json::Value::String("do X, then Y, then Z".into()));
    }

    #[test]
    fn validate_and_coerce_guidance_entry_empty_value_means_remove() {
        let v = validate_and_coerce("dynamicWorkflows.guidance.0", "   ").unwrap();
        assert!(
            v.is_null(),
            "an empty/whitespace-only guidance value must coerce to Null (entry removal), got: {v:?}"
        );
    }

    #[test]
    fn validate_and_coerce_guidance_entry_rejects_over_length_cap() {
        let too_long = "a".repeat(crate::data::config::repo::GUIDANCE_MAX_ENTRY_LEN + 1);
        let err = validate_and_coerce("dynamicWorkflows.guidance.0", &too_long).unwrap_err();
        assert!(
            err.contains("maximum"),
            "an over-length guidance entry must be rejected, got: {err}"
        );
    }

    // ── per-agent field name validation / scope ─────────────────────────────

    #[test]
    fn agents_to_models_entries_are_valid_repo_only_fields() {
        assert!(is_valid_field_name(
            "dynamicWorkflows.agentsToModels.claude"
        ));
        assert_eq!(
            field_scope("dynamicWorkflows.agentsToModels.claude"),
            Some(FieldScope::RepoOnly)
        );
        // The bare map and deeper paths are not per-agent entries.
        assert!(agents_to_models_entry_key("dynamicWorkflows.agentsToModels").is_none());
        assert!(agents_to_models_entry_key("dynamicWorkflows.agentsToModels.a.b").is_none());
    }

    #[test]
    fn guidance_entries_are_valid_repo_only_fields() {
        assert!(is_valid_field_name("dynamicWorkflows.guidance.0"));
        assert!(is_valid_field_name("dynamicWorkflows.guidance.41"));
        assert_eq!(
            field_scope("dynamicWorkflows.guidance.0"),
            Some(FieldScope::RepoOnly)
        );
        // The bare array and non-numeric or dotted trailing segments are not
        // per-index guidance entries.
        assert!(guidance_entry_index("dynamicWorkflows.guidance").is_none());
        assert!(guidance_entry_index("dynamicWorkflows.guidance.abc").is_none());
        assert!(guidance_entry_index("dynamicWorkflows.guidance.0.1").is_none());
    }

    // ── updated_config (silent-save-failure fix) ─────────────────────────────

    #[test]
    fn updated_config_applies_dynamic_workflows_fields() {
        let cfg = crate::data::config::repo::RepoConfig::default();
        let updated = updated_config(
            &cfg,
            "dynamicWorkflows.defaultLeader",
            serde_json::Value::String("claude::claude-opus-4-8".into()),
        )
        .expect("a valid defaultLeader must round-trip through the schema");
        assert_eq!(
            updated
                .dynamic_workflows
                .expect("dynamicWorkflows block must be created")
                .default_leader
                .as_deref(),
            Some("claude::claude-opus-4-8")
        );
    }

    #[test]
    fn updated_config_reports_schema_mismatch_instead_of_dropping_edit() {
        // `workItems` is an object in the schema; a plain string used to be
        // silently discarded (nothing saved, no error shown). The mismatch
        // must now surface a reason naming the field.
        let cfg = crate::data::config::repo::RepoConfig::default();
        let err = updated_config(&cfg, "workItems", serde_json::Value::String("oops".into()))
            .expect_err("schema-mismatched values must be rejected, not ignored");
        assert!(
            err.contains("workItems"),
            "the reason must name the field: {err}"
        );
    }

    // ── remove_config_field / apply_config_field ─────────────────────────────

    #[test]
    fn apply_config_field_null_removes_nested_entry() {
        let mut json = serde_json::json!({
            "dynamicWorkflows": {
                "agentsToModels": {"claude": ["a"], "codex": ["b"]}
            }
        });
        apply_config_field(
            &mut json,
            "dynamicWorkflows.agentsToModels.claude",
            serde_json::Value::Null,
        );
        assert!(
            json["dynamicWorkflows"]["agentsToModels"]
                .get("claude")
                .is_none(),
            "Null must remove the entry: {json}"
        );
        assert_eq!(
            json["dynamicWorkflows"]["agentsToModels"]["codex"],
            serde_json::json!(["b"]),
            "sibling entries must be preserved"
        );
    }

    #[test]
    fn remove_config_field_missing_path_is_noop() {
        let mut json = serde_json::json!({"agent": "claude"});
        remove_config_field(&mut json, "dynamicWorkflows.agentsToModels.claude");
        assert_eq!(json, serde_json::json!({"agent": "claude"}));
    }

    #[test]
    fn config_field_value_renders_string_arrays_comma_separated() {
        // List values display in the same shape the user types them, so
        // inline edits round-trip.
        let json = serde_json::json!({"yoloDisallowedTools": ["rm", "curl"]});
        assert_eq!(
            config_field_value(&json, "yoloDisallowedTools"),
            Some("rm, curl".to_string())
        );
    }

    // ── collect_config_rows: dynamicWorkflows (WI-0095) ─────────────────────

    #[test]
    fn collect_config_rows_includes_dynamic_workflows_scalar_fields() {
        let global = serde_json::json!({});
        let repo = serde_json::json!({
            "dynamicWorkflows": {
                "defaultLeader": "claude::claude-opus-4-8",
                "maxConcurrentSteps": 3
            }
        });
        let rows = collect_config_rows(&global, &repo);

        let leader_row = rows
            .iter()
            .find(|r| r.field == "dynamicWorkflows.defaultLeader")
            .expect("flattened dynamicWorkflows.defaultLeader row must be present");
        assert_eq!(
            leader_row.repo_value.as_deref(),
            Some("claude::claude-opus-4-8")
        );
        assert!(!leader_row.read_only, "defaultLeader stays inline-editable");

        let steps_row = rows
            .iter()
            .find(|r| r.field == "dynamicWorkflows.maxConcurrentSteps")
            .expect("flattened dynamicWorkflows.maxConcurrentSteps row must be present");
        assert_eq!(steps_row.repo_value.as_deref(), Some("3"));
        assert!(
            !steps_row.read_only,
            "maxConcurrentSteps stays inline-editable"
        );
    }

    #[test]
    fn collect_config_rows_expands_agents_to_models_into_per_agent_rows() {
        let global = serde_json::json!({});
        let repo = serde_json::json!({
            "dynamicWorkflows": {
                "agentsToModels": {
                    "claude": ["claude-opus-4-8", "claude-sonnet-4-6"],
                    "codex": ["codex-mini-latest"]
                }
            }
        });
        let rows = collect_config_rows(&global, &repo);

        // The map itself appears as a read-only summary row (never a raw
        // JSON blob), so the mapping is discoverable even when empty.
        let summary_row = rows
            .iter()
            .find(|r| r.field == "dynamicWorkflows.agentsToModels")
            .expect("agentsToModels summary row must be present");
        assert_eq!(summary_row.repo_value.as_deref(), Some("2 agents mapped"));
        assert!(summary_row.read_only, "the summary row is not editable");

        let claude_row = rows
            .iter()
            .find(|r| r.field == "dynamicWorkflows.agentsToModels.claude")
            .expect("per-agent row for claude must be present");
        assert_eq!(
            claude_row.repo_value.as_deref(),
            Some("claude-opus-4-8, claude-sonnet-4-6")
        );
        assert!(
            !claude_row.read_only,
            "agentsToModels per-agent rows must be inline-editable"
        );
        assert!(
            !claude_row.global_writable && claude_row.repo_writable,
            "per-agent rows are repo-only"
        );

        let codex_row = rows
            .iter()
            .find(|r| r.field == "dynamicWorkflows.agentsToModels.codex")
            .expect("per-agent row for codex must be present");
        assert_eq!(codex_row.repo_value.as_deref(), Some("codex-mini-latest"));
    }

    #[test]
    fn collect_config_rows_shows_empty_map_summary_when_unset() {
        let rows = collect_config_rows(&serde_json::json!({}), &serde_json::json!({}));
        let summary_row = rows
            .iter()
            .find(|r| r.field == "dynamicWorkflows.agentsToModels")
            .expect("summary row must be present even with no mapping configured");
        assert_eq!(summary_row.repo_value.as_deref(), Some("(none)"));
    }

    // ── removed_field_message ────────────────────────────────────────────────

    #[test]
    fn env_passthrough_is_recognized_as_removed_field() {
        let msg = removed_field_message("envPassthrough")
            .expect("envPassthrough must be flagged as a removed field");
        assert!(
            msg.contains("env(VAR_NAME)") || msg.contains("overlays"),
            "removed-field message must steer users to the env() overlay form; got: {msg}"
        );
    }

    #[test]
    fn unknown_field_returns_no_removed_message() {
        assert!(removed_field_message("agent").is_none());
        assert!(removed_field_message("notARealField").is_none());
    }

    #[test]
    fn env_passthrough_is_not_in_valid_fields() {
        let names = valid_field_names();
        assert!(
            !names.contains(&"envPassthrough"),
            "envPassthrough must be removed from VALID_CONFIG_FIELDS"
        );
    }

    // ── field_scope ──────────────────────────────────────────────────────────

    #[test]
    fn field_scope_global_only() {
        assert_eq!(field_scope("runtime"), Some(FieldScope::GlobalOnly));
    }

    #[test]
    fn field_scope_repo_only() {
        assert_eq!(field_scope("work_items.dir"), Some(FieldScope::RepoOnly));
    }

    #[test]
    fn field_scope_both() {
        assert_eq!(field_scope("agent"), Some(FieldScope::Both));
    }

    // ── levenshtein ───────────────────────────────────────────────────────────

    #[test]
    fn levenshtein_identical_strings_is_zero() {
        assert_eq!(levenshtein("agent", "agent"), 0);
    }

    #[test]
    fn levenshtein_empty_string_is_length_of_other() {
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
    }

    #[test]
    fn levenshtein_one_substitution() {
        assert_eq!(levenshtein("cat", "cut"), 1);
    }

    #[test]
    fn levenshtein_one_insertion() {
        assert_eq!(levenshtein("agent", "agents"), 1);
    }

    #[test]
    fn levenshtein_one_deletion() {
        assert_eq!(levenshtein("agents", "agent"), 1);
    }

    // ── levenshtein_suggestions ───────────────────────────────────────────────

    #[test]
    fn levenshtein_suggestions_finds_close_match() {
        let names = valid_field_names();
        let result = levenshtein_suggestions("agnet", &names);
        // "agnet" is distance 2 from "agent" (two transpositions); should appear.
        assert!(
            result.contains(&"agent"),
            "suggestions must contain 'agent' for input 'agnet': {result:?}"
        );
    }

    #[test]
    fn levenshtein_suggestions_empty_when_no_close_match() {
        let names = valid_field_names();
        let result = levenshtein_suggestions("zzzzzzzzzzz", &names);
        assert!(
            result.is_empty(),
            "suggestions must be empty for very distant input"
        );
    }

    #[test]
    fn levenshtein_suggestions_sorted_by_distance() {
        let names = valid_field_names();
        // "runtim" is distance 1 from "runtime" and distance 2+ from all others.
        let result = levenshtein_suggestions("runtim", &names);
        if result.len() >= 2 {
            // First result must be "runtime" (closest match).
            assert_eq!(
                result[0], "runtime",
                "closest match must be first: {result:?}"
            );
        }
    }
}

#[cfg(test)]
mod edit_loop_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Scripted frontend for the interactive `config show` edit loop: returns
    /// the next canned response on each `present_config_table` call and
    /// records the rejection (if any) it was shown alongside the table.
    struct ScriptedConfigFrontend {
        responses: Vec<Option<ConfigEditRequest>>,
        seen_rejections: Arc<Mutex<Vec<Option<ConfigEditRejection>>>>,
    }

    impl crate::data::message::UserMessageSink for ScriptedConfigFrontend {
        fn write_message(&mut self, _msg: crate::data::message::UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    impl ConfigCommandFrontend for ScriptedConfigFrontend {
        fn present_config_table(
            &mut self,
            _rows: &[ConfigFieldRow],
            rejected: Option<&ConfigEditRejection>,
        ) -> Result<Option<ConfigEditRequest>, CommandError> {
            self.seen_rejections.lock().unwrap().push(rejected.cloned());
            Ok(self.responses.remove(0))
        }
    }

    fn make_engines() -> crate::command::dispatch::Engines {
        let runtime = Arc::new(crate::engine::container::ContainerRuntime::docker());
        let overlay = Arc::new(crate::engine::overlay::OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(std::path::PathBuf::from(
                "/tmp",
            )),
        ));
        let git_engine = Arc::new(crate::engine::git::GitEngine::new());
        let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            overlay.clone(),
            runtime.clone(),
        ));
        let auth_engine = Arc::new(crate::engine::auth::AuthEngine::with_paths(
            crate::data::fs::auth_paths::AuthPathResolver::at_home("/tmp"),
            crate::data::fs::api_paths::ApiPaths::at_root("/tmp"),
        ));
        let workflow_state_store = {
            let tmp = tempfile::tempdir().unwrap();
            Arc::new(crate::data::EngineWorkflowStateStore::at_git_root(
                tmp.path(),
            ))
        };
        crate::command::dispatch::Engines {
            runtime: runtime.clone(),
            container_runtime: Some(runtime),
            sandbox_runtime: None,
            git_engine,
            overlay_engine: overlay,
            auth_engine,
            agent_engine,
            workflow_state_store,
        }
    }

    fn open_session(git_root: &std::path::Path) -> crate::data::session::Session {
        crate::data::session::Session::open_at_git_root(
            git_root.to_path_buf(),
            git_root.to_path_buf(),
            crate::data::session::SessionOpenOptions::default(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn show_edit_loop_preserves_rejected_input_and_persists_valid_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let frontend = ScriptedConfigFrontend {
            responses: vec![
                // 1: invalid leader — must come back as a rejection carrying
                // the typed value, not silently vanish.
                Some(ConfigEditRequest {
                    field: "dynamicWorkflows.defaultLeader".into(),
                    value: "claude".into(),
                    global: false,
                }),
                // 2: corrected value — must persist to .awman/config.json.
                Some(ConfigEditRequest {
                    field: "dynamicWorkflows.defaultLeader".into(),
                    value: "claude::claude-opus-4-8".into(),
                    global: false,
                }),
                // 3: dismiss the dialog.
                None,
            ],
            seen_rejections: seen.clone(),
        };

        let cmd = ConfigCommand::new(
            ConfigSubcommand::Show(ConfigShowFlags {}),
            make_engines(),
            open_session(tmp.path()),
        );
        cmd.run_with_frontend(Box::new(frontend))
            .await
            .expect("the edit loop must not error on a rejected value");

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3, "the table must be presented three times");
        assert!(seen[0].is_none(), "no rejection on first present");
        let rej = seen[1]
            .as_ref()
            .expect("an invalid edit must be re-presented as a rejection");
        assert_eq!(rej.field, "dynamicWorkflows.defaultLeader");
        assert_eq!(rej.value, "claude", "the user's input must be preserved");
        assert!(
            rej.reason.contains("expected agent::model"),
            "the rejection must carry the validation reason: {}",
            rej.reason
        );
        assert!(
            seen[2].is_none(),
            "a successful save must not carry a rejection"
        );

        let cfg = crate::data::config::repo::RepoConfig::load(tmp.path())
            .expect("the saved repo config must load cleanly");
        assert_eq!(
            cfg.dynamic_workflows
                .expect("dynamicWorkflows must be persisted")
                .default_leader
                .as_deref(),
            Some("claude::claude-opus-4-8"),
            "the corrected edit must be persisted to config.json"
        );
    }

    #[tokio::test]
    async fn set_reports_schema_mismatch_instead_of_silent_success() {
        let tmp = tempfile::tempdir().unwrap();
        let frontend = ScriptedConfigFrontend {
            responses: vec![],
            seen_rejections: Arc::new(Mutex::new(Vec::new())),
        };

        let cmd = ConfigCommand::new(
            ConfigSubcommand::Set(ConfigSetFlags {
                field: "workItems".into(),
                value: "not-an-object".into(),
                global: false,
            }),
            make_engines(),
            open_session(tmp.path()),
        );
        let err = cmd
            .run_with_frontend(Box::new(frontend))
            .await
            .expect_err("a value the schema cannot hold must fail the command");
        assert!(
            err.to_string().contains("workItems"),
            "the error must name the field: {err}"
        );
        assert!(
            !crate::data::config::repo::RepoConfig::path(tmp.path()).exists(),
            "nothing must be written when the set fails"
        );
    }
}
