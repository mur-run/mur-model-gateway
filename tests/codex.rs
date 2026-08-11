//! Stage 1 acceptance: the Codex route attaches OAuth credentials.
//!
//! Only compiled when codex_impl.rs is present (cfg(has_codex_hook)).
#![cfg(has_codex_hook)]

use httpmock::prelude::*;
use mur_model_gateway::cc_version::{VersionCache, VersionStrategy};
use mur_model_gateway::{AppState, TokenSource, build_router};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, MutexGuard};

fn pinned_version() -> Arc<VersionCache> {
    Arc::new(VersionCache::new(VersionStrategy::Static(
        "9.9.9".to_string(),
    )))
}

// Fix round 1, finding 7: `AppState::with_version` (via `spawn_codex`) reads
// `MUR_MODEL_GATEWAY_COMPRESS` on every call, and the refresh tests below
// mutate `MUR_MODEL_GATEWAY_CODEX_TOKEN_ENDPOINT`. `std::env::set_var` is
// `unsafe` in edition 2024 precisely because a concurrent `getenv` on *any*
// key while a `setenv` on *any* key is in flight is a data race at the libc
// level — not only same-key races. `cargo test` runs every `#[tokio::test]`
// in this file on its own thread by default, so every test here is a
// potential concurrent env reader. Rather than argue a partial lock over
// just the env-mutating tests is enough, every test in this file acquires
// this lock for its full body: the file runs effectively single-threaded,
// and the `unsafe` env blocks below are then actually sound instead of
// "probably fine in practice."
//
// This has to be `tokio::sync::Mutex`, not `std::sync::Mutex`: every test
// body holds the guard across many `.await` points (mock server startup,
// the actual request, etc.), and clippy's `await_holding_lock` correctly
// flags a blocking-lock guard held that way as a hazard (it can park a
// tokio worker thread while holding a lock nothing async-aware can see).
// `tokio::sync::Mutex` is designed to be held across `.await` — acquiring
// it yields instead of blocking the executor.
static ENV_SERIAL: Mutex<()> = Mutex::const_new(());

/// Acquire the file-wide serialisation lock. `tokio::sync::Mutex` has no
/// poisoning concept — unlike `std::sync::Mutex`, a task that panics while
/// holding the guard simply drops and releases it during unwind, so a
/// previous test panicking mid-body can't brick every later test in the
/// file; there is no poisoned state to recover from.
async fn serial_guard() -> MutexGuard<'static, ()> {
    ENV_SERIAL.lock().await
}

/// Clears the named env var on drop, including on panic — an `assert!`
/// failing mid-test must not leak the override into whatever the harness
/// runs next in this process.
struct EnvVarGuard(&'static str);

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // SAFETY: the caller obtained this guard while holding
        // `ENV_SERIAL` (via `serial_guard()`), and every test in this file
        // acquires that same lock before touching env — no other thread in
        // this process can be reading or writing env while this runs.
        unsafe { std::env::remove_var(self.0) }
    }
}

/// Same shape as `spawn` in tests/disguise.rs:330, with the Codex upstream and
/// token source overridden. The other three upstreams point at .invalid so a
/// misrouted request fails loudly instead of escaping to the network.
async fn spawn_codex(upstream: String, codex_ts: TokenSource) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = AppState::with_version(
        "https://a.invalid",
        "https://o.invalid",
        "https://g.invalid",
        TokenSource::Disabled,
        pinned_version(),
    )
    .unwrap()
    .with_upstream_codex(upstream)
    .with_token_source_codex(codex_ts);
    let app = build_router(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(20)).await;
    addr.to_string()
}

#[tokio::test]
async fn codex_route_attaches_bearer_token() {
    // Fix round 1, finding 7: serialises this test against every other
    // test in the file for the env-var reasons on `ENV_SERIAL` above.
    let _serial = serial_guard().await;
    let upstream = MockServer::start_async().await;
    // path("/responses") also asserts the /v1 strip from Task 3.
    let mock = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/responses")
                .header("authorization", "Bearer codex-tok")
                .matches(exactly_one_authorization_header);
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    let proxy = spawn_codex(
        upstream.base_url(),
        TokenSource::Static(Arc::new("codex-tok".to_string())),
    )
    .await;

    // Client sends no auth at all — the proxy fills it in.
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/responses"))
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-5-codex","input":"say ok"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    mock.assert_async().await;
}

