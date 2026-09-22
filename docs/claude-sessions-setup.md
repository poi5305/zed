# Claude Sessions 面板：設定與使用

- 日期：2026-09-18
- 對象：在 Zed 裡用 Claude Sessions 面板看、控制正在跑的 Claude Code session 的人（本機或 remote host 都適用）
- 技術細節在 `docs/claude-sessions-architecture.md`；為什麼要這樣改在 `docs/claude-health-check.md`

## 這次改了什麼（一段話）

面板不再讀 tmux 畫面、也不再對 tmux 送按鍵。**讀**的部分靠三個檔：session 的 transcript、一支 hook 腳本把 Claude Code 每個 hook 事件 append 進 `~/.claude/zed-events/<session_id>.jsonl`、一支 statusLine 包裝腳本把 CLI 的狀態 JSON 寫進 `~/.claude/zed-status/<session_id>.json`。**寫**的部分靠一個 Zed 自己的 channel MCP server（`~/.claude/zed-channel/server.mjs`）：你在面板送出的訊息、Allow／Deny、Stop，都是寫成檔案丟進 `~/.claude/zed-channel/<claude pid>/outbox/`，由 server 轉成 Claude Code 的 channel 通知。terminal 本身完全不會被碰到，所以之前「Zed 一開就把 CLI 壓成 20 列」「Saying 卡住不消失」「auto mode 顯示不同步」這類問題的根因已經不存在了。

## 三步設定

### 1. 在面板按 Install hooks

打開 Claude Sessions 面板（dock），session 列表上方會有 **Install hooks** 按鈕（hooks 裝好之後按鈕就不會再出現）。按下去會做這些事：

- 寫三個檔：
  - `~/.claude/hooks/zed-claude-events.sh` — hook 事件分派器
  - `~/.claude/hooks/zed-claude-status.sh` — statusLine 包裝
  - `~/.claude/zed-channel/server.mjs` — channel server
- 改 `~/.claude/settings.json` 的兩個地方：
  - `hooks`：13 個事件各加一條指向 `zed-claude-events.sh` 的 entry — `UserPromptSubmit`、`PreToolUse`、`PostToolUse`、`PermissionRequest`、`PermissionDenied`、`Notification`、`Stop`、`SubagentStop`、`PreCompact`、`PostCompact`、`PostModelSwitch`、`MessageDisplay`、`SessionEnd`。你自己原本的 hook 不會被動；舊版 Zed 裝的 `record-pending-question.sh`／`record-live-message.sh`（兩種引號寫法都算）會被移掉。
  - `statusLine`：改成跑 `zed-claude-status.sh`。如果你原本就有自己的 statusLine 指令，它會被存進 `~/.claude/zed-status/chained-command.txt`，包裝腳本每次都會接著執行它、把它的輸出原樣印回去，所以你的狀態列長相不變。
- 改 settings 之前先把原檔複製一份到 `~/.claude/settings.json.zed-backup`（只留一份，每次真的有改才覆蓋）。
- settings.json 若解析不了，什麼都不寫，直接報錯。

按完會在輸入框上方看到結果之一：

- `Hooks installed (settings backed up to …); restart Claude to pick them up`
- `Hook scripts updated; restart Claude to pick them up`
- `Hooks are up to date`

remote 專案也一樣按這顆，只是檔案會寫在遠端主機的家目錄。

### 2. 註冊 MCP server（每台機器一次）

```sh
claude mcp add --scope user zed-claude -- node "$HOME/.claude/zed-channel/server.mjs"
```

面板在 channel 還沒接上時會顯示 `Zed channel not loaded — click Setup`，旁邊的 **Copy setup commands** 會把這條指令和下一步的 `export` 一起複製到剪貼簿，路徑已經替你填好（remote 專案填的是遠端的路徑）。

### 3. 每個 session 啟動時帶旗標，然後重啟

```sh
export CLAUDE_EXTRA_ARGS='--dangerously-load-development-channels server:zed-claude'
```

- 放進 `~/.zshrc` 就好：你的 `claude()` zsh function 會自動把 `CLAUDE_EXTRA_ARGS` 帶進每個 session。沒用那個 function 的話就直接在 `claude` 後面加那個旗標。
- 設好之後**把正在跑的 Claude session 重啟**：hooks 和 channel 都是啟動時載入的，舊的 process 看不到。
- **每次互動式啟動都會跳一次** `WARNING: Loading development channels` 確認框（選 `I am using this for local development` 就繼續，選 `Exit` 就結束）。沒有辦法預先同意：對照 claude 2.1.276 的 `DevChannelsDialog`，`onAccept` 只是註冊 channel 並往下走，不會寫任何設定，也沒有類似 `bypassPermissionsModeAccepted` 的持久化鍵。唯一不跳的情況是 channel 功能本身被關掉（非 first-party provider、或組織政策沒開 `channelsEnabled`），那時 channel 也不會真的能用。
- 不能改用 `--channels zed-claude`：那個旗標只吃 Anthropic 核可清單上的 channel server，我們自己裝的 `server.mjs` 會被擋，訊息是 `… is not on the approved channels allowlist (use --dangerously-load-development-channels for local dev)`。
- 面板下方狀態列出現 `channel: connected` 就代表接通了。

