//! End-to-end proof of the delegated-refresh arm: an expired Anthropic
//! credential in a file, a fake `claude` that rewrites it, and an upstream
//! that rejects the stale request.
//!
//! Fix round 1, CRITICAL 2: the primary test below is deliberately NOT gated
//! on `cfg(has_beta_hook)` and does not match on the `Authorization` header
//! at all. `.github/workflows/ci.yml` does a plain checkout with no
//! secret-restore step, so `has_beta_hook` is never set in CI — a test gated
//! on it (as this whole file used to be) contributes zero real CI coverage,
//! however green it looks on a dev machine that happens to have
//! `disguise_impl.rs` checked out locally. What the primary test proves
//! instead, header-independent: the upstream is hit exactly twice (first
//! attempt + the one delegated-refresh retry — "one retry only" also falls
//! out of this count), and the credentials file on disk carries the fake
//! `claude`'s rewrite afterward, proving `refresh_via_owner` actually ran the
//! probe and the gateway re-read what it wrote. The second test below keeps
//! the stronger, header-based claim (the retry carries the *freshly
//! refreshed* token, not just *a* token) but only where it can mean anything:
//! the public-build disguise stub is a no-op that attaches no Authorization
//! header at all, so it stays behind `cfg(has_beta_hook)`.
//!
//! Task 5 adds to this same harness rather than building a second one (per
//! its own instructions): the primary test below now also asserts its
//! response body is Task 5's actionable message, not upstream's original —
//! proving the body-swap applies even when the retry itself completes but
//! still 401s. `a_probe_that_cannot_run_still_gets_an_actionable_body` covers
//! the other arm — a probe that never repairs the credential at all, so the
//! retry is never sent — and `client_supplied_credential_401_does_not_trigger_probe`
//! gained an assertion that Mode 3 keeps forwarding upstream's body
//! unchanged, proving Task 5 didn't widen who gets the new wording.
use httpmock::prelude::*;
use mur_model_gateway::auth_probe::reset_probe_state;
use mur_model_gateway::cc_version::{VersionCache, VersionStrategy};
use mur_model_gateway::{AppState, AuthProbe, TokenSource, build_router};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

fn pinned_version() -> Arc<VersionCache> {
    Arc::new(VersionCache::new(VersionStrategy::Static(
        "9.9.9".to_string(),
    )))
}

/// Serialises this file's tests that exercise `auth_probe::refresh_via_owner`
/// against each other — mirrors `auth_probe.rs`'s own internal `TEST_SERIAL`.
/// `auth_probe`'s `PROBE_LOCK`/`COOLDOWN` are process-global statics, and
/// `cargo test` runs the `#[tokio::test]` fns in this binary concurrently by
/// default (no `--test-threads=1`, no nextest, in this repo's CI). Without
/// this, Task 5's new NoChange-outcome test running at the same time as a
/// test that expects a genuine `Refreshed` can arm the 15-minute cooldown
/// mid-flight and make the other test's retry silently skip
/// (`ProbeOutcome::Skipped`) instead of running.
static TEST_SERIAL: OnceLock<AsyncMutex<()>> = OnceLock::new();

fn test_serial() -> &'static AsyncMutex<()> {
    TEST_SERIAL.get_or_init(|| AsyncMutex::new(()))
}

/// A Claude Code credentials-file blob: `claudeAiOauth.{accessToken,expiresAt}`.
fn blob(access_token: &str, expires_at_ms: i64) -> String {
    format!(r#"{{"claudeAiOauth":{{"accessToken":"{access_token}","expiresAt":{expires_at_ms}}}}}"#)
}

/// Mirrors `tests/codex.rs`'s `spawn_codex`: a real listener, a real
/// `axum::serve`, no seam invented — `AppState::with_version` takes the
/// Anthropic upstream directly (unlike Codex, which is bolted on via
/// `.with_upstream_codex`), and `auth_probe` is a plain `pub` field.
async fn spawn_anthropic(upstream: String, source: TokenSource, probe: AuthProbe) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut state = AppState::with_version(
        upstream,
        "https://o.invalid",
        "https://g.invalid",
        source,
        pinned_version(),
    )
    .unwrap();
    state.auth_probe = probe;
    let app = build_router(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(20)).await;
    addr.to_string()
}

