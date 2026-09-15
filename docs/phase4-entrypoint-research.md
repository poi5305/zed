# Phase 4 — web build entrypoint port (research)

Read-only. The only file this pass wrote is this report. No `.rs`, `Cargo.toml`, `.sh`, or prior `docs/` report was edited. No `git stash` / `checkout` / `restore` / `reset` / `rebase`. No commit. No `./script/clippy`. No `--release --all-features`. No cargo command was run that would write a lockfile or compile; toolchain facts below are from `rustup` / `git show` / file reads.

- **Date:** 2026-09-15
- **Branch:** `andy/web-version` (left as found)
- **Reference:** `zedweb/zed-web` (`web/build.sh` and `crates/{wasm_rpc,wasm_remote,zed_web_server,zed_web_workspace}`)
- **Spec:** `docs/web-zed-plan.md` §2.4, §3.2, §7, Phase 4; F3/F4 from `docs/phase1-review-round1.md` (not re-measured)
- **Prior reports used, not redone:** `docs/phase0b-wasm-report.md`, `docs/phase2-dependency-wall.md`, `docs/phase1-review-round1.md`, plus `docs/phase1-workspace.md` / `docs/phase2-vendored.md` for “what is already on disk”

Claims are `[verified]` when a command was run in this session and its output read, or `[inferred]` when they follow from those facts plus the spec. F3/F4 compiler measurements are cited from round-1 review as `[verified elsewhere]`, not re-run.

---

## Method

One `codegraph explore` from the repo root, then `git show` / `git ls-tree` of `zedweb/zed-web` for the four crates that are not in this tree.

```
codegraph explore -p /Users/andy/go/src/github.com/poi5305/zed --max-files 20 \
  "zed_web_workspace load_core_panels zed_web_server wasm_rpc wasm_remote web/build.sh gpui_web WebDispatcher download-wasi-sdk rust-toolchain.toml home_dir wasm32-unknown-unknown"
```

**Result [verified].** 103 symbols / 11 files. No indexed file matches `web/build.sh`. The four Phase 4 crates are absent from this tree, so they have **no callers, no blast radius, and no tests in the index**. What came back is the already-present web platform:

| Symbol | Where | Callers / tests (from that explore) |
| --- | --- | --- |
| `WebDispatcher` | `crates/gpui_web/src/dispatcher.rs:146` | 2 callers in `crates/gpui_web/src/http_client.rs`; no tests within 3 hops |
| `WebDispatcher::new` | `:156` | return `Self`; `supports_threads` is `multithreaded` feature ∧ `allow_threads` ∧ `shared_memory_supported()` ∧ `wait_async_supported()`; else warn and single-thread (`:164–175`) |
| `home_dir` | `crates/util/src/paths.rs:23` | `#[cfg(not(target_family = "wasm"))]`; return `&'static PathBuf` — the §6.3 hole Phase 0b already compiled |

Everything below about `wasm_rpc` / `zed_web_server` / `wasm_remote` / `zed_web_workspace` / `web/build.sh` is from `git show zedweb/zed-web:<path>`, not from this index.

---

## 0. What is already on disk (not Phase 4)

[verified] `web/` today:

| Path | Role |
| --- | --- |
| `web/Cargo.toml` | second workspace; 4 vendor members; `[profile.web-release]`; `[patch]` for the four thin forks plus the five wasm-reachable root patches |
| `web/.cargo/config.toml` | 13 wasm rustflags under `[target.wasm32-unknown-unknown]`; F3/F4 recorded in the header comment |
| `web/check-workspace-isolation.sh` | §9 / F1–F6 gate |
| `web/vendor/{agent_client_protocol_patch,url_wasm,smol_wasm,wasm_thread_patch}/` | Phase 2 thin forks |
| **no** `web/build.sh` | the subject of question 1 |
| **no** `web/crates/` | the three wasm crates are not here yet |
| **no** `crates/zed_web_server` | not a root member (`Cargo.toml` `members` has 256 entries, none named `zed_web_server`) |

Phase 2 did **not** vendor `tree-sitter` / `lsp-types` / `which` / `async-tar`, and did **not** apply the Phase 3 MANIFEST cfg moves (`docs/phase2-vendored.md`). `web/vendor/smol_wasm/src/rpc.rs` is an explicit stand-in for Phase 4’s `wasm_rpc::RpcClient` (same method names, every `call` returns an error).

This machine [verified]:

| Tool | State |
| --- | --- |
| Active toolchain from `web/` | `1.97.1-aarch64-apple-darwin` (overridden by repo `rust-toolchain.toml`) |
| `rustup toolchain list` | `stable-aarch64-apple-darwin`, `1.97.1-aarch64-apple-darwin` — **no nightly** |
| `rustup run nightly rustc --version` | `error: toolchain 'nightly-aarch64-apple-darwin' is not installed` |
| `wasm-bindgen` | not on `PATH` |
| `target/wasi-sdk` | not present |
| `script/download-wasi-sdk` | **exists**, tracked, executable, 62 lines, WASI SDK **v25** |

---

## 1. How to rewrite `web/build.sh` for the second workspace

Reference script: `git show zedweb/zed-web:web/build.sh` (110 lines). It builds **from the root workspace** (`--manifest-path "${repo_dir}/Cargo.toml" -p zed_web_workspace`). §3.2 forbids that. The script cannot be copied.

### 1.1 Working directory (F3) — must be `web/` for the wasm cargo, and only then

Cargo discovers `.cargo/config.toml` by walking **up from the cwd**, not from `--manifest-path`. Round-1 F3 measured it: `cd web` → all 13 flags on rustc; from the repo root with `--manifest-path web/Cargo.toml` → **none** of them, silently replaced by desktop `-C symbol-mangling-version=v0 --cfg tokio_unstable`. [verified elsewhere]

