//! Iter 0 acceptance: cc-proxy must transparently forward
//! requests + headers + body to an upstream and stream the
//! response (status, headers, body) back unchanged.

use cc_proxy::{AppState, build_router};
use httpmock::prelude::*;
use std::time::Duration;

#[tokio::test]
async fn passthrough_preserves_headers_body_and_status() {
    let upstream = MockServer::start_async().await;

    let upstream_mock = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/messages")
                .header("authorization", "Bearer sk-ant-oat-test")
                .header("anthropic-beta", "claude-code-20250219,oauth-2025-04-20")
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .body_contains("x-anthropic-billing-header");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"id":"msg_test","content":[{"type":"text","text":"ok"}]}"#);
        })
        .await;

    let proxy_addr = spawn_proxy(upstream.base_url()).await;

    let body = serde_json::json!({
        "model": "claude-3-5-sonnet-latest",
        "max_tokens": 16,
        "system": "x-anthropic-billing-header: cc_version=2.1.77; cc_entrypoint=sdk-cli;",
        "messages": [{"role": "user", "content": "hi"}],
    });

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("authorization", "Bearer sk-ant-oat-test")
        .header("anthropic-beta", "claude-code-20250219,oauth-2025-04-20")
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .expect("proxy request");

    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(text.contains("msg_test"));
    upstream_mock.assert_async().await;
}

#[tokio::test]
async fn passthrough_preserves_non_2xx_status_and_body() {
    let upstream = MockServer::start_async().await;
    let _m = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/v1/messages");
            then.status(429).body(r#"{"error":"rate_limit"}"#);
        })
        .await;

    let proxy_addr = spawn_proxy(upstream.base_url()).await;
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .body("{}")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 429);
    assert_eq!(resp.text().await.unwrap(), r#"{"error":"rate_limit"}"#);
}

#[tokio::test]
async fn passthrough_forwards_arbitrary_path() {
    let upstream = MockServer::start_async().await;
    let _m = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/v1/messages/count_tokens");
            then.status(200).body(r#"{"input_tokens":42}"#);
        })
        .await;

    let proxy_addr = spawn_proxy(upstream.base_url()).await;
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages/count_tokens"))
        .body("{}")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert!(resp.text().await.unwrap().contains("42"));
}

#[tokio::test]
async fn passthrough_handles_get() {
    let upstream = MockServer::start_async().await;
    let _m = upstream
        .mock_async(|when, then| {
            when.method(GET).path("/v1/messages/batches/abc");
            then.status(200)
                .body(r#"{"id":"abc","status":"in_progress"}"#);
        })
        .await;

    let proxy_addr = spawn_proxy(upstream.base_url()).await;
    let resp = reqwest::Client::new()
        .get(format!("http://{proxy_addr}/v1/messages/batches/abc"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert!(resp.text().await.unwrap().contains("in_progress"));
}

/// Spawn cc-proxy on an ephemeral port pointing at `upstream`.
async fn spawn_proxy(upstream: String) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = build_router(AppState::new(&upstream).unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    addr.to_string()
}
