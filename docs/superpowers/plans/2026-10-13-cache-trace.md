# Cache trace (prompt-cache diagnostics) — implementation plan

> **Execute with `mur-executing-plans`.** `cargo` works in this checkout
> (`cargo check --tests` finishes in seconds); run the tasks in order, one
> commit per task, on `main`, as the spec commits were.

**Spec:** `docs/specs/2026-10-13-cache-trace-design.md` (commit `6834f1e`).

**Goal:** One DEBUG line, `cache trace`, per Anthropic `/v1/messages`
request, pairing request-side fingerprints, breakpoint layout, divergence
and recency with the response's prompt-cache usage, plus
`scripts/cache-trace-report.sh`, which turns those lines into cold-start
buckets and avoidable write tokens per model.

**Architecture:** A new module `src/cache_trace.rs` parses the exact bytes
reqwest sends (`on_send`), keeps three bounded tables behind one mutex (the
previous request per client+model, prefix recency, tools recency), and
returns an `InflightGuard`. The guard is the only writer of the line: the
handler gives it the status, `UsageTap` gives it the first chunk and the
usage, and `Drop` covers every other path. The report is awk over the line,
with every definition copied from the spec.

**Tech stack:** Rust 2024 edition (let chains are in use), serde_json 1
(no `preserve_order`, so re-serialization sorts keys), lru 0.12.5 (already
a dependency), tracing 0.1, axum 0.8 and httpmock 0.7 / tempfile in tests,
bash + BWK awk (macOS `awk`, no gawk extensions) for the report.

## Global Constraints (copied from the spec — every task includes them)

- Hashes, sizes and offsets only; no block content is ever logged.
- Fail-open: a body that does not parse produces no line and touches no
  table. The trace never changes a request or a response.
- The guard is the only writer of the line. The handler and `UsageTap` only
  call it. Exactly one line per request.
- `Drop` never calls `begun`; a guard dropped before any start was observed
  calls `abandon`, which releases the pending count and leaves
  `readable_until` untouched.
- Only a send with a prefix breakpoint (`ttl!=-`) inserts, refreshes or
  counts as in flight in the prefix table; the tools table is refreshed by
  every send with at least one tool.
- `on_send` runs once per request, before the first upstream attempt; the
  401 retry arm reuses the same guard.
- The hook runs only when provider is Anthropic, the path is `/v1/messages`
  (query allowed, `count_tokens` excluded) and `tracing::enabled!(DEBUG)`.
- The existing `upstream usage` and `compress summary` lines are unchanged.
- Canonical bytes are serde_json's compact re-serialization of a block with
  its `cache_control` member removed (refinement recorded in Task 1): the
  breakpoint layout lives in `bp=`, not in the fingerprints.
- CI runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`
  and `cargo test`; every commit must pass all three.
- Commit style: `area: summary` in lower case with a body, ending with
  `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`.

## File structure

| File | Responsibility |
|---|---|
| `src/cache_trace.rs` (new) | `analyze` (fingerprints, `bp`, TTLs, byte sizes, `ctx_fp`), `Tracker` (three tables), `on_send`, `InflightGuard`, line formatting; unit tests |
| `src/lib.rs` | `pub mod cache_trace`, `AppState.cache_trace`, the hook after `final_body`, status hand-off at the 401 early return and at the tap site, `UpstreamUsage` gains `id`/`model`/`ephemeral_5m`/`ephemeral_1h`, `UsageTap` carries the guard; tap unit tests |
| `docs/specs/2026-10-13-cache-trace-design.md` | one sentence under Fingerprints (cache_control excluded), Status line |
| `tests/cache_trace.rs` (new) | end-to-end: two sends, cwd change, 429 rollback, `count_tokens`, pending-writer race, 401 retry (Assumption 1) |
| `scripts/cache-trace-report.sh` (new, executable) | per-model buckets, avoidable, B1 line, sanity |
| `tests/fixtures/cache-trace/sample.log` (new) | 13 hand-written lines covering every bucket, sub-class and sanity counter |
| `tests/cache_trace_report.rs` (new) | runs the script on the fixture, asserts the computed lines |
| `AGENTS.md`, `README.md` | one bullet each pointing at the line and the script |

---

## Task 0 — Toolchain check

- [ ] `cd /Users/david/Projects/mur-model-gateway && git status --short` prints nothing (clean tree on `main`).
- [ ] `cargo check --tests 2>&1 | tail -1` prints a line starting with `    Finished`.
- [ ] `awk --version 2>/dev/null || awk -W version 2>&1 | head -1` — macOS prints `awk version 2020....`; any BWK or gawk is fine. The report avoids gawk-only functions (`asort`, `length(array)`).

No commit.

---

## Task 1 — `src/cache_trace.rs`: request analysis

Everything the spec's Definitions section derives from one body: per-block
hashes, `tools_fp`/`system_fp`/`prefix_fp`/`ctx_fp`, `bp`, `bp_ttls`, `ttl`,
`ttl_max`, byte sizes, `bp_bytes`, `span_bytes`.

### Step 1.1 — register the module and write the analysis half

- [ ] In `src/lib.rs`, after the line `pub mod cc_version;`, nothing: `cache_trace` sorts before `cc_version`, so insert **before** it:

```rust
pub mod cache_trace;
pub mod cc_version;
```

- [ ] Create `src/cache_trace.rs` with exactly this content:

```rust
//! Prompt-cache diagnostics (spec: docs/specs/2026-10-13-cache-trace-design.md).
//!
//! One `cache trace` DEBUG line per Anthropic `/v1/messages` request. The
//! request side (fingerprints, breakpoint layout, divergence from the previous
//! request, prefix and tools recency, pending writers, byte sizes) is computed
//! by [`on_send`] from the exact bytes handed to reqwest; the response side
//! (usage, id, model, status) is added by the [`InflightGuard`], the only
//! writer of the line. Fail-open: a body that does not parse produces no line
//! and touches no table. Hashes, sizes and offsets only; never block content.

use serde_json::Value;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Slots in the previous-request table (one per client+model).
pub const PREV_SLOTS: usize = 64;
/// Canonical bytes a previous-request slot keeps for byte offsets; above
/// this the slot keeps hashes only and offsets print as `?`.
pub const PREV_BYTES_BUDGET: usize = 512 * 1024;
/// Entries in each recency table.
pub const RECENCY_CAP: usize = 1024;
/// Per-section hash caps; blocks beyond are folded into one tail hash.
pub const TOOLS_HASH_CAP: usize = 1024;
pub const SYSTEM_HASH_CAP: usize = 64;
pub const MSGS_HASH_CAP: usize = 4096;

pub const TTL_5M: Duration = Duration::from_secs(300);
pub const TTL_1H: Duration = Duration::from_secs(3600);

/// `/v1/messages` with an optional query; never `count_tokens`.
pub fn applies(path: &str) -> bool {
    path == "/v1/messages" || path.starts_with("/v1/messages?")
}

/// FNV-1a 64: inline, keyless, stable across restarts.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn fnv1a_seq(hashes: &[u64]) -> u64 {
    let mut bytes = Vec::with_capacity(hashes.len() * 8);
    for h in hashes {
        bytes.extend_from_slice(&h.to_le_bytes());
    }
    fnv1a(&bytes)
}

fn hex(h: u64) -> String {
    format!("{h:016x}")
}

/// Compact re-serialization; keys come out sorted because the crate is
/// built without `preserve_order`.
fn canon(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_default()
}

/// A block without its `cache_control` member: the breakpoint layout is a
/// directive, not content, and lives in `bp=`.
fn without_cc(block: &Value) -> Value {
    match block {
        Value::Object(m) => {
            let mut m = m.clone();
            m.remove("cache_control");
            Value::Object(m)
        }
        other => other.clone(),
    }
}

fn canon_block(block: &Value) -> Vec<u8> {
    canon(&without_cc(block))
}

fn canon_message(m: &Value) -> Vec<u8> {
    let mut m = m.clone();
    if let Some(Value::Array(blocks)) = m.get_mut("content") {
        for b in blocks.iter_mut() {
            *b = without_cc(b);
        }
    }
    canon(&m)
}

fn content(m: &Value) -> &[Value] {
    m.get("content")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Ttl {
    FiveMin,
    OneHour,
}

impl Ttl {
    pub fn as_str(self) -> &'static str {
        match self {
            Ttl::FiveMin => "5m",
            Ttl::OneHour => "1h",
        }
    }

    pub fn duration(self) -> Duration {
        match self {
            Ttl::FiveMin => TTL_5M,
            Ttl::OneHour => TTL_1H,
        }
    }
}

/// The breakpoint on a block, if any: an ephemeral `cache_control` with its
/// effective TTL (an omitted `ttl` is `5m`).
fn breakpoint(block: &Value) -> Option<Ttl> {
    let cc = block.get("cache_control")?.as_object()?;
    if cc.get("type").and_then(Value::as_str) != Some("ephemeral") {
        return None;
    }
    Some(match cc.get("ttl").and_then(Value::as_str) {
        Some("1h") => Ttl::OneHour,
        _ => Ttl::FiveMin,
    })
}

