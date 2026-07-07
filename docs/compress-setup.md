# 啓用 proxy 壓縮(CC_PROXY_COMPRESS)

wire-level tool_result 壓縮的安裝設定。設計背景見
[specs/2026-07-03-mur-compress-design.md](specs/2026-07-03-mur-compress-design.md)。

## install 參數

```
cc-proxy install --compress      # 服務定義寫入 CC_PROXY_COMPRESS=1(啓用)
cc-proxy install --no-compress   # 強制關閉,即使環境有 CC_PROXY_COMPRESS=1
cc-proxy install                 # 不帶參數 → 沿用安裝當下的環境變數;都沒有就是關(預設)
```

優先順序:**參數 > 環境變數 > 預設關閉**。

三個平台(launchd plist / systemd unit / Windows cmd)都會寫入服務定義,
重跑 `setup.sh` 不會流失。

## 套用到本機服務

```bash
./scripts/setup.sh                       # 重新 build + 安裝 + 重啓服務(不啓壓縮)
CC_PROXY_COMPRESS=1 ./scripts/setup.sh   # 同上,但啓用壓縮
```

## 驗證

```bash
# macOS:確認 plist 有寫入
grep -A2 CC_PROXY_COMPRESS ~/Library/LaunchAgents/run.cc-proxy.plist

# 跑一輪 Claude Code 後看共用統計
mur compress stats
```

壓縮後的原文存在 `~/.mur/compress`,模型可透過 `mur_retrieve` MCP tool 還原。
沒有掛 mur MCP server 的 client 只看得到壓縮摘要,無法還原——全面預設開啓前
先確認各 mur agent 都有接上。
