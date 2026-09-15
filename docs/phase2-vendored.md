# Phase 2 — vendored thin forks

Four thin forks under `web/vendor/`, registered as members of the web workspace.
No first-party `crates/` manifest was edited. Root `Cargo.toml` was not edited.

- **Date:** 2026-09-15
- **Branch:** `andy/web-version`
- **Spec:** `docs/web-zed-plan.md` §4, §4.1, §9; `docs/phase2-dependency-wall.md` §1, §3

`README.md` was not touched. The `> [!IMPORTANT]` two-line header is still present.

§4.1 target-cfg moves in `crates/fs`, `rpc`, `agent`, `edit_prediction`, `extension_host`, etc. were **not** done. Those are Phase 3 MANIFEST work.

`tree-sitter`, `lsp-types`, `which`, and `async-tar` were **not** vendored.

## What was created / changed

| Path | Action |
| --- | --- |
| `web/vendor/agent_client_protocol_patch/` | new; crates.io `agent-client-protocol` 2.0.0 + 4 `cfg` lines + native-only process deps |
| `web/vendor/url_wasm/` | new; crates.io `url` 2.5.7 + wasm `from_file_path` / `from_directory_path` / `to_file_path` |
| `web/vendor/smol_wasm/` | new; wrapper. Native `pub use` of real smol. Wasm drops `async-io` / `async-process` |
| `web/vendor/wasm_thread_patch/` | new; our git `0cf96c77` + drop atomic-wait + sqlez SQL-RPC worker JS |
| `web/Cargo.toml` | members + `[patch]` for the four packages (plus the existing `tree-sitter-language` patch) |
| `docs/phase2-vendored.md` | this report |

## Open question 3 — does `url` 2.5.8 already have wasm file-path support?

**No.** Local crates.io source
`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/url-2.5.8/src/lib.rs`
has **zero** `wasm32` and **zero** `target_arch`. `from_file_path`, `from_directory_path`, and `to_file_path` are still gated on

```
unix, windows, target_os = "redox", target_os = "wasi", target_os = "hermit"
```

`wasm32-unknown-unknown` is `target_os = "unknown"` and is not in that list, so the methods do not exist on wasm in 2.5.8. This phase therefore forks 2.5.7 (the version in our lockfile) and ports only the wasm branches.

## Per-crate port

### 1. `agent_client_protocol_patch`

- **Base:** crates.io `agent-client-protocol` 2.0.0 (checksum `6d87bc7769eba641753ba5dc52f73ec3765d51022c6753bf040967125ddc86a8`). Copied from the cargo registry, not from `zedweb/zed-web`.
- **`src/`:** 51 `.rs` files. `diff -rq` against the registry `src/` reports **only** `lib.rs`.
- **Ported lines** (the four `cfg`s from `git show zedweb/zed-web:crates/agent_client_protocol_patch/src/lib.rs`):

```
 #[cfg(not(target_family = "wasm"))]
 mod acp_agent;
 #[cfg(not(target_family = "wasm"))]
 pub use acp_agent::{AcpAgent, AcpAgentConfig, LineDirection};

 #[cfg(not(target_family = "wasm"))]
 mod stdio;
 #[cfg(not(target_family = "wasm"))]
 pub use stdio::Stdio;
```

- **Cargo.toml:** `async-io`, `async-process`, and `blocking` moved to
  `[target.'cfg(not(target_family = "wasm"))'.dependencies]`. Without that, gating the modules still leaves those crates in the wasm graph (`phase2-dependency-wall.md` §1, ACP row). No other manifest keys were rewritten (no rustfmt of the normalized crates.io toml).

### 2. `url_wasm`

- **Base:** crates.io `url` 2.5.7 (checksum `08bc136a29a3d1758e07a9cca267be308aeebf5cfd5a10f3f67ab2097683ef5b`).
- **`diff -rq` against the registry crate reports only `src/lib.rs`.**
- **Ported:** `target_arch = "wasm32"` added to the three file-path `any(...)` lists; wasm bodies for `from_file_path` / `from_directory_path` / `to_file_path` taken from zed-web (`Url::parse("file://…")` / `PathBuf::from(self.path())`); native bodies left as 2.5.7 wrote them, wrapped in `cfg(not(wasm32))`.
- zed-web's url fork was `+82 / −58` with rustfmt noise. This port is `+76 / −47` on `lib.rs` only (the extra lines are the wasm branches and the native wrappers, not a reformat of host.rs / parser.rs / …).

