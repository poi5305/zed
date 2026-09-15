# Phase 5d — wasm workspace compile blockers (`db` `pet-fs` `settings`)

Date: 2026-09-15. Branch `andy/web-version`. **No commit.** `web/` was not edited. `README.md` `> [!IMPORTANT]` lines left in place. The five pre-dirty rustfmt files (`claude_sessions_panel.rs`, `session_store.rs`, `remote/claude_sessions.rs`, `remote_server/server.rs`, `tmux_sessions_panel.rs`) were not touched.

Specs: `docs/web-zed-plan.md` §2.2, §4.1, §5.1, §9. cfg spelling: `target_family = "wasm"` only.
Reference: `git diff fecc3273ed32643c2ea1b04a74c8780e2c9ffaf8..zedweb/zed-web -- crates/db crates/gpui crates/settings crates/settings_json crates/languages`.

Baseline: `cp Cargo.lock /tmp/lock-before-p5d` before any edit. Lock unchanged (`cmp` identical, 502232 bytes).

This round wrote only under `crates/` (plus this report). Root `Cargo.toml` / `web/` were not retouched.

## 0. codegraph (required first call)

```
codegraph explore "block_on ForegroundExecutor migrator infer_json_indent_size update_value_in_json_text"
```

| Symbol | File:line | Notes |
| --- | --- | --- |
| `ForegroundExecutor::block_on` | `crates/gpui/src/executor.rs:447` | Already `#[cfg(not(target_family = "wasm"))]`. Calls `scheduler::LocalExecutor::block_on`. |
| `gpui::block_on` | `crates/gpui/src/gpui.rs:168` | `pub use pollster::block_on`, already `not(wasm)`. |
| `update_value_in_json_text` | `crates/settings_json/src/settings_json.rs:15` | `all(feature = "editing", not(wasm))` after Phase 3a. Callers: `settings_store`, `migrator`. |
| `infer_json_indent_size` | `crates/settings_json/src/settings_json.rs:633` | Same cfg. Callers: `settings_store`, `migrator`, `dev_container`, `keymap_editor`. |
| `append_top_level_array_value_in_json_text` / `replace_top_level_array_value_in_json_text` | `settings_json.rs:508` / `:385` | Same cfg. Callers: `settings/src/keymap_file.rs`. |

Manifests are not in the graph. `crates/settings/Cargo.toml` already has `migrator` under `not(wasm)` (Phase 3a). `crates/languages/Cargo.toml` still named `pet-fs` unconditionally (this round). `pet-fs` lives in the cargo git checkout of `python-environment-tools` `bb8e046`, not under `crates/`.

## 1. Baseline (this machine, re-run)

```
cd web && CARGO_TARGET_DIR=../target/web-probe cargo check --workspace --target wasm32-unknown-unknown
```

Exit 101. 9 `error[` in three crates (the prompt’s “12” counted the two E0432 rows as four missing names):

| Crate | Count | Errors |
| --- | ---: | --- |
| `db` | 2 | E0425 `gpui::block_on` — `db.rs:65` (`AppDatabase::new`), `kvp.rs:246` (`GLOBAL_KEY_VALUE_STORE`) |
| `settings` | 5 | E0599 `ForegroundExecutor::block_on` — `settings_store.rs:372`, `:376`; E0432 four `settings_json` names (`settings_store.rs:44`, `keymap_file.rs:24`); E0433 `migrator` — `settings_store.rs:805` |
| `pet-fs` (third-party) | 2 | E0308 `path.rs:54` `strip_trailing_separator`, `:133` `norm_case` — bodies are `#[cfg(unix)]` / `#[cfg(windows)]` only; wasm32-unknown-unknown is neither, so the functions return `()` |

## 2. What `zed-web` actually did

### (1) `block_on`

zed-web **kept** blocking APIs on wasm:

- `crates/gpui/src/gpui.rs`: ungated `pub use pollster::block_on;`
- `ForegroundExecutor::block_on`: ungated, still calls `inner.block_on`

Call sites in `db` / `settings` were **not** rewritten to async:

- `AppDatabase::new` still does `gpui::block_on(open_db(…))`
- `GLOBAL_KEY_VALUE_STORE` still does `gpui::block_on(open_db(…))`
- `SettingsStore::watch_settings_files` still does `foreground_executor().block_on(rx.next())` twice

What they *added* instead:

- `AppDatabase::open_in_memory` + `open_in_memory_db` using `locking_queue()`, so the future can complete without parking a background thread
- `prepare_web_database()` — async, talks to `sqlez::remote_sql` (server SQLite over RPC)
- `open_db` skips `create_dir_all` on wasm
- The **web entrypoint** never calls the blocking constructors. `web/crates/zed_web_workspace/src/main.rs` uses `now_or_never(AppDatabase::open_in_memory("zed-web"))` and `db::prepare_web_database()`, and loads settings with `load_user_settings` (spawn + `SettingsStore::load_settings`, no `watch_settings_files`)