The reference already computes `web_dir` on line 4 and then **never cds into it**. That is fine for zed-web because their wasm flags live in `export RUSTFLAGS` (line 45) and a one-line `[target.wasm32-unknown-unknown]` in the **root** `.cargo/config.toml` (only `--cfg getrandom_backend="wasm_js"`). Our flags live in `web/.cargo/config.toml`. A root-cwd `--manifest-path web/Cargo.toml` build is the F3 failure mode.

**Rule for our script:** two cwds, two workspaces.

| Stage | cwd | manifest | why |
| --- | --- | --- | --- |
| Native `zed_web_server` | **repo root** | root `Cargo.toml` | server is a root member; `cd web && cargo -p zed_web_server` cannot see it |
| WASI SDK download | **repo root** | n/a | `script/download-wasi-sdk` writes `./target/wasi-sdk` relative to **its cwd** [verified, lines 5 and 50–56] |
| Wasm `zed_web_workspace` | **`web/`** | `web/Cargo.toml` | so `web/.cargo/config.toml` is discovered |
| `wasm-bindgen` / dist | either | n/a | paths are absolute once `web_dir` / `dist_dir` are set |

Concrete shape [inferred from F3 + the script’s own `web_dir`]:

```sh
web_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd "${web_dir}/.." && pwd)"

# native — root workspace
(
  cd "${repo_dir}"
  rustup run "${stable_toolchain}" cargo build \
    --manifest-path "${repo_dir}/Cargo.toml" \
    --target-dir "${native_target}" \
    --release \
    -p zed_web_server
)

# wasi-sdk — must run with cwd = repo root (see §1.4)
(
  cd "${repo_dir}"
  if [[ ! -x "${wasi_sdk}/bin/clang" ]]; then
    "${repo_dir}/script/download-wasi-sdk"
  fi
)

# wasm — web workspace; subshell so the rest of the script is not stuck in web/
(
  cd "${web_dir}"
  unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS   # see §1.3
  export CARGO_TARGET_DIR="${wasm_target}"
  export CC_wasm32_unknown_unknown="${wasi_sdk}/bin/clang"
  export CFLAGS_wasm32_unknown_unknown="-isystem ${wasi_sdk}/share/wasi-sysroot/include/wasm32-wasi"
  rustup run "${nightly_toolchain}" cargo build \
    --manifest-path "${web_dir}/Cargo.toml" \
    -p zed_web_workspace \
    --target wasm32-unknown-unknown \
    --profile "${profile}" \
    -Z build-std=std,panic_abort
)
```

A header comment that says “run me from the repo root” is not enough: the wasm cargo must **itself** `cd` to `web/`. Invoking `./web/build.sh` from the repo root is the intended UX; the script changes directory internally. Invoking `./build.sh` from `web/` also works if every cargo is prefixed with an explicit `cd`.

Do not use `--manifest-path web/Cargo.toml` from the repo root for the wasm build. That is the exact F3 command.

### 1.2 Nightly: `rustup run` / `+nightly` / `web/rust-toolchain.toml`

Need nightly because `-Z build-std=std,panic_abort` is a nightly cargo flag, and `+atomics` needs a std rebuilt with those rustflags (F4; zed-web `web/build.sh:53-58`; `web/.cargo/config.toml` header). [verified elsewhere + this session’s `rustup run nightly` failure]

zed-web’s **root** `rust-toolchain.toml` is also `channel = "1.97.1"` [verified, identical shape to ours]. They do **not** pin the repo (or `web/`) to nightly. They select nightly only on that one cargo line. Their README (“Build From Source”) lists “Rust nightly with `rust-src`” as a host prerequisite, not as a toolchain file. Validation in the same README uses `cargo +nightly check -p zed_web_workspace … -Z build-std=std,panic_abort` — still a per-command override.

rustup override order, from https://rust-lang.github.io/rustup/overrides.html [verified, fetched this session]:

1. `cargo +nightly` (shorthand on the proxied binary)
2. `RUSTUP_TOOLCHAIN`
3. directory override (`rustup override set`)
4. `rust-toolchain.toml` (walk-up; closer file wins against a farther directory override)
5. default

`rustup run <toolchain> env` on this machine sets `RUSTUP_TOOLCHAIN=<toolchain>` [verified: `rustup run 1.97.1 env` printed `RUSTUP_TOOLCHAIN=1.97.1-aarch64-apple-darwin`]. That is slot 2, which beats the repo `rust-toolchain.toml` (slot 4). `rustup run --help` also states that `cargo +nightly build` ≡ `rustup run --install nightly cargo build`.

| Mechanism | Pins rust-analyzer / `cd web && cargo check` to nightly? | Beats root `rust-toolchain.toml` for that one command? | Verdict |
| --- | --- | --- | --- |
| `web/rust-toolchain.toml` with `channel = "nightly"` (or a date) | **Yes** — RA and every cargo in `web/` walk into it | Yes, for all commands under `web/` | **Reject.** This is the thing F4’s review already refused (`docs/phase1-review-round1.md` §3.1). |
| `rustup override set nightly` in `web/` | Yes, sticky, not even in git | Yes | **Reject.** |
| `cargo +nightly` / `cargo +nightly-YYYY-MM-DD` | No | Yes (slot 1) | Acceptable. |
| `rustup run nightly cargo` / `rustup run nightly-YYYY-MM-DD cargo` | No | Yes (sets `RUSTUP_TOOLCHAIN`, slot 2) | **Preferred.** Matches the reference line 53, fails clearly when nightly is missing (`--install` is optional; zed-web does not pass it). |

