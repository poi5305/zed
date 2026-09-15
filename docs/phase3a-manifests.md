# Phase 3a — MANIFEST only (Cargo.toml)

Date: 2026-09-15. Branch `andy/web-version`. No `.rs` files touched. No commit.

Specs followed as already-verified: `docs/web-zed-plan.md` §4.1, §5.1, §5.3, §9; `docs/phase2-dependency-wall.md` §1. Community ref `zedweb/zed-web` vs merge-base `fecc3273ed32643c2ea1b04a74c8780e2c9ffaf8`.

## 0. codegraph (required first call)

```
codegraph explore "Cargo.toml MANIFEST wasm target_family not wasm trash zstd extension_host migrator tree-sitter-json languages default-features getrandom web-time fs rpc agent edit_prediction language_models extensions_ui agent_ui settings debugger_ui keymap_editor settings_json gpui remote"
```

Returned 64 Rust symbols across 8 files (`project.rs`, `inlays.rs`, `editor.rs`, `keymap.rs`, `language_settings.rs`, …). Blast radius was `debugger` / `language` / `edit_prediction` / `Keymap`, not Cargo packages. A second call on `trash zstd extension_host migrator tree_sitter_json getrandom web_time languages load-grammars` returned `LanguageLoader` / `Extension` / grammar registration.

Same finding as `docs/phase2-dependency-wall.md`: the index is Rust symbols, not manifests. Everything below is from `git diff fecc3273..zedweb/zed-web -- '*/Cargo.toml' 'Cargo.toml'` and the current tree.

## 1. What changed

50 `Cargo.toml` files, plus `Cargo.lock` rewritten by Cargo to record the new first-party edges. Zero `.rs`. `crates/zed/RELEASE_CHANNEL` still `dev`. `web/` and existing `docs/` reports not touched.

`git diff --stat -- Cargo.toml '**/Cargo.toml'`:

```
 50 files changed, 157 insertions(+), 51 deletions(-)
```

(Root `Cargo.toml` also still carries Phase 1’s uncommitted `exclude = ["web"]`. This phase added only the `languages` `default-features` line.)

### 1.1 §4.1 wall items (all landed)

| Item | File | What |
| --- | --- | --- |
| `trash` | `crates/fs/Cargo.toml` | Moved to `[target.'cfg(not(target_family = "wasm"))'.dependencies]` together with `async-tar`, `libc`, `tempfile`, `is_executable`, `notify`. `git` / `smol` stay in `[dependencies]` (wasm still needs them; zed-web duplicated them in both tables — not copied). |
| `zstd` | `crates/rpc/Cargo.toml`, `crates/agent/Cargo.toml`, `crates/edit_prediction/Cargo.toml` | Moved to `not(wasm)`. `agent` also moved `tempfile` (zed-web did; native still has it). |
| `extension_host` | `crates/language_models/Cargo.toml`, `crates/extensions_ui/Cargo.toml`, `crates/agent_ui/Cargo.toml` | Moved to `not(wasm)`. `language_models` also moved AWS Bedrock / `gpui_tokio` / `tokio` rt-multi-thread (zed-web; wasmtime edge is the wall one). `agent_ui` also moved `gpui_tokio`. |
| `migrator` | `crates/settings/Cargo.toml` | Moved to `not(wasm)`. Did **not** add `wasm_remote` (crate does not exist yet). |
| `tree-sitter-json` | `crates/debugger_ui/Cargo.toml`, `crates/keymap_editor/Cargo.toml`, `crates/settings_json/Cargo.toml` | `keymap_editor`: moved `tree-sitter-json` and `tree-sitter-rust` to `not(wasm)`. `debugger_ui`: our tree never had these in `[dependencies]` (merge-base neither); added them under `not(wasm)` as zed-web did. `settings_json`: our tree already optional-gates them behind `editing`; moved the optional deps to `not(wasm)` and **kept** the `editing` feature (zed-web’s merge-base had them unconditional). |
| `languages` default-features | root `Cargo.toml` | `languages = { path = "crates/languages", default-features = false }`. `crates/zed` already has `features = ["load-grammars"]`, so the desktop binary still opts in. The `languages` crate currently has no `[features] default`, so this is a no-op for feature selection until a later phase adds `python-support` / `native-adapters`. |
| `getrandom` dummy | `crates/rpc/Cargo.toml` | Added zed-web’s wasm table verbatim: `getrandom_02` 0.2 `js`, `getrandom` 0.3 `wasm_js`, `web-sys` / `wasm-bindgen` / `js-sys`. Did **not** replace `async-tungstenite` with `tungstenite = "0.27"` (that needs `.rs`). |
| `getrandom` `wasm_js` on gpui | `crates/gpui/Cargo.toml` | **Already present** at `[target.'cfg(target_family = "wasm")'.dependencies] getrandom = { version = "0.3.4", features = ["wasm_js"] }`. Left alone. Did not add zed-web’s `smol.workspace = true` under wasm (that assumes a root `[patch]` onto `smol_wasm`). |
| `web-time` | 38 crate manifests | `web-time.workspace = true` only. Workspace key already `web-time = "1.1.0"`. No `.rs` Instant swaps. |

