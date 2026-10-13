//! End-to-end proof of the `cache trace` line (spec:
//! docs/specs/2026-10-13-cache-trace-design.md): one line per Anthropic
//! `/v1/messages` request pairing request-side fingerprints and recency with
//! the response's usage; nothing for `count_tokens`; the 429 rollback; the
//! pending-writer race; and Assumption 1 (the 401 retry re-sends identical
//! bytes and leaves exactly one line).
//!
//! Each test spawns its own gateway (own tables) and tags its requests with a
//! unique user-agent, because the log capture is shared by the test binary.

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use mur_model_gateway::cc_version::{VersionCache, VersionStrategy};
use mur_model_gateway::{AppState, TokenSource, build_router};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// Exact field match: `key=` followed by the token.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}=");
    line.split_whitespace()
        .find_map(|tok| tok.strip_prefix(prefix.as_str()))
}

/// Every `cache trace` line for one client tag, in emission order.
fn traces(tag: &str) -> Vec<String> {
    lines(logs())
        .into_iter()
        .filter(|l| l.contains("cache trace ") && field(l, "client") == Some(tag))
        .collect()
}

async fn wait_for_traces(tag: &str, n: usize) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let t = traces(tag);
        if t.len() >= n || tokio::time::Instant::now() >= deadline {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn num(line: &str, key: &str) -> i64 {
    field(line, key)
        .unwrap_or_else(|| panic!("no {key} in {line}"))
        .parse()
        .unwrap_or_else(|_| panic!("{key} is not a number in {line}"))
}

const START: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_t1\",\"model\":\"claude-sonnet-5-5\",\"usage\":{\"input_tokens\":12,\"cache_creation_input_tokens\":300,\"cache_read_input_tokens\":0,\"cache_creation\":{\"ephemeral_5m_input_tokens\":300,\"ephemeral_1h_input_tokens\":0}}}}\n\n";
const STOP: &str = "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

const STALE: &str = "sk-ant-oat01-stale";
const FRESH: &str = "sk-ant-oat01-fresh";

/// A Claude Code credentials blob; `expires_in_ms` is relative to now.
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

#[derive(Clone, Default)]
struct Upstream {
    hits: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
    /// When set, the first hit answers 401 and rewrites this credentials
    /// file, standing in for another terminal refreshing the token.
    rewrite: Option<PathBuf>,
}

/// A two-frame SSE body: `message_start` after `before_start`, then
/// `message_stop` after a further `after_start`.
fn sse(before_start: Duration, after_start: Duration) -> Response {
    let frames = vec![(before_start, START), (after_start, STOP)];
    let stream = futures_util::stream::unfold(frames.into_iter(), |mut it| async move {
        let (delay, text) = it.next()?;
        tokio::time::sleep(delay).await;
        Some((Ok::<_, std::io::Error>(axum::body::Bytes::from(text)), it))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

async fn messages(
    State(up): State<Upstream>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let n = up.hits.fetch_add(1, Ordering::SeqCst);
    up.bodies.lock().unwrap().push(body.to_vec());
    if let Some(p) = &up.rewrite
        && n == 0
    {
        write_credential(p, FRESH, 60 * 60 * 1000);
        return (
            StatusCode::UNAUTHORIZED,
            r#"{"error":{"message":"expired"}}"#,
        )
            .into_response();
    }
    let mode = headers
        .get("x-test-mode")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("sse");
    match mode {
        "429" => (
            StatusCode::TOO_MANY_REQUESTS,
            [("content-type", "application/json")],
            r#"{"error":{"type":"rate_limit_error"}}"#,
        )
            .into_response(),
        "slow-start" => sse(Duration::from_millis(1500), Duration::ZERO),
        "slow-tail" => sse(Duration::ZERO, Duration::from_millis(1500)),
        _ => sse(Duration::ZERO, Duration::ZERO),
    }
}

async fn count_tokens() -> Response {
    (
        StatusCode::OK,
        [("content-type", "application/json")],
        r#"{"input_tokens":5}"#,
    )
        .into_response()
}

async fn spawn_upstream(up: Upstream) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .with_state(up);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn pinned_version() -> Arc<VersionCache> {
    Arc::new(VersionCache::new(VersionStrategy::Static(
        "9.9.9".to_string(),
    )))
}

async fn spawn_gateway(upstream: &str, source: TokenSource) -> String {
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

/// One tool, two system blocks with the breakpoint on the second (the
/// runtime's shape), `turns` alternating messages, no message breakpoint.
fn body(cwd: &str, turns: usize) -> String {
    let messages: Vec<serde_json::Value> = (0..turns)
        .map(|i| {
            serde_json::json!({
                "role": if i % 2 == 0 { "user" } else { "assistant" },
                "content": [{"type": "text", "text": format!("turn {i}")}]
            })
        })
        .collect();
    serde_json::json!({
        "model": "claude-sonnet-5-5",
        "max_tokens": 8,
        "stream": true,
        "tools": [{"name": "read", "description": "Read a file",
                   "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}}],
        "system": [
            {"type": "text", "text": "You are a careful assistant."},
            {"type": "text", "text": format!("Working directory: {cwd}"),
             "cache_control": {"type": "ephemeral"}}
        ],
        "messages": messages
    })
    .to_string()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

async fn call(gw: &str, tag: &str, mode: &str, body: String) -> reqwest::Response {
    client()
        .post(format!("http://{gw}/v1/messages"))
        .header("content-type", "application/json")
        .header("user-agent", format!("{tag}/1.0"))
        .header("x-test-mode", mode)
        .body(body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn the_same_prefix_twice_is_recent_and_diverges_in_messages() {
    let _ = logs();
    let gw = spawn_gateway(
        &spawn_upstream(Upstream::default()).await,
        TokenSource::Disabled,
    )
    .await;
    let r = call(&gw, "t1", "sse", body("/w/one", 1)).await;
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    let r = call(&gw, "t1", "sse", body("/w/one", 3)).await;
    r.bytes().await.unwrap();

    let t = wait_for_traces("t1", 2).await;
    assert_eq!(t.len(), 2, "{t:#?}");
    let (a, b) = (&t[0], &t[1]);
    assert_eq!(field(a, "diverged"), Some("first"));
    assert_eq!(field(a, "prefix_seen_ago_ms"), Some("-"));
    assert_eq!(field(a, "prefix_ttl_left_ms"), Some("-"));
    assert_eq!(field(a, "tools_ttl_left_ms"), Some("-"));
    assert_eq!(field(a, "bp"), Some("tools:none,system:last,msgs:none"));
    assert_eq!(field(a, "bp_ttls"), Some("5m"));
    assert_eq!(field(a, "ttl"), Some("5m"));
    assert_eq!(field(a, "ttl_max"), Some("5m"));
    assert_eq!(field(a, "status"), Some("200"));
    assert_eq!(field(a, "streaming"), Some("true"));
    assert_eq!(field(a, "compress_on"), Some("0"));
    assert_eq!(field(a, "req_id"), Some("msg_t1"));
    assert_eq!(field(a, "model"), Some("claude-sonnet-5-5"));
    assert_eq!(field(a, "input"), Some("12"));
    assert_eq!(field(a, "read"), Some("0"));
    assert_eq!(field(a, "creation"), Some("300"));
    assert_eq!(field(a, "creation_5m"), Some("300"));
    assert_eq!(field(a, "creation_1h"), Some("0"));
    assert_eq!(field(a, "msgs"), Some("1"));
    assert_eq!(field(a, "tools_count"), Some("1"));
    assert_eq!(field(a, "system_blocks"), Some("2"));
    assert_eq!(field(b, "diverged"), Some("msgs:1"));
    assert_eq!(field(b, "msgs"), Some("3"));
    assert_eq!(field(b, "inflight_same_prefix"), Some("0"));
    let seen = num(b, "prefix_seen_ago_ms");
    let left = num(b, "prefix_ttl_left_ms");
    assert!(
        (0..300_000).contains(&seen) && (1..=300_000).contains(&left),
        "{b}"
    );
    assert!(num(b, "tools_ttl_left_ms") > 0, "{b}");
    assert_eq!(field(a, "prefix_fp"), field(b, "prefix_fp"));
}

#[tokio::test]
async fn a_different_cwd_is_a_new_prefix_with_recent_tools() {
    let _ = logs();
    let gw = spawn_gateway(
        &spawn_upstream(Upstream::default()).await,
        TokenSource::Disabled,
    )
    .await;
    call(&gw, "t2", "sse", body("/w/one", 1))
        .await
        .bytes()
        .await
        .unwrap();
    call(&gw, "t2", "sse", body("/w/two", 1))
        .await
        .bytes()
        .await
        .unwrap();

    let t = wait_for_traces("t2", 2).await;
    assert_eq!(t.len(), 2, "{t:#?}");
    let b = &t[1];
    // canonical system block 1 is {"text":"Working directory: /w/one","type":"text"}:
    // byte 31 is the first that differs
    assert_eq!(field(b, "diverged"), Some("system:1@31"));
    assert_eq!(field(b, "prefix_seen_ago_ms"), Some("-"));
    assert_eq!(field(b, "prefix_ttl_left_ms"), Some("-"));
    assert!(num(b, "tools_seen_ago_ms") >= 0, "{b}");
    assert!(num(b, "tools_ttl_left_ms") > 0, "{b}");
    assert_ne!(field(&t[0], "prefix_fp"), field(b, "prefix_fp"));
    assert_eq!(field(&t[0], "tools_fp"), field(b, "tools_fp"));
}

#[tokio::test]
async fn a_failed_send_is_rolled_back() {
    let _ = logs();
    let gw = spawn_gateway(
        &spawn_upstream(Upstream::default()).await,
        TokenSource::Disabled,
    )
    .await;
    call(&gw, "t3", "sse", body("/w/f", 1))
        .await
        .bytes()
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let r = call(&gw, "t3", "429", body("/w/f", 1)).await;
    assert_eq!(r.status(), 429);
    call(&gw, "t3", "sse", body("/w/f", 1))
        .await
        .bytes()
        .await
        .unwrap();

    let t = wait_for_traces("t3", 3).await;
    assert_eq!(t.len(), 3, "{t:#?}");
    let failed = &t[1];
    assert_eq!(field(failed, "status"), Some("429"));
    assert_eq!(field(failed, "streaming"), Some("false"));
    assert_eq!(field(failed, "read"), Some("-"));
    assert_eq!(field(failed, "creation"), Some("-"));
    assert_eq!(field(failed, "req_id"), Some("-"));
    assert_eq!(field(failed, "model"), Some("claude-sonnet-5-5"));
    let third = &t[2];
    // rolled back to the first send, which was at least 300 ms ago; without
    // the rollback this would be the few ms since the 429
    assert!(num(third, "prefix_seen_ago_ms") >= 300, "{third}");
    assert!(num(third, "prefix_ttl_left_ms") > 0, "{third}");
    assert_eq!(field(third, "inflight_same_prefix"), Some("0"));
}

#[tokio::test]
async fn count_tokens_leaves_no_line() {
    let _ = logs();
    let gw = spawn_gateway(
        &spawn_upstream(Upstream::default()).await,
        TokenSource::Disabled,
    )
    .await;
    let r = client()
        .post(format!("http://{gw}/v1/messages/count_tokens"))
        .header("content-type", "application/json")
        .header("user-agent", "t4/1.0")
        .body(body("/w/c", 1))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    // a real request afterwards proves the capture is live for this tag
    call(&gw, "t4", "sse", body("/w/c", 1))
        .await
        .bytes()
        .await
        .unwrap();
    let t = wait_for_traces("t4", 1).await;
    assert_eq!(t.len(), 1, "{t:#?}");
    assert_eq!(field(&t[0], "diverged"), Some("first"));
}

#[tokio::test]
async fn a_pending_writer_is_a_race_but_a_begun_one_is_not() {
    let _ = logs();
    let gw = spawn_gateway(
        &spawn_upstream(Upstream::default()).await,
        TokenSource::Disabled,
    )
    .await;

    // A's first chunk leaves after 1.5 s; B goes out 300 ms after A.
    let gw_a = gw.clone();
    let a = tokio::spawn(async move {
        call(&gw_a, "t5", "slow-start", body("/w/r", 1))
            .await
            .bytes()
            .await
            .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    call(&gw, "t5", "sse", body("/w/r", 2))
        .await
        .bytes()
        .await
        .unwrap();
    a.await.unwrap();
    let t = wait_for_traces("t5", 2).await;
    let b = t
        .iter()
        .find(|l| field(l, "msgs") == Some("2"))
        .expect("B's line");
    assert_eq!(field(b, "inflight_same_prefix"), Some("1"), "{b}");
    assert_eq!(field(b, "prefix_ttl_left_ms"), Some("-"), "{b}");
    assert_eq!(field(b, "inflight_same_tools"), Some("1"), "{b}");

    // C's first chunk leaves at once and its tail 1.5 s later; D goes out
    // 300 ms after C: the entry is readable, no race.
    let gw_c = gw.clone();
    let c = tokio::spawn(async move {
        call(&gw_c, "t5", "slow-tail", body("/w/r", 3))
            .await
            .bytes()
            .await
            .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    call(&gw, "t5", "sse", body("/w/r", 4))
        .await
        .bytes()
        .await
        .unwrap();
    c.await.unwrap();
    let t = wait_for_traces("t5", 4).await;
    let d = t
        .iter()
        .find(|l| field(l, "msgs") == Some("4"))
        .expect("D's line");
    assert_eq!(field(d, "inflight_same_prefix"), Some("0"), "{d}");
    assert!(num(d, "prefix_ttl_left_ms") > 0, "{d}");
}

/// Assumption 1 of the spec: the 401 retry re-sends byte-identical bytes,
/// so one `on_send` per request is sound.
#[tokio::test]
async fn the_401_retry_resends_identical_bytes_and_leaves_one_line() {
    let _ = logs();
    let dir = tempfile::tempdir().unwrap();
    let cred = dir.path().join(".credentials.json");
    write_credential(&cred, STALE, -60_000);
    let up = Upstream {
        rewrite: Some(cred.clone()),
        ..Default::default()
    };
    let hits = up.hits.clone();
    let bodies = up.bodies.clone();
    let gw = spawn_gateway(
        &spawn_upstream(up).await,
        TokenSource::CredentialsFile(cred),
    )
    .await;

    let r = call(&gw, "t6", "sse", body("/w/401", 1)).await;
    assert_eq!(r.status(), 200, "the retry should have succeeded");
    r.bytes().await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 2, "first attempt + one retry");
    {
        let b = bodies.lock().unwrap();
        assert_eq!(
            b[0], b[1],
            "the retry must re-send the bytes the trace fingerprinted"
        );
    }
    let t = wait_for_traces("t6", 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let again = traces("t6");
    assert_eq!(t.len(), 1, "{t:#?}");
    assert_eq!(again.len(), 1, "a second line appeared late: {again:#?}");
    assert_eq!(field(&t[0], "status"), Some("200"));
    assert_eq!(field(&t[0], "read"), Some("0"));
}
