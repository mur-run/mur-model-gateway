//! The expired-vs-revoked branch. A 401 whose stored expiry is still in the
//! future means the token was revoked upstream, and no refresh can help — so
//! the gateway must not spawn anything.

use mur_model_gateway::{Provider, TokenSource, anthropic_retry_eligible};
use reqwest::StatusCode;
use std::sync::Arc;

const NOW_MS: i64 = 1_787_000_000_000;

#[test]
fn expired_token_is_eligible() {
    assert!(anthropic_retry_eligible(
        Provider::Anthropic,
        StatusCode::UNAUTHORIZED,
        &TokenSource::Keychain,
        Some(NOW_MS - 1),
        NOW_MS
    ));
}

#[test]
fn a_source_claude_code_does_not_own_is_never_eligible() {
    // A raw key from the environment, or a test's Static token, is not
    // something `claude auth status` can refresh — asking it to would spawn a
    // process that cannot possibly help. Same principle as the Codex arm's
    // ApiKey case: a 401 on a key means the key is rejected, and resending it
    // cannot succeed.
    for src in [
        TokenSource::EnvVar("ANTHROPIC_API_KEY".into()),
        TokenSource::Static(Arc::new("sk-ant-raw".to_string())),
        TokenSource::Disabled,
    ] {
        assert!(
            !anthropic_retry_eligible(
                Provider::Anthropic,
                StatusCode::UNAUTHORIZED,
                &src,
                Some(NOW_MS - 1),
                NOW_MS
            ),
            "{src:?} must not be eligible"
        );
    }
}

#[test]
fn a_credentials_file_source_is_eligible() {
    // The Linux and Windows install shape: Claude Code owns the file, so a
    // delegated refresh is exactly as applicable as it is for the keychain.
    assert!(anthropic_retry_eligible(
        Provider::Anthropic,
        StatusCode::UNAUTHORIZED,
        &TokenSource::CredentialsFile("/tmp/creds.json".into()),
        Some(NOW_MS - 1),
        NOW_MS
    ));
}

#[test]
fn revoked_token_is_not_eligible() {
    // 401 while the stored expiry is still in the future: revoked, not expired.
    assert!(!anthropic_retry_eligible(
        Provider::Anthropic,
        StatusCode::UNAUTHORIZED,
        &TokenSource::Keychain,
        Some(NOW_MS + 60_000),
        NOW_MS
    ));
}

#[test]
fn unknown_expiry_is_eligible_once() {
    // Older blobs carry no expiresAt. Allow the probe; the cooldown bounds
    // the cost if it turns out to be fruitless.
    assert!(anthropic_retry_eligible(
        Provider::Anthropic,
        StatusCode::UNAUTHORIZED,
        &TokenSource::Keychain,
        None,
        NOW_MS
    ));
}

#[test]
fn non_401_is_never_eligible() {
    assert!(!anthropic_retry_eligible(
        Provider::Anthropic,
        StatusCode::INTERNAL_SERVER_ERROR,
        &TokenSource::Keychain,
        Some(NOW_MS - 1),
        NOW_MS
    ));
}

#[test]
fn other_providers_are_never_eligible() {
    // Codex keeps its own path; OpenAI and Gemini have no delegated owner.
    for p in [Provider::OpenAI, Provider::Gemini, Provider::Codex] {
        assert!(!anthropic_retry_eligible(
            p,
            StatusCode::UNAUTHORIZED,
            &TokenSource::Keychain,
            Some(NOW_MS - 1),
            NOW_MS
        ));
    }
}

#[test]
fn error_body_names_the_fix() {
    let b = mur_model_gateway::anthropic_auth_error_body(&TokenSource::Keychain, true);
    assert!(b.contains("/login anthropic"), "names the fix: {b}");
    assert!(
        b.contains("claude auth login"),
        "names the CLI fallback: {b}"
    );
}

#[test]
fn error_body_names_the_store_the_token_came_from() {
    // A file-backed install must not be told to look in a keychain it does not
    // have. This is the fourth place in this plan where hardcoding the keychain
    // would have been wrong.
    let b = mur_model_gateway::anthropic_auth_error_body(
        &TokenSource::CredentialsFile("/home/u/.claude/.credentials.json".into()),
        true,
    );
    assert!(b.contains("/home/u/.claude/.credentials.json"), "{b}");
    assert!(
        !b.contains("Claude Code-credentials"),
        "must not name the keychain for a file source: {b}"
    );
}

#[test]
fn revoked_body_does_not_promise_a_refresh() {
    // Re-running a refresh cannot fix a revoked credential; saying so would
    // send the user in circles.
    let b = mur_model_gateway::anthropic_auth_error_body(&TokenSource::Keychain, false);
    assert!(b.contains("revoked"), "{b}");
    assert!(!b.contains("expired"), "{b}");
}

#[test]
fn error_body_never_contains_the_token() {
    // describe_credential_store falls through to `{other:?}` for the remaining
    // variants, and TokenSource::Static holds a real token. The redacting Debug
    // added in Task 4 is what keeps this true — this test is its guard from the
    // other side.
    let b = mur_model_gateway::anthropic_auth_error_body(
        &TokenSource::Static(std::sync::Arc::new("sk-ant-secret-value".to_string())),
        true,
    );
    assert!(
        !b.contains("sk-ant-secret-value"),
        "token leaked into an error body: {b}"
    );
}
