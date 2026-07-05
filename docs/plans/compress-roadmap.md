# Compress roadmap — cc-proxy / mur-compress

## Ground truths (verified 2026-07-05)

1. **Grammar 沒有缺。** headroom 0.30.0 走 `tree_sitter_language_pack`（`code_compressor.py` 的 `_get_parser`），python/js/ts/php/go/java/swift/dart 實測全部載入 OK。`headroom-ai[code]` 是 no-op，`code_compressor.py:1052` 的 Kompress fallback 在本機不會觸發。
2. **headroom 是第三方 pip 套件**（non-editable）。`read_lifecycle.py`、`cache_aligner.py`、`parser.py` 只能走 config 或 upstream PR，不是我們的實作面。
3. **我們擁有的是 Rust 這條路**：cc-proxy `src/compress.rs` → `mur-compress`（detect.rs 五類 regex 路由；compressors = diff/json/log/search/fallback；CcrStore hash-addressed 可取回）。沒有 code-aware 壓縮器。
4. **cc-proxy 無狀態但看得到全部。** 每個 request body 帶完整 transcript，所以 supersession / 跨訊息 dedup 不需要 session state——在 `rewrite_request_body` 內對整份 messages 做即可。
5. **Prefix-cache 約束的精確形式**：mur-compress 目前 cache-safe 是因為壓縮是 *content-deterministic*（同輸入→同輸出，與時間無關）。任何新 transform 只要維持「輸出只是 transcript 內容的純函數」就自動 prefix-stable；引入 recency window（距結尾 N 條）會讓舊訊息 bytes 每 turn 變動 → 每 turn cache miss，禁止。允許的例外：**one-shot flip** —— 某訊息因「後面出現了取代事件」而坍縮，flip 只發生一次，之後永久凍結；代價是一次 cache break，換整段 session 的持續節省。

## Ranked plan

### P1 — Per-type ledger（量測先行）✅ 完成
- `mur-compress`: `stats.rs` 新增 `StatsData.by_type: HashMap<String, TypeStats>`，keyed by `ContentType::as_str()`。`record_compression` 現在多收一個 content-type 參數，同時更新 all-time / per-version-day bucket / per-type 三個視圖。`StatsSnapshot.by_type` 對外暴露。
- `lib.rs` 的 `compress()` 已知 detect 出的 `ContentType`，一行接上 `ct.as_str()`。
- 舊 `stats.json`（沒有 `by_type` 欄位）照樣能反序列化（`#[serde(default)]`）。
- 沒有另外做 CLI 顯示——這個 repo 沒有既有的 `mur gain` 消費者(`gain` 屬於獨立的 `rtk` 工具)，資料就緒即可，等有消費者再接。

### P2 — Supersession collapse（最大單一槓桿）✅ 完成

位置：`cc-proxy/src/compress.rs`，在既有 per-block 壓縮**之前**跑一個 transcript-level pass，三個 provider 都有：`collapse_stale_tool_results_anthropic` / `_openai` / `_gemini`，共用 `compute_stale_reasons`（決策邏輯只寫一份）。

- Anthropic：`tool_use.id` 對 `tool_result.tool_use_id` 精確配對；`input.file_path` 取路徑。
- OpenAI：`tool_calls[].id` 對 `tool_call_id`；`function.arguments`（JSON 字串）解析取 `file_path`。
- Gemini：wire 上沒有 call/response id，用「同名 functionCall/functionResponse FIFO 配對」——只要一個 call 的 response 一定緊跟在它自己後面送回（任何合法 transcript 皆然），順序配對就是精確的。
- 決策：某 path 的 Read 只要不是「最後一次碰這個 path 的事件」（Read 或 Edit/Write 皆算），就判定 superseded。Edit/Write 結果本身、以及每個 path 最新一次 Read，永不碰——那是 in-flight Edit 的 `old_string` 錨點所在。
- 決定性：純粹是 transcript 內容的函數，重跑同一輸入得到全同 bytes（idempotent）。決策只會「向前翻轉一次」（某 superseding 事件第一次出現的那個 turn），之後永久凍結——對應一次 cache break，換之後全部重用。
- 原文一律經 `CompressEngine::archive()` 存入 CcrStore（見下），stub 內嵌 `mur_retrieve` hash。
- 混合內容（如 text+image 的 tool_result）一律跳過，不觸碰——`extract_tool_result_text` 只在「純文字」時回傳 Some。

**額外發現並補的洞**：`compress()` 對 Generic/程式碼內容（fallback compressor）從不 offload，永遠拿不到 hash——絕大多數 code Read 結果會落在這裡。加了 `CompressEngine::archive(&str) -> Option<String>`，無條件寫入 CcrStore、繞過 payoff gating，專門給「因為別的理由（過期/重複）要丟棄、而非因為壓得好」的內容用。

### P3 — Exact-duplicate tool-result dedup ✅ 完成（搭 P2 便車）
同一 pass 裡：對所有未被 supersession 標記的候選，直接用字串相等分組（O(n²)，單一 request 的 tool_result 數量小，可接受，且不像 hash 那樣有碰撞風險）；完全相同的重複（重跑 build/test 同輸出），保留**最後**一份，較早的換 `[identical to a later tool result in this conversation — mur_retrieve hash=…]`。沒做 near-dup diff（rolling similarity）——先看 P1 ledger 證明 near-dup 量值得再說。