/// Finding 1 regression: a client that authenticates itself with its own
/// `Authorization` header must see that exact header upstream — singular,
/// unmodified — and never the stored Codex credential.
#[tokio::test]
async fn codex_route_preserves_client_authorization_and_withholds_codex_token() {
    // Fix round 1, finding 7: serialises this test against every other
    // test in the file for the env-var reasons on `ENV_SERIAL` above.
    let _serial = serial_guard().await;
    let upstream = MockServer::start_async().await;
    let mock = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/responses")
                .header("authorization", "Bearer client-owns-this-token")
                .matches(exactly_one_authorization_header);
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    // Deliberately distinct from the client's own token: if the proxy ever
    // overwrites or duplicates it with this stored value, the mock (which
    // requires the client's exact value, and only one such header) fails.
    let proxy = spawn_codex(
        upstream.base_url(),
        TokenSource::Static(Arc::new("codex-tok-must-not-leak".to_string())),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/responses"))
        .header("authorization", "Bearer client-owns-this-token")
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-5-codex","input":"say ok"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    mock.assert_async().await;
}

/// Finding 1 regression (the exact bug shape): a client that authenticates
/// itself with `x-api-key` alone (no `Authorization`) must see that header
/// upstream unmodified, with no `Authorization` header injected at all.
#[tokio::test]
async fn codex_route_preserves_client_api_key_and_withholds_codex_token() {
    // Fix round 1, finding 7: serialises this test against every other
    // test in the file for the env-var reasons on `ENV_SERIAL` above.
    let _serial = serial_guard().await;
    let upstream = MockServer::start_async().await;
    let mock = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/responses")
                .header("x-api-key", "client-owns-this-key")
                .matches(no_authorization_header);
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    let proxy = spawn_codex(
        upstream.base_url(),
        TokenSource::Static(Arc::new("codex-tok-must-not-leak".to_string())),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/responses"))
        .header("x-api-key", "client-owns-this-key")
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-5-codex","input":"say ok"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    mock.assert_async().await;
}

/// Finding 1 edge case: an empty `Authorization` header is not a real
/// credential, so it must not count as "already authenticated" — the proxy
/// should still inject the stored Codex token. This is also the concrete
/// scenario where Finding 4's duplicate-header risk would show up if the
/// client's (empty) header were forwarded alongside the injected one, so it
/// asserts exactly one `authorization` header upstream, not two.
#[tokio::test]
async fn codex_route_treats_empty_authorization_as_unauthenticated() {
    // Fix round 1, finding 7: serialises this test against every other
    // test in the file for the env-var reasons on `ENV_SERIAL` above.
    let _serial = serial_guard().await;
    let upstream = MockServer::start_async().await;
    let mock = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/responses")
                .header("authorization", "Bearer codex-tok-for-empty-header-case")
                .matches(exactly_one_authorization_header);
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    let proxy = spawn_codex(
        upstream.base_url(),
        TokenSource::Static(Arc::new("codex-tok-for-empty-header-case".to_string())),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/responses"))
        .header("authorization", "")
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-5-codex","input":"say ok"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    mock.assert_async().await;
}

/// Finding 3: the production credential path — `TokenSource::Codex(path)` →
/// `codex::read_auth` — has its own test, separate from the `Static` shortcut
/// the other tests use. Covers both the bearer token and the account-id
/// header derived from the same fixture. Finding 4: also asserts there is
/// exactly one `authorization` header, guarding against `apply_codex_headers`
/// appending a duplicate alongside one already forwarded upstream.
#[tokio::test]
async fn codex_route_reads_bearer_and_account_id_from_auth_file() {
    // Obviously-fake fixture, same shape `codex::parse_auth` expects (see
    // src/codex.rs's own `parses_chatgpt_mode_auth` test).
    let mut fixture = tempfile::NamedTempFile::new().unwrap();
    write!(
        fixture,
        r#"{{"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{{"id_token":"fixture-id-token","access_token":"fixture-access-token","refresh_token":"fixture-refresh-token","account_id":"fixture-account-id"}},"last_refresh":"2026-07-10T00:20:57.310171Z"}}"#
    )
    .unwrap();
    fixture.flush().unwrap();

    // Fix round 1, finding 7: serialises this test against every other
    // test in the file for the env-var reasons on `ENV_SERIAL` above.
    let _serial = serial_guard().await;
    let upstream = MockServer::start_async().await;
    let mock = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/responses")
                .header("authorization", "Bearer fixture-access-token")
                .matches(exactly_one_authorization_header)
                .matches(fixture_account_id_present);
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    let proxy = spawn_codex(
        upstream.base_url(),
        TokenSource::Codex(fixture.path().to_path_buf()),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/responses"))
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-5-codex","input":"say ok"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    mock.assert_async().await;
}

// Multi-thread flavor (unlike every other test in this file): the refresh
// grant runs through `codex_impl`'s blocking bridge, which uses
// `tokio::task::block_in_place` — valid only on a multi-threaded runtime.
// That matches production (`#[tokio::main]` defaults to multi-thread); a
// current-thread test runtime would panic before ever reaching the retry.
#[tokio::test(flavor = "multi_thread")]
async fn expired_token_triggers_one_refresh_and_retry() {
    // Fix round 1, finding 7: serialises this test against every other
    // test in the file for the env-var reasons on `ENV_SERIAL` above.
    let _serial = serial_guard().await;
    let upstream = MockServer::start_async().await;

    // The stored token is rejected...
    let stale = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/responses")
                .header("authorization", "Bearer stale-tok");
            then.status(401).body(r#"{"error":"expired"}"#);
        })
        .await;

    // ...the refreshed one is accepted.
    let fresh = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/responses")
                .header("authorization", "Bearer fresh-tok");
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    let token_ep = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/oauth/token");
            then.status(200).body(r#"{"access_token":"fresh-tok"}"#);
        })
        .await;

    let dir = std::env::temp_dir().join("mmg-codex-refresh");
    std::fs::create_dir_all(&dir).unwrap();
    let auth = dir.join("auth.json");
    std::fs::write(
        &auth,
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"stale-tok","refresh_token":"fake-refresh-token","account_id":"acct-fake"}}"#,
    )
    .unwrap();

    // SAFETY: `_serial` (acquired above) holds `ENV_SERIAL` for this test's
    // full body, and every test in this file acquires that same lock before
    // touching env — no other thread in this process can be reading or
    // writing env while this runs.
    unsafe {
        std::env::set_var(
            "MUR_MODEL_GATEWAY_CODEX_TOKEN_ENDPOINT",
            format!("{}/oauth/token", upstream.base_url()),
        );
    }
    // Cleared on drop (including on an assert panic below), so a failure
    // here can't leak the override into whatever the harness runs next.
    let _env_guard = EnvVarGuard("MUR_MODEL_GATEWAY_CODEX_TOKEN_ENDPOINT");
    mur_model_gateway::codex::reset_refresh_cache().await;

    let proxy = spawn_codex(upstream.base_url(), TokenSource::Codex(auth.clone())).await;
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/responses"))
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-5-codex","input":"say ok"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(
        stale.hits_async().await,
        1,
        "exactly one 401 — never a retry loop"
    );
    assert_eq!(token_ep.hits_async().await, 1, "exactly one refresh");
    assert_eq!(fresh.hits_async().await, 1, "exactly one retry");
    std::fs::remove_file(&auth).ok();
}

