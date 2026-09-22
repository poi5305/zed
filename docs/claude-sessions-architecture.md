# Claude Sessions panel — architecture

Written 2026-09-18 for agents who will work on `crates/claude_sessions`, `crates/remote/src/claude_sessions.rs`, `crates/remote_server/src/headless_project.rs` and `crates/proto/proto/claude_sessions.proto`. The user guide is `docs/claude-sessions-setup.md`; the analysis that led here is `docs/claude-health-check.md`; the WP-by-WP build log is `docs/claude-health-check-progress.md`.

## 1. Data flow

```
 Claude Code CLI (tmux, claude.ai, phone — untouched)
   │ hooks (13 events)             │ statusLine (300 ms debounce)      │ MCP stdio
   ▼                               ▼                                   ▼
 ~/.claude/hooks/zed-claude-events.sh   ~/.claude/hooks/zed-claude-status.sh   ~/.claude/zed-channel/server.mjs
   │ append one JSONL line           │ tmp+rename raw JSON              │ child of the claude process
   ▼                                 ▼   (+ chains the user's own cmd)  ▼
 ~/.claude/zed-events/<session_id>.jsonl  ~/.claude/zed-status/<session_id>.json   ~/.claude/zed-channel/<claude_pid>/
                                                                                    ├ server.json   (heartbeat, features)
 ~/.claude/projects/*/<session_id>.jsonl  (transcript, unchanged)                  ├ inbox.jsonl   (server → Zed)
 ~/.claude/sessions/<pid>.json            (registry, unchanged)                    └ outbox/*.json (Zed → server)
   │ 250 ms tail        │ 250 ms tail        │ 1 s read      │ 1 s scan (+ `claude agents --json` every 3rd)   │ 250 ms tail / 1 s status
   └────────────────────┴────────────────────┴───────────────┴──────────────────────────────────────────────────┘
                                   SessionSource  (LocalSource = fs; RemoteSource = proto RPC → headless_project → same fs code)
                                                  │
                                   ClaudeSessionStore  (identity = sessionId; LiveState; StatusSnapshot; ChannelInboxEvent; polls)
                                                  │
                                   ClaudeSessionsPanel (entries, now row, turn summaries, cards, input)  ── writes ──► outbox/
```

Nothing reads a terminal (`capture-pane`, `TerminalView` attach, pane mirror are gone) and nothing writes to one (`send-keys`, `paste-buffer`, `PaneKey`, `Digit` are gone). The only spawned commands are `claude --bg --resume`, `claude respawn|stop <id>`, `claude agents --json`, and a user-visible terminal for `claude attach` / `tmux attach`.

## 2. File protocols

### 2.1 Events JSONL — `~/.claude/zed-events/<session_id>.jsonl`

| Field | Meaning |
|---|---|
| `received_at_ms` | ms since epoch, written by the hook script (python3, else `date +%s`000). Missing → parsed as 0. |
| `event` | the hook payload verbatim (always has `hook_event_name`, `session_id`, `permission_mode`, `cwd`, `transcript_path`; per-event fields such as `tool_use_id`, `tool_name`, `tool_input`, `message_id`, `index`, `final`, `delta`, `notification_type`). |

One line per event, appended with a single `write(2)` (parallel hooks must not interleave). A payload without a string `session_id`, or one containing `/`, `.`, `..`, writes nothing. The file is truncated when it passes 8 MiB; the tail restarts on shrink, like the transcript tail. Parsed by `live_state::parse_hook_event` into `HookEvent`, folded by `LiveState::apply` (rules 1–15 in `scratch/wp1-spec.txt` §D, mirrored by the blind tests).

### 2.2 Status JSON — `~/.claude/zed-status/<session_id>.json`

