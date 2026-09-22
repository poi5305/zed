# Claude Sessions 面板 — 架構（給接手的 agent）

2026-09-22 版。對象是要動 `crates/claude_sessions`、`crates/remote/src/claude_sessions.rs`、`crates/remote_server/src/headless_project.rs`、`crates/proto/proto/claude_sessions.proto` 的 agent。使用者文件在 `docs/claude-sessions-setup.md`；這一版的規格、裁決與 WP 紀錄在 `docs/claude-terminal-rail.md`；前一階段（訊息輸入框＋channel）的交接在 `docs/claude-health-check-progress.md`。

## 1. 一句話

**讀**靠三份檔（transcript、hook events、statusLine），**寫**只剩 channel 的兩件事（interrupt、permission），**顯示**是「內嵌一個 attach 到 tmux pane 的 `TerminalView`，右邊一條 rail 只畫 terminal 顯示不了的東西，中間一條 gutter 把 rail 對齊到 terminal 的行」。使用者的輸入全部在那個 terminal 裡做，面板不送文字。

## 2. 資料流

```
 Claude Code CLI（tmux pane 裡跑；面板的 TerminalView 透過 tmux 鏡像 attach 上去）
   │ hooks（13 事件）              │ statusLine（300 ms debounce）     │ MCP stdio
   ▼                               ▼                                   ▼
 ~/.claude/hooks/zed-claude-events.sh   ~/.claude/hooks/zed-claude-status.sh   ~/.claude/zed-channel/server.mjs
   │ append 一行 JSONL                │ tmp+rename                        │ claude process 的子行程
   ▼                                 ▼   （＋串接使用者原本的指令）      ▼
 ~/.claude/zed-events/<session_id>.jsonl  ~/.claude/zed-status/<session_id>.json   ~/.claude/zed-channel/<claude_pid>/
                                                                                    ├ server.json   （heartbeat、features）
 ~/.claude/projects/*/<session_id>.jsonl（transcript，不動）                        ├ inbox.jsonl   （server → Zed）
 ~/.claude/sessions/<pid>.json          （registry，不動；tmux 欄位是 attach 的來源） └ outbox/*.json （Zed → server：只有 interrupt／permission）
   │ 250 ms tail        │ 250 ms tail        │ 1 s read      │ 1 s scan（每 3 次跑一次 `claude agents --json`）│ 250 ms tail / 1 s status
   └────────────────────┴────────────────────┴───────────────┴──────────────────────────────────────────────────┘
                                   SessionSource（LocalSource = fs；RemoteSource = proto RPC → headless_project → 同一套 fs 碼）
                                                  │
                                   ClaudeSessionStore（身分 = sessionId；LiveState；StatusSnapshot；ChannelInboxEvent；polls；attach_arguments）
                                                  │
                                   ClaudeSessionsPanel
                                     ├ L1 toolbar / status line       ← statusLine、hook events、Transcript::spend()
                                     ├ TerminalView（tmux mirror attach）← project.create_terminal_task
                                     ├ L3 gutter                       ← Terminal::last_content() + reachable_anchors
                                     ├ L2 rail（entries／EntryCache／render_entry，rail_worth 過濾）
                                     └ permission 卡 / question 卡（唯讀）
                                   ── 寫 ──► outbox/（interrupt、permission）
```

面板對 terminal 的關係是**只看不碰**：不 `capture-pane`、不 `send-keys`。它讀的是 Zed 自己那個 `Terminal` 的 `last_content()`（渲染格子），寫的只有 tmux 的 attach 命令本身。使用者打字是打進 TerminalView，走 Zed terminal 正常的 PTY 路徑，面板碼不經手。

## 3. 檔案協定（沒變的部分只寫重點）

### 3.1 events JSONL — `~/.claude/zed-events/<session_id>.jsonl`

`{received_at_ms, event}`，`event` 是 hook payload 原樣。一行一事件、單一 `write(2)`。`session_id` 不是字串或含 `/`、`.`、`..` 就不寫。超過 8 MiB 截斷，tail 見縮小就重來。`live_state::parse_hook_event` → `HookEvent`，`LiveState::apply` 折疊。

### 3.2 status JSON — `~/.claude/zed-status/<session_id>.json`

