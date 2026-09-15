# Phase 2 remainder — vendor `alacritty_terminal`

Thin fork under `web/vendor/alacritty_terminal/`, registered as a web-workspace member
and patched by git URL. No first-party `crates/` file was edited. Root `Cargo.toml`
and root `Cargo.lock` were not edited.

- **Date:** 2026-09-15
- **Branch:** `andy/web-version`
- **Spec:** `docs/web-zed-plan.md` §4 (`alacritty_terminal` row), §4.1, §9
- **Prior vendors:** `docs/phase2-vendored.md` (four crates). This phase adds the fifth.

`README.md` was not touched. The `> [!IMPORTANT]` two-line header is still present.

No wasm compile was attempted as a success criterion (no nightly, no WASI SDK).
`web/check-workspace-isolation.sh` itself runs a wasm `cargo check` as V11; that is
the script's assertion, not this phase's acceptance bar. Graph resolution is.

## Base: where the sources came from

Not `zedweb/zed-web:crates/alacritty_terminal/`. §4: *base is git `4c129667`*.

Cargo already had the checkout:

```
~/.cargo/git/checkouts/alacritty-20195d12a03fa0c5/4c12966/
```

```
git -C ~/.cargo/git/checkouts/alacritty-20195d12a03fa0c5/4c12966 rev-parse HEAD
4c129667ce56611becdc82de6e28218c80e2e88f
```

That matches root `Cargo.lock`:

```
source = "git+https://github.com/zed-industries/alacritty?rev=4c129667ce56611becdc82de6e28218c80e2e88f#4c129667ce56611becdc82de6e28218c80e2e88f"
```

The crate itself lives one directory down (`alacritty_terminal/`). Copied with
`rsync -a` excluding `.git` / `.github` / `.cargo-ok` (none of those exist in the
crate dir). `LICENSE-APACHE` in the checkout is a symlink to the repo-root license;
that would dangle at `web/vendor/LICENSE-APACHE`, so it was replaced with the
**same 10843 bytes** as a regular file (`cksum 3303457422` on both). Content-identical
to `4c129667`; not a rustfmt change.

## What zed-web actually changed (and what we refused)

`diff` of `4c129667`'s crate against `zedweb/zed-web:crates/alacritty_terminal/`
(207 files on both sides):

| Class | Count |
| --- | ---: |
| Byte-identical | 182 |
| Format-only (whitespace + trailing-comma before `}`/`)`/`]`) | **12** |
| Still different after that normalisation | 13 |

The 13 are: `Cargo.toml`, `LICENSE-APACHE` (zed-web replaced the Apache text with
the 17-byte stub `../LICENSE-APACHE`), `src/lib.rs` (the three `cfg`s), and ten
`.rs` files whose remaining diff is still rustfmt (line wrapping / import layout
that is not just a trailing comma): `event_loop.rs`, `grid/mod.rs`, `index.rs`,
`selection.rs`, `term/mod.rs`, `term/search.rs`, `tty/unix.rs`,
`tty/windows/blocking.rs`, `tty/windows/conpty.rs`, `vi_mode.rs`.

**Format-only file count in the reference tree: 12 (strict) / 22 (every `.rs`
that is not the `lib.rs` cfg port).** §4's "sampled as rustfmt noise" holds.
Those files were **not** copied.

Functional zed-web `Cargo.toml` hunk, used as the port list:

```
-rust-version.workspace = true
+rust-version = "1.85.0"
```

and `home` / `libc` / `polling` moved from `[dependencies]` to

```
[target.'cfg(not(target_family = "wasm"))'.dependencies]
```

Functional zed-web `src/lib.rs` hunk:

```
 #[cfg(not(target_family = "wasm"))]
 pub mod event_loop;
 ...
 #[cfg(not(target_family = "wasm"))]
 pub mod thread;
 #[cfg(not(target_family = "wasm"))]
 pub mod tty;
```

`event.rs` / `term/` do not `use` those three modules. Gating the `mod`s is
enough; the modules themselves were left byte-identical to `4c129667`.

## Port onto `4c129667`

`diff -rq` of `web/vendor/alacritty_terminal/` against the checkout crate:

```
Files .../alacritty_terminal/Cargo.toml and .../web/vendor/alacritty_terminal/Cargo.toml differ
Files .../alacritty_terminal/src/lib.rs and .../web/vendor/alacritty_terminal/src/lib.rs differ
```

**Rustfmt-noise file count in our vendor: 0.** Two files differ, both functional.

`src/lib.rs` (exactly the three `cfg` lines, no reformat):

```
 pub mod event;
+#[cfg(not(target_family = "wasm"))]
 pub mod event_loop;
 ...
+#[cfg(not(target_family = "wasm"))]
 pub mod thread;
+#[cfg(not(target_family = "wasm"))]
 pub mod tty;
```

