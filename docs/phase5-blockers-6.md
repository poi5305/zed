# Phase 5g — wasm blockers unrelated to `TSLanguage` (`language` `remote` `terminal`)

Date: 2026-09-15. Branch `andy/web-version`. **No commit.** `web/` was not edited. `README.md` `> [!IMPORTANT]` lines left in place. The five pre-dirty rustfmt files (`claude_sessions_panel.rs`, `session_store.rs`, `remote/claude_sessions.rs`, `remote_server/server.rs`, `tmux_sessions_panel.rs`) were not touched.

Specs: `docs/web-zed-plan.md` §5.1, §9, Phase 5 BLOCKER. cfg spelling: `target_family = "wasm"` only. **No `unsafe impl`.**

This round wrote only under `crates/` (plus this report). Root `Cargo.toml` / `web/` were not retouched.

Baseline lock: `cp Cargo.lock /tmp/lock-before-p5g` before any edit. Lock unchanged (`cmp` identical, 502232 bytes).

## 0. codegraph (required first call)

```
codegraph explore "TSLanguage Grammar ts_language TcpStream clone incoming event_loop tty sysinfo ProcessIdGetter"
```

Returned 108 symbols across 8 files. Blast radius was `Language` / `grammar` / `ProcessIdGetter` / `PtyProcessInfo`, not Cargo manifests and not `tree_sitter_wasm::Language`'s `Send` impls.

Manifests are not in the graph. Confirmed by grep / `git show`:

- `crates/terminal/Cargo.toml` already has `sysinfo` / `libc` under `not(wasm)` (Phase 3a). The `.rs` still named them.
- `crates/remote` has no zed-web `port_forward.rs` at all (`git ls-tree zedweb/zed-web crates/remote/` is `json_log` `protocol` `proxy` `remote_client` `transport/*` only). Port forwarding is this fork's feature.
- `language_core::Grammar.ts_language` is already `#[cfg(not(target_family = "wasm"))]`; wasm stores `parseable_language: ParseableLanguage`. `From<tree_sitter::Language> for ParseableLanguage` is native-only.

## 1. Baseline (this machine, re-run)

```
cd web && CARGO_TARGET_DIR=../target/web-probe cargo check --workspace --target wasm32-unknown-unknown
```

`error[` in `language` + `remote` + `terminal` = **23**. Plus 3 `could not compile` summary lines = the prompt's **26**. Out of scope and ignored: `wasm_remote` (6) and `tree-sitter-json` WASI (`stdlib.h`).

| Crate | `error[` | Notes |
| --- | ---: | --- |
| `language` | 15 | 10× `*const TSLanguage` Send/Sync, 2× `ts_language` field, plus `NonNull<TSParser>` Send, `ParseableLanguage: From<Language>`, `block_with_timeout` |
| `terminal` | 5 | `event_loop`/`tty`, `sysinfo` import, `ProcessIdGetter::pid` ×2, `sysinfo` in `terminal.rs:3081` |
| `remote` | 3 | `TcpStream::clone` ×2, `TcpListener::incoming` |

## 2. Classification

### (a) TSLanguage family — **left untouched**

