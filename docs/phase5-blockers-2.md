# Phase 5b — next wasm workspace compile blockers (`askpass` `fuzzy` `lsp` `rpc`)

Date: 2026-09-15. Branch `andy/web-version`. **No commit.** `web/` was not edited. `README.md` `> [!IMPORTANT]` lines left in place. The five pre-dirty rustfmt files (`claude_sessions_panel.rs`, `session_store.rs`, `remote/claude_sessions.rs`, `remote_server/server.rs`, `tmux_sessions_panel.rs`) were not touched.

Specs: `docs/web-zed-plan.md` §5.1, §5.5, §9, §9.1. cfg spelling: `target_family = "wasm"` only.
Reference: `git diff fecc3273ed32643c2ea1b04a74c8780e2c9ffaf8..zedweb/zed-web -- crates/askpass crates/fuzzy crates/lsp crates/rpc crates/util crates/gpui`.

Baseline: `cp Cargo.lock /tmp/lock-before-p5b` before any edit.

## 0. codegraph (required first call)

```
codegraph explore "BackgroundExecutor scoped try_shell_safe make_file_executable send_blocking PathExt ShellKind new_command"
```

| Symbol | File:line | Return / shape | Notes |
| --- | --- | --- | --- |
| `BackgroundExecutor::scoped` | `crates/gpui/src/executor.rs:156` | `async fn scoped<'scope, F>(&self, scheduler: F)` where `F: FnOnce(&mut Scope<'scope>)` | Was `#[cfg(not(target_family = "wasm"))]`. Callers: `fuzzy/src/paths.rs`, `fuzzy/src/strings.rs`. |
| `PathExt::try_shell_safe` | `crates/util/src/paths.rs:194` | `fn try_shell_safe(&self, shell_kind: ShellKind) -> anyhow::Result<String>` | Was `not(wasm)`. Quotes via `ShellKind::try_quote`. |
| `make_file_executable` | `crates/util/src/fs.rs` | `async fn(&Path) -> io::Result<()>` | Unix: `chmod 0o755`. `not(unix)`: `Ok(())`. Askpass caller at `askpass.rs`. |
| `new_command` | `crates/util/src/command.rs:16` | `fn new_command(program) -> Command` | 25 callers; LSP uses `util::command::{Child, Stdio}` + `new_command`. |
| `send_blocking` | `async_channel::Sender` | not in this repo | LSP `notify_internal` at `lsp.rs:1684`. Missing on wasm (`async_channel` has no blocking API there). |

Blast radius of `new_command` / `ShellKind` is large (dap_adapters, git, languages, …). Those crates were **not** edited. Only the util modules they import were made to exist on wasm.

## 1. Baseline (this machine, re-run)

```
cd web && CARGO_TARGET_DIR=../target/web-probe cargo check --workspace --target wasm32-unknown-unknown
```

Exit 101. rustc errors in the four named crates (plus `task`, already visible before any edit):

| Crate | Count | Errors |
| --- | ---: | --- |
| `askpass` | 5 | `util::fs` (`askpass.rs:8`); `util::shell` (`:28`); E0034 `write_all` (`:454`); `try_shell_safe` (`:494`, `:527`) |
| `fuzzy` | 2 | `BackgroundExecutor::scoped` (`fuzzy/src/paths.rs:185`, `fuzzy/src/strings.rs:159`) |
| `lsp` | 4 | `util::command` (`lsp.rs:22`, `:539`); `OsString: Serialize` (`:95`); `send_blocking` (`:1684`) |
| `rpc` | 3 | `zstd` (`message_stream.rs:54`, `:88`, `:89`) |
| `task` (already visible) | 2 | `util::shell` / `util::shell_builder` re-exports (`task.rs:29`, `:30`) |

The “19” in the prompt is the workspace total including `task` plus `could not compile` wrappers / `wasmtime` that were already peeking through. The four-crate rustc set is **14**. `task` is the same root cause as askpass (util modules cfg’d out).

Also present before any edit, and **not** this round: `wasmtime` mmap (3) and `tree-sitter` C build (`wasm_store.c:315` undeclared `printf`).

