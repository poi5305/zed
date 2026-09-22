# Claude Session Message 健康檢查報告

- 日期：2026-09-18
- 對象：`crates/claude_sessions`（panel 14,621 行、store 2,990 行、source 986 行、transcript 1,047 行）、`crates/remote/src/claude_sessions.rs`（6,686 行）、`crates/remote_server/src/headless_project.rs` 的 11 個 `handle_*claude*`、`crates/proto/proto/claude_sessions.proto`
- 快照：HEAD `dd850ebeb7` + 未 commit 的 working tree（panel +604 行、remote +106 行，主要是 `SendUserFile` 支援）。所有 `file:line` 以 working tree 為準。
- 方法：五隻乾淨 context 的 reviewer 分區審（A 畫面刮取／B 輸入路徑／C 渲染覆蓋／D terminal 與生命週期／E remote 與輪詢，grok 4.6 xhigh ×2、Opus ×3），另一隻查官方文件；我再逐項用本機的 Claude Code 2.1.274、你的 `~/.claude/` 實際檔案、tmux 現場狀態與官方文件交叉驗證。文件 agent 的說法有多處錯誤，已全部改以官方頁面原文為準（見 §1.5）。
- 原始報告：`/private/tmp/claude-501/-Users-andy-go-src-github-com-poi5305-zed/1cd663f5-04b1-466b-8087-4df8816f6e7c/scratchpad/report{A,C,D}.md`；B、E 兩份被 harness 擋住不能寫檔，內容只在這份文件裡整合。

## 狀態（2026-09-18）

下表對照 §4 的 HC 編號，說明重構（WP1–WP5，實作與審查記錄在 `docs/claude-health-check-progress.md`）之後每一條的下場。**shipped** = 已落地且有測試；**partial** = 只做了一部分或靠別的機制間接解掉；**deferred** = 沒排進任何 WP，原因在括號內。使用者側的設定與操作請看 `docs/claude-sessions-setup.md`。

| HC | 狀態 | 做了什麼 |
|---|---|---|
| HC-01 | shipped | Saying 改讀 hook 事件流：`Stop` 清、transcript 追上清、`UserPromptSubmit` 清；`final`／`message_id` 都讀；panel 端再加 30 s + idle 守衛（WP1／WP4b） |
| HC-02 | shipped | `TerminalView` attach、mirror、resize handle 全刪；Zed 不再是 tmux client（WP1） |
| HC-03 | shipped | pane poll 整個刪除；每個讀取（transcript／events／status／channel／registry／agents）各自 loop、各自 timeout、各自 `ErrorSource`（WP1／WP5） |
| HC-04 | shipped | 新安裝器兩種引號都認、同 script 去重、先 parse settings 再寫；舊的兩個 hook 與資料夾一併移除（WP1） |
| HC-05 | shipped | transcript／subagent／events／status／channel／agents 全部 `with_timeout`，timeout 進各自的 `ErrorSource`，不再閃爍（WP5） |
| HC-06 | shipped | 鍵盤巨集刪除；問題改成組文字訊息經 channel 送出（WP3b） |
| HC-07 | shipped | `typing_answer_for`／`TickedAnswer` 刪除；Type something 改為預填輸入框（WP3b） |
| HC-08 | shipped | 不再送數字鍵，選項數量不再有上限（WP3b） |
| HC-09 | shipped | 問題卡一次顯示整個 call 的所有問題，結束靠 `PostToolUse`／`tool_result`／`Stop`；`pending-questions/` 檔不再產生（WP1／WP3b） |
| HC-10 | shipped | `show_tool_calls` 的刪除濾鏡換成每輪摘要（`TurnSummary`）+ now row；工具卡是摺疊不是刪除（WP4b） |
| HC-11 | shipped | `permission-mode`／`ai-title`／`bridge-session`／`total_tokens_reminder`／`auto_mode`／`cost-state` 在 `absorb` 時快取並各有 accessor（WP1／WP4a） |
| HC-12 | shipped | 有 `content` 的 system subtype 畫成 `SystemNote`（`away_summary` 標「While you were away」、`bridge_status` 帶白名單連結）；`turn_duration` 畫成 `TurnFooter`（WP4a） |
| HC-13 | shipped | attachment 依 turn 摺疊、放在該輪 user 訊息之後；tokens-left／auto-mode 提到工具列（WP4a） |
| HC-14 | shipped | 權限模式改讀 `permission-mode` 記錄與 hook payload；BTab 按鈕刪除（WP1） |
| HC-15 | shipped | `SentFiles` 帶 `is_error`，失敗標紅附結果文字（WP4a） |
| HC-16 | shipped | 非圖片路徑列是按鈕：本機用 OS 開、remote 先下載；受 `attachment_is_readable` 邊界（WP4a／review） |
| HC-17 | shipped | 4 MiB 以上改「Load anyway」按鈕，硬上限 64 MiB（WP4a） |
| HC-18 | shipped | `tool_target` 對所有工具有 fallback 欄位順序；mcp 名稱拆 server／tool（WP4a） |
| HC-19 | shipped | 身分改 `sessionId`；process 消失變 ended row 保留 transcript，同 id 新 pid 自動重綁；Resume in background（WP5） |
| HC-20 | shipped | Saying 用 markdown、上緣拖曳、3 rem–60% 夾限、跟隨改用內容 identity（WP4b） |
| HC-21 | shipped | slash 三分類：純文字送、帶參數的沒參數不送、只在 terminal 的標記且不送（WP3b） |
| HC-22 | shipped | 送失敗的 pending 列保留原文，變成 Failed 狀態附 **Retry**（同一段文字重送）／**Copy**／關閉；不再依賴輸入框是否為空（WP7）。列屬於它的 session：切 session 隱藏不刪、`/clear` 改綁新 sessionId、已結束的列最多留 50 條、session 消失後的 in-flight 列標成 Failed（WP7b／WP7c） |
| HC-23 | shipped | 貼圖錯誤有自己的 `paste_error` 欄位、畫在輸入框下方；打字或下一次貼檔成功即清除，不再共用 `hook_install_note`（WP7） |
| HC-24 | shipped | `ListClaudeSessionFiles` 改帶 `session_id`，目錄必須在 session cwd 內；slash 的 `project_root` 同樣邊界（WP5） |
| HC-25 | shipped | `@` 選單只有一份列表：`offered_files` 先截到 `FILE_MENU_ROWS`，高亮、繪製、Enter 都吃同一份（WP7） |
| HC-26 | shipped | `@` 列表 150 ms debounce（GPUI timer，每鍵取消前一個）；等 host 回應期間用上一批結果本地縮窄（前綴優先再子字串）；單獨 `@` 不開選單、不發 RPC（WP7） |
| HC-27 | shipped | `PendingSends` 以 outbox 檔名配對 `message_sent`（delivered），60 s 沒進 transcript 顯示 waiting 而不是永遠 Sending；等待以本機觀察時間計（WP3b／review） |
| HC-28 | shipped | pane poll 已刪除，問題消失（WP1） |
| HC-29 | shipped | 一次 `ListClaudeSubagentsForSessions` RPC；conversation 快取改增量（整行為單位、lossy UTF-8）、每次掃描修剪（WP5／review） |
| HC-30 | shipped | `TailState.pending` 4 MiB 上限，超過丟棄並在下一個換行重新同步（WP5；殘留見 wontfix WP5R1-8） |
| HC-31 | shipped | 貼檔命名 `pasted-<ms>-<6 hex SHA-256>.<ext>`；在 session cwd 下插 `@相對路徑`，否則絕對路徑；每次寫檔順帶清掉 `~/.claude/zed-pasted/` 直屬 7 天以上的一般檔，不跟 symlink、不進子目錄（WP7） |
| HC-32 | shipped | `DismissMenus` 同時綁在 `ClaudeSessionsPanel` 與 `ClaudeSessionsInput > Editor`（三份 keymap），輸入框收合時 Escape 也能關選單／問題卡焦點（WP7） |
| HC-33 | shipped | `quit_armed` 刪除；背景 session 的 Stop 用 5 s 自動解除的 Confirm stop（WP5） |
| HC-34 | shipped | 版面只有對話是 flex，Saying 有上限先縮、輸入框 1–8 行；dock 列表 `size_full` 可捲（WP4b） |
| HC-35 | shipped | `hooks_installed` 改獨立 task：啟動問一次，之後最多每 30 s 一次；Install／Uninstall 成功後立刻補問一次（WP7） |
| HC-36 | shipped | `scan_in_flight` + watchdog：前一個 registry scan 未回就不重發，改亮 stale chip（WP5） |
| HC-37 | shipped | slash 指令改以 session cwd 為根（`list_slash_commands_for`、`slash_command_project_root`）（WP5） |
| HC-38 | shipped | Up 歷史 = pending sends（新→舊）＋ `Transcript::full_path()` 的使用者訊息與 slash 指令；連續重複合併、上限 200（WP7） |
| HC-39 | shipped | 切 session 時清 `file_matches`／`file_matches_for`／進行中的 `@` 查詢／兩個 dismissed 欄位／高亮／slash 列表快取，並以新 session 的 `session_directory()` 重列 slash；焦點回來時同 cwd、每 session 30 s 內不重問（WP7） |
| HC-40 | shipped | mirror session 不再產生；`is_zed_mirror_session` 只留作 tmux 列表過濾（WP1／WP3b） |
| HC-41 | shipped | 面板在 hooks 已裝時顯示 **Uninstall hooks**；新 `SessionSource::uninstall_hooks` + proto `UninstallClaudeHooks`；host 跑既有 `uninstall_zed_hooks`（移除 hooks 條目與 `server.mjs`，不碰 `~/.claude.json`）；成功句提醒手動 `claude mcp remove zed-claude`（WP7） |
| HC-42 | shipped | `ErrorSource::Send` 在選取變更、`/clear` 換 session、兩條 process 消失路徑都清除（WP7） |
| HC-43 | shipped | tool result 的 image block 變 `EntryKind::Image`，其餘 block 套 `without_base64_payload`（WP4a） |
| HC-44 | shipped | Edit 用 `similar` 畫 `-`／`+` diff，結果有 `structuredPatch` 時只畫 patch；diff 進 `EntryCache`、128 KiB 上限（WP4a／review） |
| HC-45 | shipped | proto `ClaudeSession.bridge_session_id`；Open in claude.ai 在列、ended row、工具列都有，URL 過 `bridge_url` 白名單（WP5／review） |
| HC-46 | shipped | `set_visible(false)` 時所有 poll 降到 5 s，不停止（WP5） |
| HC-47 | shipped | `ListClaudeSessionsResponse.liveness_unavailable_reason`：非 unix host 回一句原因，面板空清單時畫成灰字提示而不是無聲空白（WP7） |
| HC-48 | shipped | 角色由 `promptSource`／`origin.kind` 決定：peer 畫成摺疊「Message from …」卡、task-notification 進背景工作卡、system 進 Context；無欄位的舊記錄以開頭文字判斷（WP4a／review） |

