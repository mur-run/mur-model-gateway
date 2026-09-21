# Per-provider concurrency cap — implementation plan

> **Execute with `mur-executing-plans`** (no delegation target holds `cargo`
> for this repo; run tasks sequentially in-context, ship `cargo` commands
> through `fleet_run` when the local sandbox denies the binary).

**Spec:** `docs/superpowers/specs/2026-09-21-per-provider-concurrency-cap-design.md`

**Goal:** Turn the uncommitted `ProviderSemaphores` draft into a shipped,
env-configured feature whose queue wait is bounded and whose overflow is a
named local 429.

**Architecture:** `AppState` already holds an optional per-provider
`Semaphore` set and `forward()` already acquires a permit for the whole
call. This plan (1) wraps that acquire in `tokio::time::timeout` and returns
a synthesised 429 on expiry, (2) parses two env vars in `main.rs` into the
existing builder chain, (3) proves both with integration tests, (4)
documents the knobs.

**Tech stack:** Rust 2021, axum 0.8, tokio 1 (`sync`, `time`), reqwest 0.12
(tests), serde_json 1.

## Global Constraints (copied from spec — every task includes them)

- Cap default is unlimited (`None`); unset env means today's behaviour, byte
  for byte.
- `MUR_MODEL_GATEWAY_MAX_CONCURRENCY`: unset / `0` / unparsable → unlimited
  and a `tracing::warn!` naming the variable and the value.
- `MUR_MODEL_GATEWAY_QUEUE_TIMEOUT_SECS`: default 30; `0` / unparsable →
  30 and a `tracing::warn!`.
- Overflow response is `429`, header `retry-after: 5`, JSON body with
  `error.type = "gateway_concurrency_cap"`, `error.provider`, `error.cap`,
  `error.waited_secs`, and a message that says it is not an upstream limit.