## 2. What `zed-web` actually did

`fecc3273..zedweb/zed-web` on these crates (31 files, +902/−165). Pattern: **do not cfg-out the util modules; stub inside them.** `fuzzy` is untouched — they keep `BackgroundExecutor::scoped` on wasm and make `Scope::drop` work without `scheduler.block`.

Taken this round (adapted onto our tree, native path kept):

| File | zed-web move |
| --- | --- |
| `util/src/util.rs` | Ungate `archive` `command` `fs` `process` `shell` `shell_builder` `shell_env`; ungate `pub use self::shell::{…}`; wasm `get_shell_safe_zed_path` bails |
| `util/Cargo.toml` | smol available on wasm |
| `util/src/command.rs` | Native `Command` `all(not(macos), not(wasm))`; wasm `Command` wrapping `smol::process::Command` |
| `util/src/fs.rs` | Native fs ops `not(wasm)`; wasm stubs |
| `util/src/paths.rs` | `try_shell_safe` exists on wasm (same body as native) |
| `util/src/shell_builder.rs` | wasm `build_smol_command` via `Command::new` + `args` (no `From<std::process::Command>`, which `smol_wasm` lacks) |
| `util/src/process.rs` | wasm `Child` forwarding to `smol_wasm` remote spawn; native spawn/kill cfgs narrowed with `not(wasm)` |
| `gpui/src/executor.rs` | `block_on_ready` for `Scope::drop` on wasm |
| `askpass/src/askpass.rs` | UFCS `std::io::Write::write_all` / `Read::read_to_end`; `futures::AsyncWriteExt::write_all` |
| `lsp/src/lsp.rs` | `serialize_arguments` for `OsString`; `try_send` on wasm |
| `rpc/src/message_stream.rs` | zstd encode/decode `not(wasm)`; wasm sends/reads uncompressed protobuf |

**Not taken** (later / native-path / Phase 3a already decided):

- `rpc` `wasm_conn.rs`, `async-tungstenite` → `tungstenite`, `MaybeSend`, `Connection` minus `Send`
- `lsp` `ZED_WEB_PROCESS_KIND`, `working_dir` skip `is_dir`, `process_id: None` (not in the 14)
- `paths.rs` `home_dir() = "/workspace"` — previous round already has that placeholder; this round only touched `try_shell_safe`
- Moving `smol` into util `[dependencies]` (always). Phase 3a refused that for the root workspace. This round adds a **wasm-only** target table instead, so native still resolves smol from `not(wasm)` only.

## 3. Per-crate how it was fixed

### `util` (unblocks the four, and `task`)

Modules `command` / `fs` / `shell` / `shell_builder` / `process` / `archive` / `shell_env` are public on wasm again, matching zed-web. Native bodies are the original functions behind `not(target_family = "wasm")`.

`Cargo.toml`: new `[target.'cfg(target_family = "wasm")'.dependencies] smol.workspace = true`. Native table unchanged. Needed so wasm `command.rs` / `process.rs` / `shell_builder.rs` can name `smol::process`. In the web workspace that smol is the `smol_wasm` patch; spawn fails with `"remote RPC client not initialized"` until Phase 4/6 wires the bridge — that is `io::Error`, not a fake child.

### `askpass`

No wasm stubs. UFCS so `UnixStream`’s `std::io::Write` and `futures::AsyncWriteExt` do not collide (E0034). `util::fs` / `util::shell` / `try_shell_safe` exist again, so the other four errors go away with no askpass control-flow change.

### `fuzzy`

No file change. `BackgroundExecutor::scoped` and `Scope` are compiled on wasm again. Native `scoped` body is the same; only the cfg that hid it was removed. `Scope::drop` still calls `scheduler.block` on native; wasm uses zed-web’s `block_on_ready` (polls a noop waker until `Ready`).

### `lsp`

