# Claude Sessions 面板：設定與使用

- 日期：2026-09-22
- 對象：在 Zed 裡看、控制正在跑的 Claude Code session 的人（本機或 remote host 都適用）
- 技術細節在 `docs/claude-sessions-architecture.md`；這次改版的規格與裁決在 `docs/claude-terminal-rail.md`

## 一句話

**打字在 terminal，看在面板。** 面板開成 editor tab 之後，左邊是你那個 Claude Code 的 tmux pane（面板自己 attach 上去的 terminal，可以直接在裡面打字），右邊一條 rail 只畫 terminal 顯示不了的東西：圖片、被截斷的工具輸出、Edit 的 diff、附件、每輪摘要、每輪一張費用卡。中間一條窄窄的 gutter，把 rail 裡的東西對齊到 terminal 畫面上那一行。頂端是 context／模型／費用／rate limit 的工具列，底端是狀態列和 Stop 按鈕。

面板本身只會做三件「寫」的事：Stop（中斷這一輪）、權限 Allow／Deny、裝／解除 hooks。其他一切輸入都在 terminal 做。

## 三步設定

### 1. 在面板按 Install hooks

打開 Claude Sessions 面板（dock），session 列表的標題列有 **Install hooks**（裝好之後這顆會變成 **Uninstall hooks**）。按下去會：

- 寫三個檔：
  - `~/.claude/hooks/zed-claude-events.sh` — hook 事件分派器，把每個事件 append 進 `~/.claude/zed-events/<session_id>.jsonl`
  - `~/.claude/hooks/zed-claude-status.sh` — statusLine 包裝，把 CLI 的狀態 JSON 寫進 `~/.claude/zed-status/<session_id>.json`
  - `~/.claude/zed-channel/server.mjs` — channel MCP server
- 改 `~/.claude/settings.json` 兩個地方：
  - `hooks`：13 個事件各加一條指向 `zed-claude-events.sh` 的 entry — `UserPromptSubmit`、`PreToolUse`、`PostToolUse`、`PermissionRequest`、`PermissionDenied`、`Notification`、`Stop`、`SubagentStop`、`PreCompact`、`PostCompact`、`PostModelSwitch`、`MessageDisplay`、`SessionEnd`。你自己的 hook 不會被動；舊版 Zed 裝的 `record-pending-question.sh`／`record-live-message.sh` 會被移掉。
  - `statusLine`：改成跑 `zed-claude-status.sh`。你原本的 statusLine 指令會存進 `~/.claude/zed-status/chained-command.txt`，包裝腳本每次都接著執行它、把輸出原樣印回去，狀態列長相不變。
- 改 settings 之前先複製一份到 `~/.claude/settings.json.zed-backup`（只留一份，真的有改才覆蓋）。settings.json 解析不了就什麼都不寫，直接報錯。

結果會顯示在 session 列表下方，三種之一：

- `Hooks installed (settings backed up to …); restart Claude to pick them up`
- `Hook scripts updated; restart Claude to pick them up`
- `Hooks are up to date`

remote 專案也是同一顆按鈕，只是檔案寫在遠端主機的家目錄。

### 2. 註冊 MCP server（每台機器一次）

```sh
claude mcp add --scope user zed-claude -- node "$HOME/.claude/zed-channel/server.mjs"
```

remote 主機就在那台主機上跑同一條，路徑換成該主機的家目錄。（以前面板有一顆「Copy setup commands」幫你填路徑，現在沒有了，自己打。）

### 3. 每個 session 啟動時帶旗標，然後重啟

```sh
export CLAUDE_EXTRA_ARGS='--dangerously-load-development-channels server:zed-claude'
```

- 放進 `~/.zshrc`：你的 `claude()` zsh function 會把 `CLAUDE_EXTRA_ARGS` 帶進每個 session。沒用那個 function 就直接在 `claude` 後面加那個旗標。
- 設好之後**把正在跑的 Claude session 重啟**：hooks 和 channel 都是啟動時載入的。
- 每次互動式啟動都會跳一次 `WARNING: Loading development channels` 確認框，選 `I am using this for local development` 繼續。沒有辦法預先同意。
- 不能改用 `--channels zed-claude`：那個旗標只吃 Anthropic 核可清單上的 channel，自己裝的 `server.mjs` 會被擋。
- 面板底端狀態列出現 `channel: connected` 就是接通了。**channel 只影響 Stop 與 Allow／Deny**；沒接通，面板照樣能看、terminal 照樣能打字。

