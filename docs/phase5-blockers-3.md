# Phase 5c — wasm workspace compile blockers (`fs` `sqlez`)

Date: 2026-09-15. Branch `andy/web-version`. **No commit.** `web/` was not edited. `README.md` `> [!IMPORTANT]` lines left in place. The five pre-dirty rustfmt files (`claude_sessions_panel.rs`, `session_store.rs`, `remote/claude_sessions.rs`, `remote_server/server.rs`, `tmux_sessions_panel.rs`) were not touched.

Specs: `docs/web-zed-plan.md` §2.2, §4.1, §5.1, §9. cfg spelling: `target_family = "wasm"` only.
Reference: `git diff fecc3273ed32643c2ea1b04a74c8780e2c9ffaf8..zedweb/zed-web -- crates/fs crates/sqlez crates/sqlez_macros`.

Baseline: `cp Cargo.lock /tmp/lock-before-p5c` before any edit. Lock unchanged (`cmp` identical, 502232 bytes).

This round wrote only under `crates/`. Manifests (`crates/fs/Cargo.toml`, `crates/sqlez/Cargo.toml`) were already gated in Phase 3a and were **not** retouched. `crates/fs/src/fs_watcher.rs` was already dirty from Phase 3b Instant and was **not** retouched.

## 0. codegraph (required first call)

```
codegraph explore "notify trash tempfile libsqlite3_sys"
```

| Symbol | File:line | Notes |
| --- | --- | --- |
| `Fs::trash` | `crates/fs/src/fs.rs:123` | Trait method. Native `RealFs` calls `trash::delete_with_info`. |
| `TrashedEntry` / `From<trash::TrashItem>` | `crates/fs/src/fs.rs` | Needs the `trash` crate. |
| `notify` | `crates/fs/src/fs_watcher.rs` | Whole module is the watcher. Also `FakeWatches` under `test-support`. |
| `tempfile` | `crates/fs/src/fs.rs` (`NamedTempFile` / `TempDir`) | `RealFs::atomic_write` / `is_case_sensitive`. |
| `libsqlite3_sys` | `crates/sqlez/src/{connection,statement,migrations}.rs` | Direct FFI. Not in codegraph as a first-party symbol. |

Blast radius of `Fs::trash` includes `web/crates/wasm_remote/src/fs.rs` (`RemoteFs` over RPC). That crate was **not** edited.

Manifests are not in the graph; `crates/fs/Cargo.toml` / `crates/sqlez/Cargo.toml` were read with grep. Phase 3a already moved `notify` `trash` `tempfile` `async-tar` `libc` `is_executable` and `libsqlite3-sys` `sqlformat` to `[target.'cfg(not(target_family = "wasm"))'.dependencies]`.

## 1. Baseline (this machine, re-run)

```
cd web && CARGO_TARGET_DIR=../target/web-probe cargo check --workspace --target wasm32-unknown-unknown
```

Exit 101. 61 `error[` plus two `pet-fs` E0308 that cargo was already compiling in parallel:

| Crate | Count | Errors |
| --- | ---: | --- |
| `fs` | 56 | `notify` (module + `fs_watcher.rs` uses), `trash`, `tempfile`, `async_tar`, missing `FileHandle::current_path` (no unix/windows arm), `inode`/`is_fifo`/`is_executable` unbound in `RealFs::metadata` |
| `sqlez` | 3 | `libsqlite3_sys` in `connection.rs:10`, `migrations.rs:11`, `statement.rs:6` |
| `pet-fs` (third-party, already present) | 2 | E0308 in `python-environment-tools` `path.rs:54` and `:133` |

The prompt’s “61 / only fs and sqlez” is the first-party set. `pet-fs` was already failing in the same run and is **not** this round.

## 2. What `zed-web` actually did

### `fs` (`fecc3273..zedweb/zed-web`)

Gates `mod fs_watcher`, `RealFs`, `FakeFs`, trash conversions, `async_tar` / `tempfile` imports. Adds `WasmFs` whose `create_dir` / `create_file` / `save` / `write` / `atomic_write` / `load_bytes` return **`Ok` / empty** with no I/O. Changes `Fs::global` on wasm to `Arc::new(WasmFs)` instead of reading `GlobalFs`.

**Not taken:** `WasmFs`, the `Fs::global` swap, silent `Ok(())`. Those violate the honesty rule. Browser I/O is `web/crates/wasm_remote::RemoteFs` (already implements `Fs` over RPC, including `trash`). This crate only has to compile.

**Taken (adapted):** cfg-out native-only modules/types/imports so the `Fs` trait still exists on wasm. `extract_tar_file` is `not(wasm)` on the trait — `RemoteFs` already skips that method, matching zed-web.

### `sqlez`

zed-web adds `connection_wasm.rs` / `statement_wasm.rs` / `migrations_wasm.rs` / `remote_sql.rs` and a wasm dependency table:

```
wasm_rpc.workspace = true
wasm_thread = { version = "0.3", features = ["es_modules"] }
web-sys / wasm-bindgen / serde / base64 / uuid js
```

`remote_sql.rs` is an 800-line sync XHR + SharedArrayBuffer bridge to `zed_web_server` `sql_rpc`. **That requires `crates/sqlez` (root workspace) to depend on `web/crates/wasm_rpc`.** Phase 3a already refused those deps. This round does not add them.

**Architecture (not implemented):** wiring sqlite over RPC belongs in a later decision. A stub that compiles, and fails on every SQL operation, is what this layer can do without reversing the workspace split.

`sqlez_macros` was unchanged on zed-web for this purpose (proc-macro, host `sqlformat`). Left alone.

## 3. Per-crate how it was fixed

### `fs`

Native files/bodies kept. wasm stops naming crates that Phase 3a removed from the graph.

| Site | Native still there? | wasm |
| --- | --- | --- |
| `pub mod fs_watcher` | yes, `not(wasm)` | module absent — that is the 38 `notify` errors |
| `mod git_clone_progress` | yes, `not(wasm)` | unused without `RealFs` |
| `use async_tar` / `tempfile` / `trash` conversions | yes | gated |
| `Fs::extract_tar_file` | yes, `not(wasm)` on the trait | method does not exist (matches `RemoteFs`) |
| `RealFs`, `RealWatcher`, `impl FileHandle for std::fs::File`, `impl Fs for RealFs` | yes | not compiled. No `WasmFs`. |
| `read_dir_entries` `not(unix)` | windows native still | `all(not(unix), not(wasm))` so wasm does not compile a leftover `std::fs::read_dir` helper |
| `FakeFs` | yes, still `feature = "test-support"` only | `--lib` does not build it. `extract_tar_file` inside FakeFs also `not(wasm)` so the trait and impl stay aligned |
| `Fs::global` | original `GlobalFs::global(cx)` | **unchanged** (zed-web’s `WasmFs` return refused) |

`TrashedEntry` / `JobTracker` still exist in the wasm object file and warn `dead_code`. Native `RealFs` still uses them. Not deleted.

### `sqlez`

`connection.rs` / `statement.rs` / `migrations.rs` have **zero edits**. `lib.rs` only adds cfg routing:

```
not(wasm) → connection / statement / migrations
wasm     → connection_wasm as connection, etc.
```

New files: `connection_wasm.rs`, `statement_wasm.rs`, `migrations_wasm.rs`. No `wasm_rpc`. No `remote_sql`.

`thread_safe_connection.rs`, `typed_statements.rs`, `savepoint.rs`, `bindable.rs`, `domain.rs` untouched — they talk to `Connection` / `Statement` through the same method names.

## 4. Native behaviour — proof it did not change

Every new arm is `#[cfg(target_family = "wasm")]` or a wrap of an existing item with `not(target_family = "wasm")`. Inner native statements are the original ones.

| Site | Native still runs |
| --- | --- |
| `fs_watcher` + `notify` | module compiled; `RealFs::watch` still calls `fs_watcher::watch` |
| `trash::delete_with_info` / `restore_all` | inside `impl Fs for RealFs`, which is compiled |
| `tempfile::{NamedTempFile, TempDir}` | `atomic_write` / `is_case_sensitive` |
| `async_tar::Archive` | `RealFs::extract_tar_file` and FakeFs (test-support) |
| `libsqlite3_sys` FFI | original `connection.rs` / `statement.rs` / `migrations.rs` |
| `sqlformat` in migrations | original `migrations.rs` |
| `ThreadSafeConnection` write queue / `std::thread` | original file |

Compiler:

```
CARGO_TARGET_DIR=target/web-probe cargo check -p fs -p sqlez -p db --lib
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.63s
```

## 5. Stub honesty

### `fs`

No wasm `Fs` impl is added. There is nothing to return fake success from. A wasm binary that needs a filesystem uses `RemoteFs` (already in `web/`, not this round). Calling `RealFs::new` on wasm is a compile error.

`extract_tar_file` missing on the wasm trait is a compile-time absence, not a silent unpack.

### `sqlez` — what sqlite **is** on wasm

It is a **compile stub**, not RPC. There is no server round-trip. zed-web’s `sql_rpc` / SharedArrayBuffer path is **not** here.

