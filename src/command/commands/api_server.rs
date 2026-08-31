//! `ApiServerCommand` — `api start | kill | logs | status`.

use async_trait::async_trait;
use serde::Serialize;

use std::net::IpAddr;
use std::path::PathBuf;

use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::config::env::Env;
use crate::data::fs::daemon_guard::{AcquireError, DaemonGuard, DaemonKind};
use crate::data::fs::daemon_process::{
    self, API_PLIST_LABEL, API_UNIT_NAME, DaemonProcess, ServerMeta, Termination,
};
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::engine::auth::TlsMaterial;

/// Build the API daemon's process handle from its paths.
fn api_daemon(api_paths: &crate::data::fs::ApiPaths) -> DaemonProcess {
    DaemonProcess::new(api_paths.daemon(), API_UNIT_NAME, API_PLIST_LABEL)
}

/// Configuration handed from the `api start` command to Layer 3's
/// `serve_until_shutdown`. Lives in Layer 2 so the trait signature does
/// not pull Layer 3 types into the command layer.
#[derive(Debug, Clone)]
pub struct ApiServeConfig {
    pub port: u16,
    pub bind_ip: IpAddr,
    pub workdirs: Vec<PathBuf>,
    pub dangerously_skip_auth: bool,
    /// `None` means TLS is disabled (plain HTTP); only set when the user
    /// explicitly passed `--dangerously-skip-tls`.
    pub tls_material: Option<TlsMaterial>,
}

pub mod banner;

#[derive(Debug, Clone)]
pub struct ApiServerStartFlags {
    pub port: u16,
    pub workdirs: Vec<String>,
    pub background: bool,
    pub refresh_key: bool,
    pub dangerously_skip_auth: bool,
    pub dangerously_skip_tls: bool,
}

#[derive(Debug, Clone)]
pub struct ApiServerKillFlags {}

#[derive(Debug, Clone)]
pub struct ApiServerLogsFlags {}

#[derive(Debug, Clone)]
pub struct ApiServerStatusFlags {}

