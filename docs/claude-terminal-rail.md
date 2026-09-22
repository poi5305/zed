# Claude Session：terminal 為主、rail 註解 —— 規格與進度

> 給「大腦」（Claude 主 session）在 context compact 之後接手用。每完成一個工作包（WP）就更新。
> 前一階段（訊息輸入框 + channel）的交接在 `docs/claude-health-check-progress.md`；那一階段的架構文件是 `docs/claude-sessions-architecture.md`。

## 0. 使用者拍板（2026-09-22）

- **放棄 message 輸入框**。打字回到 terminal（tmux + Claude Code CLI）。`/context` 等 CLI 本地指令由 terminal 處理。
- **channel MCP server 留著**（`crates/remote/assets/zed-claude-channel/`）：Interrupt、權限 Allow／Deny 仍走 channel。只拆「輸入框 + 送訊息」這條 UI。
- **panel 內嵌 TerminalView**（不是掛到既有 terminal pane 旁邊）：第 3 層行級對齊要讀該 terminal 的格子。
- 頂部照舊顯示 context bar／模型／effort／累計花費／turn 狀態／背景指令。
- 每一輪都顯示 input／cache read／cache write／output／thinking token 與美金。
- 派工規則沿用 `~/.claude/CLAUDE.md`；同一時間最多 2 隻 agent，**同一時間只有一隻改 Rust**；大腦不寫 code，只寫文件與 prompt。

## 1. 三層架構（一句話各一）

| 層 | 內容 | 與 terminal 同步 | 資料來源 |
|---|---|---|---|
| L1 header | 既有 `render_conversation_toolbar`（context meter、模型、effort、花費、tokens_left、rate limit）+ `render_status_line` | 不用 | statusLine json、hook events、transcript spend |
| L2 rail | 右側固定寬欄，turn 級卡片：費用、圖片縮圖、被截斷的 tool 輸出、Edit diff、subagent、SendUserFile、權限卡（Allow／Deny 走 channel）、問題卡（唯讀） | 不用，只按 turn 排 | transcript（重用 `entries`／`EntryCache`／`render_entry`） |
| L3 gutter | terminal 與 rail 之間的窄欄，chip 對齊 terminal 可見行 | 要：讀 `Terminal::last_content().cells`，行首 `>`／`⏺` 錨點與 transcript 單調對齊 | `crates/terminal` `Content` + transcript |

版面（in_pane 模式）：

```
toolbar（L1）
error / agent chips（既有）
┌──────────────────────────────┬──┬──────────────┐
│ TerminalView（tmux attach）   │L3│ rail（L2）    │
│ flex_grow                    │  │ 固定寬，可收合 │
└──────────────────────────────┴──┴──────────────┘
pending permission 卡（既有，channel）
status line（L1）
```

dock 模式（session 列表）不變。

## 2. 事實清單（實作者與盲測共用）

### 2.1 既有程式位置（`crates/claude_sessions/src/claude_sessions_panel.rs`，20 901 行，基線 commit `0904ace174`）

- `pub struct ClaudeSessionsPanel` 868–1033；欄位含 `message_editor: Entity<Editor>`、`pending_sends: PendingSends`、`input_expanded`、`history_index`、`question_ticks`、`ticked_question`、`slash_send_note`、`opened_input_for_question`、`file_matches*`、`file_query`、`file_highlight`、`dismissed_file_menu_for`、`_listing_files`、`_pasting`、`paste_error`、`slash_highlight`、`dismissed_slash_menu_for`、`slash_commands*`、`_listing_slash_commands`、`_editor_subscription`。
- `impl Render` 7869–7913：`in_pane` 時依序 `render_conversation_toolbar` → `render_error` → `render_agent_chips` → `render_transcript_section` → `render_live_message` → `render_pending_permission` → `render_question` → `render_input`（非 subagent）→ `render_status_line`（非 subagent）。
- `render_transcript_section` 4990–5075：`list(self.list_state, render_entry)` + scrollbar + 「N new below」+ `render_now_row`。
- 輸入框相關：`render_input` 5952–6114、`render_slash_commands` 5536、`render_file_matches` 5645、`send_message` 5117、`dispatch_message` 5161、`can_send` 5094、`sync_input_availability` 5105、`handle_message_editor_event` 2393、`recall_previous/next_message` 2451／2475、`@` 檔案選單 2500–2676、slash 選單 2677–2831、`toggle_input` 2880、`open_the_input_for_a_new_question` 2891、`dismiss_menus` 5262、`PendingSend`／`PendingSends` 8078–8530、歷史 `message_history*` 699–811、`MenuStep`／`EnterInMenu`／`CursorDirection`／`HistoryStep` 622–698、`SlashSendDecision`／`slash_send_decision` 10048–10087、`compose_*_answer` 10088–10106、`pending_send_note` 10107。
- 權限卡 `render_pending_permission` 5705、`answer_permission` 5184（走 store → channel，**保留**）。問題卡 `render_question` 5791（現在點選項會組文字送出；改成唯讀）。
- `spawn_in_terminal` 4367–4412：現在用 `TerminalPanel::spawn_task` 開在 terminal dock（Attach／Open in tmux 按鈕用；**保留**）。
- toolbar `render_conversation_toolbar` 5290–5535；`context_meter` 1751；`toolbar_model/effort/cost` 1882–1906；`format_status_line` 1907。
- 費用：`answer_cost` 423、`answer_summary` 444、`session_cost` 497、`format_usd` 511、`compact_token_count` 538；`crates/claude_sessions/src/usage.rs` 的 `Usage { input_tokens, cache_write_1h_tokens, cache_write_5m_tokens, cache_read_tokens, output_tokens, thinking_tokens }`、`Usage::from_record(&Value)`、`context_tokens()`、`cost(ModelRates)`、`rates_for_model`。
- tool 目標文字：`tool_target(block)` 8721、`tool_target_from_input(input)` 8725（`Read(auth.rs)` 括號內的字就是這個）。
- actions（`crates/claude_sessions/src/claude_sessions.rs`）：`ToggleFocus`、`SendMessage`、`DismissMenus`、`OpenInEditor`、`PreviousMessage`、`NextMessage`、`PasteIntoMessage`。keymap：`assets/keymaps/default-{macos,linux,windows}.json` 各有 `claude_sessions::SendMessage/DismissMenus/PreviousMessage/NextMessage/PasteIntoMessage`（macOS 在 1757–1772 行附近）。
- Cargo：`claude_sessions` 已依賴 `terminal_view`、`editor`、`workspace`、`task`、`tmux_sessions`、`remote`。**沒有**直接依賴 `terminal`、`project`（WP6a 拿掉了 `project`）；要用 `Terminal::last_content` 得加 `terminal.workspace = true`，要 `project.create_terminal_task` 得加回 `project.workspace = true`。

