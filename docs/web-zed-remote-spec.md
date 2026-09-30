# Web Zed — Remote (SSH) 支援規格

讓瀏覽器裡的 Zed 開啟 SSH 遠端專案：**browser → zed_web_server → ssh → 遠端主機上的 `zed-remote-server`**。
這份文件是**可以直接派工的 spec**：每個工作包（WP）都寫了要動的檔案、介面、訊息形狀、驗收測試，以及該派哪個模型。

- **Branch:** `andy/web-version`
- **日期:** 2026-09-29
- **前置文件（照它們的慣例寫）:** `docs/web-zed-plan.md`、`docs/web-zed-handoff.md`、`docs/phase4b-web-crates.md`、`docs/phase4c-zed-web-server.md`
- **狀態:** 只讀過原始碼，**一行都沒編譯、沒執行**。這份文件本身就是 Phase 7（remote）的計畫。

標記規則沿用 plan：**[verified]** 表示這段內容是這次研究真的讀過原始碼（附 `file:line`）才寫的；**[unverified]** 表示是推論。
「verified」只代表「原始碼這樣寫」，**不代表編譯或執行過**。沒有任何一條是跑出來的結果（見 §8.2）。

---

## 0. 怎麼用這份文件

| 如果你要… | 讀 |
| --- | --- |
| 決定做不做、做哪個方案 | §1、§3 |
| 理解現在兩條線路（web RPC、desktop remote）怎麼運作 | §2 |
| 實作其中一個 WP | §6 裡該 WP 的段落，加上它引用的 §4 小節；§5 的陷阱全部都要讀 |
| 寫測試 | §7 |
| 審查 | §5、§8，再對照 WP 的「驗收」一欄 |

### 0.1 派工通則（每個 WP 的 prompt 都要逐字附上）

依 repo owner 的規則（`~/.claude/CLAUDE.md`），每一隻 subagent／CLI agent 的 prompt 都要附上：

1. Claude subagent 的第一段逐字貼：
   > 第一步先跑 `ToolSearch("select:mcp__codegraph__codegraph_explore")`，再用 `codegraph_explore` 查符號定義／回傳形狀／呼叫者／blast radius／測試覆蓋，一次問完。配額用完改走 shell `codegraph explore "<符號>"`。grep 只准用於字串常數、檔案列舉、build 產物。

   agy／agent（grok、gemini）逐字貼：
   > 先用 shell `codegraph explore "<符號或問題>"` 查符號定義／回傳形狀／呼叫者／blast radius／測試覆蓋，一次問完。grep 只准用於字串常數、檔案列舉、build 產物。

   （本 repo 目前**沒有** `.codegraph/` 索引 [verified: `ls -d .codegraph` 找不到]。要不要 `codegraph init` 由大腦決定，WP agent 不要自己建。）
2. **禁止** `git stash` / `git checkout` / `git restore` / `git reset` / `git commit`。
3. > 發現規格自相矛盾、或你正在挑輸入／加資料讓某個測試過，停下來回報，不要換輸入。回報「做不到」是成功的結果。
4. repo `CLAUDE.md` 的 HARD RULE：修改任何 source 檔之前，如果 `README.md` 開頭還沒有 `> [!IMPORTANT]` 和 `> Remove this line to confirm you've reviewed this PR before submitting.` 這兩行，要先加上去；**永遠不要刪掉**。
5. 不准用 `unwrap()`；不准用 `let _ =` 吞掉會失敗的操作的錯誤；不准建立 `mod.rs`；只寫說明「為什麼」的註解。
6. 新的 `Instant` 一律用 `web_time::Instant`；任何 `SystemTime::now()` 在 wasm 圖裡都算缺陷（見 §5）。
7. wasm stub 要「誠實地失敗」：`panic!` 並點名替代方案，或回 `Err(ErrorKind::Unsupported)`，**不准假裝成功**（handoff §3「Every wasm stub fails honestly」）。

---

## 1. 目標與非目標

### 1.1 目標

1. 在瀏覽器分頁開啟 `ssh://user@host:port/abs/path` 專案，行為盡量等同 desktop 的 SSH remote project。
2. 以下面板在遠端專案上都要能用：

| 面板／功能 | 在遠端專案上走的路徑 | 需要新增的東西 |
| --- | --- | --- |
| project panel、worktree | proto：`WorktreeStore::remote`（project.rs:1464-1471） | 無，WP8 接好 transport 就會通 |
| editor、buffer、LSP、diagnostics | proto：`BufferStore`／`LspStore`（project.rs:1709-1711） | 無 |
| terminal | desktop：`create_remote_shell` → `RemoteClient::build_command` 產生 `ssh … -t host …`，在「client 機器」上跑（terminals.rs:623-661）。web：「client 機器」= zed_web_server，交給既有的 `RemotePty` → `Terminal::open` | WP4 的 command recipe、WP8 的 `build_command` |
| git | proto：`GitStore::init(&remote_proto)`（project.rs:1717） | 無（askpass 走 proto） |
| project search | proto：`handle_find_search_candidates_chunk`（project.rs:1706） | 無 |
| tasks | `TaskStore::init(Some(&remote_proto))`（project.rs:1713）；task 在 terminal 裡跑，跟 terminal 同一條路 | 同 terminal |
| `claude_sessions` | desktop 在有 `remote_client()` 時用 `RemoteSource`；**web 目前寫死用 `WebSource`**（claude_sessions_panel.rs:2785-2799） | WP11 |
| `tmux_sessions` | 已經看 `project.remote_client()`（tmux_sessions_panel.rs:91-94） | WP11 要驗證 |
| `project_manager` | 目前 wasm 直接拒絕 `ssh://`（project_manager_panel.rs:217-238） | WP10 改成「開新分頁」 |

3. 第一階段只做 SSH，而且只支援 **Linux／macOS 的 zed_web_server 主機**。

### 1.2 非目標（明確排除）

| 項目 | 理由 |
| --- | --- |
| WSL、docker／dev container | `RemoteConnectionOptions::Wsl/Docker`（remote_client.rs:1338-1344）在 wasm 上回誠實的錯誤。以後可以用同一條 relay 做，但不在這次範圍內 |
| Windows 上的 zed_web_server | `SshSocket` 的 ControlMaster 行為由 `#[cfg(not(windows))]` 決定（ssh.rs:169-174、1432-1435、1446-1456）；在 wasm 上，這個 cfg 回答的是瀏覽器的 target，不是伺服器的 OS（§5.1） |
| Windows 遠端主機 | 協定層支援（`build_command_windows`，ssh.rs:1969），但沒有測試資源。WP4 的 recipe 會帶 `is_windows`，**不另外驗收** |
| `forward_ports` | plan §6.5 已經重新定義它在 web 上的語意；這次 `build_forward_ports_command` 回 `Err(Unsupported)` |
| 在瀏覽器裡保存 SSH 密碼 | web 目前沒有任何憑證儲存機制；密碼只在 askpass 提示時經由 WS 傳一次（§4.11） |
| 遠端的 extensions／web_agent_panel | `web_extensions`、`web_agent_panel` 用的是 zed_web_server 自己的 RPC（main.rs:1308、1584），不在這次範圍 |
| 同一個分頁同時開多台主機 | 一個分頁 = 一個 `Project` = 一個 host，跟 desktop 一個 window 一個 host 一致 |

---

## 2. 現況盤點

### 2.1 web build 怎麼跟 zed_web_server 溝通 [verified]

**一條 WebSocket、純文字 JSON、自訂的 RPC**，不是 Zed 的 proto。

| 項目 | 事實 | 出處 |
| --- | --- | --- |
| 端點 | `GET /rpc` 升級成 WS，掛在需要 cookie 驗證的 `protected` router | lib.rs:147-156 |
| 驗證 | `ZED_WEB_TOKEN` → HMAC 簽章的 cookie；WS 升級時還會檢查 `auth::same_origin` | lib.rs:376-390、auth.rs:169-190 |
| 訊息大小上限 | `max_message_size` / `max_frame_size` 都是 16 MiB | lib.rs:385-386 |
| 請求格式 | `{"id":u64,"method":"Ns::name","params":{…},"session_id":"workspace:<id>"}`；`session_id` 由 JS 的 `identify()` 補上 | wasm_rpc lib.rs:232-244、55-59 |
| 回應 | `{"id":…,"result":…,"error":null\|string}` | rpc.rs:419-424 |
| 通知 | `{"method":…,"params":…}`，沒有 `id` | wasm_rpc lib.rs:252-257、364-385 |
| 握手 | 伺服器第一則訊息是 `Server::hello {instance_id}`；如果 instance id 變了（伺服器重啟過），JS 會 reload 整頁 | rpc.rs:264-275、wasm_rpc lib.rs:83-121 |
| 心跳 | 伺服器每 20 秒送一次 WS `Ping` | rpc.rs:285-294 |
| 重連 | JS 端指數退避：`min(10000, 250·2^min(n,6))` ms，±20% jitter；斷線期間的送出會排隊，上限 10000 則；斷線那一刻所有 pending 的 `call` 都會拿到 `Err("WebSocket disconnected …; reconnecting")` | wasm_rpc lib.rs:61-71、183-194、407-418 |
| 二進位 | **不支援**。瀏覽器收到 `ArrayBuffer` 會轉成 UTF-8 字串（lib.rs:334-343）；伺服器收到非 `Text` 的訊息直接 `continue` 跳過（rpc.rs:310-312） | |
| Session | `SessionRegistry` 以 `session_id` 當 key（rpc.rs:33-52）。`workspace_id` URL 參數只有 JS 在讀（wasm_rpc lib.rs:34），**整個 repo 找不到任何寫入它的地方**，所以實際上所有分頁都共用 `workspace:default` 這個 session [verified: grep] |
| 通知廣播 | `NotificationTarget::forward` 會把通知送給同一個 session 裡**所有**連線（rpc.rs:82-92） | |
| 斷線清理 | 連線關閉 → 解除 generation 綁定 → 30 秒後回收 watch 和孤兒程序 | rpc.rs:648-686 |
| Namespace | `Fs::` `GitRepository::` `Process::`（`output/status` 是無狀態的；`spawn/write_stdin/close_stdin/kill/attach/running_sessions` 是串流的）`Terminal::{open,write,resize,bind,attach,close}` `ClaudeSessions::` `Home::dirs` `ShellEnv::capture` `Sql::` `Extensions::` `Agent::` `Workspace::{ui_state,activate,set_sidebar_open,set_project_groups}` `Highlight::document` `Browser::relay_localhost_callback` | rpc.rs:714-782、process_rpc.rs:34-48、terminal_rpc.rs:66-77 |
| 路徑限制 | `ZED_WEB_RESTRICT_PATHS`／`--restrict-paths`／`.zed/web.json` 裡的 `restrict_paths`，預設 false | lib.rs:271-305 |
| 虛擬根目錄 | `/workspace` 開頭的路徑會被改寫成伺服器的根目錄（`FsRpc::path`、`rewrite_legacy_workspace_path`、`canonical_workspace_location`） | fs_rpc.rs:78-133、lib.rs:672-702 |

wasm 端把同一個 `RpcClient` 透過 `OnceLock` hook 裝進各個 crate：`wasm_remote::set_remote_client`、`smol::set_remote_client`、`terminal::set_remote_client`、`claude_sessions::set_remote_client`、`util::shell_env::set_remote_client`（main.rs:1298-1308；terminal.rs:105-117 是這個 pattern 的範本）。
它建的是 **`Project::local`**，只是 `Fs` 換成 `RemoteFs`（main.rs:1321-1328 → `workspace::open_paths`，main.rs:2410-2411；plan §6.1）。

### 2.2 desktop Zed 怎麼跟 remote_server 溝通 [verified]

```
Zed GUI (client)                                   remote host
┌───────────────────────────┐   ssh ControlMaster  ┌────────────────────────────────────┐
│ Project::remote            │  (-N, askpass)       │ zed-remote-server run  (daemon)    │
│  └ AnyProtoClient          │═════════════════════▶│  HeadlessProject                   │
│     └ ChannelClient ◀─mpsc─┤  ssh … env <bin>     │  ▲ unix sockets stdin/stdout/stderr│
│ RemoteClient (state mach.) │  proxy --identifier X│  │                                 │
│  └ Arc<dyn RemoteConnection│─────────stdio───────▶│ zed-remote-server proxy ──────────┘│
│      = SshRemoteConnection │ [u32 LE len][Envelope]                                   │
└───────────────────────────┘                      └────────────────────────────────────┘
```

