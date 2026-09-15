# Phase 4b — three wasm crates into the web workspace

- **Date:** 2026-09-15
- **Branch:** `andy/web-version` (left as found; no commit)
- **Spec:** `docs/web-zed-plan.md` §2.2, §3.2, §4.2, Phase 4
- **Prior research used, not redone:** `docs/phase4-entrypoint-research.md`
- **Reference:** `zedweb/zed-web` `crates/{wasm_rpc,wasm_remote,zed_web_workspace}`

Wrote only under `web/` plus this report. Did not touch `crates/`, root `Cargo.toml`, root `Cargo.lock`, `web/check-workspace-isolation.sh`, `web/check-refusals.sh`, `web/build.sh`, or `web/vendor/`. Did not run `./script/clippy`, `--release --all-features`, or `web/build.sh`. `zed_web_server` stays a later root-workspace job.

Claims are `[verified]` when a command was run in this session.

---

## 0. codegraph (required first call)

```
codegraph explore -p /Users/andy/go/src/github.com/poi5305/zed --max-files 20 \
  "zed_web_workspace load_core_panels wasm_rpc wasm_remote RpcClient RemoteFs WebDispatcher web/crates"
```

**Result [verified].** 151 symbols / 9 files. The three Phase 4 crates are still absent from this tree’s index (they lived only on `zedweb/zed-web` until this pass copied them), so they have **no callers, no blast radius, and no tests in the index**. What came back is the already-present web platform:

| Symbol | Where | Callers / tests |
| --- | --- | --- |
| `WebDispatcher` | `crates/gpui_web/src/dispatcher.rs:146` | 2 callers in `crates/gpui_web/src/http_client.rs`; no tests within 3 hops |
| `WebDispatcher::new` | `:156` | `supports_threads` is `multithreaded` ∧ `allow_threads` ∧ `shared_memory_supported()` ∧ `wait_async_supported()` |

Manifests are not in the index (confirmed three times across earlier phases). File lists and `[workspace.dependencies]` keys below are from `git ls-tree` / `git show` / `git cat-file` of `zedweb/zed-web` plus a parse of the copied `Cargo.toml` files, matching the research document’s 97.

---

## 1. What moved

Copied with `git archive zedweb/zed-web crates/wasm_rpc crates/wasm_remote crates/zed_web_workspace` into `web/crates/<name>/`. All **19 files are byte-identical** to the reference blobs (`git show | cmp`). No rustfmt, no import rewrites: the crates already speak `.workspace = true`.

### 1.1 `web/crates/wasm_rpc` — 2 files, 550 lines

| Lines | Path |
| ---: | --- |
| 21 | `Cargo.toml` |
| 529 | `src/lib.rs` |

Public surface (unchanged): `RpcClient`, `connect`, `call`, `call_void`, `is_connected`, `on_notification`, `subscribe_reconnect`.

`.workspace = true` keys (5): `anyhow`, `futures`, `serde`, `serde_json`, `wasm-bindgen`. No root-member crates.

Did **not** replace `web/vendor/smol_wasm/src/rpc.rs` (vendor is out of scope). The stub remains; `wasm_rpc` is now a sibling the stub can later `pub use`.

### 1.2 `web/crates/wasm_remote` — 6 files, 2505 lines

| Lines | Path |
| ---: | --- |
| 26 | `Cargo.toml` |
| 54 | `README.md` |
| 772 | `src/fs.rs` |
| 1623 | `src/git.rs` |
| 29 | `src/lib.rs` |
| 1 | `src/transport.rs` |

`.workspace = true` keys (15): 6 root members (`collections`, `fs`, `gpui`, `git`, `rope`, `text`) + 8 third-party + `wasm_rpc`. `git` and `rope` are the two root members `zed_web_workspace` does not name.

### 1.3 `web/crates/zed_web_workspace` — 11 files, 5892 lines

