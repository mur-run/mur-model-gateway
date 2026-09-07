# Changelog

Notable changes per release, newest first. Versions are the git tags that
`release.yml` publishes signed, notarized binaries for.

**Upgrading:** re-run `scripts/install-release.sh` (or `scripts/setup.sh` from a
checkout). The gateway is a long-lived background service — a new binary does
nothing until the service restarts, which both scripts do for you.

The **Upgrade** line on each release says who actually needs it. Not every
release is worth interrupting a working install for; the ones that are say so.

## Unreleased

**Upgrade: yes, on macOS — this contains the fix for the repeated keychain
password prompts, and `v0.3.0` does not.**

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