| 層 | 事實 | 出處 |
| --- | --- | --- |
| 框架（framing） | 每則訊息是 `[u32 little-endian 長度][prost 編碼的 proto::Envelope]`，**不壓縮** | protocol.rs:8-9、25-50 |
| 壓縮 | zstd 只用在 collab 的 `message_stream`（message_stream.rs:54、93），而且 wasm 上根本不編 zstd（rpc/Cargo.toml:39-40）。stdio 這條路沒有壓縮 | |
| Envelope | `id=1, responding_to=2, original_sender_id=3, ack_id=266, oneof payload`；`ack=5`、`error=6`、`ping=7`、`flush_buffered_messages=267`、`remote_started=381` | zed.proto:24-34、262、414 |
| 版本檢查 | 協定本身**沒有任何版本握手**：`RemoteStarted {}`、`Ping {}` 都是空訊息（zed.proto:613、655）。相容性完全靠「兩邊是同一版 build」 | |
| fork 自己加的 proto | `claude_sessions.proto`、`tmux.proto`、`port_forward.proto`；遠端主機上的 handler 在 headless_project.rs:299-327 | → 遠端必須跑**這個 fork** 編出來的 `remote_server` |
| ChannelClient | 從 mpsc channel 建立；一開始就送 `RemoteStarted`，收到對方的 `RemoteStarted` 才算 ready；維護 ack buffer，重連時用 `FlushBufferedMessages` 補送 | remote_client.rs:2148-2240、2366-2416 |
| AnyProtoClient | `RemoteClient::proto_client_from_channels(incoming_rx, outgoing_tx, cx, name, has_wsl_interop)` 是 pub（remote_client.rs:539-547），**但** `Project::remote` 要的是 `Entity<RemoteClient>`（project.rs:1440-1449），所以瀏覽器端必須提供一個 `RemoteConnection` 實作 | |
| RemoteClient 狀態機 | `Connecting → Connected ⇄ HeartbeatMissed → Reconnecting → ReconnectFailed/ReconnectExhausted/ServerNotRunning`；`MAX_RECONNECT_ATTEMPTS=3`；心跳間隔 5 秒、逾時 5 秒、最多漏 5 次；初始連線逾時在 release 是 60 秒、debug 是 5 秒 | remote_client.rs:161-196、411-537、588-778、780-877 |
| 重連流程 | `remote_connection.kill()` → `ConnectionPool::connect(options)`（又回到同一個 pool）→ `start_proxy(identifier, reconnect=true)` → `client.reconnect` + `resync` | remote_client.rs:668-716 |
| monitor | io task 結束時：exit code 90（`ProxyLaunchError::ServerNotRunning`，proxy.rs:9-11）→ 進 `ServerNotRunning` 狀態；其他 exit code 或錯誤 → 觸發 reconnect | remote_client.rs:879-918 |
| ConnectionPool | gpui global，以 `RemoteConnectionOptions` 當 key（它有 derive `Hash/Eq`，remote_client.rs:1338）；只存 `Weak` 參照；依 `Ssh/Wsl/Docker/Mock` 分派到各自的 transport | remote_client.rs:1225-1335（Ssh 那一支在 :1279-1283） |
| RemoteConnection trait | `start_proxy`、`upload_directory`、`kill`、`has_been_killed`、`build_command`、`build_forward_ports_command`、`connection_options`、`path_style`、`remote_platform`、`remote_os_version`、`shell`、`default_system_shell`、`has_wsl_interop`；另有 `shares_network_interface` 有預設值。`#[async_trait(?Send)]`，trait 本身要求 `Send + Sync` | remote_client.rs:2043-2092 |
| SSH 建立 | 先找使用者現有的 ControlMaster（`ssh -G` + `-O check`）→ 找不到就建 `AskPassSession` → `MasterProcess`（`ssh -N -o ControlPersist=no -o ControlMaster=yes -o ControlPath=<tmp>/ssh.sock`，帶 `SSH_ASKPASS_REQUIRE=force`）→ 偵測 Windows／shell／`uname -sm`／OS 版本 → `ensure_server_binary` | ssh.rs:558-632、635-832、181-225 |
| askpass | 產生 `askpass.sh`，內容是 `printf '%s\0' "$@" \| <current_exe> --askpass=<sock>`；unix socket 由 `PasswordProxy` 監聽，把 prompt 交給 `RemoteClientDelegate::ask_password`。host-key 的 yes/no 也走這裡（因為 `SSH_ASKPASS_REQUIRE=force`） | askpass.rs:252-330、480-500、ssh.rs:204-205 |
| proxy 啟動 | `ssh … env [RUST_LOG=…] <bin> proxy --identifier <id> [--reconnect]`，`kill_on_drop(true)`，然後用 `handle_rpc_messages_over_child_process_stdio` 橋接 stdio | ssh.rs:466-539、transport.rs:128-240 |
| 遠端 daemon | `proxy` 用 pid file 找 `run` daemon；非 reconnect 時如果同 identifier 的 server 已在跑，**會先殺掉它**；daemon 閒置 10 分鐘自動退出 | server.rs:856-915、409-450 |
| identifier | `ConnectionIdentifier::Workspace(id)` → `"{channel}-workspace-{id}"`（stable 沒有前綴）；它會出現在 socket 路徑裡，所以必須很短 | remote_client.rs:350-379 |
| binary 名稱 | `~/.zed_server/zed-remote-server-{channel}-{version}`；dev channel 的 version 字串是 `"build"` | ssh.rs:840-856、paths.rs:69-73 |
| binary 是否存在 | 只看 `<bin> version` 能不能跑成功，**不比對內容** | ssh.rs:858-866 |
| dev channel | 從原始碼 build／`ZED_COPY_REMOTE_SERVER` 只在 `cfg(any(debug_assertions, feature = "build-remote-server-binary"))` 時存在（ssh.rs:869；transport.rs:242-261）；否則 Dev 會 `bail!("ZED_BUILD_REMOTE_SERVER is not set and no remote server exists …")`（ssh.rs:897-903） | |
| 這個 fork 的 channel | `crates/zed/RELEASE_CHANNEL` = `dev`；wasm 呼叫 `release_channel::init(Version::new(0,0,0))`（main.rs:1390） | |
| `version` 子命令輸出 | Dev/Nightly 印 `ZED_COMMIT_SHA`（編譯期環境變數），有 `ZED_BUILD_ID` 的話印 `{build_id}+{sha}` | server.rs:107-124 |
| Delegate | `RemoteClientDelegate { ask_password, get_download_url, download_server_binary_locally, set_status }`；desktop 的實作用 `RemoteConnectionModal` 顯示提示、用 `AutoUpdater` 下載 | remote_client.rs:136-159、remote_connection.rs:462-553 |
| 開專案流程 | `recent_projects::open_remote_project` → modal + delegate → `remote::connect` → `workspace::open_remote_project_with_new_connection` → `deserialize_remote_project`（**會寫 DB**）→ `RemoteClient::new(ConnectionIdentifier::Workspace(id)…)` → `Project::remote` | remote_connections.rs:147-420、workspace.rs:11384-11441、11587-11610 |

### 2.3 web 目前怎麼拒絕 remote [verified]

- `project_manager_panel.rs:217-238`：wasm 上遇到 `ProjectLocation::Remote` 就 `report_error("ssh://, wsl:// and docker:// projects cannot be opened from the browser …")`（commit `cf8f1b55ba`）。
- `recent_projects` 在 wasm 上把 `remote_servers`（SSH 管理 UI）、`ssh_config`、`dev_container_suggest` 都 gate 掉了（recent_projects.rs:1-8、40-58），但 **`remote_connections`（`open_remote_project`）和 `disconnected_overlay` 有編進去**（recent_projects.rs:3-4、24-25）。
- plan §6.1：web 刻意建 local Project；§6.6：否決了「把 zed_web_server 變成 `HeadlessProject`」。

### 2.4 已經在 wasm 圖裡、可以直接用的東西 [verified from source, not compiled]

- `crates/remote` 會編進 wasm（plan §3.2 的 `[patch]` 量測範圍包含 `remote`），而且它的 `remote_client.rs`／`transport*.rs` **沒有任何 wasm cfg**（grep 結果：這幾個檔案零筆）。這代表 `SshRemoteConnection::new` 在瀏覽器裡**編得過、也跑得起來**：它會透過 `util::command` → `smol_wasm` 的 `Process::spawn` 請 zed_web_server 去執行 `ssh`。這是一條**意外存在、但行不通的路**（§3 的 A′），必須明文禁止。
- `RemoteConnectionModal`、`remote_connection::RemoteClientDelegate`、`DisconnectedOverlay`、`workspace::open_remote_project_with_new_connection`、`project_manager::{parse_project_location, remote_project_uri}`（project_manager.rs:15-17、project_location.rs:47-132）。

---

## 3. 候選架構

共同的限制：瀏覽器不能開 TCP socket，也不能跑 `ssh`；**一定要有一個原生行程來持有 SSH 連線**。

### A. zed_web_server 當 transport；瀏覽器是完整的 remote client（**推薦**）

```
Browser (wasm)                          zed_web_server (native)                     remote host
┌──────────────────────────────┐  /rpc  ┌──────────────────────────────────┐ ssh  ┌─────────────────────────┐
│ Project::remote               │ JSON  │ RemoteSsh::* (tokio)              │ CM   │ zed-remote-server run   │
│  └ RemoteClient (unchanged)   │──────▶│   │ HostCommand (mpsc)            │═════▶│  HeadlessProject        │
│     └ WebRelayConnection      │       │   ▼                               │      │  LSP / git / fs / tmux  │
│        (impl RemoteConnection)│       │ gpui headless App (main thread)   │      │  claude_sessions        │
│  ConnectionPool(wasm): Ssh →  │       │   remote::connect → SshRemoteConn │      └──────────▲──────────────┘
│    WebRelayConnection         │ /remote/channel (binary WS, per proxy)  │ ssh … proxy      │
│  [u32 len][Envelope] bytes    │══════▶│ relay: bytes ⇄ Envelope ⇄ start_proxy ═════════════┘
└──────────────────────────────┘       └──────────────────────────────────┘
       Terminal::open("ssh", [-o ControlPath=<server tmp>, -t, host, …])  → RemotePty（既有）
```

- 瀏覽器端：`RemoteClient`、`ChannelClient`、`Project::remote` 這三層**完全不改**；只新增一個 transport。心跳、ack buffer、resync、`ServerNotRunning` 全部 end-to-end 由既有程式碼處理。
- 伺服器端：用 **native 的 `remote::connect`**，重用 ssh.rs 的全部邏輯（ControlMaster 重用、askpass、平台偵測、binary 部署）；relay 只負責搬 envelope。
- 優點：跟 desktop 共用同一份程式碼，「同一件事只做一次」；所有面板的 proto 路徑都已經存在（desktop 有測試）；遠端主機上什麼都不用裝（只需要 binary 部署）；一個 zed_web_server 可以連很多台主機。
- 缺點：zed_web_server 要多帶一個 gpui headless App（§4.2）；`Project::remote` **從來沒有在 wasm 上執行過**；要自己部署這個 fork 的 remote_server binary。
- 工作量：約 12 個 WP，其中 6 個是困難格（Opus 5.5 high）。風險：中高，主要在 wasm 執行期（§5）。

#### A′. 讓 `SshRemoteConnection` 直接在瀏覽器裡跑（**禁止**）

它編得過（§2.4），但跑起來一定會失敗：
- `AskPassSession::new` 要 bind `UnixListener`（askpass.rs:5、312），wasm 上的 `smol_wasm::net` 不支援；
- `tempfile::Builder::new().tempdir()` 產生的是瀏覽器視角的路徑，但 `ControlPath` 必須是伺服器上的路徑（ssh.rs:644-646、676）；
- `std::process::id()` 在 wasm 上會 panic（ssh.rs:881、912；claude_sessions.rs:3550 的註解有記錄這件事）；
- `find_existing_control_master` 讀的是 `ssh -G` 的結果，而那是伺服器上的狀態；
- proto stream 會經過 `Process::stdout` 的 base64 JSON 通知，雙倍編碼。

→ WP8 要在 wasm 的 `ConnectionPool` 把 `Ssh` 這一支改道到 `WebRelayConnection`，並讓 `SshRemoteConnection::new` **不可能**在 wasm 上被呼叫（§4.5.3）。

### B. zed_web_server 在內部跑一個 native 的 remote client `Project`，對瀏覽器照樣暴露 Fs/Process/Git…（**否決**）

```
Browser (local Project, RemoteFs)  ──/rpc JSON──▶  zed_web_server: Fs::* handler ─▶ native Project::remote ─▶ ssh ─▶ remote_server
```

- plan §6.6 已經否決過「把 zed_web_server 變成 `HeadlessProject`」；B 其實更重：伺服器要同時扮演 client 端 Project，再把 proto 語意翻譯回 web RPC 的 trait 語意。
- remote_server **沒有通用的 Fs proto**：讀檔是 `OpenBufferByPath`（buffer 語意），目錄是 `ListRemoteDirectory`、`GetPathMetadata`（remote_editing 用）；grep 找不到任意寫檔／rename／watch 的訊息 [verified: grep `crates/proto/proto/*.proto`；是否完整 unverified]。`RemoteFs` 需要的 `Fs::watch`、`Fs::rename`、`Fs::save` 都得重寫。
- `GitRepository::*` 在 web 是直接在本機跑 git CLI（git_rpc.rs:16-30），對遠端就得變成 `ssh git …` 或 proto，等於重做 git_rpc。
- LSP 會跑在 zed_web_server 上，而不是遠端主機上 → toolchain 用錯機器。
- 結論：工作量最大、語意翻譯層最容易出錯，而且跟 desktop 分岔。**否決。**

### C. 把 zed_web_server 部署到遠端主機上，另開一個分頁（**便宜的 fallback，當 WP0**）

```
Browser ──https──▶ tailscale / ssh -L 18090:127.0.0.1:8090 ──▶ remote host: zed-web-server <proj> <static>
```

- 在遠端主機上跑一份 `web/dist`（`bin/zed-web-server` + `static/`，約 84 MB，handoff §1），用 `ssh -L` 或 Tailscale Serve 暴露，瀏覽器開另一個 origin。
- 優點：**今天就能用**，所有面板（包括 web 專屬的 agent panel、extensions）都跟 local 一模一樣；不用改任何 code。
- 缺點：
  - 每台主機要部署 dist（84 MB + 升級流程），而且要有對應架構的 build（目前 build.sh 只 build 本機架構，build.sh:280-291）。
  - 每台主機各有一個 token、一個 cookie origin，要各自登入；一個分頁不能切換主機；`project_manager` 的 `ssh://` 項目沒辦法直接點開（最多只能產生一個連結）。
  - 遠端主機要能 bind port，或能 `ssh -L`；Tailscale Serve 要在遠端主機上設定。
  - 使用者的 settings／keymap 各主機各自一份（`initialize_config` 會寫進 `<root>/.config/zed`，lib.rs:226-236）。
- 工作量：一個 script + 文件。風險：低。

### D. 伺服器用 sshfs 掛載 + ssh exec 包裝，瀏覽器以為是 local（**否決**）

- `FsRpc` 指向 sshfs 掛載點；`Process::spawn` 包成 `ssh host cmd`。
- 否決理由：git 和搜尋透過 FUSE 會非常慢；LSP、toolchain、`claude_sessions` 的 liveness（`/proc/<pid>` 必須在主機上讀，plan §10.2）都跑在錯的機器上；`notify` watch 在 sshfs 上不可靠 [unverified]；而且要在伺服器上裝 FUSE。

### E. 瀏覽器透過遠端主機上的 WebSocket bridge 直接連 remote_server（**以後再說**）

- 在遠端主機上放一個小 bridge（或新的 `zed-remote-server ws` 子命令），瀏覽器直接用 WS 連 `proxy` 的 stdio。這其實就是 A 把 transport 端點移到遠端主機上。
- 缺點：遠端主機要能被瀏覽器連到、要有 TLS 和 token、COOP/COEP 跨 origin（lib.rs:790-798 要求 same-origin 資源）。瀏覽器端的 `WebRelayConnection` 可以直接重用 A 的，所以 **A 做完之後 E 只剩 transport endpoint 的差異**。

### 3.1 比較與推薦

| | A | B | C | D | E |
| --- | --- | --- | --- | --- | --- |
| 跟 desktop 共用程式碼 | 最高（RemoteClient 以上完全不改） | 低 | 不適用（跑的就是 web） | 低 | 高 |
| 每台主機要部署 | 只有 remote_server binary（自動部署） | 同 A | 整份 dist + 手動 | 無 | bridge + binary |
| 一個 web server 連多台主機 | 可以 | 可以 | 不行 | 可以 | 可以 |
| 面板正確性 | proto 路徑，desktop 有測過 | 需要翻譯層 | 完整 | 錯的機器 | 同 A |
| 工作量 | 大（約 12 WP） | 最大 | 極小 | 中 | A + 1 |
| 主要風險 | wasm 執行期、伺服器帶 gpui | 語意翻譯 | 維運 | 效能／語意 | 網路／安全 |

**推薦：A 當正式架構；C 當 WP0 馬上可用的 fallback；B、D 否決；E 等 A 完成後再評估。**
理由：A 是唯一一個「RemoteClient 以上零改動」的方案。它的新東西只有兩個 transport 半邊（瀏覽器的 `WebRelayConnection`、伺服器的 relay），而這兩半都可以跟 desktop 的 `MockConnection`（transport/mock.rs:186-300）對照著測試。

---

## 4. 推薦方案（A）的詳細設計

### 4.1 元件總表

