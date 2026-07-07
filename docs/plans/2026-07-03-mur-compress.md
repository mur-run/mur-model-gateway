# cc-proxy × mur-compress Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Compress oversized `tool_result` blocks in `/v1/messages` request bodies through the mur-compress engine before forwarding upstream, sharing `~/.mur/compress` so `mur_retrieve` recovers originals.

**Architecture:** New `src/compress.rs` module in cc-proxy. In `forward()`, after body buffering and before the disguise step, an env-gated rewrite parses the JSON body, compresses fat `tool_result` text via `mur_compress::auto_compress`, and re-serializes. Everything is fail-open: any error forwards the original bytes.

**Tech Stack:** Rust edition 2024, axum 0.8, `mur-compress` (path dep on the sibling mur checkout), httpmock + tempfile for tests.

**Spec:** `docs/specs/2026-07-03-mur-compress-design.md` (this repo).

## Global Constraints

- **Fail-open everywhere:** JSON parse failure, engine init failure, no home dir → forward the original body untouched. Compression must never 4xx/5xx a request.
- **`tool_result` blocks only.** Never touch `system`, user text, or assistant turns.
- **Preserve sibling fields:** `tool_use_id`, `is_error`, `cache_control` must survive the rewrite (mutate text values in place; never rebuild blocks).
- **No hardcoded thresholds:** `min_tokens` and all gates come from mur's `CompressConfig::load(mur_home)` (`~/.mur/compress.yaml`, defaults built in). Honor `cfg.enabled` and `cfg.auto.enabled`.
- **Opt-in rollout:** compression runs only when env `CC_PROXY_COMPRESS=1`. Default off.
- **Zero changes to the mur repo.**
- Repo hygiene: `cargo fmt` clean, `cargo clippy -- -D warnings` clean, single source file ≤ 800 lines.
- The mur checkout must exist at `../mur` relative to this repo (`/Volumes/Firecuda4tb/Projects/mur`).

---

### Task 1: Dependency + `compress` module skeleton (path gate, marker detector)

**Files:**
- Modify: `Cargo.toml` (add `mur-compress` dep, `tempfile` dev-dep)
- Create: `src/compress.rs`
- Modify: `src/lib.rs` (add `pub mod compress;` next to `pub mod disguise;`)

**Interfaces:**
- Produces: `compress::should_compress(path: &str) -> bool`, `compress::has_retrieve_marker(text: &str) -> bool`. Task 2 builds the rewrite core in the same module; Task 3 calls `should_compress` from `forward()`.

- [ ] **Step 1: Add dependencies**

In `Cargo.toml` under `[dependencies]` add:

```toml
mur-compress = { path = "../mur/mur-compress" }
```

Under `[dev-dependencies]` add:

```toml
tempfile = "3"
```

Run: `cargo check`
Expected: compiles (pulls tiktoken-rs, blake3, regex, flate2 — all pure Rust).

- [ ] **Step 2: Write failing tests for the two pure gates**

Create `src/compress.rs`:

```rust
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
        if tail.chars().take_while(|c| c.is_ascii_hexdigit()).count() >= 16 {
            return true;
        }
        rest = tail;
    }
    false
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
}
```

In `src/lib.rs`, next to `pub mod disguise;` add:

```rust
pub mod compress;
```

- [ ] **Step 3: Run tests**