`prepare_web_database` / `remote_sql` are **not** available here. Phase 5c refused wiring `crates/sqlez` to `web/crates/wasm_rpc`.

Restoring `gpui::block_on` / `ForegroundExecutor::block_on` on wasm would compile the call sites the way zed-web does. That is the thing this round was told not to do: the browser main thread cannot block.

### (2) `settings_json` wasm fallback — **not copied**

zed-web’s `settings_json.rs` adds +69 lines of wasm functions with the same signatures as native. They do **not** parse JSON structure. On some inputs they return a plausible-looking result that is wrong:

| Function | wasm body | Why it is silent-wrong |
| --- | --- | --- |
| `update_value_in_json_text` | replace **entire** `text` with `new_value.to_string()` | Drops comments, unrelated keys’ formatting, key order |
| `replace_value_in_json_text` | `(0..text.len(), new_value.to_string())` | Same whole-file smash |
| `replace_top_level_array_value_in_json_text` | same whole-file smash | A keymap is a top-level array; this would delete every other binding |
| `append_top_level_array_value_in_json_text` | `(text.len()..text.len(), format!(",{new_value}"))` | Appends `,value` **after** the closing `]`; invalid JSON, or a second value sitting outside the array |
| `infer_json_indent_size` | always `2` | Native walks the tree; a 4-space file would be rewritten at 2 |

That is exactly the “fallback that silently returns the wrong result” stop condition. Native tree-sitter bodies are unchanged. Wasm gets panic stubs with the same signatures (the functions do not return `Result`, so panic is the honest failure without a signature change).

### (3) `migrator`

zed-web wraps `migrator::migrate_settings` in `not(wasm)` and on wasm parses the file as-is (`MigrationStatus` stays `NotNeeded`). Same shape as Phase 3a’s manifest gate. Taken.

### `pet-fs` / `languages`

zed-web moved `pet*` behind a `python-support` **feature** (`default = ["native-adapters", "python-support"]`) and `#[cfg(feature = "python-support")] mod python`. They also put `python-support` **inside** `load-grammars`.

That feature trick does **not** drop `pet-fs` from *our* wasm graph: `markdown` and `edit_prediction` enable `languages` `load-grammars`, Cargo unifies features, and `python-support` would come back. Our workspace table already has `languages = { …, default-features = false }`, so making `python-support` a default feature would also strip Python from native `zed` unless every consumer opted back in — and those consumers are not all under the “only `crates/`” rule.

Taken instead: Phase 3a target-cfg. `pet*` moved to `[target.'cfg(not(target_family = "wasm"))'.dependencies]`. `mod python` and its `init` wiring are `not(wasm)`. Native still links `pet-fs` (verified). wasm never names the crate, so the third-party E0308 cannot fire.

**Not taken:** zed-web’s `native-adapters` gate (would drop C/C++/Go/Rust adapters on wasm). `path_exists` helper. `wasm_remote` settings write path. `last_user_settings_content` cache.

## 3. Per-crate how it was fixed

### `db` — `block_on`

Native `AppDatabase::new` / `GLOBAL_KEY_VALUE_STORE` / `open_fallback_db` bodies are the original statements, behind `not(wasm)`.

| Site | Native still there? | wasm |
| --- | --- | --- |
| `AppDatabase::new` | yes, still `gpui::block_on(open_db::<AppMigrator>(…))` | `panic!("… cannot block the wasm main thread; use AppDatabase::open_in_memory")` |
| `AppDatabase::open_in_memory` | **new** async API, both targets | `locking_queue()` so `build` does not spawn `std::thread`. With the Phase 5c sqlez stub, `migrate`/`exec` return `Err` and this **panics** with that error — it does not return a fake DB |
| `GLOBAL_KEY_VALUE_STORE` LazyLock | yes, still `gpui::block_on(open_db)` | static not compiled. `GlobalKeyValueStore::global()` panics (same reason) |
| `create_dir_all` in `open_db` | yes | skipped (no local fs). Matches zed-web |
| `open_fallback_db` | original `builder` + `expect` | `open_in_memory_db` |

`prepare_web_database` was **not** added.

**What blocking wait becomes on web (the important bit):**

