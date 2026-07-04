# Multi-Provider (OpenAI + Gemini) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend cc-proxy to route and CCR-compress tool results for Anthropic, OpenAI, and Gemini through a single instance via path-based auto-detection.

**Architecture:** New `Provider` enum derived from request path selects upstream URL and compression extractor. Three provider-specific extractors traverse each API's JSON shape to find and compress tool results. Disguise remains Anthropic-only.

**Tech Stack:** Rust (edition 2024), axum 0.8, reqwest 0.12, serde_json, mur-compress (path dep)

## Global Constraints

- Zero changes to the mur repo (mur-compress is consumed as-is)
- Compression gate: `CC_PROXY_COMPRESS=1` env var, default off, applies uniformly
- Fail-open: any parse/engine error forwards the original body untouched
- Backward compatible: existing `CC_PROXY_UPSTREAM` env var is a fallback for all three providers
- Disguise layer: Anthropic-only, never touches OpenAI or Gemini traffic

---

### Task 1: Provider enum, detection, and upstream routing in lib.rs

**Files:**
- Modify: `src/lib.rs` — all locations noted below

**Interfaces:**
- Produces: `Provider` enum (3 variants), `detect_provider(path) -> Provider`, `AppState::upstream_for(path) -> &str`, updated `AppState::new()` signature
- Consumes: nothing new (uses existing `DEFAULT_UPSTREAM`)

- [ ] **Step 1: Add Provider enum and detection function**

Add after the existing constants block (`pub const MAX_BODY_BYTES: ...`):

```rust
/// Which LLM API provider a request targets, derived from its path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Provider {
    Anthropic,
    OpenAI,
    Gemini,
}

/// Map a request path to its provider. Falls back to Anthropic for unrecognised paths.
pub fn detect_provider(path: &str) -> Provider {
    if path == "/v1/messages"
        || path.starts_with("/v1/messages/")
        || path.starts_with("/v1/messages?")
    {
        return Provider::Anthropic;
    }
    if path.starts_with("/v1/chat/completions")
        || path.starts_with("/v1/embeddings")
        || path.starts_with("/v1/models")
        || path.starts_with("/v1/images")
        || path.starts_with("/v1/files")
        || path.starts_with("/v1/threads")
        || path.starts_with("/v1/assistants")
    {
        return Provider::OpenAI;
    }
    if path.starts_with("/v1beta/models") {
        return Provider::Gemini;
    }
    Provider::Anthropic
}
```

- [ ] **Step 2: Add upstream constants and update AppState struct**

Replace the existing `DEFAULT_UPSTREAM` constant with:

```rust
pub const DEFAULT_UPSTREAM_ANTHROPIC: &str = "https://api.anthropic.com";
pub const DEFAULT_UPSTREAM_OPENAI: &str = "https://api.openai.com";
pub const DEFAULT_UPSTREAM_GEMINI: &str = "https://generativelanguage.googleapis.com";
/// Backward-compatible alias — points to Anthropic.
pub const DEFAULT_UPSTREAM: &str = DEFAULT_UPSTREAM_ANTHROPIC;
```

Update `AppState` struct — replace `pub upstream: String` with:

```rust
#[derive(Clone)]
pub struct AppState {
    pub upstream_anthropic: String,
    pub upstream_openai: String,
    pub upstream_gemini: String,
    pub client: reqwest::Client,
    pub token_source: TokenSource,
    pub version_cache: Arc<cc_version::VersionCache>,
    pub compress: bool,
}
```

- [ ] **Step 3: Update AppState constructors and add upstream_for()**

Replace the existing `AppState` impl block:

