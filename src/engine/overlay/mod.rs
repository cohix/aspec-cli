//! `engine::overlay` — `OverlayEngine`.
//!
//! Consolidates overlay construction and management. Layer 0 *resolves* host
//! paths; this layer *builds* the resolved overlay specs that
//! `ContainerOption::Overlay` accepts. Replaces `oldsrc/overlays/` and the
//! agent-settings-passthrough bits of `oldsrc/passthrough.rs`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::data::fs::auth_paths::AuthPathResolver;
use crate::data::fs::overlay_paths::OverlayPathResolver;
use crate::data::fs::skill_library::read_library_meta;
use crate::data::session::{AgentName, Session};
use crate::engine::container::options::{OverlayPermission, OverlaySpec};
use crate::engine::error::EngineError;

/// Top-level entries in `~/.claude/` that the legacy code excludes when
/// preparing a sanitized overlay copy. Single source of truth.
pub const CLAUDE_DENYLIST: &[&str] = &[
    "projects",
    "sessions",
    "session-env",
    "debug",
    "file-history",
    "history.jsonl",
    "telemetry",
    "downloads",
    "ide",
    "shell-snapshots",
    "paste-cache",
];

/// Scope for a context overlay — lives here in Layer 1 so both the engine
/// (Layer 1) and command (Layer 2) layers can reference it without an
/// upward dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextScope {
    Global,
    Repo,
    Workflow,
}

/// A resolved context-directory overlay (host path already ensured-to-exist).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextOverlay {
    pub scope: ContextScope,
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    pub permission: OverlayPermission,
}

/// Description of "overlays I want for this command, with these flags".
#[derive(Debug, Default, Clone)]
pub struct OverlayRequest {
    /// Inline directory specs (host:container[:perm]).
    pub directories: Vec<DirectorySpec>,
    /// When true, mount all skill directories.
    pub include_all_skills: bool,
    /// Named skills to mount (when `include_all_skills` is false).
    pub named_skills: Vec<String>,
    /// Whether to include agent-settings overlays for `agent`. When `Some`
    /// the engine prepares per-agent host configs (e.g. `~/.claude.json`).
    pub agent: Option<AgentName>,
    /// When `true`, write `skipDangerousModePermissionPrompt: true` into the
    /// prepared Claude `settings.json` (Yolo mode).
    pub yolo: bool,
    /// Override container `$HOME` (defaults to `/root`).
    pub container_home: Option<String>,
    /// Context-directory overlays (global/repo/workflow).
    pub context_overlays: Vec<ContextOverlay>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectorySpec {
    pub host: String,
    pub container: String,
    pub permission: OverlayPermission,
}

/// Resolved directory overlay (after canonicalization + tilde expansion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryOverlay {
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    pub permission: OverlayPermission,
}

/// Pluggable provider for per-agent file-form keychain artifacts. The
/// production binding shells out to the host OS keychain
/// (`engine::auth::keychain::agent_keychain_files`); tests inject a stub so
/// they don't accidentally read the dev's real macOS keychain.
pub type AgentSecretFilesProvider = std::sync::Arc<
    dyn Fn(&AgentName) -> Vec<crate::engine::auth::keychain::AgentSecretFile> + Send + Sync,
>;

pub struct OverlayEngine {
    auth_resolver: AuthPathResolver,
    /// Source of file-form host-keychain artifacts to plant into agent
    /// settings overlays (e.g. `~/.gemini/antigravity-cli/...`). Injectable
    /// for testability; defaults to the real host-keychain reader.
    secret_files_provider: AgentSecretFilesProvider,
    /// Sanitized temp directories that back agent-settings overlays. Held
    /// here so the directories live as long as this engine instance and are
    /// removed on `Drop` (RAII via `tempfile::TempDir`). This prevents the
    /// sanitized `~/.claude.json` and copied `~/.claude/` contents from
    /// leaking to `/tmp` after process exit.
    sanitized: std::sync::Mutex<Vec<tempfile::TempDir>>,
}

impl std::fmt::Debug for OverlayEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlayEngine")
            .field("auth_resolver", &self.auth_resolver)
            .field("sanitized", &"<TempDir guard>")
            .finish_non_exhaustive()
    }
}

impl OverlayEngine {
    pub fn new(_session: &Session) -> Result<Self, EngineError> {
        let auth_resolver = AuthPathResolver::from_process_env().map_err(EngineError::Data)?;
        Ok(Self {
            auth_resolver,
            secret_files_provider: default_secret_files_provider(),
            sanitized: std::sync::Mutex::new(Vec::new()),
        })
    }

