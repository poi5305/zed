# Phase 4a — web build entrypoint (`web/build.sh`)

Wrote `web/build.sh` only (plus this report). Did not edit `web/Cargo.toml`,
`web/check-workspace-isolation.sh`, `web/check-refusals.sh`, `web/vendor/`,
`crates/`, the root manifests, or any prior `docs/` report. Did not install a
toolchain, download the WASI SDK, or run `cargo build` / `cargo check`.

- **Date:** 2026-09-15
- **Branch:** `andy/web-version` (left as found)
- **Reference:** `zedweb/zed-web:web/build.sh` (110 lines, `git show`)
- **Spec:** `docs/web-zed-plan.md` §2.4, §3.2, §4.1, §4.2, Phase 4
- **Prior reports used, not redone:** `docs/phase4-entrypoint-research.md`,
  `docs/phase1-review-round1.md`

Claims are `[verified]` when a command was run in this session and its output
read.

---

## Method

Shell `codegraph explore` from the repo root (same query as the Phase 4
research). Codegraph does not model Cargo manifests or shell scripts:
`web/build.sh` is unindexed; `zed_web_workspace` / `zed_web_server` /
`wasm_rpc` / `wasm_remote` have no callers, no blast radius, and no tests in
this tree. What came back is the already-present web platform
(`WebDispatcher` in `crates/gpui_web/src/dispatcher.rs:146`,
`home_dir` gated `not(wasm)`). The four Phase 4 crates are still absent
(`[verified]`: `ls crates/zed_web_server` and `ls web/crates` both miss).

Manifests, the reference script, and WASI were read with `git show` / `ls` /
`grep`, not codegraph:

```
ls script/ | grep wasi
# download-wasi-sdk          [verified] tracked, executable, WASI SDK v25
```

---

## Line-by-line against `zedweb/zed-web:web/build.sh`

| Ref lines | What zed-web does | What we do | Why |
| --- | --- | --- | --- |
| 1–2 | `bash`, `set -euo pipefail` | Keep. | Same shell contract. |
| 4–5 | `web_dir`, `repo_dir` via `BASH_SOURCE` | Keep. Invoking `./web/build.sh` from the repo root or `./build.sh` from `web/` both resolve. | F3 is enforced by later `cd`, not by how the user invoked us. |
| 6–7 | `dist_dir`, `static_dir` | Keep. | Dist layout is independent of workspace split. |
| 8–9 | `native_target=…/target/web-native`; `wasm_target=${CARGO_TARGET_DIR:-…/target/web-wasm}` | Native: same. Wasm: **`${ZED_WEB_WASM_TARGET:-…/target/web-wasm}`**, not `CARGO_TARGET_DIR`. | A caller with `CARGO_TARGET_DIR=target` would share the desktop `target/` (§9). We still `export CARGO_TARGET_DIR` *inside* the wasm subshell to the dedicated path. |
| 10 | `wasi_sdk=…/target/wasi-sdk` | Keep; overridable via `WASI_SDK_PATH`. | Same on-disk location the existing downloader writes. |
| 11 | `profile=web-release` | Keep. Profile already lives in `web/Cargo.toml` (F2). | `--profile web-release` from the **root** workspace would still fail; wasm cargo uses the web manifest by being run *in* `web/`. |
| 12–13 | `stable=1.97.1`, `nightly=nightly` | Keep as defaults (`RUST_STABLE_TOOLCHAIN` / `RUST_NIGHTLY_TOOLCHAIN`). | Stable matches `rust-toolchain.toml`. Nightly stays floating until a dated pin is measured; none was, because nightly is not installed. |
| 14 | `WASM_BINDGEN_VERSION=0.2.127` | Keep the version check, but **before** cargo. | Fail fast. Crate table is still `0.2.120`; same CLI/crate skew the research recorded. |
| 16–24 | `revision()` / `web_revision` / `upstream-revision` | Keep; if `web/upstream-revision` is missing, recorded upstream is `"unknown"`. | That file is not ported yet. Do not invent a sync point. |
| **(new, before any cargo)** | Reference has no prereq block. | `require_unset_rustflags`, `require_nightly`, `require_wasi_sdk`, `require_wasm_bindgen_cli`, then the two static-asset existence checks. Dist is not wiped until these pass. | This machine has no nightly and no WASI SDK. A copy of the reference would have `rm -rf dist` and then either downloaded WASI or started a native cargo. Both are forbidden this round. |
| 26–28 | wipe dist, copy `web/static/workspace.html` | Same commands, **after** the prereqs. Missing `workspace.html` dies with an explicit “not ported yet” rather than `install: No such file`. | Files are not in this tree yet. |
| 30–35 | `rustup run stable cargo build --manifest-path ROOT --release -p extension_runtime_cli -p zed_web_server` | Same stable invocation, cwd=repo root, `--target-dir target/web-native`, **`-p zed_web_server` only**. | `crates/extension_runtime_cli` does not exist here. Server starts without the runtime binary (research §1.5). Native **must not** `cd web/` — the server is a root member. |
| 37–42 | install both bins into `dist/bin` | Install `zed-web-server` only. | Until the runtime CLI is ported. |
| 44 | `export CARGO_TARGET_DIR=${wasm_target}` | Keep, **inside the wasm subshell**. | Does not leak into the native build. |
| **45** | **`export RUSTFLAGS='…13 flags…'`** | **Deleted.** `require_unset_rustflags` aborts if `RUSTFLAGS` or `CARGO_ENCODED_RUSTFLAGS` is set, including empty. The script never exports either. | See “Point 2” below. |
| 47–51 | if clang missing, run `script/download-wasi-sdk`; then `export CC_` / `CFLAGS_` | **Do not invoke the downloader.** `require_wasi_sdk` checks clang + `wasi-sysroot` and prints the repo-root install command. Exports happen only in the wasm subshell, after the check. | User instruction for this round: no downloads. `ls script/ \| grep wasi` found `download-wasi-sdk`; no `web/` copy was written. |
| **53–58** | `rustup run nightly cargo build --manifest-path ROOT -p zed_web_workspace --target wasm32-unknown-unknown --profile web-release -Z build-std=std,panic_abort` | Subshell: `enter_web_cwd_for_wasm` (cd + assertions), then the same `-p` / target / profile / `-Z`, **no `--manifest-path`**. Cargo sees `web/Cargo.toml` because cwd is `web/`. | `--manifest-path web/Cargo.toml` from the repo root is the exact F3 failure mode. `--manifest-path ${repo_dir}/Cargo.toml` is the layout §3.2 rejected. |
| 60–66 | require `wasm-bindgen-cli` 0.2.127 | Same check, moved **before** cargo (see above). Install line added. | Compiling and then noticing the CLI is missing wastes a nightly build. |
| 68–75 | `wasm-bindgen` + `web/scripts/patch-wasm-bindgen-memory.sh` | Keep the calls. Missing patch script is a pre-cargo error. | Script is not in this tree yet; do not copy it in 4a. |
| 77–109 | tar assets, `build-info.json`, gzip/brotli, size print | Keep. | Unchanged post-process. |

