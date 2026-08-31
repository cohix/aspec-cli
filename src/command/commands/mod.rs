//! `src/command/commands/` — one struct per awman command.
//!
//! Each module contains the `*Command` struct (owning every flag value and
//! engine reference it needs), its `*CommandFrontend` trait (defining the
//! exact user-input methods that command requires), and the
//! `Command` impl whose `run_with_frontend(frontend) -> *Outcome` body holds
//! all of the command's business logic.

pub mod agent_auth;
pub mod agent_setup;
pub mod amie;
pub mod api_server;
pub mod auth;
pub mod chat;
pub mod clean;
pub mod command_trait;
pub mod config;
pub mod download;
pub mod exec_prompt;
pub mod exec_workflow;
pub(crate) mod http_core;
// HttpCore is the shared transport seam used by daemon-facing clients and
// must be reachable by integration consumers without exposing its module's
// implementation registry.
pub use http_core::HttpCore;
pub(crate) mod dynamic_repair;
// The WI-0092 leader/repair budget is the one decision core `exec workflow
// --dynamic` and the amie evaluator share; both callers — and the tests that
// prove they behave identically — reach it through this re-export.
pub use dynamic_repair::{RepairDecision, WorkflowRepairLoop};
pub mod init;
pub mod mount_scope;
pub mod new;
pub mod prompt_templates;
pub mod ready;
pub mod remote;
pub(crate) mod remote_client;
pub mod skill_library;
pub mod specs;
pub mod status;
pub mod status_tips;
pub mod worktree_lifecycle;

pub use command_trait::Command;

/// Result of resolving an ACP request for a concrete agent.
///
/// This is deliberately command-layer policy: the engine remains the final
/// safety guard, while this decision determines whether a repository default
/// may use the configured fallback before any setup, overlay, or runtime work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaunchModeDecision {
    Stdio,
    Acp,
    StdioWithFallbackWarning,
}

#[cfg(test)]
mod launch_mode_tests {
    use super::*;
    use crate::data::config::global::{GlobalConfig, LaunchModeFallback};
    use crate::data::config::repo::LaunchMode;

    fn config(fallback: LaunchModeFallback) -> crate::data::config::effective::EffectiveConfig {
        crate::data::config::effective::EffectiveConfig::new(
            crate::data::config::FlagConfig {
                launch_mode: Some(LaunchMode::Acp),
                ..Default::default()
            },
            Default::default(),
            Default::default(),
            GlobalConfig {
                launch_mode_fallback: Some(fallback),
                ..Default::default()
            },
        )
    }

    #[test]
    fn fallback_decision_matrix() {
        let supported = crate::data::session::AgentName::new("cline").unwrap();
        let unsupported = crate::data::session::AgentName::new("claude").unwrap();

        assert_eq!(
            resolve_launch_mode(&config(LaunchModeFallback::Error), &supported, false).unwrap(),
            LaunchModeDecision::Acp
        );
        assert!(matches!(
            resolve_launch_mode(&config(LaunchModeFallback::Error), &unsupported, false),
            Err(crate::engine::error::EngineError::AcpUnsupported { .. })
        ));
        assert_eq!(
            resolve_launch_mode(&config(LaunchModeFallback::Stdio), &unsupported, false).unwrap(),
            LaunchModeDecision::StdioWithFallbackWarning
        );
        assert!(matches!(
            resolve_launch_mode(&config(LaunchModeFallback::Stdio), &unsupported, true),
            Err(crate::engine::error::EngineError::AcpUnsupported { .. })
        ));
    }
}

/// Apply the ACP support and fallback policy after normal agent resolution.
/// Explicit single-agent ACP intent is never downgraded.
pub(crate) fn resolve_launch_mode(
    config: &crate::data::config::effective::EffectiveConfig,
    agent: &crate::data::session::AgentName,
    explicit_single_agent_acp: bool,
) -> Result<LaunchModeDecision, crate::engine::error::EngineError> {
    use crate::data::config::global::LaunchModeFallback;
    use crate::data::config::repo::LaunchMode;

    if config.launch_mode() == LaunchMode::Stdio {
        return Ok(LaunchModeDecision::Stdio);
    }
    if crate::engine::agent::agent_matrix::matrix_for(agent.as_str())?.supports_acp {
        return Ok(LaunchModeDecision::Acp);
    }
    if explicit_single_agent_acp || config.launch_mode_fallback() == LaunchModeFallback::Error {
        return Err(crate::engine::error::EngineError::AcpUnsupported {
            agent: agent.as_str().to_string(),
        });
    }
    Ok(LaunchModeDecision::StdioWithFallbackWarning)
}

pub(crate) fn acp_fallback_warning(agent: &crate::data::session::AgentName) -> String {
    format!(
        "agent '{}' does not support ACP; falling back to stdio for this session — see launchModeFallback",
        agent.as_str()
    )
}

/// Resolve the agent name to use for a command, in precedence order:
///   1. explicit CLI flag (`flag`)
///   2. `session.default_agent()` (which itself resolves flag > repo > global)
///   3. fallback to `"claude"` so a fresh repo with no config still works.
///
/// Frontends (CLI / TUI / API session-setup) must funnel through this helper
/// so the agent choice is uniformly driven by `.awman/config.json`, with the
/// hard-coded fallback only used as a last resort.
pub fn resolve_agent(
    flag: &Option<String>,
    session: &crate::data::session::Session,
) -> Result<crate::data::session::AgentName, crate::command::error::CommandError> {
    use crate::command::error::CommandError;
    use crate::data::session::AgentName;

    if let Some(name) = flag.as_deref() {
        return AgentName::new(name).map_err(CommandError::from);
    }
    if let Some(name) = session.default_agent() {
        return Ok(name.clone());
    }
    AgentName::new("claude").map_err(CommandError::from)
}

/// Report the end of an interactive agent session on the sink, reflecting
/// how it actually ended: Info for a clean exit, Error with the exit code
/// when the agent exited non-zero, Error when waiting on the agent itself
/// failed. Shared by `chat` and `exec prompt` so a failed launch is never
/// reported as a quiet "Agent session ended".
pub(crate) fn report_session_end(
    sink: &mut dyn crate::data::message::UserMessageSink,
    command: &str,
    exit: &Result<
        crate::engine::agent_runtime::execution::AgentExitInfo,
        crate::engine::error::EngineError,
    >,
) {
    use crate::data::message::{MessageLevel, UserMessage};
    let (level, text) = match exit {
        Ok(info) if info.exit_code == 0 => (MessageLevel::Info, "Agent session ended".to_string()),
        Ok(info) => (
            MessageLevel::Error,
            format!("Agent session ended with exit code {}", info.exit_code),
        ),
        Err(e) => (
            MessageLevel::Error,
            format!("{command}: failed to wait for agent exit: {e}"),
        ),
    };
    sink.write_message(UserMessage { level, text });
}

/// Specification for a skill overlay: all skills or a named one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSpec {
    All,
    Named(String),
}

pub use crate::engine::overlay::ContextScope;

/// A parsed context overlay specification (scope + permission).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextOverlaySpec {
    pub scope: ContextScope,
    pub permission: crate::engine::container::options::OverlayPermission,
}

