//! `RemoteClient` — typed HTTP client for talking to a remote awman API
//! server. Constructed fresh per `RemoteCommand` invocation; not exported
//! beyond `command/commands/`.

use std::time::Duration;

use serde::Deserialize;

use crate::command::commands::http_core::{HttpCore, HttpResponse};
use crate::command::error::CommandError;
use crate::data::execution_event::ExecutionEvent;
use crate::data::session::Session;
use crate::data::session_setup_event::SessionSetupState;
use crate::engine::auth::ApiKey;

/// Typed HTTP client for talking to a remote awman API server. A thin façade
/// over one [`HttpCore`]: `RemoteClient` owns only the route-specific methods,
/// the generic transport lives in the core.
pub struct RemoteClient {
    core: HttpCore,
}

/// Response for the low-level verb helpers. Kept as a type alias so external
/// callers referencing `remote_client::RemoteResponse` (e.g. `get_job`'s return
/// type) do not change.
pub type RemoteResponse = HttpResponse;

/// Test-only sink used by the legacy SSE parser test. Production code never
/// uses this; see `ExecutionEventSink` for the typed surface.
#[cfg(test)]
pub trait RemoteEventSink: Send + Sync {
    fn on_event(&mut self, event_type: &str, data: &str);
    fn on_done(&mut self);
}

/// Sink for typed `ExecutionEvent`s streaming over SSE from the per-job
/// `/logs` endpoint. The default impl ignores everything — callers override
/// the methods they care about. Each callback returns `bool`; returning
/// `true` from any callback ends the stream early (e.g. on Ctrl-C).
pub trait ExecutionEventSink: Send {
    fn on_event(&mut self, event: ExecutionEvent) -> bool {
        let _ = event;
        false
    }

    /// Called once when the stream terminates cleanly.
    fn on_stream_end(&mut self) {}
}

/// Request body for `POST /v1/sessions`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StartSessionRequest {
    pub session_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// `StartSession` response body.
#[derive(Debug, Clone, Deserialize)]
pub struct StartSessionResponse {
    pub session_id: String,
}

/// Exec routing argument — either a workflow path or a one-shot prompt.
#[derive(Debug, Clone)]
pub enum ExecArg {
    Workflow(String),
    Prompt(String),
}

/// Response for `POST /v1/commands`.
#[derive(Debug, Clone, Deserialize)]
pub struct ExecJobResponse {
    pub command_id: String,
    #[serde(default)]
    pub flags_applied: serde_json::Value,
}

/// Response body for `GET /v1/sessions/{id}/status`. Wraps a `SessionSetupState`
/// with the session id echoed back.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionSetupStatusResponse {
    pub session_id: String,
    #[serde(flatten)]
    pub state: SessionSetupState,
}

impl RemoteClient {
    /// The API version prefix. Hardcoded here (rather than at every call site)
    /// and threaded into the shared [`HttpCore`].
    const PREFIX: &'static str = "v1";

    pub const CONNECT_TIMEOUT: Duration = HttpCore::CONNECT_TIMEOUT;
    pub const READ_TIMEOUT: Duration = HttpCore::READ_TIMEOUT;

    pub fn new(base_url: &str, api_key: Option<&ApiKey>) -> Result<Self, CommandError> {
        Self::new_with_pinned_cert(base_url, api_key, None)
    }

    /// Construct a client that additionally trusts a specific PEM-encoded
    /// certificate. Used when talking to a loopback awman API server with
    /// a self-signed cert: the cert PEM is loaded from the local `tls/`
    /// directory and added as a trusted root, effectively pinning by identity.
    /// For non-loopback targets, the caller MUST NOT pass `pinned_cert_pem` —
    /// standard webpki verification stays in force.
    pub fn new_with_pinned_cert(
        base_url: &str,
        api_key: Option<&ApiKey>,
        pinned_cert_pem: Option<&str>,
    ) -> Result<Self, CommandError> {
        Ok(Self {
            core: HttpCore::new_with_pinned_cert(base_url, Self::PREFIX, api_key, pinned_cert_pem)?,
        })
    }

    /// Returns `true` when `addr` resolves to a loopback host (`127.0.0.1`,
    /// `::1`, `localhost`). Used to decide whether the locally-stored
    /// self-signed cert should be trusted.
    pub fn is_loopback_addr(addr: &str) -> bool {
        HttpCore::is_loopback_addr(addr)
    }