Run: `cargo test compress::`
Expected: both tests PASS (functions are written with the tests; this task's "failing first" is the initial `cargo check` without the module — keep the commit atomic instead of splitting hairs).

- [ ] **Step 4: Lint and commit**

```bash
cargo fmt && cargo clippy -- -D warnings
git add Cargo.toml Cargo.lock src/compress.rs src/lib.rs
git commit -m "feat(compress): mur-compress dep + path gate and marker detector"
```

---

### Task 2: Rewrite core — `rewrite_tool_results`

**Files:**
- Modify: `src/compress.rs`

**Interfaces:**
- Consumes: `has_retrieve_marker` from Task 1; `mur_compress::{auto_compress, retrieval_note, CompressEngine, CompressConfig}`.
- Produces: `compress::rewrite_tool_results(engine: &CompressEngine, min_tokens: usize, body: &[u8]) -> Option<Vec<u8>>` — `Some(bytes)` iff at least one block was replaced, `None` means "forward the original". Task 3 wraps it with engine construction.

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `src/compress.rs`:

```rust
    use mur_compress::{CompressConfig, CompressEngine};
    use serde_json::{Value, json};

    fn test_engine() -> (tempfile::TempDir, CompressEngine) {
        let dir = tempfile::tempdir().unwrap();
        let engine = CompressEngine::new(dir.path().join("store"), CompressConfig::default())
            .unwrap();
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
        assert!(has_retrieve_marker(text), "compressed text carries a marker");
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
        let items = v["messages"][1]["content"][0]["content"].as_array().unwrap();
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test compress:: 2>&1 | tail -5`
Expected: FAIL — `rewrite_tool_results` not found.

- [ ] **Step 3: Implement the rewrite core**

Add to `src/compress.rs` (above the tests module):

```rust
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test compress::`
Expected: all PASS. If `compresses_fat_string_tool_result_and_preserves_siblings` fails on "should fire", the log heuristic didn't detect the content — make `fat_log()` longer (3000 lines) rather than changing the implementation.

- [ ] **Step 5: Lint and commit**

```bash
cargo fmt && cargo clippy -- -D warnings
git add src/compress.rs
git commit -m "feat(compress): tool_result rewrite core over mur-compress"
```

---

### Task 3: Wire into `forward()` — env gate, engine builder, integration test

**Files:**
- Modify: `src/compress.rs` (engine builder + public entry point)
- Modify: `src/lib.rs` (`AppState.compress` field, `forward()` insertion)
- Create: `tests/compress_e2e.rs`

**Interfaces:**
- Consumes: `rewrite_tool_results` from Task 2.
- Produces: `compress::rewrite_request_body(body: &[u8]) -> Option<Vec<u8>>`; `pub compress: bool` on `AppState` (read from env `CC_PROXY_COMPRESS` in `with_version`, overridable by tests via the pub field).

- [ ] **Step 1: Write the failing integration test**

Create `tests/compress_e2e.rs`:

```rust
//! Wire-level compression acceptance: with CC_PROXY_COMPRESS on, a fat
//! tool_result in /v1/messages reaches the upstream compressed; with the
//! flag off the body is forwarded byte-identical.

use cc_proxy::{AppState, TokenSource, build_router};
use httpmock::prelude::*;
use std::time::Duration;

async fn spawn_proxy(upstream: String, compress: bool) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut state = AppState::new(&upstream, TokenSource::Disabled).unwrap();
    state.compress = compress;
    let app = build_router(state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    addr.to_string()
}

fn fat_body() -> serde_json::Value {
    let log: String = (0..1500)
        .map(|i| {
            format!(
                "2026-07-03 12:{:02}:{:02} INFO worker-{}: request {} completed in {}ms status OK",
                (i / 60) % 60, i % 60, i % 8, 100_000 + i, 10 + (i % 90)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::json!({
        "model": "claude-sonnet-5",
        "max_tokens": 16,
        "messages": [
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1",
                 "cache_control": {"type": "ephemeral"},
                 "content": log}
            ]}
        ]
    })
}

#[tokio::test]
async fn compresses_when_enabled_and_passes_through_when_disabled() {
    // Isolate the CCR store from the real ~/.mur. Both scenarios run in
    // this one test to avoid parallel env races.
    let mur_home = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("MUR_HOME", mur_home.path()) };

    let body = fat_body();
    let raw_len = serde_json::to_vec(&body).unwrap().len();

    // ── enabled: upstream sees a smaller body with a retrieval note ──
    let upstream = MockServer::start_async().await;
    let mock = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/v1/messages").body_contains("mur_retrieve");
            then.status(200).body("{}");
        })
        .await;
    let addr = spawn_proxy(upstream.base_url(), true).await;
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    mock.assert_async().await; // upstream received the compressed body

    // ── disabled: upstream sees the original, full-size body ──
    let upstream2 = MockServer::start_async().await;
    let mock2 = upstream2
        .mock_async(move |when, then| {
            when.method(POST)
                .path("/v1/messages")
                .matches(move |req| {
                    req.body.as_ref().is_some_and(|b| b.len() >= raw_len)
                });
            then.status(200).body("{}");
        })
        .await;
    let addr2 = spawn_proxy(upstream2.base_url(), false).await;
    let resp2 = reqwest::Client::new()
        .post(format!("http://{addr2}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status(), 200);
    mock2.assert_async().await;
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --test compress_e2e 2>&1 | tail -5`
Expected: FAIL — `AppState` has no field `compress`.

- [ ] **Step 3: Implement engine builder + entry point**

Add to `src/compress.rs`:

```rust
use mur_compress::CompressConfig;
use std::path::PathBuf;

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
pub fn rewrite_request_body(body: &[u8]) -> Option<Vec<u8>> {
    let (engine, min_tokens) = build_engine()?;
    rewrite_tool_results(&engine, min_tokens, body)
}
```

- [ ] **Step 4: Add the AppState field and forward() insertion**

In `src/lib.rs`, add the field to `AppState`:

```rust
#[derive(Clone)]
pub struct AppState {
    pub upstream: String,
    pub client: reqwest::Client,
    pub token_source: TokenSource,
    pub version_cache: Arc<cc_version::VersionCache>,
    /// Wire-level tool_result compression (spec: docs/specs/2026-07-03).
    /// Env-gated: CC_PROXY_COMPRESS=1. Tests flip the field directly.
    pub compress: bool,
}
```

In `with_version` (the ctor that builds the struct literal), add:

```rust
            compress: std::env::var("CC_PROXY_COMPRESS").is_ok_and(|v| v == "1"),
```

In `forward()`, immediately after the `body_bytes` read:

```rust
    let body_bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .context("read incoming body")?;

    // Wire-level compression (opt-in): rewrite fat tool_result blocks
    // before disguise. Fail-open — None means forward the original.
    let body_bytes = if state.compress && compress::should_compress(path_only) {
        match compress::rewrite_request_body(&body_bytes) {
            Some(rewritten) => {
                tracing::debug!(
                    before = body_bytes.len(),
                    after = rewritten.len(),
                    "compressed tool_result blocks"
                );
                axum::body::Bytes::from(rewritten)
            }
            None => body_bytes,
        }
    } else {
        body_bytes
    };
```

- [ ] **Step 5: Run the full suite**

Run: `cargo test`
Expected: all tests PASS, including the untouched `passthrough`/`disguise`/`cc_version` suites (proving default-off changes nothing).

- [ ] **Step 6: Lint and commit**

```bash
cargo fmt && cargo clippy -- -D warnings
git add src/compress.rs src/lib.rs tests/compress_e2e.rs
git commit -m "feat(compress): CC_PROXY_COMPRESS-gated wire compression in forward()"
```

---

### Task 4: Live verification + spec status

**Files:**
- Modify: `docs/specs/2026-07-03-mur-compress-design.md` (Status line)

**Interfaces:**
- Consumes: the release binary and the shared `~/.mur/compress` store.

- [ ] **Step 1: Build and run a live proxy against a local echo upstream**

```bash
cargo build --release
python3 -c "
from http.server import BaseHTTPRequestHandler, HTTPServer
class H(BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers['Content-Length']))
        print(f'RECEIVED {len(body)} bytes'); print(body[:400].decode())
        self.send_response(200); self.end_headers(); self.wfile.write(b'{}')
HTTPServer(('127.0.0.1', 9909), H).serve_forever()
" &
CC_PROXY_COMPRESS=1 CC_PROXY_BIND=127.0.0.1:9908 \
  CC_PROXY_UPSTREAM=http://127.0.0.1:9909 CC_PROXY_TOKEN_SOURCE=off \
  ./target/release/cc-proxy serve &
```

- [ ] **Step 2: Send a fat tool_result through it and verify compression + retrieval**

```bash
python3 - <<'EOF'
import json, urllib.request
log = "\n".join(
    f"2026-07-03 12:{i//60%60:02}:{i%60:02} INFO worker-{i%8}: request {100000+i} completed in {10+i%90}ms status OK"
    for i in range(1500))
body = json.dumps({"model": "claude-sonnet-5", "max_tokens": 16, "messages": [
    {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": log}]}]}).encode()
print(f"SENT {len(body)} bytes")
urllib.request.urlopen(urllib.request.Request(
    "http://127.0.0.1:9908/v1/messages", data=body,
    headers={"content-type": "application/json"}))
EOF
```

Expected: echo server prints `RECEIVED <n>` with n well below the SENT size, and the body excerpt contains a `mur_retrieve` note with a hash.

Then, with the hash printed in the received body:

```bash
mur retrieve <hash> | head -3        # recovers the original log lines
mur compress stats                    # shows the compression recorded
```

Expected: original content comes back; stats delta shows one new compression. Kill both background processes afterwards.

- [ ] **Step 3: Update spec status and commit**

In `docs/specs/2026-07-03-mur-compress-design.md` change the Status line to:

```markdown
**Status:** Implemented (env-gated `CC_PROXY_COMPRESS=1`, default off)
```

```bash
git add docs/specs/2026-07-03-mur-compress-design.md
git commit -m "docs: mark mur-compress spec implemented"
```

---

## Rollout note (post-plan, manual)

The installed launchd service does not set `CC_PROXY_COMPRESS`. To enable live: add the env var to the service descriptor (`cc-proxy install` path) or the launchd plist, restart the service, and watch `mur compress stats` for a few sessions before considering a default flip. Verify which mur agents have the mur MCP server before flipping the default (retrieval caveat in the spec).
