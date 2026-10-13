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
previous request, prefix and tools recency, in-flight count, byte sizes) is
computed at send time; the response side (usage, id, model, status) is added
when the response's usage arrives, and the two halves go out as a single
line. A new script, `scripts/cache-trace-report.sh`, aggregates the line into
"avoidable write tokens" per model. The existing `upstream usage` and
`compress summary` lines are unchanged.

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

The blocks of `tools` and `system`, in document order, up to and including
the last breakpoint inside those two sections, in the body handed to reqwest
(after compression and after `disguise::inject_billing_prefix`). That is the
unit upstream actually caches: with `system:1` on a three-block system the
prefix is tools plus system blocks 0 and 1, and a change to block 2 does not
touch it; with `tools:last,system:none` the prefix is the tools alone. When
the two sections hold no breakpoint, the prefix is all of tools plus system,
the unit B1 would create. Messages are never part of the prefix.

### Fingerprints

Per-block hash: FNV-1a 64 over the block's canonical bytes, where canonical
means the block re-serialized by `serde_json::to_vec` (compact, object keys
sorted because the crate is built without `preserve_order`). Consequences,
by design:

- insensitive to JSON whitespace and object key order, sensitive to every
  content byte. Upstream hashes the rendered prompt, not the JSON text, so
  this is the closer model. Byte-exact hashing of the original body would
  need block byte ranges, which serde_json does not expose.
- `tools_fp` = FNV-1a over the sequence of all tool hashes; `system_fp`
  likewise over all system block hashes (a string `system` counts as one
  block). Both cover their whole section and are diagnostic.
- `prefix_fp` = FNV-1a over the hashes of exactly the prefix blocks as
  defined above. It is the cache-unit identity.
- `ctx_fp` = FNV-1a over the canonical bytes of the request settings that
  the API documents as invalidating the system or tools cache without any
  change to their content: `thinking`, `output_config.effort`, `speed`, and
  a boolean "any `citations.enabled: true` inside `messages`". Absent
  settings hash as absent. Web search is a tool and is already inside
  `tools_fp`. The thinking and effort rows are model-specific in the
  documentation (they invalidate tools and system only on models that render
  the configuration ahead of them); the key treats them as always
  invalidating, which can only under-count avoidable writes, never inflate
  them.
- All printed as 16 hex digits. FNV-1a is implemented inline (no crate) and
  has no per-process key, so fingerprints are comparable across gateway
  restarts.

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

`bp_ttls=` lists, in document order, the TTL of every breakpoint inside
tools and system: `5m` or `1h`, with an omitted `ttl` recorded as its
effective value `5m`. `-` when there is none. It is a diagnostic column: the
API requires 1h entries before 5m ones, and the report checks the order.

`ttl=` is the TTL of the last breakpoint inside tools and system, i.e. the
last element of `bp_ttls`, or `-` when there is none. It is the client's
original value; a gateway-sent TTL (`sent_ttl`) is added only when B1
exists.

`ttl_max=` is the highest TTL among every breakpoint in the request,
messages and the automatic last block included, or `-` when there is none.
Individual message breakpoint TTLs are not logged. The API requires longer
TTLs to come first, so a breakpoint B1 adds ahead of the client's own
cannot be shorter than any of them: B1's TTL is `ttl_max`, or `5m` when
`ttl_max=-`. The report calls that value `b1_ttl`.

### Byte sizes

All in canonical bytes, so they are comparable with each other and with the
fingerprints, not with the wire body:

- `tools_bytes`, `system_bytes`: the whole section.
- `prefix_bytes`: the prefix blocks only (equals `tools_bytes +
  system_bytes` when the last prefix breakpoint is on the last system block
  or there is none).
- `span_bytes`: every block from the start of the request through the last
  breakpoint anywhere in it (tools, system, messages, or the automatic last
  block). `-` when the request has no breakpoint at all. On a full-cold
  response this is the span that `creation` paid for.

The report uses byte shares of `creation` as token estimates (see Cold
start). Tokens per byte differ between tool schemas, prose and tool
results, so these are estimates and are labelled `est` wherever printed;
the raw `creation` is always printed beside them as the upper bound.

### Divergence `diverged=`

Compared at send time against the previous request sent by the same
`client` for the same request `model`, ordered by send time, without waiting
for responses. Values:

- `first`: no previous request for this client+model since the gateway
  started (or since the slot was evicted).