/// `none`, or positions joined by `+`: `last` for the section's last block,
/// otherwise the 0-based index.
fn positions(bps: &[(usize, Ttl)], len: usize) -> String {
    if bps.is_empty() {
        return "none".to_string();
    }
    bps.iter()
        .map(|(i, _)| {
            if *i + 1 == len {
                "last".to_string()
            } else {
                i.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

fn join<T: std::fmt::Display>(items: impl Iterator<Item = T>) -> String {
    let parts: Vec<String> = items.map(|x| x.to_string()).collect();
    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(",")
    }
}

/// Request-side facts about one body (spec: Definitions).
#[derive(Debug, Clone)]
pub struct Analysis {
    pub model: String,
    pub tools_count: usize,
    pub system_blocks: usize,
    pub msgs: usize,
    pub tools_bytes: usize,
    pub system_bytes: usize,
    /// Cumulative canonical bytes at each breakpoint inside tools+system,
    /// document order; parallel to `bp_ttls`.
    pub bp_bytes: Vec<usize>,
    pub prefix_bytes: usize,
    /// Bytes through the last breakpoint anywhere; `None` without one.
    pub span_bytes: Option<usize>,
    pub ctx_fp: u64,
    pub tools_fp: u64,
    pub system_fp: u64,
    pub prefix_fp: u64,
    pub bp: String,
    pub bp_ttls: Vec<Ttl>,
    pub ttl: Option<Ttl>,
    pub ttl_max: Option<Ttl>,
    pub(crate) tools_hashes: Vec<u64>,
    pub(crate) system_hashes: Vec<u64>,
    pub(crate) msgs_hashes: Vec<u64>,
    pub(crate) tools_canon: Vec<Vec<u8>>,
    pub(crate) system_canon: Vec<Vec<u8>>,
}

/// `None` when the body is not a JSON object (fail-open).
pub fn analyze(body: &[u8]) -> Option<Analysis> {
    let root: Value = serde_json::from_slice(body).ok()?;
    let obj = root.as_object()?;
    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("-")
        .to_string();
    let none: Vec<Value> = Vec::new();

    let tools = obj.get("tools").and_then(Value::as_array).unwrap_or(&none);
    let tools_canon: Vec<Vec<u8>> = tools.iter().map(canon_block).collect();
    let tools_hashes: Vec<u64> = tools_canon.iter().map(|b| fnv1a(b)).collect();
    let tools_bps: Vec<(usize, Ttl)> = tools
        .iter()
        .enumerate()
        .filter_map(|(i, t)| breakpoint(t).map(|ttl| (i, ttl)))
        .collect();

    let system: Vec<Value> = match obj.get("system") {
        Some(Value::String(s)) => vec![Value::String(s.clone())],
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    let system_canon: Vec<Vec<u8>> = system.iter().map(canon_block).collect();
    let system_hashes: Vec<u64> = system_canon.iter().map(|b| fnv1a(b)).collect();
    let system_bps: Vec<(usize, Ttl)> = system
        .iter()
        .enumerate()
        .filter_map(|(i, b)| breakpoint(b).map(|ttl| (i, ttl)))
        .collect();

    let messages = obj
        .get("messages")
        .and_then(Value::as_array)
        .unwrap_or(&none);
    let msgs_canon: Vec<Vec<u8>> = messages.iter().map(canon_message).collect();
    let msgs_hashes: Vec<u64> = msgs_canon.iter().map(|b| fnv1a(b)).collect();
    let msgs_len: Vec<usize> = msgs_canon.iter().map(Vec::len).collect();
    let msg_bps: Vec<(usize, Ttl)> = messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            content(m)
                .iter()
                .filter_map(breakpoint)
                .max()
                .map(|ttl| (i, ttl))
        })
        .collect();
    let auto = breakpoint(&root);
    let citations = messages.iter().any(|m| {
        content(m).iter().any(|b| {
            b.get("citations")
                .and_then(|c| c.get("enabled"))
                .and_then(Value::as_bool)
                == Some(true)
        })
    });

    // Settings the API documents as invalidating the system/tools cache
    // without any content change. Absent ones hash as `null`.
    let ctx = serde_json::json!({
        "thinking": obj.get("thinking").cloned().unwrap_or(Value::Null),
        "effort": obj
            .get("output_config")
            .and_then(|o| o.get("effort"))
            .cloned()
            .unwrap_or(Value::Null),
        "speed": obj.get("speed").cloned().unwrap_or(Value::Null),
        "citations": citations,
    });
    let ctx_fp = fnv1a(&canon(&ctx));

    let tools_bytes: usize = tools_canon.iter().map(Vec::len).sum();
    let system_bytes: usize = system_canon.iter().map(Vec::len).sum();
    let mut bp_bytes = Vec::new();
    let mut bp_ttls = Vec::new();
    for (i, ttl) in &tools_bps {
        bp_bytes.push(tools_canon[..=*i].iter().map(Vec::len).sum::<usize>());
        bp_ttls.push(*ttl);
    }
    for (i, ttl) in &system_bps {
        bp_bytes.push(tools_bytes + system_canon[..=*i].iter().map(Vec::len).sum::<usize>());
        bp_ttls.push(*ttl);
    }
    let ttl = bp_ttls.last().copied();

    // The prefix is the block set up to the last breakpoint inside
    // tools+system; all of both when there is none (B1's hypothetical unit).
    let (prefix_tools, prefix_system) = match (system_bps.last(), tools_bps.last()) {
        (Some((i, _)), _) => (tools.len(), *i + 1),
        (None, Some((i, _))) => (*i + 1, 0),
        (None, None) => (tools.len(), system.len()),
    };
    let prefix_bytes = bp_bytes
        .last()
        .copied()
        .unwrap_or(tools_bytes + system_bytes);
    let mut prefix_seq: Vec<u64> = tools_hashes[..prefix_tools].to_vec();
    prefix_seq.extend_from_slice(&system_hashes[..prefix_system]);
    let prefix_fp = fnv1a_seq(&prefix_seq);

    let span_bytes = if auto.is_some() {
        Some(tools_bytes + system_bytes + msgs_len.iter().sum::<usize>())
    } else if let Some((i, _)) = msg_bps.last() {
        Some(tools_bytes + system_bytes + msgs_len[..=*i].iter().sum::<usize>())
    } else if bp_bytes.is_empty() {
        None
    } else {
        Some(prefix_bytes)
    };
    let ttl_max = tools_bps
        .iter()
        .chain(system_bps.iter())
        .chain(msg_bps.iter())
        .map(|(_, t)| *t)
        .chain(auto)
        .max();

    let msgs_pos = {
        let mut parts: Vec<String> = msg_bps
            .iter()
            .map(|(i, _)| match messages.len() - 1 - *i {
                0 => "last".to_string(),
                k => format!("last-{k}"),
            })
            .collect();
        if auto.is_some() {
            parts.push("auto".to_string());
        }
        if parts.is_empty() {
            "none".to_string()
        } else {
            parts.join("+")
        }
    };
    let bp = format!(
        "tools:{},system:{},msgs:{}",
        positions(&tools_bps, tools.len()),
        positions(&system_bps, system.len()),
        msgs_pos
    );

    Some(Analysis {
        model,
        tools_count: tools.len(),
        system_blocks: system.len(),
        msgs: messages.len(),
        tools_bytes,
        system_bytes,
        bp_bytes,
        prefix_bytes,
        span_bytes,
        ctx_fp,
        tools_fp: fnv1a_seq(&tools_hashes),
        system_fp: fnv1a_seq(&system_hashes),
        prefix_fp,
        bp,
        bp_ttls,
        ttl,
        ttl_max,
        tools_hashes,
        system_hashes,
        msgs_hashes,
        tools_canon,
        system_canon,
    })
}

#[cfg(test)]
mod analysis_tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str) -> Value {
        json!({"name": name, "description": format!("{name} a file"), "input_schema": {"type": "object"}})
    }

    fn bp(v: Value, ttl: Option<&str>) -> Value {
        let mut v = v;
        v["cache_control"] = match ttl {
            Some(t) => json!({"type": "ephemeral", "ttl": t}),
            None => json!({"type": "ephemeral"}),
        };
        v
    }

    fn msg(role: &str, text: &str) -> Value {
        json!({"role": role, "content": [{"type": "text", "text": text}]})
    }

    fn body(tools: Vec<Value>, system: Value, messages: Vec<Value>) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "model": "claude-sonnet-5-5", "max_tokens": 8,
            "tools": tools, "system": system, "messages": messages
        }))
        .unwrap()
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn fingerprints_ignore_key_order_and_whitespace_but_not_content() {
        let a = br#"{"model":"m","system":[{"type":"text","text":"hi"}],"messages":[]}"#;
        let b = b"{ \"messages\": [],\n \"system\": [ { \"text\": \"hi\", \"type\": \"text\" } ], \"model\": \"m\" }";
        let (x, y) = (analyze(a).unwrap(), analyze(b).unwrap());
        assert_eq!(x.prefix_fp, y.prefix_fp);
        assert_eq!(x.system_fp, y.system_fp);
        assert_eq!(x.ctx_fp, y.ctx_fp);
        let c = br#"{"model":"m","system":[{"type":"text","text":"hj"}],"messages":[]}"#;
        let z = analyze(c).unwrap();
        assert_ne!(x.system_fp, z.system_fp);
        assert_ne!(x.prefix_fp, z.prefix_fp);
        // a breakpoint is layout, not content
        let d = br#"{"model":"m","system":[{"type":"text","text":"hi","cache_control":{"type":"ephemeral","ttl":"1h"}}],"messages":[]}"#;
        let w = analyze(d).unwrap();
        assert_eq!(x.system_fp, w.system_fp);
        assert_eq!(x.prefix_fp, w.prefix_fp);
        assert_eq!(w.ttl, Some(Ttl::OneHour));
    }

    #[test]
    fn ctx_fp_tracks_the_documented_settings_only() {
        let base = json!({"model": "m", "messages": [msg("user", "x")]});
        let fp = |v: &Value| analyze(&serde_json::to_vec(v).unwrap()).unwrap().ctx_fp;
        let b0 = fp(&base);
        let mut t = base.clone();
        t["thinking"] = json!({"type": "enabled", "budget_tokens": 1024});
        assert_ne!(fp(&t), b0);
        let mut e = base.clone();
        e["output_config"] = json!({"effort": "low"});
        assert_ne!(fp(&e), b0);
        let mut s = base.clone();
        s["speed"] = json!("fast");
        assert_ne!(fp(&s), b0);
        let mut c = base.clone();
        c["messages"] = json!([{"role": "user", "content": [{"type": "document",
            "source": {"type": "text", "media_type": "text/plain", "data": "d"},
            "citations": {"enabled": true}}]}]);
        assert_ne!(fp(&c), b0);
        let mut tc = base.clone();
        tc["tool_choice"] = json!({"type": "auto"});
        assert_eq!(fp(&tc), b0);
        let mut im = base.clone();
        im["messages"] = json!([{"role": "user", "content": [{"type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": "AA=="}}]}]);
        assert_eq!(fp(&im), b0);
    }

    #[test]
    fn breakpoint_layouts() {
        // 1. no cache_control anywhere
        let a = analyze(&body(
            vec![tool("read")],
            json!("sys"),
            vec![msg("user", "hi")],
        ))
        .unwrap();
        assert_eq!(a.bp, "tools:none,system:none,msgs:none");
        assert!(a.bp_ttls.is_empty());
        assert_eq!((a.ttl, a.ttl_max, a.span_bytes), (None, None, None));
        assert!(a.bp_bytes.is_empty());
        assert_eq!(a.prefix_bytes, a.tools_bytes + a.system_bytes);
        assert_eq!((a.tools_count, a.system_blocks, a.msgs), (1, 1, 1));

        // 2. tools only
        let a = analyze(&body(
            vec![tool("read"), bp(tool("grep"), None)],
            json!("sys"),
            vec![msg("user", "hi")],
        ))
        .unwrap();
        assert_eq!(a.bp, "tools:last,system:none,msgs:none");
        assert_eq!(a.bp_ttls, vec![Ttl::FiveMin]);
        assert_eq!(a.ttl, Some(Ttl::FiveMin));
        assert_eq!(a.prefix_bytes, a.tools_bytes);
        assert_eq!(a.bp_bytes, vec![a.tools_bytes]);
        assert_eq!(a.span_bytes, Some(a.tools_bytes));

        // 3. system:last
        let a = analyze(&body(
            vec![tool("read")],
            json!([{"type": "text", "text": "a"}, bp(json!({"type": "text", "text": "b"}), None)]),
            vec![msg("user", "hi")],
        ))
        .unwrap();
        assert_eq!(a.bp, "tools:none,system:last,msgs:none");
        assert_eq!(a.prefix_bytes, a.tools_bytes + a.system_bytes);
        assert_eq!(a.span_bytes, Some(a.prefix_bytes));

        // 4. explicit messages only
        let m =
            json!({"role": "user", "content": [bp(json!({"type": "text", "text": "hi"}), None)]});
        let a = analyze(&body(
            vec![tool("read")],
            json!("sys"),
            vec![msg("user", "a"), msg("assistant", "b"), m],
        ))
        .unwrap();
        assert_eq!(a.bp, "tools:none,system:none,msgs:last");
        assert_eq!((a.ttl, a.ttl_max), (None, Some(Ttl::FiveMin)));
        assert!(a.bp_bytes.is_empty());
        assert_eq!(a.prefix_bytes, a.tools_bytes + a.system_bytes);
        assert!(a.span_bytes.unwrap() > a.prefix_bytes);

        // 5. top-level automatic caching
        let mut v: Value = serde_json::from_slice(&body(
            vec![tool("read")],
            json!("sys"),
            vec![msg("user", "a")],
        ))
        .unwrap();
        v["cache_control"] = json!({"type": "ephemeral", "ttl": "1h"});
        let a = analyze(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!(a.bp, "tools:none,system:none,msgs:auto");
        assert_eq!((a.ttl, a.ttl_max), (None, Some(Ttl::OneHour)));
        assert_eq!(
            a.span_bytes,
            Some(a.tools_bytes + a.system_bytes + canon_message(&msg("user", "a")).len())
        );

        // 6. 1h tools then 5m system, neither on its section's last block
        let a = analyze(&body(
            vec![bp(tool("read"), Some("1h")), tool("grep")],
            json!([bp(json!({"type": "text", "text": "a"}), Some("5m")), {"type": "text", "text": "b"}]),
            vec![msg("user", "hi")],
        ))
        .unwrap();
        assert_eq!(a.bp, "tools:0,system:0,msgs:none");
        assert_eq!(a.bp_ttls, vec![Ttl::OneHour, Ttl::FiveMin]);
        assert_eq!((a.ttl, a.ttl_max), (Some(Ttl::FiveMin), Some(Ttl::OneHour)));
        let read_len = canon_block(&tool("read")).len();
        let sys_a = canon_block(&json!({"type": "text", "text": "a"})).len();
        assert_eq!(a.bp_bytes, vec![read_len, a.tools_bytes + sys_a]);
        assert_eq!(a.prefix_bytes, a.tools_bytes + sys_a);
        assert_eq!(a.span_bytes, Some(a.prefix_bytes));
    }

    #[test]
    fn prefix_fp_covers_only_the_cached_blocks() {
        let sys = |third: &str| json!([{"type": "text", "text": "a"}, bp(json!({"type": "text", "text": "b"}), None), {"type": "text", "text": third}]);
        let x = analyze(&body(vec![tool("read")], sys("c"), vec![])).unwrap();
        let y = analyze(&body(vec![tool("read")], sys("d"), vec![])).unwrap();
        assert_eq!(x.bp, "tools:none,system:1,msgs:none");
        assert_eq!(x.prefix_fp, y.prefix_fp);
        assert_ne!(x.system_fp, y.system_fp);
        assert_eq!(x.prefix_bytes, y.prefix_bytes);
        assert!(x.prefix_bytes < x.tools_bytes + x.system_bytes);

        // tools:last,system:none → the prefix is the tools alone
        let t = analyze(&body(
            vec![bp(tool("read"), None)],
            json!("sys one"),
            vec![],
        ))
        .unwrap();
        let u = analyze(&body(
            vec![bp(tool("read"), None)],
            json!("sys two"),
            vec![],
        ))
        .unwrap();
        assert_eq!(t.prefix_fp, u.prefix_fp);
        assert_eq!(t.prefix_bytes, t.tools_bytes);

        // the same two blocks under `last` and under an index are one unit
        let two =
            json!([{"type": "text", "text": "a"}, bp(json!({"type": "text", "text": "b"}), None)]);
        let three = json!([{"type": "text", "text": "a"}, bp(json!({"type": "text", "text": "b"}), None), {"type": "text", "text": "c"}]);
        let p = analyze(&body(vec![], two, vec![])).unwrap();
        let q = analyze(&body(vec![], three, vec![])).unwrap();
        assert_eq!(p.bp, "tools:none,system:last,msgs:none");
        assert_eq!(q.bp, "tools:none,system:1,msgs:none");
        assert_eq!(p.prefix_fp, q.prefix_fp);
    }

    #[test]
    fn garbage_is_none() {
        assert!(analyze(b"not json").is_none());
        assert!(analyze(b"[1,2]").is_none());
        assert!(analyze(b"{}").is_some());
    }

    #[test]
    fn applies_to_messages_only() {
        assert!(applies("/v1/messages"));
        assert!(applies("/v1/messages?beta=true"));
        assert!(!applies("/v1/messages/count_tokens"));
        assert!(!applies("/v1/complete"));
    }
}
```

- [ ] `cargo test --lib cache_trace::analysis_tests 2>&1 | tail -3` prints `test result: ok. 7 passed; 0 failed` (the exact line is `test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; N filtered out; ...`).
- [ ] `cargo clippy --all-targets -- -D warnings 2>&1 | tail -1` prints a line starting with `    Finished`. If clippy flags `join` or `hex` as dead code, that is expected until Task 2 uses them: add `#[allow(dead_code)]` on those two functions **for this commit only** and remove the attribute in Task 2.
- [ ] `cargo fmt`.

### Step 1.2 — record the cache_control refinement in the spec

- [ ] In `docs/specs/2026-10-13-cache-trace-design.md`, under `### Fingerprints`, replace the first paragraph's opening sentence

```
Per-block hash: FNV-1a 64 over the block's canonical bytes, where canonical
means the block re-serialized by `serde_json::to_vec` (compact, object keys
sorted because the crate is built without `preserve_order`). Consequences,
by design:
```

with

```
Per-block hash: FNV-1a 64 over the block's canonical bytes, where canonical
means the block, with its `cache_control` member removed, re-serialized by
`serde_json::to_vec` (compact, object keys sorted because the crate is built
without `preserve_order`). The breakpoint is a directive, not content: the
same blocks with a moved breakpoint or a changed TTL are one cache unit, and
`bp=` records the layout. Consequences, by design:
```

### Step 1.3 — commit

```
cache_trace: analyse a request body for fingerprints and breakpoint layout

New module, request side only: per-block FNV-1a over canonical bytes
(serde_json re-serialization, cache_control stripped), tools/system/
prefix/ctx fingerprints, bp= layout, bp_ttls/ttl/ttl_max, byte sizes,
bp_bytes and span_bytes, as the cache trace spec defines them. Nothing
calls it yet.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
```

**Interfaces — Produces (used by Task 2):**
`pub fn analyze(body: &[u8]) -> Option<Analysis>`; `pub struct Analysis` with the public fields listed above plus `pub(crate) tools_hashes / system_hashes / msgs_hashes: Vec<u64>` and `pub(crate) tools_canon / system_canon: Vec<Vec<u8>>`; `pub enum Ttl { FiveMin, OneHour }` with `as_str()` and `duration()`; `pub fn applies(&str) -> bool`; `pub fn fnv1a(&[u8]) -> u64`; private `fnv1a_seq`, `hex`, `join`, `canon_block`, `canon_message`; the constants `PREV_SLOTS`, `PREV_BYTES_BUDGET`, `RECENCY_CAP`, `TOOLS_HASH_CAP`, `SYSTEM_HASH_CAP`, `MSGS_HASH_CAP`.

---

## Task 2 — `src/cache_trace.rs`: tables, `on_send`, `InflightGuard`, the line

### Step 2.1 — extend `UpstreamUsage` (the guard formats it)

- [ ] In `src/lib.rs`, replace

```rust
/// Prompt-cache accounting from an Anthropic response's `usage` block.
#[derive(Debug, Default, PartialEq, Eq)]
struct UpstreamUsage {
    input_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
}

impl UpstreamUsage {
    /// `container` is the object holding `usage`: the response root for a
    /// JSON body, `message` inside the `message_start` frame for SSE.
    fn from_container(container: &serde_json::Value) -> Option<Self> {
        let u = container.get("usage")?.as_object()?;
        let n = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        Some(Self {
            input_tokens: n("input_tokens"),
            cache_read_input_tokens: n("cache_read_input_tokens"),
            cache_creation_input_tokens: n("cache_creation_input_tokens"),
        })
    }
```

with

```rust
/// Prompt-cache accounting from an Anthropic response's `usage` block, plus
/// the response's `id` and `model` and the 5m/1h split of the write, which
/// only the `cache trace` line prints.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct UpstreamUsage {
    pub(crate) input_tokens: u64,
    pub(crate) cache_read_input_tokens: u64,
    pub(crate) cache_creation_input_tokens: u64,
    pub(crate) id: Option<String>,
    pub(crate) model: Option<String>,
    /// `usage.cache_creation.ephemeral_5m_input_tokens`; `None` when the
    /// `cache_creation` object is absent (a field missing inside it is 0).
    pub(crate) ephemeral_5m: Option<u64>,
    pub(crate) ephemeral_1h: Option<u64>,
}

impl UpstreamUsage {
    /// `container` is the object holding `usage`: the response root for a
    /// JSON body, `message` inside the `message_start` frame for SSE.
    fn from_container(container: &serde_json::Value) -> Option<Self> {
        let u = container.get("usage")?.as_object()?;
        let n = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        let s = |k: &str| {
            container
                .get(k)
                .and_then(|x| x.as_str())
                .map(str::to_string)
        };
        let cc = u.get("cache_creation").and_then(|x| x.as_object());
        let split = |k: &str| cc.map(|c| c.get(k).and_then(|x| x.as_u64()).unwrap_or(0));
        Some(Self {
            input_tokens: n("input_tokens"),
            cache_read_input_tokens: n("cache_read_input_tokens"),
            cache_creation_input_tokens: n("cache_creation_input_tokens"),
            id: s("id"),
            model: s("model"),
            ephemeral_5m: split("ephemeral_5m_input_tokens"),
            ephemeral_1h: split("ephemeral_1h_input_tokens"),
        })
    }
```

- [ ] In `mod usage_tap_tests` (near the end of `src/lib.rs`), make the three struct literals compile and pin the new fields. Replace the module's `START` constant and the first and last tests with:

```rust
    const START: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"m","model":"claude-sonnet-5-5","usage":{"input_tokens":12,"cache_creation_input_tokens":300,"cache_read_input_tokens":45000,"cache_creation":{"ephemeral_5m_input_tokens":100,"ephemeral_1h_input_tokens":200},"output_tokens":1}}}

"#;

    #[test]
    fn sse_message_start_yields_usage() {
        match usage_from_sse(START.as_bytes()) {
            SseScan::Found(u) => assert_eq!(
                u,
                UpstreamUsage {
                    input_tokens: 12,
                    cache_read_input_tokens: 45000,
                    cache_creation_input_tokens: 300,
                    id: Some("m".into()),
                    model: Some("claude-sonnet-5-5".into()),
                    ephemeral_5m: Some(100),
                    ephemeral_1h: Some(200),
                }
            ),
            _ => panic!("expected usage"),
        }
    }
```

and

```rust
    #[test]
    fn json_body_usage_is_read_from_root() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"id":"m","content":[],"usage":{"input_tokens":5,"cache_read_input_tokens":7}}"#,
        )
        .unwrap();
        assert_eq!(
            UpstreamUsage::from_container(&v).unwrap(),
            UpstreamUsage {
                input_tokens: 5,
                cache_read_input_tokens: 7,
                cache_creation_input_tokens: 0,
                id: Some("m".into()),
                ..Default::default()
            }
        );
        assert!(UpstreamUsage::from_container(&serde_json::json!({"id": "m"})).is_none());
        // the split object present but a side missing: that side is 0, not absent
        let v: serde_json::Value = serde_json::from_str(
            r#"{"usage":{"input_tokens":1,"cache_creation":{"ephemeral_1h_input_tokens":9}}}"#,
        )
        .unwrap();
        let u = UpstreamUsage::from_container(&v).unwrap();
        assert_eq!((u.ephemeral_5m, u.ephemeral_1h), (Some(0), Some(9)));
    }
```

(The middle two tests, `sse_frame_split_across_chunks_is_pending_until_terminated` and `sse_first_frame_not_message_start_is_absent`, are unchanged.)

- [ ] `cargo test --lib usage_tap_tests 2>&1 | tail -3` prints `test result: ok. 4 passed`.

### Step 2.2 — append the tracker half to `src/cache_trace.rs`

