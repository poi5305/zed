# Phase 2 — How zed-web makes the wasm dependency wall disappear

Research only. No `.rs` or `Cargo.toml` in this tree was changed. Evidence is `git show` / `git diff` of ref `zedweb/zed-web` (`4ef3a8dcc5619f5362351a2cbdcc0e34403a311b`) against merge-base `fecc3273ed32643c2ea1b04a74c8780e2c9ffaf8`. Compiler facts are taken from `docs/phase0b-wasm-report.md`; those `cargo check`s were not re-run.

**Method.** Two `codegraph explore` calls were made first (crate names plus the wasm workspace). The index is Rust symbols, not Cargo packages, so it returned `RemoteClient` / `SumTree` / `ProjectPanel::trash` rather than `getrandom` / `polling` / `wasmtime`. Everything below is from `zedweb/zed-web` manifests, forks, `web/build.sh`, and `Cargo.lock`.

**Graph that actually builds.** zed-web does not compile `-p rpc` or `-p remote` for wasm. It compiles `-p zed_web_workspace` (`web/build.sh:53-58`). That binary’s direct deps are `crates/zed_web_workspace/Cargo.toml`. Several wall crates die because an *intermediate* crate is missing from that graph, not because `rpc`/`remote` themselves were patched.

Means, as specified:

| # | Means |
| --- | --- |
| (1) | `[patch]` → vendored fork |
| (2) | Cargo.toml feature / `default-features = false` |
| (3) | dep moved to `[target.'cfg(not(target_family = "wasm"))'.dependencies]` |
| (4) | never enters the wasm graph, because an intermediate dep is cfg’d out — intermediate named |
| (5) | `RUSTFLAGS` / `-Z build-std` |
| (6) | other (named) |

---

## 1. Per-crate disposition