    pub fn with_auth_resolver(auth_resolver: AuthPathResolver) -> Self {
        Self {
            auth_resolver,
            secret_files_provider: default_secret_files_provider(),
            sanitized: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Replace the keychain provider. Used in tests to substitute a stub for
    /// the OS-keychain reader so the test suite stays deterministic and never
    /// reads a developer's real credentials.
    pub fn with_secret_files_provider(mut self, provider: AgentSecretFilesProvider) -> Self {
        self.secret_files_provider = provider;
        self
    }

    /// Track a sanitized tempdir so its cleanup is deferred until this
    /// engine is dropped.
    fn retain_tempdir(&self, dir: tempfile::TempDir) -> PathBuf {
        let path = dir.path().to_path_buf();
        if let Ok(mut guard) = self.sanitized.lock() {
            guard.push(dir);
        }
        path
    }

    /// Build the resolved overlay set for a request. Deduplicated by
    /// canonicalized host path; most restrictive permission wins.
    pub fn build_overlays(
        &self,
        session: &Session,
        request: &OverlayRequest,
    ) -> Result<Vec<OverlaySpec>, EngineError> {
        let mut by_key: HashMap<String, OverlaySpec> = HashMap::new();

        // 1. User-supplied directory overlays.
        for spec in &request.directories {
            let resolved = self.resolve_user_overlay(
                spec,
                session.working_dir(),
                request.container_home.as_deref(),
            )?;
            let key = OverlayPathResolver::conflict_key(&resolved.host_path);
            insert_or_merge(&mut by_key, key, resolved);
        }

        // 2. Agent settings overlays. Forward the yolo flag so Claude's
        //    settings sanitization can inject the bypass-permissions overlay,
        //    and the request's container_home so settings paths agree with
        //    user-supplied overlays.
        if let Some(agent) = &request.agent {
            for spec in self.agent_settings_overlays_with(
                agent,
                request.yolo,
                session.git_root(),
                request.container_home.as_deref(),
            )? {
                let key = OverlayPathResolver::conflict_key(&spec.host_path);
                insert_or_merge(&mut by_key, key, spec);
            }
        }

        // 3. Skills overlay (mount ~/.awman/skills/ read-only into agent's native path).
        if request.include_all_skills || !request.named_skills.is_empty() {
            if let Some(agent) = &request.agent {
                for spec in self.skill_overlays(
                    agent,
                    request.include_all_skills,
                    &request.named_skills,
                    &request.container_home,
                    session.git_root(),
                )? {
                    let key = OverlayPathResolver::conflict_key(&spec.host_path);
                    insert_or_merge(&mut by_key, key, spec);
                }
            }
        }

        // 4. Context-directory overlays.
        for ctx in &request.context_overlays {
            let spec = OverlaySpec {
                host_path: ctx.host_path.clone(),
                container_path: ctx.container_path.clone(),
                permission: ctx.permission,
            };
            let key = OverlayPathResolver::conflict_key(&spec.host_path);
            insert_or_merge(&mut by_key, key, spec);
        }

        let mut out: Vec<OverlaySpec> = by_key.into_values().collect();
        out.sort_by(|a, b| a.host_path.cmp(&b.host_path));
        Ok(out)
    }

    /// Resolve a single user-supplied overlay spec into its canonical form.
    ///
    /// Relative host paths are resolved against `cwd` (the session's working
    /// directory), not the process's current directory.
    ///
    /// Fails fast when the host path does not exist on disk. Without this
    /// guard, Docker would auto-create an empty bind-mount source at run
    /// time and silently break tools that expect real content there
    /// (e.g. `ssh()` against a missing `~/.ssh`).
    pub fn resolve_user_overlay(
        &self,
        spec: &DirectorySpec,
        cwd: &Path,
        container_home: Option<&str>,
    ) -> Result<OverlaySpec, EngineError> {
        // Allow container paths starting with ~/ (expanded below).
        if !Path::new(&spec.container).is_absolute() && !spec.container.starts_with("~/") {
            return Err(EngineError::Other(format!(
                "overlay container path '{}' must be absolute",
                spec.container
            )));
        }
        let host_abs = OverlayPathResolver::make_absolute_with_cwd(&spec.host, cwd);
        let host_canon = OverlayPathResolver::canonicalize_lossy(&host_abs);
        if !host_canon.exists() {
            return Err(EngineError::Other(format!(
                "overlay host path '{}' does not exist (resolved to '{}')",
                spec.host,
                host_canon.display()
            )));
        }
        // Expand ~/ in container path to the container home directory.
        let container_path = if spec.container.starts_with("~/") {
            let home = container_home.unwrap_or("/root");
            format!("{}{}", home, &spec.container[1..])
        } else {
            spec.container.clone()
        };
        Ok(OverlaySpec {
            host_path: host_canon,
            container_path: PathBuf::from(container_path),
            permission: spec.permission,
        })
    }

    /// Per-agent settings overlays. Returns the host paths that exist; an
    /// empty list when the agent has no configured credentials on disk.
    pub fn agent_settings_overlays(
        &self,
        agent: &AgentName,
        git_root: &Path,
    ) -> Result<Vec<OverlaySpec>, EngineError> {
        self.agent_settings_overlays_with(agent, false, git_root, None)
    }

    /// Like `agent_settings_overlays` but threading the `yolo` flag so the
    /// Claude agent path can inject the bypass-permissions setting, and an
    /// optional `container_home_override` so all overlay container paths
    /// agree on the agent's home directory (matches `resolve_user_overlay`
    /// and `skill_overlays`).
    pub fn agent_settings_overlays_with(
        &self,
        agent: &AgentName,
        yolo: bool,
        git_root: &Path,
        container_home_override: Option<&str>,
    ) -> Result<Vec<OverlaySpec>, EngineError> {
        let home = self.auth_resolver.home();
        let paths = self.auth_resolver.resolve(agent.as_str());
        let mut out = Vec::new();
        let container_home = container_home_override
            .map(|s| s.to_string())
            .or_else(|| detect_container_home(home, agent.as_str(), git_root))
            .unwrap_or_else(|| "/root".to_string());

        match agent.as_str() {
            "claude" => {
                let has_config = paths
                    .config_file
                    .as_ref()
                    .map(|p| p.exists())
                    .unwrap_or(false);
                if has_config {
                    let cfg = paths.config_file.as_ref().unwrap();
                    let host_path = match sanitize_claude_config(cfg) {
                        Ok((dir, path)) => {
                            let _retained = self.retain_tempdir(dir);
                            path
                        }
                        Err(_) => cfg.clone(),
                    };
                    out.push(OverlaySpec {
                        host_path,
                        container_path: PathBuf::from(format!("{container_home}/.claude.json")),
                        permission: OverlayPermission::ReadWrite,
                    });
                } else {
                    // First-time user: no ~/.claude.json on host. Synthesize a
                    // minimal config with the /workspace trust dialog accepted
                    // so the agent doesn't prompt inside the container.
                    let host_path = match synthesize_minimal_claude_config() {
                        Ok((dir, path)) => {
                            let _retained = self.retain_tempdir(dir);
                            path
                        }
                        Err(_) => {
                            // Can't create temp file — skip this overlay.
                            PathBuf::new()
                        }
                    };
                    if host_path.exists() {
                        out.push(OverlaySpec {
                            host_path,
                            container_path: PathBuf::from(format!("{container_home}/.claude.json")),
                            permission: OverlayPermission::ReadWrite,
                        });
                    }
                }
                let has_settings_dir = paths
                    .settings_dir
                    .as_ref()
                    .map(|p| p.exists())
                    .unwrap_or(false);
                if has_settings_dir {
                    let dir = paths.settings_dir.as_ref().unwrap();
                    let host_path = match sanitize_claude_settings_dir(dir, yolo) {
                        Ok((tmp, path)) => {
                            let _retained = self.retain_tempdir(tmp);
                            path
                        }
                        Err(_) => dir.clone(),
                    };
                    out.push(OverlaySpec {
                        host_path,
                        container_path: PathBuf::from(format!("{container_home}/.claude")),
                        permission: OverlayPermission::ReadWrite,
                    });
                } else {
                    // First-time user: no ~/.claude/ on host. Synthesize a
                    // minimal settings dir with LSP suppression.
                    if let Ok((tmp, path)) = synthesize_minimal_claude_settings_dir(yolo) {
                        let _retained = self.retain_tempdir(tmp);
                        out.push(OverlaySpec {
                            host_path: path,
                            container_path: PathBuf::from(format!("{container_home}/.claude")),
                            permission: OverlayPermission::ReadWrite,
                        });
                    }
                }
            }
            "codex" => {
                if let Some(dir) = paths.settings_dir.as_ref() {
                    if dir.exists() {
                        out.push(OverlaySpec {
                            host_path: dir.clone(),
                            container_path: PathBuf::from(format!("{container_home}/.codex")),
                            permission: OverlayPermission::ReadWrite,
                        });
                    }
                }
            }
            "gemini" => {
                if let Some(dir) = paths.settings_dir.as_ref() {
                    if dir.exists() {
                        out.push(OverlaySpec {
                            host_path: dir.clone(),
                            container_path: PathBuf::from(format!("{container_home}/.gemini")),
                            permission: OverlayPermission::ReadWrite,
                        });
                    }
                }
            }
            "antigravity" => {
                // Antigravity reads its OAuth token from a fixed file inside
                // `~/.gemini/antigravity-cli/` when the in-container keyring
                // (Secret Service / D-Bus) is unreachable — which is always
                // the case in our agent containers. We pull the same token
                // from the host keychain and seed it into the staged dir.
                let secret_files = (self.secret_files_provider)(agent);
                let host_dir = paths.settings_dir.as_ref();
                let dir_exists = host_dir.map(|p| p.exists()).unwrap_or(false);
                if dir_exists || !secret_files.is_empty() {
                    let staged = if dir_exists {
                        stage_settings_dir_with_secrets(
                            host_dir.unwrap(),
                            &secret_files,
                            "awman-antigravity-",
                        )
                    } else {
                        // First-time user: no host `~/.gemini` but a keychain
                        // token is still good enough for agy to authenticate.
                        synthesize_settings_dir_with_secrets(
                            &secret_files,
                            "awman-antigravity-minimal-",
                        )
                    };
                    let host_path = match staged {
                        Ok((tmp, path)) => {
                            let _retained = self.retain_tempdir(tmp);
                            path
                        }
                        Err(_) => host_dir
                            .cloned()
                            .unwrap_or_else(|| PathBuf::from("/nonexistent")),
                    };
                    if host_path.exists() {
                        out.push(OverlaySpec {
                            host_path,
                            container_path: PathBuf::from(format!("{container_home}/.gemini")),
                            permission: OverlayPermission::ReadWrite,
                        });
                    }
                }
            }
            "opencode" => {
                if let Some(dir) = paths.settings_dir.as_ref() {
                    if dir.exists() {
                        out.push(OverlaySpec {
                            host_path: dir.clone(),
                            container_path: PathBuf::from(format!(
                                "{container_home}/.config/opencode"
                            )),
                            permission: OverlayPermission::ReadWrite,
                        });
                    }
                }
            }
            "crush" => {
                let dir = home.join(".config").join("crush");
                if dir.exists() {
                    out.push(OverlaySpec {
                        host_path: dir,
                        container_path: PathBuf::from(format!("{container_home}/.config/crush")),
                        permission: OverlayPermission::ReadWrite,
                    });
                }
            }
            "cline" => {
                let dir = home.join(".cline").join("data");
                if dir.exists() {
                    out.push(OverlaySpec {
                        host_path: dir,
                        container_path: PathBuf::from(format!("{container_home}/.cline/data")),
                        permission: OverlayPermission::ReadWrite,
                    });
                }
            }
            // copilot, maki: no host overlays.
            _ => {}
        }

        Ok(out)
    }

    /// Build overlay specs for the global skills directory, mapping it to the
    /// agent's native skills/commands path inside the container (read-only).
    pub fn skill_overlays(
        &self,
        agent: &AgentName,
        include_all: bool,
        names: &[String],
        container_home_override: &Option<String>,
        git_root: &Path,
    ) -> Result<Vec<OverlaySpec>, EngineError> {
        // Early return when no skills requested.
        if !include_all && names.is_empty() {
            return Ok(vec![]);
        }
        let skill_dirs = crate::data::fs::skill_dirs::SkillDirs::from_process_env(None)
            .map_err(EngineError::Data)?;
        let host_skills_dir = skill_dirs.global_dir();
        if !host_skills_dir.exists() {
            tracing::debug!(
                path = %host_skills_dir.display(),
                "global skills directory does not exist; skipping skills overlay"
            );
            return Ok(vec![]);
        }

        let home = self.auth_resolver.home();
        let container_home = container_home_override.clone().unwrap_or_else(|| {
            detect_container_home(home, agent.as_str(), git_root)
                .unwrap_or_else(|| "/root".to_string())
        });

        let container_path = match agent.as_str() {
            "claude" => format!("{container_home}/.claude/commands"),
            "codex" => format!("{container_home}/.codex/skills"),
            "opencode" => format!("{container_home}/.config/opencode/commands"),
            "gemini" => format!("{container_home}/.gemini/commands"),
            "antigravity" => format!("{container_home}/.gemini/antigravity-cli/skills"),
            "copilot" => format!("{container_home}/.copilot/instructions"),
            "crush" => format!("{container_home}/.config/crush/commands"),
            "cline" => format!("{container_home}/.cline/skills"),
            "maki" => {
                tracing::warn!(
                    agent = "maki",
                    "skills overlay is not supported for maki; no known skills directory"
                );
                return Ok(vec![]);
            }
            other => {
                tracing::warn!(agent = other, "skills overlay: unknown agent, skipping");
                return Ok(vec![]);
            }
        };

        if include_all {
            Ok(vec![OverlaySpec {
                host_path: OverlayPathResolver::canonicalize_lossy(&host_skills_dir),
                container_path: PathBuf::from(container_path),
                permission: OverlayPermission::ReadOnly,
            }])
        } else {
            let mut specs = Vec::new();
            for name in names {
                match name.split_once('/') {
                    // ── Single skill inside a pulled library: `library/skill` ──
                    //
                    // Mount only `<library>/<subdir>/<skill>` at
                    // `{container_path}/<library>/<skill>`, preserving the
                    // library namespace so `skill(lib)` and `skill(lib/skill)`
                    // never collide on container path when both are requested.
                    Some((library, skill)) => {
                        validate_skill_reference_segment(library, name)?;
                        validate_skill_reference_segment(skill, name)?;
                        let library_dir = skill_dirs.library_dir(library);
                        if !library_dir.exists() {
                            return Err(EngineError::Other(format!(
                                "skill library '{library}' not found in {} (for named skill '{name}')",
                                skill_dirs.library_root().display()
                            )));
                        }
                        let meta = read_library_meta(&library_dir).map_err(EngineError::Data)?;
                        let subdir = validate_library_subdir(&meta.subdir)?;
                        let skill_path = library_dir.join(&subdir).join(skill);
                        // A skill is a directory holding a `SKILL.md`. Merely
                        // existing is not enough: mounting an arbitrary
                        // directory inside a clone would expose non-skill
                        // content (including `.git/`) to the agent.
                        if !skill_path.is_dir() || !skill_path.join("SKILL.md").is_file() {
                            return Err(EngineError::Other(format!(
                                "skill '{skill}' not found in library '{library}' (looked for a SKILL.md in {})",
                                skill_path.display()
                            )));
                        }
                        specs.push(OverlaySpec {
                            host_path: OverlayPathResolver::canonicalize_lossy(&skill_path),
                            container_path: PathBuf::from(format!(
                                "{container_path}/{library}/{skill}"
                            )),
                            permission: OverlayPermission::ReadOnly,
                        });
                    }
                    // ── No slash: a plain skill, or a whole pulled library ──
                    None => {
                        validate_skill_reference_segment(name, name)?;
                        // 1. Plain skill wins — a user's own local skill is
                        //    never shadowed by a same-named pulled library.
                        let plain_dir = host_skills_dir.join(name);
                        if plain_dir.exists() {
                            specs.push(OverlaySpec {
                                host_path: OverlayPathResolver::canonicalize_lossy(&plain_dir),
                                container_path: PathBuf::from(format!("{container_path}/{name}")),
                                permission: OverlayPermission::ReadOnly,
                            });
                            continue;
                        }
                        // 2. Whole library — mount `<library>/<subdir>` at
                        //    `{container_path}/<name>`, giving the same mount
                        //    shape as any other named skill (a directory of
                        //    `<skill>/SKILL.md` entries).
                        let library_dir = skill_dirs.library_dir(name);
                        if library_dir.exists() {
                            let meta =
                                read_library_meta(&library_dir).map_err(EngineError::Data)?;
                            let subdir = validate_library_subdir(&meta.subdir)?;
                            let mount = library_dir.join(&subdir);
                            specs.push(OverlaySpec {
                                host_path: OverlayPathResolver::canonicalize_lossy(&mount),
                                container_path: PathBuf::from(format!("{container_path}/{name}")),
                                permission: OverlayPermission::ReadOnly,
                            });
                            continue;
                        }
                        // 3. Nothing resolved — name both search locations.
                        return Err(EngineError::Other(format!(
                            "named skill '{name}' not found in {} or {}",
                            host_skills_dir.display(),
                            skill_dirs.library_root().display()
                        )));
                    }
                }
            }
            Ok(specs)
        }
    }
}

/// Strip `oauthAccount` from `~/.claude.json`, inject
/// `projects["/workspace"]["hasTrustDialogAccepted"] = true` to suppress the
/// in-container trust dialog, and write the result to a `TempDir` whose
/// lifetime is owned by the caller. The sanitized path is `<tempdir>/claude.json`.
fn sanitize_claude_config(src: &Path) -> Result<(tempfile::TempDir, PathBuf), std::io::Error> {
    let raw = std::fs::read_to_string(src)?;
    let mut value: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}));
    if let serde_json::Value::Object(obj) = &mut value {
        obj.remove("oauthAccount");

        // Mark `/workspace` as a trusted project so Claude does not prompt for
        // trust inside the container. Mirrors legacy
        // `oldsrc/runtime/mod.rs::sanitize_claude_config`.
        let projects = obj
            .entry("projects".to_string())
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        if let serde_json::Value::Object(p) = projects {
            let project = p
                .entry("/workspace".to_string())
                .or_insert_with(|| serde_json::Value::Object(Default::default()));
            if let serde_json::Value::Object(pobj) = project {
                pobj.insert(
                    "hasTrustDialogAccepted".into(),
                    serde_json::Value::Bool(true),
                );
            }
        }
    }

    let tmp_dir = tempfile::Builder::new().prefix("awman-claude-").tempdir()?;
    let dest = tmp_dir.path().join("claude.json");
    let body = serde_json::to_string_pretty(&value).unwrap_or(raw);
    std::fs::write(&dest, body)?;
    Ok((tmp_dir, dest))
}

