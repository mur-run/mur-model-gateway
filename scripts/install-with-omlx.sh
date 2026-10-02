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
#   MUR_OMLX_HOME     UV 版 oMLX 的 venv、設定、log（預設 ~/.mur/omlx）
#   MUR_MODEL_DIR     共用模型庫，UV 版和 oMLX.app 都讀這裡（預設 ~/.omlx/models，就是 oMLX.app 的預設）
#   MUR_OMLX_MODE     uv 或 app：有裝 oMLX.app 時不用問，直接用這一版（沒有終端機可以問時必填）
set -euo pipefail

REPO_URL="https://github.com/mur-run/mur-model-gateway.git"

# oMLX 版本：安裝時自動抓 GitHub 上最新的「正式版」（tag 只能是 vX.Y.Z）。
# 作者會把 rc 版標成非 prerelease，所以不用 /releases/latest，也不看 prerelease 標記。
# SHA-256 用 release asset 公布的 digest 比對。查不到（沒網路、API 限流、沒 jq）就退回下面的備用版本。
# MUR_OMLX_VERSION=X.Y.Z 可以釘死某一版（仍然從 API 拿 digest 驗證）。
OMLX_REPO="jundot/omlx"
OMLX_FALLBACK_VERSION="0.6.4"
OMLX_FALLBACK_SHA256="f13d92900bf6c7e925e9a6d5525b4465c615c404ee796d328ffc5d4d379ddb0b"
OMLX_VERSION="$OMLX_FALLBACK_VERSION"
OMLX_WHEEL_NAME="" OMLX_WHEEL_URL="" OMLX_WHEEL_SHA256=""
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
# 共用模型庫：UV 版用 --model-dir 指到這裡，oMLX.app 的 model_dirs 也會把它排第一。
SHARED_MODEL_DIR="${MUR_MODEL_DIR:-$HOME/.omlx/models}"
MODEL_DIR="$SHARED_MODEL_DIR/$MODEL_NAME"
OLD_MODEL_DIR="$OMLX_HOME/models/$MODEL_NAME"   # 改共用之前 UV 版放模型的地方
OMLX_APP_LOG_DIR="$HOME/.omlx/logs"
OMLX_MODE=""                 # uv 或 app，preflight 決定
OMLX_APP_BUNDLE=""
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
  -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d'; exit 0 ;;
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
  # 設定檔要改（port 或模型庫）的話也要關：app 開著時改設定檔會被它寫回去。
  omlx_app_settings_need_change "$OMLX_APP_PORT" 0 && on_port=1
  if [[ "$on_port" == 0 ]]; then
    info "oMLX.app：開著（pid ${pids}），沒占 port ${OMLX_PORT}、設定也不用改，不用關"
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
    # shellcheck disable=SC2086 # $pids 是多個 PID，需要分詞
    kill $pids 2>/dev/null || true
    sleep 2
    pids="$(omlx_app_pids)"
    if [[ -n "$pids" ]]; then
      # shellcheck disable=SC2086 # $pids 是多個 PID，需要分詞
      kill -9 $pids 2>/dev/null || true
    fi
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