- `none`: ctx, tools, system and messages all identical.
- `ctx`: `ctx_fp` differs (settings changed; content may be identical).
- `tools:i@off` / `system:i@off`: `i` is the first block whose hash differs,
  `off` the first differing byte of that block's canonical bytes; when one
  side's bytes are a prefix of the other's, `off` is the shorter length.
  When all shared blocks match and the counts differ, `off` is `+` (this
  request has more blocks) or `-` (fewer). `off` is `?` when the previous
  slot kept hashes only (see Tracker state).
- `msgs:i`: first differing message index; no offset, because only message
  hashes are retained.
- `tools:1024+`, `system:64+`, `msgs:4096+`: the difference lies beyond the
  per-section hash cap (see Tracker state), inside the folded tail.

Sections are compared in order ctx, tools, system, messages, mirroring the
cache hierarchy; the first differing section is reported.

### Prefix recency, tools recency, concurrency

Two recency tables with the same entry type.

- **Prefix key**: (request `model`, `ctx_fp`, `prefix_fp`, the `tools` and
  `system` segments of `bp`). The `msgs` segment is excluded on purpose: it
  says nothing about whether the prefix is a cache unit, and a client that
  changes its message breakpoint layout would otherwise split one prefix
  across keys. The `model` is in the key because a cache entry is the model's
  computed KV state; the same prefix on another model is a guaranteed miss.
  `ctx_fp` is in the key because the documented settings change the rendered
  prompt ahead of the content.
- **Tools key**: (request `model`, `ctx_fp`, `tools_fp`). No `bp` segment:
  the question it answers is "would a tools-level entry have existed had
  every request carried a tools breakpoint", which is B1's hypothetical, so
  the client's actual layout is irrelevant.

At send time, under one lock, in this order: read, classify, update, for
both tables.

- `prefix_seen_ago_ms=`: milliseconds since the last send with the same
  prefix key, `-` when the key is new. Every send updates the timestamp, not
  only cold ones, matching the API rule that a cache entry's lifetime
  restarts at the start of any request that writes or reads it.
- `tools_seen_ago_ms=`: the same for the tools key.
- `inflight_same_tools=`: the same count for the tools key.
- `inflight_same_prefix=`: number of sends with the same prefix key whose
  response had not **begun** when this one was sent. The API documents that
  "a cache entry only becomes available after the first response begins",
  so a send that overlaps only with another request's output generation is
  not a race and must not be counted. "Begun" is observed as: the first SSE
  chunk (which carries `message_start`) for streaming responses, the
  response headers for non-streaming ones, or any failure. For a
  non-streaming response the headers arrive after generation, so the window
  is overstated for such clients; both known clients stream, and the
  `streaming` column lets the report see the share affected.

Invariant, for each table: a `-` recency and an in-flight count above zero
never hold together, because an in-flight send inserted the key. The
report's buckets rely on it. If a key ever changes, re-verify the
invariant.

### Cold start, avoidable, buckets

The report counts a response as a **cold start** when `read=0` and
`creation>0`. Cold starts fall into exactly one bucket; rows are evaluated
top to bottom and the first match wins:

