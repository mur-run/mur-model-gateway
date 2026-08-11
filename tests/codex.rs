//! Stage 1 acceptance: the Codex route attaches OAuth credentials.
//!
//! Only compiled when codex_impl.rs is present (cfg(has_codex_hook)).
#![cfg(has_codex_hook)]

use httpmock::prelude::*;
use mur_model_gateway::cc_version::{VersionCache, VersionStrategy};
use mur_model_gateway::{AppState, TokenSource, build_router};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

fn pinned_version() -> Arc<VersionCache> {
    Arc::new(VersionCache::new(VersionStrategy::Static(
        "9.9.9".to_string(),
    )))
}

/// Same shape as `spawn` in tests/disguise.rs:330, with the Codex upstream and
/// token source overridden. The other three upstreams point at .invalid so a
/// misrouted request fails loudly instead of escaping to the network.
async fn spawn_codex(upstream: String, codex_ts: TokenSource) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = AppState::with_version(
        "https://a.invalid",
        "https://o.invalid",
        "https://g.invalid",
        TokenSource::Disabled,
        pinned_version(),
    )
    .unwrap()
    .with_upstream_codex(upstream)
    .with_token_source_codex(codex_ts);
    let app = build_router(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(20)).await;
    addr.to_string()
}

#[tokio::test]
async fn codex_route_attaches_bearer_token() {
    let upstream = MockServer::start_async().await;
    // path("/responses") also asserts the /v1 strip from Task 3.
    let mock = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/responses")
                .header("authorization", "Bearer codex-tok")
                .matches(exactly_one_authorization_header);
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    let proxy = spawn_codex(
        upstream.base_url(),
        TokenSource::Static(Arc::new("codex-tok".to_string())),
    )
    .await;

    // Client sends no auth at all — the proxy fills it in.
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/responses"))
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-5-codex","input":"say ok"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    mock.assert_async().await;
}

