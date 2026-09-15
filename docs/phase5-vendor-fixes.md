# Phase 5 — vendor fixes (`tree_sitter_wasm` Send/Sync, `async_tar_wasm` real/)

- **Date:** 2026-09-15
- **Branch:** `andy/web-version`
- **Spec:** `docs/web-zed-plan.md` §4 (`tree_sitter_wasm`, `async_tar_wasm`), §4.2, §9
- **Scope:** only `web/vendor/tree_sitter_wasm/` and `web/vendor/async_tar_wasm/`
- **Not touched:** `crates/`, root `Cargo.toml` / `Cargo.lock`, `web/Cargo.toml`, `web/build.sh`, `web/check-*.sh`, `web/crates/`

`README.md` still starts with the two `> [!IMPORTANT]` lines. No commit, no `git stash` / `checkout` / `restore` / `reset`.

## Problem 1 — `*const TSLanguage` is not `Send`/`Sync`

### What the cfg is

`web/vendor/tree_sitter_wasm/binding_rust/lib.rs:4103-4124` (byte-identical to git `43623ec`):

```
#[cfg(not(target_family = "wasm"))]
unsafe impl Send for Language {}
#[cfg(not(target_family = "wasm"))]
unsafe impl Sync for Language {}
# ... same gate on LookaheadIterator, LookaheadNamesIterator, Parser ...
```

`Language` is `pub struct Language(*const ffi::TSLanguage);` (line 68). `Parser` is `NonNull<TSParser>` (line 180). `Node` / `Query` / `QueryCursor` / `Tree` / `TreeCursor` are already ungated.

`zedweb/zed-web:crates/tree_sitter_wasm/binding_rust/lib.rs` (~3915) has the same `unsafe impl`s **without** those gates. That snapshot is older (`7f534862`). Ours is `43623ec`.

### Why upstream gated it — not wasmtime

`git blame` on `43623ec` attributes the eight `#[cfg(not(target_family = "wasm"))]` lines to `0e2af0d8` (2026-08-14), PR **#5851**, message:

> Allow using Tree-sitter rust lib in multi-threaded web apps compiled to wasm32-unknown-unknown

The commit **added** the gates (Language/Parser/Lookahead* were previously `Send`/`Sync` on every target). It also made `Tree`/`Node` sendable across wasm workers by giving the C core an instance id:

```c
// lib/src/language.c
// Linear memory can be shared by multiple WebAssembly instances, but a
// module-defined global belongs to one instance. Assign each instance an ID
// from a counter in shared memory so trees can identify the function table
// that owns their language callbacks.
```

On `__wasm__`, `ts_tree_language` returns `ts_language_copy_without_callbacks` when `language_context_id` does not match the current instance — an unparseable stub. Language callbacks (`lex_fn`, external scanners) stay tied to the instance that created them. So upstream's rule is: **send `Tree`, do not send `Language` / `Parser`.**

That is independent of wasmtime. Wasmtime is `TREE_SITTER_FEATURE_WASM` / `WasmStore::load_language` on a **native** host. The cfg is `target_family = "wasm"` (compiling the bindings **to** wasm32).

### Does the wasmtime reason apply to our build?

**No.** §4.2 / phase-2 decision (b): `wasm = ["std"]` does not pull wasmtime. `wasmtime-c-api` is under `[target.'cfg(not(target_family = "wasm"))'.dependencies]`. `wasm_language.rs` on wasm32 is a stub whose `load_language` returns `Err("WASM grammars are not supported in the browser")`. `cargo metadata --filter-platform wasm32-unknown-unknown` from `web/`: **zero** packages named `wasmtime` or `wasmtime-c-api-impl`.

If wasmtime were the only issue, dropping the eight cfg lines would be sound: every `Language` we can construct is a `LanguageFn` static from a grammar crate, the same immutable `TSLanguage` native already marks `Send`/`Sync`.

### Does #5851 still apply to us?

**Yes.** `web/vendor/wasm_thread_patch/src/wasm32/mod.rs` packs `wasm_bindgen::module()` and `wasm_bindgen::memory()` and posts them to a `Worker`. The worker script does `wasm_bindgen({ module_or_path, memory })` / `init({ module_or_path, memory })` — a **new instance**, shared linear memory. That is the architecture #5851 wrote the gates for. Their own fixture README says the same: web workers, shared memory, parse on a background thread.

`crates/language` then does exactly what #5851 forbids at the type level:

