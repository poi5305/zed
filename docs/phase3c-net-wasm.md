# Phase 3c — `crates/net` wasm Unix-socket stubs

Date: 2026-09-15. Branch `andy/web-version`. **Only `crates/net/` source was edited. No commit.**
`web/` was not edited. `README.md` `> [!IMPORTANT]` lines left in place.

Specs: `docs/web-zed-plan.md` §4.1, §5.1, §9.
Reference: `zedweb/zed-web` vs merge-base `fecc3273ed32643c2ea1b04a74c8780e2c9ffaf8`.

Baseline: `cp Cargo.lock /tmp/lock-before-net` before any edit.

## 0. codegraph (required first call)

```
codegraph explore "UnixListener"
codegraph explore "smol::net::unix"
```

`UnixListener` / `UnixStream` in this crate:

| Type | File | Shape |
| --- | --- | --- |
| `async_net::UnixListener` | `crates/net/src/async_net.rs` | native unix: re-export `smol::net::unix::{UnixListener, UnixStream}`; windows: wrapper around `Async<crate::UnixListener>` |
| `net::UnixListener` | `crates/net/src/net.rs` | native unix: re-export `std::os::unix::net::{UnixListener, UnixStream}`; windows: `listener.rs` / `stream.rs` |

Blast radius of the **crate's** types (not GPUI `listener`):

- `UnixListener` (`async_net.rs`) — callers in `crates/net/src/async_net.rs`, `crates/http_proxy/src/proxy.rs`; tests via `crates/http_proxy/tests/end_to_end.rs`
- `UnixStream` (`async_net.rs`) — callers in `async_net.rs`, `crates/context_server/src/listener.rs`, `crates/etw_tracing/etw_tracing.rs`, `crates/net/src/listener.rs`

Other first-party users (grep, because codegraph also matched GPUI `listener` / sandbox `std::os::unix::net`): `crates/askpass/src/askpass.rs`, `crates/remote_server/src/server.rs`. None of those crates were touched.

Return shapes that the wasm stub has to name:

