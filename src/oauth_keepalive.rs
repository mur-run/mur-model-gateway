//! Background refresh of the Claude Code OAuth credential.
//!
//! The gateway never redeems the refresh token itself — only Claude Code
//! does, and only when it makes a request. With no Claude Code session for
//! eight hours (overnight, say) the stored access token ages out and every
//! agent request 401s until someone next opens `claude`. This task closes
//! that gap: it watches the stored expiry and, once it has (nearly) passed,
//! runs one minimal `claude -p` so Claude Code rewrites the credential.
//!
//! On by default; `MUR_MODEL_GATEWAY_OAUTH_KEEPALIVE=0` turns it off. Each
//! refresh spends one small model request (about three haiku calls a day) —
//! cheap next to every agent request 401ing each morning. Bounded: at most
//! one spawn per [`TICK`], and a spawn that did not move the expiry backs
//! off for [`FAILURE_BACKOFF`].

use std::time::Duration;

use crate::TokenSource;

pub const ENV_VAR: &str = "MUR_MODEL_GATEWAY_OAUTH_KEEPALIVE";
/// How often the stored expiry is checked — also the worst-case outage.
const TICK: Duration = Duration::from_secs(5 * 60);
/// Refresh this long before the stored expiry. Kept inside Claude Code's own
/// pre-expiry refresh window so the spawned request actually refreshes.
const LEAD: Duration = Duration::from_secs(2 * 60);
/// A spawn that left the expiry unchanged waits this long before the next.
const FAILURE_BACKOFF: Duration = Duration::from_secs(30 * 60);
const SPAWN_TIMEOUT: Duration = Duration::from_secs(120);
/// The cheapest request that makes Claude Code check its credential.
const CLAUDE_ARGS: [&str; 4] = ["-p", "ok", "--model", "haiku"];

/// On unless the variable is explicitly `0` (or `false`/`off`/`no`).
pub fn enabled() -> bool {
    enabled_from(std::env::var(ENV_VAR).ok().as_deref())
}

/// Whether the install-time env explicitly opted out — the only value worth
/// persisting into the service definition, since on is the default.
pub fn opted_out() -> bool {
    !enabled()
}

fn enabled_from(v: Option<&str>) -> bool {
    !matches!(
        v.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("0" | "false" | "off" | "no")
    )
}

/// Whether a credential expiring at `expires_at_ms` should be refreshed now.
/// An unknown expiry is never due: there is nothing to schedule against.
pub fn refresh_due(expires_at_ms: Option<i64>, now_ms: i64) -> bool {
    expires_at_ms.is_some_and(|e| e - LEAD.as_millis() as i64 <= now_ms)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn stored_expiry(source: &TokenSource) -> Option<i64> {
    match source.resolve_credential() {
        Ok(Some(c)) => c.expires_at_ms,
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(error = %e, "oauth keepalive: credential read failed");
            None
        }
    }
}

async fn run_claude(bin: &std::path::Path) -> Result<std::process::ExitStatus, String> {
    let child = tokio::process::Command::new(bin)
        .args(CLAUDE_ARGS)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .status();
    match tokio::time::timeout(SPAWN_TIMEOUT, child).await {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!("timed out after {}s", SPAWN_TIMEOUT.as_secs())),
    }
}

/// Spawn the keepalive loop when opted in and the Anthropic credential comes
/// from Claude Code's own store. Anything else has no expiry to act on.
pub fn spawn_if_enabled(source: TokenSource) {
    if !enabled() {
        return;
    }
    if !matches!(
        source,
        TokenSource::Keychain | TokenSource::CredentialsFile(_)
    ) {
        tracing::info!(
            ?source,
            "oauth keepalive: token source has no stored expiry; not started"
        );
        return;
    }
    let Some(bin) = crate::which_claude() else {
        tracing::warn!("oauth keepalive: `claude` not found on PATH or install dirs; not started");
        return;
    };
    tracing::info!(claude = %bin.display(), "oauth keepalive: started");
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(TICK);
        let mut backoff_until_ms = 0;
        loop {
            interval.tick().await;
            let before = stored_expiry(&source);
            if now_ms() < backoff_until_ms || !refresh_due(before, now_ms()) {
                continue;
            }
            tracing::info!(expires_at_ms = ?before, "oauth keepalive: credential due; running claude");
            let result = run_claude(&bin).await;
            crate::keychain::invalidate_cache();
            let after = stored_expiry(&source);
            if after != before && !refresh_due(after, now_ms()) {
                tracing::info!(expires_at_ms = ?after, "oauth keepalive: credential refreshed");
            } else {
                backoff_until_ms = now_ms() + FAILURE_BACKOFF.as_millis() as i64;
                tracing::warn!(
                    ?result,
                    expires_at_ms = ?after,
                    backoff_secs = FAILURE_BACKOFF.as_secs(),
                    "oauth keepalive: claude ran but the credential did not change"
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_due_only_inside_the_lead_window() {
        let now = 1_000_000_000;
        let lead = LEAD.as_millis() as i64;
        assert!(!refresh_due(None, now), "unknown expiry is never due");
        assert!(!refresh_due(Some(now + lead + 1), now));
        assert!(refresh_due(Some(now + lead), now));
        assert!(refresh_due(Some(now - 1), now), "already expired is due");
    }

    #[test]
    fn on_by_default_and_only_an_explicit_off_disables() {
        assert!(enabled_from(None), "unset means on");
        for on in ["1", "", "yes", "true"] {
            assert!(enabled_from(Some(on)), "{on:?} means on");
        }
        for off in ["0", "false", "OFF", " no "] {
            assert!(!enabled_from(Some(off)), "{off:?} means off");
        }
    }
}