| bucket | condition |
|---|---|
| `first` | `prefix_seen_ago_ms=-` (a genuine first send of this prefix) |
| `concurrent` | `ttl!=-` and `inflight_same_prefix>0` (B3's target) |
| `sequential` | `ttl!=-`, `inflight_same_prefix=0`, `prefix_seen_ago_ms` < TTL (same prefix, no hit: cause to investigate) |
| `expired` | `ttl!=-`, `inflight_same_prefix=0`, `prefix_seen_ago_ms` >= TTL |
| `no_prefix_bp` | `ttl=-`; sub-classed `no_cache_control` / `top_level_only` / `msgs_only` |

TTL is 300 000 ms for `5m` and 3 600 000 ms for `1h`.

**Prefix estimate.** On a cold start, `creation` covers the whole span up
to the last breakpoint, which usually includes messages. Had the prefix hit,
the messages part would still have been written, so only the prefix part is
avoidable:

```
prefix_est = creation * prefix_bytes / span_bytes
tools_est  = creation * tools_bytes  / span_bytes
```

**Weighting an estimate.** An estimate is never split by the response's
5m/1h proportion: with mixed TTLs the 1h entries sit ahead of the 5m ones,
so the prefix is written entirely at its own breakpoint's rate and the
messages at theirs. `prefix_est` is weighted at `ttl` (at `b1_ttl` when
`ttl=-`, a diagnostic case) and `tools_est` at `b1_ttl`, the rate B1's
breakpoint would pay. `weighted = tokens * 1.25` for `5m`, `tokens * 2` for
`1h` (multiples of the model's base input price). The response's own
`creation_5m`/`creation_1h` feed only the raw totals and a sanity check.

**Avoidable write tokens (B3 and the sequential investigation)** =
`prefix_est` summed over `concurrent` plus `sequential`. Reported as raw
`creation` (upper bound), `prefix_est`, and its weighted value, so that
savings after B3 can be compared with the bill.

**B1's target** is the tools entry only: B1 adds a tools breakpoint, so a
cold start's system and message writes are not its business even when the
prefix was seen recently. A cold start counts, whatever its bucket, when
its `bp` has `tools:none`, `tools_count>0`, `tools_seen_ago_ms` < `b1_ttl`
and `inflight_same_tools=0` (the earlier request's response had begun, so
under B1 its tools entry would have been readable). Value: `tools_est`,
weighted at `b1_ttl`. This is the runtime's main case (identical tools, a
system block that differs by cwd): the prefix is genuinely new, but a tools
breakpoint would have hit. The report prints the figure with its per-bucket
breakdown and names the part that falls in `concurrent` or `sequential` as
the overlap with the B3 figure: both fixes address those writes, so the two
headline numbers are not additive. Two caveats the report cannot resolve
and B1's own design must: a tools section below the model's minimum
cacheable length gets no entry (the report prints the model's smallest and
median `tools_bytes` next to the figure), and the breakpoint B1 adds is one
of the four the API allows.

The `no_prefix_bp` bucket's `prefix_est` is a diagnostic of that client
shape, not a B1 figure: turning it into savings needs a system breakpoint
as well, which is a B1 extension decided when such a client shows up.

Everything is grouped by `model=`.

**Raw totals and the fallback rule.** The totals section sums the
response's own `creation_5m` and `creation_1h`. When the `cache_creation`
object is absent (`creation_5m=-`), the whole `creation` is attributed by
`ttl=`: `1h` to the 1h column, `5m` or `-` to the 5m column, and the line
is counted in a `fallback` column so pre-split API data is never silently
mixed in.

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
pub struct InflightGuard { /* keys, seq, outcome, begun, emitted, Arc<Tracker> */ }

pub fn applies(path: &str) -> bool;                 // /v1/messages[?...]
pub fn on_send(t: &Arc<Tracker>, client: &str, body: &[u8], compress_on: bool)
    -> Option<(SendTrace, InflightGuard)>;
impl InflightGuard {
    pub fn set_status(&mut self, status: u16);      // also marks begun for non-streaming / failures
    pub fn begun(&mut self);                        // idempotent: decrements inflight once
    pub fn complete(&mut self, usage: Option<&UsageDetail>);   // writes the line
}
impl Drop for InflightGuard { /* begun(); rollback on failure; write line if not emitted */ }
```

`on_send` parses the body with `serde_json::from_slice`, computes the
per-block hashes, the byte sizes and the canonical bytes of tools and
system blocks outside the lock, then takes the lock once for the
compare-and-update described under Definitions. Lock hold time is bounded by
the block comparisons (microseconds); hashing is never done under it.

### Tracker state

Three tables, all `lru::LruCache` (already a dependency, used by the
compress memo):

- **Previous-request table**, cap 64, key (`client`, request `model`), value:
  `ctx_fp`, the three hash sequences, and the canonical bytes of the tools
  and system blocks. Needed for `diverged=`. Two caps make a slot's size a
  constant, not a function of the request: the bytes are kept only while
  tools plus system total at most 512 KB canonical, above which the slot
  stores hashes only and `diverged` offsets print as `?`; and each hash
  sequence keeps at most 1024 tool, 64 system and 4096 message hashes, with
  every block beyond the cap folded into one trailing FNV so a difference
  there still shows, as `tools:1024+`, `system:64+` or `msgs:4096+`. A slot
  is therefore at most 512 KB + 5184 × 8 B ≈ 553 KB, the table at most
  64 × 553 KB ≈ 35 MB, instead of 64 × `MAX_BODY_BYTES` (32 MiB) in the
  worst case; typical slots are ~150 KB. Full table evicts the least
  recently used client+model; the only effect is that its next request
  logs `diverged=first`.
- **Prefix table**, cap 1024, key as defined, and **tools table**, cap
  1024, key as defined; both hold `{ last_seq, last_sent, prev:
  Option<(seq, Instant)>, inflight }`, a few dozen bytes plus the key. Every
  send `get_mut`s the entry, which promotes it, so LRU order is send recency;
  no second index is needed. Eviction on insert when full: loop `peek_lru`;
  if its `inflight == 0`, `pop_lru` and insert; otherwise `pop_lru` and
  `push` it back (it becomes most recent, which is harmless: it is in use)
  and continue; after `cap` iterations with nothing evictable, refuse the
  insert and return `-` for that table's recency (and `0` in-flight) for
  this request. That case needs 1024 distinct keys whose responses have all
  not begun, and does not occur in practice; it is handled so the request
  path can never stall on a table.

`seq` is a process-wide monotonic counter assigned per send; rollback
compares sequence numbers, not `Instant`s. `inflight` counts guards that
have not yet reported `begun`.

### Line format

```
cache trace client=claude-cli model=claude-opus-5-5 req_id=msg_01… status=200 streaming=true compress_on=1 tools_count=38 system_blocks=3 msgs=27 tools_bytes=30534 system_bytes=4120 prefix_bytes=34654 span_bytes=61200 ctx_fp=… tools_fp=9f3a… system_fp=… prefix_fp=… bp=tools:none,system:last,msgs:last ttl=5m bp_ttls=5m ttl_max=5m input=1200 read=0 creation=142579 creation_5m=142579 creation_1h=0 diverged=system:0@45 prefix_seen_ago_ms=- tools_seen_ago_ms=4210 inflight_same_prefix=0 inflight_same_tools=0
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
| `tools_bytes` `system_bytes` `prefix_bytes` `span_bytes` | Definitions, Byte sizes | canonical bytes; `span_bytes=-` without any breakpoint |
| `ctx_fp` `tools_fp` `system_fp` `prefix_fp` | Definitions | 16 hex |
| `bp` `bp_ttls` `ttl` `ttl_max` | Definitions | |
| `input` `read` `creation` | `usage.input_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens` | `-` when no usage was seen |
| `creation_5m` `creation_1h` | `usage.cache_creation.ephemeral_5m_input_tokens` / `ephemeral_1h_input_tokens` | `-` when the `cache_creation` object is absent |
| `diverged` `prefix_seen_ago_ms` `tools_seen_ago_ms` `inflight_same_prefix` `inflight_same_tools` | Definitions | |

Hashes, sizes and offsets only; no block content is ever logged.

### Response side

`UpstreamUsage` gains `id: Option<String>`, `model: Option<String>`,
`ephemeral_5m: Option<u64>`, `ephemeral_1h: Option<u64>`, read from the same
container (`message_start.message` for SSE, the root for JSON). Its existing
`log` and the `upstream usage` line are unchanged.

**The guard is the only writer of the line.** The handler and `UsageTap`
only call it:

- non-2xx: the handler calls `guard.set_status(status)` (which marks the
  response begun and, being a failure, triggers the rollback below) then
  `guard.complete(None)` and drops the guard. The line goes out with all
  usage fields `-`. The guard is not handed to `UsageTap`.
- 2xx: the handler calls `set_status` (for a non-streaming response this
  also marks it begun) and moves the guard into `UsageTap`. For a streaming
  response the tap calls `guard.begun()` on the first chunk it observes.
  When the tap finds the usage (SSE `message_start`, or the JSON body at
  EOF) it calls `guard.complete(Some(&usage))`. If the tap finishes without
  usage (first frame not `message_start`, buffer cap exceeded, stream error)
  or is dropped before that (client went away), the guard's `Drop` writes
  the line with usage fields `-`.

`complete` sets an `emitted` flag; `Drop` writes only when the flag is
clear. Exactly one line per request follows from that single owner.
`begun` is idempotent and `Drop` calls it too, so an in-flight count is
released exactly once on every path.

### Failure handling

- **Fail-open.** Body not JSON, not an object, or any other parse problem:
  `on_send` returns `None`, no line, no table update, request forwarded
  untouched. The trace never changes a request or a response.
- **Failed sends must not pollute recency.** A 401, 429, 529 or connection
  error never ran the model and wrote no cache entry; if its timestamp stayed
  in a recency table, the next same-prefix cold start would be mis-bucketed
  as `sequential`. When the outcome is known to be a failure (a non-2xx
  status, or a drop before any status), in both the prefix and the tools
  table: if the entry's `last_seq` is still this guard's `seq` (nothing was
  sent since), restore `last_sent` from `prev`; if `prev` is empty this was
  the key's first send and nothing later touched it, so remove the entry.
  Rollback is one level deep: `prev` is cleared after the restore.
  - Consecutive failures F1, F2 on a key last seen at t0: F1 rolls back to
    t0; F2 sees t0 (the value from before F1; F1's timestamp is erased), and
    rolls back to t0 again. The log never shows F1's timestamp.
  - Accepted boundary: F1 sent, S2 sent while F1 has not begun (S2 is
    bucketed `concurrent`, which under B3 is still right: it would have
    waited on F1), F1 fails: `last_seq` is S2's, so no rollback. If S2 then
    fails too, it rolls back to `prev`, which is F1's timestamp, and a third
    send is judged against a failed send. One level is the design; this
    case is documented, not handled.
- **401 retry.** The retry re-sends bytes identical to `final_body` (see
  Assumptions), so the first attempt's `SendTrace` and guard are reused;
  `set_status` is called once, with the final attempt's status, so the
  intermediate 401 triggers no rollback. One line.
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
   `expired`, `no_prefix_bp` (with its three sub-classes): count, raw
   `creation`, `prefix_est`, its weighted value at the prefix TTL, and
   share of the model's total weighted writes.
3. **Avoidable (B3 / sequential)**: `concurrent + sequential` raw
   `creation`, `prefix_est`, weighted, share of the model's writes.
4. **B1 target**: the tools line as defined (count, raw `creation`,
   `tools_est`, weighted at `b1_ttl`), its per-bucket breakdown, the
   overlap with line 3, and the model's smallest and median `tools_bytes`.
5. **Sanity**: lines where `ttl` is not the last element of `bp_ttls`
   (expect 0), lines where `bp_ttls` has `5m` before `1h` (expect 0), lines
   where `prefix_bytes > span_bytes` (expect 0), lines where `ttl=1h` yet
   `creation_1h=0` with `creation>0`, or `ttl_max=5m` yet `creation_1h>0`
   (the response's split disagrees with the layout), lines with `status`
   not 2xx, lines with `read=-`, lines with `diverged` offsets `?` or a `+`
   tail, the `fallback` count, lines violating either recency/in-flight
   invariant (expect 0), and the share of non-streaming lines (whose
   in-flight window is overstated).

### Cost

Per traced request: one `serde_json::from_slice` of the final body plus a
canonical re-serialization of every block (about the body's size in bytes)
and FNV over it. Expected single-digit milliseconds per MB, an order below
the compress stage's 60 ms average. The implementation plan measures it on
a 1 MB fixture and records the number; no timing field is added to the line.
Memory: a hard 35 MB bound for the previous-request table (typically a few
MB) plus two tables of 1024 small entries.

## Assumptions

Recorded because the design leans on them; each has a test or a labelled
column.

1. **Billing-prefix injection is deterministic.** `on_send` runs once and the
   401 retry reuses its result, which is only valid if
   `inject_billing_prefix(body_bytes, ver)` yields byte-identical output on
   the retry. The private implementation takes only the body and
   `cc_version`; if it ever adds a nonce or a timestamp, the assumption
   breaks and `on_send` would have to run per attempt (and then a failed
   first attempt must not touch the tables). Integration test: a request
   that triggers the 401 retry arm reaches the mock upstream twice with
   byte-identical bodies, and the log holds exactly one `cache trace` line
   for it.
2. **Canonical hashing.** Fingerprints are over serde_json's re-serialization,
   so they ignore JSON whitespace and key order and see every content byte.
   Unit test: two bodies with the same content and different key order give
   the same `prefix_fp`; one changed character in a system block changes
   `system_fp` and `prefix_fp`.
3. **Byte share approximates token share within one request.** `prefix_est`
   and `tools_est` scale `creation` by canonical byte ratios. Tool schemas,
   prose and tool results tokenize at different rates, so the estimates can
   be off by tens of percent for a single request; the report always prints
   the raw `creation` beside them and labels them `est`. If a later
   sub-project needs exact figures, a `count_tokens` call per distinct prefix
   is the upgrade path, not a change to this line.
4. **"Response begun" is when the entry becomes readable.** From the API
   documentation ("a cache entry only becomes available after the first
   response begins"). For streaming responses the first SSE chunk is that
   moment as seen from the gateway; for non-streaming ones the headers come
   after generation, so the in-flight window is overstated. The `streaming`
   column and the sanity share make the affected lines visible.

## Testing

- **`cache_trace.rs` unit tests** on JSON fixtures: fingerprint determinism
  and key-order insensitivity (Assumption 2); `ctx_fp` changes with
  `thinking`, `output_config.effort`, `speed` and a `citations.enabled`
  document block, and not with `tool_choice` or an image; `bp`, `bp_ttls`,
  `ttl` for six shapes (no `cache_control`; tools only; `system:last`;
  explicit messages only; top-level automatic; `1h` then `5m` mixed);
  prefix cut: `system:1` on three system blocks gives a `prefix_fp` and
  `prefix_bytes` that ignore block 2, `tools:last,system:none` gives the
  tools alone, no prefix breakpoint gives all of both; `span_bytes` through
  a message breakpoint and `-` without any; `diverged` values `first`,
  `none`, `ctx`, tools and system offsets, message index, `@+`, `@-`, and
  `@?` when the previous slot exceeded the 512 KB byte budget, and
  `msgs:4096+` when the difference lies past the hash cap; `ttl_max` across
  tools, system, message and automatic breakpoints; Tracker
  semantics: A, B, A, B interleaving gives the third send the first send's
  timestamp; two sends on one key before the first has begun give the
  second `inflight_same_prefix=1` and `prefix_seen_ago_ms` near 0; a second
  send after the first has begun but before it finished gives
  `inflight_same_prefix=0`; the tools table answers `tools_seen_ago_ms` for
  a request whose prefix key is new, and `inflight_same_tools=1` while that
  earlier request has not begun; eviction never removes an entry whose
  response has not begun and refuses when none is evictable; failure
  rollback in both tables (single failure, consecutive failures, the
  documented F1/S2 boundary).
- **`lib.rs` tap tests**: extend the `message_start` fixture with `id`,
  `model` and a nested `cache_creation` object; assert the four new
  `UpstreamUsage` fields for SSE and for a JSON body; `Absent` and the
  buffer cap produce one line with usage `-` via the guard; `begun` is
  released exactly once when the tap is dropped before the first chunk.
- **Integration `tests/cache_trace.rs`**, using the global log capture
  pattern of `tests/client_disconnect.rs` and httpmock: two requests with
  the same tools+system give a second line with numeric
  `prefix_seen_ago_ms` and `diverged=msgs:*`; changing one character in the
  middle of the second system block gives `diverged=system:1@<offset>`, a
  new prefix key (`prefix_seen_ago_ms=-`) and a numeric `tools_seen_ago_ms`;
  a 429 followed by a same-prefix request shows the rolled-back value; a
  `count_tokens` request produces no line; compress off still produces the
  line with `compress_on=0`; a mock upstream that delays its first SSE chunk
  makes a second concurrent request log `inflight_same_prefix=1`, and one
  that delays only after `message_start` makes it log `0`; the 401 retry
  case of Assumption 1.
- **Report**: `tests/fixtures/cache-trace/sample.log`, a hand-written log
  covering every bucket and sub-class, the B1 line with its overlap and
  per-bucket breakdown, a mixed-TTL request whose `prefix_est` is weighted
  at `ttl` and `tools_est` at `b1_ttl`, the fallback rule, the `?` offset
  and a `+` tail across two models and both clients; a `#[test]` runs the
  script on it and compares against an expected output file.

## Estimated size

`src/cache_trace.rs` ~650 lines plus tests; `src/lib.rs` ~90 lines
(`AppState` field, hook, `UpstreamUsage` fields, tap wiring);
`scripts/cache-trace-report.sh` ~200 lines; integration test and fixtures
~400 lines.

## Out of scope, decided elsewhere

- Renaming `client=none` across the three log lines (one separate commit).
- `sent_ttl` on the line (with B1).
- Exact per-prefix token counts via `count_tokens` (only if the byte-share
  estimates prove too coarse for B1/B3 sizing).
- A: runtime breakpoint layout, brainstormed in the mur repo once this
  trace has run for a day.
- B1 and B3: gateway-side breakpoint injection and cold-start coalescing,
  sized from this report.