The CLI's statusLine stdin, unmodified: `session_id`, `model{id,display_name}`, `context_window{context_window_size,used_percentage,total_input_tokens,total_output_tokens,…}`, `cost{total_cost_usd,…}`, `effort{level}`, `rate_limits{five_hour,seven_day}{used_percentage,resets_at}`, `exceeds_200k_tokens`. Written tmp+rename. Read side refuses files over 1 MiB (`MAX_STATUS_FILE_BYTES`). `StatusSnapshot::parse` returns `None` only for a non-object; every field is individually optional. `~/.claude/zed-status/chained-command.txt` holds the user's displaced statusLine command; the wrapper runs it and prints its stdout.

### 2.3 Channel — `~/.claude/zed-channel/<claude_pid>/` (protocol 1; full text in `crates/remote/assets/zed-claude-channel/README.md`)

| File | Direction | Shape |
|---|---|---|
| `server.json` | server → Zed | `{pid, claude_pid, started_at_ms, heartbeat_at_ms, protocol:1, features:["message","permission","interrupt"]}`; rewritten every 5 s; removed on shutdown. Live = exists, not a symlink, `heartbeat_at_ms` within 20 s (`CHANNEL_HEARTBEAT_STALE_MS`). A missing or non-array `features` is treated as empty, never as "dead". |
| `outbox/<13-digit ms>-<4-digit seq>.json` | Zed → server | written as `<name>.<zed pid>.tmp` then renamed; a taken name is skipped for the next sequence (up to 10 000 per ms). Bodies: `{"kind":"message","content":"…","meta":{…}}` (≤ 4 MiB, non-empty), `{"kind":"permission","request_id":"…","behavior":"allow"\|"deny"}` (request_id 1..=16 `[a-z0-9]`), `{"kind":"interrupt","reason":"…"}` (reason ≤ 200 chars on the Zed side). Server processes in lexical order every 200 ms, deletes after acting; files dropped before `notifications/initialized` wait on disk. |
| `inbox.jsonl` | server → Zed | kinds `ready`, `permission_request{request_id,tool_name,description,input_preview}`, `permission_answered{request_id,behavior}`, `message_sent{outbox_file,content_chars}`, `interrupted{claude_pid,reason}`, `error{outbox_file?,reason}`, `closed{reason: stdin_end\|signal}`; every line carries `at_ms` (host clock; numeric strings accepted). Replaced wholesale past 4 MiB, so the tail handles shrink. Unknown kinds and unparseable lines become `ChannelInboxEvent::Unknown`, never dropped. |
| `server.log` | server | stderr mirror, ≤ 1 MiB. |

`error.reason` is an **open set**; readers must tolerate new values. Known today: `invalid_json`, `not_an_object`, `invalid_message`, `invalid_behavior`, `unknown_request_id`, `unknown_kind`, `not_a_file`, `too_large`, `unreadable`, `interrupt_throttled`, `interrupt_stale`, `interrupt_unavailable`, `interrupt_failed` (+`errno`). Server-side bounds: 512 open permissions / 30 min TTL (evicted verdicts fail closed); one stdin line ≤ 8 MiB; interrupt = one SIGINT to the startup `ppid` only, 3 s monotonic throttle, mtime > 10 s rejected; session dir and `outbox/` are 0700.

Permission correlation: the hook's `PermissionRequest` (has `tool_use_id`) and the inbox `permission_request` (has `request_id`) are paired by same `tool_name` and `|at_ms − since_ms| ≤ 5 s` (`CHANNEL_PERMISSION_MATCH_WINDOW_MS`), newest unanswered first. Two prompts of the same tool inside 5 s are still told apart only by order — a protocol limit, not a bug.

## 3. Module map

### `crates/claude_sessions/src/live_state.rs` (pure, no GPUI)
`HookEvent` + `parse_hook_event`; `LiveState { live_message, running_tools, pending_permission, pending_question, permission_mode, turn: Idle|Running, compacting, last_notification, last_event_at_ms, session_ended }` with `apply`, `note_transcript_assistant`, `note_tool_result`, `is_idle`; `StatusSnapshot::parse`; `timestamp_ms` (RFC3339 → ms); `ChannelInboxEvent` + `parse_channel_inbox_line`.