### 2.2 store／registry

- `crates/remote/src/claude_sessions.rs`：`pub struct RegisteredSession { process_id: u32 (json "pid"), session_id, working_directory (json "cwd"), process_start, version, kind, name, status, updated_at, tmux_target: Option<String> (json "tmux"，形狀 `session:@window.%pane`，例 `awp:@1.%1`), bridge_session_id }`。`pub fn tmux_session_name(&str) -> Option<&str>`（第 3401 行）、`pub fn claude_command_operand(&str) -> Option<&str>`（第 594 行）、`shell_quote` 第 2278 行。
- `crates/claude_sessions/src/session_store.rs`：`LiveSession { session: RegisteredSession, background, agent_id, state, waiting_for }`（Deref 到 RegisteredSession）；`ClaudeSessionStore::{sessions(), ended_sessions(), selected() -> Option<&str>, selected_process_id() -> Option<u32>, selected_is_ended(), transcript_target() -> TranscriptTarget (Main | Subagent{..}), live(), status(), transcript(), main_transcript(), channel_live(), permission_mode()}`。
- **WP3b（commit `0904ace174`）刪掉、可用 `git show 0904ace174^:<path>` 唯讀取回的舊碼**：
  - `crates/remote/src/claude_sessions.rs`：`pub fn pane_target(tmux_field)`、`pub fn window_target(tmux_field)`、`pub fn attach_arguments(tmux_field) -> Option<Vec<String>>`、`fn mirror_arguments(session_name, window_target, mirror_name)`、`MIRROR_SESSION_PREFIX`、`MIRROR_SEQUENCE`，以及它們的測試。mirror 做法：`tmux new-session -d -t =<session> -s <mirror> ; select-window -t <mirror>:<@window> ; attach-session -t <mirror> ; set destroy-unattached`（見該 commit 的 doc comment，順序有意義）。
  - `session_store.rs`：`pub fn pane_target(&self)`、`pub fn attach_arguments(&self) -> Option<Vec<String>>`（對 selected session）。
  - panel：欄位 `terminal: Option<Entity<TerminalView>>`、`terminal_process_id`、`_terminal_attach: Task<()>`、`pane_expanded`；`fn sync_terminal(&mut self, window, cx)`：以 `SpawnInTerminal { command: Some("tmux"), args: attach_arguments, use_new_terminal: true, allow_concurrent_runs: true, reveal: RevealStrategy::NoFocus, .. }` 呼叫 `project.update(cx, |p, cx| p.create_terminal_task(spawn, cx))`，成功後 `cx.new(|cx| TerminalView::new(terminal, workspace, None, project.downgrade(), window, cx))`；選了別的 session 就丟掉（比對 process id）。

### 2.3 terminal API

- `crates/project/src/terminals.rs`：`Project::create_terminal_task(&mut self, SpawnInTerminal, cx) -> Task<Result<Entity<Terminal>>>`（remote project 也走這條）。
- `crates/terminal_view/src/terminal_view.rs`：`TerminalView::new(terminal: Entity<Terminal>, workspace: WeakEntity<Workspace>, workspace_id: Option<WorkspaceId>, project: WeakEntity<Project>, window, cx)`（233 行）；`set_embedded_mode(&mut self, max_lines_when_unfocused: Option<usize>, cx)`（311 行）；`set_show_workspace_actions(&mut self, bool, cx)`（328 行）；`terminal(&self) -> &Entity<Terminal>`（872 行）。agent_ui 的 agent_panel 是內嵌 TerminalView 的既有範例。
- `crates/terminal/src/terminal.rs`：`Terminal::last_content(&self) -> &Content`（1952 行）。`pub struct Content { cells: Vec<IndexedCell>, mode, total_lines, display_offset, columns, screen_lines, selection_text, selection, cursor, cursor_char, terminal_bounds: TerminalBounds, last_hovered_word, grid_lines_change: GridLinesChange (Unchanged|Changed), scrolled_to_top, scrolled_to_bottom, bottom_row_occupied }`。`IndexedCell { point, cell }`，`point.line`（i32，可為負＝scrollback）與 `point.column`；`cell.character() -> char`（`crates/terminal/src/alacritty.rs` 6398 行）。`terminal_bounds` 帶 `line_height`、`cell_width`、`bounds`（terminal_element 用 `origin.y + point.line * line_height` 定位）。

### 2.4 Claude Code TUI 錨點（觀察到的慣例，不是 API，放 settings 可改）

- 使用者輸入行以 `>` 開頭（`> fix the login bug`）；目前輸入框那一行也是 `> `＋游標，**它不對應任何 transcript 訊息**。
- assistant 每一段文字／每個 tool call 以 `⏺` 開頭（`⏺ I'll look at auth.rs first.`、`⏺ Read(auth.rs)`）。
- tool 結果以 `⎿` 開頭（`⎿ Read 250 lines (ctrl+o to expand)`），不當錨點。
- 長行會折行；只有第一行有 glyph。markdown 會被渲染（`**bold**` 只剩 bold），所以比對要用「只留字母數字、小寫」的骨架。

## 3. 凍結介面：`crates/claude_sessions/src/terminal_anchors.rs`（純函式，無 gpui；盲測與實作共用）

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnchorGlyph { UserPrompt, Assistant }

/// One visible terminal row. `row` is 0-based from the top of the visible screen.
/// `text` is the row's characters joined in column order with trailing spaces trimmed
/// (wide characters appear once; the spacer cell is skipped).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenRow { pub row: usize, pub text: String }

/// A row that starts an anchor: after optional leading spaces, `>` or `⏺`, then at least
/// one space, then the text. `text` is what follows the glyph, trimmed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnchorRow { pub row: usize, pub glyph: AnchorGlyph, pub text: String }

