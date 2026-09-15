# Phase 2 final — vendor `tree-sitter` and `async-tar`

Last two of the nine §4 forks, under `web/vendor/` only. No first-party `crates/`
file was edited. Root `Cargo.toml` and root `Cargo.lock` were not edited.

- **Date:** 2026-09-15
- **Branch:** `andy/web-version`
- **Spec:** `docs/web-zed-plan.md` §4 (`tree_sitter_wasm`, `async_tar_wasm`), §4.1, §4.2, §4.3, §9, §9.1;
  `docs/phase2-dependency-wall.md` §1

`README.md` was not touched. The `> [!IMPORTANT]` two-line header is still present.

WASI SDK was **not** installed. `markdown` / `load-grammars` were **not** touched.
No wasm binary was built.

## What was created / changed

| Path | Action |
| --- | --- |
| `web/vendor/tree_sitter_wasm/` | new; git `43623ec` `lib/` + (a)–(d) |
| `web/vendor/async_tar_wasm/` | new; wrapper. Native `pub use` of `git_bd3ad6f`. Wasm stub |
| `web/vendor/async_tar_wasm/git_bd3ad6f/` | copy of git `bd3ad6f`, **not** zed-web `real/` |
| `web/Cargo.toml` | two members, two git-URL `[patch]` tables. 97 keys, profile, existing patches untouched |
| `web/check-workspace-isolation.sh` | V1 expected list 5 → 7 packages |
| `docs/phase2-final-vendoring.md` | this report |

`codegraph explore` was run first (`WasmStore`, `tree-sitter`). The index is first-party
Rust, not Cargo packages, so it returned `language::WasmStore` / `syntax_map` rather than
the vendored crates. Manifest facts are from grep / `git show` / the cargo git checkouts.

## 1. `tree-sitter` — redone on lock git `43623ec`

### Base

Not `zedweb/zed-web:crates/tree_sitter_wasm/` (stale snapshot `7f534862`, §4: **don't
import**). Copied from the cargo git checkout of the rev in the root lock:

```
~/.cargo/git/checkouts/tree-sitter-a21c02e4b1d6dd0c/43623ec/lib/
git -C …/43623ec rev-parse HEAD
43623ec9bf0eaaf7113285c46e8a09018f181b18
```

That matches root `Cargo.lock`:

```
source = "git+https://github.com/tree-sitter/tree-sitter?rev=43623ec9bf0eaaf7113285c46e8a09018f181b18#43623ec9bf0eaaf7113285c46e8a09018f181b18"
```

`rsync -a` of `lib/`, excluding `binding_web`, `lldb_pretty_printers`, `package.nix`, `.ccls`.
The git tree is a workspace; `lib/Cargo.toml` inherits `version` / `authors` / `edition` /
`cc` / `serde_json` / `tree-sitter-language` / `[lints]` via `.workspace = true`. Those
were inlined so the vendored crate is a standalone package. `tree-sitter-language` is
`"0.1.8"` (crates.io) so the existing web `[patch.crates-io]` redirects it to the same git
rev. That is flattening, not a source rewrite.

### (a) `build.rs` returns before `cc` on wasm32

