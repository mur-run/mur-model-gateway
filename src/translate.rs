//! Chat Completions ↔ Responses translation.
//!
//! Pure functions: no network, no state beyond `SseTranslator`'s cursor.
//! Both API shapes are public documentation, so unlike the Codex client
//! headers this module is tracked source.

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

    // Semantic parameters: the Responses API cannot honour these, and
    // silently ignoring them would hand back a result the caller believes
    // is seeded / scored / schema-constrained when it is not.
    for param in ["seed", "logprobs", "top_logprobs", "response_format"] {
        if chat.contains_key(param) && !chat[param].is_null() {
            return Err(TranslateError::Rejected {
                param: match param {
                    "seed" => "seed",
                    "logprobs" => "logprobs",
                    "top_logprobs" => "top_logprobs",
                    _ => "response_format",
                },
            });
        }
    }
    if chat.get("n").and_then(Value::as_u64).is_some_and(|n| n > 1) {
        return Err(TranslateError::Rejected { param: "n" });
    }

    // Cosmetic parameters are dropped, not rejected: clients send library
    // defaults for these and would break on a 400.
    for param in [
        "presence_penalty",
        "frequency_penalty",
        "logit_bias",
        "user",
        "stop",
    ] {
        if chat.contains_key(param) {
            tracing::warn!(param, "parameter has no Responses equivalent, dropped");
        }
    }

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

    let empty = Vec::new();
    for msg in chat
        .get("messages")
        .and_then(Value::as_array)
        .unwrap_or(&empty)
    {
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
                for call in msg
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .unwrap_or(&empty)
                {
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
    }

    if !instructions.is_empty() {
        out.insert("instructions".into(), json!(instructions.join("\n")));
    }
    out.insert("input".into(), Value::Array(input));

    if let Some(v) = chat
        .get("max_tokens")
        .or_else(|| chat.get("max_completion_tokens"))
    {
        out.insert("max_output_tokens".into(), v.clone());
    }
    for name in ["temperature", "top_p", "tool_choice"] {
        if let Some(v) = chat.get(name) {
            out.insert(name.into(), v.clone());
        }
    }

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

    Ok(Value::Object(out))
}

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
        for key in [
            "id",
            "created_at",
            "status",
            "model",
            "usage",
            "incomplete_details",
        ] {
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

/// Translate a Responses reply into a Chat Completions reply. `model` is the
/// string the client sent, echoed back verbatim.
pub fn responses_to_chat(resp: &Value, model: &str) -> Value {
    let empty = Vec::new();
    let output = resp
        .get("output")
        .and_then(Value::as_array)
        .unwrap_or(&empty);

    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .unwrap_or(&empty)
                {
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
                    .and_then(|id| self.item_index.get(id).copied());
                if let Some(index) = index {
                    if let Some(d) = data.get("delta").and_then(Value::as_str) {
                        out.push(self.chunk(
                            json!({"tool_calls": [{
                                "index": index,
                                "function": {"arguments": d},
                            }]}),
                            Value::Null,
                        ));
                    }
                } else {
                    tracing::warn!(
                        item_id = ?data.get("item_id"),
                        "orphaned tool-call argument delta dropped"
                    );
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
        let reason = if self.saw_tool_call {
            "tool_calls"
        } else {
            "stop"
        };
        vec![self.chunk(json!({}), json!(reason))]
    }
}

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
        assert_eq!(
            input.len(),
            2,
            "system becomes instructions, not an input item"
        );
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
        assert_eq!(
            chat_to_responses(&chat).unwrap()["max_output_tokens"],
            json!(99)
        );
    }

    #[test]
    fn rejects_non_object_body() {
        assert!(matches!(
            chat_to_responses(&json!([1, 2, 3])),
            Err(TranslateError::NotAnObject)
        ));
    }

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
                Err(TranslateError::Rejected {
                    param: match param {
                        "seed" => "seed",
                        "logprobs" => "logprobs",
                        "top_logprobs" => "top_logprobs",
                        _ => "response_format",
                    }
                }),
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
        for dropped in [
            "user",
            "presence_penalty",
            "frequency_penalty",
            "logit_bias",
            "stop",
        ] {
            assert!(
                out.get(dropped).is_none(),
                "{dropped} must not reach upstream"
            );
        }
        assert_eq!(out["input"][0]["content"][0]["text"], json!("hi"));
    }

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
        assert_eq!(
            buf, "event: b\ndata: {\"y\"",
            "the partial frame stays buffered"
        );

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
        assert_eq!(
            call["function"]["arguments"],
            json!("{\"city\":\"Taipei\"}")
        );
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
        assert!(
            !out.to_string().contains("thinking"),
            "reasoning must not leak"
        );
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
        assert_eq!(
            text, "1\n2\n3\n4\n5",
            "deltas must reassemble to the captured reply"
        );

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
    fn drops_orphaned_tool_call_argument_deltas() {
        let mut t = SseTranslator::new("m".into());
        // Delta with an item_id that was never announced — must be dropped.
        let chunks = t.push(
            "response.function_call_arguments.delta",
            &json!({"item_id": "unknown_item", "delta": "orphan"}),
        );
        // The first push always emits a role chunk, but the orphaned delta
        // must NOT produce any tool_calls chunk.
        let tool_call_chunks: Vec<_> = chunks
            .iter()
            .filter(|c| c["choices"][0]["delta"]["tool_calls"].is_array())
            .collect();
        assert!(
            tool_call_chunks.is_empty(),
            "orphaned delta must produce no tool_calls chunks (was silently attached to index 0)"
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
}
