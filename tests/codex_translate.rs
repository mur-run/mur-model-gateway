//! The translated Codex path: what actually reaches the upstream, and what
//! comes back to the client.

use httpmock::prelude::*;
use mur_model_gateway::{AppState, TokenSource, build_router};
use serde_json::{Value, json};
use std::sync::Arc;

/// Start a gateway whose Codex upstream is `upstream`. Mirrors `spawn_proxy`
/// in tests/passthrough.rs. `compress` toggles the wire-level rewriter that
/// Task 10 exercises.
async fn spawn_gateway(upstream: &str, compress: bool) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut state = AppState::new(upstream, upstream, upstream, TokenSource::Disabled)
        .unwrap()
        .with_upstream_codex(upstream)
        .with_token_source_codex(TokenSource::Static(Arc::new("codex-tok".to_string())));
    state.compress = compress;
    let app = build_router(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr.to_string()
}

/// Start a gateway whose Codex route runs in API-key mode: the credential
/// comes from a temp `auth.json` whose `auth_mode` is "apikey", and the
/// API-key upstream is pointed at the mock (the production default is the
/// hard-coded api.openai.com — see the plan's deviation note). Returns the
/// address and the TempDir; the TempDir must stay alive for the test body,
/// because the gateway re-reads auth.json on every request.
async fn spawn_apikey_gateway(upstream: &str, auth_json: &str) -> (String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("auth.json"), auth_json).unwrap();
    let path = dir.path().join("auth.json");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut state = AppState::new(upstream, upstream, upstream, TokenSource::Disabled)
        .unwrap()
        .with_upstream_codex_apikey(upstream)
        .with_token_source_codex(TokenSource::Codex(path));
    state.compress = false;
    let app = build_router(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr.to_string(), dir)
}

async fn post(gw: &str, path: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{gw}{path}"))
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .expect("gateway request")
}

/// httpmock's `matches` takes a plain fn pointer (`MockMatcherFunction`),
/// not a closure — hence the free function.
fn is_translated_responses_body(req: &HttpMockRequest) -> bool {
    let Some(body) = req.body.as_ref() else {
        return false;
    };
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    // `stream` is always true: the upstream has no non-streaming mode.
    v.get("input").is_some()
        && v.get("messages").is_none()
        && v["store"] == json!(false)
        && v["stream"] == json!(true)
}

/// True only if the body is still in Chat Completions shape.
fn is_untranslated_chat_body(req: &HttpMockRequest) -> bool {
    let Some(body) = req.body.as_ref() else {
        return false;
    };
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    v.get("messages").is_some() && v.get("input").is_none()
}

/// True only if the request carries none of the ChatGPT client headers the
/// OAuth `apply_codex_headers` hook would set. API-key mode sends the key
/// and nothing else.
///
/// httpmock 0.7 exposes headers as `Option<Vec<(String, String)>>` — a list,
/// not a map — so this scans it. Absent headers (`None`) trivially satisfy
/// the absence check.
fn has_no_chatgpt_account_id(req: &HttpMockRequest) -> bool {
    req.headers.as_ref().is_none_or(|hs| {
        !hs.iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("chatgpt-account-id"))
    })
}

/// The upstream body must be translated AND smaller than the fat input.
/// If compression ever ran after translation the rewriter would see a
/// Responses body it does not understand and silently do nothing.
fn is_translated_and_compressed(req: &HttpMockRequest) -> bool {
    let Some(body) = req.body.as_ref() else {
        return false;
    };
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    v.get("input").is_some() && body.len() < 50_000
}

/// A minimal reply in the shape the real backend sends: SSE, with the output
/// items on `response.output_item.done` and an EMPTY `output` on
/// `response.completed`.
const SSE_REPLY: &str = concat!(
    "event: response.created\n",
    r#"data: {"type":"response.created","response":{"id":"resp_1","created_at":1,"model":"gpt-5.4","status":"in_progress"}}"#,
    "\n\n",
    "event: response.output_text.delta\n",
    r#"data: {"type":"response.output_text.delta","delta":"hi back"}"#,
    "\n\n",
    "event: response.output_item.done\n",
    r#"data: {"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"hi back"}]}}"#,
    "\n\n",
    "event: response.completed\n",
    r#"data: {"type":"response.completed","response":{"id":"resp_1","created_at":1,"model":"gpt-5.4","status":"completed","incomplete_details":null,"output":[],"usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}"#,
    "\n\n",
);

const APICKEY_AUTH: &str = r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-test-key","tokens":null}"#;