| Crate | Means | zed-web evidence | What to copy |
| --- | --- | --- | --- |
| **`getrandom` 0.2.16** | **(2)** | Dummy wasm-only dep so the `js` feature unifies onto every 0.2 user: `crates/rpc/Cargo.toml:42-43` `getrandom_02 = { package = "getrandom", version = "0.2", features = ["js"] }`. Lockfile `getrandom 0.2.16` then lists `js-sys` / `wasm-bindgen` (features actually on). | On the wasm workspace (or any wasm-reachable crate, `rpc` is enough because features unify): depend on `getrandom` 0.2 with `features = ["js"]`. Do **not** expect `RUSTFLAGS` to fix 0.2; that crate wants the Cargo feature. |
| **`getrandom` 0.3.4** | **(2) + (5)** | Feature: `crates/rpc/Cargo.toml:44` and `crates/gpui/Cargo.toml:112-113` `getrandom = { version = "0.3.4", features = ["wasm_js"] }`. Cfg: `web/build.sh:45` `--cfg getrandom_backend="wasm_js"` **and** `.cargo/config.toml:23-24` `[target.wasm32-unknown-unknown] rustflags = ["--cfg", "getrandom_backend=\"wasm_js\""]`. Phase 0b already showed the feature alone is not enough: `getrandom-0.3.4/src/backends.rs` selects the `wasm_js` backend only when that `--cfg` is set, then still `compile_error!`s unless the `wasm_js` feature is on. | Copy both. Put the cfg in `web/.cargo/config.toml` for `cargo check`, and **repeat it inside `web/build.sh`’s `RUSTFLAGS=`** because env `RUSTFLAGS` replaces `target.*` rustflags rather than concatenating (plan §3.2). Also enable `wasm_js` on at least one wasm-reachable crate (`gpui` or `rpc`). |
| **`errno` 0.3.14** | **(4)** intermediate: **`async-io` → `polling` → `rustix` → `errno`**, killed by **`smol_wasm`** | `crates/smol_wasm/Cargo.toml:15-20` puts `async-io` / `async-process` only under `cfg(not(target_family = "wasm"))`. Lockfile: `rustix 0.38.44` and `rustix 1.1.4` are the packages that depend on `errno 0.3.14`; `async-io 2.6.0` depends on `polling`. On wasm, `smol` is the patch (`Cargo.toml:999`) and does not take `async-io`. Extra cuts of the same edge: `crates/alacritty_terminal/Cargo.toml:27-30` (`polling` native-only); `crates/agent_client_protocol_patch/Cargo.toml:41-44` (`async-io` / `async-process` native-only); `crates/fs/Cargo.toml:45-52` (`notify` native-only). `crates/net/Cargo.toml:21-22` keeps `async-io` on **Windows only**, so `askpass` → `net` does not reintroduce polling on wasm. | Take `smol_wasm` (already in §4). Do not vendor `errno`. The `agent` / `agent_servers` comments that “tempfile pulls errno” (`crates/agent/Cargo.toml:87-90`, `crates/agent_servers/Cargo.toml:67-69`) are about the **unix** tempfile path (`tempfile-3.27.0` rustix/getrandom are `cfg(any(unix, …))`, not `wasm32-unknown-unknown`). tempfile can stay in `util` / `remote` / `project` on wasm without pulling errno. |
| **`polling` 3.11.0** | **(4)** same intermediate as errno, plus **(3)** on alacritty, plus **(1)** ACP patch | Primary: `smol_wasm` does not depend on `async-io` on wasm (`crates/smol_wasm/Cargo.toml:15-20`). Direct `polling` dep: `crates/alacritty_terminal/Cargo.toml:27-30`. Comment on the ACP patch: root `Cargo.toml:1005` “Browser wasm: no async-process / polling”. Lockfile still lists `smol` → `async-io` → `polling` because a lockfile is target-agnostic; the wasm unit does not build those crates. | Same as errno: `smol_wasm` + alacritty `cfg`s (already in §4) + ACP patch (already in §4). No `polling` fork. |
| **`zstd-sys` 2.0.16** | **(3)** | Direct `zstd` users in the web binary’s graph are moved native-only: `crates/rpc/Cargo.toml:39-40`, `crates/agent/Cargo.toml:87-89`, `crates/edit_prediction/Cargo.toml:75-76`. Lockfile `zstd` reverse-deps: `agent`, `crashes`, `edit_prediction`, `rpc`, `zip`. `crashes` is not in `zed_web_workspace`. `zip` is pulled by `webrtc-sys-build` (native livekit), not the wasm binary. WASI SDK is **not** how they clear this one. | Copy the three `[target.'cfg(not(target_family = "wasm"))'.dependencies] zstd` moves. Gate any new wasm-reachable `zstd` the same way. No `zstd-sys` fork. |
| **`tree-sitter-json` 0.24.8** | **(3)** on most consumers, **(2)** `languages` default-features, **(6)** WASI SDK for the one consumer left on | Gated native-only: `crates/debugger_ui/Cargo.toml:77-78`, `crates/keymap_editor/Cargo.toml:45-46`, `crates/settings_json/Cargo.toml:25-27`. `crates/settings/Cargo.toml:41-42` moves `migrator` (which depends on `tree-sitter-json`) to `not(wasm)`. Workspace `languages = { path = "crates/languages", default-features = false }` at root `Cargo.toml:404` so `zed_web_workspace` does not enable `load-grammars` / `grammars` C crates. **Still on the wasm graph:** `crates/tasks_ui/Cargo.toml:26-27` and `crates/tasks_ui/src/tasks_ui.rs:30,37,45` (`parser.set_language(&tree_sitter_json::LANGUAGE.into())`). That C build is why `web/build.sh:47-51` sets `CC_wasm32_unknown_unknown` to WASI clang and `CFLAGS_wasm32_unknown_unknown=-isystem …/wasi-sysroot/include/wasm32-wasi` — this is the `'stdlib.h' file not found` fix. | Copy the `not(wasm)` gates and `languages` `default-features = false`. Either also gate `tasks_ui` the way `debugger_ui` did, **or** keep WASI SDK as a required wasm build input. zed-web kept the dep and used WASI. No `tree-sitter-json` fork. |
| **`tree-sitter` git `43623ec`** | **(1)** | Git-source patch `Cargo.toml:987-988` and crates-io patch `Cargo.toml:1003`: `tree-sitter = { path = "crates/tree_sitter_wasm" }`. `crates/tree_sitter_wasm/binding_rust/build.rs:14-17` — “On WASM we rely on stub Rust bindings and do not compile the C library”; `if target.starts_with("wasm32") { return; }`. That skips `wasm_store.c` (Phase 0b / plan open question 5). `wasm` feature is only `["std"]` (`crates/tree_sitter_wasm/Cargo.toml:73-75`), not wasmtime. `wasmtime-c-api` is `cfg(not(target_family = "wasm"))` (`:96-97`). WasmStore API is a Rust stub: `binding_rust/wasm_language.rs:3` / `:157-158` / `:258-259`. **This fork is not our git `43623ec`.** `tree_sitter_wasm/Cargo.toml:16` is version `0.27.0`; plan §4 already called it an older snapshot (`7f534862`). | Do **not** import the stale snapshot as-is (plan §4 still holds). Redo on `43623ec`: (a) `build.rs` must return before `cc` on `wasm32`; (b) `wasm` feature must not pull wasmtime; (c) `wasmtime-c-api` native-only; (d) stub `WasmStore`. WASI SDK is not what clears this crate’s C failure — the early `return` is. |
| **`async-tar` git `bd3ad6f`** | **(1)** plus **(3)** on some users | Git-source patch `Cargo.toml:984-985` and crates-io patch `Cargo.toml:1004`: `async-tar = { path = "crates/async_tar_wasm" }`. Wrapper: `crates/async_tar_wasm/Cargo.toml:8-12` real crate only on `not(wasm)`; wasm depends on `futures-core`. `crates/async_tar_wasm/src/lib.rs:3-7` `pub use async_tar_real::*` vs `stub`. Native-only direct deps: `crates/fs/Cargo.toml:45-46`, `crates/http_client/Cargo.toml:14-16,41-44` (`github-download`’s `async-tar` is behind `not(wasm)` even when the feature is on). **Still in the wasm graph, as the stub:** `crates/languages/Cargo.toml:40`, `crates/node_runtime/Cargo.toml:21`, `crates/dap/Cargo.toml:28`. | Take the wrapper pattern (plan §4: wrapper yes, `real/` no — re-export our git `bd3ad6f`). Also copy the `fs` / `http_client` target gates. The patch is load-bearing because `languages` / `node_runtime` / `dap` stay unconditional. |
| **`trash` git `41c6c800`** | **(3)** | `crates/fs/Cargo.toml:45-53`: `trash = { git = "…/trash-rs", rev = "41c6c800…" }` lives only under `[target.'cfg(not(target_family = "wasm"))'.dependencies]`. Lockfile reverse-dep: `fs` only. `zed_web_workspace` still depends on `fs`; wasm `fs` simply does not name `trash`. | Move `trash` to `not(wasm)` in `crates/fs/Cargo.toml`. No trash fork. Any `fs` method that called `trash` already has to be cfg’d or RPC’d in Phase 3; that is source, not this wall. |
| **`wasmtime` 48.0.1** | **(1)** on the tree-sitter edge, **(3)/(4)** on the extension_host edge | Phase 0b’s `remote` hit wasmtime through **`language` → `tree-sitter` with `features = ["wasm"]`**. On zed-web that feature no longer pulls wasmtime (`tree_sitter_wasm/Cargo.toml:73-75`, `:96-97`). Second edge: `extension_host` still depends on `wasmtime` / `wasmtime-wasi` unconditionally (`crates/extension_host/Cargo.toml:56-57`), but it is native-only from every wasm-reachable reverse-dep: `crates/language_models/Cargo.toml:62-70`, `crates/extensions_ui/Cargo.toml:49-50`, `crates/agent_ui/Cargo.toml:120-121`. `zed_web_workspace` does not depend on `extension_host`. Lockfile wasmtime reverse-deps that are first-party: `edit_prediction_cli`, `extension_cli`, `extension_host`, `tree-sitter` — none of the first three are in the wasm binary graph. | Fork/redo tree-sitter so `wasm` ≠ wasmtime (already in §4). Gate `extension_host` behind `not(wasm)` on `language_models` / `extensions_ui` / `agent_ui`. Do not vendor wasmtime. |