1. **Opening the app DB.** Native `AppDatabase::new()` still blocks the thread until `open_db` finishes. The web binary does not call `new()`. It is expected to `await` / `now_or_never` `open_in_memory`. Until `remote_sql` exists, that future fails at migrate and panics — visible failure, not an empty in-memory sqlite pretending to be the server DB.
2. **`GlobalKeyValueStore::global()`.** Native still opens a second on-disk DB via `block_on` inside a `LazyLock`. wasm panics if anything calls it. `KeyValueStore::from_app_db` still exists for a store derived from an `AppDatabase` that was opened asynchronously. `prompt_store` currently calls `GlobalKeyValueStore::global()`; if that crate is reached on wasm it will panic, not silently skip KVP reads.
3. **`watch_settings_files`.** See `settings` below. The web entrypoint already avoids this function.

zed-web’s leftover `block_on` in `new()` / KVP / `watch_settings_files` still compiles for them only because they kept pollster. We did not put it back.

### `settings` — `block_on` + `migrator`

| Site | Native still there? | wasm |
| --- | --- | --- |
| `watch_settings_files` first two `rx.next()` | yes, still `foreground_executor().block_on(…).unwrap()` then `set_user_settings` / `set_global_settings` **before return** | those two waits are not compiled. The existing spawned watcher still runs; the **first** `fs.load` of each file is applied there, after `watch_settings_files` has already returned |
| `parse_and_migrate_zed_settings` | yes, `migrator::migrate_settings` | parse the text as-is; `migration_status` stays `NotNeeded` |

**`watch_settings_files` on web vs native.** Native: the function does not return until both config files have been read and pushed into the store. wasm: it returns immediately; the same two values arrive later on the same channels and go through the same `set_*_settings` + `settings_changed` path inside `cx.spawn`. Settings are not skipped. They are not applied synchronously. The web workspace does not call this function today (`load_user_settings` is already that async load). If something in the wasm graph *does* call `watch_settings_files`, the first paint can happen before user/global settings exist — that is a real timing difference, not a silent empty-file success.

This is not zed-web’s crate-level change (they kept `block_on`). It is the only way to compile the function without blocking the main thread and without inventing empty initial content.

### `settings_json`

Native editing functions: still `#[cfg(all(feature = "editing", not(target_family = "wasm")))]`, bodies untouched.

Wasm: five `pub fn` with the same signatures, all `panic!("JSON structural edits require tree-sitter, which is not available on wasm")`. Callers in `settings_store` / `keymap_file` keep compiling. The first settings-file or keymap structural edit on wasm panics instead of rewriting the file incorrectly.

### `languages` / `pet-fs`

`pet-conda` `pet-core` `pet-fs` `pet-poetry` `pet-reporter` `pet-virtualenv` `pet` moved to `not(wasm)`. `mod python` and python `LanguageInfo` / adapters / `PyprojectTomlManifestProvider` are `not(wasm)`. Cargo.toml on native still has those deps; `cargo check -p languages --lib` still compiled `pet-fs`.

On wasm there is no Python toolchain/LSP registration from this crate. The grammar may still exist via `grammars` once that layer compiles. Python env discovery via PET cannot work in the browser anyway.

## 4. Native behaviour — proof it did not change

Every new arm is `#[cfg(target_family = "wasm")]` or a wrap of an existing item with `not(target_family = "wasm")`. Inner native statements are the original ones.

| Site | Native still runs |
| --- | --- |
| `gpui::block_on` in `AppDatabase::new` / `GLOBAL_KEY_VALUE_STORE` | yes |
| `ForegroundExecutor::block_on` initial settings load | yes, before the watcher spawn |
| `migrator::migrate_settings` | yes |
| tree-sitter JSON edits | yes, same functions |
| `pet-fs` / `mod python` / python adapters | yes |

```
CARGO_TARGET_DIR=target/web-probe cargo check -p db -p settings -p settings_json --lib
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 22.98s

CARGO_TARGET_DIR=target/web-probe cargo check -p languages --lib
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 35.89s
    (includes Checking pet-fs)
```

## 5. Stub honesty

| Stub | Honesty |
| --- | --- |
| `AppDatabase::new` on wasm | panics; does not open a fake DB |
| `AppDatabase::open_in_memory` | real `ThreadSafeConnection::build` with `locking_queue`. Phase 5c sqlez then `Err`s on SQL; we panic on that `Err`. No `Ok` empty sqlite |
| `GlobalKeyValueStore::global` on wasm | panics |
| `watch_settings_files` wasm | does **not** invent empty settings. It waits asynchronously. Timing vs native is different (see §3) |
| `parse_and_migrate_zed_settings` wasm | does not pretend a migration ran. Parses current JSON. Old keys that need migrator stay unmigrated |
| `settings_json` five editing fns | panic. zed-web’s whole-file / `,value` fallbacks refused |
| python / `pet-fs` | crate absent on wasm. Not a stub that returns `Ok` |