## 怎麼打開、怎麼用

1. dock 裡的 Claude Sessions 面板列出這台主機上正在跑的 session（本機或 remote）。點一列選它。
2. 按列表旁邊的箭頭（tooltip「Open in Editor」）或跑 `claude_sessions::OpenInEditor`，會在 editor 區開一個 tab，標題是 `[tmux session 名] 對話名稱`。
3. tab 左邊的 terminal 就是那個 session 的 tmux pane。**直接在裡面打字**：回答 Claude 的問題、跑 `/context`、`/model`、`/compact`、`/clear` 這些 slash 指令、貼路徑、Ctrl+C，全部照 Claude Code CLI 原本的方式。面板不會替你送任何文字。
4. 右邊 rail 和中間 gutter 是拿來看的（點 chip 或卡片會捲動、展開），不是拿來輸入的。

**只能在 terminal 做的事**：打訊息、回答 AskUserQuestion、所有 slash 指令（含 `/context` 這類 CLI 本地指令）、貼圖或貼檔。

## 面板長什麼樣

### 工具列（最上面）

- 標題是 Claude 自己取的 session 名稱（transcript 的 `ai-title`），沒有就用 registry 的名字。
- context 進度條用 statusLine 的 `used_percentage`；60% 轉黃、85% 轉紅；compact 過會在 tooltip 註明；沒有 statusLine 只顯示數字。
- facts：模型、effort、累計費用、`15.0M tokens left`、5h／7d rate limit（tooltip 有重置時間）、`+x −y` 行數、還在跑的 agent 數。太窄收成「…」。模型費率不認識時多一個 `unpriced models`。
- 右側按鈕：
  - **收合／展開 rail**（側欄圖示，tooltip「Show/Hide details」）：收起來 terminal 就拿到整個寬度。
  - **rail 範圍**（i 圖示）：預設「只畫 terminal 顯示不了的」；按下去變「Show everything」，rail 就是完整對話（含純文字訊息、thinking、每則訊息底下的費用行）。
  - **Open in claude.ai**、**Expand all tool calls**、**Show full history**、**Show costs**（預設開）。

### terminal（左）

- 面板選到一個活著、而且 registry 裡有 tmux pane 的 session，就會自動 attach。attach 中顯示 `Attaching…`。
- 沒有 tmux pane 的 session（例如用 `claude --bg` 起的）顯示 `This session is not running in a tmux pane. Attach from the sessions list.`，這時只能在 dock 用 **Attach** 開一個一般 terminal。
- `/clear` 之後 session id 會換，但 terminal **不會**重新 attach，捲動歷史留著。
- 切到 subagent 的對話時 terminal 會拆掉（那不是 tmux pane 裡的東西），切回主對話會重新 attach，捲動歷史從頭來。這是刻意的，見架構文件。
- 面板拿到焦點時焦點落在 terminal 上，直接打字就行。

### gutter（中間那條）

- 28 px 寬。rail 裡有東西可看的那一輪，會在 terminal 畫面上對應那一行的旁邊畫一個小 chip：
  - diff 圖示：這次是 Edit，rail 有 diff
  - 圖片圖示：工具結果裡有圖片
  - 展開圖示：工具輸出在 terminal 被截成 `ctrl+o to expand`，rail 有完整的
  - 箭頭：一般訊息行
- 點 chip，rail 捲到那一筆並展開。
- 對齊靠 terminal 行首的 `>`（你的輸入）和 `⏺`（Claude 的每段文字／每個工具呼叫）跟 transcript 做順序保留的比對。捲回歷史時也會跟著算；對不上就不畫，不會畫錯位置。
- 你的 Claude Code 主題如果換了行首符號，在 Zed settings 加：

```json
"claude_sessions": {
  "user_prompt_glyph": "❯",
  "assistant_glyph": "●"
}
```

一個字元；空字串或多字元會退回預設。

### rail（右）