`getrandom` 0.4 exists in the lockfile (`tempfile` → `getrandom 0.4.1`) but tempfile’s getrandom is `cfg(any(unix, windows, target_os = "wasi"))`, so it is **not** a wasm wall.

---

## 2. Does §4 need more forks?

**Zero additional vendored forks** for the ten wall crates.

The nine already cover the crate-shaped holes (`tree-sitter`, `async-tar`, `smol`, `alacritty_terminal`, `agent-client-protocol`). The other six wall crates are cleared without a fork.

What §4 actually omitted (work items, not forks):

1. **WASI SDK v25** as a wasm build input — `web/build.sh:10,47-51`, `script/download-wasi-sdk` (clang + `wasi-sysroot` includes). Load-bearing for `tree-sitter-json` via `tasks_ui`.
2. **`getrandom` 0.2 `js` + 0.3 `wasm_js` + `--cfg getrandom_backend="wasm_js"`** — not a fork.
3. **Target-cfg list** that is not a fork: `trash` (`fs:45-53`), `zstd` (`rpc:39-40`, `agent:87-89`, `edit_prediction:75-76`), `extension_host` (`language_models:64-70`, `extensions_ui:49-50`, `agent_ui:120-121`), `migrator` (`settings:41-42`), `tree-sitter-json` (`debugger_ui:77-78`, `keymap_editor:45-46`, `settings_json:25-27`).
4. **`languages = { …, default-features = false }`** at workspace level (`Cargo.toml:404`) so `zed_web_workspace` does not enable `load-grammars`.
5. **`tasks_ui` still depends on `tree-sitter-json`.** Either gate it like `debugger_ui`, or keep WASI. zed-web kept WASI.
6. **`smol_wasm`’s real job for this wall** is dropping `async-io` / `async-process` on wasm (errno + polling), not only the RPC fs/process stubs §4 described.
7. **Git-URL `[patch]` tables** in addition to `[patch.crates-io]`: tree-sitter, async-tar, lsp-types, wasm_thread (see §3). Workspace `path =` for `alacritty_terminal` is **not** a `[patch]` (`Cargo.toml:531`).