- `util::command` exists on wasm.
- `OsString: Serialize`: serde implements this on unix/windows only, so the derive fails only on wasm. `#[cfg_attr(target_family = "wasm", serde(serialize_with = "serialize_arguments"))]` — **native still uses serde’s `OsString` impl**. zed-web applied `serialize_with` unconditionally (lossy on native too); refused here so native serialization is unchanged.
- `send_blocking` kept on native. wasm `try_send`. zed-web swapped unconditionally, same class as the §5.3.4 `remote_server` refusal. Gating is the only way to satisfy both “wasm compiles” and “native verbatim”.

### `rpc`

Only the zstd sites in `message_stream.rs`. Native still `zstd::stream::encode_all` / `copy_decode` at the same compression levels. wasm writes/reads the protobuf bytes uncompressed (zstd is native-only since Phase 3a). Instant / tungstenite left as Phase 3b left them.

## 4. Native behaviour — proof it did not change

Every new arm is `#[cfg(target_family = "wasm")]`. Every pre-existing native body is either untouched or wrapped `not(target_family = "wasm")` / `all(…, not(target_family = "wasm"))` with the original statements inside.

Concrete native-still-there checks:

| Site | Native still runs |
| --- | --- |
| `util::command::Command` | `all(not(macos), not(wasm))` — the previous `not(macos)` impl, including the windows `CREATE_NO_WINDOW` branch |
| `util::fs::{remove_matching, collect_matching, find_file_name_in_dir, move_folder_files_to_folder}` | original `async_fs` bodies under `not(wasm)` |
| `make_file_executable` unix | original `chmod 0o755` |
| `make_file_executable` windows | original `Ok(())` under `all(not(unix), not(wasm))` |
| `PathExt::try_shell_safe` | same `try_quote` body; only the `not(wasm)` cfg on the method was removed |
| `process::Child::spawn` / `kill` / `output` | original unix/windows impls, cfgs narrowed with `not(wasm)` |
| `BackgroundExecutor::scoped` / `Scope::drop` | original spawn-and-await / `scheduler.block` |
| `lsp::notify_internal` | `send_blocking` (zed-web’s native `try_send` **not** taken) |
| `LanguageServerBinary::arguments` | native serde `OsString` (zed-web’s unconditional `serialize_with` **not** taken) |
| `message_stream` zstd | original encode/decode + `COMPRESSION_LEVEL` −7/4 |

Compiler:

```
CARGO_TARGET_DIR=target/web-probe cargo check -p askpass -p fuzzy -p lsp -p rpc -p util --lib
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 14.86s
```

Exit 0. Zero `error[` lines.

## 5. Stub honesty

| Stub | Honest? | Why |
| --- | --- | --- |
| `get_shell_safe_zed_path` (wasm) | **Yes** | `anyhow::bail!("zed executable path is not available in the browser")` |
| `make_file_executable` (wasm) | **Yes** | `Err(ErrorKind::Unsupported)` — **divergence from zed-web**, which fell through `not(unix)` to silent `Ok(())`. Native windows still `Ok(())`. |
| `move_folder_files_to_folder` (wasm) | **Yes** | `bail!("filesystem operations are not supported in the browser")` |
| `command::Command::spawn` (wasm) | **Yes** | Delegates to `smol_wasm`; `remote_state()` returns `io_error("remote RPC client not initialized")` until the bridge is up |
| `command::Command::{output,status}` (wasm) | **Yes** | `self.spawn()?.output()/status()` — same failure as spawn. zed-web called `self.0.output()`; our `smol_wasm` `Command` has no `output`/`status` (those are on `Child`) |
| `process::Child::spawn` (wasm) | **Yes** | Same remote spawn; fails without RPC |
| `process::Child::output` (wasm) | **Yes** | `bail!("process output is not supported in the browser")` (zed-web) |
| `process::Child::kill` (wasm) | **Yes** | Forwards to inner `kill`; no-op only if `inner` already taken |
| `shell_builder::build_smol_command` (wasm) | **Yes** | Builds a real `smol::process::Command`; does not pretend it ran |
| `message_stream` uncompressed (wasm) | **Yes** | No zstd crate on wasm; codec is identity, not a fake compressed frame |
| `lsp` `try_send` (wasm) | **Yes** | Returns the channel error on full; does not drop silently. Native still blocks. |