### `crates/claude_sessions/src/session_source.rs`
`trait SessionSource`: `list_sessions`, `tail_transcript`, `list_subagents`, `list_subagents_for_sessions` (one batched RPC), `tail_events`, `read_status`, `install_hooks`, `uninstall_hooks` (proto `UninstallClaudeHooks`; host runs `uninstall_zed_hooks`, never touches `~/.claude.json`), `hooks_installed` (polled by its own store task: once at start, then ≤ 1 per 30 s, plus once right after install/uninstall), `list_slash_commands` / `list_slash_commands_for(session_id)`, `list_session_files` / `list_session_files_in(session_id, …)`, `write_session_file`, `tail_subagent`, `read_file`, `read_attachment` (bounded), `channel_status`, `channel_send_message`, `channel_interrupt`, `channel_answer_permission`, `tail_channel_inbox`, `is_remote`, `list_agents`, `resume_session_in_background`, `run_claude_agent_command`. `LocalSource` runs the fs functions on the background executor; `RemoteSource` sends the proto below; `*_from_proto` converters live at the bottom.

### `crates/claude_sessions/src/session_store.rs`
`ClaudeSessionStore` keyed by `selected: Option<SessionKey /* sessionId */>`; `selected_process_id()` is derived. `SessionRow::{Live(LiveSession{session, background, agent_id, state, waiting_for}), Ended(EndedSession{…, ended_reason: ProcessGone|Cleared})}`, `ended` bounded to 20. Five poll loops, each with `with_timeout(REGISTRY_SCAN_TIMEOUT)` and its own `ErrorSource`: `spawn_registry_poll` (1 s; `list_agents` every 3rd tick; `scan_in_flight` + `_scan_watchdog` stop scans stacking) → `Poll`/`Agents`; `spawn_transcript_poll` (250 ms) → `Transcript`/`Subagents`; `spawn_events_poll` (250 ms) → `Events`; `spawn_status_poll` (1 s, also `hooks_installed`) → `Status`; `spawn_channel_poll` (inbox 250 ms, status every 4th tick) → `Channel`. `Send` is the sixth source. `set_visible(false)` slows every loop to 5 s (`HIDDEN_POLL_INTERVAL`); an ended row is tailed at the same slow rate. `StoreClock::stale_for` budgets: transcript 5 s, events 5 s, status 15 s, registry 5 s — only for loops that are actually due. `apply_registry_scan` implements rebind (same id, new pid), `/clear` (same pid, new id → old id `Cleared`), process gone (`ProcessGone`, transcript kept). Write side: `send_message` (refuses ended / channel not live), `answer_permission` (uses `open_permission_request_id`), `interrupt` (`can_interrupt` = live ∧ `features ∋ "interrupt"` ∧ `Turn::Running`).

### Pending sends (`PendingSends` in `claude_sessions_panel.rs`)
Rows are keyed by the sessionId they were sent to and are never dropped by a selection change: `entries_for(selected)` / `message_history_for(selected)` filter for rendering and Up-history; pairing (`pair_with_session`, `holds_text`, `pair_with_channel_for`) only considers the selected session; `/clear` rebinding comes from the store's `take_cleared_rebinds()` (pairs in scan order, consumed in `rebuild_entries`); in-flight rows whose session is neither live nor ended are marked failed (`the session ended before this message arrived`); finished rows (delivered or failed) are bounded to 50 with the oldest evicted first, in-flight rows are never evicted. A row disappears only through its close control, a successful delivery, or a Retry that succeeds.