| # | 元件 | 位置（新增／修改） | target | WP |
| --- | --- | --- | --- | --- |
| 1 | `EnvelopeFramer`：位元組流 ⇄ `Envelope` 的純函式編解碼 | 修改 `crates/remote/src/protocol.rs` | 兩邊都用 | WP1 |
| 2 | `ByteChannel`：二進位 WebSocket | 修改 `web/crates/wasm_rpc/src/lib.rs`；`web/vendor/smol_wasm/src/rpc.rs`、`lib.rs` 加 re-export | wasm | WP2 |
| 3 | 伺服器行程模型：gpui headless 在主執行緒、tokio 在背景、`--allow-ssh`、`--askpass=` | 修改 `crates/zed_web_server/src/main.rs`、`lib.rs`、`Cargo.toml`；新增 `crates/zed_web_server/src/ssh_host.rs` | native | WP3 |
| 4 | `SshCommandRecipe` + `RemoteConnection::command_recipe()`；bundled binary 提供者 hook | 修改 `crates/remote/src/remote_client.rs`、`transport/ssh.rs`、`transport.rs`、`remote.rs` | 兩邊都用（hook 只在 native） | WP4 |
| 5 | 控制面 `RemoteSsh::*`、`WebSshDelegate`、host pool、安全閘門 | 新增 `crates/zed_web_server/src/ssh_rpc.rs`；修改 `rpc.rs`、`ssh_host.rs` | native | WP5 |
| 6 | 資料面 `/remote/channel` relay | 新增 `crates/zed_web_server/src/ssh_relay.rs`；修改 `lib.rs` 路由 | native | WP6 |
| 7 | remote_server binary 的 build 與 manifest；伺服器端的 provider | 修改 `web/build.sh`；新增 `crates/zed_web_server/src/remote_server_bundle.rs` | build、native | WP7 |
| 8 | `WebRelayConnection` + wasm 的 `ConnectionPool` 改道 + `set_web_rpc_client` | 新增 `crates/remote/src/transport/web_relay.rs`、`crates/remote/src/web_relay_core.rs`（可在 native 測的純邏輯）；修改 `transport.rs`、`remote_client.rs`、`remote.rs` | wasm（core 兩邊都編） | WP8 |
| 9 | wasm 上的 remote 專案持久化 | `crates/workspace/src/persistence.rs`、`crates/sqlez`…（WP9 先調查） | wasm | WP9 |
| 10 | 入口、URL、project_manager | `web/crates/zed_web_workspace/src/main.rs`、`crates/project_manager/src/project_manager_panel.rs` | wasm | WP10 |
| 11 | 各面板在遠端專案上的行為 | `crates/claude_sessions/src/claude_sessions_panel.rs`、`tmux_sessions_panel.rs`（驗證）、main.rs 的 title bar | wasm | WP11 |
| 12 | gates 與 probe | 新增 `web/check-remote-seams.sh`；修改 `web/rpc-probe.mjs`、`web/one-sided-rpc.allowlist` | 腳本 | WP12 |

### 4.2 伺服器行程模型 [設計；依據已 verified]

**問題**：`remote::connect(options, delegate, cx: &mut AsyncApp)`（remote_client.rs:382-395）和 `start_proxy(…, cx: &mut AsyncApp)` 都要在 gpui 的前景執行緒上跑。zed_web_server 目前是 `#[tokio::main]`（main.rs:1-6），沒有任何 gpui App（`grep gpui crates/zed_web_server/src` 為零）。

**先例**：remote_server 本身就是「gpui headless 跑在主執行緒 + tokio」：`gpui_platform::headless()`（server.rs:570）、`release_channel::init`、`gpui_tokio::init`（server.rs:661-668）。`gpui_tokio::init_from_handle(cx, handle)` 可以直接接上外部建立的 runtime（gpui_tokio.rs:27-32）。在 macOS 上 headless 用的是 `MacPlatform::new(true)`（gpui_platform.rs:23-25、57-60），一定要在主執行緒跑 [主執行緒要求是 unverified 推論；remote_server 在 macOS 主機上就是這樣跑的]。

**規定**：

```rust
// crates/zed_web_server/src/main.rs (sketch; exact code is WP3's job)
fn main() -> anyhow::Result<()> {
    // Handled before clap and before any runtime: askpass.sh execs `<current_exe> --askpass=<sock>`
    // (askpass.rs generate_askpass_script), and clap would reject the flag.
    if let Some(socket) = std::env::args().find_map(|arg| arg.strip_prefix("--askpass=").map(str::to_owned)) {
        askpass::main(&socket);
        return Ok(());
    }
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    if !zed_web_server::ssh_enabled()? {
        return runtime.block_on(zed_web_server::run());          // unchanged path
    }
    zed_web_server::ssh_host::run_with_gpui(runtime)            // main thread becomes gpui
}
```

- `pub async fn run()`（lib.rs:80）**保持原樣**；新增 `pub async fn run_with_ssh_host(host: ssh_host::SshHostHandle) -> Result<()>`，兩者共用一個私有的 `run_inner(Option<SshHostHandle>)`。`AppState` 新增 `ssh: Option<ssh_host::SshHostHandle>` 欄位。
- `ssh_enabled()` 的真值來源：`--allow-ssh` CLI flag，或 `ZED_WEB_ALLOW_SSH=1`，或 `.zed/web.json` 的 `"ssh": {"enabled": true}`。**預設關閉**（§4.11）。解析方式跟 `load_restrict_paths` 一致（lib.rs:271-305），同一個來源不要寫兩份：抽一個 `fn read_web_json(root) -> Result<serde_json::Value>`。
- `run_with_gpui`：
  1. `let app = gpui_platform::headless();`
  2. `let (commands_tx, commands_rx) = futures::channel::mpsc::unbounded::<HostCommand>();`
  3. `let server = runtime.spawn(run_with_ssh_host(SshHostHandle { commands: commands_tx }));`
  4. `app.run(move |cx| { release_channel::init(web_app_version(), cx); gpui_tokio::init_from_handle(cx, runtime.handle().clone()); ssh_host::install(commands_rx, cx); /* when `server` finishes: cx.quit() */ })`
  5. 現有的特殊 argv 模式（`open`/`xdg-open` shim lib.rs:82-90、`__debug-adapter-proxy` lib.rs:91-93、`--printenv` lib.rs:102-105）行為不能變，而且要在 gpui 啟動之前就處理掉。
- `web_app_version()`：`AppVersion::load(env!("CARGO_PKG_VERSION"), option_env!("ZED_BUILD_ID"), option_env!("ZED_COMMIT_SHA").map(AppCommitSha::new))`，比照 server.rs:662-667 [`AppVersion::load` 的 signature 以 server.rs:662 為準]。
- **tokio 到 gpui 的跨執行緒只准走 `HostCommand` channel**。`AsyncApp` 不是 `Send`，絕對不能讓 tokio task 拿到它。

```rust
// crates/zed_web_server/src/ssh_host.rs
#[derive(Clone)]
pub struct SshHostHandle { commands: futures::channel::mpsc::UnboundedSender<HostCommand> }

pub(crate) enum HostCommand {
    Connect {
        options: remote::SshConnectionOptions,
        delegate: Arc<crate::ssh_rpc::WebSshDelegate>,
        reply: futures::channel::oneshot::Sender<anyhow::Result<ConnectedHost>>,
    },
    StartProxy {
        host_id: HostId,
        identifier: String,
        reconnect: bool,
        incoming_tx: futures::channel::mpsc::UnboundedSender<rpc::proto::Envelope>, // proxy -> relay
        outgoing_rx: futures::channel::mpsc::UnboundedReceiver<rpc::proto::Envelope>, // relay -> proxy
        cancel: futures::channel::oneshot::Receiver<()>,
        exit: futures::channel::oneshot::Sender<anyhow::Result<i32>>,
    },
    Acquire { host_id: HostId, handle_id: HandleId, reply: oneshot::Sender<anyhow::Result<()>> },
    Release { handle_id: HandleId },
    Check { host_id: HostId, reply: oneshot::Sender<bool> }, // `ssh -O check` via recipe
}

pub(crate) struct ConnectedHost {
    pub host_id: HostId,               // "h-" + 32 hex, random
    pub platform: remote::RemotePlatform,
    pub os_version: Option<String>,
    pub path_style: util::paths::PathStyle,
    pub shell: String,
    pub default_system_shell: String,
    pub recipe: remote::SshCommandRecipe,   // WP4
}
```

- gpui 端的 host pool 是 `Rc<RefCell<HostPool>>`（或一個 gpui `Entity<HostPool>`），裡面是 `HashMap<HostId, HostEntry>`。它只在 gpui 執行緒上被存取，所以**不需要 `Mutex`**。
  `HostEntry { options, connection: Arc<dyn RemoteConnection>, handles: HashSet<HandleId>, idle_since: Option<web_time::Instant> }`。
- **`install()` 的迴圈絕對不准 inline await 某一個命令**：每收到一個 `HostCommand` 就 `cx.spawn` 一個獨立的 task 去處理。否則分頁 A 卡在密碼提示時，分頁 B 連 `StartProxy` 都做不了。借用 `RefCell` 時不准跨過 `await`。
- 兩個分頁同時連同一台主機：`remote::connect` 自己的 `ConnectionPool` 已經會把「同一組 options、正在連線中」的請求合併成同一個 task（remote_client.rs:1244-1256），所以不會建出兩個 master。host pool 只要在 connect 完成之後，用 `Arc::ptr_eq` 判斷是不是同一個連線，把它合併到既有的 `HostEntry` 就好。
- `StartProxy`：在 gpui 上呼叫 `connection.start_proxy(identifier, reconnect, incoming_tx, outgoing_rx, activity_tx, delegate, cx)`，然後 `select!(io_task, cancel)`：cancel 先到就 drop `io_task`（ssh.rs:505-507 的 `kill_on_drop(true)` 會把 ssh proxy 殺掉）；io_task 先結束就把結果送進 `exit`。`activity_tx` 在伺服器端直接丟掉：心跳是瀏覽器端的 `RemoteClient` 負責，end-to-end。
- 伺服器端的 `start_proxy` 會用到的 `delegate` 就是 connect 時那一個（`set_status("Starting proxy")`，ssh.rs:477）。

### 4.3 控制面：`RemoteSsh::*`（在 `/rpc` 上的 JSON）

所有方法都在 `rpc::serve` 的主迴圈裡，用跟 `agent_rpc::handles` 同樣的方式分派（rpc.rs:448-462）：**`RemoteSsh::connect` 要 spawn 成獨立的 tokio task**，不能卡住整條連線（因為 askpass 提示可能要等好幾十秒）。回應透過該連線自己的 `outgoing` 送出（參考 extension network 那一段 rpc.rs:361-399 的寫法）。

`ssh_rpc::handles(method) = method.starts_with("RemoteSsh::")`。

#### 4.3.1 `RemoteSsh::capabilities`

```jsonc
// params
{}
// result
{
  "enabled": true,                 // false when --allow-ssh is off OR restrict_paths is on (§4.11)
  "reason": null,                  // string when enabled=false; names the flag/variable that caused it
  "commit": "a1b2c3d4…",           // manifest commit of bundled remote_server, null if none bundled
  "platforms": ["linux-x86_64", "macos-aarch64"] // bundled remote_server platforms (RemoteOs::as_str-RemoteArch::as_str)
}
```

#### 4.3.2 `RemoteSsh::connect`

```jsonc
// params
{
  "connect_id": "c-<32 hex>",      // client-generated; scopes notifications below
  "options": { /* remote::SshConnectionOptions, serde as derived (ssh.rs:138-150) */
    "host": {"Hostname": "devbox"}, "username": "andy", "port": 22, "password": null,
    "args": [], "port_forwards": null, "connection_timeout": null,
    "nickname": null, "upload_binary_over_ssh": false },
  "client_commit": "a1b2c3d4…"     // option_env!("ZED_COMMIT_SHA") of the wasm build, or null
}
// result
{
  "host_id": "h-<32 hex>",
  "handle_id": "k-<32 hex>",       // one per WebRelayConnection instance; see Release
  "platform": {"os": "linux", "arch": "x86_64"},   // RemoteOs::as_str / RemoteArch::as_str (remote_client.rs:64-106)
  "os_version": "ubuntu 24.04",
  "path_style": "unix",            // "unix" | "windows"  (PathStyle has no serde, path.rs:29-33)
  "shell": "/bin/bash",
  "default_system_shell": "/bin/sh",
  "recipe": {                      // remote::SshCommandRecipe (WP4) serde
    "ssh_options": ["-o","ControlMaster=no","-o","ControlPath=/tmp/zed-ssh-sessionAbC/ssh.sock"],
    "destination": "andy@devbox",
    "env": {},
    "shell": "/bin/bash",
    "is_windows": false,
    "path_style": "unix"
  }
}
```

連線期間的通知，**只送給發出這個請求的那條連線的 `outgoing`**，不送進 `session.notifications`（因為 session 會廣播給所有分頁，rpc.rs:82-92）：

| 方法名（用 `format!` 產生） | params |
| --- | --- |
| `RemoteSsh::status:{connect_id}` | `{"status": "Connecting" \| null}` |
| `RemoteSsh::prompt:{connect_id}` | `{"prompt_id": "p-<32 hex>", "prompt": "andy@devbox's password: "}` |
| `RemoteSsh::prompt_cancelled:{connect_id}` | `{"prompt_id": "p-…"}` |

通知名稱一律用 `format!("RemoteSsh::prompt:{connect_id}")` 產生，跟 `Terminal::data:{id}`（terminal_rpc.rs:474-475）一樣。原因有兩個：
(1) wasm_rpc 的 `on_notification` 每個方法名只能掛一個 handler，後掛的會蓋掉先掛的，而且沒有移除的 API（wasm_rpc lib.rs:516-518）；
(2) `web/check-one-sided-rpc.sh` 的 regex 是 `"[A-Z][A-Za-z]+::[a-z_]+"`，常數寫成 `"RemoteSsh::prompt:"`（尾巴帶冒號）才不會被誤判成「伺服器有 dispatch 但沒人呼叫」的 method。

錯誤（`error` 字串）必須能讓使用者看懂，至少要有這幾種：
- `"ssh is disabled on this server: start zed-web-server with --allow-ssh or set ZED_WEB_ALLOW_SSH=1"`
- `"ssh is refused while ZED_WEB_RESTRICT_PATHS is on"`
- `"host devbox is not in .zed/web.json ssh.allowed_hosts"`
- `"remote_server commit mismatch: browser a1b2…, bundled c3d4…; reload the page"`
- `"options.password is not accepted; the password is asked for when ssh needs it"`
- `"no bundled remote_server for linux-aarch64; rebuild with ZED_WEB_REMOTE_SERVER_TARGETS=…"`
- `remote::connect` 本身的錯誤原文（`"Failed to connect to host: …"`）

#### 4.3.3 `RemoteSsh::answer_prompt`

```jsonc
{"prompt_id": "p-…", "response": "hunter2"}   // response null == user cancelled
// result: null
```
伺服器端：`EncryptedPassword::try_from(response.as_str())`（askpass 的 encrypted_password.rs），送進存起來的 `oneshot::Sender<EncryptedPassword>`；`null` 就 drop 那個 sender（askpass 看到之後會回 `ControlFlow::Break`，進而殺掉 master，askpass.rs:158-166）。找不到對應的 `prompt_id` → 回 `Err("unknown or expired prompt")`。

#### 4.3.4 `RemoteSsh::cancel_connect`

```jsonc
{"connect_id": "c-…"}   // result: null
```
取消 in-flight 的 connect：drop 掉 gpui 端的 connect future，並取消所有 pending 的 prompt。`/rpc` 連線關閉時，伺服器要對這條連線發起的所有 connect 自動做同樣的事。

#### 4.3.5 `RemoteSsh::open_channel`

```jsonc
// params
{"handle_id": "k-…", "identifier": "web-dev-workspace-12", "reconnect": false}
// result
{"channel_id": "ch-<32 hex>", "token": "<64 hex>"}
```
- `identifier` 就是 `start_proxy` 收到的 `unique_identifier`，**瀏覽器端要先加上 `web-` 前綴**（§4.5.4）。伺服器要檢查格式：`^[A-Za-z0-9_-]{1,40}$`，不合格就拒絕（它最後會變成遠端主機上的 socket 目錄名，remote_client.rs:360-366）。
- token 只能用一次；30 秒內沒有 WS 來 attach，這個 channel 就作廢。

