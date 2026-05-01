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
      systemctl --user disable --now cc-proxy.service 2>/dev/null || true
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
      systemctl --user daemon-reload
      systemctl --user enable --now cc-proxy.service
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
      cat <<EOF
Logs : journalctl --user -u cc-proxy.service -f
Stop : systemctl --user disable --now cc-proxy.service
EOF
      ;;
  esac
}

# ─── argv parsing ───────────────────────────────────────────────────

ACTION=install
for arg in "$@"; do
  case "$arg" in
    --no-service)  ACTION=binary_only ;;
    --uninstall)   ACTION=uninstall ;;
    -h|--help)
      awk 'NR==1 { next } /^[^#]/ { exit } { sub(/^# ?/, ""); print }' "$0"
      exit 0
      ;;
    *) err "unknown flag: $arg"; exit 2 ;;
  esac
done

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

log "building cc-proxy (release)"
cd "$REPO_ROOT"
cargo build --release

# ─── install binary ────────────────────────────────────────────────

mkdir -p "$INSTALL_DIR"
log "installing binary → $INSTALL_PATH"
install -m 755 "$REPO_ROOT/target/release/cc-proxy" "$INSTALL_PATH"
ok "binary installed"

if [[ "$ACTION" == "binary_only" ]]; then
  ok "skipped service registration (--no-service)"
  exit 0
fi

# ─── service ────────────────────────────────────────────────────────

log "stopping any existing $SERVICE_LABEL"
teardown_service

log "writing service descriptor"
"$INSTALL_PATH" install >/dev/null

log "starting service"
start_service

# brief settle
sleep 1

if is_listening; then
  ok "listening on 127.0.0.1:$BIND_PORT"
  print_post_install_help
else
  err "service not listening on 127.0.0.1:$BIND_PORT"
  case "$PLATFORM" in
    macos) err "check: tail ~/Library/Logs/cc-proxy/proxy.log" ;;
    linux) err "check: journalctl --user -u cc-proxy.service" ;;
  esac
  exit 1
fi
