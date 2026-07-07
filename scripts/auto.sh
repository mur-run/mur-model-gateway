#!/usr/bin/env bash
# cc-proxy zero-question installer: detects platform quirks and calls
# setup.sh with the right flags. Safe to re-run.
#
#   ./scripts/auto.sh                 # detect everything, install, start, verify
#   CC_PROXY_TOKEN_SOURCE=env:MY_VAR ./scripts/auto.sh   # override token source
#
# Detection rules:
#   - Linux + GLIBC < 2.34            → --musl static build
#   - Linux + no active login session
#     (headless server / ssh-only)    → --system unit (needs sudo)
#   - token source: explicit env override > ~/.claude/.credentials.json
#     exists → file > default keychain (auto-falls back to file anyway)

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

FLAGS=()
INSTALL_FLAGS=()

if [[ "$(uname -s)" == Linux ]]; then
  # old glibc → static musl
  glibc="$(ldd --version 2>/dev/null | head -1 | grep -oE '[0-9]+\.[0-9]+$' || echo 0)"
  if [[ "$(printf '%s\n' "$glibc" 2.34 | sort -V | head -1)" != 2.34 ]]; then
    echo "[auto] GLIBC $glibc < 2.34 → static musl build"
    FLAGS+=(--musl)
  fi
  # no graphical/user session bus → user units won't start at boot; go system
  if [[ -z "${XDG_RUNTIME_DIR:-}" ]] || ! systemctl --user is-system-running >/dev/null 2>&1; then
    echo "[auto] no user session bus → system-level unit (sudo)"
    FLAGS+=(--system)
  fi
fi

if [[ -n "${CC_PROXY_TOKEN_SOURCE:-}" ]]; then
  echo "[auto] token source from env: $CC_PROXY_TOKEN_SOURCE"
  INSTALL_FLAGS+=(--token-source "$CC_PROXY_TOKEN_SOURCE")
elif [[ -f "$HOME/.claude/.credentials.json" ]]; then
  echo "[auto] found ~/.claude/.credentials.json → --token-source file"
  INSTALL_FLAGS+=(--token-source file)
else
  echo "[auto] no credentials file → default keychain source"
fi

if ((${#INSTALL_FLAGS[@]})); then
  exec ./scripts/setup.sh "${FLAGS[@]+"${FLAGS[@]}"}" -- "${INSTALL_FLAGS[@]}"
else
  exec ./scripts/setup.sh "${FLAGS[@]+"${FLAGS[@]}"}"
fi