#### 4.3.6 `RemoteSsh::release` / `RemoteSsh::attach_handles`

```jsonc
// release
{"handle_id": "k-…"}                       // result: null
// attach_handles — sent after a /rpc reconnect (RpcClient::subscribe_reconnect, wasm_rpc lib.rs:524)
{"handle_ids": ["k-…"]}                    // result: {"attached": ["k-…"], "missing": []}
```
跟 `Fs::attach_watches` 的形狀完全一樣（rpc.rs:586-603）。handle 跟連線的 generation 綁在一起；連線關閉 30 秒後，還沒被重新 attach 的 handle 會被自動 release（比照 rpc.rs:648-686）。

### 4.4 資料面：`GET /remote/channel?channel_id=…&token=…`

- 掛在 `protected` router 底下（lib.rs:147-156），所以需要 cookie 驗證；升級時也要做 `auth::same_origin` 檢查（lib.rs:381）。
- **一個 WS 對應一個 proxy 行程**。不跟 `/rpc` 共用連線，理由是：`/rpc` 只能跑文字（§2.1）；大的 envelope 會卡住 JSON 回應（head-of-line blocking）；而且「WS 關閉 = proxy 結束」這個生命週期最清楚。
- 兩個方向都**只送 binary message**。payload 是**原始的 stdio 位元組流**，也就是一連串 `[u32 LE len][Envelope]` frame（protocol.rs:37-50）。**一個 WS message 不保證剛好是一個 frame**：送的一方可以任意切塊，每塊 ≤ 1 MiB；收的一方用 `EnvelopeFramer` 重組。
  這樣做就不會撞到 16 MiB 的 WS 上限（lib.rs:385-386），大檔案的 buffer 也不會被截斷。relay 路由的上限另外設成 `max_message_size(2 MiB)`。
- 伺服器端：WS binary → `EnvelopeFramer::push` → `outgoing_tx`（交給 proxy）；`incoming_rx`（proxy 送出的）→ `encode_frame` → WS binary。
- 關閉碼：

| 誰關的 | code | reason | 瀏覽器端的 `start_proxy` 回傳 |
| --- | --- | --- | --- |
| 伺服器：proxy 正常結束 | 1000 | `{"exit_code": N}` | `Ok(N)`（90 → `ServerNotRunning`，remote_client.rs:886-897） |
| 伺服器：token 不對或過期 | 4400 | `invalid channel token` | `Err` |
| 伺服器：channel 已經被 attach 過 | 4409 | `channel already attached` | `Err` |
| 伺服器：frame 解碼失敗、relay 錯誤 | 1011 | 錯誤訊息 | `Err` |
| 瀏覽器：`WebRelayConnection` 被 drop | 1000 | `client closed` | —（伺服器端送 `cancel`，殺掉 proxy） |
| 網路斷線，沒有收到 close frame | 1006 | — | `Err("relay disconnected")` → `RemoteClient` 觸發 reconnect |

- **重連完全交給既有的 `RemoteClient::reconnect`**：`start_proxy(reconnect=true)` → 新的 `open_channel` → 新的 WS → 遠端執行 `proxy --reconnect` → daemon 還活著 → `resync`（remote_client.rs:668-716）。這個 WS **不要**自己做自動重連（跟 `/rpc` 的 JS 重連器不同）。

### 4.5 瀏覽器端：`WebRelayConnection`

#### 4.5.1 放在哪裡、怎麼拿到 RpcClient

- 檔案：`crates/remote/src/transport/web_relay.rs`，整個檔案 `#[cfg(target_family = "wasm")]`。在 `transport.rs` 宣告成 `#[cfg(target_family = "wasm")] pub mod web_relay;`。
- 放在 `remote` crate 裡而不是 web crate 的原因：`build_command` 必須呼叫 ssh.rs 裡的 `build_command_posix/windows`（ssh.rs:1851、1969），它們是模組私有的 free function。放在同一個 crate、用 `pub(super)` 開放，才能確保「產生 ssh 指令」這件事只寫一次。
- RpcClient：在 web workspace 裡，remote crate 用到的 `smol` 是 `smol_wasm`，它 re-export 了 `wasm_rpc::RpcClient`（smol_wasm lib.rs:41-45、rpc.rs:8）。hook 照 terminal.rs:105-127 的 pattern 寫：

```rust
// crates/remote/src/transport/web_relay.rs
#[cfg(target_family = "wasm")]
static WEB_RPC_CLIENT: std::sync::OnceLock<smol::RpcClient> = std::sync::OnceLock::new();
#[cfg(target_family = "wasm")]
pub fn set_web_rpc_client(client: smol::RpcClient) { /* warn if already set, like terminal.rs:108-112 */ }
```
從 `remote.rs` re-export：`#[cfg(target_family = "wasm")] pub use transport::web_relay::set_web_rpc_client;`。
呼叫端：`zed_web_workspace::init_app_state`，跟 main.rs:1304-1308 那一排放在一起。

- 為了能在 native 上測試，**不碰 wasm API 的純邏輯**放在 `crates/remote/src/web_relay_core.rs`（不加 cfg，兩個 target 都編）：`prefixed_identifier`、`ConnectResponse` 的 serde 型別與解析（`platform`／`path_style` 字串 ⇄ enum）、close reason ⇄ `Result<i32>` 的對應、`ChannelPump` 的狀態機。`web_relay.rs` 只負責把這些東西接上 RpcClient 和 ByteChannel。

#### 4.5.2 trait 各方法怎麼實作

| 方法 | 行為 |
| --- | --- |
| `new(options, delegate, cx)`（inherent, async） | 產生 `connect_id` → 掛 `RemoteSsh::status:{id}`、`RemoteSsh::prompt:{id}`、`RemoteSsh::prompt_cancelled:{id}` 三個通知 handler → `call("RemoteSsh::connect", …)`。等待**沒有上限，但可以取消**（prompt 可能要等使用者）；被取消時送 `RemoteSsh::cancel_connect`。**notification handler 裡拿不到 `AsyncApp`**：`on_notification` 的 handler 型別是 `Fn(Value) + Send`，是從 JS 的 `onmessage` closure 裡呼叫的（wasm_rpc lib.rs:516、364-375）。所以 handler 只能把 `(prompt_id, prompt)` 推進一個 `mpsc::UnboundedSender`；由 `new()` 裡用 `cx.spawn` 起的一個迴圈把它讀出來，再呼叫 `delegate.ask_password(prompt, tx, cancel_rx, cx)`。這個形狀跟 `AskPassDelegate::new_with_cancellation` 完全一樣（askpass.rs:69-91），照著寫。delegate 就是 desktop 的那一個，會顯示同一個 `RemoteConnectionModal`（remote_connection.rs:462-482）。等 `tx` → `password.decrypt(IKnowWhatIAmDoingAndIHaveReadTheDocs)` → `call("RemoteSsh::answer_prompt", …)`；使用者取消就送 `response: null`。`status` 通知也用同樣的方式轉給 `delegate.set_status`。結束後把三個 handler 換成 no-op（因為沒有移除的 API）。呼叫 `subscribe_reconnect()`，每次 `/rpc` 重連就送 `RemoteSsh::attach_handles`。 |
| `start_proxy(identifier, reconnect, incoming_tx, outgoing_rx, activity_tx, delegate, cx) -> Task<Result<i32>>` | `call("RemoteSsh::open_channel", {handle_id, identifier: prefixed_identifier(&identifier), reconnect})` → `RpcClient::open_byte_channel("/remote/channel?channel_id=…&token=…")` → 兩個 pump：`outgoing_rx` → `encode_frame` → 送出；收到 bytes → `EnvelopeFramer::push` → 每個 envelope 送進 `incoming_tx`，並 `activity_tx.try_send(())`（跟 transport.rs:190-209 一樣）→ 等到 close，依 §4.4 的表回傳。**不准用 `smol::Timer`**（§5.4）。 |
| `kill()` | 設定 `killed` 旗標 → `call("RemoteSsh::release", {handle_id})`。錯誤要回傳，不准吞掉。 |
| `has_been_killed()` | 讀 `killed` 旗標 |
| `build_command(...)` | `crate::transport::ssh::build_ssh_command(&self.recipe, program, args, env, working_dir, port_forward, interactive)`（WP4）。**同步、純函式、不發 RPC**。 |
| `build_forward_ports_command` | `Err(anyhow!("port forwarding is not available from the browser; see docs/web-zed-plan.md §6.5"))` |
| `upload_directory` | `Task::ready(Err(anyhow!("upload_directory is not supported by the web relay yet")))`。desktop 在 SSH 上的 caller 是 dev container 流程 [unverified：還沒 grep 所有 caller]；WP8 要 grep 並列出。 |
| `connection_options()` | `RemoteConnectionOptions::Ssh(self.options.clone())`。**一定要是原本的 options**，這樣 `ConnectionPool` 的 key、workspace 持久化、`remote_connection_identity` 才會跟 desktop 一致。 |
| `path_style / remote_platform / remote_os_version / shell / default_system_shell` | 直接回傳 connect 回應裡的值 |
| `has_wsl_interop()` | `false` |
| `shares_network_interface()` | 用預設的 `false` |

#### 4.5.3 wasm 上的 `ConnectionPool` 改道

remote_client.rs:1279-1283 的 `Ssh` 那一支改成：

```rust
RemoteConnectionOptions::Ssh(opts) => {
    #[cfg(target_family = "wasm")]
    { crate::transport::web_relay::WebRelayConnection::new(opts, delegate, cx).await
        .map(|connection| Arc::new(connection) as Arc<dyn RemoteConnection>) }
    #[cfg(not(target_family = "wasm"))]
    { SshRemoteConnection::new(opts, delegate, cx).await
        .map(|connection| Arc::new(connection) as Arc<dyn RemoteConnection>) }
}
```
`Wsl` 和 `Docker` 在 wasm 上回 `Err(anyhow!("WSL/dev container connections are not available from the browser"))`。
另外要把 `SshRemoteConnection::new` 這個函式本身標成 `#[cfg(not(target_family = "wasm"))]`：這樣 A′ 就會在**編譯期**被擋下來，而不是等到執行期才失敗。如果 ssh.rs 裡有其他東西依賴它、導致連鎖的 cfg 修改，WP8 要回報影響範圍，不要自己擴大修改。

#### 4.5.4 identifier 前綴

`prefixed_identifier(id) = format!("web-{id}")`。原因：遠端 `proxy` 在非 reconnect 模式下，如果發現同一個 identifier 的 server 已經在跑，**會先把它殺掉**（server.rs:905-913）。desktop 的 identifier 是 `dev-workspace-{id}`，其中 id 來自 desktop 的 DB；web 的 id 來自伺服器上的 SQL。同一台遠端主機同時被 desktop 和 web 連上時，兩邊的 id 可能撞號，然後互相殺掉對方的 server。
長度：`web-dev-workspace-` 是 18 個字元，加上 id；伺服器端檢查 ≤ 40 字元。socket 路徑的預算見 remote_client.rs:360-366 的註解（大約 100 字元）。

### 4.6 `SshCommandRecipe`：讓瀏覽器產生 terminal 用的 ssh 指令

**問題**：`RemoteConnection::build_command` 是同步函式（remote_client.rs:2066-2074），在 wasm 上不能發 RPC。它要的資料是 `SshSocket::ssh_command_options()`（ControlPath 在伺服器上的路徑，ssh.rs:1444-1458）、destination、shell、shell kind、path style、env，這些都在 `SshRemoteConnection` 的私有欄位裡（ssh.rs:43-57）。

**設計（WP4）**：

```rust
// crates/remote/src/remote_client.rs — additive, default impl keeps every existing transport unchanged
pub trait RemoteConnection: Send + Sync {
    // …existing methods…
    /// The pieces `build_command` uses, for a transport that must rebuild commands elsewhere.
    fn command_recipe(&self) -> Option<crate::SshCommandRecipe> { None }
}

// crates/remote/src/transport/ssh.rs
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SshCommandRecipe {
    pub ssh_options: Vec<String>,      // SshSocket::ssh_command_options()
    pub destination: String,           // connection_options.ssh_destination()
    pub env: HashMap<String, String>,  // SshSocket::envs
    pub shell: String,                 // ssh_shell
    pub is_windows: bool,              // ssh_platform.os.is_windows()
    pub path_style: RecipePathStyle,   // "unix" | "windows"
}

pub fn build_ssh_command(
    recipe: &SshCommandRecipe,
    program: Option<String>, args: &[String], env: &HashMap<String, String>,
    working_dir: Option<String>, port_forward: Option<(u16, String, u16)>, interactive: Interactive,
) -> Result<CommandTemplate>
```
- `SshRemoteConnection::build_command`（ssh.rs:331-380）改成 `build_ssh_command(&self.recipe(), …)`，**desktop 的輸出必須跟改之前 byte-for-byte 一樣**（驗收：把 ssh.rs:2083-2260 既有的 `test_build_command*` 全部跑過）。
- `ShellKind` 不做序列化，在接收端用 `ShellKind::new(&recipe.shell, recipe.is_windows)` 重新算出來（ssh.rs:795 就是這樣做的；shell.rs:205）。
- 替代方案（**不採用**，列出來讓大腦裁決）：拿 `build_forward_ports_command(vec![])` 的 `args[..len-2]` 反推 ssh options（ssh.rs:382-405）。這是「從一個事實推論另一個事實」，handoff §6 明確警告過這種做法會漂移。

**Terminal 在遠端專案上的完整路徑** [verified]：`create_remote_shell`（terminals.rs:623-661）→ `build_command(program, args, env, working_dir, None, Interactive::Yes)` → `Shell::WithArguments { program: "ssh", args }` → wasm 的 terminal 走 `RemotePty` → `Terminal::open`（remote_pty.rs:440）→ 伺服器在 PTY 裡 spawn `ssh -o ControlMaster=no -o ControlPath=<伺服器 tmp> -t user@host "cd …; exec $SHELL -l"`（terminal_rpc.rs:80-140）。
遠端專案的 terminal 傳進來的 `local_path` 是 `None`（terminals.rs:99、335），所以伺服器不會拿遠端路徑去 `chdir`，也不會碰到 `restrict_paths` 的限制 [verified]。

### 4.7 連線生命週期

#### 4.7.1 第一次連線（需要密碼）

```
wasm launch()/project_manager      zed_web_server (tokio)              gpui thread                 remote
  open_remote_project(opts,paths)
  → RemoteConnectionModal
  → remote::connect → pool(wasm) → WebRelayConnection::new
      on_notification(prompt:c1…)
      call RemoteSsh::connect{c1} ───▶ gates(§4.11) → HostCommand::Connect ──▶ remote::connect(opts, WebSshDelegate)
                                                                           AskPass + MasterProcess ──ssh──▶
                                   ◀── WebSshDelegate.ask_password ◀─────── prompt "password:"
      ◀── notify prompt:c1 {p1}
      delegate.ask_password → modal
      user types → answer_prompt{p1} ──▶ tx.send(EncryptedPassword) ──────▶ askpass returns ──────────▶ auth ok
                                                                           uname/shell/os_version ────▶
                                                                           ensure_server_binary (WP4 hook)
                                                                             `<bin> version` != commit → upload
                                   ◀── ConnectedHost ◀────────────────────
      ◀── {host_id, handle_id, recipe…}
  open_remote_project_with_new_connection
    deserialize_remote_project (WP9!)
    RemoteClient::new → start_proxy
      call RemoteSsh::open_channel ──▶ {channel_id, token}
      WS /remote/channel?… ──────────▶ HostCommand::StartProxy ─────────────▶ start_proxy ──ssh env <bin> proxy──▶ daemon
      ═══ RemoteStarted / Ack / Ping ═════════════ relay ═════════════════════════════════════════════════════▶
    Project::remote(session) → Workspace
```