### `crates/claude_sessions/src/claude_sessions_panel.rs` (≈19 k lines; line numbers drift)
Constants and `session_facts` (top) · `ClaudeSessionsPanel` struct, `Entry`/`EntryKind`/`EntryCache` · `collapse_turns` (per-turn `TurnSummary` post-pass) · `now_row` / `context_meter` / `attach_command` / `claude_ai_session_url` / `format_status_line` · `MessageRole` · selection & tabs (`reveal_session_in_pane`, `show_the_newest_of_another_conversation`) · `@` mentions and paste · `request_interrupt`, `open_sent_path` · dock: `render_session_section`, `render_live_session_row`, `render_ended_session_row` · tab: `render_agent_chips`, `render_live_message` (`DraggedLiveMessageDivider`), `render_now_row`, `render_transcript_section`, `can_send`/`send_message`/`dispatch_message`/`answer_permission`, `render_conversation_toolbar`, `render_slash_commands`, `render_file_matches`, `render_hook_install_note`, `render_pending_permission`, `render_question`, `render_input`, `render_status_line` · per-entry renderers (`render_entry`, `render_tool_use`, `render_todo_card`, `render_question_card`, `render_diff_card`, `render_dispatch_card`/`render_dispatch_report`, `render_persisted_output`, `render_sent_file`, `render_tool_output`) · `PendingSends` (pairs by outbox file → `message_sent`, then by transcript record) · `tool_target`, dispatch-command parsing (`command_words`, `dispatch_flag_value`, `dispatch_redirect_path`), `background_shells` · entry building (`build_entries`, `append_record`, `message_kind`, `message_role`, `peer_handback_from`, `bridge_url`, `block_kind`, `user_visible_text`, `rewrite_slash_commands`) · tests.

### `crates/remote/src/claude_sessions.rs` (shared by LocalSource and remote_server)
Registry & agents (`RegisteredSession`, `AgentListing`, `parse_claude_agents_json`, `output_within` with `kill_on_drop` + 4 MiB output cap, `run_claude_command`, `resume_session_in_background`, `run_claude_agent_command`, `list_claude_agents`, `liveness`, `visible_sessions`) · path boundaries (`single_path_component`, `claude_command_working_directory`, `claude_command_operand`, `session_working_directory`, `path_stays_inside`, `session_files_directory`, `slash_command_project_root`, `attachment_is_readable`) · tails (`TailState`/`TailProgress`, `read_transcript_tail`, `read_events_tail`, `read_session_status`, `split_complete_lines` with `TAIL_PENDING_CAP_BYTES` 4 MiB) · channel client (`ChannelStatus`, `channel_status`, `publish_outbox_file`, `channel_send_message`, `channel_interrupt`, `channel_answer_permission`, `read_channel_inbox_tail`, `channel_setup_commands`) · spend, subagents, slash commands, mentions, pasted files · hook installer (`EVENTS_HOOK_SOURCE`, `STATUS_HOOK_SOURCE`, `CHANNEL_SERVER_SOURCE` = `include_str!` of `assets/zed-claude-channel/server.mjs`, `HOOK_EVENTS`, `install_zed_hooks`, `uninstall_zed_hooks`, `zed_hooks_installed`, `HookInstallOutcome`) · subagent conversation cache (`read_session_conversation`, incremental by whole lines, lossy UTF-8, pruned per scan) · tests.

### `crates/remote_server/src/headless_project.rs`
`handle_*` per proto message; each validates before touching the fs. `ListClaudeSessions` fills `liveness_unavailable_reason` on hosts without process start times (non-unix) so the panel can show a note instead of an empty list. `WriteClaudeSessionFile` names pasted files `pasted-<ms>-<6 hex of SHA-256>.<ext>` and prunes direct regular-file children of `~/.claude/zed-pasted/` older than 7 days (symlink_metadata, no descent, no symlinks) (session ids as single path components, cwd via `claude_command_working_directory`, files via `session_files_directory`, pids ≠ 0, content ≤ 4 MiB).