- [ ] Remove any `#[allow(dead_code)]` added in Task 1. Append at the end of the file:

```rust
// ── Tracker: previous request, prefix recency, tools recency ────────────

/// A hash sequence capped at `cap` entries; anything beyond is folded into
/// one tail hash so a difference there still shows.
#[derive(Debug)]
struct Seq {
    head: Vec<u64>,
    tail: Option<u64>,
    total: usize,
}

impl Seq {
    fn new(hashes: &[u64], cap: usize) -> Self {
        if hashes.len() <= cap {
            Seq {
                head: hashes.to_vec(),
                tail: None,
                total: hashes.len(),
            }
        } else {
            Seq {
                head: hashes[..cap].to_vec(),
                tail: Some(fnv1a_seq(&hashes[cap..])),
                total: hashes.len(),
            }
        }
    }
}

enum SeqDiff {
    Same,
    /// First differing block index, inside the kept head.
    At(usize),
    /// The difference lies beyond the hash cap, in the folded tail.
    Tail,
    /// All shared blocks match; the counts differ.
    Count {
        shared: usize,
        cur_more: bool,
    },
}

fn seq_diff(prev: &Seq, cur: &Seq, cap: usize) -> SeqDiff {
    let n = prev.head.len().min(cur.head.len());
    if let Some(i) = (0..n).find(|&i| prev.head[i] != cur.head[i]) {
        return SeqDiff::At(i);
    }
    if prev.total != cur.total {
        let shared = prev.total.min(cur.total);
        if shared >= cap {
            return SeqDiff::Tail;
        }
        return SeqDiff::Count {
            shared,
            cur_more: cur.total > prev.total,
        };
    }
    if prev.tail != cur.tail {
        return SeqDiff::Tail;
    }
    SeqDiff::Same
}

struct Canon {
    tools: Vec<Vec<u8>>,
    system: Vec<Vec<u8>>,
}

struct PrevSlot {
    ctx_fp: u64,
    tools: Seq,
    system: Seq,
    msgs: Seq,
    /// Kept only while tools+system fit `PREV_BYTES_BUDGET`.
    canon: Option<Canon>,
}

fn first_diff(a: &[u8], b: &[u8]) -> usize {
    a.iter()
        .zip(b)
        .position(|(x, y)| x != y)
        .unwrap_or(a.len().min(b.len()))
}

fn section(
    name: &str,
    cap: usize,
    diff: SeqDiff,
    prev: Option<&[Vec<u8>]>,
    cur: &[Vec<u8>],
) -> Option<String> {
    Some(match diff {
        SeqDiff::Same => return None,
        SeqDiff::Tail => format!("{name}:{cap}+"),
        SeqDiff::Count { shared, cur_more } => {
            format!("{name}:{shared}@{}", if cur_more { '+' } else { '-' })
        }
        SeqDiff::At(i) => match prev {
            Some(p) if i < p.len() && i < cur.len() => {
                format!("{name}:{i}@{}", first_diff(&p[i], &cur[i]))
            }
            _ => format!("{name}:{i}@?"),
        },
    })
}

fn section_msgs(diff: SeqDiff) -> Option<String> {
    Some(match diff {
        SeqDiff::Same => return None,
        SeqDiff::Tail => format!("msgs:{MSGS_HASH_CAP}+"),
        SeqDiff::Count { shared, .. } => format!("msgs:{shared}"),
        SeqDiff::At(i) => format!("msgs:{i}"),
    })
}

/// Where this request first differs from the previous one by the same
/// client+model: ctx, tools, system, messages, in cache-hierarchy order.
fn diverged_from(prev: &PrevSlot, a: &Analysis, tools: &Seq, system: &Seq, msgs: &Seq) -> String {
    if prev.ctx_fp != a.ctx_fp {
        return "ctx".to_string();
    }
    let (pt, ps) = match &prev.canon {
        Some(c) => (Some(c.tools.as_slice()), Some(c.system.as_slice())),
        None => (None, None),
    };
    section(
        "tools",
        TOOLS_HASH_CAP,
        seq_diff(&prev.tools, tools, TOOLS_HASH_CAP),
        pt,
        &a.tools_canon,
    )
    .or_else(|| {
        section(
            "system",
            SYSTEM_HASH_CAP,
            seq_diff(&prev.system, system, SYSTEM_HASH_CAP),
            ps,
            &a.system_canon,
        )
    })
    .or_else(|| section_msgs(seq_diff(&prev.msgs, msgs, MSGS_HASH_CAP)))
    .unwrap_or_else(|| "none".to_string())
}

/// One upstream cache entry as the gateway models it.
#[derive(Debug)]
struct Entry {
    last_seq: u64,
    last_sent: Instant,
    /// One level of history for the failure rollback.
    prev: Option<(u64, Instant)>,
    /// Raised when a refreshing send's response begins; never by a failure.
    readable_until: Option<Instant>,
    /// Refreshing sends whose response has not begun.
    pending: u32,
}

type PrefixKey = (String, u64, u64);
type ToolsKey = (String, u64, u64);

struct Inner {
    prev: lru::LruCache<(String, String), PrevSlot>,
    prefix: lru::LruCache<PrefixKey, Entry>,
    tools: lru::LruCache<ToolsKey, Entry>,
    seq: u64,
}

/// The three tables behind one mutex. Lives in `AppState`.
pub struct Tracker {
    inner: Mutex<Inner>,
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Tracker {
    pub fn new() -> Self {
        let cap = |n: usize| NonZeroUsize::new(n).expect("non-zero cap");
        Self {
            inner: Mutex::new(Inner {
                prev: lru::LruCache::new(cap(PREV_SLOTS)),
                prefix: lru::LruCache::new(cap(RECENCY_CAP)),
                tools: lru::LruCache::new(cap(RECENCY_CAP)),
                seq: 0,
            }),
        }
    }

    fn lock_inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn settle(
        &self,
        seq: u64,
        prefix: Option<&(PrefixKey, Instant)>,
        tools: Option<&(ToolsKey, Instant)>,
        success: bool,
    ) {
        let mut inner = self.lock_inner();
        if let Some((k, exp)) = prefix {
            settle_entry(&mut inner.prefix, k, seq, *exp, success);
        }
        if let Some((k, exp)) = tools {
            settle_entry(&mut inner.tools, k, seq, *exp, success);
        }
    }
}

struct Recency {
    seen_ago_ms: Option<u128>,
    ttl_left_ms: Option<i128>,
    pending: u32,
}

impl Recency {
    const NONE: Recency = Recency {
        seen_ago_ms: None,
        ttl_left_ms: None,
        pending: 0,
    };
}

fn read_entry(e: Option<&Entry>, now: Instant) -> Recency {
    let Some(e) = e else {
        return Recency::NONE;
    };
    Recency {
        seen_ago_ms: Some(now.saturating_duration_since(e.last_sent).as_millis()),
        ttl_left_ms: e.readable_until.map(|r| {
            if r >= now {
                r.saturating_duration_since(now).as_millis() as i128
            } else {
                -(now.saturating_duration_since(r).as_millis() as i128)
            }
        }),
        pending: e.pending,
    }
}

/// Make room for one more entry: evict the least recently used entry with
/// no pending writer, rotating pending ones to the front. `false` when every
/// entry is pending; the insert is refused and the request still traced.
fn make_room<K: std::hash::Hash + Eq>(table: &mut lru::LruCache<K, Entry>) -> bool {
    if table.len() < table.cap().get() {
        return true;
    }
    for _ in 0..table.cap().get() {
        if table.peek_lru().is_none() {
            return true;
        }
        let evictable = matches!(table.peek_lru(), Some((_, e)) if e.pending == 0);
        if evictable {
            table.pop_lru();
            return true;
        }
        if let Some((k, v)) = table.pop_lru() {
            table.push(k, v);
        }
    }
    false
}

/// Update half of read-classify-update for one table. `true` when the entry
/// now holds this send; `false` when the table refused it.
fn touch<K: std::hash::Hash + Eq + Clone>(
    table: &mut lru::LruCache<K, Entry>,
    key: &K,
    seq: u64,
    now: Instant,
) -> bool {
    if let Some(e) = table.get_mut(key) {
        e.prev = Some((e.last_seq, e.last_sent));
        e.last_seq = seq;
        e.last_sent = now;
        e.pending += 1;
        return true;
    }
    if !make_room(table) {
        return false;
    }
    table.put(
        key.clone(),
        Entry {
            last_seq: seq,
            last_sent: now,
            prev: None,
            readable_until: None,
            pending: 1,
        },
    );
    true
}

/// A send's outcome. Success raises `readable_until`; failure rolls the
/// timestamp back one level when nothing was sent since, and removes a
/// key whose only send failed.
fn settle_entry<K: std::hash::Hash + Eq>(
    table: &mut lru::LruCache<K, Entry>,
    key: &K,
    seq: u64,
    expiry: Instant,
    success: bool,
) {
    let mut remove = false;
    if let Some(e) = table.peek_mut(key) {
        e.pending = e.pending.saturating_sub(1);
        if success {
            e.readable_until = Some(e.readable_until.map_or(expiry, |r| r.max(expiry)));
        } else if e.last_seq == seq {
            match e.prev.take() {
                Some((ps, pt)) => {
                    e.last_seq = ps;
                    e.last_sent = pt;
                }
                None => remove = e.readable_until.is_none() && e.pending == 0,
            }
        }
    }
    if remove {
        table.pop(key);
    }
}

/// Request-side half of the line, fixed at send time.
#[derive(Debug, Clone)]
pub struct SendTrace {
    pub client: String,
    pub compress_on: bool,
    pub analysis: Analysis,
    pub diverged: String,
    pub prefix_seen_ago_ms: Option<u128>,
    pub prefix_ttl_left_ms: Option<i128>,
    pub tools_seen_ago_ms: Option<u128>,
    pub tools_ttl_left_ms: Option<i128>,
    pub inflight_same_prefix: u32,
    pub inflight_same_tools: u32,
}

/// Parse `body`, compare with the previous request, read and update the
/// recency tables under one lock, and hand back the guard that owns the
/// line. `now` is a parameter so tests drive the clock. `None` when the body
/// is not a JSON object: nothing is logged and nothing is touched.
pub fn on_send(
    tracker: &Arc<Tracker>,
    client: &str,
    body: &[u8],
    compress_on: bool,
    now: Instant,
) -> Option<InflightGuard> {
    let mut a = analyze(body)?;
    let tools_seq = Seq::new(&a.tools_hashes, TOOLS_HASH_CAP);
    let system_seq = Seq::new(&a.system_hashes, SYSTEM_HASH_CAP);
    let msgs_seq = Seq::new(&a.msgs_hashes, MSGS_HASH_CAP);
    let prefix_key: PrefixKey = (a.model.clone(), a.ctx_fp, a.prefix_fp);
    let tools_key: ToolsKey = (a.model.clone(), a.ctx_fp, a.tools_fp);
    let b1_ttl = a.ttl_max.unwrap_or(Ttl::FiveMin);

    let mut inner = tracker.lock_inner();
    inner.seq += 1;
    let seq = inner.seq;

    let slot_key = (client.to_string(), a.model.clone());
    let diverged = match inner.prev.get(&slot_key) {
        Some(prev) => diverged_from(prev, &a, &tools_seq, &system_seq, &msgs_seq),
        None => "first".to_string(),
    };
    let canon = (a.tools_bytes + a.system_bytes <= PREV_BYTES_BUDGET).then(|| Canon {
        tools: std::mem::take(&mut a.tools_canon),
        system: std::mem::take(&mut a.system_canon),
    });
    inner.prev.put(
        slot_key,
        PrevSlot {
            ctx_fp: a.ctx_fp,
            tools: tools_seq,
            system: system_seq,
            msgs: msgs_seq,
            canon,
        },
    );

    let mut prefix = read_entry(inner.prefix.peek(&prefix_key), now);
    let prefix_hold = match a.ttl {
        Some(ttl) if touch(&mut inner.prefix, &prefix_key, seq, now) => {
            Some((prefix_key, now + ttl.duration()))
        }
        Some(_) => {
            prefix = Recency::NONE;
            None
        }
        None => None,
    };
    let mut tools = read_entry(inner.tools.peek(&tools_key), now);
    let tools_hold = if a.tools_count == 0 {
        None
    } else if touch(&mut inner.tools, &tools_key, seq, now) {
        Some((tools_key, now + b1_ttl.duration()))
    } else {
        tools = Recency::NONE;
        None
    };
    drop(inner);

    Some(InflightGuard {
        tracker: Arc::clone(tracker),
        seq,
        prefix: prefix_hold,
        tools: tools_hold,
        trace: SendTrace {
            client: client.to_string(),
            compress_on,
            diverged,
            prefix_seen_ago_ms: prefix.seen_ago_ms,
            prefix_ttl_left_ms: prefix.ttl_left_ms,
            inflight_same_prefix: prefix.pending,
            tools_seen_ago_ms: tools.seen_ago_ms,
            tools_ttl_left_ms: tools.ttl_left_ms,
            inflight_same_tools: tools.pending,
            analysis: a,
        },
        status: None,
        streaming: false,
        started: false,
        emitted: false,
    })
}

/// Owns the `cache trace` line for one request. The handler reports the
/// status, `UsageTap` reports the first chunk and the usage; `Drop` covers
/// every other path. Exactly one line per guard.
pub struct InflightGuard {
    tracker: Arc<Tracker>,
    seq: u64,
    prefix: Option<(PrefixKey, Instant)>,
    tools: Option<(ToolsKey, Instant)>,
    trace: SendTrace,
    status: Option<u16>,
    streaming: bool,
    started: bool,
    emitted: bool,
}

impl InflightGuard {
    pub fn trace(&self) -> &SendTrace {
        &self.trace
    }

    /// The final upstream status. A failure settles the tables at once; a
    /// non-streaming success counts as begun (its headers come after
    /// generation); a streaming success waits for `begun`.
    pub fn set_status(&mut self, status: u16, streaming: bool) {
        self.status = Some(status);
        self.streaming = streaming;
        if !(200..300).contains(&status) {
            self.finish(false);
        } else if !streaming {
            self.finish(true);
        }
    }

    /// An observed start: releases the pending count and raises
    /// `readable_until`. Idempotent.
    pub fn begun(&mut self) {
        self.finish(true);
    }

    /// No start observed: releases the pending count only. Idempotent.
    pub fn abandon(&mut self) {
        self.finish(false);
    }

    fn finish(&mut self, success: bool) {
        if self.started {
            return;
        }
        self.started = true;
        self.tracker
            .settle(self.seq, self.prefix.as_ref(), self.tools.as_ref(), success);
    }

    /// Write the line, once. Later calls are no-ops.
    pub(crate) fn complete(&mut self, usage: Option<&crate::UpstreamUsage>) {
        if self.emitted {
            return;
        }
        self.emitted = true;
        tracing::debug!("cache trace {}", self.line(usage));
    }

    /// The line's `key=value` body, in the spec's field order.
    pub(crate) fn line(&self, usage: Option<&crate::UpstreamUsage>) -> String {
        fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
            v.map_or_else(|| "-".to_string(), |x| x.to_string())
        }
        let t = &self.trace;
        let a = &t.analysis;
        let model = usage
            .and_then(|u| u.model.clone())
            .unwrap_or_else(|| a.model.clone());
        let req_id = usage
            .and_then(|u| u.id.clone())
            .unwrap_or_else(|| "-".to_string());
        let (input, read, creation, c5, c1) = match usage {
            Some(u) => (
                u.input_tokens.to_string(),
                u.cache_read_input_tokens.to_string(),
                u.cache_creation_input_tokens.to_string(),
                opt(u.ephemeral_5m),
                opt(u.ephemeral_1h),
            ),
            None => ("-".into(), "-".into(), "-".into(), "-".into(), "-".into()),
        };
        format!(
            "client={} model={} req_id={} status={} streaming={} compress_on={} \
             tools_count={} system_blocks={} msgs={} tools_bytes={} system_bytes={} \
             bp_bytes={} prefix_bytes={} span_bytes={} \
             ctx_fp={} tools_fp={} system_fp={} prefix_fp={} bp={} bp_ttls={} ttl={} ttl_max={} \
             input={} read={} creation={} creation_5m={} creation_1h={} \
             diverged={} prefix_seen_ago_ms={} prefix_ttl_left_ms={} \
             tools_seen_ago_ms={} tools_ttl_left_ms={} inflight_same_prefix={} inflight_same_tools={}",
            t.client,
            model,
            req_id,
            opt(self.status),
            self.streaming,
            u8::from(t.compress_on),
            a.tools_count,
            a.system_blocks,
            a.msgs,
            a.tools_bytes,
            a.system_bytes,
            join(a.bp_bytes.iter()),
            a.prefix_bytes,
            opt(a.span_bytes),
            hex(a.ctx_fp),
            hex(a.tools_fp),
            hex(a.system_fp),
            hex(a.prefix_fp),
            a.bp,
            join(a.bp_ttls.iter().map(|x| x.as_str())),
            opt(a.ttl.map(Ttl::as_str)),
            opt(a.ttl_max.map(Ttl::as_str)),
            input,
            read,
            creation,
            c5,
            c1,
            t.diverged,
            opt(t.prefix_seen_ago_ms),
            opt(t.prefix_ttl_left_ms),
            opt(t.tools_seen_ago_ms),
            opt(t.tools_ttl_left_ms),
            t.inflight_same_prefix,
            t.inflight_same_tools,
        )
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if !self.started {
            self.abandon();
        }
        if !self.emitted {
            self.complete(None);
        }
    }
}

#[cfg(test)]
mod tracker_tests {
    use super::*;
    use serde_json::json;

    /// One tool, two system blocks, the breakpoint on the second (the
    /// runtime's shape). `ttl` goes on that breakpoint.
    fn body(cwd: &str, turns: usize, ttl: Option<&str>) -> Vec<u8> {
        let cc = match ttl {
            Some(t) => json!({"type": "ephemeral", "ttl": t}),
            None => json!({"type": "ephemeral"}),
        };
        let messages: Vec<Value> = (0..turns)
            .map(|i| {
                json!({"role": if i % 2 == 0 { "user" } else { "assistant" },
                       "content": [{"type": "text", "text": format!("turn {i}")}]})
            })
            .collect();
        serde_json::to_vec(&json!({
            "model": "claude-sonnet-5-5", "max_tokens": 8,
            "tools": [{"name": "read", "description": "Read a file", "input_schema": {"type": "object"}}],
            "system": [
                {"type": "text", "text": "You are careful."},
                {"type": "text", "text": format!("Working directory: {cwd}"), "cache_control": cc}
            ],
            "messages": messages
        }))
        .unwrap()
    }

    /// Same tools and system, no breakpoint on either; one on the last
    /// message instead.
    fn body_no_prefix_bp(turns: usize) -> Vec<u8> {
        let mut v: Value = serde_json::from_slice(&body("/w", turns, None)).unwrap();
        v["system"][1]
            .as_object_mut()
            .unwrap()
            .remove("cache_control");
        v["messages"][turns - 1]["content"][0]["cache_control"] = json!({"type": "ephemeral"});
        serde_json::to_vec(&v).unwrap()
    }

    fn send(t: &Arc<Tracker>, client: &str, body: &[u8], at: Instant) -> InflightGuard {
        on_send(t, client, body, false, at).expect("body parses")
    }

    fn usage(read: u64, creation: u64) -> crate::UpstreamUsage {
        crate::UpstreamUsage {
            input_tokens: 10,
            cache_read_input_tokens: read,
            cache_creation_input_tokens: creation,
            id: Some("msg_1".into()),
            model: Some("claude-sonnet-5-5".into()),
            ephemeral_5m: Some(creation),
            ephemeral_1h: Some(0),
        }
    }

    fn field<'a>(line: &'a str, key: &str) -> &'a str {
        let p = format!("{key}=");
        line.split(' ')
            .find_map(|tok| tok.strip_prefix(p.as_str()))
            .unwrap_or_else(|| panic!("no {key} in {line}"))
    }

    #[test]
    fn first_send_then_same_prefix() {
        let t = Arc::new(Tracker::new());
        let t0 = Instant::now();
        let mut g1 = send(&t, "c", &body("/w", 1, None), t0);
        let l1 = g1.line(None);
        assert_eq!(field(&l1, "diverged"), "first");
        assert_eq!(field(&l1, "prefix_seen_ago_ms"), "-");
        assert_eq!(field(&l1, "prefix_ttl_left_ms"), "-");
        assert_eq!(field(&l1, "inflight_same_prefix"), "0");
        assert_eq!(field(&l1, "tools_ttl_left_ms"), "-");
        assert_eq!(field(&l1, "compress_on"), "0");
        assert_eq!(field(&l1, "bp"), "tools:none,system:last,msgs:none");
        assert_eq!(field(&l1, "ttl"), "5m");
        assert_eq!(field(&l1, "status"), "-");
        assert_eq!(field(&l1, "read"), "-");
        g1.set_status(200, true);
        g1.begun();
        let g2 = send(
            &t,
            "c",
            &body("/w", 3, None),
            t0 + Duration::from_millis(1500),
        );
        let l2 = g2.line(Some(&usage(0, 500)));
        assert_eq!(field(&l2, "diverged"), "msgs:1");
        assert_eq!(field(&l2, "prefix_seen_ago_ms"), "1500");
        assert_eq!(field(&l2, "prefix_ttl_left_ms"), "298500");
        assert_eq!(field(&l2, "inflight_same_prefix"), "0");
        assert_eq!(field(&l2, "tools_seen_ago_ms"), "1500");
        assert_eq!(field(&l2, "tools_ttl_left_ms"), "298500");
        assert_eq!(field(&l2, "read"), "0");
        assert_eq!(field(&l2, "creation"), "500");
        assert_eq!(field(&l2, "creation_5m"), "500");
        assert_eq!(field(&l2, "creation_1h"), "0");
        assert_eq!(field(&l2, "req_id"), "msg_1");
        assert_eq!(field(&l2, "model"), "claude-sonnet-5-5");
    }

    #[test]
    fn compress_on_is_printed() {
        let t = Arc::new(Tracker::new());
        let g = on_send(&t, "c", &body("/w", 1, None), true, Instant::now()).unwrap();
        assert_eq!(field(&g.line(None), "compress_on"), "1");
    }

    #[test]
    fn a_different_cwd_is_a_new_prefix_with_recent_tools() {
        let t = Arc::new(Tracker::new());
        let t0 = Instant::now();
        let mut g1 = send(&t, "c", &body("/w/one", 1, None), t0);
        g1.set_status(200, true);
        g1.begun();
        let g2 = send(
            &t,
            "c",
            &body("/w/two", 1, None),
            t0 + Duration::from_millis(100),
        );
        let l2 = g2.line(None);
        // canonical block 1 is {"text":"Working directory: /w/one","type":"text"};
        // byte 31 is the first that differs
        assert_eq!(field(&l2, "diverged"), "system:1@31");
        assert_eq!(field(&l2, "prefix_seen_ago_ms"), "-");
        assert_eq!(field(&l2, "prefix_ttl_left_ms"), "-");
        assert_eq!(field(&l2, "tools_seen_ago_ms"), "100");
        assert_eq!(field(&l2, "tools_ttl_left_ms"), "299900");
        assert_eq!(field(&l2, "inflight_same_tools"), "0");
    }

    #[test]
    fn interleaved_tracks_keep_their_own_recency() {
        // A, B, A: the third send is A again and sees the first A, not B.
        let t = Arc::new(Tracker::new());
        let t0 = Instant::now();
        let mut a1 = send(&t, "c", &body("/a", 1, None), t0);
        a1.set_status(200, false);
        let mut b1 = send(
            &t,
            "c",
            &body("/b", 1, None),
            t0 + Duration::from_millis(100),
        );
        b1.set_status(200, false);
        let a2 = send(
            &t,
            "c",
            &body("/a", 1, None),
            t0 + Duration::from_millis(250),
        );
        let l = a2.line(None);
        assert_eq!(field(&l, "prefix_seen_ago_ms"), "250");
        assert_eq!(field(&l, "prefix_ttl_left_ms"), "299750");
        assert_eq!(field(&l, "diverged"), "system:1@29");
    }

    #[test]
    fn pending_writer_is_a_race_until_its_response_begins() {
        let t = Arc::new(Tracker::new());
        let t0 = Instant::now();
        let mut g1 = send(&t, "c", &body("/w", 1, None), t0);
        let g2 = send(
            &t,
            "c",
            &body("/w", 1, None),
            t0 + Duration::from_millis(50),
        );
        let l2 = g2.line(None);
        assert_eq!(field(&l2, "inflight_same_prefix"), "1");
        assert_eq!(field(&l2, "prefix_seen_ago_ms"), "50");
        assert_eq!(field(&l2, "prefix_ttl_left_ms"), "-");
        assert_eq!(field(&l2, "inflight_same_tools"), "1");
        assert_eq!(field(&l2, "tools_ttl_left_ms"), "-");
        g1.set_status(200, true);
        g1.begun();
        let g3 = send(
            &t,
            "c",
            &body("/w", 1, None),
            t0 + Duration::from_millis(80),
        );
        let l3 = g3.line(None);
        assert_eq!(field(&l3, "inflight_same_prefix"), "1"); // g2 still pending
        assert_eq!(field(&l3, "prefix_ttl_left_ms"), "299920");
        assert_eq!(field(&l3, "tools_ttl_left_ms"), "299920");
        drop(g2); // never began: abandon releases its count, readable_until untouched
        let g4 = send(
            &t,
            "c",
            &body("/w", 1, None),
            t0 + Duration::from_millis(100),
        );
        let l4 = g4.line(None);
        assert_eq!(field(&l4, "inflight_same_prefix"), "1"); // g3 pending
        assert_eq!(field(&l4, "prefix_ttl_left_ms"), "299900");
    }

    #[test]
    fn ttl_left_goes_negative_by_the_writers_ttl() {
        let t = Arc::new(Tracker::new());
        let t0 = Instant::now();
        let mut g1 = send(&t, "c", &body("/w", 1, Some("5m")), t0);
        g1.set_status(200, false);
        // same content, the asking request carries 1h: the 5m entry is dead
        let g2 = send(
            &t,
            "c",
            &body("/w", 1, Some("1h")),
            t0 + Duration::from_secs(400),
        );
        let l2 = g2.line(None);
        assert_eq!(field(&l2, "diverged"), "none");
        assert_eq!(field(&l2, "ttl"), "1h");
        assert_eq!(field(&l2, "prefix_seen_ago_ms"), "400000");
        assert_eq!(field(&l2, "prefix_ttl_left_ms"), "-100000");
    }

    #[test]
    fn a_send_without_a_prefix_breakpoint_reads_but_never_writes() {
        let t = Arc::new(Tracker::new());
        let t0 = Instant::now();
        let mut g1 = send(&t, "c", &body_no_prefix_bp(1), t0);
        g1.set_status(200, false);
        let g2 = send(
            &t,
            "c",
            &body_no_prefix_bp(1),
            t0 + Duration::from_millis(10),
        );
        let l2 = g2.line(None);
        assert_eq!(field(&l2, "ttl"), "-");
        assert_eq!(field(&l2, "bp"), "tools:none,system:none,msgs:last");
        assert_eq!(field(&l2, "prefix_seen_ago_ms"), "-");
        assert_eq!(field(&l2, "prefix_ttl_left_ms"), "-");
        assert_eq!(field(&l2, "inflight_same_prefix"), "0");
        // the tools table is B1's hypothetical: every send refreshes it
        assert_eq!(field(&l2, "tools_seen_ago_ms"), "10");
        assert_eq!(field(&l2, "tools_ttl_left_ms"), "299990");
        // a breakpoint-carrying send of the same content is then readable
        // to a breakpoint-less one
        let mut g3 = send(
            &t,
            "c",
            &body("/w", 1, None),
            t0 + Duration::from_millis(20),
        );
        g3.set_status(200, false);
        let g4 = send(
            &t,
            "c",
            &body_no_prefix_bp(1),
            t0 + Duration::from_millis(30),
        );
        assert_eq!(field(&g4.line(None), "prefix_ttl_left_ms"), "299990");
    }

    #[test]
    fn failed_sends_roll_back_seen_ago_and_never_become_readable() {
        let t = Arc::new(Tracker::new());
        let t0 = Instant::now();
        let mut ok = send(&t, "c", &body("/w", 1, None), t0);
        ok.set_status(200, false);
        let mut f1 = send(
            &t,
            "c",
            &body("/w", 1, None),
            t0 + Duration::from_millis(100),
        );
        f1.set_status(429, false);
        drop(f1);
        let g = send(
            &t,
            "c",
            &body("/w", 1, None),
            t0 + Duration::from_millis(150),
        );
        let l = g.line(None);
        assert_eq!(field(&l, "prefix_seen_ago_ms"), "150");
        assert_eq!(field(&l, "prefix_ttl_left_ms"), "299850");
        assert_eq!(field(&l, "inflight_same_prefix"), "0");
        drop(g);

        // consecutive failures: F2 sees the value from before F1, and F1's
        // timestamp is erased
        let t = Arc::new(Tracker::new());
        let mut ok = send(&t, "c", &body("/w", 1, None), t0);
        ok.set_status(200, false);
        let mut f1 = send(
            &t,
            "c",
            &body("/w", 1, None),
            t0 + Duration::from_millis(100),
        );
        f1.set_status(429, false);
        drop(f1);
        let mut f2 = send(
            &t,
            "c",
            &body("/w", 1, None),
            t0 + Duration::from_millis(200),
        );
        assert_eq!(field(&f2.line(None), "prefix_seen_ago_ms"), "200");
        f2.set_status(529, false);
        drop(f2);
        let g = send(
            &t,
            "c",
            &body("/w", 1, None),
            t0 + Duration::from_millis(300),
        );
        assert_eq!(field(&g.line(None), "prefix_seen_ago_ms"), "300");

        // a key whose only send failed is removed: the next send is first
        let t = Arc::new(Tracker::new());
        let mut f = send(&t, "c", &body("/w", 1, None), t0);
        f.set_status(401, false);
        drop(f);
        let g = send(&t, "c", &body("/w", 1, None), t0 + Duration::from_millis(5));
        let l = g.line(None);
        assert_eq!(field(&l, "prefix_seen_ago_ms"), "-");
        assert_eq!(field(&l, "tools_seen_ago_ms"), "-");
        assert_eq!(field(&l, "diverged"), "none");
    }

    #[test]
    fn documented_boundary_a_pending_failure_behind_a_later_send() {
        let t = Arc::new(Tracker::new());
        let t0 = Instant::now();
        let mut f1 = send(&t, "c", &body("/w", 1, None), t0);
        let mut s2 = send(
            &t,
            "c",
            &body("/w", 1, None),
            t0 + Duration::from_millis(100),
        );
        assert_eq!(field(&s2.line(None), "inflight_same_prefix"), "1");
        f1.set_status(429, false);
        drop(f1); // last_seq is s2's: no rollback
        s2.set_status(200, true);
        s2.begun();
        let g = send(
            &t,
            "c",
            &body("/w", 1, None),
            t0 + Duration::from_millis(300),
        );
        let l = g.line(None);
        assert_eq!(field(&l, "prefix_seen_ago_ms"), "200");
        assert_eq!(field(&l, "prefix_ttl_left_ms"), "299800");
        assert_eq!(field(&l, "inflight_same_prefix"), "0");
    }

    #[test]
    fn eviction_skips_pending_entries_and_refuses_when_none_is_evictable() {
        let t = Arc::new(Tracker::new());
        let t0 = Instant::now();
        let mut held: Vec<InflightGuard> = (0..RECENCY_CAP)
            .map(|i| send(&t, "c", &body(&format!("/w/{i}"), 1, None), t0))
            .collect();
        // every entry is pending: a new key is refused, the request still traced
        let g = send(
            &t,
            "c",
            &body("/w/extra", 1, None),
            t0 + Duration::from_millis(1),
        );
        let l = g.line(None);
        assert_eq!(field(&l, "prefix_seen_ago_ms"), "-");
        assert_eq!(field(&l, "inflight_same_prefix"), "0");
        // one begins: it is evictable, the next new key gets in
        held[0].set_status(200, false);
        let g2 = send(
            &t,
            "c",
            &body("/w/extra2", 1, None),
            t0 + Duration::from_millis(2),
        );
        assert_eq!(field(&g2.line(None), "prefix_seen_ago_ms"), "-");
        let g3 = send(
            &t,
            "c",
            &body("/w/extra2", 1, None),
            t0 + Duration::from_millis(3),
        );
        assert_eq!(field(&g3.line(None), "prefix_seen_ago_ms"), "1");
        // a pending entry was rotated, never evicted
        let g4 = send(
            &t,
            "c",
            &body("/w/1", 1, None),
            t0 + Duration::from_millis(4),
        );
        assert_eq!(field(&g4.line(None), "inflight_same_prefix"), "1");
        drop(held);
    }

    #[test]
    fn differences_past_the_hash_cap_show_as_a_folded_tail() {
        let t = Arc::new(Tracker::new());
        let t0 = Instant::now();
        let big = |last: &str| {
            let mut v: Value =
                serde_json::from_slice(&body("/w", MSGS_HASH_CAP + 2, None)).unwrap();
            v["messages"][MSGS_HASH_CAP + 1]["content"][0]["text"] = json!(last);
            serde_json::to_vec(&v).unwrap()
        };
        let mut g1 = send(&t, "c", &big("x"), t0);
        g1.set_status(200, false);
        let g2 = send(&t, "c", &big("y"), t0);
        assert_eq!(field(&g2.line(None), "diverged"), "msgs:4096+");
        let g3 = send(&t, "c", &body("/w", MSGS_HASH_CAP + 3, None), t0);
        assert_eq!(field(&g3.line(None), "diverged"), "msgs:4096+");
    }

    #[test]
    fn offsets_are_unknown_past_the_byte_budget() {
        let t = Arc::new(Tracker::new());
        let t0 = Instant::now();
        let fat = |cwd: &str| {
            let mut v: Value = serde_json::from_slice(&body(cwd, 1, None)).unwrap();
            v["system"][0]["text"] = json!("x".repeat(PREV_BYTES_BUDGET + 1));
            serde_json::to_vec(&v).unwrap()
        };
        let mut g1 = send(&t, "c", &fat("/a"), t0);
        g1.set_status(200, false);
        let g2 = send(&t, "c", &fat("/b"), t0);
        assert_eq!(field(&g2.line(None), "diverged"), "system:1@?");
    }

    #[test]
    fn a_settings_change_diverges_at_ctx_and_is_a_new_key() {
        let t = Arc::new(Tracker::new());
        let t0 = Instant::now();
        let mut g1 = send(&t, "c", &body("/w", 1, None), t0);
        g1.set_status(200, false);
        let mut v: Value = serde_json::from_slice(&body("/w", 1, None)).unwrap();
        v["thinking"] = json!({"type": "enabled", "budget_tokens": 2048});
        let g2 = send(
            &t,
            "c",
            &serde_json::to_vec(&v).unwrap(),
            t0 + Duration::from_millis(10),
        );
        let l = g2.line(None);
        assert_eq!(field(&l, "diverged"), "ctx");
        assert_eq!(field(&l, "prefix_seen_ago_ms"), "-");
        assert_eq!(field(&l, "tools_seen_ago_ms"), "-");
    }
}
```