#[derive(Debug, Clone)]
pub enum ApiServerSubcommand {
    Start(ApiServerStartFlags),
    Kill(ApiServerKillFlags),
    Logs(ApiServerLogsFlags),
    Status(ApiServerStatusFlags),
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiServerStartOutcome {
    pub port: u16,
    pub background: bool,
    pub workdirs: Vec<String>,
    pub refreshed_key: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiServerKillOutcome {
    pub stopped_pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiServerLogsOutcome {
    pub log_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiServerStatusOutcome {
    pub running: bool,
    pub pid: Option<u32>,
    /// Bound endpoint (e.g. `http://127.0.0.1:9876` or `https://...`),
    /// populated when the meta sidecar is present.
    pub bound_addr: Option<String>,
    /// Server version reported by `GET /v1/status`, when reachable.
    pub version: Option<String>,
    /// Whether the HTTP probe succeeded. `false` when the PID is alive but
    /// the server didn't respond — surfaces hung-server cases.
    pub responsive: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum ApiServerOutcome {
    Start(ApiServerStartOutcome),
    Kill(ApiServerKillOutcome),
    Logs(ApiServerLogsOutcome),
    Status(ApiServerStatusOutcome),
}

/// Methods Layer 3 must provide to the api start command.
#[async_trait]
pub trait ApiServerStartCommandFrontend: UserMessageSink + Send + Sync {
    async fn serve_until_shutdown(&mut self, config: ApiServeConfig) -> Result<(), CommandError>;
}

pub trait ApiServerKillCommandFrontend: UserMessageSink + Send + Sync {}
pub trait ApiServerLogsCommandFrontend: UserMessageSink + Send + Sync {}
pub trait ApiServerStatusCommandFrontend: UserMessageSink + Send + Sync {}

/// Catch-all frontend for the umbrella `ApiServerCommand`. Includes
/// `serve_until_shutdown` so the dispatched frontend can boot the server.
#[async_trait]
pub trait ApiServerCommandFrontend: UserMessageSink + Send + Sync {
    async fn serve_until_shutdown(&mut self, config: ApiServeConfig) -> Result<(), CommandError>;
}

pub struct ApiServerCommand {
    sub: ApiServerSubcommand,
    engines: Engines,
}

impl ApiServerCommand {
    pub fn new(sub: ApiServerSubcommand, engines: Engines) -> Self {
        Self { sub, engines }
    }

    pub fn subcommand(&self) -> &ApiServerSubcommand {
        &self.sub
    }
}

#[async_trait]
impl Command for ApiServerCommand {
    type Frontend = Box<dyn ApiServerCommandFrontend>;
    type Outcome = ApiServerOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let api_paths = self.engines.auth_engine.api_paths();
        api_paths.ensure_root().map_err(CommandError::Data)?;

        let outcome = match self.sub {
            ApiServerSubcommand::Start(f) => {
                run_start(f, &self.engines, &mut *frontend, api_paths).await?
            }
            ApiServerSubcommand::Kill(_) => run_kill(api_paths, &mut *frontend)?,
            ApiServerSubcommand::Logs(_) => run_logs(api_paths, &mut *frontend)?,
            ApiServerSubcommand::Status(_) => run_status(api_paths).await?,
        };
        frontend.replay_queued();
        Ok(outcome)
    }
}

async fn run_start(
    flags: ApiServerStartFlags,
    engines: &Engines,
    frontend: &mut dyn ApiServerCommandFrontend,
    api_paths: &crate::data::fs::ApiPaths,
) -> Result<ApiServerOutcome, CommandError> {
    let daemon = api_daemon(api_paths);

    // Check if already running.
    if let Some(pid) = daemon.running_pid()? {
        return Err(CommandError::ApiServerAlreadyRunning { pid });
    }

    // Cross-daemon guard — the actual `check()`s happen at the two points that
    // truly start a server (background spawn, foreground claim), so
    // `--refresh-key` and other non-serving early returns are unaffected.
    // Built from the *supplied* `api_paths` so the pidfile the guard claims is
    // the same one `run_kill`/`run_status` read.
    let guard = DaemonGuard::with_paths(
        DaemonKind::Api,
        api_paths,
        &crate::data::fs::AmiePaths::from_env(&Env::from_process()).map_err(CommandError::Data)?,
    );

    // Resolve workdirs by merging CLI --workdirs with the global API config.
    let config_workdirs: Vec<String> = crate::data::config::global::GlobalConfig::load()
        .unwrap_or_default()
        .api
        .as_ref()
        .and_then(|h| h.work_dirs.clone())
        .unwrap_or_default();
    let workdirs = resolve_workdirs(&flags.workdirs, &config_workdirs)?;

    // --refresh-key: generate new key, print banner, exit.
    if flags.refresh_key {
        let key = engines.auth_engine.refresh_api_key()?;
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: banner::render_api_key_banner(key.as_str()),
        });
        return Ok(ApiServerOutcome::Start(ApiServerStartOutcome {
            port: flags.port,
            background: false,
            workdirs: workdirs.iter().map(|p| p.display().to_string()).collect(),
            refreshed_key: true,
        }));
    }

    // First-run convenience: if auth is required but no key hash exists on
    // disk, generate one now and display the banner instead of forcing the
    // user to re-run with `--refresh-key`.
    let mut auto_generated_key = false;
    if !flags.dangerously_skip_auth && engines.auth_engine.read_api_key_hash()?.is_none() {
        let key = engines.auth_engine.refresh_api_key()?;
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text:
                "No API key configured — generating one now (store it; it will not be shown again):"
                    .to_string(),
        });
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: banner::render_api_key_banner(key.as_str()),
        });
        auto_generated_key = true;
    }

    if flags.dangerously_skip_auth {
        frontend.write_message(UserMessage {
            level: MessageLevel::Warning,
            text:
                "--dangerously-skip-auth set — API endpoints will accept unauthenticated requests."
                    .to_string(),
        });
    }

    let workdir_strings: Vec<String> = workdirs.iter().map(|p| p.display().to_string()).collect();

    // Background mode: spawn a child process and exit.
    if flags.background {
        // Refuse to start if the amie daemon is running.
        guard.check().map_err(CommandError::Data)?;
        let binary = std::env::current_exe()
            .map_err(|e| CommandError::Other(format!("cannot determine awman binary: {e}")))?;
        let mut args = vec![
            "api".to_string(),
            "start".to_string(),
            "--port".to_string(),
            flags.port.to_string(),
        ];
        if flags.dangerously_skip_auth {
            args.push("--dangerously-skip-auth".to_string());
        }
        if flags.dangerously_skip_tls {
            args.push("--dangerously-skip-tls".to_string());
        }
        for w in &flags.workdirs {
            args.push("--workdirs".to_string());
            args.push(w.clone());
        }

        let child_pid = daemon.spawn_detached(&binary, &args)?;
        if child_pid > 0 {
            // Use exclusive write so a racing parallel `api start --background`
            // can't trample the PID we just spawned.
            if !daemon.claim_pidfile(child_pid)? {
                if let Some(existing) = daemon.read_pid()? {
                    if existing != child_pid
                        && daemon_process::is_process_alive(existing)
                        && daemon_process::pid_is_awman(existing)
                    {
                        return Err(CommandError::ApiServerAlreadyRunning { pid: existing });
                    }
                }
                // Stale or matching — overwrite.
                daemon.force_write_pidfile(child_pid)?;
            }
        }

        frontend.write_message(UserMessage {
            level: MessageLevel::Success,
            text: format!("API server started in background (PID {child_pid})."),
        });

        return Ok(ApiServerOutcome::Start(ApiServerStartOutcome {
            port: flags.port,
            background: true,
            workdirs: workdir_strings,
            refreshed_key: false,
        }));
    }

    // Foreground mode: claim the machine through the one typed cross-daemon
    // rule (`DaemonGuard::acquire`: shared startup lock → check → pidfile claim
    // → re-check), then boot the HTTP server and clean up on exit. This command
    // owns only the mapping onto its own already-running error.
    guard
        .acquire(std::process::id())
        .map_err(|error| match error {
            AcquireError::AlreadyRunning { pid } => CommandError::ApiServerAlreadyRunning { pid },
            other => CommandError::Data(other.into_data_error()),
        })?;

    // TLS material: generate or load now (unless explicitly skipped) so the
    // bind_ip warning surfaces BEFORE we hand off to serve_until_shutdown.
    let bind_ip: std::net::IpAddr = "127.0.0.1".parse().expect("static loopback ip");
    let tls_material = if flags.dangerously_skip_tls {
        frontend.write_message(UserMessage {
            level: MessageLevel::Warning,
            text: "--dangerously-skip-tls set — serving plain HTTP on the loopback interface. Use only in trusted local environments.".to_string(),
        });
        None
    } else {
        let (mat, regenerated) = engines.auth_engine.ensure_self_signed_tls(bind_ip)?;
        if regenerated && api_paths.tls_bind_ip_file().exists() {
            // Existing sidecar file means a previous cert was here — emit the
            // re-pin warning. (We can't reliably distinguish "first ever cert"
            // from "regenerated for new IP" without extra state, but the sidecar
            // existing post-write is good enough as a proxy.)
            frontend.write_message(UserMessage {
                level: MessageLevel::Warning,
                text:
                    "TLS cert regenerated for new bind IP — pinned remote clients will need to re-pin"
                        .into(),
            });
        }
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!(
                "TLS ready (self-signed; cert fingerprint sha256:{}…)",
                &mat.fingerprint_sha256_hex[..16]
            ),
        });
        Some(mat)
    };

    let scheme = if tls_material.is_some() {
        "https"
    } else {
        "http"
    };

    // Persist server metadata so `api status` and remote clients can
    // probe the right endpoint.
    let _ = daemon.write_meta(&ServerMeta {
        port: flags.port,
        bind_ip: bind_ip.to_string(),
        scheme: scheme.to_string(),
        auth_disabled: flags.dangerously_skip_auth,
    });

    frontend.write_message(UserMessage {
        level: MessageLevel::Info,
        text: format!(
            "Starting {} API server on {}://{}:{} (Ctrl-C to stop).",
            if scheme == "https" { "HTTPS" } else { "HTTP" },
            scheme,
            bind_ip,
            flags.port,
        ),
    });

    let _ = auto_generated_key; // currently informational only; outcome already covers it via downstream signals

    let config = ApiServeConfig {
        port: flags.port,
        bind_ip,
        workdirs,
        dangerously_skip_auth: flags.dangerously_skip_auth,
        tls_material,
    };

    let serve_result = frontend.serve_until_shutdown(config).await;

    // Always clean up PID + meta files.
    let _ = daemon.release_pidfile();
    let _ = daemon.clear_meta();

    serve_result?;

    Ok(ApiServerOutcome::Start(ApiServerStartOutcome {
        port: flags.port,
        background: false,
        workdirs: workdir_strings,
        refreshed_key: false,
    }))
}