/// A parsed overlay expression: directory mount, skill, env passthrough, or context.
#[derive(Debug, Clone, PartialEq)]
pub enum TypedOverlay {
    Directory(crate::engine::overlay::DirectorySpec),
    Skill(SkillSpec),
    Env(String),
    Context(ContextOverlaySpec),
}

/// Aggregated overlay information after collecting from all sources.
#[derive(Debug)]
pub struct CollectedOverlays {
    pub directories: Vec<crate::engine::overlay::DirectorySpec>,
    pub include_all_skills: bool,
    pub named_skills: Vec<String>,
    pub env_passthrough: Vec<String>,
    pub context_overlays: Vec<ContextOverlaySpec>,
}

/// Parse a user-supplied overlay spec string in the form
/// `host:container` or `host:container:perm` (where perm is `ro` or `rw`).
///
/// Returns the parsed `DirectorySpec` or a descriptive error string on failure.
pub fn parse_overlay_spec(spec: &str) -> Result<crate::engine::overlay::DirectorySpec, String> {
    use crate::engine::container::options::OverlayPermission;
    use crate::engine::overlay::DirectorySpec;

    let parts: Vec<&str> = spec.splitn(3, ':').collect();
    if parts.len() < 2 {
        return Err(format!(
            "expected 'host:container' or 'host:container:perm', got '{spec}'"
        ));
    }
    let host = parts[0].to_string();
    if host.is_empty() {
        return Err("host path must not be empty".to_string());
    }
    let container = parts[1].to_string();
    if container.is_empty() {
        return Err("container path must not be empty".to_string());
    }
    if !container.starts_with('/') {
        return Err(format!("container path '{container}' must be absolute"));
    }
    let permission = match parts.get(2).copied() {
        None | Some("rw") | Some("") => OverlayPermission::ReadWrite,
        Some("ro") => OverlayPermission::ReadOnly,
        Some(other) => {
            return Err(format!(
                "unknown permission '{other}'; expected 'ro' or 'rw'"
            ));
        }
    };
    Ok(DirectorySpec {
        host,
        container,
        permission,
    })
}

/// Parse a comma-separated list of typed overlay expressions from the
/// `AWMAN_OVERLAYS` env var or config arrays.
///
/// Grammar: `dir(host:container[:perm])` or `skill()` expressions separated
/// by commas. Bare `host:container[:perm]` strings (no type tag) are accepted
/// as legacy shorthand for `dir(...)`. Commas inside parentheses are ignored
/// (paren-aware splitting).
pub fn parse_overlay_list(input: &str) -> Result<Vec<TypedOverlay>, String> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(vec![]);
    }
    let mut results = Vec::new();
    for expr in split_top_level_commas(input) {
        let expr = expr.trim();
        if expr.is_empty() {
            continue;
        }
        results.push(parse_single_typed_overlay(expr)?);
    }
    Ok(results)
}

