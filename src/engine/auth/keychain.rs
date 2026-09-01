//! Per-platform keychain credential resolution.
//!
//! macOS uses `security find-generic-password`. Linux uses `secret-tool`
//! (libsecret/Secret-Service) when available. Windows returns no credentials.
//!
//! Two delivery shapes, picked per agent:
//!
//! 1. **Env-var credentials** (`agent_keychain_credentials`) — `(key, value)`
//!    pairs injected via `docker -e` / `container --env`. Used by agents that
//!    accept their OAuth token through an env var (e.g. Claude with
//!    `CLAUDE_CODE_OAUTH_TOKEN`).
//!
//! 2. **File-form credentials** (`agent_keychain_files`) — files to plant
//!    inside the agent's settings-dir overlay before mount. Used by agents
//!    that only read tokens from a fixed on-disk path (e.g. Antigravity
//!    reads `~/.gemini/antigravity-cli/antigravity-oauth-token` when its
//!    in-container keyring is unreachable).

use std::path::PathBuf;
use std::process::Command;

use crate::data::session::AgentName;
use crate::engine::auth::credential::{
    self, HostCredentialSource, RefreshableCredentialSpec, CLAUDE_KEYCHAIN_SERVICE,
};

/// File-form credential to be written into an agent's settings-dir overlay
/// before mounting the overlay into the container. Lifecycle: produced from
/// the host keychain, copied into a per-session tempdir alongside the rest of
/// the agent's settings, then bind-mounted under the agent's `$HOME`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSecretFile {
    /// Path **relative** to the agent's settings dir. E.g. for antigravity
    /// this is `antigravity-cli/antigravity-oauth-token`, joined with the
    /// staged `~/.gemini` to produce the final on-disk path.
    pub relative_path: PathBuf,
    /// File contents.
    pub contents: Vec<u8>,
    /// Unix permission mode (e.g. `0o600`). Ignored on non-Unix.
    pub mode: u32,
}

/// Env-var credentials for the agent. Empty when the platform has no keychain
/// integration, the entry is missing, or the payload fails to decode.
pub fn agent_keychain_credentials(agent: &AgentName) -> Vec<(String, String)> {
    match agent.as_str() {
        "claude" => claude_keychain_credentials(),
        _ => Vec::new(),
    }
}

/// File-form credentials for the agent. Empty when the platform has no
/// keychain integration, the entry is missing, or the payload fails to decode.
pub fn agent_keychain_files(agent: &AgentName) -> Vec<AgentSecretFile> {
    match agent.as_str() {
        "antigravity" => antigravity_keychain_files(),
        _ => Vec::new(),
    }
}

/// Refresh descriptor lookup — the generic replacement for the per-agent
/// dispatch. `Some` only for agents that opt into refreshable file delivery;
/// `None` keeps an agent's behaviour EXACTLY as it is today (env delivery for
/// claude-less agents, `AgentSecretFile` delivery for antigravity).
pub fn refreshable_spec_for(agent: &AgentName) -> Option<&'static RefreshableCredentialSpec> {
    match agent.as_str() {
        "claude" => Some(credential::claude_spec()),
        _ => None,
    }
}

// ── Claude (env-var) ────────────────────────────────────────────────────────

/// macOS-only: look up the Claude Code OAuth credential and extract its access
/// token. This is the legacy **env** delivery path (`CLAUDE_CODE_OAUTH_TOKEN`),
/// still used by the sandbox/sbx runtime and any non-container caller; the
/// container path now delivers Claude's credential as a refreshable file via
/// [`refreshable_spec_for`].
///
/// The keychain code stays macOS-only (the descriptor's cross-platform *source*
/// handles non-macOS); a missing entry or unparseable payload is logged with a
/// typed reason so `awman ready` can surface it, rather than silently returning
/// an empty vec.
fn claude_keychain_credentials() -> Vec<(String, String)> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let source = HostCredentialSource::MacosKeychain {
        service: CLAUDE_KEYCHAIN_SERVICE,
    };
    match credential::read_claude_credential(&source) {
        Ok(snap) => vec![(
            "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
            snap.secret.expose().to_string(),
        )],
        Err(reason) => {
            // Never log the secret — `reason` and the agent name only.
            tracing::warn!(
                agent = "claude",
                reason = %reason,
                "could not read Claude keychain credential for env delivery"
            );
            Vec::new()
        }
    }
}

// ── Antigravity (file-form) ─────────────────────────────────────────────────

/// Antigravity stores its OAuth token under macOS Keychain service `gemini`,
/// account `antigravity` (or the corresponding libsecret entry on Linux),
/// wrapped with the `go-keyring-base64:` envelope (zalando/go-keyring on
/// macOS encodes every secret this way to dodge `security`'s hex-mangling).
/// Unwrapped, the payload is a JSON object:
///
/// ```json
/// {"token":{"access_token":"...","token_type":"Bearer",
///           "refresh_token":"...","expiry":"..."},
///  "auth_method":"consumer"}
/// ```
///
/// Inside the container the keyring backend has no D-Bus to talk to, so agy
/// falls back to reading the same JSON from a fixed file at
/// `~/.gemini/antigravity-cli/antigravity-oauth-token` (verified via strace
/// of agy + a live login round-trip).
fn antigravity_keychain_files() -> Vec<AgentSecretFile> {
    let Some(raw) = read_antigravity_secret() else {
        return Vec::new();
    };
    let Some(decoded) = decode_go_keyring_payload(&raw) else {
        return Vec::new();
    };
    // Sanity-check the JSON shape so we never plant a malformed token file
    // that would itself fail the container's silent-auth path.
    let parsed: serde_json::Value = match serde_json::from_slice(&decoded) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    if parsed
        .get("token")
        .and_then(|t| t.get("access_token"))
        .and_then(|v| v.as_str())
        .is_none()
    {
        return Vec::new();
    }
    vec![AgentSecretFile {
        relative_path: PathBuf::from("antigravity-cli").join("antigravity-oauth-token"),
        contents: decoded,
        mode: 0o600,
    }]
}