CLI statusLine 的 stdin 原樣。讀端拒收超過 1 MiB（`MAX_STATUS_FILE_BYTES`）。`StatusSnapshot::parse` 每個欄位各自 optional。`chained-command.txt` 放使用者原本的 statusLine 指令。

### 3.3 channel — `~/.claude/zed-channel/<claude_pid>/`（協定 1，全文在 `crates/remote/assets/zed-claude-channel/README.md`）

- `server.json`：`{pid, claude_pid, started_at_ms, heartbeat_at_ms, protocol:1, features:[...]}`，5 秒重寫；live = 存在、非 symlink、heartbeat 20 秒內（`CHANNEL_HEARTBEAT_STALE_MS`）。server 目前仍宣告 `["message","permission","interrupt"]`。
- `outbox/<13 位 ms>-<4 位 seq>.json`：tmp+rename。**Zed 現在只寫兩種**：`{"kind":"permission","request_id","behavior":"allow"|"deny"}`、`{"kind":"interrupt","reason"}`。`kind:"message"` 的寫入碼（`channel_send_message`、store `send_message`、proto `ClaudeChannelSend`）還在 crate 裡，但面板沒有任何呼叫它的路徑；server 端也還接受它。
- `inbox.jsonl`：`ready`、`permission_request{request_id,tool_name,description,input_preview}`、`permission_answered`、`message_sent`、`interrupted`、`error`、`closed`。未知 kind → `ChannelInboxEvent::Unknown`。`error.reason` 是開放集合。

權限配對：hook 的 `PermissionRequest`（有 `tool_use_id`）與 inbox 的 `permission_request`（有 `request_id`）用同 `tool_name` ＋ `|at_ms − since_ms| ≤ 5 s` 配，最新未答優先。

Interrupt：outbox 一個檔 → server 對 `ppid` 送**一次** SIGINT，3 秒節流、mtime 超過 10 秒拒收。Zed 端 `can_interrupt` = channel live ∧ `features ∋ "interrupt"` ∧ `Turn::Running`；面板另有 5 秒的 `interrupt_sent_at_ms` 守衛（`interrupt_is_awaiting_result`），連按不會排第二個檔。

## 4. 內嵌 terminal

### 4.1 為什麼是 tmux 鏡像，不是直接 attach

`attach_arguments(tmux_field)`（`crates/remote/src/claude_sessions.rs`）。registry 的 `tmux` 欄位形狀是 `session:@window.%pane`。

- 直接 `tmux attach -t <session>` 不行：一個 session 只有一個 current window，所有 client 看到的都是它。同一個 tmux session 裡開兩個 Claude Code（兩個 window），兩個面板都會被帶到使用者目前所在的 window，而不是各自跑的那個。
- 所以建一個**同 group 的 session**（`new-session -t =<name>`）：共用 windows、但 current window 自己管。命令列順序有意義（`mirror_arguments` 的 doc comment）：

```
new-session -d -t =<session> -s zed-claude-mirror-<zed pid>-<seq> ;
select-window -t <mirror>:@<window> ;
attach-session -t <mirror> ;
set-option -t <mirror> destroy-unattached on
```

  `-d` 先不 attach（否則後面的命令會作用到來源 session）；`=` 要求名字精確匹配；`select-window` 在有 client 之前做，畫面不會跳；`destroy-unattached` 放最後，否則沒 client 的瞬間就被銷毀。mirror 名帶 `MIRROR_SEQUENCE`，因為失敗的命令會讓 tmux 放棄剩下的清單，撞到還沒 detach 完的舊 mirror 名就沒有 terminal。
- session 名結尾是 `;` 會把命令列切斷（tmux 把結尾分號當命令分隔），`with_any_trailing_semicolon_escaped` 補反斜線；只有結尾那一個要處理。
- 欄位沒有 `@window` id 就退回 `attach -t %pane`（單 window 的 session 這樣就對）。
- `pane_target`／`window_target` 只接受 `%數字`／`@數字`，registry 裡的東西永遠到不了 tmux 當 flag。
- `is_zed_mirror_session`（前綴 `zed-claude-mirror-`）留給 tmux session 列表過濾。

`attach_arguments` 每呼叫一次就 `fetch_add` 一個 mirror 序號，所以**只能在真的要 attach 時呼叫**，不能每 frame 呼叫——這是下面三態存在的理由之一。