# 要寫進 oMLX.app 設定檔的內容：port、共用模型庫排第一（app 下載的模型也會放這裡）；
# $2=1 時再打開「app 啟動就開 server」（APP 版要靠它提供服務）。
omlx_app_settings_filter() {
  jq --argjson port "$1" --argjson auto "$2" --arg dir "$SHARED_MODEL_DIR" '
    .server.port = $port
    | .model.model_dirs = ([$dir] + ((.model.model_dirs // []) - [$dir]))
    | .model.model_dir = $dir
    | if $auto == 1 then .server.auto_start_on_launch = true else . end
  ' "$OMLX_APP_SETTINGS"
}

omlx_app_settings_need_change() {
  [[ -f "$OMLX_APP_SETTINGS" ]] || return 1
  [[ "$(jq -S . "$OMLX_APP_SETTINGS")" != "$(omlx_app_settings_filter "$1" "$2" | jq -S .)" ]]
}

# 改 oMLX.app 的設定檔。一定要在 app 關著時改，不然 app 結束時會把舊值寫回去。
# UV 版：port 讓給 UV 版，改成 ${OMLX_APP_PORT}；APP 版：port 用 ${OMLX_PORT}。兩版都共用模型庫。
update_omlx_app_settings() {
  local port="$1" auto="$2"
  if [[ ! -f "$OMLX_APP_SETTINGS" ]]; then
    info "oMLX.app 設定檔：沒有（${OMLX_APP_SETTINGS}），不用改"
    return 0
  fi
  if ! omlx_app_settings_need_change "$port" "$auto"; then
    info "oMLX.app 設定檔：port ${port}、模型庫 ${SHARED_MODEL_DIR}，不用改"
    return 0
  fi
  if [[ "$CHECK_ONLY" == 1 ]]; then
    info "oMLX.app 設定檔：正式安裝時會改成 port ${port}、模型庫 ${SHARED_MODEL_DIR} 排第一"
    return 0
  fi
  if [[ -n "$(omlx_app_pids)" ]]; then
    if [[ "$OMLX_MODE" == app ]]; then
      info "oMLX.app 設定檔：app 開著，等安裝後段關掉 app 再改"
      return 0
    fi
    die "oMLX.app 還開著，不能改它的設定檔。請先結束 app 再重跑。"
  fi
  local backup tmp
  backup="${OMLX_APP_SETTINGS}.bak-$(date +%Y%m%d-%H%M%S)"
  cp -p "$OMLX_APP_SETTINGS" "$backup"
  tmp="$(mktemp "${OMLX_APP_SETTINGS}.XXXXXX")"
  if ! { omlx_app_settings_filter "$port" "$auto" > "$tmp" \
      && [[ "$(jq -r '.server.port' "$tmp")" == "$port" ]] \
      && [[ "$(jq -r '.model.model_dirs[0]' "$tmp")" == "$SHARED_MODEL_DIR" ]]; }; then
    rm -f "$tmp"; die "改不了 ${OMLX_APP_SETTINGS}，原檔沒動。"
  fi
  chmod "$(stat -f '%Lp' "$OMLX_APP_SETTINGS")" "$tmp"
  mv "$tmp" "$OMLX_APP_SETTINGS"
  info "oMLX.app 設定檔：port ${port}、模型庫 ${SHARED_MODEL_DIR}（備份：${backup}）"
}

# UV 版專用：port 讓出來之後，原本開著的 app 再打開。
move_omlx_app_port() {
  update_omlx_app_settings "$OMLX_APP_PORT" 0
  if [[ "$CHECK_ONLY" == 0 && "${OMLX_APP_WAS_STOPPED:-0}" == 1 ]]; then
    if open -a oMLX >/dev/null 2>&1; then
      info "oMLX.app：已重新打開（改用 port ${OMLX_APP_PORT}）"
    fi
  fi
}

find_omlx_app() {
  local d
  for d in /Applications/oMLX.app "$HOME/Applications/oMLX.app"; do
    if [[ -d "$d" ]]; then echo "$d"; return 0; fi
  done
  return 0
}

print_mode_choices() {
  cat <<EOF
    偵測到 oMLX.app（${OMLX_APP_BUNDLE}）。MUR 要一個 oMLX 在 ${OMLX_HOST}:${OMLX_PORT} 提供 embedding，請選一個：

    [1] UV 版（建議）：另外裝到 ${OMLX_HOME}，由 launchd 常駐
        + 開機就啟動，不用登入桌面、不用開 app；當掉 launchd 會自己重啟
        + 每次安裝自動升到最新正式版（跳過 rc/dev，驗過 SHA-256）
        - 多一份 Python 環境（約 1GB）
        - 沒有圖形介面；oMLX.app 會改用 port ${OMLX_APP_PORT}，兩個一起開會吃兩份記憶體

    [2] APP 版：直接用這個 oMLX.app
        + 不多裝東西，只有一份 oMLX；有圖形介面可以管模型、看狀態
        - 要登入桌面、app 開著才有服務，結束 app，MUR 的 embedding 就斷了
        - app 自動更新，版本不固定
        - 會把 app 的 port 設成 ${OMLX_PORT}、打開「啟動時開 server」，並加進登入項目
        - 如果之前裝過 UV 版服務（${OMLX_LABEL}），會把它停掉

    兩版的模型庫都是 ${SHARED_MODEL_DIR}，下載一次兩邊都看得到。
EOF
}

choose_omlx_mode() {
  OMLX_APP_BUNDLE="$(find_omlx_app)"
  if [[ -z "$OMLX_APP_BUNDLE" ]]; then
    OMLX_MODE=uv
    info "oMLX.app：沒安裝，用 UV 版"
    return 0
  fi
  case "${MUR_OMLX_MODE:-}" in
    uv|app) OMLX_MODE="$MUR_OMLX_MODE"; info "oMLX：用 ${OMLX_MODE} 版（MUR_OMLX_MODE）"; return 0 ;;
    "") ;;
    *) die "MUR_OMLX_MODE 只能是 uv 或 app，拿到：${MUR_OMLX_MODE}" ;;
  esac
  print_mode_choices
  if [[ "$CHECK_ONLY" == 1 ]]; then
    OMLX_MODE=uv
    info "--check：先照 UV 版檢查，正式安裝時會問你（或用 MUR_OMLX_MODE=app 檢查 APP 版）"
    return 0
  fi
  { : </dev/tty; } 2>/dev/null \
    || die "偵測到 oMLX.app，但沒有終端機可以問。請用 MUR_OMLX_MODE=uv 或 MUR_OMLX_MODE=app 指定。"
  local ans
  while :; do
    printf '    要用哪一版？輸入 1 或 2（直接 Enter = 1）：' >/dev/tty
    read -r ans </dev/tty || die "讀不到輸入。請用 MUR_OMLX_MODE=uv 或 MUR_OMLX_MODE=app 指定。"
    case "$ans" in
      ""|1|uv|UV) OMLX_MODE=uv; break ;;
      2|app|APP)  OMLX_MODE=app; break ;;
      *) echo "    請輸入 1 或 2" >/dev/tty ;;
    esac
  done
  info "選擇：${OMLX_MODE} 版"
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

  command -v jq >/dev/null 2>&1 || die "找不到 jq（macOS 15 內建 /usr/bin/jq）。"

  choose_omlx_mode
  OMLX_APP_WILL_STOP=0
  if [[ "$OMLX_MODE" == uv ]]; then
    stop_omlx_app
    move_omlx_app_port
  else
    [[ -f "$OMLX_APP_SETTINGS" ]] \
      || die "找不到 ${OMLX_APP_SETTINGS}：oMLX.app 還沒開過。請先打開一次 oMLX.app 再重跑。"
    update_omlx_app_settings "$OMLX_PORT" 1
  fi

  local app_on_port=0 p listeners
  listeners=" $(lsof -nP -tiTCP:"$OMLX_PORT" -sTCP:LISTEN 2>/dev/null | tr '\n' ' ' || true) "
  for p in $(omlx_app_pids); do
    [[ "$listeners" == *" $p "* ]] && app_on_port=1
  done
  if [[ "$OMLX_APP_WILL_STOP" == 1 ]] && port_listening; then
    info "port ${OMLX_PORT}：oMLX.app 在用，正式安裝關掉它之後就會空出來"
  elif [[ "$OMLX_MODE" == app && "$app_on_port" == 1 ]]; then
    info "port ${OMLX_PORT}：oMLX.app 在用，APP 版就是要它"
  elif port_listening; then
    local ours holders
    ours="$(service_pid)"
    holders="$(lsof -nP -tiTCP:"$OMLX_PORT" -sTCP:LISTEN 2>/dev/null | sort -u | tr '\n' ' ' || true)"
    if [[ -n "$ours" && ( -z "$holders" || " $holders " == *" $ours "* ) ]]; then
      if [[ "$OMLX_MODE" == app ]]; then
        info "port ${OMLX_PORT}：上次安裝的 $OMLX_LABEL 在用，稍後會停掉它，換 oMLX.app"
      else
        info "port ${OMLX_PORT}：上次安裝的 $OMLX_LABEL 在用，稍後會換成新的"
      fi
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
    if [[ -n "$(git -C "$GATEWAY_SRC" status --porcelain --untracked-files=no)" ]]; then
      info "⚠ $GATEWAY_SRC 有未 commit 的修改，略過 git pull，沿用目前的原始碼。"
    else
      git -C "$GATEWAY_SRC" pull --ff-only --no-rebase \
        || info "⚠ git pull 失敗，沿用目前的原始碼繼續安裝。"
    fi
  else
    git clone "$REPO_URL" "$GATEWAY_SRC"
  fi

  log "編譯並安裝 gateway（--compress）"
  (cd "$GATEWAY_SRC" && ./scripts/setup.sh -- --compress)
}

