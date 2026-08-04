//! Wire-level compression acceptance: with MUR_MODEL_GATEWAY_COMPRESS on, a fat
//! tool_result reaches the upstream compressed for all three providers;
//! with the flag off the body is forwarded byte-identical.
//!
//! All scenarios run in a single test to avoid parallel MUR_HOME env races.

use httpmock::prelude::*;
use mur_model_gateway::{AppState, TokenSource, build_router};
use std::time::Duration;

async fn spawn_proxy(upstream: String, compress: bool) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut state = AppState::new(&upstream, &upstream, &upstream, TokenSource::Disabled).unwrap();
    state.compress = compress;
    let app = build_router(state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    addr.to_string()
}

fn fat_log() -> String {
    (0..2000)
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
        .join("\n")
}

fn fat_anthropic_body() -> serde_json::Value {
    serde_json::json!({
        "model": "claude-sonnet-5",
        "max_tokens": 16,
        "messages": [
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1",
                 "cache_control": {"type": "ephemeral"},
                 "content": fat_log()}
            ]}
        ]
    })
}

fn fat_openai_body() -> serde_json::Value {
    serde_json::json!({
        "model": "gpt-4o",
        "messages": [
            {"role": "assistant", "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": fat_log()}
        ]
    })
}

fn fat_gemini_body() -> serde_json::Value {
    serde_json::json!({
        "contents": [
            {"role": "model", "parts": [
                {"functionCall": {"name": "bash", "args": {}}}
            ]},
            {"role": "user", "parts": [
                {"functionResponse": {"name": "bash", "response": {"result": fat_log()}}}
            ]}
        ]
    })
}

#[tokio::test]
async fn all_providers_compress_and_disabled_passthrough() {
    let mur_home = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("MUR_HOME", mur_home.path()) };

    // ── Anthropic: enabled ──
    let body_a = fat_anthropic_body();
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
        .json(&body_a)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    mock.assert_async().await;

    // ── Anthropic: disabled (byte-identical passthrough) ──
    let upstream2 = MockServer::start_async().await;
    let mock2 = upstream2
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/messages")
                .json_body(body_a.clone());
            then.status(200).body("{}");
        })
        .await;
    let addr2 = spawn_proxy(upstream2.base_url(), false).await;
    let resp2 = reqwest::Client::new()
        .post(format!("http://{addr2}/v1/messages"))
        .json(&body_a)
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status(), 200);
    mock2.assert_async().await;

    // ── OpenAI: enabled ──
    let body_oa = fat_openai_body();
    let upstream_oa = MockServer::start_async().await;
    let mock_oa = upstream_oa
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("mur_retrieve");
            then.status(200).body("{}");
        })
        .await;
    let addr_oa = spawn_proxy(upstream_oa.base_url(), true).await;
    let resp_oa = reqwest::Client::new()
        .post(format!("http://{addr_oa}/v1/chat/completions"))
        .json(&body_oa)
        .send()
        .await
        .unwrap();
    assert_eq!(resp_oa.status(), 200);
    mock_oa.assert_async().await;

    // ── Gemini: enabled ──
    let body_gm = fat_gemini_body();
    let upstream_gm = MockServer::start_async().await;
    let mock_gm = upstream_gm
        .mock_async(|when, then| {
            when.method(POST)
                .path_contains("generateContent")
                .body_contains("mur_retrieve");
            then.status(200).body("{}");
        })
        .await;
    let addr_gm = spawn_proxy(upstream_gm.base_url(), true).await;
    let resp_gm = reqwest::Client::new()
        .post(format!(
            "http://{addr_gm}/v1beta/models/gemini-2.5-flash:generateContent"
        ))
        .json(&body_gm)
        .send()
        .await
        .unwrap();
    assert_eq!(resp_gm.status(), 200);
    mock_gm.assert_async().await;
}
