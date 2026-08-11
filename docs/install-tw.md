# mur-model-gateway 安裝指南

`mur-model-gateway install` 會寫出平台對應的 service 描述檔，並把設定烘進環境變數。
Runtime 只讀環境變數（`MUR_MODEL_GATEWAY_TOKEN_SOURCE*` / `MUR_MODEL_GATEWAY_BIND` / `MUR_MODEL_GATEWAY_UPSTREAM*` /
`MUR_MODEL_GATEWAY_COMPRESS`），沒有 config 檔。

## 快速開始

```bash
./scripts/auto.sh             # 全自動：偵測 GLIBC(→musl)、headless(→--system)、token 來源，裝好啟動
./scripts/setup.sh            # build → 裝到 ~/.local/bin → 註冊 user service → 啟動
./scripts/setup.sh --no-service   # 只裝 binary
./scripts/setup.sh --uninstall    # 移除 service（binary 保留）
```

`setup.sh` 在 `--` 之後的參數會原封不動傳給 `mur-model-gateway install`：

```bash
./scripts/setup.sh -- --token-source file --bind 127.0.0.1:9099
```

## install 旗標

| 旗標 | 效果 |
|------|------|
| `--token-source <spec>` | 烘入 `MUR_MODEL_GATEWAY_TOKEN_SOURCE`（見下） |
| `--token-source-codex <spec>` | 烘入 `MUR_MODEL_GATEWAY_TOKEN_SOURCE_CODEX` — `/v1/responses` 路由的憑證來源（見下） |
| `--bind <addr>` | 烘入 `MUR_MODEL_GATEWAY_BIND`（預設 `127.0.0.1:8088`） |
| `--upstream <url>` | 烘入 `MUR_MODEL_GATEWAY_UPSTREAM` |
| `--compress` / `--no-compress` | `MUR_MODEL_GATEWAY_COMPRESS=1` 開關（無旗標時嗅探環境變數） |
| `--system` | Linux 限定：system-level unit（見下） |

值裡不允許空白與 `<>"&`（會被直接拼進 plist/unit/cmd，install 時就擋下）。

## Token 來源（`--token-source`）

| spec | 行為 |
|------|------|
| `keychain`（預設） | 讀 OS keychain 的 `Claude Code-credentials`。**非 macOS 上自動 fallback 到 `~/.claude/.credentials.json`**（Linux/Windows 的 Claude Code 寫檔不寫 keychain），所以有登入 Claude Code 的機器零設定即可用 |
| `file` | 讀 `~/.claude/.credentials.json` |
| `file:/path/to/credentials.json` | 讀指定 JSON（同 `claudeAiOauth.accessToken` 格式） |
| `env:VAR` | 每次請求從環境變數 `VAR` 讀 token（無 Claude Code 的 headless 主機用這個） |
| `off` / `disabled` | 純 passthrough，不做 disguise |

Token 一律每請求重讀，Claude Code 背景刷新後自動生效。

## Codex token 來源（`--token-source-codex`）

| spec | 行為 |
|------|------|
| `codex`（預設） | 讀 `~/.codex/auth.json`，`codex login` 寫的那個檔 |
| `env:VAR` | 每次請求從環境變數 `VAR` 讀 token |
| `off` / `disabled` | `/v1/responses` 純 passthrough，不做 disguise |

這只管 `/v1/responses` 路由 — 上面的 `--token-source` 仍然管 Anthropic/OpenAI/Gemini。
`codex` 本來就是 runtime 預設值，不用加旗標；上游刷新 token 後，新的 access token
會自動寫回 `~/.codex/auth.json`。

**目前沒有 MUR agent 能碰到 `/v1/responses`。** MUR 的 OpenAI client 只會講 Chat
Completions，不會講 Responses API，所以現在這條路由只有本來就講 Responses API
的 client（例如 `curl`、Codex CLI 本身）能用。

## 各平台

### macOS（launchd）