#[tokio::test]
async fn translates_request_and_aggregates_the_sse_reply() {
    let upstream = MockServer::start_async().await;
    let m = upstream
        .mock_async(|when, then| {
            // The mock matches ONLY a translated body, so a hit proves
            // translation happened.
            when.method(POST)
                .path("/responses")
                .matches(is_translated_responses_body);
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(SSE_REPLY);
        })
        .await;

    let gw = spawn_gateway(&upstream.base_url(), false).await;
    // stream is absent, so the client wants a single JSON reply — even
    // though the upstream can only answer with SSE.
    let resp = post(
        &gw,
        "/codex/v1/chat/completions",
        json!({"model": "gpt-5.4", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()["content-type"],
        "application/json",
        "the upstream's text/event-stream must not leak to the client"
    );
    let out: Value = resp.json().await.unwrap();
    assert_eq!(out["object"], json!("chat.completion"));
    assert_eq!(out["model"], json!("gpt-5.4"));
    assert_eq!(out["choices"][0]["message"]["content"], json!("hi back"));
    assert_eq!(out["choices"][0]["finish_reason"], json!("stop"));
    assert_eq!(out["usage"]["prompt_tokens"], json!(3));
    m.assert_async().await;
}

#[tokio::test]
async fn client_stream_false_still_asks_the_upstream_to_stream() {
    let upstream = MockServer::start_async().await;
    let m = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/responses")
                .matches(is_translated_responses_body);
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(SSE_REPLY);
        })
        .await;

    let gw = spawn_gateway(&upstream.base_url(), false).await;
    let resp = post(
        &gw,
        "/codex/v1/chat/completions",
        json!({"model": "gpt-5.4", "stream": false,
               "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;

    assert_eq!(resp.status(), 200);
    let out: Value = resp.json().await.unwrap();
    assert_eq!(out["object"], json!("chat.completion"));
    m.assert_async().await;
}

#[tokio::test]
async fn rejected_parameter_is_a_400_before_any_upstream_call() {
    let upstream = MockServer::start_async().await;
    let m = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/responses");
            then.status(200).body("{}");
        })
        .await;

    let gw = spawn_gateway(&upstream.base_url(), false).await;
    let resp = post(
        &gw,
        "/codex/v1/chat/completions",
        json!({"model": "m", "messages": [], "seed": 42}),
    )
    .await;

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert!(body["error"]["message"].as_str().unwrap().contains("seed"));
    m.assert_hits_async(0).await;
}

#[tokio::test]
async fn plain_openai_path_is_still_untranslated() {
    let upstream = MockServer::start_async().await;
    let m = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .matches(is_untranslated_chat_body);
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"ok":true}"#);
        })
        .await;

    let gw = spawn_gateway(&upstream.base_url(), false).await;
    let resp = post(
        &gw,
        "/v1/chat/completions",
        json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;

    assert_eq!(resp.status(), 200);
    m.assert_async().await;
}

/// Collect the `data:` payloads of an SSE response, in order.
async fn post_sse(gw: &str, path: &str, body: Value) -> Vec<String> {
    let resp = reqwest::Client::new()
        .post(format!("http://{gw}{path}"))
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .json(&body)
        .send()
        .await
        .expect("gateway request");
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    text.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(|d| d.trim().to_string())
        .collect()
}

fn fixture_sse(name: &str) -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/codex/");
    std::fs::read_to_string(format!("{path}{name}")).expect("run Task 1 first")
}

#[tokio::test]
async fn translates_a_streaming_response() {
    let upstream = MockServer::start_async().await;
    let _m = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/responses");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(fixture_sse("streaming.sse"));
        })
        .await;

    let gw = spawn_gateway(&upstream.base_url(), false).await;
    let frames = post_sse(
        &gw,
        "/codex/v1/chat/completions",
        json!({
            "model": "gpt-5-codex",
            "messages": [{"role": "user", "content": "count"}],
            "stream": true
        }),
    )
    .await;

    assert!(!frames.is_empty());
    let first: Value = serde_json::from_str(&frames[0]).unwrap();
    assert_eq!(first["object"], json!("chat.completion.chunk"));
    assert_eq!(first["choices"][0]["delta"]["role"], json!("assistant"));

    let text: String = frames
        .iter()
        .filter_map(|f| serde_json::from_str::<Value>(f).ok())
        .filter_map(|c| {
            c["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_string)
        })
        .collect();
    assert!(!text.is_empty(), "content must survive translation");

    assert_eq!(frames.last().unwrap(), "[DONE]");
}

