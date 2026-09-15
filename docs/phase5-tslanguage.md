# Phase 5 — `tree_sitter::Language` is not `Send`/`Sync` on wasm

Date: 2026-09-15. Branch `andy/web-version`. **No commit.** `web/` was not edited. `README.md` `> [!IMPORTANT]` lines left in place. The five pre-dirty rustfmt files (`claude_sessions_panel.rs`, `session_store.rs`, `remote/claude_sessions.rs`, `remote_server/server.rs`, `tmux_sessions_panel.rs`) were not touched.

Spec: `docs/web-zed-plan.md` Phase 5 **BLOCKER — `tree_sitter::Language` is not `Send`/`Sync` on wasm**. The plan said a `cfg` cannot fix this and named three vendor-level exits. This round implemented the later ruling: **on wasm, run the work on the foreground executor so `Language` never crosses threads.** Native `background_spawn` is unchanged. **No `unsafe impl Send` / `unsafe impl Sync`.** cfg spelling: `target_family = "wasm"` only.

Wrote only under `crates/` (plus this report). Root `Cargo.lock` was copied to `/tmp/lock-before-ts` before edits; it is unchanged.

## 0. codegraph (required first call)

```
codegraph explore "AvailableGrammar LanguageRegistry background_spawn reparse buffer"
```

Returned 133 symbols across 11 files. Blast radius of `reparse` is small (`lsp_store` + `buffer`); blast radius of `Buffer` is huge (885 callers). The errors are not at `Buffer` itself — they are at spawn sites that capture `Arc<LanguageRegistry>`, which contains `AvailableGrammar` / `tree_sitter::Language`.

Second call (`AvailableGrammar LanguageRegistry ts_language Grammar language_registry spawn`) showed:

- `AvailableGrammar` (`language_registry.rs:72`) holds `tree_sitter::Language` in `Native` / `Loaded` / `Loading`.
- `LanguageRegistry` stores `executor: BackgroundExecutor` and is constructed as `LanguageRegistry::new(cx.background_executor().clone())` — including `web/crates/zed_web_workspace/src/main.rs:1238`, which this round must not touch. **`new` signature stayed `(BackgroundExecutor) -> Self` on every target.**
- `language_core::Grammar.ts_language` is already `#[cfg(not(target_family = "wasm"))]`; wasm stores `parseable_language: ParseableLanguage` and exposes `parseable_language()`.

`BackgroundExecutor::spawn` requires `Future + Send`. `ForegroundExecutor::spawn` requires `Future + 'static` only. That is the lever.

`gpui_web::WebDispatcher::dispatch_on_main_thread`, when already on the main thread, uses `setTimeout(0)` (default priority) or `queueMicrotask` (realtime). It does **not** poll the new future before returning, so a spawn taken while `LanguageRegistryState`'s `RwLock` is held does not re-enter that lock on the same stack.

## 1. Baseline (this machine, re-run)

```
cd web && CARGO_TARGET_DIR=../target/web-probe cargo check --workspace --target wasm32-unknown-unknown
```

`language` (lib): **12 errors**. Plus `tree-sitter-json` WASI (`stdlib.h`), left alone.