/// Fix round 3, finding C1 (red against pre-fix code — see
/// `codex_retry_eligible` in src/lib.rs for the unit-testable core of this
/// same guard). Before this fix, the 401-retry guard checked only that the
/// route was Codex, the status was 401, and a Codex token source was
/// configured — never whether the credential that actually got rejected was
/// one *this gateway* had attached. A client posting its own bad key to
/// `/v1/responses` would get a 401 from upstream, and the old guard would
/// still fire: it would redeem, and rotate, the user's real stored refresh
/// token to retry a request that never used that credential in the first
/// place. This test authenticates with a client-owned credential distinct
/// from the stored fixture, and requires the token endpoint see zero hits
/// and `auth.json` stay byte-for-byte unchanged.
#[tokio::test(flavor = "multi_thread")]
async fn client_credential_401_does_not_trigger_refresh() {
    // Fix round 1, finding 7: serialises this test against every other
    // test in the file for the env-var reasons on `ENV_SERIAL` above.
    let _serial = serial_guard().await;
    let upstream = MockServer::start_async().await;

    // The client's own bad key, rejected upstream — distinct from every
    // stored-credential fixture value used elsewhere in this file, so a
    // failure here can't be confused with the gateway's own token leaking.
    let rejected = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/responses")
                .header("authorization", "Bearer client-owns-this-bad-token");
            then.status(401).body(r#"{"error":"invalid_api_key"}"#);
        })
        .await;

    // Configured, but must see zero hits: this test's whole point is that a
    // client-owned credential's 401 must never reach the refresh path at all.
    let token_ep = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/oauth/token");
            then.status(200)
                .body(r#"{"access_token":"should-never-be-issued"}"#);
        })
        .await;

    let dir = std::env::temp_dir().join("mmg-codex-c1-guard");
    std::fs::create_dir_all(&dir).unwrap();
    let auth = dir.join("auth.json");
    let fixture = r#"{"auth_mode":"chatgpt","tokens":{"access_token":"stored-tok","refresh_token":"stored-refresh-token","account_id":"acct-fake"}}"#;
    std::fs::write(&auth, fixture).unwrap();

    // SAFETY: `_serial` (acquired above) holds `ENV_SERIAL` for this test's
    // full body, and every test in this file acquires that same lock before
    // touching env — no other thread in this process can be reading or
    // writing env while this runs.
    unsafe {
        std::env::set_var(
            "MUR_MODEL_GATEWAY_CODEX_TOKEN_ENDPOINT",
            format!("{}/oauth/token", upstream.base_url()),
        );
    }
    // Cleared on drop (including on an assert panic below), so a failure
    // here can't leak the override into whatever the harness runs next.
    let _env_guard = EnvVarGuard("MUR_MODEL_GATEWAY_CODEX_TOKEN_ENDPOINT");
    mur_model_gateway::codex::reset_refresh_cache().await;

    let proxy = spawn_codex(upstream.base_url(), TokenSource::Codex(auth.clone())).await;
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/responses"))
        .header("authorization", "Bearer client-owns-this-bad-token")
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-5-codex","input":"say ok"}"#)
        .send()
        .await
        .unwrap();

    // The client's own 401 is proxied straight back — no retry attempted.
    assert_eq!(resp.status(), 401);
    assert_eq!(rejected.hits_async().await, 1);
    assert_eq!(
        token_ep.hits_async().await,
        0,
        "a client-owned credential's 401 must never redeem the gateway's stored refresh token"
    );
    assert_eq!(
        std::fs::read_to_string(&auth).unwrap(),
        fixture,
        "auth.json must be untouched when the rejected credential wasn't the gateway's own"
    );
    std::fs::remove_file(&auth).ok();
}