- `static PARSERS: Mutex<Vec<Parser>>` (`language.rs:134`) — needs `Parser: Send`
- `AvailableGrammar` holds `tree_sitter::Language` inside `LanguageRegistry`, sent through `BackgroundExecutor::spawn` (`language_registry.rs:705`) — needs `Language: Send + Sync`

GPUI's background executor on wasm is wasm_thread. Ungating `Language`/`Parser` would let Zed invoke language callbacks on a worker instance whose function table / wasm globals are not the ones the `TSLanguage` was created with.

zed-web's ungated impls are not a safety review. They are a fork that predates `0e2af0d8`.

### Decision: do not add the `unsafe impl`s

The wasmtime reason does not apply. The #5851 reason does. Adding `unsafe impl Send/Sync for Language` (and Parser / Lookahead*) on this target would undo an upstream soundness fix for the threading model we actually ship. Stopped. `web/vendor/tree_sitter_wasm/binding_rust/lib.rs` is unchanged.

A sound fix is `crates/language` work (out of scope): keep `Language`/`Parser` on the creating instance; send `Tree`s; or compile the C core and use #5851's `copy_without_callbacks` protocol. Vendor-only `unsafe impl` is the wrong layer.

## Problem 2 — `async-tar-real` compiled on wasm

### Root cause

The wrapper **already** had the smol_wasm shape for the dependency:

```
[target.'cfg(not(target_family = "wasm"))'.dependencies]
async_tar_real = { package = "async-tar-real", path = "git_bd3ad6f" }
```