/// Finding 1 regression: a client that authenticates itself with its own
/// `Authorization` header must see that exact header upstream — singular,
/// unmodified — and never the stored Codex credential.
#[tokio::test]
async fn codex_route_preserves_client_authorization_and_withholds_codex_token() {
    let upstream = MockServer::start_async().await;
    let mock = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/responses")
                .header("authorization", "Bearer client-owns-this-token")
                .matches(exactly_one_authorization_header);
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    // Deliberately distinct from the client's own token: if the proxy ever
    // overwrites or duplicates it with this stored value, the mock (which
    // requires the client's exact value, and only one such header) fails.
    let proxy = spawn_codex(
        upstream.base_url(),
        TokenSource::Static(Arc::new("codex-tok-must-not-leak".to_string())),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/responses"))
        .header("authorization", "Bearer client-owns-this-token")
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-5-codex","input":"say ok"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    mock.assert_async().await;
}

/// Finding 1 regression (the exact bug shape): a client that authenticates
/// itself with `x-api-key` alone (no `Authorization`) must see that header
/// upstream unmodified, with no `Authorization` header injected at all.
#[tokio::test]
async fn codex_route_preserves_client_api_key_and_withholds_codex_token() {
    let upstream = MockServer::start_async().await;
    let mock = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/responses")
                .header("x-api-key", "client-owns-this-key")
                .matches(no_authorization_header);
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    let proxy = spawn_codex(
        upstream.base_url(),
        TokenSource::Static(Arc::new("codex-tok-must-not-leak".to_string())),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/responses"))
        .header("x-api-key", "client-owns-this-key")
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-5-codex","input":"say ok"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    mock.assert_async().await;
}

/// Finding 1 edge case: an empty `Authorization` header is not a real
/// credential, so it must not count as "already authenticated" — the proxy
/// should still inject the stored Codex token. This is also the concrete
/// scenario where Finding 4's duplicate-header risk would show up if the
/// client's (empty) header were forwarded alongside the injected one, so it
/// asserts exactly one `authorization` header upstream, not two.
#[tokio::test]
async fn codex_route_treats_empty_authorization_as_unauthenticated() {
    let upstream = MockServer::start_async().await;
    let mock = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/responses")
                .header("authorization", "Bearer codex-tok-for-empty-header-case")
                .matches(exactly_one_authorization_header);
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    let proxy = spawn_codex(
        upstream.base_url(),
        TokenSource::Static(Arc::new("codex-tok-for-empty-header-case".to_string())),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/responses"))
        .header("authorization", "")
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-5-codex","input":"say ok"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    mock.assert_async().await;
}

/// Finding 3: the production credential path — `TokenSource::Codex(path)` →
/// `codex::read_auth` — has its own test, separate from the `Static` shortcut
/// the other tests use. Covers both the bearer token and the account-id
/// header derived from the same fixture. Finding 4: also asserts there is
/// exactly one `authorization` header, guarding against `apply_codex_headers`
/// appending a duplicate alongside one already forwarded upstream.
#[tokio::test]
async fn codex_route_reads_bearer_and_account_id_from_auth_file() {
    // Obviously-fake fixture, same shape `codex::parse_auth` expects (see
    // src/codex.rs's own `parses_chatgpt_mode_auth` test).
    let mut fixture = tempfile::NamedTempFile::new().unwrap();
    write!(
        fixture,
        r#"{{"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{{"id_token":"fixture-id-token","access_token":"fixture-access-token","refresh_token":"fixture-refresh-token","account_id":"fixture-account-id"}},"last_refresh":"2026-07-10T00:20:57.310171Z"}}"#
    )
    .unwrap();
    fixture.flush().unwrap();

    let upstream = MockServer::start_async().await;
    let mock = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/responses")
                .header("authorization", "Bearer fixture-access-token")
                .matches(exactly_one_authorization_header)
                .matches(fixture_account_id_present);
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    let proxy = spawn_codex(
        upstream.base_url(),
        TokenSource::Codex(fixture.path().to_path_buf()),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/responses"))
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-5-codex","input":"say ok"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    mock.assert_async().await;
}

// Multi-thread flavor (unlike every other test in this file): the refresh
// grant runs through `codex_impl`'s blocking bridge, which uses
// `tokio::task::block_in_place` — valid only on a multi-threaded runtime.
// That matches production (`#[tokio::main]` defaults to multi-thread); a
// current-thread test runtime would panic before ever reaching the retry.
#[tokio::test(flavor = "multi_thread")]
async fn expired_token_triggers_one_refresh_and_retry() {
    let upstream = MockServer::start_async().await;

    // The stored token is rejected...
    let stale = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/responses")
                .header("authorization", "Bearer stale-tok");
            then.status(401).body(r#"{"error":"expired"}"#);
        })
        .await;

    // ...the refreshed one is accepted.
    let fresh = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/responses")
                .header("authorization", "Bearer fresh-tok");
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    let token_ep = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/oauth/token");
            then.status(200).body(r#"{"access_token":"fresh-tok"}"#);
        })
        .await;

    let dir = std::env::temp_dir().join("mmg-codex-refresh");
    std::fs::create_dir_all(&dir).unwrap();
    let auth = dir.join("auth.json");
    std::fs::write(
        &auth,
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"stale-tok","refresh_token":"fake-refresh-token","account_id":"acct-fake"}}"#,
    )
    .unwrap();

    // SAFETY: edition 2024 makes set_var unsafe; this test owns the process env.
    unsafe {
        std::env::set_var(
            "MUR_MODEL_GATEWAY_CODEX_TOKEN_ENDPOINT",
            format!("{}/oauth/token", upstream.base_url()),
        );
    }
    mur_model_gateway::codex::reset_refresh_cache();

    let proxy = spawn_codex(upstream.base_url(), TokenSource::Codex(auth.clone())).await;
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/responses"))
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-5-codex","input":"say ok"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(
        stale.hits_async().await,
        1,
        "exactly one 401 — never a retry loop"
    );
    assert_eq!(token_ep.hits_async().await, 1, "exactly one refresh");
    assert_eq!(fresh.hits_async().await, 1, "exactly one retry");
    std::fs::remove_file(&auth).ok();
}

// ─── helpers ─────────────────────────────────────────────────────────
//
// httpmock's `.matches()` takes a bare `fn(&HttpMockRequest) -> bool`, not a
// closure trait, so these are free functions (a non-capturing closure would
// also coerce, but a named fn reads clearer at the call site).

fn header_count(req: &HttpMockRequest, name: &str) -> usize {
    req.headers
        .as_ref()
        .map(|hs| {
            hs.iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case(name))
                .count()
        })
        .unwrap_or(0)
}

fn exactly_one_authorization_header(req: &HttpMockRequest) -> bool {
    header_count(req, "authorization") == 1
}

fn no_authorization_header(req: &HttpMockRequest) -> bool {
    header_count(req, "authorization") == 0
}

/// True if some header carries this exact fixture value. Checked by value
/// rather than by header name so this test doesn't need to know (or
/// hardcode) which header `apply_codex_headers` uses for the account id —
/// that detail lives in the gitignored codex_impl.rs.
fn fixture_account_id_present(req: &HttpMockRequest) -> bool {
    req.headers
        .as_ref()
        .is_some_and(|hs| hs.iter().any(|(_, v)| v == "fixture-account-id"))
}