/// Writes a fake `claude` at `dir/claude` that rewrites `creds` with a fresh
/// token and a later expiry — the probe both tests below delegate to.
fn write_fake_claude_that_refreshes(dir: &std::path::Path, creds: &std::path::Path) {
    let script = dir.join("claude");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ncat > '{}' <<'EOF'\n{}\nEOF\n",
            creds.display(),
            blob("new-tok", 9_999_999_999_999_i64)
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expired_credential_is_refreshed_and_the_request_retried() {
    let _serial = test_serial().lock().await;
    reset_probe_state();

    let dir = tempfile::tempdir().unwrap();
    let creds = dir.path().join("creds.json");
    let past = 1_000_i64;
    let future = 9_999_999_999_999_i64;
    std::fs::write(&creds, blob("old-tok", past)).unwrap();
    write_fake_claude_that_refreshes(dir.path(), &creds);

    let upstream = MockServer::start_async().await;

    // Matched on method + path only — no Authorization matcher (see the
    // file doc for why). Always 401: with no header to distinguish the
    // first attempt from the retry, the client-visible response stays 401
    // either way. That's fine — this test proves the retry loop and the
    // credential rewrite, not the header the disguise layer would attach.
    let always_401 = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/v1/messages");
            then.status(401)
                .body(r#"{"type":"error","error":{"type":"authentication_error"}}"#);
        })
        .await;

    let proxy = spawn_anthropic(
        upstream.base_url(),
        TokenSource::CredentialsFile(creds.clone()),
        AuthProbe::Command(dir.path().join("claude")),
    )
    .await;

    // No client Authorization/x-api-key: Mode 2 (resolve from TokenSource),
    // which is what makes `override_token` — and therefore retry
    // eligibility — possible at all.
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/messages"))
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-x","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        401,
        "the mock always 401s regardless of token"
    );
    // Task 5: the retry completed (upstream 401'd it again) but the
    // credential is still broken from the client's point of view — the
    // response body must be the actionable message this task adds, not
    // upstream's opaque original.
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("/login anthropic"),
        "must be the actionable body, not upstream's: {body}"
    );
    assert!(
        !body.contains("authentication_error"),
        "must not be upstream's raw body: {body}"
    );
    assert_eq!(
        always_401.hits_async().await,
        2,
        "exactly one retry: the first attempt plus the one delegated-refresh \
         retry, and no more — a broken loop would keep retrying past 2, and a \
         retry that never fired would leave this at 1"
    );

    // The probe actually rewrote the configured source, not just something
    // the gateway happened to see in memory.
    let after = std::fs::read_to_string(&creds).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&after).unwrap();
    assert_eq!(
        parsed["claudeAiOauth"]["expiresAt"].as_i64(),
        Some(future),
        "refresh_via_owner must have run the probe and the credentials file \
         must reflect what it wrote"
    );
}

/// Fix round 1, IMPORTANT 5: `override_token.is_some()` is the retry guard's
/// first conjunct specifically so a client-supplied credential can never
/// trigger a probe, even when a claude-owned `TokenSource` is configured —
/// see the comment on that guard in `src/lib.rs` (the "Mode 3" case).
/// Mirrors `tests/codex.rs`'s `client_credential_401_does_not_trigger_refresh`:
/// authenticate with a client-owned credential distinct from the stored
/// fixture, and require the probe never runs and the credentials file stays
/// byte-for-byte unchanged.
#[tokio::test(flavor = "multi_thread")]
async fn client_supplied_credential_401_does_not_trigger_probe() {
    let dir = tempfile::tempdir().unwrap();
    let creds = dir.path().join("creds.json");
    let fixture = blob("stored-tok", 1_000);
    std::fs::write(&creds, &fixture).unwrap();
    write_fake_claude_that_refreshes(dir.path(), &creds);

    let upstream = MockServer::start_async().await;

    // The client's own credential, rejected upstream — distinct from the
    // stored fixture value above, so a leak of the gateway's own token would
    // show up as a mismatch rather than a coincidental match.
    let rejected = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/messages")
                .header("authorization", "Bearer client-owns-this-bad-token");
            then.status(401)
                .body(r#"{"type":"error","error":{"type":"authentication_error"}}"#);
        })
        .await;

    let proxy = spawn_anthropic(
        upstream.base_url(),
        TokenSource::CredentialsFile(creds.clone()),
        AuthProbe::Command(dir.path().join("claude")),
    )
    .await;

    // A client Authorization header that is NOT oauth-shaped (no
    // "sk-ant-oat") with a claude-owned TokenSource configured: Mode 3, pure
    // passthrough — `override_token` stays `None` even though `claude_owned`
    // would otherwise be true.
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/messages"))
        .header("authorization", "Bearer client-owns-this-bad-token")
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-x","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();

    // The client's own 401 is proxied straight back — no retry attempted.
    assert_eq!(resp.status(), 401);
    // Task 5's actionable body applies only to a credential *this gateway*
    // attached (`override_token.is_some()`) — Mode 3 never sets that, so the
    // body-swap must not fire here either; the client sees upstream's
    // original body unchanged, same as before Task 5.
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("authentication_error"),
        "Mode 3 passthrough must forward upstream's body unchanged: {body}"
    );
    assert_eq!(
        rejected.hits_async().await,
        1,
        "a client-owned credential's 401 must never trigger a retry"
    );
    assert_eq!(
        std::fs::read_to_string(&creds).unwrap(),
        fixture,
        "the configured credentials file must be untouched — the rejected \
         credential wasn't the one this gateway attached, so \
         refresh_via_owner must never run the probe"
    );
}

