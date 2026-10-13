# mur-model-gateway: cache trace (prompt-cache diagnostics)

**Date:** 2026-10-13
**Status:** Draft
**Scope:** mur-model-gateway only. First sub-project of the cache-write work
("B2 + C": prefix-divergence logging plus the cache-economics report). The
runtime breakpoint layout (A), the gateway-added tools breakpoint (B1) and
cold-start coalescing (B3) are out of scope; each is decided on the evidence
this sub-project produces.

## Problem

The 2026-10-13 analysis of 634 `upstream usage` lines priced cache writes at
67% of the MUR agent runtime's Anthropic spend and 37% of Claude Code's. On
the runtime side 67 of 527 responses were full-cold (`cache_read = 0`,
`cache_creation >= 10k`) and wrote 2.1M tokens; 47 of them came within five
minutes of the previous cold start and 43 carried byte-identical `tools`. The
suspected causes are the session cwd embedded in the single system block, the
absence of a breakpoint after `tools`, and concurrent session starts. None of
it can be proven from proxy.log: `upstream usage` has no request-side
identity, `compress summary` has sizes but no fingerprints, and the two lines
are not paired. A gateway change (B1, B3) or a runtime change (A) made on that
guess would be measured against the same blind log.

## Decision

Emit one new DEBUG line, `cache trace`, per Anthropic `/v1/messages` request.
The request side (fingerprints, breakpoint layout, divergence from the
previous request, prefix recency, in-flight count) is computed at send time;
the response side (usage, id, model, status) is added when the response's
usage arrives, and the two halves go out as a single line. A new script,
`scripts/cache-trace-report.sh`, aggregates the line into "avoidable write
tokens" per model. The existing `upstream usage` and `compress summary` lines
are unchanged.

Rejected alternatives:

- **Two lines (send + response) joined by the report.** Lines are written in
  response order, which is not send order under concurrency; pairing in awk
  would need a request sequence number and would still be fragile.
- **Piggyback on compress's JSON parse** (`compress::rewrite_request_body`
  already walks the blocks). The fingerprint would be taken before the
  billing prefix is injected, so it is not what upstream sees; the trace
  would vanish whenever `MUR_MODEL_GATEWAY_COMPRESS` is off, which is exactly
  when it is needed as a control; and it couples wire compression to cache
  diagnostics.

## Definitions

These are the contract between the gateway line and the report. The report
must not re-derive any of them differently.

### Prefix

`tools` plus `system`, in the body handed to reqwest: after compression and
after `disguise::inject_billing_prefix`. Messages are never part of the
prefix. This is the unit a `system`-side breakpoint caches upstream.

### Fingerprints

Per-block hash: FNV-1a 64 over the block's canonical bytes, where canonical
means the block re-serialized by `serde_json::to_vec` (compact, object keys
sorted because the crate is built without `preserve_order`). Consequences,
by design:

- insensitive to JSON whitespace and object key order, sensitive to every
  content byte. Upstream hashes the rendered prompt, not the JSON text, so
  this is the closer model. Byte-exact hashing of the original body would
  need block byte ranges, which serde_json does not expose.
- `tools_fp` = FNV-1a over the sequence of tool hashes; `system_fp` likewise
  over the system block hashes (a string `system` counts as one block);
  `prefix_fp` = FNV-1a over (`tools_fp`, `system_fp`). All printed as 16 hex
  digits. FNV-1a is implemented inline (no crate) and has no per-process key,
  so fingerprints are comparable across gateway restarts.

### Breakpoint layout `bp=`

`bp=tools:<p>,system:<p>,msgs:<p>`. Each `<p>` is `none`, or one or more
positions joined by `+`:

- tools and system: `last` when the breakpoint sits on the section's last
  block, otherwise the 0-based block index (`system:1`).
- messages: `last` for a breakpoint inside the last message, `last-k` for
  the k-th message from the end, `auto` for a top-level `cache_control`
  (automatic caching, which marks the last block). `auto` may coexist with
  explicit positions (`msgs:last+auto`). Message granularity, not content
  block, is enough for diagnosis and is stable across turns.

The report derives the third-bucket sub-classes from `bp=`:
`tools:none,system:none,msgs:none` is `no_cache_control`;
`tools:none,system:none` with `msgs` containing `auto` only is
`top_level_only`; `tools:none,system:none` with explicit `msgs` positions is
`msgs_only`.

### TTLs