/// Split on commas not inside parentheses.
fn split_top_level_commas(input: &str) -> Vec<&str> {
    let mut results = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (i, ch) in input.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                results.push(&input[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    results.push(&input[start..]);
    results
}

/// Parse a single typed overlay expression like `dir(/host:/container:ro)`
/// or `skill()`. If the input has no parentheses, it is treated as a legacy
/// bare path spec (`host:container[:perm]`).
fn parse_single_typed_overlay(expr: &str) -> Result<TypedOverlay, String> {
    if !expr.contains('(') {
        return parse_overlay_spec(expr).map(TypedOverlay::Directory);
    }
    let open = expr
        .find('(')
        .ok_or_else(|| format!("malformed overlay expression (missing '('): '{expr}'"))?;
    let close = expr
        .rfind(')')
        .ok_or_else(|| format!("malformed overlay expression (missing ')'): '{expr}'"))?;
    if close <= open {
        return Err(format!(
            "malformed overlay expression (parentheses out of order): '{expr}'"
        ));
    }
    let tag = expr[..open].trim();
    let args = expr[open + 1..close].trim();
    match tag {
        "dir" => parse_dir_overlay_args(args, expr).map(TypedOverlay::Directory),
        "skill" => {
            if args.is_empty() {
                return Err("skill() requires an argument; use skill(*) to mount all skills or skill(name) for a specific named skill".to_string());
            }
            if args.contains(',') {
                return Err("skill() takes one argument; use separate skill() calls for multiple named skills".to_string());
            }
            if args == "*" {
                Ok(TypedOverlay::Skill(SkillSpec::All))
            } else {
                if args.matches('/').count() > 1 {
                    return Err(format!(
                        "skill(name) supports at most one '/' (library/skill); got '{args}'"
                    ));
                }
                // Each segment must be a single, contained path component:
                // an empty, '.' or '..' segment is joined onto a host skills
                // path at mount time and would resolve somewhere the reference
                // never named (e.g. `skill(lib/..)` = the whole clone).
                if args
                    .split('/')
                    .any(|segment| segment.is_empty() || segment == "." || segment == "..")
                {
                    return Err(format!(
                        "skill(name) segments must not be empty, '.' or '..'; got '{args}'"
                    ));
                }
                Ok(TypedOverlay::Skill(SkillSpec::Named(args.to_string())))
            }
        }
        "skills" => {
            Err("skills() has been removed; use skill(*) to mount all skills or skill(name) for a specific named skill".to_string())
        }
        "ssh" => {
            if !args.is_empty() {
                return Err("ssh() takes no arguments".to_string());
            }
            let home = dirs::home_dir().unwrap_or_default();
            let ssh_host = home.join(".ssh").to_string_lossy().into_owned();
            Ok(TypedOverlay::Directory(crate::engine::overlay::DirectorySpec {
                host: ssh_host,
                container: "~/.ssh".to_string(),
                permission: crate::engine::container::options::OverlayPermission::ReadOnly,
            }))
        }
        "env" => {
            if args.is_empty() {
                return Err("env() requires an argument".to_string());
            }
            if args.contains(',') {
                return Err("env() takes one argument; use separate env() calls for multiple vars".to_string());
            }
            Ok(TypedOverlay::Env(args.to_string()))
        }
        "context" => {
            if args.is_empty() {
                return Err("context() requires a scope argument: context(global), context(repo), or context(workflow)".to_string());
            }
            let parts: Vec<&str> = args.splitn(2, ':').collect();
            let scope_str = parts[0].trim();
            let scope = match scope_str {
                "global" => ContextScope::Global,
                "repo" => ContextScope::Repo,
                "workflow" => ContextScope::Workflow,
                other => {
                    return Err(format!(
                        "unknown context scope '{other}' in '{expr}'; \
                         supported scopes: global, repo, workflow"
                    ));
                }
            };
            let permission = if let Some(perm_str) = parts.get(1).map(|s| s.trim()) {
                match perm_str {
                    "ro" => crate::engine::container::options::OverlayPermission::ReadOnly,
                    "rw" => crate::engine::container::options::OverlayPermission::ReadWrite,
                    other => {
                        return Err(format!(
                            "unknown permission '{other}' in '{expr}'; expected 'ro' or 'rw'"
                        ));
                    }
                }
            } else {
                crate::engine::container::options::OverlayPermission::ReadWrite
            };
            Ok(TypedOverlay::Context(ContextOverlaySpec { scope, permission }))
        }
        _ => Err(format!(
            "unknown overlay type '{tag}' in '{expr}'; supported types: dir, skill, ssh, env, context"
        )),
    }
}

fn parse_dir_overlay_args(
    args: &str,
    full_expr: &str,
) -> Result<crate::engine::overlay::DirectorySpec, String> {
    use crate::engine::container::options::OverlayPermission;
    use crate::engine::overlay::DirectorySpec;

    if args.is_empty() {
        return Err(format!(
            "empty arguments in overlay expression: '{full_expr}'"
        ));
    }
    let parts: Vec<&str> = args.splitn(3, ':').collect();
    let (host_str, container_str, perm_str) = match parts.len() {
        2 => (parts[0], parts[1], None),
        3 => {
            let candidate = parts[2].trim();
            if candidate == "ro" || candidate == "rw" {
                (parts[0], parts[1], Some(candidate))
            } else {
                return Err(format!(
                    "invalid permission '{candidate}' in '{full_expr}'; expected 'ro' or 'rw'"
                ));
            }
        }
        _ => {
            return Err(format!("expected 'host:container[:perm]' in '{full_expr}'"));
        }
    };
    let host = host_str.trim();
    let container = container_str.trim();
    if host.is_empty() {
        return Err(format!("empty host path in '{full_expr}'"));
    }
    if container.is_empty() {
        return Err(format!("empty container path in '{full_expr}'"));
    }
    let permission = match perm_str {
        Some("ro") => OverlayPermission::ReadOnly,
        _ => OverlayPermission::ReadWrite,
    };
    let host_expanded = crate::data::fs::OverlayPathResolver::expand_tilde(host)
        .to_string_lossy()
        .into_owned();
    Ok(DirectorySpec {
        host: host_expanded,
        container: container.to_string(),
        permission,
    })
}

/// Collect all overlay specs from config sources (global, repo, env var, CLI flags)
/// and optional per-step overlays. Returns a `CollectedOverlays` with directories,
/// skill settings, and env passthrough vars.
pub fn collect_all_overlay_specs(
    session: &crate::data::session::Session,
    cli_typed_overlays: Vec<TypedOverlay>,
    workflow_overlays: Option<&[String]>,
    step_overlays: Option<&[String]>,
) -> Result<CollectedOverlays, crate::command::error::CommandError> {
    let ec = session.effective_config();
    let mut dirs = Vec::new();
    let mut include_all_skills = false;
    let mut named_skills: Vec<String> = Vec::new();
    let mut env_passthrough: Vec<String> = Vec::new();
    let mut context_overlays: Vec<ContextOverlaySpec> = Vec::new();

    let mut process_typed = |typed: TypedOverlay| match typed {
        TypedOverlay::Directory(spec) => dirs.push(spec),
        TypedOverlay::Skill(SkillSpec::All) => include_all_skills = true,
        TypedOverlay::Skill(SkillSpec::Named(name)) => {
            if !named_skills.contains(&name) {
                named_skills.push(name);
            }
        }
        TypedOverlay::Env(var) => {
            if !env_passthrough.contains(&var) {
                env_passthrough.push(var);
            }
        }
        TypedOverlay::Context(spec) => {
            if !context_overlays.iter().any(|c| c.scope == spec.scope) {
                context_overlays.push(spec);
            }
        }
    };

    // 1. Global config overlays (lowest priority).
    if let Some(overlay_strs) = ec.global().overlays.as_ref() {
        for s in overlay_strs {
            let parsed = parse_overlay_list(s).map_err(|reason| {
                crate::command::error::CommandError::InvalidOverlaySpec {
                    spec: s.clone(),
                    reason,
                }
            })?;
            for typed in parsed {
                process_typed(typed);
            }
        }
    }

    // 2. Repo config overlays.
    if let Some(overlay_strs) = ec.repo().overlays.as_ref() {
        for s in overlay_strs {
            let parsed = parse_overlay_list(s).map_err(|reason| {
                crate::command::error::CommandError::InvalidOverlaySpec {
                    spec: s.clone(),
                    reason,
                }
            })?;
            for typed in parsed {
                process_typed(typed);
            }
        }
    }

    // 3. AWMAN_OVERLAYS env var.
    if let Some(env_str) = ec.env().overlays() {
        let parsed = parse_overlay_list(env_str).map_err(|reason| {
            crate::command::error::CommandError::InvalidOverlaySpec {
                spec: format!("AWMAN_OVERLAYS: {env_str}"),
                reason,
            }
        })?;
        for typed in parsed {
            process_typed(typed);
        }
    }

    // 4. CLI flag overlays (highest priority).
    for typed in cli_typed_overlays {
        process_typed(typed);
    }

    // 5. Workflow-level overlays (between CLI and step).
    if let Some(wf_strs) = workflow_overlays {
        for s in wf_strs {
            let parsed = parse_overlay_list(s).map_err(|reason| {
                crate::command::error::CommandError::InvalidOverlaySpec {
                    spec: s.clone(),
                    reason,
                }
            })?;
            for typed in parsed {
                process_typed(typed);
            }
        }
    }

    // 6. Per-step overlays (highest priority).
    if let Some(step_strs) = step_overlays {
        for s in step_strs {
            let parsed = parse_overlay_list(s).map_err(|reason| {
                crate::command::error::CommandError::InvalidOverlaySpec {
                    spec: s.clone(),
                    reason,
                }
            })?;
            for typed in parsed {
                process_typed(typed);
            }
        }
    }

    Ok(CollectedOverlays {
        directories: dirs,
        include_all_skills,
        named_skills,
        env_passthrough,
        context_overlays,
    })
}

/// Resolve `ContextOverlaySpec`s into `ContextOverlay`s (host paths ensured-to-exist)
/// and build the combined system prompt. Used by `chat`, `exec_prompt`, and
/// `exec_workflow`.
///
/// `workflow_step_info` is `Some` when running inside a multi-step workflow
/// (provides step progress for the prompt); `None` for chat / exec_prompt.
///
/// `workflow_invocation_id` keys the workflow context directory. Inside a
/// workflow, pass `WorkflowState::invocation_id` so resumed runs reuse the same
/// dir. For `chat`/`exec prompt`, pass `None` and the session UUID is used as
/// a stable per-session fallback.
///
/// `agent` is used to emit `Warning`/`Info` `UserMessage`s when the agent's
/// system-prompt delivery is degraded (`Replace`) or unsupported.
pub fn resolve_context_overlays(
    specs: &[ContextOverlaySpec],
    session: &crate::data::session::Session,
    agent: &crate::data::session::AgentName,
    workflow_invocation_id: Option<uuid::Uuid>,
    workflow_step_info: Option<&crate::engine::context_prompt::WorkflowStepInfo>,
    sink: &mut dyn crate::data::message::UserMessageSink,
) -> Result<
    (Vec<crate::engine::overlay::ContextOverlay>, Option<String>),
    crate::command::error::CommandError,
> {
    use crate::data::fs::ContextDirResolver;
    use crate::data::message::{MessageLevel, UserMessage};
    use crate::engine::agent::agent_matrix::{SystemPromptMode, matrix_for};
    use crate::engine::context_prompt::ContextPromptBuilder;
    use crate::engine::overlay::ContextOverlay;

    if specs.is_empty() {
        return Ok((vec![], None));
    }

    let resolver = ContextDirResolver::from_process_env().map_err(|e| {
        crate::command::error::CommandError::Other(format!("context dir resolver: {e}"))
    })?;

    let mut overlays = Vec::new();
    let mut builder = ContextPromptBuilder::new();
    let mut saw_repo_fallback: Option<std::path::PathBuf> = None;

    for spec in specs {
        let (host_path, container_path) = match spec.scope {
            ContextScope::Global => {
                let p = resolver.global_dir();
                (p, std::path::PathBuf::from("/awman/context/global"))
            }
            ContextScope::Repo => {
                let p = resolver.repo_dir(session.git_root());
                // Detect the `_local/...` fallback so we can surface an Info
                // message naming the resolved dir.
                if let Some(suffix) = p.iter().rev().nth(1).and_then(|c| c.to_str()) {
                    if suffix == "_local" {
                        saw_repo_fallback = Some(p.clone());
                    }
                }
                (p, std::path::PathBuf::from("/awman/context/repo"))
            }
            ContextScope::Workflow => {
                let uuid = workflow_invocation_id.unwrap_or_else(|| session.id().as_uuid());
                let p = resolver.workflow_dir(uuid);
                (p, std::path::PathBuf::from("/awman/context/workflow"))
            }
        };

        if let Err(e) = ContextDirResolver::ensure_exists(&host_path) {
            sink.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: format!(
                    "context overlay: failed to create directory {}: {e}",
                    host_path.display()
                ),
            });
            continue;
        }

        overlays.push(ContextOverlay {
            scope: spec.scope,
            host_path,
            container_path,
            permission: spec.permission,
        });

        match spec.scope {
            ContextScope::Global => {
                builder = builder.with_global();
            }
            ContextScope::Repo => {
                builder = builder.with_repo();
            }
            ContextScope::Workflow => {
                if let Some(info) = workflow_step_info {
                    builder = builder.with_workflow(info);
                } else {
                    sink.write_message(UserMessage {
                        level: MessageLevel::Info,
                        text: "context(workflow) is most useful inside a workflow; \
                               workflow state will be empty"
                            .to_string(),
                    });
                    builder = builder.with_workflow_oneshot();
                }
            }
        }
    }

    if let Some(path) = saw_repo_fallback {
        sink.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!(
                "context(repo): no git remote configured; using local fallback directory {}",
                path.display()
            ),
        });
    }

    // Surface agent-compatibility warnings now that we know context overlays
    // will be active for this run.
    if let Ok(matrix) = matrix_for(agent.as_str()) {
        match &matrix.system_prompt_delivery {
            SystemPromptMode::Replace => {
                sink.write_message(UserMessage {
                    level: MessageLevel::Warning,
                    text: format!(
                        "context overlay: '{}' replaces the default system prompt with the \
                         context instructions; a baseline preamble is prepended but you may \
                         see degraded tool-use guidance.",
                        agent.as_str()
                    ),
                });
            }
            SystemPromptMode::Unsupported => {
                sink.write_message(UserMessage {
                    level: MessageLevel::Warning,
                    text: format!(
                        "context() overlay mounted for '{}' but system prompt injection is \
                         not supported for this agent; the agent will not be automatically \
                         notified about the context directory.",
                        agent.as_str()
                    ),
                });
            }
            _ => {}
        }
    }

    let prompt = builder.build();
    Ok((overlays, prompt))
}

