# Phase 4c — `zed_web_server` into the root workspace

- **Date:** 2026-09-15
- **Branch:** `andy/web-version` (left as found; **no commit**)
- **Spec:** `docs/web-zed-plan.md` §2.2, §3.2, §9; `docs/phase4-entrypoint-research.md` §2.4, §3
- **Reference:** `zedweb/zed-web:crates/zed_web_server`
- **Prior reports used, not redone:** `docs/phase4-entrypoint-research.md`, `docs/phase5-wasm-remote.md`

Wrote `crates/zed_web_server/`, two lines in the root `Cargo.toml` (`members` + `[workspace.dependencies]`), the additive `Cargo.lock` delta, the member-count assertion in `web/check-workspace-isolation.sh` (257 → 258), and this report. Did not touch `web/vendor/`, `web/crates/`, `web/build.sh`, `web/check-refusals.sh`. Did not `git stash` / `checkout` / `restore` / `reset` / `rebase`. Did not commit. `README.md` `> [!IMPORTANT]` lines left in place.

Claims are `[verified]` when a command was run in this session and its output read.

Baseline lock: `cp Cargo.lock /tmp/lock-before-p4c` before any edit.

---

## 0. codegraph (required first call)

```
codegraph explore "Fs trait path_exists is_path_case_sensitive requires_poll_watcher GitRepository load_commit GitCommitTemplate CommitDataReader"
codegraph explore "GitRepository load_commit ignore_shallow_boundary is_shallow_boundary PollWatcher MaxFilesWatch tempfile keep"
```

`zed_web_server` is not in this tree's index (it lived only on `zedweb/zed-web` until this pass). What came back is the **current** native / wasm_remote surface this server has to speak JSON to:

| Symbol | File:line | Shape that matters here |
| --- | --- | --- |
| `Fs::path_exists` / `is_path_case_sensitive` / `requires_poll_watcher` | `crates/fs/src/fs.rs` | Sync. Not on the wire. `wasm_remote` answers them locally (`docs/phase5-wasm-remote.md`). |
| `Fs::read_dir_with_types` | — | **Removed from the trait.** `wasm_remote` no longer calls it. |
| `GitRepository::load_commit` | `crates/git/src/repository.rs:921` / impl `:1448` | 4th parameter is `ignore_shallow_boundary`. When `false` and the SHA is in `.git/shallow`, returns empty files + `is_shallow_boundary: true`. |
| `is_shallow_boundary_commit` | `:634` | Reads `common_dir/shallow`, `rev-parse --verify '{commit}^{commit}'`, compares. |
| `GitCommitTemplate` | `:1344` | `{ template: String }`. No `Deserialize`. Server already returns `{"template": ...}`. |
| `CommitDataReader` | `:140` | Private fields. `from_async_resolver` gone. `for_test` is test-only. `wasm_remote` returns `Err` rather than wrapping `GitRepository::commit_data`. |
| `notify::EventKindMask` / `PollWatcher` / `ErrorKind::MaxFilesWatch` | used by `crates/fs/src/fs_watcher.rs` | Present on our patched notify git `d842f16`. |
| `tempfile::TempDir::keep` | used by `crates/http_client`, `crates/zed` | Present on tempfile 3.20. |

The server does **not** implement `Fs` or `GitRepository`. It is a native JSON-RPC process (`axum` + `std::process::Command` git + `portable-pty` + `rusqlite`). Trait drift shows up as **RPC request/response shape**, not as `E0046`/`E0407`.

---

## 1. What moved

Copied with `git archive zedweb/zed-web crates/zed_web_server` into `crates/zed_web_server/`. Then:

- `src/main.rs` (908 lines on the ref) became `src/lib.rs` with `pub async fn run()`, plus a 6-line binary that calls it. The acceptance command is `cargo check --lib --bins`; the reference crate had **only** a `[[bin]]`, so `--lib` would have failed with `no library targets found`.
- `git_rpc.rs` `load_commit` follows the current `GitRepository::load_commit` flag (below).
- Manifest rewritten to inherit every root-table key it can.

