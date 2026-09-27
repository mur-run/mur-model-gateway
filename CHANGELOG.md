# Changelog

Notable changes per release, newest first. Versions are the git tags that
`release.yml` publishes signed, notarized binaries for.

**Upgrading:** re-run `scripts/install-release.sh` (or `scripts/setup.sh` from a
checkout). The gateway is a long-lived background service — a new binary does
nothing until the service restarts, which both scripts do for you.

The **Upgrade** line on each release says who actually needs it. Not every
release is worth interrupting a working install for; the ones that are say so.

## Unreleased

## v0.5.0 — 2026-09-27

**Upgrade: yes** — stopping or reinstalling the service no longer kills
requests in flight, the OAuth keepalive is now on by default, and large
compressed sessions can read their files again. Re-run `setup.sh` /
`install-release.sh` so the service file picks up the new shutdown grace.

### Added

- `MUR_MODEL_GATEWAY_MAX_CONCURRENCY` caps simultaneous upstream calls per
  provider (unset = unlimited, as before). A call that cannot get a slot within
  `MUR_MODEL_GATEWAY_QUEUE_TIMEOUT_SECS` (default 30) gets a local `429` with
  `retry-after: 5` and a JSON body naming the provider and cap, instead of
  hanging until the caller's own timeout. A slot is held for the whole
  upstream stream, not just until headers. `0` or garbage in either variable
  is logged and treated as unset; both values appear in the `listening` line.
- `/__mur/health` reports `claudeHook` beside `codexHook`, so MUR Hub can tell
  a build without the Anthropic disguise hook from an unreachable gateway.
- `scripts/install-with-omlx.sh`: one-shot Apple Silicon setup that builds the
  gateway from source with compression on and sets up a local oMLX embedding
  server beside it. `--check` runs only the preflight.

### Fixed

- Stopping or restarting the service no longer cuts requests in flight. launchd
  (`launchctl kickstart -k`, `bootout`, a `setup.sh` reinstall) and systemd stop
  the gateway with SIGTERM, which it used to ignore, so the process died on
  the spot. SIGTERM now goes through the same graceful shutdown as Ctrl-C:
  requests already in flight finish, and the log shows
  `shutdown signal signal="SIGTERM"` before the process exits.
- The service manager now waits longer for that drain before it force-kills
  the gateway: 300 s on systemd (`TimeoutStopSec` in both units), 60 s on
  macOS. The launchd plist asks for 300 s too (`ExitTimeOut`), but launchd
  caps `ExitTimeOut` at 60 s: throwaway agents set to 59 and 60 keep their
  value, while 61 and 120 both come back as `exit timeout = 60` in
  `launchctl print`. Without the key, launchd
  typically allows only a few seconds (5 s for most agents), and systemd
  90 s, which cut long streaming responses mid-generation. On macOS a stream
  still running 60 s after the stop is still cut. An idle gateway exits
  immediately; the limit only matters while requests are in flight.
  **Upgrade:** takes effect on the next `setup.sh` / `install`, which
  rewrites the service file.
- With `MUR_MODEL_GATEWAY_COMPRESS=1`, the gateway no longer compresses the
  output of `Read`, `mur_retrieve`, or a Bash `mur retrieve`. It used to
  compress every tool result, so a retrieval came back under a new hash and
  could never be read in full, and file reads turned into stubs. Anthropic
  wire format only.
- OpenAI-style vision requests on the Codex route no longer arrive empty.
  Array `content` (`text` + `image_url`) was read as a plain string, so both
  the text and the image vanished and the model answered a blank turn. Parts
  now map to `input_text` / `input_image`; string content is unchanged.
- The 401 Fix line for a rejected Claude OAuth credential now says to run
  `claude auth logout` before `claude auth login`. On its own, `login` reports
  "already authenticated" and keeps the stale token.
