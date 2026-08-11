# 啓用 proxy 壓縮(MUR_MODEL_GATEWAY_COMPRESS)

wire-level tool_result 壓縮的安裝設定。設計背景見
[specs/2026-07-03-mur-compress-design.md](specs/2026-07-03-mur-compress-design.md)。

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
./scripts/setup.sh --system -- --compress   # system 層級安裝,啓用壓縮
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
