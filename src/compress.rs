//! Wire-level tool_result compression: rewrites Anthropic, OpenAI, and Gemini
//! request bodies through mur-compress before they leave for the upstream.
//! Fail-open — any parse or engine failure forwards the original bytes.

use crate::Provider;
use mur_compress::{CompressConfig, CompressEngine, auto_compress, retrieval_note};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

/// Why a `tool_result` was collapsed into a stub — drives the wording so the
/// model understands what happened without needing to retrieve the original.
enum StaleReason {
    /// A later `Read`/`Edit`/`Write`/`NotebookEdit` of the same file appears
    /// further along in the conversation; this one's view is out of date.
    SupersededPath(String),
    /// Byte-identical to a later tool result (e.g. a re-run build/test with
    /// unchanged output).
    Duplicate,
}

/// Extract the plain text of a `tool_result`'s `content` field, but only when
/// it is *purely* text (a bare string, or an array of only `type: "text"`
/// items). Mixed content (text alongside an image, say) is left `None` so
/// the caller never risks dropping a non-text sibling by overwriting the
/// whole field with a string.
fn extract_tool_result_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => {
            if items.is_empty()
                || !items
                    .iter()
                    .all(|i| i.get("type").and_then(|t| t.as_str()) == Some("text"))
            {
                return None;
            }
            Some(
                items
                    .iter()
                    .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }
        _ => None,
    }
}

/// Build the replacement text for a collapsed `tool_result`. Archives the
/// original unconditionally so it stays recoverable via `mur_retrieve`
/// whenever the store write succeeds; falls back to a non-retrievable note
/// (still fine — the content was stale, not lost information the model
/// currently needs).
///
/// For a superseded file Read, also tries to skeletonize the stale content
/// (imports/signatures/types kept, function bodies elided) and inlines it
/// below the marker — a denser *summary* than the plain note, at no extra
/// retrieval hop, for content that's already dead (never the most recent
/// view of a path, so no Edit anchor risk). Falls back to the plain marker
/// when the extension isn't recognized or there's nothing worth eliding.
fn stale_stub(engine: &CompressEngine, reason: &StaleReason, original: &str) -> String {
    let (what, skeleton) = match reason {
        StaleReason::SupersededPath(path) => (
            format!(
                "superseded: a later read or edit of {path} appears further below in this conversation"
            ),
            mur_compress::skeleton::skeletonize(original, path),
        ),
        StaleReason::Duplicate => (
            "identical to a later tool result in this conversation".to_string(),
            None,
        ),
    };
    let marker = match engine.archive(original) {
        Some(hash) => format!("[{what}. {}]", retrieval_note(Some(&hash), None)),
        None => format!("[{what}]"),
    };
    match skeleton {
        Some(sk) => format!("{marker}\n\n{sk}"),
        None => marker,
    }
}

/// One collapsible `tool_result`/`functionResponse`, stripped of any
/// provider-specific locator so the decision logic in
/// [`compute_stale_reasons`] is shared across Anthropic/OpenAI/Gemini.
struct StaleCandidate {
    path: Option<String>,
    is_read: bool,
    text: String,
}

/// Decide which `candidates` (in transcript order) are provably stale: a
/// Read whose file was read or edited again later, or a byte-exact duplicate
/// of a later result. The most recent view of any given path, and every
/// non-Read result, is left alone (`None`) — that's the content an in-flight
/// `Edit`'s `old_string` may still anchor against. Purely a function of the
/// input order/content, so repeated calls on the same transcript prefix
/// produce identical decisions (stable under prefix caching); a path's
/// collapse decision can only flip forward in time, the one turn a
/// superseding event first appears.
fn compute_stale_reasons(candidates: &[StaleCandidate]) -> Vec<Option<StaleReason>> {
    // Last event index touching each path, so a Read not in that slot is
    // provably superseded by something later.
    let mut path_last_idx: HashMap<&str, usize> = HashMap::new();
    for (i, c) in candidates.iter().enumerate() {
        if let Some(p) = &c.path {
            path_last_idx.insert(p.as_str(), i);
        }
    }

    let mut reason: Vec<Option<StaleReason>> = (0..candidates.len()).map(|_| None).collect();
    for (i, c) in candidates.iter().enumerate() {
        if c.is_read
            && let Some(p) = &c.path
            && path_last_idx.get(p.as_str()) != Some(&i)
        {
            reason[i] = Some(StaleReason::SupersededPath(p.clone()));
        }
    }
    // Exact-duplicate pass: among candidates not already superseded, an
    // earlier byte-identical text is a stale rerun of a later one.
    // Transcript-sized result counts are small enough that O(n^2) is fine.
    for i in 0..candidates.len() {
        if reason[i].is_some() {
            continue;
        }
        for j in (i + 1)..candidates.len() {
            if reason[j].is_none() && candidates[i].text == candidates[j].text {
                reason[i] = Some(StaleReason::Duplicate);
                break;
            }
        }
    }
    reason
}

