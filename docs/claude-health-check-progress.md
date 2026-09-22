# Claude Session Message 重構：進度與交接

> 這份檔是給「大腦」（Claude 主 session）在 context compact 之後接手用的。每完成一個工作包（WP）就更新。
> 目標規格：`docs/claude-health-check.md`。使用者拍板：**放棄 tmux 讀取／控制全部功能，走 MCP channel 路線，做到「最完美的 claude session message」。**

## 0. 不變的規則（來自 `~/.claude/CLAUDE.md`，這裡只列會影響派工的）

- 大腦不寫 code。困難／跨檔案 → `agent --model cursor-grok-4.6-xhigh`；一整頁／中等 → `cursor-grok-4.6-high`；機械性 → `agy --model gemini-3.8-flash-high`；review 奇數輪 Opus、偶數輪 grok xhigh，一隻做完 findings JSON → RED 測試 → 修到綠；盲測 Opus medium、不看實作。
- 同一時間最多 2 隻 agent；**同一時間只能有一隻在改 Rust 檔**（同一個 checkout，避免互相覆寫）。第二個 slot 給非 Rust 工作（node server、hook script、盲測寫在獨立新檔、文件）。
- 每隻 prompt 開頭逐字貼 codegraph 段落；禁 `git stash/checkout/restore/reset`；prompt 寫進檔案再 `-p "$(cat …)"`；報告要求寫進 `<scratch>/wpN-report.md`。
- 每個 WP 完成後：大腦跑 `cargo test -p claude_sessions -p remote` + `./script/clippy -p claude_sessions -p remote`，打包 `git diff` 到 scratch/baseline，更新本檔。
- README.md 前兩行的 `> [!IMPORTANT]` 已在，不要移除。
- 不 commit（使用者沒要求）。

## 1. 目標架構（一句話）

**讀**：transcript（250 ms tail）+ `~/.claude/zed-events/<session_id>.jsonl`（一支 hook dispatcher 寫所有事件，250 ms tail）+ `~/.claude/zed-status/<session_id>.json`（statusLine wrapper，1 s）。
**寫**：Zed channel MCP server（node，Claude Code 以 `--dangerously-load-development-channels server:zed-claude` 載入），Zed 透過 `~/.claude/zed-channel/<claude_pid>/` 的檔案佇列送訊息與權限決定。
**刪**：`capture_pane`、`send_input`／`PaneKey`／send-keys、`TerminalView` attach、pane mirror、`permission_mode()` 畫面 parser、問題游標 parser、兩個舊 hook。

## 2. 工作包與狀態