**Answer:** select nightly **only inside `web/build.sh`**, with `rustup run "${nightly_toolchain}" cargo …`. Do **not** add `web/rust-toolchain.toml`. Leave `cd web && cargo check` and rust-analyzer on stable 1.97.1 via the existing root file.

Pinning: zed-web defaults `RUST_NIGHTLY_TOOLCHAIN:-nightly` (floating). Floating nightly is what they ship; it is also what will break `-Z build-std` on a random Tuesday. A date pin (`nightly-YYYY-MM-DD`) is the safer default for us. **No working date was measured here** — nightly is not installed — so the date itself is `[inferred]` as “pick one once a build has succeeded, then put it in the script”. Do not invent a date in this report.

Host pieces the script should *check* (and refuse with an install line), not silently `rustup toolchain install`:

- `rustup toolchain list` contains that nightly
- `rust-src` on that nightly (`-Z build-std` needs it)
- `wasm32-unknown-unknown` on that nightly (the target in root `rust-toolchain.toml` is for 1.97.1 only)
- `wasm-bindgen-cli` version match (reference: `0.2.127`; crate in our root table is `wasm-bindgen = "0.2.120"` — zed-web has the same crate/CLI skew)

`web/check-workspace-isolation.sh` F4 today passes if `web/` is on nightly **or** if `web/.cargo/config.toml` mentions `build-std`. A `rustup run` in `build.sh` does **not** flip the first branch (`cd web && rustup show` stays 1.97.1). Keep the comment; optionally later teach the gate to parse `build.sh`. That change is not this research.

Putting `-Z build-std` into `web/.cargo/config.toml` (`[unstable] build-std = …`) would make **stable** `cd web && cargo check` hit nightly-only config. Leave it on the nightly command line.

### 1.3 Must `build.sh` export `RUSTFLAGS`? **No.**

Cargo rustflags sources are mutually exclusive, first match wins (Cargo book `build.rustflags`, fetched this session) [verified]:

1. `CARGO_ENCODED_RUSTFLAGS`
2. `RUSTFLAGS`
3. all matching `target.*` rustflags, concatenated
4. `build.rustflags`

§3.2 already forbids `export RUSTFLAGS=` for this reason. Round-1 F6 says adopting zed-web’s line 45 “re-introduces the override §3.2 forbids.” Our 13 flags already live in `web/.cargo/config.toml` as `[target.wasm32-unknown-unknown] rustflags`.

zed-web **must** export `RUSTFLAGS` because they build from the root workspace, whose `.cargo/config.toml` only has the getrandom cfg (`.cargo/config.toml:23-24` on that ref). Env `RUSTFLAGS` is how they inject atomics/shared-memory, and they duplicate the getrandom cfg inside the string so it is not lost. We already moved that whole string into the web config. Exporting it here would **replace** that config list. A later flag added to the toml and forgotten in the script would vanish with no error — the F3 class of bug, on a different axis.

**Answer:** our `web/build.sh` must **not** `export RUSTFLAGS`. It should `unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS` in the wasm subshell so a caller’s environment cannot strip `+atomics`. Then cwd=`web/` makes source (3) apply. Native builds stay in the repo root, so they keep desktop `[build] rustflags` and never see the wasm `target.*` list.

This is a spec-vs-reference conflict, not a spec self-contradiction: §3.2 is internally consistent; the reference script is written for a different workspace layout.

### 1.4 WASI SDK

`script/download-wasi-sdk` **is in this tree** [verified]: tracked, `script/download-wasi-sdk`, 62 lines, downloads WASI SDK **v25** from `https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-25/wasi-sdk-25.0-${ARCH}-${OS}.tar.gz` into `./target/wasi-sdk` if that directory is missing. `uname` `arm64|aarch64` → `arm64`, `darwin` → `macos`.

It is **cwd-relative**. zed-web `build.sh:8,47-51` defaults `wasi_sdk` to `${repo_dir}/target/wasi-sdk` and invokes `"${repo_dir}/script/download-wasi-sdk"` **without** cd’ing to `repo_dir`. That only lands in the right place if the user already invoked the script from the repo root. If our wasm section `cd`s to `web/` first and then calls the downloader, clang is written to `web/target/wasi-sdk` while `CC_wasm32_unknown_unknown` still points at `repo/target/wasi-sdk`. **Invoke the downloader with cwd = repo root**, then export:

```
CC_wasm32_unknown_unknown=${wasi_sdk}/bin/clang
CFLAGS_wasm32_unknown_unknown=-isystem ${wasi_sdk}/share/wasi-sysroot/include/wasm32-wasi
```

`target/wasi-sdk` is absent today; the first build will download. Do not put WASI under `web/target/` — §9 wants the web build off the desktop `target/`, but the downloader already uses `target/wasi-sdk`, which is the same convention zed-web uses next to `target/web-native` and `target/web-wasm`.

WASI is load-bearing for `tree-sitter-json` via `tasks_ui` (`docs/phase2-dependency-wall.md` §1), not for tree-sitter core (that is the fork `build.rs` early return).

### 1.5 Line-by-line rewrite

