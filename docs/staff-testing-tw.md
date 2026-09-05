# 內部測試安裝指南

> English: [staff-testing.md](staff-testing.md)

## 這是什麼

mur-model-gateway 是一個跑在**你自己電腦上**的本機代理服務(`127.0.0.1:8088`)。

- **沒有共用伺服器**,你不會連到別人的機器,也不會有人連到你的
- 它讀的是**你自己**的 Claude Code 登入憑證,憑證不會離開你的電腦
- 每個人各自安裝、各自執行、各自升級

## 安裝

```bash
curl -fsSL https://raw.githubusercontent.com/mur-run/mur-model-gateway/main/scripts/install-release.sh -o install-release.sh
less install-release.sh          # 執行別人給的腳本前先看一眼,是好習慣
bash install-release.sh
```

腳本會下載已簽章公證的官方發佈版、**驗證 SHA-256**、裝到 `~/.local/bin`,並註冊成開機自動啟動的背景服務。

## 第一次會跳一次密碼視窗 —— 這是正常的

macOS 會問「**"mur-model-gateway" 想存取鑰匙圈中的密碼 "Claude Code-credentials"**」。

### 請按「永遠允許」,不要按「允許」

| 你按的 | 結果 |
|---|---|
| **永遠允許** | 授權一次,之後再也不會問 ✅ |
| 允許 | 只授權這一次,之後還會一直問 |
| 拒絕 | 閘道器讀不到憑證,無法運作 |

**為什麼要授權**:閘道器必須讀取你的 Claude Code 憑證才能代你轉發請求。這個視窗是 macOS 在確認你同意,不是異常。

不小心按錯不要緊——重跑一次下面的健康檢查,它會再問一次。

## 確認正常運作

```bash
curl -s http://127.0.0.1:8088/__mur/health
```

應該看到:

```json
{"claudeCredential":"oauth","codexCredential":"chatgpt","status":"ok","version":"0.3.0"}
```

**關鍵是 `claudeCredential` 要是 `"oauth"`。** 如果是 `"missing"`:

- 這台機器還沒登入過 Claude Code → 執行 `claude auth login`
- 或者剛才的密碼視窗按了「拒絕」→ 再跑一次上面的健康檢查,重新授權

## 使用

```bash
export ANTHROPIC_BASE_URL="http://127.0.0.1:8088"
```

寫進 `~/.zshenv` 就會一直生效。

## 回報問題時請附上這兩項

```bash
curl -s http://127.0.0.1:8088/__mur/health      # 含版本號,這很重要
tail -50 ~/Library/Logs/mur-model-gateway/proxy.log
```

第一行的 `version` 欄位讓我們知道你跑的是哪一版——**沒有它我們無法判斷你遇到的問題是否已經修掉了**。

日誌在設計上不會寫出 token(所有持有憑證的型別都強制遮蔽),但貼出來之前自己掃一眼還是比較保險。

## 升級

重跑安裝指令即可,它會抓最新版並重啟服務。

## 移除

```bash
launchctl bootout gui/$(id -u)/run.mur-model-gateway
rm ~/.local/bin/mur-model-gateway ~/Library/LaunchAgents/run.mur-model-gateway.plist
```

移除後,鑰匙圈裡那筆授權會留著(失效但無害)。想一併清掉的話:「鑰匙圈存取」→ 搜尋 `Claude Code-credentials` → ⌘I → 「存取控制」→ 移除 `mur-model-gateway` 那筆。**不要刪除 `Claude Code-credentials` 項目本身**,那是你的 Claude Code 登入憑證。
