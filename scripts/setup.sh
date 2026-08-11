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
    lsof -nP "-iTCP:$BIND_PORT" -sTCP:LISTEN 2>/dev/null | grep -q mur-model-gateway
  elif command -v ss >/dev/null 2>&1; then
    ss -tlnp 2>/dev/null | grep -q ":$BIND_PORT.*mur-model-gateway"
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
# Re-sign with a real identity + stable identifier so the keychain
# "Always Allow" grant survives rebuilds. The default linker ad-hoc
# signature changes on every build, which re-triggers the password
# prompt for the Claude Code-credentials item on each request.
if [[ "$PLATFORM" == macos ]]; then
  SIGN_ID="${MUR_MODEL_GATEWAY_SIGN_IDENTITY:-$(security find-identity -v -p codesigning 2>/dev/null | awk -F'"' 'NR==1 {print $2}')}"
  if [[ -n "$SIGN_ID" ]]; then
    log "codesigning with: $SIGN_ID"
    codesign -f -s "$SIGN_ID" -i com.mur-model-gateway "$BUILD_OUT"
  else
    log "no codesigning identity — skipping (keychain will re-prompt after every rebuild)"
  fi
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

# settle: give systemd/launchd time to bring the listener up.
# 15s, not 5: a freshly codesigned binary pays Gatekeeper verification on first launch.
for _ in $(seq 15); do
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