#[tokio::test]
async fn streams_tool_calls() {
    let upstream = MockServer::start_async().await;
    let _m = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/responses");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(fixture_sse("toolcall-streaming.sse"));
        })
        .await;

    let gw = spawn_gateway(&upstream.base_url(), false).await;
    let frames = post_sse(
        &gw,
        "/codex/v1/chat/completions",
        json!({"model": "m", "messages": [], "stream": true}),
    )
    .await;

    let args: String = frames
        .iter()
        .filter_map(|f| serde_json::from_str::<Value>(f).ok())
        .filter_map(|c| {
            c["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .map(str::to_string)
        })
        .collect();
    serde_json::from_str::<Value>(&args).expect("arguments must reassemble into JSON");
}

/// Log-shaped text large enough that mur-compress reliably fires. A pure run
/// of repeated characters (`"x".repeat(50_000)`) is deliberately NOT used:
/// mur-compress's payoff logic refuses to compress degenerate input
/// (`tokens_saved == 0`), so the body would never shrink below the matcher's
/// 50_000-byte bar and the ordering claim would be untestable.
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

#[tokio::test]
async fn compression_runs_before_translation_on_the_codex_path() {
    // Point the wire rewriter at a fresh store so the test does not depend on
    // the developer's ~/.mur config (same isolation as compress_e2e).
    let mur_home = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("MUR_HOME", mur_home.path()) };
    let fat = fat_log();
    let upstream = MockServer::start_async().await;
    let m = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/responses")
                .matches(is_translated_and_compressed);
            // The upstream is streaming-only, and the client asked for a plain
            // reply — the gateway aggregates (Task 8).
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(SSE_REPLY);
        })
        .await;

    let gw = spawn_gateway(&upstream.base_url(), true).await;
    let resp = post(
        &gw,
        "/codex/v1/chat/completions",
        json!({
            "model": "m",
            "messages": [
                {"role": "assistant", "tool_calls": [
                    {"id": "c1", "function": {"name": "f", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "c1", "content": fat}
            ]
        }),
    )
    .await;

    assert_eq!(resp.status(), 200);
    m.assert_async().await;
}

#[tokio::test]
async fn apikey_mode_keeps_v1_and_sends_bearer_key() {
    let upstream = MockServer::start_async().await;
    let m = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/responses")
                .header("authorization", "Bearer sk-test-key")
                .matches(has_no_chatgpt_account_id);
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"ok":true}"#);
        })
        .await;

    let (gw, _dir) = spawn_apikey_gateway(&upstream.base_url(), APICKEY_AUTH).await;
    let resp = post(&gw, "/v1/responses", json!({"model": "m", "input": "hi"})).await;

    assert_eq!(resp.status(), 200);
    // The mock matches ONLY `/v1/responses` carrying `Bearer sk-test-key`
    // and no `chatgpt-account-id` header. OAuth mode would hit `/responses`,
    // never this key, and would attach the account-id header — so a hit
    // proves all three API-key facts at once.
    m.assert_async().await;
}

#[tokio::test]
async fn apikey_401_is_returned_unchanged_with_no_retry() {
    let upstream = MockServer::start_async().await;
    let m = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/v1/responses");
            then.status(401).body(r#"{"error":"invalid api key"}"#);
        })
        .await;

    let (gw, _dir) = spawn_apikey_gateway(&upstream.base_url(), APICKEY_AUTH).await;
    let resp = post(&gw, "/v1/responses", json!({"model": "m", "input": "hi"})).await;

    assert_eq!(resp.status(), 401);
    // Exactly one upstream hit: API-key mode has no refresh token to redeem,
    // so a 401 must not trigger the OAuth refresh-and-retry path.
    m.assert_hits_async(1).await;
}

#[tokio::test]
async fn apikey_mode_translates_chat_completions_to_v1_responses() {
    let upstream = MockServer::start_async().await;
    let m = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/responses")
                .matches(is_translated_responses_body);
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(SSE_REPLY);
        })
        .await;

    let (gw, _dir) = spawn_apikey_gateway(&upstream.base_url(), APICKEY_AUTH).await;
    // stream is absent, so the client wants a single JSON reply — the gateway
    // translates, asks the upstream to stream, and aggregates.
    let resp = post(
        &gw,
        "/codex/v1/chat/completions",
        json!({"model": "gpt-5.4", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()["content-type"],
        "application/json",
        "the upstream's text/event-stream must not leak to the client"
    );
    let out: Value = resp.json().await.unwrap();
    assert_eq!(out["object"], json!("chat.completion"));
    assert_eq!(out["choices"][0]["message"]["content"], json!("hi back"));
    // The upstream saw a translated Responses body at /v1/responses — the
    // OAuth translate route targets /responses instead, so this proves the
    // mode-aware path.
    m.assert_async().await;
}
