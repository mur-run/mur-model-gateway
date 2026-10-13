//! Prompt-cache diagnostics (spec: docs/specs/2026-10-13-cache-trace-design.md).
//!
//! One `cache trace` DEBUG line per Anthropic `/v1/messages` request. The
//! request side (fingerprints, breakpoint layout, divergence from the previous
//! request, prefix and tools recency, pending writers, byte sizes) is computed
//! by [`on_send`] from the exact bytes handed to reqwest; the response side
//! (usage, id, model, status) is added by the [`InflightGuard`], the only
//! writer of the line. Fail-open: a body that does not parse produces no line
//! and touches no table. Hashes, sizes and offsets only; never block content.

// Task 1 lands the analysis half alone; the tables and the guard (Task 2) use
// the rest. Removed in Task 2.
#![allow(dead_code, unused_imports)]

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