```bash
mur-model-gateway install [flags]
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/run.mur-model-gateway.plist
launchctl enable gui/$(id -u)/run.mur-model-gateway
```

Log：`~/Library/Logs/mur-model-gateway/proxy.log`。

### Linux — user unit（預設）

```bash
mur-model-gateway install [flags]
systemctl --user daemon-reload
systemctl --user enable --now mur-model-gateway.service
```

⚠️ user unit 只在你登入時跑；headless / 開機自啟要嘛
`loginctl enable-linger $USER`，要嘛用 `--system`。

### Linux — system unit（`--system`，headless 伺服器）

```bash
sudo mur-model-gateway install --system --token-source env:MUR_MODEL_GATEWAY_OAUTH_TOKEN
# 然後自己把 token 補進 env 檔（不要經過 shell history / 工具輸出）：
#   sudoedit /etc/mur-model-gateway.env   → 加一行 MUR_MODEL_GATEWAY_OAUTH_TOKEN=sk-ant-oat01-…
sudo systemctl daemon-reload
sudo systemctl enable --now mur-model-gateway.service
journalctl -u mur-model-gateway.service -f
```

產物：
- `/etc/systemd/system/mur-model-gateway.service` — `User=<執行 install 的使用者>`、
  `EnvironmentFile=/etc/mur-model-gateway.env`、`WantedBy=multi-user.target`（開機即啟動，不需登入）
- `/etc/mur-model-gateway.env` — root-owned、mode 600，所有環境變數（含 secret）都放這裡

`setup.sh --system -- <flags>` 會自動 sudo 完成上述流程。

### Windows（Task Scheduler）

```powershell
mur-model-gateway install [flags]
# install 會印出可直接貼上的指令（elevated prompt）：
schtasks /Create /F /SC ONLOGON /TN mur-model-gateway /TR "\"C:\Users\you\AppData\Local\mur-model-gateway\mur-model-gateway.cmd\""
schtasks /Run /TN mur-model-gateway
```

`.cmd` 內含所有 `set` 環境變數行，輸出導到
`%LOCALAPPDATA%\mur-model-gateway\logs\proxy.log`。`/F` 讓重複安裝直接覆蓋。

## 舊 GLIBC 主機（如 Ubuntu 20.04 / GLIBC 2.31）

動態連結的 release build 需要 GLIBC ≥2.34，在舊系統會直接起不來
（`version 'GLIBC_2.34' not found`）。改用靜態 musl build：

```bash
./scripts/setup.sh --musl [--system] [-- <install flags>]
```

本機沒 musl 工具鏈時，照 script 印出的提示用 Docker build：

```bash
docker run --rm -v "$PWD":/src -w /src rust:1.91-bookworm bash -c \
  'rustup target add x86_64-unknown-linux-musl && apt-get update && \
   apt-get install -y musl-tools && cargo build --release --target x86_64-unknown-linux-musl'
```

注意：mur-model-gateway 是 edition 2024，需要 Rust ≥1.85（建議 rust:1.91 以上的 image）。

## 解除安裝 / 狀態

```bash
mur-model-gateway status      # 列出 binary、user/system service 檔與 env 檔是否存在
mur-model-gateway uninstall   # 移除 user + system 兩邊的 service/env 檔（/etc 需 sudo，會印提示）
```

## 疑難排解

- **systemd restart-storm、`Address in use (os error 98)`** — 有殘留的 mur-model-gateway
  process 佔著 port：`ss -ltnp | grep 8088` 找到後 kill 再 start。
- **上游回 404 `not_found_error: model: …`** — 這是 Anthropic 本尊的回應，
  代表**認證已成功**，只是 model id 過期；不是 proxy 路由問題。
- **接入應用** — 應用端設 `ANTHROPIC_BASE_URL=http://127.0.0.1:8088` 即可；
  帶 OAuth 形狀 key 或不帶 auth 的請求會走 disguise，帶正常 `sk-ant-api03-*`
  的請求原樣 passthrough。