/// A transcript message the screen may show. `key` is the entry key the panel already
/// uses; `text` is the message's first line as the transcript holds it (raw markdown for
/// text, `Name(target)` for a tool call, exactly what `tool_target` gives).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptAnchor { pub key: String, pub glyph: AnchorGlyph, pub text: String }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Anchoring { pub row: usize, pub key: String }

pub struct Glyphs { pub user_prompt: char, pub assistant: char }
impl Default for Glyphs { /* '>' and '⏺' */ }

pub const MIN_SKELETON: usize = 6;
pub const MAX_TRANSCRIPT_ANCHORS: usize = 512;

/// Keeps only alphanumeric characters, lowercased (Unicode alphanumerics included).
pub fn skeleton(text: &str) -> String;

/// Picks the rows that start an anchor, top to bottom. A row whose text after the glyph
/// is empty is not an anchor (that is the input line).
pub fn anchor_rows(rows: &[ScreenRow], glyphs: &Glyphs) -> Vec<AnchorRow>;

/// Whether a screen row can stand for a transcript anchor: same glyph, and the shorter
/// skeleton is a prefix of the longer one. Skeletons under `MIN_SKELETON` (= 6) chars
/// must be equal.
pub fn rows_match(screen: &AnchorRow, transcript: &TranscriptAnchor) -> bool;

/// Order-preserving alignment (longest common subsequence under `rows_match`): screen
/// rows top→bottom, transcript oldest→newest. Each screen row and each transcript
/// anchor is used at most once. Ties prefer the newest transcript anchors (the screen
/// shows the tail of the conversation). Returns anchorings sorted by `row`.
/// Only the last `MAX_TRANSCRIPT_ANCHORS` (= 512) transcript anchors are considered.
pub fn align(screen: &[AnchorRow], transcript: &[TranscriptAnchor]) -> Vec<Anchoring>;