- [ ] `cargo test --lib cache_trace 2>&1 | tail -3` prints `test result: ok. 20 passed; 0 failed` (7 analysis + 13 tracker).
- [ ] `cargo clippy --all-targets -- -D warnings 2>&1 | tail -1` prints a line starting with `    Finished`.
- [ ] `cargo fmt`.

### Step 2.3 — commit

```
cache_trace: recency tables, on_send and the guard that owns the line

Three lru tables behind one mutex: the previous request per
client+model (diverged=, hashes capped per section, canonical bytes
kept only under 512 KB), prefix recency keyed on (model, ctx_fp,
prefix_fp) and refreshed by breakpoint-carrying sends only, and tools
recency keyed on (model, ctx_fp, tools_fp) for B1's hypothetical entry.
An entry becomes readable when a send's response begins; a failed send
rolls its timestamp back one level and never raises readable_until.
The InflightGuard formats the cache trace line and is its only writer.
UpstreamUsage gains id, model and the 5m/1h write split.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
```

**Interfaces — Consumes (from Task 1):** `analyze`, `Analysis`, `Ttl`, the constants.
**Interfaces — Produces (used by Tasks 3 and 4):**
`pub struct Tracker` with `Tracker::new()` and `Default`;
`pub fn on_send(tracker: &Arc<Tracker>, client: &str, body: &[u8], compress_on: bool, now: Instant) -> Option<InflightGuard>`;
`pub struct InflightGuard` with `pub fn trace(&self) -> &SendTrace`, `pub fn set_status(&mut self, status: u16, streaming: bool)`, `pub fn begun(&mut self)`, `pub fn abandon(&mut self)`, `pub(crate) fn complete(&mut self, usage: Option<&crate::UpstreamUsage>)`, `pub(crate) fn line(&self, usage: Option<&crate::UpstreamUsage>) -> String`, and a `Drop` that calls `abandon` when no start was observed and `complete(None)` when no line was written;
`pub struct SendTrace` (Clone) with the fields above;
`pub(crate) struct UpstreamUsage` in `src/lib.rs` with the seven fields above.

