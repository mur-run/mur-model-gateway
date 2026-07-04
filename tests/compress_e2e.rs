//! Wire-level compression acceptance: with CC_PROXY_COMPRESS on, a fat
//! tool_result in /v1/messages reaches the upstream compressed; with the
//! flag off the body is forwarded byte-identical.

use cc_proxy::{AppState, TokenSource, build_router};
use httpmock::prelude::*;
use std::time::Duration;

async fn spawn_proxy(upstream: String, compress: bool) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut state = AppState::new(
        &upstream,
        "https://api.openai.com",
        "https://generativelanguage.googleapis.com",
        TokenSource::Disabled,
    )
    .unwrap();
    state.compress = compress;
    let app = build_router(state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    addr.to_string()
}

fn fat_body() -> serde_json::Value {
    let log: String = (0..1500)
        .map(|i| {
            format!(
                "2026-07-03 12:{:02}:{:02} INFO worker-{}: request {} completed in {}ms status OK",
                (i / 60) % 60,
                i % 60,
                i % 8,
                100_000 + i,
                10 + (i % 90)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::json!({
        "model": "claude-sonnet-5",
        "max_tokens": 16,
        "messages": [
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1",
                 "cache_control": {"type": "ephemeral"},
                 "content": log}
            ]}
        ]
    })
}

#[tokio::test]
async fn compresses_when_enabled_and_passes_through_when_disabled() {
    // Isolate the CCR store from the real ~/.mur. Both scenarios run in
    // this one test to avoid parallel env races.
    let mur_home = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("MUR_HOME", mur_home.path()) };

    let body = fat_body();

    // ── enabled: upstream sees a smaller body with a retrieval note ──
    let upstream = MockServer::start_async().await;
    let mock = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/messages")
                .body_contains("mur_retrieve");
            then.status(200).body("{}");
        })
        .await;
    let addr = spawn_proxy(upstream.base_url(), true).await;
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    mock.assert_async().await; // upstream received the compressed body

    // ── disabled: upstream sees the original, byte-identical body ──
    let upstream2 = MockServer::start_async().await;
    let mock2 = upstream2
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/messages")
                .json_body(body.clone());
            then.status(200).body("{}");
        })
        .await;
    let addr2 = spawn_proxy(upstream2.base_url(), false).await;
    let resp2 = reqwest::Client::new()
        .post(format!("http://{addr2}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status(), 200);
    mock2.assert_async().await;
}
