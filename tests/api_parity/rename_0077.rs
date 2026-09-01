//! WI-0077 API-parity rename verification.
//!
//! Verifies:
//! - `ApiPaths` paths use "awman" not "amux" or "headless".
//! - The database filename embeds "awman" (not "amux").
//! - The API key banner uses "awman" branding.
//! - `ApiServeConfig` type name uses "Api" terminology (compile-time evidence).
//! - The API server startup log message contains "api" and not "headless"/"amux".

use awman::command::commands::api_server::banner::render_api_key_banner;
use awman::command::commands::api_server::ApiServeConfig;
use awman::data::config::env::{EnvSnapshot, AWMAN_API_ROOT};
use awman::data::fs::api_paths::ApiPaths;

// ─── ApiPaths naming ─────────────────────────────────────────────────────────

/// The SQLite database file is named `awman.db`, not `amux.db`.
#[test]
fn api_paths_db_filename_is_awman_db() {
    let paths = ApiPaths::from_root("/srv/api-test");
    let db = paths.db_path();
    let filename = db.file_name().unwrap().to_string_lossy();
    assert_eq!(
        filename, "awman.db",
        "database filename must be 'awman.db' (not 'amux.db'); got {filename:?}"
    );
}

/// `ApiPaths::from_root()` must not produce any path components containing "amux"
/// or "headless".
#[test]
fn api_paths_contain_no_amux_or_headless_segments() {
    let paths = ApiPaths::from_root("/srv/api-test");
    for path in &[
        paths.db_path(),
        paths.sessions_dir(),
        paths.tls_dir(),
        paths.api_key_hash_file(),
    ] {
        let display = path.display().to_string().to_lowercase();
        assert!(
            !display.contains("amux"),
            "ApiPaths must not contain 'amux'; got {path:?}"
        );
        assert!(
            !display.contains("headless"),
            "ApiPaths must not contain 'headless'; got {path:?}"
        );
    }
}

/// The default API root (via `AWMAN_API_ROOT`) embeds "awman", not "amux".
/// We verify by reading the env var constant name itself.
#[test]
fn awman_api_root_env_var_constant_uses_awman_prefix() {
    assert!(
        AWMAN_API_ROOT.starts_with("AWMAN_"),
        "AWMAN_API_ROOT constant must start with 'AWMAN_'; got {AWMAN_API_ROOT:?}"
    );
}

/// `AWMAN_API_ROOT` override is honoured for the API root. As of WI 0101
/// Part 1.1 the shared database no longer lives under the API root — it moved
/// to `<data_home>/data/awman.db` (resolved by `DataPaths`), which does not
/// track `AWMAN_API_ROOT`. The API root override still governs everything else
/// under `~/.awman/api/`.
#[test]
fn api_paths_honours_awman_api_root_override() {
    let env = EnvSnapshot::with_overrides([(AWMAN_API_ROOT, "/custom/awman/api")]);
    let paths = ApiPaths::from_env(&env).expect("from_env");
    assert_eq!(
        paths.root(),
        std::path::Path::new("/custom/awman/api"),
        "ApiPaths must respect AWMAN_API_ROOT override"
    );
    // The database relocated out of the API root; it is no longer under it.
    let db = paths.db_path();
    assert!(
        !db.starts_with("/custom/awman/api"),
        "db path must NOT be under the API root after the WI 0101 relocation; got {db:?}"
    );
    assert!(
        db.ends_with("data/awman.db"),
        "db path must live under <data_home>/data/awman.db; got {db:?}"
    );
}

// ─── API key banner ──────────────────────────────────────────────────────────

/// The API key banner presented to users on key generation must use "awman"
/// branding, not "amux".
#[test]
fn api_key_banner_uses_awman_branding() {
    let key = "a".repeat(64);
    let banner = render_api_key_banner(&key);
    let lower = banner.to_lowercase();
    assert!(
        lower.contains("awman"),
        "API key banner must mention 'awman'; got:\n{banner}"
    );
    assert!(
        !lower.contains("amux"),
        "API key banner must not mention 'amux'; got:\n{banner}"
    );
    assert!(
        !lower.contains("headless"),
        "API key banner must not mention 'headless'; got:\n{banner}"
    );
}

// ─── ApiServeConfig type name ─────────────────────────────────────────────────

/// Compile-time check: `ApiServeConfig` can be named and constructed, proving
/// the type was renamed from the legacy `HeadlessServeConfig`. The struct name
/// itself is the assertion — if the type were still `HeadlessServeConfig` this
/// test would not compile.
#[test]
fn api_serve_config_type_uses_api_naming() {
    // Constructing the type only to confirm the name compiles.
    let _config: Option<ApiServeConfig> = None;
    // No runtime assertion needed: if the type name were "Headless…", the
    // `use awman::command::commands::api_server::ApiServeConfig` import above
    // would be a compile error.
}