另外原表沒有編號但已完成的：抱怨 12 的 context 進度條（statusLine `used_percentage`＋`context_window_size`）、抱怨 13 的無 terminal box、§8.7 的 shell 小弟卡、§6.2 的 Zed channel（含 Interrupt：一次 SIGINT、3 s 節流）、§9.3 的 `claude agents --json` 背景 session。

---

## 0. 一頁結論

### 要不要放棄 panel、直接開網頁？

**不用放棄，但要換掉一根柱子。** 三個理由：

1. **你抱怨的 13 件事裡有 10 件是同一個根因**：panel 把 tmux 畫面（`capture-pane`）當成控制平面來讀，把 `send-keys` 當成控制平面來寫。Saying 卡住、preview 卡住、auto mode 不同步、問題答完不更新、terminal 縮小整個爛掉、更新 CLI 就消失，全部是這根柱子的副作用。拔掉它不是重寫，是刪 code（remote 層約 350 行、panel 一整組 parser）。
2. **panel 缺的東西，12 件裡有 10 件早就在 transcript 裡了**，只是沒人讀：permission mode（`permission-mode` 記錄）、session 標題（`ai-title`）、官方網頁 URL（`bridge-session`）、token 預算（`total_tokens_reminder`）、pending agent 數與 turn 時長（`system/turn_duration`）、離開時的摘要（`away_summary`）、todo list（`TodoWrite` 輸入）、context 用量（`message.usage`）。剩下兩件（串流中的字、正在等的權限／問題）官方 hooks 都有訊號。
3. **官方在 2.1.2xx 補齊了無 terminal 的整套生命週期與控制通道**：`PermissionRequest` hook 可以直接回 allow／deny、`Stop` hook 帶 `last_assistant_message`、`Notification` hook 有 `permission_prompt`／`idle_prompt`、`statusLine` 每次 API 回應餵一份含 `context_window.used_percentage` 的 JSON、Channels（research preview）讓一個 MCP server 把訊息 push 進 session 並接權限核准、`claude --bg`／`attach`／`respawn`／`agents --json` 讓 session 不靠 tmux 也能活過 CLI 更新。

**如果你只想要最省的版本**：做「方案 Z」（§6.9）——panel 退回原始 spec 的 P1（唯讀 transcript + 列表），每個 session 一顆「在 claude.ai 開啟」按鈕（URL 已在 registry 裡），輸入全部交給官方網頁。零 terminal、零 hook、零 send-keys，一週內可完成，而且比現在穩。

### 根因一句話

現在的 panel 有三個互不同步的時鐘：transcript（250 ms）、hook 側檔（1 s，且被綁在 tmux 之後）、tmux 畫面（1 s，無 timeout）。UI 把第三個當成真相來源，而第二個沒有「結束」訊號。

### 推薦路線

| 階段 | 內容 | 目標 |
|---|---|---|
| **Phase 0（1–2 天）** | 修 hook 重複安裝；不再 attach tmux client（刪 `TerminalView`，保留 14 行 capture 預覽或直接刪）；「在 claude.ai 開啟」按鈕 | 立刻止血：terminal 不再壓扁你的 CLI；互動式指令有出口 |
| **Phase 1（1–2 週）** | 讀取面全部改成 transcript + hooks + statusLine（§7）；session 身分改用 `sessionId`（§9）；最新 tool call 常駐列 + 每輪摘要（§8.2）；context 進度條（§8.4）；修 §4 的 High 以上 bug | 不讀 terminal；panel 顯示量追平網頁版 |
| **Phase 2（決策點）** | 寫入面換掉 `send-keys`：首選 **Zed channel**（§6.2），過渡期只留「純文字 + Esc」兩種 send-keys 並用 hook 狀態守門 | 不控 terminal；互動式指令有結構化答案 |

---

## 1. 已驗證事實

### 1.1 Claude Code 2.1.274 的資料面（本機實測）

| 事實 | 驗證方式 |
|---|---|
| **`permission-mode` 記錄**：`{"type":"permission-mode","permissionMode":"auto"}`，40 份最近 transcript 共 1,161 筆（`auto` 1,158、`bypassPermissions` 3）。**這才是權限模式。** | 掃 `~/.claude/projects/*/*.jsonl` |
| **`mode` 記錄不是權限模式**：1,795 筆全部是 `"normal"`，即使同一 session 的 hook payload 說 `permission_mode: auto`。不要用它。 | 同上 + 對照 `pending-questions/<id>.json` |
| `attachment/auto_mode`：`{"autoModeConsentFlow":false,"bashFirst":true,"bashFirstSteer":"strict","steerOnly":true,"bypass":false}`，有 uuid、在 path 上 | 本 session transcript |
| 每筆 assistant 記錄有 `message.usage`：`input_tokens`、`cache_creation_input_tokens`、`cache_read_input_tokens`、`output_tokens`。三者相加 = 目前 context 用量（官方 statusLine 文件明寫 `used_percentage` 用同一個公式，不含 output） | 本 session transcript + statusline.md |
| 無 uuid 的記錄型別（永遠到不了 `active_path`）：`mode`、`permission-mode`、`ai-title`、`last-prompt`、`bridge-session`、`atis-latch`、`queue-operation`、`cost-state`、`file-history-*` | Reviewer C 掃 30 份 |
| `system` subtype 實測：`compact_boundary` 7、`local_command` 16、`turn_duration` 436（含 `pendingBackgroundAgentCount`、`durationMs`、`messageCount`）、`away_summary` 7、`bridge_status` 30（含 `url` = 官方網頁）、`stop_hook_summary` 68、`informational` 2 | Reviewer C |
| registry `~/.claude/sessions/<pid>.json` 的 `bridgeSessionId`（例 `session_01FqC4PM…`）就是 `https://claude.ai/code/<id>`；remote-control.md 原文：「The ID is the part of the session's URL at claude.ai/code between `/code/` and any `?`」 | 本機 registry + 官方文件 |
| `claude agents --json` 對 interactive 與 background 都列；interactive 有 `pid`、`status`（busy／waiting／idle）；文件說 `status: waiting` 時附 `waitingFor`：`permission prompt`／`input needed`／`sandbox request`／`dialog open` | 本機執行 + agent-view.md |

### 1.2 你機器上的 hook 現況（直接證據）

`~/.claude/settings.json` 裡 Zed 裝的兩個 hook **各被登錄兩次**：

```
PreToolUse   AskUserQuestion  "/Users/andy/.claude/hooks/record-pending-question.sh"
PreToolUse   AskUserQuestion  "'/Users/andy/.claude/hooks/record-pending-question.sh'"
MessageDisplay -              "/Users/andy/.claude/hooks/record-live-message.sh"
MessageDisplay -              "'/Users/andy/.claude/hooks/record-live-message.sh'"
```

原因：`settings_name_the_hook`（`crates/remote/src/claude_sessions.rs:1606`）只認單引號包起來的 command 字串，舊版裝的是沒引號的，所以第二次安裝又 push 一份。後果：每個 `MessageDisplay` payload 被寫進 `~/.claude/live-messages/<session>.jsonl` **兩次**（本 session 的檔案：`indexes 0,0`），Saying 的組裝邏輯（`assemble_live_message`，`:1390`）看到的是重複片段。

`MessageDisplay` payload 實際欄位（本機檔案原文）：`session_id`、`transcript_path`、`cwd`、`scratchpad_dir`、`prompt_id`、`hook_event_name`、`turn_id`、`message_id`、`index`、**`final: true`**、`delta`。Rust 端的 `LiveMessagePiece`（`:1348–1357`）只認 `index`／`delta`／`message_id`，**沒有用 `final` 與 `turn_id`**。

`~/.claude/pending-questions/*.json` 從 9/13 到現在 13 個檔全部還在，沒有任何東西刪它；`live-messages/` 20 個檔同樣。

### 1.3 tmux 現場（直接證據）

```
tmux 3.6b · window-size latest · aggressive-resize off
/dev/ttys011: zed-claude-mirror-76859-52 [151x20]  (attached,focused)
/dev/ttys005: poc                       [158x52]  (attached,focused)
```

現在就有一個 Zed 掛的 mirror client 以 **20 列** attach 在同一個 window 上。`window-size latest` 的語意是「最近一個有動作的 client 決定 window 大小」，所以 Zed 那個 20 列的 client 一被 focus，整個 window（含你 iTerm 裡的 CLI）就被壓成 20 列。這就是抱怨 3 的全部機制。

### 1.4 官方文件（逐頁抓原文驗證）

**Hooks**（`code.claude.com/docs/en/hooks.md`）— 32 個事件，跟這件事有關的：

| 事件 | 給什麼 | 對 panel 的意義 |
|---|---|---|
| `PermissionRequest` | 輸入 `tool_name`、`tool_input`、`tool_use_id`、`permission_mode`；**輸出可回** `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"\|"deny","message":"…"}}}`；command hook 預設 timeout 600 s | Zed 可以不碰 terminal 就核准工具權限 |
| `Notification` | `notification_type`：`permission_prompt`、`idle_prompt`、`agent_needs_input`、`agent_completed`、`elicitation_*`…，含 `title`、`message` | 「正在等權限」「已 idle」有正式訊號 |
| `Stop` / `SubagentStop` | 含 `last_assistant_message`（文件明寫：需要最終文字的 hook 用這個，不要讀 transcript） | Saying 的「結束」訊號 |
| `PreCompact` / `PostCompact` | matcher `manual`／`auto` | compact 進度與分界 |
| `PreModelSwitch` / `PostModelSwitch`、`ConfigChange` | — | model／設定變動 |
| `MessageDisplay` | 現在用的；預設 timeout 只有 10 s；無 matcher | 串流文字 |
| `UserPromptSubmit`、`PreToolUse`（任何工具）、`PostToolUse`、`TaskCreated`／`TaskCompleted`、`SessionStart`／`SessionEnd` | 所有事件都帶 `session_id`、`permission_mode`、`cwd`、`transcript_path` | 「現在在跑哪個工具」「turn 開始」 |