| Ref lines | What zed-web does | What we do |
| --- | --- | --- |
| 1–2 | `bash`, `set -euo pipefail` | Keep. |
| 4–5 | `web_dir`, `repo_dir` | Keep. These two variables are how F3 is enforced: wasm cargo runs with cwd=`$web_dir`. |
| 6–7 | `dist_dir`, `static_dir` | Keep (`web/dist/…`). |
| 8–9 | `native_target=…/target/web-native`, `wasm_target=CARGO_TARGET_DIR or …/target/web-wasm` | Keep. Do **not** let wasm fall through to `web/target` *or* desktop `target/`. Plan §2.4 / §9. |
| 10 | `wasi_sdk=…/target/wasi-sdk` | Keep; download with cwd=repo root (§1.4). |
| 11 | `profile=web-release` | Keep. Profile is already in `web/Cargo.toml` [verified]. Building `--profile web-release` from the **root** workspace would still fail (`profile 'web-release' is not defined` there — F2). Wasm cargo must use the web manifest. |
| 12–13 | `stable=1.97.1`, `nightly=nightly` | Stable pin 1.97.1 is correct (matches `rust-toolchain.toml`). Nightly: `rustup run`, optionally a date; **not** a `web/rust-toolchain.toml`. |
| 14 | `WASM_BINDGEN_VERSION=0.2.127` | Keep the version check. CLI is not installed here. |
| 16–24 | `revision()` / `web_revision` / `upstream_revision` | Keep if we also take `web/upstream-revision`; otherwise write `"unknown"` and leave sync-upstream to a later phase. |
| 26–28 | wipe dist, copy `web/static/workspace.html` | Keep. We do not have `workspace.html` yet (158 lines on the ref). Copy it as part of Phase 4, not in this research. |
| 30–35 | `rustup run stable cargo build --manifest-path ROOT --release -p extension_runtime_cli -p zed_web_server` | Native: same stable invocation, cwd=repo root, `--target-dir target/web-native`, **`-p zed_web_server` only**. `crates/extension_runtime_cli` does not exist in this tree [verified `MISS`]. It is a heavy native binary (`extension_host`, `gpui`, …). First light does not need it: `zed_web_server` resolves `ZED_EXTENSION_RUNTIME` optionally (`extension_rpc.rs:937-951`) and starts without the binary. |
| 37–42 | install both bins into `dist/bin` | Install `zed-web-server` only until the runtime CLI is ported. |
| 44 | `export CARGO_TARGET_DIR=${wasm_target}` | Keep, inside the wasm subshell. |
| **45** | **`export RUSTFLAGS='…13 flags…'`** | **Delete.** `unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS`. Flags come from `web/.cargo/config.toml` once cwd is `web/`. |
| 47–51 | download wasi-sdk; `CC_` / `CFLAGS_` | Keep the exports; run the downloader with cwd=repo root. |
| **53–58** | `rustup run nightly cargo build --manifest-path ROOT -p zed_web_workspace --target wasm32-unknown-unknown --profile web-release -Z build-std=std,panic_abort` | `cd "$web_dir"`; `--manifest-path "$web_dir/Cargo.toml"` (or implicit after cd); same `-p` / target / profile / `-Z`. **Not** the root manifest. |
| 60–66 | require `wasm-bindgen-cli` 0.2.127 | Keep. |
| 68–73 | `wasm-bindgen --target web --no-typescript --out-dir static …/zed_web_workspace.wasm` | Keep; wasm artifact path is `${wasm_target}/wasm32-unknown-unknown/${profile}/zed_web_workspace.wasm`. |
| 74–75 | `web/scripts/patch-wasm-bindgen-memory.sh` (71 lines on the ref) | Copy that script (and its test) in Phase 4. Shared memory / 128 MiB heap. |
| 77–81 | `tar` fonts/icons/images/themes/sounds/prompts from `assets/` | Keep. |
| 83–86 | `build-info.json` | Keep if revision helpers stay. |
| 88–109 | gzip/brotli + size print | Keep. Brotli is optional (`command -v brotli`). |

`web/run.sh` (reference) execs `dist/bin/zed-web-server <workspace> <static> --host 127.0.0.1 --port 8090`. Port that as-is once the dist layout exists. Server default bind is already `127.0.0.1:8090` (`main.rs:41-44`). COOP/COEP are set on responses (`main.rs:776-777`): `cross-origin-opener-policy: same-origin`, `cross-origin-embedder-policy: require-corp` — required for `SharedArrayBuffer` / wasm threads (§2.3, §7).

---

## 2. Where the four crates go, and what they depend on

Plan §3.2: `zed_web_server` stays in the **root** workspace; `wasm_rpc`, `wasm_remote`, `zed_web_workspace` go in the **web** workspace. Layout already drawn:

```
web/crates/zed_web_workspace/
crates/zed_web_server/          # root member
```

The plan does not spell a path for the other two. Put them next to the wasm binary so they are not root members and cannot pick up (or pollute) the desktop graph: `web/crates/wasm_rpc`, `web/crates/wasm_remote`.

None of the four exist in this tree. File lists and line counts are `git ls-tree -r zedweb/zed-web` + `git cat-file -p | wc -l` [verified]. `wc -l` counts newline-terminated lines in the blob.

### 2.1 `wasm_rpc` — web workspace (`web/crates/wasm_rpc`)

**Files (2), 550 lines**

| Lines | Path |
| ---: | --- |
| 21 | `crates/wasm_rpc/Cargo.toml` |
| 529 | `crates/wasm_rpc/src/lib.rs` |

No `tests/`. No `#[test]` in the crate [verified].

**Public surface** (`src/lib.rs`) [verified]:

```
pub struct RpcClient
pub fn connect(url: &str) -> Result<Self>          # :279 wasm, :459 non-wasm
pub async fn call<P: Serialize, R: DeserializeOwned>(&self, method: &str, params: &P) -> Result<R>
pub async fn call_void<P: Serialize>(&self, method: &str, params: &P) -> Result<()>
pub fn is_connected(&self) -> bool
pub fn on_notification<F: Fn(Value) + Send + 'static>(&self, method: &str, handler: F)
pub fn subscribe_reconnect(&self) -> mpsc::UnboundedReceiver<u64>
```

That is the same shape `web/vendor/smol_wasm/src/rpc.rs` already stubs. Phase 4 replaces the stub with `pub use wasm_rpc::RpcClient` (as that file’s comment says).

**Cargo.toml dependencies**

