# Phase 3b — WASM_CFG `.rs` (Instant + small cfg gates)

Date: 2026-09-15. Branch `andy/web-version`. **No `Cargo.toml` / `Cargo.lock` touched. No commit.**

Specs: `docs/web-zed-plan.md` §5.1, §5.2, §5.3, §5.5, Phase 3. Community ref `zedweb/zed-web` vs merge-base `fecc3273ed32643c2ea1b04a74c8780e2c9ffaf8`. Instant allowlist from `git show zedweb/zed-web:web/wasm-std-instant.allowlist`.

This is a **complete subset**, not all 174 `WASM_CFG` files. Instant (§5.5) plus the small cfg-only files that do not need Phase 4 crates or signature changes. The rest of the 174 is mixed/large/Phase-4 and is listed under skipped.

## 0. codegraph (required first call)

```
codegraph explore "web_time Instant std::time Instant BackgroundExecutor now queued_early_messages_since proto_client clock.rs LanguageSettings for_buffer BufferEvent SettingsChanged prettier_store on_settings_changed PathPromptOptions open_local_project MultiWrite flush send_blocking"
```

Returned `scheduler/src/clock.rs:5` `pub use web_time::Instant` and `Clock::now() -> Instant`. `BackgroundExecutor::now()` (gpui + scheduler) returns that type. `LanguageSettings::for_buffer` is already `-> Arc<LanguageSettings>` (`language_settings.rs:280`). `BufferEvent::SettingsChanged` is already present (`buffer.rs:343`). `open_local_project` still has `PathPromptOptions { files: true, ... }`. Blast radius of `for_buffer` is 32 callers in editor/project tests — those APIs were **not** retouched (§5.2).

Second call (`AnyProtoClient queued_early_messages_since …`) showed `proto_client.rs` storing `cx.background_executor().now()` into `queued_early_messages_since: Option<Instant>` with `now: Instant` on `maybe_queue`. That Instant was `std::time::Instant` at import line 21 — the §5.5 mismatch.

## 1. What changed

**72 `.rs` files, +197 / −83.** Zero manifests. `crates/zed/RELEASE_CHANNEL` still `dev`. `web/` and existing `docs/` reports not touched.

```
72 files changed, 197 insertions(+), 83 deletions(-)
```

### 1.1 Instant (§5.5) — 64 files

Native path unchanged: off wasm, `web_time` re-exports `std::time`.

**Must-fix (not in zed-web; our tree only):** `crates/rpc/src/proto_client.rs` — `std::time::{Duration, Instant}` split to `std::time::Duration` + `use web_time::Instant`. `queued_early_messages_since` / `maybe_queue(now: Instant)` now match `BackgroundExecutor::now()`.

**Import / path swap to `web_time::Instant` (zed-web did the same; not on the allowlist as remaining `std` Instant):**

`activity_indicator.rs`, `buffer_codegen.rs`, `conversation_view.rs`, `terminal_codegen.rs`, `telemetry.rs`, `event_coalescer.rs`, `system_clock.rs`, `codestral.rs`, `context_server/client.rs`, `edit_prediction.rs`, `fim.rs`, `sweep_prompt.rs`, `bm25_context.rs`, `extension_builder.rs`, `extension_host.rs`, `fs.rs`, `fs_watcher.rs`, `threaded_dispatcher.rs`, `gpui_util/lib.rs`, `http_proxy/connection.rs`, `keymap_editor.rs`, `syntax_map.rs`, `lsp_button.rs`, `lsp.rs`, `oauth_callback_server.rs`, `log_store.rs`, `project_search.rs`, `project_panel.rs`, `macros.rs`, `typed_envelope.rs`, `remote_client.rs`, `docker.rs`, `ssh.rs`, `wsl.rs`, `message_stream.rs` (Instant only; tungstenite **not** taken), `peer.rs` (Instant only; zstd/wasm **not** taken), `delegate.rs`, `parser.rs`, `preview_view.rs`, `tabular_data_preview.rs`, `terminal_panel.rs`, `font_family_cache.rs`, `copy_button.rs`, `context_menu.rs`, `notifications.rs`, `toast_layer.rs`, `worktree.rs`, `zlog.rs`, `acp_thread.rs` (runtime import only).