**statusLine**（`statusline.md`）：每次 API 回應後 300 ms debounce 執行一次你設定的 command，stdin 給 JSON，含 `model.id/display_name`、`context_window.{total_input_tokens,context_window_size,used_percentage,remaining_percentage,current_usage}`、`cost.*`、`effort.level`、`rate_limits.{five_hour,seven_day}`、`session_id`、`transcript_path`、`exceeds_200k_tokens`。**compact 進度條的分母（`context_window_size`）就在這裡。**

**Channels**（`channels.md`、`channels-reference.md`，research preview）：一個 MCP server 宣告 `capabilities.experimental['claude/channel']` 就能用 `notifications/claude/channel {content, meta}` 把事件 push 進正在跑的 session；宣告 `claude/channel/permission` 後會收到 `notifications/claude/channel/permission_request {request_id, tool_name, description, input_preview}`，回 `notifications/claude/channel/permission {request_id, behavior: allow|deny}`。訊息以 `<channel source="…">` 標籤進入模型 context；session 啟動時要帶 `--channels plugin:…` 或 `--dangerously-load-development-channels server:<name>`（後者會跳一次確認）。AskUserQuestion **不會**被 relay，只有 tool permission 會。`-p` 模式下 AskUserQuestion 等需要 terminal 的工具會被關掉。

**Cross-session inbox socket**（`cross-session-messaging.md`）：文件明寫「when you want a script or hook to post into a session」可以連 `CLAUDE_CODE_MESSAGING_SOCKET`（`/tmp/cc-socks/<pid>.sock`），第一行可送 `{"type":"auth","token":…}`（macOS 選填）。**但**送進去的訊息會被標成「另一個 session 說的」：不能核准權限、`/compact` 等指令不會執行、收件 session 若是 bypassPermissions 會先 **hold 等你在 terminal 核准**。訊息本體的 JSON 格式沒有文件化。

**Remote Control**（`remote-control.md`）：沒有第三方客戶端 API，只有 claude.ai／手機 app。網頁能做：送訊息、核准權限、答 AskUserQuestion、切 model／effort、看 compact 進度、看 subagent／workflow、看 uncommitted diff。網頁**不能**做的指令：`/plugin`、`/resume` 等只在 terminal 的；能以參數形式做的：`/model <name>`、`/effort <level>`、`/config key=value`、`/autocompact <n>`、`/output-style <s>`、`/advisor <m>`、`/mcp` 子指令、`/compact`、`/clear`、`/context`、`/usage`、`/exit`。**這些帶參數的形式在 interactive terminal 也有效**——也就是 panel 現在把 `/model` 當純文字送出去會開 picker，但送 `/model sonnet` 不會。

**背景 session**（`agent-view.md`）：`claude --bg`；`claude attach <id>` 是把 TTY 接回同一個 process（不是 tmux，是 Claude 自己的 supervisor）；`claude respawn <id>|--all`「重啟 session 以套用新 binary，恢復已存的對話」；文件明寫「Session state persists on disk through auto-updates and supervisor restarts」、「After an auto-update: the supervisor restarts itself onto the new version and moves idle sessions over in the background」；agent view 的 peek panel 可以直接對背景 session 打字送出，但這條 IPC 沒有公開協定。`-p` 與 `--bg` 互斥。

**Headless**（`headless.md`）：`claude -p --resume <sessionId>` 是**開一個新 process 續同一份對話檔**，不是接上正在跑的那個；`--continue` 甚至明寫只接「已結束」的背景 session。所以它不能當成「對 tmux 裡那個 session 說話」的通道。stream-json 的 stdin 控制訊息格式官方沒寫（GitHub issue #24594 承認）。transcript JSONL 格式官方明寫是 internal、會跨版本變。

### 1.5 更正（誰說錯了什麼）

| 錯誤說法 | 事實 |
|---|---|
| 我一開始寫給 reviewer 的「`mode` 記錄就是 permission mode」 | 錯。`mode` 永遠 `normal`；permission mode 在 **`permission-mode`** 記錄。Reviewer A 的 B4、Reviewer E 最後一段引用了我的錯誤前提，結論方向仍對，來源名稱要改。 |
| 文件 agent：「沒有 `PermissionRequest` hook、沒有 `PreCompact`、statusLine 沒有 schema」 | 三個都錯，官方頁面都有，原文見 §1.4。 |
| Reviewer A／E：「`MessageDisplay` 沒有訊息結束訊號」 | 半對。每則訊息有 `final: true`，但 **turn** 結束沒有；`Stop` hook 才是 turn 結束。 |
| Reviewer D／E：「`claude -p --resume` 可能是 attach」 | 不是，是 fork 一個新 process。 |
| panel 測試 `claude_sessions_panel.rs:8559–8561` 宣稱「footer 是唯一寫出 mode 的地方」 | 錯，見 `permission-mode` 記錄。 |

---

## 2. 根因：三個時鐘與錯位的控制平面

```
transcript (250 ms) ─┐
                     ├─► entries / activity / queue / spend     ← 對話內容，正確
hook 側檔 (1 s) ─────┤   live_message / recorded_question       ← 沒有「結束」，且排在 tmux 之後
tmux 畫面 (1 s) ─────┘   permission_mode / question index / 「Ready to submit?」 / 14 行 mirror
                          ↑ 無 timeout；失敗就清空；被 Zed 自己的 client 壓扁後 parser 全失效
send-keys ◄────────────── 純文字 paste、Escape、BTab、C-c、C-d、數字、Up/Down ×N、Enter
                          ↑ 不知道 pane 現在是不是在 prompt；把答案當 120 ms 一鍵的巨集
```

- **`activity()`（`claude_sessions_panel.rs:6071`）在整個當前 turn 都是 Idle**：assistant 記錄要等 turn 結束才寫進 transcript，所以「正在跑 Bash」永遠是上一輪的事。這是抱怨 9 的根源，也是為什麼 Saying 變成唯一的「它在動」訊號。
- **Saying 的隱藏條件是「最近 8 筆 assistant Message 之一與 hook 檔內容逐字相等」**（`:3386–3400`）。tool-only 的 turn、被 markdown 重排的文字、超過 8 筆的情況，全部不會隱藏。hook 檔只在下一則訊息 `index==0` 時清空；session idle 就永遠掛著。
- **pane poll 是 `capture_pane().await` → `pending_question().await` → sleep**（`session_store.rs:571–608`），兩個都沒 timeout。tmux 卡住 → Saying、問題、mode 全部凍結；capture 失敗 → `.ok()` 清空畫面（`:584`）但 `pending_question` 失敗 → 保留舊值（`:588`, `:616`）。**相鄰兩行、相反的失敗策略。**
- **答問題是鍵盤巨集**（`send_answer :2213`、`keys_for_several_choices :6419`）：勾選 → 按 `option_count + 3` 次 Down → Up → Enter，假設 CLI 選單下方永遠恰好三列。中途被取消（再點一次、切 session）就留下半勾選的選單。第 10 個選項以後 `Digit::for_option` 回 `None`，按鈕變成無聲的 no-op。

---

## 3. 你的 13 條抱怨 → 診斷

| # | 抱怨 | 根因 | 對應 bug | 解法（章節） |
|---|---|---|---|---|
| 1 | Saying 卡住不消失 | 無 turn 結束訊號 + 逐字比對 + pane poll 凍結 + hook 重複寫 | HC-01, HC-03, HC-04, HC-05 | §7 用 `Stop` hook 清；hook poll 脫離 tmux |
| 2 | preview 卡住、不能調大小、UX 差 | 同上；`Label` 不是 markdown；固定 10 rem 無把手 | HC-01, HC-20 | §8.3 |
| 3 | terminal 縮小 UI 爛掉 | Zed attach 了一個真 tmux client，`window-size latest` 讓 20 列的它決定整個 window | HC-02 | Phase 0 刪 attach |
| 4 | 輸入框選項出錯、問完不更新 | `typing_answer_for` 寫了沒讀；巨集可被中途取消；10+ 選項無聲失敗；pending-questions 檔永不刪；`call_is_answered` 要等整個 call 結束 | HC-06, HC-07, HC-08, HC-09 | §6 結構化答案；過渡期只顯示不點 |
| 5 | Message view 顯示太少 | `show_tool_calls=false` 直接**刪除**（不是折疊）所有 ToolUse/ToolResult/Thinking；uuid-less 記錄到不了 path；attachment 全塞一個 Context 摺疊區 | HC-10, HC-11, HC-12, HC-13 | §5、§8.2 |
| 6 | auto mode 不同步 | 從 footer 文字 parse；capture 失敗就清空；BTab 盲送 | HC-03, HC-14 | 讀 `permission-mode` 記錄；刪 BTab |
| 7 | 漏東漏西最後還是開 tmux | 上述總和 + 沒有網頁出口 | — | Phase 0 網頁按鈕；§5 |
| 8 | SendUserFile 沒顯示 | HEAD 沒有 `SentFiles`；tool_result 被 `show_tool_calls` 濾掉。working tree 已補，但失敗的送檔畫成成功、mp4 無法開、>4 MB 拒載 | HC-15, HC-16, HC-17 | §4 |
| 9 | 要開 show tool calls 才知道它在做事 | `activity()` 當前 turn 永遠 Idle；tool entries 被刪不是折疊 | HC-10, HC-18 | §8.2「now row + 每輪摘要」 |
| 10 | 更新 CLI／exit 整個消失 | store 以 **pid** 為身分；pid 消失 → 清掉整份 transcript、選取、tail | HC-19 | §9 改 `sessionId` 身分 + ended row + Resume |
| 11 | 互動式指令不能用 | 純文字送 `/model` 開 picker，panel 看不到也關不掉；下一則訊息打進 picker | HC-21 | §8.6 帶參數形式 + 標記 + 網頁出口 |
| 12 | compact 進度條 | 分子（`context_tokens()`）已算，分母沒來源 | — | §8.4 statusLine `context_window_size` |
| 13 | 不要 terminal box | — | — | §6、§7 |
| + | shell 小弟（grok／agy）在做什麼看不到 | 背景 Bash 只剩一張 task 通知 | — | §8.7 |
| + | subagent 回報／系統通知被畫成「You」 | 角色只看 `type: "user"`，沒看 `origin.kind`／`promptSource` | HC-48 | 角色改由 `promptSource` 決定；peer 訊息畫成摺疊卡片 |

---

## 4. Bug 清單（合併五份報告、去重、依嚴重度）

編號 HC-xx；來源標 `[A-B1]` = Reviewer A 的 B1。行號以 working tree 為準。

