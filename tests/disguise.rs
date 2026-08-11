//! Iter 1 acceptance: disguise layer on `/v1/messages*`.
//!
//! When the inbound request has no auth and a token is available, the
//! upstream sees Bearer auth, the claude-code-* beta header, and a
//! billing-prefix block prepended to `system`. When the inbound has its
//! own auth, the upstream sees that auth unchanged.
//!
//! Only compiled when `disguise_impl.rs` is present (cfg(has_beta_hook)).

#![cfg(has_beta_hook)]

use httpmock::prelude::*;
use mur_model_gateway::cc_version::{VersionCache, VersionStrategy};
use mur_model_gateway::disguise::{OAUTH_BETAS, billing_prefix};
use mur_model_gateway::{AppState, TokenSource, build_router};
use std::sync::Arc;
use std::time::Duration;

const TEST_VERSION: &str = "9.9.9";

fn pinned_version() -> Arc<VersionCache> {
    Arc::new(VersionCache::new(VersionStrategy::Static(
        TEST_VERSION.to_string(),
    )))
}

fn expected_prefix() -> String {
    billing_prefix(TEST_VERSION)
}

#[tokio::test]
async fn disguise_injects_auth_betas_and_billing_prefix() {
    let upstream = MockServer::start_async().await;
    let upstream_mock = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/messages")
                .header("authorization", "Bearer test-oauth-token")
                .header("anthropic-beta", OAUTH_BETAS)
                .body_contains(expected_prefix());
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    let proxy_addr = spawn(upstream.base_url(), with_token("test-oauth-token")).await;
    // Client sends no auth at all — proxy should fill it in.
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(r#"{"model":"x","max_tokens":1,"system":"hello","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    upstream_mock.assert_async().await;
}

#[tokio::test]
async fn disguise_merges_client_anthropic_beta_with_oauth_betas() {
    // Claude Code 4.7 sends its own `anthropic-beta` (e.g. clear-thinking-*)
    // describing body features it uses. The proxy must merge — not replace —
    // so the upstream sees both the OAuth-required betas and the client's.
    let upstream = MockServer::start_async().await;
    let upstream_mock = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/v1/messages").matches(|req| {
                let beta = req
                    .headers
                    .as_ref()
                    .and_then(|hs| {
                        hs.iter()
                            .find(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta"))
                            .map(|(_, v)| v.clone())
                    })
                    .unwrap_or_default();
                let parts: std::collections::HashSet<&str> =
                    beta.split(',').map(str::trim).collect();
                parts.contains("claude-code-20250219")
                    && parts.contains("oauth-2025-04-20")
                    && parts.contains("interleaved-thinking-2025-05-14")
                    && parts.contains("compact-2026-01-12")
                    && parts.contains("clear-thinking-2025-10-15")
                    && parts.contains("client-only-beta")
            });
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    let proxy_addr = spawn(upstream.base_url(), with_token("test-oauth-token")).await;
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header(
            "anthropic-beta",
            "clear-thinking-2025-10-15,client-only-beta,compact-2026-01-12",
        )
        .header("content-type", "application/json")
        .body(r#"{"model":"x","max_tokens":1,"messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    upstream_mock.assert_async().await;
}

#[tokio::test]
async fn disguise_does_not_double_inject_when_client_already_authed() {
    let upstream = MockServer::start_async().await;
    let upstream_mock = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/messages")
                .header("authorization", "Bearer client-supplied-token")
                .body_contains("client-system")
                // Crucially: client-supplied request must NOT gain the
                // billing prefix; the client already did its own disguise.
                .matches(|req| {
                    let body = req
                        .body
                        .as_ref()
                        .map(|b| String::from_utf8_lossy(b).into_owned())
                        .unwrap_or_default();
                    !body.contains(&expected_prefix())
                });
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    let proxy_addr = spawn(upstream.base_url(), with_token("test-oauth-token")).await;
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("authorization", "Bearer client-supplied-token")
        .header("content-type", "application/json")
        .body(r#"{"model":"x","max_tokens":1,"system":"client-system","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    upstream_mock.assert_async().await;
}

#[tokio::test]
async fn disguise_skips_non_messages_paths() {
    let upstream = MockServer::start_async().await;
    let upstream_mock = upstream
        .mock_async(|when, then| {
            when.method(GET).path("/v1/files/abc").matches(|req| {
                // No Bearer should be added on non-Messages paths.
                !req.headers
                    .as_ref()
                    .map(|hs| {
                        hs.iter().any(|(k, v)| {
                            k.eq_ignore_ascii_case("authorization")
                                && v.starts_with("Bearer test-oauth-token")
                        })
                    })
                    .unwrap_or(false)
            });
            then.status(200).body("{}");
        })
        .await;

    let proxy_addr = spawn(upstream.base_url(), with_token("test-oauth-token")).await;
    let resp = reqwest::Client::new()
        .get(format!("http://{proxy_addr}/v1/files/abc"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    upstream_mock.assert_async().await;
}

#[tokio::test]
async fn disguise_disabled_passthrough_is_lossless() {
    let upstream = MockServer::start_async().await;
    let upstream_mock = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/v1/messages").matches(|req| {
                // No Authorization should be present (client didn't send one,
                // and Disabled token source must not synthesize one).
                !req.headers
                    .as_ref()
                    .map(|hs| {
                        hs.iter()
                            .any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                    })
                    .unwrap_or(false)
            });
            then.status(401).body(r#"{"error":"no_auth"}"#);
        })
        .await;

    let proxy_addr = spawn(upstream.base_url(), TokenSource::Disabled).await;
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    upstream_mock.assert_async().await;
}

#[tokio::test]
async fn disguise_preserves_array_form_system_with_cache_control() {
    let upstream = MockServer::start_async().await;
    let upstream_mock = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/v1/messages").matches(|req| {
                let body = req
                    .body
                    .as_ref()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .unwrap_or_default();
                let v: serde_json::Value = match serde_json::from_str(&body) {
                    Ok(v) => v,
                    Err(_) => return false,
                };
                let arr = match v["system"].as_array() {
                    Some(a) => a,
                    None => return false,
                };
                arr.len() == 2
                    && arr[0]["text"] == expected_prefix()
                    && arr[1]["text"] == "ctxblock"
                    && arr[1]["cache_control"]["type"] == "ephemeral"
            });
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    let proxy_addr = spawn(upstream.base_url(), with_token("test-oauth-token")).await;
    let body = serde_json::json!({
        "model": "x",
        "max_tokens": 1,
        "system": [
            {"type": "text", "text": "ctxblock", "cache_control": {"type": "ephemeral"}}
        ],
        "messages": []
    });
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    upstream_mock.assert_async().await;
}

