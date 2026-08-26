//! Cross-platform OS keychain reader for the OAuth token Claude Code stores.
//!
//! Uses the [`keyring`] crate which dispatches to:
//!   - **macOS**: Security framework (`SecKeychainItem`)
//!   - **Linux**: libsecret via D-Bus (gnome-keyring / KWallet front-end)
//!   - **Windows**: Credential Manager (`CredRead`)
//!
//! The token's stored under generic-password service `Claude Code-credentials`
//! with the OS account = current username. The blob is JSON; we extract
//! `claudeAiOauth.accessToken`. Reads are cached for [`CACHE_TTL`]: on macOS
//! every uncached read can pop a keychain permission dialog (and a rebuilt
//! binary always does), so per-request reads turn into a dialog storm.
//! Claude Code refreshes the token on the order of hours; 60s staleness is fine.

use serde_json::Value;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const SERVICE: &str = "Claude Code-credentials";

// pub(crate): also memoises `codex::refreshed_access_token`'s refresh cache,
// so the two don't drift onto different staleness windows.
pub(crate) const CACHE_TTL: Duration = Duration::from_secs(60);
type CachedRead = Option<(Instant, Result<Option<OauthCredential>, KeychainError>)>;
static CACHE: Mutex<CachedRead> = Mutex::new(None);

#[derive(Debug, Clone, thiserror::Error)]
pub enum KeychainError {
    #[error("keychain backend error: {0}")]
    Backend(String),
    #[error("keychain entry malformed: {0}")]
    Malformed(String),
}

/// A Claude Code OAuth credential: the token the gateway forwards, plus the
/// non-secret expiry that shipped with it. The refresh token is deliberately
/// NOT represented here — the gateway never redeems it (see the spec's
/// Rejected section), so it must not be able to leak it either.
#[derive(Clone, PartialEq, Eq)]
pub struct OauthCredential {
    pub access_token: String,
    /// `claudeAiOauth.expiresAt`, milliseconds since the Unix epoch.
    /// `None` when the blob omits it — treat as "unknown", never as expired.
    pub expires_at_ms: Option<i64>,
}

/// Hand-written, not derived: this crate's discipline is that any `Debug` on
/// a type holding a live secret must redact it (see `CodexAuth`/
/// `CodexCredential` in `codex.rs`). A derived impl here would print
/// `access_token` in full on any `{:?}` — a log line, a `dbg!()`, a panic
/// message — which is exactly the leak the redaction discipline exists to
/// prevent.
impl std::fmt::Debug for OauthCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OauthCredential")
            .field("access_token", &"<redacted>")
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

/// Read the current Claude Code OAuth credential from the OS keychain.
///
/// `Ok(Some)` — entry found and parsed.
/// `Ok(None)` — no entry exists (Claude Code never logged in).
/// `Err(_)` — backend error (locked keychain / permission denied / parse failure).
pub fn read_claude_code_credential() -> Result<Option<OauthCredential>, KeychainError> {
    cached(&CACHE, CACHE_TTL, read_keychain_uncached)
}

fn read_keychain_uncached() -> Result<Option<OauthCredential>, KeychainError> {
    let user = whoami::username();
    let entry = keyring::Entry::new(SERVICE, &user)
        .map_err(|e| KeychainError::Backend(format!("entry::new({SERVICE}, {user}): {e}")))?;
    match entry.get_password() {
        Ok(raw) => parse_oauth_blob(&raw),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(KeychainError::Backend(format!("get_password: {e}"))),
    }
}

/// TTL cache. The lock is held across `fetch` on purpose: concurrent requests
/// trigger at most one macOS keychain permission dialog instead of one each,
/// and once it's answered the rest are served from cache.
fn cached<T: Clone>(
    cache: &Mutex<Option<(Instant, Result<T, KeychainError>)>>,
    ttl: Duration,
    fetch: impl FnOnce() -> Result<T, KeychainError>,
) -> Result<T, KeychainError> {
    let mut slot = cache.lock().unwrap();
    if let Some((at, res)) = slot.as_ref()
        && at.elapsed() < ttl
    {
        return res.clone();
    }
    let res = fetch();
    *slot = Some((Instant::now(), res.clone()));
    res
}

/// Drop the memoised read so the next call hits the store. Used after an
/// external process is believed to have rewritten the credential — a cached
/// read would otherwise return the token we already know is dead for up to
/// `CACHE_TTL`.
pub fn invalidate_cache() {
    *CACHE.lock().unwrap() = None;
}

/// Default path of Claude Code's on-disk credentials file (Linux/Windows/WSL
/// installs that don't use the OS keychain): `~/.claude/.credentials.json`.
pub fn default_credentials_path() -> Option<std::path::PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().join(".claude/.credentials.json"))
}

/// Read a credential from a Claude Code credentials JSON file (same blob).
pub fn read_credentials_file_credential(
    path: &std::path::Path,
) -> Result<Option<OauthCredential>, KeychainError> {
    match std::fs::read_to_string(path) {
        Ok(raw) => parse_oauth_blob(&raw),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(KeychainError::Backend(format!(
            "read {}: {e}",
            path.display()
        ))),
    }
}

