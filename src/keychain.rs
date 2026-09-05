//! Cross-platform OS keychain reader for the OAuth token Claude Code stores.
//!
//! Uses the [`keyring`] crate which dispatches to:
//!   - **macOS**: Security framework (`SecKeychainItem`)
//!   - **Linux**: libsecret via D-Bus (gnome-keyring / KWallet front-end)
//!   - **Windows**: Credential Manager (`CredRead`)
//!
//! The token's stored under generic-password service `Claude Code-credentials`
//! with the OS account = current username. The blob is JSON; we extract
//! `claudeAiOauth.accessToken`. Reads are cached until the credential's own
//! `expiresAt` (minus [`EXPIRY_MARGIN`], capped at [`MAX_CACHE_TTL`]): on macOS
//! every uncached read re-runs the item's ACL authorization, which pops a
//! keychain permission dialog whenever the grant doesn't match — after an
//! upgrade, or after Claude Code rewrites the item. A flat [`CACHE_TTL`]
//! turned that into a dialog *every minute, forever*, because a long-lived
//! daemon re-reads on every request: ~1440 authorizations a day for a value
//! that rotates ~3 times a day. Keying the TTL to the expiry the blob already
//! carries makes it one read per rotation, so one dialog per rotation at
//! worst. A 401 still forces a fresh read via [`invalidate_cache`], so a
//! revoked-before-expiry token is not cached past its usefulness.

use serde_json::Value;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const SERVICE: &str = "Claude Code-credentials";

// pub(crate): also memoises `codex::refreshed_access_token`'s refresh cache,
// so the two don't drift onto different staleness windows.
//
// This is now the *floor* — the TTL used when a read yields no usable expiry
// (backend error, no entry, or a blob with no `expiresAt`). A credential that
// does carry an expiry is cached until it, via `credential_ttl`.
pub(crate) const CACHE_TTL: Duration = Duration::from_secs(60);

/// Never hold a credential longer than this, however far out its `expiresAt`
/// claims to be. Bounds the damage if a blob carries a bogus expiry.
const MAX_CACHE_TTL: Duration = Duration::from_secs(8 * 60 * 60);

/// Re-read this long before the stored expiry, so the gateway rotates onto a
/// fresh token slightly early rather than serving a just-expired one.
const EXPIRY_MARGIN: Duration = Duration::from_secs(5 * 60);
/// A `cached` slot: when the value was stored, how long it stays good, and
/// what was stored. The TTL rides along with the value because `cached`
/// freezes it at fetch time (see there).
type Slot<T> = Mutex<Option<(Instant, Duration, Result<T, KeychainError>)>>;
static CACHE: Slot<Option<OauthCredential>> = Mutex::new(None);

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
    cached(
        &CACHE,
        |res| {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64);
            credential_ttl(res, now_ms)
        },
        read_keychain_uncached,
    )
}