| Lines | Path |
| ---: | --- |
| 120 | `Cargo.toml` |
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

`.workspace = true` keys (92, of which 79 are root members). Binary `[[bin]] name = "zed_web_workspace"`.

Line counts match `docs/phase4-entrypoint-research.md` §2 exactly.

---

## 2. `web/Cargo.toml` — 97 keys, path math, surgical edit

`[profile.web-release]` and both `[patch.*]` tables were left byte-identical. The edit added:

1. three members: `crates/wasm_rpc`, `crates/wasm_remote`, `crates/zed_web_workspace`
2. `[workspace.package]` `publish = false` / `edition = "2024"` (required: the copied manifests say `edition.workspace = true` / `publish.workspace = true`; vendor crates pin edition themselves, which is why Phase 2 never needed this)
3. `[workspace.dependencies]` with **97** keys, parsed from the three member manifests (same union the research counted)

### 2.1 The 97

| Bucket | Count | How written |
| --- | ---: | --- |
| First-party root members | **81** | root table’s value with `path = "crates/…"` rewritten to `path = "../crates/…"`; extra fields kept (`gpui` / `gpui_platform` / `languages` `default-features = false`, `collections` `version = "0.1.0"`, `tabular_data_preview`’s missing space before `}`) |
| Third-party | **14** | **verbatim** from root `[workspace.dependencies]` |
| New web crates | **2** | `wasm_rpc = { path = "crates/wasm_rpc" }`, `wasm_remote = { path = "crates/wasm_remote" }` |
| **Total** | **97** | matches the research number |

`zed_web_workspace` is a member but **not** a workspace-dependency key (nothing writes `zed_web_workspace.workspace = true`).

Third-party values copied:

```
agent-client-protocol = { version = "=2.0.0", features = ["unstable"] }
anyhow = "1.0.86"
async-channel = "2.5.0"
async-trait = "0.1"
base64 = "0.22"
futures = "0.3.32"
log = { version = "0.4.16", features = ["kv_unstable_serde", "serde"] }
semver = { version = "1.0", features = ["serde"] }
serde = { version = "1.0.221", features = ["derive", "rc"] }
serde_json = { version = "1.0.144", features = ["preserve_order", "raw_value"] }
smallvec = { version = "1.6", features = ["union", "const_new"] }
smol = "2.0"
uuid = { version = "1.1.2", features = ["v4", "v5", "v7", "serde"] }
wasm-bindgen = "0.2.120"
```

`languages` in **this** tree already has `default-features = false` (Phase 3a). The web key copies that.

### 2.2 Path math (measured, not guessed)

Workspace-dependency `path` is relative to **`web/Cargo.toml`**, not to the member crate.

| Kind | In `web/Cargo.toml` | Resolves to |
| --- | --- | --- |
| Root member (`gpui`, `fs`, …) | `../crates/<name>` | `/Users/andy/go/src/github.com/poi5305/zed/crates/<name>` |
| New web crate | `crates/<name>` | `/Users/andy/go/src/github.com/poi5305/zed/web/crates/<name>` |

`../../crates/<name>` would be right **inside** a file at `web/crates/<name>/Cargo.toml`. Those files do not use path deps; they use `.workspace = true`, so the workspace-root paths above are the ones that matter.

`cargo metadata --no-deps` confirmed the resolution, e.g. `wasm_remote` → `fs` at `/Users/andy/go/src/github.com/poi5305/zed/crates/fs`, `zed_web_workspace` → `wasm_rpc` at `/Users/andy/go/src/github.com/poi5305/zed/web/crates/wasm_rpc`.

---

## 3. Acceptance

Root lock snapshot before any cargo: `sha1 e878132c43d5165e87f2479412a9c18da40640be` (`/tmp/lock-before-p4b`).

### 3.1 `cd web && cargo metadata --no-deps --format-version 1`

**Succeeded [verified].** Parsed:

```
workspace_root: /Users/andy/go/src/github.com/poi5305/zed/web
workspace_members: 7
packages:
  agent-client-protocol  .../web/vendor/agent_client_protocol_patch/Cargo.toml
  smol                   .../web/vendor/smol_wasm/Cargo.toml
  url                    .../web/vendor/url_wasm/Cargo.toml
  wasm_remote            .../web/crates/wasm_remote/Cargo.toml
  wasm_rpc               .../web/crates/wasm_rpc/Cargo.toml
  wasm_thread            .../web/vendor/wasm_thread_patch/Cargo.toml
  zed_web_workspace      .../web/crates/zed_web_workspace/Cargo.toml
```

### 3.2 `cd web && CARGO_TARGET_DIR=../target/web-probe cargo metadata --filter-platform wasm32-unknown-unknown --format-version 1`

**Succeeded [verified] in 2.3s, stderr empty.** Parsed:

```
workspace_root: /Users/andy/go/src/github.com/poi5305/zed/web
packages: 972
resolve_nodes: 972
workspace_members: 7
```

**Package count: 972.**

The four patched crates, one copy each, `source` null, `manifest_path` under `web/vendor/`:

| name | version | source | manifest_path |
| --- | --- | --- | --- |
| `url` | 2.5.7 | `None` | `.../web/vendor/url_wasm/Cargo.toml` |
| `smol` | 2.0.2 | `None` | `.../web/vendor/smol_wasm/Cargo.toml` |
| `agent-client-protocol` | 2.0.0 | `None` | `.../web/vendor/agent_client_protocol_patch/Cargo.toml` |
| `wasm_thread` | 0.3.3 | `None` | `.../web/vendor/wasm_thread_patch/Cargo.toml` |

This is the same “path dep on a root crate still builds against the web `[patch]` graph” fact the throwaway probe measured; now it holds for the real 79-crate `zed_web_workspace` graph.

`web/Cargo.lock` was rewritten by this metadata (now 17064 lines). That file lives under `web/` and is supposed to pin the web graph (isolation V9). Root lock was not used.

### 3.3 Root `Cargo.lock` unchanged

```
$ diff /tmp/lock-before-p4b Cargo.lock
# (empty)
$ shasum /tmp/lock-before-p4b Cargo.lock
e878132c43d5165e87f2479412a9c18da40640be  /tmp/lock-before-p4b
e878132c43d5165e87f2479412a9c18da40640be  Cargo.lock
```

(`git status` still shows `M Cargo.lock` versus **HEAD**; that dirt predates this pass. The snapshot-to-now diff is empty.)

### 3.4 Isolation gate — **not all green** (spec contradiction)

`./web/check-workspace-isolation.sh` exit 1. **32 checks, 3 failures.** Root lock still identical after the script.

The script prints 32 because **V11 is two assertions** (native `--all-targets` and `--target wasm32-unknown-unknown`). Counted as one heading that is how the “currently 31” figure was described; the extra assertion is V11’s second cargo.

29 checks still pass, including §9.1 / §9.2 (root member count still 257, no root package under `web/`), F1–F4, F6, V1–V9, M1–M3, S1, and `[profile.web-release]` / rustflags.

**Failures, verbatim:**

```
FAIL V10 the web wasm32 graph is free of async-io/async-process/polling/errno/rustix (§4.1's wall)
       expected: none
       actual:   blocking,polling
FAIL V11 cd web && cargo check --workspace --all-targets succeeds
       expected: exit 0
       actual:   exit 101
       error[E0432]: unresolved import `merman_render::text::VendoredFontMetricsTextMeasurer` error[E0599]: no associated function or constant named `parity` found for struct `TextMeasurementPolicy` in the current scope error: could not compile `merman` (lib) due to 2 previous errors
FAIL V11 cd web && cargo check --workspace --target wasm32-unknown-unknown succeeds
       expected: exit 0
       actual:   exit 101
       error[E0432]: unresolved import `util::paths::home_dir` error: could not compile `paths` (lib) due to 1 previous error; 1 warning emitted error[E0433]: cannot find `unix` in `os`
```