### Critical

**HC-19 · process 一消失就毀掉整份對話** `[D-B1][E-B1]`
`session_store.rs:836–853`、`:896–913`。registry 掃描不再列出選取的 pid → `selected_process_id=None` → 主對話走 `follow_this_sessions_main_conversation` → `Transcript::new()` 換掉整棵樹、`followed_session_id=None`、不再 tail。jsonl 還在磁碟上，UI 卻畫「Select a session」。subagent tab 反而保留記錄（`:838–848`），且有測試；主對話**沒有**測試。觸發：CLI 自動更新、`exit`、`claude respawn`、crash。
修法：身分改用 `sessionId`，pid 只是屬性；「process 走了」與「`/clear` 換對話」分成兩條路，前者只清 pane／question／live 並停 tail、保留所有記錄，畫成 ended row（§9）。

### High

**HC-02 · 展開 terminal = attach 一個真 tmux client，把你的 CLI 壓扁** `[A-B2][D-B2]`
`toggle_pane_mirror :1686–1688`、`sync_terminal :1612`、`attach_arguments` → `remote/src/claude_sessions.rs:2365–2418`（`new-session -d -t =<session> … ; attach-session`）。沒有 `ignore-size`、`aggressive-resize`、`resize-window`、`-x/-y`。`terminal_height` 預設 260 px、最小 80 px（`:713–720`）。grouped session 只解決「不搶你目前的 window」，不解決尺寸。registry 沒 `@window` 時 fallback 直接 `attach -t %pane`（`:2367–2370`）連 group 都沒有。§1.3 有現場證據。
修法：不要 attach。要預覽就用既有 `capture-pane`（`render_pane_mirror :3826`，不是 client）。

**HC-01 · Saying 沒有結束條件** `[A-B1][E-B4]`
隱藏條件 `claude_sessions_panel.rs:3376–3400`（非 Main／空字串／最近 8 筆 assistant Message 逐字相等）。hook 只在 `index==0` 清檔（`remote/src/claude_sessions.rs:1515–1523`）。`LiveMessagePiece` 沒讀 `final`／`turn_id`；`#[serde(default)] index` 讓缺 `index` 的 payload 全變 0，`rposition(index==0)` 只剩最後一片。
修法：`Stop` hook 寫「turn 結束」→ 清；或 transcript 出現比 hook `message_id` 更新的 assistant 記錄→ 清；加 mtime 上限；比對用前綴不用 `==`。

**HC-03 · pane poll 無 timeout、序列化、失敗策略相反** `[A-B6][E-B3][E-B5]`
`session_store.rs:584`（`capture_pane().await.ok()` → `None` → **清空**）、`:588`（`pending_question().await.log_err()` → `None` → **保留**）。tmux subprocess（`capture_tmux :2668`、`run_tmux :2689`、`run_with_stdin :2712`）也無 kill deadline。tmux 卡住 → live／question 永遠凍結；capture 偶爾失敗 → permission 標籤每秒閃一下（抱怨 6）。
修法：兩個讀取分開跑、各自 timeout；失敗保留舊值並標 stale；不需要 tmux 的資料不要排在 tmux 後面。

**HC-04 · hook 重複安裝** `[本機證據]`
`settings_name_the_hook :1606` 只比對 `shell_quote` 後的字串；舊版寫的是未加引號的路徑 → 每次「已安裝」判定為 false → 再 push 一份。每個 payload 寫兩次。
修法：比對時把兩種寫法都當作已安裝；安裝時先 dedupe 同 matcher 同 script 的 entry；順便處理 E-S3（merge 進既有 matcher 而不是 push 第二個）。

**HC-05 · transcript poll 與 subagent scan 無 timeout** `[E-B2][E-B6]`
`session_store.rs:692/:699`（tail）、`:549`（`list_subagents_for_sessions`）。只有 `list_sessions`（`:519`）有 `REGISTRY_SCAN_TIMEOUT`。一個 RPC 不回 → 對話凍結、`/clear` 偵測停、session 消失偵測停，**沒有錯誤 banner**。remote 上 `list_subagents_for_sessions` 是 N 個 request 串行（`session_source.rs:389`），任何一個卡住就全卡。
修法：一律 `with_timeout`；timeout 轉成 `ErrorSource::Transcript`；subagent scan 給自己的 `ErrorSource`（E-B7：現在跟 registry 共用 `Poll`，成功清、失敗設，每秒閃兩次 notify）。

**HC-06 · 答問題是可被中途取消的鍵盤巨集** `[A-B3][B-B2][B-B12]`
`send_answer :2213–2233`（`_answering` 被替換就取消）、`keys_for_several_choices :6419–6440`（假設選項下方恰好 3 列）、`ANSWER_KEY_INTERVAL` 120 ms（remote 上每鍵一個 RPC，延遲累加）。取消後 `ticked` 已清，下一次點擊從錯的游標位置重算。
修法：結構化答案（§6）；過渡期：`_answering` 期間停用所有答題控制；不做 Down-walk；送 Enter 前 capture 確認高亮列。

**HC-07 · `typing_answer_for` 寫了從來沒讀** `[A-B11][B-B6]`
`:806` 宣告、`:2203` 設定、`:1596` 清除，讀取只在測試。按「Type something」後卡片仍畫所有選項為可點按鈕；再點選項會把數字打進文字框。這是抱怨 4 最直接的形狀。

**HC-08 · 第 10 個選項起無聲失敗** `[A-B3][B-B7]`
`Digit::for_option`（`remote/src/claude_sessions.rs:2566–2579`）只到 9；`pick_option :2123–2128` `let Some(..) else { return }`；`keys_for_own_words :6396` 用 `option_count` 當列號，所以 **9 個選項就足以讓「Type something」死掉**。
修法：畫不出來就不要畫成可點；超過 9 個顯示「請在 terminal／網頁回答」。

**HC-09 · 問題答完 panel 不更新** `[A-B3][B-Q4]`
`pending-questions/<id>.json` 永不刪（`read_pending_question :1420–1424` 刻意不過期）；`call_is_answered :6724` 要等整個 `AskUserQuestion` call 的 `tool_result`；你在 terminal 答了第一題，panel 還畫第一題直到整個 call 結束。Ctrl+C 中斷的 call 是否會寫 `tool_result` 未知（若不會，卡片卡到下一個問題覆寫）。
修法：一次顯示 call 的**全部**問題（像網頁），不追蹤游標；`Stop`／`Notification` hook 到就收。

**HC-10 · 預設視圖刪掉所有工作證據** `[C-BUG-1]`
`show_tool_calls=false`（`:1555`）+ `:2363–2371` 的 `retain` **刪除** ToolUse／ToolResult／Thinking。spec §5 寫的是「折疊卡片」。40 個 Bash、4 分鐘的 turn 在預設視圖是「4 分鐘空白然後一句話」。
修法：§8.2。

**HC-14 · permission mode 從 footer 文字 parse，且 BTab 盲送** `[A-B4]`
`permission_mode :271–288` 找 `(shift+tab to cycle)` 同一行；換行就 `None` → 標籤退成「Permission mode」，按鈕仍送 BTab 到不知道在什麼狀態的 TUI。
修法：讀 `permission-mode` 記錄（掃 `records`，如 `spend()`）；刪 cycle 按鈕或改為唯讀，等結構化通道。

**HC-21 · 互動式 slash 指令是單向門** `[B-B5][B-§4]`
`can_send :3601–3604` 只看有沒有 pane id；`/model`、`/config`、`/resume`、`/agents`、`/export` 在內建清單裡有友善描述（`remote/src/claude_sessions.rs:960–975`），送出去開 dialog 後 panel 看不到，下一則訊息會被 paste 進 picker 的過濾框，**Enter 會啟動 picker 高亮的那一列**（例如換掉 model）。
修法：§8.6。

**HC-22 · 送失敗會遺失訊息** `[B-B3]`
`send_message :3659–3660` 立刻清空輸入框；失敗時只有輸入框「仍為空」才放回（`apply_send_outcome :5740–5749`）；pending row 同時被 `resolve→dismiss` 拿掉（`:5831–5836`）。pane 剛消失 + 你在等待時打了字 → 訊息不存在任何地方。
修法：失敗的 pending 留在畫面標 failed，附 Retry／Copy。

**HC-23 · 貼圖失敗寫到 dock 才會畫的欄位** `[B-B4]`
`paste_into_message :1909–1913` 把錯誤寫進 `hook_install_note`，那個欄位只在 `render_session_section :2845–2852`（dock）畫；貼圖發生在 tab。>32 MB、名稱被拒、`~/.claude` 不可寫、連線斷 → **完全沒回饋**。

**HC-48 · 系統注入的訊息被畫成「You」** `[你 09-18 截圖][本機證據]`
subagent 回報（`<agent-message from=…>`）、背景工作完成通知（`<task-notification>`）、`[SYSTEM NOTIFICATION]` 都以 `type: "user"` 寫進 transcript，panel 的 `message_kind :7547` 只看 `type` 就給 `MessageRole::User`，`INJECTED_WRAPPERS :102` 只剝掉 `system-reminder`／`task-notification` 等 6 種標籤的外殼，剝不掉的（`agent-message`、`Another Claude session sent a message:` 前導句、`[SYSTEM NOTIFICATION…]`）就整段當你打的字。本 session 實測這些記錄有明確欄位可判：

```
你打的：      origin.kind = "human",             promptSource = "typed" | "queued"
subagent 回報：origin.kind = "peer",              promptSource = "system", isMeta = true
task 通知：    origin.kind = "task-notification", promptSource = "system"（沒有 isMeta！）
```

`isMeta` 過濾（`:7366–7376`）只擋得住前者；截圖裡那則就是沒有 `isMeta` 的 task-notification，同一筆還把 Reviewer E 的整份報告帶進來。
修法：角色改由 `promptSource`／`origin.kind` 決定（沒有這兩個欄位的舊記錄才退回 `type`）；`origin.kind == "peer"` 畫成「來自 <name> 的訊息」摺疊卡片（像 CLI 的 `› Message from @… (ctrl+o to expand)`）；`task-notification` 併進 §8.7 的背景工作卡片；`[SYSTEM NOTIFICATION]` 進 Context 區。

**HC-24 · `handle_list_claude_session_files` 沒驗任何邊界** `[E-B8]`
`headless_project.rs:1553–1567` 把 wire 上的 `directory` 直接餵 `list_files_under`（深 8 層、2 萬 entry）。同連線其他 handler 都有邊界（`validate_claude_attachment_path :2009` 的註解甚至寫了規則）。`handle_list_claude_slash_commands :1590` 同型較輕。
修法：改帶 `session_id`，在 host 端用 registry 的 cwd 當邊界。