/// Emit deprecation warnings for legacy `envPassthrough` config fields.
pub fn warn_legacy_config(
    session: &crate::data::session::Session,
    sink: &mut dyn crate::data::message::UserMessageSink,
) {
    let ec = session.effective_config();
    if ec.repo().legacy_env_passthrough.is_some() {
        sink.write_message(crate::data::message::UserMessage {
            level: crate::data::message::MessageLevel::Warning,
            text: "'.awman/config.json' contains a deprecated 'envPassthrough' field. Move these vars to the 'overlays' array as env() expressions, e.g. \"env(VAR_NAME)\", then remove 'envPassthrough'.".into(),
        });
    }
    if ec.global().legacy_env_passthrough.is_some() {
        sink.write_message(crate::data::message::UserMessage {
            level: crate::data::message::MessageLevel::Warning,
            text: "'~/.awman/config.json' contains a deprecated 'envPassthrough' field. Move these vars to the 'overlays' array as env() expressions, e.g. \"env(VAR_NAME)\", then remove 'envPassthrough'.".into(),
        });
    }
}

#[cfg(test)]
mod skill_parser_tests {
    use super::*;

    #[test]
    fn skill_empty_returns_error() {
        let err = parse_overlay_list("skill()").unwrap_err();
        assert!(
            err.contains("requires an argument"),
            "error must mention 'requires an argument'; got: {err}"
        );
    }

    #[test]
    fn skill_star_parses_to_skill_all() {
        let result = parse_overlay_list("skill(*)").unwrap();
        assert_eq!(result, vec![TypedOverlay::Skill(SkillSpec::All)]);
    }

    #[test]
    fn skill_named_parses_to_skill_named() {
        let result = parse_overlay_list("skill(myskill)").unwrap();
        assert_eq!(
            result,
            vec![TypedOverlay::Skill(SkillSpec::Named("myskill".to_string()))]
        );
    }

    #[test]
    fn skill_library_slash_skill_parses_to_named() {
        // `skill(library/skill)` (exactly one slash) is a valid single-skill
        // reference into a pulled library — it parses as a Named spec.
        let result = parse_overlay_list("skill(superpowers/brainstorming)").unwrap();
        assert_eq!(
            result,
            vec![TypedOverlay::Skill(SkillSpec::Named(
                "superpowers/brainstorming".to_string()
            ))]
        );
    }

    #[test]
    fn skill_with_two_slashes_is_rejected() {
        // More than one slash is ambiguous (`library/skill` is the deepest
        // form) and must be rejected at parse time with a descriptive error.
        let err = parse_overlay_list("skill(a/b/c)").unwrap_err();
        assert!(
            err.contains("at most one '/'") && err.contains("a/b/c"),
            "error must explain the one-slash limit and echo the bad value; got: {err}"
        );
    }

    /// Segments are joined onto host skills paths at mount time, so a `.`,
    /// `..` or empty segment would resolve somewhere the reference never named
    /// — `skill(superpowers/..)` would mount the whole managed clone, `.git/`
    /// included (WI-0103 remediation).
    #[test]
    fn skill_with_traversal_or_empty_segments_is_rejected() {
        for bad in [
            "superpowers/..",
            "superpowers/.",
            "../superpowers",
            "./superpowers",
            "superpowers/",
            "/superpowers",
            "..",
            ".",
        ] {
            let expr = format!("skill({bad})");
            let err = parse_overlay_list(&expr)
                .err()
                .unwrap_or_else(|| panic!("'{expr}' must be rejected at parse time"));
            assert!(
                err.contains("must not be empty, '.' or '..'"),
                "'{expr}' must be rejected with the segment rule; got: {err}"
            );
        }
    }