`Cargo.toml`:

1. `home` / `libc` / `polling` moved to `[target.'cfg(not(target_family = "wasm"))'.dependencies]`.
2. `rust-version.workspace = true` → `rust-version = "1.85.0"`. The original inherits
   from alacritty's workspace (`[workspace.package] rust-version = "1.85.0"` at the
   checkout root). `web/Cargo.toml`'s `[workspace.package]` has `edition` and
   `publish` only; leaving `rust-version.workspace = true` makes `cargo metadata`
   fail. Same literal zed-web wrote, same value the checkout workspace declared.
   `edition.workspace = true` stays: web workspace edition is `2024`, matching
   alacritty's.

`readme = "../README.md"` was left as in `4c129667`. It points at a file that does
not exist under `web/vendor/`; `cargo metadata` and `cargo check -p alacritty_terminal`
do not require it.

Native compile of the fork (not a wasm build, not `--release`):

```
cd web && CARGO_TARGET_DIR=../target/web-probe cargo check -p alacritty_terminal --all-targets
```

```
    Checking alacritty_terminal v0.26.1-dev (.../web/vendor/alacritty_terminal)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 5.96s
alacritty_native_check_exit:0
```

## `web/Cargo.toml` — minimum edit

Existing `[profile.web-release]`, every pre-existing `[patch.*]` entry, all 97
`[workspace.dependencies]` keys, and the original 7 members: unchanged. Added:

- eighth member `"vendor/alacritty_terminal"`
- a new git-URL patch table (see next section)

Parsed after the edit:

```
members 8
workspace.dependencies keys 97
profile.web-release {inherits=release, debug=False, strip=symbols, opt-level=z, lto=thin, codegen-units=1}
patch tables: crates-io, https://github.com/zed-industries/wasm_thread, https://github.com/zed-industries/alacritty
```

`crates-io` keys still: `tree-sitter-language`, `async-process`, `async-task`,
`notify`, `notify-types`, `agent-client-protocol`, `url`, `smol`.

## Why the patch table is a git URL, not `[patch.crates-io]`

Root workspace.dependency:

```
alacritty_terminal = { git = "https://github.com/zed-industries/alacritty", rev = "4c129667ce56611becdc82de6e28218c80e2e88f" }
```

Lock `source`:

```
git+https://github.com/zed-industries/alacritty?rev=4c129667ce56611becdc82de6e28218c80e2e88f#4c129667ce56611becdc82de6e28218c80e2e88f
```

Phase 2 round 2 measured that `[patch.crates-io] wasm_thread` is **inert** for a
git-sourced crate: cargo resolves the git URL and prints `patch was not used`.
`alacritty_terminal` is the same shape. The table that actually binds is:

```
[patch."https://github.com/zed-industries/alacritty"]
alacritty_terminal = { path = "vendor/alacritty_terminal" }
```

Confirmed by wasm metadata: the package id is
`path+file:///Users/andy/go/src/github.com/poi5305/zed/web/vendor/alacritty_terminal#0.26.1-dev`
(`source` field `None`, i.e. a path package), not the git URL.

zed-web itself did **not** `[patch]` alacritty; it overrode the workspace.dependency
to `path = "crates/alacritty_terminal"` in the *root* manifest. We cannot do that
without contaminating the desktop graph. The git-URL `[patch]` in the **web**
workspace is the isolation-preserving equivalent.

Isolation V8 (source-kind vs patch-kind, no inert twin) stayed green.

## Acceptance

Root `Cargo.lock` was copied to `/tmp/lock-before-p2rest` **before** the vendor
copy (`502232` bytes, `cksum 2265851802`). Cargo invocations used
`CARGO_TARGET_DIR=../target/web-probe`. No `./script/clippy`. No
`--release --all-features`. No wasm compile as a success check.

### 1. Web wasm32 metadata: `polling` is gone

```
cd /Users/andy/go/src/github.com/poi5305/zed/web && \
  CARGO_TARGET_DIR=../target/web-probe \
  cargo metadata --filter-platform wasm32-unknown-unknown --format-version 1 \
  > /tmp/web-wasm-meta-p2rest.json
echo "metadata_exit:$?"
```

```
metadata_exit:0
```

JSON 5235112 bytes. Programmatic verdict:

```
polling_in_packages False
async-io_in_packages False
async-process_in_packages False
errno_in_packages False
rustix_in_packages False
blocking_in_packages True
package_count 912
polling_package_ids []
alacritty_id path+file:///Users/andy/go/src/github.com/poi5305/zed/web/vendor/alacritty_terminal#0.26.1-dev
```

`polling` is not in the package list. That is this phase's graph job.

`alacritty_terminal`'s *declared* `dependencies` array in metadata still names
`polling` / `home` / `libc` — Cargo lists every target-specific dep, including
ones whose cfg is false for this platform. They are not packages in the resolved
wasm graph.

