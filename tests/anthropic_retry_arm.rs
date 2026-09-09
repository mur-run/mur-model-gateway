//! End-to-end proof of the Anthropic 401 retry arm: an expired credential in
//! a file, an upstream that rejects it, and — in the case worth retrying —
//! the credential store changing underneath the request while it is in
//! flight.
//!
//! That change is what the arm keys on. There is no probe here and nothing is
//! spawned: the gateway does not hold the refresh token, and the owner CLI
//! has no command that redeems it (`claude auth status` does not rewrite the
//! store). What repairs one of these outages in practice is an ordinary
//! `claude` session refreshing the credential in another terminal, so the
//! only question this arm can usefully ask is whether that has happened.
//!
//! The upstream here is a hand-rolled axum service rather than `httpmock`,
//! because the interesting test needs a side effect *between* the two
//! attempts: the 401 handler itself rewrites the credentials file, standing
//! in for that other terminal. Nothing else can produce the race this arm
//! exists for.
//!
//! Header-independence, kept from the version of this file that tested the
//! old probe (fix round 1, CRITICAL 2): the primary assertions count upstream
//! hits and read the response, and never match on `Authorization`. CI does a
//! plain checkout with no secret-restore step, so `has_beta_hook` is never
//! set there and the public disguise stub attaches no auth header at all — a
//! test gated on it contributes zero CI coverage however green it looks
//! locally. The stronger claim (the retry carries the *new* token, not just
//! *a* token) is asserted separately, behind that cfg.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use mur_model_gateway::cc_version::{VersionCache, VersionStrategy};
use mur_model_gateway::{AppState, TokenSource, build_router};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const STALE: &str = "sk-ant-oat01-stale";
const FRESH: &str = "sk-ant-oat01-fresh";

fn pinned_version() -> Arc<VersionCache> {
    Arc::new(VersionCache::new(VersionStrategy::Static(
        "9.9.9".to_string(),
    )))
}

/// A Claude Code credentials blob. `expires_in_ms` is relative to now, so a
/// negative value writes the already-expired credential these tests start
/// from.
fn write_credential(path: &std::path::Path, token: &str, expires_in_ms: i64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    std::fs::write(
        path,
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"{token}","refreshToken":"r","expiresAt":{}}}}}"#,
            now + expires_in_ms
        ),
    )
    .unwrap();
}

#[derive(Clone)]
struct Upstream {
    hits: Arc<AtomicUsize>,
    /// When set, the first 401 rewrites this file with `FRESH` — the other
    /// terminal, mid-request.
    rewrite: Option<PathBuf>,
    seen_auth: Arc<std::sync::Mutex<Vec<Option<String>>>>,
}

async fn messages(State(up): State<Upstream>, headers: HeaderMap) -> impl IntoResponse {
    let n = up.hits.fetch_add(1, Ordering::SeqCst);
    up.seen_auth.lock().unwrap().push(
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
    );
    if n == 0 {
        if let Some(p) = &up.rewrite {
            write_credential(p, FRESH, 60 * 60 * 1000);
        }
        return (
            StatusCode::UNAUTHORIZED,
            r#"{"error":{"message":"upstream original body"}}"#,
        );
    }
    // Second attempt: the rewrite case answers it, the unchanged case never
    // reaches here.
    (StatusCode::OK, r#"{"ok":true}"#)
}

async fn spawn_upstream(up: Upstream) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route("/v1/messages", post(messages))
        .with_state(up);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn spawn_gateway(upstream: String, source: TokenSource) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = AppState::with_version(
        upstream,
        "https://o.invalid",
        "https://g.invalid",
        source,
        pinned_version(),
    )
    .unwrap();
    let app = build_router(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(20)).await;
    addr.to_string()
}

fn body() -> &'static str {
    r#"{"model":"claude-sonnet-5","max_tokens":1,"messages":[{"role":"user","content":"hi"}]}"#
}