| Dep | Form | Classification |
| --- | --- | --- |
| `anyhow`, `futures`, `serde`, `serde_json` | `.workspace = true` | third-party; redeclare versions in `web/Cargo.toml` `[workspace.dependencies]` |
| `wasm-bindgen` | `.workspace = true` (wasm target) | third-party `0.2.120` in our root table |
| `js-sys = "0.3"`, `wasm-bindgen-futures = "0.4"`, `web-sys = { version = "0.3", features = [console, WebSocket, MessageEvent, Event, CloseEvent, BinaryType] }` | direct, wasm target | not a workspace key |

Also `edition.workspace = true` and `publish.workspace = true` → web workspace needs `[workspace.package] edition = "2024"` and `publish = false` (root is `edition = "2024"`, `publish = false`). `web/Cargo.toml` does **not** have `[workspace.package]` today [verified]. Vendor crates pin `edition` themselves, which is why Phase 2 did not hit this.

**Root members named:** none. Protocol crate only.

**Tests:** none. Blast radius in *this* tree: `smol_wasm`’s stub is the only planned caller until `wasm_remote` lands.

### 2.2 `wasm_remote` — web workspace (`web/crates/wasm_remote`)

**Files (6), 2505 lines**

| Lines | Path |
| ---: | --- |
| 26 | `crates/wasm_remote/Cargo.toml` |
| 54 | `crates/wasm_remote/README.md` |
| 772 | `crates/wasm_remote/src/fs.rs` |
| 1623 | `crates/wasm_remote/src/git.rs` |
| 29 | `crates/wasm_remote/src/lib.rs` |
| 1 | `crates/wasm_remote/src/transport.rs` (`pub use wasm_rpc::RpcClient as RemoteClient;`) |

No `tests/`. No `#[test]` [verified].

`lib.rs` is entirely `#[cfg(target_family = "wasm")]`: modules `fs` / `git` / `transport`, `RemoteFs`, `RemoteGitRepository`, `RemoteClient`, `set_remote_client` / `remote_client`. Off wasm the crate is empty. [verified]

**`.workspace = true` deps (15)**

| Name | Kind |
| --- | --- |
| `collections`, `fs`, `gpui`, `git`, `rope`, `text` | **root members** → `path = "../crates/<name>"` in web `[workspace.dependencies]` |
| `anyhow`, `async-channel`, `async-trait`, `base64`, `futures`, `log`, `serde`, `serde_json` | third-party, copy the root version table |
| `wasm_rpc` | new web member → `path = "crates/wasm_rpc"` |

**Root members this crate names:** `collections`, `fs`, `gpui`, `git`, `rope`, `text` (6). Two of those (`git`, `rope`) are **not** named by `zed_web_workspace`.

### 2.3 `zed_web_workspace` — web workspace (`web/crates/zed_web_workspace`)

This is the wasm **binary**. `[[bin]] name = "zed_web_workspace"`. Entry is `#[wasm_bindgen(start)] pub fn main()` at `src/main.rs:2109` (wasm) / a second `fn main` at `:2238`. It builds `RemoteFs`, then `workspace::open_paths` (`:2230`). `load_core_panels` (`:1930`) loads Project / Outline / Git / Debug / Terminal / Agent only — not our four local panels (§6.1).

**Files (11), 5892 lines**

| Lines | Path |
| ---: | --- |
| 120 | `crates/zed_web_workspace/Cargo.toml` |
| 187 | `src/host_debug_adapter.rs` |
| 2240 | `src/main.rs` |
| 242 | `src/remote_highlight.rs` |
| 1328 | `src/web_agent_panel.rs` |
| 512 | `src/web_extensions.rs` |
| 191 | `src/web_menu_bar.rs` |
| 201 | `src/web_proxy_http.rs` |
| 401 | `src/web_quick_action_bar.rs` |
| 150 | `src/web_settings_modal.rs` |
| 320 | `src/web_user_menu.rs` |

**Tests:** 3 `#[test]` in `web_proxy_http.rs` (`:167`, `:180`, `:186`). Nothing else. [verified]

**Direct (non-workspace) deps:** `tar = "0.4"`; wasm-only `wasm-bindgen = "0.2"`, `wasm-bindgen-futures = "0.4"`, `js-sys = "0.3"`, `web-sys` (console/Location/Window), `instant = { version = "0.1", features = ["wasm-bindgen"] }`. The `instant` line is the fastrand/futures-lite `now` symbol fix from `phase2-dependency-wall.md` §2.

#### The number: `[workspace.dependencies]` keys the web workspace must re-declare

`foo.workspace = true` resolves against **that crate’s** workspace. Shared Zed crates stay root members and keep inheriting from the root table (§3.2). The web workspace only has to name keys **its own members** write as `.workspace = true`.

Union across `wasm_rpc` + `wasm_remote` + `zed_web_workspace` [verified, parsed from the three `Cargo.toml` blobs]:

| Bucket | Count |
| --- | ---: |
| `.workspace = true` keys, unique | **97** |
| of which first-party root members (`path = "../crates/…"`) | **81** |
| of which third-party (copy version from root table) | **14** |
| of which new web crates (`wasm_rpc`, `wasm_remote`) | **2** |

`zed_web_workspace` itself names **79** of those 81 root members. `wasm_remote` adds `git` and `rope`. `wasm_rpc` names none.

Root `[workspace.dependencies]` has **511** parsed keys. The web table is **97**, not 511. That is the size §3.2 was pointing at.

**79 root crates named by `zed_web_workspace` (every one exists as `crates/<name>` in this tree [verified]):**

