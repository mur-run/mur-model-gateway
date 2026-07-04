# cc-proxy: multi-provider support (OpenAI + Gemini)

**Date:** 2026-07-04
**Status:** Draft
**Scope:** cc-proxy only. Zero changes to the mur repo.

## Problem

cc-proxy currently targets only Anthropic's API. It provides three services on
that traffic:

1. **Passthrough routing** — forward requests to `api.anthropic.com`.
2. **Disguise** — inject OAuth tokens + billing headers so requests present as
   Claude Code.
3. **Wire-level CCR compression** — compress `tool_result` blocks in
   `/v1/messages*` request bodies through `mur-compress`, sharing the
   `~/.mur/compress` store so `mur_retrieve` recovers originals.

None of this works for OpenAI or Gemini traffic. The compression service in
particular is valuable for any LLM provider: tool results (command output, file
contents, search results) dominate token spend regardless of which model
processes them. cc-proxy already sits at 127.0.0.1:8088 — if it understood
OpenAI and Gemini request shapes, it could compress their tool results with no
new infrastructure.

## Decision

Extend cc-proxy to route and compress traffic for Anthropic, OpenAI, and Gemini
through a single instance. Path-based auto-detection selects the upstream;
compression is dispatched to provider-aware extractors that understand each
API's tool-result JSON structure. Disguise remains Anthropic-only.

Rejected alternatives:

- **Multiple proxy instances on different ports** — operational overhead (three
  services to install/start/stop), and clients must know which port to target.
  Path-based routing means every client just points at `127.0.0.1:8088`.
- **Shell out to provider-specific compressors** — same objection as the
  original mur-compress spec: process spawn on the hot path.
- **Response compression** — CCR is inherently request-direction: compress
  tool results before they reach the model, store the original, let the model
  retrieve if needed. Response bodies are model output — there's nothing to
  store-and-retrieve. Out of scope unless a concrete use case appears.

## Why it works

1. **Provider APIs have orthogonal paths.** `/v1/messages` (Anthropic),
   `/v1/chat/completions` (OpenAI), `/v1beta/models` (Gemini) never collide.
   Path-based dispatch is deterministic and zero-config.
2. **`mur-compress` is content-type agnostic.** It compresses text blobs; it
   doesn't care which API carried them. The same engine, store, and retrieval
   path work for all three providers.
3. **Auth is simpler for non-Anthropic providers.** OpenAI and Gemini clients
   already send `Authorization: Bearer <key>` — the proxy just passes it
   through. No disguise, no token injection, no billing prefix.

## Design

### Routing

New `Provider` enum, derived from the request path:

| Path pattern | Provider | Default upstream |
|---|---|---|
| `/v1/messages`, `/v1/messages/*` | `Anthropic` | `https://api.anthropic.com` |
| `/v1/chat/completions`, `/v1/embeddings`, `/v1/models`, `/v1/images/*`, `/v1/files/*`, `/v1/threads/*`, `/v1/assistants/*` | `OpenAI` | `https://api.openai.com` |
| `/v1beta/models/*` | `Gemini` | `https://generativelanguage.googleapis.com` |
| Anything else | `Anthropic` (backwards-compatible fallback) | |

Environment variables, each optional with the default above:

```
CC_PROXY_UPSTREAM_ANTHROPIC
CC_PROXY_UPSTREAM_OPENAI
CC_PROXY_UPSTREAM_GEMINI
```

The existing `CC_PROXY_UPSTREAM` is kept as a fallback: if a provider-specific
var is unset AND `CC_PROXY_UPSTREAM` is set, it overrides the default for all
three providers. This preserves backward compatibility for single-provider
deployments.

`AppState` holds the three resolved URLs. A new method `upstream_for(path)`
returns the correct one.

### Compression — provider-aware extractors

The shared `CompressEngine` and `compress_text()` helper are unchanged. Three
extractor functions traverse each provider's JSON shape:

#### Anthropic (existing, unchanged)

Path: `messages[].content[]` where `type == "tool_result"`

Two content shapes, both handled:
- String: `"content": "output"`
- Array of text blocks: `"content": [{"type": "text", "text": "output"}]`

Sibling fields (`tool_use_id`, `is_error`, `cache_control`) survive because
mutation is in-place — only the text payload is swapped.

#### OpenAI (new)

Path: `messages[]` where `role == "tool"`

Content shapes:
- String: `"content": "output"` (dominant case)
- Array of content parts: `"content": [{"type": "text", "text": "output"}]` (rare but valid)

The `tool_call_id` sibling is preserved.

#### Gemini (new)

Path: `contents[].parts[]` where has key `functionResponse`

The `functionResponse` object contains:
```json
{"name": "bash", "response": {"result": "output"}}
```

Two response shapes:
- Object with `result` key: `{"result": "big output"}`
- Plain string: `"response": "big output"`

Only the `result` / string value is compressed. The `name` field is preserved.

### Disguise — Anthropic-only, unchanged

`should_disguise()` already gates on `/v1/messages*` paths. The only change is
an additional guard: `provider == Provider::Anthropic`. OpenAI and Gemini
traffic passes through with the client's own auth headers intact.

### Data flow (updated `forward()`)

```
1. Buffer body bytes
2. Detect provider from path
3. If compression enabled AND path matches provider's tool-result endpoint:
   → rewrite_request_body(body, provider)  // dispatch to correct extractor
4. If provider == Anthropic AND on messages path:
   → resolve token, inject billing prefix (existing disguise logic)
5. Forward to upstream_for(path)
6. Stream response back
```

Steps 3 and 4 are independent — Gemini/OpenAI skip disguise but still get
compression.

### Skip rules (unchanged)

- Blocks under `min_tokens` (from mur's `AutoCfg`).
- Blocks already carrying a retrieve marker (`hash=<hex>`).
- Non-JSON bodies → passthrough.

### Failure behavior (unchanged)

Any error (JSON parse, engine init, store write, unknown provider) → forward
the original body untouched, log at debug. Fail-open. Compression must never
block or corrupt a request.

### Rollout gate (unchanged)

`CC_PROXY_COMPRESS=1` env var, default off. The gate applies to all three
providers uniformly — when on, all tool-result-shaped blocks are candidates.

## Testing

Unit (in `compress.rs`):

- **Anthropic** — existing tests unchanged.
- **OpenAI** — `role: "tool"` string content → compressed; idempotent second
  pass no-op; `tool_call_id` preserved.
- **Gemini** — `functionResponse` with object result → compressed;
  `functionResponse` with string response → compressed; `name` preserved.
- **Cross-provider** — a body with only OpenAI messages on a Gemini path →
  passthrough (no tool_result blocks found).
- **Malformed JSON** → byte-identical passthrough for all three.

Integration (new or expanded):

- Three synthetic requests (one per provider) through the full proxy, each
  carrying a fat tool result. Verify status 200, body smaller, marker present.
- Verify OpenAI/Gemini auth headers pass through unmodified.
- Verify Anthropic disguise still fires (regression).

## Non-goals (YAGNI)

- Response body compression.
- Disguise/auth injection for OpenAI or Gemini.
- Streaming (SSE) compression.
- Gemini `v1alpha` path variant (add later if needed).
- Per-provider compression on/off toggles (one gate is enough until proven
  otherwise).

## Estimated size

~200 lines delta:
- `lib.rs`: +30 (upstream map, provider detection, routing call site)
- `compress.rs`: +140 (two new extractors, expanded `should_compress`,
  provider-aware dispatch)
- `main.rs`: +15 (new CLI/env plumbing for provider-specific upstream vars)
- Tests: +80 (provider-specific unit tests)
