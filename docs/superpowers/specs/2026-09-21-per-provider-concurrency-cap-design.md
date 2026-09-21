# Per-provider upstream concurrency cap — design

**Date:** 2026-09-21
**Status:** decided (brainstorm complete, plan in
`docs/superpowers/plans/2026-09-21-per-provider-concurrency-cap.md`)

## Problem

A MUR fleet is N processes proxying through one gateway. No worker can see
its siblings' in-flight calls, so a six-member fleet dials the same provider
at once and the whole batch dies — local connect refusals, hangs until the
step gives up, upstream 429s throughout, and no recorded reason (mur
`fleet develop-rust`, 2026-09-09). mur-core capped its own fan-out
(`MUR_FLEET_FANOUT`, default 3), but that is per-process: two fleets, or a
fleet plus an interactive session, still add up.

The gateway is the only place that sees every caller, so the cap lives here.

## Decisions

### 1. When a permit is unavailable: bounded wait, then a local 429

`acquire()` is wrapped in a timeout. On expiry the gateway synthesises a
`429 Too Many Requests` itself with a `retry-after` header and a body that
names the gateway cap as the cause (not the upstream).

Why not the alternatives:

- **Unbounded queue** (the uncommitted draft's behaviour): six steps against
  cap 2 leave four silently hung until the client gives up — the 2026-09-09
  failure mode, just relocated.
- **Immediate 429 (`try_acquire`)**: too jittery; two requests 1 ms apart
  bounce one of them and burn a retry for nothing.

Why 429 specifically: mur's `llm/mod.rs::from_status` already classifies 429
as `RetryThenAdvance` and `durable/rate_limit.rs` reads `retry-after`. The
existing caller chain handles it with no changes. The error has a name, so
"not one recorded reason" cannot recur.

#### The permit lives as long as the response body, not the function

`forward()` returns the moment upstream headers arrive; on streaming calls
the body it hands back is `Body::from_stream(...)` (`src/lib.rs:1175`,
`:1219`) and the upstream connection stays open for as long as tokens keep
coming. A permit scoped to the function therefore guards only the
connect-to-first-byte window — for agentic traffic, a few hundred
milliseconds out of a response that streams for tens of seconds. Six fleet
steps against cap 2 would still hold six upstream streams at once.

So the permit is an `OwnedSemaphorePermit` and it moves *into* the response
body: on the non-streaming path it is dropped when `forward()` builds the
final response as today; on both streaming paths (Codex translation and raw
proxy) it is carried inside the stream's state and released when the stream
ends — upstream EOF, transport error, or the client disconnecting and the
body being dropped. "Concurrent upstream calls" means concurrent open
connections, which is what the provider actually counts.

Consequence for the numbers in §2: with slow streams a permit can be held
for the whole 600 s upstream timeout, so `retry-after: 5` is a hint, not a
promise. That is acceptable for the same reason fixed `retry-after` is —
mur's jittered backoff and three-attempt cap bound the caller's total wait
regardless.

### 2. Numbers

| Knob | Value | Reason |
|---|---|---|
| Cap default | unlimited (`None`) | A solo agent must not queue for no reason. Fleet users opt in. Flipping the default is a later, separate decision. |
| `MUR_MODEL_GATEWAY_MAX_CONCURRENCY` | unset / `0` / unparsable → unlimited + `warn!` | Same spirit as `MUR_FLEET_FANOUT`: garbage never means "unbounded by accident" — here it means "unchanged", and says so in the log. |
| Queue wait | 30 s, `MUR_MODEL_GATEWAY_QUEUE_TIMEOUT_SECS`; `0` / unparsable → 30 + `warn!` | Matches mur's retry `max_delay`. Longer than that and the fleet step has already waited too long elsewhere. |
| `retry-after` | fixed `5` | A permit usually frees within seconds; mur's full-jitter backoff spreads retries on its own. Dynamic (mean hold time of current holders) is more accurate but adds a metric to maintain — revisit only if small cap + slow upstream shows retries re-colliding. |

### 3. Local 429 shape

```
HTTP/1.1 429 Too Many Requests
retry-after: 5
content-type: application/json

{"error":{"type":"gateway_concurrency_cap","message":"mur-model-gateway: anthropic concurrency cap (2) held for 30s; not an upstream rate limit","provider":"anthropic","cap":2,"waited_secs":30}}
```

`provider` and `cap` are in the body so mur-side logs can classify without
parsing the message string.

### 4. Caller-side boundaries (constraints, not changes)

- mur only sets `connect_timeout 10s` (`mur-agent-runtime/src/llm/mod.rs:34`);
  there is no overall request timeout. The gateway's queue timeout is the
  only thing bounding the wait.
- Gateway upstream timeout is 600 s (`src/lib.rs:47`); the queue wait is
  separate and precedes it.
- mur retry backoff: max 30 s, max 3 attempts (`companion/mod.rs:358`).

## Out of scope

- Retrying upstream 429s inside the gateway (still passed through verbatim).
- A capped default.
- Dynamic `retry-after`.
- Per-provider different caps (one number for all four semaphores).
