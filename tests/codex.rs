//! Stage 1 acceptance: the Codex route attaches OAuth credentials.
//!
//! Only compiled when codex_impl.rs is present (cfg(has_codex_hook)).
#![cfg(has_codex_hook)]

use httpmock::prelude::*;
use mur_model_gateway::cc_version::{VersionCache, VersionStrategy};
use mur_model_gateway::{AppState, TokenSource, build_router};
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
                .header("authorization", "Bearer codex-tok");
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