`activity_indicator`, `agent_settings`, `agent_ui`, `assets`, `breadcrumbs`, `client`, `clock`, `collections`, `command_palette`, `dap`, `dap_adapters`, `db`, `debugger_ui`, `diagnostics`, `edit_prediction`, `edit_prediction_ui`, `editor`, `encoding_selector`, `extensions_ui`, `feature_flags`, `file_finder`, `fs`, `git_ui`, `go_to_line`, `gpui`, `gpui_platform`, `gpui_web`, `http_client`, `image_viewer`, `keymap_editor`, `language`, `language_model`, `language_models`, `language_selector`, `language_tools`, `languages`, `line_ending_selector`, `lsp`, `markdown_preview`, `menu`, `multi_buffer`, `node_runtime`, `open_path_prompt`, `outline`, `outline_panel`, `paths`, `platform_title_bar`, `project`, `project_panel`, `project_symbols`, `prompt_store`, `recent_projects`, `release_channel`, `search`, `session`, `settings`, `settings_ui`, `sidebar`, `snippet_provider`, `snippets_ui`, `sqlez`, `svg_preview`, `tab_switcher`, `tabular_data_preview`, `task`, `tasks_ui`, `terminal`, `terminal_view`, `text`, `theme`, `theme_selector`, `theme_settings`, `toolchain_selector`, `ui`, `util`, `watch`, `which_key`, `workspace`, `zed_actions`

Plus from `wasm_remote`: `git`, `rope`.

**14 third-party keys to copy from the root table (values as they stand in our `Cargo.toml` today) [verified]:**

| Key | Root value (truncated) |
| --- | --- |
| `anyhow` | `"1.0.86"` |
| `async-channel` | `"2.5.0"` |
| `async-trait` | `"0.1"` |
| `base64` | `"0.22"` |
| `futures` | `"0.3.32"` |
| `log` | `{ version = "0.4.16", features = ["kv_unstable_serde", "serde"] }` |
| `semver` | `{ version = "1.0", features = ["serde"] }` |
| `serde` | `{ version = "1.0.221", features = ["derive", "rc"] }` |
| `serde_json` | `{ version = "1.0.144", features = ["preserve_order", "raw_value"] }` |
| `smallvec` | `{ version = "1.6", features = ["union", "const_new"] }` |
| `smol` | `"2.0"` — then `[patch.crates-io] smol` already points at `vendor/smol_wasm` |
| `uuid` | `{ version = "1.1.2", features = ["v4", "v5", "v7", "serde"] }` |
| `wasm-bindgen` | `"0.2.120"` |
| `agent-client-protocol` | `{ version = "=2.0.0", features = ["unstable"] }` — already patched to `vendor/agent_client_protocol_patch` |

`languages` **must** be declared as `{ path = "../crates/languages", default-features = false }` once the zed-web feature split is ported. zed-web’s workspace key is `languages = { path = "crates/languages", default-features = false }` at their `Cargo.toml:404`. Our root key is `languages = { path = "crates/languages" }` with **no** `default-features = false` [verified, line 403]. Our `crates/languages` currently has no `default = [...]` feature (only `test-support` / `load-grammars`) [verified]; zed-web’s `languages` crate added `default = ["native-adapters", "python-support"]`. Phase 3 owns that drift. The web workspace key still needs to exist (it is one of the 79); the `default-features = false` flag is required the moment those defaults exist, and is harmless while they do not.

`[patch]` still does not cross workspaces. The web graph will also need the git-URL patches Phase 2 has not taken yet (`tree-sitter`, `async-tar`, `lsp-types`) when those forks land. Not a Phase 4 crate-count issue; it is why first light is not “copy four crates and build”.

### 2.4 `zed_web_server` — **root** workspace (`crates/zed_web_server`)

Native axum binary `zed-web-server`. Does **not** depend on `wasm_rpc` / `wasm_remote` / `zed_web_workspace` / `gpui` / any other first-party Zed crate [verified, `Cargo.toml` and `main.rs` imports: `axum`, `anyhow`, `clap`, `tokio`, `reqwest`, `notify`, …]. JSON RPC is reimplemented server-side, not shared as a Rust crate.

**Files (15), 12273 lines**

| Lines | Path |
| ---: | --- |
| 43 | `Cargo.toml` |
| 880 | `src/agent_rpc.rs` |
| 277 | `src/auth.rs` |
| 129 | `src/auth_callback.rs` |
| 309 | `src/debug_adapter.rs` |
| 1209 | `src/extension_rpc.rs` |
| 850 | `src/fs_rpc.rs` |
| 1599 | `src/git_rpc.rs` |
| 304 | `src/highlight_rpc.rs` |
| 908 | `src/main.rs` |
| 1925 | `src/process_rpc.rs` |
| 1609 | `src/rpc.rs` |
| 922 | `src/sql_rpc.rs` |
| 944 | `src/terminal_rpc.rs` |
| 365 | `src/workspace_state.rs` |

**Tests [verified]:** 71 `#[test]` / `#[tokio::test]` in-source. `git_rpc.rs` and `debug_adapter.rs` have **zero**. No `tests/` directory. Reference README: `cargo test -p zed_web_server --bin zed-web-server`.

**`.workspace = true` (root table, because this crate stays a root member):** `anyhow`, `base64`, `clap` (+ `features = ["env"]`), `futures`, `rand`, `reqwest`, `serde`, `serde_json`, `sha2`, `tempfile`, `tokio` (+ `features = ["full"]`), `toml`, `tracing`, `url`, `urlencoding`, `portable-pty`, and Linux `libc`.

**Direct crates.io:**

| Dep | Version req |
| --- | --- |
| `axum` | `0.6`, features `headers`, `ws` |
| `flate2` | `1` |
| `fs2` | `0.4` |
| `hex` | `0.4` |
| `hmac` | `0.12` |
| `mime_guess` | `2` |
| `notify` | `9.0.0-rc.4` |
| `rusqlite` | `0.32`, features `bundled` |
| `tar` | `0.4` |
| `tracing-subscriber` | `0.3`, features `env-filter` |
| `walkdir` | `2` |