### Medium

**HC-11 · uuid-less 記錄全部到不了畫面** `[C-§1.6]`
`permission-mode`、`ai-title`（session 標題）、`bridge-session`（官方 URL）、`last-prompt`（帶權威 `leafUuid`）、`cost-state` 的 `modelUsage`／`totalLinesAdded`、`file-history-*`。修法：像 `spend()` 一樣直接掃 `Transcript::records`，各加一個 accessor。

**HC-12 · 已知 `system` subtype 畫成「Unrecognized」JSON** `[C-BUG-5][C-BUG-6]`
`:7501–7511`。`away_summary`（你離開時的中文摘要）、`bridge_status`（含官方 URL）、`stop_hook_summary`、`informational` 都有 `content` 字串；`turn_duration :7441` 整筆 `return`，丟掉 spec §4.1 要求的 `pendingBackgroundAgentCount`。

**HC-13 · attachment 全塞一個「Context」摺疊區、插在 index 0** `[C-§1.5]`
`auto_mode`、`total_tokens_reminder`（「N tokens left」）、`edited_text_file` 被埋在裡面；31 種 subtype 一視同仁；五次 compact 後這一區描述的是相隔數小時注入的東西。

**HC-15 · 失敗的 SendUserFile 畫成成功** `[C-BUG-3]`
`sent_files_kind` 在 `:7641–7643` 提前 `return`，`is_error`（`:7652–7656`）從沒被讀；`EntryKind::SentFiles` 沒有錯誤欄位。

**HC-16 · 送來的 mp4 是死路** `[C-BUG-8]`
`sent_image_format :7738` 對 `video/mp4` 回 `None` → 只剩 icon、檔名、大小、不可點的絕對路徑。樣本裡多數 SendUserFile 是 mp4。修法：路徑列做成按鈕，本機用 OS handler 開，remote 先 `read_attachment` 下載。

**HC-17 · 4 MB 圖片上限拒掉一般截圖** `[C-BUG-9]`
`MAX_SENT_IMAGE_BYTES :126`。Retina 全螢幕 PNG 常超過。修法：改為「超過就顯示載入按鈕」。

**HC-18 · `tool_target` 只認 5 個工具** `[C-BUG-2]`
`:6164–6192` 只有 Read／Write／Edit／NotebookEdit／Bash；其餘 `_ => ""` → 「Running Agent」六分鐘沒有任何目標、沒有 elapsed。

**HC-20 · Saying 是 `Label` 不是 markdown、固定 10 rem 無把手** `[A-B10][C-BUG-7]`
`:3428–3430`、`LIVE_MESSAGE_MAX_HEIGHT_REMS :68`；`follow_the_live_message :3449` 只在長度改變時跟隨。

**HC-25 · `@` 選單高亮可走到畫不出來的列** `[B-B1]`
`offered_files` 回 50 筆、`render_file_matches :4117` 只畫 8 筆、高亮對 50 算 → Down 9 次後看起來沒反應，Enter 插入沒看過的路徑。`/` 選單是先截斷再高亮（`:6245`），兩個選單相反慣例。

**HC-26 · `@` 每個按鍵重走一次 host 樹、選單在鍵間消失** `[B-B10]`
`BufferEdited` → `list_files_for_mention`（`:1503–1507`），每鍵一個 RPC，最多 2 萬 entry；`offered_files` 在 query 不完全相等時回 `None`（`:1783–1785`），所以選單每鍵消失一次。`FILE_MENU_ROWS` 的註解說「本地縮窄」但沒有實作。空 `@` 會列出前 50 個檔並吃掉 Enter（B-B11）。

**HC-27 · `PendingSends` 永不逾時、配對用逐字相等** `[A-B7]`
`resolve` 成功時什麼都不做（`:5831–5836`）；配對 `source.trim()==text`（`:5886`）；送進 dialog 的訊息永遠不會出現在 transcript → 「Sending…」到 session 結束。修法：也對 `queue-operation` 的 enqueue 文字配對；pane 不在 prompt 就拒送。

**HC-28 · pane poll 的過期守衛是值相等，A→B→A 會失效** `[E-B9]`
`session_store.rs:595–602`。transcript poll 有 `transcript_resets` 世代（`:125`），pane poll 沒用。

**HC-29 · remote 的 subagent scan 沒有 batch，每秒重讀整份 transcript** `[E-B10]`
`RemoteSource::list_subagents_for_sessions :376–405` 發 N 個 `ListClaudeSubagents`；每個走 `find_session_directory`（全掃 projects）+ `read_session_conversation`（整檔逐行），快取鍵 `(len, mtime)` 在活著的 session 上每秒失效；`READ_CONVERSATIONS` 全域 map 永不清。五個活 session、20 MB transcript ≈ 100 MB/s 讀取。這就是「remote 很慢」讓 capture／tail 排隊的來源。

**HC-30 · `TailState.pending` 無上限、每 250 ms 來回過線** `[E-B11]`

**HC-31 · 貼上的檔案同名互相覆蓋、永不清理** `[B-B15][E-B12]`
`~/.claude/zed-pasted/` 用原檔名（macOS 截圖全叫 `Screenshot ….png`、多數工具叫 `image.png`）；沒有 `@`，靠模型自己決定 Read。

**HC-32 · 快捷鍵在輸入框收合時失效，但註解說會通** `[B-B8]`
綁定全在 `ClaudeSessionsInput > Editor`（`default-macos.json:1755–1769`），收合後沒有 Editor → Escape 中斷做不到。

**HC-33 · `quit_armed` 永不自動解除** `[B-B9]`
點一次 Quit 後改變主意，二十分鐘後再點一次 → session 結束，且文件寫「不可回復」。

**HC-34 · stacked 固定高度餓死對話** `[D-B4]`
toolbar + chips + live 10 rem + terminal 260 + 8 行 editor + 按鈕列；只有 transcript `flex_grow_1`。短 pane 下對話消失，留下 tmux 與輸入框。dock 的 `render_session_section` 外層不是 `size_full`，多列可能不能捲。

**HC-35 · `question_hook_is_installed` 每秒重讀三個檔 + parse settings.json** `[E-B13]`，且在 remote 上跟 `read_live_message` 同一個 RPC。

**HC-36 · registry scan timeout 後重發，host 上前一個還在跑** `[E-B14]`（慢主機越滾越慢）。

### Low

- **HC-37** slash 指令清單每個 panel 只列一次、對 Zed worktree 而不是 session cwd `[B-B13]`
- **HC-38** Up 歷史來自 `entries`：剛送的訊息（還在 pending）不在、slash 指令不在、compact 前的不在 `[B-B14]`
- **HC-39** 切 session 沒清 `file_matches`／`dismissed_*`／`slash_commands` `[B-B16]`
- **HC-40** mirror session 名用本機 `std::process::id()`，remote 上兩台機器同 pid 會撞；`destroy-unattached` 在 attach 回來之後才設，Zed 崩潰會漏 session `[D-B6]`
- **HC-41** `install_question_hook` 回傳 `Option<PathBuf>`，`None` 代表三種不同結果（沒東西備份／已裝／只更新了 script）`[E-B15]`；settings.json 壞掉時 script 先寫了、settings 沒改、永遠「未安裝」`[E-B16]`；沒有 uninstall `[E-S2]`；備份檔無限累積 `[E-S4]`
- **HC-42** `ErrorSource::Send` 在 `/clear`／session 消失時不清 `[E-B17]`
- **HC-43** tool result 裡的 image block 被印成 base64 JSON（`block_content_text :7946–7960`），`without_base64_payload :7965` 沒套用 `[C-BUG-4]`
- **HC-44** Edit 只顯示 `new_string`、沒有 diff `[C-BUG-10]`（對比網頁版最大的品質差距）
- **HC-45** `bridgeSessionId` 過 remote wire 時被剝掉（`session_source.rs:611–627`）→ remote panel 沒法給網頁 URL `[A-Q8]`
- **HC-46** 三個 poll loop 不管 panel 有沒有畫都在跑（remote 上每秒 2+N 個 RPC）`[E-S7]`
- **HC-47** Windows remote host 永遠空清單且無錯誤 `[E-S6]`

### 沒問題的部分（已查，記錄下來免得下次重查）`[E]`

offset 在檔案被截斷／替換時的處理正確（`start_offset` 在 reset 前擷取、`:947` 比對）；`/clear` 依 `sessionId` 跟隨正確；`~` 一律在 server 端解；三個 loop 都不會因錯誤退出（只會**不前進**）；production 路徑沒有 `unwrap`／`expect`／`let _ =`／slice indexing；RPC 派發層沒有 head-of-line blocking（`5c69df78fd` 修過）；`pane_target`／`PaneKey` 對 tmux 參數的淨化正確；hook 安裝是 merge 不是 overwrite 且有備份。

---

## 5. 渲染覆蓋盤點（對比 terminal／網頁版）

樣本：40 份 transcript、35,465 筆記錄（Reviewer C）。

### 5.1 訊息與 block

| 項目 | 狀態 | 位置 |
|---|---|---|
| assistant markdown（表格、fence、標題、連結） | ✅ | `message_kind :7575`、`markdown_for :2547` |
| user 文字（去 injected wrapper） | ✅ | `user_visible_text :7999` |
| slash 指令重建 | ✅ | `rewrite_slash_commands :8038` |
| thinking | ⚠️ 有，預設被**刪**不是折疊 | `:2369` |
| user 貼圖 | ✅ | `image_kind :7780` |
| compact 摘要 | ✅ | `:7551–7556` |
| **串流中的字** | ⚠️ 純 `Label`，不是 markdown | `:3428` |

### 5.2 `tool_use`（依實測頻率）

| 工具 | 次數 | 狀態 |
|---|---|---|
| Bash | 4,203 | ✅ 指令當 shell fence |
| Edit / Write / NotebookEdit | 272 / 26 | ⚠️ 只有新內容、**無 diff** |
| `mcp__*` | ≈190 | ❌ JSON；卡片標題是原始 `mcp__server__tool` |
| Read | 85 | ⚠️ 刻意 JSON |
| ToolSearch | 44 | ❌ JSON |
| **AskUserQuestion** | 33 | ❌ 對話裡是 JSON；答題 UI 靠 capture-pane |
| Agent / Workflow | 26 / 7 | ✅ 卡片、phase |
| Monitor、TaskStop、SendMessage、Skill、Artifact、ScheduleWakeup | 20 / 10 / … | ❌ JSON |
| WebFetch / WebSearch | 17 / 12 | ❌ JSON（沒有 URL／標題） |
| SendUserFile | 11 | ⚠️ working tree 才有；mp4 死路、失敗畫成成功、>4 MB 拒 |
| **TodoWrite** | — | ❌ JSON（terminal 最常看的清單） |
| Glob / Grep | — | ❌ JSON |