- The permit is an `OwnedSemaphorePermit`. On non-streaming paths it drops
  when `forward()` returns; on both streaming paths it is moved into the
  response stream so the slot is released when the body finishes or is
  dropped, not when headers are sent (spec §1, "The permit lives as long as
  the response body").
- Upstream 429s remain passed through unchanged.
- Commit style: `feat(...)` / `test(...)` / `docs(...)` with a body, matching
  `git log`.

## File structure

| File | Responsibility |
|---|---|
| `src/lib.rs` | `Provider::name()`, `DEFAULT_QUEUE_TIMEOUT`, `AppState.queue_timeout`, `with_queue_timeout`, bounded acquire + `concurrency_cap_response()` in `forward()` |
| `src/main.rs` | `parse_max_concurrency()`, `parse_queue_timeout()`, wiring into `serve()`, unit tests for both parsers |
| `tests/max_concurrency.rs` | existing three tests (unchanged) + `overflow_is_a_local_429_with_retry_after` + `queue_timeout_is_configurable` + `fire_collect` helper |
| `README.md` | two rows in the env table |
| `docs/install.md` | one sentence pointing at the two vars |

---

## Task 0 — Branch and carry the draft

- [x] `cd ~/Projects/mur-model-gateway && git checkout -b feat/per-provider-concurrency-cap`
  (uncommitted `src/lib.rs` edits and untracked `tests/max_concurrency.rs`
  follow the checkout automatically).
- [x] `git add src/lib.rs tests/max_concurrency.rs docs/superpowers/specs/2026-09-21-per-provider-concurrency-cap-design.md docs/superpowers/plans/2026-09-21-per-provider-concurrency-cap.md`
- [x] Commit:
  ```
  feat(concurrency): per-provider upstream semaphore behind with_max_concurrency

  One tokio Semaphore per provider on AppState, acquired for the whole
  of forward(). Default None keeps today's unlimited behaviour; nothing
  in main.rs calls the builder yet. Spec and plan alongside.
  ```
- [x] `git branch --show-current` prints `feat/per-provider-concurrency-cap`; `git status --short` is empty.

Note: the draft's `ProviderSemaphores` holds bare `Semaphore`s and `forward()`
takes a borrowed permit. That is fine to commit as-is; Task 1 converts it to
`Arc<Semaphore>` + `acquire_owned()` because the permit has to outlive the
function (see the streaming test in Step 1.1).

**Interfaces — Produces:** `AppState::with_max_concurrency(self, usize) -> Self` (already in draft, public).

---

## Task 1 — Bounded acquire and the local 429 (`src/lib.rs`)

### Step 1.1 — failing integration test

- [x] Append to `tests/max_concurrency.rs` (after `fire_path`):

```rust
/// Like `fire_path`, but returns every response's status, `retry-after`
/// header (if any) and body, so a test can assert on the overflow shape
/// rather than just counting peak concurrency.
async fn fire_collect(gw: &str, n: usize) -> Vec<(reqwest::StatusCode, Option<String>, String)> {
    let client = reqwest::Client::new();
    let mut handles = Vec::new();
    for _ in 0..n {
        let client = client.clone();
        let url = format!("http://{gw}/v1/messages");
        handles.push(tokio::spawn(async move {
            let resp = client
                .post(&url)
                .header("content-type", "application/json")
                .body(body())
                .send()
                .await
                .unwrap();
            let status = resp.status();
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let text = resp.text().await.unwrap();
            (status, retry_after, text)
        }));
    }
    let mut out = Vec::with_capacity(n);
    for h in handles {
        out.push(h.await.unwrap());
    }
    out
}

/// Spec §1 + §3: with cap 1 and a queue timeout shorter than the upstream
/// hold, the second caller must get a gateway-made 429 — not hang, not a
/// 502 — carrying `retry-after: 5` and a body that names the cap.
#[tokio::test]
async fn overflow_is_a_local_429_with_retry_after() {
    let (upstream, _peak) = spawn_upstream(Duration::from_millis(1500)).await;
    let gw = spawn_gateway_with_queue(&upstream, Some(1), Duration::from_millis(200)).await;

    let results = fire_collect(&gw, 2).await;

    let overflow: Vec<_> = results
        .iter()
        .filter(|(s, _, _)| *s == reqwest::StatusCode::TOO_MANY_REQUESTS)
        .collect();
    assert_eq!(overflow.len(), 1, "exactly one caller should overflow: {results:?}");
    let (_, retry_after, text) = overflow[0];
    assert_eq!(retry_after.as_deref(), Some("5"));
    let json: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(json["error"]["type"], "gateway_concurrency_cap");
    assert_eq!(json["error"]["provider"], "anthropic");
    assert_eq!(json["error"]["cap"], 1);
    assert!(
        json["error"]["message"].as_str().unwrap().contains("not an upstream"),
        "message must disclaim the upstream: {text}"
    );
    assert!(
        results.iter().all(|(s, _, _)| *s != reqwest::StatusCode::BAD_GATEWAY),
        "overflow must never surface as 502: {results:?}"
    );
}
```

- [x] Add the helper next to `spawn_gateway_multi`:

```rust
/// Like `spawn_gateway`, but also sets the queue timeout — needed to make
/// the overflow path fire inside a test's patience.
async fn spawn_gateway_with_queue(
    upstream: &str,
    max_concurrency: Option<usize>,
    queue_timeout: Duration,
) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut state = AppState::new(
        upstream,
        "https://o.invalid",
        "https://g.invalid",
        TokenSource::Disabled,
    )
    .unwrap()
    .with_queue_timeout(queue_timeout);
    if let Some(n) = max_concurrency {
        state = state.with_max_concurrency(n);
    }
    let app = build_router(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(20)).await;
    addr.to_string()
}
```

- [x] Run `cargo test --test max_concurrency overflow_is_a_local_429 2>&1 | tail -20`.
  Expected: compile error `no method named `with_queue_timeout``. That is the red.

- [x] Also append a streaming-hold test. The upstream sends headers at once,
  then trickles chunks for `hold`; the gateway must not release the permit
  when headers arrive.

```rust
/// Upstream that answers headers immediately and then streams the body
/// slowly. `current`/`peak` count *open bodies*, decremented when the
/// stream is fully drained — which is what a provider's concurrency limit
/// actually measures. A permit released at header time would let `peak`
/// exceed the cap here even though `caps_concurrent_upstream_calls_across_many_callers` passes.
async fn slow_stream_handler(State(up): State<SlowUpstream>) -> impl IntoResponse {
    use futures_util::stream;
    let now = up.current.fetch_add(1, Ordering::SeqCst) + 1;
    let mut observed = up.peak.load(Ordering::SeqCst);
    while now > observed {
        match up.peak.compare_exchange(observed, now, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => break,
            Err(v) => observed = v,
        }
    }
    let per_chunk = up.hold / 5;
    let current = up.current.clone();
    let body = stream::unfold(0u8, move |i| {
        let current = current.clone();
        async move {
            if i == 5 {
                current.fetch_sub(1, Ordering::SeqCst);
                return None;
            }
            tokio::time::sleep(per_chunk).await;
            Some((Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"data: {}\n\n")), i + 1))
        }
    });
    (
        StatusCode::OK,
        [("content-type", "text/event-stream")],
        axum::body::Body::from_stream(body),
    )
}

#[tokio::test]
async fn permit_is_held_until_the_stream_is_drained() {
    let (upstream, peak) = spawn_stream_upstream(Duration::from_millis(200)).await;
    let gw = spawn_gateway(&upstream, Some(2)).await;
    fire(&gw, 6).await; // `fire` reads each body to the end
    assert_eq!(
        peak.load(Ordering::SeqCst),
        2,
        "streaming responses must hold the permit until the body ends, not until headers are sent"
    );
}
```

  `spawn_stream_upstream` is `spawn_upstream` with `slow_stream_handler`
  in place of `slow_handler` (factor the router construction if you like).
  `fire_path` currently stops at `assert_eq!(resp.status(), 200)` and drops
  the response, which closes the body early and would release a
  stream-carried permit at once — the test could never fail. Add
  `resp.bytes().await.unwrap();` after the status assert so every caller
  drains its body. (`futures_util` is a regular dependency, so the
  integration test can use it.)

- [x] Run `cargo test --test max_concurrency permit_is_held_until_the_stream_is_drained 2>&1 | tail -20`.
  Expected: `assertion failed` with `peak` > 2 (the draft releases at header time). That is the second red.

### Step 1.2 — minimal code

- [x] In `src/lib.rs`, directly after `pub const DEFAULT_BIND`, add:

```rust
/// How long `forward()` waits for a provider permit before answering with a
/// local 429 (spec §2). Matches mur-core's retry `max_delay`; anything longer
/// and the fleet step has already timed out somewhere else.
pub const DEFAULT_QUEUE_TIMEOUT: Duration = Duration::from_secs(30);

/// `retry-after` value on the local 429. Fixed, not measured: a permit
/// usually frees within seconds and mur's full-jitter backoff spreads the
/// retries on its own (spec §2).
const CONCURRENCY_RETRY_AFTER_SECS: u64 = 5;
```

- [x] Add to `impl Provider` (create the impl block after the enum if none exists):

```rust
impl Provider {
    /// Lower-case wire name, used in the local 429 body and logs.
    pub fn name(self) -> &'static str {
        match self {
            Provider::Anthropic => "anthropic",
            Provider::OpenAI => "openai",
            Provider::Gemini => "gemini",
            Provider::Codex => "codex",
        }
    }
}
```

- [x] In `ProviderSemaphores`, add a field and store it:

```rust
struct ProviderSemaphores {
    cap: usize,
    anthropic: Semaphore,
    openai: Semaphore,
    gemini: Semaphore,
    codex: Semaphore,
}
```
  and in `new`: `Self { cap: max_concurrency, anthropic: ..., ... }`.

- [x] In `pub struct AppState`, after `concurrency`, add:

```rust
    /// Bounded wait for a permit before the local 429 (spec §1).
    /// Only consulted when `concurrency` is `Some`.
    queue_timeout: Duration,
```
  and in `AppState::new`'s struct literal: `queue_timeout: DEFAULT_QUEUE_TIMEOUT,`.

- [x] After `with_max_concurrency`, add:

```rust
    /// Override the bounded wait for a provider permit. Ignored while the
    /// cap is unset. Production reads this from
    /// `MUR_MODEL_GATEWAY_QUEUE_TIMEOUT_SECS`; tests set it directly.
    pub fn with_queue_timeout(mut self, queue_timeout: Duration) -> Self {
        self.queue_timeout = queue_timeout;
        self
    }
```

- [x] Change `ProviderSemaphores` to hold `Arc<Semaphore>` per provider
  (`Arc::new(Semaphore::new(max_concurrency))` in `new`, `for_provider`
  returns `Arc<Semaphore>` by clone). Owned permits need an `Arc` to hang off.

- [x] Replace the `let _permit = match &state.concurrency { ... };` block in `forward()` with:

```rust
    // Acquire before anything else so every exit path — success, error
    // returns via `?`, and the retry branches further down — happens while
    // still holding it.
    //
    // Owned, not borrowed: on streaming paths the permit is moved into the
    // response body below and released when the stream ends, not when this
    // function returns. Headers arrive long before the last token, and the
    // upstream connection is open for all of it — that is the thing the
    // cap is counting.
    //
    // The wait is bounded: past `queue_timeout` we answer with a local 429
    // rather than leave the caller hanging until *its* timeout. mur-core
    // classifies 429 as retry-then-advance and reads `retry-after`, so the
    // existing caller chain handles this without changes.
    let permit: Option<OwnedSemaphorePermit> = match &state.concurrency {
        Some(sems) => {
            let sem = sems.for_provider(provider);
            match tokio::time::timeout(state.queue_timeout, sem.acquire_owned()).await {
                Ok(permit) => Some(permit.context("provider concurrency semaphore closed")?),
                Err(_elapsed) => {
                    tracing::warn!(
                        provider = provider.name(),
                        cap = sems.cap,
                        waited_secs = state.queue_timeout.as_secs(),
                        "concurrency cap: permit not free in time, answering 429"
                    );
                    return Ok(concurrency_cap_response(
                        provider,
                        sems.cap,
                        state.queue_timeout,
                    ));
                }
            }
        }
        None => None,
    };
```

  with `use tokio::sync::OwnedSemaphorePermit;` at the top.

- [x] Add a helper next to `concurrency_cap_response`:

```rust
/// Tie a provider permit to a response stream so the slot frees when the
/// body finishes (EOF, error, or client drop), not when headers go out.
/// `None` (cap disabled) is a no-op wrapper.
fn hold_permit_through<S>(stream: S, permit: Option<OwnedSemaphorePermit>) -> impl futures_util::Stream<Item = S::Item>
where
    S: futures_util::Stream,
{
    use futures_util::StreamExt;
    stream.map(move |item| {
        let _held = &permit;
        item
    })
}
```

  (The closure owns `permit`; it is dropped with the closure when the
  stream is dropped. `map` is the cheapest adaptor that owns state.)

- [x] Codex translation path (`src/lib.rs` around `:1175`): change
  `.body(Body::from_stream(stream))` to
  `.body(Body::from_stream(hold_permit_through(stream, permit)))`.

- [x] Raw proxy streaming path (around `:1219`): change
  `let body = Body::from_stream(stream);` to
  `let body = Body::from_stream(hold_permit_through(stream, permit));`.

- [x] Every non-streaming return path leaves `permit` unmoved; it drops at
  function end as before. If the compiler reports `permit` used after move
  on some branch, that branch reached a streaming return and must not also
  fall through — fix the control flow, do not `clone` a permit.

- [x] Add a free function right after `forward()`:

```rust
/// The gateway-made overflow response (spec §3). Distinguishable from an
/// upstream 429 by `error.type`, and self-describing in the message so a
/// human reading a fleet log does not go looking at the provider's quota.
fn concurrency_cap_response(provider: Provider, cap: usize, waited: Duration) -> Response<Body> {
    let waited_secs = waited.as_secs();
    let body = serde_json::json!({
        "error": {
            "type": "gateway_concurrency_cap",
            "message": format!(
                "mur-model-gateway: {} concurrency cap ({cap}) held for {waited_secs}s; not an upstream rate limit",
                provider.name()
            ),
            "provider": provider.name(),
            "cap": cap,
            "waited_secs": waited_secs,
        }
    });
    (
        StatusCode::TOO_MANY_REQUESTS,
        [
            ("retry-after", CONCURRENCY_RETRY_AFTER_SECS.to_string()),
            ("content-type", "application/json".to_string()),
        ],
        body.to_string(),
    )
        .into_response()
}
```

- [x] `cargo test --test max_concurrency 2>&1 | tail -20` — expected last line
  `test result: ok. 5 passed; 0 failed`.
- [x] `cargo clippy --all-targets -- -D warnings 2>&1 | tail -5` — expected no output before `Finished`.

### Step 1.3 — commit

- [x] ```
  feat(concurrency): bound the permit wait and answer overflow with a local 429

  forward() now waits at most AppState.queue_timeout (default 30s) for a
  provider permit. On expiry it returns 429 + retry-after: 5 with a JSON
  body naming the provider and cap, so mur-core's existing 429 handling
  retries instead of hanging on the caller's own timeout.

  The permit is now owned and rides inside the response stream on both
  streaming paths, so a slot is held for the life of the upstream
  connection rather than until headers are sent.
  ```

**Interfaces — Consumes:** `with_max_concurrency` (Task 0).
**Produces:** `pub const DEFAULT_QUEUE_TIMEOUT: Duration`,
`AppState::with_queue_timeout(self, Duration) -> Self`,
`Provider::name(self) -> &'static str`.

---

## Task 2 — Env wiring (`src/main.rs`)

### Step 2.1 — failing unit tests

- [x] Append to the bottom of `src/main.rs`:

```rust
#[cfg(test)]
mod concurrency_env_tests {
    use super::*;

    #[test]
    fn max_concurrency_unset_is_none() {
        assert_eq!(parse_max_concurrency(None), None);
    }

    #[test]
    fn max_concurrency_positive_parses() {
        assert_eq!(parse_max_concurrency(Some("3")), Some(3));
    }

    #[test]
    fn max_concurrency_zero_is_none() {
        assert_eq!(parse_max_concurrency(Some("0")), None);
    }

    #[test]
    fn max_concurrency_garbage_is_none() {
        assert_eq!(parse_max_concurrency(Some("lots")), None);
        assert_eq!(parse_max_concurrency(Some("")), None);
        assert_eq!(parse_max_concurrency(Some("-2")), None);
    }

    #[test]
    fn queue_timeout_unset_is_default() {
        assert_eq!(parse_queue_timeout(None), DEFAULT_QUEUE_TIMEOUT);
    }

    #[test]
    fn queue_timeout_positive_parses() {
        assert_eq!(parse_queue_timeout(Some("7")), Duration::from_secs(7));
    }

    #[test]
    fn queue_timeout_zero_and_garbage_fall_back() {
        assert_eq!(parse_queue_timeout(Some("0")), DEFAULT_QUEUE_TIMEOUT);
        assert_eq!(parse_queue_timeout(Some("soon")), DEFAULT_QUEUE_TIMEOUT);
    }
}
```

- [x] `cargo test --bin mur-model-gateway concurrency_env 2>&1 | tail -8` —
  expected: compile error `cannot find function `parse_max_concurrency``.

### Step 2.2 — minimal code

- [x] In `src/main.rs` imports: extend the `use mur_model_gateway::{...}` list
  with `DEFAULT_QUEUE_TIMEOUT`, and add `use std::time::Duration;`.

- [x] Add before `fn init_tracing()`:

```rust
/// `MUR_MODEL_GATEWAY_MAX_CONCURRENCY` → per-provider cap. `None` means
/// unlimited (today's behaviour). `0` and unparsable values also mean
/// unlimited — never "some default cap" — but say so, because a silently
/// ignored knob is how the 2026-09-09 fleet pile-up went unexplained.
fn parse_max_concurrency(raw: Option<&str>) -> Option<usize> {
    let raw = raw?;
    match raw.trim().parse::<usize>() {
        Ok(n) if n > 0 => Some(n),
        _ => {
            tracing::warn!(
                value = raw,
                "MUR_MODEL_GATEWAY_MAX_CONCURRENCY must be a positive integer; leaving the cap unlimited"
            );
            None
        }
    }
}

/// `MUR_MODEL_GATEWAY_QUEUE_TIMEOUT_SECS` → bounded permit wait. Unset,
/// `0`, or unparsable → `DEFAULT_QUEUE_TIMEOUT` (30s), with a warning for
/// the latter two.
fn parse_queue_timeout(raw: Option<&str>) -> Duration {
    let Some(raw) = raw else {
        return DEFAULT_QUEUE_TIMEOUT;
    };
    match raw.trim().parse::<u64>() {
        Ok(secs) if secs > 0 => Duration::from_secs(secs),
        _ => {
            tracing::warn!(
                value = raw,
                default_secs = DEFAULT_QUEUE_TIMEOUT.as_secs(),
                "MUR_MODEL_GATEWAY_QUEUE_TIMEOUT_SECS must be a positive integer; using the default"
            );
            DEFAULT_QUEUE_TIMEOUT
        }
    }
}
```

- [x] In `serve()`, after the `MUR_MODEL_GATEWAY_TOKEN_SOURCE_CODEX` block and
  before `let app = build_router(state);`, add:

```rust
    let max_concurrency_raw = std::env::var("MUR_MODEL_GATEWAY_MAX_CONCURRENCY").ok();
    let queue_timeout_raw = std::env::var("MUR_MODEL_GATEWAY_QUEUE_TIMEOUT_SECS").ok();
    let max_concurrency = parse_max_concurrency(max_concurrency_raw.as_deref());
    let queue_timeout = parse_queue_timeout(queue_timeout_raw.as_deref());
    state = state.with_queue_timeout(queue_timeout);
    if let Some(n) = max_concurrency {
        state = state.with_max_concurrency(n);
    }
```

- [x] In the `tracing::info!(... "mur-model-gateway listening")` call, add two
  fields before the message string:

```rust
        max_concurrency = ?max_concurrency,
        queue_timeout_secs = queue_timeout.as_secs(),
```

- [x] `cargo test --bin mur-model-gateway concurrency_env 2>&1 | tail -5` —
  expected `test result: ok. 7 passed`.
- [x] `cargo clippy --all-targets -- -D warnings 2>&1 | tail -3` — clean.

### Step 2.3 — commit

- [x] ```
  feat(concurrency): wire MUR_MODEL_GATEWAY_MAX_CONCURRENCY and _QUEUE_TIMEOUT_SECS

  Unset cap = unlimited, unchanged. 0 or garbage in either variable is
  logged and treated as unset rather than inventing a bound. Both values
  appear in the "listening" line so a fleet log shows what was in force.
  ```

**Interfaces — Consumes:** `DEFAULT_QUEUE_TIMEOUT`, `with_queue_timeout`,
`with_max_concurrency` (Task 1).
**Produces:** nothing downstream depends on these; `parse_*` are private.

---

## Task 3 — Queue timeout is honoured (`tests/max_concurrency.rs`)

### Step 3.1 — failing test

- [ ] Append:

```rust
/// Spec §2: the wait is the configured timeout, not a hard-coded one. With
/// a 1s upstream hold, cap 1, and a 2s queue timeout, the second caller
/// must wait its turn and succeed — no 429 at all.
#[tokio::test]
async fn queue_timeout_is_configurable() {
    let (upstream, peak) = spawn_upstream(Duration::from_millis(1000)).await;
    let gw = spawn_gateway_with_queue(&upstream, Some(1), Duration::from_secs(2)).await;

    let results = fire_collect(&gw, 2).await;

    assert!(
        results.iter().all(|(s, _, _)| *s != reqwest::StatusCode::TOO_MANY_REQUESTS),
        "a 2s queue must outlast a 1s hold: {results:?}"
    );
    assert_eq!(
        peak.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "cap 1 must still serialise the two calls"
    );
}
```

- [ ] `cargo test --test max_concurrency queue_timeout_is_configurable 2>&1 | tail -5`.
  Expected: passes immediately (Task 1 already honours the field). Record
  that in the commit body — this test guards against a future hard-coded
  timeout, which is why it exists even though it is green on arrival.
- [ ] Temporarily change `state.queue_timeout` in `forward()`'s `timeout(...)`
  call to `Duration::from_millis(100)`, rerun: expected 1 failed. Revert.
  Rerun: expected `6 passed`.

### Step 3.2 — commit

- [ ] ```
  test(concurrency): pin the queue timeout to the configured value

  Green on arrival; proven meaningful by hard-coding 100ms in forward()
  and watching it fail. Guards against the timeout drifting back into a
  constant.
  ```

---

## Task 4 — Docs

- [ ] `README.md`: after the `MUR_MODEL_GATEWAY_COMPRESS` row, add:

```
| `MUR_MODEL_GATEWAY_MAX_CONCURRENCY` | unlimited | Per-provider cap on simultaneous upstream calls; `0`/garbage = unlimited (logged) |
| `MUR_MODEL_GATEWAY_QUEUE_TIMEOUT_SECS` | `30` | How long a call waits for a free slot before a local `429` with `retry-after: 5` |
```

- [ ] `docs/install.md`: after line 6's sentence ending `— there is no config file.`, add on a new line:

```
A fleet of MUR workers sharing one gateway should set
`MUR_MODEL_GATEWAY_MAX_CONCURRENCY` (e.g. `3`); see the README table for the
overflow behaviour.
```

- [ ] Commit:
  ```
  docs(concurrency): document the cap and queue-timeout variables
  ```

---

## Task 5 — Full verification and PR

- [ ] `cargo fmt --check` — no output.
- [ ] `cargo clippy --all-targets -- -D warnings 2>&1 | tail -3` — clean.
- [ ] `cargo test 2>&1 | grep -E 'test result|FAILED|panicked'` — every line
  `test result: ok.`; total across all binaries includes the 5
  `max_concurrency` tests and 7 `concurrency_env` tests.
- [ ] `git push -u origin feat/per-provider-concurrency-cap`
- [ ] `gh pr create --base main --title "feat(concurrency): per-provider upstream cap with bounded queue and local 429"`
  with a body summarising spec §1–§3 and ending in its own line
  `Generated with [MUR](https://app.mur.run/products/mur)`.

**Blocked-binary rule:** if any `cargo` command is denied locally, ship it via
`fleet_run` with the exact command and `cwd ~/Projects/mur-model-gateway` in
`goal`, and paste the output tail as the evidence for the checkbox.

---

## Self-review

- Spec §1 (bounded wait → 429): Task 1. §2 numbers: Task 1 (`DEFAULT_QUEUE_TIMEOUT`, `CONCURRENCY_RETRY_AFTER_SECS`), Task 2 (env parsing). §3 body shape: Task 1 `concurrency_cap_response`, asserted in Task 1's test. §4: constraints only, no task. Out-of-scope items: none touched.
- Names consistent across tasks: `with_queue_timeout`, `DEFAULT_QUEUE_TIMEOUT`, `Provider::name`, `spawn_gateway_with_queue`, `fire_collect`, `ProviderSemaphores.cap`.
- No placeholders; every step shows code or an exact command with expected output.