    /// The new segment rule must not narrow what already parsed.
    #[test]
    fn skill_ordinary_names_still_parse_after_segment_validation() {
        for good in [
            "lint",
            "superpowers",
            "superpowers/brainstorming",
            "my.skill",
        ] {
            let expr = format!("skill({good})");
            let parsed = parse_overlay_list(&expr)
                .unwrap_or_else(|e| panic!("'{expr}' must still parse; got error: {e}"));
            assert_eq!(
                parsed,
                vec![TypedOverlay::Skill(SkillSpec::Named(good.to_string()))],
                "'{expr}' must parse to the same named skill as before"
            );
        }
    }

    #[test]
    fn skill_and_dir_in_comma_list_produces_both_variants() {
        let result = parse_overlay_list("skill(*),dir(/host:/container:ro)").unwrap();
        assert_eq!(result.len(), 2, "expected 2 overlays; got {result:?}");
        assert!(
            matches!(result[0], TypedOverlay::Skill(SkillSpec::All)),
            "first entry must be Skill(All); got {result:?}"
        );
        assert!(
            matches!(result[1], TypedOverlay::Directory(_)),
            "second entry must be Directory; got {result:?}"
        );
    }

    #[test]
    fn unknown_tag_error_lists_supported_types() {
        let err = parse_overlay_list("foobar(/x:/y)").unwrap_err();
        assert!(
            err.contains("dir"),
            "error must mention 'dir' as a supported type; got: {err}"
        );
        assert!(
            err.contains("skill"),
            "error must mention 'skill' as a supported type; got: {err}"
        );
        assert!(
            err.contains("ssh"),
            "error must mention 'ssh' as a supported type; got: {err}"
        );
        assert!(
            err.contains("env"),
            "error must mention 'env' as a supported type; got: {err}"
        );
    }

    #[test]
    fn ssh_parses_to_directory_overlay() {
        let result = parse_overlay_list("ssh()").unwrap();
        assert_eq!(result.len(), 1);
        match &result[0] {
            TypedOverlay::Directory(spec) => {
                assert!(
                    spec.host.ends_with(".ssh"),
                    "host must end with .ssh; got: {}",
                    spec.host
                );
                assert_eq!(spec.container, "~/.ssh");
            }
            other => panic!("expected Directory, got {other:?}"),
        }
    }

    #[test]
    fn env_parses_to_env_variant() {
        let result = parse_overlay_list("env(MY_VAR)").unwrap();
        assert_eq!(result, vec![TypedOverlay::Env("MY_VAR".to_string())]);
    }

    #[test]
    fn env_empty_returns_error() {
        let err = parse_overlay_list("env()").unwrap_err();
        assert!(
            err.contains("requires an argument"),
            "error must mention 'requires an argument'; got: {err}"
        );
    }

    #[test]
    fn ssh_with_arguments_returns_error() {
        let err = parse_overlay_list("ssh(foo)").unwrap_err();
        assert!(
            err.contains("ssh()") || err.contains("no arguments"),
            "error must explain ssh() takes no arguments; got: {err}"
        );
    }

    #[test]
    fn ssh_overlay_has_read_only_permission() {
        use crate::engine::container::options::OverlayPermission;
        let result = parse_overlay_list("ssh()").unwrap();
        match &result[0] {
            TypedOverlay::Directory(spec) => {
                assert_eq!(
                    spec.permission,
                    OverlayPermission::ReadOnly,
                    "ssh() must produce a ReadOnly mount; got: {:?}",
                    spec.permission
                );
                assert_eq!(spec.container, "~/.ssh", "container path must be ~/.ssh");
            }
            other => panic!("expected Directory, got {other:?}"),
        }
    }

    #[test]
    fn skill_multiple_args_returns_error() {
        let err = parse_overlay_list("skill(foo, bar)").unwrap_err();
        assert!(
            err.contains("separate skill()") || err.contains("one argument"),
            "error must direct user to separate skill() calls; got: {err}"
        );
    }

    #[test]
    fn skills_plural_named_returns_error_with_migration_hint() {
        let err = parse_overlay_list("skills(foo)").unwrap_err();
        assert!(
            err.contains("removed") || err.contains("skill("),
            "error must mention the removed form and replacement; got: {err}"
        );
    }

    #[test]
    fn skills_plural_star_returns_error_with_migration_hint() {
        let err = parse_overlay_list("skills(*)").unwrap_err();
        assert!(
            err.contains("skill(*)") || err.contains("removed"),
            "error must mention skill(*) replacement; got: {err}"
        );
    }

    #[test]
    fn skills_plural_empty_returns_error_with_migration_hint() {
        let err = parse_overlay_list("skills()").unwrap_err();
        assert!(
            err.contains("skill(*)") || err.contains("removed"),
            "error must mention skill(*) or removed form; got: {err}"
        );
    }

    #[test]
    fn env_multiple_args_returns_error() {
        let err = parse_overlay_list("env(A, B)").unwrap_err();
        assert!(
            err.contains("separate env()") || err.contains("one argument"),
            "error must direct user to use separate env() calls; got: {err}"
        );
    }

    #[test]
    fn env_list_produces_two_separate_env_overlays() {
        let result = parse_overlay_list("env(A), env(B)").unwrap();
        assert_eq!(
            result.len(),
            2,
            "two env() expressions must produce two overlays; got {result:?}"
        );
        assert_eq!(result[0], TypedOverlay::Env("A".to_string()));
        assert_eq!(result[1], TypedOverlay::Env("B".to_string()));
    }
}

#[cfg(test)]
mod collect_overlay_specs_tests {
    use super::*;
    use crate::data::config::env::{AWMAN_CONFIG_HOME, AWMAN_OVERLAYS, EnvSnapshot};
    use crate::data::config::global::GlobalConfig;
    use crate::data::config::repo::RepoConfig;
    use crate::data::session::{Session, SessionOpenOptions, StaticGitRootResolver};

    fn open_session(git_root: &std::path::Path, env: EnvSnapshot) -> Session {
        let resolver = StaticGitRootResolver::new(git_root);
        let opts = SessionOpenOptions {
            flags: Default::default(),
            env: Some(env),
            available_agents: None,
        };
        Session::open(git_root.to_path_buf(), &resolver, opts).unwrap()
    }

    #[test]
    fn skills_enabled_when_repo_config_has_skill_star() {
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let repo_config = RepoConfig {
            overlays: Some(vec!["skill(*)".to_string()]),
            ..Default::default()
        };
        repo_config.save(git_tmp.path()).unwrap();
        let env =
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        let session = open_session(git_tmp.path(), env);

        let collected = collect_all_overlay_specs(&session, vec![], None, None).unwrap();
        assert!(
            collected.include_all_skills,
            "skills must be enabled from repo config"
        );
    }

    #[test]
    fn skills_enabled_when_global_config_has_skill_star() {
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let global_config = GlobalConfig {
            overlays: Some(vec!["skill(*)".to_string()]),
            ..Default::default()
        };
        let env =
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        global_config.save_with(&env).unwrap();
        let session = open_session(git_tmp.path(), env);

        let collected = collect_all_overlay_specs(&session, vec![], None, None).unwrap();
        assert!(
            collected.include_all_skills,
            "skills must be enabled from global config"
        );
    }

    #[test]
    fn skills_enabled_when_awman_overlays_env_contains_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([
            (AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap()),
            (AWMAN_OVERLAYS, "skill(*)"),
        ]);
        let session = open_session(tmp.path(), env);