### 1.2 Other MANIFEST graph prunes applied (from the same git diff)

These are not in the §4.1 table but are the same class of `not(wasm)` moves, and they do not depend on Phase 4 crates:

| File | What |
| --- | --- |
| `crates/acp_thread/Cargo.toml` | `portable-pty` → `not(wasm)` + `web-time`. Did not add `smol` (unused without `.rs`). |
| `crates/activity_indicator/Cargo.toml` | `extension_host` → `not(wasm)` + `web-time`. |
| `crates/agent_servers/Cargo.toml` | `tempfile` → `not(wasm)`. Did not add `libc`/`nix` unix deps (those pair with `.rs`). |
| `crates/client/Cargo.toml` | Native-only: `async-tungstenite`, `fs`, `gpui_tokio`, `http_client_tls`, `paths`, `tiny_http`, `tokio`, `worktree`, `smol`, `zed_credentials_provider`, `rpc`. Wasm: `rpc = { path = "../rpc", default-features = false, features = ["gpui"] }`. TLS target cfgs narrowed with `not(wasm)`. `proxy_handshake` left in `[dependencies]` (zed-web kept it there and also duplicated it under `not(wasm)` — duplicate not copied). |
| `crates/git_ui/Cargo.toml` | `sysinfo` → `not(wasm)` + `web-time`. Did not add `proto`/`remote`/`zeroize`/`js-sys`/`windows` (pair with `.rs` / wasm stubs). |
| `crates/prompt_store/Cargo.toml` | `heed` → `not(wasm)` (no lmdb on wasm). |
| `crates/recent_projects/Cargo.toml` | `dev_container` + `extension_host` → `not(wasm)`. This is **not** the §5.3 `PathPromptOptions.files` change (that is `.rs`). |
| `crates/settings_ui/Cargo.toml` | `audio`, `codestral`, `edit_prediction`, `edit_prediction_ui`, `extension_host` → `not(wasm)`. Did not add wasm `extensions_ui`. |
| `crates/sidebar/Cargo.toml` | `agent_ui` `features = ["audio"]` native-only; wasm `agent_ui` with empty features; `recent_projects` native-only. |
| `crates/sqlez/Cargo.toml` | `libsqlite3-sys` + `sqlformat` → `not(wasm)`. Did not add `wasm_rpc` / `wasm_thread` / `web-sys`. |
| `crates/terminal/Cargo.toml` | `libc` + `sysinfo` → `not(wasm)` + `web-time`. Did not add `wasm_rpc` / `web-sys`. This is **not** the §5.3 Shift+Click deletion (that is `.rs`). |

`web-time` only (no other zed-web hunk applied): `clock`, `codestral`, `context_server`, `edit_prediction_context`, `edit_prediction_ui`, `editor`, `extension`, `extension_host`, `gpui_util`, `http_proxy`, `language`, `language_tools`, `lsp`, `multi_buffer`, `oauth_callback_server`, `project`, `project_panel`, `proto`, `remote`, `search`, `tabular_data_preview`, `terminal_view`, `text`, `theme`, `workspace`, `worktree`, `zlog`.

## 2. Cargo.lock

### 2.1 Snapshot

```
$ cp Cargo.lock /tmp/lock-before-p3a
$ wc -c Cargo.lock /tmp/lock-before-p3a
  501613 Cargo.lock
  501613 /tmp/lock-before-p3a
```

### 2.2 After `cargo metadata --no-deps`

Lock stayed byte-identical. `--no-deps` does not refresh workspace members’ dependency lists.

### 2.3 After `web/check-workspace-isolation.sh` (root `cargo metadata` without `--no-deps`)

Lock grew by first-party edges only. `cmp` then failed; investigation:

- **0** `[[package]]` added or removed (name/version/source keys).
- **0** `version =` / `source =` / `checksum =` / `name =` lines in the diff.
- Every added line is one of: `"web-time"`, `"tree-sitter"`, `"tree-sitter-json"`, `"getrandom 0.2.16"`, `"getrandom 0.3.4"`, `"js-sys"`, `"wasm-bindgen"`, `"wasm-bindgen-futures"`, `"web-sys"`.

Those packages were already in the lock (`web-time` 1.1.0 via gpui/scheduler; `getrandom` 0.2.16 already lists `js-sys` / `wasm-bindgen`; `getrandom` 0.3.4 already has `wasm_js` via gpui). The dummy deps did not introduce a new crate or move a checksum.

This is the case the spec called expected for new `web-time` / `getrandom` edges, and it does **not** change any existing package version or source. Accepted.

Full `diff /tmp/lock-before-p3a Cargo.lock`:

```
131a132
>  "web-time",
198a200
>  "web-time",
314a317
>  "web-time",
551a555
>  "web-time",
3197a3202
>  "web-time",
3209a3215
>  "web-time",
3363a3370
>  "web-time",
3775a3783
>  "web-time",
4934a4943
>  "tree-sitter",
4935a4945
>  "tree-sitter-json",
5424a5435
>  "web-time",
5520a5532
>  "web-time",
5581a5594
>  "web-time",
5668a5682
>  "web-time",
6173a6188
>  "web-time",
6256a6272
>  "web-time",
6763a6780
>  "web-time",
7284a7302
>  "web-time",
7773a7792
>  "web-time",
8369a8389
>  "web-time",
9462a9483
>  "web-time",
9595a9617
>  "web-time",
9841a9864
>  "web-time",
10456a10480
>  "web-time",
11244a11269
>  "web-time",
11781a11807
>  "web-time",
14005a14032
>  "web-time",
14101a14129
>  "web-time",
14287a14316
>  "web-time",
15089a15119
>  "web-time",
15540a15571,15572
>  "getrandom 0.2.16",
>  "getrandom 0.3.4",
15541a15574
>  "js-sys",
15551a15585,15588
>  "wasm-bindgen",
>  "wasm-bindgen-futures",
>  "web-sys",
>  "web-time",
16219a16257
>  "web-time",
18109a18148
>  "web-time",
18302a18342
>  "web-time",
18351a18392
>  "web-time",
18373a18415
>  "web-time",
18394a18437
>  "web-time",
22396a22440
>  "web-time",
22432a22477
>  "web-time",
23384a23430
>  "web-time",
```

The `getrandom` / `js-sys` / `wasm-bindgen` / `web-sys` block is on the `rpc` package (wasm dummy table). The `tree-sitter` / `tree-sitter-json` block is on `debugger_ui`.

Target-cfg *moves* (`trash`, `zstd`, `extension_host`, `migrator`, …) left no lock trace, as predicted: the lock is target-agnostic and those crates were already named.

## 3. Acceptance outputs

### 3.1 `cargo metadata --no-deps --format-version 1`

```
$ CARGO_TARGET_DIR=target/web-probe cargo metadata --no-deps --format-version 1
EXIT:0
workspace_root= /Users/andy/go/src/github.com/poi5305/zed
workspace_members= 257
web_member_manifests= []
```

Reconfirm with `--offline`:

```
$ CARGO_TARGET_DIR=target/web-probe cargo metadata --no-deps --format-version 1 --offline
workspace_root= /Users/andy/go/src/github.com/poi5305/zed
workspace_members= 257
```

Member count still **257**. No package with `manifest_path` under `web/`.

### 3.2 Nine crates’ `source` in root `Cargo.lock`