## 面板怎麼讀

### 標題與工具列

- 標題用 Claude 自己取的 session 名稱（transcript 裡的 `ai-title`），沒有就退回 registry 的名字。
- context 進度條：用 statusLine 給的 `used_percentage`；60% 以上轉黃、85% 以上轉紅；compact 過會在 tooltip 註明；沒有 statusLine 時只顯示數字不畫條。
- facts：模型、effort、費用、`15.0M tokens left`、5h／7d 的 rate limit（tooltip 有重置時間）、`+x −y` 行數、還在跑的 agent 數。面板太窄時收成「…」。
- 按鈕：**Open in claude.ai**、**Expand all tool calls**、**Show full history**、**Show costs**。

### now row（現在在跑什麼）

- 對話最底下釘一列：工具圖示、名稱、目標（檔名／指令／查詢字）、已跑秒數、脈動點。
- 來源是 hook 的 `PreToolUse`，所以在 turn 進行中就會動，不用等 transcript。
- 沒有工具在跑但 turn 還沒結束會顯示 thinking；idle 時整列消失。
- 點一下可以展開那個工具的完整卡片（結果已經有的話一起展開）。

### 每輪摘要

- 已經結束的 turn 預設摺成一行 `▸ 12 tool calls · 3 files edited · 2 agents · 41s`，tooltip 列出改過的檔名。
- 點開才看到工具卡、結果、thinking；再點一次收回。
- 正在進行的 turn 永遠展開，工具卡會邊跑邊長出來。
- **Expand all tool calls** 是「預設全部展開」的開關。

### Saying 框（串流中的字）

- 標題 `Saying…`；訊息說完但 transcript 還沒追上時變 `Said`。
- 用 markdown 畫；上緣可以拖曳調高度（3 rem 到面板 60% 之間），高度會記住。
- `Stop` hook 到、或 transcript 出現同一則訊息就自動收起；超過 30 秒沒更新且 session 已 idle 也會收。

### 權限卡

- Claude Code 問權限時卡片會列工具名、目標、`input_preview`；channel 接通時有 **Allow**／**Deny**。
- 按了之後顯示 `answered — waiting for the session`，等 hook 說這個 prompt 結束才消失。
- terminal 的對話框同時也開著，誰先答誰算。
- channel 沒接通時卡片寫 `Approve in the terminal or on claude.ai (channel not loaded)`，沒有按鈕。

### 問題卡（AskUserQuestion）

- 一次顯示這個 call 的所有問題和選項，不追游標。
- 單選直接點選項；多選勾完按 **Submit**；**Type something** 會把 `Answer to "<問題>": ` 填進輸入框讓你接著打。
- 全部都是**組成一則文字訊息送出去**（例如 `Answer to "Which DB?": Postgres`），因為 Claude Code 不會把問題 relay 給 channel；卡片下方有一行小字提醒這件事。

### 輸入框、slash 選單、狀態列

- Enter 送出、Shift+Enter 換行、Esc 只關選單（不再是中斷）。
- `/` 選單分三類：
  - 純文字（skills、`/compact`、`/clear`…）直接送。
  - 帶參數的（`/model`、`/effort`、`/config`、`/autocompact`、`/output-style`、`/advisor`）要有參數才送，沒有會提示 `add an argument, e.g. /model opus`。
  - 只能在 terminal 用的（`/permissions`、`/login`、`/resume`、`/agents`、`/plugin`、`/hooks`、`/memory`、`/theme`、`/export`、`/bug`、沒帶參數的 `/mcp`…）掛著 `terminal` 標記，Enter 不會送。
- 送出的訊息在進 transcript 前顯示 `delivered to Claude`；60 秒還沒被 session 拿走會變 `sent, waiting for the session to pick it up`。
- 狀態列一行：`● idle` 或 `● running 1:24` · `mode: auto`（tooltip 有 auto-mode 旗標） · `channel: connected` 或 `channel: not loaded — Setup`。
- 工具列偶爾會出現 `stale: transcript 12 s`，表示那個來源太久沒回；registry 掃描沒回來時也會亮，不會重複發送。

### Stop 按鈕（中斷）

