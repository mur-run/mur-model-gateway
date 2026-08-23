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
use httpmock::prelude::*;
use mur_model_gateway::cc_version::{VersionCache, VersionStrategy};
use mur_model_gateway::{AppState, AuthProbe, TokenSource, build_router};
use std::sync::Arc;
use std::time::Duration;

fn pinned_version() -> Arc<VersionCache> {
    Arc::new(VersionCache::new(VersionStrategy::Static(
        "9.9.9".to_string(),
    )))
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