### 5.3 `tool_result`

| 項目 | 狀態 |
|---|---|
| 文字結果、12 行截斷 + disclosure | ✅ |
| 結構化 `toolUseResult` | ⚠️ 只懂 `stdout`／`stderr`／`content`／string；無 diff、無檔案內容、無搜尋命中列表 |
| `is_error` | ✅（除 SentFiles） |
| `<persisted-output>` lazy load | ✅ |
| **result 裡的 image** | ❌ base64 印成 JSON |

### 5.4 `system/*`、uuid-less 記錄、attachment

見 §4 的 HC-11、HC-12、HC-13。**terminal 有、transcript 也有、panel 沒讀**的清單：permission mode、auto_mode 旗標、session 標題、claude.ai URL、「N tokens left」、pending agent 數、turn 時長、離開摘要、todo list、最新正在跑的 call。只有兩件 transcript 真的沒有：串流中的字（hook 已解）、問題選單的游標位置（不該追）。

### 5.5 網頁版有、panel 要補的（依價值排序）

1. 最新 tool call 常駐 + 每輪摘要（§8.2）
2. Edit／Write 的 diff（Zed 有現成 diff element）
3. compact／context 進度條（§8.4）
4. session 標題（`ai-title`）與「在 claude.ai 開啟」
5. AskUserQuestion 卡片顯示**全部**問題（不追游標）
6. 權限請求卡片（`PermissionRequest` hook）— 網頁能核准、panel 現在連看都看不到
7. TodoWrite 清單、Glob／Grep／WebFetch 的目標
8. `away_summary`、`turn_duration` 的 turn footer
9. uncommitted diff 面板（網頁有；Zed 本來就有 git panel，加一顆跳轉即可）
10. MCP 工具名稱拆 server／tool

---

## 6. 輸入通道：所有可能性

「不讀 terminal」靠 §7，跟這一節無關；這一節只談**寫入**——怎麼把訊息、中斷、權限核准、問題答案、model／mode 切換送進正在跑的 session。

### 6.0 總表

| 方案 | 能送什麼 | 官方狀態 | 需要 session 重啟？ | 訊息身分 | 保得住「tmux／手機／網頁／Zed 四邊同一個 session」？ | 風險 |
|---|---|---|---|---|---|---|
| 6.1 現狀 `tmux send-keys`（藏起 box） | 文字、Esc、任何按鍵 | 非官方 | 否 | **使用者本人** | ✅ | 不知道 pane 在哪個狀態；巨集脆弱；dialog 看不到 |
| **6.2 Zed channel（MCP server）** | 文字事件、**權限 allow/deny** | research preview，有文件 | **是**（`--channels`／dev flag） | 「channel 事件」 | ✅ | preview 會變；AskUserQuestion 不 relay；allowlist |
| 6.3 Inbox socket | 文字 | 有文件（給 script／hook 用） | 否 | 「另一個 session」 | ✅ | 不能核准、指令不執行、bypass 模式會 hold 等你在 terminal 按 |
| 6.4 `PermissionRequest` hook（阻塞等 Zed 決定） | **只有權限 allow/deny** | 有文件 | 否（下一個 process 生效） | 使用者 | ✅ | hook timeout；Zed 不在時要立刻放行 |
| 6.5 帶參數的 slash 純文字 | `/model x`、`/effort x`、`/config k=v`、`/autocompact n`、`/output-style s`、`/compact`、`/clear` | 有文件 | 否 | 使用者 | ✅ | 仍需一條送文字的通道（6.1／6.2／6.3） |
| 6.6 `claude -p --input-format stream-json --resume <id>` | 全部（訊息、中斷、model、mode、權限回覆） | 部分文件 | 它是**新 process** | 使用者 | ❌ 變成兩個 process 寫同一份檔 | 只適合 Zed 自己擁有的 session |
| 6.7 Agent SDK／ACP（Zed 自己開 session） | 全部 | 有文件 | Zed 自己 spawn | 使用者 | ❌（除非 Zed 也帶 `--remote-control`，且 tmux 不再是家） | 等於重做 Zed Agent Panel；spec 明列非目標 |
| 6.8 Remote Control 第三方客戶端 | 全部（網頁能做的） | **無 API** | 否 | 使用者 | ✅ | 逆向、憑證、ToS；spec 明列非目標 |
| 6.9 放棄輸入：只做「在 claude.ai 開啟」 | — | 有文件 | 否 | — | ✅ | panel 變唯讀 |
| 6.10 `claude --bg` + supervisor peek 回覆 | 文字 | agent view 有這功能，IPC 無文件 | session 要改用 `--bg` 啟動 | 使用者 | ⚠️ tmux 不再是家；手機／網頁仍可 | 協定未公開 |

### 6.1 現狀：`tmux send-keys`，只是把 box 藏起來

Reviewer A 的結論：可以當**過渡**，不能當終點。真的要留，只留兩種操作：

- `paste-buffer` + `Enter` 送純文字（`send_text :2493`，bracketed paste，已測）
- `Escape` 中斷

其餘一律刪：數字選項、Up／Down／Enter 巨集、BTab 切 mode、C-d 退出、14 行 mirror 的三顆按鈕。並且用 §7 的 hook 狀態守門：`Stop` 之後才算 idle、`PermissionRequest`／`Notification(permission_prompt)` 期間禁送、`PreToolUse(AskUserQuestion)` 期間禁送一般訊息。這樣 HC-21 的「訊息打進 picker」至少不會發生在 panel 知道的狀態下。

### 6.2 Zed channel（推薦的終點）

**設計**：Zed 提供一個小 MCP server（bun／node，stdio），宣告 `claude/channel` 與 `claude/channel/permission`。它跟 Zed 之間走 Unix socket 或檔案佇列。Session 啟動時帶 `--dangerously-load-development-channels server:zed`（preview 期間；日後若上 allowlist 改 `--channels`）。你現有的 `claude()` shell function 已經在幫每個 session 加 `--remote-control`，再加一個 flag 是零成本。

**能做**：
- Zed 送訊息 → `notifications/claude/channel {content, meta:{from:"zed", …}}` → 模型收到 `<channel source="zed">…</channel>`，idle 時起新 turn，忙時在下一個 tool call 之間讀。
- 權限請求 → Zed 收到 `permission_request {request_id, tool_name, description, input_preview}` → 畫卡片 → 回 `permission {request_id, behavior}`。terminal 的 dialog 同時開著，誰先答誰算。
- `instructions` 可以告訴模型「來自 zed 的訊息視同使用者在 IDE 打的字」。

**做不到／要注意**：
- AskUserQuestion 不 relay（官方只 relay tool permission）。答問題仍要靠 6.1 的文字（「回答：選項 2」）或等官方擴充。
- 訊息帶 `<channel>` 標籤，不是原生 user turn；模型的處理方式由 `instructions` 決定，跟 Telegram bridge 同等級。
- `--channels` 只接受 Anthropic allowlist 的 plugin；dev flag 每次啟動要按一次確認（文件寫「after a confirmation prompt」，需實測是否每次）。
- research preview：「the `--channels` flag syntax and protocol contract may change」。
- Team／Enterprise 需 admin 開 `channelsEnabled`；你是個人帳號不受此限。
- 若 session 是 `-p` 模式，AskUserQuestion 會被關掉——不影響你的 interactive session。

**成本**：一個 ~200 行的 MCP server + Zed 端一個 socket client + proto 兩則訊息（remote 用）。比重寫 send-keys 巨集少。

### 6.3 Inbox socket

文件明寫可以「script or hook post into a session」，macOS 不用 token。但語意是 peer message：模型被告知「這不是使用者說的」、不能拿來核准、`/compact` 不會執行；而且**收件方是 bypassPermissions 時會先 hold 等你在 terminal 核准**（你的 session 多半是 auto／bypass）。訊息 JSON 格式沒文件（binary 裡看得到 `kind:"peer"`／`delivered`／`not_delivered`／`peer_inbound_approval`）。**結論：不當主通道；可當「通知 session 某件事」的旁路。**

### 6.4 `PermissionRequest` hook 當核准通道

Zed 裝一個 hook script：收到 payload → 寫 `~/.claude/zed-events/<session>/perm-<tool_use_id>.json` → 若存在 Zed 心跳檔（Zed 正在看這個 session）就 poll 決定檔最多 N 秒 → 回 `decision`；Zed 不在或逾時 → exit 0 什麼都不回，terminal 照常跳 dialog。優點：不用重啟 flag、不依賴 preview。缺點：hook 執行期間 terminal **不會**先顯示 dialog（hooks 先跑），所以 N 要短（例如 20 s）且 Zed 心跳要準。這條跟 6.2 可以並存，先做 6.4 再升 6.2。

### 6.5 帶參數的 slash 指令

官方明列可在 terminal／網頁／`-p` 以參數形式執行的指令（§1.4）。panel 的 `/` 選單應該把 `/model`、`/effort`、`/config`、`/autocompact`、`/output-style`、`/advisor` 改成 Zed 原生 picker → 組成 `/model claude-opus-5` 這種文字送出，不再送裸 `/model`。剩下真的只有 TUI 的（`/permissions`、`/login`、`/resume`、`/agents`、`/plugin`、`/hooks`、`/mcp` 無參數、`/memory`、`/theme`、`/export`、`/bug`）標「需要 terminal 或網頁」，列上提供兩顆按鈕。

### 6.6 `claude -p --input-format stream-json --resume <id>`

headless.md 明寫是續對話檔的新 process；`--continue` 只接「已結束」的 session。對正在跑的 interactive session 這麼做等於兩個 process 寫同一份 jsonl，且 registry／tmux／手機看的是舊 process。**排除**，除非 Zed 放棄「接既有 session」改成「Zed 擁有 session」（那就是 6.7）。

### 6.7 Zed 自己開 session（Agent SDK／ACP）

