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
}