        let collected = collect_all_overlay_specs(&session, vec![], None, None).unwrap();
        assert!(
            collected.include_all_skills,
            "skills must be enabled when AWMAN_OVERLAYS contains skill(*)"
        );
    }

    #[test]
    fn skills_enabled_when_cli_typed_overlays_contains_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let session = open_session(tmp.path(), env);

        let collected = collect_all_overlay_specs(
            &session,
            vec![TypedOverlay::Skill(SkillSpec::All)],
            None,
            None,
        )
        .unwrap();
        assert!(
            collected.include_all_skills,
            "skills must be enabled from CLI TypedOverlay::Skill(All)"
        );
    }

    #[test]
    fn skills_disabled_when_no_source_enables_it() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let session = open_session(tmp.path(), env);

        let collected = collect_all_overlay_specs(&session, vec![], None, None).unwrap();
        assert!(
            !collected.include_all_skills,
            "skills must be disabled when no source sets it"
        );
    }

    #[test]
    fn skills_enabled_is_additive_or_single_source_sufficient() {
        // Only global config has skill(*); repo config and CLI do not.
        // include_all_skills must still be true — OR semantics, not AND.
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let global_config = GlobalConfig {
            overlays: Some(vec!["skill(*)".to_string()]),
            ..Default::default()
        };
        let env =
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        global_config.save_with(&env).unwrap();
        // Repo config has no overlays; no CLI TypedOverlay::Skill.
        let session = open_session(git_tmp.path(), env);

        let collected = collect_all_overlay_specs(&session, vec![], None, None).unwrap();
        assert!(
            collected.include_all_skills,
            "a single source (global config) must be sufficient to enable skills (additive OR)"
        );
    }

    #[test]
    fn env_passthrough_collected_from_overlay_expressions() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([
            (AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap()),
            (AWMAN_OVERLAYS, "env(GH_TOKEN),env(AWS_PROFILE)"),
        ]);
        let session = open_session(tmp.path(), env);

        let collected = collect_all_overlay_specs(&session, vec![], None, None).unwrap();
        assert_eq!(collected.env_passthrough, vec!["GH_TOKEN", "AWS_PROFILE"]);
    }

    // ─── New tests for WI-0082 ────────────────────────────────────────────────

    #[test]
    fn malformed_awman_overlays_env_var_returns_err() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([
            (AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap()),
            (AWMAN_OVERLAYS, "###not-an-overlay###"),
        ]);
        let session = open_session(tmp.path(), env);

        let result = collect_all_overlay_specs(&session, vec![], None, None);
        assert!(result.is_err(), "malformed AWMAN_OVERLAYS must return Err");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("###not-an-overlay###") || msg.contains("invalid overlay"),
            "error must identify the bad spec; got: {msg}"
        );
    }

    #[test]
    fn skill_star_in_flag_and_named_in_step_union_semantics() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let session = open_session(tmp.path(), env);

        let cli_overlays = vec![TypedOverlay::Skill(SkillSpec::All)];
        let step_overlays = vec!["skill(foo)".to_string()];
        let collected =
            collect_all_overlay_specs(&session, cli_overlays, None, Some(&step_overlays)).unwrap();

        assert!(
            collected.include_all_skills,
            "skill(*) in CLI flags must set include_all_skills to true"
        );
        assert!(
            collected.named_skills.contains(&"foo".to_string()),
            "skill(foo) from step must accumulate in named_skills; got {:?}",
            collected.named_skills
        );
    }

    #[test]
    fn skill_named_in_repo_and_step_both_accumulate() {
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let repo_config = RepoConfig {
            overlays: Some(vec!["skill(foo)".to_string()]),
            ..Default::default()
        };
        repo_config.save(git_tmp.path()).unwrap();
        let env =
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        let session = open_session(git_tmp.path(), env);

        let step_overlays = vec!["skill(bar)".to_string()];
        let collected =
            collect_all_overlay_specs(&session, vec![], None, Some(&step_overlays)).unwrap();

        assert!(
            !collected.include_all_skills,
            "no skill(*) source; include_all_skills must be false"
        );
        assert!(
            collected.named_skills.contains(&"foo".to_string()),
            "skill(foo) from repo config must be in named_skills; got {:?}",
            collected.named_skills
        );
        assert!(
            collected.named_skills.contains(&"bar".to_string()),
            "skill(bar) from step must be in named_skills; got {:?}",
            collected.named_skills
        );
    }

    #[test]
    fn skills_plural_in_repo_config_returns_err_with_migration_message() {
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let repo_config = RepoConfig {
            overlays: Some(vec!["skills()".to_string()]),
            ..Default::default()
        };
        repo_config.save(git_tmp.path()).unwrap();
        let env =
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        let session = open_session(git_tmp.path(), env);

        let result = collect_all_overlay_specs(&session, vec![], None, None);
        assert!(result.is_err(), "skills() in repo config must return Err");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("skill(") || msg.contains("removed"),
            "error must contain migration guidance; got: {msg}"
        );
    }

    #[test]
    fn skills_plural_named_in_env_var_returns_err_with_migration_message() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([
            (AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap()),
            (AWMAN_OVERLAYS, "skills(foo)"),
        ]);
        let session = open_session(tmp.path(), env);

        let result = collect_all_overlay_specs(&session, vec![], None, None);
        assert!(
            result.is_err(),
            "skills(foo) in AWMAN_OVERLAYS must return Err"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("skill(") || msg.contains("removed"),
            "error must contain migration guidance; got: {msg}"
        );
    }

    #[test]
    fn ssh_from_flag_and_step_both_resolve_to_same_host_path() {
        // ssh() from any source expands to the same ~/.ssh host path.
        // After passing through OverlayEngine::build_overlays, there must be
        // exactly one mount for that path (insert_or_merge deduplication).
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let session = open_session(tmp.path(), env);

        let ssh_typed = parse_overlay_list("ssh()").unwrap().remove(0);
        let step_overlays = vec!["ssh()".to_string()];
        let collected =
            collect_all_overlay_specs(&session, vec![ssh_typed], None, Some(&step_overlays))
                .unwrap();

        let ssh_entries: Vec<_> = collected
            .directories
            .iter()
            .filter(|d| d.host.ends_with(".ssh"))
            .collect();
        assert!(
            !ssh_entries.is_empty(),
            "at least one ssh() overlay must appear in directories"
        );
        let unique_hosts: std::collections::HashSet<_> =
            ssh_entries.iter().map(|d| d.host.as_str()).collect();
        assert_eq!(
            unique_hosts.len(),
            1,
            "all ssh() expansions from any source must resolve to the same host path; \
             got unique hosts: {unique_hosts:?}"
        );
    }

    #[test]
    fn env_from_two_sources_deduplicates_to_one_entry() {
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let repo_config = RepoConfig {
            overlays: Some(vec!["env(MY_TOKEN)".to_string()]),
            ..Default::default()
        };
        repo_config.save(git_tmp.path()).unwrap();
        let env =
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        let session = open_session(git_tmp.path(), env);

        let step_overlays = vec!["env(MY_TOKEN)".to_string()];
        let collected =
            collect_all_overlay_specs(&session, vec![], None, Some(&step_overlays)).unwrap();

        let count = collected
            .env_passthrough
            .iter()
            .filter(|v| *v == "MY_TOKEN")
            .count();
        assert_eq!(
            count, 1,
            "MY_TOKEN from two sources must appear exactly once in env_passthrough; \
             got {:?}",
            collected.env_passthrough
        );
    }

    #[test]
    fn env_from_repo_config_and_step_both_present_in_passthrough() {
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let repo_config = RepoConfig {
            overlays: Some(vec!["env(REPO_VAR)".to_string()]),
            ..Default::default()
        };
        repo_config.save(git_tmp.path()).unwrap();
        let env =
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        let session = open_session(git_tmp.path(), env);

        let step_overlays = vec!["env(STEP_VAR)".to_string()];
        let collected =
            collect_all_overlay_specs(&session, vec![], None, Some(&step_overlays)).unwrap();

        assert!(
            collected.env_passthrough.contains(&"REPO_VAR".to_string()),
            "env(REPO_VAR) from repo config must be in env_passthrough; got {:?}",
            collected.env_passthrough
        );
        assert!(
            collected.env_passthrough.contains(&"STEP_VAR".to_string()),
            "env(STEP_VAR) from step overlays must be in env_passthrough; got {:?}",
            collected.env_passthrough
        );
    }
}

