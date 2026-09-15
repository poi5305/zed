# Phase 5 — wasm `worktree`: `block_on` + `send_blocking`

Date: 2026-09-15. Branch `andy/web-version`. **No commit.** `web/` source was not edited. `README.md` `> [!IMPORTANT]` lines left in place. Only `crates/worktree/src/worktree.rs` was changed under `crates/`.

Specs: `docs/web-zed-plan.md` Phase 5「Rulings on behaviour that could not be preserved」; precedents in `crates/settings/src/settings_store.rs` (skip `block_on`, same stream delivers later) and `crates/db/src/db.rs` (honest `panic!` naming the alternative). cfg spelling: `target_family = "wasm"` only.

Baseline lock: `cp Cargo.lock /tmp/lock-before-wt` before any edit. Lock unchanged (`cmp` identical, 503887 bytes).

## 0. codegraph (required first call)

```
codegraph explore "insert_entry UpdateIgnoreStatusJob worktree snapshot"
```

Returned 66 symbols across 7 files.

| Symbol | File:line | Callers |
| --- | --- | --- |
| `LocalSnapshot::insert_entry` | `crates/worktree/src/worktree.rs:3012` (pre-edit) | 1 in-file: `Worktree::local` via `block_on`; also `BackgroundScannerState::insert_entry` |
| `BackgroundScannerState::insert_entry` | `:3328` | scanner path; awaits `snapshot.insert_entry` |
| `RemoteWorktree::insert_entry` | `:2357` | proto path; not this round |
| `Snapshot::insert_entry` | `:2636` | remote snapshot |
| `UpdateIgnoreStatusJob` | `:6538` | seeded at `:5871` (`send_blocking`); recursive enqueue at `:6094` (already `.send().await`) |

Blast radius of `insert_entry` stays inside `worktree.rs` (integration tests hit it via `Worktree::local`). The two wasm errors were exactly those two call sites.

## 1. The two errors (reproduced, then gone)

The prompt’s two `E0599`s were the only `worktree` failures. After the edit they are gone.

Focused check (nightly, `-Z build-std=std,panic_abort`, cwd `web/` so `web/.cargo/config.toml` rustflags apply):

```
cd web && rustup run nightly cargo check -p worktree --target wasm32-unknown-unknown --lib -Z build-std=std,panic_abort
```

```
    Checking worktree v0.1.0 (/Users/andy/go/src/github.com/poi5305/zed/crates/worktree)
warning: `worktree` (lib) generated 1 warning (1 duplicate)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 48.92s
```

No `error[` in `crates/worktree`. The duplicate warning is the workspace-wide inferred-readme lint, not this file.

## 2. Site 1 — `ForegroundExecutor::block_on(snapshot.insert_entry(…))`

**Where.** `Worktree::local` (`async fn`, `cx: &mut AsyncApp`) builds the snapshot inside `cx.new(|cx| { … })`, which is a **synchronous** `FnOnce`. Native then:

```
cx.foreground_executor()
    .block_on(snapshot.insert_entry(entry, fs.as_ref()));
```

before `start_background_scanner`.

**Precedent 2 was tried first, and cannot be used in the settings shape.** Settings skips the blocking `rx.next()` and lets the already-spawned watcher apply the same first content one frame later. The analogous skip here — omit the constructor insert and let the scanner’s `insert_entry` deliver the root — **drops the scan**, not just delays it:

- `BackgroundScanner::run` only enqueues the root when `state.snapshot.root_entry()` is `Some` (`:4463`). An empty snapshot never scans.
- `is_single_file` is computed at scanner start as `snapshot.snapshot.root_dir().is_none()` (`:1375`). Without a root entry, a directory worktree is classified as a single-file tree.

So skipping the insert is not “content delayed”; it is a different worktree. That would be a new design. Stopped.

**Cannot `.await` either.** The call sits inside `cx.new`’s sync closure. `Worktree::local` being async does not help; gpui’s `App::new` callback is not async. Hoisting snapshot construction out of `cx.new` just to `.await` would duplicate the constructor (not 最小差异) and, for this call, would not even yield: the future is already `Ready` (see below).

**What we did (precedent 2’s data path + precedent 1 as a safety net).** Native `block_on` is unchanged behind `#[cfg(not(target_family = "wasm"))]`. On wasm we drive the **same** `insert_entry` future with `now_or_never()` (`FutureExt` was already imported). If it is `Pending` we `panic!` naming that we cannot block the wasm main thread — honest failure, same class as `AppDatabase::new`.

**Proof the panic is unreachable at this site.** `LocalSnapshot::insert_entry` has a single `.await`: loading a `.gitignore` when `entry.is_file() && entry.path.file_name() == Some(&GITIGNORE)`. The constructor always inserts `RelPath::empty_arc()` (worktree root, including single-file trees — the on-disk name lives on `abs_path`, not `entry.path`). `RelPath::file_name` is `self.components().next_back()`; the empty path has no components, so `file_name()` is `None`. The gitignore branch is not taken. The rest of `insert_entry` is synchronous tree edits. First poll is `Ready`. `now_or_never` returns `Some`; the root is in the snapshot **before** `start_background_scanner`, same order as native.

