//! Wire-level tool_result compression: rewrites Anthropic, OpenAI, and Gemini
//! request bodies through mur-compress before they leave for the upstream.
//! Fail-open — any parse or engine failure forwards the original bytes.

/// Paths whose bodies we compress, per provider.
/// `count_tokens` (and Gemini's `:countTokens`) are deliberately excluded —
/// their bodies never reach the model, and they run on a hot path.
/// The client over-counting context (vs the compressed send) is the fail-safe direction.
pub fn should_compress(path: &str, provider: Provider) -> bool {
    match provider {
        Provider::Anthropic => {
            path == "/v1/messages" || path.starts_with("/v1/messages?")
        }
        Provider::OpenAI => {
            path == "/v1/chat/completions" || path.starts_with("/v1/chat/completions?")
        }
        Provider::Gemini => {
            path.starts_with("/v1beta/models/")
                && (path.contains(":generateContent") || path.contains(":streamGenerateContent"))
        }
    }
}

/// True if `text` already carries a mur-compress retrieval marker
/// (`hash=` followed by 16+ hex chars). Over-matching is fail-safe: a
/// skipped block just stays uncompressed.
pub fn has_retrieve_marker(text: &str) -> bool {
    let mut rest = text;
    while let Some(i) = rest.find("hash=") {
        let tail = &rest[i + 5..];
        let hex_start = tail.strip_prefix('"').unwrap_or(tail);
        if hex_start
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .count()
            >= 16
        {
            return true;
        }
        rest = tail;
    }
    false
}

use crate::Provider;
use mur_compress::{CompressConfig, CompressEngine, auto_compress, retrieval_note};
use serde_json::Value;
use std::path::PathBuf;

/// Compress oversized `tool_result` text in `body`. Returns `Some(bytes)`
/// iff at least one block was replaced; `None` means "forward the original".
/// Sibling fields (`tool_use_id`, `is_error`, `cache_control`) survive
/// because mutation is in place — only text payloads are swapped.
fn rewrite_tool_results_anthropic(
    engine: &CompressEngine,
    min_tokens: usize,
    body: &[u8],
) -> Option<Vec<u8>> {
    let mut root: Value = serde_json::from_slice(body).ok()?;
    let messages = root.get_mut("messages")?.as_array_mut()?;
    let mut changed = false;
    for msg in messages.iter_mut() {
        let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        for block in content.iter_mut() {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                continue;
            }
            let Some(inner) = block.get_mut("content") else {
                continue;
            };
            match inner {
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
    }
    if !changed {
        return None;
    }
    serde_json::to_vec(&root).ok()
}

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

/// Replace `s` with its compressed form when compression fires.
/// Returns true iff replaced.
fn compress_text(engine: &CompressEngine, min_tokens: usize, s: &mut String) -> bool {
    if has_retrieve_marker(s) {
        return false;
    }
    let out = auto_compress(engine, s, None, min_tokens);
    if !out.fired {
        return false;
    }
    *s = match out.hash.as_deref() {
        Some(h) => format!("{}\n[{}]", out.text, retrieval_note(Some(h), None)),
        None => out.text,
    };
    true
}

/// mur's home resolution, mirrored: `MUR_HOME` env else `~/.mur`.
fn mur_home() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("MUR_HOME") {
        return Some(PathBuf::from(v));
    }
    directories::BaseDirs::new().map(|b| b.home_dir().join(".mur"))
}

/// Per-request engine rooted at `<mur_home>/compress` — the same store mur
/// itself uses, so `mur_retrieve` recovers proxy-compressed blocks. Returns
/// `None` (→ passthrough) when mur config disables compression or the
/// store can't be opened. Per-request construction matches mur's own
/// call sites (MCP server); cost is negligible next to an LLM round trip.
fn build_engine() -> Option<(CompressEngine, usize)> {
    let home = mur_home()?;
    let cfg = CompressConfig::load(&home);
    if !cfg.enabled || !cfg.auto.enabled {
        return None;
    }
    let min_tokens = cfg.auto.min_tokens;
    CompressEngine::new(home.join("compress"), cfg)
        .ok()
        .map(|e| (e, min_tokens))
}