**Non-`Result` stubs that cannot fail without a signature change** (zed-web; not invented). Reported rather than “fixed”:

| Stub | What it returns | Why not `Err` |
| --- | --- | --- |
| `fs::remove_matching` (wasm) | `()` no-op | Signature is `async fn(...)` with no `Result` |
| `fs::collect_matching` (wasm) | `Vec::new()` | Returns `Vec`, not `Result` |
| `fs::find_file_name_in_dir` (wasm) | `None` | Returns `Option` |
| `process::Child::id` (wasm) | `0` if `inner` is `None` | zed-web sentinel after stdio/status taken; not a live pid |

Pre-existing from Phase 3b (ungated this round, not rewritten): `archive::extract_zip` wasm `Ok(())`; `shell_env::capture` wasm `Ok(empty map)`. Both are silent success. They were already on disk; this round only made the modules compile. Not re-decided.

## 6. Acceptance

### 1. wasm workspace check

```
cd web && CARGO_TARGET_DIR=../target/web-probe cargo check --workspace --target wasm32-unknown-unknown
```

Exit 101. **Zero errors in `askpass` `fuzzy` `lsp` `rpc`.** (Confirmed separately: `cargo check -p askpass -p fuzzy -p lsp -p rpc --lib --target wasm32-unknown-unknown` → `Finished … in 4.64s`, exit 0.)

Collateral: `cargo check -p task --lib --target wasm32-unknown-unknown` → `Finished … in 3.06s`. The two `task` import errors from the baseline are gone because `util::shell` / `util::shell_builder` exist. Not a drive-by edit of `task`.

**New errors that the workspace run actually emitted** (next round’s input):

```
error[E0432]: unresolved import `crate::runtime::vm::sys::mmap`
   --> wasmtime-48.0.1/src/runtime/vm.rs:120:34
        pub use crate::runtime::vm::sys::mmap::open_file_for_mmap;
        note: gated on `has_virtual_memory`

error[E0433]: cannot find type `Mmap` in this scope
   --> wasmtime-48.0.1/src/runtime/vm/mmap_vec.rs:195:20
        let mmap = Mmap::from_file(...)

error[E0599]: no variant ... `new_mmap` found for enum `MmapVec`
   --> wasmtime-48.0.1/src/runtime/vm/mmap_vec.rs:198:21
        Ok(MmapVec::new_mmap(mmap, len))

error: failed to run custom build command for `tree-sitter v0.27.0`
        (rev 43623ec9…)
        lib/src/wasm_store.c:315: undeclared `printf`
        (ISO C99 implicit function declaration)
```

Two third-party crates: **`wasmtime` 48.0.1** (3 rustc) and **`tree-sitter` git `43623ec`** (cc build). No first-party crate beyond the four (and the `task` collateral) failed this run — cargo aborted on those two build failures before later members were type-checked. Whatever is behind `tree-sitter` / `wasmtime` in the graph is the next wall, not more of `askpass`/`fuzzy`/`lsp`/`rpc`.

### 2. native

See §4. `Finished … in 14.86s`, exit 0.

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

`--edition 2024 --config-path rustfmt.toml --check` on every `.rs` this round edited: exit 0. The five pre-dirty files were not formatted.

### 5. `Cargo.lock`

```
cmp /tmp/lock-before-p5b Cargo.lock
IDENTICAL
```

No `[[package]]` / version / source / checksum change. Adding wasm-target `smol` on `util` does not resolve a new crate (smol is already in the lock for native).

## 7. Files this round

```
crates/askpass/src/askpass.rs
crates/gpui/src/executor.rs
crates/lsp/src/lsp.rs
crates/rpc/src/message_stream.rs
crates/util/Cargo.toml
crates/util/src/command.rs
crates/util/src/fs.rs
crates/util/src/paths.rs          # try_shell_safe cfg only; home_dir left as previous round
crates/util/src/process.rs
crates/util/src/shell_builder.rs
crates/util/src/util.rs
docs/phase5-blockers-2.md
```

`fuzzy` not edited. `web/` not edited. No commit.