fn run_kill(
    api_paths: &crate::data::fs::ApiPaths,
    frontend: &mut dyn ApiServerCommandFrontend,
) -> Result<ApiServerOutcome, CommandError> {
    let daemon = api_daemon(api_paths);

    // Termination is a `DaemonProcess` operation; this command only maps the
    // typed outcome onto its own messages.
    let pid = match daemon.terminate_running()? {
        Termination::NotRunning => {
            frontend.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: "No API server is running (no PID file found).".to_string(),
            });
            return Err(CommandError::ApiServerNotRunning);
        }
        Termination::StalePidFile { pid } => {
            frontend.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: format!("Stale PID file removed (PID {pid} was not running)."),
            });
            return Err(CommandError::ApiServerNotRunning);
        }
        Termination::NotAwman { pid } => {
            frontend.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: format!(
                    "PID {pid} is alive but is not an awman server; stale PID file cleaned up."
                ),
            });
            return Err(CommandError::ApiServerNotRunning);
        }
        Termination::Terminated { pid } => pid,
    };
    let _ = daemon.clear_meta();

    frontend.write_message(UserMessage {
        level: MessageLevel::Success,
        text: format!("API server (PID {pid}) stopped."),
    });

    Ok(ApiServerOutcome::Kill(ApiServerKillOutcome {
        stopped_pid: Some(pid),
    }))
}