That is not enough. A path dep inside `web/` is auto-adopted as an 11th workspace member (`async-tar-real` is in `cargo metadata`'s `workspace_members`). `cargo check --workspace --target wasm32-unknown-unknown` builds every member for that target, including `git_bd3ad6f`, even when the wrapper does not name it on wasm.

On wasm, `git_bd3ad6f/src/header.rs` takes `#[cfg(any(windows, target_arch = "wasm32"))]` / `#[cfg(target_arch = "wasm32")]` arms whose `Cow<[u8]>` / `Cow<Path>` (no lifetime, edition 2024) are `error[E0277]: the size for values of type [u8] cannot be known at compilation time`.

### Why not git, like smol_wasm

smol can `git = "https://github.com/smol-rs/smol"` because the workspace patches **crates.io** `smol`. The git URL is a different `CanonicalUrl`.

`async-tar` is a **git** source. `web/Cargo.toml` has

```
[patch."https://github.com/zed-industries/async-tar"]
async-tar = { path = "vendor/async_tar_wasm" }
```

A wrapper dep on that URL is patched back onto the wrapper (cycle, measured in `docs/phase2-final-vendoring.md`; `https://github.com:443/…` canonicalises to the same URL). `exclude` in the workspace root does not undo path-dep adoption. An empty `[workspace]` in `git_bd3ad6f/Cargo.toml` is a second workspace root inside `web/`. `web/Cargo.toml` is out of scope.

So the smol git shape is **not available** for this crate without violating the scope or the patch. Spec is not contradictory; the substitute is a path copy of git `bd3ad6f` (not zed-web's crates.io `real/`).

### Fix (inside `git_bd3ad6f`)

Keep the wrapper's `not(wasm)` path dep. Stop the nested member from compiling the real sources or pulling their crates on wasm:

1. `git_bd3ad6f/src/lib.rs`: `#![cfg(not(target_family = "wasm"))]` — native sources unchanged; wasm is an empty crate.
2. All `[dependencies]` and `[dev-dependencies]` moved to `[target.'cfg(not(target_family = "wasm"))'.dependencies]` (unix / redox tables already target-gated).

After that, wasm `resolve` for `async-tar-real` has **no** deps. `async-std` left the wasm resolve graph (it was only pulled by this crate). `async-tar` (the wrapper) still depends only on `futures-core` on wasm.

`async-tar-real` remains a workspace member. That is the leftover §4 gap that cannot be closed without git (cycle) or editing `web/Cargo.toml`. It no longer compiles `pax.rs` / `header.rs` / async-std on wasm.

Native re-export is still `pub use async_tar_real::*;` of git `bd3ad6f` (pax embedded-newline fix intact).

### Stub honesty

Unchanged. Wasm I/O goes through `unsupported()` → `io::ErrorKind::Unsupported`, `"async-tar is not supported on wasm32"`. `Archive::unpack` / `entries` / `Entry::unpack` / `Builder::append_*` return that error. `Entries` as a `Stream` yields `Poll::Ready(Some(Err(…)))`, not an empty success. Constructors still succeed; they do no I/O. zed-web's stub returning `Ok(())` was not copied.

## Acceptance

### 1. `cd web && CARGO_TARGET_DIR=../target/web-probe cargo check --workspace --target wasm32-unknown-unknown`

`async-tar-real` compiled (empty crate). No `E0277` `[u8]` / `async-tar-real` errors. Log:

```
    Checking async-tar v0.6.1 (…/web/vendor/async_tar_wasm)
    Checking async-tar-real v0.6.1 (…/web/vendor/async_tar_wasm/git_bd3ad6f)
warning: `async-tar-real` (lib) generated 1 warning (1 duplicate)
   Compiling tree-sitter-json v0.24.8
error: failed to run custom build command for `tree-sitter-json v0.24.8`
  src/tree_sitter/parser.h:10:10: fatal error: 'stdlib.h' file not found
  CC_wasm32_unknown_unknown = None
```

Workspace check died on **tree-sitter-json** / missing WASI SDK (`CC_wasm32_unknown_unknown = None`). That is §4.2, not this assignment.

`*const TSLanguage` did not appear in that log because `language` was not reached. A follow-up (one cargo):

```
CARGO_TARGET_DIR=../target/web-probe cargo check -p language --lib --target wasm32-unknown-unknown
```

```
error: could not compile `language` (lib) due to 15 previous errors
```

| Count | Error |
| ---: | --- |
| 10 | `*const TSLanguage` not `Send`/`Sync` (`language_registry.rs` `AvailableGrammar` / `BackgroundExecutor::spawn`) |
| 1 | `NonNull<TSParser>` not `Send` (`language.rs:134` `PARSERS`) |
| 1 | `ForegroundExecutor::block_with_timeout` missing (`buffer.rs`) |
| 2 | `Grammar.ts_language` missing (`language.rs`, `syntax_map.rs`) |
| 1 | `ParseableLanguage: From<tree_sitter::Language>` (`language.rs`) |

The 10 + Parser rows are Problem 1, left in place. The last four are `crates/language` follow-up (also listed in `docs/phase5-blockers-5.md`), out of vendor scope.

### 2. Root `Cargo.lock`

```
cp Cargo.lock /tmp/lock-before-p5f
# … edits and cargo in web/ …
diff /tmp/lock-before-p5f Cargo.lock
# diff_exit:0
```

Empty. `cksum` both `2265851802 502232`.

### 3. `./web/check-workspace-isolation.sh` V10

```
ok   V10 the web wasm32 graph is free of async-io/async-process/polling/errno/rustix (§4.1's wall)
```

33 checks, 2 failures, both **V11** (pre-existing: native `livekit_protocol::enum_dispatch`; wasm `TSParser` / `alacritty_terminal::{event_loop,tty}` / `sysinfo`). V11 is not this assignment.

### 4. `./web/check-refusals.sh`

```
ok   §5.3.1 RELEASE_CHANNEL is dev
ok   §5.3.2 terminal Shift+Click selection extension exists
ok   §5.3.3 recent_projects open_local_project PathPromptOptions.files is true
ok   §5.3.4 remote_server MultiWrite::flush uses send_blocking

4 checks, 0 failures
```

## New errors (not fixed)

From the workspace wasm check, first fatal:

1. **`tree-sitter-json` C build** — `'stdlib.h' file not found`, `CC_wasm32_unknown_unknown = None`. §4.2 WASI SDK. Recurs for the other 17 grammar crates once json is past this. `web/build.sh` / env, not vendor.

From `-p language` (and V11's first wasm errors), still present after this round:

2. **`*const TSLanguage` / `NonNull<TSParser>` Send/Sync** — refused; see Problem 1.
3. **`block_with_timeout` / `ts_language` / `ParseableLanguage: From<Language>`** — `crates/language`, out of scope.
4. **`alacritty_terminal::{event_loop,tty}` / `sysinfo`** — `crates/terminal`, out of scope.

## Files changed

| Path | Change |
| --- | --- |
| `web/vendor/async_tar_wasm/Cargo.toml` | comment only (git cycle vs smol shape) |
| `web/vendor/async_tar_wasm/git_bd3ad6f/Cargo.toml` | deps + dev-deps behind `not(target_family = "wasm")` |
| `web/vendor/async_tar_wasm/git_bd3ad6f/src/lib.rs` | `#![cfg(not(target_family = "wasm"))]` |
| `web/vendor/tree_sitter_wasm/` | **none** |
| `docs/phase5-vendor-fixes.md` | this report |
