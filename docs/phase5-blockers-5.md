# Phase 5e — wasm workspace compile blockers (`client` `context_server` `language` `openai_subscribed` `remote` `terminal`)

Date: 2026-09-15. Branch `andy/web-version`. **No commit.** `web/` was not edited. `README.md` `> [!IMPORTANT]` lines left in place. The five pre-dirty rustfmt files (`claude_sessions_panel.rs`, `session_store.rs`, `remote/claude_sessions.rs`, `remote_server/server.rs`, `tmux_sessions_panel.rs`) were not touched.

Specs: `docs/web-zed-plan.md` §2.2, §4.1, §4.2, §5.1, §9. cfg spelling: `target_family = "wasm"` only.

This round wrote only under `crates/` (plus this report). Root `Cargo.toml` / `web/` were not retouched.

Baseline lock: `cp Cargo.lock /tmp/lock-before-p5e` before any edit. Lock unchanged (`cmp` identical, 502232 bytes).

## 0. codegraph (required first call)

```
codegraph explore "tokio TSLanguage Language tree_sitter"
```

Returned 96 symbols across 7 files. Blast radius was `syntax_map` / `LanguageRegistry` / `AvailableLanguage` / `LoadedLanguage`, not Cargo manifests and not `tree_sitter_wasm::Language`'s `Send` impls. A second call on `tokio::time tokio::net tokio::io …` pointed at `gpui_tokio` / `NodeRuntime`, not the `client` TCP proxy.

Manifests are not in the graph. `crates/client/Cargo.toml` already has `tokio` / `async-tungstenite` / `gpui_tokio` / `tiny_http` / `zed_credentials_provider` under `not(wasm)` (Phase 3a). `web/vendor/tree_sitter_wasm` is not a first-party symbol.

Third call (`establish_websocket_connection connect_proxy_stream Grammar ts_language ParseableLanguage block_with_timeout Parser`) showed:

- `connect_proxy_stream` is `crates/client/src/proxy.rs:30`, tokio TCP + `proxy_handshake::tokio`.
- `language_core::Grammar.ts_language` is already `#[cfg(not(target_family = "wasm"))]`; wasm uses `parseable_language: ParseableLanguage`.
- `From<tree_sitter::Language> for ParseableLanguage` is already native-only; wasm has `ParseableLanguage::from_resolver`.
- `ForegroundExecutor::block_with_timeout` is already `not(wasm)` in `crates/gpui/src/executor.rs:452`.

## 1. Baseline (this machine, re-run)

```
cd web && CARGO_TARGET_DIR=../target/web-probe cargo check --workspace --target wasm32-unknown-unknown
```

Exit 101 (the `tee; echo EXIT:$?` pipeline printed 0 because `tee` succeeds; cargo itself failed). `error[` = 58 plus several bare `error:` rows. `could not compile` for seven packages:

| Crate | Count (approx) | Notes |
| --- | ---: | --- |
| `client` | 26 | 7× `tokio` plus `async_tungstenite` / `gpui_tokio` / `tiny_http` / `fs` / `worktree` / `paths` / `zed_credentials_provider` / `http_client_tls` / `cfg_select` `os_version` |
| `language` | 15 | `*const TSLanguage` Send/Sync, `NonNull<TSParser>` Send, `ts_language` field, `ParseableLanguage: From<Language>`, `block_with_timeout` |
| `terminal` | 5 | `alacritty_terminal::{event_loop,tty}`, `sysinfo`, `ProcessIdGetter::pid` |
| `remote` | 3 | `smol::net::TcpStream::clone`, `TcpListener::incoming` |
| `openai_subscribed` | 2 | `start_oauth_callback_server_with_config` / `OAuthCallbackServerConfig` configured out |
| `context_server` | 2 | `OAuthCallbackParams` / `start_oauth_callback_server` configured out |
| `wasm_remote` | 6 | `web/crates/` — out of scope |

The prompt's "60 / 6 crates" matches this set if `wasm_remote` is excluded from the crate count. The 7 `tokio` + 10 `*const TSLanguage` rows are the two named classes; they were never the whole 60.