Zed 已有 Agent Panel 走 ACP 跑 Claude Code；spec §0 明列非目標。若你願意放棄「tmux 是家」，這是功能最完整的路：權限、問題、model、mode、中斷、串流全部結構化。但手機／網頁要靠 `--remote-control` 是否能與 SDK 模式共存（文件說 `-p` 可以帶 remote-control 但「not commonly used」），且 Zed 關掉 session 就死（除非包在 `--bg`）。列出供比較，不推薦。

### 6.8 Remote Control 第三方客戶端

無 API（dev.to 那篇「hidden API you can't use yet」講的就是這件事）。要用要逆向 bridge 協定、拿 claude.ai 憑證，spec §0 明列排除。**排除**。

### 6.9 方案 Z：放棄輸入，只做「在 claude.ai 開啟」

panel = spec P1（列表 + 唯讀 rich transcript + subagent + persisted output）+ 每列一顆按鈕開 `https://claude.ai/code/<bridgeSessionId>`。remote host 要先補 proto（HC-45）。你在網頁打字、核准、切 model，Zed 負責看（它看得比網頁多：subagent 全文、persisted output 全文、Zed 內點檔名開檔）。**這是最便宜且立刻穩定的版本**，也是 Phase 0 的一部分——不管後面選哪條，這顆按鈕都要有。

### 6.10 `claude --bg` + supervisor

agent view 的 peek panel 能對背景 session 打字送出，代表 supervisor 有一條「送 user turn 給背景 session」的 IPC，但沒公開。若日後公開（或 `claude agents` 加 CLI 子命令），它會是最乾淨的「使用者身分」通道，且 `respawn` 直接解決抱怨 10。**列入觀察**；現在不做。

### 6.11 推薦組合

- **Phase 0**：6.9 的按鈕（不論如何都要）。
- **Phase 1 過渡**：6.1 縮到「文字 + Esc」+ 6.5 帶參數 slash + 6.4 hook 核准；全部用 §7 的狀態守門。
- **Phase 2**：6.2 Zed channel 取代文字與核准；6.1 完全刪除；AskUserQuestion 用文字答（「選 2」）或等官方。
- 6.3 當旁路（例如「Zed 已核准 X」的通知）；6.10 觀察。

---

## 7. 讀取面：不讀 terminal 的每一個狀態

| 狀態 | 現在來源 | 新來源 | 可行性 |
|---|---|---|---|
| 對話、compact、subagent、cost、queue | transcript | 不變 | ✅ 已做 |
| **permission mode** | `capture-pane` footer 文字 | `permission-mode` 記錄（掃 `records` 取最後一筆）；備援：任何 hook payload 的 `permission_mode`；`attachment/auto_mode` 補旗標 | ✅ 資料已在檔裡 |
| **Saying（串流文字）** | `MessageDisplay` hook 檔，排在 tmux 後 | 同一個 hook，但：讀 `final`／`turn_id`；`Stop` hook 寫 turn 結束 → 清；transcript 出現同 turn 的 assistant 記錄 → 清；mtime > 30 s → 隱藏；hook poll 獨立於 tmux | ✅ |
| **正在跑哪個工具**（抱怨 9） | `activity()` 只看已 flush 的記錄（當前 turn 永遠 Idle） | `PreToolUse`（所有工具，不只 AskUserQuestion）+ `PostToolUse` 寫進 `~/.claude/zed-events/<session>.jsonl`；備援：registry `status: busy` + `statusUpdatedAt` 新鮮度 | ✅ |
| **正在等權限** | 看不到 | `PermissionRequest` hook（含 `tool_name`、`tool_input`、`tool_use_id`）；`Notification(permission_prompt)`；`claude agents --json` 的 `waitingFor: "permission prompt"` | ✅ |
| **正在等問題** | `PreToolUse(AskUserQuestion)` 檔 + pane 游標 | 同一個 hook，顯示**全部**問題；結束靠 `tool_result` 或 `Stop`；`Notification(agent_needs_input)` | ✅（放棄追游標） |
| idle／turn 結束 | 無 | `Stop` hook（帶 `last_assistant_message`）、`Notification(idle_prompt)`、`system/turn_duration` | ✅ |
| **context 用量 %** | 只有分子 | `statusLine` wrapper 把 JSON 寫到 `~/.claude/zed-status/<session>.json`（含 `used_percentage`、`context_window_size`、`model`、`effort`、`cost`、`rate_limits`）並 chain 你原本的 statusLine command；備援：`message.usage` 分子 + model→window 表 | ✅ |
| compact 進行中 | 無 | `PreCompact`／`PostCompact` hook | ✅ |
| model／effort | 無 | statusLine JSON；`PostModelSwitch` hook | ✅ |
| session 標題 | registry `name`（`zed-e4` 這種 slug） | `ai-title` 記錄 | ✅ |
| 官方網頁 URL | 無 | registry `bridgeSessionId` 或 `bridge-session` 記錄 | ✅（remote 要補 proto） |
| pending agent 數、turn 時長 | 丟掉 | `system/turn_duration` | ✅ |
| 「N tokens left」 | 埋在 Context 區 | `attachment/total_tokens_reminder` | ✅ |
| session 活著／等待中 | pid + procStart | 不變 + `claude agents --json` 的 `status`／`waitingFor`／`state` 補背景 session | ✅ |
| 14 行 terminal mirror | `capture-pane` | **刪除** | — |

**hook 安裝的重新設計**：一個 dispatcher script `~/.claude/hooks/zed-claude-events.sh`，登錄在 `UserPromptSubmit`、`PreToolUse`（無 matcher）、`PostToolUse`、`PermissionRequest`、`Notification`、`Stop`、`SubagentStop`、`PreCompact`、`PostCompact`、`PostModelSwitch`、`MessageDisplay`、`SessionEnd`，全部 append 到 `~/.claude/zed-events/<session_id>.jsonl`（`Stop` 或 256 KB 時 rotate）。panel 用跟 transcript 一樣的 tail 邏輯讀它。取代現在的兩個 script、兩個目錄、永不刪的檔案。`MessageDisplay` 預設 timeout 10 s，script 必須只做 append。

---

## 8. UI／UX 重設計（類網頁版）

### 8.1 版面

```
┌ Tab: 「Claude session message 健康檢查」(ai-title) · zed-41 · ~/go/src/…/zed ┐
│ [auto ▾] [opus · high] ▰▰▰▰▰▱▱▱▱▱ 108K / 200K · 54%  $3.21 · 5h 23% · [claude.ai ↗] │
│ [Main] [WP1 forward ports ●] [WP3 fixes]                                        │
├──────────────────────────────────────────────────────────────────────────────────┤
│  對話（唯一 flex 區）                                                             │
│  … user …                                                                        │
│  ▸ 12 tool calls · 3 files edited · 2 agents · 41s          ← 每輪摘要（可展開）  │
│  … assistant markdown …                                                          │
│  ▸ Bash · ./script/clippy --all-targets · 1:24 ⟳            ← now row（最多一列）  │
│  ┌ 權限請求 ────────────────────────────────────────────┐                        │
│  │ Edit crates/foo.rs  [Allow] [Deny] [Always allow…]   │  ← PermissionRequest   │
│  └──────────────────────────────────────────────────────┘                        │
│  ┌ 問題 (2/3) ──────────────────────────────────────────┐                        │
│  │ Q1 … ○a ○b   Q2 … ☑a ☐b   Q3 … [Type something]      │  ← 全部一起顯示        │
│  └──────────────────────────────────────────────────────┘                        │
│  正在說… （markdown，可拖高，Stop 一到就收）                                        │
├──────────────────────────────────────────────────────────────────────────────────┤
│ [輸入框 1–8 行]                                            [Esc] [Send]         │
│ (session 狀態：idle / 忙 / 等權限 / 等問題 / 對話框開著 → 決定 Send 能不能按)          │
└──────────────────────────────────────────────────────────────────────────────────┘
```

- **沒有 terminal 區**。需要 TUI 時兩顆按鈕：「在 tmux 開」（既有 tmux_sessions panel 跳過去）、「在 claude.ai 開」。
- 唯一 flex 的是對話；Saying 有拖曳把手（沿用 `DraggedTerminalDivider` 的機制）；輸入框 auto_height 1–8。
- dock 版保留列表 + 每列：標題（`ai-title`）、狀態點（idle／busy／等你）、context %、cost、網頁按鈕。點一列開 tab（維持現在的分工）。

### 8.2 「now row + 每輪摘要」取代 `show_tool_calls`

三層：
1. **訊息**永遠畫。
2. **now row**：唯一一列，釘在對話尾端，來源是 `PreToolUse` 事件（沒有 `PostToolUse` 配對的那個）或 `activity()`：icon · 工具名 · 目標（HC-18 的 fallback：`description`／`query`／`prompt`／`pattern`／`url`／`command`／`file_path`）· elapsed · 脈動。點開 = 現在的 ToolUse 卡片 + 部分輸出。它是**衍生狀態**，不是保留的 entry，所以不會像 Saying 那樣卡住。
3. **每輪摘要**：每個結束的 turn 一列 `12 tool calls · 3 files edited · 2 agents · 41s`，點開展開該輪的 ToolUse／ToolResult／Thinking。

實作位置（Reviewer C）：`rebuild_entries :2323–2372` 的 `retain` 換成 post-pass，在每個 `Message{role: User}` 切輪，摺疊的輪 splice 一個 `EntryKind::TurnSummary`；`EntryCache` 不動所以展開零重算；`expanded` 沿用；`live_cache_keys :2514` 加一個 arm。`show_tool_calls` 語意改成「預設展開所有輪」。

### 8.3 Saying

- 用 `markdown_for` 畫，stable key。
- 拖曳把手，記住高度。
- 收合條件（任一）：`Stop` 事件；transcript 出現 `message_id` 對得上或時間更新的 assistant 記錄；mtime > 30 s。
- 跟隨用內容 identity，不用長度。

### 8.4 context 進度條

- 分子：`Spend::context_tokens`（已算）；分母：statusLine 的 `context_window_size`；備援：model→window 表，未知就只顯示數字不畫條。
- 畫在 toolbar 取代「94K ctx」：細條 + `108K / 200K · 54%` 文字 + autocompact 門檻刻度（`/autocompact` 的值若在 statusLine 沒有，就用 CLI 預設；不確定就不畫刻度）。
- compact 後 `context_is_post_compaction` 當 tooltip 解釋為何數字掉下來。
- 旁邊第二個 fact：「15.0M tokens left」（`total_tokens_reminder`）與 rate limit 5h／7d %。

### 8.5 問題卡片與權限卡片