- 只有在 channel 接通、server 支援 interrupt、而且 turn 正在跑的時候才能按。
- 按一下送一次 SIGINT（等同 terminal 的 Ctrl+C 一次），顯示 `interrupt sent` 直到 turn 結束或 5 秒過去；期間再按無效。
- idle 時停用，tooltip 會說原因（例如 `Interrupt: session is idle`），避免 idle 時的單次 Ctrl+C 變成「再按一次就退出」。

### ended row（process 不見了）

- CLI 自動更新、`exit`、crash 之後 transcript 不會被清掉，那一列變成 ended：內容照樣可讀可展開，但不能送。
- 動作：
  - **Resume in background** — 在原 cwd 跑 `claude --bg --resume <sessionId>`，registry 再掃到同一個 sessionId 就自動重新綁回這個 tab。
  - **Open in claude.ai** — 開 `https://claude.ai/code/<bridgeSessionId>`。
  - **Open in tmux** — session 原本有 tmux target 才會出現；這是開一個你看得到的 terminal 去 attach，不是隱藏 client。
  - **Dismiss** — 從列表移除。
- `/clear` 過的舊對話也會以 ended 形式留著。最多保留 20 筆。

### 背景 session

- 面板每 3 秒跑一次 `claude agents --json`，`claude --bg` 起的 session 也會列出來，並把 `waiting: permission prompt` 這類狀態貼到對應的列上。
- 動作：**Attach**（開 terminal 跑 `claude attach <id>`）、**Respawn**（`claude respawn <id>`）、**Stop**（按第一下變 **Confirm stop**，5 秒內再按一下才真的 `claude stop`，過了自動解除）。
- 找不到 `claude` 指令時只會顯示一次淡淡的提示，不會跳錯誤。

### shell 小弟卡

- Bash 工具若是用 `agent … --model …`、`agy … -p …`、`codex exec -m …` 派工，卡片會顯示模型、prompt 檔第一行、log 路徑。
- prompt 裡有寫 report 路徑的話會多一列 `Report · <檔名>` 和 **Load**，載入後以 markdown 顯示（超過 40 行摺疊）。

### 送檔卡（SendUserFile）

- 失敗的送檔會標紅並附結果文字。
- 圖片直接顯示，4 MiB 以上改成 **Load anyway (N MB)** 按鈕（上限 64 MiB）。
- mp4 或其他檔案的路徑列是按鈕：本機用系統預設程式開，remote 會先下載再開。
- 只有 session 工作目錄底下或 `/tmp` 的檔才會給你開。

## 已知限制

- **AskUserQuestion 只能用文字回答**：Claude Code 的 channel 不 relay 問題，面板組出來的答案是一般訊息，模型要自己讀懂。官方補 relay 之前這是永久做法。
- **Interrupt 就是一次 SIGINT**：只在 turn 進行中可按，server 端 3 秒內只送一次；連按不會變 Ctrl+C 兩下。
- **channel 是 research preview**：`--dangerously-load-development-channels` 的語法和確認提示都可能改。
- **同一 turn 內 5 秒內兩個同名工具的權限提示**只能靠先後順序配對（channel 的 `permission_request` 沒帶 `tool_use_id`）。不同工具名或相隔超過 5 秒的不會配錯。
- **remote host 必須用和 `claude` 相同的 POSIX user** 跑 Zed 的 remote server：channel 目錄是 0700。
- **遠端主機時鐘偏差**會讓 Saying 框的 30 秒守衛提早收起、費用顯示偏向較舊的 statusLine 值。
- `claude agents --json` 找不到 `claude` 時會顯示一次提示；背景 session 就列不出來。
- 兩個 Zed 視窗同時對同一個 session 送訊息，極端情況下仍可能撞名（機率極低，協定不允許獨占鎖）。

## 解除安裝

hooks 已裝好時，面板會在 Install hooks 旁邊多一顆 **Uninstall hooks**。按下去會：

1. 從 `~/.claude/settings.json` 拿掉 13 個事件裡指向 `zed-claude-events.sh` 的 entry，並把 `statusLine` 還原成 Zed 串接前的指令（原本沒有就整個拿掉）。
2. 刪掉 `~/.claude/hooks/zed-claude-events.sh`、`zed-claude-status.sh` 與 `~/.claude/zed-channel/server.mjs`。

它**不會**碰 `~/.claude.json`，所以 MCP 那條要自己收：`claude mcp remove zed-claude`，並從 `~/.zshrc` 拿掉 `CLAUDE_EXTRA_ARGS` 那行。資料目錄 `~/.claude/zed-events/`、`~/.claude/zed-status/`、`~/.claude/zed-channel/`、`~/.claude/zed-pasted/` 想清就清，留著也無害。最後重啟 Claude session。

面板進不去時的手動版：`cp ~/.claude/settings.json.zed-backup ~/.claude/settings.json` 還原 settings（那是 Zed 最後一次改 settings 之前的版本），再照上面第 2 步刪檔。
