# Phase 5 — wasm `agent` + `edit_prediction`

Date: 2026-09-15. Branch `andy/web-version`, HEAD `23456e3e4e`. **No commit.** `web/` source was not edited. `README.md` `> [!IMPORTANT]` lines left in place. The five pre-dirty rustfmt files (`claude_sessions_panel.rs`, `session_store.rs`, `remote/claude_sessions.rs`, `remote_server/server.rs`, `tmux_sessions_panel.rs`) were not touched.

Specs: `docs/web-zed-plan.md` §2.2, §4.1, §5.5. cfg spelling: `target_family = "wasm"` only. **No `unsafe impl`.**

Only `crates/agent/src/thread.rs`, `crates/agent/src/db.rs`, and `crates/edit_prediction/src/edit_prediction.rs` were edited. Manifests already had `web-time` and `zstd`/`tempfile` under `not(wasm)` (Phase 3a).

Baseline lock: `cp Cargo.lock /tmp/lock-ag` before any edit. Lock unchanged (`cmp` identical, 503887 bytes).

## 0. codegraph (required first call)

```
codegraph explore "agent db thread zstd compression"
```

Returned 160 symbols across 7 files. Blast radius was `Thread` / `ThreadStore` / `AgentThread`, not the compression sites. Second call:

```
codegraph explore "zstd encode_all decode_all ThreadsDatabase compress decompress db.rs tempfile Instant"
```

Pinned `crates/agent/src/db.rs`. `ThreadsDatabase` (line 392) is called from `agent.rs` and `thread_store.rs`. `SharedThread::to_bytes` / `from_bytes` are clipboard (`agent_ui` `copy_thread_to_clipboard` / `load_thread_from_clipboard`), not the SQLite blob path. `sandboxed_terminal_temp_dir` is Seatbelt `$TMPDIR` for local macOS terminals (`agent.rs:3292` `create_terminal`); Linux/Windows already skip it.

Manifests are not in the graph. Confirmed by read:

- `crates/agent/Cargo.toml:86-89` already gates `zstd` and `tempfile` under `not(target_family = "wasm")`.
- `crates/edit_prediction/Cargo.toml:76-77` already gates `zstd` the same way. `web-time` is in `[dependencies]`.
- `crates/acp_thread/src/acp_thread.rs:53` already `use web_time::Instant;` (`RetryStatus.started_at`).

## 1. Baseline (this machine, re-run)

```
cd web && CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=../target/web-probe \
  cargo check -p agent -p edit_prediction --target wasm32-unknown-unknown --lib
```

Exit 101. **9 rustc errors** (the briefing said 11; that was the last known count). No other errors in these two crates.

| Crate | Count | Errors |
| --- | ---: | --- |
| `agent` | 7 | `tempfile` `thread.rs:1520` E0433; `Instant` `thread.rs:3034`, `:3349` E0308; `zstd` `db.rs:178`, `:183`, `:535`, `:660` E0433 |
| `edit_prediction` | 2 | `zstd` `edit_prediction.rs:2459`, `:3025` E0433 |

`error: could not compile 'edit_prediction' (lib) due to 2 previous errors`
`error: could not compile 'agent' (lib) due to 7 previous errors`

## 2. Three classes

### (1) `Instant` — import only

`thread.rs` imported `std::time::Instant`. `RetryStatus.started_at` is `web_time::Instant`. On native those are the same type; on wasm they are distinct (`E0308`).

Change: `use std::time::Duration;` + `use web_time::Instant;`. The two `Instant::now()` call sites are otherwise untouched. `web-time` was already in `crates/agent/Cargo.toml`.

`tests/mod.rs` still uses `std::time::Instant` (tests, not `--lib`, already on the allowlist).

### (2) `tempfile` — honest failure, not zed-web's fake path

`Thread::sandboxed_terminal_temp_dir` exists on every OS that is not Linux/Windows (Seatbelt needs a writable `$TMPDIR`; bwrap already has tmpfs `/tmp`). `wasm32-unknown-unknown` has `target_os = "unknown"`, so the function compiled and named `tempfile::Builder`.

Caller `NativeThreadEnvironment::create_terminal` (`agent.rs:3334`) already maps `Ok(Some(Err(error)))` to `Task::ready(Err(error))`.

zed-web returned `PathBuf::from("/tmp/zed-agent-terminal-wasm")` and `create_dir_all(...).ok()`. That is a path that does not exist in the browser, so later writes fail silently. **Not copied.**

Wasm arm:

```
anyhow::bail!("sandboxed terminal temp directories are not available in the browser");
```

Native arm is the previous `tempfile::Builder::new().prefix("zed-agent-terminal-")...keep()` sequence, byte-for-byte.

If a loaded thread already has `sandboxed_terminal_temp_dir: Some(...)`, the recreate-`create_dir_all` path still runs on wasm (no tempfile). That path is not a new stub; it will fail at `std::fs` if ever reached. New directories never get a fake path.

### (3) `zstd` — paired encode/decode

## 3. zstd data compatibility (the important section)

### What zed-web did

`git diff fecc3273ed..zedweb/zed-web -- crates/agent/src/db.rs`:

- **Client** compresses. Server `sql_rpc` stores the blob the client hands it. The server does **not** compress.
- Native: write `DataType::Zstd` via `zstd::encode_all`; read `Zstd` via `zstd::decode_all`.
- Wasm: write `DataType::Json` (uncompressed JSON bytes); read `Json` as UTF-8. A `Zstd` row on wasm tries `String::from_utf8` and fails with context `"zstd thread data not supported on wasm"`.
- `SharedThread::{to,from}_bytes` (clipboard) is the same split: native zstd, wasm raw JSON.

