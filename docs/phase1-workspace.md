# Phase 1 — empty second Cargo workspace

Stand up `web/` as a virtual workspace and prove the desktop graph is unchanged.
No vendored crates, no `[patch]` path to a directory that does not exist, no `.rs`.

- **Date:** 2026-09-15
- **Branch:** `andy/web-version`
- **Spec:** `docs/web-zed-plan.md` §3.2, §3.3, §9

## What was created / changed

| Path | Action |
| --- | --- |
| `web/Cargo.toml` | new virtual workspace |
| `web/.cargo/config.toml` | new; wasm target rustflags only |
| `Cargo.toml` | `exclude = ["web"]` added under `[workspace]` |
| `docs/phase1-workspace.md` | this report |

`README.md` was not touched. The `> [!IMPORTANT]` two-line header is still present.

The only existing-file diff is one line on the root workspace table:

```
diff --git a/Cargo.toml b/Cargo.toml
index dbdaf5d0c5..413fe56875 100644
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -269,6 +269,7 @@ members = [
     "tooling/xtask",
 ]
 default-members = ["crates/zed"]
+exclude = ["web"]
 
 [workspace.package]
 publish = false
```

`exclude = ["web"]` is required by §3.2: a path dependency inside the workspace directory is otherwise auto-adopted as a member.

## File contents

### `web/Cargo.toml`

Virtual workspace. Own `[workspace]` table so `cd web && cargo …` does not walk up into the root workspace. No members yet, so `[workspace.dependencies]` is empty. `[patch.crates-io]` repeats the one root patch the future web graph still needs (`tree-sitter-language`); it is a git source, not a path.

```toml
[workspace]
resolver = "2"
members = []

[workspace.dependencies]

[patch.crates-io]
tree-sitter-language = { git = "https://github.com/tree-sitter/tree-sitter", rev = "43623ec9bf0eaaf7113285c46e8a09018f181b18" }
```

### `web/.cargo/config.toml`

Rustflags live under `[target.wasm32-unknown-unknown]` only. No `export RUSTFLAGS=`, no `[build] rustflags`. Values are the wasm flags from `zedweb/zed-web:web/build.sh` (atomics / shared memory / 128 MiB initial / 4 GiB max / getrandom wasm_js cfg), written as a target-specific list so they concatenate instead of replacing.

```toml
[target.wasm32-unknown-unknown]
rustflags = [
    "--cfg", "getrandom_backend=\"wasm_js\"",
    "-C", "target-feature=+atomics,+bulk-memory,+mutable-globals",
    "-C", "link-arg=--shared-memory",
    "-C", "link-arg=--import-memory",
    "-C", "link-arg=--initial-memory=134217728",
    "-C", "link-arg=--max-memory=4294967296",
    "-C", "link-arg=--export=__heap_base",
    "-C", "link-arg=--export=__stack_pointer",
    "-C", "link-arg=--export=__tls_size",
    "-C", "link-arg=--export=__tls_align",
    "-C", "link-arg=--export=__tls_base",
    "-C", "link-arg=--export=__wasm_init_tls",
    "-C", "link-arg=--export=__wasm_call_ctors",
]
```

## rustflags scar, still true

Read before writing `web/.cargo/config.toml`.

`.cargo/config.toml`:

```toml
[build]
# v0 mangling scheme provides more detailed backtraces around closures
rustflags = ["-C", "symbol-mangling-version=v0", "--cfg", "tokio_unstable"]
```

`.cargo/bundle-config.toml`:

```toml
[build]
rustflags = ["-Z", "share-generics=y"]
```

Both still set `[build] rustflags`. Because that key replaces rather than concatenates, the bundle file still drops `symbol-mangling-version=v0` and `tokio_unstable` when it is the config that wins. The new file does not add a `[build] rustflags`, so it cannot repeat that failure. Desktop builds from the repo root do not read `web/.cargo/config.toml` (Cargo walks config files upward, never downward).

## §9 checks — commands actually run, output as captured

No `./script/clippy`. No `--release --all-features`. No `cargo tree`. No desktop bundle.

`Cargo.lock` was copied to `/tmp/zed-Cargo.lock.phase1-before` **before** any edit (`501613` bytes, identical to the working copy at that moment).

### 1. Root `Cargo.lock` must not gain a `web/` member or a `path` source into `web/`

```
rg -n 'web/' /Users/andy/go/src/github.com/poi5305/zed/Cargo.lock
```

No matches. (empty output)

### 2. Lockfile byte-identical to the pre-edit snapshot