## 2. Category (1) `tokio` — fixed in `crates/`

Phase 3a moved `tokio` (and the collab TCP stack next to it) to `not(wasm)` in `crates/client/Cargo.toml`. The `.rs` still named those crates. Same pattern as `settings_json` / `fs` / `migrator`.

### What zed-web actually does

`git show zedweb/zed-web:crates/client/src/client.rs`:

- `#[cfg(not(target_family = "wasm"))] mod proxy;`
- native collab path stays tokio + `async_tungstenite` + `gpui_tokio` + `tiny_http`
- wasm collab path calls **`rpc::wasm_conn::connect`** (a JS WebSocket, §2.2)
- wasm `authenticate_with_browser` bails: `"authenticate_with_browser is not supported on WASM"`
- wasm `WasmCredentialsProvider` **`write_credentials` / `delete_credentials` return `Ok(())`**

This repo has **no** `rpc::wasm_conn` (`crates/rpc/src/` is `auth` `conn` `message_stream` `notification` `peer` `proto_client` only). Inventing it would be `crates/rpc` plus likely `web/` JS. Stopped.

### What this round did (native bodies unchanged)

`crates/client/src/client.rs` — WASM_CFG only:

- `mod proxy` and the tokio / tungstenite / `smol::future::yield_now` imports and the functions that use them (`establish_websocket_connection`, `authenticate_with_browser` native, `authenticate_as_admin`, `connect_to_cloud`, `run_cloud_connection`) are `not(wasm)`.
- wasm `establish_connection` returns `Err(EstablishConnectionError::other(anyhow!("collab websocket is not available on wasm until rpc::wasm_conn lands")))`.
- wasm `authenticate_with_browser` bails with the same message zed-web uses.
- `set_connection`: native still `executor.spawn(handle_io)`; wasm uses `cx.spawn` so the I/O future does not have to be `Send`.
- `WasmCredentialsProvider`: `read` → `Ok(None)` (nothing stored). **`write` / `delete` → `Err("OS keychain is not available in the browser")`.** zed-web's silent `Ok(())` is a fake success; refused.

`crates/client/src/telemetry.rs`:

- `os_name` wasm arm `"Web"` (zed-web). Native `#[cfg(target_os = …)]` arms untouched.
- `os_version` wasm arm of the existing `cfg_select!` returns `"unknown"` (same string the Windows failure path already uses). **Did not** change the native return type to `Option<String>` the way zed-web did — that would be `API_BREAK` on `pub fn os_version`.
- `fs` / `worktree` / `paths` / log file / project-type detection gated `not(wasm)`. Native spawn that creates `telemetry.log` is the same body inside the cfg.

`crates/oauth_callback_server` (not one of the six, but it is `crates/` and it is what those two crates actually named):

- `OAuthCallbackParams` / `OAuthCallbackServerConfig` / `parse_query` moved to the crate root so wasm can parse a query string. Native `mod server` still owns bind/recv/`tiny_http`; `start_*` bodies were not rewritten.
- wasm `start_oauth_callback_server` / `start_oauth_callback_server_with_config` **`bail!("OAuth callback server cannot bind a loopback TCP port in the browser")`**.
- `url` + `futures` moved from the `not(wasm)` table into `[dependencies]` so the wasm stubs and `parse_query` type-check. Native still has `tiny_http` + `log` only on `not(wasm)`.

That cleared `client`, `context_server`, and `openai_subscribed` on wasm. `openai_subscribed` / `context_server` `.rs` were not edited.

## 3. Category (2) `*const TSLanguage` — **must edit `web/vendor/tree_sitter_wasm`**

Stopped. Did not touch `web/`.

### Comparison

| Tree | `unsafe impl Send/Sync for Language` |
| --- | --- |
| Real checkout `~/.cargo/git/checkouts/tree-sitter-a21c02e4b1d6dd0c/43623ec/lib/binding_rust/lib.rs:4103-4124` | **present, but `#[cfg(not(target_family = "wasm"))]`** |
| Ours `web/vendor/tree_sitter_wasm/binding_rust/lib.rs:4103-4124` | **byte-identical to 43623ec**, including those gates |
| `git show zedweb/zed-web:crates/tree_sitter_wasm/binding_rust/lib.rs` ~3915 | **ungated** |