### 4.2 身分是「pane ＋ process」，不是 conversation id

```rust
struct TerminalTarget { pane: String, process_id: u32 }
```

`session_store::apply_registry_scan` 判 `/clear` 的方式是 `previous_by_pid`：同一個 `process_id` 換了 `session_id`。process 沒換、tmux pane 沒換，只有對話 id 換了。如果 terminal 的身分是 sessionId，每次 `/clear` 都會拆掉 TerminalView 重建：捲動歷史歸零、多鑄一個 mirror。所以 `wanted_terminal(in_pane, transcript_is_main, live_session)` 回的是 `pane_target(tmux_field)` ＋ `process_id`；`/clear` 前後相等。

`wanted_terminal` 回 `None` 的情況：不是 in_pane（dock 模式）、正在讀 subagent（`TranscriptTarget::Subagent`）、沒選到活著的 session、`tmux` 欄位沒有 pane id。

### 4.3 `terminal_sync` 三態

```rust
fn terminal_sync(current: Option<&TerminalTarget>, wanted: Option<&TerminalTarget>) -> TerminalSync
// current == wanted        → Keep    （含 /clear）
// wanted.is_some()         → Attach  （換 pane 或換 process：丟掉重 attach）
// 否則                     → Drop    （沒有可 attach 的東西：丟掉）
```

`sync_terminal` 每次 render 都跑（`Render::render` 在 `in_pane` 時第一件事）。**只有 `Attach` 會讀 `store.attach_arguments()`**，所以 mirror 序號不會白燒。Attach 的實際路徑：

1. `SpawnInTerminal { command: "tmux", args: attach_arguments, use_new_terminal: true, allow_concurrent_runs: true, reveal: NoFocus, id: "claude-session-attach-<pid>" }` → `project.create_terminal_task(spawn, cx)`（remote project 也走這條，所以 remote host 的 tmux 也 attach 得到）。`command` 當程式名、`args` 逐個傳，不經 shell。
2. 成功後 `TerminalView::new(terminal, workspace, None, project, window, cx)`，`set_embedded_mode(None)`、`set_show_workspace_actions(false)`。
3. 中途 `terminal_for` 變了就丟結果（`_terminal_attach` 是 `Task`，重 attach 時被覆蓋即取消）。
4. 失敗把錯誤寫進 `lifecycle_note`，`terminal_placeholder` 顯示；其他 placeholder：`Attaching…`、`This session is not running in a tmux pane. Attach from the sessions list.`、`This session has ended.`、`SELECT_A_SESSION`。

`Focusable::focus_handle` 委給 terminal 的 focus handle（`terminal_focus_handle`），沒 terminal 才用面板自己的。

### 4.4 為什麼切到 subagent 要 Drop 而不是「留著不畫」（§5.6 裁決）

mirror 與本尊同一個 session group、共用 window。一個看不見卻仍 attached 的 client 會把使用者真正在用的 pane 尺寸壓到它自己的大小（`window-size latest` 下尤其明顯）。丟掉捲動歷史，比縮掉使用者的 pane 便宜。`a_subagent_does_not_embed_a_terminal` 鎖住這個行為。

## 5. rail（L2）

### 5.1 版面

`work_area(rail_expanded, reading_an_agent)`：

| | 結果 | 畫什麼 |
|---|---|---|
| 讀 subagent | `RailOnly` | 只有 transcript list（沒有 terminal 可對齊，收合 rail 會變空白，所以忽略 `rail_expanded`） |
| rail 展開 | `TerminalAndRail` | terminal（flex_grow）＋ gutter ＋ rail（固定 380 px） |
| rail 收合 | `TerminalOnly` | terminal ＋ gutter |

rail 內容 = `render_transcript_section`（`list(list_state, render_entry)` ＋ scrollbar ＋「N new below」＋ now row）＋ `render_live_message`（Saying 框）。權限卡、問題卡、狀態列畫在工作區**下方**，不在 rail 裡。

### 5.2 挑選規則 `rail_worth(kind) -> TerminalHasIt | Draw`