| WP | 內容 | 模型 | 狀態 | 報告 |
|---|---|---|---|---|
| WP0 | 打包基線、交接文件、基線測試 | 大腦 | ✅ 2026-09-18 08:24 patch 在 `scratch/baseline/` | — |
| WP1 | 事件／狀態讀取層 + 刪除 terminal 讀取面（hook dispatcher、statusLine wrapper、`tail_events`／`read_status`、`LiveState`、store 換 poll、panel 拆 terminal） | grok xhigh | ✅ 09:12；大腦 gate：363+164 tests 綠、clippy 乾淨；diff 快照 `scratch/baseline/after-wp1-*.patch` | `scratch/wp1-report.md` |
| WP1-blind | `live_state.rs` 盲測：40 tests 落在 `blind_live_state_tests.rs`（1599 行），**尚未掛進 `claude_sessions.rs`**；等 WP1-review 結束後大腦加 `#[cfg(test)] mod blind_live_state_tests;` 親跑。盲測者假設 `MessageDisplay` 欄位直接在 `event` 物件上（正確）。 | Opus | ✅ 09:30 落地，待跑 | `scratch/wp1-blind-report.md` |
| WP3a-readme | README 補環境變數／上限／error.reason 開放集合／setup 指令（74 行，大腦抽查關鍵字齊） | agy gemini-3.8-flash-high | ✅ 09:38 | `scratch/wp3a-readme-report.md` |
| WP3b／WP4a／WP5 規格 | 已寫好：`scratch/wp3b-spec.txt`、`wp4a-spec.txt`、`wp5-spec.txt`；依序在 WP1-review 之後派（同時只一隻改 Rust） | — | 待派 | |
| WP1-review | 第 1 輪：7 findings（F00 缺 id 事件忽略、F01 hook 多 write(2) 交錯遺失、F02 status 檔無上限→256 KiB、F03 未選 session 時 hooks_installed 永遠 false、F04 `session_id: null` 變 "None" → fixed；F05/F06 wontfix）；366+169 綠；clippy 乾淨 | Opus high | ✅ 09:46 | `scratch/wp1-review1-report.md` |
| WP1-blind 執行 | 大腦掛進 `claude_sessions.rs` 後親跑：**40/40 綠**（實作與盲測零分歧） | 大腦 | ✅ 09:48 | `scratch/wp1-blind-run.log` |
| WP3a | Zed channel MCP server（node，無依賴）+ node 測試 | grok xhigh | ✅ 08:37，11/11 tests 綠（大腦親跑） | `scratch/wp3a-report.md` |
| WP3a-review-1 | Opus 對抗式 review：12 findings（9 fixed、3 wontfix）、9 RED→GREEN，21/21（大腦親跑） | Opus high | ✅ 08:56 | `scratch/wp3a-review1-report.md` |
| WP3a-review-2 | 第 2 輪換家族：3 findings（F13 parked-name 重用被跳過 → fixed；F14 `id:null` 視為 notification、F15 512 上限 fail-closed → wontfix），22/22（大腦親跑）。**channel server 收工。** | grok xhigh | ✅ 09:16 | `scratch/wp3a-review2-report.md` |
| WP1 gate（review 後） | 大腦親跑：406+169 tests 綠（含盲測 40）；clippy 起初因盲測檔兩個 `redundant_clone` 紅，大腦直接刪掉那兩個 `.clone()` 後乾淨。快照 `scratch/baseline/after-wp1-review1-*.patch` | 大腦 | ✅ 09:52 | `scratch/wp1-review1-gate.log` |
| WP3b | Rust 整合：channel 檔案佇列的 source／proto／handler、store `send_message`／`answer_permission`、刪 `send_input`／`PaneKey`／`SendClaudeInput`、問題卡文字答、slash 三分類。大腦 gate：372（含盲測 40）+159 綠、clippy 乾淨、codegraph 查無 send-keys 殘留、node 22/22。快照 `scratch/baseline/after-wp3b-*.patch` | grok xhigh | ✅ 10:20 | `scratch/wp3b-report.md` |
| WP3b-review-1 | Opus：11 findings（**WP3B-1 high**：hook 的 pending permission 與 inbox `permission_request` 配對沒比時間 → 可能核准錯的 prompt，改成 5 s 視窗；WP3B-2/3 answered／ticks 改以 `tool_use_id` 為鍵；WP3B-4 outbox 名稱跨 process 碰撞；WP3B-6 heartbeat 顯示 → fixed；5 wontfix；WP3B-5 blocked→大腦裁決 R1）。大腦 gate：377+160 綠、clippy 乾淨 | Opus high | ✅ 11:10 | `scratch/wp3b-review1-report.md` |
| WP3b-review-2 | grok：4 findings 全修（R1、R2、WP3B-12 outbox 名稱重試只 16 次就放棄、WP3B-13 `at_ms` 字串數字變 0 落入配對視窗）；round-1 wontfix 維持。大腦 gate：**379+161 綠、clippy 乾淨**。**WP3 全部收工。** 殘留：同一 turn 內 <5 s 兩個同名工具 prompt 只能靠順序（channel 協定沒帶 tool_use_id） | grok xhigh | ✅ 11:32 | `scratch/wp3b-review2-report.md` |
| WP4a | entries／records：HC-48 角色（peer／task-notification）、system content、`turn_duration` footer、`ai-title` 標題、tokens_left／auto_mode／cost-state facts、attachment 依 turn 摺疊、每工具卡（Edit diff 用 `similar`、TodoWrite、mcp 拆名、AskUserQuestion 卡、shell 小弟卡）、result 內 image、SentFiles is_error／mp4／大圖。大腦 gate：**415+161 綠、clippy 乾淨**；快照 `after-wp4a-*.patch` | grok xhigh | ✅ 12:05 | `scratch/wp4a-report.md` |
| WP4a-review-1 | Opus：15 findings（12 fixed／3 wontfix）。**兩條安全**：F1 `bridge_status.url` 直接 `open_url`（`file://` 也會開）→ 只接受 `https://claude.ai/`；F2 `open_sent_path` 對任何本機存在的路徑 `open_with_system` → 套 `attachment_is_readable` 邊界。F4 Edit diff 每 frame 重算無上限 → 進 `EntryCache` + 128 KiB 上限；F10 dispatch 卡 prompt 讀取任務被 rebuild 每 250 ms 取消 → 永遠 Loading；F7 舊 peer 記錄仍畫成 You；F8 `15,000,000` 解成 15；F9 dispatch 指令解析未錨定。大腦 gate：**430+161 綠、clippy 乾淨** | Opus high | ✅ 12:38 | `scratch/wp4a-review1-report.md` |
| WP4b | view：now row + 每輪摘要取代 `show_tool_calls` 刪除濾鏡、Saying markdown／拖曳／30 s 守衛、context 進度條 + facts、status line、Interrupt（`channel_interrupt` + `can_interrupt`）、版面；併入 WP4a review 的 F11／D1／D2／D3。大腦 gate：**438+163 綠、clippy 乾淨**；快照 `after-wp4b-*.patch`。註：`blind_input_tests.rs`（150 行）在 WP3b 時已被刪——它測的是 `SessionInput`／send-keys 路徑，整條路徑已移除，屬合理刪除，WP3b 報告有列 | grok xhigh | ✅ 12:58 | `scratch/wp4b-report.md` |
| WP4b-review-1 | grok：7 findings（F1 **Stop 雙擊會排兩個 SIGINT**、F2 `features` 非陣列讓整個 channel 誤判死亡、F3 elapsed 未上限、F4 meter 未夾 0–100、F5 負／零 `resets_at` 顯示 1969 → fixed；W1 遠端時鐘偏差讓 Saying 30 s 守衛提早、W2 status 過期仍優先 → wontfix）。大腦 gate：**442+164 綠、clippy 乾淨** | grok xhigh | ✅ 13:20 | `scratch/wp4b-review1-report.md` |
| WP5 | `sessionId` 身分、ended row（Resume in background／Open in claude.ai／Open in tmux／Dismiss）、`claude agents --json` 背景 session、全部 poll timeout + 各自 `ErrorSource`、`stale_for`、`pending` 上限、隱藏時降頻、remote batch subagent／增量快取／`ListClaudeSessionFiles` 邊界；`can_send` 對 Ended 拒送。大腦 gate：**452+169 綠、clippy 乾淨**；快照 `after-wp5-*.patch` | grok xhigh | ✅ 14:35 | `scratch/wp5-report.md` |
| WP5-review-1 | Opus：10 findings（**WP5R1-1 high**：Attach 按鈕把 agents 列表的 `id` 直接插進 shell 指令行，remote host 可藉此在你本機 terminal 跑任意指令 → `claude_command_operand` 白名單；WP5R1-2/3 增量快取半行讀取與非 UTF-8 行永久漏記錄；WP5R1-4 timeout 沒殺子程序 → `kill_on_drop`；WP5R1-5 未選 session 時 stale chip 亂亮；WP5R1-6 同 sessionId 兩個 pid 每秒誤判 rebind；WP5R1-7 id 以 `-` 開頭會變成 `claude` 的選項 → fixed；3 wontfix）。大腦 gate：**456+173 綠、clippy 乾淨** | Opus high | ✅ 15:45 | `scratch/wp5-review1-report.md` |
| WP5-review-2 | grok 安全第 2 輪：4 findings 全修（spawn 指令 stdout 上限、相對 `cwd` 拒收、`claude_ai_session_url` 與 `bridge_url` 共用白名單、agents JSON 對抗輸入）；round-1 的注入修法 probe 未破。大腦 gate：**460+180 綠、clippy 乾淨**。**WP5 收工** | grok xhigh | ✅ 16:05 | `scratch/wp5-review2-report.md` |
| zed 全 app build | `cargo build -p zed` 成功（dev profile，55 分鐘；無 error、無 unused warning）— 全 app 可編譯 | 大腦 | ✅ 15:30 | `scratch/zed-build.log` |
| WP3c | channel server 加 `{"kind":"interrupt"}` → 一次 SIGINT 給 ppid；3 s 節流（`interrupt_throttled`）、mtime > 10 s 拒（`interrupt_stale`）、ppid 0/1 拒（`interrupt_unavailable`）、kill 失敗 `interrupt_failed`；inbox `{"kind":"interrupted",…}`；`server.json.features = ["message","permission","interrupt"]`。大腦親跑 26/26 | grok xhigh | ✅ 10:40 | `scratch/wp3c-report.md` |
| WP3c-review | Opus：6 findings（I01 節流改 monotonic clock、I02 只對啟動時的 `claudePid` 送、I03 reason 截 512、I04 **回歸**：R2 把 Set 改 Map 後 prune 迴圈迭代 pair 永不清 → fixed；I05/I06 wontfix），33/33（大腦親跑） | Opus high | ✅ 11:00 | `scratch/wp3c-review1-report.md` |
| WP3c-verify | 窄驗證：**V01 證實**——`reason.slice(0,512)` 會切開 surrogate pair，寫出 `\ud83d` 孤立跳脫，`serde_json` 拒收整行（實測 exit 2）；改成以 code point 截斷。其餘界限（首發節流、pid 守衛、prune）probe 皆通過。**34/34（大腦親跑）。channel server 正式收工**（實作 + 4 輪審） | grok xhigh | ✅ 11:20 | `scratch/wp3c-verify-report.md` |
| WP3d（待派，Rust） | Zed 端 Interrupt：`SessionSource::channel_interrupt(claude_pid)` 寫 `{"kind":"interrupt"}`；store 只在 `live().turn == Running` 且 `server.json.features` 含 `interrupt` 時允許；Stop 按鈕啟用；並入 WP4b 或獨立小包 | grok high | 待 WP3b-review 後 | |
| WP4a | entries／records：HC-48 角色、`ai-title`、`bridge-session`、`turn_duration`、`away_summary`、system content、`tool_target` fallback、result 內 image、SentFiles is_error／mp4／大圖、Edit diff | grok high | 待 | |
| WP4b | view：now row + 每輪摘要、Saying markdown／拖曳、context bar、權限卡、問題卡（全部顯示 + 文字答）、slash 三分類、輸入狀態守門、版面 | grok xhigh | 待 | |
| WP5 | 生命週期：`sessionId` 身分、ended row、Resume（`claude --bg --resume`）、`claude agents --json`、poll timeouts、ErrorSource、remote batch／邊界（HC-05、HC-24、HC-29、HC-30） | grok xhigh | 待 | |
| WP6a | 機械掃尾：Gemini 只完成一件事（移除 `claude_sessions` 已無人用的 `project` 依賴），其餘卡在等自己的背景 cargo 而逾時、沒寫報告；死碼確認與 keymap／action 對照併入 WP7 第 13／14 項，最終驗收由大腦自跑 | agy gemini-3.8-flash-high | ⚠️ 部分完成 16:35（gate 見 `scratch/gate-after-wp6a.log`） | 無報告 |
| WP6b | 文件完成：`docs/claude-sessions-setup.md`（160 行 zh-TW）、`docs/claude-sessions-architecture.md`（122 行）、`docs/claude-health-check.md` 加 48 列狀態表。它列出的 12 處來源分歧已看過：本檔 §3.1 的「README 尚未補」已過期（README 已補）；status 檔上限現為 1 MiB；events 檔 8 MiB 截斷；`uninstall_zed_hooks` 存在但無 UI（WP7 補）。未涵蓋項目 HC-22／25／26／31／38／39／47 與 partial 的 HC-23／32／35／41／42 → WP7 | Fable | ✅ 16:35 | `scratch/wp6b-report.md` |
| WP7 | 收尾剩餘 HC 項：失敗送出保留＋Retry、貼圖錯誤獨立列、`@` 選單截斷／debounce／本地縮窄、貼檔命名與清理、Up 歷史來源、選單狀態隨 session、Uninstall hooks 按鈕、`hooks_installed` 30 s 快取、Windows remote 提示、Send 錯誤清除、Escape 全面板 | grok xhigh → **GPT-5.6 Sol xhigh**（grok 跑 89 分鐘 2 s CPU 零輸出，判定卡死砍掉） | ✅ 17:20 codex 完成（15 測試、3 舊測試依規格移除＋1 改寫；gate 469+182 綠）；review-1 Opus ✅ 17:45（2 fix：prune 失敗不再害死貼檔、空 liveness 提示不畫；6 wontfix；3 spec gap → §3.4 裁定）；review-2 grok xhigh（刪除路徑）✅ 18:10（2 fix：uninstall 不再連使用者同 matcher 的 hook 一起刪、不覆寫使用者換過的 statusLine；+1 守衛測試；2 wontfix；R2-G1 → §3.4 裁定為缺陷 → WP7b）；gate 471+187 綠；快照 `after-wp7-review2-*.patch` | `scratch/wp7-report.md` |
| WP7b | Failed／pending 送出列屬於 session：切 session 隱藏不刪、`/clear` 改綁新 sessionId、`dismiss_ended`／process-gone 保留、50 列上限（先丟已結束的舊列）；規格 `scratch/wp7b-spec.txt` | grok high | ✅ 18:26（第 1 次正確停下回報測試衝突 → 大腦補裁決 → 第 2 版完成：`rebuild_entries` 不再 `retain_session`、`entries_for(selected)` 過濾、store `take_cleared_rebind()`、配對只看選中 session、finished-only 50 列上限；8 新測試 + 1 改寫；gate 479+187 綠）；窄審 Opus ✅ 18:47（1 fix：Up 歷史撈到別 session 的列 → `message_history_for(selected)`；2 wontfix F2 走訪守衛／F3 同一次 scan 兩段 `/clear` 未串接；spec gap G1 死 session 的 in-flight 列永不驟逐 → §3.4 裁定 → WP7c）；gate 480+187 綠 | `scratch/wp7b-report.md`（第 1 次：`wp7b-attempt1-report.md`） |
| WP7c | WP7b 窄審收尾：G1 死 session 的 in-flight 列標 Failed 交給 50 列上限；F3 同一 scan 多段 `/clear` 依序改綁；F2 走訪守衛改看選中 session 有列 | grok high | ✅ 18:58（`take_cleared_rebinds()` Vec 依序改綁、孤兒 in-flight 列標 Failed、走訪守衛看選中 session；6 新測試、0 既有測試改動；gate 486+187 綠）；窄審 Opus ✅ 19:15（0 fix／4 wontfix；mutation 測試 5/6 新測試有牙；**SG-1**：F3 前提錯 — 一次 scan 兩段 `/clear` 必來自兩個 pid，是 store 迴圈中途改 `self.selected` 的假象，會把 pid41 的訊息錯綁到 pid42 的 session → WP7d；DW-1 既有測試 `a_followed_transcript_that_never_answers_is_still_reported_stale` 在高負載下 1/40 會紅 → WP7d） | `scratch/wp7c-report.md` |
| WP7d | `apply_registry_scan` 以 scan 開始時擷取的 selection 比對，一次 scan 最多一組 pair、selection 跟著自己的 pid 走；改寫兩個 WP7c 測試、新增跨 scan 累積測試；DW-1 測試改成確定性 | grok high | ✅ 19:30（scan 開始擷取 selection、一次最多一組 pair；兩測試改寫＋1 跨 scan 累積測試；DW-1 改成 `now = last_transcript_ok_ms + 60s` 只斷言 `is_some()`，連跑 5 次全綠；gate 487+187）；窄審 Opus ✅ 19:45（0 fix／3 wontfix／1 blocked-by-rule：DW-1 測試被放寬成 `is_some()` 失去守護力，reviewer 驗證出強斷言的確定性 patch；大腦裁定採用並自行套用，10× `--test-threads=10` 全綠；REBOUND-PRECEDENCE 既有、≤1 s 自癒 → wontfix 記入架構文件 §6） | `scratch/wp7d-report.md`、`wp7d-review1-report.md` |

