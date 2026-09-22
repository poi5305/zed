# zed-claude channel MCP server

A dependency-free Node.js MCP server that Claude Code loads as a **channel** (research preview). Zed's Claude session panel writes messages and permission verdicts into files; this process turns them into `notifications/claude/channel` (and permission) JSON-RPC on stdout. The same files work when the Claude session is on a remote host, because Zed's remote server can read and write them over its own RPC.

Zed installs `server.mjs` to `~/.claude/zed-channel/server.mjs` together with its hooks (the "Install hooks" button in the Claude Sessions panel).

## Registration

Zed's panel offers a "Copy setup commands" button that fills in the absolute path for the two manual setup commands:

```sh
claude mcp add --scope user zed-claude -- node "$HOME/.claude/zed-channel/server.mjs"
export CLAUDE_EXTRA_ARGS='--dangerously-load-development-channels server:zed-claude'   # picked up by the user's claude() shell function; or pass the flag directly
```

The session loads this server only when `--dangerously-load-development-channels server:zed-claude` is present. Do not point it at `~/.claude/sessions/<pid>.json` or any `.key` file; Claude Code spawns this process as a child, and `process.ppid` is the Claude pid Zed already maps to a session.

## File protocol

Environment variables:
- `ZED_CLAUDE_CHANNEL_ROOT`: root directory (default `~/.claude/zed-channel`).
- `ZED_CLAUDE_CHANNEL_HEARTBEAT_MS`: heartbeat interval for `server.json` (default 5000; tests only).

Each Claude session gets `<root>/<ppid>/`, created by the server at startup (`ppid` is the Claude process). The session directory and `outbox/` are created with mode 0700; readers must run as the same POSIX user as `claude`.

| Path | Direction | Role |
| --- | --- | --- |
| `server.json` | server → Zed | Written at start and every 5 s (or `ZED_CLAUDE_CHANNEL_HEARTBEAT_MS`): `{"pid","claude_pid","started_at_ms","heartbeat_at_ms","protocol":1,"features":["message","permission","interrupt"]}`. Atomic (`*.tmp` + rename). Removed on shutdown. Liveness as Zed reads it: `server.json` exists and `heartbeat_at_ms` is within the last 20 s. Feature-detect via `features`. |
| `outbox/` | Zed → server | Zed writes `outbox/<name>.tmp` then renames to `<zero-padded ms>-<seq>.json`. Max 4 MiB per file (`too_large`). The server ignores `*.tmp` and `*.bad`, polls every 200 ms, processes remaining files in lexical order, and deletes each file after it has been processed (stdout JSON-RPC for `message`/`permission`, one SIGINT for `interrupt`). |
| `outbox/*.json` body `{"kind":"message","content":"…","meta":{…}}` | Zed → Claude | Becomes `notifications/claude/channel`. The server sets `meta.from` to `"zed"` and `meta.sent_at_ms` to a millisecond string; drops meta keys that are not `^[A-Za-z0-9_]+$`; stringifies non-string values. |
| `outbox/*.json` body `{"kind":"permission","request_id":"…","behavior":"allow"\|"deny"}` | Zed → Claude | Becomes `notifications/claude/channel/permission` if `request_id` is an open relay. Otherwise rejected: logged, deleted, and recorded as an inbox `error` (`unknown_request_id` or `invalid_behavior`). |
| `outbox/*.json` body `{"kind":"interrupt","reason":"…"}` | Zed → Claude | Optional `reason` (string) is copied onto the inbox line, truncated to 512 Unicode code points (never mid-surrogate). Sends **one** `SIGINT` to `process.ppid` (the Claude process) and appends `interrupted`. Never any other signal, never any other pid, never more than once per outbox file. |
| `inbox.jsonl` | server → Zed | Append-only events, replaced wholesale beyond 4 MiB. Kinds: `ready`, `permission_request`, `permission_answered`, `message_sent`, `interrupted`, `error`, `closed`. |
| `server.log` | server | Human-readable stderr mirror, replaced wholesale beyond 1 MiB. |