#### 4.7.2 其他事件

| 事件 | 會發生什麼 | 由誰處理 |
| --- | --- | --- |
| 重新整理分頁 | 舊的 WS 關閉 → proxy 被殺 → daemon 還在；新分頁用同一個 workspace id → `ConnectionIdentifier::Workspace(id)` 相同 → 非 reconnect 的 `proxy` 會殺掉舊 daemon 再啟一個新的（server.rs:905-913），跟 desktop 重開 window 的行為一樣 | 既有 |
| `/rpc` 斷線（筆電睡眠） | 資料面 WS 通常也一起斷 → `start_proxy` 回 `Err` → `RemoteClient::reconnect` → `kill()`（`release` 會先排隊，等 `/rpc` 恢復）→ `pool.connect` → 新的 `WebRelayConnection::new` → 伺服器的 `HostEntry` 還在 grace 期間 + `-O check` 通過 → **不需要重新驗證** → `proxy --reconnect` → `resync` | §4.12 的 grace + 既有的 RemoteClient |
| `/rpc` 斷太久 | `MAX_RECONNECT_ATTEMPTS=3` 用完 → `ReconnectExhausted` → `DisconnectedOverlay` 顯示重連按鈕 | 既有 |
| SSH 斷線（ControlMaster 死掉） | proxy 的 exit code 是 255 → `Ok(255)` → reconnect → 伺服器 `Check` 失敗 → 丟掉這個 host → 重新 `remote::connect` → 可能要重新輸入密碼 | WP5 |
| 遠端 daemon 已經不在 | `proxy --reconnect` exit 90 → `ServerNotRunning`（remote_client.rs:886-897） | 既有 |
| zed_web_server 重啟 | `Server::hello` 的 instance id 變了 → JS reload 整頁（wasm_rpc lib.rs:108-118） | 既有 |
| 關閉分頁 | 兩條 WS 都關 → proxy 被殺；30 秒後 handle 被自動 release；再 30 秒 grace 後 host 被丟掉（master 被殺、TempDir 被刪）；遠端 daemon 閒置 10 分鐘後自己退出（server.rs:409-450） | WP5、WP6 |

### 4.8 remote_server binary 的部署與版本比對

**事實**：這個 fork 是 dev channel，而 dev channel 在 release build 裡根本沒有辦法部署 binary（§2.2）；遠端必須跑這個 fork 的 build（fork 自己加了 proto）；協定沒有版本握手。

**設計（WP4 + WP7）**：

1. **build**（`web/build.sh`，WP7）：在 native server 那一步之後（build.sh:280-291），用同一個 stable toolchain 和 native target dir `cargo build --release -p remote_server`，並且對它**和 wasm 那一步**都 export `ZED_COMMIT_SHA="$(git -C "$repo_dir" rev-parse HEAD)"`。產物：

```
web/dist/bin/remote/
  manifest.json
  zed-remote-server-<os>-<arch>        # e.g. zed-remote-server-linux-x86_64, mode 0755
```
```jsonc
// manifest.json
{"commit": "a1b2c3d4e5f6…", "binaries": [
  {"os": "linux", "arch": "x86_64", "file": "zed-remote-server-linux-x86_64", "sha256": "…"}]}
```
   `os`／`arch` 字串要跟 `RemoteOs::as_str`／`RemoteArch::as_str` 完全一致（remote_client.rs:64-106）。預設只 build 本機的 triple；`ZED_WEB_REMOTE_SERVER_TARGETS="aarch64-unknown-linux-gnu …"` 可以加其他 triple（需要對應的 cross toolchain [unverified]）。
   Linux 上 desktop 預設用 musl static（transport.rs:292-317）；web 預設先用 gnu，並在文件裡寫明「遠端主機的 glibc 不能比 build 機器舊」[unverified 相容範圍]。

2. **remote crate 的 hook**（WP4，只在 native）：

```rust
// crates/remote/src/transport.rs
pub struct BundledRemoteServer { pub path: PathBuf, pub version: String }
pub fn set_bundled_remote_server_provider(
    provider: impl Fn(RemotePlatform) -> Option<BundledRemoteServer> + Send + Sync + 'static,
)
```
   `ensure_server_binary`（ssh.rs:834）的**第一步**：provider 回 `Some(bundle)` 時，
   - `dst = .zed_server/zed-remote-server-web-{bundle.version 的前 12 字元}`；
   - `run_command(dst, ["version"])` 的輸出 `.trim()` 等於 `bundle.version` → 直接使用；
   - 否則：`upload_local_server_binary(bundle.path, tmp)` + `extract_server_binary(dst, tmp)`（ssh.rs:1067-1160；tmp 不以 `.gz` 結尾，所以只做 `chmod + mv`，ssh.rs:1141-1147）；
   - 上傳之後再跑一次 `version` 驗證，不一致就 `Err`。
   provider 回 `None`，或沒設定 provider → 走原本的流程，**desktop 行為不變**（驗收要證明這一點）。
   - 會誤殺的合法輸入：遠端已經有一個正確的 `zed-remote-server-dev-build`，但我們不會用它 → 這是刻意的（它的內容無法驗證）。

3. **伺服器端**（WP7）：`remote_server_bundle.rs` 在 SSH 啟用時讀 `<current_exe 所在目錄>/remote/manifest.json`，註冊 provider；`RemoteSsh::capabilities` 回報 commit 和平台。**這個路徑的常數只能寫在一個地方**，然後由 WP12 的 gate 比對 build.sh 真的寫到那裡。

4. **版本比對**：wasm 用 `option_env!("ZED_COMMIT_SHA")` 取得自己的 commit，在 connect 時帶上 `client_commit`。伺服器端：兩邊都有值而且不同 → 拒絕（錯誤訊息要求使用者 reload）；manifest 不存在 → 拒絕，並點名 build.sh。
   `PROTOCOL_VERSION`（rpc.rs:19 = 68）只用在 collab 的握手，不能拿來比對 remote_server [verified：remote 的 stdio 路徑上沒有用到它]。

### 4.9 URL 格式與入口

- **URL 格式：不新增參數**，直接把 `ssh://` URI 放進既有的 `path=`：
  `/?path=ssh%3A%2F%2Fandy%40devbox%3A2222%2Fhome%2Fandy%2Fproj`
  URI 格式就是 `projects.json` 在用的那一套：`project_manager::remote_project_uri`（產生，project_location.rs:88-132）和 `parse_project_location`（解析，:47-86）。所以這件事只做一次。
- 伺服器的 `index`（lib.rs:637-668）看到有 `path=` 就直接提供頁面；`canonical_workspace_location`（lib.rs:672-702）只改寫以 `/workspace` 開頭的值，`ssh://…` 不受影響 [verified by reading；WP10 要補一個測試]。
- `launch()`（main.rs:2348-2415）：`workspace_paths_from_url()` 拿到的每個值先過 `parse_project_location(first, rest)`：
  - `Local(paths)` → 原本的 `workspace::open_paths`；
  - `Remote { options: Ssh(..), paths }` → `recent_projects::open_remote_project(options, paths, app_state, OpenOptions::default(), cx)`（remote_connections.rs:147）；這時候它就是第一個 window，所以不會撞到「GPUI web 只支援一個 top-level window」（project_manager_panel.rs:191-193 的註解）；
  - `Remote { Wsl | Docker }` → 在 console 印錯誤，並用 local root 開啟，同時顯示 toast。
- `sync_project_paths_url`（main.rs:199-213）：專案是遠端時（`project.read(cx).remote_client()` 是 `Some`），每個 worktree 的 `abs_path` 都要用 `remote_project_uri(&options, path)` 轉回 URI；而且**不要呼叫** `save_active_workspace`，因為伺服器的 `Workspace::activate` 會拿本機的 `Fs::canonicalize` 去處理遠端路徑（rpc.rs:508-514、690-712）。
- `project_manager`：把 wasm 的拒絕區塊（project_manager_panel.rs:226-237）換成：把 `paths` 用 `remote_project_uri(&options, p)` 轉成 URI → `crate::open_in_new_tab(&uris_as_pathbufs)`（project_manager.rs:254-259，最後會到 main.rs:99-110 的 `zedOpenWorkspaceInNewTab`，它只會 append `path=`）。`RemoteSsh::capabilities.enabled == false` 時照舊拒絕，但錯誤訊息改成伺服器回傳的 `reason`。WSL／Docker 照舊拒絕。
- `workspace_id` 參數：這次不處理（§8.1 Q6）。

### 4.10 各面板

| 面板 | 要做的事 | WP |
| --- | --- | --- |
| project panel／editor／LSP／git／search／tasks | 什麼都不用改；靠 WP13 的 e2e 驗收 | — |
| terminal | 靠 WP4 的 recipe 加 WP8 的 `build_command`；手動驗收：開一個 terminal，`hostname` 印出遠端主機名稱 | WP8 |
| `claude_sessions` | claude_sessions_panel.rs:2785-2799：wasm 上改成「`project.remote_client()` 是 `Some` → `RemoteSource::new(proto_client, executor)`；是 `None` → `WebSource`」。`RemoteSource` 在 wasm 上編不編得過要實際確認（session_source.rs:457-470） | WP11 |
| `tmux_sessions` | 已經依 `remote_client()` 選路徑（tmux_sessions_panel.rs:91-94）；attach 要透過 remote terminal。只驗證、不改 | WP11 |
| `project_manager` | §4.9 | WP10 |
| title bar | `path_hint` 是空的時候顯示 `"remote"`（main.rs:1958-1963），在真的遠端專案上會讓人誤會；改成顯示 `RemoteConnectionOptions::display_name()` | WP11 |
| `DisconnectedOverlay` | 已經編進 wasm（recent_projects.rs:3）。確認它在 `ReconnectExhausted`／`ServerNotRunning` 時會出現，而且它的「重連」按鈕在 web 上走 `open_remote_project`，不會想開第二個 window | WP11 |
| web_agent_panel／extensions | 不在範圍內；遠端專案上它們繼續對 zed_web_server 本機操作。WP11 要在 UI 上標示清楚，或在遠端專案上隱藏（由大腦裁決，§8.1 Q5） | — |

### 4.11 安全

**威脅模型**（沿用 plan §6.7）：拿到 web token 的人，已經可以透過 `Process::spawn` 以伺服器使用者的身分執行任何指令，當然也包括 `ssh`。所以開放 SSH 在權限上**沒有新增**，但會把「用伺服器使用者的 ssh key／agent 去連其他主機」變成 UI 上一鍵就能做的事。規定：

1. **預設關閉**：`--allow-ssh`／`ZED_WEB_ALLOW_SSH=1`／`.zed/web.json` 的 `ssh.enabled`（§4.2）。關閉時，`capabilities` 要回傳 `reason`；`connect` 回錯誤；`/remote/channel` 回 403。
2. **允許的主機清單**（可選）：`.zed/web.json` 的 `"ssh": {"allowed_hosts": ["devbox", "*.corp.example"]}`。比對的對象是 `SshConnectionOptions.host.to_string()`，只支援開頭的 `*.` 萬用字元。有設清單時，不在清單上就拒絕。
3. **`ZED_WEB_RESTRICT_PATHS` 開啟時，`RemoteSsh::*` 一律拒絕**。依 plan §6.7「一個限制套用到每一個 RPC，沒有例外」：遠端主機上的路徑本來就在被限制的根目錄之外。錯誤訊息要點名這個變數。（這是一個裁決，列在 §8.1 Q1，由大腦確認。）
4. **`options.args` 過濾**：瀏覽器可以傳任意的 `SshConnectionOptions.args`。伺服器要用 `SshConnectionOptions::parse_command_line` 的白名單邏輯（ssh.rs:1665-1671）重新驗證；尤其要拒絕 `-o ProxyCommand=…`、`-o LocalCommand=…`、`-o PermitLocalCommand=…`（這三個會在伺服器上執行任意指令）。`-o` 的其他值維持允許。會誤殺的合法輸入：使用者本來就依賴 `ProxyCommand` 的跳板設定 → 請他改寫在伺服器的 `~/.ssh/config`（那個由 ssh 自己讀，不受這個過濾影響）。
5. **憑證**：`connect` 的 `options.password`（`SshConnectionOptions.password: Option<String>`，ssh.rs:142；desktop 把它當成 `known_password` 用，remote_connections.rs:291-299）**不是 `null` 就拒絕**。祕密只能經由 prompt 流程傳送，否則就多了一條沒有經過審查的憑證路徑。伺服器**從不**把密碼寫到磁碟；`EncryptedPassword` 只存在記憶體，用完就丟。瀏覽器也不存。建議使用伺服器上的 ssh-agent（`SSH_AUTH_SOCK` 會繼承伺服器行程的環境，plan §10.6）或使用者既有的 ControlMaster（ssh.rs:651-659）。
6. **channel token**：32 bytes 亂數、只能用一次、30 秒內有效；`/remote/channel` 另外還有 cookie 驗證和 same-origin 檢查。
7. **log**：prompt 的內容可以 log（它不含密碼）；`answer_prompt` 的 params **絕對不能** log。rpc.rs 在失敗時會 `tracing::warn!(?error, %method, …)`（rpc.rs:422），但不會印 params，這一點要保持。

### 4.12 多分頁、多連線共用與資源清理

| 資源 | 擁有者 | key | 釋放時機 |
| --- | --- | --- | --- |
| `HostEntry`（ControlMaster、TempDir、`Arc<dyn RemoteConnection>`） | gpui 執行緒上的 host pool | `RemoteConnectionOptions`（跟 `ConnectionPool` 一樣，remote_client.rs:1230-1233）＋ `HostId` | handles 變成空的之後，經過 `HOST_GRACE = 30s`；或 `Check` 失敗；或伺服器關閉 |
| handle | `/rpc` 連線的 generation | `HandleId` | `release`；或連線關閉 30 秒後沒有 `attach_handles` |
| channel（proxy 行程） | `/remote/channel` 的 WS | `ChannelId` | WS 關閉 → 送 `cancel` → drop io task → 殺掉 ssh proxy |
| 遠端 daemon | 遠端主機 | identifier | 閒置 10 分鐘（server.rs:409-450） |

- 多個分頁連同一台主機：共用同一個 `HostEntry`（同一個 ControlMaster），每個分頁有自己的 handle 和 proxy（跟 desktop 多個 window 一樣）。
- **跟 desktop 不一樣的地方（刻意的）**：desktop 的 `kill()` 會直接殺 master，所以每次重連都要重新驗證（remote_client.rs:661-666 → ssh.rs:304-315）。web 在 `kill()` 時只是 release handle，master 會保留 30 秒，因為 web 最常見的斷線原因是 WS 斷掉，而不是 SSH 斷掉。在 reuse 之前一定要先過 `ssh -O check`，所以不會把一個已經死掉的 master 交出去。這一點列在 §8.1 Q2 請大腦確認。
- 伺服器關閉（`shutdown_signal`，lib.rs:196-224）：tokio 那邊結束之後通知 gpui `cx.quit()`；host pool 被 drop → 所有 master 被殺（`kill_on_drop`）。