| Draw | TerminalHasIt |
|---|---|
| `Image`、`SentFiles`、`Attachments`、`TurnSummary`、`TurnFooter`、`CompactBoundary`、`SystemNote`、`Unknown` | `Message`、`Thinking`、`LocalCommand`、`SlashCommand`、`Queued` |
| `ToolUse` 且 `diff.is_some()`（Edit） | `ToolUse` 無 diff |
| `ToolResult` 是 `Persisted`，或 `Inline` 超過 `MAX_UNCLAMPED_OUTPUT_LINES`（12）行 | `ToolResult` 短的 inline |

`render_entry`：`show_body = rail_shows_everything || rail_worth == Draw`。不畫 body 的 entry **仍佔一個 index**，回一個零高度 `div`（或費用卡）——`list_state` 以 `entries` 索引，拿掉會讓 splice 全錯。`rail_shows_everything`（工具列 i 圖示）是退路開關：開了就是舊的完整對話畫面，這時不畫 turn 費用卡，改畫每則訊息底下的 `answer_cost` 費用行，避免兩套並存。

### 5.3 費用卡怎麼算（§5.7 裁決，照 code 寫）

**事實**：Claude Code 一個 content block 寫一筆 record。同一次 API 呼叫的 2–3 筆 record uuid 不同、`requestId` 相同、各帶一份 `message.usage`。前面幾筆是串流中的快照（`output_tokens` 只有個位數），**最後一筆才是完整值**；input 與 cache 三欄全程相同。使用者真實 transcript 量測：不去重膨脹 1.68×，取第一筆少算 14.8% output。

**實作**（`claude_sessions_panel.rs`）：

1. `billing_of_path(path) -> HashMap<record_base_key, Billed { call_id, usage }>`：對 path 上**每一筆有 usage 的 record**（不是每個 entry——83.5% 的計費 record 沒有 text block，根本不會變成帶 usage 的 `Message` entry）建一筆。`call_id` = `requestId`；沒有或空字串的 record 自己當一次呼叫（`base_key`）。
2. `turn_billing(entries, turn_start, billing)`：走這一輪的 entries，`billed_entry` 把 entry 對回它的 record（`record_id_of(key)` 去掉 `#block`），同一 `call_id` **第一次出現時占位、之後每次覆蓋 usage**——結果就是「取最後一筆」，而且呼叫順序不會亂跳。回 `(Vec<Usage> 每次呼叫, Usage 合計)`。
3. **在 `collapse_turns` 之前算**：`rebuild_entries` 先 `self.turn_bills = turn_bills(&new_entries, &billing)`，鍵是每輪第一個 entry（user message）的 key。理由：`collapse_one_turn` 預設把 `Thinking`／`ToolUse`／`ToolResult` 丟掉，而一次呼叫**最後一筆** record 最常見的正是 thinking 或 tool_use block；從收合後的 entries 算，最常見的 turn 形狀整張卡消失。
4. 卡掛哪裡：`cost_anchor_at(entries, index, has_calls, billing)`——這輪有 `TurnSummary` 就掛在 summary 上；沒有（正在跑、或 Expand all tool calls）就掛在這輪最後一個有計費的 entry 上。`has_calls` 由呼叫端從 pre-collapse 的帳給，不在這裡重算。
5. `render_turn_cost`：`displayed_turn_usage` 取帳 → `rates_for_model(store.transcript().spend().model)`，`None` 就不畫卡 → 預設一行合計，展開（key `<turn_id>#cost`，已放進 `live_cache_keys` 才不會每 250 ms 自己收合）變成每次呼叫一行。格式 `answer_summary(usage, rates, None)`。
6. `Transcript::spend()`（`transcript.rs`）同一套規則：先掃一遍建 `last_of_call: requestId → index`，主迴圈遇到有 `requestId` 但不是最後一筆的就 skip。skip 放在 cost-state／compact boundary 分支**之後**，靠結構成立（能走到那裡的 record 必有 usage、必在 `last_of_call`）。`cost-state` record（CLI 自己的總帳）優先於 token 推算。

**取捨**：`rates_for_model` 用整個 transcript 最新那筆的 `model`，換模型之後舊輪也用新費率（與既有 `answer_cost` 一致，T2R1 wontfix）。`Usage::cost` 只計 input／cache write 1h／5m／cache read／output；`thinking_tokens` 有 parse、`answer_summary` 以 `(N thinking)` 括號附在 `out` 後面，但不另計價（thinking 已含在 output 裡）。`answer_summary` 的格式：`[時間 ·] $ · in · cache read · cache write (1h) · cache write (5m) · out · (thinking)`，為 0 的欄位省略。