### `crates/proto/proto/claude_sessions.proto` (envelope 501–541; 507 reserved)
`ListClaudeSessions` (501/502; `ClaudeSession` carries `bridge_session_id`), `TailClaudeTranscript` (503/504), `ReadClaudeFile` (505/506), `ListClaudeSubagents` (508/509), `TailClaudeEvents` (510/511), `ReadClaudeStatus` (512/513), `InstallClaudeHooks` (514/515), `ListClaudeSlashCommands` (516/517), `ListClaudeSessionFiles` (518/519, carries `session_id`), `WriteClaudeSessionFile` (520/521), `ClaudeHooksInstalled` (522/523), `ClaudeChannelStatus` (524/525), `ClaudeChannelSend` (526/527), `ClaudeChannelAnswerPermission` (528/529), `TailClaudeChannelInbox` (530/531), `ClaudeChannelInterrupt` (532/533), `ListClaudeSubagentsForSessions` (534/535), `ListClaudeAgents` (536/537), `ResumeClaudeSession` (538/539), `RunClaudeAgentCommand` (540/541). Numbers 510–515 were reused from the deleted pane/question messages rather than reserved (accepted).

## 4. Invariants (do not break)

1. **Identity is `sessionId`.** pid, tmux target and `bridgeSessionId` are attributes. A new pid for the same id is a rebind, never a reset; a vanished pid is an ended row that keeps its transcript.
2. **No terminal reads or writes anywhere.** No `capture-pane`, no `send-keys`, no hidden tmux client. `is_zed_mirror_session` survives only as a list filter.
3. **Every RPC/poll has a timeout and its own `ErrorSource`;** a loop clears only its own source, and no loop ever stops (hidden = slower).
4. **Untrusted JSON is never indexed.** Transcript records, hook payloads, status files, inbox lines and `agents --json` are read with `get`/`as_*`, degrade to `Unknown`/JSON, never `unwrap`/`[…]`.
5. **Every spawned command operand goes through `claude_command_operand`** (alphanumeric, `-`, `_`, ≤ 64 bytes, no leading `-`), every cwd through `claude_command_working_directory` (absolute, exists, no NUL), every child through `output_within` (kill on drop, 4 MiB cap).
6. **Every opened path or URL goes through a whitelist:** files via `attachment_is_readable` (session cwd or `/tmp`, `/private/tmp`; remote always downloads first), URLs via `bridge_url` (`https://claude.ai/` prefix, no whitespace/control chars) — `bridge_status` records and Open in claude.ai share it.
7. **Hook scripts print nothing and exit 0** on every path; the installer parses settings before writing anything and keeps one rolling backup.
8. **Channel readers treat `error.reason` and inbox `kind` as open sets.**

## 5. Test layout

- **Blind tests** (`crates/claude_sessions/src/blind_live_state_tests.rs`, `blind_registry_tests.rs`, `blind_subagent_tests.rs`, `blind_transcript_tests.rs`) were written from the frozen spec without reading the implementation and are held out: implementers and fixers must not read, run in isolation, or edit them. They are wired as `#[cfg(test)] mod` in `claude_sessions.rs` and run with the crate. `blind_input_tests.rs` was deleted with the send-keys path it tested.
- **Regression tests from the review rounds** live beside the code they guard: `live_state.rs` `mod tests`; `session_store.rs` `mod tests` (FakeSource writes real files under a temp home); `claude_sessions_panel.rs` `mod tests` (ScriptedSource, PromptSource); `crates/remote/src/claude_sessions.rs` `mod tests`, `hook_freshness_tests`, `mention_tests`, `slash_command_skill_tests`, `attachment_boundary_tests`, `session_lifecycle_tests` (hook scripts are executed with `sh` under a temp `HOME`); `headless_project.rs` `mod tests`.
- **Channel server**: `crates/remote/assets/zed-claude-channel/server.test.mjs` (`node --test`, 34 cases, spawns the server as a child). Do not run its manual smoke test from an agent.
- Gates: `cargo test -p claude_sessions -p remote` (460 + 180 at hand-off), `./script/clippy -p claude_sessions -p remote -p remote_server`, `node --test server.test.mjs`.

