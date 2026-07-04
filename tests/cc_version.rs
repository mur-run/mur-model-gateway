//! Iter 2 acceptance: cc_version detection and propagation.
//!
//! Live test depends on `claude --version` being on PATH; we
//! `#[ignore]` it by default so CI without Claude Code installed
//! still passes. Run with `cargo test --test cc_version -- --ignored`.

use cc_proxy::cc_version::{FALLBACK_VERSION, VersionCache, VersionStrategy};
use cc_proxy::disguise::billing_prefix;
use cc_proxy::{AppState, TokenSource, build_router};
use httpmock::prelude::*;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn cached_version_propagates_into_billing_prefix() {
    let upstream = MockServer::start_async().await;
    let upstream_mock = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/messages")
                .body_contains("cc_version=4.5.6");
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    let cache = Arc::new(VersionCache::new(VersionStrategy::Static("4.5.6".into())));
    let proxy_addr = spawn_with(upstream.base_url(), cache).await;
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(r#"{"model":"x","max_tokens":1,"messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    upstream_mock.assert_async().await;
}

#[tokio::test]
async fn fallback_strategy_uses_known_constant() {
    let upstream = MockServer::start_async().await;
    let upstream_mock = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/messages")
                .body_contains(format!("cc_version={FALLBACK_VERSION}"));
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    let cache = Arc::new(VersionCache::new(VersionStrategy::FallbackOnly));
    let proxy_addr = spawn_with(upstream.base_url(), cache).await;
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    upstream_mock.assert_async().await;
}

#[tokio::test]
#[ignore = "requires `claude` CLI on PATH"]
async fn live_detection_returns_real_version() {
    let cache = VersionCache::detect_or_fallback();
    let v = cache.get();
    // Must look like dotted numerics (e.g. "2.1.126").
    assert!(
        v.chars().all(|c| c.is_ascii_digit() || c == '.'),
        "got non-version: {v}"
    );
    assert!(v.contains('.'), "got short version: {v}");
    let p = billing_prefix(&v);
    assert!(p.contains(&v));
}

async fn spawn_with(upstream: String, version_cache: Arc<VersionCache>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let token = TokenSource::Static(Arc::new("test-token".to_string()));
    let app = build_router(
        AppState::with_version(
            &upstream,
            "https://api.openai.com",
            "https://generativelanguage.googleapis.com",
            token,
            version_cache,
        )
        .unwrap(),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    addr.to_string()
}