**Dual-cfg, matching zed-web / allowlist** (`#[cfg(not(wasm))] use std::time::Instant` + `#[cfg(wasm)] use web_time::Instant`):

`editor.rs` (Instant only — **not** the §5.2 `for_buffer` / `SettingsChanged` hunks), `element/mouse.rs`, `language/buffer.rs` (Instant only), `multi_buffer/transaction.rs`, `agent_registry_store.rs` (Instant only — not the wasm registry URL), `buffer_store.rs`, `git_store.rs`, `job_debug_queue.rs`, `lsp_store.rs`, `text.rs`, `git_graph.rs`.

**Comment kept so the allowlist’s `std::time::Instant` string still matches**, plus `use web_time::Instant`:

`terminal/alacritty/hyperlinks.rs`, `terminal.rs` (Instant import only — Shift+Click **not** deleted), `terminal_view/terminal_element.rs`.

**`acp_thread.rs` tests** still use `std::time::Instant::now()` (allowlist: two path hits). Runtime import is `web_time`.

### 1.2 Small cfg gates — 8 files

Native path unchanged. No Phase 4 crate names, no signature changes.

| File | Gate |
| --- | --- |
| `configure_context_server_modal.rs` | `extension_host` / `ExtensionStore` `not(wasm)`; wasm `extension = None` |
| `auto_update.rs` | `cleanup_stale_installer_dirs` also `not(wasm)` |
| `dap/adapters.rs` | `latest_github_release` `not(wasm)` |
| `dap_adapters.rs` | codelldb/go/js/python adapters `not(wasm)`; gdb stays |
| `vscode_import.rs` | wasm `platform = "web"` |
| `util/archive.rs` | wasm `extract_zip` stub |
| `util/shell_env.rs` | wasm `capture` returns empty map |
| `git_runtime_diagnostics.rs` | `collect_process_tree` / `descendants_of` `not(wasm)`; wasm empty object |

## 2. Allowlist vs this tree

Allowlist rule used: keep `std::time::Instant` **strings** the checker counts; convert everything else in the wasm-reachable runtime.

| Allowlist entry | This round |
| --- | --- |
| `acp_thread.rs` two `std::time::Instant` (tests) | kept |
| `agent/src/tests/mod.rs`, evals fixture | skipped (already std; test/fixture) |
| `agent_panel.rs` `use std::time::Instant` | skipped (test module; large mixed file) |
| `alacritty_terminal` event_loop / unix | skipped (native tty; not Instant-swap in our tree) |
| `editor.rs` `use std::time::Instant` + path | dual-cfg so both strings remain |
| `editor_tests.rs` | skipped |
| `element/mouse.rs` | dual-cfg |
| `scroll.rs` `use std::time::Instant` | **skipped** — our tree has **no** Instant in this file; did not add unused imports |
| `git_graph.rs` | dual-cfg |
| `gpui/executor.rs` comment `std::time::Instant` | skipped (already `scheduler::Instant`) |
| `platform_scheduler.rs` `Instant as StdInstant` | skipped (already `scheduler::Instant`; wasm `block` rewrite not Instant) |
| `language/buffer.rs` | dual-cfg |
| `buffer_tests.rs`, `multi_buffer_tests.rs`, `text/tests.rs` | skipped |
| `transaction.rs` | dual-cfg |
| `agent_registry_store`, `buffer_store`, `git_store`, `job_debug_queue`, `lsp_store` | dual-cfg |
| `recent_projects/remote_connections.rs` five `std::time::Instant` | **kept** (allowlist); no Instant swap |
| `sandbox/macos_seatbelt.rs` | skipped (native) |
| `sidebar_tests.rs` | skipped |
| `hyperlinks.rs`, `terminal.rs`, `terminal_element.rs` | comment keeps the `std::time::Instant` token; runtime uses `web_time` |
| `util/command/darwin.rs`, `util/process.rs` | skipped (native / mixed process spawn) |

