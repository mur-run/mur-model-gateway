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

const CACHE_TTL: Duration = Duration::from_secs(60);
type CachedRead = Option<(Instant, Result<Option<String>, KeychainError>)>;
static CACHE: Mutex<CachedRead> = Mutex::new(None);

#[derive(Debug, Clone, thiserror::Error)]
pub enum KeychainError {
    #[error("keychain backend error: {0}")]
    Backend(String),
    #[error("keychain entry malformed: {0}")]
    Malformed(String),
}

/// Read the current Claude Code OAuth access token from the OS keychain.
///
/// `Ok(Some)` — entry found and parsed.
/// `Ok(None)` — no entry exists (Claude Code never logged in).
/// `Err(_)` — backend error (locked keychain / permission denied / parse failure).
pub fn read_claude_code_oauth() -> Result<Option<String>, KeychainError> {
    cached(&CACHE, CACHE_TTL, read_keychain_uncached)
}

fn read_keychain_uncached() -> Result<Option<String>, KeychainError> {
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
fn cached(
    cache: &Mutex<CachedRead>,
    ttl: Duration,
    fetch: impl FnOnce() -> Result<Option<String>, KeychainError>,
) -> Result<Option<String>, KeychainError> {
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

/// Default path of Claude Code's on-disk credentials file (Linux/Windows/WSL
/// installs that don't use the OS keychain): `~/.claude/.credentials.json`.
pub fn default_credentials_path() -> Option<std::path::PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().join(".claude/.credentials.json"))
}

/// Read the OAuth token from a Claude Code credentials JSON file.
/// Same blob shape as the keychain entry. `Ok(None)` if the file doesn't exist.
pub fn read_credentials_file(path: &std::path::Path) -> Result<Option<String>, KeychainError> {
    match std::fs::read_to_string(path) {
        Ok(raw) => parse_oauth_blob(&raw),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(KeychainError::Backend(format!(
            "read {}: {e}",
            path.display()
        ))),
    }
}

/// Extract `claudeAiOauth.accessToken` from a Claude Code keychain blob.
fn parse_oauth_blob(raw: &str) -> Result<Option<String>, KeychainError> {
    let creds: Value = serde_json::from_str(raw.trim())
        .map_err(|e| KeychainError::Malformed(format!("not JSON: {e}")))?;
    let token = creds
        .get("claudeAiOauth")
        .and_then(|o| o.get("accessToken"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| KeychainError::Malformed("missing claudeAiOauth.accessToken".into()))?;
    Ok(Some(token.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extracts_access_token() {
        let blob = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-test","refreshToken":"x"}}"#;
        assert_eq!(
            parse_oauth_blob(blob).unwrap().as_deref(),
            Some("sk-ant-oat01-test")
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
            read_credentials_file(&path).unwrap().as_deref(),
            Some("sk-ant-oat01-file")
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn credentials_file_missing_is_none() {
        let r = read_credentials_file(std::path::Path::new("/nonexistent/creds.json"));
        assert!(matches!(r, Ok(None)));
    }

    #[test]
    fn credentials_file_garbage_is_malformed() {
        let dir = std::env::temp_dir().join("mur-model-gateway-cred-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("garbage.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(matches!(
            read_credentials_file(&path),
            Err(KeychainError::Malformed(_))
        ));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn cached_serves_within_ttl_and_refetches_after_expiry() {
        let cache: Mutex<CachedRead> = Mutex::new(None);
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
        let cache: Mutex<CachedRead> = Mutex::new(None);
        let ttl = Duration::from_secs(60);
        let r1 = cached(&cache, ttl, || Err(KeychainError::Backend("denied".into())));
        let r2 = cached(&cache, ttl, || Ok(Some("never-fetched".into())));
        assert!(matches!(r1, Err(KeychainError::Backend(_))));
        assert!(matches!(r2, Err(KeychainError::Backend(_))));
    }
}