---

## 5. wasm 與跨機器的陷阱

| # | 陷阱 | 出處 | 規定 |
| --- | --- | --- | --- |
| 5.1 | **`cfg!` 回答的是編譯時的 target，不是另一台機器**。這個 port 已經踩過三次（commit `cf8f1b55ba` 的訊息）。ssh.rs 的 `#[cfg(not(windows))]`、`#[cfg(windows)]`（:169-174、181、227、650、723、1307、1316、1432、1446）在 wasm 上都會走 `not(windows)` 那一支 | ssh.rs | 瀏覽器端只能透過 recipe 產生指令，recipe 必須由伺服器上的 native 程式碼產生。遠端主機的 OS 只能從 `connect` 回應裡的 `platform` 得知，不准用 `cfg!` |
| 5.2 | `std::process::id()` 在 wasm 上會 panic | claude_sessions.rs:3550-3552；ssh.rs:881、912 | ssh.rs 的 connect 和部署路徑只能在伺服器上跑（§4.5.3 用 cfg 擋掉） |
| 5.3 | `std::time::Instant` / `SystemTime::now()` 在 wasm 上會 panic | handoff §2b、plan §5.5；`web/check-wasm-time.sh`、`web/check-wasm-systemtime.sh` | 一律用 `web_time`（remote_client.rs:54 已經是這樣） |
| 5.4 | `smol::Timer` 在 wasm 上**永遠不會觸發** | smol_wasm lib.rs:50-80 | 一律用 `cx.background_executor().timer()` |
| 5.5 | 會阻塞的呼叫：`smol::block_on`、`now_or_never` 等著一個還沒完成的 future、`lock_shared` 的 spin（wasm_rpc lib.rs:12-26） | handoff §3 fuzzy 那一段 | wasm 路徑上不准同步等待；不准拿著 `lock_shared` 的 guard 跨過 `await` |
| 5.6 | 執行緒：web 是用 `gpui_platform::single_threaded_web()` 啟動的（main.rs:2378），所有 `background_spawn` 其實都跑在 UI 執行緒上 | gpui_platform.rs:41-47 | `EnvelopeFramer::push` 每次最多解碼 N 個 frame 就要讓出（或者本身就是依 chunk 處理），不准一次解一個 100 MB 的 buffer 卡住畫面 |
| 5.7 | 訊息大小：`/rpc` WS 上限是 16 MiB（lib.rs:385-386）；frame 的長度是 u32，`read_message_with_len` 會直接 `resize` 到那個長度，沒有上限（protocol.rs:15-23） | | 資料面用 1 MiB 的 chunk；`EnvelopeFramer` 設 `max_frame_len`，預設 1 GiB。**會被誤殺的合法輸入**：超過 1 GiB 的單一 envelope（目前找不到會產生這種 envelope 的地方 [unverified]）→ 測試要證明 64 MiB 的 envelope 可以通過 |
| 5.8 | backpressure：瀏覽器的 `WebSocket.send` 不會阻塞，`bufferedAmount` 會無限制地長上去 | | `ByteChannel` 在 `bufferedAmount > 8 MiB` 時暫停從 `outgoing_rx` 取資料，用 gpui timer 每 10 ms 檢查一次 |
| 5.9 | 壓縮：stdio 路徑沒有壓縮，wasm 上也沒有 zstd | protocol.rs；rpc/Cargo.toml:39-40 | 不要加壓縮。permessage-deflate 要不要開 → [unverified]，先不開 |
| 5.10 | wasm_rpc 只處理文字 | wasm_rpc lib.rs:334-343；rpc.rs:310 | binary 資料不准走 `/rpc` |
| 5.11 | `on_notification` 每個方法名只有一個 handler，而且沒辦法移除 | wasm_rpc lib.rs:516-518 | 每次 connect 用自己的方法名；結束後換成 no-op |
| 5.12 | session 的通知會廣播給同一個 session 的所有連線 | rpc.rs:82-92 | prompt 只送給發出請求的那條連線的 `outgoing` |
| 5.13 | web 只能有一個 window | project_manager_panel.rs:191-193 | 遠端專案只能在 `launch()` 當第一個 window 開，或是開新的分頁 |
| 5.14 | **wasm 上的 workspace DB 寫入**：handoff §5 記錄 `get_or_create_remote_connection` 用的 `self.write` 在 wasm 上會失敗；而 `open_remote_project_with_new_connection` 在建立 RemoteClient 之前就會呼叫 `deserialize_remote_project` → `get_or_create_remote_connection` + `next_id`（workspace.rs:11587-11610、persistence.rs:2208-2214） | | **這是會擋住整條路的問題** → WP9，排在 WP10 之前 |
| 5.15 | 虛擬根目錄 `/workspace` 的改寫：`Terminal::open` 的 program／args 會經過 `rewrite_legacy_workspace_path`（terminal_rpc.rs:106-121、fs_rpc.rs:125-133）。某個 arg 如果**剛好**是 `/workspace/…` 開頭的遠端路徑，就會被改成伺服器的本機路徑 | | ssh 的 exec 字串是 `cd … && …` 開頭，不會被改到 [verified by reading build_command_posix ssh.rs:1851-1880]；但 `ssh_options` 裡如果有 `-i /workspace/key` 就會被改掉 → WP8 的測試要涵蓋這個情況，並在 §8 記錄 |
| 5.16 | `Workspace::activate` 會在本機 canonicalize 路徑 | rpc.rs:508-514、690-712 | 遠端專案不准呼叫它（§4.9） |
| 5.17 | dev channel 加上版本 0.0.0 | RELEASE_CHANNEL、main.rs:1390 | identifier 會是 `dev-workspace-N`；binary 部署一律走 WP4 的 hook |
| 5.18 | `wasm_rpc::call` 沒有逾時；`RemoteClient::reconnect` 呼叫 `pool.connect` 時也沒有逾時（remote_client.rs:681-686） | | `WebRelayConnection` 的 `open_channel`、`release`、`attach_handles` 都要包 30 秒逾時；`connect` 因為要等使用者輸入，不設逾時，但可以取消 |
| 5.19 | 初始連線逾時是 60 秒（release）；`web-release` profile 繼承 release，所以 `debug_assertions` 是關的 [unverified：web/Cargo.toml 的 profile 設定沒有逐行讀] | remote_client.rs:164-165 | 部署大 binary 的時間算在 `remote::connect` 裡（伺服器端，沒有這個逾時），不算在 `RemoteClient::new` 的 ready 等待裡 → OK |

**必須維持綠燈的 gates**（handoff §4）：
`web/check-panel-actions.sh`、`web/check-one-sided-rpc.sh`、`web/check-workspace-isolation.sh`、`web/check-refusals.sh`、`web/check-wasm-time.sh`、`web/check-wasm-systemtime.sh`、`./web/build.sh`、兩個 target 的 `cargo check`、`./script/clippy`、`cargo fmt --all -- --check`。
新增一個：`web/check-remote-seams.sh`（WP12）。

---

## 6. 分階段派工計畫

### 6.0 依賴圖

```
WP0 (C fallback, independent)

WP1 framer ─┬─▶ WP6 relay ──┐
            └─▶ WP8 WebRelay ┤
WP2 ByteChannel ─▶ WP8       │
WP3 server process ─▶ WP5 control ─▶ WP6
WP4 recipe+hook ─┬─▶ WP5
                 ├─▶ WP7 bundle
                 └─▶ WP8
WP9 wasm persistence (independent investigation; blocks WP10)
WP5+WP6+WP7+WP8+WP9 ─▶ WP10 entry/URL ─▶ WP11 panels
WP12 gates: after WP5 (method names) and WP7 (paths); re-run at the end
WP13 e2e harness: after WP6 (server half testable alone), full after WP10
Blind tests BT1..BT4: written from this spec only, in parallel with their WPs, held out
```

建議順序：**WP0 → (WP1 ∥ WP2 ∥ WP3 ∥ WP4 ∥ WP9) → WP5 → WP6 → WP7 → WP8 → WP10 → WP11 → WP12 → WP13**。
每個 WP 完成後都跑 review loop，輪次依作者而定：**Claude 寫的**（WP0、WP2、WP3、WP4、WP5、WP6、WP8、WP9、WP13）→ grok 4.7 xhigh → gemini-3.8-flash-high → grok 4.7 xhigh；**grok／gemini 寫的**（WP1、WP7、WP10、WP11、WP12）→ grok 4.7 xhigh → gemini-3.8-flash-high → Sonnet 5.5 high →（只有安全／刪除才做）Opus 5.5 high，與作者同家族的那一輪順延。安全相關的 WP5、WP6 要再加一輪不同家族的審查。

模型分配依 owner 的表格；owner 目前特別交代「盡量用 Opus 5.5 寫 code」，所以表格允許 Claude 的格子一律用 Opus 5.5。

---

### WP0 — 方案 C：把 web 部署到遠端主機的腳本（fallback）

- **目標**：今天就能在瀏覽器裡用遠端主機上的專案。
- **檔案**：新增 `web/scripts/deploy-remote.sh`；在 `docs/web-zed-handoff.md` §7 加一段說明（不要改其他段落）。
- **介面**：`web/scripts/deploy-remote.sh <ssh-destination> <remote-project-path> [--port 8090]`：
  1. 確認 `web/dist/bin/zed-web-server` 存在，而且 `file` 顯示的架構跟 `ssh <dest> uname -sm` 一致，不一致就中止並說明原因；
  2. `rsync -a web/dist/ <dest>:~/.zed-web/dist/`；
  3. `ssh <dest> 'nohup ~/.zed-web/dist/bin/zed-web-server <path> ~/.zed-web/dist/static --port <port> > ~/.zed-web/server.log 2>&1 &'`；
  4. 印出 `ssh -N -L <port>:127.0.0.1:<port> <dest>`，以及 token 檔案的位置 `<path>/.zed/web-auth-token`（lib.rs:248）。
- **驗收**：`shellcheck` 通過；對 `localhost` 手動跑一次，瀏覽器能開啟、能登入。
- **依賴**：無。**模型**：`Sonnet 5.5 high`（沒有測試守著的中等任務）。

---

### WP1 — `EnvelopeFramer`：位元組流編解碼（純函式）

- **目標**：一份兩個 target 共用的、會測試的 frame 編解碼，讓 WP6 和 WP8 共用。
- **檔案**：`crates/remote/src/protocol.rs`（在現有函式後面新增；不改現有函式）。
- **事實清單**：
  - frame = `u32::to_le_bytes(len)` + `Envelope::encode_to_buffer` 的輸出，`len = message.encoded_size() as u32`（protocol.rs:37-50）。
  - 解碼用 `Envelope::decode_from_slice`（protocol.rs:21-22）。
  - `MESSAGE_LEN_SIZE = 4`（protocol.rs:9）。
- **介面**：
```rust
pub const DEFAULT_MAX_FRAME_LEN: usize = 1 << 30;
pub fn encode_frame(envelope: &Envelope, out: &mut Vec<u8>) -> Result<()>;   // appends
pub struct EnvelopeFramer { buffer: Vec<u8>, max_frame_len: usize }
impl EnvelopeFramer {
    pub fn new(max_frame_len: usize) -> Self;
    /// Appends bytes, returns every complete envelope in order. A frame whose declared length
    /// exceeds max_frame_len is an error, and the framer is unusable afterwards (poisoned).
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Envelope>>;
    pub fn pending_len(&self) -> usize;
}
```
- **驗收（單元測試，native）**：一個 frame 一次送完；一個 frame 切成 1 byte 一塊送；三個 frame 合在一塊送；長度前綴被切在中間；長度 0 的 envelope（`Envelope::default()`）；64 MiB 的 envelope（證明上限不會誤殺合法輸入）；長度超過上限 → `Err`，而且後續的 `push` 也都是 `Err`；解碼失敗 → `Err`。**終止性**：`push` 的迴圈每一輪都一定會消耗掉 `≥ 4` bytes，或者直接 return。
- **依賴**：無。**模型**：`gemini-3.8-flash-high`（規格可以寫到事實清單那麼死的 core 套件）；盲測 BT1 由 `Sonnet 5.5 high` 寫。

---

### WP2 — wasm_rpc 的 `ByteChannel`（二進位 WebSocket）

- **目標**：讓 wasm 端可以開一條只跑 binary 的 WS，不影響現有的 `/rpc`。
- **檔案**：`web/crates/wasm_rpc/src/lib.rs`（新增一段 inline JS 和 Rust 包裝）；`web/vendor/smol_wasm/src/rpc.rs` 與 `lib.rs`（re-export `ByteChannel`，只在 wasm）。
- **事實清單**：
  - 現有的 inline JS 在 wasm_rpc lib.rs:28-223；用 `#[wasm_bindgen(inline_js = …)] extern "C"` 綁定（:225-243）。
  - WS URL 的組法跟 main.rs:1284-1292 一樣：`wss` 或 `ws`，加上 `location.host`。
  - `RpcClient` 裡面沒有存 base URL，所以新方法要收完整的 URL。
- **介面**：
```rust
pub struct ByteChannel {
    pub sender: futures::channel::mpsc::UnboundedSender<Vec<u8>>,
    pub receiver: futures::channel::mpsc::UnboundedReceiver<Vec<u8>>,
    pub closed: futures::channel::oneshot::Receiver<ByteChannelClose>,
}
#[derive(Clone, Debug)]
pub struct ByteChannelClose { pub code: u16, pub reason: String, pub was_clean: bool }
impl RpcClient {
    #[cfg(target_family = "wasm")]
    pub fn open_byte_channel(&self, url: &str) -> anyhow::Result<ByteChannel>;
    #[cfg(not(target_family = "wasm"))]
    pub fn open_byte_channel(&self, _url: &str) -> anyhow::Result<ByteChannel> { Err(anyhow!("… only on WASM")) }
}
```
  JS 端：`binaryType = "arraybuffer"`；**不自動重連**；在 `onopen` 之前送的資料要排隊；`bufferedAmount > 8 MiB` 時暫停從 sender 取資料（§5.8）；收到文字訊息 → 以 code 1003 關閉。`sender` 被 drop → 以 1000 `"client closed"` 關閉。
- **驗收**：`cargo check --target wasm32-unknown-unknown`（在 `web/` 下）；native 的 `cargo check -p wasm_rpc`；手動：在瀏覽器 console 對一個 echo 端點測試（WP6 完成後可以對 `/remote/channel` 測）；`check-workspace-isolation.sh` 的 V6（smol_wasm 在 native 上只是 re-export）要保持綠燈。
- **依賴**：無。**模型**：`Sonnet 5.5 high`（JS glue，沒有測試守著）。

---

### WP3 — 伺服器行程模型：gpui headless + tokio、`--allow-ssh`、`--askpass=`

- **目標**：SSH 啟用時，zed_web_server 的主執行緒跑 gpui headless，axum 跑在 tokio 上；沒啟用時行為完全不變。
- **檔案**：`crates/zed_web_server/src/main.rs`、`lib.rs`、`Cargo.toml`；新增 `crates/zed_web_server/src/ssh_host.rs`（這個 WP 只有 `SshHostHandle`、`HostCommand` 的骨架，以及 `run_with_gpui`、`install`；`Connect` 先回 `Err("not implemented: WP5")`）。
- **事實清單**：§4.2 全部。另外：
  - `main.rs` 目前的內容就是 `#[tokio::main] async fn main() { zed_web_server::run().await }`（main.rs:1-6）。
  - 新的依賴（全部都是 root workspace 的成員）：`gpui.workspace`、`gpui_platform.workspace`、`gpui_tokio.workspace`、`release_channel.workspace`、`askpass.workspace`、`rpc.workspace`、`semver.workspace`。remote_server 的寫法可以參考（remote_server/Cargo.toml:42-58）。
  - root `Cargo.lock` 只能**增加**行（handoff §1 / phase4c §4.1 的規則）；用 `--locked` 驗證沒有改到既有的 package。