/// Builds `ScreenRow`s from what the terminal reports: cells with `point.line` in
/// `0..screen_lines` only (negative lines are scrollback), grouped by line, ordered by column.
/// Rows with no cells at all still appear (with empty text) so `row` stays the screen row;
/// the result has exactly `screen_lines` rows. A missing column is a space.
/// Takes `(line: i32, column: usize, character: char)` triples so it stays free of the
/// terminal crate's types.
pub fn screen_rows(cells: impl Iterator<Item = (i32, usize, char)>, screen_lines: usize) -> Vec<ScreenRow>;
```

盲測 prompt 全文（含所有已決定行為的例子）：`scratch/blind-anchors-prompt.txt`。

決定的行為：
- 空白處理：`skeleton` 去掉所有非字母數字，所以空白／標點／markdown 記號全不算。
- `rows_match` 的前綴方向雙向：畫面行可能被寬度截斷（畫面較短），transcript 第一行也可能比畫面短（畫面行尾接了折行前的字？不會，折行是另一行；但 tool call 畫面上可能多了 `…`），所以「短的是長的前綴」。
- `align` 平手偏新：同一段文字出現多次時，取 transcript 較新的那些（LCS 從尾端回溯）。
- 錨點只看可見畫面（`0..screen_lines`），scrollback 不看；捲動時每 frame 重算。

## 4. 工作包

| WP | 內容 | 模型 | 狀態 | 報告 |
|---|---|---|---|---|
| T0 | 規格（本檔）、基線測試、事實清單 | 大腦 | 進行中 2026-09-22 | `scratch/baseline-tests.log` |
| T1 | 內嵌 TerminalView（tmux mirror attach，取回 WP3b 刪的碼）+ 拆輸入框／送訊息 UI／PendingSends／5 個 actions＋keymap；問題卡改唯讀；權限卡與 Interrupt 保留；rail 先沿用既有 list 放右欄 | grok 4.7 xhigh | ✅ 10:46。大腦親跑 gate：**claude_sessions 460 + remote 193 綠、clippy 乾淨**（基線 511＋187；刪 57 加 6，數字對帳過）。快照 `scratch/baseline/after-t1.patch` | `scratch/wpT1-report.md` |
| T1-review-1（gemini 嘗試） | ❌ 零產出：4 分鐘後 `exit=0`、252 bytes log、報告沒寫、工作樹與快照 byte 相同。失敗點是它把 cargo check 丟進自己的背景任務就結束回合（與 WP6a 同一個失敗模式）。**結論：agy 不再接需要跑 build 的工作**，已記進 memory | agy gemini-3.8-flash-high | ❌ 11:05 | 無 |
| T1-review-1 | Opus（與作者 grok 不同家族）：4 findings。**T1R1-1 high**：`TerminalTarget` 以 session_id 當身分，`/clear` 會拆掉並重建 terminal（捲動歷史歸零、多鑄一個 mirror）→ 被「不准改既有測試」擋住，**照規則回報 blocked、留兩個紅測試、一行 code 沒動**；**T1R1-2 medium fixed**：tmux session 名結尾是 `;` 會切斷 mirror 命令列（實測 tmux 3.6b `exit 1`、mirror 沒建）→ `with_any_trailing_semicolon_escaped`；T1R1-3（rail 收合＋subagent 畫成空白）與 T1R1-4（`SessionSource::list_slash_commands` 死碼）wontfix。`discoveredWhileFixing`：panel 檔 38 個 rustfmt diff（大腦已跑 `cargo fmt -p claude_sessions` 修掉） | Opus 5 high | ✅ 11:35。gate：remote 195 綠、claude_sessions 460 綠 + 2 刻意紅、clippy 乾淨、rustfmt 乾淨。快照 `scratch/baseline/after-t1-review1.patch` | `scratch/wpT1-review1-report.md` |
| T1-review-2 | grok xhigh，換家族。①T1R1-1 落地：`TerminalTarget { pane, process_id }`、新 `terminal_sync` 三態（`Keep`／`Attach`／`Drop`），只有 `Attach` 會讀 `attach_arguments`，所以 `/clear` 之後不重建也不再鑄 mirror；只改那一個被授權的既有測試，第 1 輪兩個紅測試靠改 code 轉綠。②自己找到 **T1R2-1 fixed**：`rebuild_entries` 不再 take，`cleared_rebinds` 沒有消費者、與 panel 同壽無限長大 → 加 drain。③實測 tmux 驗過第 1 輪的跳脫修法（`done;;` → `done;\;` 正確）。specGap 1 條（切 subagent 拆 terminal）→ 大腦裁決維持現狀，見 §5.6 | grok 4.7 xhigh | ✅ 11:47。**大腦親跑 gate：claude_sessions 466 + remote 195 全綠、clippy 零 warning、兩 crate rustfmt 乾淨、diff 無 `unwrap()`／`let _ =`／`#[allow]`**。快照 `scratch/baseline/after-t1-review2.patch`。**T1 收工** | `scratch/wpT1-review2-report.md` |
| Tblind | `terminal_anchors` 盲測（只看 §3 介面）寫在 `crates/claude_sessions/src/blind_anchor_tests.rs`，held out | Opus 5 medium | 🏃 派出 10:05（Workflow `wf_0ede90e3-848`，slot 2） | `scratch/blind-anchors-report.md` |
| T2 | rail 模式：隱藏純文字 body（terminal 已顯示），每 turn 一張費用卡（in／cr／cw 1h／cw 5m／out／thinking／$），保留圖片／tool 輸出展開／Edit diff／subagent／SendUserFile 卡；rail 可收合。三個純函式 `rail_worth`／`turn_usage`／`work_area` ＋ `rail_shows_everything` 退路開關；併入 T1R1-3。規格 `scratch/wpT2-prompt.txt` | grok 4.7 high | ✅ 12:14。只改 panel 一個檔。大腦親跑 gate：**472 + 195 全綠、clippy 零 warning、rustfmt 乾淨、無 `unwrap()`／`let _ =`／`#[allow]`**。快照 `scratch/baseline/after-t2.patch`。作者自補的決定：截斷沿用 `MAX_UNCLAMPED_OUTPUT_LINES`（12）、一筆 record 的多個 text block 共用一次 usage（key 取第一個 `#` 之前）、`rates_for_model` 回 `None` 就不畫卡、`rail_shows_everything` 時不畫 turn 卡避免兩套並存 | `scratch/wpT2-report.md` |
| T2-review-1 | 換家族（作者 grok → Opus）。重點盯錢：多 text block 去重鍵、turn 邊界、換模型費率、零高度 entry 對 `list_state`／`unread_below` 的影響、`show_costs` 預設翻 true 後 turn 卡那條 render 路徑有沒有測試真的走到 | Opus 5 high | ✅ 12:45。**在使用者真實 transcript 上量測後**找到 7 條。4 fixed：**T2R1-1 high**（`turn_usage` 只讀 `Message` 的 usage，但 83.5% 計費 record 沒有 text block → 整筆錢消失）、**T2R1-2 high**（去重鍵該用 `requestId` 不是 uuid）、T2R1-3（`turn_cost_key` 沒進 `live_cache_keys`，展開的卡每秒自己收合）、T2R1-8（一輪沒寫字就找不到 anchor、整張卡不畫）。3 wontfix（`unread_below` 數字、費用卡 render 無測試覆蓋、換模型費率——與既有 `answer_cost` 一致）。**T2R1-7 high blocked**：`Transcript::spend()` 同樣重複計算，在 T2 diff 之外 → 大腦另派。gate 477 + 195 綠 | `scratch/wpT2-review1-report.md` |
| T2-review-2 | 換家族（Opus → grok）。三件事：①**T2R2-1**（大腦量測發現第 1 輪的修法取每組第一筆，少算 14.8% output，應取最後一筆，見 §5.7）②T2R1-7 落地（授權改 `transcript.rs`；既有測試若把重複計算寫死要停下來回報）③自己的對抗式 review（`billing_of_path` 鍵配對、退路重複計、compaction 換 path、subagent 計費歸屬、`live_cache_keys` 洩漏） | grok 4.7 xhigh | ✅ 13:17。落地 T2R2-1 與 T2R1-7，並自己找到 **T2R2-2 high**：費用是從 **collapse 之後**剩下的 entry 讀的，而 `collapse_one_turn` 預設就把 `Thinking`／`ToolUse`／`ToolResult` 丟掉——帶完整 output 的最後一筆正是被丟掉的那筆，所以「取最後一筆」在預設 rail 上根本不成立（最常見的 thinking｜tool_use 形狀會整張卡不畫）。修法：rebuild 在 `collapse_turns` **之前**先把每輪的帳算好存進 `turn_bills`，卡片讀這份帳。11 個新測試。大腦親跑 gate：**488 + 195 全綠、clippy 零 warning、rustfmt 乾淨**。快照 `scratch/baseline/after-t2-review2.patch` | `scratch/wpT2-review2-report.md` |
| T2-verify | 規則要求：最後一次修正加了新快取 → 補一次**只審 diff** 的窄驗證。兩件事：(A) 大腦發現 `spend()` 的 requestId skip 擺在 cost-state／compact 分支**之前**，靠「Claude Code 不在 cost-state 寫 requestId」這個外部慣例成立（實測 40 檔：cost-state 24 筆、compact 4 筆、帶 requestId 皆 0，**今天不會誤殺**），要改成靠結構成立 (B) 只審新快取 `turn_bills`（會不會長大、早退路徑是否讀到過期帳、key 碰撞、正式路徑有無測試覆蓋） | Opus 5 high | ✅ 13:33。**(A) 大腦的前提是錯的**：磁碟上 skip 在第 302 行，排在 cost-state（276）、compact boundary（287）與 `Usage::from_record`（299）**之後**——大腦讀 `git diff` 時把預先掃描迴圈當成主迴圈了。守衛本來就靠結構成立（能走到 302 的 record 必有 usage，必已進 `last_of_call`）。驗證者沒有去修一個不存在的問題，而是把這個結構性質釘成 2 個回歸測試，並**證明它們咬得到**：暫時把 skip 上提取得真 RED（`None` vs `Some(2.25)`、`680002` vs `16995`），再用編輯還原（非 `git restore`）。大腦已逐行核對非測試部分與前一份快照完全相同。(B) `turn_bills` 四個問題全乾淨：每次整份重建不累加、寫入早退路徑 65 行、key 三個互斥命名空間、正式路徑 `cost_anchor_at` 有兩個測試直接走到。**本輪零非測試 `src/` 改動。** 大腦親跑 gate：**490 + 195 全綠、clippy 零 warning、rustfmt 乾淨**。快照 `scratch/baseline/after-t2-verify.patch`。**T2 收工** | `scratch/wpT2-verify-report.md` |
| T3 | `terminal_anchors.rs` 實作 + gutter 渲染 + glyph 進 settings | grok 4.7 xhigh | ✅ 18:5x。**46 個盲測一次全過、中途沒紅過**，盲測檔 SHA-256 與寫成當下相同（大腦驗過）。大腦親跑 gate：**claude_sessions 549 + remote 195 全綠、clippy 零 warning、rustfmt 乾淨、無 `unwrap()`／`let _ =`／`#[allow]`**。快照 `scratch/baseline/after-t3.patch` + `after-t3-new-files.tgz`。誠實回報兩件事：①動了不在允許清單的 `crates/settings_content/src/workspace.rs`（settings schema 只能放那裡，只加兩個 `Option<String>` 對齊既有 `dock`；大腦已驗 `cargo check -p settings_content -p settings -p claude_sessions` 通過並接受）②圖片／展開 chip 永遠畫不出來（`Image`／`ToolResult` 不是錨點），**沒有自作主張放寬錨點集合** → 大腦裁決見 §5.8 | `scratch/wpT3-report.md` |
| T3-review-1 | 換家族（grok → Opus）。①落地 §5.8 的 chip 歸屬裁決 ②自己的對抗式 review（align 每 frame 成本、串流時的快取失效、錨點配錯、寬字元／emoji、gutter 定位、glyph settings 邊界、新快取會不會長大） | Opus 5 high | ✅ 19:35。落地 §5.8 chip 歸屬裁決，並找到 2 條自己的：**T3R1-1 high**——`cell.point.line` 不是可見畫面列號而是相對螢幕頂端的格線編號，捲回歷史時是負數，所以 `display_offset > 0` 時最上面幾列的 chip 消失、其餘整排畫高，配到的 entry 也跟著錯（而「捲回去找舊東西」正是唯一會用到 gutter 的情境）；證據引 Zed 自己的 `viewport_line_for_point(point, display_offset)`。**T3R1-2 medium**——快取 miss 時把整份 transcript 的 tool 輸入 JSON 重新 parse（Edit 的 input 好幾 KB × 每 frame）。兩條都修。另列 3 個 specGap 交大腦 → §5.9。大腦親跑 gate：**553 + 195 全綠、盲測 46 綠、clippy 零 warning、rustfmt 乾淨**，盲測檔 SHA-256 未變 | `scratch/wpT3-review1-report.md` |
| T3-review-2 | 換家族（Opus → grok）。①落地 §5.9 三條裁決（SG1 空骨架不得互配、SG2 錨點改在 collapse 前算並記可到達 index、SG3 `ToolResult` 加 `tool_use_id`）②自己的 review（第 1 輪 `display_offset` 修法的邊界、`entries_generation` 有沒有漏 bump、pre-collapse 後 512 上限截到的東西變了、reveal index 在 rebuild 後失效、DP 表配置） | grok 4.7 xhigh | ✅ 19:52（第一次派工因 CLI 改掉模型字串格式而 exit 1 零產出、工作樹未動，改用 `--model grok-4.7-xhigh` 重派）。三條裁決全部落地，並自己找到 **T3R2-1**：錨點改從 pre-collapse 算之後數量變多，512 上限截到的東西變了，捲回歷史時尾端 512 窗涵蓋不到畫面 → 改成選「覆蓋最多可見列」的窗。另 T3R2-2 wontfix（只有圖片沒有文字的 tool_result 沒有 `ToolResult` entry 可放 id，後果是少畫不是畫錯，且修它會越過授權邊界）。14 個 RED 測試。大腦親跑 gate：**567 + 195 全綠、盲測 46 綠且 SHA-256 未變、clippy 零 warning、rustfmt 乾淨** | `scratch/wpT3-review2-report.md` |
| T4 | 文件更新（`docs/claude-sessions-architecture.md`、`docs/claude-sessions-setup.md`） | Fable | ✅ 20:1x。三個 .md 全改：setup（190 行）、architecture（237 行）、health-check-progress 只加結尾一段指路。禁用詞檢查乾淨。回報 6 處規格與 code 不一致（文件以 code 為準），見報告 | `scratch/wpT4-report.md` |
| T5 | 使用者回報四件事：①terminal 下方那條「待回答問題」橫幅是殘留（terminal 裡的 TUI 本來就畫得更好而且能選）→ 刪 ②rail 380px 寫死，圖片太小 → 可拖曳 ③圖片點不開 → 點擊放大覆蓋層 ④看不出 rail 卡片對應 terminal 哪一段 → 畫面上的錨點卡加左邊條、hover 兩邊互亮、捲回歷史時 rail 跟著捲 | grok 4.7 xhigh | ✅ 2026-09-23 00:39。只改 panel 一個檔。大腦親跑 gate：**claude_sessions 599 全綠（+28，attribute 數對帳過）、clippy 零 warning、rustfmt 乾淨**；盲測與 `terminal_anchors.rs` 逐位元組驗過未動；T5 diff 無 `unwrap()`／`let _ =`／`#[allow]`。快照 `scratchpad/before-t5.patch` | `scratchpad/wpT5-report.md` |