預設模式下只畫這些：

- **每輪一張費用卡**：一行 `$0.42 · 1.2K in · 85K cache read · 3.1K cache write (5m) · 900 out · (400 thinking)`（為 0 的欄位不顯示；thinking 括號裡是 out 的一部分，不另外算錢），點一下展開成這輪每一次 API 呼叫各一行。已經收合的輪掛在那一行 `▸ 12 tool calls · 3 files edited · 41s` 摘要底下；正在跑的輪掛在最後一筆有計費的 record 底下。模型費率不認識就不畫。
- **圖片**、**SendUserFile 送檔卡**（失敗標紅；4 MiB 以上改成 **Load anyway (N MB)**；mp4 或其他檔案的路徑是按鈕，本機用系統程式開、remote 先下載）、**附件**。
- **Edit 的 diff**。
- **被截斷的工具輸出**（超過 12 行、或被 Claude Code 存成檔案的那種），點開看全部。
- **每輪摘要**、compact 邊界、系統註記。
- 純文字訊息、thinking、短的工具輸出、沒有 diff 的工具呼叫：terminal 已經有了，rail 留一個零高度的位子，不重畫。

按工具列的 i 圖示切到「Show everything」就是以前那個完整對話畫面。

rail 底部還有：**now row**（正在跑哪個工具、跑了幾秒；來自 hook，點一下展開那張卡）、**Saying 框**（串流中的字，可拖高度；說完自動收）、以及「N new below」（你捲上去看舊東西時，下面又長出來的筆數）。

### 權限卡（rail 下面、狀態列上面）

- Claude Code 問權限時出現 `Permission requested: <工具> <目標>`，channel 有給的話多兩行 description 與 `input_preview`。
- channel 接通：有 **Allow**／**Deny**。按了之後顯示 `answered — waiting for the session`，等 Claude Code 說這個 prompt 結束才消失。terminal 的對話框同時也開著，誰先答誰算。
- channel 沒接通：`Approve in the terminal or on claude.ai (channel not loaded)`，沒有按鈕，去 terminal 按。

### 問題卡（AskUserQuestion）

只顯示，不能答：列出每個問題的 header、題目、選項，最後一行 `Answer in the terminal.`。到 terminal 用方向鍵選。

### 狀態列與 Stop（最下面）

- 一行 `● idle` 或 `● running 1:24` · `mode: auto`（tooltip 有 auto-mode 旗標） · `channel: connected` 或 `channel: not loaded — Setup`（後者代表你還沒做第 2、3 步）。
- 右邊 **Stop**：只在 channel 接通、server 支援 interrupt、而且這一輪正在跑時能按。按一下送一次 SIGINT（等同 terminal 按一次 Ctrl+C），按鈕變 `interrupt sent` 五秒或直到這一輪結束；期間再按無效。idle 時停用，tooltip 說原因。
- 讀 subagent 時沒有狀態列。

### dock 裡的列表

- **活著的 session**：名字、目錄、狀態；`claude --bg` 起的也會列（每 3 秒跑一次 `claude agents --json`），動作 **Attach**（開一般 terminal 跑 `claude attach <id>`）、**Respawn**、**Stop**（按第一下變 **Confirm stop**，5 秒內再按才真的 `claude stop`）。
- **ended（process 不見了）**：CLI 自動更新、`exit`、crash、`/clear` 之後舊對話留在列表，還能讀；動作 **Resume**（原 cwd 跑 `claude --bg --resume <id>`，registry 掃到就綁回來）、**Open in claude.ai**、**Open in tmux**（原本有 tmux target 才有；開一個你看得到的 terminal）、**Dismiss**。最多留 20 筆。
- **Install hooks／Uninstall hooks** 在列表標題列。
- **agent chips**：這個 session 派出去的 subagent，點一下讀它的對話（terminal 會拆掉，見上）。

## 已經拿掉的功能（舊文件教過，別找了）

2026-09-22 起，面板**不再有任何輸入框**。以下全部沒有了：