`proto_client.rs` is **not** on the allowlist and was converted.

## 3. Skipped (with reasons)

### §5.2 already on our tree — not work

- `LanguageSettings::for_buffer -> Arc` — present; editor.rs zed-web hunk that retouches it **not** applied.
- `BufferEvent::SettingsChanged` — present; editor.rs / prettier hunks **not** applied.
- `prettier_store::on_settings_changed` already removed — zed-web’s corresponding `.rs` **not** applied.

### §5.3 must not take

- `crates/zed/RELEASE_CHANNEL` still `dev`.
- `terminal.rs` Shift+Click selection extension **kept** (only Instant import + comment).
- `recent_projects.rs` `open_local_project` `files: true` **kept** (file not edited).
- `remote_server/src/server.rs` `send_blocking` **kept** (file not edited).

### Already wasm-safe Instant

- `gpui/src/gestures.rs` — already `use scheduler::Instant` (re-export of `web_time`).
- `gpui/src/executor.rs` — already `use scheduler::Instant`.

### Phase 4 / new crates / new files

zed-web hunks that mention `wasm_rpc`, `wasm_remote`, `wasm_conn`, `terminals_wasm`, `connection_wasm`, `remote_pty` on wasm `TerminalBuilder::new`, etc. Examples not taken: `client.rs`, `project.rs`, `rpc.rs`/`wasm_conn.rs`, `settings_store.rs`, `sqlez/lib.rs` (+ companion wasm modules), `net/async_net.rs` (needs `crate::wasm`), `oauth_callback_server` wasm stub + `openai_subscribed` pairing.

### Signature / native-path / mixed

- `context_server/.../stdio_transport.rs` — zed-web adds a parameter and tightens `pub` → `pub(crate)`. Forbidden (no signature changes).
- Large mixed files (`fs.rs` beyond Instant import, `workspace.rs`, `persistence.rs`, `languages/lib.rs`, `agent_panel.rs`, `recent_projects.rs` beyond Instant, `worktree.rs` beyond Instant, …): Instant import taken where listed above; the rest of those diffs is not this subset.
- `rpc/message_stream.rs` tungstenite swap **not** taken.
- `rpc/peer.rs` zstd/wasm cfg **not** taken.

### Not Instant-swap in our tree

- `editor/src/scroll.rs` — zed-web dual-cfg Instant; we have no Instant here.

The remaining ~100 of the 174 `WASM_CFG` files are this mixed/Phase-4 bucket. Doing them as a mechanical patch from `fecc3273` fails on drift; they need per-file apply against current HEAD.

## 4. Acceptance (real output)

### 4.1 `git status --short -- crates/` excluding `Cargo.toml`

72 modified `.rs` files (list in §1). No other crates/ paths except the other agent’s `Cargo.toml`s.

### 4.2 `git diff --name-only -- '*/Cargo.toml' 'Cargo.toml'`

49 paths. Unchanged by this work (Phase 3a / the other agent). No new toml names from this session.

### 4.3 `./web/check-refusals.sh`

```
ok   §5.3.1 RELEASE_CHANNEL is dev
ok   §5.3.2 terminal Shift+Click selection extension exists
ok   §5.3.3 recent_projects open_local_project PathPromptOptions.files is true
ok   §5.3.4 remote_server MultiWrite::flush uses send_blocking

4 checks, 0 failures
```

### 4.4 `./web/check-workspace-isolation.sh`