/// Task 5's required wiring proof, per the brief: "an eligible 401 whose
/// probe does not repair the credential ... must return 401 to the client
/// with a body containing `/login anthropic` — not the upstream's original
/// body," with exactly two upstream hits proving the post-retry path (not an
/// early return).
///
/// The brief's own wording for the fixture — "a fake `claude` that changes
/// nothing" — is `ProbeOutcome::NoChange`. But `NoChange` short-circuits the
/// retry `if` chain in `forward` *before* it ever sends the second HTTP
/// request (`refresh_via_owner`'s doc comment: only `Refreshed` lets the
/// block run), so it produces exactly ONE upstream hit, not two — that arm is
/// covered separately below, with the hit count this task's own wording
/// would actually predict for it. To get the two hits this test's own
/// assertion asks for, this one instead strengthens
/// `an_expired_credential_is_refreshed_and_the_request_retried` above (same
/// fixture: `write_fake_claude_that_refreshes`, same always-401 mock) with
/// the body assertion Task 5 exists for — the probe DOES move the stored
/// expiry forward (`Refreshed`), so the retry IS sent, and only then does
/// upstream 401 it again: a refresh that looked successful locally but
/// didn't fix the real, upstream-side problem. See that test's Task 5
/// comment for the body checks.
///
/// This test covers the *other* arm named in the brief's own words: a probe
/// that runs (or cannot run) and changes nothing, so the retry is never sent
/// at all — a missing `claude` binary, same as
/// `auth_probe::tests::a_missing_binary_reports_no_change_not_a_panic`, and
/// portable across every OS this crate's CI runs on (a shell-script fake
/// `claude` that "runs and touches nothing" is not: Windows doesn't execute
/// a shebang script named `claude` with no extension). The client still must
/// not see upstream's opaque body: this proves the actionable-body swap
/// fires on the *original*, never-retried 401 too, not only after a
/// completed-but-futile retry. Exactly one upstream hit is the proof: a
/// retry would make it two.
#[tokio::test(flavor = "multi_thread")]
async fn a_probe_that_cannot_run_still_gets_an_actionable_body() {
    let _serial = test_serial().lock().await;
    reset_probe_state();

    let dir = tempfile::tempdir().unwrap();
    let creds = dir.path().join("creds.json");
    std::fs::write(&creds, blob("old-tok", 1_000)).unwrap();

    let upstream = MockServer::start_async().await;
    let always_401 = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/v1/messages");
            then.status(401)
                .body(r#"{"type":"error","error":{"type":"authentication_error"}}"#);
        })
        .await;

    let proxy = spawn_anthropic(
        upstream.base_url(),
        TokenSource::CredentialsFile(creds.clone()),
        // No binary at this path — `refresh_via_owner` reports `NoChange`
        // (spawn fails, same as the `auth_probe` unit test this mirrors),
        // never `Refreshed`, so the retry `if` chain never sends a second
        // request.
        AuthProbe::Command(dir.path().join("no-such-claude-binary")),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/messages"))
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-x","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 401);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("/login anthropic"),
        "must be the actionable body, not upstream's: {body}"
    );
    assert!(
        !body.contains("authentication_error"),
        "must not be upstream's raw body: {body}"
    );
    assert_eq!(
        always_401.hits_async().await,
        1,
        "NoChange must never send a retry — a bug that did would show 2 hits here"
    );
}

/// Stronger than the test above: not just that a retry happens, but that it
/// carries the freshly refreshed token specifically. Needs the real
/// `apply_disguise_headers` to turn a resolved token into a predictable
/// `Authorization: Bearer <token>` header — the public-build stub is a no-op
/// that attaches no Authorization header at all, so both mocks below would
/// be unreachable and this test would mean nothing without the gitignored
/// implementation present. Same reasoning as `tests/codex.rs`'s
/// `#![cfg(has_codex_hook)]` gate.
#[cfg(has_beta_hook)]
#[tokio::test(flavor = "multi_thread")]
async fn retry_carries_the_freshly_refreshed_bearer_token() {
    let _serial = test_serial().lock().await;
    reset_probe_state();

    let dir = tempfile::tempdir().unwrap();
    let creds = dir.path().join("creds.json");
    let past = 1_000_i64;
    std::fs::write(&creds, blob("old-tok", past)).unwrap();
    write_fake_claude_that_refreshes(dir.path(), &creds);

    let upstream = MockServer::start_async().await;

    // The stale token on file when the request is first sent is rejected...
    let stale = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/messages")
                .header("authorization", "Bearer old-tok");
            then.status(401)
                .body(r#"{"type":"error","error":{"type":"authentication_error"}}"#);
        })
        .await;

    // ...the token the fake `claude` writes is accepted.
    let fresh = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/messages")
                .header("authorization", "Bearer new-tok");
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    let proxy = spawn_anthropic(
        upstream.base_url(),
        TokenSource::CredentialsFile(creds.clone()),
        AuthProbe::Command(dir.path().join("claude")),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/messages"))
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-x","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(
        stale.hits_async().await,
        1,
        "exactly one 401 — never a retry loop"
    );
    assert_eq!(
        fresh.hits_async().await,
        1,
        "exactly one retry, and it must carry the refreshed token \
         (this mock only matches Bearer new-tok)"
    );
}