- 問題：一次顯示 call 的所有 question，單選／多選／自由輸入各自的控制，**送出時組成一則文字**（Phase 1：`paste` 進 prompt；Phase 2：channel）。不追游標、不送數字、不做 Down-walk。超過能力範圍（例如 CLI 正在 review 畫面）直接說「請在 terminal／網頁完成」。
- 權限：`PermissionRequest` 事件 → 卡片（工具、目標、`input_preview` 折疊）→ Allow／Deny → 6.4 或 6.2 回覆；terminal 同時開著 dialog，誰先答誰算。

### 8.6 Slash 指令

- `/` 選單分三類標色：**純文字**（skills、project／user commands、`/compact`、`/clear`、`/context`…）；**帶參數**（`/model`、`/effort`、`/config`、`/autocompact`、`/output-style`、`/advisor`）→ Zed 原生 picker 組字串；**只在 terminal**（`/permissions`、`/login`、`/resume`、`/agents`、`/plugin`、`/hooks`、`/memory`、`/theme`、`/export`、`/bug`、無參數 `/mcp`）→ 列上兩顆按鈕。
- 送出任何指令前檢查 §7 的狀態，不在 idle 就排隊顯示而不是送。

### 8.7 shell 小弟（grok／agy／codex）能見度

偵測 Bash `tool_use` 的 `run_in_background: true` 且 command 符合 `agent -f --trust -p "$(cat FILE)" --model M > LOG`／`agy … -p`／`codex exec …`：
- 卡片：模型（從 `--model`）、任務（prompt 檔第一段）、log 路徑、report 路徑（你的規則要求 prompt 內寫 `Report file:`，可 parse）。
- tail report／log 檔（走既有 `read_file`／`tail_transcript` 的 RPC）顯示最後幾行。
- 狀態：`TaskCreated`／`TaskCompleted` hook 或 transcript 裡的 `<task-notification>`（`TASK_NOTIFICATION_*` 已有 parser）。
- 這是 panel 相對 terminal／網頁的獨有價值，跟 subagent 卡片同一個位置。

### 8.8 檔案與媒體

- SendUserFile：mp4／任何檔 → 路徑列是按鈕（本機 OS 開啟、remote 先下載）；`is_error` 標紅；>4 MB 顯示「載入」按鈕。
- tool result 裡的 image → `EntryKind::Image`。
- Edit → 用 `structuredPatch`／`oldString`／`newString` 畫 Zed diff。

---

## 9. tmux／session 生命週期整合

### 9.1 身分

- **對話的身分是 `sessionId`**；pid、tmux pane、`bridgeSessionId` 都是屬性。
- registry 掃描：同 `sessionId` 換 pid = respawn／resume → 原 tab 直接重綁，不開新 tab、不清記錄。
- 同 pid 換 `sessionId` = `/clear` → 維持現在的行為（換對話）。
- pid 消失、`sessionId` 沒有新 pid → **ended row**：保留 transcript、標題、cwd、tmux target、`bridgeSessionId`；停 tail（或每 10 s 慢 tail，因為別的 process 可能 `--resume` 它）；可讀、可展開；不能送。

### 9.2 ended row 的動作

- **Resume here**：往記住的 tmux pane 送 `claude --resume <sessionId>`（用你的 `claude()` function 就自帶 `--remote-control`）；pane 也沒了就 `tmux new-window -c <cwd>` 再送。這是唯一保留的「對 shell 送字」，目標是 shell prompt 不是 Claude TUI，風險低。
- **Respawn**（`kind == background`）：`claude respawn <id>`。
- **Open in claude.ai**：bridge 還在的話網頁那邊可能還能看。
- **Dismiss**。

### 9.3 列舉來源

- 保留 registry + pid/procStart 判活（spec 要求，且 `agents --json` 沒說它證明 process 活著）。
- **加** `claude agents --json`：補 `kind: background` 的 session（現在 `visible_sessions :104` 硬過濾 interactive）、補 `status: waiting` + `waitingFor`（等權限／等輸入／對話框開著——這就是 §7 的「pane 狀態」，不用刮畫面）、補 `state`（working／blocked／done／failed／stopped）。背景 session 沒 tmux → 不能 send-keys，但能讀、能 `claude logs`、能 attach 到 tmux 新 window。
- 判活失敗但 `agents --json` 說活著 → 顯示但標「無法判活」。

### 9.4 CLI 更新

- 用 `--bg` 起的 session：supervisor 自己處理，panel 只要 9.1 的重綁。
- tmux 裡的 interactive session：更新後 process 死 → ended row → 你按 Resume here。比現在少的點擊：不用找 pane、不用重開 tab、歷史不消失。
- 若你願意改習慣：`claude --bg --remote-control` 起 session、Zed 顯示、`claude attach <id>` 在需要 TUI 時進去。tmux 變成可選。

### 9.5 mirror 殘留

Phase 0 刪 attach 後，`zed-claude-mirror-*` 不再產生。現在還掛著的那個（`76859-52`）在下次 Zed 重啟後由 `destroy-unattached` 收掉；若沒收掉，`tmux kill-session -t zed-claude-mirror-76859-52` 一次清乾淨。

---

## 10. 分期計畫與驗收

實作進度、每個 WP 的規格與審查結論、大腦的裁決：`docs/claude-health-check-progress.md`。
使用者側的設定步驟、面板操作與已知限制：`docs/claude-sessions-setup.md`。

---

## 11. 待你拍板的規格漏洞

這些不是 bug，是原 spec（`docs/claude-session-viewer-requests.txt` §0–9）沒決定、被實作自己填答案的地方。每條都會影響多個 reviewer 的結論。

1. **Zed 是不是 tmux client？**（D-G2, A-G4）建議：不是。
2. **對話身分是 pid 還是 sessionId？**（D-G4）建議：sessionId。
3. **process 死了但 jsonl 在，顯示什麼？**（D-G1）建議：ended row。
4. **背景 session（`kind: background`）要不要列？選了之後「送」是什麼意思？**（D-G5, E-S5）建議：列，唯讀 + attach 按鈕。
5. **輸入通道終點選哪個？**（§6）建議：6.2 channel；過渡 6.1 縮小版。
6. **問題卡片：追游標還是全部顯示？**建議：全部顯示。
7. **哪些已知記錄型別要畫、畫成什麼？**（C-§8.1）spec 只說「未知」退回 JSON，沒說「已知」長怎樣。
8. **31 種 attachment 哪幾種要提出來？**（C-§8.3）建議：`auto_mode`、`total_tokens_reminder`、`queued_command`（已做）、`edited_text_file`。
9. **每個 model 的 context window 與 autocompact 門檻的來源？**建議：statusLine；沒有就不畫條。
10. **staleness 預算**：每個值多久沒更新要標 stale？（E-S1）建議：transcript 5 s、events 5 s、statusLine 10 s、registry 3 s。
11. **hook 要不要有 uninstall？備份保留幾份？**（E-S2, S4）
12. **panel 沒畫時 poll 要不要停？**（E-S7）建議：dock 關閉且無 tab 時降到 5 s。
13. **送失敗的訊息、失敗的送檔長什麼樣？**（B-B3, C-§8.5）
14. **貼上的檔案放哪、叫什麼、多久清？**（B-G4, E-B12）
15. **slash 指令對 session cwd 還是 Zed worktree？**（B-B13）建議：session cwd。
16. **Windows remote host 顯示什麼？**（E-S6）

---

## 12. 尚未能從原始碼回答、需實測的問題

1. `--dangerously-load-development-channels` 的確認提示是每次啟動都跳，還是記住一次？
2. channel 訊息在 `instructions` 說「視同使用者」後，模型實際的服從程度是否與 user turn 相當？
3. `PermissionRequest` hook 執行期間 terminal 是否完全不顯示 dialog？（決定 6.4 的等待秒數）
4. Ctrl+C 中斷的 `AskUserQuestion` 是否留下 `tool_result`？（HC-09 的第二種卡住機制）
5. `claude respawn` 是否保留 `sessionId`（文件說「resumes its saved conversation」，應該是）與 tmux pane？
6. `claude agents --json` 對 interactive session 是否也給 `waitingFor`？（文件表格說「When `status` is `waiting`」，未限定 kind）
7. `~/.claude/settings.json` 是嚴格 JSON 還是 JSONC？（HC-41 安裝失敗的嚴重度）
8. tool result 的 `content` 陣列實務上是否出現 image item？（HC-43 的頻率）
9. `Transcript::spend()` 每 frame 全掃在 20 MB 檔上的成本；加三個 accessor 後是否要改成增量。
10. remote 的 `capture-pane` 在沒有任何 client attach 時 pane 是多大？（Phase 0 之後不重要）

---

## 附錄 A：Reviewer 分工與報告

| 區 | 模型 | 檔案 | 主要結論 |
|---|---|---|---|
| A 畫面刮取／live state | grok 4.6 xhigh | `scratchpad/reportA.md` | 三個時鐘；Saying 無結束；每個狀態的 transcript／hook 替代表；方案 a/b/c 比較 |
| B 輸入路徑 | Opus | （訊息內） | 16 條 bug；slash 指令互動性表；「終點要換型別不是換傳輸」 |
| C 渲染覆蓋 | Opus | `scratchpad/reportC.md` | 35,465 筆實測盤點；「12 缺 10 在檔裡」；now row 設計 |
| D terminal／生命週期 | grok 4.6 xhigh | `scratchpad/reportD.md` | attach 是尺寸問題根因；pid 身分毀對話；`--bg` 評估 |
| E remote／輪詢 | Opus | （訊息內） | 三處無 timeout；remote subagent scan 每秒重讀整檔；`ListClaudeSessionFiles` 無邊界 |

## 附錄 B：引用的官方文件

- https://code.claude.com/docs/en/hooks.md（PermissionRequest／Notification／Stop／PreCompact／MessageDisplay）
- https://code.claude.com/docs/en/statusline.md（`context_window` JSON）
- https://code.claude.com/docs/en/channels.md、https://code.claude.com/docs/en/channels-reference.md（channel 協定、permission relay、dev flag）
- https://code.claude.com/docs/en/cross-session-messaging.md（inbox socket、inbound 規則）
- https://code.claude.com/docs/en/remote-control.md（URL 形式、網頁能做的指令、無第三方 API）
- https://code.claude.com/docs/en/agent-view.md（`--bg`／`attach`／`respawn`／`agents --json` 欄位）
- https://code.claude.com/docs/en/headless.md（`-p --resume` 是新 process；`-p` 拒 `--bg`）
- https://code.claude.com/docs/en/sessions.md（transcript 格式為 internal、會變）
