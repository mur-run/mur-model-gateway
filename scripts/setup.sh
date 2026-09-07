#!/usr/bin/env bash
# mur-model-gateway setup: build release → install to ~/.local/bin → register
# as a user service (launchd on macOS, systemd --user on Linux).
#
# Re-runnable. Tears down any existing service before re-installing,
# so you can run this after every `git pull`.
#
# Usage:
#   ./scripts/setup.sh              # build + install + start
#   ./scripts/setup.sh --no-service # build + install binary only
#   ./scripts/setup.sh --uninstall  # tear down service, leave binary
#   ./scripts/setup.sh --musl       # static musl build (old-GLIBC hosts, e.g. Ubuntu 20.04)
#   ./scripts/setup.sh --system     # Linux: system-level unit (boots headless; needs sudo)
#   ./scripts/setup.sh -- --token-source env:MUR_MODEL_GATEWAY_OAUTH_TOKEN --bind 127.0.0.1:9099
#                                   # everything after -- goes to `mur-model-gateway install`
#   INSTALL_DIR=~/bin ./scripts/setup.sh   # override install location

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
INSTALL_PATH="$INSTALL_DIR/mur-model-gateway"
SERVICE_LABEL="run.mur-model-gateway"
BIND_PORT="${MUR_MODEL_GATEWAY_BIND_PORT:-8088}"

case "$(uname -s)" in
  Darwin) PLATFORM=macos ;;
  Linux)  PLATFORM=linux ;;
  *) echo "unsupported platform: $(uname -s)" >&2; exit 1 ;;
esac

# ─── helpers ────────────────────────────────────────────────────────

log() { printf '\033[36m[mur-model-gateway setup]\033[0m %s\n' "$*"; }
ok()  { printf '\033[32m✓\033[0m %s\n' "$*"; }
err() { printf '\033[31m✗\033[0m %s\n' "$*" >&2; }

teardown_service() {
  case "$PLATFORM" in
    macos)
      if launchctl print "gui/$(id -u)/$SERVICE_LABEL" >/dev/null 2>&1; then
        launchctl bootout "gui/$(id -u)/$SERVICE_LABEL" 2>/dev/null || true
      fi
      ;;
    linux)
      if [[ "${SYSTEM:-0}" == 1 ]]; then
        sudo systemctl disable --now mur-model-gateway.service 2>/dev/null || true
      else
        systemctl --user disable --now mur-model-gateway.service 2>/dev/null || true
      fi
      ;;
  esac
}

start_service() {
  case "$PLATFORM" in
    macos)
      launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/$SERVICE_LABEL.plist"
      launchctl enable "gui/$(id -u)/$SERVICE_LABEL"
      ;;
    linux)
      if [[ "${SYSTEM:-0}" == 1 ]]; then
        sudo systemctl daemon-reload
        sudo systemctl enable --now mur-model-gateway.service
      else
        systemctl --user daemon-reload
        systemctl --user enable --now mur-model-gateway.service
      fi
      ;;
  esac
}

is_listening() {
  if command -v lsof >/dev/null 2>&1; then
    # +c 0: without it lsof truncates COMMAND to 9 chars ("mur-model")
    lsof +c 0 -nP "-iTCP:$BIND_PORT" -sTCP:LISTEN 2>/dev/null | grep -q mur-model-gateway
  elif command -v ss >/dev/null 2>&1; then
    # ss reports /proc/comm, which the kernel caps at 15 chars
    ss -tlnp 2>/dev/null | grep -q ":$BIND_PORT.*mur-model-gatew"
  else
    (echo >"/dev/tcp/127.0.0.1/$BIND_PORT") 2>/dev/null
  fi
}

print_post_install_help() {
  cat <<EOF

mur-model-gateway is up on 127.0.0.1:$BIND_PORT.

Add to your shell init (\$HOME/.zshenv / \$HOME/.bashrc):
  export ANTHROPIC_BASE_URL="http://127.0.0.1:$BIND_PORT"

EOF
  case "$PLATFORM" in
    macos)
      cat <<EOF
Logs : tail -f ~/Library/Logs/mur-model-gateway/proxy.log
Stop : launchctl bootout gui/\$(id -u)/$SERVICE_LABEL
EOF
      ;;
    linux)
      if [[ "${SYSTEM:-0}" == 1 ]]; then
        cat <<EOF
