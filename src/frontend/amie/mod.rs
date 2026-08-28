//! amie daemon frontend and bootstrap.

pub mod routes;
pub mod state;
pub mod unattended;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use crate::command::commands::amie::commands::AmieServeConfig;
use crate::command::commands::amie::evaluation::LocalConditionEvaluator;
use crate::command::commands::amie::gateway::LocalConditionGateway;
use crate::command::commands::amie::runtime_guard::require_container_tier;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::config::env::Env;
use crate::data::fs::daemon_process::{
    DaemonProcess, ServerMeta, AMIE_PLIST_LABEL, AMIE_UNIT_NAME,
};
use crate::data::fs::{AmiePaths, ConditionStore, DataPaths};
use crate::data::session::{Session, SessionOpenOptions, StaticGitRootResolver};
use crate::engine::amie::{AmieScheduler, ConditionEvaluator};
use crate::frontend::api::serve::{self, ServeOptions};

use self::state::AmieAppState;
use self::unattended::UnattendedFrontends;

/// Start the daemon after `AmieDaemonCommand` has acquired its `DaemonGuard`.
/// The order below is deliberately load-bearing and is kept contiguous so it
/// remains auditable: runtime admission → DB relocation → open/migrate store →
/// orphan reconciliation → scheduler → HTTP server.
pub async fn serve(config: AmieServeConfig) -> Result<(), CommandError> {
    let env = Env::from_process();
    let amie_paths = AmiePaths::from_env(&env)?;
    let engines = build_engines(&amie_paths)?;
    // The production evaluator is Layer 2; this frontend supplies only the
    // unattended frontends it runs agents and workflows with.
    let evaluator = Arc::new(LocalConditionEvaluator::new(
        engines.clone(),
        env,
        UnattendedFrontends::shared(),
    ));
    serve_with(config, engines, evaluator).await
}

/// Injectable bootstrap used by integration tests and by the command layer
/// once its concrete evaluator is available.
pub async fn serve_with(
    config: AmieServeConfig,
    engines: Engines,
    evaluator: Arc<dyn ConditionEvaluator>,
) -> Result<(), CommandError> {
    // This is immediately after DaemonGuard::acquire (in AmieDaemonCommand)
    // and before either database is opened.
    require_container_tier(&engines)?;

    let env = Env::from_process();
    let amie_paths = AmiePaths::from_env(&env)?;
    let process = DaemonProcess::new(amie_paths.daemon(), AMIE_UNIT_NAME, AMIE_PLIST_LABEL);
    let data_paths = DataPaths::from_env(&env)?;
    let legacy_api_paths = crate::data::fs::ApiPaths::from_env(&env)?;
    let outcome = data_paths.migrate_legacy_db(legacy_api_paths.root())?;
    tracing::info!(migration = ?outcome, "amie database migration outcome");

    // Startup order is load-bearing: open the store, apply amie's idempotent
    // schema migration, then reconcile any run left `running` by a crash.
    let store = Arc::new(ConditionStore::open(&data_paths.db_path())?);
    store.migrate()?;
    let reconciled = store.reconcile_orphaned_runs(chrono::Utc::now())?;
    if reconciled > 0 {
        tracing::warn!(count = reconciled, "amie reconciled orphaned runs");
    }

    let scheduler = AmieScheduler::new(store.clone(), amie_paths.clone(), evaluator, env.clone());
    let scheduler_status = scheduler.status_handle();
    let gateway = Arc::new(LocalConditionGateway::new(
        store.clone(),
        engines.clone(),
        scheduler_status,
    ));

    let auth_mode = serve::resolve_auth_mode(
        &amie_paths.daemon(),
        config.dangerously_skip_auth,
        "awman amie start --refresh-key",
    )?;
    let cwd = std::env::current_dir().map_err(|error| {
        CommandError::Other(format!("cannot resolve amie working directory: {error}"))
    })?;
    let resolver = StaticGitRootResolver::new(&cwd);
    let session = Arc::new(tokio::sync::RwLock::new(Session::open_or_workdir_fallback(
        cwd,
        &resolver,
        SessionOpenOptions::default(),
    )?));
    let state = Arc::new(AmieAppState {
        store,
        gateway,
        auth_mode,
        engines,
        session,
        started_at: std::time::Instant::now(),
        bound_addr: std::sync::Mutex::new(None),
    });

    let shutdown = tokio_util::sync::CancellationToken::new();
    let scheduler_shutdown = shutdown.clone();
    let scheduler_task = tokio::spawn(scheduler.run(scheduler_shutdown));
    let router = routes::build_router(state.clone());
    let requested = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), config.port);
    let serve_result = serve::serve_router_with_bound(
        router,
        ServeOptions {
            addr: requested,
            tls: None,
            shutdown_grace: std::time::Duration::from_secs(30),
        },
        |addr| {
            let endpoint = format!("http://{addr}");
            process.write_meta(&ServerMeta {
                port: addr.port(),
                bind_ip: "127.0.0.1".into(),
                scheme: "http".into(),
                auth_disabled: config.dangerously_skip_auth,
            })?;
            *state
                .bound_addr
                .lock()
                .expect("amie bound-address mutex poisoned") = Some(endpoint.clone());
            tracing::info!(endpoint, "amie daemon listening");
            Ok(())
        },
    )
    .await;
    shutdown.cancel();
    let _ = scheduler_task.await;
    serve_result
}

/// Build the runtime bundle needed by a standalone daemon process.  This is
/// the same runtime-detection construction used by API mode, but it opens no
/// SQLite connection; the runtime-tier check in `serve_with` remains before
/// the data-store migration/open sequence.
fn build_engines(amie_paths: &AmiePaths) -> Result<Engines, CommandError> {
    let auth_paths = crate::data::fs::AuthPathResolver::from_process_env()?;
    let api_paths = crate::data::fs::ApiPaths::from_process_env()?;
    let auth_engine = crate::engine::auth::AuthEngine::with_paths(auth_paths.clone(), api_paths);
    let global_config = crate::data::config::GlobalConfig::load().unwrap_or_default();
    let detected = crate::engine::agent_runtime::detect(&global_config)
        .map_err(|error| CommandError::Other(format!("agent runtime detect: {error}")))?;
    let runtime = detected.engine();
    let container_runtime = detected.container_runtime();
    let sandbox_runtime = detected.sandbox_runtime();
    let overlay_engine = Arc::new(crate::engine::overlay::OverlayEngine::with_auth_resolver(
        auth_paths,
    ));
    let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
        overlay_engine.clone(),
        container_runtime
            .clone()
            .unwrap_or_else(|| Arc::new(crate::engine::container::ContainerRuntime::docker())),
    ));
    Ok(Engines {
        runtime,
        container_runtime,
        sandbox_runtime,
        git_engine: Arc::new(crate::engine::git::GitEngine::new()),
        overlay_engine,
        auth_engine: Arc::new(auth_engine),
        agent_engine,
        workflow_state_store: Arc::new(crate::data::EngineWorkflowStateStore::at_git_root(
            amie_paths.root(),
        )),
    })
}