### 3. `smol_wasm`

- **Not a fork of smol's sources.** Package name stays `smol` 2.0.2 so `[patch.crates-io] smol` can point here.
- **Native:** `pub use smol_real::*` with
  `smol_real = { package = "smol", git = "https://github.com/smol-rs/smol", tag = "v2.0.2" }`
  (peeled commit `a1e642196803fc5dff56f1e04e867bfc024966bd`). Git, not crates.io, because `[patch.crates-io] smol = { path = "vendor/smol_wasm" }` would recurse if this crate depended on crates.io `smol`. Native `spawn` is therefore the real crate's, not zed-web's hand copy.
- **Wasm graph job (the wall):** `async-io` and `async-process` are **not** in the wasm dependency table. That is how `errno` / `polling` disappear (`phase2-dependency-wall.md` §1).
- **Wasm stubs:** `net.rs` copied from zed-web. `fs.rs` and `process.rs` copied from zed-web. `lib.rs` wasm `Timer` / `Async` / `Unblock` / `block_on` copied. `spawn.rs` is the zed-web wasm stub only.
- **Limitation (no clean path for the RPC client this phase):** zed-web's `fs` / `process` call `wasm_rpc::RpcClient` (`call`, `call_void`, `on_notification`, `subscribe_reconnect`). `wasm_rpc` is Phase 4 and does not exist in this tree. A path dep on it would make `cargo metadata --no-deps` fail. `src/rpc.rs` is a same-signature stand-in that returns errors; Phase 4 should replace that module with `pub use wasm_rpc::RpcClient`. The wall-critical Cargo.toml edge does not depend on that client.

### 4. `wasm_thread_patch`

- **Base:** the fork we already use, `zed-industries/wasm_thread` rev `0cf96c7708dfb97ccf3da50347e25edcf75d6937`. Copied from the cargo git checkout. `.git`, `.github`, `rust-toolchain.toml`, and `examples-wasm-pack` were not copied.
- **Not swapped for zed-web's tree.** `diff -rq` against `0cf96c77` reports only four files:
  - `src/lib.rs` — removed `#![cfg_attr(target_arch = "wasm32", feature(stdarch_wasm_atomic_wait))]`
  - `src/wasm32/signal.rs` — dropped `memory_atomic_notify` / `memory_atomic_wait32`; `wait()` spins. Import style of the 0cf96c77 file kept (no zed-web rustfmt of `Waker`).
  - `src/wasm32/js/web_worker.js` and `web_worker_module.js` — `prepareSqlRpcBridge()` from zed-web injected; `onmessage` now `Promise.all([init, prepareSqlRpcBridge])`. Our original `import init, { wasm_thread_entry_point }` spacing was kept on the module worker (zed-web had `{wasm_thread_entry_point}`).

Native `pub use std::thread::*` is unchanged.

## `web/Cargo.toml`

Members are the four vendor directories. `[patch.crates-io]` maps `agent-client-protocol`, `url`, `smol`, and `wasm_thread` onto them, and still repeats `tree-sitter-language`. The git-URL table for `https://github.com/zed-industries/wasm_thread` is present because workspace `wasm_thread` is a git source (`phase2-dependency-wall.md` §3). The other git-URL patches (tree-sitter, async-tar, lsp-types) are **not** here — those crates were not vendored.

## §9 checks — commands actually run, output as captured

No `./script/clippy`. No `--release --all-features`. No `cargo tree`. No desktop bundle.

`Cargo.lock` was copied to `/tmp/lock-before-phase2` **before** any vendor copy (`501613` bytes, `cksum 1343995929`, identical to the working copy at that moment).

Cargo invocations that can write artefacts used
`CARGO_TARGET_DIR=/Users/andy/go/src/github.com/poi5305/zed/target/web-probe`.

### 1. Root `Cargo.lock` vs the pre-edit snapshot

```
diff /tmp/lock-before-phase2 /Users/andy/go/src/github.com/poi5305/zed/Cargo.lock
echo "diff_exit:$?"
```

```
diff_exit:0
```

Empty diff. No `web/` path in the lockfile.

### 2. The nine crate `source` fields (verbatim)

