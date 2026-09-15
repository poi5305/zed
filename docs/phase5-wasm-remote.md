# Phase 5h — `wasm_remote` Fs / git trait follow-up

Date: 2026-09-15. Branch `andy/web-version`. **No commit.** Only `web/crates/wasm_remote/` was edited (plus this report). `README.md` `> [!IMPORTANT]` lines left in place. `crates/`, `web/vendor/`, `web/Cargo.toml`, `web/build.sh`, `web/check-*.sh` were not touched.

Specs: `docs/web-zed-plan.md` §2.2 (`wasm_remote` implements Zed's existing `Fs` and git traits over RPC), §9.

Baseline lock: `cp Cargo.lock /tmp/lock-before-p5h` before any edit. Lock unchanged (`diff` empty).

## 0. codegraph (required first call)

```
codegraph explore "Fs"
codegraph explore "GitCommitTemplate"
```

MCP `codegraph_explore` on `Fs trait path_exists is_path_case_sensitive requires_poll_watcher read_dir_with_types load_commit CommitDiff is_shallow_boundary GitCommitTemplate CommitDataReader from_async_resolver`, then a second call on `GitRepository trait load_commit GitCommitTemplate CommitDataReader FakeGitRepository FakeFs path_exists`.

| Symbol | File:line | Shape | Notes |
| --- | --- | --- | --- |
| `Fs::path_exists` | `crates/fs/src/fs.rs:174` | `fn path_exists(&self, path: &Path) -> bool` | Sync. Does not follow a final symlink. Callers: `fs_watcher` (treat missing as pending). RealFs: `symlink_metadata(path).is_ok()`. |
| `Fs::is_path_case_sensitive` | `crates/fs/src/fs.rs:176` | `fn is_path_case_sensitive(&self, path: &Path) -> bool` | Sync, per-volume. Callers: `fs_watcher` watch-key folding. RealFs: `!fs_watcher::case_insensitive_path(path)`. |
| `Fs::requires_poll_watcher` | `crates/fs/src/fs.rs:179` | `fn requires_poll_watcher(&self, path: &Path) -> bool` | Sync. `true` means native watches do not deliver events. Callers: `worktree` defers `fs.watch`; `fs_watcher` picks poll vs native. FakeFs: `false`. |
| `Fs::read_dir_with_types` | — | **removed from the trait** | Not a member of current `Fs`. zed-web leftover. |
| `GitRepository::load_commit` | `crates/git/src/repository.rs:921` | `fn load_commit(&self, commit: String, ignore_shallow_boundary: bool, cx: AsyncApp)` | 4th parameter (counting `self`) is `ignore_shallow_boundary`. When `false`, a shallow-boundary commit returns empty files + `is_shallow_boundary: true`. When `true`, load the snapshot anyway. |
| `CommitDiff::is_shallow_boundary` | `crates/git/src/repository.rs:531` | `pub is_shallow_boundary: bool` | `true` means this commit is the shallow-clone wall; UI shows the empty-history banner instead of a fake empty diff. |
| `GitCommitTemplate` | `crates/git/src/repository.rs:1344` | `{ pub template: String }` | `Clone + Debug` only. No `Deserialize`. Proto/RPC carry a string, then wrap. |
| `CommitDataReader` | `crates/git/src/repository.rs:140` | `{ request_tx, _task }` (private) | `from_async_resolver` is gone. Public API is `read`. `for_test` is `#[cfg(any(test, feature = "test-support"))]` and takes a **sync** `Fn(Oid) -> Result<CommitData>`. RealGitRepository constructs the struct in-module. |

`git_store::local_commit_data_reader` already treats `commit_data_reader()` `Err` as fatal for that reader (logs and returns). That is the honest failure path.

## 1. Baseline (this machine, re-run)

```
cd web && CARGO_TARGET_DIR=../target/web-probe cargo check --workspace --target wasm32-unknown-unknown
```

`wasm_remote`: **6** `error[` (plus `could not compile wasm_remote`). Out of scope and ignored: `language` TSLanguage family (12) and `tree-sitter-json` WASI (`stdlib.h`).

| Error | Site |
| --- | --- |
| E0407 `read_dir_with_types` is not a member of trait `Fs` | `web/crates/wasm_remote/src/fs.rs:600` |
| E0046 missing `path_exists`, `is_path_case_sensitive`, `requires_poll_watcher` | `web/crates/wasm_remote/src/fs.rs:298` |
| E0050 `load_commit` has 3 parameters, trait has 4 | `web/crates/wasm_remote/src/git.rs:720` |
| E0063 missing field `is_shallow_boundary` | `web/crates/wasm_remote/src/git.rs:733` |
| E0277 `GitCommitTemplate: Deserialize` | `web/crates/wasm_remote/src/git.rs:1386` |
| E0599 no `CommitDataReader::from_async_resolver` | `web/crates/wasm_remote/src/git.rs:1533` |

## 2. What changed (minimum, `wasm_remote` only)

### `Fs` (`web/crates/wasm_remote/src/fs.rs`)

| Method | Implementation | RPC? | Why this value is safe |
| --- | --- | --- | --- |
| `path_exists` | `true` only if the path is already in the prefetch caches (`prefetched_metadata` key, `prefetched_directories` key, or a cached listing entry). Otherwise `false`. | No. Trait is **sync**; WASM cannot block on `Fs::metadata` / `Fs::is_file` without deadlocking the same thread that drives the socket. There is no `Fs::path_exists` method on the wire. | `false` makes `fs_watcher` treat the path as pending. `true` would try to watch a path that may not exist. |
| `is_path_case_sensitive` | Always `true`. | No. No per-path sync RPC. Async `Fs::is_case_sensitive` already exists and is unchanged. | `true` matches typical Linux servers. `false` would fold `Foo`/`foo` into one watch key and merge distinct files on a case-sensitive volume. A missed watch on a case-insensitive volume is conservative; a false merge is not. |
| `requires_poll_watcher` | Always `false`. | No. | `RemoteFs::watch` already forwards server-side native events over `Fs::watch`. `true` would make the worktree defer that RPC watch and poll `path_exists` locally, which we cannot answer honestly. FakeFs also returns `false`. |
| `read_dir_with_types` | **Removed** (and `ReadDirWithTypesResponse` / `ReadDirEntryResponse`). | Was `Fs::read_dir_with_types`; trait member is gone. `Fs::read_dir` still covers listings. | — |

### git (`web/crates/wasm_remote/src/git.rs`)

| Method / field | Implementation | RPC? | Why this value is safe |
| --- | --- | --- | --- |
| `load_commit` | Added `ignore_shallow_boundary: bool`. Still calls `GitRepository::load_commit`. Forwards the new flag in the JSON. Still deserializes the existing `Vec<CommitFileResponse>` body. | Yes — existing method, extra request field. Response shape unchanged (old servers ignore unknown fields; we ignore a missing flag). | — |
| `CommitDiff.is_shallow_boundary` | Hard-coded `false`. | Not on the current response. | `true` would hide the files we just received behind the shallow-history banner. `false` shows the diffs the server returned. Native only sets `true` with empty files. |
| `load_commit_template` | Deserialize `Option<GitCommitTemplateResponse { template: String }>` then wrap `GitCommitTemplate { template }`. | Yes — `GitRepository::load_commit_template`. Local DTO because the git crate type has no `Deserialize`. | — |
| `commit_data_reader` | `Err(anyhow!("… not supported over the web RPC bridge: CommitDataReader has no public constructor"))`. | Wire method `GitRepository::commit_data` still exists, but cannot be wrapped: no public constructor, `from_async_resolver` removed, `for_test` is test-only and sync. Would need a public constructor on `CommitDataReader` in `crates/git` — out of scope; not done. | Returning `Err` is honest. `git_store` already logs and stops the local reader on this error. A fake `Ok(reader)` that never resolves would stall the git graph. |

## 3. Acceptance

### 3.1 wasm workspace check — `wasm_remote`'s 6 errors gone

Re-ran the same command. `Checking wasm_remote` then `warning: wasm_remote (lib) generated 1 warning (1 duplicate)` (the workspace-wide `atomics` target-feature warning). **No `error[` under `web/crates/wasm_remote/`.** `could not compile wasm_remote` is gone.

Remaining (untouched, as specified):

**`language` — 12 `error[` (TSLanguage family)**

```
error[E0277]: `*const TSLanguage` cannot be sent between threads safely
    --> crates/language/src/language_registry.rs:852:26
error[E0277]: `*const TSLanguage` cannot be shared between threads safely
    --> crates/language/src/language_registry.rs:852:26
error[E0609]: no field `ts_language` on type `&language_core::Grammar`
    --> crates/language/src/syntax_map.rs:1571:38
error[E0609]: no field `ts_language` on type `&language_core::Grammar`
    --> crates/language/src/language.rs:1423:36
error[E0277]: `*const TSLanguage` cannot be sent between threads safely
    --> crates/language/src/buffer.rs:1374:12
error[E0277]: `*const TSLanguage` cannot be shared between threads safely
    --> crates/language/src/buffer.rs:1374:12
error[E0277]: `*const TSLanguage` cannot be sent between threads safely
    --> crates/language/src/buffer.rs:1956:29
error[E0277]: `*const TSLanguage` cannot be shared between threads safely
    --> crates/language/src/buffer.rs:1956:29
error[E0277]: `*const TSLanguage` cannot be sent between threads safely
    --> crates/language/src/buffer.rs:3542:12
error[E0277]: `*const TSLanguage` cannot be shared between threads safely
    --> crates/language/src/buffer.rs:3542:12
error[E0277]: `*const TSLanguage` cannot be sent between threads safely
    --> crates/language/src/language_registry.rs:705:22
error[E0277]: `*const TSLanguage` cannot be shared between threads safely
    --> crates/language/src/language_registry.rs:705:22
error: could not compile `language` (lib) due to 12 previous errors
```

**`tree-sitter-json` — WASI / missing C stdlib (needs WASI SDK, not touched)**

```
error: failed to run custom build command for `tree-sitter-json v0.24.8`
cargo:warning=src/tree_sitter/parser.h:10:10: fatal error: 'stdlib.h' file not found
```

### 3.2 refusals

```
$ ./web/check-refusals.sh
ok   §5.3.1 RELEASE_CHANNEL is dev
ok   §5.3.2 terminal Shift+Click selection extension exists
ok   §5.3.3 recent_projects open_local_project PathPromptOptions.files is true
ok   §5.3.4 remote_server MultiWrite::flush uses send_blocking

4 checks, 0 failures
```

### 3.3 root `Cargo.lock`

```
$ diff -u /tmp/lock-before-p5h Cargo.lock
```

Empty. Identical.

### 3.4 rustfmt

```
rustfmt --edition 2024 --check web/crates/wasm_remote/src/fs.rs web/crates/wasm_remote/src/git.rs
```

Exit 0.

## 4. Follow-ups (not in this round)

- A public `CommitDataReader` constructor (async resolver) in `crates/git` would let `wasm_remote` wrap `GitRepository::commit_data` again. That is a trait/crate-API change; stopped here as required.
- `GitRepository::load_commit` response could grow `{ files, is_shallow_boundary }` when the server is updated. Until then `is_shallow_boundary: false` is the honest default.
- Sync `Fs::path_exists` / `is_path_case_sensitive` could cache the last async `metadata` / `is_case_sensitive` RPC if a later round adds a small cache. Not done: would be speculative protocol.
