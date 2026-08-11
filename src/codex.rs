//! Codex route: forwards `/v1/responses*` to ChatGPT's Codex backend with
//! Codex credentials attached.
//!
//! In public builds only `should_route()` is active — the header and OAuth
//! details are no-op stubs. The real implementation lives in a gitignored
//! file activated via build.rs (`cfg(has_codex_hook)`).

/// True if `path` is a Codex Responses endpoint we route to ChatGPT.
pub fn should_route(path: &str) -> bool {
    path == "/v1/responses"
        || path.starts_with("/v1/responses/")
        || path.starts_with("/v1/responses?")
        || should_translate(path)
}

/// True if `path` is the Chat Completions path that must be translated into
/// a Responses request before it goes upstream. Stage 2 only; stage 1's
/// `/v1/responses*` is forwarded untranslated.
pub fn should_translate(path: &str) -> bool {
    path == "/codex/v1/chat/completions"
        || path.starts_with("/codex/v1/chat/completions/")
        || path.starts_with("/codex/v1/chat/completions?")
}

// ── cfg-gated: real impl or stub ──

// The #[rustfmt::skip] is load-bearing: rustfmt resolves `mod` declarations
// syntactically and ignores cfg, so a clean checkout without the gitignored
// file fails `cargo fmt --check` without it. Same fix as src/disguise.rs.
#[rustfmt::skip]
#[cfg(has_codex_hook)]
mod codex_impl;

#[cfg(not(has_codex_hook))]
mod codex_impl {
    /// Stub: forwards without Codex client headers.
    pub fn apply_codex_headers(
        req: reqwest::RequestBuilder,
        _token: &str,
        _account_id: Option<&str>,
    ) -> reqwest::RequestBuilder {
        req
    }

    /// Stub: no OAuth constants in the public build.
    pub fn refresh_access_token(_refresh_token: &str) -> anyhow::Result<super::RefreshedTokens> {
        anyhow::bail!("codex refresh unavailable in this build")
    }
}

pub use codex_impl::*;

use anyhow::Context;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;
// `tokio::sync::Mutex`, not `std::sync::Mutex` (fix round 2, finding A):
// `refreshed_access_token` holds this lock across a blocking network call
// to restore single-flight. A `tokio`-aware mutex is safe to hold there —
// acquiring it yields the task instead of parking a worker thread, and it
// has no poisoning concept — where a `std::sync::Mutex` held the same way
// was round 1 finding 5's original defect.
use tokio::sync::Mutex;

/// What an OAuth refresh grant returns. `refresh_token` is `Some` when the
/// provider rotates it — ChatGPT does, so it must be persisted or the next
/// refresh fails.
///
/// No `#[derive(Debug)]`: these are raw credentials, and a derived Debug puts
/// them in any `{:?}`, tracing capture, or panic message. Hand-write a
/// redacting impl if one is needed, as `CodexAuth` does.
#[derive(Clone)]
pub struct RefreshedTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
}

/// Credentials as Codex CLI stores them in `~/.codex/auth.json`.
#[derive(Clone)]
pub struct CodexAuth {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub account_id: Option<String>,
}

impl std::fmt::Debug for CodexAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexAuth")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("account_id", &self.account_id)
            .finish()
    }
}

/// `~/.codex/auth.json`.
pub fn default_auth_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().join(".codex/auth.json"))
}

/// Parse the auth blob. `None` for malformed JSON, missing tokens, or
/// API-key mode — all of which mean "no OAuth credential available".
pub fn parse_auth(raw: &str) -> Option<CodexAuth> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    if v.get("auth_mode").and_then(|m| m.as_str()) != Some("chatgpt") {
        return None;
    }
    let tokens = v.get("tokens")?;
    Some(CodexAuth {
        access_token: tokens.get("access_token")?.as_str()?.to_string(),
        refresh_token: tokens
            .get("refresh_token")
            .and_then(|t| t.as_str())
            .map(str::to_string),
        account_id: tokens
            .get("account_id")
            .and_then(|t| t.as_str())
            .map(str::to_string),
    })
}

/// Read and parse the auth file. `None` if absent or unusable — the caller
/// falls through to passthrough.
pub fn read_auth(path: &Path) -> Option<CodexAuth> {
    parse_auth(&std::fs::read_to_string(path).ok()?)
}