/// Fail-open entry point for `forward()`: `Some(rewritten)` or `None` to
/// forward the original body untouched.
pub fn rewrite_request_body(body: &[u8], provider: Provider) -> Option<Vec<u8>> {
    let (engine, min_tokens) = build_engine()?;
    match provider {
        Provider::Anthropic => rewrite_tool_results_anthropic(&engine, min_tokens, body),
        Provider::OpenAI => rewrite_tool_results_openai(&engine, min_tokens, body),
        Provider::Gemini => rewrite_tool_results_gemini(&engine, min_tokens, body),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_compress_per_provider() {
        // Anthropic
        assert!(should_compress("/v1/messages", Provider::Anthropic));
        assert!(should_compress("/v1/messages?beta=true", Provider::Anthropic));
        assert!(!should_compress("/v1/messages/count_tokens", Provider::Anthropic));

        // OpenAI
        assert!(should_compress("/v1/chat/completions", Provider::OpenAI));
        assert!(should_compress("/v1/chat/completions?stream=true", Provider::OpenAI));
        assert!(!should_compress("/v1/chat/completions/messages", Provider::OpenAI));

        // Gemini — only :generateContent and :streamGenerateContent
        assert!(should_compress(
            "/v1beta/models/gemini-2.5-flash:generateContent",
            Provider::Gemini
        ));
        assert!(should_compress(
            "/v1beta/models/gemini-2.5-flash:streamGenerateContent",
            Provider::Gemini
        ));
        // countTokens and other non-chat endpoints excluded
        assert!(!should_compress(
            "/v1beta/models/gemini-2.5-flash:countTokens",
            Provider::Gemini
        ));
        assert!(!should_compress(
            "/v1beta/models/gemini-2.5-flash:embedContent",
            Provider::Gemini
        ));
    }

    #[test]
    fn marker_detection() {
        assert!(has_retrieve_marker(
            "[408 items compressed to 312. Retrieve more: hash=365700af5c32b04c0a4b0443]"
        ));
        assert!(has_retrieve_marker(
            "Call mur_retrieve with hash=\"e5f97460c423350e9d59d79e\" for the full result."
        ));
        // short hex after hash= is not a marker (e.g. a URL param)
        assert!(!has_retrieve_marker("GET /commit?hash=deadbeef done"));
        assert!(!has_retrieve_marker("no marker here"));
        // hash= at end of string must not panic
        assert!(!has_retrieve_marker("trailing hash="));
    }

    use mur_compress::{CompressConfig, CompressEngine};
    use serde_json::{Value, json};

    fn test_engine() -> (tempfile::TempDir, CompressEngine) {
        let dir = tempfile::tempdir().unwrap();
        let engine =
            CompressEngine::new(dir.path().join("store"), CompressConfig::default()).unwrap();
        (dir, engine)
    }

    /// Log-shaped text large enough that auto_compress reliably fires.
    fn fat_log() -> String {
        (0..1500)
            .map(|i| {
                format!(
                    "2026-07-03 12:{:02}:{:02} INFO worker-{}: request {} completed in {}ms status OK",
                    (i / 60) % 60, i % 60, i % 8, 100_000 + i, 10 + (i % 90)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn body_with_tool_result(content: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "model": "claude-sonnet-5",
            "max_tokens": 16,
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "is_error": false,
                     "cache_control": {"type": "ephemeral"},
                     "content": content}
                ]}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn compresses_fat_string_tool_result_and_preserves_siblings() {
        let (_dir, engine) = test_engine();
        let body = body_with_tool_result(json!(fat_log()));
        let out = rewrite_tool_results_anthropic(&engine, 800, &body).expect("should fire");
        assert!(out.len() < body.len(), "rewritten body must be smaller");

        let v: Value = serde_json::from_slice(&out).unwrap();
        let block = &v["messages"][1]["content"][0];
        assert_eq!(block["tool_use_id"], "toolu_1");
        assert_eq!(block["is_error"], false);
        assert_eq!(block["cache_control"]["type"], "ephemeral");
        let text = block["content"].as_str().unwrap();
        assert!(
            has_retrieve_marker(text),
            "compressed text carries a marker"
        );
        assert!(text.contains("mur_retrieve"), "retrieval note appended");
    }

    #[test]
    fn compresses_array_form_text_blocks_only() {
        let (_dir, engine) = test_engine();
        let body = body_with_tool_result(json!([
            {"type": "text", "text": fat_log()},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
        ]));
        let out = rewrite_tool_results_anthropic(&engine, 800, &body).expect("should fire");
        let v: Value = serde_json::from_slice(&out).unwrap();
        let items = v["messages"][1]["content"][0]["content"]
            .as_array()
            .unwrap();
        assert!(has_retrieve_marker(items[0]["text"].as_str().unwrap()));
        assert_eq!(items[1]["type"], "image", "non-text sibling untouched");
        assert_eq!(items[1]["source"]["data"], "AAAA");
    }

    #[test]
    fn small_blocks_and_markers_are_skipped() {
        let (_dir, engine) = test_engine();
        // Small block: under min_tokens → no rewrite at all.
        let body = body_with_tool_result(json!("short output"));
        assert!(rewrite_tool_results_anthropic(&engine, 800, &body).is_none());

        // Already-marked block, padded fat so the size gate alone can't save it.
        let marked = format!(
            "{}\n[1500 lines compressed. Retrieve more: hash=0123456789abcdef0123]",
            fat_log()
        );
        let body = body_with_tool_result(json!(marked));
        assert!(rewrite_tool_results_anthropic(&engine, 800, &body).is_none());
    }

    #[test]
    fn idempotent_second_pass_is_noop() {
        let (_dir, engine) = test_engine();
        let body = body_with_tool_result(json!(fat_log()));
        let once = rewrite_tool_results_anthropic(&engine, 800, &body).expect("first pass fires");
        assert!(
            rewrite_tool_results_anthropic(&engine, 800, &once).is_none(),
            "second pass must not double-compress"
        );
    }

    #[test]
    fn malformed_or_foreign_bodies_pass_through() {
        let (_dir, engine) = test_engine();
        assert!(rewrite_tool_results_anthropic(&engine, 800, b"not json {").is_none());
        assert!(rewrite_tool_results_anthropic(&engine, 800, br#"{"no_messages": true}"#).is_none());
        // string-content user message (no tool_result) untouched
        let plain = serde_json::to_vec(&json!({
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();
        assert!(rewrite_tool_results_anthropic(&engine, 800, &plain).is_none());
    }

    // ── Gemini extractor tests ──────────────────────────────────────────

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

    // ── OpenAI extractor tests ──────────────────────────────────────────

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
}
