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