/// Most recent refresh, memoised so a burst of 401s triggers one grant.
/// True single-flight (fix round 2, finding A): `refreshed_access_token`
/// holds this lock for its *entire* check-refresh-store sequence, so
/// concurrent cold-cache callers queue behind whichever one gets there
/// first — the losers, once they acquire the lock, re-check the cache and
/// reuse the token the winner just stored instead of redeeming the
/// (rotating) refresh token a second time.
static REFRESHED: OnceLock<Mutex<Option<(Instant, String)>>> = OnceLock::new();

/// A usable access token, refreshing when the stored one was rejected. The
/// grant rotates the refresh token, so the new pair is persisted — discarding
/// it strands both this gateway and Codex CLI on a dead credential.
/// Memoised for `keychain::CACHE_TTL`.
pub async fn refreshed_access_token(path: &Path) -> Option<String> {
    let cell = REFRESHED.get_or_init(|| Mutex::new(None));

    // Fix round 2, finding A: held for the whole check-then-refresh-then-
    // store sequence, not just the read. Round 1, finding 5 released this
    // lock before the network call to stop it from parking every other
    // Codex 401 in this process for a full round trip and from poisoning
    // permanently on panic — both `std::sync::Mutex` problems. Neither
    // applies to `tokio::sync::Mutex`: a task waiting on `.lock().await`
    // yields instead of blocking a worker thread, and there is no
    // poisoning to propagate. Holding it here is what makes concurrent
    // cold-cache 401s single-flight instead of each redeeming the same
    // rotating refresh token.
    let mut slot = cell.lock().await;
    if let Some((at, tok)) = slot.as_ref()
        && at.elapsed() < crate::keychain::CACHE_TTL
    {
        return Some(tok.clone());
    }

    let rt = read_auth(path)?.refresh_token?;
    match refresh_access_token(&rt) {
        Ok(new) => {
            if let Err(e) = persist_rotation(path, &new) {
                // The access token still serves this request, but a lost
                // rotation means the next refresh fails. Warn loudly.
                tracing::warn!(error = %e, "codex token rotation not persisted");
            }
            *slot = Some((Instant::now(), new.access_token.clone()));
            Some(new.access_token)
        }
        Err(e) => {
            tracing::warn!(error = %e, "codex token refresh failed");
            None
        }
    }
}

/// Clear the in-memory refreshed token. Test-only: the cache is process-global
/// and would otherwise leak between integration tests.
pub async fn reset_refresh_cache() {
    if let Some(cell) = REFRESHED.get() {
        *cell.lock().await = None;
    }
}

/// Removes its path on drop unless [`Self::keep`] was called. Guarantees a
/// failed write, sync, or rename never leaves a stray, world-readable-or-not
/// temp file holding a live access/refresh token pair sitting on disk (fix
/// round 1, finding 1). Must only be constructed after the temp file has
/// actually been created by this call — see the call site in
/// `persist_rotation` — otherwise a `create_new` collision (EEXIST) would
/// arm the guard to delete a path this call never created, likely another
/// process's in-flight rotation (fix round 2, finding B).
struct TempFileGuard<'a> {
    path: &'a Path,
    keep: bool,
}

impl<'a> TempFileGuard<'a> {
    fn new(path: &'a Path) -> Self {
        Self { path, keep: false }
    }

    /// Disarm: the file was renamed into place, so there is nothing left
    /// at `path` to remove.
    fn keep(mut self) {
        self.keep = true;
    }
}

impl Drop for TempFileGuard<'_> {
    fn drop(&mut self) {
        if !self.keep {
            // Best-effort: something between construction and drop (write,
            // sync, rename) may already have consumed this path, in which
            // case removal errors and that error is expected and ignored.
            // Safe to assume this path is ours to remove: the guard is
            // only ever constructed after `create_new` has already
            // succeeded (fix round 2, finding B), so it can never point at
            // a different process's in-flight temp file from a
            // `create_new` collision.
            let _ = std::fs::remove_file(self.path);
        }
    }
}

/// Open `tmp` for writing, created fresh with no window where it exists at
/// looser permissions. Fails if `tmp` already exists — see the temp-name
/// scheme in `persist_rotation` for why that's the correct behaviour, not
/// a bug (fix round 1, findings 1 and 2).
#[cfg(unix)]
fn create_temp_exclusive(tmp: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tmp)
}

#[cfg(not(unix))]
fn create_temp_exclusive(tmp: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)
}