#[tokio::test]
async fn disguise_upgrades_x_api_key_oauth_shape_token() {
    // New-mur path: the public client always sends `x-api-key: $TOKEN`,
    // even when $TOKEN is a subscription OAuth token. The proxy recognizes
    // sk-ant-oat* as an OAuth intent signal, then resolves a fresh token
    // from the configured TokenSource (Keychain in production). The client's
    // token is never used as the upstream bearer — it is only the signal.
    let upstream = MockServer::start_async().await;
    let upstream_mock = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/messages")
                .header("authorization", "Bearer unused-keychain-token")
                .header("anthropic-beta", OAUTH_BETAS)
                .body_contains(expected_prefix())
                .matches(|req| {
                    // Original x-api-key must be stripped.
                    !req.headers
                        .as_ref()
                        .map(|hs| hs.iter().any(|(k, _)| k.eq_ignore_ascii_case("x-api-key")))
                        .unwrap_or(false)
                });
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    let proxy_addr = spawn(upstream.base_url(), with_token("unused-keychain-token")).await;
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("x-api-key", "sk-ant-oat-from-client")
        .header("content-type", "application/json")
        .body(r#"{"model":"x","max_tokens":1,"messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    upstream_mock.assert_async().await;
}

#[tokio::test]
async fn disguise_passes_through_regular_api_key_x_api_key() {
    // A real console API key (sk-ant-api03-*) must NOT be touched even
    // when the path is a Messages endpoint and a TokenSource is configured.
    let upstream = MockServer::start_async().await;
    let upstream_mock = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/messages")
                .header("x-api-key", "sk-ant-api03-real-key")
                .matches(|req| {
                    let body = req
                        .body
                        .as_ref()
                        .map(|b| String::from_utf8_lossy(b).into_owned())
                        .unwrap_or_default();
                    !body.contains(&expected_prefix())
                });
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    let proxy_addr = spawn(upstream.base_url(), with_token("unused-keychain-token")).await;
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("x-api-key", "sk-ant-api03-real-key")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    upstream_mock.assert_async().await;
}

// ─── helpers ─────────────────────────────────────────────────────────

fn with_token(t: &str) -> TokenSource {
    TokenSource::Static(Arc::new(t.to_string()))
}

async fn spawn(upstream: String, token_source: TokenSource) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = build_router(
        AppState::with_version(
            &upstream,
            &upstream,
            &upstream,
            token_source,
            pinned_version(),
        )
        .unwrap()
        // Fix round 1, finding 6: `with_version` already defaults
        // `token_source_codex` to `Disabled`, so this is belt-and-suspenders
        // — explicit here so nobody reading this helper has to go check the
        // library default to know these disguise tests can never resolve,
        // read, or refresh-and-rewrite the real ~/.codex/auth.json.
        .with_token_source_codex(TokenSource::Disabled),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    addr.to_string()
}