## 3. 已決定的介面（實作與盲測共用；改了要同步改這裡）

見 `scratch/wp1-spec.txt` §「介面凍結」。摘要：

- `crates/claude_sessions/src/live_state.rs`（純函式，無 gpui）：`HookEvent`、`parse_hook_event(&str) -> Option<HookEvent>`、`LiveState` + `apply(&HookEvent)` + `note_transcript_assistant(timestamp_ms)` + `note_tool_result(tool_use_id)`、`StatusSnapshot::parse(&str) -> Option<StatusSnapshot>`。
- `SessionSource` 新增 `tail_events(session_id, TailState) -> Task<Result<TailProgress>>`、`read_status(session_id) -> Task<Result<Option<String>>>`、`install_hooks() -> Task<Result<HookInstallOutcome>>`、`hooks_installed() -> Task<Result<bool>>`；移除 `pending_question`、`install_question_hook`、`capture_pane`。`send_input` 由 WP3b 換掉。
- 檔案路徑：`~/.claude/hooks/zed-claude-events.sh`、`~/.claude/hooks/zed-claude-status.sh`、`~/.claude/zed-events/<session_id>.jsonl`、`~/.claude/zed-status/<session_id>.json`、`~/.claude/zed-status/chained-command.txt`、`~/.claude/zed-channel/<claude_pid>/{server.json,inbox.jsonl,outbox/,server.log}`。