Logs : journalctl -u mur-model-gateway.service -f
Stop : sudo systemctl disable --now mur-model-gateway.service
EOF
      else
        cat <<EOF
Logs : journalctl --user -u mur-model-gateway.service -f
Stop : systemctl --user disable --now mur-model-gateway.service
EOF
      fi
      ;;
  esac
}

# ─── argv parsing ───────────────────────────────────────────────────

ACTION=install
MUSL=0
SYSTEM=0
INSTALL_ARGS=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --no-service)  ACTION=binary_only ;;
    --uninstall)   ACTION=uninstall ;;
    --musl)        MUSL=1 ;;
    --system)      SYSTEM=1 ;;
    --)            shift; INSTALL_ARGS=("$@"); break ;;
    -h|--help)
      awk 'NR==1 { next } /^[^#]/ { exit } { sub(/^# ?/, ""); print }' "$0"
      exit 0
      ;;
    *) err "unknown flag: $1"; exit 2 ;;
  esac
  shift
done

if [[ "$SYSTEM" == 1 && "$PLATFORM" != linux ]]; then
  err "--system is Linux-only"; exit 2
fi

# ─── uninstall path ─────────────────────────────────────────────────

if [[ "$ACTION" == "uninstall" ]]; then
  log "tearing down service"
  teardown_service
  if [[ -x "$INSTALL_PATH" ]]; then
    "$INSTALL_PATH" uninstall || true
  fi
  ok "uninstalled (binary at $INSTALL_PATH left in place)"
  exit 0
fi

# ─── build ──────────────────────────────────────────────────────────

# ponytail: source cargo env so script works when invoked outside a login shell
[[ -f "$HOME/.cargo/env" ]] && source "$HOME/.cargo/env"
cd "$REPO_ROOT"
if [[ "$MUSL" == 1 ]]; then
  MUSL_TARGET=x86_64-unknown-linux-musl
  log "building mur-model-gateway (release, static $MUSL_TARGET)"
  if ! command -v cargo >/dev/null 2>&1 || ! cargo build --release --target "$MUSL_TARGET"; then
    if command -v docker >/dev/null 2>&1; then
      log "cargo unavailable or musl build failed → building via Docker (rust:1.91-bookworm)"
      docker run --rm -v "$REPO_ROOT":/src -w /src rust:1.91-bookworm bash -c \
        "rustup target add $MUSL_TARGET && apt-get update -qq && apt-get install -y -qq musl-tools && cargo build --release --target $MUSL_TARGET"
    else
      err "musl build failed and no docker available. On a glibc host you need:"
      err "  rustup target add $MUSL_TARGET && apt-get install musl-tools"
      exit 1
    fi
  fi
  BUILD_OUT="$REPO_ROOT/target/$MUSL_TARGET/release/mur-model-gateway"
else
  log "building mur-model-gateway (release)"
  cargo build --release
  BUILD_OUT="$REPO_ROOT/target/release/mur-model-gateway"
fi

