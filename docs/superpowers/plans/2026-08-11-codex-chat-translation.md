# Codex Chat Completions Translation (Stage 2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a MUR agent's ordinary OpenAI client hold a tool-using, optionally streaming conversation against a ChatGPT subscription, by translating Chat Completions requests into the Responses API and back.

**Architecture:** A new tracked module `src/translate.rs` holds pure functions — `chat_to_responses`, `responses_to_chat`, and an `SseTranslator` state machine — that never touch the network. `forward()` in `src/lib.rs` gains one branch that applies them on the `/codex/v1/chat/completions` path. Everything stage 1 built (credential resolution, `apply_codex_headers`, refresh-on-401) is reused unchanged.

**Tech Stack:** Rust, axum 0.8, reqwest 0.12, serde_json 1, tokio 1, futures-util 0.3. No new dependencies.

## Global Constraints

- **No new Cargo dependencies, and no new features on existing ones.** Everything needed is already
  in `Cargo.toml`. In particular: `async-stream` and `tokio-stream` are **not** available, and
  `tokio` is built with only `macros, rt-multi-thread, net, signal` — so `tokio::sync::mpsc` is not
  available either. Streaming bodies are built with `futures_util::stream::unfold`.
- **Integration tests use `httpmock` 0.7** with a local `spawn_proxy` helper per test file, matching
  `tests/passthrough.rs` and `tests/codex.rs`. Do not add a `tests/common/` module.
- `AppState::new(anthropic, openai, gemini, token_source)` takes four upstreams; Codex is configured
  afterwards with `.with_upstream_codex(url)` and `.with_token_source_codex(TokenSource::Static(...))`.
  `state.compress` is a public field.
- **`/v1/chat/completions` and `/v1/responses` must not change behaviour.** Every task that touches `forward()` keeps the existing passthrough tests green.
- **Translation code is tracked source.** It never goes into `src/codex/codex_impl.rs`, which is gitignored.
- **Tests never touch the network.** They read fixtures captured in Task 1.
- **Tests are inline `#[cfg(test)] mod tests`** in the module they cover, matching `src/lib.rs` and `src/compress.rs`. Cross-module integration tests go in `tests/`, matching `tests/passthrough.rs`.
- **`cargo fmt --check` and `cargo clippy` must pass** before every commit.
- **The Codex upstream is streaming-only.** A Responses request without `stream: true` is rejected
  with `400 {"detail":"Stream must be set to true"}`. `chat_to_responses` always sets `stream: true`,
  and a non-streaming Chat Completions request is served by aggregating the SSE reply. The client's
  `stream` flag — not the upstream's content type — decides what the client receives.
- Spec: `docs/superpowers/specs/2026-08-11-codex-chat-translation-design.md`.

### Deviation from the spec, recorded

The spec says routing returns `(Provider::Codex, translate: bool)`. Changing `detect_provider`'s
return type would churn every one of its call sites and roughly ten existing assertions for no
behavioural gain. Task 2 instead adds a sibling predicate `codex::should_translate(path)` next to the
existing `codex::should_route(path)`. Same decision, less blast radius.

---

### Task 1: Capture ground-truth fixtures and correct the mapping tables

> **✅ COMPLETE — done by the controller on 2026-08-11, before any other task.** Fixtures are
> committed at `tests/fixtures/codex/`; the spec's tables are corrected. The steps below are kept as
> the record of how they were captured. **Read the four findings — Tasks 3, 6, 8 and 9 below were
> rewritten because of them.**
>
> 1. **The upstream is streaming-only.** Omitting `stream: true` returns
>    `400 {"detail":"Stream must be set to true"}`. There is no non-streaming reply to translate.
> 2. **`response.completed` has `usage` and `status` but an empty `output`.** Items arrive
>    individually on `response.output_item.done`, each a complete Responses output item.
> 3. **Model names are account-tier dependent.** `gpt-5-codex` and `gpt-5` are rejected; `gpt-5.4`
>    and `gpt-5.5` work. Fixtures use `gpt-5.4`.
> 4. **Every guessed event and field name was correct**, and the captured tool call splits its
>    arguments across six `response.function_call_arguments.delta` events.

The spec's mapping tables and SSE event names are written from the public Responses API shape and
have **not** been verified against live Codex. The streaming event names are the likeliest to be
wrong. Nothing else in this plan is trustworthy until this task is done.

**This task needs a live ChatGPT subscription and a running gateway.** It is the one task that
cannot be done offline.

**Files:**
- Create: `tests/fixtures/codex/nonstreaming.json`
- Create: `tests/fixtures/codex/streaming.sse`
- Create: `tests/fixtures/codex/toolcall.json`
- Create: `tests/fixtures/codex/toolcall-streaming.sse`
- Modify: `docs/superpowers/specs/2026-08-11-codex-chat-translation-design.md` (correct the tables)

**Interfaces:**
- Consumes: nothing.
- Produces: four fixture files that every later task reads, and corrected mapping tables that every later task implements.

- [ ] **Step 1: Start the gateway against the real Codex backend**

```bash
cargo run --release -- --port 8099
```

Leave it running in a second terminal. Stage 1's route is already live at `/v1/responses`.

- [ ] **Step 2: Capture a non-streaming response**

```bash
mkdir -p tests/fixtures/codex
curl -sS -X POST http://127.0.0.1:8099/v1/responses \
  -H 'content-type: application/json' \
  -d '{"model":"gpt-5-codex","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"say ok"}]}],"store":false}' \
  > tests/fixtures/codex/nonstreaming.json
cat tests/fixtures/codex/nonstreaming.json
```

Expected: a JSON object with `output`, `usage`, and `status` fields. If it is a 401, run
`codex login` and retry. If `output` is absent, stop — the rest of this plan assumes it exists.

- [ ] **Step 3: Capture a streaming response**

```bash
curl -sS -N -X POST http://127.0.0.1:8099/v1/responses \
  -H 'content-type: application/json' \
  -H 'accept: text/event-stream' \
  -d '{"model":"gpt-5-codex","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"count to five"}]}],"stream":true,"store":false}' \
  > tests/fixtures/codex/streaming.sse
grep '^event:' tests/fixtures/codex/streaming.sse | sort -u
```

Expected: a list of event names. **Write them down.** These are the real names the `SseTranslator`
must handle in Task 7 — the spec guesses `response.output_text.delta`, `response.completed`, and
`response.output_item.added`, and any of the three may be wrong.

- [ ] **Step 4: Capture a tool call, streaming and not**

```bash
TOOLS='"tools":[{"type":"function","name":"get_weather","description":"Get weather","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}]'
curl -sS -X POST http://127.0.0.1:8099/v1/responses \
  -H 'content-type: application/json' \
  -d "{\"model\":\"gpt-5-codex\",\"input\":[{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"weather in Taipei?\"}]}],$TOOLS,\"store\":false}" \
  > tests/fixtures/codex/toolcall.json
curl -sS -N -X POST http://127.0.0.1:8099/v1/responses \
  -H 'content-type: application/json' -H 'accept: text/event-stream' \
  -d "{\"model\":\"gpt-5-codex\",\"input\":[{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"weather in Taipei?\"}]}],$TOOLS,\"stream\":true,\"store\":false}" \
  > tests/fixtures/codex/toolcall-streaming.sse
grep -o '"call_id":"[^"]*"' tests/fixtures/codex/toolcall.json | head -1
grep '^event:' tests/fixtures/codex/toolcall-streaming.sse | sort -u
```

Expected: a `call_id`, and the function-call event names. Note whether arguments arrive as one
delta or several — Task 7's accumulation test depends on it.