Add `crates/zed_web_server` to root `members` and `zed_web_server = { path = "crates/zed_web_server" }` only if something else inherits it; the binary does not need to be in `[workspace.dependencies]` for `cargo -p zed_web_server`.

---

## 3. Does `zed_web_server` in the root workspace drag wasm into the desktop build?

**Wasm forks / web `[patch]`: no.** [verified] The server’s `Cargo.toml` names none of `wasm_rpc`, `wasm_remote`, `zed_web_workspace`, `smol_wasm`, `url_wasm`, `tree_sitter_wasm`, `gpui_web`. Web `[patch]` lives only in `web/Cargo.toml` and does not apply to a root-workspace build (§3.2). `url` on the server is the root workspace `url` (crates.io 2.5.7), not the wasm fork.

**Root `Cargo.lock`: yes, it will change** — not because of wasm, because of a new member and two packages the lockfile does not have.

Adding any new workspace member writes a `[[package]]` for that member. On top of that, this session parsed the current root `Cargo.lock` (1820 packages) [verified]:

| Server dep | In root lock today? | Consequence of adding the crate |
| --- | --- | --- |
| `axum` 0.6 | **Yes**, `0.6.20`. Only reverse-dep: **`collab` 0.44.0**. Lock already lists `headers` and `tokio-tungstenite 0.20.1` on that axum, i.e. `headers`/`ws` features look already on | Likely no new axum version. Feature unification with `collab` is probably a no-op. **Not proven** without generating a lock (would edit `Cargo.lock`; not done). |
| `flate2`, `fs2`, `hex`, `hmac`, `mime_guess`, `walkdir`, `tracing-subscriber`, `portable-pty`, `libc` | Present at compatible versions | Unlikely to add a new major line; still `[inferred]` for exact feature bits |
| `notify` `9.0.0-rc.4` | Present as **git** `zed-industries/notify` rev `d842f16…`, version `9.0.0-rc.4`, plus crates.io `notify 6.1.1` | Root `[patch.crates-io] notify` already redirects crates.io notify to that git rev. The server’s `"9.0.0-rc.4"` req should resolve to the patched git crate, not a second crates.io 9.x. That is the intended desktop notify, not a wasm fork. |
| **`rusqlite`** | **`ABSENT`** (no package named `rusqlite`). `libsqlite3-sys 0.30.1` exists for `sqlez` and `sqlx-sqlite` | **New package.** `bundled` may also move `libsqlite3-sys` or add a second version. Transitive set not measured (would require a lockfile generate). |
| **`tar`** | **`ABSENT`** (no package, no reverse-deps) | **New package.** Used by `debug_adapter.rs` (`tar::Archive`) and would also be a direct dep of `zed_web_workspace` on the web side, which does not touch this lock. |
| workspace-inherited `anyhow` / `tokio` / `reqwest` / … | Already present | Version reqs are the root table; no new names from those keys |

So: **no wasm artifact in the desktop graph**, and **the nine §9 crate `source` fields are not implied to change** by this crate’s own deps (it does not depend on those nine). The lockfile is still not byte-identical: `zed_web_server` + `rusqlite` + `tar` (+ rusqlite’s unknown transitives). That is allowed by §9 as worded — §9 cares about those nine sources and about `manifest_path` under `web/` — but anyone treating “lockfile hash unchanged” as the Phase 4 gate will see a real diff. Report it rather than paper over it.

`collab` already depending on axum 0.6 is why axum is not a new third-party. It is also why enabling extra axum features on the server could in principle change `collab`’s compile; the current lock suggests those features are already unified. `[inferred]` until the crate is actually added.

---

## 4. Phase 5 “first light” — actual remaining sequence

Plan Phase 5: “Build, serve, open a project read-only in a desktop browser.” From **this** tree, that is not “Phase 4 then a browser.” Phase 2 is incomplete and Phase 3 has not started. First light is the first time `zed_web_workspace` is compiled for wasm and `zed_web_server` serves it; the compiler wall in front of that is still Phase 2 leftovers + Phase 3.

### Already done

| Step | Evidence |
| --- | --- |
| Phase 0 rebase-cost measurement | `docs/phase0-rebase-cost.md` |
| Phase 0b wasm probe of `rpc`/`remote` | `docs/phase0b-wasm-report.md` — never reached our sources; vendor wall |
| Phase 1 empty second workspace | `web/Cargo.toml`, `web/.cargo/config.toml`, root `exclude = ["web"]` |
| Phase 1 review + F1–F6 ratchets | `docs/phase1-review-round1.md`; F3/F4 recorded, not functionally closed |
| Phase 2 dependency-wall map | `docs/phase2-dependency-wall.md` |
| Phase 2 four thin forks | `web/vendor/*`, `docs/phase2-vendored.md` |
| `gpui_web` already a root member | used by the future wasm binary as `path = "../crates/gpui_web"` |
| `script/download-wasi-sdk` | present (v25) |
| `wasm32-unknown-unknown` on **stable** 1.97.1 | `rust-toolchain.toml` `targets` |
| Isolation gate | `web/check-workspace-isolation.sh` |

### Not done, in order, to “open a project read-only”

