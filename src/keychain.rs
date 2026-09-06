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

/// Warn when a request waited longer than this for the cache lock. Well above
/// any healthy value — a cache hit takes microseconds — so this stays silent
/// unless a request genuinely queued behind someone else's keychain read.
const LOCK_WAIT_WARN: Duration = Duration::from_secs(1);
/// How long a cached read stays authoritative, and how long it may still be
/// handed to callers who arrive while someone else is refreshing it.
///
/// Two deadlines rather than one because they answer different questions.
/// `refresh_after` is "should someone go get a new value?"; `usable_for` is
/// "is the value we already hold still *correct*?". For a Claude credential
/// they differ by exactly `EXPIRY_MARGIN` — the margin exists so the token
/// outlives its own refresh deadline, and that gap is what makes serving a
/// stale-but-unexpired credential sound rather than a shortcut.
///
/// Named fields, not a tuple: swapping two `Duration`s here would mean serving
/// a dead token, and nothing about the types would catch it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Freshness {
    refresh_after: Duration,
    /// Never less than `refresh_after`.
    usable_for: Duration,
}

impl Freshness {
    /// No grace window: the value stops being served the moment it goes stale.
    /// Correct whenever the caller cannot vouch for the value past its TTL —
    /// a failed read, or a credential whose real expiry is unknown.
    fn fixed(d: Duration) -> Self {
        Freshness {
            refresh_after: d,
            usable_for: d,
        }
    }
}

/// A `cached` slot: when the value was stored, its two deadlines, and the
/// value. The deadlines ride along with the value because `cached` freezes
/// them at fetch time (see there).
type Slot<T> = Mutex<Option<(Instant, Freshness, Result<T, KeychainError>)>>;
static CACHE: Slot<Option<OauthCredential>> = Mutex::new(None);

/// Single-flight gate. Separate from `CACHE` on purpose: `CACHE`'s lock is now
/// only ever held for a clone, never across the keychain read, so a slow read
/// cannot stall readers. This is what serialises the *refreshers*, so a
/// rotation still triggers one keychain dialog rather than one per request.
static REFRESH: Mutex<()> = Mutex::new(());

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
        &REFRESH,
        |res| {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64);
            credential_freshness(res, now_ms)
        },
        read_keychain_uncached,
    )
}

/// How long a completed read may be reused, and how long it may still be
/// served during someone else's refresh.
///
/// A credential carrying an `expiresAt` is refreshed shortly before it and
/// remains servable right up to it; everything else — a backend error, no
/// entry, a blob with no expiry — falls back to [`CACHE_TTL`] with **no grace
/// window**. That asymmetry is deliberate: a grace window is only sound when
/// we know the value is still valid, and in those three cases we do not.
///
/// Takes `now_ms` as a parameter rather than reading the clock itself, the
/// same way `keychain_fallback` takes `is_macos`: it is the only way to
/// assert this arithmetic deterministically instead of against wall time.
fn credential_freshness(
    res: &Result<Option<OauthCredential>, KeychainError>,
    now_ms: i64,
) -> Freshness {
    let Ok(Some(cred)) = res else {
        return Freshness::fixed(CACHE_TTL);
    };
    let Some(expires_at_ms) = cred.expires_at_ms else {
        return Freshness::fixed(CACHE_TTL);
    };
    let remaining = Duration::from_millis(expires_at_ms.saturating_sub(now_ms).max(0) as u64);
    // Clamped low as well as high: an already-expired credential falls back to
    // CACHE_TTL rather than 0, so a dead token still can't spin the keychain
    // (and its dialog) on every single request.
    let refresh_after = remaining
        .saturating_sub(EXPIRY_MARGIN)
        .clamp(CACHE_TTL, MAX_CACHE_TTL);
    // Servable until the real expiry — never past it. `.max(refresh_after)`
    // only matters for an already-expired credential, where `remaining` is
    // below the floor: there the two collapse and no grace is granted.
    let usable_for = remaining.min(MAX_CACHE_TTL).max(refresh_after);
    Freshness {
        refresh_after,
        usable_for,
    }
}