---

## Task 3 — `src/lib.rs`: wire the hook, the status hand-off and the tap

### Step 3.1 — `AppState` owns a `Tracker`

- [ ] In `pub struct AppState`, after the `compress_body_tokens: bool,` field and its doc comment, add:

```rust
    /// Prompt-cache diagnostics tables (spec: docs/specs/2026-10-13). Always
    /// on at DEBUG for Anthropic `/v1/messages`; `Arc` because `AppState` is
    /// cloned per request and the tables must be shared.
    pub cache_trace: Arc<cache_trace::Tracker>,
```

- [ ] In `AppState::with_version`, after the line `compress_body_tokens: std::env::var("MUR_MODEL_GATEWAY_COMPRESS_BODY_TOKENS")` … `.is_ok_and(|v| v == "1"),` add:

```rust
            cache_trace: Arc::new(cache_trace::Tracker::new()),
```

### Step 3.2 — the hook, right after `final_body`

- [ ] In `forward()`, find

```rust
    let final_body: Vec<u8> = if let Some(ver) = cc_version.as_deref() {
        tracing::debug!(path = %path_only, cc_version = %ver, "applying disguise");
        disguise::inject_billing_prefix(&body_bytes, ver)?
    } else {
        body_bytes.to_vec()
    };
```

and insert immediately after it:

```rust
    // Prompt-cache diagnostics (spec: docs/specs/2026-10-13-cache-trace-design.md).
    // Reads the exact bytes reqwest sends: nothing between here and
    // `upstream_req.body(final_body)` touches the body, only headers. The
    // guard is the only writer of the `cache trace` line; this handler and
    // `UsageTap` only call it. Runs once per request: the 401 retry below
    // re-sends byte-identical bytes and reuses this guard.
    let mut cache_guard: Option<cache_trace::InflightGuard> = if provider == Provider::Anthropic
        && cache_trace::applies(path_only)
        && tracing::enabled!(tracing::Level::DEBUG)
    {
        let compress_on = state.compress && compress::should_compress(path_only, provider);
        cache_trace::on_send(
            &state.cache_trace,
            &client,
            &final_body,
            compress_on,
            std::time::Instant::now(),
        )
    } else {
        None
    };
```

### Step 3.3 — the 401 early return reports its status

- [ ] Find the block that ends with

```rust
        tracing::warn!(
            expired,
            retried,
            "returning 401: the credential this gateway attached was rejected"
        );
        return Ok(anthropic_auth_error_response(
```

and insert between the `tracing::warn!(...);` statement and the `return`:

```rust
        if let Some(g) = cache_guard.as_mut() {
            g.set_status(status.as_u16(), false);
            g.complete(None);
        }
```

### Step 3.4 — the tap site

- [ ] Replace

```rust
    let tap = matches!(provider, Provider::Anthropic) && status.is_success();
    let stream = UsageTap::new(upstream_resp.bytes_stream(), tap, streaming, client);
```

with

```rust
    let tap = matches!(provider, Provider::Anthropic) && status.is_success();
    // Non-2xx: the guard writes its line now (usage `-`) and is dropped
    // here. 2xx: it rides inside the tap, which reports the first chunk
    // and the usage.
    if let Some(g) = cache_guard.as_mut() {
        g.set_status(status.as_u16(), streaming);
        if !status.is_success() {
            g.complete(None);
        }
    }
    let cache_guard = cache_guard.filter(|_| status.is_success());
    let stream = UsageTap::new(
        upstream_resp.bytes_stream(),
        tap,
        streaming,
        client,
        cache_guard,
    );
```

### Step 3.5 — `UsageTap` carries the guard

- [ ] Replace the struct, `new`, `observe` and `eof`:

```rust
struct UsageTap<S> {
    inner: Pin<Box<S>>,
    buf: Vec<u8>,
    streaming: bool,
    done: bool,
    client: String,
    /// Owner of the `cache trace` line; `None` outside Anthropic
    /// `/v1/messages` or below DEBUG. Dropped with the tap, which is when
    /// the body finishes or the client goes away.
    guard: Option<cache_trace::InflightGuard>,
}

impl<S> UsageTap<S> {
    fn new(
        inner: S,
        enabled: bool,
        streaming: bool,
        client: String,
        guard: Option<cache_trace::InflightGuard>,
    ) -> Self {
        Self {
            inner: Box::pin(inner),
            buf: Vec::new(),
            streaming,
            done: !enabled,
            client,
            guard,
        }
    }

    fn finish(&mut self) {
        self.done = true;
        self.buf = Vec::new();
    }

    fn observe(&mut self, chunk: &[u8]) {
        // The first chunk is the response beginning: the cache entry this
        // request wrote (or read) is readable from here on. Idempotent.
        if let Some(g) = self.guard.as_mut() {
            g.begun();
        }
        let cap = if self.streaming {
            USAGE_TAP_SSE_CAP
        } else {
            USAGE_TAP_JSON_CAP
        };
        if self.buf.len() + chunk.len() > cap {
            self.finish();
            return;
        }
        self.buf.extend_from_slice(chunk);
        if self.streaming {
            match usage_from_sse(&self.buf) {
                SseScan::Pending => {}
                SseScan::Found(u) => {
                    if let Some(g) = self.guard.as_mut() {
                        g.complete(Some(&u));
                    }
                    u.log(true, &self.client);
                    self.finish();
                }
                SseScan::Absent => self.finish(),
            }
        }
    }

    fn eof(&mut self) {
        if !self.streaming
            && let Some(u) = serde_json::from_slice::<serde_json::Value>(&self.buf)
                .ok()
                .and_then(|v| UpstreamUsage::from_container(&v))
        {
            if let Some(g) = self.guard.as_mut() {
                g.complete(Some(&u));
            }
            u.log(false, &self.client);
        }
        self.finish();
    }
}
```

(`poll_next` and `hold_permit_through` are unchanged.)

### Step 3.6 — tap tests for the guard hand-off

- [ ] Append to `mod usage_tap_tests` in `src/lib.rs`:

```rust
    const TRACE_BODY: &[u8] = br#"{"model":"m","tools":[{"name":"t","input_schema":{"type":"object"}}],"system":[{"type":"text","text":"s","cache_control":{"type":"ephemeral"}}],"messages":[]}"#;

    #[tokio::test]
    async fn tap_marks_the_entry_readable_on_the_first_chunk_and_hands_over_usage() {
        use futures_util::StreamExt;
        let tracker = Arc::new(cache_trace::Tracker::new());
        let now = std::time::Instant::now();
        let mut guard = cache_trace::on_send(&tracker, "t", TRACE_BODY, false, now).unwrap();
        guard.set_status(200, true);
        // before any chunk: a second send sees a pending writer, nothing readable
        let peek = cache_trace::on_send(&tracker, "t", TRACE_BODY, false, now).unwrap();
        let p = peek.trace().clone();
        assert_eq!((p.inflight_same_prefix, p.prefix_ttl_left_ms), (1, None));
        drop(peek);
        let chunks: Vec<Result<axum::body::Bytes, reqwest::Error>> =
            vec![Ok(axum::body::Bytes::from(START))];
        let mut tap = UsageTap::new(
            futures_util::stream::iter(chunks),
            true,
            true,
            "t".into(),
            Some(guard),
        );
        assert!(tap.next().await.is_some());
        let after = cache_trace::on_send(&tracker, "t", TRACE_BODY, false, now).unwrap();
        let a = after.trace().clone();
        assert_eq!(
            a.inflight_same_prefix, 0,
            "begun released the pending count"
        );
        assert!(
            a.prefix_ttl_left_ms.is_some_and(|ms| ms > 299_000),
            "readable for the writer's 5m: {:?}",
            a.prefix_ttl_left_ms
        );
        assert!(tap.next().await.is_none());
    }

    #[tokio::test]
    async fn a_tap_dropped_before_its_first_chunk_abandons_without_readability() {
        let tracker = Arc::new(cache_trace::Tracker::new());
        let now = std::time::Instant::now();
        let mut guard = cache_trace::on_send(&tracker, "t", TRACE_BODY, false, now).unwrap();
        guard.set_status(200, true);
        let chunks: Vec<Result<axum::body::Bytes, reqwest::Error>> = Vec::new();
        let tap = UsageTap::new(
            futures_util::stream::iter(chunks),
            true,
            true,
            "t".into(),
            Some(guard),
        );
        drop(tap);
        let next = cache_trace::on_send(&tracker, "t", TRACE_BODY, false, now).unwrap();
        let n = next.trace().clone();
        assert_eq!(
            (
                n.inflight_same_prefix,
                n.prefix_ttl_left_ms,
                n.prefix_seen_ago_ms
            ),
            (0, None, None),
            "the only send never began and was abandoned, so its key is gone"
        );
    }
```

- [ ] `cargo test 2>&1 | grep -E "^test result|FAILED|panicked" ` prints only `test result: ok.` lines (one per test binary; the lib binary shows `26 passed` or more: 20 cache_trace + 6 usage_tap + the existing lib tests are counted together, so read the line for `0 failed`).
- [ ] `cargo clippy --all-targets -- -D warnings 2>&1 | tail -1` prints a line starting with `    Finished`.
- [ ] `cargo fmt`.
- [ ] Smoke it on the real binary: `cargo run -q -- --help >/dev/null && echo ok` prints `ok` (the binary still starts).

### Step 3.7 — commit

```
log: one cache trace line per Anthropic /v1/messages request

AppState owns a cache_trace::Tracker. forward() calls on_send on the
exact bytes handed to reqwest (after compression and the billing
prefix) when the provider is Anthropic, the path is /v1/messages and
DEBUG is enabled; the guard gets the final status at the 401 early
return or at the tap site, and rides inside UsageTap for 2xx so the
first chunk marks the entry readable and message_start / the JSON body
supplies the usage. The upstream usage and compress summary lines are
unchanged.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
```

**Interfaces — Consumes (from Task 2):** `cache_trace::{Tracker, on_send, InflightGuard, applies}`, `UpstreamUsage` fields.
**Interfaces — Produces (used by Task 4):** the `cache trace` line in `proxy.log` for every Anthropic `/v1/messages` request at DEBUG, with the field order of `InflightGuard::line`; `AppState.cache_trace`.

---

## Task 4 — `tests/cache_trace.rs`: end to end

The log capture is the one from `tests/client_disconnect.rs`: one global
subscriber for the test binary, so every test spawns its own gateway (own
tables) and tags its requests with a unique user-agent product token, which
becomes `client=`. The upstream is a hand-rolled axum service because the
race tests need control over when the first SSE chunk leaves.

### Step 4.1 — write the file

- [ ] Create `tests/cache_trace.rs`:

```rust
//! End-to-end proof of the `cache trace` line (spec:
//! docs/specs/2026-10-13-cache-trace-design.md): one line per Anthropic
//! `/v1/messages` request pairing request-side fingerprints and recency with
//! the response's usage; nothing for `count_tokens`; the 429 rollback; the
//! pending-writer race; and Assumption 1 (the 401 retry re-sends identical
//! bytes and leaves exactly one line).
//!
//! Each test spawns its own gateway (own tables) and tags its requests with a
//! unique user-agent, because the log capture is shared by the test binary.

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use mur_model_gateway::cc_version::{VersionCache, VersionStrategy};
use mur_model_gateway::{AppState, TokenSource, build_router};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// Every log line this test process emits, ANSI off. One global subscriber:
/// the gateway runs on spawned tasks, which a thread-local default would miss.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn logs() -> &'static Captured {
    static LOGS: OnceLock<Captured> = OnceLock::new();
    LOGS.get_or_init(|| {
        let cap = Captured::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(cap.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish();
        tracing::subscriber::set_global_default(sub).expect("one global subscriber");
        cap
    })
}

fn lines(cap: &Captured) -> Vec<String> {
    let raw = cap.0.lock().unwrap().clone();
    String::from_utf8_lossy(&raw)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Exact field match: `key=` followed by the token.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}=");
    line.split_whitespace()
        .find_map(|tok| tok.strip_prefix(prefix.as_str()))
}

/// Every `cache trace` line for one client tag, in emission order.
fn traces(tag: &str) -> Vec<String> {
    lines(logs())
        .into_iter()
        .filter(|l| l.contains("cache trace ") && field(l, "client") == Some(tag))
        .collect()
}

async fn wait_for_traces(tag: &str, n: usize) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let t = traces(tag);
        if t.len() >= n || tokio::time::Instant::now() >= deadline {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn num(line: &str, key: &str) -> i64 {
    field(line, key)
        .unwrap_or_else(|| panic!("no {key} in {line}"))
        .parse()
        .unwrap_or_else(|_| panic!("{key} is not a number in {line}"))
}

const START: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_t1\",\"model\":\"claude-sonnet-5-5\",\"usage\":{\"input_tokens\":12,\"cache_creation_input_tokens\":300,\"cache_read_input_tokens\":0,\"cache_creation\":{\"ephemeral_5m_input_tokens\":300,\"ephemeral_1h_input_tokens\":0}}}}\n\n";
const STOP: &str = "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

const STALE: &str = "sk-ant-oat01-stale";
const FRESH: &str = "sk-ant-oat01-fresh";

/// A Claude Code credentials blob; `expires_in_ms` is relative to now.
fn write_credential(path: &std::path::Path, token: &str, expires_in_ms: i64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    std::fs::write(
        path,
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"{token}","refreshToken":"r","expiresAt":{}}}}}"#,
            now + expires_in_ms
        ),
    )
    .unwrap();
}

#[derive(Clone, Default)]
struct Upstream {
    hits: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
    /// When set, the first hit answers 401 and rewrites this credentials
    /// file, standing in for another terminal refreshing the token.
    rewrite: Option<PathBuf>,
}

/// A two-frame SSE body: `message_start` after `before_start`, then
/// `message_stop` after a further `after_start`.
fn sse(before_start: Duration, after_start: Duration) -> Response {
    let frames = vec![(before_start, START), (after_start, STOP)];
    let stream = futures_util::stream::unfold(frames.into_iter(), |mut it| async move {
        let (delay, text) = it.next()?;
        tokio::time::sleep(delay).await;
        Some((Ok::<_, std::io::Error>(axum::body::Bytes::from(text)), it))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

async fn messages(
    State(up): State<Upstream>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let n = up.hits.fetch_add(1, Ordering::SeqCst);
    up.bodies.lock().unwrap().push(body.to_vec());
    if let Some(p) = &up.rewrite
        && n == 0
    {
        write_credential(p, FRESH, 60 * 60 * 1000);
        return (
            StatusCode::UNAUTHORIZED,
            r#"{"error":{"message":"expired"}}"#,
        )
            .into_response();
    }
    let mode = headers
        .get("x-test-mode")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("sse");
    match mode {
        "429" => (
            StatusCode::TOO_MANY_REQUESTS,
            [("content-type", "application/json")],
            r#"{"error":{"type":"rate_limit_error"}}"#,
        )
            .into_response(),
        "slow-start" => sse(Duration::from_millis(1500), Duration::ZERO),
        "slow-tail" => sse(Duration::ZERO, Duration::from_millis(1500)),
        _ => sse(Duration::ZERO, Duration::ZERO),
    }
}

async fn count_tokens() -> Response {
    (
        StatusCode::OK,
        [("content-type", "application/json")],
        r#"{"input_tokens":5}"#,
    )
        .into_response()
}

async fn spawn_upstream(up: Upstream) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .with_state(up);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn pinned_version() -> Arc<VersionCache> {
    Arc::new(VersionCache::new(VersionStrategy::Static(
        "9.9.9".to_string(),
    )))
}

async fn spawn_gateway(upstream: &str, source: TokenSource) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = AppState::with_version(
        upstream,
        "https://o.invalid",
        "https://g.invalid",
        source,
        pinned_version(),
    )
    .unwrap();
    let app = build_router(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(20)).await;
    addr.to_string()
}

/// One tool, two system blocks with the breakpoint on the second (the
/// runtime's shape), `turns` alternating messages, no message breakpoint.
fn body(cwd: &str, turns: usize) -> String {
    let messages: Vec<serde_json::Value> = (0..turns)
        .map(|i| {
            serde_json::json!({
                "role": if i % 2 == 0 { "user" } else { "assistant" },
                "content": [{"type": "text", "text": format!("turn {i}")}]
            })
        })
        .collect();
    serde_json::json!({
        "model": "claude-sonnet-5-5",
        "max_tokens": 8,
        "stream": true,
        "tools": [{"name": "read", "description": "Read a file",
                   "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}}],
        "system": [
            {"type": "text", "text": "You are a careful assistant."},
            {"type": "text", "text": format!("Working directory: {cwd}"),
             "cache_control": {"type": "ephemeral"}}
        ],
        "messages": messages
    })
    .to_string()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

async fn call(gw: &str, tag: &str, mode: &str, body: String) -> reqwest::Response {
    client()
        .post(format!("http://{gw}/v1/messages"))
        .header("content-type", "application/json")
        .header("user-agent", format!("{tag}/1.0"))
        .header("x-test-mode", mode)
        .body(body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn the_same_prefix_twice_is_recent_and_diverges_in_messages() {
    let _ = logs();
    let gw = spawn_gateway(
        &spawn_upstream(Upstream::default()).await,
        TokenSource::Disabled,
    )
    .await;
    let r = call(&gw, "t1", "sse", body("/w/one", 1)).await;
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    let r = call(&gw, "t1", "sse", body("/w/one", 3)).await;
    r.bytes().await.unwrap();

    let t = wait_for_traces("t1", 2).await;
    assert_eq!(t.len(), 2, "{t:#?}");
    let (a, b) = (&t[0], &t[1]);
    assert_eq!(field(a, "diverged"), Some("first"));
    assert_eq!(field(a, "prefix_seen_ago_ms"), Some("-"));
    assert_eq!(field(a, "prefix_ttl_left_ms"), Some("-"));
    assert_eq!(field(a, "tools_ttl_left_ms"), Some("-"));
    assert_eq!(field(a, "bp"), Some("tools:none,system:last,msgs:none"));
    assert_eq!(field(a, "bp_ttls"), Some("5m"));
    assert_eq!(field(a, "ttl"), Some("5m"));
    assert_eq!(field(a, "ttl_max"), Some("5m"));
    assert_eq!(field(a, "status"), Some("200"));
    assert_eq!(field(a, "streaming"), Some("true"));
    assert_eq!(field(a, "compress_on"), Some("0"));
    assert_eq!(field(a, "req_id"), Some("msg_t1"));
    assert_eq!(field(a, "model"), Some("claude-sonnet-5-5"));
    assert_eq!(field(a, "input"), Some("12"));
    assert_eq!(field(a, "read"), Some("0"));
    assert_eq!(field(a, "creation"), Some("300"));
    assert_eq!(field(a, "creation_5m"), Some("300"));
    assert_eq!(field(a, "creation_1h"), Some("0"));
    assert_eq!(field(a, "msgs"), Some("1"));
    assert_eq!(field(a, "tools_count"), Some("1"));
    assert_eq!(field(a, "system_blocks"), Some("2"));
    assert_eq!(field(b, "diverged"), Some("msgs:1"));
    assert_eq!(field(b, "msgs"), Some("3"));
    assert_eq!(field(b, "inflight_same_prefix"), Some("0"));
    let seen = num(b, "prefix_seen_ago_ms");
    let left = num(b, "prefix_ttl_left_ms");
    assert!(
        (0..300_000).contains(&seen) && (1..=300_000).contains(&left),
        "{b}"
    );
    assert!(num(b, "tools_ttl_left_ms") > 0, "{b}");
    assert_eq!(field(a, "prefix_fp"), field(b, "prefix_fp"));
}

#[tokio::test]
async fn a_different_cwd_is_a_new_prefix_with_recent_tools() {
    let _ = logs();
    let gw = spawn_gateway(
        &spawn_upstream(Upstream::default()).await,
        TokenSource::Disabled,
    )
    .await;
    call(&gw, "t2", "sse", body("/w/one", 1))
        .await
        .bytes()
        .await
        .unwrap();
    call(&gw, "t2", "sse", body("/w/two", 1))
        .await
        .bytes()
        .await
        .unwrap();

    let t = wait_for_traces("t2", 2).await;
    assert_eq!(t.len(), 2, "{t:#?}");
    let b = &t[1];
    // canonical system block 1 is {"text":"Working directory: /w/one","type":"text"}:
    // byte 31 is the first that differs
    assert_eq!(field(b, "diverged"), Some("system:1@31"));
    assert_eq!(field(b, "prefix_seen_ago_ms"), Some("-"));
    assert_eq!(field(b, "prefix_ttl_left_ms"), Some("-"));
    assert!(num(b, "tools_seen_ago_ms") >= 0, "{b}");
    assert!(num(b, "tools_ttl_left_ms") > 0, "{b}");
    assert_ne!(field(&t[0], "prefix_fp"), field(b, "prefix_fp"));
    assert_eq!(field(&t[0], "tools_fp"), field(b, "tools_fp"));
}

#[tokio::test]
async fn a_failed_send_is_rolled_back() {
    let _ = logs();
    let gw = spawn_gateway(
        &spawn_upstream(Upstream::default()).await,
        TokenSource::Disabled,
    )
    .await;
    call(&gw, "t3", "sse", body("/w/f", 1))
        .await
        .bytes()
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let r = call(&gw, "t3", "429", body("/w/f", 1)).await;
    assert_eq!(r.status(), 429);
    call(&gw, "t3", "sse", body("/w/f", 1))
        .await
        .bytes()
        .await
        .unwrap();

    let t = wait_for_traces("t3", 3).await;
    assert_eq!(t.len(), 3, "{t:#?}");
    let failed = &t[1];
    assert_eq!(field(failed, "status"), Some("429"));
    assert_eq!(field(failed, "streaming"), Some("false"));
    assert_eq!(field(failed, "read"), Some("-"));
    assert_eq!(field(failed, "creation"), Some("-"));
    assert_eq!(field(failed, "req_id"), Some("-"));
    assert_eq!(field(failed, "model"), Some("claude-sonnet-5-5"));
    let third = &t[2];
    // rolled back to the first send, which was at least 300 ms ago; without
    // the rollback this would be the few ms since the 429
    assert!(num(third, "prefix_seen_ago_ms") >= 300, "{third}");
    assert!(num(third, "prefix_ttl_left_ms") > 0, "{third}");
    assert_eq!(field(third, "inflight_same_prefix"), Some("0"));
}

#[tokio::test]
async fn count_tokens_leaves_no_line() {
    let _ = logs();
    let gw = spawn_gateway(
        &spawn_upstream(Upstream::default()).await,
        TokenSource::Disabled,
    )
    .await;
    let r = client()
        .post(format!("http://{gw}/v1/messages/count_tokens"))
        .header("content-type", "application/json")
        .header("user-agent", "t4/1.0")
        .body(body("/w/c", 1))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    // a real request afterwards proves the capture is live for this tag
    call(&gw, "t4", "sse", body("/w/c", 1))
        .await
        .bytes()
        .await
        .unwrap();
    let t = wait_for_traces("t4", 1).await;
    assert_eq!(t.len(), 1, "{t:#?}");
    assert_eq!(field(&t[0], "diverged"), Some("first"));
}

#[tokio::test]
async fn a_pending_writer_is_a_race_but_a_begun_one_is_not() {
    let _ = logs();
    let gw = spawn_gateway(
        &spawn_upstream(Upstream::default()).await,
        TokenSource::Disabled,
    )
    .await;

    // A's first chunk leaves after 1.5 s; B goes out 300 ms after A.
    let gw_a = gw.clone();
    let a = tokio::spawn(async move {
        call(&gw_a, "t5", "slow-start", body("/w/r", 1))
            .await
            .bytes()
            .await
            .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    call(&gw, "t5", "sse", body("/w/r", 2))
        .await
        .bytes()
        .await
        .unwrap();
    a.await.unwrap();
    let t = wait_for_traces("t5", 2).await;
    let b = t
        .iter()
        .find(|l| field(l, "msgs") == Some("2"))
        .expect("B's line");
    assert_eq!(field(b, "inflight_same_prefix"), Some("1"), "{b}");
    assert_eq!(field(b, "prefix_ttl_left_ms"), Some("-"), "{b}");
    assert_eq!(field(b, "inflight_same_tools"), Some("1"), "{b}");

    // C's first chunk leaves at once and its tail 1.5 s later; D goes out
    // 300 ms after C: the entry is readable, no race.
    let gw_c = gw.clone();
    let c = tokio::spawn(async move {
        call(&gw_c, "t5", "slow-tail", body("/w/r", 3))
            .await
            .bytes()
            .await
            .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    call(&gw, "t5", "sse", body("/w/r", 4))
        .await
        .bytes()
        .await
        .unwrap();
    c.await.unwrap();
    let t = wait_for_traces("t5", 4).await;
    let d = t
        .iter()
        .find(|l| field(l, "msgs") == Some("4"))
        .expect("D's line");
    assert_eq!(field(d, "inflight_same_prefix"), Some("0"), "{d}");
    assert!(num(d, "prefix_ttl_left_ms") > 0, "{d}");
}

/// Assumption 1 of the spec: the 401 retry re-sends byte-identical bytes,
/// so one `on_send` per request is sound.
#[tokio::test]
async fn the_401_retry_resends_identical_bytes_and_leaves_one_line() {
    let _ = logs();
    let dir = tempfile::tempdir().unwrap();
    let cred = dir.path().join(".credentials.json");
    write_credential(&cred, STALE, -60_000);
    let up = Upstream {
        rewrite: Some(cred.clone()),
        ..Default::default()
    };
    let hits = up.hits.clone();
    let bodies = up.bodies.clone();
    let gw = spawn_gateway(
        &spawn_upstream(up).await,
        TokenSource::CredentialsFile(cred),
    )
    .await;

    let r = call(&gw, "t6", "sse", body("/w/401", 1)).await;
    assert_eq!(r.status(), 200, "the retry should have succeeded");
    r.bytes().await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 2, "first attempt + one retry");
    {
        let b = bodies.lock().unwrap();
        assert_eq!(
            b[0], b[1],
            "the retry must re-send the bytes the trace fingerprinted"
        );
    }
    let t = wait_for_traces("t6", 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let again = traces("t6");
    assert_eq!(t.len(), 1, "{t:#?}");
    assert_eq!(again.len(), 1, "a second line appeared late: {again:#?}");
    assert_eq!(field(&t[0], "status"), Some("200"));
    assert_eq!(field(&t[0], "read"), Some("0"));
}
```

- [ ] `cargo test --test cache_trace 2>&1 | tail -3` prints `test result: ok. 6 passed; 0 failed`. The two race assertions depend on 1.5 s upstream delays against 300 ms gaps; if one flakes under load, double both delays (they are the only timing in the file).
- [ ] `cargo clippy --all-targets -- -D warnings 2>&1 | tail -1` prints a line starting with `    Finished`; `cargo fmt`.

Note for the local checkout only: `src/disguise/disguise_impl.rs` exists here, so `has_beta_hook` is set and the 401 test runs the real billing-prefix injection; CI has the stub. The assertions above hold in both (the test does not look at `system_blocks`).

### Step 4.2 — commit

```
tests: end-to-end cache trace lines

Two sends of one prefix, a cwd change, the 429 rollback, count_tokens
producing nothing, the pending-writer race against a begun writer,
and the 401 retry re-sending identical bytes with one line.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
```

**Interfaces — Consumes (from Task 3):** the line format, `AppState::with_version`, `build_router`, `TokenSource::{Disabled, CredentialsFile}`.

---

## Task 5 — `scripts/cache-trace-report.sh`, fixture and its test

Every definition below is copied from the spec's "Cold start, avoidable,
buckets" section; the script is the only place they are re-derived, so it
cites the spec at the top.

### Step 5.1 — the fixture

- [ ] Create `tests/fixtures/cache-trace/sample.log` with exactly these 13 lines (each is one physical line; the `…` below are not in the file, every line is written out in full):