# ─── codesign (macOS) ───────────────────────────────────────────────
# Sign with a stable identity and the `com.mur-model-gateway` identifier.
#
# This block used to be load-bearing for credential access: the keychain's
# grant on Claude Code-credentials is scoped to the signing team, so signing
# with the wrong one brought the password prompt back on every token rotation.
# That is no longer true. Since the gateway reads through /usr/bin/security
# (see src/keychain.rs), securityd sees an Apple-signed client and this
# binary's own signature does not participate in the read at all — verified
# with an ad-hoc-signed build that read the credential in 30ms.
#
# So signing is now about distribution — Gatekeeper and notarization, which
# matter in release.yml — not about whether the gateway works. A locally built
# binary carries no quarantine attribute, so an unsigned one runs fine here.
# Hence: prefer the best identity available, report what was used, and do not
# block an install over it.
if [[ "$PLATFORM" == macos ]]; then
  # Prefer "Developer ID Application" — the distribution cert — and never take
  # whatever `find-identity` prints first, which is keychain order rather than
  # a choice. That accident is how a shipped binary once ended up signed with
  # an Apple Development cert.
  SIGN_ID="${MUR_MODEL_GATEWAY_SIGN_IDENTITY:-}"
  if [[ -z "$SIGN_ID" ]]; then
    SIGN_ID=$(security find-identity -v -p codesigning 2>/dev/null \
      | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -1)
  fi
  if [[ -z "$SIGN_ID" ]]; then
    # Any valid identity is fine for a local install; prefer a real one over
    # ad-hoc only because it stays stable across rebuilds.
    SIGN_ID=$(security find-identity -v -p codesigning 2>/dev/null \
      | sed -n 's/.*"\([^"]*\)".*/\1/p' | head -1)
    if [[ -n "$SIGN_ID" ]]; then
      log "no Developer ID Application cert; using: $SIGN_ID"
      log "  fine for a local install — the keychain read goes through"
      log "  /usr/bin/security and does not depend on this signature."
      log "  Set MUR_MODEL_GATEWAY_SIGN_IDENTITY to choose a different one."
    fi
  fi
  if [[ -z "$SIGN_ID" ]]; then
    SIGN_ID="-"
    log "no codesigning identity at all; signing ad-hoc"
  fi

  log "codesigning with: $SIGN_ID"
  codesign -f -s "$SIGN_ID" -i com.mur-model-gateway "$BUILD_OUT"

  # Captured once, then matched in-shell. Piping `codesign -dvvv` into a
  # short-circuiting reader (`grep -q`, `head`) makes codesign die of SIGPIPE
  # and exit 141, which `set -o pipefail` above then reports as a failed check
  # — a false negative that blocks the install while printing the *correct*
  # identifier one line above the error. That shipped here once already.
  DESC=$(codesign -dvvv "$BUILD_OUT" 2>&1 || true)
  IDENT=$(printf '%s\n' "$DESC" | sed -n 's/^Identifier=//p')
  TEAM=$(printf '%s\n' "$DESC" | sed -n 's/^TeamIdentifier=//p')
  log "  Identifier=$IDENT"
  log "  TeamIdentifier=${TEAM:-<none, ad-hoc>}"
  # Still asserted, because `codesign` derives the identifier from the file
  # name when `-i` is missing and release.yml shipped exactly that in v0.1.0
  # and v0.2.0 with the evidence sitting in the build log the whole time.
  # A wrong identifier no longer costs a password prompt, but it does mean the
  # binary is not the artifact this project claims to produce.
  [ "$IDENT" = "com.mur-model-gateway" ] || {
    err "signed with the wrong identifier: '$IDENT' (expected com.mur-model-gateway)"
    exit 1
  }
fi

# ─── install binary ────────────────────────────────────────────────

mkdir -p "$INSTALL_DIR"
log "installing binary → $INSTALL_PATH"
install -m 755 "$BUILD_OUT" "$INSTALL_PATH"
ok "binary installed"

if [[ "$ACTION" == "binary_only" ]]; then
  ok "skipped service registration (--no-service)"
  exit 0
fi

# ─── service ────────────────────────────────────────────────────────

log "stopping any existing $SERVICE_LABEL"
teardown_service

log "writing service descriptor"
if [[ "$SYSTEM" == 1 ]]; then
  sudo "$INSTALL_PATH" install --system "${INSTALL_ARGS[@]+"${INSTALL_ARGS[@]}"}"
else
  "$INSTALL_PATH" install "${INSTALL_ARGS[@]+"${INSTALL_ARGS[@]}"}" >/dev/null
fi

log "starting service"
start_service

# settle: give systemd/launchd up to 5s to bring the listener up
for _ in 1 2 3 4 5; do
  is_listening && break
  sleep 1
done

if is_listening; then
  ok "listening on 127.0.0.1:$BIND_PORT"
  print_post_install_help
else
  err "service not listening on 127.0.0.1:$BIND_PORT"
  case "$PLATFORM" in
    macos) err "check: tail ~/Library/Logs/mur-model-gateway/proxy.log" ;;
    linux)
      if [[ "${SYSTEM:-0}" == 1 ]]; then
        err "check: journalctl -u mur-model-gateway.service"
      else
        err "check: journalctl --user -u mur-model-gateway.service"
      fi
      ;;
  esac
  exit 1
fi
