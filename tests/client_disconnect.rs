//! A client that hangs up before the response starts used to leave no trace:
//! hyper drops the handler future, so neither `proxied` nor `proxy error`
//! ever runs. Chasing one such request meant ruling out a keychain stall, an
//! upstream error, an upstream timeout and a gateway restart one by one.
//! These tests pin the replacement: exactly one `client went away` line for
//! an abandoned request, and none for one that was answered.

use axum::http::StatusCode;
use axum::response::IntoResponse;
use mur_model_gateway::{AppState, TokenSource, build_router};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// Every log line this test process emits, ANSI off. One global subscriber:
/// the gateway runs on spawned tasks, which a thread-local default would miss.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn logs() -> &'static Captured {
    static LOGS: OnceLock<Captured> = OnceLock::new();
    LOGS.get_or_init(|| {
        let cap = Captured::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(cap.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish();
        tracing::subscriber::set_global_default(sub).expect("one global subscriber");
        cap
    })
}

fn lines(cap: &Captured) -> Vec<String> {
    let raw = cap.0.lock().unwrap().clone();
    String::from_utf8_lossy(&raw)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Exact field match, so `path=/v1/messages` never matches
/// `path=/v1/messages/count_tokens` (the two tests share one log).
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}=");
    line.split_whitespace()
        .find_map(|tok| tok.strip_prefix(prefix.as_str()))
}

/// Upstream that answers every path after `hold`.
async fn spawn_upstream(hold: Duration) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new().fallback(move || async move {
        tokio::time::sleep(hold).await;
        (StatusCode::OK, r#"{"ok":true}"#).into_response()
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn spawn_gateway(upstream: &str) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = AppState::new(
        upstream,
        "https://o.invalid",
        "https://g.invalid",
        TokenSource::Disabled,
    )
    .unwrap();
    let app = build_router(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(20)).await;
    addr.to_string()
}

#[tokio::test]
async fn client_hanging_up_mid_request_leaves_one_line() {
    let logs = logs();
    let upstream = spawn_upstream(Duration::from_secs(60)).await;
    let gw = spawn_gateway(&upstream).await;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(300))
        .build()
        .unwrap();
    let err = client
        .post(format!("http://{gw}/v1/messages"))
        .header("content-type", "application/json")
        .body(r#"{"model":"m","messages":[]}"#)
        .send()
        .await
        .expect_err("upstream holds for 60s, so the client must give up first");
    assert!(err.is_timeout(), "expected a client timeout, got {err:#}");

    // The server side notices the closed socket asynchronously.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let gone = loop {
        let hits: Vec<String> = lines(logs)
            .into_iter()
            .filter(|l| l.contains("client went away") && field(l, "path") == Some("/v1/messages"))
            .collect();
        if !hits.is_empty() || tokio::time::Instant::now() >= deadline {
            break hits;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    assert_eq!(
        gone.len(),
        1,
        "want exactly one `client went away` line for /v1/messages; log was:\n{}",
        lines(logs).join("\n")
    );
    let line = &gone[0];
    assert!(
        line.contains(" WARN "),
        "must surface in a WARN|ERROR grep: {line}"
    );
    assert_eq!(field(line, "method"), Some("POST"), "{line}");
    assert_eq!(field(line, "provider"), Some("anthropic"), "{line}");
    let waited: u64 = field(line, "waited_ms")
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("waited_ms missing or not a number: {line}"));
    assert!(
        (250..5_000).contains(&waited),
        "waited_ms should be ~the client's 300ms patience, got {waited}: {line}"
    );
}

#[tokio::test]
async fn answered_request_does_not_claim_the_client_left() {
    let logs = logs();
    let upstream = spawn_upstream(Duration::ZERO).await;
    let gw = spawn_gateway(&upstream).await;

    let path = "/v1/messages/count_tokens";
    let resp = reqwest::Client::new()
        .post(format!("http://{gw}{path}"))
        .header("content-type", "application/json")
        .body(r#"{"model":"m","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    resp.bytes().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let all = lines(logs);
    // Guard against a vacuous pass: the capture must have seen this request.
    assert!(
        all.iter()
            .any(|l| l.contains("proxied") && field(l, "path") == Some(path)),
        "capture never saw `proxied` for {path}; log was:\n{}",
        all.join("\n")
    );
    assert!(
        !all.iter()
            .any(|l| l.contains("client went away") && field(l, "path") == Some(path)),
        "answered request logged as abandoned; log was:\n{}",
        all.join("\n")
    );
}