/// Collapse `tool_result` blocks in Anthropic `messages` that
/// [`compute_stale_reasons`] finds stale into a short stub.
fn collapse_stale_tool_results_anthropic(engine: &CompressEngine, root: &mut Value) -> bool {
    let Some(messages) = root.get("messages").and_then(|m| m.as_array()) else {
        return false;
    };

    // tool_use_id -> (tool name, file_path if any)
    let mut tool_meta: HashMap<String, (String, Option<String>)> = HashMap::new();
    for msg in messages {
        if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        let Some(content) = msg.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for block in content {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                continue;
            }
            let Some(id) = block.get("id").and_then(|i| i.as_str()) else {
                continue;
            };
            let name = block
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_string();
            let path = block
                .get("input")
                .and_then(|i| i.get("file_path"))
                .and_then(|p| p.as_str())
                .map(String::from);
            tool_meta.insert(id.to_string(), (name, path));
        }
    }
    if tool_meta.is_empty() {
        return false;
    }

    let mut locators: Vec<(usize, usize)> = Vec::new(); // (msg_idx, block_idx)
    let mut candidates: Vec<StaleCandidate> = Vec::new();
    for (msg_idx, msg) in messages.iter().enumerate() {
        let Some(content) = msg.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for (block_idx, block) in content.iter().enumerate() {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                continue;
            }
            let Some(tool_use_id) = block.get("tool_use_id").and_then(|t| t.as_str()) else {
                continue;
            };
            let Some((name, path)) = tool_meta.get(tool_use_id) else {
                continue;
            };
            let Some(inner) = block.get("content") else {
                continue;
            };
            let Some(text) = extract_tool_result_text(inner) else {
                continue;
            };
            if text.is_empty() || has_retrieve_marker(&text) {
                continue;
            }
            locators.push((msg_idx, block_idx));
            candidates.push(StaleCandidate {
                path: path.clone(),
                is_read: name == "Read",
                text,
            });
        }
    }

    let reasons = compute_stale_reasons(&candidates);
    let mut changed = false;
    // Safe: `messages` (immutable) above is dropped before this call by NLL,
    // since `locators`/`candidates`/`reasons` only hold owned data.
    let messages_mut = root
        .get_mut("messages")
        .and_then(|m| m.as_array_mut())
        .expect("messages array checked present above");
    for (i, &(msg_idx, block_idx)) in locators.iter().enumerate() {
        let Some(r) = &reasons[i] else { continue };
        let stub = stale_stub(engine, r, &candidates[i].text);
        if let Some(content_field) = messages_mut
            .get_mut(msg_idx)
            .and_then(|m| m.get_mut("content"))
            .and_then(|c| c.get_mut(block_idx))
            .and_then(|b| b.get_mut("content"))
        {
            *content_field = Value::String(stub);
            changed = true;
        }
    }
    changed
}