- A client that hangs up before the response starts now leaves one WARN line,
  `client went away before a response`, with `method`, `path` (without the
  query string), `provider` and `waited_ms`. Until now such a request left no
  line at all, and "the client left" could only be reached by ruling out
  everything else. It does not fire for a client that leaves mid-stream (that
  request already logged `proxied`), or for a process stopped by SIGTERM or
  SIGKILL.
  **Upgrade:** optional — only changes what the log says.
- The install scripts no longer report a healthy service as failed. Both
  checked the port in `MUR_MODEL_GATEWAY_BIND_PORT` (default 8088), a variable
  the gateway never reads, so `setup.sh -- --bind 127.0.0.1:9099` — the bind
  from the script's own usage example — started the service on 9099, waited
  on 8088, and gave up. `setup.sh` now takes the port from `--bind`, rejecting
  a malformed one before it builds; `install-release.sh`, which never passes
  `--bind`, checks `127.0.0.1:8088`. Setting `MUR_MODEL_GATEWAY_BIND_PORT` now
  prints a note and is otherwise ignored. On Linux the `ss` check also stopped
  counting a listener on `:8088` as one on `:80`.
  **Upgrade:** nothing to install — the fix is in the scripts, not the binary.
  If a run with `--bind` said the service never came up, check
  `/__mur/health` on that address; it was likely running all along.

### Changed

- The OAuth keepalive is now **on by default**. With no Claude Code session
  open for 8 hours the stored token aged out and every agent request 401'd
  until someone next ran `claude`; the gateway now runs one small haiku
  `claude -p` just before expiry (about three a day) so Claude Code renews it.
  `MUR_MODEL_GATEWAY_OAUTH_KEEPALIVE=0` turns it off, and an install run with
  that set keeps it off in the service definition.
  **Upgrade:** yes, if your agents greet you with a 401 each morning.

## v0.4.1 — 2026-09-15

### Fixed

- Requests larger than 10 MiB no longer fail with `502 read incoming body`. A
  conversation carrying a few screenshots crosses that line easily, and the
  gateway — not the upstream, which takes 32 MB — was the one refusing it. The
  buffer the gateway needs to disguise, compress, or translate a body is now
  capped at 32 MiB, above the upstream's own ceiling, so the upstream decides.
  **Upgrade:** yes, if you paste images or share long sessions — the failure
  looked transient and the client retried it ten times, all in vain.
- `proxy error` log lines now carry the whole error chain
  (`read incoming body: length limit exceeded`), not just the outermost step.

### Changed