### 3.1 WP3a 已凍結的檔案協定細節（實作者選擇，大腦已接受）

- 位置：`crates/remote/assets/zed-claude-channel/{server.mjs,server.test.mjs,README.md}`；root `$ZED_CLAUDE_CHANNEL_ROOT` 或 `~/.claude/zed-channel`；session dir = `<root>/<claude pid = server ppid>/`。
- `server.json` 每 5 s 重寫（`{pid, claude_pid, started_at_ms, heartbeat_at_ms, protocol:1}`）；SIGHUP 照預設終止。
- `outbox/`：Zed 寫 `<name>.tmp` 再 rename；server 每 200 ms 依檔名字典序處理、寫出 stdout 後刪檔；`initialized` 前檔案留在磁碟不送。
- `inbox.jsonl` kinds：`ready`、`permission_request`、`permission_answered`、`message_sent`、`error`（reason ∈ `invalid_json|not_an_object|invalid_message|invalid_behavior|unknown_request_id|unknown_kind`）、`closed`（reason `stdin_end|signal`）。超過 4 MiB 時整檔換成新行。
- 非字串 `content` 拒送；非字串 `tool_name/description/input_preview` 轉 `""`。
- Review 1 後追加（已接受）：session dir 與 `outbox/` 為 0700（假設 Zed remote server 與 `claude` 同一 POSIX user）；outbox 單檔上限 4 MiB、stdin 單行上限 8 MiB；open permission 最多 512 筆、30 min TTL，超出者 verdict 一律拒（fail closed）；不可刪的 outbox 檔會被 park 不重送；無法讀的 entry 改名 `.bad`；`error.reason` 另有 `not_a_file|too_large|unreadable`（Zed 端視為開放集合）；env `ZED_CLAUDE_CHANNEL_HEARTBEAT_MS`（測試用）；stdout EPIPE 不殺 process，等 stdin EOF 走 `stdin_end`。README 尚未補這些（WP6）。