/// Sanitize `~/.claude/`: filter out denylisted entries, optionally inject
/// the yolo-mode settings file, and suppress the LSP recommendation banner.
/// Returns the `TempDir` (cleaned on drop) and its path.
fn sanitize_claude_settings_dir(
    src: &Path,
    yolo: bool,
) -> Result<(tempfile::TempDir, PathBuf), std::io::Error> {
    let tmp = tempfile::Builder::new()
        .prefix("awman-claude-dir-")
        .tempdir()?;
    let tmp_root = tmp.path().to_path_buf();
    // Mirror only the entries that are not on the denylist.
    let denylist: std::collections::HashSet<&str> = CLAUDE_DENYLIST.iter().copied().collect();
    if let Ok(entries) = std::fs::read_dir(src) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if denylist.contains(name_str.as_ref()) {
                continue;
            }
            let dest = tmp_root.join(&name);
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                copy_dir_all(&entry.path(), &dest)?;
            } else {
                std::fs::copy(entry.path(), dest)?;
            }
        }
    }
    // Inject (or update) settings.json to suppress LSP banner and optionally
    // grant yolo bypass-permissions.
    let settings_path = tmp_root.join("settings.json");
    let mut settings: serde_json::Value = if settings_path.exists() {
        std::fs::read_to_string(&settings_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    if let serde_json::Value::Object(obj) = &mut settings {
        // Set both LSP suppression keys for compatibility with different
        // Claude Code versions.
        obj.insert(
            "hasShownLspRecommendation".into(),
            serde_json::Value::Bool(true),
        );
        obj.insert(
            "lspRecommendationDismissed".into(),
            serde_json::Value::Bool(true),
        );
        if yolo {
            obj.insert(
                "skipDangerousModePermissionPrompt".into(),
                serde_json::Value::Bool(true),
            );
            obj.insert(
                "permissionMode".into(),
                serde_json::Value::String("bypassPermissions".into()),
            );
        }
    }
    let body = serde_json::to_string_pretty(&settings).unwrap_or_default();
    let _ = std::fs::write(&settings_path, body);
    Ok((tmp, tmp_root))
}

/// Synthesize a minimal `.claude.json` for first-time users: trust dialog
/// accepted for `/workspace`, no oauthAccount.
fn synthesize_minimal_claude_config() -> Result<(tempfile::TempDir, PathBuf), std::io::Error> {
    let value = serde_json::json!({
        "projects": {
            "/workspace": {
                "hasTrustDialogAccepted": true
            }
        }
    });
    let tmp_dir = tempfile::Builder::new()
        .prefix("awman-claude-minimal-")
        .tempdir()?;
    let dest = tmp_dir.path().join("claude.json");
    let body = serde_json::to_string_pretty(&value).unwrap_or_default();
    std::fs::write(&dest, body)?;
    Ok((tmp_dir, dest))
}

/// Synthesize a minimal `~/.claude/` directory for first-time users with
/// LSP suppression and (optionally) yolo bypass.
fn synthesize_minimal_claude_settings_dir(
    yolo: bool,
) -> Result<(tempfile::TempDir, PathBuf), std::io::Error> {
    let tmp = tempfile::Builder::new()
        .prefix("awman-claude-dir-minimal-")
        .tempdir()?;
    let tmp_root = tmp.path().to_path_buf();
    let mut settings = serde_json::json!({});
    if let serde_json::Value::Object(obj) = &mut settings {
        obj.insert(
            "hasShownLspRecommendation".into(),
            serde_json::Value::Bool(true),
        );
        obj.insert(
            "lspRecommendationDismissed".into(),
            serde_json::Value::Bool(true),
        );
        if yolo {
            obj.insert(
                "skipDangerousModePermissionPrompt".into(),
                serde_json::Value::Bool(true),
            );
            obj.insert(
                "permissionMode".into(),
                serde_json::Value::String("bypassPermissions".into()),
            );
        }
    }
    let body = serde_json::to_string_pretty(&settings).unwrap_or_default();
    std::fs::write(tmp_root.join("settings.json"), body)?;
    Ok((tmp, tmp_root))
}

/// Copy a host settings dir into a `TempDir` snapshot, then write each
/// `AgentSecretFile` into the staged tree (creating parent dirs as needed).
///
/// Reusable across any agent whose container expects an on-disk credential
/// file inside its settings dir. Currently used by antigravity to seed
/// `antigravity-cli/antigravity-oauth-token` alongside the host's `~/.gemini`
/// snapshot; structured so future agents (e.g. ones that store tokens in
/// libsecret on Linux) can drop straight in.
fn stage_settings_dir_with_secrets(
    src: &Path,
    secret_files: &[crate::engine::auth::keychain::AgentSecretFile],
    tmpdir_prefix: &str,
) -> Result<(tempfile::TempDir, PathBuf), std::io::Error> {
    let tmp = tempfile::Builder::new().prefix(tmpdir_prefix).tempdir()?;
    let tmp_root = tmp.path().to_path_buf();
    copy_dir_all(src, &tmp_root)?;
    for f in secret_files {
        write_secret_file(&tmp_root, f)?;
    }
    Ok((tmp, tmp_root))
}

/// Build a fresh empty settings dir and plant the given secret files into it.
/// Used for first-time-user paths where the host has no settings dir on disk
/// but the agent's keychain entry is sufficient on its own.
fn synthesize_settings_dir_with_secrets(
    secret_files: &[crate::engine::auth::keychain::AgentSecretFile],
    tmpdir_prefix: &str,
) -> Result<(tempfile::TempDir, PathBuf), std::io::Error> {
    let tmp = tempfile::Builder::new().prefix(tmpdir_prefix).tempdir()?;
    let tmp_root = tmp.path().to_path_buf();
    for f in secret_files {
        write_secret_file(&tmp_root, f)?;
    }
    Ok((tmp, tmp_root))
}

/// Write a single `AgentSecretFile` under the staged root, creating parent
/// directories. On Unix the file is opened with the requested mode so the
/// secret never lands on disk world-readable.
fn write_secret_file(
    staged_root: &Path,
    file: &crate::engine::auth::keychain::AgentSecretFile,
) -> std::io::Result<()> {
    let dest = staged_root.join(&file.relative_path);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;
        let mut handle = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(file.mode)
            .open(&dest)?;
        handle.write_all(&file.contents)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&dest, &file.contents)?;
    }
    Ok(())
}

