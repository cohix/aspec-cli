//! Generic, agent-agnostic credential model for refreshable file-delivered
//! credentials.
//!
//! This module defines the types the whole WI-0107 credential-refresh pipeline
//! agrees on — a redacting [`SecretString`], a refresh-token-free
//! [`CredentialSnapshot`], the [`CredentialFile`] planted into a staged settings
//! overlay, and the per-agent [`RefreshableCredentialSpec`] descriptor that ties
//! reading, expiry, materialization and host-refresh together. Claude is the
//! only agent with a descriptor today.
//!
//! ## Security invariants enforced here
//!
//! - **INV-1** — the parser deserializes into [`ClaudeCredentialPayload`], a
//!   struct with *no field* for `refreshToken` / `refreshTokenExpiresAt`, so no
//!   code path can carry them downstream. We never parse into a
//!   `serde_json::Value` that could transiently hold the refresh token.
//! - **INV-5** — [`SecretString`] implements neither `Display` nor `Serialize`
//!   and redacts its `Debug`; the only reader is [`SecretString::expose`], and
//!   [`CredentialFile`]'s `Debug` prints path/mode/length only.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ring::digest;
use serde::{Deserialize, Serialize};

use crate::data::fs::auth_paths::AuthPathResolver;

/// macOS Keychain generic-password service name for the Claude Code credential.
pub const CLAUDE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// Wrapper that keeps a credential value out of logs, `Debug` output and
/// serialized payloads. Deliberately implements neither `Display` nor
/// `Serialize`; the only way to reach the bytes is [`SecretString::expose`].
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The only accessor. Every call site is a place a reviewer must check.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    /// Renders exactly `SecretString(<redacted>)` — never the value, never a
    /// prefix of the value, never its length.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}

/// Agent-specific materialization inputs. Claude is the only consumer in this
/// work item; a second agent's fields would join here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CredentialExtra {
    /// `claudeAiOauth.scopes`, verbatim, when present.
    pub scopes: Vec<String>,
    /// `claudeAiOauth.subscriptionType`, verbatim, when present.
    pub subscription_type: Option<String>,
}

/// A point-in-time read of the host credential for one agent.
///
/// INVARIANT (INV-1): this struct has **no field** that can hold a refresh
/// token or `refreshTokenExpiresAt`, and the parser that produces it
/// deserializes into a struct with no such field either — so no code path,
/// present or future, can carry them downstream.
#[derive(Clone, Debug)]
pub struct CredentialSnapshot {
    /// The access token. Never the refresh token.
    pub secret: SecretString,
    /// Absolute expiry of `secret`, when the host payload states one.
    pub expires_at: Option<SystemTime>,
    /// Agent-specific fields needed only to re-render the container file.
    pub extra: CredentialExtra,
}

/// Stable, non-reversible identity of a snapshot, for change detection and
/// logging. Not derivable back to the secret: SHA-256 over
/// (secret bytes || expiry) truncated to 8 hex chars.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CredentialFingerprint([u8; 4]);

impl CredentialFingerprint {
    pub fn of(snapshot: &CredentialSnapshot) -> Self {
        let mut ctx = digest::Context::new(&digest::SHA256);
        ctx.update(snapshot.secret.expose().as_bytes());
        if let Some(ms) = snapshot.expires_at.and_then(system_time_to_ms) {
            ctx.update(&ms.to_le_bytes());
        }
        let d = ctx.finish();
        let mut out = [0u8; 4];
        out.copy_from_slice(&d.as_ref()[..4]);
        Self(out)
    }

    /// A fixed all-zero fingerprint, used as a placeholder on the
    /// discriminant-only [`super::RefreshableCredentialDelivery`] the auth layer
    /// produces before staging assigns a real one.
    pub fn zeroed() -> Self {
        Self([0u8; 4])
    }

    /// 8 lowercase hex chars. This is the ONLY form allowed in logs.
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(8);
        for b in self.0 {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
        }
        s
    }
}