/// Collapse stale `role: "tool"` results in OpenAI `messages`. Matching is
/// by `tool_call_id`, same as the wire protocol itself, so it's exact even
/// when multiple calls target the same file.
fn collapse_stale_tool_results_openai(engine: &CompressEngine, root: &mut Value) -> bool {
    let Some(messages) = root.get("messages").and_then(|m| m.as_array()) else {
        return false;
    };

    // tool_call_id -> (function name, file_path if any)
    let mut tool_meta: HashMap<String, (String, Option<String>)> = HashMap::new();
    for msg in messages {
        let Some(calls) = msg.get("tool_calls").and_then(|c| c.as_array()) else {
            continue;
        };
        for call in calls {
            let Some(id) = call.get("id").and_then(|i| i.as_str()) else {
                continue;
            };
            let Some(func) = call.get("function") else {
                continue;
            };
            let name = func
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_string();
            // `arguments` is a JSON-encoded string per the OpenAI wire format.
            let path = func
                .get("arguments")
                .and_then(|a| a.as_str())
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .and_then(|v| {
                    v.get("file_path")
                        .and_then(|p| p.as_str())
                        .map(String::from)
                });
            tool_meta.insert(id.to_string(), (name, path));
        }
    }
    if tool_meta.is_empty() {
        return false;
    }

    let mut locators: Vec<usize> = Vec::new(); // msg_idx
    let mut candidates: Vec<StaleCandidate> = Vec::new();
    for (msg_idx, msg) in messages.iter().enumerate() {
        if msg.get("role").and_then(|r| r.as_str()) != Some("tool") {
            continue;
        }
        let Some(tool_call_id) = msg.get("tool_call_id").and_then(|t| t.as_str()) else {
            continue;
        };
        let Some((name, path)) = tool_meta.get(tool_call_id) else {
            continue;
        };
        let Some(content) = msg.get("content") else {
            continue;
        };
        let Some(text) = extract_tool_result_text(content) else {
            continue;
        };
        if text.is_empty() || has_retrieve_marker(&text) {
            continue;
        }
        locators.push(msg_idx);
        candidates.push(StaleCandidate {
            path: path.clone(),
            is_read: name == "Read",
            text,
        });
    }

    let reasons = compute_stale_reasons(&candidates);
    let mut changed = false;
    let messages_mut = root
        .get_mut("messages")
        .and_then(|m| m.as_array_mut())
        .expect("messages array checked present above");
    for (i, &msg_idx) in locators.iter().enumerate() {
        let Some(r) = &reasons[i] else { continue };
        let stub = stale_stub(engine, r, &candidates[i].text);
        if let Some(content_field) = messages_mut
            .get_mut(msg_idx)
            .and_then(|m| m.get_mut("content"))
        {
            *content_field = Value::String(stub);
            changed = true;
        }
    }
    changed
}

/// Collapse stale `functionResponse` parts in Gemini `contents`. Gemini has
/// no call/response id on the wire — matching is FIFO per function name (the
/// order responses are sent back in), which is exact as long as calls to the
/// same function are answered in the order they were issued (true for any
/// well-formed transcript: a response always follows its own call).
fn collapse_stale_tool_results_gemini(engine: &CompressEngine, root: &mut Value) -> bool {
    let Some(contents) = root.get("contents").and_then(|c| c.as_array()) else {
        return false;
    };

    let mut pending_paths: HashMap<String, std::collections::VecDeque<Option<String>>> =
        HashMap::new();
    let mut locators: Vec<(usize, usize)> = Vec::new(); // (content_idx, part_idx)
    let mut candidates: Vec<StaleCandidate> = Vec::new();
    for (content_idx, content_item) in contents.iter().enumerate() {
        let Some(parts) = content_item.get("parts").and_then(|p| p.as_array()) else {
            continue;
        };
        for (part_idx, part) in parts.iter().enumerate() {
            if let Some(fc) = part.get("functionCall") {
                let name = fc
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or_default()
                    .to_string();
                let path = fc
                    .get("args")
                    .and_then(|a| a.get("file_path"))
                    .and_then(|p| p.as_str())
                    .map(String::from);
                pending_paths.entry(name).or_default().push_back(path);
                continue;
            }
            let Some(fr) = part.get("functionResponse") else {
                continue;
            };
            let name = fr.get("name").and_then(|n| n.as_str()).unwrap_or_default();
            let path = pending_paths
                .get_mut(name)
                .and_then(|q| q.pop_front())
                .flatten();
            let Some(response) = fr.get("response") else {
                continue;
            };
            let text = match response {
                Value::String(s) => s.clone(),
                Value::Object(obj) => match obj.get("result") {
                    Some(Value::String(s)) => s.clone(),
                    _ => continue,
                },
                _ => continue,
            };
            if text.is_empty() || has_retrieve_marker(&text) {
                continue;
            }
            locators.push((content_idx, part_idx));
            candidates.push(StaleCandidate {
                path,
                is_read: name == "Read",
                text,
            });
        }
    }
    if candidates.is_empty() {
        return false;
    }

    let reasons = compute_stale_reasons(&candidates);
    let mut changed = false;
    let contents_mut = root
        .get_mut("contents")
        .and_then(|c| c.as_array_mut())
        .expect("contents array checked present above");
    for (i, &(content_idx, part_idx)) in locators.iter().enumerate() {
        let Some(r) = &reasons[i] else { continue };
        let stub = stale_stub(engine, r, &candidates[i].text);
        let Some(part) = contents_mut
            .get_mut(content_idx)
            .and_then(|c| c.get_mut("parts"))
            .and_then(|p| p.get_mut(part_idx))
            .and_then(|p| p.get_mut("functionResponse"))
        else {
            continue;
        };
        match part.get_mut("response") {
            Some(r @ Value::String(_)) => {
                *r = Value::String(stub);
                changed = true;
            }
            Some(Value::Object(obj)) => {
                obj.insert("result".to_string(), Value::String(stub));
                changed = true;
            }
            _ => {}
        }
    }
    changed
}

