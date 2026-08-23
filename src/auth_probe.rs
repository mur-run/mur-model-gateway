//! Delegated refresh: ask the credential's owner CLI to refresh it, then
//! check whether it did. The gateway never reads or redeems the refresh
//! token itself — see the spec's Rejected section for why.

use crate::AuthProbe;
use crate::keychain;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Long enough for a 325MB binary to cold-start and answer, short enough that
/// a wedged probe cannot hold a request open. `claude auth status` answers in
/// well under a second warm.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// After a probe that changed nothing, wait this long before spawning again.
/// Well above `keychain::CACHE_TTL` (60s) — without that gap an unrepairable
/// credential would spawn a process on every cache miss, indefinitely.
const PROBE_COOLDOWN: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The owner refreshed the credential; the caller should re-read and retry.
    Refreshed,
    /// The probe ran (or could not run) and the credential did not move.
    NoChange,
    /// No probe was attempted: disabled, or inside the cooldown.
    Skipped,
}

/// Serialises probes: held across the child's execution so concurrent 401s
/// queue behind one probe rather than each spawning their own — the same
/// single-flight shape as `codex::refreshed_access_token`. `tokio::sync::Mutex`
/// is deliberate: a waiter yields instead of parking a worker thread, and there
/// is no poisoning to propagate.
static PROBE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// When the last fruitless probe ran. Deliberately a `std::sync::Mutex`, NOT
/// part of `PROBE_LOCK`: `reset_probe_state` is a sync fn called from
/// `#[tokio::test]` bodies, and `blocking_lock()` on a tokio mutex panics
/// inside a runtime. Never hold this across an await — take it, read or write,
/// drop it.
static COOLDOWN: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

fn probe_lock() -> &'static Mutex<()> {
    PROBE_LOCK.get_or_init(|| Mutex::new(()))
}

fn cooldown_active() -> bool {
    COOLDOWN
        .lock()
        .unwrap()
        .is_some_and(|at| at.elapsed() < PROBE_COOLDOWN)
}

/// Ask the owner CLI to refresh, then report whether the stored expiry moved.
///
/// `before_ms` is the `expiresAt` observed before the probe; the credential is
/// re-read afterwards and the two compared. A blob without an expiry compares
/// as unchanged, which is the safe direction: it costs one wasted retry, not a
/// spawn loop.
pub async fn refresh_via_owner(probe: &AuthProbe, before_ms: Option<i64>) -> ProbeOutcome {
    refresh_via_owner_with(probe, before_ms, || {
        // Bypass the 60s memoise: the child just rewrote the store, and a
        // cached read would return the token we already know is dead.
        keychain::invalidate_cache();
        keychain::read_claude_code_credential()
            .ok()
            .flatten()
            .and_then(|c| c.expires_at_ms)
    })
    .await
}