```rust
impl AppState {
    pub fn new(
        upstream_anthropic: impl Into<String>,
        upstream_openai: impl Into<String>,
        upstream_gemini: impl Into<String>,
        token_source: TokenSource,
    ) -> anyhow::Result<Self> {
        Self::with_version(
            upstream_anthropic,
            upstream_openai,
            upstream_gemini,
            token_source,
            Arc::new(cc_version::VersionCache::detect_or_fallback()),
        )
    }

    pub fn with_version(
        upstream_anthropic: impl Into<String>,
        upstream_openai: impl Into<String>,
        upstream_gemini: impl Into<String>,
        token_source: TokenSource,
        version_cache: Arc<cc_version::VersionCache>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            upstream_anthropic: upstream_anthropic.into().trim_end_matches('/').to_string(),
            upstream_openai: upstream_openai.into().trim_end_matches('/').to_string(),
            upstream_gemini: upstream_gemini.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(UPSTREAM_TIMEOUT)
                .build()
                .context("reqwest client")?,
            token_source,
            version_cache,
            compress: std::env::var("CC_PROXY_COMPRESS").is_ok_and(|v| v == "1"),
        })
    }

    /// Return the upstream URL for the provider inferred from `path`.
    pub fn upstream_for(&self, path: &str) -> &str {
        match detect_provider(path) {
            Provider::Anthropic => &self.upstream_anthropic,
            Provider::OpenAI => &self.upstream_openai,
            Provider::Gemini => &self.upstream_gemini,
        }
    }
}
```

- [ ] **Step 4: Update forward() — provider-aware routing, compression dispatch, disguise gate**

Replace the existing `forward()` function (lines 123–283 in current lib.rs):

In the target_url line, change from:
```rust
let target_url = format!("{}{}", state.upstream, path_and_query);
```
to:
```rust
let provider = detect_provider(path_only);
let target_url = format!("{}{}", state.upstream_for(path_only), path_and_query);
```

Update the compression gate — replace:
```rust
let body_bytes = if state.compress && compress::should_compress(path_only) {
    match compress::rewrite_request_body(&body_bytes) {
```
with:
```rust
let body_bytes = if state.compress && compress::should_compress(path_only, provider) {
    match compress::rewrite_request_body(&body_bytes, provider) {
```

Update the disguise gate — replace:
```rust
let override_token: Option<String> = if !disguise_enabled || !on_messages_path {
```
with:
```rust
let override_token: Option<String> = if !disguise_enabled || !on_messages_path || provider != Provider::Anthropic {
```

Update the log line at the end of `forward()` — replace:
```rust
tracing::debug!(
    method = %parts.method,
    path = %path_and_query,
    status = %status,
    disguise = override_token.is_some(),
    "proxied"
);
```
with:
```rust
tracing::debug!(
    method = %parts.method,
    path = %path_and_query,
    status = %status,
    provider = ?provider,
    disguise = override_token.is_some(),
    "proxied"
);
```

- [ ] **Step 5: Update existing tests for new AppState constructor**

Update `appstate_strips_trailing_slash` test:
```rust
#[test]
fn appstate_strips_trailing_slash() {
    let s = AppState::new(
        "https://api.anthropic.com/",
        "https://api.openai.com/",
        "https://generativelanguage.googleapis.com/",
        TokenSource::Disabled,
    )
    .unwrap();
    assert_eq!(s.upstream_anthropic, "https://api.anthropic.com");
    assert_eq!(s.upstream_openai, "https://api.openai.com");
    assert_eq!(s.upstream_gemini, "https://generativelanguage.googleapis.com");
}
```

Add provider detection unit tests at the bottom of the `#[cfg(test)] mod tests` block:

```rust
#[test]
fn detect_provider_anthropic() {
    assert_eq!(detect_provider("/v1/messages"), Provider::Anthropic);
    assert_eq!(detect_provider("/v1/messages?beta=true"), Provider::Anthropic);
    assert_eq!(detect_provider("/v1/messages/count_tokens"), Provider::Anthropic);
}

#[test]
fn detect_provider_openai() {
    assert_eq!(detect_provider("/v1/chat/completions"), Provider::OpenAI);
    assert_eq!(detect_provider("/v1/chat/completions?stream=true"), Provider::OpenAI);
    assert_eq!(detect_provider("/v1/embeddings"), Provider::OpenAI);
    assert_eq!(detect_provider("/v1/models"), Provider::OpenAI);
}

#[test]
fn detect_provider_gemini() {
    assert_eq!(detect_provider("/v1beta/models/gemini-2.5-flash:generateContent"), Provider::Gemini);
    assert_eq!(detect_provider("/v1beta/models/gemini-2.5-flash:streamGenerateContent"), Provider::Gemini);
}

#[test]
fn detect_provider_fallback() {
    assert_eq!(detect_provider("/"), Provider::Anthropic);
    assert_eq!(detect_provider("/v1/unknown"), Provider::Anthropic);
}

#[test]
fn upstream_for_resolves_correctly() {
    let s = AppState::new(
        "https://api.anthropic.com",
        "https://api.openai.com",
        "https://generativelanguage.googleapis.com",
        TokenSource::Disabled,
    )
    .unwrap();
    assert_eq!(s.upstream_for("/v1/messages"), "https://api.anthropic.com");
    assert_eq!(s.upstream_for("/v1/chat/completions"), "https://api.openai.com");
    assert_eq!(
        s.upstream_for("/v1beta/models/gemini-2.5-flash:generateContent"),
        "https://generativelanguage.googleapis.com"
    );
}
```