## 6. gutter（L3）

### 6.1 純函式層 `terminal_anchors.rs`（無 gpui，盲測 `blind_anchor_tests.rs` 46 條鎖住）

- `screen_rows(cells: (line, column, char), screen_lines)`：只收 `0..screen_lines` 的 line，缺的 column 補空白，同格後者覆蓋前者，每列 trailing space 去掉。
- `anchor_rows(rows, glyphs)`：行首（允許前導 U+0020）是 `user_prompt` 或 `assistant` glyph、後面至少一個 U+0020、再有非空白文字，才是錨點。`> ` 加游標那行（沒有文字）不是。`⎿` 不是。折行的第二行沒 glyph，不是。
- `skeleton(text)`：只留字母數字、小寫。markdown 記號、標點、空白全不算。
- `rows_match`／`skeletons_match`：glyph 相同、短的骨架是長的前綴；任一側 `< 6` 字元（`MIN_SKELETON`）就要求完全相等；**兩側骨架都空回 false**（`⏺ ✅` 這種純符號行不配任何東西）。
- `align(screen, transcript)`：LCS，screen 上→下、transcript 舊→新，每個最多用一次；表從尾端回溯、平手取 transcript 較新那格。只看最後 512 個 transcript 錨點（`MAX_TRANSCRIPT_ANCHORS`）。

### 6.2 面板端

`render_anchor_gutter`（28 px，`ANCHOR_GUTTER_WIDTH_PIXELS`；只在有 terminal 且不是 `RailOnly` 時畫，`anchor_gutter_shown`）：

1. 讀 `terminal.last_content()`：`cells` 濾掉 `is_wide_char_spacer()`，`point.line` 經 `visible_grid_line(line, display_offset)` 轉成可見列號——grid line 是相對螢幕頂端的格線編號，捲回歷史時是負數，不加 `display_offset` 會讓最上面幾列 chip 消失、其他整排畫高（T3R1-1）。
2. `screen_rows` → `anchor_rows(glyphs)`，glyph 來自 `ClaudeSessionsSettings.user_prompt_glyph`／`assistant_glyph`（settings key `claude_sessions.user_prompt_glyph`／`assistant_glyph`，字串；`configured_glyph` 只收剛好一個字元，否則預設 `>`／`⏺`）。
3. transcript 端用 `reachable_anchors`（見下），`transcript_window_for_screen`：在底部就用尾端 512 窗；**捲回歷史時**改選「覆蓋最多可見錨點列」的 512 窗，平手取較新（T3R2-1，否則尾端窗涵蓋不到畫面）。
4. `align` → `Anchoring { row, key }`。
5. `AnchorCache`：鍵是 `entries_generation`、glyphs、columns、screen_lines、line_height、cell_width、**rows 本身**、scrolled_back。`grid_lines_change` 在格子原地改寫時不會變，所以不能拿它當鍵。cache miss 但 `entries_generation` 沒變時，transcript 錨點沿用（`reusable_anchors`），不重 parse 每個工具的 input JSON（T3R1-2）。
6. chip 用 `absolute().top(line_height * row)` 定位，點了 `reveal_anchored_entry(key)`：從**最新** `reachable_anchors` 查 index（不是畫 chip 那一 frame 的），`scroll_to_reveal_item` 並展開。

### 6.3 錨點在 collapse 前算、chip 掛在工具呼叫那一行（§5.8／§5.9 SG2）

`reachable_anchors(pre_collapse, collapsed)`：從 collapse **前**的 entries 建 `TranscriptAnchor`（`Message` → 第一行原文；`ToolUse` → `Name(target)`，與 `tool_target` 同一個字串），再記每個錨點在 collapse 後**可到達的** index：entry 自己還在就是它，被收掉就是那一輪的 `TurnSummary`。理由與費用卡相同——收合會丟掉 `ToolUse`，不先算的話已結束的輪一個 `⏺` 錨點都沒有。