    /// API-key resolution per spec §6.5: explicit > AWMAN_API_KEY > global
    /// config (only when target_addr matches global default_addr).
    pub fn resolve_api_key(
        session: &Session,
        target_addr: &str,
        explicit: Option<&str>,
    ) -> Result<Option<ApiKey>, CommandError> {
        if let Some(explicit) = explicit {
            let trimmed = explicit.trim();
            if !trimmed.is_empty() {
                return Ok(Some(ApiKey::from_string(trimmed)));
            }
        }
        if let Some(env) = session.env().api_key() {
            let trimmed = env.trim();
            if !trimmed.is_empty() {
                return Ok(Some(ApiKey::from_string(trimmed)));
            }
        }
        // Compare canonicalized URLs against the global config default.
        let global = session.global_config();
        if let Some(remote) = global.remote.as_ref() {
            if let (Some(default_addr), Some(default_key)) = (
                remote.default_addr.as_deref(),
                remote.default_api_key.as_deref(),
            ) {
                if canonicalize_url(target_addr) == canonicalize_url(default_addr) {
                    return Ok(Some(ApiKey::from_string(default_key)));
                }
            }
        }
        Ok(None)
    }

    // ─── Typed methods (preferred public surface) ────────────────────────────

    /// `POST /v1/sessions` — request session creation. Returns the new session
    /// id; setup runs asynchronously and the session is not ready for jobs
    /// until its `/status` endpoint returns `"ready"`.
    pub async fn start_session(
        &self,
        req: &StartSessionRequest,
    ) -> Result<StartSessionResponse, CommandError> {
        let url = self.core.url(&["sessions"]);
        let resp = self
            .core
            .http()
            .post(&url)
            .json(req)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;
        let status = resp.status().as_u16();
        let body = resp
            .json::<serde_json::Value>()
            .await
            .map_err(Self::map_reqwest_error)?;
        if status >= 400 {
            return Err(CommandError::RemoteHttpStatus {
                status,
                body: body.to_string(),
            });
        }
        serde_json::from_value::<StartSessionResponse>(body)
            .map_err(|e| CommandError::Other(format!("invalid start-session response: {e}")))
    }

    /// `DELETE /v1/sessions/{id}` — kill a session.
    pub async fn kill_session(&self, session_id: &str) -> Result<(), CommandError> {
        let _ = self.delete(&["sessions", session_id]).await?;
        Ok(())
    }

    /// `GET /v1/sessions/{id}/status` — fetch the deserialized setup state.
    pub async fn get_session_status(
        &self,
        session_id: &str,
    ) -> Result<SessionSetupStatusResponse, CommandError> {
        let resp = self.get(&["sessions", session_id, "status"]).await?;
        serde_json::from_value::<SessionSetupStatusResponse>(resp.body)
            .map_err(|e| CommandError::Other(format!("invalid session-status response: {e}")))
    }

    /// Submit an exec-prompt or exec-workflow job. Returns the command id.
    pub async fn exec_job(
        &self,
        session_id: &str,
        exec: ExecArg,
        extra_args: &[String],
    ) -> Result<ExecJobResponse, CommandError> {
        let (subcommand, mut args) = match exec {
            ExecArg::Workflow(path) => ("exec workflow", vec![path]),
            ExecArg::Prompt(text) => ("exec prompt", vec![text]),
        };
        args.extend(extra_args.iter().cloned());

        let resp = self
            .send_command_with_headers(
                &["commands"],
                &[
                    ("subcommand", serde_json::json!(subcommand)),
                    (
                        "args",
                        serde_json::json!(args
                            .iter()
                            .map(|s| serde_json::json!(s))
                            .collect::<Vec<_>>()),
                    ),
                ],
                &[("x-awman-session", session_id)],
            )
            .await?;

        serde_json::from_value::<ExecJobResponse>(resp.body)
            .map_err(|e| CommandError::Other(format!("invalid exec response: {e}")))
    }

    /// `GET /v1/commands/{id}/status` — fetch a job's metadata.
    pub async fn get_job(&self, command_id: &str) -> Result<RemoteResponse, CommandError> {
        self.get(&["commands", command_id, "status"]).await
    }