### 3.2 WP1 實作者的問題 → 大腦裁決（已定案）

- `#[cfg(test)] mod pane_screen_parsers`（舊的 pane 游標 parser 與測試）：**WP3b 一併刪除**。
- `PreToolUse`／`PermissionRequest` 沒有 `tool_use_id`：**忽略該事件**（只做 rule 1），不要建 `""` id 的 RunningTool。WP1-review 時修。
- `TailClaudeEvents` 沒有 `path` 欄位：**接受**。events 檔只會被我們自己截斷，shrink 偵測足夠。
- `PermissionDenied` 不清 `pending_question`：**接受**（AskUserQuestion 不會走 permission）。
- `SessionEnd` 連 `compacting`／`last_notification` 也清：**接受**（spec 原意就是「除 permission_mode 外全清」）。
- `Turn` 用 `#[derive(Default)]`；`tool_input` 為 null 時存 `None`；`MessageDisplay` 缺 `delta` 視為 `""`：**接受**。
- 新 proto：`TailClaudeEvents{project_id, session_id, offset, pending}`、`ReadClaudeStatus`、`InstallClaudeHooks`、`ClaudeHooksInstalled`；舊的 `CaptureClaudePane`／`GetClaudePendingQuestion`／`InstallClaudeQuestionHook` 已刪。`SendClaudeInput` 仍在（WP3b 換掉）。

### 3.3 WP3b 實作者的問題 → 大腦裁決（已定案）

- **Interrupt**：channel 沒有 Escape 原語。裁決：WP6 加 outbox `{"kind":"interrupt"}` → server 對 `claude_pid` 送 **一次 SIGINT**（等同終端 Ctrl+C，headless.md 也說 SIGINT 結束當前 turn）；Zed 端只在 `turn == Running` 時啟用 Stop 按鈕，避免 idle 時的單次 Ctrl+C 變成「再按一次就離開」。server 端要防連按（同一秒內只送一次）。在那之前 Stop 停用 + tooltip。
- `send_message` 回 `Task<Result<String>>`（outbox 檔名，供配對）：**接受**。
- AskUserQuestion 用組成文字回答：**永久 workaround**，直到官方 relay 問題。
- `/mcp <arg>` 送、裸 `/mcp` 只在 terminal：**確認**。
- Escape → `DismissMenus` 全平台：**接受**。
- `is_zed_mirror_session` 留給 tmux 列表過濾：**接受**（mirror 不再產生，過濾無害）。