The Phase 5 BLOCKER. Vendor `43623ec` gates `unsafe impl Send/Sync for Language` (upstream #5851). Adding those impls is refused.

| Count | Error | Site |
| ---: | --- | --- |
| 5 | `*const TSLanguage` cannot be sent | `language_registry.rs:852`, `buffer.rs:1374`, `buffer.rs:1956`, `buffer.rs:3542`, `language_registry.rs:705` |
| 5 | `*const TSLanguage` cannot be shared | same five sites |
| 2 | no field `ts_language` on `&language_core::Grammar` | `syntax_map.rs:1571`, `language.rs:1423` (`parse_text`) |

**12 errors. Still present after this round.** These are the input for the architecture ruling (Language/Parser stay on the creating thread; only `Tree`s cross).

zed-web's `language.rs` / `syntax_map.rs` rewrite those two field accesses to `grammar.parseable_language()`. That *is* the API `language_core` already exposes for wasm, and native `parseable_language()` is `Ok(self.ts_language.clone())`. It was left alone because this assignment listed those two with the Send/Sync pile.

### (b) Everything else — **cleared**

11 `error[` (the prompt's "14" counted the 3 `could not compile` summaries into the 26). Same pattern as earlier Phase 5 layers: manifest already gated, `.rs` did not; wasm-missing API; native-only dep used unconditionally.

## 3. What zed-web actually did

```
git diff fecc3273ed32643c2ea1b04a74c8780e2c9ffaf8..zedweb/zed-web -- crates/remote crates/terminal crates/language
```

**terminal.** Gates `event_loop`/`tty`/`sysinfo`. Wasm `AlacrittyPty = ()`. Wasm `PtySender` forwards to `remote_pty::RemotePty` over `wasm_rpc`. Changes `ProcessIdGetter::{pid,fallback_pid}` and `Terminal::pid` to `u32` **on all platforms** (API_BREAK). Adds `web-sys` / `wasm_rpc` / `base64` wasm deps.

**remote.** Instant → `web_time` only. **No `port_forward.rs`.** `smol_wasm::net::TcpStream` has no `clone`; `TcpListener` has `accept` but no `incoming`. §4.3: do not put those APIs back into `smol_wasm`.

**language.** `buffer.rs` is mostly the `resolved_settings` / `BufferEvent::SettingsChanged` API_BREAK (already in our tree by another route). zed-web still calls `block_with_timeout` because their gpui keeps it on wasm. Ours already gated it `not(wasm)`. They rewrite `.ts_language` → `.parseable_language()` (left in (a)).

Not copied: `RemotePty` + `wasm_rpc` (lives under `web/crates/`, out of scope). Not copied: zed-web's native `fallback_pid: Pid → u32` signature change.

## 4. How (b) was fixed (native bodies unchanged)

### `terminal` (5)

`crates/terminal/src/alacritty.rs` — WASM_CFG around `event_loop`/`tty`. Native `PtySender` / `open_pty` / `pty_options` / `spawn_event_loop` / `current_child_signal_mask` / `From<&AlacrittyPty>` are the pre-existing bodies under `not(wasm)`. Wasm `PtySender` methods `panic!` naming RemotePty RPC. Wasm `AlacrittyPty = ()`.

`crates/terminal/src/pty_info.rs` — native `PtyProcessInfo` and unix/windows `pid()` bodies moved under `not(wasm)` / `all(unix, not(wasm))` with the same statements. Native `fallback_pid() -> Pid` unchanged. Wasm: `pid() -> None`, `PtyProcessInfo` stub with `kill_*` returning `false` (the native failure value; no process was killed).

`crates/terminal/src/terminal.rs` — native `Terminal::pid() -> Option<sysinfo::Pid>` unchanged. Local-PTY branch of `Terminal::new` wrapped `not(wasm)`. Wasm that branch `bail!(TerminalError { source: ErrorKind::Unsupported, "local PTY is not available in the browser until RemotePty RPC lands" })`. Shift+Click selection extension not touched (§5.3.2).

### `remote` (3)

`crates/remote/src/port_forward.rs` — this fork only.

- `TcpListener::incoming()` stays on the native path. Wasm logs and returns (bind already `Err(Unsupported)` from `smol_wasm`).
- `TcpStream::clone()` stays on the native path. Wasm `begin_tunnel` / `handle_open_port_tunnel` `panic!` naming the missing clone / browser TCP limit. At runtime wasm `connect`/`bind` fail first with `Unsupported`, so the panic is a compile-time backstop, not the success path.

### `language` (3 that were not the 12)

`buffer.rs:request_autoindent` — native still `foreground_executor().block_with_timeout`. Wasm takes the same spawn path the `None` budget already uses (work is applied asynchronously; it is not dropped). Browser main thread cannot block.

`language.rs:with_parser` — native still `static PARSERS: Mutex<Vec<Parser>>`. Wasm `thread_local! { RefCell<Vec<Parser>> }` so `Parser` (`NonNull<TSParser>`, `!Send` on wasm) never crosses threads. This is *not* `unsafe impl Send for Parser`. It is the small slice of "Parser stays on the creating thread" that unblocks the pool without touching `LanguageRegistry`.

`language.rs:Language::new_with_id` — native still `Grammar::new(ts_language)` (`From<Language>` exists). Wasm `panic!` naming `ParseableLanguage::from_resolver` (cannot put a `!Send` `Language` in a `Send + Sync` resolver closure without `unsafe`). `Language::new(config, None)` (plain text) does not panic.

## 5. Native behaviour

```
CARGO_TARGET_DIR=target/web-probe cargo check -p language -p remote -p terminal --lib
```

```
    Checking remote v0.1.0 (/Users/andy/go/src/github.com/poi5305/zed/crates/remote)
    Checking terminal v0.1.0 (/Users/andy/go/src/github.com/poi5305/zed/crates/terminal)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 13.82s
```

Exit 0.

Proof the native path is the same code:

- `alacritty.rs`: every tty/event_loop function body is the pre-existing body inside `#[cfg(not(target_family = "wasm"))]`.
- `pty_info.rs`: native `pid()` / `fallback_pid() -> Pid` / `PtyProcessInfo` / unix `tcgetpgrp` / sysinfo refresh — same statements inside `not(wasm)`.
- `terminal.rs`: native `pid() -> Option<sysinfo::Pid>`; local PTY `open_pty` + `spawn_event_loop` block is the original body inside `not(wasm)`.
- `port_forward.rs`: native `incoming()` loop and `stream.clone()` are the original statements inside `not(wasm)`.
- `buffer.rs`: native `block_with_timeout` match is the original match.
- `language.rs`: native parser pool is still a process-global `Mutex<Vec<Parser>>`; native `Grammar::new(ts_language)` is the original call.

No native signature of `ProcessIdGetter::fallback_pid` or `Terminal::pid` was changed (unlike zed-web).

## 6. Stub honesty

| Stub | Failure |
| --- | --- |
| wasm `PtySender::{notify,resize,shutdown}` | `panic!` naming RemotePty RPC |
| wasm `Terminal::new` local PTY branch | `Err(Unsupported)` naming RemotePty RPC |
| wasm `ProcessIdGetter::pid` | `None` (no local child) |
| wasm `PtyProcessInfo::kill_*` | `false` (did not kill) |
| wasm `PtyProcessInfo::emit_title_changed_if_changed` | no-op (no local process to track) |
| wasm port-forward `incoming` | log + return; bind already `Unsupported` |
| wasm `TcpStream::clone` sites | `panic!` naming browser TCP |
| wasm `request_autoindent` with a budget | spawn (same as `None` budget); does not invent indent sizes |
| wasm `Language::new(..., Some(language))` | `panic!` naming `from_resolver` |
| wasm parser pool | thread-local real `Parser`s, not a fake tree |

Nothing returns a fabricated success (`Ok(())` with no work, a dummy pid, a fake grammar).

## 7. After this round — remaining errors (architecture input)

`terminal` **compiles**. `remote` **compiles**. `language` remains with **exactly the 12 TSLanguage-family errors**.

```
error[E0277]: `*const TSLanguage` cannot be sent between threads safely
   --> crates/language/src/language_registry.rs:852
error[E0277]: `*const TSLanguage` cannot be shared between threads safely
   --> crates/language/src/language_registry.rs:852
error[E0609]: no field `ts_language` on type `&language_core::Grammar`
   --> crates/language/src/syntax_map.rs:1571
error[E0609]: no field `ts_language` on type `&language_core::Grammar`
   --> crates/language/src/language.rs:1423
error[E0277]: `*const TSLanguage` cannot be sent between threads safely
   --> crates/language/src/buffer.rs:1374
error[E0277]: `*const TSLanguage` cannot be shared between threads safely
   --> crates/language/src/buffer.rs:1374
error[E0277]: `*const TSLanguage` cannot be sent between threads safely
   --> crates/language/src/buffer.rs:1956
error[E0277]: `*const TSLanguage` cannot be shared between threads safely
   --> crates/language/src/buffer.rs:1956
error[E0277]: `*const TSLanguage` cannot be sent between threads safely
   --> crates/language/src/buffer.rs:3542
error[E0277]: `*const TSLanguage` cannot be shared between threads safely
   --> crates/language/src/buffer.rs:3542
error[E0277]: `*const TSLanguage` cannot be sent between threads safely
   --> crates/language/src/language_registry.rs:705
error[E0277]: `*const TSLanguage` cannot be shared between threads safely
   --> crates/language/src/language_registry.rs:705

error: could not compile `language` (lib) due to 12 previous errors; 1 warning emitted
```

The Send/Sync notes all require `BackgroundExecutor::spawn`'s `Send` bound, because `LanguageRegistry` / `AvailableGrammar` / `Arc<Language>` carry `tree_sitter::Language(*const TSLanguage)` into those tasks.

Also still failing, **out of this assignment's crates/** scope:

- `wasm_remote` (6) — trait drift vs current `Fs` / git APIs; `web/crates/`
- `tree-sitter-json` build — `'stdlib.h' file not found`; §4.2 WASI SDK

## 8. Other acceptance

```
./web/check-refusals.sh
ok   §5.3.1 RELEASE_CHANNEL is dev
ok   §5.3.2 terminal Shift+Click selection extension exists
ok   §5.3.3 recent_projects open_local_project PathPromptOptions.files is true
ok   §5.3.4 remote_server MultiWrite::flush uses send_blocking
4 checks, 0 failures
```

rustfmt `--check` clean on the files this round wrote:

- `crates/terminal/src/alacritty.rs`
- `crates/terminal/src/pty_info.rs`
- `crates/terminal/src/terminal.rs`
- `crates/remote/src/port_forward.rs`
- `crates/language/src/buffer.rs`
- `crates/language/src/language.rs`

```
cmp Cargo.lock /tmp/lock-before-p5g && echo identical
# identical, 502232 bytes
```

## 9. Files this round actually wrote

| File | Why |
| --- | --- |
| `crates/terminal/src/alacritty.rs` | gate `event_loop`/`tty`; honest wasm `PtySender` |
| `crates/terminal/src/pty_info.rs` | native `sysinfo` path `not(wasm)`; wasm process stub |
| `crates/terminal/src/terminal.rs` | native pid type preserved; wasm local PTY `Unsupported` |
| `crates/remote/src/port_forward.rs` | native `incoming`/`clone`; wasm honest fail |
| `crates/language/src/buffer.rs` | wasm autoindent spawn instead of `block_with_timeout` |
| `crates/language/src/language.rs` | wasm thread-local parser pool; wasm `Language::new(Some(_))` panics |

No `unsafe impl`. No `web/` edits. No lock change.
