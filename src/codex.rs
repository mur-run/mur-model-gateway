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
    pub fn refresh_access_token(_refresh_token: &str) -> anyhow::Result<super::RefreshedTokens> {
        anyhow::bail!("codex refresh unavailable in this build")
    }
}

pub use codex_impl::*;

use anyhow::Context;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// What an OAuth refresh grant returns. `refresh_token` is `Some` when the
/// provider rotates it — ChatGPT does, so it must be persisted or the next
/// refresh fails.
///
/// No `#[derive(Debug)]`: these are raw credentials, and a derived Debug puts
/// them in any `{:?}`, tracing capture, or panic message. Hand-write a
/// redacting impl if one is needed, as `CodexAuth` does.
#[derive(Clone)]
pub struct RefreshedTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
}

/// Credentials as Codex CLI stores them in `~/.codex/auth.json`.
#[derive(Clone)]
pub struct CodexAuth {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub account_id: Option<String>,
}

impl std::fmt::Debug for CodexAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexAuth")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("account_id", &self.account_id)
            .finish()
    }
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

/// Most recent refresh, memoised so a burst of 401s triggers one grant.
static REFRESHED: OnceLock<Mutex<Option<(Instant, String)>>> = OnceLock::new();

/// A usable access token, refreshing when the stored one was rejected. The
/// grant rotates the refresh token, so the new pair is persisted — discarding
/// it strands both this gateway and Codex CLI on a dead credential.
/// Memoised for `keychain::CACHE_TTL`.
pub fn refreshed_access_token(path: &Path) -> Option<String> {
    let cell = REFRESHED.get_or_init(|| Mutex::new(None));
    let mut slot = cell.lock().unwrap();
    if let Some((at, tok)) = slot.as_ref()
        && at.elapsed() < crate::keychain::CACHE_TTL
    {
        return Some(tok.clone());
    }
    let rt = read_auth(path)?.refresh_token?;
    match refresh_access_token(&rt) {
        Ok(new) => {
            if let Err(e) = persist_rotation(path, &new) {
                // The access token still serves this request, but a lost
                // rotation means the next refresh fails. Warn loudly.
                tracing::warn!(error = %e, "codex token rotation not persisted");
            }
            *slot = Some((Instant::now(), new.access_token.clone()));
            Some(new.access_token)
        }
        Err(e) => {
            tracing::warn!(error = %e, "codex token refresh failed");
            None
        }
    }
}

/// Clear the in-memory refreshed token. Test-only: the cache is process-global
/// and would otherwise leak between integration tests.
pub fn reset_refresh_cache() {
    if let Some(cell) = REFRESHED.get() {
        *cell.lock().unwrap() = None;
    }
}

/// Replace only the rotated fields, atomically. Codex CLI reads keys this
/// gateway does not model, so the rest of the document is preserved verbatim.
/// `last_refresh` is deliberately left alone — it is Codex CLI's bookkeeping,
/// and updating it would need a date dependency this crate does not have.
fn persist_rotation(path: &Path, new: &RefreshedTokens) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let mut doc: serde_json::Value = serde_json::from_str(&raw)?;
    let tokens = doc
        .get_mut("tokens")
        .context("auth.json has no tokens object")?;
    tokens["access_token"] = serde_json::Value::String(new.access_token.clone());
    if let Some(rt) = &new.refresh_token {
        tokens["refresh_token"] = serde_json::Value::String(rt.clone());
    }

    // Temp file in the SAME directory, so the rename stays on one filesystem —
    // that is what makes it atomic. A concurrent reader sees the old file or
    // the new one, never a torn one.
    let dir = path.parent().context("auth.json has no parent dir")?;
    let tmp = dir.join(".auth.json.mmg-tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&doc)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
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

    #[test]
    fn debug_redacts_tokens() {
        let auth = CodexAuth {
            access_token: "fake-access-token".to_string(),
            refresh_token: Some("fake-refresh-token".to_string()),
            account_id: Some("acct-fake".to_string()),
        };
        let debug_str = format!("{:?}", auth);

        // Verify tokens are redacted
        assert!(!debug_str.contains("fake-access-token"));
        assert!(!debug_str.contains("fake-refresh-token"));
        assert!(debug_str.contains("<redacted>"));

        // Verify account_id is NOT redacted
        assert!(debug_str.contains("acct-fake"));
    }

    #[test]
    fn persist_rotation_preserves_unmodelled_fields() {
        let dir = std::env::temp_dir().join("mmg-codex-persist");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("auth.json");
        std::fs::write(
            &p,
            r#"{"auth_mode":"chatgpt","OPENAI_API_KEY":null,"some_future_key":42,
                "tokens":{"id_token":"fake-id","access_token":"old-a","refresh_token":"old-r","account_id":"acct"}}"#,
        )
        .unwrap();

        persist_rotation(
            &p,
            &RefreshedTokens {
                access_token: "new-a".into(),
                refresh_token: Some("new-r".into()),
            },
        )
        .unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["tokens"]["access_token"], "new-a");
        assert_eq!(v["tokens"]["refresh_token"], "new-r");
        // Untouched fields survive — Codex CLI depends on them.
        assert_eq!(v["tokens"]["id_token"], "fake-id");
        assert_eq!(v["tokens"]["account_id"], "acct");
        assert_eq!(v["some_future_key"], 42);
        assert_eq!(v["auth_mode"], "chatgpt");
        std::fs::remove_file(&p).ok();
    }
}
