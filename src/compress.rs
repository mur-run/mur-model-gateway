//! Wire-level tool_result compression: rewrites `/v1/messages` request
//! bodies through mur-compress before they leave for the upstream.
//! Fail-open — any parse or engine failure forwards the original bytes.

/// Paths whose bodies we compress: message sends only. `count_tokens` is
/// deliberately excluded — its body never reaches the model, and it runs on
/// a hot path. The client over-counting context (vs the compressed send) is
/// the fail-safe direction.
pub fn should_compress(path: &str) -> bool {
    path == "/v1/messages" || path.starts_with("/v1/messages?")
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

use mur_compress::{CompressEngine, auto_compress, retrieval_note};
use serde_json::Value;

/// Compress oversized `tool_result` text in `body`. Returns `Some(bytes)`
/// iff at least one block was replaced; `None` means "forward the original".
/// Sibling fields (`tool_use_id`, `is_error`, `cache_control`) survive
/// because mutation is in place — only text payloads are swapped.
pub fn rewrite_tool_results(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_compress_matches_messages_send_only() {
        assert!(should_compress("/v1/messages"));
        assert!(should_compress("/v1/messages?beta=true"));
        assert!(!should_compress("/v1/messages/count_tokens"));
        assert!(!should_compress("/v1/models"));
        assert!(!should_compress("/"));
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
        let out = rewrite_tool_results(&engine, 800, &body).expect("should fire");
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
        let out = rewrite_tool_results(&engine, 800, &body).expect("should fire");
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
        assert!(rewrite_tool_results(&engine, 800, &body).is_none());

        // Already-marked block, padded fat so the size gate alone can't save it.
        let marked = format!(
            "{}\n[1500 lines compressed. Retrieve more: hash=0123456789abcdef0123]",
            fat_log()
        );
        let body = body_with_tool_result(json!(marked));
        assert!(rewrite_tool_results(&engine, 800, &body).is_none());
    }

    #[test]
    fn idempotent_second_pass_is_noop() {
        let (_dir, engine) = test_engine();
        let body = body_with_tool_result(json!(fat_log()));
        let once = rewrite_tool_results(&engine, 800, &body).expect("first pass fires");
        assert!(
            rewrite_tool_results(&engine, 800, &once).is_none(),
            "second pass must not double-compress"
        );
    }

    #[test]
    fn malformed_or_foreign_bodies_pass_through() {
        let (_dir, engine) = test_engine();
        assert!(rewrite_tool_results(&engine, 800, b"not json {").is_none());
        assert!(rewrite_tool_results(&engine, 800, br#"{"no_messages": true}"#).is_none());
        // string-content user message (no tool_result) untouched
        let plain = serde_json::to_vec(&json!({
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();
        assert!(rewrite_tool_results(&engine, 800, &plain).is_none());
    }
}