/// Wraps the real read to time it. Only reached on a cache miss — this is
/// `cached`'s `fetch` — so at the expiry-derived TTL it fires roughly three
/// times a day, not per request.
///
/// This is the only operation in the gateway that can block *every* request at
/// once: `cached` deliberately holds its lock across this call so concurrent
/// requests trigger one keychain dialog instead of N. When the read is slow the
/// whole proxy stalls behind it — and until this line existed that stall was
/// invisible from the inside. A rotation-time hang was only ever observed
/// externally, by a health poll timing out, with nothing in 52k lines of log to
/// corroborate it.
///
/// Logs a *kind*, never the credential: `outcome` is a `&'static str`, so it is
/// structurally incapable of carrying the token, and `KeychainError`'s message
/// is a backend string that never contains one either.
fn read_keychain_uncached() -> Result<Option<OauthCredential>, KeychainError> {
    let started = Instant::now();
    let res = read_keychain_now();
    let outcome = match &res {
        Ok(Some(_)) => "found",
        Ok(None) => "no-entry",
        Err(_) => "error",
    };
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        outcome,
        "uncached keychain read"
    );
    res
}

fn read_keychain_now() -> Result<Option<OauthCredential>, KeychainError> {
    let user = whoami::username();
    let entry = keyring::Entry::new(SERVICE, &user)
        .map_err(|e| KeychainError::Backend(format!("entry::new({SERVICE}, {user}): {e}")))?;
    match entry.get_password() {
        Ok(raw) => parse_oauth_blob(&raw),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(KeychainError::Backend(format!("get_password: {e}"))),
    }
}

