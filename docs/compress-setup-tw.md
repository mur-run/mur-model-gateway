# 啓用 proxy 壓縮(MUR_MODEL_GATEWAY_COMPRESS)

wire-level tool_result 壓縮的安裝設定。設計背景見
[specs/2026-07-03-mur-compress-design.md](specs/2026-07-03-mur-compress-design.md)。
在轉譯過的 Codex chat 路徑（`/codex/v1/chat/completions`）上，壓縮在*轉譯之前*
執行，所以 rewriter 看到的是它懂的 Chat Completions body。

## install 參數

```
mur-model-gateway install --compress      # 服務定義寫入 MUR_MODEL_GATEWAY_COMPRESS=1(啓用)
mur-model-gateway install --no-compress   # 強制關閉,即使環境有 MUR_MODEL_GATEWAY_COMPRESS=1
mur-model-gateway install                 # 不帶參數 → 沿用安裝當下的環境變數;都沒有就是關(預設)
```

優先順序:**參數 > 環境變數 > 預設關閉**。

三個平台(launchd plist / systemd unit / Windows cmd)都會寫入服務定義,
重跑 `setup.sh` 不會流失。

## 套用到本機服務

```bash
./scripts/setup.sh                          # 重新 build + 安裝 + 重啓服務(不啓壓縮)
./scripts/setup.sh -- --compress            # 同上,但啓用壓縮
./scripts/setup.sh --system -- --compress   # system 層級安裝,啓用壓縮(僅 Linux)
```

`--` 之後的參數會傳給 `mur-model-gateway install`,所以 `--compress` 是到處都有效的寫法。
不要用 `MUR_MODEL_GATEWAY_COMPRESS=1 ./scripts/setup.sh`:嗅探只看得到真正傳進 install
行程的環境,而 `--system` 會經過 `sudo`,sudo 預設會重設環境——變數被丟掉,裝出來的服務壓縮是關的。

## 驗證

```bash
# macOS:確認 plist 有寫入
grep -A2 MUR_MODEL_GATEWAY_COMPRESS ~/Library/LaunchAgents/run.mur-model-gateway.plist

# 跑一輪 Claude Code 後看共用統計
mur compress stats
```

壓縮後的原文存在 `~/.mur/compress`,模型可透過 `mur_retrieve` MCP tool 還原。
沒有掛 mur MCP server 的 client 只看得到壓縮摘要,無法還原——全面預設開啓前
先確認各 mur agent 都有接上。

## 診斷:整個 body 的 token 數

`compress summary` 的 `tokens_before/after` 只算有進 tokenizer 的 block,不是整個請求。
要知道「這層對整個請求省了幾 %」,另外開:

```
MUR_MODEL_GATEWAY_COMPRESS_BODY_TOKENS=1   # 搭配 RUST_LOG=debug
```

每個請求會多做一次全 body tokenize(約 150 ms CPU / MB),在背景執行、不擋請求,
但是持續的 CPU 稅,觀察結束就關掉。結果記在 `compress body tokens` 這行,
用 `scripts/compress-summary-report.sh` 彙總。

## 診斷:prompt cache 與 stale stub

Anthropic 的每個成功回應會多一行 `upstream usage`,帶 `input_tokens`、
`cache_read_input_tokens`、`cache_creation_input_tokens`(串流取自 `message_start`,
非串流取自 JSON 根層)。`cache_creation` 大於前一個 turn 新增的內容,就表示
前綴快取被打掉了。不需要額外 flag,`RUST_LOG=mur_model_gateway=debug` 就有。

`compress summary` 另有 `stale_hit` / `stale_miss`:stale stub(被後來的 Read/Edit
取代的 Read、與後面完全相同的 tool result)現在是 memo 過的,同一段 transcript 每個
turn 重送只會 hit。`stale_miss` 每個新的取代事件大約一次;若持續偏高,代表 memo
容量不夠或 transcript 內容在變。兩者都由 `scripts/compress-summary-report.sh` 彙總。