## 5. 大腦裁決紀錄

### 5.1 `terminal_anchors` 規格漏洞（盲測作者提出 Q1–Q7，2026-09-22 10:20 裁決；T3 實作照此做，盲測檔不改）

- **Q1 `MIN_SKELETON` 邊界**：「under 6」嚴格解讀——長度 `< 6` 才要求相等，長度恰為 6 走前綴規則。**任一側**骨架 `< 6` 就要求兩側骨架完全相等。
- **Q2 `align` 交叉對**：畫面 `[B, A]`、transcript `[A, B]` 時 LCS 長度 1，兩種解都合法；套用「偏新」：在所有最長對齊中，取 transcript 索引序列（從尾端比較）字典序最大的那個。此例結果為 `(row 0, B)`。實作方式：LCS 表從尾端回溯、平手時走「取 transcript 較新那格」的分支。
- **Q3 寬字元 spacer**：**呼叫端**（panel）負責在餵 `screen_rows` 之前略過 alacritty 標為 wide-char spacer 的 cell；`screen_rows` 收到的 column 可以不連續，缺的 column 一律補一個空白（Q5）。寬字元後多出的一個空白不影響骨架。
- **Q4 同 `(line, column)` 重複 cell**：後者覆蓋前者（last wins）。
- **Q5 column 洞**：每個缺的 column index 補一個 `' '`（columns 0 與 3 有字 → `"a  b"`）。
- **Q6 兩個 glyph 相同**：先判 `user_prompt`；相同時視為 `UserPrompt`。不做參數驗證。
- **Q7 空白定義**：glyph 前的「前導空白」與 glyph 後的「至少一個空白」都只認 U+0020；tab 或其他 Unicode 空白不算，該行就不是錨點。