```
## tree-sitter (expect 43623ec)
  [OK] 0.27.0 | git+https://github.com/tree-sitter/tree-sitter?rev=43623ec9bf0eaaf7113285c46e8a09018f181b18#43623ec9bf0eaaf7113285c46e8a09018f181b18
## lsp-types (expect f1783e63)
  [OK] 0.95.1 | git+https://github.com/zed-industries/lsp-types?rev=f1783e63a7f4eb4397bf51d4148b4895a1f7ab16#f1783e63a7f4eb4397bf51d4148b4895a1f7ab16
## which (expect 8.0.5)
  [OK] 8.0.5 | registry+https://github.com/rust-lang/crates.io-index
## url (expect 2.5.7)
  [OK] 2.5.7 | registry+https://github.com/rust-lang/crates.io-index
## smol (expect 2.0.2)
  [OK] 2.0.2 | registry+https://github.com/rust-lang/crates.io-index
## async-tar (expect bd3ad6f)
  [OK] 0.6.1 | git+https://github.com/zed-industries/async-tar?rev=bd3ad6f89df9a9da7a8535958756d6bf465936a0#bd3ad6f89df9a9da7a8535958756d6bf465936a0
## alacritty_terminal (expect 4c129667)
  [OK] 0.26.1-dev | git+https://github.com/zed-industries/alacritty?rev=4c129667ce56611becdc82de6e28218c80e2e88f#4c129667ce56611becdc82de6e28218c80e2e88f
## agent-client-protocol (expect 2.0.0)
  [OK] 2.0.0 | registry+https://github.com/rust-lang/crates.io-index
## wasm_thread (expect 0cf96c77)
  [OK] 0.3.3 | git+https://github.com/zed-industries/wasm_thread?rev=0cf96c7708dfb97ccf3da50347e25edcf75d6937#0cf96c7708dfb97ccf3da50347e25edcf75d6937
```

### 3.3 `web/check-workspace-isolation.sh` — 28/28

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

28 checks, 0 failures
not covered here: §9 bullet 4, "the desktop bundle must still build and install"
                  (script/bundle-mac; too slow and too destructive for a gate)