| Crate | version | `source` |
| --- | --- | --- |
| `tree-sitter` | 0.27.0 | `git+https://github.com/tree-sitter/tree-sitter?rev=43623ec9bf0eaaf7113285c46e8a09018f181b18#43623ec9bf0eaaf7113285c46e8a09018f181b18` |
| `lsp-types` | 0.95.1 | `git+https://github.com/zed-industries/lsp-types?rev=f1783e63a7f4eb4397bf51d4148b4895a1f7ab16#f1783e63a7f4eb4397bf51d4148b4895a1f7ab16` |
| `which` | 8.0.5 | `registry+https://github.com/rust-lang/crates.io-index` |
| `url` | 2.5.7 | `registry+https://github.com/rust-lang/crates.io-index` |
| `smol` | 2.0.2 | `registry+https://github.com/rust-lang/crates.io-index` |
| `async-tar` | 0.6.1 | `git+https://github.com/zed-industries/async-tar?rev=bd3ad6f89df9a9da7a8535958756d6bf465936a0#bd3ad6f89df9a9da7a8535958756d6bf465936a0` |
| `alacritty_terminal` | 0.26.1-dev | `git+https://github.com/zed-industries/alacritty?rev=4c129667ce56611becdc82de6e28218c80e2e88f#4c129667ce56611becdc82de6e28218c80e2e88f` |
| `agent-client-protocol` | 2.0.0 | `registry+https://github.com/rust-lang/crates.io-index` |
| `wasm_thread` | 0.3.3 | `git+https://github.com/zed-industries/wasm_thread?rev=0cf96c7708dfb97ccf3da50347e25edcf75d6937#0cf96c7708dfb97ccf3da50347e25edcf75d6937` |

### 3. Root `Cargo.toml` untouched this phase

`git diff --stat -- Cargo.toml` still shows only the Phase 1 line `exclude = ["web"]`. This phase did not edit it.

### 4. `cd web && cargo metadata --no-deps --format-version 1`

```
cd /Users/andy/go/src/github.com/poi5305/zed/web && \
  CARGO_TARGET_DIR=/Users/andy/go/src/github.com/poi5305/zed/target/web-probe \
  cargo metadata --no-deps --format-version 1
```

Exit 0. stderr empty. No `web/Cargo.lock` written. JSON 23800 bytes. Parsed:

```
workspace_root: /Users/andy/go/src/github.com/poi5305/zed/web
target_directory: /Users/andy/go/src/github.com/poi5305/zed/target/web-probe
packages: 4
workspace_members:
 - path+file:///Users/andy/go/src/github.com/poi5305/zed/web/vendor/agent_client_protocol_patch#agent-client-protocol@2.0.0
 - path+file:///Users/andy/go/src/github.com/poi5305/zed/web/vendor/url_wasm#url@2.5.7
 - path+file:///Users/andy/go/src/github.com/poi5305/zed/web/vendor/smol_wasm#smol@2.0.2
 - path+file:///Users/andy/go/src/github.com/poi5305/zed/web/vendor/wasm_thread_patch#wasm_thread@0.3.3
resolve: null
```

`workspace_root` is still `…/zed/web`.

### 5. Root desktop metadata still resolves, and does not adopt `web/vendor`

```
cd /Users/andy/go/src/github.com/poi5305/zed && \
  CARGO_TARGET_DIR=/Users/andy/go/src/github.com/poi5305/zed/target/web-probe \
  cargo metadata --no-deps --format-version 1
```

Exit 0.

```
workspace_root: /Users/andy/go/src/github.com/poi5305/zed
packages: 257
workspace_members: 257
packages under zed/web: 0
```

## `git status --short`

After the files above landed, including this report:

```
 M Cargo.toml
 M docs/web-zed-plan.md
?? docs/phase0-rebase-cost.md
?? docs/phase0b-wasm-report.md
?? docs/phase1-review-round1.md
?? docs/phase1-workspace.md
?? docs/phase2-dependency-wall.md
?? docs/phase2-vendored.md
?? web/
```

This phase added `web/vendor/**`, updated `web/Cargo.toml`, and wrote `docs/phase2-vendored.md`. `Cargo.toml` (root) and the other `docs/` / `web/` entries were already dirty or untracked before this work. No `crates/` file was modified. No commit was made.