/// Fix round 1, finding 4: the retry rebuilt the request from scratch with
/// only `apply_codex_headers` applied (bearer/originator/account-id —
/// confirmed by inspection that it never sets `content-type` or `accept`),
/// dropping every other client header. A real upstream can reject that; a
/// streaming call in particular loses `accept: text/event-stream`. This
/// mock requires that header on the retried request specifically, so it
/// fails if the retry ever goes out bare again.
#[tokio::test(flavor = "multi_thread")]
async fn retry_forwards_original_request_headers() {
    let _serial = serial_guard().await;
    let upstream = MockServer::start_async().await;

    let stale = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/responses")
                .header("authorization", "Bearer stale-tok");
            then.status(401).body(r#"{"error":"expired"}"#);
        })
        .await;

    let fresh = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/responses")
                .header("authorization", "Bearer fresh-tok")
                // The load-bearing condition: only present if the retry
                // forwarded the client's original headers.
                .header("accept", "text/event-stream")
                // Fix round 3, finding I5: the retry must still carry the
                // account-id header derived from the fixture — a real
                // backend rejects requests missing it. Checked by value,
                // like `fixture_account_id_present`, so this test doesn't
                // need to hardcode which header name carries it.
                .matches(retry_fixture_account_id_present);
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    let token_ep = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/oauth/token");
            then.status(200).body(r#"{"access_token":"fresh-tok"}"#);
        })
        .await;

    let dir = std::env::temp_dir().join("mmg-codex-retry-headers");
    std::fs::create_dir_all(&dir).unwrap();
    let auth = dir.join("auth.json");
    std::fs::write(
        &auth,
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"stale-tok","refresh_token":"fake-refresh-token","account_id":"acct-fake"}}"#,
    )
    .unwrap();

    // SAFETY: `_serial` (acquired above) holds `ENV_SERIAL` for this test's
    // full body, and every test in this file acquires that same lock before
    // touching env.
    unsafe {
        std::env::set_var(
            "MUR_MODEL_GATEWAY_CODEX_TOKEN_ENDPOINT",
            format!("{}/oauth/token", upstream.base_url()),
        );
    }
    let _env_guard = EnvVarGuard("MUR_MODEL_GATEWAY_CODEX_TOKEN_ENDPOINT");
    mur_model_gateway::codex::reset_refresh_cache().await;

    let proxy = spawn_codex(upstream.base_url(), TokenSource::Codex(auth.clone())).await;
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/responses"))
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .body(r#"{"model":"gpt-5-codex","input":"say ok","stream":true}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(stale.hits_async().await, 1, "exactly one 401");
    assert_eq!(token_ep.hits_async().await, 1, "exactly one refresh");
    assert_eq!(
        fresh.hits_async().await,
        1,
        "retry must carry the client's original accept header"
    );
    std::fs::remove_file(&auth).ok();
}