```
error[E0277]: `*const TSLanguage` cannot be sent between threads safely
   --> crates/language/src/language_registry.rs:852
error[E0277]: `*const TSLanguage` cannot be shared between threads safely
   --> crates/language/src/language_registry.rs:852
error[E0609]: no field `ts_language` on type `&language_core::Grammar`
   --> crates/language/src/syntax_map.rs:1571
error[E0609]: no field `ts_language` on type `&language_core::Grammar`
   --> crates/language/src/language.rs:1423
error[E0277]: `*const TSLanguage` cannot be sent between threads safely
   --> crates/language/src/buffer.rs:1374
error[E0277]: `*const TSLanguage` cannot be shared between threads safely
   --> crates/language/src/buffer.rs:1374
error[E0277]: `*const TSLanguage` cannot be sent between threads safely
   --> crates/language/src/buffer.rs:1956
error[E0277]: `*const TSLanguage` cannot be shared between threads safely
   --> crates/language/src/buffer.rs:1956
error[E0277]: `*const TSLanguage` cannot be sent between threads safely
   --> crates/language/src/buffer.rs:3542
error[E0277]: `*const TSLanguage` cannot be shared between threads safely
   --> crates/language/src/buffer.rs:3542
error[E0277]: `*const TSLanguage` cannot be sent between threads safely
   --> crates/language/src/language_registry.rs:705
error[E0277]: `*const TSLanguage` cannot be shared between threads safely
   --> crates/language/src/language_registry.rs:705

error: could not compile `language` (lib) due to 12 previous errors
error: failed to run custom build command for `tree-sitter-json v0.24.8`
```

The prompt listed three spawn sites. The compiler listed **five** (the extra two are the same `AvailableGrammar` capture: `load_language` at `:705` and `snapshot_with_edits` at `:3542`). All twelve are one family: `BackgroundExecutor::spawn` / `AppContext::background_spawn` require `Send`, and the closure holds `Arc<LanguageRegistry>` → `AvailableGrammar` → `tree_sitter::Language(*const TSLanguage)`.

## 2. Why this is the ruling, not `unsafe impl`