chip（`anchored_chip`）：`Image`／`ToolResult` 在 TUI 上沒有自己的錨點行（結果行首是 `⎿`，圖片夾在結果裡），所以掛到它所屬的那次 `ToolUse`：

| `ToolUse` 的條件 | chip |
|---|---|
| 是 `Edit` 且有 diff | `FileDiff` |
| 結果（`call_results`：先用 `tool_use_id` 配，沒 id 才用位置）含 `Image` | `Image` |
| 結果是 `Persisted` 或超過 12 行 | `ExpandVertical` |
| 以上皆非 | 不畫 |

優先序 diff > 圖片 > 展開。`Message` 錨點行畫 `ChevronRight`。

### 6.4 降級

- 沒 attach 上 terminal → 沒有 gutter。
- 畫面上沒有能配對的行、或使用者改了 glyph 卻沒設 settings → 沒有 chip，不會畫錯位置。
- 純符號行不配對（SG1）。
- 平行 `tool_use` 靠 `ToolResult.tool_use_id` 配（SG3）；只有圖片沒有文字的 tool_result 沒有 `ToolResult` entry，少畫不畫錯（T3R2-2 wontfix）。

## 7. 模組地圖（只列這一版新增或改動的；其他見 `docs/claude-health-check-progress.md` 時期的描述仍成立）

- `crates/claude_sessions/src/claude_sessions.rs`：`ClaudeSessionsSettings { dock, user_prompt_glyph, assistant_glyph }`；actions 只剩 `ToggleFocus`、`OpenInEditor`；`mod terminal_anchors`、`mod usage`、五個 blind test mod。
- `crates/claude_sessions/src/terminal_anchors.rs`：§6.1。
- `crates/claude_sessions/src/usage.rs`：`Usage::from_record`（含 `cache_creation` 1h／5m 拆分、`thinking_tokens`）、`context_tokens`、`cost(ModelRates)`、`rates_for_model`（依家族前綴，不認識回 `None`）。
- `crates/claude_sessions/src/claude_sessions_panel.rs`（約 19.6 k 行）：`TerminalTarget`／`TerminalSync`／`terminal_sync`／`wanted_terminal`／`sync_terminal`／`terminal_placeholder`／`render_terminal_area`／`render_terminal_and_rail`／`work_area`；`RailWorth`／`rail_worth`；`Billed`／`billing_of_path`／`billed_entry`／`turn_billing`／`turn_bounds`／`cost_anchor_at`／`turn_bills`／`displayed_turn_usage`／`render_turn_cost`；`AnchorChip`／`anchor_chip`／`anchored_chip`／`call_results`／`visible_grid_line`／`ReachableAnchor`／`reachable_anchors`／`transcript_anchor`／`transcript_window_for_screen`／`AnchorCache`／`cached_anchorings`／`render_anchor_gutter`／`reveal_anchored_entry`；`render_pending_permission`／`answer_permission`；`render_question`（唯讀）；`render_status_line`（含 Stop）／`request_interrupt`／`interrupt_is_awaiting_result`。`rebuild_entries` 的順序：build → `billing_of_path` → **`turn_bills`** → `collapse_turns` → **`reachable_anchors`** → splice。
- `crates/claude_sessions/src/session_store.rs`：新增 `attach_arguments()`（對 selected session）；`take_cleared_rebinds()` 仍在，`rebuild_entries` 只 drain 不讀。`send_message` 仍在但沒有生產呼叫者。
- `crates/remote/src/claude_sessions.rs`：取回 `pane_target`、`window_target`、`attach_arguments`、`mirror_arguments`、`MIRROR_SEQUENCE`；新增 `with_any_trailing_semicolon_escaped`。`channel_send_message`、slash command 列舉、貼檔（`write_session_file`）等碼仍在。
- `crates/settings_content/src/workspace.rs`：`ClaudeSessionsSettingsContent` 多兩個 `Option<String>`。
- `crates/claude_sessions/Cargo.toml`：加 `project`、`terminal`；拿掉 `editor`、`sha2`、`zed_actions`；dev-dep `workspace = { features = ["test-support"] }`（`./script/clippy --all-features` 會打開 remote 的 `test-support`，需要 project／workspace 的對應 match arm）。
- keymap：三個平台的 `ClaudeSessionsPanel`／`ClaudeSessionsInput > Editor` 綁定整段刪掉。

