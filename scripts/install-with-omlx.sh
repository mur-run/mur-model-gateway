#!/usr/bin/env bash
# install-with-omlx.sh — 一鍵安裝 mur-model-gateway（從原始碼編譯、開壓縮）+ oMLX embedding。
# 只支援 macOS Apple Silicon。
# 不需要 Homebrew。缺 Rust / uv 會自動用官方安裝器補；port 8000 被別人占用就停。
#
# 裝的是公開版：沒有私有 impl，codex / disguise 用 stub（/__mur/health 會標示）。
#
# 用法：
#   ./scripts/install-with-omlx.sh           # 全部裝好；輸出同時存到 ~/.mur/omlx/logs/install-<時間>.log
#   ./scripts/install-with-omlx.sh --check   # 只做前置檢查，什麼都不安裝，也不存 log
#
# 可覆寫：
#   MUR_GATEWAY_DIR   gateway 原始碼位置（預設：在 repo 裡執行就用這份，否則 ./mur-model-gateway）
#   MUR_OMLX_HOME     oMLX 的 venv、設定、模型、log（預設 ~/.mur/omlx，跟 oMLX.app 的 ~/.omlx 分開）
set -euo pipefail

REPO_URL="https://github.com/mur-run/mur-model-gateway.git"

# oMLX 固定版本：/releases/latest 可能指到 rc 版，所以不用 latest。
# SHA-256 與 GitHub release 上公布的 digest 一致。
OMLX_VERSION="0.6.4"
OMLX_WHEEL_NAME="omlx-${OMLX_VERSION}-cp312-cp312-macosx_15_0_universal2.whl"
OMLX_WHEEL_URL="https://github.com/jundot/omlx/releases/download/v${OMLX_VERSION}/${OMLX_WHEEL_NAME}"
OMLX_WHEEL_SHA256="f13d92900bf6c7e925e9a6d5525b4465c615c404ee796d328ffc5d4d379ddb0b"
OMLX_PYTHON="3.12"           # wheel 是 cp312；uv 沒找到就自己下載，不用系統 Python
OMLX_HOST="127.0.0.1"
OMLX_PORT="8000"             # MUR 預設找 http://127.0.0.1:8000/v1
OMLX_APP_PORT="8001"         # oMLX.app 讓出 8000 後改用這個 port
OMLX_APP_SETTINGS="$HOME/.omlx/settings.json"
OMLX_LABEL="com.mur.omlx"
OMLX_HOME="${MUR_OMLX_HOME:-$HOME/.mur/omlx}"
OMLX_VENV="$OMLX_HOME/venv"
OMLX_BIN="$OMLX_VENV/bin/omlx"
MODEL_ID="mlx-community/Qwen3-Embedding-0.6B-8bit"
MODEL_NAME="${MODEL_ID##*/}"
MODEL_DIR="$OMLX_HOME/models/$MODEL_NAME"
LOG_DIR="$OMLX_HOME/logs"
LAUNCH_AGENT="$HOME/Library/LaunchAgents/$OMLX_LABEL.plist"
GUI_DOMAIN="gui/$(id -u)"

# gateway 裝到哪裡由 scripts/setup.sh 決定，這裡只是抄過來給完成摘要用。
# 本腳本不傳 --bind，所以是 setup.sh 的預設 127.0.0.1:8088。
GATEWAY_HOST="127.0.0.1"
GATEWAY_PORT="8088"
GATEWAY_URL="http://$GATEWAY_HOST:$GATEWAY_PORT"
GATEWAY_LABEL="run.mur-model-gateway"                                        # setup.sh SERVICE_LABEL
GATEWAY_BIN="${INSTALL_DIR:-$HOME/.local/bin}/mur-model-gateway"             # setup.sh INSTALL_PATH
GATEWAY_PLIST="$HOME/Library/LaunchAgents/$GATEWAY_LABEL.plist"              # src/install.rs
GATEWAY_LOG_DIR="$HOME/Library/Logs/mur-model-gateway"                       # src/install.rs
# 注意：原始碼目錄叫 GATEWAY_SRC，不要叫 INSTALL_DIR。使用者環境裡如果 export 過 INSTALL_DIR，
# 在這裡改它的值會被 setup.sh 繼承，binary 就會裝進原始碼目錄。
MIN_RUST="1.85"              # Cargo.toml: edition = "2024"