### Outbox handling and errors

- Outbox entries that cannot be read, are not regular files, or are too large (> 4 MiB), as well as invalid JSON, are renamed to `<name>.bad` and left in place.
- Malformed payloads (`not_an_object`, `invalid_message`, `invalid_behavior`, `unknown_request_id`, `unknown_kind`, `interrupt_throttled`, `interrupt_stale`, `interrupt_unavailable`, `interrupt_failed`) are deleted.
- Outbox entries that cannot be deleted after sending are parked (never re-sent); parking is by name **and** file identity so a later file reusing the same name is still processed.
- `error.reason` is an OPEN set. Currently: `invalid_json`, `not_an_object`, `invalid_message`, `invalid_behavior`, `unknown_request_id`, `unknown_kind`, `not_a_file`, `too_large`, `unreadable`, `interrupt_throttled`, `interrupt_stale`, `interrupt_unavailable`, `interrupt_failed`. Readers must tolerate new values. `interrupt_failed` also carries `errno` (the Node `error.code` string, e.g. `ESRCH` / `EPERM`).
- Outbox files dropped before Claude sends `notifications/initialized` stay on disk and are not processed until that notification arrives, then flushed in lexical order. An `interrupt` whose file mtime is older than 10 s at that moment is rejected as `interrupt_stale` and does not send a signal.

### Permissions, streams, and limits

- At most 512 open permission requests are kept in memory (oldest evicted; a verdict for an evicted or expired id is rejected — fail closed). Open requests expire after 30 minutes.
- One stdin JSON-RPC line at most 8 MiB (overlong lines are dropped and logged, the stream resyncs at the next newline).
- A JSON-RPC message with `"id": null` or no `id` is treated as a notification (this is what makes `permission_request` with an explicit null id work); `initialize` must carry a real id.
- A broken stdout pipe does not kill the server; it logs and keeps running until stdin ends (`closed.reason` = `stdin_end`). `closed.reason` values remain `stdin_end` | `signal`.
- Interrupt: exactly one `SIGINT` per accepted outbox file, only to `process.ppid`. If `ppid` is `0` or `1` (orphaned / container init), or is no longer the `claude_pid` this session was opened for (the Claude process died and the server was reparented), reject `interrupt_unavailable` and send nothing. A second interrupt fewer than 3000 ms after the last SIGINT this server sent is `interrupt_throttled`; the 3000 ms are measured on a monotonic clock, so a wall-clock correction can neither disarm the throttle nor jam it shut. Kill failures (`ESRCH`, `EPERM`) are `interrupt_failed` (with `errno`) and do not exit the server. `interrupted` is `{"kind":"interrupted","at_ms":…,"claude_pid":<ppid>,"reason":<string|null>}`, where `reason` is at most 512 Unicode code points.

## Manual smoke test

1. Register the server as above (once).
2. In a terminal, start Claude Code:

   ```sh
   claude --dangerously-load-development-channels server:zed-claude
   ```

3. Note that process's pid (`echo $$` is the shell, not Claude; use the pid of the `claude` process, for example from Activity Monitor or `pgrep -n claude`).
4. Drop a message (create `outbox/` first if the server has not started yet; normally the server creates it):

   ```sh
   echo '{"kind":"message","content":"say hi"}' > ~/.claude/zed-channel/<pid>/outbox/1.json
   ```

5. The model should answer in that terminal as if you had typed `say hi`. `~/.claude/zed-channel/<pid>/inbox.jsonl` should grow (`ready`, then `message_sent`).

Do not run this smoke test from automated agents that must not start a real `claude` session or write under `~/.claude/`.

## Known limits

- Channel support is a research preview. `--dangerously-load-development-channels` syntax may change.
- Claude Code does not relay `AskUserQuestion` through the permission channel. Answers to those prompts should be sent as ordinary channel messages.
- The development flag shows a confirmation prompt on startup. Whether that prompt appears on every start is unverified.
