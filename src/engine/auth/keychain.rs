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

// ── Claude (env-var) ────────────────────────────────────────────────────────

/// Returns `true` when the harness already supplies its own Anthropic
/// credential, making keychain OAuth injection unnecessary and harmful.
/// Claude Code warns "auth may not work" when both `CLAUDE_CODE_OAUTH_TOKEN`
/// and `ANTHROPIC_API_KEY` are present in the same environment.
///
/// Triggers when:
/// - `ANTHROPIC_API_KEY` is set and non-empty (direct API-key auth), or
/// - `ANTHROPIC_BASE_URL` is set to a non-anthropic.com endpoint (local/omlx
///   harness pointing at a custom base URL).
///
/// `lookup_env` abstracts `std::env::var` so tests stay hermetic and never
/// mutate process-global state, mirroring the `lookup_env` pattern used by
/// `auto_auth_env_overlays`.
fn harness_supplies_anthropic_auth(lookup_env: impl Fn(&str) -> Option<String>) -> bool {
    if lookup_env("ANTHROPIC_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        return true;
    }
    if let Some(base_url) = lookup_env("ANTHROPIC_BASE_URL") {
        if !base_url.is_empty() {
            let lower = base_url.to_ascii_lowercase();
            // Cloud endpoints: api.anthropic.com and *.anthropic.com are
            // first-party — keychain OAuth still applies. Anything else (local
            // address, omlx harness, custom proxy) means the harness owns auth.
            let is_cloud = lower.contains("anthropic.com");
            if !is_cloud {
                return true;
            }
        }
    }
    false
}

/// macOS-only: look up the Claude Code OAuth credential and extract its
/// access token via the JSON path `claudeAiOauth.accessToken`.
///
/// Returns an empty list immediately (without touching the keychain) when the
/// harness already supplies its own Anthropic credential — see
/// [`harness_supplies_anthropic_auth`].
fn claude_keychain_credentials() -> Vec<(String, String)> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    if harness_supplies_anthropic_auth(|key| std::env::var(key).ok()) {
        return Vec::new();
    }
    let Some(raw) = run_macos_keychain_lookup("Claude Code-credentials", None) else {
        return Vec::new();
    };
    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    parsed
        .get("claudeAiOauth")
        .and_then(|v| v.get("accessToken"))
        .and_then(|v| v.as_str())
        .map(|t| vec![("CLAUDE_CODE_OAUTH_TOKEN".to_string(), t.to_string())])
        .unwrap_or_default()
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

fn run_macos_keychain_lookup(service: &str, account: Option<&str>) -> Option<String> {
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

    // ── harness_supplies_anthropic_auth ────────────────────────────────────

    /// Helper: build a lookup closure from a static list of (key, value) pairs.
    fn env_from<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn harness_auth_true_when_api_key_set_nonempty() {
        let lookup = env_from(&[("ANTHROPIC_API_KEY", "sk-ant-test123")]);
        assert!(
            harness_supplies_anthropic_auth(lookup),
            "non-empty ANTHROPIC_API_KEY must signal harness auth"
        );
    }

    #[test]
    fn harness_auth_false_when_api_key_empty_string() {
        let lookup = env_from(&[("ANTHROPIC_API_KEY", "")]);
        assert!(
            !harness_supplies_anthropic_auth(lookup),
            "empty ANTHROPIC_API_KEY must not trigger the guard"
        );
    }

    #[test]
    fn harness_auth_true_when_base_url_is_non_cloud() {
        let lookup = env_from(&[("ANTHROPIC_BASE_URL", "http://192.168.65.1:8000")]);
        assert!(
            harness_supplies_anthropic_auth(lookup),
            "non-anthropic.com ANTHROPIC_BASE_URL must signal harness auth"
        );
    }

    #[test]
    fn harness_auth_false_when_base_url_is_anthropic_com() {
        let lookup = env_from(&[("ANTHROPIC_BASE_URL", "https://api.anthropic.com")]);
        assert!(
            !harness_supplies_anthropic_auth(lookup),
            "official anthropic.com base URL must not suppress keychain OAuth"
        );
    }

    #[test]
    fn harness_auth_false_when_nothing_set() {
        let lookup = env_from(&[]);
        assert!(
            !harness_supplies_anthropic_auth(lookup),
            "no env vars set must not trigger the guard"
        );
    }
}