/// Production binding for `AgentSecretFilesProvider`: reads file-form
/// keychain artifacts from the host OS keychain via
/// `engine::auth::keychain::agent_keychain_files`.
fn default_secret_files_provider() -> AgentSecretFilesProvider {
    std::sync::Arc::new(|agent: &AgentName| {
        crate::engine::auth::keychain::agent_keychain_files(agent)
    })
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    if let Ok(entries) = std::fs::read_dir(src) {
        for entry in entries.flatten() {
            let target = dst.join(entry.file_name());
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                copy_dir_all(&entry.path(), &target)?;
            } else {
                std::fs::copy(entry.path(), target)?;
            }
        }
    }
    Ok(())
}

/// Parse a Dockerfile for the last non-root `USER` directive and return
/// `/home/<name>`. Returns `None` when the file doesn't exist, can't be read,
/// or only uses root.
pub(crate) fn detect_home_from_dockerfile(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut result: Option<String> = None;
    for line in content.lines() {
        let trimmed = line.trim();
        let upper = trimmed.to_uppercase();
        if let Some(rest) = upper.strip_prefix("USER ") {
            let name = rest.split_whitespace().next().unwrap_or("").trim();
            if !name.is_empty() && name != "ROOT" && name != "0" {
                let orig_rest = &trimmed[5..]; // skip "USER "
                let orig_name = orig_rest.split_whitespace().next().unwrap_or("root");
                result = Some(format!("/home/{orig_name}"));
            } else {
                // Switched back to root — reset.
                result = None;
            }
        }
    }
    result
}

/// Detect the container home directory by inspecting `Dockerfile.<agent>`.
///
/// Looks for a `USER <name>` directive (where `<name>` is not "root" or "0")
/// in `Dockerfile.<agent>` files under `<git_root>/.awman/` and `<home>/.awman/`.
/// Returns `Some("/home/<name>")` when found, `None` otherwise.
pub(crate) fn detect_container_home(home: &Path, agent: &str, git_root: &Path) -> Option<String> {
    let dockerfile_name = format!("Dockerfile.{agent}");
    let search_dirs: Vec<PathBuf> = [git_root.join(".awman"), home.join(".awman")]
        .into_iter()
        .collect();

    for dir in &search_dirs {
        let path = dir.join(&dockerfile_name);
        if let Some(home) = detect_home_from_dockerfile(&path) {
            return Some(home);
        }
    }
    None
}

/// Validate one segment of a `skill(...)` reference (a plain skill name, a
/// library name, or a skill name inside a library) as a single, contained
/// path component.
///
/// The overlay parser applies the same rule, but named skills also reach this
/// function from config files and the API, so containment is re-checked here:
/// an empty, `.`, or `..` segment would otherwise be joined onto a host path
/// and resolve to a directory the reference was never meant to name (e.g.
/// `skill(lib/..)` mounting the whole managed clone, `.git/` included).
fn validate_skill_reference_segment(segment: &str, name: &str) -> Result<(), EngineError> {
    let mut components = Path::new(segment).components();
    let first = components.next();
    let contained =
        matches!(first, Some(std::path::Component::Normal(_))) && components.next().is_none();
    if !contained {
        return Err(EngineError::Other(format!(
            "named skill '{name}' has an invalid path segment '{segment}'; segments must not be \
             empty, '.', '..', or contain a path separator"
        )));
    }
    Ok(())
}

/// Validate a persisted library `subdir` as a relative path *inside* the
/// managed clone and return its normalized form. Rejects empty values and any
/// absolute/root/prefix, `.`, or `..` component so a crafted `.awman.json`
/// (or `--subdir` value that produced it) can never turn a library mount into
/// a host path outside `skill_dirs.library_dir(<slug>)`. Mirrors the
/// containment rule applied by the command-layer pull orchestration.
fn validate_library_subdir(subdir: &str) -> Result<PathBuf, EngineError> {
    let mut normalized = PathBuf::new();
    let mut components = 0;
    for component in Path::new(subdir).components() {
        match component {
            std::path::Component::Normal(part) => {
                normalized.push(part);
                components += 1;
            }
            _ => {
                return Err(EngineError::Other(format!(
                    "skill library subdir '{subdir}' must be a relative path inside the \
                     library (no absolute, '.', or '..' components)"
                )));
            }
        }
    }
    if components == 0 {
        return Err(EngineError::Other(
            "skill library subdir must not be empty".to_string(),
        ));
    }
    Ok(normalized)
}