## 8. 不變式

1. **session 身分是 `sessionId`；terminal 身分是 `pane ＋ process_id`。** 同 pid 換 id（`/clear`）是 rebind，terminal `Keep`；pid 不見是 ended row，transcript 留著。
2. **面板不對 terminal 送任何東西。** 沒有 `send-keys`、`paste-buffer`、`capture-pane`。唯一的 tmux 命令是 attach（`attach_arguments`），而且只在 `TerminalSync::Attach` 時呼叫。
3. **channel outbox 只寫 `permission` 與 `interrupt`。** 要加回 `message` 之前先讀 `docs/claude-terminal-rail.md` §0。
4. **費用與錨點都在 `collapse_turns` 之前算。** 收合會丟掉帶完整資料的那筆 record／那個 `ToolUse`。
5. **一次 API 呼叫 = 一個 `requestId`，取最後一筆 record 的 usage。** `turn_billing` 與 `spend()` 兩處都是；改一處要改另一處。
6. **每個 poll 有 timeout 與自己的 `ErrorSource`**；hidden 只是變慢，不停。
7. **不信任的 JSON 不直接索引**（transcript、hook payload、status、inbox、`agents --json`、`last_content` 的字元都用 `get`／`as_*`，降級不 panic）。
8. **spawn 的 operand 走 `claude_command_operand`，cwd 走 `claude_command_working_directory`，子行程走 `output_within`。** tmux 的 pane／window id 走 `pane_target`／`window_target`。
9. **開檔／開 URL 走白名單**：`attachment_is_readable`、`bridge_url`。
10. **hook 腳本任何路徑都不印、exit 0**；installer 先 parse settings 再寫，只留一份 backup。

## 9. 已知取捨（§5 裁決，不是 bug）

- **切到 subagent 會拆掉 terminal**（§4.4）。切回主對話重 attach，捲動歷史從頭。
- **rail 預設模式下「N new below」的數字與畫面對不上**：`unread_below` 算的是 entries 筆數，包括 `TerminalHasIt` 那些零高度的位子。切「Show everything」就對得上。
- **換模型後舊輪用新費率**（§5.3）。
- **`Message` 錨點行永遠畫 `ChevronRight`**，不管 rail 有沒有東西。
- **同一 turn 內 5 秒兩個同名工具的權限提示**只能靠順序配（channel 協定沒帶 `tool_use_id`）。
- **狀態列的 `channel: not loaded — Setup` 沒有對應按鈕**：「Copy setup commands」隨輸入區一起拆了，`channel_setup_commands` 目前沒有 UI 呼叫者，使用者照文件手打。
- **`kind:"message"` 的整條寫入路徑（store／source／proto／server）是死碼但保留**，slash command 列舉、貼檔 RPC 亦同。

## 10. 測試

- **盲測**（held out，實作者與 fixer 不讀不跑不改）：`blind_anchor_tests.rs`（46，`terminal_anchors` 介面）、`blind_live_state_tests.rs`、`blind_registry_tests.rs`、`blind_subagent_tests.rs`、`blind_transcript_tests.rs`。全部以 `#[cfg(test)] mod` 掛在 `claude_sessions.rs`。實作者跑 `cargo test -p claude_sessions -- --skip blind_`，含盲測的完整 gate 由大腦跑。
- **回歸測試**與 code 同檔：`claude_sessions_panel.rs mod tests`（terminal 三態、`/clear` 不重建、rail_worth、turn_billing 多 record 去重、cost_anchor、`turn_bills` 不長大、錨點 pre-collapse、chip 歸屬、`display_offset`、512 窗選擇、Stop 守衛）；`terminal_anchors.rs mod tests`；`transcript.rs`（spend 去重、skip 位置）；`usage.rs`；`crates/remote/src/claude_sessions.rs`（mirror 命令、結尾分號、pane／window target）。
- **channel server**：`crates/remote/assets/zed-claude-channel/server.test.mjs`（`node --test`）。
- Gate（T3-review-2 收工時）：`cargo test -p claude_sessions -p remote` 567 + 195、`./script/clippy -p claude_sessions -p remote -p remote_server` 零 warning、`cargo fmt --check`、`node --test server.test.mjs`。