/// Stale-while-revalidate cache with single-flight refresh.
///
/// The `CACHE` lock is held only long enough to clone a snapshot — never
/// across `fetch` — and `refresh` is what serialises refreshers. That split is
/// the whole point. Holding one lock across the read did guarantee a single
/// keychain dialog, but it also made every concurrent request wait for the
/// read: at a real token rotation that was measured at
/// `elapsed_ms=72980` with requests queued `waited_ms=64103` and `53642`
/// behind it, against an 18ms baseline.
///
/// So: one caller refreshes, and everyone else keeps being served the value
/// already in hand for as long as it is still *correct* (`usable_for`), not
/// merely fresh (`refresh_after`). For credentials those differ by
/// `EXPIRY_MARGIN`, which is exactly the window the margin was created to
/// provide — a 73s refresh fits inside 5 minutes with room to spare. Callers
/// with nothing usable to fall back on still wait, because a wrong answer is
/// worse than a slow one.
///
/// The deadlines are derived from the fetched value by `freshness_of` and
/// **frozen next to it** rather than recomputed on every hit. Recomputing
/// would be wrong for an expiry-derived deadline: `at.elapsed()` grows as the
/// deadline shrinks, so the comparison would count elapsed time twice and
/// evict at roughly half the intended age.
fn cached<T: Clone>(
    cache: &Slot<T>,
    refresh: &Mutex<()>,
    freshness_of: impl Fn(&Result<T, KeychainError>) -> Freshness,
    fetch: impl FnOnce() -> Result<T, KeychainError>,
) -> Result<T, KeychainError> {
    let snapshot = cache.lock().unwrap().clone();
    if let Some((at, f, res)) = &snapshot
        && at.elapsed() < f.refresh_after
    {
        return res.clone();
    }

    let _guard = match refresh.try_lock() {
        Ok(g) => g,
        Err(_) => {
            // Someone is already refreshing. Serve what we have if it is still
            // valid — this is the path that used to block for a minute.
            if let Some((at, f, res)) = &snapshot
                && at.elapsed() < f.usable_for
            {
                return res.clone();
            }
            // Nothing servable: no value at all, or one past its real expiry.
            // Wait for the refresher rather than answer wrongly.
            let started = Instant::now();
            let g = refresh.lock().unwrap();
            let waited = started.elapsed();
            if waited > LOCK_WAIT_WARN {
                tracing::warn!(
                    waited_ms = waited.as_millis() as u64,
                    "request blocked waiting for a credential refresh with no \
                     usable cached value to fall back on"
                );
            }
            // The refresher stored a value while we waited; prefer it over
            // running a second, redundant read.
            let refreshed = cache.lock().unwrap().clone();
            if let Some((at, f, res)) = &refreshed
                && at.elapsed() < f.refresh_after
            {
                return res.clone();
            }
            g
        }
    };

    let res = fetch();
    *cache.lock().unwrap() = Some((Instant::now(), freshness_of(&res), res.clone()));
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

    /// A refresh gate for tests that never contend on it. Tests that *do*
    /// exercise contention declare their own, so one test cannot serialise
    /// against another.
    static R: Mutex<()> = Mutex::new(());

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
        let ttl = credential_freshness(&cred(Some(now + eight_hours)), now).refresh_after;
        assert!(
            ttl > CACHE_TTL * 10,
            "an 8h-valid credential must not be re-read every {CACHE_TTL:?}: got {ttl:?}"
        );
        // expiry - margin, capped by MAX_CACHE_TTL.
        assert_eq!(
            ttl,
            MAX_CACHE_TTL.min(Duration::from_secs(8 * 3600) - EXPIRY_MARGIN)
        );
    }

    #[test]
    fn credential_freshness_re_reads_early_by_the_margin() {
        let now = 1_800_000_000_000;
        let ttl = credential_freshness(&cred(Some(now + 60 * 60 * 1000)), now).refresh_after;
        assert_eq!(ttl, Duration::from_secs(3600) - EXPIRY_MARGIN);
    }

    #[test]
    fn credential_freshness_caps_a_bogus_far_future_expiry() {
        let now = 1_800_000_000_000;
        let year = 365i64 * 24 * 3600 * 1000;
        assert_eq!(
            credential_freshness(&cred(Some(now + year)), now).refresh_after,
            MAX_CACHE_TTL
        );
    }

    /// Unknown expiry, no entry, and backend errors all keep the old floor —
    /// nothing gets held *longer* than before on the strength of a guess.
    #[test]
    fn credential_freshness_falls_back_to_the_floor_without_a_usable_expiry() {
        let now = 1_800_000_000_000;
        assert_eq!(
            credential_freshness(&cred(None), now),
            Freshness::fixed(CACHE_TTL)
        );
        assert_eq!(
            credential_freshness(&Ok(None), now),
            Freshness::fixed(CACHE_TTL)
        );
        assert_eq!(
            credential_freshness(&Err(KeychainError::Backend("denied".into())), now),
            Freshness::fixed(CACHE_TTL)
        );
    }

    /// An already-expired credential must not drop the TTL to zero: that would
    /// put the keychain (and its dialog) back on the per-request path — the
    /// exact failure mode this change exists to remove — for a token the 401
    /// path already handles via `invalidate_cache`.
    #[test]
    fn credential_freshness_of_an_expired_credential_still_holds_the_floor() {
        let now = 1_800_000_000_000;
        assert_eq!(
            credential_freshness(&cred(Some(now - 1)), now),
            Freshness::fixed(CACHE_TTL)
        );
        assert_eq!(
            credential_freshness(&cred(Some(0)), now),
            Freshness::fixed(CACHE_TTL)
        );
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
            Ok(Some(v)) if v == "long" => Freshness::fixed(Duration::from_secs(3600)),
            _ => Freshness::fixed(Duration::ZERO),
        };
        assert_eq!(
            cached(&cache, &R, ttl_of, || Ok(Some("long".into())))
                .unwrap()
                .as_deref(),
            Some("long")
        );
        assert_eq!(
            cached(&cache, &R, ttl_of, || Ok(Some("ignored".into())))
                .unwrap()
                .as_deref(),
            Some("long"),
            "the stored 1h TTL must serve this hit, not a recomputed one"
        );

        let short: Slot<Option<String>> = Mutex::new(None);
        assert_eq!(
            cached(&short, &R, ttl_of, || Ok(Some("short".into())))
                .unwrap()
                .as_deref(),
            Some("short")
        );
        assert_eq!(
            cached(&short, &R, ttl_of, || Ok(Some("refetched".into())))
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
        let r1 = cached(
            &cache,
            &R,
            |_| Freshness::fixed(ttl),
            || Ok(Some("first".into())),
        );
        let r2 = cached(
            &cache,
            &R,
            |_| Freshness::fixed(ttl),
            || Ok(Some("second".into())),
        );
        assert_eq!(r1.unwrap().as_deref(), Some("first"));
        assert_eq!(r2.unwrap().as_deref(), Some("first")); // served from cache

        // The TTL is frozen at store time now, so an entry has to be *stored*
        // with a zero TTL for the next call to refetch — passing a zero
        // `ttl_of` at read time no longer evicts a live entry, by design.
        let expired: Slot<Option<String>> = Mutex::new(None);
        cached(
            &expired,
            &R,
            |_| Freshness::fixed(Duration::ZERO),
            || Ok(Some("third".into())),
        )
        .unwrap();
        let r3 = cached(
            &expired,
            &R,
            |_| Freshness::fixed(Duration::ZERO),
            || Ok(Some("fourth".into())),
        );
        assert_eq!(r3.unwrap().as_deref(), Some("fourth")); // expired → refetched
    }

    #[test]
    fn cached_caches_errors_too() {
        // A denied keychain prompt must not re-prompt on every retry.
        // Same rationale as above: String payload, independent of `CACHE`.
        let cache: Slot<Option<String>> = Mutex::new(None);
        let ttl = Duration::from_secs(60);
        let r1 = cached(
            &cache,
            &R,
            |_| Freshness::fixed(ttl),
            || Err(KeychainError::Backend("denied".into())),
        );
        let r2 = cached(
            &cache,
            &R,
            |_| Freshness::fixed(ttl),
            || Ok(Some("never-fetched".into())),
        );
        assert!(matches!(r1, Err(KeychainError::Backend(_))));
        assert!(matches!(r2, Err(KeychainError::Backend(_))));
    }

    /// THE regression test for this change.
    ///
    /// At a real rotation the keychain read took 72,980ms and requests queued
    /// 64,103ms and 53,642ms behind it, because one lock was held across the
    /// read. A caller arriving mid-refresh must now be served the value
    /// already in hand — it is stale but not yet expired — instead of waiting.
    ///
    /// Deterministic by handshake, not by timing: the fetch signals that it
    /// has started and then blocks until released, so the second call is
    /// guaranteed to land while the refresh is genuinely in flight.
    ///
    /// The `panic!` in the second call's fetch is the single-flight half of
    /// the assertion: serving stale must not also mean running a second read.
    #[test]
    fn a_stale_but_unexpired_value_is_served_while_someone_else_refreshes() {
        let cache: Slot<Option<String>> = Mutex::new(None);
        let refresh: Mutex<()> = Mutex::new(());
        // Already stale, still valid for a minute — the rotation shape.
        let window = Freshness {
            refresh_after: Duration::ZERO,
            usable_for: Duration::from_secs(60),
        };
        *cache.lock().unwrap() = Some((Instant::now(), window, Ok(Some("old".to_string()))));

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();

        let (c, r) = (&cache, &refresh);
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let _ = cached(
                    c,
                    r,
                    |_| window,
                    || {
                        started_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        Ok(Some("new".to_string()))
                    },
                );
            });

            started_rx.recv().unwrap(); // the refresh is now genuinely in flight

            let t = Instant::now();
            let got = cached(
                c,
                r,
                |_| window,
                || panic!("must not start a second keychain read while one is in flight"),
            )
            .unwrap();
            let took = t.elapsed();

            assert_eq!(
                got.as_deref(),
                Some("old"),
                "must serve the stale-but-unexpired value"
            );
            assert!(
                took < Duration::from_secs(1),
                "must not block behind the in-flight refresh; took {took:?}"
            );

            release_tx.send(()).unwrap();
        });
    }

    /// The other half of the policy: a caller with nothing valid to fall back
    /// on waits, because a wrong answer is worse than a slow one. It must then
    /// reuse what the refresher stored rather than run a redundant read.
    #[test]
    fn a_caller_with_nothing_usable_waits_instead_of_answering_wrongly() {
        let cache: Slot<Option<String>> = Mutex::new(None); // nothing to serve
        let refresh: Mutex<()> = Mutex::new(());
        let window = Freshness::fixed(Duration::from_secs(60));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();

        let (c, r) = (&cache, &refresh);
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let _ = cached(
                    c,
                    r,
                    |_| window,
                    || {
                        started_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        Ok(Some("fetched-once".to_string()))
                    },
                );
            });
            started_rx.recv().unwrap();

            scope.spawn(move || {
                let got = cached(
                    c,
                    r,
                    |_| window,
                    || panic!("must reuse the refresher's value, not read again"),
                )
                .unwrap();
                assert_eq!(got.as_deref(), Some("fetched-once"));
            });

            // Nudges the waiter into the blocking path. Not load-bearing: if it
            // loses the race the refresher has already stored, and the waiter's
            // first snapshot check returns the same value — the assertion holds
            // either way, so this cannot flake.
            std::thread::sleep(Duration::from_millis(50));
            release_tx.send(()).unwrap();
        });
    }

    /// The grace window is exactly `EXPIRY_MARGIN`, and that identity is what
    /// makes serving a stale credential sound rather than a shortcut: the
    /// margin was always the amount by which the token outlives its own
    /// refresh deadline. A measured refresh took 73s; the window is 300s.
    #[test]
    fn a_live_credential_gets_exactly_the_margin_as_its_grace_window() {
        let now = 1_800_000_000_000;
        let f = credential_freshness(&cred(Some(now + 60 * 60 * 1000)), now);
        assert_eq!(f.refresh_after, Duration::from_secs(3600) - EXPIRY_MARGIN);
        assert_eq!(f.usable_for, Duration::from_secs(3600));
        assert_eq!(
            f.usable_for - f.refresh_after,
            EXPIRY_MARGIN,
            "grace must be exactly the margin — more would serve an expired token"
        );
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
