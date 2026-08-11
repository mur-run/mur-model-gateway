//! Codex route: forwards `/v1/responses*` to ChatGPT's Codex backend with
//! Codex credentials attached.
//!
//! In public builds only `should_route()` is active — the header and OAuth
//! details are no-op stubs. The real implementation lives in a gitignored
//! file activated via build.rs (`cfg(has_codex_hook)`).

/// True if `path` is a Codex Responses endpoint we route to ChatGPT.
pub fn should_route(path: &str) -> bool {
    path == "/v1/responses"
        || path.starts_with("/v1/responses/")
        || path.starts_with("/v1/responses?")
}

// ── cfg-gated: real impl or stub ──

// The #[rustfmt::skip] is load-bearing: rustfmt resolves `mod` declarations
// syntactically and ignores cfg, so a clean checkout without the gitignored
// file fails `cargo fmt --check` without it. Same fix as src/disguise.rs.
#[rustfmt::skip]
#[cfg(has_codex_hook)]
mod codex_impl;

#[cfg(not(has_codex_hook))]
mod codex_impl {
    /// Stub: forwards without Codex client headers.
    pub fn apply_codex_headers(
        req: reqwest::RequestBuilder,
        _token: &str,
        _account_id: Option<&str>,
    ) -> reqwest::RequestBuilder {
        req
    }

    /// Stub: no OAuth constants in the public build.
    pub fn refresh_access_token(_refresh_token: &str) -> anyhow::Result<String> {
        anyhow::bail!("codex refresh unavailable in this build")
    }
}

pub use codex_impl::*;

use std::path::{Path, PathBuf};

/// Credentials as Codex CLI stores them in `~/.codex/auth.json`.
#[derive(Clone, Debug)]
pub struct CodexAuth {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub account_id: Option<String>,
}

/// `~/.codex/auth.json`.
pub fn default_auth_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().join(".codex/auth.json"))
}

/// Parse the auth blob. `None` for malformed JSON, missing tokens, or
/// API-key mode — all of which mean "no OAuth credential available".
pub fn parse_auth(raw: &str) -> Option<CodexAuth> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    if v.get("auth_mode").and_then(|m| m.as_str()) != Some("chatgpt") {
        return None;
    }
    let tokens = v.get("tokens")?;
    Some(CodexAuth {
        access_token: tokens.get("access_token")?.as_str()?.to_string(),
        refresh_token: tokens
            .get("refresh_token")
            .and_then(|t| t.as_str())
            .map(str::to_string),
        account_id: tokens
            .get("account_id")
            .and_then(|t| t.as_str())
            .map(str::to_string),
    })
}

/// Read and parse the auth file. `None` if absent or unusable — the caller
/// falls through to passthrough.
pub fn read_auth(path: &Path) -> Option<CodexAuth> {
    parse_auth(&std::fs::read_to_string(path).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_responses_paths_only() {
        assert!(should_route("/v1/responses"));
        assert!(should_route("/v1/responses/abc"));
        assert!(should_route("/v1/responses?stream=true"));
        assert!(!should_route("/v1/messages"));
        assert!(!should_route("/v1/chat/completions"));
        assert!(!should_route("/v1/responsesX"));
    }

    #[test]
    fn parses_chatgpt_mode_auth() {
        let raw = r#"{
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "fake-id-token",
                "access_token": "fake-access-token",
                "refresh_token": "fake-refresh-token",
                "account_id": "acct-fake"
            },
            "last_refresh": "2026-07-10T00:20:57.310171Z"
        }"#;
        let a = parse_auth(raw).expect("should parse");
        assert_eq!(a.access_token, "fake-access-token");
        assert_eq!(a.refresh_token.as_deref(), Some("fake-refresh-token"));
        assert_eq!(a.account_id.as_deref(), Some("acct-fake"));
    }

    #[test]
    fn rejects_api_key_mode() {
        // Stage 1 handles OAuth only; API-key mode resolves to None so the
        // caller falls through to passthrough rather than sending a bad token.
        let raw = r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-fake","tokens":null}"#;
        assert!(parse_auth(raw).is_none());
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(parse_auth("{not json").is_none());
        assert!(parse_auth("{}").is_none());
    }
}