/// Why a host credential read failed. Replaces the silent empty-vec returns in
/// `keychain.rs`; surfaced through `awman ready` and the monitor's warnings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialReadError {
    /// Platform has no keychain integration for this agent.
    Unsupported,
    /// No keychain entry / no host credential file.
    NotFound,
    /// Entry present but the payload did not parse.
    Malformed,
    /// Read failed for an OS reason (locked keychain, permission, I/O).
    Unavailable(String),
}

impl std::fmt::Display for CredentialReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => f.write_str("no keychain integration on this platform"),
            Self::NotFound => f.write_str("no host credential found"),
            Self::Malformed => f.write_str("host credential payload did not parse"),
            Self::Unavailable(why) => write!(f, "host credential unavailable: {why}"),
        }
    }
}

/// A credential rendered for planting into an agent's staged settings dir.
///
/// Structurally identical to [`super::keychain::AgentSecretFile`], which is
/// reused for the *create* path. This type is separate because it is produced
/// by a descriptor and rewritten in place by the monitor, whereas
/// `AgentSecretFile` is a one-shot staging artifact.
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialFile {
    /// Path RELATIVE to the agent's staged settings dir. For Claude this is
    /// exactly `.credentials.json` (joined with the staged `~/.claude`), NOT
    /// `.claude/.credentials.json` — the staged root already IS `~/.claude`.
    pub relative_path: PathBuf,
    /// Rendered file bytes.
    pub contents: Vec<u8>,
    /// Unix mode. MUST be `0o600` for every credential file (INV-3).
    pub mode: u32,
}

impl std::fmt::Debug for CredentialFile {
    /// Prints path, mode and `contents.len()` only — never the bytes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialFile")
            .field("relative_path", &self.relative_path)
            .field("mode", &format_args!("{:#o}", self.mode))
            .field("contents_len", &self.contents.len())
            .finish()
    }
}

/// Where a host credential is read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostCredentialSource {
    /// macOS Keychain generic password. For Claude: service
    /// `"Claude Code-credentials"`, no account.
    MacosKeychain { service: &'static str },
    /// A host file. For Claude on non-macOS: the path resolved from
    /// `AuthPathResolver::resolve("claude").settings_dir` joined with
    /// `.credentials.json`. READ ONLY — this path is never mounted (INV-2).
    HostFile { path: PathBuf },
}

/// Where one process should read host credentials from.
///
/// Production binds only a home directory and lets each descriptor pick the
/// platform's native store — for Claude that is the Keychain on macOS and
/// `~/.claude/.credentials.json` everywhere else.
///
/// The reason this type exists rather than every caller invoking
/// `(spec.source)(resolver)` directly: [`claude_source`] branches on
/// `cfg!(target_os = "macos")` and, on that branch, ignores the resolver
/// entirely. A caller that rebinds the home directory to isolate itself
/// therefore isolates nothing on macOS and silently reads the developer's real
/// Keychain. Binding the source explicitly with [`CredentialBinding::to_file`]
/// makes that isolation hold on every platform instead of on two out of three.
///
/// [`claude_source`]: fn@claude_source
#[derive(Clone, Debug)]
pub struct CredentialBinding {
    resolver: AuthPathResolver,
    /// When set, used verbatim in place of the descriptor's platform default.
    explicit: Option<HostCredentialSource>,
}

impl CredentialBinding {
    /// Bind a home directory and let each descriptor choose the platform's
    /// native credential store. The production constructor.
    pub fn platform_default(resolver: AuthPathResolver) -> Self {
        Self {
            resolver,
            explicit: None,
        }
    }

    /// Bind an explicit host credential FILE, overriding the platform default.
    ///
    /// This is what makes a temp-HOME fixture mean the same thing on macOS as
    /// it does on Linux. It reads the file and never mounts it (INV-2), exactly
    /// as the non-macOS platform default does.
    pub fn to_file(resolver: AuthPathResolver, path: impl Into<PathBuf>) -> Self {
        Self {
            resolver,
            explicit: Some(HostCredentialSource::HostFile { path: path.into() }),
        }
    }

    /// The home directory this binding resolves agent paths against.
    pub fn resolver(&self) -> &AuthPathResolver {
        &self.resolver
    }