No `web/run.sh`, no `web/static/workspace.html`, no patch script, no
`web/rust-toolchain.toml`, no helper under `web/` other than `build.sh`.
`script/download-wasi-sdk` is used as the documented install path, not invoked.

---

## The four required fixes

### 1. F3 — cwd trap

Cargo walks **up from the working directory** for `.cargo/config.toml`. Round-1
F3 measured it: `cd web` → all 13 flags; from the repo root with
`--manifest-path web/Cargo.toml` → none of them, silently replaced by desktop
`-C symbol-mangling-version=v0 --cfg tokio_unstable`.

The reference already computes `web_dir` and then never cds into it, which is
fine for zed-web because their flags are `export RUSTFLAGS`. Ours live in
`web/.cargo/config.toml`. A comment is not a fix.

`enter_web_cwd_for_wasm` **cds** to `${web_dir}` and then **asserts**:

- `${PWD}` equals `${web_dir}`
- `${PWD}` is not `${repo_dir}` (the measured failure mode)
- `${PWD}/.cargo/config.toml` exists (otherwise cargo walks up to the desktop
  config)
- that file contains `target-feature=+atomics`

The wasm cargo is run in a subshell after that function, with no
`--manifest-path`. Native cargo is a **different** subshell with cwd = repo
root, so it keeps desktop `[build] rustflags` and never sees the wasm
`target.*` list.

### 2. RUSTFLAGS precedence

Cargo book order (research Appendix A): `CARGO_ENCODED_RUSTFLAGS` > `RUSTFLAGS`
> concatenated `target.*` > `build.rustflags`. Env vars **replace**, they do
not concatenate.

The reference **must** export `RUSTFLAGS` because it builds from the root
workspace, whose `.cargo/config.toml` only has the getrandom cfg. Adopting that
line here would replace the 13 flags already in `web/.cargo/config.toml`
(round-1 F6: “adopting zed-web’s line 45 re-introduces the override §3.2
forbids”).

This script:

- never `export RUSTFLAGS` / `CARGO_ENCODED_RUSTFLAGS`
- aborts if either variable is **set** (including empty — empty still wins
  and would apply *no* flags)
- re-checks inside the wasm subshell so a later edit cannot sneak an export
  in between

No `unset`: silently clearing a caller’s flags would hide the footgun the
check exists to name.

### 3. F4 — nightly + `-Z build-std`

`+atomics` needs std rebuilt with those rustflags. That is
`-Z build-std=std,panic_abort`, nightly-only. `cd web && rustup show` is
stable 1.97.1 via the root `rust-toolchain.toml`. This machine has no nightly
(`[verified]`: `rustup toolchain list` is `stable` and `1.97.1`;
`rustup run nightly rustc --version` → not installed).

`require_nightly` runs `rustup run "${nightly_toolchain}" rustc --version`
**without** `--install`. Failure prints rustup’s own message plus:

```
rustup toolchain install nightly
rustup component add rust-src --toolchain nightly
rustup target add wasm32-unknown-unknown --toolchain nightly
```

If nightly exists but `rust-src` does not, that is a separate non-zero exit
(`-Z build-std` needs the sources).

**Selection: `rustup run nightly cargo`, not `cargo +nightly`, not
`web/rust-toolchain.toml`.**

| Mechanism | Pins rust-analyzer / `cd web && cargo` to nightly? | Beats root `rust-toolchain.toml` for that one command? | Verdict |
| --- | --- | --- | --- |
| `web/rust-toolchain.toml` `channel = "nightly"` | **Yes** | Yes, for every command under `web/` | **Reject.** Phase 1 review §3.1 already refused this. |
| `rustup override set nightly` in `web/` | Yes, sticky, not in git | Yes | **Reject.** |
| `cargo +nightly` | No | Yes (rustup slot 1) | Acceptable equivalent. |
| `rustup run nightly cargo` | No | Yes (`RUSTUP_TOOLCHAIN`, slot 2) | **Chosen.** Matches the reference line 53; missing nightly fails clearly because we do not pass `--install`. |

Default stays the floating name `nightly` (overridable with
`RUST_NIGHTLY_TOOLCHAIN=nightly-YYYY-MM-DD`). No date was invented: nothing
has `-Z build-std`’d this graph yet.

`-Z build-std` stays on that cargo command line. Putting it in
`web/.cargo/config.toml` (`[unstable] build-std`) would make stable
`cd web && cargo check` hit nightly-only config.

### 4. WASI SDK is a hard prerequisite (§4.2)

Not “one grammar for `tasks_ui`”. `load-grammars` is **on** in the wasm graph
(`crates/markdown` and `crates/edit_prediction`); `zed_web_workspace` reaches
`markdown` via `editor` / `markdown_preview` / `agent_ui`; that pulls **18**
`tree-sitter-*` C libraries. Host clang has no `wasm32-wasi` headers.
`CC_wasm32_unknown_unknown` +
`CFLAGS_…=-isystem <wasi-sysroot>/include/wasm32-wasi` are the only way those
C builds exist.

`script/download-wasi-sdk` **is in this tree** (`ls script/ | grep wasi` →
`download-wasi-sdk`, v25, writes `./target/wasi-sdk` relative to **its cwd**).
No equivalent was written under `web/`. The downloader is **not invoked**:
`require_wasi_sdk` checks `${wasi_sdk}/bin/clang` and the sysroot include dir,
then prints

```
<repo>/script/download-wasi-sdk
```

and tells the operator to run it **from the repo root** (cwd-relative install
path). `WASI_SDK_PATH` can point at an existing v25 tree.

`target/wasi-sdk` is absent today `[verified]`. The check is in the script;
this run did not reach it (nightly fails first).

---

## Execution (real output)

Command: `./web/build.sh` from the repo root. No `timeout`. No cargo.

```
error: nightly Rust is not installed (toolchain 'nightly').
error: toolchain 'nightly-aarch64-apple-darwin' is not installed
help: run `rustup toolchain install nightly-aarch64-apple-darwin` to install it

+atomics requires rebuilding std with -Z build-std=std,panic_abort, which
only nightly cargo can do. The repo rust-toolchain.toml stays on stable
1.97.1; this script selects nightly for the wasm cargo line only.

Install (this script will not do it):
  rustup toolchain install nightly
  rustup component add rust-src --toolchain nightly
  rustup target add wasm32-unknown-unknown --toolchain nightly
EXIT:1
```

That is the correct behaviour on this machine. Dist was not wiped (`require_nightly`
runs before `rm -rf`). WASI, wasm-bindgen, `workspace.html`, and both cargo
invocations were not reached.

`bash -n web/build.sh` passed before the run.

---

## Not done (honest remaining)

These are out of Phase 4a’s file scope, not failures of the script:

- Nightly + `rust-src` + wasm32 target are still not installed (by instruction).
- WASI SDK v25 is still not on disk (by instruction).
- `zed_web_server`, `wasm_rpc`, `wasm_remote`, `zed_web_workspace` are still
  absent, so even with the toolchains the cargo lines would fail.
- `web/static/workspace.html`, `web/scripts/patch-wasm-bindgen-memory.sh`,
  `web/run.sh` are still absent; the script names them and dies clearly if
  reached.
- No dated nightly pin. Pick one after the first successful `-Z build-std`.

---

## Files this pass wrote

```
?? docs/phase4a-build-entrypoint.md
?? web/build.sh
```

`web/` as a whole was already untracked (`Cargo.toml`, isolation/refusal gates, `vendor/`). Those files’ mtimes are unchanged (Cargo.toml 15:14, check-refusals 15:21, isolation 15:43, `.cargo/config.toml` 14:40; `build.sh` is 16:03). Dirty `crates/` / root `Cargo.toml` / `Cargo.lock` belong to the concurrent native compile and were not touched here.
