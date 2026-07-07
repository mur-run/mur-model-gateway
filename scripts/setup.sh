#!/usr/bin/env bash
# cc-proxy setup: build release → install to ~/.local/bin → register
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
#   ./scripts/setup.sh -- --token-source env:CC_PROXY_OAUTH_TOKEN --bind 127.0.0.1:9099
#                                   # everything after -- goes to `cc-proxy install`
#   INSTALL_DIR=~/bin ./scripts/setup.sh   # override install location

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
INSTALL_PATH="$INSTALL_DIR/cc-proxy"
SERVICE_LABEL="run.cc-proxy"
BIND_PORT="${CC_PROXY_BIND_PORT:-8088}"

case "$(uname -s)" in
  Darwin) PLATFORM=macos ;;
  Linux)  PLATFORM=linux ;;
  *) echo "unsupported platform: $(uname -s)" >&2; exit 1 ;;
esac

# ─── helpers ────────────────────────────────────────────────────────

log() { printf '\033[36m[cc-proxy setup]\033[0m %s\n' "$*"; }
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
        sudo systemctl disable --now cc-proxy.service 2>/dev/null || true
      else
        systemctl --user disable --now cc-proxy.service 2>/dev/null || true
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
        sudo systemctl enable --now cc-proxy.service
      else
        systemctl --user daemon-reload
        systemctl --user enable --now cc-proxy.service
      fi
      ;;
  esac
}

is_listening() {
  if command -v lsof >/dev/null 2>&1; then
    lsof -nP "-iTCP:$BIND_PORT" -sTCP:LISTEN 2>/dev/null | grep -q cc-proxy
  elif command -v ss >/dev/null 2>&1; then
    ss -tlnp 2>/dev/null | grep -q ":$BIND_PORT.*cc-proxy"
  else
    (echo >"/dev/tcp/127.0.0.1/$BIND_PORT") 2>/dev/null
  fi
}

print_post_install_help() {
  cat <<EOF

cc-proxy is up on 127.0.0.1:$BIND_PORT.

Add to your shell init (\$HOME/.zshenv / \$HOME/.bashrc):
  export ANTHROPIC_BASE_URL="http://127.0.0.1:$BIND_PORT"

EOF
  case "$PLATFORM" in
    macos)
      cat <<EOF
Logs : tail -f ~/Library/Logs/cc-proxy/proxy.log
Stop : launchctl bootout gui/\$(id -u)/$SERVICE_LABEL
EOF
      ;;
    linux)
      if [[ "${SYSTEM:-0}" == 1 ]]; then
        cat <<EOF
Logs : journalctl -u cc-proxy.service -f
Stop : sudo systemctl disable --now cc-proxy.service
EOF
      else
        cat <<EOF
Logs : journalctl --user -u cc-proxy.service -f
Stop : systemctl --user disable --now cc-proxy.service
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
  log "building cc-proxy (release, static $MUSL_TARGET)"
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
  BUILD_OUT="$REPO_ROOT/target/$MUSL_TARGET/release/cc-proxy"
else
  log "building cc-proxy (release)"
  cargo build --release
  BUILD_OUT="$REPO_ROOT/target/release/cc-proxy"
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
    macos) err "check: tail ~/Library/Logs/cc-proxy/proxy.log" ;;
    linux)
      if [[ "${SYSTEM:-0}" == 1 ]]; then
        err "check: journalctl -u cc-proxy.service"
      else
        err "check: journalctl --user -u cc-proxy.service"
      fi
      ;;
  esac
  exit 1
fi