## 6. Accepted `wontfix` (from the review reports)

- WP1 F05 — `install_zed_hooks` writes scripts before failing on a non-object `hooks`; rerun completes it.
- WP1 F06 — uninstall drops the chained command if `statusLine` was hand-edited into a non-object.
- WP3a F10 — outbox entries are read through symlinks (needs write access to a 0700 dir anyway).
- WP3a F11 — `.bad` quarantine files are never pruned (kept as evidence).
- WP3a F12 — `content_chars` counts UTF-16 units.
- WP3a F14 — `initialize` with `id: null` is treated as a notification (never sent by Claude Code).
- WP3a F15 — the 512-open-permission cap evicts oldest and fails closed.
- WP3b-7 — `request_id` accepted only as 1..=16 `[a-z0-9]`.
- WP3b-8 — `can_send` ignored `session_ended` (closed later by WP5's ended rows).
- WP3b-9 — envelope numbers 510–515 reused, not reserved.
- WP3b-10 — `outbox/` created by Zed inherits the umask (the parent is 0700).
- WP3b-11 — a `.tmp` whose rename fails is left behind.
- WP3b residual — two same-tool prompts within 5 s are order-matched; the free-name check and rename are not atomic across two Zed processes.
- WP3c I05 — an `Invalid Date` mtime would fail open (unreachable from any input).
- WP3c I06 — the throttle test's SIGINT count alone is not proof (inbox assertions are).
- WP4a F11 — dispatch report row wording/boundary (superseded by WP4b: it now loads via `read_attachment`).
- WP4a F14 — attachments before the first user message fold into that turn's Context.
- WP4a F15 — `parse_todos` is all-or-nothing on an unknown status (spec'd degradation).
- WP4b W1 — Saying's 30 s hide compares host `updated_at_ms` to the local clock.
- WP4b W2 — toolbar cost prefers `status.total_cost_usd` even when the status is stale.
- WP5R1-8 — after a > 4 MiB partial line is dropped, its tail may surface as one unparseable line.
- WP5R1-9 — a selected ended row is redrawn once a second.
- WP5R1-10 — `agents --json` `pid` printed as a string is not parsed.
- WP7R1 W1 — store-initiated selection changes (`/clear`, `dismiss_ended`) do not clear the `@`/slash menu state (they never cross a cwd).
- WP7R1 W2 — Retry re-sends the trimmed text the failed row shows, not the raw editor text.
- WP7R1 W3 — Escape on the panel context is consumed even when no menu is open (wanted by HC-32).
- WP7R1 W4 — the paste error line is drawn only while the input is expanded.
- WP7R1 W5 — a pasted file is named by the client clock and pruned by the host clock.
- WP7R2 W1 — hook ownership is `command.contains("zed-claude-events.sh")`.
- WP7R2 W2 — a paste whose exact name was pre-planted as a hard link would truncate the shared inode.
- WP7bR1 F2/F3 — the transcript-walk guard and `/clear` pair chaining were later closed by WP7c/WP7d.
- WP7cR1 R1-4 — before the first registry scan every session looks gone, but no pending row can exist yet (`can_send` needs a live channel).
- WP7cR1 — a session pushed past the 20-row `ended` bound makes its in-flight rows fail (consistent with the G1 ruling).
- WP7dR1 REBOUND-PRECEDENCE — when one scan both rebinds the selected sessionId onto another pid (`--resume` in a second terminal) and clears the reader's own pid, the rebind branch wins: the follow target and a stale Send error lag by one scan (≤ 1 s) and self-heal.
- WP7dR1 — `take_cleared_rebind()` (Option) has no production caller; kept so two store tests stay unmodified.
