//! `/__mur/health`: a loopback readiness probe MUR Hub polls before routing a
//! ChatGPT-subscription agent through the gateway. It reports *which kind*
//! of Codex credential is on disk — never the credential itself.

use mur_model_gateway::{AppState, TokenSource, build_router};
use std::io::Write;

async fn spawn(ts: TokenSource) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let dead = "http://127.0.0.1:9";
    let state = AppState::new(dead, dead, dead, TokenSource::Disabled)
        .unwrap()
        .with_token_source_codex(ts);
    tokio::spawn(async move {
        axum::serve(listener, build_router(state)).await.unwrap();
    });
    format!("http://{addr}/__mur/health")
}

#[tokio::test]
async fn health_is_local_and_non_secret() {
    let url = spawn(TokenSource::Disabled).await;
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 200);
    let raw = resp.text().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(json["status"], "ok");
    assert!(json.get("codexHook").unwrap().is_boolean());
    assert_eq!(json["codexCredential"], "missing");
    assert!(json.get("compression").unwrap().is_boolean());
    assert!(!raw.contains("access_token"));
    assert!(!raw.contains("refresh_token"));
}

#[tokio::test]
async fn health_reports_credential_mode_without_the_credential() {
    for (blob, mode) in [
        (
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"at-SECRET","refresh_token":"rt-SECRET","account_id":"acct-SECRET"}}"#,
            "chatgpt",
        ),
        (
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-SECRET"}"#,
            "apikey",
        ),
        (r#"{"auth_mode":"apikey"}"#, "missing"),
        ("not json", "missing"),
    ] {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(blob.as_bytes()).unwrap();
        let url = spawn(TokenSource::Codex(f.path().to_path_buf())).await;
        let raw = reqwest::get(&url).await.unwrap().text().await.unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(json["codexCredential"], mode, "{blob}");
        assert!(!raw.contains("SECRET"), "{raw}");
    }
}