    /// `GET /v1/workflows/{id}` — fetch the workflow state JSON for a job.
    /// Returns `None` on HTTP 404 (job is a prompt job or pending).
    pub async fn get_workflow_state(
        &self,
        command_id: &str,
    ) -> Result<Option<serde_json::Value>, CommandError> {
        match self.get(&["workflows", command_id]).await {
            Ok(resp) => Ok(Some(resp.body)),
            Err(CommandError::RemoteHttpStatus { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `GET /v1/commands/{id}/logs` (SSE) — stream typed
    /// `ExecutionEvent` values to the sink. Terminates when the server sends
    /// a `Done` event or the sink returns `true` from any callback.
    pub async fn stream_job_logs(
        &self,
        _session_id: &str,
        job_id: &str,
        sink: &mut dyn ExecutionEventSink,
    ) -> Result<(), CommandError> {
        use crate::data::execution_event::EventPayload;
        use futures_util::StreamExt;

        let url = self.core.url(&["commands", job_id, "logs"]);

        let resp = self
            .core
            .http()
            .get(&url)
            .timeout(Duration::from_secs(86400))
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;
        if resp.status().as_u16() >= 400 {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(CommandError::RemoteHttpStatus { status, body });
        }

        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk_res) = stream.next().await {
            let chunk = chunk_res.map_err(|e| CommandError::RemoteTransport(e.to_string()))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(pos) = buffer.find("\n\n") {
                let block: String = buffer.drain(..pos + 2).collect();
                let trimmed = block.trim_end_matches('\n');
                if trimmed.is_empty() {
                    continue;
                }
                // SSE comment lines start with `:` — surface as a sink hook
                // by ignoring them here.
                let mut data_lines: Vec<&str> = Vec::new();
                for line in trimmed.lines() {
                    if let Some(rest) = line.strip_prefix("data: ") {
                        data_lines.push(rest);
                    } else if let Some(rest) = line.strip_prefix("data:") {
                        data_lines.push(rest);
                    }
                    // event: and : (comment) lines are ignored — the typed
                    // payload already includes the event kind.
                }
                let data = data_lines.join("\n");
                if data.is_empty() {
                    continue;
                }
                let event: ExecutionEvent = match serde_json::from_str(&data) {
                    Ok(e) => e,
                    Err(_) => continue, // skip malformed lines
                };
                let is_done = matches!(event.payload, EventPayload::Done);
                if sink.on_event(event) {
                    sink.on_stream_end();
                    return Ok(());
                }
                if is_done {
                    sink.on_stream_end();
                    return Ok(());
                }
            }
        }

        sink.on_stream_end();
        Ok(())
    }

    // ─── Generic low-level helpers (crate-private) ───────────────────────────

    pub(crate) async fn send_command(
        &self,
        path: &[&str],
        flags: &[(&str, serde_json::Value)],
    ) -> Result<RemoteResponse, CommandError> {
        self.core.post_command(path, flags, &[]).await
    }

    /// Like `send_command` but also attaches request headers — used to set
    /// `x-awman-session` on `POST /v1/commands` (the server reads the session
    /// from the header, not the body).
    pub(crate) async fn send_command_with_headers(
        &self,
        path: &[&str],
        flags: &[(&str, serde_json::Value)],
        headers: &[(&str, &str)],
    ) -> Result<RemoteResponse, CommandError> {
        self.core.post_command(path, flags, headers).await
    }

    pub(crate) async fn get(&self, path: &[&str]) -> Result<RemoteResponse, CommandError> {
        self.core.get(path).await
    }

    pub(crate) async fn delete(&self, path: &[&str]) -> Result<RemoteResponse, CommandError> {
        self.core.delete(path).await
    }

    /// Stream raw SSE events to the given sink. Kept crate-private for tests
    /// of the SSE parser; production code should use `stream_job_logs`.
    #[cfg(test)]
    pub(crate) async fn stream_command_legacy(
        &self,
        path: &[&str],
        _flags: &[(&str, serde_json::Value)],
        sink: &mut dyn RemoteEventSink,
    ) -> Result<(), CommandError> {
        use futures_util::StreamExt;

        let url = self.core.url(path);

        let resp = self
            .core
            .http()
            .get(&url)
            .timeout(Duration::from_secs(86400))
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        if resp.status().as_u16() >= 400 {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(CommandError::RemoteHttpStatus { status, body });
        }

        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk_res) = stream.next().await {
            let chunk = chunk_res.map_err(|e| CommandError::RemoteTransport(e.to_string()))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // Pull every complete `\n\n`-delimited event block out of the buffer
            // and dispatch it. Whatever's left after the final separator stays
            // in the buffer until more bytes arrive.
            while let Some(pos) = buffer.find("\n\n") {
                let event_block = buffer[..pos].to_string();
                buffer.drain(..pos + 2);
                if Self::dispatch_sse_event(&event_block, sink) {
                    return Ok(());
                }
            }
        }

        // Stream ended without [awman:done] — emit any partial event then close.
        if !buffer.trim().is_empty() {
            let trailing = std::mem::take(&mut buffer);
            if Self::dispatch_sse_event(&trailing, sink) {
                return Ok(());
            }
        }
        sink.on_done();
        Ok(())
    }

    /// Parse one `\n\n`-delimited SSE event block and forward it to the sink.
    /// Returns `true` when the block was the `[awman:done]` sentinel (caller
    /// should stop streaming).
    #[cfg(test)]
    fn dispatch_sse_event(block: &str, sink: &mut dyn RemoteEventSink) -> bool {
        if block.trim().is_empty() {
            return false;
        }
        let mut event_type = "message";
        let mut data_lines: Vec<&str> = Vec::new();
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("event: ") {
                event_type = rest;
            } else if let Some(rest) = line.strip_prefix("event:") {
                event_type = rest;
            } else if let Some(rest) = line.strip_prefix("data: ") {
                data_lines.push(rest);
            } else if let Some(rest) = line.strip_prefix("data:") {
                data_lines.push(rest);
            }
        }
        let data = data_lines.join("\n");
        if data == "[awman:done]" {
            sink.on_done();
            return true;
        }
        sink.on_event(event_type, &data);
        false
    }

    pub fn map_reqwest_error(e: reqwest::Error) -> CommandError {
        HttpCore::map_reqwest_error(e)
    }
}

/// Canonicalize a URL for the default-addr comparison rule (§6.5):
///   - lowercase scheme
///   - lowercase host
///   - elide default ports (80/http, 443/https)
///   - normalize trailing slash
fn canonicalize_url(s: &str) -> String {
    let s = s.trim();
    let (scheme_part, rest) = match s.split_once("://") {
        Some(t) => t,
        None => return s.to_lowercase(),
    };
    let scheme = scheme_part.to_lowercase();
    let (host_part, path_part) = match rest.split_once('/') {
        Some((h, p)) => (h, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match host_part.split_once(':') {
        Some((h, p)) => (h.to_lowercase(), Some(p.to_string())),
        None => (host_part.to_lowercase(), None),
    };
    let port_render = match (scheme.as_str(), port.as_deref()) {
        ("http", Some("80")) | ("https", Some("443")) | (_, None) => String::new(),
        (_, Some(p)) => format!(":{p}"),
    };
    let path_render = if path_part == "/" {
        ""
    } else {
        path_part.as_str()
    };
    format!("{scheme}://{host}{port_render}{path_render}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::env::EnvSnapshot;
    use crate::data::session::{Session, SessionOpenOptions};

    // ─── is_loopback_addr ─────────────────────────────────────────────────────

    #[test]
    fn loopback_addr_recognizes_ipv4_and_ipv6_and_localhost() {
        assert!(RemoteClient::is_loopback_addr("https://127.0.0.1:9876"));
        assert!(RemoteClient::is_loopback_addr("http://127.0.0.1:9876/"));
        assert!(RemoteClient::is_loopback_addr("https://localhost"));
        assert!(RemoteClient::is_loopback_addr("https://[::1]:9876"));
    }

    #[test]
    fn loopback_addr_rejects_remote_hosts() {
        assert!(!RemoteClient::is_loopback_addr("https://example.com:9876"));
        assert!(!RemoteClient::is_loopback_addr("http://10.0.0.1"));
        assert!(!RemoteClient::is_loopback_addr("https://my-host"));
    }

    // ─── URL canonicalize helpers ─────────────────────────────────────────────

    #[test]
    fn url_canonicalize_default_port_elided() {
        assert_eq!(canonicalize_url("http://1.2.3.4:80/"), "http://1.2.3.4");
        assert_eq!(
            canonicalize_url("https://example.com:443/"),
            "https://example.com"
        );
    }

    #[test]
    fn url_canonicalize_case_insensitive_scheme_and_host() {
        assert_eq!(
            canonicalize_url("HTTP://Example.COM/"),
            "http://example.com"
        );
    }

    #[test]
    fn url_canonicalize_distinguishes_schemes() {
        assert_ne!(
            canonicalize_url("https://example.com/"),
            canonicalize_url("http://example.com/"),
        );
    }

    // ─── Test-session helpers ─────────────────────────────────────────────────

    fn make_session(env: EnvSnapshot) -> (tempfile::TempDir, Session) {
        let tmp = tempfile::tempdir().unwrap();
        // Pin `AWMAN_CONFIG_HOME` at an empty tempdir so the session can't
        // fall through to the developer's real `~/.awman/config.json` (which
        // on a working machine may legitimately have `remote` configured and
        // would invalidate the "no source" / "addr mismatch" assertions).
        // All current callers pass either `EnvSnapshot::empty()` or a snapshot
        // with `AWMAN_API_KEY` only, so we rebuild the snapshot with the
        // config-home pin added on top of those known keys.
        let mut entries: Vec<(String, String)> = Vec::new();
        for key in [
            "AWMAN_API_KEY",
            "AWMAN_REMOTE_ADDR",
            "AWMAN_REMOTE_SESSION",
            "AWMAN_OVERLAYS",
            "AWMAN_API_ROOT",
        ] {
            if let Some(v) = env.get(key) {
                entries.push((key.to_string(), v.to_string()));
            }
        }
        entries.push((
            "AWMAN_CONFIG_HOME".to_string(),
            tmp.path().to_str().unwrap().to_string(),
        ));
        let opts = SessionOpenOptions {
            env: Some(EnvSnapshot::with_overrides(entries)),
            ..Default::default()
        };
        let session =
            Session::open_at_git_root(tmp.path().to_path_buf(), tmp.path().to_path_buf(), opts)
                .unwrap();
        (tmp, session)
    }

    fn make_session_with_global_config(config_json: &str) -> (tempfile::TempDir, Session) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("config.json"), config_json).unwrap();
        let env =
            EnvSnapshot::with_overrides([("AWMAN_CONFIG_HOME", tmp.path().to_str().unwrap())]);
        let opts = SessionOpenOptions {
            env: Some(env),
            ..Default::default()
        };
        let session =
            Session::open_at_git_root(tmp.path().to_path_buf(), tmp.path().to_path_buf(), opts)
                .unwrap();
        (tmp, session)
    }

    // ─── resolve_api_key tests ────────────────────────────────────────────────

    #[test]
    fn resolve_api_key_explicit_takes_priority_over_env_and_config() {
        let env = EnvSnapshot::with_overrides([("AWMAN_API_KEY", "env-key")]);
        let (_tmp, session) = make_session(env);
        let result =
            RemoteClient::resolve_api_key(&session, "http://localhost:9876", Some("explicit-key"));
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap().unwrap().as_str(),
            "explicit-key",
            "explicit key must win over env"
        );
    }

    #[test]
    fn resolve_api_key_env_var_used_when_no_explicit() {
        let env = EnvSnapshot::with_overrides([("AWMAN_API_KEY", "env-key")]);
        let (_tmp, session) = make_session(env);
        let result = RemoteClient::resolve_api_key(&session, "http://localhost:9876", None);
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap().unwrap().as_str(),
            "env-key",
            "env var must be used when no explicit key"
        );
    }

