//! The client-facing 401 for a credential the gateway itself attached: which
//! store it names, and which of the three things that can happen to such a
//! request it reports.
//!
//! The retry that runs before it is covered by `anthropic_retry_arm.rs`.

use mur_model_gateway::TokenSource;

#[test]
fn error_body_names_the_fix() {
    let b = mur_model_gateway::anthropic_auth_error_body(&TokenSource::Keychain, true, false);
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
        false,
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
    let b = mur_model_gateway::anthropic_auth_error_body(&TokenSource::Keychain, false, false);
    assert!(b.contains("revoked"), "{b}");
    assert!(!b.contains("expired"), "{b}");
}

/// The wording this whole change exists for. The old body said "an automatic
/// refresh did not resolve it" no matter what happened — including the case
/// where the store had not moved and nothing was ever attempted, which is what
/// sent a user to `claude auth login` during an outage that repaired itself
/// three minutes later.
#[test]
fn an_unchanged_store_says_the_token_is_the_same_one() {
    let b = mur_model_gateway::anthropic_auth_error_body(&TokenSource::Keychain, true, false);
    assert!(b.contains("still holds the same token"), "{b}");
    assert!(
        !b.contains("refresh"),
        "nothing was refreshed; do not imply otherwise: {b}"
    );
}

/// `retried` outranks `expired`: the newer credential is the one that just
/// failed, so the age of its predecessor is not the reader's problem.
#[test]
fn a_retried_newer_credential_is_reported_as_the_one_rejected() {
    for expired in [true, false] {
        let b = mur_model_gateway::anthropic_auth_error_body(&TokenSource::Keychain, expired, true);
        assert!(
            b.contains("newer stored credential"),
            "expired={expired}: {b}"
        );
        assert!(
            !b.contains("still holds the same token"),
            "expired={expired}: {b}"
        );
    }
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
        false,
    );
    assert!(
        !b.contains("sk-ant-secret-value"),
        "token leaked into an error body: {b}"
    );
}