/// Replace only the rotated fields (`access_token`, and `refresh_token` when
/// rotated) in `auth.json`, atomically (see [`TempFileGuard`] and
/// `create_temp_exclusive` for how). Parses the document as a
/// `serde_json::Value` without the `preserve_order` feature, so any
/// *unmodelled* key this gateway doesn't know about — top-level or nested
/// under `tokens` — survives by value. This is NOT a verbatim, byte-for-byte
/// round-trip (fix round 3, finding D4): `serde_json::to_vec_pretty`
/// re-sorts object keys alphabetically and applies its own formatting, so
/// key order and whitespace can both change even though every value Codex
/// CLI depends on is preserved.
///
/// `last_refresh` is deliberately one of the fields left alone — it is
/// Codex CLI's own bookkeeping, and updating it would need a date
/// dependency this crate does not have. The cost (fix round 3, finding D5):
/// Codex CLI sees a stale `last_refresh` the next time it runs and performs
/// one extra refresh of its own before trusting the token this gateway just
/// rotated.
fn persist_rotation(path: &Path, new: &RefreshedTokens) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let mut doc: serde_json::Value = serde_json::from_str(&raw)?;
    let tokens = doc
        .get_mut("tokens")
        .context("auth.json has no tokens object")?;
    tokens["access_token"] = serde_json::Value::String(new.access_token.clone());
    if let Some(rt) = &new.refresh_token {
        tokens["refresh_token"] = serde_json::Value::String(rt.clone());
    }
    let bytes = serde_json::to_vec_pretty(&doc)?;

    // Temp file in the SAME directory, so the rename stays on one filesystem —
    // that is what makes it atomic. A concurrent reader sees the old file or
    // the new one, never a torn one.
    //
    // The name includes the pid and a per-process attempt counter (fix
    // round 1, finding 2): two gateway *processes* refreshing at the same
    // moment would otherwise both target the fixed name
    // `.auth.json.mmg-tmp`, and whichever renames second would silently
    // stomp the first mid-write. `create_new` below turns any surviving
    // collision (e.g. pid reuse across processes) into an error instead of
    // one writer clobbering the other.
    let dir = path.parent().context("auth.json has no parent dir")?;
    static ATTEMPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let attempt = ATTEMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(
        ".auth.json.{}.{attempt}.mmg-tmp",
        std::process::id()
    ));

    // Armed only after `create_new` below actually succeeds (fix round 2,
    // finding B): arming it first meant a collision (EEXIST — e.g. pid
    // reuse racing another process's in-flight rotation) would delete that
    // OTHER process's temp file on drop, since `create_new` failing here
    // means this call never created anything at `tmp` to clean up.
    // 0600 from creation (fix round 1, finding 1): the old `write` then
    // `set_permissions` left a window where the temp file held a live
    // token pair at the default (world-readable) mode, and a crash in that
    // window left it that way.
    let mut file = create_temp_exclusive(&tmp)
        .with_context(|| format!("create temp file for {}", path.display()))?;
    let guard = TempFileGuard::new(&tmp);
    use std::io::Write;
    file.write_all(&bytes)
        .with_context(|| format!("write temp file for {}", path.display()))?;
    // Durability (fix round 1, finding 3): without this, the rename below
    // can hit disk before the data does — a crash between the two leaves
    // an empty or garbage auth.json.
    file.sync_all()
        .with_context(|| format!("sync temp file for {}", path.display()))?;
    drop(file);

    std::fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))?;
    guard.keep();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_responses_paths_only() {
        assert!(should_route("/v1/responses"));
        assert!(should_route("/v1/responses/abc"));
        assert!(should_route("/v1/responses?stream=true"));
        assert!(!should_route("/v1/messages"));
        assert!(!should_route("/v1/chat/completions"));
        assert!(!should_route("/v1/responsesX"));
    }

    #[test]
    fn should_translate_matches_codex_chat_path() {
        assert!(should_translate("/codex/v1/chat/completions"));
        assert!(should_translate("/codex/v1/chat/completions?stream=true"));
        assert!(should_translate("/codex/v1/chat/completions/"));
        // The plain OpenAI path must never translate.
        assert!(!should_translate("/v1/chat/completions"));
        // Stage 1's passthrough must never translate.
        assert!(!should_translate("/v1/responses"));
        assert!(!should_translate("/codex/v1/chat/completionsX"));
    }

    #[test]
    fn translated_path_is_routed_to_codex() {
        assert!(should_route("/codex/v1/chat/completions"));
    }

    #[test]
    fn parses_chatgpt_mode_auth() {
        let raw = r#"{
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "fake-id-token",
                "access_token": "fake-access-token",
                "refresh_token": "fake-refresh-token",
                "account_id": "acct-fake"
            },
            "last_refresh": "2026-07-10T00:20:57.310171Z"
        }"#;
        let a = parse_auth(raw).expect("should parse");
        assert_eq!(a.access_token, "fake-access-token");
        assert_eq!(a.refresh_token.as_deref(), Some("fake-refresh-token"));
        assert_eq!(a.account_id.as_deref(), Some("acct-fake"));
    }

    #[test]
    fn rejects_api_key_mode() {
        // Stage 1 handles OAuth only; API-key mode resolves to None so the
        // caller falls through to passthrough rather than sending a bad token.
        let raw = r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-fake","tokens":null}"#;
        assert!(parse_auth(raw).is_none());
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(parse_auth("{not json").is_none());
        assert!(parse_auth("{}").is_none());
    }

    #[test]
    fn debug_redacts_tokens() {
        let auth = CodexAuth {
            access_token: "fake-access-token".to_string(),
            refresh_token: Some("fake-refresh-token".to_string()),
            account_id: Some("acct-fake".to_string()),
        };
        let debug_str = format!("{:?}", auth);

        // Verify tokens are redacted
        assert!(!debug_str.contains("fake-access-token"));
        assert!(!debug_str.contains("fake-refresh-token"));
        assert!(debug_str.contains("<redacted>"));

        // Verify account_id is NOT redacted
        assert!(debug_str.contains("acct-fake"));
    }

    #[test]
    fn persist_rotation_preserves_unmodelled_fields() {
        let dir = std::env::temp_dir().join("mmg-codex-persist");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("auth.json");
        std::fs::write(
            &p,
            r#"{"auth_mode":"chatgpt","OPENAI_API_KEY":null,"some_future_key":42,
                "tokens":{"id_token":"fake-id","access_token":"old-a","refresh_token":"old-r","account_id":"acct"}}"#,
        )
        .unwrap();

        persist_rotation(
            &p,
            &RefreshedTokens {
                access_token: "new-a".into(),
                refresh_token: Some("new-r".into()),
            },
        )
        .unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["tokens"]["access_token"], "new-a");
        assert_eq!(v["tokens"]["refresh_token"], "new-r");
        // Untouched fields survive — Codex CLI depends on them.
        assert_eq!(v["tokens"]["id_token"], "fake-id");
        assert_eq!(v["tokens"]["account_id"], "acct");
        assert_eq!(v["some_future_key"], 42);
        assert_eq!(v["auth_mode"], "chatgpt");
        std::fs::remove_file(&p).ok();
    }

    /// Fix round 1, findings 1-3: the rewritten atomic write (create with
    /// 0600 from birth, unique per-attempt temp name, `sync_all`, rename)
    /// must still leave the directory exactly as clean as the original
    /// `write`-then-`chmod` version did — no stray `.auth.json.*.mmg-tmp`
    /// left behind by a successful run.
    #[test]
    fn persist_rotation_leaves_no_temp_file_behind() {
        let dir = std::env::temp_dir().join("mmg-codex-persist-notemp");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("auth.json");
        std::fs::write(
            &p,
            r#"{"tokens":{"access_token":"old-a","refresh_token":"old-r"}}"#,
        )
        .unwrap();

        persist_rotation(
            &p,
            &RefreshedTokens {
                access_token: "new-a".into(),
                refresh_token: Some("new-r".into()),
            },
        )
        .unwrap();

        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            entries,
            vec![std::ffi::OsString::from("auth.json")],
            "no leftover temp file after a successful persist"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Fix round 1, finding 1: the temp file — and so, after rename, the
    /// final file — must be owner-only from the moment it exists, never
    /// created at the default (typically world-readable) mode and tightened
    /// after the fact.
    #[cfg(unix)]
    #[test]
    fn persist_rotation_creates_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join("mmg-codex-persist-perms");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("auth.json");
        std::fs::write(
            &p,
            r#"{"tokens":{"access_token":"old-a","refresh_token":"old-r"}}"#,
        )
        .unwrap();
        // Start deliberately world-readable, so a pass would be a false
        // positive if the code merely left the original mode alone instead
        // of actively setting 0600.
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();

        persist_rotation(
            &p,
            &RefreshedTokens {
                access_token: "new-a".into(),
                refresh_token: None,
            },
        )
        .unwrap();

        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "rotated file must be owner-read-write only");
        std::fs::remove_dir_all(&dir).ok();
    }
}