測試：`cc-proxy/src/compress.rs` 新增 12 個測試，涵蓋三個 provider 的 supersession / 精確 dedup / idempotency-prefix-stability / mixed-content 安全性；`mur-compress` 新增 `archive()` 的 store-then-retrieve 測試。全綠，clippy 乾淨。

### P4 — Code skeletonization ✅ 完成（只作用於已死內容）
- `mur-compress` 新增 `src/skeleton.rs`：`skeletonize(source, file_path) -> Option<String>`，用副檔名(不是內容 heuristic——省了整套 detect 邏輯，因為呼叫端本來就知道 `file_path`)選 tree-sitter grammar，遞迴走 AST：命中 function-like node（Rust `function_item`；Python `function_definition`；JS/TS `function_declaration`/`method_definition`/`function_expression`/`arrow_function`）就把它的 `body` 欄位範圍換成 `{ /* elided */ }`，不再往內遞迴；只在 container-like node(module/class/impl/…)才繼續往下找巢狀函式。Imports、struct/class 宣告、簽名、docstring 全部保留原樣。
- 支援 **9 種語言**：Rust / Python / JavaScript / TypeScript(含 TSX) / Go / PHP / Java / Swift / Dart。其他副檔名回傳 `None`，呼叫端退回純文字 stub。再加語言一樣簡單(一個 grammar crate + 一筆 `LangSpec`，但要照下面的 ABI 規則先探測相容版本)。
- 依賴與 ABI 陷阱：`tree-sitter` workspace 內已被 `mur-core`(tokensave 的 code-graph 索引，只用於 Rust)釘在 `0.24`(ABI 14)；每個新 grammar crate 都要配合這個 ABI，而且**版本號跟 ABI 版本不是線性對應**，只能實測。踩到的坑：
  - `tree-sitter-python`/`tree-sitter-javascript` 的 `0.25.x` 是 ABI 15(給 tree-sitter 0.25+ 核心)，`set_language` 在 runtime 直接回 `LanguageError { version: 15 }`，編譯期完全看不出來——得靠測試抓；改釘 `=0.23.x` 才是 ABI 14。
  - `tree-sitter-go`/`tree-sitter-php`/`tree-sitter-swift` 同理，最新版都是 ABI 15，往回退到 `=0.23.4` / `=0.23.11` / `=0.6.0` 才吃 ABI 14。
  - `tree-sitter-dart` 更極端：連最舊的 `0.1.0`/`0.2.0` 都是 ABI 15，只有 `=0.0.4`(pre-1.0 alpha)是 ABI 14，而且這個版本的 grammar 本身node kind 命名也不同(用 `program`/`lambda_expression` 而非新版的 `source_file`/`function_declaration`)，且 API 是舊式 `pub fn language() -> Language`(不是 `LANGUAGE: LanguageFn` const)——`LangSpec` 裡 dart 那筆因此跟其他語言長得不一樣。
  - 排錯方法：對每個新語言，先暫時在 `skeletonize()` 裡塞 `eprintln!` 印 `set_language` 的 `Err`/`ranges.len()`，跑一次 `cargo test`，看到 `LanguageError { version: N }` 就代表 ABI 不合，往舊版本退；空 `ranges` 但 `Ok` 則代表 node kind 名稱猜錯，得用一個小測試把 `tree.root_node()` 遞迴印出來核對實際 kind。
- 掛載點：`cc-proxy`「一律無條件」不加 feature flag，只在 `stale_stub` 的 `SupersededPath` 分支呼叫——即只有已被 P2 判定過期的 Read 結果才可能被骨架化；`Duplicate`(重跑輸出)和任何「最新一份」都不碰。Skeleton 是**內嵌**在 stub 訊息裡的密集摘要，不需要額外 retrieval hop；完整原文仍照 P2 的方式經 `archive()` 存 CcrStore，兩者互不取代。

測試：`mur-compress::skeleton` 14 個測試(九語言各自的函式/方法/class 骨架化、無函式回 None、未知副檔名回 None、idempotency)；`cc-proxy::compress` 新增 1 個整合測試驗證 superseded Read 的 stub 真的內嵌了骨架。全部通過，clippy 乾淨，整個 mur workspace(`cargo check`)也確認未受影響。

### P5 — 分層協調（防重壓）
- cc-proxy 已有 `has_retrieve_marker` skip。反向：headroom 的 `parser.py` 認得 CCR marker——確認它對含 marker 區塊真的 no-op；若否，開 upstream issue/PR，這是唯一值得碰 headroom 的點。

### 不做（現階段）
- headroom 內部改造（supersession/read_lifecycle 強化）：非我們的 code；P2 在 proxy 層拿到同一份收益且對兩條鏈（有/無 headroom）都生效。
- SmartCrusher columnar/dict 編碼：headroom 側，upstream 題。mur-compress 的 `json.rs` 若 P1 顯示 JSON 佔比高再考慮。
- 跨 transcript rolling-hash diff store：P3 的 exact-dup 先吃掉大宗；near-dup 等數據。

## 落地順序
P1 ✅ → P2+P3 ✅（同一個 PR，共用 transcript pass，三個 provider）→ P4 ✅ → 用 ledger 驗證收益 → P5。
