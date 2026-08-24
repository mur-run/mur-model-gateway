//! Delegated refresh: ask the credential's owner CLI to refresh it, then
//! check whether it did. The gateway never reads or redeems the refresh
//! token itself — see the spec's Rejected section for why.

use crate::AuthProbe;
use crate::TokenSource;
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

/// Serialises probes: held across the child's whole execution so no two
/// children ever run at once, and so a queued caller re-checks fresh
/// post-probe state (cooldown, then the dedup check below) instead of the
/// stale snapshot it saw before waiting. Serialising is necessary but not
/// sufficient to stop duplicate spawns on its own — see the pre-spawn check
/// in `refresh_via_owner_with` for the other half: without it, a caller that
/// wakes up after a *successful* probe would still spawn its own, since
/// serialising only guarantees no overlap, not "don't bother". Same
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
/// re-read afterwards **from `source`**, via [`TokenSource::resolve_credential`]
/// — the single shared fallback path (fix round 1, CRITICAL 1) — and the two
/// compared:
///
/// - `(Some(before), Some(after))` — `Refreshed` iff `after > before`.
/// - `(None, Some(_))` — the blob had no expiry before the probe and has one
///   now: also `Refreshed`. A missing `before_ms` most commonly means the
///   *pre-probe* read failed to resolve a credential at all (no entry yet, or
///   a backend error), so gaining one is itself the signal, not something to
///   compare against a value that never existed.
/// - `(None, None)` — still `NoChange`. This is a real, permanent gap: a
///   Claude Code install that never writes `expiresAt` at all makes this
///   feature unable to ever detect its own refresh, on every call, forever.
///   It is bounded, not unbounded, damage — `PROBE_COOLDOWN` caps it at one
///   wasted retry per cooldown window, not a spawn loop — but it does not
///   self-heal the way the `(None, Some(_))` case does.
///
/// Before `resolve_credential` existed, this read a hardcoded Keychain-only
/// call for the `Keychain` variant, ignoring `source`'s non-macOS
/// credentials-file fallback entirely. That was wrong on every Linux/Windows
/// install (no OS keychain backend, or `TokenSource::Keychain` with no entry
/// yet) — the post-probe read would never see the file the probe just
/// rewrote, so a genuine refresh would be reported as `NoChange` (arming a
/// 15-minute cooldown over a credential that is actually fine now).
/// `resolve_credential` fixes that once, for every caller, instead of this
/// function reimplementing the fallback on its own.
pub async fn refresh_via_owner(
    probe: &AuthProbe,
    source: &TokenSource,
    before_ms: Option<i64>,
) -> ProbeOutcome {
    refresh_via_owner_with(probe, before_ms, || {
        // Bypass the 60s memoise: the child may have just rewritten the
        // keychain-backed store, and a cached read would return the token we
        // already know is dead. Inert (not a bug) when `source` isn't
        // keychain-backed at all — `CredentialsFile` reads are already
        // uncached, so invalidating a cache they never consult is a no-op.
        keychain::invalidate_cache();
        source
            .resolve_credential()
            .ok()
            .flatten()
            .and_then(|c| c.expires_at_ms)
    })
    .await
}