    /// The source `spec` should read from under this binding.
    pub fn source_for(&self, spec: &RefreshableCredentialSpec) -> HostCredentialSource {
        match &self.explicit {
            Some(source) => source.clone(),
            None => (spec.source)(&self.resolver),
        }
    }
}

/// How to cause the HOST to rotate its own credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostRefreshAction {
    /// Run the sanctioned `ready` local-agent ping for this agent. The ONLY
    /// variant in this work item, and the only host-side agent execution awman
    /// is permitted (INV-8).
    ReadyCheckPing { agent: &'static str },
}

/// One agent's credential-refresh descriptor. Claude is the only instance.
///
/// Deliberately a plain struct of function pointers, not a trait: there is
/// exactly one implementor and a `&'static` struct keeps the monitor free of
/// generics and trait objects.
pub struct RefreshableCredentialSpec {
    /// Agent name this descriptor applies to, e.g. `"claude"`.
    pub agent: &'static str,

    /// The env-var name this credential would occupy under legacy env delivery
    /// (`"CLAUDE_CODE_OAUTH_TOKEN"`). Used ONLY to reuse the existing
    /// [`super::service_for_credential`] mapping so the dedup rule suppresses
    /// the credential FILE exactly as it suppresses the env var.
    pub credential_env_key: &'static str,

    /// Resolve this host's credential source for the current platform.
    pub source: fn(&AuthPathResolver) -> HostCredentialSource,

    /// Read the current host credential.
    pub read: fn(&HostCredentialSource) -> Result<CredentialSnapshot, CredentialReadError>,

    /// Extract the expiry. Separate from `read` so callers that only need
    /// time-to-expiry never hold a secret.
    pub expiry: fn(&CredentialSnapshot) -> Option<SystemTime>,

    /// Render the credential into the agent's staged settings overlay.
    pub materialize: fn(&CredentialSnapshot) -> CredentialFile,

    /// How to make the host rotate its credential.
    pub host_refresh: fn() -> HostRefreshAction,

    /// Does this tail of container output look like an auth failure? For Claude:
    /// case-insensitive match on `401`, `OAuth access token`, or
    /// `Failed to authenticate`. Lives here, not in the workflow engine, so
    /// adding an agent never touches the engine.
    pub is_auth_failure: fn(&str) -> bool,
}

/// The single Claude descriptor.
pub fn claude_spec() -> &'static RefreshableCredentialSpec {
    &CLAUDE_SPEC
}

static CLAUDE_SPEC: RefreshableCredentialSpec = RefreshableCredentialSpec {
    agent: "claude",
    credential_env_key: "CLAUDE_CODE_OAUTH_TOKEN",
    source: claude_source,
    read: read_claude_credential,
    expiry: claude_expiry,
    materialize: claude_materialize,
    host_refresh: claude_host_refresh,
    is_auth_failure: claude_is_auth_failure,
};

// ── Claude descriptor implementation ────────────────────────────────────────

/// macOS reads the Keychain; every other platform reads
/// `~/.claude/.credentials.json` (the SOURCE is generalized, the keychain code
/// is not — INV-2 corollary). The host file is READ, never mounted.
fn claude_source(resolver: &AuthPathResolver) -> HostCredentialSource {
    if cfg!(target_os = "macos") {
        HostCredentialSource::MacosKeychain {
            service: CLAUDE_KEYCHAIN_SERVICE,
        }
    } else {
        let settings_dir = resolver
            .resolve("claude")
            .settings_dir
            .unwrap_or_else(|| resolver.home().join(".claude"));
        HostCredentialSource::HostFile {
            path: settings_dir.join(".credentials.json"),
        }
    }
}