/// Extract `claudeAiOauth.{accessToken,expiresAt}` from a Claude Code blob.
fn parse_oauth_blob(raw: &str) -> Result<Option<OauthCredential>, KeychainError> {
    let creds: Value = serde_json::from_str(raw.trim())
        .map_err(|e| KeychainError::Malformed(format!("not JSON: {e}")))?;
    let oauth = creds
        .get("claudeAiOauth")
        .ok_or_else(|| KeychainError::Malformed("missing claudeAiOauth".into()))?;
    let access_token = oauth
        .get("accessToken")
        .and_then(|t| t.as_str())
        .ok_or_else(|| KeychainError::Malformed("missing claudeAiOauth.accessToken".into()))?
        .to_string();
    Ok(Some(OauthCredential {
        access_token,
        expires_at_ms: oauth.get("expiresAt").and_then(Value::as_i64),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extracts_access_token() {
        let blob = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-test","refreshToken":"x"}}"#;
        assert_eq!(
            parse_oauth_blob(blob).unwrap().unwrap().access_token,
            "sk-ant-oat01-test"
        );
    }

    #[test]
    fn parse_rejects_non_json() {
        let r = parse_oauth_blob("not json");
        assert!(matches!(r, Err(KeychainError::Malformed(_))));
    }

    #[test]
    fn parse_rejects_missing_field() {
        let r = parse_oauth_blob(r#"{"claudeAiOauth":{}}"#);
        assert!(matches!(r, Err(KeychainError::Malformed(_))));
    }

    #[test]
    fn parse_rejects_wrong_shape() {
        let r = parse_oauth_blob(r#"{"foo":"bar"}"#);
        assert!(matches!(r, Err(KeychainError::Malformed(_))));
    }

    #[test]
    fn parse_blob_keeps_expiry() {
        let blob = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-test","refreshToken":"x","expiresAt":1787497765291}}"#;
        let c = parse_oauth_blob(blob).unwrap().unwrap();
        assert_eq!(c.access_token, "sk-ant-oat01-test");
        assert_eq!(c.expires_at_ms, Some(1_787_497_765_291));
    }

    #[test]
    fn parse_blob_without_expiry_is_still_valid() {
        // Older Claude Code writes omitted expiresAt. A missing expiry must
        // not fail the read — it degrades to "unknown", never to an error.
        let blob = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-test"}}"#;
        let c = parse_oauth_blob(blob).unwrap().unwrap();
        assert_eq!(c.access_token, "sk-ant-oat01-test");
        assert_eq!(c.expires_at_ms, None);
    }

    #[test]
    fn parse_blob_ignores_non_integer_expiry() {
        let blob = r#"{"claudeAiOauth":{"accessToken":"t","expiresAt":"soon"}}"#;
        assert_eq!(parse_oauth_blob(blob).unwrap().unwrap().expires_at_ms, None);
    }

    #[test]
    fn credentials_file_reads_token() {
        let dir = std::env::temp_dir().join("mur-model-gateway-cred-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("creds.json");
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-file"}}"#,
        )
        .unwrap();
        assert_eq!(
            read_credentials_file_credential(&path)
                .unwrap()
                .unwrap()
                .access_token,
            "sk-ant-oat01-file"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn credentials_file_missing_is_none() {
        let r = read_credentials_file_credential(std::path::Path::new("/nonexistent/creds.json"));
        assert!(matches!(r, Ok(None)));
    }

    #[test]
    fn credentials_file_garbage_is_malformed() {
        let dir = std::env::temp_dir().join("mur-model-gateway-cred-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("garbage.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(matches!(
            read_credentials_file_credential(&path),
            Err(KeychainError::Malformed(_))
        ));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn cached_serves_within_ttl_and_refetches_after_expiry() {
        // Local String-payload cache: this test is about TTL mechanics, not
        // credential shape, so it deliberately doesn't reuse `CachedRead`
        // (which now holds `OauthCredential`).
        #[allow(clippy::type_complexity)]
        let cache: Mutex<Option<(Instant, Result<Option<String>, KeychainError>)>> =
            Mutex::new(None);
        let ttl = Duration::from_secs(60);
        let r1 = cached(&cache, ttl, || Ok(Some("first".into())));
        let r2 = cached(&cache, ttl, || Ok(Some("second".into())));
        assert_eq!(r1.unwrap().as_deref(), Some("first"));
        assert_eq!(r2.unwrap().as_deref(), Some("first")); // served from cache

        let r3 = cached(&cache, Duration::ZERO, || Ok(Some("third".into())));
        assert_eq!(r3.unwrap().as_deref(), Some("third")); // expired → refetched
    }

    #[test]
    fn cached_caches_errors_too() {
        // A denied keychain prompt must not re-prompt on every retry.
        // Same rationale as above: String payload, independent of `CachedRead`.
        #[allow(clippy::type_complexity)]
        let cache: Mutex<Option<(Instant, Result<Option<String>, KeychainError>)>> =
            Mutex::new(None);
        let ttl = Duration::from_secs(60);
        let r1 = cached(&cache, ttl, || Err(KeychainError::Backend("denied".into())));
        let r2 = cached(&cache, ttl, || Ok(Some("never-fetched".into())));
        assert!(matches!(r1, Err(KeychainError::Backend(_))));
        assert!(matches!(r2, Err(KeychainError::Backend(_))));
    }

    /// I3: `OauthCredential` holds a live access token and its `Debug` is
    /// hand-written, not derived — this proves the redaction actually
    /// happens rather than merely compiling. Would fail against a plain
    /// `#[derive(Debug)]`: the raw token would appear verbatim in `dbg`.
    #[test]
    fn oauth_credential_debug_redacts_the_access_token() {
        let cred = OauthCredential {
            access_token: "sk-ant-oat01-super-secret".to_string(),
            expires_at_ms: Some(1_787_497_765_291),
        };
        let dbg = format!("{cred:?}");
        assert!(
            !dbg.contains("sk-ant-oat01-super-secret"),
            "access_token leaked into Debug output: {dbg}"
        );
        assert!(
            dbg.contains("<redacted>"),
            "Debug output must show the field was deliberately redacted: {dbg}"
        );
        assert!(
            dbg.contains("1787497765291"),
            "expires_at_ms is not a secret and must still be visible: {dbg}"
        );
    }
}
