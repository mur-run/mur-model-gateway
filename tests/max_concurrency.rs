//! Cross-process concurrency cap: multiple MUR fleet workers, each its own
//! process, proxy through one shared mur-model-gateway instance. No single
//! worker can see its siblings' in-flight calls — only the gateway process
//! they all share can. `AppState::with_max_concurrency` gives that one
//! process a per-provider ceiling on simultaneous upstream calls, so a fleet
//! fan-out can't blow past a provider's concurrency quota just because it
//! spread itself across N terminals.
//!
//! This is deliberately request concurrency, not requests/sec — governor-style
//! rate limiting is a different, complementary knob (see mur-core's
//! `RateLimitedBackend`, which throttles the *rate* a single process issues
//! calls at). This gateway throttles how many calls from ALL of them may be
//! in flight upstream at once, which a per-process rate limiter structurally
//! cannot do.
//!
//! The upstream here is hand-rolled (not httpmock) because the assertion
//! needs a live high-water mark of concurrent in-flight requests, not just a
//! final call count — httpmock's `.delay()` fixes a duration but doesn't hand
//! back a running concurrency counter to assert against mid-flight.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use mur_model_gateway::{AppState, TokenSource, build_router};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[derive(Clone)]
struct SlowUpstream {
    current: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    hold: Duration,
}

async fn slow_handler(State(up): State<SlowUpstream>) -> impl IntoResponse {
    let now = up.current.fetch_add(1, Ordering::SeqCst) + 1;
    // Racily-safe high-water mark: fetch_max isn't stable on AtomicUsize's
    // convenience API surface we want here, so CAS-loop it.
    let mut observed = up.peak.load(Ordering::SeqCst);
    while now > observed {
        match up
            .peak
            .compare_exchange(observed, now, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => break,
            Err(v) => observed = v,
        }
    }
    tokio::time::sleep(up.hold).await;
    up.current.fetch_sub(1, Ordering::SeqCst);
    (StatusCode::OK, r#"{"ok":true}"#)
}

async fn spawn_upstream(hold: Duration) -> (String, Arc<AtomicUsize>) {
    spawn_upstream_at("/v1/messages", hold).await
}

/// Like `spawn_upstream`, but the mock route is at `path` rather than the
/// hardcoded `/v1/messages` — needed to stand up an OpenAI-shaped upstream
/// (`/v1/chat/completions`) alongside an Anthropic one in the same test.
async fn spawn_upstream_at(path: &str, hold: Duration) -> (String, Arc<AtomicUsize>) {
    let up = SlowUpstream {
        current: Arc::new(AtomicUsize::new(0)),
        peak: Arc::new(AtomicUsize::new(0)),
        hold,
    };
    let peak = up.peak.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route(path, post(slow_handler))
        .with_state(up);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), peak)
}

async fn spawn_gateway(upstream: &str, max_concurrency: Option<usize>) -> String {
    spawn_gateway_multi(upstream, "https://o.invalid", max_concurrency).await
}

/// Like `spawn_gateway`, but lets the caller point Anthropic and OpenAI at
/// two independent upstreams — needed to prove the cap is per-provider, not
/// one shared gate that would let a saturated Anthropic queue starve OpenAI.
async fn spawn_gateway_multi(
    upstream_anthropic: &str,
    upstream_openai: &str,
    max_concurrency: Option<usize>,
) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut state = AppState::new(
        upstream_anthropic,
        upstream_openai,
        "https://g.invalid",
        TokenSource::Disabled,
    )
    .unwrap();
    if let Some(n) = max_concurrency {
        state = state.with_max_concurrency(n);
    }
    let app = build_router(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(20)).await;
    addr.to_string()
}

fn body() -> &'static str {
    r#"{"model":"claude-sonnet-5","max_tokens":1,"messages":[{"role":"user","content":"hi"}]}"#
}

async fn fire(gw: &str, n: usize) {
    fire_path(gw, "/v1/messages", n).await
}