# ─── 4. oMLX（release wheel，獨立 venv）─────────────────────────────
omlx_wheel_name() { echo "omlx-${1}-cp312-cp312-macosx_15_0_universal2.whl"; }

# 決定要裝哪一版 oMLX，設定 OMLX_VERSION / OMLX_WHEEL_NAME / OMLX_WHEEL_URL / OMLX_WHEEL_SHA256。
resolve_omlx_release() {
  local want="${MUR_OMLX_VERSION:-}" json="" ver="" sha=""
  if command -v jq >/dev/null 2>&1; then
    json="$(curl -fsSL -m 15 "https://api.github.com/repos/${OMLX_REPO}/releases?per_page=30" 2>/dev/null || true)"
  fi
  if [[ -n "$json" ]]; then
    if [[ -n "$want" ]]; then
      ver="$want"
    else
      ver="$(jq -r '.[] | select(.draft|not) | .tag_name' <<<"$json" \
        | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | sed 's/^v//' | sort -V | tail -1 || true)"
    fi
    if [[ -n "$ver" ]]; then
      sha="$(jq -r --arg t "v$ver" --arg n "$(omlx_wheel_name "$ver")" \
        '.[] | select(.tag_name==$t) | .assets[] | select(.name==$n) | .digest // empty' <<<"$json" \
        | sed 's/^sha256://' | head -1)"
    fi
  fi
  if [[ -n "$ver" && "$sha" =~ ^[0-9a-f]{64}$ ]]; then
    OMLX_VERSION="$ver" OMLX_WHEEL_SHA256="$sha"
    if [[ -n "$want" ]]; then info "oMLX 版本：${ver}（MUR_OMLX_VERSION 指定）"; else info "oMLX 版本：${ver}（GitHub 最新正式版）"; fi
  elif [[ -n "$want" && "$want" != "$OMLX_FALLBACK_VERSION" ]]; then
    die "找不到 oMLX ${want} 的 wheel 或 SHA-256，不安裝。"
  else
    OMLX_VERSION="$OMLX_FALLBACK_VERSION" OMLX_WHEEL_SHA256="$OMLX_FALLBACK_SHA256"
    info "⚠ 查不到 oMLX 最新版本（網路、API 限流或沒有 jq），改用備用版本 ${OMLX_VERSION}"
  fi
  OMLX_WHEEL_NAME="$(omlx_wheel_name "$OMLX_VERSION")"
  OMLX_WHEEL_URL="https://github.com/${OMLX_REPO}/releases/download/v${OMLX_VERSION}/${OMLX_WHEEL_NAME}"
}

