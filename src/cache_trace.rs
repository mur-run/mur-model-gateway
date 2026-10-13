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