### 3.4 WP7 review-1 提出的規格漏洞 → 大腦裁決（已定案）

- **R1-G1 盲測與 gate 指令衝突**：`cargo test -p claude_sessions` 一定會編進 blind 模組。裁決：實作者／reviewer 用 `-- --skip blind_` 跑，禁令只針對「開檔讀／改」；含盲測的完整 gate 由大腦跑。
- **R1-G2 store 自己換 selection 要不要清選單狀態**：不用。`/clear` 同 pid 換 sessionId 但 cwd 不變；`dismiss_ended` 選成無、輸入框唯讀。只有使用者自己的選取手勢才清（維持 WP7 實作）。
- **R1-G3 貼檔 7 天用誰的時鐘**：host 時鐘管清理，client 時鐘只負責命名；偏差要超過 7 天才有影響，接受。
- **R2-G1 Failed 送出列在 selection 改變時被 `retain_session` 丟掉**：裁決 → 這是缺陷，不是設計。Failed 列屬於它的 session：切到別的 session 時**隱藏不刪**，切回來要還在；`/clear` 同 pid 換 sessionId 時 pending／Failed 列要跟著改綁到新 sessionId；`dismiss_ended`（選成無）與 process-gone 保留列直到使用者按關閉或 Retry 成功。既有測試 `pending_messages_do_not_follow_the_reader_to_another_session` 描述的是「不顯示在別的 session 底下」，與此裁決相容（隱藏即可）。→ WP7b（review-2 結束後派 grok high，Opus 窄審 diff）。
- **WP7b 補充裁決（18:20）**：(a) `pending_messages_do_not_follow_the_reader_to_another_session` 是編碼舊行為的測試，允許改寫成「隱藏不刪」版本（改名 `pending_messages_are_hidden_not_dropped_when_the_reader_switches_session`），其他既有測試不准動；(b) `/clear` 改綁靠 store 最小 hook `take_cleared_rebind() -> Option<(old, new)>`；(c) 配對（`pair_with`／`holds_text`／`pair_with_channel`）只看選中 session 的列；(d) 50 列上限只算已結束（delivered／failed）的列，in-flight 不計也不驅逐。
- **WP7b 窄審 G1（18:50）**：session 既不在 live 也不在 ended 的 in-flight 列，在 `rebuild_entries` 裡（改綁之後、渲染之前）標成 Failed（錯誤文字 `the session ended before this message arrived`），交給既有 finished-only 50 列上限；live 或 ended 的 session 的列一律不動。F3 串接、F2 守衛一併收進 WP7c。
- **WP7c 窄審 SG-1（19:18）**：F3 裁決前提有誤。修正裁決：`apply_registry_scan` 在 scan 開始時擷取一次 selection，所有被換掉的 pid 都與這個值比對；一次 scan 最多產出一組 (old, new)，`self.selected` 跟著讀者正在讀的那個 pid 走到新 id，不跳到別的 pid。`cleared_rebinds: Vec` 保留，用途是跨 scan 累積。WP7c 兩個把假象寫死的測試允許改寫（名單在 `wp7d-spec.txt`）。
- **WP7d 窄審（19:48）**：(a) DW-1 裁決 2 的「放寬到裁決保證的內容」指的是「transcript 被回報為 stale」，不是 `is_some()`；採用 reviewer 驗證的 patch（select 後多推一次 `REGISTRY_POLL_INTERVAL` 讓 registry 時鐘嚴格較新，斷言 `matches!(stale, Some(("transcript", _)))`），由大腦套用。(b) REBOUND-PRECEDENCE（同一 scan 既 rebind 選中 sessionId 到別的 pid 又 clear 自己的 pid → follow 目標與 Send 錯誤慢一個 scan）：既有缺陷、≤1 s 自癒、觸發需第二終端 `--resume` 與自己的 `/clear` 落在同一秒 → wontfix。(c) 兩個 nit（新測試裡多餘的 `select`、註解理由寫錯）wontfix。
- **R1-W3 面板 Escape 沒選單也吞掉**：接受，這正是 HC-32 要的（收合時 Esc 也要清問題卡焦點）；popover／context menu 有更深的 key context 不受影響。

## 4. 已驗證事實速查（實作時不要重查）