```

Exit 0.

Did not run `./script/clippy` or any `--release --all-features` cargo. Cargo invocations used `CARGO_TARGET_DIR=target/web-probe` except the isolation script, which uses `web/target` by design.

## 4. §5.3 — is any refusal a manifest change?

No. The four refusals were checked against every Cargo.toml this phase considered:

| Refusal | Manifest? | Action |
| --- | --- | --- |
| `crates/zed/RELEASE_CHANNEL` `dev` → `stable` | **No** (not a Cargo.toml). zed-web’s Cargo.toml list does not include `crates/zed/Cargo.toml`. | Not applied. File still contains `dev\n`. |
| `crates/terminal/src/terminal.rs` Shift+Click | **No** (`.rs`). | `crates/terminal/Cargo.toml` only moved `libc`/`sysinfo` and added `web-time`. |
| `crates/recent_projects/src/recent_projects.rs` `files: true` → `false` | **No** (`.rs`). | `crates/recent_projects/Cargo.toml` only moved `dev_container` / `extension_host`. |
| `crates/remote_server/src/server.rs` `send_blocking` → `try_send` | **No** (`.rs`). | `crates/remote_server/Cargo.toml` was **not** edited. zed-web’s only manifest hunk there is `languages = { workspace = true, features = ["python-support"] }`, which is the languages-feature split, not the log-flush change. Skipped (see below). |

Nothing under `crates/zed/` was modified.

## 5. zed-web MANIFEST changes not applied, and why

zed-web vs merge-base touched 74 `Cargo.toml` paths. Classification:

### 5.1 Crates that do not exist in this tree (Phase 2 vendors live under `web/vendor/`; Phase 4 crates are not here)

`agent_client_protocol_patch`, `alacritty_terminal`, `async_tar_wasm` (+ `real/`), `extension_runtime_cli`, `lsp_types_wasm`, `smol_wasm`, `tree_sitter_wasm`, `url_wasm`, `wasm_remote`, `wasm_rpc`, `wasm_thread_patch`, `which_wasm` (+ `real/`), `zed_web_poc`, `zed_web_server`, `zed_web_workspace`.

Applying any of these as root members would break the 257 count and §9 isolation.

### 5.2 Root `Cargo.toml` hunks refused (would contaminate the desktop graph)

zed-web’s root diff is +49/−2. Taken: `languages … default-features = false` only.

Refused:

- New `members` (`alacritty_terminal`, `async_tar_wasm`, `smol_wasm`, `tree_sitter_wasm`, `wasm_rpc`, `wasm_remote`, `which_wasm`, `zed_web_*`, `extension_runtime_cli`, `lsp_types_wasm`) — member count, crates absent.
- `alacritty_terminal = { path = "crates/alacritty_terminal" }` — would replace git `4c129667` (§9 / §4: port cfgs onto that rev, do not import the tree).
- `jsonschema = { version = "0.37.0", default-features = false }` — our table is already `{ version = "0.51", default-features = false }`. Taking 0.37.0 would roll a version.
- `wasm_rpc` / `wasm_remote` workspace.dependency keys — crates absent (Phase 4).
- All new `[patch."https://…"]` and `[patch.crates-io]` forks (`smol`, `which`, `url`, `tree-sitter`, `async-tar`, `lsp-types`, `agent-client-protocol`, `wasm_thread`) — isolation: those live only in `web/Cargo.toml` (Phase 2).
- `[profile.web-release]` — isolation F2: already declared in `web/Cargo.toml`, not the desktop root (§3.2).

### 5.3 Manifest hunks that require `.rs` or a missing crate

| File | Skipped hunk | Why |
| --- | --- | --- |
| `crates/rpc/Cargo.toml` | `async-tungstenite` → `tungstenite = "0.27"` | Native `rpc` still compiles against `async-tungstenite`. Replacing it without the wasm conn `.rs` breaks the desktop path. |
| `crates/gpui/Cargo.toml` | wasm `smol.workspace = true` | Assumes root `[patch]` → `smol_wasm`. On this root workspace that would pull native smol (async-io) into a wasm gpui graph. `getrandom` `wasm_js` already present. |
| `crates/gpui_web/Cargo.toml` | `wasm_thread` git URL → `version = "0.3"`; extra `web-sys` features | The version form assumes a root patch. Changing the git source would violate §9 (`0cf96c77`). |
| `crates/util/Cargo.toml` | Move `smol` from `not(wasm)` to always | zed-web needs always-on smol because they patch it at the root. Our `util` already has smol native-only, which is the right root-workspace shape. |
| `crates/languages/Cargo.toml` | `default = ["native-adapters", "python-support"]`, `pet*` optional | Requires `.rs` cfg on the pet adapters. Making `pet` optional without that fails native compile. |
| `crates/remote_server/Cargo.toml` | `languages = { workspace = true, features = ["python-support"] }` | Only needed once `languages` grows that feature. Adjacent to a §5.3 file but not the refused hunk; skipped because the feature does not exist. |
| `crates/extensions_ui/Cargo.toml` | wasm `wasm_remote` + `smallvec`/`snippet_provider`/`theme` | `wasm_remote` does not exist; extra deps pair with the wasm extension-host stub `.rs`. |
| `crates/settings/Cargo.toml` | wasm `wasm_remote` | Crate absent (Phase 4). |
| `crates/sqlez/Cargo.toml` | wasm `wasm_rpc` / `wasm_thread` / `web-sys` | Crates / worker bridge are Phase 4 + `.rs`. |
| `crates/terminal/Cargo.toml` | wasm `wasm_rpc` / `web-sys` | Same. |
| `crates/settings_ui/Cargo.toml` | wasm `extensions_ui` | Unused without `.rs`. |
| `crates/git_ui/Cargo.toml` | `proto`/`remote`/`zeroize`/`js-sys`/`windows` | Unused without `.rs`. |
| `crates/net/Cargo.toml` | wasm `futures` | Unused without `.rs`. |
| `crates/oauth_callback_server/Cargo.toml` | wasm `futures`/`url` | `url` already native-only; wasm stubs are `.rs`. |
| `crates/acp_thread/Cargo.toml` | add `smol` | Unused without `.rs`. |
| `crates/agent_servers/Cargo.toml` | unix `libc`/`nix` | Unused without `.rs`. |
| `crates/benchmarks/Cargo.toml` | `indoc`/`rpc` | Not in the wasm graph. |
| `crates/tabular_data_preview/Cargo.toml` | extra `[dev-dependencies]` | Not required for the wall; `web-time` was added. |
| `crates/fs/Cargo.toml` | duplicate `git`/`smol` under `not(wasm)` | Already in `[dependencies]`; duplicating is a no-op. |
| `crates/client/Cargo.toml` | duplicate `proxy_handshake` under `not(wasm)` | Already in `[dependencies]`. |

`tasks_ui` still depends on `tree-sitter-json` unconditionally. That is deliberate: phase2-dependency-wall §1 says zed-web kept it and used the WASI SDK. Not gated here.

## 6. Notes for the cfg-gate agent (not done here)

Moving a dep to `not(wasm)` leaves native compile unchanged and will fail wasm type-check of the same crate until the matching `#[cfg(not(target_family = "wasm"))]` lands in `.rs`. That is Phase 3b’s 174 `WASM_CFG` files. This phase does not cfg-gate source.

`crates/rpc` still names `async-tungstenite` in `[dependencies]`, so a root `cargo check -p rpc --target wasm32-unknown-unknown` will still see that crate. The getrandom dummy deps are in place for feature unification once that probe is re-run from `web/` (where rustflags carry `--cfg getrandom_backend="wasm_js"`).