```
2026-10-13T06:00:01.000000Z DEBUG mur_model_gateway::cache_trace: cache trace client=none model=claude-opus-5-5 req_id=msg_01 status=200 streaming=true compress_on=1 tools_count=38 system_blocks=1 msgs=1 tools_bytes=30534 system_bytes=4120 bp_bytes=34654 prefix_bytes=34654 span_bytes=40000 ctx_fp=00000000000000c0 tools_fp=00000000000000a1 system_fp=00000000000000b1 prefix_fp=00000000000000f1 bp=tools:none,system:last,msgs:last bp_ttls=5m ttl=5m ttl_max=5m input=100 read=0 creation=40000 creation_5m=40000 creation_1h=0 diverged=first prefix_seen_ago_ms=- prefix_ttl_left_ms=- tools_seen_ago_ms=- tools_ttl_left_ms=- inflight_same_prefix=0 inflight_same_tools=0
2026-10-13T06:00:01.800000Z DEBUG mur_model_gateway::cache_trace: cache trace client=none model=claude-opus-5-5 req_id=msg_02 status=200 streaming=true compress_on=1 tools_count=38 system_blocks=1 msgs=1 tools_bytes=30534 system_bytes=4120 bp_bytes=34654 prefix_bytes=34654 span_bytes=40000 ctx_fp=00000000000000c0 tools_fp=00000000000000a1 system_fp=00000000000000b1 prefix_fp=00000000000000f1 bp=tools:none,system:last,msgs:last bp_ttls=5m ttl=5m ttl_max=5m input=100 read=0 creation=40000 creation_5m=40000 creation_1h=0 diverged=msgs:0 prefix_seen_ago_ms=800 prefix_ttl_left_ms=- tools_seen_ago_ms=800 tools_ttl_left_ms=- inflight_same_prefix=1 inflight_same_tools=1
2026-10-13T06:02:01.000000Z DEBUG mur_model_gateway::cache_trace: cache trace client=none model=claude-opus-5-5 req_id=msg_03 status=200 streaming=true compress_on=1 tools_count=38 system_blocks=1 msgs=1 tools_bytes=30534 system_bytes=4120 bp_bytes=34654 prefix_bytes=34654 span_bytes=40000 ctx_fp=00000000000000c0 tools_fp=00000000000000a1 system_fp=00000000000000b2 prefix_fp=00000000000000f2 bp=tools:none,system:last,msgs:last bp_ttls=5m ttl=5m ttl_max=5m input=100 read=0 creation=40000 creation_5m=40000 creation_1h=0 diverged=system:0@31 prefix_seen_ago_ms=- prefix_ttl_left_ms=- tools_seen_ago_ms=120000 tools_ttl_left_ms=180000 inflight_same_prefix=0 inflight_same_tools=0
2026-10-13T06:03:01.000000Z DEBUG mur_model_gateway::cache_trace: cache trace client=none model=claude-opus-5-5 req_id=msg_04 status=200 streaming=true compress_on=1 tools_count=38 system_blocks=1 msgs=1 tools_bytes=30534 system_bytes=4120 bp_bytes=34654 prefix_bytes=34654 span_bytes=40000 ctx_fp=00000000000000c0 tools_fp=00000000000000a1 system_fp=00000000000000b2 prefix_fp=00000000000000f2 bp=tools:none,system:last,msgs:last bp_ttls=5m ttl=5m ttl_max=5m input=100 read=0 creation=40000 creation_5m=40000 creation_1h=0 diverged=none prefix_seen_ago_ms=60000 prefix_ttl_left_ms=240000 tools_seen_ago_ms=60000 tools_ttl_left_ms=240000 inflight_same_prefix=0 inflight_same_tools=0
2026-10-13T06:04:41.000000Z DEBUG mur_model_gateway::cache_trace: cache trace client=none model=claude-opus-5-5 req_id=msg_05 status=200 streaming=true compress_on=1 tools_count=38 system_blocks=1 msgs=3 tools_bytes=30534 system_bytes=4120 bp_bytes=34654 prefix_bytes=34654 span_bytes=41000 ctx_fp=00000000000000c0 tools_fp=00000000000000a1 system_fp=00000000000000b2 prefix_fp=00000000000000f2 bp=tools:none,system:last,msgs:last bp_ttls=5m ttl=5m ttl_max=5m input=120 read=34654 creation=5346 creation_5m=5346 creation_1h=0 diverged=msgs:1 prefix_seen_ago_ms=100000 prefix_ttl_left_ms=200000 tools_seen_ago_ms=100000 tools_ttl_left_ms=200000 inflight_same_prefix=0 inflight_same_tools=0
2026-10-13T07:00:00.000000Z DEBUG mur_model_gateway::cache_trace: cache trace client=claude-cli model=claude-opus-5-5 req_id=msg_06 status=200 streaming=true compress_on=1 tools_count=90 system_blocks=3 msgs=40 tools_bytes=120000 system_bytes=10000 bp_bytes=120000,130000 prefix_bytes=130000 span_bytes=400000 ctx_fp=00000000000000c1 tools_fp=00000000000000a9 system_fp=00000000000000b9 prefix_fp=00000000000000f9 bp=tools:last,system:last,msgs:last+last-2 bp_ttls=1h,1h ttl=1h ttl_max=1h input=2000 read=0 creation=400000 creation_5m=0 creation_1h=400000 diverged=msgs:38 prefix_seen_ago_ms=4000000 prefix_ttl_left_ms=-400000 tools_seen_ago_ms=4000000 tools_ttl_left_ms=-400000 inflight_same_prefix=0 inflight_same_tools=0
2026-10-13T07:03:20.000000Z DEBUG mur_model_gateway::cache_trace: cache trace client=claude-cli model=claude-opus-5-5 req_id=msg_07 status=200 streaming=true compress_on=1 tools_count=90 system_blocks=3 msgs=20 tools_bytes=120000 system_bytes=10000 bp_bytes=120000,130000 prefix_bytes=130000 span_bytes=200000 ctx_fp=00000000000000c1 tools_fp=00000000000000a9 system_fp=00000000000000b9 prefix_fp=00000000000000f9 bp=tools:last,system:last,msgs:last bp_ttls=1h,5m ttl=5m ttl_max=1h input=1500 read=0 creation=200000 creation_5m=80000 creation_1h=120000 diverged=msgs:18 prefix_seen_ago_ms=200000 prefix_ttl_left_ms=100000 tools_seen_ago_ms=200000 tools_ttl_left_ms=3400000 inflight_same_prefix=0 inflight_same_tools=0
2026-10-13T08:00:00.000000Z DEBUG mur_model_gateway::cache_trace: cache trace client=claude-cli model=claude-sonnet-5-5 req_id=msg_08 status=200 streaming=true compress_on=0 tools_count=38 system_blocks=1 msgs=3 tools_bytes=30534 system_bytes=4120 bp_bytes=- prefix_bytes=34654 span_bytes=50000 ctx_fp=00000000000000c0 tools_fp=00000000000000a1 system_fp=00000000000000b3 prefix_fp=00000000000000f3 bp=tools:none,system:none,msgs:last bp_ttls=- ttl=- ttl_max=5m input=300 read=0 creation=50000 creation_5m=50000 creation_1h=0 diverged=first prefix_seen_ago_ms=- prefix_ttl_left_ms=- tools_seen_ago_ms=30000 tools_ttl_left_ms=270000 inflight_same_prefix=0 inflight_same_tools=0
2026-10-13T08:01:00.000000Z DEBUG mur_model_gateway::cache_trace: cache trace client=none model=claude-sonnet-5-5 req_id=msg_09 status=200 streaming=true compress_on=0 tools_count=12 system_blocks=1 msgs=1 tools_bytes=6000 system_bytes=2000 bp_bytes=8000 prefix_bytes=8000 span_bytes=10000 ctx_fp=00000000000000c0 tools_fp=00000000000000a2 system_fp=00000000000000b4 prefix_fp=00000000000000f4 bp=tools:none,system:last,msgs:last bp_ttls=5m ttl=5m ttl_max=5m input=50 read=0 creation=10000 creation_5m=- creation_1h=- diverged=first prefix_seen_ago_ms=- prefix_ttl_left_ms=- tools_seen_ago_ms=- tools_ttl_left_ms=- inflight_same_prefix=0 inflight_same_tools=0
2026-10-13T08:01:05.000000Z DEBUG mur_model_gateway::cache_trace: cache trace client=none model=claude-sonnet-5-5 req_id=- status=429 streaming=false compress_on=0 tools_count=38 system_blocks=1 msgs=1 tools_bytes=30534 system_bytes=4120 bp_bytes=34654 prefix_bytes=34654 span_bytes=40000 ctx_fp=00000000000000c0 tools_fp=00000000000000a1 system_fp=00000000000000b1 prefix_fp=00000000000000f1 bp=tools:none,system:last,msgs:last bp_ttls=5m ttl=5m ttl_max=5m input=- read=- creation=- creation_5m=- creation_1h=- diverged=none prefix_seen_ago_ms=5000 prefix_ttl_left_ms=295000 tools_seen_ago_ms=5000 tools_ttl_left_ms=295000 inflight_same_prefix=0 inflight_same_tools=0
2026-10-13T08:02:00.000000Z DEBUG mur_model_gateway::cache_trace: cache trace client=none model=claude-opus-5-5 req_id=msg_11 status=200 streaming=true compress_on=1 tools_count=38 system_blocks=1 msgs=4100 tools_bytes=30534 system_bytes=4120 bp_bytes=34654 prefix_bytes=34654 span_bytes=900000 ctx_fp=00000000000000c0 tools_fp=00000000000000a1 system_fp=00000000000000b2 prefix_fp=00000000000000f2 bp=tools:none,system:last,msgs:last bp_ttls=5m ttl=5m ttl_max=5m input=800 read=34654 creation=1000 creation_5m=1000 creation_1h=0 diverged=msgs:4096+ prefix_seen_ago_ms=20000 prefix_ttl_left_ms=250000 tools_seen_ago_ms=20000 tools_ttl_left_ms=250000 inflight_same_prefix=0 inflight_same_tools=0
2026-10-13T08:03:00.000000Z DEBUG mur_model_gateway::cache_trace: cache trace client=claude-cli model=claude-sonnet-5-5 req_id=msg_12 status=200 streaming=false compress_on=0 tools_count=38 system_blocks=1 msgs=2 tools_bytes=30534 system_bytes=4120 bp_bytes=34654 prefix_bytes=34654 span_bytes=42000 ctx_fp=00000000000000c0 tools_fp=00000000000000a1 system_fp=00000000000000b3 prefix_fp=00000000000000f5 bp=tools:none,system:last,msgs:last bp_ttls=5m ttl=5m ttl_max=5m input=20 read=20000 creation=0 creation_5m=0 creation_1h=0 diverged=msgs:1 prefix_seen_ago_ms=15000 prefix_ttl_left_ms=285000 tools_seen_ago_ms=15000 tools_ttl_left_ms=285000 inflight_same_prefix=0 inflight_same_tools=0
2026-10-13T09:00:00.000000Z DEBUG mur_model_gateway::cache_trace: cache trace client=claude-cli model=claude-opus-5-5 req_id=msg_13 status=200 streaming=true compress_on=1 tools_count=90 system_blocks=3 msgs=40 tools_bytes=120000 system_bytes=10000 bp_bytes=120000,130000 prefix_bytes=130000 span_bytes=400000 ctx_fp=00000000000000c1 tools_fp=00000000000000a9 system_fp=00000000000000b9 prefix_fp=00000000000000f9 bp=tools:last,system:last,msgs:last bp_ttls=1h,1h ttl=1h ttl_max=1h input=2000 read=0 creation=400000 creation_5m=0 creation_1h=400000 diverged=none prefix_seen_ago_ms=100000 prefix_ttl_left_ms=-50000 tools_seen_ago_ms=100000 tools_ttl_left_ms=-50000 inflight_same_prefix=0 inflight_same_tools=0
```

What each line exercises (the test below asserts the totals this implies):

| line | model / client | bucket | notes |
|---|---|---|---|
| 1 | opus / none | first | genuine first send |
| 2 | opus / none | concurrent | pending writer, nothing readable |
| 3 | opus / none | first | the cwd case: new prefix, tools readable → B1 line |
| 4 | opus / none | sequential | readable entry, still cold → B3 line and B1 line (overlap) |
| 5 | opus / none | not cold | hit |
| 6 | opus / claude-cli | expired | 1h prefix, dead for 400 s; `tools:last` so not B1 |
| 7 | opus / claude-cli | sequential | mixed TTL prefix: segments 120000@1h + 10000@5m |
| 8 | sonnet / claude-cli | no_prefix_bp / msgs_only | tools readable → B1 line |
| 9 | sonnet / none | first | no `cache_creation` object → fallback |
| 10 | sonnet / none | no usage | 429 |
| 11 | opus / none | not cold | `diverged` tail form |
| 12 | sonnet / claude-cli | not cold | non-streaming |
| 13 | opus / claude-cli | expired | `prefix_seen_ago_ms < ttl` yet dead: TTL changed between sends |

### Step 5.2 — the script

- [ ] Create `scripts/cache-trace-report.sh` and `chmod +x` it:

```bash
#!/usr/bin/env bash
# Aggregate the gateway's `cache trace` log lines into the numbers that size
# the prompt-cache fixes: cold-start buckets, avoidable write tokens (B3 and
# the sequential investigation) and B1's tools-breakpoint target, per model.
# Every definition is the spec's (docs/specs/2026-10-13-cache-trace-design.md,
# "Cold start, avoidable, buckets"); change it there first.
#
#   scripts/cache-trace-report.sh [proxy.log]
#   MUR_REPORT_CLIENT=claude-cli scripts/cache-trace-report.sh   # one client only
#
# Estimates (`prefix_est`, `tools_est`) scale `creation` by canonical byte
# share and are labelled est; the raw `creation` beside them is the bound.
set -euo pipefail

LOG="${1:-$HOME/Library/Logs/mur-model-gateway/proxy.log}"
strip() {
  sed 's/\x1b\[[0-9;]*m//g' "$LOG" \
  | { grep "cache trace " || true; } \
  | if [ -n "${MUR_REPORT_CLIENT:-}" ]; then grep -E "client=\"?${MUR_REPORT_CLIENT}\"?( |$)" || true; else cat; fi \
  | sed -E 's/.*cache trace //'
}

strip | awk '
function num(x) { return x == "-" ? 0 : x + 0 }
function rate(t) { return t == "1h" ? 2 : 1.25 }
function ttl_ms(t) { return t == "1h" ? 3600000 : 300000 }
function pct(a, b) { return b ? sprintf("%.1f%%", 100 * a / b) : "n/a" }
function median(m, n,   i, j, t, arr) {
  for (i = 1; i <= n; i++) arr[i] = tbv[m, i]
  for (i = 2; i <= n; i++) { t = arr[i]; j = i - 1; while (j >= 1 && arr[j] > t) { arr[j + 1] = arr[j]; j-- } arr[j + 1] = t }
  return arr[int((n + 1) / 2)]
}
{
  delete v
  fb = 0
  for (i = 1; i <= NF; i++) { eq = index($i, "="); if (eq) v[substr($i, 1, eq - 1)] = substr($i, eq + 1) }
  m = v["model"]; gsub(/"/, "", m); if (m == "") m = "-"
  c = v["client"]; gsub(/"/, "", c); if (c == "") c = "-"
  if (!(m in seen_model)) { seen_model[m] = 1; mlist[++mn] = m }
  mc[m SUBSEP c] = 1
  lines[m]++; lines_c[m, c]++
  if (v["streaming"] != "true") nonstream[m]++
  if (v["status"] !~ /^2/) bad_status[m]++
  tb = num(v["tools_bytes"]); tbn[m]++; tbv[m, tbn[m]] = tb
  if (v["read"] == "-") { no_usage[m]++; next }
  usage[m]++; usage_c[m, c]++
  creation = num(v["creation"]); rd = num(v["read"])
  if (v["creation_5m"] != "-") { c5 = num(v["creation_5m"]); c1 = num(v["creation_1h"]) }
  else { fallback[m]++; fb = 1; if (v["ttl"] == "1h") { c1 = creation; c5 = 0 } else { c5 = creation; c1 = 0 } }
  cr[m] += creation; cr5[m] += c5; cr1[m] += c1; cr_c[m, c] += creation
  wr[m] += c5 * 1.25 + c1 * 2
  ttl = v["ttl"]; tmax = v["ttl_max"]; b1 = (tmax == "-") ? "5m" : tmax
  # sanity
  nt = split(v["bp_ttls"], bt, ",")
  if (v["bp_ttls"] != "-" && bt[nt] != ttl) s_ttl[m]++
  if (v["bp_ttls"] == "-" && ttl != "-") s_ttl[m]++
  seen5 = 0; for (k = 1; k <= nt; k++) { if (bt[k] == "5m") seen5 = 1; else if (bt[k] == "1h" && seen5) s_order[m]++ }
  span = v["span_bytes"]; pb = num(v["prefix_bytes"])
  if (span != "-" && pb > num(span)) s_span[m]++
  nb = split(v["bp_bytes"], bb, ",")
  if (v["bp_bytes"] != "-") { for (k = 2; k <= nb; k++) if (bb[k] + 0 < bb[k - 1] + 0) s_bb[m]++; if (bb[nb] + 0 != pb) s_bb[m]++ }
  if (v["creation_5m"] != "-" && ((ttl == "1h" && c1 == 0 && creation > 0) || (tmax == "5m" && c1 > 0))) s_split[m]++
  if (v["diverged"] ~ /@\?$/ || v["diverged"] ~ /\+$/) s_div[m]++
  if ((v["prefix_seen_ago_ms"] == "-" && num(v["inflight_same_prefix"]) > 0) || (v["tools_seen_ago_ms"] == "-" && num(v["inflight_same_tools"]) > 0)) s_inv[m]++
  ptl = v["prefix_ttl_left_ms"]; psa = v["prefix_seen_ago_ms"]; ip = num(v["inflight_same_prefix"])
  if (ttl != "-" && ip == 0 && psa != "-" && num(psa) < ttl_ms(ttl) && (ptl == "-" || num(ptl) < 0)) s_seen[m]++
  # cold start: read=0 and creation>0
  if (!(rd == 0 && creation > 0)) next
  cold[m]++; cold_c[m, c]++
  sp = num(span)
  pest = sp ? creation * pb / sp : 0
  test = sp ? creation * tb / sp : 0
  if (ttl == "-") wp = pest * rate(b1)
  else { w = 0; prev = 0; for (k = 1; k <= nb; k++) { w += (bb[k] - prev) * rate(bt[k]); prev = bb[k] }; wp = sp ? creation / sp * w : 0 }
  wt = test * rate(b1)
  split(v["bp"], bps, ","); tseg = substr(bps[1], 7); mseg = substr(bps[3], 6)
  if (ttl == "-") { b = "no_prefix_bp"; sub_ = (mseg == "none") ? "no_cache_control" : (mseg == "auto" ? "top_level_only" : "msgs_only"); subn[m, sub_]++ }
  else if (ptl == "-" && ip == 0) b = "first"
  else if ((ptl == "-" || num(ptl) < 0) && ip > 0) b = "concurrent"
  else if (num(ptl) >= 0) b = "sequential"
  else b = "expired"
  bn[m, b]++; bcr[m, b] += creation; bpe[m, b] += pest; bw[m, b] += wp; if (fb) bfb[m, b]++
  if (tseg == "none" && num(v["tools_count"]) > 0 && v["tools_ttl_left_ms"] != "-" && num(v["tools_ttl_left_ms"]) >= 0) {
    b1n[m]++; b1cr[m] += creation; b1te[m] += test; b1w[m] += wt; b1b[m, b]++
    if (b == "concurrent" || b == "sequential") { ovn[m]++; ovte[m] += test }
  }
}
END {
  if (!mn) { print "none logged"; exit }
  for (i = 2; i <= mn; i++) { t = mlist[i]; j = i - 1; while (j >= 1 && mlist[j] > t) { mlist[j + 1] = mlist[j]; j-- } mlist[j + 1] = t }
  nbk = split("first concurrent sequential expired no_prefix_bp", bk, " ")
  for (mi = 1; mi <= mn; mi++) {
    m = mlist[mi]
    printf "== model %s ==\n", m
    printf "lines             %d  with usage %d  cold starts %d  non-streaming %d (%s)\n", lines[m], usage[m], cold[m], nonstream[m], pct(nonstream[m], lines[m])
    printf "writes (raw)      creation %d  5m %d  1h %d  weighted %.1f  fallback %d\n", cr[m], cr5[m], cr1[m], wr[m], fallback[m]
    for (k in mc) { split(k, p, SUBSEP); if (p[1] != m) continue; c = p[2]
      printf "  client %-12s lines=%d  usage=%d  cold=%d  creation=%d\n", c, lines_c[m, c], usage_c[m, c], cold_c[m, c], cr_c[m, c] }
    printf "buckets           prefix_est = creation * prefix_bytes / span_bytes, weighted at the prefix breakpoint TTL\n"
    for (bi = 1; bi <= nbk; bi++) { b = bk[bi]
      printf "  %-14s n=%d  creation=%d  prefix_est=%.1f  weighted=%.1f  fallback=%d  (%s of weighted writes)\n", b, bn[m, b], bcr[m, b], bpe[m, b], bw[m, b], bfb[m, b], pct(bw[m, b], wr[m])
      if (b == "no_prefix_bp") printf "    sub-classes   no_cache_control=%d  top_level_only=%d  msgs_only=%d\n", subn[m, "no_cache_control"], subn[m, "top_level_only"], subn[m, "msgs_only"] }
    an = bn[m, "concurrent"] + bn[m, "sequential"]; acr = bcr[m, "concurrent"] + bcr[m, "sequential"]
    ape = bpe[m, "concurrent"] + bpe[m, "sequential"]; aw = bw[m, "concurrent"] + bw[m, "sequential"]
    printf "avoidable (B3)    n=%d  creation=%d  prefix_est=%.1f  weighted=%.1f  (%s of weighted writes)\n", an, acr, ape, aw, pct(aw, wr[m])
    printf "B1 tools bp       n=%d  creation=%d  tools_est=%.1f  weighted@b1_ttl=%.1f  overlap with B3: n=%d tools_est=%.1f\n", b1n[m], b1cr[m], b1te[m], b1w[m], ovn[m], ovte[m]
    printf "  by bucket       first=%d  concurrent=%d  sequential=%d  expired=%d  no_prefix_bp=%d\n", b1b[m, "first"], b1b[m, "concurrent"], b1b[m, "sequential"], b1b[m, "expired"], b1b[m, "no_prefix_bp"]
    mn_tb = tbv[m, 1]; for (k = 2; k <= tbn[m]; k++) if (tbv[m, k] < mn_tb) mn_tb = tbv[m, k]
    printf "  tools_bytes     min=%d  median=%d\n", mn_tb, median(m, tbn[m])
    printf "sanity            ttl!=last(bp_ttls)=%d  ttl_order=%d  prefix>span=%d  bp_bytes=%d  split_disagree=%d  status!=2xx=%d  read=-:%d  diverged?+=%d  invariant=%d  seen_vs_ttl_left=%d\n", s_ttl[m], s_order[m], s_span[m], s_bb[m], s_split[m], bad_status[m], no_usage[m], s_div[m], s_inv[m], s_seen[m]
    print ""
  }
}'
```