## 6. Acceptance

### 1. wasm workspace check — `db` `pet-fs` `settings` gone

```
cd web && CARGO_TARGET_DIR=../target/web-probe cargo check --workspace --target wasm32-unknown-unknown
```

`Checking db` and `Checking settings` succeed (no `could not compile` for those, no `pet-fs` in the log). The previous 9 rustc errors in those three crates are gone.

**New errors (not fixed this round).** The graph now proceeds past layer 5 and dies on:

| Crate | What |
| --- | --- |
| `tree-sitter-json` 0.24.8 | build.rs: host clang, `'stdlib.h' file not found` for `--target=wasm32-unknown-unknown`. This is the WASI SDK hole (§4.2). Previously hidden because `pet-fs` / `db` / `settings` failed first. `CC_wasm32_unknown_unknown` was unset |
| `openai_subscribed` | 2 errors: `start_oauth_callback_server_with_config` / `OAuthCallbackServerConfig` cfg’d out of `oauth_callback_server` (`openai_subscribed.rs:1123`, `:1124`) |
| `language` | 15 errors: `Parser`/`TSLanguage` not `Send`/`Sync` on the wasm tree-sitter stub (`language.rs:134`, `language_registry.rs:852`, `buffer.rs` spawn sites); `ForegroundExecutor::block_with_timeout` missing (`buffer.rs:2082`); `grammar.ts_language` missing (`syntax_map.rs:1571`, `language.rs:1386`); `ParseableLanguage: From<tree_sitter::Language>` (`language.rs:967`) |

`could not compile` wrappers: `openai_subscribed`, `language`. Exit still 101.

### 2. native

See §4. Both commands `Finished`.

### 3. refusals

```
./web/check-refusals.sh
ok   §5.3.1 RELEASE_CHANNEL is dev
ok   §5.3.2 terminal Shift+Click selection extension exists
ok   §5.3.3 recent_projects open_local_project PathPromptOptions.files is true
ok   §5.3.4 remote_server MultiWrite::flush uses send_blocking
4 checks, 0 failures
```

### 4. rustfmt

Repo-root `rustfmt.toml`, edition 2024, stdin `--check` (so `mod` children are not pulled in):

`crates/db/src/db.rs` `crates/db/src/kvp.rs` `crates/settings/src/settings_store.rs` `crates/settings_json/src/settings_json.rs` `crates/languages/src/lib.rs`

All clean. The five pre-dirty files were not formatted.

### 5. `Cargo.lock`

```
cmp /tmp/lock-before-p5d Cargo.lock   # identical
wc -c: 502232  502232
diff: empty
```

Target-cfg moves of `pet*` leave no lock trace; those packages were already named.

## 7. Files this round

| File | Change |
| --- | --- |
| `crates/db/src/db.rs` | wasm panic in `new`; `open_in_memory`; skip mkdir; wasm fallback uses `locking_queue` |
| `crates/db/src/kvp.rs` | LazyLock native-only; wasm `global()` panics |
| `crates/settings/src/settings_store.rs` | wasm does not `block_on` the first settings reads; migrator `not(wasm)` |
| `crates/settings_json/src/settings_json.rs` | wasm panic stubs for the five editing functions |
| `crates/languages/Cargo.toml` | `pet*` → `not(wasm)` |
| `crates/languages/src/lib.rs` | `mod python` and python `init` wiring `not(wasm)` |
| `docs/phase5-blockers-4.md` | this report |

Not edited: `web/**`, `Cargo.lock`, root `Cargo.toml`, `crates/gpui/**` (block_on stays absent on wasm), `crates/sqlez/**`, `crates/oauth_callback_server/**`.

## 8. Stop / architecture notes (for the brain)

1. **`block_on` was not restored.** zed-web’s compile strategy for these call sites is pollster on wasm. Ours is: async constructor for DB (`open_in_memory`), panic for the leftover sync constructors, async apply for `watch_settings_files`. The web entrypoint already matches the DB/settings half of that. KVP `global()` still needs a later async init if `prompt_store` is in the wasm graph.
2. **`prepare_web_database` / `remote_sql` still illegal** across the two-workspace split. `open_in_memory` will panic at migrate until that decision is made.
3. **settings_json:** do not take zed-web’s wasm fallback. It corrupts files. Panic is honest; a real comment-preserving editor on wasm needs tree-sitter (WASI) or an explicit “rewrite whole document with serde_json and lose comments” product decision.
4. Next compile wall is `language` + WASI for `tree-sitter-json`, plus `openai_subscribed` oauth cfg. Not this round.