| Lines | Path |
| ---: | --- |
| 52 | `Cargo.toml` |
| 6 | `src/main.rs` |
| 907 | `src/lib.rs` (was ref `src/main.rs`) |
| 880 | `src/agent_rpc.rs` |
| 277 | `src/auth.rs` |
| 129 | `src/auth_callback.rs` |
| 309 | `src/debug_adapter.rs` |
| 1209 | `src/extension_rpc.rs` |
| 850 | `src/fs_rpc.rs` |
| 1633 | `src/git_rpc.rs` (was 1599; +shallow-boundary helper) |
| 304 | `src/highlight_rpc.rs` |
| 1925 | `src/process_rpc.rs` |
| 1609 | `src/rpc.rs` |
| 922 | `src/sql_rpc.rs` |
| 944 | `src/terminal_rpc.rs` |
| 365 | `src/workspace_state.rs` |

Root `Cargo.toml`:

```
members += "crates/zed_web_server"          # after zed_env_vars, before zeta_prompt
zed_web_server = { path = "crates/zed_web_server" }
```

The path key is not required for `cargo -p zed_web_server` (research §2.4). It is listed because every other root member is.

---

## 2. Dependency declarations

### 2.1 Inherited (`.workspace = true`)

`anyhow`, `base64`, `clap` (+ `features = ["env"]`), `futures`, `hex`, `portable-pty`, `rand`, `reqwest` (the root `zed-reqwest` git pin), `serde`, `serde_json`, `sha2`, `tempfile`, `tokio` (+ `features = ["full"]`), `toml`, `tracing`, `url`, `urlencoding`, `walkdir`, Linux `libc`.

`hex` and `walkdir` were literal `"0.4"` / `"2"` on the reference; they are workspace keys here (`"0.4.3"` / `"2.5"`).

### 2.2 Direct pins — root table has no key

Do **not** add these to `[workspace.dependencies]`. Versions match packages already in the root lock, except the two research called out as absent:

| Dep | Spec | Already in root lock? |
| --- | --- | --- |
| `axum` | `0.6`, features `headers`, `ws` | Yes — `0.6.20`, same features as `collab` |
| `flate2` | `"1"` | Yes — `1.1.8` |
| `fs2` | `"0.4"` | Yes — `0.4.3` |
| `hmac` | `"0.12"` | Yes — `0.12.1` (was transitive only) |
| `mime_guess` | `"2"` | Yes — `2.0.5` |
| `notify` | `"9.0.0-rc.4"` | Yes — root `[patch.crates-io]` rewrites this to git `d842f16`. **Not** a workspace key (the git pin lives only in `[patch]`). `.workspace = true` fails to parse. |
| `tracing-subscriber` | `0.3`, features `env-filter` | Yes — `0.3.22`, `env-filter` already on |
| **`rusqlite`** | `0.32`, features `bundled` | **New** — `0.32.1`. Reuses existing `libsqlite3-sys 0.30.1` (already `features = ["bundled"]` in the root table) |
| **`tar`** | `"0.4"` | **New** — `0.4.46` |

No new third-party **version** of an existing package. `rusqlite` / `tar` plus rusqlite's three unique transitives (`fallible-iterator 0.3.0`, `fallible-streaming-iterator 0.1.9`, `hashlink 0.9.1`) are the new names. `hashlink 0.8.4` and `0.10.0` were already in the lock; `0.9.1` is an extra version, not a replacement.

Also on the crate: `[lints] workspace = true`, `publish.workspace = true`, `[lib] doctest = false`. Repo convention for root members; the reference had `publish = false` and no lib.

---

## 3. Trait / protocol follow-up (our tree, not a byte copy)

The server is JSON-RPC. It never names `fs::Fs` or `git::GitRepository`. The mismatches against `docs/phase5-wasm-remote.md` are:

| Site | Reference behaviour | What we did | Why |
| --- | --- | --- | --- |
| `GitRepository::load_commit` | Always `git show` the files; ignores extra JSON fields | Honour `ignore_shallow_boundary` (default **`true`** when omitted). When `false`, if `rev-parse --git-common-dir` + `.git/shallow` contains `{commit}^{commit}`, return `[]`. | Matches native `:1457–1463` without changing the response type. `wasm_remote` still deserializes `Vec<CommitFileResponse>` and hard-codes `is_shallow_boundary: false`. Growing the body to `{ files, is_shallow_boundary }` would break that client; `web/crates/` is out of scope. Missing-field default `true` keeps old callers on the previous “always show files” behaviour. |
| `Fs::read_dir_with_types` | Server still handles it | **Left in place** | Trait member is gone; `wasm_remote` no longer calls it. Harmless extra RPC. |
| `GitRepository::commit_data` | Server still handles it | **Left in place** | `wasm_remote` cannot construct `CommitDataReader` (no public constructor). Same follow-up as phase 5h. |
| `Fs::path_exists` / `is_path_case_sensitive` / `requires_poll_watcher` | Not on the wire | Nothing | Sync; answered in `wasm_remote`. |
| `notify` / `portable-pty` / `tempfile::keep` | Used as on the ref | Unchanged | APIs exist on our notify git `d842f16`, pty 0.9.0, tempfile 3.x. |
| `rand::rng()` | Already rand 0.9 | Unchanged | Matches root `rand = "0.9"`. |

`Fs::is_case_sensitive` on the server still returns `false` (the reference). That is a protocol default, not a trait impl; not changed.

---

## 4. `Cargo.lock`

### 4.1 First resolver pass tried to rewrite three **existing** packages

The first `cargo check -p zed_web_server --lib --bins` (no `--locked`) also retargeted Windows-only dependency **versions** inside three packages that were already in the lock:

| Package (version/source unchanged) | Before | After that first check |
| --- | --- | --- |
| `generator 0.8.9` | `windows-link 0.1.3`, `windows-result 0.3.4` | `windows-link 0.2.1`, `windows-result 0.4.1` |
| `iana-time-zone 0.1.64` | `windows-core 0.57.0` | `windows-core 0.62.2` |
| `tracy-client-sys 0.27.0` | `windows-targets 0.48.5` | `windows-targets 0.52.6` |

That is the “既有項目被改動” stop condition. Those three hunks were **reverted by hand**. They are not required: `cargo check -p zed_web_server --lib --bins --locked` then succeeded in 0.53s. The lock as left on disk is additive only. No existing package's version, source, checksum, or dependency list changed.

### 4.2 Final diff vs `/tmp/lock-before-p4c` [verified]

Unified diff of the working tree against the pre-change snapshot. The `hashlink 0.8.4` `dependencies = ["hashbrown 0.14.5"]` lines in the hunk are a **unified-diff artefact**: that block was already there; `hashlink 0.9.1` was inserted between `0.8.4` and `0.10.0`. Parsed package identity: 6 added, 0 removed, 0 existing bodies changed.

```
--- /tmp/lock-before-p4c	2026-09-15 19:54:44
+++ Cargo.lock	2026-09-15 20:02:57
@@ -6318,6 +6318,18 @@
 version = "0.2.0"
 source = "registry+https://github.com/rust-lang/crates.io-index"
 checksum = "c942e64b20ecd39933d5ff938ca4fdb6ef0d298cc3855b231179a5ef0b24948d"
+
+[[package]]
+name = "fallible-iterator"
+version = "0.3.0"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "2acce4a10f12dc2fb14a218589d4f1f62ef011b2d0cc4b3cb1bba8e94da14649"
+
+[[package]]
+name = "fallible-streaming-iterator"
+version = "0.1.9"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "7360491ce676a36bf9bb3c56c1aa791658183a54d2744120f27285738d90465a"
 
 [[package]]
 name = "fancy-regex"
@@ -8078,6 +8090,15 @@
 version = "0.8.4"
 source = "registry+https://github.com/rust-lang/crates.io-index"
 checksum = "e8094feaf31ff591f651a2664fb9cfd92bba7a60ce3197265e9482ebe753c8f7"
+dependencies = [
+ "hashbrown 0.14.5",
+]
+
+[[package]]
+name = "hashlink"
+version = "0.9.1"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "6ba4ff7128dee98c7dc9794b6a411377e1404dba1c97deb8d1a55297bd25d8af"
 dependencies = [
  "hashbrown 0.14.5",
 ]
@@ -15652,6 +15673,20 @@
 ]
 
 [[package]]
+name = "rusqlite"
+version = "0.32.1"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "7753b721174eb8ff87a9a0e799e2d7bc3749323e773db92e0984debb00019d6e"
+dependencies = [
+ "bitflags 2.13.1",
+ "fallible-iterator",
+ "fallible-streaming-iterator",
+ "hashlink 0.9.1",
+ "libsqlite3-sys",
+ "smallvec",
+]
+
+[[package]]
 name = "rust-embed"
 version = "8.11.0"
 source = "registry+https://github.com/rust-lang/crates.io-index"
@@ -18189,6 +18224,17 @@
 version = "1.0.1"
 source = "registry+https://github.com/rust-lang/crates.io-index"
 checksum = "55937e1799185b12863d447f42597ed69d9928686b8d88a1df17376a097d8369"
+
+[[package]]
+name = "tar"
+version = "0.4.46"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "3f6221d9a6003c78398e3b239969f352578258df48c8eb051caadae0015bc840"
+dependencies = [
+ "filetime",
+ "libc",
+ "xattr",
+]
 
 [[package]]
 name = "target-lexicon"
@@ -23261,6 +23307,40 @@
 ]
 
 [[package]]
+name = "zed_web_server"
+version = "0.1.0"
+dependencies = [
+ "anyhow",
+ "axum",
+ "base64 0.22.1",
+ "clap",
+ "flate2",
+ "fs2",
+ "futures 0.3.32",
+ "hex",
+ "hmac",
+ "libc",
+ "mime_guess",
+ "notify 9.0.0-rc.4",
+ "portable-pty",
+ "rand 0.9.4",
+ "rusqlite",
+ "serde",
+ "serde_json",
+ "sha2",
+ "tar",
+ "tempfile",
+ "tokio",
+ "toml 0.8.23",
+ "tracing",
+ "tracing-subscriber",
+ "url",
+ "urlencoding",
+ "walkdir",
+ "zed-reqwest",
+]
+
+[[package]]
 name = "zeno"
 version = "0.3.3"
 source = "registry+https://github.com/rust-lang/crates.io-index"
```

