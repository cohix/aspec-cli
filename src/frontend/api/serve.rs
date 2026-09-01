//! Daemon bootstrap primitives shared by every awman HTTP daemon.
//!
//! Extracted verbatim from the `frontend::api::serve` monolith so a second
//! daemon (the squad daemon, WI 0101 Part 4) can bind, serve, and shut down over
//! the exact same code path instead of duplicating it. Nothing here is
//! API-specific: `serve_router` takes a fully-formed [`Router`] and
//! `resolve_auth_mode` takes a [`DaemonPaths`], so both daemons drive them with
//! their own state.

use std::net::SocketAddr;
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};

use crate::command::error::CommandError;
use crate::data::fs::daemon_paths::DaemonPaths;
use crate::engine::auth::TlsMaterial;
use crate::frontend::api::routes::AuthMode;

/// The single JSON error envelope every awman HTTP daemon emits.
#[derive(serde::Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

/// Build the shared `{"error": "..."}` body. Both routers use this, so the
/// wire shape has exactly one definition.
pub fn error_json(message: impl Into<String>) -> Json<ErrorResponse> {
    Json(ErrorResponse {
        error: message.into(),
    })
}

/// The one bearer-token authentication decision, shared by every awman daemon
/// router.
///
/// Returns `Some(response)` when the request must be rejected and `None` when
/// it may proceed. Keeping SHA-256 hashing, the accepted header syntax, the
/// constant-time comparison, and the two rejection messages in one function is
/// what stops two security-sensitive implementations from drifting apart.
pub fn check_bearer_auth(mode: &AuthMode, headers: &HeaderMap) -> Option<Response> {
    let AuthMode::Enabled { ref key_hash } = *mode else {
        return None;
    };
    let header = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty());
    let Some(header) = header else {
        return Some(
            (
                StatusCode::UNAUTHORIZED,
                error_json(
                    "API key required. Pass the key via the Authorization header \
                     (e.g. Authorization: Bearer <key>).",
                ),
            )
                .into_response(),
        );
    };
    let supplied = if header
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer "))
    {
        &header[7..]
    } else {
        header
    };
    let supplied_hash = {
        use ring::digest;
        let digest = digest::digest(&digest::SHA256, supplied.as_bytes());
        digest
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    use subtle::ConstantTimeEq;
    let matches: bool = supplied_hash.as_bytes().ct_eq(key_hash.as_bytes()).into();
    if matches {
        None
    } else {
        Some((StatusCode::UNAUTHORIZED, error_json("Invalid API key.")).into_response())
    }
}

/// Everything `serve_router` needs to bind and serve a router.
pub struct ServeOptions {
    /// The socket address to bind.
    pub addr: SocketAddr,
    /// TLS material for an HTTPS listener, or `None` for plain HTTP.
    pub tls: Option<TlsMaterial>,
    /// Graceful-shutdown grace period applied when SIGINT/SIGTERM fires.
    pub shutdown_grace: Duration,
}

/// Bind `router` on `options.addr`, serve it, and install a SIGINT/SIGTERM →
/// graceful-shutdown task. Maps `EADDRINUSE` to the same user-facing message
/// `frontend::api::serve` has always emitted.
///
/// Returns once the server has stopped accepting new connections (after the
/// shutdown signal drains). Any caller-specific post-shutdown work (task
/// draining, "stopped" tracing) stays in the caller.
pub async fn serve_router(router: Router, options: ServeOptions) -> Result<(), CommandError> {
    serve_router_with_bound(router, options, |_| Ok(())).await
}