- `permission-mode` 記錄 = 權限模式；`mode` 記錄不是（永遠 `normal`）。
- `MessageDisplay` payload 欄位：`session_id, transcript_path, cwd, scratchpad_dir, prompt_id, hook_event_name, turn_id, message_id, index, final, delta`。
- 你打的 user 記錄：`origin.kind="human"`, `promptSource="typed"|"queued"`；subagent 回報：`origin.kind="peer"`, `promptSource="system"`, `isMeta=true`；task 通知：`origin.kind="task-notification"`, `promptSource="system"`（無 isMeta）。
- hook 事件（官方）：`PermissionRequest`（可回 decision）、`PermissionDenied`、`Notification(notification_type)`、`Stop(last_assistant_message)`、`SubagentStop`、`PreCompact/PostCompact`、`PreModelSwitch/PostModelSwitch`、`UserPromptSubmit`、`PreToolUse/PostToolUse`、`SessionStart/SessionEnd`、`MessageDisplay`（timeout 預設 10 s）。所有事件帶 `session_id, cwd, permission_mode, transcript_path, hook_event_name`。
- statusLine stdin JSON：`session_id, transcript_path, model{id,display_name}, context_window{total_input_tokens,total_output_tokens,context_window_size,used_percentage,remaining_percentage,current_usage}, cost{total_cost_usd,…}, effort{level}, rate_limits{five_hour{used_percentage,resets_at},seven_day{…}}, exceeds_200k_tokens`。300 ms debounce。
- Channel 協定：server 宣告 `capabilities.experimental['claude/channel']={}` 與 `['claude/channel/permission']={}`；送 `notifications/claude/channel {content, meta}`；收 `notifications/claude/channel/permission_request {request_id, tool_name, description, input_preview}`；回 `notifications/claude/channel/permission {request_id, behavior:'allow'|'deny'}`。啟動旗標 `--dangerously-load-development-channels server:<mcp server name>`（會有一次確認提示，是否每次未實測）。
- registry `~/.claude/sessions/<pid>.json`：`pid, sessionId, cwd, tmux, bridgeSessionId, status, name, version, procStart`。`https://claude.ai/code/<bridgeSessionId>`。
- 本機 tmux：`window-size latest`；Zed mirror client `zed-claude-mirror-*` 目前仍掛著，WP1 刪 attach 後用 `tmux kill-session -t <name>` 清。
- `claude -p --resume` 是新 process，不是 attach。`claude --bg --resume <id>` 在背景續同一 session。
- 使用者的 `claude()` zsh function 預設加 `--remote-control`，可用 `CLAUDE_EXTRA_ARGS` 注入額外旗標（放 `--dangerously-load-development-channels server:zed-claude`）。

## 5. 待使用者確認／實測的事

- dev-channel 旗標的確認提示是否每次啟動都跳（WP3a 完成後大腳在 tmux 新 window 實測一次）。
- `claude mcp add --scope user zed-claude -- node ~/.claude/zed-channel/server.mjs` 是否為註冊 MCP server 的正確做法（WP3a 驗）。

## 6. 日誌