# GATEWAY_SRC_OWNED=1 才把「刪原始碼」列進移除步驟：指定的目錄或正在開發的 repo 不能叫人 rm -rf。
GATEWAY_SRC_OWNED=0
if [[ -n "${MUR_GATEWAY_DIR:-}" ]]; then
  GATEWAY_SRC="$MUR_GATEWAY_DIR"
elif [[ -d "$PWD/.git" && -f "$PWD/build.rs" && -f "$PWD/scripts/setup.sh" ]]; then
  GATEWAY_SRC="$PWD"         # 在 repo 裡直接執行：用這份，不要再 clone 一份進來
else
  GATEWAY_SRC="$PWD/mur-model-gateway"
  GATEWAY_SRC_OWNED=1
fi

CHECK_ONLY=0
case "${1:-}" in
  "") ;;
  --check) CHECK_ONLY=1 ;;
  -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
  *) echo "Unknown option: ${1}（只接受 --check）" >&2; exit 2 ;;
esac

log()  { printf '\n==> %s\n' "$*"; }
info() { printf '    %s\n' "$*"; }
die()  { printf '\nError: %s\n' "$*" >&2; exit 1; }

# $1 >= $2？比較點分版本號（只看前三段數字）
version_ge() {
  local IFS=.
  local -a a b
  read -r -a a <<<"$1"
  read -r -a b <<<"$2"
  local i x y
  for i in 0 1 2; do
    x="${a[i]:-0}"; y="${b[i]:-0}"
    if (( 10#$x > 10#$y )); then return 0; fi
    if (( 10#$x < 10#$y )); then return 1; fi
  done
  return 0
}

port_listening() { /usr/bin/nc -z -G 1 "$OMLX_HOST" "$OMLX_PORT" >/dev/null 2>&1; }

# 上次這支腳本裝的 oMLX 服務 PID（沒在跑就是空字串）
service_pid() {
  # 服務不存在時 launchctl print 回 113；配上 pipefail + set -e 會讓整支腳本無聲退出
  { launchctl print "$GUI_DOMAIN/$OMLX_LABEL" 2>/dev/null || true; } \
    | awk '$1=="pid" && $2=="=" {print $3; exit}'
}

load_cargo_env() {
  if [[ -f "$HOME/.cargo/env" ]]; then
    # shellcheck source=/dev/null
    source "$HOME/.cargo/env"
  fi
}

load_uv_path() {
  if ! command -v uv >/dev/null 2>&1 && [[ -x "$HOME/.local/bin/uv" ]]; then
    export PATH="$HOME/.local/bin:$PATH"
  fi
}

rust_version() { { rustc --version 2>/dev/null || echo "rustc 0"; } | awk '{print $2}' | sed 's/-.*//'; }

# ─── 安裝 log：終端機照常顯示，同時存一份檔案 ────────────────────────
# 用 fifo + 背景 tee，不用 >(…)：bash 3.2 拿不到 >(…) 的 PID，結束時沒辦法等它寫完。
INSTALL_LOG=""
TEE_PID=""

start_install_log() {
  mkdir -p "$LOG_DIR"
  INSTALL_LOG="$LOG_DIR/install-$(date +%Y%m%d-%H%M%S).log"
  local fifo="$tmp_dir/log.fifo"
  mkfifo "$fifo"
  tee -a "$INSTALL_LOG" <"$fifo" &
  TEE_PID=$!
  exec 3>&1 4>&2 >"$fifo" 2>&1
  echo "install-with-omlx.sh 開始：$(date '+%Y-%m-%d %H:%M:%S %z')"
  echo "腳本 SHA-256：$(shasum -a 256 "$0" | awk '{print substr($1, 1, 12)}')　目前目錄：$PWD"
}

# 進度條靠 \r 覆寫同一行，存進檔案會變成一整串；每行只留最後一段，並去掉顏色碼
clean_log() {
  LC_ALL=C awk -F'\r' '{
    gsub(/\033\[[0-9;?]*[A-Za-z]/, "")
    for (i = NF; i > 1; i--) if ($i != "") break
    print $i
  }' "$1" >"$1.tmp" && mv "$1.tmp" "$1"
}

on_exit() {
  local rc=$?
  trap '' PIPE INT TERM   # 收尾時再按一次 Ctrl-C 不會打斷，fd 一定會還原
  if [[ -n "$TEE_PID" ]]; then
    exec 1>&3 2>&4 3>&- 4>&-   # 先把終端機還回來，關掉 fifo 的寫入端，tee 讀到 EOF 才會結束
    # 萬一有背景常駐程式繼承了寫入端，tee 永遠等不到 EOF；最多等 5 秒
    local i
    for i in 1 2 3 4 5 6 7 8 9 10; do
      kill -0 "$TEE_PID" 2>/dev/null || break
      sleep 0.5
    done
    kill "$TEE_PID" 2>/dev/null || true
    wait "$TEE_PID" 2>/dev/null || true
    clean_log "$INSTALL_LOG" 2>/dev/null || true
    # 這段直接寫終端機和檔案，不經過 tee：Ctrl-C 或 tee 出事時也一定看得到
    if (( rc != 0 )); then
      local msg
      msg="$(printf '\n安裝沒有完成（exit %s）。完整的安裝 log：\n    %s\n請把這個檔案傳給提供這支腳本的人。' "$rc" "$INSTALL_LOG")"
      printf '%s\n' "$msg" >>"$INSTALL_LOG" 2>/dev/null || true
      printf '%s\n' "$msg" >&2
    fi
  fi
  rm -rf "${tmp_dir:-}"
  exit "$rc"
}

# ─── 1. 前置檢查：全部在安裝任何東西之前 ─────────────────────────────
# 只比對 .app bundle 裡的程式，不會動到 ~/.mur/omlx 底下我們自己的服務。
omlx_app_pids() { pgrep -f '/oMLX\.app/Contents/' 2>/dev/null | tr '\n' ' ' | sed 's/ *$//' || true; }

stop_omlx_app() {
  local pids
  pids="$(omlx_app_pids)"
  if [[ -z "$pids" ]]; then
    info "oMLX.app：沒有開著"
    return 0
  fi
  # 方案 C：app 改用別的 port（例如 8001）就讓它繼續開著，只有占住 ${OMLX_PORT} 才關。
  local holders p on_port=0
  holders="$(lsof -nP -tiTCP:"$OMLX_PORT" -sTCP:LISTEN 2>/dev/null | sort -u | tr '\n' ' ' || true)"
  for p in $pids; do [[ " $holders " == *" $p "* ]] && on_port=1; done
  # 設定檔還寫著 ${OMLX_PORT} 的話也要關：app 開著時改設定檔會被它寫回去。
  [[ "$(omlx_app_settings_port)" == "$OMLX_PORT" ]] && on_port=1
  if [[ "$on_port" == 0 ]]; then
    info "oMLX.app：開著（pid ${pids}），但沒占 port ${OMLX_PORT}，不用關"
    return 0
  fi
  if [[ "$CHECK_ONLY" == 1 ]]; then
    info "oMLX.app：開著（pid ${pids}），正式安裝時會把它關掉"
    OMLX_APP_WILL_STOP=1
    return 0
  fi
  info "oMLX.app：開著（pid ${pids}），先把它關掉"
  osascript -e 'tell application "oMLX" to quit' >/dev/null 2>&1 || true
  local i
  for i in {1..10}; do
    [[ -z "$(omlx_app_pids)" ]] && break
    sleep 1
  done
  local how="quit"
  pids="$(omlx_app_pids)"
  if [[ -n "$pids" ]]; then
    how="kill"
    kill $pids 2>/dev/null || true
    sleep 2
    pids="$(omlx_app_pids)"
    [[ -n "$pids" ]] && kill -9 $pids 2>/dev/null || true
    sleep 1
  fi
  [[ -z "$(omlx_app_pids)" ]] || die "關不掉 oMLX.app（pid $(omlx_app_pids)），請從選單列手動結束再重跑。"
  info "oMLX.app：已關閉（方式：${how}）"
  OMLX_APP_WAS_STOPPED=1
}

omlx_app_settings_port() {
  [[ -f "$OMLX_APP_SETTINGS" ]] || return 0
  jq -r '.server.port // empty' "$OMLX_APP_SETTINGS" 2>/dev/null || true
}

# 方案 C：把 oMLX.app 的 port 從 ${OMLX_PORT} 改成 ${OMLX_APP_PORT}，重開機才不會互搶。
# 一定要在 app 關著時改，不然 app 結束時會把舊值寫回去。
move_omlx_app_port() {
  local cur
  cur="$(omlx_app_settings_port)"
  if [[ -z "$cur" ]]; then
    info "oMLX.app 設定檔：沒有（${OMLX_APP_SETTINGS}），不用改"
    return 0
  fi
  if [[ "$cur" != "$OMLX_PORT" ]]; then
    info "oMLX.app 設定檔：port ${cur}，沒占 ${OMLX_PORT}，不用改"
    return 0
  fi
  if [[ "$CHECK_ONLY" == 1 ]]; then
    info "oMLX.app 設定檔：port ${cur}，正式安裝時會改成 ${OMLX_APP_PORT}"
    return 0
  fi
  [[ -z "$(omlx_app_pids)" ]] || die "oMLX.app 還開著，不能改它的設定檔。請先結束 app 再重跑。"
  local backup tmp
  backup="${OMLX_APP_SETTINGS}.bak-$(date +%Y%m%d-%H%M%S)"
  cp -p "$OMLX_APP_SETTINGS" "$backup"
  tmp="$(mktemp "${OMLX_APP_SETTINGS}.XXXXXX")"
  jq --argjson port "$OMLX_APP_PORT" '.server.port = $port' "$OMLX_APP_SETTINGS" > "$tmp" \
    && [[ "$(jq -r '.server.port' "$tmp")" == "$OMLX_APP_PORT" ]] \
    || { rm -f "$tmp"; die "改不了 ${OMLX_APP_SETTINGS}，原檔沒動。"; }
  chmod "$(stat -f '%Lp' "$OMLX_APP_SETTINGS")" "$tmp"
  mv "$tmp" "$OMLX_APP_SETTINGS"
  info "oMLX.app 設定檔：port ${OMLX_PORT} → ${OMLX_APP_PORT}（備份：${backup}）"
  if [[ "${OMLX_APP_WAS_STOPPED:-0}" == 1 ]]; then
    open -a oMLX >/dev/null 2>&1 && info "oMLX.app：已重新打開（改用 port ${OMLX_APP_PORT}）" || true
  fi
}

preflight() {
  log "檢查環境（這一步不會安裝任何東西）"

  [[ "$(uname -s)" == Darwin ]] || die "只支援 macOS。oMLX 需要 Apple Silicon + macOS 15 以上。"

  local arm64 translated
  arm64="$(sysctl -n hw.optional.arm64 2>/dev/null || true)"
  translated="$(sysctl -n sysctl.proc_translated 2>/dev/null || true)"
  if [[ "$translated" == 1 ]]; then
    die "這個終端機是透過 Rosetta 執行的，請改用原生（arm64）終端機再跑一次。"
  fi
  if [[ "$arm64" != 1 && "$(uname -m)" != arm64 ]]; then
    die "oMLX 只支援 Apple Silicon，這台是 $(uname -m)。"
  fi
  info "Apple Silicon：OK"

  local macos
  macos="$(sw_vers -productVersion)"
  version_ge "$macos" 15.0 || die "oMLX 需要 macOS 15 以上，這台是 ${macos}。"
  info "macOS ${macos}：OK"

  if ! xcode-select -p >/dev/null 2>&1; then
    if [[ "$CHECK_ONLY" == 1 ]]; then
      die "缺 Xcode Command Line Tools（git、編譯器都靠它）。請執行：xcode-select --install"
    fi
    xcode-select --install >/dev/null 2>&1 || true
    die "缺 Xcode Command Line Tools，已經跳出安裝視窗。裝完之後請再執行一次這支腳本。"
  fi
  info "Xcode Command Line Tools：OK"

  load_cargo_env
  if command -v cargo >/dev/null 2>&1; then
    local have
    have="$(rust_version)"
    if version_ge "$have" "$MIN_RUST"; then
      info "Rust ${have}：OK"
    elif command -v rustup >/dev/null 2>&1; then
      info "Rust $have 太舊（需要 $MIN_RUST+）：稍後會用 rustup update stable 更新"
    else
      die "Rust $have 太舊，這個專案需要 $MIN_RUST 以上，而且這份 Rust 不是 rustup 裝的，請自行更新。"
    fi
  else
    info "Rust：沒有，稍後會用 rustup 官方安裝器安裝"
  fi

  load_uv_path
  if command -v uv >/dev/null 2>&1; then
    info "uv $(uv --version | awk '{print $2}')：OK"
  else
    info "uv：沒有，稍後會用官方安裝器裝到 ~/.local/bin"
  fi

  OMLX_APP_WILL_STOP=0
  stop_omlx_app
  move_omlx_app_port

  if [[ "$OMLX_APP_WILL_STOP" == 1 ]] && port_listening; then
    info "port ${OMLX_PORT}：oMLX.app 在用，正式安裝關掉它之後就會空出來"
  elif port_listening; then
    local ours holders
    ours="$(service_pid)"
    holders="$(lsof -nP -tiTCP:"$OMLX_PORT" -sTCP:LISTEN 2>/dev/null | sort -u | tr '\n' ' ' || true)"
    if [[ -n "$ours" && ( -z "$holders" || " $holders " == *" $ours "* ) ]]; then
      info "port ${OMLX_PORT}：上次安裝的 $OMLX_LABEL 在用，稍後會換成新的"
    else
      {
        echo
        echo "Error: port $OMLX_PORT 已經被別的程式占用，MUR 要用這個 port 連 oMLX，所以先停下來。"
        echo "目前監聽 $OMLX_PORT 的程式："
        lsof -nP -iTCP:"$OMLX_PORT" -sTCP:LISTEN 2>/dev/null || echo "  （查不到，可以自己跑：lsof -nP -iTCP:$OMLX_PORT -sTCP:LISTEN）"
        echo
        echo "如果是 oMLX.app：從選單列把它結束，再重跑這支腳本。"
        echo "到目前為止什麼都還沒安裝（只存了這次的安裝 log）。"
      } >&2
      exit 1
    fi
  else
    info "port ${OMLX_PORT}：沒被占用"
  fi
}

# ─── 2. 補工具 ───────────────────────────────────────────────────────
ensure_rust() {
  load_cargo_env
  if ! command -v cargo >/dev/null 2>&1; then
    log "安裝 Rust（rustup 官方安裝器；會把 ~/.cargo/bin 加進你的 shell 設定）"
    curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
    load_cargo_env
    command -v cargo >/dev/null 2>&1 || die "rustup 裝完了，但找不到 cargo（預期在 ~/.cargo/bin）。"
  fi

  local have
  have="$(rust_version)"
  if ! version_ge "$have" "$MIN_RUST"; then
    log "Rust $have 太舊，更新 stable"
    rustup update stable
    have="$(rust_version)"
    version_ge "$have" "$MIN_RUST" \
      || die "更新後 Rust 還是 ${have}（需要 $MIN_RUST+）。預設 toolchain 可能不是 stable：rustup default stable"
  fi
}

ensure_uv() {
  load_uv_path
  if ! command -v uv >/dev/null 2>&1; then
    log "安裝 uv（官方安裝器，裝到 ~/.local/bin）"
    curl -LsSf https://astral.sh/uv/install.sh | env UV_INSTALL_DIR="$HOME/.local/bin" sh
    export PATH="$HOME/.local/bin:$PATH"
    command -v uv >/dev/null 2>&1 || die "uv 裝完了，但找不到執行檔（預期在 ~/.local/bin/uv）。"
  fi
}

# ─── 3. gateway（開壓縮）──────────────────────────────
install_gateway() {
  log "準備 gateway 原始碼：$GATEWAY_SRC"
  if [[ -e "$GATEWAY_SRC" ]]; then
    [[ -d "$GATEWAY_SRC/.git" ]] || die "$GATEWAY_SRC 已存在，但不是 Git repository。"
    git -C "$GATEWAY_SRC" pull --ff-only
  else
    git clone "$REPO_URL" "$GATEWAY_SRC"
  fi

  log "編譯並安裝 gateway（--compress）"
  (cd "$GATEWAY_SRC" && ./scripts/setup.sh -- --compress)
}

# ─── 4. oMLX（release wheel，獨立 venv）─────────────────────────────
install_omlx() {
  log "安裝 oMLX $OMLX_VERSION 到 $OMLX_VENV"
  local wheel="$tmp_dir/$OMLX_WHEEL_NAME" got
  curl -fL --retry 3 -o "$wheel" "$OMLX_WHEEL_URL"
  got="$(shasum -a 256 "$wheel" | awk '{print $1}')"
  [[ "$got" == "$OMLX_WHEEL_SHA256" ]] \
    || die "oMLX wheel 的 SHA-256 不符（拿到 ${got}），不安裝。"

  mkdir -p "$OMLX_HOME"
  if [[ ! -x "$OMLX_VENV/bin/python" ]]; then
    uv venv --python "$OMLX_PYTHON" "$OMLX_VENV"
  fi
  uv pip install --python "$OMLX_VENV/bin/python" "$wheel"
  [[ -x "$OMLX_BIN" ]] || die "安裝完成，但找不到 ${OMLX_BIN}。"
}

download_model() {
  log "下載模型 $MODEL_ID"
  [[ -x "$OMLX_VENV/bin/hf" ]] || die "找不到 $OMLX_VENV/bin/hf（應該隨 oMLX 的 huggingface-hub 一起裝好）。"
  mkdir -p "$MODEL_DIR"
  "$OMLX_VENV/bin/hf" download "$MODEL_ID" --local-dir "$MODEL_DIR"
}

# ─── 5. LaunchAgent 常駐 ─────────────────────────────────────────────
# 用獨立的 base path，所以不會改到 oMLX.app 的 ~/.omlx/settings.json，
# 也不會沿用它的 API key：oMLX 在沒設定 key 時不驗證請求。
# 例外：oMLX 每次啟動都會改寫 ~/.omlx/bin/omlx-cluster-python（cluster 功能用，
# omlx/cluster/worker_shim.py），指向最後啟動的那一份。移除指令只在它指向這裡時才刪。
write_launch_agent() {
  log "設定開機自動啟動：$LAUNCH_AGENT"
  mkdir -p "$(dirname "$LAUNCH_AGENT")" "$LOG_DIR"
  cat > "$LAUNCH_AGENT" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>$OMLX_LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$OMLX_BIN</string>
    <string>serve</string>
    <string>--base-path</string>
    <string>$OMLX_HOME</string>
    <string>--model-dir</string>
    <string>$OMLX_HOME/models</string>
    <string>--host</string>
    <string>$OMLX_HOST</string>
    <string>--port</string>
    <string>$OMLX_PORT</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>OMLX_BASE_PATH</key>
    <string>$OMLX_HOME</string>
    <key>PATH</key>
    <string>$OMLX_VENV/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>30</integer>
  <key>StandardOutPath</key>
  <string>$LOG_DIR/server.log</string>
  <key>StandardErrorPath</key>
  <string>$LOG_DIR/server.error.log</string>
</dict>
</plist>
EOF
  plutil -lint "$LAUNCH_AGENT" >/dev/null || die "產生的 plist 格式不對：$LAUNCH_AGENT"
}

restart_service() {
  local i
  if launchctl print "$GUI_DOMAIN/$OMLX_LABEL" >/dev/null 2>&1; then
    launchctl bootout "$GUI_DOMAIN/$OMLX_LABEL" 2>/dev/null || true
    for i in $(seq 1 20); do
      launchctl print "$GUI_DOMAIN/$OMLX_LABEL" >/dev/null 2>&1 || break
      sleep 0.5
    done
  fi
  for i in $(seq 1 20); do
    port_listening || break
    sleep 0.5
  done
  if port_listening; then
    die "port $OMLX_PORT 在安裝過程中被別的程式占走了（lsof -nP -iTCP:$OMLX_PORT -sTCP:LISTEN 看是誰）。"
  fi
  launchctl enable "$GUI_DOMAIN/$OMLX_LABEL" 2>/dev/null || true
  launchctl bootstrap "$GUI_DOMAIN" "$LAUNCH_AGENT"
}

# ─── 6. 確認真的能用才宣告成功 ───────────────────────────────────────
fail_with_logs() {
  {
    echo
    echo "Error: $1"
    echo "--- $LOG_DIR/server.error.log（最後 30 行）"
    tail -n 30 "$LOG_DIR/server.error.log" 2>/dev/null || true
    echo "--- $LOG_DIR/server.log（最後 30 行）"
    tail -n 30 "$LOG_DIR/server.log" 2>/dev/null || true
  } >&2
  exit 1
}

verify_omlx() {
  log "等 oMLX 起來並確認 embedding 可以用（不帶 API key）"
  local base="http://$OMLX_HOST:$OMLX_PORT/v1" models="" model_id="" i
  for i in $(seq 1 90); do
    models="$(curl -fsS --max-time 3 "$base/models" 2>/dev/null || true)"
    if [[ "$models" == *"$MODEL_NAME"* ]]; then break; fi
    sleep 2
  done
  [[ "$models" == *"$MODEL_NAME"* ]] \
    || fail_with_logs "3 分鐘內 $base/models 沒有列出 ${MODEL_NAME}。"

  model_id="$(printf '%s' "$models" | "$OMLX_VENV/bin/python" -c '
import json, sys
name = sys.argv[1]
ids = [m.get("id", "") for m in json.load(sys.stdin).get("data", [])]
print(next((i for i in ids if name in i), ""))
' "$MODEL_NAME")"
  [[ -n "$model_id" ]] || fail_with_logs "看不懂 $base/models 的回應：$models"

  local resp
  resp="$(curl -fsS --max-time 180 "$base/embeddings" \
            -H 'Content-Type: application/json' \
            -d "{\"model\":\"$model_id\",\"input\":\"hello\"}" 2>&1)" \
    || fail_with_logs "embedding 請求失敗：$resp"
  [[ "$resp" == *'"embedding"'* ]] || fail_with_logs "embedding 回應裡沒有向量：${resp:0:300}"
  MODEL_SERVED_AS="$model_id"
}

summary() {
  # setup.sh 在安裝當下確認過 8088；oMLX 那段跑了好幾分鐘，這裡再看一次。
  local gw_state="在聽"
  /usr/bin/nc -z -G 1 "$GATEWAY_HOST" "$GATEWAY_PORT" >/dev/null 2>&1 \
    || gw_state="現在沒在聽！看 $GATEWAY_LOG_DIR/proxy.log"

  # 已經設好就不叫人再加一次；註解掉的、port 不一樣的（:80889）都不算。
  local rc_file="" f url_re="${GATEWAY_URL//./\\.}"
  for f in "$HOME/.zshenv" "$HOME/.zshrc"; do
    if grep -qsE "^[[:space:]]*(export[[:space:]]+)?ANTHROPIC_BASE_URL=[\"']?$url_re/?([\"'[:space:];#]|\$)" "$f"; then
      rc_file="$f"
      break
    fi
  done
  # 移除說明要指到實際設定的那個檔；還沒設的話就是我們叫他加的 ~/.zshenv。
  local rc_hint="~/.zshenv"
  [[ -n "$rc_file" ]] && rc_hint="~${rc_file#"$HOME"}"

  cat <<EOF

==> 完成
    gateway：${GATEWAY_URL}（${gw_state}）
    oMLX：http://$OMLX_HOST:$OMLX_PORT/v1（不需要 API key，已實際打過 /v1/embeddings）
    模型：$MODEL_SERVED_AS
    安裝 log：$INSTALL_LOG

==> 還差一步：讓 Claude Code 走 gateway
EOF
  if [[ -n "$rc_file" ]]; then
    echo "    ~${rc_file#"$HOME"} 已經有 ANTHROPIC_BASE_URL=${GATEWAY_URL}，這步可以跳過。"
  else
    cat <<EOF
    把這行加進 ~/.zshenv，再開一個新的終端機：
      export ANTHROPIC_BASE_URL="$GATEWAY_URL"
EOF
  fi
  cat <<EOF
    確認：curl -s $GATEWAY_URL/__mur/health
      claudeCredential 要是 "oauth"。第一次會跳鑰匙圈視窗，請按「永遠允許」。
      是 "missing" 的話：沒登入過 Claude Code 就先跑 claude auth login；按了拒絕就再跑一次上面的 curl。

==> 位置
    gateway 程式：$GATEWAY_BIN
    gateway log：$GATEWAY_LOG_DIR/proxy.log
    oMLX 資料：$OMLX_HOME
    oMLX log：$LOG_DIR/server.log、server.error.log
    停止 gateway：launchctl bootout $GUI_DOMAIN/$GATEWAY_LABEL
    停止 oMLX：launchctl bootout $GUI_DOMAIN/$OMLX_LABEL

    移除 gateway（先把 ${rc_hint} 裡的 ANTHROPIC_BASE_URL 刪掉，不然 Claude Code 會連不上）：
      launchctl bootout $GUI_DOMAIN/$GATEWAY_LABEL
      rm "$GATEWAY_PLIST" "$GATEWAY_BIN"
      rm -rf "$GATEWAY_LOG_DIR"
EOF
  # 在 repo 裡執行、或用 MUR_GATEWAY_DIR 指定的目錄，不是這支腳本 clone 的，不叫人刪。
  if [[ "$GATEWAY_SRC_OWNED" == 1 ]]; then
    echo "      rm -rf \"$GATEWAY_SRC\""
  fi
  cat <<EOF

    移除 oMLX（四行照順序貼；第三行只在 cluster shim 指向這裡時才刪，不會動到 oMLX.app 的）：
      launchctl bootout $GUI_DOMAIN/$OMLX_LABEL
      rm "$LAUNCH_AGENT"
      grep -qF "$OMLX_VENV/" "\$HOME/.omlx/bin/omlx-cluster-python" 2>/dev/null && rm "\$HOME/.omlx/bin/omlx-cluster-python"
      rm -rf "$OMLX_HOME"

接下來安裝 MUR：
    curl -fsSL https://mur.run/install.sh | sh
EOF
}

# ─── main ────────────────────────────────────────────────────────────
if [[ "$CHECK_ONLY" == 1 ]]; then
  preflight
  log "檢查通過（--check：沒有安裝任何東西，也沒有存 log）"
  exit 0
fi

tmp_dir="$(mktemp -d)"
trap on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
MODEL_SERVED_AS=""

start_install_log   # 在 preflight 之前開始：被擋下來的原因也要留在 log 裡
preflight
ensure_rust
ensure_uv
install_gateway
install_omlx
download_model
write_launch_agent
restart_service
verify_omlx
summary