/// Like [`serve_router`], but calls `on_bound` after the listener is bound and
/// before it accepts connections.  Daemons which request port `0` use this to
/// publish the kernel-selected port without a bind/close/rebind race.
pub async fn serve_router_with_bound<F>(
    router: Router,
    options: ServeOptions,
    on_bound: F,
) -> Result<(), CommandError>
where
    F: FnOnce(SocketAddr) -> Result<(), CommandError>,
{
    let ServeOptions {
        addr,
        tls,
        shutdown_grace,
    } = options;

    // Bind before publishing readiness.  In particular, this gives a daemon
    // using port 0 the actual ephemeral port selected by the OS.
    let listener = std::net::TcpListener::bind(addr).map_err(|e| bind_error(addr, e))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| CommandError::Other(format!("Failed to configure bound listener: {e}")))?;
    let bound_addr = listener
        .local_addr()
        .map_err(|e| CommandError::Other(format!("Failed to inspect bound listener: {e}")))?;
    on_bound(bound_addr)?;

    // Spawn the shutdown signal as a background task — we trigger axum-server's
    // graceful shutdown handle when it fires.
    let server_handle = axum_server::Handle::new();
    let shutdown_handle = server_handle.clone();
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut sigterm =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!("Failed to install SIGTERM handler: {e}");
                        return;
                    }
                };
            tokio::select! {
                _ = ctrl_c => { tracing::info!("Received SIGINT, shutting down"); }
                _ = sigterm.recv() => { tracing::info!("Received SIGTERM, shutting down"); }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = ctrl_c.await;
            tracing::info!("Received SIGINT, shutting down");
        }
        shutdown_handle.graceful_shutdown(Some(shutdown_grace));
    });

    let serve_result = if let Some(tls) = tls {
        let rustls_config = axum_server::tls_rustls::RustlsConfig::from_pem(
            tls.cert_pem.into_bytes(),
            tls.key_pem.into_bytes(),
        )
        .await
        .map_err(|e| CommandError::Other(format!("TLS setup: {e}")))?;
        axum_server::from_tcp_rustls(listener, rustls_config)
            .map_err(|e| CommandError::Other(format!("Server setup: {e}")))?
            .handle(server_handle.clone())
            .serve(router.into_make_service())
            .await
    } else {
        axum_server::from_tcp(listener)
            .map_err(|e| CommandError::Other(format!("Server setup: {e}")))?
            .handle(server_handle.clone())
            .serve(router.into_make_service())
            .await
    };

    serve_result.map_err(|e| CommandError::Other(format!("Server error: {e}")))?;

    Ok(())
}

fn bind_error(addr: SocketAddr, error: std::io::Error) -> CommandError {
    if let Some(io) = error.raw_os_error() {
        // Linux EADDRINUSE = 98, macOS = 48, Windows = 10048
        if matches!(io, 98 | 48 | 10048) {
            return CommandError::Other(format!(
                "Port {} is already in use. Use --port to choose a different port.",
                addr.port()
            ));
        }
    }
    if error
        .to_string()
        .to_lowercase()
        .contains("address already in use")
    {
        return CommandError::Other(format!(
            "Port {} is already in use. Use --port to choose a different port.",
            addr.port()
        ));
    }
    CommandError::Other(format!("Server error: {error}"))
}

/// Resolve the [`AuthMode`] for a daemon from its key-hash file plus a
/// skip-auth flag.
///
/// * `skip` short-circuits to [`AuthMode::Disabled`] (the
///   `--dangerously-skip-auth` path).
/// * Otherwise the daemon's `<key_stem>.hash` file must exist; its absence is
///   an error naming `refresh_hint` (e.g. `awman api start --refresh-key`) so
///   each daemon can point the user at its own refresh command.
pub fn resolve_auth_mode(
    paths: &DaemonPaths,
    skip: bool,
    refresh_hint: &str,
) -> Result<AuthMode, CommandError> {
    if skip {
        return Ok(AuthMode::Disabled);
    }
    let hash = paths
        .read_key_hash()
        .map_err(CommandError::Data)?
        .ok_or_else(|| {
            CommandError::Other(format!(
                "No API key hash on disk. Run `{refresh_hint}` to generate one."
            ))
        })?;
    Ok(AuthMode::Enabled { key_hash: hash })
}