/// The body, with the post-probe credential read injected. Tests drive every
/// outcome through this; production goes through the wrapper above, which
/// supplies the real keychain read. Without this seam the `Refreshed` outcome
/// could not be tested at all — it would depend on the machine's live
/// keychain.
pub(crate) async fn refresh_via_owner_with(
    probe: &AuthProbe,
    before_ms: Option<i64>,
    read_after: impl FnOnce() -> Option<i64>,
) -> ProbeOutcome {
    let AuthProbe::Command(bin) = probe else {
        return ProbeOutcome::Skipped;
    };

    // Single-flight: held across the child's whole execution so a concurrent
    // 401 queues behind this probe instead of spawning its own. Acquired
    // before the cooldown check so a queued waiter observes the cooldown
    // state this probe leaves behind, rather than a stale pre-lock snapshot.
    let _guard = probe_lock().lock().await;

    if cooldown_active() {
        return ProbeOutcome::Skipped;
    }

    // stdin is closed, not inherited: a probe that decides to prompt must fail
    // fast rather than wait forever on a daemon's stdin.
    let child = tokio::process::Command::new(bin)
        .args(["auth", "status"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn();

    match child {
        Ok(mut c) => {
            if tokio::time::timeout(PROBE_TIMEOUT, c.wait()).await.is_err() {
                tracing::warn!("auth probe timed out after {PROBE_TIMEOUT:?}");
                let _ = c.kill().await;
            }
        }
        Err(e) => {
            // Missing binary (transplanted credential) or no permission — a
            // supported state, not a bug.
            tracing::warn!(error = %e, bin = %bin.display(), "auth probe could not start");
            *COOLDOWN.lock().unwrap() = Some(Instant::now());
            return ProbeOutcome::NoChange;
        }
    }

    let after_ms = read_after();

    match (before_ms, after_ms) {
        (Some(before), Some(after)) if after > before => {
            *COOLDOWN.lock().unwrap() = None;
            ProbeOutcome::Refreshed
        }
        _ => {
            tracing::warn!(
                "auth probe did not refresh the credential; backing off for {PROBE_COOLDOWN:?}"
            );
            *COOLDOWN.lock().unwrap() = Some(Instant::now());
            ProbeOutcome::NoChange
        }
    }
}

/// Clear the cooldown. Test-only: the state is process-global and would
/// otherwise leak between tests in the same binary. Sync, and touches only
/// `COOLDOWN`, so it is safe to call from inside a `#[tokio::test]`.
pub fn reset_probe_state() {
    *COOLDOWN.lock().unwrap() = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises this module's tests. They all exercise the same
    /// process-global `PROBE_LOCK`/`COOLDOWN` statics, and `cargo test` runs
    /// `#[tokio::test]` fns concurrently by default — without this, one
    /// test's cooldown leaks into a sibling running at the same time (seen
    /// as spurious `Skipped` outcomes). Held for the whole test body, not
    /// just around `reset_probe_state`, since the probe call itself reads
    /// and writes the same shared state. `tokio::sync::Mutex`, not
    /// `std::sync::Mutex`: the guard is held across `.await` points, and a
    /// std guard there is exactly what clippy's `await_holding_lock` exists
    /// to catch. Same `OnceLock`-wrapped shape as `probe_lock()`, for the
    /// same reason: `Mutex::new` isn't `const` for this type.
    static TEST_SERIAL: OnceLock<Mutex<()>> = OnceLock::new();

    fn test_serial() -> &'static Mutex<()> {
        TEST_SERIAL.get_or_init(|| Mutex::new(()))
    }

    #[tokio::test]
    async fn disabled_probe_is_skipped() {
        let _serial = test_serial().lock().await;
        reset_probe_state();
        assert_eq!(
            refresh_via_owner_with(&AuthProbe::Disabled, Some(1), || Some(2)).await,
            ProbeOutcome::Skipped
        );
    }

    #[tokio::test]
    async fn a_probe_that_does_not_move_the_expiry_reports_no_change() {
        let _serial = test_serial().lock().await;
        // /usr/bin/true exits 0 and touches nothing — the shape of a probe
        // that runs fine but repairs nothing.
        reset_probe_state();
        let probe = AuthProbe::Command("/usr/bin/true".into());
        assert_eq!(
            refresh_via_owner_with(&probe, Some(1_000), || Some(1_000)).await,
            ProbeOutcome::NoChange
        );
    }

    #[tokio::test]
    async fn no_change_arms_the_cooldown() {
        let _serial = test_serial().lock().await;
        // Without this, an unrepairable credential spawns a 325MB process
        // every CACHE_TTL forever.
        reset_probe_state();
        let probe = AuthProbe::Command("/usr/bin/true".into());
        assert_eq!(
            refresh_via_owner_with(&probe, Some(1_000), || Some(1_000)).await,
            ProbeOutcome::NoChange
        );
        assert_eq!(
            refresh_via_owner_with(&probe, Some(1_000), || Some(9_999)).await,
            ProbeOutcome::Skipped,
            "second call within the cooldown must not spawn again — note the \
             read would report a refresh, so only the cooldown can produce Skipped"
        );
    }

    #[tokio::test]
    async fn a_moved_expiry_reports_refreshed() {
        let _serial = test_serial().lock().await;
        // The happy path, and the only test that would catch an inverted or
        // dropped comparison. Without it every assertion here is NoChange or
        // Skipped, which a `fn(..) -> NoChange` stub would satisfy.
        reset_probe_state();
        let probe = AuthProbe::Command("/usr/bin/true".into());
        let outcome = refresh_via_owner_with(&probe, Some(1_000), || Some(2_000)).await;
        assert_eq!(outcome, ProbeOutcome::Refreshed);
    }

    #[tokio::test]
    async fn an_expiry_that_moves_backwards_is_not_a_refresh() {
        let _serial = test_serial().lock().await;
        // Strictly-greater, not merely different: a store that rolled back is
        // not a successful refresh.
        reset_probe_state();
        let probe = AuthProbe::Command("/usr/bin/true".into());
        assert_eq!(
            refresh_via_owner_with(&probe, Some(2_000), || Some(1_000)).await,
            ProbeOutcome::NoChange
        );
    }

    #[tokio::test]
    async fn a_refresh_clears_a_previously_armed_cooldown() {
        let _serial = test_serial().lock().await;
        // Arm the cooldown with a fruitless probe, then prove a real refresh
        // releases it — otherwise one failure would suppress probes for 15
        // minutes even after the credential was repaired.
        reset_probe_state();
        let probe = AuthProbe::Command("/usr/bin/true".into());
        assert_eq!(
            refresh_via_owner_with(&probe, Some(1), || None).await,
            ProbeOutcome::NoChange
        );
        assert!(cooldown_active(), "fruitless probe must arm the cooldown");
        reset_probe_state();
        assert_eq!(
            refresh_via_owner_with(&probe, Some(1), || Some(2)).await,
            ProbeOutcome::Refreshed
        );
        assert!(!cooldown_active(), "a refresh must clear the cooldown");
    }

    #[tokio::test]
    async fn a_missing_binary_reports_no_change_not_a_panic() {
        let _serial = test_serial().lock().await;
        // A transplanted credential is a supported setup: a valid token with
        // no owner CLI installed.
        reset_probe_state();
        let probe = AuthProbe::Command("/nonexistent/claude".into());
        assert_eq!(
            refresh_via_owner_with(&probe, Some(1), || Some(2)).await,
            ProbeOutcome::NoChange
        );
    }
}