/// How long a completed read may be reused. A credential carrying an
/// `expiresAt` is held until shortly before it; everything else — a backend
/// error, no entry, a blob with no expiry — falls back to [`CACHE_TTL`].
///
/// Takes `now_ms` as a parameter rather than reading the clock itself, the
/// same way `keychain_fallback` takes `is_macos`: it is the only way to
/// assert this arithmetic deterministically instead of against wall time.
fn credential_ttl(
    res: &Result<Option<OauthCredential>, KeychainError>,
    now_ms: i64,
) -> Duration {
    let Ok(Some(cred)) = res else {
        return CACHE_TTL;
    };
    let Some(expires_at_ms) = cred.expires_at_ms else {
        return CACHE_TTL;
    };
    let remaining = Duration::from_millis(expires_at_ms.saturating_sub(now_ms).max(0) as u64);
    // Clamped low as well as high: an already-expired credential falls back to
    // CACHE_TTL rather than 0, so a dead token still can't spin the keychain
    // (and its dialog) on every single request.
    remaining
        .saturating_sub(EXPIRY_MARGIN)
        .clamp(CACHE_TTL, MAX_CACHE_TTL)
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
///
/// The TTL is derived from the fetched value by `ttl_of` and **frozen next to
/// it** rather than recomputed on every hit. Recomputing would be wrong for an
/// expiry-derived TTL: `at.elapsed()` grows as the deadline shrinks, so the
/// comparison would count the passage of time twice and evict at roughly half
/// the intended age.
fn cached<T: Clone>(
    cache: &Slot<T>,
    ttl_of: impl Fn(&Result<T, KeychainError>) -> Duration,
    fetch: impl FnOnce() -> Result<T, KeychainError>,
) -> Result<T, KeychainError> {
    let mut slot = cache.lock().unwrap();
    if let Some((at, ttl, res)) = slot.as_ref()
        && at.elapsed() < *ttl
    {
        return res.clone();
    }
    let res = fetch();
    *slot = Some((Instant::now(), ttl_of(&res), res.clone()));
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


    fn cred(expires_at_ms: Option<i64>) -> Result<Option<OauthCredential>, KeychainError> {
        Ok(Some(OauthCredential {
            access_token: "sk-ant-oat01-test".into(),
            expires_at_ms,
        }))
    }

    /// THE regression test for the keychain-dialog storm.
    ///
    /// Claude Code's access token lives ~8h. Under the old flat 60s TTL a
    /// long-lived gateway re-ran the macOS keychain ACL authorization ~1440
    /// times a day for it, so whenever the ACL grant didn't match (right
    /// after an upgrade, or after Claude Code rewrote the item) the user got
    /// the "enter your keychain password" dialog *every minute, forever*
    /// instead of once. Fails against a flat `CACHE_TTL`.
    #[test]
    fn a_live_credential_is_cached_until_its_own_expiry_not_for_one_minute() {
        let now = 1_800_000_000_000;
        let eight_hours = 8 * 60 * 60 * 1000;
        let ttl = credential_ttl(&cred(Some(now + eight_hours)), now);
        assert!(
            ttl > CACHE_TTL * 10,
            "an 8h-valid credential must not be re-read every {CACHE_TTL:?}: got {ttl:?}"
        );
        // expiry - margin, capped by MAX_CACHE_TTL.
        assert_eq!(ttl, MAX_CACHE_TTL.min(Duration::from_secs(8 * 3600) - EXPIRY_MARGIN));
    }

    #[test]
    fn credential_ttl_re_reads_early_by_the_margin() {
        let now = 1_800_000_000_000;
        let ttl = credential_ttl(&cred(Some(now + 60 * 60 * 1000)), now);
        assert_eq!(ttl, Duration::from_secs(3600) - EXPIRY_MARGIN);
    }

    #[test]
    fn credential_ttl_caps_a_bogus_far_future_expiry() {
        let now = 1_800_000_000_000;
        let year = 365i64 * 24 * 3600 * 1000;
        assert_eq!(credential_ttl(&cred(Some(now + year)), now), MAX_CACHE_TTL);
    }

    /// Unknown expiry, no entry, and backend errors all keep the old floor —
    /// nothing gets held *longer* than before on the strength of a guess.
    #[test]
    fn credential_ttl_falls_back_to_the_floor_without_a_usable_expiry() {
        let now = 1_800_000_000_000;
        assert_eq!(credential_ttl(&cred(None), now), CACHE_TTL);
        assert_eq!(credential_ttl(&Ok(None), now), CACHE_TTL);
        assert_eq!(
            credential_ttl(&Err(KeychainError::Backend("denied".into())), now),
            CACHE_TTL
        );
    }

    /// An already-expired credential must not drop the TTL to zero: that would
    /// put the keychain (and its dialog) back on the per-request path — the
    /// exact failure mode this change exists to remove — for a token the 401
    /// path already handles via `invalidate_cache`.
    #[test]
    fn credential_ttl_of_an_expired_credential_still_holds_the_floor() {
        let now = 1_800_000_000_000;
        assert_eq!(credential_ttl(&cred(Some(now - 1)), now), CACHE_TTL);
        assert_eq!(credential_ttl(&cred(Some(0)), now), CACHE_TTL);
    }

    /// `cached` must freeze the TTL next to the value at fetch time. If it
    /// recomputed on every hit from a shrinking deadline, `at.elapsed()`
    /// growing while the TTL shrank would count elapsed time twice and evict
    /// at ~half the intended age.
    #[test]
    fn cached_derives_and_freezes_the_ttl_from_the_fetched_value() {
        let cache: Slot<Option<String>> = Mutex::new(None);
        // TTL depends on the value: "long" caches, "short" does not.
        let ttl_of = |r: &Result<Option<String>, KeychainError>| match r {
            Ok(Some(v)) if v == "long" => Duration::from_secs(3600),
            _ => Duration::ZERO,
        };
        assert_eq!(
            cached(&cache, ttl_of, || Ok(Some("long".into())))
                .unwrap()
                .as_deref(),
            Some("long")
        );
        assert_eq!(
            cached(&cache, ttl_of, || Ok(Some("ignored".into())))
                .unwrap()
                .as_deref(),
            Some("long"),
            "the stored 1h TTL must serve this hit, not a recomputed one"
        );

        let short: Slot<Option<String>> = Mutex::new(None);
        assert_eq!(
            cached(&short, ttl_of, || Ok(Some("short".into())))
                .unwrap()
                .as_deref(),
            Some("short")
        );
        assert_eq!(
            cached(&short, ttl_of, || Ok(Some("refetched".into())))
                .unwrap()
                .as_deref(),
            Some("refetched"),
            "a zero TTL derived from the value must refetch"
        );
    }

    #[test]
    fn cached_serves_within_ttl_and_refetches_after_expiry() {
        // Local String-payload cache: this test is about TTL mechanics, not
        // credential shape, so it instantiates `Slot` over `String` rather
        // than touching the real `CACHE`.
        let cache: Slot<Option<String>> = Mutex::new(None);
        let ttl = Duration::from_secs(60);
        let r1 = cached(&cache, |_| ttl, || Ok(Some("first".into())));
        let r2 = cached(&cache, |_| ttl, || Ok(Some("second".into())));
        assert_eq!(r1.unwrap().as_deref(), Some("first"));
        assert_eq!(r2.unwrap().as_deref(), Some("first")); // served from cache

        // The TTL is frozen at store time now, so an entry has to be *stored*
        // with a zero TTL for the next call to refetch — passing a zero
        // `ttl_of` at read time no longer evicts a live entry, by design.
        let expired: Slot<Option<String>> = Mutex::new(None);
        cached(&expired, |_| Duration::ZERO, || Ok(Some("third".into()))).unwrap();
        let r3 = cached(&expired, |_| Duration::ZERO, || Ok(Some("fourth".into())));
        assert_eq!(r3.unwrap().as_deref(), Some("fourth")); // expired → refetched
    }

    #[test]
    fn cached_caches_errors_too() {
        // A denied keychain prompt must not re-prompt on every retry.
        // Same rationale as above: String payload, independent of `CACHE`.
        let cache: Slot<Option<String>> = Mutex::new(None);
        let ttl = Duration::from_secs(60);
        let r1 = cached(&cache, |_| ttl, || Err(KeychainError::Backend("denied".into())));
        let r2 = cached(&cache, |_| ttl, || Ok(Some("never-fetched".into())));
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