- [ ] **Step 6: Run tests to verify**

Run: `cargo test -p cc-proxy`
Expected: compile error — `compress::should_compress` and `compress::rewrite_request_body` signature mismatch (we haven't updated compress.rs yet). This is expected; Task 2 fixes it.

- [ ] **Step 7: Commit**

```bash
git add src/lib.rs
git commit -m "feat: add Provider enum, detection, and upstream routing"
```

---

### Task 2: OpenAI tool_result extractor in compress.rs

**Files:**
- Modify: `src/compress.rs`

**Interfaces:**
- Produces: `should_compress(path, provider) -> bool`, `rewrite_request_body(body, provider) -> Option<Vec<u8>>`, `rewrite_tool_results_openai(engine, min_tokens, body) -> Option<Vec<u8>>`
- Consumes: `Provider` enum from lib.rs (Task 1), existing `CompressEngine`, `compress_text()`, `has_retrieve_marker()`

- [ ] **Step 1: Rename existing Anthropic extractor for clarity**

Rename `rewrite_tool_results` to `rewrite_tool_results_anthropic` (function and all test call sites).

In the function definition (line 42): `pub fn rewrite_tool_results(` → `fn rewrite_tool_results_anthropic(`

Update the call in `rewrite_request_body` (line 129): `rewrite_tool_results(&engine, min_tokens, body)` → `rewrite_tool_results_anthropic(&engine, min_tokens, body)`

Update all test call sites (lines 206, 228, 243, 251, 257, 260, 268, 275): `rewrite_tool_results(` → `rewrite_tool_results_anthropic(`

- [ ] **Step 2: Update should_compress to accept Provider**

Replace:
```rust
pub fn should_compress(path: &str) -> bool {
    path == "/v1/messages" || path.starts_with("/v1/messages?")
}
```

With:
```rust
pub fn should_compress(path: &str, provider: Provider) -> bool {
    match provider {
        Provider::Anthropic => path == "/v1/messages" || path.starts_with("/v1/messages?"),
        Provider::OpenAI => {
            path == "/v1/chat/completions" || path.starts_with("/v1/chat/completions?")
        }
        Provider::Gemini => path.starts_with("/v1beta/models/"),
    }
}
```

Add `use crate::Provider;` at the top of `compress.rs`.

- [ ] **Step 3: Update rewrite_request_body dispatch**

Replace:
```rust
pub fn rewrite_request_body(body: &[u8]) -> Option<Vec<u8>> {
    let (engine, min_tokens) = build_engine()?;
    rewrite_tool_results(&engine, min_tokens, body)
}
```

With:
```rust
pub fn rewrite_request_body(body: &[u8], provider: Provider) -> Option<Vec<u8>> {
    let (engine, min_tokens) = build_engine()?;
    match provider {
        Provider::Anthropic => rewrite_tool_results_anthropic(&engine, min_tokens, body),
        Provider::OpenAI => rewrite_tool_results_openai(&engine, min_tokens, body),
        Provider::Gemini => rewrite_tool_results_gemini(&engine, min_tokens, body),
    }
}
```

- [ ] **Step 4: Add OpenAI extractor function**

Add after the `rewrite_tool_results_anthropic` function (after its closing `}`):

```rust
/// Compress oversized `content` fields in OpenAI `role: "tool"` messages.
/// Sibling fields (`role`, `tool_call_id`) are preserved in-place.
fn rewrite_tool_results_openai(
    engine: &CompressEngine,
    min_tokens: usize,
    body: &[u8],
) -> Option<Vec<u8>> {
    let mut root: Value = serde_json::from_slice(body).ok()?;
    let messages = root.get_mut("messages")?.as_array_mut()?;
    let mut changed = false;
    for msg in messages.iter_mut() {
        if msg.get("role").and_then(|r| r.as_str()) != Some("tool") {
            continue;
        }
        let Some(content) = msg.get_mut("content") else {
            continue;
        };
        match content {
            Value::String(s) => changed |= compress_text(engine, min_tokens, s),
            Value::Array(items) => {
                for item in items.iter_mut() {
                    if item.get("type").and_then(|t| t.as_str()) != Some("text") {
                        continue;
                    }
                    if let Some(Value::String(s)) = item.get_mut("text") {
                        changed |= compress_text(engine, min_tokens, s);
                    }
                }
            }
            _ => {}
        }
    }
    if !changed {
        return None;
    }
    serde_json::to_vec(&root).ok()
}
```

- [ ] **Step 5: Update should_compress test for new signature**

Replace the existing `should_compress_matches_messages_send_only` test with:

```rust
#[test]
fn should_compress_per_provider() {
    use crate::Provider;

    // Anthropic
    assert!(should_compress("/v1/messages", Provider::Anthropic));
    assert!(should_compress("/v1/messages?beta=true", Provider::Anthropic));
    assert!(!should_compress("/v1/messages/count_tokens", Provider::Anthropic));

    // OpenAI
    assert!(should_compress("/v1/chat/completions", Provider::OpenAI));
    assert!(should_compress("/v1/chat/completions?stream=true", Provider::OpenAI));
    assert!(!should_compress("/v1/chat/completions/messages", Provider::OpenAI));

    // Gemini — any /v1beta/models/ path
    assert!(should_compress(
        "/v1beta/models/gemini-2.5-flash:generateContent",
        Provider::Gemini
    ));
}
```

- [ ] **Step 6: Add OpenAI unit tests**

Add after the existing Anthropic tests:

```rust
fn body_with_openai_tool_result(content: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": "gpt-4o",
        "messages": [
            {"role": "assistant", "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": content}
        ]
    }))
    .unwrap()
}

#[test]
fn openai_compresses_string_tool_content() {
    let (_dir, engine) = test_engine();
    let body = body_with_openai_tool_result(json!(fat_log()));
    let out = rewrite_tool_results_openai(&engine, 800, &body).expect("should fire");
    assert!(out.len() < body.len(), "rewritten body must be smaller");

    let v: Value = serde_json::from_slice(&out).unwrap();
    let tool_msg = &v["messages"][1];
    assert_eq!(tool_msg["tool_call_id"], "call_1");
    assert!(has_retrieve_marker(tool_msg["content"].as_str().unwrap()));
}

#[test]
fn openai_skips_small_blocks() {
    let (_dir, engine) = test_engine();
    let body = body_with_openai_tool_result(json!("short output"));
    assert!(rewrite_tool_results_openai(&engine, 800, &body).is_none());
}

#[test]
fn openai_idempotent_second_pass_is_noop() {
    let (_dir, engine) = test_engine();
    let body = body_with_openai_tool_result(json!(fat_log()));
    let once = rewrite_tool_results_openai(&engine, 800, &body).expect("first pass fires");
    assert!(
        rewrite_tool_results_openai(&engine, 800, &once).is_none(),
        "second pass must not double-compress"
    );
}

#[test]
fn openai_skips_non_tool_roles() {
    let (_dir, engine) = test_engine();
    // A body with no role=tool messages — should pass through.
    let body = serde_json::to_vec(&json!({
        "model": "gpt-4o",
        "messages": [
            {"role": "system", "content": fat_log()},
            {"role": "user", "content": fat_log()},
            {"role": "assistant", "content": fat_log()}
        ]
    }))
    .unwrap();
    assert!(rewrite_tool_results_openai(&engine, 800, &body).is_none());
}

#[test]
fn openai_array_form_text_blocks() {
    let (_dir, engine) = test_engine();
    let body = body_with_openai_tool_result(json!([
        {"type": "text", "text": fat_log()}
    ]));
    let out = rewrite_tool_results_openai(&engine, 800, &body).expect("should fire");
    let v: Value = serde_json::from_slice(&out).unwrap();
    let content = &v["messages"][1]["content"];
    let items = content.as_array().unwrap();
    assert!(has_retrieve_marker(items[0]["text"].as_str().unwrap()));
}
```

- [ ] **Step 7: Run tests to verify**

Run: `cargo test -p cc-proxy`
Expected: All tests pass. The new OpenAI extractor compiles and compresses correctly.

- [ ] **Step 8: Commit**

```bash
git add src/compress.rs
git commit -m "feat: OpenAI tool_result extractor in compress layer"
```

---

### Task 3: Gemini tool_result extractor in compress.rs

**Files:**
- Modify: `src/compress.rs`

**Interfaces:**
- Produces: `rewrite_tool_results_gemini(engine, min_tokens, body) -> Option<Vec<u8>>`
- Consumes: existing `CompressEngine`, `compress_text()`, `has_retrieve_marker()`

- [ ] **Step 1: Add Gemini extractor function**

Add after the OpenAI extractor function (after its closing `}`):

```rust
/// Compress oversized `functionResponse` parts in Gemini request bodies.
/// The `functionResponse.response` field is either a string or an object
/// with a `result` key — both are handled. Sibling fields (`name`) survive.
fn rewrite_tool_results_gemini(
    engine: &CompressEngine,
    min_tokens: usize,
    body: &[u8],
) -> Option<Vec<u8>> {
    let mut root: Value = serde_json::from_slice(body).ok()?;
    let contents = root.get_mut("contents")?.as_array_mut()?;
    let mut changed = false;
    for content_item in contents.iter_mut() {
        let Some(parts) = content_item.get_mut("parts").and_then(|p| p.as_array_mut()) else {
            continue;
        };
        for part in parts.iter_mut() {
            let Some(fr) = part.get_mut("functionResponse") else {
                continue;
            };
            let Some(response) = fr.get_mut("response") else {
                continue;
            };
            match response {
                Value::String(s) => changed |= compress_text(engine, min_tokens, s),
                Value::Object(obj) => {
                    if let Some(Value::String(s)) = obj.get_mut("result") {
                        changed |= compress_text(engine, min_tokens, s);
                    }
                }
                _ => {}
            }
        }
    }
    if !changed {
        return None;
    }
    serde_json::to_vec(&root).ok()
}
```

- [ ] **Step 2: Add Gemini unit tests**

Add after the OpenAI tests:

```rust
fn body_with_gemini_function_response(response: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "contents": [
            {"role": "model", "parts": [
                {"functionCall": {"name": "bash", "args": {}}}
            ]},
            {"role": "user", "parts": [
                {"functionResponse": {"name": "bash", "response": response}}
            ]}
        ]
    }))
    .unwrap()
}

#[test]
fn gemini_compresses_object_result() {
    let (_dir, engine) = test_engine();
    let body = body_with_gemini_function_response(json!({"result": fat_log()}));
    let out = rewrite_tool_results_gemini(&engine, 800, &body).expect("should fire");
    assert!(out.len() < body.len(), "rewritten body must be smaller");

    let v: Value = serde_json::from_slice(&out).unwrap();
    let fr = &v["contents"][1]["parts"][0]["functionResponse"];
    assert_eq!(fr["name"], "bash");
    assert!(has_retrieve_marker(fr["response"]["result"].as_str().unwrap()));
}

#[test]
fn gemini_compresses_string_response() {
    let (_dir, engine) = test_engine();
    let body = body_with_gemini_function_response(json!(fat_log()));
    let out = rewrite_tool_results_gemini(&engine, 800, &body).expect("should fire");
    assert!(out.len() < body.len());

    let v: Value = serde_json::from_slice(&out).unwrap();
    let fr = &v["contents"][1]["parts"][0]["functionResponse"];
    assert_eq!(fr["name"], "bash");
    assert!(has_retrieve_marker(fr["response"].as_str().unwrap()));
}

#[test]
fn gemini_skips_small_blocks() {
    let (_dir, engine) = test_engine();
    let body = body_with_gemini_function_response(json!({"result": "short"}));
    assert!(rewrite_tool_results_gemini(&engine, 800, &body).is_none());
}

#[test]
fn gemini_idempotent_second_pass_is_noop() {
    let (_dir, engine) = test_engine();
    let body = body_with_gemini_function_response(json!({"result": fat_log()}));
    let once = rewrite_tool_results_gemini(&engine, 800, &body).expect("first pass fires");
    assert!(
        rewrite_tool_results_gemini(&engine, 800, &once).is_none(),
        "second pass must not double-compress"
    );
}

#[test]
fn gemini_skips_non_function_response_parts() {
    let (_dir, engine) = test_engine();
    // A body with no functionResponse — text parts only — should pass through.
    let body = serde_json::to_vec(&json!({
        "contents": [
            {"role": "user", "parts": [
                {"text": fat_log()},
                {"inlineData": {"mimeType": "image/png", "data": "AAAA"}}
            ]}
        ]
    }))
    .unwrap();
    assert!(rewrite_tool_results_gemini(&engine, 800, &body).is_none());
}
```

- [ ] **Step 3: Run tests to verify**

Run: `cargo test -p cc-proxy`
Expected: All tests pass. Existing Anthropic + new OpenAI + new Gemini tests all green.

- [ ] **Step 4: Commit**

```bash
git add src/compress.rs
git commit -m "feat: Gemini functionResponse extractor in compress layer"
```

---

### Task 4: main.rs env plumbing and integration verification

**Files:**
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: updated `AppState::new(upstream_anthropic, upstream_openai, upstream_gemini, token_source)` from Task 1
- Produces: env var resolution helper for three upstream vars

- [ ] **Step 1: Add upstream resolution helper and update serve() in main.rs**

Replace the upstream resolution in `serve()`:

Current (lines 72-73):
```rust
let upstream =
    std::env::var("CC_PROXY_UPSTREAM").unwrap_or_else(|_| DEFAULT_UPSTREAM.to_string());
```

Replace with:
```rust
let upstream_anthropic = resolve_upstream("CC_PROXY_UPSTREAM_ANTHROPIC", DEFAULT_UPSTREAM_ANTHROPIC);
let upstream_openai = resolve_upstream("CC_PROXY_UPSTREAM_OPENAI", DEFAULT_UPSTREAM_OPENAI);
let upstream_gemini = resolve_upstream("CC_PROXY_UPSTREAM_GEMINI", DEFAULT_UPSTREAM_GEMINI);
```

Replace the AppState construction and log line. Current:
```rust
let state = AppState::new(&upstream, token_source)?;
let upstream_for_log = state.upstream.clone();
// ...
tracing::info!(addr = %bind, upstream = %upstream_for_log, "cc-proxy listening");
```

Replace with:
```rust
let state = AppState::new(
    &upstream_anthropic,
    &upstream_openai,
    &upstream_gemini,
    token_source,
)?;
let upstream_for_log_a = state.upstream_anthropic.clone();
let upstream_for_log_o = state.upstream_openai.clone();
let upstream_for_log_g = state.upstream_gemini.clone();
// ...
tracing::info!(
    addr = %bind,
    upstream_anthropic = %upstream_for_log_a,
    upstream_openai = %upstream_for_log_o,
    upstream_gemini = %upstream_for_log_g,
    "cc-proxy listening"
);
```

Add the helper function at the bottom of `main.rs` (before `shutdown_signal()`):

```rust
/// Resolve an upstream URL: provider-specific var → generic CC_PROXY_UPSTREAM → default.
fn resolve_upstream(provider_var: &str, default: &str) -> String {
    std::env::var(provider_var)
        .or_else(|_| std::env::var("CC_PROXY_UPSTREAM"))
        .unwrap_or_else(|_| default.to_string())
}
```

Update the imports at the top — add `DEFAULT_UPSTREAM_ANTHROPIC`, `DEFAULT_UPSTREAM_OPENAI`, `DEFAULT_UPSTREAM_GEMINI` to the `use cc_proxy::` line, and remove `DEFAULT_UPSTREAM`.

- [ ] **Step 2: Build to verify compilation**

Run: `cargo build -p cc-proxy`
Expected: Compiles cleanly with no warnings.

- [ ] **Step 3: Run full test suite**

Run: `cargo test -p cc-proxy`
Expected: All tests pass (provider detection + Anthropic compression + OpenAI compression + Gemini compression + disguise + hop-by-hop).

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat: plumb provider-specific upstream env vars into main"
```

---

### Task 5: End-to-end smoke test (manual)

**Files:**
- No code changes — verification only

- [ ] **Step 1: Start cc-proxy with compression enabled**

```bash
CC_PROXY_COMPRESS=1 cargo run -- serve &
```

- [ ] **Step 2: Send a synthetic OpenAI request with a fat tool result**

```bash
curl -s -X POST http://127.0.0.1:8088/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer sk-test" \
  -d '{
    "model": "gpt-4o",
    "messages": [
      {"role": "assistant", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]},
      {"role": "tool", "tool_call_id": "call_1", "content": "'"$(head -c 10000 /dev/urandom | base64)"'"}
    ]
  }' | head -c 200