### 5.2 T1 實作者偏離規格之處（大腦已接受，2026-09-22 11:00）

- 純函式定名 `wanted_terminal`，用 `pane_target` 而非 `attach_arguments` 判斷可否 attach：後者會 `fetch_add` 全域 `MIRROR_SEQUENCE`，每 frame 呼叫會鑄出用不到的 mirror 名。等價性由 `a_tmux_target_embeds_a_terminal_only_when_attach_arguments_accepts_it` 鎖住。
- Stop 按鈕原在 `render_input`，改放 `render_status_line`（仍只在非 subagent 的 in_pane 顯示）。
- `Cargo.toml` 除加 `project` 外拿掉 `editor`／`sha2`／`zed_actions`（無引用），並加 dev-dependency `workspace = { features = ["test-support"] }`——`./script/clippy` 帶 `--all-features` 會打開 remote 的 `test-support` 而露出 `RemoteConnectionOptions::Mock`，對應 match arm 在 project／workspace 自己的 `test-support` 後面。沒改那兩個 crate。
- `user_messages`、`user_message_entries` 改成 `#[cfg(test)]`。
- 規格 §2.2 寫錯一處：`MIRROR_SESSION_PREFIX` 與 `is_zed_mirror_session` 並未被 `0904ace174` 刪掉，現在還在。

### 5.4 T1R1-1 裁決（2026-09-22 11:38）

**缺陷成立，授權改那一個既有測試。** 大腦獨立驗證過前提：`session_store.rs::apply_registry_scan` 是用 `previous_by_pid`（同 `process_id`、換 `session_id`）判定 `/clear`，process 與 tmux pane 都沒換。所以內嵌 terminal 的身分是「pane ＋ process」，不是 conversation id。`TerminalTarget` 改成 `{ pane: String, process_id: u32 }`，`wanted_terminal` 用 `pane_target(tmux_target?)?` 取 pane。

只授權改 `a_main_pane_embeds_a_terminal_only_for_an_attachable_live_session`（它把缺陷寫死成斷言）。第 1 輪落地的兩個紅測試不准改斷言或輸入，只能靠改 code 變綠。

### 5.9 T3-review-1 的三個規格漏洞（2026-09-22 19:40 裁決，三條全修，交第 2 輪）

**SG1 空骨架互相 match → 修。** `skeleton` 只留 alphanumeric，所以 `⏺ ✅`、`⏺ ---`、`⏺ 🎉` 的骨架都是空字串，而 §5.1 Q1 的「任一側 < 6 就要求兩側相等」讓 `"" == ""` 成立——一個純符號的畫面行可以配上任何一筆純符號的 transcript 錨點，chip 落在錯的 entry 上。
**裁決：`skeletons_match` 在兩側骨架皆為空時回 `false`。** 沒有可辨識內容的行不該配上任何東西。
**大腦已查證這不會踩紅盲測**：盲測裡唯一碰空骨架的是 `skeleton_of_punctuation_and_space_only_is_empty`，它只斷言 `skeleton("  ***  --- ...  ") == ""`，完全沒有對 `rows_match` 的空骨架行為下任何斷言。reviewer 因為不准讀盲測而不敢動，判斷正確；由大腦查證後授權。

**SG2 收合的 turn 沒有 `ToolUse` entry，chip 只在當前那一輪出現 → 修。** `show_tool_calls` 預設 false，`collapse_one_turn` 把 `ToolUse`／`ToolResult`／`Thinking`／`TurnFooter` 換成一行 `TurnSummary`，所以 `transcript_anchors(&self.entries)` 對已結束的 turn 一個 `⏺` 錨點都產不出來。
**裁決：沿用 T2 已經建立的同一個模式——在 `collapse_turns` 之前算錨點。** T2 的 `turn_bills` 就是因為同樣的理由（收合會丟掉帶完整資料的那筆）而在收合前先算好存起來。錨點比照辦理：從 pre-collapse 的 entries 建 `TranscriptAnchor`，並記下每個錨點在收合後畫面上**可到達的** index（該 entry 自己還在就是它，被收掉了就是它那一輪的 `TurnSummary`）。點 chip 時 reveal 那個可到達的 index。

**SG3 `EntryKind::ToolResult` 沒有 `tool_use_id`，平行呼叫會配錯 → 修。** 一個 assistant record 同時發出多個 `tool_use` 時 entries 是 `ToolUse(a), ToolUse(b), ToolResult(a), ToolResult(b)`，位置配對會把兩筆結果都算到 b 頭上。後果是 chip 圖示錯（點下去期待圖片卻沒有），不影響導覽位置。
**裁決：在 `EntryKind::ToolResult` 加 `tool_use_id: Option<SharedString>`**，`build_entries` 已經從 `tool_result_ids(record)` 拿得到它。**大腦授權為了新欄位而修改既有測試的 struct literal，但只准加欄位，不准改任何斷言或輸入。**

### 5.8 gutter chip 掛在哪一行（2026-09-22 19:00）

T3 回報：圖片 chip 與「被截斷的輸出」chip 永遠畫不出來，因為 `align` 只會指到 `Message` 與 `ToolUse`，而 `Image`／`ToolResult` 依規格不是錨點。作者沒有自作主張放寬錨點集合，正確。

**裁決：不放寬錨點集合，改成把 chip 掛到它所屬的那一次工具呼叫。** `Image` 與 `ToolResult` 在 TUI 上沒有自己的錨點行（結果的行首 `⎿` 明文不是錨點，圖片夾在訊息裡），但它們都屬於某次工具呼叫，而 `⏺ Read(auth.rs)` 就是錨點。所以一行 `⏺` 的 chip 反映「這次呼叫產出了什麼」：