### 2. Root `Cargo.lock` unchanged

```
diff /tmp/lock-before-p2rest /Users/andy/go/src/github.com/poi5305/zed/Cargo.lock
echo "lock_diff_exit:$?"
```

```
lock_diff_exit:0
```

Empty diff.

### 3. `web/check-workspace-isolation.sh`

Run in full. Exit 1. 32 checks, 4 failures. Full output:

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
FAIL V1 the four vendored packages keep their base name and version
       expected: agent-client-protocol 2.0.0
smol 2.0.2
url 2.5.7
wasm_thread 0.3.3
       actual:   agent-client-protocol 2.0.0
alacritty_terminal 0.26.1-dev
smol 2.0.2
url 2.5.7
wasm_thread 0.3.3
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
FAIL V10 the web wasm32 graph is free of async-io/async-process/polling/errno/rustix (§4.1's wall)
       expected: none
       actual:   blocking
FAIL V11 cd web && cargo check --workspace --all-targets succeeds
       expected: exit 0
       actual:   exit 101
       error[E0432]: unresolved import `merman_render::text::VendoredFontMetricsTextMeasurer` error[E0599]: no associated function or constant named `parity` found for struct `TextMeasurementPolicy` in the current scope error: could not compile `merman` (lib) due to 2 previous errors
FAIL V11 cd web && cargo check --workspace --target wasm32-unknown-unknown succeeds
       expected: exit 0
       actual:   exit 101
       error[E0432]: unresolved import `util::paths::home_dir` error[E0432]: unresolved import `tree_sitter` error[E0433]: cannot find module or crate `tree_sitter` in this scope
ok   M1 every dependency Phase 3a added to a crate manifest is used by that crate (or is a named §4.1/§5.5 addition)
ok   M2 every dependency Phase 3a wrote that the workspace already pins is inherited with workspace = true
ok   M3 every wasm clause Phase 3a wrote into a cfg gate changes the gate on at least one target
ok   S1 every .rs file naming web_time is rustfmt-clean (cargo fmt --all -- --check)

32 checks, 4 failures
```

#### V10

**`polling` is gone.** The remaining name in V10's blocked set is `blocking`.

Reverse edges in the same wasm metadata:

```
blocking ← async-fs ← languages ← zed_web_workspace
```

`crates/languages/Cargo.toml:24` is `async-fs.workspace = true` with no wasm cfg.
zed-web's `crates/languages/Cargo.toml` has the same unconditional `async-fs`.
This phase is forbidden to edit `crates/`, `web/check-*.sh`, or `web/crates/`.
Clearing `blocking` is therefore **out of scope**. V10 as written cannot turn
green from the alacritty port alone.

#### V1

The gate globs `web/vendor/*/Cargo.toml` and expects **exactly** the four Phase 2
crates. Putting `alacritty_terminal` at the path §4 named (`web/vendor/alacritty_terminal/`)
makes V1 fail. The script must not be edited. That is a contradiction between this
phase's layout and a frozen four-crate assertion; not a defect in the fork.

#### V11

Native failure is `merman` (`VendoredFontMetricsTextMeasurer` / `TextMeasurementPolicy::parity`),
a crate this phase did not touch. Wasm failure is `util::paths::home_dir` and
`tree_sitter` — the remaining wall, not `polling`. `cargo check -p alacritty_terminal --all-targets`
from `web/` is green (above).

V11's wasm `cargo check` is what the isolation script runs; this phase did not
invoke a wasm compile of its own.

## Spec points that cannot be met from this phase's write set

Two acceptance bullets fight the hard locks:

1. **V10 green** requires `blocking` out of the wasm graph. That edge is
   `languages → async-fs`, first-party, not alacritty. zed-web did not gate it
   either. Doing it here would mean editing `crates/languages/Cargo.toml`.
2. **No other isolation row newly red** fights **put the crate at
   `web/vendor/alacritty_terminal/`** and **do not edit `check-workspace-isolation.sh`**.
   V1 hardcodes four vendor names.

The graph job this phase was given — `polling` no longer in the wasm package
list, lockfile uncontaminated, cfg-only port onto `4c129667` — is done. Turning
V1 and V10 green requires either updating the frozen gate or editing `crates/`.
That is reported as 做不到, not worked around.

## `git status --short` (paths this phase wrote)

```
?? docs/phase2-alacritty.md
?? web/vendor/alacritty_terminal/
```

plus the two-line member add and the git-URL `[patch]` table in `web/Cargo.toml`
(the `web/` tree was already untracked). Root `Cargo.lock` / `Cargo.toml` /
`crates/` / `web/build.sh` / `web/check-*.sh` / `web/crates/` were not modified.
No commit.
