# mur-model-gateway × mur-compress: wire-level tool_result compression

**Date:** 2026-07-03
**Status:** Implemented (env-gated `MUR_MODEL_GATEWAY_COMPRESS=1`, default off)
**Scope:** mur-model-gateway only. Zero changes to the mur repo.

## Problem

`headroom wrap claude` compresses LLM traffic at the wire level by proxying the
Anthropic API. MUR's compression today is hook-level only: the Claude Code
PostToolUse hook and the mur MCP server compress tool outputs at the source.
That leaves uncovered every client that dials Anthropic without those hooks —
notably the mur agent runtimes, which all route through mur-model-gateway
(127.0.0.1:8088). mur-model-gateway already sits in the path of ALL local Anthropic
traffic; adding compression there gives universal coverage with one change.

## Decision

Link the `mur-compress` crate into mur-model-gateway and compress `tool_result` blocks
in `/v1/messages*` request bodies, sharing the `~/.mur/compress` store so the
existing `mur_retrieve` MCP tool recovers originals with no new plumbing.

Rejected alternatives:

- **Shell out to `mur compress` per block** — process spawn per tool_result on
  the hot path; latency and failure modes for no gain.
- **Fold the proxy into mur (`mur wrap`)** — headroom-parity product feature,
  but requires merging mur-model-gateway's auth-disguise layer into mur. Possible later
  evolution; out of scope here.

## Why it works

1. **`mur-compress` is a standalone workspace crate** with a clean API:
   `CompressEngine::new(store_dir, cfg)` → `.compress(text, query)` →
   `.retrieve(hash, query)`. Both projects are local Rust; a path dependency
   suffices.
2. **Hashes are content-derived** (`blake3::hash(content)`,
   `mur-compress/src/ccr/store.rs`). Compression is deterministic: Claude Code
   resends the whole transcript every turn, the proxy re-compresses identical
   bytes to identical output, so the rewritten prefix is byte-stable across
   turns and **Anthropic prompt caching survives**.
3. **Retrieval is already end-to-end**: writing to the shared
   `~/.mur/compress` store means the model can recover any compressed block
   via the existing `mur_retrieve` MCP tool.

## Design

### Dependency & wiring

- `Cargo.toml`: `mur-compress = { path = "../mur/mur-compress" }`.
- New module `src/compress.rs`. One `CompressEngine` rooted at
  `<mur_home>/compress` with `CompressConfig::load(mur_home)` — same store,
  same thresholds as mur itself; no new config surface. Savings land in the
  shared stats (`mur compress stats`, per-day buckets).

### Data flow

In `forward()` (`src/lib.rs`), after body buffering, only on `/v1/messages*`
(reuse the existing path check):

1. Parse the JSON body.
2. Walk `messages[].content[]`; for each `tool_result` block, extract text
   (both the string form and the `[{type:"text"}]` array form).
3. If over `min_tokens` (from mur's `AutoCfg`), compress and replace the text
   in place, preserving **all sibling fields** — `tool_use_id`, `is_error`,
   and especially `cache_control` (Claude Code's cache breakpoints must
   survive).
4. Re-serialize, then hand to the existing disguise step.

Responses are untouched (SSE streams pass through as today).

### Skip rules

- Blocks under `min_tokens` — this naturally skips already-compressed hook
  output, whose marker text is small.
- Belt-and-suspenders: skip blocks matching the retrieve-marker pattern
  (`hash=<hex>`), so a fat-but-already-compressed block is never
  double-offloaded.
- `system`, user text, assistant turns: never touched. `tool_result` only.

### Failure behavior

Any error (JSON parse, engine init, store write) → forward the original body
untouched, log at debug. Compression must never block or corrupt a request.
Fail-open.

### Rollout gate

`MUR_MODEL_GATEWAY_COMPRESS=1` env var, default **off**. All local Anthropic traffic
(Claude Code + every mur agent) flows through this proxy — opt in for the
first live period; flip the default once proven.

### Retrieval caveat

Clients without the mur MCP server cannot call `mur_retrieve`; for them the
compressed summary is all the model sees. Claude Code has it. Before flipping
the default on, verify which mur agents have the mur MCP server configured.

## Testing

Unit (in mur-model-gateway):

- Fat `tool_result` → smaller body, marker present, hash retrievable from the
  store.
- Idempotence: second pass over already-compressed body is a no-op.
- Malformed JSON → byte-identical passthrough.
- `cache_control` and sibling fields preserved.

Live:

- One Claude Code turn through the proxy with `MUR_MODEL_GATEWAY_COMPRESS=1`; confirm
  `mur compress stats` delta and that `mur_retrieve` recovers the content.

## Estimated size

~150 lines in mur-model-gateway.