V10 from this session’s 972-package graph [verified]:

| wall crate | in graph? | reverse-dep |
| --- | --- | --- |
| `blocking` 1.7.0 | yes (crates.io) | `async-fs` |
| `polling` 3.11.0 | yes (crates.io) | `alacritty_terminal` |
| `async-io` / `async-process` / `errno` / `rustix` | no | — |

`alacritty_terminal` is reached because `zed_web_workspace` names `terminal`. `async-fs` is reached through the now-full graph, not through `smol_wasm` (smol still has no `async-io` on wasm). V10 was written when the web workspace’s only members were the four vendor forks; that graph really was wall-free. Adding `zed_web_workspace` makes V10 a Phase 2 leftover (`async-tar` / `alacritty` / `which` forks not taken — `docs/phase2-vendored.md`).

V11 `--workspace` **compiles every member**. That is a real `cargo check` of `zed_web_workspace` and therefore of `editor` / `gpui` / `paths` / `merman`. This pass was told **not** to compile wasm (no nightly, no WASI SDK) and **not** to edit `crates/` (another agent is mid-edit there — the native `merman` errors are that in-flight tree, not a 4b mistake). Wasm V11 dies on `util::paths::home_dir` (`#[cfg(not(target_family = "wasm"))]`, already in Phase 0b / the research note).

**Contradiction, not a botched copy.** The same instructions require:

1. add `zed_web_workspace` as a web member (this pass)
2. keep isolation all-green (V10/V11 assume a vendor-only workspace)
3. do not edit the isolation script
4. do not compile wasm
5. do not drop members just to make a check pass

(1) and (2) cannot hold together until Phase 2’s remaining forks land and the other agent’s `crates/` tree compiles. Dropping `zed_web_workspace` / `wasm_remote` from `members` would turn V10/V11 green again and would be picking the input the gate wants. Not done.

`zed_web_server` was correctly left out (native, root workspace, later).

### 3.5 What was not run

- no `web/build.sh`
- no `./script/clippy`
- no `--release --all-features`
- no nightly / WASI SDK install
- V11’s `cargo check` ran **inside the isolation script**, which this pass was required to execute; this pass did not invoke `cargo check` itself

---

## 4. Scope self-check

```
$ git status --short -- web docs/phase4b-web-crates.md
?? web/
?? docs/phase4b-web-crates.md
```

This pass created `web/crates/{wasm_rpc,wasm_remote,zed_web_workspace}/` (19 files, byte-identical to the reference), edited `web/Cargo.toml` (members + `[workspace.package]` + 97 dependency keys; profile/patch untouched), and let `cargo metadata` refresh `web/Cargo.lock`. `web/` was already untracked as a directory.

Did not create or modify anything under `crates/`. Pre-existing `M crates/…` / `M Cargo.lock` / `M Cargo.toml` lines in a full `git status` are the other agent’s Phase 3 tree, not this diff.

---

## 5. Outcome

| Gate | Result |
| --- | --- |
| Three crates in `web/crates/` | done, 19 files, byte-identical to `zedweb/zed-web` |
| 97 `[workspace.dependencies]` keys | **97 = 81 + 14 + 2**, matches research |
| Path math | `../crates/<name>` from `web/Cargo.toml`; measured |
| `cargo metadata --no-deps` | workspace_root = `…/zed/web`, 7 members including the three |
| wasm32 metadata | **972 packages**; four forks → `web/vendor/*`, `source` null |
| Root `Cargo.lock` | byte-identical to pre-phase snapshot |
| Isolation all-green | **做不到** — V10 (`blocking,polling`) and both V11 checks fail for the reasons in §3.4. Not papered over. |