| 條件 | chip |
|---|---|
| `ToolUse` 有 diff（Edit） | diff |
| 結果含圖片 | 圖片 |
| 結果被截斷或有 persisted output | 展開 |
| 以上皆非 | 不畫 |

同時符合時優先序 **diff > 圖片 > 展開**。`Message` 錨點行維持原行為。

### 5.7 計費去重：一次呼叫取**最後**一筆 record 的 usage（2026-09-22 12:50，大腦親自量測）

Claude Code **一個 content block 寫一筆 record**：同一次 API 呼叫的 2–3 筆 record 有不同 uuid、同一個 `requestId`，每筆都帶一份 `message.usage`。所以去重的鍵是 **`requestId`**，不是 uuid。

大腦在使用者真實 transcript（`~/.claude/projects`，最近 12 個檔）上獨立量測，不採信單一來源：

| 量 | 值 |
|---|---|
| 計費 record | 721 |
| 實際 API 呼叫（distinct `requestId`） | 413 |
| 沒有 `requestId` 的 record | 0 |
| 一次呼叫跨多筆 record | 245 |
| 其中每份副本完全相同 | 233 |
| 不去重的膨脹倍率 | **1.68×** |

**副本不一致的 12 組，只有 `output_tokens` 不同，而且最後一筆才是完整值**（前面幾筆是串流途中的快照，`output` 只有個位數）。input 與 cache 三欄全程相同。

| 取法 | output tokens 合計 |
|---|---|
| 每組取第一筆 | 222 958 |
| 每組取最後一筆 | 261 755 |
| 每組取最大值 | 261 755（與最後一筆相同） |

**裁決：每個 `requestId` 取最後一筆 record 的 usage。** 取第一筆會少算 **14.8%** 的 output——而 output 是最貴的一類 token。T2 review 第 1 輪的 `turn_billing` 用 `seen_calls.insert` 取第一筆，是錯的，列為 T2R2-1 交第 2 輪修。

沒有 `requestId` 的舊 record 各自成一次呼叫（現行 `billing_of_path` 的退路正確，保留）。

### 5.6 「切到 subagent 會拆掉 terminal」裁決（2026-09-22 11:55）——**不是缺陷，維持現狀**

第 2 輪列為 specGap：切到 subagent 時 `wanted_terminal` 回 `None` → `Drop`，回到主對話要重 attach，捲動歷史不在了。

**裁決：維持 Drop，不要改成「留著但不畫」。** 理由是 WP3b 之前那版自己寫下的：*"an attached client that nobody is looking at still holds the pane's size down to its own."* mirror 與本尊同一個 session group、共用 window，所以一個看不見卻仍 attached 的 client 會把使用者真正在用的 tmux pane 壓到它的尺寸。丟掉捲動歷史，比縮掉使用者的 pane 便宜。`a_subagent_does_not_embed_a_terminal` 維持不動。

T2 因此只要處理「收合 rail ＋ 讀 subagent 畫成空白」（T1R1-3），不要嘗試讓 terminal 活著。

### 5.5 T2 要接的棒

- **T1R1-3**：rail 收合時讀 subagent，`render_terminal_and_rail` 回一個空 div——沒有 terminal 也沒有 transcript，中間整片空白。T2 重做 rail 收合時一併處理（subagent 時忽略 `rail_expanded`）。

### 5.3 盲測狀態

`crates/claude_sessions/src/blind_anchor_tests.rs`：46 個測試，rustfmt 過，**尚未註冊為 mod**（T3 落地時大腦在 `claude_sessions.rs` 加 `#[cfg(test)] mod blind_anchor_tests;`）。實作者與 fixer 不准讀、不准跑、不准改。報告 `scratch/blind-anchors-report.md`。

## 6. 收工狀態（2026-09-22）

全部四個工作包完成，每包都經過對抗式 review，兩個加了窄驗證。

| gate | 值 |
|---|---|
| `cargo test -p claude_sessions` | 571 綠 |
| `cargo test -p remote` | 195 綠 |
| 其中 held-out 盲測 | 46 綠 |
| `./script/clippy -p claude_sessions -p remote` | 零 warning |
| `cargo fmt --check`（兩個 crate） | 乾淨 |
| 盲測檔 SHA-256 | `3a9b342a…dcfdc4d8`，從寫成到收工未變 |

最終快照 `scratch/baseline/final-all.patch` + `final-new-files.tgz`。**未 commit**（使用者沒要求）。

### 6.1 盲測的價值

`blind_anchor_tests.rs` 由 Opus 只看 §3 凍結介面與 §5.1 裁決寫成，46 個測試。實作者（grok）與後續三輪 reviewer 全程不准讀、不准改；大腦每一輪都用 SHA-256 驗一次。T3 實作完成時 46 個**一次全過、中途沒紅過**，後續三輪修改也都維持 46 綠。

### 6.2 兩個以牆鐘時間斷言的測試（大腦裁決保留）

窄驗證加的 `the_anchor_pass_does_not_rescan_the_conversation_for_every_call`（預算 110 ms）與 `choosing_a_window_skeletonises_each_row_once`（預算 250 ms）用 `Instant::elapsed` 斷言。時間斷言原則上會 flaky，但這兩條要抓的是**複雜度回歸**（O(n²)），換成確定性斷言就得在正式路徑加計數器。

大腦實測後決定保留：連續跑 20 次全綠，其中 10 次在 8 路 CPU 滿載（機器 10 核）下跑。修之前的實測值是 590 ms 對 110 ms 預算、1316 ms 對 250 ms 預算，餘裕足以只在真的退化時才觸發。與 WP7d 的 DW-1 不同——那一條是實際觀察到 1/40 會紅才改成確定性的。

### 6.3 已知取捨（review 判 wontfix，大腦接受）

- 切到 subagent 會拆掉 terminal，回來要重 attach、捲動歷史不在了。看不見卻仍 attached 的 client 會把使用者真正的 tmux pane 壓小，代價更高（§5.6）。
- rail 模式下「N new below」數的是到站 entry 數，與畫面上實際畫出來的數量對不上。
- session 中途換模型後，回頭看舊輪會用新模型費率——與既有 `answer_cost` 一致，不是這次引進的。
- 只有圖片、沒有文字的 tool_result 沒有 `ToolResult` entry 可放 `tool_use_id`，那次呼叫不畫 chip（少畫，不是畫錯）。
- `SessionSource::list_slash_commands`、channel 的 `kind:"message"` 整條寫入路徑、貼檔 RPC 仍在但已無 UI 呼叫者（規格 §0 決定只拆 UI，channel server 保留）。