- **驗收**：
  - 單元測試：`ssh_enabled` 的真值表（CLI／env／web.json 三個來源，加上衝突的情況）；`--askpass=` 的偵測（有 `=`、沒有 `=`、放在其他參數後面）。
  - 整合測試：`--allow-ssh` 啟動後，`web/rpc-probe.mjs 'Home::dirs' '{}'` 仍然正常；沒有 `--allow-ssh` 時，`ps` 看不到任何 gpui 執行緒 [怎麼觀察由實作者提出]。
  - 手動：Linux 和 macOS 各啟動一次，而且 `Ctrl-C` 可以正常結束（驗證 tokio 結束 → gpui quit 的串接）。
  - Linux 上的 build 大小／時間：記錄加上 gpui_platform 前後的差異（§8.2）。
- **依賴**：無。**模型**：`Opus 5.5 high`（跨執行緒的架構）。

---

### WP4 — remote crate：`SshCommandRecipe`、`command_recipe()`、bundled binary hook

- **目標**：(a) 讓 ssh 指令可以在別的地方由同一份程式碼產生；(b) 讓 zed_web_server 可以部署自己帶的 remote_server。**desktop 行為不變**。
- **檔案**：`crates/remote/src/remote_client.rs`（trait 加一個有預設實作的方法）、`crates/remote/src/transport/ssh.rs`、`crates/remote/src/transport.rs`、`crates/remote/src/remote.rs`（re-export）。
- **事實清單**：§4.6、§4.8 第 2 點。另外：
  - `build_command_posix` 的參數順序（ssh.rs:1851-1864）：`input_program, input_args, input_env, working_dir, port_forward, ssh_env, ssh_path_style, ssh_shell, ssh_shell_kind, ssh_options, ssh_destination, interactive`；`build_command_windows` 相同（ssh.rs:1969）。
  - `SshSocket::envs` 在 unix 上一定是空的（ssh.rs:1307-1314）。
  - `run_command` 回傳 stdout 字串，失敗時是 `Err`（ssh.rs:1388-1404）。
  - 上傳時的 tmp 名稱：native 上可以用 `std::process::id()`（它只在伺服器上跑）；要加一段亂數尾綴，避免同一個伺服器內同時有兩個 connect 在上傳時撞名。
- **驗收**：
  - ssh.rs:2078 之後既有的所有測試不改、全部通過（證明 desktop 的輸出沒變）。
  - 新測試：`build_ssh_command(&recipe, …)` 對同樣的輸入，產生跟 `SshRemoteConnection::build_command` 一樣的 `CommandTemplate`（posix 和 windows 各一組）；recipe 的 serde 來回轉換一致。
  - hook：沒有 provider 時，`ensure_server_binary` 不做任何新的事（用一個 fake `SshSocket`／command runner 驗證；如果目前的結構做不到 fake，**回報，不要自己重構**）。
  - `cargo check -p remote`（native）；在 `web/` 下做 wasm 的 check；`./script/clippy -p remote`。
- **依賴**：無。**模型**：`Opus 5.5 high`（desktop 共用的 crate、公開的 trait）。

---

### WP5 — 伺服器控制面：`RemoteSsh::*`、`WebSshDelegate`、host pool、安全閘門

- **目標**：§4.3 的所有方法（`open_channel` 只負責發 token；真正 attach 的是 WP6）。
- **檔案**：新增 `crates/zed_web_server/src/ssh_rpc.rs`；修改 `rpc.rs`（分派；handle 跟 generation 綁定；斷線時的回收）、`ssh_host.rs`（`Connect`／`Acquire`／`Release`／`Check` 的實作、host pool、grace）、`lib.rs`（`AppState.ssh`、讀 `.zed/web.json` 的 `ssh` 區塊）。
- **事實清單**：§4.3、§4.11、§4.12；rpc.rs 的分派結構（:300-640）；`EncryptedPassword::try_from(&str)`（askpass/encrypted_password.rs）；`RemoteClientDelegate` 的 4 個方法（remote_client.rs:136-159）。
  - `WebSshDelegate::get_download_url` → `Task::ready(Ok(None))`；`download_server_binary_locally` → `Err("remote_server binaries are provisioned by zed-web-server's bundle; see RemoteSsh::capabilities")`（有 WP4 hook 時根本不會呼叫到它）。
  - `Check`：照抄 `find_existing_control_master` 已經在用的參數（ssh.rs:600-611）：`ssh <additional_args> -O check -o ControlPath=<socket> <destination>`，stdin/stdout/stderr 都是 null，exit 0 代表還活著。socket 路徑從 recipe 的 `ControlPath=` 那一項取得。
- **驗收**：
  - 單元測試：allowed_hosts 的比對（完全相同、`*.x`、大小寫、IPv6）；args 過濾（§4.11 第 4 點，包含會誤殺的案例：`-o ServerAliveInterval=30` 必須通過）；restrict_paths 開啟時拒絕；identifier 格式驗證；prompt 只送到發出請求的那條連線（用兩個 `outgoing` 模擬兩個分頁）；未知 `prompt_id` → 錯誤；連線關閉 → 30 秒後 handle 被回收（用 tokio 的 paused time）。
  - 盲測 BT2（§7）。
  - `web/check-one-sided-rpc.sh`：這時候 client 還沒寫，所以要把新方法暫時加到 allowlist 的「unfinished work」區塊，**並附上理由**，寫明是哪一個 WP 會補上呼叫端：`connect`／`answer_prompt`／`cancel_connect`／`open_channel`／`release`／`attach_handles` → WP8；`capabilities` → WP10（project_manager）。**每一行由補上呼叫端的那個 WP 負責刪掉**（gate 會檢查過期的條目）。
- **依賴**：WP3、WP4。**模型**：`Opus 5.5 high`；安全相關，review loop 之外再加一輪不同家族的審查（grok 4.7 xhigh）。

---

### WP6 — 伺服器資料面：`/remote/channel` relay

- **目標**：§4.4。
- **檔案**：新增 `crates/zed_web_server/src/ssh_relay.rs`；修改 `lib.rs`（在 `protected` 加上路由）。
- **事實清單**：§4.4 的關閉碼表；WS 升級的寫法參考 lib.rs:376-390；`HostCommand::StartProxy` 的形狀（§4.2）；`EnvelopeFramer`（WP1）。
- **驗收**：
  - 整合測試（不需要真的 ssh）：在測試裡把 `HostCommand::StartProxy` 接到一個假的 proxy。這個假 proxy 用一個 `ChannelClient` 對接（可以借 `RemoteClient::proto_client_from_channels`，remote_client.rs:539-547）或是回音 envelope。驗證：token 錯誤 → 4400；重複 attach → 4409；proxy 結束 → 1000 `{"exit_code":N}`；client 關閉 → 收到 `cancel`；1 byte 一塊地送一個 5 MiB 的 envelope，能完整收到。
  - 盲測 BT3。
- **依賴**：WP1、WP5。**模型**：`Opus 5.5 high`（協定）。

---

### WP7 — remote_server binary 的 build、manifest 與伺服器端 provider

- **目標**：§4.8 第 1、3、4 點。
- **檔案**：`web/build.sh`；新增 `crates/zed_web_server/src/remote_server_bundle.rs`；修改 `ssh_host.rs`（啟動時呼叫 `remote::set_bundled_remote_server_provider`）、`ssh_rpc.rs`（`capabilities`、connect 時比對 commit）。
- **事實清單**：build.sh 的 native 那一步（build.sh:280-291）；`dist_dir/bin` 的配置（build.sh:275-276）；manifest 的 schema（§4.8）；`RemoteOs/RemoteArch::as_str`。
- **驗收**：
  - 單元測試：manifest 解析（缺欄位、平台不認得、sha256 對不上 → 錯誤）；provider 依 `RemotePlatform` 選對檔案。
  - `./web/build.sh` exit 0，並產生 `web/dist/bin/remote/manifest.json` 和至少一個 binary；`web/dist/bin/remote/zed-remote-server-<os>-<arch> version` 印出的內容等於 manifest 的 `commit`。
- **依賴**：WP4、WP5。**模型**：`grok 4.7 high`（有單元測試守著的串接）；build.sh 的部分由大腦實際跑過驗收。

---

### WP8 — 瀏覽器端 `WebRelayConnection` + wasm 的 `ConnectionPool` 改道

- **目標**：§4.5。
- **檔案**：新增 `crates/remote/src/transport/web_relay.rs`（wasm）、`crates/remote/src/web_relay_core.rs`（兩個 target）；修改 `transport.rs`、`remote_client.rs:1279-1305`、`remote.rs`；`web/crates/zed_web_workspace/src/main.rs` 的 `init_app_state` 加一行 `remote::set_web_rpc_client(remote_client.clone())`。
- **事實清單**：§4.3、§4.4、§4.5、§4.6；**`remote` 目前不在 `web/crates/zed_web_workspace/Cargo.toml` 裡，也不是 `web/Cargo.toml` 的 `[workspace.dependencies]` key** [verified: grep 只找到 `wasm_remote`]。所以要在 `web/Cargo.toml` 加 `remote = { path = "../crates/remote" }`（路徑的算法見 phase4b §2.2），在 zed_web_workspace 加 `remote.workspace = true`。這是 handoff §2b 列的最常見失敗形狀，漏掉它 `main.rs` 就編不過；加完之後要跑 `web/check-workspace-isolation.sh` 的 M1/M2；trait 的簽名（remote_client.rs:2043-2092）；`MockRemoteConnection` 是最接近的範本（transport/mock.rs:186-300）；stdio pump 的寫法（transport.rs:128-240）；`RemoteConnectionModal` 的 delegate（remote_connection.rs:462-482）。
- **驗收**：
  - `web_relay_core` 的 native 單元測試：`prefixed_identifier`、connect 回應的解析（每一個欄位、不認得的 os 字串 → `Err`）、close reason → `Result<i32>`（§4.4 表格的每一列）。
  - wasm 的 `cargo check`；`web/check-wasm-time.sh`、`check-wasm-systemtime.sh` 綠燈。
  - 盲測 BT4（`web_relay_core`）。
  - `grep -n '^remote' web/crates/zed_web_workspace/Cargo.toml` 有結果；`web/Cargo.lock` 只能增加行。
  - 把 WP5 暫時加的、呼叫端在這個 WP 裡的 allowlist 條目刪掉（`connect`、`answer_prompt`、`cancel_connect`、`open_channel`、`release`、`attach_handles`），`check-one-sided-rpc.sh` 要綠燈。
  - 手動（需要 WP9、WP10 都完成才能做）：見 WP13。
- **依賴**：WP1、WP2、WP4、WP5、WP6。**模型**：`Opus 5.5 high`。

---

### WP9 — wasm 上的遠端專案持久化（會擋住整條路的調查＋修正）

- **目標**：讓「開啟遠端專案」這條路徑上**每一個碰到 DB 的地方**在 wasm 上都能成功，而不只是其中一個函式。已知的有：
  - `find_existing_workspace`（remote_connections.rs:156）、`workspace::remote_workspace_position_from_db`（remote_connections.rs:231）：在 `deserialize_remote_project` **之前**就會讀 DB；
  - `deserialize_remote_project`（workspace.rs:11587-11610）：`get_or_create_remote_connection`（persistence.rs:2208-2214，`self.write`）、`remote_workspace_for_roots`、`next_id`；
  - `Project::remote(…, init_worktree_trust = true, …)`（workspace.rs:11422）會走到 `save_trusted_worktrees`（persistence.rs:3118）；
  - `ToolchainStore::init(&remote_proto)`（project.rs:1714）會走到 `toolchains`／`set_toolchain`（persistence.rs:3031、3081）。
  handoff §5 列了**四個**在 wasm 上會失敗的 `self.write` closure（`get_or_create_remote_connection`、`toolchains`、`set_toolchain`、`save_trusted_worktrees`），這四個全部都在這條路上。
- **檔案**：先調查，再決定。可能涉及 `crates/workspace/src/persistence.rs`、`crates/sqlez/src/thread_safe_connection.rs`（`write` 在 :186-206）、`crates/db`、web 的 SQL RPC（plan §5.7 的 Revision 1–4）。
- **為什麼不能直接寫事實清單**：handoff §5 只記錄了「`get_or_create_remote_connection` 在 web 上是 dead code，而且會失敗」，沒有說它**怎麼**失敗；plan §5.7 的 Revision 1 規定「SQL 走 RPC，但 workspace layout 不走 SQL」。這兩條在這裡會衝突（遠端專案的 layout 要靠 `remote_connections` 表）。**所以這一格派給困難格，而且第一步是回報，不是修改**。
- **第一步的產出**：一份（以回覆的形式，不寫檔）清單：從 `open_remote_project` 到 `Project::remote` 回傳為止，**每一個** DB 讀寫的呼叫路徑、它在 wasm 上實際的行為（成功／回錯誤／panic）、失敗點（file:line）、以及修正選項（例如：`get_or_create_remote_connection` 在 wasm 上改走 async 的 `Sql::query … RETURNING`，plan §5.7 Revision 1c 已經證明它會回傳資料列）。由大腦裁決之後才進行修改。
- **驗收**：一個 wasm 端就能跑的測試，或者 rpc-probe 加上一段：對同一組 options 呼叫兩次，得到同一個 id；`next_id` 單調遞增。
- **依賴**：無（可以跟 WP1–4 並行）。**模型**：`Opus 5.5 high`。

---

### WP10 — 入口、URL、project_manager

- **目標**：§4.9。
- **檔案**：`web/crates/zed_web_workspace/src/main.rs`（`launch`、`sync_project_paths_url`、`save_active_workspace` 的呼叫點）；`crates/project_manager/src/project_manager_panel.rs:217-238`；`crates/zed_web_server/src/lib.rs`（只加測試：`canonical_workspace_location` 不會改動 `ssh://` 的值）。
- **事實清單**：§4.9；`parse_project_location`、`remote_project_uri` 的簽名（project_location.rs:47、88）；`open_remote_project` 的簽名（remote_connections.rs:147-153）；`open_in_new_tab`（project_manager.rs:254-259）。
- **驗收**：
  - 刪掉 allowlist 裡的 `RemoteSsh::capabilities`，`check-one-sided-rpc.sh` 綠燈。
  - lib.rs 新增測試：`/?path=ssh%3A%2F%2Fa%40h%2Fworkspace%2Fx` → `canonical_workspace_location` 回 `None`（遠端路徑剛好是 `/workspace` 開頭也不會被改）。
  - project_manager：wasm 那一支的單元測試（如果能在 native 上把「產生 URI 列表」那一段抽成純函式來測）。
  - `web/check-panel-actions.sh` 綠燈；wasm 的 `cargo check`。
- **依賴**：WP8、WP9。**模型**：`grok 4.7 high`（有 tsc 守著的串接）。

---

### WP11 — 各面板在遠端專案上的行為