### 4.3 Nine §9 sources — byte-identical [verified]

```
agent-client-protocol 2.0.0 registry+https://github.com/rust-lang/crates.io-index
alacritty_terminal 0.26.1-dev git+https://github.com/zed-industries/alacritty?rev=4c129667ce56611becdc82de6e28218c80e2e88f#4c129667ce56611becdc82de6e28218c80e2e88f
async-tar 0.6.1 git+https://github.com/zed-industries/async-tar?rev=bd3ad6f89df9a9da7a8535958756d6bf465936a0#bd3ad6f89df9a9da7a8535958756d6bf465936a0
lsp-types 0.95.1 git+https://github.com/zed-industries/lsp-types?rev=f1783e63a7f4eb4397bf51d4148b4895a1f7ab16#f1783e63a7f4eb4397bf51d4148b4895a1f7ab16
smol 2.0.2 registry+https://github.com/rust-lang/crates.io-index
tree-sitter 0.27.0 git+https://github.com/tree-sitter/tree-sitter?rev=43623ec9bf0eaaf7113285c46e8a09018f181b18#43623ec9bf0eaaf7113285c46e8a09018f181b18
url 2.5.7 registry+https://github.com/rust-lang/crates.io-index
wasm_thread 0.3.3 git+https://github.com/zed-industries/wasm_thread?rev=0cf96c7708dfb97ccf3da50347e25edcf75d6937#0cf96c7708dfb97ccf3da50347e25edcf75d6937
which 8.0.5 registry+https://github.com/rust-lang/crates.io-index
```

`notify` on the server resolves to that same git `d842f16` (the desktop notify), not a wasm fork. `url` is crates.io 2.5.7, not `url_wasm`. No `manifest_path` under `web/` in the root workspace metadata.

---

## 5. Acceptance [verified]

### 5.1 Native compile

```
$ CARGO_TARGET_DIR=target/web-probe cargo check -p zed_web_server --lib --bins
    Updating crates.io index
     Locking 5 packages to latest compatible versions
      Adding fallible-iterator v0.3.0
      Adding fallible-streaming-iterator v0.1.9
      Adding hashlink v0.9.1
      Adding rusqlite v0.32.1 (available: v0.40.2)
      Adding tar v0.4.46
 Downloading crates ...
  Downloaded fallible-streaming-iterator v0.1.9
  Downloaded hashlink v0.9.1
  Downloaded rusqlite v0.32.1
    Checking smallvec v1.15.1
    …
    Checking zed-reqwest v0.12.15-zed (https://github.com/zed-industries/reqwest.git?rev=33bc764aa15ff7b200bf7c93bd96e24878d53e14#33bc764a)
    Checking zed_web_server v0.1.0 (/Users/andy/go/src/github.com/poi5305/zed/crates/zed_web_server)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 13.16s
```