/// The body, with the post-probe credential read injected. Tests drive every
/// outcome through this; production goes through the wrapper above, which
/// supplies a real, source-matched read. Without this seam the `Refreshed`
/// outcome could not be tested at all — it would depend on the machine's live
/// keychain. `read_after` is `Fn`, not `FnOnce`: it is called up to twice, once
/// for the pre-spawn dedup check and again after the child exits.
pub(crate) async fn refresh_via_owner_with(
    probe: &AuthProbe,
    before_ms: Option<i64>,
    read_after: impl Fn() -> Option<i64>,
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

    // Dedup: someone else may have already fixed this while we were queued
    // behind PROBE_LOCK above. Check before spawning our own child. This is
    // *not* a substitute for PROBE_LOCK — two callers that both reach this
    // line before either has produced a result both see "unmoved" here and
    // both spawn; PROBE_LOCK is what forces that not to happen by making one
    // of them wait until the other has fully finished (spawned, waited, and
    // re-read) before it even gets this far.
    //
    // Deliberately narrower than the post-spawn match below: this early-out
    // only fires on `(Some(before), Some(after))`, not also on
    // `(None, Some(_))`. A queued caller with a `None` `before_ms` falls
    // through and spawns its own probe even when the winner ahead of it in
    // PROBE_LOCK already repaired the credential. That's a bounded cost
    // (one extra child, under PROBE_LOCK's serialisation, not a loop), not a
    // correctness bug — the post-spawn match a few lines down still reports
    // it as `Refreshed` — so it's left as-is rather than widened to match.
    if matches!((before_ms, read_after()), (Some(before), Some(after)) if after > before) {
        *COOLDOWN.lock().unwrap() = None;
        return ProbeOutcome::Refreshed;
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
                // Deliberately falls through to the same re-read below rather
                // than returning here. A credential that moved has moved
                // regardless of whether the child exited cleanly or was
                // killed for running long — reporting NoChange in that case
                // would arm a 15-minute cooldown over a credential that is
                // actually fine now.
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
        // No expiry existed before the probe and one exists now: the blob
        // gained an expiry, which is itself evidence of a refresh — there is
        // nothing to compare it against, so treat gaining one as the signal.
        (None, Some(_)) => {
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
///
/// Public because `tests/anthropic_retry_arm.rs` (an external integration
/// test binary) calls it directly — not because it's part of the crate's
/// intended API surface.
#[doc(hidden)]
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

    /// I4: `before_ms` is `None` — the pre-probe read found no expiry at all
    /// (no entry yet, or a blob that omits `expiresAt`) — and the post-probe
    /// read finds one. Must report `Refreshed`: with only the old
    /// `(Some,Some) if after > before` arm, this falls into the catch-all and
    /// reports `NoChange` — permanently, since `before_ms` can never become
    /// `Some` for a credential that starts with no expiry, so this outcome
    /// would repeat on every single call. Uses `/usr/bin/true` (a real spawn
    /// that touches nothing) rather than the pre-spawn dedup check, since
    /// that check only special-cases `(Some,Some)` and deliberately does not
    /// short-circuit `(None, Some(_))` — see the comment above it — so this
    /// test exercises the post-spawn match, the arm this finding is about.
    #[tokio::test]
    async fn a_credential_that_gains_an_expiry_is_a_refresh() {
        let _serial = test_serial().lock().await;
        reset_probe_state();
        let probe = AuthProbe::Command("/usr/bin/true".into());
        let outcome = refresh_via_owner_with(&probe, None, || Some(2_000)).await;
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
        // no owner CLI installed. `read_after` must be non-moving (equal to
        // `before_ms`, not greater) — with the pre-spawn dedup check, a
        // moving fixture here would short-circuit to `Refreshed` before ever
        // attempting the spawn, and this test would stop testing the
        // missing-binary path it's named for.
        reset_probe_state();
        let probe = AuthProbe::Command("/nonexistent/claude".into());
        assert_eq!(
            refresh_via_owner_with(&probe, Some(1), || Some(1)).await,
            ProbeOutcome::NoChange
        );
    }

    /// Proves `PROBE_LOCK` actually serialises concurrent callers, not just
    /// that the module's own tests don't trip over each other. Every test
    /// above calls `refresh_via_owner_with` from one sequential `.await`
    /// chain — none of them create two calls in flight at once, so none of
    /// them can tell a working `PROBE_LOCK` from a deleted one. This one
    /// does, with two real `tokio::spawn`ed callers.
    ///
    /// Needs `flavor = "multi_thread"`: the default current-thread runtime
    /// every other test in this file uses cannot run two tasks at the same
    /// instant, which would make the race this test wants to create
    /// impossible regardless of `PROBE_LOCK`.
    ///
    /// Still takes `test_serial()` around its own body — unlike the finding
    /// that prompted this test suggested ("does not hold TEST_SERIAL"),
    /// holding it here does not defeat the point: `test_serial()` only
    /// blocks this test's *outer* task from overlapping a sibling test, it
    /// does not block the two inner `tokio::spawn`ed tasks below from racing
    /// each other on `PROBE_LOCK` — that's a different lock, and simply
    /// holding an already-acquired guard blocks no one else from running.
    /// Dropping it instead would let this test interleave with any sibling
    /// mid-assertion on the same process-global `COOLDOWN`, reintroducing
    /// the exact flake this file's `TEST_SERIAL` exists to prevent.
    // C1: writes and executes a `#!/bin/sh` fixture — Windows has no shebang
    // interpreter, so this must not even compile the test in on that target,
    // not merely skip running it (a `#[cfg(unix)]` inner chmod block alone
    // still leaves the spawn itself attempted on Windows).
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_callers_spawn_exactly_one_child() {
        let _serial = test_serial().lock().await;
        reset_probe_state();

        // A real probe binary that sleeps long enough to make the race
        // observable, then appends one line to `log` — a fake `claude auth
        // status` that "fixes" the credential the slow way. Args are fixed
        // to `auth status` by `refresh_via_owner_with`, so the log path is
        // baked into the script itself rather than passed as an argument.
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("calls.log");
        let script = dir.path().join("probe.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nsleep 0.1\necho x >> {}\n", log.display()),
        )
        .expect("write fake probe script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o700);
            std::fs::set_permissions(&script, perms).unwrap();
        }

        // `read_after` reports the log's line count as the "expiry": 0
        // before the fake probe has run, 1 after. Real content, not a
        // constant — a constant closure can't distinguish "ran before me"
        // from "hasn't run yet", which is exactly what the pre-spawn dedup
        // check and the post-spawn re-read need to tell apart.
        fn line_count(path: &std::path::Path) -> Option<i64> {
            let n = std::fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .count();
            Some(n as i64)
        }

        let probe_a = AuthProbe::Command(script.clone());
        let log_a = log.clone();
        let probe_b = AuthProbe::Command(script.clone());
        let log_b = log.clone();

        let (r1, r2) = tokio::join!(
            tokio::spawn(async move {
                refresh_via_owner_with(&probe_a, Some(0), || line_count(&log_a)).await
            }),
            tokio::spawn(async move {
                refresh_via_owner_with(&probe_b, Some(0), || line_count(&log_b)).await
            })
        );

        let lines = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .count();
        assert_eq!(
            lines, 1,
            "PROBE_LOCK must serialise concurrent callers to exactly one \
             spawn — the second caller should observe the first's result \
             via the dedup check instead of spawning its own"
        );
        // Both callers see a moved expiry by the time they return: the
        // winner via its own post-spawn read, the loser via the pre-spawn
        // dedup check reading what the winner already wrote.
        assert_eq!(r1.unwrap(), ProbeOutcome::Refreshed);
        assert_eq!(r2.unwrap(), ProbeOutcome::Refreshed);
    }

    /// The bug this fixes: `refresh_via_owner` used to re-read the OS
    /// keychain unconditionally, so a `CredentialsFile`-sourced gateway
    /// (every Linux/Windows install) could never observe a probe that
    /// successfully rewrote the file — it would report `NoChange` even
    /// though the credential was genuinely repaired. Would fail against
    /// that version: the fake `claude` below touches only the file, never
    /// the keychain, so a keychain-only read sees nothing move.
    // C1: writes and executes a `#!/bin/sh` fixture (the fake `claude`) —
    // must not compile in on Windows, not merely skip at run time.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn refresh_via_owner_reads_the_configured_source_not_just_the_keychain() {
        let _serial = test_serial().lock().await;
        reset_probe_state();

        let dir = tempfile::tempdir().expect("tempdir");
        let creds = dir.path().join("creds.json");
        std::fs::write(
            &creds,
            r#"{"claudeAiOauth":{"accessToken":"old","expiresAt":1000}}"#,
        )
        .expect("write initial creds");

        // A fake `claude auth status` that rewrites the file with a later
        // expiry — the CredentialsFile counterpart of the log-writing fake
        // probe above.
        let script = dir.path().join("claude");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncat > '{}' <<'EOF'\n{{\"claudeAiOauth\":{{\"accessToken\":\"new\",\"expiresAt\":9999}}}}\nEOF\n",
                creds.display()
            ),
        )
        .expect("write fake claude");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake claude");
        }

        let probe = AuthProbe::Command(script);
        let source = TokenSource::CredentialsFile(creds);
        let outcome = refresh_via_owner(&probe, &source, Some(1_000)).await;
        assert_eq!(
            outcome,
            ProbeOutcome::Refreshed,
            "refresh_via_owner must detect a refresh written to the configured \
             CredentialsFile, not only to the OS keychain"
        );
    }

    /// Fix round 1, CRITICAL 1: `TokenSource::Keychain` is the *production
    /// default*. Before `resolve_credential`/`keychain_fallback` existed,
    /// this function's own `read_after` reimplemented a Keychain-only read
    /// with no fallback — so on every default Linux/Windows install (no OS
    /// keychain backend, Claude Code writes `~/.claude/.credentials.json`
    /// instead) a probe that genuinely repaired the file was reported as
    /// `NoChange`, not `Refreshed`, silently breaking the whole feature on
    /// exactly the platform pairing it exists to fix.
    ///
    /// Can't drive this through `TokenSource::Keychain::resolve_credential()`
    /// directly and stay deterministic across dev machines: that method
    /// calls `keychain_fallback(cfg!(target_os = "macos"), ...)`, which
    /// bakes in the *real* host platform at compile time — on a macOS test
    /// runner it would short-circuit straight to the live OS keychain and
    /// never reach the fallback branch, for any fixture this test could
    /// write. `keychain_fallback` takes `is_macos` as an explicit parameter
    /// for exactly this reason (see its doc comment in `src/lib.rs`), so
    /// `read_after` here calls it directly with `is_macos: false` and a
    /// synthetic keychain-unavailable `Err` — reproducing exactly what a
    /// real non-macOS `TokenSource::Keychain::resolve_credential()` call
    /// computes, without touching this machine's real keychain.
    // C1: writes and executes a `#!/bin/sh` fixture (the fake `claude`) —
    // must not compile in on Windows, not merely skip at run time.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn keychain_default_falls_back_to_credentials_file_on_non_macos() {
        let _serial = test_serial().lock().await;
        reset_probe_state();

        let dir = tempfile::tempdir().expect("tempdir");
        let creds = dir.path().join("creds.json");
        std::fs::write(
            &creds,
            r#"{"claudeAiOauth":{"accessToken":"old","expiresAt":1000}}"#,
        )
        .expect("write initial creds");

        // Same fake `claude auth status` shape as the test above: rewrites
        // the credentials-file fixture with a later expiry.
        let script = dir.path().join("claude");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncat > '{}' <<'EOF'\n{{\"claudeAiOauth\":{{\"accessToken\":\"new\",\"expiresAt\":9999}}}}\nEOF\n",
                creds.display()
            ),
        )
        .expect("write fake claude");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake claude");
        }

        let probe = AuthProbe::Command(script);
        let creds_for_read = creds.clone();

        let outcome = refresh_via_owner_with(&probe, Some(1_000), || {
            let err = keychain::KeychainError::Backend("no keychain backend available".into());
            crate::keychain_fallback(false, Err(err), Some(creds_for_read.clone()))
                .ok()
                .flatten()
                .and_then(|c| c.expires_at_ms)
        })
        .await;

        assert_eq!(
            outcome,
            ProbeOutcome::Refreshed,
            "a Keychain-default source with no keychain backend available must \
             fall back to the credentials file and observe the probe's rewrite \
             there — reporting NoChange here is the exact symptom of the bug \
             this test guards against"
        );
    }
}