```

Expected: Request reaches OpenAI API (auth error is fine — verify it's NOT hitting Anthropic's error format). The request body sent to upstream should be compressed (smaller than original).

- [ ] **Step 3: Send a synthetic Gemini request**

```bash
curl -s -X POST "http://127.0.0.1:8088/v1beta/models/gemini-2.5-flash:generateContent?key=test" \
  -H "Content-Type: application/json" \
  -d '{
    "contents": [
      {"role": "model", "parts": [{"functionCall": {"name": "bash", "args": {}}}]},
      {"role": "user", "parts": [{"functionResponse": {"name": "bash", "response": {"result": "'"$(head -c 10000 /dev/urandom | base64)"'"}}}]}
    ]
  }' | head -c 200
```

Expected: Request reaches Gemini API (auth error is fine — verify it's hitting the right host).

- [ ] **Step 4: Verify Anthropic disguise still works (regression check)**

```bash
curl -s -X POST http://127.0.0.1:8088/v1/messages \
  -H "Content-Type: application/json" \
  -H "anthropic-version: 2023-06-01" \
  -d '{
    "model": "claude-sonnet-5",
    "max_tokens": 16,
    "messages": [{"role": "user", "content": "say hi"}]
  }' | head -c 200
```

Expected: Response from Anthropic API with disguise applied (auth is injected from keychain).

- [ ] **Step 5: Stop the proxy**

```bash
kill %1
```

- [ ] **Step 6: Commit any final adjustments**

(Only if the smoke test reveals issues.)

---

## Self-Review

1. **Spec coverage:**
   - Routing (Provider enum, detect_provider, upstream_for) → Task 1 ✓
   - Anthropic extractor preserved → Task 2 (rename only) ✓
   - OpenAI extractor with string + array content shapes → Task 2 ✓
   - Gemini extractor with object result + string response shapes → Task 3 ✓
   - Disguise Anthropic-only gate → Task 1 (forward() condition) ✓
   - Env vars with CC_PROXY_UPSTREAM fallback → Task 4 ✓
   - Rollout gate (CC_PROXY_COMPRESS=1) unchanged → not modified, existing logic preserved ✓
   - Fail-open behavior unchanged → not modified ✓
   - Skip rules unchanged → not modified ✓
   - Tests per spec → Tasks 1-3 ✓
   - Integration verification → Task 5 ✓
   - YAGNI non-goals → nothing implements response compression, disguise for non-Anthropic, or SSE ✓

2. **Placeholder scan:** No "TBD", "TODO", or vague descriptions. Every step has exact code. ✓

3. **Type consistency:**
   - `Provider` enum defined in Task 1, consumed in Tasks 2-4 ✓
   - `detect_provider(path: &str) -> Provider` defined in Task 1, used in `upstream_for()` and `forward()` ✓
   - `upstream_for(path: &str) -> &str` defined in Task 1 ✓
   - `should_compress(path: &str, provider: Provider) -> bool` signature matches between Task 2 definition and Task 1 call site ✓
   - `rewrite_request_body(body: &[u8], provider: Provider) -> Option<Vec<u8>>` signature matches between Task 2 definition and Task 1 call site ✓
   - `AppState::new(upstream_anthropic, upstream_openai, upstream_gemini, token_source)` matches between Task 1 definition and Task 4 call site ✓
   - `resolve_upstream(provider_var: &str, default: &str) -> String` used in Task 4 ✓