The stub did **not** forget the impls. It faithfully copied 43623ec, and 43623ec itself turns `Send`/`Sync` off on `target_family = "wasm"` (upstream is thinking of the wasmtime grammar-loader, not "Zed compiled to wasm32"). zed-web's older snapshot dropped those gates so `Language` / `Parser` can cross GPUI background tasks.

The wasm check names the vendor file:

```
note: required because it appears within the type `tree_sitter::Language`
   --> vendor/tree_sitter_wasm/binding_rust/lib.rs:68:11
```

`Language` is `pub struct Language(*const ffi::TSLanguage);` at line 68. `Parser` is `NonNull<TSParser>` (`language.rs:134`).

### Exact vendor patch (next assignment)

File: **`web/vendor/tree_sitter_wasm/binding_rust/lib.rs`**

Delete these eight `#[cfg(not(target_family = "wasm"))]` lines so the impls match zed-web / compile on wasm. Leave the `unsafe impl` bodies:

```
4103: #[cfg(not(target_family = "wasm"))]   // DROP — Language Send
4105: #[cfg(not(target_family = "wasm"))]   // DROP — Language Sync
4111: #[cfg(not(target_family = "wasm"))]   // DROP — LookaheadIterator Send
4113: #[cfg(not(target_family = "wasm"))]   // DROP — LookaheadIterator Sync
4116: #[cfg(not(target_family = "wasm"))]   // DROP — LookaheadNamesIterator Send
4118: #[cfg(not(target_family = "wasm"))]   // DROP — LookaheadNamesIterator Sync
4121: #[cfg(not(target_family = "wasm"))]   // DROP — Parser Send
4123: #[cfg(not(target_family = "wasm"))]   // DROP — Parser Sync
```

After the drop, these remain (already ungated in both 43623ec and the vendor): `Node`, `Query`, `QueryCursor`, `Tree`, `TreeCursor`.

Do **not** add new impls. Do **not** change `Language(*const TSLanguage)` itself.

`crates/language` still has follow-up **after** that vendor patch (not done this round; would be `crates/`):

| Site | Why it still fails even with Send/Sync |
| --- | --- |
| `language.rs:967` | `ParseableLanguage: From<tree_sitter::Language>` is native-only in `language_core`; wasm wants `from_resolver` |
| `language.rs:1386`, `syntax_map.rs:1571` | `Grammar.ts_language` is native-only; wasm should call `parseable_language()` |
| `buffer.rs:2082` | `ForegroundExecutor::block_with_timeout` is already `not(wasm)` |
| `language.rs:134-168` | parser pool + `WasmStore` / `wasmtime` (desktop tree-sitter wasm grammars, the §4.1 wall) |

Working around Send by changing `AvailableGrammar` / `LanguageRegistry` would touch the native path. Refused.

## 4. Native behaviour

```
CARGO_TARGET_DIR=target/web-probe cargo check -p client -p language -p remote -p terminal --lib
```

```
    Checking client v0.1.0 (/Users/andy/go/src/github.com/poi5305/zed/crates/client)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 19.21s
```

Exit 0.

Proof the native path is the same code:

- `client.rs`: every native function body is the pre-existing body with a `#[cfg(not(target_family = "wasm"))]` on the item or around the call. `establish_websocket_connection` / `authenticate_with_browser` / `connect_to_cloud` / `proxy` / tokio imports are not rewritten.
- `telemetry.rs`: native `os_name` / `os_version` arms, native log-file spawn, native `detect_project_types` — same statements inside `not(wasm)`.
- `oauth_callback_server`: `parse_query` moved up a module with the same loop over `url::form_urlencoded::parse`. `mod server` still binds `tiny_http` and spawns the OS thread.

`web_time::Instant` in `telemetry.rs` was already on the working tree from Phase 3b (`git status` at session start listed `telemetry.rs`); this round did not swap Instant on native.

## 5. After this round — wasm check (real output)

