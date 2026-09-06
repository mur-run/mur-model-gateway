#!/usr/bin/env bash
# Install a *released* mur-model-gateway build and register it as a user service.
#
# For people who just want to run the gateway: no Rust toolchain, no signing
# certificate, no repository checkout. scripts/setup.sh builds from source and
# needs both — this does not.
#
# Usage:
#   ./install-release.sh              # latest release
#   ./install-release.sh 0.3.0        # a specific version
#   ./install-release.sh --no-service # install the binary only, don't register
#   INSTALL_DIR=~/bin ./install-release.sh
set -euo pipefail

REPO=mur-run/mur-model-gateway
BIN=mur-model-gateway
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
SERVICE_LABEL=run.mur-model-gateway
BIND_PORT="${MUR_MODEL_GATEWAY_BIND_PORT:-8088}"

VERSION=""
WITH_SERVICE=1
for arg in "$@"; do
  case "$arg" in
    --no-service) WITH_SERVICE=0 ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    -*) echo "unknown flag: $arg" >&2; exit 1 ;;
    *) VERSION="${arg#v}" ;;
  esac
done

log() { printf '\033[36m[install]\033[0m %s\n' "$*"; }
ok()  { printf '\033[32m✓\033[0m %s\n' "$*"; }
err() { printf '\033[31m✗\033[0m %s\n' "$*" >&2; }

# ─── platform ───────────────────────────────────────────────────────
case "$(uname -s)" in
  Darwin) PLATFORM=macos;  ASSET_SUFFIX=macos-universal; SHACHECK="shasum -a 256 -c" ;;
  Linux)  PLATFORM=linux;  SHACHECK="sha256sum -c"
          [ "$(uname -m)" = x86_64 ] || { err "unsupported Linux arch: $(uname -m) (releases ship x86_64 only)"; exit 1; }
          ASSET_SUFFIX=linux-x86_64 ;;
  *) err "unsupported platform: $(uname -s). On Windows, download the .zip from the releases page."; exit 1 ;;
esac

# ─── resolve version ────────────────────────────────────────────────
if [ -z "$VERSION" ]; then
  log "looking up the latest release"
  VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"v\{0,1\}\([^"]*\)".*/\1/p' | head -1)
  [ -n "$VERSION" ] || { err "could not determine the latest release"; exit 1; }
fi
NAME="$BIN-$VERSION-$ASSET_SUFFIX.tar.gz"
BASE="https://github.com/$REPO/releases/download/v$VERSION"
log "installing $BIN $VERSION ($ASSET_SUFFIX)"

# ─── download + verify ──────────────────────────────────────────────
# The checksum is the whole point of downloading the second file: the macOS
# builds are notarized, but a Linux tarball carries no signature at all, and
# on either platform a truncated download is otherwise silent.
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
curl -fsSL -o "$TMP/$NAME" "$BASE/$NAME" || { err "download failed: $BASE/$NAME"; exit 1; }
curl -fsSL -o "$TMP/$NAME.sha256" "$BASE/$NAME.sha256" || { err "checksum download failed"; exit 1; }
( cd "$TMP" && $SHACHECK "$NAME.sha256" >/dev/null ) || { err "CHECKSUM MISMATCH — discarding $NAME, do not run it"; exit 1; }
ok "checksum verified"

tar -xzf "$TMP/$NAME" -C "$TMP"
mkdir -p "$INSTALL_DIR"
install -m 755 "$TMP/$BIN" "$INSTALL_DIR/$BIN"
ok "installed → $INSTALL_DIR/$BIN"

if [ "$WITH_SERVICE" = 0 ]; then ok "skipped service registration (--no-service)"; exit 0; fi

# ─── service ────────────────────────────────────────────────────────
log "registering the user service"
"$INSTALL_DIR/$BIN" install --compress >/dev/null

case "$PLATFORM" in
  macos)
    PLIST="$HOME/Library/LaunchAgents/$SERVICE_LABEL.plist"
    launchctl bootout "gui/$(id -u)/$SERVICE_LABEL" 2>/dev/null || true
    launchctl bootstrap "gui/$(id -u)" "$PLIST"
    launchctl enable "gui/$(id -u)/$SERVICE_LABEL"
    ;;
  linux)
    systemctl --user daemon-reload 2>/dev/null || true
    systemctl --user enable --now mur-model-gateway.service
    ;;
esac

# ─── verify ─────────────────────────────────────────────────────────
HEALTH="http://127.0.0.1:$BIND_PORT/__mur/health"
for _ in 1 2 3 4 5; do
  BODY=$(curl -fsS --max-time 5 "$HEALTH" 2>/dev/null) && break
  sleep 1
done

if [ -z "${BODY:-}" ]; then
  err "the service did not answer on 127.0.0.1:$BIND_PORT"
  case "$PLATFORM" in
    macos) err "check: tail ~/Library/Logs/mur-model-gateway/proxy.log" ;;
    linux) err "check: journalctl --user -u mur-model-gateway.service" ;;
  esac
  exit 1
fi

ok "listening on 127.0.0.1:$BIND_PORT"
echo "  $BODY"
echo
case "$BODY" in
  *'"claudeCredential":"oauth"'*)
    ok "Claude credential resolved" ;;
  *)
    # Not an install failure: the binary is fine, the credential is not
    # readable *yet*. Most often the keychain prompt below has not been
    # answered, or Claude Code was never logged in on this machine.
    err "Claude credential NOT resolved (claudeCredential is not \"oauth\")."
    err "  - macOS may have shown a keychain password prompt. Answer it with"
    err "    \"Always Allow\" and re-run the health check below."
    err "  - If you have never signed in here, run: claude auth login"
    ;;
esac

cat <<EOF

Point your client at the gateway:
  export ANTHROPIC_BASE_URL="http://127.0.0.1:$BIND_PORT"

Health : curl -s $HEALTH
Logs   : $( [ "$PLATFORM" = macos ] && echo 'tail -f ~/Library/Logs/mur-model-gateway/proxy.log' || echo 'journalctl --user -u mur-model-gateway.service -f' )
Remove : $( [ "$PLATFORM" = macos ] && echo "launchctl bootout gui/\$(id -u)/$SERVICE_LABEL" || echo 'systemctl --user disable --now mur-model-gateway.service' )
EOF
