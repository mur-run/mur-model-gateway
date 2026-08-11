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
}