Content is not lost. Timing on wasm matches native (insert completes before `local()` returns). The only difference is we do not park the thread.

## 3. Site 2 — `ignore_queue_tx.send_blocking(UpdateIgnoreStatusJob { … })`

**Where.** Already inside `async fn update_ignore_statuses_for_paths`. The recursive worker path at `:6094` already uses `.send().await`. Only the seed loop used `send_blocking`.

**Not `try_send`.** `try_send` on a full channel drops the message. That is the §5.3.4 `remote_server` `MultiWrite::flush` refusal (lossy log). `UpdateIgnoreStatusJob` dropping would leave `.gitignore` status wrong. Forbidden.

**What we did (precedent 2, the async path).** Native `send_blocking(…).unwrap()` is unchanged behind `not(wasm)`. Wasm uses the same `UpdateIgnoreStatusJob` values on `.send().await.unwrap()` — the method the workers already use.

**Proof nothing is dropped.**

1. The channel is `async_channel::unbounded()` (`:5883`). Unbounded send never waits for capacity and never fails with “full”. There is no full-channel case for `try_send` to even consider.
2. `ignore_queue_rx` is still in scope when we send. `send().await` returns `Err` only if every receiver has been dropped. That does not happen here.
3. Failure is `.unwrap()`, same as native `send_blocking(…).unwrap()` — panic, not a skipped job.
4. After the seed loop, `drop(ignore_queue_tx)` still runs. Jobs in the queue hold `ignore_queue: Sender` clones, so the channel stays open for the recursive `.send().await` at `:6094` until those jobs finish. Closing behaviour is unchanged.
5. Seed-then-spawn order is unchanged: workers start after the loop (and after dropping the local sender).

## 4. Native unchanged

`git diff -- crates/worktree/src/worktree.rs` is +29 lines, both `#[cfg(target_family = "wasm")]` blocks plus the two `not(wasm)` attributes. The native statements are the pre-edit calls:

```
cx.foreground_executor()
    .block_on(snapshot.insert_entry(entry, fs.as_ref()));

ignore_queue_tx
    .send_blocking(UpdateIgnoreStatusJob { … })
    .unwrap();
```

Native `cargo check` (root workspace, stable toolchain):

```
CARGO_TARGET_DIR=target/web-probe cargo check -p worktree -p project --lib
```

```
    Checking worktree v0.1.0 (/Users/andy/go/src/github.com/poi5305/zed/crates/worktree)
    Checking project v0.1.0 (/Users/andy/go/src/github.com/poi5305/zed/crates/project)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 24.27s
```

## 5. `./web/build.sh` — worktree gone; next wall is `project` + `prompt_store`

Native server half: `Finished release … in 0.98s` (cached).

Wasm half compiled `worktree` (`Compiling worktree v0.1.0`) with **no** `error[` under `crates/worktree` and **no** `send_blocking` errors anywhere. Exit 101. Two crates failed:

```
error: could not compile `prompt_store` (lib) due to 9 previous errors
error: could not compile `project` (lib) due to 25 previous errors
```

Out of scope (not `crates/worktree`). Recorded so the next round does not have to rediscover them:

| Crate | `error[` / `error:` | What |
| --- | ---: | --- |
| `project` | 25 | `http_client::github` / `github_download` cfg’d out (`agent_server_store.rs`); `AvailableGrammar: Send` on `LocalLspAdapterDelegate` async trait methods (`lsp_store.rs`, the Phase 5 `Language` BLOCKER leaking into `project`); `ForegroundExecutor::block_on` in `project_settings.rs:1482` and `:1522` (same class as this round, now in `project`); `str` unsized from the skipped `block_on` leaving a `Stream<Item = str>`; `terminals.rs` type mismatch; `Telemetry::report_discovered_project_type_events` missing on wasm |
| `prompt_store` | 9 | `heed` not in the wasm graph (`prompt_store.rs`) |

The leftover `block_on` is **`crates/project/src/project_settings.rs`**, not worktree. Same ruling will apply there; this round was not allowed to write it.

## 6. Other gates

```
./web/check-refusals.sh
ok   §5.3.1 RELEASE_CHANNEL is dev
ok   §5.3.2 terminal Shift+Click selection extension exists
ok   §5.3.3 recent_projects open_local_project PathPromptOptions.files is true
ok   §5.3.4 remote_server MultiWrite::flush uses send_blocking
4 checks, 0 failures
```

`rustfmt --edition 2024 --check crates/worktree/src/worktree.rs` → `rustfmt_clean`.

Root `Cargo.lock` vs `/tmp/lock-before-wt`: identical, 503887 bytes. `git diff --stat -- Cargo.lock` vs `HEAD` is the pre-existing dirty lock from this branch, not this round.

## 7. Files

- `crates/worktree/src/worktree.rs` — two `target_family = "wasm"` branches
- `docs/phase5-worktree.md` — this report

Not edited: `web/**` (source), `Cargo.lock`, root `Cargo.toml`, any crate other than `worktree`.