/// Read and parse the Claude host credential for the given source.
///
/// The parse deserializes into [`ClaudeCredentialPayload`], which has no field
/// for the refresh token, so it is dropped by construction (INV-1).
pub(crate) fn read_claude_credential(
    source: &HostCredentialSource,
) -> Result<CredentialSnapshot, CredentialReadError> {
    let raw = match source {
        HostCredentialSource::MacosKeychain { service } => {
            if !cfg!(target_os = "macos") {
                return Err(CredentialReadError::Unsupported);
            }
            super::keychain::run_macos_keychain_lookup(service, None)
                .ok_or(CredentialReadError::NotFound)?
        }
        HostCredentialSource::HostFile { path } => match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(CredentialReadError::NotFound)
            }
            Err(e) => return Err(CredentialReadError::Unavailable(e.to_string())),
        },
    };
    parse_claude_payload(&raw)
}

fn parse_claude_payload(raw: &str) -> Result<CredentialSnapshot, CredentialReadError> {
    let payload: ClaudeCredentialPayload =
        serde_json::from_str(raw).map_err(|_| CredentialReadError::Malformed)?;
    let oauth = payload.claude_ai_oauth;
    Ok(CredentialSnapshot {
        secret: SecretString::new(oauth.access_token),
        expires_at: oauth.expires_at.and_then(ms_to_system_time),
        extra: CredentialExtra {
            scopes: oauth.scopes,
            subscription_type: oauth.subscription_type,
        },
    })
}

fn claude_expiry(snapshot: &CredentialSnapshot) -> Option<SystemTime> {
    snapshot.expires_at
}

/// Render the refresh-token-free `~/.claude/.credentials.json` (INV-4). Omits
/// `scopes` / `subscriptionType` when the host payload did not carry them.
fn claude_materialize(snapshot: &CredentialSnapshot) -> CredentialFile {
    let rendered = RenderedPayload {
        claude_ai_oauth: RenderedOauth {
            access_token: snapshot.secret.expose(),
            expires_at: snapshot.expires_at.and_then(system_time_to_ms),
            scopes: &snapshot.extra.scopes,
            subscription_type: snapshot.extra.subscription_type.as_deref(),
        },
    };
    // Serialization cannot fail: every field is a plain string/number/array.
    let contents = serde_json::to_vec(&rendered).unwrap_or_default();
    CredentialFile {
        relative_path: PathBuf::from(".credentials.json"),
        contents,
        mode: 0o600,
    }
}

fn claude_host_refresh() -> HostRefreshAction {
    HostRefreshAction::ReadyCheckPing { agent: "claude" }
}

fn claude_is_auth_failure(tail: &str) -> bool {
    let lower = tail.to_lowercase();
    lower.contains("401")
        || lower.contains("oauth access token")
        || lower.contains("failed to authenticate")
}

// ── Wire shapes ─────────────────────────────────────────────────────────────

/// Parsed host payload. Has NO field for `refreshToken` /
/// `refreshTokenExpiresAt`; serde ignores unknown keys, so those never enter
/// the process's memory as a typed value (INV-1).
#[derive(Deserialize)]
struct ClaudeCredentialPayload {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: ClaudeOauth,
}

#[derive(Deserialize)]
struct ClaudeOauth {
    #[serde(rename = "accessToken")]
    access_token: String,
    /// Milliseconds since the Unix epoch, matching what Claude Code writes.
    #[serde(rename = "expiresAt", default)]
    expires_at: Option<i64>,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(rename = "subscriptionType", default)]
    subscription_type: Option<String>,
}

#[derive(Serialize)]
struct RenderedPayload<'a> {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: RenderedOauth<'a>,
}

#[derive(Serialize)]
struct RenderedOauth<'a> {
    #[serde(rename = "accessToken")]
    access_token: &'a str,
    #[serde(rename = "expiresAt", skip_serializing_if = "Option::is_none")]
    expires_at: Option<i64>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    scopes: &'a [String],
    #[serde(rename = "subscriptionType", skip_serializing_if = "Option::is_none")]
    subscription_type: Option<&'a str>,
}

// ── Time helpers ────────────────────────────────────────────────────────────

fn ms_to_system_time(ms: i64) -> Option<SystemTime> {
    if ms < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_millis(ms as u64))
}