`43623ec` compiles C for `wasm32-unknown` (`configure_wasm_build` then `cc`). zed-web
`binding_rust/build.rs:14-17` returns first. Inserted after the `stdlib-symbols.txt` copy
(still `src/wasm-stdlib/imports.txt`, the 43623ec path, not zed-web's `src/wasm/…`) and
**before** `let mut config = cc::Build::new()`:

```
// On WASM we rely on stub Rust bindings and do not compile the C library.
if target.starts_with("wasm32") {
    return;
}
```

Predicate is `target.starts_with("wasm32")` as specified, not 43623ec's
`wasm32-unknown`.

### (b) `wasm` feature does not pull wasmtime

43623ec: `wasm = [ "std", "wasmtime-c-api" ]`. Now `wasm = ["std"]`, matching
zed-web `Cargo.toml:73-75`.

### (c) `wasmtime-c-api` is native-only

Moved to `[target.'cfg(not(target_family = "wasm"))'.dependencies]` and made
unconditional on native (zed-web `:96-97`), not `optional` behind the `wasm` feature.
On wasm, `cargo metadata --filter-platform wasm32-unknown-unknown` gives tree-sitter
deps `cc, regex, serde_json, streaming-iterator, tree-sitter-language` — **no
wasmtime**.

### (d) `WasmStore` is a Rust stub on wasm

Native body is kept byte-identical in `binding_rust/wasm_language_native.rs` (`cmp`
against 43623ec `wasm_language.rs`). `wasm_language.rs` is a dispatcher:

```
#[cfg(not(target_family = "wasm"))]
include!("wasm_language_native.rs");
```

Wasm side is the zed-web stub shape (`:3` / `:157-158` / `:258-259`): a fake
`wasmtime::{Config, Engine}` so `crates/language` still typechecks
`WasmStore::new(&WASM_ENGINE)`, `load_language` returns `Err` ("WASM grammars are
not supported in the browser"), `Language::is_wasm` is `false`.

### Diff vs 43623ec `lib/` (excluding the TS/lldb dirs we never copied)

| Path | Why |
| --- | --- |
| `Cargo.toml` | flatten workspace inherit + (b)(c) |
| `binding_rust/build.rs` | (a) |
| `binding_rust/wasm_language.rs` | (d) dispatcher + stub |
| `binding_rust/wasm_language_native.rs` | extra file; byte-identical to 43623ec |

**rustfmt noise vs 43623ec: 0 files.** No `.rs` in `src/` or `binding_rust/` other than
the three above was rewritten. `wasm_language_native.rs` was not rustfmt'd.

## 2. `async-tar` — wrapper yes, zed-web `real/` no

### Base

Wrapper pattern from `zedweb/zed-web:crates/async_tar_wasm/` (`git show` of
`Cargo.toml` and `src/lib.rs`). Native payload is **not** that `real/` (crates.io
0.6.1, pax embedded-newline regression). Native payload is git
`bd3ad6f89df9a9da7a8535958756d6bf465936a0`:

```
~/.cargo/git/checkouts/async-tar-f6807244e52310d1/bd3ad6f
git -C …/bd3ad6f rev-parse HEAD
bd3ad6f89df9a9da7a8535958756d6bf465936a0
```

Root lock: `git+https://github.com/zed-industries/async-tar?rev=bd3ad6f…#bd3ad6f…`.

### Why native is a path re-export, not a git dep of that URL

`smol_wasm` can `pub use` a **git** smol because the workspace patches **crates.io**
`smol`; the git source is a different `CanonicalUrl` and does not recurse.

`async-tar` is a **git** source. The required patch is

```
[patch."https://github.com/zed-industries/async-tar"]
async-tar = { path = "vendor/async_tar_wasm" }
```

A wrapper dep on the same git URL is patched back onto the wrapper. Measured:

```
error: cyclic package dependency: package `async-tar v0.6.1 (…/web/vendor/async_tar_wasm)` depends on itself.
```

Tried two `CanonicalUrl` tricks from rust-lang/cargo#5478:

| Inner git URL | Result |
| --- | --- |
| `https://github.com/zed-industries////async-tar` | GitHub 404 |
| `https://github.com:443/zed-industries/async-tar` | canonicalised to the patched URL; same cycle |

There is **no crates.io `async-tar` in the graph** (one `[[package]]` in the root
lock, git only), so a crates-io twin patch is not required. V8 forbids that twin
when the root source is git anyway.

So native is:

```
#[cfg(not(target_family = "wasm"))]
pub use async_tar_real::*;
```

with `async_tar_real = { package = "async-tar-real", path = "git_bd3ad6f" }`.
`git_bd3ad6f/` is the bd3ad6f tree, directory **not** named `real/`.

Proof it is git `bd3ad6f`, not crates.io 0.6.1:

```
diff -rq -x .git -x .github -x Cargo.toml  <checkout/bd3ad6f>  git_bd3ad6f
# empty

cksum …/bd3ad6f/src/pax.rs  git_bd3ad6f/src/pax.rs
3339086585 5258
3339086585 5258
```

`test_parse_pax_with_binary_newlines` is present (the test zed-web's `real/`
deleted). `Cargo.toml` differs in two lines: `name = "async-tar-real"` (so the
graph does not contain two packages named `async-tar`) and `tokio-stream`
`0.1.18` → `0.1.17` so L1 does not upgrade the root lock's `tokio-stream`.

Cargo auto-adopts that nested path crate as an 11th web-workspace member.
`exclude = ["vendor/async_tar_wasm/git_bd3ad6f"]` does not stop path-dep
adoption. An empty `[workspace]` in the nested manifest is rejected as a second
workspace root inside `web/`. Left as-is; `--workspace --target wasm32` would
compile it. This run died earlier (see § Acceptance).

### Stub honesty

zed-web's stub returns `Ok(())` from `unpack` / `entries` / `append_*` (silent
success). Ours does not. Fallible I/O goes through:

```
fn unsupported<T>() -> io::Result<T> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "async-tar is not supported on wasm32",
    ))
}
```

`Archive::unpack`, `entries`, `entries_raw`, `Entry::unpack` / `unpack_in` /
`path`, `Header::{set_path,size,path,set_username,set_groupname}`,
`Builder::{append_*,finish,into_inner}` all return that error.
`Entries` as a `Stream` yields `Poll::Ready(Some(Err(…)))`, not an empty
`Ready(None)`. Constructors (`Archive::new`, `Header::new_gnu`) still succeed;
they do not perform I/O. `into_inner` on `Archive` returns `Err(self)`.

Wasm-reachable callers (`languages` / `node_runtime` / `dap`) only need
`Archive::new` + `unpack().await`; those typecheck against the stub.

## `web/Cargo.toml`

Members: added `vendor/tree_sitter_wasm` and `vendor/async_tar_wasm`. The previous
five vendor members, the three web crates, `[workspace.package]`, all **97**
`[workspace.dependencies]` keys, `[profile.web-release]`, and the existing
`[patch.crates-io]` / wasm_thread / alacritty tables are unchanged.

Added git-URL tables only (V8: both crates are git in the root lock; a
`[patch.crates-io]` twin would be inert and fail V8):

```
[patch."https://github.com/tree-sitter/tree-sitter"]
tree-sitter = { path = "vendor/tree_sitter_wasm" }

[patch."https://github.com/zed-industries/async-tar"]
async-tar = { path = "vendor/async_tar_wasm" }
```

`cargo metadata --filter-platform wasm32-unknown-unknown` from `web/`:

- `tree-sitter` → `…/web/vendor/tree_sitter_wasm/Cargo.toml`
- `async-tar` → `…/web/vendor/async_tar_wasm/Cargo.toml`, wasm deps `['futures-core']`
- **zero** packages whose name starts with `wasmtime`

## rustfmt noise

| Tree | rustfmt-only files vs base |
| --- | ---: |
| `tree_sitter_wasm` vs 43623ec `lib/` | **0** (three functional files + one extra include) |
| `async_tar_wasm/git_bd3ad6f` vs git `bd3ad6f` | **0** `.rs` (Cargo.toml two lines only) |
| `async_tar_wasm/src/lib.rs`, `wasm_language.rs`, `build.rs` | rustfmt-clean (`rustfmt --check`) |

## Acceptance — commands actually run

`CARGO_TARGET_DIR=/Users/andy/go/src/github.com/poi5305/zed/target/web-probe`.
One cargo process at a time. Root lock snapped before any cargo:

```
cp Cargo.lock /tmp/lock-before-p2final
cksum 2265851802 502232   # both files
```

### 1. `cd web && cargo check --workspace --target wasm32-unknown-unknown`

```
cd /Users/andy/go/src/github.com/poi5305/zed/web && \
  CARGO_TARGET_DIR=../target/web-probe \
  cargo check --workspace --target wasm32-unknown-unknown
```

**async-tar and wasmtime errors are gone.** The log contains:

```
Checking async-tar v0.6.1 (…/web/vendor/async_tar_wasm)
Compiling tree-sitter v0.27.0 (…/web/vendor/tree_sitter_wasm)
```

`grep wasmtime` on that log: **0 hits**. `grep 'could not compile \`async-tar\`|\`wasmtime\`'`: neither.

**New error, not fixed** (exit 101). `sqlez` cannot see `libsqlite3_sys` on wasm:

```
error[E0432]: unresolved import `libsqlite3_sys`
  --> crates/sqlez/src/connection.rs:10:5
error[E0432]: unresolved import `libsqlite3_sys`
  --> crates/sqlez/src/migrations.rs:11:5
error[E0432]: unresolved import `libsqlite3_sys`
  --> crates/sqlez/src/statement.rs:6:5
error: could not compile `sqlez` (lib) due to 3 previous errors
```

Grammar C / `'stdlib.h'` was **not reached**. This is not WASI evidence yet; it is
sqlez failing first. Phase 5 still has to install WASI for the 18 grammar crates
(§4.2) once the graph gets that far.

### 2. Root `Cargo.lock` vs the pre-edit snapshot

```
diff /tmp/lock-before-p2final /Users/andy/go/src/github.com/poi5305/zed/Cargo.lock
echo "diff_exit:$?"
```

```
diff_exit:0
```

Empty. Expected.

### 3. `./web/check-workspace-isolation.sh`

V1 list updated to seven packages (`async-tar 0.6.1`, `tree-sitter 0.27.0` added;
label "five" → "seven"). No other assertion in that script was edited.

V1, V8, V10, L1, F2, the nine-crate sources, 97-key-adjacent checks: **ok**.

V11 both red — **new errors, not fixed:**

```
FAIL V11 cd web && cargo check --workspace --all-targets succeeds
       expected: exit 0
       actual:   exit 101
       error[E0432]: unresolved import `livekit_protocol::enum_dispatch` …

FAIL V11 cd web && cargo check --workspace --target wasm32-unknown-unknown succeeds
       expected: exit 0
       actual:   exit 101
       error[E0432]: unresolved import `notify` error[E0432]: unresolved import `async_tar` error[E0432]: unresolved import `tempfile`
```

The wasm V11 first-three are a parallel-compile race (`grep -m3 '^error'`); the
full check log in (1) died on `sqlez`. Native V11's `livekit_protocol` is the
media stack L1 already exempts from lock agreement; `--all-targets` still
typechecks it. Neither is async-tar/wasmtime.

Script footer: `33 checks, 2 failures`.

### 4. `./web/check-refusals.sh`

```
ok   §5.3.1 RELEASE_CHANNEL is dev
ok   §5.3.2 terminal Shift+Click selection extension exists
ok   §5.3.3 recent_projects open_local_project PathPromptOptions.files is true
ok   §5.3.4 remote_server MultiWrite::flush uses send_blocking

4 checks, 0 failures
```

### 5. No wasm binary

Not attempted.

## Open constraints this phase did not lift

1. **WASI SDK** — still absent. Grammar C not observed this run because `sqlez`
   failed first. §4.2 still applies.
2. **Git re-export of a git-patched crate** — cargo cannot do the smol trick
   when `[patch]` is the git-URL table. Path copy of `bd3ad6f` is the substitute
   that keeps pax.rs honest.
3. **`async-tar-real` workspace membership** — nested path crate under `web/` is
   auto-adopted; `exclude` does not undo that.
4. **`sqlez` / `libsqlite3_sys` on wasm** — next compile wall after this phase's
   two crates. Not a vendor fork in §4.