fn insert_or_merge(map: &mut HashMap<String, OverlaySpec>, key: String, spec: OverlaySpec) {
    use std::collections::hash_map::Entry;
    match map.entry(key) {
        Entry::Occupied(mut e) => {
            // Most restrictive permission wins.
            let existing = e.get_mut();
            if matches!(spec.permission, OverlayPermission::ReadOnly)
                && matches!(existing.permission, OverlayPermission::ReadWrite)
            {
                existing.permission = OverlayPermission::ReadOnly;
            }
            // Keep the existing container path; first writer wins for clarity.
        }
        Entry::Vacant(e) => {
            e.insert(spec);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::session::AgentName;

    /// Serialises tests that write to `AWMAN_CONFIG_HOME` (a process-global env var).
    static AWMAN_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Set `AWMAN_CONFIG_HOME` to `home`, run `f`, then restore the previous value.
    fn with_awman_config_home<F, R>(home: &Path, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let _g = AWMAN_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("AWMAN_CONFIG_HOME").ok();
        std::env::set_var("AWMAN_CONFIG_HOME", home.to_str().unwrap());
        let result = f();
        match prev {
            Some(v) => std::env::set_var("AWMAN_CONFIG_HOME", v),
            None => std::env::remove_var("AWMAN_CONFIG_HOME"),
        }
        result
    }

    fn make_engine(home: &Path) -> OverlayEngine {
        // Default test engine substitutes a no-op host-keychain reader so the
        // suite stays deterministic on dev macOS machines that may actually
        // have antigravity/claude credentials in their real keychain. Tests
        // that want to exercise the file-seed path inject their own provider
        // via `OverlayEngine::with_secret_files_provider`.
        OverlayEngine::with_auth_resolver(AuthPathResolver::at_home(home))
            .with_secret_files_provider(std::sync::Arc::new(|_| Vec::new()))
    }

    /// Build an engine with an explicit stub for file-form keychain artifacts.
    fn make_engine_with_secrets(
        home: &Path,
        files: Vec<crate::engine::auth::keychain::AgentSecretFile>,
    ) -> OverlayEngine {
        OverlayEngine::with_auth_resolver(AuthPathResolver::at_home(home))
            .with_secret_files_provider(std::sync::Arc::new(move |_| files.clone()))
    }

    // ─── skill_overlays ───────────────────────────────────────────────────────

    /// Create a temp dir, make `<dir>/skills/` exist, and return both.
    fn make_home_with_skills() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let skills = tmp.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        let skills_canon = std::fs::canonicalize(&skills).unwrap_or(skills);
        (tmp, skills_canon)
    }

    // ─── antigravity agent_settings_overlays ─────────────────────────────────

    #[test]
    fn antigravity_settings_overlay_when_dir_exists() {
        let tmp = tempfile::tempdir().unwrap();
        // Create ~/.gemini/ with a config file so the overlay fires.
        let gemini_dir = tmp.path().join(".gemini");
        std::fs::create_dir_all(&gemini_dir).unwrap();
        std::fs::write(gemini_dir.join("settings.json"), r#"{"key":"val"}"#).unwrap();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("antigravity").unwrap();

        let overlays = engine
            .agent_settings_overlays_with(&agent, false, tmp.path(), None)
            .unwrap();

        assert_eq!(
            overlays.len(),
            1,
            "exactly one overlay expected when ~/.gemini exists; got {overlays:?}"
        );
        assert!(
            overlays[0]
                .container_path
                .to_string_lossy()
                .ends_with(".gemini"),
            "container_path must end with .gemini; got {:?}",
            overlays[0].container_path
        );
        // Must be a temp-dir copy, not the original.
        assert_ne!(
            overlays[0].host_path, gemini_dir,
            "host_path must be a temp-dir copy, not the original ~/.gemini"
        );
        // The copied content must be present.
        assert!(
            overlays[0].host_path.join("settings.json").exists(),
            "copied settings.json must exist in the temp-dir overlay"
        );
    }

    #[test]
    fn antigravity_settings_overlay_empty_when_dir_absent() {
        let tmp = tempfile::tempdir().unwrap();
        // Deliberately do NOT create ~/.gemini/.
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("antigravity").unwrap();

        let overlays = engine
            .agent_settings_overlays_with(&agent, false, tmp.path(), None)
            .unwrap();

        assert!(
            overlays.is_empty(),
            "overlay list must be empty when ~/.gemini does not exist and no \
             keychain credential is available; got {overlays:?}"
        );
    }

    #[test]
    fn antigravity_settings_overlay_plants_keychain_token_file_alongside_host_copy() {
        use crate::engine::auth::keychain::AgentSecretFile;
        let tmp = tempfile::tempdir().unwrap();
        let gemini_dir = tmp.path().join(".gemini");
        std::fs::create_dir_all(&gemini_dir).unwrap();
        std::fs::write(gemini_dir.join("settings.json"), r#"{"model":"flash"}"#).unwrap();
        let token_payload = br#"{"token":{"access_token":"a","token_type":"Bearer",
            "refresh_token":"r","expiry":"2099-01-01T00:00:00Z"},"auth_method":"consumer"}"#;
        let engine = make_engine_with_secrets(
            tmp.path(),
            vec![AgentSecretFile {
                relative_path: std::path::PathBuf::from("antigravity-cli")
                    .join("antigravity-oauth-token"),
                contents: token_payload.to_vec(),
                mode: 0o600,
            }],
        );
        let agent = AgentName::new("antigravity").unwrap();

        let overlays = engine
            .agent_settings_overlays_with(&agent, false, tmp.path(), None)
            .unwrap();

        assert_eq!(overlays.len(), 1, "expected one .gemini overlay");
        let staged = &overlays[0].host_path;
        let staged_token = staged.join("antigravity-cli/antigravity-oauth-token");
        assert!(
            staged_token.exists(),
            "staged dir must contain antigravity-cli/antigravity-oauth-token; \
             listed under {:?}",
            std::fs::read_dir(staged).map(|d| d
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect::<Vec<_>>()),
        );
        assert_eq!(
            std::fs::read(&staged_token).unwrap(),
            token_payload.to_vec(),
            "staged token contents must round-trip"
        );
        // Host copy is preserved alongside the planted secret.
        assert!(staged.join("settings.json").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&staged_token)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "token file must be mode 0600; got {mode:o}");
        }
    }

    #[test]
    fn antigravity_settings_overlay_synthesizes_dir_when_only_keychain_present() {
        use crate::engine::auth::keychain::AgentSecretFile;
        let tmp = tempfile::tempdir().unwrap();
        // No host ~/.gemini, but keychain has a token. This mirrors the
        // first-time-container-user path where the host never ran agy
        // directly but did authorize it through some other route.
        let token_payload = br#"{"token":{"access_token":"a","token_type":"Bearer",
            "refresh_token":"r","expiry":"2099-01-01T00:00:00Z"},"auth_method":"consumer"}"#;
        let engine = make_engine_with_secrets(
            tmp.path(),
            vec![AgentSecretFile {
                relative_path: std::path::PathBuf::from("antigravity-cli")
                    .join("antigravity-oauth-token"),
                contents: token_payload.to_vec(),
                mode: 0o600,
            }],
        );
        let agent = AgentName::new("antigravity").unwrap();

        let overlays = engine
            .agent_settings_overlays_with(&agent, false, tmp.path(), None)
            .unwrap();

        assert_eq!(overlays.len(), 1, "expected synthesized overlay");
        let staged_token = overlays[0]
            .host_path
            .join("antigravity-cli/antigravity-oauth-token");
        assert!(staged_token.exists(), "synthesized dir must hold the token");
    }

    // ─── antigravity skill_overlays ───────────────────────────────────────────

    #[test]
    fn skill_overlays_returns_single_ro_spec_for_claude() {
        let (tmp, skills_canon) = make_home_with_skills();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();

        let specs = with_awman_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, true, &[], &None, Path::new("/"))
                .unwrap()
        });

        assert_eq!(specs.len(), 1, "expected 1 OverlaySpec; got {specs:?}");
        assert_eq!(
            specs[0].host_path, skills_canon,
            "host path must be global skills dir"
        );
        assert_eq!(
            specs[0].permission,
            OverlayPermission::ReadOnly,
            "must be :ro"
        );
        assert!(
            specs[0]
                .container_path
                .to_string_lossy()
                .contains("/.claude/commands"),
            "claude container path must contain /.claude/commands; got {:?}",
            specs[0].container_path
        );
    }

    #[test]
    fn skill_overlays_returns_single_ro_spec_for_codex() {
        let (tmp, skills_canon) = make_home_with_skills();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("codex").unwrap();

        let specs = with_awman_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, true, &[], &None, Path::new("/"))
                .unwrap()
        });

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].host_path, skills_canon);
        assert_eq!(specs[0].permission, OverlayPermission::ReadOnly);
        assert!(
            specs[0]
                .container_path
                .to_string_lossy()
                .contains("/.codex/skills"),
            "codex container path must contain /.codex/skills; got {:?}",
            specs[0].container_path
        );
    }

    #[test]
    fn skill_overlays_returns_single_ro_spec_for_gemini() {
        let (tmp, skills_canon) = make_home_with_skills();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("gemini").unwrap();

        let specs = with_awman_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, true, &[], &None, Path::new("/"))
                .unwrap()
        });

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].host_path, skills_canon);
        assert_eq!(specs[0].permission, OverlayPermission::ReadOnly);
        assert!(
            specs[0]
                .container_path
                .to_string_lossy()
                .contains("/.gemini/commands"),
            "gemini container path must contain /.gemini/commands; got {:?}",
            specs[0].container_path
        );
    }

    #[test]
    fn skill_overlays_returns_single_ro_spec_for_antigravity() {
        let (tmp, skills_canon) = make_home_with_skills();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("antigravity").unwrap();

        let specs = with_awman_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, true, &[], &None, Path::new("/"))
                .unwrap()
        });

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].host_path, skills_canon);
        assert_eq!(specs[0].permission, OverlayPermission::ReadOnly);
        assert!(
            specs[0]
                .container_path
                .to_string_lossy()
                .ends_with(".gemini/antigravity-cli/skills"),
            "antigravity container path must end with .gemini/antigravity-cli/skills; got {:?}",
            specs[0].container_path
        );
    }

    #[test]
    fn skill_overlays_returns_single_ro_spec_for_opencode() {
        let (tmp, skills_canon) = make_home_with_skills();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("opencode").unwrap();

        let specs = with_awman_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, true, &[], &None, Path::new("/"))
                .unwrap()
        });

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].host_path, skills_canon);
        assert_eq!(specs[0].permission, OverlayPermission::ReadOnly);
        assert!(
            specs[0]
                .container_path
                .to_string_lossy()
                .contains("/.config/opencode/commands"),
            "opencode container path must contain /.config/opencode/commands; got {:?}",
            specs[0].container_path
        );
    }

    #[test]
    fn skill_overlays_returns_single_ro_spec_for_copilot() {
        let (tmp, skills_canon) = make_home_with_skills();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("copilot").unwrap();

        let specs = with_awman_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, true, &[], &None, Path::new("/"))
                .unwrap()
        });

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].host_path, skills_canon);
        assert_eq!(specs[0].permission, OverlayPermission::ReadOnly);
        assert!(
            specs[0]
                .container_path
                .to_string_lossy()
                .contains("/.copilot/instructions"),
            "copilot container path must contain /.copilot/instructions; got {:?}",
            specs[0].container_path
        );
    }

    #[test]
    fn skill_overlays_returns_single_ro_spec_for_crush() {
        let (tmp, skills_canon) = make_home_with_skills();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("crush").unwrap();

        let specs = with_awman_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, true, &[], &None, Path::new("/"))
                .unwrap()
        });

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].host_path, skills_canon);
        assert_eq!(specs[0].permission, OverlayPermission::ReadOnly);
        assert!(
            specs[0]
                .container_path
                .to_string_lossy()
                .contains("/.config/crush/commands"),
            "crush container path must contain /.config/crush/commands; got {:?}",
            specs[0].container_path
        );
    }

    #[test]
    fn skill_overlays_returns_single_ro_spec_for_cline() {
        let (tmp, skills_canon) = make_home_with_skills();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("cline").unwrap();

        let specs = with_awman_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, true, &[], &None, Path::new("/"))
                .unwrap()
        });

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].host_path, skills_canon);
        assert_eq!(specs[0].permission, OverlayPermission::ReadOnly);
        assert!(
            specs[0]
                .container_path
                .to_string_lossy()
                .contains("/.cline/skills"),
            "cline container path must contain /.cline/skills; got {:?}",
            specs[0].container_path
        );
    }

    #[test]
    fn skill_overlays_returns_empty_when_skills_dir_does_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        // Deliberately do NOT create <tmp>/skills/.
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();

        let specs = with_awman_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, true, &[], &None, Path::new("/"))
                .unwrap()
        });

        assert!(
            specs.is_empty(),
            "must return empty vec when skills dir is absent; got {specs:?}"
        );
    }

    #[test]
    fn skill_overlays_returns_empty_for_maki_no_error() {
        let (tmp, _) = make_home_with_skills();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("maki").unwrap();

        let specs = with_awman_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, true, &[], &None, Path::new("/"))
                .unwrap()
        });

        assert!(
            specs.is_empty(),
            "maki must produce no skills mount; got {specs:?}"
        );
    }

    #[test]
    fn skill_overlays_uses_container_home_override_when_set() {
        let (tmp, _) = make_home_with_skills();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let override_home = Some("/home/appuser".to_string());

        let specs = with_awman_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, true, &[], &override_home, Path::new("/"))
                .unwrap()
        });

        assert_eq!(specs.len(), 1);
        assert!(
            specs[0]
                .container_path
                .to_string_lossy()
                .starts_with("/home/appuser/"),
            "container path must use the override home '/home/appuser'; got {:?}",
            specs[0].container_path
        );
    }

    #[test]
    fn skill_overlays_defaults_to_root_when_no_dockerfile_present() {
        let (tmp, _) = make_home_with_skills();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();

        let specs = with_awman_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, true, &[], &None, tmp.path())
                .unwrap()
        });

        assert_eq!(specs.len(), 1);
        assert!(
            specs[0].container_path.to_string_lossy().starts_with("/root/"),
            "container path must default to /root/ when detect_container_home returns None; got {:?}",
            specs[0].container_path
        );
    }

    #[test]
    fn resolve_user_overlay_rejects_relative_container_path() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = make_engine(tmp.path());
        let spec = DirectorySpec {
            host: "/h".into(),
            container: "rel/path".into(),
            permission: OverlayPermission::ReadOnly,
        };
        let err = engine
            .resolve_user_overlay(&spec, Path::new("/"), None)
            .unwrap_err();
        assert!(matches!(err, EngineError::Other(_)));
    }

    #[test]
    fn agent_settings_synthesized_when_no_files_present() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let out = engine.agent_settings_overlays(&agent, tmp.path()).unwrap();
        assert!(
            out.iter().any(|o| o
                .container_path
                .to_string_lossy()
                .ends_with("/.claude.json")),
            "expected synthesized .claude.json overlay for first-time user, got {out:?}"
        );
    }

    #[test]
    fn agent_settings_overlays_claude_config_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        // Create ~/.claude.json so the overlay resolver picks it up.
        let config_file = tmp.path().join(".claude.json");
        std::fs::write(&config_file, r#"{"model":"claude-sonnet-4-6"}"#).unwrap();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let overlays = engine.agent_settings_overlays(&agent, tmp.path()).unwrap();
        // The overlay engine sanitizes the .claude.json file (strips
        // oauthAccount) and writes it to a temp path; we expect at least one
        // overlay mounting a file as `/root/.claude.json`.
        assert!(
            overlays.iter().any(|o| o
                .container_path
                .to_string_lossy()
                .ends_with("/.claude.json")),
            "expected overlay targeting /root/.claude.json, got {overlays:?}"
        );
    }

    #[test]
    fn build_overlays_deduplicates_overlapping_host_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let host_dir = tmp.path().join("shared");
        std::fs::create_dir_all(&host_dir).unwrap();
        let engine = make_engine(tmp.path());
        // Fake a session — overlay engine doesn't use it in this path.
        let session_tmp = tempfile::tempdir().unwrap();
        let session = {
            use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};
            let resolver = StaticGitRootResolver::new(session_tmp.path());
            crate::data::session::Session::open(
                session_tmp.path().to_path_buf(),
                &resolver,
                SessionOpenOptions::default(),
            )
            .unwrap()
        };
        let request = OverlayRequest {
            directories: vec![
                DirectorySpec {
                    host: host_dir.to_str().unwrap().to_string(),
                    container: "/app/data".into(),
                    permission: OverlayPermission::ReadWrite,
                },
                DirectorySpec {
                    host: host_dir.to_str().unwrap().to_string(),
                    container: "/app/data".into(),
                    permission: OverlayPermission::ReadOnly,
                },
            ],
            include_all_skills: false,
            named_skills: vec![],
            agent: None,
            yolo: false,
            container_home: None,
            context_overlays: vec![],
        };
        let overlays = engine.build_overlays(&session, &request).unwrap();
        // The two entries sharing the same canonicalized host path must collapse.
        let matches: Vec<_> = overlays
            .iter()
            .filter(|o| o.host_path == host_dir.canonicalize().unwrap_or(host_dir.clone()))
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "duplicate host path must be deduplicated, got {overlays:?}"
        );
    }

    #[test]
    fn resolve_user_overlay_rejects_missing_container_path() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = make_engine(tmp.path());
        let spec = DirectorySpec {
            host: tmp.path().to_str().unwrap().to_string(),
            container: "relative/path".into(),
            permission: OverlayPermission::ReadOnly,
        };
        assert!(engine
            .resolve_user_overlay(&spec, Path::new("/"), None)
            .is_err());
    }

    #[test]
    fn sanitize_claude_config_strips_oauth_account() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join(".claude.json");
        std::fs::write(
            &config_file,
            r#"{"model":"claude-sonnet-4-6","oauthAccount":{"token":"secret"}}"#,
        )
        .unwrap();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let overlays = engine.agent_settings_overlays(&agent, tmp.path()).unwrap();
        // One overlay for the config file.
        let config_overlay = overlays
            .iter()
            .find(|o| {
                o.container_path
                    .to_string_lossy()
                    .ends_with("/.claude.json")
            })
            .expect("must have .claude.json overlay");
        // The sanitized file must not contain oauthAccount.
        let sanitized = std::fs::read_to_string(&config_overlay.host_path).unwrap();
        assert!(
            !sanitized.contains("oauthAccount"),
            "oauthAccount must be stripped from sanitized config: {sanitized}"
        );
        assert!(
            sanitized.contains("claude-sonnet-4-6"),
            "model field must be preserved: {sanitized}"
        );
    }

    #[test]
    fn sanitize_claude_config_injects_workspace_trust_dialog_accepted() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join(".claude.json");
        std::fs::write(&config_file, r#"{"model":"claude-sonnet-4-6"}"#).unwrap();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let overlays = engine.agent_settings_overlays(&agent, tmp.path()).unwrap();
        let config_overlay = overlays
            .iter()
            .find(|o| {
                o.container_path
                    .to_string_lossy()
                    .ends_with("/.claude.json")
            })
            .expect("must have .claude.json overlay");
        let sanitized = std::fs::read_to_string(&config_overlay.host_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&sanitized).unwrap();
        assert_eq!(
            parsed["projects"]["/workspace"]["hasTrustDialogAccepted"],
            serde_json::Value::Bool(true),
            "trust dialog must be accepted for /workspace: {sanitized}"
        );
    }

    #[test]
    fn sanitize_claude_settings_dir_filters_denylist_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        // Create a denylisted entry.
        std::fs::create_dir_all(claude_dir.join("projects")).unwrap();
        // Create an allowed entry.
        std::fs::write(claude_dir.join("allowed.json"), r#"{"foo":"bar"}"#).unwrap();

        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let overlays = engine.agent_settings_overlays(&agent, tmp.path()).unwrap();
        let dir_overlay = overlays
            .iter()
            .find(|o| o.container_path.to_string_lossy().ends_with("/.claude"))
            .expect("must have .claude dir overlay");

        let sanitized_root = &dir_overlay.host_path;
        assert!(
            !sanitized_root.join("projects").exists(),
            "denylisted 'projects' dir must be excluded from sanitized overlay"
        );
        assert!(
            sanitized_root.join("allowed.json").exists(),
            "allowed file must be present in sanitized overlay"
        );
    }

    #[test]
    fn sanitize_claude_settings_dir_suppresses_lsp_banner() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();

        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let overlays = engine.agent_settings_overlays(&agent, tmp.path()).unwrap();
        let dir_overlay = overlays
            .iter()
            .find(|o| o.container_path.to_string_lossy().ends_with("/.claude"))
            .expect("must have .claude dir overlay");

        let settings_path = dir_overlay.host_path.join("settings.json");
        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(
            settings["lspRecommendationDismissed"],
            serde_json::Value::Bool(true),
            "lspRecommendationDismissed must be true in sanitized settings"
        );
    }

    #[test]
    fn sanitize_claude_settings_dir_injects_yolo_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();

        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let overlays = engine
            .agent_settings_overlays_with(&agent, true, tmp.path(), None)
            .unwrap();
        let dir_overlay = overlays
            .iter()
            .find(|o| o.container_path.to_string_lossy().ends_with("/.claude"))
            .expect("must have .claude dir overlay");

        let settings_path = dir_overlay.host_path.join("settings.json");
        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(
            settings["permissionMode"],
            serde_json::Value::String("bypassPermissions".into()),
            "permissionMode must be bypassPermissions when yolo=true"
        );
    }

    #[test]
    fn detect_container_home_finds_user_directive() {
        let tmp = tempfile::tempdir().unwrap();
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join("Dockerfile.claude"),
            "FROM ubuntu:22.04\nRUN apt-get update\nUSER appuser\nWORKDIR /home/appuser\n",
        )
        .unwrap();

        let result = detect_container_home(tmp.path(), "claude", tmp.path());

        assert_eq!(
            result,
            Some("/home/appuser".to_string()),
            "detect_container_home must return /home/appuser for USER appuser"
        );
    }

    #[test]
    fn detect_container_home_returns_none_when_no_dockerfile() {
        let tmp = tempfile::tempdir().unwrap();
        let result = detect_container_home(tmp.path(), "claude", tmp.path());
        assert!(
            result.is_none(),
            "detect_container_home must return None when no Dockerfile found"
        );
    }

    #[test]
    fn detect_container_home_returns_none_for_root_user() {
        let tmp = tempfile::tempdir().unwrap();
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join("Dockerfile.claude"),
            "FROM ubuntu:22.04\nUSER root\n",
        )
        .unwrap();

        let result = detect_container_home(tmp.path(), "claude", tmp.path());

        assert!(
            result.is_none(),
            "detect_container_home must return None when USER is root"
        );
    }

    #[test]
    fn detect_container_home_returns_none_for_user_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join("Dockerfile.claude"),
            "FROM ubuntu:22.04\nUSER 0\n",
        )
        .unwrap();

        let result = detect_container_home(tmp.path(), "claude", tmp.path());

        assert!(
            result.is_none(),
            "detect_container_home must return None when USER is 0"
        );
    }

    // ─── detect_home_from_dockerfile ──────────────────────────────────────────

    #[test]
    fn detect_home_from_dockerfile_finds_non_root_user() {
        let tmp = tempfile::tempdir().unwrap();
        let df = tmp.path().join("Dockerfile.dev");
        std::fs::write(
            &df,
            "FROM debian:bookworm\nUSER awman\nWORKDIR /workspace\n",
        )
        .unwrap();
        assert_eq!(
            detect_home_from_dockerfile(&df),
            Some("/home/awman".to_string()),
        );
    }

    #[test]
    fn detect_home_from_dockerfile_returns_none_for_root() {
        let tmp = tempfile::tempdir().unwrap();
        let df = tmp.path().join("Dockerfile.dev");
        std::fs::write(&df, "FROM debian:bookworm\nUSER root\n").unwrap();
        assert!(detect_home_from_dockerfile(&df).is_none());
    }

    #[test]
    fn detect_home_from_dockerfile_returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(detect_home_from_dockerfile(&tmp.path().join("nonexistent")).is_none());
    }

    #[test]
    fn detect_home_from_dockerfile_uses_last_non_root_user() {
        let tmp = tempfile::tempdir().unwrap();
        let df = tmp.path().join("Dockerfile");
        std::fs::write(&df, "FROM debian\nUSER builder\nRUN make\nUSER runner\n").unwrap();
        assert_eq!(
            detect_home_from_dockerfile(&df),
            Some("/home/runner".to_string()),
        );
    }

    #[test]
    fn detect_home_from_dockerfile_resets_on_root_switch() {
        let tmp = tempfile::tempdir().unwrap();
        let df = tmp.path().join("Dockerfile");
        std::fs::write(&df, "FROM debian\nUSER builder\nRUN make\nUSER root\n").unwrap();
        assert!(detect_home_from_dockerfile(&df).is_none());
    }

    // ─── resolve_user_overlay missing-host fail-fast ─────────────────────────

    #[test]
    fn resolve_user_overlay_errors_when_host_path_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("no-such-dir");
        let engine = make_engine(tmp.path());

        let spec = DirectorySpec {
            host: missing.to_str().unwrap().to_string(),
            container: "/workspace/data".into(),
            permission: OverlayPermission::ReadOnly,
        };

        let err = engine
            .resolve_user_overlay(&spec, Path::new("/"), None)
            .expect_err("missing host path must surface an EngineError");
        let msg = err.to_string();
        assert!(
            msg.contains("does not exist"),
            "error must say the host path doesn't exist; got: {msg}"
        );
        assert!(
            msg.contains("no-such-dir"),
            "error must name the offending host path; got: {msg}"
        );
    }

    #[test]
    fn resolve_user_overlay_errors_when_ssh_dir_missing() {
        // The realistic `ssh()` case: ~/.ssh doesn't exist on the host.
        let tmp = tempfile::tempdir().unwrap();
        let ssh_dir = tmp.path().join(".ssh"); // deliberately not created
        let engine = make_engine(tmp.path());

        let spec = DirectorySpec {
            host: ssh_dir.to_str().unwrap().to_string(),
            container: "~/.ssh".into(),
            permission: OverlayPermission::ReadOnly,
        };

        let err = engine
            .resolve_user_overlay(&spec, Path::new("/"), None)
            .expect_err("missing ~/.ssh must surface an EngineError");
        assert!(
            err.to_string().contains("does not exist"),
            "ssh() with missing ~/.ssh must fail fast; got: {err}"
        );
    }

    // ─── resolve_user_overlay tilde expansion ────────────────────────────────

    #[test]
    fn resolve_user_overlay_expands_tilde_with_container_home() {
        let tmp = tempfile::tempdir().unwrap();
        let ssh_dir = tmp.path().join(".ssh");
        std::fs::create_dir_all(&ssh_dir).unwrap();
        let engine = make_engine(tmp.path());

        let spec = DirectorySpec {
            host: ssh_dir.to_str().unwrap().to_string(),
            container: "~/.ssh".to_string(),
            permission: OverlayPermission::ReadOnly,
        };

        let result = engine
            .resolve_user_overlay(&spec, Path::new("/"), Some("/home/alice"))
            .unwrap();
        assert_eq!(
            result.container_path,
            std::path::PathBuf::from("/home/alice/.ssh"),
            "~/.ssh must expand to /home/alice/.ssh when container_home is /home/alice"
        );
    }

    #[test]
    fn resolve_user_overlay_expands_tilde_without_container_home_defaults_to_root() {
        let tmp = tempfile::tempdir().unwrap();
        let ssh_dir = tmp.path().join(".ssh");
        std::fs::create_dir_all(&ssh_dir).unwrap();
        let engine = make_engine(tmp.path());

        let spec = DirectorySpec {
            host: ssh_dir.to_str().unwrap().to_string(),
            container: "~/.ssh".to_string(),
            permission: OverlayPermission::ReadOnly,
        };

        let result = engine
            .resolve_user_overlay(&spec, Path::new("/"), None)
            .unwrap();
        assert_eq!(
            result.container_path,
            std::path::PathBuf::from("/root/.ssh"),
            "~/.ssh must default to /root/.ssh when container_home is None"
        );
    }

    // ─── skill_overlays: named skills ─────────────────────────────────────────

    #[test]
    fn skill_overlays_named_only_emits_that_skill() {
        let (tmp, _) = make_home_with_skills();
        // Create a named skill directory inside the global skills dir.
        let lint_dir = tmp.path().join("skills").join("lint");
        std::fs::create_dir_all(&lint_dir).unwrap();

        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();

        let specs = with_awman_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, false, &["lint".to_string()], &None, Path::new("/"))
                .unwrap()
        });

        assert_eq!(
            specs.len(),
            1,
            "only the named skill must be emitted; got {specs:?}"
        );
        assert!(
            specs[0].container_path.to_string_lossy().ends_with("/lint"),
            "container path must include the skill name 'lint'; got {:?}",
            specs[0].container_path
        );
        assert_eq!(
            specs[0].permission,
            OverlayPermission::ReadOnly,
            "named skill must be mounted read-only"
        );
    }

    #[test]
    fn skill_overlays_nonexistent_named_skill_returns_engine_error() {
        let (tmp, _) = make_home_with_skills();
        // Deliberately do NOT create a "nonexistent" skill directory.
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();

        let result = with_awman_config_home(tmp.path(), || {
            engine.skill_overlays(
                &agent,
                false,
                &["nonexistent".to_string()],
                &None,
                Path::new("/"),
            )
        });

        assert!(
            result.is_err(),
            "nonexistent named skill must return EngineError"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("nonexistent"),
            "error must name the missing skill; got: {msg}"
        );
    }

    // ─── skill_overlays: pulled libraries (WI-0103) ──────────────────────────

    /// Seed a pulled library at `<home>/skills/.library/<slug>/` with the given
    /// `subdir` and skill names (each a `<skill>/SKILL.md`), plus `.awman.json`.
    fn seed_library(home: &Path, slug: &str, subdir: &str, skills: &[&str]) {
        let lib_dir = home.join("skills").join(".library").join(slug);
        for skill in skills {
            let skill_dir = lib_dir.join(subdir).join(skill);
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(skill_dir.join("SKILL.md"), format!("# {skill}")).unwrap();
        }
        crate::data::fs::skill_library::write_library_meta(
            &lib_dir,
            &crate::data::fs::skill_library::SkillLibraryMeta {
                source: format!("https://github.com/someone/{slug}.git"),
                owner: "someone".to_string(),
                repo: slug.to_string(),
                subdir: subdir.to_string(),
            },
        )
        .unwrap();
    }

    #[test]
    fn skill_named_plain_skill_wins_over_same_named_library() {
        let (tmp, _) = make_home_with_skills();
        // A hand-authored plain skill named 'superpowers'.
        let plain = tmp.path().join("skills").join("superpowers");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("SKILL.md"), "# plain").unwrap();
        // A pulled library ALSO named 'superpowers'.
        seed_library(tmp.path(), "superpowers", "skills", &["brainstorming"]);

        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let specs = with_awman_config_home(tmp.path(), || {
            engine
                .skill_overlays(
                    &agent,
                    false,
                    &["superpowers".to_string()],
                    &None,
                    Path::new("/"),
                )
                .unwrap()
        });

        assert_eq!(specs.len(), 1);
        assert_eq!(
            specs[0].host_path,
            std::fs::canonicalize(&plain).unwrap(),
            "the plain skill must win over a same-named pulled library"
        );
    }

    #[test]
    fn skill_named_whole_library_mounts_subdir_at_library_container_path() {
        let (tmp, _) = make_home_with_skills();
        seed_library(
            tmp.path(),
            "superpowers",
            "skills",
            &["brainstorming", "debugging"],
        );

        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let specs = with_awman_config_home(tmp.path(), || {
            engine
                .skill_overlays(
                    &agent,
                    false,
                    &["superpowers".to_string()],
                    &None,
                    Path::new("/"),
                )
                .unwrap()
        });

        assert_eq!(specs.len(), 1);
        let expected_host = std::fs::canonicalize(
            tmp.path()
                .join("skills")
                .join(".library")
                .join("superpowers")
                .join("skills"),
        )
        .unwrap();
        assert_eq!(
            specs[0].host_path, expected_host,
            "whole-library mount must point at .library/<slug>/<subdir>"
        );
        assert!(
            specs[0]
                .container_path
                .to_string_lossy()
                .ends_with("/superpowers"),
            "container path must namespace the whole library under its name; got {:?}",
            specs[0].container_path
        );
    }

    #[test]
    fn skill_named_single_library_skill_mounts_that_skill_dir() {
        let (tmp, _) = make_home_with_skills();
        seed_library(tmp.path(), "superpowers", "skills", &["brainstorming"]);

        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let specs = with_awman_config_home(tmp.path(), || {
            engine
                .skill_overlays(
                    &agent,
                    false,
                    &["superpowers/brainstorming".to_string()],
                    &None,
                    Path::new("/"),
                )
                .unwrap()
        });

        assert_eq!(specs.len(), 1);
        let expected_host = std::fs::canonicalize(
            tmp.path()
                .join("skills")
                .join(".library")
                .join("superpowers")
                .join("skills")
                .join("brainstorming"),
        )
        .unwrap();
        assert_eq!(
            specs[0].host_path, expected_host,
            "single-skill mount must point at the individual skill directory"
        );
        assert!(
            specs[0]
                .container_path
                .to_string_lossy()
                .ends_with("/superpowers/brainstorming"),
            "container path must preserve the library namespace; got {:?}",
            specs[0].container_path
        );
    }

    #[test]
    fn skill_named_library_present_but_skill_missing_gives_distinct_error() {
        let (tmp, _) = make_home_with_skills();
        seed_library(tmp.path(), "superpowers", "skills", &["brainstorming"]);

        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let result = with_awman_config_home(tmp.path(), || {
            engine.skill_overlays(
                &agent,
                false,
                &["superpowers/ghost".to_string()],
                &None,
                Path::new("/"),
            )
        });

        let msg = result
            .expect_err("a missing skill in a present library must error")
            .to_string();
        assert!(
            msg.contains("not found in library")
                && msg.contains("superpowers")
                && msg.contains("ghost"),
            "error must name both the library and the missing skill; got: {msg}"
        );
    }

    /// A skill is a directory holding a `SKILL.md`. An arbitrary directory
    /// inside a library's subdir must not be mountable just because it exists
    /// (WI-0103 remediation).
    #[test]
    fn skill_named_library_dir_without_skill_md_is_rejected() {
        let (tmp, _) = make_home_with_skills();
        seed_library(tmp.path(), "superpowers", "skills", &["brainstorming"]);
        // A directory inside the library's subdir with no SKILL.md.
        let not_a_skill = tmp
            .path()
            .join("skills")
            .join(".library")
            .join("superpowers")
            .join("skills")
            .join("not-a-skill");
        std::fs::create_dir_all(&not_a_skill).unwrap();
        std::fs::write(not_a_skill.join("README.md"), "no skill here").unwrap();

        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let result = with_awman_config_home(tmp.path(), || {
            engine.skill_overlays(
                &agent,
                false,
                &["superpowers/not-a-skill".to_string()],
                &None,
                Path::new("/"),
            )
        });

        let msg = result
            .expect_err("a directory without SKILL.md is not a skill")
            .to_string();
        assert!(
            msg.contains("not-a-skill") && msg.contains("superpowers") && msg.contains("SKILL.md"),
            "error must name the library, the missing skill, and SKILL.md; got: {msg}"
        );
    }

    /// The parser rejects traversal segments, but named skills also arrive from
    /// config files and the API, so `skill_overlays` re-checks containment
    /// rather than joining `..` onto a host path (WI-0103 remediation).
    #[test]
    fn skill_named_traversal_segments_are_rejected_by_the_engine() {
        let (tmp, _) = make_home_with_skills();
        seed_library(tmp.path(), "superpowers", "skills", &["brainstorming"]);

        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        for bad in ["superpowers/..", "superpowers/", "..", "../superpowers"] {
            let result = with_awman_config_home(tmp.path(), || {
                engine.skill_overlays(&agent, false, &[bad.to_string()], &None, Path::new("/"))
            });
            let msg = match result {
                Ok(specs) => panic!("'{bad}' must be rejected, but produced specs: {specs:?}"),
                Err(e) => e.to_string(),
            };
            assert!(
                msg.contains("invalid path segment"),
                "'{bad}' must be rejected as an invalid segment; got: {msg}"
            );
        }
    }

    #[test]
    fn skill_named_neither_plain_nor_library_names_both_locations() {
        let (tmp, _) = make_home_with_skills();
        // Neither a plain skill nor a library called 'ghost' exists.
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let result = with_awman_config_home(tmp.path(), || {
            engine.skill_overlays(&agent, false, &["ghost".to_string()], &None, Path::new("/"))
        });

        let msg = result
            .expect_err("an unresolvable name must error")
            .to_string();
        assert!(
            msg.contains("ghost"),
            "error must name the skill; got: {msg}"
        );
        assert!(
            msg.contains(&tmp.path().join("skills").display().to_string()),
            "error must name the global skills dir; got: {msg}"
        );
        assert!(
            msg.contains(".library"),
            "error must name the .library location; got: {msg}"
        );
    }

    #[test]
    fn skill_star_is_identical_with_and_without_populated_library() {
        let (tmp, skills_canon) = make_home_with_skills();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();

        let before = with_awman_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, true, &[], &None, Path::new("/"))
                .unwrap()
        });

        // Populate `.library/` — skill(*) must be entirely unaffected by it.
        seed_library(tmp.path(), "superpowers", "skills", &["brainstorming"]);

        let after = with_awman_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, true, &[], &None, Path::new("/"))
                .unwrap()
        });

        assert_eq!(
            before, after,
            "skill(*) must emit an identical OverlaySpec list regardless of .library/"
        );
        assert_eq!(before.len(), 1, "skill(*) is a single mount");
        assert_eq!(
            before[0].host_path, skills_canon,
            "skill(*) still mounts the global skills dir as-is"
        );
    }

    // ─── build_overlays: least-permissive-wins ────────────────────────────────

    #[test]
    fn build_overlays_least_permissive_wins_for_same_host_path() {
        let tmp = tempfile::tempdir().unwrap();
        let host_dir = tmp.path().join("shared");
        std::fs::create_dir_all(&host_dir).unwrap();
        let engine = make_engine(tmp.path());

        let session_tmp = tempfile::tempdir().unwrap();
        let session = {
            use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};
            let resolver = StaticGitRootResolver::new(session_tmp.path());
            crate::data::session::Session::open(
                session_tmp.path().to_path_buf(),
                &resolver,
                SessionOpenOptions::default(),
            )
            .unwrap()
        };

        let request = OverlayRequest {
            directories: vec![
                DirectorySpec {
                    host: host_dir.to_str().unwrap().to_string(),
                    container: "/app/data".into(),
                    permission: OverlayPermission::ReadOnly,
                },
                DirectorySpec {
                    host: host_dir.to_str().unwrap().to_string(),
                    container: "/app/data".into(),
                    permission: OverlayPermission::ReadWrite,
                },
            ],
            include_all_skills: false,
            named_skills: vec![],
            agent: None,
            yolo: false,
            container_home: None,
            context_overlays: vec![],
        };

        let overlays = engine.build_overlays(&session, &request).unwrap();
        let host_canon = host_dir.canonicalize().unwrap_or_else(|_| host_dir.clone());
        let matched: Vec<_> = overlays
            .iter()
            .filter(|o| o.host_path == host_canon)
            .collect();
        assert_eq!(
            matched.len(),
            1,
            "same host path must deduplicate; got {overlays:?}"
        );
        assert_eq!(
            matched[0].permission,
            OverlayPermission::ReadOnly,
            "ReadOnly must win over ReadWrite (least-permissive-wins); got {:?}",
            matched[0].permission
        );
    }

    #[test]
    fn sanitize_claude_settings_dir_no_yolo_when_false() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();

        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let overlays = engine
            .agent_settings_overlays_with(&agent, false, tmp.path(), None)
            .unwrap();
        let dir_overlay = overlays
            .iter()
            .find(|o| o.container_path.to_string_lossy().ends_with("/.claude"))
            .expect("must have .claude dir overlay");

        let settings_path = dir_overlay.host_path.join("settings.json");
        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(
            settings.get("permissionMode").is_none(),
            "permissionMode must NOT be set when yolo=false"
        );
    }

    // ─── WI-0087: context overlay mounts ──────────────────────────────────────

    fn make_session(session_root: &std::path::Path) -> crate::data::session::Session {
        use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};
        let resolver = StaticGitRootResolver::new(session_root);
        crate::data::session::Session::open(
            session_root.to_path_buf(),
            &resolver,
            SessionOpenOptions::default(),
        )
        .unwrap()
    }

    #[test]
    fn build_overlays_context_overlay_produces_expected_container_path() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = make_engine(tmp.path());
        let session_tmp = tempfile::tempdir().unwrap();
        let session = make_session(session_tmp.path());

        // Host path for the context dir (doesn't need to exist for context overlays).
        let ctx_host = tmp.path().join("context").join("global");

        let request = OverlayRequest {
            context_overlays: vec![ContextOverlay {
                scope: ContextScope::Global,
                host_path: ctx_host,
                container_path: std::path::PathBuf::from("/awman/context/global"),
                permission: crate::engine::container::options::OverlayPermission::ReadWrite,
            }],
            ..Default::default()
        };

        let specs = engine.build_overlays(&session, &request).unwrap();
        let ctx_spec = specs
            .iter()
            .find(|s| s.container_path == std::path::Path::new("/awman/context/global"));
        assert!(
            ctx_spec.is_some(),
            "build_overlays must produce an OverlaySpec with container path \
             /awman/context/global; got {specs:?}"
        );
        assert_eq!(
            ctx_spec.unwrap().permission,
            crate::engine::container::options::OverlayPermission::ReadWrite,
        );
    }

    #[test]
    fn build_overlays_context_overlay_repo_scope_container_path() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = make_engine(tmp.path());
        let session_tmp = tempfile::tempdir().unwrap();
        let session = make_session(session_tmp.path());

        let ctx_host = tmp
            .path()
            .join("context")
            .join("repo")
            .join("org")
            .join("myrepo");

        let request = OverlayRequest {
            context_overlays: vec![ContextOverlay {
                scope: ContextScope::Repo,
                host_path: ctx_host,
                container_path: std::path::PathBuf::from("/awman/context/repo"),
                permission: crate::engine::container::options::OverlayPermission::ReadOnly,
            }],
            ..Default::default()
        };

        let specs = engine.build_overlays(&session, &request).unwrap();
        let ctx_spec = specs
            .iter()
            .find(|s| s.container_path == std::path::Path::new("/awman/context/repo"));
        assert!(
            ctx_spec.is_some(),
            "build_overlays must produce an OverlaySpec with container path \
             /awman/context/repo; got {specs:?}"
        );
        assert_eq!(
            ctx_spec.unwrap().permission,
            crate::engine::container::options::OverlayPermission::ReadOnly,
        );
    }

    #[test]
    fn build_overlays_context_overlay_collides_with_user_dir_most_restrictive_wins() {
        // A context overlay (ReadOnly) sharing a host path with a user dir(ReadWrite)
        // must merge to ReadOnly.
        let tmp = tempfile::tempdir().unwrap();
        let engine = make_engine(tmp.path());
        let session_tmp = tempfile::tempdir().unwrap();
        let session = make_session(session_tmp.path());

        // Shared host directory (must exist for the user dir overlay path check).
        let shared_host = tmp.path().join("shared");
        std::fs::create_dir_all(&shared_host).unwrap();
        let shared_host_str = shared_host.to_str().unwrap().to_string();

        let request = OverlayRequest {
            directories: vec![DirectorySpec {
                host: shared_host_str,
                container: "/app/data".to_string(),
                permission: crate::engine::container::options::OverlayPermission::ReadWrite,
            }],
            context_overlays: vec![ContextOverlay {
                scope: ContextScope::Global,
                host_path: shared_host.clone(),
                container_path: std::path::PathBuf::from("/awman/context/global"),
                permission: crate::engine::container::options::OverlayPermission::ReadOnly,
            }],
            ..Default::default()
        };

        let specs = engine.build_overlays(&session, &request).unwrap();

        // Both map to the same canonicalized host path, so they must merge to one entry.
        let shared_canon = shared_host
            .canonicalize()
            .unwrap_or_else(|_| shared_host.clone());
        let matching: Vec<_> = specs
            .iter()
            .filter(|s| s.host_path == shared_canon)
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "user dir + context overlay with same host path must merge to one entry; \
             got {specs:?}"
        );
        assert_eq!(
            matching[0].permission,
            crate::engine::container::options::OverlayPermission::ReadOnly,
            "ReadOnly must win over ReadWrite (most-restrictive); got {:?}",
            matching[0].permission
        );
    }
}