| Operation | wasm result |
| --- | --- |
| `Connection::open_file` / `open_memory` | Constructs a **handle** (no sqlite). Signature is `-> Self`, same as native. Subsequent SQL fails. |
| `Statement::prepare` | `Err`: `"SQLite is not supported on wasm"` |
| `exec` / `exec_bound` / `select*` (typed_statements) | `Err` via `prepare` |
| `migrate` | `Err` same message |
| `backup_main` / `backup_main_to` | `Err` (zed-web returned `Ok(())` — refused) |
| bind / column / step / rows | `Err` same message |
| `sql_has_syntax_error` | `Some(("SQLite is not supported on wasm", 0))` — fail-closed. `None` would claim “valid SQL”. Signature is `Option`, not `Result`. |
| `reset` | no-op (nothing prepared) |
| `parameter_count` | `0` — unreachable: `prepare` always fails, so no `Statement` exists |
| `with_write` / `can_write` / `persistent` | same control-flow flags as native; not I/O |
| `ThreadSafeConnection::build` | `Err` when `exec` / `migrate` run |
| `ThreadSafeConnection` deref with `connection_initialize_query` | **panics**, same as native when initialize SQL fails (`create_connection` was not changed) |

First SQL use fails. Construction of a `Connection` handle does not mean a database exists.

## 6. Acceptance

### 1. wasm workspace check — `fs` and `sqlez` gone

```
cd web && CARGO_TARGET_DIR=../target/web-probe cargo check --workspace --target wasm32-unknown-unknown
```

`Checking fs` succeeds (4 `dead_code` warnings on `TrashedEntry` / `JobTracker`). `sqlez` succeeds (1 `dead_code` on unused `last_error`, kept for API parity with native `Connection`).

**New first error (not fixed this round):**

```
error[E0308]: mismatched types
  --> ~/.cargo/git/checkouts/python-environment-tools-…/bb8e046/crates/pet-fs/src/path.rs:54:61
   pub fn strip_trailing_separator<P: AsRef<Path>>(path: P) -> PathBuf
   expected `PathBuf`, found `()`

error[E0308]: mismatched types
  --> …/pet-fs/src/path.rs:133:46
   pub fn norm_case<P: AsRef<Path>>(path: P) -> PathBuf
   expected `PathBuf`, found `()`

error: could not compile `pet-fs` (lib) due to 2 previous errors
```

This is a third-party git dep (`microsoft/python-environment-tools`), not under `crates/`. The functions have `#[cfg(windows)]` / `#[cfg(not(windows))]` bodies and no wasm arm, so the wasm build hits an empty function. It was already compiling in the baseline run; it is now the **blocking** crate because `fs` / `sqlez` no longer stop the graph. Out of scope.

### 2. native

```
CARGO_TARGET_DIR=target/web-probe cargo check -p fs -p sqlez -p db --lib
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.63s
```

### 3. refusals

```
./web/check-refusals.sh
ok   §5.3.1 RELEASE_CHANNEL is dev
ok   §5.3.2 terminal Shift+Click selection extension exists
ok   §5.3.3 recent_projects open_local_project PathPromptOptions.files is true
ok   §5.3.4 remote_server MultiWrite::flush uses send_blocking
4 checks, 0 failures
```

### 4. rustfmt

Repo-root `rustfmt.toml`, edition 2024, `--check` on:

`crates/fs/src/fs.rs` `crates/sqlez/src/lib.rs` `crates/sqlez/src/connection_wasm.rs` `crates/sqlez/src/statement_wasm.rs` `crates/sqlez/src/migrations_wasm.rs`

`RUSTFMT_OK`. The five pre-dirty files were not formatted.

### 5. `Cargo.lock`

```
cmp /tmp/lock-before-p5c Cargo.lock   # identical
wc -c: 502232  502232
diff: empty
```

No package version or source change. No `web/` path appeared. Native graph for `fs` / `sqlez` / `db` still resolves `libsqlite3-sys`, `notify`, `trash`, `tempfile` as before.

## 7. Files this round

| File | Change |
| --- | --- |
| `crates/fs/src/fs.rs` | cfg-gate native-only modules, imports, `RealFs`, trash conversions, `extract_tar_file` |
| `crates/sqlez/src/lib.rs` | cfg-route connection / statement / migrations |
| `crates/sqlez/src/connection_wasm.rs` | **new** — handle + honest `Err` |
| `crates/sqlez/src/statement_wasm.rs` | **new** — `prepare` and all SQL methods `Err` |
| `crates/sqlez/src/migrations_wasm.rs` | **new** — `migrate` `Err` |

Not edited: `web/**`, `Cargo.lock`, `Cargo.toml`, `crates/fs/Cargo.toml`, `crates/sqlez/Cargo.toml`, `crates/fs/src/fs_watcher.rs`, `crates/sqlez_macros/**`.

## 8. Stop / architecture note (for the next round)

zed-web’s working sqlite on wasm is `crates/sqlez` → `wasm_rpc` → server `sql_rpc`. That edge is illegal in this repo’s two-workspace split (`crates/sqlez` cannot depend on `web/`). The stub will fail at first `prepare` / `migrate` / `exec`. Making the editor’s DB actually work in the browser needs a decision on where the RPC client lives — not a silent success inside `sqlez`.