`jsonschema = { version = "0.37.0", default-features = false }` (`Cargo.toml:674`) and `instant = { version = "0.1", features = ["wasm-bindgen"] }` (`crates/zed_web_workspace/Cargo.toml:117-120`) showed up in the same manifest pass. They are not in the Phase 0b wall table.

---

## 3. Complete `[patch]` list in zed-web root `Cargo.toml`

Line numbers from `zedweb/zed-web:Cargo.toml`.

### Added vs merge-base `fecc3273`

| Lines | Table | Package | Replacement |
| --- | ---: | --- | --- |
| 978-979 | `[patch."https://github.com/zed-industries/lsp-types"]` | `lsp-types` | `path = "crates/lsp_types_wasm"` |
| 981-982 | `[patch."https://github.com/zed-industries/wasm_thread"]` | `wasm_thread` | `path = "crates/wasm_thread_patch"` |
| 984-985 | `[patch."https://github.com/zed-industries/async-tar"]` | `async-tar` | `path = "crates/async_tar_wasm"` |
| 987-988 | `[patch."https://github.com/tree-sitter/tree-sitter"]` | `tree-sitter` | `path = "crates/tree_sitter_wasm"` |
| 991 | `[patch.crates-io]` | `wasm_thread` | `path = "crates/wasm_thread_patch"` |
| 999 | `[patch.crates-io]` | `smol` | `path = "crates/smol_wasm"` |
| 1000 | `[patch.crates-io]` | `which` | `path = "crates/which_wasm"` |
| 1001 | `[patch.crates-io]` | `lsp-types` | `path = "crates/lsp_types_wasm"` |
| 1002 | `[patch.crates-io]` | `url` | `path = "crates/url_wasm"` |
| 1003 | `[patch.crates-io]` | `tree-sitter` | `path = "crates/tree_sitter_wasm"` |
| 1004 | `[patch.crates-io]` | `async-tar` | `path = "crates/async_tar_wasm"` |
| 1005-1006 | `[patch.crates-io]` | `agent-client-protocol` | `path = "crates/agent_client_protocol_patch"` |

Git-URL tables exist because workspace `tree-sitter` / `async-tar` / `lsp-types` / `wasm_thread` are **git sources**; `[patch.crates-io]` does not catch them. Both sides are required.

**Not a `[patch]`:** `alacritty_terminal = { path = "crates/alacritty_terminal" }` at `Cargo.toml:531` (workspace.dependency override).

### Already at merge-base (unchanged; still present)

From `fecc3273:Cargo.toml:964-977`, same lines in zed-web at `995-1012` (with the new entries inserted above `calloop`):

| Package | Source |
| --- | --- |
| `tree-sitter-language` | git `tree-sitter/tree-sitter` rev `43623ec9bf0eaaf7113285c46e8a09018f181b18` |
| `async-process` | git `zed-industries/async-process` rev `0b6d6713570af61806e1e5cb40e0f757cb93fd9d` |
| `async-task` | git `smol-rs/async-task` rev `b4486cd71e4e94fbda54ce6302444de14f4d190e` |
| `windows-capture` | git `zed-industries/windows-capture` rev `f0d6c1b6691db75461b732f6d5ff56eed002eeb9` |
| `calloop` | git `zed-industries/calloop` |
| `livekit` | git `zed-industries/livekit-rust-sdks` rev `d0e27be0cdad89eadab3e36207cda0a2b6e359ee` |
| `libwebrtc` | same livekit rev |
| `notify` | git `zed-industries/notify` rev `0890bbb8ca40a4b5d1f67031698dd7918b37d991` |
| `notify-types` | same notify rev |
| `webrtc-sys` | same livekit rev |