## 7. 建置與安裝（2026-09-22 21:01）

`LK_CUSTOM_WEBRTC=… CARGO_BUILD_JOBS=6 script/bundle-mac -i`，release profile，單一架構 aarch64。三段編譯（zed+cli 11m01s、remote_server 4m15s、cargo bundle 自己再跑一次 17m57s），全程沒有被 macOS 記憶體殺手打斷。

**安裝驗證**（不看 exit code——腳本失敗也會回 0）：

| 檢查 | 結果 |
|---|---|
| `/Applications/Zed Dev.app/Contents/MacOS/zed` mtime | Sep 22 21:01（本次建置） |
| 大小 | 366 481 200 bytes |
| `codesign -v` | 通過，ad-hoc、`dev.zed.Zed-Dev` |
| 本次改動的字串 | `This session has ended.`、`Attaching`、`Show everything`、`Show only what the terminal cannot`、`Approve in the terminal or on claude.ai (channel not loaded)` 全部找得到 |

`exit=1` 是已知且無害的 DMG 步驟失敗（`mv … dmg/Zed Dev.app: No such file or directory`），發生在 `Installed application bundle: /Applications/Zed Dev.app` **之後**——`-i` 已經把 app 搬走了，DMG 步驟才找不到來源。

### 7.1 已結案：`render_question` 其實會畫，靜態追查的結論是錯的

**2026-09-23 結案**：使用者在實機上看到了這條橫幅（「Waiting for your answer」＋選項＋「Answer in the terminal.」），所以下面整段「二進位檔裡找不到符號與字串 → 無法判定會不會畫出來」的推論是錯的。函式是活的，只是被 inline 掉，字串搜尋的方法本身不可靠。**不要再用「符號／字串不在二進位檔裡」推論一段 UI 不會畫出來。**

橫幅已於 T5 刪除（與 terminal 裡的 TUI 重複，而且要等 store 清 `pending_question` 才消失）。rail 的歷史問題卡 `render_question_card` 保留。以下保留原始追查紀錄，只當作方法論的反例。

#### 原始追查紀錄（結論已作廢）

`render_question`（唯讀問題卡，panel 5413 行，由 `impl Render` 7597 行呼叫）在 release 二進位檔裡**沒有符號、也沒有它的字串常數**：

- `nm | grep render_question` → 0，而同一支檔案的 `render_pending_permission`／`render_status_line` → 6。整個 crate 有 2372 個符號存活，符號表本身有效。
- 全檔位元組搜尋 `Answer in the terminal.` 與 `Waiting for your answer` → 兩者都找不到；但它們在原始碼裡是純 ASCII（已用十六進位核對過），而相鄰函式的字串都找得到。
- 已安裝的與 `target/` 剛建好的兩個二進位檔結果完全一致，所以**不是裝到舊版**。
- rlib 裡搜不到任何字串（連確定存在於最終檔的 `This session has ended` 也搜不到），所以 rlib 這條線索無效，不能用來區分「編譯期消失」或「連結期被砍」。

靜態檢查到此為止，無法判定該卡片是否真的會畫出來。下一步要在實機上開一個有 AskUserQuestion 待答的 session 直接看。**這不影響其他功能**：rail 另有 `render_question_card`（6422 行）畫問題卡，權限卡、Interrupt、terminal、gutter、費用卡的符號與字串都在。

追查時已排除的解釋：

- **不是「欄位永遠是 None 所以被優化掉」**——`live_state.rs:256` 有真正的 `self.pending_question = Some(PendingQuestion { … })` 寫入點，LLVM 無從證明它恆為 None。
- **不是被 `#[cfg(test)]` 擋掉**——該檔所有 `#[cfg(test)]` 屬性各自只 gate 一個函式，`mod tests` 起於 11165 行，遠在 `render_question`（5413）與 `impl Render`（7570）之後。
- **不是字串寫法問題**——位元組核對過是純 ASCII；子字串 `Answer in`、`aiting for your`、`nswer in the termina` 在二進位檔裡一個都找不到（`your answer` 只出現在無關的 prompt 範本裡）。

## 8. 修正：內嵌 terminal 只剩一行（2026-09-22）

**症狀**：panel 內嵌的 terminal 只畫得出 Claude Code 的輸入框那一行，其餘區域是 tmux 的 `·` 填充。

**量到的事實**（`tmux list-clients`）：

```
/dev/ttys008 session=zed-claude-mirror-74012-4 size=75x1   ← Zed 內嵌的 client
/dev/ttys007 session=zed                       size=144x52 ← 使用者真正的終端機
zed:0 75x1   zed-claude-mirror-74012-4:0 75x1             ← 共用的 window 被縮成 1 行
```

**成因**：`sync_terminal` 建好 `TerminalView` 之後呼叫了 `set_embedded_mode(None, cx)`。embedded 模式下 `content_mode()` 回 `ContentMode::Inline { displayed_lines: terminal.used_lines(), .. }`，而 `terminal_element.rs:1148-1159` 直接把元素高度定成 `displayed_lines * line_height`——高度跟著「已用行數」走，不是跟著可用空間走。剛 attach 的 pane 已用行數是 1，於是 pty 被設成 1 行；tmux 的 `window-size latest`（預設值）讓共用 window 跟著這個 client 縮成 1 行；window 只剩 1 行，`used_lines()` 就永遠是 1——自我鎖死。

順帶一提，被縮掉的是**共用的** window，所以使用者那個 144x52 的真終端機也一起被縮成 1 行。

**修法**：不呼叫 `set_embedded_mode`，維持 `TerminalMode::Standalone`（`content_mode()` 回 `Scrollable`，元素填滿被分配到的空間）。`set_show_workspace_actions(false, cx)` 與 mode 無關，照舊呼叫。

**留下的取捨**：mirror session 與原 session 共用同一個 window，tmux 的 `window-size latest` 表示「最後活動的 client 決定 window 尺寸」。panel 與使用者的真終端機寬高不同時，window 會在兩者之間跳動，Claude Code TUI 會跟著重畫。目前不處理。