```
diff -u /tmp/zed-Cargo.lock.phase1-before /Users/andy/go/src/github.com/poi5305/zed/Cargo.lock
echo "diff_exit:$?"
```

```
diff_exit:0
```

Diff is empty. The nine crate `source` fields are therefore unchanged. They read:

| Crate | `source` (verbatim) |
| --- | --- |
| `tree-sitter` 0.27.0 | `git+https://github.com/tree-sitter/tree-sitter?rev=43623ec9bf0eaaf7113285c46e8a09018f181b18#43623ec9bf0eaaf7113285c46e8a09018f181b18` |
| `lsp-types` 0.95.1 | `git+https://github.com/zed-industries/lsp-types?rev=f1783e63a7f4eb4397bf51d4148b4895a1f7ab16#f1783e63a7f4eb4397bf51d4148b4895a1f7ab16` |
| `which` 8.0.5 | `registry+https://github.com/rust-lang/crates.io-index` |
| `url` 2.5.7 | `registry+https://github.com/rust-lang/crates.io-index` |
| `smol` 2.0.2 | `registry+https://github.com/rust-lang/crates.io-index` |
| `async-tar` 0.6.1 | `git+https://github.com/zed-industries/async-tar?rev=bd3ad6f89df9a9da7a8535958756d6bf465936a0#bd3ad6f89df9a9da7a8535958756d6bf465936a0` |
| `alacritty_terminal` 0.26.1-dev | `git+https://github.com/zed-industries/alacritty?rev=4c129667ce56611becdc82de6e28218c80e2e88f#4c129667ce56611becdc82de6e28218c80e2e88f` |
| `agent-client-protocol` 2.0.0 | `registry+https://github.com/rust-lang/crates.io-index` |
| `wasm_thread` 0.3.3 | `git+https://github.com/zed-industries/wasm_thread?rev=0cf96c7708dfb97ccf3da50347e25edcf75d6937#0cf96c7708dfb97ccf3da50347e25edcf75d6937` |

### 3. `cd web && cargo metadata --no-deps --format-version 1`

```
cd /Users/andy/go/src/github.com/poi5305/zed/web && cargo metadata --no-deps --format-version 1
```

Exit 0. Full stdout:

```
{"packages":[],"workspace_members":[],"workspace_default_members":[],"resolve":null,"target_directory":"/Users/andy/go/src/github.com/poi5305/zed/web/target","build_directory":"/Users/andy/go/src/github.com/poi5305/zed/web/target","version":1,"workspace_root":"/Users/andy/go/src/github.com/poi5305/zed/web","metadata":null}
```

`workspace_root` is `…/zed/web`, not the repo root. No `web/target/` directory was created on disk. No `web/Cargo.lock` was written (`--no-deps`, no members).

### 4. Root desktop metadata still resolves

```
cd /Users/andy/go/src/github.com/poi5305/zed && cargo metadata --no-deps
```

```
warning: please specify `--format-version` flag explicitly to avoid compatibility problems
```

Exit 0. JSON is 1502253 bytes. Parsed fields:

```
workspace_root: /Users/andy/go/src/github.com/poi5305/zed
packages: 257
workspace_members: 257
resolve: None
```

No workspace member is under `web/`. The only package id containing the substring `web/` is the pre-existing root member `crates/gpui_web` (`path+file:///Users/andy/go/src/github.com/poi5305/zed/crates/gpui_web#0.1.0`), which is not the new workspace.

A second invocation with `--format-version 1` (same directory, used only to parse without the warning) reported the same counts and the same `workspace_root`.

## `git status --short`

After the files above landed, including this report:

```
 M Cargo.toml
 M docs/web-zed-plan.md
?? docs/phase0-rebase-cost.md
?? docs/phase0b-wasm-report.md
?? docs/phase1-workspace.md
?? web/
```

`git status --short --untracked-files=all`:

```
 M Cargo.toml
 M docs/web-zed-plan.md
?? docs/phase0-rebase-cost.md
?? docs/phase0b-wasm-report.md
?? docs/phase1-workspace.md
?? web/.cargo/config.toml
?? web/Cargo.toml
```

This phase touched `Cargo.toml`, `web/Cargo.toml`, `web/.cargo/config.toml`, and `docs/phase1-workspace.md`. The other three `docs/` entries were already dirty on the branch before this work (`docs/web-zed-plan.md` modified; `docs/phase0-rebase-cost.md` and `docs/phase0b-wasm-report.md` untracked). No commit was made.