```
ok   §9.1 the nine crates resolve to their recorded sources in the root Cargo.lock
ok   §9.2 no root-workspace package has a manifest under web/
ok   §9.2 root workspace member count is unchanged
ok   §9.2 root workspace_root
ok   §3.2 web workspace_root is web/, not the repo root
ok   §9 web build does not share target/ with the desktop build
ok   §3.2 wasm rustflags equal zedweb/zed-web:web/build.sh:45
ok   F6 no config above web/ redefines [target.wasm32-unknown-unknown]
ok   F6 RUSTFLAGS is not set (it would replace the config rustflags)
ok   F6 CARGO_ENCODED_RUSTFLAGS is not set (it would replace the config rustflags)
ok   F1 every wasm-reachable root [patch.crates-io] entry is repeated in web/Cargo.toml
ok   F2 web/Cargo.toml declares [profile.web-release] as zedweb/zed-web:Cargo.toml:1099 does
ok   F4 web/ cannot run -Z build-std yet, and the requirement is recorded in web/.cargo/config.toml
ok   F3 web/.cargo/config.toml records that it is discovered from the working directory
ok   V1 the four vendored packages keep their base name and version
ok   V2 agent_client_protocol_patch/src is byte-identical to crates.io 2.0.0 apart from 4 cfg lines in lib.rs
ok   V3 url_wasm differs from crates.io url 2.5.7 in src/lib.rs only (no rustfmt noise)
ok   V4 url_wasm's wasm branch selectors fire on wasm32-unknown-unknown only
ok   V5 wasm_thread_patch differs from git 0cf96c77 in the four §4 files only
ok   V5 wasm_thread_patch keeps 0cf96c77's native 'pub use std::thread::…' verbatim
ok   V6 smol_wasm's native half is a re-export of real smol, not a hand copy
ok   V7 every git dependency inside web/vendor is pinned by rev, not a mutable tag/branch
ok   V8 each vendored package is patched on the source kind the root workspace resolves it from, with no inert twin
ok   V9 web/Cargo.lock exists, so the web graph is pinned at all
ok   V9 the vendored agent-client-protocol is built by the companion crates the desktop builds it with
ok   V10 the web wasm32 graph is free of async-io/async-process/polling/errno/rustix (§4.1's wall)
ok   V11 cd web && cargo check --workspace --all-targets succeeds
ok   V11 cd web && cargo check --workspace --target wasm32-unknown-unknown succeeds
ok   M1 every dependency Phase 3a added to a crate manifest is used by that crate (or is a named §4.1/§5.5 addition)
ok   M2 every dependency Phase 3a wrote that the workspace already pins is inherited with workspace = true
ok   M3 every wasm clause Phase 3a wrote into a cfg gate changes the gate on at least one target

31 checks, 0 failures
```

`Cargo.lock` sha256 before and after isolation: `0d7228e7dbb8c643f30c9e45dd880002f3bfb14341532270e3d9049407e5b146` (unchanged).

### 4.5 Native `cargo check -p rpc -p remote --lib`

```
CARGO_TARGET_DIR=target/web-probe cargo check -p rpc -p remote --lib
…
    Checking rpc v0.1.0 (/Users/andy/go/src/github.com/poi5305/zed/crates/rpc)
    Checking remote v0.1.0 (/Users/andy/go/src/github.com/poi5305/zed/crates/remote)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 56.88s
```

Exit 0. Native Instant path still type-checks (`proto_client` + `remote_client` included).

`./script/clippy` was not run. No `--release --all-features`.

## 5. Where this stops

Done: §5.5 Instant for the zed-web swap set that exists on this tree (plus `proto_client.rs`), allowlist-preserving dual-cfg/comments, and eight small cfg gates.

Not done: the rest of the 174 `WASM_CFG` files (large mixed diffs, Phase 4 modules, signature changes). Next Phase 3b pass should take those file-by-file against current HEAD, still skipping §5.2 / §5.3 / Phase 4 crates.