Our tree-sitter base `43623ec` gates `unsafe impl Send/Sync for Language` (upstream **#5851**, threading-model soundness). The zed-web snapshot `7f534862` still has those impls ungated. Adding them back would undo that fix for the model the web build ships (wasm atomics + workers), and nothing would go red.

The cost of the ruling is that the browser parses on the main thread. That is a **performance trade-off, not a correctness trade-off**. §2.3 already says `gpui_web` falls back to a single-threaded dispatcher when `SharedArrayBuffer` / `waitAsync` are missing, so main-thread parse is not a new class of restriction.

`AvailableGrammar` was **not** given a wasm-only shape. Native and wasm still share the same enum.

## 3. How each site was changed

Pattern at every spawn: bind the existing `async move { ... }` to a local future, then cfg the **spawn call**. Native is still `background_spawn` / `BackgroundExecutor::spawn` of that same future. Wasm uses `ForegroundExecutor::spawn`, which does not require `Send`.

`cx.spawn` was not used for the parse futures. `Context::spawn` takes `(WeakEntity<T>, &mut AsyncApp)`; `App::spawn` takes `AsyncFnOnce(&mut AsyncApp)`. The existing closures are plain `Future`s, the same shape `background_spawn` already took. `cx.foreground_executor().spawn(future)` is the equivalent that keeps that shape.

`LanguageRegistry` has no `cx`. Wasm constructs a `ForegroundExecutor` from `self.executor.dispatcher()` (same `PlatformDispatcher` the app already uses) so `LanguageRegistry::new` stays `new(BackgroundExecutor)` and `web/` does not need a signature change.

### `language_registry.rs:705` — `load_language`

Native: `self.executor.spawn(future).detach();`
Wasm: `ForegroundExecutor::new(self.executor.dispatcher().clone()).spawn(future).detach();`

The future body is the pre-existing load / `get_or_load_grammar` / oneshot fan-out. It still writes `state` after the spawn returns, so the `RwLock` is dropped before the queued main-thread poll (`setTimeout(0)`). Nested `load_language` / `get_or_load_grammar` awaits yield on oneshots; they do not `block_on` (`block_on` is already `not(wasm)`).

### `language_registry.rs:852` — `get_or_load_grammar`

Same spawn split. Future body still `std::fs::read` + `with_parser` + `AvailableGrammar::Loaded`. `with_parser` on wasm is already `thread_local!` (Phase 5g), so `Parser` stays on the creating thread — which is now the main thread.

### `buffer.rs:1374` — `preview_edits`

Native: `cx.background_spawn(future)`
Wasm: `cx.foreground_executor().spawn(future)`

Return type remains `Task<EditPreview>`. Callers do not require that task to be `Send` at these sites (workspace check did not demand it).

### `buffer.rs:1956` — `reparse`

Native: `let parse_task = cx.background_spawn(parse_future);`
Wasm: `let parse_task = cx.foreground_executor().spawn(parse_future);`

The waiter is still `cx.spawn(async move |this, cx| { parse_task.await; ... })` on both targets. On wasm both the parse and the waiter are foreground tasks; the waiter `.await`s and yields so the parse can run. This is cooperative, not a deadlock.

### `buffer.rs:3542` — `snapshot_with_edits`

Same split as `preview_edits`. Same capture (`Option<Arc<LanguageRegistry>>`).

### E0609 — `grammar.ts_language`

Native keeps the field access. Wasm uses the API `language_core` already exposes:

```
// language.rs:1422 parse_text
#[cfg(not(target_family = "wasm"))]
parser.set_language(&grammar.ts_language).expect("incompatible grammar");
#[cfg(target_family = "wasm")]
parser.set_language(&grammar.parseable_language().expect("incompatible grammar"))
    .expect("incompatible grammar");

// syntax_map.rs:1571
#[cfg(not(target_family = "wasm"))]
parser.set_language(&grammar.ts_language)?;
#[cfg(target_family = "wasm")]
parser.set_language(&grammar.parseable_language()?)?;
```

### Downstream sites that only appeared after `language` compiled

The first workspace check stopped at `language`. Once that crate type-checked, two more `background_spawn` sites capturing `Arc<LanguageRegistry>` failed with the same `*const TSLanguage` notes. Same ruling, same cfg-split, not a new design:

| Site | Native | Wasm |
| --- | --- | --- |
| `language_detection.rs:detect_language` | `cx.background_spawn(future)` | `cx.foreground_executor().spawn(future)` |
| `markdown.rs:start_background_parse` | `cx.background_spawn(parse_future)` | `cx.foreground_executor().spawn(parse_future)` |

`markdown` still waits on that task with `cx.spawn`; only the parse half moves to the foreground on wasm.

## 4. Native behaviour is the original spawn

Every cfg answers "is the native spawn still here?":

| Site | Native line after the change |
| --- | --- |
| `load_language` | `self.executor.spawn(future).detach();` under `not(wasm)` |
| `get_or_load_grammar` | `self.executor.spawn(future).detach();` under `not(wasm)` |
| `preview_edits` | `cx.background_spawn(future)` under `not(wasm)` |
| `reparse` | `cx.background_spawn(parse_future)` under `not(wasm)` |
| `snapshot_with_edits` | `cx.background_spawn(future)` under `not(wasm)` |
| `detect_language` | `cx.background_spawn(future)` under `not(wasm)` |
| `start_background_parse` | `cx.background_spawn(parse_future)` under `not(wasm)` |
| `parse_text` / `syntax_map::parse_text` | `grammar.ts_language` under `not(wasm)` |

`LanguageRegistry::new(BackgroundExecutor)` is unchanged. `AvailableGrammar` is unchanged. No function signature changed.

```
CARGO_TARGET_DIR=target/web-probe cargo check -p language -p editor -p project --lib
```

First attempt died with `No space left on device` on `/Volumes/XDATA` (the `target` symlink; 63Mi free). Deleted only `target/web-probe/debug/incremental` (18G rustc incremental cache). Retry:

```
    Checking editor v0.1.0 (.../crates/editor)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 5.97s
```

Exit 0. (`language` and `project` were already type-checked in the truncated run; `editor` was the crate that had failed on disk full.)

## 5. Wasm behaviour difference (honest)

On wasm, syntax parse, grammar wasm-file load, language-registry load, edit preview, snapshot-with-edits, content language detection, and markdown parse that needs the registry all run **on the browser main thread**.

Consequences:

- A large buffer reparse can jank input and painting for the duration of `syntax_snapshot.reparse`. Native still does that work on the background pool.
- Grammar load (`std::fs::read` of a `.wasm` grammar + `WasmStore::load_language`) also occupies the main thread. Native still does that in `BackgroundExecutor`.
- When `SharedArrayBuffer` is present, workers still exist for other `Send` work. These closures stay off those workers on purpose (#5851).
- When SAB is missing, the dispatcher is already single-threaded (§2.3); the visible difference vs native is then the same as the rest of the app.

This round does **not** make `Language::new(..., Some(ts_language))` succeed on wasm. That path still `panic!`s naming `ParseableLanguage::from_resolver` (Phase 5g). Plain text (`None`) does not panic. Loading a real grammar at runtime still has to go through whatever constructs `Grammar` on the parsing thread; that is separate from the Send/Sync compile errors.

## 6. After this round — remaining errors

**Zero** `*const TSLanguage` / `ts_language` errors. `language`, `language_detection`, and `markdown` compile for `wasm32-unknown-unknown`.

The prompt expected only `tree-sitter-json`'s WASI build. That is still there. Clearing `language` also uncovered crates that depend on it and were not type-checked while it failed. Those are **not** `TSLanguage` and were not changed:

```
error[E0433]: cannot find module or crate `heed`
  --> crates/prompt_store/src/prompt_store.rs:11
error[E0432]: unresolved import `heed`
  --> crates/prompt_store/src/prompt_store.rs:11
error[E0433]: cannot find module or crate `heed`
  --> crates/prompt_store/src/prompt_store.rs:222
error[E0433]: cannot find module or crate `heed`
  --> crates/prompt_store/src/prompt_store.rs:158
error[E0433]: cannot find module or crate `heed`
  --> crates/prompt_store/src/prompt_store.rs:248
error[E0433]: cannot find module or crate `heed`
  --> crates/prompt_store/src/prompt_store.rs:249
error[E0433]: cannot find module or crate `heed`
  --> crates/prompt_store/src/prompt_store.rs:250
error[E0433]: cannot find module or crate `heed`
  --> crates/prompt_store/src/prompt_store.rs:263
error[E0433]: cannot find module or crate `heed`
  --> crates/prompt_store/src/prompt_store.rs:275
error: could not compile `prompt_store` (lib) due to 9 previous errors

error[E0599]: no method named `block_on` found for reference `&ForegroundExecutor`
  --> crates/worktree/src/worktree.rs:593
error[E0599]: no method named `send_blocking` found for struct `async_channel::Sender<T>`
  --> crates/worktree/src/worktree.rs:5871
error: could not compile `worktree` (lib) due to 2 previous errors

error: failed to run custom build command for `tree-sitter-json v0.24.8`
  cargo:warning=src/tree_sitter/parser.h:10:10: fatal error: 'stdlib.h' file not found
```

`tree-sitter-json` still needs the WASI SDK (§4.2 / the plan). Do not touch it here. `prompt_store`/`heed` and `worktree` `block_on` / `send_blocking` are the next cfg layer, not this blocker.

## 7. Other acceptance

`./web/check-refusals.sh` — 4/4:

```
ok   §5.3.1 RELEASE_CHANNEL is dev
ok   §5.3.2 terminal Shift+Click selection extension exists
ok   §5.3.3 recent_projects open_local_project PathPromptOptions.files is true
ok   §5.3.4 remote_server MultiWrite::flush uses send_blocking

4 checks, 0 failures
```

rustfmt (`rustfmt.toml`, edition 2024) `--check` on the six edited sources: exit 0.

`cmp /tmp/lock-before-ts Cargo.lock`: identical (502232 bytes). Diff empty.

No `unsafe impl Send` / `unsafe impl Sync` for `tree_sitter::Language` (or anything else) was added.