- [ ] **Step 5: Correct the spec's tables against what you captured**

Open `docs/superpowers/specs/2026-08-11-codex-chat-translation-design.md`. For every row in the
`responses_to_chat` table and every row in the streaming table, confirm the field or event name
appears in your fixtures. Fix any that do not. If a fixture shows a field the spec never mentions
(a `reasoning` item, an `incomplete_details` variant), add a row for it.

- [ ] **Step 6: Redact and commit**

Fixtures are real API responses. Before committing, check for anything account-identifying:

```bash
grep -o '"id":"[^"]*"' tests/fixtures/codex/*.json | head
grep -ric 'account\|user_id\|email' tests/fixtures/codex/ || true
```

Replace any account id or email with a placeholder like `acct-fixture`. Response and item ids are
fine to keep.

```bash
git add tests/fixtures/codex docs/superpowers/specs/2026-08-11-codex-chat-translation-design.md
git commit -m "test(codex): capture Responses API fixtures and correct the mapping tables"
```

---

### Task 2: Route the translation path

**Files:**
- Modify: `src/codex.rs` (add `should_translate`, beside `should_route` at `:9-13`)
- Modify: `src/lib.rs:303-306` (target path selection)
- Test: inline in both files

**Interfaces:**
- Consumes: nothing.
- Produces: `pub fn codex::should_translate(path: &str) -> bool`.

- [ ] **Step 1: Write the failing test**

In `src/codex.rs`, inside the existing `#[cfg(test)] mod tests`:

```rust
#[test]
fn should_translate_matches_codex_chat_path() {
    assert!(should_translate("/codex/v1/chat/completions"));
    assert!(should_translate("/codex/v1/chat/completions?stream=true"));
    assert!(should_translate("/codex/v1/chat/completions/"));
    // The plain OpenAI path must never translate.
    assert!(!should_translate("/v1/chat/completions"));
    // Stage 1's passthrough must never translate.
    assert!(!should_translate("/v1/responses"));
    assert!(!should_translate("/codex/v1/chat/completionsX"));
}

#[test]
fn translated_path_is_routed_to_codex() {
    assert!(should_route("/codex/v1/chat/completions"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib codex::tests::should_translate_matches_codex_chat_path`
Expected: FAIL — `cannot find function should_translate in this scope`.

- [ ] **Step 3: Write minimal implementation**

In `src/codex.rs`, directly below `should_route`:

```rust
/// True if `path` is the Chat Completions path that must be translated into
/// a Responses request before it goes upstream. Stage 2 only; stage 1's
/// `/v1/responses*` is forwarded untranslated.
pub fn should_translate(path: &str) -> bool {
    path == "/codex/v1/chat/completions"
        || path.starts_with("/codex/v1/chat/completions/")
        || path.starts_with("/codex/v1/chat/completions?")
}
```

Extend `should_route` so the new path resolves to the Codex upstream:

```rust
pub fn should_route(path: &str) -> bool {
    path == "/v1/responses"
        || path.starts_with("/v1/responses/")
        || path.starts_with("/v1/responses?")
        || should_translate(path)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib codex::`
Expected: PASS, including the pre-existing stage-1 codex tests.

- [ ] **Step 5: Write the failing target-path test**

In `src/lib.rs`'s `#[cfg(test)] mod tests`, beside `codex_target_path_strips_v1`:

```rust
#[test]
fn codex_translate_path_targets_responses() {
    // A translated request goes to /responses regardless of its inbound path.
    assert_eq!(codex_target_path("/codex/v1/chat/completions"), "/responses");
    assert_eq!(
        codex_target_path("/codex/v1/chat/completions?stream=true"),
        "/responses"
    );
    // Stage 1's behaviour is unchanged.
    assert_eq!(codex_target_path("/v1/responses"), "/responses");
    assert_eq!(
        codex_target_path("/v1/responses?stream=true"),
        "/responses?stream=true"
    );
}
```

- [ ] **Step 6: Run it to verify it fails**

Run: `cargo test --lib codex_translate_path_targets_responses`
Expected: FAIL — returns `/codex/v1/chat/completions`, expected `/responses`.

- [ ] **Step 7: Implement**

In `src/lib.rs`, replace the body of `codex_target_path`:

```rust
pub fn codex_target_path(path_and_query: &str) -> String {
    // A translated Chat Completions request always becomes a Responses call.
    // Its inbound query string describes the chat request, not the Responses
    // one, so it is deliberately dropped — `stream` travels in the body.
    let path_only = path_and_query.split('?').next().unwrap_or(path_and_query);
    if codex::should_translate(path_only) {
        return "/responses".to_string();
    }
    match path_and_query.strip_prefix("/v1") {
        Some(rest) => rest.to_string(),
        None => path_and_query.to_string(),
    }
}
```

- [ ] **Step 8: Run the full suite**

Run: `cargo test`
Expected: PASS. `tests/passthrough.rs` proves the untouched routes still behave.