- **目標**：§4.10 表格中標了 WP11 的每一列。
- **檔案**：`crates/claude_sessions/src/claude_sessions_panel.rs:2785-2799`；`web/crates/zed_web_workspace/src/main.rs:1958-1963`（title bar）；驗證 `crates/tmux_sessions/src/tmux_sessions_panel.rs:91-94`；驗證 `crates/recent_projects/src/disconnected_overlay.rs`。
- **事實清單**：`RemoteSource::new(proto_client, executor)`（session_source.rs:462-466）；`WebSource::new(executor)`（:914-919）。注意：`claude_sessions_panel.rs` 目前在 working tree 裡有其他 agent 未 commit 的修改，**開工前先 `git diff` 看過，只做最小修改**。
- **驗收**：wasm 的 `cargo check`；`check-panel-actions.sh`；手動（WP13）：在遠端專案上打開 claude_sessions 面板，看到的是**遠端主機**上的 session。
- **前置事實（派工前由大腦先確認）**：在 `web/` 下跑 wasm 的 `cargo check`，證明 `RemoteSource` 在 wasm 上編得過（它目前被 `#[cfg(not(target_family = "wasm"))]` 的那一支獨佔，claude_sessions_panel.rs:2790-2798）。確認之前，這件事的事實不在 prompt 裡，不能降級派工。
- **依賴**：WP10。**模型**：前置事實確認為「編得過」→ claude_sessions 的選路交給 `gemini-3.8-flash-medium`（機械性的小修改）；「編不過」→ 升級成 `Opus 5.5 high`（跨 crate 的 wasm 移植）。DisconnectedOverlay 的 web 行為如果需要改 → `grok 4.7 high`。

---

### WP12 — gates 與 probe

- **目標**：把這次新增的「兩個半邊、中間只靠字串接起來」的接縫變成會紅的檢查（handoff §4 的規則：新增這種接縫時，要一起加 check）。
- **檔案**：新增 `web/check-remote-seams.sh`；修改 `web/rpc-probe.mjs`（`RemoteSsh::capabilities` 加進預設的檢查清單）、`web/one-sided-rpc.allowlist`。
- **`check-remote-seams.sh` 的斷言**（每一條都要有「為什麼」的註解；最後一行印 `REMOTE SEAMS OK`，而且只在成功時印）：
  1. `web/crates/zed_web_workspace/src/main.rs` 有呼叫 `remote::set_web_rpc_client(`；
  2. `crates/zed_web_server/src/` 有呼叫 `set_bundled_remote_server_provider(`；
  3. build.sh 寫出 manifest 的路徑，跟 `remote_server_bundle.rs` 讀的是同一個相對路徑（從兩個檔案各自抽出字串來比對）；
  4. `crates/remote/src/remote_client.rs` 的 `ConnectionPool::connect` 裡有 `cfg(target_family = "wasm")` 的 `WebRelayConnection` 分支；
  5. `SshRemoteConnection::new` 帶有 `cfg(not(target_family = "wasm"))`；
  6. `web/one-sided-rpc.allowlist` 裡已經沒有任何 `RemoteSsh::` 開頭的條目。
  找不到任何比對對象時要**失敗**，不准當作通過。
- **驗收**：刻意拿掉每一條對應的程式碼，gate 要變紅（每條各做一次，把 RED 輸出貼出來），再放回去變綠。
- **依賴**：WP5、WP7、WP8。**模型**：`gemini-3.8-flash-medium`。

---

### WP13 — 端到端測試 harness

- **目標**：§7.2 的第 2、3 層。
- **檔案**：新增 `web/test-support/fake-ssh`（可執行的 shell 腳本）、`web/test-support/remote-e2e.mjs`；`docs/web-zed-handoff.md` 加上怎麼跑的說明。
- **驗收**：`remote-e2e.mjs` 對 fake-ssh 模式和 docker sshd 模式都綠燈；把 WP6 的 exit-code 對應刻意弄壞，e2e 要變紅。
- **依賴**：WP6（伺服器那一半可以先測）、WP10（完整流程）。**模型**：`Opus 5.5 high`（harness 本身會碰協定）。

---

### 6.x 盲測（held out；實作者和 fixer 都不准讀、不准跑、不准改）

| 編號 | 對象 | 只給這些 | 撰寫者 |
| --- | --- | --- | --- |
| BT1 | `EnvelopeFramer` | WP1 的介面與 §5.7 | `Sonnet 5.5 high` |
| BT2 | `RemoteSsh::*` 的 JSON 形狀、錯誤訊息、prompt 的路由 | §4.3、§4.11，以及 `ssh_rpc` 的公開 dispatch 函式簽名 | `Opus 5.5 medium` |
| BT3 | `/remote/channel` 的關閉碼、切塊、token 語意 | §4.4 | `Opus 5.5 medium` |
| BT4 | `web_relay_core` 的解析和對應 | §4.3.2 回應的 schema、§4.4 的表、§4.5.4 | `Sonnet 5.5 high` |

盲測寫完的當下必須是紅的（因為實作還不存在），而且要紅在對的原因上。

---

## 7. 測試策略

### 7.1 盲測要斷言什麼

- **BT1**：任意切塊的結果都跟一次送完的結果完全一樣（可以用 proptest 或固定的切點）；超過上限的 frame 會讓 framer 進入 poisoned 狀態；64 MiB 可以通過；空輸入不產生任何 envelope。
- **BT2**：
  - `connect` 的回應包含 §4.3.2 的所有欄位，而且型別正確；
  - SSH 關閉時，回傳的錯誤字串裡有 `--allow-ssh`；restrict_paths 開啟時，錯誤字串裡有 `ZED_WEB_RESTRICT_PATHS`；
  - 兩條連線各自發起 connect，prompt 只送到發起的那一條；
  - `answer_prompt` 送 `null` 會取消；
  - args 帶 `ProxyCommand` 會被拒絕，`ServerAliveInterval` 會通過；
  - identifier 超過 40 字元，或含有 `/`，會被拒絕。
- **BT3**：§4.4 表格的每一列；token 只能用一次；30 秒沒有 attach 就失效（用 paused time）。
- **BT4**：`"linux"/"macos"/"windows"` 和 `"x86_64"/"aarch64"` 都能解析，不認得的值 → `Err`；reason `{"exit_code":90}` → `Ok(90)`；1006 → `Err`；identifier 的前綴。

### 7.2 不需要真的遠端主機的 SSH 測試（分三層）

1. **單元／in-process**：WP1、WP4、WP5、WP6、WP8 core 各自的測試。WP6 用一個假的 proxy（`ChannelClient` 或回音），完全不需要 ssh。
2. **fake-ssh**（hermetic，不需要 sshd）：`web/test-support/fake-ssh` 模擬 ssh.rs 會用到的每一種呼叫形狀，然後放在 `PATH` 最前面：
   - `ssh -G <dest>` → 印一行 `controlpath /nonexistent`（讓 `find_existing_control_master` 找不到，ssh.rs:561-632）；
   - master：`-N … -o ControlMaster=yes -o ControlPath=<p>` → 在 `<p>` 建一個 unix socket（`python3` 不准用；用 `nc -lU`，或者直接只 `touch` 一個檔案 [要驗證 `wait_connected` 只讀 stdout 到 EOF，ssh.rs:216-224]），然後關閉 stdout 並一直睡；
   - `-O check` → exit 0；
   - 其他指令 `… <dest> <cmd…>` → `exec sh -c "<cmd…>"`（在本機執行）；
   - sftp／scp → `cp`。
   這樣真的 remote_server 會在本機跑起來，proto 完整地走一遍：`RemoteStarted` → `Ack` → `Ping`。askpass：fake master 可以依 `FAKE_SSH_PROMPT=1` 呼叫 `$SSH_ASKPASS "password:"`，檢查輸出等於預期的密碼，才建立 socket → 這樣就測到了 prompt 的完整往返。
3. **docker sshd**：`linuxserver/openssh-server`（或者 `debian` + `openssh-server`）開在 `localhost:2222`，用密碼登入和 key 登入各跑一次；測試 host key 的 yes/no prompt。macOS 開發機也可以用系統的「遠端登入」當成 `ssh localhost`。

`remote-e2e.mjs`：沿用 rpc-probe.mjs 手動握手的做法（它的 cookie 處理就是這樣，rpc-probe.mjs:12-13）：`RemoteSsh::connect` → 處理 prompt → `open_channel` → 開 `/remote/channel` → 手動編碼一個 `Envelope { id:1, payload: RemoteStarted }`（`remote_started = 381`、`id = 1`，zed.proto:25、414）和 `Ping`（`ping = 7`）→ 期待收到 `Ack`（`ack = 5`）以及對方送來的 `RemoteStarted`。protobuf 的編碼手寫就好（只有 varint 和 length-delimited，不要加依賴）。

### 7.3 瀏覽器手動驗收（WP10 完成之後，每次改動 relay 都要做）

1. `zed-web-server <root> <static> --allow-ssh`，開啟 `/?path=ssh%3A%2F%2F<user>%40<host>%2F<path>`。
2. 出現 `RemoteConnectionModal` → 輸入密碼 → 專案面板出現遠端的檔案。
3. 打開一個檔案，確認 LSP 有 hover；做一次專案搜尋；git 面板顯示遠端 repo 的狀態。
4. 開 terminal，`hostname` 印出遠端主機名稱；執行一個 task。
5. claude_sessions 面板顯示遠端的 session；tmux 面板列出遠端的 tmux session。
6. DevTools → Network 把網路設成 Offline 10 秒再恢復 → 不需要重新輸入密碼就能恢復編輯。
7. 在遠端 `pkill -f zed-remote-server` → 出現 `DisconnectedOverlay`。
8. 在 project_manager 點一個 `ssh://` 項目 → 開出一個新分頁，而且可以正常連線。
9. console 裡沒有任何 panic；`web/check-*.sh` 全部綠燈。

---

## 8. 未決問題與未驗證事項

### 8.1 未決問題（需要大腦裁決）

1. **Q1 — `ZED_WEB_RESTRICT_PATHS` 開啟時要不要禁止 SSH？** 本 spec 依 plan §6.7「沒有例外」建議禁止。反方論點：restrict_paths 限制的是**這台**伺服器的檔案系統，而 SSH 帶出去的是別台主機的存取。兩種解讀都說得通，**需要裁決**。
2. **Q2 — `kill()` 的語意要跟 desktop 不一樣嗎？** §4.12 建議保留 master 30 秒（web 最常見的斷線是 WS 斷線）。如果裁決「要跟 desktop 完全一樣」，那麼每次 WS 斷線都要重新驗證。
3. **Q3 — 要不要在公開的 `RemoteConnection` trait 上加 `command_recipe()`？** 這會改到 desktop 共用 crate 的公開 API（雖然有預設實作、是純增加）。替代方案（從 `build_forward_ports_command` 反推）被本 spec 否決，理由見 §4.6。需要 owner 同意修改公開 API。
4. **Q4 — 其他平台的 remote_server 要怎麼 build？** 預設只 build 本機 triple；linux-aarch64／macOS 等 cross build 需要的 toolchain 還沒量測。另一個選項：讓伺服器在沒有 bundle 時退回 desktop 的「從原始碼 build」路徑（需要開 `build-remote-server-binary` feature，ssh.rs:869）。
5. **Q5 — 遠端專案上的 web_agent_panel／extensions 怎麼處理？** 它們會繼續在 zed_web_server 本機上運作，可能讓使用者誤以為是在遠端執行。選項：隱藏、加上標示、或者之後改走 proto 的 `AgentServerStore::init_remote`（project.rs:1718）。
6. **Q6 — `workspace_id` URL 參數目前沒有任何地方會寫入**（§2.1），所以所有分頁共用一個 RPC session。這次的設計（prompt 送到連線、handle 綁 generation）不依賴它；但要不要趁這次一起修，需要決定。
7. **Q7 — plan §6.6 的否決理由要不要改寫？** 它否決的是「伺服器當 HeadlessProject」。本 spec 的 A 方案不是那樣（伺服器只是 transport；HeadlessProject 在遠端主機上），但 plan 裡「reuse remote::* over JSON RPC」那段論述需要補一條註記，說明它不適用於真正的遠端主機。大腦決定要不要修改 plan。
8. **規格上的張力（已記錄，不自行解決）**：handoff §5 說 `get_or_create_remote_connection` 在 web 上是「dead code」，但本 spec 會讓它變成必經之路；plan §5.7 Revision 1 規定「workspace layout 不走 SQL」，但遠端專案的 layout 要依賴 `remote_connections` 表。→ WP9 的第一步就是回報這件事，由大腦裁決。

### 8.2 未驗證事項

- **這份文件裡的所有東西都沒有編譯過，也沒有執行過。** 標了 [verified] 的只代表原始碼是這樣寫的。
- `Project::remote`、`RemoteClient`、`ChannelClient` **從來沒有在 wasm 上執行過**；`RemoteSource`（claude_sessions）在 wasm 上編不編得過也不知道。
- zed_web_server 加上 `gpui_platform` 之後，在 Linux 和 Docker image 上 build 的成本（x11／wayland 的連結依賴）；handoff §5 已經指出，光是加 `remote` 那次都還沒量過。
- macOS 上 gpui headless 一定要在主執行緒跑：這是推論（remote_server 在 macOS 主機上就是這樣跑的），沒有實際測試過在非主執行緒跑會發生什麼事。
- `AppVersion::load` 的確切簽名：只看過 server.rs:662-667 的呼叫方式。
- `upload_directory` 在 SSH transport 上的所有 caller（WP8 要 grep 列出）。
- permessage-deflate 對 axum 0.6 WS 是否可用；要不要開。
- `web-release` profile 的 `debug_assertions` 真的是關的（web/Cargo.toml 的 profile 段沒有逐行讀過）。
- 單一 envelope 的實際最大尺寸（大檔案的 `OpenBufferResponse`／`CreateBufferForPeer` 的分塊方式）；1 GiB 的上限是保守的猜測。
- glibc 版本相容範圍（gnu build 的 remote_server 部署到較舊的遠端主機）。
- `SshConnectionOptions` 用 serde 預設產生的 JSON 形狀（例如 `SshConnectionHost` 的 externally tagged enum 會變成 `{"Hostname": "…"}`），是從 derive 推論的，沒有實際序列化過。
- fake-ssh 的 master 只 `touch` 一個 socket 檔案夠不夠：要看 `SshSocket` 之後的 `run_command` 會不會真的連 ControlPath（fake 的 `ssh` 會直接忽略 ControlPath，所以應該可以，但沒有驗證）。
- B 方案的否決理由之一「remote_server 沒有通用的寫檔／rename／watch proto」是用 grep 訊息名稱得出的，沒有逐一讀完所有 proto 檔。
- 連線失敗時 `open_remote_project` 會呼叫 `window.prompt(PromptLevel::Critical, …, &["Retry", "Cancel"])`（remote_connections.rs:334、395）；gpui_web 有沒有實作這種原生 prompt、會不會卡住，沒有驗證過。
- 伺服器 gpui App 的 `AppVersion`：remote_server 讀的是 `env!("ZED_PKG_VERSION")`（server.rs:663）；§4.2 用的是 `CARGO_PKG_VERSION`。因為 WP4 的 hook 路徑不會用到版本號，所以沒有影響；但如果 hook 沒設、退回 desktop 的流程，dev channel 反正會 bail（ssh.rs:897-903），所以也沒有影響。這是推論。
- CodeGraph：本 repo 沒有索引，這次研究全程用 Read／grep；呼叫者列表都是 grep 的結果，可能漏掉用 macro 或 trait dispatch 間接呼叫的地方。