- `UnixListener::bind(path) -> io::Result<Self>`
- `UnixListener::accept(&self) ->` async `io::Result<(UnixStream, ())>`
- `UnixStream::connect(path) -> io::Result<Self>` (std / this stub; smol's native unix `connect` is async)
- `std::io::Read` / `Write` on the std type; `futures::io::AsyncRead` / `AsyncWrite` on the async type

## 1. What `zed-web` actually did

`git ls-tree -r --name-only zedweb/zed-web -- crates/net` is the same six sources plus `Cargo.toml` / `LICENSE-GPL`. No extra file.

`git diff fecc3273ed32643c2ea1b04a74c8780e2c9ffaf8..zedweb/zed-web -- crates/net` is three hunks:

1. **`Cargo.toml`**: add wasm-only `futures.workspace = true`. Leave `async-io = "2.4"` under `[target.'cfg(target_os = "windows")'.dependencies]` (this is the §4.1 "Windows only" line — it was already Windows-only at the merge-base; zed-web did not move it).
2. **`async_net.rs`**: change `#[cfg(not(windows))]` re-export of `smol::net::unix` to `#[cfg(all(not(windows), not(wasm)))]`, and `#[cfg(wasm)] pub use crate::wasm::{UnixListener, UnixStream}`.
3. **`net.rs`**: same gate on `std::os::unix::net`, plus an inline `pub mod wasm` of stub `UnixListener` / `UnixStream` that `bind`/`connect`/`accept` with `ErrorKind::Unsupported`, and that implement `std::io::{Read,Write}` plus `futures::io::{AsyncRead,AsyncWrite}` as no-ops / ready-zero.

It does **not** add `smol::net::unix` to `smol_wasm`. That is the §4.1 wall: `smol_wasm` dropping `async-io`/`async-process` is what removes `polling`/`rustix`/`errno` from the wasm graph.

## 2. What this tree did

Same three hunks, applied onto our `crates/net` (which still matched the merge-base). `git diff -- crates/net` is byte-identical to the zed-web diff above.

Native cfg matrix after the change:

| Target | `net::{UnixListener,UnixStream}` | `async_net::{UnixListener,UnixStream}` |
| --- | --- | --- |
| unix not wasm | `std::os::unix::net` | `smol::net::unix` |
| windows | `listener.rs` / `stream.rs` (`async-io`) | `async_net::windows` wrapping those |
| wasm | `net::wasm` stubs | same stubs via `crate::wasm` |

`smol` stays in `[dependencies]` (zed-web did too). `async-io` stays Windows-only. `futures` is wasm-only.

## 3. Why that is the minimum difference

- The two compiler errors were exactly those two un-gated imports (`std::os::unix`, `smol::net::unix`). Gating them off wasm and substituting a stub is the whole fix.
- No new file (zed-web inlined `mod wasm` in `net.rs`).
- No function-signature change on the native arms.
- No change to `web/vendor/smol_wasm`.
- Callers outside `crates/net/` were not retouched; they still see the same native types.
- `Cargo.lock` vs the pre-edit backup is **one added line** under package `net`: `"futures 0.3.32"`. That crate was already in the lock (`source = registry`, checksum `8b147ee9d1f6d097cef9ce628cd2ee62288d963e16fb287bd9286455b241382d`). No version or source of any existing package moved.

## 4. Native behaviour

Unchanged. On this Darwin host, `not(windows)` and `not(wasm)` still select `std::os::unix::net` and `smol::net::unix`. Windows modules, `async-io`, and the in-crate tests are untouched. The stub module is `#[cfg(target_family = "wasm")]` so it is not compiled into the desktop `net` lib.

## 5. Acceptance — real output

### 5.1 Native `cargo check -p net --lib`

Command: `CARGO_TARGET_DIR=target/web-probe cargo check -p net --lib`

```
    Checking fastrand v2.3.0
   Compiling rustix v1.1.4
    Checking crossbeam-utils v0.8.21
    Checking libc v0.2.186
    Checking futures-lite v2.6.1
    Checking piper v0.2.4
    Checking concurrent-queue v2.5.0
    Checking errno v0.3.14
    Checking signal-hook-registry v1.4.6
    Checking event-listener v5.4.1
    Checking event-listener-strategy v0.5.4
    Checking async-channel v2.5.0
    Checking async-lock v3.4.2
    Checking async-executor v1.13.3
    Checking blocking v1.6.2
    Checking async-fs v2.2.0
    Checking polling v3.11.0
    Checking async-io v2.6.0
    Checking async-signal v0.2.13
    Checking async-net v2.0.0
    Checking async-process v2.5.0 (https://github.com/zed-industries/async-process.git?rev=0b6d6713570af61806e1e5cb40e0f757cb93fd9d#0b6d6713)
    Checking smol v2.0.2
    Checking net v0.1.0 (/Users/andy/go/src/github.com/poi5305/zed/crates/net)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.35s
```

Exit 0. `net` compiled.

### 5.2 wasm `cargo check -p net --lib`

Command: `cd /Users/andy/go/src/github.com/poi5305/zed/web && CARGO_TARGET_DIR=../target/web-probe cargo check -p net --target wasm32-unknown-unknown --lib`

```
warning: unstable feature specified for `-Ctarget-feature`: `atomics`
  |
  = note: this feature is not stably supported; its behavior can change in the future

warning: field `path` is never read
  --> vendor/smol_wasm/src/fs.rs:50:5
   |
49 | pub struct File {
   |            ---- field in this struct
50 |     path: PathBuf,
   |     ^^^^
   |
   = note: `#[warn(dead_code)]` (part of `#[warn(unused)]`) on by default

warning: function `unsupported` is never used
   --> vendor/smol_wasm/src/process.rs:289:4
    |
289 | fn unsupported<T>(message: &str) -> io::Result<T> {
    |    ^^^^^^^^^^^

warning: `smol` (lib) generated 3 warnings
    Checking net v0.1.0 (/Users/andy/go/src/github.com/poi5305/zed/crates/net)
warning: `net` (lib) generated 1 warning (1 duplicate)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.38s
```

Exit 0. **`net` itself compiled for `wasm32-unknown-unknown`.** The original two errors are gone:

- `error[E0433]: cannot find unix in os` — gone (`std::os::unix` is not selected on wasm)
- `error[E0432]: unresolved import smol::net::unix` — gone (`smol::net::unix` is not selected on wasm)

The `net` warning is a duplicate of the workspace `-Ctarget-feature=atomics` note, not a compile error. `smol_wasm` warnings are pre-existing in the vendored crate and were not touched.

Side effect of this required command: cargo updated already-untracked `web/Cargo.lock`. No file under `web/` was hand-edited.

### 5.3 `./web/check-refusals.sh`

```
ok   §5.3.1 RELEASE_CHANNEL is dev
ok   §5.3.2 terminal Shift+Click selection extension exists
ok   §5.3.3 recent_projects open_local_project PathPromptOptions.files is true
ok   §5.3.4 remote_server MultiWrite::flush uses send_blocking

4 checks, 0 failures
```

4/4 green.

### 5.4 `Cargo.lock` vs `/tmp/lock-before-net`

```
--- /tmp/lock-before-net	2026-09-15 16:32:51
+++ Cargo.lock	2026-09-15 16:35:19
@@ -11376,6 +11376,7 @@
 version = "0.1.0"
 dependencies = [
  "async-io",
+ "futures 0.3.32",
  "smol",
  "tempfile",
  "windows 0.62.2",
```

That is the entire delta from this phase. `futures 0.3.32` was already locked from crates.io; no checksum, version, or source of any existing package changed. `async-io` remains listed on `net` (Windows-gated in the manifest; cargo still records it). §9's nine pinned crate sources were not in this diff.

`git diff HEAD -- Cargo.lock` is larger (~45 insertions) because the working tree lock already carried prior-phase `web-time` / `rpc` wasm deps before this session. Those lines are not part of the `/tmp/lock-before-net` delta.

### 5.5 rustfmt

```
rustfmt --edition 2024 --config-path rustfmt.toml crates/net/src/net.rs crates/net/src/async_net.rs
rustfmt --edition 2024 --config-path rustfmt.toml --check crates/net/src/net.rs crates/net/src/async_net.rs
# FMT_OK
```

The five pre-existing dirty files (`claude_sessions_panel.rs`, `session_store.rs`, `remote/claude_sessions.rs`, `remote_server/server.rs`, `tmux_sessions_panel.rs`) were not formatted.

Did not run `./script/clippy` or `--release --all-features`.

## 6. Files touched

| Path | Change |
| --- | --- |
| `crates/net/Cargo.toml` | wasm `futures.workspace = true`; `async-io` still Windows-only |
| `crates/net/src/async_net.rs` | cfg split + wasm re-export |
| `crates/net/src/net.rs` | cfg split + `pub mod wasm` stubs |
| `Cargo.lock` | `net` gained `"futures 0.3.32"` |
| `docs/phase3c-net-wasm.md` | this report |