- 面板底部的訊息輸入框、Enter 送出、Shift+Enter 換行、`delivered to Claude` 那類送出狀態、待送佇列（PendingSends）、Retry。
- `/` slash 選單（含「哪些指令只能在 terminal」的分類）——現在所有指令都在 terminal 打，Claude Code 自己處理。
- `@` 檔案選單、貼圖（`~/.claude/zed-pasted/`）、Cmd+V 貼進訊息。
- Up／Down 叫出歷史訊息。
- 問題卡的可點選項、**Submit**、**Type something**——問題卡只剩顯示。
- Esc 關選單（`claude_sessions::DismissMenus`）。
- keymap 裡 `claude_sessions::SendMessage`／`DismissMenus`／`PreviousMessage`／`NextMessage`／`PasteIntoMessage` 五個 action 與它們在 macOS／Linux／Windows 的綁定。剩下的 action 只有 `ToggleFocus` 與 `OpenInEditor`。
- 「Copy setup commands」按鈕。

保留的：Stop、權限 Allow／Deny、Install／Uninstall hooks、dock 的 Attach／Respawn／Stop／Resume／Open in tmux／Dismiss。

## 疑難排解

**terminal 區顯示 `This session is not running in a tmux pane.`**
registry（`~/.claude/sessions/<pid>.json`）的 `tmux` 欄位沒有 `%pane` id。這個 session 不是在 tmux 裡起的（例如 `claude --bg`），面板沒有東西可以 attach。用 dock 的 **Attach** 開一般 terminal，或在 tmux 裡重開 session。

**terminal 區一直 `Attaching…` 或顯示 `Attaching to the session: …`**
tmux 命令沒成功。最常見：Zed 跟 tmux server 不是同一個 user、或 tmux 不在 PATH。錯誤文字會直接顯示在那一區。

**多出來的 `zed-claude-mirror-*` tmux session**
那是面板 attach 用的鏡像 session（和你的 session 同一個 group、共用 window），關掉 tab 就會自己銷毀。殘留的話 `tmux kill-session -t zed-claude-mirror-…` 無害。列表會把它們過濾掉。

**狀態列 `channel: not loaded — Setup`，權限卡沒有 Allow／Deny，Stop 按不下去**
第 2、3 步沒做完，或 session 沒重啟。確認 `claude mcp list` 有 `zed-claude`、啟動時有跳 `Loading development channels` 確認框、`~/.claude/zed-channel/<claude pid>/server.json` 存在且 5 秒更新一次。

**工具列沒有 context 進度條、沒有模型／費用、rail 沒有 now row**
hooks 或 statusLine 沒裝，或 session 沒重啟。dock 若還看得到 **Install hooks** 就是沒裝；裝了看 `~/.claude/zed-events/<session_id>.jsonl` 與 `~/.claude/zed-status/<session_id>.json` 有沒有在長。

**gutter 一個 chip 都沒有**
gutter 只在 terminal 有 attach 上、而且畫面上有 `>`／`⏺` 行首能對上 transcript 時畫。你的 Claude Code 主題如果換了符號，照上面設 `user_prompt_glyph`／`assistant_glyph`。純符號的行（`⏺ ✅`）刻意不對齊。

**rail「N new below」的數字比看到的多**
那個數字算的是 transcript 筆數，包括 terminal 已經顯示、rail 沒重畫的那些。切到「Show everything」數字就對得上。已知取捨。

## 解除安裝

面板 **Uninstall hooks** 會：

1. 從 `~/.claude/settings.json` 拿掉 13 個事件裡指向 `zed-claude-events.sh` 的 entry，`statusLine` 還原成串接前的指令（原本沒有就整個拿掉）。
2. 刪 `~/.claude/hooks/zed-claude-events.sh`、`zed-claude-status.sh`、`~/.claude/zed-channel/server.mjs`。

它**不會**碰 `~/.claude.json`，MCP 那條自己收：`claude mcp remove zed-claude`，再從 `~/.zshrc` 拿掉 `CLAUDE_EXTRA_ARGS`。資料目錄 `~/.claude/zed-events/`、`~/.claude/zed-status/`、`~/.claude/zed-channel/` 想清就清。最後重啟 Claude session。

面板進不去時的手動版：`cp ~/.claude/settings.json.zed-backup ~/.claude/settings.json`（那是 Zed 最後一次改 settings 之前的版本），再照第 2 點刪檔。