install_omlx() {
  resolve_omlx_release
  local cur=""
  [[ -x "$OMLX_VENV/bin/python" ]] && cur="$(uv pip show --python "$OMLX_VENV/bin/python" omlx 2>/dev/null | awk '/^Version:/{print $2}')"
  if [[ "$cur" == "$OMLX_VERSION" ]]; then
    info "oMLX $OMLX_VERSION 已經是最新，略過下載"
    return 0
  fi
  log "安裝 oMLX $OMLX_VERSION 到 $OMLX_VENV${cur:+（原本 $cur）}"
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
  log "下載模型 $MODEL_ID 到共用模型庫 $SHARED_MODEL_DIR"
  mkdir -p "$SHARED_MODEL_DIR"
  # 以前 UV 版的模型放在自己的目錄；共用模型庫還沒有的話直接搬過去，省得重下載。
  if [[ "$OLD_MODEL_DIR" != "$MODEL_DIR" && -d "$OLD_MODEL_DIR" && ! -e "$MODEL_DIR" ]]; then
    mv "$OLD_MODEL_DIR" "$MODEL_DIR"
    info "已把 $OLD_MODEL_DIR 搬到 $MODEL_DIR"
  fi
  mkdir -p "$MODEL_DIR"
  if [[ "$OMLX_MODE" == uv ]]; then
    [[ -x "$OMLX_VENV/bin/hf" ]] || die "找不到 $OMLX_VENV/bin/hf（應該隨 oMLX 的 huggingface-hub 一起裝好）。"
    "$OMLX_VENV/bin/hf" download "$MODEL_ID" --local-dir "$MODEL_DIR"
  else
    # APP 版沒有我們的 venv，用 uvx 臨時跑 huggingface-hub 的 hf。
    uvx --python "$OMLX_PYTHON" --from huggingface-hub hf download "$MODEL_ID" --local-dir "$MODEL_DIR"
  fi
}