If the web workspace is split off as §3.2, **every row that the wasm graph still names must be repeated** in `web/Cargo.toml`. Patches do not cross workspaces (plan §3.2). The livekit / notify / calloop rows are native-only in practice; `tree-sitter-language` is not — grammar crates.io packages still need it.

---

## 4. `web/build.sh` `RUSTFLAGS` and the rest of the wasm compiler env

Single assignment, `web/build.sh:45`:

```
--cfg getrandom_backend="wasm_js"
-C target-feature=+atomics,+bulk-memory,+mutable-globals
-C link-arg=--shared-memory
-C link-arg=--import-memory
-C link-arg=--initial-memory=134217728
-C link-arg=--max-memory=4294967296
-C link-arg=--export=__heap_base
-C link-arg=--export=__stack_pointer
-C link-arg=--export=__tls_size
-C link-arg=--export=__tls_align
-C link-arg=--export=__tls_base
-C link-arg=--export=__wasm_init_tls
-C link-arg=--export=__wasm_call_ctors
```

| Flag | What it fixes |
| --- | --- |
| `--cfg getrandom_backend="wasm_js"` | **Wall crate `getrandom` 0.3.4** (and 0.4 if it were compiled). Selects the wasm_js backend; still needs the `wasm_js` Cargo feature. |
| `-C target-feature=+atomics,+bulk-memory,+mutable-globals` | Wasm threads / `SharedArrayBuffer`. Not a Phase 0b wall crate. |
| `--shared-memory` | Linker: one shared memory for atomics. Threads. |
| `--import-memory` | Memory is imported; `web/scripts/patch-wasm-bindgen-memory.sh` (`build.sh:73-74`) rewrites the JS side. Threads / 128 MB heap. |
| `--initial-memory=134217728` | 128 MiB initial heap so startup does not grow shared memory repeatedly (`web/README.md`). |
| `--max-memory=4294967296` | 4 GiB cap. |
| `--export=__heap_base`, `__stack_pointer` | Allocator / stack for the imported memory setup. |
| `--export=__tls_size`, `__tls_align`, `__tls_base`, `__wasm_init_tls` | TLS for wasm threads. |
| `--export=__wasm_call_ctors` | Run static constructors after the module instantiates. |

Not `RUSTFLAGS`, but on the same wasm compile (`build.sh:47-58`):

| Setting | What it fixes |
| --- | --- |
| `CC_wasm32_unknown_unknown=$WASI_SDK/bin/clang` | Host clang “No available targets are compatible with triple `wasm32-unknown-unknown`” (`zstd-sys` in Phase 0b; any `cc` crate that is still in the graph). Combined with the early return in `tree_sitter_wasm`, zstd is gone, so the remaining C unit is **`tree-sitter-json`**. |
| `CFLAGS_wasm32_unknown_unknown=-isystem …/include/wasm32-wasi` | `'stdlib.h' file not found` in `tree-sitter-json`. |
| `-Z build-std=std,panic_abort` (nightly, `build.sh:53-58`) | Rebuild libstd with the atomics target-features. Not a Phase 0b wall crate; required for the threaded wasm std. |
| `--profile web-release` (`Cargo.toml:1099-1105` in zed-web) | Size; not a wall. |

`.cargo/config.toml:23-24` only has the getrandom cfg. `export RUSTFLAGS=…` in `build.sh:45` **replaces** that `target.*` list. The getrandom cfg is duplicated in the env string so the production build still has it. A bare `cargo +nightly check -p zed_web_workspace --target wasm32-unknown-unknown -Z build-std=std,panic_abort` (as in `web/README.md`) uses the config.toml cfg and does **not** get atomics/shared-memory unless the caller exports `RUSTFLAGS`.

---

## 5. One-sentence conclusion

**§3.2’s second workspace remains the right isolation story for *our* desktop build, but it is not how zed-web itself works and it is not sufficient to clear this wall:** zed-web patches the **root** `Cargo.toml`; the nine forks plus a pile of target-cfg / feature / WASI / `RUSTFLAGS` wiring are what actually remove `getrandom` / `errno` / `polling` / `zstd-sys` / `tree-sitter*` / `async-tar` / `trash` / `wasmtime` from the wasm compile, and several of those disappear only because `zed_web_workspace` never depends on `extension_host` and because `smol_wasm` drops `async-io`.