fn run_logs(
    api_paths: &crate::data::fs::ApiPaths,
    frontend: &mut dyn ApiServerCommandFrontend,
) -> Result<ApiServerOutcome, CommandError> {
    let log_path = api_paths.log_file();
    let log_str = log_path.display().to_string();

    match std::fs::read_to_string(&log_path) {
        Ok(content) => {
            for line in content.lines() {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Info,
                    text: line.to_string(),
                });
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            frontend.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: format!("Log file not found: {log_str}"),
            });
        }
        Err(e) => {
            return Err(CommandError::Data(crate::data::error::DataError::io(
                &log_path, e,
            )));
        }
    }

    Ok(ApiServerOutcome::Logs(ApiServerLogsOutcome {
        log_path: log_str,
    }))
}

async fn run_status(
    api_paths: &crate::data::fs::ApiPaths,
) -> Result<ApiServerOutcome, CommandError> {
    let daemon = api_daemon(api_paths);

    let pid = match daemon.running_pid()? {
        Some(pid) => pid,
        None => {
            // Cleanup any orphan meta file when no server is running.
            let _ = daemon.clear_meta();
            return Ok(ApiServerOutcome::Status(ApiServerStatusOutcome {
                running: false,
                pid: None,
                bound_addr: None,
                version: None,
                responsive: false,
            }));
        }
    };

    let meta = daemon.read_meta()?;
    let bound_addr = meta
        .as_ref()
        .map(|m| format!("{}://{}:{}", m.scheme, m.bind_ip, m.port));

    // HTTP-probe the running server when we know its endpoint. A short
    // timeout keeps `status` snappy; a missing/timed-out probe means the
    // process is alive but the server is not responsive.
    let (responsive, version) = if let Some(m) = meta.as_ref() {
        let probe_url = format!("{}://127.0.0.1:{}/v1/status", m.scheme, m.port);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .danger_accept_invalid_certs(true) // self-signed certs on loopback
            .build()
            .map_err(|e| CommandError::RemoteTransport(e.to_string()))?;
        match client.get(&probe_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body = resp.json::<serde_json::Value>().await.ok();
                let v = body
                    .as_ref()
                    .and_then(|b| b.get("version"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                (true, v)
            }
            _ => (false, None),
        }
    } else {
        (false, None)
    };

    Ok(ApiServerOutcome::Status(ApiServerStatusOutcome {
        running: true,
        pid: Some(pid),
        bound_addr,
        version,
        responsive,
    }))
}

/// Resolve the merged-and-validated workdir allowlist (per spec §6.4a).
/// Concatenate CLI-supplied workdirs and config workdirs, canonicalize,
/// deduplicate, and reject missing paths.
pub fn resolve_workdirs(
    cli: &[String],
    config: &[String],
) -> Result<Vec<std::path::PathBuf>, CommandError> {
    use std::collections::BTreeSet;
    let mut seen: BTreeSet<std::path::PathBuf> = BTreeSet::new();
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    for raw in cli.iter().chain(config.iter()) {
        let path = std::path::PathBuf::from(raw);
        if !path.exists() {
            return Err(CommandError::ApiWorkdirNotFound { path });
        }
        let canon = path.canonicalize().unwrap_or(path);
        if seen.insert(canon.clone()) {
            out.push(canon);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_workdirs_dedupes_overlapping_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let s = tmp.path().to_str().unwrap().to_string();
        let merged = resolve_workdirs(std::slice::from_ref(&s), std::slice::from_ref(&s)).unwrap();
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn resolve_workdirs_errors_on_missing_path() {
        let err = resolve_workdirs(&["/no/such/path".into()], &[]).unwrap_err();
        assert!(matches!(err, CommandError::ApiWorkdirNotFound { .. }));
    }

    #[test]
    fn resolve_workdirs_merges_cli_and_config() {
        let tmp_a = tempfile::tempdir().unwrap();
        let tmp_b = tempfile::tempdir().unwrap();
        let cli = vec![tmp_a.path().to_str().unwrap().to_string()];
        let cfg = vec![tmp_b.path().to_str().unwrap().to_string()];
        let merged = resolve_workdirs(&cli, &cfg).unwrap();
        assert_eq!(merged.len(), 2, "must contain both cli and config entries");
    }

    use crate::command::dispatch::Engines;
    use crate::data::fs::api_paths::ApiPaths;
    use crate::data::fs::auth_paths::AuthPathResolver;
    use crate::data::message::{UserMessage, UserMessageSink};
    use crate::engine::auth::AuthEngine;
    use std::sync::Arc;

    fn make_engines(tmp: &std::path::Path) -> Engines {
        let api_paths = ApiPaths::at_root(tmp);
        let auth_paths = AuthPathResolver::at_home(tmp);
        let runtime = Arc::new(crate::engine::container::ContainerRuntime::docker());
        let overlay = Arc::new(crate::engine::overlay::OverlayEngine::with_auth_resolver(
            auth_paths.clone(),
        ));
        let git_engine = Arc::new(crate::engine::git::GitEngine::new());
        let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            overlay.clone(),
            runtime.clone(),
        ));
        let auth_engine = Arc::new(AuthEngine::with_paths(auth_paths, api_paths));
        let workflow_state_store =
            Arc::new(crate::data::EngineWorkflowStateStore::at_git_root(tmp));
        Engines {
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

    struct NullFrontend {
        messages: Vec<String>,
    }
    impl UserMessageSink for NullFrontend {
        fn write_message(&mut self, msg: UserMessage) {
            self.messages.push(msg.text);
        }
        fn replay_queued(&mut self) {}
    }
    #[async_trait::async_trait]
    impl ApiServerCommandFrontend for NullFrontend {
        async fn serve_until_shutdown(
            &mut self,
            _config: ApiServeConfig,
        ) -> Result<(), crate::command::error::CommandError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn start_refresh_key_short_circuits_without_checking_auth() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        let engines = make_engines(tmp.path());
        let api_paths = engines.auth_engine.api_paths().clone();

        // Ensure API root exists.
        api_paths.ensure_root().unwrap();

        let flags = ApiServerStartFlags {
            port: 9876,
            workdirs: Vec::new(),
            background: false,
            refresh_key: true,
            dangerously_skip_auth: false, // no auth configured, but refresh_key skips check
            dangerously_skip_tls: false,
        };

        let mut frontend = NullFrontend {
            messages: Vec::new(),
        };
        let result = run_start(flags, &engines, &mut frontend, &api_paths).await;
        assert!(result.is_ok(), "refresh_key must short-circuit: {result:?}");
        if let Ok(ApiServerOutcome::Start(outcome)) = result {
            assert!(outcome.refreshed_key, "refreshed_key must be true");
        }
    }

    #[tokio::test]
    async fn start_without_auth_configured_auto_generates_key_and_proceeds() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        let engines = make_engines(tmp.path());
        let api_paths = engines.auth_engine.api_paths().clone();
        api_paths.ensure_root().unwrap();

        let flags = ApiServerStartFlags {
            port: 9876,
            workdirs: Vec::new(),
            background: false,
            refresh_key: false,
            dangerously_skip_auth: false,
            dangerously_skip_tls: false,
        };

        let mut frontend = NullFrontend {
            messages: Vec::new(),
        };
        let result = run_start(flags, &engines, &mut frontend, &api_paths).await;
        assert!(
            result.is_ok(),
            "first-run with no key must auto-generate one and proceed: {result:?}"
        );
        assert!(
            engines.auth_engine.read_api_key_hash().unwrap().is_some(),
            "auto-generated hash must be persisted to disk"
        );
        assert!(
            frontend
                .messages
                .iter()
                .any(|m| m.contains("No API key configured")),
            "must explain that a key was auto-generated; got: {:?}",
            frontend.messages
        );
        assert!(
            frontend.messages.iter().any(|m| m.starts_with('╔')),
            "must emit the banner; got: {:?}",
            frontend.messages
        );
    }

    #[tokio::test]
    async fn start_dangerously_skip_auth_proceeds_without_api_key() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        let engines = make_engines(tmp.path());
        let api_paths = engines.auth_engine.api_paths().clone();
        api_paths.ensure_root().unwrap();

        let flags = ApiServerStartFlags {
            port: 9876,
            workdirs: Vec::new(),
            background: false,
            refresh_key: false,
            dangerously_skip_auth: true,
            dangerously_skip_tls: false,
        };

        let mut frontend = NullFrontend {
            messages: Vec::new(),
        };
        let result = run_start(flags, &engines, &mut frontend, &api_paths).await;
        assert!(
            result.is_ok(),
            "dangerously_skip_auth must bypass auth check: {result:?}"
        );
    }

    #[tokio::test]
    async fn start_dangerously_skip_tls_yields_plain_http_config() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        let engines = make_engines(tmp.path());
        let api_paths = engines.auth_engine.api_paths().clone();
        api_paths.ensure_root().unwrap();

        struct CaptureFrontend {
            messages: Vec<String>,
            tls_was_present: Option<bool>,
            persisted_scheme: Option<String>,
            api_paths: crate::data::fs::ApiPaths,
        }
        impl UserMessageSink for CaptureFrontend {
            fn write_message(&mut self, msg: UserMessage) {
                self.messages.push(msg.text);
            }
            fn replay_queued(&mut self) {}
        }
        #[async_trait::async_trait]
        impl ApiServerCommandFrontend for CaptureFrontend {
            async fn serve_until_shutdown(
                &mut self,
                config: ApiServeConfig,
            ) -> Result<(), crate::command::error::CommandError> {
                self.tls_was_present = Some(config.tls_material.is_some());
                // Capture the persisted scheme BEFORE run_start's post-serve
                // cleanup removes the meta file.
                self.persisted_scheme = api_daemon(&self.api_paths)
                    .read_meta()
                    .ok()
                    .flatten()
                    .map(|m| m.scheme);
                Ok(())
            }
        }

        let flags = ApiServerStartFlags {
            port: 9876,
            workdirs: Vec::new(),
            background: false,
            refresh_key: false,
            dangerously_skip_auth: true,
            dangerously_skip_tls: true,
        };

        let mut frontend = CaptureFrontend {
            messages: Vec::new(),
            tls_was_present: None,
            persisted_scheme: None,
            api_paths: api_paths.clone(),
        };
        let result = run_start(flags, &engines, &mut frontend, &api_paths).await;
        assert!(result.is_ok(), "skip-tls must allow startup: {result:?}");
        assert_eq!(
            frontend.tls_was_present,
            Some(false),
            "serve_until_shutdown must be called with tls_material = None"
        );
        assert!(
            frontend
                .messages
                .iter()
                .any(|m| m.contains("--dangerously-skip-tls")),
            "must warn about plaintext mode; got: {:?}",
            frontend.messages
        );
        assert!(
            frontend
                .messages
                .iter()
                .any(|m| m.contains("http://127.0.0.1:9876")),
            "must announce the http:// scheme; got: {:?}",
            frontend.messages
        );
        assert_eq!(
            frontend.persisted_scheme.as_deref(),
            Some("http"),
            "server meta scheme must be persisted as http while the server is running"
        );
    }

    #[test]
    fn kill_no_pid_file_returns_api_not_running_with_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = make_engines(tmp.path());
        let api_paths = engines.auth_engine.api_paths().clone();
        api_paths.ensure_root().unwrap();

        let mut frontend = NullFrontend {
            messages: Vec::new(),
        };
        let result = run_kill(&api_paths, &mut frontend);
        assert!(
            matches!(result, Err(CommandError::ApiServerNotRunning)),
            "kill with no PID file must surface ApiServerNotRunning: {result:?}"
        );
        assert!(
            frontend
                .messages
                .iter()
                .any(|m| m.contains("No API") || m.contains("no PID")),
            "must emit a warning; got: {:?}",
            frontend.messages
        );
    }

    #[test]
    fn kill_stale_pid_file_is_cleaned_up_and_returns_api_not_running() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = make_engines(tmp.path());
        let api_paths = engines.auth_engine.api_paths().clone();
        api_paths.ensure_root().unwrap();
        let pid_path = api_paths.pid_file();

        // Write a PID that can't possibly be alive.
        api_daemon(&api_paths)
            .force_write_pidfile(u32::MAX - 1)
            .unwrap();

        let mut frontend = NullFrontend {
            messages: Vec::new(),
        };
        let result = run_kill(&api_paths, &mut frontend);
        assert!(
            matches!(result, Err(CommandError::ApiServerNotRunning)),
            "stale PID must surface ApiServerNotRunning: {result:?}"
        );
        assert!(
            !pid_path.exists(),
            "PID file must be removed after stale detection"
        );
    }

    #[tokio::test]
    async fn status_no_pid_file_returns_not_running() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = make_engines(tmp.path());
        let api_paths = engines.auth_engine.api_paths().clone();
        api_paths.ensure_root().unwrap();

        let result = run_status(&api_paths).await;
        assert!(result.is_ok());
        if let Ok(ApiServerOutcome::Status(outcome)) = result {
            assert!(!outcome.running);
            assert!(outcome.pid.is_none());
            assert!(!outcome.responsive, "no server → not responsive");
            assert!(outcome.bound_addr.is_none());
            assert!(outcome.version.is_none());
        }
    }

    #[tokio::test]
    async fn status_with_alive_pid_but_no_meta_reports_not_responsive() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = make_engines(tmp.path());
        let api_paths = engines.auth_engine.api_paths().clone();
        api_paths.ensure_root().unwrap();

        // Write our own PID — definitely alive and "awman"-named on most CI.
        // On platforms where pid_is_awman returns false for the test binary,
        // check_already_running will treat it as stale; that's still a
        // useful signal — running=false, responsive=false.
        api_daemon(&api_paths)
            .force_write_pidfile(std::process::id())
            .unwrap();

        let result = run_status(&api_paths).await.unwrap();
        if let ApiServerOutcome::Status(outcome) = result {
            // Either the test binary identifies as "awman" (running=true) or
            // not (running=false, stale-cleanup). In both cases responsive=false
            // because we wrote no server meta.
            assert!(!outcome.responsive, "no meta + no server → not responsive");
        }
    }

    #[test]
    fn logs_missing_log_file_emits_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = make_engines(tmp.path());
        let api_paths = engines.auth_engine.api_paths().clone();
        api_paths.ensure_root().unwrap();

        let mut frontend = NullFrontend {
            messages: Vec::new(),
        };
        let result = run_logs(&api_paths, &mut frontend);
        assert!(
            result.is_ok(),
            "missing log file must not error: {result:?}"
        );
        assert!(
            frontend
                .messages
                .iter()
                .any(|m| m.contains("not found") || m.contains("Log")),
            "must emit log-not-found warning; got: {:?}",
            frontend.messages
        );
    }

    #[test]
    fn logs_existing_log_file_streams_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = make_engines(tmp.path());
        let api_paths = engines.auth_engine.api_paths().clone();
        api_paths.ensure_root().unwrap();

        // Write a log file.
        let log_path = api_paths.log_file();
        std::fs::write(&log_path, "line one\nline two\nline three\n").unwrap();

        let mut frontend = NullFrontend {
            messages: Vec::new(),
        };
        let result = run_logs(&api_paths, &mut frontend);
        assert!(result.is_ok());
        assert_eq!(frontend.messages.len(), 3, "must stream all lines");
        assert_eq!(frontend.messages[0], "line one");
        assert_eq!(frontend.messages[2], "line three");
    }
}