fn system_time_to_ms(t: SystemTime) -> Option<i64> {
    t.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "claudeAiOauth": {
            "accessToken": "sk-ant-oat-ACCESS",
            "refreshToken": "sk-ant-ort-REFRESH-SENTINEL",
            "refreshTokenExpiresAt": 1799999999000,
            "expiresAt": 1750000000000,
            "scopes": ["user:inference", "user:profile"],
            "subscriptionType": "max"
        }
    }"#;

    #[test]
    fn parse_captures_secret_and_expiry_and_drops_refresh_token() {
        let snap = parse_claude_payload(SAMPLE).expect("parse");
        assert_eq!(snap.secret.expose(), "sk-ant-oat-ACCESS");
        assert_eq!(
            snap.expires_at.and_then(system_time_to_ms),
            Some(1_750_000_000_000)
        );
        assert_eq!(snap.extra.subscription_type.as_deref(), Some("max"));
        // The refresh token appears nowhere reachable from the snapshot.
        let dbg = format!("{snap:?}");
        assert!(
            !dbg.contains("REFRESH-SENTINEL"),
            "refresh token leaked into Debug: {dbg}"
        );
        assert!(
            !dbg.contains("sk-ant-oat-ACCESS"),
            "secret leaked into Debug"
        );
    }

    #[test]
    fn materialized_file_has_no_refresh_token_and_is_0600() {
        let snap = parse_claude_payload(SAMPLE).expect("parse");
        let file = claude_materialize(&snap);
        assert_eq!(file.mode, 0o600);
        assert_eq!(file.relative_path, PathBuf::from(".credentials.json"));
        let text = String::from_utf8(file.contents.clone()).unwrap();
        assert!(text.contains("sk-ant-oat-ACCESS"));
        assert!(text.contains("\"expiresAt\":1750000000000"));
        assert!(!text.contains("refreshToken"), "rendered file: {text}");
        assert!(!text.contains("REFRESH-SENTINEL"));
    }

    #[test]
    fn materialize_omits_absent_optional_fields() {
        let minimal = r#"{"claudeAiOauth":{"accessToken":"tok"}}"#;
        let snap = parse_claude_payload(minimal).expect("parse");
        let file = claude_materialize(&snap);
        let text = String::from_utf8(file.contents).unwrap();
        assert!(!text.contains("scopes"), "{text}");
        assert!(!text.contains("subscriptionType"), "{text}");
        assert!(!text.contains("expiresAt"), "{text}");
    }

    #[test]
    fn malformed_payload_is_typed_error() {
        assert!(matches!(
            parse_claude_payload("not json"),
            Err(CredentialReadError::Malformed)
        ));
        assert!(matches!(
            parse_claude_payload("{}"),
            Err(CredentialReadError::Malformed)
        ));
    }

    #[test]
    fn secret_debug_is_redacted() {
        let s = SecretString::new("super-secret");
        assert_eq!(format!("{s:?}"), "SecretString(<redacted>)");
    }

    #[test]
    fn fingerprint_is_eight_hex_chars_and_stable() {
        let snap = parse_claude_payload(SAMPLE).expect("parse");
        let fp = CredentialFingerprint::of(&snap);
        let hex = fp.to_hex();
        assert_eq!(hex.len(), 8);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(fp, CredentialFingerprint::of(&snap));
    }

    #[test]
    fn host_file_read_missing_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let source = HostCredentialSource::HostFile {
            path: dir.path().join("nope").join(".credentials.json"),
        };
        assert!(matches!(
            read_claude_credential(&source),
            Err(CredentialReadError::NotFound)
        ));
    }

    #[test]
    fn host_file_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        std::fs::write(&path, SAMPLE).unwrap();
        let source = HostCredentialSource::HostFile { path };
        let snap = read_claude_credential(&source).expect("read");
        assert_eq!(snap.secret.expose(), "sk-ant-oat-ACCESS");
    }

    #[test]
    fn claude_is_auth_failure_matches_signatures() {
        assert!(claude_is_auth_failure("HTTP 401 Unauthorized"));
        assert!(claude_is_auth_failure("invalid OAuth access token"));
        assert!(claude_is_auth_failure("Failed to authenticate with server"));
        assert!(!claude_is_auth_failure("everything is fine"));
    }
}