- [ ] **Step 9: Commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings
git add src/codex.rs src/lib.rs
git commit -m "feat(codex): route /codex/v1/chat/completions to the Responses endpoint"
```

---

### Task 3: `chat_to_responses` — messages and simple parameters

**Files:**
- Create: `src/translate.rs`
- Modify: `src/lib.rs` (add `pub mod translate;` beside `pub mod codex;` at `:12`)
- Test: inline in `src/translate.rs`

**Interfaces:**
- Consumes: `codex::should_translate` (not directly; routing only).
- Produces: `pub fn chat_to_responses(chat: &serde_json::Value) -> Result<serde_json::Value, TranslateError>` and `pub enum TranslateError { Rejected { param: &'static str }, NotAnObject }`. Later tasks extend this same function; they do not add a second one.

- [ ] **Step 1: Write the failing test**

Create `src/translate.rs` containing only:

```rust
//! Chat Completions ↔ Responses translation.
//!
//! Pure functions: no network, no state beyond `SseTranslator`'s cursor.
//! Both API shapes are public documentation, so unlike the Codex client
//! headers this module is tracked source.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_roles_to_input_items() {
        let chat = json!({
            "model": "gpt-5-codex",
            "messages": [
                {"role": "system", "content": "be terse"},
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello"}
            ]
        });
        let out = chat_to_responses(&chat).unwrap();

        assert_eq!(out["instructions"], json!("be terse"));
        assert_eq!(out["model"], json!("gpt-5-codex"));
        assert_eq!(out["store"], json!(false));

        let input = out["input"].as_array().unwrap();
        assert_eq!(input.len(), 2, "system becomes instructions, not an input item");
        assert_eq!(input[0]["role"], json!("user"));
        assert_eq!(input[0]["content"][0]["type"], json!("input_text"));
        assert_eq!(input[0]["content"][0]["text"], json!("hi"));
        assert_eq!(input[1]["role"], json!("assistant"));
        assert_eq!(input[1]["content"][0]["type"], json!("output_text"));
    }

    #[test]
    fn joins_multiple_system_messages_with_newline() {
        let chat = json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "one"},
                {"role": "developer", "content": "two"},
                {"role": "user", "content": "go"}
            ]
        });
        let out = chat_to_responses(&chat).unwrap();
        assert_eq!(out["instructions"], json!("one\ntwo"));
    }

    #[test]
    fn maps_simple_parameters() {
        let chat = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 256,
            "temperature": 0.5,
            "top_p": 0.9
        });
        let out = chat_to_responses(&chat).unwrap();
        assert_eq!(out["max_output_tokens"], json!(256));
        assert_eq!(out["temperature"], json!(0.5));
        assert_eq!(out["top_p"], json!(0.9));
        assert!(out.get("max_tokens").is_none(), "renamed, not duplicated");
    }

    #[test]
    fn stream_is_always_forced_true() {
        // The upstream rejects anything else with
        // 400 {"detail":"Stream must be set to true"}. A non-streaming client
        // request is satisfied by aggregating the stream, not by asking the
        // upstream for a non-streaming reply.
        for asked in [json!(false), json!(true), Value::Null] {
            let mut chat = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
            if !asked.is_null() {
                chat["stream"] = asked.clone();
            }
            assert_eq!(
                chat_to_responses(&chat).unwrap()["stream"],
                json!(true),
                "stream must be forced true (client asked {asked})"
            );
        }
    }

    #[test]
    fn max_completion_tokens_also_maps() {
        let chat = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "max_completion_tokens": 99
        });
        assert_eq!(chat_to_responses(&chat).unwrap()["max_output_tokens"], json!(99));
    }

    #[test]
    fn rejects_non_object_body() {
        assert!(matches!(
            chat_to_responses(&json!([1, 2, 3])),
            Err(TranslateError::NotAnObject)
        ));
    }
}
```

Add to `src/lib.rs` beside `pub mod codex;`:

```rust
pub mod translate;
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib translate::`
Expected: FAIL to compile — `cannot find function chat_to_responses`.

- [ ] **Step 3: Write minimal implementation**

At the top of `src/translate.rs`, above the test module:

```rust
use serde_json::{Map, Value, json};

/// Why a Chat Completions body could not be translated.
#[derive(Debug, PartialEq, Eq)]
pub enum TranslateError {
    /// A parameter whose semantics the Responses API cannot honour. The
    /// caller gets a 400 naming it rather than a subtly different result.
    Rejected { param: &'static str },
    /// The body was not a JSON object.
    NotAnObject,
}

/// Translate a Chat Completions request into a Responses request.
pub fn chat_to_responses(chat: &Value) -> Result<Value, TranslateError> {
    let chat = chat.as_object().ok_or(TranslateError::NotAnObject)?;

    let mut out = Map::new();
    if let Some(model) = chat.get("model") {
        // Passed through verbatim: the gateway keeps no model allowlist.
        out.insert("model".into(), model.clone());
    }
    // Stateless by construction. Chat Completions carries its whole history
    // on every call, so server-side threading would double-count context.
    out.insert("store".into(), json!(false));
    // The upstream has no non-streaming mode; it rejects anything else with
    // 400 {"detail":"Stream must be set to true"}. A client that asked for a
    // non-streaming reply gets the stream aggregated for it downstream.
    out.insert("stream".into(), json!(true));

    let mut instructions: Vec<String> = Vec::new();
    let mut input: Vec<Value> = Vec::new();

    for msg in chat.get("messages").and_then(Value::as_array).unwrap_or(&vec![]) {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
        let text = msg.get("content").and_then(Value::as_str).unwrap_or("");
        match role {
            "system" | "developer" => instructions.push(text.to_string()),
            "assistant" => input.push(json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text}],
            })),
            _ => input.push(json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": text}],
            })),
        }
    }

    if !instructions.is_empty() {
        out.insert("instructions".into(), json!(instructions.join("\n")));
    }
    out.insert("input".into(), Value::Array(input));

    if let Some(v) = chat.get("max_tokens").or_else(|| chat.get("max_completion_tokens")) {
        out.insert("max_output_tokens".into(), v.clone());
    }
    for name in ["temperature", "top_p", "tool_choice"] {
        if let Some(v) = chat.get(name) {
            out.insert(name.into(), v.clone());
        }
    }

    Ok(Value::Object(out))
}
```

Note: `unwrap_or(&vec![])` will not compile — a temporary cannot be borrowed. Use:

```rust
    let empty = Vec::new();
    for msg in chat.get("messages").and_then(Value::as_array).unwrap_or(&empty) {
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib translate::`
Expected: PASS, six tests.

- [ ] **Step 5: Commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings
git add src/translate.rs src/lib.rs
git commit -m "feat(translate): map chat messages and simple params to Responses"
```

---

### Task 4: `chat_to_responses` — tools and tool-call history

**Files:**
- Modify: `src/translate.rs`
- Test: inline

**Interfaces:**
- Consumes: `chat_to_responses` from Task 3.
- Produces: no new signatures; the same function now handles `tools` and tool-role messages.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn flattens_tool_definitions() {
        let chat = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get weather",
                    "parameters": {"type": "object", "properties": {}}
                }
            }]
        });
        let out = chat_to_responses(&chat).unwrap();
        let tool = &out["tools"][0];
        // Responses puts name/description/parameters at the top level;
        // Chat Completions nests them under "function".
        assert_eq!(tool["type"], json!("function"));
        assert_eq!(tool["name"], json!("get_weather"));
        assert_eq!(tool["description"], json!("Get weather"));
        assert_eq!(tool["parameters"]["type"], json!("object"));
        assert!(tool.get("function").is_none(), "the nesting is removed");
    }

    #[test]
    fn maps_tool_call_round_trip() {
        let chat = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_abc",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Taipei\"}"}
                }]},
                {"role": "tool", "tool_call_id": "call_abc", "content": "22C"}
            ]
        });
        let input = chat_to_responses(&chat).unwrap();
        let input = input["input"].as_array().unwrap();

        assert_eq!(input.len(), 3);
        assert_eq!(input[1]["type"], json!("function_call"));
        assert_eq!(input[1]["call_id"], json!("call_abc"));
        assert_eq!(input[1]["name"], json!("get_weather"));
        assert_eq!(input[1]["arguments"], json!("{\"city\":\"Taipei\"}"));
        assert_eq!(input[2]["type"], json!("function_call_output"));
        assert_eq!(input[2]["call_id"], json!("call_abc"));
        assert_eq!(input[2]["output"], json!("22C"));
    }

    #[test]
    fn assistant_with_content_and_tool_calls_becomes_two_items() {
        // Spec decision: message first, then the call, in the order the
        // model produced them.
        let chat = json!({
            "model": "m",
            "messages": [{"role": "assistant", "content": "checking", "tool_calls": [{
                "id": "call_1",
                "function": {"name": "f", "arguments": "{}"}
            }]}]
        });
        let out = chat_to_responses(&chat).unwrap();
        let input = out["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], json!("message"));
        assert_eq!(input[0]["content"][0]["text"], json!("checking"));
        assert_eq!(input[1]["type"], json!("function_call"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib translate::`
Expected: FAIL — `tools` absent from output; tool-role messages become user messages.

- [ ] **Step 3: Implement**

Replace the `for msg in …` loop body in `chat_to_responses`:

```rust
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
        let text = msg.get("content").and_then(Value::as_str).unwrap_or("");
        match role {
            "system" | "developer" => instructions.push(text.to_string()),
            "tool" => input.push(json!({
                "type": "function_call_output",
                "call_id": msg.get("tool_call_id").cloned().unwrap_or(Value::Null),
                "output": text,
            })),
            "assistant" => {
                // Content and tool_calls can both be present. Emit the
                // message first, then each call, matching production order.
                if !text.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text}],
                    }));
                }
                for call in msg.get("tool_calls").and_then(Value::as_array).unwrap_or(&empty) {
                    let f = call.get("function").unwrap_or(&Value::Null);
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.get("id").cloned().unwrap_or(Value::Null),
                        "name": f.get("name").cloned().unwrap_or(Value::Null),
                        "arguments": f.get("arguments").cloned().unwrap_or(json!("")),
                    }));
                }
            }
            _ => input.push(json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": text}],
            })),
        }
```

Add tool flattening after the parameter loop:

```rust
    if let Some(tools) = chat.get("tools").and_then(Value::as_array) {
        let flattened: Vec<Value> = tools
            .iter()
            .map(|t| {
                let f = t.get("function").unwrap_or(t);
                json!({
                    "type": "function",
                    "name": f.get("name").cloned().unwrap_or(Value::Null),
                    "description": f.get("description").cloned().unwrap_or(Value::Null),
                    "parameters": f.get("parameters").cloned().unwrap_or(json!({})),
                })
            })
            .collect();
        out.insert("tools".into(), Value::Array(flattened));
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib translate::`
Expected: PASS, eight tests.

- [ ] **Step 5: Commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings
git add src/translate.rs
git commit -m "feat(translate): map tool definitions and tool-call history"
```

---

### Task 5: `chat_to_responses` — parameter policy

**Files:**
- Modify: `src/translate.rs`
- Test: inline

**Interfaces:**
- Consumes: `chat_to_responses`, `TranslateError::Rejected` from Task 3.
- Produces: no new signatures.

- [ ] **Step 1: Write the failing test**

```rust
    fn chat_with(extra: (&str, Value)) -> Value {
        let mut v = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}]
        });
        v[extra.0] = extra.1;
        v
    }

    #[test]
    fn rejects_semantic_parameters() {
        // Each of these changes what the caller may assume about the result,
        // so failing loudly beats returning something subtly different.
        for param in ["seed", "logprobs", "top_logprobs", "response_format"] {
            let chat = chat_with((param, json!(1)));
            assert_eq!(
                chat_to_responses(&chat),
                Err(TranslateError::Rejected { param: match param {
                    "seed" => "seed",
                    "logprobs" => "logprobs",
                    "top_logprobs" => "top_logprobs",
                    _ => "response_format",
                }}),
                "{param} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_n_above_one_but_allows_one() {
        assert_eq!(
            chat_to_responses(&chat_with(("n", json!(2)))),
            Err(TranslateError::Rejected { param: "n" })
        );
        assert!(chat_to_responses(&chat_with(("n", json!(1)))).is_ok());
    }

    #[test]
    fn drops_cosmetic_parameters_without_failing() {
        // These tune output rather than define it; rejecting them would
        // break clients that send library defaults.
        let mut chat = chat_with(("user", json!("u1")));
        chat["presence_penalty"] = json!(0.0);
        chat["frequency_penalty"] = json!(0.0);
        chat["logit_bias"] = json!({});
        chat["stop"] = json!(["\n"]);

        let out = chat_to_responses(&chat).unwrap();
        for dropped in ["user", "presence_penalty", "frequency_penalty", "logit_bias", "stop"] {
            assert!(out.get(dropped).is_none(), "{dropped} must not reach upstream");
        }
        assert_eq!(out["input"][0]["content"][0]["text"], json!("hi"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib translate::`
Expected: FAIL — rejected parameters currently return `Ok`.

- [ ] **Step 3: Implement**

Insert immediately after the `as_object()` line in `chat_to_responses`:

```rust
    // Semantic parameters: the Responses API cannot honour these, and
    // silently ignoring them would hand back a result the caller believes
    // is seeded / scored / schema-constrained when it is not.
    for param in ["seed", "logprobs", "top_logprobs", "response_format"] {
        if chat.contains_key(param) && !chat[param].is_null() {
            return Err(TranslateError::Rejected { param: match param {
                "seed" => "seed",
                "logprobs" => "logprobs",
                "top_logprobs" => "top_logprobs",
                _ => "response_format",
            }});
        }
    }
    if chat.get("n").and_then(Value::as_u64).is_some_and(|n| n > 1) {
        return Err(TranslateError::Rejected { param: "n" });
    }

    // Cosmetic parameters are dropped, not rejected: clients send library
    // defaults for these and would break on a 400.
    for param in ["presence_penalty", "frequency_penalty", "logit_bias", "user", "stop"] {
        if chat.contains_key(param) {
            tracing::warn!(param, "parameter has no Responses equivalent, dropped");
        }
    }
```

Cosmetic parameters need no removal step — the translator builds its output from an allowlist, so
anything not explicitly copied is already absent.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib translate::`
Expected: PASS, eleven tests.

- [ ] **Step 5: Commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings
git add src/translate.rs
git commit -m "feat(translate): reject semantic params, drop cosmetic ones"
```

---

### Task 6: `responses_to_chat`, SSE frame splitting, and aggregation

The upstream is streaming-only (Global Constraints), so `responses_to_chat`'s input never arrives as
a JSON body — it is always rebuilt from the SSE stream. This task therefore builds three things:
the frame splitter both later tasks share, the aggregator that rebuilds a Responses object, and
`responses_to_chat` itself.

**Files:**
- Modify: `src/translate.rs`
- Test: inline, reading `tests/fixtures/codex/*` (already committed — do not re-capture)

**Interfaces:**
- Consumes: the committed fixtures.
- Produces:
  - `pub fn split_sse_frames(buf: &mut String) -> Vec<(String, Value)>` — drains complete frames from
    `buf`, leaving any trailing partial frame in place. Returns `(event, data)` pairs, skipping
    frames whose data will not parse as JSON.
  - `pub struct ResponseAggregator` with `new() -> Self`, `push(&mut self, event: &str, data: &Value)`,
    and `finish(self) -> Value` returning a Responses object.
  - `pub fn responses_to_chat(resp: &Value, model: &str) -> Value`.

- [ ] **Step 1: Write the failing test**

```rust
    fn fixture(name: &str) -> Value {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/codex/");
        let raw = std::fs::read_to_string(format!("{path}{name}"))
            .unwrap_or_else(|e| panic!("fixture {name}: {e}"));
        serde_json::from_str(&raw).unwrap()
    }

    fn fixture_raw(name: &str) -> String {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/codex/");
        std::fs::read_to_string(format!("{path}{name}"))
            .unwrap_or_else(|e| panic!("fixture {name}: {e}"))
    }

    #[test]
    fn splits_sse_frames_and_keeps_partials() {
        let mut buf = String::from("event: a\ndata: {\"x\":1}\n\nevent: b\ndata: {\"y\"");
        let frames = split_sse_frames(&mut buf);
        assert_eq!(frames.len(), 1, "only the complete frame is drained");
        assert_eq!(frames[0].0, "a");
        assert_eq!(frames[0].1["x"], json!(1));
        assert_eq!(buf, "event: b\ndata: {\"y\"", "the partial frame stays buffered");

        // Completing the partial yields it on the next call.
        buf.push_str(":2}\n\n");
        let frames = split_sse_frames(&mut buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, "b");
        assert_eq!(frames[0].1["y"], json!(2));
        assert!(buf.is_empty());
    }

    #[test]
    fn aggregates_the_captured_text_stream() {
        // response.completed carries usage and status but an EMPTY output
        // array; the items arrive on response.output_item.done. Rebuilding
        // from response.completed alone would silently produce empty replies.
        let mut buf = fixture_raw("streaming.sse");
        let mut agg = ResponseAggregator::new();
        for (event, data) in split_sse_frames(&mut buf) {
            agg.push(&event, &data);
        }
        let rebuilt = agg.finish();
        assert_eq!(rebuilt, fixture("nonstreaming.json"));
    }

    #[test]
    fn aggregates_the_captured_tool_call_stream() {
        let mut buf = fixture_raw("toolcall-streaming.sse");
        let mut agg = ResponseAggregator::new();
        for (event, data) in split_sse_frames(&mut buf) {
            agg.push(&event, &data);
        }
        assert_eq!(agg.finish(), fixture("toolcall.json"));
    }

    #[test]
    fn converts_real_nonstreaming_fixture() {
        let out = responses_to_chat(&fixture("nonstreaming.json"), "gpt-5.4");
        assert_eq!(out["object"], json!("chat.completion"));
        assert_eq!(out["model"], json!("gpt-5.4"));
        let choice = &out["choices"][0];
        assert_eq!(choice["index"], json!(0));
        assert_eq!(choice["message"]["role"], json!("assistant"));
        assert_eq!(choice["message"]["content"], json!("1\n2\n3\n4\n5"));
        assert_eq!(choice["finish_reason"], json!("stop"));
        assert_eq!(out["usage"]["prompt_tokens"], json!(9));
        assert_eq!(out["usage"]["completion_tokens"], json!(13));
        assert_eq!(out["usage"]["total_tokens"], json!(22));
    }

    #[test]
    fn converts_real_toolcall_fixture() {
        let out = responses_to_chat(&fixture("toolcall.json"), "gpt-5.4");
        let choice = &out["choices"][0];
        assert_eq!(choice["finish_reason"], json!("tool_calls"));
        let call = &choice["message"]["tool_calls"][0];
        assert_eq!(call["type"], json!("function"));
        // The OpenAI tool-call id is call_id, NOT the item's own id.
        assert_eq!(call["id"], json!("call_uphHcxpvlpMMt0m2ZeigzBfH"));
        assert_eq!(call["function"]["name"], json!("get_weather"));
        assert_eq!(call["function"]["arguments"], json!("{\"city\":\"Taipei\"}"));
    }

    #[test]
    fn drops_reasoning_items() {
        let resp = json!({
            "output": [
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "thinking"}]},
                {"type": "message", "content": [{"type": "output_text", "text": "answer"}]}
            ],
            "usage": {"input_tokens": 1, "output_tokens": 2, "total_tokens": 3}
        });
        let out = responses_to_chat(&resp, "m");
        assert_eq!(out["choices"][0]["message"]["content"], json!("answer"));
        assert!(!out.to_string().contains("thinking"), "reasoning must not leak");
    }

    #[test]
    fn truncation_wins_over_tool_calls() {
        // Spec decision: a call truncated mid-arguments is not actionable,
        // so "length" beats "tool_calls".
        let resp = json!({
            "output": [{"type": "function_call", "call_id": "c", "name": "f", "arguments": "{\"a"}],
            "incomplete_details": {"reason": "max_output_tokens"},
            "usage": {"input_tokens": 1, "output_tokens": 2, "total_tokens": 3}
        });
        assert_eq!(
            responses_to_chat(&resp, "m")["choices"][0]["finish_reason"],
            json!("length")
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib translate::`
Expected: FAIL — `cannot find function split_sse_frames` / `responses_to_chat`.

- [ ] **Step 3: Implement the frame splitter and the aggregator**

```rust
/// Drain complete SSE frames from `buf`, leaving any trailing partial frame in
/// place for the next chunk. Returns `(event name, data)` pairs; the `[DONE]`
/// sentinel and any frame whose data is not JSON are skipped.
pub fn split_sse_frames(buf: &mut String) -> Vec<(String, Value)> {
    // The upstream separates frames with "\n\n", but CR is legal in SSE and
    // costs one line to tolerate.
    buf.retain(|c| c != '\r');

    let mut frames = Vec::new();
    while let Some(end) = buf.find("\n\n") {
        let frame: String = buf.drain(..end + 2).collect();
        let mut event = String::new();
        let mut data = String::new();
        for line in frame.lines() {
            if let Some(v) = line.strip_prefix("event:") {
                event = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("data:") {
                data.push_str(v.trim());
            }
        }
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(&data) {
            if event.is_empty() {
                event = value
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
            }
            frames.push((event, value));
        }
    }
    frames
}

/// Rebuilds a Responses object from the upstream's SSE stream. The upstream is
/// streaming-only, so this is the only way to serve a non-streaming request.
#[derive(Default)]
pub struct ResponseAggregator {
    response: Value,
    output: Vec<Value>,
}

impl ResponseAggregator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, _event: &str, data: &Value) {
        // response.created / .in_progress / .completed each carry the whole
        // response object; last one wins, so usage and status end up being the
        // ones from response.completed.
        if let Some(resp) = data.get("response") {
            self.response = resp.clone();
        }
        // response.completed's own output array is EMPTY on this backend — the
        // items only ever arrive on response.output_item.done, already in the
        // exact shape responses_to_chat expects.
        if data.get("type").and_then(Value::as_str) == Some("response.output_item.done")
            && let Some(item) = data.get("item")
        {
            self.output.push(item.clone());
        }
    }

    /// Only the fields `responses_to_chat` reads are kept.
    pub fn finish(self) -> Value {
        let mut out = serde_json::Map::new();
        for key in ["id", "created_at", "status", "model", "usage", "incomplete_details"] {
            out.insert(
                key.into(),
                self.response.get(key).cloned().unwrap_or(Value::Null),
            );
        }
        out.insert("object".into(), json!("response"));
        out.insert("output".into(), Value::Array(self.output));
        Value::Object(out)
    }
}
```

- [ ] **Step 4: Implement `responses_to_chat`**

```rust
/// Translate a Responses reply into a Chat Completions reply. `model` is the
/// string the client sent, echoed back verbatim.
pub fn responses_to_chat(resp: &Value, model: &str) -> Value {
    let empty = Vec::new();
    let output = resp.get("output").and_then(Value::as_array).unwrap_or(&empty);

    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for part in item.get("content").and_then(Value::as_array).unwrap_or(&empty) {
                    if part.get("type").and_then(Value::as_str) == Some("output_text")
                        && let Some(t) = part.get("text").and_then(Value::as_str)
                    {
                        text.push_str(t);
                    }
                }
            }
            Some("function_call") => tool_calls.push(json!({
                "id": item.get("call_id").cloned().unwrap_or(Value::Null),
                "type": "function",
                "function": {
                    "name": item.get("name").cloned().unwrap_or(Value::Null),
                    "arguments": item.get("arguments").cloned().unwrap_or(json!("")),
                },
            })),
            // "reasoning" and anything else the API adds later: dropped.
            // The Chat Completions shape has nowhere to put them.
            _ => {}
        }
    }

    // Truncation is checked before tool calls: a call cut off mid-arguments
    // is not one the caller can act on.
    let truncated = resp
        .get("incomplete_details")
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
        == Some("max_output_tokens");
    let finish_reason = if truncated {
        "length"
    } else if !tool_calls.is_empty() {
        "tool_calls"
    } else {
        "stop"
    };

    let mut message = json!({"role": "assistant", "content": text});
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }

    let usage = resp.get("usage").cloned().unwrap_or(Value::Null);
    json!({
        "id": resp.get("id").cloned().unwrap_or(json!("chatcmpl-codex")),
        "object": "chat.completion",
        "created": resp.get("created_at").cloned().unwrap_or(json!(0)),
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
        "usage": {
            "prompt_tokens": usage.get("input_tokens").cloned().unwrap_or(json!(0)),
            "completion_tokens": usage.get("output_tokens").cloned().unwrap_or(json!(0)),
            "total_tokens": usage.get("total_tokens").cloned().unwrap_or(json!(0)),
        },
    })
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib translate::`
Expected: PASS, including the seven tests added here. If a fixture test fails on a field name, the
fixture is right and the code is wrong — fix the code, and correct the spec table too.

- [ ] **Step 6: Commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings
git add src/translate.rs
git commit -m "feat(translate): aggregate the Codex SSE stream into chat.completion"
```

---

### Task 7: `SseTranslator`

**Files:**
- Modify: `src/translate.rs`
- Test: inline, reading `tests/fixtures/codex/streaming.sse` and `toolcall-streaming.sse`

**Interfaces:**
- Consumes: `split_sse_frames(&mut String) -> Vec<(String, Value)>` from Task 6, and the committed
  fixtures. The event names below are the ones the captures actually contain — do not invent others.
- Produces: `pub struct SseTranslator`, `SseTranslator::new(model: String) -> Self`, `push(&mut self, event: &str, data: &Value) -> Vec<Value>`, `finish(&mut self) -> Vec<Value>`.

- [ ] **Step 1: Write the failing test**

```rust
    /// Split a captured SSE file into (event, data) pairs, using the same
    /// splitter the proxy uses at runtime (Task 6).
    fn sse_events(name: &str) -> Vec<(String, Value)> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/codex/");
        let mut raw = std::fs::read_to_string(format!("{path}{name}"))
            .unwrap_or_else(|e| panic!("fixture {name}: {e}"));
        split_sse_frames(&mut raw)
    }

    #[test]
    fn streams_text_as_content_chunks() {
        let mut t = SseTranslator::new("gpt-5.4".into());
        let mut chunks = Vec::new();
        for (event, data) in sse_events("streaming.sse") {
            chunks.extend(t.push(&event, &data));
        }
        chunks.extend(t.finish());

        assert!(!chunks.is_empty(), "fixture produced no chunks");
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], json!("assistant"));
        assert_eq!(chunks[0]["object"], json!("chat.completion.chunk"));

        let text: String = chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert_eq!(text, "1\n2\n3\n4\n5", "deltas must reassemble to the captured reply");

        let last = chunks.last().unwrap();
        assert_eq!(last["choices"][0]["finish_reason"], json!("stop"));
    }

    #[test]
    fn accumulates_split_tool_call_arguments() {
        let mut t = SseTranslator::new("m".into());
        let mut chunks = Vec::new();
        for (event, data) in sse_events("toolcall-streaming.sse") {
            chunks.extend(t.push(&event, &data));
        }
        chunks.extend(t.finish());

        // The call is announced once, with an id and a name. The id is the
        // item's call_id, not its own id.
        let announced: Vec<_> = chunks
            .iter()
            .filter(|c| c["choices"][0]["delta"]["tool_calls"][0]["id"].is_string())
            .collect();
        assert_eq!(announced.len(), 1, "the call is announced exactly once");
        let call = &announced[0]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(call["id"], json!("call_uphHcxpvlpMMt0m2ZeigzBfH"));
        assert_eq!(call["function"]["name"], json!("get_weather"));

        // The capture splits the arguments across six deltas; they must
        // reassemble byte for byte.
        let args: String = chunks
            .iter()
            .filter_map(|c| {
                c["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str()
            })
            .collect();
        assert_eq!(args, "{\"city\":\"Taipei\"}");

        assert_eq!(
            chunks.last().unwrap()["choices"][0]["finish_reason"],
            json!("tool_calls")
        );
    }

    #[test]
    fn role_chunk_is_emitted_only_once() {
        let mut t = SseTranslator::new("m".into());
        let mut chunks = Vec::new();
        for _ in 0..3 {
            chunks.extend(t.push("response.output_text.delta", &json!({"delta": "x"})));
        }
        let roles = chunks
            .iter()
            .filter(|c| c["choices"][0]["delta"]["role"].is_string())
            .count();
        assert_eq!(roles, 1);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib translate::`
Expected: FAIL — `cannot find type SseTranslator`.

- [ ] **Step 3: Implement**

```rust
/// Converts a Responses SSE stream into Chat Completions chunks.
///
/// Its whole state is three values: whether the opening role chunk has gone
/// out, the next tool-call index, and which index each Responses item id was
/// assigned.
pub struct SseTranslator {
    model: String,
    role_sent: bool,
    next_index: u64,
    item_index: std::collections::HashMap<String, u64>,
    saw_tool_call: bool,
    finished: bool,
}

impl SseTranslator {
    pub fn new(model: String) -> Self {
        Self {
            model,
            role_sent: false,
            next_index: 0,
            item_index: std::collections::HashMap::new(),
            saw_tool_call: false,
            finished: false,
        }
    }

    fn chunk(&self, delta: Value, finish_reason: Value) -> Value {
        json!({
            "id": "chatcmpl-codex",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": self.model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
        })
    }

    /// Feed one SSE event. Returns zero or more chunks to forward.
    pub fn push(&mut self, event: &str, data: &Value) -> Vec<Value> {
        let mut out = Vec::new();
        if !self.role_sent {
            self.role_sent = true;
            out.push(self.chunk(json!({"role": "assistant"}), Value::Null));
        }

        match event {
            "response.output_text.delta" => {
                if let Some(d) = data.get("delta").and_then(Value::as_str) {
                    out.push(self.chunk(json!({"content": d}), Value::Null));
                }
            }
            "response.output_item.added" => {
                let item = data.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    self.saw_tool_call = true;
                    let index = self.next_index;
                    self.next_index += 1;
                    if let Some(id) = item.get("id").and_then(Value::as_str) {
                        self.item_index.insert(id.to_string(), index);
                    }
                    out.push(self.chunk(
                        json!({"tool_calls": [{
                            "index": index,
                            "id": item.get("call_id").cloned().unwrap_or(Value::Null),
                            "type": "function",
                            "function": {
                                "name": item.get("name").cloned().unwrap_or(Value::Null),
                                "arguments": "",
                            },
                        }]}),
                        Value::Null,
                    ));
                }
            }
            "response.function_call_arguments.delta" => {
                let index = data
                    .get("item_id")
                    .and_then(Value::as_str)
                    .and_then(|id| self.item_index.get(id).copied())
                    .unwrap_or(0);
                if let Some(d) = data.get("delta").and_then(Value::as_str) {
                    out.push(self.chunk(
                        json!({"tool_calls": [{
                            "index": index,
                            "function": {"arguments": d},
                        }]}),
                        Value::Null,
                    ));
                }
            }
            "response.completed" => out.extend(self.finish()),
            "response.failed" | "error" => {
                self.finished = true;
                out.push(json!({
                    "error": {
                        "message": data
                            .pointer("/response/error/message")
                            .or_else(|| data.get("message"))
                            .and_then(Value::as_str)
                            .unwrap_or("codex stream failed"),
                        "type": "upstream_error",
                    }
                }));
            }
            _ => {}
        }
        out
    }

    /// Emit the terminal chunk. Idempotent — safe to call after
    /// `response.completed` already triggered it.
    pub fn finish(&mut self) -> Vec<Value> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let reason = if self.saw_tool_call { "tool_calls" } else { "stop" };
        vec![self.chunk(json!({}), json!(reason))]
    }
}
```

The `match` arms above are the event names the captures actually contain. Events the captures also
carry but this translator ignores — `response.created`, `response.in_progress`,
`response.content_part.added`/`.done`, `response.output_text.done`,
`response.function_call_arguments.done`, `response.output_item.done` — are deliberate: their content
already went out as deltas, and re-emitting it would duplicate the reply.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib translate::`
Expected: PASS, including the three tests added here.

- [ ] **Step 5: Commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings
git add src/translate.rs
git commit -m "feat(translate): SSE state machine for Responses streams"
```

---

### Task 8: Wire the non-streaming path into `forward()`

**Files:**
- Modify: `src/lib.rs` (request translate between the compression block `:323-337` and the
  `final_body` binding `:415`; reply aggregation before `bytes_stream()` at `:532`)
- Modify: `src/compress.rs` (`should_compress` for Codex)
- Test: `tests/codex_translate.rs` (create)

**Interfaces:**
- Consumes: `translate::chat_to_responses`, `translate::responses_to_chat`,
  `translate::split_sse_frames`, `translate::ResponseAggregator`, `translate::TranslateError`,
  `codex::should_translate`.
- Produces: `client_wants_stream` — a `bool` binding in `forward()` that Task 9 branches on, and
  `openai_error_response(StatusCode, &str) -> Response<Body>`.

**Critical detail:** the 401 retry at `src/lib.rs:501` re-sends `body_bytes`, while the first send
uses `final_body` (`:453`). Translation must therefore assign into **`body_bytes`**, before the
`final_body` binding at `:408`. Writing it into `final_body` instead would make a refreshed retry
re-send the untranslated Chat Completions body, which the Responses endpoint would reject — and
only on the token-expiry path, so ordinary testing would never catch it.

- [ ] **Step 1: Write the failing test**

Create `tests/codex_translate.rs`, modelled on `tests/passthrough.rs`:

```rust
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
    let Some(body) = req.body.as_ref() else { return false };
    let Ok(v) = serde_json::from_slice::<Value>(body) else { return false };
    // `stream` is always true: the upstream has no non-streaming mode.
    v.get("input").is_some()
        && v.get("messages").is_none()
        && v["store"] == json!(false)
        && v["stream"] == json!(true)
}

/// True only if the body is still in Chat Completions shape.
fn is_untranslated_chat_body(req: &HttpMockRequest) -> bool {
    let Some(body) = req.body.as_ref() else { return false };
    let Ok(v) = serde_json::from_slice::<Value>(body) else { return false };
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test codex_translate`
Expected: FAIL — the upstream receives `messages`, not `input`.

- [ ] **Step 3: Enable compression on the translated path only**

In `src/compress.rs`, replace the `Provider::Codex => false` arm at `:443`:

```rust
        // The translated path's inbound body is a Chat Completions body, so
        // the existing OpenAI rewriter understands it as-is — provided
        // compression runs *before* translation. The raw /v1/responses
        // passthrough stays uncompressed, as stage 1 decided.
        Provider::Codex => codex::should_translate(path),
```

(`compress.rs` imports `codex::should_translate` — add `use crate::codex;` at the top.)

Route it to the OpenAI rewriter (the `Provider::Codex => None` arm at `:646`, in
`rewrite_request_body`) — the inbound body is a Chat Completions body, which the OpenAI rewriter
already understands:

```rust
        Provider::Codex => rewrite_tool_results_openai(&engine, min_tokens, body),
```

- [ ] **Step 4: Translate the request body**

In `src/lib.rs`, immediately after the compression block (`:337`), before the `final_body` binding
at `:415`:

```rust
    // Stage 2: the Codex chat path arrives as Chat Completions and must
    // leave as a Responses request. Assigning into `body_bytes` (not
    // `final_body`) is load-bearing: the 401 retry below re-sends
    // `body_bytes`, and an untranslated retry would be rejected upstream.
    let translating = codex::should_translate(path_only);
    let body_bytes = if translating {
        let chat: serde_json::Value = serde_json::from_slice(&body_bytes)
            .map_err(|_| anyhow::anyhow!("codex chat body is not JSON"))?;
        match translate::chat_to_responses(&chat) {
            Ok(v) => axum::body::Bytes::from(serde_json::to_vec(&v)?),
            Err(translate::TranslateError::Rejected { param }) => {
                return Ok(openai_error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("`{param}` is not supported on the Codex route"),
                ));
            }
            Err(translate::TranslateError::NotAnObject) => {
                return Ok(openai_error_response(
                    StatusCode::BAD_REQUEST,
                    "request body must be a JSON object",
                ));
            }
        }
    } else {
        body_bytes
    };
```

Add the error helper near `is_hop_by_hop`:

```rust
/// An OpenAI-shaped error body. The Codex route's clients are OpenAI
/// clients, so they parse this shape and nothing else.
fn openai_error_response(status: StatusCode, message: &str) -> Response<Body> {
    let body = serde_json::json!({
        "error": {"message": message, "type": "invalid_request_error"}
    });
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("static error response builds")
}
```

- [ ] **Step 5: Aggregate the SSE reply into a `chat.completion`**

The upstream answers every request with `text/event-stream` (Global Constraints). What the *client*
asked for decides what it gets — not the upstream's content type. This task handles
`client_wants_stream == false`; Task 9 handles the other branch.

After the header-copy loop (`:515-521`), **before** `upstream_resp.bytes_stream()` at `:532` — that
call consumes the body, so the aggregation must read it first:

```rust
    if translating && !client_wants_stream {
        let raw = upstream_resp.bytes().await.context("read upstream body")?;
        if !status.is_success() {
            // Pass the upstream's status through, but in a shape an OpenAI
            // client can parse.
            let msg = String::from_utf8_lossy(&raw).to_string();
            return Ok(openai_error_response(status, &msg));
        }
        let mut buf = String::from_utf8_lossy(&raw).into_owned();
        let mut agg = translate::ResponseAggregator::new();
        for (event, data) in translate::split_sse_frames(&mut buf) {
            agg.push(&event, &data);
        }
        let model = client_model.as_deref().unwrap_or("");
        let chat = translate::responses_to_chat(&agg.finish(), model);
        let out = serde_json::to_vec(&chat)?;
        // The upstream said text/event-stream; the client is getting JSON.
        response_headers.remove("content-length");
        response_headers.remove("content-type");
        let mut builder = Response::builder()
            .status(status)
            .header("content-type", "application/json");
        for (name, value) in response_headers.iter() {
            builder = builder.header(name, value);
        }
        return Ok(builder.body(Body::from(out)).context("build translated response")?);
    }
```

A stream that ends without `response.completed` needs no special case: the aggregator simply has no
`usage` or `status`, and `responses_to_chat` defaults the token counts to zero. Whatever items did
arrive are still returned.

`client_model` and `client_wants_stream` are captured before translation replaces the body — add
beside the `translating` binding:

```rust
    let (client_model, client_wants_stream) = if translating {
        let chat = serde_json::from_slice::<serde_json::Value>(&body_bytes).unwrap_or_default();
        (
            chat.get("model")
                .and_then(|m| m.as_str().map(str::to_string)),
            chat.get("stream").and_then(serde_json::Value::as_bool) == Some(true),
        )
    } else {
        (None, false)
    };
```

Place this **before** the translation block, so it reads the original chat body.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS, including `tests/passthrough.rs` and `tests/compress_e2e.rs` unchanged.

- [ ] **Step 7: Commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings
git add src/lib.rs src/compress.rs tests/codex_translate.rs
git commit -m "feat(codex): translate non-streaming chat requests and replies"
```

---

### Task 9: Wire the streaming path

**Files:**
- Modify: `src/lib.rs` (the streaming branch added in Task 8)
- Test: `tests/codex_translate.rs`

**Interfaces:**
- Consumes: `translate::SseTranslator` from Task 7.
- Produces: no new public signatures.

- [ ] **Step 1: Write the failing test**

Add to `tests/codex_translate.rs`:

```rust
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
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str().map(str::to_string))
        .collect();
    assert!(!text.is_empty(), "content must survive translation");

    assert_eq!(frames.last().unwrap(), "[DONE]");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test codex_translate translates_a_streaming_response`
Expected: FAIL — the client receives raw Responses events, not chunks.

- [ ] **Step 3: Implement**

Replace the `if translating && !client_wants_stream` guard from Task 8 with a two-armed branch,
adding the streaming arm ahead of it. The client's `stream` flag decides, because the upstream
always streams (Global Constraints) — the branch cannot key off the response's content type.

`async-stream` and `tokio::sync::mpsc` are both unavailable per the Global Constraints, so build the
body with `futures_util::stream::unfold`. Its state is a four-tuple; each step drains one upstream
chunk and returns every translated frame that chunk produced. Frames are split with the same
`split_sse_frames` shape as Task 6 — inline here because this arm consumes the byte stream chunk by
chunk, and each chunk may carry a partial frame.

```rust
    if translating && client_wants_stream {
        use futures_util::StreamExt;

        let model = client_model.clone().unwrap_or_default();
        let state = (
            upstream_resp.bytes_stream(),
            String::new(),                                  // partial-frame buffer
            translate::SseTranslator::new(model),
            false,                                          // upstream exhausted?
        );

        let stream = futures_util::stream::unfold(
            state,
            |(mut src, mut buf, mut translator, drained)| async move {
                if drained {
                    return None;
                }
                loop {
                    match src.next().await {
                        Some(Ok(chunk)) => {
                            buf.push_str(&String::from_utf8_lossy(&chunk));
                            let mut out = Vec::new();
                            // SSE frames are separated by a blank line.
                            while let Some(split) = buf.find("\n\n") {
                                let frame: String = buf.drain(..split + 2).collect();
                                let (mut event, mut data) = ("", "");
                                for line in frame.lines() {
                                    if let Some(e) = line.strip_prefix("event:") {
                                        event = e.trim();
                                    }
                                    if let Some(d) = line.strip_prefix("data:") {
                                        data = d.trim();
                                    }
                                }
                                let Ok(parsed) = serde_json::from_str::<serde_json::Value>(data)
                                else {
                                    continue;
                                };
                                for c in translator.push(event, &parsed) {
                                    out.push(format!("data: {c}\n\n"));
                                }
                            }
                            // A chunk can arrive mid-frame and yield nothing;
                            // keep pulling rather than emitting an empty item.
                            if !out.is_empty() {
                                let bytes = axum::body::Bytes::from(out.concat());
                                return Some((
                                    Ok::<_, std::io::Error>(bytes),
                                    (src, buf, translator, false),
                                ));
                            }
                        }
                        // End of stream, or a transport error: close the
                        // conversation cleanly. Headers are already sent, so
                        // the status can no longer change.
                        _ => {
                            let mut tail: Vec<String> = translator
                                .finish()
                                .iter()
                                .map(|c| format!("data: {c}\n\n"))
                                .collect();
                            tail.push("data: [DONE]\n\n".to_string());
                            let bytes = axum::body::Bytes::from(tail.concat());
                            return Some((Ok(bytes), (src, buf, translator, true)));
                        }
                    }
                }
            },
        );

        response_headers.remove("content-length");
        let mut builder = Response::builder().status(status);
        for (name, value) in response_headers.iter() {
            builder = builder.header(name, value);
        }
        return Ok(builder
            .body(Body::from_stream(stream))
            .context("build translated stream")?);
    }
```

`translator.finish()` is idempotent (Task 7), so a stream that already saw `response.completed`
emits only `[DONE]` here rather than a second terminal chunk.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Add the tool-call streaming test**

```rust
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
```

- [ ] **Step 6: Run it**

Run: `cargo test --test codex_translate streams_tool_calls`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings
git add src/lib.rs tests/codex_translate.rs
git commit -m "feat(codex): translate streaming chat responses"
```

---

### Task 10: Compression interaction and documentation

**Files:**
- Modify: `tests/codex_translate.rs`
- Modify: `README.md`, `README-tw.md`, `docs/compress-setup.md`, `docs/compress-setup-tw.md`

**Interfaces:**
- Consumes: everything above.
- Produces: nothing code-facing.

- [ ] **Step 1: Write the failing test**

Add to `tests/codex_translate.rs` (it already has `spawn_gateway`, which takes the `compress` flag):

```rust
/// The upstream body must be translated AND smaller than the fat input.
/// If compression ever ran after translation the rewriter would see a
/// Responses body it does not understand and silently do nothing.
fn is_translated_and_compressed(req: &HttpMockRequest) -> bool {
    let Some(body) = req.body.as_ref() else { return false };
    let Ok(v) = serde_json::from_slice::<Value>(body) else { return false };
    v.get("input").is_some() && body.len() < 50_000
}

#[tokio::test]
async fn compression_runs_before_translation_on_the_codex_path() {
    let fat = "x".repeat(50_000);
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
```

- [ ] **Step 2: Run it**

Run: `cargo test --test codex_translate compression_runs_before_translation_on_the_codex_path`
Expected: PASS if Task 8 Step 3 was done correctly. If it FAILS, the ordering is wrong —
compression is running after translation, or not running at all. Fix the ordering in `forward()`,
not the test.

- [ ] **Step 3: Document the route**

In `README.md`, in the section listing routes, add:

```markdown
| `/codex/v1/chat/completions` | ChatGPT Codex, translated | Chat Completions in, Responses upstream. Point an OpenAI client here to use a ChatGPT subscription. |
```

Mirror it in `README-tw.md`. In `docs/compress-setup.md` and `docs/compress-setup-tw.md`, note that
compression now covers the translated Codex path and runs before translation.

Remove the stage-1 caveat that no MUR agent can reach the Codex route — it is no longer true.

- [ ] **Step 4: Verify the docs claim is true**

```bash
grep -rn "cannot use the Codex route\|until stage 2" README.md README-tw.md docs/
```

Expected: no hits. Any remaining sentence saying agents cannot reach Codex is now false.

- [ ] **Step 5: Full verification**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Expected: all green.

- [ ] **Step 6: Commit**

```bash
git add tests/codex_translate.rs README.md README-tw.md docs/
git commit -m "test(codex): prove compression precedes translation; document the route"
```

---

## Self-Review

**Spec coverage.** Routing → Task 2. Request flow and compression ordering → Tasks 8, 10.
`chat_to_responses` table → Tasks 3, 4. Parameter policy → Task 5. `responses_to_chat` table →
Task 6. Streaming table → Task 7, wired in Task 9. `finish_reason` precedence and the
content-plus-tool_calls case → Tasks 6, 4. Ground truth before code → Task 1. Error handling table →
Tasks 5, 8 (400s, non-2xx re-shaping, 502 on unparseable), 7 (mid-stream failure). 401 retry body →
Task 8's critical detail. Verification list → Tasks 3–10. Out-of-scope items are absent, as intended.

**Known soft spot.** Task 1 is the only task that cannot be done offline — it needs a live ChatGPT
subscription — and every later task's correctness rests on the fixtures and corrected tables it
produces. If Task 1 is skipped, Tasks 6, 7, 9 and 10 will fail at their fixture reads with an
explicit "run Task 1 first" message rather than silently passing against invented data.

**Dependency check performed.** `Cargo.toml` confirms `futures-util 0.3` and `httpmock 0.7` are
present, and that `async-stream`, `tokio-stream`, and tokio's `sync` feature are not. Task 9's
`unfold` construction is the only one available without violating the Global Constraints; it is
written out in full rather than left to the implementer's judgement.

**Type consistency.** `chat_to_responses(&Value) -> Result<Value, TranslateError>` and
`responses_to_chat(&Value, &str) -> Value` are used with those exact signatures in Tasks 8 and 9.
`SseTranslator::new/push/finish` match Task 7's definition. `codex::should_translate(&str) -> bool`
is defined in Task 2 and consumed in Tasks 8 and 10 (via `compress.rs`).