```
cd web && CARGO_TARGET_DIR=../target/web-probe cargo check --workspace --target wasm32-unknown-unknown
```

`error[` = 51. **`client` compiles** (13 warnings, no `could not compile`). **`context_server` and `openai_subscribed` no longer appear in `could not compile`.** Remaining `could not compile`:

```
error: could not compile `async-tar-real` (lib) due to 22 previous errors; 3 warnings emitted
error: failed to run custom build command for `tree-sitter-json v0.24.8`
error: could not compile `wasm_remote` (lib) due to 6 previous errors; 1 warning emitted
error: could not compile `terminal` (lib) due to 5 previous errors; 2 warnings emitted
error: could not compile `language` (lib) due to 15 previous errors; 1 warning emitted
error: could not compile `remote` (lib) due to 3 previous errors; 2 warnings emitted
```

### Remaining of the original six

**`language` (15)** — blocked on §3 vendor Send/Sync, then the `crates/language` follow-up in the table above.

**`terminal` (5)** — original, not either named class. Vendored `alacritty_terminal` already gates `event_loop` / `tty` `not(wasm)`. zed-web's `crates/terminal/src/alacritty.rs` + `pty_info.rs` wrap those in `not(wasm)` and stub `AlacrittyPty = ()`. That is `crates/` work. Not done (large, §5.3 Shift+Click must stay). Sites:

- `crates/terminal/src/alacritty.rs:11` `event_loop`, `tty`
- `crates/terminal/src/pty_info.rs:8` `sysinfo`; `:122` / `:242` `ProcessIdGetter::pid`
- `crates/terminal/src/terminal.rs:3081` `sysinfo`

**`remote` (3)** — original. `smol_wasm` has no `TcpStream::clone` / `TcpListener::incoming` (§4.3: do not put unix/net back into `smol_wasm`). Gate `crates/remote/src/port_forward.rs` in `crates/` or stub the tunnels honestly. Sites: `:384` `incoming`, `:416` / `:645` `clone`.

### Newly emerging (not in the original six; not fixed)

**`async-tar-real`** (`web/vendor/async_tar_wasm/git_bd3ad6f/`) — 22 errors, `async_std::fs` missing on wasm. Was not in the baseline `could not compile` list. `web/` — stopped.

**`tree-sitter-json` build** — `src/tree_sitter/parser.h:10:10: fatal error: 'stdlib.h' file not found`, `CC_wasm32_unknown_unknown = None`. This is §4.2 WASI SDK (`web/build.sh` `CC_wasm32_unknown_unknown` + wasi-sysroot). `web/` / env — stopped.

**`wasm_remote`** (6) — `web/crates/wasm_remote/src/{fs,git}.rs` trait drift vs current `Fs` / git APIs. `web/` — stopped.

## 6. Other acceptance

```
./web/check-refusals.sh
ok   §5.3.1 RELEASE_CHANNEL is dev
ok   §5.3.2 terminal Shift+Click selection extension exists
ok   §5.3.3 recent_projects open_local_project PathPromptOptions.files is true
ok   §5.3.4 remote_server MultiWrite::flush uses send_blocking
4 checks, 0 failures
```

rustfmt `--check` clean on the files this round wrote:

- `crates/client/src/client.rs`
- `crates/client/src/telemetry.rs`
- `crates/oauth_callback_server/src/oauth_callback_server.rs`

```
cmp Cargo.lock /tmp/lock-before-p5e && echo identical
# identical, 502232 bytes
```

## 7. Files this round actually wrote

| File | Why |
| --- | --- |
| `crates/client/src/client.rs` | tokio / collab WASM_CFG + honest wasm stubs |
| `crates/client/src/telemetry.rs` | `os_name` / `os_version` wasm arms; native-only fs/worktree/log |
| `crates/oauth_callback_server/src/oauth_callback_server.rs` | types always available; wasm `start_*` bail |
| `crates/oauth_callback_server/Cargo.toml` | `url` + `futures` always, so wasm stubs type-check |

Pre-existing dirty `crates/client/Cargo.toml` and `crates/client/src/telemetry/event_coalescer.rs` were not edited this round.