fn read_antigravity_secret() -> Option<String> {
    if cfg!(target_os = "macos") {
        run_macos_keychain_lookup("gemini", Some("antigravity"))
    } else if cfg!(target_os = "linux") {
        run_linux_secret_lookup("gemini", "antigravity")
    } else {
        None
    }
}

// ── Shared OS keychain shims ────────────────────────────────────────────────

pub(crate) fn run_macos_keychain_lookup(service: &str, account: Option<&str>) -> Option<String> {
    let mut cmd = Command::new("security");
    cmd.arg("find-generic-password").arg("-s").arg(service);
    if let Some(a) = account {
        cmd.arg("-a").arg(a);
    }
    cmd.arg("-w");
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if raw.is_empty() {
        None
    } else {
        Some(raw)
    }
}

fn run_linux_secret_lookup(service: &str, account: &str) -> Option<String> {
    let out = Command::new("secret-tool")
        .args(["lookup", "service", service, "account", account])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if raw.is_empty() {
        None
    } else {
        Some(raw)
    }
}

/// Strip the `go-keyring-base64:` envelope (or the legacy
/// `go-keyring-encoded:` hex form) used by zalando/go-keyring. Passes the
/// payload through unchanged when no prefix matches.
fn decode_go_keyring_payload(raw: &str) -> Option<Vec<u8>> {
    const B64_PREFIX: &str = "go-keyring-base64:";
    const HEX_PREFIX: &str = "go-keyring-encoded:";
    if let Some(rest) = raw.strip_prefix(B64_PREFIX) {
        // go-keyring writes a strict RFC 4648 standard-alphabet payload; allow
        // the trailing newlines that some shells leave on the value.
        base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            rest.trim().as_bytes(),
        )
        .ok()
    } else if let Some(rest) = raw.strip_prefix(HEX_PREFIX) {
        decode_hex(rest.trim())
    } else {
        Some(raw.as_bytes().to_vec())
    }
}

fn decode_hex(input: &str) -> Option<Vec<u8>> {
    if !input.len().is_multiple_of(2) {
        return None;
    }
    (0..input.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(input.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn decode_go_keyring_base64_unwraps_prefix() {
        let inner = b"{\"token\":{\"access_token\":\"x\",\"token_type\":\"Bearer\",\
                       \"refresh_token\":\"y\",\"expiry\":\"2099-01-01T00:00:00Z\"},\
                       \"auth_method\":\"consumer\"}";
        let b64 = base64::engine::general_purpose::STANDARD.encode(inner);
        let wrapped = format!("go-keyring-base64:{b64}");
        let out = decode_go_keyring_payload(&wrapped).expect("decode");
        assert_eq!(out, inner);
    }

    #[test]
    fn decode_go_keyring_base64_rejects_invalid_alphabet() {
        let wrapped = "go-keyring-base64:!!!not-valid-base64!!!";
        assert_eq!(decode_go_keyring_payload(wrapped), None);
    }

    #[test]
    fn decode_go_keyring_passes_through_when_unprefixed() {
        let raw = "{\"plain\":1}";
        assert_eq!(
            decode_go_keyring_payload(raw),
            Some(raw.as_bytes().to_vec())
        );
    }

    #[test]
    fn decode_hex_round_trips() {
        assert_eq!(decode_hex("deadbeef"), Some(vec![0xde, 0xad, 0xbe, 0xef]));
        assert_eq!(decode_hex("DEADBEEF"), Some(vec![0xde, 0xad, 0xbe, 0xef]));
    }

    #[test]
    fn decode_hex_rejects_odd_length() {
        assert_eq!(decode_hex("abc"), None);
    }

    #[test]
    fn agent_keychain_files_for_unknown_agent_is_empty() {
        let agent = AgentName::new("totallymadeup").unwrap();
        assert!(agent_keychain_files(&agent).is_empty());
    }

    #[test]
    fn agent_keychain_credentials_for_unknown_agent_is_empty() {
        let agent = AgentName::new("totallymadeup").unwrap();
        assert!(agent_keychain_credentials(&agent).is_empty());
    }

    #[test]
    fn refreshable_spec_only_for_claude() {
        let claude = AgentName::new("claude").unwrap();
        assert!(refreshable_spec_for(&claude).is_some());
        // Agents without a descriptor keep today's behaviour (env / AgentSecretFile).
        for other in ["antigravity", "codex", "gemini", "totallymadeup"] {
            let agent = AgentName::new(other).unwrap();
            assert!(
                refreshable_spec_for(&agent).is_none(),
                "{other} must have no refresh descriptor"
            );
        }
    }
}
