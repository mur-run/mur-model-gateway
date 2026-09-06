//! `/__mur/health`: a loopback readiness probe MUR Hub polls before routing a
//! subscription agent through the gateway. It reports *which kind* of
//! credential is on disk — never the credential itself.

use mur_model_gateway::{AppState, TokenSource, build_router};
use std::io::Write;

async fn spawn(anthropic: TokenSource, codex: TokenSource) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let dead = "http://127.0.0.1:9";
    let state = AppState::new(dead, dead, dead, anthropic)
        .unwrap()
        .with_token_source_codex(codex);
    tokio::spawn(async move {
        axum::serve(listener, build_router(state)).await.unwrap();
    });
    format!("http://{addr}/__mur/health")
}

#[tokio::test]
async fn health_is_local_and_non_secret() {
    let url = spawn(TokenSource::Disabled, TokenSource::Disabled).await;
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 200);
    let raw = resp.text().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(json["status"], "ok");
    assert!(json.get("codexHook").unwrap().is_boolean());
    assert_eq!(json["codexCredential"], "missing");
    assert_eq!(json["claudeCredential"], "missing");
    assert!(json.get("compression").unwrap().is_boolean());
    // Triage field: a tester's bug report has to carry the build it came
    // from. Asserted against the crate version rather than a literal so a
    // version bump can't silently desync the two.
    assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
    assert!(!raw.contains("access_token"));
    assert!(!raw.contains("refresh_token"));
    assert!(!raw.contains("accessToken"));
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
        let url = spawn(
            TokenSource::Disabled,
            TokenSource::Codex(f.path().to_path_buf()),
        )
        .await;
        let raw = reqwest::get(&url).await.unwrap().text().await.unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(json["codexCredential"], mode, "{blob}");
        assert!(!raw.contains("SECRET"), "{raw}");
    }
}

#[tokio::test]
async fn health_reports_claude_credential_kind_without_the_credential() {
    for (blob, mode) in [
        (
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-SECRET","refreshToken":"rt-SECRET","expiresAt":1787497765291}}"#,
            "oauth",
        ),
        (r#"{"claudeAiOauth":{}}"#, "missing"),
        ("not json", "missing"),
    ] {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(blob.as_bytes()).unwrap();
        let url = spawn(
            TokenSource::CredentialsFile(f.path().to_path_buf()),
            TokenSource::Disabled,
        )
        .await;
        let raw = reqwest::get(&url).await.unwrap().text().await.unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(json["claudeCredential"], mode, "{blob}");
        assert!(!raw.contains("SECRET"), "{raw}");
        assert!(!raw.contains("sk-ant"), "{raw}");
        assert!(!raw.contains("1787497765291"), "expiry leaked: {raw}");
    }
    // A missing file is `missing`, not an error.
    let url = spawn(
        TokenSource::CredentialsFile("/nonexistent/credentials.json".into()),
        TokenSource::Disabled,
    )
    .await;
    let json: serde_json::Value = reqwest::get(&url).await.unwrap().json().await.unwrap();
    assert_eq!(json["claudeCredential"], "missing");
}