- An Anthropic 401 on a credential the gateway attached is now retried when —
  and only when — the credential store has since come to hold a *different*
  token. Previously the retry was gated on a delegated-refresh probe that
  spawned `claude auth status` and compared `expiresAt` before and after.
  `claude auth status` does not rewrite the credential (verified against the
  keychain item's modification date) and `claude auth` has no refresh
  subcommand, so the probe could only ever report no change, arm a 15-minute
  cooldown, and suppress the retry. Comparing the token directly also fixes
  the opposite gap: a credential the user had just re-authenticated was
  previously read as "revoked, not aged out" and never retried.
- The 401 body says what the request actually did. It used to claim "an
  automatic refresh did not resolve it" in every case, including the ones
  where nothing was attempted.

### Removed

- `MUR_MODEL_GATEWAY_NO_AUTH_PROBE`, and the probe it opted out of. Nothing is
  spawned on a 401 any more, so there is nothing to disable. Setting the
  variable is now inert rather than an error.

**Upgrade:** worth it if you have ever seen `auth refused (401)` from a client
and re-authenticated a credential that turned out to be fine. Otherwise
routine — the retry that actually worked is unchanged in effect, only in what
gates it.

## v0.4.0 — 2026-09-07

**Upgrade: yes, on macOS — this contains the fix for the repeated keychain
password prompts, and `v0.3.0` does not.**

- **Resolve `claude` outside `PATH` (#25).** launchd hands a service
  `/usr/bin:/bin:/usr/sbin:/sbin`, which holds no user-installed binary, so
  `claude` in `~/.local/bin` was invisible. Two features had therefore never
  run on a service install: delegated refresh, and `cc_version` detection —
  which meant every disguised request carried the hardcoded fallback version
  rather than the installed one. `which_claude` now searches `PATH` first, then
  the locations the CLI installs to.
- **Drop the cached credential when upstream rejects it (#26).** A token
  revoked while its stored expiry was still in the future never left the cache,
  so `claude auth login` changed nothing until the service was restarted by
  hand. Rate-limited to once a minute.

- **Read the keychain through `/usr/bin/security` (#23).** Claude Code resets
  the credential item's ACL partition list to `apple-tool:` on every token
  rotation, dropping the gateway's team. securityd then refused the gateway and
  put a password dialog on screen — measured at 73s and 178s of a request
  stalling while it waited to be clicked. `/usr/bin/security` is Apple-signed
  and so sits in the one partition that survives the reset. The gateway's own
  code signature no longer participates in the read at all, which also stops a
  debug build from prompting.
- **Serve a stale-but-valid credential during a refresh (#22).** The cache lock
  was held across the keychain read, so every concurrent request waited for it.
  The refresher now absorbs the wait alone; callers with nothing valid to serve
  still wait rather than answer wrongly.
- **`scripts/setup.sh` no longer refuses to install without a Developer ID
  certificate (#24).** That check existed because the keychain grant was scoped
  to the signing team. After #23 it is not, so the check was blocking installs
  for a reason that had stopped being true.

## v0.3.0 — 2026-09-06

**Upgrade: superseded. Go straight to the next release** — this one still
prompts for the keychain password at every token rotation on macOS.

- Cache the Claude credential until its own `expiresAt` instead of a flat 60s
  (#14), cutting keychain authorizations from ~1440/day to ~3.
- Skip the macOS keychain read whose result is discarded when naming the
  credential store in a 401 body (#15). Also stops `cargo test` prompting.
- Report the build version in `/__mur/health` (#18), so a bug report can say
  which binary produced it.
- Install from a published release without a Rust toolchain or a signing
  certificate: `scripts/install-release.sh`, plus `docs/staff-testing.md` (#19).
- **Fix the release signing identifier (#19).** `v0.1.0` and `v0.2.0` were
  signed as `mur-model-gateway` rather than `com.mur-model-gateway`, so a
  release-installed gateway could not use the keychain grant a source-installed
  one held.
- Instrument the keychain read and the cache wait (#20) — the measurements that
  located the real cause, fixed in the next release.
- `scripts/setup.sh` signing fixes: prefer Developer ID (#16), and stop a
  `grep -q` SIGPIPE from failing the identifier check under `pipefail` (#21).

## v0.2.0 — 2026-08-30

- **Delegated refresh on an expired Anthropic 401 (#5):** ask the owning
  `claude` CLI to refresh, then retry once. Single-flight, with a cooldown.
  Says so when the probe cannot be armed (#6).
- **Codex route:** `/codex/v1/chat/completions` translated to the Responses API,
  streaming and non-streaming, tool definitions and tool-call history; API-key
  mode (#4).
- `/__mur/health` reports Codex readiness (#11) and the Claude credential kind
  (#12) — a kind, never a token.
- Resolve `claude` through `PATHEXT` on Windows (#8), where delegated refresh
  had been unreachable.
- Prefer the Developer ID Application signing identity rather than whatever
  `security find-identity` printed first (#7).

## v0.1.0 — 2026-08-04

First public release.

- Multi-provider reverse proxy: Anthropic, OpenAI, Gemini, Codex.
- Claude Code disguise layer with auto-detected `cc_version`.
- Wire-level `tool_result` compression via mur-compress.
- Service install across macOS (launchd), Linux (systemd, user or system) and
  Windows (Task Scheduler).
- Signed macOS builds with a stable identifier, so the keychain grant survives
  a rebuild (#1).