#[cfg(test)]
mod warn_legacy_config_tests {
    use super::*;
    use crate::data::config::env::{AWMAN_CONFIG_HOME, EnvSnapshot};
    use crate::data::message::{MessageLevel, RecordingMessageSink};
    use crate::data::session::{Session, SessionOpenOptions, StaticGitRootResolver};

    fn open_session(git_root: &std::path::Path, env: EnvSnapshot) -> Session {
        let resolver = StaticGitRootResolver::new(git_root);
        let opts = SessionOpenOptions {
            flags: Default::default(),
            env: Some(env),
            available_agents: None,
        };
        Session::open(git_root.to_path_buf(), &resolver, opts).unwrap()
    }

    #[test]
    fn repo_legacy_env_passthrough_triggers_warning_mentioning_config_path() {
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();

        // Write a repo config with the legacy envPassthrough field.
        let awman_dir = git_tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join("config.json"),
            r#"{"envPassthrough": ["MY_VAR"]}"#,
        )
        .unwrap();

        let env =
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        let session = open_session(git_tmp.path(), env);

        let mut sink = RecordingMessageSink::new();
        warn_legacy_config(&session, &mut sink);

        let warnings: Vec<_> = sink
            .queued()
            .iter()
            .filter(|m| m.level == MessageLevel::Warning)
            .collect();
        assert!(
            !warnings.is_empty(),
            "must emit at least one warning for legacy envPassthrough in repo config"
        );
        let text = &warnings[0].text;
        assert!(
            text.contains(".awman/config.json"),
            "warning must mention .awman/config.json to identify the source file; got: {text}"
        );
    }

    #[test]
    fn global_legacy_env_passthrough_triggers_warning_mentioning_global_config_path() {
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();

        // Write a global config with the legacy envPassthrough field.
        std::fs::write(
            cfg_tmp.path().join("config.json"),
            r#"{"envPassthrough": ["GLOBAL_VAR"]}"#,
        )
        .unwrap();

        let env =
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        let session = open_session(git_tmp.path(), env);

        let mut sink = RecordingMessageSink::new();
        warn_legacy_config(&session, &mut sink);

        let warnings: Vec<_> = sink
            .queued()
            .iter()
            .filter(|m| m.level == MessageLevel::Warning)
            .collect();
        assert!(
            !warnings.is_empty(),
            "must emit at least one warning for legacy envPassthrough in global config"
        );
        let text = &warnings[0].text;
        assert!(
            text.contains("~/.awman/config.json"),
            "warning must mention ~/.awman/config.json to identify the source file; got: {text}"
        );
    }

    #[test]
    fn no_legacy_fields_produces_no_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let session = open_session(tmp.path(), env);

        let mut sink = RecordingMessageSink::new();
        warn_legacy_config(&session, &mut sink);

        assert!(
            sink.queued().is_empty(),
            "no warning must be emitted when no legacy fields are present; got {:?}",
            sink.queued()
        );
    }
}

#[cfg(test)]
mod overlay_spec_tests {
    use super::*;
    use crate::engine::container::options::OverlayPermission;

    #[test]
    fn parse_overlay_spec_host_container_default_rw() {
        let spec = parse_overlay_spec("/host/path:/container/path").unwrap();
        assert_eq!(spec.host, "/host/path");
        assert_eq!(spec.container, "/container/path");
        assert_eq!(spec.permission, OverlayPermission::ReadWrite);
    }

    #[test]
    fn parse_overlay_spec_with_ro_permission() {
        let spec = parse_overlay_spec("/host/path:/container/path:ro").unwrap();
        assert_eq!(spec.permission, OverlayPermission::ReadOnly);
    }

    #[test]
    fn parse_overlay_spec_with_rw_permission() {
        let spec = parse_overlay_spec("/host/path:/container/path:rw").unwrap();
        assert_eq!(spec.permission, OverlayPermission::ReadWrite);
    }

    #[test]
    fn parse_overlay_spec_missing_container_returns_error() {
        let result = parse_overlay_spec("/host/only");
        assert!(result.is_err(), "must error when container path is missing");
    }

    #[test]
    fn parse_overlay_spec_relative_container_path_returns_error() {
        let result = parse_overlay_spec("/host/path:relative/path");
        assert!(result.is_err(), "must error for relative container path");
    }

    #[test]
    fn parse_overlay_spec_unknown_permission_returns_error() {
        let result = parse_overlay_spec("/host:/container:rx");
        assert!(result.is_err(), "must error for unknown permission 'rx'");
    }

    #[test]
    fn parse_overlay_spec_empty_host_returns_error() {
        let result = parse_overlay_spec(":/container/path");
        assert!(result.is_err(), "must error for empty host path");
    }
}

// ─── WI-0087: context() overlay parsing ──────────────────────────────────────

#[cfg(test)]
mod context_parser_tests {
    use super::*;
    use crate::engine::container::options::OverlayPermission;
    use crate::engine::overlay::ContextScope;

    fn parse_context(input: &str) -> Result<(ContextScope, OverlayPermission), String> {
        let result = parse_overlay_list(input)?;
        assert_eq!(
            result.len(),
            1,
            "expected exactly 1 overlay; got {result:?}"
        );
        match &result[0] {
            TypedOverlay::Context(spec) => Ok((spec.scope, spec.permission)),
            other => Err(format!("expected TypedOverlay::Context, got {other:?}")),
        }
    }

    #[test]
    fn context_global_default_rw() {
        let (scope, perm) = parse_context("context(global)").unwrap();
        assert_eq!(scope, ContextScope::Global);
        assert_eq!(perm, OverlayPermission::ReadWrite);
    }

    #[test]
    fn context_repo_default_rw() {
        let (scope, perm) = parse_context("context(repo)").unwrap();
        assert_eq!(scope, ContextScope::Repo);
        assert_eq!(perm, OverlayPermission::ReadWrite);
    }