/// Paths whose bodies we compress, per provider.
/// `count_tokens` (and Gemini's `:countTokens`) are deliberately excluded —
/// their bodies never reach the model, and they run on a hot path.
/// The client over-counting context (vs the compressed send) is the fail-safe direction.
pub fn should_compress(path: &str, provider: Provider) -> bool {
    match provider {
        Provider::Anthropic => path == "/v1/messages" || path.starts_with("/v1/messages?"),
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
    let mut changed = collapse_stale_tool_results_anthropic(engine, &mut root);
    let Some(messages) = root.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return changed.then(|| serde_json::to_vec(&root).ok()).flatten();
    };
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
    let mut changed = collapse_stale_tool_results_openai(engine, &mut root);
    let Some(messages) = root.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return changed.then(|| serde_json::to_vec(&root).ok()).flatten();
    };
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
    let mut changed = collapse_stale_tool_results_gemini(engine, &mut root);
    let Some(contents) = root.get_mut("contents").and_then(|c| c.as_array_mut()) else {
        return changed.then(|| serde_json::to_vec(&root).ok()).flatten();
    };
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
        assert!(should_compress(
            "/v1/messages?beta=true",
            Provider::Anthropic
        ));
        assert!(!should_compress(
            "/v1/messages/count_tokens",
            Provider::Anthropic
        ));

        // OpenAI
        assert!(should_compress("/v1/chat/completions", Provider::OpenAI));
        assert!(should_compress(
            "/v1/chat/completions?stream=true",
            Provider::OpenAI
        ));
        assert!(!should_compress(
            "/v1/chat/completions/messages",
            Provider::OpenAI
        ));

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

    /// A Read/Edit/Write `tool_use` block paired with a text `tool_result`.
    fn tool_use_and_result(id: &str, name: &str, file_path: &str, result_text: &str) -> Value {
        json!([
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": id, "name": name, "input": {"file_path": file_path}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": id, "content": result_text}
            ]}
        ])
    }

    #[test]
    fn superseded_read_collapses_and_stays_retrievable() {
        let (_dir, engine) = test_engine();
        let original = "fn main() {}\n".repeat(5);
        let mut messages = Vec::new();
        messages.extend(
            tool_use_and_result("toolu_read1", "Read", "src/main.rs", &original)
                .as_array()
                .unwrap()
                .clone(),
        );
        messages.extend(
            tool_use_and_result(
                "toolu_read2",
                "Read",
                "src/main.rs",
                "fn main() { changed(); }",
            )
            .as_array()
            .unwrap()
            .clone(),
        );
        let body =
            serde_json::to_vec(&json!({"messages": messages, "model": "x", "max_tokens": 16}))
                .unwrap();

        let out = rewrite_tool_results_anthropic(&engine, 800, &body).expect("should fire");
        let v: Value = serde_json::from_slice(&out).unwrap();

        let first = v["messages"][1]["content"][0]["content"].as_str().unwrap();
        assert!(first.contains("superseded"));
        assert!(first.contains("src/main.rs"));
        assert!(has_retrieve_marker(first), "original stays retrievable");

        // The later (most recent) read of the same path is untouched.
        let second = v["messages"][3]["content"][0]["content"].as_str().unwrap();
        assert_eq!(second, "fn main() { changed(); }");
    }

    #[test]
    fn superseded_read_of_recognized_language_inlines_a_skeleton() {
        let (_dir, engine) = test_engine();
        let original = "fn add(a: i32, b: i32) -> i32 {\n    let sum = a + b;\n    sum\n}\n\nstruct Point { x: i32 }\n";
        let mut messages = Vec::new();
        messages.extend(
            tool_use_and_result("toolu_r1", "Read", "src/math.rs", original)
                .as_array()
                .unwrap()
                .clone(),
        );
        messages.extend(
            tool_use_and_result(
                "toolu_r2",
                "Read",
                "src/math.rs",
                "fn add(a: i32, b: i32) -> i32 { a + b }",
            )
            .as_array()
            .unwrap()
            .clone(),
        );
        let body =
            serde_json::to_vec(&json!({"messages": messages, "model": "x", "max_tokens": 16}))
                .unwrap();

        let out = rewrite_tool_results_anthropic(&engine, 800, &body).expect("should fire");
        let v: Value = serde_json::from_slice(&out).unwrap();
        let first = v["messages"][1]["content"][0]["content"].as_str().unwrap();
        assert!(first.contains("superseded"));
        // Skeleton kept the signature and struct, elided the body.
        assert!(first.contains("fn add(a: i32, b: i32) -> i32"));
        assert!(first.contains("struct Point { x: i32 }"));
        assert!(!first.contains("let sum = a + b;"));
        assert!(has_retrieve_marker(first));
    }

    #[test]
    fn read_followed_by_edit_of_same_path_is_superseded() {
        let (_dir, engine) = test_engine();
        let original = "line one\nline two\n".repeat(10);
        let mut messages = Vec::new();
        messages.extend(
            tool_use_and_result("toolu_read", "Read", "src/lib.rs", &original)
                .as_array()
                .unwrap()
                .clone(),
        );
        messages.extend(
            tool_use_and_result("toolu_edit", "Edit", "src/lib.rs", "edited 1 line")
                .as_array()
                .unwrap()
                .clone(),
        );
        let body =
            serde_json::to_vec(&json!({"messages": messages, "model": "x", "max_tokens": 16}))
                .unwrap();

        let out = rewrite_tool_results_anthropic(&engine, 800, &body).expect("should fire");
        let v: Value = serde_json::from_slice(&out).unwrap();
        let first = v["messages"][1]["content"][0]["content"].as_str().unwrap();
        assert!(first.contains("superseded"));
        // The edit's own (small) result is left alone entirely.
        let second = v["messages"][3]["content"][0]["content"].as_str().unwrap();
        assert_eq!(second, "edited 1 line");
    }

    #[test]
    fn most_recent_read_of_a_path_is_never_collapsed() {
        let (_dir, engine) = test_engine();
        let only = "just one read, nothing supersedes it".repeat(5);
        let body = serde_json::to_vec(&json!({
            "messages": tool_use_and_result("toolu_only", "Read", "src/only.rs", &only),
            "model": "x",
            "max_tokens": 16,
        }))
        .unwrap();
        let out = rewrite_tool_results_anthropic(&engine, 800, &body);
        // No supersession possible (nothing later touches the path) and the
        // text is under the min_tokens gate, so nothing should fire.
        assert!(out.is_none());
    }

    #[test]
    fn exact_duplicate_tool_result_collapses_to_last_occurrence() {
        let (_dir, engine) = test_engine();
        let repeated_output = "cargo test\n".to_string() + &"test result: ok\n".repeat(50);
        let body = serde_json::to_vec(&json!({
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_a", "name": "bash", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_a", "content": repeated_output}
                ]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_b", "name": "bash", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_b", "content": repeated_output}
                ]},
            ],
            "model": "x",
            "max_tokens": 16,
        }))
        .unwrap();

        let out = rewrite_tool_results_anthropic(&engine, 800, &body).expect("should fire");
        let v: Value = serde_json::from_slice(&out).unwrap();
        let first = v["messages"][1]["content"][0]["content"].as_str().unwrap();
        assert!(first.contains("identical to a later tool result"));
        let second = v["messages"][3]["content"][0]["content"].as_str().unwrap();
        assert_eq!(second, repeated_output, "last occurrence kept in full");
    }

    #[test]
    fn collapse_pass_is_idempotent_and_prefix_stable() {
        let (_dir, engine) = test_engine();
        let original = "struct Foo { x: i32 }\n".repeat(5);
        let mut messages = Vec::new();
        messages.extend(
            tool_use_and_result("toolu_r1", "Read", "src/foo.rs", &original)
                .as_array()
                .unwrap()
                .clone(),
        );
        messages.extend(
            tool_use_and_result(
                "toolu_r2",
                "Read",
                "src/foo.rs",
                "struct Foo { x: i32, y: i32 }",
            )
            .as_array()
            .unwrap()
            .clone(),
        );
        let body =
            serde_json::to_vec(&json!({"messages": messages, "model": "x", "max_tokens": 16}))
                .unwrap();

        let once = rewrite_tool_results_anthropic(&engine, 800, &body).expect("first pass fires");
        // Second pass on the same input is a pure function of the transcript:
        // identical bytes out, every time (prefix-cache safe).
        let twice = rewrite_tool_results_anthropic(&engine, 800, &once);
        assert!(
            twice.is_none() || twice.unwrap() == once,
            "re-collapsing an already-collapsed transcript must be a no-op"
        );
    }

    #[test]
    fn mixed_content_tool_result_is_never_collapsed_for_supersession() {
        let (_dir, engine) = test_engine();
        // Read result with an image sibling: extract_tool_result_text bails
        // out (None), so this must never be touched even though a later Read
        // of the same path exists.
        let mut messages = Vec::new();
        messages.push(json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "toolu_img", "name": "Read", "input": {"file_path": "img.rs"}}
        ]}));
        messages.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_img", "content": [
                {"type": "text", "text": "some code"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
            ]}
        ]}));
        messages.extend(
            tool_use_and_result("toolu_img2", "Read", "img.rs", "later read")
                .as_array()
                .unwrap()
                .clone(),
        );
        let body =
            serde_json::to_vec(&json!({"messages": messages, "model": "x", "max_tokens": 16}))
                .unwrap();
        let out = rewrite_tool_results_anthropic(&engine, 800, &body);
        if let Some(out) = out {
            let v: Value = serde_json::from_slice(&out).unwrap();
            let items = v["messages"][1]["content"][0]["content"]
                .as_array()
                .unwrap();
            assert_eq!(
                items[0]["text"], "some code",
                "mixed-content read left intact"
            );
            assert_eq!(items[1]["type"], "image");
        }
    }

    #[test]
    fn malformed_or_foreign_bodies_pass_through() {
        let (_dir, engine) = test_engine();
        assert!(rewrite_tool_results_anthropic(&engine, 800, b"not json {").is_none());
        assert!(
            rewrite_tool_results_anthropic(&engine, 800, br#"{"no_messages": true}"#).is_none()
        );
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
        assert!(has_retrieve_marker(
            fr["response"]["result"].as_str().unwrap()
        ));
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
    fn gemini_mixed_parts_survive_compression() {
        let (_dir, engine) = test_engine();
        // A functionResponse alongside a non-functionResponse part —
        // only the functionResponse text is compressed; sibling survives.
        let body = serde_json::to_vec(&json!({
            "contents": [
                {"role": "user", "parts": [
                    {"functionResponse": {"name": "bash", "response": {"result": fat_log()}}},
                    {"inlineData": {"mimeType": "image/png", "data": "AAAA"}}
                ]}
            ]
        }))
        .unwrap();
        let out = rewrite_tool_results_gemini(&engine, 800, &body).expect("should fire");
        let v: Value = serde_json::from_slice(&out).unwrap();
        let parts = v["contents"][0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        // First part: functionResponse.response.result was compressed.
        assert!(has_retrieve_marker(
            parts[0]["functionResponse"]["response"]["result"]
                .as_str()
                .unwrap()
        ));
        // Second part: non-functionResponse sibling untouched.
        assert_eq!(parts[1]["inlineData"]["mimeType"], "image/png");
        assert_eq!(parts[1]["inlineData"]["data"], "AAAA");
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
            {"type": "text", "text": fat_log()},
            {"type": "image_url", "image_url": {"url": "https://example.com/img.png"}}
        ]));
        let out = rewrite_tool_results_openai(&engine, 800, &body).expect("should fire");
        let v: Value = serde_json::from_slice(&out).unwrap();
        let content = &v["messages"][1]["content"];
        let items = content.as_array().unwrap();
        assert!(has_retrieve_marker(items[0]["text"].as_str().unwrap()));
        // Non-text sibling must survive unchanged.
        assert_eq!(items[1]["type"], "image_url");
        assert_eq!(items[1]["image_url"]["url"], "https://example.com/img.png");
    }

    #[test]
    fn openai_superseded_read_collapses_by_tool_call_id() {
        let (_dir, engine) = test_engine();
        let original = "def foo():\n    pass\n".repeat(10);
        let body = serde_json::to_vec(&json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "assistant", "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {
                        "name": "Read", "arguments": "{\"file_path\": \"src/foo.py\"}"
                    }}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": original},
                {"role": "assistant", "tool_calls": [
                    {"id": "call_2", "type": "function", "function": {
                        "name": "Read", "arguments": "{\"file_path\": \"src/foo.py\"}"
                    }}
                ]},
                {"role": "tool", "tool_call_id": "call_2", "content": "def foo():\n    return 1\n"},
            ],
        }))
        .unwrap();

        let out = rewrite_tool_results_openai(&engine, 800, &body).expect("should fire");
        let v: Value = serde_json::from_slice(&out).unwrap();
        let first = v["messages"][1]["content"].as_str().unwrap();
        assert!(first.contains("superseded"));
        assert!(first.contains("src/foo.py"));
        assert!(has_retrieve_marker(first));
        let second = v["messages"][3]["content"].as_str().unwrap();
        assert_eq!(second, "def foo():\n    return 1\n");
    }

    #[test]
    fn openai_exact_duplicate_collapses_to_last() {
        let (_dir, engine) = test_engine();
        let repeated = "npm test\n".to_string() + &"PASS\n".repeat(50);
        let body = serde_json::to_vec(&json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "assistant", "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": repeated},
                {"role": "assistant", "tool_calls": [
                    {"id": "call_2", "type": "function", "function": {"name": "bash", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_2", "content": repeated},
            ],
        }))
        .unwrap();
        let out = rewrite_tool_results_openai(&engine, 800, &body).expect("should fire");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(
            v["messages"][1]["content"]
                .as_str()
                .unwrap()
                .contains("identical to a later tool result")
        );
        assert_eq!(v["messages"][3]["content"].as_str().unwrap(), repeated);
    }

    #[test]
    fn gemini_superseded_read_collapses_fifo_by_name() {
        let (_dir, engine) = test_engine();
        let original = "class Foo:\n    pass\n".repeat(10);
        let body = serde_json::to_vec(&json!({
            "contents": [
                {"role": "model", "parts": [
                    {"functionCall": {"name": "Read", "args": {"file_path": "src/foo.py"}}}
                ]},
                {"role": "user", "parts": [
                    {"functionResponse": {"name": "Read", "response": {"result": original}}}
                ]},
                {"role": "model", "parts": [
                    {"functionCall": {"name": "Read", "args": {"file_path": "src/foo.py"}}}
                ]},
                {"role": "user", "parts": [
                    {"functionResponse": {"name": "Read", "response": {"result": "class Foo:\n    x = 1\n"}}}
                ]},
            ]
        }))
        .unwrap();

        let out = rewrite_tool_results_gemini(&engine, 800, &body).expect("should fire");
        let v: Value = serde_json::from_slice(&out).unwrap();
        let first = v["contents"][1]["parts"][0]["functionResponse"]["response"]["result"]
            .as_str()
            .unwrap();
        assert!(first.contains("superseded"));
        assert!(first.contains("src/foo.py"));
        assert!(has_retrieve_marker(first));
        let second = v["contents"][3]["parts"][0]["functionResponse"]["response"]["result"]
            .as_str()
            .unwrap();
        assert_eq!(second, "class Foo:\n    x = 1\n");
    }

    #[test]
    fn gemini_exact_duplicate_collapses_to_last() {
        let (_dir, engine) = test_engine();
        let repeated = "go test ./...\n".to_string() + &"ok\n".repeat(50);
        let body = serde_json::to_vec(&json!({
            "contents": [
                {"role": "model", "parts": [{"functionCall": {"name": "bash", "args": {}}}]},
                {"role": "user", "parts": [
                    {"functionResponse": {"name": "bash", "response": {"result": repeated}}}
                ]},
                {"role": "model", "parts": [{"functionCall": {"name": "bash", "args": {}}}]},
                {"role": "user", "parts": [
                    {"functionResponse": {"name": "bash", "response": {"result": repeated}}}
                ]},
            ]
        }))
        .unwrap();
        let out = rewrite_tool_results_gemini(&engine, 800, &body).expect("should fire");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(
            v["contents"][1]["parts"][0]["functionResponse"]["response"]["result"]
                .as_str()
                .unwrap()
                .contains("identical to a later tool result")
        );
        assert_eq!(
            v["contents"][3]["parts"][0]["functionResponse"]["response"]["result"]
                .as_str()
                .unwrap(),
            repeated
        );
    }
}