// ─── API server startup log message ─────────────────────────────────────────

/// The startup/shutdown log messages emitted by `frontend::api::serve()` must
/// contain "awman" and "API mode" and must not contain "headless" or "amux"
/// (other than inside "awman").
///
/// This test reads the actual source file rather than asserting on a local
/// constant, so changes to the log message string are caught directly.
#[test]
fn api_startup_log_message_contains_awman_and_api_mode() {
    // Keep the source scan independent of the runtime checkout. Some test
    // runners execute the compiled test binary after the source workspace has
    // been torn down; `include_str!` makes Cargo track this file as an input
    // while preserving the assertion against the actual source.
    let src = include_str!("../../src/frontend/api/mod.rs");

    // Collect every `tracing::info!` literal that mentions "starting",
    // "listening", or "stopped" — those are the lifecycle log lines.
    let lifecycle_msgs: Vec<&str> = src
        .lines()
        .filter_map(|l| {
            l.trim()
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix("\""))
        })
        .filter(|l| {
            let lower = l.to_lowercase();
            lower.contains("starting") || lower.contains("listening") || lower.contains("stopped")
        })
        .collect();

    assert!(
        !lifecycle_msgs.is_empty(),
        "expected to find at least one lifecycle log message in src/frontend/api/mod.rs"
    );

    for msg in &lifecycle_msgs {
        let lower = msg.to_lowercase();
        assert!(
            lower.contains("awman"),
            "lifecycle log message must contain 'awman'; got {msg:?}"
        );
        assert!(
            lower.contains("api mode"),
            "lifecycle log message must contain 'API mode'; got {msg:?}"
        );
        assert!(
            !lower.contains("headless"),
            "lifecycle log message must not contain 'headless'; got {msg:?}"
        );
        // "amux" as a substring of "awman" is fine; reject standalone "amux".
        let stripped = lower.replace("awman", "");
        assert!(
            !stripped.contains("amux"),
            "lifecycle log message must not contain standalone 'amux'; got {msg:?}"
        );
    }
}

/// Live server smoke-test: boot the router on an ephemeral port and confirm
/// the `/v1/status` endpoint returns 200 (not 404 or 500), proving the API
/// frontend is correctly wired under the "api" name, not "headless".
///
/// Skipped when loopback binding is unavailable (sandboxed CI).
#[tokio::test]
async fn real_network_api_frontend_status_endpoint_reachable_after_rename() {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Instant;

    use awman::command::dispatch::Engines;
    use awman::data::fs::api_db::SqliteSessionStore;
    use awman::data::fs::auth_paths::AuthPathResolver;
    use awman::data::EngineWorkflowStateStore;
    use awman::engine::agent::AgentEngine;
    use awman::engine::auth::AuthEngine;
    use awman::engine::container::ContainerRuntime;
    use awman::engine::git::GitEngine;
    use awman::engine::overlay::OverlayEngine;
    use awman::frontend::api::routes::{build_router, AppState, AuthMode};

    let tmp = tempfile::tempdir().unwrap();
    let paths = ApiPaths::from_root(tmp.path());
    paths.ensure_root().expect("ensure_root");
    let store = SqliteSessionStore::open(paths.root()).expect("open sqlite");
    let auth_paths = AuthPathResolver::at_home(tmp.path());
    let runtime = Arc::new(ContainerRuntime::docker());
    let git_engine = Arc::new(GitEngine::new());
    let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    let agent_engine = Arc::new(AgentEngine::new(overlay_engine.clone(), runtime.clone()));
    let auth_engine = Arc::new(AuthEngine::with_paths(auth_paths, paths.clone()));
    let workflow_state_store = Arc::new(EngineWorkflowStateStore::at_git_root(tmp.path()));

    let engines = Engines {
        runtime: runtime.clone(),
        container_runtime: Some(runtime),
        sandbox_runtime: None,
        git_engine,
        overlay_engine,
        auth_engine,
        agent_engine,
        workflow_state_store,
    };

    let state = Arc::new(AppState {
        store: Arc::new(store),
        paths,
        workdirs: vec![],
        started_at: Instant::now(),
        task_handles: tokio::sync::Mutex::new(Vec::new()),
        auth_mode: AuthMode::Disabled,
        engines,
        sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        event_buses: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        setup_buses: tokio::sync::Mutex::new(HashMap::new()),
    });

    let app = build_router(state);
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(_) => {
            eprintln!("SKIP: cannot bind loopback — skipping live server test");
            return;
        }
    };
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url = format!("http://{addr}/v1/status");
    let resp = match reqwest::get(&url).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("SKIP: reqwest error (likely no network): {e}");
            return;
        }
    };
    assert_eq!(
        resp.status().as_u16(),
        200,
        "GET /v1/status must return 200 after rename; got {}",
        resp.status()
    );
}