### Is the web agent db independent?

**Yes, and in this repo it is not even `sql_rpc`.**

| | Native desktop | zed-web wasm | this fork wasm |
| --- | --- | --- | --- |
| Store | `paths::data_dir()/threads/threads.db` (local rusqlite) | `Connection::open_file("threads.db")` → `/sql` shim → server rusqlite | `crates/sqlez/src/connection_wasm.rs`: handle only; every SQL op is `Err("SQLite is not supported on wasm")` |
| Who compresses | client (`save_thread_sync`) | client (JSON, no zstd) | client (JSON, no zstd) — same pairing; persistence still dies at `exec` |
| Shared with native `threads.db` | — | no (different machine / server file) | no (SQL never succeeds) |

`sqlez` has no `sql_rpc` / `wasm_rpc` edge (Phase 5 blockers-3). Wasm cannot read native's local zstd rows because it never opens that file and cannot run `SELECT`.

Therefore: **wasm will not see native-compressed DB blobs.** This is not the "stop and let the brain decide" case. Encode and decode stay paired per target:

- Native write Zstd ↔ native read Zstd (unchanged).
- Wasm write Json ↔ wasm read Json.
- Wasm read Zstd → `bail!("zstd-compressed thread data cannot be decoded on wasm")` (honest; zed-web's utf-8-of-zstd-bytes trick is not used).

`ThreadsDatabase::new` was **not** given zed-web's wasm `open_file("threads.db")` skip of `create_dir_all`. That is runtime-only, not one of the 9 errors, and this stub Connection ignores the path anyway.

### Clipboard caveat (not the DB)

`SharedThread` clipboard is also paired per target (native zstd, wasm JSON). Copy on desktop and paste in the browser (or the reverse) will fail to decode. That is the same choice zed-web made. It is not a shared SQLite. Brain can reopen if native↔web thread clipboard must work.

### `edit_prediction` HTTP body

Not a database. Native still `zstd::encode_all` + `Content-Encoding: zstd`. Wasm sends uncompressed JSON and **omits** that header (the body is not zstd; claiming `zstd` would be a lie). zed-web used `Content-Encoding: identity`; omitting the header is equivalent and smaller. Tests that decode zstd live in `edit_prediction_tests.rs` (`cfg(test)`), not `--lib`.

## 4. Native unchanged

Every wasm `cfg` has a `not(target_family = "wasm")` arm with the pre-existing statements:

- `SharedThread::to_bytes` / `from_bytes`: `zstd::encode_all(..., 3)` / `zstd::decode_all`.
- `save_thread_sync`: `DataType::Zstd` + `zstd::encode_all(json_data.as_bytes(), COMPRESSION_LEVEL)` at level 3.
- `deserialize_thread(DataType::Zstd)`: `zstd::decode_all` then UTF-8.
- `sandboxed_terminal_temp_dir`: `tempfile::Builder::new().prefix("zed-agent-terminal-").tempdir()?.keep()`.
- `Instant::now()` still called; the type is `web_time::Instant`, which on native is `std::time::Instant`.

`cargo check -p agent -p edit_prediction --lib` (host, default `target/`): exit 0, `Finished ... in 1m 08s`.

The requested `CARGO_TARGET_DIR=target/web-probe` native check hit `No space left on device` on `/Volumes/XDATA` (the `target` symlink; web-probe native `debug/` had grown to 24G beside 9.7G of wasm artifacts). That `debug/` directory was removed to free 23G; wasm32 artifacts were kept. Native verification used the existing `target/debug` graph on the same volume.

## 5. Verification

1. **wasm** (after the edit):

```
cd web && CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=../target/web-probe \
  cargo check -p agent -p edit_prediction --target wasm32-unknown-unknown --lib
```

Exit 0. `Finished 'dev' profile [unoptimized + debuginfo] target(s) in 5.63s`.
`Checking agent` / `Checking edit_prediction` — no rustc errors. Duplicate `atomics` warnings only, same as baseline.

2. **native**: `CARGO_BUILD_JOBS=2 cargo check -p agent -p edit_prediction --lib` — exit 0 (see §4).

3. **`./web/check-refusals.sh`**: `4 checks, 0 failures` (all four `ok`).

4. **rustfmt**: `rustfmt --check` on the three edited files — clean. The five pre-dirty files were not formatted.

5. **`Cargo.lock`**: `cmp /tmp/lock-ag Cargo.lock` identical (503887 bytes). No package added; `zstd` / `tempfile` / `web-time` were already in the lock.

## 6. Remaining errors

**These two crates, this probe: none.**

Workspace remainder was not measured (`./web/build.sh` is reserved for the parent). Runtime still broken for agent persistence on wasm: `sqlez` `Connection` stubs every SQL statement. That is pre-existing and outside the allowed crates.

## 7. Files

| File | Change |
| --- | --- |
| `crates/agent/src/thread.rs` | `web_time::Instant`; wasm `bail!` on new sandbox temp dir |
| `crates/agent/src/db.rs` | paired zstd/json cfg on `SharedThread` bytes, `save_thread_sync`, `deserialize_thread` |
| `crates/edit_prediction/src/edit_prediction.rs` | wasm uncompressed JSON body; native still zstd + header |
| `docs/phase5-agent.md` | this report |