// Multi-thread flavor: same reason as `expired_token_triggers_one_refresh_
// and_retry` — the refresh grant runs through the blocking bridge.
//
// Fix round 2, finding A: releasing the cache lock across the network call
// (round 1, finding 5) restored liveness but lost single-flight — N
// concurrent cold-cache 401s could each pass the "cache is empty" check
// before any of them finished refreshing, and each would redeem the same
// (rotating) refresh token. The first redemption invalidates it, so the
// losers would get `invalid_grant`, and an interleaved `persist_rotation`
// could write a superseded token back over a newer one in `auth.json`,
// stranding the user's real Codex CLI on a dead credential. This test
// fires many concurrent requests at a cold cache and asserts the token
// endpoint is hit exactly once. The token endpoint mock is deliberately
// slow so the race window a broken implementation would need is wide open
// regardless of scheduling — the assertion is deterministic, not a timing
// coincidence.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_401s_redeem_the_refresh_token_exactly_once() {
    let _serial = serial_guard().await;
    let upstream = MockServer::start_async().await;

    let stale = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/responses")
                .header("authorization", "Bearer stale-tok");
            then.status(401).body(r#"{"error":"expired"}"#);
        })
        .await;

    let fresh = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/responses")
                .header("authorization", "Bearer fresh-tok");
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    let token_ep = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/oauth/token");
            // Deliberately slow (fix round 2, finding A's test): widens
            // the window a non-single-flight implementation would race
            // through, so 16 concurrently spawned requests are guaranteed
            // to all reach their own cache check well before this
            // response lands, regardless of how the OS schedules them.
            then.status(200)
                .delay(Duration::from_millis(150))
                .body(r#"{"access_token":"fresh-tok"}"#);
        })
        .await;

    let dir = std::env::temp_dir().join("mmg-codex-concurrent-refresh");
    std::fs::create_dir_all(&dir).unwrap();
    let auth = dir.join("auth.json");
    std::fs::write(
        &auth,
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"stale-tok","refresh_token":"fake-refresh-token","account_id":"acct-fake"}}"#,
    )
    .unwrap();

    // SAFETY: `_serial` (acquired above) holds `ENV_SERIAL` for this test's
    // full body, and every test in this file acquires that same lock before
    // touching env — no other thread in this process can be reading or
    // writing env while this runs.
    unsafe {
        std::env::set_var(
            "MUR_MODEL_GATEWAY_CODEX_TOKEN_ENDPOINT",
            format!("{}/oauth/token", upstream.base_url()),
        );
    }
    let _env_guard = EnvVarGuard("MUR_MODEL_GATEWAY_CODEX_TOKEN_ENDPOINT");
    mur_model_gateway::codex::reset_refresh_cache().await;

    let proxy = spawn_codex(upstream.base_url(), TokenSource::Codex(auth.clone())).await;

    // Fire many requests at once, all racing to refresh the same cold cache.
    let mut handles = Vec::new();
    for _ in 0..16 {
        let proxy = proxy.clone();
        handles.push(tokio::spawn(async move {
            reqwest::Client::new()
                .post(format!("http://{proxy}/v1/responses"))
                .header("content-type", "application/json")
                .body(r#"{"model":"gpt-5-codex","input":"say ok"}"#)
                .send()
                .await
                .unwrap()
                .status()
        }));
    }
    let mut statuses = Vec::new();
    for h in handles {
        statuses.push(h.await.unwrap());
    }

    assert!(
        statuses.iter().all(|s| *s == 200),
        "every concurrent request must succeed via the one refreshed token: {statuses:?}"
    );
    assert_eq!(
        stale.hits_async().await,
        16,
        "every request's own first attempt hits the stale token once"
    );
    assert_eq!(
        token_ep.hits_async().await,
        1,
        "single-flight: 16 concurrent cold-cache 401s must redeem the refresh token exactly once"
    );
    assert_eq!(
        fresh.hits_async().await,
        16,
        "every retry must use the one refreshed token"
    );
    std::fs::remove_file(&auth).ok();
}