Exit 0. After reverting the three Windows retargets:

```
$ CARGO_TARGET_DIR=target/web-probe cargo check -p zed_web_server --lib --bins --locked
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.53s
```

Exit 0. One cargo at a time. `target/web-probe` only.

### 5.2 Isolation gate

Only allowed edit: `web/check-workspace-isolation.sh` line 117, `"257"` → `"258"`. That assertion passed.

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
ok   V1 the seven vendored packages keep their base name and version
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
ok   L1 no shared package resolves in web to a version the root lock lacks (§9.1)
ok   V10 the web wasm32 graph is free of async-io/async-process/polling/errno/rustix (§4.1's wall)
FAIL V11 cd web && cargo check --workspace --all-targets succeeds
       expected: exit 0
       actual:   exit 101
       error[E0432]: unresolved import `livekit_protocol::enum_dispatch` …
FAIL V11 cd web && cargo check --workspace --target wasm32-unknown-unknown succeeds
       expected: exit 0
       actual:   exit 101
       error[E0433]: cannot find module or crate `heed` …
       error[E0432]: unresolved import `heed`
       error: failed to run custom build command for `tree-sitter-json v0.24.8`
FAIL M1 every dependency Phase 3a added to a crate manifest is used by that crate …
       actual:   git_bd3ad6f:redox_syscall,tree_sitter_wasm:…,zed_web_workspace:…
FAIL M2 every dependency Phase 3a wrote that the workspace already pins is inherited with workspace = true
       actual:   agent_client_protocol_patch:…,zed_web_workspace:…
ok   M3 every wasm clause Phase 3a wrote into a cfg gate changes the gate on at least one target
ok   S1 every .rs file naming web_time is rustfmt-clean (cargo fmt --all -- --check)

33 checks, 4 failures
```

The four failures are **not** `zed_web_server`. V11 is the web workspace (`livekit_protocol`, `heed`, `tree-sitter-json` WASI) — the Phase 5 wall, and `web/crates/` was forbidden this round. M1/M2 diff `*Cargo.toml` against `ee080f343354ad3a367e35bbd95132dea535c806` and flag vendor / `zed_web_workspace` / `wasm_rpc` literals; they do not name `zed_web_server`. Those assertions were not to be edited.

**Spec tension, not a 4c regression:** this phase's gate list includes a green `./web/check-workspace-isolation.sh`, but V11 cannot go green until Phase 5's wasm graph compiles, and M1/M2 judge the whole branch's Cargo.toml drift vs Phase 3a, not this crate. Stopped there rather than touching `web/crates/` or those assertions.

§9.1 / §9.2 (member count 258, no `web/` manifest in the root graph, nine sources) are green.

### 5.3 Refusals

```
$ ./web/check-refusals.sh
ok   §5.3.1 RELEASE_CHANNEL is dev
ok   §5.3.2 terminal Shift+Click selection extension exists
ok   §5.3.3 recent_projects open_local_project PathPromptOptions.files is true
ok   §5.3.4 remote_server MultiWrite::flush uses send_blocking

4 checks, 0 failures
```

### 5.4 rustfmt

```
$ rustfmt --edition 2024 --check crates/zed_web_server/src/*.rs
```

Exit 0. Did not format the five pre-existing dirty files (`claude_sessions_panel.rs`, `session_store.rs`, `remote/claude_sessions.rs`, `remote_server/server.rs`, `tmux_sessions_panel.rs`).

---

## 6. Follow-ups (not this round)

- Coordinate `load_commit` response `{ files, is_shallow_boundary }` with `wasm_remote` so the shallow-history banner can light up. Blocked on editing `web/crates/`.
- Public `CommitDataReader` constructor in `crates/git` so `GitRepository::commit_data` can be wrapped on the client.
- Isolation V11 / M1 / M2: web-workspace compile and Phase 3a-base Cargo.toml hygiene. Outside this crate.
- `extension_runtime_cli` still absent; server starts without `ZED_EXTENSION_RUNTIME` (research §1.5).