- [ ] `bash -n scripts/cache-trace-report.sh` prints nothing.
- [ ] `scripts/cache-trace-report.sh tests/fixtures/cache-trace/sample.log` prints two sections. The `claude-opus-5-5` section must contain these lines verbatim (the `client` lines may come in either order):

```
== model claude-opus-5-5 ==
lines             9  with usage 9  cold starts 7  non-streaming 0 (0.0%)
writes (raw)      creation 1166346  5m 246346  1h 920000  weighted 2147932.5  fallback 0
  client none         lines=6  usage=6  cold=4  creation=166346
  client claude-cli   lines=3  usage=3  cold=3  creation=1000000
  first          n=2  creation=80000  prefix_est=69308.0  weighted=86635.0  fallback=0  (4.0% of weighted writes)
  concurrent     n=1  creation=40000  prefix_est=34654.0  weighted=43317.5  fallback=0  (2.0% of weighted writes)
  sequential     n=2  creation=240000  prefix_est=164654.0  weighted=295817.5  fallback=0  (13.8% of weighted writes)
  expired        n=2  creation=800000  prefix_est=260000.0  weighted=520000.0  fallback=0  (24.2% of weighted writes)
  no_prefix_bp   n=0  creation=0  prefix_est=0.0  weighted=0.0  fallback=0  (0.0% of weighted writes)
avoidable (B3)    n=3  creation=280000  prefix_est=199308.0  weighted=339135.0  (15.8% of weighted writes)
B1 tools bp       n=2  creation=80000  tools_est=61068.0  weighted@b1_ttl=76335.0  overlap with B3: n=1 tools_est=30534.0
  by bucket       first=1  concurrent=0  sequential=1  expired=0  no_prefix_bp=0
  tools_bytes     min=30534  median=30534
sanity            ttl!=last(bp_ttls)=0  ttl_order=0  prefix>span=0  bp_bytes=0  split_disagree=0  status!=2xx=0  read=-:0  diverged?+=1  invariant=0  seen_vs_ttl_left=1
```

and the `claude-sonnet-5-5` section these:

```
== model claude-sonnet-5-5 ==
lines             4  with usage 3  cold starts 2  non-streaming 2 (50.0%)
writes (raw)      creation 60000  5m 60000  1h 0  weighted 75000.0  fallback 1
  first          n=1  creation=10000  prefix_est=8000.0  weighted=10000.0  fallback=1  (13.3% of weighted writes)
  no_prefix_bp   n=1  creation=50000  prefix_est=34654.0  weighted=43317.5  fallback=0  (57.8% of weighted writes)
    sub-classes   no_cache_control=0  top_level_only=0  msgs_only=1
avoidable (B3)    n=0  creation=0  prefix_est=0.0  weighted=0.0  (0.0% of weighted writes)
B1 tools bp       n=1  creation=50000  tools_est=30534.0  weighted@b1_ttl=38167.5  overlap with B3: n=0 tools_est=0.0
  by bucket       first=0  concurrent=0  sequential=0  expired=0  no_prefix_bp=1
  tools_bytes     min=6000  median=30534
sanity            ttl!=last(bp_ttls)=0  ttl_order=0  prefix>span=0  bp_bytes=0  split_disagree=0  status!=2xx=1  read=-:1  diverged?+=0  invariant=0  seen_vs_ttl_left=0
```

How the opus numbers arise, so a mismatch can be traced: raw creation is lines 1–7, 11, 13 (160000 + 5346 + 400000 + 200000 + 1000 + 400000); 5m tokens 246346 and 1h tokens 920000 give 246346 × 1.25 + 920000 × 2 = 2147932.5. Sequential is lines 4 and 7: prefix_est 34654 + 130000; weighted 34654 × 1.25 + (120000 × 2 + 10000 × 1.25) = 43317.5 + 252500. Expired is lines 6 and 13, each 130000 at 1h → 260000 each. Avoidable adds concurrent (line 2: 34654 / 43317.5). B1 is lines 3 and 4 (`tools:none`, `tools_ttl_left_ms ≥ 0`): tools_est 30534 each at 5m. Line 11 has a `+` tail in `diverged`; line 13 has `prefix_seen_ago_ms` 100000 < 1h with a negative `prefix_ttl_left_ms`.

- [ ] `MUR_REPORT_CLIENT=none scripts/cache-trace-report.sh tests/fixtures/cache-trace/sample.log | grep -c "client claude-cli"` prints `0`.

### Step 5.3 — the test

- [ ] Create `tests/cache_trace_report.rs`:

```rust
//! The report script against the hand-written fixture: every bucket, the
//! mixed-TTL weighting, the B1 line with its overlap, the fallback rule and
//! the sanity counters, with the numbers worked out in the plan
//! (docs/superpowers/plans/2026-10-13-cache-trace.md, Task 5).

use std::process::Command;

fn run(env_client: Option<&str>) -> String {
    let root = env!("CARGO_MANIFEST_DIR");
    let mut cmd = Command::new("bash");
    cmd.arg(format!("{root}/scripts/cache-trace-report.sh"))
        .arg(format!("{root}/tests/fixtures/cache-trace/sample.log"));
    if let Some(c) = env_client {
        cmd.env("MUR_REPORT_CLIENT", c);
    }
    let out = cmd.output().expect("bash runs the report");
    assert!(
        out.status.success(),
        "report failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn buckets_estimates_and_sanity_from_the_fixture() {
    let text = run(None);
    let expected = [
        "== model claude-opus-5-5 ==",
        "lines             9  with usage 9  cold starts 7  non-streaming 0 (0.0%)",
        "writes (raw)      creation 1166346  5m 246346  1h 920000  weighted 2147932.5  fallback 0",
        "  client none         lines=6  usage=6  cold=4  creation=166346",
        "  client claude-cli   lines=3  usage=3  cold=3  creation=1000000",
        "  first          n=2  creation=80000  prefix_est=69308.0  weighted=86635.0  fallback=0  (4.0% of weighted writes)",
        "  concurrent     n=1  creation=40000  prefix_est=34654.0  weighted=43317.5  fallback=0  (2.0% of weighted writes)",
        "  sequential     n=2  creation=240000  prefix_est=164654.0  weighted=295817.5  fallback=0  (13.8% of weighted writes)",
        "  expired        n=2  creation=800000  prefix_est=260000.0  weighted=520000.0  fallback=0  (24.2% of weighted writes)",
        "avoidable (B3)    n=3  creation=280000  prefix_est=199308.0  weighted=339135.0  (15.8% of weighted writes)",
        "B1 tools bp       n=2  creation=80000  tools_est=61068.0  weighted@b1_ttl=76335.0  overlap with B3: n=1 tools_est=30534.0",
        "  by bucket       first=1  concurrent=0  sequential=1  expired=0  no_prefix_bp=0",
        "  tools_bytes     min=30534  median=30534",
        "sanity            ttl!=last(bp_ttls)=0  ttl_order=0  prefix>span=0  bp_bytes=0  split_disagree=0  status!=2xx=0  read=-:0  diverged?+=1  invariant=0  seen_vs_ttl_left=1",
        "== model claude-sonnet-5-5 ==",
        "lines             4  with usage 3  cold starts 2  non-streaming 2 (50.0%)",
        "writes (raw)      creation 60000  5m 60000  1h 0  weighted 75000.0  fallback 1",
        "  first          n=1  creation=10000  prefix_est=8000.0  weighted=10000.0  fallback=1  (13.3% of weighted writes)",
        "  no_prefix_bp   n=1  creation=50000  prefix_est=34654.0  weighted=43317.5  fallback=0  (57.8% of weighted writes)",
        "    sub-classes   no_cache_control=0  top_level_only=0  msgs_only=1",
        "avoidable (B3)    n=0  creation=0  prefix_est=0.0  weighted=0.0  (0.0% of weighted writes)",
        "B1 tools bp       n=1  creation=50000  tools_est=30534.0  weighted@b1_ttl=38167.5  overlap with B3: n=0 tools_est=0.0",
        "  by bucket       first=0  concurrent=0  sequential=0  expired=0  no_prefix_bp=1",
        "  tools_bytes     min=6000  median=30534",
        "sanity            ttl!=last(bp_ttls)=0  ttl_order=0  prefix>span=0  bp_bytes=0  split_disagree=0  status!=2xx=1  read=-:1  diverged?+=0  invariant=0  seen_vs_ttl_left=0",
    ];
    for line in expected {
        assert!(
            text.lines().any(|l| l == line),
            "missing line:\n{line}\nin report:\n{text}"
        );
    }
    let opus = text.find("== model claude-opus-5-5 ==").unwrap();
    let sonnet = text.find("== model claude-sonnet-5-5 ==").unwrap();
    assert!(opus < sonnet, "models are printed in name order");
}

#[test]
fn client_filter_drops_the_other_client() {
    let text = run(Some("none"));
    assert!(!text.contains("client claude-cli"), "{text}");
    assert!(
        text.contains("  client none         lines=6  usage=6  cold=4  creation=166346"),
        "{text}"
    );
    assert!(
        text.contains("lines             6  with usage 6  cold starts 4"),
        "{text}"
    );
}

#[test]
fn an_empty_log_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty.log");
    std::fs::write(
        &empty,
        "2026-10-13T06:00:00Z DEBUG mur_model_gateway: proxied\n",
    )
    .unwrap();
    let root = env!("CARGO_MANIFEST_DIR");
    let out = Command::new("bash")
        .arg(format!("{root}/scripts/cache-trace-report.sh"))
        .arg(&empty)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "none logged");
}
```

- [ ] `cargo test --test cache_trace_report 2>&1 | tail -3` prints `test result: ok. 3 passed; 0 failed`.
- [ ] `cargo clippy --all-targets -- -D warnings 2>&1 | tail -1` prints a line starting with `    Finished`; `cargo fmt`.

### Step 5.4 — commit

```
scripts: cache-trace-report turns cache trace lines into avoidable writes

Per model: raw write totals with the 5m/1h split, cold-start buckets
(first / concurrent / sequential / expired / no_prefix_bp with its
sub-classes) with prefix_est weighted segment by segment from bp_bytes
and bp_ttls, the avoidable (B3) figure, the B1 tools-breakpoint line
with its per-bucket breakdown and overlap, and the sanity counters the
spec lists. A 13-line fixture pins every number.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
```

**Interfaces — Consumes (from Task 3):** the line's field names and `-` conventions. **Produces:** `scripts/cache-trace-report.sh [log]`, `MUR_REPORT_CLIENT` filter, the section layout above.

---

## Task 6 — docs and spec status

- [ ] In `README.md`, after the "Optional wire compression" bullet (the one ending `time taken).`), add:

```
- **Prompt-cache diagnostics.** At `debug`, every Anthropic `/v1/messages`
  request logs one `cache trace` line: fingerprints of the cached prefix,
  breakpoint layout, how recently the same prefix was sent and whether a
  writer was still pending, next to the response's cache reads and writes.
  `scripts/cache-trace-report.sh` turns a day of them into cold-start
  buckets and avoidable write tokens per model. Hashes and sizes only,
  never content.
```

- [ ] In `AGENTS.md`, before `## Gotchas`, add:

```
## Logs

- `cache trace` (DEBUG, Anthropic `/v1/messages` only) is one line per
  request pairing request-side fingerprints with the response's prompt-cache
  usage; `scripts/cache-trace-report.sh` aggregates it. Definitions live in
  `docs/specs/2026-10-13-cache-trace-design.md`; change the spec before the
  script. Hashes and sizes only, never block content.
```

- [ ] In `docs/specs/2026-10-13-cache-trace-design.md`, change `**Status:** Draft` to `**Status:** Implemented (2026-10-13, plan docs/superpowers/plans/2026-10-13-cache-trace.md)`.
- [ ] `cargo test 2>&1 | grep -E "^test result" | grep -v "0 failed" | wc -l` prints `0`.
- [ ] Commit:

```
docs: cache trace line and report in README, AGENTS.md, spec status

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
```

- [ ] Deploy and observe (not part of CI): `./scripts/setup.sh -- --compress` rebuilds and restarts the service (see `docs/compress-setup.md`); the plist already carries `RUST_LOG=info,mur_model_gateway=debug`. After a day of traffic, `scripts/cache-trace-report.sh` on `~/Library/Logs/mur-model-gateway/proxy.log` is the evidence sub-projects A, B1 and B3 wait for.

---

## Self-review (done while writing; re-run before handing off)

- **Spec coverage.** Prefix definition, canonical hashing, `ctx_fp`, `bp` encoding, `bp_ttls`/`ttl`/`ttl_max`, byte sizes and `bp_bytes`/`span_bytes`, `diverged` values including `ctx`, `@?` and the `+` tail → Task 1/2. Prefix key, tools key, breakpoint-carrying-only refresh, `readable_until` on begin, pending counts, invariant → Task 2. Hook point, once-per-request, DEBUG gate, guard as single writer, `abandon` on drop, status hand-off, retry reuse → Task 3. Buckets, estimates, segment weighting, B1 line with overlap, fallback rule, sanity list, `MUR_REPORT_CLIENT` → Task 5. Assumptions 1 (401 test) and 2 (key-order test) → Tasks 4 and 1; 3 and 4 are labelled in the script header and spec; 5 is pinned by `ttl_left_goes_negative_by_the_writers_ttl`.
- **Placeholders.** None: every step shows the code, every command its expected output.
- **Cross-task names.** `cache_trace::{applies, on_send, Tracker, InflightGuard, SendTrace, Analysis, Ttl}` and `UpstreamUsage` field names are identical in Tasks 1–4; the line's field order in `InflightGuard::line` (Task 2) is the order the fixture (Task 5) uses.
- **Deviation from the spec, recorded in the spec (Task 1):** canonical bytes exclude `cache_control`.