// ─── helpers ─────────────────────────────────────────────────────────
//
// httpmock's `.matches()` takes a bare `fn(&HttpMockRequest) -> bool`, not a
// closure trait, so these are free functions (a non-capturing closure would
// also coerce, but a named fn reads clearer at the call site).

fn header_count(req: &HttpMockRequest, name: &str) -> usize {
    req.headers
        .as_ref()
        .map(|hs| {
            hs.iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case(name))
                .count()
        })
        .unwrap_or(0)
}

fn exactly_one_authorization_header(req: &HttpMockRequest) -> bool {
    header_count(req, "authorization") == 1
}

fn no_authorization_header(req: &HttpMockRequest) -> bool {
    header_count(req, "authorization") == 0
}

/// True if some header carries this exact fixture value. Checked by value
/// rather than by header name so this test doesn't need to know (or
/// hardcode) which header `apply_codex_headers` uses for the account id —
/// that detail lives in the gitignored codex_impl.rs.
fn fixture_account_id_present(req: &HttpMockRequest) -> bool {
    req.headers
        .as_ref()
        .is_some_and(|hs| hs.iter().any(|(_, v)| v == "fixture-account-id"))
}

/// Same "check by value, not by header name" rationale as
/// `fixture_account_id_present`, for the `"acct-fake"` account id shared by
/// every 401-retry test's `auth.json` fixture in this file (fix round 3,
/// finding I5).
fn retry_fixture_account_id_present(req: &HttpMockRequest) -> bool {
    req.headers
        .as_ref()
        .is_some_and(|hs| hs.iter().any(|(_, v)| v == "acct-fake"))
}