# ─── 5b. APP 版：停掉 UV 版服務，讓 oMLX.app 接手 port 8000 ─────────
retire_uv_service() {
  local i
  if launchctl print "$GUI_DOMAIN/$OMLX_LABEL" >/dev/null 2>&1; then
    log "停掉 UV 版服務 $OMLX_LABEL（改用 oMLX.app）"
    launchctl bootout "$GUI_DOMAIN/$OMLX_LABEL" 2>/dev/null || true
    for i in $(seq 1 20); do
      launchctl print "$GUI_DOMAIN/$OMLX_LABEL" >/dev/null 2>&1 || break
      sleep 0.5
    done
  fi
  if [[ -f "$LAUNCH_AGENT" ]]; then
    rm "$LAUNCH_AGENT"
    info "已刪除 $LAUNCH_AGENT（開機不會再啟動 UV 版）"
  fi
}

start_omlx_app() {
  log "用 oMLX.app 提供 embedding（port ${OMLX_PORT}）"
  # 剛下載的模型要 app 重開才看得到；設定檔也要在 app 關著時改。
  local pids i
  pids="$(omlx_app_pids)"
  if [[ -n "$pids" ]]; then
    info "oMLX.app：開著（pid ${pids}），先關掉再改設定"
    osascript -e 'tell application "oMLX" to quit' >/dev/null 2>&1 || true
    for i in {1..10}; do [[ -z "$(omlx_app_pids)" ]] && break; sleep 1; done
    pids="$(omlx_app_pids)"
    if [[ -n "$pids" ]]; then
      # shellcheck disable=SC2086 # $pids 是多個 PID，需要分詞
      kill $pids 2>/dev/null || true; sleep 2
      pids="$(omlx_app_pids)"
      if [[ -n "$pids" ]]; then
        # shellcheck disable=SC2086 # $pids 是多個 PID，需要分詞
        kill -9 $pids 2>/dev/null || true
      fi
      sleep 1
    fi
    [[ -z "$(omlx_app_pids)" ]] || die "關不掉 oMLX.app，請從選單列手動結束再重跑。"
  fi
  update_omlx_app_settings "$OMLX_PORT" 1
  for i in $(seq 1 20); do
    port_listening || break
    sleep 0.5
  done
  port_listening && die "port $OMLX_PORT 被別的程式占走了（lsof -nP -iTCP:$OMLX_PORT -sTCP:LISTEN 看是誰）。"
  open -a "$OMLX_APP_BUNDLE" || die "打不開 ${OMLX_APP_BUNDLE}。"
  info "oMLX.app：已打開"
  # 登入時自動打開：沒有這個，重開機後 MUR 就沒有 embedding。
  local items
  items="$(osascript -e 'tell application "System Events" to get the name of every login item' 2>/dev/null || true)"
  if [[ "$items" == *oMLX* ]]; then
    info "登入項目：已經有 oMLX"
  elif osascript -e "tell application \"System Events\" to make login item at end with properties {path:\"$OMLX_APP_BUNDLE\", hidden:true}" >/dev/null 2>&1; then
    info "登入項目：已加入 oMLX（登入時自動打開）"
  else
    info "登入項目：加不進去（可能沒給終端機「自動化」權限）。請到「系統設定 → 一般 → 登入項目」手動加入 oMLX.app，不然重開機後 MUR 會沒有 embedding。"
  fi
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
    <string>$SHARED_MODEL_DIR</string>
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
    local d="$LOG_DIR"
    [[ "$OMLX_MODE" == app ]] && d="$OMLX_APP_LOG_DIR"
    echo "--- $d/server.error.log（最後 30 行）"
    tail -n 30 "$d/server.error.log" 2>/dev/null || true
    echo "--- $d/server.log（最後 30 行）"
    tail -n 30 "$d/server.log" 2>/dev/null || true
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

  model_id="$(printf '%s' "$models" | jq -r --arg n "$MODEL_NAME" \
                '[.data[]?.id // empty | select(contains($n))][0] // empty' 2>/dev/null || true)"
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
  # shellcheck disable=SC2088 # 只是顯示給使用者看的字串
  local rc_hint="~/.zshenv"
  # shellcheck disable=SC2088
  [[ -n "$rc_file" ]] && rc_hint="~${rc_file#"$HOME"}"

  cat <<EOF

==> 完成
    gateway：${GATEWAY_URL}（${gw_state}）
    oMLX：http://$OMLX_HOST:$OMLX_PORT/v1（${OMLX_MODE} 版，不帶 API key 實際打過 /v1/embeddings）
    模型庫：${SHARED_MODEL_DIR}（UV 版和 oMLX.app 共用）
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
    停止 gateway：launchctl bootout $GUI_DOMAIN/$GATEWAY_LABEL
EOF
  if [[ "$OMLX_MODE" == uv ]]; then
    cat <<EOF
    oMLX 資料：$OMLX_HOME
    oMLX log：$LOG_DIR/server.log、server.error.log
    停止 oMLX：launchctl bootout $GUI_DOMAIN/$OMLX_LABEL
EOF
  else
    cat <<EOF
    oMLX：${OMLX_APP_BUNDLE}（結束 app，MUR 的 embedding 就會斷）
    oMLX log：$OMLX_APP_LOG_DIR/server.log
EOF
    if [[ -d "$OMLX_HOME/venv" ]]; then
      echo "    以前的 UV 版 venv 還在，用不到了可以刪：rm -rf \"$OMLX_HOME/venv\""
    fi
  fi
  if [[ "$OLD_MODEL_DIR" != "$MODEL_DIR" && -d "$OLD_MODEL_DIR" ]]; then
    echo "    舊的模型副本還在，共用模型庫已經有一份，可以刪：rm -rf \"$OLD_MODEL_DIR\""
  fi
  cat <<EOF

    移除 gateway（先把 ${rc_hint} 裡的 ANTHROPIC_BASE_URL 刪掉，不然 Claude Code 會連不上）：
      launchctl bootout $GUI_DOMAIN/$GATEWAY_LABEL
      rm "$GATEWAY_PLIST" "$GATEWAY_BIN"
      rm -rf "$GATEWAY_LOG_DIR"
EOF
  # 在 repo 裡執行、或用 MUR_GATEWAY_DIR 指定的目錄，不是這支腳本 clone 的，不叫人刪。
  if [[ "$GATEWAY_SRC_OWNED" == 1 ]]; then
    echo "      rm -rf \"$GATEWAY_SRC\""
  fi
  if [[ "$OMLX_MODE" == uv ]]; then
  cat <<EOF

    移除 oMLX（四行照順序貼；第三行只在 cluster shim 指向這裡時才刪，不會動到 oMLX.app 的）：
      launchctl bootout $GUI_DOMAIN/$OMLX_LABEL
      rm "$LAUNCH_AGENT"
      grep -qF "$OMLX_VENV/" "\$HOME/.omlx/bin/omlx-cluster-python" 2>/dev/null && rm "\$HOME/.omlx/bin/omlx-cluster-python"
      rm -rf "$OMLX_HOME"
    （共用模型庫 $SHARED_MODEL_DIR 不會刪，oMLX.app 也在用）
EOF
  fi
  cat <<EOF

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
if [[ "$OMLX_MODE" == uv ]]; then
  install_omlx
  download_model
  write_launch_agent
  restart_service
else
  retire_uv_service
  download_model
  start_omlx_app
fi
verify_omlx
summary