`bp_ttls=` lists, in document order, the TTL of every breakpoint inside the
prefix (tools, then system): `5m` or `1h`, with an omitted `ttl` recorded as
its effective value `5m`. `-` when the prefix has no breakpoint. It is a
diagnostic column: the API requires 1h entries before 5m ones, and the
report checks the order.

`ttl=` is the TTL of the last breakpoint inside the prefix, i.e. the last
element of `bp_ttls`, or `-` when the prefix has none. It is the client's
original value; a gateway-sent TTL (`sent_ttl`) is added only when B1
exists. Message breakpoint TTLs are not logged.

### Divergence `diverged=`

Compared at send time against the previous request sent by the same
`client` for the same request `model`, ordered by send time, without waiting
for responses. Values:

- `first`: no previous request for this client+model since the gateway
  started (or since the slot was evicted).
- `none`: tools, system and messages sequences all identical.
- `tools:i@off` / `system:i@off`: `i` is the first block whose hash differs,
  `off` the first differing byte of that block's canonical bytes; when one
  side's bytes are a prefix of the other's, `off` is the shorter length.
  When all shared blocks match and the counts differ, `off` is `+` (this
  request has more blocks) or `-` (fewer).
- `msgs:i`: first differing message index; no offset, because only message
  hashes are retained.

Sections are compared in order tools, system, messages; the first differing
section is reported.

### Prefix recency and concurrency

One table keyed by (request `model`, `prefix_fp`, the `tools` and `system`
segments of `bp`). The `msgs` segment is excluded on purpose: it says
nothing about whether the prefix is a cache unit, and a client that changes
its message breakpoint layout would otherwise split one prefix across keys.
The `model` is in the key because a cache entry is the model's computed KV state; the same
prefix on another model is a guaranteed miss.

At send time, under one lock, in this order: read, classify, update.

- `prefix_seen_ago_ms=`: milliseconds since the last send with the same key,
  `-` when the key is new. Every send updates the timestamp, not only cold
  ones, matching the API rule that a cache entry's lifetime restarts at the
  start of any request that writes or reads it.
- `inflight_same_prefix=`: number of sends with the same key whose response
  has not finished when this one is sent.

Invariant: `prefix_seen_ago_ms=-` and `inflight_same_prefix>0` never hold
together, because an in-flight send inserted the key. The report's buckets
rely on it. If the key ever changes, re-verify the invariant.

### Cold start, avoidable, buckets

The report counts a response as a **cold start** when `read=0` and
`creation>0`. Cold starts fall into exactly one bucket; rows are evaluated
top to bottom and the first match wins:

| bucket | condition |
|---|---|
| `first` | `prefix_seen_ago_ms=-` (a genuine first send) |
| `concurrent` | `ttl!=-` and `inflight_same_prefix>0` (B3's target) |
| `sequential` | `ttl!=-`, `inflight_same_prefix=0`, `prefix_seen_ago_ms` < TTL (same prefix, no hit: cause to investigate) |
| `expired` | `ttl!=-`, `inflight_same_prefix=0`, `prefix_seen_ago_ms` >= TTL |
| `no_prefix_bp` | `ttl=-`; sub-classed `no_cache_control` / `top_level_only` / `msgs_only` (B1's target) |

TTL is 300 000 ms for `5m` and 3 600 000 ms for `1h`. `no_prefix_bp` is
judged against the 5m window (what B1 would add by default): its tokens
count as avoidable when `prefix_seen_ago_ms` < 5m, regardless of in-flight
state, and are reported separately from `concurrent`/`sequential` so B1's
effect is measured apart from B3's.

**Avoidable write tokens** = the `creation` tokens of `concurrent` plus
`sequential`, with `no_prefix_bp`'s within-window tokens listed as a separate
B1 line. Each figure is reported as raw 5m tokens, raw 1h tokens, and
`weighted = 5m * 1.25 + 1h * 2` (multiples of the model's base input price),
so that savings after B1/B3 can be compared with the bill. Everything is
grouped by `model=`.

**5m/1h split rule.** When the response carries the `cache_creation` object,
`creation_5m` and `creation_1h` are used directly and `ttl=` is diagnostic
only. When the object is absent (`creation_5m=-`), the whole `creation` is
attributed by `ttl=`: `1h` to the 1h column, `5m` or `-` to the 5m column,
and the line is counted in a `fallback` column so third-bucket weighting is
never silently mixed with pre-split API data.

## Design

### Hook point

`src/lib.rs`, in the proxy handler: right after `final_body` is built (the
`inject_billing_prefix` branch) and before `upstream_req.body(final_body)`.
Verified on the current tree: the code between those two points only
touches headers. If a future change edits the body after this point, the
hook moves after that edit; the fingerprint must be of the bytes reqwest
sends, or the trace is worthless.

`on_send` runs once per request, before the first upstream attempt. The 401
retry arm reuses the same `SendTrace` and guard (see Assumptions). It runs
only when all hold: provider is Anthropic, the path is `/v1/messages`
(query allowed; `count_tokens` excluded), and `tracing::enabled!(DEBUG)`.
Otherwise nothing is parsed and no state is kept. `RUST_LOG` is fixed at
process start, so there is no partial-state toggle problem.

### Module `src/cache_trace.rs`

```rust
pub struct Tracker { inner: Mutex<Inner> }          // lives in AppState
pub struct SendTrace { /* request-side fields of the line */ }
pub struct InflightGuard { /* key, seq, outcome, emitted, Arc<Tracker> */ }

pub fn applies(path: &str) -> bool;                 // /v1/messages[?...]
pub fn on_send(t: &Arc<Tracker>, client: &str, body: &[u8], compress_on: bool)
    -> Option<(SendTrace, InflightGuard)>;
impl InflightGuard {
    pub fn set_status(&mut self, status: u16);
    pub fn complete(&mut self, usage: Option<&UsageDetail>);   // writes the line
}
impl Drop for InflightGuard { /* decrement; rollback on failure; write line if not emitted */ }
```

`on_send` parses the body with `serde_json::from_slice`, computes the
per-block hashes and the canonical bytes of tools and system blocks outside
the lock, then takes the lock once for the compare-and-update described
under Definitions. Lock hold time is bounded by the block comparisons
(microseconds); hashing is never done under it.

### Tracker state

Two tables, both `lru::LruCache` (already a dependency, used by the compress
memo):

- **Previous-request table**, cap 64, key (`client`, request `model`), value:
  the three hash sequences plus the canonical bytes of tools and system
  blocks (at most ~150 KB per slot). Needed for `diverged=`. Full table
  evicts the least recently used client+model; the only effect is that its
  next request logs `diverged=first`.
- **Prefix table**, cap 1024, key (request `model`, `prefix_fp`, `bp`
  tools segment, `bp` system segment), value `{ last_seq, last_sent,
  prev: Option<(seq, Instant)>, inflight }`. Every send `get_mut`s the entry,
  which promotes it, so LRU order is send recency; no second index is
  needed. Eviction on insert when full: loop `peek_lru`; if its
  `inflight == 0`, `pop_lru` and insert; otherwise `pop_lru` and `push` it
  back (it becomes most recent, which is harmless: it is in use) and
  continue; after `cap` iterations with nothing evictable, refuse the
  insert and return `prefix_seen_ago_ms=-`, `inflight_same_prefix=0` for
  this request. That case needs 1024 distinct prefixes in flight at once and
  does not occur in practice; it is handled so the request path can never
  stall on the table.

`seq` is a process-wide monotonic counter assigned per send; rollback
compares sequence numbers, not `Instant`s.

### Line format

```
cache trace client=claude-cli model=claude-opus-5-5 req_id=msg_01… status=200 streaming=true compress_on=1 tools_count=38 system_blocks=3 msgs=27 tools_fp=9f3a… system_fp=… prefix_fp=… bp=tools:none,system:last,msgs:last ttl=5m bp_ttls=5m input=1200 read=0 creation=142579 creation_5m=142579 creation_1h=0 diverged=system:1@45 prefix_seen_ago_ms=4210 inflight_same_prefix=0
```

| field | source | notes |
|---|---|---|
| `client` | user-agent product token (`client_label`) | `none` for the runtime, unchanged in this sub-project; renaming to `unknown` touches all three lines in one separate commit |
| `model` | response `model`; request `model` when the response has none (non-2xx, or absent) | billing follows the response model; the report groups by this |
| `req_id` | `message_start.message.id` (SSE) or root `id` (JSON) | `-` when absent |
| `status` | HTTP status of the final upstream attempt | |
| `streaming` | response content-type is `text/event-stream` | |
| `compress_on` | `state.compress && compress::should_compress(path, provider)` | 1 when the compress stage ran for this request, whether or not it rewrote anything; lets a later A2-style check compare fingerprints |
| `tools_count` | entries in `tools` | 0 when absent |
| `system_blocks` | system blocks | 1 for a string `system`, 0 when absent |
| `msgs` | entries in `messages` | |
| `tools_fp` `system_fp` `prefix_fp` | Definitions | 16 hex |
| `bp` `bp_ttls` `ttl` | Definitions | |
| `input` `read` `creation` | `usage.input_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens` | `-` when no usage was seen |
| `creation_5m` `creation_1h` | `usage.cache_creation.ephemeral_5m_input_tokens` / `ephemeral_1h_input_tokens` | `-` when the `cache_creation` object is absent |
| `diverged` `prefix_seen_ago_ms` `inflight_same_prefix` | Definitions | |

Hashes and offsets only; no block content is ever logged.

### Response side

`UpstreamUsage` gains `id: Option<String>`, `model: Option<String>`,
`ephemeral_5m: Option<u64>`, `ephemeral_1h: Option<u64>`, read from the same
container (`message_start.message` for SSE, the root for JSON). Its existing
`log` and the `upstream usage` line are unchanged.

**The guard is the only writer of the line.** The handler and `UsageTap`
only call it:

- non-2xx: the handler calls `guard.set_status(status)` then
  `guard.complete(None)` and drops the guard. The line goes out with all
  usage fields `-`. The guard is not handed to `UsageTap`.
- 2xx: the handler calls `set_status` and moves the guard into `UsageTap`.
  When the tap finds the usage (SSE `message_start`, or the JSON body at
  EOF) it calls `guard.complete(Some(&usage))`. If the tap finishes without
  usage (first frame not `message_start`, buffer cap exceeded, stream error)
  or is dropped before that (client went away), the guard's `Drop` writes
  the line with usage fields `-`.

`complete` sets an `emitted` flag; `Drop` writes only when the flag is
clear. Exactly one line per request follows from that single owner.

### Failure handling

- **Fail-open.** Body not JSON, not an object, or any other parse problem:
  `on_send` returns `None`, no line, no table update, request forwarded
  untouched. The trace never changes a request or a response.
- **Failed sends must not pollute recency.** A 401, 429, 529 or connection
  error never ran the model and wrote no cache entry; if its timestamp stayed
  in the prefix table, the next same-prefix cold start would be mis-bucketed
  as `sequential`. On drop with a non-2xx outcome (or a send error, which is
  a drop before any status), after decrementing `inflight`: if the entry's
  `last_seq` is still this guard's `seq` (nothing was sent since), restore
  `last_sent` from `prev`; if `prev` is empty this was the key's first send
  and nothing later touched it, so remove the entry. Rollback is one
  level deep: `prev` is cleared after the restore.
  - Consecutive failures F1, F2 on a key last seen at t0: F1 rolls back to
    t0; F2 sees t0 (the value from before F1; F1's timestamp is erased), and
    rolls back to t0 again. The log never shows F1's timestamp.
  - Accepted boundary: F1 sent, S2 sent while F1 is in flight (S2 is
    bucketed `concurrent`, which under B3 is still right: it would have waited
    on F1), F1 fails: `last_seq` is S2's, so no rollback. If S2 then fails
    too, it rolls back to `prev`, which is F1's timestamp, and a third send
    is judged against a failed send. One level is the design; this case is
    documented, not handled.
- **401 retry.** The retry re-sends bytes identical to `final_body` (see
  Assumptions), so the first attempt's `SendTrace` and guard are reused;
  `set_status` is called with the final attempt's status. One line.
- **Tables full.** Covered under Tracker state; never blocks a request.

### Report `scripts/cache-trace-report.sh`

Same conventions as `compress-summary-report.sh`: first argument is the log
path (default `~/Library/Logs/mur-model-gateway/proxy.log`),
`MUR_REPORT_CLIENT` filters one client, every grep is guarded against an
empty match. Only `cache trace` lines are read. Output, one section per
`model=`:

1. **Totals**: lines, lines with usage, cold starts, total `creation`,
   `creation_5m`, `creation_1h`, weighted; per client within the model.
2. **Cold-start buckets**: for each of `first`, `concurrent`, `sequential`,
   `expired`, `no_prefix_bp` (with its three sub-classes): count, `creation`,
   5m tokens, 1h tokens, weighted, `fallback` count, and share of the model's
   total weighted writes.
3. **Avoidable**: `concurrent + sequential` raw 5m, raw 1h, weighted, share
   of the model's writes; the `no_prefix_bp` within-5m figure on its own
   line labelled as B1's target.
4. **Sanity**: lines where `ttl` is not the last element of `bp_ttls`
   (expect 0), lines where `bp_ttls` has `5m` before `1h` (expect 0), lines
   with `status` not 2xx, lines with `read=-`, lines violating the
   `prefix_seen_ago_ms=-` / `inflight>0` invariant (expect 0).

### Cost

Per traced request: one `serde_json::from_slice` of the final body plus a
canonical re-serialization of every block (about the body's size in bytes)
and FNV over it. Expected single-digit milliseconds per MB, an order below
the compress stage's 60 ms average. The implementation plan measures it on
a 1 MB fixture and records the number; no timing field is added to the line.
Memory: at most 64 × ~150 KB for the previous-request table plus 1024 small
prefix entries.

## Assumptions

Recorded because the design leans on them; each has a test.

1. **Billing-prefix injection is deterministic.** `on_send` runs once and the
   401 retry reuses its result, which is only valid if
   `inject_billing_prefix(body_bytes, ver)` yields byte-identical output on
   the retry. The private implementation takes only the body and
   `cc_version`; if it ever adds a nonce or a timestamp, the assumption
   breaks and `on_send` would have to run per attempt (and then a failed
   first attempt must not touch the table). Integration test: a request that
   triggers the 401 retry arm reaches the mock upstream twice with
   byte-identical bodies, and the log holds exactly one `cache trace` line
   for it.
2. **Canonical hashing.** Fingerprints are over serde_json's re-serialization,
   so they ignore JSON whitespace and key order and see every content byte.
   Unit test: two bodies with the same content and different key order give
   the same `prefix_fp`; one changed character in a system block changes
   `system_fp` and `prefix_fp`.

## Testing

- **`cache_trace.rs` unit tests** on JSON fixtures: fingerprint determinism
  and key-order insensitivity (Assumption 2); `bp`, `bp_ttls`, `ttl` for six
  shapes (no `cache_control`; tools only; `system:last`; explicit messages
  only; top-level automatic; `1h` then `5m` mixed); `diverged` values
  `first`, `none`, tools and system offsets, message index, `@+`, `@-`;
  Tracker semantics: A, B, A, B interleaving gives the third send the first
  send's timestamp; two concurrent sends on one key give the second
  `inflight_same_prefix=1` and `prefix_seen_ago_ms` near 0; eviction never
  removes an `inflight>0` entry and refuses when none is evictable; failure
  rollback (single failure, consecutive failures, the documented
  F1/S2 boundary).
- **`lib.rs` tap tests**: extend the `message_start` fixture with `id`,
  `model` and a nested `cache_creation` object; assert the four new
  `UpstreamUsage` fields for SSE and for a JSON body; `Absent` and the
  buffer cap produce one line with usage `-` via the guard.
- **Integration `tests/cache_trace.rs`**, using the global log capture
  pattern of `tests/client_disconnect.rs` and httpmock: two requests with
  the same tools+system give a second line with numeric
  `prefix_seen_ago_ms` and `diverged=msgs:*`; changing one character in the
  middle of the second system block gives `diverged=system:1@<offset>`; a
  429 followed by a same-prefix request shows the rolled-back value; a
  `count_tokens` request produces no line; compress off still produces the
  line with `compress_on=0`; the 401 retry case of Assumption 1.
- **Report**: `tests/fixtures/cache-trace/sample.log`, a hand-written log of
  a dozen lines covering every bucket and sub-class across two models and
  both clients, with the fallback rule exercised; a `#[test]` runs the
  script on it and compares against an expected output file.

## Estimated size

`src/cache_trace.rs` ~500 lines plus tests; `src/lib.rs` ~80 lines
(`AppState` field, hook, `UpstreamUsage` fields, tap wiring);
`scripts/cache-trace-report.sh` ~150 lines; integration test and fixtures
~300 lines.

## Out of scope, decided elsewhere

- Renaming `client=none` across the three log lines (one separate commit).
- `sent_ttl` on the line (with B1).
- A: runtime breakpoint layout, brainstormed in the mur repo once this
  trace has run for a day.
- B1 and B3: gateway-side breakpoint injection and cold-start coalescing,
  sized from this report.