/// Like `fire`, but posts to `path` — needed to drive the OpenAI-shaped
/// route (`/v1/chat/completions`) in the cross-provider isolation test.
async fn fire_path(gw: &str, path: &str, n: usize) {
    let client = reqwest::Client::new();
    let mut handles = Vec::new();
    for _ in 0..n {
        let client = client.clone();
        let url = format!("http://{gw}{path}");
        handles.push(tokio::spawn(async move {
            let resp = client
                .post(url)
                .header("content-type", "application/json")
                .body(body())
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
}

/// The cap holds: 6 callers hitting a gateway configured for max concurrency
/// 2 never push more than 2 requests in flight at the upstream, no matter
/// how many callers arrive at once (standing in for N separate fleet worker
/// processes, all sharing this one gateway).
#[tokio::test]
async fn caps_concurrent_upstream_calls_across_many_callers() {
    let (upstream, peak) = spawn_upstream(Duration::from_millis(150)).await;
    let gw = spawn_gateway(&upstream, Some(2)).await;

    fire(&gw, 6).await;

    assert!(
        peak.load(Ordering::SeqCst) <= 2,
        "peak concurrent upstream calls must never exceed the configured cap of 2, saw {}",
        peak.load(Ordering::SeqCst)
    );
}

/// Unconfigured stays unconfigured: today's behaviour (unlimited concurrency)
/// must not regress for every existing deployment that never calls
/// `with_max_concurrency`. 6 callers against an uncapped gateway should be
/// free to run essentially all at once.
#[tokio::test]
async fn default_is_unlimited() {
    let (upstream, peak) = spawn_upstream(Duration::from_millis(150)).await;
    let gw = spawn_gateway(&upstream, None).await;

    fire(&gw, 6).await;

    assert!(
        peak.load(Ordering::SeqCst) >= 5,
        "no cap configured: calls should run essentially concurrently, only saw {} in flight at once",
        peak.load(Ordering::SeqCst)
    );
}

/// The cap is per-provider, not one shared gate: saturating Anthropic's
/// quota must not steal headroom from a concurrent OpenAI call. Each
/// provider gets its own `Semaphore` (see `ProviderSemaphores` in
/// `src/lib.rs`) sized to the same `max_concurrency`, so this fires 6
/// Anthropic calls (over the cap of 2) at the same time as 2 OpenAI calls
/// (at the cap) against two independent upstreams, then asserts OpenAI's
/// upstream still saw its own calls run concurrently rather than being
/// throttled by Anthropic's overflow.
#[tokio::test]
async fn provider_caps_are_independent() {
    let (anthropic_upstream, anthropic_peak) = spawn_upstream(Duration::from_millis(150)).await;
    let (openai_upstream, openai_peak) =
        spawn_upstream_at("/v1/chat/completions", Duration::from_millis(150)).await;
    let gw = spawn_gateway_multi(&anthropic_upstream, &openai_upstream, Some(2)).await;

    // Fire both providers concurrently: Anthropic well over its cap of 2,
    // OpenAI right at its cap of 2. If the two shared one semaphore,
    // Anthropic's 6 waiters would contend with — and could starve —
    // OpenAI's 2.
    let anthropic_fire = fire(&gw, 6);
    let openai_fire = fire_path(&gw, "/v1/chat/completions", 2);
    tokio::join!(anthropic_fire, openai_fire);

    assert!(
        anthropic_peak.load(Ordering::SeqCst) <= 2,
        "anthropic peak concurrent upstream calls must never exceed its cap of 2, saw {}",
        anthropic_peak.load(Ordering::SeqCst)
    );
    assert!(
        openai_peak.load(Ordering::SeqCst) >= 2,
        "openai's 2 calls should have run concurrently, undisturbed by anthropic's overflow; only saw {} in flight at once",
        openai_peak.load(Ordering::SeqCst)
    );
}
