//! API HTTP frontend — full Axum server.
//!
//! Wire-identical to `oldsrc/commands/headless/server.rs`; the only internal
//! change is that `POST /v1/commands` dispatches through Layer 2 instead of
//! spawning a child process.

pub mod command_frontend;
pub mod event_bus;
pub mod queue_worker;
pub mod routes;
pub mod serve;
pub mod session_setup;

use crate::command::commands::api_server::ApiServeConfig;
use crate::command::error::CommandError;

/// Boot the API HTTP server and block until shutdown signal.
pub async fn serve(config: ApiServeConfig) -> Result<(), CommandError> {
    use std::sync::Arc;
    use std::time::Instant;

    use crate::data::fs::api_db::SqliteSessionStore;
    use crate::data::fs::api_paths::ApiPaths;

    let api_paths = ApiPaths::from_process_env().map_err(CommandError::Data)?;
    api_paths.ensure_root().map_err(CommandError::Data)?;

    // The database is shared with the squad daemon and lives under
    // `<data_home>/data/`. Relocate a pre-squad `<api_root>/awman.db` *before*
    // any connection is opened, exactly as the squad daemon does — otherwise the
    // two daemons would open different files and API-mode data would appear to
    // vanish after squad migrated it.
    let data_paths = api_paths.data_paths();
    let migration = data_paths
        .migrate_legacy_db(api_paths.root())
        .map_err(CommandError::Data)?;
    tracing::info!(migration = ?migration, "api database migration outcome");

    let db_path = api_paths.db_path();
    tracing::info!(db = %db_path.display(), "Opening session store");
    let store = SqliteSessionStore::open_at(&db_path).map_err(CommandError::Data)?;

    // Startup cleanup: remove closed sessions older than 24 hours.
    if let Ok(deleted) = store.delete_closed_sessions_older_than(24) {
        for (sid, cmd_count) in &deleted {
            tracing::info!(
                session_id = %sid,
                commands = cmd_count,
                "Purging stale closed session"
            );
        }
    }

    let auth_paths = crate::data::fs::auth_paths::AuthPathResolver::from_process_env()
        .map_err(CommandError::Data)?;
    let auth_engine =
        crate::engine::auth::AuthEngine::with_paths(auth_paths.clone(), api_paths.clone());

    let auth_mode = serve::resolve_auth_mode(
        &api_paths.daemon(),
        config.dangerously_skip_auth,
        "awman api start --refresh-key",
    )?;
    let auth_enabled = matches!(auth_mode, routes::AuthMode::Enabled { .. });

    // Construct Layer 1 engines for dispatch. The container runtime is
    // resolved via the same `detect` path the CLI/TUI use so that per-user
    // `~/.awman/config.json` settings (e.g. `runtime: "apple-containers"`)
    // are honored in API mode. Per-repo `.awman/config.json` is loaded later
    // per session via `Session::open_or_workdir_fallback`.
    let global_config = crate::data::config::global::GlobalConfig::load().unwrap_or_default();
    let detected = crate::engine::agent_runtime::detect(&global_config)
        .map_err(|e| CommandError::Other(format!("agent runtime detect: {e}")))?;
    let runtime = detected.engine();
    let container_runtime = detected.container_runtime();
    let sandbox_runtime = detected.sandbox_runtime();
    tracing::info!(runtime = runtime.runtime_name(), "Agent runtime resolved");
    let git_engine = Arc::new(crate::engine::git::GitEngine::new());
    let overlay_engine = Arc::new(crate::engine::overlay::OverlayEngine::with_auth_resolver(
        auth_paths,
    ));
    // AgentEngine is container-paradigm-specific. Under a sandbox-class
    // runtime it receives an inert Docker handle that is never exercised:
    // every container-paradigm flow guards via
    // `Engines::require_container_runtime()` first (sandbox flows land in
    // WI 0090).
    let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
        overlay_engine.clone(),
        container_runtime
            .clone()
            .unwrap_or_else(|| Arc::new(crate::engine::container::ContainerRuntime::docker())),
    ));
    let auth_engine_arc = Arc::new(auth_engine);
    // Use a temporary workflow state store path; each command opens its own
    // session-scoped store via the workdir, but Engines requires one at
    // construction time.
    let workflow_state_store = Arc::new(crate::data::EngineWorkflowStateStore::at_git_root(
        api_paths.root(),
    ));

    let engines = crate::command::dispatch::Engines {
        runtime,
        container_runtime,
        sandbox_runtime,
        git_engine,
        overlay_engine,
        auth_engine: auth_engine_arc,
        agent_engine,
        workflow_state_store,
    };

    // Restore in-memory sessions for any active sessions persisted in SQLite
    // from a previous server lifetime. This ensures session continuity across
    // server restarts.
    tracing::info!("Restoring active sessions from previous server lifetime");
    let mut restored_sessions = std::collections::HashMap::new();
    if let Ok(records) = store.list_sessions_by_status(Some("active")) {
        for rec in records {
            let workdir_path = std::path::PathBuf::from(&rec.workdir);

            // For remote sessions, the workdir IS the clone directory; if the
            // clone no longer exists on disk, close the session and skip.
            if rec.session_type == "remote" && !workdir_path.exists() {
                tracing::warn!(
                    session_id = %rec.id,
                    workdir = %rec.workdir,
                    "Remote session clone no longer exists; closing session"
                );
                let now = chrono::Utc::now().to_rfc3339();
                let _ = store.close_session_force(&rec.id, &now);
                continue;
            }

            let resolver = crate::data::session::StaticGitRootResolver::new(&workdir_path);
            match crate::data::session::Session::open_or_workdir_fallback(
                workdir_path.clone(),
                &resolver,
                crate::data::session::SessionOpenOptions::default(),
            ) {
                Ok(mut s) => {
                    // Restore the SessionType variant for remote sessions so
                    // that worktree suppression and other type-aware code
                    // paths behave correctly. repo_url and branch aren't
                    // persisted in the schema; use empty placeholders since
                    // only the variant and cloned_path are actually consumed.
                    if rec.session_type == "remote" {
                        if let Some(cloned) = rec.cloned_path.as_deref() {
                            s.set_session_type(crate::data::session::SessionType::Remote {
                                repo_url: String::new(),
                                branch: String::new(),
                                cloned_path: std::path::PathBuf::from(cloned),
                            });
                        }
                    }
                    restored_sessions.insert(rec.id.clone(), Arc::new(tokio::sync::RwLock::new(s)));
                    tracing::info!(session_id = %rec.id, workdir = %rec.workdir, "Restored session");
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = %rec.id,
                        workdir = %rec.workdir,
                        error = %e,
                        "Failed to restore session (workdir may no longer exist)"
                    );
                }
            }
        }
    }

    // Mark sessions with in-progress setup as failed (server restarted mid-setup).
    // Authoritative source is the DB's setup_status column; we also persist
    // a setup_state.json for the /status endpoint's disk fallback path so
    // that the failure reason is visible to clients.
    {
        use crate::data::session_setup_event::{SessionSetupError, SessionSetupStatus};
        if let Ok(records) = store.list_sessions_with_in_progress_setup() {
            for rec in &records {
                tracing::warn!(
                    session_id = %rec.id,
                    previous_status = %rec.setup_status,
                    "Marking session as failed (server restarted during setup)"
                );
                let _ = store.update_setup_status(&rec.id, "failed");

                // Clean up any partial clone for remote sessions (Layer 1).
                if rec.session_type == "remote" {
                    if let Some(cloned) = rec.cloned_path.as_deref() {
                        let _ = engines
                            .git_engine
                            .delete_directory(&std::path::PathBuf::from(cloned));
                    }
                }

                // Update or create setup_state.json so /status reflects the
                // restart failure once the in-memory bus is gone (Layer 0).
                let mut ss = api_paths.read_setup_state(&rec.id).unwrap_or_default();
                ss.status = SessionSetupStatus::Failed;
                ss.current_stage = Some("Server restarted during session setup".to_string());
                ss.error = Some(SessionSetupError {
                    stage: "server_restart".to_string(),
                    message: "Server restarted during session setup".to_string(),
                });
                let _ = api_paths.save_setup_state(&rec.id, &ss);
            }
        }
    }

    let store = Arc::new(store);

    // Stale command recovery: at startup, every command still in `running`
    // is unequivocally stale — the worker that owned it died with the
    // previous server process. Use a zero-second threshold so we recover all
    // of them immediately rather than leaving recently-started ones stuck
    // for the duration of the threshold.
    match store.recover_stale_commands(0) {
        Ok(recovered) if !recovered.is_empty() => {
            tracing::info!(
                count = recovered.len(),
                "Recovered stale running commands back to queued"
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to recover stale commands");
        }
        _ => {}
    }

    let sessions = Arc::new(tokio::sync::Mutex::new(restored_sessions));
    let event_buses = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

    let state = Arc::new(routes::AppState {
        store: Arc::clone(&store),
        paths: api_paths,
        workdirs: config.workdirs,
        started_at: Instant::now(),
        task_handles: tokio::sync::Mutex::new(Vec::new()),
        auth_mode,
        engines: engines.clone(),
        sessions: Arc::clone(&sessions),
        event_buses: Arc::clone(&event_buses),
        setup_buses: tokio::sync::Mutex::new(std::collections::HashMap::new()),
    });

    // Spawn queue workers.
    let worker_count = global_config.workers();
    if worker_count == 0 {
        tracing::warn!("workers config is 0 — no queue workers will run; commands will be enqueued but never processed");
    }
    for i in 0..worker_count {
        let worker = queue_worker::QueueWorker::new(
            format!("worker-{i}-{}", uuid::Uuid::new_v4()),
            Arc::clone(&store),
            engines.clone(),
            Arc::clone(&sessions),
            Arc::clone(&event_buses),
            state.paths.clone(),
        );
        tokio::spawn(worker.run());
        tracing::debug!(worker_index = i, "Spawned queue worker");
    }
    tracing::info!(count = worker_count, "Queue workers started");

    let app = routes::build_router(state.clone());
    let addr = std::net::SocketAddr::from((config.bind_ip, config.port));

    let scheme = if config.tls_material.is_some() {
        "https"
    } else {
        "http"
    };
    tracing::info!(
        port = config.port,
        bind_ip = %config.bind_ip,
        tls = config.tls_material.is_some(),
        auth = auth_enabled,
        "awman API mode starting"
    );
    // Pre-bind announce so the user sees the endpoint BEFORE the server
    // blocks on the accept loop. The matching post-bind "listening" log
    // below previously sat after `.await` and never fired in the success
    // path (the future only resolves on shutdown).
    tracing::info!(
        endpoint = %format!("{scheme}://{}:{}", config.bind_ip, config.port),
        "awman API mode listening — binding HTTP listener"
    );

    // Bind, serve, and handle graceful shutdown via the shared daemon bootstrap.
    // The EADDRINUSE mapping and SIGINT/SIGTERM handling live in `serve_router`.
    serve::serve_router(
        app,
        serve::ServeOptions {
            addr,
            tls: config.tls_material.clone(),
            shutdown_grace: std::time::Duration::from_secs(30),
        },
    )
    .await?;

    tracing::info!(
        port = config.port,
        "awman API mode stopped accepting new connections"
    );

    // Grace period for running commands (30s).
    const GRACE_SECS: u64 = 30;
    let handles: Vec<_> = state.task_handles.lock().await.drain(..).collect();
    if !handles.is_empty() {
        tracing::info!(
            count = handles.len(),
            grace_seconds = GRACE_SECS,
            "Waiting for running commands to finish"
        );
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(GRACE_SECS);
        for handle in handles {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                handle.abort();
            } else {
                let _ = tokio::time::timeout(remaining, handle).await;
            }
        }
    }

    tracing::info!("awman API mode stopped");
    Ok(())
}