1. **Finish Phase 2 high-risk forks** (not started): redo `tree-sitter` wasm `build.rs` on git `43623ec`; port `lsp-types` `to_file_path` onto `f1783e63`; `which` 8 stub; `async-tar` wrapper without zed-web’s stale `real/`. Repeat the matching git-URL `[patch]` tables in `web/Cargo.toml`. Without this, Phase 0b’s wall is still in front of `zed_web_workspace`.
2. **Phase 3 MANIFEST + `WASM_CFG`** (not started): target-cfg moves (`fs`/`rpc`/`agent`/…), `getrandom` 0.2 `js` + 0.3 `wasm_js`, `languages` default-features, `web_time::Instant`, the 174 cfg files, §5.3 post-merge audit. Re-run the Phase 0b `rpc`/`remote` wasm check as a gate at the **end** of this step (plan §8 consequence).
3. **Phase 4 crates into the right workspaces:**
   - `web/crates/wasm_rpc`, `web/crates/wasm_remote`, `web/crates/zed_web_workspace` as web members
   - declare the **97** `[workspace.dependencies]` keys (§2.3) plus `[workspace.package]`
   - replace `smol_wasm`’s `rpc` stub with `wasm_rpc::RpcClient`
   - `crates/zed_web_server` as a **root** member (accept the `Cargo.lock` delta in §3)
4. **Phase 4 entrypoint:** `web/build.sh` as §1 (cwd=`web/` for wasm, **no** `RUSTFLAGS`, `rustup run` nightly, wasi-sdk from repo root). Copy `web/static/workspace.html`, `web/scripts/patch-wasm-bindgen-memory.sh`, `web/run.sh`.
5. **Host toolchain (this machine is missing all of it) [verified]:** install a nightly (preferably dated) with `rust-src` and `wasm32-unknown-unknown`; install `wasm-bindgen-cli 0.2.127`; first `build.sh` downloads WASI SDK v25 into `target/wasi-sdk`.
6. **Build:** `./web/build.sh` → `target/web-native/release/zed-web-server` and `web/dist/static/zed_web_workspace_bg.wasm` (+ JS glue, patched memory, assets tar, COOP/COEP served by the binary).
7. **Serve locally:** `./web/run.sh /absolute/path/to/project` → `http://127.0.0.1:8090`, token from `<project>/.zed/web-auth-token` (or `ZED_WEB_TOKEN`). Tailscale Serve (§7) is the deployment shape, **not** a first-light requirement. Desktop browser with SharedArrayBuffer (localhost is a secure context). If threads are unavailable, `WebDispatcher::new` already degrades [verified in this tree].
8. **Open a project.** The wasm app calls `workspace::open_paths` with `RemoteFs` over RPC. “Read-only” here means “a tree you can look at,” not a new product mode — the port is a full editor. Our four local panels are **not** registered in `load_core_panels`; they are Phase 6.

### Explicitly not on the first-light path

| Item | When |
| --- | --- |
| `extension_runtime_cli` / `zed-extension-runtime` | After first light, if extensions are in scope. Not in this tree. Server starts without it. |
| `Home::` RPC, `project_manager` / `tmux_sessions` / `claude_sessions` / `forward_ports` | Phase 6 |
| Docker image / `web/compose.yml` / Tailscale | Deployment, §7 |
| `web/check-wasm-time.sh` | Needs `zed_web_workspace` in the graph; run as a Phase 4/5 gate, not before |
| `web/rust-toolchain.toml` nightly | Never, if rust-analyzer is to stay on 1.97.1 |
| Pinning the web workspace members into the root `members` list | Forbidden by §3.2 (nested membership) |

### What would make first light “impossible” rather than just incomplete

Nothing in this pass was blocked. Two things are **not yet knowable** and must not be pretended otherwise:

- Whether `path = "../crates/editor"` from the web workspace actually builds editor against the **web** `[patch]` graph. §3.2 asserts it; Phase 1 never had a member that path-deps a root crate (F7: empty workspace). `[inferred]` until the first `cd web && cargo metadata` with `zed_web_workspace` as a member.
- A nightly date that can `-Z build-std` this graph. Nightly is not installed; not measured.

---

## Appendix A — rustup / Cargo facts used above

```
# [verified]
(cd web && rustup show active-toolchain)
# 1.97.1-aarch64-apple-darwin (overridden by …/rust-toolchain.toml)

rustup run nightly rustc --version
# error: toolchain 'nightly-aarch64-apple-darwin' is not installed

rustup run 1.97.1 env | grep RUSTUP_TOOLCHAIN
# RUSTUP_TOOLCHAIN=1.97.1-aarch64-apple-darwin

ls script/download-wasi-sdk
# exists; WASI SDK v25; installs to ./target/wasi-sdk

which wasm-bindgen
# not found
```

Cargo rustflags precedence (Cargo book, fetched): `CARGO_ENCODED_RUSTFLAGS` > `RUSTFLAGS` > concatenated `target.*` rustflags > `build.rustflags`. Mutually exclusive.

rustup override precedence (rustup book, fetched): `+toolchain` > `RUSTUP_TOOLCHAIN` > `rustup override set` > `rust-toolchain.toml` > default.

---

## Appendix B — zed-web `web/` files we still have to copy for a runnable entrypoint

`git ls-tree -r --name-only zedweb/zed-web -- web/` [verified]:

```
web/DOCKERHUB.md
web/Dockerfile
web/Dockerfile.dockerignore
web/README.md
web/build.sh
web/check-wasm-time.sh
web/compose.yml
web/entrypoint.sh
web/run.sh
web/scripts/patch-wasm-bindgen-memory.sh
web/scripts/test-patch-wasm-bindgen-memory.sh
web/static/workspace.html
web/sync-upstream.sh
web/test/package-lock.json
web/test/package.json
web/test/playwright.config.mjs
web/test/tests/web.spec.mjs
web/upstream-revision
web/wasm-std-instant.allowlist
```

Minimum for first light besides `build.sh`: `run.sh`, `static/workspace.html`, `scripts/patch-wasm-bindgen-memory.sh`. Docker/playwright/sync-upstream are not first light.