- 2026-09-18 08:24 WP0 完成。基線 patch：`scratch/baseline/working-tree-20260918-0824.patch`（604+106+… 行 SendUserFile 工作，未 commit）。基線測試結果見 `scratch/baseline/test-baseline.log`。
- 2026-09-18 16:35 WP6a（Gemini）只移除 `project` 依賴即逾時；殘留項併入 WP7。WP6b 文件三份落地。Gate 460+180 綠。
- 2026-09-18 16:40 WP7 派 grok xhigh；跑 89 分鐘 2 s CPU 零輸出 → 砍掉，改派 codex GPT-5.6 Sol xhigh（使用者事後說明 grok 額度仍在，下次先重試 grok 一次）。
- 2026-09-18 17:20 WP7（codex）完成：HC-22／23／25／26／31／32／35／37／38／39／41／42／47 全部 shipped；15 個新測試；移除 3 個編碼舊行為的測試、改寫 1 個。大腦跑含盲測的完整 gate：469+182 綠、clippy 乾淨。順手還原 `remote_server/src/server.rs` 與 `tmux_sessions_panel.rs` 兩處純 fmt 差異。快照 `scratch/baseline/after-wp7-1722.patch`。健康檢查狀態表 48 列全部 shipped；setup 文件補 Uninstall hooks 段、架構文件補 `uninstall_hooks`／`liveness_unavailable_reason`／貼檔清理。
- 2026-09-18 17:25 WP7 review-1（Opus，對抗式）派出；規格 `scratch/wp7-review1-prompt.txt`，報告 `scratch/wp7-review1-report.md`。之後：gate → 依嚴重度決定是否第 2 輪（grok xhigh）→ 最終驗收（`cargo build -p zed`、node 34 已綠）。
- 2026-09-18 17:45 WP7 review-1（Opus）完成：2 fix／6 wontfix／3 spec gap（裁定見 §3.4）。Gate 471+184 綠。快照 `after-wp7-review1-1742.patch`。
- 2026-09-18 17:56 WP7 review-2（grok xhigh，刪除路徑）跑 11 分鐘看似卡死被大腦砍掉，其實它 30 秒前剛寫完 findings JSON（`wp7-review2-report.md`：R2-F1 uninstall 連使用者同 matcher 的 hook 一起刪；R2-F2 statusLine 使用者換過仍被覆寫；2 wontfix；R2-G1 Failed 列在 `/clear`／切 session 會被 `retain_session` 丟掉 → 規格漏洞待裁）與兩個 RED 測試。大腦驗證兩測試紅在對的原因後，18:05 重派 grok 接續步驗 3（`wp7-review2-resume-spec.txt`）。教訓：grok xhigh 首次 tool call 前會想 15 分鐘以上，30 分鐘內不判卡死。
- 2026-09-18 18:10 review-2 步驗 3 由重派的 grok 完成；大腦 gate 471+187 綠、clippy 乾淨；產品碼改動只在兩個被點名的函式。18:12 派 WP7b（grok high）。之後：Opus 窄審 WP7b diff → `cargo build -p zed` → 總結。
- 2026-09-18 18:27 WP7b 完成，gate 479+187 綠、clippy 乾淨，快照 `after-wp7b-1827.patch`。18:30 派 Opus 窄審（`wp7b-review1-spec.txt` 內容擴充後直接餵給 Agent）。`cargo build -p zed` 等窄審結束再跑，避免與 reviewer 搶 target 目錄鎖。
- 2026-09-18 18:47 WP7b 窄審（Opus）完成：1 fix／2 wontfix／1 spec gap；gate 480+187 綠、clippy 乾淨；快照 `after-wp7b-review1-*.patch`。18:50 派 WP7c（grok high）。之後：Opus 只審 WP7c diff → `cargo build -p zed` → 總結。
- 2026-09-18 19:03 WP7c 完成；大腦 gate 486+187 綠、clippy 乾淨；快照 `after-wp7c-*.patch`；panel 實際 delta 377 行（diff 統計縮水是 git 對齊）。18:59 派 Opus 窄審 WP7c diff。HC-22 狀態列與架構文件已補 PendingSends 段。
- 2026-09-18 19:15 WP7c 窄審完成，樹與審前逐位元相同；SG-1／DW-1 → 19:18 派 WP7d（grok high）。之後：Opus 只審 WP7d diff → `cargo build -p zed` → 總結。
- 2026-09-18 19:33 WP7d 完成；大腦 gate 487+187 綠、clippy 乾淨；快照 `after-wp7d-*.patch`。19:35 派 Opus 窄審 WP7d diff（SG-1 情境重跑、重複 pid、DW-1 mutation 驗證）。窄審綠即凍結程式碼 → `cargo build -p zed` → 總結。
- 2026-09-18 19:48 WP7d 窄審完成，程式碼凍結。大腦套用 DW-1 強斷言 patch，單測綠、整支 binary 10× 10 執行緒全綠。快照 `baseline/final-1948.patch` + `untracked-final.tgz`。最終驗收串跑中：fmt check → `cargo test` → clippy → `cargo build -p zed`（log `scratch/final-acceptance.log`）。channel assets 自 11:01 未動，node 34/34 仍有效。
- 2026-09-18 19:53 **最終驗收全綠**：`cargo fmt --check`（claude_sessions／remote）0 差異；`cargo test -p claude_sessions -p remote` 487（含 130 盲測）+ 187；`./script/clippy -p claude_sessions -p remote -p remote_server` 乾淨；`cargo build -p zed` dev 增量 1 分 59 秒成功；channel server node 34/34（assets 自 11:01 未動）。log `scratch/final-acceptance.log`，最終 diff 快照 `scratch/baseline/final-1950.patch`。**未 commit**（使用者未要求）。刻意不動：`crates/remote_server/src/server.rs`、`crates/tmux_sessions/src/tmux_sessions_panel.rs` 在 HEAD 就未格式化，已還原成 HEAD 原樣。

## 7. 後續（2026-09-22 起）

這份檔到此為止是 2026-09-18 那一階段的紀錄，**不再更新**。2026-09-22 使用者拍板轉向：放棄面板的訊息輸入框，打字回到 terminal，面板改成「內嵌 tmux 鏡像 attach 的 TerminalView ＋ 右側 rail ＋ 中間 gutter」。新的規格、事實清單、工作包與裁決全部在 `docs/claude-terminal-rail.md`；對應的架構文件與使用者文件（`docs/claude-sessions-architecture.md`、`docs/claude-sessions-setup.md`）已改寫成新架構。

上面那些 WP 裡，**下列部分作廢**（code 已刪，紀錄留著只為了追歷史）：

- WP3b 的「送訊息」半邊：store `send_message` 的 UI 呼叫、問題卡以組成文字回答、slash 三分類（純文字／帶參數／terminal 專用）。channel server、`answer_permission`、interrupt 那一半**保留**。
- WP7 的輸入框全套：`render_input`、`@` 檔案選單、`/` slash 選單、貼圖（`PasteIntoMessage`、`~/.claude/zed-pasted/`）、Up／Down 歷史、Esc `DismissMenus`、`SendMessage`／`PreviousMessage`／`NextMessage` actions 與三個平台的 keymap 綁定。
- WP7b／WP7c／WP7d 的 `PendingSends`（待送列、`/clear` 改綁、50 列上限、Failed 列、Retry）與 §3.4 R2-G1 起的所有 PendingSends 裁決。store 的 `take_cleared_rebinds()` 留著但只被 drain。
- §5「待使用者確認」兩條已由實測定案：dev-channel 確認框每次啟動都跳；`claude mcp add --scope user zed-claude -- node ~/.claude/zed-channel/server.mjs` 是正確做法。

其餘（hook dispatcher、statusLine wrapper、`LiveState`、五個 poll、registry rebind／ended row、channel 檔案協定、權限卡、Stop、entries／EntryCache／各種工具卡、盲測）沿用，且是新架構的地基。
