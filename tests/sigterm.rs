//! launchd stops and restarts the service with SIGTERM (`launchctl kickstart -k`,
//! `bootout`, setup.sh reinstalls); systemd does the same. The gateway used to
//! wait only for Ctrl-C, so SIGTERM took the default action — the process died
//! on the spot and every in-flight request was cut. These tests pin the
//! replacement: SIGTERM goes through the same graceful shutdown as Ctrl-C.
#![cfg(unix)]

use httpmock::prelude::*;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn spawn_gateway(upstream: &str, port: u16) -> Child {
    Command::new(env!("CARGO_BIN_EXE_mur-model-gateway"))
        .arg("serve")
        .env("MUR_MODEL_GATEWAY_BIND", format!("127.0.0.1:{port}"))
        .env("MUR_MODEL_GATEWAY_UPSTREAM", upstream)
        .env("MUR_MODEL_GATEWAY_TOKEN_SOURCE", "off")
        .env("MUR_MODEL_GATEWAY_TOKEN_SOURCE_CODEX", "off")
        .env("MUR_MODEL_GATEWAY_OAUTH_KEEPALIVE", "0")
        .env_remove("MUR_MODEL_GATEWAY_COMPRESS")
        .env("RUST_LOG", "info")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gateway binary")
}

async fn wait_ready(port: u16) {
    let url = format!("http://127.0.0.1:{port}/__mur/health");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if reqwest::get(&url).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("gateway never came up on {port}");
}

fn sigterm(child: &Child) {
    let ok = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("run kill")
        .success();
    assert!(ok, "kill -TERM failed");
}

/// Wait for exit without blocking the runtime forever: a gateway that ignores
/// SIGTERM must fail the test, not hang it.
async fn wait_exit(child: &mut Child, within: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + within;
    loop {
        if let Some(st) = child.try_wait().unwrap() {
            return st;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("gateway still running {within:?} after SIGTERM");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn output(child: &mut Child) -> String {
    let mut s = String::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut s);
    }
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut s);
    }
    s
}

#[tokio::test(flavor = "multi_thread")]
async fn sigterm_lets_in_flight_request_finish() {
    let upstream = MockServer::start_async().await;
    let mock = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/v1/messages");
            then.status(200)
                .delay(Duration::from_secs(2))
                .header("content-type", "application/json")
                .body(r#"{"id":"msg_slow","content":[{"type":"text","text":"ok"}]}"#);
        })
        .await;

    let port = free_port();
    let mut child = spawn_gateway(&upstream.base_url(), port);
    wait_ready(port).await;

    let req = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("authorization", "Bearer sk-ant-oat-test")
            .header("content-type", "application/json")
            .body(r#"{"model":"claude-3-5-haiku-latest","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#)
            .send()
            .await
    });

    // Let the request reach the (2 s slow) upstream, then stop the service
    // the way launchd does.
    tokio::time::sleep(Duration::from_millis(500)).await;
    sigterm(&child);

    let resp = req
        .await
        .unwrap()
        .expect("in-flight request was cut by SIGTERM");
    assert_eq!(resp.status(), 200);
    assert!(resp.text().await.unwrap().contains("msg_slow"));
    mock.assert_async().await;

    let st = wait_exit(&mut child, Duration::from_secs(10)).await;
    let out = output(&mut child);
    assert!(
        st.success(),
        "exit status {st:?} — killed by the signal?\n{out}"
    );
    assert!(out.contains("shutdown signal"), "no shutdown line:\n{out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn sigterm_on_idle_gateway_exits_cleanly() {
    let port = free_port();
    let mut child = spawn_gateway("http://127.0.0.1:9", port);
    wait_ready(port).await;

    sigterm(&child);
    let st = wait_exit(&mut child, Duration::from_secs(5)).await;
    let out = output(&mut child);
    assert!(
        st.success(),
        "exit status {st:?} — killed by the signal?\n{out}"
    );
    assert!(out.contains("shutdown signal"), "no shutdown line:\n{out}");
}
