//! Typed `ContainerOption` enum and surrounding option types.
//!
//! Every flag the legacy `oldsrc/runtime/{docker,apple,mod}.rs` exposes
//! becomes one variant here. Adding a new option is one variant + one branch
//! in `ResolvedContainerOptions::ingest`.

use std::path::PathBuf;

/// A reference to a container image (e.g. `awman-myproj-claude:latest`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef(pub String);

impl ImageRef {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Container entrypoint command + args (e.g. `["claude", "--print"]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entrypoint(pub Vec<String>);

impl Entrypoint {
    pub fn new(parts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self(parts.into_iter().map(Into::into).collect())
    }
}

/// Stable name for a container (e.g. `awman-abc123`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerName(pub String);

impl ContainerName {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A directory or file overlay to mount into the container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlaySpec {
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    pub permission: OverlayPermission,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayPermission {
    ReadOnly,
    ReadWrite,
}

impl OverlayPermission {
    pub fn as_str(&self) -> &'static str {
        match self {
            OverlayPermission::ReadOnly => "ro",
            OverlayPermission::ReadWrite => "rw",
        }
    }
}

/// A passthrough environment variable (read from host at launch time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvVar(pub String);

/// A literal env-var key/value pair injected into the container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvLiteral {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum YoloMode {
    #[default]
    Disabled,
    Enabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutoMode {
    #[default]
    Disabled,
    Enabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlanMode {
    #[default]
    Disabled,
    Enabled,
}

/// CPU limit in fractional cores (e.g. `2.0` for two cores).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuLimit(pub f64);

/// Memory limit in megabytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryLimit(pub u64);

/// How a model flag is delivered to the agent (e.g. `--model NAME` vs
/// `--model-claude-opus-4-6`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelFlagForm {
    /// `--model NAME`
    Argument(String),
    /// A standalone shorthand like `--model-claude-opus-4-6`.
    Shorthand(String),
}

/// A bundle of host-side agent settings prepared by `OverlayEngine`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSettings {
    /// Container `$HOME` (typically `/root` or `/home/<user>`).
    pub container_home: String,
    /// Pre-built overlay specs derived from the host's agent config files.
    pub overlays: Vec<OverlaySpec>,
}

/// Every knob a `AgentInstance` accepts. Adding a new option is a single
/// variant and a single branch in `ResolvedContainerOptions::ingest`.
#[derive(Debug, Clone, PartialEq)]
pub enum ContainerOption {
    Image(ImageRef),
    Entrypoint(Entrypoint),
    Overlay(OverlaySpec),
    EnvPassthrough(EnvVar),
    EnvLiteral(EnvLiteral),
    SeededPrompt(String),
    /// Flag used to deliver the seeded prompt in *interactive* mode (e.g.
    /// opencode `--prompt <text>`). Absent means the prompt is appended as a
    /// trailing positional argv arg, which is what most agents expect. Emitted
    /// alongside `SeededPrompt` by `build_options` when the agent matrix's
    /// `interactive_seed_delivery` is `Flag`.
    InteractiveSeedFlag(String),
    Interactive(bool),
    /// Launch the agent over ACP (Agent Client Protocol) instead of raw
    /// container stdio. Selects the persistent-piped spawn path (`-i`, no PTY)
    /// so a newline-delimited JSON-RPC 2.0 channel can ride the container's
    /// stdio pipes for the whole session. Introduces NO new host exposure —
    /// no ports, no `--network`, no new mounts (see
    /// `aspec/architecture/security.md`); the bytes flow over the exact stdio
    /// pipes `-i` already wires up.
    Acp(bool),
    AllowDocker(bool),
    Yolo(YoloMode),
    Auto(AutoMode),
    Plan(PlanMode),
    WorkingDir(PathBuf),
    Name(ContainerName),
    Cpu(CpuLimit),
    Memory(MemoryLimit),
    AgentSettingsPassthrough(AgentSettings),
    AgentCredentials {
        env_vars: Vec<(String, String)>,
    },
    DisallowedTools(Vec<String>),
    AllowedTools(Vec<String>),
    Model {
        flag: ModelFlagForm,
    },
    NonInteractivePrintFlag(String),
    /// Container-side `$HOME` remapped from `/root` when a non-root `USER`
    /// directive is detected in the agent's Dockerfile.
    DockerfileUser(String),
    /// A container label — emitted as `--label <key>=<value>`. Used to
    /// attribute containers (e.g. `awman.session=<id>` so `list_running` can
    /// map a container to an awman session, or `awman.amie.condition=<name>`
    /// for amie's background agents). Multiple labels accumulate.
    Label {
        key: String,
        value: String,
    },
    /// Per-agent mode flags (yolo, auto, plan) — emitted as literal argv
    /// strings after the entrypoint in `build_run_argv`.
    AgentModeFlags(Vec<String>),
    /// The flag name to use when emitting disallowed tools (e.g. `--disallowedTools`).
    DisallowedToolsFlag(String),
    /// The flag name to use when emitting allowed tools (e.g. `--allowedTools`).
    AllowedToolsFlag(String),
    /// Keep the container after exit (do not pass `--rm`).
    KeepContainer,
    /// System prompt delivered via a file mount + CLI flag (e.g.
    /// `--append-system-prompt-file <container_path>`).
    SystemPromptFile {
        host_path: PathBuf,
        container_path: PathBuf,
        flag: String,
    },
    /// System prompt delivered via an env var pointing to a mounted file
    /// (e.g. `GEMINI_SYSTEM_MD=<container_path>`).
    SystemPromptEnvFile {
        env_var: String,
        host_path: PathBuf,
        container_path: PathBuf,
    },
    /// System prompt delivered as inline text via a CLI flag (e.g.
    /// `--system <text>` for cline).
    SystemPromptInline {
        flag: String,
        text: String,
    },
    /// Extra workspace dir for the agent (e.g. `--add-dir <container_path>`).
    AgentAddDir {
        flag: String,
        container_path: PathBuf,
    },
}

/// Injection-time dedup: drop any entry from `agent_credentials` whose
/// credential key maps to the same provider service as a key already declared
/// in `env_passthrough` or `env_literal`, **and whose host value is actually
/// resolvable**.
///
/// Mirrors the rationale of the sbx path's `CLAUDE_CODE_OAUTH_TOKEN` silent
/// skip ([`crate::engine::sandbox::dsbx::auth::inject_credentials`]):
/// when the harness has **declared** an env var (via `env(VAR)`) that already
/// authenticates the same provider, the keychain OAuth token is redundant and
/// its presence causes the container to receive two conflicting credentials for
/// the same service.
///
/// An `env_passthrough` entry only counts as "covering" a service when its host
/// value is actually set — mirroring `build_run_argv`'s own emission gate
/// (`if let Ok(value) = std::env::var(name)`).  A declared but unset passthrough
/// var does NOT suppress a keychain credential, because the passthrough will emit
/// nothing and the container would otherwise receive zero credentials for that
/// service.  `env_literal` entries always carry a value and always count.
///
/// `lookup_env` abstracts the host env lookup so that callers in tests can
/// supply a hermetic closure instead of reading `std::env::var` directly.
///
/// If dedup would drop the **last** remaining credential for a service, a
/// `log::warn!` is emitted so the situation is never silent.
///
/// Example: harness declares `env(ANTHROPIC_API_KEY)` → service "anthropic";
/// keychain resolves `CLAUDE_CODE_OAUTH_TOKEN` → also service "anthropic";
/// host has `ANTHROPIC_API_KEY` set → the passthrough will be emitted →
/// `CLAUDE_CODE_OAUTH_TOKEN` is dropped from `agent_credentials`.
///
/// Counter-example: same declaration but `ANTHROPIC_API_KEY` is NOT set on the
/// host → the passthrough emits nothing → `CLAUDE_CODE_OAUTH_TOKEN` is retained.
pub(crate) fn dedup_credentials_by_declared_env(
    agent_credentials: &mut Vec<(String, String)>,
    env_passthrough: &[EnvVar],
    env_literal: &[EnvLiteral],
    lookup_env: &dyn Fn(&str) -> Option<String>,
) {
    // Collect the set of provider services already covered by declared env vars
    // that will actually be emitted to the container:
    //   - env_passthrough: only when the host value is set (and non-empty).
    //   - env_literal: always (they carry an explicit value).
    let covered_services: Vec<&'static str> = env_passthrough
        .iter()
        .filter(|v| lookup_env(v.0.as_str()).is_some_and(|val| !val.is_empty()))
        .map(|v| v.0.as_str())
        .chain(env_literal.iter().map(|l| l.key.as_str()))
        .filter_map(crate::engine::auth::service_for_credential)
        .collect();

    if covered_services.is_empty() {
        return;
    }

    // Before dropping, check whether this dedup would leave any service with
    // zero credentials.  If so, warn — the outcome is intentional (the literal
    // or resolvable passthrough will cover it), but it should never be silent.
    for service in &covered_services {
        let service_creds_before: Vec<_> = agent_credentials
            .iter()
            .filter(|(k, _)| crate::engine::auth::service_for_credential(k) == Some(service))
            .collect();
        if !service_creds_before.is_empty() {
            tracing::warn!(
                dropped_keys = ?service_creds_before
                    .iter()
                    .map(|(k, _)| k.as_str())
                    .collect::<Vec<_>>(),
                service = service,
                "awman: dropping keychain credential(s) for service because the repo \
                 declared an env var that covers the same provider; the container will \
                 receive credentials via the declared env overlay.",
            );
        }
    }

    // Retain only credentials whose service is NOT already covered by a
    // harness-declared env var that will be emitted to the container.
    agent_credentials.retain(|(key, _)| {
        match crate::engine::auth::service_for_credential(key) {
            Some(service) => !covered_services.contains(&service),
            // Credential with no known service mapping — retain unconditionally.
            None => true,
        }
    });
}

/// Resolved option bag — all options merged into a single struct that the
/// backend consumes. Conflicting options are detected here.
#[derive(Debug, Clone, Default)]
pub struct ResolvedContainerOptions {
    pub image: Option<ImageRef>,
    pub entrypoint: Option<Entrypoint>,
    pub overlays: Vec<OverlaySpec>,
    pub env_passthrough: Vec<EnvVar>,
    pub env_literal: Vec<EnvLiteral>,
    pub seeded_prompt: Option<String>,
    /// Flag used to deliver `seeded_prompt` in interactive mode. When `None`,
    /// the prompt is appended as a trailing positional argv arg. When `Some`,
    /// it is delivered as `<flag> <text>` (e.g. opencode `--prompt <text>`,
    /// since opencode treats a bare positional as a project directory).
    pub interactive_seed_flag: Option<String>,
    pub interactive: bool,
    /// When `true`, the runtime uses the persistent-piped spawn path (`-i`,
    /// never a PTY) so an ACP JSON-RPC channel can span the whole session.
    /// Default `false` keeps today's PTY/one-shot-piped behaviour unchanged.
    pub acp: bool,
    pub allow_docker: bool,
    pub yolo: YoloMode,
    pub auto: AutoMode,
    pub plan: PlanMode,
    pub working_dir: Option<PathBuf>,
    pub name: Option<ContainerName>,
    pub cpu: Option<CpuLimit>,
    pub memory: Option<MemoryLimit>,
    pub agent_settings: Option<AgentSettings>,
    pub agent_credentials: Vec<(String, String)>,
    pub disallowed_tools: Vec<String>,
    pub allowed_tools: Vec<String>,
    pub model: Option<ModelFlagForm>,
    pub non_interactive_flag: Option<String>,
    pub dockerfile_user: Option<String>,
    /// Container labels accumulated from `ContainerOption::Label`, emitted as
    /// one `--label key=value` each (after the hardcoded `awman=true`).
    pub labels: Vec<(String, String)>,
    pub agent_mode_flags: Vec<String>,
    pub disallowed_tools_flag: Option<String>,
    pub allowed_tools_flag: Option<String>,
    pub remove_on_exit: bool,
    pub system_prompt_file: Option<(PathBuf, PathBuf, String)>,
    pub system_prompt_env_file: Option<(String, PathBuf, PathBuf)>,
    pub system_prompt_inline: Option<(String, String)>,
    pub agent_add_dirs: Vec<(String, PathBuf)>,
}

impl ResolvedContainerOptions {
    pub fn resolve(
        options: impl IntoIterator<Item = ContainerOption>,
    ) -> Result<Self, ResolveError> {
        let mut r = Self {
            yolo: YoloMode::Disabled,
            auto: AutoMode::Disabled,
            plan: PlanMode::Disabled,
            remove_on_exit: true,
            ..Self::default()
        };
        for opt in options {
            r.ingest(opt)?;
        }
        // Part A: drop agent_credentials that duplicate a service already covered
        // by a harness-declared env var.  Applies to ALL container runtimes.
        // Production callers pass the real host-env lookup; tests inject a
        // hermetic closure to avoid mutating process-global state.
        dedup_credentials_by_declared_env(
            &mut r.agent_credentials,
            &r.env_passthrough,
            &r.env_literal,
            &|name| std::env::var(name).ok(),
        );
        r.validate()?;
        Ok(r)
    }

    fn ingest(&mut self, opt: ContainerOption) -> Result<(), ResolveError> {
        match opt {
            ContainerOption::Image(v) => self.image = Some(v),
            ContainerOption::Entrypoint(v) => self.entrypoint = Some(v),
            ContainerOption::Overlay(v) => self.overlays.push(v),
            ContainerOption::EnvPassthrough(v) => self.env_passthrough.push(v),
            ContainerOption::EnvLiteral(v) => self.env_literal.push(v),
            ContainerOption::SeededPrompt(v) => self.seeded_prompt = Some(v),
            ContainerOption::InteractiveSeedFlag(v) => self.interactive_seed_flag = Some(v),
            ContainerOption::Interactive(v) => self.interactive = v,
            ContainerOption::Acp(v) => self.acp = v,
            ContainerOption::AllowDocker(v) => self.allow_docker = v,
            ContainerOption::Yolo(v) => self.yolo = v,
            ContainerOption::Auto(v) => self.auto = v,
            ContainerOption::Plan(v) => self.plan = v,
            ContainerOption::WorkingDir(v) => self.working_dir = Some(v),
            ContainerOption::Name(v) => self.name = Some(v),
            ContainerOption::Cpu(v) => self.cpu = Some(v),
            ContainerOption::Memory(v) => self.memory = Some(v),
            ContainerOption::AgentSettingsPassthrough(v) => self.agent_settings = Some(v),
            ContainerOption::AgentCredentials { env_vars } => {
                self.agent_credentials.extend(env_vars);
            }
            ContainerOption::DisallowedTools(v) => self.disallowed_tools.extend(v),
            ContainerOption::AllowedTools(v) => self.allowed_tools.extend(v),
            ContainerOption::Model { flag } => self.model = Some(flag),
            ContainerOption::NonInteractivePrintFlag(v) => self.non_interactive_flag = Some(v),
            ContainerOption::DockerfileUser(v) => self.dockerfile_user = Some(v),
            ContainerOption::Label { key, value } => self.labels.push((key, value)),
            ContainerOption::AgentModeFlags(v) => self.agent_mode_flags.extend(v),
            ContainerOption::DisallowedToolsFlag(v) => self.disallowed_tools_flag = Some(v),
            ContainerOption::AllowedToolsFlag(v) => self.allowed_tools_flag = Some(v),
            ContainerOption::KeepContainer => self.remove_on_exit = false,
            ContainerOption::SystemPromptFile {
                host_path,
                container_path,
                flag,
            } => {
                self.system_prompt_file = Some((host_path, container_path, flag));
            }
            ContainerOption::SystemPromptEnvFile {
                env_var,
                host_path,
                container_path,
            } => {
                self.system_prompt_env_file = Some((env_var, host_path, container_path));
            }
            ContainerOption::SystemPromptInline { flag, text } => {
                self.system_prompt_inline = Some((flag, text));
            }
            ContainerOption::AgentAddDir {
                flag,
                container_path,
            } => {
                self.agent_add_dirs.push((flag, container_path));
            }
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), ResolveError> {
        // Yolo + Plan are mutually exclusive — yolo grants permissions, plan
        // forbids them.
        if matches!(self.yolo, YoloMode::Enabled) && matches!(self.plan, PlanMode::Enabled) {
            return Err(ResolveError::Conflict(
                "yolo and plan modes are mutually exclusive".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("conflicting container options: {0}")]
    Conflict(String),
}

impl From<ResolveError> for crate::engine::error::EngineError {
    fn from(e: ResolveError) -> Self {
        match e {
            ResolveError::Conflict(msg) => {
                crate::engine::error::EngineError::ConflictingOptions(msg)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn yolo_and_plan_conflict_returns_error() {
        let result = ResolvedContainerOptions::resolve([
            ContainerOption::Yolo(YoloMode::Enabled),
            ContainerOption::Plan(PlanMode::Enabled),
        ]);
        assert!(
            matches!(result, Err(ResolveError::Conflict(_))),
            "expected Conflict, got {result:?}"
        );
    }

    #[test]
    fn all_options_round_trip_to_resolved() {
        let image = ImageRef::new("my-image:latest");
        let entrypoint = Entrypoint::new(["claude", "--print"]);
        let result = ResolvedContainerOptions::resolve([
            ContainerOption::Image(image.clone()),
            ContainerOption::Entrypoint(entrypoint.clone()),
            ContainerOption::Interactive(true),
            ContainerOption::AllowedTools(vec!["Bash".to_string()]),
            ContainerOption::Yolo(YoloMode::Disabled),
        ]);
        let resolved = result.expect("from_iter should succeed");
        assert_eq!(
            resolved.image.as_ref().map(|i| i.as_str()),
            Some("my-image:latest")
        );
        assert_eq!(
            resolved.entrypoint.as_ref().map(|e| &e.0),
            Some(&vec!["claude".to_string(), "--print".to_string()])
        );
        assert!(resolved.interactive);
        assert_eq!(resolved.allowed_tools, vec!["Bash".to_string()]);
        assert!(matches!(resolved.yolo, YoloMode::Disabled));
    }

    #[test]
    fn acp_defaults_to_false_and_round_trips_when_set() {
        // Default: no ACP option → `acp` stays false (today's behaviour).
        let default = ResolvedContainerOptions::resolve([ContainerOption::Image(ImageRef::new(
            "img:latest",
        ))])
        .expect("resolve should succeed");
        assert!(!default.acp, "acp must default to false");

        // Explicitly requested → the resolved bag carries it through.
        let enabled = ResolvedContainerOptions::resolve([
            ContainerOption::Image(ImageRef::new("img:latest")),
            ContainerOption::Acp(true),
        ])
        .expect("resolve should succeed");
        assert!(enabled.acp, "ContainerOption::Acp(true) must set acp");
    }

    #[test]
    fn dedup_is_not_required_at_resolve_level() {
        let host = PathBuf::from("/host/overlay");
        let container = PathBuf::from("/container/overlay");
        let spec = OverlaySpec {
            host_path: host.clone(),
            container_path: container.clone(),
            permission: OverlayPermission::ReadOnly,
        };
        let result = ResolvedContainerOptions::resolve([
            ContainerOption::Overlay(spec.clone()),
            ContainerOption::Overlay(spec.clone()),
            ContainerOption::Overlay(spec.clone()),
        ]);
        let resolved = result.expect("from_iter should succeed");
        // Multiple overlay entries accumulate — dedup is caller's responsibility.
        assert_eq!(resolved.overlays.len(), 3);
    }

    // ── Part A: injection-time credential dedup (resolve-level, env_literal path) ──

    /// `env_literal` (always-valued) coverage → same-service keychain credential
    /// is dropped.  This goes through `resolve()` because env_literal entries
    /// always have a value — no host lookup needed.
    #[test]
    fn declared_anthropic_env_literal_drops_oauth_token_from_agent_credentials() {
        let resolved = ResolvedContainerOptions::resolve([
            ContainerOption::EnvLiteral(EnvLiteral {
                key: "ANTHROPIC_API_KEY".into(),
                value: "sk-ant-key-literal".into(),
            }),
            ContainerOption::AgentCredentials {
                env_vars: vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "sk-ant-oat-secret".into())],
            },
        ])
        .expect("resolve must succeed");

        assert!(
            resolved.agent_credentials.is_empty(),
            "ANTHROPIC_API_KEY via env_literal must still trigger dedup; \
             got: {:?}",
            resolved.agent_credentials
        );
    }

    /// When agent_credentials is empty the dedup is a no-op (no panic, no error).
    #[test]
    fn dedup_with_empty_agent_credentials_is_noop() {
        // Use env_literal so the test is hermetic (no host-env lookup).
        let resolved =
            ResolvedContainerOptions::resolve([ContainerOption::EnvLiteral(EnvLiteral {
                key: "ANTHROPIC_API_KEY".into(),
                value: "sk-key".into(),
            })])
            .expect("resolve must succeed");
        assert!(resolved.agent_credentials.is_empty());
    }

    // ── Part A: dedup_credentials_by_declared_env unit tests (injectable lookup) ──
    //
    // All of these pass a hermetic closure as `lookup_env` so no process-global
    // std::env mutation is needed.  The closure mimics the subset of env vars
    // that would be set on the host in each scenario.

    /// declared env(ANTHROPIC_API_KEY) that IS set on host + keychain OAuth →
    /// OAuth dropped (the passthrough will be emitted; no need for two creds).
    #[test]
    fn dedup_fn_drops_oauth_when_anthropic_passthrough_set_on_host() {
        let mut creds = vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "tok".into())];
        let pt = vec![EnvVar("ANTHROPIC_API_KEY".into())];
        // Simulate: ANTHROPIC_API_KEY is set on the host.
        let lookup = |name: &str| -> Option<String> {
            if name == "ANTHROPIC_API_KEY" {
                Some("sk-ant-api-key".into())
            } else {
                None
            }
        };
        dedup_credentials_by_declared_env(&mut creds, &pt, &[], &lookup);
        assert!(
            creds.is_empty(),
            "OAuth must be dropped when ANTHROPIC_API_KEY is set on host; got: {creds:?}"
        );
    }

    /// declared env(ANTHROPIC_API_KEY) that is NOT set on host + keychain OAuth →
    /// OAuth retained (passthrough emits nothing; dropping it would leave zero
    /// credentials for the 'anthropic' service).
    #[test]
    fn dedup_fn_retains_oauth_when_anthropic_passthrough_declared_but_unset() {
        let mut creds = vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "tok".into())];
        let pt = vec![EnvVar("ANTHROPIC_API_KEY".into())];
        // Simulate: ANTHROPIC_API_KEY is NOT set on the host.
        let lookup = |_name: &str| -> Option<String> { None };
        dedup_credentials_by_declared_env(&mut creds, &pt, &[], &lookup);
        assert_eq!(
            creds.len(),
            1,
            "OAuth must be retained when the declared passthrough var is unset on host; \
             got: {creds:?}"
        );
    }

    /// No declared anthropic var → cloud harness path — keychain OAuth is
    /// retained regardless of host env.
    #[test]
    fn dedup_fn_retains_oauth_when_no_anthropic_declared() {
        let mut creds = vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "tok".into())];
        let pt = vec![EnvVar("OPENAI_API_KEY".into())]; // covers openai, not anthropic
                                                        // Even if OPENAI_API_KEY is set, it doesn't cover the anthropic service.
        let lookup = |name: &str| -> Option<String> {
            if name == "OPENAI_API_KEY" {
                Some("sk-openai".into())
            } else {
                None
            }
        };
        dedup_credentials_by_declared_env(&mut creds, &pt, &[], &lookup);
        assert_eq!(
            creds.len(),
            1,
            "no anthropic declared → OAuth must be retained"
        );
    }

    /// env_literal (always-valued) coverage → same-service credential dropped.
    #[test]
    fn dedup_fn_handles_env_literal_source() {
        let mut creds = vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "tok".into())];
        let lit = vec![EnvLiteral {
            key: "ANTHROPIC_API_KEY".into(),
            value: "literal-key".into(),
        }];
        // lookup_env is irrelevant for literals but must be provided.
        let lookup = |_: &str| -> Option<String> { None };
        dedup_credentials_by_declared_env(&mut creds, &[], &lit, &lookup);
        assert!(
            creds.is_empty(),
            "env_literal coverage must also trigger dedup"
        );
    }

    /// No declared vars → no dedup, regardless of host env.
    #[test]
    fn dedup_fn_retains_credential_when_no_declared_vars() {
        let mut creds = vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "tok".into())];
        let lookup = |_: &str| -> Option<String> { None };
        dedup_credentials_by_declared_env(&mut creds, &[], &[], &lookup);
        assert_eq!(creds.len(), 1, "no declared vars → no dedup");
    }

    /// A credential with no known service mapping is never dropped by the dedup.
    #[test]
    fn dedup_fn_unmapped_credential_is_never_dropped() {
        let mut creds = vec![
            ("CLAUDE_CODE_OAUTH_TOKEN".into(), "tok".into()),
            ("MY_CUSTOM_INTERNAL_TOKEN".into(), "custom".into()),
        ];
        let pt = vec![EnvVar("ANTHROPIC_API_KEY".into())];
        let lookup = |name: &str| -> Option<String> {
            if name == "ANTHROPIC_API_KEY" {
                Some("sk-ant".into())
            } else {
                None
            }
        };
        dedup_credentials_by_declared_env(&mut creds, &pt, &[], &lookup);
        // OAuth (anthropic) dropped; custom (no mapping) retained.
        assert!(
            !creds.iter().any(|(k, _)| k == "CLAUDE_CODE_OAUTH_TOKEN"),
            "OAuth must be dropped when anthropic passthrough is set"
        );
        assert!(
            creds.iter().any(|(k, _)| k == "MY_CUSTOM_INTERNAL_TOKEN"),
            "unmapped credential must survive dedup; got: {creds:?}"
        );
    }

    /// When keychain credential's OWN name equals the declared passthrough var
    /// (e.g. declared env(ANTHROPIC_API_KEY) + keychain returns ANTHROPIC_API_KEY
    /// itself), the keychain entry is dropped because both map to service
    /// "anthropic" and the passthrough is set on host.
    #[test]
    fn dedup_fn_drops_when_credential_name_equals_declared_var() {
        let mut creds = vec![("ANTHROPIC_API_KEY".into(), "sk-ant-from-keychain".into())];
        let pt = vec![EnvVar("ANTHROPIC_API_KEY".into())];
        // Passthrough is set on host (perhaps to a different value).
        let lookup = |name: &str| -> Option<String> {
            if name == "ANTHROPIC_API_KEY" {
                Some("sk-ant-from-env".into())
            } else {
                None
            }
        };
        dedup_credentials_by_declared_env(&mut creds, &pt, &[], &lookup);
        assert!(
            creds.is_empty(),
            "credential whose own name equals the declared var must be dropped; \
             got: {creds:?}"
        );
    }

    /// Multi-service: declared var covers service X (anthropic), credential for
    /// service Y (openai) is retained.
    #[test]
    fn dedup_fn_multi_service_declared_x_retains_credential_for_y() {
        let mut creds = vec![
            ("CLAUDE_CODE_OAUTH_TOKEN".into(), "tok-anthropic".into()),
            ("OPENAI_API_KEY".into(), "tok-openai".into()),
        ];
        let pt = vec![EnvVar("ANTHROPIC_API_KEY".into())];
        // Only anthropic passthrough is set on host.
        let lookup = |name: &str| -> Option<String> {
            if name == "ANTHROPIC_API_KEY" {
                Some("sk-ant".into())
            } else {
                None
            }
        };
        dedup_credentials_by_declared_env(&mut creds, &pt, &[], &lookup);
        // Anthropic OAuth dropped; OpenAI retained (different service, not declared).
        assert!(
            !creds.iter().any(|(k, _)| k == "CLAUDE_CODE_OAUTH_TOKEN"),
            "anthropic OAuth must be dropped"
        );
        assert!(
            creds.iter().any(|(k, _)| k == "OPENAI_API_KEY"),
            "openai credential must be retained (covers service 'openai', not declared); \
             got: {creds:?}"
        );
    }
}