    #[test]
    fn context_workflow_default_rw() {
        let (scope, perm) = parse_context("context(workflow)").unwrap();
        assert_eq!(scope, ContextScope::Workflow);
        assert_eq!(perm, OverlayPermission::ReadWrite);
    }

    #[test]
    fn context_global_ro_explicit() {
        let (scope, perm) = parse_context("context(global:ro)").unwrap();
        assert_eq!(scope, ContextScope::Global);
        assert_eq!(perm, OverlayPermission::ReadOnly);
    }

    #[test]
    fn context_workflow_rw_explicit() {
        let (scope, perm) = parse_context("context(workflow:rw)").unwrap();
        assert_eq!(scope, ContextScope::Workflow);
        assert_eq!(perm, OverlayPermission::ReadWrite);
    }

    #[test]
    fn context_repo_ro() {
        let (scope, perm) = parse_context("context(repo:ro)").unwrap();
        assert_eq!(scope, ContextScope::Repo);
        assert_eq!(perm, OverlayPermission::ReadOnly);
    }

    // ─── Error cases ──────────────────────────────────────────────────────────

    #[test]
    fn context_missing_scope_returns_error() {
        let err = parse_overlay_list("context()").unwrap_err();
        assert!(
            err.contains("scope") || err.contains("requires"),
            "error must explain context() requires a scope; got: {err}"
        );
    }

    #[test]
    fn context_unknown_scope_returns_error() {
        let err = parse_overlay_list("context(unknown)").unwrap_err();
        assert!(
            err.contains("unknown"),
            "error must identify the unknown scope value; got: {err}"
        );
        // Must mention the supported scopes.
        assert!(
            err.contains("global") || err.contains("scope"),
            "error must name supported scopes; got: {err}"
        );
    }

    #[test]
    fn context_bad_permission_returns_error() {
        // context(global:rx) — 'rx' is not a valid permission
        let err = parse_overlay_list("context(global:rx)").unwrap_err();
        assert!(
            err.contains("rx") || err.contains("permission"),
            "error must mention the bad permission 'rx'; got: {err}"
        );
    }

    #[test]
    fn context_too_many_parts_returns_error() {
        // context(global:ro:extra) — the ':'-split yields "ro:extra" as perm
        let err = parse_overlay_list("context(global:ro:extra)").unwrap_err();
        assert!(
            err.contains("ro:extra") || err.contains("permission"),
            "error must identify the malformed permission segment; got: {err}"
        );
    }
}

// ─── WI-0087: collect_all_overlay_specs with context overlays ─────────────────

#[cfg(test)]
mod context_collect_0087_tests {
    use super::*;
    use crate::data::config::env::{AWMAN_CONFIG_HOME, EnvSnapshot};
    use crate::data::config::global::GlobalConfig;
    use crate::data::session::{Session, SessionOpenOptions, StaticGitRootResolver};
    use crate::engine::overlay::ContextScope;

    fn open_session(git_root: &std::path::Path, env: EnvSnapshot) -> Session {
        let resolver = StaticGitRootResolver::new(git_root);
        let opts = SessionOpenOptions {
            flags: Default::default(),
            env: Some(env),
            available_agents: None,
        };
        Session::open(git_root.to_path_buf(), &resolver, opts).unwrap()
    }

    #[test]
    fn context_global_from_global_config_appears_in_context_overlays() {
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let global_config = GlobalConfig {
            overlays: Some(vec!["context(global)".to_string()]),
            ..Default::default()
        };
        let env =
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        global_config.save_with(&env).unwrap();
        let session = open_session(git_tmp.path(), env);

        let collected = collect_all_overlay_specs(&session, vec![], None, None).unwrap();
        assert_eq!(
            collected.context_overlays.len(),
            1,
            "one context overlay expected; got {:?}",
            collected.context_overlays
        );
        assert_eq!(collected.context_overlays[0].scope, ContextScope::Global);
    }

    #[test]
    fn context_global_deduplicates_when_in_global_config_and_step() {
        // context(global) in both global config and step overlays → exactly one entry.
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let global_config = GlobalConfig {
            overlays: Some(vec!["context(global)".to_string()]),
            ..Default::default()
        };
        let env =
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        global_config.save_with(&env).unwrap();
        let session = open_session(git_tmp.path(), env);

        let step_overlays = vec!["context(global)".to_string()];
        let collected =
            collect_all_overlay_specs(&session, vec![], None, Some(&step_overlays)).unwrap();

        let global_count = collected
            .context_overlays
            .iter()
            .filter(|c| c.scope == ContextScope::Global)
            .count();
        assert_eq!(
            global_count, 1,
            "context(global) from two sources must deduplicate to one entry; \
             got {:?}",
            collected.context_overlays
        );
    }

    #[test]
    fn workflow_level_context_global_applies_to_step() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let session = open_session(tmp.path(), env);

        let workflow_overlays = vec!["context(global)".to_string()];
        let collected =
            collect_all_overlay_specs(&session, vec![], Some(&workflow_overlays), None).unwrap();

        assert_eq!(
            collected.context_overlays.len(),
            1,
            "workflow-level context(global) must appear in context_overlays; \
             got {:?}",
            collected.context_overlays
        );
        assert_eq!(collected.context_overlays[0].scope, ContextScope::Global);
    }

    #[test]
    fn workflow_and_step_context_union_produces_two_entries() {
        // Workflow-level context(repo) + step-level context(global) → two entries.
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let session = open_session(tmp.path(), env);

        let workflow_overlays = vec!["context(repo)".to_string()];
        let step_overlays = vec!["context(global)".to_string()];
        let collected = collect_all_overlay_specs(
            &session,
            vec![],
            Some(&workflow_overlays),
            Some(&step_overlays),
        )
        .unwrap();

        assert_eq!(
            collected.context_overlays.len(),
            2,
            "workflow-level context(repo) + step-level context(global) must \
             produce two distinct context overlay specs; got {:?}",
            collected.context_overlays
        );
        let has_repo = collected
            .context_overlays
            .iter()
            .any(|c| c.scope == ContextScope::Repo);
        let has_global = collected
            .context_overlays
            .iter()
            .any(|c| c.scope == ContextScope::Global);
        assert!(has_repo, "Repo scope must be present");
        assert!(has_global, "Global scope must be present");
    }

    #[test]
    fn step_context_workflow_does_not_override_workflow_level() {
        // Step-level context(workflow) + workflow-level context(global) → two entries (union).
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let session = open_session(tmp.path(), env);

        let workflow_overlays = vec!["context(global)".to_string()];
        let step_overlays = vec!["context(workflow)".to_string()];
        let collected = collect_all_overlay_specs(
            &session,
            vec![],
            Some(&workflow_overlays),
            Some(&step_overlays),
        )
        .unwrap();

        assert_eq!(
            collected.context_overlays.len(),
            2,
            "union semantics: both workflow-level and step-level context scopes \
             must be present; got {:?}",
            collected.context_overlays
        );
        assert!(
            collected
                .context_overlays
                .iter()
                .any(|c| c.scope == ContextScope::Global),
            "Global scope from workflow level must be present"
        );
        assert!(
            collected
                .context_overlays
                .iter()
                .any(|c| c.scope == ContextScope::Workflow),
            "Workflow scope from step level must be present"
        );
    }
}