    #[test]
    fn resolve_api_key_global_config_matched_by_default_addr() {
        let config_json =
            r#"{"remote":{"defaultAddr":"http://localhost:9876","defaultAPIKey":"config-key"}}"#;
        let (_tmp, session) = make_session_with_global_config(config_json);
        let result = RemoteClient::resolve_api_key(&session, "http://localhost:9876", None);
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap().unwrap().as_str(),
            "config-key",
            "global config key must be returned when target_addr matches default_addr"
        );
    }

    #[test]
    fn resolve_api_key_global_config_not_used_when_addr_differs() {
        let config_json =
            r#"{"remote":{"defaultAddr":"http://other-host:9876","defaultAPIKey":"config-key"}}"#;
        let (_tmp, session) = make_session_with_global_config(config_json);
        let result = RemoteClient::resolve_api_key(&session, "http://localhost:9876", None);
        assert!(result.is_ok());
        assert!(
            result.unwrap().is_none(),
            "config key must NOT be returned when addr does not match"
        );
    }

    #[test]
    fn resolve_api_key_returns_none_when_no_source_available() {
        let (_tmp, session) = make_session(EnvSnapshot::empty());
        let result = RemoteClient::resolve_api_key(&session, "http://localhost:9876", None);
        assert!(result.is_ok());
        assert!(
            result.unwrap().is_none(),
            "must return None when no key source exists"
        );
    }

    #[test]
    fn resolve_api_key_explicit_blank_falls_through_to_env() {
        let env = EnvSnapshot::with_overrides([("AWMAN_API_KEY", "env-key")]);
        let (_tmp, session) = make_session(env);
        // An explicit empty string should fall through to env.
        let result = RemoteClient::resolve_api_key(&session, "http://localhost:9876", Some("   "));
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap().unwrap().as_str(),
            "env-key",
            "blank explicit key must fall through to env"
        );
    }

    // ─── send_command tests (mock HTTP server) ────────────────────────────────

    #[tokio::test]
    async fn send_command_200_response_returns_parsed_remote_response() {
        use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/v1/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let client = RemoteClient::new(&server.uri(), None).unwrap();
        let result = client.send_command(&["status"], &[]).await;
        assert!(result.is_ok(), "200 must return Ok: {result:?}");
        let response = result.unwrap();
        assert_eq!(response.status, 200);
    }

    #[tokio::test]
    async fn send_command_400_response_maps_to_remote_http_status_error() {
        use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/v1/exec/workflow"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({"error": "bad request"})),
            )
            .mount(&server)
            .await;

        let client = RemoteClient::new(&server.uri(), None).unwrap();
        let result = client.send_command(&["exec", "workflow"], &[]).await;
        assert!(
            matches!(
                result,
                Err(CommandError::RemoteHttpStatus { status: 400, .. })
            ),
            "400 must map to RemoteHttpStatus, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn send_command_500_response_maps_to_remote_http_status_error() {
        use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/v1/status"))
            .respond_with(
                ResponseTemplate::new(500)
                    .set_body_json(serde_json::json!({"error": "internal server error"})),
            )
            .mount(&server)
            .await;

        let client = RemoteClient::new(&server.uri(), None).unwrap();
        let result = client.send_command(&["status"], &[]).await;
        assert!(
            matches!(
                result,
                Err(CommandError::RemoteHttpStatus { status: 500, .. })
            ),
            "500 must map to RemoteHttpStatus, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn stream_command_parses_sse_events_and_calls_sink() {
        use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

        let sse_body = "data: hello world\n\ndata: second line\n\ndata: [awman:done]\n\n";

        let server = MockServer::start().await;
        Mock::given(matchers::method("GET"))
            .and(matchers::path("/v1/commands/cmd-1/logs/stream"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_body),
            )
            .mount(&server)
            .await;

        let client = RemoteClient::new(&server.uri(), None).unwrap();

        struct CollectSink {
            events: Vec<(String, String)>,
            done: bool,
        }
        impl RemoteEventSink for CollectSink {
            fn on_event(&mut self, event_type: &str, data: &str) {
                self.events.push((event_type.to_string(), data.to_string()));
            }
            fn on_done(&mut self) {
                self.done = true;
            }
        }

        let mut sink = CollectSink {
            events: Vec::new(),
            done: false,
        };
        let result = client
            .stream_command_legacy(&["commands", "cmd-1", "logs", "stream"], &[], &mut sink)
            .await;
        assert!(result.is_ok(), "stream_command should succeed: {result:?}");
        assert!(sink.done, "on_done must be called");
        assert_eq!(sink.events.len(), 2);
        assert_eq!(sink.events[0].1, "hello world");
        assert_eq!(sink.events[1].1, "second line");
    }

    #[tokio::test]
    async fn map_reqwest_error_connection_refused_maps_to_remote_connection_refused() {
        // Port 1 is reserved and should never have anything listening.
        let client = RemoteClient::new("http://127.0.0.1:1", None).unwrap();
        let result = client.send_command(&["status"], &[]).await;
        assert!(
            matches!(result, Err(CommandError::RemoteConnectionRefused(_))),
            "connection refused must map to RemoteConnectionRefused, got: {result:?}"
        );
    }
}
