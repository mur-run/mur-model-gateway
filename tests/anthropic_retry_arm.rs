//! End-to-end proof of the delegated-refresh arm: an expired Anthropic
//! credential in a file, a fake `claude` that rewrites it, and an upstream
//! that rejects the stale token once and accepts the refreshed one.
//!
//! Only compiled when disguise_impl.rs is present (cfg(has_beta_hook)): the
//! real `apply_disguise_headers` is what turns the resolved token into a
//! predictable `Authorization: Bearer <token>` header, which is how this
//! test tells the stale request from the retried one. The public-build stub
//! is a no-op that never attaches Authorization at all — both attempts would
//! carry no auth header and the two mocks below would be unreachable — so
//! this test cannot mean anything without the real implementation present.
//! Same reasoning as `tests/codex.rs`'s `#![cfg(has_codex_hook)]` gate.
#![cfg(has_beta_hook)]

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

#[tokio::test(flavor = "multi_thread")]
async fn an_expired_credential_is_refreshed_and_the_request_retried() {
    let dir = tempfile::tempdir().unwrap();
    let creds = dir.path().join("creds.json");
    let past = 1_000_i64;
    let future = 9_999_999_999_999_i64;
    std::fs::write(&creds, blob("old-tok", past)).unwrap();

    // A fake `claude` that does what the real one does for our purposes:
    // rewrite the credential with a fresh token and a later expiry. Written
    // via `std::fs::write` (one syscall) rather than an open handle + writeln
    // pair, so there is no window where the script exists but is truncated.
    let fake = dir.path().join("claude");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\ncat > '{}' <<'EOF'\n{}\nEOF\n",
            creds.display(),
            blob("new-tok", future)
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

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
        AuthProbe::Command(fake),
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

    // The probe actually rewrote the configured source, not just the
    // in-memory response the gateway happened to see.
    let after = std::fs::read_to_string(&creds).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&after).unwrap();
    assert_eq!(
        parsed["claudeAiOauth"]["expiresAt"].as_i64(),
        Some(future),
        "refresh_via_owner must have run the probe and the credentials file \
         must reflect what it wrote"
    );
}