/// The case the arm exists for: the credential the request was sent with is
/// no longer the credential the store holds, so one retry is worth sending.
#[tokio::test]
async fn a_store_that_changed_under_a_rejection_is_retried() {
    let dir = tempfile::tempdir().unwrap();
    let cred = dir.path().join(".credentials.json");
    write_credential(&cred, STALE, -60_000);

    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let upstream = spawn_upstream(Upstream {
        hits: hits.clone(),
        rewrite: Some(cred.clone()),
        seen_auth: seen.clone(),
    })
    .await;
    let gw = spawn_gateway(upstream, TokenSource::CredentialsFile(cred.clone())).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("content-type", "application/json")
        .body(body())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200, "the retry should have succeeded");
    // Exactly two: first attempt plus one retry. "One retry only" falls out
    // of this count — a loop would keep going, since the store stays newer
    // than the token any single attempt was built with.
    assert_eq!(hits.load(Ordering::SeqCst), 2, "first attempt + one retry");

    #[cfg(has_beta_hook)]
    {
        let seen = seen.lock().unwrap();
        assert!(
            seen[1].as_deref().is_some_and(|h| h.contains(FRESH)),
            "the retry must carry the token the store now holds, not a re-send \
             of the rejected one: {seen:?}"
        );
    }
}

/// The common case, and the one the old wording got wrong: nothing has
/// changed, so there is nothing to retry with. The client gets the actionable
/// body naming its own credential store — not upstream's opaque one, and not
/// a claim that some refresh was attempted.
#[tokio::test]
async fn an_unchanged_store_is_not_retried_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let cred = dir.path().join(".credentials.json");
    write_credential(&cred, STALE, -60_000);

    let hits = Arc::new(AtomicUsize::new(0));
    let upstream = spawn_upstream(Upstream {
        hits: hits.clone(),
        rewrite: None,
        seen_auth: Arc::new(std::sync::Mutex::new(Vec::new())),
    })
    .await;
    let gw = spawn_gateway(upstream, TokenSource::CredentialsFile(cred.clone())).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("content-type", "application/json")
        .body(body())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 401);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "resending an identical token cannot succeed; it must not be sent"
    );
    let text = resp.text().await.unwrap();
    assert!(text.contains("still holds the same token"), "{text}");
    assert!(
        text.contains(&cred.display().to_string()),
        "must name the store the token came from: {text}"
    );
    assert!(
        !text.contains("upstream original body"),
        "upstream's opaque body must be replaced here: {text}"
    );
}

/// Mode 3: the client authenticated itself with a non-OAuth-shape key, so
/// this gateway attached nothing and has no standing to touch the user's
/// credential store or to reword the answer. A third party's bad key must not
/// be able to spend the user's credential on a retry either.
#[tokio::test]
async fn a_client_supplied_credential_401_is_forwarded_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let cred = dir.path().join(".credentials.json");
    write_credential(&cred, STALE, -60_000);

    let hits = Arc::new(AtomicUsize::new(0));
    let upstream = spawn_upstream(Upstream {
        hits: hits.clone(),
        rewrite: Some(cred.clone()),
        seen_auth: Arc::new(std::sync::Mutex::new(Vec::new())),
    })
    .await;
    let gw = spawn_gateway(upstream, TokenSource::CredentialsFile(cred.clone())).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("content-type", "application/json")
        .header("x-api-key", "sk-ant-api03-a-clients-own-key")
        .body(body())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 401);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "no retry on someone else's key"
    );
    let text = resp.text().await.unwrap();
    assert!(
        text.contains("upstream original body"),
        "Mode 3 must forward upstream's body verbatim: {text}"
    );
}

/// The arm must be invisible on the happy path: a 200 reads no credential
/// store, sends nothing twice, and keeps upstream's body.
#[tokio::test]
async fn an_upstream_success_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let cred = dir.path().join(".credentials.json");
    write_credential(&cred, STALE, -60_000);

    // `hits` starts at 1 so the handler takes its 200 branch immediately.
    let hits = Arc::new(AtomicUsize::new(1));
    let upstream = spawn_upstream(Upstream {
        hits: hits.clone(),
        rewrite: None,
        seen_auth: Arc::new(std::sync::Mutex::new(Vec::new())),
    })
    .await;
    let gw = spawn_gateway(upstream, TokenSource::CredentialsFile(cred)).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("content-type", "application/json")
        .body(body())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(hits.load(Ordering::SeqCst), 2, "one call, no retry");
    assert!(resp.text().await.unwrap().contains("\"ok\":true"));
}
